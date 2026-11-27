//! 视频帧供给核心：有界帧队列、PTS/单调时钟调度、丢帧策略与可观测状态。
//!
//! 设计要点（规范 §3.3 / §6.4）：
//! - **有界帧队列**：默认 3 帧，可配置（1..=16）；队列满时不再向生产者拉帧，
//!   避免堆积；过期帧按 [`VideoOptions::drop_late_tolerance`] 丢弃而非积压。
//! - **PTS 调度**：帧 PTS 映射到单调时间轴（首次 `poll_frame` 的 `now` 为 epoch，
//!   帧 due = epoch + pts）；**不**按显示器刷新率强制解码。
//!   [`VideoPlayer::next_deadline`] 把"下一帧的绝对到期时刻"暴露给调用方，
//!   使上层可以用**绝对时刻**休眠（相对休眠会累积 tick 自身耗时造成漂移）。
//! - **每输出 fps 上限**：[`VideoOptions::fps_limit`] 限制两次呈现的最小间隔。
//! - **暂停/恢复/循环**：暂停停止拉帧；恢复时 epoch 平移暂停时长，
//!   避免恢复后疯狂丢帧；循环播放通过 [`FrameProducer::seek_start`] 重播。
//! - **可观测状态**：[`VideoStats`] 暴露解码器、fps、丢帧计数、队列长度与最近 PTS。
//! - 不可恢复错误经 [`MediaError`] 上抛，由上层回退静态帧（本层不接管）。
//!
//! 刻意**没有**给"上层的坏节拍"打补丁的自愈时钟：节拍由上层按
//! [`VideoPlayer::next_deadline`] 精确驱动；本层只做「到期呈现 / 过期丢弃」。
//!
//! **唯一例外：产能不足**。若生产者**持续**低于源帧率（例如 4K 60fps 源在只能
//! 解到 ~45fps 的路径上），"迟到"会变成**每一帧都迟到** ⇒ 整队列被丢空 ⇒ 画面
//! 永远停在上一帧（实测表现为 `presented` 恒为 0，看门狗判停摆）。这与"坏节拍"
//! 不同：它不是上层驱动错了，而是**源超过本路径的产能**。
//! 因此当**连续丢弃数达到队列深度**（等于"整个队列没呈现过一帧"）时，重锚一次
//! 时间轴到当前帧，并计入 [`VideoPlayer::reanchored`]，让画面继续以**可达到的
//! 速率**播放（丢帧但不出静止画面）。偶发迟到仍按原语义丢弃。
//!
//! 无硬件即可完整测试：[`MockProducer`] 提供确定性合成帧，
//! 测试内另有脚本化生产者验证调度语义。

use crate::{FramePoll, MediaError, MediaSource};
use liuguang_core::types::{CpuFrame, DisplayFrame, MediaInfo, MediaKind, PixelFormat};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 解码器种类（可观测状态的一部分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecoderKind {
    /// VA-API 硬件解码（如 `vaapih264dec`）。
    Vaapi,
    /// 软件解码（如 `avdec_h264`）。
    Software,
    /// 无解码器（静态图等路径）。
    None,
    /// 测试用合成生产者。
    Mock,
}

impl DecoderKind {
    /// 是否为硬件解码路径。
    pub const fn is_hardware(self) -> bool {
        matches!(self, Self::Vaapi)
    }
}

/// [`FrameProducer`] 的单次产出。
#[derive(Debug, Clone)]
pub enum VideoPoll {
    /// 解码出一帧（携带媒体 PTS）。
    Frame {
        /// 媒体呈现时间戳（相对媒体起点）。
        pts: Duration,
        /// 解码帧。
        frame: DisplayFrame,
    },
    /// 暂时没有帧（解码管线在跑或尚未产出），调用方稍后再试。
    NeedMore,
    /// 生产者已到流末尾。
    Ended,
}

/// DMA-BUF 直通的结构化状态（由生产者上报，用于 status/日志）。
///
/// `Default` 是"未知/非直通"：`is_dmabuf=false`、`fallbacks=0`、`keepalive_len=0`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DmabufState {
    /// 当前是否真的走 DMA-BUF 直通。
    pub is_dmabuf: bool,
    /// 构造期回退次数（>0 = 发生过回退）。
    pub fallbacks: u64,
    /// keepalive 环当前持有的帧数。
    pub keepalive_len: usize,
}

/// 视频帧生产者：底层解码管线（GStreamer / mock）的最小抽象。
///
/// 实现者负责解码与 PTS 标注；调度、有界队列与丢帧策略由
/// [`VideoPlayer`] 统一实现。
pub trait FrameProducer: Send {
    /// 媒体元数据（尺寸/时长/源帧率/编码）。
    fn info(&self) -> MediaInfo;
    /// 拉取下一帧。不阻塞等待解码；无帧时返回 [`VideoPoll::NeedMore`]。
    ///
    /// # Errors
    ///
    /// 解码失败时返回 [`MediaError`]，不 panic。
    fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError>;
    /// DMA-BUF 直通状态；不支持该概念的实现返回 `None`（默认）。
    fn dmabuf_state(&self) -> Option<DmabufState> {
        None
    }
    /// 回到流起点（循环重播）。成功后 PTS 从 0 重新计数。
    ///
    /// # Errors
    ///
    /// seek 失败时返回 [`MediaError`]。
    fn seek_start(&mut self) -> Result<(), MediaError>;
    /// 实际使用的解码器种类。
    fn decoder_kind(&self) -> DecoderKind;
    /// 设置「丢弃水位」（媒体时间）：早于该 PTS 的样本可在**不做像素
    /// 转换**的前提下直接丢弃。高帧率源 + 低输出帧率时显著省 CPU。
    ///
    /// 默认无操作（合成生产者无转换成本）。
    fn set_drop_horizon(&mut self, _horizon: Option<Duration>) {}
    /// 源侧已跳过（未做像素转换）的帧数，用于可观测上报。
    ///
    /// 默认 0（合成生产者不做像素转换，没有"跳过"概念）。
    fn skipped_frames(&self) -> u64 {
        0
    }
    /// 控制底层解码运行：`false` 时应暂停管线（省电），`true` 恢复。
    ///
    /// 默认无操作。
    fn set_playing(&mut self, _playing: bool) -> Result<(), MediaError> {
        Ok(())
    }
    /// 建议后续帧按 `width`×`height`（面板物理像素）出图，省掉合成器那次
    /// 全屏缩放。
    ///
    /// 默认无操作：只有"自己渲染到输出缓冲"的生产者（RGB 转换）从中受益，
    /// 直接转发解码表面的生产者改不了尺寸。
    fn set_output_size(&mut self, _width: u32, _height: u32) {}
    /// 运行期无闪烁更新渲染变换（fit/zoom/offset/rotation/brightness）。
    ///
    /// 由 GPU 管线在下一帧着色器中直接生效，无需重建解码管线。
    fn set_render_transform(&mut self, _transform: liuguang_core::transform::RenderTransform) {}
    /// 生产者是否正在GPU侧自行应用 [`liuguang_core::transform::RenderTransform`]。
    fn handles_render_transform(&self) -> bool {
        false
    }
}

/// 视频播放选项。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoOptions {
    /// 有界帧队列容量（1..=16），默认 3。0 视为非法（构造时校验报错）。
    pub queue_size: usize,
    /// 输出帧率上限（每秒呈现帧数）；`None` 表示不额外限速。
    pub fps_limit: Option<f64>,
    /// 迟到容忍：媒体时间晚于 pts + tolerance 的帧直接丢弃计
    /// [`VideoStats::dropped_late`]；默认 100 ms。
    pub drop_late_tolerance: Duration,
    /// 是否循环播放（默认 true）。
    pub looping: bool,
    /// **滞后呈现帧数（t49，默认 1）**：队列里至少留这么多帧才下发。
    ///
    /// 动机：直通路径下解码器 surface 被直接送去扫描，若"刚到手就下发"，它的
    /// **写可能仍在飞行**（H-fence）⇒ 用户看到的横向带 / 角色重影 / 色斑。
    /// 留 1 帧（20fps ⇒ ≥50ms）让每个被下发的 buffer 至少经历一个帧周期。
    ///
    /// **时序启发式，不是同步保证**（真正的保证是显示侧 `IN_FENCE_FD`）。
    /// `queue_size=1` 时按 0 处理（否则永远不会呈现）。
    pub lag_frames: usize,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            queue_size: 3,
            fps_limit: None,
            drop_late_tolerance: Duration::from_millis(100),
            looping: true,
            lag_frames: 1,
        }
    }
}

impl VideoOptions {
    /// 队列容量上限。
    pub const MAX_QUEUE_SIZE: usize = 16;

    /// 校验选项合法性。
    ///
    /// # Errors
    ///
    /// `queue_size` 越界（0 或 > 16）或 `fps_limit` 非有限正数时返回
    /// [`MediaError::InvalidOptions`]。
    pub fn validate(self) -> Result<(), MediaError> {
        if self.queue_size == 0 || self.queue_size > Self::MAX_QUEUE_SIZE {
            return Err(MediaError::InvalidOptions(format!(
                "queue_size 必须在 1..={} 之间，实际 {}",
                Self::MAX_QUEUE_SIZE,
                self.queue_size
            )));
        }
        if let Some(fps) = self.fps_limit {
            if !fps.is_finite() || fps <= 0.0 {
                return Err(MediaError::InvalidOptions(format!(
                    "fps_limit 必须为有限正数，实际 {fps}"
                )));
            }
        }
        Ok(())
    }
}

/// 视频播放可观测状态（规范 §6.4）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoStats {
    /// 已呈现帧数。
    pub presented: u64,
    /// 因迟到（超过容忍）被丢弃的帧数。
    pub dropped_late: u64,
    /// 因队列满被拒绝入队的帧数（push 路径统计）。
    pub dropped_queue_full: u64,
    /// 当前队列长度。
    pub queue_len: usize,
    /// 源侧提前丢弃（未做像素转换；高帧率源 + 低输出帧率时发生）。
    pub skipped: u64,
    /// 实际解码器种类。
    pub decoder: DecoderKind,
    /// 源帧率（生产者上报）。
    pub source_fps: Option<f64>,
    /// 最近呈现帧的 PTS。
    pub last_pts: Option<Duration>,
}

/// 队列中的待呈现帧。
struct QueuedFrame {
    pts: Duration,
    frame: DisplayFrame,
}

/// 循环重播时单次 `poll_frame` 内允许的重启次数上限（防空转）。
const MAX_LOOP_RESTARTS: u32 = 2;

/// 连续"重启后仍无帧"的重启次数上限（达到即判停摆，不再重试）。
///
/// 判据锚在**事实量**上（seek 之后是否真的产出过帧），既不用墙钟也不数尝试次数：
/// - 慢 seek（4K 回填可达数百毫秒）只是"还没到"——一旦出帧就清零，不误判；
/// - 极低 fps（1 fps 是合法配置）同理，出帧即清零；
/// - 而"永远不出帧"（文件被删/截断、解码器死掉、seek 后无数据）必然有限收敛。
///
/// 真机教训（AUDIT/t6 F1）：曾经每次重启都只做 1 次 seek 且无限重试，文件被删后
/// `presented` 冻结、状态永远 `VideoPlaying`、5 秒内 EOS+seek **1222 次**、
/// CPU 恒 ~8% 单核且**零 WARN**。有限收敛是契约，不是"把上限调大"。
const MAX_FRUITLESS_RESTARTS: u32 = 2;

/// 队列空、但生产者仍在供帧时的回探间隔。
///
/// **绝不在"生产者活跃"时返回"无期限"**（`next_in: None`）：上层一旦收到
/// 无期限就只能退化成兜底长休眠，视频会停住。`None` 只保留给真正的
/// "等外部事件"语义（暂停：等 resume）。
const DECODE_PROBE: Duration = Duration::from_millis(4);

/// 单次 `poll_frame` 内补队列的帧数上限（队列本身已是有界容量）。
///
/// 补到队列容量（而非只补 1 帧）是为了让解码与呈现流水起来：每次唤醒只补
/// 1 帧会让"解码→呈现"串行化，在 60fps 源上直接掉到源帧率以下。
const MAX_FILL_PER_CALL: usize = 4;

/// 视频播放器：包住任意 [`FrameProducer`]，实现 [`MediaSource`] 调度语义。
///
/// 时间轴：首次 `poll_frame` 的 `now` 记为 epoch，帧 `pts` 对应
/// `epoch + pts` 的单调时刻（**到点呈现**）；`pts` 早于媒体时间
/// `drop_late_tolerance` 以上的帧丢弃计 [`VideoStats::dropped_late`]
/// （迟到太多就不再补播，保持与媒体时间轴对齐）。
pub struct VideoPlayer<P: FrameProducer> {
    producer: P,
    options: VideoOptions,
    queue: VecDeque<QueuedFrame>,
    /// 单调时间轴起点（首次 poll 或循环重播时锚定）。
    epoch: Option<Instant>,
    paused: bool,
    paused_at: Option<Instant>,
    /// 生产者已报告 [`VideoPoll::Ended`]（队列仍可有残帧待排空）。
    producer_done: bool,
    /// 自上次循环重播以来**是否真的产出过帧**（停摆判据的"事实量"）。
    saw_frame_since_restart: bool,
    /// 连续"重启后仍无帧"的次数（见 [`MAX_FRUITLESS_RESTARTS`]）。
    fruitless_restarts: u32,
    /// 时间轴等待"第一帧"锚定（启动时/循环重播后为 true）。
    ///
    /// 到达性：只要为 true，队列里出现第一帧的那次 `poll_frame` 才锚定时间轴，
    /// 于是"首帧立即到期"——不会因为打开/seek 的回填耗时把开头判成迟到丢弃。
    reanchor_on_first_frame: bool,
    last_present_at: Option<Instant>,
    last_pts: Option<Duration>,
    presented: u64,
    dropped_late: u64,
    dropped_queue_full: u64,
    source_fps: Option<f64>,
    /// 连续被判"迟到"而丢弃的帧数；呈现一帧即清零。
    consecutive_late: u32,
    /// 因"产能不足"重锚时间轴的次数（可观测：>0 说明源帧率超过本路径能力）。
    reanchored: u64,
}

impl<P: FrameProducer> VideoPlayer<P> {
    /// 创建播放器并校验选项。
    ///
    /// # Errors
    ///
    /// 选项非法时返回 [`MediaError::InvalidOptions`]。
    pub fn new(producer: P, options: VideoOptions) -> Result<Self, MediaError> {
        options.validate()?;
        let source_fps = producer.info().source_fps;
        Ok(Self {
            producer,
            options,
            queue: VecDeque::new(),
            epoch: None,
            paused: false,
            paused_at: None,
            producer_done: false,
            saw_frame_since_restart: false,
            fruitless_restarts: 0,
            // 启动时同样等第一帧到达再锚定时间轴。
            reanchor_on_first_frame: true,
            last_present_at: None,
            last_pts: None,
            presented: 0,
            dropped_late: 0,
            dropped_queue_full: 0,
            source_fps,
            consecutive_late: 0,
            reanchored: 0,
        })
    }

    /// 因"产能不足"重锚时间轴的次数（见模块文档；偶发迟到不计入）。
    pub const fn reanchored(&self) -> u64 {
        self.reanchored
    }

    /// 当前可观测状态。
    pub fn stats(&self) -> VideoStats {
        VideoStats {
            presented: self.presented,
            dropped_late: self.dropped_late,
            dropped_queue_full: self.dropped_queue_full,
            queue_len: self.queue.len(),
            // 源侧跳过计数由生产者上报（播放器自己没有跳过能力）。
            skipped: self.producer.skipped_frames(),
            decoder: self.producer.decoder_kind(),
            source_fps: self.source_fps,
            last_pts: self.last_pts,
        }
    }

    /// 当前生效选项。
    pub fn options(&self) -> VideoOptions {
        self.options
    }

    /// 下一次需要推进本视频源的**绝对**时刻（队首帧到期时间与输出 fps 门控取较晚者）。
    ///
    /// 上层据此休眠：用绝对时刻而非"相对间隔"可避免把 tick 自身耗时累加进
    /// 唤醒漂移（相对间隔会让每帧都晚 `tick 耗时`，很快超出迟到容忍）。
    ///
    /// `None` 表示队列为空、到期时间不可知（生产者在途），调用方应短间隔回探。
    pub fn next_deadline(&self) -> Option<Instant> {
        let epoch = self.epoch?;
        let head = self.queue.front()?;
        let due = epoch.checked_add(head.pts)?;
        let gate = self.fps_gate_next_at();
        Some(gate.map_or(due, |g| due.max(g)))
    }

    /// fps 门控的下一次可呈现时刻（无上限或尚未呈现过帧时为 `None`）。
    ///
    /// **门控只限速，不追罚**：纯 `上次呈现 + 1/fps` 会把每次唤醒的调度延迟
    /// （4K60 真机实测 ≈0.1ms/帧）单向累加进时间轴——门控永远比队首的到期时刻
    /// 更晚，队列稳态滞后从 0 漂到 [`VideoOptions::drop_late_tolerance`]，于是
    /// 每几秒丢一帧（预读做足后仍会丢，见 0.2.0-13 的实测）。
    ///
    /// 这里给门控一个"最多比队首到期晚一帧"的上限：落后时允许**立刻**呈现把
    /// 累积的滞后补回来，而不是把它固化。滞后因此被夹在 `[0, 1/fps]` 内，
    /// 远低于丢帧容差；源帧率高于上限时（队首到期在将来）门控照旧生效。
    fn fps_gate_next_at(&self) -> Option<Instant> {
        let fps = self.options.fps_limit?;
        if !fps.is_finite() || fps <= 0.0 {
            return None;
        }
        let min_gap = Duration::from_secs_f64(1.0 / fps);
        let gate = self.last_present_at?.checked_add(min_gap)?;
        // 上限**低于**源帧率时（用户调低 `video_fps_limit` 降载）门控是排程本身，
        // 不能因为"队首已到期"就放行——那会让上限失效（回归测试
        // `fps_gate_halves_presentation_rate_without_starvation`）。
        let limiting = self.source_fps.is_some_and(|src| src > fps * 1.01);
        if limiting {
            return Some(gate);
        }
        // 上限不低于源帧率：门控只是"别在同一刷新周期里连发两帧"，不该成为
        // 时间轴的额外延迟。用 `due + 一帧` 封顶，迟到时允许立刻补上。
        let due = self
            .epoch
            .zip(self.queue.front())
            .and_then(|(epoch, head)| epoch.checked_add(head.pts));
        Some(match due.and_then(|due| due.checked_add(min_gap)) {
            Some(ceiling) => gate.min(ceiling),
            None => gate,
        })
    }

    /// 运行期改写输出帧率上限（`performance.reduce_load_on_cpu_pressure` 的执行机构）。
    ///
    /// 只影响**门控与水位**：门控看 `last_present_at + 1/fps`，水位按门周期收紧，
    /// 因此降上限的下一帧起就少提交、并让生产者在像素转换**之前**多丢帧。
    /// 恢复配置值由调用方保存（本类型不记住"原始上限"）。
    pub fn set_fps_cap(&mut self, fps: Option<f64>) {
        if self.options.fps_limit == fps {
            return;
        }
        self.options.fps_limit = fps;
        self.update_drop_horizon(Instant::now());
    }

    /// 暂停（时间戳由调用方给出，便于确定性测试）。
    ///
    /// # Errors
    ///
    /// 已处于暂停状态时返回 [`MediaError::InvalidState`]。
    pub fn pause_at(&mut self, now: Instant) -> Result<(), MediaError> {
        // 暂停时停下解码管线（避免空转解码）。
        self.producer.set_playing(false)?;
        if self.paused {
            return Err(MediaError::InvalidState("已处于暂停状态".into()));
        }
        self.paused = true;
        self.paused_at = Some(now);
        Ok(())
    }

    /// 恢复：把 epoch 后移暂停时长（时间戳由调用方给出）。
    ///
    /// # Errors
    ///
    /// 未处于暂停状态时返回 [`MediaError::InvalidState`]。
    pub fn resume_at(&mut self, now: Instant) -> Result<(), MediaError> {
        self.producer.set_playing(true)?;
        if !self.paused {
            return Err(MediaError::InvalidState("未处于暂停状态".into()));
        }
        self.paused = false;
        if let (Some(epoch), Some(paused_at)) = (self.epoch, self.paused_at) {
            if now > paused_at {
                let shift = now.saturating_duration_since(paused_at);
                if let Some(shifted) = epoch.checked_add(shift) {
                    self.epoch = Some(shifted);
                }
            }
        }
        self.paused_at = None;
        Ok(())
    }

    /// 主动投递一帧（push 模式生产者/测试用）。
    ///
    /// 队列满时拒绝入队并计 [`VideoStats::dropped_queue_full`]，
    /// 保证队列长度永不超上限。
    pub fn offer_frame(&mut self, pts: Duration, frame: DisplayFrame) {
        if self.queue.len() >= self.options.queue_size {
            self.dropped_queue_full += 1;
            return;
        }
        self.queue.push_back(QueuedFrame { pts, frame });
    }

    /// 从生产者补帧，直到队列达到容量、生产者暂无数据或达到单次上限。
    ///
    /// 每次 `poll_frame` 都会把队列补到容量（上限 [`MAX_FILL_PER_CALL`] 帧），
    /// 让后续唤醒直接拥有"下一帧到期时刻"（[`Self::next_deadline`]），
    /// 无需盲探。工作量有界：最多 `queue_size` 帧的像素转换。
    fn fill_queue(&mut self) -> Result<(), MediaError> {
        let mut filled = 0usize;
        while !self.producer_done
            && self.queue.len() < self.options.queue_size
            && filled < MAX_FILL_PER_CALL
        {
            match self.producer.next_producer_frame()? {
                VideoPoll::Frame { pts, frame } => {
                    self.queue.push_back(QueuedFrame { pts, frame });
                    // 停摆判据的事实量：生产者**真的**产出了帧。
                    self.saw_frame_since_restart = true;
                    filled += 1;
                }
                VideoPoll::NeedMore => break,
                VideoPoll::Ended => {
                    self.producer_done = true;
                    break;
                }
            }
        }
        Ok(())
    }

    /// 把「丢弃水位」下发生产者：早于 `media_now − 容忍` 的样本无需像素转换。
    ///
    /// **队列为空时不下发**：否则时钟一旦跑在解码前面，水位会吃掉所有
    /// 后续帧 → 队列永空 → 供给饥饿（真机实测的饥饿死锁）。
    ///
    /// **按呈现排程预测水位**（2026-10-20 修）：有 fps 门控时每 `1/fps_limit`
    /// 最多呈现一帧，因此**比一个门周期更老的帧注定被丢** —— 把水位收紧到一个
    /// 门周期，就能在 `sample_to_frame` **之前**跳过它们，省下每帧 12–33MB 的
    /// 像素搬运（下载 + 行拷贝）。此前水位只用 `drop_late_tolerance`（默认
    /// 100ms），远大于 30fps 的 33ms 门控间隔 ⇒ 判不出"注定被丢"的帧 ⇒
    /// **4K120 源仍会下载并拷贝全部源帧**（用户实测反馈）。
    ///
    /// 只有当门控确实生效时才收紧（`fps_limit` 为 `None` 时行为不变）。
    fn update_drop_horizon(&mut self, now: Instant) {
        // 循环重播的回填窗口内不下发水位：此时时间轴尚未重锚，
        // 若按旧时间轴算水位，新一轮开头的样本会被当成"过期"直接跳过。
        if self.reanchor_on_first_frame {
            self.producer.set_drop_horizon(None);
            return;
        }
        let Some(epoch) = self.epoch else { return };
        let horizon = if self.queue.is_empty() {
            None
        } else {
            let media_now = now.saturating_duration_since(epoch);
            // 有 fps 门控 ⇒ 一个门周期内最多呈现一帧 ⇒ 更老的帧不可能被呈现。
            let age = match self.options.fps_limit {
                Some(fps) if fps.is_finite() && fps > 0.0 => {
                    let gate = Duration::from_secs_f64(1.0 / fps);
                    self.options.drop_late_tolerance.min(gate)
                }
                _ => self.options.drop_late_tolerance,
            };
            Some(media_now.saturating_sub(age))
        };
        self.producer.set_drop_horizon(horizon);
    }

    /// fps 上限门控：两次呈现间隔不足 `1/fps_limit` 时延迟出队。
    fn fps_gate_pending(&self, now: Instant) -> Option<FramePoll> {
        let next_at = self.fps_gate_next_at()?;
        if now < next_at {
            return Some(FramePoll::Pending {
                next_in: Some(next_at.saturating_duration_since(now)),
            });
        }
        None
    }

    /// 调度队首：过期丢弃 → 到期呈现 → 未到期 Pending。
    /// 返回 `None` 表示队列已空。
    ///
    /// 到期判据是**帧自己的 PTS**（`media_now >= pts`）：提前呈现会让视频
    /// 按解码速度而不是源帧率播放（硬解下可达实时数倍）。
    fn schedule_head(&mut self, now: Instant) -> Option<FramePoll> {
        let epoch = self.epoch?;
        let tolerance = self.options.drop_late_tolerance;
        loop {
            let pts = self.queue.front()?.pts;
            let media_now = now.saturating_duration_since(epoch);
            // 过期（pts 早于媒体时间 tolerance 以上）：丢弃继续看下一帧。
            if media_now > pts.saturating_add(tolerance) {
                tracing::debug!(
                    lag_ms = media_now.saturating_sub(pts).as_millis() as u64,
                    queue_len = self.queue.len(),
                    consecutive_late = self.consecutive_late,
                    reanchored = self.reanchored,
                    "队首过期：丢弃一帧（dropped_late）"
                );
                self.queue.pop_front();
                self.dropped_late += 1;
                self.consecutive_late = self.consecutive_late.saturating_add(1);
                // 产能不足：整个队列都没呈现过一帧 ⇒ 继续丢只会让画面永远静止。
                // 把时间轴重锚到这一帧（此后按可达速率播放），并单独计数。
                if self.consecutive_late >= self.options.queue_size.max(1) as u32 {
                    self.reanchored += 1;
                    self.consecutive_late = 0;
                    // epoch = now − pts ⇒ 这一帧此刻到期。checked_sub 防 pts 超过进程运行时长。
                    self.epoch = Some(now.checked_sub(pts).unwrap_or(now));
                    tracing::debug!(
                        reanchored = self.reanchored,
                        dropped_late = self.dropped_late,
                        pts_ms = pts.as_millis() as u64,
                        "源帧率超过本路径产能：重锚时间轴以继续出画（不是坏节拍补丁）"
                    );
                    continue;
                }
                continue;
            }
            if media_now >= pts {
                // t49：**滞后一帧呈现**——队里只剩 1 帧时不下发（那帧刚到手，
                // 解码写可能未完成）。等到下一帧入队（≈1 帧周期）再下发最旧那帧。
                // 时序启发式，不是同步保证（见 `VideoOptions::lag_frames`）。
                let lag = self
                    .options
                    .lag_frames
                    .min(self.options.queue_size.saturating_sub(1));
                if lag > 0 && self.queue.len() <= lag {
                    return Some(FramePoll::Pending {
                        next_in: Some(Duration::from_millis(5)),
                    });
                }
                let queued = self.queue.pop_front()?;
                self.consecutive_late = 0;
                self.presented += 1;
                self.last_present_at = Some(now);
                self.last_pts = Some(queued.pts);
                return Some(FramePoll::Ready(queued.frame));
            }
            return Some(FramePoll::Pending {
                next_in: Some(pts.saturating_sub(media_now)),
            });
        }
    }

    /// 循环重播：seek 回起点，并把时间轴**惰性重锚**。
    ///
    /// 不在这里写 `epoch = Some(now)`：seek 之后到第一帧到达之间可能有一段
    /// 空窗（seek 需要时间、解码要回填），若此刻锚死时间轴，等下一帧真的
    /// 到达时 `media_now` 已经跑过 PTS，整段开头会被判"迟到"逐帧丢弃。
    /// 这里只打标记；`poll_frame` 会在**重播后第一帧到达的那次调用**里重锚，
    /// 于是 seek 后首帧立即到期（回填窗口内也不下发丢帧水位）。
    /// # Errors
    ///
    /// 连续 [`MAX_FRUITLESS_RESTARTS`] 次重播都**没有产出任何帧**时返回
    /// [`MediaError::Decode`]（停摆）：交给上层既有错误路径收敛
    /// （`drive(ErrorOccurred)` + 后备画面 + `EventKind::Error`），
    /// 而不是无限 seek 空转。
    fn restart_loop(&mut self) -> Result<(), MediaError> {
        // 判据：上一次重播之后一帧都没产出，才算"一次失败的重播"。
        if self.saw_frame_since_restart {
            self.fruitless_restarts = 0;
        } else {
            self.fruitless_restarts += 1;
        }
        if self.fruitless_restarts > MAX_FRUITLESS_RESTARTS {
            return Err(MediaError::Decode(format!(
                "循环重播连续 {} 次未产出任何帧（seek 后无数据）：判为停摆，停止重试",
                self.fruitless_restarts
            )));
        }
        if self.fruitless_restarts > 0 {
            tracing::warn!(
                presented = self.presented,
                dropped_late = self.dropped_late,
                fruitless_restarts = self.fruitless_restarts,
                "循环重播：seek 回起点（时间轴惰性重锚）"
            );
        } else {
            tracing::debug!(
                presented = self.presented,
                dropped_late = self.dropped_late,
                "循环重播：seek 回起点（时间轴惰性重锚）"
            );
        }
        self.producer.seek_start()?;
        self.producer_done = false;
        self.reanchor_on_first_frame = true;
        self.saw_frame_since_restart = false;
        Ok(())
    }
}

impl<P: FrameProducer> MediaSource for VideoPlayer<P> {
    fn metadata(&self) -> MediaInfo {
        self.producer.info()
    }

    fn poll_frame(&mut self, now: Instant) -> Result<FramePoll, MediaError> {
        if self.paused {
            return Ok(FramePoll::Pending { next_in: None });
        }
        if let Some(pending) = self.fps_gate_pending(now) {
            return Ok(pending);
        }
        for _ in 0..MAX_LOOP_RESTARTS {
            // 时间轴锚定（首次 poll / 循环 seek 之后的惰性重锚都在这里发生）。
            if self.epoch.is_none() {
                self.epoch = Some(now);
            }
            self.update_drop_horizon(now);
            self.fill_queue()?;
            // 循环重播后的第一帧到达：以"现在"为新的时间轴起点。
            // 这样 seek/回填耗时（真机可达数十到数百毫秒）不会把新一轮开头
            // 判成"迟到"逐帧丢弃 —— 这是"播完一次就卡住/开头跳帧"的修复点。
            if self.reanchor_on_first_frame && !self.queue.is_empty() {
                self.reanchor_on_first_frame = false;
                self.epoch = Some(now);
                self.last_present_at = None;
            }
            if let Some(poll) = self.schedule_head(now) {
                return Ok(poll);
            }
            // 队列已空。
            if !self.producer_done {
                // 生产者还在跑但暂无数据（解码在途/seek 后回填）：给出**有界**
                // 短期限，调用方据此回探。这里返回 None 会让上层退化成长休眠。
                return Ok(FramePoll::Pending {
                    next_in: Some(DECODE_PROBE),
                });
            }
            if self.options.looping {
                self.restart_loop()?;
                continue;
            }
            return Ok(FramePoll::Ended);
        }
        Ok(FramePoll::Pending {
            next_in: Some(DECODE_PROBE),
        })
    }

    fn pause(&mut self) -> Result<(), MediaError> {
        self.pause_at(Instant::now())
    }

    fn resume(&mut self) -> Result<(), MediaError> {
        self.resume_at(Instant::now())
    }
}

/// 视频通道汇总 trait（对象安全）：播放 + 可观测状态 + 唤醒期限。
///
/// daemon 以 `Box<dyn VideoSource + Send>` 同时驱动播放、读取
/// 解码器/fps/丢帧数/队列长度（规范 §6.4），并按
/// [`VideoSource::next_deadline`] 精确唤醒。
pub trait VideoSource: MediaSource {
    /// 当前视频可观测状态。
    fn video_stats(&self) -> VideoStats;
    /// 下一次需要推进的绝对时刻；`None` = 队列为空、期限未知（短间隔回探）。
    fn next_deadline(&self) -> Option<Instant>;
    /// DMA-BUF 直通状态；不支持该概念的实现返回 `None`（默认）。
    fn dmabuf_state(&self) -> Option<DmabufState> {
        None
    }
    /// 运行期改写输出帧率上限（降载用）；`None` = 不额外限速。
    ///
    /// 默认无操作：没有"输出门控"概念的实现（直接转发帧的源）无从限速。
    fn set_fps_cap(&mut self, _fps: Option<f64>) {}
    /// 建议按 `width`×`height`（面板物理像素）出图；默认无操作。
    fn set_output_size(&mut self, _width: u32, _height: u32) {}
    /// 更新实时画面变换与亮度，无需重建解码管线；默认无操作。
    fn set_render_transform(&mut self, _transform: liuguang_core::transform::RenderTransform) {}
    /// 是否由内部管线直接在 GPU 端完成缩放/偏移/旋转与亮度（即跳过 CPU 端变换）。
    fn handles_render_transform(&self) -> bool {
        false
    }
}

impl<P: FrameProducer> VideoSource for VideoPlayer<P> {
    fn video_stats(&self) -> VideoStats {
        self.stats()
    }

    fn next_deadline(&self) -> Option<Instant> {
        VideoPlayer::next_deadline(self)
    }

    /// 转发到内部生产者（`GstProducer` 会给出真实直通状态；mock 返回 `None`）。
    fn dmabuf_state(&self) -> Option<DmabufState> {
        self.producer.dmabuf_state()
    }

    fn set_fps_cap(&mut self, fps: Option<f64>) {
        VideoPlayer::set_fps_cap(self, fps);
    }

    /// 转发到内部生产者：会自绘输出缓冲的生产者据此改成按显示尺寸渲染。
    fn set_output_size(&mut self, width: u32, height: u32) {
        self.producer.set_output_size(width, height);
    }

    fn set_render_transform(&mut self, transform: liuguang_core::transform::RenderTransform) {
        self.producer.set_render_transform(transform);
    }

    fn handles_render_transform(&self) -> bool {
        self.producer.handles_render_transform()
    }
}

/// 确定性合成帧生产者：每帧纯色，帧号编码进像素（小端）。
///
/// PTS = `n * frame_interval`；`decoder_kind()` 为 [`DecoderKind::Mock`]。
/// 供测试与集成验证使用，不依赖任何硬件。
pub struct MockProducer {
    width: u32,
    height: u32,
    frame_count: u64,
    frame_interval: Duration,
    next: u64,
    info: MediaInfo,
}

impl MockProducer {
    /// 创建合成生产者。
    ///
    /// `width`/`height` 为 0 时按 1 处理（避免构造非法帧）；
    /// `frame_interval` 为 0 时全部帧 PTS 为 0（一次性全部到期）。
    pub fn new(width: u32, height: u32, frame_count: u64, frame_interval: Duration) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let duration = frame_interval.saturating_mul(frame_count.min(u32::MAX as u64) as u32);
        let source_fps = if frame_interval.is_zero() {
            None
        } else {
            let secs = frame_interval.as_secs_f64();
            if secs > 0.0 && secs.is_finite() {
                Some(1.0 / secs)
            } else {
                None
            }
        };
        let info = MediaInfo {
            kind: MediaKind::Video,
            width,
            height,
            duration: Some(duration),
            source_fps,
            codec: Some("mock".into()),
        };
        Self {
            width,
            height,
            frame_count,
            frame_interval,
            next: 0,
            info,
        }
    }

    /// 从合成帧像素中还原帧号（每像素首字节起 3 字节小端帧号 + 0xFF）。
    ///
    /// 非合成帧或数据过短时返回 `None`。
    pub fn frame_number(frame: &CpuFrame) -> Option<u64> {
        let data = frame.data.get(0..4)?;
        Some(u64::from(data[0]) | (u64::from(data[1]) << 8) | (u64::from(data[2]) << 16))
    }

    /// 构造第 `n` 号合成帧。
    fn make_frame(&self, n: u64) -> DisplayFrame {
        let stride = self.width.saturating_mul(4);
        let len = (stride as usize).saturating_mul(self.height as usize);
        let mut data = vec![0u8; len];
        let pixel = [
            (n & 0xff) as u8,
            ((n >> 8) & 0xff) as u8,
            ((n >> 16) & 0xff) as u8,
            0xff,
        ];
        for chunk in data.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
        DisplayFrame::Cpu(CpuFrame {
            width: self.width,
            height: self.height,
            stride,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data.into_boxed_slice()),
        })
    }
}

impl FrameProducer for MockProducer {
    fn info(&self) -> MediaInfo {
        self.info.clone()
    }

    fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError> {
        if self.next >= self.frame_count {
            return Ok(VideoPoll::Ended);
        }
        let n = self.next;
        self.next += 1;
        Ok(VideoPoll::Frame {
            pts: self
                .frame_interval
                .saturating_mul(n.min(u32::MAX as u64) as u32),
            frame: self.make_frame(n),
        })
    }

    fn seek_start(&mut self) -> Result<(), MediaError> {
        self.next = 0;
        Ok(())
    }

    fn decoder_kind(&self) -> DecoderKind {
        DecoderKind::Mock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::types::FrameKind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 脚本化生产者：按预设 (pts, 帧号) 序列产出，记录 seek 次数。
    struct ScriptedProducer {
        frames: Vec<(Duration, u64)>,
        pos: usize,
        seeks: AtomicUsize,
        width: u32,
        height: u32,
        interval: Duration,
    }

    impl ScriptedProducer {
        fn new(frames: &[(u64, u64)], interval_ms: u64) -> Self {
            Self {
                frames: frames
                    .iter()
                    .map(|(ms, n)| (Duration::from_millis(*ms), *n))
                    .collect(),
                pos: 0,
                seeks: AtomicUsize::new(0),
                width: 4,
                height: 4,
                interval: Duration::from_millis(interval_ms),
            }
        }

        fn seek_count(&self) -> usize {
            self.seeks.load(Ordering::SeqCst)
        }
    }

    impl FrameProducer for ScriptedProducer {
        fn info(&self) -> MediaInfo {
            MediaInfo {
                kind: MediaKind::Video,
                width: self.width,
                height: self.height,
                duration: Some(Duration::from_millis(self.interval.as_millis() as u64)),
                source_fps: None,
                codec: Some("scripted".into()),
            }
        }

        fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError> {
            if self.pos >= self.frames.len() {
                return Ok(VideoPoll::Ended);
            }
            let (pts, n) = self.frames[self.pos];
            self.pos += 1;
            let mock = MockProducer::new(self.width, self.height, 1, Duration::ZERO);
            Ok(VideoPoll::Frame {
                pts,
                frame: mock.make_frame(n),
            })
        }

        fn seek_start(&mut self) -> Result<(), MediaError> {
            self.pos = 0;
            self.seeks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn decoder_kind(&self) -> DecoderKind {
            DecoderKind::Software
        }
    }

    fn player(frames: &[(u64, u64)], options: VideoOptions) -> VideoPlayer<ScriptedProducer> {
        VideoPlayer::new(ScriptedProducer::new(frames, 100), options).unwrap()
    }

    fn frame_num(f: &DisplayFrame) -> u64 {
        match f {
            DisplayFrame::Cpu(cpu) => MockProducer::frame_number(cpu).unwrap(),
            other => panic!("期望 CPU 帧，实际 {other:?}"),
        }
    }

    // ---- 选项校验 ----

    #[test]
    fn options_default_values() {
        let opt = VideoOptions::default();
        assert_eq!(opt.queue_size, 3);
        assert_eq!(opt.fps_limit, None);
        assert_eq!(opt.drop_late_tolerance, Duration::from_millis(100));
        assert!(opt.looping);
        assert!(opt.validate().is_ok());
    }

    #[test]
    fn options_reject_invalid_queue_size() {
        for bad in [0, VideoOptions::MAX_QUEUE_SIZE + 1] {
            let opt = VideoOptions {
                lag_frames: 0,
                queue_size: bad,
                ..Default::default()
            };
            assert!(matches!(opt.validate(), Err(MediaError::InvalidOptions(_))));
        }
        assert!(matches!(
            VideoPlayer::new(
                MockProducer::new(2, 2, 1, Duration::ZERO),
                VideoOptions {
                    lag_frames: 0,
                    queue_size: 0,
                    ..Default::default()
                }
            ),
            Err(MediaError::InvalidOptions(_))
        ));
    }

    #[test]
    fn options_reject_nonpositive_fps_limit() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let opt = VideoOptions {
                lag_frames: 0,
                fps_limit: Some(bad),
                ..Default::default()
            };
            assert!(matches!(opt.validate(), Err(MediaError::InvalidOptions(_))));
        }
    }

    // ---- MockProducer ----

    #[test]
    fn mock_producer_pts_and_frame_numbers() {
        let mut p = MockProducer::new(4, 2, 3, Duration::from_millis(40));
        assert_eq!(p.decoder_kind(), DecoderKind::Mock);
        let info = p.info();
        assert_eq!(info.kind, MediaKind::Video);
        assert_eq!(info.width, 4);
        assert_eq!(info.source_fps, Some(25.0));
        for n in 0..3u64 {
            match p.next_producer_frame().unwrap() {
                VideoPoll::Frame { pts, frame } => {
                    assert_eq!(pts, Duration::from_millis(40 * n));
                    let cpu = match &frame {
                        DisplayFrame::Cpu(c) => c,
                        other => panic!("期望 CPU 帧，实际 {other:?}"),
                    };
                    assert_eq!(MockProducer::frame_number(cpu), Some(n));
                    assert_eq!(cpu.format, PixelFormat::Rgba8888);
                    assert!(cpu.validate().is_ok());
                }
                other => panic!("期望 Frame，实际 {other:?}"),
            }
        }
        assert!(matches!(p.next_producer_frame().unwrap(), VideoPoll::Ended));
        p.seek_start().unwrap();
        match p.next_producer_frame().unwrap() {
            VideoPoll::Frame { pts, .. } => assert_eq!(pts, Duration::ZERO),
            other => panic!("seek 后期望 Frame，实际 {other:?}"),
        }
    }

    #[test]
    fn mock_producer_zero_interval_yields_zero_pts() {
        let mut p = MockProducer::new(1, 1, 2, Duration::ZERO);
        assert_eq!(p.info().source_fps, None);
        for _ in 0..2 {
            match p.next_producer_frame().unwrap() {
                VideoPoll::Frame { pts, .. } => assert_eq!(pts, Duration::ZERO),
                other => panic!("期望 Frame，实际 {other:?}"),
            }
        }
    }

    // ---- 队列与调度 ----

    /// 「滞后一帧」的**专门**单测（t52）。
    ///
    /// ## 为什么用「队列水位」而不是「帧号」
    /// FIFO 让首帧总是更旧那帧 ⇒ **只断言帧号不可证伪**。
    /// 而「`poll_frame` 一次拉几帧」已由代码确认：`fill_queue` 会把队列补到
    /// `queue_size`（上限 `MAX_FILL_PER_CALL=4`，每次 `next_producer_frame` 取一帧）。
    ///
    /// ## 判据（可证伪）
    /// 直接构造「队列里只有 1 帧」的状态：
    /// - `lag_frames=1` ⇒ `schedule_head` 必须返回 `Pending`（不下发，等下一帧入队）；
    /// - `lag_frames=0` ⇒ 同一状态必须返回 `Ready`。
    ///
    /// **变异**：把 `lag_frames` 改回 0 ⇒ 第一个断言失败。
    #[test]
    fn lag_frames_withholds_head_until_a_second_frame_is_queued() {
        use std::time::Instant as StdInstant;

        // 队列容量 3、只推入 1 帧（pts=0，epoch=now）⇒ 该帧"已到点"但队列水位=1。
        let mut with_lag = player(
            &[(0, 1), (100, 2)],
            VideoOptions {
                lag_frames: 1,
                queue_size: 3,
                looping: false,
                ..VideoOptions::default()
            },
        );
        let now = StdInstant::now();
        with_lag.epoch = Some(now);
        with_lag.queue.clear();
        with_lag.fill_queue().expect("fill");
        // fill_queue 会补满到 queue_size ⇒ 先清到只剩 1 帧以制造"水位=1"
        while with_lag.queue.len() > 1 {
            with_lag.queue.pop_back();
        }
        assert_eq!(with_lag.queue.len(), 1, "前提：队列水位 = 1");
        let poll_lag = with_lag.schedule_head(now).expect("应有判定");
        assert!(
            matches!(poll_lag, FramePoll::Pending { .. }),
            "lag_frames=1 时水位 1 必须 Pending（滞后一帧），实际 {poll_lag:?}"
        );

        // 同样水位，但 lag_frames=0 ⇒ 必须立即下发
        let mut no_lag = player(
            &[(0, 1), (100, 2)],
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                looping: false,
                ..VideoOptions::default()
            },
        );
        no_lag.epoch = Some(now);
        no_lag.queue.clear();
        no_lag.fill_queue().expect("fill");
        while no_lag.queue.len() > 1 {
            no_lag.queue.pop_back();
        }
        assert_eq!(no_lag.queue.len(), 1);
        let poll_no_lag = no_lag.schedule_head(now).expect("应有判定");
        assert!(
            matches!(poll_no_lag, FramePoll::Ready { .. }),
            "lag_frames=0 时水位 1 必须 Ready，实际 {poll_no_lag:?}"
        );
    }

    /// `lag_frames` 在 `queue_size=1` 时**自动退化为 0**（否则会永久 Pending）。
    #[test]
    fn lag_frames_degrades_to_zero_when_queue_is_one() {
        use std::time::Instant as StdInstant;
        let mut p = player(
            &[(0, 1), (100, 2)],
            VideoOptions {
                lag_frames: 1,
                queue_size: 1,
                looping: false,
                ..VideoOptions::default()
            },
        );
        let now = StdInstant::now();
        p.epoch = Some(now);
        p.queue.clear();
        p.fill_queue().expect("fill");
        while p.queue.len() > 1 {
            p.queue.pop_back();
        }
        let poll = p.schedule_head(now).expect("应有判定");
        assert!(
            matches!(poll, FramePoll::Ready { .. }),
            "queue_size=1 时 lag 必须退化为 0（否则永久 Pending），实际 {poll:?}"
        );
    }

    #[test]
    fn fill_queue_keeps_pipeline_full() {
        // 队列容量 3：一次 poll 应把队列补满（而不是只补 1 帧），
        // 否则"解码→呈现"串行化，在 60fps 源上直接掉到源帧率以下。
        let mut p = player(
            &[(0, 0), (100, 1), (200, 2), (300, 3), (400, 4)],
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 0),
            other => panic!("期望 Ready(0)，实际 {other:?}"),
        }
        assert_eq!(p.stats().queue_len, 2, "呈现一帧后队列应仍有 2 帧在流水");
    }

    #[test]
    fn frames_are_not_presented_before_their_pts() {
        // 提前呈现会让视频按**解码速度**播放（硬解下可达实时的数倍），
        // 而不是按源帧率。默认容忍 100ms 曾经把 due 提前 100ms，这里必须拒绝。
        let mut p = player(
            &[(0, 0), (100, 1), (200, 2)],
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 0),
            other => panic!("期望 Ready(0)，实际 {other:?}"),
        }
        // 同一时刻再 poll：第 1 帧（pts=100ms）不得提前呈现。
        match p.poll_frame(t0).unwrap() {
            FramePoll::Pending { next_in } => {
                assert_eq!(next_in, Some(Duration::from_millis(100)));
            }
            other => panic!("不得提前呈现未到期帧，实际 {other:?}"),
        }
        assert_eq!(p.stats().presented, 1);
        // 到期后才呈现。
        match p.poll_frame(t0 + Duration::from_millis(100)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 1),
            other => panic!("期望到期 Ready(1)，实际 {other:?}"),
        }
    }

    #[test]
    fn persistent_lateness_reanchors_instead_of_freezing() {
        // 产能不足（本测试用"时间轴跳 2 秒"模拟）：源 60fps，时间轴却落后 2 秒，
        // 于是队列里每一帧都超过 100ms 容忍 ⇒ 修前会被**全部丢弃**，presented 停在 1，
        // 画面永远静止（真机表现：presented 恒为 0，看门狗判停摆）。
        // 修后：连续丢弃数达到队列深度 ⇒ 重锚时间轴，继续出画，且 reanchored 可观测。
        let mut p = player(
            &[(0, 0), (16, 1), (33, 2), (50, 3), (66, 4), (83, 5)],
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 0),
            other => panic!("首帧应到期呈现，实际 {other:?}"),
        }
        assert_eq!(p.stats().presented, 1);

        // 时间轴跳 2 秒：剩下的帧全部"迟到"。真实调用方会反复 poll，
        // 所以这里模拟连续 poll（每次前进一帧周期）。
        let mut now = t0 + Duration::from_secs(2);
        for _ in 0..12 {
            let _ = p.poll_frame(now);
            now += Duration::from_millis(16);
        }
        let st = p.stats();
        assert!(
            p.reanchored() >= 1,
            "产能不足必须触发重锚（修前恒为 0），实际 reanchored={} presented={} dropped_late={}",
            p.reanchored(),
            st.presented,
            st.dropped_late
        );
        assert!(
            st.presented >= 2,
            "重锚后必须继续出画（修前 presented 恒为 1、画面静止），实际 presented={} dropped_late={}",
            st.presented,
            st.dropped_late
        );
        assert!(
            st.dropped_late >= 1,
            "被跳过的迟到帧仍应计入 dropped_late（如实记账），实际 {}",
            st.dropped_late
        );
    }

    #[test]
    fn next_deadline_targets_head_pts() {
        let mut p = player(
            &[(0, 0), (100, 1), (200, 2)],
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        assert!(p.next_deadline().is_none(), "尚未 poll：无时间轴，期限未知");
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ready(_)));
        assert_eq!(
            p.next_deadline(),
            Some(t0 + Duration::from_millis(100)),
            "期限应是队首帧的绝对到期时刻（epoch + pts）"
        );
        assert!(matches!(
            p.poll_frame(t0 + Duration::from_millis(100)).unwrap(),
            FramePoll::Ready(_)
        ));
        assert_eq!(p.next_deadline(), Some(t0 + Duration::from_millis(200)));
    }

    /// 唤醒总是迟 2ms 的调度器：门控不得把这份延迟累加成时间轴漂移。
    ///
    /// 回归 0.2.0-13 之前的真机现象：`上次呈现 + 1/fps` 让每帧的唤醒延迟单向
    /// 累加，队列稳态滞后从 0 漂到 100ms 容差，于是每几秒丢一帧（预读做足也丢）。
    #[test]
    fn fps_gate_does_not_accumulate_wakeup_lateness() {
        // 300 帧 60fps 素材：pts = 0, 16, 33, ...（毫秒取整）。
        let frames: Vec<(u64, u64)> = (0..300).map(|n| ((n * 1000) / 60, n)).collect();
        let mut p = player(
            &frames,
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                looping: false,
                fps_limit: Some(60.0),
                ..Default::default()
            },
        );
        let lateness = Duration::from_millis(2);
        let t0 = Instant::now();
        let mut now = t0;
        let mut presented: Vec<Instant> = Vec::new();
        while presented.len() < 300 {
            match p.poll_frame(now).unwrap() {
                FramePoll::Ready(_) => {
                    presented.push(now);
                    // 呈现之后按"下一次期限 + 固定延迟"再调用（真机主循环的简化）。
                    now = p.next_deadline().unwrap_or(now) + lateness;
                }
                FramePoll::Pending { next_in } => {
                    now += next_in.unwrap_or(Duration::from_millis(1)) + lateness;
                }
                other => panic!("未预期的轮询结果：{other:?}"),
            }
        }
        let stats = p.stats();
        assert_eq!(
            stats.presented, 300,
            "每一帧都必须呈现（不得靠丢帧追时间轴）"
        );
        assert_eq!(stats.dropped_late, 0, "迟 2ms 的唤醒不该造成丢帧");
        // 时间轴不漂：最后一帧的呈现时刻 ≈ epoch + pts（落在容差的一个小零头内）。
        let last_pts = Duration::from_millis((299 * 1000) / 60);
        let lag = presented
            .last()
            .unwrap()
            .saturating_duration_since(t0 + last_pts);
        assert!(
            lag < Duration::from_millis(20),
            "呈现应始终贴着媒体时间轴（实测滞后 {lag:?}）"
        );
    }

    #[test]
    fn next_deadline_honours_fps_limit_gate() {
        // 两个帧 pts 都为 0：门控把第二次呈现推迟到 last_present + 1/fps。
        let mut p = player(
            &[(0, 1), (0, 2)],
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                looping: false,
                fps_limit: Some(10.0),
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ready(_)));
        assert_eq!(
            p.next_deadline(),
            Some(t0 + Duration::from_millis(100)),
            "fps 门控应成为更晚的期限"
        );
    }

    /// 模拟真机 `GstProducer` 的源侧跳过：`pts < horizon` 的样本不转换直接跳过，
    /// 并上报 [`FrameProducer::skipped_frames`]（真机原先恒为 0，掩盖了"水位不生效"）。
    struct SkippingProducer {
        frames: Vec<(Duration, u64)>,
        pos: usize,
        horizon: Option<Duration>,
        skipped: u64,
        horizon_applied: u32,
    }

    impl SkippingProducer {
        fn new(frames: Vec<(Duration, u64)>) -> Self {
            Self {
                frames,
                pos: 0,
                horizon: None,
                skipped: 0,
                horizon_applied: 0,
            }
        }
    }

    impl FrameProducer for SkippingProducer {
        fn info(&self) -> MediaInfo {
            MediaInfo {
                kind: MediaKind::Video,
                width: 2,
                height: 2,
                duration: Some(Duration::from_secs(2)),
                source_fps: Some(60.0),
                codec: Some("skipping".into()),
            }
        }

        fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError> {
            while let Some(&(pts, n)) = self.frames.get(self.pos) {
                if let Some(h) = self.horizon {
                    if pts < h {
                        self.pos += 1;
                        self.skipped += 1;
                        continue;
                    }
                }
                self.pos += 1;
                let mock = MockProducer::new(2, 2, 1, Duration::ZERO);
                return Ok(VideoPoll::Frame {
                    pts,
                    frame: mock.make_frame(n),
                });
            }
            Ok(VideoPoll::Ended)
        }

        fn seek_start(&mut self) -> Result<(), MediaError> {
            self.pos = 0;
            Ok(())
        }

        fn decoder_kind(&self) -> DecoderKind {
            DecoderKind::Software
        }

        fn set_drop_horizon(&mut self, horizon: Option<Duration>) {
            if horizon.is_some() {
                self.horizon_applied += 1;
            }
            self.horizon = horizon;
        }

        fn skipped_frames(&self) -> u64 {
            self.skipped
        }
    }

    #[test]
    fn fps_gate_halves_presentation_rate_without_starvation() {
        // 60fps 源 + 30fps 输出上限：呈现速率应约为源的一半，且不得饿死。
        //
        // **2026-10-20 结论更新**：此前这里断言 `skipped == 0`，理由是容忍窗口
        // （100ms）大于门控间隔（33ms）⇒ 判不出"注定被丢"的帧 ⇒ fps 上限省不到
        // 下载/转换 CPU（真机实测一致）。用户反馈 4K120 素材仍解码全部源帧后修正：
        // 水位改为**按呈现排程**收紧到一个门周期 ⇒ 比一个门更老的帧在
        // `sample_to_frame` **之前**就被跳过 ⇒ `skipped > 0`，省掉每帧 12–33MB 搬运。
        let frames: Vec<(Duration, u64)> = (0..120u64)
            .map(|n| (Duration::from_micros(16_667 * n), n))
            .collect();
        let mut p = VideoPlayer::new(
            SkippingProducer::new(frames),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: Some(30.0),
                drop_late_tolerance: Duration::from_millis(100),
                looping: false,
            },
        )
        .unwrap();
        let mut now = Instant::now();
        for _ in 0..5_000 {
            match p.poll_frame(now).unwrap() {
                FramePoll::Ready(_) => {}
                FramePoll::Pending { next_in } => {
                    now += next_in.unwrap_or(Duration::from_millis(5));
                }
                FramePoll::Ended => break,
            }
        }
        let st = p.stats();
        assert!(
            (50..=70).contains(&st.presented),
            "30fps 上限下应呈现约一半帧，实际 {}",
            st.presented
        );
        assert!(
            st.skipped > 0,
            "按呈现排程收紧水位后，注定被丢的帧必须在**下载之前**跳过（真机 4K120 反馈的修复点）；\
             实际 skipped={}（若为 0 说明水位又退回'判不出'的旧行为）",
            st.skipped
        );
        assert!(
            st.presented + st.dropped_late <= 120,
            "三种去向之和不得超过总帧数"
        );
        // 水位在队列非空时确实下发过（虽然判不出"注定被丢"，机制本身是通的）。
        assert!(
            p.producer.horizon_applied > 0,
            "队列非空时应把水位下发给生产者"
        );
    }

    #[test]
    fn producer_side_skips_are_reported_in_stats() {
        // 真机缺陷：`VideoStats::skipped` 原先读的是播放器自己的恒 0 字段，
        // 生产者实际跳过多少帧外界看不到（掩盖了水位是否生效）。
        // 用 60fps 的帧间隔（16.6ms < 容忍 100ms）才可能出现"源侧跳过"：
        // 队列里已有帧 → 水位下发 → 生产者把水位以下的样本直接跳掉。
        let frames: Vec<(Duration, u64)> = (0..60u64)
            .map(|n| (Duration::from_micros(16_667 * n), n))
            .collect();
        let mut p = VideoPlayer::new(
            SkippingProducer::new(frames),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::from_millis(100),
                looping: false,
            },
        )
        .unwrap();
        let t0 = Instant::now();
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ready(_)));
        assert_eq!(p.stats().skipped, 0, "刚开始没有可跳过的帧");
        // t0+200ms：媒体时间已跑过队首 100ms 容忍，水位把 50/66/83ms 三帧
        // 在**不做像素转换**的前提下跳过 —— 计数必须出现在 stats 里。
        assert!(matches!(
            p.poll_frame(t0 + Duration::from_millis(200)).unwrap(),
            FramePoll::Ready(_)
        ));
        let st = p.stats();
        assert!(
            st.skipped >= 1,
            "生产者上报的跳过数必须出现在 stats 里，实际 {}",
            st.skipped
        );
        assert_eq!(st.presented, 2);
    }

    #[test]
    fn no_fps_limit_keeps_horizon_conservative() {
        // 无 fps 上限时不得凭空下发水位（避免把还要呈现的帧提前跳过）。
        let frames: Vec<(Duration, u64)> = (0..20u64)
            .map(|n| (Duration::from_millis(50 * n), n))
            .collect();
        let mut p = VideoPlayer::new(
            SkippingProducer::new(frames),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::from_millis(100),
                looping: false,
            },
        )
        .unwrap();
        let t0 = Instant::now();
        let mut presented = 0;
        for n in 0..20u64 {
            if matches!(
                p.poll_frame(t0 + Duration::from_millis(50 * n)).unwrap(),
                FramePoll::Ready(_)
            ) {
                presented += 1;
            }
        }
        assert_eq!(presented, 20, "无门控时每帧都应呈现");
        assert_eq!(p.stats().skipped, 0, "无门控时不得跳过任何帧");
    }

    #[test]
    fn active_producer_with_empty_queue_gets_bounded_deadline() {
        // 生产者暂无数据但仍在供帧：必须给**有界**短期限。
        // 返回"无期限"会让上层退化成长兜底休眠 —— 视频停住的同一个机理。
        struct EmptyProducer;
        impl FrameProducer for EmptyProducer {
            fn info(&self) -> MediaInfo {
                MediaInfo {
                    kind: MediaKind::Video,
                    width: 2,
                    height: 2,
                    duration: None,
                    source_fps: Some(60.0),
                    codec: Some("empty".into()),
                }
            }
            fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError> {
                Ok(VideoPoll::NeedMore)
            }
            fn seek_start(&mut self) -> Result<(), MediaError> {
                Ok(())
            }
            fn decoder_kind(&self) -> DecoderKind {
                DecoderKind::Mock
            }
        }
        let mut p = VideoPlayer::new(EmptyProducer, VideoOptions::default()).unwrap();
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Pending {
                next_in: Some(next_in),
            } => {
                assert!(next_in > Duration::ZERO && next_in <= Duration::from_millis(10));
                assert_eq!(next_in, DECODE_PROBE);
            }
            other => panic!("活跃生产者不得返回无期限 Pending，实际 {other:?}"),
        }
        // 队列为空时"下一帧到期时刻"确实未知，但上层仍有回探期限可用。
        assert!(p.next_deadline().is_none());
        // 真正"等外部事件"的语义只在暂停时出现。
        p.pause().unwrap();
        assert!(matches!(
            p.poll_frame(t0).unwrap(),
            FramePoll::Pending { next_in: None }
        ));
    }

    /// seek 后需要回填若干次的生产者（模拟真机 GStreamer FLUSH seek 的回填延迟）。
    struct SeekLagProducer {
        frames: Vec<(Duration, u64)>,
        pos: usize,
        lag: u32,
        pending: u32,
    }

    impl SeekLagProducer {
        fn new(frames: &[(u64, u64)], lag: u32) -> Self {
            Self {
                frames: frames
                    .iter()
                    .map(|(ms, n)| (Duration::from_millis(*ms), *n))
                    .collect(),
                pos: 0,
                lag,
                pending: 0,
            }
        }
    }

    impl FrameProducer for SeekLagProducer {
        fn info(&self) -> MediaInfo {
            MediaInfo {
                kind: MediaKind::Video,
                width: 2,
                height: 2,
                duration: None,
                source_fps: Some(20.0),
                codec: Some("seek-lag".into()),
            }
        }

        fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError> {
            if self.pending > 0 {
                self.pending -= 1;
                return Ok(VideoPoll::NeedMore);
            }
            if self.pos >= self.frames.len() {
                return Ok(VideoPoll::Ended);
            }
            let (pts, n) = self.frames[self.pos];
            self.pos += 1;
            let mock = MockProducer::new(2, 2, 1, Duration::ZERO);
            Ok(VideoPoll::Frame {
                pts,
                frame: mock.make_frame(n),
            })
        }

        fn seek_start(&mut self) -> Result<(), MediaError> {
            self.pos = 0;
            self.pending = self.lag;
            Ok(())
        }

        fn decoder_kind(&self) -> DecoderKind {
            DecoderKind::Software
        }
    }

    #[test]
    fn looping_restart_reanchors_on_first_frame_after_seek_lag() {
        let mut p = VideoPlayer::new(
            SeekLagProducer::new(&[(0, 0), (50, 1)], 2),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::from_millis(100),
                looping: true,
            },
        )
        .unwrap();
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 0),
            other => panic!("期望 Ready(0)，实际 {other:?}"),
        }
        match p.poll_frame(t0 + Duration::from_millis(50)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 1),
            other => panic!("期望 Ready(1)，实际 {other:?}"),
        }
        // 第一遍结束：走循环重播（seek 已发出，样本要等回填）。
        match p.poll_frame(t0 + Duration::from_millis(100)).unwrap() {
            FramePoll::Pending { next_in } => {
                assert_eq!(next_in, Some(DECODE_PROBE), "回填窗口必须给出有界期限");
            }
            other => panic!("期望 Pending(Some)，实际 {other:?}"),
        }
        // 最坏情况：调用方整整 1 秒后才回来（真机旧实现就是 1s 兜底）。
        let late = t0 + Duration::from_millis(1100);
        let mut first_of_second_pass = None;
        for _ in 0..4 {
            match p.poll_frame(late).unwrap() {
                FramePoll::Ready(f) => {
                    first_of_second_pass = Some(frame_num(&f));
                    break;
                }
                FramePoll::Pending { .. } => continue,
                other => panic!("期望 Ready/Pending，实际 {other:?}"),
            }
        }
        assert_eq!(
            first_of_second_pass,
            Some(0),
            "循环重播后必须重新播放第 0 帧"
        );
        let stats = p.stats();
        assert_eq!(stats.presented, 3, "第二轮首帧必须被呈现");
        assert_eq!(
            stats.dropped_late, 0,
            "seek 回填耗时不得被算作迟到丢弃（时间轴应在重播首帧处重锚）"
        );
    }

    #[test]
    fn queue_never_exceeds_bound() {
        let mut p = player(
            &[(0, 0), (10, 1), (20, 2), (30, 3), (40, 4), (50, 5)],
            VideoOptions {
                lag_frames: 0,
                queue_size: 2,
                looping: false,
                drop_late_tolerance: Duration::ZERO,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        // 连续 poll：队列长度永远 ≤ 2。
        for i in 0..8 {
            let _ = p.poll_frame(t0 + Duration::from_millis(i * 5)).unwrap();
            assert!(p.stats().queue_len <= 2, "第 {i} 轮队列超上限");
        }
    }

    #[test]
    fn present_due_frame_in_pts_order() {
        let mut p = player(
            &[(0, 10), (200, 20), (400, 30)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 10),
            other => panic!("期望 Ready，实际 {other:?}"),
        }
        // 尚未到期：Pending 并给出 next_in。
        match p.poll_frame(t0 + Duration::from_millis(50)).unwrap() {
            FramePoll::Pending {
                next_in: Some(next_in),
            } => {
                assert!(next_in > Duration::ZERO);
                assert!(next_in <= Duration::from_millis(200));
            }
            other => panic!("期望 Pending(Some)，实际 {other:?}"),
        }
        match p.poll_frame(t0 + Duration::from_millis(250)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 20),
            other => panic!("期望 Ready，实际 {other:?}"),
        }
        let stats = p.stats();
        assert_eq!(stats.presented, 2);
        assert_eq!(stats.last_pts, Some(Duration::from_millis(200)));
    }

    #[test]
    fn late_frames_are_dropped_and_counted() {
        // pts 200/300/400ms，容忍 100ms；先在 t0 锚定 epoch，
        // 再于 t0+1s 拉取：全部过期丢弃。
        let mut p = player(
            &[(200, 1), (300, 2), (400, 3)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        assert!(matches!(
            p.poll_frame(t0).unwrap(),
            FramePoll::Pending { .. }
        ));
        // t0+1s：三帧全部过期丢弃（单轮即可把已入队的三帧全部判过期）。
        let late = t0 + Duration::from_secs(1);
        let mut rounds = 0;
        let mut ended = false;
        while rounds < 16 {
            rounds += 1;
            match p.poll_frame(late).unwrap() {
                FramePoll::Ended => {
                    ended = true;
                    break;
                }
                FramePoll::Pending { .. } => {}
                other => panic!("不应呈现已过期帧: {other:?}"),
            }
        }
        let stats = p.stats();
        assert_eq!(stats.dropped_late, 3, "三帧应全部按过期丢弃");
        assert_eq!(stats.presented, 0, "过期帧不得呈现");
        assert!(ended, "流末尾应在有限轮次内上报 Ended");
    }

    #[test]
    fn mixed_drop_and_present() {
        // pts 200/500/1500ms，容忍 100ms；t0 锚定 epoch，
        // t0+600ms：丢 200、呈 500、1500 未到期。
        let mut p = player(
            &[(200, 1), (500, 2), (1500, 3)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        assert!(matches!(
            p.poll_frame(t0).unwrap(),
            FramePoll::Pending { .. }
        ));
        match p.poll_frame(t0 + Duration::from_millis(600)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 2),
            other => panic!("期望 Ready(2)，实际 {other:?}"),
        }
        let stats = p.stats();
        assert_eq!(stats.dropped_late, 1);
        assert_eq!(stats.presented, 1);
    }

    #[test]
    fn fps_limit_defers_presentation() {
        // 两个帧 pts 都为 0（同时到期），fps_limit=10 → 最小间隔 100ms。
        let mut p = player(
            &[(0, 1), (0, 2)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                fps_limit: Some(10.0),
                // 容忍调大，确保门控延迟不会被误判为迟到丢帧。
                drop_late_tolerance: Duration::from_secs(1),
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 1),
            other => panic!("期望 Ready(1)，实际 {other:?}"),
        }
        // 门控未开：Pending，且提示 100ms 内。
        match p.poll_frame(t0 + Duration::from_millis(10)).unwrap() {
            FramePoll::Pending {
                next_in: Some(next_in),
            } => {
                assert!(next_in <= Duration::from_millis(100));
                assert!(next_in > Duration::ZERO);
            }
            other => panic!("期望 Pending(Some)，实际 {other:?}"),
        }
        assert_eq!(p.stats().presented, 1, "门控期间不得出队");
        match p.poll_frame(t0 + Duration::from_millis(150)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 2),
            other => panic!("期望 Ready(2)，实际 {other:?}"),
        }
    }

    /// 降载：运行期将输出上限从 10fps 压到 1fps 后，同一时刻原本能出的帧必须被门控挡住。
    ///
    /// 这条守的是 `performance.reduce_load_on_cpu_pressure` 的**执行机构**：
    /// 上限只在构造时生效的话，服务器压力升高时显示侧照样满速提交，配置就是空的。
    #[test]
    fn fps_cap_applies_at_runtime() {
        let mut p = player(
            &[(0, 1), (0, 2)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                fps_limit: Some(10.0),
                // 容忍放到 5s：本测试只验门控，不想让第二帧被当成"迟到"丢掉。
                drop_late_tolerance: Duration::from_secs(5),
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        assert!(
            matches!(p.poll_frame(t0).unwrap(), FramePoll::Ready(_)),
            "第一帧应立即可呈现（作为门控基线）"
        );
        p.set_fps_cap(Some(1.0));
        assert_eq!(p.options().fps_limit, Some(1.0), "上限必须真的写进调度选项");
        // 10fps 时 150ms 早已开门（100ms）；压到 1fps 后应还差约 850ms。
        match p.poll_frame(t0 + Duration::from_millis(150)).unwrap() {
            FramePoll::Pending {
                next_in: Some(next_in),
            } => {
                assert!(
                    next_in > Duration::from_millis(500),
                    "1fps 门控的剩余等待应远大于原 10fps 周期，实际 {next_in:?}"
                );
            }
            other => panic!("期望降载后被门控挡住（Pending），实际 {other:?}"),
        }
        assert_eq!(p.stats().presented, 1, "降载期间不得多出帧");
        match p.poll_frame(t0 + Duration::from_millis(1_100)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 2),
            other => panic!("门控到期后应恢复出帧，实际 {other:?}"),
        }
    }

    #[test]
    fn offer_frame_rejects_when_queue_full() {
        let mut p = player(
            &[],
            VideoOptions {
                lag_frames: 0,
                queue_size: 2,
                looping: false,
                ..Default::default()
            },
        );
        let frame = MockProducer::new(2, 2, 1, Duration::ZERO).make_frame(7);
        p.offer_frame(Duration::ZERO, frame.clone());
        p.offer_frame(Duration::from_millis(10), frame.clone());
        p.offer_frame(Duration::from_millis(20), frame);
        let stats = p.stats();
        assert_eq!(stats.queue_len, 2);
        assert_eq!(stats.dropped_queue_full, 1);
    }

    // ---- 暂停/恢复 ----

    #[test]
    fn pause_resume_shifts_epoch_without_mass_drops() {
        let mut p = player(
            &[(0, 0), (500, 1), (1000, 2)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 0),
            other => panic!("期望 Ready(0)，实际 {other:?}"),
        }
        p.pause_at(t0 + Duration::from_millis(10)).unwrap();
        assert!(matches!(
            p.poll_frame(t0 + Duration::from_millis(20)).unwrap(),
            FramePoll::Pending { next_in: None }
        ));
        // 暂停 1s 后恢复：epoch 后移 1s，媒体时间回到 ~10ms。
        p.resume_at(t0 + Duration::from_millis(1010)).unwrap();
        match p.poll_frame(t0 + Duration::from_millis(1010)).unwrap() {
            FramePoll::Pending {
                next_in: Some(next_in),
            } => {
                // pts=500ms 的帧未到期，且不应被视为过期。
                assert!(next_in > Duration::from_millis(200));
            }
            other => panic!("期望 Pending(Some)，实际 {other:?}"),
        }
        assert_eq!(p.stats().dropped_late, 0, "恢复后不应疯狂丢帧");
        match p.poll_frame(t0 + Duration::from_millis(1510)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 1),
            other => panic!("期望 Ready(1)，实际 {other:?}"),
        }
    }

    #[test]
    fn pause_stops_pulling_and_state_errors() {
        let mut p = player(&[(0, 0), (100, 1)], VideoOptions::default());
        p.pause().unwrap();
        let before = p.stats().queue_len;
        assert!(matches!(
            p.poll_frame(Instant::now()).unwrap(),
            FramePoll::Pending { next_in: None }
        ));
        assert_eq!(p.stats().queue_len, before, "暂停期间不得拉帧");
        assert!(matches!(p.pause(), Err(MediaError::InvalidState(_))));
        p.resume().unwrap();
        assert!(matches!(p.resume(), Err(MediaError::InvalidState(_))));
    }

    // ---- 循环 / 结束 ----

    #[test]
    fn looping_seeks_back_to_start() {
        let mut p = player(
            &[(0, 1), (100, 2)],
            VideoOptions {
                lag_frames: 0,
                looping: true,
                drop_late_tolerance: Duration::ZERO,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        match p.poll_frame(t0).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 1),
            other => panic!("期望 Ready(1)，实际 {other:?}"),
        }
        match p.poll_frame(t0 + Duration::from_millis(100)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 2),
            other => panic!("期望 Ready(2)，实际 {other:?}"),
        }
        // 队列空 + Ended + looping → seek 重播第一帧。
        match p.poll_frame(t0 + Duration::from_millis(200)).unwrap() {
            FramePoll::Ready(f) => assert_eq!(frame_num(&f), 1),
            other => panic!("期望循环后 Ready(1)，实际 {other:?}"),
        }
        assert_eq!(p.producer.seek_count(), 1, "循环重播应调用一次 seek_start");
    }

    #[test]
    fn non_looping_reports_ended() {
        let mut p = player(
            &[(0, 1)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ready(_)));
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ended));
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ended));
        assert_eq!(p.stats().presented, 1);
    }

    // ---- 端到端 / 汇总 trait ----

    #[test]
    fn mock_end_to_end_media_source_flow() {
        let producer = MockProducer::new(8, 4, 5, Duration::from_millis(100));
        let mut p = VideoPlayer::new(
            producer,
            VideoOptions {
                lag_frames: 0,
                looping: false,
                drop_late_tolerance: Duration::ZERO,
                ..Default::default()
            },
        )
        .unwrap();
        let meta = MediaSource::metadata(&p);
        assert_eq!(meta.width, 8);
        assert_eq!(meta.height, 4);
        let t0 = Instant::now();
        for n in 0..5u64 {
            let now = t0 + Duration::from_millis(n * 100);
            match p.poll_frame(now).unwrap() {
                FramePoll::Ready(f) => {
                    assert_eq!(frame_num(&f), n);
                    assert_eq!(f.width(), 8);
                    assert_eq!(f.kind(), FrameKind::Cpu);
                }
                other => panic!("第 {n} 帧期望 Ready，实际 {other:?}"),
            }
        }
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ended));
        let stats = VideoSource::video_stats(&p);
        assert_eq!(stats.presented, 5);
        assert_eq!(stats.dropped_late, 0);
        assert_eq!(stats.decoder, DecoderKind::Mock);
        assert_eq!(stats.source_fps, Some(10.0));
        assert_eq!(stats.last_pts, Some(Duration::from_millis(400)));
    }

    #[test]
    fn video_source_is_object_safe() {
        let producer = MockProducer::new(2, 2, 3, Duration::from_millis(10));
        let player = VideoPlayer::new(producer, VideoOptions::default()).unwrap();
        let mut boxed: Box<dyn VideoSource + Send> = Box::new(player);
        let t0 = Instant::now();
        assert!(matches!(boxed.poll_frame(t0).unwrap(), FramePoll::Ready(_)));
        let stats = boxed.video_stats();
        assert_eq!(stats.presented, 1);
        assert_eq!(stats.decoder, DecoderKind::Mock);
        // daemon 只持有 `Box<dyn VideoSource + Send>`：降载执行机构必须在这条对象安全
        // 的路径上真能被调到（编译期即验证，运行期不报错）。
        boxed.set_fps_cap(Some(1.0));
        assert!(
            matches!(
                boxed.poll_frame(Instant::now()).unwrap(),
                FramePoll::Ready(_) | FramePoll::Pending { .. }
            ),
            "限速后仍可正常推进"
        );
    }

    #[test]
    fn zero_interval_presents_all_then_ends() {
        let mut p = player(
            &[(0, 1), (0, 2), (0, 3)],
            VideoOptions {
                lag_frames: 0,
                looping: false,
                drop_late_tolerance: Duration::ZERO,
                ..Default::default()
            },
        );
        let t0 = Instant::now();
        for n in [1u64, 2, 3] {
            match p.poll_frame(t0).unwrap() {
                FramePoll::Ready(f) => assert_eq!(frame_num(&f), n),
                other => panic!("期望 Ready({n})，实际 {other:?}"),
            }
        }
        assert!(matches!(p.poll_frame(t0).unwrap(), FramePoll::Ended));
    }

    #[test]
    fn stats_report_decoder_and_pts() {
        // t49：这条断言的是**调度语义**（到期即交付），故显式关掉滞后启发式
        // （`lag_frames: 0`），其断言语义与改动前完全一致——不是放宽，而是把
        // "滞后"这条新行为留给专门的测试去覆盖。
        let mut p = player(
            &[(0, 9)],
            VideoOptions {
                lag_frames: 0,
                ..VideoOptions::default()
            },
        );
        let t0 = Instant::now();
        p.poll_frame(t0).unwrap();
        let stats = p.stats();
        assert_eq!(stats.decoder, DecoderKind::Software);
        assert_eq!(stats.last_pts, Some(Duration::ZERO));
        assert!(!stats.decoder.is_hardware());
        assert!(DecoderKind::Vaapi.is_hardware());
    }
    /// 只产一遍帧；`seek_start` 之后**永不再产帧**（直接 Ended）——
    /// 模拟真机"播放中文件被删/截断/网络盘掉线"后的 seek。
    struct OnePassProducer {
        frames: Vec<(Duration, u64)>,
        pos: usize,
        allow: bool,
    }

    impl OnePassProducer {
        fn new(frames: &[(u64, u64)]) -> Self {
            Self {
                frames: frames
                    .iter()
                    .map(|(ms, n)| (Duration::from_millis(*ms), *n))
                    .collect(),
                pos: 0,
                allow: true,
            }
        }
    }

    impl FrameProducer for OnePassProducer {
        fn info(&self) -> MediaInfo {
            MediaInfo {
                kind: MediaKind::Video,
                width: 2,
                height: 2,
                duration: Some(Duration::from_millis(60)),
                source_fps: Some(60.0),
                codec: Some("one-pass".into()),
            }
        }

        fn next_producer_frame(&mut self) -> Result<VideoPoll, MediaError> {
            if !self.allow || self.pos >= self.frames.len() {
                return Ok(VideoPoll::Ended);
            }
            let (pts, n) = self.frames[self.pos];
            self.pos += 1;
            let mock = MockProducer::new(2, 2, 1, Duration::ZERO);
            Ok(VideoPoll::Frame {
                pts,
                frame: mock.make_frame(n),
            })
        }

        fn seek_start(&mut self) -> Result<(), MediaError> {
            // seek 报告成功，但之后再也不会产出任何帧（文件已被截断/删除）。
            self.pos = 0;
            self.allow = false;
            Ok(())
        }

        fn decoder_kind(&self) -> DecoderKind {
            DecoderKind::Software
        }
    }

    /// F1（t6 回归）：**重启后永不产帧必须有限收敛**。
    ///
    /// 真机场景：播放中文件被截断/删除 → EOS → seek 成功但无数据 →
    /// 旧实现每次醒来都 EOS+seek（实测 244 次/秒、CPU 恒 ~8% 单核、
    /// `presented` 冻结且零 WARN）。契约=有限收敛。
    #[test]
    fn restart_without_frames_converges_to_error_instead_of_busy_retrying() {
        let mut p = VideoPlayer::new(
            OnePassProducer::new(&[(0, 0), (20, 1), (40, 2)]),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::from_millis(100),
                looping: true,
            },
        )
        .unwrap();
        let mut now = Instant::now();
        let mut presented = 0u32;
        let mut polls = 0u32;
        let mut err = None;
        for _ in 0..500 {
            polls += 1;
            match p.poll_frame(now) {
                Ok(FramePoll::Ready(_)) => presented += 1,
                Ok(FramePoll::Pending { next_in }) => {
                    now += next_in.unwrap_or(Duration::from_millis(5));
                }
                Ok(FramePoll::Ended) => panic!(
                    "停摆必须以上抛错误收敛（交给上层错误路径降级）；返回 Ended 会让上层 \
                     保持 VideoPlaying 却永不推进（第 {polls} 次 poll）"
                ),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("有限次数内必须上抛停摆错误，而不是无限 Pending/seek");
        assert!(
            err.to_string().contains("停摆"),
            "错误信息应说明停摆原因: {err}"
        );
        assert!(polls <= 30, "必须有限收敛（实际 {polls} 次 poll；上限 30）");
        assert_eq!(presented, 3, "第一遍的 3 帧仍应正常呈现");
    }

    /// 边界用例 1（core-auditor 指定）：**慢 seek 不得被误判为停摆**。
    ///
    /// seek 后需要 30 次 `NeedMore` 才回填（真机 4K seek 回填可达数百毫秒）：
    /// 只要最终产帧，就该继续循环播放。
    #[test]
    fn slow_seek_filling_is_not_misjudged_as_stall() {
        let mut p = VideoPlayer::new(
            SeekLagProducer::new(&[(0, 0), (50, 1), (100, 2)], 30),
            VideoOptions {
                lag_frames: 0,
                queue_size: 3,
                fps_limit: None,
                drop_late_tolerance: Duration::from_millis(100),
                looping: true,
            },
        )
        .unwrap();
        let mut now = Instant::now();
        let mut presented = 0u32;
        for _ in 0..600 {
            match p.poll_frame(now) {
                Ok(FramePoll::Ready(_)) => presented += 1,
                Ok(FramePoll::Pending { next_in }) => {
                    now += next_in.unwrap_or(Duration::from_millis(5));
                }
                Ok(FramePoll::Ended) => break,
                Err(e) => panic!("慢 seek 不得判停摆: {e}"),
            }
            if presented >= 9 {
                break;
            }
        }
        assert!(
            presented >= 9,
            "慢 seek 下必须持续循环播放（实际 {presented} 帧）"
        );
    }
}
