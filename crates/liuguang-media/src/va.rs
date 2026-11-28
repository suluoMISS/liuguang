//! VA-API 驱动能力探测：H.264 硬解前提的**显式守卫**。
//!
//! 为什么必须有这一层（真机实测结论，规范 §6.1/§6.2）：
//! - GStreamer 的 `va` 插件**按当前 VA 驱动能力惰性注册元素**：驱动不宣告
//!   `VAProfileH264*: VAEntrypointVLD` 时，`vah264dec` 在注册表里根本不存在。
//!   因此"元素是否存在"只能作为**当前环境**的判据，不能作为唯一判据。
//! - VA 驱动由 libva 按搜索顺序选择（`dri-nonfree` → `dri-freeworld` → `dri`）。
//!   本函数在**进程外**运行 `vainfo`（每个候选驱动一次）读出真实能力，
//!   从而知道"当前默认驱动到底能不能硬解 H.264"，而不是靠猜。
//! - 只有默认驱动**不具备** H.264 能力、而某个已知驱动具备时，才把
//!   `LIBVA_DRIVER_NAME` 固化到该驱动（并大声记录），保证不需要任何手工
//!   `export`。默认驱动已经具备能力时**不改写环境**（手工硬编码 i965 反而
//!   会绕过更新的驱动实现）。
//!
//! 该模块不做任何解码、不依赖 `gstreamer` feature，可离线单元测试。

use std::sync::OnceLock;

/// `vainfo` 一次调用的解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct VaInfo {
    /// 驱动自报版本（`vainfo: Driver version: ...`）。
    version: Option<String>,
    /// 宣告 `VAEntrypointVLD` 的 `VAProfileH264*` 行。
    h264_profiles: Vec<String>,
}

/// VA 能力探测结论（进程内缓存一份）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VaProbe {
    /// 探测前环境里的 `LIBVA_DRIVER_NAME`（`None` = 未设置，交给 libva 自动选）。
    pub env_before: Option<String>,
    /// 建议使用的驱动：`Some(name)` 表示应设置 `LIBVA_DRIVER_NAME=name`；
    /// `None` = 保持 libva 自动选择（默认驱动即具备 H.264 能力）。
    pub recommended: Option<String>,
    /// 建议驱动自报版本。
    pub driver_version: Option<String>,
    /// 建议驱动是否宣告 H.264 VLD（硬解可用性）。
    pub h264_vld: bool,
    /// 建议驱动宣告的 H.264 profile 行（诊断用）。
    pub h264_profiles: Vec<String>,
    /// 是否真的跑过 `vainfo`（`vainfo` 缺失时为 false：结论**未知**，不是"没有"）。
    pub probed: bool,
    /// 人类可读结论（进日志）。
    pub note: String,
}

impl VaProbe {
    /// 探测结论：H.264 硬解可用。
    pub const fn h264_hardware_available(&self) -> bool {
        self.h264_vld
    }

    /// 日志/`status` 用的简短描述。
    pub fn summary(&self) -> String {
        if !self.probed {
            return format!("VA 能力未知（{}）", self.note);
        }
        let driver = self
            .recommended
            .clone()
            .or_else(|| self.env_before.clone())
            .unwrap_or_else(|| "libva 自动".to_string());
        if self.h264_vld {
            format!(
                "VA 驱动 {driver}（{}）宣告 {} 项 H.264 VLD profile → 硬解可用",
                self.driver_version.as_deref().unwrap_or("版本未知"),
                self.h264_profiles.len()
            )
        } else {
            format!(
                "VA 驱动 {driver}（{}）无 H.264 VLD profile → 硬解不可用",
                self.driver_version.as_deref().unwrap_or("版本未知")
            )
        }
    }
}

/// 驱动能力探测结果（首次调用执行探测，之后返回缓存）。
///
/// 探测在**进程外**运行 `vainfo`，不改变本进程环境；需要固化驱动时另外调用
/// [`apply_recommended_driver`]。
pub fn probe() -> &'static VaProbe {
    static PROBE: OnceLock<VaProbe> = OnceLock::new();
    PROBE.get_or_init(probe_impl)
}

/// 按探测结论固化 `LIBVA_DRIVER_NAME`；返回是否真的改写了环境。
///
/// # 前置条件（重要）
///
/// 必须在**进程早期、其它线程启动之前**调用：`set_var` 与并发的 `getenv`
/// 竞争是未定义行为（Rust 2024 因此把 `set_var` 标记为 unsafe）。
/// `liuguang` 在 `daemon::run` 开头（IPC 线程创建之前）调用一次。
///
/// 默认驱动已具备 H.264 能力时不做任何改写（不硬编码任何驱动名）。
pub fn apply_recommended_driver(probe: &VaProbe) -> bool {
    let Some(name) = probe.recommended.as_deref() else {
        return false;
    };
    if env_driver().as_deref() == Some(name) {
        return false;
    }
    set_libva_driver(name);
    true
}

/// 当前进程环境里的 `LIBVA_DRIVER_NAME`（空串视为未设置）。
fn env_driver() -> Option<String> {
    std::env::var("LIBVA_DRIVER_NAME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 写入 `LIBVA_DRIVER_NAME`。
///
/// # Safety
///
/// 见 [`apply_recommended_driver`] 的前置条件：调用点必须是单线程阶段。
#[allow(unsafe_code)]
fn set_libva_driver(name: &str) {
    // SAFETY: 调用点位于进程早期（daemon::run 起始处，IPC 线程创建之前），
    // 此时不存在并发读取环境变量的线程。
    unsafe { std::env::set_var("LIBVA_DRIVER_NAME", name) };
}

/// VA 驱动 `.so` 可能安装的目录（Fedora/RPM Fusion、Arch、Debian/Ubuntu x86_64/aarch64）。
const DRIVER_DIRS: [&str; 6] = [
    "/usr/lib/dri",
    "/usr/lib64/dri",
    "/usr/lib64/dri-nonfree",
    "/usr/lib/x86_64-linux-gnu/dri",
    "/usr/lib/aarch64-linux-gnu/dri",
    "/usr/lib32/dri",
];

/// 候选驱动顺序：显式环境变量 → libva 自动 → 已知回退驱动（按安装情况）。
///
/// 探测是"谁先宣告 H.264 VLD 用谁"，所以顺序即优先级：
/// 尊重运维显式设置，其次尊重系统默认，最后才用 legacy 回退驱动。
fn candidate_drivers(env_before: Option<&str>) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = Vec::new();
    if let Some(e) = env_before {
        out.push(Some(e.to_string()));
    }
    out.push(None);
    for name in ["i965", "iHD"] {
        if env_before == Some(name) || !driver_installed(name) {
            continue;
        }
        out.push(Some(name.to_string()));
    }
    out
}

/// 该驱动是否安装（`<name>_drv_video.so` 存在于任一已知目录）。
fn driver_installed(name: &str) -> bool {
    let file = format!("{name}_drv_video.so");
    DRIVER_DIRS
        .iter()
        .any(|dir| std::path::Path::new(dir).join(&file).is_file())
}

#[cfg(target_os = "linux")]
fn probe_impl() -> VaProbe {
    if std::env::var("LIUGUANG_VA_PROBE")
        .map(|v| v.trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
    {
        return VaProbe {
            env_before: env_driver(),
            probed: false,
            note: "探测被 LIUGUANG_VA_PROBE=off 关闭".to_string(),
            ..VaProbe::default()
        };
    }
    let env_before = env_driver();
    let mut ran_any = false;
    let mut last_version: Option<String> = None;
    for candidate in candidate_drivers(env_before.as_deref()) {
        let Some(info) = run_vainfo(candidate.as_deref()) else {
            continue;
        };
        ran_any = true;
        if let Some(v) = info.version.clone() {
            last_version = Some(v);
        }
        if info.h264_profiles.is_empty() {
            continue;
        }
        return VaProbe {
            env_before,
            recommended: candidate,
            driver_version: info.version,
            h264_vld: true,
            h264_profiles: info.h264_profiles,
            probed: true,
            note: "探测到可硬解 H.264 的 VA 驱动".to_string(),
        };
    }
    VaProbe {
        env_before,
        recommended: None,
        driver_version: last_version,
        h264_vld: false,
        h264_profiles: Vec::new(),
        probed: ran_any,
        note: if ran_any {
            "所有候选 VA 驱动都不宣告 H.264 VLD profile".to_string()
        } else {
            "vainfo 不可用，无法探测".to_string()
        },
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_impl() -> VaProbe {
    VaProbe {
        env_before: env_driver(),
        probed: false,
        note: "非 Linux 平台不做 VA 探测".to_string(),
        ..VaProbe::default()
    }
}

/// 以指定驱动运行 `vainfo` 并解析能力；`vainfo` 缺失/失败返回 `None`。
#[cfg(target_os = "linux")]
fn run_vainfo(driver: Option<&str>) -> Option<VaInfo> {
    use std::io::Read as _;
    let mut cmd = std::process::Command::new("vainfo");
    match driver {
        Some(name) => cmd.env("LIBVA_DRIVER_NAME", name),
        None => cmd.env_remove("LIBVA_DRIVER_NAME"),
    };
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    const MAX_BYTES: u64 = 256 * 1024;
    let out_reader = child.stdout.take().map(|s| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = s.take(MAX_BYTES).read_to_end(&mut b);
            b
        })
    });
    let err_reader = child.stderr.take().map(|s| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = s.take(MAX_BYTES).read_to_end(&mut b);
            b
        })
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(h) = out_reader {
                    let _ = h.join();
                }
                if let Some(h) = err_reader {
                    let _ = h.join();
                }
                return None;
            }
        }
    }
    let stdout = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&stderr));
    Some(parse_vainfo(&text))
}

/// 解析 `vainfo` 输出：驱动版本 + 宣告 VLD 的 H.264 profile 行。
fn parse_vainfo(text: &str) -> VaInfo {
    let mut info = VaInfo::default();
    for line in text.lines() {
        if let Some(rest) = line.split("Driver version:").nth(1) {
            let v = rest.trim();
            if !v.is_empty() {
                info.version = Some(v.to_string());
            }
        }
        let trimmed = line.trim();
        if trimmed.starts_with("VAProfileH264") && trimmed.contains("VAEntrypointVLD") {
            info.h264_profiles.push(trimmed.to_string());
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    const I965_LIKE: &str = "\
libva info: VA-API version 1.23.0
vainfo: VA-API version: 1.23 (libva 2.23.0)
vainfo: Driver version: Intel i965 driver for Intel(R) Coffee Lake - 2.4.0.pre1
vainfo: Supported profile and entrypoints
      VAProfileMPEG2Simple            :\tVAEntrypointVLD
      VAProfileH264Main               :\tVAEntrypointVLD
      VAProfileH264High               :\tVAEntrypointVLD
      VAProfileH264High               :\tVAEntrypointEncSlice
      VAProfileVP9Profile0            :\tVAEntrypointVLD
";

    const IHD_NO_H264: &str = "\
error: can't connect to X server!
vainfo: VA-API version: 1.23 (libva 2.23.0)
vainfo: Driver version: Intel iHD driver for Intel(R) Gen Graphics - 26.3.5 ()
vainfo: Supported profile and entrypoints
      VAProfileMPEG2Simple            :\tVAEntrypointVLD
      VAProfileVP9Profile2            :\tVAEntrypointVLD
";

    #[test]
    fn parses_h264_vld_profiles_only() {
        let info = parse_vainfo(I965_LIKE);
        assert_eq!(info.h264_profiles.len(), 2, "只统计 VLD 的 H.264 行");
        assert!(
            info.h264_profiles
                .iter()
                .all(|l| l.contains("VAEntrypointVLD"))
        );
        assert!(
            !info
                .h264_profiles
                .iter()
                .any(|l| l.contains("VAEntrypointEncSlice")),
            "编码入口点不算解码能力"
        );
        assert_eq!(
            info.version.as_deref(),
            Some("Intel i965 driver for Intel(R) Coffee Lake - 2.4.0.pre1")
        );
    }

    #[test]
    fn reports_no_h264_when_driver_lacks_profiles() {
        let info = parse_vainfo(IHD_NO_H264);
        assert!(info.h264_profiles.is_empty());
        assert!(info.version.is_some(), "版本仍应解析出来用于诊断");
    }

    #[test]
    fn parses_empty_and_garbage_without_panicking() {
        for text in ["", "garbage", "VAProfileH264High:\tVAEntrypointVLD"] {
            let info = parse_vainfo(text);
            let _ = info.version;
        }
        // 缺少 tab 分隔的行也应被识别（宽松匹配）。
        let loose = parse_vainfo("VAProfileH264High: VAEntrypointVLD");
        assert_eq!(loose.h264_profiles.len(), 1);
    }

    #[test]
    fn candidate_order_prefers_explicit_then_auto() {
        let c = candidate_drivers(Some("i965"));
        assert_eq!(c.first(), Some(&Some("i965".to_string())));
        assert_eq!(c.get(1), Some(&None), "第二个候选是 libva 自动选择");
        assert_eq!(
            c.iter().flatten().filter(|n| *n == "i965").count(),
            1,
            "显式设置的驱动不重复探测"
        );

        let auto = candidate_drivers(None);
        assert_eq!(auto.first(), Some(&None), "未设置时先试系统默认驱动");
        assert!(
            auto.iter().flatten().filter(|n| *n == "iHD").count() <= 1,
            "回退驱动最多出现一次"
        );
    }

    #[test]
    fn probe_is_cached_and_describes_itself() {
        // 真实环境探测（可能成功也可能因无 vainfo 而"未知"，两者都合法）。
        let p = probe();
        let again = probe();
        assert!(
            std::ptr::eq(p, again),
            "探测结果应缓存（进程内只跑一次 vainfo）"
        );
        let s = p.summary();
        assert!(!s.is_empty());
        if !p.probed {
            assert!(s.contains("未知"), "未知状态必须如实标注：{s}");
        }
    }

    #[test]
    fn summary_marks_available_and_unavailable() {
        let ok = VaProbe {
            probed: true,
            h264_vld: true,
            recommended: Some("i965".into()),
            driver_version: Some("Intel i965 - 2.4.0".into()),
            h264_profiles: vec!["VAProfileH264High".into()],
            note: String::new(),
            env_before: None,
        };
        assert!(ok.h264_hardware_available());
        assert!(ok.summary().contains("硬解可用"));

        let bad = VaProbe {
            probed: true,
            h264_vld: false,
            recommended: None,
            driver_version: Some("Intel iHD - 26.3.5".into()),
            h264_profiles: Vec::new(),
            note: String::new(),
            env_before: None,
        };
        assert!(!bad.h264_hardware_available());
        assert!(bad.summary().contains("硬解不可用"));
    }

    #[test]
    fn apply_uses_probe_recommendation_only() {
        // 不触碰真实环境：recommended=None 时不得改写任何东西。
        let none = VaProbe {
            probed: true,
            h264_vld: true,
            recommended: None,
            ..VaProbe::default()
        };
        assert!(
            !apply_recommended_driver(&none),
            "默认驱动已具备能力时不得写 LIBVA_DRIVER_NAME"
        );
    }
}
