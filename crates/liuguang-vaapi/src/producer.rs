//! [`FrameProducer`] 实现：解码 → 导出 → 每帧一个多 fd 的 [`DmaBufFrame`]。
//!
//! # 两层结构
//!
//! 1. [`SyncPipeline`]：真正干活的同步链（`next_surface` → 导出 → 可选 RGB 转换）。
//!    它不知道线程的存在：回退路径直接调用它，单测也只需要它。
//! 2. **解码预读线程**（[`Prefetch`]）：独占解码器与转换器，循环"解一帧 → 推进
//!    有界通道"，把解码/转换与呈现彻底解耦。`next_producer_frame` 只做
//!    `try_recv`，永不阻塞。
//!
//! 为什么要这样：4K60 解码一帧（NVDEC + 导出 dma-buf + NV12→XRGB8888 + `glFinish`）
//! 是毫秒级，而呈现节拍是 16.67ms。同步链上"拉一帧 → 等解码 → 等转换"意味着
//! 任何一次抖动都直接落在呈现关键路径上，队列永远只有 0–1 帧，媒体时钟一跑过
//! deadline 就按设计丢帧（真机实测 58.2fps、`dropped_late` ≈ 2/s）。把解码挪到
//! 独立线程、留 3 帧预读之后，呈现侧只剩"从队列取帧"。
//!
//! 线程起不来（`thread::spawn` 失败）时退回同步路径：功能不变，只是少了预读。
//!
//! # 帧的寿命
//!
//! 解码表面（`AVFrame` 引用）被塞进 `FrameKeepalive`，随
//! `wl_buffer.release` 一起释放；RGB 路径的 keepalive 持有输出槽位（BO）。
//! 合成器还没归还缓冲就复用同一块表面会产生撕裂/错帧，所以宁可让上层看到
//! `NeedMore`（背压）也不能提前放表面。
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use liuguang_core::transform::RenderTransform;
use liuguang_core::types::{DisplayFrame, FrameKeepalive, MediaInfo, MediaKind};
use liuguang_media::video::{DecoderKind, DmabufState, FrameProducer, VideoPoll};

use crate::{DecodedSurface, RgbConverter, StreamInfo, VaapiDecoder, VaapiError, export};

/// 未归还的表面超过这个数量就暂停解码（背压）。
///
/// 这个数必须**大于**「合成器攥着的缓冲数 + 播放器队列深度」，否则会死锁：
/// 真机实测（niri + NVIDIA，4K60）合成器同时攥 2 个缓冲不 release，播放器队列还有
/// 2 帧待提交；上限设 3 时解到第 3 帧就再也不放行，而合成器要等新帧才释放旧的 ⇒
/// 双方互等，10s 后被"停摆"看门狗判死、壁纸退回静态。
///
/// 6 = 合成器 2 + 队列 2 + 1 帧余量（再多只是白占内存：4K NV12 每帧约 24MB）。
const MAX_INFLIGHT: usize = 6;

/// 预读深度默认值（有界通道容量 = 预读帧数）。
// 深度 2：预读只保留"正在解码的一帧 + 已转好待取的一帧"，足以覆盖单帧解码
// 抖动（4K60 实测仍稳定 60fps），同时比 3 少常驻一个输出缓冲（2560×1600 ≈ 16.4 MB）。
const DEFAULT_PREFETCH: usize = 2;

/// 预读深度允许范围（内存：每帧 4K RGB 输出 BO 约 33MB）。
const MIN_PREFETCH: usize = 1;
/// 见 [`MIN_PREFETCH`]。
const MAX_PREFETCH: usize = 4;

/// 覆盖预读深度的环境变量（`1..=4`；`0` = 关闭预读退回同步路径，非法值退回默认）。
const PREFETCH_ENV: &str = "LIUGUANG_VIDEO_PREFETCH";

/// 轮询粒度：暂停/背压/通道满都按这个间隔 park。
///
/// 用 `park_timeout` 而不是忙等：暂停时 CPU 必须 ≈0（硬指标 ≤1%），
/// 1ms 的粒度为「暂停后 200ms 内停止出帧」留了足够余量。
const IDLE_POLL: Duration = Duration::from_millis(1);

/// 背压期间的轮询粒度（见 `PrefetchStep::Throttled` 分支）。
const THROTTLE_POLL: Duration = Duration::from_millis(2);

/// 暂停期间的轮询粒度（比 [`IDLE_POLL`] 粗得多）。
///
/// 暂停时线程是"纯等待"：1ms 的粒度会白烧 ~1.4% 单核（1000 次/s 的定时 park），
/// 而"暂停时 CPU ≈0"是硬指标。20ms 只换来 50 次/s 的唤醒（≈0.07%），
/// 恢复是由 [`PrefetchControl::resume`] 的 `unpark` 直接叫醒的，不受这个粒度影响。
const PAUSE_POLL: Duration = Duration::from_millis(20);

/// 预读深度：默认 [`DEFAULT_PREFETCH`]，可用 [`PREFETCH_ENV`] 覆盖。
fn prefetch_depth() -> usize {
    let Ok(raw) = std::env::var(PREFETCH_ENV) else {
        return DEFAULT_PREFETCH;
    };
    match raw.trim().parse::<usize>() {
        // `0` 是显式的"关掉预读"开关（现场回退 / A-B 对比），不是非法值。
        Ok(0) => 0,
        Ok(value) if (MIN_PREFETCH..=MAX_PREFETCH).contains(&value) => value,
        Ok(value) => {
            tracing::warn!(
                env = PREFETCH_ENV,
                value,
                min = MIN_PREFETCH,
                max = MAX_PREFETCH,
                default = DEFAULT_PREFETCH,
                "预读深度超出允许范围，改用默认值"
            );
            DEFAULT_PREFETCH
        }
        Err(error) => {
            tracing::warn!(
                env = PREFETCH_ENV,
                value = %raw,
                %error,
                default = DEFAULT_PREFETCH,
                "预读深度不是整数，改用默认值"
            );
            DEFAULT_PREFETCH
        }
    }
}

/// 一帧已解码/转换完成。
struct Produced {
    pts: Duration,
    frame: DisplayFrame,
    /// 逐帧回读的像素（仅诊断探针开启时）。
    capture: Option<Capture>,
}

/// [`SyncPipeline::step`] 的结果。
enum StepOutcome {
    /// 产出一帧。
    Frame(Produced),
    /// 暂时没条件出帧（输出槽位/表面未归还）：退避后重试。
    Throttled,
    /// 流到头（要 seek 回起点才能继续）。
    EndOfSegment,
}

/// 逐帧回读的像素（仅诊断探针：`examples/rgb_convert_probe.rs`）。
struct Capture {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// 同步解码链：解一帧 → 导出 dma-buf →（可选）GPU 转成 XRGB8888。
///
/// 没有线程概念：既被生产者直接调用（回退路径），也被预读线程独占调用。
struct SyncPipeline {
    decoder: VaapiDecoder,
    /// `Some` 时每帧先经 GPU 转成 XRGB8888 再交出去（[`VaapiProducer::open_rgb`]）。
    converter: Option<RgbConverter>,
    /// 要建 RGB 转换器时用的渲染节点；`None` = 不做转换（纯 DMA-BUF 直通）。
    ///
    /// 转换器**懒建**：必须等第一帧拿到解码表面的分配尺寸（1080p 的 NV12 表面
    /// 是 1920x1088），按码流的 1920x1080 去建会被尺寸校验挡下——真机上就表现
    /// 成"除 4K 之外的视频全部回落到 CPU 合成"。
    rgb_node: Option<PathBuf>,
    /// 码流的**显示**尺寸（可见区域），与解码表面的分配尺寸可能不同。
    visible: (u32, u32),
    /// 期望的 RGB 输出尺寸（显示器物理像素）：建转换器时直接按此分配，避免首帧后再重建。
    output_size: Option<(u32, u32)>,
    /// 实时几何与亮度参数（仅 RGB 路径由 GPU 着色器直接消费）。
    render_transform: RenderTransform,
    /// 导出帧计数（与生产者共享：预读线程会继续加）。
    exported: Arc<AtomicU64>,
    /// 未归还的解码表面数（非 RGB 路径的背压来源）。
    surface_inflight: Arc<AtomicUsize>,
}

impl SyncPipeline {
    /// 尝试推进一帧。
    ///
    /// **会阻塞**在解码/导出/转换上（毫秒级）；预读线程里调用它没问题，
    /// 同步回退路径上就是原来的行为。
    fn step(&mut self, capture: bool) -> Result<StepOutcome, VaapiError> {
        let exported_so_far = self.exported.load(Ordering::Relaxed);
        if let Some(converter) = &self.converter {
            let inflight = converter.inflight();
            let slots = converter.slots();
            if inflight >= slots {
                // RGB 背压：输出槽位还被合成器占着，转换无处落笔。
                tracing::trace!(
                    inflight,
                    slots,
                    exported = exported_so_far,
                    "RGB 输出缓冲未归还，暂缓解码"
                );
                return Ok(StepOutcome::Throttled);
            }
        } else if self.rgb_node.is_none() {
            // 纯 DMA-BUF 直通：按解码表面占用做背压。RGB 路径此时转换器还没建
            // （等第一帧的分配尺寸），槽位压力由建好之后的那个分支接管。
            let inflight = self.surface_inflight.load(Ordering::Relaxed);
            if inflight >= MAX_INFLIGHT {
                // 背压：合成器还没归还前面的缓冲。继续解码只会挤占表面池。
                tracing::trace!(inflight, exported = exported_so_far, "表面未归还，暂缓解码");
                return Ok(StepOutcome::Throttled);
            }
        }
        let Some(surface) = self.decoder.next_surface()? else {
            tracing::debug!(exported = exported_so_far, "解码器已到流末尾");
            return Ok(StepOutcome::EndOfSegment);
        };
        let pts = surface.pts;
        let exported_surface =
            export::export_surface(self.decoder.display(), surface.surface_id())?;
        self.ensure_converter(exported_surface.width, exported_surface.height)?;
        if let Some(converter) = self.converter.as_mut() {
            // 转换路径：NV12 只作为采样源，表面不需要吊在 keepalive 里。
            let nv12 = exported_surface.into_frame(None)?;
            let rgb = match converter.convert(&nv12) {
                Ok(frame) => frame,
                Err(VaapiError::Busy) => {
                    // 正常不会走到：上面已经确认有空闲槽位，而槽位只减不增。
                    // 真撞上了就把这帧丢掉让调用方重试，不要死等。
                    tracing::warn!(
                        inflight = converter.inflight(),
                        "RGB 转换缓冲刚好被占满，丢弃本帧"
                    );
                    return Ok(StepOutcome::Throttled);
                }
                Err(error) => return Err(error),
            };
            let converted = converter.converted();
            // 回读必须在下一帧画进同一个槽位之前做；`convert` 里的 glFinish 已经
            // 保证像素就绪，这里读的是刚画好的那一帧。
            let captured = if capture {
                converter
                    .readback_last_xrgb()
                    .ok()
                    .map(|(width, height, rgba)| Capture {
                        width,
                        height,
                        rgba,
                    })
            } else {
                None
            };
            // `convert` 里的 glFinish 已经保证采样完成，解码表面现在就能还给 NVDEC。
            drop(nv12);
            drop(surface);
            let exported_total = self.exported.fetch_add(1, Ordering::Relaxed) + 1;
            if exported_total == 1 {
                tracing::info!(
                    fourcc = format!("0x{:08X}", rgb.fourcc),
                    modifier = format!("0x{:016X}", rgb.modifier),
                    stride = rgb.stride,
                    width = rgb.width,
                    height = rgb.height,
                    converted,
                    "首个 RGB 转换帧已导出（NV12 → XRGB8888）"
                );
            }
            return Ok(StepOutcome::Frame(Produced {
                pts,
                frame: DisplayFrame::DmaBuf(rgb),
                capture: captured,
            }));
        }
        let fourcc = exported_surface.fourcc;
        let modifier = exported_surface
            .objects
            .first()
            .map_or(0, |(_, modifier)| *modifier);
        // 先增加计数再交出去：keepalive 一被 drop 就减回去。
        self.surface_inflight.fetch_add(1, Ordering::Relaxed);
        let guard = SurfaceGuard {
            _surface: surface,
            inflight: Arc::clone(&self.surface_inflight),
        };
        let keepalive = Some(FrameKeepalive::new(guard));
        let frame = exported_surface.into_frame(keepalive)?;
        let exported_total = self.exported.fetch_add(1, Ordering::Relaxed) + 1;
        if exported_total == 1 {
            // 首个导出帧打一次完整信息：跨 GPU 场景下这条日志就是全部线索。
            tracing::info!(
                fourcc = format!("0x{fourcc:08X}"),
                modifier = format!("0x{modifier:016X}"),
                objects = frame.object_count(),
                width = frame.width,
                height = frame.height,
                "首个 DMA-BUF 帧已导出"
            );
        }
        Ok(StepOutcome::Frame(Produced {
            pts,
            frame: DisplayFrame::DmaBuf(frame),
            capture: None,
        }))
    }

    /// 按解码表面的**分配**尺寸建立 / 重建 RGB 转换器，并声明可见区域。
    ///
    /// 解码表面按高度 16 对齐（1080 → 1088、1530 → 1536），所以转换器必须用
    /// 第一帧报出来的尺寸建，不能预先用码流的宽高建。
    fn ensure_converter(&mut self, width: u32, height: u32) -> Result<(), VaapiError> {
        let Some(node) = self.rgb_node.clone() else {
            return Ok(());
        };
        let rebuild = match &self.converter {
            Some(converter) => converter.source_size() != (width, height),
            None => true,
        };
        if !rebuild {
            return Ok(());
        }
        let (out_w, out_h) = self.output_size.unwrap_or((width, height));
        let mut converter = RgbConverter::new_with_output(&node, width, height, out_w, out_h)?;
        converter.set_visible_size(self.visible.0, self.visible.1);
        converter.set_render_transform(self.render_transform);
        tracing::info!(
            node = %node.display(),
            surface = format!("{width}x{height}"),
            output = format!("{out_w}x{out_h}"),
            visible = format!("{}x{}", self.visible.0, self.visible.1),
            slots = converter.slots(),
            "RGB 转换器已按解码表面尺寸建立"
        );
        self.converter = Some(converter);
        Ok(())
    }

    /// 回退路径的 `next_producer_frame` 主体。
    fn poll(&mut self) -> Result<VideoPoll, liuguang_media::MediaError> {
        match self.step(false)? {
            StepOutcome::Frame(produced) => Ok(VideoPoll::Frame {
                pts: produced.pts,
                frame: produced.frame,
            }),
            StepOutcome::Throttled => Ok(VideoPoll::NeedMore),
            StepOutcome::EndOfSegment => Ok(VideoPoll::Ended),
        }
    }

    /// 当前未归还的帧数：RGB 路径报槽位占用，非 RGB 报解码表面占用。
    fn inflight(&self) -> usize {
        match &self.converter {
            Some(converter) => converter.inflight(),
            None => self.surface_inflight.load(Ordering::Relaxed),
        }
    }

    fn seek_start(&mut self) -> Result<(), VaapiError> {
        self.decoder.seek_start()?;
        Ok(())
    }

    fn set_output_size(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.output_size = Some((width, height));
        }
        if let Some(converter) = self.converter.as_mut() {
            converter.set_output_size(width, height);
        }
    }

    fn set_render_transform(&mut self, transform: RenderTransform) {
        self.render_transform = transform;
        if let Some(converter) = self.converter.as_mut() {
            converter.set_render_transform(transform);
        }
    }

    fn readback(&mut self) -> Result<(u32, u32, Vec<u8>), VaapiError> {
        match self.converter.as_mut() {
            Some(converter) => converter.readback_last_xrgb(),
            None => Err(VaapiError::Unsupported(
                "生产者不在 RGB 转换模式（用 open_rgb 建立）".to_string(),
            )),
        }
    }
}

/// 预读线程的一步。
enum PrefetchStep {
    Frame {
        pts: Duration,
        frame: DisplayFrame,
        capture: Option<Capture>,
    },
    /// 暂无可用帧（背压/转换忙）：低速退避后重试。
    Throttled,
    /// 流到头：线程自己 seek 回起点继续（沿用 looping 语义）。
    EndOfSegment,
}

/// 预读线程看到的"帧源"。
///
/// 抽成 trait 是为了让队列/背压/暂停/EOF 这些**线程逻辑**能在没有 GPU 的单测里
/// 跑起来（真机路径由 [`VaapiPipeline`] 实现）。
trait PrefetchSource: Send {
    /// 产出一帧或报告状态；`Err` 是致命错误（线程会退出并把错误交给消费者）。
    fn step(&mut self) -> Result<PrefetchStep, String>;

    /// seek 回流起点（EOF 后由线程自己调用）。
    fn seek_start(&mut self) -> Result<(), String>;

    /// 消费生产者从另一线程塞进来的配置变更（目前只有输出尺寸）。
    fn sync_config(&mut self) {}
}

/// 预读线程手里的真机管线：[`SyncPipeline`] + 跨线程的配置/统计入口。
struct VaapiPipeline {
    pipeline: SyncPipeline,
    /// 生产者写入、线程读取：待应用的输出尺寸（0 = 无；高 32 位宽、低 32 位高）。
    output_size: Arc<AtomicU64>,
    /// 生产者写入、线程读取：待应用的实时画面变换与亮度。
    render_transform: Arc<Mutex<Option<RenderTransform>>>,
    /// 逐帧回读开关（诊断探针）。
    capture: Arc<AtomicBool>,
    /// 上一帧 pts（单调性自检；循环 seek 后重置）。
    last_pts: Option<Duration>,
    /// pts 倒退次数（只记日志：坏封装不该让壁纸停摆）。
    pts_regressions: u64,
    /// 背压命中的累计次数（诊断：判断预读是否真的超前了）。
    throttles: u64,
    /// 已交付帧数（诊断，用于对齐背压与出帧速率）。
    delivered: u64,
}

impl VaapiPipeline {
    fn new(
        pipeline: SyncPipeline,
        output_size: Arc<AtomicU64>,
        render_transform: Arc<Mutex<Option<RenderTransform>>>,
        capture: Arc<AtomicBool>,
    ) -> Self {
        Self {
            pipeline,
            output_size,
            render_transform,
            capture,
            last_pts: None,
            pts_regressions: 0,
            throttles: 0,
            delivered: 0,
        }
    }

    /// pts 必须单调：daemon 靠 `last_pts` 倒退判断"播完一轮"（据此推进播放列表），
    /// 乱序会让它把同一轮误判成新的一轮。真出了只记日志，不 panic。
    fn note_pts(&mut self, pts: Duration) {
        if let Some(last) = self.last_pts {
            if pts < last {
                self.pts_regressions = self.pts_regressions.saturating_add(1);
                if self.pts_regressions <= 3 {
                    tracing::warn!(
                        last_ms = last.as_millis() as u64,
                        pts_ms = pts.as_millis() as u64,
                        regressions = self.pts_regressions,
                        "预读线程的 pts 非单调（解码器输出乱序？）"
                    );
                }
            }
        }
        self.last_pts = Some(pts);
    }
}

impl PrefetchSource for VaapiPipeline {
    fn step(&mut self) -> Result<PrefetchStep, String> {
        let capture = self.capture.load(Ordering::Relaxed);
        match self
            .pipeline
            .step(capture)
            .map_err(|error| error.to_string())?
        {
            StepOutcome::Frame(produced) => {
                self.note_pts(produced.pts);
                self.delivered = self.delivered.saturating_add(1);
                Ok(PrefetchStep::Frame {
                    pts: produced.pts,
                    frame: produced.frame,
                    capture: produced.capture,
                })
            }
            StepOutcome::Throttled => {
                // 背压次数：`inflight` 打满说明槽位被下游（播放器队列/合成器）占满，
                // 预读线程被迫停下——这时"预读"其实没有超前，抖动只能靠队列吸收。
                self.throttles = self.throttles.saturating_add(1);
                if self.throttles % 600 == 1 {
                    tracing::debug!(
                        throttles = self.throttles,
                        inflight = self.pipeline.inflight(),
                        delivered = self.delivered,
                        "预读线程被背压暂停（槽位/表面未归还）"
                    );
                }
                Ok(PrefetchStep::Throttled)
            }
            StepOutcome::EndOfSegment => Ok(PrefetchStep::EndOfSegment),
        }
    }

    fn seek_start(&mut self) -> Result<(), String> {
        self.pipeline
            .seek_start()
            .map_err(|error| error.to_string())?;
        // 新一轮的 pts 从 0 开始（daemon 靠这个倒退检测"播完一轮"）。
        self.last_pts = None;
        Ok(())
    }

    fn sync_config(&mut self) {
        let packed = self.output_size.swap(0, Ordering::AcqRel);
        if packed != 0 {
            let width = (packed >> 32) as u32;
            let height = (packed & 0xFFFF_FFFF) as u32;
            self.pipeline.set_output_size(width, height);
        }
        if let Ok(mut slot) = self.render_transform.lock() {
            if let Some(transform) = slot.take() {
                self.pipeline.set_render_transform(transform);
            }
        }
    }
}

/// 预读线程发给生产者的消息（FIFO，顺序即交付顺序）。
enum PrefetchEvent {
    Frame {
        pts: Duration,
        frame: DisplayFrame,
        capture: Option<Capture>,
    },
    /// 一轮播完：线程已经把 `seek_start` 打过（随后继续出帧）。
    LoopBoundary,
    /// 致命错误：线程即将退出。
    Failed(String),
}

/// 生产者与预读线程之间的控制块（原子量 + 唤醒）。
struct PrefetchControl {
    paused: AtomicBool,
    stop: AtomicBool,
    /// 待应用的输出尺寸（0 = 无；高 32 位宽、低 32 位高）。
    ///
    /// `Arc`：预读线程手里的 [`VaapiPipeline`] 也要看这个值，两边共享。
    output_size: Arc<AtomicU64>,
    /// 待应用的实时画面变换与亮度。
    render_transform: Arc<Mutex<Option<RenderTransform>>>,
    /// 逐帧回读（诊断探针）。同上，与线程共享。
    capture: Arc<AtomicBool>,
    /// 线程句柄，用来唤醒 `park_timeout` 里的线程。
    thread: Mutex<Option<std::thread::Thread>>,
}

impl PrefetchControl {
    fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            output_size: Arc::new(AtomicU64::new(0)),
            render_transform: Arc::new(Mutex::new(None)),
            capture: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }
    }

    fn attach(&self, thread: std::thread::Thread) {
        if let Ok(mut slot) = self.thread.lock() {
            *slot = Some(thread);
        }
    }

    fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// 唤醒 `park_timeout` 里的线程（暂停/恢复/停止立即生效）。
    fn unpark(&self) {
        if let Ok(slot) = self.thread.lock() {
            if let Some(thread) = slot.as_ref() {
                thread.unpark();
            }
        }
    }

    /// 暂停：置标志并唤醒线程，让它在 1ms 内停手。
    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        self.unpark();
    }

    /// 恢复：清标志并唤醒线程。
    fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.unpark();
    }

    /// 停止：线程在下一个检查点退出。
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.unpark();
    }

    /// 暂停/停止期间低速等待；返回 `false` 表示线程应当退出。
    fn wait_until_runnable(&self) -> bool {
        loop {
            if self.is_stopped() {
                return false;
            }
            if !self.is_paused() {
                return true;
            }
            // 暂停期间用粗粒度 park：唤醒由 resume 的 unpark 触发，不必靠轮询。
            std::thread::park_timeout(PAUSE_POLL);
        }
    }

    fn set_output_size(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let packed = (u64::from(width) << 32) | u64::from(height);
        self.output_size.store(packed, Ordering::Release);
    }

    fn set_capture(&self, enabled: bool) {
        self.capture.store(enabled, Ordering::Release);
    }

    fn set_render_transform(&self, transform: RenderTransform) {
        if let Ok(mut slot) = self.render_transform.lock() {
            *slot = Some(transform);
        }
        self.unpark();
    }
}

/// 生产者手里的预读句柄：接收端 + 控制块 + 线程。
struct Prefetch {
    rx: Receiver<PrefetchEvent>,
    ctl: Arc<PrefetchControl>,
    handle: Option<JoinHandle<()>>,
    /// RGB 路径的槽位占用计数（转换器在线程里，生产者只能持有计数句柄）。
    slot_inflight: Option<Arc<AtomicUsize>>,
    /// 通道容量（预读帧数），日志/诊断用。
    depth: usize,
    /// 已交付给上层的帧数。
    delivered: u64,
    /// 线程报的错（此后每次 poll 都返回它，保持"解码失败"语义）。
    error: Option<String>,
    /// 线程已结束且通道已空。
    finished: bool,
    /// 最近一次**交付给上层**的帧的像素（诊断探针）。
    last_capture: Option<Capture>,
    /// 最近一次交付的 pts（诊断）。
    last_pts: Option<Duration>,
    /// 线程退出时把管线**交还**给这里的接收端（t43）。
    ///
    /// 为什么不是让线程自己 drop：`SyncPipeline` 里有 VA 解码器与 EGL/GL 转换器，
    /// 它们的析构（`vaTerminate`/`eglTerminate`/`cuCtxDestroy`）必须在"发起拆除的
    /// 那个线程"上完成，否则驱动侧的析构顺序与资源归属都只能靠运气。线程把管线
    /// 放回这里，`Prefetch::drop` 在 join **之后**、于调用方线程（daemon 主线程，
    /// 即 `stop_video`）drop 它，顺序就是确定的：先停下线程 → 再销毁 display/context。
    returned: Option<Receiver<Box<dyn PrefetchSource>>>,
}

impl Prefetch {
    /// 起预读线程。返回句柄 + 管线移交入口。
    ///
    /// 管线**不直接 move 进闭包**：`thread::spawn` 失败时闭包会被丢弃，管线会
    /// 跟着一起没掉 —— 那就没法退回同步路径了。这里先起线程，成功后再把管线
    /// `send` 过去（线程第一步就是 `recv`）。
    fn spawn(
        depth: usize,
        slot_inflight: Option<Arc<AtomicUsize>>,
        capture: bool,
    ) -> std::io::Result<(Self, SyncSender<Box<dyn PrefetchSource>>)> {
        let depth = depth.max(1);
        let (tx, rx) = sync_channel(depth);
        let (handoff_tx, handoff_rx) = sync_channel::<Box<dyn PrefetchSource>>(1);
        let (return_tx, return_rx) = sync_channel::<Box<dyn PrefetchSource>>(1);
        let ctl = Arc::new(PrefetchControl::new());
        ctl.set_capture(capture);
        let worker_ctl = Arc::clone(&ctl);
        let handle = std::thread::Builder::new()
            .name("vaapi-prefetch".to_string())
            .spawn(move || {
                let Ok(mut source) = handoff_rx.recv() else {
                    return;
                };
                run_prefetch_worker(source.as_mut(), tx, worker_ctl);
                // 管线交还给 `Prefetch::drop`：解码器/EGL 上下文的析构留在主线程做
                // （见 `Prefetch::returned` 的注释）。线程已经退出循环，此后不再碰它。
                let _ = return_tx.send(source);
            })?;
        ctl.attach(handle.thread().clone());
        let prefetch = Self {
            rx,
            ctl,
            handle: Some(handle),
            slot_inflight,
            depth,
            delivered: 0,
            error: None,
            finished: false,
            last_capture: None,
            last_pts: None,
            returned: Some(return_rx),
        };
        Ok((prefetch, handoff_tx))
    }

    /// 非阻塞取一帧。
    fn poll(&mut self) -> Result<VideoPoll, liuguang_media::MediaError> {
        if let Some(reason) = self.error.as_deref() {
            return Err(liuguang_media::MediaError::Decode(format!(
                "解码预读线程失败: {reason}"
            )));
        }
        if self.finished {
            return Ok(VideoPoll::Ended);
        }
        match self.rx.try_recv() {
            Ok(PrefetchEvent::Frame {
                pts,
                frame,
                capture,
            }) => {
                self.delivered = self.delivered.saturating_add(1);
                self.last_pts = Some(pts);
                self.last_capture = capture;
                Ok(VideoPoll::Frame { pts, frame })
            }
            Ok(PrefetchEvent::LoopBoundary) => {
                tracing::debug!(
                    delivered = self.delivered,
                    depth = self.depth,
                    "预读线程播完一轮：已自行 seek 回起点，随后继续出帧"
                );
                Ok(VideoPoll::Ended)
            }
            Ok(PrefetchEvent::Failed(reason)) => {
                self.finished = true;
                let message = format!("解码预读线程失败: {reason}");
                self.error = Some(reason);
                Err(liuguang_media::MediaError::Decode(message))
            }
            Err(TryRecvError::Empty) => Ok(VideoPoll::NeedMore),
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                // 线程已退出：先把管线收回来（在调用方线程析构），再 join 拿 panic 信息。
                self.take_returned();
                match self.join_after_exit() {
                    Some(reason) => Err(liuguang_media::MediaError::Decode(format!(
                        "解码预读线程异常退出: {reason}"
                    ))),
                    None => Ok(VideoPoll::Ended),
                }
            }
        }
    }

    /// 线程已退出（通道断开）：join 拿 panic 信息。
    fn join_after_exit(&mut self) -> Option<String> {
        let handle = self.handle.take()?;
        handle.join().err().map(|payload| panic_message(&*payload))
    }

    fn inflight(&self) -> Option<usize> {
        self.slot_inflight
            .as_ref()
            .map(|counter| counter.load(Ordering::Acquire))
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        self.ctl.stop();
        if let Some(handle) = self.handle.take() {
            // 线程最多等一次 park 检查点（暂停时 20ms）；若它正在 seek，等它做完再释放
            // 解码器/EGL 上下文（设备关闭顺序敏感）。
            if handle.join().is_err() {
                tracing::warn!("解码预读线程在退出时 panic（管线已随线程结束）");
            }
        }
        // join 已返回：线程把管线放进回收通道后就退出了。**在这里** drop 它，
        // 让 VA display / EGL display 的销毁发生在发起拆除的线程上（t43）。
        self.take_returned();
    }
}

impl Prefetch {
    /// 收下线程交还的管线并在**当前线程**析构。
    ///
    /// 返回 `true` 表示确实收回了管线（析构已在本线程完成）。线程在接管前就
    /// panic 时收不到：那说明管线已随线程展开销毁，记一条 warn 即可。
    fn take_returned(&mut self) -> bool {
        let Some(returned) = self.returned.take() else {
            return false;
        };
        match returned.try_recv() {
            Ok(source) => {
                drop(source);
                true
            }
            Err(_) => {
                tracing::warn!("解码预读线程没有交还管线（线程在接管前退出），资源已随线程释放");
                false
            }
        }
    }
}

/// 把事件交给消费者；通道满/暂停时低速等待。返回 `false` 表示线程应当退出。
///
/// `try_send` + 1ms 轮询（而不是阻塞 `send`）：阻塞的 `send` 在暂停或上层丢弃
/// 播放器时无法及时停手，而 `sync_channel` 的容量本身已经是背压。
fn send_event(tx: &SyncSender<PrefetchEvent>, ctl: &PrefetchControl, event: PrefetchEvent) -> bool {
    let mut event = event;
    loop {
        if !ctl.wait_until_runnable() {
            return false;
        }
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned)) => {
                event = returned;
                // 队列满：等消费者取走一帧（这就是背压）。
                std::thread::park_timeout(IDLE_POLL);
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// 预读线程主循环：解码 → 推通道；EOF 自己 seek；暂停/停止立即响应。
fn run_prefetch_worker(
    source: &mut dyn PrefetchSource,
    tx: SyncSender<PrefetchEvent>,
    ctl: Arc<PrefetchControl>,
) {
    let mut loops: u64 = 0;
    loop {
        if !ctl.wait_until_runnable() {
            return;
        }
        source.sync_config();
        match source.step() {
            Ok(PrefetchStep::Frame {
                pts,
                frame,
                capture,
            }) => {
                if !send_event(
                    &tx,
                    &ctl,
                    PrefetchEvent::Frame {
                        pts,
                        frame,
                        capture,
                    },
                ) {
                    return;
                }
            }
            // 背压时槽位/表面要等显示侧归还（60fps 下每帧一次），2ms 的粒度足够：
            // 只把"醒来看看有没有空位"的唤醒次数砍半，不引入可观的补充延迟
            // （队列里还有 3 帧缓冲，够 50ms 的余量）。
            Ok(PrefetchStep::Throttled) => std::thread::park_timeout(THROTTLE_POLL),
            Ok(PrefetchStep::EndOfSegment) => {
                loops = loops.saturating_add(1);
                tracing::debug!(loops, "解码预读线程到达流末尾：自行 seek 回起点继续");
                // 先把边界事件交给消费者（FIFO 保证它在旧帧之后），再 seek：
                // 上层据此走既有的 `restart_loop`（时间轴惰性重锚）。
                if !send_event(&tx, &ctl, PrefetchEvent::LoopBoundary) {
                    return;
                }
                if let Err(reason) = source.seek_start() {
                    let _ = send_event(&tx, &ctl, PrefetchEvent::Failed(reason));
                    return;
                }
            }
            Err(reason) => {
                let _ = send_event(&tx, &ctl, PrefetchEvent::Failed(reason));
                return;
            }
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "未知 panic".to_string())
}

/// 当前管线状态。
enum Pipeline {
    /// 同步路径（预读线程还没启动，或启动失败退回来了）。
    ///
    /// `Box`：`SyncPipeline` 装了解码器 + 转换器（>1KB），而 `Prefetch` 只有
    /// 150 字节；不装箱的话每个 `Pipeline` 都要按大的一边占位。
    Local(Box<SyncPipeline>),
    /// 解码器与转换器已经交给预读线程。
    Prefetch(Prefetch),
}

/// VA-API 视频生产者。
pub struct VaapiProducer {
    info: MediaInfo,
    /// 渲染节点（日志用）。
    node: PathBuf,
    /// 导出帧总数（与预读线程共享）。
    exported: Arc<AtomicU64>,
    /// 未归还的解码表面数（与预读线程共享，非 RGB 路径背压）。
    surface_inflight: Arc<AtomicUsize>,
    /// 预读深度（通道容量）。
    depth: usize,
    /// 是否已请求逐帧回读（诊断探针）。
    capture: bool,
    /// RGB 转换模式（`open_rgb` 建立）。
    rgb: bool,
    /// 管线：同步 / 预读线程接管。
    pipeline: Option<Pipeline>,
}

impl VaapiProducer {
    /// 用 `path` + `render_node` 建生产者。
    ///
    /// # Errors
    ///
    /// 设备/解码器打不开时返回 [`VaapiError`]。
    pub fn open(path: &Path, render_node: &Path) -> Result<Self, VaapiError> {
        let decoder = VaapiDecoder::open(path, render_node)?;
        let StreamInfo {
            width,
            height,
            source_fps,
        } = decoder.stream_info();
        let depth = prefetch_depth();
        tracing::info!(
            media = %path.display(),
            node = %render_node.display(),
            width,
            height,
            fps = ?source_fps,
            prefetch = depth,
            "VA-API 解码管线已建立（零拷贝 DMA-BUF 导出；首次轮询时启动解码预读线程）"
        );
        let info = MediaInfo {
            kind: MediaKind::Video,
            width,
            height,
            duration: None,
            source_fps,
            codec: Some(format!("vaapi:{}", decoder.codec_name())),
        };
        let exported = Arc::new(AtomicU64::new(0));
        let surface_inflight = Arc::new(AtomicUsize::new(0));
        let pipeline = SyncPipeline {
            decoder,
            converter: None,
            rgb_node: None,
            visible: (0, 0),
            output_size: None,
            render_transform: RenderTransform::default(),
            exported: Arc::clone(&exported),
            surface_inflight: Arc::clone(&surface_inflight),
        };
        Ok(Self {
            info,
            node: render_node.to_path_buf(),
            exported,
            surface_inflight,
            depth,
            capture: false,
            rgb: false,
            pipeline: Some(Pipeline::Local(Box::new(pipeline))),
        })
    }

    /// 同 [`Self::open`]，但每帧先经 EGL/GLES 转成 XRGB8888 再交出去。
    ///
    /// 有的合成器（denial / Impeller）只把多平面 YUV 的 DMA-BUF 采样到亮度平面，
    /// 整屏发红；这里在 GPU 上把 NV12 直接渲染成 XRGB8888：颜色正确，而且依然是
    /// 零拷贝（输出是 gbm 分配的 block-linear BO，不经 CPU 回读）。
    ///
    /// # Errors
    ///
    /// 解码器打不开或 EGL/gbm 初始化失败时返回 [`liuguang_media::MediaError`]。
    pub fn open_rgb(path: &Path, render_node: &Path) -> Result<Self, liuguang_media::MediaError> {
        let mut producer = Self::open(path, render_node)?;
        let (width, height) = (producer.info.width, producer.info.height);
        tracing::info!(
            media = %path.display(),
            node = %render_node.display(),
            width,
            height,
            "RGB 转换管线已就绪（转换器等第一帧的解码表面尺寸再建：NV12 → XRGB8888，零拷贝输出）"
        );
        let Some(pipeline) = producer.local_mut() else {
            return Err(liuguang_media::MediaError::Decode(
                "RGB 生产者状态异常：管线已被预读线程接管".to_string(),
            ));
        };
        pipeline.rgb_node = Some(render_node.to_path_buf());
        pipeline.visible = (width, height);
        producer.rgb = true;
        Ok(producer)
    }

    /// 同步管线（只在还没被预读线程接管时可用）。
    fn local_mut(&mut self) -> Option<&mut SyncPipeline> {
        match self.pipeline.as_mut() {
            Some(Pipeline::Local(pipeline)) => Some(pipeline.as_mut()),
            Some(Pipeline::Prefetch(_)) | None => None,
        }
    }

    /// 把同步管线取出来；不在同步状态时原样放回并返回 `None`。
    fn take_local(&mut self) -> Option<SyncPipeline> {
        match self.pipeline.take() {
            Some(Pipeline::Local(pipeline)) => Some(*pipeline),
            other => {
                self.pipeline = other;
                None
            }
        }
    }

    /// 启动解码预读线程（懒启动：第一次 `next_producer_frame` 时才起）。
    ///
    /// 起不来就什么都不做：同步路径继续可用（功能不变，只是少了预读）。
    fn start_prefetch(&mut self) {
        if !matches!(self.pipeline, Some(Pipeline::Local(_))) {
            return;
        }
        if self.depth == 0 {
            tracing::info!("解码预读已关闭（LIUGUANG_VIDEO_PREFETCH=0），保持同步解码路径");
            return;
        }
        let slot_inflight = self.pipeline.as_ref().and_then(|pipeline| match pipeline {
            Pipeline::Local(local) => local.converter.as_ref().map(RgbConverter::inflight_handle),
            Pipeline::Prefetch(_) => None,
        });
        let spawned = Prefetch::spawn(self.depth, slot_inflight, self.capture);
        let (mut prefetch, handoff) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "解码预读线程启动失败：退回同步解码路径（功能不变，只是少一层预读）"
                );
                return;
            }
        };
        let Some(pipeline) = self.take_local() else {
            return;
        };
        tracing::debug!(
            depth = prefetch.depth,
            "解码预读线程已启动，解码器/转换器移交完成"
        );
        let source = VaapiPipeline::new(
            pipeline,
            Arc::clone(&prefetch.ctl.output_size),
            Arc::clone(&prefetch.ctl.render_transform),
            Arc::clone(&prefetch.ctl.capture),
        );
        if handoff.send(Box::new(source)).is_err() {
            // 线程在 recv 前退出（只可能是极早期 panic）：管线已经随消息一起
            // 被丢弃，没法再退回同步路径，只能让下一次 poll 报错。
            tracing::error!("预读管线移交失败：线程在接管前退出，本次播放按解码失败处理");
            prefetch.error = Some("预读线程在接管解码管线前退出".to_string());
        }
        self.pipeline = Some(Pipeline::Prefetch(prefetch));
    }

    /// 当前未归还的帧数。
    ///
    /// RGB 路径下合成器攥着的是转换器的输出槽位，所以这里报槽位占用数，
    /// 而不是（那条路径上恒为 0 的）解码表面计数。
    #[must_use]
    pub fn inflight(&self) -> usize {
        match self.pipeline.as_ref() {
            Some(Pipeline::Local(pipeline)) => pipeline.inflight(),
            Some(Pipeline::Prefetch(prefetch)) => prefetch
                .inflight()
                .unwrap_or_else(|| self.surface_inflight.load(Ordering::Relaxed)),
            None => 0,
        }
    }

    /// 导出帧总数。
    #[must_use]
    pub fn exported_frames(&self) -> u64 {
        self.exported.load(Ordering::Relaxed)
    }

    /// 渲染节点。
    #[must_use]
    pub fn node(&self) -> &Path {
        &self.node
    }

    /// 开启逐帧 GPU 回读（诊断探针用；正常播放路径不要开）。
    ///
    /// 预读线程会**多转换几帧**，所以不能再"回读转换器最后一次绘制"——那可能是
    /// 还没交付给上层的一帧。开启后每帧的像素随帧一起传送，
    /// [`Self::readback_last_xrgb`] 返回**最近一次交付给上层**的那帧。
    pub fn enable_capture(&mut self) {
        self.capture = true;
        if let Some(Pipeline::Prefetch(prefetch)) = self.pipeline.as_ref() {
            prefetch.ctl.set_capture(true);
        }
    }

    /// 回读最近一次 RGB 转换的输出（诊断探针用，正常播放路径不调用）。
    ///
    /// 返回 `(width, height, RGBA 自上而下)`；非 RGB 转换模式下返回错误。
    pub fn readback_last_xrgb(&mut self) -> Result<(u32, u32, Vec<u8>), VaapiError> {
        match self.pipeline.as_mut() {
            Some(Pipeline::Local(pipeline)) => pipeline.readback(),
            Some(Pipeline::Prefetch(prefetch)) => match prefetch.last_capture.as_ref() {
                Some(capture) => Ok((capture.width, capture.height, capture.rgba.clone())),
                None => Err(VaapiError::Unsupported(
                    "预读模式下的回读需要逐帧捕获（先调 enable_capture）".to_string(),
                )),
            },
            None => Err(VaapiError::Unsupported("解码管线正在移交".to_string())),
        }
    }
}

/// `FrameKeepalive` 的载荷：持有解码表面，并在释放时把计数器减一。
struct SurfaceGuard {
    _surface: DecodedSurface,
    inflight: Arc<AtomicUsize>,
}

impl Drop for SurfaceGuard {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl FrameProducer for VaapiProducer {
    fn info(&self) -> MediaInfo {
        self.info.clone()
    }

    fn next_producer_frame(&mut self) -> Result<VideoPoll, liuguang_media::MediaError> {
        // 懒启动：解码器/转换器在第一次轮询时才交给预读线程（构造阶段不碰线程，
        // 错误也就留在各自该出现的位置上）。
        if matches!(self.pipeline, Some(Pipeline::Local(_))) {
            self.start_prefetch();
        }
        match self.pipeline.as_mut() {
            Some(Pipeline::Local(pipeline)) => pipeline.poll(),
            Some(Pipeline::Prefetch(prefetch)) => prefetch.poll(),
            None => Ok(VideoPoll::NeedMore),
        }
    }

    fn dmabuf_state(&self) -> Option<DmabufState> {
        Some(DmabufState {
            is_dmabuf: true,
            fallbacks: 0,
            keepalive_len: self.inflight(),
        })
    }

    fn seek_start(&mut self) -> Result<(), liuguang_media::MediaError> {
        match self.pipeline.as_mut() {
            Some(Pipeline::Local(pipeline)) => pipeline.seek_start().map_err(Into::into),
            // 预读模式：线程到 EOF 自己 seek；上层 `restart_loop` 的这次调用是
            // 补一个仪式感——再 seek 一次会打乱线程正在回填的新一轮。
            Some(Pipeline::Prefetch(_)) => Ok(()),
            None => Ok(()),
        }
    }

    fn decoder_kind(&self) -> DecoderKind {
        DecoderKind::Vaapi
    }

    /// 暂停/恢复预读线程。
    ///
    /// 暂停必须**立刻停手**（硬指标：暂停时 CPU ≤1%）：置标志 + `unpark`，
    /// 线程在 1ms 内停下，连解码都不再发起。
    fn set_playing(&mut self, playing: bool) -> Result<(), liuguang_media::MediaError> {
        if let Some(Pipeline::Prefetch(prefetch)) = self.pipeline.as_ref() {
            if playing {
                prefetch.ctl.resume();
            } else {
                prefetch.ctl.pause();
            }
        }
        Ok(())
    }

    /// 转发给转换器（RGB 路径会按面板尺寸懒重建输出池）。
    ///
    /// **必须实现在 trait 里**：上层 `VideoPlayer<P>` 是泛型调用
    /// `P::set_output_size`，只会命中 trait 方法；写成 inherent 方法会被
    /// 默认空实现吞掉（真机踩过：日志里看不到尺寸变化）。
    fn set_output_size(&mut self, width: u32, height: u32) {
        match self.pipeline.as_mut() {
            Some(Pipeline::Local(pipeline)) => pipeline.set_output_size(width, height),
            // 转换器在线程里：把尺寸塞进控制块，线程在下一帧之前落地。
            Some(Pipeline::Prefetch(prefetch)) => prefetch.ctl.set_output_size(width, height),
            None => {}
        }
    }

    fn set_render_transform(&mut self, transform: RenderTransform) {
        match self.pipeline.as_mut() {
            Some(Pipeline::Local(pipeline)) => pipeline.set_render_transform(transform),
            Some(Pipeline::Prefetch(prefetch)) => prefetch.ctl.set_render_transform(transform),
            None => {}
        }
    }

    fn handles_render_transform(&self) -> bool {
        self.rgb
    }
}

impl std::fmt::Debug for VaapiProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaapiProducer")
            .field("node", &self.node)
            .field("inflight", &self.inflight())
            .field("exported", &self.exported_frames())
            .field("rgb", &self.rgb)
            .field("depth", &self.depth)
            .field(
                "prefetch",
                &matches!(self.pipeline, Some(Pipeline::Prefetch(_))),
            )
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Instant;

    use liuguang_core::types::{CpuFrame, PixelFormat};

    /// 无 GPU 的假帧源：按固定步长产出 CPU 帧，每 `segment` 帧报告一次 EOF。
    struct ScriptedSource {
        /// 每段最多产出多少帧（到上限后返回 `EndOfSegment`）。
        segment: usize,
        /// 每帧模拟耗时（0 = 立即产出）。
        cost: Duration,
        /// 每个 pts 的步进。
        step_us: u64,
        produced: Arc<AtomicUsize>,
        seeks: Arc<AtomicUsize>,
        /// 当前段已产出帧数（seek 时清零）。
        in_segment: usize,
    }

    impl ScriptedSource {
        fn new(segment: usize, produced: Arc<AtomicUsize>, seeks: Arc<AtomicUsize>) -> Self {
            Self {
                segment,
                cost: Duration::ZERO,
                step_us: 16_667,
                produced,
                seeks,
                in_segment: 0,
            }
        }
    }

    impl PrefetchSource for ScriptedSource {
        fn step(&mut self) -> Result<PrefetchStep, String> {
            if !self.cost.is_zero() {
                std::thread::sleep(self.cost);
            }
            if self.in_segment >= self.segment {
                return Ok(PrefetchStep::EndOfSegment);
            }
            let index = self.in_segment;
            self.in_segment += 1;
            self.produced.fetch_add(1, Ordering::SeqCst);
            Ok(PrefetchStep::Frame {
                pts: Duration::from_micros(index as u64 * self.step_us),
                frame: cpu_frame(index as u8),
                capture: None,
            })
        }

        fn seek_start(&mut self) -> Result<(), String> {
            self.in_segment = 0;
            self.seeks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn cpu_frame(fill: u8) -> DisplayFrame {
        DisplayFrame::Cpu(CpuFrame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Xrgb8888,
            data: Arc::from([fill; 16]),
        })
    }

    fn frame_pts(event: &PrefetchEvent) -> Option<Duration> {
        match event {
            PrefetchEvent::Frame { pts, .. } => Some(*pts),
            _ => None,
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        condition()
    }

    fn spawn_scripted(
        segment: usize,
        depth: usize,
    ) -> (Prefetch, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let produced = Arc::new(AtomicUsize::new(0));
        let seeks = Arc::new(AtomicUsize::new(0));
        let (prefetch, handoff) = Prefetch::spawn(depth, None, false).expect("预读线程应能启动");
        handoff
            .send(Box::new(ScriptedSource::new(
                segment,
                Arc::clone(&produced),
                Arc::clone(&seeks),
            )))
            .expect("管线移交应成功");
        (prefetch, produced, seeks)
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn backpressure_limit_is_small_and_nonzero() {
        assert!(
            MAX_INFLIGHT >= 2 && MAX_INFLIGHT <= 8,
            "实际: {MAX_INFLIGHT}"
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn prefetch_depth_bounds_are_sane() {
        assert!(DEFAULT_PREFETCH >= MIN_PREFETCH && DEFAULT_PREFETCH <= MAX_PREFETCH);
        // 2 = 「正在解码的一帧 + 已转好待取的一帧」，省一个输出缓冲仍能维持 60fps。
        assert_eq!(DEFAULT_PREFETCH, 2);
    }

    /// 预读队列满时，线程必须停在 `try_send` 上（= 背压），不能再解码。
    #[test]
    fn prefetch_queue_full_applies_backpressure() {
        let depth = 2;
        let (prefetch, produced, _seeks) = spawn_scripted(1_000, depth);
        // 队列容量 depth ⇒ 线程最多推进到 "depth 帧在通道里 + 1 帧卡在发送上"。
        assert!(
            wait_until(
                || produced.load(Ordering::SeqCst) > depth,
                Duration::from_secs(2)
            ),
            "预读线程没把队列灌满（produced={}）",
            produced.load(Ordering::SeqCst)
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            produced.load(Ordering::SeqCst),
            depth + 1,
            "队列满时线程不得继续解码（背压失效）"
        );
        // 取走一帧 ⇒ 线程只补一帧，然后再次卡住。
        prefetch
            .rx
            .recv_timeout(Duration::from_secs(1))
            .expect("队列里应有帧");
        assert!(
            wait_until(
                || produced.load(Ordering::SeqCst) == depth + 2,
                Duration::from_secs(2)
            ),
            "取走一帧后线程应补一帧（produced={}）",
            produced.load(Ordering::SeqCst)
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            produced.load(Ordering::SeqCst),
            depth + 2,
            "补完一帧后应再次卡在背压上"
        );
    }

    /// 暂停期间线程一帧都不许解；恢复后立刻继续。
    #[test]
    fn prefetch_does_not_advance_while_paused() {
        let (prefetch, produced, _seeks) = spawn_scripted(1_000, 2);
        assert!(wait_until(
            || produced.load(Ordering::SeqCst) >= 3,
            Duration::from_secs(2)
        ));
        prefetch.ctl.pause();
        // 吃掉通道里已有的帧：通道腾出空位，若线程没停手就一定会继续产帧。
        while prefetch.rx.try_recv().is_ok() {}
        // 给"暂停瞬间正在 step 的那一帧"一点时间落定（它不会被送出通道）。
        std::thread::sleep(Duration::from_millis(20));
        let frozen = produced.load(Ordering::SeqCst);
        while prefetch.rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            produced.load(Ordering::SeqCst),
            frozen,
            "暂停期间线程不得解码"
        );
        assert!(prefetch.ctl.is_paused());
        prefetch.ctl.resume();
        assert!(
            wait_until(
                || produced.load(Ordering::SeqCst) > frozen,
                Duration::from_secs(2)
            ),
            "恢复后线程应继续解码"
        );
    }

    /// EOF 由线程自己 seek 回起点，并把"一轮结束"事件排在旧帧之后。
    #[test]
    fn prefetch_seeks_and_continues_at_end_of_stream() {
        let segment = 2;
        let (prefetch, produced, seeks) = spawn_scripted(segment, 4);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut events = Vec::new();
        while events.len() < segment * 2 + 1 && Instant::now() < deadline {
            if let Ok(event) = prefetch.rx.recv_timeout(Duration::from_millis(500)) {
                events.push(event);
            }
        }
        assert!(events.len() > segment * 2, "事件太少: {}", events.len());
        assert_eq!(
            frame_pts(&events[0]),
            Some(Duration::ZERO),
            "第一段应从 pts=0 开始"
        );
        assert!(
            frame_pts(&events[segment - 1]).is_some(),
            "第 {segment} 帧应在边界之前"
        );
        assert!(
            matches!(&events[segment], PrefetchEvent::LoopBoundary),
            "一轮结束后应先给出 LoopBoundary"
        );
        // seek 之后 pts 必须回到 0：daemon 靠 `last_pts` 倒退判断"播完一轮"。
        assert_eq!(
            frame_pts(&events[segment + 1]),
            Some(Duration::ZERO),
            "seek 之后应是新一轮的第一帧（pts 回到 0）"
        );
        assert!(
            wait_until(|| seeks.load(Ordering::SeqCst) >= 1, Duration::from_secs(2)),
            "线程应自己 seek 回起点"
        );
        assert!(produced.load(Ordering::SeqCst) > segment);
    }

    #[test]
    fn open_rgb_missing_file_is_reported() {
        // `open_rgb` 先走一遍解码器打开流程，坏输入必须在碰 EGL 之前就报出来。
        let error = VaapiProducer::open_rgb(
            Path::new("/nonexistent/definitely-no-video.mp4"),
            Path::new("/dev/dri/renderD128"),
        )
        .expect_err("不存在的文件必须报错");
        assert!(
            matches!(error, liuguang_media::MediaError::Decode(_)),
            "实际: {error:?}"
        );
    }

    #[test]
    fn missing_file_is_reported() {
        let error = VaapiProducer::open(
            Path::new("/nonexistent/definitely-no-video.mp4"),
            Path::new("/dev/dri/renderD128"),
        )
        .expect_err("不存在的文件必须报错");
        assert!(matches!(error, VaapiError::Ffmpeg(_)), "实际: {error}");
    }
}
