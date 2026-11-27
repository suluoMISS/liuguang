//! 系统 CPU 压力探测与降载决策（`performance.reduce_load_on_cpu_pressure`，计划 §11.2）。
//!
//! **口径（如实说明）**：
//!
//! - 首选 PSI：`/proc/pressure/cpu` 的 `some avg10` = 最近约 10 秒内"至少有
//!   一个任务因等 CPU 而处于不可运行状态"的时间占比（PSI 的 `avg10/avg60/avg300`
//!   是 10s/60s/300s 三个指数移动平均窗口，不是分钟数）。这正是"服务器其他服务
//!   被挤占"的直接信号，也是 §11.2「CPU 或 I/O 压力升高」想要的那一个。
//! - 回退 `/proc/loadavg` 的 1 分钟值 ÷ 在线核数（×100）：内核未编译 PSI
//!   （`CONFIG_PSI_DEFAULT_DISABLED=n` 且启动未开 `psi=1`）时 PSI 文件不存在。
//!   这条口径**更钝**（loadavg 含不可中断 IO 任务，且不区分"是否真的在等 CPU"），
//!   因此读数会带上来源标签，日志与 `get_metrics` 里能看出用的是哪条。
//! - 两者都读不到 ⇒ [`PressureSource::Unavailable`]，判据退回 [`LoadLevel::Normal`]
//!   （**不降载**：宁可显示满速，也不要在没有事实时静默把画质压下去）。
//!
//! **阈值是策略选择，不是实测结论**：滞回区间见各常量注释，且**按来源分开**
//! （`thresholds()`）：PSI 用 `[RELIEF_AT, SHED_AT]` = 5%..20%，loadavg 用
//! `[RELIEF_AT_LOADAVG, SHED_AT_LOADAVG]` = 70%..80%——两条口径的"百分比"不是
//! 同一量纲，混用会把正常桌面判成有压力。真机上"降载到底省下多少 CPU"尚未测量
//! （`docs/PROGRESS.md` 已标注为未验证项）。
//!
//! PSI 的行格式是 `some avg10=<val> avg60=<val> avg300=<val> total=<ns>`
//! （**等号**分隔，不是空格分隔）；解析只认 `some` 行，见 `parse_psi_some_avg10`。

use std::time::{Duration, Instant};

/// 读数来源（可观测：不同口径的钝感程度不同，必须在日志里可区分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureSource {
    /// `/proc/pressure/cpu` 的 `some avg10`。
    Psi,
    /// `/proc/loadavg` 1 分钟值 ÷ 在线核数。
    LoadAvg,
    /// 两条都读不到。
    Unavailable,
}

/// 一次归一化到 `0.0..=100.0` 的 CPU 压力读数。
#[derive(Debug, Clone, Copy)]
pub struct CpuPressure {
    /// 压力百分比（口径见 [`PressureSource`] 各变体注释）。
    pub percent: f32,
    /// 该读数出自哪条口径。
    pub source: PressureSource,
}

/// 进入降载档的阈值（PSI `some avg10` ≥ 20%）。
///
/// 取 20 而不是 50：本守护进程的定位是**服务器共存**（计划 §11「服务器服务
/// 优先」），等 CPU 争用明显到一半以上才让路，等于让其他服务先难受一个
/// `avg10` 窗口（约 10 秒）。
pub const SHED_AT: f32 = 20.0;

/// 退出降载档的阈值（PSI 口径的滞回下沿）。
///
/// 不取 `SHED_AT` 本身：压力在阈值附近抖动时，同阈值会让提交频率每 5s 抖一次
/// （画面忽快忽慢）。恢复比降载慢得多是刻意的——让路要让到底，回来要稳。
pub const RELIEF_AT: f32 = 5.0;

/// 回退到 `/proc/loadavg` 时的降载阈值（1 分钟均载 ÷ 在线核数 ≥ 80%）。
///
/// **与 PSI 的 [`SHED_AT`] 不是同一量纲**：`some avg10` 是"有任务在等 CPU 的时间
/// 占比"，而 loadavg 的百分比只是"平均可运行线程数 ÷ 核数"——32 核机器 loadavg
/// 6.4 就有 20%，那只是正常桌面（编译、浏览器、容器都在跑），并不代表有人在
/// 等 CPU。所以这条更钝的口径必须用高得多的阈值：几乎每个核都有可运行线程
/// （>80%）才让路。
pub const SHED_AT_LOADAVG: f32 = 80.0;

/// `loadavg` 口径的恢复阈值（滞回下沿）。
///
/// 与 [`RELIEF_AT`] 同样是策略选择：80% 进、70% 出，留下 10 个百分点的抖动带。
pub const RELIEF_AT_LOADAVG: f32 = 70.0;

/// 一个读数口径的滞回阈值对。
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// ≥ 该值进入 [`LoadLevel::Shed`]。
    pub shed_at: f32,
    /// ≤ 该值退出 [`LoadLevel::Shed`]。
    pub relief_at: f32,
}

impl Thresholds {
    /// PSI 口径（`some avg10`，见 [`SHED_AT`]）。
    pub const PSI: Self = Self {
        shed_at: SHED_AT,
        relief_at: RELIEF_AT,
    };

    /// loadavg 口径（见 [`SHED_AT_LOADAVG`]）。
    pub const LOADAVG: Self = Self {
        shed_at: SHED_AT_LOADAVG,
        relief_at: RELIEF_AT_LOADAVG,
    };
}

/// 按读数来源选阈值；`Unavailable` 没有阈值（不切档）。
///
/// 把这件事收成一处，是为了让"哪条口径用哪个阈值"只有一份答案：如果 `apply()`
/// 里直接写 `reading.percent >= SHED_AT`，回退到 loadavg 时就又变成 20% 降载了
/// ——那正是 2026-11-22 把正常桌面压到 30fps 的原因（t35）。
pub fn thresholds(source: PressureSource) -> Option<Thresholds> {
    match source {
        PressureSource::Psi => Some(Thresholds::PSI),
        PressureSource::LoadAvg => Some(Thresholds::LOADAVG),
        PressureSource::Unavailable => None,
    }
}

/// 采样间隔：`avg10` 是约 10 秒的滑动窗口，5s 采一次已足够跟上它的变化速度。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// 降载档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadLevel {
    /// 正常：按配置提交、允许预加载。
    #[default]
    Normal,
    /// 降载：减半视频提交频率、暂停预加载（§11.2 第一行）。
    Shed,
}

/// 降载判定器：带滞回的档位状态机 + 采样节流。
#[derive(Debug)]
pub struct LoadShedder {
    /// `performance.reduce_load_on_cpu_pressure`。
    enabled: bool,
    level: LoadLevel,
    last_sample: Option<Instant>,
    /// 最近一次读数（`get_metrics`/日志用；`None` = 还没采过）。
    last_reading: Option<CpuPressure>,
}

impl LoadShedder {
    /// 创建判定器。`enabled = false` 时 [`Self::refresh`] 永远不采样、档位恒为
    /// [`LoadLevel::Normal`]（配置关掉就真的什么都不做）。
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            level: LoadLevel::default(),
            last_sample: None,
            last_reading: None,
        }
    }

    /// 当前档位。
    pub fn level(&self) -> LoadLevel {
        self.level
    }

    /// 最近一次读数（含来源）；未采样过返回 `None`。
    pub fn reading(&self) -> Option<CpuPressure> {
        self.last_reading
    }

    /// 是否配置了降载能力。
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 同步 `performance.reduce_load_on_cpu_pressure`（`reload_config` 路径）。
    ///
    /// 关掉时必须把档位复位成 [`LoadLevel::Normal`]：否则"压力期间 reload 关掉
    /// 降载"会让视频永远停在半速（配置说不要降载，运行期却还在降）。
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.level = LoadLevel::Normal;
            self.last_sample = None;
            self.last_reading = None;
        }
    }

    /// 现在是否该采样（首次立即采，之后按 [`SAMPLE_INTERVAL`] 节流）。
    pub fn sample_due(&self, now: Instant) -> bool {
        if !self.enabled {
            return false;
        }
        match self.last_sample {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= SAMPLE_INTERVAL,
        }
    }

    /// 采样并推进档位（读 `/proc`）。档位**变化**时返回 `(旧, 新)`。
    pub fn refresh(&mut self, now: Instant) -> Option<(LoadLevel, LoadLevel)> {
        if !self.sample_due(now) {
            return None;
        }
        self.last_sample = Some(now);
        let reading = read_cpu_pressure();
        self.last_reading = Some(reading);
        self.apply(reading)
    }

    /// 推进档位（纯函数部分，测试直接喂读数）。
    ///
    /// 读数不可用时**保持当前档位**：已经在降载就别因为"读不到"突然满速，
    /// 那会让压力实际更高时反而提速（不可观测的错误方向）。
    pub fn apply(&mut self, reading: CpuPressure) -> Option<(LoadLevel, LoadLevel)> {
        if !self.enabled {
            return None;
        }
        let Some(thresholds) = thresholds(reading.source) else {
            // 读数不可用：**保持当前档位**（见函数 doc）。
            return None;
        };
        let next = match self.level {
            LoadLevel::Normal if reading.percent >= thresholds.shed_at => LoadLevel::Shed,
            LoadLevel::Shed if reading.percent <= thresholds.relief_at => LoadLevel::Normal,
            other => other,
        };
        if next == self.level {
            return None;
        }
        let old = self.level;
        self.level = next;
        Some((old, next))
    }
}

/// 该档位下视频提交频率上限（`fps_limit` 的媒体层取值）。
///
/// `base` 是配置值（`media.video_fps_limit`，校验保证 1..=240）。降载时**减半**
/// 并保底 1fps：再低就没有"动态画面"可言了（壁纸机的降级方向是静态，
/// 不是 0.2fps 的幻灯片）。
pub fn fps_cap(base: u32, level: LoadLevel) -> f64 {
    let base = f64::from(base.max(1));
    match level {
        LoadLevel::Normal => base,
        LoadLevel::Shed => (base / 2.0).max(1.0),
    }
}

/// 一次 `/proc` 读取（PSI 优先，回退 loadavg）。
pub fn read_cpu_pressure() -> CpuPressure {
    let psi = std::fs::read_to_string("/proc/pressure/cpu").ok();
    let loadavg = std::fs::read_to_string("/proc/loadavg").ok();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as f32)
        .unwrap_or(1.0)
        .max(1.0);
    resolve_pressure(psi.as_deref(), loadavg.as_deref(), cores)
}

/// 把两条 `/proc` 文本（读不到就是 `None`）解析成带来源标签的读数。
///
/// 拆成纯函数是为了能测"PSI 不可读时回退 loadavg"与"两条都没有 ⇒ Unavailable"，
/// 而不必依赖宿主机是否编译了 PSI（CI 容器里经常没有）。
fn resolve_pressure(psi: Option<&str>, loadavg: Option<&str>, cores: f32) -> CpuPressure {
    if let Some(text) = psi
        && let Some(percent) = parse_psi_some_avg10(text)
    {
        return CpuPressure {
            percent,
            source: PressureSource::Psi,
        };
    }
    let cores = cores.max(1.0);
    if let Some(text) = loadavg
        && let Some(one_min) = parse_loadavg_1m(text)
    {
        return CpuPressure {
            percent: (one_min / cores * 100.0).clamp(0.0, 100.0),
            source: PressureSource::LoadAvg,
        };
    }
    CpuPressure {
        percent: 0.0,
        source: PressureSource::Unavailable,
    }
}

/// 解析 `/proc/pressure/cpu` 里 `some` 行的 `avg10`。
///
/// 真实内核格式是 **`key=value`**（Linux 5.13+，实测 6.x）：
///
/// ```text
/// some avg10=0.63 avg60=0.81 avg300=1.22 total=4080600820
/// full avg10=0.00 avg60=0.00 avg300=0.00 total=0
/// ```
///
/// 注意第二个字段是 `avg10=0.63` —— **不是** `avg10 0.63`。t35 之前这里按空格
/// 取字段（`fields.next() != Some("avg10")`），于是真实内核上永远返回 `None`，
/// 每 5s 采样都静默回退到 loadavg，把 32 核机器上 43% 的正常桌面负载判成
/// 「有压力」并降载到半速（30fps）。
///
/// 只认 `some` 行：`full` 行的 `avg10` 是所有任务都被 CPU 卡住的时间占比，
/// 口径完全不同（实际几乎恒为 0），混用会让降载永不触发。
fn parse_psi_some_avg10(text: &str) -> Option<f32> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("some"))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// 解析 `/proc/loadavg` 的 1 分钟均载（第一个字段）。
fn parse_loadavg_1m(text: &str) -> Option<f32> {
    text.split_whitespace()
        .next()?
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psi(percent: f32) -> CpuPressure {
        CpuPressure {
            percent,
            source: PressureSource::Psi,
        }
    }

    fn loadavg(percent: f32) -> CpuPressure {
        CpuPressure {
            percent,
            source: PressureSource::LoadAvg,
        }
    }

    /// 真实内核样本（本机 `/proc/pressure/cpu`，2026-11-22 采集）。
    const KERNEL_PSI: &str = "some avg10=4.35 avg60=1.85 avg300=1.50 total=4078764526\n\
                              full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";

    #[test]
    fn psi_kernel_format_is_parsed() {
        // 内核是 `avg10=4.35`（等号），不是 `avg10 4.35`（空格）。
        assert_eq!(parse_psi_some_avg10(KERNEL_PSI), Some(4.35));
        // `full` 行在前也不能误取（它恒为 0，混用会让降载永不触发）。
        let reordered = "full avg10=44.44 avg60=0.00 avg300=0.00 total=0\n\
                         some avg10=7.50 avg60=0.00 avg300=0.00 total=9\n";
        assert_eq!(parse_psi_some_avg10(reordered), Some(7.5));
        // 只有 `full` 行 ⇒ 读不出（不得拿 full 的口径充数）。
        assert_eq!(
            parse_psi_some_avg10("full avg10=44.44 avg60=0.00 avg300=0.00 total=0\n"),
            None
        );
        // 真实内核读数走完 `resolve_pressure` 后来源必须是 Psi。
        let reading = resolve_pressure(Some(KERNEL_PSI), Some("6.40 5.00 4.00 1/100 1"), 32.0);
        assert_eq!(reading.source, PressureSource::Psi);
        assert_eq!(reading.percent, 4.35);
    }

    #[test]
    fn psi_missing_avg10_falls_back_to_loadavg() {
        // PSI 存在但读不出 avg10（畸形/老内核）⇒ 回退 loadavg，且来源标对。
        let reading = resolve_pressure(
            Some("some avg60=1.00\n"),
            Some("6.40 5.00 4.00 1/100 1"),
            32.0,
        );
        assert_eq!(reading.source, PressureSource::LoadAvg);
        assert!((reading.percent - 20.0).abs() < 0.01, "6.4/32 = 20%");
        // PSI 文件根本不存在（CI/容器常见）⇒ 同样回退。
        let reading = resolve_pressure(None, Some("25.60 5.00 4.00 1/100 1"), 32.0);
        assert_eq!(reading.source, PressureSource::LoadAvg);
        assert!((reading.percent - 80.0).abs() < 0.01, "25.6/32 = 80%");
        // 两条都读不到 ⇒ Unavailable（不得假装 0% 压力）。
        let reading = resolve_pressure(None, None, 32.0);
        assert_eq!(reading.source, PressureSource::Unavailable);
        assert_eq!(reading.percent, 0.0);
    }

    #[test]
    fn loadavg_percent_still_computed() {
        // 回退口径的百分比 = 1 分钟均载 ÷ 核数（clamp 到 0..=100）。
        let reading = resolve_pressure(None, Some("0.71 0.66 0.62 2/1234 5678"), 1.0);
        assert_eq!(reading.source, PressureSource::LoadAvg);
        assert!((reading.percent - 71.0).abs() < 0.01);
        // 超载也不得超过 100（否则日志/指标里出现 >100% 的读数）。
        let reading = resolve_pressure(None, Some("128.00 0.00 0.00 1/1 1"), 16.0);
        assert_eq!(reading.percent, 100.0);
        // 核数为 0/未知时按 1 核算，不得除出 inf/NaN。
        let reading = resolve_pressure(None, Some("4.00 0.00 0.00 1/1 1"), 0.0);
        assert_eq!(reading.percent, 100.0);
    }

    #[test]
    fn loadavg_pressure_does_not_shed_at_normal_desktop_load() {
        // 32 核 loadavg 13.8 ⇒ 43%：正常桌面（编译/浏览器/容器），不是 CPU 压力。
        let mut shed = LoadShedder::new(true);
        assert_eq!(shed.apply(loadavg(43.0)), None);
        assert_eq!(shed.level(), LoadLevel::Normal);
        // 贴近阈值也不降（79.9% < 80%）。
        assert_eq!(shed.apply(loadavg(79.9)), None);
        // 达到 80% 才降。
        assert_eq!(
            shed.apply(loadavg(SHED_AT_LOADAVG)),
            Some((LoadLevel::Normal, LoadLevel::Shed))
        );
        // 70% 以上保持降载（滞回带内不抖）。
        assert_eq!(shed.apply(loadavg(75.0)), None);
        assert_eq!(shed.level(), LoadLevel::Shed);
        // 低于 70% 才恢复。
        assert_eq!(
            shed.apply(loadavg(RELIEF_AT_LOADAVG - 0.1)),
            Some((LoadLevel::Shed, LoadLevel::Normal))
        );
    }

    #[test]
    fn psi_pressure_sheds_at_threshold() {
        // PSI 21% 在阈值（20%）之上 ⇒ 降载。
        let mut shed = LoadShedder::new(true);
        assert_eq!(
            shed.apply(psi(21.0)),
            Some((LoadLevel::Normal, LoadLevel::Shed))
        );
        // PSI 5% ⇒ 不进入降载（同一读数在 loadavg 口径下也远低于 80%）。
        let mut quiet = LoadShedder::new(true);
        assert_eq!(quiet.apply(psi(5.0)), None);
        assert_eq!(quiet.level(), LoadLevel::Normal);
        // 同一份 43%：PSI 口径要降（>20%），loadavg 口径不降（<80%）。
        let mut psi_shed = LoadShedder::new(true);
        assert_eq!(
            psi_shed.apply(psi(43.0)).map(|(_, new)| new),
            Some(LoadLevel::Shed)
        );
        let mut load_shed = LoadShedder::new(true);
        assert_eq!(load_shed.apply(loadavg(43.0)), None);
        assert_eq!(load_shed.level(), LoadLevel::Normal);
    }

    #[test]
    fn malformed_proc_text_is_not_guessed() {
        assert_eq!(parse_psi_some_avg10(""), None);
        assert_eq!(parse_psi_some_avg10("some"), None);
        assert_eq!(
            parse_psi_some_avg10("some avg60=1.00\n"),
            None,
            "没有 avg10 键时必须读不出"
        );
        assert_eq!(parse_psi_some_avg10("some avg10=nan\n"), None);
        assert_eq!(
            parse_psi_some_avg10("some avg10=-1.50\n"),
            None,
            "负值不是压力"
        );
        assert_eq!(parse_loadavg_1m("0.71 0.66 0.62 2/1234 5678"), Some(0.71));
        assert_eq!(parse_loadavg_1m(""), None);
    }

    #[test]
    fn shed_requires_the_threshold_and_relief_requires_the_lower_band() {
        let mut shed = LoadShedder::new(true);
        // 阈值以下：不降载（哪怕很接近）。
        assert_eq!(shed.apply(psi(SHED_AT - 0.1)), None);
        assert_eq!(shed.level(), LoadLevel::Normal);
        // 达到阈值：进入降载，返回变化。
        assert_eq!(
            shed.apply(psi(SHED_AT)),
            Some((LoadLevel::Normal, LoadLevel::Shed))
        );
        // 滞回带内（两者之间）：**保持**降载，不来回抖。
        let mid = (SHED_AT + RELIEF_AT) / 2.0;
        assert_eq!(shed.apply(psi(mid)), None);
        assert_eq!(shed.level(), LoadLevel::Shed);
        // 低于下沿才恢复。
        assert_eq!(
            shed.apply(psi(RELIEF_AT - 0.1)),
            Some((LoadLevel::Shed, LoadLevel::Normal))
        );
    }

    #[test]
    fn unreadable_pressure_keeps_the_current_level() {
        let mut shed = LoadShedder::new(true);
        shed.apply(psi(80.0));
        assert_eq!(shed.level(), LoadLevel::Shed);
        // 读不到时**不得**因为"0.0 < RELIEF_AT"而恢复满速。
        assert_eq!(
            shed.apply(CpuPressure {
                percent: 0.0,
                source: PressureSource::Unavailable
            }),
            None
        );
        assert_eq!(shed.level(), LoadLevel::Shed, "读数缺失时保持当前档位");
    }

    #[test]
    fn disabled_option_does_nothing_at_all() {
        // `reduce_load_on_cpu_pressure = false` ⇒ 不采样、不切档、上限恒为配置值。
        let mut shed = LoadShedder::new(false);
        let now = Instant::now();
        assert!(!shed.sample_due(now), "关闭时不该去读 /proc");
        assert_eq!(shed.apply(psi(100.0)), None);
        assert_eq!(shed.level(), LoadLevel::Normal);
        assert_eq!(shed.refresh(now), None);
        assert!(shed.reading().is_none(), "关闭时不该留下任何读数");
        // `fps_cap` 是纯函数：Shed 档就是半速（30 ⇒ 15），与开关无关。
        // "关闭降载"要验证的不变量是 **LoadShedder 永不进入 Shed 档**，
        // 于是 `fps_cap(base, shed.level())` 恒等于配置值 —— 这才是真实行为。
        assert_eq!(
            fps_cap(30, LoadLevel::Shed),
            15.0,
            "Shed 档固定半速（纯函数契约）"
        );
        assert_eq!(
            fps_cap(30, shed.level()),
            30.0,
            "关闭降载时档位恒为 Normal，上限即配置值"
        );
    }

    #[test]
    fn turning_the_option_off_resets_to_normal() {
        // `reduce_load_on_cpu_pressure = false` 必须在 reload 后**立刻**停止降载，
        // 否则配置说不要降、运行期还在半速（静默失效的反向版本）。
        let mut shed = LoadShedder::new(true);
        shed.apply(psi(60.0));
        assert_eq!(shed.level(), LoadLevel::Shed);
        shed.set_enabled(false);
        assert_eq!(shed.level(), LoadLevel::Normal, "关闭后不得留在降载档");
        assert!(shed.reading().is_none(), "关闭后不该留旧读数");
        assert!(!shed.sample_due(Instant::now()));
        shed.set_enabled(true);
        assert_eq!(shed.level(), LoadLevel::Normal, "重新打开时从零判定");
    }

    #[test]
    fn sampling_is_throttled() {
        let mut shed = LoadShedder::new(true);
        let t0 = Instant::now();
        assert!(shed.sample_due(t0), "首次必须采样（否则永远没有档位）");
        shed.refresh(t0);
        assert!(!shed.sample_due(t0 + Duration::from_millis(900)));
        assert!(shed.sample_due(t0 + SAMPLE_INTERVAL));
    }

    #[test]
    fn shed_halves_the_submit_rate_but_never_below_one_fps() {
        assert_eq!(fps_cap(30, LoadLevel::Normal), 30.0);
        assert_eq!(fps_cap(30, LoadLevel::Shed), 15.0);
        assert_eq!(fps_cap(1, LoadLevel::Shed), 1.0, "1fps 配置不得被压成 0.5");
        assert_eq!(
            fps_cap(0, LoadLevel::Shed),
            1.0,
            "非法 0 也不该产出 0（=不限速的假象）"
        );
    }

    #[test]
    fn proc_is_readable_on_this_host_or_reports_unavailable() {
        // 这条只验证"不 panic、口径可观测"：CI/容器里读不到 /proc 时必须是
        // Unavailable，而不是假装 0% 压力（后者会让降载静默失效）。
        let reading = read_cpu_pressure();
        match reading.source {
            PressureSource::Psi | PressureSource::LoadAvg => {
                assert!(
                    (0.0..=100.0).contains(&reading.percent),
                    "百分比必须落在 0..=100，实际 {}",
                    reading.percent
                );
            }
            PressureSource::Unavailable => {}
        }

        // 只要能读到可解析的 `/proc/pressure/cpu`，真实读取就必须报 Psi——
        // 回退成 LoadAvg 走的是另一套阈值（t35 的 bug 就是这里永远走不到 Psi）。
        let psi_text = std::fs::read_to_string("/proc/pressure/cpu").ok();
        if let Some(percent) = psi_text.as_deref().and_then(parse_psi_some_avg10) {
            assert_eq!(reading.source, PressureSource::Psi);
            assert_eq!(reading.percent, percent);
        }
    }
}
