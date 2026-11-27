//! `liuguang-render` — 帧变换与切换特效渲染接口。
//!
//! **生产路径 = CPU 光栅化（[`cpu`]）+ CPU 转场（[`transition`]）**，
//! 两者都在 4K（3840x2400）下有实测数据：转场每帧 fade 20.2 ms、
//! slide/wipe/circle 4.2–4.4 ms（真机 DP-1，见
//! `crates/liuguang-display/PERF.md`）。
//!
//! `gles`（feature `"gles"`，GBM + EGL/GLES shader）**是未验证路径**：
//! 未在真机跑通、不被任何生产配置启用、**不参与验收**，仅作为规范 §7.2 的
//! 参考实现保留（详细理由与启用前必须先完成的验证见该模块文档）。
//!
//! 本 crate 还定义 [`TransitionEffect`] trait、[`RenderError`] 与
//! [`NoTransition`]（立即切换）。
//!
//! 职责边界（规范 §3.3）：本 crate 不访问播放列表、不修改配置。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cpu;
pub mod transition;

/// GLES 渲染路径（feature `gles`，非默认）：GBM + EGL + GLSL ES shader。
///
/// **真机未验证（阻塞 B1），仅编译级交付**，详见该模块文档。
#[cfg(feature = "gles")]
pub mod gles;

use liuguang_core::types::{DisplayFrame, PixelFormat};

/// 渲染错误。
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// 尚未调用 `prepare` 或已 `release`。
    #[error("转场资源未就绪（prepare 未调用或已 release）")]
    NotPrepared,
    /// 进度超出 `[0.0, 1.0]` 或非有限值。
    #[error("转场进度非法: {progress}（要求有限且位于 [0.0, 1.0]）")]
    InvalidProgress {
        /// 实际进度。
        progress: f32,
    },
    /// 源帧格式不支持。
    #[error("不支持的源帧格式: {format:?}")]
    UnsupportedSourceFormat {
        /// 实际格式。
        format: PixelFormat,
    },
    /// 输出尺寸非法。
    #[error("输出尺寸必须大于 0，实际 {width}x{height}")]
    InvalidOutput {
        /// 实际宽。
        width: u32,
        /// 实际高。
        height: u32,
    },
    /// 输出缓冲超过预算。
    #[error("输出缓冲需要 {needed} 字节，超过预算 {limit}")]
    OutputTooLarge {
        /// 需要字节数。
        needed: u64,
        /// 预算上限。
        limit: u64,
    },
    /// 转场双方帧不匹配（尺寸/像素格式不一致或格式不支持）。
    #[error("转场帧不匹配: {0}")]
    FrameMismatch(String),
    /// 帧结构自检失败。
    #[error("源帧不合法: {0}")]
    InvalidFrame(#[from] liuguang_core::types::FrameError),
    /// 适配计算失败。
    #[error("适配计算失败: {0}")]
    Fit(#[from] liuguang_core::fit::FitError),
    /// 渲染失败。
    #[error("渲染失败: {0}")]
    Failed(String),
}

/// 切换特效接口（规范 §3.4 的固定边界接口）。
///
/// 生命周期：`prepare` 持有必要的 old/new 资源 → 逐次 `render(progress)`
/// → `release` 释放中间资源（M4 的 FBO/纹理等）。新的切换请求到来时
/// 采用「最后请求优先」：上层取消过期任务后重新 `prepare`。
pub trait TransitionEffect {
    /// 预加载/持有转场所需资源。
    ///
    /// # Errors
    ///
    /// 资源准备失败时返回 [`RenderError`]。
    fn prepare(&mut self, from: &DisplayFrame, to: &DisplayFrame) -> Result<(), RenderError>;
    /// 渲染进度画面（`progress` ∈ `[0.0, 1.0]`）。
    ///
    /// # Errors
    ///
    /// 未就绪或进度非法时返回 [`RenderError`]。
    fn render(&mut self, progress: f32) -> Result<DisplayFrame, RenderError>;
    /// 释放转场资源。
    fn release(&mut self);
}

/// 无特效：立即切换到目标帧（也是 GPU 特效不可用时的回退路径）。
#[derive(Debug, Default)]
pub struct NoTransition {
    target: Option<DisplayFrame>,
}

impl NoTransition {
    /// 创建空的无特效转场。
    pub fn new() -> Self {
        Self::default()
    }
}

impl TransitionEffect for NoTransition {
    fn prepare(&mut self, _from: &DisplayFrame, to: &DisplayFrame) -> Result<(), RenderError> {
        self.target = Some(to.clone());
        Ok(())
    }

    fn render(&mut self, progress: f32) -> Result<DisplayFrame, RenderError> {
        if !(0.0..=1.0).contains(&progress) {
            return Err(RenderError::InvalidProgress { progress });
        }
        self.target.clone().ok_or(RenderError::NotPrepared)
    }

    fn release(&mut self) {
        self.target = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::types::{CpuFrame, PixelFormat};
    use std::sync::Arc;

    fn cpu_frame(w: u32, h: u32) -> DisplayFrame {
        let stride = w * 4;
        DisplayFrame::Cpu(CpuFrame {
            width: w,
            height: h,
            stride,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; (stride * h) as usize].into_boxed_slice()),
        })
    }

    #[test]
    fn no_transition_returns_target_frame() {
        let mut t = NoTransition::new();
        let from = cpu_frame(16, 16);
        let to = cpu_frame(32, 32);
        t.prepare(&from, &to).unwrap();
        let frame = t.render(0.5).unwrap();
        assert_eq!(frame.width(), 32);
        assert_eq!(frame.height(), 32);
    }

    #[test]
    fn no_transition_rejects_bad_progress_and_unprepared() {
        let mut t = NoTransition::new();
        assert!(matches!(t.render(0.5), Err(RenderError::NotPrepared)));

        let from = cpu_frame(8, 8);
        let to = cpu_frame(8, 8);
        t.prepare(&from, &to).unwrap();
        for p in [-0.1, 1.1, f32::NAN] {
            assert!(matches!(
                t.render(p),
                Err(RenderError::InvalidProgress { .. })
            ));
        }
        t.release();
        assert!(matches!(t.render(0.0), Err(RenderError::NotPrepared)));
    }
}
