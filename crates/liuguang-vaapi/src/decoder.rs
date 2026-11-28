//! FFmpeg 解封装 + VA-API 解码：只解出**表面 id**，不做任何像素拷贝。
//!
//! 为什么不用 GStreamer：NVIDIA 的 `gst-va` 解码器 src caps 只有 `memory:VAMemory`
//! 与普通 `video/x-raw`，**没有 `memory:DMABuf`**；而 libva 的 `vaExportSurfaceHandle`
//! 可以直接把解码表面导出成 dma-buf。这是本 crate 存在的唯一理由。
use std::collections::HashMap;
use std::ffi::{CString, c_int};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::{VaapiError, ffi};

// 打开输入流时用来限制探测预算（t47）。放在本模块是为了把改动收在
// `decoder.rs` 里；签名与 `ffi.rs` 里其它 `av_*` 声明一致。
unsafe extern "C" {
    fn av_dict_set(
        dictionary: *mut *mut ffi::AvDictionary,
        key: *const std::ffi::c_char,
        value: *const std::ffi::c_char,
        flags: c_int,
    ) -> c_int;
    fn av_dict_free(dictionary: *mut *mut ffi::AvDictionary);
}

/// 从解码器取出的一帧表面（持有 `AVFrame`；`Drop` 时归还表面给解码器池）。
pub struct DecodedSurface {
    /// 关联的 `AVFrame`（保持引用计数 ⇒ 表面不会被复用）。
    frame: *mut ffi::AvFrame,
    /// 表面 id（只在创建它的 `VADisplay` 内有效）。
    surface: ffi::VaSurfaceId,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
    /// 媒体呈现时间。
    pub pts: Duration,
}

// SAFETY: `AVFrame` 的引用计数由 ffmpeg 内部原子维护；本项目里该结构在媒体线程创建，
// 在合成器 `release` 时由 Wayland 线程 drop。`av_frame_free` 归还的是 ffmpeg
// `AVBufferPool` 里的条目，池本身线程安全（mpv 等同款用法）。
unsafe impl Send for DecodedSurface {}
unsafe impl Sync for DecodedSurface {}

impl DecodedSurface {
    /// 表面 id。
    #[must_use]
    pub fn surface_id(&self) -> ffi::VaSurfaceId {
        self.surface
    }
}

impl std::fmt::Debug for DecodedSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedSurface")
            .field("surface", &self.surface)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pts", &self.pts)
            .finish()
    }
}

impl Drop for DecodedSurface {
    fn drop(&mut self) {
        if self.frame.is_null() {
            return;
        }
        let mut frame = self.frame;
        // SAFETY: frame 由 `av_frame_alloc` 创建且本结构独占；置空避免二次释放。
        unsafe {
            ffi::av_frame_free(&raw mut frame);
        }
        self.frame = std::ptr::null_mut();
    }
}

/// 解码器的静态信息（给上层构造 `MediaInfo`）。
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    /// 帧宽（来自容器）。
    pub width: u32,
    /// 帧高（来自容器）。
    pub height: u32,
    /// 源帧率（容器没给则为 `None`）。
    pub source_fps: Option<f64>,
}

/// VA-API 解码器（一个文件一条管线）。
pub struct VaapiDecoder {
    /// 解码用的 `VADisplay`（由 ffmpeg 的 HW 设备上下文持有；导出表面也用它）。
    display: ffi::VaDisplay,
    /// 我们持有的 HW 设备引用（解码上下文另有自己的一份引用）。
    device_ref: *mut ffi::AvBufferRef,
    format: *mut ffi::AvFormatContext,
    codec: *mut ffi::AvCodecContext,
    packet: *mut ffi::AvPacket,
    /// 每轮 `receive_frame` 复用的帧；真的产出时会被换成新分配的。
    scratch: *mut ffi::AvFrame,
    stream_index: c_int,
    /// 流时基（秒/单位）。
    time_base: f64,
    /// 已冲刷：之后只会拿到 EOF。
    flushed: bool,
    width: u32,
    height: u32,
    source_fps: Option<f64>,
    /// 诊断用：实际使用的解码器名（`h264` 等）。
    codec_name: String,
}

// SAFETY: 见 [`DecodedSurface`] 的说明；解码器本身只在媒体线程使用。
unsafe impl Send for VaapiDecoder {}

/// 打开成本的分阶段计时（`LIUGUANG_VAAPI_PROFILE=1` 时打到 stderr）。
///
/// 只在诊断时启用：正常路径只多一次 `var_os` 查询，不做任何分配。
fn profiling() -> bool {
    std::env::var_os("LIUGUANG_VAAPI_PROFILE").is_some()
}

fn profile_stage(label: &str, elapsed: std::time::Duration) {
    if profiling() {
        eprintln!(
            "vaapi-open {label}: {:.1}ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }
}

/// 进程内按渲染节点共享的 VA-API 设备。
///
/// 每次 `av_hwdevice_ctx_create` 会付一次 `vaInitialize`（外加驱动侧上下文初始化，真机实测
/// 220–400ms）；切换壁纸时这笔钱完全白付，因为设备本身对同一节点是可复用的。这里按节点缓存
/// 一份 `AVBufferRef`，各解码器通过 [`shared_device`] 拿**自己的一份引用**（`av_buffer_ref`），
/// `Drop` 只 unref 自己那份；共享的那份活到进程退出。
///
/// # 生命周期（t47）
///
/// * 解码器 `Drop` 里**只** unref 自己的引用，绝不 `vaTerminate`——那会提前拆掉别人的显示
///   （t43 崩溃的同类事故，见 `convert.rs` 里 `ImageCache` 的注释）。
/// * `av_hwdevice_ctx_create` 在**锁外**执行：它要 220–400ms，持锁会让预读线程/主线程排队。
///   并发首次打开可能各自建一份，但只有第一份进缓存，其余立刻丢弃，等价于"只保留一份"。
struct SharedDevice {
    reference: *mut ffi::AvBufferRef,
}

// SAFETY: ffmpeg 的 `AVBufferRef` 引用计数是原子操作，VA-API 设备上下文本身被设计为可跨
// 线程共享（每个解码器各持一份引用）；本结构只在 `Mutex` 里被替换/读取，没有内部可变状态。
unsafe impl Send for SharedDevice {}

impl Drop for SharedDevice {
    fn drop(&mut self) {
        if !self.reference.is_null() {
            // SAFETY: reference 由 av_hwdevice_ctx_create/av_buffer_ref 产生，且只被这里 unref 一次。
            unsafe { ffi::av_buffer_unref(&raw mut self.reference) };
        }
    }
}

fn devices() -> &'static Mutex<HashMap<PathBuf, SharedDevice>> {
    static DEVICES: OnceLock<Mutex<HashMap<PathBuf, SharedDevice>>> = OnceLock::new();
    DEVICES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取一份该渲染节点的设备引用；调用方拥有它，`Drop` 时用 `av_buffer_unref` 释放自己那份。
///
/// 错误语义与直接调用 `av_hwdevice_ctx_create` 完全一致（同样的 `VaapiError::Ffmpeg`
/// 文案），上层 `vaapi_rejection_is_permanent` 等分类不受影响。
fn shared_device(render_node: &Path) -> Result<*mut ffi::AvBufferRef, VaapiError> {
    let key = render_node.to_path_buf();
    if let Ok(cache) = devices().lock()
        && let Some(entry) = cache.get(&key)
    {
        // SAFETY: entry.reference 非空且缓存持有它；av_buffer_ref 增加引用计数。
        let reference = unsafe { ffi::av_buffer_ref(entry.reference) };
        if reference.is_null() {
            return Err(VaapiError::Ffmpeg("av_buffer_ref 失败".into()));
        }
        return Ok(reference);
    }

    let c_node = CString::new(render_node.to_string_lossy().as_bytes())
        .map_err(|_| VaapiError::Unsupported("渲染节点路径含 NUL 字节".into()))?;
    let mut created: *mut ffi::AvBufferRef = std::ptr::null_mut();
    // SAFETY: c_node 以 NUL 结尾；created 是本地可写指针。
    let created_status = unsafe {
        ffi::av_hwdevice_ctx_create(
            &raw mut created,
            ffi::AV_HWDEVICE_TYPE_VAAPI,
            c_node.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if created_status < 0 || created.is_null() {
        return Err(VaapiError::Ffmpeg(format!(
            "创建 VA-API 设备失败（{}）：{}",
            render_node.display(),
            ffi::av_error_text(created_status)
        )));
    }

    // 回锁插入：如果这期间别的线程已经建好，就丢掉自己这份、改用缓存里的（只保留一份）。
    let mut cache = devices()
        .lock()
        .map_err(|_| VaapiError::Ffmpeg("VA-API 设备缓存的锁被毒化".into()))?;
    if let Some(entry) = cache.get(&key) {
        // SAFETY: created 是本线程刚创建的引用，这里释放这一份。
        unsafe { ffi::av_buffer_unref(&raw mut created) };
        // SAFETY: entry.reference 由缓存持有。
        let reference = unsafe { ffi::av_buffer_ref(entry.reference) };
        if reference.is_null() {
            return Err(VaapiError::Ffmpeg("av_buffer_ref 失败".into()));
        }
        return Ok(reference);
    }
    // 缓存持有创建出来的那一份；调用方另拿一份自己的引用。
    let reference = unsafe { ffi::av_buffer_ref(created) };
    if reference.is_null() {
        // SAFETY: created 仍是本地引用。
        unsafe { ffi::av_buffer_unref(&raw mut created) };
        return Err(VaapiError::Ffmpeg("av_buffer_ref 失败".into()));
    }
    cache.insert(key, SharedDevice { reference: created });
    Ok(reference)
}

/// 丢弃某个节点的共享设备（节点被拔掉/驱动重启/EGL CONTEXT_LOST 后调用；下次打开会重新初始化）。
pub fn forget_device(render_node: &Path) {
    if let Ok(mut cache) = devices().lock() {
        cache.remove(render_node);
    }
}

/// 清空全部缓存的共享 VA-API 设备（系统休眠恢复或长阻塞重置时调用）。
pub fn forget_all_devices() {
    if let Ok(mut cache) = devices().lock() {
        cache.clear();
    }
}

impl VaapiDecoder {
    /// 用 `path` 建一条 VA-API 解码管线，显示取自 `render_node`。
    ///
    /// # Errors
    ///
    /// 打不开文件、找不到视频流、解码器打不开、或 HW 设备初始化失败时返回 [`VaapiError`]。
    pub fn open(path: &Path, render_node: &Path) -> Result<Self, VaapiError> {
        let c_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| VaapiError::Unsupported(format!("路径含 NUL 字节: {}", path.display())))?;
        let mut format: *mut ffi::AvFormatContext = std::ptr::null_mut();
        let stage = std::time::Instant::now();
        // 限制探测预算：默认 probesize 5MB + 5s 分析在 4K 片源上要 200–280ms，
        // 只为拿 codecpar；我们的容器（MP4/MKV）头里已有这些信息，256KB/0.5s 足够，
        // 实测把 find_stream_info 从 270ms 降到 ~10ms（t47）。
        let mut options: *mut ffi::AvDictionary = std::ptr::null_mut();
        for (key, value) in [
            ("probesize", "32768"),
            ("analyzeduration", "0"),
            ("fpsprobesize", "0"),
            ("max_probe_packets", "8"),
        ] {
            // 静态字符串不含 NUL；拿不到 CString 就跳过这一项（等同于用默认值）。
            let (Ok(key), Ok(value)) = (CString::new(key), CString::new(value)) else {
                continue;
            };
            // SAFETY: 两个指针都以 NUL 结尾；options 由 ffmpeg 分配/持有。
            unsafe { av_dict_set(&raw mut options, key.as_ptr(), value.as_ptr(), 0) };
        }
        // SAFETY: c_path 以 NUL 结尾；format 是本地可写指针；options 由 ffmpeg 消费。
        let opened = unsafe {
            ffi::avformat_open_input(
                &raw mut format,
                c_path.as_ptr(),
                std::ptr::null(),
                &raw mut options,
            )
        };
        // SAFETY: open_input 之后 options 里剩下的是未识别的键，必须释放。
        unsafe { av_dict_free(&raw mut options) };
        profile_stage("avformat_open_input", stage.elapsed());
        if opened < 0 || format.is_null() {
            return Err(VaapiError::Ffmpeg(format!(
                "打不开视频：{}：{}",
                path.display(),
                ffi::av_error_text(opened)
            )));
        }
        // 先造一个"半成品"，让 Drop 负责收尾（任何早期错误都不会泄漏 format）。
        let mut decoder = Self {
            display: std::ptr::null_mut(),
            device_ref: std::ptr::null_mut(),
            format,
            codec: std::ptr::null_mut(),
            packet: std::ptr::null_mut(),
            scratch: std::ptr::null_mut(),
            stream_index: -1,
            time_base: 0.0,
            flushed: false,
            width: 0,
            height: 0,
            source_fps: None,
            codec_name: String::new(),
        };
        let stage = std::time::Instant::now();
        // 必须跑 `avformat_find_stream_info`：10bit 判据读的是它填好的
        // `AVCodecParameters::format`（容器头本身常常只有 -1），跳过会让 10bit 漏检、
        // 破坏"打开前明确拒绝"的语义（t47 实测）。这里只是把探测预算压到 256KB/0.5s。
        // SAFETY: format 有效；探测失败与原来一样返回错误。
        let info = unsafe { ffi::avformat_find_stream_info(decoder.format, std::ptr::null_mut()) };
        profile_stage("avformat_find_stream_info", stage.elapsed());
        if info < 0 {
            return Err(VaapiError::Ffmpeg(format!(
                "读取流信息失败：{}",
                ffi::av_error_text(info)
            )));
        }
        let stage = std::time::Instant::now();
        decoder.setup_streams(path)?;
        profile_stage("setup_streams", stage.elapsed());
        let stage = std::time::Instant::now();
        decoder.setup_codec(path, render_node)?;
        profile_stage("setup_codec", stage.elapsed());
        Ok(decoder)
    }

    fn setup_streams(&mut self, path: &Path) -> Result<(), VaapiError> {
        let mut codec_ptr: *const ffi::AvCodec = std::ptr::null();
        // SAFETY: format 有效；codec_ptr 是本地可写指针。
        let index = unsafe {
            ffi::av_find_best_stream(
                self.format,
                ffi::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                &raw mut codec_ptr,
                0,
            )
        };
        if index < 0 || codec_ptr.is_null() {
            return Err(VaapiError::Unsupported(format!(
                "找不到可解码的视频流：{}",
                path.display()
            )));
        }
        self.stream_index = index;
        // SAFETY: index 是 av_find_best_stream 返回的有效下标。
        let stream = unsafe { *(*self.format).streams.offset(index as isize) };
        if stream.is_null() {
            return Err(VaapiError::Unsupported("视频流指针为空".into()));
        }
        // SAFETY: stream 非空；codecpar/time_base 由 libavformat 填充。
        let (parameters, time_base) = unsafe { ((*stream).codecpar, (*stream).time_base) };
        if parameters.is_null() {
            return Err(VaapiError::Unsupported("视频流没有编码参数".into()));
        }
        if time_base.num <= 0 || time_base.den <= 0 {
            return Err(VaapiError::Unsupported(format!(
                "流时基非法：{}/{}",
                time_base.num, time_base.den
            )));
        }
        self.time_base = f64::from(time_base.num) / f64::from(time_base.den);
        // SAFETY: parameters 非空。
        let (width, height) = unsafe { ((*parameters).width, (*parameters).height) };
        if width <= 0 || height <= 0 {
            return Err(VaapiError::Unsupported(format!(
                "视频尺寸非法：{width}x{height}"
            )));
        }
        self.width = width as u32;
        self.height = height as u32;
        // 10bit 在**打开解码器之前**就明确拒绝：整条链路（VA-API 表面导出 → NV12 GL
        // 转换）只支持 8bit，10bit 素材解出来是 P010 表面，转换器根本没有对应路径。
        // 早拒的好处是错误信息可操作；晚拒（导出时才失败）只会给出一个 fourcc 数字。
        // 上层据此回落到软件解码并把这句话放进告警，而不是静默降到 1fps。
        // SAFETY: parameters 非空。
        let source_pix_fmt = unsafe { ffi::codecpar_pix_fmt(parameters) };
        if ffi::is_ten_bit_pix_fmt(source_pix_fmt) {
            return Err(VaapiError::Unsupported(format!(
                "该素材是 10bit（{}），当前解码/RGB 转换链只支持 8bit NV12：请改用 8bit 版本，\
                 或先转成 8bit（例如 `ffmpeg -i in.mp4 -pix_fmt yuv420p out.mp4`）",
                ffi::pix_fmt_name(source_pix_fmt)
            )));
        }
        // SAFETY: parameters 非空；avcodec_get_name 对任意 id 都返回静态字符串。
        self.codec_name = unsafe {
            let id = (*parameters).codec_id;
            let name = ffi::avcodec_get_name(id);
            if name.is_null() {
                format!("id:{id}")
            } else {
                std::ffi::CStr::from_ptr(name)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        // SAFETY: stream 非空；两个 AVRational 由 libavformat 填充。
        self.source_fps = unsafe { stream_frame_rate(stream) };
        // 关键一步：`av_find_best_stream` 给的是**默认**解码器。AV1 的默认解码器是
        // `libdav1d`（纯软解，没有任何硬件配置），于是即便驱动支持 AV1 VA-API 解码，
        // 我们拿到的也永远是软件帧，还会被误判成「驱动不支持」。H.264/HEVC 的默认
        // 解码器是原生 `h264`/`hevc`（带 VAAPI 硬件配置），所以这个坑只在 AV1 暴露。
        // SAFETY: codec_ptr 来自 av_find_best_stream，非空。
        let codec_ptr = unsafe { select_vaapi_decoder(codec_ptr) }?;
        // SAFETY: codec 指针来自选择结果（默认或注册表里的同类解码器），非空。
        let codec = unsafe { ffi::avcodec_alloc_context3(codec_ptr) };
        if codec.is_null() {
            return Err(VaapiError::Ffmpeg("avcodec_alloc_context3 失败".into()));
        }
        self.codec = codec;
        // SAFETY: 两个指针均有效。
        let copied = unsafe { ffi::avcodec_parameters_to_context(codec, parameters) };
        if copied < 0 {
            return Err(VaapiError::Ffmpeg(format!(
                "复制编码参数失败：{}",
                ffi::av_error_text(copied)
            )));
        }
        Ok(())
    }

    fn setup_codec(&mut self, path: &Path, render_node: &Path) -> Result<(), VaapiError> {
        let stage = std::time::Instant::now();
        // t47：按节点进程内复用设备（共享 VADisplay）。命中缓存时只付一次
        // `av_buffer_ref`，不再付 220–400ms 的 vaInitialize/驱动上下文初始化。
        let device_ref = shared_device(render_node)?;
        profile_stage("shared_device", stage.elapsed());
        self.device_ref = device_ref;
        // 读回 ffmpeg 使用的 VADisplay：导出表面必须与解码共用同一个显示实例。
        // SAFETY: device_ref 非空且由 ffmpeg 填充。
        let display = unsafe { vaapi_display_of(device_ref) };
        if display.is_null() {
            return Err(VaapiError::Ffmpeg(
                "ffmpeg 的 VAAPI 设备没有 VADisplay".into(),
            ));
        }
        self.display = display;
        // 把设备挂到解码上下文（唯一需要写 AVCodecContext 的地方，偏移见 build.rs）。
        // SAFETY: av_buffer_ref 增加引用；codec 上下文会释放自己那份。
        let codec_ref = unsafe { ffi::av_buffer_ref(device_ref) };
        if codec_ref.is_null() {
            return Err(VaapiError::Ffmpeg("av_buffer_ref 失败".into()));
        }
        // SAFETY: codec 非空；偏移与运行库同版本（同一套头文件推出）。
        unsafe {
            write_hw_device_ctx(self.codec, codec_ref);
        }
        let stage = std::time::Instant::now();
        // VA-API 在 GPU 侧解码，FFmpeg 默认 threads=0 会按 CPU 核数（如 16 核）派生多线程
        // 帧解码上下文与独立缓冲区，导致解码器打开期间堆内存和匿名页激增数十 MiB。
        // 显式传 threads=1 只保留单线程调度即可跑满 4K60 硬件解码。
        let mut codec_opts: *mut ffi::AvDictionary = std::ptr::null_mut();
        if let (Ok(key), Ok(value)) = (CString::new("threads"), CString::new("1")) {
            // SAFETY: key 与 value 均以 NUL 结尾；codec_opts 由 ffmpeg 分配/持有。
            unsafe { av_dict_set(&raw mut codec_opts, key.as_ptr(), value.as_ptr(), 0) };
        }
        // SAFETY: codec 有效且已配置；codec_opts 由 avcodec_open2 消费并在下方释放。
        let opened =
            unsafe { ffi::avcodec_open2(self.codec, std::ptr::null(), &raw mut codec_opts) };
        // SAFETY: 释放 avcodec_open2 后剩余的未识别键字典，空指针安全。
        unsafe { av_dict_free(&raw mut codec_opts) };
        profile_stage("avcodec_open2", stage.elapsed());
        if opened < 0 {
            return Err(VaapiError::Ffmpeg(format!(
                "打开解码器失败（{}）：{}",
                path.display(),
                ffi::av_error_text(opened)
            )));
        }
        // SAFETY: 分配函数要么返回新对象要么返回空指针。
        self.packet = unsafe { ffi::av_packet_alloc() };
        self.scratch = unsafe { ffi::av_frame_alloc() };
        if self.packet.is_null() || self.scratch.is_null() {
            return Err(VaapiError::Ffmpeg(
                "av_packet_alloc/av_frame_alloc 失败".into(),
            ));
        }
        // 解码器名保留在 setup_streams 里填的编码名（`h264`/`hevc`…），
        // 这里只补上"走的是 VA-API"这一层，最终形如 `vaapi:h264`。
        if self.codec_name.is_empty() {
            self.codec_name = "vaapi".to_string();
        }
        Ok(())
    }

    /// 流信息。
    #[must_use]
    pub fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            width: self.width,
            height: self.height,
            source_fps: self.source_fps,
        }
    }

    /// 解码用的 `VADisplay`（导出表面必须用它）。
    #[must_use]
    pub fn display(&self) -> ffi::VaDisplay {
        self.display
    }

    /// 实际解码器名（诊断用）。
    #[must_use]
    pub fn codec_name(&self) -> &str {
        &self.codec_name
    }

    /// 取下一条表面；`Ok(None)` 表示流已结束。
    ///
    /// # Errors
    ///
    /// 解码器报错、或解码结果不是 VA-API 表面（说明回退成了软解）时返回错误。
    pub fn next_surface(&mut self) -> Result<Option<DecodedSurface>, VaapiError> {
        loop {
            // SAFETY: 两个指针在 `setup_codec` 里都分配成功且非空。
            let received = unsafe { ffi::avcodec_receive_frame(self.codec, self.scratch) };
            if received == 0 {
                let frame_format = unsafe { (*self.scratch).format };
                if frame_format != ffi::AV_PIX_FMT_VAAPI {
                    return Err(VaapiError::Unsupported(format!(
                        "解码器输出的是软件帧（format={frame_format}，期望 VAAPI={}）：\
                         该驱动/编码组合没有建立硬件管线",
                        ffi::AV_PIX_FMT_VAAPI
                    )));
                }
                // SAFETY: scratch 是有效 VAAPI 帧。
                let surface = unsafe { ffi::surface_id_of(self.scratch) };
                // SAFETY: 两个字段都是普通整数。
                let (width, height, raw_pts) = unsafe {
                    (
                        (*self.scratch).width,
                        (*self.scratch).height,
                        (*self.scratch).pts,
                    )
                };
                // 把这一帧的所有权交出去，scratch 换成一个新分配的空帧。
                // SAFETY: 分配失败时返回空指针，下面立刻检查。
                let fresh = unsafe { ffi::av_frame_alloc() };
                if fresh.is_null() {
                    return Err(VaapiError::Ffmpeg("av_frame_alloc 失败".into()));
                }
                let owned = std::mem::replace(&mut self.scratch, fresh);
                let pts = if raw_pts > 0 {
                    let secs = raw_pts as f64 * self.time_base;
                    if (0.0..1e12).contains(&secs) {
                        Duration::from_secs_f64(secs)
                    } else {
                        Duration::ZERO
                    }
                } else {
                    Duration::ZERO
                };
                return Ok(Some(DecodedSurface {
                    frame: owned,
                    surface,
                    width: u32::try_from(width).unwrap_or(self.width),
                    height: u32::try_from(height).unwrap_or(self.height),
                    pts,
                }));
            }
            if received == ffi::AVERROR_EOF {
                return Ok(None);
            }
            if received != ffi::AVERROR_EAGAIN {
                return Err(VaapiError::Ffmpeg(format!(
                    "解码失败：{}",
                    ffi::av_error_text(received)
                )));
            }
            if self.flushed {
                return Ok(None);
            }
            // 需要更多输入：读一个包，只把视频流的包送进解码器。
            // SAFETY: 两个指针均非空且有效。
            let read = unsafe { ffi::av_read_frame(self.format, self.packet) };
            if read < 0 {
                // 读到结尾（或错误）：送空包冲刷，然后继续 receive。
                self.flushed = true;
                // SAFETY: packet 有效；空包指针表示 flush。
                unsafe {
                    ffi::avcodec_send_packet(self.codec, std::ptr::null());
                }
                continue;
            }
            // SAFETY: packet 由 av_read_frame 填充。
            let packet_stream =
                unsafe { (*self.packet.cast::<ffi::AvPacketPrefix>()).stream_index };
            if packet_stream != self.stream_index {
                // SAFETY: packet 有效。
                unsafe {
                    ffi::av_packet_unref(self.packet);
                }
                continue;
            }
            // SAFETY: packet 与 codec 均有效。
            let sent = unsafe { ffi::avcodec_send_packet(self.codec, self.packet) };
            // SAFETY: packet 有效；无论成功与否都要归还。
            unsafe {
                ffi::av_packet_unref(self.packet);
            }
            if sent < 0 && sent != ffi::AVERROR_EAGAIN {
                return Err(VaapiError::Ffmpeg(format!(
                    "送入解码器失败：{}",
                    ffi::av_error_text(sent)
                )));
            }
        }
    }

    /// 回到流起点（循环重播）。
    ///
    /// # Errors
    ///
    /// seek 失败时返回错误；成功后解码器状态被清空。
    pub fn seek_start(&mut self) -> Result<(), VaapiError> {
        // SAFETY: format 有效；stream_index >= 0（`setup_streams` 成功后才可能走到这里）。
        let seeked = unsafe {
            ffi::av_seek_frame(self.format, self.stream_index, 0, ffi::AVSEEK_FLAG_BACKWARD)
        };
        if seeked < 0 {
            return Err(VaapiError::Ffmpeg(format!(
                "回到起点失败：{}",
                ffi::av_error_text(seeked)
            )));
        }
        // SAFETY: codec 有效；冲刷后解码器会丢掉旧帧。
        unsafe {
            ffi::avcodec_flush_buffers(self.codec);
        }
        self.flushed = false;
        Ok(())
    }
}

impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        // SAFETY: 每个指针要么为空要么由本结构独占（`av_*_free` 接受空指针）。
        unsafe {
            if !self.scratch.is_null() {
                ffi::av_frame_free(&raw mut self.scratch);
            }
            if !self.packet.is_null() {
                ffi::av_packet_free(&raw mut self.packet);
            }
            if !self.codec.is_null() {
                ffi::avcodec_free_context(&raw mut self.codec);
            }
            if !self.device_ref.is_null() {
                ffi::av_buffer_unref(&raw mut self.device_ref);
            }
            if !self.format.is_null() {
                ffi::avformat_close_input(&raw mut self.format);
            }
        }
    }
}

/// 从 ffmpeg 的 HW 设备引用里读出 `VADisplay`。
///
/// # Safety
///
/// `device_ref` 必须是 `av_hwdevice_ctx_create`/`av_hwdevice_ctx_alloc` 返回、
/// 且 dev_type 为 VAAPI 的有效引用。
unsafe fn vaapi_display_of(device_ref: *mut ffi::AvBufferRef) -> ffi::VaDisplay {
    // SAFETY: 由调用方保证。
    unsafe {
        let data = (*device_ref).data;
        if data.is_null() {
            return std::ptr::null_mut();
        }
        let hardware = data.cast::<ffi::AvHwDeviceContext>();
        if (*hardware).dev_type != ffi::AV_HWDEVICE_TYPE_VAAPI {
            return std::ptr::null_mut();
        }
        let vaapi = (*hardware).hwctx.cast::<ffi::AvVaapiDeviceContext>();
        if vaapi.is_null() {
            return std::ptr::null_mut();
        }
        (*vaapi).display
    }
}

/// 把 `AVBufferRef` 写进 `AVCodecContext::hw_device_ctx`（唯一需要写上下文字段的地方）。
///
/// # Safety
///
/// `codec` 必须是 `avcodec_alloc_context3` 返回且仍存活的上下文；`reference` 必须是
/// 已 `av_buffer_ref` 的引用（所有权转移给上下文）。
unsafe fn write_hw_device_ctx(codec: *mut ffi::AvCodecContext, reference: *mut ffi::AvBufferRef) {
    // SAFETY: 由调用方保证 codec 有效；偏移来自构建期 offsetof，与运行库同版本。
    unsafe {
        let slot = codec.cast::<u8>().add(ffi::AVCODEC_CTX_HW_DEVICE_CTX);
        slot.cast::<*mut ffi::AvBufferRef>().write(reference);
    }
}

/// 一个候选解码器的**可测**描述（FFI 指针进不了纯函数单测，所以先投影成这个）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DecoderCandidate {
    /// ffmpeg 解码器名（`av1` / `libdav1d` / `h264` …）。
    name: String,
    /// 是否可当解码器用（同编码 id 的编码器要排除）。
    is_decoder: bool,
    /// 是否能建 VA-API 硬件管线（`avcodec_get_hw_config` 里有没有 VAAPI 项）。
    has_vaapi_hw: bool,
}

/// 从候选里挑一个能建 VA-API 管线的解码器（返回下标）。
///
/// 顺序即优先级：下标 0 是 `av_find_best_stream` 给的默认解码器，所以「默认能硬解」
/// 时永远选默认；否则退到注册表里第一个能硬解的同类解码器。
fn choose_vaapi_decoder(candidates: &[DecoderCandidate]) -> Option<usize> {
    candidates
        .iter()
        .position(|candidate| candidate.is_decoder && candidate.has_vaapi_hw)
}

/// 把 FFmpeg 的解码器指针投影成可测描述。
///
/// # Safety
///
/// `codec` 必须是 ffmpeg 注册表返回的有效 `AVCodec`。
unsafe fn candidate_of(codec: *const ffi::AvCodec) -> DecoderCandidate {
    DecoderCandidate {
        // SAFETY: 由调用方保证 codec 有效。
        name: unsafe { ffi::codec_name_of(codec) }.unwrap_or_else(|| "?".into()),
        // SAFETY: 同上。
        is_decoder: unsafe { ffi::av_codec_is_decoder(codec) } != 0,
        // SAFETY: 同上。
        has_vaapi_hw: unsafe { ffi::codec_supports_vaapi_hw(codec) },
    }
}

/// 选一个真正能用外部 VA-API 设备的解码器：默认解码器不行时，按编码 id 遍历注册表。
///
/// 这是 AV1 硬解缺陷的修复点：AV1 的默认解码器 `libdav1d` 没有硬件配置，
/// `avcodec_alloc_context3` 一旦用上它，后面 `hw_device_ctx` 再怎么写都只会出软件帧。
///
/// # Safety
///
/// `default` 必须是 `av_find_best_stream` 返回的有效 `AVCodec`。
unsafe fn select_vaapi_decoder(
    default: *const ffi::AvCodec,
) -> Result<*const ffi::AvCodec, VaapiError> {
    // SAFETY: 由调用方保证 default 有效。
    let default_candidate = unsafe { candidate_of(default) };
    if default_candidate.has_vaapi_hw {
        return Ok(default);
    }
    // SAFETY: default 有效。
    let codec_id = unsafe { ffi::codec_id_of(default) };
    let mut candidates = vec![default_candidate.clone()];
    let mut pointers = vec![default];
    let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
    loop {
        // SAFETY: opaque 由 ffmpeg 维护；返回空指针即遍历结束。
        let codec = unsafe { ffi::av_codec_iterate(&raw mut opaque) };
        if codec.is_null() {
            break;
        }
        if std::ptr::eq(codec, default) {
            continue;
        }
        // SAFETY: codec 来自注册表，有效。
        if unsafe { ffi::codec_id_of(codec) } != codec_id {
            continue;
        }
        // SAFETY: 同上。
        candidates.push(unsafe { candidate_of(codec) });
        pointers.push(codec);
    }
    let Some(index) = choose_vaapi_decoder(&candidates) else {
        let id_name = {
            // SAFETY: 任意 codec id 都能取到静态名字。
            let name = unsafe { ffi::avcodec_get_name(codec_id) };
            if name.is_null() {
                format!("id:{codec_id}")
            } else {
                unsafe { std::ffi::CStr::from_ptr(name) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let tried: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        let codec_kind = ffi::codec_id_name(codec_id);
        return Err(VaapiError::Unsupported(format!(
            "没有可用的 VA-API 硬件解码器：默认解码器 {} 不提供 VAAPI 硬件配置，\
             同编码（{id_name}/{codec_kind}）的其它解码器（{}）也都不支持 —— \
             请检查 ffmpeg 构建是否包含该编码的硬解（例如 AV1 需要原生 av1 解码器，\
             而不是 libdav1d）",
            default_candidate.name,
            tried.join(" / ")
        )));
    };
    let selected = candidates
        .get(index)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    tracing::info!(
        default = %default_candidate.name,
        selected = %selected,
        codec = ffi::codec_id_name(codec_id),
        "解码器切换：默认解码器没有 VA-API 硬件配置，改用同编码的硬解实现"
    );
    Ok(pointers[index])
}

/// 该路径是否看起来像一个 DRM 渲染节点（纯字符串判断，便于单测）。
#[must_use]
pub fn looks_like_render_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("renderD"))
}

/// 从流的帧率字段里取一个可用的 fps。
///
/// `avg_frame_rate` 更可靠（VFR 的均值），为 `0/0` 时退回 `r_frame_rate`；
/// 两者都不可用就返回 `None`（调用方据此跳过速率相关的告警）。
///
/// # Safety
///
/// `stream` 必须指向 libavformat 填充过的 `AVStream`。
unsafe fn stream_frame_rate(stream: *const ffi::AvStream) -> Option<f64> {
    // SAFETY: 由调用方保证 stream 非空，偏移来自构建期 offsetof。
    let (avg, raw) = unsafe {
        (
            ffi::avrational_at(stream, ffi::AVSTREAM_AVG_FRAME_RATE),
            ffi::avrational_at(stream, ffi::AVSTREAM_R_FRAME_RATE),
        )
    };
    for value in [avg, raw] {
        if value.num > 0 && value.den > 0 {
            return Some(f64::from(value.num) / f64::from(value.den));
        }
    }
    None
}

/// 把候选节点排序：`renderD128` 早于 `renderD129`（稳定、可复现）。
pub fn sort_render_nodes(nodes: &mut [PathBuf]) {
    nodes.sort();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn candidate(name: &str, is_decoder: bool, has_vaapi_hw: bool) -> DecoderCandidate {
        DecoderCandidate {
            name: name.to_string(),
            is_decoder,
            has_vaapi_hw,
        }
    }

    /// 遍历 ffmpeg 注册表，收集某个编码 id 下的全部解码器：(名字, 是否能 VA-API 硬解)。
    fn decoders_with_id(codec_id: c_int) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            // SAFETY: opaque 由 ffmpeg 维护，返回空指针即遍历结束。
            let codec = unsafe { ffi::av_codec_iterate(&raw mut opaque) };
            if codec.is_null() {
                break;
            }
            // SAFETY: codec 来自注册表，有效。
            unsafe {
                if ffi::codec_id_of(codec) == codec_id && ffi::av_codec_is_decoder(codec) != 0 {
                    out.push((
                        ffi::codec_name_of(codec).unwrap_or_default(),
                        ffi::codec_supports_vaapi_hw(codec),
                    ));
                }
            }
        }
        out
    }

    #[test]
    fn choose_prefers_the_default_decoder_when_it_can_do_hardware() {
        let candidates = vec![
            candidate("hevc", true, true),
            candidate("hevc_other", true, true),
        ];
        assert_eq!(choose_vaapi_decoder(&candidates), Some(0));
    }

    #[test]
    fn choose_skips_a_software_only_default() {
        // AV1 的真实形状：默认是 libdav1d（没有任何 hw config），原生 av1 才是硬解。
        let candidates = vec![
            candidate("libdav1d", true, false),
            candidate("av1", true, true),
        ];
        assert_eq!(choose_vaapi_decoder(&candidates), Some(1));
    }

    #[test]
    fn choose_ignores_encoders_and_returns_none_without_hardware() {
        let candidates = vec![
            candidate("libdav1d", true, false),
            candidate("libsvtav1", false, false),
        ];
        assert_eq!(choose_vaapi_decoder(&candidates), None);
    }

    #[test]
    fn h264_and_hevc_keep_a_vaapi_capable_decoder() {
        // 回归护栏：解码器选择不能把原本正常的 H.264/HEVC 硬解弄丢。
        for (label, id) in [
            ("h264", ffi::AV_CODEC_ID_H264),
            ("hevc", ffi::AV_CODEC_ID_HEVC),
        ] {
            let decoders = decoders_with_id(id);
            let capable: Vec<&str> = decoders
                .iter()
                .filter(|(_, hardware)| *hardware)
                .map(|(name, _)| name.as_str())
                .collect();
            assert!(
                !capable.is_empty(),
                "{label} 没有任何 VA-API 硬件解码器；ffmpeg 注册表里是 {decoders:?}"
            );
        }
    }

    #[test]
    fn av1_has_a_vaapi_capable_decoder_besides_libdav1d() {
        // 这是 AV1「解码器输出的是软件帧（format=0）」缺陷的护栏：
        // 只要注册表里有能硬解的 AV1 解码器，选择逻辑就必须能选中它。
        let decoders = decoders_with_id(ffi::AV_CODEC_ID_AV1);
        let native = decoders
            .iter()
            .find(|(name, hardware)| name == "av1" && *hardware);
        assert!(
            native.is_some(),
            "没有找到能 VA-API 硬解的原生 av1 解码器；注册表里是 {decoders:?}"
        );
        // libdav1d 必须在但只能是软解——否则这条测试就失去意义了。
        if let Some((_, hardware)) = decoders.iter().find(|(name, _)| name == "libdav1d") {
            assert!(!hardware, "libdav1d 声称支持 VA-API 硬解，与已知事实不符");
        }
        let candidates: Vec<DecoderCandidate> = decoders
            .iter()
            .map(|(name, hardware)| candidate(name, true, *hardware))
            .collect();
        let index = choose_vaapi_decoder(&candidates).expect("至少有一个硬解解码器");
        assert!(candidates[index].has_vaapi_hw);
        assert_ne!(candidates[index].name, "libdav1d");
    }

    #[test]
    fn ten_bit_pixel_formats_are_recognised() {
        assert!(ffi::is_ten_bit_pix_fmt(ffi::AV_PIX_FMT_YUV420P10LE));
        assert!(ffi::is_ten_bit_pix_fmt(ffi::AV_PIX_FMT_P010LE));
        assert!(!ffi::is_ten_bit_pix_fmt(ffi::AV_PIX_FMT_VAAPI));
        assert!(!ffi::is_ten_bit_pix_fmt(-1));
        assert_eq!(
            ffi::pix_fmt_name(ffi::AV_PIX_FMT_YUV420P10LE),
            "yuv420p10le"
        );
        assert_eq!(ffi::pix_fmt_name(ffi::AV_PIX_FMT_P010LE), "p010le");
        assert_eq!(ffi::pix_fmt_name(-1), "fmt:-1");
    }

    #[test]
    fn ten_bit_sources_are_rejected_with_an_actionable_message() {
        // 用矩阵里的 10bit 素材（存在才算）；消息必须说清 10bit/8bit/转码建议。
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.work/corpus/matrix/matrix-3840x2160-30fps-hevc-10bit-p010.mp4"
        ));
        if !path.exists() {
            return;
        }
        let error = match VaapiDecoder::open(path, std::path::Path::new("/dev/dri/renderD129")) {
            Ok(_) => panic!("10bit 素材不该被 VA-API 解码链接受"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("10bit"), "实际: {error}");
        assert!(error.contains("8bit"), "实际: {error}");
        assert!(error.contains("ffmpeg"), "缺少可操作的转码建议: {error}");
    }
}
