//! 支持**定点**取帧的缩略图（GUI 悬停动效要 0/25%/50%/75% 多点帧）。
//!
//! `at_ms == 0`（首帧）与图片直接委托给 [`liuguang_media::thumbnail::extract`] ——
//! 那条路径已在真机上验证过；只有当调用方要求"视频第 N 毫秒的画面"时才走本模块
//! 自己的 seek 管线（`crates/liuguang-media` 不在本次改动范围内，所以 seek 写在
//! daemon 侧）。
//!
//! 为什么不是每帧都 seek：seek 要 flush + 重新预滚，单次成本远高于"拉首帧"。
//! GUI 的悬停动效只在 hover 时按需取 3–4 个时刻，且 daemon 侧一次性返回，够用。
//!
//! ## 真机教训（t10 修的那次回归）
//!
//! 第一版写成 `pipeline.state(5s)` 再要求 `current == PAUSED`，结果**所有视频的
//! `at_ms > 0` 都失败**（报"管线无法预滚到 PAUSED"）。根因不是解码器：
//! `Element::state()`（`gst_element_get_state`）只**等**一个已经在进行的异步状态
//! 变化，它**不发起**状态变化——管线还在 NULL 且没有 pending 变化时，它立刻返回
//! `(SUCCESS, NULL, VOID_PENDING)`，于是被误判成预滚失败。
//!
//! 现在改成显式 `set_state(PAUSED)` 再在总线上等 `ASYNC_DONE`（这才是"预滚完成"
//! 的信号），seek 之后再等一次，最后切 PLAYING 用 `try-pull-sample` 拉帧 ——
//! 与 `crates/liuguang-media` 里那条已验证的首帧路径同一个套路。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use liuguang_media::thumbnail::Thumbnail;

/// 取一帧缩略图。
///
/// - 图片：忽略 `at_ms`（图片只有一帧），走既有实现；
/// - 视频 `at_ms == 0`：走既有实现（最快路径）；
/// - 视频 `at_ms > 0`：seek 到该时刻再取一帧（超出时长则取最后一帧附近）。
///
/// # Errors
///
/// 解码失败、seek 失败、超时、文件不可读时返回人类可读的原因。
pub fn extract_at(path: &Path, max_w: u32, max_h: u32, at_ms: u64) -> Result<Thumbnail, String> {
    if at_ms == 0 || is_image(path) {
        if let Some(thumb) = decode_png_direct(path, max_w, max_h)? {
            return Ok(thumb);
        }
        return liuguang_media::thumbnail::extract(path, max_w, max_h).map_err(|e| e.to_string());
    }
    imp::extract_seek(path, max_w, max_h, at_ms)
}

/// 是否图片扩展名（图片没有时间轴，`at_ms` 无意义）。
#[must_use]
pub fn is_image(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    liuguang_core::library::DEFAULT_IMAGE_EXTENSIONS.contains(&ext.as_str())
}

/// 预滚上限：解码 4K/8K 素材的首个关键帧可能要几百毫秒，给足余量。
#[cfg(feature = "gstreamer")]
const PREROLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 拉帧上限：seek 之后已经预滚过，正常情况下第一个缓冲就是目标帧。
#[cfg(feature = "gstreamer")]
const PULL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 抽帧后最多按 `MAX_SAMPLE_DIM` 抽样（与 `liuguang-media` 保守一致）。
const MAX_SAMPLE_DIM: u32 = 1024;

/// 内部缩略图子进程参数：将 GStreamer / FFmpeg / libLLVM 隔离到按需启停的短命子进程中。
pub const INTERNAL_THUMB_WORKER_ARG: &str = "--internal-thumb-worker";

/// 缩略图子进程空闲超时后自动退出，归还全部 GStreamer 插件与匿名页。
const CHILD_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// 单次缩略图子进程请求的读超时上限（覆盖 GStreamer 预滚/seek/拉帧，防止卡死永久阻塞 worker）。
const CHILD_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 若输入确认为 PNG 文件（匹配 IHDR 签名），使用纯 Rust 流式降采样解码，绕开 GStreamer。
fn decode_png_direct(path: &Path, max_w: u32, max_h: u32) -> Result<Option<Thumbnail>, String> {
    use std::io::Read as _;
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if max_w == 0 || max_h == 0 {
        return Err("缩略图尺寸不能为 0".to_string());
    }
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    let mut head = [0u8; 8];
    let n = file
        .read(&mut head)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    if n < SIG.len() || head != SIG {
        return Ok(None);
    }
    drop(file);
    let cap_w = max_w.min(MAX_SAMPLE_DIM);
    let cap_h = max_h.min(MAX_SAMPLE_DIM);
    let decode_w = cap_w.saturating_mul(2).min(MAX_SAMPLE_DIM);
    let decode_h = cap_h.saturating_mul(2).min(MAX_SAMPLE_DIM);
    let cpu = liuguang_media::png_load::load_png_fitted(path, decode_w, decode_h)
        .map_err(|e| e.to_string())?;
    let thumb = Thumbnail {
        width: cpu.width,
        height: cpu.height,
        rgba: cpu.data.to_vec(),
    };
    Ok(Some(liuguang_media::thumbnail::scale_to_fit(
        thumb, cap_w, cap_h,
    )))
}

#[cfg(feature = "gstreamer")]
mod imp {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_video as gst_video;

    use super::{MAX_SAMPLE_DIM, PREROLL_TIMEOUT, PULL_TIMEOUT, Thumbnail};

    /// 取帧管线：**`sync=true`** 与首帧路径不同。
    ///
    /// `sync=false` 时解码器会以"能跑多快跑多快"的速度灌缓冲，`max-buffers=1
    /// drop=true` 只留最新一帧 —— 于是"seek 到 3s"实际拉到的可能是 3.7s 的画面，
    /// 每次还不一样（真机实测：同一时刻两次请求取到不同帧）。定点取帧要**可复现**，
    /// 所以这里让 sink 跟着时钟走，第一个缓冲就是 seek 目标那一帧。
    const DESC: &str = "filesrc name=src ! decodebin ! videoconvert ! \
                        video/x-raw,format=RGBA ! appsink name=sink sync=true \
                        max-buffers=1 drop=true";

    /// 取一帧并**在管线里就缩到目标尺寸**（t39：CPU 热点）。
    ///
    /// 与首帧/定点路径的区别只在最后一段：`videoscale` 放在 `videoconvert`
    /// 之前，且 app sink 的 caps 直接钉死目标尺寸 —— 于是 4K/8K 帧只在
    /// I420/NV12 域里缩一次小图再转 RGBA，省掉"整帧 4K RGBA 转换"这一大笔
    /// CPU（真机实测 4K HEVC 单帧 ~1.4s → ~0.2s）。
    pub fn extract_fitted(
        path: &Path,
        fit_w: u32,
        fit_h: u32,
        at_ms: u64,
    ) -> Result<Thumbnail, String> {
        if fit_w == 0 || fit_h == 0 {
            return Err("缩略图尺寸不能为 0".to_string());
        }
        gst::init().map_err(|e| format!("gst::init 失败: {e}"))?;
        let desc = format!(
            "filesrc name=src ! decodebin ! videoscale ! videoconvert ! \
             video/x-raw,format=RGBA,width={fit_w},height={fit_h} ! \
             appsink name=sink sync=true max-buffers=1 drop=true"
        );
        let element = gst::parse::launch(&desc).map_err(|e| format!("构建缩略图管线失败: {e}"))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| "管线不是 Pipeline".to_string())?;
        let Some(src) = pipeline.by_name("src") else {
            return Err("找不到 filesrc".to_string());
        };
        src.set_property("location", path.to_string_lossy().into_owned());
        let Some(sink) = pipeline.by_name("sink") else {
            return Err("找不到 appsink".to_string());
        };
        let bus = pipeline.bus().ok_or_else(|| "管线没有 bus".to_string())?;
        let result = run(&pipeline, &bus, &sink, at_ms, fit_w, fit_h);
        let _ = pipeline.set_state(gst::State::Null);
        result
    }

    /// 预滚到 PAUSED → 定点 seek → PLAYING 拉一帧。
    pub fn extract_seek(
        path: &Path,
        max_w: u32,
        max_h: u32,
        at_ms: u64,
    ) -> Result<Thumbnail, String> {
        if max_w == 0 || max_h == 0 {
            return Err("缩略图尺寸不能为 0".to_string());
        }
        gst::init().map_err(|e| format!("gst::init 失败: {e}"))?;
        let element = gst::parse::launch(DESC).map_err(|e| format!("构建缩略图管线失败: {e}"))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| "管线不是 Pipeline".to_string())?;
        let Some(src) = pipeline.by_name("src") else {
            return Err("找不到 filesrc".to_string());
        };
        src.set_property("location", path.to_string_lossy().into_owned());
        let Some(sink) = pipeline.by_name("sink") else {
            return Err("找不到 appsink".to_string());
        };
        let bus = pipeline.bus().ok_or_else(|| "管线没有 bus".to_string())?;

        // 结果在这里统一清理管线，避免每条错误路径都手写一次 set_state(Null)。
        let result = run(&pipeline, &bus, &sink, at_ms, max_w, max_h);
        let _ = pipeline.set_state(gst::State::Null);
        result
    }

    /// 真正的取帧流程（`extract_seek` 负责建管线与收尾）。
    fn run(
        pipeline: &gst::Pipeline,
        bus: &gst::Bus,
        sink: &gst::Element,
        at_ms: u64,
        max_w: u32,
        max_h: u32,
    ) -> Result<Thumbnail, String> {
        // 1) 显式发起预滚，再等 ASYNC_DONE。
        //
        //    必须**先** set_state：`gst_element_get_state`（gstreamer-rs 的
        //    `Element::state()`）只等一个已经在进行的异步变化，不发起变化。
        //    t10 之前漏了这一行，于是 state() 立刻返回 (SUCCESS, NULL)，
        //    被误判成"预滚失败"——所有视频的定点缩略图因此全挂。
        let _ = pipeline.set_state(gst::State::Paused);
        wait_async_done(bus, PREROLL_TIMEOUT, at_ms, "预滚到 PAUSED")?;

        // 2) 定点 seek。
        //
        //    FLUSH：丢掉已缓冲的帧；ACCURATE：落在**请求的时刻**而不是前一个
        //    关键帧（悬停动效对画面位置敏感）。
        if at_ms > 0 {
            let target = seek_target(pipeline, at_ms);
            pipeline
                .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, target)
                .map_err(|e| format!("seek 到 {at_ms}ms 失败: {e}"))?;
            // seek 会触发一次新的异步预滚；等不到 ASYNC_DONE 不算致命——下面能
            // 拉到样本就算成功（有些解复用器不发 ASYNC_DONE 但照样能出帧）。
            let _ = wait_async_done(bus, PREROLL_TIMEOUT, at_ms, "seek 后重新预滚");
        }

        // 3) PLAYING 拉第一帧（与 liuguang-media 已验证的首帧路径同一套路）。
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| format!("管线进入 PLAYING 失败: {e}"))?;
        let deadline = Instant::now() + PULL_TIMEOUT;
        let sample = loop {
            // 先拉再等：多数情况下 seek 预滚完成时目标帧已经在队列里，这样拿到
            // 的就是"请求的那一帧"，而不是等完 50ms 之后解码器又跑出来的下一帧。
            if let Some(sample) =
                sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&0u64])
            {
                break sample;
            }
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
                match msg.view() {
                    gst::MessageView::Error(err) => {
                        let detail = err.error();
                        let debug = err.debug().map(|d| d.to_string()).unwrap_or_default();
                        return Err(format!("解码失败: {detail}（{debug}）"));
                    }
                    gst::MessageView::Eos(..) => {
                        return Err(format!("seek 到 {at_ms}ms 已超出视频时长"));
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "在 {at_ms}ms 处取帧超时（{}s 内没有产出样本）",
                    PULL_TIMEOUT.as_secs()
                ));
            }
        };

        let frame = sample_to_rgba(&sample)?;
        Ok(liuguang_media::thumbnail::scale_to_fit(
            frame,
            max_w.min(MAX_SAMPLE_DIM),
            max_h.min(MAX_SAMPLE_DIM),
        ))
    }

    /// seek 目标：超出时长时夹到"最后一帧附近"，让越界的悬停时刻也能出图。
    ///
    /// 时长查询失败（部分容器头部没有时长为 0）时不夹，交给 seek 自己报错。
    fn seek_target(pipeline: &gst::Pipeline, at_ms: u64) -> gst::ClockTime {
        let target = gst::ClockTime::from_mseconds(at_ms);
        match pipeline.query_duration::<gst::ClockTime>() {
            Some(duration) if duration <= target => {
                duration.saturating_sub(gst::ClockTime::from_mseconds(1))
            }
            _ => target,
        }
    }

    /// 等一次异步状态变化完成（预滚 / seek 后重新预滚）。
    ///
    /// 只认总线上的 `ASYNC_DONE`；`Element::state()` 在这里不可用（见模块文档）。
    fn wait_async_done(
        bus: &gst::Bus,
        timeout: Duration,
        at_ms: u64,
        stage: &str,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "{stage}超时（{}s 内没有 ASYNC_DONE，at_ms = {at_ms}）",
                    timeout.as_secs()
                ));
            }
            let slice = left.min(Duration::from_millis(50));
            let slice_ms = u64::try_from(slice.as_millis()).unwrap_or(50);
            let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(slice_ms)) else {
                continue;
            };
            match msg.view() {
                gst::MessageView::AsyncDone(..) => return Ok(()),
                gst::MessageView::Error(err) => {
                    let detail = err.error();
                    let debug = err.debug().map(|d| d.to_string()).unwrap_or_default();
                    return Err(format!("解码失败: {detail}（{debug}）"));
                }
                gst::MessageView::Eos(..) => {
                    return Err(format!("seek 到 {at_ms}ms 已超出视频时长"));
                }
                _ => {}
            }
        }
    }

    /// 把一次样本拷成紧凑 RGBA8（去掉行末 padding）。
    fn sample_to_rgba(sample: &gst::Sample) -> Result<Thumbnail, String> {
        let caps = sample.caps().ok_or_else(|| "样本没有 caps".to_string())?;
        let info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|e| format!("样本 caps 无法解析为视频格式: {e}（caps={caps:?}）"))?;
        if info.format() != gst_video::VideoFormat::Rgba {
            return Err(format!("解码结果不是 RGBA（实际 {:?}）", info.format()));
        }
        let buffer = sample
            .buffer()
            .ok_or_else(|| "样本没有 buffer".to_string())?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|e| format!("映射视频帧失败: {e}"))?;
        let (width, height) = (info.width(), info.height());
        if width == 0 || height == 0 {
            return Err("视频帧尺寸不能为 0".to_string());
        }
        liuguang_media::png_load::check_image_budget(width, height)
            .map_err(|e| format!("视频帧尺寸超限: {e}"))?;
        let stride = info
            .stride()
            .first()
            .copied()
            .and_then(|s| usize::try_from(s).ok())
            .filter(|s| *s >= width as usize * 4)
            .ok_or_else(|| "非法 RGBA 跨度".to_string())?;
        let data = frame
            .plane_data(0)
            .map_err(|e| format!("读取 RGBA 平面失败: {e}"))?;
        let row = width as usize * 4;
        let mut rgba = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = y * stride;
            let end = start + row;
            if end > data.len() {
                return Err("RGBA 平面小于声明尺寸".to_string());
            }
            rgba.extend_from_slice(&data[start..end]);
        }
        drop(frame);
        Ok(Thumbnail {
            width,
            height,
            rgba,
        })
    }
}

#[cfg(not(feature = "gstreamer"))]
mod imp {
    use std::path::Path;

    use super::Thumbnail;

    /// 未编入 `gstreamer` feature 时无法抽帧。
    pub fn extract_fitted(
        _path: &Path,
        _fit_w: u32,
        _fit_h: u32,
        _at_ms: u64,
    ) -> Result<Thumbnail, String> {
        Err("视频缩略图需要 --features gstreamer 构建".to_string())
    }

    /// 未编入 `gstreamer` feature 时无法 seek 取帧。
    pub fn extract_seek(
        _path: &Path,
        _max_w: u32,
        _max_h: u32,
        at_ms: u64,
    ) -> Result<Thumbnail, String> {
        Err(format!(
            "视频定点缩略图（at_ms = {at_ms}）需要 --features gstreamer 构建"
        ))
    }
}

// ===== t39：缩略图两级缓存 + 串行解码队列 =====
//
// 真机现场（t39 前的 t38 构建）：GUI 建 38 张卡 = 发 38 次 `get_thumbnail`，
// 每一次都在**运行时线程**上同步解码（4K/8K 帧一帧 ~0.4–1.5s CPU），于是
// daemon CPU 从 10% 飙到 ~100% 持续几十秒，同时把渲染与其它请求全堵死
// （另一条连接的 `status` 要 1.1s、`set_wallpaper` 1.3s）。
//
// 这里的做法：
// 1. **两级缓存**：内存 LRU（已编码 PNG 字节）+ 磁盘
//    `$XDG_CACHE_HOME/liuguang/thumbs/<key>.png`（key = sha256(路径、大小、
//    mtime、at_ms、目标尺寸)）。命中直接返回，绝不再走 GStreamer。
// 2. **串行解码 + 有界队列**：一条后台线程按 FIFO 解码，队列上限
//    [`MAX_QUEUE`]；满了立刻回 [`liuguang_ipc::ErrorKind::Busy`]（不阻塞、
//    不排队无限长）。同一个 key 的多张请求合并成一次解码（in-flight 合并）。
// 3. **占空比节流**：每解一张就按 [`DUTY_TARGET`] 歇一会儿，把后台解码压在
//    单核四分之一左右 —— GUI 首屏加载不该把正在播的壁纸拖卡。
// 4. **管线内缩放**：解码时就把画面缩到目标尺寸（[`imp::extract_fitted`]），
//    不再先转整帧 4K RGBA 再缩，单帧 CPU 降一个数量级。

/// 内存缓存条目上限（磁盘已有缓存，内存只保留少量热条目）。
const MAX_MEM_ENTRIES: usize = 32;
/// 内存缓存的总字节上限（已编码 PNG）：磁盘缓存毫秒级可读，收紧内存 LRU 至 4 MiB。
const MAX_MEM_BYTES: usize = 4 * 1024 * 1024;

/// 排队上限：超过就直接回 `BUSY`。
///
/// 为什么不排更长：GUI 一次要几十张时，排长队的唯一效果是"每张都晚、且
/// 后台长时间满载"；立刻回 `BUSY` 让调用方在滚动/下一次可见时再要，CPU
/// 曲线和首屏响应都更好（协议里 `BUSY` 的语义就是"稍后重试"）。
pub const MAX_QUEUE: usize = 32;

/// 后台解码的占空比目标（单核百分比）：压在 ~15%，给正在播的视频留足余量；
/// 并发解码线程数（`LIUGUANG_THUMB_WORKERS` 可覆盖，1–8）。
///
/// 磁盘缓存命中不经过工作线程，只有首次建库时才会批量解码；默认单线程即可满足
/// 队列合并，且可避免多余线程堆与 glibc arena 开销。
pub const DEFAULT_WORKERS: usize = 1;

/// 启动阶段配置 glibc `mallopt`，限制 arena 数量及 mmap/trim 阈值以抑制常驻匿名页。
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
pub(crate) fn configure_allocator() {
    const M_TRIM_THRESHOLD: std::ffi::c_int = -1;
    const M_MMAP_THRESHOLD: std::ffi::c_int = -3;
    const M_ARENA_MAX: std::ffi::c_int = -8;
    unsafe extern "C" {
        fn mallopt(param: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
    }
    // SAFETY: glibc `mallopt` 对整型阈值参数安全无副作用；在启动初期单线程阶段调用。
    unsafe {
        let _ = mallopt(M_ARENA_MAX, 2);
        let _ = mallopt(M_MMAP_THRESHOLD, 131_072);
        let _ = mallopt(M_TRIM_THRESHOLD, 65_536);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn configure_allocator() {}

/// 将已释放的堆顶空闲页归还给操作系统（仅 glibc 环境生效）。
///
/// 在缩略图批解码排空、预缓存清空或长时间暂停卸载视频管线后调用，
/// 避免 4K 解码缓冲释放后仍然长期挂在进程 RSS 中。
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
pub(crate) fn trim_heap() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }
    // SAFETY: `malloc_trim(0)` 是 glibc 提供的线程安全堆收缩接口，不依赖调用方指针或生命周期。
    unsafe {
        let _ = malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn trim_heap() {}

/// 解一张歇 `wall * (1/DUTY - 1)`。
///
/// 为什么是 15% 而不是"看队列深度动态调"：串行客户端（GUI 就是一张一张要）
/// 的队列深度恒等于 1，"队列还浅 ⇒ 用户在看 ⇒ 快一点"会把 CPU 推到半个核
/// （实测 20s 均值 48%）。首屏的代价是总填充时间变长（38 张 ≈ 75s），但卡片
/// 是逐个出图的，且**每张都落盘**——第二次打开是命中缓存（毫秒级）。
const DUTY_TARGET: f64 = 0.15;

/// 单次节流休息的上限（偶发一张特别慢时别歇到天荒地老）。
const MAX_THROTTLE: Duration = Duration::from_secs(2);

/// 一次缩略图请求（也是缓存键的全部输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// 媒体路径（调用方已 canonicalize）。
    pub path: PathBuf,
    /// 视频取帧时刻（毫秒）；图片忽略。
    pub at_ms: u64,
    /// 目标最大宽。
    pub max_w: u32,
    /// 目标最大高。
    pub max_h: u32,
}

impl Request {
    /// 缓存键（`sha256(路径 \0 大小 \0 mtime_ns \0 at_ms \0 max_w \0 max_h)`）。
    ///
    /// 读不到元信息（文件不存在/权限）时返回 `None`——与"没有缓存"不同，
    /// 这是可以让调用方直接报错的信号。
    #[must_use]
    pub fn key(&self) -> Option<String> {
        let meta = std::fs::metadata(&self.path).ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let canonical = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        let raw = format!(
            "{}\0{}\0{mtime}\0{}\0{}\0{}",
            canonical.display(),
            meta.len(),
            self.at_ms,
            self.max_w,
            self.max_h
        );
        Some(sha256_hex(raw.as_bytes()))
    }
}

/// 计数器（`get_metrics` 的 `thumb_*` 字段）。
#[derive(Debug, Default)]
pub struct Counters {
    requests: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    decoded: AtomicU64,
    decode_us: AtomicU64,
    queue_depth: AtomicU32,
}

/// [`Counters`] 的一致快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// 累计请求数。
    pub requests: u64,
    /// 缓存命中（含同 key 在途合并）。
    pub hits: u64,
    /// 缓存未命中。
    pub misses: u64,
    /// 实际解码次数。
    pub decoded: u64,
    /// 单次解码平均墙钟耗时（毫秒）。
    pub avg_ms: u64,
    /// 当前排队任务数。
    pub queue_depth: u32,
}

impl Counters {
    /// 读一次快照。
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let decoded = self.decoded.load(Ordering::Relaxed);
        let total_us = self.decode_us.load(Ordering::Relaxed);
        Snapshot {
            requests: self.requests.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            decoded,
            avg_ms: total_us.checked_div(decoded).map_or(0, |us| us / 1000),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
        }
    }
}

/// [`Service::submit`] 的三种答案。
pub enum Submit {
    /// 缓存命中（或参数立刻可判错）：直接回这条应答。
    Ready(liuguang_ipc::Response),
    /// 已排队/已在解：连接线程等这个接收端（运行时线程不阻塞）。
    Deferred(Receiver<liuguang_ipc::Response>),
    /// 队列满：立刻回可操作的 `BUSY`。
    Busy(liuguang_ipc::Response),
}

/// 一条缩略图任务。
#[derive(Debug)]
struct Job {
    key: String,
    request: Request,
}

/// 等同一个 key 的调用方。
struct Waiter {
    request_id: String,
    tx: Sender<liuguang_ipc::Response>,
}

/// 内存缓存条目（存**已编码 PNG**，命中时只做 base64）。
struct MemEntry {
    bytes: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    used: u64,
}

/// 缓存与队列的共享状态。
struct State {
    mem: HashMap<String, MemEntry>,
    stamp: u64,
    waiters: HashMap<String, Vec<Waiter>>,
    queue: std::collections::VecDeque<Job>,
    inflight: std::collections::HashSet<String>,
}

/// 解码结果（编码后的 PNG + 尺寸）。
struct Decoded {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}

/// 缩略图服务：两级缓存 + 一条串行解码线程。
pub struct Service {
    state: Arc<Mutex<State>>,
    wake: Arc<Condvar>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    counters: Arc<Counters>,
    dir: PathBuf,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Service {
    /// 用默认缓存目录（`$XDG_CACHE_HOME/liuguang/thumbs`）起服务。
    #[must_use]
    pub fn new() -> Self {
        Self::with_dir(cache_dir())
    }

    /// 用指定缓存目录起服务（测试用；生产走 [`Service::new`]）。
    #[must_use]
    pub fn with_dir(dir: PathBuf) -> Self {
        let state = Arc::new(Mutex::new(State {
            mem: HashMap::new(),
            stamp: 0,
            waiters: HashMap::new(),
            queue: std::collections::VecDeque::new(),
            inflight: std::collections::HashSet::new(),
        }));
        let wake = Arc::new(Condvar::new());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let count = worker_count();
        let mut workers = Vec::with_capacity(count);
        for index in 0..count {
            let state = Arc::clone(&state);
            let wake = Arc::clone(&wake);
            let stop = Arc::clone(&stop);
            let counters = Arc::clone(&counters);
            let dir = dir.clone();
            if let Ok(handle) = std::thread::Builder::new()
                .name(format!("liuguang-thumb-{index}"))
                .spawn(move || worker_loop(&state, &wake, &stop, &counters, &dir))
            {
                workers.push(handle);
            }
        }
        Self {
            state,
            wake,
            stop,
            counters,
            dir,
            workers,
        }
    }

    /// 计数器。
    #[must_use]
    pub fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    /// 提交一次缩略图请求。
    ///
    /// `request_id` 是协议里的请求 id（回包要原样带回）。
    pub fn submit(&self, request: Request, request_id: &str) -> Submit {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let media = request.path.to_string_lossy().into_owned();
        let Some(key) = request.key() else {
            return Submit::Ready(liuguang_ipc::Response::err(
                request_id,
                liuguang_ipc::ErrorBody::new(
                    liuguang_ipc::ErrorKind::FileUnavailable,
                    format!("无法读取 {media}（文件不存在或没有权限）"),
                ),
            ));
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stamp += 1;
        let stamp = state.stamp;
        if let Some(entry) = state.mem.get_mut(&key) {
            entry.used = stamp;
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            let encoded = Arc::clone(&entry.bytes);
            let (width, height) = (entry.width, entry.height);
            drop(state);
            return Submit::Ready(ok_response(request_id, &media, &encoded, width, height));
        }
        // 同一个 key 已经在解：合并成一次解码，各自拿一份应答。
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(waiters) = state.waiters.get_mut(&key) {
            waiters.push(Waiter {
                request_id: request_id.to_string(),
                tx,
            });
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            return Submit::Deferred(rx);
        }
        // 磁盘缓存：释放互斥锁后再读盘（带大小上限，防止大文件阻塞全池/OOM）。
        let path = self.dir.join(format!("{key}.png"));
        drop(state);
        let disk_bytes = read_cached_png(&path, MAX_THUMB_PNG_BYTES);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stamp += 1;
        let stamp = state.stamp;
        if let Some(entry) = state.mem.get_mut(&key) {
            entry.used = stamp;
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            let encoded = Arc::clone(&entry.bytes);
            let (width, height) = (entry.width, entry.height);
            drop(state);
            return Submit::Ready(ok_response(request_id, &media, &encoded, width, height));
        }
        if let Some(waiters) = state.waiters.get_mut(&key) {
            waiters.push(Waiter {
                request_id: request_id.to_string(),
                tx,
            });
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            return Submit::Deferred(rx);
        }
        if let Some((bytes, width, height)) = disk_bytes {
            let bytes = Arc::new(bytes);
            state.mem.insert(
                key.clone(),
                MemEntry {
                    bytes: Arc::clone(&bytes),
                    width,
                    height,
                    used: stamp,
                },
            );
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            evict(&mut state);
            drop(state);
            return Submit::Ready(ok_response(request_id, &media, &bytes, width, height));
        }
        self.counters.misses.fetch_add(1, Ordering::Relaxed);
        if state.queue.len() + state.inflight.len() >= MAX_QUEUE {
            let depth = state.queue.len() + state.inflight.len();
            drop(state);
            return Submit::Busy(liuguang_ipc::Response::err(
                request_id,
                liuguang_ipc::ErrorBody::new(
                    liuguang_ipc::ErrorKind::Busy,
                    format!(
                        "缩略图队列已满（{depth}/{MAX_QUEUE}）：{media} 需要解码，请稍后重试（滚动回来或重新打开面板即可，不必反复连点）"
                    ),
                ),
            ));
        }
        state.waiters.insert(
            key.clone(),
            vec![Waiter {
                request_id: request_id.to_string(),
                tx,
            }],
        );
        state.inflight.insert(key.clone());
        state.queue.push_back(Job { key, request });
        self.counters
            .queue_depth
            .store((state.queue.len()) as u32, Ordering::Relaxed);
        drop(state);
        self.wake.notify_one();
        Submit::Deferred(rx)
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.notify_all();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// 解码线程数：`LIUGUANG_THUMB_WORKERS` 可覆盖（1–8，非法值回落默认）。
fn worker_count() -> usize {
    std::env::var("LIUGUANG_THUMB_WORKERS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .map_or(DEFAULT_WORKERS, |n| n.clamp(1, 8))
}

/// 后台解码线程：共享队列、按需拉起短命 GStreamer 子进程、按占空比节流。
fn worker_loop(
    state: &Arc<Mutex<State>>,
    wake: &Arc<Condvar>,
    stop: &Arc<std::sync::atomic::AtomicBool>,
    counters: &Arc<Counters>,
    dir: &Path,
) {
    let mut gst_child: Option<ThumbChild> = None;
    let mut idle_since = Instant::now();
    loop {
        let job = {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if stop.load(Ordering::Relaxed) {
                    drop(guard);
                    drop(gst_child.take());
                    return;
                }
                if let Some(job) = guard.queue.pop_front() {
                    counters
                        .queue_depth
                        .store(guard.queue.len() as u32, Ordering::Relaxed);
                    break job;
                }
                counters.queue_depth.store(0, Ordering::Relaxed);
                let idle_expired = gst_child.is_some()
                    && guard.inflight.is_empty()
                    && idle_since.elapsed() >= CHILD_IDLE_TIMEOUT;
                if idle_expired {
                    drop(guard);
                    drop(gst_child.take());
                    trim_heap();
                    guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                guard = match wake.wait_timeout(guard, Duration::from_millis(500)) {
                    Ok((guard, _)) => guard,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
        };
        let started = Instant::now();
        let outcome = decode_and_encode_with_child(&job.request, &mut gst_child);
        idle_since = Instant::now();
        let elapsed = started.elapsed();
        counters.decoded.fetch_add(1, Ordering::Relaxed);
        counters
            .decode_us
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);

        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        guard.inflight.remove(&job.key);
        let waiters = guard.waiters.remove(&job.key).unwrap_or_default();
        let media = job.request.path.to_string_lossy().into_owned();
        match outcome {
            Ok(decoded) => {
                // 先落盘（原子替换），再回包：下次启动直接命中。
                if let Err(error) = write_cache(dir, &job.key, &decoded.bytes) {
                    tracing::debug!(error = %error, key = %job.key, "缩略图磁盘缓存写入失败");
                }
                guard.stamp += 1;
                let used = guard.stamp;
                guard.mem.insert(
                    job.key.clone(),
                    MemEntry {
                        bytes: Arc::new(decoded.bytes.clone()),
                        width: decoded.width,
                        height: decoded.height,
                        used,
                    },
                );
                evict(&mut guard);
                let bytes = Arc::new(decoded.bytes);
                for waiter in waiters {
                    let _ = waiter.tx.send(ok_response(
                        &waiter.request_id,
                        &media,
                        &bytes,
                        decoded.width,
                        decoded.height,
                    ));
                }
            }
            Err(error) => {
                for waiter in waiters {
                    let _ = waiter.tx.send(liuguang_ipc::Response::err(
                        &waiter.request_id,
                        liuguang_ipc::ErrorBody::new(
                            liuguang_ipc::ErrorKind::FileUnavailable,
                            format!("缩略图解码失败（{media}）: {error}"),
                        ),
                    ));
                }
            }
        }
        let drained = guard.queue.is_empty() && guard.inflight.is_empty();
        drop(guard);
        if drained {
            trim_heap();
        }

        // 占空比节流：解多久就歇 (1/DUTY - 1) 倍。
        let restore = elapsed.mul_f64((1.0 / DUTY_TARGET) - 1.0);
        if !restore.is_zero() {
            std::thread::sleep(restore.min(MAX_THROTTLE));
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ChildThumbReq {
    path: PathBuf,
    max_w: u32,
    max_h: u32,
    at_ms: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ChildThumbResp {
    ok: bool,
    width: u32,
    height: u32,
    png_b64: String,
    error: String,
}

struct ThumbChild {
    stdin: Option<std::io::BufWriter<std::process::ChildStdin>>,
    stdout_rx: Receiver<std::io::Result<Option<String>>>,
    child: std::process::Child,
}

impl ThumbChild {
    fn spawn() -> Option<Self> {
        if cfg!(test) {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let mut child = std::process::Command::new(exe)
            .arg(INTERNAL_THUMB_WORKER_ARG)
            .env("MALLOC_ARENA_MAX", "1")
            .env("MALLOC_MMAP_THRESHOLD_", "65536")
            .env("MALLOC_TRIM_THRESHOLD_", "32768")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let stdin = std::io::BufWriter::new(child.stdin.take()?);
        let stdout = child.stdout.take()?;
        let (tx, stdout_rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("liuguang-thumb-child-rx".into())
            .spawn(move || {
                use std::io::BufRead as _;
                let mut reader = std::io::BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            let _ = tx.send(Ok(None));
                            break;
                        }
                        Ok(_) => {
                            if tx.send(Ok(Some(line))).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err));
                            break;
                        }
                    }
                }
            })
            .is_err()
        {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Some(Self {
            stdin: Some(stdin),
            stdout_rx,
            child,
        })
    }

    fn request(&mut self, req: &Request) -> Result<Decoded, String> {
        use std::io::Write as _;
        let payload = ChildThumbReq {
            path: req.path.clone(),
            max_w: req.max_w,
            max_h: req.max_h,
            at_ms: req.at_ms,
        };
        let Some(stdin) = self.stdin.as_mut() else {
            return Err("缩略图子进程标准输入已关闭".to_string());
        };
        let serialized = serde_json::to_string(&payload)
            .map_err(|e| format!("序列化缩略图子进程请求失败: {e}"))?;
        stdin
            .write_all(serialized.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("向缩略图子进程写入请求失败: {e}"))?;
        let line = match self.stdout_rx.recv_timeout(CHILD_REQUEST_TIMEOUT) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => return Err("缩略图子进程意外退出".to_string()),
            Ok(Err(e)) => return Err(format!("读取缩略图子进程响应失败: {e}")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                return Err(format!(
                    "读取缩略图子进程响应超时（>{}s）",
                    CHILD_REQUEST_TIMEOUT.as_secs()
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("缩略图子进程响应通道意外关闭".to_string());
            }
        };
        let resp: ChildThumbResp =
            serde_json::from_str(&line).map_err(|e| format!("解析缩略图子进程响应失败: {e}"))?;
        if !resp.ok {
            return Err(resp.error);
        }
        let bytes = liuguang_ipc::base64::decode(&resp.png_b64)
            .ok_or_else(|| "缩略图子进程返回的 base64 非法".to_string())?;
        Ok(Decoded {
            bytes,
            width: resp.width,
            height: resp.height,
        })
    }
}

impl Drop for ThumbChild {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

/// 缩略图隔离工作子进程主循环：从 stdin 逐行接收请求并回写已编码 PNG。
#[must_use]
pub fn run_internal_worker_main() -> u8 {
    use std::io::{BufRead as _, Write as _};
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<ChildThumbReq>(&line) {
            Ok(req) => {
                let r = Request {
                    path: req.path,
                    at_ms: req.at_ms,
                    max_w: req.max_w,
                    max_h: req.max_h,
                };
                match encode_job_in_process(&r) {
                    Ok(decoded) => ChildThumbResp {
                        ok: true,
                        width: decoded.width,
                        height: decoded.height,
                        png_b64: liuguang_ipc::base64::encode(&decoded.bytes),
                        error: String::new(),
                    },
                    Err(error) => ChildThumbResp {
                        ok: false,
                        width: 0,
                        height: 0,
                        png_b64: String::new(),
                        error,
                    },
                }
            }
            Err(error) => ChildThumbResp {
                ok: false,
                width: 0,
                height: 0,
                png_b64: String::new(),
                error: format!("参数解析失败: {error}"),
            },
        };
        if serde_json::to_writer(&mut out, &resp).is_err()
            || out.write_all(b"\n").is_err()
            || out.flush().is_err()
        {
            break;
        }
        trim_heap();
    }
    0
}

fn decode_and_encode_with_child(
    request: &Request,
    gst_child: &mut Option<ThumbChild>,
) -> Result<Decoded, String> {
    if is_image(&request.path)
        && let Some(thumb) = decode_png_direct(&request.path, request.max_w, request.max_h)?
    {
        return encode_thumbnail_bounded(thumb);
    }
    if !cfg!(test) {
        for attempt in 0..2 {
            if gst_child.is_none() {
                *gst_child = ThumbChild::spawn();
            }
            let Some(child) = gst_child.as_mut() else {
                break;
            };
            match child.request(request) {
                Ok(decoded) => return Ok(decoded),
                Err(error) => {
                    let pipe_broken = error.contains("子进程");
                    if !pipe_broken || attempt == 1 {
                        if pipe_broken {
                            *gst_child = None;
                        }
                        return Err(error);
                    }
                    *gst_child = None;
                }
            }
        }
    }
    encode_job_in_process(request)
}

fn encode_job_in_process(request: &Request) -> Result<Decoded, String> {
    let thumb = decode(request)?;
    encode_thumbnail_bounded(thumb)
}

fn encode_thumbnail_bounded(mut thumb: Thumbnail) -> Result<Decoded, String> {
    let mut bytes = encode_png(&thumb.rgba, thumb.width, thumb.height)?;
    for _ in 0..3 {
        if bytes.len() <= MAX_THUMB_PNG_BYTES || (thumb.width <= 128 && thumb.height <= 128) {
            break;
        }
        let next_w = (thumb.width * 3 / 4).max(1);
        let next_h = (thumb.height * 3 / 4).max(1);
        thumb = liuguang_media::thumbnail::scale_to_fit(thumb, next_w, next_h);
        bytes = encode_png(&thumb.rgba, thumb.width, thumb.height)?;
    }
    if bytes.len() > MAX_THUMB_PNG_BYTES {
        return Err(format!(
            "缩略图编码体积 {}B 超过 IPC 上限 {MAX_THUMB_PNG_BYTES}B",
            bytes.len()
        ));
    }
    Ok(Decoded {
        bytes,
        width: thumb.width,
        height: thumb.height,
    })
}

/// 取帧 + 按目标尺寸缩放（PNG 走纯 Rust，视频/其余图片走管线内缩放）。
fn decode(request: &Request) -> Result<Thumbnail, String> {
    if is_image(&request.path) {
        if let Some(thumb) = decode_png_direct(&request.path, request.max_w, request.max_h)? {
            return Ok(thumb);
        }
        return liuguang_media::thumbnail::extract(&request.path, request.max_w, request.max_h)
            .map_err(|e| e.to_string());
    }
    let kind = "video";
    match crate::probe::probe(&request.path, kind) {
        Some(details) if details.width > 0 && details.height > 0 => {
            let (width, height) =
                fit_within(details.width, details.height, request.max_w, request.max_h);
            imp::extract_fitted(&request.path, width, height, request.at_ms)
        }
        // 容器头不认识（罕见格式）：退回"整帧再缩"，仍能出图。
        _ => {
            let thumb = extract_at(&request.path, request.max_w, request.max_h, request.at_ms)?;
            Ok(liuguang_media::thumbnail::scale_to_fit(
                thumb,
                request.max_w,
                request.max_h,
            ))
        }
    }
}

/// 等比缩放到不超过 `max_w × max_h`（沿用既有行为：不放大、不变形）。
#[must_use]
pub fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (max_w.max(1), max_h.max(1));
    }
    let scale = (f64::from(max_w) / f64::from(src_w)).min(f64::from(max_h) / f64::from(src_h));
    let width = (f64::from(src_w) * scale).round() as u32;
    let height = (f64::from(src_h) * scale).round() as u32;
    (width.clamp(1, max_w), height.clamp(1, max_h))
}

/// 缩略图磁盘缓存目录。
#[must_use]
pub fn cache_dir() -> PathBuf {
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
        if !base.is_empty() {
            return PathBuf::from(base).join("liuguang").join("thumbs");
        }
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".cache").join("liuguang").join("thumbs")
}

/// 组装成功应答（`GetThumbnailResult` + base64）。
fn ok_response(
    request_id: &str,
    media: &str,
    bytes: &Arc<Vec<u8>>,
    width: u32,
    height: u32,
) -> liuguang_ipc::Response {
    let payload = liuguang_ipc::protocol::GetThumbnailResult {
        id: media.to_string(),
        width,
        height,
        format: "png".to_string(),
        data_b64: liuguang_ipc::base64::encode(bytes),
    };
    let value = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
    liuguang_ipc::Response::ok(request_id, value)
}

/// PNG 编码（RGBA8）。
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG 头写入失败: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("PNG 数据写入失败: {e}"))?;
    }
    Ok(out)
}

/// 从 PNG 头读尺寸（磁盘缓存命中时用，避免再解码）。
fn png_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes.get(..8)? != SIGNATURE {
        return None;
    }
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((width, height))
}

/// 单条缩略图 PNG 最大允许字节（保证 base64 + JSON 封装低于 1 MiB IPC 上限）。
const MAX_THUMB_PNG_BYTES: usize = 700 * 1024;

fn read_cached_png(path: &Path, limit: usize) -> Option<(Vec<u8>, u32, u32)> {
    use std::io::Read as _;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > limit as u64 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > limit {
        return None;
    }
    let (w, h) = png_dims(&bytes)?;
    Some((bytes, w, h))
}

/// 原子写缓存（`O_EXCL` 临时文件 → rename），失败清理临时文件。
fn write_cache(dir: &Path, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{key}.png"));
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let part = dir.join(format!(".{key}.{}.{seq}.png.part", std::process::id()));
    let write_res = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&part, &path)?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(())
}

/// 淘汰最久未用的条目，直到同时满足「条数」与「字节」两个上限。
fn evict(state: &mut State) {
    let mut bytes: usize = state.mem.values().map(|entry| entry.bytes.len()).sum();
    while state.mem.len() > MAX_MEM_ENTRIES || bytes > MAX_MEM_BYTES {
        let Some(oldest) = state
            .mem
            .iter()
            .min_by_key(|(_, entry)| entry.used)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        if let Some(removed) = state.mem.remove(&oldest) {
            bytes = bytes.saturating_sub(removed.bytes.len());
        } else {
            break;
        }
    }
}

/// SHA-256（十六进制小写）。只用来做缓存文件名，不参与任何安全判断。
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate().take(16) {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{extract_at, is_image};
    use std::path::Path;

    // ===== t39：缓存 / 队列 / 尺寸 =====

    #[test]
    fn fit_within_keeps_aspect_and_never_upscales() {
        assert_eq!(super::fit_within(1920, 1080, 400, 250), (400, 225));
        assert_eq!(super::fit_within(1080, 1920, 400, 250), (141, 250));
        assert_eq!(super::fit_within(320, 200, 400, 250), (400, 250));
        assert_eq!(super::fit_within(0, 0, 400, 250), (400, 250));
    }

    #[test]
    fn png_dims_reads_the_ihdr_header() {
        let png = super::encode_png(&[255, 0, 0, 255], 1, 1).expect("编码");
        assert_eq!(super::png_dims(&png), Some((1, 1)));
        assert_eq!(super::png_dims(b"not a png"), None);
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            super::sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// 缓存键对"路径 / 大小 / mtime / at_ms / 目标尺寸"敏感：任何一项变化都换 key。
    #[test]
    fn request_key_is_stable_and_sensitive() {
        let dir = std::env::temp_dir().join(format!("liuguang-t39-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("a.png");
        let base = super::Request {
            path: path.clone(),
            at_ms: 0,
            max_w: 400,
            max_h: 250,
        };
        // 文件不存在时没有 key（可执行文件缺失/权限不足的信号）。
        assert!(base.key().is_none(), "无文件时不该算出 key");
        std::fs::write(&path, b"x").expect("写文件");
        let first = base.key().expect("有文件就有 key");
        assert_eq!(first, base.key().expect("同输入必须同 key"));
        assert_ne!(
            first,
            super::Request {
                at_ms: 1000,
                ..base.clone()
            }
            .key()
            .expect("key")
        );
        assert_ne!(
            first,
            super::Request {
                max_w: 320,
                ..base.clone()
            }
            .key()
            .expect("key")
        );
        std::fs::write(&path, b"yy").expect("改大小");
        assert_ne!(first, base.key().expect("大小变了 key 必须变"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 磁盘缓存命中：**不解码**也返回 PNG（读文件 + 读 IHDR 就够）。
    #[test]
    fn disk_cache_hit_is_served_without_decoding() {
        let dir = std::env::temp_dir().join(format!("liuguang-t39-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        let media_dir = dir.join("media");
        std::fs::create_dir_all(&media_dir).expect("建媒体目录");
        let file = media_dir.join("cover.png");
        // 不是合法图片：只要缓存命中就不会去解码，因此不该报错。
        std::fs::write(&file, b"definitely-not-a-png").expect("写测试文件");
        let request = super::Request {
            path: file,
            at_ms: 0,
            max_w: 400,
            max_h: 250,
        };
        let key = request.key().expect("key");
        let png = super::encode_png(&[10, 20, 30, 255], 1, 1).expect("编码");
        std::fs::create_dir_all(&dir).expect("缓存目录");
        std::fs::write(dir.join(format!("{key}.png")), &png).expect("写缓存");

        let service = super::Service::with_dir(dir.clone());
        match service.submit(request, "req-1") {
            super::Submit::Ready(resp) => {
                assert!(resp.ok, "缓存命中必须成功: {resp:?}");
                let body = resp.result.expect("带结果");
                assert_eq!(body["format"], "png");
                assert_eq!(body["width"], 1);
                assert_eq!(body["height"], 1);
                assert_eq!(body["id"], body["id"]);
            }
            _ => panic!("磁盘缓存命中必须立刻 Ready，而不是排队"),
        }
        let snapshot = service.counters().snapshot();
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.hits, 1, "磁盘缓存算命中");
        assert_eq!(snapshot.decoded, 0, "命中不得解码");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一个 key 的两张请求合并成一次解码（in-flight 合并）。
    #[test]
    fn concurrent_requests_for_one_key_share_a_single_decode() {
        let dir = std::env::temp_dir().join(format!("liuguang-t39-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        // 慢素材（大图）先占住一条解码线程，保证后两张一定落在"在途"分支上；
        // 多线程解码（[`super::DEFAULT_WORKERS`]）下它不会阻塞 fast，所以下面
        // 两张 fast 请求仍然合并成一次解码，而 slow 自己再解一次。
        let slow = dir.join("slow.png");
        write_large_png(&slow, 1200);
        let fast = dir.join("fast.png");
        write_large_png(&fast, 64);

        let service = super::Service::with_dir(dir.join("cache"));
        let slow_req = super::Request {
            path: slow,
            at_ms: 0,
            max_w: 400,
            max_h: 250,
        };
        let slow = service.submit(slow_req, "req-slow");
        let request = super::Request {
            path: fast,
            at_ms: 0,
            max_w: 400,
            max_h: 250,
        };
        let first = service.submit(request.clone(), "req-a");
        let second = service.submit(request, "req-b");
        let mut answers = 0;
        for submit in [first, second] {
            match submit {
                super::Submit::Deferred(rx) => {
                    let resp = rx
                        .recv_timeout(std::time::Duration::from_secs(30))
                        .expect("必须在期限内返回");
                    assert!(resp.ok, "解码成功: {resp:?}");
                    answers += 1;
                }
                super::Submit::Ready(resp) => {
                    assert!(resp.ok);
                    answers += 1;
                }
                super::Submit::Busy(resp) => panic!("队列不该满: {resp:?}"),
            }
        }
        assert_eq!(answers, 2);
        // 慢素材也必须解完，否则计数会随调度抖动（多线程下 fast 可能先完成）。
        match slow {
            super::Submit::Deferred(rx) => {
                let resp = rx
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .expect("慢素材必须在期限内返回");
                assert!(resp.ok, "慢素材解码成功: {resp:?}");
            }
            super::Submit::Busy(resp) => panic!("慢素材不该被拒绝: {resp:?}"),
            super::Submit::Ready(resp) => panic!("慢素材应当排队而不是立刻 Ready: {resp:?}"),
        }
        let snapshot = service.counters().snapshot();
        assert_eq!(
            snapshot.decoded, 2,
            "slow + fast 各解一次（fast 的两张请求必须合并）"
        );
        assert!(snapshot.hits >= 1, "合并/缓存至少要记一次命中");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写一张真实的大 PNG（用来占住解码线程）。
    fn write_large_png(path: &std::path::Path, side: u32) {
        let rgba = vec![7u8; (side as usize) * (side as usize) * 4];
        let bytes = super::encode_png(&rgba, side, side).expect("编码大图");
        std::fs::write(path, bytes).expect("写大图");
    }

    #[test]
    fn queue_bound_is_documented_by_max_queue() {
        assert!(
            (1..=64).contains(&super::MAX_QUEUE),
            "队列深度上限必须落在 1..=16"
        );
    }

    #[test]
    fn image_extensions_are_recognized_case_insensitively() {
        assert!(is_image(Path::new("/m/a.PNG")));
        assert!(is_image(Path::new("/m/a.jpeg")));
        assert!(is_image(Path::new("/m/a.webp")));
        assert!(!is_image(Path::new("/m/a.mp4")));
        assert!(!is_image(Path::new("/m/noext")));
    }

    /// 图片的 `at_ms` 被忽略：传任何时刻都应当走既有实现（这里是"文件不存在"的
    /// 错误路径，证明它确实进了那条分支而不是 seek 分支）。
    #[test]
    fn image_ignores_at_ms_and_takes_the_plain_path() {
        let err = extract_at(Path::new("/nonexistent/封面.png"), 320, 200, 12_345)
            .expect_err("不存在的文件必须报错");
        assert!(
            !err.contains("定点缩略图"),
            "图片不该走 seek 分支，实际错误: {err}"
        );
    }

    /// 视频 `at_ms = 0` 也是首帧路径（不该要求 gstreamer 才给结果）。
    #[test]
    fn zero_at_ms_for_video_takes_the_plain_path() {
        let err = extract_at(Path::new("/nonexistent/clip.mp4"), 320, 200, 0)
            .expect_err("不存在的文件必须报错");
        assert!(
            !err.contains("定点缩略图"),
            "at_ms=0 应走首帧路径，实际错误: {err}"
        );
    }

    /// 不带 gstreamer 构建时，视频定点取帧必须给出**明确**的 feature 提示，
    /// 而不是一句笼统的"解码失败"。
    #[cfg(not(feature = "gstreamer"))]
    #[test]
    fn video_seek_without_gstreamer_explains_the_feature() {
        let err = extract_at(Path::new("/nonexistent/clip.mp4"), 320, 200, 5_000)
            .expect_err("没有解码器时无法取帧");
        assert!(err.contains("--features gstreamer"), "{err}");
        assert!(
            err.contains("5000"),
            "错误里要带上请求的时刻，便于定位: {err}"
        );
    }

    /// t10 回归：视频定点取帧的失败路径必须报到**解码/预滚**这一层，而不是
    /// 之前那句误导人的"管线无法预滚到 PAUSED"（那是漏了 set_state 的自伤）。
    #[cfg(feature = "gstreamer")]
    #[test]
    fn video_seek_on_missing_file_reports_decode_error() {
        let err = extract_at(Path::new("/nonexistent/clip.mp4"), 320, 200, 1_000)
            .expect_err("不存在的文件必须报错");
        assert!(
            err.contains("解码失败") || err.contains("预滚"),
            "缺失文件的错误应指向解码/预滚层，实际: {err}"
        );
        assert!(
            !err.contains("管线无法预滚到 PAUSED"),
            "不该再出现 t10 之前的误判文案: {err}"
        );
    }
}
