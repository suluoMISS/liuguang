//! GPU 显存压力探针与「让路」状态机（`performance.pause_when_gpu_memory_above`）。
//!
//! **为什么要有它**：2026-11-23 真机实测——用户在运行大型 3D 游戏时显存占用
//! 7692/8188 MiB（91%），我们仍在 4K60 解码 + GPU 转 RGB，`vaExportSurfaceHandle`
//! 报 `resource allocation failed（VAStatus=2）`，随后 NVIDIA 驱动刷
//! `NVRM: ... gpu_vaspace.c` 断言、桌面卡死（用户按电源键重启）。denial 合成器
//! 不提供窗口全屏状态（只有 `ext_foreign_toplevel_list_v1`，没有 fullscreen 位），
//! 所以「游戏全屏就让路」在这台机器上不可实现；显存占用是**唯一可观测**的等价信号。
//!
//! **口径**：优先 `nvidia-smi --query-gpu=memory.used,memory.total --format=csv,noheader,nounits`
//! 的 `used/total`（MiB）；无 NVIDIA 时回退 AMD amdgpu sysfs（`mem_info_vram_used` /
//! `mem_info_vram_total`）。3 秒采一次（独立线程，tick 只读缓存值，绝不阻塞主循环）。
//! 两类探针都不可用时采样线程立刻退出，状态机永远是 `Unchanged`。
//!
//! **阈值是策略**：超过 `above` 就暂停视频供给（保住桌面，别把驱动逼到断言），
//! 回落到 `below` 才恢复；两者之间保持现状（滞回，避免在边界来回抖）。默认
//! 85/75；`above <= 0` 或 `above <= below` 视为关闭。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 采样间隔。3 秒足够跟上游戏的显存爬升，也不会让 `nvidia-smi` 进程刷屏。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);

/// 一次显存读数（MiB）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// 已用显存（MiB）。
    pub used_mib: u64,
    /// 总显存（MiB）。
    pub total_mib: u64,
}

impl Usage {
    /// 占用百分比（`total_mib == 0` 时为 0）。
    #[must_use]
    pub fn percent(&self) -> f32 {
        if self.total_mib == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let used = self.used_mib as f32;
        #[allow(clippy::cast_precision_loss)]
        let total = self.total_mib as f32;
        used / total * 100.0
    }
}

/// 状态机输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 进入让路：暂停在播的视频。
    Pause,
    /// 退出让路：恢复此前由本组件暂停的通道。
    Resume,
    /// 不变。
    Unchanged,
}

/// 解析 `nvidia-smi --query-gpu=memory.used,memory.total` 的一行输出。
///
/// 形如 `7692, 8188`（`nounits`）。任何解析失败返回 `None`——宁可不动，
/// 也不要在没有事实时乱暂停。
#[must_use]
pub fn parse_usage(line: &str) -> Option<Usage> {
    let mut it = line.split(',');
    let used = it.next()?.trim().parse::<u64>().ok()?;
    let total = it.next()?.trim().parse::<u64>().ok()?;
    if total == 0 {
        return None;
    }
    Some(Usage {
        used_mib: used,
        total_mib: total,
    })
}

/// 解析 AMD sysfs `mem_info_vram_used` / `mem_info_vram_total`（字节字符串 → MiB）。
#[must_use]
pub fn parse_sysfs_vram(used_bytes: &str, total_bytes: &str) -> Option<Usage> {
    let used = used_bytes.trim().parse::<u64>().ok()?;
    let total = total_bytes.trim().parse::<u64>().ok()?;
    if total == 0 {
        return None;
    }
    Some(Usage {
        used_mib: used / (1024 * 1024),
        total_mib: (total / (1024 * 1024)).max(1),
    })
}

#[derive(Debug, Clone)]
enum Backend {
    Nvidia,
    AmdSysfs(PathBuf),
}

fn detect_backend() -> Option<Backend> {
    if query_nvidia().is_some() {
        return Some(Backend::Nvidia);
    }
    find_amd_vram_dir(Path::new("/sys/class/drm")).map(Backend::AmdSysfs)
}

fn find_amd_vram_dir(drm_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(drm_dir).ok()?;
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("card") && !name.contains('-'))
        })
        .collect();
    dirs.sort();
    for card in dirs {
        let dev = card.join("device");
        if dev.join("mem_info_vram_used").is_file() && dev.join("mem_info_vram_total").is_file() {
            return Some(dev);
        }
    }
    None
}

fn query_amd_sysfs(dev_dir: &Path) -> Option<Usage> {
    let used = std::fs::read_to_string(dev_dir.join("mem_info_vram_used")).ok()?;
    let total = std::fs::read_to_string(dev_dir.join("mem_info_vram_total")).ok()?;
    parse_sysfs_vram(&used, &total)
}

/// 带滞回的显存压力状态机。
#[derive(Debug)]
pub struct Guard {
    above: f32,
    below: f32,
    engaged: bool,
}

impl Guard {
    /// 用阈值构造；非法组合（`above <= 0` 或 `above <= below`）返回 `None`（= 关闭）。
    #[must_use]
    pub fn new(above: f32, below: f32) -> Option<Self> {
        if above <= 0.0 || above <= below {
            return None;
        }
        Some(Self {
            above,
            below,
            engaged: false,
        })
    }

    /// 喂一次读数，返回需要执行的动作。
    pub fn decide(&mut self, usage: Option<Usage>) -> Action {
        let Some(usage) = usage else {
            return Action::Unchanged;
        };
        let percent = usage.percent();
        if !self.engaged && percent >= self.above {
            self.engaged = true;
            return Action::Pause;
        }
        if self.engaged && percent <= self.below {
            self.engaged = false;
            return Action::Resume;
        }
        Action::Unchanged
    }
}

/// 采样器的只读句柄：tick 里读缓存值，永不阻塞。
#[derive(Clone, Debug)]
pub struct Sampler {
    latest: Arc<Mutex<Option<Usage>>>,
    started: Arc<AtomicBool>,
    stop: Arc<(Mutex<bool>, Condvar)>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            latest: Arc::new(Mutex::new(None)),
            started: Arc::new(AtomicBool::new(false)),
            stop: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }
}

impl Sampler {
    /// 起后台线程（严格幂等：运行中再调用不会起第二条线程；`stop()` 后可再次启动）。
    pub fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let (lock, _) = &*self.stop;
            if let Ok(mut guard) = lock.lock() {
                *guard = false;
            }
        }
        let latest = Arc::clone(&self.latest);
        let stop = Arc::clone(&self.stop);
        let started = Arc::clone(&self.started);
        let spawned = std::thread::Builder::new()
            .name("gpu-memory".to_string())
            .spawn(move || {
                let Some(backend) = detect_backend() else {
                    tracing::debug!("未检测到 NVIDIA/AMD 显存探针，GPU 显存采样线程退出");
                    started.store(false, Ordering::Release);
                    return;
                };
                loop {
                    let usage = match &backend {
                        Backend::Nvidia => query_nvidia(),
                        Backend::AmdSysfs(dir) => query_amd_sysfs(dir),
                    };
                    if let Ok(mut slot) = latest.lock() {
                        *slot = usage;
                    }
                    let (lock, cvar) = &*stop;
                    let Ok(guard) = lock.lock() else {
                        break;
                    };
                    if *guard {
                        break;
                    }
                    let (guard, _) = match cvar.wait_timeout(guard, SAMPLE_INTERVAL) {
                        Ok(res) => res,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if *guard {
                        break;
                    }
                }
                started.store(false, Ordering::Release);
            });
        if let Err(err) = spawned {
            self.started.store(false, Ordering::Release);
            tracing::warn!(error = %err, "显存采样线程启动失败：GPU 压力让路不可用");
        }
    }

    /// 通知采样线程停止（Controller 销毁或配置关闭时调用）。
    pub fn stop(&self) {
        let (lock, cvar) = &*self.stop;
        if let Ok(mut guard) = lock.lock() {
            *guard = true;
            cvar.notify_all();
        }
        self.started.store(false, Ordering::Release);
    }

    /// 最近一次读数。
    #[must_use]
    pub fn latest(&self) -> Option<Usage> {
        self.latest.lock().ok().and_then(|slot| *slot)
    }
}

/// 检查是否所有已枚举的 NVIDIA PCI 设备均处于 runtime `suspended`（D3cold）。
/// 若是，调用 `nvidia-smi` 会把独显从零功耗 D3cold 唤醒，必须跳过轮询。
fn nvidia_all_suspended_in(pci_devices_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(pci_devices_dir) else {
        return false;
    };
    let mut saw_nvidia = false;
    for entry in entries.flatten() {
        let dev = entry.path();
        let Ok(vendor) = std::fs::read_to_string(dev.join("vendor")) else {
            continue;
        };
        if !vendor.trim().eq_ignore_ascii_case("0x10de") {
            continue;
        }
        saw_nvidia = true;
        let Ok(status) = std::fs::read_to_string(dev.join("power/runtime_status")) else {
            return false;
        };
        if status.trim() != "suspended" {
            return false;
        }
    }
    saw_nvidia
}

/// 跑一次 `nvidia-smi` 取显存；D3cold 休眠中或失败返回 `None`。
fn query_nvidia() -> Option<Usage> {
    if nvidia_all_suspended_in(Path::new("/sys/bus/pci/devices")) {
        return None;
    }
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().and_then(parse_usage)
}

#[cfg(test)]
fn query_once() -> Option<Usage> {
    query_nvidia()
}

/// 给 tick 用的一体化结构：采样器 + 状态机（阈值关闭时 `guard == None`）。
#[derive(Debug)]
pub struct Controller {
    sampler: Sampler,
    guard: Option<Guard>,
    started_at: Option<Instant>,
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.sampler.stop();
    }
}

impl Controller {
    /// 按配置构造。
    #[must_use]
    pub fn new(above: f32, below: f32) -> Self {
        Self {
            sampler: Sampler::default(),
            guard: Guard::new(above, below),
            started_at: None,
        }
    }

    /// 当前是否处于显存让路暂停状态。
    #[must_use]
    pub fn is_engaged(&self) -> bool {
        self.guard.as_ref().is_some_and(|g| g.engaged)
    }

    /// 当前生效的 `(暂停阈值%, 恢复阈值%)`（关闭时为 `None`）。
    #[cfg(test)]
    #[must_use]
    pub fn thresholds(&self) -> Option<(u8, u8)> {
        self.guard
            .as_ref()
            .map(|g| (g.above.round() as u8, g.below.round() as u8))
    }

    /// 运行期重载阈值（`reload` 路径）：不重复起采样线程；若禁用且之前正在让路，返回 `Resume`。
    pub fn reconfigure(&mut self, above: f32, below: f32) -> Action {
        let was_engaged = self.is_engaged();
        let Some(mut next_guard) = Guard::new(above, below) else {
            self.guard = None;
            self.started_at = None;
            self.sampler.stop();
            return if was_engaged {
                Action::Resume
            } else {
                Action::Unchanged
            };
        };
        next_guard.engaged = was_engaged;
        let action = next_guard.decide(self.sampler.latest());
        self.guard = Some(next_guard);
        action
    }

    /// 最近读数（供日志/`get_metrics` 展示）。
    #[must_use]
    pub fn usage(&self) -> Option<Usage> {
        self.sampler.latest()
    }

    /// 喂一次（首次调用时起采样线程）。返回动作。
    pub fn poll(&mut self, now: Instant) -> Action {
        let Some(guard) = self.guard.as_mut() else {
            return Action::Unchanged;
        };
        if self.started_at.is_none() {
            self.started_at = Some(now);
            self.sampler.start();
            return Action::Unchanged;
        }
        guard.decide(self.sampler.latest())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(used: u64, total: u64) -> Option<Usage> {
        Some(Usage {
            used_mib: used,
            total_mib: total,
        })
    }

    #[test]
    fn parses_nvidia_smi_line() {
        let u = parse_usage("7692, 8188").expect("应能解析");
        assert_eq!(u.used_mib, 7692);
        assert_eq!(u.total_mib, 8188);
        assert!((u.percent() - 93.94).abs() < 0.1);
        assert!(parse_usage("n/a, n/a").is_none());
        assert!(parse_usage("0, 0").is_none());
    }

    #[test]
    fn hysteresis_prevents_flapping() {
        let mut g = Guard::new(90.0, 80.0).expect("合法阈值");
        assert_eq!(g.decide(usage(5000, 8188)), Action::Unchanged); // 61%
        assert_eq!(g.decide(usage(7500, 8188)), Action::Pause); // 91%
        assert_eq!(g.decide(usage(7200, 8188)), Action::Unchanged); // 88% 仍在带内
        assert_eq!(g.decide(usage(6500, 8188)), Action::Resume); // 79%
        assert_eq!(g.decide(usage(6600, 8188)), Action::Unchanged); // 80.5% 不复发
    }

    #[test]
    fn disabled_thresholds_never_fire() {
        assert!(Guard::new(0.0, 80.0).is_none());
        assert!(Guard::new(80.0, 90.0).is_none());
        let mut c = Controller::new(0.0, 0.0);
        assert_eq!(c.poll(Instant::now()), Action::Unchanged);
        assert_eq!(c.poll(Instant::now()), Action::Unchanged);
    }

    /// 真机口径：`nvidia-smi` 在的机器上必须能读出非零总量。
    /// 没有 `nvidia-smi`（CI/非 NVIDIA）时静默跳过。
    #[test]
    fn default_thresholds_are_85_and_75() {
        let performance = liuguang_core::config::PerformanceConfig::default();
        assert_eq!(performance.pause_when_gpu_memory_above, 85);
        assert_eq!(performance.resume_when_gpu_memory_below, 75);
    }

    #[test]
    fn real_nvidia_smi_is_readable_when_present() {
        if std::process::Command::new("nvidia-smi")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let usage = query_once().expect("nvidia-smi 存在时必须解析出显存读数");
        assert!(usage.total_mib > 0, "总显存必须非零: {usage:?}");
        assert!(
            usage.used_mib <= usage.total_mib,
            "已用不得超过总量: {usage:?}"
        );
    }

    #[test]
    fn missing_reading_keeps_state() {
        let mut g = Guard::new(90.0, 80.0).expect("合法阈值");
        assert_eq!(g.decide(None), Action::Unchanged);
        assert_eq!(
            g.decide(Some(Usage {
                used_mib: 100,
                total_mib: 100
            })),
            Action::Pause
        );
    }

    #[test]
    fn parses_amd_sysfs_vram_bytes() {
        let dir = std::env::temp_dir().join(format!("liuguang-gpu-d3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d1 = dir.join("0000:01:00.0");
        std::fs::create_dir_all(d1.join("power")).unwrap();
        std::fs::write(d1.join("vendor"), "0x10de\n").unwrap();
        std::fs::write(d1.join("power/runtime_status"), "suspended\n").unwrap();
        assert!(nvidia_all_suspended_in(&dir));
        std::fs::write(d1.join("power/runtime_status"), "active\n").unwrap();
        assert!(!nvidia_all_suspended_in(&dir));
        let _ = std::fs::remove_dir_all(&dir);

        let sampler = Sampler::default();
        sampler.stop();
        let ctrl = Controller::new(85.0, 75.0);
        let mut ctrl = ctrl;
        assert_eq!(ctrl.reconfigure(0.0, 0.0), Action::Unchanged);
        assert_eq!(ctrl.reconfigure(85.0, 75.0), Action::Unchanged);
        assert_eq!(ctrl.thresholds(), Some((85, 75)));
        let u = parse_sysfs_vram("4294967296\n", "8589934592\n").expect("应能解析字节数");
        assert_eq!(u.used_mib, 4096);
        assert_eq!(u.total_mib, 8192);
        assert!((u.percent() - 50.0).abs() < 0.1);
        assert!(parse_sysfs_vram("100", "0").is_none());
        assert!(parse_sysfs_vram("bad", "1024").is_none());
    }

    #[test]
    fn reconfigure_disabling_resumes_when_engaged() {
        let mut c = Controller::new(85.0, 75.0);
        assert_eq!(c.thresholds(), Some((85, 75)));
        if let Some(g) = c.guard.as_mut() {
            assert_eq!(g.decide(usage(7800, 8188)), Action::Pause);
        }
        assert!(c.is_engaged());
        // 关闭显存让路（above=0）⇒ 必须触发 Resume，释放被它暂停的通道。
        assert_eq!(c.reconfigure(0.0, 75.0), Action::Resume);
        assert!(!c.is_engaged());
        assert_eq!(c.thresholds(), None);
        // 再次关闭保持 Unchanged。
        assert_eq!(c.reconfigure(0.0, 0.0), Action::Unchanged);
    }

    #[test]
    fn reconfigure_keeps_engaged_and_updates_thresholds() {
        let mut c = Controller::new(85.0, 75.0);
        if let Ok(mut slot) = c.sampler.latest.lock() {
            *slot = usage(7000, 8188); // ~85.5%
        }
        if let Some(g) = c.guard.as_mut() {
            assert_eq!(g.decide(usage(7000, 8188)), Action::Pause);
        }
        assert!(c.is_engaged());
        // 调整为 95/90：当前 85.5% <= 90% ⇒ 立刻 Resume。
        assert_eq!(c.reconfigure(95.0, 90.0), Action::Resume);
        assert!(!c.is_engaged());
        assert_eq!(c.thresholds(), Some((95, 90)));
    }
}
