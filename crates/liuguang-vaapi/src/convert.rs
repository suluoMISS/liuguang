//! NV12 → XRGB8888 的 GPU 转换（EGL + GLES2）。
//!
//! # 为什么需要这一层
//!
//! 有些合成器（denial 0.6.x，Impeller 后端）把**多平面 YUV** 的 dma-buf 只采样到
//! Y 平面，整屏发红。把 NV12 在 GPU 上转成**单平面 `XRGB8888`** 之后，合成器只需
//! 按 RGB 处理，这条路径就恢复正常（哪些合成器需要走这里见
//! `apps/liuguang/src/compositor_profile.rs` 的 `KNOWN_LUMA_ONLY`）。
//!
//! 实现提炼自 `examples/egl_convert.rs`（commit 8f444b8，NVIDIA RTX 4060 + 620.70
//! 真机端到端验证）：示例里的回读、PPM 导出、扩展打印都不进产品路径，这里只留
//! "导入两个平面 → 画到输出 BO → 把输出 BO 交出去"。
//!
//! # 几个必须照抄的细节
//!
//! * **`eglCreateImageKHR` 手写**：`khronos-egl 6.0` 的 `create_image` 把
//!   `Attrib` 定义成 `usize`（见其 `egl1_0::Attrib`），而 EGL 的 ABI 是 32 位
//!   `EGLint`：属性表按 `usize` 填会在 64 位平台上错位，驱动直接返回
//!   `EGL_BAD_ATTRIBUTE`。所以这里用 `eglGetProcAddress` 自己取入口、按
//!   `*const i32` 传属性表。
//! * **UV 平面有两种 fourcc 方言**：NVIDIA 只接受 `RG88`（`DRM_FORMAT_RG88`），
//!   Mesa/Intel 只接受 `GR88`（`DRM_FORMAT_GR88`），认错的那个返回 `NULL` +
//!   `EGL_BAD_MATCH`。两者的 U/V 顺序还相反，所以换方言时必须打开着色器里的
//!   `u_uv_swap`。
//! * **输出 BO 必须把 modifier 原样交出去**：NVIDIA gbm 对 `XRGB8888` 只给
//!   block-linear（真机实测 `0x0300000000E08014`），`RENDERING|LINEAR` 直接
//!   `EINVAL`；把 `bo.modifier()` 填进 [`DmaBufFrame::modifier`] 才能让
//!   `zwp_linux_dmabuf_v1` 的 filter 通过。
//!
//! # 缓冲生命周期
//!
//! 输出 BO 一旦被合成器引用就不能再画（否则屏幕上会看到撕裂/回退），所以在
//! [`RgbConverter::SLOTS`] 个槽位之间轮转，并给每个交出去的帧挂 [`SlotGuard`]
//! （放进 [`DmaBufFrame::keepalive`]）。合成器 `release` 掉旧帧时 guard 归还槽位；
//! 如果轮到的槽位仍被占用，[`RgbConverter::convert`] 返回 [`VaapiError::Busy`]，
//! 由上层退避（不要在这里死等：4K60 下等一帧就会堵死解码线程）。
//!
//! # 线程
//!
//! EGL 上下文只能在"当前线程"上使用：构造时在 daemon 线程建好 display/context
//! 并编译着色器，然后立刻 `eglMakeCurrent(None)` 释放；之后整个转换器移动到播放
//! 线程，每次 [`RgbConverter::convert`] 前后各自 make current / release。
//! 因此这里 `unsafe impl Send for RgbConverter`（同一时刻只有一个线程用它，
//! 与 `decoder.rs` 里 `VaapiDecoder` 的处理一致）。
//!
//! # 同步
//!
//! 每帧结束时 `glFinish()`：NVDEC 的 NV12 表面在采样完成后才能还给解码器复用，
//! 否则会出现花屏/闪烁（示例里同样这么做，只是那里主要是为了回读）。
#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gbm::AsRaw;
use khronos_egl as egl;
use liuguang_core::fit::{FitParams, compute_fit};
use liuguang_core::transform::{RenderTransform, Rotation};
use liuguang_core::types::{DmaBufFrame, FrameKeepalive, PixelFormat};

use crate::{VaapiError, export};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// Y 平面：单通道 8 bit。
const DRM_FORMAT_R8: u32 = 0x2020_3852;
/// UV 平面（NVIDIA 方言，或通用 LE 解释 `[15:0] R:G`）。
const DRM_FORMAT_RG88: u32 = 0x3838_4752;
/// UV 平面（Mesa/Intel 方言：`[15:0] G:R` LE，内存字节序 `[U, V]`）。
const DRM_FORMAT_GR88: u32 = 0x3838_5247;
/// DRM 隐式 modifier（`DRM_FORMAT_MOD_INVALID`），`0` 则是 `DRM_FORMAT_MOD_LINEAR`。
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_PLANE0_FD: i32 = 0x3272;
const EGL_PLANE0_OFFSET: i32 = 0x3273;
const EGL_PLANE0_PITCH: i32 = 0x3274;
const EGL_PLANE0_MODIFIER_LO: i32 = 0x3443;
const EGL_PLANE0_MODIFIER_HI: i32 = 0x3444;
const EGL_WIDTH_I32: i32 = 0x3057;
const EGL_HEIGHT_I32: i32 = 0x3056;
const EGL_NONE_I32: i32 = 0x3038;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_NEAREST: i32 = 0x2600;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_INFO_LOG_LENGTH: u32 = 0x8B84;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_TRIANGLE_STRIP: u32 = 0x0005;
const GL_FLOAT: u32 = 0x1406;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_FALSE: u8 = 0;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;

/// 全屏两个三角形（`GL_TRIANGLE_STRIP`）的裁剪空间坐标。
///
/// 纹理坐标按 `GL_TRIANGLE_STRIP` 顺序配：`v=0` 是纹理（EGLImage）的第一行，
/// 也就是**画面顶部**；`v=1` 是底部。按显示尺寸渲染时这四个坐标由
/// [`RgbConverter::cover_texcoords`] 算成 cover 裁剪后的子矩形。
const QUAD_POSITIONS: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];

const VERTEX_SHADER: &str = "\
#version 100
attribute vec2 a_position;
attribute vec2 a_texcoord;
uniform vec2 u_tex_scale;
varying vec2 v_texcoord;
void main() {
    // Y 取反是**方向**修复，不是风格问题（真机实测：不翻就整幅倒像）。
    //
    // GL 的裁剪空间 +Y 朝上、帧缓冲第 0 行在**下**；而 dma-buf 的第 0 行
    // 被合成器当成**画面最上**一行。不翻的话，写进 BO 第 0 行的是画面
    // 最后一行 ⇒ 上屏整幅上下颠倒（颜色对、方向错，正是真机看到的现象）。
    // 这里把几何沿 Y 镜像，`a_texcoord` 不用动：镜像后落在帧缓冲下部的
    // 顶点取到的是源图下部的纹素，BO 第 0 行就是源图第 0 行。
    gl_Position = vec4(a_position.x, -a_position.y, 0.0, 1.0);
    // 解码表面按高度 16 对齐分配（1080 → 1088、1530 → 1536），底部多出的行是
    // 垃圾；`u_tex_scale` 把 0..1 的采样范围收窄到**可见区域**，否则会露出
    // 一条底边、且画面被纵向压缩。见 `RgbConverter::set_visible_size`。
    v_texcoord = a_texcoord * u_tex_scale;
}
";

/// 两个平面分别采样后手动做 YUV→RGB。
///
/// `yuv = vec3(Y, U, V) - u_offset` 把有限范围的码值挪到 0 附近，
/// `u_matrix` 的列依次是 Y / U / V 对 (R, G, B) 的贡献；
/// `u_uv_swap` 处理 `RG88` / `GR88` 两种方言的 U/V 顺序。
const FRAGMENT_SHADER: &str = "\
#version 100
precision mediump float;
uniform sampler2D u_tex_y;
uniform sampler2D u_tex_uv;
uniform mat3 u_matrix;
uniform vec3 u_offset;
uniform int u_uv_swap;
uniform float u_brightness;
varying vec2 v_texcoord;
void main() {
    float y = texture2D(u_tex_y, v_texcoord).r;
    vec2 uv = texture2D(u_tex_uv, v_texcoord).rg;
    uv = mix(uv, uv.yx, float(u_uv_swap));
    vec3 yuv = vec3(y, uv.x, uv.y) - u_offset;
    vec3 rgb = clamp(u_matrix * yuv, 0.0, 1.0) * clamp(u_brightness, 0.1, 1.0);
    gl_FragColor = vec4(rgb, 1.0);
}
";

// ---------------------------------------------------------------------------
// 纯逻辑：UV 方言 / 色彩矩阵 / 平面描述 / 输出帧
// ---------------------------------------------------------------------------

/// UV 平面的 fourcc 方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UvDialect {
    /// NVIDIA 方言：`DRM_FORMAT_RG88`（NVIDIA 专有 EGL 把它映射到 GL_RG8；标准 DRM 驱动则 `.r=V, .g=U` 需交换）。
    Rg88,
    /// Mesa/Intel/AMD 标准 DRM 方言：`DRM_FORMAT_GR88`（`[15:0] G:R` 小端，内存字节 0=U→`.r`，字节 1=V→`.g`，不交换）。
    Gr88,
}

impl UvDialect {
    /// 首选方言：Mesa/Intel/AMD 原生认 DRM 标准的 `GR88`；NVIDIA 拒掉 `GR88` 后会精确回退 `RG88`。
    const PREFERRED: Self = Self::Gr88;

    fn fourcc(self) -> u32 {
        match self {
            Self::Rg88 => DRM_FORMAT_RG88,
            Self::Gr88 => DRM_FORMAT_GR88,
        }
    }

    /// 采样到这个方言时是否需要把 `.rg` 交换成 `.gr`。
    fn swaps_uv(self, is_nvidia: bool) -> bool {
        match self {
            Self::Gr88 => false,
            Self::Rg88 => !is_nvidia,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rg88 => "RG88",
            Self::Gr88 => "GR88",
        }
    }

    /// 导入失败时该回退到哪种方言（两种方言互为唯一替代）。
    fn fallback(self) -> Self {
        match self {
            Self::Rg88 => Self::Gr88,
            Self::Gr88 => Self::Rg88,
        }
    }
}

/// 色彩矩阵。目前产品路径固定 BT.709（4K 素材基本都是），保留枚举是为了
/// 让"系数是否写对"能在单测里钉住，将来要按 `VUI colour_primaries` 切换时也方便。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matrix {
    /// 保留给将来按素材的 `VUI colour_primaries` 切换（BT.601 素材在 4K 里很少见）；
    /// 单测会用到，产品路径当前固定 BT.709。
    #[allow(dead_code)]
    Bt601,
    Bt709,
}

impl Matrix {
    /// 列主序的 3×3 系数矩阵（列 = Y、U、V 的系数）。
    ///
    /// 有限范围换算：`Y' = (Y - 16) / 219`、`U' = (U - 128) / 224`、
    /// `V' = (V - 128) / 224`，与 `examples/egl_convert.rs` 的真机验证值一致。
    fn coefficients(self) -> [f32; 9] {
        match self {
            // R = 1.164 Y' + 1.596 V'
            // G = 1.164 Y' - 0.392 U' - 0.813 V'
            // B = 1.164 Y' + 2.017 U'
            Self::Bt601 => [
                1.164, 1.164, 1.164, // Y 的系数
                0.0, -0.392, 2.017, // U 的系数
                1.596, -0.813, 0.0, // V 的系数
            ],
            // R = 1.164 Y' + 1.793 V'
            // G = 1.164 Y' - 0.213 U' - 0.533 V'
            // B = 1.164 Y' + 2.112 U'
            Self::Bt709 => [
                1.164, 1.164, 1.164, // Y 的系数
                0.0, -0.213, 2.112, // U 的系数
                1.793, -0.533, 0.0, // V 的系数
            ],
        }
    }

    /// 偏移量：把 8 bit 的有限范围码值转成以 0 为中心的分量。
    fn offset(self) -> [f32; 3] {
        [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]
    }
}

/// 产品路径使用的色彩矩阵。
const OUTPUT_MATRIX: Matrix = Matrix::Bt709;

/// 一个 EGLImage 导入所需的平面参数（fd 是借用，不负责关闭）。
#[derive(Debug, Clone, Copy)]
struct PlaneDesc {
    label: &'static str,
    width: u32,
    height: u32,
    fourcc: u32,
    fd: i32,
    offset: u32,
    pitch: u32,
    modifier: Option<u64>,
}

/// 把 [`PlaneDesc`] 展开成 `eglCreateImageKHR` 要的属性表（`EGLint` 对）。
fn push_plane_attributes(attributes: &mut Vec<i32>, plane: &PlaneDesc) {
    attributes.extend([
        EGL_WIDTH_I32,
        plane.width as i32,
        EGL_HEIGHT_I32,
        plane.height as i32,
        EGL_LINUX_DRM_FOURCC_EXT,
        plane.fourcc as i32,
        EGL_PLANE0_FD,
        plane.fd,
        EGL_PLANE0_OFFSET,
        plane.offset as i32,
        EGL_PLANE0_PITCH,
        plane.pitch as i32,
    ]);
    if let Some(modifier) = plane
        .modifier
        .filter(|value| *value != DRM_FORMAT_MOD_INVALID)
    {
        // modifier 是 64 位，EGL 用 LO/HI 两个 32 位属性传；DRM_FORMAT_MOD_INVALID 时不能带（0 是 LINEAR，合法可带）。
        attributes.extend([
            EGL_PLANE0_MODIFIER_LO,
            (modifier as u32) as i32,
            EGL_PLANE0_MODIFIER_HI,
            ((modifier >> 32) as u32) as i32,
        ]);
    }
    attributes.push(EGL_NONE_I32);
}

/// NV12 两个平面在输入帧里的原始 fd/offset/pitch。
#[derive(Debug, Clone, Copy)]
struct RawPlanes {
    y_fd: i32,
    y_offset: u32,
    y_pitch: u32,
    uv_fd: i32,
    uv_offset: u32,
    uv_pitch: u32,
}

/// 从 [`DmaBufFrame`] 里取出 NV12 的两个平面。
///
/// 导出侧约定：`object_index[plane]` 指向 `fds` 里的第几个对象（NVIDIA 是 Y/UV
/// 各一个 fd，Intel 通常同一个 fd 的两个 offset）。
fn nv12_planes(frame: &DmaBufFrame) -> Result<RawPlanes, VaapiError> {
    if frame.fourcc != export::DRM_FORMAT_NV12 {
        return Err(VaapiError::Unsupported(format!(
            "RGB 转换只接受 NV12 输入，实际 fourcc=0x{:08X}",
            frame.fourcc
        )));
    }
    let lookup = |plane: &str, index: u8| -> Result<i32, VaapiError> {
        let object = usize::from(index);
        let fd = frame.fds.get(object).ok_or_else(|| {
            VaapiError::Unsupported(format!(
                "{plane} 平面的对象下标 {object} 超出 fds（共 {} 个）",
                frame.fds.len()
            ))
        })?;
        Ok(fd.as_raw_fd())
    };
    let y_fd = lookup("Y", frame.object_index[0])?;
    let uv_fd = lookup("UV", frame.object_index[1])?;
    if frame.strides[0] == 0 || frame.strides[1] == 0 {
        return Err(VaapiError::Unsupported(format!(
            "NV12 的 plane pitch 不能为 0（strides=[{}, {}, {}, {}]）",
            frame.strides[0], frame.strides[1], frame.strides[2], frame.strides[3]
        )));
    }
    Ok(RawPlanes {
        y_fd,
        y_offset: frame.offsets[0],
        y_pitch: frame.strides[0],
        uv_fd,
        uv_offset: frame.offsets[1],
        uv_pitch: frame.strides[1],
    })
}

/// 构造交出去的 XRGB8888 帧：单 object、单 fd、`object_index` 全 0。
fn output_frame(
    width: u32,
    height: u32,
    fd: Arc<OwnedFd>,
    stride: u32,
    offset: u32,
    modifier: u64,
    keepalive: FrameKeepalive,
) -> DmaBufFrame {
    DmaBufFrame {
        width,
        height,
        stride,
        format: PixelFormat::Xrgb8888,
        fds: vec![fd],
        modifier,
        fourcc: export::DRM_FORMAT_XRGB8888,
        strides: [stride, 0, 0, 0],
        offsets: [offset, 0, 0, 0],
        object_index: [0, 0, 0, 0],
        keepalive: Some(keepalive),
    }
}

// ---------------------------------------------------------------------------
// EGL 入口点（hand-written：khronos-egl 的 Attrib 宽度不对）
// ---------------------------------------------------------------------------

type EglInstance = egl::DynamicInstance<egl::EGL1_5>;
type EglCreateImageKhrFn =
    extern "system" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type EglCreateImageCoreFn =
    extern "system" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const isize) -> *mut c_void;
type EglDestroyImageFn = extern "system" fn(*mut c_void, *mut c_void) -> u32;
type EglGetErrorFn = extern "system" fn() -> i32;

const EGL_CONTEXT_LOST_CODE: i32 = 0x300E;

#[derive(Clone, Copy)]
enum EglCreateImageFn {
    /// KHR 扩展版本 `eglCreateImageKHR`，属性数组元素为 32 位 `EGLint` (`i32`)。
    Khr(EglCreateImageKhrFn),
    /// EGL 1.5 核心版本 `eglCreateImage`，属性数组元素为指针宽度 `EGLAttrib` (`isize`)。
    Core(EglCreateImageCoreFn),
}

impl EglCreateImageFn {
    fn call(
        self,
        display: *mut c_void,
        context: *mut c_void,
        target: u32,
        buffer: *mut c_void,
        attributes: &[i32],
    ) -> *mut c_void {
        match self {
            Self::Khr(func) => func(display, context, target, buffer, attributes.as_ptr()),
            Self::Core(func) => {
                let widened: Vec<isize> = attributes.iter().map(|&v| v as isize).collect();
                func(display, context, target, buffer, widened.as_ptr())
            }
        }
    }
}

fn init_error(node: &Path, message: impl Into<String>) -> VaapiError {
    VaapiError::Init {
        node: node.to_path_buf(),
        message: message.into(),
    }
}

fn load_symbol<T: Copy>(instance: &EglInstance, name: &str) -> Result<T, String> {
    let symbol = instance
        .get_proc_address(name)
        .ok_or_else(|| format!("eglGetProcAddress 没有返回 {name}"))?;
    if std::mem::size_of::<T>() != std::mem::size_of::<extern "system" fn()>() {
        return Err(format!(
            "{name} 的声明大小（{}）与函数指针（{}）不一致",
            std::mem::size_of::<T>(),
            std::mem::size_of::<extern "system" fn()>()
        ));
    }
    // SAFETY: 上面已确认尺寸一致，且 `name` 就是该入口的规范名字；具体签名由
    // Khronos 规范（`T`）给出，驱动按 ABI 调用即可。
    Ok(unsafe { std::mem::transmute_copy::<extern "system" fn(), T>(&symbol) })
}

fn egl_error_name(code: i32) -> &'static str {
    match code {
        0x3000 => "EGL_SUCCESS",
        0x3001 => "EGL_NOT_INITIALIZED",
        0x3002 => "EGL_BAD_ACCESS",
        0x3003 => "EGL_BAD_ALLOC",
        0x3004 => "EGL_BAD_ATTRIBUTE",
        0x3005 => "EGL_BAD_CONFIG",
        0x3006 => "EGL_BAD_CONTEXT",
        0x3007 => "EGL_BAD_CURRENT_SURFACE",
        0x3008 => "EGL_BAD_DISPLAY",
        0x3009 => "EGL_BAD_MATCH",
        0x300A => "EGL_BAD_NATIVE_PIXMAP",
        0x300B => "EGL_BAD_NATIVE_WINDOW",
        0x300C => "EGL_BAD_PARAMETER",
        0x300D => "EGL_BAD_SURFACE",
        0x300E => "EGL_CONTEXT_LOST",
        0x300F => "EGL_BAD_STREAM",
        _ => "未知 EGL 错误",
    }
}

/// 三个手取的 EGL KHR 入口。
struct EglRaw {
    create_image: EglCreateImageFn,
    destroy_image: EglDestroyImageFn,
    get_error: EglGetErrorFn,
}

impl EglRaw {
    fn load(instance: &EglInstance) -> Result<Self, String> {
        let create_image = match load_symbol::<EglCreateImageKhrFn>(instance, "eglCreateImageKHR") {
            Ok(symbol) => EglCreateImageFn::Khr(symbol),
            Err(khr) => {
                let symbol = load_symbol::<EglCreateImageCoreFn>(instance, "eglCreateImage")
                    .map_err(|core| {
                        format!(
                            "加载 eglCreateImageKHR 失败（{khr}），eglCreateImage 也失败（{core}）"
                        )
                    })?;
                EglCreateImageFn::Core(symbol)
            }
        };
        let destroy_image = match load_symbol::<EglDestroyImageFn>(instance, "eglDestroyImageKHR") {
            Ok(symbol) => symbol,
            Err(khr) => {
                load_symbol::<EglDestroyImageFn>(instance, "eglDestroyImage").map_err(|core| {
                    format!(
                        "加载 eglDestroyImageKHR 失败（{khr}），eglDestroyImage 也失败（{core}）"
                    )
                })?
            }
        };
        let get_error = load_symbol::<EglGetErrorFn>(instance, "eglGetError")
            .map_err(|message| format!("加载 eglGetError 失败: {message}"))?;
        Ok(Self {
            create_image,
            destroy_image,
            get_error,
        })
    }

    fn take_error_code(&self) -> i32 {
        (self.get_error)()
    }
}

/// `EGLImage` 的 RAII 包装：drop 时 `eglDestroyImageKHR`。
struct EglImage {
    display: *mut c_void,
    image: *mut c_void,
    destroy: EglDestroyImageFn,
}

impl Drop for EglImage {
    fn drop(&mut self) {
        if !self.image.is_null() {
            // image 由本结构独占；display 在 `Renderer` 里比它活得久
            // （`RgbConverter` 的字段顺序保证了这一点）。
            (self.destroy)(self.display, self.image);
            self.image = ptr::null_mut();
        }
    }
}

// ---------------------------------------------------------------------------
// GLES2 入口点
// ---------------------------------------------------------------------------

type GlCreateShader = extern "system" fn(u32) -> u32;
type GlShaderSource = extern "system" fn(u32, i32, *const *const c_char, *const i32);
type GlCompileShader = extern "system" fn(u32);
type GlGetShaderiv = extern "system" fn(u32, u32, *mut i32);
type GlGetShaderInfoLog = extern "system" fn(u32, i32, *mut i32, *mut c_char);
type GlDeleteShader = extern "system" fn(u32);
type GlCreateProgram = extern "system" fn() -> u32;
type GlAttachShader = extern "system" fn(u32, u32);
type GlBindAttribLocation = extern "system" fn(u32, u32, *const c_char);
type GlLinkProgram = extern "system" fn(u32);
type GlGetProgramiv = extern "system" fn(u32, u32, *mut i32);
type GlGetProgramInfoLog = extern "system" fn(u32, i32, *mut i32, *mut c_char);
type GlDeleteProgram = extern "system" fn(u32);
type GlUseProgram = extern "system" fn(u32);
type GlGetUniformLocation = extern "system" fn(u32, *const c_char) -> i32;
type GlUniform1f = extern "system" fn(i32, f32);
type GlUniform1i = extern "system" fn(i32, i32);
type GlUniform2f = extern "system" fn(i32, f32, f32);
type GlUniform3f = extern "system" fn(i32, f32, f32, f32);
type GlUniformMatrix3fv = extern "system" fn(i32, i32, u8, *const f32);
type GlGenTextures = extern "system" fn(i32, *mut u32);
type GlDeleteTextures = extern "system" fn(i32, *const u32);
type GlBindTexture = extern "system" fn(u32, u32);
type GlActiveTexture = extern "system" fn(u32);
type GlTexParameteri = extern "system" fn(u32, u32, i32);
type GlGenFramebuffers = extern "system" fn(i32, *mut u32);
type GlDeleteFramebuffers = extern "system" fn(i32, *const u32);
type GlBindFramebuffer = extern "system" fn(u32, u32);
type GlFramebufferTexture2d = extern "system" fn(u32, u32, u32, u32, i32);
type GlCheckFramebufferStatus = extern "system" fn(u32) -> u32;
type GlViewport = extern "system" fn(i32, i32, i32, i32);
type GlClearColor = extern "system" fn(f32, f32, f32, f32);
type GlClear = extern "system" fn(u32);
type GlDrawArrays = extern "system" fn(u32, i32, i32);
type GlEnableVertexAttribArray = extern "system" fn(u32);
type GlVertexAttribPointer = extern "system" fn(u32, i32, u32, u8, i32, *const c_void);
type GlFinish = extern "system" fn();
type GlGetError = extern "system" fn() -> u32;
type GlEglImageTargetTexture2d = extern "system" fn(u32, *mut c_void);
type GlReadPixels = extern "system" fn(i32, i32, i32, i32, u32, u32, *mut c_void);
type GlPixelStorei = extern "system" fn(u32, i32);

macro_rules! gl_entry {
    ($instance:expr, $node:expr, $name:literal, $signature:ty) => {
        load_symbol::<$signature>($instance, $name).map_err(|message| init_error($node, message))?
    };
}

struct Gl {
    create_shader: GlCreateShader,
    shader_source: GlShaderSource,
    compile_shader: GlCompileShader,
    get_shaderiv: GlGetShaderiv,
    get_shader_info_log: GlGetShaderInfoLog,
    delete_shader: GlDeleteShader,
    create_program: GlCreateProgram,
    attach_shader: GlAttachShader,
    bind_attrib_location: GlBindAttribLocation,
    link_program: GlLinkProgram,
    get_programiv: GlGetProgramiv,
    get_program_info_log: GlGetProgramInfoLog,
    delete_program: GlDeleteProgram,
    use_program: GlUseProgram,
    get_uniform_location: GlGetUniformLocation,
    uniform_1f: GlUniform1f,
    uniform_1i: GlUniform1i,
    uniform_2f: GlUniform2f,
    uniform_3f: GlUniform3f,
    uniform_matrix_3fv: GlUniformMatrix3fv,
    gen_textures: GlGenTextures,
    delete_textures: GlDeleteTextures,
    bind_texture: GlBindTexture,
    active_texture: GlActiveTexture,
    tex_parameteri: GlTexParameteri,
    gen_framebuffers: GlGenFramebuffers,
    delete_framebuffers: GlDeleteFramebuffers,
    bind_framebuffer: GlBindFramebuffer,
    framebuffer_texture_2d: GlFramebufferTexture2d,
    check_framebuffer_status: GlCheckFramebufferStatus,
    viewport: GlViewport,
    clear_color: GlClearColor,
    clear: GlClear,
    draw_arrays: GlDrawArrays,
    enable_vertex_attrib_array: GlEnableVertexAttribArray,
    vertex_attrib_pointer: GlVertexAttribPointer,
    finish: GlFinish,
    get_error: GlGetError,
    egl_image_target_texture_2d: GlEglImageTargetTexture2d,
    read_pixels: GlReadPixels,
    pixel_storei: GlPixelStorei,
}

impl Gl {
    fn load(instance: &EglInstance, node: &Path) -> Result<Self, VaapiError> {
        Ok(Self {
            create_shader: gl_entry!(instance, node, "glCreateShader", GlCreateShader),
            shader_source: gl_entry!(instance, node, "glShaderSource", GlShaderSource),
            compile_shader: gl_entry!(instance, node, "glCompileShader", GlCompileShader),
            get_shaderiv: gl_entry!(instance, node, "glGetShaderiv", GlGetShaderiv),
            get_shader_info_log: gl_entry!(
                instance,
                node,
                "glGetShaderInfoLog",
                GlGetShaderInfoLog
            ),
            delete_shader: gl_entry!(instance, node, "glDeleteShader", GlDeleteShader),
            create_program: gl_entry!(instance, node, "glCreateProgram", GlCreateProgram),
            attach_shader: gl_entry!(instance, node, "glAttachShader", GlAttachShader),
            bind_attrib_location: gl_entry!(
                instance,
                node,
                "glBindAttribLocation",
                GlBindAttribLocation
            ),
            link_program: gl_entry!(instance, node, "glLinkProgram", GlLinkProgram),
            get_programiv: gl_entry!(instance, node, "glGetProgramiv", GlGetProgramiv),
            get_program_info_log: gl_entry!(
                instance,
                node,
                "glGetProgramInfoLog",
                GlGetProgramInfoLog
            ),
            delete_program: gl_entry!(instance, node, "glDeleteProgram", GlDeleteProgram),
            use_program: gl_entry!(instance, node, "glUseProgram", GlUseProgram),
            get_uniform_location: gl_entry!(
                instance,
                node,
                "glGetUniformLocation",
                GlGetUniformLocation
            ),
            uniform_1f: gl_entry!(instance, node, "glUniform1f", GlUniform1f),
            uniform_1i: gl_entry!(instance, node, "glUniform1i", GlUniform1i),
            uniform_2f: gl_entry!(instance, node, "glUniform2f", GlUniform2f),
            uniform_3f: gl_entry!(instance, node, "glUniform3f", GlUniform3f),
            uniform_matrix_3fv: gl_entry!(instance, node, "glUniformMatrix3fv", GlUniformMatrix3fv),
            gen_textures: gl_entry!(instance, node, "glGenTextures", GlGenTextures),
            delete_textures: gl_entry!(instance, node, "glDeleteTextures", GlDeleteTextures),
            bind_texture: gl_entry!(instance, node, "glBindTexture", GlBindTexture),
            active_texture: gl_entry!(instance, node, "glActiveTexture", GlActiveTexture),
            tex_parameteri: gl_entry!(instance, node, "glTexParameteri", GlTexParameteri),
            gen_framebuffers: gl_entry!(instance, node, "glGenFramebuffers", GlGenFramebuffers),
            delete_framebuffers: gl_entry!(
                instance,
                node,
                "glDeleteFramebuffers",
                GlDeleteFramebuffers
            ),
            bind_framebuffer: gl_entry!(instance, node, "glBindFramebuffer", GlBindFramebuffer),
            framebuffer_texture_2d: gl_entry!(
                instance,
                node,
                "glFramebufferTexture2D",
                GlFramebufferTexture2d
            ),
            check_framebuffer_status: gl_entry!(
                instance,
                node,
                "glCheckFramebufferStatus",
                GlCheckFramebufferStatus
            ),
            viewport: gl_entry!(instance, node, "glViewport", GlViewport),
            clear_color: gl_entry!(instance, node, "glClearColor", GlClearColor),
            clear: gl_entry!(instance, node, "glClear", GlClear),
            draw_arrays: gl_entry!(instance, node, "glDrawArrays", GlDrawArrays),
            enable_vertex_attrib_array: gl_entry!(
                instance,
                node,
                "glEnableVertexAttribArray",
                GlEnableVertexAttribArray
            ),
            vertex_attrib_pointer: gl_entry!(
                instance,
                node,
                "glVertexAttribPointer",
                GlVertexAttribPointer
            ),
            finish: gl_entry!(instance, node, "glFinish", GlFinish),
            get_error: gl_entry!(instance, node, "glGetError", GlGetError),
            egl_image_target_texture_2d: gl_entry!(
                instance,
                node,
                "glEGLImageTargetTexture2DOES",
                GlEglImageTargetTexture2d
            ),
            read_pixels: gl_entry!(instance, node, "glReadPixels", GlReadPixels),
            pixel_storei: gl_entry!(instance, node, "glPixelStorei", GlPixelStorei),
        })
    }

    /// 排空 GL 错误队列，返回第一条（其余丢弃：驱动可能同时报多个）。
    fn take_error(&self, context: &str) -> Option<String> {
        let mut first = None;
        loop {
            let code = (self.get_error)();
            if code == 0 {
                return first;
            }
            if first.is_none() {
                first = Some(format!("{context}: GL 错误 0x{code:04X}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Renderer：EGL display + context + 着色器 + 输出目标
// ---------------------------------------------------------------------------

struct Renderer {
    node: PathBuf,
    instance: EglInstance,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    surface: Option<egl::Surface>,
    raw: EglRaw,
    gl: Gl,
    program: u32,
    matrix_location: i32,
    offset_location: i32,
    uv_swap_location: i32,
    brightness_location: i32,
    y_sampler_location: i32,
    uv_sampler_location: i32,
    tex_scale_location: i32,
    /// 输入纹理上 `0..1` 采样范围对应的可见比例（`可见/分配`）。
    tex_scale: [f32; 2],
    width: u32,
    height: u32,
}

struct DrawArgs<'a> {
    framebuffer: u32,
    y_texture: u32,
    uv_texture: u32,
    uv_swap: bool,
    matrix: Matrix,
    positions: &'a [f32; 8],
    texcoords: &'a [f32; 8],
    brightness: f32,
}

impl Renderer {
    fn new(
        node: &Path,
        gbm_device: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<Self, VaapiError> {
        // SAFETY: `load_required_from_filename` 只做 dlopen。
        let instance = unsafe { EglInstance::load_required_from_filename("libEGL.so.1") }
            .map_err(|error| init_error(node, format!("加载 libEGL.so.1 失败: {error}")))?;
        let raw = EglRaw::load(&instance).map_err(|message| init_error(node, message))?;
        // SAFETY: `gbm_device` 指向的 gbm 设备由 `RgbConverter` 的 `_device` 持有，
        // 生命周期覆盖整个 EGL display。
        let display = unsafe {
            instance.get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, &[egl::ATTRIB_NONE])
        }
        .map_err(|error| {
            init_error(
                node,
                format!("eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR) 失败: {error}"),
            )
        })?;
        let (major, minor) = match instance.initialize(display) {
            Ok(version) => version,
            Err(error) => {
                let _ = instance.terminate(display);
                return Err(init_error(node, format!("eglInitialize 失败: {error}")));
            }
        };
        tracing::debug!(node = %node.display(), major, minor, "EGL 已初始化");
        if let Err(error) = instance.bind_api(egl::OPENGL_ES_API) {
            let _ = instance.terminate(display);
            return Err(init_error(
                node,
                format!("eglBindAPI(EGL_OPENGL_ES_API) 失败: {error}"),
            ));
        }
        let config = match choose_config(&instance, display) {
            Ok(config) => config,
            Err(error) => {
                let _ = instance.terminate(display);
                return Err(error);
            }
        };
        let context = match instance.create_context(
            display,
            config,
            None,
            &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
        ) {
            Ok(context) => context,
            Err(error) => {
                let _ = instance.terminate(display);
                return Err(init_error(
                    node,
                    format!("eglCreateContext(ES2) 失败: {error}"),
                ));
            }
        };
        let gl = match Gl::load(&instance, node) {
            Ok(gl) => gl,
            Err(error) => {
                let _ = instance.destroy_context(display, context);
                let _ = instance.terminate(display);
                return Err(error);
            }
        };
        // 从这里开始，任何失败都由 `Drop` 兜底（它知道怎么销毁 context/display）。
        let mut renderer = Self {
            node: node.to_path_buf(),
            instance,
            display,
            config,
            context,
            surface: None,
            raw,
            gl,
            program: 0,
            matrix_location: -1,
            offset_location: -1,
            uv_swap_location: -1,
            brightness_location: -1,
            y_sampler_location: -1,
            uv_sampler_location: -1,
            tex_scale_location: -1,
            tex_scale: [1.0, 1.0],
            width,
            height,
        };
        renderer.attach_surface()?;
        if let Err(err) = renderer.build_program() {
            renderer.release();
            return Err(err);
        }
        renderer.release();
        Ok(renderer)
    }

    fn node_error(&self, message: impl Into<String>) -> VaapiError {
        init_error(&self.node, message)
    }

    fn convert_error(&self, message: impl Into<String>) -> VaapiError {
        VaapiError::Convert(message.into())
    }

    /// 让 EGL 上下文在当前线程生效。
    fn acquire(&self) -> Result<(), VaapiError> {
        self.instance
            .make_current(self.display, self.surface, self.surface, Some(self.context))
            .map_err(|error| {
                if matches!(error, egl::Error::ContextLost) {
                    crate::decoder::forget_device(&self.node);
                }
                self.convert_error(format!("eglMakeCurrent 失败: {error}"))
            })
    }

    /// 释放 EGL 上下文（失败只打日志：不能让清理路径把播放线程带崩）。
    fn release(&self) {
        if let Err(error) = self.instance.make_current(self.display, None, None, None) {
            tracing::debug!(%error, "eglMakeCurrent(None) 失败");
        }
    }

    /// 优先 surfaceless（NVIDIA 支持），不行就建个 pbuffer 顶着。
    fn attach_surface(&mut self) -> Result<(), VaapiError> {
        if self
            .instance
            .make_current(self.display, None, None, Some(self.context))
            .is_ok()
        {
            return Ok(());
        }
        let attributes = [
            egl::WIDTH,
            self.width as egl::Int,
            egl::HEIGHT,
            self.height as egl::Int,
            egl::NONE,
        ];
        let surface =
            match self
                .instance
                .create_pbuffer_surface(self.display, self.config, &attributes)
            {
                Ok(surface) => surface,
                Err(error) => {
                    return Err(self.node_error(format!(
                        "surfaceless 与 pbuffer 都失败（eglCreatePbufferSurface: {error}）"
                    )));
                }
            };
        self.surface = Some(surface);
        self.instance
            .make_current(
                self.display,
                Some(surface),
                Some(surface),
                Some(self.context),
            )
            .map_err(|error| self.node_error(format!("eglMakeCurrent(pbuffer) 失败: {error}")))
    }

    fn build_program(&mut self) -> Result<(), VaapiError> {
        let vertex = self.compile_shader(GL_VERTEX_SHADER, VERTEX_SHADER, "顶点着色器")?;
        let fragment = match self.compile_shader(GL_FRAGMENT_SHADER, FRAGMENT_SHADER, "片元着色器")
        {
            Ok(shader) => shader,
            Err(error) => {
                (self.gl.delete_shader)(vertex);
                return Err(error);
            }
        };
        let program = (self.gl.create_program)();
        (self.gl.attach_shader)(program, vertex);
        (self.gl.attach_shader)(program, fragment);
        (self.gl.bind_attrib_location)(program, 0, c"a_position".as_ptr());
        (self.gl.bind_attrib_location)(program, 1, c"a_texcoord".as_ptr());
        (self.gl.link_program)(program);
        let mut linked = 0;
        (self.gl.get_programiv)(program, GL_LINK_STATUS, &mut linked);
        if linked == 0 {
            let log = self
                .program_log(program)
                .unwrap_or_else(|| "（无日志）".to_owned());
            (self.gl.delete_program)(program);
            (self.gl.delete_shader)(vertex);
            (self.gl.delete_shader)(fragment);
            return Err(self.convert_error(format!("着色器链接失败: {log}")));
        }
        (self.gl.delete_shader)(vertex);
        (self.gl.delete_shader)(fragment);
        self.program = program;
        self.matrix_location = self.uniform_location("u_matrix")?;
        self.offset_location = self.uniform_location("u_offset")?;
        self.uv_swap_location = self.uniform_location("u_uv_swap")?;
        self.brightness_location = self.uniform_location("u_brightness")?;
        self.y_sampler_location = self.uniform_location("u_tex_y")?;
        self.uv_sampler_location = self.uniform_location("u_tex_uv")?;
        self.tex_scale_location = self.uniform_location("u_tex_scale")?;
        if let Some(error) = self.gl.take_error("编译着色器") {
            return Err(self.convert_error(error));
        }
        Ok(())
    }

    fn compile_shader(
        &self,
        kind: u32,
        source: &'static str,
        label: &str,
    ) -> Result<u32, VaapiError> {
        let shader = (self.gl.create_shader)(kind);
        let pointer = source.as_ptr().cast::<c_char>();
        let length = i32::try_from(source.len())
            .map_err(|_| self.convert_error(format!("{label}源码长度超出 i32")))?;
        (self.gl.shader_source)(shader, 1, &pointer, &length);
        (self.gl.compile_shader)(shader);
        let mut compiled = 0;
        (self.gl.get_shaderiv)(shader, GL_COMPILE_STATUS, &mut compiled);
        if compiled == 0 {
            let log = self
                .shader_log(shader)
                .unwrap_or_else(|| "（无日志）".to_owned());
            (self.gl.delete_shader)(shader);
            return Err(self.convert_error(format!("{label}编译失败: {log}")));
        }
        Ok(shader)
    }

    fn shader_log(&self, shader: u32) -> Option<String> {
        let mut length = 0;
        (self.gl.get_shaderiv)(shader, GL_INFO_LOG_LENGTH, &mut length);
        if length <= 0 {
            return None;
        }
        let mut buffer = vec![0_u8; usize::try_from(length).ok()?];
        let mut written = 0;
        (self.gl.get_shader_info_log)(
            shader,
            length,
            &mut written,
            buffer.as_mut_ptr().cast::<c_char>(),
        );
        text_from_buffer(&buffer)
    }

    fn program_log(&self, program: u32) -> Option<String> {
        let mut length = 0;
        (self.gl.get_programiv)(program, GL_INFO_LOG_LENGTH, &mut length);
        if length <= 0 {
            return None;
        }
        let mut buffer = vec![0_u8; usize::try_from(length).ok()?];
        let mut written = 0;
        (self.gl.get_program_info_log)(
            program,
            length,
            &mut written,
            buffer.as_mut_ptr().cast::<c_char>(),
        );
        text_from_buffer(&buffer)
    }

    fn uniform_location(&self, name: &str) -> Result<i32, VaapiError> {
        let c_name = CString::new(name).map_err(|error| {
            self.convert_error(format!("uniform 名字含 NUL（{name}）: {error}"))
        })?;
        let location = (self.gl.get_uniform_location)(self.program, c_name.as_ptr());
        if location < 0 {
            return Err(self.convert_error(format!("着色器里没有 uniform {name}")));
        }
        Ok(location)
    }

    /// 把一个平面的 dma-buf 导入成 `EGLImage`。
    ///
    /// 带 modifier 导入失败时退回"隐式修饰符"再试一次：某些驱动（比如 Intel iHD
    /// 的单对象导出）只在没有 modifier 属性时接受。
    fn create_image(&self, plane: &PlaneDesc) -> Result<EglImage, VaapiError> {
        match self.create_image_once(plane) {
            Ok(image) => Ok(image),
            Err(first) => {
                let Some(modifier) = plane.modifier else {
                    return Err(first);
                };
                let mut relaxed = *plane;
                relaxed.modifier = None;
                match self.create_image_once(&relaxed) {
                    Ok(image) => {
                        tracing::debug!(
                            plane = plane.label,
                            modifier = format!("0x{modifier:016X}"),
                            error = %first,
                            "带 modifier 的 EGLImage 导入失败，已按隐式修饰符重试"
                        );
                        Ok(image)
                    }
                    Err(second) => Err(self.convert_error(format!(
                        "{} 的 EGLImage 导入失败（带 modifier: {first}；不带 modifier: {second}）",
                        plane.label
                    ))),
                }
            }
        }
    }

    fn create_image_once(&self, plane: &PlaneDesc) -> Result<EglImage, VaapiError> {
        let mut attributes = Vec::with_capacity(16);
        push_plane_attributes(&mut attributes, plane);
        // 属性表以 EGL_NONE 结尾（`push_plane_attributes` 保证），
        // fd 在调用期间有效（借用自输入帧）。
        let image = self.raw.create_image.call(
            self.display.as_ptr(),
            ptr::null_mut(),
            EGL_LINUX_DMA_BUF_EXT,
            ptr::null_mut(),
            &attributes,
        );
        if image.is_null() {
            let code = self.raw.take_error_code();
            if code == EGL_CONTEXT_LOST_CODE {
                crate::decoder::forget_device(&self.node);
            }
            return Err(self.convert_error(format!(
                "{}（fourcc=0x{:08X}, {}x{}, modifier={:?}）的 eglCreateImage 返回空: {}（0x{code:04X}）",
                plane.label,
                plane.fourcc,
                plane.width,
                plane.height,
                plane.modifier,
                egl_error_name(code),
            )));
        }
        Ok(EglImage {
            display: self.display.as_ptr(),
            image,
            destroy: self.raw.destroy_image,
        })
    }

    /// 把 `EGLImage` 绑到纹理单元 `unit` 上，返回纹理名。
    fn bind_plane_texture(
        &self,
        unit: u32,
        image: &EglImage,
        label: &str,
    ) -> Result<u32, VaapiError> {
        let mut texture = 0;
        (self.gl.gen_textures)(1, &mut texture);
        (self.gl.active_texture)(GL_TEXTURE0 + unit);
        (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
        (self.gl.egl_image_target_texture_2d)(GL_TEXTURE_2D, image.image);
        if let Some(error) = self.gl.take_error("glEGLImageTargetTexture2DOES") {
            (self.gl.delete_textures)(1, &texture);
            return Err(self.convert_error(format!("{label}: {error}")));
        }
        // MIN_FILTER 必须显式设，否则采样出全黑（GLES2 默认是 mipmap 过滤）。
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        if let Some(error) = self.gl.take_error("glTexParameteri") {
            (self.gl.delete_textures)(1, &texture);
            return Err(self.convert_error(format!("{label}: {error}")));
        }
        Ok(texture)
    }

    fn delete_textures(&self, textures: &[u32]) {
        if textures.is_empty() {
            return;
        }
        let count = i32::try_from(textures.len()).unwrap_or(i32::MAX);
        (self.gl.delete_textures)(count, textures.as_ptr());
    }

    fn delete_framebuffer(&self, framebuffer: u32) {
        if framebuffer != 0 {
            (self.gl.delete_framebuffers)(1, &framebuffer);
        }
    }

    /// 为一个输出 BO 建纹理 + FBO，并检查完整性。
    fn attach_output(&self, image: &EglImage, label: &str) -> Result<(u32, u32), VaapiError> {
        let texture = self.bind_output_texture(image, label)?;
        let mut framebuffer = 0;
        (self.gl.gen_framebuffers)(1, &mut framebuffer);
        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
        (self.gl.framebuffer_texture_2d)(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            texture,
            0,
        );
        let status = (self.gl.check_framebuffer_status)(GL_FRAMEBUFFER);
        if status != GL_FRAMEBUFFER_COMPLETE {
            self.delete_framebuffer(framebuffer);
            self.delete_textures(&[texture]);
            return Err(self.convert_error(format!(
                "{label}: FBO 不完整（glCheckFramebufferStatus=0x{status:04X}）"
            )));
        }
        (self.gl.viewport)(0, 0, self.width as i32, self.height as i32);
        (self.gl.clear_color)(0.0, 0.0, 0.0, 1.0);
        (self.gl.clear)(GL_COLOR_BUFFER_BIT);
        Ok((texture, framebuffer))
    }

    fn bind_output_texture(&self, image: &EglImage, label: &str) -> Result<u32, VaapiError> {
        let mut texture = 0;
        (self.gl.gen_textures)(1, &mut texture);
        (self.gl.active_texture)(GL_TEXTURE0);
        (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
        (self.gl.egl_image_target_texture_2d)(GL_TEXTURE_2D, image.image);
        if let Some(error) = self.gl.take_error("glEGLImageTargetTexture2DOES(输出)") {
            (self.gl.delete_textures)(1, &texture);
            return Err(self.convert_error(format!("{label}: {error}")));
        }
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        if let Some(error) = self.gl.take_error("glTexParameteri(输出)") {
            (self.gl.delete_textures)(1, &texture);
            return Err(self.convert_error(format!("{label}: {error}")));
        }
        Ok(texture)
    }

    /// 把两个输入平面画到 `framebuffer` 上。
    ///
    /// `texcoords` 是四边形的采样坐标：整幅画面是 0..1，按显示尺寸渲染时是
    /// cover 裁剪后的子矩形（见 [`cover_crop`]）。
    fn draw(&self, args: DrawArgs<'_>) -> Result<(), VaapiError> {
        (self.gl.use_program)(self.program);
        (self.gl.uniform_matrix_3fv)(
            self.matrix_location,
            1,
            GL_FALSE,
            args.matrix.coefficients().as_ptr(),
        );
        let offset = args.matrix.offset();
        (self.gl.uniform_3f)(self.offset_location, offset[0], offset[1], offset[2]);
        (self.gl.uniform_1i)(self.uv_swap_location, i32::from(args.uv_swap));
        (self.gl.uniform_1f)(self.brightness_location, args.brightness);
        (self.gl.uniform_1i)(self.y_sampler_location, 0);
        (self.gl.uniform_1i)(self.uv_sampler_location, 1);
        (self.gl.uniform_2f)(
            self.tex_scale_location,
            self.tex_scale[0],
            self.tex_scale[1],
        );

        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, args.framebuffer);
        (self.gl.viewport)(0, 0, self.width as i32, self.height as i32);
        (self.gl.clear_color)(0.0, 0.0, 0.0, 1.0);
        (self.gl.clear)(GL_COLOR_BUFFER_BIT);

        (self.gl.active_texture)(GL_TEXTURE0);
        (self.gl.bind_texture)(GL_TEXTURE_2D, args.y_texture);
        (self.gl.active_texture)(GL_TEXTURE0 + 1);
        (self.gl.bind_texture)(GL_TEXTURE_2D, args.uv_texture);

        (self.gl.enable_vertex_attrib_array)(0);
        (self.gl.vertex_attrib_pointer)(
            0,
            2,
            GL_FLOAT,
            0,
            0,
            args.positions.as_ptr().cast::<c_void>(),
        );
        (self.gl.enable_vertex_attrib_array)(1);
        (self.gl.vertex_attrib_pointer)(
            1,
            2,
            GL_FLOAT,
            0,
            0,
            args.texcoords.as_ptr().cast::<c_void>(),
        );
        (self.gl.draw_arrays)(GL_TRIANGLE_STRIP, 0, 4);
        if let Some(error) = self.gl.take_error("绘制") {
            return Err(self.convert_error(error));
        }
        Ok(())
    }

    /// 改渲染目标尺寸（视口与回读都跟着它）。
    ///
    /// 只在输出池重建时调用；EGL surface/context 不受影响（渲染始终进 FBO）。
    /// 设置输入纹理的采样范围（`可见/分配`，默认 1.0）。
    fn set_tex_scale(&mut self, x: f32, y: f32) {
        self.tex_scale = [x, y];
    }

    fn set_target_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    /// 等 GPU 真的画完（`glFinish`）。
    ///
    /// **不可省略**：输出 BO 没有 dma-buf fence，合成器采样的那一刻必须保证渲染
    /// 已经完成，否则会花屏/撕裂（见模块文档的"同步"）。计时把它和 `draw` 分开，
    /// 是因为两者优化手段不同（前者是绘制调用，后者是等 GPU 排空）。
    fn finish(&self) -> Result<(), VaapiError> {
        (self.gl.finish)();
        if let Some(error) = self.gl.take_error("glFinish") {
            if self.raw.take_error_code() == EGL_CONTEXT_LOST_CODE {
                crate::decoder::forget_device(&self.node);
            }
            return Err(self.convert_error(error));
        }
        Ok(())
    }

    /// 从输出 FBO 回读一帧，返回**自上而下**的 RGBA 行（诊断探针用）。
    ///
    /// 块状 modifier 的输出 BO 无法按行 `mmap`，回读只能走 GPU。正常播放路径
    /// 不调用这里（`readback_last_xrgb` 是给 `examples/rgb_convert_probe.rs` 的）。
    ///
    /// 注意**不要**再翻一次行：顶点着色器对 y 取了负（`gl_Position.y = -y`），
    /// 图像的行序在 FBO 里已经和 GL 的 bottom-up 行序相反，`glReadPixels` 从
    /// 底行读起刚好得到自上而下的图像。
    fn read_framebuffer(&self, framebuffer: u32) -> Result<Vec<u8>, VaapiError> {
        let row_bytes = self.width as usize * 4;
        let mut raw = vec![0u8; row_bytes * self.height as usize];
        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
        (self.gl.pixel_storei)(GL_PACK_ALIGNMENT, 4);
        (self.gl.read_pixels)(
            0,
            0,
            self.width as i32,
            self.height as i32,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            raw.as_mut_ptr().cast(),
        );
        if let Some(error) = self.gl.take_error("glReadPixels(输出 FBO)") {
            return Err(self.convert_error(error));
        }
        Ok(raw)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // 尽量把 GL 对象删干净；上下文/display 的销毁即使失败也只能记日志。
        if self.program != 0 && self.acquire().is_ok() {
            (self.gl.delete_program)(self.program);
            self.program = 0;
            self.release();
        }
        if let Some(surface) = self.surface.take() {
            let _ = self.instance.destroy_surface(self.display, surface);
        }
        let _ = self.instance.destroy_context(self.display, self.context);
        let _ = self.instance.terminate(self.display);
    }
}

fn choose_config(instance: &EglInstance, display: egl::Display) -> Result<egl::Config, VaapiError> {
    let pbuffer = [
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES2_BIT,
        egl::SURFACE_TYPE,
        egl::PBUFFER_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::NONE,
    ];
    // 拿不到带 pbuffer 的配置时不要直接失败：NVIDIA 恰好支持 surfaceless，
    // 保留 RENDERABLE_TYPE=OPENGL_ES2_BIT + SURFACE_TYPE=0 以兼容严格驱动。
    let surfaceless = [
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES2_BIT,
        egl::SURFACE_TYPE,
        0,
        egl::NONE,
    ];
    let candidates: [(&str, &[i32]); 2] = [("pbuffer", &pbuffer), ("surfaceless", &surfaceless)];
    for (label, attributes) in candidates {
        match instance.choose_first_config(display, attributes) {
            Ok(Some(config)) => {
                tracing::debug!(label, "eglChooseConfig 成功");
                return Ok(config);
            }
            Ok(None) => tracing::debug!(label, "eglChooseConfig 没有匹配配置"),
            Err(error) => tracing::debug!(label, %error, "eglChooseConfig 失败"),
        }
    }
    Err(VaapiError::Unsupported(
        "没有可用的 EGLConfig（需要 GLES2 + pbuffer 或 surfaceless）".to_owned(),
    ))
}

fn text_from_buffer(buffer: &[u8]) -> Option<String> {
    let c_str = CStr::from_bytes_until_nul(buffer).ok()?;
    Some(c_str.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// gbm 输出缓冲
// ---------------------------------------------------------------------------

/// 取 `gbm_device` 的裸指针，交给 `eglGetPlatformDisplay`。
fn gbm_device_ptr<T: AsFd>(device: &gbm::Device<T>) -> *mut c_void {
    fn raw_as_usize<D: AsRaw<U>, U>(object: &D) -> usize {
        object.as_raw() as usize
    }
    raw_as_usize(device) as *mut c_void
}

/// 分配一个可以当渲染目标的 `XRGB8888` BO。
///
/// 真机事实（NVIDIA 620.70 / Mesa 26.2.7 gbm nvidia 后端）：
/// `RENDERING|LINEAR` 直接 `EINVAL`，`RENDERING` 拿到 block-linear
/// `0x0300000000E08014`。所以按下面的顺序试，成功即用，**不**强求 LINEAR。
/// 仅接受单平面 GBM BO：辅助压缩面（如 Intel CCS，plane_count > 1）在后续
/// 导出单 fd 时会被截断导致花屏或拒绝，拒绝后继续尝试非压缩或 LINEAR 回退。
fn accept_single_plane_bo(
    label: &str,
    buffer: gbm::BufferObject<()>,
) -> Option<gbm::BufferObject<()>> {
    let planes = buffer.plane_count();
    if planes > 1 {
        tracing::debug!(
            flags = label,
            planes,
            modifier = format!("0x{:016X}", u64::from(buffer.modifier())),
            "跳过多平面 GBM BO（如 CCS 压缩面），尝试下一种 modifier/策略"
        );
        return None;
    }
    Some(buffer)
}

fn allocate_output(
    device: &gbm::Device<File>,
    node: &Path,
    width: u32,
    height: u32,
    preferred_modifiers: &[u64],
) -> Result<gbm::BufferObject<()>, VaapiError> {
    use gbm::{BufferObjectFlags, Format};

    let explicit_mods: Vec<gbm::Modifier> = preferred_modifiers
        .iter()
        .copied()
        .filter(|m| *m != DRM_FORMAT_MOD_INVALID)
        .map(gbm::Modifier::from)
        .collect();
    if !explicit_mods.is_empty() {
        for (label, flags) in [
            ("preferred(RENDERING)", BufferObjectFlags::RENDERING),
            (
                "preferred(RENDERING|WRITE)",
                BufferObjectFlags::RENDERING | BufferObjectFlags::WRITE,
            ),
        ] {
            match device.create_buffer_object_with_modifiers2::<()>(
                width,
                height,
                Format::Xrgb8888,
                explicit_mods.iter().copied(),
                flags,
            ) {
                Ok(buffer) => {
                    if let Some(accepted) = accept_single_plane_bo(label, buffer) {
                        tracing::debug!(
                            flags = label,
                            modifier = format!("0x{:016X}", u64::from(accepted.modifier())),
                            "gbm 输出缓冲已按合成器首选 modifier 分配"
                        );
                        return Ok(accepted);
                    }
                }
                Err(error) => {
                    tracing::debug!(flags = label, %error, "按首选 modifier 分配失败");
                }
            }
        }
        match device.create_buffer_object_with_modifiers::<()>(
            width,
            height,
            Format::Xrgb8888,
            explicit_mods.into_iter(),
        ) {
            Ok(buffer) => {
                if let Some(accepted) = accept_single_plane_bo("preferred(no-flags)", buffer) {
                    tracing::debug!(
                        modifier = format!("0x{:016X}", u64::from(accepted.modifier())),
                        "gbm 输出缓冲已按合成器首选 modifier 分配（无 flags）"
                    );
                    return Ok(accepted);
                }
            }
            Err(error) => {
                tracing::debug!(%error, "按首选 modifier 分配失败（无 flags），回退默认梯队");
            }
        }
    }

    let ladder: [(&str, BufferObjectFlags); 6] = [
        (
            "RENDERING|LINEAR",
            BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
        ),
        ("RENDERING", BufferObjectFlags::RENDERING),
        (
            "RENDERING|WRITE",
            BufferObjectFlags::RENDERING | BufferObjectFlags::WRITE,
        ),
        (
            "SCANOUT|RENDERING|WRITE",
            BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING | BufferObjectFlags::WRITE,
        ),
        (
            "WRITE|LINEAR",
            BufferObjectFlags::WRITE | BufferObjectFlags::LINEAR,
        ),
        ("0", BufferObjectFlags::empty()),
    ];
    for (label, flags) in ladder {
        match device.create_buffer_object::<()>(width, height, Format::Xrgb8888, flags) {
            Ok(buffer) => {
                if let Some(accepted) = accept_single_plane_bo(label, buffer) {
                    tracing::debug!(flags = label, "gbm 输出缓冲已分配");
                    return Ok(accepted);
                }
            }
            Err(error) => {
                tracing::debug!(flags = label, %error, "gbm 分配失败，试下一组 flags");
            }
        }
    }
    // 显式要求 LINEAR 的兜底（有些后端只在这条路径上给 LINEAR）。
    if let Ok(buffer) = device.create_buffer_object_with_modifiers2::<()>(
        width,
        height,
        Format::Xrgb8888,
        [gbm::Modifier::Linear].into_iter(),
        BufferObjectFlags::RENDERING | BufferObjectFlags::WRITE,
    ) {
        if let Some(accepted) = accept_single_plane_bo("explicit(LINEAR)", buffer) {
            tracing::debug!("gbm 输出缓冲已按显式 LINEAR 分配");
            return Ok(accepted);
        }
    }
    Err(init_error(
        node,
        format!("gbm 无法分配单平面 {width}x{height} XRGB8888 渲染缓冲"),
    ))
}

// ---------------------------------------------------------------------------
// 输出槽位
// ---------------------------------------------------------------------------

/// gbm BO 的拥有者。`Arc<SlotBuffer>` 被帧的 keepalive 持有：
/// 合成器用完最后一帧之前，BO（以及它绑定着的 gbm_device）不会被释放。
struct SlotBuffer {
    _bo: gbm::BufferObject<()>,
    /// 必须声明在 `_bo` 之后：即使 `RgbConverter` 已被销毁，合成器仍持有的旧帧
    /// 在释放时也需要 `gbm_device` 仍存活，否则 `gbm_bo_destroy` 会 UAF。
    _device: Arc<gbm::Device<File>>,
}

/// 交给使命中的 `FrameKeepalive`：drop 时把槽位标记为空闲。
struct SlotGuard {
    _buffer: Arc<SlotBuffer>,
    inflight: Arc<AtomicUsize>,
    /// 全部槽位的合计计数（跨线程可读，见 [`RgbConverter::inflight_handle`]）。
    total: Arc<AtomicUsize>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::Release);
        self.total.fetch_sub(1, Ordering::Release);
    }
}

/// 一个输出槽位：BO + 它的 EGLImage + 纹理 + FBO。
struct Slot {
    /// 先声明，保证 drop 时 EGL display 还活着。
    _image: EglImage,
    texture: u32,
    framebuffer: u32,
    fd: Arc<OwnedFd>,
    buffer: Arc<SlotBuffer>,
    stride: u32,
    offset: u32,
    modifier: u64,
    inflight: Arc<AtomicUsize>,
}

impl Slot {
    fn is_busy(&self) -> bool {
        self.inflight.load(Ordering::Acquire) != 0
    }
}

// ---------------------------------------------------------------------------
// 分段耗时统计
// ---------------------------------------------------------------------------

/// 一帧里各渲染阶段的耗时。
#[derive(Debug, Clone, Copy)]
struct StageTimings {
    image: Duration,
    bind: Duration,
    draw: Duration,
    finish: Duration,
}

/// 一帧的完整分段耗时（`total` 从进入 `convert()` 起算）。
#[derive(Debug, Clone, Copy)]
struct FrameTimings {
    total: Duration,
    stage: StageTimings,
}

/// 按 2 秒窗口聚合的分段耗时统计。
///
/// `LIUGUANG_RGB_STATS=1` 时每 2 秒打一行 INFO（真机取时间序列证据的来源）；
/// 未启用时只累计数值、不落日志。用 `std::time::Instant`，不引额外依赖。
#[derive(Debug)]
struct ConvertStats {
    enabled: bool,
    window: Instant,
    frames: u64,
    busy: u64,
    reuse: u64,
    total_us: u64,
    max_total_us: u64,
    image_us: u64,
    bind_us: u64,
    draw_us: u64,
    finish_us: u64,
}

impl ConvertStats {
    fn new() -> Self {
        Self {
            enabled: matches!(
                std::env::var("LIUGUANG_RGB_STATS").as_deref(),
                Ok("1" | "true" | "yes" | "on")
            ),
            window: Instant::now(),
            frames: 0,
            busy: 0,
            reuse: 0,
            total_us: 0,
            max_total_us: 0,
            image_us: 0,
            bind_us: 0,
            draw_us: 0,
            finish_us: 0,
        }
    }

    /// 累计一帧成功转换的分段耗时。
    fn record(&mut self, timings: FrameTimings) {
        let total = micros(timings.total);
        self.frames = self.frames.saturating_add(1);
        self.total_us = self.total_us.saturating_add(total);
        self.max_total_us = self.max_total_us.max(total);
        self.image_us = self.image_us.saturating_add(micros(timings.stage.image));
        self.bind_us = self.bind_us.saturating_add(micros(timings.stage.bind));
        self.draw_us = self.draw_us.saturating_add(micros(timings.stage.draw));
        self.finish_us = self.finish_us.saturating_add(micros(timings.stage.finish));
        self.maybe_report();
    }

    /// 槽位全忙（返回 `Busy`）的次数：这段时间没有渲染耗时，单独记账。
    fn record_busy(&mut self) {
        self.busy = self.busy.saturating_add(1);
        self.maybe_report();
    }

    /// EGLImage 缓存命中次数（优化 B 的收益证据）。
    fn record_reuse(&mut self, hits: u64) {
        self.reuse = self.reuse.saturating_add(hits);
    }

    /// 到点就打一行并清零窗口。
    fn maybe_report(&mut self) {
        if !self.enabled {
            return;
        }
        let elapsed = self.window.elapsed();
        if elapsed < Duration::from_secs(2) {
            return;
        }
        let divisor = self.frames.max(1);
        tracing::info!(
            "RGB 转换统计 frames={} avg_total_us={} max_total_us={} avg_image_us={} \
             avg_draw_us={} avg_finish_us={} busy={} reuse={} avg_bind_us={}",
            self.frames,
            self.total_us / divisor,
            self.max_total_us,
            self.image_us / divisor,
            self.draw_us / divisor,
            self.finish_us / divisor,
            self.busy,
            self.reuse,
            self.bind_us / divisor,
        );
        self.window = Instant::now();
        self.frames = 0;
        self.busy = 0;
        self.reuse = 0;
        self.total_us = 0;
        self.max_total_us = 0;
        self.image_us = 0;
        self.bind_us = 0;
        self.draw_us = 0;
        self.finish_us = 0;
    }
}

/// `Duration` → 微秒（饱和到 `u64`，避免统计代码 panic）。
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// RgbConverter
// ---------------------------------------------------------------------------

/// EGLImage 缓存的键：一块解码输出 BO 的身份 + 导入参数。
///
/// 解码器的输出 BO 来自固定表面池，每帧只是换内容；EGLImage 绑定的是**内存**
/// 而不是内容，所以同一块 BO 的 EGLImage 可以跨帧复用。BO 身份用
/// `fstat(fd)` 的 `(st_dev, st_ino)`：`vaExportSurfaceHandle` 每帧 dup 一个新 fd，
/// 但指向同一块 dma-buf，inode 不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageKey {
    dev: u64,
    ino: u64,
    offset: u32,
    pitch: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: Option<u64>,
    uv_swap: bool,
}

impl ImageKey {
    fn of(plane: &PlaneDesc, uv_swap: bool) -> Result<Self, VaapiError> {
        // SAFETY: `plane.fd` 是调用方持有、本次调用期间仍存活的 dma-buf fd。
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(plane.fd, &mut stat) } != 0 {
            return Err(VaapiError::Unsupported(format!(
                "fstat({}) 失败: {}",
                plane.label,
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self {
            dev: stat.st_dev,
            ino: stat.st_ino,
            offset: plane.offset,
            pitch: plane.pitch,
            width: plane.width,
            height: plane.height,
            fourcc: plane.fourcc,
            modifier: plane.modifier,
            uv_swap,
        })
    }
}

/// 输入平面 EGLImage 的 LRU 缓存（按 BO 复用，省掉每帧的 `eglCreateImageKHR`）。
///
/// 上界 10 条：解码表面池在 5 个输出槽位在途时最多同时出现几块 BO，10 足以覆盖
/// 工作集；收紧上限可减少驱动层长期持有的 EGLImage 引用。
struct ImageCache {
    entries: std::collections::VecDeque<(ImageKey, Rc<EglImage>)>,
}

impl ImageCache {
    const CAPACITY: usize = 10;

    fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::new(),
        }
    }

    /// 命中就移到队首（LRU）并返回共享引用。
    fn lookup(&mut self, key: &ImageKey) -> Option<Rc<EglImage>> {
        let position = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(position)?;
        let image = Rc::clone(&entry.1);
        self.entries.push_front(entry);
        Some(image)
    }

    fn insert(&mut self, key: ImageKey, image: Rc<EglImage>) {
        while self.entries.len() >= Self::CAPACITY {
            self.entries.pop_back();
        }
        self.entries.push_front((key, image));
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// 解码表面带 16 对齐填充时，纹理坐标要收窄到可见区域。
///
/// 返回 `(可见宽/分配宽, 可见高/分配高)`：`(1920x1088, 1920x1080)` → `(1.0, 0.9926…)`。
/// 尺寸非法（0）或可见大于分配时按 1.0 处理（宁可露出填充，也不要采样越界）。
fn visible_tex_scale(source: (u32, u32), visible: (u32, u32)) -> (f32, f32) {
    let scale = |source: u32, visible: u32| {
        if source == 0 || visible == 0 || visible > source {
            1.0
        } else {
            visible as f32 / source as f32
        }
    };
    (scale(source.0, visible.0), scale(source.1, visible.1))
}

/// NV12 → XRGB8888 的转换器：构造一次，反复调用。
///
/// 每个实例独占一个 `gbm_device` + EGL display + GLES2 上下文，并预分配
/// [`RgbConverter::SLOTS`] 个输出 BO 轮转使用。构造完成后不再需要调用者
/// 关心 EGL 细节，只把 [`DmaBufFrame`] 喂进来。
///
/// # 输出尺寸
///
/// 默认按**源尺寸**出帧；[`RgbConverter::set_output_size`] 可以让后续帧按面板
/// 物理像素出帧（4K 源 + 2560×1600 面板时少一次合成器全屏缩放）。改尺寸是
/// 懒重建：下一帧 `convert` 之前重建输出池，失败则继续用旧尺寸。
pub struct RgbConverter {
    /// 输入平面 EGLImage 的 BO 缓存。
    ///
    /// **必须声明在 `renderer` 之前**（Rust 按声明顺序 drop）：`EglImage::drop` 会
    /// 拿存下来的 EGL display 调 `eglDestroyImageKHR`，而 `Renderer::drop` 的最后一步
    /// 是 `eglTerminate`。声明在 `renderer` 后面的话，缓存里的 image 会在 terminate
    /// **之后**才销毁 —— 驱动在已销毁的 display 上释放对象，真机表现就是 glibc
    /// 报 `corrupted double-linked list` 并 abort（t41 复核抓到的 `vaapi-prefetch`
    /// 崩溃就是这条路径；t43 修）。
    images: ImageCache,
    slots: Vec<Slot>,
    next_slot: usize,
    renderer: Renderer,
    uv_dialect: UvDialect,
    is_nvidia_gbm: bool,
    preferred_modifiers: Vec<u64>,
    /// 输入 NV12 的尺寸（解码器给的**分配**尺寸，按 16 对齐）。
    source_width: u32,
    source_height: u32,
    /// 输入纹理里**真正有效**的像素区域（码流显示尺寸）。
    ///
    /// 解码表面按高度 16 对齐分配（1080 → 1088、1530 → 1536），多出来的行是
    /// 填充。采样时必须只取可见区域，否则画面底部会露出一条垃圾、并且被纵向
    /// 压扁。默认等于 `source`（无填充时是恒等变换）。
    visible_width: u32,
    visible_height: u32,
    /// 当前输出（XRGB8888）的尺寸。
    output_width: u32,
    output_height: u32,
    /// 实时画面变换与亮度（每次 `paint` 直接进 GL 着色器，无需重建输出池）。
    transform: RenderTransform,
    /// 下一帧之前要切到的输出尺寸（`None` = 不切）。
    pending_output: Option<(u32, u32)>,
    /// 渲染节点（重建输出池时打日志/报错用）。
    node: PathBuf,
    converted: u64,
    /// 最近一次成功绘制的槽位（诊断回读用）。
    last_painted: Option<usize>,
    /// 全部槽位的合计占用数。
    ///
    /// 单独维护一份（而不是每次遍历 `slots` 求和）是为了**跨线程观察**：
    /// 解码预读线程独占转换器时，生产者只能拿到这个 `Arc` 来报
    /// `keepalive_len`（见 [`RgbConverter::inflight_handle`]）。
    inflight_total: Arc<AtomicUsize>,
    /// 分段耗时统计（`LIUGUANG_RGB_STATS=1` 时每 2 秒打一行）。
    stats: ConvertStats,
    /// 输出 BO 的 gbm 设备。必须最后 drop：EGL display 与它同生共死。
    _device: Arc<gbm::Device<File>>,
}

impl RgbConverter {
    /// 输出槽位数。
    ///
    /// **必须大于 4**：真机实测合成器（denial）会同时攥住最多 4 个缓冲（当前显示 +
    /// 排队 + 一拍 + 一帧在途），槽位不多于它的持有深度时会出现"缓冲池深度死锁"
    /// ——所有槽位都在合成器手里，我们不再 commit，合成器也就不再 release，
    /// 双方互等，10s 后上层看门狗把壁纸降级成静态图（3 槽实测必死）。
    ///
    /// 取 **5**：比"最多持有 4"多留一个周转位（实测 `keepalive_len` 峰值 4），
    /// 同时比 6 省一个输出 BO —— 2560×1600 的 XRGB8888 每个 16.4 MB，
    /// 少一个就是少 16 MB 常驻。可用 `LIUGUANG_RGB_SLOTS`（4..=8）覆盖。
    pub const SLOTS: usize = 5;

    /// 实际槽位数：默认 [`Self::SLOTS`]，可用 `LIUGUANG_RGB_SLOTS` 覆盖。
    fn slot_count() -> usize {
        match std::env::var("LIUGUANG_RGB_SLOTS")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
        {
            Some(value) if (4..=8).contains(&value) => value,
            _ => Self::SLOTS,
        }
    }

    /// 在 `node`（渲染节点，如 `/dev/dri/renderD129`）上建好转换器。
    ///
    /// `width`/`height` 是**输入 NV12** 的尺寸，必须与后续输入帧一致，且为偶数
    /// （NV12 的色度平面是半分辨率）。初始输出尺寸与源尺寸相同。
    pub fn new(node: &Path, width: u32, height: u32) -> Result<Self, VaapiError> {
        Self::new_with_output(node, width, height, width, height)
    }

    /// 在 `node` 上建立转换器并直接按 `output_width`×`output_height` 分配输出槽位。
    ///
    /// 已知显示器物理尺寸时，从首帧起就按目标分辨率建 GBM 输出池，避免先按 4K
    /// 源尺寸分配 5 个大 BO 后又立即重建的瞬时显存与延迟尖峰。
    pub fn new_with_output(
        node: &Path,
        width: u32,
        height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Result<Self, VaapiError> {
        Self::new_with_output_and_modifiers(node, width, height, output_width, output_height, &[])
    }

    /// 在 `node` 上建立转换器，按 `output_width`×`output_height` 与合成器首选 `preferred_modifiers` 分配输出槽位。
    pub fn new_with_output_and_modifiers(
        node: &Path,
        width: u32,
        height: u32,
        output_width: u32,
        output_height: u32,
        preferred_modifiers: &[u64],
    ) -> Result<Self, VaapiError> {
        if width == 0 || height == 0 {
            return Err(VaapiError::Unsupported(format!(
                "RGB 转换器尺寸非法: {width}x{height}"
            )));
        }
        if width % 2 != 0 || height % 2 != 0 {
            return Err(VaapiError::Unsupported(format!(
                "NV12 的宽高必须是偶数，实际 {width}x{height}"
            )));
        }
        let (out_w, out_h) = if output_width > 0 && output_height > 0 {
            (output_width, output_height)
        } else {
            (width, height)
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|source| VaapiError::OpenNode {
                path: node.to_path_buf(),
                source,
            })?;
        let device = Arc::new(
            gbm::Device::new(file)
                .map_err(|error| init_error(node, format!("gbm_create_device 失败: {error}")))?,
        );
        let backend_name = device.backend_name().to_string();
        let is_nvidia_gbm = backend_name.to_ascii_lowercase().contains("nvidia");
        let uv_dialect = if is_nvidia_gbm {
            UvDialect::Rg88
        } else {
            UvDialect::PREFERRED
        };
        tracing::debug!(
            node = %node.display(),
            backend = %backend_name,
            is_nvidia_gbm,
            preferred_modifiers = preferred_modifiers.len(),
            "gbm 设备已建立"
        );
        let renderer = Renderer::new(node, gbm_device_ptr(device.as_ref()), out_w, out_h)?;
        renderer.acquire()?;
        let built = Self::build_slots(&renderer, &device, node, out_w, out_h, preferred_modifiers);
        renderer.release();
        let slots = built?;
        if let Some(first) = slots.first() {
            tracing::info!(
                node = %node.display(),
                source = %format!("{width}x{height}"),
                output = %format!("{out_w}x{out_h}"),
                slots = slots.len(),
                stride = first.stride,
                modifier = format!("0x{:016X}", first.modifier),
                "RGB 转换输出缓冲已建立（NV12 → XRGB8888）"
            );
        }
        Ok(Self {
            slots,
            next_slot: 0,
            renderer,
            uv_dialect,
            is_nvidia_gbm,
            preferred_modifiers: preferred_modifiers.to_vec(),
            source_width: width,
            source_height: height,
            visible_width: width,
            visible_height: height,
            output_width: out_w,
            output_height: out_h,
            transform: RenderTransform::default(),
            pending_output: None,
            images: ImageCache::new(),
            node: node.to_path_buf(),
            converted: 0,
            last_painted: None,
            inflight_total: Arc::new(AtomicUsize::new(0)),
            stats: ConvertStats::new(),
            _device: device,
        })
    }

    /// 绑定外部共享的 `inflight` 计数器（用于预读线程懒建转换器前与生产者共享占用数）。
    pub fn set_inflight_counter(&mut self, counter: Arc<AtomicUsize>) {
        self.inflight_total = counter;
    }

    /// 输入帧的**分配**尺寸（`source_width` × `source_height`）。
    pub fn source_size(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    /// 声明输入帧的可见区域（码流的显示尺寸）。
    ///
    /// 解码表面通常按 16 对齐分配：1080p 的 NV12 表面是 1920×1088，1530p 的
    /// 是 2720×1536，多出来的几行是填充。传进来的 `width`/`height` 超过分配
    /// 尺寸时按分配尺寸截断（调用方不必先算 min）。
    pub fn set_visible_size(&mut self, width: u32, height: u32) {
        let width = width.clamp(2, self.source_width.max(2));
        let height = height.clamp(2, self.source_height.max(2));
        self.visible_width = width;
        self.visible_height = height;
        let (scale_x, scale_y) =
            visible_tex_scale((self.source_width, self.source_height), (width, height));
        self.renderer.set_tex_scale(scale_x, scale_y);
        if (width, height) != (self.source_width, self.source_height) {
            tracing::debug!(
                source = format!("{}x{}", self.source_width, self.source_height),
                visible = format!("{width}x{height}"),
                "RGB 转换器按可见区域采样（解码表面有 16 对齐填充）"
            );
        }
    }

    /// 当前声明的可见尺寸。
    pub fn visible_size(&self) -> (u32, u32) {
        (self.visible_width, self.visible_height)
    }

    /// 设置实时画面变换与亮度（fit / zoom / offset / rotation / brightness）。
    ///
    /// 在下一帧 `paint` 的顶点/片段着色器中生效，不销毁解码器或 GBM 输出池。
    pub fn set_render_transform(&mut self, transform: RenderTransform) {
        self.transform = transform;
    }

    /// 当前实时画面变换与亮度。
    #[must_use]
    pub fn render_transform(&self) -> RenderTransform {
        self.transform
    }

    fn destroy_slots(renderer: &Renderer, slots: &[Slot]) {
        for slot in slots {
            renderer.delete_framebuffer(slot.framebuffer);
            renderer.delete_textures(&[slot.texture]);
        }
    }

    fn build_slots(
        renderer: &Renderer,
        device: &Arc<gbm::Device<File>>,
        node: &Path,
        width: u32,
        height: u32,
        preferred_modifiers: &[u64],
    ) -> Result<Vec<Slot>, VaapiError> {
        let count = Self::slot_count();
        let mut slots = Vec::with_capacity(count);
        for index in 0..count {
            let buffer =
                match allocate_output(device.as_ref(), node, width, height, preferred_modifiers) {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        Self::destroy_slots(renderer, &slots);
                        return Err(error);
                    }
                };
            if buffer.plane_count() > 1 {
                Self::destroy_slots(renderer, &slots);
                return Err(init_error(node, "gbm 输出缓冲仍为多平面格式，无法安全导出"));
            }
            let fd = match buffer.fd() {
                Ok(fd) => fd,
                Err(error) => {
                    Self::destroy_slots(renderer, &slots);
                    return Err(init_error(node, format!("取输出 BO 的 fd 失败: {error}")));
                }
            };
            let stride = buffer.stride();
            let offset = buffer.offset(0);
            let modifier = u64::from(buffer.modifier());
            let label = "输出";
            let plane = PlaneDesc {
                label,
                width,
                height,
                fourcc: export::DRM_FORMAT_XRGB8888,
                fd: fd.as_raw_fd(),
                offset,
                pitch: stride,
                modifier: (modifier != DRM_FORMAT_MOD_INVALID).then_some(modifier),
            };
            // LINEAR(0) 在个别驱动上需要不带 modifier 属性的隐式导入兜底；其它明确 modifier 原样传。
            let image = match renderer.create_image_once(&plane) {
                Ok(image) => image,
                Err(first_err) if modifier == 0 => {
                    let implicit = PlaneDesc {
                        modifier: None,
                        ..plane
                    };
                    match renderer.create_image_once(&implicit) {
                        Ok(image) => image,
                        Err(_) => {
                            Self::destroy_slots(renderer, &slots);
                            return Err(first_err);
                        }
                    }
                }
                Err(error) => {
                    Self::destroy_slots(renderer, &slots);
                    return Err(error);
                }
            };
            let (texture, framebuffer) = match renderer.attach_output(&image, label) {
                Ok(target) => target,
                Err(error) => {
                    tracing::debug!(slot = index, %error, "输出槽位建立失败");
                    Self::destroy_slots(renderer, &slots);
                    return Err(error);
                }
            };
            slots.push(Slot {
                _image: image,
                texture,
                framebuffer,
                fd: Arc::new(fd),
                buffer: Arc::new(SlotBuffer {
                    _bo: buffer,
                    _device: Arc::clone(device),
                }),
                stride,
                offset,
                modifier,
                inflight: Arc::new(AtomicUsize::new(0)),
            });
        }
        Ok(slots)
    }

    /// 请求把后续帧渲染成 `width`×`height`（面板物理像素）。
    ///
    /// 重建是**懒**的：真正的输出池替换发生在下一次 [`Self::convert`] 之前；
    /// 重建失败时继续用旧尺寸（宁可多一次缩放，也不能不出画面）。尺寸没变或
    /// 参数非法时直接忽略。
    pub fn set_output_size(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            tracing::warn!(width, height, "忽略非法的 RGB 输出尺寸");
            return;
        }
        if width == self.output_width && height == self.output_height {
            self.pending_output = None;
            return;
        }
        tracing::info!(
            from = %format!("{}x{}", self.output_width, self.output_height),
            to = %format!("{width}x{height}"),
            "RGB 输出尺寸变化：下一帧前重建输出池"
        );
        self.pending_output = Some((width, height));
    }

    /// 有挂起的尺寸变更就重建输出池；失败只记日志、退回旧尺寸。
    fn ensure_output_size(&mut self) {
        let Some((width, height)) = self.pending_output.take() else {
            return;
        };
        if width == self.output_width && height == self.output_height {
            return;
        }
        let started = Instant::now();
        match self.rebuild_output(width, height) {
            Ok(()) => tracing::info!(
                width,
                height,
                slots = self.slots.len(),
                took_us = micros(started.elapsed()),
                "RGB 输出池已按新尺寸重建"
            ),
            Err(error) => {
                // 回退：输出尺寸保持旧值，后续帧照常出（顶层还能用）。
                self.renderer
                    .set_target_size(self.output_width, self.output_height);
                tracing::warn!(
                    %error,
                    width,
                    height,
                    kept = %format!("{}x{}", self.output_width, self.output_height),
                    "RGB 输出池重建失败，继续用旧尺寸"
                );
            }
        }
    }

    fn rebuild_output(&mut self, width: u32, height: u32) -> Result<(), VaapiError> {
        self.renderer.set_target_size(width, height);
        self.renderer.acquire()?;
        let built = Self::build_slots(
            &self.renderer,
            &self._device,
            &self.node,
            width,
            height,
            &self.preferred_modifiers,
        );
        self.renderer.release();
        let slots = built?;
        // 旧槽位只可能在合成器手里还有 keepalive 引用；Arc 会把 BO 活到最后一帧
        // 释放为止，这里先在当前上下文里释放旧的 GL FBO/纹理，再 drop 掉旧 Slot。
        let replaced = std::mem::replace(&mut self.slots, slots);
        if self.renderer.acquire().is_ok() {
            Self::destroy_slots(&self.renderer, &replaced);
            self.renderer.release();
        }
        drop(replaced);
        self.next_slot = 0;
        self.last_painted = None;
        self.output_width = width;
        self.output_height = height;
        Ok(())
    }

    /// 一帧 NV12 → 一帧 XRGB8888。
    ///
    /// 返回的帧引用内部输出 BO，靠 `keepalive` 持有槽位；调用方（合成器）释放
    /// 该帧之前，对应槽位不会被重画。槽位都被占用时返回 [`VaapiError::Busy`]。
    pub fn convert(&mut self, frame: &DmaBufFrame) -> Result<DmaBufFrame, VaapiError> {
        let started = Instant::now();
        if frame.width != self.source_width || frame.height != self.source_height {
            return Err(VaapiError::Unsupported(format!(
                "输入帧 {}x{} 与转换器 {}x{} 不一致",
                frame.width, frame.height, self.source_width, self.source_height
            )));
        }
        // 挂起的输出尺寸变更在这里落地（在取槽位之前：重建会把 next_slot 归零）。
        self.ensure_output_size();
        // 从 next_slot 起找第一个空闲槽，而不是死守轮转位。
        //
        // 死守轮转位时合成器通常握着 4 个槽位，轮到的下一个有约一半概率仍被
        // 占——那意味着这一帧已经付掉了解码 + 导出 dma-buf 的代价才被丢弃
        // （真机实测每 2 秒 40 次 ≈ 出帧尝试的一半）。六个槽位都是等价的输出
        // 缓冲，取任意空闲槽即可；只有全部被占时才让上层退避。
        let slots = self.slots.len();
        let free = (0..slots)
            .map(|offset| (self.next_slot + offset) % slots)
            .find(|index| !self.slots[*index].is_busy());
        let Some(slot_index) = free else {
            self.stats.record_busy();
            return Err(VaapiError::Busy);
        };
        self.next_slot = (slot_index + 1) % slots;
        if let Err(err) = self.renderer.acquire() {
            self.images.clear();
            return Err(err);
        }
        let painted = self.paint(slot_index, frame);
        self.renderer.release();
        let stage = match painted {
            Ok(stage) => stage,
            Err(err) => {
                if err.to_string().contains("EGL_CONTEXT_LOST") {
                    self.images.clear();
                }
                return Err(err);
            }
        };
        self.stats.record(FrameTimings {
            total: started.elapsed(),
            stage,
        });
        self.last_painted = Some(slot_index);

        let slot = &self.slots[slot_index];
        if self.converted == 0 {
            tracing::info!(
                fourcc = format!("0x{:08X}", export::DRM_FORMAT_XRGB8888),
                modifier = format!("0x{:016X}", slot.modifier),
                stride = slot.stride,
                width = self.output_width,
                height = self.output_height,
                "首帧 RGB 转换完成"
            );
        }
        self.converted = self.converted.saturating_add(1);
        slot.inflight.fetch_add(1, Ordering::Release);
        self.inflight_total.fetch_add(1, Ordering::Release);
        let keepalive = FrameKeepalive::new(SlotGuard {
            _buffer: Arc::clone(&slot.buffer),
            inflight: Arc::clone(&slot.inflight),
            total: Arc::clone(&self.inflight_total),
        });
        Ok(output_frame(
            self.output_width,
            self.output_height,
            Arc::clone(&slot.fd),
            slot.stride,
            slot.offset,
            slot.modifier,
            keepalive,
        ))
    }

    fn paint(
        &mut self,
        slot_index: usize,
        frame: &DmaBufFrame,
    ) -> Result<StageTimings, VaapiError> {
        let image_started = Instant::now();
        let planes = nv12_planes(frame)?;
        let modifier = (frame.modifier != DRM_FORMAT_MOD_INVALID).then_some(frame.modifier);
        let framebuffer = self.slots[slot_index].framebuffer;

        let y_image = self.create_image_cached(
            &PlaneDesc {
                label: "平面 0 (Y)",
                width: self.source_width,
                height: self.source_height,
                fourcc: DRM_FORMAT_R8,
                fd: planes.y_fd,
                offset: planes.y_offset,
                pitch: planes.y_pitch,
                modifier,
            },
            false,
        )?;
        let uv_size = (self.source_width / 2, self.source_height / 2);
        let (uv_image, dialect) = self.import_uv_cached(&planes, uv_size, modifier)?;
        let image = image_started.elapsed();

        let bind_started = Instant::now();
        let y_texture = self
            .renderer
            .bind_plane_texture(0, &y_image, "平面 0 (Y)")?;
        let uv_texture = match self
            .renderer
            .bind_plane_texture(1, &uv_image, "平面 1 (UV)")
        {
            Ok(texture) => texture,
            Err(error) => {
                self.renderer.delete_textures(&[y_texture]);
                return Err(error);
            }
        };
        let bind = bind_started.elapsed();

        let draw_started = Instant::now();
        let (positions, texcoords) = self.render_geometry();
        let swap_uv = dialect.swaps_uv(self.is_nvidia_gbm);
        let drawn = self.renderer.draw(DrawArgs {
            framebuffer,
            y_texture,
            uv_texture,
            uv_swap: swap_uv,
            matrix: OUTPUT_MATRIX,
            positions: &positions,
            texcoords: &texcoords,
            brightness: self.transform.brightness as f32,
        });
        let draw = draw_started.elapsed();
        self.renderer.delete_textures(&[y_texture, uv_texture]);
        drop(uv_image);
        drop(y_image);
        drawn?;

        // glFinish 单独计时：它是"等 GPU 排空"，优化空间和绘制调用完全不同。
        let finish_started = Instant::now();
        self.renderer.finish()?;
        let finish = finish_started.elapsed();

        if self.uv_dialect != dialect {
            tracing::info!(
                dialect = dialect.name(),
                swap_uv,
                "UV 平面改用另一种 fourcc 方言导入"
            );
            self.uv_dialect = dialect;
        }
        Ok(StageTimings {
            image,
            bind,
            draw,
            finish,
        })
    }

    /// 按 BO 身份取 EGLImage：命中缓存直接复用，未命中才 `eglCreateImageKHR`。
    fn create_image_cached(
        &mut self,
        plane: &PlaneDesc,
        uv_swap: bool,
    ) -> Result<Rc<EglImage>, VaapiError> {
        let key = ImageKey::of(plane, uv_swap)?;
        if let Some(image) = self.images.lookup(&key) {
            self.stats.record_reuse(1);
            return Ok(image);
        }
        let image = Rc::new(self.renderer.create_image(plane)?);
        self.images.insert(key, Rc::clone(&image));
        Ok(image)
    }

    fn import_uv_cached(
        &mut self,
        planes: &RawPlanes,
        size: (u32, u32),
        modifier: Option<u64>,
    ) -> Result<(Rc<EglImage>, UvDialect), VaapiError> {
        let preferred = self.uv_dialect;
        let plane = |dialect: UvDialect| PlaneDesc {
            label: "平面 1 (UV)",
            width: size.0,
            height: size.1,
            fourcc: dialect.fourcc(),
            fd: planes.uv_fd,
            offset: planes.uv_offset,
            pitch: planes.uv_pitch,
            modifier,
        };
        let is_nvidia = self.is_nvidia_gbm;
        match self.create_image_cached(&plane(preferred), preferred.swaps_uv(is_nvidia)) {
            Ok(image) => Ok((image, preferred)),
            Err(first) => {
                let fallback = preferred.fallback();
                tracing::debug!(
                    preferred = preferred.name(),
                    fallback = fallback.name(),
                    error = %first,
                    "UV 平面按首选 fourcc 导入失败，回退另一种方言"
                );
                let image =
                    self.create_image_cached(&plane(fallback), fallback.swaps_uv(is_nvidia))?;
                Ok((image, fallback))
            }
        }
    }

    /// 计算当前转换器的 GPU 绘制顶点与旋转纹理坐标。
    fn render_geometry(&self) -> ([f32; 8], [f32; 8]) {
        compute_quad_geometry(
            self.visible_width,
            self.visible_height,
            self.output_width,
            self.output_height,
            self.transform,
        )
    }

    /// 当前被合成器占用（尚未归还）的输出槽位数。
    pub fn inflight(&self) -> usize {
        self.inflight_total.load(Ordering::Acquire)
    }

    /// 槽位占用计数的**跨线程句柄**。
    ///
    /// 转换器被解码预读线程独占之后（见 `producer.rs`），生产者不再能调用
    /// `&self` 方法，但仍要报 `keepalive_len`；这个 `Arc` 与每次
    /// [`RgbConverter::convert`] 的槽位占用同步加减，可以安全地在另一线程读。
    pub fn inflight_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.inflight_total)
    }

    /// 输出槽位总数。
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// 输出帧的 gbm modifier（用于日志；槽位一致时取第一个）。
    pub fn output_modifier(&self) -> Option<u64> {
        self.slots.first().map(|slot| slot.modifier)
    }

    /// 输出帧尺寸。
    pub fn dimensions(&self) -> (u32, u32) {
        (self.output_width, self.output_height)
    }

    /// 输入帧尺寸。
    pub fn source_dimensions(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    /// 已转换的帧数。
    pub fn converted(&self) -> u64 {
        self.converted
    }

    /// 当前记忆的 UV 方言名（诊断用）。
    pub fn uv_dialect(&self) -> &'static str {
        self.uv_dialect.name()
    }

    /// 回读最近一次成功 `convert` 的输出，返回 `(width, height, RGBA 自上而下)`。
    ///
    /// 只给诊断探针（`examples/rgb_convert_probe.rs`）用：正常播放路径不回读。
    /// 输出 BO 可能是块状 modifier，`mmap` 按行读会错位，所以走 GPU 回读。
    pub fn readback_last_xrgb(&mut self) -> Result<(u32, u32, Vec<u8>), VaapiError> {
        let Some(slot_index) = self.last_painted else {
            return Err(VaapiError::Unsupported(
                "还没有成功转换过任何帧，无法回读".to_string(),
            ));
        };
        let framebuffer = self.slots[slot_index].framebuffer;
        self.renderer.acquire()?;
        let read = self.renderer.read_framebuffer(framebuffer);
        self.renderer.release();
        let data = read?;
        Ok((self.output_width, self.output_height, data))
    }
}

/// 源 → 目标的 cover（填满并居中裁切）映射，返回 `(u0, v0, du, dv)`。
#[cfg_attr(not(test), allow(dead_code))]
fn cover_crop(
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
) -> (f32, f32, f32, f32) {
    if source_width == 0 || source_height == 0 || output_width == 0 || output_height == 0 {
        return (0.0, 0.0, 1.0, 1.0);
    }
    let source_aspect = source_width as f64 / source_height as f64;
    let output_aspect = output_width as f64 / output_height as f64;
    if (source_aspect - output_aspect).abs() < 1e-9 {
        return (0.0, 0.0, 1.0, 1.0);
    }
    if source_aspect > output_aspect {
        // 源更宽：裁掉左右两边。
        let visible = (output_aspect / source_aspect) as f32;
        (((1.0 - visible) / 2.0), 0.0, visible, 1.0)
    } else {
        // 源更高：裁掉上下两边。
        let visible = (source_aspect / output_aspect) as f32;
        (0.0, ((1.0 - visible) / 2.0), 1.0, visible)
    }
}

/// 根据 `RenderTransform` 计算 GL 裁剪空间四边形与旋转纹理坐标。
///
/// GL_TRIANGLE_STRIP 顺序为 `[BL, BR, TL, TR]`，与 `VERTEX_SHADER` 对
/// `a_position.y` 取反的方向约定一致：`a_position.y = +1` 是输出顶部。
fn compute_quad_geometry(
    visible_width: u32,
    visible_height: u32,
    output_width: u32,
    output_height: u32,
    transform: RenderTransform,
) -> ([f32; 8], [f32; 8]) {
    let texcoords = match transform.rotation {
        Rotation::Deg0 => [0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0],
        Rotation::Deg90 => [1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        Rotation::Deg180 => [1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0],
        Rotation::Deg270 => [0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0],
    };
    if visible_width == 0 || visible_height == 0 || output_width == 0 || output_height == 0 {
        return (QUAD_POSITIONS, texcoords);
    }
    let effective = transform
        .rotation
        .rotated_size((visible_width, visible_height));
    let params = FitParams {
        mode: transform.fit,
        zoom: transform.zoom,
        offset_x: transform.offset_x,
        offset_y: transform.offset_y,
    };
    let Ok(fit) = compute_fit(effective, (output_width, output_height), &params) else {
        return (QUAD_POSITIONS, texcoords);
    };
    let ow = f64::from(output_width);
    let oh = f64::from(output_height);
    let x0 = (-1.0 + 2.0 * (fit.dst_x / ow)) as f32;
    let x1 = (-1.0 + 2.0 * ((fit.dst_x + fit.dst_width) / ow)) as f32;
    let y_top = (1.0 - 2.0 * (fit.dst_y / oh)) as f32;
    let y_bot = (1.0 - 2.0 * ((fit.dst_y + fit.dst_height) / oh)) as f32;
    ([x0, y_bot, x1, y_bot, x0, y_top, x1, y_top], texcoords)
}

impl Drop for RgbConverter {
    fn drop(&mut self) {
        // 先在有当前上下文时删 GL 对象；槽位里的 EGLImage / BO 随后由字段析构。
        if self.renderer.acquire().is_ok() {
            Self::destroy_slots(&self.renderer, &self.slots);
            self.renderer.release();
        }
    }
}

impl fmt::Debug for RgbConverter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RgbConverter")
            .field("dimensions", &self.dimensions())
            .field("slots", &self.slots.len())
            .field("inflight", &self.inflight())
            .field("uv_dialect", &self.uv_dialect.name())
            .field("converted", &self.converted)
            .finish()
    }
}

// SAFETY: 转换器只在构造它的线程里初始化 EGL（编译完着色器即 `eglMakeCurrent(None)`），
// 随后整体移动到唯一的使用线程；每次 `convert` 前后各自 make current / release，
// 不会被两个线程同时持有。这与 `decoder.rs` 里 `VaapiDecoder` 的处理一致。
unsafe impl Send for RgbConverter {}

// ---------------------------------------------------------------------------
// 单测（纯逻辑，不需要 GPU）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    #[test]
    fn visible_tex_scale_crops_alignment_padding() {
        // 1080p 的 NV12 表面是 1920x1088（高度 16 对齐）：纵向只取 1080/1088。
        let (x, y) = visible_tex_scale((1920, 1088), (1920, 1080));
        assert_eq!(x, 1.0);
        assert!((y - 1080.0 / 1088.0).abs() < 1e-6, "y={y}");

        // 1530p → 1536：两个方向都不该被裁，只有纵向收窄。
        let (x, y) = visible_tex_scale((2720, 1536), (2720, 1530));
        assert_eq!(x, 1.0);
        assert!((y - 1530.0 / 1536.0).abs() < 1e-6, "y={y}");

        // 4K 的 2160 本来就 16 对齐 ⇒ 恒等（这正是"只有龙族不卡"的原因）。
        assert_eq!(visible_tex_scale((3840, 2160), (3840, 2160)), (1.0, 1.0));
    }

    #[test]
    fn visible_tex_scale_falls_back_to_identity() {
        assert_eq!(visible_tex_scale((1920, 1088), (0, 0)), (1.0, 1.0));
        assert_eq!(visible_tex_scale((0, 0), (1920, 1080)), (1.0, 1.0));
        // 可见大于分配：不裁（采样越界会得到边缘拉伸，比露填充更糟）。
        assert_eq!(visible_tex_scale((1920, 1088), (1920, 1200)), (1.0, 1.0));
    }
    use super::*;

    #[test]
    fn cover_crop_uses_visible_size_not_aligned_allocation() {
        // 1080p 解码表面按 16 对齐分配为 1920x1088，但有效画面是 1920x1080；
        // 按 1920x1080 适配 2560x1600 应和 4K (16:9) 完全一致（左右各裁 5%）。
        let visible_crop = cover_crop(1920, 1080, 2560, 1600);
        let uhd_crop = cover_crop(3840, 2160, 2560, 1600);
        assert!((visible_crop.0 - uhd_crop.0).abs() < 1e-6);
        assert!((visible_crop.2 - uhd_crop.2).abs() < 1e-6);
        assert_eq!((visible_crop.1, visible_crop.3), (0.0, 1.0));
        // 若误用 1920x1088 会得到更窄的可见比例。
        let aligned_crop = cover_crop(1920, 1088, 2560, 1600);
        assert!((aligned_crop.2 - visible_crop.2).abs() > 0.005);
    }

    #[test]
    fn cover_crop_identity_when_aspect_matches() {
        assert_eq!(cover_crop(3840, 2160, 1920, 1080), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(cover_crop(2560, 1600, 2560, 1600), (0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn cover_crop_trims_sides_for_4k_into_16x10() {
        // 3840x2160 (16:9) → 2560x1600 (16:10)：源更宽，左右各裁 5%。
        let (u0, v0, du, dv) = cover_crop(3840, 2160, 2560, 1600);
        assert!((u0 - 0.05).abs() < 1e-6, "u0={u0}");
        assert!((du - 0.9).abs() < 1e-6, "du={du}");
        assert_eq!((v0, dv), (0.0, 1.0));
    }

    #[test]
    fn cover_crop_trims_vertical_bars_when_source_is_taller() {
        // 1080x1920（竖屏）→ 2560x1600：源更高，上下各裁。
        let (u0, v0, du, dv) = cover_crop(1080, 1920, 2560, 1600);
        assert_eq!((u0, du), (0.0, 1.0));
        // 可见高度 = (0.5625/1.6) = 0.3515625，上下各留一半。
        assert!((dv - 0.351_562_5).abs() < 1e-6, "dv={dv}");
        assert!((v0 - 0.324_218_75).abs() < 1e-6, "v0={v0}");
    }

    #[test]
    fn cover_crop_is_centered_and_matches_compositor_cover() {
        // 4K → 16:10：可见宽度 = 1.6 / (16/9) = 0.9，左右各让 0.05；
        // 合成器按同一规则裁切，所以两边的可见画面一致。
        let (u0, v0, du, dv) = cover_crop(3840, 2160, 2560, 1600);
        assert!((u0 - 0.05).abs() < 1e-6, "u0={u0}");
        assert!((du - 0.9).abs() < 1e-6, "du={du}");
        assert_eq!((v0, dv), (0.0, 1.0));
        // 裁剪是对称的：左边让出的量 == 右边让出的量。
        assert!((u0 - (1.0 - (u0 + du))).abs() < 1e-6);
    }

    #[test]
    fn quad_geometry_matches_cover_and_handles_contain_zoom_rotation() {
        use liuguang_core::fit::FitMode;

        // Cover 默认与 16:9 -> 16:10 裁剪等价：左右超出 [-1, 1]，上下正好 [-1, 1]。
        let (pos, uv) = compute_quad_geometry(3840, 2160, 2560, 1600, RenderTransform::default());
        assert!((pos[0] + (1.0 / 0.9)).abs() < 1e-4, "x0={}", pos[0]);
        assert!((pos[2] - (1.0 / 0.9)).abs() < 1e-4, "x1={}", pos[2]);
        assert!((pos[1] + 1.0).abs() < 1e-5);
        assert!((pos[5] - 1.0).abs() < 1e-5);
        assert_eq!(uv, [0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0]);

        // Contain 在 16:10 屏幕上显示 16:9：左右撑满 [-1, 1]，上下留黑边（|< 1|）。
        let contain = RenderTransform {
            fit: FitMode::Contain,
            ..RenderTransform::default()
        };
        let (pos, _) = compute_quad_geometry(3840, 2160, 2560, 1600, contain);
        assert!((pos[0] + 1.0).abs() < 1e-5);
        assert!((pos[2] - 1.0).abs() < 1e-5);
        assert!(pos[5] < 1.0 && pos[1] > -1.0);

        // 顺时针 90° 后宽高交换，纹理坐标角点旋转；zoom/offset 放大并右移。
        let rotated = RenderTransform {
            fit: FitMode::Contain,
            zoom: 2.0,
            offset_x: 100.0,
            offset_y: 0.0,
            rotation: Rotation::Deg90,
            brightness: 0.75,
        };
        let (pos_r, uv_r) = compute_quad_geometry(1920, 1080, 2560, 1600, rotated);
        assert_eq!(uv_r, [1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert!(pos_r[2] + pos_r[0] > 0.0, "正 offset_x 应使四边形整体右移");
    }

    fn null_fd() -> Arc<OwnedFd> {
        let file =
            File::open("/dev/null").unwrap_or_else(|error| panic!("打开 /dev/null 失败: {error}"));
        Arc::new(OwnedFd::from(file))
    }

    fn frame_with(fourcc: u32, object_index: [u8; 4], fds: usize) -> DmaBufFrame {
        DmaBufFrame {
            width: 1920,
            height: 1080,
            stride: 1920,
            format: PixelFormat::Nv12,
            fds: (0..fds).map(|_| null_fd()).collect(),
            modifier: 0,
            fourcc,
            strides: [1920, 1920, 0, 0],
            offsets: [0, 0, 0, 0],
            object_index,
            keepalive: None,
        }
    }

    #[test]
    fn uv_dialects_are_mirror_images() {
        assert_eq!(UvDialect::Rg88.fallback(), UvDialect::Gr88);
        assert_eq!(UvDialect::Gr88.fallback(), UvDialect::Rg88);
        assert_eq!(UvDialect::PREFERRED, UvDialect::Gr88);
        assert_eq!(UvDialect::Rg88.name(), "RG88");
        assert_eq!(UvDialect::Gr88.name(), "GR88");
    }

    #[test]
    fn uv_dialects_map_to_expected_fourcc_and_swap() {
        assert_eq!(UvDialect::Rg88.fourcc(), DRM_FORMAT_RG88);
        assert_eq!(UvDialect::Rg88.fourcc(), 0x3838_4752);
        // NVIDIA 专有 EGL 对 RG88 不交换；标准 DRM 驱动下 RG88 需交换、GR88 永不交换。
        assert!(!UvDialect::Rg88.swaps_uv(true));
        assert!(UvDialect::Rg88.swaps_uv(false));
        assert_eq!(UvDialect::Gr88.fourcc(), DRM_FORMAT_GR88);
        assert_eq!(UvDialect::Gr88.fourcc(), 0x3838_5247);
        assert!(!UvDialect::Gr88.swaps_uv(false));
        assert!(!UvDialect::Gr88.swaps_uv(true));
    }

    #[test]
    fn matrix_matches_bt709_limited_range() {
        assert_eq!(
            Matrix::Bt709.coefficients(),
            [1.164, 1.164, 1.164, 0.0, -0.213, 2.112, 1.793, -0.533, 0.0]
        );
        assert_eq!(
            Matrix::Bt709.offset(),
            [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]
        );
        assert_eq!(
            Matrix::Bt601.coefficients(),
            [1.164, 1.164, 1.164, 0.0, -0.392, 2.017, 1.596, -0.813, 0.0]
        );
        assert_eq!(OUTPUT_MATRIX, Matrix::Bt709);
    }

    #[test]
    fn plane_attributes_carry_modifier_words() {
        let plane = PlaneDesc {
            label: "平面 0 (Y)",
            width: 3840,
            height: 2160,
            fourcc: DRM_FORMAT_R8,
            fd: 42,
            offset: 0,
            pitch: 3840,
            modifier: Some(0x0300_0000_00E0_8014),
        };
        let mut attributes = Vec::new();
        push_plane_attributes(&mut attributes, &plane);
        assert_eq!(attributes.last(), Some(&EGL_NONE_I32));
        assert_eq!(attributes.first(), Some(&EGL_WIDTH_I32));
        assert_eq!(attributes.get(1), Some(&3840));
        let pair = |key: i32| -> Option<i32> {
            attributes
                .iter()
                .position(|value| *value == key)
                .and_then(|index| attributes.get(index + 1))
                .copied()
        };
        assert_eq!(pair(EGL_LINUX_DRM_FOURCC_EXT), Some(DRM_FORMAT_R8 as i32));
        assert_eq!(pair(EGL_PLANE0_FD), Some(42));
        assert_eq!(pair(EGL_PLANE0_PITCH), Some(3840));
        assert_eq!(pair(EGL_PLANE0_MODIFIER_LO), Some(0x00E0_8014_i32));
        assert_eq!(pair(EGL_PLANE0_MODIFIER_HI), Some(0x0300_0000_i32));
    }

    #[test]
    fn plane_attributes_include_linear_and_omit_invalid_modifier() {
        let linear_plane = PlaneDesc {
            label: "输出",
            width: 2560,
            height: 1600,
            fourcc: export::DRM_FORMAT_XRGB8888,
            fd: 7,
            offset: 0,
            pitch: 10240,
            modifier: Some(0),
        };
        let mut linear_attrs = Vec::new();
        push_plane_attributes(&mut linear_attrs, &linear_plane);
        assert!(linear_attrs.contains(&EGL_PLANE0_MODIFIER_LO));
        assert!(linear_attrs.contains(&EGL_PLANE0_MODIFIER_HI));
        assert_eq!(linear_attrs.last(), Some(&EGL_NONE_I32));

        let invalid_plane = PlaneDesc {
            modifier: Some(DRM_FORMAT_MOD_INVALID),
            ..linear_plane
        };
        let mut invalid_attrs = Vec::new();
        push_plane_attributes(&mut invalid_attrs, &invalid_plane);
        assert!(!invalid_attrs.contains(&EGL_PLANE0_MODIFIER_LO));
        assert!(!invalid_attrs.contains(&EGL_PLANE0_MODIFIER_HI));
        assert_eq!(invalid_attrs.last(), Some(&EGL_NONE_I32));
    }

    #[test]
    fn output_frame_maps_slot_fields() {
        struct Probe;
        let frame = output_frame(
            2560,
            1600,
            null_fd(),
            10240,
            128,
            0x0300_0000_00E0_8014,
            FrameKeepalive::new(Probe),
        );
        assert_eq!(frame.width, 2560);
        assert_eq!(frame.height, 1600);
        assert_eq!(frame.fourcc, export::DRM_FORMAT_XRGB8888);
        assert_eq!(frame.fourcc, 0x3432_5258);
        assert_eq!(frame.format, PixelFormat::Xrgb8888);
        assert_eq!(frame.fds.len(), 1);
        assert_eq!(frame.object_index, [0, 0, 0, 0]);
        assert_eq!(frame.object_count(), 1);
        assert!(!frame.is_multi_object());
        assert_eq!(frame.stride, 10240);
        assert_eq!(frame.strides, [10240, 0, 0, 0]);
        assert_eq!(frame.offsets, [128, 0, 0, 0]);
        assert_eq!(frame.modifier, 0x0300_0000_00E0_8014);
        assert!(frame.keepalive.is_some());
        assert_eq!(
            frame.keepalive.as_ref().map(FrameKeepalive::strong_count),
            Some(1)
        );
    }

    #[test]
    fn nv12_planes_reads_separate_objects() {
        let frame = frame_with(export::DRM_FORMAT_NV12, [0, 1, 0, 0], 2);
        let planes = match nv12_planes(&frame) {
            Ok(planes) => planes,
            Err(error) => panic!("应能解析 NV12 两对象帧: {error}"),
        };
        assert_eq!(planes.y_fd, frame.fds[0].as_raw_fd());
        assert_eq!(planes.uv_fd, frame.fds[1].as_raw_fd());
        assert_eq!(planes.y_pitch, 1920);
        assert_eq!(planes.uv_pitch, 1920);
    }

    #[test]
    fn nv12_planes_reads_single_object_offsets() {
        let mut frame = frame_with(export::DRM_FORMAT_NV12, [0, 0, 0, 0], 1);
        frame.offsets = [0, 1920 * 1080, 0, 0];
        let planes = match nv12_planes(&frame) {
            Ok(planes) => planes,
            Err(error) => panic!("应能解析 NV12 单对象帧: {error}"),
        };
        assert_eq!(planes.y_fd, planes.uv_fd);
        assert_eq!(planes.y_offset, 0);
        assert_eq!(planes.uv_offset, 1920 * 1080);
    }

    #[test]
    fn nv12_planes_rejects_foreign_format_and_bad_index() {
        let xrgb = frame_with(export::DRM_FORMAT_XRGB8888, [0, 0, 0, 0], 1);
        match nv12_planes(&xrgb) {
            Err(VaapiError::Unsupported(message)) => {
                assert!(message.contains("NV12"), "错误文本应点明 NV12: {message}");
            }
            Ok(_) => panic!("XRGB8888 不该被当成 NV12"),
            Err(other) => panic!("意外错误: {other}"),
        }

        let bad_index = frame_with(export::DRM_FORMAT_NV12, [0, 5, 0, 0], 1);
        match nv12_planes(&bad_index) {
            Err(VaapiError::Unsupported(message)) => {
                assert!(
                    message.contains("对象下标"),
                    "错误文本应点明对象下标: {message}"
                );
            }
            Ok(_) => panic!("越界的 object_index 不该被接受"),
            Err(other) => panic!("意外错误: {other}"),
        }
    }

    #[test]
    fn nv12_planes_rejects_zero_pitch() {
        let mut frame = frame_with(export::DRM_FORMAT_NV12, [0, 1, 0, 0], 2);
        frame.strides = [0, 0, 0, 0];
        match nv12_planes(&frame) {
            Err(VaapiError::Unsupported(message)) => {
                assert!(message.contains("pitch"), "错误文本应点明 pitch: {message}");
            }
            Ok(_) => panic!("pitch 为 0 不该被接受"),
            Err(other) => panic!("意外错误: {other}"),
        }
    }
}
