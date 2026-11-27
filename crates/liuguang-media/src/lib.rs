//! `liuguang-media` — 媒体源抽象与帧供给接口。
//!
//! PNG 解码已实现（[`png_load`]，含尺寸/内存上限）；
//! 视频帧供给核心（[`video`]：有界帧队列 / PTS 调度 / 丢帧策略 / 暂停循环）
//! 与预加载缓存（[`prepared`]）属 M5；真实 MP4/H.264 解码（[`gstreamer`]）
//! 位于非默认 feature `gstreamer` 之后，编译级验证。
//! 本 crate 还定义 [`MediaSource`] trait、[`FramePoll`]、[`MediaError`]
//! 与内存静态源 [`StaticFrameSource`]（测试与 mock 用）。
//!
//! 职责边界（规范 §3.3）：本 crate 不接管显示器输出、不触碰 KMS。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod png_load;
pub mod prepared;
pub mod thumbnail;
pub mod va;
pub mod video;

#[cfg(feature = "gstreamer")]
pub mod gstreamer;

use liuguang_core::types::DisplayFrame;
/// 媒体元数据（`MediaSource::metadata` 的返回类型，属公开 API）。
pub use liuguang_core::types::MediaInfo;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 帧供给结果。
#[derive(Debug, Clone)]
pub enum FramePoll {
    /// 一帧已就绪（按 PTS/单调时钟调度）。
    Ready(DisplayFrame),
    /// 暂无帧；`next_in` 提示下次可查询时间（`None` 表示等待外部事件）。
    Pending {
        /// 建议的下次查询间隔。
        next_in: Option<Duration>,
    },
    /// 播放结束（静态图第二次查询后即为此状态）。
    Ended,
}

/// 媒体错误。
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// 源处于暂停状态。
    #[error("媒体源已暂停")]
    Paused,
    /// 解码/读取失败。
    #[error("媒体读取或解码失败: {0}")]
    Decode(String),
    /// 状态非法（如重复 resume）。
    #[error("媒体状态非法: {0}")]
    InvalidState(String),
    /// 文件 I/O 失败。
    #[error("媒体文件读取失败: {path}: {source}")]
    Io {
        /// 文件路径。
        path: PathBuf,
        /// 底层错误。
        source: std::io::Error,
    },
    /// 文件超过大小上限。
    #[error("媒体文件超过大小上限: {path}（{size} > {limit} 字节）")]
    FileTooLarge {
        /// 文件路径。
        path: PathBuf,
        /// 实际大小。
        size: u64,
        /// 上限。
        limit: u64,
    },
    /// 图像尺寸超过单边上限。
    #[error("图像尺寸 {width}x{height} 超过单边上限 {limit}")]
    DimensionExceeded {
        /// 实际宽。
        width: u32,
        /// 实际高。
        height: u32,
        /// 单边上限。
        limit: u32,
    },
    /// 帧结构自检失败。
    #[error("帧不合法: {0}")]
    InvalidFrame(#[from] liuguang_core::types::FrameError),
    /// 解码缓冲超过内存预算。
    #[error("解码缓冲需要 {needed} 字节，超过预算 {limit}")]
    DecodedTooLarge {
        /// 需要字节数（0 表示解码器报告超限）。
        needed: u64,
        /// 预算上限。
        limit: u64,
    },
    /// 选项非法（如队列容量为 0、fps 上限非正数）。
    #[error("媒体选项非法: {0}")]
    InvalidOptions(String),
}

/// 媒体源接口（规范 §3.4 的固定边界接口）。
///
/// 帧时间基于媒体 PTS 与单调时钟（`now` 参数），
/// **不**按显示器刷新率强制解码。
pub trait MediaSource {
    /// 媒体元数据。
    fn metadata(&self) -> MediaInfo;
    /// 供给下一帧。
    ///
    /// # Errors
    ///
    /// 解码失败或状态非法时返回 [`MediaError`]。
    fn poll_frame(&mut self, now: Instant) -> Result<FramePoll, MediaError>;
    /// 暂停（显示关闭/断开时调用，停止解码与预取）。
    ///
    /// # Errors
    ///
    /// 状态非法时返回 [`MediaError`]。
    fn pause(&mut self) -> Result<(), MediaError>;
    /// 恢复。
    ///
    /// # Errors
    ///
    /// 状态非法时返回 [`MediaError`]。
    fn resume(&mut self) -> Result<(), MediaError>;
}

/// 内存静态帧源：第一次 `poll_frame` 返回该帧，之后返回 [`FramePoll::Ended`]。
/// 用于测试与「显示 fallback/最后成功画面」路径。
#[derive(Debug, Clone)]
pub struct StaticFrameSource {
    info: MediaInfo,
    frame: DisplayFrame,
    emitted: bool,
    paused: bool,
}

impl StaticFrameSource {
    /// 以给定帧与元数据创建。
    pub fn new(info: MediaInfo, frame: DisplayFrame) -> Self {
        Self {
            info,
            frame,
            emitted: false,
            paused: false,
        }
    }
}

impl MediaSource for StaticFrameSource {
    fn metadata(&self) -> MediaInfo {
        self.info.clone()
    }

    fn poll_frame(&mut self, _now: Instant) -> Result<FramePoll, MediaError> {
        if self.paused {
            return Ok(FramePoll::Pending { next_in: None });
        }
        if self.emitted {
            return Ok(FramePoll::Ended);
        }
        self.emitted = true;
        Ok(FramePoll::Ready(self.frame.clone()))
    }

    fn pause(&mut self) -> Result<(), MediaError> {
        if self.paused {
            return Err(MediaError::InvalidState("已处于暂停状态".into()));
        }
        self.paused = true;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), MediaError> {
        if !self.paused {
            return Err(MediaError::InvalidState("未处于暂停状态".into()));
        }
        self.paused = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::types::{CpuFrame, MediaKind, PixelFormat};
    use std::sync::Arc;

    fn source() -> StaticFrameSource {
        let stride = 8 * 4;
        let frame = DisplayFrame::Cpu(CpuFrame {
            width: 8,
            height: 8,
            stride,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; (stride * 8) as usize].into_boxed_slice()),
        });
        let info = MediaInfo {
            kind: MediaKind::Image,
            width: 8,
            height: 8,
            duration: None,
            source_fps: None,
            codec: None,
        };
        StaticFrameSource::new(info, frame)
    }

    #[test]
    fn static_source_emits_once_then_ends() {
        let mut s = source();
        let now = Instant::now();
        match s.poll_frame(now).unwrap() {
            FramePoll::Ready(f) => assert_eq!(f.width(), 8),
            other => panic!("期望 Ready，实际 {other:?}"),
        }
        assert!(matches!(s.poll_frame(now).unwrap(), FramePoll::Ended));
    }

    #[test]
    fn pause_blocks_supply_until_resume() {
        let mut s = source();
        let now = Instant::now();
        s.pause().unwrap();
        assert!(matches!(
            s.poll_frame(now).unwrap(),
            FramePoll::Pending { next_in: None }
        ));
        s.resume().unwrap();
        assert!(matches!(s.poll_frame(now).unwrap(), FramePoll::Ready(_)));
    }

    #[test]
    fn double_pause_and_unpaused_resume_are_rejected() {
        let mut s = source();
        s.pause().unwrap();
        assert!(matches!(s.pause(), Err(MediaError::InvalidState(_))));
        s.resume().unwrap();
        assert!(matches!(s.resume(), Err(MediaError::InvalidState(_))));
    }

    #[test]
    fn metadata_is_reported() {
        let s = source();
        let meta = s.metadata();
        assert_eq!(meta.kind, MediaKind::Image);
        assert_eq!(meta.width, 8);
    }
}
