//! DRM master 状态、占用者诊断与失败日志限速。
//!
//! # 背景（真机已确证）
//!
//! 内核只在**第一个打开**主设备节点的进程上自动授予 DRM master
//! （`dev->master == NULL` 时）。若启动时已有别的进程持有 master，本进程
//! 既拿不到 master，也**不会**在提交时才失败——于是出现"进程自认为正常
//! （`presented` 在涨）但屏幕全黑"的静默失败。
//!
//! 更糟的是失败日志逐帧打印（真机实测 ~140 行/秒），把真正的原因淹没。
//! 本模块提供三件事：
//!
//! 1. [`MasterStatus`]：显式获取 master 的结果 + **谁占着设备**的诊断；
//! 2. [`scan_device_holders`]：扫描 `/proc/*/fd` 找出打开该设备的进程
//!    （给出可直接操作的 PID/命令名，例如"先 `systemctl stop liuguang`"）；
//! 3. [`RateLimiter`]：失败日志限速（前 N 次逐条 + 之后按最小间隔打印一次，
//!    并附"已抑制 M 次"），把洪泛变成可读信息。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// DRM master 状态与诊断信息。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MasterStatus {
    /// 最近一次已知的 master 状态（`None` = 尚未确定）。
    ///
    /// 判定来源按可信度排序：显式 `DRM_IOCTL_SET_MASTER` 的结果、
    /// 提交成功（⇒ 是 master）、提交 `EACCES`（⇒ 不是 master）。
    pub master: Option<bool>,
    /// 显式获取尝试的结果（人类可读；失败时含 errno 语义）。
    pub acquire_result: String,
    /// 打开该设备的其它进程（尽力而为：能看到 `/proc/<pid>/fd` 的进程）。
    pub holders: Vec<String>,
    /// 自愈尝试累计次数（t26：daemon 周期调用 `poll_master_recovery` 的产物）。
    ///
    /// 让"自愈到底有没有在跑"变成**可读的计数**，而不是只能靠翻日志（t14 的
    /// "70s 内 0 次尝试"就是因为没有任何计数可看）。
    pub heal_attempts: u64,
    /// 自愈尝试里"仍未取得 master"的累计次数。
    pub heal_failures: u64,
}

impl MasterStatus {
    /// 是否**确定**不是 master。
    pub fn is_known_non_master(&self) -> bool {
        self.master == Some(false)
    }

    /// 是否**值得再尝试**自愈。
    ///
    /// 只有"已确认 master（`Some(true)`）"才停止重试；`None`（未知）与
    /// `Some(false)`（确定不是）都要继续试——否则会出现 t26 的第二个坑：
    /// 第一次重取失败后状态被置成"未知"，之后就**再也不试**，
    /// 占用者退出后屏幕永远不恢复。
    pub fn should_retry_heal(&self) -> bool {
        self.master != Some(true)
    }

    /// 生成面向运维的可操作提示（含占用者）。
    pub fn remedy_hint(&self) -> String {
        if self.holders.is_empty() {
            "未发现同用户可见的占用进程；可用 `sudo cat /sys/kernel/debug/dri/<n>/clients` 查看 master=y 的进程，\
             或直接重启本服务"
                .to_string()
        } else {
            format!(
                "检测到其它进程正打开该设备：{}；请先停止它们（例如 `systemctl stop liuguang` 或 \
                 `pkill -f liuguang`），再启动本服务",
                self.holders.join(", ")
            )
        }
    }
}

/// 扫描 `/proc` 找出打开了 `device` 的进程。
///
/// 返回形如 `PID 1234 (liuguang)` 的可读条目（最多 [`MAX_HOLDERS`] 条）。
/// 只覆盖当前用户/权限范围内可见的进程——看不到的进程交由运维用
/// `/sys/kernel/debug/dri/<n>/clients` 排查（该文件需要 root）。
pub fn scan_device_holders(device: &Path) -> Vec<String> {
    scan_holders_in(Path::new("/proc"), device)
}

/// 同上，但**排除本进程**（否则必然把自己列成"占用者"，产生假警报）。
pub fn scan_device_holders_excluding_self(device: &Path) -> Vec<String> {
    scan_holders_in_excluding(Path::new("/proc"), device, Some(std::process::id()))
}

/// [`scan_device_holders`] 的可测试版本：`proc_root` 一般传 `/proc`。
pub(crate) fn scan_holders_in(proc_root: &Path, device: &Path) -> Vec<String> {
    scan_holders_in_excluding(proc_root, device, None)
}

/// 真正的实现：可排除一个 PID（本进程）。
pub(crate) fn scan_holders_in_excluding(
    proc_root: &Path,
    device: &Path,
    exclude_pid: Option<u32>,
) -> Vec<String> {
    let canonical = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
    let mut found: Vec<(u32, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if exclude_pid == Some(pid) {
            continue;
        }
        let fds = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fds) else {
            continue;
        };
        let holds = fds.flatten().any(|fd| {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                return false;
            };
            target == canonical || target == device
        });
        if holds {
            found.push((pid, process_name(entry.path().as_path())));
        }
        if found.len() >= MAX_HOLDERS {
            break;
        }
    }
    found.sort_by_key(|(pid, _)| *pid);
    found
        .into_iter()
        .map(|(pid, name)| format!("PID {pid} ({name})"))
        .collect()
}

/// 最多列出多少个占用进程（避免刷屏）。
pub const MAX_HOLDERS: usize = 4;

fn process_name(proc_dir: &Path) -> String {
    let Ok(comm) = std::fs::read_to_string(proc_dir.join("comm")) else {
        return "unknown".to_string();
    };
    comm.trim().to_string()
}

/// 失败日志限速器：前 `burst` 次逐条放行，之后每 `min_interval` 最多一条。
///
/// 时间由调用方注入（`now`），因此可在无硬件环境完整单测。
#[derive(Debug)]
pub struct RateLimiter {
    min_interval: Duration,
    burst: u32,
    seen: u64,
    last_allowed: Option<Instant>,
    suppressed: u64,
}

impl RateLimiter {
    /// 构造：`burst` 次突发 + `min_interval` 之后的最小间隔。
    pub fn new(burst: u32, min_interval: Duration) -> Self {
        Self {
            min_interval,
            burst,
            seen: 0,
            last_allowed: None,
            suppressed: 0,
        }
    }

    /// 记录一次事件；返回 `Some(自上次放行以来被抑制的次数)` 表示**这次应当打印**，
    /// 返回 `None` 表示应抑制。
    pub fn check(&mut self, now: Instant) -> Option<u64> {
        self.seen = self.seen.saturating_add(1);
        let first_burst = self.seen <= u64::from(self.burst);
        let interval_ok = match self.last_allowed {
            None => true,
            Some(last) => now.duration_since(last) >= self.min_interval,
        };
        if first_burst || interval_ok {
            self.last_allowed = Some(now);
            let suppressed = self.suppressed;
            self.suppressed = 0;
            Some(suppressed)
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }

    /// 累计被抑制的次数（用于"已抑制 N 次"的最终汇总）。
    pub fn suppressed_total(&self) -> u64 {
        self.suppressed
    }
}

/// 设备路径 → 便于日志阅读的短名（如 `/dev/dri/card1`）。
pub fn device_short_name(device: &Path) -> String {
    device
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| device.display().to_string())
}

/// 汇总 `/proc` 扫描结果（去重、稳定排序；便于测试）。
pub(crate) fn dedup_holders(holders: Vec<String>) -> Vec<String> {
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut out = Vec::new();
    for h in holders {
        if seen.insert(h.clone(), ()).is_none() {
            out.push(h);
        }
    }
    out.sort();
    out
}

/// 便捷构造：把扫描结果封进 [`MasterStatus`]。
pub fn status_with_holders(
    master: Option<bool>,
    acquire_result: String,
    device: &Path,
) -> MasterStatus {
    MasterStatus {
        master,
        acquire_result,
        holders: dedup_holders(scan_device_holders(device)),
        heal_attempts: 0,
        heal_failures: 0,
    }
}

/// 诊断用：设备路径（供上层日志使用）。
pub fn device_path(device: &Path) -> PathBuf {
    std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_burst_then_interval() {
        let mut rl = RateLimiter::new(3, Duration::from_secs(1));
        let t0 = Instant::now();
        // 前 3 次全部放行。
        assert_eq!(rl.check(t0), Some(0));
        assert_eq!(rl.check(t0), Some(0));
        assert_eq!(rl.check(t0), Some(0));
        // 之后被抑制，并累计计数。
        assert_eq!(rl.check(t0), None);
        assert_eq!(rl.check(t0 + Duration::from_millis(100)), None);
        assert_eq!(rl.suppressed_total(), 2);
        // 超过最小间隔后放行，并汇报抑制次数。
        assert_eq!(rl.check(t0 + Duration::from_secs(1)), Some(2));
        assert_eq!(rl.suppressed_total(), 0);
    }

    #[test]
    fn rate_limiter_caps_flood_to_one_per_interval() {
        // 模拟真机 140 行/秒的洪泛：限速后每 2 秒最多 1 条。
        let mut rl = RateLimiter::new(5, Duration::from_secs(2));
        let start = Instant::now();
        let mut printed = 0;
        let mut suppressed = 0u64;
        for i in 0..1400u64 {
            let now = start + Duration::from_millis(i * 1000 / 140);
            if let Some(s) = rl.check(now) {
                printed += 1;
                suppressed += s;
            }
        }
        suppressed += rl.suppressed_total();
        // 10 秒 × 每 2 秒 1 条 + 5 次突发 ≈ 10 条（允许少量边界差）。
        assert!(printed <= 12, "限速后最多打印 12 条，实际 {printed}");
        assert_eq!(printed + suppressed as usize, 1400, "每次事件都被计数");
    }

    #[test]
    fn holder_scan_finds_symlink_openers() {
        // 造一个假 /proc 树：<tmp>/1234/fd/3 -> <tmp>/card1
        let base =
            std::env::temp_dir().join(format!("liuguang-master-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dev = base.join("card1");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&dev, b"").unwrap();
        let pid_dir = base.join("1234");
        std::fs::create_dir_all(pid_dir.join("fd")).unwrap();
        std::fs::write(pid_dir.join("comm"), b"liuguang\n").unwrap();
        std::os::unix::fs::symlink(&dev, pid_dir.join("fd/3")).unwrap();
        // 另一个进程不打开设备。
        let other = base.join("4321");
        std::fs::create_dir_all(other.join("fd")).unwrap();
        std::fs::write(other.join("comm"), b"bash\n").unwrap();

        let holders = scan_holders_in(&base, &dev);
        assert_eq!(holders, vec!["PID 1234 (liuguang)".to_string()]);
        // 排除本进程：把 1234 当作"自己"时应看不到占用者。
        assert!(scan_holders_in_excluding(&base, &dev, Some(1234)).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remedy_hint_is_actionable() {
        let mut st = MasterStatus {
            master: Some(false),
            acquire_result: "EACCES".to_string(),
            holders: vec![],
            heal_attempts: 0,
            heal_failures: 0,
        };
        assert!(st.remedy_hint().contains("clients"));
        st.holders = vec!["PID 42 (liuguang)".to_string()];
        let hint = st.remedy_hint();
        assert!(hint.contains("PID 42 (liuguang)"), "实际: {hint}");
        assert!(hint.contains("systemctl stop liuguang"), "实际: {hint}");
        assert!(!MasterStatus::default().is_known_non_master());
        assert!(st.is_known_non_master());
        // t26：自愈重试判据——只有"已确认 master"才停手。
        assert!(
            MasterStatus::default().should_retry_heal(),
            "未知状态必须继续尝试（否则占用者退出后永不恢复）"
        );
        assert!(
            MasterStatus {
                master: Some(false),
                ..MasterStatus::default()
            }
            .should_retry_heal(),
            "确定不是 master 时必须继续尝试"
        );
        assert!(
            !MasterStatus {
                master: Some(true),
                ..MasterStatus::default()
            }
            .should_retry_heal(),
            "已确认 master 时不该白白重开设备"
        );
    }

    #[test]
    fn short_name_is_last_component() {
        assert_eq!(device_short_name(Path::new("/dev/dri/card1")), "card1");
    }
}
