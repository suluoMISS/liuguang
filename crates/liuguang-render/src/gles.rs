//! GLES 渲染路径（feature `gles`，非默认）：GBM + EGL + GLSL ES 1.00 shader。
//!
//! # ⚠️ 未验证路径：隔离保留，不参与任何验收
//!
//! **状态（2026-10-18 复核）**：本模块仅编译级交付，尚未在任何真机/模拟器上验证；
//! EGL/GBM 上下文创建、shader 编译、FBO 渲染与 `glReadPixels` 回读的运行时行为
//! 均未经实测（阻塞项 B1）。**默认构建不编译本模块，运行时零成本；没有任何生产
//! 配置会启用它**（`apps/liuguang` 不引用 `gles`）。
//!
//! **保留而非删除的理由**：它是规范 §7.2「过渡动效用 GPU shader」的唯一参考实现；
//! 非默认 feature 隔离使维护成本为零；`scripts/ci.sh` 会以 `--features gles`
//! 编译+单测它（删除需同步改动该脚本）。
//!
//! **启用前必须先完成**：真机（DP-1 3840x2400）上验证 EGL surfaceless/GBM 初始化、
//! shader 编译、FBO 渲染与 `glReadPixels` 回读的正确性与耗时，并与 CPU 路径
//! （4K fade 20.2 ms/帧）对比后写入实测报告。在那之前**不得**在任何文档/状态里
//! 声称"GPU 转场可用"。
//!
//! CPU 正确性路径见 [`crate::transition`]。
//!
//! # 设计（规范 §7.2）
//!
//! - GPU shader 优先（M4）：通过 GBM device fd 创建 EGL 上下文
//!   （GBM platform display + pbuffer surface），上传 `from`/`to` 两张纹理；
//! - 编译 GLSL ES 1.00 着色器实现 fade / slide / wipe / circle 四种效果，
//!   uniform 接口为 `u_progress`（进度）、`u_direction`（方向）、
//!   `u_effect`（效果选择）+ 辅助 `u_size`（像素尺寸，circle 用）；
//! - 渲染到 FBO 后 `glReadPixels` 回 [`CpuFrame`]（真机优化走 GBM dumb
//!   buffer 属后续工作）；
//! - 任一 GL/EGL 调用失败映射 [`RenderError::Failed`] 并带上下文信息；
//!   上层捕获失败后应**立即切换**（回退 [`crate::NoTransition`]）；
//! - [`GlesTransition::release`] 释放纹理/FBO/着色器/上下文等中间资源。
//!
//! # unsafe 约束
//!
//! workspace 对 `unsafe_code` 为 `deny`；本模块是唯一允许 `unsafe` 的位置
//! （`#![allow(unsafe_code)]` 局部覆盖）。所有 `unsafe` 块的 safety invariant：
//!
//! 1. EGL/GL 函数指针仅通过 `eglGetProcAddress` 解析并立即检查非空，
//!    指针签名与调用约定（`extern "system"`）和 Khronos 规范一致；
//! 2. GL 对象 id（纹理/FBO/shader/program）仅在本模块创建的当前 EGL
//!    上下文中使用；上下文在 `release` 前保持 current；
//! 3. 传给 `glTexImage2D` / `glReadPixels` 的缓冲长度按 `width/height`
//!    计算并严格校验，生命周期覆盖整个调用；
//! 4. GBM device fd 由调用方保证在 [`GlesTransition`] 生命周期内有效
//!    （`BorrowedFd` 借用约束），本模块不关闭该 fd。

// safety invariant 见模块文档；workspace 的 deny 在本模块内局部降级。
#![allow(unsafe_code)]

use crate::RenderError;
use crate::transition::{EffectKind, TransitionSpec, apply_easing};
use khronos_egl as egl;
use liuguang_core::config::TransitionDirection;
use liuguang_core::types::{CpuFrame, PixelFormat};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// 着色器源码（GLSL ES 1.00，纯数据，可脱离硬件单测）
// ---------------------------------------------------------------------------

/// 顶点着色器：全屏四边形 + 翻转 V 的纹理坐标。
///
/// V 翻转（`tex.y = (1 - clip.y) / 2`）使纹理坐标与 CPU 光栅化一致
/// （y=0 为画面顶部），`glReadPixels` 读回后再逐行翻转回 top-down。
pub const VERTEX_SHADER: &str = "attribute vec2 a_position;\n\
attribute vec2 a_texcoord;\n\
varying vec2 v_texcoord;\n\
void main() {\n\
    v_texcoord = a_texcoord;\n\
    gl_Position = vec4(a_position, 0.0, 1.0);\n\
}\n";

/// fade 片元着色器：逐通道线性混合（`u_effect == 0`）。
pub const FRAG_FADE: &str = "precision mediump float;\n\
uniform sampler2D u_from;\n\
uniform sampler2D u_to;\n\
uniform float u_progress;\n\
uniform vec2 u_direction;\n\
uniform vec2 u_size;\n\
uniform int u_effect;\n\
varying vec2 v_texcoord;\n\
void main() {\n\
    if (u_effect != 0) {\n\
        gl_FragColor = texture2D(u_to, v_texcoord);\n\
        return;\n\
    }\n\
    gl_FragColor = mix(texture2D(u_from, v_texcoord), texture2D(u_to, v_texcoord), u_progress);\n\
}\n";

/// slide 片元着色器：`to` 沿 `u_direction` 平移 `(1 - u_progress)`（`u_effect == 1`）。
pub const FRAG_SLIDE: &str = "precision mediump float;\n\
uniform sampler2D u_from;\n\
uniform sampler2D u_to;\n\
uniform float u_progress;\n\
uniform vec2 u_direction;\n\
uniform vec2 u_size;\n\
uniform int u_effect;\n\
varying vec2 v_texcoord;\n\
void main() {\n\
    if (u_effect != 1) {\n\
        gl_FragColor = texture2D(u_to, v_texcoord);\n\
        return;\n\
    }\n\
    vec2 src = v_texcoord - (1.0 - u_progress) * u_direction;\n\
    if (src.x >= 0.0 && src.x < 1.0 && src.y >= 0.0 && src.y < 1.0) {\n\
        gl_FragColor = texture2D(u_to, src);\n\
    } else {\n\
        gl_FragColor = texture2D(u_from, v_texcoord);\n\
    }\n\
}\n";

/// wipe 片元着色器：`to` 从 `u_direction` 命名边缘硬边推进（`u_effect == 2`）。
pub const FRAG_WIPE: &str = "precision mediump float;\n\
uniform sampler2D u_from;\n\
uniform sampler2D u_to;\n\
uniform float u_progress;\n\
uniform vec2 u_direction;\n\
uniform vec2 u_size;\n\
uniform int u_effect;\n\
varying vec2 v_texcoord;\n\
void main() {\n\
    if (u_effect != 2) {\n\
        gl_FragColor = texture2D(u_to, v_texcoord);\n\
        return;\n\
    }\n\
    bool on_to;\n\
    if (u_direction.x < 0.0) {\n\
        on_to = v_texcoord.x < u_progress;\n\
    } else if (u_direction.x > 0.0) {\n\
        on_to = v_texcoord.x > 1.0 - u_progress;\n\
    } else if (u_direction.y < 0.0) {\n\
        on_to = v_texcoord.y < u_progress;\n\
    } else {\n\
        on_to = v_texcoord.y > 1.0 - u_progress;\n\
    }\n\
    gl_FragColor = on_to ? texture2D(u_to, v_texcoord) : texture2D(u_from, v_texcoord);\n\
}\n";

/// circle 片元着色器：圆心为画面中心，半径 `u_progress * max_radius`（`u_effect == 3`）。
pub const FRAG_CIRCLE: &str = "precision mediump float;\n\
uniform sampler2D u_from;\n\
uniform sampler2D u_to;\n\
uniform float u_progress;\n\
uniform vec2 u_direction;\n\
uniform vec2 u_size;\n\
uniform int u_effect;\n\
varying vec2 v_texcoord;\n\
void main() {\n\
    if (u_effect != 3) {\n\
        gl_FragColor = texture2D(u_to, v_texcoord);\n\
        return;\n\
    }\n\
    vec2 d = (v_texcoord - vec2(0.5)) * u_size;\n\
    float max_radius = 0.5 * length(u_size);\n\
    if (length(d) < u_progress * max_radius) {\n\
        gl_FragColor = texture2D(u_to, v_texcoord);\n\
    } else {\n\
        gl_FragColor = texture2D(u_from, v_texcoord);\n\
    }\n\
}\n";

/// 返回指定效果的（顶点源码，片元源码）。
///
/// 纯函数、无 GL 调用：片元源码含 `main` 与 uniform 声明，顶点源码含
/// `attribute` 声明，可脱离硬件单测（结构断言见测试模块）。
/// `u_effect` 为效果选择（0=fade、1=slide、2=wipe、3=circle）；
/// 与编译 kind 不符时片元着色器回退为直接采样 `u_to`（防御性处理）。
pub fn shader_sources(kind: EffectKind) -> (&'static str, &'static str) {
    let fragment = match kind {
        EffectKind::Fade => FRAG_FADE,
        EffectKind::Slide { .. } => FRAG_SLIDE,
        EffectKind::Wipe { .. } => FRAG_WIPE,
        EffectKind::Circle => FRAG_CIRCLE,
    };
    (VERTEX_SHADER, fragment)
}

/// 效果编码（uniform `u_effect` 的取值）。
fn effect_code(kind: EffectKind) -> i32 {
    match kind {
        EffectKind::Fade => 0,
        EffectKind::Slide { .. } => 1,
        EffectKind::Wipe { .. } => 2,
        EffectKind::Circle => 3,
    }
}

/// `to` 的平移方向向量（uniform `u_direction`，slide/wipe 使用）。
///
/// 与 [`crate::transition`] 的 CPU 语义一致：`Left/Up` 为负轴方向。
fn direction_vec(direction: Option<TransitionDirection>) -> [f32; 2] {
    match direction {
        Some(TransitionDirection::Left) => [-1.0, 0.0],
        Some(TransitionDirection::Right) => [1.0, 0.0],
        Some(TransitionDirection::Up) => [0.0, -1.0],
        Some(TransitionDirection::Down) => [0.0, 1.0],
        None => [0.0, 0.0],
    }
}

/// 从 [`EffectKind`] 提取方向（fade/circle 无方向）。
fn kind_direction(kind: EffectKind) -> Option<TransitionDirection> {
    match kind {
        EffectKind::Slide { direction } | EffectKind::Wipe { direction } => Some(direction),
        EffectKind::Fade | EffectKind::Circle => None,
    }
}

// ---------------------------------------------------------------------------
// EGL / GL 常量与函数指针（签名对齐 Khronos 规范，见模块文档 invariant 1）
// ---------------------------------------------------------------------------

/// `EGL_PLATFORM_GBM_KHR`（EGL_EXT_platform_gbm / EGL 1.5）。
const PLATFORM_GBM_KHR: u32 = 0x31D7;
/// `EGL_OPENGL_ES2_BIT`。
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;

type GLenum = u32;
type GLuint = u32;
type GLint = i32;
type GLsizei = i32;
type GLfloat = f32;
type GLboolean = u8;
type GLchar = std::os::raw::c_char;

const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
const GL_VERTEX_SHADER: GLenum = 0x8B31;
const GL_COMPILE_STATUS: GLenum = 0x8B81;
const GL_LINK_STATUS: GLenum = 0x8B82;
const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_TEXTURE0: GLenum = 0x84C0;
const GL_TEXTURE1: GLenum = 0x84C1;
const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
const GL_NEAREST: GLint = 0x2600;
const GL_CLAMP_TO_EDGE: GLint = 0x812F;
const GL_RGBA: GLenum = 0x1908;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_FLOAT: GLenum = 0x1406;
const GL_FRAMEBUFFER: GLenum = 0x8D40;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
const GL_TRIANGLE_STRIP: GLenum = 0x0005;

/// 全屏四边形顶点（triangle strip，clip 坐标）。
const QUAD_POS: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
/// 与 [`VERTEX_SHADER`] 配套的纹理坐标（V 已翻转）。
const QUAD_UV: [f32; 8] = [0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0];

/// GLES 2.0 入口（全部经 `eglGetProcAddress` 解析）。
#[allow(missing_docs)]
struct GlFns {
    create_shader: extern "system" fn(GLenum) -> GLuint,
    shader_source: extern "system" fn(GLuint, GLsizei, *const *const GLchar, *const GLint),
    compile_shader: extern "system" fn(GLuint),
    get_shaderiv: extern "system" fn(GLuint, GLenum, *mut GLint),
    get_shader_info_log: extern "system" fn(GLuint, GLsizei, *mut GLsizei, *mut GLchar),
    create_program: extern "system" fn() -> GLuint,
    attach_shader: extern "system" fn(GLuint, GLuint),
    link_program: extern "system" fn(GLuint),
    get_programiv: extern "system" fn(GLuint, GLenum, *mut GLint),
    get_program_info_log: extern "system" fn(GLuint, GLsizei, *mut GLsizei, *mut GLchar),
    use_program: extern "system" fn(GLuint),
    get_uniform_location: extern "system" fn(GLuint, *const GLchar) -> GLint,
    uniform_1i: extern "system" fn(GLint, GLint),
    uniform_1f: extern "system" fn(GLint, GLfloat),
    uniform_2f: extern "system" fn(GLint, GLfloat, GLfloat),
    delete_shader: extern "system" fn(GLuint),
    delete_program: extern "system" fn(GLuint),
    gen_textures: extern "system" fn(GLsizei, *mut GLuint),
    bind_texture: extern "system" fn(GLenum, GLuint),
    tex_image_2d: extern "system" fn(
        GLenum,
        GLint,
        GLint,
        GLsizei,
        GLsizei,
        GLint,
        GLenum,
        GLenum,
        *const std::ffi::c_void,
    ),
    tex_parameteri: extern "system" fn(GLenum, GLenum, GLint),
    active_texture: extern "system" fn(GLenum),
    delete_textures: extern "system" fn(GLsizei, *const GLuint),
    gen_framebuffers: extern "system" fn(GLsizei, *mut GLuint),
    bind_framebuffer: extern "system" fn(GLenum, GLuint),
    check_framebuffer_status: extern "system" fn(GLenum) -> GLenum,
    framebuffer_texture_2d: extern "system" fn(GLenum, GLenum, GLenum, GLuint, GLint),
    delete_framebuffers: extern "system" fn(GLsizei, *const GLuint),
    viewport: extern "system" fn(GLint, GLint, GLsizei, GLsizei),
    draw_arrays: extern "system" fn(GLenum, GLint, GLsizei),
    enable_vertex_attrib_array: extern "system" fn(GLuint),
    vertex_attrib_pointer:
        extern "system" fn(GLuint, GLint, GLenum, GLboolean, GLsizei, *const std::ffi::c_void),
    read_pixels:
        extern "system" fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *mut std::ffi::c_void),
    get_error: extern "system" fn() -> GLenum,
}

/// EGL 实例别名（动态加载 libEGL，要求 EGL 1.5 以使用 platform display）。
type EglInstance = egl::DynamicInstance<egl::EGL1_5>;

/// 解析单个 GL 入口并转为具体签名。
///
/// # Safety
///
/// `T` 必须与 `name` 对应的 Khronos GL 入口签名一致（模块文档 invariant 1）。
unsafe fn load_sym<T: Copy>(egl: &EglInstance, name: &str) -> Result<T, RenderError> {
    let sym = egl
        .get_proc_address(name)
        .ok_or_else(|| RenderError::Failed(format!("eglGetProcAddress 未返回 {name}")))?;
    // SAFETY：由调用方保证 T 与 name 的签名一致；fn 指针等宽，转位安全。
    Ok(unsafe { std::mem::transmute_copy::<extern "system" fn(), T>(&sym) })
}

/// 加载全部 GLES 2.0 入口（任一缺失即失败，带上下文信息）。
fn load_gl(egl: &EglInstance) -> Result<GlFns, RenderError> {
    macro_rules! load {
        ($name:literal, $ty:ty) => {
            unsafe { load_sym::<$ty>(egl, $name) }?
        };
    }
    Ok(GlFns {
        create_shader: load!("glCreateShader", extern "system" fn(GLenum) -> GLuint),
        shader_source: load!(
            "glShaderSource",
            extern "system" fn(GLuint, GLsizei, *const *const GLchar, *const GLint)
        ),
        compile_shader: load!("glCompileShader", extern "system" fn(GLuint)),
        get_shaderiv: load!(
            "glGetShaderiv",
            extern "system" fn(GLuint, GLenum, *mut GLint)
        ),
        get_shader_info_log: load!(
            "glGetShaderInfoLog",
            extern "system" fn(GLuint, GLsizei, *mut GLsizei, *mut GLchar)
        ),
        create_program: load!("glCreateProgram", extern "system" fn() -> GLuint),
        attach_shader: load!("glAttachShader", extern "system" fn(GLuint, GLuint)),
        link_program: load!("glLinkProgram", extern "system" fn(GLuint)),
        get_programiv: load!(
            "glGetProgramiv",
            extern "system" fn(GLuint, GLenum, *mut GLint)
        ),
        get_program_info_log: load!(
            "glGetProgramInfoLog",
            extern "system" fn(GLuint, GLsizei, *mut GLsizei, *mut GLchar)
        ),
        use_program: load!("glUseProgram", extern "system" fn(GLuint)),
        get_uniform_location: load!(
            "glGetUniformLocation",
            extern "system" fn(GLuint, *const GLchar) -> GLint
        ),
        uniform_1i: load!("glUniform1i", extern "system" fn(GLint, GLint)),
        uniform_1f: load!("glUniform1f", extern "system" fn(GLint, GLfloat)),
        uniform_2f: load!("glUniform2f", extern "system" fn(GLint, GLfloat, GLfloat)),
        delete_shader: load!("glDeleteShader", extern "system" fn(GLuint)),
        delete_program: load!("glDeleteProgram", extern "system" fn(GLuint)),
        gen_textures: load!("glGenTextures", extern "system" fn(GLsizei, *mut GLuint)),
        bind_texture: load!("glBindTexture", extern "system" fn(GLenum, GLuint)),
        tex_image_2d: load!(
            "glTexImage2D",
            extern "system" fn(
                GLenum,
                GLint,
                GLint,
                GLsizei,
                GLsizei,
                GLint,
                GLenum,
                GLenum,
                *const std::ffi::c_void,
            )
        ),
        tex_parameteri: load!("glTexParameteri", extern "system" fn(GLenum, GLenum, GLint)),
        active_texture: load!("glActiveTexture", extern "system" fn(GLenum)),
        delete_textures: load!(
            "glDeleteTextures",
            extern "system" fn(GLsizei, *const GLuint)
        ),
        gen_framebuffers: load!(
            "glGenFramebuffers",
            extern "system" fn(GLsizei, *mut GLuint)
        ),
        bind_framebuffer: load!("glBindFramebuffer", extern "system" fn(GLenum, GLuint)),
        check_framebuffer_status: load!(
            "glCheckFramebufferStatus",
            extern "system" fn(GLenum) -> GLenum
        ),
        framebuffer_texture_2d: load!(
            "glFramebufferTexture2D",
            extern "system" fn(GLenum, GLenum, GLenum, GLuint, GLint)
        ),
        delete_framebuffers: load!(
            "glDeleteFramebuffers",
            extern "system" fn(GLsizei, *const GLuint)
        ),
        viewport: load!(
            "glViewport",
            extern "system" fn(GLint, GLint, GLsizei, GLsizei)
        ),
        draw_arrays: load!("glDrawArrays", extern "system" fn(GLenum, GLint, GLsizei)),
        enable_vertex_attrib_array: load!("glEnableVertexAttribArray", extern "system" fn(GLuint)),
        vertex_attrib_pointer: load!(
            "glVertexAttribPointer",
            extern "system" fn(GLuint, GLint, GLenum, GLboolean, GLsizei, *const std::ffi::c_void)
        ),
        read_pixels: load!(
            "glReadPixels",
            extern "system" fn(
                GLint,
                GLint,
                GLsizei,
                GLsizei,
                GLenum,
                GLenum,
                *mut std::ffi::c_void,
            )
        ),
        get_error: load!("glGetError", extern "system" fn() -> GLenum),
    })
}

/// 取 `gbm_device*`（`gbm` 未导出底层类型名，经 `gbm::AsRaw` 唯一 impl 推断）。
fn gbm_device_ptr<T: AsFd>(device: &gbm::Device<T>) -> *mut std::ffi::c_void {
    use gbm::AsRaw;
    fn raw_as_usize<D, U>(d: &D) -> usize
    where
        D: AsRaw<U>,
    {
        d.as_raw() as usize
    }
    raw_as_usize(device) as *mut std::ffi::c_void
}

/// EGL 错误包装（带调用上下文）。
fn egl_err(ctx: &'static str) -> impl FnOnce(egl::Error) -> RenderError {
    move |e| RenderError::Failed(format!("{ctx} 失败: {e:?}"))
}

/// 编译并链接单个效果的着色器程序。
fn build_program(gl: &GlFns, kind: EffectKind) -> Result<GLuint, RenderError> {
    let (vs_src, fs_src) = shader_sources(kind);
    let vs = compile_shader(gl, GL_VERTEX_SHADER, vs_src, "顶点着色器")?;
    let fs = compile_shader(gl, GL_FRAGMENT_SHADER, fs_src, "片元着色器")?;
    let program = (gl.create_program)();
    if program == 0 {
        return Err(RenderError::Failed("glCreateProgram 返回 0".to_string()));
    }
    (gl.attach_shader)(program, vs);
    (gl.attach_shader)(program, fs);
    (gl.link_program)(program);
    // 着色器已附加，链接后即可删除（GL 内部保留引用）。
    (gl.delete_shader)(vs);
    (gl.delete_shader)(fs);

    let mut status: GLint = 0;
    (gl.get_programiv)(program, GL_LINK_STATUS, &mut status);
    if status == 0 {
        let mut log = vec![0u8; 4096];
        let mut written: GLsizei = 0;
        (gl.get_program_info_log)(
            program,
            log.len() as GLsizei,
            &mut written,
            log.as_mut_ptr().cast::<GLchar>(),
        );
        (gl.delete_program)(program);
        let text = String::from_utf8_lossy(&log[..written.max(0) as usize]).into_owned();
        return Err(RenderError::Failed(format!(
            "glLinkProgram 失败（{kind:?}）: {text}"
        )));
    }
    Ok(program)
}

/// 编译单个着色器，失败时带 info log。
fn compile_shader(
    gl: &GlFns,
    stage: GLenum,
    source: &str,
    label: &str,
) -> Result<GLuint, RenderError> {
    let shader = (gl.create_shader)(stage);
    if shader == 0 {
        return Err(RenderError::Failed(format!(
            "glCreateShader 返回 0（{label}）"
        )));
    }
    let mut bytes = source.as_bytes().to_vec();
    bytes.push(0); // NUL 结尾
    let ptr = bytes.as_ptr().cast::<GLchar>();
    let len = (bytes.len() - 1) as GLint;
    let sources: [*const GLchar; 1] = [ptr];
    let lengths: [GLint; 1] = [len];
    (gl.shader_source)(shader, 1, sources.as_ptr(), lengths.as_ptr());
    (gl.compile_shader)(shader);

    let mut status: GLint = 0;
    (gl.get_shaderiv)(shader, GL_COMPILE_STATUS, &mut status);
    if status == 0 {
        let mut log = vec![0u8; 4096];
        let mut written: GLsizei = 0;
        (gl.get_shader_info_log)(
            shader,
            log.len() as GLsizei,
            &mut written,
            log.as_mut_ptr().cast::<GLchar>(),
        );
        (gl.delete_shader)(shader);
        let text = String::from_utf8_lossy(&log[..written.max(0) as usize]).into_owned();
        return Err(RenderError::Failed(format!("{label} 编译失败: {text}")));
    }
    Ok(shader)
}

/// GL 调用后检查 `glGetError`（失败映射 [`RenderError::Failed`]）。
fn check_gl(gl: &GlFns, ctx: &str) -> Result<(), RenderError> {
    let err = (gl.get_error)();
    if err != 0 {
        return Err(RenderError::Failed(format!(
            "{ctx} 失败: glError 0x{err:X}"
        )));
    }
    Ok(())
}

/// 读取 uniform 位置（不存在返回错误，带上下文）。
fn uniform_loc(gl: &GlFns, program: GLuint, name: &str) -> Result<GLint, RenderError> {
    let mut buf = name.as_bytes().to_vec();
    buf.push(0);
    let loc = (gl.get_uniform_location)(program, buf.as_ptr().cast::<GLchar>());
    if loc < 0 {
        return Err(RenderError::Failed(format!("uniform {name} 不存在")));
    }
    Ok(loc)
}

/// 把 [`CpuFrame`] 像素转换为 GL_RGBA8 顶行在前的字节流。
///
/// Xrgb8888 内存 `[B,G,R,X]` → `(R,G,B,255)`；Rgba8888 原样（A 置 255）。
fn frame_to_rgba(frame: &CpuFrame) -> Result<Vec<u8>, RenderError> {
    frame.validate().map_err(RenderError::InvalidFrame)?;
    let mut out = vec![0u8; (frame.width * frame.height * 4) as usize];
    for y in 0..frame.height {
        for x in 0..frame.width {
            let src = (y * frame.stride + x * 4) as usize;
            let dst = ((y * frame.width + x) * 4) as usize;
            match frame.format {
                PixelFormat::Xrgb8888 => {
                    out[dst] = frame.data[src + 2];
                    out[dst + 1] = frame.data[src + 1];
                    out[dst + 2] = frame.data[src];
                    out[dst + 3] = 255;
                }
                PixelFormat::Rgba8888 => {
                    out[dst..dst + 3].copy_from_slice(&frame.data[src..src + 3]);
                    out[dst + 3] = 255;
                }
                other => {
                    return Err(RenderError::UnsupportedSourceFormat { format: other });
                }
            }
        }
    }
    Ok(out)
}

/// 把 `glReadPixels` 的 GL_RGBA8（底行在前）转换为 Xrgb8888 [`CpuFrame`]（顶行在前）。
fn rgba_to_xrgb_frame(pixels: &[u8], width: u32, height: u32) -> Result<CpuFrame, RenderError> {
    let needed = (width * height * 4) as usize;
    if pixels.len() < needed {
        return Err(RenderError::Failed(format!(
            "glReadPixels 数据不足: {} < {needed}",
            pixels.len()
        )));
    }
    let mut out = vec![0u8; needed];
    for y in 0..height {
        // GL 行序 bottom-up → top-down。
        let src_row = ((height - 1 - y) * width) as usize;
        let dst_row = (y * width) as usize;
        for x in 0..width {
            let s = (src_row + x as usize) * 4;
            let d = (dst_row + x as usize) * 4;
            out[d] = pixels[s + 2]; // B
            out[d + 1] = pixels[s + 1]; // G
            out[d + 2] = pixels[s]; // R
            out[d + 3] = 0; // X
        }
    }
    Ok(CpuFrame {
        width,
        height,
        stride: width * 4,
        format: PixelFormat::Xrgb8888,
        data: Arc::from(out.into_boxed_slice()),
    })
}

// ---------------------------------------------------------------------------
// GlesTransition
// ---------------------------------------------------------------------------

/// EGL 上下文与表面（`release` 时销毁）。
struct EglState {
    instance: EglInstance,
    display: egl::Display,
    context: egl::Context,
    surface: egl::Surface,
}

/// GLES 转场渲染器（**真机未验证，仅编译级交付**）。
///
/// 生命周期：[`GlesTransition::new`] 建上下文 → [`GlesTransition::upload_frames`]
/// 上传纹理 → 逐次 [`GlesTransition::render_to_buffer`] → [`GlesTransition::release`]
/// 释放中间资源。新的转场请求覆盖旧纹理（最后请求优先）。
pub struct GlesTransition {
    width: u32,
    height: u32,
    egl: Option<EglState>,
    gl: Option<GlFns>,
    /// `from` / `to` 纹理 id。
    frame_tex: [GLuint; 2],
    /// FBO 与其颜色附件纹理 id。
    fbo: [GLuint; 2],
    /// 每种效果的着色器程序（惰性编译，`release` 删除）。
    programs: [Option<GLuint>; 4],
    /// 纹理是否已上传。
    uploaded: bool,
}

impl GlesTransition {
    /// 通过 GBM device fd 初始化 EGL/上下文；**真机未验证**。
    ///
    /// 创建 GBM platform display + pbuffer surface + ES2 上下文，
    /// 加载 GL 入口并建立 FBO。
    ///
    /// # Errors
    ///
    /// 尺寸非法、GBM/EGL/GL 初始化或 FBO 建立失败时返回 [`RenderError::Failed`]
    /// （含上下文信息）；上层应立即回退 [`crate::NoTransition`]。
    pub fn new(
        gbm_device_fd: BorrowedFd<'_>,
        width: u32,
        height: u32,
    ) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::InvalidOutput { width, height });
        }

        // SAFETY：fd 由 BorrowedFd 借用约束保证有效，本模块不关闭它
        // （模块文档 invariant 4）。
        let gbm_device = gbm::Device::new(gbm_device_fd)
            .map_err(|e| RenderError::Failed(format!("GBM 设备打开失败: {e}")))?;
        let gbm_ptr = gbm_device_ptr(&gbm_device);

        // SAFETY：动态加载 libEGL 并要求 EGL 1.5（invariant 1）。
        let instance = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
        }
        .map_err(|e| RenderError::Failed(format!("libEGL 动态加载失败: {e}")))?;

        // SAFETY：gbm_ptr 来自有效 GBM 设备，与 PLATFORM_GBM_KHR 匹配。
        let display = unsafe {
            instance.get_platform_display(PLATFORM_GBM_KHR, gbm_ptr, &[egl::ATTRIB_NONE])
        }
        .map_err(egl_err("eglGetPlatformDisplay(GBM)"))?;
        instance
            .initialize(display)
            .map_err(egl_err("eglInitialize"))?;
        instance
            .bind_api(egl::OPENGL_ES_API)
            .map_err(egl_err("eglBindAPI(OPENGL_ES_API)"))?;

        let config = instance
            .choose_first_config(
                display,
                &[
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
                ],
            )
            .map_err(egl_err("eglChooseConfig"))?
            .ok_or_else(|| {
                RenderError::Failed("eglChooseConfig 无匹配的 ES2 pbuffer 配置".to_string())
            })?;

        let context = instance
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
            )
            .map_err(egl_err("eglCreateContext(ES2)"))?;
        let surface = instance
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
            .map_err(egl_err("eglCreatePbufferSurface"))?;
        instance
            .make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(egl_err("eglMakeCurrent"))?;

        let gl = load_gl(&instance)?;

        // 纹理：0 = from，1 = to；另建 FBO 颜色附件。
        let mut frame_tex: [GLuint; 2] = [0; 2];
        (gl.gen_textures)(2, frame_tex.as_mut_ptr());
        if frame_tex.contains(&0) {
            return Err(RenderError::Failed("glGenTextures 返回 0".to_string()));
        }
        let mut fbo: [GLuint; 2] = [0; 2];
        (gl.gen_framebuffers)(1, fbo.as_mut_ptr());
        (gl.gen_textures)(1, fbo[1..].as_mut_ptr());
        if fbo[0] == 0 || fbo[1] == 0 {
            return Err(RenderError::Failed(
                "glGenFramebuffers/glGenTextures 返回 0".to_string(),
            ));
        }

        for tex in frame_tex {
            (gl.bind_texture)(GL_TEXTURE_2D, tex);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        }
        // FBO 颜色附件（尺寸固定为输出尺寸）。
        (gl.bind_texture)(GL_TEXTURE_2D, fbo[1]);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        // 依据：data 为空指针表示仅分配不填充（fn 指针调用是安全语法，
        // 真正的 unsafe 在 load_sym 的转位，见模块文档 invariant 1/3）。
        (gl.tex_image_2d)(
            GL_TEXTURE_2D,
            0,
            GL_RGBA as GLint,
            width as GLsizei,
            height as GLsizei,
            0,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            std::ptr::null(),
        );
        (gl.bind_framebuffer)(GL_FRAMEBUFFER, fbo[0]);
        (gl.framebuffer_texture_2d)(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            fbo[1],
            0,
        );
        let status = (gl.check_framebuffer_status)(GL_FRAMEBUFFER);
        if status != GL_FRAMEBUFFER_COMPLETE {
            return Err(RenderError::Failed(format!("FBO 不完整: 0x{status:X}")));
        }
        check_gl(&gl, "GLES 初始化")?;

        Ok(Self {
            width,
            height,
            egl: Some(EglState {
                instance,
                display,
                context,
                surface,
            }),
            gl: Some(gl),
            frame_tex,
            fbo,
            programs: [None, None, None, None],
            uploaded: false,
        })
    }

    /// 上传 `from`/`to` 两张纹理（覆盖旧纹理，最后请求优先）。
    ///
    /// # Errors
    ///
    /// 尺寸与本渲染器不一致、帧自检失败或 GL 调用失败时返回 [`RenderError`]。
    pub fn upload_frames(&mut self, from: &CpuFrame, to: &CpuFrame) -> Result<(), RenderError> {
        let gl = self.gl.as_ref().ok_or(RenderError::NotPrepared)?;
        for frame in [from, to] {
            frame.validate().map_err(RenderError::InvalidFrame)?;
            if frame.width != self.width || frame.height != self.height {
                return Err(RenderError::FrameMismatch(format!(
                    "帧尺寸 {}x{} 与渲染器 {}x{} 不一致",
                    frame.width, frame.height, self.width, self.height
                )));
            }
            if !matches!(frame.format, PixelFormat::Xrgb8888 | PixelFormat::Rgba8888) {
                return Err(RenderError::UnsupportedSourceFormat {
                    format: frame.format,
                });
            }
        }
        let from_rgba = frame_to_rgba(from)?;
        let to_rgba = frame_to_rgba(to)?;
        for (tex, rgba) in [
            (self.frame_tex[0], &from_rgba),
            (self.frame_tex[1], &to_rgba),
        ] {
            (gl.bind_texture)(GL_TEXTURE_2D, tex);
            // 依据：rgba 长度恰为 width*height*4（frame_to_rgba 保证），
            // 生命周期覆盖调用（invariant 3）。
            (gl.tex_image_2d)(
                GL_TEXTURE_2D,
                0,
                GL_RGBA as GLint,
                self.width as GLsizei,
                self.height as GLsizei,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                rgba.as_ptr().cast::<std::ffi::c_void>(),
            );
            check_gl(gl, "glTexImage2D(上传帧)")?;
        }
        self.uploaded = true;
        Ok(())
    }

    /// 渲染一帧转场画面到 CPU 缓冲（`progress ∈ [0, 1]`，内部先过缓动）。
    ///
    /// # Errors
    ///
    /// 未上传纹理、进度非法或 GL 调用失败时返回 [`RenderError`]。
    pub fn render_to_buffer(
        &mut self,
        spec: &TransitionSpec,
        progress: f32,
    ) -> Result<CpuFrame, RenderError> {
        if progress.is_nan() || !(0.0..=1.0).contains(&progress) {
            return Err(RenderError::InvalidProgress { progress });
        }
        if !self.uploaded {
            return Err(RenderError::NotPrepared);
        }
        let eased = apply_easing(spec.easing, progress);
        let code = effect_code(spec.kind);
        let dir = direction_vec(kind_direction(spec.kind));

        let gl = self.gl.as_ref().ok_or(RenderError::NotPrepared)?;
        let program = match self.programs[code as usize] {
            Some(p) => p,
            None => {
                let p = build_program(gl, spec.kind)?;
                self.programs[code as usize] = Some(p);
                p
            }
        };

        (gl.use_program)(program);
        let loc_progress = uniform_loc(gl, program, "u_progress")?;
        let loc_direction = uniform_loc(gl, program, "u_direction")?;
        let loc_size = uniform_loc(gl, program, "u_size")?;
        let loc_effect = uniform_loc(gl, program, "u_effect")?;
        let loc_from = uniform_loc(gl, program, "u_from")?;
        let loc_to = uniform_loc(gl, program, "u_to")?;
        (gl.uniform_1f)(loc_progress, eased);
        (gl.uniform_2f)(loc_direction, dir[0], dir[1]);
        (gl.uniform_2f)(loc_size, self.width as f32, self.height as f32);
        (gl.uniform_1i)(loc_effect, code);
        (gl.uniform_1i)(loc_from, 0);
        (gl.uniform_1i)(loc_to, 1);

        (gl.active_texture)(GL_TEXTURE0);
        (gl.bind_texture)(GL_TEXTURE_2D, self.frame_tex[0]);
        (gl.active_texture)(GL_TEXTURE1);
        (gl.bind_texture)(GL_TEXTURE_2D, self.frame_tex[1]);

        (gl.bind_framebuffer)(GL_FRAMEBUFFER, self.fbo[0]);
        (gl.viewport)(0, 0, self.width as GLsizei, self.height as GLsizei);
        (gl.enable_vertex_attrib_array)(0);
        (gl.enable_vertex_attrib_array)(1);
        // 依据：QUAD_POS/QUAD_UV 是 'static 数组，客户端顶点数组在
        // glDrawArrays 返回前保持有效（invariant 3）。
        (gl.vertex_attrib_pointer)(
            0,
            2,
            GL_FLOAT,
            0,
            0,
            QUAD_POS.as_ptr().cast::<std::ffi::c_void>(),
        );
        (gl.vertex_attrib_pointer)(
            1,
            2,
            GL_FLOAT,
            0,
            0,
            QUAD_UV.as_ptr().cast::<std::ffi::c_void>(),
        );
        (gl.draw_arrays)(GL_TRIANGLE_STRIP, 0, 4);
        check_gl(gl, "glDrawArrays")?;

        let mut pixels = vec![0u8; (self.width * self.height * 4) as usize];
        // 依据：pixels 长度恰为 width*height*4（invariant 3）。
        (gl.read_pixels)(
            0,
            0,
            self.width as GLsizei,
            self.height as GLsizei,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            pixels.as_mut_ptr().cast::<std::ffi::c_void>(),
        );
        check_gl(gl, "glReadPixels")?;
        rgba_to_xrgb_frame(&pixels, self.width, self.height)
    }

    /// 释放纹理/FBO/着色器与 EGL 上下文等中间资源（可重复调用）。
    pub fn release(&mut self) {
        self.uploaded = false;
        self.programs = [None, None, None, None];
        if let (Some(gl), Some(egl_state)) = (self.gl.as_ref(), self.egl.as_ref()) {
            for program in self.programs.iter().flatten() {
                (gl.delete_program)(*program);
            }
            for tex in self.frame_tex {
                if tex != 0 {
                    (gl.delete_textures)(1, &tex);
                }
            }
            // fbo[0] 是 FBO 对象，fbo[1] 是颜色附件纹理，分别删除。
            if self.fbo[0] != 0 {
                (gl.delete_framebuffers)(1, &self.fbo[0]);
            }
            if self.fbo[1] != 0 {
                (gl.delete_textures)(1, &self.fbo[1]);
            }
            self.frame_tex = [0, 0];
            self.fbo = [0, 0];
            let _ = egl_state
                .instance
                .make_current(egl_state.display, None, None, None);
            let _ = egl_state
                .instance
                .destroy_surface(egl_state.display, egl_state.surface);
            let _ = egl_state
                .instance
                .destroy_context(egl_state.display, egl_state.context);
            let _ = egl_state.instance.terminate(egl_state.display);
        }
        self.gl = None;
        self.egl = None;
    }
}

impl Drop for GlesTransition {
    fn drop(&mut self) {
        self.release();
    }
}

impl std::fmt::Debug for GlesTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlesTransition")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("uploaded", &self.uploaded)
            .field("egl_ready", &self.egl.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_kinds() -> [EffectKind; 8] {
        [
            EffectKind::Fade,
            EffectKind::Slide {
                direction: TransitionDirection::Left,
            },
            EffectKind::Slide {
                direction: TransitionDirection::Right,
            },
            EffectKind::Slide {
                direction: TransitionDirection::Up,
            },
            EffectKind::Slide {
                direction: TransitionDirection::Down,
            },
            EffectKind::Wipe {
                direction: TransitionDirection::Left,
            },
            EffectKind::Wipe {
                direction: TransitionDirection::Down,
            },
            EffectKind::Circle,
        ]
    }

    #[test]
    fn shader_sources_have_main_and_declarations() {
        for kind in all_kinds() {
            let (vs, fs) = shader_sources(kind);
            assert!(vs.contains("void main()"), "{kind:?} 顶点着色器缺 main");
            assert!(vs.contains("attribute"), "{kind:?} 顶点着色器缺 attribute");
            assert!(fs.contains("void main()"), "{kind:?} 片元着色器缺 main");
            assert!(
                fs.contains("uniform float u_progress"),
                "{kind:?} 缺 u_progress"
            );
            assert!(
                fs.contains("uniform vec2 u_direction"),
                "{kind:?} 缺 u_direction"
            );
            assert!(fs.contains("uniform int u_effect"), "{kind:?} 缺 u_effect");
            assert!(
                fs.contains("varying vec2 v_texcoord"),
                "{kind:?} 缺 varying"
            );
        }
    }

    #[test]
    fn fragment_sources_select_own_effect_code() {
        for kind in all_kinds() {
            let (_, fs) = shader_sources(kind);
            let code = effect_code(kind);
            assert!(
                fs.contains(&format!("u_effect != {code}")),
                "{kind:?} 片元应校验 u_effect == {code}"
            );
        }
    }

    #[test]
    fn effect_codes_and_directions_are_distinct() {
        assert_eq!(effect_code(EffectKind::Fade), 0);
        assert_eq!(
            effect_code(EffectKind::Slide {
                direction: TransitionDirection::Left
            }),
            1
        );
        assert_eq!(
            effect_code(EffectKind::Wipe {
                direction: TransitionDirection::Up
            }),
            2
        );
        assert_eq!(effect_code(EffectKind::Circle), 3);
        assert_eq!(direction_vec(Some(TransitionDirection::Left)), [-1.0, 0.0]);
        assert_eq!(direction_vec(Some(TransitionDirection::Right)), [1.0, 0.0]);
        assert_eq!(direction_vec(Some(TransitionDirection::Up)), [0.0, -1.0]);
        assert_eq!(direction_vec(Some(TransitionDirection::Down)), [0.0, 1.0]);
        assert_eq!(direction_vec(None), [0.0, 0.0]);
    }

    #[test]
    fn fade_fragment_mixes_linearly() {
        let (_, fs) = shader_sources(EffectKind::Fade);
        assert!(fs.contains("mix(texture2D(u_from"), "fade 应使用 mix 混合");
        assert!(fs.contains("u_progress"), "fade 应消费 u_progress");
    }

    #[test]
    fn frame_conversion_swizzles_xrgb() {
        let frame = CpuFrame {
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![30u8, 20, 10, 0].into_boxed_slice()), // [B,G,R,X]
        };
        let rgba = frame_to_rgba(&frame).unwrap();
        assert_eq!(rgba, vec![10, 20, 30, 255], "应转为 (R,G,B,A)");

        let back = rgba_to_xrgb_frame(&[10, 20, 30, 255], 1, 1).unwrap();
        assert_eq!(back.format, PixelFormat::Xrgb8888);
        assert_eq!(&back.data[..], &[30, 20, 10, 0], "应回到 [B,G,R,X]");
    }

    #[test]
    fn readback_flips_rows_bottom_up() {
        // 2x2：GL 输入底行在前。顶行（y=0）在数据尾部。
        let gl_pixels: [u8; 16] = [
            9, 9, 9, 255, // GL 行 0（画面底部）
            9, 9, 9, 255, //
            1, 2, 3, 255, // GL 行 1（画面顶部）
            1, 2, 3, 255,
        ];
        let frame = rgba_to_xrgb_frame(&gl_pixels, 2, 2).unwrap();
        let top_left = &frame.data[0..4];
        assert_eq!(top_left, &[3, 2, 1, 0], "顶部行应来自 GL 末行");
    }

    #[test]
    fn kind_direction_only_for_slide_and_wipe() {
        assert_eq!(
            kind_direction(EffectKind::Slide {
                direction: TransitionDirection::Up
            }),
            Some(TransitionDirection::Up)
        );
        assert_eq!(kind_direction(EffectKind::Fade), None);
        assert_eq!(kind_direction(EffectKind::Circle), None);
    }

    #[test]
    fn egl_error_wrapper_carries_context() {
        let err = egl_err("eglInitialize")(egl::Error::NotInitialized);
        let text = err.to_string();
        assert!(text.contains("eglInitialize"), "错误应带上下文: {text}");
    }
}
