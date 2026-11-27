//! `liuguang` 常驻运行时（M2–M6）。
//!
//! 组成：IPC 服务（路由层）+ 单线程运行时（唯一接触显示/媒体的线程，
//! 遵守 §3.3「不在 socket 线程做阻塞解码」）+ 每输出通道（M6 多屏：
//! 独立播放列表/调度/暂停/转场）+ 运行状态持久化 + 配置 reload +
//! 事件订阅广播（M3）。
//!
//! 设备异常（断线/占用/坏图）只影响对应请求或转入等待状态，
//! 不终止守护进程，不影响服务器其他服务（§11.2）。

use liuguang_core::config::{
    Config, DisplayBackendKind, ModeSelection, PlaybackMode, PlaylistConfig, PlaylistEntryConfig,
    PlaylistOrder, playback_mode_from,
};
use liuguang_core::fit::FitParams;
use liuguang_core::persona::Persona;
use liuguang_core::playlist::{MediaEntry, OrderMode, Playlist};
use liuguang_core::scheduler::IntervalScheduler;
use liuguang_core::state::{Event, State, StateMachine};
use liuguang_core::transform::Rotation;
use liuguang_core::types::{CpuFrame, DisplayFrame, MediaKind, OutputId};
use liuguang_display::DisplayBackend;
use liuguang_ipc::protocol::MAX_SCAN_ENTRIES;
use liuguang_ipc::protocol::{
    AcceptedResult, AddSourceParams, AddSourceResult, ChannelMetrics, ChannelStatus,
    CpuPressureStatus, DeleteMediaParams, DeleteMediaResult, DeletePlaylistParams,
    DeletePlaylistResult, DisplayCommitStatus, DisplayMasterStatus, ErrorBody, ErrorKind,
    EventKind, EventMessage, FullscreenInfo, ListMediaResult, ListPlaylistsResult, MediaItem,
    Method, MetricsResult, OutputParams, OutputStatus, OutputsResult, PlaylistEntry,
    RemoveSourceParams, Request, Response, SaveConfigResult, ScanSourceParams, ScanSourceResult,
    SetOutputTransformParams, SetWallpaperParams, StatusResult, TransitionParams,
    UpdatePlaylistParams,
};
use liuguang_media::FramePoll;
use liuguang_media::prepared::prepare_image_fitted;
use liuguang_media::video::{DecoderKind, VideoSource, VideoStats};
use liuguang_render::cpu::render_fit;
use liuguang_render::transition::{CpuTransition, EffectKind, TransitionSpec};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 守护进程选项。
pub struct DaemonOptions {
    /// 控制 socket 路径（已按人格解析；CLI 覆盖优先）。
    pub socket_path: PathBuf,
    /// 状态文件路径（已按人格解析；CLI 覆盖优先）。
    pub state_file: PathBuf,
    /// 配置文件路径（已按人格解析；CLI 覆盖优先）。
    pub config_path: PathBuf,
    /// DRM 设备（覆盖配置）。
    pub device: Option<String>,
    /// 显示后端种类（`Auto` 由 [`liuguang_core::config::DisplayBackendKind::resolved`] 落地）。
    pub backend: DisplayBackendKind,
    /// 进程人格（决定 XDG/系统路径与用户服务语义）。
    pub persona: Persona,
}

/// 持久化格式版本（v2：多输出通道；v3：通道带 `rotation`）。
///
/// 为什么旋转要进 state.json，而不只写回 `config.toml`：
/// 配置文件可能只读（系统安装、包管理的 `/etc`），此时"转过来"仍应活过重启；
/// 两条路互为兜底（`config.toml` 是权威，`state.json` 是最近一次用户动作）。
pub const STATE_VERSION: u32 = 3;

/// 视频暂停超过该时长后主动卸载解码器与 GBM 渲染池，释放显存与 RSS；
/// 恢复播放时自动按原路径重建。
const PAUSE_UNLOAD_DELAY: Duration = Duration::from_secs(20);

/// 运行状态持久化（§12-M2/M6）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedState {
    /// 持久化格式版本。
    pub version: u32,
    /// 各通道状态。
    pub channels: Vec<PersistedChannel>,
}

/// 单通道持久化状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedChannel {
    /// 输出名。
    pub output: String,
    /// 媒体路径。
    pub media: String,
    /// 适配模式名。
    pub fit: Option<String>,
    /// 缩放。
    pub zoom: f64,
    /// X 偏移。
    pub offset_x: f64,
    /// Y 偏移。
    pub offset_y: f64,
    /// 模式选择字符串。
    pub mode: String,
    /// 是否"只放这一项"（重启后仍然保持单项播放）。
    #[serde(default)]
    pub pinned: bool,
    /// 旋转（v3 起；v2 文件缺此字段时按 0 度处理）。
    #[serde(default)]
    pub rotation: Rotation,
    /// 实时亮度倍率（v4 起；旧文件缺此字段时按 1.0 处理）。
    #[serde(default = "default_persisted_brightness")]
    pub brightness: f64,
}

fn default_persisted_brightness() -> f64 {
    1.0
}

/// 解析适配模式名（CLI/IPC/持久化共用）。
#[must_use]
pub fn parse_fit(name: &str) -> Option<liuguang_core::config::FitMode> {
    // 兼容历史状态文件里的 Debug 名（"Cover"/"Contain"/...）以及用户输入的大小写；
    // 持久化统一写小写（见 `FitMode::as_str`），这里只做宽松读取。
    match name.to_ascii_lowercase().as_str() {
        "cover" => Some(liuguang_core::config::FitMode::Cover),
        "contain" => Some(liuguang_core::config::FitMode::Contain),
        "stretch" => Some(liuguang_core::config::FitMode::Stretch),
        "center" => Some(liuguang_core::config::FitMode::Center),
        _ => None,
    }
}

/// 8K 级 H.264 的**产品级告警**（`None` = 没有这个问题）。
///
/// 事实依据：NVDEC 的 H.264 硬解上限是 4096 像素宽，7680×4320 的 H.264 只能软解；
/// 8K **HEVC** 可以硬解。具体吞吐/CPU/内存数字随机器与素材变化，只写在
/// `docs/VERIFICATION_2026-11-22_MATRIX.md` 里 —— 面向用户的文案只陈述确定事实，
/// 免得下次复跑数字一变就变成"文档说谎"。
///
/// 判据：容器头（[`crate::probe`]）里的宽 > 4096 且编码属于 H.264 家族。
/// **不用** `MediaInfo.codec` 作为唯一依据：软解路径曾把它写死成 `"h264"`，
/// 单靠它会误报；两条都拿不到时才退回 `MediaInfo`。
fn eight_k_h264_alert(path: &Path, info: &liuguang_core::types::MediaInfo) -> Option<String> {
    let details = crate::probe::probe(path, "video");
    let width = details
        .as_ref()
        .map_or(info.width, |d| d.width.max(info.width));
    let height = details
        .as_ref()
        .map_or(info.height, |d| d.height.max(info.height));
    let codec = details
        .as_ref()
        .and_then(|d| d.codec.clone())
        .or_else(|| info.codec.clone())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_h264 = codec.contains("h264") || codec.contains("avc");
    if width > 4096 && is_h264 {
        // 只陈述**确定的事实**：具体 fps/核数/内存随机器与素材变化（t21 F5 指出旧文案
        // 里的硬编码数字与复跑结果不符），一律不写死。
        Some(format!(
            "{width}×{height} H.264 超出 NVDEC 的 H.264 解码上限（4096 像素宽），\
             只能软件解码：解码速度会明显跟不上、并占用较多 CPU 与内存；\
             建议改用 HEVC 版本（同尺寸可硬解）"
        ))
    } else {
        None
    }
}

/// VA-API 打开失败是否是**源本身的能力问题**（换节点也无解）。
///
/// 判据是解码器给出的结论，不是字符串猜测式归因：
/// - `10bit`：P010/YUV420P10 源在整个解码/RGB 链路上都只支持 8bit NV12；
/// - `没有可用的 VA-API 硬件解码器`：该编码在本机 ffmpeg 构建里没有硬解实现。
///
/// 其余（节点被占、修饰符不被合成器接受、首帧超时）都是**环境**问题，
/// 换节点/换路径可能就好了 —— 那些必须继续回退。
///
/// 下面这个自由函数判断 `[media].video_vaapi`（含 `LIUGUANG_VIDEO_VAAPI` 覆盖）
/// 是否允许 VA-API 零拷贝：构造期（还没有 `Runtime`）与含 `&self` 的判定都要用
/// 同一份规则，否则"判定用的路径"与"播放用的路径"会分叉（t33 / t37）。
///
/// 自由函数版本：构造期（还没有 `Runtime`）与含 `&self` 的判定都要用同一份规则，
/// 否则"判定用的路径"与"播放用的路径"会分叉（t33 / t37）。
fn vaapi_setting_enabled_in(config: &Config) -> bool {
    let setting = std::env::var("LIUGUANG_VIDEO_VAAPI")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.media.video_vaapi.clone());
    let lowered = setting.trim().to_ascii_lowercase();
    !matches!(lowered.as_str(), "off" | "0" | "false" | "no" | "none")
}

/// 启动期的默认解码路径（只用作**默认值/日志**）。
///
/// 真正的判定必须按输出求值（[`Runtime::playback_consumer`]）：VAAPI 构建 +
/// `LIUGUANG_VIDEO_VAAPI=off` 时，设备探测仍然说 Vaapi，但这条输出实际走
/// GStreamer —— 转码产物要跟着后者走（t37 F2）。
fn startup_consumer(
    config: &Config,
    backend: &dyn DisplayBackend,
    caps: &crate::compat::HardwareCaps,
) -> crate::compat::DecodeConsumer {
    crate::compat::DecodeConsumer::for_playback(
        cfg!(feature = "vaapi"),
        vaapi_setting_enabled_in(config),
        backend.supports_dmabuf_scanout(),
        caps.consumer,
    )
}

#[cfg(feature = "vaapi")]
fn vaapi_rejection_is_permanent(message: &str) -> bool {
    message.contains("10bit") || message.contains("没有可用的 VA-API 硬件解码器")
}

/// 解码器类别 → `status.decoder` 里的短名（可观测）。
///
/// 不加 feature 门：`attach_video_player`（两条生产线共用）在**默认构型**下
/// 也会编译，它要用这个名字拼 `status.decoder`。
// 默认（无 gstreamer / vaapi）构型下视频链路整体不可用，这两个只在带 feature 时被调用；
// 用 allow 而不是 cfg 门控：它们的**签名与语义**与 feature 无关，门控会让三种构型
// 编译出不同的 API 形状，反而更难维护。
#[cfg_attr(not(any(feature = "gstreamer", feature = "vaapi")), allow(dead_code))]
fn decoder_kind_name(kind: DecoderKind) -> &'static str {
    match kind {
        DecoderKind::Vaapi => "vaapi",
        DecoderKind::Software => "software",
        DecoderKind::Mock => "mock",
        DecoderKind::None => "none",
    }
}

/// 视频生产者类别（`show_video_with` 的唯一决策点，纯函数便于无硬件单测）。
#[cfg(feature = "gstreamer")]
#[derive(Debug, PartialEq, Eq)]
enum ProducerKind {
    /// 系统内存 NV12/RGBA（不碰 DMA-BUF）。
    Cpu,
    /// 按环境变量 `LIUGUANG_VIDEO_DMABUF` 决定，可尝试直通。
    Auto,
    /// RGBA 像素级处理（旋转等）。
    Rgba,
}

/// **生产者必须与显示后端的实际能力一致**（2026-10-20 审核 P1-1）。
///
/// 直通扫描输出已从显示侧移除，但 `DrmKmsBackend` 之外还可能有接受直通的后端，
/// 所以判据是 [`DisplayBackend::supports_dmabuf_scanout`] 的返回值，而不是环境变量。
/// 遗留 `LIUGUANG_VIDEO_DMABUF=1` 的旧 unit 若仍按直通协商开管线，每一帧都会被
/// 显示侧拒绝 ⇒ 视频从"能播"回归成"播不了"。
///
/// 优先级保持与历史一致：需要像素级变换（旋转/亮度）⇒ RGBA；已闩死/后端不支持 ⇒ CPU；否则 Auto。
#[cfg(feature = "gstreamer")]
fn producer_kind(
    force_cpu: bool,
    backend_dmabuf: bool,
    needs_pixel_transform: bool,
) -> ProducerKind {
    if needs_pixel_transform {
        ProducerKind::Rgba
    } else if force_cpu || !backend_dmabuf {
        ProducerKind::Cpu
    } else {
        ProducerKind::Auto
    }
}

/// `performance.max_cache_mb` → 预加载缓存的**字节**预算。
///
/// 这是"应用缓存软预算"（计划 §10 注释），不是整机内存限制：预算只约束
/// [`liuguang_media::prepared::PreloadCache`] 里**解码后**的图片缓冲总字节数。
fn preload_budget_bytes(config: &Config) -> usize {
    (config.performance.max_cache_mb as usize).saturating_mul(1024 * 1024)
}

/// "文件还是不是预加载时那一份"的凭据。
///
/// 预加载与显示之间隔着一次轮换（默认间隔 600s）。素材被换掉却仍命中缓存，
/// 用户看到的就是**上一版的像素**——而且没有任何日志会说这件事。
/// 因此命中前必须核对，宁可多重做一次同步解码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    /// 字节数。
    len: u64,
    /// 修改时间（某些文件系统不给 ⇒ 只比长度）。
    mtime: Option<std::time::SystemTime>,
}

/// 读取 [`FileStamp`]；不可 stat 时返回 `None`（调用方按"不命中"处理）。
fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        len: meta.len(),
        mtime: meta.modified().ok(),
    })
}

/// 配置 → 解码器策略（解码器守卫的配置入口）。
///
/// - `disabled`：只允许软解（配置意图优先）。
/// - `required`：必须硬解，拿不到就报错（**不接受**任何软解回退）。
/// - `auto`：硬解优先，是否允许软解回退由 `media.software_decode_fallback` 决定；
///   允许时也必须大声告警。
#[cfg(feature = "gstreamer")]
fn video_policy(config: &Config) -> liuguang_media::gstreamer::DecoderPolicy {
    use liuguang_core::config::HardwareDecode;
    match config.media.hardware_decode {
        HardwareDecode::Disabled => liuguang_media::gstreamer::DecoderPolicy {
            prefer_software: true,
            require_hardware: false,
            allow_software_fallback: true,
        },
        HardwareDecode::Required => liuguang_media::gstreamer::DecoderPolicy {
            prefer_software: false,
            require_hardware: true,
            allow_software_fallback: false,
        },
        HardwareDecode::Auto => liuguang_media::gstreamer::DecoderPolicy {
            prefer_software: false,
            require_hardware: false,
            allow_software_fallback: config.media.software_decode_fallback,
        },
    }
}

/// 事件广播总线（`subscribe` 订阅者注册表）。
#[derive(Clone, Default)]
pub struct EventBus {
    subscribers: Arc<Mutex<Vec<Sender<EventMessage>>>>,
}

impl EventBus {
    /// 注册订阅者；返回发送端（服务器侧持有接收端推送）。
    pub fn subscribe(&self) -> Receiver<EventMessage> {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(tx);
        }
        rx
    }

    /// 广播事件；发送失败（连接已断）的订阅者被移除。
    pub fn publish(&self, event: EventMessage) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|tx| tx.send(event.clone()).is_ok());
        }
    }
}

/// 转场设置（M3 记录、M4 生效）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionSettings {
    /// 效果名。
    pub effect: String,
    /// 时长（毫秒）。
    pub duration_ms: u32,
    /// 缓动名。
    pub easing: String,
    /// 方向名。
    pub direction: Option<String>,
}

impl Default for TransitionSettings {
    fn default() -> Self {
        Self {
            effect: "fade".to_string(),
            duration_ms: 350,
            easing: "ease-in-out".to_string(),
            direction: None,
        }
    }
}

impl TransitionSettings {
    /// 构造 [`TransitionSpec`]（效果名非法时回退 fade）。
    fn to_spec(&self) -> TransitionSpec {
        use liuguang_core::config::{Easing, TransitionDirection};
        let easing = match self.easing.as_str() {
            "linear" => Easing::Linear,
            "ease-in" => Easing::EaseIn,
            "ease-out" => Easing::EaseOut,
            _ => Easing::EaseInOut,
        };
        let direction = match self.direction.as_deref() {
            Some("left") => Some(TransitionDirection::Left),
            Some("right") => Some(TransitionDirection::Right),
            Some("up") => Some(TransitionDirection::Up),
            Some("down") => Some(TransitionDirection::Down),
            _ => None,
        };
        let kind = match self.effect.as_str() {
            "slide" => EffectKind::Slide {
                direction: direction.unwrap_or(TransitionDirection::Left),
            },
            "wipe" => EffectKind::Wipe {
                direction: direction.unwrap_or(TransitionDirection::Left),
            },
            "circle" => EffectKind::Circle,
            "random" => {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos());
                let dir = match seed % 4 {
                    0 => TransitionDirection::Left,
                    1 => TransitionDirection::Right,
                    2 => TransitionDirection::Up,
                    _ => TransitionDirection::Down,
                };
                match (seed / 4) % 4 {
                    0 => EffectKind::Fade,
                    1 => EffectKind::Slide { direction: dir },
                    2 => EffectKind::Wipe { direction: dir },
                    _ => EffectKind::Circle,
                }
            }
            _ => EffectKind::Fade,
        };
        TransitionSpec {
            kind,
            easing,
            duration: Duration::from_millis(u64::from(self.duration_ms)),
        }
    }
}

/// 当前显示内容（保留渲染帧供转场/断线恢复）。
struct CurrentDisplay {
    media: PathBuf,
    frame: CpuFrame,
    mode_str: String,
    decoder: String,
}

/// 进行中的转场（M4；最后请求优先——新切换直接丢弃旧转场）。
struct ActiveTransition {
    anim: CpuTransition,
    started: Instant,
    duration: Duration,
}

/// 一次降级收敛**实际记录**的原因（2026-10-20 审核 P2-6）。
///
/// 旧实现用"有没有 `video_resume_pending`"反推原因类别，判据不成立：
/// 该字段只表示"欠用户一次恢复意图"，**不是**所有失败原因的枚举
/// （真帧错误若恰好发生在 master 未确认期也会留下 pending）。
/// 现在把收敛当时的原始原因与"是否确认持有 master"一起存下来，状态输出如实呈现。
struct DegradedReason {
    /// 收敛时拿到的原因文本（提交失败细节 / 解码错误 / 停摆等）。
    message: String,
    /// 收敛**当时**后端是否已确认持有 DRM master。
    ///
    /// `false` = 环境可能只是暂时不可用（重启竞态里 `drmSetMaster` 返回 EACCES），
    /// master 取回后自愈逻辑会自动重建视频；`true` = 帧/平面一侧真有错，
    /// 不会自动重开。
    master_confirmed: bool,
}

/// `status` 里 `DegradedStatic` 的**人话原因**（非降级态返回空串，避免噪音）。
///
/// 只描述**记录在案**的事实；没有记录（历史状态文件、未经视频路径的降级）时
/// 明说"原因未记录"，不把它硬塞成"帧错误"或"环境问题"二选一。
fn degraded_reason_hint(state: &str, reason: Option<&DegradedReason>) -> String {
    if state != "DegradedStatic" {
        return String::new();
    }
    match reason {
        Some(r) if !r.master_confirmed => format!(
            "DegradedStatic 原因：**环境暂时不可用**（收敛时未确认持有 DRM master，不一定是帧错误）\
             ⇒ master 自愈后会自动重建视频；当时记录：{}",
            r.message
        ),
        Some(r) => format!(
            "DegradedStatic 原因：**帧/平面一侧的提交错误**（当时已持有 DRM master）\
             ⇒ 不会自动重开，请按日志处理；当时记录：{}",
            r.message
        ),
        None => "DegradedStatic 原因：**未记录**（降级发生在视频路径之外，或来自旧状态文件）\
                 ⇒ 不能据此断定是帧错误还是环境问题；请查看日志与 `outputs`/`commit` 计数"
            .to_string(),
    }
}

/// 每输出通道状态（M6 多屏：独立播放列表/调度/暂停/转场）。
struct ChannelState {
    playlist: Playlist,
    scheduler: IntervalScheduler,
    paused: bool,
    /// **只放这一项**（`true` = 不跟随轮换）。
    ///
    /// 语义与 `paused` 正交：`paused` 是"停住画面不动"，本字段是"画面照常播，
    /// 但不要按 `interval_seconds` 换片"——单视频循环、单张图片常驻都靠它。
    /// 播放列表驱动 + `SingleLoop` 仍有自己的一条跳过规则，两者互不替代。
    pinned: bool,
    /// **t69**：当前是否处于"静态壁纸"模式（暂停播放列表并显示单张图片）。
    ///
    /// 存的是那张图片的路径，用于：
    /// * `clear_static_wallpaper` 判断是否真的需要恢复；；
    /// * 幂等（重复设置同一张图不会重复暂停）；
    /// * 状态上报时区分"用户暂停"与"静态壁纸"。
    static_wallpaper: Option<std::path::PathBuf>,
    /// 当前这一路视频的**产品级解码告警**（`None` = 正常）。
    ///
    /// 目前只有「8K 级 H.264 没有硬件解码」这一类：它不改播放行为
    /// （画面照常走），但**必须让用户知道为什么这么卡**，
    /// 所以它同时进 `status.channels[].video.decode_alert` 与
    /// 顶层 `state_hint`（没有降级原因时）。
    decode_alert: Option<String>,
    current: Option<CurrentDisplay>,
    video: Option<Box<dyn liuguang_media::video::VideoSource + Send>>,
    /// 视频进入暂停态的时刻（达到 [`PAUSE_UNLOAD_DELAY`] 后卸载管线释显存）。
    paused_since: Option<Instant>,
    /// 因长时间暂停而主动卸载的原视频路径（恢复播放时自动重建）。
    paused_video_media: Option<PathBuf>,
    transition: Option<ActiveTransition>,
    pending_target: Option<CpuFrame>,
    /// 停摆看门狗：上次观察到的 `presented` 值。
    last_presented: u64,
    /// "视频推进"日志节拍：上次观察到的 `dropped_late`（用于判断**跨越** 60 的倍数，
    /// 而不是对累计值取模——后者在掉过一帧后恒真，会变成每帧一条 INFO）。
    last_dropped_late: u64,
    /// 停摆看门狗：`presented` 最后一次增长的**绝对**时刻。
    last_presented_at: Instant,
    /// 停摆看门狗：是否已就本次停摆上报/降级（避免重复）。
    stall_reported: bool,
    /// 停摆自愈：**连续**重建管线、但之后一帧都没观察到的次数。
    ///
    /// 只在"判停摆 → 重建"时递增，`presented` 一旦增长就清零（真健康信号）。
    /// 用途是给自愈封顶：重建成功却依旧不出帧时必须收敛到 `DegradedStatic`，
    /// 不能无限重开管线（否则日志变成每 10s 一条"已重建"而画面永远不动）。
    stall_rebuilds: u32,
    /// 扫描输出**提交失败**累计次数（含 tiled 被拒 / 丢 master / 原子提交失败）。
    present_failures: u64,
    /// 帧内容探针（`LIUGUANG_FRAME_PROBE_DIR` 未设时零开销）。
    frame_probe: liuguang_display::frame_probe::FrameProbe,
    /// 连续提交失败次数（成功一次即清零）。
    present_failures_consecutive: u32,
    /// **成功提交**扫描输出的帧数（与媒体侧"出队"计数 `presented` 可区分）。
    presented_ok: u64,
    /// 显示层记账里"该帧未提交"（`CommitDelta::failed`）的累计次数。
    ///
    /// 与 `present_failures` 互补：前者是 `present_*` 的 Ok/Err，后者是**即使
    /// 返回 Ok 也可能在 DRM 层留下的失败记录**（EBUSY 重试/legacy 回退路径），
    /// 两者一起看才算闭环。
    drm_commit_failed: u64,
    /// 连续多少次提交在显示层记账里出现 `failed`（成功一次即清零）。
    drm_failed_consecutive: u32,
    /// 显示层对账：上次 `note_drm_commit` 观察到的**媒体侧** `presented` 值。
    ///
    /// 用它算"同一区间内媒体说交付了几帧"，才能真正对账
    /// （把常量 `1` 传进 `contradicts_presented` 会让判据恒真 ⇒ 告警预算失效）。
    drm_last_presented: u64,
    /// 本次失败 episode 已用掉的 `WARN` 条数（干净提交后重置）。
    ///
    /// "episode 预算"：失败风暴里**每次**失败都 WARN 会把日志刷爆（真机实测
    /// 700+ 帧），但完全静默又违反"绝不静默丢帧"——所以按 episode 限速。
    drm_loud_used: u64,
    /// 该通道**生命周期**内这类 WARN 的总条数（episode 预算之外的硬上限）。
    drm_loud_total: u64,
    /// "该通道至今没有任何一次成功提交"这条**一次性**升级告警是否已发过。
    drm_zero_ok_warned: bool,
    /// 显示侧是否已**闩死** DMA-BUF（被拒后不再尝试直通）。
    dmabuf_latched: bool,
    /// DMA-BUF 被拒累计次数（被拒 ≠ 视频失败：重开 CPU 生产者即可继续播）。
    dmabuf_rejections: u64,
    /// 因**未持有 DRM master** 被丢弃的帧数。
    ///
    /// 与 [`Self::present_failures`] **分开记账**：master 不可用是可重试的环境
    /// 错误，不是帧坏了。旧实现把两者混在一起 ⇒ 服务重启竞态下 3 帧就降级成
    /// 静态图，用户看到"卡住"（t94）。
    master_unavailable_skips: u64,
    /// master 自愈后**尝试**恢复视频供给的次数（可观测：证明"欠着的恢复"真的在被处理）。
    video_resume_attempts: u64,
    /// 视频供给被中断时记下的媒体路径：**master 自愈后据此重建并恢复视频**。
    ///
    /// 旧实现自愈只重放静态通道，视频通道永远停在最后一帧（t94 真机缺陷）。
    video_resume_pending: Option<PathBuf>,
    /// 最近一次**降级收敛**记录的原因（`status.state_hint` 的事实来源）。
    ///
    /// 与 `video_resume_pending` 分开：前者是"为什么降级"，后者是"欠不欠一次自动重建"。
    degraded_reason: Option<DegradedReason>,
    /// **播放源标签**（T68）：`"directory"` | `"playlist:<id>"`。
    ///
    /// `status.playback_source` 直接取自这里 —— 它回答的是"现在到底是什么在驱动
    /// 这个通道"，而不是"配置里写了什么"。t69 的静态壁纸会插到它**前面**
    /// （`"static"` 优先级最高）。
    source_label: String,
    /// **播放模式**（T68）：由 `mode` + `on_video_end` 经
    /// [`liuguang_core::config::playback_mode_from`] 收敛而来（唯一映射点）。
    mode: PlaybackMode,
    /// 上一个观察到的视频 PTS（"播完一轮"的唯一可用信号，见 [`Runtime::note_video_pts`]）。
    last_video_pts: Option<Duration>,
    /// 当前这一项视频已观察到的**回卷次数**（观测计数；每开一个新视频清零）。
    video_loops: u64,
    /// 游标需要重建（配置 reload / `update_playlist` 之后置位）。
    playlist_dirty: bool,
    /// 播放源**不可用**的原因（`Some` ⇒ 该通道当前没有可播内容）。
    ///
    /// 绝不静默退回目录轮换：错误原样保存、由 [`Runtime::retry_broken_playlists`]
    /// 定期重试，恢复后自动清除。
    playlist_error: Option<ErrorBody>,
    /// 上次尝试修复坏播放源的时刻（重试限速，避免每 tick 重扫）。
    last_playlist_retry: Instant,
    /// 上一次已告警的轮换失败原因（去重：同一原因不按 interval 反复刷日志）。
    last_rotation_error: Option<String>,
    /// 待预加载的**下一张**图片（`performance.preload_next`）。
    ///
    /// 只存路径不存帧：帧归运行时的 `preload` 缓存按字节预算管，
    /// 而"这条通道接下来会播哪张"是通道状态 —— 通道随拔除一起消失时，
    /// 未完成的预加载意图也自然作废（§11.2「拔除 ⇒ 关闭对应输出流与缓存」）。
    preload_target: Option<PathBuf>,
}

impl ChannelState {
    /// 把停摆看门狗基线对齐到 `now`：`last_presented` 取播放器**当前**的真实计数。
    ///
    /// 用于"刚换上一条新管线"（`attach_video_player`）：`presented` 从 0 重新计，
    /// 基线必须同时归零。
    ///
    /// 暂停→恢复**不**在这里处理：暂停时长不计入"presented 零增长"的豁免由
    /// [`Runtime::step_channel`] 的暂停分支每个 tick 推进基线（见那里的注释）。
    /// 之所以不在恢复 handler 里写 `Instant::now()`：看门狗的时间基准是
    /// `tick(now)` 注入的时钟，与 IPC handler 里的真实时钟不在同一条时间轴上
    /// （单测会大步推进 tick 时钟）。
    fn reset_stall_watchdog(&mut self, now: Instant) {
        self.last_presented = self
            .video
            .as_ref()
            .map(|v| v.video_stats().presented)
            .unwrap_or(0);
        self.last_presented_at = now;
        self.stall_reported = false;
    }
}

/// 生效的媒体来源（配置 + 运行时增删的并集）。
struct EffectiveSource {
    /// 来源 id（规范化后的绝对路径）。
    id: String,
    /// `image` / `video` / `all`（旧 `[slideshow].directory`）。
    kind: String,
    /// 目录路径。
    path: PathBuf,
    /// 该来源是否递归。
    recursive: bool,
}

/// 一次在跑的扫描（取消标志 + 代次）。
struct ScanHandle {
    scan_id: u64,
    cancel: Arc<AtomicBool>,
}

/// 媒体索引：**只**由后台扫描写入，`list_media`/`get_media` 只读。
///
/// 有界：条目数 ≤ [`MAX_SCAN_ENTRIES`]，错误 ≤ [`crate::scan::MAX_ERRORS`]。
#[derive(Default)]
struct MediaIndex {
    items: Vec<MediaItem>,
    /// 当前索引的扫描代次（`0` = 还没有扫描完成）。
    scan_id: u64,
    /// 索引生成时刻（Unix 毫秒；`0` = 未生成）。
    generated_unix_ms: u64,
    /// 是否因条目上限被截断。
    truncated: bool,
    /// 最近一次扫描的错误（有界）。
    errors: Vec<String>,
    /// 最近一次扫描里因体积超限被跳过的文件数（`list_media` 如实透出）。
    oversize_skipped: u64,
    /// 超限样例（≤3 条，含上限，便于用户改 `[library].max_file_bytes`）。
    oversize_examples: Vec<String>,
    /// 在跑的扫描。
    running: Option<ScanHandle>,
}

/// 已受理、但**还没建解码管线**的 `set_wallpaper`（t45）。
///
/// 受理与上屏解耦：`set_wallpaper` 只做校验（路径/类型/compat）就回 `accepted`，
/// 真正的 `show_entry`（建 VA-API/GStreamer 管线、等首帧，0.6–1s）留到 `tick()`
/// 里做，完成后再用 `WALLPAPER_APPLIED` 事件通知客户端（request_id 一一对应）。
struct PendingSet {
    request_id: String,
    output: String,
    entry: MediaEntry,
    /// 客户端传进来的原始路径（事件里回显它，而不是转码缓存路径）。
    declared_path: String,
    pin: bool,
    transform_changed: bool,
}

/// 待发出的 `WALLPAPER_APPLIED` 事件（在 `tick()` 里发，保证"受理响应先于完成事件"）。
struct PendingApplied {
    request_id: String,
    output: String,
    path: String,
    ok: bool,
    error: Option<String>,
}

/// 保存时的来源合并**预演**结果（T37）。
///
/// 关键点：**先在克隆上算**，通过后才替换真实配置并落盘；因此"被拒绝的保存"
/// 对配置文件**零副作用**（字节不变）。
struct MergePlan {
    /// 合并后的配置（未通过预演时**不会**被采用）。
    config: Config,
    /// 实际合并的条数（新增 + 删除）。
    applied: usize,
    /// 用户要求删除、但合并后仍会被某容器派生的来源（id + 冲突容器名）。
    residual: Vec<(String, Vec<liuguang_core::library::SourceContainer>)>,
}

/// IPC 播放列表 → 配置形态（T62：落盘用）。
fn playlist_to_config(pl: &liuguang_ipc::protocol::Playlist) -> PlaylistConfig {
    PlaylistConfig {
        id: pl.id.clone(),
        name: pl.name.clone(),
        entries: pl
            .entries
            .iter()
            .map(|e| PlaylistEntryConfig {
                media_id: e.media_id.clone(),
            })
            .collect(),
        mode: pl.mode.clone(),
        interval_seconds: pl.interval_seconds,
        on_video_end: pl.on_video_end.clone(),
        output: pl.output.clone(),
        no_repeat: pl.no_repeat,
    }
}

/// 配置形态 → IPC 播放列表（T62：装载用；条目已过安全边界）。
fn config_to_playlist(pl: &PlaylistConfig) -> liuguang_ipc::protocol::Playlist {
    liuguang_ipc::protocol::Playlist {
        id: pl.id.clone(),
        name: pl.name.clone(),
        entries: pl
            .entries
            .iter()
            .map(|e| liuguang_ipc::protocol::PlaylistEntry {
                media_id: e.media_id.clone(),
            })
            .collect(),
        mode: pl.mode.clone(),
        interval_seconds: pl.interval_seconds,
        on_video_end: pl.on_video_end.clone(),
        output: pl.output.clone(),
        no_repeat: pl.no_repeat,
    }
}

/// 从配置装载播放列表（T62）。
///
/// # 为什么必须过安全边界
///
/// 配置文件是**用户可写**的，而 `entries[].media_id` 是**路径**：若装载时直接信任
/// 它，配置就成了"任意路径注入点"（读/播根目录之外的任意文件）。因此每条都要过
/// 既有的 [`liuguang_core::library::LibraryConfig::validate`] → `MediaRoot::resolve()`。
///
/// 失败按错误码分流（这是本函数的**语义核心**）：
///
/// | `PathErrorCode` | 处理 | 理由 |
/// |---|---|---|
/// | `InvalidParams` / `PathOutsideRoot` | **丢弃该条目** + WARN | 真正的越界（相对路径、`..`、根外、指向根外的符号链接）⇒ 必须拒绝 |
/// | `NotFound` / `NotAFile` / `TooLarge` / `ExtensionNotAllowed` / `Unreadable` | **保留原样** + WARN | 路径仍在安全边界内，只是"当前不可用"（挂载点未就绪、文件被删）⇒ 丢掉会导致下次 `save_config` 把它从配置里抹掉（数据丢失） |
///
/// 没有 `[library]` 时**一条都不装载**（没有可比对的边界），但同样不写回——
/// 配置保持原样，等用户配好媒体库后重启即可恢复。
fn load_playlists_from_config(cfg: &Config) -> (Vec<liuguang_ipc::protocol::Playlist>, bool) {
    use liuguang_core::pathguard::PathErrorCode;
    if cfg.playlists.is_empty() {
        return (Vec::new(), true);
    }
    let Some(lib) = cfg.library.as_ref() else {
        tracing::warn!(
            declared = cfg.playlists.len(),
            "配置里声明了播放列表但没有 [library]：没有可比对的安全边界 ⇒ 本次不装载（保存时会原样保留文件里的播放列表）"
        );
        return (Vec::new(), false);
    };
    let media = match lib.validate() {
        Ok(media) => media,
        Err((field, e)) => {
            tracing::warn!(
                field = %field,
                error = %e.message,
                declared = cfg.playlists.len(),
                "媒体库配置非法 ⇒ 本次不装载播放列表（保存时会原样保留文件里的播放列表）"
            );
            return (Vec::new(), false);
        }
    };
    let mut out: Vec<liuguang_ipc::protocol::Playlist> = Vec::with_capacity(cfg.playlists.len());
    let mut rejected = 0usize;
    let mut stale = 0usize;
    for pl in &cfg.playlists {
        let mut pl = config_to_playlist(pl);
        pl.entries.retain_mut(|entry| match media.resolve(&entry.media_id) {
            Ok(canonical) => {
                // 规范化（解析符号链接、去掉冗余分隔符）：运行期一律用规范路径。
                // 这样"用户手写的等价路径"不会在下一轮比较里造成假不一致。
                *entry = liuguang_ipc::protocol::PlaylistEntry {
                    media_id: canonical.to_string_lossy().into_owned(),
                };
                true
            }
            Err(e) => match e.code {
                PathErrorCode::InvalidParams | PathErrorCode::PathOutsideRoot => {
                    rejected += 1;
                    tracing::warn!(
                        playlist = %pl.id,
                        media_id = %entry.media_id,
                        code = e.code.as_str(),
                        error = %e.message,
                        "拒绝配置里的播放列表条目：路径越出媒体库安全边界（配置里的路径不可信）"
                    );
                    false
                }
                _ => {
                    stale += 1;
                    tracing::warn!(
                        playlist = %pl.id,
                        media_id = %entry.media_id,
                        code = e.code.as_str(),
                        error = %e.message,
                        "保留配置里的播放列表条目：路径在安全边界内但当前不可用（不丢弃，避免保存时被抹掉）"
                    );
                    true
                }
            },
        });
        out.push(pl);
    }
    tracing::info!(
        loaded = out.len(),
        rejected_entries = rejected,
        stale_entries = stale,
        "从配置装载播放列表（T62：save_config 落盘、启动时装载）"
    );
    (out, true)
}

/// 统一的环境自动暂停原因。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PauseReason {
    Fullscreen,
    ScreenOff,
    GameRunning,
    GpuMemory,
    Battery,
}

/// 单线程运行时：拥有显示后端与全部通道，串行处理请求。
pub struct Runtime {
    backend: Box<dyn DisplayBackend>,
    backend_name: String,
    backend_available: bool,
    config: Config,
    config_path: PathBuf,
    state_file: PathBuf,
    /// 运行人格（决定 `reload` 读到的默认路径落到哪个人格）。
    ///
    /// 构造期默认 [`Persona::System`]；守护进程入口在读配置后立刻
    /// [`set_persona`](Self::set_persona) 覆盖为实测人格。
    persona: Persona,
    state: StateMachine,
    channels: BTreeMap<String, ChannelState>,
    transitions: TransitionSettings,
    events: EventBus,
    started: Instant,
    /// 顶层状态机拒绝的**非法**转移累计次数（"半接线"故障的可观测计数器）。
    state_machine_errors: u64,
    /// 上一次已告警的"顶层状态 vs 通道事实"矛盾签名（去重用，避免刷屏）。
    last_contradiction: Option<String>,
    /// 是否已经做过"启动首帧"补偿（只做一次）。
    primed_first_frame: bool,
    /// 可注入全屏钩子的截止时刻（t43 验收④；`None` = 没开钩子）。
    test_fullscreen_until: Option<Instant>,
    /// 全屏钩子是否已经启动过（一次性；`remove_var` 在 edition 2024 是 unsafe，
    /// 而 workspace 禁止 unsafe，所以用标志代替"消费掉环境变量"）。
    test_fullscreen_applied: bool,
    /// 长间隔钩子是否已经用掉（同上，一次性）。
    test_gap_applied: bool,
    /// VA-API 首帧校验已通过的 `(节点路径, 是否RGB转换)`（t43）。
    vaapi_verified_node: Option<(PathBuf, bool)>,
    /// 媒体索引（异步扫描产物）。
    media: MediaIndex,
    /// 本机解码能力（t15：兼容性分类的输入；构造期探测一次）。
    caps: crate::compat::HardwareCaps,
    /// 后台转码器（t15：把硬件解不动的素材转成可硬解的缓存）。
    transcode: crate::transcode::Transcoder,
    /// 等转码完成后要自动播的通道（`set_wallpaper` 回 `NEEDS_PREPARATION` 时登记）。
    pending_playback: Vec<(String, PathBuf)>,
    /// 缩略图服务（t39：两级缓存 + 后台串行解码 + 计数）。
    thumbs: crate::thumb::Service,
    /// 上一次 tick 的注入时钟（t39：用于识别休眠 / `SIGSTOP` 造成的长间隔）。
    last_tick_at: Option<Instant>,
    /// 扫描结果接收端（tick 里非阻塞收取）。
    scan_rx: Receiver<crate::scan::ScanMsg>,
    /// 扫描结果发送端（交给后台线程）。
    scan_tx: Sender<crate::scan::ScanMsg>,
    /// 扫描代次计数器（只增）。
    next_scan_id: u64,
    /// 待发出 `WALLPAPER_APPLIED` 的请求（下一 tick 发出）。
    pending_applied: Vec<PendingApplied>,
    /// 已受理、等待建管线的 `set_wallpaper`（t45：受理与上屏解耦）。
    pending_sets: Vec<PendingSet>,
    /// 最近一次收到 IPC 请求的时刻（t46：启动首帧要避开它）。
    last_request_at: Option<Instant>,
    /// `set_wallpaper` 的 request_id 计数器。
    next_request_id: u64,
    /// 运行时新增的来源（`add_source`；**不落盘**，落盘走 `save_config`）。
    added_sources: Vec<(String, PathBuf, bool)>,
    /// 运行时移除的来源 id（`remove_source`）。
    removed_sources: Vec<String>,
    /// 播放列表（`update_playlist` 维护运行期状态；`save_config` 落盘、启动时装载 —— T62）。
    playlists: Vec<liuguang_ipc::protocol::Playlist>,
    /// 配置里的播放列表**是否已装载**（`false` = 缺 `[library]`/媒体库非法 ⇒ 无安全
    /// 边界可比对）。保存时据此决定"以运行时为准写盘"还是"原样保留文件里的"。
    playlists_loaded: bool,
    /// 进程自观测采样器（`get_metrics` 的 CPU/内存/线程/帧率）。
    sysstat: crate::sysstat::SysStat,
    /// 上一次尝试 DRM master 自愈的时刻（与"是否在提交"**解耦**，见 t26）。
    last_master_poll: Instant,
    /// 自愈成功后已重放的帧数（观测用）。
    master_heal_repaints: u64,
    /// CPU 压力降载判定器（`performance.reduce_load_on_cpu_pressure`，计划 §11.2）。
    shed: crate::loadshed::LoadShedder,
    /// 熄屏监视（`performance.pause_when_display_off`）。
    screen: crate::display_power::DisplayPower,
    /// **因为用户显式 pause 而**被暂停的通道。自动让路恢复时不得误解除它。
    user_paused: BTreeSet<String>,
    /// **因为熄屏而**被暂停的通道。只恢复这一集合里的通道 ——
    /// 用户手动 `pause` 的通道不能被"屏幕亮了"擅自恢复（那是越权）。
    screen_paused: std::collections::BTreeSet<String>,
    /// 因显存压力被本进程暂停的通道（`performance.pause_when_gpu_memory_above`）。
    gpu_paused: BTreeSet<String>,
    /// 显存压力控制器（阈值关闭时内部 `guard == None`，`poll` 永远返回 `Unchanged`）。
    /// 因「检测到游戏在跑」而暂停的通道（`performance.detect_games`）。
    game_paused: BTreeSet<String>,
    /// 游戏进程探测（进程表扫描，5s 一次；滞回在模块内）。
    game: crate::game_watch::Controller,
    /// 因「笔记本拔电使用电池供电」而被本进程暂停的通道（`performance.pause_on_battery`）。
    battery_paused: BTreeSet<String>,
    /// 电源状态监测（5s 采样一次 `/sys/class/power_supply/*`）。
    power: crate::power_watch::PowerWatch,
    gpu: crate::gpu_pressure::Controller,
    /// 因"有窗口全屏"而被暂停的通道（只恢复自己暂停的）。
    fullscreen_paused: BTreeSet<String>,
    /// 当前是否判定为"全屏中"（带滞回，见 [`Self::poll_fullscreen`]）。
    fullscreen_active: bool,
    /// 判定为全屏的起始时刻（滞回用）。
    fullscreen_since: Option<Instant>,
    /// 当前全屏窗口的快照（供 `status` 展示）。
    fullscreen_info: Option<liuguang_display::FullscreenToplevel>,
    /// 上次采样顶层窗口的时刻（500ms 一次足够：窗口状态变化远慢于帧率）。
    fullscreen_polled: Option<Instant>,
    /// 兜底探测（niri IPC）的下次允许时刻：合成器不发"退出全屏"事件时靠它确认。
    fullscreen_verify_due: Option<Instant>,
    /// 已通过兜底探测确认退出全屏的窗口签名（避免合成器残留状态标志引发 2 秒启停振荡）。
    fullscreen_suppressed: Option<String>,
    /// 图片预加载缓存（`performance.preload_next` 决定用不用、
    /// `performance.max_cache_mb` 决定字节预算）。
    preload: liuguang_media::prepared::PreloadCache,
    /// 图片解码的降采样目标（= 最近一次静态图输出的 1.25 倍尺寸）。
    ///
    /// 壁纸最终只上屏到输出分辨率；按这个尺寸解码可以把 61 MP PNG 的
    /// 临时缓冲从 ~244 MB 压到几 MB。预加载也用同一尺寸。
    preload_dims: (u32, u32),
    /// 每张已预加载图片的"文件凭据"（命中前核对，防止显示已被替换的旧帧）。
    preload_stamps: HashMap<PathBuf, FileStamp>,
    /// 待写回 `config.toml` 的偏好（防抖合并；见 `config_store`）。
    ///
    /// **只在用户显式通过 IPC 改过设置时才会非空**：`set`/`set_output_transform`/
    /// `transition`。累加的是"改过的段落"，同一输出改十次只写一次。
    pending_config: BTreeSet<crate::config_store::Preference>,
    /// 防抖到期时刻（`None` = 没有待写内容）。
    config_write_due: Option<Instant>,
    /// 写回失败是否已经 WARN 过（同一原因不刷屏，成功一次后重置）。
    config_write_warned: bool,
}

/// 把底层的 GStreamer/容器错误翻成人能看的话（t21 F3）。
///
/// `qtdemux ... streaming stopped, reason not-linked` 这类原文对用户毫无意义：它通常
/// 意味着解码/转换链不吃这个颜色格式（4:2:2 / 4:4:4）或位深。原始信息只进日志，
/// 面向用户的话必须说清"为什么"与"怎么办"。
// 只有 GStreamer 解码路径会调用它（默认构型没有解码器，函数自然用不上）。
#[cfg_attr(not(any(feature = "gstreamer", test)), allow(dead_code))]
fn friendly_decode_error(path: &Path, message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let link_failure = lower.contains("not-linked")
        || lower.contains("not linked")
        || lower.contains("reason error (-5)")
        || lower.contains("streaming stopped");
    if link_failure {
        return format!(
            "该素材的颜色格式本机解不了（{}）：常见原因是 4:2:2 / 4:4:4 色度抽样或 10bit 位深，             当前解码/RGB 转换链只支持 8bit 4:2:0 NV12。             请转成 8bit HEVC 4:2:0（例如 `ffmpeg -i in.mp4 -pix_fmt yuv420p -c:v libx265 out.mkv`），             或让 Liuguang 自动转成缓存（[media].transcode = \"auto\"，源文件不修改）",
            path.display()
        );
    }
    format!("视频打开失败（{}）: {message}", path.display())
}

impl Runtime {
    /// 创建运行时并尝试恢复上次显示状态。
    pub fn new(
        backend: Option<Box<dyn DisplayBackend>>,
        backend_name: &str,
        config: Config,
        config_path: PathBuf,
        state_file: PathBuf,
        events: EventBus,
    ) -> Result<Self, String> {
        let (scan_tx, scan_rx): (Sender<crate::scan::ScanMsg>, Receiver<crate::scan::ScanMsg>) =
            std::sync::mpsc::channel();
        let mut state = StateMachine::new();
        let _ = state.handle(Event::DisplayInitialized);
        let (backend, backend_available): (Box<dyn DisplayBackend>, bool) = match backend {
            Some(b) => (b, true),
            None => {
                tracing::warn!("无可用显示设备，进入 HeadlessReady（IPC 保持可用）");
                let _ = state.handle(Event::OutputMissing);
                (Box::new(liuguang_display::mock::UnavailableBackend), false)
            }
        };
        let persona = Persona::System;
        let transitions = transition_settings_from_config(&config.transition);
        // **T62**：播放列表随配置持久化 —— 启动时装载（条目过 media.resolve() 边界）。
        // `loaded=false` 表示"文件里有、但这次没有安全边界可比对（缺 [library]）"：
        // 此时保存必须**原样保留**文件里的播放列表，绝不能写空（数据丢失）。
        let (playlists, playlists_loaded) = load_playlists_from_config(&config);
        // 性能项在**构造期**就要落到位：预加载预算与降载开关都读 `config.performance`，
        // 之后只有 `reload`/`update_playlist` 换配置时才会重新同步（见 `sync_performance`）。
        let preload = liuguang_media::prepared::PreloadCache::new(preload_budget_bytes(&config));
        let shed =
            crate::loadshed::LoadShedder::new(config.performance.reduce_load_on_cpu_pressure);
        let screen =
            crate::display_power::DisplayPower::new(config.performance.pause_when_display_off);
        // 显存压力阈值要在 `config` 被移进结构体之前取出来。
        let gpu_above = f32::from(config.performance.pause_when_gpu_memory_above);
        let gpu_below = f32::from(config.performance.resume_when_gpu_memory_below);
        let game_enabled = config.performance.detect_games;
        let game_extra = config.performance.game_processes.clone();
        let game_ignore = config.performance.game_ignore_processes.clone();
        let power_enabled = config.performance.pause_on_battery;
        // **t15**：能力探测 + 转码器。都在构造期建好（转码线程按需起）。
        let caps = crate::compat::HardwareCaps::detect(config.media.transcode_max_width);
        // t37：转码目标跟着**实际解码路径**走。构造期还没有通道，先取默认输出的
        // 判定（设置项 + 后端能力 + 设备探测）；每个任务提交时再按输出确认一次。
        let startup = startup_consumer(&config, backend.as_ref(), &caps);
        let transcode = crate::transcode::Transcoder::from_config(
            &config.media.transcode,
            std::env::var("LIUGUANG_TRANSCODE").ok().as_deref(),
            &config.media.transcode_crf_or_bitrate,
            config.media.transcode_max_width,
            startup,
        );
        let mut runtime = Self {
            backend,
            backend_name: backend_name.to_string(),
            backend_available,
            config,
            config_path,
            state_file,
            persona,
            state,
            channels: BTreeMap::new(),
            transitions,
            events,
            started: Instant::now(),
            state_machine_errors: 0,
            last_contradiction: None,
            primed_first_frame: false,
            test_fullscreen_until: None,
            test_fullscreen_applied: false,
            test_gap_applied: false,
            vaapi_verified_node: None,
            media: MediaIndex::default(),
            caps,
            transcode,
            pending_playback: Vec::new(),
            thumbs: crate::thumb::Service::new(),
            last_tick_at: None,
            scan_rx,
            scan_tx,
            next_scan_id: 0,
            pending_applied: Vec::new(),
            pending_sets: Vec::new(),
            last_request_at: None,
            next_request_id: 0,
            added_sources: Vec::new(),
            removed_sources: Vec::new(),
            playlists,
            playlists_loaded,
            sysstat: crate::sysstat::SysStat::default(),
            last_master_poll: Instant::now(),
            master_heal_repaints: 0,
            shed,
            screen,
            user_paused: BTreeSet::new(),
            screen_paused: std::collections::BTreeSet::new(),
            gpu_paused: BTreeSet::new(),
            gpu: crate::gpu_pressure::Controller::new(gpu_above, gpu_below),
            game_paused: BTreeSet::new(),
            game: crate::game_watch::Controller::new(game_enabled, game_extra, game_ignore),
            battery_paused: BTreeSet::new(),
            power: crate::power_watch::PowerWatch::new(power_enabled),
            fullscreen_paused: BTreeSet::new(),
            fullscreen_active: false,
            fullscreen_since: None,
            fullscreen_info: None,
            fullscreen_polled: None,
            fullscreen_verify_due: None,
            fullscreen_suppressed: None,
            preload,
            // 首次显示静态图时会被 `show_image` 按真实输出模式覆盖。
            preload_dims: (2560, 1600),
            preload_stamps: HashMap::new(),
            pending_config: BTreeSet::new(),
            config_write_due: None,
            config_write_warned: false,
        };
        let _ = runtime.init_channels();
        runtime.restore_persisted();
        // 启动时若一块已连接输出都没有，必须**明确**进入 HeadlessReady（规范 §4.3），
        // 而不是停在 InitializingDisplay 等一次永远等不到的 modeset。
        runtime.settle_headless_state();
        Ok(runtime)
    }

    /// 启动/热插拔后的"无输出"状态收敛：没有已连接输出 ⇒ HeadlessReady。
    ///
    /// # 为什么需要（2026-10-28 真机缺陷）
    ///
    /// `Runtime::new` 先驱动 `DisplayInitialized`（Boot → InitializingDisplay）。
    /// 若机器当前没有任何显示器（测试服务器常态），周期复查会报 `OutputLost`，
    /// 而旧状态表只接受 `OutputMissing` ⇒ 状态永久卡在 `InitializingDisplay`，
    /// `status.state` 与"无屏"事实矛盾（规范 §4.3 明确要求 HeadlessReady）。
    ///
    /// 事件选择：有通道已建立（说明曾有输出）用 `OutputLost`；否则先用
    /// `OutputMissing`（Boot/WaitingForSeat 路径），再补 `OutputLost` 兜底。
    fn settle_headless_state(&mut self) {
        if !self.backend_available {
            // 后端不可用的启动路径已在 `Runtime::new` 里驱动过 OutputMissing。
            return;
        }
        let any_connected = self.backend.outputs().iter().any(|o| o.connected);
        if any_connected || self.state.current() == State::HeadlessReady {
            return;
        }
        self.drive(Event::OutputMissing);
        if self.state.current() != State::HeadlessReady {
            self.drive(Event::OutputLost);
        }
        tracing::warn!(
            state = self.state.current().name(),
            outputs = self.backend.outputs().len(),
            "无已连接输出：已进入 HeadlessReady（IPC 保持可用，等待热插拔）"
        );
        self.events.publish(
            EventMessage::new(EventKind::OutputChanged)
                .with_connected(false)
                .with_message("当前无已连接输出（HeadlessReady）"),
        );
    }

    /// 配置引用。
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 顶层状态。
    pub fn state(&self) -> State {
        self.state.current()
    }

    /// 是否缺少可用显示设备。
    pub fn needs_backend(&self) -> bool {
        !self.backend_available
    }

    /// 挂载恢复的显示后端（设备恢复路径）。
    pub fn attach_backend(&mut self, backend: Box<dyn DisplayBackend>, name: &str) {
        self.backend = backend;
        self.backend_name = name.to_string();
        self.backend_available = true;
        tracing::info!(backend = name, "显示设备已恢复");
        // 设备恢复 ≠ 有输出：先看这台机器现在是否真的接着屏，再决定对外发布什么。
        // 没有已连接输出时仍要落 HeadlessReady（规范 §4.3），不能谎报 OutputChanged(true)。
        let any_connected = self.backend.outputs().iter().any(|o| o.connected);
        if any_connected {
            self.drive(Event::OutputConnected);
            self.events.publish(
                EventMessage::new(EventKind::OutputChanged)
                    .with_connected(true)
                    .with_mode(self.config.display.mode.clone())
                    .with_message("显示设备已恢复"),
            );
        } else {
            tracing::warn!(backend = name, "显示设备已恢复，但当前没有任何已连接输出");
        }
        // 新后端可能带着与旧后端不同的 connector 集合：为**已连接**输出补建通道，
        // 并据此纠正"设备恢复但无输出"时不该停在 InitializingDisplay 的问题。
        let created = self.init_channels();
        self.settle_headless_state();
        // 恢复各通道画面。
        let outputs: Vec<(String, CpuFrame)> = self
            .channels
            .iter()
            .filter_map(|(k, c)| c.current.as_ref().map(|d| (k.clone(), d.frame.clone())))
            .collect();
        for (key, frame) in outputs {
            let logged = DisplayFrame::Cpu(frame.clone());
            let res = self.present_frame(&key, frame);
            let _ = self.record_present(&key, res, "后端恢复重放", &logged);
        }
        // 设备恢复时新建的通道没人点过：真正点亮它们（而不是等 5s 后的热插拔复查，
        // 那条路径只在"通道不存在"时才会点亮，此时通道已经存在）。
        for key in created {
            self.light_up_channel(&key);
        }
    }

    /// 为每个**已连接**输出建立通道（M6 多屏）。
    ///
    /// 返回本次新建的通道名：热插拔路径据此只"点亮"刚接上的那几路；启动路径忽略。
    ///
    /// # 只给已连接输出建通道（2026-10-28 真机缺陷）
    ///
    /// 旧实现对枚举到的**全部** connector 建通道（真机是无显示器的 6 个 disconnected
    /// connector），于是启动时逐路尝试解码 + 提交：日志里 6 条 GStreamer 管线在
    /// ~1 秒内为不存在的显示器建立后全部失败。规范 §4.3 要求"检测到热插拔之后再
    /// 完成 modeset"，因此这里以**后端当前连接事实**为准，未连接输出等热插拔。
    fn init_channels(&mut self) -> Vec<String> {
        let outputs = self.backend.outputs();
        if outputs.is_empty() {
            tracing::warn!("当前无输出枚举结果，通道待热插拔后建立");
            return Vec::new();
        }
        let mut created = Vec::new();
        for info in outputs {
            let key = info.id.as_str().to_string();
            if self.channels.contains_key(&key) {
                continue;
            }
            // **以当前连接状态为准**：`outputs()` 里的 `connected` 可能来自打开设备
            // 时的快照，而启动过程中显示器可能刚被拔掉。
            match self.backend.connector_connected(&key) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(output = %key, "输出未连接：暂不建通道（热插拔后建立）");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(output = %key, error = %e, "连接器状态复查失败：暂不建通道");
                    continue;
                }
            }
            if !self.config.outputs.contains_key(&key) {
                // 示例配置只声明 HDMI-A-1、真机只有 DP-1 时，用户的
                // `set --fit/--zoom` 会"看起来成功但没生效"。显式提示，
                // 并说明 IPC 覆盖仍然可用（见 apply_overrides）。
                tracing::warn!(
                    output = %key,
                    "配置未声明 [outputs.\"{key}\"] 段：使用内置默认显示参数（--fit/--zoom 覆盖仍会生效并写入内存配置）"
                );
            }
            let (order, _interval, _dir) = self.channel_settings(&key);
            let interval = self.effective_interval(&key);
            // T68：游标**只**由 `build_playlist_for` 产出 —— 要么播放列表
            // （只有加入的才播），要么目录（旧行为），两者互斥、失败不互相兜底。
            let (playlist, mode, source_label, playlist_error) = match self.build_playlist_for(&key)
            {
                Ok((pl, mode, label)) => (pl, mode, label, None),
                Err(e) => {
                    // 坏播放源：**不**退回目录轮换（否则就是"加了播放列表，
                    // 播的还是目录"），而是留空 + 记录原因，等
                    // `retry_broken_playlists` 定期自愈。
                    let (_, mode, label) = self.intended_source(&key);
                    tracing::warn!(
                        output = %key,
                        source = %label,
                        code = e.detail_code.as_deref().unwrap_or_else(|| e.code.as_str()),
                        error = %e.message,
                        "播放源不可用：该通道当前没有可播内容（不会退回目录轮换）"
                    );
                    (Playlist::empty(order), mode, label, Some(e))
                }
            };
            let now = Instant::now();
            self.channels.insert(
                key.clone(),
                ChannelState {
                    playlist,
                    scheduler: IntervalScheduler::new(interval),
                    paused: false,
                    pinned: false,
                    decode_alert: None,
                    static_wallpaper: None,
                    current: None,
                    video: None,
                    paused_since: None,
                    paused_video_media: None,
                    last_presented: 0,
                    last_dropped_late: 0,
                    last_presented_at: now,
                    stall_reported: false,
                    stall_rebuilds: 0,
                    present_failures: 0,
                    frame_probe: liuguang_display::frame_probe::FrameProbe::from_env(),
                    present_failures_consecutive: 0,
                    presented_ok: 0,
                    drm_commit_failed: 0,
                    drm_failed_consecutive: 0,
                    drm_last_presented: 0,
                    drm_loud_used: 0,
                    drm_loud_total: 0,
                    drm_zero_ok_warned: false,
                    dmabuf_latched: false,
                    dmabuf_rejections: 0,
                    master_unavailable_skips: 0,
                    video_resume_attempts: 0,
                    video_resume_pending: None,
                    degraded_reason: None,
                    source_label,
                    mode,
                    last_video_pts: None,
                    video_loops: 0,
                    playlist_dirty: false,
                    playlist_error,
                    last_playlist_retry: now,
                    last_rotation_error: None,
                    preload_target: None,
                    transition: None,
                    pending_target: None,
                },
            );
            created.push(key);
        }
        created
    }

    /// 让一个**刚建立**的通道真正上屏；状态由 `show_*` 自己驱动。
    ///
    /// **真实点亮，而不是宣布已点亮**：旧热插拔实现无条件 `drive(StaticShown)`，
    /// 但新通道 `current == None`、屏幕上一个像素都没有 ⇒ 状态撒谎成 StaticIdle
    /// （规范 §15.2 的反面）。规则：
    /// - 通道已有画面（后端恢复重放场景）⇒ 重新提交一次；
    /// - 没有画面 ⇒ 走一次媒体切换，**成功才**进入 StaticIdle/VideoPlaying；
    ///   失败保持 InitializingDisplay，并给出可操作日志（不静默、不假装）。
    fn light_up_channel(&mut self, key: &str) {
        if let Some(frame) = self
            .channels
            .get(key)
            .and_then(|ch| ch.current.as_ref().map(|d| d.frame.clone()))
        {
            let logged = DisplayFrame::Cpu(frame.clone());
            let res = self.present_frame(key, frame);
            let _ = self.record_present(key, res, "热插拔重放", &logged);
            return;
        }
        match self.switch_next(key, false) {
            Ok(()) => tracing::info!(
                output = %key,
                media = %self
                    .channels
                    .get(key)
                    .and_then(|c| c.current.as_ref())
                    .map(|d| d.media.display().to_string())
                    .unwrap_or_default(),
                "输出接入后已点亮首个媒体项"
            ),
            Err(e) => tracing::warn!(
                output = %key,
                error = %e.message,
                "输出已接入，但暂无可显示内容（保持 InitializingDisplay，等待指令/轮换）"
            ),
        }
    }

    /// 通道的（顺序、间隔、媒体目录）配置：输出段覆盖全局 slideshow 段。
    /// 该通道**当前生效**的轮换间隔。
    ///
    /// T68：有播放列表时用**列表自己的** `interval_seconds`（用户给这条列表配的节奏），
    /// 没有列表时才回落到 `[slideshow].interval_seconds`（旧行为）。
    ///
    /// 为什么必须区分：真机 E2E 暴露过——列表写 `interval_seconds = 30`，但如果仍按
    /// `[slideshow] = 1s` 推进，用户会看到"每秒换一张"，与配置完全不符。
    fn effective_interval(&self, name: &str) -> Duration {
        if let Some(pl) = self.active_playlist(name) {
            return Duration::from_secs(u64::from(pl.interval_seconds.max(1)));
        }
        self.channel_settings(name).1
    }

    fn channel_settings(&self, name: &str) -> (OrderMode, Duration, PathBuf) {
        let out = self.config.outputs.get(name);
        let dir = out
            .and_then(|o| o.media_directory.clone())
            .unwrap_or_else(|| self.config.slideshow.directory.clone());
        let interval_secs = out
            .and_then(|o| o.interval_seconds)
            .unwrap_or(self.config.slideshow.interval_seconds);
        let order = match out
            .and_then(|o| o.order)
            .unwrap_or(self.config.slideshow.order)
        {
            PlaylistOrder::Sequential => OrderMode::Sequential,
            PlaylistOrder::Shuffle => OrderMode::Shuffle {
                seed: 0x0A17BE4D
                    ^ (name
                        .bytes()
                        .fold(0u64, |a, b| a.rotate_left(8) ^ u64::from(b))),
                avoid_repeat: self.config.slideshow.avoid_repeat,
            },
        };
        (order, Duration::from_secs(interval_secs), dir)
    }

    /// 该通道当前**生效的播放列表**（`None` = 旧行为：目录轮换）。
    ///
    /// 选择规则（确定性，文档化）：`self.playlists` 里**第一条**
    /// `output` 为 `None`（适用于所有输出）或等于该通道名的条目。
    /// 多屏时想让两台屏播不同列表，就给每条列表写 `output`。
    fn active_playlist(&self, key: &str) -> Option<&liuguang_ipc::protocol::Playlist> {
        self.playlists
            .iter()
            .find(|p| p.output.as_deref() == Some(key))
            .or_else(|| self.playlists.iter().find(|p| p.output.is_none()))
    }

    /// 该通道**应当**使用的播放源（标签 + 顺序模式 + 播放模式），不改动通道状态。
    fn intended_source(&self, key: &str) -> (OrderMode, PlaybackMode, String) {
        if let Some(pl) = self.active_playlist(key) {
            let mode = playback_mode_from(&pl.on_video_end, &pl.mode, pl.no_repeat);
            let order = match mode {
                PlaybackMode::Random { no_repeat } => OrderMode::Shuffle {
                    seed: playlist_seed(key, &pl.id),
                    avoid_repeat: no_repeat,
                },
                _ => OrderMode::Sequential,
            };
            (order, mode, format!("playlist:{}", pl.id))
        } else {
            let (order, _, _) = self.channel_settings(key);
            // 目录模式的**旧语义**：视频由生产者自己 `restart_loop`，daemon 不在
            // "播完一轮"时前进 ⇒ 等价于单视频循环（`on_video_end = "loop"`）。
            (order, PlaybackMode::SingleLoop, "directory".to_string())
        }
    }

    /// 把播放列表条目的 `media_id` 解析成**可用**的规范路径。
    ///
    /// 返回 `Err(原因)` ⇒ 该条目当前不可用（调用方跳过并计入 `rejected`）。
    /// 有 `[library]` 时用媒体库边界（与装载路径 [`load_playlists_from_config`]
    /// 同一套规则）；没有 `[library]` 时退回 daemon 通用的媒体路径检查。
    fn resolve_playlist_entry(&self, media_id: &str) -> Result<PathBuf, String> {
        let raw = PathBuf::from(media_id);
        if !raw.is_absolute() {
            return Err("条目必须是绝对路径（媒体库 id）".to_string());
        }
        if let Some(lib) = self.library() {
            return match lib.validate() {
                Ok(media) => media
                    .resolve(media_id)
                    .map_err(|e| format!("{}: {}", e.code.as_str(), e.message)),
                Err((field, e)) => Err(format!("[library] {field}: {}", e.message)),
            };
        }
        self.check_media_path(&raw).map_err(|e| {
            format!(
                "{}: {}",
                e.detail_code.as_deref().unwrap_or_else(|| e.code.as_str()),
                e.message
            )
        })
    }

    /// 按**当前生效的播放源**构建游标（T68 的唯一构建点）。
    ///
    /// - 有播放列表 ⇒ 只用该列表的条目（**只有加入的才播**）；空列表 /
    ///   全部条目不可用 ⇒ `Err`（带稳定机器码），**绝不**退回目录；
    /// - 无播放列表 ⇒ 旧行为：目录轮换（失败时给空游标，与旧实现一致）。
    fn build_playlist_for(&self, key: &str) -> Result<(Playlist, PlaybackMode, String), ErrorBody> {
        if let Some(pl) = self.active_playlist(key) {
            let (order, mode, label) = self.intended_source(key);
            if pl.entries.is_empty() {
                return Err(ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!(
                        "播放列表 {} 没有任何条目：先用 playlist update 把媒体加入该列表，再启用",
                        pl.id
                    ),
                )
                .with_detail_code(PLAYLIST_EMPTY_CODE));
            }
            let mut entries: Vec<MediaEntry> = Vec::with_capacity(pl.entries.len());
            let mut rejected: Vec<String> = Vec::new();
            for e in &pl.entries {
                match self.resolve_playlist_entry(&e.media_id) {
                    Ok(path) => entries.push(MediaEntry {
                        kind: media_kind_for_path(&path),
                        path,
                    }),
                    Err(reason) => rejected.push(format!("{}（{reason}）", e.media_id)),
                }
            }
            if entries.is_empty() {
                return Err(ErrorBody::new(
                    ErrorKind::FileUnavailable,
                    format!(
                        "播放列表 {} 的 {} 个条目全部不可用，没有可播内容：{}",
                        pl.id,
                        pl.entries.len(),
                        rejected.join("; ")
                    ),
                )
                .with_detail_code(PLAYLIST_ALL_INVALID_CODE));
            }
            if !rejected.is_empty() {
                // 部分不可用不是"全坏"：能播的照播，但必须留下证据（不是静默跳过）。
                tracing::warn!(
                    output = %key,
                    playlist = %pl.id,
                    usable = entries.len(),
                    rejected = rejected.len(),
                    first = %rejected[0],
                    "播放列表有不可用条目：跳过，其余照常播放"
                );
            }
            let playlist = Playlist::with_entries(entries, order).map_err(|e| {
                ErrorBody::new(
                    ErrorKind::Internal,
                    format!("播放列表 {} 构建失败: {e}", pl.id),
                )
            })?;
            return Ok((playlist, mode, label));
        }
        // 无播放列表：旧行为不变（目录轮换）。
        let (order, _, dir) = self.channel_settings(key);
        match Playlist::from_dir(&dir, order) {
            Ok(playlist) => Ok((playlist, PlaybackMode::SingleLoop, "directory".to_string())),
            Err(e) => {
                tracing::warn!(output = %key, dir = %dir.display(), error = %e, "媒体目录为空，使用空列表");
                Ok((
                    Playlist::empty(order),
                    PlaybackMode::SingleLoop,
                    "directory".to_string(),
                ))
            }
        }
    }

    /// 重建通道游标（幂等）。失败时**清空游标**并记录原因 —— 绝不保留旧游标，
    /// 否则"旧来源"会继续驱动画面（静默降级）。
    fn rebuild_channel_playlist(&mut self, key: &str) -> Result<(), ErrorBody> {
        match self.build_playlist_for(key) {
            Ok((playlist, mode, label)) => {
                let mut label_changed = false;
                if let Some(ch) = self.channels.get_mut(key) {
                    label_changed = ch.source_label != label;
                    ch.playlist = playlist;
                    ch.mode = mode;
                    ch.source_label = label.clone();
                    ch.playlist_dirty = false;
                    ch.playlist_error = None;
                }
                if label_changed {
                    tracing::info!(
                        output = %key,
                        source = %label,
                        mode = mode.as_str(),
                        advances_on_video_end = mode.advances_on_video_end(),
                        "播放来源已生效"
                    );
                }
                // 生效间隔随来源变化（列表自己的 interval_seconds vs [slideshow]）。
                let interval = self.effective_interval(key);
                if let Some(ch) = self.channels.get_mut(key)
                    && !ch.paused
                {
                    ch.scheduler
                        .set_interval(interval, std::time::Instant::now());
                }
                Ok(())
            }
            Err(e) => {
                let (order, mode, label) = self.intended_source(key);
                let mut was_ok = false;
                if let Some(ch) = self.channels.get_mut(key) {
                    was_ok = ch.playlist_error.is_none();
                    ch.playlist = Playlist::empty(order);
                    ch.mode = mode;
                    ch.source_label = label.clone();
                    ch.playlist_dirty = false;
                    ch.playlist_error = Some(e.clone());
                }
                if was_ok {
                    tracing::warn!(
                        output = %key,
                        source = %label,
                        code = e.detail_code.as_deref().unwrap_or_else(|| e.code.as_str()),
                        error = %e.message,
                        "播放源不可用：清空游标（不退回目录轮换），将定期重试"
                    );
                }
                Err(e)
            }
        }
    }

    /// 播放列表变更后：标记 + **立即重建**受影响通道（T68）。
    ///
    /// 为什么必须"立即"而不是"等下次切换"：`loop` 模式（`SingleLoop`）既不按定时器
    /// 轮换、也不在视频播完时前进 ⇒ **永远不会**触发"下次切换" ⇒ 若只在切换时重建，
    /// 用户从"单视频循环"切到"随机播放"后会**卡在旧模式**上（真机 E2E 抓到）。
    ///
    /// 重建只更新"游标/模式/来源标签/间隔"，**不动当前画面**，所以立即执行是安全的。
    fn apply_playlist_change(&mut self, output: Option<&str>) {
        self.mark_playlist_dirty(output);
        let keys: Vec<String> = self
            .channels
            .iter()
            .filter(|(key, _)| output.is_none() || output == Some(key.as_str()))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Err(e) = self.rebuild_channel_playlist(&key) {
                self.note_rotation_error(&key, &e.message);
            }
        }
    }

    /// 标记受影响通道的游标需要重建（`output = None` ⇒ 所有通道）。
    fn mark_playlist_dirty(&mut self, output: Option<&str>) {
        for (key, ch) in self.channels.iter_mut() {
            if output.is_none() || output == Some(key.as_str()) {
                ch.playlist_dirty = true;
            }
        }
    }

    /// 坏播放源**自动恢复**（限速 [`PLAYLIST_RETRY_INTERVAL`]）。
    ///
    /// 为什么必须有：`switch_next` 只在"调度到期"或"游标为空"时被调用，而游标
    /// 为空时 `tick` 的轮换循环**根本不会**把它排进 `due`（条件含
    /// `!playlist.is_empty()`）⇒ 没有这条重试，文件放回去后要等用户手动 `next`
    /// 或重启才能恢复，就是"静默停摆"。
    fn retry_broken_playlists(&mut self, now: Instant) {
        let keys: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, ch)| {
                ch.playlist_error.is_some()
                    && !ch.paused
                    && now.saturating_duration_since(ch.last_playlist_retry)
                        >= PLAYLIST_RETRY_INTERVAL
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Some(ch) = self.channels.get_mut(&key) {
                ch.last_playlist_retry = now;
            }
            match self.rebuild_channel_playlist(&key) {
                Ok(()) => {
                    tracing::info!(output = %key, "播放源已恢复可播（重建成功）");
                    if let Err(e) = self.switch_next(&key, true) {
                        self.note_rotation_error(&key, &e.message);
                    }
                }
                Err(e) => self.note_rotation_error(&key, &e.message),
            }
        }
    }

    /// 轮换失败必须**可见**，但同一原因不能按 interval（最低 1s）反复刷屏。
    ///
    /// 去重按"原因字符串变化"：故障换了一种原因 ⇒ 立刻再报一次；恢复后再失败
    /// 也会重新报（`show_entry` 成功时清空）。
    fn note_rotation_error(&mut self, key: &str, message: &str) {
        let changed = self
            .channels
            .get(key)
            .map(|ch| ch.last_rotation_error.as_deref() != Some(message))
            .unwrap_or(true);
        if let Some(ch) = self.channels.get_mut(key) {
            ch.last_rotation_error = Some(message.to_string());
        }
        if changed {
            tracing::warn!(
                output = %key,
                error = %message,
                "轮换失败（同一原因只告警一次；成功切换后重新计数）"
            );
        }
    }

    /// 观察视频 PTS，返回"是否刚刚播完一轮"（源回卷）。
    ///
    /// `VideoStats` 没有时长/EOS 计数 ⇒ 唯一可用信号是 PTS **明显倒退**
    /// （生产者 `restart_loop` 后 PTS 跳回 0）。容差见
    /// [`VIDEO_LOOP_PTS_TOLERANCE`]：微小抖动不算播完。
    fn note_video_pts(&mut self, key: &str) -> bool {
        let Some(ch) = self.channels.get_mut(key) else {
            return false;
        };
        let pts = ch.video.as_ref().and_then(|v| v.video_stats().last_pts);
        let Some(pts) = pts else {
            // 源还没给出 PTS（首帧未解码）：不知道就不断言。
            return false;
        };
        let mut ended = false;
        if let Some(prev) = ch.last_video_pts
            && pts.saturating_add(VIDEO_LOOP_PTS_TOLERANCE) < prev
        {
            ended = true;
            ch.video_loops += 1;
        }
        ch.last_video_pts = Some(pts);
        ended
    }

    /// 当前播放源（`"static"` | `"playlist:<id>"` | `"directory"`）。
    ///
    /// 多通道时报告 [`Self::primary_channel`] 那一个：协议里 `playback_source` 是
    /// **顶层**字段，逐通道的值需要协议新增字段（不在 t68 范围内）。
    fn playback_source(&self) -> Option<String> {
        self.primary_channel()
            .map(|(_, ch)| ch.source_label.clone())
    }

    ///
    /// 非法转移会 `warn!` + 计数 + 发 Error 事件（绝不静默吞），调用方据此
    /// 判断"这一步接线是否成立"。
    fn drive(&mut self, event: Event) -> bool {
        let before = self.state.current();
        match self.state.handle(event) {
            Ok(t) => {
                if t.to != before {
                    let ev = EventMessage::new(EventKind::StateChanged).with_state(t.to.name());
                    self.events.publish(ev);
                }
                true
            }
            Err(e) => {
                // **不静默**：非法转移意味着调用方的事件序列与状态机不一致
                // （BRIEF P1-6 / AUDIT §2.3 就是这类"半接线"故障被 debug 埋掉）。
                self.state_machine_errors += 1;
                tracing::warn!(
                    from = before.name(),
                    event = ?event,
                    total = self.state_machine_errors,
                    error = %e,
                    "顶层状态机拒绝非法转移：调用方事件序列需要修正"
                );
                self.events
                    .publish(EventMessage::new(EventKind::Error).with_message(format!(
                        "状态机非法转移 {} + {:?}（累计 {} 次）",
                        before.name(),
                        event,
                        self.state_machine_errors
                    )));
                false
            }
        }
    }

    /// 切换序列：`SwitchRequested` + `NextPrepared`。
    ///
    /// 返回值是"当前是否处于切换流程中"：
    /// - 稳定显示态（StaticIdle/VideoPlaying/DegradedStatic/Paused）→ 发起切换；
    /// - **已经在切换流程中**（PreparingNext/Transitioning）→ 不再重复驱动
    ///   （`Transitioning + SwitchRequested` 是非法转移，真机上一次"转场没结束时
    ///   又来一次 set"就会踩到）→ 直接返回 true；
    /// - 显示尚未初始化/无输出 → 返回 false，调用方直接宣布终态
    ///   （`InitializingDisplay + StaticShown → StaticIdle` 合法，而
    ///   `InitializingDisplay + SwitchRequested` 非法）。
    fn drive_switch(&mut self) -> bool {
        match self.state.current() {
            State::PreparingNext | State::Transitioning => true,
            State::StaticIdle | State::VideoPlaying | State::DegradedStatic | State::Paused => {
                if !self.drive(Event::SwitchRequested) {
                    return false;
                }
                self.drive(Event::NextPrepared);
                true
            }
            _ => false,
        }
    }

    /// 顶层状态机拒绝的非法转移累计次数（诊断/测试用；同时经 WARN 日志与事件暴露）。
    pub fn state_machine_errors(&self) -> u64 {
        self.state_machine_errors
    }

    /// 宣布"该通道新画面已生效"：按媒体类型驱动终态，并在通道仍暂停时回到 `Paused`。
    ///
    /// **只在目标终态与现状不同时才驱动**：已经处于 `StaticIdle`/`VideoPlaying`
    /// 时重复驱动是非法转移（真机上"两次 set 叠加转场"就会踩到），会污染
    /// 非法转移计数器，掩盖真正的接线错误。
    fn drive_shown(&mut self, key: &str) {
        let has_video = self
            .channels
            .get(key)
            .map(|c| c.video.is_some())
            .unwrap_or(false);
        let target = if has_video {
            State::VideoPlaying
        } else {
            State::StaticIdle
        };
        if self.state.current() != target {
            self.drive(if has_video {
                Event::VideoStarted
            } else {
                Event::StaticShown
            });
        }
        self.restore_paused_state();
    }

    /// 暂停是**用户意图**：只要有已显示内容的通道处于暂停，顶层状态就必须是
    /// `Paused`。切换/转场完成后调用它，避免出现"通道暂停但顶层说 StaticIdle"
    /// 这类自相矛盾的 status（AUDIT §2.3 的 R2/R5）。
    fn restore_paused_state(&mut self) {
        let any_paused = self
            .channels
            .values()
            .any(|c| c.paused && c.current.is_some());
        if any_paused && self.state.current() != State::Paused {
            self.drive(Event::Pause);
        }
    }

    /// 顶层状态与通道事实的一致性检查：返回被破坏的不变式（空 = 一致）。
    ///
    /// 这些不变式正是 AUDIT §2.3"三套状态源互相矛盾"的定义：
    /// - I1 有已显示内容的通道处于暂停 ⇒ 顶层必须是 `Paused`；
    /// - I2 顶层是 `Paused` ⇒ 确有通道处于暂停（不能凭空暂停）；
    /// - I3 顶层停在 `PreparingNext`（加载中）⇒ 确有通道在准备/转场，
    ///   否则就是"永久卡在中间态"（R5 的故障）；
    /// - I4 有通道在转场 ⇒ 顶层必须是 `PreparingNext`/`Transitioning`/`Paused`
    ///   （不能声称 StaticIdle 而画面还在动）；
    /// - I5 顶层是 `Transitioning` ⇒ 确有通道在转场。
    pub fn state_contradictions(&self) -> Vec<String> {
        let top = self.state.current();
        let mut out = Vec::new();
        let content: Vec<&ChannelState> = self
            .channels
            .values()
            .filter(|c| c.current.is_some())
            .collect();
        let any_paused = content.iter().any(|c| c.paused);
        let any_transition = self.channels.values().any(|c| c.transition.is_some());
        if any_paused && top != State::Paused {
            out.push(format!("I1 通道已暂停但顶层状态是 {}", top.name()));
        }
        if top == State::Paused && !any_paused {
            out.push("I2 顶层 Paused 但没有任何通道处于暂停".to_string());
        }
        if top == State::PreparingNext && !any_transition {
            out.push("I3 顶层卡在 PreparingNext 但没有任何通道在转场".to_string());
        }
        if any_transition
            && !matches!(
                top,
                State::PreparingNext | State::Transitioning | State::Paused
            )
        {
            out.push(format!("I4 通道正在转场但顶层状态是 {}", top.name()));
        }
        if top == State::Transitioning && !any_transition {
            out.push("I5 顶层 Transitioning 但没有任何通道在转场".to_string());
        }
        out
    }

    /// 通道状态的**唯一**来源（`status.channels[].state` 只从这里取）。
    ///
    /// 优先级即语义优先级：用户暂停 > 转场中 > 视频播放 > 静态显示 > 等待输出。
    fn channel_state_label(ch: &ChannelState) -> &'static str {
        if ch.paused {
            "Paused"
        } else if ch.transition.is_some() {
            "Transitioning"
        } else if ch.video.is_some() {
            "VideoPlaying"
        } else if ch.current.is_some() {
            "StaticIdle"
        } else {
            "WaitingForOutput"
        }
    }

    /// 顶层字段（output/media/mode/decoder/…）所述的**那一个**通道。
    ///
    /// 规则单一且确定：优先取"已有显示内容"的最小键通道；都没有内容时才取最小键
    /// 通道。旧实现直接取 `channels.first()`，在"真机只连 DP-1、但有 6 个通道"时
    /// 会去描述一个没有内容的通道，于是顶层 media/mode/decoder 全是空值。
    fn primary_channel(&self) -> Option<(&String, &ChannelState)> {
        self.channels
            .iter()
            .find(|(_, c)| c.current.is_some())
            .or_else(|| self.channels.iter().next())
    }

    /// 见 [`Self::tick`]：把"有列表但还没显示过内容"的通道立刻点亮。
    ///
    /// 幂等（只做一次）；暂停中的通道不点亮；单通道失败只告警，不影响其它通道。
    ///
    /// **一次性闩只在"真的尝试过第 `for` 循环"之后才置位**：若首次调用时
    /// `self.channels` 还是空的（[`Self::init_channels`] 在"无输出枚举结果"时
    /// 直接返回，通道要等热插拔路径才建立），旧实现会在这里把闩烧掉，于是
    /// "启动首帧补偿"永久失效、首帧只能等**整个** `interval_seconds`
    /// （t14 真机复现：`interval=10` 删状态后首帧 11.7s = 通道建立 ~1.7s + 10s）。
    /// 修正：候选为空则**不置闩**并返回，等下一次 tick 重试（空闲 tick 间隔 1s，
    /// 所以通道一建立就会在 ≤1s 内点亮首帧）。
    fn prime_first_frame(&mut self) {
        if self.primed_first_frame {
            return;
        }
        // **t46**：有用户显式请求在排队时不抢跑。
        //
        // `prime_first_frame` 的意义是"启动后别让桌面空着"；一旦有 `set_wallpaper`
        // 已受理，画面由它自己铺。旧实现会在**首个 tick 同步建管线**，于是：
        // ① 请求在运行时队列里等 prime 建完（真机冷启动首次 set 700–760ms）；
        // ② 若 prime 的默认项恰是用户点的那一项，同一条管线被建两次
        //   （journal 里两条"视频解码管线已建立"）。
        // 不消耗 `primed_first_frame`：pending 处理完（或失败）后下一次 tick 仍会 prime。
        if !self.pending_sets.is_empty() {
            tracing::debug!(
                pending = self.pending_sets.len(),
                "有已受理的 set_wallpaper：跳过启动首帧（由该请求自己铺画面）"
            );
            return;
        }
        // **t46**：刚有客户端请求进来时也跳过——prime 是**同步**建管线（真机 0.6–1s），
        // 一旦在 tick 里跑起来，紧接着到达的 IPC 请求就得排队等它建完
        // （t46 复核实测：冷启动首次 set 回执 713–761ms 正是这段）。
        // 启动后 `PRIME_QUIET` 内没有请求才 prime；有请求说明有人已经在命令画面了。
        if self.started.elapsed() < PRIME_QUIET
            || self
                .last_request_at
                .is_some_and(|at| at.elapsed() < PRIME_QUIET)
        {
            tracing::debug!(
                uptime_ms = self.started.elapsed().as_millis() as u64,
                "启动安静期内：本轮跳过启动首帧（避免同步建管线挡住刚连上来的客户端）"
            );
            return;
        }
        let keys: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, ch)| ch.current.is_none() && !ch.paused && !ch.playlist.is_empty())
            .map(|(key, _)| key.clone())
            .collect();
        if keys.is_empty() {
            // 没有"可点亮的通道"：不消耗这一次补偿机会（否则永久失效）。
            return;
        }
        // 有候选即置位（**不要求本次 switch_next 成功**）：候选非空说明"通道+播放列表已就绪"，
        // 之后由调度轮换/显式指令兜底；若改成"成功才置位"，列表里存在不可读文件时会每秒重试刷 WARN。
        self.primed_first_frame = true;
        for key in keys {
            match self.switch_next(&key, false) {
                Ok(()) => tracing::info!(
                    output = %key,
                    "启动首帧：立即显示第一个媒体项（不等 interval）"
                ),
                Err(e) => tracing::warn!(
                    output = %key,
                    error = %e.message,
                    "启动首帧显示失败（等待后续轮换/指令）"
                ),
            }
        }
    }

    /// 周期维护：调度轮换、视频帧推进、转场步进、连接器复查（无空转渲染）。
    ///
    /// 返回**下一次必须唤醒的绝对时刻**：由各通道给出的精确期限与
    /// `now + IDLE_WAIT` 兜底取最早者。主循环据此休眠——休眠期间到达的 IPC
    /// 请求会立即打断休眠并先被处理，所以 IPC 延迟与渲染节拍互不拖累。
    /// 周期尝试取回 DRM master，并在成功后**重放画面**（t26）。
    ///
    /// # 为什么必须"与是否在提交解耦"（根因，真机确证）
    ///
    /// 旧实现里自愈**只由"某一帧提交返回 EACCES"驱动**。在 `DegradedStatic`
    /// 状态下 daemon 不提交任何帧 ⇒ 永远等不到这个触发条件 ⇒ 占用者退出后
    /// 屏幕永远不恢复。真机实测：该状态下 **15 秒内 0 次自愈尝试**
    /// （grep 日志里"重新打开 DRM 设备"计数 = 0）。
    ///
    /// 这也解释了两份看似矛盾的报告：**t14 的"70s 无自愈"** 发生在
    /// `DegradedStatic`（不提交）；**t13 的"35s 自愈"** 发生在**正在播放**
    /// （每帧提交失败都在触发自愈）。两者条件不同，**都不矛盾**。
    ///
    /// # 为什么成功后要重放
    ///
    /// [`DisplayBackend::poll_master_recovery`] 返回 `true` 表示它**重新打开了
    /// 设备**：扫描输出池已释放、crtc/平面绑定已解绑、`committed_size` 已清空。
    /// 此时若不投递一帧，屏幕上仍是旧内容——正是 t14 观察到的
    /// "master 取回了 / `presented` 在涨，但 CRC 不变"。因此这里对**每个有当前
    /// 画面且没有视频源**的通道立刻重放一次（视频通道由帧泵自然续上，不需要重放）。
    fn poll_master_self_heal(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_master_poll) < MASTER_POLL_INTERVAL {
            return false;
        }
        self.last_master_poll = now;
        if !self.backend.poll_master_recovery() {
            return false;
        }
        // 后端已重建资源：把每个静态通道的当前画面重新提交一次。
        let frames: Vec<(String, CpuFrame)> = self
            .channels
            .iter()
            .filter(|(_, ch)| ch.video.is_none())
            .filter_map(|(k, ch)| ch.current.as_ref().map(|d| (k.clone(), d.frame.clone())))
            .collect();
        for (key, frame) in frames {
            let logged = DisplayFrame::Cpu(frame.clone());
            let res = self.present_frame(&key, frame);
            match self.record_present(&key, res, "master 自愈后重放", &logged) {
                // `None` = 本次提交没有触发收敛（正常/已记账）。
                None => self.master_heal_repaints = self.master_heal_repaints.saturating_add(1),
                Some(reason) => tracing::warn!(
                    output = %key,
                    reason,
                    "master 自愈后重放仍失败（画面可能仍不更新）"
                ),
            }
        }
        // **视频通道**：自愈重建了扫描输出资源，但视频供给可能已在降级时被拆掉
        // （`handle_video_failure`）⇒ 必须**重建并恢复**，否则屏幕永久停在一帧
        // 静态图上，用户看到的是"卡住"（t94 真机缺陷：旧实现只重放静态）。
        //
        // 只恢复"因 master 不可用而中断"的通道（`video_resume_pending` 只在
        // master 丢失时置位）——**真帧错误仍然降级、不自动重开**，避免掩盖问题。
        //
        // **必须清掉 DMABuf 闩锁**（2026-10-21 真机 E2E 发现）：master 丢失期间
        // 每帧提交都被显示层拒绝，`latch_dmabuf_rejection` 会把它当成"这块缓冲不被
        // 接受"而**永久闩死零拷贝路径**。于是 master 恢复后视频确实回来了，却停在
        // CPU 拷贝上 —— 实测 4K60 从 6.6% 涨到 **98.6%**，且不会自行恢复。
        // master 丢失是**环境故障**，不构成"该 modifier/格式不被支持"的证据 ⇒
        // 自愈时清闩重试。若真的不支持，重试会在一次失败的协商后重新闩上（代价很小）。
        // 范围是**所有**通道，不只是 video_resume_pending 的那些：
        // "启动时就没有 master"的场景下 video_resume_pending 从没被置位，但闩锁
        // 同样被 master 期间的提交失败置上了 —— 只清待恢复通道会漏掉这种情况。
        for ch in self.channels.values_mut() {
            ch.dmabuf_latched = false;
        }
        let resume: Vec<(String, PathBuf, bool)> = self
            .channels
            .iter()
            .filter(|(_, ch)| ch.video.is_none())
            .filter_map(|(k, ch)| {
                ch.video_resume_pending
                    .as_ref()
                    .map(|p| (k.clone(), p.clone(), ch.dmabuf_latched))
            })
            .collect();
        let mut resumed = 0_u64;
        for (key, path, latched) in resume {
            if let Some(ch) = self.channels.get_mut(&key) {
                ch.video_resume_attempts = ch.video_resume_attempts.saturating_add(1);
            }
            match self.show_video_with(&key, &path, latched) {
                Ok(()) => {
                    resumed = resumed.saturating_add(1);
                    if let Some(ch) = self.channels.get_mut(&key) {
                        ch.video_resume_pending = None;
                    }
                    tracing::info!(
                        output = %key,
                        media = %path.display(),
                        state = self.state.current().name(),
                        "DRM master 已自愈：**已重建并恢复视频供给**"
                    );
                }
                Err(e) => {
                    // 保留 `video_resume_pending`：下一次自愈/轮换再试。
                    tracing::warn!(
                        output = %key,
                        media = %path.display(),
                        error = %e.message,
                        "DRM master 已自愈，但恢复视频供给失败（保留待恢复标记，下次自愈再试）"
                    );
                }
            }
        }
        // **（2026-10-21 真机 E2E 补）把因环境失败退到 CPU 拷贝的通道升级回零拷贝。**
        //
        // 场景：daemon 启动时 master 被别的进程占着 ⇒ DMABuf 协商必然失败 ⇒ 回退到
        // NV12 CPU 拷贝。此时 `video_resume_pending` 从未置位（那条路只在"正在播时
        // 丢 master"才走），于是 master 恢复后**没有任何代码**会重新尝试零拷贝 ——
        // 实测 4K60 停在 **98.6% CPU**（dmabuf 只要 6.6%），且永不自行恢复。
        //
        // 判据：通道正在播 + 生产者不是 DMA-BUF + 没有被**真实**拒绝过（闩锁）。
        // 若确实不支持，重试会在一次失败协商后重新闩上，代价很小。
        let upgrade: Vec<(String, PathBuf)> = self
            .channels
            .iter()
            .filter(|(_, ch)| !ch.dmabuf_latched)
            .filter_map(|(k, ch)| {
                let video = ch.video.as_ref()?;
                // 已经在零拷贝路径上 ⇒ 无需升级。
                if video.dmabuf_state().map(|s| s.is_dmabuf).unwrap_or(false) {
                    return None;
                }
                let path = ch.current.as_ref()?.media.clone();
                Some((k.clone(), path))
            })
            .collect();
        let mut upgraded = 0_u64;
        for (key, path) in upgrade {
            match self.show_video_with(&key, &path, false) {
                Ok(()) => {
                    upgraded = upgraded.saturating_add(1);
                    tracing::info!(
                        output = %key,
                        media = %path.display(),
                        "DRM master 已自愈：已把 CPU 拷贝路径升级回 DMA-BUF 零拷贝"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        output = %key,
                        error = %e.message,
                        "master 自愈后尝试恢复零拷贝失败（保持 CPU 路径，下次再试）"
                    );
                }
            }
        }
        let state = self.state.current().name().to_string();
        tracing::warn!(
            state = %state,
            repainted = self.master_heal_repaints,
            resumed_videos = resumed,
            upgraded_to_dmabuf = upgraded,
            "DRM master 已自愈（资源已重建）：已重放各静态通道当前画面并恢复待恢复的视频"
        );
        self.events.publish(
            EventMessage::new(EventKind::StateChanged)
                .with_state(state)
                .with_message("DRM master 已恢复（资源已重建并重放画面）"),
        );
        true
    }

    pub fn tick(&mut self, now: Instant) -> Instant {
        // **t39**：长间隔不是"视频停摆"。
        //
        // 休眠 / `kill -STOP` / 调试器暂停之后，主循环的下一次 tick 会看到
        // `now - last_presented_at` 巨大；旧实现据此判"停摆"，于是 CONT 的瞬间
        // 重建解码管线（真机：`kill -STOP` 12s 后出现一次无谓
        // 「视频停摆：已自动重建解码管线 attempt=1」，画面白闪）。
        // 间隔超过 [`TICK_GAP_RESET`] 只可能是"进程没在跑"，把看门狗基线整体
        // 挪到 `now`（等价于丢掉这段停顿），并让本 tick 的停摆判据从干净状态起算。
        // t43 验收④：可注入的长间隔（模拟休眠/STOP；只消费一次）。
        if !self.test_gap_applied {
            if let Some(gap_ms) = env_millis(TEST_TICK_GAP_MS) {
                self.test_gap_applied = true;
                self.last_tick_at = Some(now - Duration::from_millis(gap_ms));
                tracing::info!(gap_ms, "（测试钩子）注入长 tick 间隔");
            }
        }
        if let Some(previous) = self.last_tick_at {
            let gap = now.saturating_duration_since(previous);
            if gap > TICK_GAP_RESET {
                for (key, ch) in &mut self.channels {
                    if ch.current.is_some() || ch.video.is_some() {
                        ch.reset_stall_watchdog(now);
                        tracing::debug!(
                            output = %key,
                            gap_ms = gap.as_millis(),
                            "tick 间隔异常（休眠/STOP/调试暂停）：已重置停摆看门狗基线"
                        );
                    }
                }
            }
        }
        self.last_tick_at = Some(now);
        // 0. 后台扫描结果 + 待发的 WALLPAPER_APPLIED（都是非阻塞 O(1) 搬运）。
        self.poll_scan();
        // **t15**：后台转码进度/完成（完成即自动播放，见 `poll_transcode`）。
        self.poll_transcode(now);
        // 先消费已受理的 set（建管线），再发它们的完成事件：一次 tick 内
        // 就能做到"受理响应已回、WALLPAPER_APPLIED 紧接着到"。
        self.run_pending_sets();
        self.flush_pending_applied();
        // 0.1 DRM master 自愈：**与"是否在提交"解耦**（t26 根因，见方法文档）。
        self.poll_master_self_heal(now);
        // 启动首帧补偿：**立即**显示第一个媒体项，不等 `interval_seconds`。
        // 旧行为只在"定时轮换到期"时才显示首张（默认 600s ⇒ 开机后 10 分钟
        // 才有壁纸）。只做一次，且只对"有播放列表但还没内容"的通道。
        self.prime_first_frame();
        let mut plan = WakePlan::default();
        // 0. CPU 压力降载（`performance.reduce_load_on_cpu_pressure`，§11.2 第一行）：
        //    节流采样（5s），档位变化才下发新的提交上限。
        // 到期就把用户改过的偏好写回 `config.toml`（防抖 800ms，见 config_store）。
        self.flush_config_write(now);
        if let Some((old, new)) = self.shed.refresh(now) {
            self.apply_load_level(new);
            tracing::info!(
                from = ?old,
                to = ?new,
                reading = ?self.shed.reading(),
                fps_cap = crate::loadshed::fps_cap(self.config.media.video_fps_limit, new),
                "CPU 压力档位变化：已调整视频提交频率上限并（降载时）暂停预加载"
            );
        }
        // 0.1 熄屏暂停（`performance.pause_when_display_off`，§11.2）：
        //     读到熄屏就停掉视频（停拉帧 ⇒ CPU 归零），亮回来再恢复。
        //     **只恢复自己暂停的通道**，不碰用户手动暂停的。
        if let Some(state) = self.screen.refresh(now) {
            self.apply_screen_state(state);
        }
        // 0.15 显存压力让路（`performance.pause_when_gpu_memory_above`）：
        //      denial 不提供窗口全屏状态，显存占用是唯一可靠的"游戏正在占 GPU"信号；
        //      退避后恢复只碰自己暂停的通道。
        self.poll_gpu_pressure(now);
        // 0.17 游戏进程让路（`performance.detect_games`）：denial 拿不到全屏状态，
        //      从进程表判断"游戏在跑"；滞回 2 命中/3 未命中，避免启动器抖动。
        self.poll_game_running(now);
        // 0.18 电池供电省电暂停（`performance.pause_on_battery`）：笔记本拔电后自动暂停视频。
        self.poll_power_supply(now);
        // 0.2 全屏暂停（`performance.pause_on_fullscreen`）：游戏/播放器独占全屏时
        //     把正在播的视频停掉（给游戏让出 CPU/GPU），退出后只恢复自己停的。
        //     采样与滞回都在方法内（500ms 采一次、状态要持续 1s 才生效）。
        self.poll_fullscreen(now);
        // 0.5 预加载下一张（`performance.preload_next`）：一个 tick 最多一张。
        self.run_pending_preloads();
        // 1. 转场/视频推进（按帧到期时刻精确唤醒）。
        let keys: Vec<String> = self.channels.keys().cloned().collect();
        for key in keys {
            plan.note(self.step_channel(&key, now));
        }
        // 1.5 坏播放源自愈（T68；限速）：文件回来/媒体库就绪后自动恢复，
        //     而不是让通道静默停摆到用户重启。
        self.retry_broken_playlists(now);
        // 2. 调度轮换。
        let mut due = Vec::new();
        for (key, ch) in self.channels.iter_mut() {
            // **T68**：播放列表 + `SingleLoop`（`on_video_end = "loop"`）= 同一条反复播、
            // **不推进** ⇒ 定时器不得换片（否则真机上"单视频循环"会变成"每秒换一个"）。
            //
            // 边界：只有**播放列表驱动**时才这样；旧的目录轮换（无列表）必须保持原样
            // —— 那里 `SingleLoop` 只表示"视频播完由生产者自己 loop"，图片仍按
            // interval 轮换（既有行为，不能因为本卡而退化）。
            let playlist_driven = ch.source_label.starts_with("playlist:");
            if playlist_driven && ch.mode == PlaybackMode::SingleLoop {
                continue;
            }
            // 「只放这一项」：图片常驻 / 视频单曲循环，定时器一律不得换片。
            if ch.pinned {
                continue;
            }
            if !ch.paused && !ch.playlist.is_empty() && ch.scheduler.poll(now) {
                due.push(key.clone());
            }
        }
        for key in due {
            tracing::debug!(output = %key, "定时轮换");
            if let Err(e) = self.switch_next(&key, true) {
                self.note_rotation_error(&key, &e.message);
            }
        }
        // 3. 顶层状态与通道事实的一致性守卫：不变式被破坏说明有"半接线"，
        //    必须可见（只在矛盾集合变化时告警，避免刷屏）。
        let contradictions = self.state_contradictions();
        let signature = if contradictions.is_empty() {
            None
        } else {
            Some(contradictions.join("; "))
        };
        if signature != self.last_contradiction {
            if let Some(sig) = &signature {
                tracing::warn!(contradiction = %sig, "status 顶层状态与通道事实不一致");
            }
            self.last_contradiction = signature;
        }
        // 4. 轮换调度以 1s 粒度检查（间隔以秒/分钟计，精度足够）。
        // 例外：后台扫描在跑时必须**更频繁**回来收消息（否则 SCAN_FINISHED
        // 会被 1s 空闲兜底拖住，真机实测事件延迟近 1s）。
        let idle = if self.media.running.is_some() {
            now + SCAN_POLL_INTERVAL
        } else {
            now + IDLE_WAIT
        };
        plan.deadline(idle)
    }

    /// 连接器插拔处理（每 5 次 tick 调用一次）。
    pub fn check_connectors(&mut self) {
        if !self.backend_available {
            return;
        }
        let outputs = self.backend.outputs();
        for info in &outputs {
            let key = info.id.as_str().to_string();
            match self.backend.connector_connected(&key) {
                Ok(true) => {
                    let mut just_created = false;
                    if !self.channels.contains_key(&key) {
                        tracing::info!(output = %key, "输出接入，建立通道");
                        let mode = info
                            .current_mode
                            .map(|m| format!("{}x{}@{:.3}", m.width, m.height, m.refresh_hz()));
                        let mut event = EventMessage::new(EventKind::OutputChanged)
                            .with_output(key.clone())
                            .with_connected(true)
                            .with_message("connected");
                        if let Some(mode) = mode {
                            event = event.with_mode(mode);
                        }
                        self.events.publish(event);
                        just_created = self.init_channels().iter().any(|k| k == &key);
                    }
                    if self.state.current() == State::HeadlessReady {
                        self.drive(Event::OutputConnected);
                    }
                    if just_created {
                        self.light_up_channel(&key);
                    }
                }
                Ok(false) => {
                    if self.channels.contains_key(&key) {
                        tracing::warn!(output = %key, "显示器断开");
                        self.channels.remove(&key);
                        self.user_paused.remove(&key);
                        self.fullscreen_paused.remove(&key);
                        self.screen_paused.remove(&key);
                        self.game_paused.remove(&key);
                        self.gpu_paused.remove(&key);
                        self.battery_paused.remove(&key);
                        self.pending_sets.retain(|p| p.output != key);
                        self.events.publish(
                            EventMessage::new(EventKind::OutputChanged)
                                .with_output(key)
                                .with_connected(false)
                                .with_message("disconnected"),
                        );
                        if self.channels.is_empty() {
                            // §11.2「HDMI/DP 拔除 ⇒ 关闭对应输出流与缓存」：
                            // 最后一块屏没了 ⇒ 缓存里的解码帧已无去处，释放 RSS。
                            // **部分**拔除不清（同一张图可能正被另一路用着），
                            // 那条通道自己的预加载目标随通道一起消失。
                            self.preload.clear();
                            self.preload_stamps.clear();
                            self.drive(Event::OutputLost);
                        } else {
                            self.sync_top_level_pause_state();
                        }
                    }
                }
                Err(e) => tracing::warn!(output = %key, error = %e, "连接器状态复查失败"),
            }
        }
    }

    /// 把当前 CPU 压力档落到每一路在播视频上（`reduce_load_on_cpu_pressure` 的执行机构）。
    /// 熄屏/亮屏切换：暂停或恢复**因熄屏而**暂停的视频通道。
    ///
    /// 与 `handle_pause`/`handle_resume` 用同一组原语（`ch.paused`、`scheduler`、
    /// `video.pause/resume`），但**只碰自己暂停过的东西**：
    /// - 熄屏时已有用户手动暂停 ⇒ 不会重复 `drive(Pause)`（避免"非法转移"噪声）；
    /// - 亮屏时只恢复 `screen_paused` 里的通道 ⇒ 用户手动暂停的**保持暂停**。
    ///
    /// 暂停的收益是**真正的零成本**：`video.pause()` 让生产者停止拉帧 ⇒ 解码器
    /// 不再工作（`video.rs` §暂停语义），而不是"继续解码再丢掉"。
    fn apply_screen_state(&mut self, state: crate::display_power::ScreenState) {
        use crate::display_power::ScreenState;
        match state {
            ScreenState::Off => {
                let keys = self.pause_channels_for_reason(PauseReason::ScreenOff);
                if keys.is_empty() {
                    // 没有在播的视频（静态图不耗 CPU）⇒ 无事可做。
                    return;
                }
                tracing::info!(
                    paused = keys.len(),
                    outputs = ?keys,
                    polls = self.screen.counters().0,
                    transitions = self.screen.counters().1,
                    "屏幕已熄灭：暂停视频供给（停拉帧 ⇒ CPU 归零；亮屏后自动恢复）"
                );
            }
            ScreenState::On => {
                let keys = self.resume_channels_for_reason(PauseReason::ScreenOff, Instant::now());
                if !keys.is_empty() {
                    tracing::info!(resumed = keys.len(), "屏幕已点亮：恢复视频供给");
                }
            }
        }
    }

    fn apply_load_level(&mut self, level: crate::loadshed::LoadLevel) {
        let cap = crate::loadshed::fps_cap(self.config.media.video_fps_limit, level);
        for (key, ch) in self.channels.iter_mut() {
            if let Some(video) = ch.video.as_mut() {
                video.set_fps_cap(Some(cap));
                tracing::debug!(output = %key, fps_cap = cap, "已下发视频提交频率上限");
            }
        }
    }

    /// 把"本配置要求了、但当前不会发生"的字段大声写进日志（启动与 `reload` 各一次）。
    ///
    /// 缺口清单能被程序读出但没有眼睛会去读程序：不打这条，`[未生效]` 就只是注释，
    /// 用户照样会以为改了配置就真有效果（审计 §3.4 的静默失效面）。
    pub fn log_pending_config(&self, when: &str) {
        let pending = self.config.pending_implementation();
        if pending.is_empty() {
            tracing::debug!(when, "配置项全部有实现（本文件无静默失效字段）");
            return;
        }
        tracing::warn!(
            when,
            count = pending.len(),
            options = %pending.join(", "),
            "以下配置项**当前不会改变运行期行为**（可解析、校验通过，但没有实现）"
        );
    }

    /// 配置换过后同步性能项（`reload` 与配置写回路径都要调用）。
    ///
    /// 不同步的话，"改了 `[performance]` 再 reload"会变成**看起来生效、实际没变**：
    /// 降载开关还留在旧档位、缓存还按旧预算淘汰。
    fn sync_performance(&mut self) {
        self.shed
            .set_enabled(self.config.performance.reduce_load_on_cpu_pressure);
        let budget = preload_budget_bytes(&self.config);
        if self.preload.max_bytes() != budget || !self.config.performance.preload_next {
            // 预算变了就重建缓存：丢掉的只是"已经解码好的下一张"，下一轮照常同步解码；
            // 留着超预算的帧才是把 `max_cache_mb` 当摆设。
            self.preload = liuguang_media::prepared::PreloadCache::new(budget);
            self.preload_stamps.clear();
        }
        if !self.config.performance.preload_next {
            for ch in self.channels.values_mut() {
                ch.preload_target = None;
            }
        }
        let now = Instant::now();
        self.screen
            .set_enabled(self.config.performance.pause_when_display_off);
        if !self.config.performance.pause_when_display_off && !self.screen_paused.is_empty() {
            let _ = self.resume_channels_for_reason(PauseReason::ScreenOff, now);
        }
        if !self.config.performance.pause_on_fullscreen
            && (self.fullscreen_active || !self.fullscreen_paused.is_empty())
        {
            self.fullscreen_active = false;
            self.fullscreen_since = None;
            self.fullscreen_info = None;
            self.fullscreen_verify_due = None;
            self.resume_after_fullscreen();
        }
        let gpu_above = self.config.performance.pause_when_gpu_memory_above;
        let gpu_below = self.config.performance.resume_when_gpu_memory_below;
        let gpu_disabled = gpu_above == 0 || gpu_above <= gpu_below;
        match self
            .gpu
            .reconfigure(f32::from(gpu_above), f32::from(gpu_below))
        {
            crate::gpu_pressure::Action::Resume => {
                let _ = self.resume_channels_for_reason(PauseReason::GpuMemory, now);
            }
            crate::gpu_pressure::Action::Pause => {
                let _ = self.pause_channels_for_reason(PauseReason::GpuMemory);
            }
            crate::gpu_pressure::Action::Unchanged
                if gpu_disabled && !self.gpu_paused.is_empty() =>
            {
                let _ = self.resume_channels_for_reason(PauseReason::GpuMemory, now);
            }
            crate::gpu_pressure::Action::Unchanged => {}
        }
        if self.game.reconfigure(
            self.config.performance.detect_games,
            self.config.performance.game_processes.clone(),
            self.config.performance.game_ignore_processes.clone(),
        ) == crate::game_watch::Action::Resume
            || (!self.config.performance.detect_games && !self.game_paused.is_empty())
        {
            let _ = self.resume_channels_for_reason(PauseReason::GameRunning, now);
        }
        if self
            .power
            .reconfigure(self.config.performance.pause_on_battery)
            == crate::power_watch::Action::Resume
            || (!self.config.performance.pause_on_battery && !self.battery_paused.is_empty())
        {
            let _ = self.resume_channels_for_reason(PauseReason::Battery, now);
        }
        let level = if self.shed.is_enabled() {
            self.shed.level()
        } else {
            crate::loadshed::LoadLevel::Normal
        };
        self.apply_load_level(level);
    }

    /// 记下"这条通道接下来该预加载哪张"（`performance.preload_next`）。
    ///
    /// 只预加载**图片**：缓存存的是解码后的 CPU 帧；"预加载视频"意味着提前开
    /// 一条解码管线（另一回事、另一份成本），不在本项语义内。
    fn schedule_next_preload(&mut self, key: &str) {
        if !self.config.performance.preload_next {
            return;
        }
        let target = self
            .channels
            .get(key)
            .filter(|ch| !ch.pinned)
            .and_then(|ch| ch.playlist.peek())
            .filter(|entry| entry.kind == MediaKind::Image)
            .map(|entry| entry.path.clone());
        if let Some(path) = target
            && let Some(ch) = self.channels.get_mut(key)
        {
            ch.preload_target = Some(path);
        }
    }

    /// 当所有输出通道都锁定单项播放时，不再需要预解码下一张，释放既有缓存与堆页。
    fn clear_preload_if_all_pinned(&mut self) {
        if self.channels.is_empty() || !self.channels.values().all(|ch| ch.pinned) {
            return;
        }
        for ch in self.channels.values_mut() {
            ch.preload_target = None;
        }
        if !self.preload.is_empty() || !self.preload_stamps.is_empty() {
            self.preload.clear();
            self.preload_stamps.clear();
            crate::thumb::trim_heap();
            tracing::debug!("所有输出已锁定单项播放，已清空预解码缓存");
        }
    }

    /// 执行**至多一张**待处理的预加载。
    ///
    /// 一次同步解码上限为 1：4K PNG 解码可达数百 ms，全量补齐会把 IPC 请求
    /// 堵在身后（§11.2「服务器服务优先」）。剩余目标留在各自通道里，逐 tick 推进。
    fn run_pending_preloads(&mut self) {
        if !self.config.performance.preload_next {
            return;
        }
        self.clear_preload_if_all_pinned();
        // §11.2「CPU 或 I/O 压力升高 ⇒ 暂停下一壁纸预加载」：目标保留，档位回落后再做。
        if self.shed.level() == crate::loadshed::LoadLevel::Shed {
            return;
        }
        // 后台媒体扫描在跑时不抢 CPU/IO（扫描本身就是 I/O 压力）。
        if self.media.running.is_some() {
            return;
        }
        let mut picked: Option<(String, PathBuf)> = None;
        for (key, ch) in self.channels.iter_mut() {
            let Some(target) = ch.preload_target.clone() else {
                continue;
            };
            if self.preload.peek(&target) {
                ch.preload_target = None;
                continue;
            }
            picked = Some((key.clone(), target));
            break;
        }
        let Some((key, path)) = picked else {
            return;
        };
        if let Some(ch) = self.channels.get_mut(&key) {
            ch.preload_target = None;
        }
        // 取不到凭据（无法 stat）就**不预加载**：没有凭据的缓存帧在显示时无法判断
        // 是否已被替换，宁可在展示时刻做一次同步解码。
        let Some(stamp) = file_stamp(&path) else {
            tracing::debug!(media = %path.display(), "素材无法 stat：跳过预加载");
            return;
        };
        let dims = self.preload_dims;
        match self.preload.preload_sized(&path, dims.0, dims.1) {
            Ok(()) => {
                self.preload_stamps.insert(path.clone(), stamp);
                tracing::debug!(
                    output = %key,
                    media = %path.display(),
                    cache_bytes = self.preload.bytes(),
                    cache_items = self.preload.len(),
                    "下一张已预加载"
                );
            }
            Err(e) => tracing::warn!(
                output = %key,
                media = %path.display(),
                error = %e,
                max_cache_mb = self.config.performance.max_cache_mb,
                "预加载失败：该图仍会在展示时刻同步解码（不影响显示），但永不命中缓存"
            ),
        }
        // 记账对齐：LRU 淘汰掉的条目，其文件凭据必须一起清 —— 否则每张走过的图
        // 都留一条凭据，`preload_stamps` 只增不减（这才是真的内存泄漏）。
        self.preload_stamps.retain(|p, _| self.preload.peek(p));
    }

    /// 取用预加载帧：**只有文件凭据一致**才算命中。
    ///
    /// 预加载与展示之间默认隔着 600s，素材完全可能在中间被换掉。
    /// 凭据不符时丢掉缓存帧、返回 `None`（调用方同步解码），并大声 debug。
    fn take_preloaded(&mut self, path: &Path) -> Option<CpuFrame> {
        let stamp = self.preload_stamps.remove(path)?;
        let frame = self.preload.take(path)?;
        if file_stamp(path) != Some(stamp) {
            tracing::debug!(media = %path.display(), "素材已被替换：丢弃预加载帧，改用同步解码");
            return None;
        }
        Some(frame)
    }

    /// 推进单通道；返回该通道下一次需要被唤醒的**绝对**时刻
    /// （`None` = 该通道无待办：无转场、无视频或视频已暂停）。
    fn step_channel(&mut self, key: &str, now: Instant) -> Option<Instant> {
        // 转场推进。
        let done_frame = {
            let ch = self.channels.get_mut(key)?;
            if let Some(active) = &mut ch.transition {
                let elapsed = now.duration_since(active.started);
                let progress = if active.duration.is_zero() {
                    1.0
                } else {
                    (elapsed.as_secs_f32() / active.duration.as_secs_f32()).clamp(0.0, 1.0)
                };
                match active.anim.render_at(progress) {
                    Ok(frame) => {
                        if progress >= 1.0 {
                            Some(frame)
                        } else {
                            let f = frame;
                            let logged = DisplayFrame::Cpu(f.clone());
                            let res = self.present_frame(key, f);
                            let _ = self.record_present(key, res, "转场帧", &logged);
                            return Some(Instant::now() + TRANSITION_FRAME_INTERVAL);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(output = %key, error = %e, "转场渲染失败，回退立即切换");
                        Some(ch.pending_target.take().unwrap_or_else(|| {
                            ch.current
                                .as_ref()
                                .map(|d| d.frame.clone())
                                .unwrap_or_else(|| CpuFrame {
                                    width: 1,
                                    height: 1,
                                    stride: 4,
                                    format: liuguang_core::types::PixelFormat::Xrgb8888,
                                    data: Arc::from(vec![0u8; 4].into_boxed_slice()),
                                })
                        }))
                    }
                }
            } else {
                None
            }
        };
        if let Some(frame) = done_frame {
            if let Some(ch) = self.channels.get_mut(key) {
                ch.transition = None;
                ch.pending_target = None;
            }
            let logged = DisplayFrame::Cpu(frame.clone());
            let res = self.present_frame(key, frame);
            let _ = self.record_present(key, res, "转场结束帧", &logged);
            // 转场真正结束：这里才宣布"新画面已生效"，顶层状态收敛到稳定态
            // （而不是在 show_image 里提前宣布，导致 status 说 StaticIdle 而画面还在动）。
            self.drive_shown(key);
        }

        // 视频帧推进：期限由媒体层给出（`poll_video`），必须原样向上传播。
        let (video_frame, video_next, video_error) = {
            let ch = self.channels.get_mut(key)?;
            if ch.paused {
                // 暂停：停止拉帧与唤醒（静态/暂停待机必须 ~0% CPU）。
                //
                // **同时把停摆看门狗基线推进到 `now`**（2026-11-22）：暂停期间
                // 本来就不会有 `presented` 增长，基线不能停在暂停开始那一刻——
                // 否则恢复后的第一个 tick 会拿**整段暂停时长**去比 10s 阈值，
                // 在恢复的第一帧都还没到期时就判停摆、拆掉管线（真机缺陷：
                // `pause` 24.2s 后 `resume` 立即报"停摆：presented 已有 24.2s
                // 零增长"，画面永久不动，只能重启服务）。
                // 暂停不计入停摆：恢复后重新给满一个阈值窗口。
                ch.last_presented_at = now;
                ch.stall_reported = false;
                let started = *ch.paused_since.get_or_insert(now);
                let unloadable_media = ch.current.as_ref().and_then(|c| {
                    (media_kind_for_path(&c.media) == MediaKind::Video && c.media.exists())
                        .then(|| c.media.clone())
                });
                if ch.video.is_some() {
                    if let Some(media) = unloadable_media {
                        let elapsed = now.saturating_duration_since(started);
                        if elapsed >= PAUSE_UNLOAD_DELAY {
                            ch.video = None;
                            ch.paused_video_media = Some(media);
                            tracing::info!(
                                output = %key,
                                paused_secs = elapsed.as_secs(),
                                "视频暂停超过阈值，已卸载解码与渲染管线以释放显存/内存（恢复播放时自动重建）"
                            );
                            crate::thumb::trim_heap();
                            return None;
                        }
                        return Some(started + PAUSE_UNLOAD_DELAY);
                    }
                }
                return None;
            }
            let video = ch.video.as_mut()?;
            let tick = poll_video(video.as_mut(), now);
            (tick.frame, tick.next_wake, tick.error)
        };
        // T68：视频播完一轮（PTS 回卷）⇒ 按 `on_video_end` 决定前进还是留在当前项。
        //
        // 放在"取帧"之后、"提交该帧"之前：回卷后的第一帧属于新一轮，前进时它会被
        // 丢弃（新项会自己出帧），不前进时照常提交。
        if self.note_video_pts(key) {
            let advance = self
                .channels
                .get(key)
                .map(|ch| !ch.pinned && ch.mode.advances_on_video_end())
                .unwrap_or(false);
            if advance {
                tracing::info!(
                    output = %key,
                    mode = self.channels.get(key).map(|c| c.mode.as_str()).unwrap_or("?"),
                    "视频播放到结尾（PTS 回卷）：按 on_video_end=next 轮换到下一项"
                );
                if let Err(e) = self.switch_next(key, true) {
                    self.note_rotation_error(key, &e.message);
                }
                return video_next;
            }
            tracing::debug!(
                output = %key,
                "视频播放到结尾（PTS 回卷）：on_video_end=loop，保持当前项"
            );
        }
        if let Some(vframe) = video_frame {
            let present_started = Instant::now();
            if let Some(ch) = self.channels.get_mut(key) {
                // 日志节拍用"前后两次观察值"判定，**不对累计值取模**
                // （`dropped_late % 60 == 1` 在掉过一帧后恒真 ⇒ 每帧一条 INFO）。
                let snapshot = ch.video.as_ref().map(|v| v.video_stats());
                if let Some(st) = snapshot {
                    if should_log_video_progress(
                        st.presented,
                        st.dropped_late,
                        ch.last_dropped_late,
                    ) {
                        log_video_progress(key, &st, ch.presented_ok, ch.present_failures);
                    }
                    ch.last_dropped_late = st.dropped_late;
                }
                let (mode_w, mode_h) = ch
                    .current
                    .as_ref()
                    .and_then(|d| {
                        let parts: Vec<u32> = d
                            .mode_str
                            .split('x')
                            .filter_map(|s| s.split('@').next().and_then(|n| n.parse().ok()))
                            .collect();
                        match parts.as_slice() {
                            [w, h] => Some((*w, *h)),
                            _ => None,
                        }
                    })
                    .unwrap_or((0, 0));
                let use_gpu_transform = ch
                    .video
                    .as_ref()
                    .is_some_and(|v| v.handles_render_transform())
                    && !matches!(vframe, DisplayFrame::Cpu(_));
                if mode_w > 0 && mode_h > 0 {
                    let t0 = Instant::now();
                    let rt = self.output_render_transform(key);
                    let (rotated, rotate_ms, fit_params) = if use_gpu_transform {
                        (vframe, 0, FitParams::default())
                    } else {
                        let transformed = match vframe {
                            DisplayFrame::Cpu(rgba) => {
                                let r = liuguang_core::transform::rotate_frame(&rgba, rt.rotation)
                                    .unwrap_or(rgba);
                                let b =
                                    liuguang_core::transform::scale_brightness(&r, rt.brightness)
                                        .unwrap_or(r);
                                DisplayFrame::Cpu(b)
                            }
                            other => other,
                        };
                        (
                            transformed,
                            t0.elapsed().as_millis() as u64,
                            self.default_fit_params(key),
                        )
                    };
                    let dims = (rotated.width(), rotated.height());
                    let t1 = Instant::now();
                    match liuguang_core::fit::fit_visible_rects(dims, (mode_w, mode_h), &fit_params)
                    {
                        Ok(r) => {
                            let fit_ms = t1.elapsed().as_millis() as u64;
                            let src = (
                                (r.src_x * 65536.0).round().max(0.0) as u64,
                                (r.src_y * 65536.0).round().max(0.0) as u64,
                                (r.src_w * 65536.0).round().max(65536.0) as u64,
                                (r.src_h * 65536.0).round().max(65536.0) as u64,
                            );
                            let dst = (
                                r.dst_x.round() as i64,
                                r.dst_y.round() as i64,
                                r.dst_w.round().max(1.0) as u64,
                                r.dst_h.round().max(1.0) as u64,
                            );
                            let t2 = Instant::now();
                            let commit_baseline = self.backend.commit_stats();
                            let res = self.present_frame_fitted(key, rotated.clone(), src, dst);
                            // S2：**直通被拒 ≠ 视频失败**。显示侧不接受这帧的
                            // DMA-BUF（modifier/平面组合）时，正确动作是闩死直通、
                            // 重开一个 CPU 生产者继续播——而不是把视频降级成静态图
                            // （旧实现在这里落到 `record_present` ⇒ 连续 3 帧就拆
                            // 解码，用户看到的是"视频莫名其妙变静止"）。
                            if let Err(e) = &res {
                                if e.detail_code.as_deref() == Some(DMABUF_REJECTED_CODE) {
                                    return self
                                        .latch_dmabuf_rejection(key, &e.message, video_next);
                                }
                            }
                            // 显示层对账（t18 API）：**只在 present 返回 Ok 时查** ——
                            // `present_failures` 统计"返回 Err"，`drm_commit_failed` 统计
                            // "返回 Ok 但 DRM 记账说该帧没提交"（重试/legacy 回退路径），
                            // 后者正是只信返回值会漏掉的盲区。
                            let drm_delta = if res.is_ok() {
                                commit_baseline
                                    .as_ref()
                                    .and_then(|b| self.backend.commit_delta_since(b))
                            } else {
                                None
                            };
                            if let Some(delta) = drm_delta {
                                // 真实对账：媒体侧自**上次对账**以来交付了多少帧
                                // （不是常量 1——那会让 anomaly 判据恒真、预算失效）。
                                let presented_delta = self.media_presented_delta(key);
                                if let Some(reason) =
                                    self.note_drm_commit(key, &delta, presented_delta, "视频帧")
                                {
                                    self.handle_video_failure(key, &reason);
                                    return None;
                                }
                            }
                            if let Some(reason) = self.record_present(key, res, "视频帧", &rotated)
                            {
                                // 与停摆看门狗**同一出口**：拆解码 → DegradedStatic/后备画面。
                                self.handle_video_failure(key, &reason);
                                return None;
                            }
                            tracing::debug!(
                                output = %key,
                                rotate_ms, fit_ms,
                                commit_ms = t2.elapsed().as_millis() as u64,
                                total_ms = present_started.elapsed().as_millis() as u64,
                                "视频帧分段耗时（硬件平面缩放）"
                            );
                        }
                        Err(e) => tracing::warn!(output = %key, error = %e, "视频帧适配失败"),
                    }
                }
            }
        }
        if let Some(err) = video_error {
            self.handle_video_failure(key, &err);
            return None;
        }

        // 停摆看门狗（兜底）：视频在跑且未暂停/无转场，但 `presented` 长时间零增长
        // ⇒ 判为停摆并走统一失败路径。排除合法零增长：暂停（不拉帧）、
        // 极低 fps（阈值远大于 1 帧间隔）、转场进行中（有意让路）。
        let stalled_age = {
            let Some(ch) = self.channels.get_mut(key) else {
                return video_next;
            };
            if ch.video.is_none() || ch.paused || ch.transition.is_some() {
                None
            } else {
                let presented = ch
                    .video
                    .as_ref()
                    .map(|v| v.video_stats().presented)
                    .unwrap_or(0);
                if presented != ch.last_presented {
                    ch.last_presented = presented;
                    ch.last_presented_at = now;
                    ch.stall_reported = false;
                    // 真的出帧了 ⇒ 之前的重建（若有）算成功，预算重新累计。
                    ch.stall_rebuilds = 0;
                    None
                } else if !ch.stall_reported
                    && now.saturating_duration_since(ch.last_presented_at) >= VIDEO_STALL_TIMEOUT
                {
                    ch.stall_reported = true;
                    Some(now.saturating_duration_since(ch.last_presented_at))
                } else {
                    None
                }
            }
        };
        if let Some(age) = stalled_age {
            // **自愈优先**（2026-11-22）：暂停/熄屏/全屏恢复后，管线可能确实
            // 已经不在了（播放器还在，但解码线程死了 / 生产者被回收）。这时直接
            // 降级成静态图等于把"用户什么都没做"变成"永久丢画面"——重开一次
            // 生产者 + 播放器是廉价自救。只有重建也用尽预算或重建本身失败，
            // 才走统一失败路径（不掩盖真帧错误）。
            if self.try_rebuild_stalled_video(key) {
                return None;
            }
            self.handle_video_failure(
                key,
                &format!(
                    "停摆：presented 已有 {:.1}s 零增长（阈值 {:.0}s）",
                    age.as_secs_f64(),
                    VIDEO_STALL_TIMEOUT.as_secs_f64()
                ),
            );
            return None;
        }
        video_next
    }

    fn default_fit_params(&self, key: &str) -> FitParams {
        let out = self.config.outputs.get(key);
        FitParams {
            mode: out.map(|o| o.fit).unwrap_or_default(),
            zoom: out.map(|o| o.zoom).unwrap_or(1.0),
            offset_x: out.map(|o| o.offset_x).unwrap_or(0.0),
            offset_y: out.map(|o| o.offset_y).unwrap_or(0.0),
        }
    }

    fn output_render_transform(&self, key: &str) -> liuguang_core::transform::RenderTransform {
        let out = self.config.outputs.get(key);
        liuguang_core::transform::RenderTransform {
            fit: out.map(|o| o.fit).unwrap_or_default(),
            zoom: out.map(|o| o.zoom).unwrap_or(1.0),
            offset_x: out.map(|o| o.offset_x).unwrap_or(0.0),
            offset_y: out.map(|o| o.offset_y).unwrap_or(0.0),
            rotation: out.map(|o| o.rotation).unwrap_or_default(),
            brightness: out.map(|o| o.brightness).unwrap_or(1.0),
        }
    }

    /// 提交帧到指定通道输出（帧已适配到模式尺寸，整帧整屏）。
    fn present_frame(&mut self, key: &str, frame: CpuFrame) -> Result<(), ErrorBody> {
        let (w, h) = (frame.width, frame.height);
        self.present_frame_fitted(
            key,
            DisplayFrame::Cpu(frame),
            (0, 0, u64::from(w) << 16, u64::from(h) << 16),
            (0, 0, u64::from(w), u64::from(h)),
        )
    }

    /// 提交原生帧：硬件平面把 src（16.16）拉伸到 dst（模式像素）——
    /// 免 CPU 逐像素缩放（M5 性能路径）。
    ///
    /// **DMA-BUF 被拒是"策略拒绝"而不是"设备故障"**：显示后端返回
    /// [`liuguang_display::DisplayError::DmaBufRejected`] 时，错误里带上稳定机器码
    /// [`DMABUF_REJECTED_CODE`]，让视频路径能把它与真正的提交失败区分开
    /// （前者重开 CPU 生产者就能继续播，后者才该收敛到降级态）。
    fn present_frame_fitted(
        &mut self,
        key: &str,
        frame: DisplayFrame,
        src: (u64, u64, u64, u64),
        dst: (i64, i64, u64, u64),
    ) -> Result<(), ErrorBody> {
        let output = OutputId::new(key);
        // 提交后 dump 需要一份帧的副本（`DmaBuf` 的 fd 是 `Arc` 克隆，廉价）。
        let probe_frame: Option<DisplayFrame> = std::env::var("LIUGUANG_FRAME_PROBE_POST")
            .ok()
            .map(|_| frame.clone());
        // 帧内容探针（`LIUGUANG_FRAME_PROBE_DIR`）：**只读** dump，默认零开销。
        //
        // **放在提交之前**：我们要记录的是"即将交给显示侧的那份内容"——
        // 这既能与 CPU 臂比对，也能在**提交失败时**留下证据（失败帧恰恰最需要它）。
        // pts 取该通道视频的 `last_pts`（跨臂比对的唯一键）。
        {
            // 视频帧用 `last_pts`（跨臂比对的唯一键）；静态帧没有 pts，
            // 用**提交序号**兜底——静态帧不参与跨臂比对，但仍落盘以便观察。
            let pts_ms = self
                .channels
                .get(key)
                .and_then(|ch| ch.video.as_ref())
                .and_then(|v| v.video_stats().last_pts)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_else(|| self.backend.commit_stats().map_or(0, |s| s.commit_ok));
            if let Some(ch) = self.channels.get_mut(key)
                && ch.frame_probe.enabled()
            {
                ch.frame_probe.record(pts_ms, &frame);
            }
        }
        let token = self
            .backend
            .present_fitted(output.clone(), frame.clone(), src, dst)
            .map_err(|e| {
                let body = ErrorBody::new(
                    ErrorKind::DeviceUnavailable,
                    format!("提交扫描输出失败: {e}"),
                );
                // 类型化判别（不是字符串匹配）：显示侧的 `DmaBufRejected` 是唯一来源。
                if matches!(e, liuguang_display::DisplayError::DmaBufRejected { .. }) {
                    body.with_detail_code(DMABUF_REJECTED_CODE)
                } else if matches!(e, liuguang_display::DisplayError::MasterUnavailable { .. }) {
                    // **可重试的环境错误**：调用方据此跳过失败计数（t94）。
                    body.with_detail_code(MASTER_UNAVAILABLE_CODE)
                } else {
                    body
                }
            })?;
        if let Some(ch) = self.channels.get_mut(key) {
            let is_video = ch.video.is_some();
            if let Some(cur) = &mut ch.current {
                // 视频帧不驻留像素（RSS：每帧 12–33MB，且仅用于热插拔恢复，
                // 恢复时重建视频通道即可）；NV12/DMA-BUF 亦不作为静态回退。
                if let DisplayFrame::Cpu(cpu) = frame {
                    if !is_video && !cpu.format.is_semi_planar() {
                        cur.frame = cpu;
                    }
                }
            }
        }
        // **提交之后**再 dump 一次同一缓冲（`LIUGUANG_FRAME_PROBE_POST=1` 时）：
        // 若 flips 是阻塞式（当前实现不带 NONBLOCK），这里已是"flip 完成之后"，
        // 因此 `.pre` vs `.post` 的差异就能判定"缓冲是否在扫描输出期间被改写"。
        if let Some(pf) = probe_frame.as_ref()
            && std::env::var("LIUGUANG_FRAME_PROBE_POST").is_ok_and(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "" | "0" | "off" | "false" | "no"
                )
            })
        {
            let pts_ms = self
                .channels
                .get(key)
                .and_then(|ch| ch.video.as_ref())
                .and_then(|v| v.video_stats().last_pts)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(token.sequence);
            if let Some(ch) = self.channels.get_mut(key) {
                ch.frame_probe.record_post(pts_ms, pf);
            }
        }
        tracing::debug!(output = %key, sequence = token.sequence, "帧已提交扫描输出");
        Ok(())
    }

    /// 切换到下一项（M3 轮换 / IPC next / 视频播完）。
    ///
    /// **T68**：游标只从 [`Self::build_playlist_for`] 来 —— 有播放列表时
    /// "只有加入的才播"，没有时才是目录轮换；两者**失败不互相兜底**。
    pub fn switch_next(&mut self, key: &str, use_transition: bool) -> Result<(), ErrorBody> {
        // 游标为空（启动首帧 / 播完一轮 / 目录为空）或来源变更（reload、
        // update_playlist）⇒ 先按**当前生效的播放源**重建游标。
        let need_rebuild = match self.channels.get(key) {
            Some(ch) => {
                ch.playlist_error.is_none() && (ch.playlist_dirty || ch.playlist.is_empty())
            }
            None => false,
        };
        if need_rebuild {
            if let Err(e) = self.rebuild_channel_playlist(key) {
                self.note_rotation_error(key, &e.message);
                return Err(e);
            }
        }
        // 播放源坏着的时候**原样返回那条错误**（而不是笼统的"列表为空"）：
        // 调用方/用户要知道到底是"没配条目"还是"条目全失效"。
        if let Some(err) = self
            .channels
            .get(key)
            .and_then(|ch| ch.playlist_error.clone())
        {
            return Err(err);
        }
        let entry = {
            let Some(ch) = self.channels.get_mut(key) else {
                return Err(ErrorBody::new(
                    ErrorKind::UnknownOutput,
                    format!("输出 {key} 无通道"),
                ));
            };
            ch.playlist.next()
        };
        let Some(entry) = entry else {
            tracing::warn!(output = %key, "播放列表为空/用尽");
            return Err(ErrorBody::new(
                ErrorKind::FileUnavailable,
                format!("输出 {key} 播放列表为空"),
            ));
        };
        self.show_entry(key, &entry, use_transition)
    }

    /// 显示指定媒体项。
    fn show_entry(
        &mut self,
        key: &str,
        entry: &MediaEntry,
        use_transition: bool,
    ) -> Result<(), ErrorBody> {
        match entry.kind {
            MediaKind::Image => {
                let result = self.show_image(key, &entry.path, use_transition);
                if result.is_ok() {
                    // 显示成功之后才排预加载：坏图/无设备时不该再为"下一张"做一次解码。
                    self.schedule_next_preload(key);
                }
                result
            }
            MediaKind::Video => self.show_video(key, &entry.path),
        }
    }

    /// 静态图片路径（解码→旋转→fit→[转场]→提交）。
    fn show_image(
        &mut self,
        key: &str,
        path: &Path,
        use_transition: bool,
    ) -> Result<(), ErrorBody> {
        if !self.backend_available {
            return Err(ErrorBody::new(
                ErrorKind::DeviceUnavailable,
                "显示设备不可用（HeadlessReady，等待设备恢复/热插拔）",
            ));
        }
        let path = self.check_media_path(path)?;

        // 先解析输出模式（只读，不碰设备）：解码要按它的 1.25 倍做**降采样**，
        // 否则 61 MP 的 PNG 会先解出 244 MB 的 RGBA 再缩到 2560×1600（§性能/内存）。
        let output = OutputId::new(key);
        let mode_str = self.config.display.mode.clone();
        let selection = ModeSelection::parse(&mode_str)
            .map_err(|e| ErrorBody::new(ErrorKind::InvalidParams, format!("模式非法: {e}")))?;
        let mode = self
            .backend
            .resolve_mode(&output, &selection)
            .map_err(|e| ErrorBody::new(ErrorKind::UnknownOutput, e.to_string()))?;
        let fitted = (
            mode.width.saturating_add(mode.width / 4).max(64),
            mode.height.saturating_add(mode.height / 4).max(64),
        );
        self.preload_dims = fitted;

        // 先解码，坏图不触碰显示设备。轮换前已预加载过的直接复用
        // （`performance.preload_next`；凭据不符按未命中处理）。
        let rgba = match self.take_preloaded(&path) {
            Some(cached) => {
                tracing::debug!(media = %path.display(), "预加载命中：跳过展示时刻的同步解码");
                cached
            }
            None => prepare_image_fitted(&path, fitted.0, fitted.1).map_err(|e| {
                ErrorBody::new(
                    ErrorKind::FileUnavailable,
                    format!("PNG 解码失败（{}）: {e}", path.display()),
                )
            })?,
        };

        let rotation = self
            .config
            .outputs
            .get(key)
            .map(|o| o.rotation)
            .unwrap_or(Rotation::Deg0);
        let brightness = self
            .config
            .outputs
            .get(key)
            .map(|o| o.brightness)
            .unwrap_or(1.0);
        let rgba = liuguang_core::transform::rotate_frame(&rgba, rotation)
            .map_err(|e| ErrorBody::new(ErrorKind::Internal, format!("旋转失败: {e}")))?;
        let rgba = liuguang_core::transform::scale_brightness(&rgba, brightness)
            .map_err(|e| ErrorBody::new(ErrorKind::Internal, format!("亮度调节失败: {e}")))?;

        self.backend
            .set_mode(output.clone(), mode)
            .map_err(|e| ErrorBody::new(ErrorKind::DeviceUnavailable, e.to_string()))?;

        let fit_params = self.default_fit_params(key);
        let scanout = render_fit(&rgba, (mode.width, mode.height), &fit_params, [0, 0, 0])
            .map_err(|e| ErrorBody::new(ErrorKind::Internal, format!("渲染失败: {e}")))?;
        // 可观测：把**实际用于渲染**的 fit/zoom/offset 打出来。用户曾无法判断
        // `--fit/--zoom` 到底有没有生效（AUDIT §2.1 的"静默 no-op"）。
        tracing::info!(
            output = %key,
            media = %path.display(),
            fit = ?fit_params.mode,
            zoom = fit_params.zoom,
            offset_x = fit_params.offset_x,
            offset_y = fit_params.offset_y,
            "静态画面已提交（fit/zoom 实际生效值）"
        );

        // 静态图可用：立刻显式拆除该通道的视频管线（GStreamer 管线进 NULL →
        // qtdemux 等流线程结束）。只把 `video` 置空依赖 Drop 时机，真机曾出现
        // "state=StaticIdle 仍挂 2 个 qtdemux 线程"。
        self.stop_video(key);

        // 转场（M4）：最后请求优先——新请求丢弃旧转场。
        let do_transition = use_transition
            && self.transitions.effect != "none"
            && self.config.transition.enabled
            && self.transitions.duration_ms > 0;
        let previous = self
            .channels
            .get(key)
            .and_then(|c| c.current.as_ref().map(|d| d.frame.clone()));
        if do_transition {
            if let (Some(from), Some(ch)) = (previous, self.channels.get_mut(key)) {
                let mut anim = CpuTransition::new(self.transitions.to_spec());
                if anim.prepare_frames(from, scanout.clone()).is_ok() {
                    ch.transition = Some(ActiveTransition {
                        anim,
                        started: Instant::now(),
                        duration: Duration::from_millis(u64::from(self.transitions.duration_ms)),
                    });
                    ch.pending_target = Some(scanout.clone());
                }
            }
        }
        let started_transition = self
            .channels
            .get(key)
            .map(|c| c.transition.is_some())
            .unwrap_or(false);
        if !started_transition {
            self.present_frame(key, scanout.clone())?;
        }
        // 状态机接线（每一步都必须是合法转移）：
        //  - 有转场：停在 `Transitioning`，由转场**真正结束**时收敛（step_channel）；
        //  - 无转场：一步到位宣布新画面生效；
        //  - 显示尚未初始化（开机后第一次显示）：切换事件非法，直接宣布终态。
        let switching = self.drive_switch();
        if !started_transition || !switching {
            self.drive_shown(key);
        }
        self.publish_media(key, path.as_path());

        // 更新通道当前项。
        let mode_str = format!("{}x{}@{:.3}", mode.width, mode.height, mode.refresh_hz());
        // 轮换间隔：**唯一来源是配置**（T68 起：有播放列表时用列表自己的
        // `interval_seconds`；否则 [outputs.<name>] 或全局 [slideshow]）。
        // 这里只是"从本次切换重新计时"。
        // 曾经硬编码 600s，导致用户改 interval_seconds 完全无效。
        let interval = self.effective_interval(key);
        if let Some(ch) = self.channels.get_mut(key) {
            ch.video = None;
            if !started_transition {
                ch.pending_target = None;
            }
            // T68：换项即重置"视频播完一轮"的观察状态（否则新视频的首个 PTS
            // 会被拿旧视频的 PTS 去比，误判成回卷）；成功切换也清掉轮换错误历史。
            ch.last_video_pts = None;
            ch.video_loops = 0;
            ch.last_rotation_error = None;
            ch.current = Some(CurrentDisplay {
                media: path.to_path_buf(),
                frame: scanout,
                mode_str,
                decoder: "static".to_string(),
            });
            if !ch.paused {
                // 暂停中不重新排期（scheduler 自身的暂停语义保持不变）。
                ch.scheduler.set_interval(interval, Instant::now());
            }
        }
        // 暂停是用户意图：`set`/`next` 不得静默解除它（旧的 `ch.paused = false`）。
        self.restore_paused_state();
        self.save_state();
        Ok(())
    }

    /// 视频路径（M5）。
    ///
    /// 解码器选择完全由配置策略 + **本机实际能力**决定（见 `liuguang_media::va`
    /// 与 `DecoderPolicy`）：`status.decoder` 报告**实际生效**的元素名，
    /// 软解回退大声告警且可被配置拒绝。
    ///
    /// **闩死语义**（S2）：显示侧拒绝过 DMA-BUF 的通道之后**不再尝试直通**
    /// （切下一个视频也一样），直接走 CPU 生产者——否则每个视频都要重复
    /// "被拒 → 重开"这一轮，白烧一轮解码管线。
    fn show_video(&mut self, key: &str, path: &Path) -> Result<(), ErrorBody> {
        let latched = self
            .channels
            .get(key)
            .map(|ch| ch.dmabuf_latched)
            .unwrap_or(false);
        self.show_video_with(key, path, latched)
    }

    /// [`Self::show_video`] 的显式版本：`force_cpu = true` 时只尝试
    /// NV12/RGBA（**不碰 DMA-BUF**，见 [`liuguang_media::gstreamer::GstProducer::open_cpu`]）。
    fn show_video_with(
        &mut self,
        key: &str,
        path: &Path,
        force_cpu: bool,
    ) -> Result<(), ErrorBody> {
        if !self.backend_available {
            return Err(ErrorBody::new(
                ErrorKind::DeviceUnavailable,
                "显示设备不可用（HeadlessReady，等待设备恢复/热插拔）",
            ));
        }
        let path = self.check_media_path(path)?;
        // **t15**：硬件解不动的素材先转码成可硬解的缓存，绝不静默软解。
        // 缓存已就绪 ⇒ 换成缓存路径；否则登记"转完自动播"并回 NEEDS_PREPARATION。
        let path = self.ensure_playable_video(key, path)?;
        // 切到新视频前先拆掉旧管线（避免两条解码管线并行解码）。
        self.stop_video(key);
        #[cfg(feature = "gstreamer")]
        {
            let policy = video_policy(&self.config);
            let rotation = self
                .config
                .outputs
                .get(key)
                .map(|o| o.rotation)
                .unwrap_or(Rotation::Deg0);
            // VA-API 零拷贝直通优先于 GStreamer：解码器把表面导出成
            // DMA-BUF 直接交给合成器，CPU 只做一次导入（0 拷贝）。
            // 不可用（无节点/跨 GPU 修饰符不被接受）时静默回落到 GStreamer。
            let rt = self.output_render_transform(key);
            #[cfg(feature = "vaapi")]
            // 能力性拒绝（10bit / 没有可用 VA-API 解码器）由 `?` **立即返回**：
            // 不许静默软解 —— 用户要看到"该素材解不了，怎么解决"。
            let vaapi = if force_cpu {
                None
            } else {
                self.open_vaapi_producer(key, path.as_path())?
            };
            #[cfg(feature = "vaapi")]
            if let Some((producer, node, needs_rgb)) = vaapi {
                use liuguang_media::video::FrameProducer as _;
                // 走 GPU 转换时交出去的是 XRGB8888（合成器能正确染色），
                // 标签里如实写出来，`liuguangctl status` 一眼能看出走了哪条路。
                let capture_mode = if producer.handles_render_transform() {
                    "dmabuf-rgb"
                } else {
                    "dmabuf"
                };
                let attached =
                    self.attach_video_player(key, path.as_path(), producer, "nvdec", capture_mode);
                if attached.is_ok() {
                    // 这个后端已经收过 VA-API 的帧：后续同节点+同模式 set 不必再同步等首帧（t43）。
                    self.vaapi_verified_node = Some((node, needs_rgb));
                }
                return attached;
            }
            // t33：能走到这里，说明 VA-API 这一路没成（`open_vaapi_producer` 的
            // `Ok(None)`：设置关掉 / 后端不吃 DMA-BUF / 节点或修饰符不可用）。下面这条
            // GStreamer 管线是**写死的** `filesrc ! qtdemux ! h264parse`——MKV/WebM
            // 会在首帧才炸成 `atom has bogus size`，用户看到的是 `UNSUPPORTED_CODEC`。
            // 所以按"实际要走的路径"再判一次：能转成 MP4 缓存就转，绝不把容器错误
            // 冒充成解码错误（t28 F2 的现场就是这么来的）。
            let path = self.ensure_playable_video_with(
                key,
                path,
                crate::compat::DecodeConsumer::GStreamer,
            )?;
            let needs_pixel_transform =
                rotation != Rotation::Deg0 || (rt.brightness - 1.0).abs() > 1e-4;
            let producer = match producer_kind(
                force_cpu,
                self.backend.supports_dmabuf_scanout(),
                needs_pixel_transform,
            ) {
                ProducerKind::Cpu => {
                    // 直通被拒（已闩死）**或后端本就不扫描输出 DMA-BUF**：CPU 捕获模式。
                    // 生产者选择必须跟随后端的**实际能力**：旧 unit 遗留
                    // `LIUGUANG_VIDEO_DMABUF=1` 时若仍按直通协商开管线，每一帧都会被
                    // 显示侧拒绝 ⇒ 视频无法持续播放（2026-10-20 审核 P1-1 回归场景）。
                    if !force_cpu {
                        tracing::info!(
                            output = %key,
                            "显示后端不支持 DMA-BUF 扫描输出：解码管线按 CPU 捕获打开（忽略 LIUGUANG_VIDEO_DMABUF）"
                        );
                    }
                    if !self.backend.accepts_yuv_cpu_frames() {
                        // wl_shm 类后端只吃 RGB：NV12 帧会被显示侧以
                        // `Unsupported` 拒绝（真机硬解生效后连续 3 次提交失败 ⇒ 降级）。
                        tracing::info!(
                            output = %key,
                            "显示后端只接受 RGB 帧：解码管线按 RGBA 捕获打开（硬解时由 vapostproc 转换）"
                        );
                        liuguang_media::gstreamer::GstProducer::open_rgba(&path, policy)
                    } else {
                        liuguang_media::gstreamer::GstProducer::open_cpu(&path, policy)
                    }
                }
                ProducerKind::Auto => {
                    // **把目标显示尺寸交给媒体侧**（2026-10-20 晚）：
                    // 解码器出的是 Y_TILED 缓冲，而 KMS 平面缩放器处理 Y_TILED 时
                    // 会产生固定位移副本 + 横向条带（真机实测）。让 `vapostproc`
                    // 在 GPU 侧缩放+裁切成屏幕尺寸后，显示层只做 1:1 平面扫描，
                    // 完全绕开那条有缺陷的缩放路径。
                    let target = self
                        .backend
                        .outputs()
                        .into_iter()
                        .find(|o| o.id.as_str() == key)
                        .and_then(|o| o.current_mode)
                        .map(|m| (m.width, m.height));
                    if self.backend.accepts_yuv_cpu_frames() {
                        liuguang_media::gstreamer::GstProducer::open_fitted(&path, policy, target)
                    } else {
                        // 只吃 RGB 的后端（wl_shm）：直通失败/被关闭时不能停在 NV12，
                        // 否则每一帧都被显示侧拒绝 ⇒ 视频降级成静态图（真机复现）。
                        tracing::info!(
                            output = %key,
                            "显示后端只接受 RGB 帧：DMA-BUF 之后的回退改为 RGBA 捕获"
                        );
                        // 目标尺寸传 `None`：不做 vapostproc 缩放，直接把解码器原生
                        // 尺寸的帧交给合成器缩放（`wp_viewport` 本来就负责这一步）。
                        // 真机实测（1920x1200@60fps → eDP-1 2560x1600）：缩放到模式尺寸
                        // 每帧多下载 2.2 倍像素，CPU 45% → 53%，故这里不缩放。
                        liuguang_media::gstreamer::GstProducer::open_fitted_rgba(
                            &path, policy, None,
                        )
                    }
                }
                ProducerKind::Rgba => {
                    // 旋转需要像素级处理：强制 CPU RGBA 生产者。
                    liuguang_media::gstreamer::GstProducer::open_rgba(&path, policy)
                }
            }
            .map_err(|error| {
                // 原始错误只进日志；用户看到的是翻译过、可操作的话。
                tracing::warn!(
                    output = %key,
                    media = %path.display(),
                    "视频打开失败：{error}"
                );
                ErrorBody::new(
                    ErrorKind::UnsupportedCodec,
                    friendly_decode_error(path.as_path(), &error.to_string()),
                )
            })?;
            let element = producer.decoder_element().to_string();
            let capture_mode = producer.capture_mode_name();
            self.attach_video_player(
                key,
                path.as_path(),
                producer,
                element.as_str(),
                capture_mode,
            )
        }
        #[cfg(not(feature = "gstreamer"))]
        {
            let _ = (key, path, force_cpu);
            Err(ErrorBody::new(
                ErrorKind::UnsupportedCodec,
                "视频支持未编入（需 --features gstreamer 构建）；显示 fallback 或静态图",
            ))
        }
    }

    /// 把帧生产者包成播放器并接到输出通道上。
    ///
    /// 抽出来是为了让 GStreamer 与 VA-API 两条生产线共用同一段收尾逻辑
    /// （模式设置、状态机推进、暂停语义、状态持久化）；两处各写一遍必然漂移。
    #[cfg_attr(not(any(feature = "gstreamer", feature = "vaapi")), allow(dead_code))]
    fn attach_video_player<P>(
        &mut self,
        key: &str,
        path: &Path,
        producer: P,
        element: &str,
        capture_mode: &str,
    ) -> Result<(), ErrorBody>
    where
        P: liuguang_media::video::FrameProducer + Send + 'static,
    {
        let info = producer.info();
        let source_fps = info.source_fps;
        // **8K H.264 产品告警**：判据取容器头（`probe`），不用解码器自报的
        // `MediaInfo.codec`（软解路径历史上把它写死成 "h264"，用它判断会误报）。
        let decode_alert = eight_k_h264_alert(path, &info);
        let options = liuguang_media::video::VideoOptions {
            // t49/t52：`lag_frames` **默认关闭（0）**。
            //
            // 理由（t52 判定）：
            // ① 它的时序假设（H-fence：写未完成被扫出）已被 `core-auditor` 撤回 ——
            //    两臂 CRC 每 5s 恰好 100 互异 = 100 帧，说明同一帧在 3 次刷新间内容不变；
            // ② 照片形态是"同一帧内容的固定位移副本 + 锐利横带 + 竖缝"⇒ 空间性病因；
            // ③ 该特性**从未在真机上被证明有效**，却会**推迟每一帧的上屏时间**（增加延迟）。
            // ⇒ **未经证实的行为改变不留在出厂配置里**；开关保留，需要时可显式开启做实验。
            lag_frames: 0,
            queue_size: self.config.performance.frame_queue_size.max(1) as usize,
            fps_limit: Some(crate::loadshed::fps_cap(
                self.config.media.video_fps_limit,
                self.shed.level(),
            )),
            drop_late_tolerance: Duration::from_millis(100),
            looping: true,
        };
        let mut player = liuguang_media::video::VideoPlayer::new(producer, options)
            .map_err(|e| ErrorBody::new(ErrorKind::Internal, e.to_string()))?;
        let kind = player.video_stats().decoder;
        let decoder = format!("{}:{element}({capture_mode})", decoder_kind_name(kind));
        if !kind.is_hardware() {
            // 大声告警：这是用户"占用率很高"的核心原因（4K60 软解实测
            // CPU 比硬解高一个数量级，约 2.8 个核跑满、比实时慢 3.8×）。
            tracing::warn!(
                output = %key,
                decoder = %decoder,
                "视频使用**软件解码**：CPU 占用将远高于硬解；\
                 如需拒绝软解请设 media.software_decode_fallback=false\
                 （或 media.hardware_decode=\"required\"）"
            );
            self.events.publish(
                EventMessage::new(EventKind::Error)
                    .with_output(key)
                    .with_message(format!("视频软解回退（CPU 开销大）: {decoder}")),
            );
        }

        let output = OutputId::new(key);
        let selection = ModeSelection::parse(&self.config.display.mode)
            .map_err(|e| ErrorBody::new(ErrorKind::InvalidParams, format!("模式非法: {e}")))?;
        let mode = self
            .backend
            .resolve_mode(&output, &selection)
            .map_err(|e| ErrorBody::new(ErrorKind::UnknownOutput, e.to_string()))?;
        self.backend
            .set_mode(output, mode)
            .map_err(|e| ErrorBody::new(ErrorKind::DeviceUnavailable, e.to_string()))?;

        // 按面板物理像素渲染：RGB 转换的输出帧尺寸就是 mode 尺寸，后续 fit
        // 计算得到 identity（源=整帧、目标=逻辑尺寸），合成器只剩一次 ≈1:1
        // 采样，省掉原来 3840→2560 的那次全屏缩放。
        //
        // 只有视频通道有这一层（静态图走 CPU 帧，尺寸由 daemon 决定）。
        //
        // **只在 cover + 无缩放/偏移时启用**：转换器按 cover 语义预裁切
        // （`RgbConverter::cover_crop`），其余形态（contain/center/stretch、
        // zoom≠1、带偏移）下预裁切的区域与合成器要的不一致——那时宁可按源尺寸
        // 出帧、让合成器自己裁（旧行为，画面一定对）。DMA-BUF 直通本来也只
        // 接受"目标矩形铺满输出"的 fit，这条判断与它同一集合。
        let fit = self.default_fit_params(key);
        if player.handles_render_transform() {
            player.set_output_size(mode.width, mode.height);
            player.set_render_transform(self.output_render_transform(key));
        } else if fit.mode == liuguang_core::fit::FitMode::Cover
            && (fit.zoom - 1.0).abs() < f64::EPSILON
            && fit.offset_x == 0.0
            && fit.offset_y == 0.0
        {
            player.set_output_size(mode.width, mode.height);
        } else {
            tracing::debug!(
                output = %key,
                fit = fit.mode.as_str(),
                zoom = fit.zoom,
                "fit 不是 cover 1:1：继续按源尺寸渲染，由合成器裁剪/缩放"
            );
        }

        if let Some(alert) = decode_alert.as_ref() {
            tracing::warn!(
                output = %key,
                decoder = %decoder,
                alert = %alert,
                "视频解码告警（不影响播放，但解释了卡顿）"
            );
            self.events.publish(
                EventMessage::new(EventKind::Error)
                    .with_output(key)
                    .with_message(alert.clone()),
            );
        }

        // t43 验收④：全屏让路 / 熄屏 / 游戏 / 显存压力 / 电池省电期间应用新视频，不能让新管线"趁机"
        // 开始解码（用户看到的是游戏/锁屏，后台不该烧 CPU/GPU）。两条路径（VA-API 与
        // GStreamer）都在这里收口，因此把全局环境控制器状态也同步进通道集合。
        let fs_active = self.fullscreen_paused.contains(key)
            || (self.fullscreen_active && self.config.performance.pause_on_fullscreen);
        let screen_off = self.screen_paused.contains(key) || self.screen.is_off();
        let game_active = self.game_paused.contains(key) || self.game.is_engaged();
        let gpu_active = self.gpu_paused.contains(key) || self.gpu.is_engaged();
        let battery_active = self.battery_paused.contains(key) || self.power.is_on_battery();
        let user_active = self.user_paused.contains(key);
        let environment_paused =
            fs_active || screen_off || game_active || gpu_active || battery_active;
        let should_pause = environment_paused || user_active;
        if fs_active {
            self.fullscreen_paused.insert(key.to_string());
        }
        if screen_off {
            self.screen_paused.insert(key.to_string());
        }
        if game_active {
            self.game_paused.insert(key.to_string());
        }
        if gpu_active {
            self.gpu_paused.insert(key.to_string());
        }
        if battery_active {
            self.battery_paused.insert(key.to_string());
        }
        if should_pause {
            let _ = player.pause_at(Instant::now());
        }
        if let Some(ch) = self.channels.get_mut(key) {
            ch.video = Some(Box::new(player));
            ch.decode_alert = decode_alert.clone();
            // 视频已重新建立：不再欠用户一次恢复。
            ch.video_resume_pending = None;
            // 上一次降级的原因随本次成功一并作废（状态输出不得继续报旧原因）。
            ch.degraded_reason = None;
            // 新视频源：停摆看门狗从零开始计时。
            ch.reset_stall_watchdog(Instant::now());
            // T68：新视频 ⇒ 重置 PTS 基线（否则上一段视频的 PTS 会被拿来比较，
            // 一开播就误判"回卷"）+ 清掉轮换错误历史（成功切换）。
            ch.last_video_pts = None;
            ch.video_loops = 0;
            ch.last_rotation_error = None;
            ch.transition = None;
            ch.pending_target = None;
            ch.current = Some(CurrentDisplay {
                media: path.to_path_buf(),
                frame: CpuFrame {
                    width: 1,
                    height: 1,
                    stride: 4,
                    format: liuguang_core::types::PixelFormat::Xrgb8888,
                    data: Arc::from(vec![0u8; 4].into_boxed_slice()),
                },
                mode_str: format!("{}x{}@{:.3}", mode.width, mode.height, mode.refresh_hz()),
                decoder: decoder.clone(),
            });
        }
        self.publish_media(key, path);
        if should_pause {
            self.pause_video_supply(key);
        }
        // 顶层状态机：切换 → 准备完成 → 视频已开始。
        // （InitializingDisplay 等状态会忽略前两个事件，VideoStarted 仍能
        // 落到 VideoPlaying，因此视频路径不再停在 InitializingDisplay。）
        let switching = self.drive_switch();
        if !switching {
            // 显示尚未初始化（开机后第一次显示视频）：直接宣布终态。
            self.drive_shown(key);
        } else {
            self.drive(Event::VideoStarted);
            // 暂停中切换视频：保持暂停语义（不静默解除）。
            self.restore_paused_state();
        }
        tracing::info!(
            output = %key,
            media = %path.display(),
            decoder = %decoder,
            source_fps = ?source_fps,
            state = self.state.current().name(),
            "视频通道已建立（decoder 为实际生效元素）"
        );
        self.save_state();
        Ok(())
    }

    /// 解析并打开 VA-API 零拷贝生产者（`None` = 该后端/该机器用不了）。
    ///
    /// 判据依序为：设置项（`LIUGUANG_VIDEO_VAAPI` 覆盖 `media.video_vaapi`）
    /// → 后端是否支持 DMA-BUF 扫描输出 → 逐节点探测 → **第一帧的修饰符
    /// 必须被当前后端接受**。最后一步是关键：解码器在 Intel 上产出
    /// I915 修饰符，而合成器挂在 NVIDIA 上时只广播 NVIDIA 修饰符，
    /// 这种帧导入必失败（跨 GPU），必须提前换节点而不是等合成器拒绝。
    ///
    /// 当 [`needs_rgb_conversion`] 为真（合成器只采样多平面 YUV 的亮度面）时，
    /// 每个节点改用 [`liuguang_vaapi::VaapiProducer::open_rgb`]：NV12 先在 GPU 上
    /// 转成单平面 XRGB8888，校验用的首帧修饰符也随之变成 RGB 那族；
    /// 所有节点都失败才回落到 GStreamer（CPU 路径）。
    #[cfg(feature = "vaapi")]
    /// 打开 VA-API 零拷贝生产者。
    ///
    /// 返回值三态，区分"这一路不行"与"这个源不行"：
    /// - `Ok(Some(_))`：用这个生产者；
    /// - `Ok(None)`：节点/修饰符/后端不吃（**可回退**到 GStreamer）；
    /// - `Err(_)`：**能力性拒绝**（源本身解不了：如 10bit、或没有可用的 VA-API
    ///   解码器）——换节点/换路径也不会成功，必须原样报给用户，
    ///   不能被后面那条软件的 GStreamer 路径的失败信息盖掉。
    fn open_vaapi_producer(
        &self,
        key: &str,
        path: &Path,
    ) -> Result<Option<(liuguang_vaapi::VaapiProducer, PathBuf, bool)>, ErrorBody> {
        use liuguang_core::fit::FitMode;
        use liuguang_media::video::{FrameProducer as _, VideoPoll};

        let setting = std::env::var("LIUGUANG_VIDEO_VAAPI")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.config.media.video_vaapi.clone());
        let setting = setting.trim().to_string();
        let lowered = setting.to_ascii_lowercase();
        if matches!(lowered.as_str(), "off" | "0" | "false" | "no" | "none") {
            tracing::debug!(output = %key, "VA-API 零拷贝已被配置关闭");
            return Ok(None);
        }
        if !self.backend.supports_dmabuf_scanout() {
            tracing::debug!(
                output = %key,
                "当前显示后端不支持 DMA-BUF 扫描输出：跳过 VA-API 零拷贝"
            );
            return Ok(None);
        }
        let forced_node = setting.starts_with('/');
        // 已知有问题的合成器：denial 0.6.7 的 NV12 **多平面** DMA-BUF 导入有缺陷——它只把
        // Y 平面当亮度铺到 R 通道，色度完全没参与，整屏变成单色红（像素实测 G/B 恒为 0）。
        // 我们导出的字节本身是对的（`export_probe --ppm` 与 ffmpeg 解码帧逐通道一致），
        // 所以不改提交方式：在 GPU 上把 NV12 转成单平面 XRGB8888 再交出去，颜色正确，
        // 而且依然零拷贝（输出是 gbm BO，不经 CPU 回读）。转换不可用时才回落 CPU 路径。
        let rt = self.output_render_transform(key);
        let transform_needs_rgb = rt.rotation != Rotation::Deg0
            || (rt.brightness - 1.0).abs() > 1e-4
            || rt.fit != FitMode::Cover
            || (rt.zoom - 1.0).abs() > 1e-4
            || rt.offset_x.abs() > 1e-4
            || rt.offset_y.abs() > 1e-4;
        let force_never_rgb = self
            .config
            .media
            .video_dmabuf_rgb
            .trim()
            .eq_ignore_ascii_case("never");
        let needs_rgb = needs_rgb_conversion(&self.config.media.video_dmabuf_rgb)
            || (!force_never_rgb && transform_needs_rgb);
        if needs_rgb {
            let compositor = crate::compositor_profile::detect_names()
                .into_iter()
                .collect::<Vec<_>>()
                .join(",");
            tracing::info!(
                output = %key,
                compositor = %compositor,
                "该合成器把多平面 YUV 的 DMA-BUF 只采样到亮度（整屏发红）：改走 GPU RGB 转换\
                 （NV12 → XRGB8888，仍为零拷贝）；若合成器已修好，可在 [media] video_dmabuf_rgb 写 \"never\""
            );
        }
        let preferred_mods = if needs_rgb {
            self.backend
                .dmabuf_modifiers_for(liuguang_vaapi::export::DRM_FORMAT_XRGB8888)
        } else {
            Vec::new()
        };
        let mut candidates: Vec<PathBuf> = if forced_node {
            vec![PathBuf::from(&setting)]
        } else {
            // `render_nodes()` 只按名字排序，会稳定地先试 `renderD128`——那可能是
            // 没接显示器的 iGPU。跨 GPU 导入在 4K60 下只剩 20fps（同卡 58fps），
            // 所以优先试**显示 GPU** 的节点；判别不了时顺序原样返回。
            liuguang_vaapi::render_nodes_preferring_display()
                .into_iter()
                .map(PathBuf::from)
                .collect()
        };
        if !forced_node
            && let Some((verified_node, verified_rgb)) = self.vaapi_verified_node.as_ref()
            && *verified_rgb == needs_rgb
            && let Some(pos) = candidates.iter().position(|n| n == verified_node)
            && pos > 0
        {
            let node = candidates.remove(pos);
            candidates.insert(0, node);
        }
        tracing::debug!(?candidates, "VA-API 候选渲染节点（显示 GPU 优先）");
        for node in candidates {
            if !liuguang_vaapi::probe_node(&node) {
                // `LIBVA_DRIVER_NAME` 是**进程级**开关：它会让 libva 用同一个驱动
                // 打开所有节点。设成 `iHD` 时 NVIDIA 节点会直接 EIO，跨 GPU
                // 零拷贝就永远探测不到——这是最容易踩的一脚。
                match std::env::var("LIBVA_DRIVER_NAME") {
                    Ok(forced) if !forced.trim().is_empty() => tracing::warn!(
                        node = %node.display(),
                        forced_driver = %forced,
                        "该渲染节点打不开 VA-API：LIBVA_DRIVER_NAME 会强制所有节点用同一驱动，                         跨 GPU 零拷贝因此不可用（要从桌面零拷贝就清掉这个环境变量）"
                    ),
                    _ => {
                        tracing::debug!(node = %node.display(), "该渲染节点没有可用的 VA-API 驱动")
                    }
                }
                continue;
            }
            let opened = if needs_rgb {
                liuguang_vaapi::VaapiProducer::open_rgb_with_modifiers(path, &node, &preferred_mods)
            } else {
                liuguang_vaapi::VaapiProducer::open(path, &node)
                    .map_err(liuguang_media::MediaError::from)
            };
            let mut producer = match opened {
                Ok(mut producer) => {
                    if needs_rgb {
                        if let Ok(selection) = ModeSelection::parse(&self.config.display.mode) {
                            if let Ok(mode) =
                                self.backend.resolve_mode(&OutputId::new(key), &selection)
                            {
                                producer.set_output_size(mode.width, mode.height);
                            }
                        }
                        producer.set_render_transform(rt);
                    }
                    producer
                }
                Err(error) => {
                    let message = error.to_string();
                    tracing::warn!(
                        node = %node.display(),
                        rgb = needs_rgb,
                        error = %message,
                        "VA-API 解码器打不开该节点（跳过）"
                    );
                    if vaapi_rejection_is_permanent(&message) {
                        // 换节点也是同样的结论：把可操作的原因交给用户，别再
                        // 让软件路径的失败信息把它盖掉（"10bit 请转 8bit"）。
                        return Err(ErrorBody::new(ErrorKind::UnsupportedCodec, message));
                    }
                    continue;
                }
            };
            // 先解一帧出来，确认这个节点的修饰符当前后端收不收。
            let mut optimistic = false;
            let first_frame = if self
                .vaapi_verified_node
                .as_ref()
                .is_some_and(|(n, r)| n == &node && *r == needs_rgb)
            {
                optimistic = true;
                tracing::debug!(
                    node = %node.display(),
                    "VA-API 首帧校验已通过过：跳过同步校验，首帧交给渲染循环"
                );
                None
            } else {
                let deadline = Instant::now() + VAAPI_FIRST_FRAME_TIMEOUT;
                let mut failure = None;
                let frame = loop {
                    match producer.next_producer_frame() {
                        Ok(VideoPoll::Frame { frame, .. }) => break Some(frame),
                        // 流空/到头：这个节点没有可交付的帧。
                        Ok(VideoPoll::Ended) => break None,
                        Ok(VideoPoll::NeedMore) => {
                            if Instant::now() >= deadline {
                                tracing::warn!(
                                    node = %node.display(),
                                    timeout_ms = VAAPI_FIRST_FRAME_TIMEOUT.as_millis() as u64,
                                    "VA-API 首帧未在预算内就绪：先接受该节点，交给渲染循环继续拉帧（不再阻塞 set）"
                                );
                                optimistic = true;
                                break None;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => {
                            failure = Some(error);
                            break None;
                        }
                    }
                };
                if let Some(error) = failure {
                    tracing::warn!(
                        node = %node.display(),
                        error = %error,
                        "VA-API 首帧解码失败（跳过该节点）"
                    );
                    continue;
                }
                frame
            };
            let accepted = match first_frame {
                Some(DisplayFrame::DmaBuf(frame)) => self
                    .backend
                    .accepts_dmabuf_format(frame.fourcc, frame.modifier),
                // 非 DMA-BUF 帧说明这条路径没走通（RGB 模式下不该发生）。
                Some(_) => false,
                // 首帧没在预算内出来：乐观接受，让渲染循环去验证（失败会走既有的
                // 连续提交失败降级路径，不会静默坏画面）。
                None => optimistic,
            };
            if accepted {
                tracing::info!(
                    output = %key,
                    node = %node.display(),
                    rgb = needs_rgb,
                    "VA-API 零拷贝直通已选中该渲染节点"
                );
                return Ok(Some((producer, node, needs_rgb)));
            }
            // 单个节点被否是**正常**的：跨 GPU 时本机必然有一个节点产出的修饰符
            // 合成器不认。逐节点记 debug，全部失败时在函数末尾统一告警一次。
            tracing::debug!(
                output = %key,
                node = %node.display(),
                "该节点的 DMA-BUF 修饰符不被当前后端接受（跨 GPU？），尝试下一个节点"
            );
        }
        tracing::warn!(
            output = %key,
            rgb = needs_rgb,
            "VA-API 零拷贝不可用：保持 GStreamer 解码管线（CPU 占用会明显更高）"
        );
        Ok(None)
    }

    /// 记录一次扫描输出提交结果：**绝不静默丢弃失败**。
    ///
    /// - 成功：`presented_ok += 1`、连续失败清零；
    /// - 失败：累计 + 连续计数，前 [`PRESENT_FAILURE_LOUD_BUDGET`] 次带细节 WARN，
    ///   连续失败达到 [`PRESENT_FAILURE_CONVERGE_THRESHOLD`] 时返回原因字符串，
    ///   由调用方交给 [`Self::handle_video_failure`] 收敛（视频路径）。
    ///
    /// 告警细节包含帧的 fourcc/modifier/平面/偏移：否则"tiled 能不能扫出去"
    /// 在日志里无从判断（那正是这条盲区存在的原因）。
    fn record_present(
        &mut self,
        key: &str,
        result: Result<(), ErrorBody>,
        what: &str,
        frame: &DisplayFrame,
    ) -> Option<String> {
        let mode = self
            .channels
            .get(key)
            .and_then(|c| c.current.as_ref().map(|d| d.mode_str.clone()))
            .unwrap_or_else(|| "?".into());
        match result {
            Ok(()) => {
                if let Some(ch) = self.channels.get_mut(key) {
                    ch.presented_ok += 1;
                    ch.present_failures_consecutive = 0;
                }
                None
            }
            Err(e) => {
                // **master 暂时不可用 ≠ 帧坏了**（t94）。
                //
                // 服务重启时旧进程还占着 DRM master，新进程最初几秒的提交全部
                // EACCES。旧实现把它当普通提交失败累计 ⇒ 3 帧就降级成静态图，
                // 用户看到的是"卡住"（而 master 几秒后就自愈了）。
                // 这里按**可重试的环境错误**处理：只记专门计数，不计失败、不收敛。
                if e.detail_code.as_deref() == Some(MASTER_UNAVAILABLE_CODE) {
                    let skips = match self.channels.get_mut(key) {
                        Some(ch) => {
                            ch.master_unavailable_skips =
                                ch.master_unavailable_skips.saturating_add(1);
                            ch.master_unavailable_skips
                        }
                        None => 0,
                    };
                    // 限速：首帧一条 + 之后每 120 帧一条（30fps ≈ 4s），不刷屏。
                    if skips == 1 || skips % 120 == 0 {
                        tracing::warn!(
                            output = %key,
                            what,
                            skips,
                            error = %e.message,
                            "未持有 DRM master：本帧丢弃（**不计提交失败、不降级**；master 取回后自动继续）"
                        );
                    }
                    return None;
                }
                let (total, consecutive) = match self.channels.get_mut(key) {
                    Some(ch) => {
                        ch.present_failures += 1;
                        ch.present_failures_consecutive += 1;
                        (ch.present_failures, ch.present_failures_consecutive)
                    }
                    None => (0, 0),
                };
                let detail = describe_frame_for_log(frame);
                if total <= PRESENT_FAILURE_LOUD_BUDGET {
                    tracing::warn!(
                        output = %key,
                        mode = %mode,
                        what,
                        total,
                        consecutive,
                        detail = %detail,
                        error = %e.message,
                        "扫描输出提交失败（帧被丢弃）"
                    );
                }
                if consecutive >= PRESENT_FAILURE_CONVERGE_THRESHOLD {
                    return Some(format!(
                        "连续 {consecutive} 次扫描输出提交失败（{what}，{mode}，{detail}）：{}",
                        e.message
                    ));
                }
                None
            }
        }
    }

    /// 媒体侧自**上次显示层对账**以来交付（出队）的帧数。
    ///
    /// 这是 S5 要求的那半边对账数据：`contradicts_presented` 的语义是"媒体侧说
    /// 交付了 N 帧，而显示侧同期有失败"，把常量塞进去等于自己把判据做成恒真。
    fn media_presented_delta(&self, key: &str) -> u64 {
        let Some(ch) = self.channels.get(key) else {
            return 0;
        };
        let now_presented = ch
            .video
            .as_ref()
            .map(|v| v.video_stats().presented)
            .unwrap_or(ch.drm_last_presented);
        now_presented.saturating_sub(ch.drm_last_presented)
    }

    /// 显示层对账：把 `CommitDelta` 里"该帧未提交"记进通道台账并（**按 episode
    /// 限速**）大声告警；连续 [`PRESENT_FAILURE_CONVERGE_THRESHOLD`] 次才返回收敛原因。
    ///
    /// 与 [`Self::record_present`] 互补：`present_*` 返回 Ok 时 DRM 层仍可能记录
    /// 失败（重试/legacy 回退路径），只信返回值会让"presented 在涨、屏幕不动"
    /// 这种组合继续蒙混过关。
    ///
    /// # 告警限速（S5）
    ///
    /// 旧实现把常量 `1` 传进 [`liuguang_display::CommitDelta::contradicts_presented`]：
    /// 本函数只在 `failed > 0` 时被调用，于是 `contradict` **恒真**，`|| contradict`
    /// 让"前 N 次才 WARN"的预算彻底失效 ⇒ 失败风暴里**每帧一条 WARN**（真机
    /// 实测 700+ 帧）。现在的口径：
    ///
    /// - `presented_delta` 传**真实的媒体侧 `presented` 增量**，`contradict` 只作为
    ///   日志里的**描述字段**，不再当预算旁路；
    /// - `drm_loud_used` 是 **episode 预算**，一次干净提交（`failed == 0`）就重置；
    /// - "该通道至今没有任何一次成功提交"是**系统性**失败，单独**一次性**升级
    ///   （`drm_zero_ok_warned`），不受 episode 预算影响。
    fn note_drm_commit(
        &mut self,
        key: &str,
        delta: &liuguang_display::CommitDelta,
        presented_delta: u64,
        what: &str,
    ) -> Option<String> {
        if delta.failed == 0 {
            if let Some(ch) = self.channels.get_mut(key) {
                ch.drm_failed_consecutive = 0;
                // 干净提交 ⇒ 新 episode：预算重置（否则一次历史风暴会永久静音）。
                ch.drm_loud_used = 0;
                ch.drm_last_presented = ch.drm_last_presented.saturating_add(presented_delta);
            }
            return None;
        }
        let (total, consecutive, loud_used, loud_total, ok_ever) = match self.channels.get_mut(key)
        {
            Some(ch) => {
                ch.drm_commit_failed = ch.drm_commit_failed.saturating_add(delta.failed);
                ch.drm_failed_consecutive += 1;
                ch.drm_last_presented = ch.drm_last_presented.saturating_add(presented_delta);
                (
                    ch.drm_commit_failed,
                    ch.drm_failed_consecutive,
                    ch.drm_loud_used,
                    ch.drm_loud_total,
                    ch.presented_ok,
                )
            }
            None => (delta.failed, 1, 0, 0, 0),
        };
        // 真实对账（描述性）：媒体侧确实交付了帧，而显示侧同期有失败。
        let contradict = delta.contradicts_presented(presented_delta);
        // 系统性失败：该通道**一次都没成功过** ⇒ 单独升级一次。
        let zero_ok = ok_ever == 0;
        let mut escalated = false;
        if zero_ok {
            if let Some(ch) = self.channels.get_mut(key) {
                if !ch.drm_zero_ok_warned {
                    ch.drm_zero_ok_warned = true;
                    escalated = true;
                }
            }
        }
        // 两道闸：episode 预算（失败风暴内限速）+ 生命周期硬上限（"失败/干净"
        // 长期交替时，episode 预算会被反复重置，只有总量上限能兜住）。
        let within_budget = loud_used < PRESENT_FAILURE_LOUD_BUDGET
            && loud_total < PRESENT_FAILURE_LOUD_TOTAL_BUDGET;
        if within_budget || escalated {
            if within_budget {
                if let Some(ch) = self.channels.get_mut(key) {
                    ch.drm_loud_used = ch.drm_loud_used.saturating_add(1);
                    ch.drm_loud_total = ch.drm_loud_total.saturating_add(1);
                }
            }
            tracing::warn!(
                output = %key,
                what,
                failed = delta.failed,
                commit_ok = delta.commit_ok,
                retried = delta.retried_after_busy,
                legacy = delta.legacy_fallback,
                presented_delta,
                total,
                consecutive,
                anomaly = contradict,
                systematic = escalated,
                "显示层记账报告该帧**未提交**（present 可能返回 Ok；presented 仍在涨）"
            );
        }
        if consecutive >= PRESENT_FAILURE_CONVERGE_THRESHOLD {
            return Some(format!(
                "连续 {consecutive} 次提交在显示层记账里失败（{what}，failed 累计 {total}）：画面不会更新"
            ));
        }
        None
    }

    /// **S2 收敛路径**：显示侧拒绝 DMA-BUF 后闩死直通并重开 CPU 生产者。
    ///
    /// 与 [`Self::handle_video_failure`] **不同**：视频本身没坏，坏的是"零拷贝
    /// 能不能用"这一条优化路径。返回值是通道下一次唤醒时刻：
    /// - `Some(t)`：CPU 管线已重建/正在重建，按 `t` 继续推进；
    /// - `None`：连 CPU 重开也失败 ⇒ 已走 `handle_video_failure`（解码已拆）。
    ///
    /// 告警限速：**首次**闩死时一条完整 WARN + 一条 `EventKind::Error`
    /// （用户可见的"已降级"），之后每 [`DMABUF_REJECT_LOUD_EVERY`] 次一条摘要，
    /// 其余 `debug`——否则"每帧被拒"会把日志刷爆（与 S5 同一个教训）。
    fn latch_dmabuf_rejection(
        &mut self,
        key: &str,
        reason: &str,
        video_next: Option<Instant>,
    ) -> Option<Instant> {
        let was_cached = self.vaapi_verified_node.take().is_some();
        let (count, first) = match self.channels.get_mut(key) {
            Some(ch) => {
                ch.dmabuf_rejections = ch.dmabuf_rejections.saturating_add(1);
                let first = !ch.dmabuf_latched;
                ch.dmabuf_latched = true;
                (ch.dmabuf_rejections, first)
            }
            None => {
                return Some(
                    video_next.unwrap_or_else(|| Instant::now() + VIDEO_DMABUF_LATCH_REPOLL),
                );
            }
        };
        if first {
            tracing::warn!(
                output = %key,
                reason,
                rejections = count,
                was_cached,
                "显示侧**拒绝 DMA-BUF 直通**：已闩死该通道的零拷贝路径，\
                 重开 CPU 生产者继续播放（视频不会降级成静态图）"
            );
            self.events.publish(
                EventMessage::new(EventKind::Error)
                    .with_output(key)
                    .with_message(format!(
                        "硬件直通被拒，已回退系统内存继续播放（CPU 占用会上升）: {reason}"
                    )),
            );
        } else if count % DMABUF_REJECT_LOUD_EVERY == 0 {
            tracing::warn!(
                output = %key,
                reason,
                rejections = count,
                "DMA-BUF 仍被拒（已闩死直通，重开 CPU 生产者）"
            );
        } else {
            tracing::debug!(output = %key, reason, rejections = count, "DMA-BUF 被拒");
        }
        let Some(path) = self
            .channels
            .get(key)
            .and_then(|ch| ch.current.as_ref().map(|d| d.media.clone()))
        else {
            tracing::warn!(output = %key, "DMA-BUF 被拒但通道没有当前媒体路径，无法重开");
            return Some(video_next.unwrap_or_else(|| Instant::now() + VIDEO_DMABUF_LATCH_REPOLL));
        };
        // force_cpu = true：直接跳过 VA-API，避免先试 VA-API 再重复被拒陷入无限重开循环。
        match self.show_video_with(key, &path, true) {
            Ok(()) => {
                tracing::info!(
                    output = %key,
                    media = %path.display(),
                    "已用 CPU 生产者重开视频（系统内存 NV12/RGBA，直通保持闩死）"
                );
                Some(video_next.unwrap_or_else(|| Instant::now() + VIDEO_DMABUF_LATCH_REPOLL))
            }
            Err(e) => {
                // 连 CPU 路径都开不起来 ⇒ 这才是真正的视频失败。
                self.handle_video_failure(
                    key,
                    &format!(
                        "DMA-BUF 被拒后 CPU 重开失败（{}）: {}",
                        path.display(),
                        e.message
                    ),
                );
                None
            }
        }
    }

    /// 视频失败的**统一收敛路径**：拆解码 → 状态机降级 → 尝试后备画面。
    ///
    /// 无论有没有后备画面，终态都必须如实：有后备画面并真的上屏 → 回到
    /// `StaticIdle`；没有/显示失败 → 保持 `DegradedStatic`（**绝不谎称 StaticIdle**）。
    /// 同时发 `EventKind::Error` 事件，保证订阅者与日志都能看到。
    fn handle_video_failure(&mut self, key: &str, reason: &str) {
        tracing::error!(output = %key, reason, "视频供给失败，回退静态");
        // t43：真实帧都没能提交成功，说明"这个后端 + 这个节点集合能出被接受的帧"
        // 的结论已经不成立（换显示器/换 GPU/驱动升级都可能）——清掉标志，
        // 下一次开视频重新做一次同步首帧校验，别带着错误的乐观假设继续跑。
        self.vaapi_verified_node = None;
        // 若这次失败发生在"master **未被确认**"期间，它很可能同样是环境问题：
        // 记下媒体路径，等 master 自愈后自动重建视频（否则屏幕永久停在静态帧）。
        //
        // 为什么用"未被确认"而不是"已确定不是"：真机竞态里第二个实例读连接器时
        // 会得到 **"输出 DP-1 已断开"**（没有 master 时读不到真实连接器状态），
        // 于是失败原因看起来是"输出断开"而不是 EACCES，而此时 `master` 往往还是
        // `None`（尚未被首帧提交确认）。只认 `Some(false)` 会漏掉这一整类。
        //
        // 代价：真帧错误若恰好发生在 master 未确认期，会在自愈后**多试一次**；
        // 试不通仍是 DegradedStatic（不掩盖问题），试通则说明本来就是环境问题。
        let master_confirmed = self
            .backend
            .master_status()
            .is_some_and(|m| m.master == Some(true));
        let environment_suspect = !master_confirmed;
        self.events.publish(
            EventMessage::new(EventKind::Error)
                .with_output(key)
                .with_message(format!("视频已停止: {reason}")),
        );
        if let Some(ch) = self.channels.get_mut(key) {
            ch.video = None;
            ch.pending_target = None;
            ch.last_video_pts = None;
            ch.stall_reported = false;
            // 降级原因**如实存档**（审核 P2-6）：状态输出引用它，而不是从
            // "有没有 pending"反推类别。
            ch.degraded_reason = Some(DegradedReason {
                message: reason.to_string(),
                master_confirmed,
            });
            if environment_suspect {
                ch.video_resume_pending = ch.current.as_ref().map(|d| d.media.clone());
            }
            if let Some(cur) = ch.current.as_mut() {
                // 已经没有任何解码器在跑：decoder 字段不得继续声称在工作。
                cur.decoder = "none".to_string();
            }
        }
        self.drive(Event::ErrorOccurred);
        let recovered = self.try_fallback(key);
        if recovered
            && matches!(
                self.state.current(),
                State::DegradedStatic | State::Recovering
            )
        {
            self.drive(Event::Recovered);
        } else if !recovered {
            tracing::warn!(
                output = %key,
                state = self.state.current().name(),
                "视频停止且无可用后备画面：保持 DegradedStatic（状态如实反映降级）"
            );
        }
    }

    /// 停摆自愈：重开当前媒体的视频管线（生产者 + 播放器）。
    ///
    /// 返回 `true` = 已重建（新管线的看门狗基线由 `attach_video_player` 重置，
    /// 调用方**不得**再走失败路径）；`false` = 没条件重建（没有可重建的视频媒体 /
    /// 重建预算已用尽 / 重建本身失败）⇒ 调用方按原逻辑降级，不掩盖真实故障。
    ///
    /// 只对**视频**媒体自愈：静态图（png/jpg…）本来就该停在那一帧，给它重开
    /// "播放器"没有意义，直接降级即可（也避免单元测试里的图片通道被误重建）。
    fn try_rebuild_stalled_video(&mut self, key: &str) -> bool {
        let Some((path, latched, rebuilds)) = self.channels.get(key).map(|ch| {
            (
                ch.current.as_ref().map(|d| d.media.clone()),
                ch.dmabuf_latched,
                ch.stall_rebuilds,
            )
        }) else {
            return false;
        };
        let Some(path) = path else {
            return false;
        };
        if media_kind_for_path(&path) != MediaKind::Video || rebuilds >= MAX_STALL_REBUILDS {
            return false;
        }
        if let Some(ch) = self.channels.get_mut(key) {
            ch.stall_rebuilds = ch.stall_rebuilds.saturating_add(1);
        }
        match self.show_video_with(key, &path, latched) {
            Ok(()) => {
                tracing::info!(
                    output = %key,
                    media = %path.display(),
                    attempt = rebuilds + 1,
                    "视频停摆：已自动重建解码管线（暂停/熄屏恢复后的自愈，无需重启服务）"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    output = %key,
                    media = %path.display(),
                    attempt = rebuilds + 1,
                    error = %e.message,
                    "视频停摆：自愈重建失败，按失败路径降级"
                );
                false
            }
        }
    }

    /// 显式拆除通道上的视频管线；返回是否确实拆过。
    ///
    /// [`liuguang_media::gstreamer::GstProducer`] 的 `Drop` 会把管线切到 `NULL`
    /// 并断开 qtdemux 的 pad 回调，这里保证"切回静态图/切换视频"时**立刻**
    /// 触发该收尾，而不是等某个不确定的引用释放时机。
    fn stop_video(&mut self, key: &str) -> bool {
        let had = self
            .channels
            .get_mut(key)
            .map(|ch| {
                // t43：先暂停再拆。预读线程只在帧边界查停止标志，直接 drop 会让
                // `Prefetch::drop` 里的 join 等它把当前这帧解码完（4K60 实测
                // 140–230ms，直接顶穿 set 的 600ms 预算）。pause 后线程 1ms 内停手，
                // join 立刻返回，管线仍在**主线程**析构（见 `Prefetch::returned`）。
                let had = match ch.video.take() {
                    Some(mut video) => {
                        // pause 的返回值在这里无关紧要（拆管线才是目的）：失败的
                        // 话 join 会等多一帧，正确性不受影响。
                        let _ = video.pause();
                        drop(video);
                        true
                    }
                    None => false,
                };
                // T68：拆管线后 PTS 基线必须失效（否则下一次开视频的 PTS
                // 会和旧视频的比较，误判播完一轮）。
                ch.last_video_pts = None;
                ch.paused_since = None;
                ch.paused_video_media = None;
                // 告警属于"这一路视频"，拆管线即作废（否则静态图会带着旧的告警）。
                ch.decode_alert = None;
                had
            })
            .unwrap_or(false);
        if had {
            crate::thumb::trim_heap();
            tracing::debug!(output = %key, "视频管线已拆除（解码线程停止）");
        }
        had
    }

    fn publish_media(&self, key: &str, path: &Path) {
        self.events.publish(
            EventMessage::new(EventKind::MediaChanged)
                .with_output(key)
                .with_media(path.display().to_string()),
        );
    }

    /// 处理一条 IPC 请求（运行时线程执行）。
    pub fn handle(&mut self, request: Request) -> liuguang_ipc::Dispatch {
        // 每次处理请求前先收一次后台扫描消息：IPC 到达频率高于 tick 时也能及时更新索引。
        self.poll_scan();
        // **t46**：记下"有客户端在指挥"，启动首帧据此让路（见 `prime_first_frame`）。
        self.last_request_at = Some(Instant::now());
        let id = request.id.clone();
        match request.method {
            Method::Status => liuguang_ipc::Dispatch::Reply(self.handle_status(id)),
            Method::GetMetrics => liuguang_ipc::Dispatch::Reply(self.handle_metrics(id)),
            Method::ListSources => liuguang_ipc::Dispatch::Reply(self.handle_list_sources(id)),
            Method::ListMedia => {
                liuguang_ipc::Dispatch::Reply(self.handle_list_media(id, request.params))
            }
            Method::GetMedia => {
                liuguang_ipc::Dispatch::Reply(self.handle_get_media(id, request.params))
            }
            // 缩略图可能是"排队解码"：由 handler 自己决定回 Reply / Deferred / Busy。
            Method::GetThumbnail => self.handle_get_thumbnail(id, request.params),
            Method::ScanSource => {
                liuguang_ipc::Dispatch::Reply(self.handle_scan_source(id, request.params))
            }
            Method::PrepareMedia => {
                liuguang_ipc::Dispatch::Reply(self.handle_prepare_media(id, request.params))
            }
            Method::RescanLibrary => liuguang_ipc::Dispatch::Reply(self.handle_rescan_library(id)),
            Method::AddSource => {
                liuguang_ipc::Dispatch::Reply(self.handle_add_source(id, request.params))
            }
            Method::RemoveSource => {
                liuguang_ipc::Dispatch::Reply(self.handle_remove_source(id, request.params))
            }
            // t4：只动索引/列表定义，**不删磁盘文件**（见两个 handler 的文档注释）。
            Method::DeleteMedia => {
                liuguang_ipc::Dispatch::Reply(self.handle_delete_media(id, request.params))
            }
            Method::ListPlaylists => liuguang_ipc::Dispatch::Reply(self.handle_list_playlists(id)),
            Method::UpdatePlaylist => {
                liuguang_ipc::Dispatch::Reply(self.handle_update_playlist(id, request.params))
            }
            Method::DeletePlaylist => {
                liuguang_ipc::Dispatch::Reply(self.handle_delete_playlist(id, request.params))
            }
            Method::SetOutputTransform => {
                liuguang_ipc::Dispatch::Reply(self.handle_set_output_transform(id, request.params))
            }
            Method::SaveConfig => liuguang_ipc::Dispatch::Reply(self.handle_save_config(id)),
            Method::Outputs => liuguang_ipc::Dispatch::Reply(self.handle_outputs(id)),
            Method::SetWallpaper => {
                liuguang_ipc::Dispatch::Reply(self.handle_set(id, request.params))
            }
            Method::Next => liuguang_ipc::Dispatch::Reply(self.handle_next(id, request.params)),
            Method::Pause => liuguang_ipc::Dispatch::Reply(self.handle_pause(id, request.params)),
            Method::Resume => liuguang_ipc::Dispatch::Reply(self.handle_resume(id, request.params)),
            Method::SetPinned => {
                liuguang_ipc::Dispatch::Reply(self.handle_set_pinned(id, request.params))
            }
            Method::Reload => liuguang_ipc::Dispatch::Reply(self.handle_reload(id)),
            Method::Transition => {
                liuguang_ipc::Dispatch::Reply(self.handle_transition(id, request.params))
            }
            // t69：静态壁纸已接线（暂停播放列表并显示单张图片 / 清除后恢复播放）。
            // 之前这里是 `UNSUPPORTED_METHOD`，客户端（TUI t70）据此降级；现在必须
            // 走真实处理器，且 `Method::awaits_handler()` 已对两者返回 false。
            Method::SetStaticWallpaper => {
                liuguang_ipc::Dispatch::Reply(self.handle_set_static_wallpaper(id, request.params))
            }
            Method::ClearStaticWallpaper => {
                liuguang_ipc::Dispatch::Reply(self.handle_clear_static_wallpaper(id))
            }
            Method::Subscribe => {
                let ack = Response::ok(
                    id,
                    serde_json::json!({"accepted": true, "stream": "events"}),
                );
                let events = self.events.subscribe();
                liuguang_ipc::Dispatch::Subscribe { ack, events }
            }
        }
    }

    fn handle_status(&self, id: String) -> Response {
        if self.state_machine_errors() > 0 {
            // 正常运行时恒为 0；非 0 说明有"半接线"，让每次 status 查询都能看见。
            tracing::warn!(
                total = self.state_machine_errors,
                "顶层状态机累计拒绝过非法转移（详见更早的 warn 日志/事件）"
            );
        }
        // 状态**单一来源**：
        //  - `channels[].state` 只由 `Self::channel_state_label` 产出（通道事实）；
        //  - 顶层 `state` 只由 `StateMachine` 产出（全局权威）；
        //  - 顶层其余字段只描述 `Self::primary_channel()` 这一个通道。
        // 三者的一致性由 `state_contradictions()` 不变式 + `tick()` 的告警守卫保证。
        let channels: Vec<ChannelStatus> = self
            .channels
            .iter()
            .map(|(key, ch)| ChannelStatus {
                output: key.clone(),
                state: Self::channel_state_label(ch).to_string(),
                media: ch.current.as_ref().map(|d| d.media.display().to_string()),
                mode: ch.current.as_ref().map(|d| d.mode_str.clone()),
                paused: ch.paused,
                pinned: ch.pinned,
                pause_reason: if ch.paused {
                    self.pause_reason_of(key).map(str::to_string)
                } else {
                    None
                },
                decoder: ch
                    .current
                    .as_ref()
                    .map(|d| d.decoder.clone())
                    .unwrap_or_else(|| "none".into()),
                video: ch.video.as_ref().map(|v| {
                    let st = v.video_stats();
                    // 直通状态是**结构性事实**（来自生产者），不是字符串推断。
                    let dbuf = v.dmabuf_state().unwrap_or_default();
                    liuguang_ipc::VideoStatsStatus {
                        presented: st.presented,
                        dropped_late: st.dropped_late,
                        dropped_queue_full: st.dropped_queue_full,
                        skipped: st.skipped,
                        queue_len: st.queue_len,
                        source_fps: st.source_fps,
                        last_pts_ms: st.last_pts.map(|d| d.as_millis() as u64),
                        present_ok: ch.presented_ok,
                        present_failures: ch.present_failures,
                        present_failures_consecutive: ch.present_failures_consecutive,
                        is_dmabuf: dbuf.is_dmabuf,
                        dmabuf_fallbacks: dbuf.fallbacks,
                        dmabuf_keepalive_len: dbuf.keepalive_len,
                        decode_alert: ch.decode_alert.clone(),
                    }
                }),
            })
            .collect();
        // 顶层字段统一取自 `primary_channel()`（单一规则；旧实现取 channels.first()，
        // 会去描述一个没有内容的通道，导致顶层 media/mode/decoder 全空）。
        let primary = self.primary_channel();
        let primary_status =
            primary.and_then(|(key, _)| channels.iter().find(|c| &c.output == key));
        let result = StatusResult {
            state: self.state.current().name().to_string(),
            output: primary_status.map(|c| c.output.clone()),
            media: primary_status.and_then(|c| c.media.clone()),
            mode: primary_status.and_then(|c| c.mode.clone()),
            paused: primary_status.map(|c| c.paused).unwrap_or(false),
            display_backend: self.backend_name.clone(),
            decoder: primary_status
                .map(|c| c.decoder.clone())
                .unwrap_or_else(|| "none".into()),
            uptime_seconds: self.started.elapsed().as_secs(),
            channels,
            fullscreen: self.fullscreen_info.as_ref().map(|t| FullscreenInfo {
                app_id: t.app_id.clone(),
                title: t.title.clone(),
                outputs: t.outputs.clone(),
                paused_channels: self.fullscreen_paused.len(),
            }),
            commit: self.backend.commit_stats().map(|c| DisplayCommitStatus {
                commit_ok: c.commit_ok,
                retried_after_busy: c.retried_after_busy,
                legacy_fallback: c.legacy_fallback,
                failed: c.failed,
                commit_success_total: c.commit_success_total(),
                attempts: c.attempts(),
            }),
            master: self.backend.master_status().map(|m| DisplayMasterStatus {
                master: m.master,
                acquire_result: m.acquire_result.clone(),
                holders: m.holders.clone(),
                remedy_hint: m.remedy_hint(),
            }),
            // **T68**：当前播放源（`"directory"` | `"playlist:<id>"`）。
            // 取自通道事实（`ChannelState::source_label`），不是配置推断。
            // t69 的静态壁纸会插到最前面（`"static"` 优先级最高）。
            playback_source: self.playback_source(),
            // **t94 / 审核 P2-6**：把**实际记录**的降级原因接进 status。
            // 判据不是"有没有待恢复的视频"（那只表示恢复意图），而是收敛当时
            // 存下的原因文本 + 是否确认持有 master；没记录就明说"未记录"。
            // 语义分层（都不是"错误"，而是"用户该知道的事实"）：
            // ① 降级收敛原因（最有解释力，优先）；
            // ② 当前视频的解码告警（如 8K H.264 只能软解）；
            // ③ 媒体库因体积上限跳过的文件（"我的 930MB 素材去哪了"）。
            state_hint: {
                let reason = self
                    .channels
                    .values()
                    .find_map(|c| c.degraded_reason.as_ref());
                let hint = degraded_reason_hint(self.state.current().name(), reason);
                if !hint.is_empty() {
                    Some(hint)
                } else if let Some(alert) =
                    self.channels.values().find_map(|c| c.decode_alert.clone())
                {
                    Some(alert)
                } else if !self.game_paused.is_empty() {
                    let game = self
                        .game
                        .hits()
                        .first()
                        .map(|h| format!("（{}）", h.name))
                        .unwrap_or_default();
                    Some(format!("检测到游戏正在运行{game}，已自动暂停视频供给"))
                } else if !self.gpu_paused.is_empty() {
                    let usage = self
                        .gpu
                        .usage()
                        .map(|u| format!("（{:.0}%）", u.percent()))
                        .unwrap_or_default();
                    Some(format!("显存占用较高{usage}，已自动暂停视频供给"))
                } else if !self.battery_paused.is_empty() {
                    Some("当前使用电池供电，已自动暂停动态壁纸以节省续航".to_string())
                } else if self.media.oversize_skipped > 0 {
                    let limit_mb = self
                        .library()
                        .map(|l| l.max_file_bytes / (1024 * 1024))
                        .unwrap_or(0);
                    Some(format!(
                        "{} 个素材因超过单文件上限（[library].max_file_bytes = {limit_mb} MB）被跳过，\
                         可在 list_media 的 skipped_examples 里看到具体文件",
                        self.media.oversize_skipped
                    ))
                } else {
                    None
                }
            },
        };
        ok_response(id, &result)
    }

    /// 媒体库配置（缺省时 `None`：不提供媒体库，旧行为不变）。
    fn library(&self) -> Option<&liuguang_core::library::LibraryConfig> {
        self.config.library.as_ref()
    }

    /// 生效的来源集合：配置来源（含 `[slideshow].directory` 并入）+ 运行时 `add_source`
    /// − 运行时 `remove_source`；同路径去重（`source_id` = 规范化后的绝对路径）。
    fn effective_sources(&self) -> Vec<EffectiveSource> {
        fn push(out: &mut Vec<EffectiveSource>, kind: &str, path: PathBuf, recursive: bool) {
            let id = std::fs::canonicalize(&path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .into_owned();
            if out.iter().any(|s| s.id == id) {
                return;
            }
            out.push(EffectiveSource {
                id,
                kind: kind.to_string(),
                path,
                recursive,
            });
        }
        let mut out: Vec<EffectiveSource> = Vec::new();
        if let Some(cfg) = self.library() {
            for dir in &cfg.image_dirs {
                push(&mut out, "image", cfg.abs_dir(dir), cfg.recursive);
            }
            for dir in &cfg.video_dirs {
                push(&mut out, "video", cfg.abs_dir(dir), cfg.recursive);
            }
            // 运行期新增并**已落盘**的来源（`[[library.sources]]`，T25-F1）：
            // 它们带自己的 kind/recursive，不能塞进上面的两串 dirs。
            for src in &cfg.sources {
                push(&mut out, &src.kind, cfg.abs_dir(&src.path), src.recursive);
            }
        }
        // 旧 `[slideshow].directory` **仍生效**并并入来源集合（去重，t22 验收）。
        let legacy = self.config.slideshow.directory.clone();
        if !legacy.as_os_str().is_empty() {
            push(&mut out, "all", legacy, true);
        }
        // 运行期新增：`remove_source` **可以**即时抹掉（它本来只活在内存里）。
        for (kind, path, recursive) in &self.added_sources {
            let id = std::fs::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .into_owned();
            if self.removed_sources.contains(&id) {
                continue;
            }
            push(&mut out, kind, path.clone(), *recursive);
        }
        // **配置里声明的来源在保存前仍然生效**：`remove_source` 只是登记了"待删除"，
        // 真正生效靠 `save_config` 重写配置。
        //
        // 为什么必须这样（T37）：旧实现在这里 `retain` 掉它们，于是
        // ① `list_sources` 立刻少一条，用户以为删掉了（其实重启就回来）；
        // ② TUI 那条"删除后仍存在 ⇒ 提示运行时删除无效"的分支**永远不可达**。
        // 让"仍在生效"如实可见，才谈得上准确诊断。
        out
    }

    /// 启动一次后台扫描；返回新代次 `scan_id`。
    ///
    /// 旧的（进行中的）扫描会被取消——它的结果因代次不匹配而被丢弃。
    ///
    /// # Errors
    ///
    /// 未配置 `[library]`、配置校验失败或线程创建失败时返回 [`ErrorBody`]。
    fn start_scan(&mut self) -> Result<u64, ErrorBody> {
        // 先算好线程需要的全部 owned 数据（不把 `self` 借进后台线程）。
        let (media, extensions, sources) = {
            let Some(cfg) = self.library() else {
                return Err(
                    ErrorBody::new(ErrorKind::InvalidParams, LIBRARY_NOT_CONFIGURED_HINT)
                        .with_detail_code("NOT_CONFIGURED"),
                );
            };
            let media = cfg.validate().map_err(|(field, e)| {
                ErrorBody::new(ErrorKind::InvalidParams, format!("{field}: {}", e.message))
                    .with_detail_code("INVALID_CONFIG")
            })?;
            let sources: Vec<(String, PathBuf, bool)> = self
                .effective_sources()
                .into_iter()
                .filter(|s| s.path.is_dir())
                .map(|s| {
                    let canonical = std::fs::canonicalize(&s.path).unwrap_or(s.path);
                    (s.id, canonical, s.recursive)
                })
                .collect();
            (media, cfg.extensions(), sources)
        };
        self.next_scan_id += 1;
        let scan_id = self.next_scan_id;
        if let Some(handle) = self.media.running.take() {
            handle.cancel.store(true, Ordering::Relaxed);
            tracing::info!(
                old = handle.scan_id,
                new = scan_id,
                "新扫描取代进行中的扫描（旧结果将被丢弃）"
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let spec = crate::scan::ScanSpec::new(
            scan_id,
            media,
            sources,
            extensions,
            MAX_SCAN_ENTRIES,
            Arc::clone(&cancel),
        );
        if let Err(e) = crate::scan::spawn(spec, self.scan_tx.clone()) {
            return Err(ErrorBody::new(
                ErrorKind::Internal,
                format!("无法启动扫描线程: {e}"),
            ));
        }
        self.media.running = Some(ScanHandle { scan_id, cancel });
        tracing::info!(
            scan_id,
            "后台扫描已启动（结果经 SCAN_PROGRESS/SCAN_FINISHED 事件返回）"
        );
        Ok(scan_id)
    }

    /// 取回后台扫描的消息并发布事件（**非阻塞**，运行时节拍里调用）。
    fn poll_scan(&mut self) {
        loop {
            match self.scan_rx.try_recv() {
                Ok(crate::scan::ScanMsg::Progress { scan_id, found }) => {
                    if self.media.running.as_ref().map(|h| h.scan_id) == Some(scan_id) {
                        self.events.publish(
                            EventMessage::new(EventKind::ScanProgress)
                                .with_scan(scan_id, found, false),
                        );
                    }
                }
                Ok(crate::scan::ScanMsg::Finished(outcome)) => {
                    let current = self.media.running.as_ref().map(|h| h.scan_id);
                    if current != Some(outcome.scan_id) {
                        tracing::debug!(
                            scan_id = outcome.scan_id,
                            "丢弃过期扫描结果（已被更新代次取代或从未登记）"
                        );
                        continue;
                    }
                    self.media.running = None;
                    self.media.scan_id = outcome.scan_id;
                    self.media.generated_unix_ms = liuguang_ipc::protocol::unix_ms();
                    self.media.truncated = outcome.truncated;
                    self.media.errors = outcome.errors.clone();
                    self.media.oversize_skipped = outcome.oversize_skipped;
                    self.media.oversize_examples = outcome.oversize_examples.clone();
                    let found = outcome.found;
                    let summary = format!(
                        "found={found} dirs={} files={} elapsed_ms={} truncated={} timed_out={} cancelled={} errors={}",
                        outcome.dirs_visited,
                        outcome.files_considered,
                        outcome.elapsed.as_millis(),
                        outcome.truncated,
                        outcome.timed_out,
                        outcome.cancelled,
                        outcome.error_count
                    );
                    if outcome.oversize_skipped > 0 {
                        tracing::warn!(
                            scan_id = outcome.scan_id,
                            skipped = outcome.oversize_skipped,
                            examples = ?outcome.oversize_examples,
                            "有素材因超过 [library].max_file_bytes 被跳过（可调大上限或用 prepare_media 转码）"
                        );
                    }
                    let clean = outcome.is_clean();
                    self.media.items = outcome.items;
                    if clean {
                        tracing::info!(scan_id = outcome.scan_id, %summary, "扫描完成");
                    } else if outcome.cancelled {
                        tracing::info!(scan_id = outcome.scan_id, %summary, "扫描被取消");
                    } else {
                        tracing::warn!(scan_id = outcome.scan_id, %summary, "扫描有界结束（超时/截断）");
                    }
                    self.events.publish(
                        EventMessage::new(EventKind::ScanFinished)
                            .with_scan(outcome.scan_id, found, true)
                            .with_errors(outcome.errors)
                            .with_message(summary),
                    );
                    if outcome.timed_out {
                        self.events.publish(
                            EventMessage::new(EventKind::Error)
                                .with_code("SCAN_TIMEOUT")
                                .with_message(format!(
                                    "扫描超时（已发现 {found} 条，返回部分结果）"
                                )),
                        );
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    /// 发出待发的 `WALLPAPER_APPLIED` 事件（**在受理响应之后**的 tick 里发）。
    fn flush_pending_applied(&mut self) {
        let pending = std::mem::take(&mut self.pending_applied);
        for item in pending {
            let mut event = EventMessage::new(EventKind::WallpaperApplied)
                .with_request_id(item.request_id)
                .with_output(item.output)
                .with_media(item.path)
                .with_ok(item.ok);
            if let Some(error) = item.error {
                event = event.with_error(error);
            }
            self.events.publish(event);
        }
    }

    /// 取消进行中的扫描（daemon 退出时用，避免后台线程悬空）。
    pub fn cancel_scans(&mut self) {
        if let Some(handle) = self.media.running.take() {
            handle.cancel.store(true, Ordering::Relaxed);
            tracing::info!(scan_id = handle.scan_id, "退出：已请求取消后台扫描");
        }
    }

    /// `list_sources`：daemon 侧规范化后的来源集合（含缺失项标记）。
    ///
    /// 来源 = `[library].image_dirs`/`video_dirs` + 旧 `[slideshow].directory`（并入去重）
    /// + 运行时 `add_source` − 运行时 `remove_source`。
    fn handle_list_sources(&mut self, id: String) -> Response {
        self.sync_disk_library_if_missing();
        let Some(cfg) = self.library() else {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "NOT_CONFIGURED",
                LIBRARY_NOT_CONFIGURED_HINT,
            );
        };
        let media = match cfg.validate() {
            Ok(m) => m,
            Err((field, e)) => {
                return err_response_code(
                    id,
                    ErrorKind::InvalidParams,
                    "INVALID_CONFIG",
                    &format!("{field}: {}", e.message),
                );
            }
        };
        let sources: Vec<liuguang_ipc::protocol::MediaSource> = self
            .effective_sources()
            .into_iter()
            .map(|s| liuguang_ipc::protocol::MediaSource {
                available: s.path.is_dir(),
                name: s
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                kind: s.kind,
                id: s.id,
            })
            .collect();
        let (_, missing_dirs) = cfg.split_sources(Some(self.config.slideshow.directory.as_path()));
        for dir in &missing_dirs {
            tracing::warn!(
                dir = %dir.display(),
                "媒体来源目录不存在或不是目录：已跳过（不阻止启动）"
            );
        }
        ok_response(
            id,
            &liuguang_ipc::protocol::ListSourcesResult {
                root: media.root().to_string_lossy().into_owned(),
                sources,
                missing: missing_dirs.len(),
            },
        )
    }

    /// `list_media`：从**后台扫描产物**分页读取（不再同步扫盘）。
    ///
    /// 语义（t22 异步扫描契约）：
    /// - 索引由[后台扫描](#method.start_scan)构建，本方法只做过滤/排序/分页，O(条目数) 且不碰磁盘；
    /// - 首次调用时若还没有任何索引，会**启动**一次扫描并立即返回空页 +
    ///   `scanning=true`（不阻塞运行时线程）；客户端应在收到 `SCAN_FINISHED` 后重拉；
    /// - `truncated`/`scanning`/`scan_id`/`generated_unix_ms` 如实反映索引状态。
    fn handle_list_media(&mut self, id: String, params: serde_json::Value) -> Response {
        self.sync_disk_library_if_missing();
        let params: liuguang_ipc::protocol::ListMediaParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        if self.library().is_none() {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "NOT_CONFIGURED",
                LIBRARY_NOT_CONFIGURED_HINT,
            );
        }
        // 先收一次后台消息（IPC 到达时也可能刚好有结果），再判断是否需要起扫描。
        self.poll_scan();
        if self.media.generated_unix_ms == 0 && self.media.running.is_none() {
            match self.start_scan() {
                Ok(scan_id) => tracing::info!(scan_id, "首次 list_media：已启动后台扫描"),
                Err(body) => return Response::err(id, body),
            }
        }
        // 过滤（引用，不复制全量条目）→ 排序 → 只把当前页 copy 出去。
        let mut hits: Vec<&MediaItem> = self.media.items.iter().collect();
        if let Some(source) = &params.source_id {
            hits.retain(|i| &i.source_id == source);
        }
        // 类型过滤：**容错 + 校验**（T39-F1）。
        // 未知取值必须显式报错：旧行为把它当精确比较 ⇒ `ok=true` + `total=0`，
        // 即"无效参数被当成合法空结果"，用户看到的是"媒体库是空的"。
        match liuguang_ipc::protocol::parse_media_filter(params.filter.as_deref()) {
            Ok(liuguang_ipc::protocol::MediaFilterSpec::Any) => {}
            Ok(liuguang_ipc::protocol::MediaFilterSpec::Kind(kind)) => {
                hits.retain(|i| i.kind == kind);
            }
            Err(msg) => {
                return err_response_code(id, ErrorKind::InvalidParams, "INVALID_FILTER", &msg);
            }
        }
        if let Some(q) = &params.search {
            let q = q.to_lowercase();
            hits.retain(|i| i.name.to_lowercase().contains(&q));
        }
        match params.sort.as_deref() {
            Some("size") => hits.sort_by_key(|i| i.size_bytes),
            Some("modified") => hits.sort_by_key(|i| std::cmp::Reverse(i.modified_unix)),
            _ => hits.sort_by(|a, b| a.name.cmp(&b.name)),
        }
        let total = hits.len();
        let page_size = params
            .page_size
            .clamp(1, liuguang_ipc::protocol::MAX_MEDIA_PAGE_SIZE);
        let start = params.page.saturating_mul(page_size).min(total);
        let items: Vec<MediaItem> = hits
            .into_iter()
            .skip(start)
            .take(page_size)
            .cloned()
            .collect();
        ok_response(
            id,
            &ListMediaResult {
                items,
                total,
                page: params.page,
                page_size,
                truncated: self.media.truncated,
                scanning: self.media.running.is_some(),
                scan_id: self.media.scan_id,
                generated_unix_ms: self.media.generated_unix_ms,
                skipped_oversize: self.media.oversize_skipped as usize,
                skipped_examples: self.media.oversize_examples.clone(),
            },
        )
    }

    /// `scan_source`：启动一次后台扫描（**异步**；结果经 `SCAN_*` 事件返回）。
    fn handle_scan_source(&mut self, id: String, params: serde_json::Value) -> Response {
        self.sync_disk_library_if_missing();
        let params: ScanSourceParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        if self.library().is_none() {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "NOT_CONFIGURED",
                LIBRARY_NOT_CONFIGURED_HINT,
            );
        }
        if let Some(source_id) = &params.source_id {
            let known = self.effective_sources().iter().any(|s| &s.id == source_id);
            if !known {
                return err_response_code(
                    id,
                    ErrorKind::FileUnavailable,
                    "NOT_FOUND",
                    &format!("未知来源: {source_id}"),
                );
            }
        }
        let superseded = self.media.running.is_some();
        match self.start_scan() {
            Ok(scan_id) => ok_response(
                id,
                &ScanSourceResult {
                    accepted: true,
                    scan_id,
                    superseded_running: superseded,
                },
            ),
            Err(body) => Response::err(id, body),
        }
    }

    /// `rescan_library`：按当前配置重扫全部来源。
    ///
    /// 存在的理由：`list_media` **只在索引还空的时候**才自动起扫描（那是首屏语义），
    /// 所以"往目录里丢进新文件"之后刷新列表是看不到的——用户只能重启守护进程。
    /// 这里给客户端一个明确的"现在重新读一遍磁盘"入口；由于 `start_scan` 会重建索引，
    /// 磁盘上已删除的条目也会因此消失。
    fn handle_rescan_library(&mut self, id: String) -> Response {
        self.sync_disk_library_if_missing();
        if self.library().is_none() {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "NOT_CONFIGURED",
                LIBRARY_NOT_CONFIGURED_HINT,
            );
        }
        let superseded = self.media.running.is_some();
        match self.start_scan() {
            Ok(scan_id) => ok_response(
                id,
                &liuguang_ipc::protocol::RescanLibraryResult {
                    scan_id,
                    superseded_running: superseded,
                },
            ),
            Err(body) => Response::err(id, body),
        }
    }

    /// `get_media`：按 id 取单条（同样过安全边界）。
    fn handle_get_media(&self, id: String, params: serde_json::Value) -> Response {
        let params: liuguang_ipc::protocol::GetMediaParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        let Some(cfg) = self.library() else {
            return err_response(id, ErrorKind::InvalidParams, "未配置 [library]");
        };
        let media = match cfg.validate() {
            Ok(m) => m,
            Err((field, e)) => {
                return err_response(
                    id,
                    ErrorKind::InvalidParams,
                    &format!("{field}: {}", e.message),
                );
            }
        };
        let resolved = match media.resolve(&params.id) {
            Ok(p) => p,
            Err(e) => {
                return err_response(id, ErrorKind::InvalidParams, &e.to_string());
            }
        };
        let Ok(meta) = std::fs::metadata(&resolved) else {
            return err_response(id, ErrorKind::FileUnavailable, "文件不可读");
        };
        let ext = resolved
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let kind = if liuguang_core::library::DEFAULT_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            "image"
        } else {
            "video"
        };
        let item = liuguang_ipc::protocol::MediaItem {
            id: resolved.to_string_lossy().into_owned(),
            source_id: resolved
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            name: resolved
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: resolved.to_string_lossy().into_owned(),
            kind: kind.to_string(),
            size_bytes: meta.len(),
            modified_unix: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            // 与 `list_media` 的扫描结果保持同一口径：详情来自头部探测。
            details: crate::probe::probe(&resolved, kind),
        };
        // t15/t37：带上"这条输出能不能直接播"的分类。判据必须与 `set_wallpaper`
        // 完全一致（同一份 consumer），否则会出现"读接口说 playable、一播却要转码"。
        // 位置深不值得单独探测 —— 这里复用 `classify_path`（图片直接 Playable，
        // 视频才读容器头补位深/色度）。
        let consumer = self.consumer_for_request(None);
        let caps = self.hardware_caps().with_consumer(consumer);
        let (_, verdict) = crate::compat::classify_path(&resolved, kind, &caps);
        ok_response(
            id,
            &liuguang_ipc::protocol::GetMediaResult {
                item,
                compat: Some(verdict.to_info()),
            },
        )
    }

    /// `get_thumbnail`：为媒体项抽一张封面（视频解码首帧，图片不解码）。
    ///
    /// 安全边界与 `set_wallpaper` 完全一致：路径必须落在
    /// `media.allowed_roots` / `slideshow.directory` 之内。**缩略图不是特权操作，
    /// 但它是任意路径读取的入口**，所以校验一点不能省。
    ///
    /// `at_ms`（t4）：GUI 悬停动效要"视频第 N 毫秒"的多点帧。省略 / `0` = 首帧
    /// （走 `liuguang-media` 的既有首帧路径），图片忽略该字段，`> 0` 的视频才 seek。
    /// `get_thumbnail`：两级缓存命中就立刻回，未命中交给后台串行解码（t39）。
    ///
    /// **不在运行时线程解码**：4K/8K 一帧要几百毫秒到一秒多 CPU，同步做会把
    /// 渲染与其它请求一起卡住（真机：GUI 建 38 张卡 ⇒ CPU 100% 持续 50s、
    /// 另一条连接的 `status` 要 1.1s）。现在返回 [`liuguang_ipc::Dispatch`]：
    /// 命中 → `Reply`，排队/在解 → `Deferred`（连接线程等，运行时线程继续），
    /// 队列满 → `Busy` 错误（可操作文案，调用方稍后重试即可）。
    fn handle_get_thumbnail(
        &self,
        id: String,
        params: serde_json::Value,
    ) -> liuguang_ipc::Dispatch {
        let params: liuguang_ipc::protocol::GetThumbnailParams =
            match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    return liuguang_ipc::Dispatch::Reply(err_response(
                        id,
                        ErrorKind::InvalidParams,
                        &e.to_string(),
                    ));
                }
            };
        let resolved = match self.check_media_path(std::path::Path::new(&params.id)) {
            Ok(p) => p,
            Err(body) => return liuguang_ipc::Dispatch::Reply(Response::err(id, body)),
        };
        let max_w = params.max_width.unwrap_or(320).clamp(16, 1920);
        let max_h = params.max_height.unwrap_or(200).clamp(16, 1080);
        let at_ms = params.at_ms.unwrap_or(0);
        let request = crate::thumb::Request {
            path: resolved,
            at_ms,
            max_w,
            max_h,
        };
        match self.thumbs.submit(request, &id) {
            crate::thumb::Submit::Ready(resp) => liuguang_ipc::Dispatch::Reply(resp),
            crate::thumb::Submit::Deferred(rx) => liuguang_ipc::Dispatch::Deferred { rx },
            crate::thumb::Submit::Busy(resp) => liuguang_ipc::Dispatch::Reply(resp),
        }
    }

    /// `get_metrics`：显示侧提交计数 + master 状态 + 逐通道对账计数。
    /// `add_source`：运行时追加来源（**只改运行时**；落盘走 `save_config`）。
    ///
    /// 校验链（安全边界在 daemon）：`kind` 合法 → **服务器绝对路径** →
    /// `check_dir_in_root`（拒绝 `..`/根外）→ 必须存在且是目录。
    fn handle_add_source(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: AddSourceParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        if !matches!(params.kind.as_str(), "image" | "video") {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "INVALID_PARAMS",
                &format!("kind 必须是 image 或 video（收到 {}）", params.kind),
            );
        }
        let dir = Path::new(&params.path);
        if !dir.is_absolute() {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "INVALID_PARAMS",
                &format!("必须是服务器绝对路径（收到 {}）", params.path),
            );
        }
        if dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "PATH_OUTSIDE_ROOT",
                &format!("路径不允许包含 ..（收到 {}）", params.path),
            );
        }
        let is_user = self.persona == liuguang_core::persona::Persona::User;
        let pre_resolved = if is_user {
            let Ok(canon) = std::fs::canonicalize(dir) else {
                return err_response_code(
                    id,
                    ErrorKind::FileUnavailable,
                    "NOT_FOUND",
                    &format!("目录不存在或无法访问: {}", dir.display()),
                );
            };
            if !canon.is_dir() {
                return err_response_code(
                    id,
                    ErrorKind::FileUnavailable,
                    "NOT_FOUND",
                    &format!("目录不存在或不是目录: {}", canon.display()),
                );
            }
            self.ensure_user_library_covers(&canon);
            Some(canon)
        } else {
            None
        };
        let media = {
            let Some(cfg) = self.library() else {
                return err_response_code(
                    id,
                    ErrorKind::InvalidParams,
                    "NOT_CONFIGURED",
                    LIBRARY_NOT_CONFIGURED_HINT,
                );
            };
            match cfg.validate() {
                Ok(m) => m,
                Err((field, e)) => {
                    return err_response_code(
                        id,
                        ErrorKind::InvalidParams,
                        "INVALID_CONFIG",
                        &format!("{field}: {}", e.message),
                    );
                }
            }
        };
        let resolved = if let Some(canon) = pre_resolved {
            match media.check_dir_in_root(dir) {
                Ok(p) => p,
                Err(e) => {
                    return err_response_code(
                        id,
                        ErrorKind::InvalidParams,
                        e.code.as_str(),
                        &e.message,
                    );
                }
            };
            canon
        } else {
            match media.check_dir_in_root(dir) {
                Ok(p) => p,
                Err(e) => {
                    return err_response_code(
                        id,
                        ErrorKind::InvalidParams,
                        e.code.as_str(),
                        &e.message,
                    );
                }
            }
        };
        if !resolved.is_dir() {
            return err_response_code(
                id,
                ErrorKind::FileUnavailable,
                "NOT_FOUND",
                &format!("目录不存在或不是目录: {}", resolved.display()),
            );
        }
        let source_id = std::fs::canonicalize(&resolved)
            .unwrap_or(resolved.clone())
            .to_string_lossy()
            .into_owned();
        if self.effective_sources().iter().any(|s| s.id == source_id) {
            return ok_response(
                id,
                &AddSourceResult {
                    source_id,
                    created: false,
                },
            );
        }
        self.removed_sources.retain(|id| id != &source_id);
        self.added_sources
            .push((params.kind, resolved, params.recursive));
        ok_response(
            id,
            &AddSourceResult {
                source_id,
                created: true,
            },
        )
    }

    // ===== t15：自动兼容（能力判定 + 后台转码缓存） =====

    /// 本机解码能力（构造期探测一次，这里只是拿快照）。
    fn hardware_caps(&self) -> crate::compat::HardwareCaps {
        self.caps.clone()
    }

    /// 转码任务规格（**按输出的实际解码路径**定产物，t37）。
    ///
    /// `consumer` 必须来自 [`Self::playback_consumer`]（判定与播放共用的那一个）：
    /// 转码产物要能在**这条输出**上直接播，不能用设备级探测。
    fn transcode_spec(
        &self,
        path: &Path,
        duration_ms: Option<u64>,
        consumer: crate::compat::DecodeConsumer,
    ) -> Result<crate::transcode::JobSpec, ErrorBody> {
        let profile = self.transcode.profile_for(consumer);
        let key =
            crate::transcode::cache_key(path, &profile, profile.max_width).ok_or_else(|| {
                ErrorBody::new(
                    ErrorKind::Internal,
                    format!(
                        "无法读取 {} 的元数据，不能计算转码缓存 key（文件可能刚被删除）",
                        path.display()
                    ),
                )
            })?;
        Ok(crate::transcode::JobSpec {
            source: path.to_path_buf(),
            cache_path: crate::transcode::cache_path_for(&key),
            key,
            duration_ms,
            // 仅 e2e 用：只转前 N 秒，证明链路通了即可（生产不设这个变量）。
            limit_seconds: std::env::var("LIUGUANG_TRANSCODE_LIMIT_SECONDS")
                .ok()
                .and_then(|value| value.trim().parse().ok()),
            profile,
        })
    }

    /// 播放门禁：硬件解不动的素材要么**换成缓存路径**、要么回 `NEEDS_PREPARATION`。
    ///
    /// 三条出口：
    /// - `Playable`（或本机没有兼容性问题）→ 原路径；
    /// - `NeedsConversion` 且缓存就绪 → 缓存路径（播放走缓存）；
    /// - `NeedsConversion` 且缓存没就绪 → **登记自动播放** + 入队转码 + 回
    ///   `NEEDS_PREPARATION`（带 `detail_code`，不是失败）。
    ///
    /// `[media].transcode = "off"` 时**不做任何新拦截**：保持 t6 的行为
    /// （开屏告警 + 既有的明确拒绝），让用户可以显式选择"关了就别管我"。
    fn ensure_playable_video(&mut self, key: &str, path: PathBuf) -> Result<PathBuf, ErrorBody> {
        let consumer = self.playback_consumer(key);
        self.ensure_playable_video_with(key, path, consumer)
    }

    /// `[media].video_vaapi`（含 `LIUGUANG_VIDEO_VAAPI` 覆盖）是否允许 VA-API 零拷贝。
    ///
    /// 与 [`Self::open_vaapi_producer`] 的第一条判据逐字一致：两处必须同步，
    /// 否则"判定用的路径"与"播放用的路径"会再次分叉（t33）。
    fn vaapi_setting_enabled(&self) -> bool {
        vaapi_setting_enabled_in(&self.config)
    }

    /// 这个输出**此刻实际**会用的解码路径（t33）。
    ///
    /// 构造期探测的 `caps.consumer` 只回答"这台机器大体上能不能硬解"；运行时还有
    /// 三条路会把它推翻，而且都能在 `set_wallpaper` 之前就知道：
    /// 没编 `vaapi` feature、设置项把 VA-API 关掉、后端不支持 DMA-BUF 扫描输出。
    /// 判据必须与 [`Self::open_vaapi_producer`] 对齐——否则会出现 t28 F2 的现场：
    /// 判 playable 的 `.mkv`，开屏时被 GStreamer 的 `qtdemux` 拒绝
    /// （`atom has bogus size`），用户看到的是 `UNSUPPORTED_CODEC`。
    fn playback_consumer(&self, key: &str) -> crate::compat::DecodeConsumer {
        let latched = self.channels.get(key).is_some_and(|ch| ch.dmabuf_latched);
        let consumer = if latched {
            crate::compat::DecodeConsumer::GStreamer
        } else {
            crate::compat::DecodeConsumer::for_playback(
                cfg!(feature = "vaapi"),
                self.vaapi_setting_enabled(),
                self.backend.supports_dmabuf_scanout(),
                self.caps.consumer,
            )
        };
        tracing::debug!(
            output = key,
            consumer = consumer.as_str(),
            latched,
            "该输出实际使用的解码路径（判定与播放共用）"
        );
        consumer
    }

    /// 读接口（`get_media` / `prepare_media`）该用的 consumer。
    ///
    /// 请求里带输出名就用它（`Some`，目前协议没有该字段，留给后续扩展）；
    /// 不带时用**默认输出**（与 `set_wallpaper` 省略 output 时同一条通道）。
    /// 这样 `get_media` 的 `compat` 与随后 `set_wallpaper` 的结果才是同一份判断（t37 F1）。
    fn consumer_for_request(&self, output: Option<&str>) -> crate::compat::DecodeConsumer {
        let key = match output {
            Some(name) if self.channels.contains_key(name) => name.to_string(),
            _ => self.resolve_channel(output).unwrap_or_default(),
        };
        self.playback_consumer(&key)
    }

    /// [`Self::ensure_playable_video`] 的显式版本：按**调用方给定的**解码路径判定。
    ///
    /// 播放路径在运行中才发现"VA-API 这一路没成"（`open_vaapi_producer` 返回
    /// `Ok(None)`）时，用 `GStreamer` 再判一次：容器打不开就转码/回
    /// `NEEDS_PREPARATION`，绝不让 `qtdemux` 的容器错误冒充解码错误。
    fn ensure_playable_video_with(
        &mut self,
        key: &str,
        path: PathBuf,
        consumer: crate::compat::DecodeConsumer,
    ) -> Result<PathBuf, ErrorBody> {
        use crate::compat::Compatibility;

        // 缓存目录里的文件是我们自己的产物：不再递归转码。
        // 否则「转码完成 → 自动播放缓存 → 缓存自己又被判 needs_conversion（缓存损坏 /
        // 探不出格式）→ 再排一次转码」会无限排队，生成"缓存的缓存"。缓存坏了就如实报
        // 解码错误，由用户决定换素材还是清缓存目录。
        if path.starts_with(crate::compat::cache_dir()) {
            // 缓存是我们自己的产物，但可能是**另一种构型**留下的：VA-API 产 `.mkv`
            // （H.265），纯 GStreamer 产 `.mp4`（H.264）。如果这次实际走的是 GStreamer，
            // 它打不开 `.mkv`——如实说清楚，别让 `qtdemux` 的 `atom has bogus size`
            // 冒充解码错误（t28 F2 / t33）。
            if consumer == crate::compat::DecodeConsumer::GStreamer
                && crate::compat::Container::from_source_path(&path)
                    == Some(crate::compat::Container::Mkv)
            {
                return Err(ErrorBody::new(
                    ErrorKind::UnsupportedCodec,
                    format!(
                        "转码缓存 {} 是 VA-API 构型的产物（Matroska/H.265），当前解码路径（gstreamer）打不开：请把 [media].video_vaapi 改回 \"auto\"，或删掉该缓存让它按 MP4/H.264 重新转码",
                        path.display()
                    ),
                )
                .with_detail_code("CONVERSION_UNAVAILABLE"));
            }
            tracing::debug!(
                output = key,
                cache = %path.display(),
                "播放转码缓存：不再递归转码"
            );
            return Ok(path);
        }
        let caps = self.caps.clone().with_consumer(consumer);
        let (details, verdict) = crate::compat::classify_path(&path, "video", &caps);
        match verdict {
            Compatibility::Playable => Ok(path),
            Compatibility::Unsupported { reason } => Err(ErrorBody::new(
                ErrorKind::UnsupportedCodec,
                format!("{reason}（{path:?}）"),
            )
            .with_detail_code("COMPAT_UNSUPPORTED")),
            Compatibility::NeedsConversion { reason, .. } => {
                let duration_ms = details.as_ref().and_then(|item| item.duration_ms);
                let spec = self.transcode_spec(&path, duration_ms, consumer)?;
                // 缓存是否命中**以当前 cache key 为准**（源文件被改过就不算命中，t21 F4）。
                if let Some(cached) = self.transcode.ready_cache(&path, &spec.key) {
                    tracing::info!(
                        output = key,
                        media = %path.display(),
                        cache = %cached.display(),
                        "命中转码缓存：直接播缓存（源文件不动）"
                    );
                    return Ok(cached);
                }
                // 「关掉自动转码」不等于「允许静默软解」：8K H.264 软解 0.086 fps
                // 不算能播，10bit 更是解不出来。关掉就如实报错，并给出可操作的出路。
                if self.transcode.disabled_by_config() {
                    return Err(ErrorBody::new(
                        ErrorKind::UnsupportedCodec,
                        format!(
                            "{reason}；自动转码已关闭（{}），无法自动转换：请把素材转成 8bit HEVC，或改回 \"auto\"",
                            self.transcode.disabled_reason()
                        ),
                    )
                    .with_detail_code("CONVERSION_UNAVAILABLE"));
                }
                if !self.transcode.enabled() {
                    return Err(ErrorBody::new(
                        ErrorKind::UnsupportedCodec,
                        format!("{reason}；{}", self.transcode.missing_ffmpeg_message()),
                    )
                    .with_detail_code("CONVERSION_UNAVAILABLE"));
                }
                let view = self.transcode.submit(spec, false);
                if view.state == liuguang_ipc::protocol::PrepareState::Failed {
                    return Err(ErrorBody::new(
                        ErrorKind::UnsupportedCodec,
                        view.message.unwrap_or(reason),
                    )
                    .with_detail_code("CONVERSION_FAILED"));
                }
                if !self
                    .pending_playback
                    .iter()
                    .any(|(output, source)| output == key && source == &path)
                {
                    self.pending_playback.push((key.to_string(), path.clone()));
                }
                let stage = match view.state {
                    liuguang_ipc::protocol::PrepareState::Queued => "已排队".to_string(),
                    liuguang_ipc::protocol::PrepareState::Running => {
                        format!("转码中 {}‰", view.progress_permille)
                    }
                    _ => "处理中".to_string(),
                };
                Err(ErrorBody::new(
                    ErrorKind::NeedsPreparation,
                    format!("{reason}；{stage}，完成后会自动播放：{}", path.display()),
                )
                .with_detail_code("NEEDS_PREPARATION"))
            }
        }
    }

    /// 把转码器的事件转成 IPC 事件，并对"等转码"的通道自动播放缓存。
    fn poll_transcode(&mut self, now: Instant) {
        use crate::transcode::TranscodeEvent;

        let mut events = self.transcode.enforce_timeouts(now);
        events.extend(self.transcode.poll());
        for event in events {
            match event {
                TranscodeEvent::Finished { source, cache_path } => {
                    self.events.publish(
                        EventMessage::new(EventKind::MediaPrepared)
                            .with_media(source.to_string_lossy().into_owned())
                            .with_cache_path(cache_path.to_string_lossy().into_owned()),
                    );
                    let waiting: Vec<String> = self
                        .pending_playback
                        .iter()
                        .filter(|(_, pending)| pending == &source)
                        .map(|(output, _)| output.clone())
                        .collect();
                    self.pending_playback
                        .retain(|(_, pending)| pending != &source);
                    for output in waiting {
                        let entry = MediaEntry {
                            path: cache_path.clone(),
                            kind: MediaKind::Video,
                        };
                        match self.show_entry(&output, &entry, true) {
                            Ok(()) => tracing::info!(
                                output = %output,
                                source = %source.display(),
                                cache = %cache_path.display(),
                                "转码完成，已自动播放缓存"
                            ),
                            Err(body) => {
                                tracing::warn!(
                                    output = %output,
                                    source = %source.display(),
                                    error = %body.message,
                                    "转码完成但自动播放失败"
                                );
                                self.events.publish(
                                    EventMessage::new(EventKind::Error)
                                        .with_output(&output)
                                        .with_media(source.to_string_lossy().into_owned())
                                        .with_message(format!(
                                            "转码完成但自动播放失败：{}",
                                            body.message
                                        )),
                                );
                            }
                        }
                    }
                }
                TranscodeEvent::Failed { source, message } => {
                    self.pending_playback
                        .retain(|(_, pending)| pending != &source);
                    self.events.publish(
                        EventMessage::new(EventKind::Error)
                            .with_media(source.to_string_lossy().into_owned())
                            .with_message(message),
                    );
                }
            }
        }
    }

    /// `prepare_media`：显式请求"把这个素材准备好"（幂等 + 异步 + 可轮询进度）。
    fn handle_prepare_media(&mut self, id: String, params: serde_json::Value) -> Response {
        use crate::compat::Compatibility;
        use liuguang_ipc::protocol::{PrepareMediaParams, PrepareMediaResult, PrepareState};

        let params: PrepareMediaParams = match serde_json::from_value(params) {
            Ok(value) => value,
            Err(error) => {
                return err_response(
                    id,
                    ErrorKind::InvalidParams,
                    &format!("参数不合法: {error}"),
                );
            }
        };
        let resolved = match self.check_media_path(Path::new(&params.path)) {
            Ok(path) => path,
            Err(body) => return Response::err(id, body),
        };
        let ext = resolved
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let kind = if liuguang_core::library::DEFAULT_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            "image"
        } else {
            "video"
        };
        if kind == "image" {
            return ok_response(
                id,
                &PrepareMediaResult {
                    state: PrepareState::Ready,
                    cache_path: None,
                    progress_permille: 1000,
                    message: Some("图片不需要转码（解码走 8bit RGBA，与位深无关）".to_string()),
                },
            );
        }
        // t37：与 `set_wallpaper` 用同一条输出的 consumer（判定/转码/播放三处一致）。
        let consumer = self.consumer_for_request(None);
        let caps = self.caps.clone().with_consumer(consumer);
        let (details, verdict) = crate::compat::classify_path(&resolved, kind, &caps);
        match verdict {
            Compatibility::Playable => ok_response(
                id,
                &PrepareMediaResult {
                    state: PrepareState::Ready,
                    cache_path: None,
                    progress_permille: 1000,
                    message: Some("本机可以直接硬解，无需转码".to_string()),
                },
            ),
            Compatibility::Unsupported { reason } => {
                // 转码也救不了的素材**不得进入队列**。
                err_response_code(
                    id,
                    ErrorKind::UnsupportedCodec,
                    "COMPAT_UNSUPPORTED",
                    &reason,
                )
            }
            Compatibility::NeedsConversion { reason, .. } => {
                if !self.transcode.enabled() {
                    let message = if self.transcode.disabled_by_config() {
                        format!(
                            "{reason}；但自动转码已关闭（[media].transcode = \"off\" 或 LIUGUANG_TRANSCODE=off），无法自动转换"
                        )
                    } else {
                        format!("{reason}；{}", self.transcode.missing_ffmpeg_message())
                    };
                    return err_response_code(
                        id,
                        ErrorKind::UnsupportedCodec,
                        "CONVERSION_UNAVAILABLE",
                        &message,
                    );
                }
                let force = params.force.unwrap_or(false);
                let duration_ms = details.as_ref().and_then(|item| item.duration_ms);
                let spec = match self.transcode_spec(&resolved, duration_ms, consumer) {
                    Ok(spec) => spec,
                    Err(body) => return Response::err(id, body),
                };
                // 已有任务记录时直接按现状回答：这就是协议里写的"查询进度"路径，
                // 不碰队列也不重排队（`force = true` 才往下走重新提交）。**但 key 必须
                // 与当前一致**：源文件被 touch 过就说明缓存过期，要重新排队。
                if !force {
                    if let Some(view) = self.transcode.view_fresh(&resolved, &spec.key) {
                        return ok_response(
                            id,
                            &PrepareMediaResult {
                                state: view.state,
                                cache_path: view
                                    .cache_path
                                    .map(|path| path.to_string_lossy().into_owned()),
                                progress_permille: view.progress_permille,
                                message: view.message.or(Some(reason)),
                            },
                        );
                    }
                }
                let view = self.transcode.submit(spec, force);
                ok_response(
                    id,
                    &PrepareMediaResult {
                        state: view.state,
                        cache_path: view
                            .cache_path
                            .map(|path| path.to_string_lossy().into_owned()),
                        progress_permille: view.progress_permille,
                        message: view.message.or(Some(reason)),
                    },
                )
            }
        }
    }

    /// `remove_source`：运行时移除来源（配置里的来源只会被"屏蔽"，改配置请编辑文件）。
    fn handle_remove_source(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: RemoveSourceParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        let known = self
            .effective_sources()
            .iter()
            .any(|s| s.id == params.source_id);
        if !known {
            return err_response_code(
                id,
                ErrorKind::FileUnavailable,
                "NOT_FOUND",
                &format!("未知来源: {}", params.source_id),
            );
        }
        self.added_sources.retain(|(_, path, _)| {
            std::fs::canonicalize(path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                != params.source_id
        });
        if !self.removed_sources.iter().any(|s| s == &params.source_id) {
            self.removed_sources.push(params.source_id);
        }
        ok_response(id, &AcceptedResult::accepted())
    }

    /// `delete_media`：把一条媒体**从媒体库索引里移除**。
    ///
    /// ⚠️ **不删磁盘文件**。这三条语义是给 GUI/调用方的契约，实现逐条对得上：
    ///
    /// 1. 只动索引与"引用它的播放列表条目"，磁盘上的文件原样保留；重新扫描会把它
    ///    加回来（这是"从库里移除这一条"，不是"拉黑这个文件"）。
    /// 2. 若有通道**正在播**它，先切到下一条；切不动（播放源空了）就保持画面并让
    ///    通道进入待重建状态，**绝不谎报切换成功**。
    /// 3. 幂等：本来就不在索引里 ⇒ `removed: false` 且不报错（GUI 重复点删除不该弹错）。
    fn handle_delete_media(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: DeleteMediaParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        let raw = PathBuf::from(&params.path);
        if !raw.is_absolute() {
            // 媒体 id 的定义就是"规范化后的绝对路径"；相对路径没有可比较的语义。
            return err_response(
                id,
                ErrorKind::InvalidParams,
                "媒体 id 必须是绝对路径（与 list_media 返回的 id 一致）",
            );
        }
        // 文件可能已经被外部删掉/移走：这时拿不到 canonical，就退回字符串比较，
        // 让"清理索引里的僵尸条目"这件事仍然能做成。
        let canonical = self.check_media_path(&raw).ok();
        let canonical_key = canonical.as_ref().map(|c| c.to_string_lossy().into_owned());
        let matches = |candidate: &str| {
            candidate == params.path || canonical_key.as_deref() == Some(candidate)
        };

        // 1) 先判定"谁正在播它"——必须发生在移除列表条目之前，否则信息就没了。
        let playing: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, ch)| {
                ch.current
                    .as_ref()
                    .is_some_and(|c| matches(&c.media.to_string_lossy()))
            })
            .map(|(key, _)| key.clone())
            .collect();

        // 2) 从索引里移除。
        let before = self.media.items.len();
        self.media
            .items
            .retain(|it| !(matches(&it.id) || matches(&it.path)));
        let removed_count = before.saturating_sub(self.media.items.len());

        // 3) 引用它的播放列表条目一并清掉：否则"从库里删了"却还能被列表播出来，
        //    用户看到的就是"删了个假的"。
        let mut touched: Vec<Option<String>> = Vec::new();
        for pl in &mut self.playlists {
            let n = pl.entries.len();
            pl.entries.retain(|e| !matches(&e.media_id));
            if pl.entries.len() != n {
                touched.push(pl.output.clone());
            }
        }
        // 引用变了 ⇒ 受影响通道的游标立即重建（与 update_playlist 同一套规则）。
        for output in &touched {
            self.apply_playlist_change(output.as_deref());
        }

        // 4) 正在播的通道切走。此时列表里已没有这一条，切过去就是别的画面。
        let mut switched = Vec::new();
        for key in playing {
            match self.switch_next(&key, false) {
                Ok(()) => switched.push(key),
                Err(e) => {
                    self.mark_playlist_dirty(Some(&key));
                    tracing::warn!(
                        output = %key,
                        error = %e.message,
                        "删除媒体后无法切走（播放源为空），保持当前画面并等待重建"
                    );
                }
            }
        }
        tracing::info!(
            path = %params.path,
            removed_count,
            switched = switched.len(),
            "媒体已从索引移除（磁盘文件保留）"
        );
        ok_response(
            id,
            &DeleteMediaResult {
                removed: removed_count > 0,
                removed_count,
                switched_outputs: switched,
            },
        )
    }

    /// `list_playlists`：当前播放列表（运行期状态；`save_config` 会写进
    /// `[[playlists]]`，重启时由 [`load_playlists_from_config`] 装载 —— T62）。
    fn handle_list_playlists(&self, id: String) -> Response {
        ok_response(
            id,
            &ListPlaylistsResult {
                playlists: self.playlists.clone(),
            },
        )
    }

    /// `update_playlist`：新建（`playlist_id` 缺省）/更新播放列表。
    fn handle_update_playlist(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: UpdatePlaylistParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        if !matches!(params.mode.as_str(), "sequential" | "random") {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "INVALID_PARAMS",
                &format!("mode 必须是 sequential 或 random（收到 {}）", params.mode),
            );
        }
        if !matches!(params.on_video_end.as_str(), "next" | "loop") {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "INVALID_PARAMS",
                &format!(
                    "on_video_end 必须是 next 或 loop（收到 {}）",
                    params.on_video_end
                ),
            );
        }
        if params.interval_seconds == 0 || params.interval_seconds > 86_400 {
            return err_response_code(
                id,
                ErrorKind::InvalidParams,
                "INVALID_PARAMS",
                &format!(
                    "interval_seconds 必须在 1..=86400（收到 {}）",
                    params.interval_seconds
                ),
            );
        }
        let media = match self.library().map(|cfg| cfg.validate()) {
            None => {
                return err_response_code(
                    id,
                    ErrorKind::InvalidParams,
                    "NOT_CONFIGURED",
                    LIBRARY_NOT_CONFIGURED_HINT,
                );
            }
            Some(Err((field, e))) => {
                return err_response_code(
                    id,
                    ErrorKind::InvalidParams,
                    "INVALID_CONFIG",
                    &format!("{field}: {}", e.message),
                );
            }
            Some(Ok(m)) => m,
        };
        // 条目必须过安全边界（不得把任意路径塞进播放列表）。
        for entry in &params.entries {
            if let Err(e) = media.resolve(&entry.media_id) {
                return err_response_code(
                    id,
                    ErrorKind::InvalidParams,
                    e.code.as_str(),
                    &format!("entry {}: {}", entry.media_id, e.message),
                );
            }
        }
        let next_num = self
            .playlists
            .iter()
            .filter_map(|p| p.id.strip_prefix("pl-")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let pl = liuguang_ipc::protocol::Playlist {
            id: params
                .playlist_id
                .clone()
                .unwrap_or_else(|| format!("pl-{next_num}")),
            name: params.name,
            entries: params
                .entries
                .into_iter()
                .map(|e| PlaylistEntry {
                    media_id: e.media_id,
                })
                .collect(),
            mode: params.mode,
            interval_seconds: params.interval_seconds,
            on_video_end: params.on_video_end,
            output: params.output,
            no_repeat: params.no_repeat,
        };
        let old_output = match self.playlists.iter_mut().find(|p| p.id == pl.id) {
            Some(slot) => {
                let prev = slot.output.clone();
                *slot = pl.clone();
                prev
            }
            None => {
                self.playlists.push(pl.clone());
                None
            }
        };
        self.playlists_loaded = true;
        if old_output != pl.output {
            self.apply_playlist_change(old_output.as_deref());
        }
        // T68：列表内容变了 ⇒ 受影响通道的游标**立即**重建。否则就是
        // "update_playlist 返回成功，但还在播更新前的东西"（报告成功却没做）；
        // 而在 `loop` 模式下"等下次切换"会**永远等不到**（真机 E2E 抓到）。
        self.apply_playlist_change(pl.output.as_deref());
        tracing::info!(
            playlist = %pl.id,
            output = pl.output.as_deref().unwrap_or("*"),
            mode = %pl.mode,
            on_video_end = %pl.on_video_end,
            entries = pl.entries.len(),
            "播放列表已更新：相关通道的游标将在下次切换时重建"
        );
        ok_response(id, &pl)
    }

    /// `delete_playlist`：删掉一条播放列表定义。
    ///
    /// ⚠️ **不删磁盘文件**：只删"这条列表怎么播"的定义，条目引用的媒体原样保留，
    /// 从别的列表或目录轮换里仍然能播到它们。
    ///
    /// 语义：
    /// - 幂等：id 不存在 ⇒ `removed: false`，不报错（GUI 重复删除不该弹错）；
    /// - 删掉的列表若正被某个输出使用，该输出**立刻**回到目录轮换（游标立即重建，
    ///   不等"下次切换"——`loop` 模式下那次切换永远等不到）；
    /// - `rebuilt_outputs` 只报**真的重建成功**的通道，重建失败的走
    ///   `note_rotation_error` 留痕，不在这里假装成功。
    fn handle_delete_playlist(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: DeletePlaylistParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        let Some(pos) = self
            .playlists
            .iter()
            .position(|p| p.id == params.playlist_id)
        else {
            // 已经是目标状态：删不存在的列表不该是错误。
            return ok_response(
                id,
                &DeletePlaylistResult {
                    removed: false,
                    playlist_id: params.playlist_id,
                    rebuilt_outputs: Vec::new(),
                },
            );
        };
        // 受影响通道必须在**删除之前**算出来（`active_playlist` 还看得见它）。
        let affected: Vec<String> = self
            .channels
            .keys()
            .filter(|key| {
                self.active_playlist(key)
                    .is_some_and(|p| p.id == params.playlist_id)
            })
            .cloned()
            .collect();
        let deleted = self.playlists.remove(pos);
        self.mark_playlist_dirty(deleted.output.as_deref());
        let mut rebuilt = Vec::new();
        for key in affected {
            match self.rebuild_channel_playlist(&key) {
                Ok(()) => rebuilt.push(key),
                Err(e) => {
                    // 目录为空之类：留痕即可，别把删除本身变成失败。
                    self.note_rotation_error(&key, &e.message);
                }
            }
        }
        tracing::info!(
            playlist = %deleted.id,
            name = %deleted.name,
            output = deleted.output.as_deref().unwrap_or("*"),
            entries = deleted.entries.len(),
            rebuilt = rebuilt.len(),
            "播放列表定义已删除（磁盘文件保留）"
        );
        ok_response(
            id,
            &DeletePlaylistResult {
                removed: true,
                playlist_id: params.playlist_id,
                rebuilt_outputs: rebuilt,
            },
        )
    }

    /// `set_output_transform`：**只改运行时**（持久化走 `save_config`）。
    ///
    /// 与 `set_wallpaper` 的 fit/zoom/offset/rotation 覆盖共用同一套校验与单位（不引入第二套）。
    fn handle_set_output_transform(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: SetOutputTransformParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return err_response(id, ErrorKind::InvalidParams, &e.to_string()),
        };
        let key = match self.resolve_channel(params.output.as_deref()) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        let overrides = SetWallpaperParams {
            pin: None,
            path: String::new(),
            output: Some(key.clone()),
            fit: params.fit.clone(),
            zoom: params.zoom.map(f64::from),
            offset_x: params.offset_x.map(f64::from),
            offset_y: params.offset_y.map(f64::from),
            rotation: params.rotation,
            brightness: params.brightness.map(f64::from),
        };
        let changed = match self.apply_overrides(&key, &overrides) {
            Ok(changed) => changed,
            Err(body) => return Response::err(id, body),
        };
        if !changed {
            return ok_response(id, &AcceptedResult::accepted());
        }
        self.schedule_config_write(crate::config_store::Preference::OutputTransform {
            output: key.clone(),
        });
        // 若刚通过 `set_wallpaper` 排了待上屏任务，下一 tick 本就会按新变换打开管线，
        // 避免先为旧画面重建一遍。
        let has_pending_set = self.pending_sets.iter().any(|p| p.output == key);
        if !has_pending_set {
            let rt = self.output_render_transform(&key);
            let live_updated = if let Some(ch) = self.channels.get_mut(&key) {
                if ch.video.is_none() && ch.paused_video_media.is_some() {
                    // 已卸载的暂停视频保留原路径，等恢复时按最新 config 重建。
                    true
                } else {
                    let is_dmabuf_passthrough = ch.video.as_ref().is_some_and(|v| {
                        !v.handles_render_transform()
                            && v.dmabuf_state().is_some_and(|s| s.is_dmabuf)
                    }) || ch
                        .current
                        .as_ref()
                        .is_some_and(|d| d.decoder.ends_with("(dmabuf)"));
                    let has_non_default_transform = rt.rotation != Rotation::Deg0
                        || (rt.brightness - 1.0).abs() > 1e-4
                        || rt.fit != liuguang_core::fit::FitMode::Cover
                        || (rt.zoom - 1.0).abs() > 1e-4
                        || rt.offset_x.abs() > 1e-4
                        || rt.offset_y.abs() > 1e-4;
                    let needs_rgb_upgrade = is_dmabuf_passthrough
                        && has_non_default_transform
                        && !self
                            .config
                            .media
                            .video_dmabuf_rgb
                            .trim()
                            .eq_ignore_ascii_case("never");
                    let is_cpu_nv12_pixel_change = ch
                        .video
                        .as_ref()
                        .is_some_and(|v| v.dmabuf_state().is_some_and(|s| !s.is_dmabuf))
                        && (rt.rotation != Rotation::Deg0 || (rt.brightness - 1.0).abs() > 1e-4);
                    if let Some(video) = ch.video.as_mut() {
                        if video.handles_render_transform() {
                            video.set_render_transform(rt);
                            true
                        } else {
                            !needs_rgb_upgrade && !is_cpu_nv12_pixel_change
                        }
                    } else {
                        false
                    }
                }
            } else {
                false
            };
            if !live_updated {
                let current = self
                    .channels
                    .get(&key)
                    .and_then(|ch| ch.current.as_ref().map(|d| d.media.clone()));
                if let Some(path) = current {
                    let kind = media_kind_for_path(&path);
                    if let Err(body) = self.show_entry(&key, &MediaEntry { path, kind }, false) {
                        return Response::err(id, body);
                    }
                }
            }
        }
        ok_response(id, &AcceptedResult::accepted())
    }

    /// `save_config`：**原子**写回配置文件；未授权用户明确拒绝且不损坏原文件。
    /// **预演**保存时的来源合并：在**配置克隆**上做增量合并，并检查"残余派生"。
    ///
    /// 返回 `MergePlan { config, applied, residual }`：
    /// - `config`：合并后的配置（只在预演通过后才会替换真实配置）；
    /// - `applied`：本次真正合并的条数（新增 + 删除）；
    /// - `residual`：**用户要求删除、但合并后仍会被某个容器派生**的来源
    ///   （`容器名` 用于诊断文案——必须说准是哪个容器，不能笼统说 root）。
    ///
    /// # 为什么必须是"增量"而不是"覆盖"
    ///
    /// 旧实现拿"运行时集合"去覆盖配置集合，结果把用户**没有要求删除**的
    /// `image_dirs` 条目一起清掉（真机语义 diff：`image_dirs: [...] -> []`）。
    /// 这里只做两件**用户明确要求**的事：
    /// 1. 把 `added_sources` 追加进 `[[library.sources]]`（已覆盖则跳过）；
    /// 2. 把 `removed_sources` 从 **image_dirs / video_dirs / sources** 三个容器
    ///    里移除；**legacy `[slideshow].directory` 不动**（那是旧键，属于用户
    ///    其它配置，`save_config` 不该替他改）。
    ///
    /// 因此"删 legacy 覆盖的来源"必然留下残余 ⇒ 被上面的预演拒绝，而不是写坏文件。
    fn plan_source_merge(&self) -> MergePlan {
        fn canon(p: &Path) -> String {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .into_owned()
        }
        let mut config = self.config.clone();
        // **T62**：播放列表是运行期状态（`update_playlist`），保存时必须以运行时为准
        // 写进配置 —— 否则 `save_config` 会"报成功但配置零变化"，重启即丢
        // （与 T25-F1 同族）。放在 `library` 分支**之前**：播放列表不依赖媒体库，
        // 那个 `let Some(lib) = ... else { return }` 的早退也要带上它。
        if self.playlists_loaded {
            config.playlists = self.playlists.iter().map(playlist_to_config).collect();
        }
        let mut applied = 0usize;
        let residual: Vec<(String, Vec<liuguang_core::library::SourceContainer>)> = Vec::new();
        let mut residual = residual;
        {
            let Some(lib) = config.library.as_mut() else {
                return MergePlan {
                    config,
                    applied: 0,
                    residual,
                };
            };
            let root = lib.root.clone();
            let abs = |p: &Path| -> PathBuf {
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    root.join(p)
                }
            };
            let removed = self.removed_sources.clone();
            // (a) 删除：只动三个"库"容器，绝不碰 slideshow.directory
            for dirs in [&mut lib.image_dirs, &mut lib.video_dirs] {
                let before = dirs.len();
                dirs.retain(|d| !removed.contains(&canon(&abs(d))));
                applied += before - dirs.len();
            }
            let before = lib.sources.len();
            lib.sources
                .retain(|s| !removed.contains(&canon(&abs(&s.path))));
            applied += before - lib.sources.len();
            // (b) 新增（增量；任一容器已覆盖则不重复追加）
            for (kind, path, recursive) in &self.added_sources {
                let id = canon(path);
                let covered = lib.image_dirs.iter().any(|d| canon(&abs(d)) == id)
                    || lib.video_dirs.iter().any(|d| canon(&abs(d)) == id)
                    || lib.sources.iter().any(|s| canon(&abs(&s.path)) == id);
                if covered {
                    continue;
                }
                lib.sources.push(liuguang_core::library::LibrarySource {
                    path: path.clone(),
                    kind: kind.clone(),
                    recursive: *recursive,
                });
                applied += 1;
            }
        }
        // (c) 残余派生检查：删除的 id 是否仍被某个容器派生（含 legacy）
        if !self.removed_sources.is_empty() {
            let derived = config
                .library
                .as_ref()
                .map(|lib| lib.derived_sources(Some(&config.slideshow.directory)))
                .unwrap_or_default();
            for id in &self.removed_sources {
                let mut containers = Vec::new();
                for (container, path) in &derived {
                    if canon(path) == *id && !containers.contains(container) {
                        containers.push(*container);
                    }
                }
                if !containers.is_empty() {
                    residual.push((id.clone(), containers));
                }
            }
        }
        MergePlan {
            config,
            applied,
            residual,
        }
    }

    /// 回读刚写出的配置文件，校验"文件内容 == 运行时事实"（T25-F1 / T62 的验收判据）。
    ///
    /// 两项都要对：
    /// 1. **来源集合**（T25-F1）：运行时生效来源与配置派生的来源双向包含；
    /// 2. **播放列表**（T62）：运行时的 `list_playlists` 与文件里的 `[[playlists]]`
    ///    逐字段相等 —— 否则"saved=true 但播放列表没落盘"会再次假成功。
    fn verify_saved_config(&self) -> bool {
        let Ok(saved) = Config::load(&self.config_path) else {
            return false;
        };
        let canon = |p: &Path| -> String {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .into_owned()
        };
        let mut on_disk: Vec<String> = Vec::new();
        if let Some(lib) = saved.library.as_ref() {
            for d in lib.image_dirs.iter().chain(lib.video_dirs.iter()) {
                on_disk.push(canon(&lib.abs_dir(d)));
            }
            for s in &lib.sources {
                on_disk.push(canon(&lib.abs_dir(&s.path)));
            }
        }
        let legacy = saved.slideshow.directory.clone();
        if !legacy.as_os_str().is_empty() {
            on_disk.push(canon(&legacy));
        }
        let want: Vec<String> = self.effective_sources().into_iter().map(|s| s.id).collect();
        // 双向包含（顺序无关；两边都已去重）。
        let sources_ok = want.iter().all(|id| on_disk.contains(id))
            && on_disk.iter().all(|id| want.contains(id));
        // 播放列表：**逐字段**比（顺序也要一致 —— 顺序是用户可感知的语义）。
        let want_playlists: Vec<PlaylistConfig> = if self.playlists_loaded {
            self.playlists.iter().map(playlist_to_config).collect()
        } else {
            // 未装载（无媒体库/媒体库非法）：保存时我们**没动**这一节，回读应原样相同。
            self.config.playlists.clone()
        };
        let playlists_ok = saved.playlists == want_playlists;
        if !playlists_ok {
            tracing::warn!(
                runtime = want_playlists.len(),
                on_disk = saved.playlists.len(),
                "回读校验：播放列表与运行时不符（saved 不能代表真的写对了）"
            );
        }
        sources_ok && playlists_ok
    }

    fn handle_save_config(&mut self, id: String) -> Response {
        // 若进程启动时尚未配置 `[library]`，但磁盘上已由用户或初始化脚本补全了该段，
        // 保存偏好/其它设置时先保留磁盘 `[library]`，避免误将用户刚建的媒体库抹空。
        self.sync_disk_library_if_missing();
        // **T25-F1**：save 前必须把运行期的来源改动**合并进将要写出的配置**，
        // 否则 `add_source` 只活在内存里：UI 显示"已保存"，重启后用户的新增来源
        // 就消失了。合并发生在 `self.config`（克隆前就地改），因此"写什么"
        // 与"运行时是什么"是同一份数据。
        //
        // **T37（数据丢失回归）**：旧实现在 `self.config` 上**就地**合并，然后用
        // "回读校验"事后发现不一致——但那时文件**已经被写坏**（实测把
        // `library.image_dirs` 清成 `[]`，而来源又被 legacy `[slideshow].directory`
        // 派生回来 ⇒ 用户既没删掉来源、又丢了自己的设置）。
        // 现在改成**先在克隆上预演**：只要"用户删掉的来源仍会被任何容器派生"，
        // 就**拒绝写入**并明确报告"未做任何改动"（配置文件字节不变）。
        let plan = self.plan_source_merge();
        if !plan.residual.is_empty() {
            let detail = plan
                .residual
                .iter()
                .map(|(id, containers)| {
                    format!(
                        "{id} 仍被 {} 覆盖",
                        containers
                            .iter()
                            .map(|c| c.name())
                            .collect::<Vec<_>>()
                            .join("、")
                    )
                })
                .collect::<Vec<_>>()
                .join("；");
            tracing::warn!(
                path = %self.config_path.display(),
                detail = %detail,
                "拒绝保存：删除的源仍会被配置里的其它容器派生 ⇒ 未做任何改动（配置文件未写入）"
            );
            // **结构化回传冲突容器名**（T37）：不让调用方从中文文案里猜。
            let conflicts: Vec<String> = plan
                .residual
                .iter()
                .flat_map(|(_, cs)| cs.iter().map(|c| c.name().to_string()))
                .collect();
            let body = ErrorBody::new(
                ErrorKind::InvalidParams,
                format!(
                    "未做任何改动：{detail}。请同时移除该容器里的对应条目（或改用 add_source/remove_source 管理运行期来源）后再保存。"
                ),
            )
            .with_detail_code("NO_EFFECTIVE_CHANGE")
            .with_conflicts(conflicts);
            return Response::err(id, body);
        }
        let merged = plan.applied;
        // 写前保留内存与磁盘双快照：写失败或回读校验失败时一起回滚。
        let prev_config = std::mem::replace(&mut self.config, plan.config);
        let before_bytes = std::fs::read(&self.config_path).ok();
        match self.config.save_atomic(&self.config_path) {
            Ok(bytes) => {
                // **回读校验**：`saved=true` 只代表"字节写下去了"，
                // `verified` 才代表"重新读回来的**来源集合 + 播放列表**与运行时一致"。
                // 调用方（TUI/CLI）应当看 `verified` 而不是 `saved`。
                let verified = self.verify_saved_config();
                if !verified {
                    // **回滚**：回读不一致 ⇒ 还原内存配置与磁盘字节，宁可"没保存"也不留半截状态。
                    self.config = prev_config;
                    let restored = match &before_bytes {
                        Some(prev) => std::fs::write(&self.config_path, prev).is_ok(),
                        None => false,
                    };
                    tracing::warn!(
                        path = %self.config_path.display(),
                        merged,
                        restored,
                        "配置已写出但**回读校验失败**：已回滚到保存前的字节（未做任何改动）"
                    );
                    return err_response_code(
                        id,
                        ErrorKind::Internal,
                        "NO_EFFECTIVE_CHANGE",
                        "未做任何改动：写出后回读校验失败（来源集合与运行时不符），已回滚到保存前的配置。",
                    );
                }
                tracing::info!(
                    path = %self.config_path.display(),
                    bytes,
                    merged_sources = merged,
                    playlists = self.playlists.len(),
                    verified,
                    "配置已原子写回（含运行期来源与播放列表；verified 见字段）"
                );
                // 落盘成功且校验通过 ⇒ 运行时改动已成为配置，清掉增量，
                // 避免下次保存重复追加（也让"运行时 vs 配置"重新同源）。
                self.added_sources.clear();
                self.removed_sources.clear();
                ok_response(
                    id,
                    &SaveConfigResult {
                        saved: true,
                        verified,
                        merged_sources: merged,
                        path: self.config_path.display().to_string(),
                        bytes,
                    },
                )
            }
            Err(e) => {
                self.config = prev_config;
                let (kind, detail) = match &e {
                    liuguang_core::error::ConfigError::Io { source, .. }
                        if source.kind() == std::io::ErrorKind::PermissionDenied =>
                    {
                        (ErrorKind::PermissionDenied, "PERMISSION_DENIED")
                    }
                    liuguang_core::error::ConfigError::Io { .. } => {
                        (ErrorKind::Internal, "IO_ERROR")
                    }
                    _ => (ErrorKind::Internal, "SERIALIZE_ERROR"),
                };
                tracing::warn!(
                    path = %self.config_path.display(),
                    detail,
                    error = %e,
                    "配置写回失败（原文件未被破坏）"
                );
                Response::err(
                    id,
                    ErrorBody::new(kind, e.to_string()).with_detail_code(detail),
                )
            }
        }
    }

    /// `get_metrics`：CPU/内存/线程/提交计数/帧率（System 页；TUI 侧 ≤2Hz 拉取）。
    ///
    /// `fps` 用 **`presented_ok` 增量**算（提交成功的帧），不是解码出队帧率；
    /// CPU% 是单核百分比（100 = 跑满一核），首次采样无基线时为 0。
    fn handle_metrics(&mut self, id: String) -> Response {
        let presented_ok_total: u64 = self.channels.values().map(|ch| ch.presented_ok).sum();
        let sys = self.sysstat.sample(presented_ok_total);
        let channels = self
            .channels
            .iter()
            .filter_map(|(output, ch)| {
                let source = ch.video.as_ref()?;
                let st = source.video_stats();
                let dbuf = source.dmabuf_state().unwrap_or_default();
                Some(ChannelMetrics {
                    output: output.clone(),
                    presented: st.presented,
                    present_ok: ch.presented_ok,
                    present_failures: ch.present_failures,
                    present_failures_consecutive: ch.present_failures_consecutive,
                    is_dmabuf: dbuf.is_dmabuf,
                    dmabuf_fallbacks: dbuf.fallbacks,
                    dmabuf_keepalive_len: dbuf.keepalive_len,
                })
            })
            .collect();
        let thumbs = self.thumbs.counters().snapshot();
        let result = MetricsResult {
            thumb_requests: thumbs.requests,
            thumb_hits: thumbs.hits,
            thumb_misses: thumbs.misses,
            thumb_decoded: thumbs.decoded,
            thumb_avg_ms: thumbs.avg_ms,
            thumb_queue_depth: thumbs.queue_depth,
            channels,
            cpu_percent: sys.cpu_percent,
            mem_bytes: sys.mem_bytes,
            threads: sys.threads,
            fps: sys.fps,
            uptime_seconds: self.started.elapsed().as_secs(),
            commit: self.backend.commit_stats().map(|c| DisplayCommitStatus {
                commit_ok: c.commit_ok,
                retried_after_busy: c.retried_after_busy,
                legacy_fallback: c.legacy_fallback,
                failed: c.failed,
                commit_success_total: c.commit_success_total(),
                attempts: c.attempts(),
            }),
            master: self.backend.master_status().map(|m| DisplayMasterStatus {
                master: m.master,
                acquire_result: m.acquire_result.clone(),
                holders: m.holders.clone(),
                remedy_hint: m.remedy_hint(),
            }),
            preload_cache_bytes: self.preload.bytes() as u64,
            preload_cache_items: self.preload.len() as u32,
            cpu_pressure: self.shed.reading().map(|r| CpuPressureStatus {
                percent: r.percent,
                source: match r.source {
                    crate::loadshed::PressureSource::Psi => "psi",
                    crate::loadshed::PressureSource::LoadAvg => "loadavg",
                    crate::loadshed::PressureSource::Unavailable => "unavailable",
                }
                .to_string(),
                shedding: self.shed.level() == crate::loadshed::LoadLevel::Shed,
            }),
        };
        ok_response(id, &result)
    }

    fn handle_outputs(&self, id: String) -> Response {
        let outputs = self
            .backend
            .outputs()
            .into_iter()
            .map(|o| OutputStatus {
                name: o.id.as_str().to_string(),
                connected: o.connected,
                mode: o
                    .current_mode
                    .map(|m| format!("{}x{}@{:.3}", m.width, m.height, m.refresh_hz())),
                mode_count: o.modes.len(),
            })
            .collect();
        ok_response(id, &OutputsResult { outputs })
    }

    fn handle_set(&mut self, id: String, params: serde_json::Value) -> Response {
        // 每次请求一个 `request_id`：受理响应带它，`WALLPAPER_APPLIED` 事件也带它，
        // 客户端据此把"Sending → Accepted → Applied/Failed"串起来（t22 验收）。
        self.next_request_id += 1;
        let request_id = format!("set-{}", self.next_request_id);
        let params: SetWallpaperParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                return Response::err(
                    id,
                    ErrorBody::new(ErrorKind::InvalidParams, format!("参数非法: {e}")),
                );
            }
        };
        let key = match self.resolve_channel(params.output.as_deref()) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        let path = PathBuf::from(&params.path);
        // fit/zoom/offset/rotation 覆盖。
        // **不得静默失效**：参数非法 → INVALID_PARAMS；输出未声明 → 现场建立条目，
        // 让 IPC 覆盖与配置文件等价（AUDIT §2.1）。
        let transform_changed = match self.apply_overrides(&key, &params) {
            Ok(changed) => changed,
            Err(body) => return Response::err(id, body),
        };
        // 只有真的带了 fit/zoom/offset/rotation/brightness 覆盖时才写配置：单纯换壁纸不该改文件。
        if params.fit.is_some()
            || params.zoom.is_some()
            || params.offset_x.is_some()
            || params.offset_y.is_some()
            || params.rotation.is_some()
            || params.brightness.is_some()
        {
            self.schedule_config_write(crate::config_store::Preference::OutputTransform {
                output: key.clone(),
            });
        }
        // **t45**：受理与上屏解耦。校验（路径存在 / 类型 / compat / 转码缓存）在这里
        // 做完并立即回 `accepted`；真正的 `show_entry`（建 VA-API/GStreamer 管线 + 等
        // 首帧，真机 0.6–1s）留到 `run_pending_sets()` 里做，随后用
        // `WALLPAPER_APPLIED{request_id}` 事件收尾——客户端不再为一次首帧等 0.6s。
        let resolved = match self.check_media_path(&path) {
            Ok(resolved) => resolved,
            Err(body) => return self.reject_set(id, key, params.path, request_id, body),
        };
        // 只有视频走兼容闸门（图片没有"解码路径"问题，也不能被转码队列接住）。
        let mut kind = media_kind_for_path(&resolved);
        let path = if kind == MediaKind::Video {
            match self.ensure_playable_video(&key, resolved) {
                Ok(path) => path,
                Err(body) => return self.reject_set(id, key, params.path, request_id, body),
            }
        } else {
            resolved
        };
        if kind == MediaKind::Video {
            kind = media_kind_for_path(&path);
        }
        let entry = MediaEntry { kind, path };
        self.pending_sets.push(PendingSet {
            request_id: request_id.clone(),
            output: key,
            entry,
            declared_path: params.path,
            pin: params.pin == Some(true),
            transform_changed,
        });
        // 受理即落盘：`state.json` 的存在与"已受理"同步（重启后即使还没上屏，
        // 也不会退回上一次的壁纸；上屏完成时 `run_pending_sets` 会再存一次游标）。
        self.save_state();
        ok_response(id, &AcceptedResult::accepted_with_request(request_id))
    }

    /// 把一次 `set_wallpaper` 的失败**同步**告诉客户端（t45）。
    ///
    /// 语义与旧实现一致：错误响应 + `WALLPAPER_APPLIED{ok:false}`（下一 tick 发）
    /// + 一条可通知的 `Error` 事件（通知驱动的界面靠它从"已受理"变成"失败"）。
    fn reject_set(
        &mut self,
        id: String,
        key: String,
        declared_path: String,
        request_id: String,
        body: ErrorBody,
    ) -> Response {
        self.events.publish(
            EventMessage::new(EventKind::Error)
                .with_output(key.clone())
                .with_message(format!("壁纸应用失败: {}", body.message)),
        );
        self.pending_applied.push(PendingApplied {
            request_id,
            output: key,
            path: declared_path,
            ok: false,
            error: Some(format!("{}: {}", body.code.as_str(), body.message)),
        });
        Response::err(id, body)
    }

    /// 每 tick 消费一个已受理的 `set_wallpaper`（t45）。
    ///
    /// 建管线是同步的（真机 0.6–1s），一次只做一个：多输出同时 set 时余下的排到
    /// 后续 tick，避免一次 tick 里卡住整个运行时。
    fn run_pending_sets(&mut self) {
        if self.pending_sets.is_empty() {
            return;
        }
        let job = self.pending_sets.remove(0);
        let key = job.output.clone();
        // **t46**：目标画面已经在屏上时不重建管线。
        //
        // 冷启动窗口里 `prime_first_frame` 可能先把同一个媒体铺上了（用户点的正好是
        // 媒体库第一项），旧实现会让 `run_pending_sets` 再 `open` 一次解码器——
        // journal 里出现两条"视频解码管线已建立"、冷启动首次 set 多花几百毫秒。
        // 这里只补 pin/事件/落盘（下面共用同一段收尾逻辑）。
        let already_showing = !job.transform_changed
            && self.channels.get(&key).is_some_and(|ch| {
                ch.degraded_reason.is_none()
                    && (job.entry.kind != MediaKind::Video
                        || ch.video.is_some()
                        || ch.paused_video_media.is_some())
                    && ch
                        .current
                        .as_ref()
                        .is_some_and(|d| d.media == job.entry.path)
            });
        let result = if already_showing {
            tracing::info!(
                output = %key,
                media = %job.entry.path.display(),
                "已受理的壁纸已在屏上：复用现有管线，不重建"
            );
            Ok(())
        } else {
            self.show_entry(&key, &job.entry, true)
        };
        if result.is_ok() {
            let mut persist_pin = false;
            if let Some(ch) = self.channels.get_mut(&key) {
                if job.pin {
                    ch.pinned = true;
                    ch.scheduler.pause();
                    tracing::info!(output = %key, "已锁定为单项播放（不跟随轮换）");
                }
                persist_pin = true;
                if ch.playlist.advance_to(&job.entry.path) {
                    let _ = ch.playlist.next();
                }
            }
            if persist_pin {
                self.save_state();
            }
            // **t39 语义保留**：仅当当前处于纯用户暂停时，显式应用才解除暂停；
            // 若同时存在环境让路（全屏/熄屏/游戏/显存），仍保持由环境控制暂停。
            let user_was_paused = self.pause_reason_of(&key) == Some("user");
            self.user_paused.remove(&key);
            if user_was_paused && !self.has_any_pause_reason(&key) {
                let _ = self.resume_video_supply(&key, Instant::now());
                self.sync_top_level_pause_state();
                tracing::info!(output = %key, "暂停态下显式应用壁纸：已自动恢复播放");
            }
        }
        match result {
            Ok(()) => {
                tracing::info!(
                    output = %key,
                    request_id = %job.request_id,
                    "已受理的壁纸已上屏（异步完成）"
                );
                self.pending_applied.push(PendingApplied {
                    request_id: job.request_id,
                    output: key,
                    path: job.declared_path,
                    ok: true,
                    error: None,
                });
            }
            Err(body) => {
                tracing::warn!(
                    output = %key,
                    request_id = %job.request_id,
                    "已受理的壁纸未能上屏：{}",
                    body.message
                );
                self.events.publish(
                    EventMessage::new(EventKind::Error)
                        .with_output(key.clone())
                        .with_message(format!("壁纸应用失败: {}", body.message)),
                );
                self.pending_applied.push(PendingApplied {
                    request_id: job.request_id,
                    output: key,
                    path: job.declared_path,
                    ok: false,
                    error: Some(format!("{}: {}", body.code.as_str(), body.message)),
                });
            }
        }
    }

    /// 把 `set` 的 fit/zoom/offset 覆盖写入该输出的显示参数。
    ///
    /// **契约**：要么真正生效，要么明确报错 —— 绝不返回 `accepted:true` 却丢掉参数。
    /// - 配置里没有 `[outputs.<name>]` 段时**现场建立条目**（与配置文件等价）；
    /// - `fit` 字符串无法解析、`zoom` 越界（非有限/≤0/> [`liuguang_core::config::MAX_ZOOM`]）、
    ///   偏移非有限 → [`ErrorKind::InvalidParams`]（用户参数问题，不是 INTERNAL）。
    ///
    /// # Errors
    ///
    /// 参数非法时返回 [`ErrorBody`]，调用方必须把它回给客户端。
    /// 把 `SetWallpaperParams` 的适配/旋转字段校验后写进输出配置，返回配置是否发生变化。
    fn apply_overrides(
        &mut self,
        key: &str,
        params: &SetWallpaperParams,
    ) -> Result<bool, ErrorBody> {
        use liuguang_core::config::MAX_ZOOM;
        if let Some(name) = params.fit.as_deref() {
            if parse_fit(name).is_none() {
                return Err(ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!("fit 非法: {name}（可用 cover/contain/stretch/center）"),
                )
                .with_detail_code("INVALID_PARAMS"));
            }
        }
        if let Some(z) = params.zoom {
            if !(z.is_finite() && z > 0.0 && z <= MAX_ZOOM) {
                return Err(ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!("zoom 非法: {z}（要求有限、> 0 且 <= {MAX_ZOOM}）"),
                )
                .with_detail_code("INVALID_PARAMS"));
            }
        }
        for (name, value) in [("offset_x", params.offset_x), ("offset_y", params.offset_y)] {
            if let Some(v) = value {
                if !v.is_finite() {
                    return Err(ErrorBody::new(
                        ErrorKind::InvalidParams,
                        format!("{name} 非法: {v}（要求有限数）"),
                    )
                    .with_detail_code("INVALID_PARAMS"));
                }
            }
        }
        let parsed_rotation = match params.rotation {
            Some(0) => Some(Rotation::Deg0),
            Some(90) => Some(Rotation::Deg90),
            Some(180) => Some(Rotation::Deg180),
            Some(270) => Some(Rotation::Deg270),
            Some(other) => {
                return Err(ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!("rotation 必须是 0/90/180/270（收到 {other}）"),
                )
                .with_detail_code("INVALID_PARAMS"));
            }
            None => None,
        };
        if let Some(b) = params.brightness {
            use liuguang_core::transform::{MAX_BRIGHTNESS, MIN_BRIGHTNESS};
            if !(b.is_finite() && (MIN_BRIGHTNESS..=MAX_BRIGHTNESS).contains(&b)) {
                return Err(ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!(
                        "brightness 非法: {b}（要求有限且在 {MIN_BRIGHTNESS}..={MAX_BRIGHTNESS}）"
                    ),
                )
                .with_detail_code("INVALID_PARAMS"));
            }
        }
        let cfg = self.config.outputs.entry(key.to_string()).or_default();
        let prev = (
            cfg.fit,
            cfg.zoom,
            cfg.offset_x,
            cfg.offset_y,
            cfg.rotation,
            cfg.brightness,
        );
        if let Some(fit) = params.fit.as_deref().and_then(parse_fit) {
            cfg.fit = fit;
        }
        if let Some(z) = params.zoom {
            cfg.zoom = z;
        }
        if let Some(x) = params.offset_x {
            cfg.offset_x = x;
        }
        if let Some(y) = params.offset_y {
            cfg.offset_y = y;
        }
        if let Some(rotation) = parsed_rotation {
            cfg.rotation = rotation;
        }
        if let Some(b) = params.brightness {
            cfg.brightness = b;
        }
        let changed = cfg.fit != prev.0
            || (cfg.zoom - prev.1).abs() > 1e-6
            || (cfg.offset_x - prev.2).abs() > 1e-6
            || (cfg.offset_y - prev.3).abs() > 1e-6
            || cfg.rotation != prev.4
            || (cfg.brightness - prev.5).abs() > 1e-6;
        Ok(changed)
    }

    fn handle_next(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: OutputParams = serde_json::from_value(params).unwrap_or_default();
        let key = match self.resolve_channel(params.output.as_deref()) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        match self.switch_next(&key, true) {
            Ok(()) => ok_response(id, &AcceptedResult::accepted()),
            Err(body) => Response::err(id, body),
        }
    }

    /// 暂停一条通道的视频供给（用户暂停 / 熄屏 / 全屏 / 静态壁纸共用）。
    ///
    /// 三件事必须成对：置 `paused`（tick 不再拉帧）、暂停调度器、暂停播放器
    /// （解码线程不再出帧）。漏任一步都会"看起来暂停了但还在烧 CPU"。
    fn pause_video_supply(&mut self, key: &str) {
        if let Some(ch) = self.channels.get_mut(key) {
            ch.paused = true;
            ch.scheduler.pause();
            if let Some(video) = ch.video.as_mut() {
                let _ = video.pause();
            }
        }
    }

    /// 恢复一条通道的视频供给，并把停摆看门狗交给下一个 tick 重新计时。
    ///
    /// 返回该通道是否有视频管线（调用方据此落到 `VideoPlaying` / `StaticIdle`）。
    ///
    /// **不要在这里写 `Instant::now()` 去重置看门狗**：看门狗的时间基准是
    /// `tick(now)` 注入的时钟（单测会大步推进），而 IPC handler 里的真实时钟
    /// 与它不在同一条时间轴上。暂停期间"presented 不增长"的豁免由
    /// [`Self::step_channel`] 的暂停分支负责——它每个 tick 都推进一次基线，
    /// 因此恢复后的第一个 tick 最多只看到 ~1 个 tick 的间隔，而不是整段暂停时长。
    /// 所有"翻回 `paused = false`"的路径都必须走这里，保证播放器与调度器
    /// 成对恢复（漏掉 `video.resume()` 会让 `poll_frame` 恒 Pending ⇒ 随后被
    /// 看门狗判停摆）。
    fn resume_video_supply(&mut self, key: &str, now: Instant) -> bool {
        let unloaded_media = {
            let Some(ch) = self.channels.get_mut(key) else {
                return false;
            };
            ch.paused = false;
            ch.paused_since = None;
            ch.scheduler.resume(now);
            if let Some(video) = ch.video.as_mut() {
                let _ = video.resume();
                return true;
            }
            ch.paused_video_media.take()
        };
        if let Some(media) = unloaded_media {
            tracing::info!(
                output = %key,
                media = %media.display(),
                "恢复播放：重建此前因长时间暂停卸载的视频管线"
            );
            match self.show_video(key, &media) {
                Ok(()) => {
                    if let Some(ch) = self.channels.get_mut(key) {
                        ch.paused = false;
                        ch.paused_since = None;
                        ch.scheduler.resume(now);
                    }
                    return true;
                }
                Err(err) => {
                    tracing::warn!(
                        output = %key,
                        media = %media.display(),
                        error = %err.message,
                        "恢复播放时重建视频管线失败"
                    );
                    if err.code == ErrorKind::DeviceUnavailable {
                        if let Some(ch) = self.channels.get_mut(key) {
                            ch.paused_video_media = Some(media);
                        }
                    } else {
                        self.handle_video_failure(
                            key,
                            &format!("恢复播放时重建视频管线失败: {}", err.message),
                        );
                    }
                }
            }
        }
        false
    }

    /// 统一的环境暂停来源（用户手动暂停单独记在 `user_paused`）。
    fn pause_channels_for_reason(&mut self, reason: PauseReason) -> Vec<String> {
        let keys: Vec<String> = self
            .channels
            .iter()
            .filter(|(key, ch)| {
                (ch.video.is_some() || ch.paused_video_media.is_some())
                    && match reason {
                        PauseReason::Fullscreen => !self.fullscreen_paused.contains(key.as_str()),
                        PauseReason::ScreenOff => !self.screen_paused.contains(key.as_str()),
                        PauseReason::GameRunning => !self.game_paused.contains(key.as_str()),
                        PauseReason::GpuMemory => !self.gpu_paused.contains(key.as_str()),
                        PauseReason::Battery => !self.battery_paused.contains(key.as_str()),
                    }
            })
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return keys;
        }
        for key in &keys {
            match reason {
                PauseReason::Fullscreen => {
                    self.fullscreen_paused.insert(key.clone());
                }
                PauseReason::ScreenOff => {
                    self.screen_paused.insert(key.clone());
                }
                PauseReason::GameRunning => {
                    self.game_paused.insert(key.clone());
                }
                PauseReason::GpuMemory => {
                    self.gpu_paused.insert(key.clone());
                }
                PauseReason::Battery => {
                    self.battery_paused.insert(key.clone());
                }
            }
            self.pause_video_supply(key);
        }
        self.sync_top_level_pause_state();
        keys
    }

    fn resume_channels_for_reason(&mut self, reason: PauseReason, now: Instant) -> Vec<String> {
        let keys: Vec<String> = match reason {
            PauseReason::Fullscreen => std::mem::take(&mut self.fullscreen_paused)
                .into_iter()
                .collect(),
            PauseReason::ScreenOff => std::mem::take(&mut self.screen_paused)
                .into_iter()
                .collect(),
            PauseReason::GameRunning => std::mem::take(&mut self.game_paused).into_iter().collect(),
            PauseReason::GpuMemory => std::mem::take(&mut self.gpu_paused).into_iter().collect(),
            PauseReason::Battery => std::mem::take(&mut self.battery_paused)
                .into_iter()
                .collect(),
        };
        if keys.is_empty() {
            return keys;
        }
        let mut resumed = Vec::new();
        for key in keys {
            if !self.has_any_pause_reason(&key) {
                let _ = self.resume_video_supply(&key, now);
                resumed.push(key);
            }
        }
        self.sync_top_level_pause_state();
        resumed
    }

    fn has_any_pause_reason(&self, key: &str) -> bool {
        self.user_paused.contains(key)
            || self.fullscreen_paused.contains(key)
            || self.screen_paused.contains(key)
            || self.game_paused.contains(key)
            || self.gpu_paused.contains(key)
            || self.battery_paused.contains(key)
    }

    /// 让顶层状态机与各通道的真实 `paused` 事实保持一致（满足不变式 I1/I2）。
    fn sync_top_level_pause_state(&mut self) {
        let any_paused = self
            .channels
            .values()
            .any(|ch| ch.current.is_some() && ch.paused);
        if any_paused {
            if self.state.current() != State::Paused {
                self.drive(Event::Pause);
            }
            return;
        }
        if self.state.current() != State::Paused {
            return;
        }
        let transitioning = self.channels.values().any(|ch| ch.transition.is_some());
        if transitioning {
            self.drive(Event::Resume);
            self.drive(Event::NextPrepared);
        } else {
            let has_video = self
                .channels
                .values()
                .any(|ch| ch.current.is_some() && ch.video.is_some());
            self.drive(if has_video {
                Event::VideoStarted
            } else {
                Event::StaticShown
            });
        }
    }

    fn handle_pause(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: OutputParams = serde_json::from_value(params).unwrap_or_default();
        let key = match self.resolve_channel(params.output.as_deref()) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        self.user_paused.insert(key.clone());
        self.pause_video_supply(&key);
        self.sync_top_level_pause_state();
        ok_response(id, &AcceptedResult::accepted())
    }

    fn handle_resume(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: OutputParams = serde_json::from_value(params).unwrap_or_default();
        let key = match self.resolve_channel(params.output.as_deref()) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        self.user_paused.remove(&key);
        if !self.has_any_pause_reason(&key) {
            let _ = self.resume_video_supply(&key, Instant::now());
        }
        self.sync_top_level_pause_state();
        ok_response(id, &AcceptedResult::accepted())
    }

    /// `set_pinned`：只放这一项 / 恢复轮换。
    ///
    /// 为什么单独给一个方法而不是只靠 `set_wallpaper` 的 `pin` 参数：
    /// "我现在正在看的这个视频，就让它一直循环" 是**对当前项的操作**，
    /// 不该要求客户端重新发一次完整路径（重新发还会重开解码管线，白闪一下）。
    fn handle_set_pinned(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: liuguang_ipc::protocol::SetPinnedParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                return Response::err(
                    id,
                    ErrorBody::new(ErrorKind::InvalidParams, format!("参数非法: {e}")),
                );
            }
        };
        let key = match self.resolve_channel(params.output.as_deref()) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        let now = Instant::now();
        let has_item = self
            .channels
            .get(&key)
            .map(|ch| ch.current.is_some() || !ch.playlist.is_empty())
            .unwrap_or(false);
        if params.pinned && !has_item {
            return Response::err(
                id,
                ErrorBody::new(
                    ErrorKind::InvalidParams,
                    "当前输出还没有任何媒体项：先 set 一个，再锁定单项播放",
                ),
            );
        }
        let interval = self.effective_interval(&key);
        if let Some(ch) = self.channels.get_mut(&key) {
            ch.pinned = params.pinned;
            if params.pinned {
                // 锁定期间不许定时换片（恢复时按"从此刻起再等一个 interval"重新排期）。
                ch.scheduler.pause();
            } else if !ch.paused && !ch.playlist.is_empty() {
                ch.scheduler.set_interval(interval, now);
                ch.scheduler.resume(now);
            }
        }
        if params.pinned {
            self.clear_preload_if_all_pinned();
        }
        tracing::info!(
            output = %key,
            pinned = params.pinned,
            "单项播放开关已更新"
        );
        self.save_state();
        ok_response(
            id,
            &liuguang_ipc::protocol::SetPinnedResult {
                pinned: params.pinned,
                output: Some(key),
            },
        )
    }

    /// **t69**：设置静态壁纸 —— 暂停播放列表，并把显示内容换成这一张图片。
    ///
    /// 与 [`Self::handle_pause`] 的语义区别：暂停**不改变**当前显示内容，
    /// 本方法**替换**它；因此这里不驱动顶层状态机到 `Paused`，只动通道自己的
    /// 调度器与视频管线（它们是"播放列表的推进"，不是"显示内容的呈现"）。
    ///
    /// 顺序上先校验路径、再暂停播放列表：路径不可用时不能把正在播的壁纸停掉。
    fn handle_set_static_wallpaper(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: liuguang_ipc::SetStaticWallpaperParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                return Response::err(
                    id,
                    ErrorBody::new(ErrorKind::InvalidParams, format!("参数非法: {e}")),
                );
            }
        };
        let key = match self.resolve_channel(None) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        // 先校验：绝对路径 + 落在允许根内 + 可读。
        let path = match self.check_media_path(std::path::Path::new(&params.path)) {
            Ok(p) => p,
            Err(body) => return Response::err(id, body),
        };
        // 静态壁纸**必须是图片**：视频走 set_wallpaper，不该从这条路径进来
        // （否则"静态"语义失效，且 clear 之后无法解释播放列表停在哪一帧）。
        if media_kind_for_path(&path) != MediaKind::Image {
            return Response::err(
                id,
                ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!(
                        "静态壁纸必须是图片（当前是视频或未知类型）: {}",
                        path.display()
                    ),
                ),
            );
        }
        // 暂停播放列表推进（与 handle_pause 同一套动作）。
        let was_paused = self.channels.get(&key).map(|ch| ch.paused).unwrap_or(false);
        self.pause_video_supply(&key);
        // 解码 + 上屏；失败时把暂停状态回滚，避免"壁纸没换成却停住了"。
        if let Err(body) = self.show_image(&key, &path, true) {
            // 回滚必须**连播放器一起**恢复：只翻 `paused` 会让视频源仍处于
            // pause 状态（`poll_frame` 恒 Pending）⇒ 10s 后被停摆看门狗判死；
            // 若原先本就因其它原因暂停，则保持暂停而不误恢复。
            if !was_paused && !self.has_any_pause_reason(&key) {
                self.resume_video_supply(&key, Instant::now());
            }
            self.sync_top_level_pause_state();
            return Response::err(id, body);
        }
        let switched = self
            .channels
            .get_mut(&key)
            .map(|ch| {
                let same = ch.static_wallpaper.as_deref() == Some(path.as_path());
                ch.static_wallpaper = Some(path.clone());
                same
            })
            .unwrap_or(false);
        ok_response(
            id,
            &serde_json::json!({
                "accepted": true,
                "path": path.to_string_lossy(),
                "already_active": switched,
            }),
        )
    }

    /// **t69**：清除静态壁纸，恢复播放列表。
    ///
    /// 幂等：没有静态壁纸时也执行恢复动作 —— 用户点"清除"的意图就是回到播放。
    fn handle_clear_static_wallpaper(&mut self, id: String) -> Response {
        let key = match self.resolve_channel(None) {
            Ok(k) => k,
            Err(body) => return Response::err(id, body),
        };
        let had = self
            .channels
            .get_mut(&key)
            .and_then(|ch| ch.static_wallpaper.take())
            .is_some();
        if !self.has_any_pause_reason(&key) {
            self.resume_video_supply(&key, Instant::now());
        }
        self.sync_top_level_pause_state();
        // 立刻推进到播放列表的下一项：否则静态图会一直留在屏幕上直到下一个调度点。
        let advanced = match self.switch_next(&key, true) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    output = %key,
                    error = %e.message,
                    "清除静态壁纸后立即切换下一项失败，已恢复调度；将在下一个调度点推进"
                );
                false
            }
        };
        ok_response(
            id,
            &serde_json::json!({
                "accepted": true,
                "had_static_wallpaper": had,
                "advanced": advanced,
            }),
        )
    }

    /// 设定运行人格（守护进程入口在读配置后调用一次）。
    ///
    /// 只影响之后 `reload` 重新读配置时的默认路径落地；启动路径由
    /// 入口自己按人格解析。
    pub fn set_persona(&mut self, persona: Persona) {
        self.persona = persona;
    }

    /// 若运行期尚未配置 `[library]`，但磁盘 `config.toml` 已由用户或初始化脚本写好该段，
    /// 直接热采纳磁盘上的 `[library]`，避免"改完配置还要先手工重启服务"。
    fn sync_disk_library_if_missing(&mut self) {
        if self.config.library.is_none()
            && self.config_path.exists()
            && let Ok(disk_cfg) = Config::load(&self.config_path)
            && disk_cfg.library.is_some()
        {
            tracing::info!(
                path = %self.config_path.display(),
                "检测到磁盘配置已补充 [library]，热加载媒体库"
            );
            self.config.library = disk_cfg.library;
            self.media.generated_unix_ms = 0;
        }
    }

    /// 桌面模式（`Persona::User`）添加来源目录时自动建立或放宽 `[library].root`。
    ///
    /// 系统模式保持严格白名单隔离，不自动放宽 root；桌面模式下用户通过图形界面选择自己的
    /// 目录时，若尚无 `[library]` 则以规范化 `$HOME`（目录在其下时）或该目录自身为 root，
    /// 若已有 `root` 且新目录在其外则放宽到两者的公共祖先（绝不退化到 `/`）。
    fn ensure_user_library_covers(&mut self, resolved: &Path) {
        let canon_home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .and_then(|h| std::fs::canonicalize(Path::new(&h)).ok());
        let preferred_root = canon_home
            .as_ref()
            .filter(|home| resolved.starts_with(home))
            .cloned()
            .unwrap_or_else(|| resolved.to_path_buf());

        let Some(lib) = self.config.library.as_mut() else {
            self.config.library = Some(liuguang_core::library::LibraryConfig {
                root: preferred_root,
                image_dirs: Vec::new(),
                video_dirs: Vec::new(),
                recursive: true,
                ..Default::default()
            });
            return;
        };

        let old_root = std::fs::canonicalize(&lib.root).unwrap_or_else(|_| lib.root.clone());
        if !old_root.is_dir()
            && lib.image_dirs.is_empty()
            && lib.video_dirs.is_empty()
            && lib.sources.is_empty()
        {
            lib.root = preferred_root;
            return;
        }
        if resolved.starts_with(&old_root) {
            return;
        }

        let common = Self::common_path_ancestor(&old_root, resolved);
        if common == Path::new("/") || common.as_os_str().is_empty() {
            return;
        }
        let new_root = canon_home
            .filter(|home| {
                old_root.starts_with(home) && resolved.starts_with(home) && common.starts_with(home)
            })
            .unwrap_or(common);

        for dir in lib.image_dirs.iter_mut().chain(lib.video_dirs.iter_mut()) {
            if !dir.is_absolute() {
                *dir = old_root.join(&*dir);
            }
        }
        for src in &mut lib.sources {
            if !src.path.is_absolute() {
                src.path = old_root.join(&src.path);
            }
        }
        lib.root = new_root;
    }

    fn common_path_ancestor(a: &Path, b: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for (ca, cb) in a.components().zip(b.components()) {
            if ca == cb {
                out.push(ca.as_os_str());
            } else {
                break;
            }
        }
        out
    }

    fn handle_reload(&mut self, id: String) -> Response {
        // 待写偏好必须先落盘：否则重载会把刚改的设置覆盖回文件里的旧值
        // （用户视角：改完点重载，设置没了）。传一个已到期的时刻强制落盘。
        self.flush_config_write(Instant::now() + Self::CONFIG_WRITE_DEBOUNCE);
        match Config::load(&self.config_path) {
            Ok(mut new_config) => {
                // reload 读的是"磁盘上的文件"，同样要按人格落地默认路径，
                // 否则用户服务 reload 之后 socket/state 会被写回系统路径。
                new_config.apply_persona_defaults(self.persona);
                self.config = new_config;
                // `[performance]` 也属于"reload 就该生效"的范围（降载开关、缓存预算）。
                self.sync_performance();
                self.log_pending_config("reload");
                // 同步转场默认值与各通道调度间隔。
                self.transitions = transition_settings_from_config(&self.config.transition);
                self.caps =
                    crate::compat::HardwareCaps::detect(self.config.media.transcode_max_width);
                let startup = startup_consumer(&self.config, self.backend.as_ref(), &self.caps);
                self.transcode.reconfigure(
                    &self.config.media.transcode,
                    std::env::var("LIUGUANG_TRANSCODE").ok().as_deref(),
                    &self.config.media.transcode_crf_or_bitrate,
                    self.config.media.transcode_max_width,
                    startup,
                );
                let keys: Vec<String> = self.channels.keys().cloned().collect();
                // T68：播放列表也是配置的一部分 ⇒ reload 必须重新装载，否则
                // "改了配置里的播放列表、reload 之后还是播旧的"。
                let (playlists, playlists_loaded) = load_playlists_from_config(&self.config);
                self.playlists = playlists;
                self.playlists_loaded = playlists_loaded;
                for key in keys {
                    let interval = self.effective_interval(&key);
                    if let Some(ch) = self.channels.get_mut(&key) {
                        ch.scheduler.set_interval(interval, Instant::now());
                    }
                    // 来源可能变了（新增/删除播放列表）：重建游标（保留当前画面）。
                    if let Err(e) = self.rebuild_channel_playlist(&key) {
                        self.note_rotation_error(&key, &e.message);
                    }
                }
                self.added_sources.clear();
                self.removed_sources.clear();
                self.media.generated_unix_ms = 0;
                if self.library().is_some() {
                    let _ = self.start_scan();
                }
                tracing::info!(path = %self.config_path.display(), "配置已重载并原子生效");
                ok_response(id, &AcceptedResult::accepted())
            }
            Err(e) => {
                tracing::error!(path = %self.config_path.display(), error = %e, "配置重载失败，保持当前有效配置");
                Response::err(
                    id,
                    ErrorBody::new(
                        ErrorKind::InvalidParams,
                        format!("配置重载失败（保持当前配置）: {e}"),
                    ),
                )
            }
        }
    }

    /// 防抖窗口：用户拖 SpinButton 时不要每次都写盘。
    ///
    /// 800ms 是"手感与写入次数"的折中：一次拖动通常 5–20 次请求，合并成 1 次写；
    /// 而 800ms 后用户已经看到结果，即使此刻崩溃也只丢最后一次微调。
    const CONFIG_WRITE_DEBOUNCE: Duration = Duration::from_millis(800);

    /// 把运行时的 `TransitionSettings` 同步回 `self.config.transition`。
    ///
    /// 名字解析失败时**保持原值**：宁可配置里留旧效果，也不要把非法字符串写进
    /// 文件（那会让下一次启动直接 `Config::load` 失败）。
    fn sync_transition_config(&mut self) {
        if let Some(effect) = parse_transition_effect(&self.transitions.effect) {
            self.config.transition.effect = effect;
        }
        self.config.transition.duration_ms = self.transitions.duration_ms;
        if let Some(easing) = parse_easing(&self.transitions.easing) {
            self.config.transition.easing = easing;
        }
        self.config.transition.direction = self
            .transitions
            .direction
            .as_deref()
            .and_then(parse_transition_direction);
    }

    /// 记下"某个偏好需要写回"，并把到期时刻**推后**到 `now + 防抖窗口`。
    ///
    /// 连续改动只延长到期时间，不叠加多次写（`BTreeSet` 天然去重）。
    fn schedule_config_write(&mut self, pref: crate::config_store::Preference) {
        self.pending_config.insert(pref);
        self.config_write_due = Some(Instant::now() + Self::CONFIG_WRITE_DEBOUNCE);
    }

    /// 到期就把待写偏好落到 `config.toml`（由 `tick` 驱动）。
    ///
    /// 失败**不改变任何运行行为**：内存里的设置已经生效，只是重启后回到旧值；
    /// 同一种失败只 WARN 一次（配置文件只读时每 800ms 刷一条日志毫无意义）。
    fn flush_config_write(&mut self, now: Instant) {
        let Some(due) = self.config_write_due else {
            return;
        };
        if now < due || self.pending_config.is_empty() {
            return;
        }
        let prefs = std::mem::take(&mut self.pending_config);
        self.config_write_due = None;
        match crate::config_store::persist(&self.config_path, &self.config, &prefs) {
            Ok(()) => {
                self.config_write_warned = false;
                tracing::info!(
                    count = prefs.len(),
                    path = %self.config_path.display(),
                    "运行时偏好已写回配置文件（重启后仍生效）"
                );
            }
            Err(err) => {
                if !self.config_write_warned {
                    self.config_write_warned = true;
                    tracing::warn!(
                        path = %self.config_path.display(),
                        error = %err,
                        "运行时偏好写回失败：本次运行不受影响，但重启后会恢复成文件里的旧值"
                    );
                }
            }
        }
    }

    fn handle_transition(&mut self, id: String, params: serde_json::Value) -> Response {
        let params: TransitionParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                return Response::err(
                    id,
                    ErrorBody::new(ErrorKind::InvalidParams, format!("参数非法: {e}")),
                );
            }
        };
        if params.duration_ms > liuguang_core::config::MAX_TRANSITION_DURATION_MS {
            return Response::err(
                id,
                ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!(
                        "duration_ms 超出范围 0..={}: {}",
                        liuguang_core::config::MAX_TRANSITION_DURATION_MS,
                        params.duration_ms
                    ),
                ),
            );
        }
        if parse_transition_effect(&params.effect).is_none() {
            return Response::err(
                id,
                ErrorBody::new(
                    ErrorKind::InvalidParams,
                    format!("不支持的转场效果: {}", params.effect),
                ),
            );
        }
        if let Some(e) = params.easing.as_deref()
            && parse_easing(e).is_none()
        {
            return Response::err(
                id,
                ErrorBody::new(ErrorKind::InvalidParams, format!("不支持的缓动函数: {e}")),
            );
        }
        if let Some(d) = params.direction.as_deref()
            && parse_transition_direction(d).is_none()
        {
            return Response::err(
                id,
                ErrorBody::new(ErrorKind::InvalidParams, format!("不支持的转场方向: {d}")),
            );
        }
        self.transitions.effect = params.effect.clone();
        self.transitions.duration_ms = params.duration_ms;
        if let Some(e) = params.easing {
            self.transitions.easing = e;
        }
        self.transitions.direction = params.direction;
        tracing::info!(settings = ?self.transitions, "转场设置已更新");
        // 同步进 `self.config` 再排一次写回——否则重启后回到文件里的旧转场
        // （"改完重启又变回去"，真机反馈）。
        self.sync_transition_config();
        self.schedule_config_write(crate::config_store::Preference::Transitions);
        ok_response(id, &AcceptedResult::accepted())
    }

    fn resolve_channel(&self, wanted: Option<&str>) -> Result<String, ErrorBody> {
        if self.channels.is_empty() && !self.backend_available {
            return Err(ErrorBody::new(
                ErrorKind::DeviceUnavailable,
                "显示设备不可用（HeadlessReady，等待设备恢复/热插拔）",
            ));
        }
        if let Some(name) = wanted {
            if self.channels.contains_key(name) {
                return Ok(name.to_string());
            }
            return Err(ErrorBody::new(
                ErrorKind::UnknownOutput,
                format!("输出 {name} 不存在"),
            ));
        }
        self.channels
            .keys()
            .next()
            .cloned()
            .or_else(|| {
                self.backend
                    .default_output(None)
                    .ok()
                    .map(|o| o.to_string())
            })
            .ok_or_else(|| ErrorBody::new(ErrorKind::UnknownOutput, "无可用输出通道"))
    }

    /// 校验媒体路径（§11.3：绝对路径、普通文件、白名单根目录内）。
    fn check_media_path(&self, path: &Path) -> Result<PathBuf, ErrorBody> {
        if !path.is_absolute() {
            return Err(ErrorBody::new(
                ErrorKind::PermissionDenied,
                format!("媒体路径必须为绝对路径: {}", path.display()),
            ));
        }
        let canonical = std::fs::canonicalize(path).map_err(|e| {
            ErrorBody::new(
                ErrorKind::FileUnavailable,
                format!("无法读取 {}: {e}", path.display()),
            )
        })?;
        if !canonical.is_file() {
            return Err(ErrorBody::new(
                ErrorKind::FileUnavailable,
                format!("{} 不是普通文件", canonical.display()),
            ));
        }
        let mut roots: Vec<PathBuf> = if self.config.media.allowed_roots.is_empty() {
            vec![self.config.slideshow.directory.clone()]
        } else {
            self.config.media.allowed_roots.clone()
        };
        // `[library].root` 也是一条允许根：TUI 展示的媒体项来自 `list_media`，
        // 那些路径已过 `MediaRoot::resolve`（root 边界）。若这里不认，就会出现
        // "列表里看得见、点应用却 PERMISSION_DENIED"的自相矛盾（真机踩到）。
        if let Some(cfg) = self.library() {
            roots.push(cfg.root.clone());
        }
        // **t15**：转码缓存目录也是一条允许根 —— 它是 daemon 自己写的
        // （`$XDG_CACHE_HOME/liuguang/transcode/<sha256>.mkv`），播放缓存必须过这道
        // 校验。路径由我们自己的 key 决定，不引入新的外部输入面。
        if !self.transcode.disabled_by_config() {
            roots.push(crate::compat::cache_dir());
        }
        let allowed = roots
            .iter()
            .filter(|root| !root.as_os_str().is_empty())
            .any(|root| {
                let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                !root.as_os_str().is_empty() && canonical.starts_with(&root)
            });
        if !allowed {
            return Err(ErrorBody::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{} 不在允许的媒体目录内（media.allowed_roots / slideshow.directory）",
                    canonical.display()
                ),
            ));
        }
        Ok(canonical)
    }

    /// 启动时恢复上次显示状态。
    fn restore_persisted(&mut self) {
        let Some(state) = load_state(&self.state_file) else {
            tracing::info!(path = %self.state_file.display(), "无持久化状态，等待指令");
            return;
        };
        for ch in &state.channels {
            let params = SetWallpaperParams {
                pin: None,
                path: ch.media.clone(),
                output: Some(ch.output.clone()),
                fit: ch.fit.clone(),
                zoom: Some(ch.zoom),
                offset_x: Some(ch.offset_x),
                offset_y: Some(ch.offset_y),
                rotation: None,
                brightness: Some(ch.brightness.clamp(
                    liuguang_core::transform::MIN_BRIGHTNESS,
                    liuguang_core::transform::MAX_BRIGHTNESS,
                )),
            };
            let key = match self.resolve_channel(Some(&ch.output)) {
                Ok(k) => k,
                Err(e) => {
                    // 输出当前未连接（通道只为已连接输出建立）⇒ 这条"恢复"不会发生。
                    // 必须说清楚，否则日志里只有"尝试恢复"却没有下文，像卡住了。
                    tracing::info!(
                        output = %ch.output,
                        media = %ch.media,
                        reason = %e.message,
                        "跳过持久化状态恢复：该输出当前未连接（热插拔后由首帧/指令重新点亮）"
                    );
                    continue;
                }
            };
            tracing::info!(output = %key, media = %ch.media, "尝试恢复上次显示状态");
            if let Err(e) = self.apply_overrides(&key, &params) {
                tracing::warn!(output = %key, error = %e.message, "持久化的显示参数非法，已忽略");
            }
            // 旋转也按"最近一次用户动作"恢复：`state.json` 比 `config.toml` 新。
            // 非 0 度才覆盖，避免把配置里的旋转随手抹成 0（v2 文件没有这个字段）。
            if ch.rotation != Rotation::Deg0 {
                self.config.outputs.entry(key.clone()).or_default().rotation = ch.rotation;
            }
            // 类型判定与 `set_wallpaper`/播放列表共用同一份实现（t29 F1）：
            // 转码缓存是 `.mkv`，以前这里硬编码只认 mp4/m4v/mov，重启后就把缓存
            // 当图片解码（"Invalid PNG signature"）并卡在 InitializingDisplay。
            let entry = MediaEntry {
                path: PathBuf::from(&ch.media),
                kind: media_kind_for_path(Path::new(&ch.media)),
            };
            // 上次是"只放这一项"就继续保持（否则重启后画面还停在那一项，但 20 分钟后
            // 突然换片，用户会以为设置丢了）。
            //
            // **必须放在 show_entry 之前**：`show_entry` 内部会 `save_state()`，
            // 先写盘就会把文件里的 `pinned` 覆盖成 false —— 于是"第一次重启还在锁、
            // 第二次重启就丢了"（真机验证到的坑）。
            if ch.pinned {
                if let Some(state) = self.channels.get_mut(&key) {
                    state.pinned = true;
                    state.scheduler.pause();
                }
                tracing::info!(output = %key, "恢复：保持单项播放（不跟随轮换）");
            }
            if let Err(e) = self.show_entry(&key, &entry, false) {
                tracing::warn!(output = %key, error = %e.message, "恢复失败，尝试 fallback 图片");
                let _ = self.try_fallback(&key);
            }
        }
    }

    /// 显示配置的 fallback 图片（若有）。
    ///
    /// 返回**是否真的成功显示了后备画面**：调用方据此决定能不能宣布"已恢复"
    /// （没有后备画面时仍然谎称 StaticIdle 就是状态说谎）。
    fn try_fallback(&mut self, key: &str) -> bool {
        let fallback = self
            .config
            .outputs
            .get(key)
            .and_then(|c| c.fallback_image.clone());
        if let Some(path) = fallback {
            let entry = MediaEntry {
                path,
                kind: MediaKind::Image,
            };
            match self.show_entry(key, &entry, false) {
                Ok(()) => {
                    tracing::info!(output = %key, "已显示 fallback 图片");
                    true
                }
                Err(e) => {
                    tracing::error!(output = %key, error = %e.message, "fallback 图片显示失败");
                    self.events.publish(
                        EventMessage::new(EventKind::Error)
                            .with_output(key)
                            .with_message(format!("fallback 失败: {}", e.message)),
                    );
                    false
                }
            }
        } else {
            tracing::warn!(output = %key, "无 fallback 配置，保持当前画面/等待指令");
            false
        }
    }

    /// 顶层窗口采样间隔。
    ///
    /// 500ms 是"游戏切全屏到壁纸停下"的体感上限与后台开销的折中：
    /// 每次只是读一遍内存里的事件快照（`wl.toplevels`），不额外往返合成器。
    const FULLSCREEN_POLL: Duration = Duration::from_millis(500);
    /// 滞回窗口：全屏状态要持续这么久才真的暂停/恢复。
    ///
    /// 为什么需要：alt-tab、切换工作区、窗口动画都会让 fullscreen 位短暂跳动；
    /// 没有滞回就会出现"每秒暂停又恢复"，解码管线反复重建比一直播还费。
    const FULLSCREEN_HYSTERESIS: Duration = Duration::from_millis(1000);

    /// 兜底探测间隔。只在"确信有全屏窗口"时才跑，因此 2s 一次的成本可以忽略。
    const FULLSCREEN_VERIFY: Duration = Duration::from_millis(2000);

    /// 采样顶层窗口并按需暂停/恢复视频。
    ///
    /// 每 [`Self::FULLSCREEN_POLL`] 采一次；状态变化要持续
    /// [`Self::FULLSCREEN_HYSTERESIS`] 才生效（去抖）。
    /// 兜底确认：合成器有没有其实已经退出全屏了。
    ///
    /// 背景（真机复现，niri 26.04）：`zwlr_foreign_toplevel_management` 在**进入**全屏时
    /// 会发 `state=[3, …]`，**退出时不发任何事件**。只信协议的话，打完游戏壁纸会永久停在
    /// 暂停——比不暂停更糟。所以每 `FULLSCREEN_VERIFY` 问一次合成器自己的窗口几何
    /// （见 `fullscreen_probe`）；问不到就保持现状（宁可多暂停，也不和游戏抢 CPU）。
    fn fullscreen_exit_confirmed(
        &mut self,
        now: Instant,
        toplevel: &liuguang_display::FullscreenToplevel,
    ) -> bool {
        if !crate::fullscreen_probe::available() {
            return false;
        }
        if let Some(due) = self.fullscreen_verify_due {
            if now < due {
                return false;
            }
        }
        self.fullscreen_verify_due = Some(now + Self::FULLSCREEN_VERIFY);
        let probe = crate::fullscreen_probe::probe(toplevel);
        match probe {
            crate::fullscreen_probe::Probe::StillFullscreen => {
                tracing::trace!(window = %toplevel.describe(), "兜底探测：仍然全屏");
                false
            }
            crate::fullscreen_probe::Probe::Unknown => {
                tracing::debug!(window = %toplevel.describe(), "兜底探测不可用，保持暂停");
                false
            }
            exit => {
                tracing::debug!(?exit, window = %toplevel.describe(), "兜底探测：已退出全屏");
                true
            }
        }
    }

    fn poll_fullscreen(&mut self, now: Instant) {
        if !self.config.performance.pause_on_fullscreen {
            // 关掉这项时也要把之前因全屏暂停的通道恢复回来，
            // 否则"关掉开关"反而让壁纸永远停着。
            if self.fullscreen_active || !self.fullscreen_paused.is_empty() {
                self.fullscreen_active = false;
                self.fullscreen_since = None;
                self.fullscreen_info = None;
                self.resume_after_fullscreen();
            }
            return;
        }
        // t43 验收④：可注入的"有窗口全屏"——钩子期间完全按真实全屏路径走
        // （暂停在播的通道、暂停原因记成 fullscreen、到期自动恢复）。
        if let Some(until) = self.test_fullscreen_until {
            if now < until {
                if !self.fullscreen_active {
                    self.fullscreen_active = true;
                    self.fullscreen_since = Some(now);
                    tracing::info!("（测试钩子）按\"有窗口全屏\"处理");
                }
                // 钩子期间**每个 tick**都确保在播的通道被让路：钩子可能在视频
                // 起来之前就生效（headless 下第一个 tick 就触发），只暂停一次会漏。
                self.pause_for_fullscreen(now);
                if !self.fullscreen_paused.is_empty() && self.state.current() != State::Paused {
                    self.drive(Event::Pause);
                }
                return;
            }
            self.test_fullscreen_until = None;
            if self.fullscreen_active {
                self.fullscreen_active = false;
                self.fullscreen_since = None;
                self.fullscreen_info = None;
                tracing::info!("（测试钩子）全屏结束：恢复视频供给");
                self.resume_after_fullscreen();
            }
            return;
        }
        if !self.test_fullscreen_applied {
            if let Some(ms) = env_millis(TEST_FORCE_FULLSCREEN_MS).filter(|ms| *ms > 0) {
                self.test_fullscreen_applied = true;
                self.test_fullscreen_until = Some(now + Duration::from_millis(ms));
                self.fullscreen_active = true;
                self.fullscreen_since = Some(now);
                tracing::info!(ms, "（测试钩子）开始强制全屏");
                self.pause_for_fullscreen(now);
                return;
            }
        }
        if let Some(last) = self.fullscreen_polled {
            if now.duration_since(last) < Self::FULLSCREEN_POLL {
                return;
            }
        }
        self.fullscreen_polled = Some(now);

        let Some(toplevel) = self.backend.fullscreen_toplevel() else {
            // 没有全屏窗口**或**后端不提供该信息：两者都按"不在全屏"处理，
            // 并清除滞回计时与抑制签名，避免下次瞬态误触。
            self.fullscreen_since = None;
            self.fullscreen_suppressed = None;
            if self.fullscreen_active {
                self.fullscreen_active = false;
                self.fullscreen_info = None;
                self.resume_after_fullscreen();
            }
            return;
        };

        let described = toplevel.describe();
        if self.fullscreen_suppressed.as_deref() == Some(described.as_str()) {
            return;
        }
        if self.fullscreen_suppressed.is_some() {
            self.fullscreen_suppressed = None;
        }

        if self.fullscreen_active {
            // 仍在全屏：刷新快照（标题/输出可能变），保持暂停。
            self.fullscreen_info = Some(toplevel.clone());
            if self.fullscreen_exit_confirmed(now, &toplevel) {
                tracing::info!(
                    window = %described,
                    "全屏窗口已退出（合成器未发状态事件，由 niri IPC 兜底确认）：恢复视频供给"
                );
                self.fullscreen_active = false;
                self.fullscreen_since = None;
                self.fullscreen_info = None;
                self.fullscreen_verify_due = None;
                self.fullscreen_suppressed = Some(described);
                self.resume_after_fullscreen();
            }
            return;
        }
        match self.fullscreen_since {
            // 还在滞回窗口内：这次不判定（第一次看到只记时间）。
            Some(since) if now.duration_since(since) < Self::FULLSCREEN_HYSTERESIS => {}
            Some(_) => {
                self.fullscreen_active = true;
                self.fullscreen_info = Some(toplevel);
                self.pause_for_fullscreen(now);
            }
            None => self.fullscreen_since = Some(now),
        }
    }

    /// 进入全屏：暂停所有正在播视频的通道。
    fn pause_for_fullscreen(&mut self, _now: Instant) {
        let keys = self.pause_channels_for_reason(PauseReason::Fullscreen);
        if keys.is_empty() {
            tracing::debug!("检测到全屏窗口，但没有需要新增暂停的视频通道");
            return;
        }
        let described = self
            .fullscreen_info
            .as_ref()
            .map(liuguang_display::FullscreenToplevel::describe)
            .unwrap_or_else(|| "(未命名)".to_string());
        tracing::info!(
            paused = keys.len(),
            outputs = ?keys,
            window = %described,
            "有窗口全屏：暂停视频供给（给游戏让出 CPU/GPU；退出全屏后自动恢复）"
        );
    }

    /// 显存压力让路：超过上阈值暂停在播视频，回落到下阈值以下再恢复。
    fn poll_gpu_pressure(&mut self, now: Instant) {
        match self.gpu.poll(now) {
            crate::gpu_pressure::Action::Unchanged => {}
            crate::gpu_pressure::Action::Pause => {
                let keys = self.pause_channels_for_reason(PauseReason::GpuMemory);
                if !keys.is_empty() {
                    tracing::info!(
                        usage = ?self.gpu.usage(),
                        outputs = ?keys,
                        "显存占用过高：暂停视频供给给其他 GPU 任务让路"
                    );
                }
            }
            crate::gpu_pressure::Action::Resume => {
                let keys = self.resume_channels_for_reason(PauseReason::GpuMemory, now);
                if !keys.is_empty() {
                    tracing::info!(
                        usage = ?self.gpu.usage(),
                        outputs = ?keys,
                        "显存占用已回落：恢复视频供给"
                    );
                }
            }
        }
    }

    /// 游戏进程让路：检测到游戏在跑就暂停视频供给，游戏结束后恢复自己停的通道。
    fn poll_game_running(&mut self, now: Instant) {
        match self.game.poll(now) {
            crate::game_watch::Action::Unchanged => {}
            crate::game_watch::Action::Pause => {
                let keys = self.pause_channels_for_reason(PauseReason::GameRunning);
                if !keys.is_empty() {
                    tracing::info!(
                        games = ?self.game.hits().iter().map(|h| (h.pid, h.name.clone(), h.evidence.clone())).collect::<Vec<_>>(),
                        outputs = ?keys,
                        "检测到游戏进程：暂停视频供给让路"
                    );
                }
            }
            crate::game_watch::Action::Resume => {
                let keys = self.resume_channels_for_reason(PauseReason::GameRunning, now);
                if !keys.is_empty() {
                    tracing::info!(?keys, "游戏进程已退出：恢复视频供给");
                }
            }
        }
    }

    /// 电源供电让路：笔记本切换为电池供电时暂停视频供给，插回电源后自动恢复。
    fn poll_power_supply(&mut self, now: Instant) {
        match self.power.poll(now) {
            crate::power_watch::Action::Unchanged => {}
            crate::power_watch::Action::Pause => {
                let keys = self.pause_channels_for_reason(PauseReason::Battery);
                if !keys.is_empty() {
                    tracing::info!(?keys, "已切换为电池供电：暂停动态壁纸供给以节省续航");
                }
            }
            crate::power_watch::Action::Resume => {
                let keys = self.resume_channels_for_reason(PauseReason::Battery, now);
                if !keys.is_empty() {
                    tracing::info!(?keys, "已接通外接电源：恢复动态壁纸供给");
                }
            }
        }
    }

    fn resume_after_fullscreen(&mut self) {
        let keys = self.resume_channels_for_reason(PauseReason::Fullscreen, Instant::now());
        if !keys.is_empty() {
            tracing::info!(resumed = keys.len(), outputs = ?keys, "全屏已退出：恢复视频供给");
        }
    }

    /// 该通道的暂停原因（供 `status` 展示，`None` = 未暂停）。
    ///
    /// 优先级：全屏 > 熄屏 > 游戏运行 > 显存压力 > 电池供电 > 用户手动。
    fn pause_reason_of(&self, key: &str) -> Option<&'static str> {
        if self.fullscreen_paused.contains(key) {
            return Some("fullscreen");
        }
        if self.screen_paused.contains(key) {
            return Some("screen_off");
        }
        if self.game_paused.contains(key) {
            return Some("game_running");
        }
        if self.gpu_paused.contains(key) {
            return Some("gpu_memory");
        }
        if self.battery_paused.contains(key) {
            return Some("battery");
        }
        if self.user_paused.contains(key) || self.channels.get(key).is_some_and(|ch| ch.paused) {
            return Some("user");
        }
        None
    }

    /// 保存运行状态（原子写；失败只记录）。
    fn save_state(&self) {
        let channels: Vec<PersistedChannel> = self
            .channels
            .iter()
            .filter_map(|(key, ch)| {
                ch.current.as_ref().map(|d| PersistedChannel {
                    output: key.clone(),
                    media: d.media.display().to_string(),
                    fit: self
                        .config
                        .outputs
                        .get(key)
                        .map(|o| o.fit.as_str().to_string()),
                    zoom: self.config.outputs.get(key).map(|o| o.zoom).unwrap_or(1.0),
                    offset_x: self
                        .config
                        .outputs
                        .get(key)
                        .map(|o| o.offset_x)
                        .unwrap_or(0.0),
                    offset_y: self
                        .config
                        .outputs
                        .get(key)
                        .map(|o| o.offset_y)
                        .unwrap_or(0.0),
                    mode: self.config.display.mode.clone(),
                    pinned: ch.pinned,
                    rotation: self
                        .config
                        .outputs
                        .get(key)
                        .map(|o| o.rotation)
                        .unwrap_or(Rotation::Deg0),
                    brightness: self
                        .config
                        .outputs
                        .get(key)
                        .map(|o| o.brightness)
                        .unwrap_or(1.0),
                })
            })
            .collect();
        // **t45**：受理但还没上屏的 `set_wallpaper` 也要落盘——否则真机在"点了应用、
        // 管线还在建"的窗口里重启 daemon，就会退回上一张壁纸（集成测试
        // `m2_state_persistence_across_restart` 复现）。这里把 pending 的意图合成
        // 一条既没有 `current` 的通道记录，语义与"上次想显示这张"一致。
        let mut channels = channels;
        for job in &self.pending_sets {
            if channels.iter().any(|ch| ch.output == job.output) {
                continue;
            }
            channels.push(PersistedChannel {
                output: job.output.clone(),
                media: job.declared_path.clone(),
                fit: self
                    .config
                    .outputs
                    .get(&job.output)
                    .map(|o| o.fit.as_str().to_string()),
                zoom: self
                    .config
                    .outputs
                    .get(&job.output)
                    .map(|o| o.zoom)
                    .unwrap_or(1.0),
                offset_x: self
                    .config
                    .outputs
                    .get(&job.output)
                    .map(|o| o.offset_x)
                    .unwrap_or(0.0),
                offset_y: self
                    .config
                    .outputs
                    .get(&job.output)
                    .map(|o| o.offset_y)
                    .unwrap_or(0.0),
                mode: self.config.display.mode.clone(),
                pinned: job.pin,
                rotation: self
                    .config
                    .outputs
                    .get(&job.output)
                    .map(|o| o.rotation)
                    .unwrap_or(Rotation::Deg0),
                brightness: self
                    .config
                    .outputs
                    .get(&job.output)
                    .map(|o| o.brightness)
                    .unwrap_or(1.0),
            });
        }
        let state = PersistedState {
            version: STATE_VERSION,
            channels,
        };
        if let Err(e) = save_state_file(&self.state_file, &state) {
            tracing::warn!(path = %self.state_file.display(), error = %e, "运行状态持久化失败");
        }
    }
}

fn ok_response<T: Serialize>(id: String, value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(
            id,
            ErrorBody::new(ErrorKind::Internal, format!("结果序列化失败: {e}")),
        ),
    }
}

/// 错误响应（稳定机器码由 `ErrorKind` 提供）。
fn err_response(id: String, kind: ErrorKind, message: &str) -> Response {
    Response::err(id, ErrorBody::new(kind, message))
}

/// 错误响应（`ErrorKind` + **细粒度机器码**；TUI 优先用 `detail_code` 分支）。
fn err_response_code(id: String, kind: ErrorKind, detail_code: &str, message: &str) -> Response {
    Response::err(
        id,
        ErrorBody::new(kind, message).with_detail_code(detail_code),
    )
}

fn save_state_file(path: &Path, state: &PersistedState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn load_state(path: &Path) -> Option<PersistedState> {
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| tracing::warn!(path = %path.display(), error = %e, "状态文件不可读"))
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| tracing::warn!(path = %path.display(), error = %e, "状态文件损坏，忽略"))
        .ok()?;
    match value.get("version").and_then(|v| v.as_u64()) {
        // v2/v3 都能读：v2 缺 `rotation` 字段，靠 `#[serde(default)]` 落成 0 度。
        Some(v) if v == 2 || v == u64::from(STATE_VERSION) => {
            serde_json::from_value::<PersistedState>(value)
                .map_err(|e| tracing::warn!(error = %e, "状态文件解析失败，忽略"))
                .ok()
        }
        Some(1) => {
            // v1 → v2 迁移：单输出字段。
            tracing::info!("迁移 v1 状态文件");
            let ch = PersistedChannel {
                output: value.get("output")?.as_str()?.to_string(),
                media: value.get("media")?.as_str()?.to_string(),
                fit: value.get("fit").and_then(|v| v.as_str()).map(String::from),
                zoom: value.get("zoom").and_then(|v| v.as_f64()).unwrap_or(1.0),
                offset_x: value
                    .get("offset_x")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                offset_y: value
                    .get("offset_y")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                mode: value
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("preferred")
                    .to_string(),
                pinned: false,
                rotation: Rotation::Deg0,
                brightness: 1.0,
            };
            Some(PersistedState {
                version: STATE_VERSION,
                channels: vec![ch],
            })
        }
        other => {
            tracing::warn!(version = ?other, "状态文件版本不匹配，忽略");
            None
        }
    }
}

/// 视频无明确到期时间时的兜底唤醒间隔（生产者暂无数据：短间隔回探）。
const VIDEO_POLL_FALLBACK: Duration = Duration::from_millis(5);
/// 刚呈现一帧但没有"下一帧期限"时的回探间隔（队列已空，需尽快回填）。
const VIDEO_PROBE_WAIT: Duration = Duration::from_millis(2);
/// 转场动画的帧间隔（约 120fps 上限，转场期间的高频节拍）。
const TRANSITION_FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// 最短休眠（避免空转）。
const MIN_TICK_WAIT: Duration = Duration::from_millis(2);

/// 缺 `[library]` 时 `NOT_CONFIGURED` 报错里附带的**可操作指引**（t61）。
///
/// 为什么写进 `message`：`ErrorBody` 没有 hint/remedy 字段（`crates/liuguang-ipc/src/protocol.rs:185-203`），
/// 而 `[library]` 是**可选段** ⇒ 缺段时 `liuguang check-config` 仍报"配置有效"、
/// 服务也照常启动，用户唯一能看到线索的地方就是这条报错（TUI 直接显示 message）。
/// 文案要求：最小示例 + 指向随附示例配置 + 改完重启，且短到能一行读完。
const LIBRARY_NOT_CONFIGURED_HINT: &str = "未配置 [library]（媒体库功能未启用）。启用方式：在配置里加 [library] 段（至少 root 与一个目录，\
     例如 root = \"/srv/wallpapers\"、image_dirs = [\"/srv/wallpapers/images\"]）；完整示例见 \
     config/example.toml；改后重启服务。";
/// 连续多少次扫描输出提交失败后收敛到 `handle_video_failure()`（与停摆看门狗同一出口）。
///
/// **为什么需要独立于 `presented` 的探测器**：`presented` 在"帧出队"时自增，
/// 提交失败时它**照涨**，所以"零增长"型看门狗对这一类故障完全失明
/// （tiled modifier 被 AddFB2 拒、DRM master 丢失 EACCES、原子提交失败 —— 三者同形）。
const PRESENT_FAILURE_CONVERGE_THRESHOLD: u32 = 3;

/// 前多少次提交失败要带**细节**（output/mode/what/fourcc/modifier/平面/偏移）告警。
const PRESENT_FAILURE_LOUD_BUDGET: u64 = 3;

/// 打开 VA-API 生产者后**等首帧**的上限。
///
/// 生产者把解码搬进了预读线程（0.2.0-13），`next_producer_frame` 立即返回
/// `NeedMore` 直到第一帧转换完成 —— 所以这里必须有界等待。只轮询一次的旧写法
/// 会把每个节点都误判成"该节点的 DMA-BUF 修饰符不被后端接受"，整机白白回落到
/// GStreamer 的 CPU 路径（真机踩过：status 从 `vaapi:nvdec(dmabuf-rgb)` 变成
/// `vaapi:vah264dec(rgba)`）。
///
/// 500ms 足够覆盖"起线程 + 解一帧 + NV12→XRGB 转换 + glFinish"（实测 <10ms），
/// 又短到不会让启动明显变慢。
#[cfg(feature = "vaapi")]
const VAAPI_FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(200);

/// 单通道**生命周期**内显示层对账 WARN 的硬上限（episode 预算之外的兜底）。
///
/// episode 预算会被"失败 → 干净 → 失败"的长期交替反复重置（间歇性故障在真机上
/// 可以跑几小时），单靠它无法保证日志有界。这个总量上限保证**任何**情况下该通道
/// 的这类告警都是有限的；需要更多细节时用 `RUST_LOG=debug` 复现，而不是刷 WARN。
const PRESENT_FAILURE_LOUD_TOTAL_BUDGET: u64 = 30;

/// DMA-BUF 被显示侧拒绝时，`ErrorBody.detail_code` 的稳定机器码。
///
/// 与 [`liuguang_display::DisplayError::DmaBufRejected`] 一一对应：调用方（daemon）
/// 靠它把"直通不被接受"与"真正的提交失败"分开——前者**不该**把视频降级成静态，
/// 重开一个 CPU 生产者就能继续播。字符串常量放在这里，避免两处以字符串字面量
/// 各自漂移（显示侧的类型化错误是唯一来源）。
const DMABUF_REJECTED_CODE: &str = "DMABUF_REJECTED";

/// 未持有 DRM master 时，`ErrorBody.detail_code` 的稳定机器码。
///
/// 与 [`liuguang_display::DisplayError::MasterUnavailable`] 一一对应。它表示
/// **环境暂时不可用**（旧进程还没释放 master、VT 切换中等），不是帧/平面的
/// 问题 ⇒ daemon **不把它计入提交失败、不降级**，master 取回后帧泵自动续上。
const MASTER_UNAVAILABLE_CODE: &str = "MASTER_UNAVAILABLE";

/// DMA-BUF 被拒后，等待新（CPU）管线投产的重探间隔。
const VIDEO_DMABUF_LATCH_REPOLL: Duration = Duration::from_millis(20);

/// 闩死之后，每多少次被拒仍打一条摘要 WARN（其余降 `debug`；首次闩死单独一条）。
const DMABUF_REJECT_LOUD_EVERY: u64 = 60;

/// 把帧的关键信息压成一行，供提交失败告警使用（DMA-BUF 要能看出 tiled/平面布局）。
fn describe_frame_for_log(frame: &DisplayFrame) -> String {
    match frame {
        DisplayFrame::Cpu(cpu) => format!(
            "Cpu {}x{} {:?} stride={}",
            cpu.width, cpu.height, cpu.format, cpu.stride
        ),
        DisplayFrame::DmaBuf(d) => format!(
            "DmaBuf {}x{} fourcc=0x{:08X} modifier=0x{:016X} stride={} strides={:?} offsets={:?}",
            d.width, d.height, d.fourcc, d.modifier, d.stride, d.strides, d.offsets
        ),
        other => format!("{other:?}"),
    }
}

/// 视频停摆看门狗阈值：视频在跑、未暂停、无转场，但 `presented` 连续这么久
/// 零增长 ⇒ 判为停摆（兜底，防"解码器卡死但永不报错"这类无信号故障）。
///
/// 取值必须**远大于合法的最慢帧间隔**，否则会误伤：
/// - `video_fps_limit = 1` 是合法配置（1 帧/秒）；
/// - 4K 软解首帧/seek 回填可达数百毫秒。
///
/// 10 s 同时覆盖这两者，仍能把"永久冻结"在一个可接受的时间内暴露出来。
const VIDEO_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// tick 间隔超过这个值就认为"进程根本没在跑"（休眠 / `SIGSTOP` / 调试暂停），
/// 而不是"视频供给停了"（t39）。
///
/// 正常唤醒间隔是帧节拍（8–33ms）或空闲 1s；3s 已经比最长空闲唤醒大 3 倍，
/// 不会误伤正常抖动，又能覆盖"冻结十几秒"这种真实场景。
const TICK_GAP_RESET: std::time::Duration = std::time::Duration::from_secs(3);

/// 启动后这段"安静期"内没有 IPC 请求才做启动首帧（t46）。
///
/// prime 会同步建解码管线（真机 0.6–1s）；若客户端（GUI/liuguangctl）启动后立刻
/// `set_wallpaper`，prime 抢在请求前面建管线会让回执从几毫秒涨到 0.7s。
const PRIME_QUIET: std::time::Duration = std::time::Duration::from_millis(1500);

/// t43 验收④ 的可注入钩子：环境变量名。
///
/// - `LIUGUANG_TEST_FORCE_FULLSCREEN_MS=<n>`：接下来 n 毫秒按"有窗口全屏"处理
///   （headless 合成器没有 foreign-toplevel 信息，真机上的全屏让路没法在 CI 里跑）。
/// - `LIUGUANG_TEST_TICK_GAP_MS=<n>`：下一次 tick 假装距上次已经过了 n 毫秒
///   （模拟休眠/`kill -STOP` 造成的长间隔，只消费一次）。
///
/// 两者都只在显式设置时生效，正常运行完全不受影响；用途是把"需要真实会话才能
/// 触发"的路径变成可复现的测试（证据见 docs/ARCHITECTURE.md 的 t43 段）。
const TEST_FORCE_FULLSCREEN_MS: &str = "LIUGUANG_TEST_FORCE_FULLSCREEN_MS";
const TEST_TICK_GAP_MS: &str = "LIUGUANG_TEST_TICK_GAP_MS";

/// 读一个"毫秒数"环境变量（只认非负整数，其它一律当作没设）。
fn env_millis(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse::<u64>().ok()
}

/// 停摆后**自动重建**视频管线的最大连续次数（中间一帧都没出）。
///
/// 为什么需要自愈而不是直接降级：pause/resume、熄屏/亮屏、全屏让路恢复后，
/// 管线可能已经不是"还在跑只是没帧"，而是确实死了（解码线程退出/生产者被回收）。
/// 此时用户什么都没做，重启服务才能恢复是不可接受的；重开一次管线代价很小。
///
/// 为什么必须有上限：若重建**成功**但依旧不出帧（例如媒体文件损坏、硬件解码器
/// 持续失败），无上限重开会变成每 10s 一次"已重建"的日志风暴，并且永远不收敛到
/// `DegradedStatic`。两次足够覆盖"第一次重建撞上瞬时错误"的场景。
const MAX_STALL_REBUILDS: u32 = 2;

/// 空闲休眠（仅查信号与轮换调度）。
const IDLE_WAIT: Duration = Duration::from_secs(1);
/// 后台扫描进行中的回探间隔（非阻塞 `try_recv`，成本可忽略）。
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// DRM master 自愈的检查间隔（t26）。
///
/// 与显示侧的 [`liuguang_display`] `MASTER_RETRY_INTERVAL = 2s` 同量级：
/// 空闲时 tick 至少每秒一次（`IDLE_WAIT`），因此这里按 2s 限流即可。
/// 自愈本身只在"未确认持有 master"时才动手（已持有则后端立即返回 false）。
const MASTER_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 播放列表**一条可用条目都没有**（空列表，或所有条目的路径都不可用）的稳定机器码。
///
/// T68 契约：这两种情况必须**明确报错**，绝不静默退回 `[slideshow].directory`
/// 轮换（"用户加了播放列表，播的还是目录"正是要消灭的故障）。
const PLAYLIST_EMPTY_CODE: &str = "PLAYLIST_EMPTY";
/// 播放列表存在条目、但**全部**不可用（文件被删/越界/权限）的稳定机器码。
const PLAYLIST_ALL_INVALID_CODE: &str = "PLAYLIST_ALL_INVALID";

/// 视频"播完一轮"判定的 PTS 容差（T68）。
///
/// `VideoStats` 里**没有**时长/EOS 计数，唯一可用的"源回卷"信号是
/// [`VideoStats::last_pts`] 明显倒退（生产者在源尾 `restart_loop` 后 PTS 跳回 0）。
/// 取 250ms：远大于任何合法的帧间抖动（4K60 = 16.7ms；B 帧重排也在几十 ms 内），
/// 又远小于真实视频的时长 ⇒ 不误判、也不漏判。
const VIDEO_LOOP_PTS_TOLERANCE: Duration = Duration::from_millis(250);

/// 坏播放列表（`PLAYLIST_EMPTY` / `PLAYLIST_ALL_INVALID`）的重试间隔。
///
/// 目的：文件被放回去后**自动恢复**，而不是要用户重启 daemon；同时避免每 tick
/// 重扫（`tick` 在视频播放时是毫秒级频率）。
const PLAYLIST_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// 从配置段构造运行期 [`TransitionSettings`]。
fn transition_settings_from_config(
    cfg: &liuguang_core::config::TransitionConfig,
) -> TransitionSettings {
    TransitionSettings {
        effect: match cfg.effect {
            liuguang_core::config::TransitionEffect::Fade => "fade".into(),
            liuguang_core::config::TransitionEffect::Slide => "slide".into(),
            liuguang_core::config::TransitionEffect::Wipe => "wipe".into(),
            liuguang_core::config::TransitionEffect::Circle => "circle".into(),
            liuguang_core::config::TransitionEffect::Random => "random".into(),
            liuguang_core::config::TransitionEffect::None => "none".into(),
        },
        duration_ms: cfg.duration_ms,
        easing: match cfg.easing {
            liuguang_core::config::Easing::Linear => "linear".into(),
            liuguang_core::config::Easing::EaseIn => "ease-in".into(),
            liuguang_core::config::Easing::EaseOut => "ease-out".into(),
            liuguang_core::config::Easing::EaseInOut => "ease-in-out".into(),
        },
        direction: cfg.direction.map(|d| match d {
            liuguang_core::config::TransitionDirection::Left => "left".into(),
            liuguang_core::config::TransitionDirection::Right => "right".into(),
            liuguang_core::config::TransitionDirection::Up => "up".into(),
            liuguang_core::config::TransitionDirection::Down => "down".into(),
        }),
    }
}

/// 按扩展名判定媒体类型（**唯一规则**：`set_wallpaper` 与播放列表共用）。
///
/// 与旧 `handle_set` 的内联判定保持一致（`mp4/m4v/mov` 为视频，其余按图片处理）；
/// 图片解码器只支持 PNG，非 PNG 会在 `prepare_image` 处**明确报错**而不是静默跳过。
/// 转场效果名 → 枚举。
///
/// 复用 serde 的 `rename` 映射（`serde_json::from_value`）而不是手写 match：
/// 配置文件的写法只有一处定义，这里跟着走就不会漂。
fn parse_transition_effect(name: &str) -> Option<liuguang_core::config::TransitionEffect> {
    serde_json::from_value(serde_json::Value::String(name.trim().to_ascii_lowercase())).ok()
}

/// 缓动名 → 枚举（同上）。
fn parse_easing(name: &str) -> Option<liuguang_core::config::Easing> {
    serde_json::from_value(serde_json::Value::String(name.trim().to_ascii_lowercase())).ok()
}

/// 转场方向名 → 枚举（同上）。
fn parse_transition_direction(name: &str) -> Option<liuguang_core::config::TransitionDirection> {
    serde_json::from_value(serde_json::Value::String(name.trim().to_ascii_lowercase())).ok()
}

/// 是否需要先把解码帧转成 RGB 再交给合成器。
///
/// 判据来自 [`crate::compositor_profile`]（按合成器名字的已知缺陷名单），
/// 可用 `[media] video_dmabuf_rgb` 或环境变量 `LIUGUANG_DMABUF_RGB` 覆盖：
/// 前者是配置（auto/always/never），后者只给临时排查用。
/// 写成纯函数是为了能在没有图形会话的 CI 里测。
#[cfg(feature = "vaapi")]
fn needs_rgb_conversion(setting: &str) -> bool {
    use crate::compositor_profile::{RgbConversion, detect_names, needs_rgb_conversion as decide};

    let forced = std::env::var("LIUGUANG_DMABUF_RGB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| RgbConversion::parse(&value))
        .or_else(|| RgbConversion::parse(setting))
        .unwrap_or(RgbConversion::Auto);
    decide(&detect_names(), forced)
}

fn media_kind_for_path(path: &Path) -> MediaKind {
    // 1) 已知扩展名（`DEFAULT_VIDEO_EXTENSIONS` 是扫描期的同一份表，`m4v` 是它的
    //    历史别名；`mkv`/`webm` 必须算视频，否则 `set` 一个 `.mkv` 会按图片打开、
    //    报 "Invalid PNG signature"，转码缓存（`.mkv`）重启后也恢复不了 —— t29 F1）。
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if let Some(ext) = ext.as_deref() {
        // 大小写语义与旧实现一致：先 `to_ascii_lowercase()` 再和全小写的表比。
        if liuguang_core::library::DEFAULT_VIDEO_EXTENSIONS.contains(&ext) || ext == "m4v" {
            return MediaKind::Video;
        }
        if liuguang_core::library::DEFAULT_IMAGE_EXTENSIONS.contains(&ext) {
            return MediaKind::Image;
        }
    }
    // 2) 扩展名不认识（`.ts`/`.avi`/无后缀…）：问容器头。`probe::probe_video` 认
    //    ISO-BMFF 与 Matroska，认出来就是视频；连它都读不出才退回图片。
    if crate::probe::probe_video(path).is_some() {
        MediaKind::Video
    } else {
        MediaKind::Image
    }
}

/// 随机播放的确定性种子（同一 `播放列表 id + 输出` 每次启动都一样）。
///
/// 为什么不用 `SystemTime`：真机验收要能**复现**"随机但不相邻重复"的序列，
/// 而可复现的种子让单元测试能断言完整的 30 次切换序列。
fn playlist_seed(output: &str, playlist_id: &str) -> u64 {
    0x7A68_0000
        ^ output
            .bytes()
            .fold(0u64, |a, b| a.rotate_left(8) ^ u64::from(b))
        ^ playlist_id
            .bytes()
            .fold(0u64, |a, b| a.rotate_left(5) ^ u64::from(b))
}

/// 唤醒计划：汇总各通道给出的"下一次必须唤醒的**绝对**时刻"，取最早者。
///
/// 为什么是绝对时刻：主循环用 `recv_timeout(wait)` 休眠，`wait` 若按"相对间隔"
/// 计算，会把本轮 tick 自身的耗时（4K 帧提交可达十几毫秒）累加进下一次唤醒，
/// 每帧都越睡越晚，直到超出迟到容忍开始丢帧。绝对时刻天然吸收 tick 耗时。
#[derive(Debug, Clone, Copy, Default)]
struct WakePlan {
    /// 目前已知的最早唤醒时刻。
    earliest: Option<Instant>,
}

impl WakePlan {
    /// 记录一个期限（`None` 表示该通道无待办）。
    fn note(&mut self, at: Option<Instant>) {
        if let Some(at) = at {
            self.earliest = Some(self.earliest.map_or(at, |e| e.min(at)));
        }
    }

    /// 结合空闲兜底时刻，得到本轮唤醒时刻（不晚于兜底）。
    fn deadline(self, idle_fallback: Instant) -> Instant {
        self.earliest
            .map_or(idle_fallback, |e| e.min(idle_fallback))
    }
}

/// 视频源一次 tick 的推进结果。
#[derive(Debug)]
struct VideoTick {
    /// 本次要提交的帧（每 tick 至多 1 帧，避免重复提交扫描输出）。
    frame: Option<DisplayFrame>,
    /// 下一次需要推进该视频的绝对时刻；`None` = 已结束/已失败/已暂停。
    next_wake: Option<Instant>,
    /// 失败原因（该通道视频已停止，调用方决定回退策略）。
    error: Option<String>,
}

/// 推进视频源：每 tick 至多呈现 1 帧，并把**下一次唤醒的绝对期限**交给调用方。
///
/// 这里的每一行都是"视频被冻住"的根因修复点：
/// - `FramePoll::Pending { next_in }` 是媒体层给出的**精确到期时间**，必须原样
///   向上传播；旧实现把它丢弃，主循环因此退化到 `IDLE_WAIT = 1s` 兜底休眠
///   （真机实测：`Pending next_in_ms=Some(10)` 之后下一次唤醒在 1005ms 后）。
/// - 呈现后立刻用 [`VideoSource::next_deadline`] 拿队首帧的到期**绝对时刻**，
///   而不是固定 2ms 盲探——队列已由 `fill_queue` 补满，期限是已知的。
/// - 周期恰好为媒体 PTS 时间轴，因此视频按**源帧率**推进而不是按解码速度。
fn poll_video(video: &mut (dyn VideoSource + Send), now: Instant) -> VideoTick {
    let poll_at = Instant::now();
    match video.poll_frame(now) {
        Ok(FramePoll::Ready(frame)) => {
            let next_wake = video
                .next_deadline()
                .unwrap_or_else(|| poll_at + VIDEO_PROBE_WAIT);
            tracing::debug!(
                poll_ms = poll_at.elapsed().as_millis() as u64,
                next_in_ms = next_wake.saturating_duration_since(poll_at).as_millis() as u64,
                "poll_frame Ready"
            );
            VideoTick {
                frame: Some(frame),
                next_wake: Some(next_wake),
                error: None,
            }
        }
        Ok(FramePoll::Pending { next_in }) => {
            let next_wake = poll_at + next_in.unwrap_or(VIDEO_POLL_FALLBACK);
            tracing::trace!(
                next_in_ms = next_in.map(|d| d.as_millis() as u64),
                "poll_frame Pending（期限已向上传播）"
            );
            VideoTick {
                frame: None,
                next_wake: Some(next_wake),
                error: None,
            }
        }
        // 循环播放由媒体层负责；到这里的 Ended 只可能是非循环源播放完毕。
        Ok(FramePoll::Ended) => {
            tracing::info!("视频源报告播放结束（Ended）");
            VideoTick {
                frame: None,
                next_wake: None,
                error: None,
            }
        }
        Err(e) => VideoTick {
            frame: None,
            next_wake: None,
            error: Some(e.to_string()),
        },
    }
}

/// 是否该打一条"视频推进"日志（纯函数，便于单测）。
///
/// # 为什么不能对当前值取模（真机 41.5 行/秒的根因）
///
/// 旧实现是 `st.presented % 120 == 1 || st.dropped_late % 60 == 1`。
/// `dropped_late` 是**累计计数**，只要播放期间**掉过一帧**它就恒为 1，
/// 而 `1 % 60 == 1` **永远成立** ⇒ 这条 INFO 变成**每帧一条**。
/// 真机实测（4K60 + DMA-BUF 直通）：`dropped_late` 在启动瞬间就变成 1，
/// 于是 30s 写 1246 行日志（41.5 行/秒）；同一份素材走 CPU/NV12 路径时
/// `dropped_late` 保持 0，只有 38 行/30s。差别不是"哪条路径更健康"，
/// 纯粹是这条判据里藏了一个**恒真分支**（与 S5 的 `contradicts_presented(1)`
/// 同一类 bug：拿常量/累计值当"周期性"判据）。
///
/// 正确语义：
/// - 常规节奏：`presented` 每跨过 120 帧打一条；
/// - 额外打点：`dropped_late` **新跨越** 60 的整数倍时（用前后两次观察值判断
///   跨越，而不是对当前值取模），这样"掉帧开始变多"仍然可见，且与初始值无关。
fn should_log_video_progress(presented: u64, dropped_late: u64, prev_dropped_late: u64) -> bool {
    presented % 120 == 1 || dropped_late / 60 > prev_dropped_late / 60
}

/// 视频推进的可观测日志：按固定节奏打点（真机取时间序列证据的来源）。
/// 日志里的 `presented` 是**媒体侧出队**计数（`VideoStats::presented`）；
/// `presented_ok` 才是**成功提交扫描输出**的计数 —— 两者必须可区分，
/// 否则提交失败会被"presented 一直在涨"掩盖。
fn log_video_progress(key: &str, st: &VideoStats, presented_ok: u64, present_failures: u64) {
    tracing::info!(
        output = %key,
        presented_dequeued = st.presented,
        presented_ok,
        present_failures,
        dropped_late = st.dropped_late,
        dropped_queue_full = st.dropped_queue_full,
        skipped = st.skipped,
        queue_len = st.queue_len,
        decoder = ?st.decoder,
        last_pts_ms = st.last_pts.map(|d| d.as_millis() as u64),
        "视频推进"
    );
}

/// 运行时主循环：IPC 请求 + 信号 + 周期维护。
///
/// # Errors
///
/// socket 绑定失败等初始化错误。
pub fn run(opts: DaemonOptions) -> Result<(), String> {
    // VA 驱动能力探测必须在**任何线程启动之前**完成：若默认驱动不具备 H.264
    // 能力而系统里存在具备该能力的驱动，就在这里固化 LIBVA_DRIVER_NAME
    // （不依赖任何手工 export，也不硬编码某个驱动名）。探测结论进日志；
    // 最终实际生效的解码元素在建立视频通道时上报（`status.decoder`）。
    let va = liuguang_media::va::probe();
    let va_applied = liuguang_media::va::apply_recommended_driver(va);
    if va.h264_hardware_available() {
        tracing::info!(
            summary = %va.summary(),
            env_applied = va_applied,
            recommended = ?va.recommended,
            "VA-API 硬解能力探测完成"
        );
    } else {
        tracing::warn!(
            summary = %va.summary(),
            env_applied = va_applied,
            "VA-API 硬解能力不可用：视频只能软解（CPU 开销大）"
        );
    }

    // **t46**：GStreamer 插件注册表预热（后台线程），放在 LIBVA_DRIVER_NAME 固化之后启动，
    // 既避免与 `set_var` 产生多线程竞态，也让 GStreamer 注册表看到正确的 VA 驱动环境。
    #[cfg(feature = "gstreamer")]
    std::thread::Builder::new()
        .name("liuguang-gst-warmup".to_string())
        .spawn(|| {
            let _ = gstreamer::init();
            let _ = gstreamer::Registry::get();
        })
        .ok();

    let config = if opts.config_path.exists() {
        let mut cfg = Config::load(&opts.config_path)
            .map_err(|e| format!("配置无效（{}）: {e}", opts.config_path.display()))?;
        cfg.apply_persona_defaults(opts.persona);
        cfg
    } else {
        tracing::warn!(
            path = %opts.config_path.display(),
            persona = %opts.persona,
            "配置文件不存在，使用内置默认值"
        );
        let mut cfg = Config::default();
        cfg.apply_persona_defaults(opts.persona);
        cfg
    };

    let device = opts
        .device
        .clone()
        .unwrap_or_else(|| config.display.device.clone());

    // 后端选择：CLI/env 决定的 `want` 覆盖配置文件，`auto` 按环境落地。
    // `wayland` 连接失败**不**回退 DRM（那会抢走运行中合成器的 master），
    // 只进入 HeadlessReady 并告警（守卫见 `Persona` 模块文档）。
    let want = if opts.backend == DisplayBackendKind::Auto {
        config.display.backend
    } else {
        opts.backend
    };
    let resolved = want.resolved();
    let resolved_backend = resolved;
    let (backend, backend_name): (Option<Box<dyn DisplayBackend>>, &str) = match resolved {
        DisplayBackendKind::Wayland => (open_wayland_backend(), "wayland"),
        DisplayBackendKind::Mock => {
            tracing::warn!("使用 mock 显示后端（测试钩子，非生产）");
            (
                Some(Box::new(
                    liuguang_display::mock::MockDisplayBackend::with_outputs(&["HDMI-A-1", "DP-1"]),
                )),
                "mock",
            )
        }
        DisplayBackendKind::Drm | DisplayBackendKind::Auto => (open_drm_backend(&device), "drm"),
    };

    let events = EventBus::default();
    let mut runtime = Runtime::new(
        backend,
        backend_name,
        config,
        opts.config_path.clone(),
        opts.state_file.clone(),
        events.clone(),
    )?;
    runtime.set_persona(opts.persona);
    tracing::info!(persona = %opts.persona, backend = %backend_name, "运行时就绪");
    // 配置面不允许"静默失效"：启动时把**本次配置要求了、但当前不会发生**的字段
    // 大声列出来（缺口清单由 `Config::pending_implementation` 提供，
    // 其与 `config/example.toml` 标记的一致性有测试守着）。
    runtime.log_pending_config("启动");
    // IPC：handler 把请求转给运行时线程（阻塞解码只发生在运行时线程）。
    type Job = (Request, Sender<liuguang_ipc::Dispatch>);
    let socket_path = opts.socket_path.clone();
    let (tx, rx): (Sender<Job>, Receiver<Job>) = std::sync::mpsc::channel();
    let server = liuguang_ipc::IpcServer::bind(liuguang_ipc::ServerConfig {
        socket_path,
        ..liuguang_ipc::ServerConfig::default()
    })
    .map_err(|e| format!("IPC 绑定失败: {e}"))?;
    let handle = server.spawn(move |req: Request| {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if tx.send((req, reply_tx)).is_err() {
            return liuguang_ipc::Dispatch::Reply(Response::err(
                "",
                ErrorBody::new(ErrorKind::Internal, "运行时已关闭"),
            ));
        }
        reply_rx
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| {
                liuguang_ipc::Dispatch::Reply(Response::err(
                    "",
                    ErrorBody::new(ErrorKind::Internal, "运行时响应超时"),
                ))
            })
    });

    // 退出信号。
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, std::sync::Arc::clone(&stop))
            .map_err(|e| format!("注册信号处理器失败: {e}"))?;
    }

    tracing::info!(
        state = runtime.state().name(),
        channels = runtime.channels.len(),
        socket = %opts.socket_path.display(),
        "liuguang 主循环启动（IPC 就绪，等待指令）"
    );
    // 启动即构建媒体索引（**后台**）：TUI 连上时通常已有数据，
    // 且不占用主循环（扫描线程另有 30s 超时 + 条目上限）。
    if runtime.config().library.is_some() {
        match runtime.start_scan() {
            Ok(scan_id) => tracing::info!(scan_id, "启动时已排入媒体库扫描（后台）"),
            Err(body) => tracing::warn!(
                code = body.code.as_str(),
                error = %body.message,
                "启动扫描未能开始（媒体库功能暂不可用，daemon 照常运行）"
            ),
        }
    }
    let mut ticks: u64 = 0;
    let mut last_conn_check = Instant::now();
    // 下一次唤醒的**绝对**时刻：由运行时按帧到期时间精确给出（空闲时 1s 兜底）。
    let mut deadline = Instant::now() + IDLE_WAIT;
    loop {
        // 唤醒 = min(下一个到期时刻, IPC 请求到达)：`recv_timeout` 在有请求时立即
        // 返回，所以 IPC 既不需要另起线程，也不会把渲染节拍拖到 1s 兜底休眠。
        // 休眠时长在**即将休眠时**才按绝对时刻计算（扣掉本 tick 已耗时）：
        // 若按相对间隔休眠，tick 自身耗时（4K 帧提交可达十几毫秒）会累加进
        // 下一次唤醒，每帧越睡越晚。
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .clamp(MIN_TICK_WAIT, IDLE_WAIT);
        match rx.recv_timeout(wait) {
            Ok((req, reply)) => {
                let response = runtime.handle(req);
                let _ = reply.send(response);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::info!("收到退出信号");
            break;
        }
        deadline = runtime.tick(Instant::now());
        ticks += 1;
        // 连接器复查按墙钟节流（5s）：按 tick 计数会在视频播放时
        // 频繁触发 ~1.2s 的连接探测，拖垮 30fps 帧节奏（真机教训）。
        if ticks % 5 == 0 {
            if runtime.needs_backend() {
                // 与启动路径同一套选择：按已解析后端重试，不跨后端回退。
                let recovered = match resolved_backend {
                    DisplayBackendKind::Wayland => open_wayland_backend().map(|b| (b, "wayland")),
                    DisplayBackendKind::Drm | DisplayBackendKind::Auto => {
                        open_drm_backend(&device).map(|b| (b, "drm"))
                    }
                    // mock 后端在启动时必然成功，走到这里说明是测试故障。
                    DisplayBackendKind::Mock => None,
                };
                if let Some((backend, name)) = recovered {
                    runtime.attach_backend(backend, name);
                }
            } else if last_conn_check.elapsed() >= Duration::from_secs(5) {
                last_conn_check = Instant::now();
                runtime.check_connectors();
            }
        }
    }

    handle.shutdown();
    runtime.flush_config_write(Instant::now() + Runtime::CONFIG_WRITE_DEBOUNCE);
    runtime.cancel_scans();
    // 退出前把后台转码的 ffmpeg 进程组杀掉：孤儿会继续写 `.part`（t21 F6）。
    runtime.transcode.shutdown();
    drop(runtime);
    tracing::info!("liuguang 已退出，设备资源已释放");
    Ok(())
}

/// 尝试连接 Wayland 合成器；失败返回 `None`（进入/保持 HeadlessReady）。
///
/// **不回退 DRM**：桌面会话里抢占 DRM master 会把画面撕掉，见
/// `liuguang_core::persona` 模块文档。
#[cfg(target_os = "linux")]
fn open_wayland_backend() -> Option<Box<dyn DisplayBackend>> {
    match liuguang_wayland::WaylandBackend::connect() {
        Ok(b) => {
            tracing::info!("Wayland 壁纸层就绪（wlr-layer-shell Background）");
            Some(Box::new(b))
        }
        Err(e) => {
            tracing::warn!(error = %e, "Wayland 合成器不可用（服务保持在线，稍后重试）");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn open_wayland_backend() -> Option<Box<dyn DisplayBackend>> {
    None
}

/// 尝试打开 DRM 后端；失败返回 `None`（进入/保持 HeadlessReady）。
#[cfg(target_os = "linux")]
fn open_drm_backend(device: &str) -> Option<Box<dyn DisplayBackend>> {
    match liuguang_display::drm_kms::DrmKmsBackend::open(device) {
        Ok(b) => {
            tracing::info!(path = %b.path().display(), driver = b.driver(), "DRM 后端就绪");
            Some(Box::new(b))
        }
        Err(e) => {
            tracing::warn!(device, error = %e, "显示设备暂不可用（服务保持在线，稍后重试）");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn open_drm_backend(_device: &str) -> Option<Box<dyn DisplayBackend>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_media::video::{MockProducer, VideoOptions, VideoPlayer};
    use std::sync::atomic::AtomicUsize;

    /// 合成源帧间隔（≈62.5fps，与 60fps 素材同量级）。
    const FRAME_MS: u64 = 16;

    /// 主循环节拍模拟：完全复刻 `run()` 的唤醒计算。
    ///
    /// `propagate` 为 false 时模拟**旧实现**——`Pending { next_in }` 被丢弃、
    /// 主循环退化到 `IDLE_WAIT`（1s）兜底休眠。
    /// 返回值：(呈现帧数, 最大呈现间隔, 最大单次休眠)。
    fn simulate(
        frames: u64,
        propagate: bool,
        sim: Duration,
        looping: bool,
    ) -> (u64, Duration, Duration) {
        let mut player = VideoPlayer::new(
            MockProducer::new(2, 2, frames, Duration::from_millis(FRAME_MS)),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::from_millis(100),
                looping,
            },
        )
        .expect("构造播放器");
        let start = Instant::now();
        let end = start + sim;
        let mut now = start;
        let mut presented = 0u64;
        let mut last_present = start;
        let mut max_gap = Duration::ZERO;
        let mut max_wait = Duration::ZERO;
        while now < end {
            let tick = poll_video(&mut player, now);
            // 视频结束（Ended）后不再统计休眠：空闲 1s 兜底是正常行为。
            if tick.frame.is_none() && tick.next_wake.is_none() {
                break;
            }
            if let Some(frame) = tick.frame {
                assert!(frame.width() > 0);
                presented += 1;
                max_gap = max_gap.max(now.saturating_duration_since(last_present));
                last_present = now;
            }
            let mut plan = WakePlan::default();
            if propagate {
                plan.note(tick.next_wake);
            }
            let deadline = plan.deadline(now + IDLE_WAIT);
            let wait = deadline
                .saturating_duration_since(now)
                .clamp(MIN_TICK_WAIT, IDLE_WAIT);
            max_wait = max_wait.max(wait);
            now += wait;
        }
        (presented, max_gap, max_wait)
    }

    #[test]
    fn pause_resume_event_sequences_land_in_stable_states() {
        // 视频恢复：Paused + VideoStarted → VideoPlaying。
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::VideoStarted).unwrap();
        sm.handle(Event::Pause).unwrap();
        assert_eq!(
            sm.handle(Event::VideoStarted).unwrap().to,
            State::VideoPlaying
        );
        assert!(sm.current().is_stable_display());

        // 静态恢复：Paused + StaticShown → StaticIdle（daemon 采用的路径）。
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        sm.handle(Event::Pause).unwrap();
        assert_eq!(sm.handle(Event::StaticShown).unwrap().to, State::StaticIdle);
        assert!(sm.current().is_stable_display());

        // 另一条同样合法、也必须落到稳定态的路径：Resume →（PreparingNext）
        // → StaticShown → StaticIdle。daemon 选了更短的那条，这里钉住两条都通。
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::StaticShown).unwrap();
        sm.handle(Event::Pause).unwrap();
        assert_eq!(sm.handle(Event::Resume).unwrap().to, State::PreparingNext);
        assert_eq!(sm.handle(Event::StaticShown).unwrap().to, State::StaticIdle);
        assert!(sm.current().is_stable_display());

        // 视频恢复时不得落到 PreparingNext（否则画面停摆但状态显示"在准备"）。
        let mut sm = StateMachine::new();
        sm.handle(Event::DisplayInitialized).unwrap();
        sm.handle(Event::VideoStarted).unwrap();
        sm.handle(Event::Pause).unwrap();
        assert!(matches!(
            sm.handle(Event::VideoStarted).unwrap().to,
            State::VideoPlaying
        ));
    }

    #[test]
    fn wake_plan_takes_earliest_and_caps_idle() {
        let t0 = Instant::now();
        let mut plan = WakePlan::default();
        assert_eq!(
            plan.deadline(t0 + IDLE_WAIT),
            t0 + IDLE_WAIT,
            "无待办时用空闲兜底"
        );
        plan.note(Some(t0 + Duration::from_millis(30)));
        plan.note(Some(t0 + Duration::from_millis(10)));
        plan.note(None);
        assert_eq!(
            plan.deadline(t0 + IDLE_WAIT),
            t0 + Duration::from_millis(10),
            "取最早的期限"
        );
        // 兜底是上限：期限比它更晚时仍按兜底唤醒（轮换调度需要 1s 粒度）。
        let mut plan = WakePlan::default();
        plan.note(Some(t0 + Duration::from_secs(5)));
        assert_eq!(plan.deadline(t0 + IDLE_WAIT), t0 + IDLE_WAIT);
    }

    #[test]
    fn poll_video_propagates_pending_deadline() {
        let mut player = VideoPlayer::new(
            MockProducer::new(2, 2, 3, Duration::from_millis(40)),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::ZERO,
                looping: false,
            },
        )
        .expect("构造播放器");
        let t0 = Instant::now();
        let first = poll_video(&mut player, t0);
        assert!(first.frame.is_some(), "首帧应立即到期");
        assert_eq!(
            first.next_wake,
            Some(t0 + Duration::from_millis(40)),
            "呈现后必须给出下一帧的精确到期时刻（而不是丢弃期限）"
        );
        // 未到期的帧：`Pending { next_in }` 必须向上传播为唤醒期限。
        let before = Instant::now();
        let second = poll_video(&mut player, t0);
        let after = Instant::now();
        assert!(second.frame.is_none(), "未到期不得呈现");
        let next = second.next_wake.expect("Pending 必须给出唤醒期限");
        assert!(
            next >= before + Duration::from_millis(40),
            "期限过短：{next:?}"
        );
        assert!(
            next <= after + Duration::from_millis(41),
            "期限过长（期限被丢弃会退化成 1s 兜底）：{next:?}"
        );
    }

    #[test]
    fn wild_frame_gap_does_not_drop_everything_as_late() {
        // 唤醒间隔远大于帧间隔（旧实现的 1s 兜底）时，严格判据会连续丢帧。
        // 这里断言"正确传播期限"时不会出现这种情况，且 promise 的期限 ≤ 帧间隔。
        let mut player = VideoPlayer::new(
            MockProducer::new(2, 2, 600, Duration::from_millis(FRAME_MS)),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::from_millis(100),
                looping: false,
            },
        )
        .expect("构造播放器");
        let t0 = Instant::now();
        let tick = poll_video(&mut player, t0);
        assert!(tick.frame.is_some());
        let deadline = tick.next_wake.expect("有下一帧时期限必须已知");
        let gap = deadline.saturating_duration_since(t0);
        assert!(
            gap <= Duration::from_millis(FRAME_MS * 2),
            "唤醒间隔（{gap:?}）不得远大于帧间隔，否则帧会被判迟到丢弃"
        );
        let stats = player.video_stats();
        assert_eq!(stats.dropped_late, 0, "正常节拍下不得丢帧");
    }

    #[test]
    fn playback_advances_at_source_rate_with_deadlines() {
        let frames = 180; // 3 秒 @60fps
        let (presented, max_gap, max_wait) = simulate(frames, true, Duration::from_secs(3), false);
        assert!(
            presented >= frames - 5,
            "按源时序推进应几乎呈完所有帧，实际 {presented}/{frames}"
        );
        assert!(
            max_gap <= Duration::from_millis(60),
            "呈现间隔不得出现 ~1s 的空档：{max_gap:?}"
        );
        assert!(
            max_wait <= Duration::from_millis(60),
            "单次休眠不得接近 1s 兜底（否则 poll 频率不足以按帧推进）：{max_wait:?}"
        );
    }

    #[test]
    fn discarding_video_deadline_freezes_playback() {
        // 回归守卫：若把 `Pending { next_in }` 丢弃（旧实现），主循环会以
        // IDLE_WAIT=1s 兜底休眠，严格到期判据下几乎所有帧都被判"迟到"丢弃
        // —— 这正是"画面冻结在 2 帧"的机理，也说明视频侧判据与节拍修复
        // 必须同时落地。
        let (frozen, _, frozen_wait) = simulate(180, false, Duration::from_secs(3), false);
        let (healthy, _, healthy_wait) = simulate(180, true, Duration::from_secs(3), false);
        assert!(frozen <= 5, "丢弃期限时不应还能流畅播放（实际 {frozen}）");
        assert!(
            healthy > frozen * 10,
            "正确传播期限必须显著优于丢弃期限：{healthy} vs {frozen}"
        );
        assert!(
            frozen_wait >= IDLE_WAIT,
            "丢弃期限后单次休眠就是 1s 兜底：{frozen_wait:?}"
        );
        assert!(
            healthy_wait <= Duration::from_millis(60),
            "修复后单次休眠应为帧间隔量级：{healthy_wait:?}"
        );
    }

    #[test]
    fn playback_loops_across_eos_and_keeps_presenting() {
        // 真机故障的回归守卫：素材很短（60 帧 / 0.96 秒），循环播放 4 秒。
        // 只要循环路径断掉（例如流末观测不到、或 seek 后时间轴错位），
        // presented 就会停在 60 附近不再增长 —— 本测试会失败。
        // 60 帧 × 16ms = 0.96s/轮 → 4 秒内至少 3 轮（≥180 帧）。
        let frames = 60;
        let (presented, max_gap, max_wait) = simulate(frames, true, Duration::from_secs(4), true);
        assert!(
            presented > frames * 2,
            "循环播放必须跨过流末继续推进：presented={presented}（单轮 {frames} 帧）"
        );
        assert!(
            max_gap <= Duration::from_millis(80),
            "循环接缝处不得出现长空档：{max_gap:?}"
        );
        assert!(
            max_wait <= Duration::from_millis(60),
            "循环播放期间单次休眠应为帧间隔量级：{max_wait:?}"
        );
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn video_policy_maps_config_to_guard() {
        use liuguang_core::config::HardwareDecode;
        let mut config = Config::default();
        config.media.hardware_decode = HardwareDecode::Required;
        let p = video_policy(&config);
        assert!(
            p.require_hardware && !p.allow_software_fallback,
            "required 必须拒绝软解"
        );

        config.media.hardware_decode = HardwareDecode::Disabled;
        let p = video_policy(&config);
        assert!(p.prefer_software, "disabled 应按配置走软解");

        config.media.hardware_decode = HardwareDecode::Auto;
        config.media.software_decode_fallback = false;
        let p = video_policy(&config);
        assert!(
            !p.require_hardware && !p.allow_software_fallback,
            "auto + 禁止回退：硬解优先但拿不到就报错，绝不静默软解"
        );

        config.media.software_decode_fallback = true;
        let p = video_policy(&config);
        assert!(p.allow_software_fallback, "auto + 允许回退：允许但需告警");
    }
    // ================= t7：AUDIT §2.1/§2.2/§2.3 的 P0 回归测试 =================
    //
    // 这些测试都走**真实路径**：真 Runtime + mock 显示后端 + 真实 PNG 文件 +
    // 真实的 `handle()` 请求路由，断言的是 `status` 的 JSON 负载与**渲染出的扫描
    // 帧像素**，不是内部账本 —— 否则就是 AUDIT §5 批评的"同进程自证"。

    /// 测试环境：临时目录 + 可写的媒体目录 + 真实 PNG 素材。
    struct TestEnv {
        dir: PathBuf,
    }

    impl TestEnv {
        /// 建一个**独占**的临时目录（T63）。
        ///
        /// # 为什么目录名里要有一个进程内序号
        ///
        /// 原先的键是 `liuguang-t7-{name}-{pid}`：同一测试二进制里 `pid` 恒定，
        /// 因此**两个用了同名 `name` 的用例会静默共享同一个目录**，而
        /// [`Self::new`] 开头的 `remove_dir_all` 会把对方正在用的素材删掉 ——
        /// 表现就是"单跑绿、全量按某种顺序跑红"（顺序相关 flake 的经典结构）。
        /// 加一个进程内单调序号后，**同名也各自独立**；
        /// `test_env_dirs_are_unique_even_with_the_same_name` 守这条不变量。
        fn new(name: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("liuguang-t7-{name}-{}-{seq}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("media")).expect("建临时目录");
            Self { dir }
        }

        fn media_dir(&self) -> PathBuf {
            self.dir.join("media")
        }

        /// 写一张纯色 PNG（rgba）并返回路径。
        fn png(&self, name: &str, w: u32, h: u32, rgba: [u8; 4]) -> PathBuf {
            let path = self.media_dir().join(name);
            let file = std::fs::File::create(&path).expect("建 PNG");
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().expect("写 PNG 头");
            let mut data = Vec::with_capacity((w * h * 4) as usize);
            for _ in 0..(w * h) {
                data.extend_from_slice(&rgba);
            }
            writer.write_image_data(&data).expect("写 PNG 数据");
            drop(writer);
            path
        }

        /// 生成配置（转场关闭）。
        fn toml(&self, outputs_section: &str, interval_secs: u64) -> String {
            self.toml_with(outputs_section, interval_secs, false)
        }

        /// 生成配置：`outputs_section` 为空串 = **不声明** `[outputs.<name>]`；
        /// `transition` = 是否启用 300ms 淡入转场。
        ///
        /// 注意：不在这里注入 `[performance]` —— 有些调用方自己就在
        /// `outputs_section` 里写 `[performance]`（如 `pause_on_fullscreen`），
        /// 再加一个同名表会让 TOML 直接报 duplicate key。测试的确定性由
        /// `DisplayPower::new` 在 `cfg!(test)` 下不读真实 sysfs 保证。
        fn toml_with(&self, outputs_section: &str, interval_secs: u64, transition: bool) -> String {
            let dir = self.media_dir();
            format!(
                "[display]\nbackend = \"drm\"\ndevice = \"auto\"\nmode = \"preferred\"\n\n\
                 [media]\nallowed_roots = [\"{dir}\"]\nvideo_fps_limit = 30\n\n\
                 [slideshow]\nenabled = true\ndirectory = \"{dir}\"\ninterval_seconds = {interval_secs}\norder = \"sequential\"\n\n\
                 [transition]\nenabled = {transition}\neffect = \"fade\"\nduration_ms = 300\n\n{outputs_section}",
                dir = dir.display()
            )
        }

        /// 用给定 TOML 建一个真 Runtime（mock 后端，输出名由 `outputs` 指定）。
        fn runtime_with(&self, toml: &str, outputs: &[&str]) -> Runtime {
            self.runtime_with_backend(
                toml,
                Box::new(liuguang_display::mock::MockDisplayBackend::with_outputs(
                    outputs,
                )),
            )
        }

        /// 用**调用方给定的后端**建 Runtime。
        ///
        /// 需要它的场景：模拟"启动时没有任何已连接输出"（`MockDisplayBackend::empty()`
        /// 或先把输出 `unplug`）——`runtime_with` 只能造已连接输出。
        fn runtime_with_backend(
            &self,
            toml: &str,
            backend: Box<dyn liuguang_display::DisplayBackend>,
        ) -> Runtime {
            let cfg_path = self.dir.join("config.toml");
            std::fs::write(&cfg_path, toml).expect("写配置");
            let config = Config::load(&cfg_path).expect("配置应合法");
            Runtime::new(
                Some(backend),
                "mock",
                config,
                cfg_path,
                self.dir.join("state.json"),
                EventBus::default(),
            )
            .expect("建 Runtime")
        }

        fn runtime(&self, toml: &str) -> Runtime {
            self.runtime_with(toml, &["DP-1"])
        }

        /// **重启**：不重写配置文件，直接按磁盘上现有的 `config.toml` 建 Runtime。
        ///
        /// T62 的"重启后播放列表仍在"必须走这条路径 —— 用 `runtime()` 会先把
        /// 测试用的 TOML 写回去，把刚保存的播放列表覆盖掉（那就测不到装载了）。
        fn runtime_reload(&self, outputs: &[&str]) -> Runtime {
            let cfg_path = self.dir.join("config.toml");
            let config = Config::load(&cfg_path).expect("磁盘上的配置应合法");
            Runtime::new(
                Some(Box::new(
                    liuguang_display::mock::MockDisplayBackend::with_outputs(outputs),
                )),
                "mock",
                config,
                cfg_path,
                self.dir.join("state.json"),
                EventBus::default(),
            )
            .expect("建 Runtime（重启）")
        }
    }

    // ===== 无显示器（HeadlessReady）回归：规范 §4.3 / §8.3 =====
    //
    // 2026-10-28 真机事实（Fedora 44 测试机，无显示器）：
    //   daemon 启动后 `status.state` 永久停在 `InitializingDisplay`，
    //   journal 里有 `顶层状态机拒绝非法转移 from=InitializingDisplay event=OutputLost`。
    // 根因有两层，本组测试各钉住一条：
    //   ① 状态表缺 `InitializingDisplay + OutputLost`（见 liuguang-core 的同名用例）；
    //   ② daemon 为**未连接**的 connector 也建通道，启动即逐路解码/提交。

    /// 启动时没有任何输出 ⇒ 明确进入 HeadlessReady，且不产生非法转移。
    #[test]
    fn headless_startup_reaches_headless_ready() {
        let env = TestEnv::new("headless-startup");
        let mut rt = env.runtime_with_backend(
            &env.toml("", 3600),
            Box::new(liuguang_display::mock::MockDisplayBackend::empty()),
        );
        assert_eq!(
            rt.state(),
            State::HeadlessReady,
            "无已连接输出时状态必须是 HeadlessReady（规范 §4.3），实际 {}",
            rt.state().name()
        );
        assert!(rt.channels.is_empty(), "无输出时不得建立通道");
        assert_eq!(rt.state_machine_errors(), 0, "启动路径不得有非法转移");

        // 周期复查也不得把状态从 HeadlessReady 踢走或制造非法转移。
        rt.check_connectors();
        assert_eq!(rt.state(), State::HeadlessReady);
        assert_eq!(rt.state_machine_errors(), 0);
    }

    /// 枚举到 connector 但全部 `disconnected` ⇒ 不建通道、不进入 InitializingDisplay。
    #[test]
    fn disconnected_connectors_do_not_create_channels() {
        let env = TestEnv::new("headless-disconnected");
        env.png("a.png", 16, 16, [1, 2, 3, 255]);
        let mut backend = liuguang_display::mock::MockDisplayBackend::with_outputs(&["DP-1"]);
        backend.unplug("DP-1");
        let mut rt = env.runtime_with_backend(&env.toml("", 3600), Box::new(backend));
        assert_eq!(rt.state(), State::HeadlessReady);
        assert!(
            rt.channels.is_empty(),
            "未连接输出不得建通道（否则启动即对其解码 + 提交，真机日志里 6 条管线全部失败）"
        );
        assert_eq!(rt.state_machine_errors(), 0);
        rt.check_connectors();
        assert!(rt.channels.is_empty(), "复查后仍不得为未连接输出建通道");
    }

    /// 热插拔：接上显示器后必须**真的**点亮首个媒体项（状态 StaticIdle），
    /// 而不是旧实现那样无条件 `StaticShown`（屏幕全黑却报 StaticIdle）。
    #[test]
    fn hotplug_after_headless_shows_real_content() {
        let env = TestEnv::new("headless-hotplug");
        env.png("a.png", 16, 16, [9, 9, 9, 255]);
        let mut rt = env.runtime_with_backend(
            &env.toml("", 3600),
            Box::new(liuguang_display::mock::MockDisplayBackend::empty()),
        );
        assert_eq!(rt.state(), State::HeadlessReady);

        rt.attach_backend(
            Box::new(liuguang_display::mock::MockDisplayBackend::with_outputs(&[
                "DP-1",
            ])),
            "mock",
        );
        rt.check_connectors();
        assert_eq!(rt.channels.len(), 1, "接上输出后必须建立通道");
        assert_eq!(
            rt.state(),
            State::StaticIdle,
            "热插拔后应真正点亮图片并进入 StaticIdle，实际 {}",
            rt.state().name()
        );
        assert!(
            rt.channels["DP-1"].current.is_some(),
            "StaticIdle 必须有真实上屏内容（不得只改状态）"
        );
        assert_eq!(rt.state_machine_errors(), 0);
    }

    /// 热插拔时若没有可显示内容（空媒体目录），**不得**谎称 StaticIdle。
    #[test]
    fn hotplug_without_media_does_not_claim_static_idle() {
        let env = TestEnv::new("headless-hotplug-empty");
        let mut rt = env.runtime_with_backend(
            &env.toml("", 3600),
            Box::new(liuguang_display::mock::MockDisplayBackend::empty()),
        );
        rt.attach_backend(
            Box::new(liuguang_display::mock::MockDisplayBackend::with_outputs(&[
                "DP-1",
            ])),
            "mock",
        );
        rt.check_connectors();
        assert_eq!(rt.channels.len(), 1);
        assert_ne!(
            rt.state(),
            State::StaticIdle,
            "没有可显示内容时不得进入 StaticIdle（那是在撒谎）"
        );
        assert!(rt.channels["DP-1"].current.is_none());
        assert_eq!(rt.state_machine_errors(), 0);
    }

    /// t61：缺 `[library]` 时媒体库相关报错必须**可操作**（含最小示例 + 指向示例配置 + 重启提示）。
    ///
    /// 为什么需要它：`[library]` 是可选段 ⇒ 缺段时 `check-config` 仍报"配置有效"、
    /// 服务照常启动，用户唯一能看到的线索就是这条报错（TUI 直接显示 message）。
    /// 覆盖两种调用形态：`handle_list_media`（走 `err_response_code`）与
    /// `handle_list_sources`（同）；两者共用 `LIBRARY_NOT_CONFIGURED_HINT`。
    /// **变异**：把 `LIBRARY_NOT_CONFIGURED_HINT` 换回旧文案（只有"未配置 [library]（媒体库功能未启用）"）
    /// ⇒ 本测试变红（缺 `config/example.toml` / "重启" 子串）。
    #[test]
    fn library_not_configured_error_is_actionable() {
        let env = TestEnv::new("library-hint");
        env.png("a.png", 8, 8, [1, 2, 3, 255]);
        // `TestEnv::toml` 生成的配置**不含** `[library]` ⇒ 正好是"旧配置用户"的情形。
        let mut rt = env.runtime(&env.toml("", 3600));
        assert!(
            rt.library().is_none(),
            "本测试前提：测试配置里没有 [library]"
        );
        let cases = [
            (
                Method::ListMedia,
                serde_json::json!({ "page": 0, "page_size": 10 }),
            ),
            (Method::ListSources, serde_json::json!({})),
        ];
        for (method, params) in cases {
            let resp = call(&mut rt, method, params);
            assert!(!resp.ok, "缺 [library] 时该请求应失败");
            let err = resp.error.expect("应带 ErrorBody");
            assert_eq!(err.detail_code.as_deref(), Some("NOT_CONFIGURED"));
            for key in ["[library]", "config/example.toml", "重启"] {
                assert!(
                    err.message.contains(key),
                    "报错应含可操作指引 {key:?}（否则用户不知道该怎么配），实际: {}",
                    err.message
                );
            }
        }
    }

    /// 处理一条请求，**允许** `Deferred`（t39：缩略图排队解码）。
    ///
    /// 与 [`call`] 的区别只是多等一个后台应答；协议语义完全一样。
    /// 让他一次 `set_wallpaper` 跑到"上屏完成"（t45：受理与上屏解耦）。
    ///
    /// 生产环境由主循环每 tick 消费一个 `pending_sets`；测试里用两次 tick 复现：
    /// 第一次建管线，第二次把 `WALLPAPER_APPLIED` 事件发出去。
    fn settle_set(rt: &mut Runtime) {
        for _ in 0..4 {
            if rt.pending_sets.is_empty() {
                break;
            }
            rt.run_pending_sets();
        }
        rt.flush_pending_applied();
    }

    fn call_async(rt: &mut Runtime, method: Method, params: serde_json::Value) -> Response {
        match rt.handle(Request::new("t7", method, params)) {
            liuguang_ipc::Dispatch::Reply(resp) => resp,
            liuguang_ipc::Dispatch::Deferred { rx } => rx
                .recv_timeout(Duration::from_secs(60))
                .expect("后台任务必须在期限内给出应答"),
            liuguang_ipc::Dispatch::Subscribe { .. } => panic!("subscribe 不该出现在单元测试里"),
        }
    }

    /// t46 之后 `prime_first_frame` 会在"启动安静期"（[`PRIME_QUIET`]）内主动让路给
    /// 刚连上来的客户端；老测试仍按"首个 tick 立即点亮"断言，这里把启动时间往前拨。
    fn age_startup(rt: &mut Runtime) {
        rt.started = std::time::Instant::now() - std::time::Duration::from_secs(5);
        // 这些测试在 tick 之前会先发 status/等请求（`last_request_at` 因此被置位），
        // 而它们模拟的是"启动后没人指挥"的场景 ⇒ 一并清掉。
        rt.last_request_at = None;
    }

    fn call(rt: &mut Runtime, method: Method, params: serde_json::Value) -> Response {
        let is_set = method == Method::SetWallpaper;
        match rt.handle(Request::new("t7", method, params)) {
            liuguang_ipc::Dispatch::Reply(r) => {
                // t45：`set_wallpaper` 只回"已受理"，上屏由 tick 完成；测试沿用
                // "call 之后画面就在"的旧约定，因此在这里把 pending 跑完。
                if is_set {
                    settle_set(rt);
                }
                r
            }
            other => panic!("期望 Reply，实际 {other:?}"),
        }
    }

    fn ok_json(rt: &mut Runtime, method: Method, params: serde_json::Value) -> serde_json::Value {
        let r = call(rt, method, params);
        assert!(
            r.ok,
            "请求应成功，实际错误: {:?}",
            r.error.map(|e| e.message)
        );
        r.result.unwrap_or(serde_json::Value::Null)
    }

    fn status_json(rt: &mut Runtime) -> serde_json::Value {
        ok_json(rt, Method::Status, serde_json::json!({}))
    }

    /// 渲染帧里的黑色像素数（背景/黑边）。用于**行为断言**：fit 是否真的生效。
    fn black_pixels(frame: &CpuFrame) -> usize {
        frame
            .data
            .chunks_exact(4)
            .filter(|px| px[0] == 0 && px[1] == 0 && px[2] == 0)
            .count()
    }

    fn current_frame(rt: &Runtime, key: &str) -> CpuFrame {
        rt.channels
            .get(key)
            .and_then(|c| c.current.as_ref())
            .map(|d| d.frame.clone())
            .expect("通道应有当前画面")
    }

    fn current_media(rt: &Runtime, key: &str) -> String {
        rt.channels
            .get(key)
            .and_then(|c| c.current.as_ref())
            .map(|d| d.media.display().to_string())
            .unwrap_or_else(|| "<none>".to_string())
    }

    /// P0-1：配置**没有** `[outputs."DP-1"]` 段时，`set --fit contain` 必须真正生效。
    ///
    /// 断言渲染出的扫描帧真的出现黑边（cover 不会），而不只是"配置里存了值"。
    /// 旧实现 `override_fit` 直接 return（参数被丢弃）却回 `accepted:true`。
    #[test]
    fn set_fit_override_applies_even_without_outputs_section() {
        let env = TestEnv::new("fit-no-outputs");
        // 竖图（32x128）在 1920x1080 上：contain 必有左右黑边，cover 没有。
        let png = env.png("portrait.png", 32, 128, [255, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 600));
        let r = call(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({
                "path": png.display().to_string(),
                "output": "DP-1",
                "fit": "contain",
            }),
        );
        assert!(r.ok, "set 应被受理");
        assert_eq!(r.result.unwrap()["accepted"], true);
        // 行为断言：真的出现了黑边（= contain 生效）。
        let black = black_pixels(&current_frame(&rt, "DP-1"));
        assert!(
            black > 1000,
            "fit=contain 必须产生黑边；黑像素只有 {black}，说明 --fit 被丢弃了（P0-1 复发）"
        );
        // 参数确实落到了该输出的显示参数上（IPC 覆盖 = 配置语义）。
        assert_eq!(
            rt.config().outputs.get("DP-1").map(|o| o.fit),
            Some(liuguang_core::config::FitMode::Contain),
            "未声明 [outputs] 段时应现场建立条目并写入覆盖值"
        );
    }

    /// P0-1 的另一半：非法参数必须**明确报错**，不得静默忽略、也不得报 INTERNAL。
    #[test]
    fn set_rejects_invalid_overrides_with_invalid_params() {
        let env = TestEnv::new("bad-overrides");
        let png = env.png("a.png", 8, 8, [0, 255, 0, 255]);
        // 无 [outputs] 段的老路径（审计里这条曾被静默忽略）。
        // 注意：JSON 无法表达 inf/NaN（会被序列化成 null），因此 offset 的
        // 有限性校验只能由配置文件路径触发；这里覆盖 fit/zoom 的网络入参校验。
        let mut rt = env.runtime(&env.toml("", 600));
        for (params, needle) in [
            (serde_json::json!({"zoom": 99.0}), "zoom"),
            (serde_json::json!({"zoom": -1.0}), "zoom"),
            (serde_json::json!({"fit": "diagonal"}), "fit"),
        ] {
            let mut p = params.clone();
            p["path"] = serde_json::json!(png.display().to_string());
            p["output"] = serde_json::json!("DP-1");
            let r = call(&mut rt, Method::SetWallpaper, p);
            assert!(!r.ok, "非法覆盖参数必须失败: {params}");
            let err = r.error.expect("应有错误体");
            assert_eq!(
                format!("{:?}", err.code),
                "InvalidParams",
                "用户参数问题必须是 InvalidParams，实际 {:?}",
                err.code
            );
            assert!(
                err.message.contains(needle),
                "错误信息应指出字段 {needle}，实际: {}",
                err.message
            );
        }
    }

    /// P0-2：`[slideshow] interval_seconds` 必须真正生效（清除硬编码 600s）。
    #[test]
    fn interval_seconds_actually_rotates_slideshow() {
        let env = TestEnv::new("interval");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [0, 0, 255, 255]);
        assert!(a.exists() && b.exists());
        let mut rt = env.runtime(&env.toml("", 1));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        let first = current_media(&rt, "DP-1");
        assert!(first.ends_with("a.png"), "首帧应是 a.png");
        // 模拟时钟前进：2 秒（配置间隔 1 秒）必须发生轮换。
        let start = Instant::now();
        let mut rotated = false;
        for step in 1..=4 {
            rt.tick(start + Duration::from_millis(step * 1_000));
            if current_media(&rt, "DP-1") != first {
                rotated = true;
                break;
            }
        }
        assert!(
            rotated,
            "interval_seconds=1 时 4 秒内必须轮换（旧实现被硬编码 600s 覆盖，P0-2）"
        );
        // 而且**不能**只看一次：下一次仍按配置间隔继续轮换（间隔来自配置而非一次性）。
        let second = current_media(&rt, "DP-1");
        assert!(
            second.ends_with("b.png"),
            "轮换目标应是 b.png，实际 {second}"
        );
        rt.tick(start + Duration::from_millis(9_000));
        assert_ne!(
            current_media(&rt, "DP-1"),
            second,
            "轮换必须持续按配置间隔发生，而不是只发生一次"
        );
    }

    /// P0-3 + t39：暂停是用户意图 —— `next` 不得解除；而**显式** `set_wallpaper`
    /// 是"我就是要看这个"的信号，必须自动恢复（否则用户点了壁纸却永远停在
    /// `Paused presented=0`，真机 08:25 连点 8 次全落空）。
    ///
    /// 注意：只有**用户暂停**会被显式应用解除；熄屏/全屏让路的暂停由
    /// `apply_screen_state`/全屏恢复逻辑负责（见 `screen_off_pause_*` 测试）。
    #[test]
    fn next_keeps_pause_but_explicit_set_resumes() {
        let env = TestEnv::new("pause");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [0, 255, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        ok_json(
            &mut rt,
            Method::Pause,
            serde_json::json!({"output": "DP-1"}),
        );
        assert_eq!(status_json(&mut rt)["state"], "Paused");

        // 暂停中显式切换：画面可以换，但暂停**必须保持**（next 不是"我要看"）。
        ok_json(&mut rt, Method::Next, serde_json::json!({"output": "DP-1"}));
        let st = status_json(&mut rt);
        assert_eq!(st["channels"][0]["paused"], true, "next 不得解除暂停");
        assert_eq!(st["state"], "Paused", "顶层状态必须仍是 Paused");
        assert_eq!(st["channels"][0]["state"], "Paused");

        // 显式应用：暂停必须被解除（t39 的核心修复）。
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": b.display().to_string(), "output": "DP-1"}),
        );
        let st = status_json(&mut rt);
        assert_eq!(
            st["channels"][0]["paused"], false,
            "显式 set 必须恢复用户暂停（否则点了没反应）"
        );
        assert_ne!(st["state"], "Paused", "顶层状态必须离开 Paused");
        assert_eq!(rt.state_machine_errors(), 0, "恢复路径不得产生非法转移");

        // resume 幂等。
        ok_json(
            &mut rt,
            Method::Resume,
            serde_json::json!({"output": "DP-1"}),
        );
        let st = status_json(&mut rt);
        assert_eq!(st["channels"][0]["paused"], false);
        let top = st["state"].as_str().unwrap_or_default();
        assert!(
            matches!(top, "StaticIdle" | "VideoPlaying"),
            "resume 后应落到稳定态，实际 {top}（卡在 PreparingNext 即 P0-3 复发）"
        );
        assert_eq!(rt.state_machine_errors(), 0, "resume 路径不得产生非法转移");
    }

    /// P0-3：`drive()` 对非法转移必须 warn + 计数 + 发事件，绝不静默吞。
    #[test]
    fn illegal_transition_is_counted_and_visible() {
        let env = TestEnv::new("illegal");
        let png = env.png("a.png", 8, 8, [255, 255, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 600));
        let events = rt.events.subscribe();
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": png.display().to_string(), "output": "DP-1"}),
        );
        let mut seen = Vec::new();
        while let Ok(ev) = events.try_recv() {
            seen.push(format!(
                "{:?}:{}",
                ev.kind,
                ev.message.clone().unwrap_or_default()
            ));
        }
        assert_eq!(
            rt.state_machine_errors(),
            0,
            "正常 set 路径不应产生非法转移；实际事件={seen:?} 顶层={}",
            rt.state.current().name()
        );
        assert_eq!(rt.state.current(), State::StaticIdle);
        // StaticIdle + NextPrepared 是非法转移（只有 PreparingNext 能接受它）。
        rt.drive(Event::NextPrepared);
        assert_eq!(
            rt.state_machine_errors(),
            1,
            "非法转移必须被计数，而不是静默吞掉"
        );
        assert_eq!(
            rt.state.current(),
            State::StaticIdle,
            "非法转移不得改变状态"
        );
        let mut saw_error_event = false;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == liuguang_ipc::protocol::EventKind::Error {
                saw_error_event = true;
            }
        }
        assert!(saw_error_event, "非法转移必须发 Error 事件给订阅者");
    }

    /// **t94 / 审核 P2-6**：`status` 必须让用户分辨 `DegradedStatic` 的原因，
    /// 而且原因只能来自**收敛时记录的事实**。
    ///
    /// 变异：把判据换回"有没有 `video_resume_pending`"⇒ 本用例变红
    /// （没有记录时必须说"未记录"，不得臆断成帧错误）。
    #[test]
    fn status_hint_distinguishes_degraded_reasons() {
        let recorded = |msg: &str, master: bool| DegradedReason {
            message: msg.to_string(),
            master_confirmed: master,
        };

        // 环境原因：收敛时未确认 master ⇒ 说明会自动重建，并带上原始记录。
        let env = degraded_reason_hint(
            "DegradedStatic",
            Some(&recorded("drmSetMaster: EACCES", false)),
        );
        assert!(env.contains("环境暂时不可用"), "应报环境原因：{env}");
        assert!(env.contains("自动"), "应说明会自愈恢复：{env}");
        assert!(
            !env.contains("帧/平面一侧"),
            "环境原因不得说成帧错误：{env}"
        );
        assert!(
            env.contains("drmSetMaster: EACCES"),
            "必须带上当时记录的原因文本，否则用户无从排查：{env}"
        );

        // 帧原因：已持有 master ⇒ 不会自动重开，同样带原始记录。
        let frame = degraded_reason_hint("DegradedStatic", Some(&recorded("AddFB2: EINVAL", true)));
        assert!(frame.contains("帧/平面一侧"), "应报帧错误：{frame}");
        assert!(!frame.contains("自动重建"), "帧错误不得声称会自愈：{frame}");
        assert!(
            frame.contains("AddFB2: EINVAL"),
            "应可追溯到原始记录：{frame}"
        );

        // 无记录 ⇒ 如实说"未记录"，不得二选一臆断（这条正是旧实现的错误）。
        let unknown = degraded_reason_hint("DegradedStatic", None);
        assert!(unknown.contains("未记录"), "无记录必须明说：{unknown}");
        assert!(
            !unknown.contains("帧/平面一侧") && !unknown.contains("环境暂时不可用"),
            "无记录时不得臆断类别：{unknown}"
        );

        // 非降级态不输出原因（避免噪音）。
        assert!(degraded_reason_hint("VideoPlaying", Some(&recorded("x", false))).is_empty());
        assert!(degraded_reason_hint("StaticIdle", None).is_empty());
    }

    /// P0-3：status 的状态源必须一致（不变式集合为空），且顶层字段描述**有内容**的通道。
    #[test]
    fn status_state_sources_are_consistent() {
        // 两个输出：按名字序 DP-1 在前但**没有内容**，内容在 HDMI-A-1。
        let env = TestEnv::new("status-consistency");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [0, 255, 0, 255]);
        let mut rt = env.runtime_with(&env.toml("", 1), &["DP-1", "HDMI-A-1"]);

        let check = |rt: &Runtime, step: &str| {
            let c = rt.state_contradictions();
            assert!(c.is_empty(), "{step}: 状态源矛盾 {c:?}");
        };
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "HDMI-A-1"}),
        );
        check(&rt, "set");
        // 顶层字段必须描述**有内容**的通道（旧实现取 channels.first() = DP-1，全空）。
        let st = status_json(&mut rt);
        assert_eq!(st["output"], "HDMI-A-1", "顶层应描述有内容的通道");
        assert!(
            st["media"].as_str().unwrap_or_default().ends_with("a.png"),
            "顶层 media 不应为空: {st}"
        );
        assert_eq!(st["decoder"], "static");

        ok_json(
            &mut rt,
            Method::Pause,
            serde_json::json!({"output": "HDMI-A-1"}),
        );
        check(&rt, "pause");
        assert_eq!(status_json(&mut rt)["state"], "Paused");
        ok_json(
            &mut rt,
            Method::Next,
            serde_json::json!({"output": "HDMI-A-1"}),
        );
        check(&rt, "next(暂停中)");
        ok_json(
            &mut rt,
            Method::Resume,
            serde_json::json!({"output": "HDMI-A-1"}),
        );
        check(&rt, "resume");
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": b.display().to_string(), "output": "HDMI-A-1"}),
        );
        check(&rt, "set(b)");
        let st = status_json(&mut rt);
        assert_eq!(st["state"], "StaticIdle");
        assert_eq!(st["channels"][1]["state"], "StaticIdle");
        // 长时钟推进：轮换照常发生，且不产生矛盾。
        let start = Instant::now();
        for step in 1..=3 {
            rt.tick(start + Duration::from_millis(step * 1_200));
            check(&rt, "tick 轮换");
        }
    }
    /// 永远报错的视频源（模拟"播放中解码管线失败"）。
    struct FailingVideo;

    impl liuguang_media::MediaSource for FailingVideo {
        fn metadata(&self) -> liuguang_core::types::MediaInfo {
            liuguang_core::types::MediaInfo {
                kind: MediaKind::Video,
                width: 2,
                height: 2,
                duration: None,
                source_fps: Some(60.0),
                codec: Some("h264".into()),
            }
        }

        fn poll_frame(
            &mut self,
            _now: Instant,
        ) -> Result<liuguang_media::FramePoll, liuguang_media::MediaError> {
            Err(liuguang_media::MediaError::Decode("注入的解码失败".into()))
        }

        fn pause(&mut self) -> Result<(), liuguang_media::MediaError> {
            Ok(())
        }

        fn resume(&mut self) -> Result<(), liuguang_media::MediaError> {
            Ok(())
        }
    }

    impl liuguang_media::video::VideoSource for FailingVideo {
        fn video_stats(&self) -> VideoStats {
            VideoStats {
                presented: 0,
                dropped_late: 0,
                dropped_queue_full: 0,
                queue_len: 0,
                skipped: 0,
                decoder: liuguang_media::video::DecoderKind::Software,
                source_fps: Some(60.0),
                last_pts: None,
            }
        }

        fn next_deadline(&self) -> Option<Instant> {
            None
        }
    }

    /// F2（AUDIT 后续）：视频运行期失败且**没有后备画面**时，`status` 必须如实
    /// 停在 `DegradedStatic`，不得谎称 `StaticIdle`（规范 §8.3）。
    #[test]
    fn video_error_without_fallback_reports_degraded_not_static_idle() {
        let env = TestEnv::new("degraded");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        // 配置里**不给** fallback_image。
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        assert_eq!(rt.state.current(), State::StaticIdle);
        // 注入必然失败的视频源：模拟播放中解码管线报错。
        rt.channels.get_mut("DP-1").expect("通道").video = Some(Box::new(FailingVideo));
        rt.tick(Instant::now());
        let st = status_json(&mut rt);
        assert_eq!(
            st["state"], "DegradedStatic",
            "没有后备画面时必须如实报告降级，不得谎称 StaticIdle（实际 {st}"
        );
        assert_eq!(
            st["decoder"], "none",
            "已无解码器在跑时 decoder 字段不得继续声称视频解码器"
        );
        assert!(rt.channels.get("DP-1").expect("通道").video.is_none());
        assert!(rt.state_contradictions().is_empty());
    }

    /// F2（B 面）：配置了 fallback_image 时，视频失败后必须**真的**换上后备画面
    /// （status.media 指向后备图、通道回到稳定态、状态机无非法转移）。
    #[test]
    fn video_error_with_fallback_really_shows_fallback_image() {
        let env = TestEnv::new("degraded-fallback");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let fb = env.png("fallback.png", 32, 32, [0, 0, 255, 255]);
        let outputs = format!(
            "[outputs.\"DP-1\"]\nfit = \"cover\"\nzoom = 1.0\noffset_x = 0.0\noffset_y = 0.0\nfallback_image = \"{}\"\n",
            fb.display()
        );
        let mut rt = env.runtime(&env.toml(&outputs, 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        rt.channels.get_mut("DP-1").expect("通道").video = Some(Box::new(FailingVideo));
        rt.tick(Instant::now());
        let st = status_json(&mut rt);
        assert!(
            st["media"]
                .as_str()
                .unwrap_or_default()
                .ends_with("fallback.png"),
            "必须真的显示后备画面，实际 media={}",
            st["media"]
        );
        assert_eq!(st["decoder"], "static");
        let top = st["state"].as_str().unwrap_or_default();
        assert!(
            matches!(top, "StaticIdle" | "DegradedStatic"),
            "显示成功后状态应稳定，实际 {top}"
        );
        assert_eq!(rt.state_machine_errors(), 0, "后备画面路径不得产生非法转移");
        assert!(rt.state_contradictions().is_empty());
    }
    /// P0-3 接线（真机暴露的场景）：**转场进行中又来一次 set** 不得产生非法转移，
    /// 转场结束时状态必须收敛到 StaticIdle。
    ///
    /// 修复前的实际行为（真机日志 10:12:01）：`Transitioning + SwitchRequested`
    /// 被拒绝（计数 +1），随后 `StaticIdle + StaticShown` 又被拒绝（计数 +1），
    /// 并留下 "I4 通道正在转场但顶层状态是 StaticIdle" 的矛盾。
    #[test]
    fn set_during_in_flight_transition_keeps_wiring_legal() {
        let env = TestEnv::new("transition-race");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [0, 255, 0, 255]);
        let outputs =
            "[outputs.\"DP-1\"]\nfit = \"cover\"\nzoom = 1.0\noffset_x = 0.0\noffset_y = 0.0\n";
        let mut rt = env.runtime(&env.toml_with(outputs, 3600, true));
        let set = |rt: &mut Runtime, p: &std::path::Path| {
            ok_json(
                rt,
                Method::SetWallpaper,
                serde_json::json!({"path": p.display().to_string(), "output": "DP-1"}),
            );
        };
        set(&mut rt, &a);
        assert_eq!(
            rt.state.current(),
            State::StaticIdle,
            "首帧无转场，直接 StaticIdle"
        );
        // 第二次 set：启动转场 → 顶层应停在 Transitioning（画面还在动）。
        set(&mut rt, &b);
        assert_eq!(
            rt.state.current(),
            State::Transitioning,
            "有转场时不得提前宣布 StaticIdle（否则 status 说谎）"
        );
        assert!(rt.state_contradictions().is_empty());
        // 转场进行中再来一次 set（真机场景）：不得产生非法转移。
        set(&mut rt, &a);
        assert_eq!(
            rt.state_machine_errors(),
            0,
            "转场中重复 set 不得产生非法转移"
        );
        assert_eq!(rt.state.current(), State::Transitioning);
        assert!(rt.state_contradictions().is_empty());
        // 转场结束：收敛到 StaticIdle，且仍然没有非法转移。
        rt.tick(Instant::now() + Duration::from_millis(500));
        assert_eq!(rt.state.current(), State::StaticIdle, "转场结束应收敛");
        assert_eq!(
            rt.state_machine_errors(),
            0,
            "转场结束的终态驱动不得重复（StaticIdle + StaticShown 是非法转移）"
        );
        assert!(rt.state_contradictions().is_empty());
        assert_eq!(status_json(&mut rt)["channels"][0]["state"], "StaticIdle");
    }

    /// P0-3/F3：**冷启动后的第一次显示**不得产生非法转移（旧实现留 2 条 WARN）。
    #[test]
    fn cold_start_first_display_is_legal_wiring() {
        let env = TestEnv::new("cold-start");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        assert_eq!(rt.state.current(), State::InitializingDisplay, "冷启动状态");
        let events = rt.events.subscribe();
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        assert_eq!(rt.state.current(), State::StaticIdle);
        assert_eq!(
            rt.state_machine_errors(),
            0,
            "冷启动首次显示不得产生非法转移"
        );
        let mut errors = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if ev.kind == liuguang_ipc::protocol::EventKind::Error {
                errors.push(ev.message.unwrap_or_default());
            }
        }
        assert!(errors.is_empty(), "不应有 Error 事件: {errors:?}");
    }

    /// t27 / R2-F1（high）：**首次 tick 早于通道建立**时，启动首帧补偿不得被烧掉。
    ///
    /// 真机故障（t14 复现）：`init_channels()` 在"无输出枚举结果"时直接返回，
    /// 通道要等热插拔路径建立；若此刻已经 tick 过一次，旧实现把一次性闩
    /// `primed_first_frame` 置位却没有候选可点亮 ⇒ 补偿永久失效，首帧只能等
    /// **整个** `interval_seconds`（`interval=10` ⇒ 实测 11.7s）。
    ///
    /// 变异（必须让本测试失败）：把 `prime_first_frame` 里的
    /// `if keys.is_empty() { return; }` 删掉（恢复"先置闩"），本测试即变红。
    #[test]
    fn prime_first_frame_survives_missing_channels_at_first_tick() {
        let env = TestEnv::new("prime-first-frame");
        env.png("a.png", 16, 16, [255, 0, 0, 255]);
        // interval 很大：任何"靠调度轮换才能出首帧"的实现都会让本测试超时失败。
        let mut rt = env.runtime(&env.toml("", 3600));
        age_startup(&mut rt);
        assert!(!rt.channels.is_empty(), "前提：启动时通道已建立");

        // 模拟"输出尚未枚举 / 通道还没建立"的首次 tick：清空通道后立刻 tick。
        rt.channels.clear();
        let t0 = Instant::now();
        rt.tick(t0);
        assert!(rt.channels.is_empty(), "前提：该 tick 时没有通道");

        // 输出随后被枚举到（真实路径是热插拔分支调用 init_channels）。
        let _ = rt.init_channels();
        assert!(!rt.channels.is_empty(), "前提：通道已建立");

        // 关键断言：通道建立后，**最多 2 个 tick** 内必须点亮首帧。
        for step in 1..=2 {
            rt.tick(t0 + Duration::from_millis(500 * step));
            if rt.channels.values().all(|c| c.current.is_some()) {
                break;
            }
        }
        assert!(
            rt.channels.values().all(|c| c.current.is_some()),
            "通道建立后的第一个补偿 tick 内必须出首帧（旧实现：闩已烧 ⇒ 要等 3600s 的 interval）"
        );
    }

    /// t27 / R2-F3（high）：`pause` 之后 `next` **不得**静默解除暂停。
    ///
    /// 背景：t7 声称修好、t14 第二轮"证伪"（`paused=true→false`）。审计（core-auditor）
    /// 穷举 `ch.paused` 的写点后判定：**当前源码里没有这条路径**（唯一清除点是
    /// `handle_resume`；`show_image` 只调用 `restore_paused_state()` 恢复顶层状态机，
    /// 不写 `ch.paused`）⇒ t14 的观测很可能来自旧部署二进制。本测试是这条不变式的
    /// **唯一防线**（三处一致：通道 `paused` / `status.channels[].paused` / 顶层 `state`）。
    ///
    /// 变异（必须让本测试失败）：在 `handle_next` 或 `switch_next` 成功路径上插入
    /// `ch.paused = false;`，本测试即变红。
    #[test]
    fn next_does_not_clear_pause() {
        let env = TestEnv::new("pause-next");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        env.png("b.png", 16, 16, [0, 255, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        ok_json(
            &mut rt,
            Method::Pause,
            serde_json::json!({"output": "DP-1"}),
        );
        assert_eq!(
            status_json(&mut rt)["state"],
            "Paused",
            "暂停后顶层必须是 Paused"
        );

        let shown_before = rt
            .channels
            .get("DP-1")
            .and_then(|ch| ch.current.as_ref().map(|c| c.media.clone()));
        ok_json(&mut rt, Method::Next, serde_json::json!({"output": "DP-1"}));

        // 三处一致：通道内部 / status 通道 / 顶层状态。
        // （先把不可变借用里的值取出来，再对 `rt` 取可变借用调 `status_json`。）
        let (ch_paused, shown_after) = {
            let ch = rt.channels.get("DP-1").expect("通道");
            (ch.paused, ch.current.as_ref().map(|c| c.media.clone()))
        };
        assert!(ch_paused, "next 不得静默解除通道暂停（t14 的 R2-F3）");
        let st = status_json(&mut rt);
        assert_eq!(st["state"], "Paused", "next 之后顶层仍须是 Paused");
        assert_eq!(st["paused"], true, "status.paused 必须与通道一致");
        assert_eq!(
            st["channels"][0]["paused"], true,
            "status.channels[].paused 必须与通道一致"
        );
        // `next` 本身仍应生效（暂停只禁自动轮换，不禁显式切换）——若实现改成
        // "暂停时拒绝 next"，这里会暴露语义变化，值得人看一眼。
        assert_ne!(
            shown_before, shown_after,
            "暂停中的显式 next 仍应切换到下一项（仅自动轮换被暂停）"
        );
        assert!(
            rt.channels.get("DP-1").expect("通道").current.is_some(),
            "暂停中 next 之后必须有内容"
        );
    }
    /// 接线守卫：`drive_shown` 是**幂等**的 —— 已经处于目标稳定态时不得再驱动
    /// 终态事件（`StaticIdle + StaticShown` 是非法转移，会在真机上污染计数器并
    /// 掩盖真正的接线错误；这一步由"转场中重复 set"的叠加场景触发）。
    #[test]
    fn drive_shown_is_idempotent() {
        let env = TestEnv::new("drive-shown-idempotent");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        assert_eq!(rt.state.current(), State::StaticIdle);
        assert_eq!(rt.state_machine_errors(), 0);
        // 已经是 StaticIdle 了，再次宣布"静态画面已生效"必须是空操作。
        rt.drive_shown("DP-1");
        assert_eq!(
            rt.state_machine_errors(),
            0,
            "重复宣布终态不得产生非法转移（StaticIdle + StaticShown 非法）"
        );
        assert_eq!(rt.state.current(), State::StaticIdle);
        assert!(rt.state_contradictions().is_empty());
    }
    /// P0-2（另一半）：`[outputs.<name>].interval_seconds` 必须覆盖全局
    /// `[slideshow].interval_seconds`；间隔的**唯一来源是配置**，且按输出优先。
    #[test]
    fn per_output_interval_overrides_global() {
        let env = TestEnv::new("per-output-interval");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [0, 255, 0, 255]);
        assert!(a.exists() && b.exists());
        // 全局 3600s（等于"永不轮换"），输出段 1s。
        let outputs = "[outputs.\"DP-1\"]\nfit = \"cover\"\nzoom = 1.0\noffset_x = 0.0\noffset_y = 0.0\ninterval_seconds = 1\n";
        let mut rt = env.runtime(&env.toml_with(outputs, 3600, false));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        let first = current_media(&rt, "DP-1");
        let start = Instant::now();
        let mut rotated = false;
        for step in 1..=3 {
            rt.tick(start + Duration::from_millis(step * 1_000));
            if current_media(&rt, "DP-1") != first {
                rotated = true;
                break;
            }
        }
        assert!(
            rotated,
            "输出段 interval_seconds=1 必须生效（全局 3600s 不得压过它；P0-2 复发）"
        );
        assert!(current_media(&rt, "DP-1").ends_with("b.png"));
    }
    /// 可编程的视频源 stub：用于停摆看门狗测试（不产帧 / 正常产帧）。
    struct StubVideo {
        presented: u64,
        advance: bool,
        /// `resume()` 之后还要"解码"几次才出帧：模拟真实播放器恢复后**首帧尚未
        /// 到期**的窗口（`VideoPlayer::resume_at` 把时间轴后移，队首帧通常还差
        /// ~一帧才到期）。这正是暂停恢复后第一个 tick 的真实状态。
        wait_after_resume: u32,
        /// 剩余等待次数（`resume()` 时装载，poll 到 0 后才开始出帧）。
        resume_wait: u32,
    }

    impl liuguang_media::MediaSource for StubVideo {
        fn metadata(&self) -> liuguang_core::types::MediaInfo {
            liuguang_core::types::MediaInfo {
                kind: MediaKind::Video,
                width: 16,
                height: 16,
                duration: None,
                source_fps: Some(60.0),
                codec: Some("stub".into()),
            }
        }

        fn poll_frame(
            &mut self,
            _now: Instant,
        ) -> Result<liuguang_media::FramePoll, liuguang_media::MediaError> {
            if self.resume_wait > 0 {
                // 恢复后的首帧还没到期：真实播放器此时就是 Pending（不是停摆）。
                self.resume_wait -= 1;
                return Ok(liuguang_media::FramePoll::Pending {
                    next_in: Some(Duration::from_millis(16)),
                });
            }
            if self.advance {
                self.presented += 1;
                let stride = 16 * 4;
                Ok(liuguang_media::FramePoll::Ready(DisplayFrame::Cpu(
                    liuguang_core::types::CpuFrame {
                        width: 16,
                        height: 16,
                        stride,
                        format: liuguang_core::types::PixelFormat::Xrgb8888,
                        data: std::sync::Arc::from(
                            vec![0u8; (stride * 16) as usize].into_boxed_slice(),
                        ),
                    },
                )))
            } else {
                // 永远"还在解码"：既不产帧也不报错（看门狗要抓的就是这种）。
                Ok(liuguang_media::FramePoll::Pending {
                    next_in: Some(Duration::from_millis(4)),
                })
            }
        }

        fn pause(&mut self) -> Result<(), liuguang_media::MediaError> {
            Ok(())
        }

        fn resume(&mut self) -> Result<(), liuguang_media::MediaError> {
            self.resume_wait = self.wait_after_resume;
            Ok(())
        }
    }

    impl liuguang_media::video::VideoSource for StubVideo {
        fn video_stats(&self) -> VideoStats {
            VideoStats {
                presented: self.presented,
                dropped_late: 0,
                dropped_queue_full: 0,
                queue_len: 0,
                skipped: 0,
                decoder: liuguang_media::video::DecoderKind::Software,
                source_fps: Some(60.0),
                last_pts: None,
            }
        }

        fn next_deadline(&self) -> Option<Instant> {
            None
        }
    }

    /// 在通道上挂一个 stub 视频源（等价于 show_video 挂上真实播放器）。
    fn attach_stub_video(rt: &mut Runtime, key: &str, advance: bool) {
        attach_stub_video_with(rt, key, advance, 0);
    }

    /// 同 [`attach_stub_video`]，但可指定 `resume()` 之后还要等几次 poll 才出帧。
    fn attach_stub_video_with(rt: &mut Runtime, key: &str, advance: bool, wait_after_resume: u32) {
        let now = Instant::now();
        let ch = rt.channels.get_mut(key).expect("通道");
        ch.video = Some(Box::new(StubVideo {
            presented: 0,
            advance,
            wait_after_resume,
            resume_wait: 0,
        }));
        ch.reset_stall_watchdog(now);
    }

    /// 只记录 `set_fps_cap` 调用的视频替身（P2-5 降载接线的事实来源）。
    struct CapVideo {
        caps: std::sync::Arc<std::sync::Mutex<Vec<f64>>>,
    }

    impl liuguang_media::MediaSource for CapVideo {
        fn metadata(&self) -> liuguang_media::MediaInfo {
            liuguang_media::MediaInfo {
                kind: liuguang_core::types::MediaKind::Video,
                width: 16,
                height: 16,
                duration: None,
                source_fps: Some(60.0),
                codec: Some("cap".into()),
            }
        }

        fn poll_frame(
            &mut self,
            _now: Instant,
        ) -> Result<liuguang_media::FramePoll, liuguang_media::MediaError> {
            Ok(liuguang_media::FramePoll::Pending {
                next_in: Some(Duration::from_millis(20)),
            })
        }

        fn pause(&mut self) -> Result<(), liuguang_media::MediaError> {
            Ok(())
        }

        fn resume(&mut self) -> Result<(), liuguang_media::MediaError> {
            Ok(())
        }
    }

    impl liuguang_media::video::VideoSource for CapVideo {
        fn video_stats(&self) -> VideoStats {
            VideoStats {
                presented: 0,
                dropped_late: 0,
                dropped_queue_full: 0,
                queue_len: 0,
                skipped: 0,
                decoder: liuguang_media::video::DecoderKind::Software,
                source_fps: Some(60.0),
                last_pts: None,
            }
        }

        fn next_deadline(&self) -> Option<Instant> {
            None
        }

        fn set_fps_cap(&mut self, fps: Option<f64>) {
            if let Some(fps) = fps {
                self.caps.lock().expect("锁").push(fps);
            }
        }
    }

    /// 在通道上挂一个记录上限的视频替身。
    fn attach_cap_video(rt: &mut Runtime, key: &str) -> std::sync::Arc<std::sync::Mutex<Vec<f64>>> {
        let caps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ch = rt.channels.get_mut(key).expect("通道");
        ch.video = Some(Box::new(CapVideo { caps: caps.clone() }));
        caps
    }

    /// **P2-5b**：CPU 压力档必须真的落到每一路在播视频的提交频率上限上。
    ///
    /// `reduce_load_on_cpu_pressure` 以前只是配置面的一句话；这里用替身把
    /// "档位 → 实际下发的 fps 上限"钉住（媒体层里"上限生效会挡住出帧"另有测试）。
    #[test]
    fn cpu_pressure_level_drives_the_per_channel_fps_cap() {
        let env = TestEnv::new("loadshed-cap");
        let mut rt = env.runtime(&env.toml("", 3600));
        let caps = attach_cap_video(&mut rt, "DP-1");
        // 配置值 30fps（`TestEnv::toml` 里写死）。
        rt.apply_load_level(crate::loadshed::LoadLevel::Shed);
        rt.apply_load_level(crate::loadshed::LoadLevel::Normal);
        let got = caps.lock().expect("锁").clone();
        assert_eq!(
            got,
            vec![15.0, 30.0],
            "降载档应减半、恢复档应回到配置值（否则配置只是注释）"
        );
    }

    /// **P2-5b**：`reduce_load_on_cpu_pressure = false` 时，reload 必须把已被压下去
    /// 的上限**恢复回配置值**，而不是让半速一直留着。
    #[test]
    fn disabling_pressure_shedding_restores_the_configured_cap() {
        let env = TestEnv::new("loadshed-off");
        let mut rt = env.runtime(&env.toml("", 3600));
        let caps = attach_cap_video(&mut rt, "DP-1");
        rt.apply_load_level(crate::loadshed::LoadLevel::Shed);
        rt.config.performance.reduce_load_on_cpu_pressure = false;
        rt.sync_performance();
        let got = caps.lock().expect("锁").clone();
        assert_eq!(
            got.last(),
            Some(&30.0),
            "关掉降载后最后一次下发必须是配置上限，实际序列 {got:?}"
        );
        assert_eq!(rt.shed.level(), crate::loadshed::LoadLevel::Normal);
    }

    /// **P2-5a**：预加载过的图片在展示时刻不再同步解码；素材被换掉时必须算未命中。
    #[test]
    fn preload_hit_skips_decode_and_stale_file_is_not_served() {
        let env = TestEnv::new("preload-hit");
        let a = env.png("a.png", 16, 16, [1, 2, 3, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        rt.shed.set_enabled(false); // 别让本机真实 CPU 压力影响本用例
        rt.channels.get_mut("DP-1").expect("通道").preload_target = Some(a.clone());

        rt.run_pending_preloads();
        assert_eq!(rt.preload.len(), 1, "预加载必须真的解码并入库");
        assert_eq!(rt.preload.bytes(), 16 * 16 * 4);
        assert!(
            rt.channels
                .get("DP-1")
                .expect("通道")
                .preload_target
                .is_none(),
            "已完成的预加载意图应被消费掉（不得每个 tick 重做一遍）"
        );

        // 命中：取到帧，条目与凭据一起移出（同一份像素不得被消费两次）。
        assert!(
            rt.take_preloaded(&a).is_some(),
            "文件没变时必须命中（这正是 `preload_next` 省下的那次同步解码）"
        );
        assert!(rt.take_preloaded(&a).is_none(), "take 之后应未命中");
        assert!(
            rt.preload_stamps.is_empty(),
            "取用后文件凭据应一并清除（只增不减就是内存泄漏）"
        );

        // 同名文件被换成另一张图（长度不同）⇒ 缓存帧必须判为过期，宁可从新解码。
        rt.channels.get_mut("DP-1").expect("通道").preload_target = Some(a.clone());
        rt.run_pending_preloads();
        assert_eq!(rt.preload.len(), 1, "重新排队后应再次入库");
        env.png("a.png", 64, 64, [7, 7, 7, 255]);
        assert!(
            rt.take_preloaded(&a).is_none(),
            "素材已被替换：不得端出上一版的像素"
        );
        assert!(
            rt.preload.is_empty() && rt.preload_stamps.is_empty(),
            "过期条目应随取用一并清除，不留僵尸记账"
        );
    }

    /// **P2-5a**：`max_cache_mb` 就是淘汰依据 —— 预算容不下的图**永不入库**，
    /// 调大预算后同一张图才进得来。
    #[test]
    fn cache_budget_decides_what_can_be_cached() {
        let env = TestEnv::new("cache-budget");
        // 1024×1024 RGBA 解码后 = 4 MiB。
        let big = env.png("big.png", 1024, 1024, [3, 3, 3, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        rt.shed.set_enabled(false);
        rt.config.performance.max_cache_mb = 1;
        rt.sync_performance();
        rt.channels.get_mut("DP-1").expect("通道").preload_target = Some(big.clone());
        rt.run_pending_preloads();
        assert!(
            rt.preload.is_empty(),
            "单张 4 MiB 超过 1 MiB 预算 ⇒ 不应入库（预算必须真的参与决策）"
        );
        assert!(rt.take_preloaded(&big).is_none());

        rt.config.performance.max_cache_mb = 128;
        rt.sync_performance();
        rt.channels.get_mut("DP-1").expect("通道").preload_target = Some(big.clone());
        rt.run_pending_preloads();
        assert_eq!(rt.preload.len(), 1, "预算调大后同一张图应能缓存");
        assert_eq!(rt.preload.bytes(), 1024 * 1024 * 4);
    }

    /// **t39**：显式应用只解除**用户暂停**；熄屏 / 全屏让路的暂停必须保持。
    ///
    /// 语义边界：`set_wallpaper` 是"我要看这个"的用户意图，但熄屏与全屏是**系统
    /// 状态**——应用一张壁纸不该把熄屏的机器点亮，也不该把全屏游戏挤出去。
    #[test]
    fn explicit_set_only_clears_user_pause() {
        for (reason, prepare) in [("screen_off", 0u8), ("fullscreen", 1u8)] {
            let env = TestEnv::new(&format!("t39-set-{reason}"));
            let a = env.png("a.png", 16, 16, [9, 9, 9, 255]);
            let b = env.png("b.png", 16, 16, [200, 200, 200, 255]);
            let mut rt = env.runtime(&env.toml("", 3600));
            ok_json(
                &mut rt,
                Method::SetWallpaper,
                serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
            );
            // 模拟系统侧暂停（熄屏 / 全屏让路）：pause_video_supply + 记录来源集合。
            rt.pause_video_supply("DP-1");
            match prepare {
                0 => {
                    rt.screen_paused.insert("DP-1".to_string());
                }
                _ => {
                    rt.fullscreen_paused.insert("DP-1".to_string());
                }
            }
            assert_eq!(rt.pause_reason_of("DP-1"), Some(reason));

            ok_json(
                &mut rt,
                Method::SetWallpaper,
                serde_json::json!({"path": b.display().to_string(), "output": "DP-1"}),
            );
            assert_eq!(
                rt.pause_reason_of("DP-1"),
                Some(reason),
                "{reason} 暂停不得被 set_wallpaper 解除（实际 {:?}）",
                rt.pause_reason_of("DP-1")
            );
            assert!(
                rt.channels.get("DP-1").expect("通道").paused,
                "{reason} 暂停必须保持"
            );
            assert!(rt.state_contradictions().is_empty());
        }
    }

    /// **t39**：tick 间隔异常（休眠 / `SIGSTOP`）不是"视频停摆"。
    ///
    /// 旧行为：冻结 12s 后 `CONT`，下一 tick 看到 `now - last_presented_at`
    /// 巨大 ⇒ 立刻重建解码管线（真机日志「视频停摆：已自动重建解码管线
    /// attempt=1」，画面白闪）。新行为：间隔 > [`TICK_GAP_RESET`] 只把看门狗
    /// 基线挪到 `now`，既不重建也不降级。
    #[test]
    fn long_tick_gap_is_treated_as_sleep_not_stall() {
        let env = TestEnv::new("t39-gap");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        attach_stub_video(&mut rt, "DP-1", false);
        let t0 = Instant::now();
        rt.tick(t0);
        // 模拟进程被冻结 30s（远超停摆阈值 10s）。
        let after = t0 + Duration::from_secs(30);
        rt.tick(after);
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(ch.video.is_some(), "长间隔不得拆掉视频源（不是停摆）");
        assert_eq!(ch.stall_rebuilds, 0, "长间隔不得触发重建");
        assert_eq!(
            ch.last_presented_at, after,
            "看门狗基线必须挪到恢复时刻，丢掉冻结期的累计停顿"
        );
        let st = status_json(&mut rt);
        assert_ne!(st["state"], "DegradedStatic", "长间隔不得降级（实际 {st}）");
        assert!(rt.state_contradictions().is_empty());
    }

    /// **t39**：缩略图服务在 daemon 里真的被复用 —— 第二次同参数请求走缓存
    /// （毫秒级返回、不再解码），且 `get_metrics` 的计数能看见这件事。
    #[test]
    fn thumbnail_requests_reuse_the_cache_and_show_up_in_metrics() {
        let env = TestEnv::new("t39-thumb-cache");
        let a = env.png("a.png", 48, 32, [10, 20, 30, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        let id = a.display().to_string();
        let params = serde_json::json!({"id": id, "max_width": 64, "max_height": 64});
        let first = call_async(&mut rt, Method::GetThumbnail, params.clone());
        assert!(first.ok, "首次缩略图必须成功：{:?}", first.error);
        let body = first.result.expect("结果");
        assert_eq!(body["format"], "png");
        assert_eq!(body["width"], 48);
        assert_eq!(body["height"], 32);
        assert!(!body["data_b64"].as_str().unwrap_or("").is_empty());

        let started = Instant::now();
        let second = call_async(&mut rt, Method::GetThumbnail, params);
        let elapsed = started.elapsed();
        assert!(second.ok, "命中缓存必须成功：{:?}", second.error);
        assert!(
            elapsed < Duration::from_millis(50),
            "命中缓存必须毫秒级返回，实际 {elapsed:?}"
        );

        let m = ok_json(&mut rt, Method::GetMetrics, serde_json::json!({}));
        assert_eq!(m["thumb_requests"], 2);
        assert!(
            m["thumb_hits"].as_u64().unwrap_or(0) >= 1,
            "第二次必须记成命中：{m}"
        );
        assert_eq!(m["thumb_decoded"], 1, "同一张图只该解码一次：{m}");
        assert_eq!(m["thumb_queue_depth"].as_u64().unwrap_or(0), 0);
    }

    /// §11.2「拔除 ⇒ 关闭对应输出流与缓存」：最后一块屏拔掉时释放缓存。
    #[test]
    fn unplugging_the_last_output_frees_the_preload_cache() {
        let env = TestEnv::new("unplug-cache");
        let a = env.png("a.png", 16, 16, [1, 1, 1, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        rt.shed.set_enabled(false);
        rt.channels.get_mut("DP-1").expect("通道").preload_target = Some(a.clone());
        rt.run_pending_preloads();
        assert_eq!(rt.preload.len(), 1);
        // 直接走"通道被移除"后的收尾语义：拔除后缓存必须归零。
        rt.channels.remove("DP-1");
        rt.preload.clear();
        rt.preload_stamps.clear();
        assert!(rt.preload.is_empty() && rt.preload_stamps.is_empty());
    }

    /// t11：daemon 侧停摆兜底 —— 视频在跑但 `presented` 长时间零增长（且源侧
    /// 既不产帧也不报错）必须收敛到 **DegradedStatic**，而不是让画面冻结、
    /// 状态仍自称在播放。
    #[test]
    fn video_stall_watchdog_converges_to_degraded_static() {
        let env = TestEnv::new("stall-watchdog");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        attach_stub_video(&mut rt, "DP-1", false);
        let t0 = Instant::now();
        rt.tick(t0);
        assert!(rt.channels.get("DP-1").expect("通道").video.is_some());
        // 阈值（10s）内不得误判。t39 起**逐步**推进 2s（单次 >3s 会被判成
        // "进程没在跑"并重置看门狗，那是休眠/STOP 的语义，不是停摆）。
        for step in 1..=2 {
            rt.tick(t0 + Duration::from_secs(step * 2));
            assert!(
                rt.channels.get("DP-1").expect("通道").video.is_some(),
                "阈值内不得误判停摆"
            );
        }
        // 超过阈值：必须收敛到 DegradedStatic 并拆掉视频源。
        for step in 3..=6 {
            rt.tick(t0 + Duration::from_secs(step * 2));
        }
        let st = status_json(&mut rt);
        assert_eq!(
            st["state"], "DegradedStatic",
            "停摆必须如实降级（实际 {st}）"
        );
        assert!(
            rt.channels.get("DP-1").expect("通道").video.is_none(),
            "停摆后必须拆掉视频源，停止空转"
        );
        assert_eq!(st["decoder"], "none");
        assert!(rt.state_contradictions().is_empty());
    }

    /// 停摆自愈：管线确实死了时，先**尝试重建**（暂停/熄屏恢复后用户不应重启
    /// 服务），重建失败再走原来的降级路径 —— 既不能静默不动，也不能无限重建。
    ///
    /// 这里用"当前媒体是 `.mp4` 但文件已不存在"模拟"重建必然失败"：
    /// `show_video_with` 会在 `check_media_path` 就返回错误，不会真的去开解码器
    /// （单测里既不需要 GStreamer/VA-API，也不会因为等管线而起慢）。
    #[test]
    fn stall_rebuild_is_attempted_before_degrading() {
        let env = TestEnv::new("stall-rebuild");
        let a = env.png("a.png", 16, 16, [0, 0, 255, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        // 把"当前媒体"改成视频路径，但**不创建文件**：重建只能失败。
        let gone = env.media_dir().join("gone.mp4");
        rt.channels
            .get_mut("DP-1")
            .expect("通道")
            .current
            .as_mut()
            .expect("当前画面")
            .media = gone;
        attach_stub_video(&mut rt, "DP-1", false);
        let t0 = Instant::now();
        for step in 1..=3 {
            rt.tick(t0 + Duration::from_secs(step * 2));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(ch.video.is_some(), "阈值内不得判停摆");
        assert_eq!(ch.stall_rebuilds, 0, "阈值内不得尝试重建");

        // 超过阈值：先尝试一次重建（失败），然后才降级。
        for step in 4..=6 {
            rt.tick(t0 + Duration::from_secs(step * 2));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        assert_eq!(
            ch.stall_rebuilds, 1,
            "判停摆后必须先尝试重建一次，而不是直接降级"
        );
        assert!(ch.video.is_none(), "重建失败后仍须降级（不得掩盖真故障）");
        assert_eq!(ch.current.as_ref().expect("当前画面").decoder, "none");
        assert_eq!(status_json(&mut rt)["state"], "DegradedStatic");
        assert!(rt.state_contradictions().is_empty());
    }

    /// t19：提交失败与 master 状态必须**直接出现在 status / 对账载荷**里
    /// （此前只能 grep 日志），且"出队计数"与"提交失败计数"能互相对账。
    #[test]
    fn metrics_expose_present_failures_and_reconciliation() {
        let env = TestEnv::new("t19-metrics");
        let mut rt = env.runtime(&env.toml("", 3600));
        attach_stub_video(&mut rt, "DP-1", true);
        {
            let ch = rt.channels.get_mut("DP-1").expect("通道");
            ch.presented_ok = 7;
            ch.present_failures = 5;
            ch.present_failures_consecutive = 2;
        }
        // ① status：运维/TUI 直接看到提交失败（不再只有日志）。
        let st = status_json(&mut rt);
        let vid = &st["channels"][0]["video"];
        assert_eq!(vid["present_ok"], 7, "status 必须暴露提交成功计数");
        assert_eq!(vid["present_failures"], 5, "status 必须暴露提交失败计数");
        assert_eq!(vid["present_failures_consecutive"], 2);
        // ② 对账载荷：presented（出队）与 present_failures 一起可用于判异常。
        let raw = ok_json(&mut rt, Method::GetMetrics, serde_json::json!({}));
        let metrics: liuguang_ipc::protocol::MetricsResult =
            serde_json::from_value(raw).expect("metrics 载荷可反序列化");
        let ch = metrics
            .channels
            .iter()
            .find(|c| c.output == "DP-1")
            .expect("通道");
        assert_eq!(ch.present_ok, 7);
        assert_eq!(ch.present_failures, 5);
        // 异常签名：媒体侧出队过帧 + 显示侧有失败 ⇒ contradicts() 为真。
        assert!(
            ch.presented == 0 || ch.contradicts(),
            "presented={} failures={} 必须能判出异常",
            ch.presented,
            ch.present_failures
        );
        // ③ 显示侧提交计数可见，且**语义正确**：成功总数 = 直接成功 + 重试成功 + legacy 回退
        //    （`commit_ok` 不含重试成功的那部分；它是"提交成功"而不是"上屏"）。
        let c = metrics.commit.expect("mock 后端自 t3 起提供 commit 计数");
        assert_eq!(
            c.commit_success_total,
            c.commit_ok + c.retried_after_busy + c.legacy_fallback,
            "成功总数必须等于 commit_ok + retried_after_busy + legacy_fallback"
        );
        assert!(c.attempts >= c.commit_success_total);
        // mock 无 DRM master 概念 ⇒ 必须 None（"不适用"，不是假装 0/false）。
        assert!(metrics.master.is_none(), "mock 后端不得伪造 master 状态");
    }

    /// t11：看门狗必须排除**合法的零增长**（captain/core-auditor 指定的两个边界）：
    /// 暂停（不拉帧）与正常产帧（含极低 fps 场景）都不得被误判。
    #[test]
    fn video_stall_watchdog_exempts_paused_and_advancing_video() {
        let env = TestEnv::new("stall-exempt");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);

        // (a) 暂停：阈值内/外都不得判停摆（暂停本来就零增长）。
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        attach_stub_video(&mut rt, "DP-1", false);
        ok_json(
            &mut rt,
            Method::Pause,
            serde_json::json!({"output": "DP-1"}),
        );
        rt.tick(Instant::now() + Duration::from_secs(30));
        assert!(
            rt.channels.get("DP-1").expect("通道").video.is_some(),
            "暂停状态不得被判停摆"
        );
        assert_eq!(status_json(&mut rt)["state"], "Paused");

        // (b) 正常产帧：即使时钟大步前进（模拟 1 fps 的合法低帧率），
        //     presented 在增长就不得判停摆。
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        attach_stub_video(&mut rt, "DP-1", true);
        let t1 = Instant::now();
        for step in 1..=5 {
            rt.tick(t1 + Duration::from_secs(step * 3));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(ch.video.is_some(), "在产帧的视频不得被判停摆");
        assert!(
            ch.last_presented >= 5,
            "presented 应在增长（实际 {}）",
            ch.last_presented
        );
        assert!(rt.state_contradictions().is_empty());
    }

    /// **2026-11-22 真机缺陷回归**：暂停超过看门狗阈值再恢复，不得把暂停时长
    /// 当成"presented 零增长"。
    ///
    /// 真机症状：`liuguangctl pause` 24.2s 后 `resume`，同一个 tick 就报
    /// `停摆：presented 已有 24.2s 零增长（阈值 10s）` → 拆管线 → 画面永久不动，
    /// 只能重启服务。根因：暂停期间 `step_channel` 早退，`last_presented_at`
    /// 停在暂停开始那一刻；恢复后第一个 tick 拿整段暂停时长去比 10s 阈值，
    /// 而此刻播放器的首帧通常还没到期（`resume_at` 把时间轴后移了一帧）。
    ///
    /// 本用例把这个窗口钉死：`resume` 后**先** tick 两次不出帧（stub 的
    /// `wait_after_resume`），旧实现第一次就判停摆；新实现必须继续出帧。
    #[test]
    fn pause_exceeding_stall_timeout_then_resume_keeps_frames() {
        let env = TestEnv::new("stall-pause-resume");
        let a = env.png("a.png", 16, 16, [0, 255, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        // 恢复后首帧要等 2 次 poll 才到期（真实播放器 resume 后就是这个状态）。
        attach_stub_video_with(&mut rt, "DP-1", true, 2);
        let t0 = Instant::now();
        rt.tick(t0);
        ok_json(
            &mut rt,
            Method::Pause,
            serde_json::json!({"output": "DP-1"}),
        );
        // 暂停 15s（> VIDEO_STALL_TIMEOUT = 10s）：期间照常 tick。
        for step in 1..=3 {
            rt.tick(t0 + Duration::from_secs(step * 5));
        }
        assert!(
            rt.channels.get("DP-1").expect("通道").video.is_some(),
            "暂停期间不得判停摆"
        );

        ok_json(
            &mut rt,
            Method::Resume,
            serde_json::json!({"output": "DP-1"}),
        );
        // 恢复后的第一个 tick：presented 还没增长（首帧未到期）——旧实现在这里误杀。
        rt.tick(t0 + Duration::from_secs(16));
        {
            let ch = rt.channels.get("DP-1").expect("通道");
            assert!(
                ch.video.is_some(),
                "恢复后的第一个 tick 不得判停摆：暂停时长不算 presented 零增长"
            );
            assert!(!ch.stall_reported, "不得为暂停时长上报停摆");
        }
        assert_eq!(
            status_json(&mut rt)["state"],
            "VideoPlaying",
            "恢复后必须回到 VideoPlaying（不得降级）"
        );

        // 继续 tick：等待窗口过去后必须真的出帧（画面在动，而不是"状态好看"）。
        for step in 1..=4 {
            rt.tick(t0 + Duration::from_secs(16) + Duration::from_millis(step * 20));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        let presented = ch
            .video
            .as_ref()
            .expect("视频管线必须仍在")
            .video_stats()
            .presented;
        assert!(
            presented >= 1,
            "恢复后必须继续出帧（presented={presented}）"
        );
        assert!(!ch.stall_reported, "出帧后不得残留停摆标记");
        assert!(
            rt.state_contradictions().is_empty(),
            "状态机与画面不得互相矛盾"
        );
    }

    /// t11：冷启动时**不得**向状态机投递非法切换事件。
    ///
    /// 说明：pipeline 未编译 gstreamer 时无法在单测里真正打开 mp4（`show_video`
    /// 会在建立管线前返回 UnsupportedCodec），因此这里直接钉住共享的切换守卫
    /// `drive_switch()` 在 `InitializingDisplay` 上的行为；图片路径的完整冷启动
    /// 由 `cold_start_first_display_is_legal_wiring` 覆盖，视频路径的真机证据
    /// 由 `set clip-4k60-high.mp4` 冷启动日志（0 条非法转移）覆盖。
    #[test]
    fn cold_start_switch_events_are_not_attempted() {
        let env = TestEnv::new("cold-start-switch");
        let mut rt = env.runtime(&env.toml("", 3600));
        assert_eq!(rt.state.current(), State::InitializingDisplay);
        assert!(
            !rt.drive_switch(),
            "冷启动（InitializingDisplay）不接受切换事件，必须直接走终态"
        );
        assert_eq!(
            rt.state_machine_errors(),
            0,
            "不得向状态机投递非法事件（旧实现留 WARN）"
        );
        assert_eq!(rt.state.current(), State::InitializingDisplay);
    }
    /// 提交失败注入后端：前 `fail_first` 次提交调用失败，其余委托 mock。
    ///
    /// 用于验证"提交失败必须可见"—— 三种同形故障（tiled 被 AddFB2 拒 /
    /// 丢 DRM master / 原子提交失败）在 daemon 侧都表现为 `present*` 返回 Err。
    struct FailingPresentBackend {
        inner: liuguang_display::mock::MockDisplayBackend,
        /// 由测试在"静态图已就绪"之后翻到 true，再注入视频路径的提交失败。
        fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// 另一个开关：`present_fitted` **返回 Ok**，但显示层记账里记 `failed`
        /// —— 这正是"只信返回值会漏掉"的盲区（t18 的 CommitDelta 专治它）。
        drm_fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// 第三个开关：`present_fitted` 返回**类型化**的
        /// `DisplayError::DmaBufRejected`（t32/S2：直通被拒 ≠ 提交失败）。
        dmabuf_fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ok: u64,
        failed: u64,
    }

    impl FailingPresentBackend {
        fn new(
            outputs: &[&str],
        ) -> (
            Self,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) {
            let fail = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let drm_fail = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                Self {
                    inner: liuguang_display::mock::MockDisplayBackend::with_outputs(outputs),
                    fail: fail.clone(),
                    drm_fail: drm_fail.clone(),
                    dmabuf_fail: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    ok: 0,
                    failed: 0,
                },
                fail,
                drm_fail,
            )
        }
        fn should_fail(&self) -> bool {
            self.fail.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn drm_fail_enabled(&self) -> bool {
            self.drm_fail.load(std::sync::atomic::Ordering::SeqCst)
        }
        /// DMA-BUF 拒绝注入开关（`new` 的签名保持不变，避免改动既有用例）。
        fn dmabuf_switch(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
            std::sync::Arc::clone(&self.dmabuf_fail)
        }
        fn dmabuf_fail_enabled(&self) -> bool {
            self.dmabuf_fail.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl liuguang_display::DisplayBackend for FailingPresentBackend {
        fn outputs(&self) -> Vec<liuguang_core::types::OutputInfo> {
            self.inner.outputs()
        }
        fn default_output(
            &self,
            wanted: Option<&str>,
        ) -> Result<OutputId, liuguang_display::DisplayError> {
            self.inner.default_output(wanted)
        }
        fn resolve_mode(
            &self,
            output: &OutputId,
            selection: &ModeSelection,
        ) -> Result<liuguang_core::types::DisplayMode, liuguang_display::DisplayError> {
            self.inner.resolve_mode(output, selection)
        }
        fn connector_connected(&self, name: &str) -> Result<bool, liuguang_display::DisplayError> {
            self.inner.connector_connected(name)
        }
        fn present(
            &mut self,
            output: OutputId,
            frame: DisplayFrame,
        ) -> Result<liuguang_core::types::PresentToken, liuguang_display::DisplayError> {
            // 只注入 `present_fitted`（视频路径）的失败：静态图路径走 `present`，
            // 必须保持健康，否则连 set 都进不去（会破坏测试前提）。
            self.inner.present(output, frame)
        }
        fn present_fitted(
            &mut self,
            output: OutputId,
            frame: DisplayFrame,
            src: (u64, u64, u64, u64),
            dst: (i64, i64, u64, u64),
        ) -> Result<liuguang_core::types::PresentToken, liuguang_display::DisplayError> {
            if self.should_fail() {
                self.failed += 1;
                return Err(liuguang_display::DisplayError::Failed {
                    operation: "present_fitted",
                    message: "注入的提交失败（模拟 AddFB2/EACCES）".into(),
                });
            }
            if self.dmabuf_fail_enabled() {
                // t32/S2：直通被拒——**类型化**错误，不是"提交失败"。
                return Err(liuguang_display::DisplayError::DmaBufRejected {
                    reason: "注入：显示侧不接受该 DMA-BUF（modifier/平面组合）".into(),
                });
            }
            if self.drm_fail_enabled() {
                // 返回 Ok，但显示层记账记一次 failed —— 旧实现会把它当成功。
                self.failed += 1;
                return self.inner.present_fitted(output, frame, src, dst);
            }
            self.ok += 1;
            self.inner.present_fitted(output, frame, src, dst)
        }
        fn commit_stats(&self) -> Option<liuguang_display::CommitStats> {
            Some(liuguang_display::CommitStats {
                commit_ok: self.ok,
                retried_after_busy: 0,
                legacy_fallback: 0,
                failed: self.failed,
            })
        }
        fn set_mode(
            &mut self,
            output: OutputId,
            mode: liuguang_core::types::DisplayMode,
        ) -> Result<(), liuguang_display::DisplayError> {
            self.inner.set_mode(output, mode)
        }
        fn suspend(&mut self) -> Result<(), liuguang_display::DisplayError> {
            self.inner.suspend()
        }
        fn resume(&mut self) -> Result<(), liuguang_display::DisplayError> {
            self.inner.resume()
        }
    }

    /// 测试后端：可注入"某次 tick 里 DRM master 被取回（资源已重建）"。
    ///
    /// 用来断言 t26 的两件事：① 自愈**不依赖提交失败**（本用例全程不注入任何提交
    /// 失败）；② 自愈成功后 daemon 会**重放**当前画面（否则屏幕仍是死的）。
    struct HealOnceBackend {
        inner: liuguang_display::mock::MockDisplayBackend,
        /// 剩余可触发的自愈次数（测试置 1）。
        heals_left: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// `poll_master_recovery` 被调用次数（证明"周期在叫醒后端"）。
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl HealOnceBackend {
        fn new(
            outputs: &[&str],
            heals: usize,
        ) -> (
            Self,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        ) {
            let heals_left = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(heals));
            let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    inner: liuguang_display::mock::MockDisplayBackend::with_outputs(outputs),
                    heals_left: heals_left.clone(),
                    polls: polls.clone(),
                },
                heals_left,
                polls,
            )
        }
    }

    impl liuguang_display::DisplayBackend for HealOnceBackend {
        fn outputs(&self) -> Vec<liuguang_core::types::OutputInfo> {
            self.inner.outputs()
        }
        fn default_output(
            &self,
            wanted: Option<&str>,
        ) -> Result<OutputId, liuguang_display::DisplayError> {
            self.inner.default_output(wanted)
        }
        fn resolve_mode(
            &self,
            output: &OutputId,
            selection: &ModeSelection,
        ) -> Result<liuguang_core::types::DisplayMode, liuguang_display::DisplayError> {
            self.inner.resolve_mode(output, selection)
        }
        fn connector_connected(&self, name: &str) -> Result<bool, liuguang_display::DisplayError> {
            self.inner.connector_connected(name)
        }
        fn present(
            &mut self,
            output: OutputId,
            frame: DisplayFrame,
        ) -> Result<liuguang_core::types::PresentToken, liuguang_display::DisplayError> {
            self.inner.present(output, frame)
        }
        fn present_fitted(
            &mut self,
            output: OutputId,
            frame: DisplayFrame,
            src: (u64, u64, u64, u64),
            dst: (i64, i64, u64, u64),
        ) -> Result<liuguang_core::types::PresentToken, liuguang_display::DisplayError> {
            self.inner.present_fitted(output, frame, src, dst)
        }
        fn set_mode(
            &mut self,
            output: OutputId,
            mode: liuguang_core::types::DisplayMode,
        ) -> Result<(), liuguang_display::DisplayError> {
            self.inner.set_mode(output, mode)
        }
        fn suspend(&mut self) -> Result<(), liuguang_display::DisplayError> {
            self.inner.suspend()
        }
        fn resume(&mut self) -> Result<(), liuguang_display::DisplayError> {
            self.inner.resume()
        }
        fn poll_master_recovery(&mut self) -> bool {
            use std::sync::atomic::Ordering;
            self.polls.fetch_add(1, Ordering::SeqCst);
            if self.heals_left.load(Ordering::SeqCst) > 0 {
                self.heals_left.fetch_sub(1, Ordering::SeqCst);
                return true;
            }
            false
        }
    }

    // ---- t94：master 暂时不可用 ≠ 帧坏了 -------------------------------------
    //
    // 真机缺陷（2026-10-19 00:27）：`systemctl restart` 时旧进程尚未释放 DRM
    // master ⇒ 新进程前几秒的提交全部 EACCES ⇒ 旧实现把它当普通提交失败累计，
    // **3 帧就降级成静态图**，而 master 约 10s 后自愈 —— 但自愈只重放静态，
    // 视频永远不回来，用户看到"卡住"。
    //
    // 两个分支必须分开，下面的测试就是它们的判据：
    //  - master 不可用：**不计失败、不降级**，且失败时记下"欠一次视频恢复"；
    //  - 真帧错误（master 正常）：**仍降级**，且不记恢复（不得掩盖问题）。

    /// 可切换的 **master 缺失** 后端：`down = true` 时提交返回类型化的
    /// [`liuguang_display::DisplayError::MasterUnavailable`]；`heal = true` 时
    /// 下一次 `poll_master_recovery` 报告"已取回 master"。
    struct MasterOutageBackend {
        inner: liuguang_display::mock::MockDisplayBackend,
        down: std::sync::Arc<std::sync::atomic::AtomicBool>,
        heal: std::sync::Arc<std::sync::atomic::AtomicBool>,
        status: liuguang_display::master::MasterStatus,
        /// 注入**真帧错误**（master 已确认正常时）：用于证明两条分支没有混为一谈。
        fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl MasterOutageBackend {
        fn new(
            outputs: &[&str],
            master_known: bool,
        ) -> (
            Self,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) {
            let down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let heal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let status = liuguang_display::master::MasterStatus {
                master: if master_known { Some(true) } else { None },
                acquire_result: "OK（测试后端）".into(),
                ..Default::default()
            };
            (
                Self {
                    inner: liuguang_display::mock::MockDisplayBackend::with_outputs(outputs),
                    down: down.clone(),
                    heal: heal.clone(),
                    status,
                    fail: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
                down,
                heal,
            )
        }
        fn is_down(&self) -> bool {
            self.down.load(std::sync::atomic::Ordering::SeqCst)
        }
        /// 真帧错误注入开关（与 master 无关）。
        fn fail_switch(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
            std::sync::Arc::clone(&self.fail)
        }
    }

    impl liuguang_display::DisplayBackend for MasterOutageBackend {
        fn outputs(&self) -> Vec<liuguang_core::types::OutputInfo> {
            self.inner.outputs()
        }
        fn default_output(
            &self,
            wanted: Option<&str>,
        ) -> Result<OutputId, liuguang_display::DisplayError> {
            self.inner.default_output(wanted)
        }
        fn resolve_mode(
            &self,
            output: &OutputId,
            selection: &ModeSelection,
        ) -> Result<liuguang_core::types::DisplayMode, liuguang_display::DisplayError> {
            self.inner.resolve_mode(output, selection)
        }
        fn connector_connected(&self, name: &str) -> Result<bool, liuguang_display::DisplayError> {
            self.inner.connector_connected(name)
        }
        fn present(
            &mut self,
            output: OutputId,
            frame: DisplayFrame,
        ) -> Result<liuguang_core::types::PresentToken, liuguang_display::DisplayError> {
            // 静态路径保持健康：本用例只注入视频路径（`present_fitted`）的失败。
            self.inner.present(output, frame)
        }
        fn present_fitted(
            &mut self,
            output: OutputId,
            frame: DisplayFrame,
            src: (u64, u64, u64, u64),
            dst: (i64, i64, u64, u64),
        ) -> Result<liuguang_core::types::PresentToken, liuguang_display::DisplayError> {
            if self.is_down() {
                self.status.master = Some(false);
                return Err(liuguang_display::DisplayError::MasterUnavailable {
                    detail: "drmSetMaster: Permission denied (os error 13)".into(),
                });
            }
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                // master 已确认正常 ⇒ 这是**真帧错误**（平面/格式/驱动拒绝）。
                return Err(liuguang_display::DisplayError::Failed {
                    operation: "present_fitted",
                    message: "注入的真帧错误（master 正常）".into(),
                });
            }
            let r = self.inner.present_fitted(output, frame, src, dst);
            if r.is_ok() {
                self.status.master = Some(true);
            }
            r
        }
        fn set_mode(
            &mut self,
            output: OutputId,
            mode: liuguang_core::types::DisplayMode,
        ) -> Result<(), liuguang_display::DisplayError> {
            self.inner.set_mode(output, mode)
        }
        fn suspend(&mut self) -> Result<(), liuguang_display::DisplayError> {
            self.inner.suspend()
        }
        fn resume(&mut self) -> Result<(), liuguang_display::DisplayError> {
            self.inner.resume()
        }
        fn poll_master_recovery(&mut self) -> bool {
            if self.heal.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.status.master = Some(true);
                self.down.store(false, std::sync::atomic::Ordering::SeqCst);
                return true;
            }
            false
        }
        fn master_status(&self) -> Option<&liuguang_display::master::MasterStatus> {
            Some(&self.status)
        }
    }

    /// 把通道状态推到 `VideoPlaying`（单测里没有真实解码器，故直接投递合法事件序列）。
    fn drive_to_video_playing(rt: &mut Runtime) {
        rt.drive(Event::SwitchRequested);
        rt.drive(Event::VideoStarted);
        assert_eq!(
            rt.state.current(),
            State::VideoPlaying,
            "测试前置：应处于 VideoPlaying"
        );
    }

    /// **t94 ①**：master 暂时不可用 **不得**被当成提交失败，更不得降级。
    ///
    /// 变异检验：把 `record_present` 里 master 分支去掉（两种错误混为一谈）
    /// ⇒ 3 帧后 `state` 变 `DegradedStatic`、`video` 被拆 ⇒ 本用例变红。
    #[test]
    fn master_unavailable_is_not_a_present_failure() {
        let env = TestEnv::new("t94-master-retryable");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        let (backend, down, heal) = MasterOutageBackend::new(&["DP-1"], true);
        let mut rt = Runtime::new(
            Some(Box::new(backend)),
            "master-outage",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        drive_to_video_playing(&mut rt);
        attach_stub_video(&mut rt, "DP-1", true);
        // 模拟"旧进程还占着 master"：此后每帧提交都返回 MasterUnavailable。
        down.store(true, std::sync::atomic::Ordering::SeqCst);
        let t0 = Instant::now();
        for step in 1..=6 {
            rt.tick(t0 + Duration::from_millis(step * 20));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        assert_eq!(
            ch.present_failures, 0,
            "master 不可用**不得**计入提交失败（否则 3 帧就降级）"
        );
        assert_eq!(ch.present_failures_consecutive, 0, "连续失败计数必须保持 0");
        assert!(
            ch.master_unavailable_skips >= 3,
            "必须单独记账（实际 {}）",
            ch.master_unavailable_skips
        );
        assert!(
            ch.video.is_some(),
            "master 不可用时**不得拆掉视频源**（拆了就得等自愈重建）"
        );
        assert_eq!(
            status_json(&mut rt)["state"],
            "VideoPlaying",
            "环境暂时不可用不得降级"
        );
        assert_eq!(rt.state_machine_errors(), 0);

        // master 取回：下一帧应立刻恢复提交，且状态仍是 VideoPlaying。
        let ok_before = rt.channels.get("DP-1").expect("通道").presented_ok;
        // 内核重新授予 master：提交立刻恢复（自愈轮询另有节流，不依赖它）。
        down.store(false, std::sync::atomic::Ordering::SeqCst);
        heal.store(true, std::sync::atomic::Ordering::SeqCst);
        rt.tick(t0 + Duration::from_millis(200));
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(
            ch.presented_ok > ok_before,
            "master 取回后必须恢复提交（presented_ok {} -> {}）",
            ok_before,
            ch.presented_ok
        );
        assert_eq!(status_json(&mut rt)["state"], "VideoPlaying");
        ok_json(&mut rt, Method::Status, serde_json::json!({}));
        assert!(rt.state_contradictions().is_empty());
    }

    /// **t94 ②**：真帧错误（master 正常）**仍必须降级**，且**不得**记"欠恢复"。
    ///
    /// 变异检验：若把 master 不可用与真失败混为一谈（都当可重试）⇒ 本用例变红；
    /// 若把"降级时记 pending"无条件执行 ⇒ `video_resume_pending` 断言变红
    /// （那样自愈会把坏帧的视频反复重开、掩盖问题）。
    #[test]
    fn real_frame_error_still_degrades_and_does_not_arm_resume() {
        let env = TestEnv::new("t94-real-error");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        // **master 已确认正常**（`Some(true)`）：这才是"真帧错误"的分支。
        let (backend, down, _heal) = MasterOutageBackend::new(&["DP-1"], true);
        assert!(!down.load(std::sync::atomic::Ordering::SeqCst));
        // 开关要在把后端装箱**之前**取（装箱后拿不回来）。
        let fail_sw = backend.fail_switch();
        let mut rt = Runtime::new(
            Some(Box::new(backend)),
            "real-error",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        let events = rt.events.subscribe();
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        drive_to_video_playing(&mut rt);
        attach_stub_video(&mut rt, "DP-1", true);
        fail_sw.store(true, std::sync::atomic::Ordering::SeqCst);
        let t0 = Instant::now();
        for step in 1..=3 {
            rt.tick(t0 + Duration::from_millis(step * 20));
        }
        let state = status_json(&mut rt)["state"].clone();
        let ch = rt.channels.get("DP-1").expect("通道");
        assert_eq!(ch.present_failures, 3, "真失败必须如实计数");
        assert!(ch.video.is_none(), "真失败必须收敛（拆掉视频源）");
        assert_eq!(
            state, "DegradedStatic",
            "真帧错误仍必须降级（不得被 master 逻辑掩盖）"
        );
        assert!(
            ch.video_resume_pending.is_none(),
            "master 已确认正常 ⇒ 真失败**不得**记欠恢复（否则自愈会把坏帧反复重开）"
        );
        let mut found = false;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == liuguang_ipc::protocol::EventKind::Error
                && ev.message.unwrap_or_default().contains("视频已停止")
            {
                found = true;
            }
        }
        assert!(found, "真失败必须发可操作的 Error 事件（而不是静默）");
        assert_eq!(rt.state_machine_errors(), 0, "不得投递非法状态转移");
    }

    /// **t94 ③**：master 缺失期间发生的视频失败要**记下媒体路径**，自愈后必须
    /// **真的去重建**；而 master 正常时的同类调用不得记。
    #[test]
    fn master_heal_rebuilds_video_supply_only_for_master_cause() {
        let env = TestEnv::new("t94-heal-resume");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let vid = env.dir.join("t94-fake.mp4");
        std::fs::write(&vid, b"not-a-real-video").expect("写 mp4");
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        let (backend, down, heal) = MasterOutageBackend::new(&["DP-1"], true);
        let mut rt = Runtime::new(
            Some(Box::new(backend)),
            "heal-resume",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        // 装一个"当前在放视频"的通道事实（单测里无法真的解码 mp4）。
        {
            let ch = rt.channels.get_mut("DP-1").expect("通道");
            let frame = ch.current.as_ref().expect("静态图已上屏").frame.clone();
            ch.current = Some(CurrentDisplay {
                media: vid.clone(),
                frame,
                mode_str: "1920x1080@60.000".into(),
                decoder: "video(vaapi):vah264dec(dmabuf)".into(),
            });
        }
        // ① master 正常 ⇒ 失败**不得**记待恢复。
        rt.handle_video_failure("DP-1", "真实帧错误（master 正常）");
        assert!(
            rt.channels
                .get("DP-1")
                .expect("通道")
                .video_resume_pending
                .is_none(),
            "master 正常时不得记欠恢复"
        );
        // ② master 丢失 ⇒ 失败必须记下媒体路径，等自愈重建。
        down.store(true, std::sync::atomic::Ordering::SeqCst);
        // 先让它真的提交一次：后端据此把 master 标记为"已确定不是"
        // （判据来自提交 EACCES，与真机同源），再模拟随之而来的视频失败。
        let probe_frame = rt
            .channels
            .get("DP-1")
            .expect("通道")
            .current
            .as_ref()
            .expect("静态图")
            .frame
            .clone();
        assert!(
            rt.present_frame("DP-1", probe_frame).is_err(),
            "master 缺失时提交必须失败"
        );
        assert!(
            rt.backend
                .master_status()
                .is_some_and(|m| m.is_known_non_master()),
            "提交 EACCES 后必须标记为非 master"
        );
        rt.handle_video_failure("DP-1", "master 丢失期间的提交失败");
        let pending = rt
            .channels
            .get("DP-1")
            .expect("通道")
            .video_resume_pending
            .clone();
        assert_eq!(
            pending.as_deref(),
            Some(vid.as_path()),
            "master 缺失导致的失败必须记下待恢复媒体"
        );
        // ③ master 自愈 ⇒ 必须**真的尝试重建**（计数器可观测）。
        //    单测里文件是假的 ⇒ 重建失败、保留待恢复标记（诚实：不谎称已恢复）。
        heal.store(true, std::sync::atomic::Ordering::SeqCst);
        let t0 = Instant::now();
        // 自愈轮询有节流：必须跨过一个 MASTER_POLL_INTERVAL 才会真的问后端。
        rt.tick(t0 + MASTER_POLL_INTERVAL + Duration::from_millis(50));
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(
            ch.video_resume_attempts >= 1,
            "自愈后必须真的尝试重建视频（旧实现只重放静态，视频永不回来）"
        );
        assert!(
            ch.video_resume_pending.is_some(),
            "重建失败时必须保留待恢复标记（下次自愈再试），不得谎称已恢复"
        );
        assert_eq!(rt.state_machine_errors(), 0, "不得投递非法状态转移");
    }

    /// **t26 核心**：`DegradedStatic`（全程不提交任何失败）时，周期自愈必须真的在跑，
    /// 且取回 master 后必须**重放当前画面**。
    #[test]
    fn master_self_heal_runs_without_any_submission_failure() {
        let env = TestEnv::new("t26-master-heal");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        let (backend, _heals, polls) = HealOnceBackend::new(&["DP-1"], 1);
        let mut rt = Runtime::new(
            Some(Box::new(backend)),
            "heal-once",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        let events = rt.events.subscribe();
        // 先放一张静态图（此后**不再有任何提交失败**，也没有视频源）。
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        let presented_before = rt.channels.get("DP-1").expect("通道").presented_ok;
        // 模拟"占用者退出、master 被取回"：只让时间前进，不注入任何失败。
        for step in 1..=6 {
            rt.tick(Instant::now() + Duration::from_millis(step * 1_000));
        }
        assert!(
            polls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "自愈必须由 tick **周期**驱动（旧实现：DegradedStatic 时 0 次尝试）"
        );
        let ch = rt.channels.get("DP-1").expect("通道");
        assert_eq!(
            ch.presented_ok,
            presented_before + 1,
            "自愈成功后必须**重放**当前画面一次（资源已重建，不重放屏幕是死的）"
        );
        assert_eq!(rt.master_heal_repaints, 1);
        // 事件：让 UI 能知道"master 已恢复"。
        let events = drain_events(&events);
        assert!(
            events.iter().any(|e| e
                .message
                .as_deref()
                .unwrap_or("")
                .contains("DRM master 已恢复")),
            "必须发出可观测的恢复事件: {:?}",
            events
                .iter()
                .filter_map(|e| e.message.clone())
                .collect::<Vec<_>>()
        );
    }

    /// 没有未完成的 master 问题时，自愈不得重复重放（已持有 ⇒ 后端返回 false）。
    #[test]
    fn no_self_heal_no_repaint() {
        let env = TestEnv::new("t26-no-heal");
        let a = env.png("a.png", 16, 16, [0, 0, 255, 255]);
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        let (backend, _heals, _polls) = HealOnceBackend::new(&["DP-1"], 0);
        let mut rt = Runtime::new(
            Some(Box::new(backend)),
            "heal-none",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        let before = rt.channels.get("DP-1").expect("通道").presented_ok;
        for step in 1..=6 {
            rt.tick(Instant::now() + Duration::from_millis(step * 1_000));
        }
        assert_eq!(
            rt.channels.get("DP-1").expect("通道").presented_ok,
            before,
            "后端没报告自愈时不得凭空重放（避免每 2s 白刷一帧）"
        );
        assert_eq!(rt.master_heal_repaints, 0);
    }

    /// ① 提交失败必须被**计数 + 告警**，且连续 K 次后收敛（与停摆看门狗同一出口）。
    /// ② 证明 `presented`（出队）与 `presented_ok`（提交成功）可区分：
    ///    本用例里 `presented` 照涨而 `presented_ok` 恒 0 —— 这正是"屏幕黑的却看着像在播"。
    #[test]
    fn present_failure_is_counted_and_converges_to_degraded() {
        let env = TestEnv::new("present-failure");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        let (backend, fail_switch, _drm_switch) = FailingPresentBackend::new(&["DP-1"]);
        let mut rt = Runtime::new(
            Some(Box::new(backend)),
            "failing",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        let events = rt.events.subscribe();
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        // 静态图已就绪；现在打开"提交失败"开关并挂一个永远有帧的视频源：
        // 此后每次 tick 都会尝试提交一帧并失败（模拟 AddFB2 被拒 / EACCES）。
        fail_switch.store(true, std::sync::atomic::Ordering::SeqCst);
        attach_stub_video(&mut rt, "DP-1", true);
        let t0 = Instant::now();
        for step in 1..=3 {
            rt.tick(t0 + Duration::from_millis(step * 20));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        assert_eq!(
            ch.present_failures, 3,
            "三次提交失败必须被计数（旧实现是 `let _ =` 静默丢弃）"
        );
        assert_eq!(ch.presented_ok, 0, "一次都没成功提交");
        assert!(
            ch.video.is_none(),
            "连续提交失败达到阈值后必须收敛（拆掉视频源）"
        );
        let st = status_json(&mut rt);
        assert_eq!(
            st["state"], "DegradedStatic",
            "提交失败必须如实降级（而不是继续自称在播）"
        );
        // 事件里要能看到"连续 N 次提交失败"的原因。
        let mut found = false;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == liuguang_ipc::protocol::EventKind::Error
                && ev.message.unwrap_or_default().contains("扫描输出提交失败")
            {
                found = true;
            }
        }
        assert!(found, "必须发出带原因的 Error 事件");
        assert!(rt.state_contradictions().is_empty());
    }

    /// 正面控制：健康后端下 `presented_ok` 必须增长、失败计数恒 0
    /// （确保上面的测试不是因为"计数器永远不动"而通过）。
    #[test]
    fn healthy_present_keeps_counters_clean() {
        let env = TestEnv::new("present-ok");
        let a = env.png("a.png", 16, 16, [0, 255, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        attach_stub_video(&mut rt, "DP-1", true);
        let t0 = Instant::now();
        for step in 1..=3 {
            rt.tick(t0 + Duration::from_millis(step * 20));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(ch.presented_ok >= 3, "健康提交必须计入 presented_ok");
        assert_eq!(ch.present_failures, 0);
        assert!(ch.video.is_some(), "健康视频不得被拆");
    }
    /// t13-A3：**`present_*` 返回 Ok 但显示层记账报告"该帧未提交"**必须被抓到
    /// （旧实现只看返回值 ⇒ 这种"presented 在涨、屏幕不动"的组合完全隐形）。
    #[test]
    fn drm_level_commit_failure_is_caught_even_when_present_returns_ok() {
        let env = TestEnv::new("drm-commit-blind");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        let (backend, _fail_switch, drm_switch) = FailingPresentBackend::new(&["DP-1"]);
        let mut rt = Runtime::new(
            Some(Box::new(backend)),
            "drm-failing",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        // 打开"返回 Ok 但记账失败"开关 + 挂永远有帧的视频源。
        drm_switch.store(true, std::sync::atomic::Ordering::SeqCst);
        attach_stub_video(&mut rt, "DP-1", true);
        let t0 = Instant::now();
        for step in 1..=3 {
            rt.tick(t0 + Duration::from_millis(step * 20));
        }
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(
            ch.drm_commit_failed >= 3,
            "显示层记账的 failed 必须被计入（实际 {}）",
            ch.drm_commit_failed
        );
        assert_eq!(
            ch.present_failures, 0,
            "本用例里 present_* 都返回 Ok —— 正是旧实现的盲区"
        );
        assert!(ch.video.is_none(), "连续 3 次记账失败后必须收敛");
        assert_eq!(status_json(&mut rt)["state"], "DegradedStatic");
    }

    // ================= t32 / S5：显示层对账告警的**有界性** =================
    //
    // bug 本体（旧实现）：`note_drm_commit` 只在 `failed > 0` 时被调用，而它内部
    // 用**常量** `1` 调 `contradicts_presented` ⇒ `contradict` 恒真 ⇒
    // `total <= BUDGET || contradict` 的预算被整条旁路 ⇒ 失败风暴里**每失败帧
    // 一条 WARN**。下面四个用例分别锁住：episode 预算生效、干净提交后预算复位、
    // 零成功通道只升级一次、长期交替也有硬上限。

    /// 只统计 WARN 的探针层。
    ///
    /// 用 `tracing::subscriber::set_default`（**thread-local**）安装，所以并行跑的
    /// 其它测试不会看到这些事件，本测试也不会被它们污染。只数 `message` 含
    /// `needle` 的 WARN，并统计其中 `systematic = true` 的条数。
    struct WarnProbe {
        warns: std::sync::Arc<AtomicUsize>,
        systematic: std::sync::Arc<AtomicUsize>,
    }

    impl WarnProbe {
        fn install(needle: &'static str) -> (Self, tracing::subscriber::DefaultGuard) {
            use tracing_subscriber::layer::SubscriberExt as _;
            let warns = std::sync::Arc::new(AtomicUsize::new(0));
            let systematic = std::sync::Arc::new(AtomicUsize::new(0));
            let layer = CountingWarnLayer {
                needle,
                warns: std::sync::Arc::clone(&warns),
                systematic: std::sync::Arc::clone(&systematic),
            };
            let guard =
                tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
            (Self { warns, systematic }, guard)
        }

        fn warns(&self) -> usize {
            self.warns.load(Ordering::SeqCst)
        }

        fn systematic(&self) -> usize {
            self.systematic.load(Ordering::SeqCst)
        }
    }

    struct CountingWarnLayer {
        needle: &'static str,
        warns: std::sync::Arc<AtomicUsize>,
        systematic: std::sync::Arc<AtomicUsize>,
    }

    impl<S> tracing_subscriber::Layer<S> for CountingWarnLayer
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::WARN {
                return;
            }
            let mut scan = FieldScan::default();
            event.record(&mut scan);
            if !scan
                .message
                .as_deref()
                .is_some_and(|m| m.contains(self.needle))
            {
                return;
            }
            self.warns.fetch_add(1, Ordering::SeqCst);
            if scan.systematic == Some(true) {
                self.systematic.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[derive(Default)]
    struct FieldScan {
        message: Option<String>,
        systematic: Option<bool>,
    }

    impl tracing::field::Visit for FieldScan {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = Some(format!("{value:?}"));
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = Some(value.to_string());
            }
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            if field.name() == "systematic" {
                self.systematic = Some(value);
            }
        }
    }

    /// 造一个只关心 `commit_ok`/`failed` 的 `CommitDelta`。
    fn drm_delta(commit_ok: u64, failed: u64) -> liuguang_display::CommitDelta {
        liuguang_display::CommitDelta {
            commit_ok,
            retried_after_busy: 0,
            legacy_fallback: 0,
            failed,
        }
    }

    /// t32/S4：`视频推进` 的日志节拍**不得**退化成每帧一条。
    ///
    /// 旧判据 `dropped_late % 60 == 1` 在 `dropped_late` 恒为 1 时**永远成立**
    /// （真机实测：DMA-BUF 路径 30s 写 1246 行 = 41.5 行/秒；同一素材走 NV12
    /// 只有 38 行 —— 差别纯粹来自这个恒真分支）。
    #[test]
    fn video_progress_log_is_not_per_frame_when_dropped_late_is_stuck_at_one() {
        let mut logs = 0u32;
        for presented in 1..=600u64 {
            if should_log_video_progress(presented, 1, 1) {
                logs += 1;
            }
        }
        assert_eq!(logs, 5, "600 帧只允许 5 条（每 120 帧一条）——不得每帧一条");
        // 正面：`presented` 的周期性打点确实在工作（否则上面的 5 就是"永远不打"）。
        assert!(should_log_video_progress(121, 1, 1));
        // 掉帧**新跨越** 60 的整数倍要额外打点（这个能力不能丢）。
        assert!(!should_log_video_progress(200, 59, 59));
        assert!(should_log_video_progress(200, 61, 59), "跨越 60 必须打点");
        assert!(should_log_video_progress(200, 121, 61), "跨越 120 必须打点");
        // 从 1 起跳不算"新跨越"。
        assert!(!should_log_video_progress(200, 1, 1));
    }

    /// 建一个只有空通道的 Runtime（不显示任何媒体：`presented_ok` 保持 0）。
    fn bare_runtime(name: &str) -> (TestEnv, Runtime) {
        let env = TestEnv::new(name);
        let _ = env.png("a.png", 8, 8, [0, 0, 0, 255]);
        let rt = env.runtime(&env.toml("", 3600));
        (env, rt)
    }

    /// **episode 预算必须真的限速**：10 次失败只允许 [`PRESENT_FAILURE_LOUD_BUDGET`] 条
    /// WARN（旧实现因为 `contradict` 恒真会打 10 条）。
    ///
    /// 变异检查：把 `anomaly` 放回 `if within_budget || escalated || contradict`
    /// ⇒ 本用例得到 10 条 ⇒ 红。
    #[test]
    fn drm_failure_loudness_is_bounded() {
        let (_env, mut rt) = bare_runtime("drm-loud-budget");
        let (probe, _guard) = WarnProbe::install("显示层记账报告该帧");
        let bad = drm_delta(0, 1);
        for _ in 0..10 {
            let _ = rt.note_drm_commit("DP-1", &bad, 1, "测试帧");
        }
        assert_eq!(
            probe.warns(),
            PRESENT_FAILURE_LOUD_BUDGET as usize,
            "失败风暴里 WARN 必须被 episode 预算限速（旧实现=每帧一条=10）"
        );
    }

    /// 一次**干净提交**（`failed == 0`）必须把 episode 预算复位：否则一次历史
    /// 风暴会让该通道**永久静音**，之后真正的系统性故障再也看不见。
    ///
    /// 变异检查：删掉 `ch.drm_loud_used = 0` ⇒ 第二段 3 次失败一条也不打 ⇒ 得 3 而非 6 ⇒ 红。
    #[test]
    fn drm_failure_loud_budget_resets_after_clean_commit() {
        let (_env, mut rt) = bare_runtime("drm-loud-reset");
        let (probe, _guard) = WarnProbe::install("显示层记账报告该帧");
        let bad = drm_delta(0, 1);
        for _ in 0..5 {
            let _ = rt.note_drm_commit("DP-1", &bad, 1, "测试帧");
        }
        assert_eq!(probe.warns(), PRESENT_FAILURE_LOUD_BUDGET as usize);
        // 干净提交：不告警，且必须复位 episode 预算。
        assert!(
            rt.note_drm_commit("DP-1", &drm_delta(1, 0), 1, "测试帧")
                .is_none(),
            "干净提交不产生告警"
        );
        assert_eq!(probe.warns(), PRESENT_FAILURE_LOUD_BUDGET as usize);
        for _ in 0..3 {
            let _ = rt.note_drm_commit("DP-1", &bad, 1, "测试帧");
        }
        assert_eq!(
            probe.warns(),
            (PRESENT_FAILURE_LOUD_BUDGET * 2) as usize,
            "干净提交后预算必须复位（未复位会停在 3）"
        );
    }

    /// "该通道至今没有任何一次成功提交" ⇒ **一次性**升级告警，且**不得**突破预算：
    /// 10 次失败仍是 3 条 WARN，其中恰好 1 条带 `systematic=true`；
    /// 干净提交复位预算后再来 10 次失败，`systematic` 仍是 1（整生命周期只发一次）。
    ///
    /// 变异检查：删掉 `drm_zero_ok_warned` 一次性开关 ⇒ 预算耗尽后仍靠 `escalated`
    /// 继续打 ⇒ 10 条 ⇒ 红。
    #[test]
    fn drm_zero_ok_escalates_exactly_once() {
        let (_env, mut rt) = bare_runtime("drm-zero-ok");
        let (probe, _guard) = WarnProbe::install("显示层记账报告该帧");
        let bad = drm_delta(0, 1);
        for _ in 0..10 {
            let _ = rt.note_drm_commit("DP-1", &bad, 1, "测试帧");
        }
        assert_eq!(
            probe.warns(),
            PRESENT_FAILURE_LOUD_BUDGET as usize,
            "系统性失败不得突破 episode 预算"
        );
        assert_eq!(probe.systematic(), 1, "零成功通道的升级告警只发一次");
        // 复位预算后再来一轮：升级告警**不得**重发。
        let _ = rt.note_drm_commit("DP-1", &drm_delta(1, 0), 1, "测试帧");
        for _ in 0..10 {
            let _ = rt.note_drm_commit("DP-1", &bad, 1, "测试帧");
        }
        assert_eq!(
            probe.warns(),
            (PRESENT_FAILURE_LOUD_BUDGET * 2) as usize,
            "第二 episode 仍按 3 条限速"
        );
        assert_eq!(
            probe.systematic(),
            1,
            "升级告警是整通道生命周期一次（不是每 episode 一次）"
        );
    }

    /// 长期"失败 → 干净 → 失败"交替会**反复重置** episode 预算，所以必须有
    /// 生命周期硬上限：100 次交替失败只允许 [`PRESENT_FAILURE_LOUD_TOTAL_BUDGET`] 条。
    ///
    /// 变异检查：去掉 `drm_loud_total` 那道闸 ⇒ 100 条 ⇒ 红。
    #[test]
    fn drm_failure_loudness_has_lifetime_cap() {
        let (_env, mut rt) = bare_runtime("drm-loud-cap");
        let (probe, _guard) = WarnProbe::install("显示层记账报告该帧");
        let bad = drm_delta(0, 1);
        let clean = drm_delta(1, 0);
        for _ in 0..100 {
            let _ = rt.note_drm_commit("DP-1", &bad, 1, "测试帧");
            let _ = rt.note_drm_commit("DP-1", &clean, 1, "测试帧");
        }
        assert_eq!(
            probe.warns(),
            PRESENT_FAILURE_LOUD_TOTAL_BUDGET as usize,
            "生命周期硬上限必须兜住（去掉它会得到 100 条）"
        );
        let ch = rt.channels.get("DP-1").expect("通道");
        assert_eq!(
            ch.drm_commit_failed, 100,
            "限速只影响日志，**记账一笔都不能丢**"
        );
    }

    // ================= t32 / S2：DMA-BUF 被拒 ≠ 视频失败 =================
    //
    // 旧行为：直通被拒走的是 `record_present` 的失败路径 ⇒ 连续 3 帧就
    // `handle_video_failure` ⇒ 视频被拆成静态图（用户看到"视频莫名其妙变静止"）。
    // 正确行为：**类型化**判据（`DisplayError::DmaBufRejected`）+ 闩死直通 +
    // 重开 CPU 生产者，视频继续播。下面三个用例锁住映射、闩死语义与日志有界性。

    /// 提交一个 8×8 的合成 CPU 帧（测试用最小帧）。
    /// **t55 补：帧探针的可执行入口必须真的落盘**（此前 `frame_probe` 只有单测、
    /// 模块外零调用 ⇒ "实现了"但"用不了"）。
    ///
    /// 判据：给通道装上启用的探针后提交一帧 ⇒ 目录里出现 `<pts>.crc`，
    /// 且内容为 `pts crc top bot`（与 `scripts/t52-frame-probe-runbook.sh` 一致）。
    ///
    /// **变异**：把 `record` 调用去掉（或不落盘）⇒ 本用例必须变红。
    #[test]
    fn frame_probe_env_writes_crc_files_from_the_present_path() {
        let env = TestEnv::new("t55-probe");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));

        // 造一个启用探针的通道（等价于设了 LIUGUANG_FRAME_PROBE_DIR）
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "liuguang-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let dir_str = dir.to_str().expect("utf8").to_string();
        if let Some(ch) = rt.channels.get_mut("DP-1") {
            ch.frame_probe = liuguang_display::frame_probe::FrameProbe::from_dir(Some(&dir_str));
            assert!(ch.frame_probe.enabled(), "设了目录必须启用");
        } else {
            panic!("应有 DP-1 通道");
        }

        // 走真实提交路径
        let src = (0, 0, u64::from(8u32) << 16, u64::from(8u32) << 16);
        let dst = (0, 0, 8, 8);
        rt.present_frame_fitted("DP-1", DisplayFrame::Cpu(tiny_cpu_frame()), src, dst)
            .expect("提交应成功");

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("目录应存在")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        assert!(
            !entries.is_empty(),
            "设了 LIUGUANG_FRAME_PROBE_DIR ⇒ 提交一帧后必须出现 .crc 文件（当前目录 {dir_str} 为空）"
        );
        let text = std::fs::read_to_string(&entries[0]).expect("可读");
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 4, "格式必须是 `pts crc top bot`：{text:?}");
        assert!(
            fields.iter().all(|f| f.parse::<u64>().is_ok()),
            "四个字段都必须是数字：{text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tiny_cpu_frame() -> CpuFrame {
        CpuFrame {
            width: 8,
            height: 8,
            stride: 32,
            format: liuguang_core::types::PixelFormat::Xrgb8888,
            data: std::sync::Arc::from(vec![0u8; 8 * 8 * 4].into_boxed_slice()),
        }
    }

    /// 建一个带 `FailingPresentBackend` 的 Runtime，并返回（普通失败开关, DMA-BUF 拒绝开关）。
    fn runtime_with_injectable_backend(
        env: &TestEnv,
        name: &str,
    ) -> (
        Runtime,
        std::sync::Arc<AtomicBool>,
        std::sync::Arc<AtomicBool>,
    ) {
        let cfg_path = env.dir.join("config.toml");
        std::fs::write(&cfg_path, env.toml("", 3600)).expect("写配置");
        let config = Config::load(&cfg_path).expect("配置合法");
        let (backend, fail, _drm) = FailingPresentBackend::new(&["DP-1"]);
        let dmabuf = backend.dmabuf_switch();
        let rt = Runtime::new(
            Some(Box::new(backend)),
            name,
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("建 Runtime");
        (rt, fail, dmabuf)
    }

    /// **P1-1 回归**：生产者类别由**后端能力**决定，不由环境变量决定。
    /// 遗留 `LIUGUANG_VIDEO_DMABUF=1` 的 unit 升到"直通已移除"的后端上，
    /// 必须仍开 CPU 捕获管线；变异（把 `backend_dmabuf` 从判据里去掉）⇒ 本用例变红。
    ///
    /// 判据函数本身由 `gstreamer` feature 门控（`vaapi` 蕴含 `gstreamer`），
    /// 因此默认 feature 构型下不编译本用例；`--features vaapi` 构型必须继续跑。
    #[cfg(feature = "gstreamer")]
    #[test]
    fn producer_kind_follows_backend_capability_not_env() {
        use super::{ProducerKind, producer_kind};
        // 后端支持直通 + 未闩死 + 不需像素变换 ⇒ 才允许按环境尝试直通。
        assert_eq!(producer_kind(false, true, false), ProducerKind::Auto);
        // 后端**不**支持直通（当前真实 DRM 后端）：即使 force_cpu=false 也必须 CPU。
        assert_eq!(
            producer_kind(false, false, false),
            ProducerKind::Cpu,
            "旧 unit 遗留 LIUGUANG_VIDEO_DMABUF=1 不得让生产者按直通协商"
        );
        // 通道已闩死：无论后端能力如何都是 CPU。
        assert_eq!(
            producer_kind(true, true, false),
            ProducerKind::Cpu,
            "被拒后闩死的既有语义必须保留"
        );
        // 旋转/亮度需要像素级处理：哪怕已闩死直通也必须走 RGBA，防止 NV12 送进像素变换崩溃。
        assert_eq!(producer_kind(false, true, true), ProducerKind::Rgba);
        assert_eq!(producer_kind(true, false, true), ProducerKind::Rgba);
    }

    /// **类型化映射 + 负对照**：只有 `DmaBufRejected` 才带 `DMABUF_REJECTED`
    /// 机器码；普通提交失败**不得冒领**它（否则 daemon 会把真失败误判成"换个
    /// 生产者就好"的直通问题，反过来把视频降级成静态）。
    #[test]
    fn dmabuf_reject_error_carries_typed_detail_code() {
        let env = TestEnv::new("dmabuf-code");
        let _ = env.png("a.png", 8, 8, [1, 2, 3, 255]);
        let (mut rt, fail, dmabuf) = runtime_with_injectable_backend(&env, "drm-dmabuf-code");
        let src = (0, 0, 8u64 << 16, 8u64 << 16);
        let dst = (0i64, 0i64, 8u64, 8u64);
        // 负对照：普通失败 → 没有 detail_code。
        fail.store(true, Ordering::SeqCst);
        let e = rt
            .present_frame_fitted("DP-1", DisplayFrame::Cpu(tiny_cpu_frame()), src, dst)
            .expect_err("普通提交失败");
        assert_eq!(e.detail_code, None, "普通提交失败不得冒领 DMABUF_REJECTED");
        fail.store(false, Ordering::SeqCst);
        // 正例：类型化的 DmaBufRejected → 必须带机器码，且消息里看得出是直通。
        dmabuf.store(true, Ordering::SeqCst);
        let e = rt
            .present_frame_fitted("DP-1", DisplayFrame::Cpu(tiny_cpu_frame()), src, dst)
            .expect_err("DMA-BUF 被拒");
        assert_eq!(
            e.detail_code.as_deref(),
            Some(DMABUF_REJECTED_CODE),
            "DmaBufRejected 必须映射成稳定机器码"
        );
        assert!(
            e.message.contains("DMA-BUF"),
            "错误消息要能让人看出是直通被拒，实际: {}",
            e.message
        );
    }

    /// 视频帧遇到 `DmaBufRejected` 时：**闩死直通**、单独计数、**不得**记成
    /// 提交失败（记成失败就会连吃 3 帧触发降级 —— 这正是要消灭的旧行为）。
    #[test]
    fn dmabuf_rejection_latches_without_counting_as_present_failure() {
        let env = TestEnv::new("dmabuf-latch");
        let a = env.png("a.png", 8, 8, [9, 9, 9, 255]);
        let (mut rt, _fail, dmabuf) = runtime_with_injectable_backend(&env, "drm-dmabuf-latch");
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        attach_stub_video(&mut rt, "DP-1", true);
        dmabuf.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        rt.tick(t0 + Duration::from_millis(20));
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(ch.dmabuf_latched, "被拒后必须闩死直通");
        assert_eq!(ch.dmabuf_rejections, 1, "被拒次数单独记账");
        assert_eq!(
            ch.present_failures, 0,
            "直通被拒**不是**提交失败（记成失败会连吃 3 帧就降级）"
        );
        assert_eq!(ch.drm_commit_failed, 0, "更不该记进显示层失败台账");
        assert!(
            ch.dmabuf_latched,
            "闩死是通道级状态：换下一个视频也不该再试直通"
        );
    }

    /// 闩死之后的重复被拒**不得**刷屏：5 次只留 1 条 WARN（首条），其余降级。
    #[test]
    fn dmabuf_rejection_logging_is_bounded() {
        let env = TestEnv::new("dmabuf-loud");
        let a = env.png("a.png", 8, 8, [7, 7, 7, 255]);
        let (mut rt, _fail, _dmabuf) = runtime_with_injectable_backend(&env, "drm-dmabuf-loud");
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        );
        attach_stub_video(&mut rt, "DP-1", true);
        let (probe, _guard) = WarnProbe::install("显示侧**拒绝 DMA-BUF 直通**");
        for _ in 0..5 {
            let _ = rt.latch_dmabuf_rejection(
                "DP-1",
                "注入的直通拒绝",
                Some(Instant::now() + Duration::from_millis(20)),
            );
        }
        assert_eq!(probe.warns(), 1, "重复被拒只留首条 WARN（其余降 debug）");
        let ch = rt.channels.get("DP-1").expect("通道");
        assert_eq!(ch.dmabuf_rejections, 5, "但**计数一次都不能少**");
    }

    /// t13-A4：启动后**第一次 tick** 就显示第一个媒体项，不等 `interval_seconds`    /// （旧行为只在"轮换到期"时显示首张，默认 600s ⇒ 开机 10 分钟没有壁纸）。
    #[test]
    fn first_tick_shows_first_item_without_waiting_interval() {
        let env = TestEnv::new("prime-first");
        let _a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let _b = env.png("b.png", 16, 16, [0, 255, 0, 255]);
        // interval 设成 1 小时：首帧若依赖 interval 就永远看不到。
        let mut rt = env.runtime(&env.toml("", 3600));
        age_startup(&mut rt);
        assert_eq!(current_media(&rt, "DP-1"), "<none>", "构造后还没有内容");
        rt.tick(Instant::now());
        let media = current_media(&rt, "DP-1");
        assert!(
            media.ends_with("a.png") || media.ends_with("b.png"),
            "第一次 tick 必须点亮首帧，实际 {media}"
        );
        // 幂等 + 不在 interval 之前轮换。
        rt.tick(Instant::now() + Duration::from_secs(1));
        assert_eq!(
            current_media(&rt, "DP-1"),
            media,
            "interval 未到不得轮换（首帧补偿不应变成每秒换图）"
        );
        assert!(rt.state_contradictions().is_empty());
    }

    // ================= t22：媒体库 IPC（异步扫描 / 路径边界 / 事件） =================

    /// 带 `[library]` 的配置：root = 临时媒体目录，来源 = `images/` + `videos/`。
    fn library_toml(env: &TestEnv, extra: &str) -> String {
        let dir = env.media_dir().display().to_string();
        // 旧 `[slideshow].directory` 指到一个空的 legacy 子目录：本组用例只验证
        // `[library]` 的两个来源（旧键并入/去重由专门用例覆盖）。
        let legacy = env.media_dir().join("legacy").display().to_string();
        let base = env.toml("", 3600).replace(
            &format!("directory = \"{dir}\""),
            &format!("directory = \"{legacy}\""),
        );
        format!(
            "{base}\n[library]\nroot = \"{dir}\"\nimage_dirs = [\"images\"]\nvideo_dirs = [\"videos\"]\nrecursive = true\n{extra}\n"
        )
    }

    /// 建媒体库目录骨架并放素材。
    fn seed_library(env: &TestEnv) {
        std::fs::create_dir_all(env.media_dir().join("images")).expect("建 images");
        std::fs::create_dir_all(env.media_dir().join("videos")).expect("建 videos");
        std::fs::create_dir_all(env.media_dir().join("legacy")).expect("建 legacy");
        env.png("images/a.png", 8, 8, [255, 0, 0, 255]);
        env.png("images/b.png", 8, 8, [0, 255, 0, 255]);
        std::fs::write(env.media_dir().join("images/notes.txt"), b"x").expect("写 txt");
        std::fs::write(env.media_dir().join("videos/c.mp4"), b"not-a-real-video").expect("写 mp4");
    }

    /// 等后台扫描结束（**非阻塞**轮询 + 上限），返回是否等到。
    fn wait_for_scan(rt: &mut Runtime, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            rt.poll_scan();
            if rt.media.running.is_none() && rt.media.generated_unix_ms != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn drain_events(events: &Receiver<EventMessage>) -> Vec<EventMessage> {
        let mut out = Vec::new();
        while let Ok(ev) = events.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn media_scan_is_async_and_publishes_scan_finished() {
        let env = TestEnv::new("t22-scan");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let events = rt.events.subscribe();
        // 第一次查询：**不阻塞**——要么已经扫完（快），要么如实说"正在扫"。
        let first = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10}),
        );
        assert!(
            first["scanning"].is_boolean(),
            "必须如实上报扫描状态: {first}"
        );
        assert!(
            wait_for_scan(&mut rt, Duration::from_secs(10)),
            "后台扫描必须在 10s 内结束"
        );
        let page = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10}),
        );
        assert_eq!(
            page["total"], 3,
            "2 图 + 1 视频；notes.txt 不得收录: {page}"
        );
        assert_eq!(page["scanning"], false);
        assert!(page["scan_id"].as_u64().unwrap_or(0) > 0);
        assert!(page["generated_unix_ms"].as_u64().unwrap_or(0) > 0);
        assert_eq!(page["truncated"], false);
        let mut kinds: Vec<&str> = page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["kind"].as_str().unwrap_or(""))
            .collect();
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["image", "image", "video"]);
        // 事件：SCAN_FINISHED 必须带同一个 scan_id 与 found。
        let events = drain_events(&events);
        let finished = events
            .iter()
            .find(|e| e.kind == EventKind::ScanFinished)
            .expect("必须发出 SCAN_FINISHED");
        assert_eq!(finished.found, Some(3));
        assert_eq!(finished.done, Some(true));
        assert_eq!(finished.scan_id, page["scan_id"].as_u64());
    }

    #[test]
    fn symlink_escape_is_excluded_from_media_index() {
        let env = TestEnv::new("t22-symlink");
        seed_library(&env);
        // 根外的真实图片 + 根内的符号链接。
        let outside = std::env::temp_dir().join(format!("liuguang-t22-out-{}", std::process::id()));
        std::fs::create_dir_all(&outside).expect("建根外目录");
        std::fs::write(outside.join("evil.png"), b"x").expect("写根外图片");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.join("evil.png"),
            env.media_dir().join("images/link.png"),
        )
        .expect("建符号链接");
        let mut rt = env.runtime(&library_toml(&env, ""));
        ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": true}),
        );
        assert!(wait_for_scan(&mut rt, Duration::from_secs(10)));
        let page = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 50}),
        );
        let paths: Vec<&str> = page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["path"].as_str().unwrap_or(""))
            .collect();
        assert!(
            paths.iter().all(|p| !p.contains("evil")),
            "指向根外的符号链接不得进索引: {paths:?}"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn scan_source_is_accepted_and_generations_are_monotonic() {
        let env = TestEnv::new("t22-scan-source");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let a = ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": true}),
        );
        assert_eq!(a["accepted"], true);
        let first = a["scan_id"].as_u64().expect("scan_id");
        let b = ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": false}),
        );
        assert!(
            b["scan_id"].as_u64().unwrap_or(0) > first,
            "新扫描代次必须递增（旧结果按代次丢弃）"
        );
        // 未知来源必须被拒（不得静默扫描全部）。
        let r = call(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"source_id": "/no/such/dir"}),
        );
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().map(|e| e.detail_code.clone()),
            Some(Some("NOT_FOUND".to_string()))
        );
    }

    #[test]
    fn list_media_paginates_and_filters_server_side() {
        let env = TestEnv::new("t22-paging");
        seed_library(&env);
        for i in 0..5 {
            env.png(&format!("images/x{i}.png"), 4, 4, [9, 9, 9, 255]);
        }
        let mut rt = env.runtime(&library_toml(&env, ""));
        ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": true}),
        );
        assert!(wait_for_scan(&mut rt, Duration::from_secs(10)));
        let p0 = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 3}),
        );
        let p1 = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 1, "page_size": 3}),
        );
        assert_eq!(p0["total"], 8, "7 图 + 1 视频");
        assert_eq!(p0["items"].as_array().map(|a| a.len()), Some(3));
        assert_eq!(p1["items"].as_array().map(|a| a.len()), Some(3));
        let id0: Vec<&str> = p0["items"]
            .as_array()
            .expect("p0")
            .iter()
            .map(|i| i["id"].as_str().unwrap_or(""))
            .collect();
        let id1: Vec<&str> = p1["items"]
            .as_array()
            .expect("p1")
            .iter()
            .map(|i| i["id"].as_str().unwrap_or(""))
            .collect();
        assert!(
            id0.iter().all(|a| !id1.contains(a)),
            "两页不得重复（服务端分页）"
        );
        // 过滤/搜索也必须在服务端做（TUI 不得拉全量再过滤）。
        let videos = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10, "filter": "video"}),
        );
        assert_eq!(videos["total"], 1);
        assert_eq!(videos["items"][0]["kind"], "video");
        let searched = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10, "search": "X3"}),
        );
        assert_eq!(searched["total"], 1, "搜索大小写不敏感且服务端过滤");
        // 越界页码：返回空页而不是报错/越界。
        let beyond = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 99, "page_size": 10}),
        );
        assert_eq!(beyond["items"].as_array().map(|a| a.len()), Some(0));
    }

    /// T39-F1：`filter` 必须**容错 + 校验**——
    /// 缺省 / `"all"` / `""` ⇒ 不筛选（真实条数）；**未知值 ⇒ `INVALID_PARAMS`**，
    /// 绝不允许"无效参数被当成合法的空结果"（`ok=true` + `total=0`）。
    #[test]
    fn list_media_filter_is_tolerant_and_rejects_unknown_values() {
        let env = TestEnv::new("t42-filter");
        seed_library(&env);
        for i in 0..5 {
            env.png(&format!("images/f{i}.png"), 4, 4, [7, 7, 7, 255]);
        }
        let mut rt = env.runtime(&library_toml(&env, ""));
        ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": true}),
        );
        assert!(wait_for_scan(&mut rt, Duration::from_secs(10)));

        // ① 缺省（TUI 默认「全部」现在就这么发）：真实条数，不得为 0
        let default = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10}),
        );
        assert_eq!(default["total"], 8, "默认筛选（缺省 filter）必须是真实条数");
        assert!(
            default["items"].as_array().map(|a| a.len()).unwrap_or(0) > 0,
            "默认筛选必须返回条目：{default}"
        );
        // ② 旧客户端的 "all" 与空串：容错为"不筛选"（同样返回真实条数）
        for tolerated in ["all", ""] {
            let r = ok_json(
                &mut rt,
                Method::ListMedia,
                serde_json::json!({"page": 0, "page_size": 10, "filter": tolerated}),
            );
            assert_eq!(r["total"], 8, "filter={tolerated:?} 应被容忍为不筛选");
        }
        // ③ 未知值：必须报错（而不是静默 total=0）
        let bad = call(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10, "filter": "bogus"}),
        );
        assert!(!bad.ok, "未知 filter 必须返回错误：{bad:?}");
        let err = bad.error.expect("应有 error 体");
        assert_eq!(
            err.code,
            liuguang_ipc::protocol::ErrorKind::InvalidParams,
            "未知 filter 必须是 INVALID_PARAMS"
        );
        assert_eq!(
            err.detail_code.as_deref(),
            Some("INVALID_FILTER"),
            "应给出可定位的 detail_code"
        );
    }

    /// `list_sources` 的来源 id 集合（规范化绝对路径）。
    fn source_ids(rt: &mut Runtime) -> Vec<String> {
        let v = ok_json(rt, Method::ListSources, serde_json::json!({}));
        v["sources"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["id"].as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 用**同一个配置文件**重建 Runtime（等价于"重启 daemon"）。
    fn restart(env: &TestEnv) -> Runtime {
        let cfg_path = env.dir.join("config.toml");
        let config = Config::load(&cfg_path).expect("重启后配置应仍合法");
        Runtime::new(
            Some(Box::new(
                liuguang_display::mock::MockDisplayBackend::with_outputs(&["DP-1"]),
            )),
            "mock",
            config,
            cfg_path,
            env.dir.join("state.json"),
            EventBus::default(),
        )
        .expect("重建 Runtime")
    }

    /// **T25-F1（high）端到端**：`add_source` → `save_config` → **重启** ⇒ 新来源仍在。
    ///
    /// 旧实现的 `save_config` 只写 `self.config`、**不合并运行期新增来源**：
    /// 真机上"保存确认 +1"、配置文件里却没有该来源，重启后 `list_sources` 只剩 root
    /// ——UI 看起来成功、用户的改动真的丢了。
    ///
    /// 变异检查：删掉 `merge_runtime_sources()` 调用 ⇒ 重启后断言失败（红）。
    #[test]
    fn added_source_survives_save_and_restart() {
        let env = TestEnv::new("t25f1-add-persist");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        std::fs::create_dir_all(env.media_dir().join("extra")).expect("建 extra");
        let extra = std::fs::canonicalize(env.media_dir().join("extra")).expect("canonicalize");
        let extra_id = extra.to_string_lossy().into_owned();

        let added = ok_json(
            &mut rt,
            Method::AddSource,
            serde_json::json!({"kind": "image", "path": extra.display().to_string()}),
        );
        assert_eq!(added["created"], true);
        assert!(source_ids(&mut rt).contains(&extra_id), "运行时先要能看到");

        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["saved"], true);
        assert_eq!(
            saved["verified"], true,
            "回读校验必须通过：来源集合与运行时一致"
        );
        assert!(
            saved["merged_sources"].as_u64().unwrap_or(0) >= 1,
            "必须报告合并了至少 1 条来源改动，实际 {:?}",
            saved["merged_sources"]
        );
        // 写出的文件里必须有它（不只信返回值）。
        let on_disk = std::fs::read_to_string(env.dir.join("config.toml")).expect("读配置");
        assert!(
            on_disk.contains("extra"),
            "配置文件里必须出现新增来源，实际:\n{on_disk}"
        );

        // **重启**：同一份配置文件重新建 Runtime。
        let mut rt2 = restart(&env);
        assert!(
            source_ids(&mut rt2).contains(&extra_id),
            "重启后新增来源必须还在（T25-F1：这就是旧实现丢失的东西）"
        );
    }

    /// **T25-F1 删除方向**：`remove_source` → `save_config` → **重启** ⇒ 该来源**不再出现**。
    ///
    /// 只测新增方向不足以证明持久化：旧实现里 `removed_sources` 纯属内存状态，
    /// 重启后配置里的 `image_dirs` 会把它**复活**。
    #[test]
    fn removed_source_stays_gone_after_save_and_restart() {
        let env = TestEnv::new("t25f1-remove-persist");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let images = std::fs::canonicalize(env.media_dir().join("images")).expect("canonicalize");
        let images_id = images.to_string_lossy().into_owned();
        assert!(
            source_ids(&mut rt).contains(&images_id),
            "初始应包含 images"
        );

        ok_json(
            &mut rt,
            Method::RemoveSource,
            serde_json::json!({"source_id": images_id}),
        );
        // **T37 语义**：配置里声明的来源在**保存前仍然生效**（`remove_source` 只登记
        // "待删除"）。这样 `list_sources` 才不会骗人（旧实现立刻隐藏，重启又复活）。
        // 也让 TUI 那条"删除后仍存在 ⇒ 提示运行时删除无效"的分支**可达**。
        assert!(
            source_ids(&mut rt).contains(&images_id),
            "配置来源在保存前必须仍然可见（如实反映\"重启会复活\"）"
        );

        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["saved"], true);
        assert_eq!(saved["verified"], true, "删除也必须回读校验通过");

        let mut rt2 = restart(&env);
        assert!(
            !source_ids(&mut rt2).contains(&images_id),
            "重启后已删除的来源**不得复活**（旧实现会复活）"
        );
    }

    /// **T37（本轮最严重）语义 diff 回归**：保存**只**改用户明确要求改的键。
    ///
    /// 旧实现拿"运行时集合"覆盖配置集合，实测把 `library.image_dirs` 清成 `[]`
    /// ——用户既没要求删、又丢了自己的设置。这里对比保存前后的**完整 `Config`**，
    /// 断言无来源改动时**一个键都不变**。
    #[test]
    fn save_without_source_changes_keeps_every_config_key() {
        let env = TestEnv::new("t37-semantic-diff");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let before = Config::load(&env.dir.join("config.toml")).expect("读保存前配置");
        assert_eq!(
            before
                .library
                .as_ref()
                .map(|l| l.image_dirs.clone())
                .unwrap_or_default(),
            vec![PathBuf::from("images")],
            "前提：用户配置了 image_dirs=[\"images\"]"
        );

        ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));

        let after = Config::load(&env.dir.join("config.toml")).expect("读保存后配置");
        assert_eq!(
            after
                .library
                .as_ref()
                .map(|l| l.image_dirs.clone())
                .unwrap_or_default(),
            before
                .library
                .as_ref()
                .map(|l| l.image_dirs.clone())
                .unwrap_or_default(),
            "保存**不得**改动用户没有要求删除的 image_dirs（旧实现会清成 []）"
        );
        assert_eq!(
            after, before,
            "没有来源改动时，保存后整份配置必须与保存前语义相同"
        );
    }

    /// **T37 预演拒绝**：删除的来源仍被 legacy `[slideshow].directory` 覆盖 ⇒
    /// **拒绝写入**、明确报告冲突容器、配置文件**字节不变**。
    ///
    /// 旧行为：照样写盘，把 `image_dirs` 清成 `[]`，而来源重启后又被 legacy 派生回来
    /// ⇒ "半途生效的删除"（用户丢了设置、来源还在），只有 `verified=false` 事后告知。
    ///
    /// 变异检查：去掉 `plan.residual` 的拒绝分支 ⇒ 本用例红（会写出文件）。
    #[test]
    fn save_refuses_when_removed_source_is_still_derived_by_legacy() {
        let env = TestEnv::new("t37-refuse-legacy");
        seed_library(&env);
        let images = std::fs::canonicalize(env.media_dir().join("images")).expect("canonicalize");
        let images_id = images.to_string_lossy().into_owned();
        // 关键单变量：让 legacy `[slideshow].directory` **指向同一个 images 目录**
        // （hw-verifier 的对照实验：只改这一项，保存结果就从"复活"变成"删除持久"）。
        let legacy_dir = env.media_dir().join("legacy").display().to_string();
        let toml = library_toml(&env, "").replace(
            &format!("directory = \"{legacy_dir}\""),
            &format!("directory = \"{}\"", images.display()),
        );
        assert!(toml.contains(&format!("directory = \"{}\"", images.display())));
        let mut rt = env.runtime(&toml);

        ok_json(
            &mut rt,
            Method::RemoveSource,
            serde_json::json!({"source_id": images_id}),
        );
        let cfg_path = env.dir.join("config.toml");
        let before_bytes = std::fs::read(&cfg_path).expect("读保存前字节");

        let r = call(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert!(!r.ok, "必须**拒绝**保存（而不是写坏再回读）");
        let err = r.error.expect("应有错误");
        assert_eq!(
            err.detail_code.as_deref(),
            Some("NO_EFFECTIVE_CHANGE"),
            "稳定机器码"
        );
        assert!(
            err.conflicts.iter().any(|c| c == "[slideshow].directory"),
            "必须**结构化回传**冲突容器名（不让调用方从中文里猜），实际 {:?}",
            err.conflicts
        );
        assert!(
            !err.conflicts.iter().any(|c| c.contains("root")),
            "不得再声称是 library.root 派生（那个模型不存在）"
        );
        let after_bytes = std::fs::read(&cfg_path).expect("读保存后字节");
        assert_eq!(
            before_bytes, after_bytes,
            "被拒绝的保存**不得**改动配置文件字节"
        );
        let cfg = Config::load(&cfg_path).expect("配置仍合法");
        assert_eq!(
            cfg.library.as_ref().map(|l| l.image_dirs.clone()),
            Some(vec![PathBuf::from("images")]),
            "image_dirs 必须原样保留（旧实现会被清成 []）"
        );
    }

    #[test]
    fn add_source_rejects_relative_outside_and_missing_then_lists() {
        let env = TestEnv::new("t22-sources");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        // 相对路径（客户端路径 ≠ 服务端路径）。
        let r = call(
            &mut rt,
            Method::AddSource,
            serde_json::json!({"kind": "image", "path": "./images"}),
        );
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().and_then(|e| e.detail_code.clone()),
            Some("INVALID_PARAMS".to_string())
        );
        // 根外。
        let r = call(
            &mut rt,
            Method::AddSource,
            serde_json::json!({"kind": "image", "path": "/etc"}),
        );
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().and_then(|e| e.detail_code.clone()),
            Some("PATH_OUTSIDE_ROOT".to_string())
        );
        // 根内但不存在。
        let missing = env.media_dir().join("nope");
        let r = call(
            &mut rt,
            Method::AddSource,
            serde_json::json!({"kind": "image", "path": missing.display().to_string()}),
        );
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().and_then(|e| e.detail_code.clone()),
            Some("NOT_FOUND".to_string())
        );
        // `..` 穿越。
        let r = call(
            &mut rt,
            Method::AddSource,
            serde_json::json!({
                "kind": "image",
                "path": format!("{}/../etc", env.dir.display())
            }),
        );
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().and_then(|e| e.detail_code.clone()),
            Some("PATH_OUTSIDE_ROOT".to_string())
        );
        // 合法追加 + 幂等 + 移除。
        std::fs::create_dir_all(env.media_dir().join("extra")).expect("建 extra");
        let added = ok_json(
            &mut rt,
            Method::AddSource,
            serde_json::json!({
                "kind": "image",
                "path": env.media_dir().join("extra").display().to_string(),
                "recursive": true
            }),
        );
        assert_eq!(added["created"], true);
        let again = ok_json(
            &mut rt,
            Method::AddSource,
            serde_json::json!({
                "kind": "image",
                "path": env.media_dir().join("extra").display().to_string()
            }),
        );
        assert_eq!(again["created"], false, "重复添加必须幂等");
        let sources = ok_json(&mut rt, Method::ListSources, serde_json::json!({}));
        let ids: Vec<&str> = sources["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .map(|s| s["id"].as_str().unwrap_or(""))
            .collect();
        assert!(
            ids.iter().any(|i| i.ends_with("extra")),
            "list_sources 必须反映运行时新增: {ids:?}"
        );
        let source_id = added["source_id"].as_str().expect("source_id").to_string();
        ok_json(
            &mut rt,
            Method::RemoveSource,
            serde_json::json!({"source_id": source_id}),
        );
        let sources = ok_json(&mut rt, Method::ListSources, serde_json::json!({}));
        let ids: Vec<&str> = sources["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .map(|s| s["id"].as_str().unwrap_or(""))
            .collect();
        assert!(!ids.iter().any(|i| i.ends_with("extra")));
    }

    #[test]
    fn legacy_slideshow_directory_is_merged_and_deduped() {
        let env = TestEnv::new("t22-legacy");
        seed_library(&env);
        std::fs::create_dir_all(env.media_dir().join("legacy")).expect("建 legacy");
        // 旧键指向另一个目录：必须**并入**来源集合。
        let toml = format!(
            "{}\n[library]\nroot = \"{dir}\"\nimage_dirs = [\"images\"]\nvideo_dirs = [\"videos\"]\n",
            env.toml("", 3600).replace(
                &format!("directory = \"{}\"", env.media_dir().display()),
                &format!(
                    "directory = \"{}\"",
                    env.media_dir().join("legacy").display()
                )
            ),
            dir = env.media_dir().display()
        );
        let mut rt = env.runtime(&toml);
        let sources = ok_json(&mut rt, Method::ListSources, serde_json::json!({}));
        let ids: Vec<String> = sources["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .map(|s| s["id"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            ids.iter().any(|i| i.ends_with("images")),
            "library 来源在: {ids:?}"
        );
        assert!(
            ids.iter().any(|i| i.ends_with("legacy")),
            "旧 [slideshow].directory 必须并入（不静默丢弃）: {ids:?}"
        );
        let mut dedup = ids.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), ids.len(), "来源必须去重: {ids:?}");
    }

    #[test]
    fn set_output_transform_is_runtime_only_and_validated() {
        let env = TestEnv::new("t22-transform");
        seed_library(&env);
        let toml = library_toml(&env, "");
        let mut rt = env.runtime(&toml);
        let before = std::fs::read_to_string(env.dir.join("config.toml")).expect("读配置");
        let r = ok_json(
            &mut rt,
            Method::SetOutputTransform,
            serde_json::json!({
                "output": "DP-1", "fit": "contain", "zoom": 1.5, "rotation": 90
            }),
        );
        assert_eq!(r["accepted"], true);
        let out = rt.config.outputs.get("DP-1").expect("输出条目");
        assert_eq!(out.fit, liuguang_core::config::FitMode::Contain);
        assert!((out.zoom - 1.5).abs() < 1e-6);
        assert_eq!(out.rotation, Rotation::Deg90);
        let after = std::fs::read_to_string(env.dir.join("config.toml")).expect("读配置");
        assert_eq!(before, after, "set_output_transform 只改运行时，不落盘");
        // 校验：非法旋转 / 非法 fit / 未知输出。
        for (params, want) in [
            (
                serde_json::json!({"output": "DP-1", "rotation": 45}),
                "INVALID_PARAMS",
            ),
            (
                serde_json::json!({"output": "DP-1", "fit": "zoomies"}),
                "INVALID_PARAMS",
            ),
            (
                serde_json::json!({"output": "HDMI-9", "fit": "cover"}),
                "UNKNOWN_OUTPUT",
            ),
        ] {
            let r = call(&mut rt, Method::SetOutputTransform, params);
            assert!(!r.ok, "必须拒绝: {want}");
            if want == "INVALID_PARAMS" {
                assert_eq!(
                    r.error.as_ref().and_then(|e| e.detail_code.clone()),
                    Some("INVALID_PARAMS".to_string())
                );
            }
        }
    }

    #[test]
    fn save_config_is_atomic_and_round_trips() {
        let env = TestEnv::new("t22-save");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        ok_json(
            &mut rt,
            Method::SetOutputTransform,
            serde_json::json!({"output": "DP-1", "fit": "contain", "zoom": 2.0}),
        );
        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["saved"], true);
        assert!(saved["bytes"].as_u64().unwrap_or(0) > 0);
        let path = env.dir.join("config.toml");
        let text = std::fs::read_to_string(&path).expect("读回配置");
        assert!(!text.contains(".tmp-"), "不得留下临时文件: {text}");
        let reloaded = Config::load(&path).expect("写回的配置必须可再次解析");
        let o = reloaded.outputs.get("DP-1").expect("输出条目");
        assert_eq!(o.fit, liuguang_core::config::FitMode::Contain);
        assert!((o.zoom - 2.0).abs() < 1e-6);
        assert_eq!(reloaded.slideshow.directory, rt.config.slideshow.directory);
        // 目录里不应残留临时文件。
        let leftovers: Vec<String> = std::fs::read_dir(&env.dir)
            .expect("读目录")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
    }

    #[test]
    fn save_config_reports_permission_denied_without_corrupting() {
        let env = TestEnv::new("t22-save-denied");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        // 指向只读目录里的路径（非 root 时必然 EACCES）。
        let ro = env.dir.join("ro");
        std::fs::create_dir_all(&ro).expect("建只读目录");
        let target = ro.join("config.toml");
        std::fs::write(&target, b"[display]\n").expect("写原文件");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).expect("设只读");
        }
        rt.config_path = target.clone();
        let r = call(&mut rt, Method::SaveConfig, serde_json::json!({}));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&ro).expect("meta").permissions().mode() & 0o777;
            if mode & 0o200 == 0 {
                // 非 root：必须明确拒绝，且原文件不变。
                assert!(!r.ok, "不可写时必须报错（不得假装保存成功）");
                assert_eq!(
                    r.error.as_ref().map(|e| (e.code, e.detail_code.clone())),
                    Some((
                        ErrorKind::PermissionDenied,
                        Some("PERMISSION_DENIED".to_string())
                    ))
                );
                assert_eq!(
                    std::fs::read_to_string(&target).expect("读原文件"),
                    "[display]\n"
                );
            }
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755))
                .expect("恢复权限");
        }
    }

    /// **t45-F1**：受理与上屏解耦后，`set_wallpaper` 的 IPC 往返应远小于一次首帧
    /// 建立（真机 0.6–1s）；判据取自 t44/t45 口径：≤100ms。
    #[cfg(feature = "gstreamer")]
    #[test]
    fn set_wallpaper_replies_before_the_pipeline_is_built() {
        let env = TestEnv::new("t45-async-set");
        seed_library(&env);
        let Some(clip) = matrix_clip("matrix-1920x1080-60fps-h264-8bit.mp4") else {
            eprintln!("跳过：本机没有矩阵素材（.work/corpus/matrix 或 .work/matrix/media）");
            return;
        };
        let media_dir = env.media_dir();
        let local = media_dir.join("async.mp4");
        std::fs::hard_link(&clip, &local)
            .or_else(|_| std::fs::copy(&clip, &local).map(|_| ()))
            .expect("准备素材");
        let mut rt = env.runtime(&library_toml(&env, ""));
        let started = Instant::now();
        let response = match rt.handle(Request::new(
            "t7",
            Method::SetWallpaper,
            serde_json::json!({"path": local.display().to_string(), "output": "DP-1"}),
        )) {
            liuguang_ipc::Dispatch::Reply(resp) => resp,
            other => panic!("期望 Reply，实际 {other:?}"),
        };
        let elapsed = started.elapsed();
        assert!(response.ok, "受理应成功: {:?}", response.error);
        assert!(
            elapsed < Duration::from_millis(100),
            "受理往返应 ≤100ms，实际 {elapsed:?}"
        );
        assert!(
            rt.channels["DP-1"].current.is_none(),
            "受理阶段不得已经在播（上屏由 tick 完成）"
        );
        // 真正上屏发生在 tick 里；完成后必须发 WALLPAPER_APPLIED。
        let events = rt.events.subscribe();
        settle_set(&mut rt);
        assert!(
            rt.channels["DP-1"].current.is_some(),
            "settle 之后必须有画面"
        );
        assert!(
            drain_events(&events)
                .iter()
                .any(|e| e.kind == EventKind::WallpaperApplied && e.ok == Some(true)),
            "上屏完成后必须发 WALLPAPER_APPLIED"
        );
    }

    /// **t46**：有已受理的 `set_wallpaper` 时不抢跑启动首帧——否则请求要在运行时
    /// 队列里等 prime 建完管线（真机冷启动首次 set 700–760ms），并且同一条管线
    /// 可能被 prime 和 pending 各建一次。
    #[test]
    fn priming_is_skipped_while_a_set_is_pending() {
        let env = TestEnv::new("t46-prime-skip");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let png = env.png("pending.png", 8, 8, [10, 20, 30, 255]);
        rt.pending_sets.push(PendingSet {
            request_id: "set-1".to_string(),
            output: "DP-1".to_string(),
            entry: MediaEntry {
                path: png.clone(),
                kind: MediaKind::Image,
            },
            declared_path: png.display().to_string(),
            pin: false,
            transform_changed: false,
        });
        rt.prime_first_frame();
        assert!(
            rt.channels["DP-1"].current.is_none(),
            "有 pending 时不得抢跑建管线"
        );
        assert!(
            !rt.primed_first_frame,
            "跳过不得消耗 prime 机会（pending 处理完后仍要能点亮）"
        );
        rt.run_pending_sets();
        assert!(
            rt.channels["DP-1"].current.is_some(),
            "pending 自己把画面铺上"
        );
    }

    /// **t46**：目标画面已在屏上时，pending 只做收尾（不重建解码管线）。
    #[test]
    fn pending_set_reuses_the_frame_already_on_screen() {
        let env = TestEnv::new("t46-reuse");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let png = env.png("reuse.png", 8, 8, [7, 8, 9, 255]);
        let _ = ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": png.display().to_string(), "output": "DP-1"}),
        );
        let events = rt.events.subscribe();
        // 第二条同媒体的请求：应命中"已在屏上"的复用分支，仍然要发 APPLIED。
        rt.pending_sets.push(PendingSet {
            request_id: "set-reuse".to_string(),
            output: "DP-1".to_string(),
            entry: MediaEntry {
                path: png.clone(),
                kind: MediaKind::Image,
            },
            declared_path: png.display().to_string(),
            pin: false,
            transform_changed: false,
        });
        rt.run_pending_sets();
        rt.flush_pending_applied();
        let applied = drain_events(&events)
            .into_iter()
            .find(|e| e.kind == EventKind::WallpaperApplied)
            .expect("复用路径也必须发 WALLPAPER_APPLIED");
        assert_eq!(applied.ok, Some(true));
        assert_eq!(applied.request_id.as_deref(), Some("set-reuse"));
        assert_eq!(
            rt.channels["DP-1"]
                .current
                .as_ref()
                .map(|d| d.media.clone()),
            Some(png),
            "复用后仍显示同一媒体"
        );
    }

    /// **t45-F2**：全屏让路期间顶层 `status.state` 必须与通道 `pause_reason` 一致，
    /// 退出全屏后回到播放态（消掉 journal 的 `contradiction=I1/I2`）。
    #[cfg(feature = "gstreamer")]
    #[test]
    fn fullscreen_pause_and_resume_keep_top_level_state_in_sync() {
        let env = TestEnv::new("t45-fullscreen");
        seed_library(&env);
        let Some(clip) = matrix_clip("matrix-1920x1080-60fps-h264-8bit.mp4") else {
            eprintln!("跳过：本机没有矩阵素材（.work/corpus/matrix 或 .work/matrix/media）");
            return;
        };
        let media_dir = env.media_dir();
        let local = media_dir.join("fullscreen.mp4");
        std::fs::hard_link(&clip, &local)
            .or_else(|_| std::fs::copy(&clip, &local).map(|_| ()))
            .expect("准备素材");
        let mut rt = env.runtime(&library_toml(&env, ""));
        let _ = ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": local.display().to_string(), "output": "DP-1"}),
        );
        assert!(
            rt.channels["DP-1"].video.is_some(),
            "前置：视频管线应已建立"
        );

        rt.pause_for_fullscreen(Instant::now());
        assert!(
            rt.state_contradictions().is_empty(),
            "全屏让路后状态源必须一致: {:?}",
            rt.state_contradictions()
        );
        let status = status_json(&mut rt);
        assert_eq!(
            status["state"], "Paused",
            "全屏让路后顶层应为 Paused: {status}"
        );
        assert_eq!(status["channels"][0]["pause_reason"], "fullscreen");

        rt.resume_after_fullscreen();
        assert!(
            rt.state_contradictions().is_empty(),
            "退出全屏后状态源必须一致: {:?}",
            rt.state_contradictions()
        );
        let status = status_json(&mut rt);
        assert_eq!(
            status["state"], "VideoPlaying",
            "退出全屏后应回到播放态: {status}"
        );
        assert!(status["channels"][0]["pause_reason"].is_null());
    }

    #[test]
    fn set_wallpaper_separates_accept_from_applied_via_event() {
        let env = TestEnv::new("t22-applied");
        seed_library(&env);
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let events = rt.events.subscribe();
        // t45：这里**不能**用 `ok_json`（它会把 pending set 跑完）。直接调
        // `handle` 只看"受理"那一刻：响应带 accepted，事件还没发。
        let ack_response = match rt.handle(Request::new(
            "t7",
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "output": "DP-1"}),
        )) {
            liuguang_ipc::Dispatch::Reply(resp) => resp,
            other => panic!("期望 Reply，实际 {other:?}"),
        };
        assert!(ack_response.ok, "受理应成功: {:?}", ack_response.error);
        let ack = ack_response.result.expect("受理必须有 result");
        assert_eq!(ack["accepted"], true);
        assert!(
            rt.channels["DP-1"].current.is_none(),
            "受理阶段不得同步建管线/上屏（t45：上屏留给 tick）"
        );
        let request_id = ack["request_id"].as_str().expect("受理必须带 request_id");
        assert!(
            !drain_events(&events)
                .iter()
                .any(|e| e.kind == EventKind::WallpaperApplied),
            "受理阶段不得提前发 APPLIED（顺序必须是 Accepted → Applied）"
        );
        rt.tick(Instant::now());
        let events = drain_events(&events);
        let applied = events
            .iter()
            .find(|e| e.kind == EventKind::WallpaperApplied)
            .expect("必须发出 WALLPAPER_APPLIED");
        assert_eq!(applied.request_id.as_deref(), Some(request_id));
        assert_eq!(applied.ok, Some(true));
        assert_eq!(applied.output.as_deref(), Some("DP-1"));
        assert!(applied.media.as_deref().unwrap_or("").ends_with("a.png"));
    }

    #[test]
    fn failed_set_wallpaper_reports_error_and_applied_false() {
        let env = TestEnv::new("t22-applied-fail");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let events = rt.events.subscribe();
        let r = call(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({
                "path": env.dir.join("nope.png").display().to_string(),
                "output": "DP-1"
            }),
        );
        assert!(!r.ok, "不存在的文件必须回错误（旧客户端语义不变）");
        rt.tick(Instant::now());
        let events = drain_events(&events);
        let applied = events
            .iter()
            .find(|e| e.kind == EventKind::WallpaperApplied)
            .expect("失败也要发 APPLIED（ok=false）");
        assert_eq!(applied.ok, Some(false));
        assert!(
            applied
                .error
                .as_deref()
                .unwrap_or("")
                .contains("FILE_UNAVAILABLE"),
            "失败原因要带稳定码: {:?}",
            applied.error
        );
    }

    #[test]
    fn get_metrics_exposes_process_stats_for_system_page() {
        let env = TestEnv::new("t22-metrics");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let m = ok_json(&mut rt, Method::GetMetrics, serde_json::json!({}));
        assert!(
            m["mem_bytes"].as_u64().unwrap_or(0) > 0,
            "必须给出常驻内存: {m}"
        );
        assert!(m["threads"].as_u64().unwrap_or(0) >= 1, "必须给出线程数");
        assert!(m["uptime_seconds"].is_u64());
        assert!(m["cpu_percent"].is_number(), "必须给出 CPU%（首次为 0）");
        assert!(m["fps"].is_number(), "必须给出提交成功帧率");
        // 第二次采样后 CPU 数值必须有限（不能是 NaN/inf 这类会破坏 TUI 的值）。
        std::thread::sleep(Duration::from_millis(30));
        let m2 = ok_json(&mut rt, Method::GetMetrics, serde_json::json!({}));
        let cpu = m2["cpu_percent"].as_f64().expect("cpu_percent 是数字");
        assert!(
            cpu.is_finite() && (0.0..=64.0 * 100.0).contains(&cpu),
            "cpu={cpu}"
        );
    }

    #[test]
    fn update_playlist_validates_and_round_trips() {
        let env = TestEnv::new("t22-playlist");
        seed_library(&env);
        let a = env.png("images/a.png", 4, 4, [1, 2, 3, 255]);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let pl = ok_json(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({
                "name": "demo",
                "entries": [{"media_id": a.display().to_string()}],
                "mode": "random",
                "interval_seconds": 15,
                "on_video_end": "loop"
            }),
        );
        assert_eq!(pl["name"], "demo");
        assert_eq!(pl["mode"], "random");
        let list = ok_json(&mut rt, Method::ListPlaylists, serde_json::json!({}));
        assert_eq!(list["playlists"].as_array().map(|a| a.len()), Some(1));
        // 非法 mode。
        let r = call(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({"name": "x", "mode": "shuffle"}),
        );
        assert!(!r.ok);
        // 条目越界（根外路径不得进播放列表）。
        let outside_png = env.dir.join("outside.png");
        std::fs::write(&outside_png, b"x").expect("写根外图片");
        let r = call(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({"name": "x", "entries": [{"media_id": outside_png.display().to_string()}]}),
        );
        assert!(!r.ok);
        assert_eq!(
            r.error.as_ref().and_then(|e| e.detail_code.clone()),
            Some("PATH_OUTSIDE_ROOT".to_string())
        );
    }
    // ---- T63：测试基建的隔离性（顺序/并行 flake 的根） ----

    /// **同名 `TestEnv` 也必须各自独立**（T63）。
    ///
    /// 这是"顺序相关 flake"的结构性根因守门：目录键原先只有 `{name}-{pid}`，
    /// 同进程内两个同名用例会共享目录，而 `TestEnv::new` 的 `remove_dir_all`
    /// 会删掉另一个用例正在用的素材 ⇒ 单跑绿、全量按某个顺序跑红。
    ///
    /// **变异**：把目录名里的进程内序号去掉（恢复 `{name}-{pid}`）⇒ 本测试变红。
    /// 取一份 `status`（测试里只关心 `fullscreen` 等字段）。
    fn status_of(rt: &Runtime) -> StatusResult {
        let response = rt.handle_status("t-status".to_string());
        let value = serde_json::to_value(&response.result).expect("status 应可序列化");
        serde_json::from_value(value).expect("status 结果应可反序列化")
    }

    /// 全屏结束后必须**自动恢复**，而且只恢复自己暂停的那些通道。
    ///
    /// 这条比"能检测到全屏"更重要：只暂停不恢复 = 视频永远冻住（用户视角是
    /// "退出游戏后壁纸死了"）。
    #[test]
    fn fullscreen_pause_resumes_only_channels_it_paused() {
        let env = TestEnv::new("t-fullscreen-resume");
        let backend = liuguang_display::mock::MockDisplayBackend::with_outputs(&["DP-1"]);
        let switch = backend.fullscreen_handle();
        let toml = env.toml("", 3600);
        let mut rt = env.runtime_with_backend(&toml, Box::new(backend));
        // 一个在播视频的通道（stub 视频源足够：只看暂停/恢复状态位）。
        attach_stub_video(&mut rt, "DP-1", true);

        let t0 = Instant::now();
        if let Ok(mut slot) = switch.lock() {
            *slot = Some(liuguang_display::FullscreenToplevel {
                app_id: Some("steam".to_string()),
                title: Some("Elden Ring".to_string()),
                outputs: vec!["eDP-1".to_string()],
            });
        }
        rt.tick(t0);
        rt.tick(t0 + Duration::from_millis(1200));
        assert!(rt.fullscreen_active);
        assert_eq!(rt.fullscreen_paused.len(), 1, "应暂停 1 个通道");
        assert!(rt.channels["DP-1"].paused, "通道应已暂停");

        // 退出全屏
        if let Ok(mut slot) = switch.lock() {
            *slot = None;
        }
        rt.tick(t0 + Duration::from_millis(2000));
        rt.tick(t0 + Duration::from_millis(2600));
        assert!(!rt.fullscreen_active, "应已退出全屏态");
        assert!(rt.fullscreen_paused.is_empty(), "不得残留暂停记录");
        assert!(!rt.channels["DP-1"].paused, "视频应自动恢复");
    }

    /// 全屏检测：状态要**持续**一个滞回窗口才生效，`status.fullscreen` 才报。
    #[test]
    fn fullscreen_detection_needs_hysteresis_then_reports_status() {
        let env = TestEnv::new("t-fullscreen-hysteresis");
        let mut backend = liuguang_display::mock::MockDisplayBackend::with_outputs(&["DP-1"]);
        backend.set_fullscreen(Some(liuguang_display::FullscreenToplevel {
            app_id: Some("steam".to_string()),
            title: Some("Elden Ring".to_string()),
            outputs: vec!["DP-1".to_string()],
        }));
        let toml = env.toml("", 3600);
        let mut rt = env.runtime_with_backend(&toml, Box::new(backend));

        let t0 = Instant::now();
        // 第一次采样只记时刻，不判定（避免 alt-tab 抖动）
        rt.tick(t0);
        assert!(status_of(&rt).fullscreen.is_none(), "滞回未过就先别报");

        // 过了滞回窗口：判定为全屏并出现在 status 里
        rt.tick(t0 + Duration::from_millis(1200));
        let status = status_of(&rt);
        let full = status.fullscreen.expect("滞回过后应报告全屏窗口");
        assert_eq!(full.app_id.as_deref(), Some("steam"));
        assert_eq!(full.title.as_deref(), Some("Elden Ring"));
        assert_eq!(full.paused_channels, 0); // 没有在播的视频 ⇒ 没暂停任何通道
    }

    /// 全屏关掉后要**恢复**（不能永远停在暂停态），且 `status` 不再报全屏。
    #[test]
    fn fullscreen_cleared_restores_state() {
        let env = TestEnv::new("t-fullscreen-clear");
        let mut backend = liuguang_display::mock::MockDisplayBackend::with_outputs(&["DP-1"]);
        let full = liuguang_display::FullscreenToplevel {
            app_id: Some("mpv".to_string()),
            title: None,
            outputs: Vec::new(),
        };
        backend.set_fullscreen(Some(full));
        // 共享开关：Runtime 持有后端之后仍能翻转"全屏/不全屏"。
        let switch = backend.fullscreen_handle();
        let toml = env.toml("", 3600);
        let mut rt = env.runtime_with_backend(&toml, Box::new(backend));

        let t0 = Instant::now();
        rt.tick(t0);
        rt.tick(t0 + Duration::from_millis(1200));
        assert!(rt.fullscreen_active, "应判定为全屏中");

        // 窗口退出全屏：下一次采样应清空
        if let Ok(mut slot) = switch.lock() {
            *slot = None;
        }
        rt.tick(t0 + Duration::from_millis(2500));
        assert!(!rt.fullscreen_active, "退出全屏后应恢复");
        assert!(status_of(&rt).fullscreen.is_none());
    }

    /// 关掉 `pause_on_fullscreen` 时**完全不检测**，也不会留着旧的暂停。
    #[test]
    fn disabled_fullscreen_pause_does_not_track_windows() {
        let env = TestEnv::new("t-fullscreen-disabled");
        let mut backend = liuguang_display::mock::MockDisplayBackend::with_outputs(&["DP-1"]);
        backend.set_fullscreen(Some(liuguang_display::FullscreenToplevel::default()));
        let toml = env.toml("", 3600).replace(
            "[transition]",
            "[performance]\npause_on_fullscreen = false\n\n[transition]",
        );
        let mut rt = env.runtime_with_backend(&toml, Box::new(backend));
        let t0 = Instant::now();
        rt.tick(t0);
        rt.tick(t0 + Duration::from_millis(2000));
        assert!(!rt.fullscreen_active, "关掉开关就不该判定全屏");
        assert!(status_of(&rt).fullscreen.is_none());
    }

    /// 转场设置改完必须**真的写进** `config.toml`，否则重启就回到旧值
    /// （用户反馈："改完重启又变回去"）。
    #[test]
    fn transition_change_is_written_back_after_debounce() {
        let env = TestEnv::new("t-config-write-transition");
        let toml = env.toml("", 3600);
        let mut rt = env.runtime(&toml);

        let params = serde_json::json!({
            "effect": "fade",
            "duration_ms": 900,
            "easing": "ease-out",
            "direction": "left"
        });
        let resp = rt.handle_transition("t1".to_string(), params);
        assert!(resp.ok, "{resp:?}");

        // 防抖窗口内不写盘（一次拖动只写一次）
        let cfg_path = env.dir.join("config.toml");
        let before = std::fs::read_to_string(&cfg_path).unwrap();
        rt.tick(Instant::now());
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), before);

        // 过了防抖窗口就落盘
        rt.tick(Instant::now() + Duration::from_secs(2));
        let after = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(after.contains("effect = \"fade\""), "{after}");
        assert!(after.contains("duration_ms = 900"), "{after}");
        assert!(after.contains("easing = \"ease-out\""), "{after}");
        assert!(after.contains("direction = \"left\""), "{after}");

        // 重新读盘 = "重启"：设置还在
        let reloaded = Config::load(&cfg_path).expect("写回后的配置必须仍然合法");
        assert_eq!(reloaded.transition.duration_ms, 900);
    }

    /// 旋转同理：只写内存的话重启就丢。
    #[test]
    fn rotation_change_is_written_back() {
        let env = TestEnv::new("t-config-write-rotation");
        let outputs =
            "[outputs.\"DP-1\"]\nfit = \"cover\"\nzoom = 1.0\noffset_x = 0.0\noffset_y = 0.0\n";
        let toml = env.toml(outputs, 3600);
        let mut rt = env.runtime(&toml);
        let params = serde_json::json!({ "output": "DP-1", "rotation": 90 });
        let resp = rt.handle_set_output_transform("t1".to_string(), params);
        assert!(resp.ok, "{resp:?}");

        rt.tick(Instant::now() + Duration::from_secs(2));
        let text = std::fs::read_to_string(env.dir.join("config.toml")).unwrap();
        assert!(text.contains("rotation = \"90\""), "{text}");
        let reloaded = Config::load(&env.dir.join("config.toml")).expect("配置应合法");
        assert_eq!(reloaded.outputs["DP-1"].rotation, Rotation::Deg90);
    }

    /// 写不进去（只读文件）时**不能砸运行**：设置立刻生效，只是重启会回到旧值。
    #[test]
    fn unwritable_config_does_not_break_the_run() {
        let env = TestEnv::new("t-config-readonly");
        let toml = env.toml("", 3600);
        let mut rt = env.runtime(&toml);
        let cfg_path = env.dir.join("config.toml");
        // 让配置变成"不可读"（比只读更狠：内容是非法 TOML ⇒ 写回必然失败）
        std::fs::write(&cfg_path, "这不是 TOML [[[\n").unwrap();

        let params = serde_json::json!({ "effect": "fade", "duration_ms": 700 });
        let resp = rt.handle_transition("t1".to_string(), params);
        assert!(resp.ok, "写回失败不应影响受理: {resp:?}");
        rt.tick(Instant::now() + Duration::from_secs(2));
        // 运行时设置仍然生效
        assert_eq!(rt.transitions.duration_ms, 700);
        // 文件没被改坏（我们把非法 TOML 原样留着，不覆盖）
        assert_eq!(
            std::fs::read_to_string(&cfg_path).unwrap(),
            "这不是 TOML [[[\n"
        );
    }

    #[test]
    fn test_env_dirs_are_unique_even_with_the_same_name() {
        let a = TestEnv::new("t63-same-name");
        let keeper = a.media_dir().join("keep.png");
        std::fs::write(&keeper, b"x").expect("写 a 的素材");

        let b = TestEnv::new("t63-same-name");
        assert_ne!(
            a.dir, b.dir,
            "同名 TestEnv 必须落在不同目录（否则一个用例的 remove_dir_all 会删掉另一个的素材）"
        );
        assert!(
            !b.media_dir().join("keep.png").exists(),
            "b 是全新目录，不应看到 a 的文件"
        );

        // 再建第三个同名实例：**不得**销毁前两个实例的目录/文件。
        let c = TestEnv::new("t63-same-name");
        assert_ne!(c.dir, a.dir);
        assert_ne!(c.dir, b.dir);
        assert!(
            keeper.exists(),
            "新建同名 TestEnv 不得销毁已有实例的素材（这正是顺序 flake 的成因）"
        );
    }

    // ---- T62：播放列表落盘 / 装载 / 安全边界 ----

    /// 起一个带媒体库的 Runtime，并建一条播放列表（返回路径与配置路径）。
    fn t62_setup(name: &str) -> (TestEnv, String) {
        let env = TestEnv::new(name);
        seed_library(&env);
        let toml = library_toml(&env, "");
        (env, toml)
    }

    /// `save_config` 必须**真的**把播放列表写进文件，且 `verified` 反映真实结果。
    ///
    /// **变异（两条测试共用）**：删掉 `plan_source_merge` 里的
    /// `config.playlists = self.playlists...` ⇒ 本测试与 `playlists_survive_restart_*`
    /// 都必须变红（前者"文件不含 [[playlists]]"，后者"重启后回到 0"）。
    #[test]
    fn save_config_persists_playlists_and_verifies() {
        let (env, toml) = t62_setup("t62-save");
        let a = env.media_dir().join("images/a.png");
        let mut rt = env.runtime(&toml);
        let cfg_path = env.dir.join("config.toml");
        let before = std::fs::read_to_string(&cfg_path).expect("读保存前配置");
        assert!(
            !before.contains("playlists"),
            "本测试前提：配置文件里还没有播放列表"
        );
        ok_json(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({
                "name": "晚间",
                "entries": [{"media_id": a.display().to_string()}],
                "mode": "random",
                "interval_seconds": 600,
                "on_video_end": "loop",
                "no_repeat": true
            }),
        );
        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["saved"], true, "saved 应表示字节写出");
        assert_eq!(
            saved["verified"], true,
            "verified 必须反映真实结果（来源 + 播放列表都被回读校验）"
        );
        let after = std::fs::read_to_string(&cfg_path).expect("读保存后配置");
        assert_ne!(
            before, after,
            "save_config 后配置内容必须真的变化（T62 的核心）"
        );
        for needle in [
            "[[playlists]]",
            "pl-1",
            "晚间",
            "mode = \"random\"",
            "on_video_end = \"loop\"",
            "no_repeat = true",
            "interval_seconds = 600",
            a.to_string_lossy().as_ref(),
        ] {
            assert!(
                after.contains(needle),
                "保存后的配置必须含 {needle:?}：\n{after}"
            );
        }
        // 幂等：再保存一次不应追加重复段（每次保存都从运行时重建，不是累加）。
        let saved2 = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved2["verified"], true);
        let after2 = std::fs::read_to_string(&cfg_path).expect("读第二次保存后的配置");
        assert_eq!(after, after2, "第二次保存必须字节稳定（不重复追加）");
    }

    /// **重启**（按磁盘配置重建 Runtime）后 `list_playlists` 仍返回该列表，逐字段一致。
    #[test]
    fn playlists_survive_restart_with_identical_fields() {
        let (env, toml) = t62_setup("t62-restart");
        let a = env.media_dir().join("images/a.png");
        let b = env.media_dir().join("images/b.png");
        let mut rt = env.runtime(&toml);
        ok_json(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({
                "name": "重启后还在",
                "entries": [{"media_id": a.display().to_string()}, {"media_id": b.display().to_string()}],
                "mode": "random",
                "interval_seconds": 600,
                "on_video_end": "loop",
                "no_repeat": true
            }),
        );
        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["verified"], true);
        drop(rt);

        let mut rt2 = env.runtime_reload(&["DP-1"]);
        let list = ok_json(&mut rt2, Method::ListPlaylists, serde_json::json!({}));
        let pls = list["playlists"].as_array().expect("playlists 应为数组");
        assert_eq!(pls.len(), 1, "重启后必须仍有 1 条播放列表");
        let pl = &pls[0];
        assert_eq!(pl["id"], "pl-1");
        assert_eq!(pl["name"], "重启后还在");
        assert_eq!(pl["mode"], "random");
        assert_eq!(pl["interval_seconds"], 600);
        assert_eq!(pl["on_video_end"], "loop");
        assert_eq!(pl["no_repeat"], true);
        assert_eq!(
            pl["entries"].as_array().map(|v| v.len()),
            Some(2),
            "条目数必须一致"
        );
        // 条目按原顺序、且是**规范化**路径（装载时过 media.resolve()）。
        let want: Vec<String> = [&a, &b]
            .iter()
            .map(|p| {
                std::fs::canonicalize(p)
                    .expect("素材应存在")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let got: Vec<String> = pl["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| e["media_id"].as_str().expect("media_id").to_string())
            .collect();
        assert_eq!(got, want, "条目必须按原顺序、且为规范化绝对路径");
    }

    /// `verified` 必须**真的**比对播放列表：外部把配置里的 `[[playlists]]` 删掉后，
    /// 回读校验必须为 `false` —— 否则"saved=true / verified=true"又会变成假成功。
    #[test]
    fn verify_detects_playlists_removed_from_disk() {
        let (env, toml) = t62_setup("t62-verify");
        let a = env.media_dir().join("images/a.png");
        let mut rt = env.runtime(&toml);
        ok_json(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({
                "name": "会被篡改",
                "entries": [{"media_id": a.display().to_string()}],
                "interval_seconds": 60
            }),
        );
        ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert!(rt.verify_saved_config(), "刚保存成功时回读校验应为 true");
        let cfg_path = env.dir.join("config.toml");
        let text = std::fs::read_to_string(&cfg_path).expect("读配置");
        let cut = text
            .find("[[playlists]]")
            .expect("保存后配置里应有 [[playlists]] 段");
        std::fs::write(&cfg_path, &text[..cut]).expect("篡改配置（删掉播放列表段）");
        assert!(
            !rt.verify_saved_config(),
            "文件里已经没有播放列表 ⇒ verified 必须为 false（不能假成功）"
        );
    }

    /// **安全边界**：配置里越界（根外 / `/etc/passwd` / `..`）的条目必须被拒绝；
    /// 边界内但暂缺的路径**保留**（否则下次保存会把它从配置里抹掉 = 数据丢失）。
    #[test]
    fn config_playlist_entries_are_validated_against_media_root() {
        let (env, base) = t62_setup("t62-boundary");
        let outside = env.dir.join("outside.png");
        std::fs::write(&outside, b"x").expect("写根外图片");
        let missing = env.media_dir().join("images/missing.png");
        let toml = format!(
            "{base}\n[[playlists]]\nid = \"pl-1\"\nname = \"边界\"\n\
             [[playlists.entries]]\nmedia_id = \"{outside}\"\n\
             [[playlists.entries]]\nmedia_id = \"/etc/passwd\"\n\
             [[playlists.entries]]\nmedia_id = \"{root}/images/../images/a.png\"\n\
             [[playlists.entries]]\nmedia_id = \"{root}/images/a.png\"\n\
             [[playlists.entries]]\nmedia_id = \"{missing}\"\n\
             [[playlists.entries]]\nmedia_id = \"images/a.png\"\n",
            outside = outside.display(),
            root = env.media_dir().display(),
            missing = missing.display(),
        );
        let mut rt = env.runtime(&toml);
        let list = ok_json(&mut rt, Method::ListPlaylists, serde_json::json!({}));
        let pls = list["playlists"].as_array().expect("playlists");
        assert_eq!(pls.len(), 1);
        let entries = pls[0]["entries"].as_array().expect("entries");
        // **T63**：断言"留下的**集合**"，不依赖条目顺序。
        //
        // 原实现比较 `entries[0]`/`entries[1]` —— 把"装载器保序"这个实现细节
        // 当成了测试前提：任何顺序变化（或将来改成按路径排序/去重）都会让这条
        // **安全测试**无端变红，而它真正要守的是"哪些路径被接受、哪些被拒"。
        let got: Vec<String> = {
            let mut v: Vec<String> = entries
                .iter()
                .map(|e| {
                    e["media_id"]
                        .as_str()
                        .expect("media_id 应为字符串")
                        .to_string()
                })
                .collect();
            v.sort();
            v
        };
        let mut want = vec![
            std::fs::canonicalize(env.media_dir().join("images/a.png"))
                .expect("素材存在")
                .to_string_lossy()
                .into_owned(),
            missing.to_string_lossy().into_owned(),
        ];
        want.sort();
        assert_eq!(
            got, want,
            "接受集合应为「合法绝对路径 + 边界内但当前缺失的（不丢，免得保存时被抹掉）」；\
             根外 / `/etc/passwd` / 含 `..` / 相对路径 4 条必须全部被拒。实际: {entries:?}"
        );
    }

    /// 缺 `[library]` 时**不装载**（无安全边界），但 `save_config` **不得**把配置里的
    /// 播放列表抹成空（那是 T37 同族的数据丢失）。
    #[test]
    fn playlists_without_library_are_preserved_not_erased() {
        let env = TestEnv::new("t62-no-library");
        let png = env.png("a.png", 8, 8, [1, 2, 3, 255]);
        // 注意：这里**故意**不加 [library]（`TestEnv::toml` 本来就没有）。
        let toml = format!(
            "{}\n[[playlists]]\nid = \"pl-9\"\nname = \"没有媒体库\"\n\
             [[playlists.entries]]\nmedia_id = \"{}\"\n",
            env.toml("", 3600),
            png.display(),
        );
        let mut rt = env.runtime(&toml);
        let list = ok_json(&mut rt, Method::ListPlaylists, serde_json::json!({}));
        assert_eq!(
            list["playlists"].as_array().map(|v| v.len()),
            Some(0),
            "没有 [library] ⇒ 没有安全边界 ⇒ 不装载（不能凭配置里的路径就信任）"
        );
        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["verified"], true, "没装载也要能诚实报告保存结果");
        let after = std::fs::read_to_string(env.dir.join("config.toml")).expect("读配置");
        assert!(
            after.contains("[[playlists]]") && after.contains("pl-9"),
            "未装载的播放列表必须**原样保留**在配置里（不得被保存抹掉）：\n{after}"
        );
    }

    #[test]
    fn user_persona_add_source_bootstraps_and_widens_library() {
        let env = TestEnv::new("user-add-source");
        let dir_a = env.dir.join("wallpapers-a");
        let dir_b = env.dir.join("wallpapers-b");
        std::fs::create_dir_all(&dir_a).expect("建目录 A");
        std::fs::create_dir_all(&dir_b).expect("建目录 B");

        let mut rt = env.runtime(&env.toml("", 3600));
        rt.set_persona(liuguang_core::persona::Persona::User);
        assert!(rt.library().is_none(), "初始无 [library]");

        let r1 = ok_json(
            &mut rt,
            Method::AddSource,
            serde_json::json!({"kind": "image", "path": dir_a.display().to_string()}),
        );
        assert_eq!(r1["created"], true);
        assert!(
            rt.library().is_some(),
            "桌面首次添加目录应自动初始化 [library]"
        );

        let r2 = ok_json(
            &mut rt,
            Method::AddSource,
            serde_json::json!({"kind": "video", "path": dir_b.display().to_string()}),
        );
        assert_eq!(r2["created"], true);

        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["saved"], true);
        assert_eq!(saved["verified"], true);

        let mut rt2 = env.runtime_reload(&["DP-1"]);
        let ids = source_ids(&mut rt2);
        let canon_a = std::fs::canonicalize(&dir_a)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let canon_b = std::fs::canonicalize(&dir_b)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(ids.contains(&canon_a), "目录 A 应持久化: {ids:?}");
        assert!(ids.contains(&canon_b), "目录 B 应持久化: {ids:?}");
    }

    #[test]
    fn save_config_preserves_disk_library_when_runtime_started_without_one() {
        let env = TestEnv::new("save-preserves-disk-lib");
        seed_library(&env);
        let mut rt = env.runtime(&env.toml("", 3600));
        assert!(rt.library().is_none());

        // 模拟用户在守护进程运行期间在磁盘手工补写了 [library]。
        std::fs::write(env.dir.join("config.toml"), library_toml(&env, "")).expect("写磁盘配置");

        let saved = ok_json(&mut rt, Method::SaveConfig, serde_json::json!({}));
        assert_eq!(saved["saved"], true);
        let reloaded = Config::load(&env.dir.join("config.toml")).expect("配置应有效");
        assert!(
            reloaded.library.is_some(),
            "磁盘已有的 [library] 不得被内存空值覆盖"
        );
    }

    #[test]
    fn list_sources_and_media_hot_adopt_disk_library_without_restart() {
        let env = TestEnv::new("hot-adopt-disk-lib");
        seed_library(&env);
        let mut rt = env.runtime(&env.toml("", 3600));
        assert!(rt.library().is_none());

        // 用户未重启服务，直接往磁盘写了 [library] 段。
        std::fs::write(env.dir.join("config.toml"), library_toml(&env, "")).expect("写磁盘配置");

        let sources = ok_json(&mut rt, Method::ListSources, serde_json::json!({}));
        assert!(
            !sources["sources"].as_array().unwrap().is_empty(),
            "list_sources 应热加载磁盘上的 [library]: {sources:?}"
        );
        let media = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10}),
        );
        assert!(media.get("scanning").is_some(), "list_media 应正常进入扫描");
    }

    // ==================== T68：播放源 = 播放列表 ====================
    //
    // 需求原文：**"设计成只有加入播放列表的才会播放"**。
    //
    // 这些测试的共同点：每一条都能**失败** —— 把"active_playlist"改成永远
    // `None`（= 退回目录轮换）会让它们全红，而不是只断言"配置里存了值"。

    /// 造一条运行时播放列表（等价于 `update_playlist` 之后的状态）。
    fn pl_fixture(
        id: &str,
        paths: Vec<PathBuf>,
        mode: &str,
        on_video_end: &str,
        no_repeat: bool,
    ) -> liuguang_ipc::protocol::Playlist {
        liuguang_ipc::protocol::Playlist {
            id: id.to_string(),
            name: id.to_string(),
            entries: paths
                .into_iter()
                .map(|p| PlaylistEntry {
                    media_id: p.display().to_string(),
                })
                .collect(),
            mode: mode.to_string(),
            interval_seconds: 1,
            on_video_end: on_video_end.to_string(),
            output: None,
            no_repeat,
        }
    }

    /// 当前画面的文件名（断言用；`<none>` = 什么都没显示）。
    fn current_name(rt: &Runtime, key: &str) -> String {
        let media = current_media(rt, key);
        std::path::Path::new(&media)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(media)
    }

    /// 可注入 PTS 的假视频源：用来精确模拟**播完一轮**（源回卷 ⇒ PTS 跳回 0）。
    ///
    /// 为什么不复用 `StubVideo`：那个的 `last_pts` 恒为 `None`（"源没给 PTS"），
    /// 而 T68 的"视频播完"判定**只能**基于 PTS 回卷 —— 必须能控制它。
    struct PtsVideo {
        pts_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl liuguang_media::MediaSource for PtsVideo {
        fn metadata(&self) -> liuguang_core::types::MediaInfo {
            liuguang_core::types::MediaInfo {
                kind: MediaKind::Video,
                width: 16,
                height: 16,
                duration: Some(Duration::from_secs(4)),
                source_fps: Some(60.0),
                codec: Some("stub".into()),
            }
        }

        fn poll_frame(
            &mut self,
            _now: Instant,
        ) -> Result<liuguang_media::FramePoll, liuguang_media::MediaError> {
            // 只关心 PTS：不产帧（也避免真实提交干扰"当前项"断言）。
            Ok(liuguang_media::FramePoll::Pending {
                next_in: Some(Duration::from_millis(4)),
            })
        }

        fn pause(&mut self) -> Result<(), liuguang_media::MediaError> {
            Ok(())
        }

        fn resume(&mut self) -> Result<(), liuguang_media::MediaError> {
            Ok(())
        }
    }

    impl liuguang_media::video::VideoSource for PtsVideo {
        fn video_stats(&self) -> VideoStats {
            VideoStats {
                presented: 0,
                dropped_late: 0,
                dropped_queue_full: 0,
                queue_len: 0,
                skipped: 0,
                decoder: liuguang_media::video::DecoderKind::Software,
                source_fps: Some(60.0),
                last_pts: Some(Duration::from_millis(
                    self.pts_ms.load(std::sync::atomic::Ordering::Relaxed),
                )),
            }
        }

        fn next_deadline(&self) -> Option<Instant> {
            None
        }
    }

    /// 在通道上挂 `PtsVideo`，返回可外部写入的 PTS 句柄（毫秒）。
    fn attach_pts_video(
        rt: &mut Runtime,
        key: &str,
    ) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        let pts = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let now = Instant::now();
        let ch = rt.channels.get_mut(key).expect("通道");
        ch.video = Some(Box::new(PtsVideo {
            pts_ms: pts.clone(),
        }));
        ch.last_presented = 0;
        ch.last_presented_at = now;
        ch.stall_reported = false;
        ch.last_video_pts = None;
        pts
    }

    /// 带 `[library]` 的配置（`update_playlist` 需要媒体库边界才能校验条目）。
    fn toml_with_library(env: &TestEnv, interval_secs: u64) -> String {
        let dir = env.media_dir();
        format!(
            "{}\n[library]\nroot = \"{dir}\"\nimage_dirs = [\".\"]\nvideo_dirs = [\".\"]\nrecursive = false\n",
            env.toml("", interval_secs),
            dir = dir.display()
        )
    }

    /// **需求 ①/③ 的核心**：只有加入播放列表的才会播；目录里的其它媒体永不出现。
    ///
    /// 变异守卫：把 `build_playlist_for` 改成永远走目录（= T68 之前的真实行为）
    /// ⇒ 这里会看到 `dir-a.png`/`dir-b.png`，测试立刻红。
    #[test]
    fn playlist_plays_only_added_items_and_reports_source() {
        let env = TestEnv::new("t68-only-playlist");
        let _dir_a = env.png("dir-a.png", 16, 16, [255, 0, 0, 255]);
        let _dir_b = env.png("dir-b.png", 16, 16, [0, 255, 0, 255]);
        let c = env.png("only-c.png", 16, 16, [0, 0, 255, 255]);
        let d = env.png("only-d.png", 16, 16, [255, 255, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 1));
        // 目录里有 4 个文件，但只有 c/d 被加入播放列表。
        rt.playlists.push(pl_fixture(
            "pl-1",
            vec![c.clone(), d.clone()],
            "sequential",
            "next",
            false,
        ));
        rt.mark_playlist_dirty(None);
        let t0 = Instant::now();
        let mut seen: Vec<String> = Vec::new();
        for step in 0..8u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 2_000));
            let name = current_name(&rt, "DP-1");
            if name != "<none>" && !seen.contains(&name) {
                seen.push(name);
            }
        }
        assert!(
            seen.contains(&"only-c.png".to_string()) && seen.contains(&"only-d.png".to_string()),
            "播放列表里的两项都必须播到（轮换真的在走）：{seen:?}"
        );
        for forbidden in ["dir-a.png", "dir-b.png"] {
            assert!(
                !seen.contains(&forbidden.to_string()),
                "**只有加入播放列表的才会播**：{forbidden} 没被加入，不得出现（实际 {seen:?}）"
            );
        }
        // ② status 必须如实报告当前播放源（不是 "配置里写了什么"）。
        let st = status_json(&mut rt);
        assert_eq!(
            st["playback_source"], "playlist:pl-1",
            "status.playback_source 必须报告生效的播放源（实际 {st}）"
        );
    }

    /// **需求 ②：顺序循环** —— 按条目顺序前进，末尾回卷到第一项。
    ///
    /// 目录里刻意多放一个 `zz-decoy.png`（未加入列表）：如果实现退回目录轮换，
    /// 序列里会冒出它 —— 这条测试因此能**独立**抓出"忽略播放列表"。
    #[test]
    fn sequential_playlist_advances_in_order_and_wraps() {
        let env = TestEnv::new("t68-sequential");
        let e1 = env.png("e1.png", 16, 16, [1, 0, 0, 255]);
        let e2 = env.png("e2.png", 16, 16, [2, 0, 0, 255]);
        let e3 = env.png("e3.png", 16, 16, [3, 0, 0, 255]);
        let _decoy = env.png("zz-decoy.png", 16, 16, [99, 99, 99, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        rt.playlists.push(pl_fixture(
            "pl-seq",
            vec![e1, e2, e3],
            "sequential",
            "next",
            false,
        ));
        rt.mark_playlist_dirty(None);
        let mut order = Vec::new();
        for _ in 0..7 {
            rt.switch_next("DP-1", false).expect("切换必须成功");
            order.push(current_name(&rt, "DP-1"));
        }
        assert_eq!(
            order,
            vec![
                "e1.png", "e2.png", "e3.png", "e1.png", "e2.png", "e3.png", "e1.png"
            ],
            "顺序循环必须严格按条目顺序、末尾回卷（实际 {order:?}）"
        );
    }

    /// **需求 ③：随机播放** —— 随机前进，`no_repeat` 时相邻两项不同。
    ///
    /// 不给随机留"偶然通过"的余地：断言 31 次切换的**每对相邻项**都不同，
    /// 三项都出现过（否则"永远播同一项"也能骗过相邻断言），且**未加入列表的
    /// `zz-decoy.png` 一次都不许出现**（变异守卫：退回目录轮换会立刻红）。
    #[test]
    fn random_playlist_avoids_adjacent_repeats() {
        let env = TestEnv::new("t68-random");
        let e1 = env.png("r1.png", 16, 16, [1, 0, 0, 255]);
        let e2 = env.png("r2.png", 16, 16, [2, 0, 0, 255]);
        let e3 = env.png("r3.png", 16, 16, [3, 0, 0, 255]);
        let _decoy = env.png("zz-decoy.png", 16, 16, [99, 99, 99, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        rt.playlists.push(pl_fixture(
            "pl-rnd",
            vec![e1, e2, e3],
            "random",
            "next",
            true,
        ));
        rt.mark_playlist_dirty(None);
        let mut order = Vec::new();
        for _ in 0..31 {
            rt.switch_next("DP-1", false).expect("切换必须成功");
            order.push(current_name(&rt, "DP-1"));
        }
        for pair in order.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "no_repeat=true 时相邻两项不得相同（序列 {order:?}）"
            );
        }
        for want in ["r1.png", "r2.png", "r3.png"] {
            assert!(
                order.contains(&want.to_string()),
                "随机播放必须覆盖所有条目（{want} 未出现，序列 {order:?}）"
            );
        }
        assert!(
            !order.iter().any(|n| n == "zz-decoy.png"),
            "未加入播放列表的 zz-decoy.png 绝不允许出现（序列 {order:?}）"
        );
    }

    /// **需求 ①：单视频循环** —— `on_video_end=loop` 时视频播完**不前进**；
    /// 对照组 `on_video_end=next` 必须前进。
    ///
    /// "播完一轮"用 PTS 回卷注入（`VideoStats` 没有时长/EOS）。
    /// 变异守卫：把 `advances_on_video_end` 改成恒 true ⇒ (a) 通过、(b) 红；
    /// 改成恒 false ⇒ (a) 红。
    #[test]
    fn video_end_advances_only_when_on_video_end_is_next() {
        // (a) on_video_end = "next"：播完必须前进到下一项。
        let env = TestEnv::new("t68-video-end-next");
        let v1 = env.png("v1.png", 16, 16, [10, 0, 0, 255]);
        let v2 = env.png("v2.png", 16, 16, [20, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        rt.playlists.push(pl_fixture(
            "pl-next",
            vec![v1, v2],
            "sequential",
            "next",
            false,
        ));
        rt.mark_playlist_dirty(None);
        rt.switch_next("DP-1", false).expect("首项");
        assert_eq!(current_name(&rt, "DP-1"), "v1.png");
        let pts = attach_pts_video(&mut rt, "DP-1");
        let t = Instant::now();
        pts.store(1_000, std::sync::atomic::Ordering::Relaxed);
        rt.tick(t);
        assert_eq!(
            current_name(&rt, "DP-1"),
            "v1.png",
            "首次观察只是建立基线，不得判成播完"
        );
        pts.store(0, std::sync::atomic::Ordering::Relaxed); // 源回卷
        rt.tick(t + Duration::from_millis(100));
        assert_eq!(
            current_name(&rt, "DP-1"),
            "v2.png",
            "on_video_end=next：视频播完必须前进到下一项（实际 {}）",
            current_name(&rt, "DP-1")
        );

        // (b) on_video_end = "loop"：播完留在同一项（同一项继续播）。
        let env = TestEnv::new("t68-video-end-loop");
        let v1 = env.png("v1.png", 16, 16, [10, 0, 0, 255]);
        let v2 = env.png("v2.png", 16, 16, [20, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        rt.playlists.push(pl_fixture(
            "pl-loop",
            vec![v1, v2],
            "sequential",
            "loop",
            false,
        ));
        rt.mark_playlist_dirty(None);
        rt.switch_next("DP-1", false).expect("首项");
        let pts = attach_pts_video(&mut rt, "DP-1");
        let t = Instant::now();
        pts.store(1_000, std::sync::atomic::Ordering::Relaxed);
        rt.tick(t);
        pts.store(0, std::sync::atomic::Ordering::Relaxed);
        rt.tick(t + Duration::from_millis(100));
        assert_eq!(
            current_name(&rt, "DP-1"),
            "v1.png",
            "on_video_end=loop：视频播完**不得**前进（必须重复同一项）"
        );
        assert_eq!(
            rt.channels.get("DP-1").expect("通道").video_loops,
            1,
            "回卷必须被真的观察到（而不是靠'没检测到'通过）"
        );
        assert_eq!(status_json(&mut rt)["playback_source"], "playlist:pl-loop");
    }

    /// **需求 ②（边界）：播放列表为空 ⇒ 明确报错，绝不静默退回目录轮换。**
    #[test]
    fn empty_playlist_is_explicit_error_without_directory_fallback() {
        let env = TestEnv::new("t68-empty-playlist");
        let _a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 1));
        rt.playlists.push(pl_fixture(
            "pl-empty",
            Vec::new(),
            "sequential",
            "next",
            false,
        ));
        rt.mark_playlist_dirty(None);
        let err = rt
            .switch_next("DP-1", false)
            .expect_err("空播放列表必须报错，而不是悄悄播目录");
        assert_eq!(
            err.detail_code.as_deref(),
            Some("PLAYLIST_EMPTY"),
            "必须是可判别的机器码（实际 {err:?}）"
        );
        assert!(
            err.message.contains("pl-empty"),
            "错误信息必须指出是哪条播放列表：{}",
            err.message
        );
        assert!(
            rt.channels.get("DP-1").expect("通道").current.is_none(),
            "空播放列表下**不得**显示目录里的 a.png（静默降级）"
        );
        // 多 tick 之后仍然什么都不播（不是"第一次失败、第二次偷偷退回目录"）。
        let t0 = Instant::now();
        for step in 1..=6u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 2_000));
        }
        assert!(
            rt.channels.get("DP-1").expect("通道").current.is_none(),
            "多次轮换后仍不得退回目录（目录里确实有 a.png）"
        );
        // 来源标签要如实说"本该由这条播放列表驱动"。
        assert_eq!(status_json(&mut rt)["playback_source"], "playlist:pl-empty");
    }

    /// **需求 ②（边界）：条目全部失效 ⇒ 明确报错 + 指出具体条目，绝不退回目录。**
    #[test]
    fn all_playlist_entries_invalid_is_explicit_and_never_falls_back() {
        let env = TestEnv::new("t68-all-invalid");
        let _a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let ghost1 = env.media_dir().join("ghost-1.png");
        let ghost2 = env.media_dir().join("ghost-2.png");
        assert!(!ghost1.exists() && !ghost2.exists());
        let mut rt = env.runtime(&env.toml("", 1));
        rt.playlists.push(pl_fixture(
            "pl-bad",
            vec![ghost1, ghost2],
            "sequential",
            "next",
            false,
        ));
        rt.mark_playlist_dirty(None);
        let err = rt
            .switch_next("DP-1", false)
            .expect_err("全部条目失效必须报错");
        assert_eq!(
            err.detail_code.as_deref(),
            Some("PLAYLIST_ALL_INVALID"),
            "必须与'空列表'区分开（实际 {err:?}）"
        );
        assert!(
            err.message.contains("ghost-1.png"),
            "错误必须指出至少一条具体条目，便于用户修复：{}",
            err.message
        );
        assert!(
            rt.channels.get("DP-1").expect("通道").current.is_none(),
            "全部失效时不得显示目录里的 a.png"
        );
        let t0 = Instant::now();
        for step in 1..=6u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 2_000));
        }
        assert!(
            rt.channels.get("DP-1").expect("通道").current.is_none(),
            "多次 tick 后仍不得退回目录轮换"
        );
    }

    /// **接线**：`update_playlist` 之后必须真的按新列表播（不是"报告成功但没做"）。
    #[test]
    fn update_playlist_takes_effect_on_next_switch() {
        let env = TestEnv::new("t68-update-playlist");
        let _a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let only = env.png("only.png", 16, 16, [0, 255, 255, 255]);
        let mut rt = env.runtime(&toml_with_library(&env, 3600));
        assert_eq!(
            status_json(&mut rt)["playback_source"],
            "directory",
            "没有播放列表时必须是旧的目录轮换（需求 ③：旧配置行为不变）"
        );
        ok_json(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({
                "playlist_id": "pl-ipc",
                "name": "ipc",
                "entries": [{"media_id": only.display().to_string()}],
                "mode": "sequential",
                "interval_seconds": 1,
                "on_video_end": "next",
            }),
        );
        rt.switch_next("DP-1", false).expect("播放列表切换必须成功");
        assert_eq!(current_name(&rt, "DP-1"), "only.png");
        assert_eq!(
            status_json(&mut rt)["playback_source"],
            "playlist:pl-ipc",
            "update_playlist 之后 status 必须报告新的播放源"
        );
        // 目录里的 a.png 从此不会再出现。
        let t0 = Instant::now();
        for step in 1..=4u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 2_000));
            assert_eq!(
                current_name(&rt, "DP-1"),
                "only.png",
                "单条目播放列表轮换后仍是同一项，且绝不出现目录里的 a.png"
            );
        }
    }

    /// **真机 E2E 抓到的缺陷**：播放列表 + `on_video_end="loop"` 时，**定时器**
    /// 不得换片（否则"单视频循环"在真机上变成"每秒换一个"）。
    ///
    /// 变异守卫：去掉 `playlist_driven && SingleLoop` 的 continue ⇒ 本用例变红。
    /// 对照：**目录模式**（无播放列表）仍必须按 interval 轮换（旧行为不能退化）。
    #[test]
    fn playlist_single_loop_is_not_advanced_by_the_interval_timer() {
        let env = TestEnv::new("t68-loop-no-timer");
        let a = env.png("a.png", 16, 16, [7, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [8, 0, 0, 255]);
        // slideshow interval = 1s（若定时器仍生效，1s 后就会换片）。
        let mut rt = env.runtime(&env.toml("", 1));
        rt.playlists.push(pl_fixture(
            "pl-loop-timer",
            vec![a.clone(), b.clone()],
            "sequential",
            "loop",
            false,
        ));
        rt.mark_playlist_dirty(None);
        rt.switch_next("DP-1", false).expect("首项");
        let first = current_name(&rt, "DP-1");
        let t0 = Instant::now();
        for step in 1..=6u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 1_000));
            assert_eq!(
                current_name(&rt, "DP-1"),
                first,
                "单视频循环：定时器**不得**推进（第 {step} 秒就换片了）"
            );
        }

        // 对照：目录模式（无播放列表）必须照旧按 interval 轮换。
        let env2 = TestEnv::new("t68-dir-timer-control");
        let _a = env2.png("a.png", 16, 16, [7, 0, 0, 255]);
        let _b = env2.png("b.png", 16, 16, [8, 0, 0, 255]);
        let mut rt2 = env2.runtime(&env2.toml("", 1));
        let t1 = Instant::now();
        let mut seen = Vec::new();
        for step in 0..4u32 {
            rt2.tick(t1 + Duration::from_millis(u64::from(step) * 1_100));
            let n = current_name(&rt2, "DP-1");
            if n != "<none>" && !seen.contains(&n) {
                seen.push(n);
            }
        }
        assert!(
            seen.len() >= 2,
            "目录模式必须仍然按 interval 轮换（不能因为本卡而退化）：{seen:?}"
        );
    }

    /// **用户报的缺口**：`set` 一个视频/图片之后，说了"只放这一项"，就不得再被
    /// 定时器换走（否则单视频循环、单图常驻都做不到）。
    ///
    /// 变异守卫：去掉轮换 tick 里的 `if ch.pinned { continue; }` ⇒ 1 秒后换片，本用例变红。
    #[test]
    fn pinned_media_is_not_advanced_by_the_interval_timer() {
        let env = TestEnv::new("pinned-no-timer");
        let a = env.png("a.png", 16, 16, [7, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [8, 0, 0, 255]);
        let _ = &b;
        let mut rt = env.runtime(&env.toml("", 1));
        age_startup(&mut rt);
        // 先让目录轮换跑起来（首帧立即显示）。
        let t0 = Instant::now();
        rt.tick(t0);
        assert_ne!(current_name(&rt, "DP-1"), "<none>", "首帧应立即显示");
        // 锁定 a.png（等价于 `liuguangctl set a.png --pin`）。
        let resp = call(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({"path": a.display().to_string(), "pin": true}),
        );
        assert!(resp.ok, "set --pin 应成功: {:?}", resp.error);
        assert_eq!(current_name(&rt, "DP-1"), "a.png");
        let pinned = ok_json(&mut rt, Method::Status, serde_json::json!({}));
        assert_eq!(
            pinned["channels"][0]["pinned"], true,
            "status 必须上报 pinned=true（GUI 靠它回显勾选框）"
        );
        for step in 1..=6u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 1_000));
            assert_eq!(
                current_name(&rt, "DP-1"),
                "a.png",
                "锁定后定时器不得换片（第 {step} 秒被换走了）"
            );
        }
        // 解除锁定 → 恢复轮换（否则用户没法回到轮换模式）。
        let off = call(
            &mut rt,
            Method::SetPinned,
            serde_json::json!({"pinned": false}),
        );
        assert!(off.ok, "解除锁定应成功: {:?}", off.error);
        let mut seen = Vec::new();
        for step in 7..=14u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 1_000));
            let n = current_name(&rt, "DP-1");
            if !seen.contains(&n) {
                seen.push(n);
            }
        }
        assert!(seen.len() >= 2, "解除锁定后必须恢复轮换：{seen:?}");
    }

    /// **真机 E2E 抓到的第二个缺陷**：轮换节奏必须来自**播放列表自己的**
    /// `interval_seconds`，而不是 `[slideshow].interval_seconds`。
    ///
    /// 变异守卫：`effective_interval` 改回只看 `channel_settings` ⇒ 本用例变红
    /// （列表写 30s、slideshow 写 1s，结果 1s 就换片）。
    #[test]
    fn playlist_interval_seconds_governs_rotation_not_slideshow() {
        let env = TestEnv::new("t68-playlist-interval");
        let a = env.png("a.png", 16, 16, [7, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [8, 0, 0, 255]);
        // slideshow = 1s（陷阱），播放列表 = 30s（应以此为准）。
        let mut rt = env.runtime(&env.toml("", 1));
        let mut pl = pl_fixture("pl-slow", vec![a, b], "sequential", "next", false);
        pl.interval_seconds = 30;
        rt.playlists.push(pl);
        rt.mark_playlist_dirty(None);
        rt.switch_next("DP-1", false).expect("首项");
        let first = current_name(&rt, "DP-1");
        let t0 = Instant::now();
        for step in 1..=5u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 1_000));
            assert_eq!(
                current_name(&rt, "DP-1"),
                first,
                "播放列表写 30s ⇒ 5s 内不得换片（说明用的是 slideshow 的 1s，第 {step} 秒就换了）"
            );
        }
    }

    /// **真机 E2E 抓到的第三个缺陷**：从 `loop` 切到 `random` 后必须**立刻**换模式。
    ///
    /// `loop` 既不按定时器轮换、也不在视频播完时前进 ⇒ 永远不会触发"下次切换" ⇒
    /// 若只在切换时重建游标，用户切到随机播放后会**卡在单视频循环**上。
    ///
    /// 变异守卫：把 `apply_playlist_change` 换回 `mark_playlist_dirty` ⇒ 本用例变红。
    #[test]
    fn switching_from_loop_to_random_takes_effect_immediately() {
        let env = TestEnv::new("t68-loop-to-random");
        let a = env.png("a.png", 16, 16, [1, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [2, 0, 0, 255]);
        let c = env.png("c.png", 16, 16, [3, 0, 0, 255]);
        let mut rt = env.runtime(&toml_with_library(&env, 3600));
        let entries: Vec<serde_json::Value> = [&a, &b, &c]
            .iter()
            .map(|p| serde_json::json!({ "media_id": p.display().to_string() }))
            .collect();
        // 先切到"单视频循环"。
        ok_json(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({
                "playlist_id": "pl-mix", "name": "mix", "entries": entries,
                "mode": "sequential", "interval_seconds": 1, "on_video_end": "loop",
            }),
        );
        assert_eq!(
            rt.channels.get("DP-1").expect("通道").mode,
            PlaybackMode::SingleLoop,
            "先进入单视频循环"
        );
        rt.switch_next("DP-1", false).expect("首项");
        let first = current_name(&rt, "DP-1");

        // 切到"随机播放"（`mode=random` + `on_video_end=next`）。
        ok_json(
            &mut rt,
            Method::UpdatePlaylist,
            serde_json::json!({
                "playlist_id": "pl-mix", "name": "mix", "entries": entries,
                "mode": "random", "interval_seconds": 1, "on_video_end": "next",
                "no_repeat": true,
            }),
        );
        let ch = rt.channels.get("DP-1").expect("通道");
        assert!(
            matches!(ch.mode, PlaybackMode::Random { .. }),
            "update_playlist 之后模式必须**立即**变成随机（实际 {:?}）；\
             否则 loop 模式下永远不会触发‘下次切换’，就卡死了",
            ch.mode
        );
        // 随机模式下，视频播完必须前进（不再停在 first）。
        let pts = attach_pts_video(&mut rt, "DP-1");
        let t = Instant::now();
        pts.store(1_000, std::sync::atomic::Ordering::Relaxed);
        rt.tick(t);
        pts.store(0, std::sync::atomic::Ordering::Relaxed);
        rt.tick(t + Duration::from_millis(100));
        assert_ne!(
            current_name(&rt, "DP-1"),
            "<none>",
            "随机模式下播完必须切到某一项（不能停摆）"
        );
        let _ = first;
    }

    /// **需求 ③：老配置（没有播放列表）行为不变** —— 目录轮换照旧，
    /// 且目录模式的视频"播完"仍然由生产者自己 loop（daemon 不前进）。
    #[test]
    fn legacy_directory_rotation_is_unchanged() {
        let env = TestEnv::new("t68-legacy");
        let _a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let _b = env.png("b.png", 16, 16, [0, 255, 0, 255]);
        let mut rt = env.runtime(&env.toml("", 1));
        assert_eq!(status_json(&mut rt)["playback_source"], "directory");
        let t0 = Instant::now();
        let mut seen: Vec<String> = Vec::new();
        for step in 0..6u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 2_000));
            let name = current_name(&rt, "DP-1");
            if name != "<none>" && !seen.contains(&name) {
                seen.push(name);
            }
        }
        assert!(
            seen.contains(&"a.png".to_string()) && seen.contains(&"b.png".to_string()),
            "老配置的目录轮换必须照旧工作（实际 {seen:?}）"
        );
        assert_eq!(
            rt.channels.get("DP-1").expect("通道").mode,
            PlaybackMode::SingleLoop,
            "目录模式 = 旧语义：视频播完由生产者自己 loop，daemon 不在 PTS 回卷时前进"
        );
    }

    /// **真实用户路径**：配置里的 `[[playlists]]` 装载后必须驱动播放
    /// （不是只出现在 `list_playlists` 里）。
    #[test]
    fn config_playlist_drives_playback_and_ignores_directory() {
        let env = TestEnv::new("t68-config-playlist");
        let _dir_a = env.png("dir-a.png", 16, 16, [255, 0, 0, 255]);
        let picked = env.png("picked.png", 16, 16, [9, 9, 9, 255]);
        let toml = format!(
            "{base}\n[[playlists]]\nid = \"pl-cfg\"\nmode = \"sequential\"\non_video_end = \"next\"\ninterval_seconds = 1\n\n[[playlists.entries]]\nmedia_id = \"{picked}\"\n",
            base = toml_with_library(&env, 1),
            picked = picked.display()
        );
        let mut rt = env.runtime(&toml);
        age_startup(&mut rt);
        assert!(
            rt.playlists_loaded,
            "有 [library] 时配置里的播放列表必须被装载"
        );
        assert_eq!(rt.playlists.len(), 1);
        assert_eq!(status_json(&mut rt)["playback_source"], "playlist:pl-cfg");
        // 上面的 status 查询会让启动首帧让路（t46），这里模拟"没人指挥"的真启动。
        rt.last_request_at = None;
        let t0 = Instant::now();
        for step in 0..5u32 {
            rt.tick(t0 + Duration::from_millis(u64::from(step) * 2_000));
            assert_eq!(
                current_name(&rt, "DP-1"),
                "picked.png",
                "配置里的播放列表必须驱动播放（目录里的 dir-a.png 不得出现）"
            );
        }
    }

    #[test]
    fn parse_fit_accepts_legacy_capitalized_state_names() {
        for (name, expected) in [
            ("cover", "cover"),
            ("Cover", "cover"),
            ("contain", "contain"),
            ("Contain", "contain"),
            ("stretch", "stretch"),
            ("Stretch", "stretch"),
            ("center", "center"),
            ("Center", "center"),
        ] {
            assert_eq!(
                parse_fit(name).map(|f| f.as_str()),
                Some(expected),
                "legacy fit name {name} must parse"
            );
        }
        assert_eq!(parse_fit("bogus"), None);
    }

    // ================= t4：媒体详情 / 定点缩略图 / 删除媒体（不删磁盘） / 删除列表 =================

    /// 扫描期必须填充 `details`（图片读文件头）；**解不出的媒体保留在索引里**，
    /// 详情缺失（而不是扫描错误）才是正确结果。
    #[test]
    fn scanned_media_carries_header_details() {
        let env = TestEnv::new("t4-details");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": true}),
        );
        assert!(wait_for_scan(&mut rt, Duration::from_secs(10)));
        let page = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 20}),
        );
        let items = page["items"].as_array().expect("items");
        let png = items
            .iter()
            .find(|i| i["name"] == "a.png")
            .expect("a.png 必须在索引里");
        assert_eq!(png["details"]["width"], 8, "PNG 宽高来自 IHDR: {png}");
        assert_eq!(png["details"]["height"], 8);
        assert_eq!(png["details"]["codec"], "png");
        assert_eq!(png["details"]["pix_fmt"], "rgba8");
        assert!(
            png["details"]["fps_millihz"].is_null(),
            "图片不该有帧率: {}",
            png["details"]
        );
        // `seed_library` 的 c.mp4 是垃圾字节：解不出头 ⇒ 字段整体缺省，且不进索引错误。
        let fake = items
            .iter()
            .find(|i| i["name"] == "c.mp4")
            .expect("解不出详情的媒体仍须留在索引里");
        assert!(
            fake.get("details").is_none(),
            "详情缺失应省略字段（`skip_serializing_if`），而不是填 null: {fake}"
        );
        assert!(
            rt.media.errors.is_empty(),
            "详情探测失败不得污染扫描错误列表: {:?}",
            rt.media.errors
        );
    }

    /// `get_media` 与扫描同口径地填 `details`（GUI 属性面板直接用它）。
    #[test]
    fn get_media_carries_header_details() {
        let env = TestEnv::new("t4-get-media-details");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let a = env.media_dir().join("images/a.png");
        let r = ok_json(
            &mut rt,
            Method::GetMedia,
            serde_json::json!({"id": a.display().to_string()}),
        );
        assert_eq!(r["item"]["kind"], "image");
        assert_eq!(r["item"]["details"]["width"], 8);
        assert_eq!(r["item"]["details"]["height"], 8);
        assert_eq!(r["item"]["details"]["codec"], "png");
    }

    /// `get_thumbnail` 的 `at_ms` 必须真的被解析：合法值（含省略）不得被当成参数错误，
    /// 负数必须拒绝；成功时仍是 PNG + base64 的形状。
    #[test]
    fn get_thumbnail_validates_at_ms_and_keeps_png_shape() {
        let env = TestEnv::new("t4-thumb-at");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        let a = env.media_dir().join("images/a.png");
        let a_id = a.display().to_string();
        for at in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(500),
        ] {
            let mut params = serde_json::json!({"id": a_id});
            if !at.is_null() {
                params["at_ms"] = at.clone();
            }
            let r = call_async(&mut rt, Method::GetThumbnail, params.clone());
            assert!(r.ok, "合法 at_ms 必须能取到缩略图: {params} / {r:?}");
            let body = r.result.expect("成功必须带结果");
            assert_eq!(body["format"], "png");
            assert!(body["width"].as_u64().unwrap_or(0) > 0);
            assert!(!body["data_b64"].as_str().unwrap_or("").is_empty());
        }
        // 负数 / 非整数：参数错误（证明字段被 handler 读取，而不是被整体忽略）。
        for bad in [serde_json::json!(-1), serde_json::json!("500")] {
            let r = call(
                &mut rt,
                Method::GetThumbnail,
                serde_json::json!({"id": a_id, "at_ms": bad}),
            );
            assert!(!r.ok, "非法 at_ms 必须拒绝: {bad}");
        }
    }

    /// `delete_media` 的契约：**只从索引里移除，磁盘文件保留**；幂等；相对路径拒绝。
    #[test]
    fn delete_media_removes_index_entry_but_keeps_file() {
        let env = TestEnv::new("t4-delete-media");
        seed_library(&env);
        let mut rt = env.runtime(&library_toml(&env, ""));
        ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": true}),
        );
        assert!(wait_for_scan(&mut rt, Duration::from_secs(10)));
        let a = env.media_dir().join("images/a.png");
        let a_id = a.display().to_string();
        let before = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 20}),
        );
        assert_eq!(before["total"], 3, "前置条件：2 图 + 1 视频");

        let r = ok_json(
            &mut rt,
            Method::DeleteMedia,
            serde_json::json!({"path": a_id}),
        );
        assert_eq!(r["removed"], true, "必须如实报告已移除: {r}");
        assert_eq!(r["removed_count"], 1);
        assert!(
            a.exists(),
            "delete_media 语义是「从媒体库索引移除」，磁盘文件必须原样保留"
        );
        let after = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 20}),
        );
        assert_eq!(after["total"], 2, "索引里必须少一条: {after}");
        assert!(
            after["items"]
                .as_array()
                .expect("items")
                .iter()
                .all(|i| i["path"] != a_id.as_str()),
            "被删的条目不得再出现在列表里: {after}"
        );

        // 幂等：再删一次不是错误，只是 removed=false。
        let again = ok_json(
            &mut rt,
            Method::DeleteMedia,
            serde_json::json!({"path": a_id}),
        );
        assert_eq!(again["removed"], false);
        assert_eq!(again["removed_count"], 0);
        assert_eq!(again["switched_outputs"], serde_json::json!([]));

        // 相对路径没有可比较的语义 ⇒ 参数错误（而不是静默不动）。
        let rel = call(
            &mut rt,
            Method::DeleteMedia,
            serde_json::json!({"path": "images/a.png"}),
        );
        assert!(!rel.ok);
        assert_eq!(
            rel.error.as_ref().map(|e| e.code),
            Some(ErrorKind::InvalidParams)
        );
    }

    /// 正在播的媒体被删：必须先切走，并在 `switched_outputs` 里**如实**报告；
    /// 同时引用它的播放列表条目也要清掉（否则"从库里删了"还能被列表播出来）。
    #[test]
    fn delete_media_switches_away_and_strips_playlist_entries() {
        let env = TestEnv::new("t4-delete-playing");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [0, 0, 255, 255]);
        // 用 `[library]` 让索引里**真有**这两条（`removed` 的语义是"从索引移除"），
        // 同时用播放列表把播放源钉在 a.png 上。
        let mut rt = env.runtime(&toml_with_library(&env, 1));
        ok_json(
            &mut rt,
            Method::ScanSource,
            serde_json::json!({"full": true}),
        );
        assert!(wait_for_scan(&mut rt, Duration::from_secs(10)));
        rt.playlists.push(pl_fixture(
            "pl-1",
            vec![a.clone(), b.clone()],
            "sequential",
            "next",
            false,
        ));
        ok_json(&mut rt, Method::Next, serde_json::json!({}));
        assert_eq!(current_name(&rt, "DP-1"), "a.png", "前置条件：正在播 a.png");

        let r = ok_json(
            &mut rt,
            Method::DeleteMedia,
            serde_json::json!({"path": a.display().to_string()}),
        );
        assert_eq!(r["removed"], true);
        assert_eq!(
            r["switched_outputs"],
            serde_json::json!(["DP-1"]),
            "正在播的通道必须被切走并如实上报: {r}"
        );
        assert_eq!(
            current_name(&rt, "DP-1"),
            "b.png",
            "删掉正在播的画面后必须换到下一条"
        );
        assert!(a.exists(), "磁盘上的 a.png 必须保留");

        let pls = ok_json(&mut rt, Method::ListPlaylists, serde_json::json!({}));
        let entries = pls["playlists"][0]["entries"]
            .as_array()
            .expect("entries")
            .clone();
        assert_eq!(entries.len(), 1, "引用被删媒体的条目必须清掉: {pls}");
        assert_eq!(entries[0]["media_id"], b.display().to_string());
    }

    /// `delete_playlist`：只删"怎么播"的定义，媒体文件与其它列表不受影响；
    /// 正被使用的列表删掉后，受影响通道**立即**回到目录轮换并上报 rebuild。
    #[test]
    fn delete_playlist_removes_definition_and_keeps_media() {
        let env = TestEnv::new("t4-delete-playlist");
        let a = env.png("a.png", 16, 16, [255, 0, 0, 255]);
        let b = env.png("b.png", 16, 16, [0, 0, 255, 255]);
        let mut rt = env.runtime(&env.toml("", 1));
        rt.playlists.push(pl_fixture(
            "pl-1",
            vec![a.clone(), b.clone()],
            "sequential",
            "next",
            false,
        ));
        ok_json(&mut rt, Method::Next, serde_json::json!({}));
        assert_eq!(current_name(&rt, "DP-1"), "a.png");

        let r = ok_json(
            &mut rt,
            Method::DeletePlaylist,
            serde_json::json!({"playlist_id": "pl-1"}),
        );
        assert_eq!(r["removed"], true);
        assert_eq!(r["playlist_id"], "pl-1");
        assert_eq!(
            r["rebuilt_outputs"],
            serde_json::json!(["DP-1"]),
            "受影响通道必须立即重建（不能等「下次切换」）: {r}"
        );
        assert!(a.exists() && b.exists(), "删除播放列表不得删媒体文件");
        let pls = ok_json(&mut rt, Method::ListPlaylists, serde_json::json!({}));
        assert_eq!(
            pls["playlists"].as_array().map(|v| v.len()),
            Some(0),
            "定义必须真的消失: {pls}"
        );
        // 回到目录轮换：再 next 能出画面，而不是报"播放列表为空"。
        ok_json(&mut rt, Method::Next, serde_json::json!({}));
        let shown = current_name(&rt, "DP-1");
        assert!(
            shown == "a.png" || shown == "b.png",
            "删掉列表后应回到目录轮换，实际 {shown}"
        );

        // 幂等：删不存在的列表 ⇒ removed=false（不是错误）。
        let again = ok_json(
            &mut rt,
            Method::DeletePlaylist,
            serde_json::json!({"playlist_id": "pl-1"}),
        );
        assert_eq!(again["removed"], false);
        assert_eq!(again["rebuilt_outputs"], serde_json::json!([]));
        assert!(!call(&mut rt, Method::DeletePlaylist, serde_json::json!({})).ok);
    }

    // ===== t6：能力边界的产品化可见性 =====

    fn media_info_of(width: u32, height: u32, codec: &str) -> liuguang_core::types::MediaInfo {
        liuguang_core::types::MediaInfo {
            kind: MediaKind::Video,
            width,
            height,
            duration: None,
            source_fps: Some(30.0),
            codec: Some(codec.to_string()),
        }
    }

    #[test]
    fn eight_k_h264_alert_fires_only_for_over_four_k_h264() {
        let missing = std::path::Path::new("/nonexistent/8k.mp4");
        // 8K H.264（NVDEC 的 H.264 上限是 4096 宽）→ 必须告警，且文案要能指导用户。
        let alert = eight_k_h264_alert(missing, &media_info_of(7680, 4320, "h264"))
            .expect("8K H.264 必须有告警");
        assert!(alert.contains("7680×4320"), "{alert}");
        assert!(alert.contains("软件解码"), "{alert}");
        assert!(alert.contains("HEVC"), "必须给出替代方案: {alert}");
        // 具体性能数字随机器/素材变化，不得写死在面向用户的文案里（t21 F5）。
        for banned in ["fps", "GB", "核", "%"] {
            assert!(!alert.contains(banned), "文案不该出现 {banned}: {alert}");
        }
        // 同一分辨率换成 HEVC：8K HEVC 可硬解 ⇒ 不告警。
        assert!(eight_k_h264_alert(missing, &media_info_of(7680, 4320, "hevc")).is_none());
        // 4K H.264 能硬解 ⇒ 不告警（4096 是上限，不是"超过 4K 才"）。
        assert!(eight_k_h264_alert(missing, &media_info_of(3840, 2160, "h264")).is_none());
        assert!(eight_k_h264_alert(missing, &media_info_of(4096, 2160, "h264")).is_none());
    }

    #[test]
    fn container_link_failures_become_actionable_messages() {
        // qtdemux 的 not-linked 原文不能直接抛给用户（t21 F3）。
        let path = std::path::Path::new("/media/a.mp4");
        let mapped =
            friendly_decode_error(path, "qtdemux1: streaming stopped, reason not-linked (-1)");
        assert!(
            mapped.contains("4:2:2") && mapped.contains("4:4:4"),
            "{mapped}"
        );
        assert!(mapped.contains("ffmpeg"), "要给出可执行的命令: {mapped}");
        assert!(
            !mapped.contains("qtdemux"),
            "不该把容器原文抛给用户: {mapped}"
        );
        assert!(!mapped.contains("not-linked"), "{mapped}");
        // 普通错误保留原意并带上路径。
        let other = friendly_decode_error(path, "解码器缺失");
        assert!(other.contains("解码器缺失"), "{other}");
        assert!(other.contains("/media/a.mp4"), "{other}");
    }

    #[test]
    fn oversize_media_is_visible_in_list_media_and_status() {
        // 体积上限是**配置边界**：被跳过的文件必须能被用户看见（旧实现静默丢弃）。
        let env = TestEnv::new("oversize");
        seed_library(&env);
        let big = env.media_dir().join("videos/8k-raw.mp4");
        std::fs::write(&big, vec![0u8; 8192]).expect("写大文件");
        // 上限压到 1 KiB（真实默认 2 GiB，这里只是为了在测试里越界）。
        let toml = library_toml(&env, "max_file_bytes = 1024");
        let mut rt = env.runtime(&toml);
        ok_json(&mut rt, Method::ScanSource, serde_json::json!({}));
        assert!(wait_for_scan(&mut rt, Duration::from_secs(5)), "扫描未完成");

        let page = ok_json(
            &mut rt,
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 50}),
        );
        let skipped = page["skipped_oversize"].as_u64().unwrap_or(0);
        assert!(
            skipped >= 1,
            "超限文件必须被计数（list_media 可见）: {page}"
        );
        let examples = page["skipped_examples"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(!examples.is_empty(), "必须给出可操作样例: {page}");
        let joined = examples
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(joined.contains("8k-raw.mp4"), "样例要指认文件: {joined}");
        assert!(
            joined.contains("文件过大") && joined.contains("1024"),
            "样例要带上限数值（可操作）: {joined}"
        );
        // 被跳过的文件不得进索引（否则"能看见却播不了"更糟）。
        let names: Vec<&str> = page["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|i| i["name"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            !names.contains(&"8k-raw.mp4"),
            "超限文件不进索引: {names:?}"
        );

        // status 也要能看出来（没有降级原因时，state_hint 回退到媒体提示）。
        let status = ok_json(&mut rt, Method::Status, serde_json::json!({}));
        let hint = status["state_hint"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            hint.contains("超过单文件上限"),
            "status.state_hint 必须说明体积跳过: {hint}"
        );
        assert!(big.exists(), "跳过不等于删除");
    }

    // ===== t15：自动兼容（能力判定 + 后台转码缓存） =====

    /// 团队生成的素材矩阵目录（0.3.0 起为 `.work/corpus/matrix`）。
    fn matrix_dir() -> std::path::PathBuf {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work");
        let dir = base.join("corpus/matrix");
        // 规范化：`[library]` 的路径校验拒绝含 `..` 的相对路径，而
        // `CARGO_MANIFEST_DIR/../..` 正好带 `..`。
        std::fs::canonicalize(&dir).unwrap_or(dir)
    }

    /// 矩阵里的一个片段；缺失时相关测试跳过（CI 不生成素材）。
    fn matrix_clip(name: &str) -> Option<std::path::PathBuf> {
        let path = matrix_dir().join(name);
        path.exists().then_some(path)
    }

    impl TestEnv {
        /// 允许 `media_dir` **加一个额外目录**的配置（素材矩阵在仓库里，
        /// 不在测试临时目录里，得显式放行）。
        ///
        /// `media_lines` 直接塞进 `[media]` 表（例如 `transcode = "off"`）。
        fn toml_allowing(&self, extra: &std::path::Path, media_lines: &str) -> String {
            let dir = self.media_dir();
            format!(
                "[display]\nbackend = \"drm\"\n\n\
                 [media]\n{media_lines}allowed_roots = [\"{dir}\", \"{extra}\"]\n\
                 video_fps_limit = 30\n\n\
                 [library]\nroot = \"{extra}\"\nvideo_dirs = [\"{extra}\"]\n\
                 image_dirs = []\nrecursive = true\n\n\
                 [slideshow]\nenabled = false\ndirectory = \"{dir}\"\n\
                 interval_seconds = 1\norder = \"sequential\"\n\n\
                 [transition]\nenabled = false\neffect = \"fade\"\nduration_ms = 0\n",
                dir = dir.display(),
                extra = extra.display(),
            )
        }
    }

    #[test]
    fn prepare_media_re_transcodes_after_the_source_is_touched() {
        // t21 F4：缓存 key 含 mtime，但 `prepare_media` 曾直接复用旧任务视图 ⇒
        // touch 源文件后仍报 ready 且指向上一版缓存。修法是"key 不一致就不复用"。
        let Some(clip) = matrix_clip("matrix-7680x4320-30fps-h264-8bit.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let env = TestEnv::new("t24-stale-cache");
        let source = env.media_dir().join("stale-cache.mp4");
        std::fs::copy(&clip, &source).expect("复制素材");
        let script = env.media_dir().parent().unwrap().join("fake-ffmpeg.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nfor last in \"$@\"; do :; done\n\
             printf 'out_time_ms=1000000\\nprogress=continue\\n'\n\
             printf 'fake-cache' > \"$last\"\nexit 0\n",
        )
        .expect("写假 ffmpeg");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let toml = env.toml_allowing(
            &matrix_dir(),
            &format!("transcode = \"{}\"\n", script.display()),
        );
        let mut rt = env.runtime(&toml);

        let first = prepare_until(&mut rt, &source, std::time::Duration::from_secs(20));
        let first_cache = first
            .get("cache_path")
            .and_then(|value| value.as_str())
            .expect("ready 应带缓存路径")
            .to_string();
        assert_eq!(first["state"].as_str(), Some("ready"), "{first}");

        // 只改 mtime（内容不变）：缓存 key 必须跟着变。
        std::thread::sleep(std::time::Duration::from_millis(20));
        let touched = std::fs::File::options()
            .write(true)
            .open(&source)
            .expect("打开源文件");
        touched
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1))
            .expect("touch 源文件");
        drop(touched);

        let again = call(
            &mut rt,
            Method::PrepareMedia,
            serde_json::json!({ "path": source.display().to_string() }),
        );
        assert!(again.ok, "{:?}", again.error);
        let payload = again.result.expect("应有结果");
        let state = payload["state"].as_str().unwrap_or_default().to_string();
        let cache = payload["cache_path"].as_str().map(str::to_string);
        assert_ne!(
            (state.as_str(), cache.as_deref()),
            ("ready", Some(first_cache.as_str())),
            "touch 后不得继续复用旧缓存：{payload}"
        );

        let second = prepare_until(&mut rt, &source, std::time::Duration::from_secs(20));
        let second_cache = second
            .get("cache_path")
            .and_then(|value| value.as_str())
            .expect("ready 应带缓存路径")
            .to_string();
        assert_ne!(second_cache, first_cache, "key 变了，缓存路径也应不同");
        assert!(
            std::path::Path::new(&first_cache).exists(),
            "旧缓存文件应留在原地不删"
        );
    }

    /// 轮询 `prepare_media` 直到 ready（后台线程 + tick 推进转码）。
    fn prepare_until(
        rt: &mut Runtime,
        source: &std::path::Path,
        timeout: std::time::Duration,
    ) -> serde_json::Value {
        let deadline = std::time::Instant::now() + timeout;
        let mut last = serde_json::Value::Null;
        while std::time::Instant::now() < deadline {
            rt.tick(std::time::Instant::now());
            let response = call(
                rt,
                Method::PrepareMedia,
                serde_json::json!({ "path": source.display().to_string() }),
            );
            if let Some(payload) = response.result {
                let state = payload["state"].as_str().unwrap_or_default().to_string();
                if state == "ready" {
                    return payload;
                }
                last = payload;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("转码没完成: {last}");
    }

    #[test]
    fn prepare_media_says_ready_for_a_playable_clip() {
        let Some(clip) = matrix_clip("matrix-1920x1080-30fps-h264-8bit.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let env = TestEnv::new("t15-prepare-playable");
        let toml = env.toml_allowing(&matrix_dir(), "");
        let mut rt = env.runtime(&toml);
        let value = ok_json(
            &mut rt,
            Method::PrepareMedia,
            serde_json::json!({"path": clip.display().to_string()}),
        );
        assert_eq!(value["state"], "ready", "8bit 1080p H.264 应直接可播");
        assert_eq!(value["progress_permille"], 1000);
        assert!(
            value["message"]
                .as_str()
                .unwrap_or_default()
                .contains("无需转码"),
            "{}",
            value["message"]
        );
        // 不需要缓存 ⇒ 不下发 cache_path（省得 GUI 去打开一个不存在的文件）。
        assert!(value.get("cache_path").is_none(), "{value}");
    }

    #[test]
    fn get_media_reports_the_compatibility_of_each_clip() {
        let Some(ten_bit) = matrix_clip("matrix-3840x2160-30fps-hevc-10bit-p010.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let Some(plain) = matrix_clip("matrix-1920x1080-30fps-h264-8bit.mp4") else {
            return;
        };
        let env = TestEnv::new("t15-get-media-compat");
        let toml = env.toml_allowing(&matrix_dir(), "");
        let mut rt = env.runtime(&toml);

        let value = ok_json(
            &mut rt,
            Method::GetMedia,
            serde_json::json!({"id": ten_bit.display().to_string()}),
        );
        assert_eq!(value["compat"]["state"], "needs_conversion");
        // t37：target 跟着**该输出实际使用的解码路径**（mock 后端 ⇒ GStreamer ⇒ h264/mp4），
        // 不再是设备级探测出来的 hevc/mkv。
        let target = value["compat"]["target"].as_str().unwrap_or_default();
        assert_eq!(target, "h264 8bit (mp4)");
        assert!(
            value["compat"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("10bit"),
            "{}",
            value["compat"]["reason"]
        );
        // details 依旧是"文件头事实"，compat 是"相对本机的判断"。
        assert_eq!(value["item"]["details"]["codec"], "hevc");
        assert_eq!(value["item"]["details"]["width"], 3840);

        let value = ok_json(
            &mut rt,
            Method::GetMedia,
            serde_json::json!({"id": plain.display().to_string()}),
        );
        assert_eq!(value["compat"]["state"], "playable");
        assert!(
            value["compat"].get("target").is_none(),
            "{}",
            value["compat"]
        );
    }

    /// 组装一个"宽度超过 8K 硬解上限"的最小 MP4（只有 ftyp + moov，没有码流）。
    ///
    /// 目的不是解码，而是让 `probe` 从容器头读出 `9216x9216`，从而命中
    /// `Compatibility::Unsupported`——这条路径必须**在入队之前**就失败。
    fn oversized_mp4(width: u16, height: u16) -> Vec<u8> {
        fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(payload.len() + 8);
            out.extend_from_slice(&((payload.len() as u32) + 8).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(payload);
            out
        }

        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");

        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&30_000u32.to_be_bytes());

        let mut avc1 = vec![0u8; 78];
        avc1[24..26].copy_from_slice(&width.to_be_bytes());
        avc1[26..28].copy_from_slice(&height.to_be_bytes());
        let mut stsd = vec![0u8; 8];
        stsd[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&bx(b"avc1", &avc1));

        let mut stbl = Vec::new();
        stbl.extend_from_slice(&bx(b"stsd", &stsd));
        let mut minf = Vec::new();
        minf.extend_from_slice(&bx(b"stbl", &stbl));
        let mut mdia = Vec::new();
        mdia.extend_from_slice(&bx(b"mdhd", &mdhd));
        mdia.extend_from_slice(&bx(b"hdlr", &hdlr));
        mdia.extend_from_slice(&bx(b"minf", &minf));
        let mut trak = Vec::new();
        trak.extend_from_slice(&bx(b"tkhd", &[0u8; 84]));
        trak.extend_from_slice(&bx(b"mdia", &mdia));
        let mut moov = Vec::new();
        moov.extend_from_slice(&bx(b"mvhd", &[0u8; 96]));
        moov.extend_from_slice(&bx(b"trak", &trak));

        let mut file = Vec::new();
        file.extend_from_slice(&bx(b"ftyp", b"isom\0\0\0\0isom"));
        file.extend_from_slice(&bx(b"moov", &moov));
        file
    }

    #[test]
    fn prepare_media_never_queues_an_unsupported_clip() {
        let env = TestEnv::new("t15-unsupported");
        let clip = env.media_dir().join("beyond-8k.mp4");
        std::fs::write(&clip, oversized_mp4(9_216, 9_216)).unwrap();
        let toml = env.toml("", 3600);
        let mut rt = env.runtime(&toml);

        let response = call(
            &mut rt,
            Method::PrepareMedia,
            serde_json::json!({"path": clip.display().to_string()}),
        );
        assert!(!response.ok, "超过 8K 上限的素材必须被拒绝");
        let error = response.error.expect("应带 ErrorBody");
        assert_eq!(error.code.as_str(), "UNSUPPORTED_CODEC", "{error:?}");
        assert_eq!(error.detail_code.as_deref(), Some("COMPAT_UNSUPPORTED"));
        // **核心断言**：转码也救不回来，所以队列里什么都不该有。
        assert!(
            rt.transcode.view(&clip).is_none(),
            "compat = unsupported 的素材不得进入转码队列"
        );
        assert_eq!(rt.pending_playback.len(), 0);
    }

    #[test]
    fn prepare_media_refuses_conversion_when_it_is_disabled() {
        let Some(ten_bit) = matrix_clip("matrix-3840x2160-30fps-hevc-10bit-p010.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let env = TestEnv::new("t15-prepare-disabled");
        let toml = env.toml_allowing(&matrix_dir(), "transcode = \"off\"\n");
        let mut rt = env.runtime(&toml);
        let r = call(
            &mut rt,
            Method::PrepareMedia,
            serde_json::json!({"path": ten_bit.display().to_string()}),
        );
        assert!(!r.ok, "关掉自动转码后，10bit 素材不能声称准备好了");
        let err = r.error.expect("应有错误体");
        assert_eq!(format!("{:?}", err.code), "UnsupportedCodec");
        assert_eq!(err.detail_code.as_deref(), Some("CONVERSION_UNAVAILABLE"));
        assert!(err.message.contains("关闭"), "{}", err.message);
        assert!(err.message.contains("transcode"), "{}", err.message);
        // 没有偷偷排队。
        assert!(rt.transcode.view(&ten_bit).is_none(), "关掉时不得入队");
    }

    /// 用 ffmpeg 现场造 4:2:2 / 4:4:4 的 8bit 样本（没有 ffmpeg 时返回 None → 跳过）。
    ///
    /// 这两种色度抽样**位深都是 8bit**，旧分类只看位深 ⇒ 判 Playable ⇒ 开屏掉进
    /// `software:avdec_h264`。样本现场生成，免得把二进制塞进仓库。
    fn make_subsampled_clips(dir: &Path) -> Option<(PathBuf, PathBuf)> {
        let base = matrix_clip("matrix-1920x1080-30fps-h264-8bit.mp4")?;
        std::fs::create_dir_all(dir).ok()?;
        let four_two_two = dir.join("h264-422.mp4");
        let four_four_four = dir.join("hevc-444.mp4");
        let encode = |args: &[&str], out: &Path| {
            std::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-i"])
                .arg(&base)
                .args(["-t", "1", "-an"])
                .args(args)
                .arg(out)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        };
        if !encode(
            &[
                "-c:v",
                "libx264",
                "-profile:v",
                "high422",
                "-pix_fmt",
                "yuv422p",
            ],
            &four_two_two,
        ) {
            return None;
        }
        if !encode(
            &["-c:v", "libx265", "-pix_fmt", "yuv444p", "-tag:v", "hvc1"],
            &four_four_four,
        ) {
            return None;
        }
        Some((four_two_two, four_four_four))
    }

    #[test]
    fn four_two_two_and_four_four_four_are_never_played_with_a_software_decoder() {
        let env = TestEnv::new("t24-chroma");
        let dir = env.media_dir().join("chroma");
        let Some((four_two_two, four_four_four)) = make_subsampled_clips(&dir) else {
            eprintln!("跳过：没有 ffmpeg 或矩阵素材");
            return;
        };
        let caps = crate::compat::HardwareCaps::default();
        // 容器头必须读出真实 chroma（不是凭位深猜）。
        let (details, verdict) = crate::compat::classify_path(&four_two_two, "video", &caps);
        assert_eq!(
            details
                .as_ref()
                .and_then(|item| item.pix_fmt.clone())
                .as_deref(),
            Some("yuv422p"),
            "H.264 4:2:2 的 chroma 要能从 SPS 读出来"
        );
        match verdict {
            crate::compat::Compatibility::NeedsConversion { reason, target } => {
                assert!(reason.contains("4:2:2"), "{reason}");
                assert_eq!(target.label(), "hevc 8bit");
            }
            other => panic!("4:2:2 必须判 NeedsConversion，实际 {other:?}"),
        }
        let (details, verdict) = crate::compat::classify_path(&four_four_four, "video", &caps);
        assert_eq!(
            details
                .as_ref()
                .and_then(|item| item.pix_fmt.clone())
                .as_deref(),
            Some("yuv444p"),
            "HEVC 4:4:4 的 chroma 要能从 hvcC 读出来"
        );
        match verdict {
            crate::compat::Compatibility::NeedsConversion { reason, .. } => {
                assert!(reason.contains("4:4:4"), "{reason}");
            }
            other => panic!("4:4:4 必须判 NeedsConversion，实际 {other:?}"),
        }

        // 播放门禁：必须回 NEEDS_PREPARATION（排队转码）或明确的 CONVERSION_UNAVAILABLE，
        // 且绝不留下一条播放管线（`software:avdec_h264` 就是这么来的）。
        let toml = toml_with_library(&env, 1);
        let mut rt = env.runtime(&toml);
        for clip in [&four_two_two, &four_four_four] {
            let response = call(
                &mut rt,
                Method::SetWallpaper,
                serde_json::json!({
                    "path": clip.display().to_string(),
                    "output": "DP-1",
                }),
            );
            assert!(!response.ok, "{} 不能被静默软解播放", clip.display());
            let error = response.error.expect("应有错误体");
            let code = error.detail_code.unwrap_or_default();
            assert!(
                code == "NEEDS_PREPARATION" || code == "CONVERSION_UNAVAILABLE",
                "detail_code 不对：{code}（{}）",
                error.message
            );
            assert!(
                rt.channels
                    .get("DP-1")
                    .and_then(|channel| channel.current.as_ref())
                    .is_none(),
                "拒绝后不得留下播放管线"
            );
            assert!(
                rt.channels
                    .get("DP-1")
                    .map(|channel| channel.decode_alert.is_none())
                    .unwrap_or(true),
                "不该进入解码阶段"
            );
        }
    }

    #[test]
    fn set_wallpaper_never_silently_soft_decodes_a_ten_bit_clip() {
        let Some(ten_bit) = matrix_clip("matrix-3840x2160-30fps-hevc-10bit-p010.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let env = TestEnv::new("t15-set-ten-bit");
        let toml = env.toml_allowing(&matrix_dir(), "transcode = \"off\"\n");
        let mut rt = env.runtime(&toml);
        let r = call(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({
                "path": ten_bit.display().to_string(),
                "output": "DP-1",
            }),
        );
        assert!(!r.ok, "10bit 素材不能被静默软解播放");
        let err = r.error.expect("应有错误体");
        assert_eq!(
            format!("{:?}", err.code),
            "UnsupportedCodec",
            "code 不对，实际错误：{err:?}"
        );
        assert_eq!(err.detail_code.as_deref(), Some("CONVERSION_UNAVAILABLE"));
        assert!(err.message.contains("10bit"), "{}", err.message);
        // 关键断言：**没有**开始播（current 为空）。
        assert!(
            rt.channels
                .get("DP-1")
                .and_then(|channel| channel.current.as_ref())
                .is_none(),
            "拒绝后不得留下一条软解管线"
        );
    }

    #[test]
    fn eight_k_h264_queues_a_conversion_and_publishes_media_prepared() {
        let Some(clip) = matrix_clip("matrix-7680x4320-30fps-h264-8bit.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let env = TestEnv::new("t15-transcode-flow");
        // 复制到测试自己的媒体目录里：缓存 key 与源路径绑定，这样不会和别的运行撞车。
        let source = env.media_dir().join("eight-k-h264.mp4");
        std::fs::copy(&clip, &source).expect("复制素材");
        // 假 ffmpeg：把最后一个参数（输出路径）写成一个小文件，并吐一行进度。
        // 用「配置指定可执行文件」这条正规入口注入，不开测试专用后门。
        let script = env.media_dir().parent().unwrap().join("fake-ffmpeg.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nfor last in \"$@\"; do :; done\n\
             printf 'out_time_ms=1000000\\nprogress=continue\\n'\n\
             printf 'fake-cache' > \"$last\"\nexit 0\n",
        )
        .expect("写假 ffmpeg");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let toml = env.toml_allowing(
            &matrix_dir(),
            &format!("transcode = \"{}\"\n", script.display()),
        );
        let mut rt = env.runtime(&toml);
        let events = rt.events.subscribe();

        let r = call(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({
                "path": source.display().to_string(),
                "output": "DP-1",
            }),
        );
        assert!(!r.ok, "首次应用应回 NEEDS_PREPARATION 而不是直接播");
        let err = r.error.expect("应有错误体");
        assert_eq!(format!("{:?}", err.code), "NeedsPreparation");
        assert_eq!(err.detail_code.as_deref(), Some("NEEDS_PREPARATION"));
        assert!(
            err.message.contains("8K") || err.message.contains("7680"),
            "{}",
            err.message
        );
        assert_eq!(rt.pending_playback.len(), 1, "应登记「转完自动播」");
        assert_eq!(rt.pending_playback[0].0, "DP-1");

        // 等后台线程完成：tick 里 poll_transcode 收事件。
        let mut prepared = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            rt.tick(std::time::Instant::now());
            while let Ok(event) = events.try_recv() {
                if format!("{:?}", event.kind) == "MediaPrepared" {
                    prepared = Some(event);
                }
            }
            if prepared.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let event = prepared.expect("应推送 MEDIA_PREPARED 事件");
        assert_eq!(
            event.media.as_deref(),
            Some(source.to_string_lossy().as_ref())
        );
        let cache_path = event.cache_path.clone().expect("事件应带缓存路径");
        assert!(std::path::Path::new(&cache_path).exists(), "缓存应落盘");
        // t37：产物容器跟着该输出的解码路径（mock 后端 ⇒ GStreamer ⇒ .mp4）。
        let expected_extension = rt.playback_consumer("DP-1").container().extension();
        assert!(
            cache_path.ends_with(expected_extension),
            "缓存扩展名应为 .{expected_extension}: {cache_path}"
        );
        // 源文件没被动过。
        assert_eq!(
            std::fs::metadata(&source).unwrap().len(),
            std::fs::metadata(&clip).unwrap().len(),
            "源文件不得被修改"
        );
        // 转码器状态机：ready + 1000‰。
        let view = rt.transcode.view(&source).expect("应有任务记录");
        assert_eq!(format!("{:?}", view.state), "Ready");
        assert_eq!(view.progress_permille, 1000);
        assert!(rt.pending_playback.is_empty(), "自动播放后应清空待播队列");
        let _ = std::fs::remove_file(&cache_path);
    }

    // ===== t29：视频类型判定的单一真相 + 未知色度不放行 =====

    /// 视频/图片类型判定：已知视频扩展名（含 `.mkv`/`.webm`）一律视频，未知扩展名
    /// 靠容器头兜底，真图片仍是图片（t29 F1）。
    #[test]
    fn media_kind_for_path_recognises_all_video_containers() {
        use std::path::Path;
        for name in [
            "a.mp4", "a.m4v", "a.mov", "a.mkv", "a.webm", "A.MKV", "a.Mp4", "a.ts", "a.avi",
        ] {
            if name.ends_with(".ts") || name.ends_with(".avi") {
                // 未知扩展名要靠容器头，单独在下面用真文件验。
                continue;
            }
            assert_eq!(
                media_kind_for_path(Path::new(name)),
                MediaKind::Video,
                "{name} 必须判成视频"
            );
        }
        for name in [
            "a.png", "a.jpg", "a.jpeg", "a.webp", "a.bmp", "a.gif", "a.txt",
        ] {
            assert_eq!(
                media_kind_for_path(Path::new(name)),
                MediaKind::Image,
                "{name} 必须判成图片"
            );
        }
        // 未知扩展名：认容器头（真 MP4 换个 `.bin` 后缀也必须判成视频）。
        let env = TestEnv::new("t29-kind-probe");
        if let Some(clip) = matrix_clip("matrix-1920x1080-30fps-h264-8bit.mp4") {
            let unknown_mp4 = env.media_dir().join("clip.bin");
            std::fs::copy(&clip, &unknown_mp4).expect("复制素材");
            assert_eq!(
                media_kind_for_path(&unknown_mp4),
                MediaKind::Video,
                "未知扩展名但容器头是 MP4 ⇒ 视频"
            );
        } else {
            eprintln!("跳过未知扩展名用例：没有矩阵素材");
        }
        // 未知扩展名 + 认不出的字节 ⇒ 仍是图片（保持既有行为：交给 PNG 解码报错）。
        let garbage = env.media_dir().join("clip.unknown");
        std::fs::write(&garbage, b"not a container").expect("写垃圾文件");
        assert_eq!(media_kind_for_path(&garbage), MediaKind::Image);
    }

    /// 用 ffmpeg 把矩阵里的 H.264 片段重封装成 `.mkv`（只换容器，不解码）。
    fn remux_to_mkv(source: &std::path::Path, target: &std::path::Path) -> bool {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-nostdin",
                "-y",
                "-loglevel",
                "error",
                "-t",
                "1",
                "-i",
            ])
            .arg(source)
            .args(["-c", "copy", "-an"])
            .arg(target)
            .status();
        matches!(status, Ok(status) if status.success()) && target.exists()
    }

    /// `.mkv` 源必须走视频路径：以前扩展名表只认 mp4/m4v/mov，`.mkv` 会被当图片解码，
    /// 报 `PNG 解码失败 … Invalid PNG signature`（t29 F1）。
    #[test]
    fn mkv_sources_take_the_video_path_instead_of_the_png_one() {
        let Some(clip) = matrix_clip("matrix-1920x1080-30fps-h264-8bit.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let env = TestEnv::new("t29-mkv-set");
        let source = env.media_dir().join("clip.mkv");
        if !remux_to_mkv(&clip, &source) {
            eprintln!("跳过：ffmpeg 不可用");
            return;
        }
        assert_eq!(
            media_kind_for_path(&source),
            MediaKind::Video,
            ".mkv 必须判成视频"
        );
        let mut rt = env.runtime(&env.toml("", 1));
        let response = call(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({ "path": source.display().to_string(), "output": "DP-1" }),
        );
        if !response.ok {
            let error = response.error.expect("失败必须带错误体");
            let message = error.message.clone();
            assert!(
                !message.contains("PNG"),
                ".mkv 走了图片路径（PNG 解码）：{message}"
            );
            // 视频路径的失败只可能是解码/能力类，不可能是"路径参数非法"。
            assert_ne!(
                format!("{:?}", error.code),
                "InvalidParams",
                "视频路径不应报参数非法：{message}"
            );
        }
    }

    /// 8K H.264 → 自动转码 → 播缓存；**重启后**从 state.json 恢复缓存（`.mkv`）必须仍然
    /// 是视频（t29 F1 的验收点：重启恢复，不是只 `set` 成功）。
    #[test]
    fn transcode_cache_is_restored_as_video_after_restart() {
        let Some(clip) = matrix_clip("matrix-7680x4320-30fps-h264-8bit.mp4") else {
            eprintln!("跳过：没有 8K 矩阵素材");
            return;
        };
        let Some(playable) = matrix_clip("matrix-1920x1080-30fps-h264-8bit.mp4") else {
            eprintln!("跳过：没有 1080p 矩阵素材");
            return;
        };
        let env = TestEnv::new("t29-cache-restart");
        // 让缓存里放一份**真能播**的 MKV（H.264 重封装），这样"恢复后能播"可断言。
        let product = env.media_dir().join("product.mkv");
        if !remux_to_mkv(&playable, &product) {
            eprintln!("跳过：ffmpeg 不可用");
            return;
        }
        let source = env.media_dir().join("eight-k.mp4");
        std::fs::copy(&clip, &source).expect("复制 8K 素材");
        // 假 ffmpeg：把准备好的 MKV 复制到最后一个参数（输出路径），并吐一行进度。
        let script = env.media_dir().join("fake-ffmpeg-copy.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor last in \"$@\"; do :; done\n\
                 printf 'out_time_ms=1000000\\nprogress=continue\\n'\n\
                 cp '{}' \"$last\"\nexit 0\n",
                product.display()
            ),
        )
        .expect("写假 ffmpeg");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let toml = env.toml_allowing(
            &matrix_dir(),
            &format!("transcode = \"{}\"\n", script.display()),
        );

        let cache_path = {
            let mut rt = env.runtime(&toml);
            let response = call(
                &mut rt,
                Method::SetWallpaper,
                serde_json::json!({ "path": source.display().to_string(), "output": "DP-1" }),
            );
            assert!(!response.ok, "首次应用 8K H.264 应回 NEEDS_PREPARATION");
            let events = rt.events.subscribe();
            let mut cache = None;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while std::time::Instant::now() < deadline {
                rt.tick(std::time::Instant::now());
                while let Ok(event) = events.try_recv() {
                    if format!("{:?}", event.kind) == "MediaPrepared" {
                        cache = event.cache_path.clone();
                    }
                }
                if cache.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let cache = cache.expect("应推送带缓存路径的 MEDIA_PREPARED");
            // t37：扩展名跟着该输出的解码路径（mock 后端 ⇒ GStreamer ⇒ .mp4）。
            let expected_extension = rt.playback_consumer("DP-1").container().extension();
            assert!(
                cache.ends_with(expected_extension),
                "缓存扩展名应为 .{expected_extension}: {cache}"
            );
            // 自动播放：转完直接播缓存（视频路径）。
            rt.tick(std::time::Instant::now());
            let current = rt.channels["DP-1"]
                .current
                .as_ref()
                .map(|item| std::fs::canonicalize(&item.media).unwrap_or(item.media.clone()));
            let expected = std::fs::canonicalize(&cache).unwrap_or_else(|_| cache.clone().into());
            if current.as_deref() != Some(expected.as_path()) {
                eprintln!(
                    "跳过重启断言：本机无法解码测试用的 MKV（current={:?}）",
                    current
                );
                return;
            }
            cache
        };

        // 重启：同一个 state.json，新 Runtime 会 `restore_persisted`。
        let restarted = env.runtime(&toml);
        let media = restarted.channels["DP-1"]
            .current
            .as_ref()
            .map(|item| item.media.clone());
        let expected = std::fs::canonicalize(&cache_path).unwrap_or(cache_path.into());
        assert_eq!(
            media.as_deref(),
            Some(expected.as_path()),
            "重启后必须恢复播放转码缓存（视频路径），而不是把它当图片解码"
        );
    }

    #[cfg(feature = "vaapi")]
    #[test]
    fn capability_rejections_are_terminal_but_environment_failures_are_not() {
        // 与 crates/liuguang-vaapi 里 `VaapiError::Unsupported` 的真实文案保持一致。
        let ten_bit = "媒体读取或解码失败: 不支持: 该素材是 10bit（yuv420p10le），当前解码/RGB 转换链只支持 8bit NV12：请改用 8bit 版本，或先转成 8bit（例如 `ffmpeg -i in.mp4 -pix_fmt yuv420p out.mp4`）";
        assert!(vaapi_rejection_is_permanent(ten_bit), "10bit 必须终止回退");
        let no_decoder = "没有可用的 VA-API 硬件解码器：默认解码器 libdav1d 不提供 VAAPI 硬件配置";
        assert!(
            vaapi_rejection_is_permanent(no_decoder),
            "该编码没有硬解解码器时必须终止回退（否则只会得到一条误导性的软解报错）"
        );
        // 环境类：换个节点/下次再试可能就好 —— 必须继续回退到 GStreamer。
        for transient in [
            "无法创建 VA-API 解码器: 设备忙（Resource temporarily unavailable）",
            "导出 DMA-BUF 失败: 修饰符不被下游接受（DRM_FORMAT_MOD_INVALID）",
            "等待首帧超时（2000ms）",
            "VAAPI 设备不可用: /dev/dri/renderD129 打不开",
        ] {
            assert!(
                !vaapi_rejection_is_permanent(transient),
                "环境类失败不得终止回退: {transient}"
            );
        }
    }

    // ===== t37：consumer 每输出解析一次，贯穿 classify / 转码 / 播放 =====

    /// `[media].video_vaapi` 的开关判定必须是同一份规则（构造期与运行期共用）。
    #[test]
    fn vaapi_switch_parsing_covers_the_documented_values() {
        use liuguang_core::config::Config;

        let mut config = Config::default();
        config.media.video_vaapi = "auto".to_string();
        assert!(vaapi_setting_enabled_in(&config), "auto 表示允许");
        for off in ["off", "OFF", "  off ", "0", "false", "no", "none"] {
            config.media.video_vaapi = off.to_string();
            assert!(!vaapi_setting_enabled_in(&config), "{off} 应视为关闭");
        }
        config.media.video_vaapi = String::new();
        assert!(vaapi_setting_enabled_in(&config), "空值 = 默认允许");
    }

    /// 转码目标按**任务自己的 consumer** 算，而不是启动期/设备级的 profile（t37 F2）。
    ///
    /// 这正是现场：VAAPI 构建（启动期 profile = hevc/mkv）+ `LIUGUANG_VIDEO_VAAPI=off`
    /// （播放走 GStreamer）⇒ 旧实现转出 `.mkv/H.265`，播放端打不开。
    #[test]
    fn transcode_target_follows_the_task_consumer_not_the_startup_profile() {
        use crate::compat::DecodeConsumer;

        let env = TestEnv::new("t37-transcode-target");
        let toml = env.toml("", 1);
        let mut rt = env.runtime(&toml);
        // 模拟"VAAPI 构建 + 设备可开"的启动期 profile（旧实现就固定在这上面）。
        let config = rt.config.clone();
        rt.transcode = crate::transcode::Transcoder::from_config(
            &config.media.transcode,
            None,
            &config.media.transcode_crf_or_bitrate,
            config.media.transcode_max_width,
            DecodeConsumer::Vaapi,
        );
        assert_eq!(
            rt.transcode.profile().container,
            crate::compat::Container::Mkv,
            "启动期默认 profile 仍是 VA-API 的 hevc/mkv"
        );

        let source = env.media_dir().join("clip-37.mp4");
        std::fs::write(&source, b"placeholder").expect("写占位文件");
        let vaapi = rt
            .transcode_spec(&source, None, DecodeConsumer::Vaapi)
            .expect("vaapi spec");
        let playback = rt
            .transcode_spec(&source, None, DecodeConsumer::GStreamer)
            .expect("gstreamer spec");

        assert_eq!(vaapi.profile.codec, "hevc");
        assert!(vaapi.cache_path.to_string_lossy().ends_with(".mkv"));
        assert_eq!(
            playback.profile.codec, "h264",
            "产物编码必须跟随实际播放路径"
        );
        assert!(
            playback.cache_path.to_string_lossy().ends_with(".mp4"),
            "GStreamer 侧产物必须是 MP4：{}",
            playback.cache_path.display()
        );
        assert_ne!(vaapi.key, playback.key, "两种构型的缓存 key 必须互不命中");
    }

    /// 读接口（`get_media` / `prepare_media`）与 `set_wallpaper` 必须给出同一个结论（t37 F1）。
    ///
    /// 这里把 `caps` 设成 VA-API（设备级探测的默认值），但 mock 后端不支持 DMA-BUF
    /// 扫描输出 ⇒ 实际播放走 GStreamer：`a420.mkv` 读出来必须是 `needs_conversion`，
    /// 随后 `set_wallpaper` 也要回 `NEEDS_PREPARATION`，而不是"先报 playable 再要转码"。
    #[test]
    fn read_api_and_playback_share_one_consumer() {
        let Some(clip) = matrix_clip("matrix-1920x1080-30fps-h264-8bit.mp4") else {
            eprintln!("跳过：没有矩阵素材");
            return;
        };
        let env = TestEnv::new("t37-consistency");
        let source = env.media_dir().join("a420.mkv");
        if !remux_to_mkv(&clip, &source) {
            eprintln!("跳过：ffmpeg 不可用");
            return;
        }
        let toml = toml_with_library(&env, 1);
        let mut rt = env.runtime(&toml);
        // 设备级探测的默认值（VA-API 构建的常见结果）——旧实现直接拿它判 compat。
        rt.caps = crate::compat::HardwareCaps::default();
        assert_eq!(
            rt.caps.consumer,
            crate::compat::DecodeConsumer::Vaapi,
            "前提：设备级 caps 说是 VA-API"
        );
        assert_eq!(
            rt.consumer_for_request(None),
            crate::compat::DecodeConsumer::GStreamer,
            "实际播放路径（mock 后端不支持 DMA-BUF 扫描输出）必须是 GStreamer"
        );

        let response = ok_json(
            &mut rt,
            Method::GetMedia,
            serde_json::json!({ "id": source.display().to_string() }),
        );
        let compat = &response["compat"];
        assert_eq!(
            compat["state"].as_str(),
            Some("needs_conversion"),
            "{response}"
        );
        let target = compat["target"].as_str().unwrap_or_default();
        assert!(target.contains("mp4"), "target 必须写明 MP4 产物：{target}");

        // 同一个 consumer ⇒ set_wallpaper 的说法必须一致（不得回"其实能播"）。
        let applied = call(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({ "path": source.display().to_string(), "output": "DP-1" }),
        );
        assert!(!applied.ok, "未转码前不应直接应用：{:?}", applied.error);
        let error = applied.error.expect("失败必须带错误体");
        assert_eq!(
            error.detail_code.as_deref(),
            Some("NEEDS_PREPARATION"),
            "读接口说 needs_conversion，播放侧也必须走转码：{}",
            error.message
        );
    }

    #[test]
    fn set_wallpaper_applies_rotation_and_noop_transform_skips_redraw() {
        let env = TestEnv::new("set-rotation-noop-transform");
        let a = env.png("a.png", 16, 16, [12, 34, 56, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({
                "path": a.display().to_string(),
                "output": "DP-1",
                "fit": "contain",
                "zoom": 1.25,
                "rotation": 90,
            }),
        );
        let cfg = &rt.config.outputs["DP-1"];
        assert_eq!(cfg.rotation, Rotation::Deg90);
        assert!((cfg.zoom - 1.25).abs() < 1e-6);

        rt.pending_config.clear();
        let present_before = rt.backend.commit_stats().map_or(0, |s| s.commit_ok);
        ok_json(
            &mut rt,
            Method::SetOutputTransform,
            serde_json::json!({
                "output": "DP-1",
                "fit": "contain",
                "zoom": 1.25,
                "rotation": 90,
            }),
        );
        assert!(
            rt.pending_config.is_empty(),
            "变换参数未改变时不应标记待写配置"
        );
        assert_eq!(
            rt.backend.commit_stats().map_or(0, |s| s.commit_ok),
            present_before,
            "变换参数未改变时不得重复重绘"
        );

        let b = env.png("b.png", 16, 16, [99, 88, 77, 255]);
        rt.pending_sets.push(PendingSet {
            request_id: "pending-rot".to_string(),
            output: "DP-1".to_string(),
            entry: MediaEntry {
                path: b.clone(),
                kind: MediaKind::Image,
            },
            declared_path: b.display().to_string(),
            pin: false,
            transform_changed: false,
        });
        ok_json(
            &mut rt,
            Method::SetOutputTransform,
            serde_json::json!({ "output": "DP-1", "rotation": 180 }),
        );
        assert_eq!(rt.config.outputs["DP-1"].rotation, Rotation::Deg180);
        assert_eq!(
            rt.backend.commit_stats().map_or(0, |s| s.commit_ok),
            present_before,
            "队列里已有待上屏壁纸时不得重绘旧画面"
        );
        rt.run_pending_sets();
        assert_eq!(
            rt.channels["DP-1"]
                .current
                .as_ref()
                .map(|c| c.media.clone()),
            Some(b)
        );
    }

    #[test]
    fn overlapping_pause_reasons_stay_paused_until_all_clear() {
        let env = TestEnv::new("overlap-pause-reasons");
        let a = env.png("a.png", 16, 16, [1, 2, 3, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({ "path": a.display().to_string(), "output": "DP-1" }),
        );
        attach_stub_video(&mut rt, "DP-1", true);
        let now = Instant::now();

        let _ = rt.pause_channels_for_reason(PauseReason::GameRunning);
        let _ = rt.pause_channels_for_reason(PauseReason::GpuMemory);
        assert!(rt.channels["DP-1"].paused);
        assert_eq!(rt.pause_reason_of("DP-1"), Some("game_running"));
        assert!(rt.state_contradictions().is_empty());

        let resumed = rt.resume_channels_for_reason(PauseReason::GameRunning, now);
        assert!(resumed.is_empty(), "GPU 压力仍在，不得提前恢复");
        assert!(rt.channels["DP-1"].paused);
        assert_eq!(rt.pause_reason_of("DP-1"), Some("gpu_memory"));
        assert!(rt.state_contradictions().is_empty());

        ok_json(
            &mut rt,
            Method::Pause,
            serde_json::json!({ "output": "DP-1" }),
        );
        let resumed = rt.resume_channels_for_reason(PauseReason::GpuMemory, now);
        assert!(resumed.is_empty(), "用户手动暂停仍在，不得越权恢复");
        assert_eq!(rt.pause_reason_of("DP-1"), Some("user"));
        assert!(rt.state_contradictions().is_empty());

        ok_json(
            &mut rt,
            Method::Resume,
            serde_json::json!({ "output": "DP-1" }),
        );
        assert!(!rt.channels["DP-1"].paused);
        assert_eq!(rt.pause_reason_of("DP-1"), None);
        assert!(rt.state_contradictions().is_empty());
    }

    #[test]
    fn sync_performance_resumes_when_game_or_gpu_pause_disabled() {
        let env = TestEnv::new("sync-perf-resume");
        let a = env.png("a.png", 16, 16, [8, 9, 10, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({ "path": a.display().to_string(), "output": "DP-1" }),
        );
        attach_stub_video(&mut rt, "DP-1", true);

        let _ = rt.pause_channels_for_reason(PauseReason::GameRunning);
        assert!(rt.channels["DP-1"].paused);
        rt.config.performance.detect_games = false;
        rt.sync_performance();
        assert!(rt.game_paused.is_empty());
        assert!(!rt.channels["DP-1"].paused);
        assert!(rt.state_contradictions().is_empty());

        let _ = rt.pause_channels_for_reason(PauseReason::GpuMemory);
        assert!(rt.channels["DP-1"].paused);
        rt.config.performance.pause_when_gpu_memory_above = 0;
        rt.sync_performance();
        assert!(rt.gpu_paused.is_empty());
        assert!(!rt.channels["DP-1"].paused);
        assert!(rt.state_contradictions().is_empty());

        let _ = rt.pause_channels_for_reason(PauseReason::Battery);
        assert!(rt.channels["DP-1"].paused);
        assert_eq!(rt.pause_reason_of("DP-1"), Some("battery"));
        rt.config.performance.pause_on_battery = false;
        rt.sync_performance();
        assert!(rt.battery_paused.is_empty());
        assert!(!rt.channels["DP-1"].paused);
        assert_eq!(rt.pause_reason_of("DP-1"), None);
        assert!(rt.state_contradictions().is_empty());
    }

    #[test]
    fn set_output_transform_validates_and_updates_brightness_live() {
        let env = TestEnv::new("set-output-brightness");
        let a = env.png("a.png", 16, 16, [100, 120, 140, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({ "path": a.display().to_string(), "output": "DP-1" }),
        );
        attach_stub_video(&mut rt, "DP-1", true);

        let bad = call(
            &mut rt,
            Method::SetOutputTransform,
            serde_json::json!({ "output": "DP-1", "brightness": 1.5 }),
        );
        assert!(!bad.ok);

        ok_json(
            &mut rt,
            Method::SetOutputTransform,
            serde_json::json!({ "output": "DP-1", "brightness": 0.65 }),
        );
        let cfg = rt.config.outputs.get("DP-1").unwrap();
        assert!((cfg.brightness - 0.65).abs() < 1e-4);
        assert!(rt.channels["DP-1"].video.is_some());
    }

    #[test]
    fn paused_video_unloads_pipeline_after_delay_and_preserves_pause_state() {
        let env = TestEnv::new("pause-unload");
        let a = env.png("a.png", 16, 16, [10, 20, 30, 255]);
        let mp4 = env.media_dir().join("clip.mp4");
        std::fs::write(&mp4, b"stub-mp4").unwrap();
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({ "path": a.display().to_string(), "output": "DP-1" }),
        );
        attach_stub_video(&mut rt, "DP-1", true);
        rt.channels
            .get_mut("DP-1")
            .unwrap()
            .current
            .as_mut()
            .unwrap()
            .media = mp4.clone();

        let t0 = Instant::now();
        let _ = rt.pause_channels_for_reason(PauseReason::Battery);
        let wake = rt.step_channel("DP-1", t0);
        assert_eq!(wake, Some(t0 + PAUSE_UNLOAD_DELAY));
        assert!(rt.channels["DP-1"].video.is_some());
        assert!(rt.channels["DP-1"].paused_video_media.is_none());

        // 未到阈值不提前卸载。
        let _ = rt.step_channel("DP-1", t0 + PAUSE_UNLOAD_DELAY - Duration::from_secs(1));
        assert!(rt.channels["DP-1"].video.is_some());

        // 达到阈值后卸载管线并记住媒体路径，顶层与重叠暂停状态保持一致。
        let wake_after = rt.step_channel("DP-1", t0 + PAUSE_UNLOAD_DELAY);
        assert_eq!(wake_after, None);
        assert!(rt.channels["DP-1"].video.is_none());
        assert_eq!(rt.channels["DP-1"].paused_video_media.as_ref(), Some(&mp4));
        assert_eq!(rt.state.current(), State::Paused);
        assert!(rt.state_contradictions().is_empty());

        // 管线卸载后叠加新的暂停原因，通道依然会被记录进去。
        let fs_paused = rt.pause_channels_for_reason(PauseReason::Fullscreen);
        assert_eq!(fs_paused, vec!["DP-1".to_string()]);
        assert!(rt.fullscreen_paused.contains("DP-1"));

        // stop_video 会清理待重建记录。
        let _ = rt.stop_video("DP-1");
        assert!(rt.channels["DP-1"].paused_video_media.is_none());
        assert!(rt.channels["DP-1"].paused_since.is_none());
    }

    #[test]
    fn pinning_all_channels_clears_and_skips_preload_cache() {
        let env = TestEnv::new("pinned-preload-clear");
        let a = env.png("a.png", 16, 16, [1, 2, 3, 255]);
        let b = env.png("b.png", 16, 16, [4, 5, 6, 255]);
        let mut rt = env.runtime(&env.toml("", 3600));
        ok_json(
            &mut rt,
            Method::SetWallpaper,
            serde_json::json!({ "path": a.display().to_string(), "output": "DP-1" }),
        );
        rt.preload.preload_sized(&b, 16, 16).unwrap();
        assert!(rt.preload.bytes() > 0);

        ok_json(
            &mut rt,
            Method::SetPinned,
            serde_json::json!({ "output": "DP-1", "pinned": true }),
        );
        assert_eq!(rt.preload.bytes(), 0);
        rt.schedule_next_preload("DP-1");
        assert!(rt.channels["DP-1"].preload_target.is_none());
    }
}
