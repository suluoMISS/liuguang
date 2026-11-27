//! 笔记本电源状态监测（拔电切换至电池供电时自动暂停动态壁纸）。
//!
//! 扫描 `/sys/class/power_supply/*` 下的电源节点：
//! * 台式机 / 无电池设备（不存在 `type = Battery` 节点）永远判定为 [`PowerState::Ac`]；
//! * 存在外接电源（`Mains` / `USB*` 等非电池类型）且任一节点 `online == "1"` → [`PowerState::Ac`]；
//! * 存在电池节点、外接电源均不在线且电池处于 `Discharging`（或所有电池均非充电且无在线外接电源）
//!   → [`PowerState::Battery`]。

use std::path::Path;
use std::time::{Duration, Instant};

const SYS_POWER_SUPPLY: &str = "/sys/class/power_supply";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// 当前主机供电状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// 已连接电源适配器，或主机无电池（台式机）。
    Ac,
    /// 使用电池供电。
    Battery,
}

/// 轮询判定出的动作建议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 供电状态未变化，无需操作。
    Unchanged,
    /// 切换至电池供电：应当暂停动态壁纸供给。
    Pause,
    /// 恢复外接电源（或功能已关闭）：恢复因省电暂停的通道。
    Resume,
}

/// `/sys/class/power_supply/<node>` 单个节点的精简快照（便于纯逻辑单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerSupplyEntry {
    /// `type` 文件内容（如 `"Mains"`, `"USB"`, `"Battery"`）。
    pub kind: String,
    /// `online` 文件内容（外接电源节点一般为 `"0"` 或 `"1"`）。
    pub online: Option<String>,
    /// `status` 文件内容（电池节点一般为 `"Charging"`, `"Discharging"`, `"Full"`, `"Not charging"`）。
    pub status: Option<String>,
}

/// 根据供电节点列表判定当前是外接电源还是电池供电。
#[must_use]
pub fn classify_power_entries(entries: &[PowerSupplyEntry]) -> PowerState {
    let mut has_battery = false;
    let mut any_external_online = false;
    let mut any_discharging = false;

    for entry in entries {
        let kind = entry.kind.trim();
        if kind.eq_ignore_ascii_case("Battery") {
            has_battery = true;
            if entry
                .status
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| s.eq_ignore_ascii_case("Discharging"))
            {
                any_discharging = true;
            }
        } else if !kind.is_empty()
            && entry
                .online
                .as_deref()
                .map(str::trim)
                .is_some_and(|v| v == "1")
        {
            any_external_online = true;
        }
    }

    if !has_battery || any_external_online {
        PowerState::Ac
    } else if any_discharging {
        PowerState::Battery
    } else {
        PowerState::Ac
    }
}

/// 读取指定电源目录（默认 `/sys/class/power_supply`）的供电状态。
#[must_use]
pub fn decide_power_state_in(dir: &Path) -> PowerState {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return PowerState::Ac;
    };
    let mut entries = Vec::new();
    for item in read_dir.flatten() {
        let path = item.path();
        let Ok(kind) = std::fs::read_to_string(path.join("type")) else {
            continue;
        };
        let online = std::fs::read_to_string(path.join("online")).ok();
        let status = std::fs::read_to_string(path.join("status")).ok();
        entries.push(PowerSupplyEntry {
            kind,
            online,
            status,
        });
    }
    classify_power_entries(&entries)
}

/// 读取宿主机 `/sys/class/power_supply` 并返回供电状态。
#[must_use]
pub fn decide_power_state() -> PowerState {
    decide_power_state_in(Path::new(SYS_POWER_SUPPLY))
}

/// 带节流与状态跳变跟踪的电源监视器。
#[derive(Debug)]
pub struct PowerWatch {
    enabled: bool,
    state: PowerState,
    last_poll: Option<Instant>,
    interval: Duration,
}

impl PowerWatch {
    /// 创建电源监视器；启用时首次 `poll` 会立即采样一次。
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: PowerState::Ac,
            last_poll: None,
            interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// 热重载开关；若从启用且处于电池状态关闭，立即建议恢复视频。
    pub fn reconfigure(&mut self, enabled: bool) -> Action {
        let was_battery = self.enabled && self.state == PowerState::Battery;
        self.enabled = enabled;
        self.last_poll = None;
        if !enabled {
            self.state = PowerState::Ac;
            if was_battery {
                return Action::Resume;
            }
        }
        Action::Unchanged
    }

    /// 到达采样间隔后检查供电状态；仅在状态发生变化时返回 `Pause` / `Resume`。
    pub fn poll(&mut self, now: Instant) -> Action {
        if !self.enabled {
            return Action::Unchanged;
        }
        if let Some(prev) = self.last_poll {
            if now.saturating_duration_since(prev) < self.interval {
                return Action::Unchanged;
            }
        }
        self.last_poll = Some(now);
        self.apply_sample(decide_power_state())
    }

    /// 用外部采样（如单测注入）推进状态机。
    pub fn apply_sample(&mut self, sampled: PowerState) -> Action {
        if !self.enabled || sampled == self.state {
            return Action::Unchanged;
        }
        self.state = sampled;
        match sampled {
            PowerState::Battery => Action::Pause,
            PowerState::Ac => Action::Resume,
        }
    }

    /// 当前是否开启且处于电池供电状态。
    #[must_use]
    pub fn is_on_battery(&self) -> bool {
        self.enabled && self.state == PowerState::Battery
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, online: Option<&str>, status: Option<&str>) -> PowerSupplyEntry {
        PowerSupplyEntry {
            kind: kind.to_string(),
            online: online.map(str::to_string),
            status: status.map(str::to_string),
        }
    }

    #[test]
    fn desktop_without_battery_always_reports_ac() {
        assert_eq!(classify_power_entries(&[]), PowerState::Ac);
        assert_eq!(
            classify_power_entries(&[entry("Mains", Some("0"), None)]),
            PowerState::Ac
        );
    }

    #[test]
    fn laptop_plugged_in_via_mains_or_usb_reports_ac() {
        let entries = [
            entry("Mains\n", Some("1\n"), None),
            entry("Battery\n", None, Some("Not charging\n")),
        ];
        assert_eq!(classify_power_entries(&entries), PowerState::Ac);

        let usb_pd = [
            entry("Mains", Some("0"), None),
            entry("USB", Some("1"), None),
            entry("Battery", None, Some("Charging")),
        ];
        assert_eq!(classify_power_entries(&usb_pd), PowerState::Ac);
    }

    #[test]
    fn laptop_unplugged_discharging_reports_battery() {
        let entries = [
            entry("Mains", Some("0"), None),
            entry("USB", Some("0"), None),
            entry("Battery", None, Some("Discharging\n")),
        ];
        assert_eq!(classify_power_entries(&entries), PowerState::Battery);
    }

    #[test]
    fn watch_transitions_and_reconfigure_resume() {
        let mut watch = PowerWatch::new(true);
        assert_eq!(watch.apply_sample(PowerState::Battery), Action::Pause);
        assert!(watch.is_on_battery());
        assert_eq!(watch.apply_sample(PowerState::Battery), Action::Unchanged);
        assert_eq!(watch.reconfigure(false), Action::Resume);
        assert!(!watch.is_on_battery());
    }
}
