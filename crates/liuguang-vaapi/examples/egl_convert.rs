//! 真机原型：把解码器导出的 NV12 `DMA-BUF` 用 EGL + GLES2 转成 `XRGB8888 DMA-BUF`。
//!
//! ```text
//! cargo run -p liuguang-vaapi --example egl_convert -- \
//!     <视频> <渲染节点> [--ppm 输出路径] [--frames N] [--matrix bt601|bt709] \
//!     [--readback auto|cpu|gl] [--uv-fourcc rg88|gr88] [--uv-swap]
//! ```
//!
//! # 为什么不用 `GL_TEXTURE_EXTERNAL_OES`
//!
//! denial 合成器把 NV12 帧直接当成一张 `EGLImage` 贴到 `GL_TEXTURE_EXTERNAL_OES`
//! 采样，结果整屏发红：NVIDIA 的 external 纹理只暴露第一个平面（Y），并没有
//! 隐式的 YUV→RGB，于是 R≈Y、G/B 是常量，画面偏红且丢掉色度。
//!
//! 所以这里改成「两个平面各自导入 + 手写着色器」：
//!
//! * 平面 0（Y）→ `DRM_FORMAT_R8`（`0x2020_3852`）的单通道 `EGLImage`；
//! * 平面 1（UV）→ 双通道 `EGLImage`，宽高各取一半（fourcc 见下）；
//! * 两者都用 `glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, …)` 绑成普通 2D
//!   纹理（不是 `EXTERNAL_OES`）；
//! * 片元着色器按 BT.601 / BT.709 的「有限范围」公式做矩阵换算。
//!
//! # UV 平面的驱动方言
//!
//! 同一块 UV 数据，NVIDIA 只接受 `DRM_FORMAT_RG88`（`0x3838_4752`），
//! Mesa / Intel 只接受 `DRM_FORMAT_GR88`（`0x3838_5247`）。用错的那个
//! `eglCreateImageKHR` 返回 `NULL`，`eglGetError()` 是 `EGL_BAD_MATCH`
//! （`0x3009`）；带不带 modifier 都一样，跟 `Y` 平面无关。两种命名对应的
//! `u`/`v` 顺序相反，所以配 `--uv-fourcc` 选格式、`--uv-swap` 交换分量。
//! 实测两台机器在各自认的格式下画出的像素几乎逐点一致（MAE ≈ 0.06/通道）。
//!
//! # 输出
//!
//! 用 `gbm` 分配一块 `XRGB8888`：先试 `RENDERING | LINEAR`，失败再退到
//! `RENDERING`。NVIDIA(620.70 + Mesa 26.2.7 的 gbm nvidia 后端) 上
//! `RENDERING | LINEAR` 一定 `EINVAL`：它的分配器拿不出「既能被 GL 渲染、
//! 又是 linear 布局」的 BO —— 带 `RENDERING` 就得到 block-linear
//! （modifier `0x0300_0000_00E0_8014`），只要 `LINEAR` 没 `RENDERING`，
//! 导入成纹理时又会报 `GL_INVALID_OPERATION`（`0x0502`）。Mesa / Intel
//! （`gbm` 的 `drm` 后端）两者可以同时满足，拿到 `modifier = 0`。
//!
//! 因此回读有两条路：`--readback cpu` 直接 `mmap`（只有 LINEAR 布局才有意义），
//! `--readback gl` 用 `glReadPixels` 从 FBO 读（块状布局也正确，是 GPU 眼里的
//! 真实内容）；`auto` 在 modifier 非 0 时走后者。两条路都会翻成自上而下，
//! 再按 `[B, G, R, X]` → RGB 写 P6 PPM 并算全幅平均 —— 均值可以和
//! `ffmpeg -f rawvideo -pix_fmt rgb24` 的结果直接对照，用来判断颜色对不对。
//!
//! # EGL 扩展要求
//!
//! * `EGL_EXT_image_dma_buf_import` —— 导入 dmabuf，必需；
//! * `EGL_EXT_image_dma_buf_import_modifiers` —— 带 `modifier` 属性导入，
//!   缺失时会自动退化成「省略 modifier」再试一次；
//! * `EGL_KHR_surfaceless_context` —— 没有它就用 pbuffer 当 current context。
//!
//! 注意：`khronos-egl` 的 `Instance::create_image` 把属性表的元素类型定义成
//! `usize`（`Attrib`），而 EGL ABI 要求的是 `EGLint`（`i32`）。在 64 位平台上
//! 这个包装函数会写出错位的属性表，所以这里直接用 `eglGetProcAddress` 取
//! `eglCreateImageKHR` / `eglDestroyImageKHR` / `eglGetError` 自己调用 ——
//! 顺带还能拿到 `eglGetError()` 的原始错误码，失败时便于定位。

#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_void};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;

use gbm::AsRaw;
use khronos_egl as egl;
use liuguang_core::types::DmaBufFrame;
use liuguang_vaapi::{VaapiDecoder, export};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// 构造一个带消息的错误，供 `?` / `return` 直接使用。
fn msg(text: impl Into<String>) -> Box<dyn std::error::Error> {
    std::io::Error::other(text.into()).into()
}

/// `fail!("…")`：在返回 `Res<_>` 的函数里提前带消息退出。
macro_rules! fail {
    ($($arg:tt)*) => {
        return Err(msg(format!($($arg)*)))
    };
}

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// `EGL_PLATFORM_GBM_KHR`（`EGL_KHR_platform_gbm`）。
const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
/// `EGL_LINUX_DMA_BUF_EXT`（`EGL_EXT_image_dma_buf_import`）。
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
/// `EGL_LINUX_DRM_FOURCC_EXT`。
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
/// `EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT` / `…HI_EXT`。
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
/// `EGL_WIDTH` / `EGL_HEIGHT`（`i32` 形式，用于属性表）。
const EGL_WIDTH_I32: i32 = 0x3057;
const EGL_HEIGHT_I32: i32 = 0x3056;
/// `EGL_NONE`，属性表终止符。
const EGL_NONE_I32: i32 = 0x3038;
/// `EGL_OPENGL_ES2_BIT`，`EGL_RENDERABLE_TYPE` 的取值。
const EGL_OPENGL_ES2_BIT: egl::Int = 0x0004;

/// `DRM_FORMAT_R8`：单通道亮度平面。
const DRM_FORMAT_R8: u32 = 0x2020_3852;
/// `DRM_FORMAT_RG88`：双通道交错色度平面（内存里第一个字节是 `R` 分量）。
const DRM_FORMAT_RG88: u32 = 0x3838_4752;
/// `DRM_FORMAT_GR88`：同一块 UV 数据的另一种命名（内存里第一个字节是 `G` 分量）。
/// NVIDIA 只认 `RG88`、Mesa 只认 `GR88`，两者对应的 `u`/`v` 顺序相反，
/// 所以配套一个 `u_uv_swap` uniform 来纠正。
const DRM_FORMAT_GR88: u32 = 0x3838_5247;
/// `DRM_FORMAT_XRGB8888`：输出格式（内存字节序 `[B, G, R, X]`）。
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
/// `DRM_FORMAT_NV12`：解码器导出的输入格式。
const DRM_FORMAT_NV12: u32 = 0x3231_564E;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TEXTURE1: u32 = 0x84C1;
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
const GL_NO_ERROR: u32 = 0;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;
const GL_EXTENSIONS: u32 = 0x1F03;

/// 全屏两个三角形（`GL_TRIANGLE_STRIP`）的裁剪空间坐标。
const QUAD_POSITIONS: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
/// 与 [`QUAD_POSITIONS`] 对应的纹理坐标（`v=0` 落在裁剪空间顶部）。
const QUAD_TEXCOORDS: [f32; 8] = [0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0];

const VERTEX_SHADER: &str = "\
#version 100
attribute vec2 a_position;
attribute vec2 a_texcoord;
varying vec2 v_texcoord;
void main() {
    gl_Position = vec4(a_position, 0.0, 1.0);
    v_texcoord = a_texcoord;
}
";

/// 两个平面分别采样后手动做 YUV→RGB。
///
/// `yuv = vec3(Y, U, V) - u_offset` 把有限范围的码值挪到 0 附近，
/// `u_matrix` 的列依次是 Y / U / V 对 (R, G, B) 的贡献。
const FRAGMENT_SHADER: &str = "\
#version 100
precision mediump float;
uniform sampler2D u_tex_y;
uniform sampler2D u_tex_uv;
uniform mat3 u_matrix;
uniform vec3 u_offset;
uniform int u_uv_swap;
varying vec2 v_texcoord;
void main() {
    float y = texture2D(u_tex_y, v_texcoord).r;
    vec2 uv = texture2D(u_tex_uv, v_texcoord).rg;
    uv = mix(uv, uv.yx, float(u_uv_swap));
    vec3 yuv = vec3(y, uv.x, uv.y) - u_offset;
    gl_FragColor = vec4(u_matrix * yuv, 1.0);
}
";

// ---------------------------------------------------------------------------
// 命令行
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Matrix {
    Bt601,
    Bt709,
}

impl Matrix {
    fn label(self) -> &'static str {
        match self {
            Self::Bt601 => "BT.601 有限范围",
            Self::Bt709 => "BT.709 有限范围",
        }
    }

    /// 列主序的 3×3 系数矩阵（列 = Y、U、V 的系数）。
    ///
    /// 公式取自 BT.601 / BT.709 的「有限范围」（limited range）换算：
    /// `Y' = (Y - 16) / 219`、`U' = (U - 128) / 224`、`V' = (V - 128) / 224`。
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

/// UV 平面用哪个 fourcc 导入：NVIDIA 只接受 `RG88`，Mesa/Intel 只接受 `GR88`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UvFourcc {
    Rg88,
    Gr88,
}

impl UvFourcc {
    fn fourcc(self) -> u32 {
        match self {
            Self::Rg88 => DRM_FORMAT_RG88,
            Self::Gr88 => DRM_FORMAT_GR88,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Rg88 => "RG88",
            Self::Gr88 => "GR88",
        }
    }
}

/// 回读方式：`cpu` 是 mmap dmabuf（只有 LINEAR 布局才有意义），
/// `gl` 是 `glReadPixels` 从 FBO 读（GPU 侧，块状布局也正确）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readback {
    Auto,
    Cpu,
    Gl,
}

struct Args {
    video: PathBuf,
    node: PathBuf,
    ppm: Option<PathBuf>,
    frames: usize,
    matrix: Matrix,
    readback: Readback,
    uv_fourcc: UvFourcc,
    uv_swap: bool,
}

impl Args {
    fn parse() -> Res<Self> {
        const USAGE: &str = "用法: egl_convert <视频> <渲染节点> [--ppm 输出路径] [--frames N] [--matrix bt601|bt709] [--readback auto|cpu|gl] [--uv-fourcc rg88|gr88] [--uv-swap]";

        let mut positional: Vec<String> = Vec::new();
        let mut ppm = None;
        let mut frames = 1usize;
        let mut matrix = Matrix::Bt709;
        let mut readback = Readback::Auto;
        let mut uv_fourcc = UvFourcc::Rg88;
        let mut uv_swap = false;

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--ppm" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| msg(format!("--ppm 缺少参数\n{USAGE}")))?;
                    ppm = Some(PathBuf::from(value));
                }
                "--frames" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| msg(format!("--frames 缺少参数\n{USAGE}")))?;
                    frames = value.parse().map_err(|error| {
                        msg(format!("--frames 需要正整数，收到 {value:?}: {error}"))
                    })?;
                    if frames == 0 {
                        return Err(msg("--frames 不能是 0"));
                    }
                }
                "--matrix" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| msg(format!("--matrix 缺少参数\n{USAGE}")))?;
                    matrix = match value.as_str() {
                        "bt601" | "601" => Matrix::Bt601,
                        "bt709" | "709" => Matrix::Bt709,
                        other => {
                            return Err(msg(format!(
                                "--matrix 只接受 bt601 / bt709，收到 {other:?}"
                            )));
                        }
                    };
                }
                "--readback" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| msg(format!("--readback 缺少参数\n{USAGE}")))?;
                    readback = match value.as_str() {
                        "auto" => Readback::Auto,
                        "cpu" => Readback::Cpu,
                        "gl" => Readback::Gl,
                        other => {
                            return Err(msg(format!(
                                "--readback 只接受 auto / cpu / gl，收到 {other:?}"
                            )));
                        }
                    };
                }
                "--uv-fourcc" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| msg(format!("--uv-fourcc 缺少参数\n{USAGE}")))?;
                    uv_fourcc = match value.to_ascii_lowercase().as_str() {
                        "rg88" => UvFourcc::Rg88,
                        "gr88" => UvFourcc::Gr88,
                        other => {
                            return Err(msg(format!(
                                "--uv-fourcc 只接受 rg88 / gr88，收到 {other:?}"
                            )));
                        }
                    };
                }
                "--uv-swap" => uv_swap = true,
                "-h" | "--help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other if other.starts_with("--") => {
                    return Err(msg(format!("未知选项 {other:?}\n{USAGE}")));
                }
                other => positional.push(other.to_owned()),
            }
        }

        if positional.len() != 2 {
            return Err(msg(format!(
                "需要正好两个位置参数（视频、渲染节点）\n{USAGE}"
            )));
        }

        Ok(Self {
            video: PathBuf::from(&positional[0]),
            node: PathBuf::from(&positional[1]),
            ppm,
            frames,
            matrix,
            readback,
            uv_fourcc,
            uv_swap,
        })
    }
}

// ---------------------------------------------------------------------------
// EGL 手写入口（绕过 khronos-egl 的 i32/usize ABI 问题）
// ---------------------------------------------------------------------------

type EglCreateImageFn =
    extern "system" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type EglDestroyImageFn = extern "system" fn(*mut c_void, *mut c_void) -> u32;
type EglGetErrorFn = extern "system" fn() -> i32;

struct EglRaw {
    create_image: EglCreateImageFn,
    destroy_image: EglDestroyImageFn,
    get_error: EglGetErrorFn,
}

/// `eglGetError()` 常见错误码的可读名字。
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
        0x300F => "EGL_BAD_STATE_KHR",
        _ => "未知错误码",
    }
}

/// 把一个 `EGLImageKHR` 包成 RAII 句柄，离开作用域时销毁。
struct EglImage {
    display: *mut c_void,
    image: *mut c_void,
    destroy: EglDestroyImageFn,
}

impl EglImage {
    fn as_ptr(&self) -> *mut c_void {
        self.image
    }
}

impl Drop for EglImage {
    fn drop(&mut self) {
        if !self.image.is_null() {
            // 返回 EGL_TRUE/EGL_FALSE；这里已经离屏，失败也没法补救，忽略即可。
            let _ = (self.destroy)(self.display, self.image);
        }
    }
}

/// 从 `eglGetProcAddress` 取一个符号并转成具体函数指针类型。
///
/// # Safety
///
/// 调用方必须保证 `T` 与 `name` 指向的 EGL/GL 入口点签名一致。
unsafe fn load_symbol<T: Copy>(instance: &EglInstance, name: &str) -> Res<T> {
    let symbol = instance
        .get_proc_address(name)
        .ok_or_else(|| msg(format!("eglGetProcAddress 没有返回 {name}")))?;
    assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<extern "system" fn()>(),
        "函数指针大小不一致: {name}"
    );
    // SAFETY: 大小已断言，签名一致性由调用方保证（见函数文档）。
    Ok(unsafe { std::mem::transmute_copy::<extern "system" fn(), T>(&symbol) })
}

/// `eglGetProcAddress` 取符号，失败时把可读信息带回来。
macro_rules! gl_load {
    ($instance:expr, $type:ty, $name:literal) => {
        unsafe { load_symbol::<$type>($instance, $name) }
            .map_err(|error| msg(format!("加载 GL 入口点 {} 失败: {error}", $name)))?
    };
}

// ---------------------------------------------------------------------------
// GLES2 手写入口
// ---------------------------------------------------------------------------

type GlCreateShader = extern "system" fn(u32) -> u32;
type GlShaderSource = extern "system" fn(u32, i32, *const *const c_char, *const i32);
type GlCompileShader = extern "system" fn(u32);
type GlGetShaderiv = extern "system" fn(u32, u32, *mut i32);
type GlGetShaderInfoLog = extern "system" fn(u32, i32, *mut i32, *mut c_char);
type GlCreateProgram = extern "system" fn() -> u32;
type GlAttachShader = extern "system" fn(u32, u32);
type GlBindAttribLocation = extern "system" fn(u32, u32, *const c_char);
type GlLinkProgram = extern "system" fn(u32);
type GlGetProgramiv = extern "system" fn(u32, u32, *mut i32);
type GlGetProgramInfoLog = extern "system" fn(u32, i32, *mut i32, *mut c_char);
type GlUseProgram = extern "system" fn(u32);
type GlGetUniformLocation = extern "system" fn(u32, *const c_char) -> i32;
type GlUniform1i = extern "system" fn(i32, i32);
type GlUniform3f = extern "system" fn(i32, f32, f32, f32);
type GlUniformMatrix3fv = extern "system" fn(i32, i32, u8, *const f32);
type GlGenTextures = extern "system" fn(i32, *mut u32);
type GlDeleteTextures = extern "system" fn(i32, *const u32);
type GlBindTexture = extern "system" fn(u32, u32);
type GlActiveTexture = extern "system" fn(u32);
type GlTexParameteri = extern "system" fn(u32, u32, i32);
type GlGenFramebuffers = extern "system" fn(i32, *mut u32);
type GlBindFramebuffer = extern "system" fn(u32, u32);
type GlFramebufferTexture2D = extern "system" fn(u32, u32, u32, u32, i32);
type GlCheckFramebufferStatus = extern "system" fn(u32) -> u32;
type GlViewport = extern "system" fn(i32, i32, i32, i32);
type GlClearColor = extern "system" fn(f32, f32, f32, f32);
type GlClear = extern "system" fn(u32);
type GlDrawArrays = extern "system" fn(u32, i32, i32);
type GlEnableVertexAttribArray = extern "system" fn(u32);
type GlVertexAttribPointer = extern "system" fn(u32, i32, u32, u8, i32, *const c_void);
type GlFinish = extern "system" fn();
type GlGetError = extern "system" fn() -> u32;
type GlGetString = extern "system" fn(u32) -> *const c_char;
type GlEglImageTargetTexture2D = extern "system" fn(u32, *mut c_void);
type GlReadPixels = extern "system" fn(i32, i32, i32, i32, u32, u32, *mut c_void);
type GlPixelStorei = extern "system" fn(u32, i32);

struct Gl {
    create_shader: GlCreateShader,
    shader_source: GlShaderSource,
    compile_shader: GlCompileShader,
    get_shaderiv: GlGetShaderiv,
    get_shader_info_log: GlGetShaderInfoLog,
    create_program: GlCreateProgram,
    attach_shader: GlAttachShader,
    bind_attrib_location: GlBindAttribLocation,
    link_program: GlLinkProgram,
    get_programiv: GlGetProgramiv,
    get_program_info_log: GlGetProgramInfoLog,
    use_program: GlUseProgram,
    get_uniform_location: GlGetUniformLocation,
    uniform_1i: GlUniform1i,
    uniform_3f: GlUniform3f,
    uniform_matrix_3fv: GlUniformMatrix3fv,
    gen_textures: GlGenTextures,
    delete_textures: GlDeleteTextures,
    bind_texture: GlBindTexture,
    active_texture: GlActiveTexture,
    tex_parameteri: GlTexParameteri,
    gen_framebuffers: GlGenFramebuffers,
    bind_framebuffer: GlBindFramebuffer,
    framebuffer_texture_2d: GlFramebufferTexture2D,
    check_framebuffer_status: GlCheckFramebufferStatus,
    viewport: GlViewport,
    clear_color: GlClearColor,
    clear: GlClear,
    draw_arrays: GlDrawArrays,
    enable_vertex_attrib_array: GlEnableVertexAttribArray,
    vertex_attrib_pointer: GlVertexAttribPointer,
    finish: GlFinish,
    get_error: GlGetError,
    get_string: GlGetString,
    egl_image_target_texture_2d: GlEglImageTargetTexture2D,
    read_pixels: GlReadPixels,
    pixel_storei: GlPixelStorei,
}

impl Gl {
    fn load(instance: &EglInstance) -> Res<Self> {
        Ok(Self {
            create_shader: gl_load!(instance, GlCreateShader, "glCreateShader"),
            shader_source: gl_load!(instance, GlShaderSource, "glShaderSource"),
            compile_shader: gl_load!(instance, GlCompileShader, "glCompileShader"),
            get_shaderiv: gl_load!(instance, GlGetShaderiv, "glGetShaderiv"),
            get_shader_info_log: gl_load!(instance, GlGetShaderInfoLog, "glGetShaderInfoLog"),
            create_program: gl_load!(instance, GlCreateProgram, "glCreateProgram"),
            attach_shader: gl_load!(instance, GlAttachShader, "glAttachShader"),
            bind_attrib_location: gl_load!(instance, GlBindAttribLocation, "glBindAttribLocation"),
            link_program: gl_load!(instance, GlLinkProgram, "glLinkProgram"),
            get_programiv: gl_load!(instance, GlGetProgramiv, "glGetProgramiv"),
            get_program_info_log: gl_load!(instance, GlGetProgramInfoLog, "glGetProgramInfoLog"),
            use_program: gl_load!(instance, GlUseProgram, "glUseProgram"),
            get_uniform_location: gl_load!(instance, GlGetUniformLocation, "glGetUniformLocation"),
            uniform_1i: gl_load!(instance, GlUniform1i, "glUniform1i"),
            uniform_3f: gl_load!(instance, GlUniform3f, "glUniform3f"),
            uniform_matrix_3fv: gl_load!(instance, GlUniformMatrix3fv, "glUniformMatrix3fv"),
            gen_textures: gl_load!(instance, GlGenTextures, "glGenTextures"),
            delete_textures: gl_load!(instance, GlDeleteTextures, "glDeleteTextures"),
            bind_texture: gl_load!(instance, GlBindTexture, "glBindTexture"),
            active_texture: gl_load!(instance, GlActiveTexture, "glActiveTexture"),
            tex_parameteri: gl_load!(instance, GlTexParameteri, "glTexParameteri"),
            gen_framebuffers: gl_load!(instance, GlGenFramebuffers, "glGenFramebuffers"),
            bind_framebuffer: gl_load!(instance, GlBindFramebuffer, "glBindFramebuffer"),
            framebuffer_texture_2d: gl_load!(
                instance,
                GlFramebufferTexture2D,
                "glFramebufferTexture2D"
            ),
            check_framebuffer_status: gl_load!(
                instance,
                GlCheckFramebufferStatus,
                "glCheckFramebufferStatus"
            ),
            viewport: gl_load!(instance, GlViewport, "glViewport"),
            clear_color: gl_load!(instance, GlClearColor, "glClearColor"),
            clear: gl_load!(instance, GlClear, "glClear"),
            draw_arrays: gl_load!(instance, GlDrawArrays, "glDrawArrays"),
            enable_vertex_attrib_array: gl_load!(
                instance,
                GlEnableVertexAttribArray,
                "glEnableVertexAttribArray"
            ),
            vertex_attrib_pointer: gl_load!(
                instance,
                GlVertexAttribPointer,
                "glVertexAttribPointer"
            ),
            finish: gl_load!(instance, GlFinish, "glFinish"),
            get_error: gl_load!(instance, GlGetError, "glGetError"),
            get_string: gl_load!(instance, GlGetString, "glGetString"),
            egl_image_target_texture_2d: gl_load!(
                instance,
                GlEglImageTargetTexture2D,
                "glEGLImageTargetTexture2DOES"
            ),
            read_pixels: gl_load!(instance, GlReadPixels, "glReadPixels"),
            pixel_storei: gl_load!(instance, GlPixelStorei, "glPixelStorei"),
        })
    }

    /// 读取并清空 GL 错误队列，返回第一条非 `GL_NO_ERROR` 的可读描述。
    fn take_error(&self, context: &str) -> Option<String> {
        let mut first = None;
        loop {
            // SAFETY: `glGetError` 只是读取并清空当前上下文的错误标志。
            let code = (self.get_error)();
            if code == GL_NO_ERROR {
                return first;
            }
            if first.is_none() {
                first = Some(format!("{context}: GL 错误 0x{code:04X}"));
            }
        }
    }

    fn string(&self, name: u32) -> Option<String> {
        // SAFETY: `name` 是 GL_RENDERER/GL_VERSION/GL_EXTENSIONS 之一。
        let pointer = (self.get_string)(name);
        if pointer.is_null() {
            return None;
        }
        // SAFETY: GL 保证这些查询返回以 NUL 结尾的字符串。
        let text = unsafe { CStr::from_ptr(pointer) }.to_string_lossy();
        Some(text.into_owned())
    }
}

type EglInstance = egl::DynamicInstance<egl::EGL1_5>;

// ---------------------------------------------------------------------------
// 平面描述
// ---------------------------------------------------------------------------

/// 一个待导入的 dmabuf 平面。
struct PlaneDesc<'a> {
    label: &'a str,
    width: u32,
    height: u32,
    fourcc: u32,
    fd: i32,
    offset: u32,
    pitch: u32,
    /// `None` 表示属性表里不写 modifier（老驱动 / 不支持 modifiers 时用）。
    modifier: Option<u64>,
}

impl PlaneDesc<'_> {
    fn describe(&self) -> String {
        let modifier = match self.modifier {
            Some(value) => format!("0x{value:016X}"),
            None => "省略".to_owned(),
        };
        format!(
            "{}: {}x{} fourcc=0x{:08X} fd={} offset={} pitch={} modifier={modifier}",
            self.label, self.width, self.height, self.fourcc, self.fd, self.offset, self.pitch
        )
    }
}

/// NV12 的两个平面拆出来，避免在转换循环里反复算下标。
struct Nv12Planes {
    y_fd: i32,
    y_offset: u32,
    y_pitch: u32,
    uv_fd: i32,
    uv_offset: u32,
    uv_pitch: u32,
}

fn nv12_planes(frame: &DmaBufFrame) -> Res<Nv12Planes> {
    if frame.fourcc != DRM_FORMAT_NV12 {
        fail!("输入不是 NV12：fourcc=0x{:08X}", frame.fourcc);
    }
    let y_object = usize::from(frame.object_index[0]);
    let uv_object = usize::from(frame.object_index[1]);
    let y_handle = frame.fds.get(y_object).ok_or_else(|| {
        msg(format!(
            "Y 平面的 object_index={y_object} 超出 fds 长度 {}",
            frame.fds.len()
        ))
    })?;
    let uv_handle = frame.fds.get(uv_object).ok_or_else(|| {
        msg(format!(
            "UV 平面的 object_index={uv_object} 超出 fds 长度 {}",
            frame.fds.len()
        ))
    })?;
    Ok(Nv12Planes {
        y_fd: y_handle.as_raw_fd(),
        y_offset: frame.offsets[0],
        y_pitch: frame.strides[0],
        uv_fd: uv_handle.as_raw_fd(),
        uv_offset: frame.offsets[1],
        uv_pitch: frame.strides[1],
    })
}

// ---------------------------------------------------------------------------
// 渲染器
// ---------------------------------------------------------------------------

struct Renderer {
    instance: EglInstance,
    display: egl::Display,
    #[allow(dead_code)] // 挂着只为让 pbuffer 活到 `Drop`。
    surface: Option<egl::Surface>,
    context: egl::Context,
    raw: EglRaw,
    gl: Gl,
    program: u32,
    matrix: Matrix,
    uv_fourcc: UvFourcc,
    matrix_location: i32,
    offset_location: i32,
    uv_swap_location: i32,
    uv_swap: i32,
    output_texture: u32,
    /// 输出 dmabuf 绑成的 `EGLImage`，渲染期间必须活着。
    output_image: Option<EglImage>,
    framebuffer: u32,
    output_stride: u32,
    width: u32,
    height: u32,
}

impl Renderer {
    fn new(
        gbm_device: *mut c_void,
        width: u32,
        height: u32,
        matrix: Matrix,
        uv_fourcc: UvFourcc,
        uv_swap: bool,
    ) -> Res<Self> {
        // SAFETY: 加载系统里 glvnd 提供的 libEGL.so.1；`DynamicInstance` 持有它的
        // 句柄，`instance` 活着期间取到的函数指针都有效。
        let instance = unsafe { EglInstance::load_required_from_filename("libEGL.so.1") }
            .map_err(|error| msg(format!("加载 libEGL.so.1 失败: {error}")))?;

        // `get_platform_display` 是 unsafe：调用方要保证 (platform, native_display)
        // 组合合法 —— 这里传的是同一进程里刚建出来的 `gbm_device` 指针。
        let display = unsafe {
            instance.get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, &[egl::ATTRIB_NONE])
        }
        .map_err(|error| {
            msg(format!(
                "eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR) 失败: {error}"
            ))
        })?;

        let (major, minor) = instance
            .initialize(display)
            .map_err(|error| msg(format!("eglInitialize 失败: {error}")))?;
        println!("eglInitialize 成功: EGL {major}.{minor}");
        print_egl_extensions(&instance, display);

        instance
            .bind_api(egl::OPENGL_ES_API)
            .map_err(|error| msg(format!("eglBindAPI(EGL_OPENGL_ES_API) 失败: {error}")))?;

        // 先要带 pbuffer 的 config；拿不到就退化成 surfaceless（SURFACE_TYPE=0）。
        let pbuffer_attribs = [
            egl::RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
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
        let surfaceless_attribs = [
            egl::RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            egl::SURFACE_TYPE,
            0,
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

        let config = match instance.choose_first_config(display, &pbuffer_attribs) {
            Ok(Some(config)) => {
                println!("eglChooseConfig: 拿到带 EGL_PBUFFER_BIT 的 ES2 配置");
                config
            }
            _ => {
                println!("eglChooseConfig(pbuffer) 没有可用配置，改用 SURFACE_TYPE=0 重试");
                match instance.choose_first_config(display, &surfaceless_attribs) {
                    Ok(Some(config)) => config,
                    Ok(None) => fail!("eglChooseConfig 两种属性组合都没有返回配置"),
                    Err(error) => fail!("eglChooseConfig(surfaceless) 失败: {error}"),
                }
            }
        };

        let context = instance
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
            )
            .map_err(|error| msg(format!("eglCreateContext(GLES2) 失败: {error}")))?;

        // 优先 surfaceless；没有 EGL_KHR_surfaceless_context 时退回 pbuffer。
        let mut surface = None;
        match instance.make_current(display, None, None, Some(context)) {
            Ok(()) => println!("eglMakeCurrent: surfaceless（不带 surface）"),
            Err(error) => {
                println!("surfaceless make_current 失败（{error}），改用 pbuffer surface");
                let pbuffer = instance
                    .create_pbuffer_surface(
                        display,
                        config,
                        &[
                            egl::WIDTH,
                            width as egl::Int,
                            egl::HEIGHT,
                            height as egl::Int,
                            egl::NONE,
                        ],
                    )
                    .map_err(|error| msg(format!("eglCreatePbufferSurface 失败: {error}")))?;
                instance
                    .make_current(display, Some(pbuffer), Some(pbuffer), Some(context))
                    .map_err(|error| msg(format!("eglMakeCurrent(pbuffer) 失败: {error}")))?;
                surface = Some(pbuffer);
            }
        }

        let gl = Gl::load(&instance)?;
        if let Some(renderer) = gl.string(GL_RENDERER) {
            println!("GL_RENDERER: {renderer}");
        }
        if let Some(version) = gl.string(GL_VERSION) {
            println!("GL_VERSION: {version}");
        }
        print_gl_extension_summary(&gl);

        let program = compile_program(&gl)?;
        let matrix_location = uniform_location(&gl, program, "u_matrix")?;
        let offset_location = uniform_location(&gl, program, "u_offset")?;
        let uv_swap_location = uniform_location(&gl, program, "u_uv_swap")?;
        println!(
            "UV 平面 fourcc: {}（0x{:08X}），u/v {}",
            uv_fourcc.label(),
            uv_fourcc.fourcc(),
            if uv_swap { "已交换" } else { "不交换" }
        );

        // 手动加载 EGL image 入口（原因见文件头对 khronos-egl ABI 的说明）。
        let create_image =
            unsafe { load_symbol::<EglCreateImageFn>(&instance, "eglCreateImageKHR") }
                .or_else(|_| unsafe {
                    load_symbol::<EglCreateImageFn>(&instance, "eglCreateImage")
                })
                .map_err(|error| {
                    msg(format!(
                        "加载 eglCreateImageKHR/eglCreateImage 失败: {error}"
                    ))
                })?;
        let destroy_image =
            unsafe { load_symbol::<EglDestroyImageFn>(&instance, "eglDestroyImageKHR") }
                .or_else(|_| unsafe {
                    load_symbol::<EglDestroyImageFn>(&instance, "eglDestroyImage")
                })
                .map_err(|error| {
                    msg(format!(
                        "加载 eglDestroyImageKHR/eglDestroyImage 失败: {error}"
                    ))
                })?;
        let get_error = unsafe { load_symbol::<EglGetErrorFn>(&instance, "eglGetError") }
            .map_err(|error| msg(format!("加载 eglGetError 失败: {error}")))?;

        Ok(Self {
            instance,
            display,
            surface,
            context,
            raw: EglRaw {
                create_image,
                destroy_image,
                get_error,
            },
            gl,
            program,
            matrix,
            uv_fourcc,
            matrix_location,
            offset_location,
            uv_swap_location,
            uv_swap: i32::from(uv_swap),
            output_texture: 0,
            output_image: None,
            framebuffer: 0,
            output_stride: 0,
            width,
            height,
        })
    }

    fn apply_matrix(&self) {
        let coefficients = self.matrix.coefficients();
        let offset = self.matrix.offset();
        // 列主序上传，`transpose = GL_FALSE`。
        (self.gl.uniform_matrix_3fv)(self.matrix_location, 1, 0, coefficients.as_ptr());
        (self.gl.uniform_3f)(self.offset_location, offset[0], offset[1], offset[2]);
        // 每帧重申一次 `u_uv_swap`，避免程序重链接后丢状态。
        (self.gl.uniform_1i)(self.uv_swap_location, self.uv_swap);
    }

    /// 把一块已经分配好的输出 dmabuf 建成 FBO 的颜色附件。
    fn attach_output(&mut self, fd: &OwnedFd, stride: u32, offset: u32, modifier: u64) -> Res<()> {
        let plane = PlaneDesc {
            label: "输出 XRGB8888",
            width: self.width,
            height: self.height,
            fourcc: DRM_FORMAT_XRGB8888,
            fd: fd.as_raw_fd(),
            offset,
            pitch: stride,
            modifier: (modifier != 0).then_some(modifier),
        };
        // 输出侧没有「省略 modifier 也可能是对的」这种选项：错了就是花屏，
        // 所以失败直接报错，不重试。
        let image = self.create_image(&plane)?;

        let mut texture = 0u32;
        (self.gl.gen_textures)(1, &mut texture);
        (self.gl.active_texture)(GL_TEXTURE0);
        (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
        (self.gl.egl_image_target_texture_2d)(GL_TEXTURE_2D, image.as_ptr());
        if let Some(error) = self.gl.take_error("glEGLImageTargetTexture2DOES(输出)") {
            return Err(msg(error));
        }
        // 输出纹理只当 FBO 附件，NEAREST 更贴近「原样写出」。
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

        let mut framebuffer = 0u32;
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
            fail!("FBO 不完整: glCheckFramebufferStatus=0x{status:04X}");
        }

        (self.gl.viewport)(0, 0, self.width as i32, self.height as i32);
        (self.gl.clear_color)(0.0, 0.0, 0.0, 1.0);
        (self.gl.clear)(GL_COLOR_BUFFER_BIT);
        if let Some(error) = self.gl.take_error("准备 FBO") {
            return Err(msg(error));
        }

        self.output_texture = texture;
        self.framebuffer = framebuffer;
        self.output_stride = stride;
        // image 要一直活到渲染结束（纹理引用着它的存储）。
        self.output_image = Some(image);
        Ok(())
    }

    /// 导入一个平面为 `EGLImage`；带 modifier 失败时自动去掉 modifier 重试。
    fn create_image(&self, plane: &PlaneDesc<'_>) -> Res<EglImage> {
        match self.try_create_image(plane, plane.modifier) {
            Ok(image) => Ok(image),
            Err(first) => {
                if plane.modifier.is_none() {
                    return Err(first);
                }
                println!("  导入失败（{}）: {first}", plane.describe());
                println!("  去掉 modifier 重试……");
                match self.try_create_image(plane, None) {
                    Ok(image) => {
                        println!("  去掉 modifier 后成功（若缓冲是块状布局，颜色可能错位）");
                        Ok(image)
                    }
                    Err(second) => Err(msg(format!(
                        "{} 两种属性组合都失败：\n    带 modifier: {first}\n    不带 modifier: {second}",
                        plane.describe()
                    ))),
                }
            }
        }
    }

    fn try_create_image(
        &self,
        plane: &PlaneDesc<'_>,
        modifier: Option<u64>,
    ) -> Result<EglImage, Box<dyn std::error::Error>> {
        let mut attribs: Vec<i32> = Vec::with_capacity(16);
        attribs.push(EGL_WIDTH_I32);
        attribs.push(plane.width as i32);
        attribs.push(EGL_HEIGHT_I32);
        attribs.push(plane.height as i32);
        attribs.push(EGL_LINUX_DRM_FOURCC_EXT);
        attribs.push(plane.fourcc as i32);
        attribs.push(EGL_DMA_BUF_PLANE0_FD_EXT);
        attribs.push(plane.fd);
        attribs.push(EGL_DMA_BUF_PLANE0_OFFSET_EXT);
        attribs.push(plane.offset as i32);
        attribs.push(EGL_DMA_BUF_PLANE0_PITCH_EXT);
        attribs.push(plane.pitch as i32);
        if let Some(value) = modifier {
            attribs.push(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT);
            attribs.push(value as u32 as i32);
            attribs.push(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT);
            attribs.push((value >> 32) as u32 as i32);
        }
        attribs.push(EGL_NONE_I32);

        let display = self.display.as_ptr();
        // SAFETY: dma-buf fd 由 `DmaBufFrame` / `gbm::BufferObject` 持有，寿命覆盖
        // 整个调用；属性表是本栈上的 i32 切片，以 EGL_NONE 结尾。
        let image = (self.raw.create_image)(
            display,
            ptr::null_mut(),
            EGL_LINUX_DMA_BUF_EXT,
            ptr::null_mut(),
            attribs.as_ptr(),
        );
        if image.is_null() {
            // SAFETY: `eglGetError` 只读当前线程的错误标志。
            let code = (self.raw.get_error)();
            return Err(msg(format!(
                "eglCreateImageKHR 返回 NULL: eglGetError=0x{code:04X} ({})，属性={attribs:?}",
                egl_error_name(code)
            )));
        }
        Ok(EglImage {
            display,
            image,
            destroy: self.raw.destroy_image,
        })
    }

    /// 把当前 NV12 帧画进输出 FBO。
    fn convert(&self, frame: &DmaBufFrame) -> Res<()> {
        if frame.width != self.width || frame.height != self.height {
            fail!(
                "帧尺寸 {}x{} 与输出 {}x{} 不一致",
                frame.width,
                frame.height,
                self.width,
                self.height
            );
        }
        if frame.width % 2 != 0 || frame.height % 2 != 0 {
            fail!("NV12 宽高必须是偶数，收到 {}x{}", frame.width, frame.height);
        }

        let planes = nv12_planes(frame)?;
        // modifier == 0 就是 LINEAR，此时不写 modifier 属性，少依赖一个扩展。
        let modifier = (frame.modifier != 0).then_some(frame.modifier);
        let y_plane = PlaneDesc {
            label: "平面 0 (Y)",
            width: frame.width,
            height: frame.height,
            fourcc: DRM_FORMAT_R8,
            fd: planes.y_fd,
            offset: planes.y_offset,
            pitch: planes.y_pitch,
            modifier,
        };
        let uv_plane = PlaneDesc {
            label: "平面 1 (UV)",
            width: frame.width / 2,
            height: frame.height / 2,
            fourcc: self.uv_fourcc.fourcc(),
            fd: planes.uv_fd,
            offset: planes.uv_offset,
            pitch: planes.uv_pitch,
            modifier,
        };

        let y_image = self.create_image(&y_plane)?;
        let uv_image = self.create_image(&uv_plane)?;

        let y_texture = self.bind_plane_texture(GL_TEXTURE0, &y_image, "Y")?;
        let uv_texture = self.bind_plane_texture(GL_TEXTURE1, &uv_image, "UV")?;

        (self.gl.use_program)(self.program);
        self.apply_matrix();

        let y_location = uniform_location(&self.gl, self.program, "u_tex_y")?;
        let uv_location = uniform_location(&self.gl, self.program, "u_tex_uv")?;
        (self.gl.uniform_1i)(y_location, 0);
        (self.gl.uniform_1i)(uv_location, 1);

        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, self.framebuffer);
        (self.gl.viewport)(0, 0, self.width as i32, self.height as i32);
        (self.gl.clear)(GL_COLOR_BUFFER_BIT);

        (self.gl.enable_vertex_attrib_array)(0);
        (self.gl.vertex_attrib_pointer)(
            0,
            2,
            GL_FLOAT,
            0,
            0,
            QUAD_POSITIONS.as_ptr().cast::<c_void>(),
        );
        (self.gl.enable_vertex_attrib_array)(1);
        (self.gl.vertex_attrib_pointer)(
            1,
            2,
            GL_FLOAT,
            0,
            0,
            QUAD_TEXCOORDS.as_ptr().cast::<c_void>(),
        );
        (self.gl.draw_arrays)(GL_TRIANGLE_STRIP, 0, 4);

        if let Some(error) = self.gl.take_error("绘制") {
            return Err(msg(error));
        }
        // 渲染必须彻底完成，`mmap` 回读才看得见结果。
        (self.gl.finish)();
        if let Some(error) = self.gl.take_error("glFinish") {
            return Err(msg(error));
        }

        // 先删纹理、再销毁 image（符合 EGL_image 的生命周期要求）。
        (self.gl.delete_textures)(1, &y_texture);
        (self.gl.delete_textures)(1, &uv_texture);
        drop(y_image);
        drop(uv_image);
        Ok(())
    }

    /// 新建一张 2D 纹理并把 `EGLImage` 绑上去。
    fn bind_plane_texture(&self, unit: u32, image: &EglImage, label: &str) -> Res<u32> {
        let mut texture = 0u32;
        (self.gl.gen_textures)(1, &mut texture);
        (self.gl.active_texture)(unit);
        (self.gl.bind_texture)(GL_TEXTURE_2D, texture);
        (self.gl.egl_image_target_texture_2d)(GL_TEXTURE_2D, image.as_ptr());
        if let Some(error) = self
            .gl
            .take_error(&format!("glEGLImageTargetTexture2DOES({label})"))
        {
            return Err(msg(error));
        }
        // 必须显式设 MIN_FILTER：默认的 GL_NEAREST_MIPMAP_LINEAR 对没有 mipmap
        // 的纹理是不完整的，采样结果会是黑色。
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        (self.gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        if let Some(error) = self.gl.take_error(&format!("纹理参数({label})")) {
            return Err(msg(error));
        }
        Ok(texture)
    }

    /// 从 FBO（挂的就是输出 dmabuf）用 `glReadPixels` 回读，返回自上而下的 RGBA 行。
    ///
    /// 输出 dmabuf 是块状布局（NVIDIA `RENDERING` 分配）时，CPU 按行 mmap 读到的
    /// 只是线性化后的乱序数据；GPU 侧这次回读才是这块 dmabuf 的真实内容。
    fn readback_gl(&self) -> Res<Vec<u8>> {
        let width = self.width as usize;
        let height = self.height as usize;
        let row_bytes = width * 4;
        let mut raw = vec![0u8; row_bytes * height];

        (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, self.framebuffer);
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
            return Err(msg(error));
        }

        // GL 的行序是从下往上，翻成自上而下。
        let mut flipped = vec![0u8; raw.len()];
        for row in 0..height {
            let source = &raw[row * row_bytes..(row + 1) * row_bytes];
            let start = (height - 1 - row) * row_bytes;
            flipped[start..start + row_bytes].copy_from_slice(source);
        }
        Ok(flipped)
    }

    /// 把输出 dmabuf `mmap` 回用户态，返回自上而下的 `stride * height` 字节。
    ///
    /// GL 的窗口原点在左下角，所以 FBO 的 row 0 是画面最下面一行；这里翻一次，
    /// 让两条回读路径（CPU mmap / GPU `glReadPixels`）产出同样的行序。
    fn readback(&self, fd: &OwnedFd) -> Res<Vec<u8>> {
        let length = (self.output_stride as usize)
            .checked_mul(self.height as usize)
            .ok_or_else(|| msg("输出缓冲大小溢出"))?;
        // SAFETY: `fd` 是这块 dmabuf 的句柄，长度按 stride*height 计算；只读映射。
        let pointer = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            fail!("mmap 输出 dmabuf 失败: {}", std::io::Error::last_os_error());
        }
        // SAFETY: mmap 成功时返回 length 字节的有效映射。
        let mut data = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) }.to_vec();
        // SAFETY: 解除上面建立的映射。
        let _ = unsafe { libc::munmap(pointer, length) };

        // 上下翻转（见函数文档）。
        let row_bytes = self.output_stride as usize;
        let height = self.height as usize;
        let mut flipped = vec![0u8; data.len()];
        for row in 0..height {
            let source = &data[row * row_bytes..(row + 1) * row_bytes];
            let start = (height - 1 - row) * row_bytes;
            flipped[start..start + row_bytes].copy_from_slice(source);
        }
        std::mem::swap(&mut data, &mut flipped);
        Ok(data)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // best effort：释放纹理、解绑输出 image，再释放 current/context/display。
        if self.output_texture != 0 {
            (self.gl.delete_textures)(1, &self.output_texture);
        }
        self.output_image = None;
        let _ = self.instance.make_current(self.display, None, None, None);
        let _ = self.instance.destroy_context(self.display, self.context);
        let _ = self.instance.terminate(self.display);
    }
}

// ---------------------------------------------------------------------------
// EGL / GL 信息输出
// ---------------------------------------------------------------------------

/// 打印与 dma-buf 导入相关的 EGL 扩展（任务要求的关键证据）。
fn print_egl_extensions(instance: &EglInstance, display: egl::Display) {
    let client = instance
        .query_string(None, egl::EXTENSIONS)
        .ok()
        .map(|text| text.to_string_lossy().into_owned());
    if let Some(client) = &client {
        let relevant: Vec<&str> = client
            .split_whitespace()
            .filter(|name| name.contains("dma_buf") || name.contains("platform_gbm"))
            .collect();
        println!("EGL client 扩展（dma_buf/platform_gbm 相关）: {relevant:?}");
    }

    let Ok(extensions) = instance
        .query_string(Some(display), egl::EXTENSIONS)
        .map(|text| text.to_string_lossy().into_owned())
    else {
        println!("eglQueryString(display, EGL_EXTENSIONS) 返回空");
        return;
    };

    let names: Vec<&str> = extensions.split_whitespace().collect();
    let relevant: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| {
            name.contains("dma_buf")
                || name.contains("image")
                || name.contains("surfaceless")
                || name.contains("platform")
        })
        .collect();
    println!("EGL display 扩展共 {} 项", names.len());
    println!("EGL 扩展（dma_buf/image/surfaceless/platform 相关）: {relevant:?}");

    for required in [
        "EGL_EXT_image_dma_buf_import",
        "EGL_EXT_image_dma_buf_import_modifiers",
        "EGL_KHR_surfaceless_context",
    ] {
        println!(
            "  {required}: {}",
            if names.contains(&required) {
                "有"
            } else {
                "没有"
            }
        );
    }
}

fn print_gl_extension_summary(gl: &Gl) {
    let Some(extensions) = gl.string(GL_EXTENSIONS) else {
        println!("glGetString(GL_EXTENSIONS) 返回空（可能是 GLES3 上下文，改用 glGetStringi）");
        return;
    };
    let relevant: Vec<&str> = extensions
        .split_whitespace()
        .filter(|name| name.contains("EGL_image") || name.contains("dma_buf"))
        .collect();
    if relevant.is_empty() {
        println!(
            "GL_EXTENSIONS: 没有与 EGL_image 相关的条目（glEGLImageTargetTexture2DOES 可能不可用）"
        );
    } else {
        println!("GL_EXTENSIONS(EGL_image 相关): {relevant:?}");
    }
}

// ---------------------------------------------------------------------------
// 着色器与 GL 小工具
// ---------------------------------------------------------------------------

fn uniform_location(gl: &Gl, program: u32, name: &str) -> Res<i32> {
    let text = CString::new(name).map_err(|error| msg(format!("uniform 名字含 NUL: {error}")))?;
    let location = (gl.get_uniform_location)(program, text.as_ptr());
    if location < 0 {
        fail!("找不到 uniform {name}（可能被编译器优化掉了）");
    }
    Ok(location)
}

/// 把（NUL 结尾的）字节缓冲转成可打印字符串。
struct LogText<'a>(&'a [u8]);

impl std::fmt::Display for LogText<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let end = self
            .0
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.0.len());
        formatter.write_str(&String::from_utf8_lossy(&self.0[..end]))
    }
}

fn shader_log(gl: &Gl, shader: u32) -> String {
    let mut length = 0i32;
    (gl.get_shaderiv)(shader, GL_INFO_LOG_LENGTH, &mut length);
    let capacity = usize::try_from(length.max(1)).unwrap_or(1024);
    let mut buffer = vec![0u8; capacity];
    let mut written = 0i32;
    (gl.get_shader_info_log)(
        shader,
        buffer.len() as i32,
        &mut written,
        buffer.as_mut_ptr().cast::<c_char>(),
    );
    LogText(&buffer).to_string()
}

fn program_log(gl: &Gl, program: u32) -> String {
    let mut length = 0i32;
    (gl.get_programiv)(program, GL_INFO_LOG_LENGTH, &mut length);
    let capacity = usize::try_from(length.max(1)).unwrap_or(1024);
    let mut buffer = vec![0u8; capacity];
    let mut written = 0i32;
    (gl.get_program_info_log)(
        program,
        buffer.len() as i32,
        &mut written,
        buffer.as_mut_ptr().cast::<c_char>(),
    );
    LogText(&buffer).to_string()
}

fn compile_shader(gl: &Gl, kind: u32, source: &str, label: &str) -> Res<u32> {
    let shader = (gl.create_shader)(kind);
    if shader == 0 {
        fail!("glCreateShader({label}) 返回 0");
    }
    let text = CString::new(source).map_err(|error| msg(format!("着色器含 NUL: {error}")))?;
    let pointer = text.as_ptr();
    let length =
        i32::try_from(source.len()).map_err(|error| msg(format!("着色器太长: {error}")))?;
    (gl.shader_source)(shader, 1, &pointer, &length);
    (gl.compile_shader)(shader);

    let mut status = 0i32;
    (gl.get_shaderiv)(shader, GL_COMPILE_STATUS, &mut status);
    if status == 0 {
        fail!("{label}着色器编译失败: {}", shader_log(gl, shader));
    }
    Ok(shader)
}

fn compile_program(gl: &Gl) -> Res<u32> {
    let vertex = compile_shader(gl, GL_VERTEX_SHADER, VERTEX_SHADER, "顶点")?;
    let fragment = compile_shader(gl, GL_FRAGMENT_SHADER, FRAGMENT_SHADER, "片元")?;

    let program = (gl.create_program)();
    if program == 0 {
        fail!("glCreateProgram 返回 0");
    }
    (gl.attach_shader)(program, vertex);
    (gl.attach_shader)(program, fragment);
    // 固定 attribute 位置，省掉 glGetAttribLocation。
    (gl.bind_attrib_location)(program, 0, c"a_position".as_ptr());
    (gl.bind_attrib_location)(program, 1, c"a_texcoord".as_ptr());
    (gl.link_program)(program);

    let mut status = 0i32;
    (gl.get_programiv)(program, GL_LINK_STATUS, &mut status);
    if status == 0 {
        fail!("program 链接失败: {}", program_log(gl, program));
    }
    Ok(program)
}

// ---------------------------------------------------------------------------
// PPM 输出
// ---------------------------------------------------------------------------

/// XRGB8888（内存字节序 `[B, G, R, X]`）→ 紧凑 RGB24，行序不变。
fn bgrx_to_rgb24(data: &[u8], width: u32, height: u32, stride: u32) -> Res<Vec<u8>> {
    let stride_usize = stride as usize;
    let width_usize = width as usize;
    let expected = stride_usize
        .checked_mul(height as usize)
        .ok_or_else(|| msg("回读尺寸溢出"))?;
    if data.len() < expected {
        fail!(
            "回读缓冲只有 {} 字节，少于 stride*height={expected}",
            data.len()
        );
    }

    let mut pixels = Vec::with_capacity(width_usize * height as usize * 3);
    for row in 0..height as usize {
        let start = row * stride_usize;
        let line = &data[start..start + width_usize * 4];
        for pixel in line.chunks_exact(4) {
            pixels.push(pixel[2]);
            pixels.push(pixel[1]);
            pixels.push(pixel[0]);
        }
    }
    Ok(pixels)
}

/// RGBA（已翻转成自上而下）→ 紧凑 RGB24。
fn rgba_to_rgb24(data: &[u8], width: u32, height: u32) -> Res<Vec<u8>> {
    let expected = width as usize * height as usize * 4;
    if data.len() < expected {
        fail!("RGBA 回读只有 {} 字节，少于 {expected}", data.len());
    }
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
    for pixel in data[..expected].chunks_exact(4) {
        pixels.push(pixel[0]);
        pixels.push(pixel[1]);
        pixels.push(pixel[2]);
    }
    Ok(pixels)
}

/// 把紧凑 RGB24 写成 P6 PPM，返回 `(平均 R, 平均 G, 平均 B)`。
fn write_ppm(path: &Path, pixels: &[u8], width: u32, height: u32) -> Res<(f64, f64, f64)> {
    let expected = width as usize * height as usize * 3;
    if pixels.len() < expected {
        fail!("PPM 像素只有 {} 字节，少于 {expected}", pixels.len());
    }
    let mut file = Vec::with_capacity(expected + 32);
    file.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    file.extend_from_slice(&pixels[..expected]);
    std::fs::write(path, &file)
        .map_err(|error| msg(format!("写 {} 失败: {error}", path.display())))?;
    Ok(mean_rgb(pixels, width, height))
}

/// 全幅平均 RGB（紧凑 RGB24）。
fn mean_rgb(pixels: &[u8], width: u32, height: u32) -> (f64, f64, f64) {
    let mut sums = [0u64; 3];
    for pixel in pixels.chunks_exact(3) {
        sums[0] += u64::from(pixel[0]);
        sums[1] += u64::from(pixel[1]);
        sums[2] += u64::from(pixel[2]);
    }
    let count = f64::from(width) * f64::from(height);
    #[allow(clippy::cast_precision_loss)]
    let mean = |sum: u64| sum as f64 / count;
    (mean(sums[0]), mean(sums[1]), mean(sums[2]))
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// gbm 不导出 ffi 类型，只能通过 `AsRaw` 把设备指针取成 `usize` 再转 `*mut c_void`。
fn gbm_device_ptr<T: AsFd>(device: &gbm::Device<T>) -> *mut c_void {
    fn raw_as_usize<D, U>(object: &D) -> usize
    where
        D: AsRaw<U>,
    {
        object.as_raw() as usize
    }
    raw_as_usize(device) as *mut c_void
}

/// 分配一块输出用的 `XRGB8888` buffer object。
///
/// NVIDIA 的 gbm 分配器对 `usage` 位组合比较挑剔（实测 `RENDERING | LINEAR`
/// 会返回 `EINVAL`），所以这里按「最想要 → 最宽松」的顺序逐个试，并把每次
/// 的结果打印出来 —— 就算全失败，日志本身就是有价值的证据。
fn allocate_output(
    device: &gbm::Device<std::fs::File>,
    width: u32,
    height: u32,
) -> Res<gbm::BufferObject<()>> {
    use gbm::BufferObjectFlags as Flags;

    // 实测（NVIDIA 620.70 + Mesa 26.2.7 的 gbm nvidia 后端）：只要带
    // `GBM_BO_USE_LINEAR` 就一定失败（`gbm_device_is_format_supported` 也返回 0，
    // errno=EINVAL）；不带 LINEAR 的 `GBM_BO_USE_RENDERING` 反而正好拿到
    // `modifier=0`（也就是 LINEAR）。所以顺序是「先按规格要 LINEAR，失败就退到
    // RENDERING」。
    let attempts: [(&str, Flags); 6] = [
        ("RENDERING|LINEAR", Flags::RENDERING | Flags::LINEAR),
        ("RENDERING", Flags::RENDERING),
        ("RENDERING|WRITE", Flags::RENDERING | Flags::WRITE),
        (
            "SCANOUT|RENDERING|WRITE",
            Flags::SCANOUT | Flags::RENDERING | Flags::WRITE,
        ),
        ("WRITE|LINEAR", Flags::WRITE | Flags::LINEAR),
        ("0（不指定用途）", Flags::empty()),
    ];
    for (label, flags) in attempts {
        let supported = device.is_format_supported(gbm::Format::Xrgb8888, flags);
        match device.create_buffer_object::<()>(width, height, gbm::Format::Xrgb8888, flags) {
            Ok(object) => {
                println!("gbm 分配成功: flags={label}（is_format_supported={supported}）");
                return Ok(object);
            }
            Err(error) => {
                println!("gbm 分配失败: flags={label}（is_format_supported={supported}）: {error}");
            }
        }
    }

    // 再试显式声明 LINEAR modifier 的版本。
    let linear_attempts: [(&str, Flags); 2] = [
        (
            "modifiers=[LINEAR] + RENDERING|WRITE",
            Flags::RENDERING | Flags::WRITE,
        ),
        ("modifiers=[LINEAR] + 0", Flags::empty()),
    ];
    for (label, flags) in linear_attempts {
        match device.create_buffer_object_with_modifiers2::<()>(
            width,
            height,
            gbm::Format::Xrgb8888,
            [gbm::Modifier::Linear].into_iter(),
            flags,
        ) {
            Ok(object) => {
                println!("gbm 分配成功: {label}");
                return Ok(object);
            }
            Err(error) => println!("gbm 分配失败: {label}: {error}"),
        }
    }

    fail!("gbm 无法分配 {width}x{height} XRGB8888 buffer，所有 flags/modifier 组合都失败");
}

fn run() -> Res<()> {
    let args = Args::parse()?;
    println!("输入视频: {}", args.video.display());
    println!("渲染节点: {}", args.node.display());
    println!("色彩矩阵: {}", args.matrix.label());

    // ---- 1. 解码并导出第一帧 NV12 ----
    let mut decoder = VaapiDecoder::open(&args.video, &args.node)?;
    println!("流信息: {:?}", decoder.stream_info());
    let mut surface = decoder
        .next_surface()?
        .ok_or_else(|| msg("解码器没有解出第一帧"))?;
    println!(
        "第一帧: {}x{} pts={:?}（{:.3} 秒）",
        surface.width,
        surface.height,
        surface.pts,
        surface.pts.as_secs_f64()
    );

    let mut frame =
        export::export_surface(decoder.display(), surface.surface_id())?.into_frame(None)?;
    println!(
        "NV12 dmabuf: {}x{} fourcc=0x{:08X} stride={} fds={} object_index={:?} strides={:?} offsets={:?} modifier=0x{:016X}",
        frame.width,
        frame.height,
        frame.fourcc,
        frame.stride,
        frame.fds.len(),
        frame.object_index,
        frame.strides,
        frame.offsets,
        frame.modifier
    );

    // ---- 2. gbm 设备 + EGL ----
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.node)
        .map_err(|error| {
            msg(format!(
                "打开渲染节点 {} 失败: {error}",
                args.node.display()
            ))
        })?;
    let gbm_device = gbm::Device::new(file).map_err(|error| {
        msg(format!(
            "gbm::Device::new({}) 失败: {error}",
            args.node.display()
        ))
    })?;
    let gbm_ptr = gbm_device_ptr(&gbm_device);

    let width = surface.width;
    let height = surface.height;
    let mut renderer = Renderer::new(
        gbm_ptr,
        width,
        height,
        args.matrix,
        args.uv_fourcc,
        args.uv_swap,
    )?;

    // ---- 3. 分配输出 XRGB8888 BO 并挂到 FBO ----
    println!("gbm 后端: {}", gbm_device.backend_name());
    let output = allocate_output(&gbm_device, width, height)?;
    let output_fd = output
        .fd()
        .map_err(|error| msg(format!("gbm_bo_get_fd 失败: {error}")))?;
    let output_stride = output.stride();
    let output_offset = output.offset(0);
    let output_modifier = u64::from(output.modifier());
    let output_format = output.format();

    println!(
        "输出 dmabuf: fourcc=0x{:08X} ({:?}) stride={} offset={} modifier=0x{output_modifier:016X} fd={}",
        DRM_FORMAT_XRGB8888,
        output_format,
        output_stride,
        output_offset,
        output_fd.as_raw_fd()
    );
    if output_modifier != 0 {
        println!(
            "注意: 输出缓冲不是 LINEAR（modifier=0x{output_modifier:016X}），下面按行读 PPM 的结果可能错位"
        );
    }

    renderer.attach_output(&output_fd, output_stride, output_offset, output_modifier)?;

    // ---- 4. 逐帧转换 ----
    let mut converted = 0usize;
    for index in 0..args.frames {
        if index > 0 {
            surface = match decoder.next_surface()? {
                Some(next) => next,
                None => {
                    println!("解码流在第 {index} 帧结束");
                    break;
                }
            };
            frame = export::export_surface(decoder.display(), surface.surface_id())?
                .into_frame(None)?;
        }
        println!(
            "转换第 {} 帧: pts={:?}（{:.3} 秒）",
            index + 1,
            surface.pts,
            surface.pts.as_secs_f64()
        );
        renderer.convert(&frame)?;
        converted += 1;
    }
    println!("共转换 {converted} 帧");

    // ---- 5. 回读 + PPM ----
    let readback = match args.readback {
        Readback::Auto if output_modifier == 0 => Readback::Cpu,
        Readback::Auto => Readback::Gl,
        other => other,
    };
    let pixels = match readback {
        Readback::Cpu => {
            println!("回读方式: mmap dmabuf（CPU 侧，按行读，要求 LINEAR 布局）");
            let data = renderer.readback(&output_fd)?;
            bgrx_to_rgb24(&data, width, height, output_stride)?
        }
        Readback::Gl => {
            println!(
                "回读方式: glReadPixels（GPU 侧读 FBO，输出 modifier=0x{output_modifier:016X}）"
            );
            let data = renderer.readback_gl()?;
            rgba_to_rgb24(&data, width, height)?
        }
        Readback::Auto => return Err(msg("回读方式未解析")),
    };
    let (red, green, blue) = match &args.ppm {
        Some(path) => {
            let means = write_ppm(path, &pixels, width, height)?;
            println!("已写出 PPM: {}（{width}x{height}）", path.display());
            means
        }
        None => mean_rgb(&pixels, width, height),
    };

    println!("PPM 全幅平均 RGB: R={red:.2} G={green:.2} B={blue:.2}");
    if red > 0.0 && green > 0.0 && blue > 0.0 {
        println!("三个通道都非零 —— 不再是「只有亮度、整屏偏红」的样子");
    } else {
        println!("警告: 有通道均值为 0，颜色链路可能没接通");
    }
    println!(
        "对照命令: ffmpeg -ss <pts 对应秒数> -i <视频> -frames:v 1 -vf scale={width}:{height} -f rawvideo -pix_fmt rgb24 -"
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("失败: {error}");
        std::process::exit(1);
    }
}
