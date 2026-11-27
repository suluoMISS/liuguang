//! 熄屏暂停（`performance.pause_when_display_off`，计划 §11.2）。
//!
//! **为什么用 sysfs 而不是 DRM ioctl**：真机验证（2026-10-20，i915 / DP-1）确认
//! `/sys/class/drm/card1-DP-1/dpms` 与 `.../status` 都存在且可读，取值就是内核
//! 侧的权威状态；读文件不需要拿 DRM master，也不会与扫描输出争用。用 ioctl 反而
//! 要把 `drm` 的 property 枚举搬进显示层，还得处理"属性名因驱动而异"。
//!
//! **口径（如实说明）**：
//! - **熄屏** = 已连接连接器的 `dpms` 为 `Off`，**或**没有任何连接器处于 `connected`。
//!   后者覆盖"拔线/显示器断电到 EDID 丢失"的常见情形。
//! - **不把"未连接的其他连接器"算作熄屏**：DP-2/DP-3/HDMI-* 在真机上恒为
//!   `disconnected` + `dpms=Off`，若把它们算进来，机器**永远**处于熄屏。
//! - **读不到任何信号 ⇒ 判为 `On`（不暂停）**。宁可满速显示，也不在没有事实时
//!   把画面停掉——这与 `loadshed` 里"压力读不到就不降载"是同一条原则。
//!
//! **已知局限（必须说清）**：本模块只**观测**、不**设置**。真机上
//! `consoleblank=0` 且无常驻合成器 ⇒ 没有进程会主动把 DPMS 置为 `Off`，
//! 因此"用户手动关掉显示器电源"这一动作**不会**改变 `dpms`，本功能对它无效；
//! 它对"被别的组件熄屏""输出被断开""内核 blanking 生效"三种情形有效。
//! 要覆盖"关显示器电源"需要读 DPCD 的 sink 状态，i915 对此无稳定用户态接口，
//! 属于未验证范围，不在此声称支持。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 采样间隔。读两个小文件，2 秒一次的开销可以忽略；再密没有意义
/// （DPMS 是给人看的开关，不是逐帧状态）。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// `/sys/class/drm` 下连接器目录的前缀（`card1-DP-1` 之类）。
const DRM_CLASS_DIR: &str = "/sys/class/drm";

/// 屏幕（显示输出）是否处于点亮状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// 至少一个连接器 `connected` 且其 `dpms` 不是 `Off`（或读不到取值）。
    On,
    /// 所有连接器都未连接，或已连接连接器的 `dpms` 为 `Off`。
    Off,
}

/// 单个连接器的一次观测。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorReading {
    /// `.../status` 的内容（`connected` / `disconnected` / `unknown`）。
    pub status: Option<String>,
    /// `.../dpms` 的内容（`On` / `Off` / `Standby` / `Suspend`）。
    pub dpms: Option<String>,
}

/// 由一组连接器观测推出屏幕状态（**纯函数**，单测直接覆盖）。
///
/// 规则见模块文档"口径"：只看 `connected` 的连接器；没有任何可用信号时判 `On`。
#[must_use]
pub fn decide(readings: &[ConnectorReading]) -> ScreenState {
    let connected: Vec<&ConnectorReading> = readings
        .iter()
        .filter(|r| r.status.as_deref() == Some("connected"))
        .collect();
    if connected.is_empty() {
        // 一个都没连 ⇒ 没有可看的画面 ⇒ 熄屏。
        // 但若所有 status 都读不到（`None`），这是"不知道"，不是"没接"：
        // 不能因为读失败就把壁纸停掉。
        let any_status_known = readings.iter().any(|r| r.status.is_some());
        return if any_status_known {
            ScreenState::Off
        } else {
            ScreenState::On
        };
    }
    // 任一已连接连接器明确 Off/Standby/Suspend ⇒ 熄屏；读不到 ⇒ 视为 On。
    let all_off = connected
        .iter()
        .all(|r| matches!(r.dpms.as_deref(), Some("Off" | "Standby" | "Suspend")));
    if all_off {
        ScreenState::Off
    } else {
        ScreenState::On
    }
}

/// 熄屏监视器：节流采样 sysfs，只在**状态变化**时上报。
#[derive(Debug)]
pub struct DisplayPower {
    enabled: bool,
    /// 连接器目录（`/sys/class/drm/card1-DP-1`）。启动时枚举一次即可：
    /// 连接器不会在运行中增删（热插只改 `status`，不改目录集合）。
    connectors: Vec<PathBuf>,
    last_state: Option<ScreenState>,
    last_poll: Option<Instant>,
    polls: u64,
    transitions: u64,
}

impl DisplayPower {
    /// 按配置构造。`enabled == false` 时 [`Self::refresh`] 恒返回 `None`（零开销）。
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        // 单测里不读宿主机真实 sysfs（`cfg!(test)`）：CI / 远程会话里屏幕可能正好
        // 处于 DPMS Off，视频通道会被"熄屏暂停"停掉，`presented_ok` 恒 0，测试
        // 结果会随机器当前的屏幕状态漂移。空连接器 ⇒ `decide(&[])` ⇒ `On`
        // （"读不到任何信号就不暂停"，与模块口径一致）；真实枚举逻辑由
        // `enumerate_connectors` / `decide` 的 temp-dir 单测覆盖。
        let connectors = if enabled && !cfg!(test) {
            enumerate_connectors(Path::new(DRM_CLASS_DIR))
        } else {
            Vec::new()
        };
        Self {
            enabled,
            connectors,
            last_state: None,
            last_poll: None,
            polls: 0,
            transitions: 0,
        }
    }

    /// 用调用方给定的连接器目录构造（**测试专用**；不碰真实 sysfs）。
    #[cfg(test)]
    #[must_use]
    pub fn with_connectors(enabled: bool, connectors: Vec<PathBuf>) -> Self {
        Self {
            enabled,
            connectors,
            last_state: None,
            last_poll: None,
            polls: 0,
            transitions: 0,
        }
    }

    /// 运行期开关同步（`reload` 路径）：从关闭切到启用时补扫连接器。
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.last_state = None;
        self.last_poll = None;
        if enabled && !cfg!(test) && self.connectors.is_empty() {
            self.connectors = enumerate_connectors(Path::new(DRM_CLASS_DIR));
        }
    }

    /// 当前是否已被确认为熄屏（仅启用且最近采样为 `Off` 时为 `true`）。
    #[must_use]
    pub const fn is_off(&self) -> bool {
        self.enabled && matches!(self.last_state, Some(ScreenState::Off))
    }

    /// 累计采样次数与状态变化次数（可观测：证明本机制真的在跑）。
    #[must_use]
    pub const fn counters(&self) -> (u64, u64) {
        (self.polls, self.transitions)
    }

    /// 节流采样。**只在状态变化时**返回 `Some(新状态)`，否则 `None`。
    pub fn refresh(&mut self, now: Instant) -> Option<ScreenState> {
        if !self.enabled {
            return None;
        }
        if let Some(last) = self.last_poll
            && now.saturating_duration_since(last) < SAMPLE_INTERVAL
        {
            return None;
        }
        self.last_poll = Some(now);
        self.polls = self.polls.saturating_add(1);
        let readings: Vec<ConnectorReading> =
            self.connectors.iter().map(|p| read_connector(p)).collect();
        let state = decide(&readings);
        if self.last_state == Some(state) {
            return None;
        }
        self.last_state = Some(state);
        self.transitions = self.transitions.saturating_add(1);
        Some(state)
    }
}

/// 读一个连接器目录下的 `status` / `dpms`。读失败记 `None`（不是""）。
fn read_connector(dir: &Path) -> ConnectorReading {
    let read = |name: &str| -> Option<String> {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    ConnectorReading {
        status: read("status"),
        dpms: read("dpms"),
    }
}

/// 枚举 `/sys/class/drm` 下形如 `card<N>-<TYPE>-<N>` 的连接器目录。
///
/// **不含** `card0`/`renderD*` 这类非连接器条目，也不含 `card0-DP-1` 的聚合项。
fn enumerate_connectors(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            // 形如 card1-DP-1：在第一个 '-' 之后还有一个 '-'。
            let Some((card, rest)) = name.split_once('-') else {
                return false;
            };
            card.starts_with("card") && rest.contains('-') && p.join("status").exists()
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn reading(status: Option<&str>, dpms: Option<&str>) -> ConnectorReading {
        ConnectorReading {
            status: status.map(str::to_owned),
            dpms: dpms.map(str::to_owned),
        }
    }

    /// 真机实测取值（2026-10-20，DP-1 接通）：On + connected ⇒ 屏幕亮着。
    #[test]
    fn connected_and_dpms_on_is_awake() {
        assert_eq!(
            decide(&[reading(Some("connected"), Some("On"))]),
            ScreenState::On
        );
    }

    /// 已连接但 DPMS 被置 Off ⇒ 熄屏。
    #[test]
    fn connected_but_dpms_off_is_asleep() {
        for v in ["Off", "Standby", "Suspend"] {
            assert_eq!(
                decide(&[reading(Some("connected"), Some(v))]),
                ScreenState::Off,
                "dpms={v} 应判熄屏"
            );
        }
    }

    /// 真机其余连接器恒为 disconnected + dpms=Off：**不得**把它们算成熄屏，
    /// 否则机器永远暂停（这是最容易被写错的地方）。
    #[test]
    fn disconnected_siblings_do_not_force_sleep() {
        let readings = [
            reading(Some("connected"), Some("On")),
            reading(Some("disconnected"), Some("Off")),
            reading(Some("disconnected"), Some("Off")),
            reading(Some("disconnected"), Some("Off")),
        ];
        assert_eq!(decide(&readings), ScreenState::On);
    }

    /// 全部未连接（拔线/显示器断电到 EDID 丢失）⇒ 熄屏。
    #[test]
    fn no_connected_output_is_asleep() {
        let readings = [
            reading(Some("disconnected"), Some("Off")),
            reading(Some("disconnected"), Some("Off")),
        ];
        assert_eq!(decide(&readings), ScreenState::Off);
    }

    /// **读不到任何信号 ⇒ 判 On（不暂停）**：没有事实时不能把画面停掉。
    #[test]
    fn unreadable_signals_keep_playing() {
        assert_eq!(decide(&[]), ScreenState::On);
        assert_eq!(decide(&[reading(None, None)]), ScreenState::On);
        // status 读到了、dpms 读不到 ⇒ 视为 On（不因缺一项而误判熄屏）
        assert_eq!(decide(&[reading(Some("connected"), None)]), ScreenState::On);
    }

    #[test]
    fn disabled_watcher_never_reports() {
        let mut w = DisplayPower::with_connectors(false, Vec::new());
        assert!(w.refresh(Instant::now()).is_none());
        assert_eq!(w.counters(), (0, 0));
    }

    /// 状态变化只报一次；同一状态重复采样不重复上报。
    #[test]
    fn reports_only_on_transition() {
        let dir = std::env::temp_dir().join(format!("liuguang-dp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let conn = dir.join("card9-DP-9");
        fs::create_dir_all(&conn).expect("mkdir");
        fs::write(conn.join("status"), "connected\n").expect("status");
        fs::write(conn.join("dpms"), "On\n").expect("dpms");

        let mut w = DisplayPower::with_connectors(true, vec![conn.clone()]);
        let t0 = Instant::now();
        assert_eq!(w.refresh(t0), Some(ScreenState::On), "首次采样必须上报");

        // 未到采样间隔 ⇒ 不采（返回 None）
        assert!(w.refresh(t0 + Duration::from_millis(500)).is_none());

        // 同一状态 ⇒ 不重复上报
        assert!(w.refresh(t0 + SAMPLE_INTERVAL).is_none());

        // 变成 Off ⇒ 上报一次
        fs::write(conn.join("dpms"), "Off\n").expect("dpms2");
        assert_eq!(
            w.refresh(t0 + SAMPLE_INTERVAL * 2),
            Some(ScreenState::Off),
            "熄屏必须上报"
        );
        // 再次采样仍 Off ⇒ 不重复
        assert!(w.refresh(t0 + SAMPLE_INTERVAL * 3).is_none());

        // 恢复
        fs::write(conn.join("dpms"), "On\n").expect("dpms3");
        assert_eq!(
            w.refresh(t0 + SAMPLE_INTERVAL * 4),
            Some(ScreenState::On),
            "恢复必须上报"
        );
        assert_eq!(w.counters().1, 3, "三次变化：On→Off→On");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 枚举只挑连接器目录（真 git 环境没有 sysfs 时返回空，不 panic）。
    #[test]
    fn enumerate_is_total() {
        let empty = std::env::temp_dir().join(format!("liuguang-dp-empty-{}", std::process::id()));
        let _ = fs::create_dir_all(&empty);
        assert!(enumerate_connectors(&empty).is_empty());
        let _ = fs::remove_dir_all(&empty);
    }
}
