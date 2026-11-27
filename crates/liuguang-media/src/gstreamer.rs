//! 真实 MP4/H.264 视频生产者（feature = `gstreamer`，非默认）。
//!
//! 管线（元素选择由 [`DecoderPolicy`] 决定，硬解可用性先经 [`crate::va`] 探测）：
//! `filesrc ! qtdemux ! h264parse ! vah264dec|vaapih264dec|avdec_h264|openh264dec
//! ! videoconvert ! video/x-raw,format=RGBA ! appsink`
//!
//! - `appsink` 以 `max-buffers` 背压：队列满时管线暂停解码（不丢帧不堆积）；
//!   [`FrameProducer::next_producer_frame`] 用 `try-pull-sample` 拉取样本。
//! - PTS 取自缓冲区时间戳；缺失时按源帧率步进回退。
//! - [`FrameProducer::seek_start`] 用 `seek_simple` 回到流起点（循环重播）。
//! - 所有错误映射为 [`MediaError::Decode`]（文件缺失为 [`MediaError::Io`]），不 panic。
//!
//! **解码器守卫**（真机教训）：
//! - GStreamer 的 `va` 插件按当前 VA 驱动能力**惰性注册**元素：驱动不宣告
//!   `VAProfileH264*: VAEntrypointVLD` 时 `vah264dec` 不存在。所以选型前先由
//!   [`crate::va::probe`] 探测驱动能力（必要时固化 `LIBVA_DRIVER_NAME`），
//!   再按元素存在性挑元素。
//! - [`Self::decoder_element`] 报告**实际生效**的元素工厂名（不是"打算用"的名字），
//!   并由上层写进 `status.decoder`。
//! - 软解回退要么被 [`DecoderPolicy::allow_software_fallback`] 明确允许（**大声告警**），
//!   要么直接报错——绝不静默降级。

use crate::MediaError;
use crate::video::{DecoderKind, DmabufState, FrameProducer, VideoPoll};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use liuguang_core::types::{CpuFrame, DisplayFrame, MediaInfo, MediaKind, PixelFormat};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 打开文件时等待解码管线协商出视频 caps 的上限。
const PREROLL_TIMEOUT: Duration = Duration::from_secs(10);
/// 帧捕获模式（性能递增：RGBA 需 CPU 色彩转换，NV12 走硬件 YUV→RGB，
/// DMA-BUF 为零拷贝目标但依赖平台支持）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// CPU RGBA（videoconvert 做 VA→RGBA 色彩转换，最贵）。
    Rgba,
    /// CPU NV12（解码器原生输出，扫描输出时硬件做 YUV→RGB）。
    Nv12,
    /// DMA-BUF 直通（零拷贝；本平台 gst-va 未证实可用，需显式开启）。
    DmaBuf,
}

impl CaptureMode {
    /// 诊断用短名（进日志与 `status`）。
    const fn name(self) -> &'static str {
        match self {
            Self::Rgba => "rgba",
            Self::Nv12 => "nv12",
            Self::DmaBuf => "dmabuf",
        }
    }
}

/// RGBA 捕获模式下选用哪个色彩转换元素。
///
/// 硬解元素（`vah264dec`/`vaapih264dec`/`vavp9dec` …，工厂名以 `va` 开头）的
/// src 是 VA 表面/NV12，用 `vapostproc` 可在 **GPU** 侧转成 RGBA，CPU 只付一次
/// 下载；缺 `vapostproc`（或软解元素）时退回 `videoconvert`（CPU 转换，旧行为）。
fn rgba_converter_for(decoder_element: &str, vapostproc_available: bool) -> &'static str {
    if decoder_element.starts_with("va") && vapostproc_available {
        "vapostproc"
    } else {
        "videoconvert"
    }
}

/// 管线生命周期守卫：任何提前返回（含错误路径）都把管线切到 `NULL`。
///
/// 只 drop 引用**不会**停掉 qtdemux 等流线程；真机实测曾在切回静态图后
/// 残留 `qtdemux` 线程，所以失败路径也必须显式收尾。
struct PipelineGuard {
    pipeline: Option<gst::Pipeline>,
}

impl PipelineGuard {
    /// 新建（内部创建一条空管线）。
    fn new() -> Self {
        Self {
            pipeline: Some(gst::Pipeline::new()),
        }
    }

    /// 借用内部管线。
    fn pipeline(&self) -> Option<&gst::Pipeline> {
        self.pipeline.as_ref()
    }

    /// 解除守卫并交出管线（成功路径）。
    fn disarm(mut self) -> gst::Pipeline {
        self.pipeline.take().unwrap_or_default()
    }
}

impl Drop for PipelineGuard {
    fn drop(&mut self) {
        if let Some(pipeline) = self.pipeline.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}

/// appsink 内部缓冲上限（背压阈值，`max-buffers` 属性为 guint）。
const APPSINK_MAX_BUFFERS: u32 = 3;
/// 缓冲区缺 PTS 且源帧率未知时的回退步长。
const FALLBACK_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// 解码器选择策略（由配置映射而来；见 `media.hardware_decode` /
/// `media.software_decode_fallback`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderPolicy {
    /// 禁用硬解（`media.hardware_decode = "disabled"`）：直接用软解。
    pub prefer_software: bool,
    /// 必须硬解（`media.hardware_decode = "required"`）：拿不到硬解就报错。
    pub require_hardware: bool,
    /// 允许软件解码回退（`media.software_decode_fallback`）：
    /// 为 false 时拿不到硬解直接报错，杜绝"配置声称硬解、实际在软解"。
    pub allow_software_fallback: bool,
}

impl Default for DecoderPolicy {
    fn default() -> Self {
        Self {
            prefer_software: false,
            require_hardware: false,
            allow_software_fallback: true,
        }
    }
}

impl DecoderPolicy {
    /// 纯软解策略（测试/离线场景）。
    pub const fn software_only() -> Self {
        Self {
            prefer_software: true,
            require_hardware: false,
            allow_software_fallback: true,
        }
    }

    /// 是否需要硬解（拿不到就报错）。
    const fn hardware_mandatory(self) -> bool {
        self.require_hardware || !self.allow_software_fallback
    }
}

/// 选定的解码元素（**实际生效**的元素名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecoderChoice {
    /// GStreamer 元素工厂名（如 `vah264dec`）。
    element: &'static str,
    /// 解码器类别。
    kind: DecoderKind,
}

/// H.264 硬解元素候选（VA-API：新/旧插件名）。
const HW_DECODERS: [DecoderChoice; 2] = [
    DecoderChoice {
        element: "vah264dec",
        kind: DecoderKind::Vaapi,
    },
    DecoderChoice {
        element: "vaapih264dec",
        kind: DecoderKind::Vaapi,
    },
];

/// H.264 软解元素候选（FFmpeg / OpenH264）。
const SW_DECODERS: [DecoderChoice; 2] = [
    DecoderChoice {
        element: "avdec_h264",
        kind: DecoderKind::Software,
    },
    DecoderChoice {
        element: "openh264dec",
        kind: DecoderKind::Software,
    },
];

/// 探测本机可用的 H.264 元素（按类别）。
fn available_decoders(candidates: &[DecoderChoice]) -> Vec<DecoderChoice> {
    candidates
        .iter()
        .copied()
        .filter(|c| gst::ElementFactory::find(c.element).is_some())
        .collect()
}

/// 按策略选出解码元素；顺序即解码尝试顺序（硬解失败可退到软解）。
///
/// # Errors
///
/// 硬解被要求（`required` 或不允许软解回退）但无可用硬解元素时返回
/// [`MediaError::Decode`]，**不静默降级为软解**。
fn select_decoders(policy: DecoderPolicy) -> Result<Vec<DecoderChoice>, MediaError> {
    let va = crate::va::probe();
    let hw = available_decoders(&HW_DECODERS);
    let sw = available_decoders(&SW_DECODERS);
    choose_decoders(policy, hw, sw, &va.summary())
}

/// [`select_decoders`] 的纯函数部分（元素可用性由调用方给出，便于离线测试）。
fn choose_decoders(
    policy: DecoderPolicy,
    hw: Vec<DecoderChoice>,
    sw: Vec<DecoderChoice>,
    va_summary: &str,
) -> Result<Vec<DecoderChoice>, MediaError> {
    if policy.prefer_software {
        // 显式禁用硬解：只走软解（配置意图优先）。
        return if sw.is_empty() {
            Err(MediaError::Decode(format!(
                "media.hardware_decode=disabled 但无可用软解元素（avdec_h264 / openh264dec）；{va_summary}"
            )))
        } else {
            tracing::warn!(
                decoder = %sw[0].element,
                "按配置禁用硬解（media.hardware_decode=disabled），使用软件解码"
            );
            Ok(sw)
        };
    }
    if hw.is_empty() {
        let detail =
            format!("无可用 VA-API H.264 硬解元素（vah264dec / vaapih264dec）；{va_summary}");
        if policy.hardware_mandatory() {
            return Err(MediaError::Decode(format!(
                "{detail}；硬解被要求（media.hardware_decode=required 或 \
                 media.software_decode_fallback=false），拒绝回退软解"
            )));
        }
        if sw.is_empty() {
            return Err(MediaError::Decode(format!("{detail}；且无可用软解元素")));
        }
        // 大声告警：软解在 4K60 上远超实时（真机实测 11.15s user / 3s 素材，约 2.8 核）。
        tracing::warn!(
            decoder = %sw[0].element,
            va = %va_summary,
            "视频软解回退：未能获得 VA-API H.264 硬解，改用 {} 软件解码；\
             4K60 实测 CPU 比硬解高一个数量级（约 2.8 个核跑满）。\
             请检查 VA 驱动能力（vainfo 的 H.264 profile），\
             或设 media.software_decode_fallback=false 拒绝软解",
            sw[0].element
        );
        return Ok(sw);
    }
    let mut choices = hw;
    if policy.allow_software_fallback {
        choices.extend(sw);
    }
    Ok(choices)
}

/// GStreamer 视频帧生产者（MP4/H.264）。
pub struct GstProducer {
    pipeline: gst::Pipeline,
    appsink: gst::Element,
    /// qtdemux（用于 `Drop` 时断开动态 pad 回调）。
    demux: gst::Element,
    /// `pad-added` 回调句柄：`Drop` 时断开以释放其持有的下游 pad 引用。
    pad_added: Option<gst::glib::SignalHandlerId>,
    video_info: gst_video::VideoInfo,
    info: MediaInfo,
    decoder_kind: DecoderKind,
    /// **实际生效**的解码元素工厂名（如 `vah264dec`），用于 `status` 上报。
    decoder_element: String,
    /// 实际使用的捕获模式名（诊断：rgba / nv12 / dmabuf）。
    capture_mode: &'static str,
    /// DMA-BUF 直通模式（false = CPU RGBA）。
    dmabuf: bool,
    /// 是否已经记录过"DMA-BUF 协商成功"的取值明细（只记一次）。
    dmabuf_logged: std::cell::Cell<bool>,
    /// DMA-BUF keepalive 环（t31/S1）。
    ///
    /// 媒体侧只 `dup` fd 就丢掉整个 `gst::Sample`；而 `dup` **只保证分配不释放**，
    /// **不能**阻止解码器/显示侧的池在校验引用归零后把**同一 surface** 交回解码器
    /// 重写（显示侧还会**跨帧保留**该内存 2 帧：`drm_kms.rs` 的双缓冲）。
    /// 后果是**撕裂/花屏而 `present_ok`/`commit_ok`/`failed=0` 全绿**——本项目最怕的
    /// "绿着坏"。这里持有最近 K 个 sample，使"仍在扫出"的 buffer 引用不为零。
    ///
    /// K=[`DMABUF_KEEPALIVE_FRAMES`]：显示侧保留 2 帧 + 1 帧在途 ⇒ 3 足够；
    /// **不能取大**：libva surface 有限，持有过多会逼解码器等空 surface
    /// （用真机 fps/CPU 守住，见 t31 报告）。
    dmabuf_keepalive: std::cell::RefCell<std::collections::VecDeque<gst::Sample>>,
    /// 构造期回退次数（S7）：成功之前有多少个候选（元素×捕获模式）失败/被拒。
    ///
    /// `0` = 第一候选就成功；`>0` = 发生过回退（例如 DMA-BUF 被拒 → NV12）。
    /// 目标是把"是否直通、回退过几次"变成**结构化可观测值**，而不是只有一句日志。
    dmabuf_fallbacks: u64,
    /// keepalive 观测日志的节流计数（每 600 帧打一条 INFO）。
    keepalive_logged: std::cell::Cell<u64>,
    /// keepalive 环容量（可由 `LIUGUANG_DMABUF_KEEPALIVE` 覆盖，见 [`parse_keepalive_cap`]）。
    keepalive_cap: usize,
    /// 归属观测的日志节流计数。
    pool_owner_logged: std::cell::Cell<u64>,
    /// 排障计数：拉到样本次数。
    pull_some: u64,
    /// 排障计数：空手次数。
    pull_none: u64,
    /// 源侧提前丢弃计数（未做像素转换）。
    skipped: u64,
    /// 丢弃水位（媒体 PTS）。
    drop_horizon: Option<Duration>,
    /// PTS 缺失时的回退步长（源帧率倒数）。
    frame_interval: Duration,
    /// 下一个回退 PTS。
    next_fallback_pts: Duration,
    /// 已观察到总线 EOS。
    eos_seen: bool,
    /// appsink 是否提供只读 `eos` 属性（GStreamer 1.24+ 有；没有时退回总线判据）。
    has_eos_property: bool,
    /// 已上报 [`VideoPoll::Ended`]。
    finished: bool,
    /// 上一帧"映射/下载"耗时（毫秒）：VA surface → 系统内存的下载在这里发生。
    last_map_ms: u32,
    /// 上一帧"行拷贝"耗时（毫秒）：把解码器缓冲拷进 CpuFrame。
    last_copy_ms: u32,
    /// 耗时统计窗口内的累计值与帧数（周期性上报媒体侧开销）。
    map_ms_sum: u64,
    copy_ms_sum: u64,
    timed_frames: u32,
}

/// 媒体侧耗时统计的上报窗口（帧）。
const TIMING_WINDOW: u32 = 120;

/// 候选探测日志的**逐条**模式开关（环境变量 `LIUGUANG_VERBOSE_PROBE`）。
const ENV_VERBOSE_PROBE: &str = "LIUGUANG_VERBOSE_PROBE";

/// 由环境变量原值解析"是否逐条打候选失败"（纯函数；便于单测，不读进程环境）。
///
/// 只有显式为真值才开启；`0`/空/未设置都是**默认的降噪模式**。
#[must_use]
fn probe_verbose_value(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "on" | "true" | "yes"
        ),
    }
}

/// 读取进程环境（**唯一的**环境读取点；测试只测 [`probe_verbose_value`]）。
#[must_use]
fn probe_verbose_from_env() -> bool {
    let raw = std::env::var(ENV_VERBOSE_PROBE).ok();
    probe_verbose_value(raw.as_deref())
}

/// 候选模式序列的紧凑名字串（汇总文本用）。
#[must_use]
fn mode_names(modes: &[CaptureMode]) -> String {
    modes.iter().map(|m| m.name()).collect::<Vec<_>>().join(",")
}

/// 候选全灭时的**一行**汇总文本（F6）。
///
/// 抽成纯函数是为了可断言：`tried` 是真正尝试过的（元素×模式）组合数，
/// `modes` 是本轮的模式序列，`last_err` 是最后一个失败原因。逐条细节已降
/// `debug`，这一行是用户在日志里唯一能看到的东西，所以它必须自足。
#[must_use]
fn probe_failure_summary(
    tried: u64,
    modes: &[CaptureMode],
    last_err: Option<&MediaError>,
) -> String {
    format!(
        "tried={tried} modes=[{}] last_error={}",
        mode_names(modes),
        last_err.map_or_else(|| "（无）".to_string(), ToString::to_string)
    )
}

/// DMA-BUF 捕获策略（环境变量 `LIUGUANG_VIDEO_DMABUF`）。
///
/// | 取值 | 行为 |
/// |---|---|
/// | 未设置 / `0` / `off` / `false` / `no` | 不尝试 DMA-BUF（NV12 → RGBA，默认） |
/// | `1` / `on` / `true` / 其它非空值 | 尝试 DMA-BUF，失败回退 NV12/RGBA |
/// | **`force-fail`** | **测试钩子**：尝试 DMA-BUF 但**注入协商失败**，用于验证"失败 → 回退系统内存 NV12"这条路径 |
///
/// 钩子的边界（不得引入新的静默失败面）：
/// - 只影响 [`CaptureMode::DmaBuf`] 这一种模式；`off`/`try` 行为逐字节不变；
/// - 注入的失败**带明确标记**（日志里写明 `注入：LIUGUANG_VIDEO_DMABUF=force-fail`），
///   不会被误认为平台真实缺陷；
/// - 失败后走的是与真实协商失败**完全相同**的回退循环（同一个 `open_with_modes`），
///   因此它验证的是真实回退路径，而不是另开一条假路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufPolicy {
    /// 不尝试 DMA-BUF。
    Off,
    /// 尝试 DMA-BUF，失败回退。
    Try,
    /// 尝试但强制协商失败（测试钩子）。
    ForceFail,
}

impl DmabufPolicy {
    /// 由环境变量原值解析（纯函数；便于单测，不读进程环境）。
    #[must_use]
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value {
            // **默认尝试 DMA-BUF**（2026-10-20 晚）：横带根因已修复（tile 对齐 +
            // keepalive 默认 4 帧），直通路径实测干净且 CPU 从 64.4% 降到 7.7%。
            // 协商失败会自动回退到 CPU 捕获（NV12/RGBA），因此默认开启是安全的。
            // 需要强制关闭时设 LIUGUANG_VIDEO_DMABUF=off。
            None => Self::Try,
            Some(raw) => {
                let v = raw.trim().to_ascii_lowercase();
                match v.as_str() {
                    "" | "0" | "off" | "false" | "no" => Self::Off,
                    "force-fail" | "force_fail" | "forcefail" | "fail" => Self::ForceFail,
                    _ => Self::Try,
                }
            }
        }
    }

    /// 读取进程环境（**唯一的**环境读取点）。
    #[must_use]
    pub fn from_env() -> Self {
        let raw = std::env::var("LIUGUANG_VIDEO_DMABUF").ok();
        // 直通需要**显示后端**支持（`DisplayBackend::supports_dmabuf_scanout`）：
        // 当前 DRM 后端在扫描期间会被解码器重写同一缓冲（实测重影，7 条修法均被
        // 实测否证），已不接受 DMA-BUF 帧。daemon 据此强制 CPU 捕获 ⇒ 这里设置了
        // 环境变量也不会真的上直通，必须**说出来**，否则运维会以为自己在跑直通。
        if raw.as_deref().is_some_and(|v| {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "" | "0" | "off" | "false" | "no")
        }) {
            tracing::warn!(
                "请求了 DMA-BUF 直通（LIUGUANG_VIDEO_DMABUF），但直通扫描输出已于 2026-10-20 \
                 从显示层移除（重影，根因未解）；守护进程会按 CPU 捕获打开解码管线\
                 （见 DisplayBackend::supports_dmabuf_scanout）"
            );
        }
        Self::from_env_value(raw.as_deref())
    }

    /// 是否要尝试 DMA-BUF 捕获模式。
    #[must_use]
    pub const fn attempts_dmabuf(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// 是否注入协商失败（测试钩子）。
    #[must_use]
    pub const fn forces_failure(self) -> bool {
        matches!(self, Self::ForceFail)
    }

    /// 本策略下要尝试的捕获模式序列（顺序即优先级；模块内部使用）。
    ///
    /// **谁能真正用上 DMA-BUF 由显示后端决定**（`DisplayBackend::supports_dmabuf_scanout`）：
    /// 当前 DRM 后端在扫描期间会被解码器重写同一缓冲（真机局部横带/重影；7 条修法实测
    /// 否证，根因未解），对 `DisplayFrame::DmaBuf` 返回**类型化**的 `DmaBufRejected`，
    /// daemon 因此只以 [`Self::Off`] 之外的策略打开管线、实际走 `open_cpu`。
    ///
    /// 本函数仍按策略给出序列，是为了：① `force-fail` 钩子继续验证真实回退循环；
    /// ② 显示侧解决缓冲生命周期（生产者租约 + 下屏确认）后无需再改媒体侧。
    #[must_use]
    fn modes(self) -> &'static [CaptureMode] {
        if self.attempts_dmabuf() {
            // **2026-10-20 晚重新启用**：横带根因已定位为「KMS 平面缩放器处理
            // Y_TILED 缓冲」，改为让媒体侧产出 **LINEAR**（见 ENV_DMABUF_MODIFIER /
            // ENV_DMABUF_FORMAT）后，同一条提交路径即可干净上屏。
            &[CaptureMode::DmaBuf, CaptureMode::Nv12, CaptureMode::Rgba]
        } else {
            &[CaptureMode::Nv12, CaptureMode::Rgba]
        }
    }
}

impl GstProducer {
    /// 打开 MP4/H.264 文件并完成首帧协商。
    ///
    /// 解码元素按 [`DecoderPolicy`] 与**本机实际探测到的能力**选择：
    /// 硬解优先；硬解容器不可用时，只有在 [`DecoderPolicy::allow_software_fallback`]
    /// 允许的前提下才回退软解（并大声告警），否则直接报错。
    ///
    /// [`Self::decoder_element`] 报告实际生效的元素；[`Self::decoder_kind`]
    /// 报告硬/软类别。
    ///
    /// # Errors
    ///
    /// 文件不存在、管线构建失败、硬解被要求但不可用、软解被禁止、
    /// 无可用解码元素或协商超时时返回 [`MediaError`]，不 panic。
    pub fn open(path: &Path, policy: DecoderPolicy) -> Result<Self, MediaError> {
        Self::open_fitted(path, policy, None)
    }

    /// 打开解码管线，并**告诉媒体侧目标显示尺寸** `(宽, 高)`。
    ///
    /// 传入目标尺寸时，DMA-BUF 模式会在解码器后插入 `vapostproc`
    /// （`add-borders=false` = cover），在 **GPU 侧**把画面缩放并裁切成
    /// 目标尺寸，输出仍是 `NV12:Y_TILED` 的 DMA-BUF。
    ///
    /// 这样显示层拿到的是**已经等于模式尺寸**的缓冲 ⇒ 平面只做 1:1 扫描，
    /// 不经过 KMS 缩放器。实测：KMS 缩放器处理 Y_TILED 缓冲会产生
    /// 固定位移副本 + 横向条带（2026-10-20 真机定位），1:1 扫描则完全干净。
    ///
    /// # Errors
    ///
    /// 同 [`Self::open`]。
    pub fn open_fitted(
        path: &Path,
        policy: DecoderPolicy,
        target: Option<(u32, u32)>,
    ) -> Result<Self, MediaError> {
        // 模式选择：NV12（解码器原生输出 + 硬件 YUV→RGB）优先，
        // 失败回退 RGBA。DMA-BUF 需显式开启（`LIUGUANG_VIDEO_DMABUF`，见 [`DmabufPolicy`]）。
        let dmabuf = DmabufPolicy::from_env();
        Self::open_with_modes(
            path,
            policy,
            dmabuf.modes(),
            dmabuf.forces_failure(),
            target,
        )
    }

    /// 同 [`Self::open_fitted`]，但**只接受 RGB 的显示后端**（wl_shm 类）用：
    /// DMA-BUF 仍优先，协商失败时直接落到 RGBA，绝不停在 NV12。
    ///
    /// 为什么单独一个入口：`open_fitted` 的回退序列是 NV12 → RGBA，而 wl_shm
    /// 只有 RGB 格式；若被强制关掉直通（`LIUGUANG_VIDEO_DMABUF=off`）或协商失败，
    /// 就会稳定选出 NV12 帧 → 显示侧连续拒绝 → 视频降级成静态图（真机复现）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::open`]。
    pub fn open_fitted_rgba(
        path: &Path,
        policy: DecoderPolicy,
        target: Option<(u32, u32)>,
    ) -> Result<Self, MediaError> {
        let dmabuf = DmabufPolicy::from_env();
        let modes: &[CaptureMode] = match dmabuf {
            DmabufPolicy::Off => &[CaptureMode::Rgba],
            _ => &[CaptureMode::DmaBuf, CaptureMode::Rgba],
        };
        Self::open_with_modes(path, policy, modes, dmabuf.forces_failure(), target)
    }

    /// **强制 CPU 捕获模式**打开：只尝试 NV12 → RGBA（不碰 DMA-BUF）。
    ///
    /// 用途（t32 / S2）：显示侧拒绝 DMA-BUF（modifier/平面组合不被接受）后，
    /// daemon 用本入口**重开**解码管线，让视频继续播（系统内存 NV12 + 硬件 YUV→RGB），
    /// 而不是把视频降级成静态图。
    ///
    /// # Errors
    ///
    /// 同 [`Self::open`]。
    pub fn open_cpu(path: &Path, policy: DecoderPolicy) -> Result<Self, MediaError> {
        Self::open_with_modes(
            path,
            policy,
            &[CaptureMode::Nv12, CaptureMode::Rgba],
            false,
            None,
        )
    }

    /// 强制 CPU RGBA 模式打开（如需像素级旋转等 CPU 处理时）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::open`]。
    pub fn open_rgba(path: &Path, policy: DecoderPolicy) -> Result<Self, MediaError> {
        Self::open_with_modes(path, policy, &[CaptureMode::Rgba], false, None)
    }

    /// 在给定捕获模式序列上尝试全部候选解码器。
    ///
    /// 顺序：逐个解码元素（硬解优先，允许时后缀软解）× 逐个捕获模式。
    /// 硬解元素存在但在本流上失败时，会继续尝试下一个候选（含软解），
    /// 每次回退都有日志。
    fn open_with_modes(
        path: &Path,
        policy: DecoderPolicy,
        modes: &[CaptureMode],
        dmabuf_force_fail: bool,
        target: Option<(u32, u32)>,
    ) -> Result<Self, MediaError> {
        // 文件不存在：在任何 GStreamer 初始化/选型之前就给明确错误。
        if !path.exists() {
            return Err(MediaError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "文件不存在"),
            });
        }
        // 元素工厂查询需要 GStreamer 已初始化（进程内只会初始化一次）。
        gst::init().map_err(|e| MediaError::Decode(format!("GStreamer 初始化失败: {e}")))?;
        // 选型失败（要求硬解 / 禁止软解）直接上抛，不做任何降级。
        let choices = select_decoders(policy)?;
        let va = crate::va::probe();
        let mut last_err: Option<MediaError> = None;
        // 记录 DMA-BUF 尝试的失败原因：回退成功时要把它作为**根因**打进 WARN。
        let mut dmabuf_err: Option<String> = None;
        // S7：构造期回退计数——成功之前有多少候选（元素×模式）失败/被拒。
        let mut fallbacks: u64 = 0;
        // F6：**中间候选失败降噪**。正常机器上"先试硬解某模式→失败→再试下一个"
        // 是常态（NV12 不可协商、DMA-BUF 被拒等），每条都 WARN 会让日志里
        // 全是"候选不可用"，真正的失败反而被埋掉。默认逐条降 `debug`，
        // 全部候选失败时**一条** WARN 汇总；`LIUGUANG_VERBOSE_PROBE=1` 恢复逐条 WARN。
        let verbose = probe_verbose_from_env();
        let mut tried: u64 = 0;
        for (i, choice) in choices.iter().enumerate() {
            for (j, mode) in modes.iter().enumerate() {
                if *mode == CaptureMode::DmaBuf && dmabuf_force_fail {
                    // 测试钩子：不真正建管线，按"协商失败"处理，走**同一条**回退路径。
                    // 注入是**用户显式要求**的排障场景，所以保持 WARN 可见
                    // （t29 的真机验收就靠这一行判定"注入确实生效"）。
                    let injected = MediaError::Decode(
                        "DMA-BUF 协商失败（注入：LIUGUANG_VIDEO_DMABUF=force-fail；                         用于验证回退系统内存 NV12 的路径，非平台真实缺陷）"
                            .to_string(),
                    );
                    tracing::warn!(
                        element = choice.element,
                        mode = ?mode,
                        error = %injected,
                        injected = true,
                        "解码候选不可用（注入失败）"
                    );
                    dmabuf_err = Some(injected.to_string());
                    last_err = Some(injected);
                    fallbacks = fallbacks.saturating_add(1);
                    tried = tried.saturating_add(1);
                    continue;
                }
                tried = tried.saturating_add(1);
                match Self::open_inner(path, *choice, *mode, target) {
                    Ok(mut p) => {
                        p.dmabuf_fallbacks = fallbacks;
                        tracing::info!(
                            element = %p.decoder_element,
                            dmabuf = p.dmabuf,
                            fallbacks = p.dmabuf_fallbacks,
                            kind = ?choice.kind,
                            mode = p.capture_mode,
                            has_eos_property = p.has_eos_property,
                            va = %va.summary(),
                            "视频解码管线已建立（实际生效元素）"
                        );
                        if p.capture_mode != CaptureMode::DmaBuf.name()
                            && modes.contains(&CaptureMode::DmaBuf)
                        {
                            // 验收要求：**带根因**地说明"回退到系统内存 NV12（CPU 上传）"。
                            tracing::warn!(
                                element = %p.decoder_element,
                                capture_mode = p.capture_mode,
                                root_cause = dmabuf_err.as_deref().unwrap_or("DMA-BUF 候选未产出结果"),
                                injected = dmabuf_force_fail,
                                "已回退系统内存 NV12（CPU 上传）：DMA-BUF 直通不可用，\n\
                                 视频仍可播放（decoder 里的 capture mode 会如实显示 nv12/rgba）"
                            );
                        }
                        if !p.has_eos_property {
                            tracing::warn!(
                                "appsink 没有 `eos` 属性：流末只能依赖总线 EOS 消息（本平台实测 appsink 不发布该消息，可能导致播完不循环）"
                            );
                        }
                        return Ok(p);
                    }
                    Err(e) => {
                        fallbacks = fallbacks.saturating_add(1);
                        if *mode == CaptureMode::DmaBuf {
                            dmabuf_err = Some(e.to_string());
                        }
                        let last_choice = i + 1 == choices.len();
                        let last_mode = j + 1 == modes.len();
                        if verbose {
                            tracing::warn!(
                                element = choice.element,
                                mode = ?mode,
                                error = %e,
                                last = last_choice && last_mode,
                                "解码候选不可用（逐条；LIUGUANG_VERBOSE_PROBE=1）"
                            );
                        } else {
                            tracing::debug!(
                                element = choice.element,
                                mode = ?mode,
                                error = %e,
                                last = last_choice && last_mode,
                                "解码候选不可用（中间失败；全失败时有一条 WARN 汇总）"
                            );
                        }
                        last_err = Some(e);
                    }
                }
            }
        }
        // 一条**汇总** WARN：候选全灭才是需要用户看见的事（逐条细节已经降
        // `debug`，需要时用 LIUGUANG_VERBOSE_PROBE=1 复现）。
        match last_err {
            Some(e) => {
                let summary = probe_failure_summary(tried, modes, Some(&e));
                tracing::warn!(
                    tried,
                    modes = %mode_names(modes),
                    last_error = %e,
                    summary = %summary,
                    "全部解码候选（元素×捕获模式）都失败：视频无法打开"
                );
                Err(e)
            }
            None => {
                let summary = probe_failure_summary(0, modes, None);
                tracing::warn!(summary = %summary, "没有可尝试的解码候选：视频无法打开");
                Err(MediaError::Decode("无可用视频捕获模式".into()))
            }
        }
    }

    /// 实际生效的解码元素工厂名（`status`/日志用）。
    pub fn decoder_element(&self) -> &str {
        &self.decoder_element
    }

    /// 实际使用的捕获模式名（诊断用）。
    pub const fn capture_mode_name(&self) -> &'static str {
        self.capture_mode
    }

    /// DMA-BUF 直通状态（结构化）：是否直通 + 构造期回退次数 + 保活长度。
    #[must_use]
    pub fn dmabuf_state(&self) -> DmabufState {
        DmabufState {
            is_dmabuf: self.is_dmabuf(),
            fallbacks: self.dmabuf_fallbacks(),
            keepalive_len: self.dmabuf_keepalive_len(),
        }
    }

    /// 排障计数（拉取成功/空手），供上层日志使用。
    pub const fn pull_counters(&self) -> (u64, u64) {
        (self.pull_some, self.pull_none)
    }

    /// 构建管线；`mode` 决定 appsink 前的 caps 与是否插入 `videoconvert`。
    fn open_inner(
        path: &Path,
        choice: DecoderChoice,
        mode: CaptureMode,
        target: Option<(u32, u32)>,
    ) -> Result<Self, MediaError> {
        let make = |factory: &str| -> Result<gst::Element, MediaError> {
            gst::ElementFactory::make(factory)
                .build()
                .map_err(|e| MediaError::Decode(format!("创建 GStreamer 元素 {factory} 失败: {e}")))
        };

        // 错误路径守卫：任何提前返回都把管线切到 NULL，停掉 qtdemux 等流线程。
        let guard = PipelineGuard::new();
        let pipeline = guard
            .pipeline()
            .ok_or_else(|| MediaError::Decode("解码管线已释放".into()))?;

        let filesrc = make("filesrc")?;
        filesrc.set_property("location", path.to_string_lossy().into_owned());
        let demux = make("qtdemux")?;
        let parse = make("h264parse")?;
        let decoder = make(choice.element)?;
        // 实际生效的元素名取自已实例化的元素工厂（而不是候选表里的名字），
        // 保证 `status.decoder` 与实际管线一致。
        let decoder_element = decoder
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_else(|| choice.element.to_string());
        let capsfilter = make("capsfilter")?;
        // **t93**：自有池必须用**协商后的完整 caps**（含 width/height）配置 ——
        // `VideoBufferPool::set_config` 内部走 `gst_video_info_from_caps()`，
        // 缺宽高就构造不出 `GstVideoInfo` ⇒ `Failed to set config`（t92 实测）。
        // 因此这里只记住"要给哪个元素挂池"，**建池推迟到协商完成之后**。
        let mut pool_target: Option<gst::Element> = None;
        let (convert, dmabuf) = match mode {
            CaptureMode::DmaBuf => {
                // **必须显式写出 `format=DMA_DRM`**：`vah264dec`/`vapostproc` 的
                // src 模板里 DMABuf 变体的 format 固定为 `DMA_DRM`，fourcc/modifier
                // 由 caps 的 `drm-format` 字段携带（如 `NV12:0x0100000000000002`）。
                // 只写 `(memory:DMABuf)`（features-only）会让协商落到没有
                // `drm-format` 的组合上，取不到 fourcc/modifier。
                // t49 实验：`LIUGUANG_VIDEO_DMABUF_LINEAR=1` ⇒ 显式要求 **LINEAR**（modifier 0x0），
                // 让显示侧按线性解释缓冲（免去 tile 语义）。若解码器拒绝 ⇒ 协商失败（如实记录）。
                // 这是**只改 caps 的实验开关**，默认不生效。
                // t53：把旧 LINEAR 开关**一般化**为任意 modifier（旧开关仍兼容）。
                // 只改 caps，不动管线结构；默认不约束（由 iHD 决定）。
                let choice = match parse_dmabuf_modifier(
                    std::env::var(ENV_DMABUF_MODIFIER).ok().as_deref(),
                    std::env::var(ENV_DMABUF_LINEAR).ok().as_deref(),
                ) {
                    Ok(c) => c,
                    Err(msg) => {
                        // **不静默降级**：非法值直接失败，避免"以为在测 X、其实没约束"的假实验。
                        return Err(MediaError::Decode(msg));
                    }
                };
                // t73 修法 B：可选择**线性 I420（YU12）**输出。
                // 需要 `vapostproc` 参与（解码器只出 tiled NV12）；VPP 产出**它自己的**
                // 线性缓冲 ⇒ 显示扫描期间没有解码器在写它（与被扫 surface 解耦）。
                let (pix_fmt, illegal_fmt) =
                    parse_dmabuf_format(std::env::var(ENV_DMABUF_FORMAT).ok().as_deref());
                if illegal_fmt {
                    tracing::warn!(
                        requested = %std::env::var(ENV_DMABUF_FORMAT).unwrap_or_default(),
                        "LIUGUANG_VIDEO_DMABUF_FORMAT 非法（只接受 nv12/yu12），已回退 NV12"
                    );
                }
                // **目标尺寸优先**（2026-10-20 晚，真机定位）：
                // 传入目标显示尺寸时，在 **GPU 侧**完成「缩放 + cover 裁切」，
                // 输出缓冲因此**等于屏幕尺寸**，显示层只做 1:1 平面扫描 ——
                // 从而绕开 KMS 缩放器处理 Y_TILED 缓冲的缺陷
                // （实测表现为固定位移副本 + 横向条带，与 7 条缓冲生命周期修法无关）。
                let force_vpp = parse_force_vpp(std::env::var(ENV_FORCE_VPP).ok().as_deref())
                    || target.is_some();
                let convert = match (pix_fmt, force_vpp) {
                    (DmabufPixFmt::Yu12, _) => Some(make("vapostproc")?),
                    (DmabufPixFmt::Nv12, true) => {
                        let vpp = make("vapostproc")?;
                        // **不要设 disable-passthrough**（2026-10-20 晚，实测）：
                        // 设了它会让 VPP 在没有可用尺寸时被迫产新缓冲，导致
                        // `Failed to link elements 'vapostprocN' and 'capsfilterN'`。
                        match target {
                            Some((tw, th)) => {
                                // cover：缩放填满目标尺寸，超出部分裁掉（不产生黑边）。
                                vpp.set_property("add-borders", false);
                                tracing::info!(
                                    target_w = tw,
                                    target_h = th,
                                    "已插入 vapostproc（add-borders=false）：GPU 侧缩放+裁切到屏幕尺寸，\
                                     显示层只做 1:1 平面扫描"
                                );
                            }
                            None => {
                                tracing::info!(
                                    "已插入 vapostproc（LIUGUANG_VIDEO_FORCE_VPP）：强制 VPP 参与"
                                );
                            }
                        }
                        if env_flag(std::env::var(ENV_OWN_POOL).ok().as_deref()) {
                            // 只记住目标；**真正的建池在协商之后**（t93）。
                            pool_target = Some(vpp.clone());
                        }
                        Some(vpp)
                    }
                    (DmabufPixFmt::Nv12, false) => None,
                };
                let mut caps_str = dmabuf_caps_for(pix_fmt, choice);
                // **必须显式给出目标尺寸**：没有 width/height 时 vapostproc 无从得知
                // 要缩放到多大，会直接 passthrough（实测输出仍是源尺寸），
                // 于是缩放又被推回 KMS 平面 —— 正是要绕开的路径。
                if let Some((tw, th)) = target {
                    caps_str.push_str(&format!(",width=(int){tw},height=(int){th}"));
                }
                match choice {
                    DmabufModifierChoice::Unconstrained => {}
                    DmabufModifierChoice::Pinned(m) => {
                        let name = match m {
                            0 => "LINEAR",
                            0x0100_0000_0000_0002 => "Y_TILED",
                            0x0100_0000_0000_0004 => "Yf_TILED",
                            _ => "自定义 modifier",
                        };
                        tracing::warn!(
                            modifier = format!("0x{m:x}"),
                            known_as = name,
                            caps = %caps_str,
                            "t53 实验：已把 DMA-BUF 的 drm-format 钉死为该 modifier —— 若解码器拒绝会协商失败（如实记录，不静默回退）"
                        );
                    }
                }
                let caps: gst::Caps = caps_str
                    .parse()
                    .map_err(|e| MediaError::Decode(format!("构造 DMABuf caps 失败: {e}")))?;
                capsfilter.set_property("caps", caps);
                (convert, true)
            }
            CaptureMode::Nv12 => {
                // 解码器原生 NV12：无 videoconvert，色彩转换交给扫描输出硬件。
                let caps = gst::Caps::builder("video/x-raw")
                    .field("format", "NV12")
                    .build();
                capsfilter.set_property("caps", caps);
                (None, false)
            }
            CaptureMode::Rgba => {
                let caps = gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .build();
                capsfilter.set_property("caps", caps);
                // **硬解优先走 GPU 侧色彩转换**（2026-11-21 真机）：
                // `vah264dec` 的 src 是 NV12/VA 表面，`videoconvert` 会在 CPU 上
                // 做 YUV→RGB（1080p 约 10–20ms/帧）；`vapostproc` 是 VA 的 VPP，
                // 在 GPU 上转 RGBA，CPU 只付一次下载。没有 vapostproc（或解码器
                // 是软解元素）时退回 videoconvert —— 行为与旧版一致。
                let factory = rgba_converter_for(
                    &decoder_element,
                    gst::ElementFactory::find("vapostproc").is_some(),
                );
                tracing::info!(
                    decoder = %decoder_element,
                    converter = factory,
                    "RGBA 捕获模式：选择色彩转换元素"
                );
                (Some(make(factory)?), false)
            }
        };
        let appsink = make("appsink")?;
        if dmabuf {
            // **t89 probe A/B 分置**：`copy_frames` 要求 VPP 看到的查询里**没有** `GstVideoMeta`
            // （`gstvabasetransform.c:434-435`），而 `vah264dec` 在 DMABuf 协商时**硬要求**有它。
            // ALLOCATION 查询从 appsink 向**上游**传播：appsink → capsfilter → vpp → decoder。
            // ⇒ 把"声明 VideoMeta"的探针放到 **decoder 的 src pad**：
            //   * `vapostproc` 先处理查询（**看不到 meta**）⇒ 有机会走 copy_frames；
            //   * 查询继续上溯到 decoder 时**才**被加上 meta ⇒ 解码器仍能协商 DMABuf。
            let upstream = std::env::var("LIUGUANG_VIDEO_META_PROBE")
                .is_ok_and(|v| v.trim().eq_ignore_ascii_case("upstream"));
            let probe_pad = if upstream {
                decoder.static_pad("src")
            } else {
                appsink.static_pad("sink")
            };
            let ok = probe_pad
                .as_ref()
                .is_some_and(attach_video_meta_query_probe);
            if ok {
                tracing::info!(
                    target = if upstream {
                        "decoder.src"
                    } else {
                        "appsink.sink"
                    },
                    "已在目标 pad 上挂 allocation 查询探针（声明 GstVideoMeta）"
                );
            } else {
                tracing::warn!(
                    "无法挂 allocation 查询探针：无法声明 GstVideoMeta，\
                     DMABuf 协商必然失败（将回退系统内存 NV12）"
                );
            }
        }
        appsink.set_property("sync", false);
        appsink.set_property("drop", false);
        appsink.set_property("wait-on-eos", false);
        appsink.set_property("max-buffers", APPSINK_MAX_BUFFERS);

        // 管线：filesrc ! qtdemux ! h264parse ! decoder [! convert] ! capsfilter ! appsink。
        // qtdemux 动态 pad 在 pad-added 回调中接 h264parse。
        let mut tail: Vec<gst::Element> = vec![parse.clone(), decoder.clone()];
        if let Some(c) = &convert {
            tail.push(c.clone());
        }
        tail.push(capsfilter.clone());
        tail.push(appsink.clone());
        let mut elements = vec![filesrc.clone(), demux.clone()];
        elements.extend(tail.iter().cloned());
        for element in &elements {
            pipeline
                .add(element)
                .map_err(|e| MediaError::Decode(format!("装配解码管线失败: {e}")))?;
        }
        // 静态链接（除 demux 动态 pad 外）。
        filesrc
            .link(&demux)
            .map_err(|e| MediaError::Decode(format!("链接 filesrc→qtdemux 失败: {e}")))?;
        for pair in tail.windows(2) {
            pair[0]
                .link(&pair[1])
                .map_err(|e| MediaError::Decode(format!("元素链接失败: {e}")))?;
        }
        let parse_sink = parse
            .static_pad("sink")
            .ok_or_else(|| MediaError::Decode("h264parse 缺少 sink pad".into()))?;
        let pad_added = demux.connect_pad_added(move |_demux, src_pad| {
            if src_pad.is_linked() {
                return;
            }
            let _ = src_pad.link(&parse_sink);
        });

        pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| MediaError::Decode(format!("解码管线进入 Paused 失败: {e}")))?;

        let caps = wait_for_video_caps(pipeline, &appsink, PREROLL_TIMEOUT, dmabuf)?;

        // **t93**：现在 `caps` 是**协商后的完整 caps**（含 width/height/framerate）——
        // 用它配置自有池，`gst_video_info_from_caps()` 才能成功。
        if let Some(vpp) = pool_target {
            // **t93**：`caps` 是**协商后的 DMA_DRM caps**，`gst_video_info_from_caps()` 解析不了
            // （`DMA_DRM` 不是 raw format）⇒ 池仍会 `Failed to set config`。
            // ⇒ 用**协商得到的宽高**重新构造 **普通 `video/x-raw,format=NV12`** caps 再配置池。
            let (pw, ph) = {
                let vi = gst_video::VideoInfo::from_caps(&caps)
                    .map_err(|e| MediaError::Decode(format!("解析视频 caps 失败: {e}")))?;
                (vi.width(), vi.height())
            };
            let pool_caps = gst::Caps::builder("video/x-raw")
                .field("format", "NV12")
                .field("width", pw as i32)
                .field("height", ph as i32)
                .build();
            let maxbuf = parse_pool_buffers(
                std::env::var("LIUGUANG_VIDEO_OWN_POOL_BUFFERS")
                    .ok()
                    .as_deref(),
            );
            match make_own_pool(&vpp, &pool_caps, maxbuf) {
                Ok(_pool) => tracing::info!(
                    max_buffers = maxbuf,
                    caps = %pool_caps,
                    "已挂自有 GstBufferPool（协商后完整 caps）：VPP 可能走 copy_frames 用它"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    "自有池挂载失败，回退到 VPP 自带池（现网行为）"
                ),
            }
        }
        let video_info = gst_video::VideoInfo::from_caps(&caps)
            .map_err(|e| MediaError::Decode(format!("解析视频 caps 失败: {e}")))?;

        let fps = video_info.fps();
        let source_fps = if fps.numer() > 0 && fps.denom() > 0 {
            Some(f64::from(fps.numer()) / f64::from(fps.denom()))
        } else {
            None
        };
        let frame_interval = source_fps
            .filter(|f| f.is_finite() && *f > 0.0)
            .and_then(|f| Duration::try_from_secs_f64(1.0 / f).ok())
            .unwrap_or(FALLBACK_FRAME_INTERVAL);
        let duration = pipeline
            .query_duration::<gst::ClockTime>()
            .map(|t| Duration::from_nanos(t.nseconds()));

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| MediaError::Decode(format!("解码管线进入 Playing 失败: {e}")))?;

        let info = MediaInfo {
            kind: MediaKind::Video,
            width: video_info.width(),
            height: video_info.height(),
            duration,
            source_fps,
            codec: Some("h264".into()),
        };

        let has_eos_property = appsink.find_property("eos").is_some();
        Ok(Self {
            pipeline: guard.disarm(),
            appsink,
            demux,
            pad_added: Some(pad_added),
            video_info,
            info,
            decoder_kind: choice.kind,
            decoder_element,
            capture_mode: mode.name(),
            frame_interval,
            next_fallback_pts: Duration::ZERO,
            pull_some: 0,
            pull_none: 0,
            skipped: 0,
            drop_horizon: None,
            dmabuf,
            dmabuf_logged: std::cell::Cell::new(false),

            dmabuf_keepalive: std::cell::RefCell::new(std::collections::VecDeque::new()),
            dmabuf_fallbacks: 0,
            keepalive_logged: std::cell::Cell::new(0),
            pool_owner_logged: std::cell::Cell::new(0),
            keepalive_cap: {
                let (cap, clamped) =
                    parse_keepalive_cap(std::env::var(ENV_DMABUF_KEEPALIVE).ok().as_deref());
                if clamped {
                    tracing::warn!(
                        requested = %std::env::var(ENV_DMABUF_KEEPALIVE).unwrap_or_default(),
                        max = DMABUF_KEEPALIVE_MAX,
                        "keepalive 容量超上限，已夹紧（防止 surface 池被耗干）"
                    );
                }
                cap
            },
            eos_seen: false,
            has_eos_property,
            finished: false,
            last_map_ms: 0,
            last_copy_ms: 0,
            map_ms_sum: 0,
            copy_ms_sum: 0,
            timed_frames: 0,
        })
    }

    /// 轮询总线：记录 EOS、将错误映射为 [`MediaError::Decode`]。
    fn poll_bus(&mut self) -> Result<(), MediaError> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        while let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(0),
            &[gst::MessageType::Error, gst::MessageType::Eos],
        ) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    let detail = format!(
                        "{}{}",
                        err.error(),
                        err.debug().map(|d| format!("（{d}）")).unwrap_or_default()
                    );
                    return Err(classify_pipeline_error(detail, self.dmabuf));
                }
                gst::MessageView::Eos(_) => {
                    tracing::info!("总线报告 EOS");
                    self.eos_seen = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// appsink 是否已收到 EOS 事件。
    ///
    /// **实测（真机 + dev 机，GStreamer 1.28）**：`appsink` 作为管线末端时
    /// **不会**在总线上发布 EOS 消息——它把流结束放在自己的只读属性 `eos`
    /// （以及 `eos` 信号）里。只看总线会永远等不到流结束：视频播完一次就
    /// 永久停住（`presented` 卡在整段帧数上），循环重播也不会发生。
    fn appsink_eos(&self) -> bool {
        self.has_eos_property && self.appsink.property::<bool>("eos")
    }

    /// 非阻塞拉取一个样本（appsink `try-pull-sample` 动作信号）。
    fn try_pull_sample(&self) -> Option<gst::Sample> {
        self.appsink
            .emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&0u64])
    }

    /// 样本 → [`DisplayFrame`]：DMA-BUF 模式提取 fd（零拷贝），
    /// RGBA 模式行拷贝为 CPU 帧。
    fn sample_to_frame(&mut self, sample: &gst::Sample) -> Result<DisplayFrame, MediaError> {
        let buffer = sample
            .buffer()
            .ok_or_else(|| MediaError::Decode("视频样本缺少缓冲".into()))?;
        if self.dmabuf {
            return self.sample_to_dmabuf_frame(sample, buffer);
        }
        // 计时起点：VA 解码器的 surface→系统内存**下载**发生在映射/访问平面时。
        let map_started = Instant::now();
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &self.video_info)
            .map_err(|e| MediaError::Decode(format!("映射视频帧失败: {e}")))?;
        let width = self.video_info.width();
        let height = self.video_info.height();

        // NV12：Y 平面 + 半高 UV 平面，归一化为「UV 跨度 = Y 跨度」的单缓冲
        // （显示侧按同一约定建 NV12 平面扫描缓冲，硬件做 YUV→RGB）。
        if self.video_info.format() == gst_video::VideoFormat::Nv12 {
            // NV12：Y 平面（全高）+ UV 平面（半高），行宽 = width 字节。
            let y = frame
                .plane_data(0)
                .map_err(|e| MediaError::Decode(format!("读取 Y 平面失败: {e}")))?;
            let uv = frame
                .plane_data(1)
                .map_err(|e| MediaError::Decode(format!("读取 UV 平面失败: {e}")))?;
            let y_stride = self
                .video_info
                .stride()
                .first()
                .copied()
                .and_then(|s| usize::try_from(s).ok())
                .filter(|s| *s >= width as usize)
                .ok_or_else(|| {
                    MediaError::Decode(format!("非法 NV12 Y 跨度: {:?}", self.video_info.stride()))
                })?;
            let strides = self.video_info.stride();
            let uv_stride = strides
                .get(1)
                .copied()
                .and_then(|s| usize::try_from(s).ok())
                .filter(|s| *s >= width as usize)
                .ok_or_else(|| MediaError::Decode(format!("非法 NV12 UV 跨度: {strides:?}")))?;
            let stride = y_stride.max(uv_stride);
            let uv_rows = (height as usize).div_ceil(2);
            // 容量按**声明的目标 stride**与 UV 实际行数算：`height*3/2` 在奇数高度
            // 会少一行（ceil(height/2) > height/2），max(y,uv) stride 也可能大于
            // 任一源平面。
            let total = stride
                .saturating_mul(height as usize)
                .saturating_add(stride.saturating_mul(uv_rows));
            let mut out = Vec::with_capacity(total);
            append_plane(
                &mut out,
                y,
                y_stride,
                width as usize,
                height as usize,
                stride,
            )?;
            self.last_map_ms = map_started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
            let copy_started = Instant::now();
            append_plane(&mut out, uv, uv_stride, width as usize, uv_rows, stride)?;
            self.last_copy_ms = copy_started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
            return Ok(DisplayFrame::Cpu(CpuFrame {
                width,
                height,
                stride: stride as u32,
                format: PixelFormat::Nv12,
                data: Arc::from(out.into_boxed_slice()),
            }));
        }

        let row_bytes = (width as usize).saturating_mul(4);
        let src = frame
            .plane_data(0)
            .map_err(|e| MediaError::Decode(format!("读取像素平面失败: {e}")))?;
        let src_stride = self
            .video_info
            .stride()
            .first()
            .copied()
            .and_then(|s| usize::try_from(s).ok())
            .filter(|s| *s >= row_bytes)
            .ok_or_else(|| {
                MediaError::Decode(format!("非法的视频行跨度: {:?}", self.video_info.stride()))
            })?;

        let format = match self.video_info.format() {
            gst_video::VideoFormat::Rgba => PixelFormat::Rgba8888,
            gst_video::VideoFormat::Bgra => PixelFormat::Argb8888,
            gst_video::VideoFormat::Bgrx => PixelFormat::Xrgb8888,
            _ => {
                return Err(MediaError::Decode(
                    "不支持的输出像素格式（预期 RGBA/BGRA/BGRx/NV12）".into(),
                ));
            }
        };

        self.last_map_ms = map_started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
        let copy_started = Instant::now();
        // 同样不清零：`with_capacity` + extend 只写真实数据（每帧省一次全帧写）。
        let total = row_bytes.saturating_mul(height as usize);
        let mut out = Vec::with_capacity(total);
        append_plane(
            &mut out,
            src,
            src_stride,
            row_bytes,
            height as usize,
            row_bytes,
        )?;

        self.last_copy_ms = copy_started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
        Ok(DisplayFrame::Cpu(CpuFrame {
            width,
            height,
            stride: row_bytes as u32,
            format,
            data: Arc::from(out.into_boxed_slice()),
        }))
    }
}

impl GstProducer {
    /// 构造期回退次数（S7）：0=首候选即成功；>0=发生过回退（如 DMA-BUF 被拒→NV12）。
    #[must_use]
    pub const fn dmabuf_fallbacks(&self) -> u64 {
        self.dmabuf_fallbacks
    }

    /// 当前是否真的在 DMA-BUF 直通（S7 的结构化判据；不是字符串片段）。
    #[must_use]
    pub const fn is_dmabuf(&self) -> bool {
        self.dmabuf
    }

    /// 当前 DMA-BUF keepalive 环长度（0=未启用或还没出帧；>0=正在持有 surface 引用）。
    ///
    /// 真机可观测：DMA-BUF 模式下应恒等于 [`DMABUF_KEEPALIVE_FRAMES`]（出满帧后），
    /// 而系统内存模式下应为 0。t30 可直接断言这一条。
    #[must_use]
    pub fn dmabuf_keepalive_len(&self) -> usize {
        self.dmabuf_keepalive.borrow().len()
    }

    /// DMA-BUF 样本 → [`DmaBufFrame`]：fd 取自 gstreamer-allocators 的
    /// DMA-BUF 内存；fourcc/modifier 取自 caps 的 `drm-format`（如
    /// `NV12:0x0100000000000002` = i915 Y_TILED）。
    fn sample_to_dmabuf_frame(
        &self,
        sample: &gst::Sample,
        buffer: &gst::BufferRef,
    ) -> Result<DisplayFrame, MediaError> {
        // 1. drm-format 解析。
        let caps = sample
            .caps()
            .ok_or_else(|| MediaError::Decode("DMA-BUF 样本缺少 caps".into()))?;
        let s = caps
            .structure(0)
            .ok_or_else(|| MediaError::Decode("caps 无结构".into()))?;
        let drm_format: String = s
            .get("drm-format")
            .map_err(|_| MediaError::Decode("caps 缺少 drm-format".into()))?;
        // t73：**线性格式（如 `YU12`）在 caps 里不带 modifier 后缀**——
        // 这里补 `:0`（= LINEAR）后再解析。**不动 `parse_drm_format` 的严格性**
        // （它仍拒绝"缺 modifier"，那条断言是有价值的）。
        let drm_format_normalized;
        let drm_format_ref = if drm_format.contains(':') {
            drm_format.as_str()
        } else {
            drm_format_normalized = format!("{drm_format}:0");
            drm_format_normalized.as_str()
        };
        let (fourcc, modifier) = parse_drm_format(drm_format_ref)?;
        let width = s
            .get::<i32>("width")
            .map_err(|_| MediaError::Decode("caps 缺少 width".into()))? as u32;
        let height =
            s.get::<i32>("height")
                .map_err(|_| MediaError::Decode("caps 缺少 height".into()))? as u32;

        // 2. 平面布局（VideoMeta）。
        let meta = buffer
            .meta::<gst_video::VideoMeta>()
            .ok_or_else(|| MediaError::Decode("样本缺少 VideoMeta".into()))?;
        let mut strides = [0u32; 4];
        let mut offsets = [0u32; 4];
        for i in 0..(meta.n_planes().min(4) as usize) {
            strides[i] = meta.stride()[i].max(0) as u32;
            offsets[i] = meta.offset()[i] as u32;
        }

        // 3. 多 GEM object 守卫：`DmaBufFrame` 只持有一个 fd，语义是
        //    "所有平面都在同一个 DRM object 里"（平面差异由 strides/offsets 表达）。
        //    libva 允许一个 object 一个 fd；真机上 iHD/va 插件对本例是单 object，
        //    但**不能假设**：一旦出现多内存块就直接拒绝并回落，绝不交出错误 fd。
        guard_dmabuf_layout(buffer.n_memory() as u32, meta.n_planes(), fourcc)?;

        // 4. DMA-BUF fd：gstreamer-allocators 的 DmaBufMemory 下转。
        let mem = buffer
            .memory(0)
            .ok_or_else(|| MediaError::Decode("DMA-BUF 样本无内存块".into()))?;
        let fd_mem = mem
            .downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>()
            .ok_or_else(|| MediaError::Decode("内存块不是 DMA-BUF 内存".into()))?;
        // dup 一份为 OwnedFd（跨线程/跨模块持有）。
        let owned = dup_fd(fd_mem.fd())?;
        // 只记录一次：证明"协商成功 + 取值来源"，便于真机核对。
        if !self.dmabuf_logged.get() {
            self.dmabuf_logged.set(true);
            tracing::info!(
                drm_format = %drm_format,
                fourcc = format!("0x{fourcc:08X}"),
                modifier = format!("0x{modifier:016X}"),
                planes = meta.n_planes(),
                strides = ?strides,
                offsets = ?offsets,
                "DMA-BUF 协商成功（fd/fourcc/modifier/strides/offsets 全部来自 caps+VideoMeta）"
            );
        }

        // **归属判别（t81）**：`gstreamer-rs` 0.24 没有 `Buffer::pool()`，无法直接问“这帧来自哪个池”。
        // 改用**更强的判别法**：把自有池的 `max_buffers` 设得**小于**我们持有环的容量
        // （例如 max=2、环=3）——若 VPP 真的用我们的池，它必然**取不到缓冲**（卡住/报错/掉帧）；
        // 若行为完全不变，则说明 **VPP 根本没用我们的池**（挂池这条路没生效）。
        // 这比“问归属”更硬：它不需要任何新 API，且结果不可伪造。
        if std::env::var("LIUGUANG_VIDEO_OWN_POOL_PROBE").is_ok() {
            let seen = self.pool_owner_logged.get().saturating_add(1);
            self.pool_owner_logged.set(seen);
            if seen == 1 || seen % 60 == 0 {
                tracing::info!(
                    frames = seen,
                    own_pool_ptr = format!("0x{:x}", OWN_POOL_PTR.with(std::cell::Cell::get)),
                    "归属判别运行中（own_pool + 小 max_buffers ⇒ 若被采用应出现饥饿）"
                );
            }
        }
        // S1（t31）：**持有**这个 sample（不是只 dup fd）——见 `dmabuf_keepalive` 注释。
        // 显示侧会跨帧保留该内存；只 dup fd 挡不住池把同一 surface 交回解码器重写。
        let keepalive_len = {
            let mut ring = self.dmabuf_keepalive.borrow_mut();
            keepalive_push(&mut ring, sample, self.keepalive_cap);
            ring.len()
        };
        // 真机可观测（S1/S7 的"持有"证据）：首帧 + 每 120 帧一条 INFO（≈2s@60fps）。
        let seen = self.keepalive_logged.get().saturating_add(1);
        self.keepalive_logged.set(seen);
        if seen == 1 || seen % 120 == 0 {
            tracing::info!(
                keepalive = keepalive_len,
                frames = seen,
                "DMA-BUF keepalive：正持有最近若干帧的 gst::Sample（防止池重写仍在扫出的 surface）"
            );
        }
        // GStreamer 的 dma-buf 只有一个 GEM 对象（两个平面共用同一 fd），
        // 因此 `object_index` 全 0、`fds` 只有一项——与 NVIDIA 的"每平面一个 fd"
        // 区分开，由 `DmaBufFrame` 统一表达。
        Ok(DisplayFrame::DmaBuf(
            liuguang_core::types::DmaBufFrame::single_object(
                width,
                height,
                strides[0],
                liuguang_core::types::PixelFormat::Nv12,
                std::sync::Arc::new(owned),
                modifier,
                fourcc,
                strides,
                offsets,
            ),
        ))
    }
}

/// 已知的「DMABuf 协商被下游拒绝」签名 → 可执行的解释。
///
/// 真机实测（gst 1.28.8 的 `va` 插件，`gstvabasedec.c`）：
/// ```text
/// ERROR vah264dec gstvabasedec.c:506 gst_va_base_dec_decide_allocation:
///       DMABuf caps negotiated without the mandatory support of VideoMeta
/// ERROR vah264dec gstvabasedec.c:1135 gst_va_base_dec_prepare_output_frame:
///       Failed to negotiate with downstream
/// 错误：来自组件 qtdemux0：Internal data stream error.  (not-negotiated)
/// ```
/// 即：**va 插件要求下游的 allocation query 声明 `GstVideoMeta`**；
/// `appsink`/`fakesink`（GstBaseSink）不声明，`kmssink` 声明 ⇒
/// `vah264dec ! kmssink` 能拿到 `memory:DMABuf`，而进 appsink 一定失败。
/// 这不是"少一个环境变量"，是下游能力问题。
/// 环境变量：把 DMA-BUF 的 `drm-format` modifier **钉死**为指定值。
///
/// 取值是十六进制 modifier（可带 `0x` 前缀），例如：
/// - `0x0` = **LINEAR**
/// - `0x0100000000000002` = `I915_FORMAT_MOD_Y_TILED`
/// - `0x0100000000000004` = `I915_FORMAT_MOD_Yf_TILED`（NV12 在内核里的**原生**项）
///
/// **默认不设** ⇒ 不约束 `drm-format`，由驱动（iHD）自行决定（保持历史行为）。
///
/// ## 与旧开关的关系
/// 旧开关 `LIUGUANG_VIDEO_DMABUF_LINEAR=1` **仍被识别**，等价于
/// `LIUGUANG_VIDEO_DMABUF_MODIFIER=0x0`（**向后兼容**，不必改已有脚本）；
/// 新变量优先。两者都不设 ⇒ 不约束。
pub const ENV_DMABUF_MODIFIER: &str = "LIUGUANG_VIDEO_DMABUF_MODIFIER";

/// 旧开关（**兼容保留**）：非空且不为 `0/off/false/no` ⇒ 等价于 `MODIFIER=0x0`。
pub const ENV_DMABUF_LINEAR: &str = "LIUGUANG_VIDEO_DMABUF_LINEAR";

/// 解析结果：要不要约束 modifier，约束成什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufModifierChoice {
    /// 不约束（默认；由驱动决定）。
    Unconstrained,
    /// 显式要求该 modifier。
    Pinned(u64),
}

/// 解析环境变量文本 → modifier 选择（**纯函数，可单测**）。
///
/// 规则：
/// - `LIUGUANG_VIDEO_DMABUF_MODIFIER` 存在且可解析 ⇒ `Pinned(值)`；
/// - 否则若旧开关 `LIUGUANG_VIDEO_DMABUF_LINEAR` 为真 ⇒ `Pinned(0)`（LINEAR）；
/// - 否则 `Unconstrained`。
///
/// **非法值不会静默变成"不约束"**：返回 `Err`，调用方必须大声告警并**放弃实验**
/// （否则会得到"以为在测 Yf_TILED、其实没约束"的假实验）。
///
/// # Errors
///
/// 值不是合法十六进制 `u64` 时返回可读错误。
pub fn parse_dmabuf_modifier(
    modifier: Option<&str>,
    linear: Option<&str>,
) -> Result<DmabufModifierChoice, String> {
    if let Some(raw) = modifier.map(str::trim).filter(|s| !s.is_empty()) {
        let cleaned = raw
            .strip_prefix("0x")
            .or_else(|| raw.strip_prefix("0X"))
            .unwrap_or(raw);
        return u64::from_str_radix(cleaned, 16)
            .map(DmabufModifierChoice::Pinned)
            .map_err(|e| {
                format!(
                    "非法 {ENV_DMABUF_MODIFIER}={raw:?}（应为十六进制 modifier，如 0x0 / 0x0100000000000004）：{e}"
                )
            });
    }
    let linear_on = linear
        .map(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "off" | "false" | "no"
            )
        })
        .unwrap_or(false);
    Ok(if linear_on {
        DmabufModifierChoice::Pinned(0)
    } else {
        DmabufModifierChoice::Unconstrained
    })
}

/// 由选择构造 DMABuf caps 的**唯一**入口（纯函数，可单测）。
///
/// `Unconstrained` ⇒ 只写 `format=DMA_DRM`（历史行为）；
/// `Pinned(m)` ⇒ 追加 `drm-format=(string)NV12:0x<m>`（**小写十六进制、带 0x**）。
///
/// 注意：这里只构造**字符串**，不依赖 GStreamer 运行时 ⇒ 可在无 gst 的环境单测。
/// 按**像素格式 + modifier 选择**构造 DMABuf caps 字符串（**纯函数，可单测**）。
///
/// `Yu12` ⇒ 请求**线性 I420**（`drm-format=(string)YU12`，无 modifier 后缀 = linear）；
/// `Nv12` ⇒ 沿用 [`dmabuf_caps_string`]（不约束或钉死某 modifier）。
#[must_use]
pub fn dmabuf_caps_for(fmt: DmabufPixFmt, choice: DmabufModifierChoice) -> String {
    match fmt {
        DmabufPixFmt::Yu12 => {
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=(string)YU12".to_string()
        }
        DmabufPixFmt::Nv12 => dmabuf_caps_string(choice),
    }
}

#[must_use]
pub fn dmabuf_caps_string(choice: DmabufModifierChoice) -> String {
    match choice {
        DmabufModifierChoice::Unconstrained => {
            "video/x-raw(memory:DMABuf),format=DMA_DRM".to_string()
        }
        DmabufModifierChoice::Pinned(m) => {
            format!("video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=(string)NV12:0x{m:x}")
        }
    }
}

pub const DMABUF_VIDEOMETA_LIMITATION: &str = "DMABuf 协商被拒：本平台 va 插件要求下游 allocation query 声明 GstVideoMeta（appsink/fakesink 不声明、kmssink 声明）。这就是 `vah264dec ! appsink` 报 `not-negotiated` / `Internal data stream error` 的根因；已回退系统内存 NV12（CPU 上传）路径。";

/// 「DMABuf 被下游拒绝」的**归因分级**（t31/S6）。
///
/// 审计指出：旧的单一文本匹配会把**任何**协商/流错误都归因到
/// "va 插件要求 VideoMeta"这一条——而那条**已由 t20 修好**，再套上去就是错误归因
/// （与"kmssink 3%"同类的误判）。因此分两级：
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufRefusalKind {
    /// 命中插件**自己的**原始文本 ⇒ 可以断言根因（这是证据，不是猜测）。
    VideoMeta,
    /// 属于"被下游拒绝"这一类，但**没有**已知根因签名 ⇒ 诚实报告"未识别根因"。
    Unclassified,
    /// 不属于这一类。
    NotRefusal,
}

/// 分级判定（纯函数，便于单测钉住真机原始文本）。
#[must_use]
pub fn classify_dmabuf_refusal(text: &str) -> DmabufRefusalKind {
    let t = text.to_ascii_lowercase();
    if t.contains("mandatory support of videometa") {
        return DmabufRefusalKind::VideoMeta;
    }
    if (t.contains("not-negotiated") || t.contains("internal data stream error"))
        || t.contains("no valid frames")
        || t.contains("failed to negotiate with downstream")
    {
        return DmabufRefusalKind::Unclassified;
    }
    DmabufRefusalKind::NotRefusal
}

/// 判断错误文本是否属于「DMABuf 被下游拒绝」这一类（保持既有语义与调用方兼容）。
///
/// 纯函数，便于单测钉住真机原始文本。
#[must_use]
pub fn looks_like_dmabuf_refusal(text: &str) -> bool {
    classify_dmabuf_refusal(text) != DmabufRefusalKind::NotRefusal
}

/// 按行把平面拷贝进目标缓冲（源跨度可大于有效行宽，做去填充）。
/// 把源平面逐行 append 到 `out`（**不预先把目标清零**）。
///
/// 为什么不用 `vec![0u8; N]` + `copy_plane`：那会先把整帧写一遍 0 再覆盖，
/// 4K NV12 每帧白写 13.8 MB（实测约占 CPU 路径 8%）。`Vec::with_capacity` +
/// `extend_from_slice` 只写真实数据与必要的行尾填充。
fn append_plane(
    out: &mut Vec<u8>,
    src: &[u8],
    src_stride: usize,
    row_bytes: usize,
    rows: usize,
    dst_stride: usize,
) -> Result<(), MediaError> {
    let pad = dst_stride.saturating_sub(row_bytes);
    for y in 0..rows {
        let start = y.saturating_mul(src_stride);
        let row = src
            .get(start..start.saturating_add(row_bytes))
            .ok_or_else(|| MediaError::Decode("视频平面数据不足".into()))?;
        out.extend_from_slice(row);
        if pad > 0 {
            out.extend(std::iter::repeat_n(0u8, pad));
        }
    }
    Ok(())
}

#[cfg(test)]
fn copy_plane(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<(), MediaError> {
    if src_stride == dst_stride && dst_stride == row_bytes {
        let total = row_bytes.saturating_mul(rows);
        let src_all = src
            .get(..total)
            .ok_or_else(|| MediaError::Decode("视频平面数据不足".into()))?;
        dst.get_mut(..total)
            .ok_or_else(|| MediaError::Decode("目标平面缓冲不足".into()))?
            .copy_from_slice(src_all);
        return Ok(());
    }
    // 源与目标的行跨度可以不同（NV12 的 Y/UV 平面常被驱动对齐成不同 stride）：
    // 行起点必须各自按**自己的** stride 计算，否则第二行起就错位。
    for y in 0..rows {
        let start = y.saturating_mul(src_stride);
        let src_row = src
            .get(start..start.saturating_add(row_bytes))
            .ok_or_else(|| MediaError::Decode("视频平面数据不足".into()))?;
        let dstart = y.saturating_mul(dst_stride);
        let dst_row = dst
            .get_mut(dstart..dstart.saturating_add(row_bytes))
            .ok_or_else(|| MediaError::Decode("目标平面缓冲不足".into()))?;
        dst_row.copy_from_slice(src_row);
    }
    Ok(())
}

/// 复制裸 fd 为 [`OwnedFd`]。
///
/// # Safety
///
/// 仅在 `raw` 为本进程有效打开文件描述符时调用（GStreamer 样本内存
/// 持有期内）；`BorrowedFd::borrow_raw` 的唯一 unsafe 点。
#[allow(unsafe_code)]
fn dup_fd(raw: std::os::unix::io::RawFd) -> Result<std::os::fd::OwnedFd, MediaError> {
    use std::os::fd::BorrowedFd;
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
    borrowed
        .try_clone_to_owned()
        .map_err(|e| MediaError::Decode(format!("dup DMA-BUF fd 失败: {e}")))
}

/// 解析 caps `drm-format`（`FOUR:0xMOD`）为 (fourcc, modifier)。
fn parse_drm_format(s: &str) -> Result<(u32, u64), MediaError> {
    let (fourcc_str, mod_str) = s
        .split_once(':')
        .ok_or_else(|| MediaError::Decode(format!("drm-format 无 modifier: {s}")))?;
    let bytes = fourcc_str.as_bytes();
    if bytes.len() != 4 {
        return Err(MediaError::Decode(format!("drm-format fourcc 非法: {s}")));
    }
    let fourcc = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let modifier = u64::from_str_radix(mod_str.trim_start_matches("0x"), 16)
        .map_err(|e| MediaError::Decode(format!("drm-format modifier 非法: {e}")))?;
    Ok((fourcc, modifier))
}

impl FrameProducer for GstProducer {
    fn info(&self) -> MediaInfo {
        self.info.clone()
    }

    fn dmabuf_state(&self) -> Option<DmabufState> {
        Some(GstProducer::dmabuf_state(self))
    }

    fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError> {
        if self.finished {
            return Ok(VideoPoll::Ended);
        }
        self.poll_bus()?;
        // 至多尝试若干样本：水位以下直接丢弃（省掉 12–33MB 的像素转换）。
        for _ in 0..MAX_SKIP_PER_CALL {
            let Some(sample) = self.try_pull_sample() else {
                self.pull_none += 1;
                if self.pull_none % 1000 == 0 {
                    tracing::debug!(
                        pulled = self.pull_some,
                        empty = self.pull_none,
                        eos_seen = self.eos_seen,
                        appsink_eos = self.appsink_eos(),
                        has_eos_property = self.has_eos_property,
                        finished = self.finished,
                        "视频拉取空手（诊断计数）"
                    );
                }
                if self.eos_seen || self.appsink_eos() {
                    self.finished = true;
                    tracing::info!(
                        pulled = self.pull_some,
                        empty = self.pull_none,
                        "解码管线到达 EOS（本段播放结束）"
                    );
                    return Ok(VideoPoll::Ended);
                }
                return Ok(VideoPoll::NeedMore);
            };
            self.pull_some += 1;
            let pts = match sample.buffer().and_then(|b| b.pts()) {
                Some(t) => Duration::from_nanos(t.nseconds()),
                None => {
                    let fallback = self.next_fallback_pts;
                    self.next_fallback_pts =
                        self.next_fallback_pts.saturating_add(self.frame_interval);
                    fallback
                }
            };
            if let Some(horizon) = self.drop_horizon {
                if pts < horizon {
                    self.skipped += 1;
                    continue;
                }
            }
            let frame = self.sample_to_frame(&sample)?;
            self.map_ms_sum += u64::from(self.last_map_ms);
            self.copy_ms_sum += u64::from(self.last_copy_ms);
            self.timed_frames += 1;
            if self.timed_frames >= TIMING_WINDOW {
                let n = u64::from(self.timed_frames);
                tracing::info!(
                    frames = n,
                    map_ms_avg = self.map_ms_sum / n,
                    copy_ms_avg = self.copy_ms_sum / n,
                    element = %self.decoder_element,
                    capture = self.capture_mode,
                    "媒体侧每帧耗时（map=解码器下载/映射, copy=行拷贝）"
                );
                self.map_ms_sum = 0;
                self.copy_ms_sum = 0;
                self.timed_frames = 0;
            }
            return Ok(VideoPoll::Frame { pts, frame });
        }
        Ok(VideoPoll::NeedMore)
    }

    fn seek_start(&mut self) -> Result<(), MediaError> {
        tracing::info!("循环重播：seek 到流起点");
        self.pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_seconds(0),
            )
            .map_err(|e| MediaError::Decode(format!("seek 到流起点失败: {e}")))?;
        self.finished = false;
        self.eos_seen = false;
        self.next_fallback_pts = Duration::ZERO;
        Ok(())
    }

    fn decoder_kind(&self) -> DecoderKind {
        self.decoder_kind
    }

    fn set_drop_horizon(&mut self, horizon: Option<Duration>) {
        self.drop_horizon = horizon;
    }

    fn skipped_frames(&self) -> u64 {
        self.skipped
    }

    fn set_playing(&mut self, playing: bool) -> Result<(), MediaError> {
        let state = if playing {
            gst::State::Playing
        } else {
            gst::State::Paused
        };
        self.pipeline
            .set_state(state)
            .map_err(|e| MediaError::Decode(format!("切换解码管线状态失败: {e}")))?;
        Ok(())
    }
}

/// 拆除管线时等待 NULL 状态的上限（不阻塞退出路径无限期挂起）。
const TEARDOWN_WAIT: gst::ClockTime = gst::ClockTime::from_seconds(3);

impl Drop for GstProducer {
    fn drop(&mut self) {
        // 顺序很重要：
        // 1. 断开 qtdemux 的 `pad-added` 回调——回调闭包持有 h264parse 的
        //    sink pad 强引用，不断开会拖住下游元素（进而流线程）；
        // 2. 把管线切到 NULL 并**等到状态真的变成 NULL**：`set_state` 只是
        //    发起异步状态转换，立即返回后马上释放引用，qtdemux 的流线程会
        //    残留（独立验证在 mock 后端复现：每次"视频→静态"泄漏 1 个
        //    `qtdemux:sink` 线程，切壁纸够多就累积）。`Element::state()`
        //    会阻塞至状态转换完成（带超时，避免退出路径卡死）。
        if let Some(id) = self.pad_added.take() {
            self.demux.disconnect(id);
        }
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            tracing::warn!(error = %e, "视频管线切换到 NULL 失败（资源可能在进程退出时释放）");
        }
        let (result, current, _pending) = self.pipeline.state(TEARDOWN_WAIT);
        if current != gst::State::Null {
            tracing::warn!(
                ?result,
                ?current,
                "视频管线未在期限内进入 NULL：流线程可能延迟退出"
            );
        } else {
            tracing::debug!("视频管线已进入 NULL（流线程已停止）");
        }
    }
}

/// 在 allocation 查询里补上 `GstVideoMeta`（只对 ALLOCATION 查询生效）。
///
/// 为什么必须这么做：`va` 插件的 DMABuf 输出**硬要求**下游在 allocation query 里
/// 声明 `GstVideoMeta`（`gstvabasedec.c: gst_va_base_dec_decide_allocation`），
/// 否则报 `DMABuf caps negotiated without the mandatory support of VideoMeta`
/// → `not-negotiated` → 上层看到 `qtdemux Internal data stream error`。
/// `appsink`（GstBaseSink）默认不声明它；`kmssink` 声明，所以
/// `vah264dec ! kmssink` 能协商成功而 `vah264dec ! appsink` 必然失败。
///
/// 这里直接调 C API 追加 meta：信号回调期间查询对象仍被 GStreamer 持有
/// （引用计数 > 1），走 `view_mut()` 会因"不可写"断言失败，而
/// `gst_query_add_allocation_meta` 正是 C 侧子类在 vfunc 里做的事（追加一个条目）。
#[allow(unsafe_code)]
fn advertise_video_meta(query: &gst::QueryRef) -> bool {
    let is_allocation = unsafe { (*query.as_ptr()).type_ == gst::ffi::GST_QUERY_ALLOCATION };
    if !is_allocation {
        return false;
    }
    let meta_type = <gst_video::VideoMeta as gst::MetaAPI>::meta_api();
    if meta_type == gstreamer::glib::Type::INVALID {
        return false;
    }
    unsafe {
        gst::ffi::gst_query_add_allocation_meta(
            query.as_ptr() as *mut gst::ffi::GstQuery,
            gstreamer::glib::translate::IntoGlib::into_glib(meta_type),
            std::ptr::null(),
        );
    }
    true
}

/// 在 `appsink` 的 sink pad 上挂一个**只追加 VideoMeta 的 allocation 查询探针**。
///
/// 背景（t9/t20 真机一手证据）：`va` 插件的 DMABuf 输出**硬要求**下游 allocation
/// query 声明 `GstVideoMeta`（`gstvabasedec.c:506`：
/// `DMABuf caps negotiated without the mandatory support of VideoMeta` → `not-negotiated`
/// → 上层看到 `qtdemux Internal data stream error`）。查询**确实送到了 appsink**
/// （`GST_DEBUG=GST_PADS:6` 可见 `vah264dec0:src peer query (allocation)` →
/// `appsink0:sink sent query ... result 0`），但 appsink 自己不会声明该 meta。
///
/// 本函数用 **pad 探针**（纯安全 API，不注册 GType、不改 vfunc 表）实现
/// "覆写 query 处理但链回原处理"：
/// 探针只对 ALLOCATION 查询调用 [`advertise_video_meta`]（内部即
/// `gst_query_add_allocation_meta(GST_VIDEO_META_API_TYPE)`），随后返回
/// [`gst::PadProbeReturn::Ok`] **放行**，appsink/GstBaseSink 照常处理
/// （pool/allocator 等仍由基类补齐）。meta 是追加到查询对象上的，因此
/// 上游（解码器）拿到回答时能看到它。
///
/// 为什么不用 `propose-allocation` 信号：真机实测该信号**从未被调用**
/// （连接成功、调用计数 0），它在本版本 GStreamer 里不驱动这次查询。
///
/// 返回是否成功挂上（挂不上要大声告警：DMABuf 一定协商不起来）。
fn attach_video_meta_query_probe(pad: &gst::Pad) -> bool {
    let logged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let id = pad.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, move |_pad, info| {
        if let Some(query) = info.query() {
            let added = advertise_video_meta(query);
            if added && !logged.swap(true, std::sync::atomic::Ordering::SeqCst) {
                tracing::info!(
                    "已在 allocation 查询中声明 GstVideoMeta（va 插件 DMABuf 输出的硬要求）"
                );
            }
        }
        // Ok = 继续正常处理（链回原处理），不是 Handled/Drop。
        gst::PadProbeReturn::Ok
    });
    id.is_some()
}

/// DRM fourcc `NV12`（'NV12' 小端 = 0x3231564E）。
/// DMA-BUF keepalive 环容量（t31/S1）：显示侧跨帧保留 2 帧 + 1 帧在途。
///
/// **不要随意调大**：解码器的 surface 池有限，持有过多会让它等不到空 surface
/// 而**变慢或卡顿**（真机用 fps/CPU/提交计数守住，见 t31 报告）。
/// **t41 曾把它调到 8；t43 的目视实验因部署失败而无效（见下）⇒ 暂恢复 3（未经证伪，只是未验证）**。
///
/// **t43 的部署事故（别再犯）**：把 `/tmp` 里上传的二进制直接 `mv` 进 `/usr/local/bin/`
/// ⇒ 它保留了 `/tmp` 的 **SELinux 上下文**，systemd 拒绝执行（`status=203/EXEC`，
/// 文件模式 0755 正常 ⇒ 排除权限位，剩下就是标签/LSM）⇒ **`keepalive=8` 那次"用户目视"是在
/// 服务已失败（无画面）时给出的，不能作为证据** ⇒ **8 仍未验证**（不是已证伪）。
/// 正确做法：`install -m 0755 <bin> /usr/local/bin/liuguang` **并** `restorecon /usr/local/bin/liuguang`
/// （或 `mv` 后用 `ls -Z` 核对上下文为 `bin_t`），然后再 `restart` 验证 `is-active`。
///
/// **为什么调大过（t41）**：3 的假设是"显示侧跨帧保留 2 帧 + 1 帧在途就够"，
/// 但用户报告**直通路径画面撕裂**（`sample_20fps.mp4`：源 20fps、1920×1088 → 3840×2400 放大 2×、
/// i915 Y_TILED），而**同一素材走 CPU 上传路径不撕裂**。撕裂形态是"同一帧内混了两帧内容"，
/// 典型成因是**"解码器在写一块正在被扫出的内存"**：直通路径下 surface 被直接送去扫描，
/// 随后又被池回收去解码下一帧 ⇒ 把持有窗口放宽到 8，作为**便宜的实验**。
///
/// **诚实边界（别把缓解当修复）**：
/// - 多持几个 `gst::Sample` 只是让"媒体侧仍引用的 surface"更多，**并不与扫描出同步**；
///   若池在**引用归零后**回收，那么**只要没有"这帧已扫完"的信号，复用就仍可能发生**（只是概率变低）；
/// - 真正的修法是"**持有该 DMA-BUF 直到对应的 page-flip 完成**"，而全仓 `PAGE_FLIP_EVENT` 无命中
///   ⇒ 目前**无法知道某块缓冲何时扫完**（t28/§G1、t41 结论）；
/// - **代价**：surface 池有限，持有过多会逼解码器等空 surface ⇒ **可能变慢或卡顿**；
///   调大后必须复测 fps / CPU / `commit.failed` / `dropped_late`。
pub const DMABUF_KEEPALIVE_FRAMES: usize = 4;

/// **t71 实测结论（重要，别重复走）**：**加大 keepalive 并不能消除"缓冲在扫描输出期间被改写"**。
///
/// 判据：`LIUGUANG_FRAME_PROBE_POST=1`（提交后再 dump **同一缓冲**，与提交前逐字节比）。
/// 实测（真实 drm、`Y_TILED`、1920×1080，环长由 `LIUGUANG_DMABUF_KEEPALIVE` 控制并**确认生效**）：
///
/// | 环容量 | pre/post 相同 | 日志确认 |
/// |---|---|---|
/// | 3（默认） | **6/7** | `keepalive=3` ✓ |
/// | 8 | **5/7** | `keepalive=8` ✓ |
/// | 16 | **6/7** | `keepalive=16` ✓ |
///
/// ⇒ **容量 3/8/16 都一样有改写** ⇒ "多持有 `gst::Sample`"**不是**这个问题的解。
/// （同时更正历史：t43 那次 `keepalive=8` 的目视**因 SELinux 部署事故无效**，见上；
/// 而 t71 这次是**字节级判据**，不依赖目视。）
///
/// **推论**：VAAPI 的 surface 池很可能**不因持有 `GstBuffer` 而钉住底层 VA surface**
/// （池由 va 插件/libva 管理）⇒ 想靠"多持 sample"来同步扫描输出**这条路不通**。
/// 下一步应转向 **t64 的修法 B**：让 `vapostproc` 输出**线性 I420（YU12）**
/// （已实测可协商），彻底摆脱 tile 语义与"写正在扫出的 surface"这一整类问题。
pub const DMABUF_KEEPALIVE_MEASURED_INEFFECTIVE: () = ();

/// 环境变量：覆盖 keepalive 环容量（**实验/排障用**，默认 [`DMABUF_KEEPALIVE_FRAMES`]）。
///
/// 为什么需要它：**"按帧数"是否足够，取决于"解码器超前产出多少帧"**——
/// 若解码器在"某帧被扫出"之前就产出了 >cap 帧，该帧会被挤出环 ⇒ 池复用 ⇒ 局部重写。
/// 因此容量必须能**实测扫描**，而不是拍脑袋定一个常数。
pub const ENV_DMABUF_KEEPALIVE: &str = "LIUGUANG_DMABUF_KEEPALIVE";

/// keepalive 环容量的**硬上限**（防误配把 surface 池耗干 ⇒ 解码器等不到空 surface）。
///
/// 超过上限**夹紧并告警**（不是静默接受）：这是"丢帧优于卡死"的落点。
pub const DMABUF_KEEPALIVE_MAX: usize = 32;

/// 解析 keepalive 容量（**纯函数，可单测**）。
///
/// - 未设 / 空 / 非法 ⇒ 默认值；
/// - `0` ⇒ **停用 keepalive**（显式选择，用于对照实验）；
/// - `> DMABUF_KEEPALIVE_MAX` ⇒ **夹紧到上限**并返回 `clamped=true` 以便告警。
#[must_use]
pub fn parse_keepalive_cap(raw: Option<&str>) -> (usize, bool) {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return (DMABUF_KEEPALIVE_FRAMES, false);
    };
    match text.parse::<usize>() {
        Ok(v) if v <= DMABUF_KEEPALIVE_MAX => (v, false),
        Ok(_) => (DMABUF_KEEPALIVE_MAX, true),
        Err(_) => (DMABUF_KEEPALIVE_FRAMES, false),
    }
}

/// DMA-BUF keepalive 环的推进策略（纯函数，便于离线单测）：
/// 追加最新 sample，并丢弃超出容量的最旧者（丢弃即释放那一帧的引用）。
fn keepalive_push(
    ring: &mut std::collections::VecDeque<gst::Sample>,
    sample: &gst::Sample,
    cap: usize,
) {
    ring.push_back(sample.clone());
    while ring.len() > cap {
        ring.pop_front();
    }
}

/// DRM fourcc `NV12` 的小端数值（`'NV12'` = 0x3231_564E）。
pub const NV12_FOURCC: u32 = 0x3231_564E;

/// DRM fourcc `YUV420`（gst 写作 `YU12`，即 I420；小端 = 0x3231_5559）。
///
/// **t73 修法 B**：让 `vapostproc` 输出**线性 I420**。
/// - `t64` 实测：`vapostproc` 的 DMABuf caps **含 `YU12`**，`gst-launch` 可协商；
/// - 而 **LINEAR NV12（`NV12:0x0`）驱动层不提供**（`t64` 实测链接失败）⇒ 不要走那条；
/// - 收益：`modifier=0x0` 摆脱 tile 语义；且 VPP 产出**自己的缓冲** ⇒
///   显示扫描期间没有解码器在写它（与被扫 surface 解耦）。
pub const YU12_FOURCC: u32 = 0x3231_5559;

/// DMA-BUF 像素格式选择（`LIUGUANG_VIDEO_DMABUF_FORMAT`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufPixFmt {
    /// 解码器原生 NV12（tiled，历史默认路径）。
    Nv12,
    /// 线性 I420（`YU12`，经 `vapostproc` 输出）。
    Yu12,
}

/// 环境变量：选择 DMA-BUF 像素格式（`nv12`（默认）/ `yu12`）。
pub const ENV_DMABUF_FORMAT: &str = "LIUGUANG_VIDEO_DMABUF_FORMAT";

/// 环境变量：**强制**在解码器后插入 `vapostproc`（并设 `disable-passthrough=true`）。
///
/// **为什么需要"强制"**（t77 实测）：
/// - 失败基线是 `vah264dec(dmabuf)` **直接上屏**（日志里根本没有 VPP）⇒ "VPP 已在链路上"不成立；
/// - 而 `vapostproc` 在**格式未变**时会 **passthrough**（直接跳过、不拷贝）；
/// - 我们唯一"能产出且内核能扫"的目标格式是 `NV12:tiled`（**与输入同格式**，t64/t73 实测）
///   ⇒ **必须 `disable-passthrough=true`** 才能强制它做真实 blit。
///
/// 目的：让被扫描的缓冲成为 **VPP 产出**的缓冲，而不是解码器的 surface
/// ⇒ 解码器下一帧不会再写它（解耦）。
pub const ENV_FORCE_VPP: &str = "LIUGUANG_VIDEO_FORCE_VPP";

/// 环境变量：让 `vapostproc` 写进**我们自己的 `GstBufferPool`**（t77b）。
///
/// **为什么需要自有池**（t71/t80 实测）：
/// - 持有**解码器**的 `gst::Sample` 无效（VA 池由 va 插件/libva 管，引用计数够不到）⇒ 容量 3/8/16 无差别；
/// - 只让 VPP 拷贝也无效（VPP 自己的输出池同样会在扫描期间被回收再写）⇒ pre/post 仍 5/7。
///
/// ⇒ **只有池归我们所有、由我们 `acquire/release`**，才能保证"被扫那块没人在写"。
///
/// 机制：在 `vapostproc` 的 **src pad** 上挂 probe，拦截**上游方向**的 `ALLOCATION` 查询，
/// 把我们自己的 [`gst_video::VideoBufferPool`] 塞进去 ⇒ VPP 从我们的池取缓冲。
pub const ENV_OWN_POOL: &str = "LIUGUANG_VIDEO_OWN_POOL";

/// **`t97`：全仓第二处 `unsafe`**（第一处是 `t84` 的 `export_write_fence`）。
///
/// 原因：`GstVaPool` 没有 Rust 绑定（`gstreamer-va` crate 不存在），
/// 而 `vapostproc` 只在 `GST_IS_VA_POOL(pool)` 为真时才采用外部池
/// （`gstvabasetransform.c:376`，`t85` 读源码结论）。
///
/// 链接方式见 `crates/liuguang-media/build.rs`：`-l:libgstva-1.0.so.0`
/// （本机没有 `libgstva-1.0.so` 开发符号链接，`#[link]` 不可用）。
///
/// # SAFETY
/// **仅此一处**：`extern "C"` 声明的签名必须与 `libgstva-1.0.so.0` 导出的符号一致
/// （`t96` 已用 `nm -D` 核对：`gst_va_pool_new`、`gst_va_pool_new_with_config`、
/// `gst_va_pool_get_type` 均为全局定义）。调用只在 GStreamer 已 `init` 之后进行；
/// 返回值按 `glib` 的 `transfer full` 语义交给 `BufferPool` 接管，避免重复释放。
#[allow(unsafe_code)]
#[allow(dead_code)]
mod va_ffi {
    unsafe extern "C" {
        /// 创建一个 `GstVaPool`（`transfer full`，返回 `GstBufferPool*`；用 `glib::Object` 指针接收）。
        pub fn gst_va_pool_new() -> *mut std::ffi::c_void;
        /// 用 config 创建（`config` 为 `GstStructure*`；`transfer full`）。
        pub fn gst_va_pool_new_with_config(config: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }
}

/// 在 `vpp` 的 **src pad** 上挂 probe，把我们**自己的** `GstBufferPool` 塞进
/// **上游方向**的 `ALLOCATION` 查询 ⇒ VPP 从我们的池 `acquire_buffer`。
///
/// **为什么这是 t77b 的关键**（t71/t80 实测）：
/// - 持**解码器**的 sample 无效（VA 池由 va 插件/libva 管，引用计数够不到）；
/// - 只让 VPP 拷贝也无效（VPP 自带池同样会在扫描期间被回收再写）。
///
/// ⇒ 只有当**池归我们所有**，`VecDeque` 里持有的 sample 才真正让池无法把同一块再发出去。
///
/// 不需要 `unsafe`：`PadProbeData::Query(&mut QueryRef)` 本身就是可变借用。
fn make_own_pool(
    vpp: &gst::Element,
    caps: &gst::Caps,
    max_buffers: u32,
) -> Result<gst::BufferPool, MediaError> {
    use gst::prelude::*;

    let pool = gst_video::VideoBufferPool::new();
    // caps 决定缓冲格式/大小；min=2 / max 由我们控制（持有窗口靠它）。
    let mut cfg = pool.config();
    // `t92`：让池声明 `GstVideoMeta`（VA 侧日志 "Video meta is required in buffer"）。
    cfg.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
    // **min 必须给小值（1）**：t86 实测 min==max 会触发断言。
    cfg.set_params(Some(caps), 0, 1, max_buffers);
    pool.set_config(cfg)
        .map_err(|e| MediaError::Decode(format!("配置自有 GstBufferPool 失败: {e}")))?;
    pool.set_active(true)
        .map_err(|e| MediaError::Decode(format!("激活自有池失败: {e}")))?;

    let src = vpp
        .static_pad("src")
        .ok_or_else(|| MediaError::Decode("vapostproc 缺少 src pad".into()))?;
    let pool_for_probe = pool.clone();
    let id = src.add_probe(gst::PadProbeType::QUERY_UPSTREAM, move |_pad, info| {
        if let Some(gst::PadProbeData::Query(q)) = info.data.as_mut()
            && let gst::query::QueryViewMut::Allocation(alloc) = (*q).view_mut()
        {
            // 把**我们的**池加进 ALLOCATION 查询 ⇒ VPP 从我们的池取缓冲。
            alloc.add_allocation_pool(Some(&pool_for_probe), 0, 2, max_buffers);
        }
        gst::PadProbeReturn::Ok
    });
    if id.is_none() {
        return Err(MediaError::Decode("挂 ALLOCATION 探针失败".into()));
    }
    // 池由 probe 闭包与调用方共同持有（活到管线结束）。
    Ok(pool.upcast())
}

/// 解析布尔型环境变量（真值 `1/true/on/yes`）。
#[must_use]
pub fn env_flag(raw: Option<&str>) -> bool {
    raw.map(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "off" | "false" | "no"
        )
    })
    .unwrap_or(false)
}

thread_local! {
    /// 我们自有池的裸指针（仅用于**归属观测**，不参与所有权；池由 probe 闭包持有）。
    static OWN_POOL_PTR: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 自有池 `max_buffers` 默认值 —— **`t86` 实测的最小可行值 = 4**。
///
/// 语义是**硬上限**（M=8/H=9 ⇒ recv 恰好 8 后永久停），不是"变慢"。
/// 配合持有环 3 ⇒ 生产方在归还前拿不到缓冲 ⇒ **饥饿即"等扫完"的背压**。
pub const OWN_POOL_MAX_BUFFERS: u32 = 4;

/// 解析自有池缓冲上限（`LIUGUANG_VIDEO_OWN_POOL_BUFFERS`，默认 [`OWN_POOL_MAX_BUFFERS`]）。
#[must_use]
pub fn parse_pool_buffers(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| (3..=32).contains(v))
        .unwrap_or(OWN_POOL_MAX_BUFFERS)
}

/// 解析 `LIUGUANG_VIDEO_FORCE_VPP`（**纯函数，可单测**）。
///
/// 真值（`1/true/on/yes`）⇒ `true`；未设/空/`0/off/false/no` ⇒ `false`。
#[must_use]
pub fn parse_force_vpp(raw: Option<&str>) -> bool {
    raw.map(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "off" | "false" | "no"
        )
    })
    .unwrap_or(false)
}

impl DmabufPixFmt {
    /// fourcc 数值。
    #[must_use]
    pub const fn fourcc(self) -> u32 {
        match self {
            Self::Nv12 => NV12_FOURCC,
            Self::Yu12 => YU12_FOURCC,
        }
    }

    /// 该格式需要的平面数（NV12=2，I420=3）。
    #[must_use]
    pub const fn planes(self) -> u32 {
        match self {
            Self::Nv12 => 2,
            Self::Yu12 => 3,
        }
    }

    /// 名称（日志/报告用）。
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12(tiled)",
            Self::Yu12 => "YU12(linear I420)",
        }
    }
}

/// 解析 `LIUGUANG_VIDEO_DMABUF_FORMAT`（**纯函数，可单测**）。
///
/// - 未设 / 空 / `nv12` ⇒ [`DmabufPixFmt::Nv12`]（**默认，保持已知可用路径**）；
/// - `yu12` / `i420` ⇒ [`DmabufPixFmt::Yu12`]；
/// - 其它值 ⇒ 回默认 NV12 **并返回 `true` 表示非法**（调用方大声告警，不静默）。
#[must_use]
pub fn parse_dmabuf_format(raw: Option<&str>) -> (DmabufPixFmt, bool) {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return (DmabufPixFmt::Nv12, false);
    };
    match text.to_ascii_lowercase().as_str() {
        "nv12" => (DmabufPixFmt::Nv12, false),
        "yu12" | "i420" => (DmabufPixFmt::Yu12, false),
        _ => (DmabufPixFmt::Nv12, true),
    }
}

/// [`DmaBufFrame`](liuguang_core::types::DmaBufFrame) 的契约守卫。
///
/// 该结构体只持有**一个** `fd` + `[u32; 4]` strides/offsets，语义是
/// "所有平面都在同一个 DRM object 内"。因此：
/// - `memory_blocks != 1` ⇒ 多 object（libva 允许一个 object 一个 fd）⇒ 拒绝；
/// - NV12 需要 Y/UV 两个平面（UV 的 stride/offset 无法从单平面推断）⇒ 拒绝；
/// - 只支持 NV12（显示层按 NV12 建平面扫描缓冲）⇒ 其它 fourcc 拒绝。
///
/// 拒绝路径都返回明确错误，由上层回退系统内存路径（绝不出错误帧）。
///
/// # Errors
///
/// 布局超出 `DmaBufFrame` 表达能力时返回 [`MediaError::Decode`]。
fn guard_dmabuf_layout(memory_blocks: u32, planes: u32, fourcc: u32) -> Result<(), MediaError> {
    if memory_blocks != 1 {
        return Err(MediaError::Decode(format!(
            "DMA-BUF 含 {memory_blocks} 个内存块（多 GEM object / 每平面一个 fd）：当前 DmaBufFrame 只能表达单 fd，拒绝该帧并回退"
        )));
    }
    let need = if fourcc == NV12_FOURCC {
        2
    } else if fourcc == YU12_FOURCC {
        // t73：线性 I420，Y/U/V 三个独立平面。
        3
    } else {
        return Err(MediaError::Decode(format!(
            "DMA-BUF fourcc 0x{fourcc:08X} 既不是 NV12 也不是 YU12（显示层只支持这两种）"
        )));
    };
    if planes < need {
        return Err(MediaError::Decode(format!(
            "DMA-BUF fourcc 0x{fourcc:08X} 只有 {planes} 个平面，需要 {need} 个：平面缺失，无法建扫描缓冲"
        )));
    }
    Ok(())
}

/// 把管线错误整理成**可执行**的错误：DMABuf 模式下命中「下游未声明 VideoMeta」
/// 这一已知签名时，附上根因与"正在回退"的说明（这就是真机
/// `vah264dec … mode=DmaBuf … qtdemux Internal data stream error` 的根因）。
fn classify_pipeline_error(detail: String, dmabuf: bool) -> MediaError {
    if !dmabuf {
        return MediaError::Decode(format!("解码管线错误: {detail}"));
    }
    match classify_dmabuf_refusal(&detail) {
        // 只有插件自己的原始文本才能断言根因（这是证据）。
        DmabufRefusalKind::VideoMeta => {
            tracing::warn!(detail = %detail, "{DMABUF_VIDEOMETA_LIMITATION}");
            MediaError::Decode(format!(
                "{DMABUF_VIDEOMETA_LIMITATION}（原始错误: {detail}）"
            ))
        }
        // t31/S6：别把"任何协商/流错误"都归因到已修好的 VideoMeta 限制。
        DmabufRefusalKind::Unclassified => {
            tracing::warn!(
                detail = %detail,
                "DMABuf 被下游拒绝（未识别到已知根因签名）：已回退系统内存 NV12；\
                 请附原始错误排查（**不要**默认归因到 VideoMeta——那条已由 t20 修好）"
            );
            MediaError::Decode(format!(
                "DMABuf 被下游拒绝（未识别根因；已回退 NV12）：{detail}"
            ))
        }
        DmabufRefusalKind::NotRefusal => MediaError::Decode(format!("解码管线错误: {detail}")),
    }
}

/// 单次调用内允许跳过的样本数上限（有界工作）。
const MAX_SKIP_PER_CALL: usize = 8;

/// 等待 appsink 侧协商出视频 caps（preroll）。
fn wait_for_video_caps(
    pipeline: &gst::Pipeline,
    appsink: &gst::Element,
    timeout: Duration,
    dmabuf: bool,
) -> Result<gst::Caps, MediaError> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| MediaError::Decode("解码管线缺少总线".into()))?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| MediaError::Decode("等待视频流就绪的截止时间非法".into()))?;
    loop {
        if let Some(pad) = appsink.static_pad("sink") {
            if let Some(caps) = pad.current_caps() {
                return Ok(caps);
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(MediaError::Decode("等待视频流就绪超时".into()));
        }
        let slice = remaining.min(Duration::from_millis(50));
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(slice.as_nanos() as u64),
            &[gst::MessageType::Error, gst::MessageType::Eos],
        ) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    let detail = format!(
                        "{}{}",
                        err.error(),
                        err.debug().map(|d| format!("（{d}）")).unwrap_or_default()
                    );
                    return Err(classify_pipeline_error(detail, dmabuf));
                }
                gst::MessageView::Eos(_) => {
                    return Err(MediaError::Decode("媒体不含可解码的视频流".into()));
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============ RGBA 捕获模式的色彩转换元素选择（2026-11-21 真机） ============

    /// 硬解元素 + 有 vapostproc ⇒ GPU 侧转换；其余一律 videoconvert。
    ///
    /// 真机背景：iHD 上 `vah264dec` 生效后帧是 NV12，wl_shm 后端只收 RGB ⇒
    /// daemon 改用 RGBA 捕获；`videoconvert` 在 CPU 上做 YUV→RGB（1080p 10–20ms/帧），
    /// `vapostproc` 交给 GPU。选错的代价是性能而不是正确性，所以判据要显式。
    #[test]
    fn rgba_converter_prefers_vapostproc_for_va_decoders() {
        assert_eq!(rgba_converter_for("vah264dec", true), "vapostproc");
        assert_eq!(rgba_converter_for("vaapih264dec", true), "vapostproc");
        assert_eq!(rgba_converter_for("vavp9dec", true), "vapostproc");
    }

    /// 没有 vapostproc（旧发行版/未装 gst-plugin-va）时必须退回 CPU 转换。
    #[test]
    fn rgba_converter_falls_back_when_vapostproc_missing() {
        assert_eq!(rgba_converter_for("vah264dec", false), "videoconvert");
    }

    /// 软解元素（avdec_h264/openh264dec）不需要 VPP：它本来就出系统内存 RGB/NV12。
    #[test]
    fn rgba_converter_uses_videoconvert_for_software_decoders() {
        assert_eq!(rgba_converter_for("avdec_h264", true), "videoconvert");
        assert_eq!(rgba_converter_for("openh264dec", true), "videoconvert");
    }

    // ============ NV12 双平面 stride 归一化（2026-10-20 审核 P2-4） ============

    /// **源 stride ≠ 目标 stride 时必须按目标 stride 重排**。
    ///
    /// 旧实现把源 stride 同时当作目标行跨度：Y=8 / UV=4 / 有效行宽=4 时，
    /// 输出声明 stride=8 却按每行 4 字节写 UV ⇒ 显示侧按 8 字节读，第二行起错位。
    /// **评审第 4 条**：NV12 的 Y / UV 平面在驱动里常被对齐成**不同 stride**，
    /// 目标缓冲又可能自行对齐（需要行尾填充）。这里用**三者互不相同**的 strides
    /// 验证 `append_plane` 不会错位，并验证填充字节为 0。
    ///
    /// 失败形态：若把 UV 行也按 Y 的 stride 取源（或目标不用 dst_stride 步进），
    /// 第二行起就会整体错位 —— 表现为画面斜切/色块。
    #[test]
    fn append_plane_handles_y_and_uv_strides_that_differ() {
        // Y: 6 字节/行 × 2 行，源 stride=8（行尾 2 字节填充）
        // UV: 6 字节/行 × 1 行，源 stride=7（与 Y 不同！）
        // 目标 stride=10（比两者都大 ⇒ 每行尾部都要补 0）
        let y_src: Vec<u8> = vec![
            1, 2, 3, 4, 5, 6, 0xAA, 0xAA, 7, 8, 9, 10, 11, 12, 0xBB, 0xBB,
        ];
        let uv_src: Vec<u8> = vec![21, 22, 23, 24, 25, 26, 0xCC];
        let mut out = Vec::with_capacity(10 * 3);
        append_plane(&mut out, &y_src, 8, 6, 2, 10).expect("Y");
        append_plane(&mut out, &uv_src, 7, 6, 1, 10).expect("UV");

        assert_eq!(out.len(), 30, "3 行 × 目标 stride 10");
        // 第 1 行：Y 前 6 字节 + 4 个填充 0
        assert_eq!(&out[0..6], &[1, 2, 3, 4, 5, 6], "Y 第 1 行数据");
        assert_eq!(&out[6..10], &[0, 0, 0, 0], "Y 第 1 行填充必须为 0");
        // 第 2 行：仍从源 stride=8 的偏移取（不是 6！）
        assert_eq!(
            &out[10..16],
            &[7, 8, 9, 10, 11, 12],
            "Y 第 2 行必须按源 stride=8 取"
        );
        assert_eq!(&out[16..20], &[0, 0, 0, 0], "Y 第 2 行填充");
        // 第 3 行：UV，按它自己的 stride=7 取（不是 Y 的 8）
        assert_eq!(
            &out[20..26],
            &[21, 22, 23, 24, 25, 26],
            "UV 必须按它自己的 stride 取"
        );
        assert_eq!(&out[26..30], &[0, 0, 0, 0], "UV 行填充");
    }

    /// 源数据不足时必须报错而不是静默产出半帧（否则画面下半部是上一次的残留）。
    #[test]
    fn append_plane_rejects_short_source() {
        let src = vec![0u8; 10];
        let mut out = Vec::new();
        assert!(
            append_plane(&mut out, &src, 8, 6, 2, 6).is_err(),
            "第二行越界必须报错"
        );
    }

    #[test]
    fn copy_plane_rewrites_rows_with_the_destination_stride() {
        // 源 UV：2 行 × 4 字节，src_stride=4（紧凑）。
        let src: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8];
        // 目标：dst_stride=8（与 Y 平面对齐），2 行 → 16 字节。
        let mut dst = vec![0u8; 16];
        copy_plane(&mut dst, 8, &src, 4, 4, 2).expect("拷贝应成功");
        assert_eq!(&dst[..4], &[1, 2, 3, 4], "第一行");
        assert_eq!(&dst[4..8], &[0, 0, 0, 0], "行末填充保持 0");
        assert_eq!(&dst[8..12], &[5, 6, 7, 8], "第二行必须落在 dst_stride 处");
        assert_eq!(&dst[12..16], &[0, 0, 0, 0], "第二行末填充");

        // 变异对照：若实现用 src_stride 计算目标行起点，第二行会写到 [4..8]。
        let mut buggy_shape = vec![0u8; 16];
        copy_plane(&mut buggy_shape, 4, &src, 4, 4, 2).expect("紧凑路径");
        assert_eq!(&buggy_shape[4..8], &[5, 6, 7, 8]);
    }

    /// 目标缓冲不足必须报错，不能 panic 或静默截断。
    #[test]
    fn copy_plane_reports_short_destination() {
        let src: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = vec![0u8; 10]; // 需要 8*2=16
        assert!(
            copy_plane(&mut dst, 8, &src, 4, 4, 2).is_err(),
            "目标平面缓冲不足必须是错误"
        );
    }

    // ============ t32 / F6：候选探测日志降噪（纯函数可测） ============

    /// 逐条模式只认**显式真值**：未设置/空/0/其它都是默认的降噪模式。
    ///
    /// 这条约束很重要——降噪不能靠"设了变量才显示"，否则用户永远看不到失败细节；
    /// 反过来也不能因为变量里塞了垃圾值就突然刷屏。
    #[test]
    fn probe_verbose_only_accepts_explicit_truthy_values() {
        assert!(!probe_verbose_value(None), "未设置 = 降噪");
        for off in ["", "0", "off", "false", "no", "  ", "garbage"] {
            assert!(
                !probe_verbose_value(Some(off)),
                "值 {off:?} 不应开启逐条模式"
            );
        }
        for on in ["1", "on", "ON", "true", "yes", " 1 "] {
            assert!(probe_verbose_value(Some(on)), "值 {on:?} 应开启逐条模式");
        }
    }

    /// 汇总文本必须**自足**：`tried=N`、`modes=[…]`、`last_error=…` 三件都在，
    /// 因为逐条细节已经降 `debug`，这行是用户在日志里唯一能看到的东西。
    #[test]
    fn probe_failure_summary_is_self_contained() {
        let modes = [CaptureMode::DmaBuf, CaptureMode::Nv12, CaptureMode::Rgba];
        let err = MediaError::Decode("协商超时".into());
        let s = probe_failure_summary(9, &modes, Some(&err));
        assert!(s.contains("tried=9"), "缺少尝试次数: {s}");
        assert!(
            s.contains("modes=[dmabuf,nv12,rgba]"),
            "缺少模式序列（顺序即优先级）: {s}"
        );
        assert!(s.contains("last_error="), "缺少最后错误: {s}");
        assert!(s.contains("协商超时"), "错误原因被丢了: {s}");
        // 没有任何候选时也要给出可读文本，而不是空串。
        let empty = probe_failure_summary(0, &[], None);
        assert!(empty.contains("modes=[]"), "{empty}");
        assert!(empty.contains("（无）"), "无错误时要有明确占位: {empty}");
    }

    /// 模式名串按**声明顺序**拼（顺序就是优先级，不能排序）。
    #[test]
    fn mode_names_preserve_priority_order() {
        assert_eq!(
            mode_names(&[CaptureMode::Rgba, CaptureMode::DmaBuf]),
            "rgba,dmabuf"
        );
        assert_eq!(mode_names(&[]), "");
    }

    // ============ t29：DMA-BUF 强制失败钩子（回退路径可测） ============

    #[test]
    fn dmabuf_policy_parses_stable_values() {
        // 未设置 = 不尝试（默认行为不变）。
        assert_eq!(
            DmabufPolicy::from_env_value(None),
            DmabufPolicy::Try,
            "未设置环境变量时默认尝试 DMA-BUF（失败会自动回退 CPU）"
        );
        for off in ["", "0", "off", "OFF", "false", "no", "  off  "] {
            assert_eq!(
                DmabufPolicy::from_env_value(Some(off)),
                DmabufPolicy::Off,
                "值 {off:?} 应当关闭 DMA-BUF"
            );
        }
        for try_ in ["1", "on", "true", "yes", "whatever"] {
            assert_eq!(
                DmabufPolicy::from_env_value(Some(try_)),
                DmabufPolicy::Try,
                "值 {try_:?} 应当尝试 DMA-BUF"
            );
        }
        for fail in ["force-fail", "FORCE-FAIL", "force_fail", "fail"] {
            assert_eq!(
                DmabufPolicy::from_env_value(Some(fail)),
                DmabufPolicy::ForceFail,
                "值 {fail:?} 应当注入协商失败"
            );
        }
        // 钩子只影响 DMA-BUF 这一种模式：Off/Try 的模式序列不含注入语义。
        assert!(!DmabufPolicy::Off.attempts_dmabuf());
        assert!(!DmabufPolicy::Off.forces_failure());
        assert!(DmabufPolicy::Try.attempts_dmabuf());
        assert!(!DmabufPolicy::Try.forces_failure(), "Try 不得注入失败");
        assert!(DmabufPolicy::ForceFail.attempts_dmabuf());
        assert!(DmabufPolicy::ForceFail.forces_failure());
        // **不变式（2026-10-20 审核 P1-1 的取舍）**：媒体策略如实描述"要不要尝试直通"，
        // **能不能用上由显示后端裁决**（`DisplayBackend::supports_dmabuf_scanout`）。
        // 判据不下沉到媒体侧，否则 `force-fail` 钩子与将来恢复的 GPU 直通路径都要靠
        // 改媒体层才能验证；上抬到 daemon 后，遗留 `LIUGUANG_VIDEO_DMABUF=1` 的旧 unit
        // 也会被强制按 CPU 捕获打开（`daemon::producer_kind` 有对应用例守这条回归）。
        assert_eq!(DmabufPolicy::Off.modes().len(), 2, "Off 不尝试直通");
        assert_eq!(
            DmabufPolicy::Off.modes().first(),
            Some(&CaptureMode::Nv12),
            "Off 首选 CPU NV12"
        );
        for policy in [DmabufPolicy::Try, DmabufPolicy::ForceFail] {
            let modes = policy.modes();
            assert!(
                modes.contains(&CaptureMode::DmaBuf),
                "{policy:?} 应保留直通候选（由后端能力决定是否真的用上）：{modes:?}"
            );
            assert_eq!(
                modes.first(),
                Some(&CaptureMode::DmaBuf),
                "尝试直通时它必须是最高优先级"
            );
            assert_eq!(modes.len(), 3, "回退序列 dmaBuf→nv12→rgba");
        }
    }

    /// 用 GStreamer 合成一个真实 MP4（拷进临时目录），供回退路径测试使用。
    ///
    /// 返回 `None` 表示本机没有可用的 H.264 编码器（跳过而不是假装通过）。
    fn synth_mp4(tag: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("liuguang-t29-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("clip.mp4");
        let encoders = ["x264enc", "vah264enc", "openh264enc"];
        for enc in encoders {
            let ok = std::process::Command::new("gst-inspect-1.0")
                .arg(enc)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                continue;
            }
            let desc = format!(
                "videotestsrc num-buffers=10 pattern=ball ! video/x-raw,width=320,height=240,framerate=30/1 ! {enc} ! mp4mux ! filesink location={}",
                path.display()
            );
            let status = std::process::Command::new("gst-launch-1.0")
                .args(["-q", &desc])
                .status();
            if let Ok(status) = status {
                if status.success() && path.exists() {
                    return Some(path);
                }
            }
        }
        None
    }

    /// **t29 核心**：注入 DMA-BUF 协商失败后，必须**回退系统内存 NV12 并成功打开**，
    /// 而不是报错或静默落在 dmabuf。
    #[test]
    fn force_fail_falls_back_to_system_memory() {
        let Some(video) = synth_mp4("fallback") else {
            eprintln!("跳过：本机没有可用的 H.264 编码器（x264enc/vah264enc/openh264enc）");
            return;
        };
        let policy = DecoderPolicy::default();
        let modes = DmabufPolicy::ForceFail.modes();
        let opened = GstProducer::open_with_modes(&video, policy, modes, true, None);
        match opened {
            Ok(producer) => {
                assert_ne!(
                    producer.capture_mode_name(),
                    "dmabuf",
                    "注入失败后不得报告 dmabuf（否则钩子没生效）"
                );
                assert!(
                    matches!(producer.capture_mode_name(), "nv12" | "rgba"),
                    "必须回退到系统内存路径（实际 {}）",
                    producer.capture_mode_name()
                );
                assert!(
                    !producer.decoder_element().is_empty(),
                    "生效元素必须如实上报（status.decoder 依赖它）"
                );
            }
            Err(e) => panic!("注入 DMA-BUF 失败后仍应回退成功，实际报错: {e}"),
        }
        let _ = std::fs::remove_file(&video);
    }

    #[test]
    fn nv12_only_modes_never_touch_dmabuf_hook() {
        // 不尝试 DMA-BUF 时，钩子参数即使为 true 也不得影响其他模式。
        let Some(video) = synth_mp4("nv12only") else {
            eprintln!("跳过：本机没有可用的 H.264 编码器");
            return;
        };
        let opened = GstProducer::open_with_modes(
            &video,
            DecoderPolicy::default(),
            DmabufPolicy::Off.modes(),
            true,
            None,
        );
        match opened {
            Ok(p) => assert!(
                matches!(p.capture_mode_name(), "nv12" | "rgba"),
                "只有 NV12/RGBA 候选时不得因钩子失败（实际 {}）",
                p.capture_mode_name()
            ),
            Err(e) => panic!("NV12 路径不应受 DMA-BUF 钩子影响: {e}"),
        }
        let _ = std::fs::remove_file(&video);
    }

    /// 硬解候选（测试用固定表，不依赖本机插件）。
    fn hw(name: &'static str) -> Vec<DecoderChoice> {
        vec![DecoderChoice {
            element: name,
            kind: DecoderKind::Vaapi,
        }]
    }

    /// 软解候选（测试用固定表）。
    fn sw(name: &'static str) -> Vec<DecoderChoice> {
        vec![DecoderChoice {
            element: name,
            kind: DecoderKind::Software,
        }]
    }

    #[test]
    fn open_missing_file_reports_io() {
        let err = GstProducer::open(
            Path::new("/nonexistent/liuguang-missing.mp4"),
            DecoderPolicy::default(),
        )
        .err()
        .unwrap_or_else(|| panic!("期望 Err"));
        assert!(matches!(err, MediaError::Io { .. }));
    }

    // ---- 解码器守卫（离线，不依赖本机插件） ----

    #[test]
    fn policy_prefers_hardware_then_software_when_fallback_allowed() {
        let got = choose_decoders(
            DecoderPolicy::default(),
            hw("vah264dec"),
            sw("openh264dec"),
            "stub",
        )
        .unwrap();
        let names: Vec<&str> = got.iter().map(|c| c.element).collect();
        assert_eq!(names, ["vah264dec", "openh264dec"]);
        assert!(got[0].kind.is_hardware());
    }

    #[test]
    fn policy_refuses_software_when_fallback_disabled() {
        let policy = DecoderPolicy {
            allow_software_fallback: false,
            ..DecoderPolicy::default()
        };
        let err = choose_decoders(policy, Vec::new(), sw("openh264dec"), "stub")
            .err()
            .unwrap_or_else(|| panic!("不允许软解回退时必须报错"));
        let MediaError::Decode(msg) = err else {
            panic!("期望 Decode 错误");
        };
        assert!(
            msg.contains("拒绝回退软解"),
            "错误信息需说明拒绝原因: {msg}"
        );
        // 硬解可用时允许软解回退也不应把软解排到硬解前面。
        let mixed = choose_decoders(policy, hw("vah264dec"), sw("openh264dec"), "stub").unwrap();
        assert_eq!(mixed.len(), 1, "禁止软解回退时不应把软解作为候选");
    }

    #[test]
    fn policy_requires_hardware_when_configured_required() {
        let policy = DecoderPolicy {
            require_hardware: true,
            ..DecoderPolicy::default()
        };
        assert!(
            choose_decoders(policy, Vec::new(), sw("openh264dec"), "stub").is_err(),
            "media.hardware_decode=required 时不得静默软解"
        );
    }

    #[test]
    fn policy_software_only_uses_software() {
        let got = choose_decoders(
            DecoderPolicy::software_only(),
            hw("vah264dec"),
            sw("openh264dec"),
            "stub",
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].element, "openh264dec");
        assert!(!got[0].kind.is_hardware());
        // 显式禁用硬解但本机没有任何软解元素：报错而不是悄悄用硬解。
        assert!(
            choose_decoders(
                DecoderPolicy::software_only(),
                hw("vah264dec"),
                Vec::new(),
                "stub"
            )
            .is_err()
        );
    }

    #[test]
    fn policy_reports_no_decoder_at_all() {
        let err = choose_decoders(DecoderPolicy::default(), Vec::new(), Vec::new(), "stub")
            .err()
            .unwrap_or_else(|| panic!("无解码器时必须报错"));
        assert!(matches!(err, MediaError::Decode(_)));
    }

    /// 真实 GStreamer 路径的端到端测试：需要本机插件与测试样本，默认忽略。
    ///
    /// 运行方式：
    /// - 设 `LIUGUANG_TEST_MP4=/path/clip.mp4` 真跑（未设置则**明确 SKIP**，打印原因并返回）；
    /// - 真机（有 VA-API H.264）再加 `LIUGUANG_TEST_REQUIRE_VA=1`：**强制断言硬解**
    ///   （元素必须是 `vah264dec`、`DecoderKind::Vaapi`）；本机没有 VA 能力时明确跳过。
    ///
    /// 不再用 `#[ignore]` 掩盖：它会让"解码链路真的能跑"这件事在 CI 里永远不可见。
    /// 真机同口径脚本：`.audit/verify/25-decoder-reality.sh`。
    #[test]
    fn runtime_open_mp4_and_pull_frames() {
        let Ok(raw) = std::env::var("LIUGUANG_TEST_MP4") else {
            eprintln!(
                "SKIP：未设置 LIUGUANG_TEST_MP4（需要真实 H.264 mp4）——这是显式跳过，不是通过"
            );
            return;
        };
        // 相对路径：`cargo test` 的工作目录是 crate 目录，这里同时按
        // "原样 / 工作区根（crate 的上级上级）" 两种解释找；都找不到就**明确跳过**。
        let candidates: Vec<std::path::PathBuf> = {
            let p = std::path::PathBuf::from(&raw);
            if p.is_absolute() {
                vec![p]
            } else {
                vec![
                    p.clone(),
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../..")
                        .join(&p),
                ]
            }
        };
        let Some(path) = candidates.into_iter().find(|p| p.exists()) else {
            eprintln!(
                "SKIP：LIUGUANG_TEST_MP4={raw} 不存在（相对路径按 crate 目录与工作区根都找过）——显式跳过，不是通过"
            );
            return;
        };
        let mut producer = GstProducer::open(&path, DecoderPolicy::default()).unwrap();
        eprintln!(
            "解码器元素={} 捕获模式={} 源帧率={:?}",
            producer.decoder_element(),
            producer.capture_mode_name(),
            producer.info().source_fps
        );
        assert_eq!(producer.info().kind, MediaKind::Video);
        assert!(
            matches!(
                producer.decoder_kind(),
                DecoderKind::Software | DecoderKind::Vaapi
            ),
            "必须是软解或 VA-API 硬解元素"
        );
        // 真机硬解口径：要求 VA 时必须是 Vaapi，且元素名是 va 插件的 vah264dec。
        if std::env::var("LIUGUANG_TEST_REQUIRE_VA").is_ok() {
            let va = crate::va::probe();
            assert!(
                va.h264_vld,
                "要求硬解但本机 VA 探测未发现 H.264 VLD 能力：{}",
                va.summary()
            );
            assert!(
                matches!(producer.decoder_kind(), DecoderKind::Vaapi),
                "要求硬解但实际生效的是 {:?}（元素 {}）",
                producer.decoder_kind(),
                producer.decoder_element()
            );
            assert_eq!(producer.decoder_element(), "vah264dec");
        }
        // **带超时的阻塞轮询**：NeedMore 时在总线上阻塞等待（最多 50ms，
        // 有 EOS/ERROR/状态消息会立刻返回），并设整段素材的硬上限 ——
        // 不再是 `for _ in 0..50_000 { continue }` 式忙等。
        let timeout_s: u64 = std::env::var("LIUGUANG_TEST_MP4_TIMEOUT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        let deadline = Instant::now() + Duration::from_secs(timeout_s);
        let mut seen = 0u32;
        let mut last_pts = Duration::ZERO;
        let mut ended = false;
        let bus = producer.pipeline.bus();
        while Instant::now() < deadline {
            match producer.next_producer_frame().unwrap() {
                VideoPoll::Frame { pts, frame } => {
                    assert!(pts >= last_pts, "PTS 必须单调不减：{last_pts:?} → {pts:?}");
                    last_pts = pts;
                    assert!(frame.width() > 0 && frame.height() > 0);
                    seen += 1;
                }
                VideoPoll::NeedMore => {
                    // 阻塞等待"有事件"（5ms~50ms 上界），而不是空转。
                    if let Some(bus) = bus.as_ref() {
                        let _ = bus.timed_pop_filtered(
                            gst::ClockTime::from_mseconds(50),
                            &[
                                gst::MessageType::Eos,
                                gst::MessageType::Error,
                                gst::MessageType::StateChanged,
                            ],
                        );
                    } else {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                VideoPoll::Ended => {
                    ended = true;
                    break;
                }
            }
        }
        assert!(
            ended || seen > 0,
            "超时前既没有解出帧也没有观测到流末（限时 {:?}）",
            deadline.saturating_duration_since(Instant::now())
        );
        assert!(seen > 1, "应至少解出多帧（实际 {seen}）");
        assert!(
            ended,
            "必须能观测到流末（appsink 的 eos 属性/总线 EOS）：否则循环重播永远不会发生"
        );
        eprintln!("解出 {seen} 帧，最后 PTS={last_pts:?}");
        // 诊断：管线是否已经到达 EOS（EOS 消息、状态、位置）。
        eprintln!(
            "eos_seen={} finished={} 状态={:?} 位置={:?}",
            producer.eos_seen,
            producer.finished,
            producer.pipeline.current_state(),
            producer.pipeline.query_position::<gst::ClockTime>(),
        );
        if let Some(bus) = producer.pipeline.bus() {
            let mut n = 0;
            while let Some(msg) = bus.timed_pop(gst::ClockTime::ZERO) {
                eprintln!("总线消息: {:?}", msg.type_());
                n += 1;
                if n > 10 {
                    break;
                }
            }
            if n == 0 {
                eprintln!("总线消息: （空）");
            }
        }
        producer.seek_start().unwrap();
        let (pulled, none) = producer.pull_counters();
        eprintln!("拉取成功 {pulled} 次、空手 {none} 次");
    }
    // ============ t9：DMA-BUF 协商与契约守卫（真机口径） ============

    /// drm-format 解析：真机 `vah264dec`/`vapostproc` 报的就是
    /// `NV12:0x0100000000000002`（i915 Y_TILED）。
    #[test]
    fn drm_format_parsing_matches_real_machine_values() {
        let (fourcc, modifier) = parse_drm_format("NV12:0x0100000000000002").expect("应能解析");
        assert_eq!(fourcc, NV12_FOURCC, "fourcc 应为 'NV12'（0x3231564E）");
        assert_eq!(
            modifier, 0x0100_0000_0000_0002,
            "modifier 应为 i915 Y_TILED"
        );
        // 无 0x 前缀（**保留前导 0**，真机字符串就是 16 位十六进制）/ LINEAR(0) 都要能吃。
        assert_eq!(
            parse_drm_format("NV12:0100000000000002").unwrap().1,
            modifier
        );
        assert_eq!(parse_drm_format("NV12:0").unwrap().1, 0);
        // 非法输入必须报错，不能瞎猜。
        assert!(parse_drm_format("NV12").is_err(), "缺 modifier 必须报错");
        assert!(
            parse_drm_format("NV1:0x2").is_err(),
            "fourcc 非 4 字符必须报错"
        );
        assert!(
            parse_drm_format("NV12:zzz").is_err(),
            "modifier 非十六进制必须报错"
        );
    }

    /// `DmaBufFrame` 契约守卫：多 GEM object / 非 NV12 / 平面不足都必须拒绝
    /// （拒绝 = 明确错误 + 上层回退，绝不交出猜测的 fd/布局）。
    #[test]
    fn dmabuf_layout_guard_rejects_unsupported_layouts() {
        // 正常：单内存块 + 2 平面 + NV12。
        assert!(guard_dmabuf_layout(1, 2, NV12_FOURCC).is_ok());
        // 多 object（每平面一个 fd）：当前结构体表达不了。
        let e = guard_dmabuf_layout(2, 2, NV12_FOURCC)
            .unwrap_err()
            .to_string();
        assert!(e.contains("内存块"), "错误应说明内存块数: {e}");
        // 非 NV12 fourcc：显示层不支持。
        let e = guard_dmabuf_layout(1, 2, 0x3432_5241)
            .unwrap_err()
            .to_string();
        assert!(e.contains("NV12"), "错误应说明只支持 NV12: {e}");
        // 单平面 NV12：UV 缺失。
        let e = guard_dmabuf_layout(1, 1, NV12_FOURCC)
            .unwrap_err()
            .to_string();
        assert!(e.contains("平面"), "错误应说明平面数: {e}");
    }

    /// 「DMABuf 被下游拒绝」的识别必须覆盖真机原始文本
    /// （`gstvabasedec.c` 的报错 + qtdemux 的继发错误）。
    #[test]
    fn dmabuf_refusal_detection_matches_real_machine_text() {
        // 逐字取自真机 `GST_DEBUG=va*:5` / gst-launch 输出。
        for text in [
            "DMABuf caps negotiated without the mandatory support of VideoMeta",
            "Failed to negotiate with downstream",
            "streaming stopped, reason not-negotiated (-4)",
            "错误：来自组件 /GstPipeline:pipeline0/GstQTDemux:qtdemux0：Internal data stream error.",
            "No valid frames decoded before end of stream",
        ] {
            assert!(
                looks_like_dmabuf_refusal(text),
                "必须识别为 DMABuf 拒绝: {text}"
            );
        }
        // 无关错误不得被误判（否则会把普通解码错误说成 DMABuf 问题）。
        for text in [
            "等待视频流就绪超时",
            "文件不存在",
            "DMA-BUF fourcc 0x34325241 不是 NV12",
        ] {
            assert!(!looks_like_dmabuf_refusal(text), "不得误判: {text}");
        }
    }

    /// DMABuf 模式的 caps 必须**显式**写出 `format=DMA_DRM`。
    ///
    /// 只写 `(memory:DMABuf)`（features-only）时，协商可能落到没有
    /// `drm-format` 的组合上 ⇒ 取不到 fourcc/modifier（旧实现的缺陷）。
    // ---- t53：modifier 开关（纯函数，不依赖 gst 运行时） ----

    #[test]
    fn modifier_env_is_translated_to_caps_constraint() {
        use super::{DmabufModifierChoice, dmabuf_caps_string, parse_dmabuf_modifier};
        // 默认：两者都不设 ⇒ 不约束（历史行为，由 iHD 决定）
        assert_eq!(
            parse_dmabuf_modifier(None, None),
            Ok(DmabufModifierChoice::Unconstrained)
        );
        assert_eq!(
            dmabuf_caps_string(DmabufModifierChoice::Unconstrained),
            "video/x-raw(memory:DMABuf),format=DMA_DRM"
        );

        // Yf_TILED（NV12 在内核里的原生项）
        let yf = parse_dmabuf_modifier(Some("0x0100000000000004"), None).expect("合法");
        assert_eq!(yf, DmabufModifierChoice::Pinned(0x0100_0000_0000_0004));
        assert_eq!(
            dmabuf_caps_string(yf),
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=(string)NV12:0x100000000000004",
            "必须翻译成 drm-format=(string)NV12:0x<modifier>"
        );

        // Y_TILED / LINEAR
        assert_eq!(
            parse_dmabuf_modifier(Some("0x0100000000000002"), None),
            Ok(DmabufModifierChoice::Pinned(0x0100_0000_0000_0002))
        );
        assert_eq!(
            dmabuf_caps_string(DmabufModifierChoice::Pinned(0)),
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=(string)NV12:0x0"
        );

        // 前缀可省、大小写不敏感
        assert_eq!(
            parse_dmabuf_modifier(Some("0100000000000004"), None),
            Ok(DmabufModifierChoice::Pinned(0x0100_0000_0000_0004))
        );
        assert_eq!(
            parse_dmabuf_modifier(Some("0X0100000000000004"), None),
            Ok(DmabufModifierChoice::Pinned(0x0100_0000_0000_0004))
        );
    }

    #[test]
    fn old_linear_switch_is_still_honoured() {
        use super::{DmabufModifierChoice, parse_dmabuf_modifier};
        // 旧开关真值 ⇒ 等价于 MODIFIER=0x0（向后兼容，不必改已有脚本）
        for on in ["1", "true", "on", "yes", "TRUE"] {
            assert_eq!(
                parse_dmabuf_modifier(None, Some(on)),
                Ok(DmabufModifierChoice::Pinned(0)),
                "旧开关 {on:?} 应等价于 LINEAR"
            );
        }
        for off in ["", "0", "off", "false", "no"] {
            assert_eq!(
                parse_dmabuf_modifier(None, Some(off)),
                Ok(DmabufModifierChoice::Unconstrained),
                "旧开关 {off:?} 应为不约束"
            );
        }
        // **新变量优先**：两者同时给出时以 MODIFIER 为准
        assert_eq!(
            parse_dmabuf_modifier(Some("0x0100000000000004"), Some("1")),
            Ok(DmabufModifierChoice::Pinned(0x0100_0000_0000_0004)),
            "新变量必须优先于旧开关"
        );
    }

    #[test]
    fn invalid_modifier_fails_loudly_instead_of_silently_unconstrained() {
        use super::parse_dmabuf_modifier;
        // 非法值**不得**静默降级为"不约束"——否则会得到"以为在测 X、其实没约束"的假实验
        for bad in ["0xZZ", "yftiled", "0x0100000000000004z", "0x"] {
            assert!(
                parse_dmabuf_modifier(Some(bad), None).is_err(),
                "{bad:?} 必须报错（不得静默变成不约束）"
            );
        }
        // 空串视为未设置（便于 `MODIFIER= ` 这种写法不炸）
        assert!(parse_dmabuf_modifier(Some("   "), None).is_ok());
    }

    /// **t97 步骤③（已实测，结论为失败 —— 故此测试 `#[ignore]`）**：
    /// 裸 `gst_va_pool_new()` **能建出池（非 null）**，但用普通 `video/x-raw,format=NV12` caps 时
    /// **`set_config` 失败**（`Failed to set config`），`set_active` 随之失败。
    ///
    /// **⇒ 与修法 A 的普通池是同一个症状** ⇒ 触发合同里的硬止损线 ⇒ **转 B（务实收口）**。
    /// 保留此测试作为**可复现证据**：取消 `#[ignore]` 即可重新验证。
    /// 若将来要再试：`GstVaPool` 大概率需要 **DMABuf caps + 活的 VA display**（这是本卡三步之外的
    /// 新前置条件，按合同不再追）。
    #[test]
    #[ignore = "t97 实测失败：裸 VA 池 set_config 同样报 Failed to set config（见文档）"]
    #[allow(unsafe_code)]
    fn va_pool_can_be_created_and_configured() {
        use gst::prelude::*;
        if gst::init().is_err() {
            eprintln!("跳过：gst::init 失败");
            return;
        }
        // SAFETY: 只在 gst::init 之后调用；返回 transfer full。
        let raw = unsafe { super::va_ffi::gst_va_pool_new() };
        assert!(!raw.is_null(), "gst_va_pool_new 返回了 null");
        // SAFETY: `raw` 是 transfer full 的 GstBufferPool*，交给 gst::BufferPool 接管。
        let pool = unsafe {
            gstreamer::glib::translate::from_glib_full::<_, gst::BufferPool>(
                raw as *mut gst::ffi::GstBufferPool,
            )
        };

        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .build();
        let mut cfg = pool.config();
        cfg.set_params(Some(&caps), 0, 1, 4);
        let set_cfg = pool.set_config(cfg);
        let activated = pool.set_active(true);
        println!("VA 池：set_config={set_cfg:?} set_active={activated:?}");
        assert!(set_cfg.is_ok(), "裸 VA 池 set_config 失败：{set_cfg:?}");
        assert!(activated.is_ok(), "裸 VA 池 set_active 失败：{activated:?}");
        let acquired = pool.acquire_buffer(None);
        println!(
            "VA 池：acquire_buffer={:?}",
            acquired.as_ref().map(|b| b.size())
        );
        assert!(
            acquired.is_ok(),
            "裸 VA 池 acquire_buffer 失败：{acquired:?}"
        );
        let _ = pool.set_active(false);
    }

    #[test]
    fn force_vpp_parsing_is_explicit() {
        use super::parse_force_vpp;
        // 默认关闭（保持已知可用路径）
        assert!(!parse_force_vpp(None));
        assert!(!parse_force_vpp(Some("")));
        assert!(!parse_force_vpp(Some("0")));
        assert!(!parse_force_vpp(Some("off")));
        assert!(!parse_force_vpp(Some("FALSE")));
        // 真值
        assert!(parse_force_vpp(Some("1")));
        assert!(parse_force_vpp(Some("true")));
        assert!(parse_force_vpp(Some("YES")));
        assert!(parse_force_vpp(Some("on")));
    }

    #[test]
    fn yu12_format_requests_linear_i420_in_caps() {
        use super::{DmabufModifierChoice, DmabufPixFmt, dmabuf_caps_for, parse_dmabuf_format};
        // 解析：默认 NV12（保持已知可用路径）
        assert_eq!(parse_dmabuf_format(None), (DmabufPixFmt::Nv12, false));
        assert_eq!(parse_dmabuf_format(Some("")), (DmabufPixFmt::Nv12, false));
        assert_eq!(
            parse_dmabuf_format(Some("nv12")),
            (DmabufPixFmt::Nv12, false)
        );
        // yu12 / i420 同义
        assert_eq!(
            parse_dmabuf_format(Some("yu12")),
            (DmabufPixFmt::Yu12, false)
        );
        assert_eq!(
            parse_dmabuf_format(Some("I420")),
            (DmabufPixFmt::Yu12, false)
        );
        // 非法值 ⇒ 回默认**并标记非法**（调用方要大声告警）
        assert_eq!(
            parse_dmabuf_format(Some("nv21")),
            (DmabufPixFmt::Nv12, true)
        );

        // **核心断言**：YU12 模式的 caps 必须真的请求 `drm-format=(string)YU12`
        let caps = dmabuf_caps_for(DmabufPixFmt::Yu12, DmabufModifierChoice::Unconstrained);
        assert!(
            caps.contains("drm-format=(string)YU12"),
            "YU12 模式必须请求线性 I420：{caps}"
        );
        assert!(!caps.contains("NV12"), "YU12 模式不得同时请求 NV12：{caps}");
        // 平面数
        assert_eq!(DmabufPixFmt::Yu12.planes(), 3, "I420 是 3 平面");
        assert_eq!(DmabufPixFmt::Nv12.planes(), 2, "NV12 是 2 平面");
        // NV12 模式仍走原有逻辑（不约束）
        assert_eq!(
            dmabuf_caps_for(DmabufPixFmt::Nv12, DmabufModifierChoice::Unconstrained),
            "video/x-raw(memory:DMABuf),format=DMA_DRM"
        );
    }

    #[test]
    fn guard_accepts_yu12_only_with_three_planes() {
        use super::{NV12_FOURCC, YU12_FOURCC, guard_dmabuf_layout};
        // NV12：2 平面够
        assert!(guard_dmabuf_layout(1, 2, NV12_FOURCC).is_ok());
        assert!(guard_dmabuf_layout(1, 1, NV12_FOURCC).is_err());
        // YU12：**必须 3 平面**
        assert!(guard_dmabuf_layout(1, 3, YU12_FOURCC).is_ok());
        assert!(
            guard_dmabuf_layout(1, 2, YU12_FOURCC).is_err(),
            "I420 只给 2 平面必须被拒绝（V 平面缺失会画错）"
        );
        // 未知 fourcc 仍拒绝
        assert!(guard_dmabuf_layout(1, 3, 0x1234_5678).is_err());
    }

    #[test]
    fn keepalive_cap_parses_and_clamps() {
        use super::{DMABUF_KEEPALIVE_FRAMES, DMABUF_KEEPALIVE_MAX, parse_keepalive_cap};
        // 未设/空/非法 ⇒ 默认
        assert_eq!(parse_keepalive_cap(None), (DMABUF_KEEPALIVE_FRAMES, false));
        assert_eq!(
            parse_keepalive_cap(Some("  ")),
            (DMABUF_KEEPALIVE_FRAMES, false)
        );
        assert_eq!(
            parse_keepalive_cap(Some("abc")),
            (DMABUF_KEEPALIVE_FRAMES, false)
        );
        // 显式 0 ⇒ 停用（对照实验要用）
        assert_eq!(parse_keepalive_cap(Some("0")), (0, false));
        // 正常值
        assert_eq!(parse_keepalive_cap(Some("12")), (12, false));
        // **超上限 ⇒ 夹紧并标记**（丢帧优于卡死：不能让池被耗干）
        assert_eq!(
            parse_keepalive_cap(Some("9999")),
            (DMABUF_KEEPALIVE_MAX, true)
        );
    }

    #[test]
    fn dmabuf_caps_are_explicit_about_dma_drm() {
        gst::init().expect("GStreamer 初始化");
        let caps: gst::Caps = "video/x-raw(memory:DMABuf),format=DMA_DRM"
            .parse()
            .expect("caps 应可解析");
        let s = caps.structure(0).expect("应有结构");
        assert_eq!(s.name(), "video/x-raw");
        assert_eq!(
            s.get::<String>("format").expect("必须有 format 字段"),
            "DMA_DRM"
        );
        assert!(
            caps.features(0)
                .map(|f| f.to_string().contains("memory:DMABuf"))
                .unwrap_or(false),
            "caps 必须带 memory:DMABuf 特性"
        );
    }
    /// t9 关键路径：`advertise_video_meta` 必须**真的**把 `GstVideoMeta` 加进
    /// allocation 查询 —— 这是 va 插件出 DMA-BUF 的硬前提
    /// （缺了就是 `not-negotiated` + `qtdemux Internal data stream error`）。
    #[test]
    fn advertise_video_meta_adds_meta_to_allocation_query() {
        gst::init().expect("GStreamer 初始化");
        let q = gst::query::Allocation::new(None, false);
        assert!(
            q.find_allocation_meta::<gst_video::VideoMeta>().is_none(),
            "初始 allocation 查询不该含 VideoMeta（这正是 appsink 的默认行为）"
        );
        assert!(advertise_video_meta(&q), "必须真的加进去");
        assert!(
            q.find_allocation_meta::<gst_video::VideoMeta>().is_some(),
            "必须声明 VideoMeta，否则 va 插件拒绝 DMA-BUF"
        );
        // 非 allocation 查询：不得被改动，也不得崩（只是走一遍类型判断分支）。
        let caps_query = gst::query::Caps::new(None);
        assert!(
            !advertise_video_meta(&caps_query),
            "非 allocation 查询不得被处理"
        );
    }

    /// t20 关键：**allocation 查询探针必须真的把 `GstVideoMeta` 加进查询**。
    ///
    /// 用 `identity ! appsink` + 从 identity 的 src pad 发一个真实的 ALLOCATION
    /// 查询（正是解码器 `decide_allocation` 的行为）；不需要 VA/DRM，可在 CI 跑。
    #[test]
    fn query_probe_declares_video_meta_for_allocation_query() {
        gst::init().expect("GStreamer 初始化");
        let pipeline = gst::Pipeline::new();
        let upstream = gst::ElementFactory::make("identity")
            .build()
            .expect("identity");
        let sink = gst::ElementFactory::make("appsink")
            .build()
            .expect("appsink");
        pipeline.add_many([&upstream, &sink]).expect("加入元素");
        gst::Element::link_many([&upstream, &sink]).expect("连接");
        assert!(
            sink.static_pad("sink")
                .as_ref()
                .is_some_and(attach_video_meta_query_probe),
            "探针必须挂得上（否则 DMABuf 协商必然失败）"
        );
        let mut query = gst::query::Allocation::new(None, false);
        assert!(
            query
                .find_allocation_meta::<gst_video::VideoMeta>()
                .is_none(),
            "构造出的查询初始不含 VideoMeta（对照）"
        );
        let _sent = upstream
            .static_pad("src")
            .expect("src pad")
            .peer_query(&mut query);
        assert!(
            query
                .find_allocation_meta::<gst_video::VideoMeta>()
                .is_some(),
            "上游发出 allocation 查询后，查询里必须带 VideoMeta"
        );
    }
    /// t31/S1：keepalive 环必须**真的持有**引用，并按容量淘汰最旧者。
    ///
    /// 两条独立断言：① 超容量后长度 == cap 且最旧者被释放（用 PTS 做身份标记）；
    /// ② 丢掉本地变量后，环里的 sample **仍能取到可用 buffer** —— 这就是"持有"，
    /// 也是"只 dup fd 不够"的反面（dup 只保证分配不释放，引用归零池就能重写 surface）。
    #[test]
    fn keepalive_ring_holds_reference_and_evicts_oldest() {
        gst::init().expect("GStreamer 初始化");
        let mk = |pts_ms: u64| {
            let mut buf = gst::Buffer::with_size(16).expect("buffer");
            if let Some(b) = buf.get_mut() {
                b.set_pts(gst::ClockTime::from_mseconds(pts_ms));
            }
            gst::Sample::builder().buffer(&buf).build()
        };
        let cap = DMABUF_KEEPALIVE_FRAMES;
        let mut ring = std::collections::VecDeque::new();
        for i in 0..(cap as u64 + 4) {
            let sample = mk(i);
            keepalive_push(&mut ring, &sample, cap);
        }
        assert_eq!(ring.len(), cap, "环长度必须被容量约束");
        // 淘汰：最旧的是 (cap+4)-cap = 4ms 那帧。
        let front_pts = ring
            .front()
            .and_then(|s| s.buffer())
            .and_then(|b| b.pts())
            .expect("front 必须有 buffer");
        assert_eq!(
            front_pts,
            gst::ClockTime::from_mseconds(4),
            "必须淘汰最旧者"
        );
        // 引用被持有的证据：本地变量已全部 drop，环里的 buffer 依旧可用。
        let newest = ring
            .back()
            .and_then(|s| s.buffer())
            .and_then(|b| b.pts())
            .expect("back 必须有 buffer");
        assert_eq!(
            newest,
            gst::ClockTime::from_mseconds(cap as u64 + 3),
            "最新帧必须在环里且可用"
        );
        // 边界：cap=1 只留最新一帧。
        let mut one = std::collections::VecDeque::new();
        keepalive_push(&mut one, &mk(1), 1);
        keepalive_push(&mut one, &mk(2), 1);
        assert_eq!(one.len(), 1);
        assert_eq!(
            one.front().and_then(|s| s.buffer()).and_then(|b| b.pts()),
            Some(gst::ClockTime::from_mseconds(2))
        );
    }

    /// t31/S1 接线自检：**系统内存**模式下 keepalive 环必须为空（不白占内存、不误报直通）。
    #[test]
    fn keepalive_stays_empty_without_dmabuf() {
        let Ok(raw) = std::env::var("LIUGUANG_TEST_MP4") else {
            eprintln!("SKIP：未设置 LIUGUANG_TEST_MP4——显式跳过，不是通过");
            return;
        };
        let p = std::path::PathBuf::from(&raw);
        let candidates: Vec<std::path::PathBuf> = if p.is_absolute() {
            vec![p]
        } else {
            vec![
                p.clone(),
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(&p),
            ]
        };
        let Some(path) = candidates.into_iter().find(|c| c.exists()) else {
            eprintln!("SKIP：LIUGUANG_TEST_MP4={raw} 不存在——显式跳过，不是通过");
            return;
        };
        let producer = GstProducer::open(&path, DecoderPolicy::default()).expect("打开 mp4");
        assert_eq!(
            producer.dmabuf_keepalive_len(),
            0,
            "非 DMA-BUF 捕获模式不得持有 keepalive 引用"
        );
    }
    /// t31/S6：归因分级——只有插件**自己的**原始文本才能断言根因；
    /// 泛化的 not-negotiated/流错误必须报"未识别根因"，否则会把已修好的
    /// VideoMeta 限制当成万能解释（误导数轮排查）。
    #[test]
    fn dmabuf_refusal_classification_distinguishes_known_root_cause() {
        assert_eq!(
            classify_dmabuf_refusal(
                "streaming stopped, reason not-negotiated (-4): DMABuf caps negotiated \
                 without the mandatory support of VideoMeta"
            ),
            DmabufRefusalKind::VideoMeta,
            "插件原文 ⇒ 可以断言根因"
        );
        for text in [
            "streaming stopped, reason not-negotiated (-4)",
            "Internal data stream error.",
            "No valid frames decoded before end of stream",
            "Failed to negotiate with downstream",
        ] {
            assert_eq!(
                classify_dmabuf_refusal(text),
                DmabufRefusalKind::Unclassified,
                "泛化拒绝必须报未识别根因: {text}"
            );
            // 向后兼容：仍算"DMABuf 被拒"（既有断言不受影响，不得放宽）。
            assert!(looks_like_dmabuf_refusal(text));
        }
        assert_eq!(
            classify_dmabuf_refusal("random io error"),
            DmabufRefusalKind::NotRefusal
        );
        assert!(!looks_like_dmabuf_refusal("random io error"));
    }
    /// t31/S7 + S2（媒体侧证据）：**注入 DMA-BUF 协商失败**后必须
    /// ① 真实回退到系统内存 NV12（不是"只拒帧"、更不是降级为静态）；
    /// ② `is_dmabuf()==false` 且 `dmabuf_fallbacks()>=1`（结构化可见，不靠字符串猜）。
    #[test]
    fn force_fail_dmabuf_falls_back_to_nv12_with_counted_fallback() {
        let Ok(raw) = std::env::var("LIUGUANG_TEST_MP4") else {
            eprintln!("SKIP：未设置 LIUGUANG_TEST_MP4——显式跳过，不是通过");
            return;
        };
        let p = std::path::PathBuf::from(&raw);
        let candidates: Vec<std::path::PathBuf> = if p.is_absolute() {
            vec![p]
        } else {
            vec![
                p.clone(),
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(&p),
            ]
        };
        let Some(path) = candidates.into_iter().find(|c| c.exists()) else {
            eprintln!("SKIP：LIUGUANG_TEST_MP4={raw} 不存在——显式跳过，不是通过");
            return;
        };
        // 强制 DMA-BUF 协商失败（同一个生产回退循环，不是假路径）。
        let opened = GstProducer::open_with_modes(
            &path,
            DecoderPolicy::default(),
            DmabufPolicy::Try.modes(),
            true, // force_fail：走与真实协商失败**同一条**回退循环
            None,
        );
        let producer = opened.expect("注入协商失败后仍必须能回退到 NV12 并成功建管");
        assert!(
            !producer.is_dmabuf(),
            "注入失败后不得仍声称直通（capture_mode={}）",
            producer.capture_mode_name()
        );
        assert!(
            producer.capture_mode_name() == "nv12" || producer.capture_mode_name() == "rgba",
            "必须真回退到系统内存模式，实际 {}",
            producer.capture_mode_name()
        );
        assert!(
            producer.dmabuf_fallbacks() >= 1,
            "回退必须被计数（结构化可见），实际 {}",
            producer.dmabuf_fallbacks()
        );
        assert!(
            producer.dmabuf_keepalive_len() == 0,
            "回退后不得持有 DMA-BUF keepalive"
        );
    }
}
