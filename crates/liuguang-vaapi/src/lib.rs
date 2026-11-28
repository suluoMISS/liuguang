//! `liuguang-vaapi`：VA-API 硬件解码 + DMA-BUF 导出。
//!
//! # 为什么需要这个 crate
//!
//! 显示侧（Wayland `zwp_linux_dmabuf_v1`）能直接扫描 dma-buf，问题是"怎么从解码器
//! 拿到 dma-buf"。两条现成路线在 NVIDIA 上都不通（真机实测，见
//! `docs/ARCHITECTURE.md` §8.6）：
//!
//! 1. **GStreamer**：`gst-va` 的 `vah264dec` 只提供 `memory:VAMemory` 与系统内存 caps，
//!    没有 `memory:DMABuf`；本机 `gst-va` 也没有 `vapostproc`/`vadownload` 可供转换。
//! 2. **mpv 的 `--vo=dmabuf-wayland`**：`Could not create device.`（VAAPI hwaccel 设备
//!    创建失败），最终退到软解。
//!
//! 可行的路线（`vashare` spike 已验证）：自己开 `VADisplay`（或让 ffmpeg 开、
//! 再读回它的句柄），用 FFmpeg 解码得到 `AV_PIX_FMT_VAAPI` 帧，然后对帧里的
//! `VASurfaceID` 调 `vaExportSurfaceHandle(DRM_PRIME_2)`，拿到 fd 列表 + modifier +
//! 每层 offset/pitch，直接交给 Wayland。
//!
//! # 安全边界
//!
//! - 本 crate 是 FFI 封装，`unsafe` 集中在 `ffi`/`decoder`/`export` 的少数函数里，
//!   每处都写明前置条件；
//! - 结构体布局不靠"我记得 ffmpeg 长这样"：`build.rs` 用系统头文件的 `offsetof`
//!   算出关键字段偏移，Rust 侧用 `offset_of!` 做**编译期断言**；
//! - 解码表面在合成器 `release` 之前不能被回收，因此把 `AVFrame` 塞进
//!   `FrameKeepalive`（见 `liuguang_core::types::FrameKeepalive`）。
#![allow(unsafe_code)]

pub mod convert;
pub mod decoder;
pub mod device;
pub mod export;
mod ffi;
pub mod producer;

pub use convert::RgbConverter;
pub use decoder::{DecodedSurface, StreamInfo, VaapiDecoder, forget_all_devices, forget_device};
pub use device::VaDevice;
pub use export::{ExportedPlane, ExportedSurface, export_surface};
pub use producer::VaapiProducer;

/// 本 crate 的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum VaapiError {
    /// 打不开渲染节点。
    #[error("打开渲染节点失败: {path}: {source}")]
    OpenNode {
        /// 节点路径。
        path: std::path::PathBuf,
        /// 底层 I/O 错误。
        source: std::io::Error,
    },
    /// 节点不支持 VA-API。
    #[error("渲染节点不支持 VA-API（vaGetDisplayDRM 返回空）: {0}")]
    NoDisplay(std::path::PathBuf),
    /// `vaInitialize` 失败。
    #[error("VA-API 初始化失败（{node}）: {message}")]
    Init {
        /// 节点路径。
        node: std::path::PathBuf,
        /// 状态文本。
        message: String,
    },
    /// 表面导出失败。
    #[error("导出解码表面失败（surface={surface}）: {message}")]
    Export {
        /// 表面 id。
        surface: u32,
        /// 状态文本。
        message: String,
    },
    /// FFmpeg 层错误。
    #[error("FFmpeg: {0}")]
    Ffmpeg(String),
    /// 路径/格式/能力不支持。
    #[error("不支持: {0}")]
    Unsupported(String),
    /// GPU 转换（NV12 → XRGB8888）过程中的驱动/GL 错误。
    #[error("RGB 转换失败: {0}")]
    Convert(String),
    /// 输出缓冲仍被合成器占用。
    ///
    /// 这不是错误：调用方应退避（返回 `VideoPoll::NeedMore`）后重试，
    /// 不要把它当失败计数，也不要死等。
    #[error("RGB 转换缓冲仍在被合成器使用")]
    Busy,
}

impl From<VaapiError> for liuguang_media::MediaError {
    fn from(error: VaapiError) -> Self {
        match error {
            VaapiError::OpenNode { .. } | VaapiError::NoDisplay(_) | VaapiError::Init { .. } => {
                Self::Decode(format!("VA-API 设备不可用: {error}"))
            }
            other => Self::Decode(other.to_string()),
        }
    }
}

/// 判断某个渲染节点现在能不能真的开 VA-API（打不开就返回 `false`，不 panic）。
///
/// 这是给配置解析用的**能力探测**，不要在每个视频打开时调用（会多开一次设备）。
#[must_use]
pub fn probe_node(node: &std::path::Path) -> bool {
    match VaDevice::open(node) {
        Ok(_device) => true,
        Err(error) => {
            tracing::debug!(node = %node.display(), %error, "渲染节点不支持 VA-API");
            false
        }
    }
}

/// 列出 `/dev/dri` 下形如 `renderD*` 的节点（已排序，稳定顺序）。
pub fn render_nodes() -> Vec<std::path::PathBuf> {
    let mut nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| decoder::looks_like_render_node(path))
        .collect();
    decoder::sort_render_nodes(&mut nodes);
    nodes
}

/// 渲染节点，**显示 GPU 优先**：按 `/sys/class/drm/card*-<连接器>/status` 找到
/// `connected` 的连接器所属 PCI 设备，把同一 PCI 设备上的 render 节点排到前面
/// （组内保持原顺序）。
///
/// 为什么需要：`render_nodes()` 的顺序只由名字决定（`renderD128` 永远在前），
/// 而名字不告诉你哪块 GPU 在驱动屏幕。本机就是 iGPU = `renderD128`、
/// dGPU（接 eDP-1）= `renderD129`；按名字选会挑到 Intel 节点，解码在 iGPU、
/// 转换与合成在 dGPU ⇒ 跨 GPU 全帧导入，4K60 实测只剩 20fps（选对是 58fps）。
/// 判据只看 sysfs 的 PCI 设备 id（形如 `0000:01:00.0`），**不猜节点编号**。
///
/// 任何一步读不到（sysfs 缺失、没有 connected 连接器、没有 PCI 匹配）都原样返回
/// `render_nodes()` 的顺序——**绝不因为 sysfs 缺失而少给节点**。
#[must_use]
pub fn render_nodes_preferring_display() -> Vec<String> {
    render_nodes_preferring_display_from(std::path::Path::new("/sys"))
}

/// [`render_nodes_preferring_display`] 的可注入版本（`sysfs` 通常是 `/sys`）。
///
/// 可注入只为了单测能伪造一棵 sysfs 树；产品路径不该传别的目录。
#[must_use]
pub fn render_nodes_preferring_display_from(sysfs: &std::path::Path) -> Vec<String> {
    let nodes: Vec<String> = render_nodes()
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    reorder_display_first(sysfs, nodes)
}

/// 把属于显示 GPU 的节点稳定地排到前面，其余保持原顺序；结果去重。
///
/// 单独抽出来是为了单测能喂**确定的**节点列表：真机 `/dev/dri` 的内容不是测试
/// 能控制的，直接断言 `renderD129` 会变成"只在这台机器上通过"的测试。
fn reorder_display_first(sysfs: &std::path::Path, nodes: Vec<String>) -> Vec<String> {
    let display = display_pci_ids(sysfs);
    if display.is_empty() {
        return dedup_nodes(nodes);
    }
    let node_pci = render_node_pci_ids(sysfs);
    let mut preferred = Vec::with_capacity(nodes.len());
    let mut rest = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node_pci.get(&node) {
            Some(pci) if display.contains(pci) => preferred.push(node),
            _ => rest.push(node),
        }
    }
    preferred.append(&mut rest);
    dedup_nodes(preferred)
}

/// `connected` 连接器所属卡的 PCI 设备 id 集合（读不到就是空集合）。
fn display_pci_ids(sysfs: &std::path::Path) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    let drm = sysfs.join("class/drm");
    let Ok(entries) = std::fs::read_dir(&drm) else {
        return ids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // 只认连接器条目 `card<digits>-<连接器>`：`card2` 是卡本身，没有 `-`。
        let Some((card, _connector)) = name.split_once('-') else {
            continue;
        };
        if !is_card_name(card) {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        if status.trim() != "connected" {
            continue;
        }
        if let Some(pci) = pci_id_of(&drm.join(card).join("device")) {
            ids.insert(pci);
        }
    }
    ids
}

/// render 节点 → 所属 PCI 设备 id 的映射（键就是 `/dev/dri/renderDNNN` 路径）。
fn render_node_pci_ids(sysfs: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let drm = sysfs.join("class/drm");
    let Ok(entries) = std::fs::read_dir(&drm) else {
        return map;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_render_name(&name) {
            continue;
        }
        if let Some(pci) = pci_id_of(&entry.path().join("device")) {
            map.insert(format!("/dev/dri/{name}"), pci);
        }
    }
    map
}

/// `card` + 全数字（`card0`、`card2`；`card2-eDP-1` 在这一步之前已被拆开）。
fn is_card_name(name: &str) -> bool {
    let Some(digits) = name.strip_prefix("card") else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// `renderD` + 全数字。
fn is_render_name(name: &str) -> bool {
    let Some(digits) = name.strip_prefix("renderD") else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// 取 `.../device` 符号链接 realpath 的 basename（PCI 设备 id，如 `0000:01:00.0`）。
fn pci_id_of(device: &std::path::Path) -> Option<String> {
    let resolved = std::fs::canonicalize(device).ok()?;
    let name = resolved.file_name()?.to_string_lossy().into_owned();
    (!name.is_empty()).then_some(name)
}

/// 保序去重。
fn dedup_nodes(nodes: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(nodes.len());
    let mut unique = Vec::with_capacity(nodes.len());
    for node in nodes {
        if seen.insert(node.clone()) {
            unique.push(node);
        }
    }
    unique
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// 一棵一次性的假 sysfs 树（`<root>/class/drm` + `<root>/devices`）。
    ///
    /// 目录名带 pid + 全局计数器，并行测试之间不会互相踩；用完删掉。
    fn fake_sysfs(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "liuguang-sysfs-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("class/drm")).unwrap();
        std::fs::create_dir_all(root.join("devices")).unwrap();
        root
    }

    /// 建一张卡：`class/drm/card<N>/device` 指向 `devices/<pci>`。
    fn add_card(sysfs: &Path, card: &str, pci: &str) {
        let device = sysfs.join("devices").join(pci);
        std::fs::create_dir_all(&device).unwrap();
        let card_dir = sysfs.join("class/drm").join(card);
        std::fs::create_dir_all(&card_dir).unwrap();
        std::os::unix::fs::symlink(&device, card_dir.join("device")).unwrap();
    }

    /// 建一个连接器目录并写它的 `status`（真 sysfs 里 `"connected\n"`）。
    fn add_connector(sysfs: &Path, card: &str, connector: &str, status: &str) {
        let dir = sysfs.join("class/drm").join(format!("{card}-{connector}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("status"), status).unwrap();
    }

    /// 建一个 render 节点条目：`class/drm/renderDNNN/device` → `devices/<pci>`。
    fn add_render_node(sysfs: &Path, name: &str, pci: &str) {
        let device = sysfs.join("devices").join(pci);
        std::fs::create_dir_all(&device).unwrap();
        let dir = sysfs.join("class/drm").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&device, dir.join("device")).unwrap();
    }

    fn node_list(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    }

    #[test]
    fn render_node_names_are_recognised() {
        assert!(decoder::looks_like_render_node(&PathBuf::from(
            "/dev/dri/renderD128"
        )));
        assert!(!decoder::looks_like_render_node(&PathBuf::from(
            "/dev/dri/card1"
        )));
        assert!(!decoder::looks_like_render_node(&PathBuf::from(
            "/dev/dri/by-path"
        )));
    }

    #[test]
    fn render_nodes_are_sorted() {
        let mut nodes = vec![
            PathBuf::from("/dev/dri/renderD129"),
            PathBuf::from("/dev/dri/renderD128"),
        ];
        decoder::sort_render_nodes(&mut nodes);
        assert_eq!(nodes[0], PathBuf::from("/dev/dri/renderD128"));
        assert_eq!(nodes[1], PathBuf::from("/dev/dri/renderD129"));
    }

    #[test]
    fn error_maps_to_media_decode_error() {
        let error = VaapiError::Ffmpeg("boom".into());
        let media: liuguang_media::MediaError = error.into();
        assert!(media.to_string().contains("boom"), "实际: {media}");
    }

    #[test]
    fn missing_node_is_reported_not_panicked() {
        let error = VaDevice::open(&PathBuf::from("/dev/dri/definitely-missing-xyz"))
            .expect_err("不存在的节点必须报错");
        assert!(
            matches!(error, VaapiError::OpenNode { .. }),
            "实际: {error}"
        );
    }

    #[test]
    fn display_gpu_render_node_is_preferred() {
        // 本机形态：Intel iGPU（renderD128，无连接器）在 0000:00:02.0；
        // NVIDIA dGPU（renderD129）接 eDP-1，在 0000:01:00.0。
        let sysfs = fake_sysfs("display-nvidia");
        add_card(&sysfs, "card1", "0000:00:02.0");
        add_connector(&sysfs, "card1", "DP-1", "disconnected\n");
        add_card(&sysfs, "card2", "0000:01:00.0");
        add_connector(&sysfs, "card2", "eDP-1", "connected\n");
        add_render_node(&sysfs, "renderD128", "0000:00:02.0");
        add_render_node(&sysfs, "renderD129", "0000:01:00.0");

        let ordered = reorder_display_first(
            &sysfs,
            node_list(&["/dev/dri/renderD128", "/dev/dri/renderD129"]),
        );
        assert_eq!(
            ordered,
            node_list(&["/dev/dri/renderD129", "/dev/dri/renderD128"]),
            "显示 GPU（NVIDIA）的 renderD129 必须排第一"
        );
        let _ = std::fs::remove_dir_all(&sysfs);
    }

    #[test]
    fn no_connected_connector_keeps_render_node_order() {
        let sysfs = fake_sysfs("no-connected");
        add_card(&sysfs, "card1", "0000:00:02.0");
        add_connector(&sysfs, "card1", "DP-1", "disconnected\n");
        add_card(&sysfs, "card2", "0000:01:00.0");
        add_connector(&sysfs, "card2", "eDP-1", "disconnected\n");
        add_render_node(&sysfs, "renderD128", "0000:00:02.0");
        add_render_node(&sysfs, "renderD129", "0000:01:00.0");

        let before = node_list(&["/dev/dri/renderD128", "/dev/dri/renderD129"]);
        assert_eq!(reorder_display_first(&sysfs, before.clone()), before);
        let _ = std::fs::remove_dir_all(&sysfs);
    }

    #[test]
    fn unmatched_pci_id_keeps_render_node_order() {
        // 显示卡在 0000:03:00.0（比如第三个 GPU），但 /dev/dri 里只有另外两块。
        let sysfs = fake_sysfs("pci-mismatch");
        add_card(&sysfs, "card0", "0000:03:00.0");
        add_connector(&sysfs, "card0", "HDMI-A-1", "connected\n");
        add_render_node(&sysfs, "renderD128", "0000:00:02.0");
        add_render_node(&sysfs, "renderD129", "0000:01:00.0");

        let before = node_list(&["/dev/dri/renderD128", "/dev/dri/renderD129"]);
        assert_eq!(reorder_display_first(&sysfs, before.clone()), before);
        let _ = std::fs::remove_dir_all(&sysfs);
    }

    #[test]
    fn missing_sysfs_returns_plain_render_nodes() {
        // 不变式：sysfs 读不到时**绝不**比 `render_nodes()` 少给节点。
        let expected: Vec<String> = render_nodes()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let got =
            render_nodes_preferring_display_from(Path::new("/nonexistent/liuguang-no-sysfs-here"));
        assert_eq!(got, expected);
    }

    #[test]
    fn duplicate_nodes_are_collapsed() {
        let sysfs = fake_sysfs("duplicates");
        add_card(&sysfs, "card2", "0000:01:00.0");
        add_connector(&sysfs, "card2", "eDP-1", "connected\n");
        add_render_node(&sysfs, "renderD129", "0000:01:00.0");

        let ordered = reorder_display_first(
            &sysfs,
            node_list(&[
                "/dev/dri/renderD128",
                "/dev/dri/renderD129",
                "/dev/dri/renderD128",
            ]),
        );
        assert_eq!(
            ordered,
            node_list(&["/dev/dri/renderD129", "/dev/dri/renderD128"])
        );
        let _ = std::fs::remove_dir_all(&sysfs);
    }
}
