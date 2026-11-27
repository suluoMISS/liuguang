//! 帧内容正确性判据（**当前最缺的尺子**）。
//!
//! 今晚已两次证明"计数类判据抓不到画面损伤"：
//! - `t30`：计数全绿，但 CRC 冻结；
//! - Lead：用"CRC 互异率是否高于源帧率"也抓不到（两臂都是 20.0·s⁻¹）。
//!
//! 本模块提供的判据**直接比较缓冲内容**，因此能区分两类完全不同的病因：
//!
//! | 观察 | 结论 |
//! |---|---|
//! | 同一 `pts` 的两次 dump **不同** | **写未完成就被读**（时序 / 生产者还没写完） |
//! | 同一 `pts` 跨路径（直通 vs CPU）CRC **不同** | **两条路径产出的缓冲内容不同**（转换/格式错） |
//! | 同一帧上下半区 CRC **分别等于不同源帧** | **时序**（半帧来自另一帧 ⇒ 撕裂） |
//! | 同一帧上下半区 **都等于同一源帧** | **空间误读**（tiling/布局/缩放读错位置） |
//!
//! ## 工程约束（务必遵守）
//!
//! **Y_TILED（modifier≠0）缓冲不能按 linear 解释**。本模块因此**只做同格式、
//! 同修饰符的逐字节比较**（`crc == crc`），**绝不把 dump 当图片看**，
//! 也不对 tiled 缓冲做"按行取像素"的任何推断。
//!
//! ## 判据不是"图像相似度"
//!
//! 全部比较都是**精确相等/不等**，没有阈值、没有容差 —— 因此不会出现
//! "看起来差不多就判过"的假绿。

use liuguang_core::types::{CpuFrame, DisplayFrame, DmaBufFrame};
use std::io;

/// 一条 CRC 记录（用于跨臂/跨帧比对）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDigest {
    /// 帧的呈现时间戳（毫秒）——**跨臂比对的唯一键**。
    pub pts_ms: u64,
    /// 整帧 CRC（同格式两条路径可直接比）。
    pub crc: u64,
    /// 上半区 CRC（`y < height/2`）。
    pub crc_top: u64,
    /// 下半区 CRC（`y >= height/2`）。
    pub crc_bottom: u64,
    /// 参与计算的字节数。
    pub bytes: usize,
}

/// 探针错误（**只读**路径上的失败原因，绝不 panic）。
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// mmap 失败（fd 无效/权限/尺寸为 0）。
    #[error("mmap 失败：{0}")]
    Mmap(#[from] io::Error),
    /// 缓冲尺寸为 0 或超出合理上限（防止误 mmap 巨量内存）。
    #[error("缓冲尺寸不可用：{0}")]
    BadGeometry(String),
}

/// mmap 上限：4K NV12 ≈ 12.4 MiB；给到 256 MiB 足够覆盖 8K 且防误用。
const MAX_MAP_BYTES: usize = 256 * 1024 * 1024;

/// i915 tile 宽度（字节）。**来源**：`intel_tile_width_bytes()`（内核 `skl_universal_plane.c`
/// 附近），Y_TILED 8bpp 为 128 B。已由实测参数交叉验证：`1920/128 = 15` tiles/行，
/// `15 × 34 × 4096 = 2088960` **恰好等于**实测 `offsets[1]`。
pub const TILE_BYTES_WIDE: usize = 128;
/// i915 tile 高度（行）。来源同上；`ceil(1080/32) = 34` tile 行。
pub const TILE_ROWS: usize = 32;
/// 单个 tile 的字节数（128 × 32）。
pub const TILE_BYTES: usize = TILE_BYTES_WIDE * TILE_ROWS;

/// **候选** tile 内部字节序（本机无法权威确认 ⇒ 用数据判决）。
///
/// 三种布局的 tile **网格**完全相同（128B × 32 行），差别只在 tile 内部如何排布。
/// 判决方法：把直通臂缓冲按每种布局还原，与 CPU 臂同一 `pts` 的线性 Y 平面逐像素比，
/// **只有正确的布局才会一致**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileLayout {
    /// **正确布局（真机实测解出，逐字节验证）**：8 个 16 字节宽的子列，
    /// 每子列 512 字节（= 32 行 × 16 字节）。
    ///
    /// `off = (wx/16)*512 + wy*16 + (wx%16)`
    ///
    /// **证据**（t57，真实 drm 后端，与 CPU 臂同一 `pts`）：
    /// - 全帧 1920×1080 **逐字节完全一致**（PNG sha256 相同）
    /// - 5/5 个 `pts`（62/125/187/250/312）复验一致
    ///
    /// 注意：这**不是**常见的"4×32 字节列" Y-major 公式（那是 32bpp 的形态）。
    /// NV12 的 Y 平面是 8bpp ⇒ 子列只有 16 字节宽。
    SubColumn16,
    /// 曾经的错误假设：4 个 32B 列（真机实测**错**，还原出横向条纹乱码）。
    Column32,
    /// 对照/基线：即"没有平铺"（真机实测**错**，垂直条纹乱码）。
    RowMajor,
}

impl TileLayout {
    /// 全部候选（判决时逐一尝试）。
    pub const ALL: [Self; 3] = [Self::SubColumn16, Self::Column32, Self::RowMajor];

    /// 人类可读名字（报告用）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SubColumn16 => "实测正确:16B子列",
            Self::Column32 => "错误假设:32B列",
            Self::RowMajor => "对照:行优先",
        }
    }

    /// tile 内偏移（`x`/`y` 为 tile **内**坐标，x 以字节计）。
    #[must_use]
    const fn offset_in_tile(self, x: usize, y: usize) -> usize {
        match self {
            Self::SubColumn16 => (x / 16) * 512 + y * 16 + (x % 16),
            Self::Column32 => (x / 32) * 1024 + y * 32 + (x % 32),
            Self::RowMajor => y * TILE_BYTES_WIDE + x,
        }
    }
}

/// 把一帧 **Y_TILED** 缓冲按指定布局还原成线性 Y 平面（8bpp，行跨度 = `width`）。
///
/// 输入 `tiled` 是 tile 网格布局（`tiles_per_row = stride / 128`），
/// 输出是紧凑线性图像（`width × height`）。
///
/// 越界读取一律**跳过**（不 panic）——真机缓冲可能比预期短。
#[must_use]
pub fn untile_y(
    tiled: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    layout: TileLayout,
) -> Vec<u8> {
    let mut out = vec![0u8; width.saturating_mul(height)];
    let tiles_per_row = stride / TILE_BYTES_WIDE;
    if tiles_per_row == 0 {
        return out;
    }
    for y in 0..height {
        for x in 0..width {
            let tile_x = x / TILE_BYTES_WIDE;
            let tile_y = y / TILE_ROWS;
            let within_x = x % TILE_BYTES_WIDE;
            let within_y = y % TILE_ROWS;
            let tile_idx = tile_y * tiles_per_row + tile_x;
            let off = tile_idx * TILE_BYTES + layout.offset_in_tile(within_x, within_y);
            if let Some(b) = tiled.get(off) {
                out[y * width + x] = *b;
            }
        }
    }
    out
}

/// 两个等长缓冲的逐字节一致率（0.0–1.0）与首个差异位置。
#[must_use]
pub fn byte_match_ratio(a: &[u8], b: &[u8]) -> (f64, Option<usize>) {
    let n = a.len().min(b.len());
    if n == 0 {
        return (0.0, None);
    }
    let mut same = 0usize;
    let mut first_diff = None;
    for i in 0..n {
        if a[i] == b[i] {
            same += 1;
        } else if first_diff.is_none() {
            first_diff = Some(i);
        }
    }
    (same as f64 / n as f64, first_diff)
}

/// 对一段线性内存算 FNV-1a 64 位 CRC（**确定性、无外部依赖**）。
///
/// 选 FNV 而不是加密散列：判据只需要"等/不等"，不需要抗碰撞安全性，
/// 而且必须能在无 `crc` 依赖的情况下跑。
#[must_use]
pub fn crc64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// 一帧的线性视图：数据 + 行跨度 + **可见字节/行**。
struct LinearView<'a> {
    data: &'a [u8],
    stride: usize,
    /// 每行**可见**字节数（= `width * bytes_per_pixel`）。
    ///
    /// **必须排除行尾 padding**：CPU 臂与直通臂的 stride 常不同
    /// （CPU 侧 stride 可能恰等于 `width*bpp`，直通侧按 128/256 对齐），
    /// 若把 padding 计入 CRC，**同一画面会算出不同 CRC ⇒ 假阳性**。
    visible: usize,
}

impl LinearView<'_> {
    /// 逐行只对**可见像素字节**累计 CRC（跳过行尾 padding）。
    fn crc(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let stride = self.stride.max(1);
        let visible = self.visible.min(stride);
        for row in self.data.chunks(stride) {
            for b in row.iter().take(visible) {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        hash
    }
}

/// 从 CPU 帧构造摘要。
#[must_use]
pub fn digest_cpu(pts_ms: u64, frame: &CpuFrame) -> FrameDigest {
    let stride = frame.stride.max(1) as usize;
    let visible = frame.width as usize * frame.format.bytes_per_pixel() as usize;
    let view = LinearView {
        data: &frame.data,
        stride,
        visible,
    };
    let rows = (frame.height as usize).max(1);
    let split_row = rows / 2;
    let split_byte = (split_row * stride).min(frame.data.len());
    let top = LinearView {
        data: &frame.data[..split_byte],
        stride,
        visible,
    };
    let bottom = LinearView {
        data: &frame.data[split_byte..],
        stride,
        visible,
    };
    FrameDigest {
        pts_ms,
        crc: view.crc(),
        crc_top: top.crc(),
        crc_bottom: bottom.crc(),
        bytes: frame.data.len(),
    }
}

/// 对 DMA-BUF 帧做 **mmap 后只读 dump** 并算摘要。
///
/// **只读映射**（`PROT_READ`），不改动缓冲、不影响显示；映射长度取
/// `stride * height`（NV12 的 UV 平面紧随其后，若需要可用 [`dump_dmabuf_planes`]）。
///
/// # Errors
///
/// mmap 失败或几何不可用时返回 [`ProbeError`]；**不会 panic**。
pub fn digest_dmabuf(pts_ms: u64, frame: &DmaBufFrame) -> Result<FrameDigest, ProbeError> {
    let (data, stride) = map_plane(frame, 0)?;
    let stride = stride.max(1);
    let rows = frame.height.max(1) as usize;
    let split_row = rows / 2;
    let split_byte = (split_row * stride).min(data.len());
    let visible = frame.width as usize * frame.format.bytes_per_pixel() as usize;
    let view = LinearView {
        data: &data,
        stride,
        visible,
    };
    let top = LinearView {
        data: &data[..split_byte],
        stride,
        visible,
    };
    let bottom = LinearView {
        data: &data[split_byte..],
        stride,
        visible,
    };
    Ok(FrameDigest {
        pts_ms,
        crc: view.crc(),
        crc_top: top.crc(),
        crc_bottom: bottom.crc(),
        bytes: data.len(),
    })
}

/// 逐平面 dump（NV12 用 `[0]`/`[1]`），返回每平面 CRC 与字节数。
///
/// 用于"UV 平面是否被正确填充"这类**同格式**比较。
///
/// # Errors
///
/// 任一平面 mmap 失败时返回 [`ProbeError`]。
pub fn dump_dmabuf_planes(frame: &DmaBufFrame) -> Result<Vec<(u64, usize)>, ProbeError> {
    let planes = if frame.format.is_semi_planar() { 2 } else { 1 };
    let mut out = Vec::with_capacity(planes);
    for idx in 0..planes {
        let (data, _stride) = map_plane(frame, idx)?;
        out.push((crc64(&data), data.len()));
    }
    Ok(out)
}

/// 对任意帧取第 `plane` 个平面的字节（DMA-BUF 走 mmap；CPU 帧直接切片）。
///
/// 这是离线还原比对的**数据源**：直通臂给 tiled 字节，CPU 臂给线性字节。
fn map_plane_of(frame: &DisplayFrame, plane: usize) -> Result<(Vec<u8>, usize), ProbeError> {
    match frame {
        DisplayFrame::DmaBuf(d) => map_plane(d, plane),
        DisplayFrame::Cpu(c) => {
            let stride = c.stride.max(1) as usize;
            let y_rows = c.height as usize;
            if plane == 0 {
                let len = stride.saturating_mul(y_rows).min(c.data.len());
                return Ok((c.data[..len].to_vec(), stride));
            }
            // NV12 的 UV 平面：**紧跟 Y 平面之后**（起点 = stride × height），半高。
            let start = stride.saturating_mul(y_rows);
            let rows = y_rows.div_ceil(2);
            let len = stride
                .saturating_mul(rows)
                .min(c.data.len().saturating_sub(start));
            let src = c.data.get(start..start + len).unwrap_or(&[]);
            Ok((src.to_vec(), stride))
        }
        DisplayFrame::GpuTexture(_) => Err(ProbeError::BadGeometry(
            "GPU 纹理帧无字节可 dump".to_string(),
        )),
    }
}

/// 把第 `plane` 个平面只读映射出来（返回 `(数据, 行跨度)`）。
///
/// # Safety 说明
///
/// 本函数是**唯一的** `unsafe` 使用点，且仅做只读映射：
/// - 映射长度经 `checked_mul` 且 ≤ [`MAX_MAP_BYTES`]，越界立即返回错误；
/// - `PROT_READ` + `MAP_SHARED`：**不改写**缓冲，不影响正在扫描输出的内容；
/// - 拷贝到 `Vec` 后**立刻** `munmap`，不存在悬垂引用；
/// - `mmap` 失败（`MAP_FAILED`）转为 `Err`，不解引用。
#[allow(unsafe_code)]
fn map_plane(frame: &DmaBufFrame, plane: usize) -> Result<(Vec<u8>, usize), ProbeError> {
    let stride = *frame.strides.get(plane).unwrap_or(&frame.stride) as usize;
    let mut rows = if plane == 0 {
        frame.height as usize
    } else {
        // 半平面（NV12 的 UV）：行数减半。
        (frame.height as usize).div_ceil(2)
    };
    // **tiled 缓冲的行数必须向上取整到 tile 高度**（Y_TILED 为 32 行）：
    // 否则最后一个 tile 行会被截断（1920x1080 ⇒ 需 1088 行，即 2088960 字节；
    // 恰好等于实测的 `offsets[1]`，这是外部可校验的证据）。
    if frame.modifier != 0 {
        rows = rows.div_ceil(TILE_ROWS) * TILE_ROWS;
    }
    if stride == 0 || rows == 0 {
        return Err(ProbeError::BadGeometry(format!(
            "stride={stride} rows={rows}（frame={}x{}）",
            frame.width, frame.height
        )));
    }
    let len = stride
        .checked_mul(rows)
        .filter(|n| *n <= MAX_MAP_BYTES)
        .ok_or_else(|| {
            ProbeError::BadGeometry(format!("映射长度超限：stride={stride} rows={rows}"))
        })?;
    let offset = *frame.offsets.get(plane).unwrap_or(&0) as usize;

    // SAFETY: `mmap` 只读映射该 fd 的 `offset..offset+len`；成功时返回的指针在
    // `len` 字节内有效，且我们在 `munmap` 前完成拷贝。失败返回 `MAP_FAILED`。
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            frame
                .fd_for_plane(plane)
                .map(std::os::fd::AsRawFd::as_raw_fd)
                .unwrap_or(-1),
            offset as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(ProbeError::Mmap(io::Error::last_os_error()));
    }
    // SAFETY: 映射有效且长度 `len`；`to_vec` 完成拷贝后立即解除映射。
    let data = unsafe {
        let slice = std::slice::from_raw_parts(ptr.cast::<u8>(), len);
        let copied = slice.to_vec();
        libc::munmap(ptr, len);
        copied
    };
    Ok((data, stride))
}

/// 跨臂比对结论（**同 `pts_ms`**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossArmVerdict {
    /// 该 pts 只有一条臂有数据（不可比）。
    Incomparable,
    /// 两臂 CRC 完全一致 ⇒ **缓冲内容正确**，若画面仍错则是**显示读取**的问题。
    Identical,
    /// 两臂 CRC 不同 ⇒ **缓冲内容错**（转换/格式/写入），与显示读取无关。
    ContentDiffers {
        /// 直通臂 CRC。
        dmabuf: u64,
        /// CPU 臂 CRC。
        cpu: u64,
    },
}

/// 用**同一 `pts_ms`** 比对两条臂（判据的核心）。
#[must_use]
pub fn compare_same_pts(
    dmabuf: &[FrameDigest],
    cpu: &[FrameDigest],
) -> Vec<(u64, CrossArmVerdict)> {
    let mut out = Vec::new();
    for d in dmabuf {
        match cpu.iter().find(|c| c.pts_ms == d.pts_ms) {
            None => out.push((d.pts_ms, CrossArmVerdict::Incomparable)),
            Some(c) if c.crc == d.crc => out.push((d.pts_ms, CrossArmVerdict::Identical)),
            Some(c) => out.push((
                d.pts_ms,
                CrossArmVerdict::ContentDiffers {
                    dmabuf: d.crc,
                    cpu: c.crc,
                },
            )),
        }
    }
    out
}

/// 同帧上下半区来源检测结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HalvesVerdict {
    /// 上下半区来自同一源帧（正常）。
    Coherent,
    /// 下半区等于**另一帧**的上半区 ⇒ **时序**（撕裂）。
    TearingAgainst {
        /// 匹配到的相邻帧 pts。
        other_pts_ms: u64,
    },
    /// 上下半区都等于**同一源帧**但整帧 CRC 与任何源帧都不符 ⇒ **空间误读**（tiling/布局）。
    SpatialMisread {
        /// 匹配到的源帧 pts。
        other_pts_ms: u64,
    },
}

/// 用相邻帧判定"时序 vs 空间"。
///
/// - 本帧 `crc_top` 与自身 `crc_bottom` 不同，且 `crc_bottom` 恰等于某相邻帧的
///   `crc_top` ⇒ **撕裂**（下半是下一帧）；
/// - 本帧上下半区分别等于**同一相邻帧**的对应半区 ⇒ **空间误读**。
#[must_use]
pub fn classify_halves(cur: &FrameDigest, neighbours: &[FrameDigest]) -> HalvesVerdict {
    if cur.crc_top == cur.crc_bottom {
        return HalvesVerdict::Coherent;
    }
    for n in neighbours {
        if n.pts_ms == cur.pts_ms {
            continue;
        }
        if cur.crc_bottom == n.crc_top && cur.crc_top != n.crc_top {
            return HalvesVerdict::TearingAgainst {
                other_pts_ms: n.pts_ms,
            };
        }
    }
    for n in neighbours {
        if n.pts_ms == cur.pts_ms {
            continue;
        }
        if cur.crc_top == n.crc_top && cur.crc_bottom == n.crc_bottom {
            return HalvesVerdict::SpatialMisread {
                other_pts_ms: n.pts_ms,
            };
        }
    }
    HalvesVerdict::Coherent
}

/// 环境变量：**帧探针落盘目录**。设了它才会写 `.crc` 文件（默认不写、零开销）。
///
/// 每个被探的帧写一个 `<pts_ms>.crc`，内容为 `pts crc top bot`（单行，空格分隔）——
/// 与 `scripts/t52-frame-probe-runbook.sh` 的读取格式一致。
pub const ENV_FRAME_PROBE_DIR: &str = "LIUGUANG_FRAME_PROBE_DIR";

/// 环境变量：除 `.crc` 外**再把原始平面字节**写成 `<pts>.raw`（用于离线还原比对）。
///
/// 值为 `1`/`true`/`on`/`yes` 时启用；用 [`ENV_FRAME_PROBE_RAW_LIMIT`] 限制帧数
/// （默认 8），避免几十 MB × 上百帧把磁盘写满。
pub const ENV_FRAME_PROBE_RAW: &str = "LIUGUANG_FRAME_PROBE_RAW";

/// 环境变量：原始 dump 的**最大帧数**（默认 8）。
pub const ENV_FRAME_PROBE_RAW_LIMIT: &str = "LIUGUANG_FRAME_PROBE_RAW_LIMIT";

/// 帧探针：按 `LIUGUANG_FRAME_PROBE_DIR` 把每帧摘要落盘。
///
/// **未设环境变量时是零开销**（`enabled()` 为假，`record()` 立即返回）。
/// 目录不存在会自动创建；单帧写失败只记一次 WARN 并**自动停用**（避免每帧刷日志）。
#[derive(Debug, Default)]
pub struct FrameProbe {
    dir: Option<std::path::PathBuf>,
    /// 写失败后置位：只告警一次，之后静默（实验设施不能变成日志洪泛源）。
    failed: bool,
    written: u64,
    /// 是否同时 dump 原始平面字节。
    dump_raw: bool,
    /// 原始 dump 的剩余帧数配额。
    raw_left: u64,
    /// "提交后 dump"剩余配额（判定局部重写用）。
    post_left: u64,
}

impl FrameProbe {
    /// 从环境变量构造（未设/空 ⇒ 停用）。
    #[must_use]
    pub fn from_env() -> Self {
        let mut probe = Self::from_dir(std::env::var(ENV_FRAME_PROBE_DIR).ok().as_deref());
        probe.configure_raw(
            std::env::var(ENV_FRAME_PROBE_RAW).ok().as_deref(),
            std::env::var(ENV_FRAME_PROBE_RAW_LIMIT).ok().as_deref(),
        );
        probe
    }

    /// 配置原始 dump（测试与真机实验都用它，保持行为一致）。
    pub fn configure_raw(&mut self, raw: Option<&str>, limit: Option<&str>) {
        let on = raw
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "" | "0" | "off" | "false" | "no"
                )
            })
            .unwrap_or(false);
        self.dump_raw = on;
        self.raw_left = limit
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(8);
        self.post_left = self.raw_left;
    }

    /// 从给定目录构造（测试用；`None`/空 ⇒ 停用）。
    #[must_use]
    pub fn from_dir(dir: Option<&str>) -> Self {
        let dir = dir
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from);
        if let Some(d) = &dir
            && let Err(e) = std::fs::create_dir_all(d)
        {
            tracing::warn!(dir = %d.display(), error = %e, "帧探针：无法创建目录，已停用");
            return Self::default();
        }
        Self {
            dir,
            failed: false,
            written: 0,
            dump_raw: false,
            raw_left: 8,
            post_left: 8,
        }
    }

    /// 是否启用。
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.dir.is_some() && !self.failed
    }

    /// 已写出的帧数（用于日志/断言）。
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written
    }

    /// **提交之后**再 dump 一次同一缓冲（判定"扫描输出期间被改写"的关键实验）。
    ///
    /// 判据：
    /// - `<pts>.pre` 与 `<pts>.post` **相同** ⇒ 该缓冲在我们持有期间**没被改写**；
    /// - **不同** ⇒ **解码器在我们持有/扫描输出期间重写了它** ⇒
    ///   "局部重写"成立 ⇒ 需要"缓冲持有到扫完"的同步。
    ///
    /// **Y 与 UV 两个平面都写**：`<pts>.<suffix>`（Y）与 `<pts>.uv.<suffix>`（UV）。
    ///
    /// 为什么 UV 必须一起写：`digest`/`.crc` 只摘要 **plane 0**（Y）⇒ 只看 Y 的判据
    /// **完全看不到色度**；而粉红斑这类伪影正是色度问题。所以"提交前/后是否被改写"
    /// 必须在 UV 上也做逐字节比较，否则等于没测。
    ///
    /// 返回 `true` 表示 **Y 平面**写成功（UV 失败只告警，不改变返回值语义）。
    pub fn record_plane_bytes(&mut self, pts_ms: u64, frame: &DisplayFrame, suffix: &str) -> bool {
        if !self.enabled() || !self.dump_raw || self.raw_left == 0 {
            return false;
        }
        let Some(dir) = &self.dir else { return false };
        let Ok((bytes, _)) = map_plane_of(frame, 0) else {
            return false;
        };
        let path = dir.join(format!("{pts_ms}.{suffix}"));
        let ok = std::fs::write(&path, &bytes).is_ok();
        // UV 平面（半高）。CPU 帧在同一 buffer 内（起点 stride*height）；
        // DMA-BUF 按 offsets[1]。**与 Y 同一时机落盘**，便于 pre/post 逐字节对比。
        if let Ok((uv, uv_stride)) = map_plane_of(frame, 1)
            && !uv.is_empty()
        {
            let uv_path = dir.join(format!("{pts_ms}.uv.{suffix}"));
            match std::fs::write(&uv_path, &uv) {
                Ok(()) => tracing::info!(
                    path = %uv_path.display(),
                    bytes = uv.len(),
                    stride = uv_stride,
                    suffix,
                    "帧探针：已 dump UV 平面（色度 pre/post 比对用）"
                ),
                Err(e) => tracing::warn!(
                    path = %uv_path.display(),
                    error = %e,
                    "帧探针：UV 写失败（Y 已写出）"
                ),
            }
        }
        ok
    }

    /// 给"提交后 dump"用：不受 raw 配额限制，但**每次启动最多写 `post_budget` 个**。
    pub fn record_post(&mut self, pts_ms: u64, frame: &DisplayFrame) -> bool {
        if !self.enabled() || !self.dump_raw || self.post_left == 0 {
            return false;
        }
        if self.record_plane_bytes(pts_ms, frame, "post") {
            self.post_left -= 1;
            return true;
        }
        false
    }

    /// 记录一帧：算摘要并写 `<dir>/<pts>.crc`（内容 `pts crc top bot`）。
    ///
    /// **返回 `true` 表示真的落盘成功**（测试与实验门槛都靠这个返回值判断，
    /// 而不是靠"调用了"）。
    pub fn record(&mut self, pts_ms: u64, frame: &DisplayFrame) -> bool {
        if !self.enabled() {
            return false;
        }
        let digest = match digest_frame(pts_ms, frame) {
            Ok(d) => d,
            Err(e) => {
                if !self.failed {
                    self.failed = true;
                    tracing::warn!(error = %e, "帧探针：取摘要失败，已停用（不影响显示）");
                }
                return false;
            }
        };
        let Some(dir) = &self.dir else {
            return false;
        };
        let path = dir.join(format!("{pts_ms}.crc"));
        let line = format!(
            "{} {} {} {}\n",
            digest.pts_ms, digest.crc, digest.crc_top, digest.crc_bottom
        );
        // 可选：原始平面字节（离线还原用；配额用完即停）
        // **同时写 UV 平面**（NV12 的 plane 1）——Y 与 UV 的布局/偏移都要能验证。
        if self.dump_raw && self.raw_left > 0 {
            if let Ok((bytes, _stride)) = map_plane_of(frame, 0) {
                let raw_path = dir.join(format!("{pts_ms}.raw"));
                if std::fs::write(&raw_path, &bytes).is_ok() {
                    self.raw_left -= 1;
                    tracing::info!(
                        path = %raw_path.display(),
                        bytes = bytes.len(),
                        "帧探针：已 dump 原始平面（还原比对用）"
                    );
                }
                // UV 平面（半高）。CPU 帧的 NV12 数据在同一个 buffer 里，
                // 起点 = stride * height；DMA-BUF 则按 offsets[1]。
                if let Ok((uv, uv_stride)) = map_plane_of(frame, 1)
                    && !uv.is_empty()
                {
                    let uv_path = dir.join(format!("{pts_ms}.uv"));
                    match std::fs::write(&uv_path, &uv) {
                        Ok(()) => tracing::info!(
                            path = %uv_path.display(),
                            bytes = uv.len(),
                            stride = uv_stride,
                            "帧探针：已 dump UV 平面（色度验证用）"
                        ),
                        Err(e) => {
                            tracing::warn!(path = %uv_path.display(), error = %e, "帧探针：UV 写失败")
                        }
                    }
                }
            }
        }
        match std::fs::write(&path, line) {
            Ok(()) => {
                self.written += 1;
                true
            }
            Err(e) => {
                if !self.failed {
                    self.failed = true;
                    tracing::warn!(path = %path.display(), error = %e, "帧探针：写文件失败，已停用");
                }
                false
            }
        }
    }
}

/// 供 daemon 调用的统一入口：对任意帧做只读摘要（DMA-BUF 走 mmap，CPU 直接算）。
///
/// # Errors
///
/// DMA-BUF 映射失败时返回 [`ProbeError`]。
pub fn digest_frame(pts_ms: u64, frame: &DisplayFrame) -> Result<FrameDigest, ProbeError> {
    match frame {
        DisplayFrame::Cpu(c) => Ok(digest_cpu(pts_ms, c)),
        DisplayFrame::DmaBuf(d) => digest_dmabuf(pts_ms, d),
        DisplayFrame::GpuTexture(_) => Err(ProbeError::BadGeometry(
            "GPU 纹理帧无 fd，无法 dump（本判据不适用）".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use liuguang_core::types::PixelFormat;
    use std::os::fd::OwnedFd;

    fn cpu_frame(rows: &[(u8, u8)]) -> CpuFrame {
        // 每行 2 字节：像素值 = 给定字节对
        let mut data = Vec::new();
        for (a, b) in rows {
            data.push(*a);
            data.push(*b);
        }
        CpuFrame {
            width: 2,
            height: rows.len() as u32,
            stride: 2,
            format: PixelFormat::Xrgb8888,
            data: data.into(),
        }
    }

    /// **UV 平面切片**：NV12 的 UV 必须紧跟 Y 之后（`stride × height`），不是从头取。
    ///
    /// 变异：把起点写成 0 ⇒ 本用例必须变红（那会把 Y 平面当 UV）。
    #[test]
    fn cpu_uv_plane_starts_after_y_plane() {
        let (w, h, stride) = (4usize, 4usize, 4usize);
        let mut data = vec![7u8; stride * h]; // Y 平面全 7
        data.extend(std::iter::repeat_n(9u8, stride * (h / 2))); // UV 平面全 9
        let f = CpuFrame {
            width: w as u32,
            height: h as u32,
            stride: stride as u32,
            format: PixelFormat::Nv12,
            data: data.into(),
        };
        let (y, _) = map_plane_of(&DisplayFrame::Cpu(f.clone()), 0).expect("Y");
        assert!(y.iter().all(|b| *b == 7), "Y 平面应全 7");
        let (uv, _) = map_plane_of(&DisplayFrame::Cpu(f), 1).expect("UV");
        assert_eq!(uv.len(), stride * (h / 2), "UV 长度应为半高");
        assert!(
            uv.iter().all(|b| *b == 9),
            "UV 平面应全 9（起点必须在 Y 之后）"
        );
    }

    /// **提交前后都必须 dump UV**（t79 补的缺口：`.crc` 只摘要 Y ⇒ 色度从未被逐字节测过）。
    ///
    /// 变异：`record_plane_bytes` 里只写 Y（去掉 UV 分支）⇒ 本用例必须变红
    /// （`.uv.post` 不存在）。
    #[test]
    fn post_dump_writes_both_y_and_uv_planes() {
        let dir = std::env::temp_dir().join(format!(
            "liuguang-probe-t79-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut probe = FrameProbe::from_dir(Some(dir.to_str().expect("utf8")));
        probe.configure_raw(Some("1"), Some("4"));
        assert!(probe.enabled());

        // NV12：Y 全 0x11，UV 全 0x22（两者**不同**，才能区分"dump 错了平面"）。
        let (w, h, stride) = (4u32, 4u32, 4u32);
        let mut data = vec![0x11u8; (stride * h) as usize];
        data.extend(std::iter::repeat_n(0x22u8, (stride * (h / 2)) as usize));
        let frame = DisplayFrame::Cpu(CpuFrame {
            width: w,
            height: h,
            stride,
            format: PixelFormat::Nv12,
            data: data.into(),
        });

        let pts = 4242u64;
        assert!(probe.record(pts, &frame), "提交前记录必须成功");
        assert!(probe.record_post(pts, &frame), "提交后 dump 必须成功");

        for name in [
            format!("{pts}.raw"),
            format!("{pts}.uv"),
            format!("{pts}.post"),
            format!("{pts}.uv.post"),
        ] {
            let path = dir.join(&name);
            assert!(
                path.exists(),
                "缺少 {name}（落盘文件：{:?}）",
                std::fs::read_dir(&dir).map(|d| d
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name())
                    .collect::<Vec<_>>())
            );
        }
        let y_post = std::fs::read(dir.join(format!("{pts}.post"))).expect("Y post");
        let uv_post = std::fs::read(dir.join(format!("{pts}.uv.post"))).expect("UV post");
        let uv_pre = std::fs::read(dir.join(format!("{pts}.uv"))).expect("UV pre");
        assert!(y_post.iter().all(|b| *b == 0x11), "post 的 Y 平面内容错");
        assert_eq!(
            uv_post.len(),
            (stride * (h / 2)) as usize,
            "UV post 长度应为半高"
        );
        assert!(
            uv_post.iter().all(|b| *b == 0x22),
            "post 的 UV 平面内容错（不是 Y）"
        );
        assert_eq!(
            uv_pre, uv_post,
            "同一缓冲未改写时 pre/post 的 UV 必须逐字节一致"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Y_TILED 软件还原（t56） ----

    /// 把线性图按给定布局**平铺**（`untile_y` 的逆运算，仅测试用）。
    fn tile_y(linear: &[u8], w: usize, h: usize, stride: usize, layout: TileLayout) -> Vec<u8> {
        let tiles_per_row = stride / TILE_BYTES_WIDE;
        let tile_rows = h.div_ceil(TILE_ROWS);
        let mut out = vec![0u8; tiles_per_row * tile_rows * TILE_BYTES];
        for y in 0..h {
            for x in 0..w {
                let idx = (y / TILE_ROWS) * tiles_per_row + x / TILE_BYTES_WIDE;
                let off =
                    idx * TILE_BYTES + layout.offset_in_tile(x % TILE_BYTES_WIDE, y % TILE_ROWS);
                out[off] = linear[y * w + x];
            }
        }
        out
    }

    /// **往返测试**：平铺 → 还原，必须逐像素完全一致（三种布局各一次）。
    ///
    /// 这证明 `untile_y` 与布局定义自洽（不依赖真机）。**变异**：把
    /// `offset_in_tile` 的任一分支写错 ⇒ 对应布局的往返必须变红。
    #[test]
    fn untile_y_roundtrips_for_every_candidate_layout() {
        let (w, h, stride) = (256usize, 96usize, 256usize); // 2 tiles/行，3 tile 行
        let linear: Vec<u8> = (0..(w * h)).map(|i| (i % 251) as u8).collect();
        for layout in TileLayout::ALL {
            let tiled = tile_y(&linear, w, h, stride, layout);
            let restored = untile_y(&tiled, w, h, stride, layout);
            assert_eq!(restored, linear, "布局 {} 往返必须一致", layout.name());
        }
    }

    /// **判据的判别力**：用**错误**布局还原 ⇒ 与真值**必须不同**。
    ///
    /// 这条是"让数据判决布局"的前提：若错误布局也能还原出正确图像，
    /// 那我们的判决就没有意义。
    #[test]
    fn wrong_layout_produces_different_pixels() {
        let (w, h, stride) = (256usize, 96usize, 256usize);
        // 用强纹理（不仅是常量）以免"错布局碰巧也对"
        let linear: Vec<u8> = (0..(w * h)).map(|i| ((i / 7) ^ (i / 13)) as u8).collect();
        let truth_layout = TileLayout::SubColumn16;
        let tiled = tile_y(&linear, w, h, stride, truth_layout);
        for wrong in TileLayout::ALL {
            let restored = untile_y(&tiled, w, h, stride, wrong);
            if wrong == truth_layout {
                assert_eq!(restored, linear, "正确布局必须一致");
            } else {
                let (ratio, _) = byte_match_ratio(&restored, &linear);
                assert!(
                    ratio < 0.99,
                    "错误布局 {} 竟还原出 {:.4} 一致率 ⇒ 判据无判别力",
                    wrong.name(),
                    ratio
                );
            }
        }
    }

    /// tile 网格常量必须能**自证**：`15 × 34 × 4096 = 2088960`（实测 `offsets[1]`）。
    #[test]
    fn tile_grid_constants_match_observed_uv_offset() {
        let tiles_per_row = 1920 / TILE_BYTES_WIDE;
        let tile_rows = 1080usize.div_ceil(TILE_ROWS);
        assert_eq!(tiles_per_row, 15);
        assert_eq!(tile_rows, 34);
        assert_eq!(
            tiles_per_row * tile_rows * TILE_BYTES,
            2_088_960,
            "Y 平面字节数必须等于实测 UV 偏移（外部可校验）"
        );
        assert_eq!(
            1920 * (34 * TILE_ROWS),
            2_088_960,
            "stride × ceil32(h) 亦相符"
        );
    }

    #[test]
    fn byte_match_ratio_reports_first_difference() {
        let (r, at) = byte_match_ratio(b"abcd", b"abcd");
        assert!((r - 1.0).abs() < f64::EPSILON);
        assert_eq!(at, None);
        let (r, at) = byte_match_ratio(b"abcd", b"abXd");
        assert!((r - 0.75).abs() < f64::EPSILON);
        assert_eq!(at, Some(2));
    }

    // ---- 帧探针落盘（可执行入口的可测部分） ----

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "liuguang-probe-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p
    }

    #[test]
    fn probe_is_disabled_without_env_and_costs_nothing() {
        let mut probe = FrameProbe::from_dir(None);
        assert!(!probe.enabled(), "未设环境变量必须停用");
        let f = DisplayFrame::Cpu(cpu_frame(&[(1, 2)]));
        assert!(!probe.record(1, &f), "停用时 record 必须返回 false");
        assert_eq!(probe.written(), 0);
        // 空串等价于未设
        assert!(!FrameProbe::from_dir(Some("   ")).enabled());
    }

    /// **核心可失败用例**：设了目录 ⇒ 真的会落盘，且文件格式与 runbook 一致。
    ///
    /// 变异（把 `record` 改成不写文件）⇒ 本用例必须变红。
    #[test]
    fn probe_writes_crc_file_in_runbook_format_when_enabled() {
        let dir = tmpdir("write");
        let mut probe = FrameProbe::from_dir(Some(dir.to_str().expect("utf8")));
        assert!(probe.enabled(), "设了目录必须启用");

        let f = DisplayFrame::Cpu(cpu_frame(&[(1, 2), (3, 4), (5, 6), (7, 8)]));
        assert!(probe.record(1234, &f), "启用时 record 必须真的写成功");
        assert_eq!(probe.written(), 1);

        let path = dir.join("1234.crc");
        let text = std::fs::read_to_string(&path).expect("文件必须存在");
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 4, "格式必须是 `pts crc top bot`：{text:?}");
        assert_eq!(fields[0], "1234", "第一字段是 pts");
        // 后三个必须能解析成 u64，且与直接算的摘要一致
        let expect = digest_cpu(1234, &cpu_frame(&[(1, 2), (3, 4), (5, 6), (7, 8)]));
        assert_eq!(fields[1].parse::<u64>().expect("crc"), expect.crc);
        assert_eq!(fields[2].parse::<u64>().expect("top"), expect.crc_top);
        assert_eq!(fields[3].parse::<u64>().expect("bottom"), expect.crc_bottom);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_records_dmabuf_frames_too() {
        let dir = tmpdir("dmabuf");
        let mut probe = FrameProbe::from_dir(Some(dir.to_str().expect("utf8")));
        let fd = memfd_with(&[9u8; 64]);
        let frame = DisplayFrame::DmaBuf(DmaBufFrame {
            width: 8,
            height: 8,
            stride: 8,
            format: PixelFormat::Xrgb8888,
            fds: vec![std::sync::Arc::new(fd)],
            object_index: [0; 4],
            keepalive: None,
            modifier: 0,
            fourcc: 0,
            strides: [8, 0, 0, 0],
            offsets: [0, 0, 0, 0],
        });
        assert!(probe.record(77, &frame), "直通臂也必须能 dump");
        let text = std::fs::read_to_string(dir.join("77.crc")).expect("文件必须存在");
        assert_eq!(text.split_whitespace().count(), 4, "{text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crc_is_deterministic_and_sensitive() {
        assert_eq!(crc64(b"abc"), crc64(b"abc"));
        assert_ne!(crc64(b"abc"), crc64(b"abd"));
        assert_ne!(crc64(b""), crc64(b"\0"));
    }

    #[test]
    fn cpu_digest_halves_are_split_at_half_height() {
        // 4 行：上 2 行全 1，下 2 行全 2
        let f = cpu_frame(&[(1, 1), (1, 1), (2, 2), (2, 2)]);
        let d = digest_cpu(10, &f);
        assert_ne!(d.crc_top, d.crc_bottom, "上下半区内容不同");
        assert_eq!(d.pts_ms, 10);
        assert_eq!(d.bytes, 8);
    }

    #[test]
    fn cpu_digest_ignores_row_padding_but_sees_visible_bytes() {
        // XRGB8888 宽 1 ⇒ 每行**可见** 4 字节；stride 8 ⇒ 行尾有 4 字节 padding。
        // 判据必须**排除 padding**：否则 CPU 臂（stride 常= width*bpp）与直通臂
        // （stride 按 128/256 对齐）会因 padding 不同而算出不同 CRC ⇒ 假阳性。
        let mk = |pad: [u8; 4]| CpuFrame {
            width: 1,
            height: 2,
            stride: 8,
            format: PixelFormat::Xrgb8888,
            data: vec![7u8, 0, 0, 0]
                .into_iter()
                .chain(pad)
                .chain([8u8, 0, 0, 0])
                .chain(pad)
                .collect::<Vec<u8>>()
                .into(),
        };
        let a = digest_cpu(0, &mk([9, 9, 9, 9]));
        let b = digest_cpu(0, &mk([1, 2, 3, 4]));
        assert_eq!(
            a.crc, b.crc,
            "行尾 padding 不同不得影响 CRC（否则跨臂比对会假阳性）"
        );

        // 但**可见字节**变化必须反映出来
        let mut c = mk([9, 9, 9, 9]);
        let mut data: Vec<u8> = c.data.to_vec();
        data[0] = 0xAA; // 第一行第一个可见字节
        c.data = data.into();
        assert_ne!(digest_cpu(0, &c).crc, a.crc, "可见字节变化必须改变 CRC");
    }

    #[test]
    fn cross_arm_same_pts_identical_means_content_ok() {
        let f = cpu_frame(&[(1, 2), (3, 4)]);
        let d = digest_cpu(100, &f);
        let verdict = compare_same_pts(&[d], &[d]);
        assert_eq!(verdict, vec![(100, CrossArmVerdict::Identical)]);
    }

    #[test]
    fn cross_arm_same_pts_differs_means_content_wrong() {
        let a = digest_cpu(100, &cpu_frame(&[(1, 2), (3, 4)]));
        let b = digest_cpu(100, &cpu_frame(&[(1, 2), (3, 5)]));
        let verdict = compare_same_pts(&[a], &[b]);
        match &verdict[0].1 {
            CrossArmVerdict::ContentDiffers { dmabuf, cpu } => {
                assert_ne!(dmabuf, cpu);
            }
            other => panic!("期望 ContentDiffers，得到 {other:?}"),
        }
    }

    #[test]
    fn cross_arm_missing_pts_is_incomparable_not_pass() {
        let a = digest_cpu(100, &cpu_frame(&[(1, 2)]));
        let b = digest_cpu(200, &cpu_frame(&[(1, 2)]));
        assert_eq!(
            compare_same_pts(&[a], &[b]),
            vec![(100, CrossArmVerdict::Incomparable)],
            "缺少同 pts 的对端必须是 Incomparable（不得当成通过）"
        );
    }

    #[test]
    fn halves_coherent_when_top_equals_bottom() {
        let f = cpu_frame(&[(5, 5), (5, 5), (5, 5), (5, 5)]);
        let d = digest_cpu(1, &f);
        assert_eq!(classify_halves(&d, &[]), HalvesVerdict::Coherent);
    }

    #[test]
    fn halves_detect_tearing_when_bottom_comes_from_next_frame() {
        // 帧 A：上 4 行全 1；帧 B：上 4 行全 2
        let a_full = cpu_frame(&[(1, 1), (1, 1), (1, 1), (1, 1)]);
        let b_full = cpu_frame(&[(2, 2), (2, 2), (2, 2), (2, 2)]);
        // 撕裂帧：上半来自 A，下半来自 B
        let torn = cpu_frame(&[(1, 1), (1, 1), (2, 2), (2, 2)]);
        let cur = digest_cpu(10, &torn);
        let a = digest_cpu(9, &a_full);
        let b = digest_cpu(11, &b_full);
        assert_eq!(
            classify_halves(&cur, &[a, b]),
            HalvesVerdict::TearingAgainst { other_pts_ms: 11 },
            "下半等于下一帧的上半 ⇒ 时序撕裂"
        );
    }

    #[test]
    fn halves_detect_spatial_misread() {
        // 本帧上下半区与另一帧**完全相同**（整帧就是那帧）⇒ 空间误读
        let same = cpu_frame(&[(3, 3), (3, 3), (4, 4), (4, 4)]);
        let cur = digest_cpu(10, &same);
        let other = digest_cpu(11, &same);
        assert_eq!(
            classify_halves(&cur, &[other]),
            HalvesVerdict::SpatialMisread { other_pts_ms: 11 }
        );
    }

    #[test]
    fn dmabuf_digest_reads_real_memory_through_fd() {
        // 用 memfd 造一段真实可 mmap 的"缓冲"
        let fd = memfd_with(&[7u8; 64]);
        let frame = DmaBufFrame {
            width: 8,
            height: 8,
            stride: 8,
            format: PixelFormat::Xrgb8888,
            fds: vec![std::sync::Arc::new(fd)],
            object_index: [0; 4],
            keepalive: None,
            modifier: 0,
            fourcc: 0,
            strides: [8, 0, 0, 0],
            offsets: [0, 0, 0, 0],
        };
        let d = digest_dmabuf(42, &frame).expect("mmap 应成功");
        assert_eq!(d.pts_ms, 42);
        assert_eq!(d.bytes, 64);
        assert_eq!(d.crc, crc64(&[7u8; 64]));
        // 上下半区（各 4 行 = 32 字节）内容相同 ⇒ 相等
        assert_eq!(d.crc_top, d.crc_bottom);
    }

    #[test]
    fn dmabuf_digest_reflects_content_changes() {
        let f1 = memfd_with(&[1u8; 32]);
        let f2 = memfd_with(&[2u8; 32]);
        let mk = |fd: OwnedFd| DmaBufFrame {
            width: 4,
            height: 4,
            stride: 8,
            format: PixelFormat::Xrgb8888,
            fds: vec![std::sync::Arc::new(fd)],
            object_index: [0; 4],
            keepalive: None,
            modifier: 0,
            fourcc: 0,
            strides: [8, 0, 0, 0],
            offsets: [0, 0, 0, 0],
        };
        let a = digest_dmabuf(1, &mk(f1)).expect("mmap");
        let b = digest_dmabuf(1, &mk(f2)).expect("mmap");
        assert_ne!(a.crc, b.crc, "内容不同 ⇒ CRC 必须不同");
    }

    #[test]
    fn dmabuf_bad_geometry_is_error_not_panic() {
        let fd = memfd_with(&[0u8; 16]);
        let frame = DmaBufFrame {
            width: 0,
            height: 0,
            stride: 0,
            format: PixelFormat::Xrgb8888,
            fds: vec![std::sync::Arc::new(fd)],
            object_index: [0; 4],
            keepalive: None,
            modifier: 0,
            fourcc: 0,
            strides: [0, 0, 0, 0],
            offsets: [0, 0, 0, 0],
        };
        assert!(matches!(
            digest_dmabuf(0, &frame),
            Err(ProbeError::BadGeometry(_))
        ));
    }

    #[test]
    fn gpu_texture_is_explicitly_unsupported() {
        let f = DisplayFrame::GpuTexture(liuguang_core::types::GpuFrame {
            width: 1,
            height: 1,
            format: PixelFormat::Xrgb8888,
            texture_id: 1,
        });
        assert!(matches!(
            digest_frame(0, &f),
            Err(ProbeError::BadGeometry(_))
        ));
    }

    /// 用普通临时文件造一段可 mmap 的缓冲（替代真实 DMA-BUF，测试用）。
    ///
    /// 真实 DMA-BUF 也是"可 mmap 的 fd"，因此这条路径与真机一致；
    /// 不引入 memfd 是为了避免为测试新增依赖。
    fn memfd_with(bytes: &[u8]) -> OwnedFd {
        use std::io::Write as _;
        let mut path = std::env::temp_dir();
        path.push(format!(
            "liuguang-probe-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // 必须读写打开：`File::create` 是只写 ⇒ `mmap(PROT_READ)` 会 EACCES。
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create");
        file.write_all(bytes).expect("write");
        file.flush().expect("flush");
        let _ = std::fs::remove_file(&path); // fd 仍有效（unlink 后仍可 mmap）
        OwnedFd::from(file)
    }
}
