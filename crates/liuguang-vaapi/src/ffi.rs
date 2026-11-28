//! FFI：libva（DRM 显示 + 表面导出）与 FFmpeg（解封装 + 解码）的最小绑定。
//!
//! 设计原则：
//! - **只绑定用得到的符号**，不为"以后可能用到"预留；
//! - 结构体只声明**只读访问所需的前缀**，并用 `offset_of!` 与构建期 C 探测值
//!   做**编译期断言**——布局漂移在 `cargo build` 阶段就会失败，而不是运行期静默出错；
//! - 唯一需要**写**入的字段是 `AVCodecContext::hw_device_ctx`，偏移同样来自构建期探测。
use std::ffi::{c_char, c_int, c_uint, c_void};

// ---------------------------------------------------------------- libva
/// `VAStatus`（0 = `VA_STATUS_SUCCESS`）。
pub type VaStatus = c_int;
/// 不透明的 `VADisplay` 句柄。
pub type VaDisplay = *mut c_void;
/// VA 表面 id。
pub type VaSurfaceId = c_uint;

/// 成功。
pub const VA_STATUS_SUCCESS: VaStatus = 0;
/// `vaExportSurfaceHandle` 的内存类型：DRM PRIME 2（带 modifier 与分层描述）。
pub const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: c_uint = 0x4000_0000;
/// 导出为只读（解码器产出、下游只消费）。
pub const VA_EXPORT_SURFACE_READ_ONLY: c_uint = 1;
/// 每层单独一项（而不是把多层合并进一个 object）。
pub const VA_EXPORT_SURFACE_SEPARATE_LAYERS: c_uint = 4;
/// 描述符里最多几个 DRM object。
pub const VA_MAX_DRM_OBJECTS: usize = 4;
/// 描述符里最多几个 layer。
pub const VA_MAX_LAYERS: usize = 4;
/// 每个 layer 最多几个平面。
pub const VA_MAX_PLANES: usize = 4;

/// 一个被导出的 DRM object（= 一个 fd）。
#[repr(C)]
#[derive(Debug)]
pub struct VaDrmObject {
    /// 导出的 dma-buf fd（调用方负责 close）。
    pub fd: c_int,
    /// 该 object 的字节数。
    pub size: u32,
    /// DRM format modifier（NVIDIA block-linear 会给出非 0 值）。
    pub drm_format_modifier: u64,
}

/// 一个 layer：一组平面共享同样的 DRM format。
#[repr(C)]
#[derive(Debug)]
pub struct VaDrmLayer {
    /// 该层的 DRM fourcc。
    pub drm_format: u32,
    /// 该层有几个有效平面。
    pub num_planes: u32,
    /// 每个平面落在哪个 object 上。
    pub object_index: [u32; VA_MAX_PLANES],
    /// 每个平面在 object 内的字节偏移。
    pub offset: [u32; VA_MAX_PLANES],
    /// 每个平面的行距。
    pub pitch: [u32; VA_MAX_PLANES],
}

/// `vaExportSurfaceHandle` 的输出。
#[repr(C)]
#[derive(Debug)]
pub struct VaDrmPrimeSurfaceDescriptor {
    /// 表面整体的 DRM fourcc（NV12 = `0x3231564e`）。
    pub fourcc: u32,
    /// 表面宽度（像素）。
    pub width: u32,
    /// 表面高度（像素）。
    pub height: u32,
    /// 有效 object 数。
    pub num_objects: u32,
    /// object 表。
    pub objects: [VaDrmObject; VA_MAX_DRM_OBJECTS],
    /// 有效 layer 数。
    pub num_layers: u32,
    /// layer 表。
    pub layers: [VaDrmLayer; VA_MAX_LAYERS],
}

impl VaDrmPrimeSurfaceDescriptor {
    /// 全零描述符（`vaExportSurfaceHandle` 会填）。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fourcc: 0,
            width: 0,
            height: 0,
            num_objects: 0,
            objects: [
                VaDrmObject {
                    fd: -1,
                    size: 0,
                    drm_format_modifier: 0,
                },
                VaDrmObject {
                    fd: -1,
                    size: 0,
                    drm_format_modifier: 0,
                },
                VaDrmObject {
                    fd: -1,
                    size: 0,
                    drm_format_modifier: 0,
                },
                VaDrmObject {
                    fd: -1,
                    size: 0,
                    drm_format_modifier: 0,
                },
            ],
            num_layers: 0,
            layers: [
                VaDrmLayer {
                    drm_format: 0,
                    num_planes: 0,
                    object_index: [0; VA_MAX_PLANES],
                    offset: [0; VA_MAX_PLANES],
                    pitch: [0; VA_MAX_PLANES],
                },
                VaDrmLayer {
                    drm_format: 0,
                    num_planes: 0,
                    object_index: [0; VA_MAX_PLANES],
                    offset: [0; VA_MAX_PLANES],
                    pitch: [0; VA_MAX_PLANES],
                },
                VaDrmLayer {
                    drm_format: 0,
                    num_planes: 0,
                    object_index: [0; VA_MAX_PLANES],
                    offset: [0; VA_MAX_PLANES],
                    pitch: [0; VA_MAX_PLANES],
                },
                VaDrmLayer {
                    drm_format: 0,
                    num_planes: 0,
                    object_index: [0; VA_MAX_PLANES],
                    offset: [0; VA_MAX_PLANES],
                    pitch: [0; VA_MAX_PLANES],
                },
            ],
        }
    }
}

impl Default for VaDrmPrimeSurfaceDescriptor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- FFmpeg：不透明类型
/// `AVCodec`（不透明）。
#[repr(C)]
pub struct AvCodec {
    _private: [u8; 0],
}
/// `AVCodecHWConfig`：解码器声明的**一项**硬件配置（`avcodec_get_hw_config` 返回）。
///
/// 三个字段都是普通整数，布局与系统头文件无关，但偏移仍来自构建期 `offsetof`
/// 探测并做编译期断言——理由与其它结构一致：不手抄布局。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvCodecHWConfig {
    /// 硬件解码输出的像素格式（VA-API 为 [`AV_PIX_FMT_VAAPI`]）。
    pub pix_fmt: c_int,
    /// 建链方式位掩码（见 [`AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX`]）。
    pub methods: c_int,
    /// 设备类型（VA-API 为 [`AV_HWDEVICE_TYPE_VAAPI`]）。
    pub device_type: c_int,
}
/// `AVCodecContext`（不透明；只通过偏移写 `hw_device_ctx`）。
#[repr(C)]
pub struct AvCodecContext {
    _private: [u8; 0],
}
/// `AVPacket`（不透明）。
#[repr(C)]
pub struct AvPacket {
    _private: [u8; 0],
}
/// `AVDictionary`（不透明）。
#[repr(C)]
pub struct AvDictionary {
    _private: [u8; 0],
}
/// `AVInputFormat`（不透明）。
#[repr(C)]
pub struct AvInputFormat {
    _private: [u8; 0],
}

// ---------------------------------------------------------------- FFmpeg：只读前缀
/// `AVRational`。
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AvRational {
    /// 分子。
    pub num: c_int,
    /// 分母。
    pub den: c_int,
}

/// `AVFrame` 前缀（到 `pts` 为止）。
#[repr(C)]
pub struct AvFrame {
    /// 平面指针；`AV_PIX_FMT_VAAPI` 时 `data[3]` 里放的是 `VASurfaceID`。
    pub data: [*mut u8; 8],
    /// 行距（VAAPI 帧不使用）。
    pub linesize: [c_int; 8],
    /// 扩展平面指针。
    pub extended_data: *mut *mut u8,
    /// 帧宽。
    pub width: c_int,
    /// 帧高。
    pub height: c_int,
    /// 音频样本数。
    pub nb_samples: c_int,
    /// 像素格式；VAAPI 帧为 [`av_pix_fmt_vaapi`]。
    pub format: c_int,
    /// 关键帧标记。
    pub key_frame: c_int,
    /// 图片类型。
    pub pict_type: c_int,
    /// 像素宽高比。
    pub sample_aspect_ratio: AvRational,
    /// 呈现时间戳（流时基）。
    pub pts: i64,
}

/// `AVPacket` 前缀（到 `stream_index` 为止）。
#[repr(C)]
pub struct AvPacketPrefix {
    /// 底层 buffer。
    pub buf: *mut AvBufferRef,
    /// 呈现时间戳。
    pub pts: i64,
    /// 解码时间戳。
    pub dts: i64,
    /// 数据指针。
    pub data: *mut u8,
    /// 数据字节数。
    pub size: c_int,
    /// 所属流下标。
    pub stream_index: c_int,
}

/// `AVCodecParameters` 前缀（到 `height` 为止）。
#[repr(C)]
pub struct AvCodecParameters {
    /// 媒体类型（0 = 视频）。
    pub codec_type: c_int,
    /// 编码 id。
    pub codec_id: c_int,
    /// tag。
    pub codec_tag: u32,
    /// 中间字段（我们不需要，按偏移跳过）。
    pub _padding: [u8; 60],
    /// 宽。
    pub width: c_int,
    /// 高。
    pub height: c_int,
}

/// `AVStream` 前缀（到 `codecpar` 为止）。
#[repr(C)]
pub struct AvStream {
    /// 私有 class。
    pub av_class: *const c_void,
    /// 流序号。
    pub index: c_int,
    /// 流 id。
    pub id: c_int,
    /// 编码参数。
    pub codecpar: *mut AvCodecParameters,
    /// 私有数据。
    pub priv_data: *mut c_void,
    /// 流时基。
    pub time_base: AvRational,
}

/// `AVFormatContext` 前缀（到 `streams` 为止）。
#[repr(C)]
pub struct AvFormatContext {
    /// 私有 class。
    pub av_class: *const c_void,
    /// 输入格式。
    pub iformat: *const c_void,
    /// 输出格式。
    pub oformat: *const c_void,
    /// 私有数据。
    pub priv_data: *mut c_void,
    /// IO 上下文。
    pub pb: *mut c_void,
    /// 上下文标志。
    pub ctx_flags: c_int,
    /// 流数量。
    pub nb_streams: c_uint,
    /// 流数组。
    pub streams: *mut *mut AvStream,
}

/// `AVBufferRef`。
#[repr(C)]
pub struct AvBufferRef {
    /// 底层 buffer。
    pub buffer: *mut c_void,
    /// 数据指针。
    pub data: *mut u8,
    /// 字节数。
    pub size: usize,
}

/// `AVHWDeviceContext` 前缀（到 `hwctx` 为止）。
#[repr(C)]
pub struct AvHwDeviceContext {
    /// 私有 class。
    pub av_class: *const c_void,
    /// 设备类型（[`av_hwdevice_type_vaapi`]）。
    pub dev_type: c_int,
    /// 对齐填充。
    pub _padding: c_int,
    /// 与设备类型相关的上下文（VAAPI 即 [`AvVaapiDeviceContext`]）。
    pub hwctx: *mut c_void,
}

/// `AVVAAPIDeviceContext` 前缀（到 `driver_quirks` 为止）。
#[repr(C)]
pub struct AvVaapiDeviceContext {
    /// `VADisplay`（`av_hwdevice_ctx_create` 之后由 ffmpeg 填好）。
    pub display: VaDisplay,
    /// 驱动 quirk 位。
    pub driver_quirks: c_uint,
}

// ---------------------------------------------------------------- 布局断言
/// 由 `build.rs` 用 `offsetof` 探测、再以 `cargo:rustc-env` 注入的偏移。
///
/// 用法：`probed!("AVFRAME_DATA")`——编译期读环境变量并转成 `usize`。
macro_rules! probed {
    ($name:literal) => {{
        const RAW: &str = env!($name);
        parse_usize_const(RAW)
    }};
}

/// 编译期字符串 → `usize`（只允许十进制）。
#[must_use]
pub const fn parse_usize_const(raw: &str) -> usize {
    let bytes = raw.as_bytes();
    let mut value: usize = 0;
    let mut index = 0;
    while index < bytes.len() {
        let digit = bytes[index];
        assert!(digit.is_ascii_digit(), "偏移探测值必须是十进制整数");
        value = value * 10 + (digit - b'0') as usize;
        index += 1;
    }
    value
}

/// 编译期字符串 → `i32`（允许 `-` 前缀，`AV_PIX_FMT_VAAPI` 等枚举是正数，这里防御性支持负数）。
#[must_use]
pub const fn parse_i32_const(raw: &str) -> i32 {
    let bytes = raw.as_bytes();
    let (negative, start) = match bytes.first() {
        Some(b'-') => (true, 1),
        _ => (false, 0),
    };
    let mut value: i32 = 0;
    let mut index = start;
    while index < bytes.len() {
        let digit = bytes[index];
        assert!(digit.is_ascii_digit(), "枚举探测值必须是十进制整数");
        value = value * 10 + (digit - b'0') as i32;
        index += 1;
    }
    if negative { -value } else { value }
}

/// `AV_PIX_FMT_VAAPI` 的枚举值（构建期从系统头文件读取）。
pub const AV_PIX_FMT_VAAPI: c_int = parse_i32_const(env!("AV_PIX_FMT_VAAPI"));
/// `AV_HWDEVICE_TYPE_VAAPI` 的枚举值。
pub const AV_HWDEVICE_TYPE_VAAPI: c_int = parse_i32_const(env!("AV_HWDEVICE_TYPE_VAAPI"));
/// `AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX`：解码器接受外部 `AVBufferRef` 设备上下文
/// （我们就是靠这个把 VA-API 设备塞进解码器）。
pub const AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX: c_int =
    parse_i32_const(env!("AVCODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX"));
/// `AV_PIX_FMT_P010LE`（10bit 4:2:0，HEVC/AV1 10bit 的常见输出格式）。
pub const AV_PIX_FMT_P010LE: c_int = parse_i32_const(env!("AV_PIX_FMT_P010LE"));
/// `AV_PIX_FMT_YUV420P10LE`。
pub const AV_PIX_FMT_YUV420P10LE: c_int = parse_i32_const(env!("AV_PIX_FMT_YUV420P10LE"));
/// 编码 id：H.264。
pub const AV_CODEC_ID_H264: c_int = parse_i32_const(env!("AV_CODEC_ID_H264"));
/// 编码 id：HEVC。
pub const AV_CODEC_ID_HEVC: c_int = parse_i32_const(env!("AV_CODEC_ID_HEVC"));
/// 编码 id：AV1。
pub const AV_CODEC_ID_AV1: c_int = parse_i32_const(env!("AV_CODEC_ID_AV1"));

/// 已知**编码** ID → 稳定短名（日志/错误里用）。
///
/// 与 [`codec_name_of`] 的区别：那个给的是 `AVCodec` 的**解码器**名
/// （`av1`、`libdav1d`、`h264`），这里给的是**编码**名 —— 排查
/// "为什么选了软解"时两者都要，且编码名跨 ffmpeg 版本稳定。
#[must_use]
pub fn codec_id_name(id: c_int) -> &'static str {
    match id {
        AV_CODEC_ID_H264 => "h264",
        AV_CODEC_ID_HEVC => "hevc",
        AV_CODEC_ID_AV1 => "av1",
        _ => "other",
    }
}

// `AVCodec.id`/`AVCodec.name` 只读，按探测偏移读；两者都不能落在结构体外。
const _: () = assert!(probed!("AVCODEC_NAME") == 0);
const _: () = assert!(probed!("AVCODEC_ID") > probed!("AVCODEC_NAME"));

// 布局断言：手写前缀必须与系统头文件一致，否则构建失败。
const _: () = assert!(std::mem::offset_of!(AvFrame, data) == probed!("AVFRAME_DATA"));
const _: () = assert!(std::mem::offset_of!(AvFrame, linesize) == probed!("AVFRAME_LINESIZE"));
const _: () = assert!(std::mem::offset_of!(AvFrame, width) == probed!("AVFRAME_WIDTH"));
const _: () = assert!(std::mem::offset_of!(AvFrame, height) == probed!("AVFRAME_HEIGHT"));
const _: () = assert!(std::mem::offset_of!(AvFrame, format) == probed!("AVFRAME_FORMAT"));
const _: () = assert!(std::mem::offset_of!(AvFrame, pts) == probed!("AVFRAME_PTS"));
const _: () = assert!(
    std::mem::offset_of!(AvFormatContext, nb_streams) == probed!("AVFORMAT_CTX_NB_STREAMS")
);
const _: () =
    assert!(std::mem::offset_of!(AvFormatContext, streams) == probed!("AVFORMAT_CTX_STREAMS"));
const _: () = assert!(std::mem::offset_of!(AvStream, codecpar) == probed!("AVSTREAM_CODECPAR"));
const _: () = assert!(std::mem::offset_of!(AvStream, time_base) == probed!("AVSTREAM_TIME_BASE"));

/// `AVStream.avg_frame_rate` 的字节偏移。
///
/// 这两个字段排在 `metadata` 之后，中间隔着一串定长字段；与其在 Rust 里把它们
/// 全部声明一遍（版本间一动就悄悄读错），不如直接用探测出的偏移做无对齐读——
/// 与 `AVCODEC_CTX_HW_DEVICE_CTX` 的写法一致。
pub const AVSTREAM_AVG_FRAME_RATE: usize = probed!("AVSTREAM_AVG_FRAME_RATE");

/// `AVStream.r_frame_rate` 的字节偏移。
pub const AVSTREAM_R_FRAME_RATE: usize = probed!("AVSTREAM_R_FRAME_RATE");

// `AVCodecHWConfig` 三个整数字段：布局同样与头文件对齐检查。
const _: () =
    assert!(std::mem::offset_of!(AvCodecHWConfig, pix_fmt) == probed!("AVCODEC_HWCONFIG_PIX_FMT"));
const _: () =
    assert!(std::mem::offset_of!(AvCodecHWConfig, methods) == probed!("AVCODEC_HWCONFIG_METHODS"));
const _: () = assert!(
    std::mem::offset_of!(AvCodecHWConfig, device_type) == probed!("AVCODEC_HWCONFIG_DEVICE_TYPE")
);

/// 按探测偏移读取一个 [`AvRational`]。
///
/// # Safety
///
/// `base` 必须指向一个 `AVStream`，且 `offset` 落在该结构体内。
pub unsafe fn avrational_at(base: *const AvStream, offset: usize) -> AvRational {
    // SAFETY: 由调用方保证 offset 在结构体内；read_unaligned 允许未对齐。
    unsafe { std::ptr::read_unaligned((base.cast::<u8>()).add(offset).cast::<AvRational>()) }
}
const _: () =
    assert!(std::mem::offset_of!(AvPacketPrefix, stream_index) == probed!("AVPACKET_STREAM_INDEX"));
const _: () = assert!(
    std::mem::offset_of!(AvCodecParameters, codec_type) == probed!("AVCODECPAR_CODEC_TYPE")
);
const _: () =
    assert!(std::mem::offset_of!(AvCodecParameters, width) == probed!("AVCODECPAR_WIDTH"));
const _: () =
    assert!(std::mem::offset_of!(AvCodecParameters, height) == probed!("AVCODECPAR_HEIGHT"));
const _: () = assert!(std::mem::offset_of!(AvBufferRef, data) == probed!("AVBUFFER_REF_DATA"));
const _: () =
    assert!(std::mem::offset_of!(AvHwDeviceContext, hwctx) == probed!("AVHWDEVICE_CTX_HWCTX"));
const _: () = assert!(
    std::mem::offset_of!(AvVaapiDeviceContext, display) == probed!("AVVAAPI_DEVICE_CTX_DISPLAY")
);

/// `AVCodecContext::hw_device_ctx` 的字节偏移（构建期探测；唯一需要写入的字段）。
pub const AVCODEC_CTX_HW_DEVICE_CTX: usize = probed!("AVCODEC_CTX_HW_DEVICE_CTX");

/// `AVCodecParameters::format` 的字节偏移（容器声明的像素格式；`-1` = 未知）。
///
/// 用它在**打开解码器之前**认出 10bit：我们的转换链只认 8bit NV12，
/// 10bit（`yuv420p10le` / `p010le`）没有任何可用路径，越早拒绝错误越可操作。
pub const AVCODECPAR_FORMAT: usize = probed!("AVCODECPAR_FORMAT");
// `format` 在 `AVCodecParameters` 里位于 time_base 之后、width/height 之前。
const _: () = assert!(AVCODECPAR_FORMAT < probed!("AVCODECPAR_WIDTH"));

/// 读 `AVCodecParameters::format`（`AVPixelFormat` 枚举值，`-1` = 未知）。
///
/// # Safety
///
/// `parameters` 必须指向 libavformat 填充过的 `AVCodecParameters`。
#[must_use]
pub unsafe fn codecpar_pix_fmt(parameters: *const AvCodecParameters) -> c_int {
    // SAFETY: 由调用方保证 parameters 有效；偏移来自构建期 offsetof，read_unaligned 防未对齐。
    unsafe {
        std::ptr::read_unaligned(
            (parameters.cast::<u8>())
                .add(AVCODECPAR_FORMAT)
                .cast::<c_int>(),
        )
    }
}

/// 像素格式名（只认我们真正会区分的那几个；其余给出 `fmt:<n>`，不猜）。
#[must_use]
pub fn pix_fmt_name(fmt: c_int) -> String {
    match fmt {
        AV_PIX_FMT_VAAPI => "vaapi".into(),
        AV_PIX_FMT_P010LE => "p010le".into(),
        AV_PIX_FMT_YUV420P10LE => "yuv420p10le".into(),
        other => format!("fmt:{other}"),
    }
}

/// 这个像素格式是不是 10bit（`p010le` / `yuv420p10le`）。
#[must_use]
pub fn is_ten_bit_pix_fmt(fmt: c_int) -> bool {
    fmt == AV_PIX_FMT_P010LE || fmt == AV_PIX_FMT_YUV420P10LE
}

// ---------------------------------------------------------------- AVERROR
/// `AVERROR(EAGAIN)`：需要更多输入 / 需要再次接收。
pub const AVERROR_EAGAIN: c_int = -11;
/// `AVERROR_EOF`：需要送入空包冲刷解码器。
pub const AVERROR_EOF: c_int = -541_478_725;
/// `AVSEEK_FLAG_BACKWARD`。
pub const AVSEEK_FLAG_BACKWARD: c_int = 1;
/// `AVMEDIA_TYPE_VIDEO`。
pub const AVMEDIA_TYPE_VIDEO: c_int = 0;

unsafe extern "C" {
    // libva-drm
    /// 由 DRM fd 取得 `VADisplay`。
    pub unsafe fn vaGetDisplayDRM(fd: c_int) -> VaDisplay;
    /// 初始化显示。
    pub unsafe fn vaInitialize(
        display: VaDisplay,
        major: *mut c_int,
        minor: *mut c_int,
    ) -> VaStatus;
    /// 释放显示。
    pub unsafe fn vaTerminate(display: VaDisplay) -> VaStatus;
    /// 状态码 → 静态字符串。
    pub unsafe fn vaErrorStr(status: VaStatus) -> *const c_char;
    /// 导出表面为 DRM PRIME 2 描述符。
    pub unsafe fn vaExportSurfaceHandle(
        display: VaDisplay,
        surface: VaSurfaceId,
        mem_type: c_uint,
        flags: c_uint,
        descriptor: *mut VaDrmPrimeSurfaceDescriptor,
    ) -> VaStatus;
    /// 等待硬件解码/后处理完成（导出 DMA-BUF 前必须同步，避免读到未写完的撕裂帧）。
    pub unsafe fn vaSyncSurface(display: VaDisplay, render_target: VaSurfaceId) -> VaStatus;

    // libavformat
    /// 打开输入。
    pub unsafe fn avformat_open_input(
        ps: *mut *mut AvFormatContext,
        url: *const c_char,
        fmt: *const AvInputFormat,
        options: *mut *mut AvDictionary,
    ) -> c_int;
    /// 读取流信息。
    pub unsafe fn avformat_find_stream_info(
        ic: *mut AvFormatContext,
        options: *mut *mut AvDictionary,
    ) -> c_int;
    /// 关闭输入。
    pub unsafe fn avformat_close_input(ps: *mut *mut AvFormatContext);
    /// 找到最佳视频流。
    pub unsafe fn av_find_best_stream(
        ic: *mut AvFormatContext,
        media_type: c_int,
        wanted_stream_nb: c_int,
        related_stream: c_int,
        decoder_ret: *mut *const AvCodec,
        flags: c_int,
    ) -> c_int;
    /// 读一个包。
    pub unsafe fn av_read_frame(ic: *mut AvFormatContext, packet: *mut AvPacket) -> c_int;
    /// seek。
    pub unsafe fn av_seek_frame(
        ic: *mut AvFormatContext,
        stream_index: c_int,
        timestamp: i64,
        flags: c_int,
    ) -> c_int;

    // libavcodec
    /// 分配解码上下文。
    /// 取编码 id 的可读名（`h264`/`hevc`…）。
    pub unsafe fn avcodec_get_name(id: c_int) -> *const std::os::raw::c_char;
    pub unsafe fn avcodec_alloc_context3(codec: *const AvCodec) -> *mut AvCodecContext;
    /// 复制编码参数到上下文。
    pub unsafe fn avcodec_parameters_to_context(
        ctx: *mut AvCodecContext,
        parameters: *const AvCodecParameters,
    ) -> c_int;
    /// 打开解码器。
    pub unsafe fn avcodec_open2(
        ctx: *mut AvCodecContext,
        codec: *const AvCodec,
        options: *mut *mut AvDictionary,
    ) -> c_int;
    /// 遍历已注册的编解码器；`opaque` 初值为空指针，返回空指针表示遍历结束。
    pub unsafe fn av_codec_iterate(opaque: *mut *mut c_void) -> *const AvCodec;
    /// 该编解码器能否当解码器用（非 0 = 可以）。
    pub unsafe fn av_codec_is_decoder(codec: *const AvCodec) -> c_int;
    /// 取第 `index` 项硬件配置；返回空指针表示没有更多项。
    pub unsafe fn avcodec_get_hw_config(
        codec: *const AvCodec,
        index: c_int,
    ) -> *const AvCodecHWConfig;
    /// 送入一个包（`None` = 冲刷）。
    pub unsafe fn avcodec_send_packet(ctx: *mut AvCodecContext, packet: *const AvPacket) -> c_int;
    /// 取出一帧。
    pub unsafe fn avcodec_receive_frame(ctx: *mut AvCodecContext, frame: *mut AvFrame) -> c_int;
    /// 释放解码上下文。
    pub unsafe fn avcodec_free_context(ctx: *mut *mut AvCodecContext);
    /// 分配包。
    pub unsafe fn av_packet_alloc() -> *mut AvPacket;
    /// 释放包。
    pub unsafe fn av_packet_free(packet: *mut *mut AvPacket);
    /// 清空包内容。
    pub unsafe fn av_packet_unref(packet: *mut AvPacket);
    /// 冲刷解码器（seek 之后必须调用）。
    pub unsafe fn avcodec_flush_buffers(ctx: *mut AvCodecContext);

    // libavutil
    /// 分配帧。
    pub unsafe fn av_frame_alloc() -> *mut AvFrame;
    /// 释放帧。
    pub unsafe fn av_frame_free(frame: *mut *mut AvFrame);
    /// 通过设备字符串创建 HW 设备。
    pub unsafe fn av_hwdevice_ctx_create(
        device_ctx: *mut *mut AvBufferRef,
        dev_type: c_int,
        device: *const c_char,
        options: *mut *mut AvDictionary,
        flags: c_int,
    ) -> c_int;
    /// 增加引用计数。
    pub unsafe fn av_buffer_ref(buffer: *mut AvBufferRef) -> *mut AvBufferRef;
    /// 减少引用计数。
    pub unsafe fn av_buffer_unref(buffer: *mut *mut AvBufferRef);
    /// 错误码 → 文本。
    pub unsafe fn av_strerror(errnum: c_int, buffer: *mut c_char, size: usize) -> c_int;
}

/// 把 ffmpeg 错误码翻译成可读文本（含码值，便于检索）。
#[must_use]
pub fn av_error_text(code: c_int) -> String {
    let mut buffer = [0 as c_char; 256];
    // SAFETY: buffer 是本地栈数组，长度与实际传入的 size 一致。
    let written = unsafe { av_strerror(code, buffer.as_mut_ptr(), buffer.len()) };
    if written < 0 {
        return format!("ffmpeg 错误码 {code}");
    }
    let text = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    format!("{}（码 {code}）", text.to_string_lossy())
}

/// 读 `AVCodec.id`（编码 id；按构建期探测的偏移读取）。
///
/// # Safety
///
/// `codec` 必须是有效的 `AVCodec`（由 ffmpeg 注册表返回）。
#[must_use]
pub unsafe fn codec_id_of(codec: *const AvCodec) -> c_int {
    // SAFETY: 由调用方保证 codec 有效；偏移来自构建期探测。
    unsafe {
        std::ptr::read_unaligned(
            (codec as *const u8)
                .add(probed!("AVCODEC_ID"))
                .cast::<c_int>(),
        )
    }
}

/// 读 `AVCodec.name`（静态字符串；解码器名如 `av1` / `libdav1d`）。
///
/// # Safety
///
/// `codec` 必须是有效的 `AVCodec`。
#[must_use]
pub unsafe fn codec_name_of(codec: *const AvCodec) -> Option<String> {
    // SAFETY: 由调用方保证 codec 有效；偏移来自构建期探测；name 是静态 C 字符串。
    let raw = unsafe {
        std::ptr::read_unaligned(
            (codec as *const u8)
                .add(probed!("AVCODEC_NAME"))
                .cast::<*const c_char>(),
        )
    };
    if raw.is_null() {
        return None;
    }
    Some(
        unsafe { std::ffi::CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// 这个解码器能不能用**外部 VA-API 设备上下文**建硬件管线。
///
/// 判据正是 ffmpeg 自己的协商方式：遍历 `avcodec_get_hw_config`，要求存在一项
/// `pix_fmt == AV_PIX_FMT_VAAPI` 且 `methods` 含
/// [`AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX`]。
///
/// 为什么需要它：`av_find_best_stream` 给的是**默认**解码器。AV1 的默认解码器是
/// `libdav1d`（纯软解，没有任何 hw config），于是即使驱动支持 AV1 VA-API 解码，
/// `avcodec_receive_frame` 也只会产出软件帧。H.264/HEVC 的默认解码器是原生
/// `h264`/`hevc`（带 VAAPI hw config），所以一直没暴露这个问题。
///
/// # Safety
///
/// `codec` 必须是有效的 `AVCodec`。
#[must_use]
pub unsafe fn codec_supports_vaapi_hw(codec: *const AvCodec) -> bool {
    let mut index: c_int = 0;
    loop {
        // SAFETY: 由调用方保证 codec 有效；index 单调递增，直到 ffmpeg 返回空指针。
        let config = unsafe { avcodec_get_hw_config(codec, index) };
        if config.is_null() {
            return false;
        }
        // SAFETY: config 非空，指向 ffmpeg 的静态配置表项。
        let (pix_fmt, methods) = unsafe { ((*config).pix_fmt, (*config).methods) };
        if pix_fmt == AV_PIX_FMT_VAAPI && (methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) != 0 {
            return true;
        }
        index += 1;
    }
}

/// `VASurfaceID` 从 `AVFrame::data[3]` 里取（ffmpeg 约定：VAAPI 帧把 id 塞在 `data[3]`）。
///
/// # Safety
///
/// 调用方必须保证 `frame` 是有效且 format 为 [`AV_PIX_FMT_VAAPI`] 的帧。
#[must_use]
pub unsafe fn surface_id_of(frame: *const AvFrame) -> VaSurfaceId {
    // SAFETY: 由调用方保证 frame 有效；data 是固定长度数组。
    let raw = unsafe { (*frame).data[3] };
    raw as usize as VaSurfaceId
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn probed_offsets_are_nonzero_and_ordered() {
        // 这些断言与编译期断言重复，但它们给出**可读的失败信息**。
        assert_eq!(std::mem::offset_of!(AvFrame, data), probed!("AVFRAME_DATA"));
        assert!(std::mem::offset_of!(AvFrame, format) > std::mem::offset_of!(AvFrame, data));
        assert_eq!(
            std::mem::offset_of!(AvHwDeviceContext, hwctx),
            probed!("AVHWDEVICE_CTX_HWCTX")
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn constants_match_expected_ranges() {
        // AV_PIX_FMT_VAAPI 在 ffmpeg 5..9 里是 44；解析失败会编译失败，这里只做范围检查。
        assert!(AV_PIX_FMT_VAAPI > 0 && AV_PIX_FMT_VAAPI < 200);
        assert!(AV_HWDEVICE_TYPE_VAAPI > 0 && AV_HWDEVICE_TYPE_VAAPI < 32);
        assert_eq!(
            AVCODEC_CTX_HW_DEVICE_CTX % std::mem::align_of::<*mut c_void>(),
            0
        );
    }

    #[test]
    fn av_error_text_includes_code() {
        let text = av_error_text(AVERROR_EAGAIN);
        assert!(text.contains("-11"), "实际: {text}");
    }
}
