//! 单帧缩略图抽取（**视频**用；图片由调用方用 gdk-pixbuf 直接解码）。
//!
//! 为什么放在 daemon 侧而不是 GUI 侧：
//!
//! 1. 解码器只在这里（`gstreamer` feature）；
//! 2. 媒体根边界校验（`media.allowed_roots` / `slideshow.directory`）只在这里
//!    ——GUI 拿到的路径不能无条件当文件读；
//! 3. GUI 进程不该为了一张封面拉起一条解码管线（它的崩溃面要尽量小）。
//!
//! 实现是**一次性的**：`filesrc ! decodebin ! videoconvert ! appsink`，
//! 拉到第一帧就拆管线。不追求「第 N 秒的画面」——壁纸封面用首帧足够，
//! 而且首帧最快、最不容易在损坏文件上卡住。

use std::path::Path;

use crate::MediaError;

/// 等首帧的上限。超时即认为该文件抽不出封面（损坏/无视频轨/解码器缺失）。
///
/// 只有 `gstreamer` 实现会用到；不带该 feature 时构造会让 dead_code 变噪音。
#[cfg(feature = "gstreamer")]
const PREROLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 抽出的缩略图（RGBA8，`width * height * 4` 字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// RGBA8 像素。
    pub rgba: Vec<u8>,
}

/// 从未超出这个尺寸的源图上抽帧后，最多运算多少像素。
///
/// 4K 源按 1/8 抽样仍然清晰（缩略图目标 320x200），而全尺寸双线性在这里毫无意义。
#[cfg(feature = "gstreamer")]
const MAX_SAMPLE_DIM: u32 = 1024;

#[cfg(feature = "gstreamer")]
mod imp {
    use std::path::Path;

    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_video as gst_video;

    use super::{MAX_SAMPLE_DIM, PREROLL_TIMEOUT, Thumbnail};
    use crate::MediaError;

    /// 抽首帧并缩放到不超过 `max_w` × `max_h`（保持宽高比）。
    pub fn extract(path: &Path, max_w: u32, max_h: u32) -> Result<Thumbnail, MediaError> {
        if max_w == 0 || max_h == 0 {
            return Err(MediaError::InvalidOptions("缩略图尺寸不能为 0".to_string()));
        }
        gst::init().map_err(|e| MediaError::Decode(format!("gst::init 失败: {e}")))?;
        let location = path.to_string_lossy().into_owned();
        // 与主播放管线同样的可靠性排序：软解（decodebin）优先，避免为了一张封面
        // 去赌硬解元素在当前 VA 驱动下是否注册得出来。
        let desc = "filesrc name=src ! decodebin ! videoconvert ! \
                    video/x-raw,format=RGBA ! appsink name=sink sync=false \
                    max-buffers=1 drop=true";
        let element = gst::parse::launch(desc)
            .map_err(|e| MediaError::Decode(format!("构建缩略图管线失败: {e}")))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| MediaError::Decode("管线不是 Pipeline".to_string()))?;
        let Some(src) = pipeline.by_name("src") else {
            return Err(MediaError::Decode("找不到 filesrc".to_string()));
        };
        src.set_property("location", &location);
        let Some(sink) = pipeline.by_name("sink") else {
            return Err(MediaError::Decode("找不到 appsink".to_string()));
        };

        // PLAYING 而不是 PAUSED：解码泵在 PAUSED 下要等 preroll 状态消息才出样本，
        // 而这里只需要「一帧」——直接让管线跑起来拉一帧更简单可靠（真机教训：
        // 用 try-pull-preroll 时 3s 都拿不到样本）。
        let bus = pipeline
            .bus()
            .ok_or_else(|| MediaError::Decode("管线没有 bus".to_string()))?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| MediaError::Decode(format!("管线进入 PLAYING 失败: {e}")))?;
        let deadline = std::time::Instant::now() + PREROLL_TIMEOUT;
        let sample = loop {
            // 每轮先看一眼总线：解码器缺失/文件损坏都会以 Error 消息报告，
            // 把它们翻译成人能读懂的原因，比笼统的超时有用得多。
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
                if let gst::MessageView::Error(err) = msg.view() {
                    let detail = err.error();
                    let debug = err.debug().map(|d| d.to_string()).unwrap_or_default();
                    let _ = pipeline.set_state(gst::State::Null);
                    return Err(MediaError::Decode(format!("解码失败: {detail}（{debug}）")));
                }
            }
            if let Some(sample) =
                sink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&0u64])
            {
                break sample;
            }
            if std::time::Instant::now() >= deadline {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(MediaError::Decode(
                    "抽取缩略图超时（3s 内没有解出首帧）".to_string(),
                ));
            }
        };
        let _ = pipeline.set_state(gst::State::Null);

        let caps = sample
            .caps()
            .ok_or_else(|| MediaError::Decode("样本没有 caps".to_string()))?;
        let info = gst_video::VideoInfo::from_caps(caps).map_err(|e| {
            MediaError::Decode(format!(
                "样本 caps 无法解析为视频格式: {e}（caps={caps:?}）"
            ))
        })?;
        if info.format() != gst_video::VideoFormat::Rgba {
            return Err(MediaError::Decode(format!(
                "解码结果不是 RGBA（实际 {:?}）",
                info.format()
            )));
        }
        let buffer = sample
            .buffer()
            .ok_or_else(|| MediaError::Decode("样本没有 buffer".to_string()))?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|e| MediaError::Decode(format!("映射视频帧失败: {e}")))?;
        let (width, height) = (info.width(), info.height());
        let stride = info
            .stride()
            .first()
            .copied()
            .and_then(|s| usize::try_from(s).ok())
            .filter(|s| *s >= width as usize * 4)
            .ok_or_else(|| MediaError::Decode("非法 RGBA 跨度".to_string()))?;
        let data = frame
            .plane_data(0)
            .map_err(|e| MediaError::Decode(format!("读取 RGBA 平面失败: {e}")))?;
        let row = width as usize * 4;
        let mut rgba = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = y * stride;
            let end = start + row;
            if end > data.len() {
                return Err(MediaError::Decode("RGBA 平面小于声明尺寸".to_string()));
            }
            rgba.extend_from_slice(&data[start..end]);
        }
        drop(frame);

        Ok(super::scale_to_fit(
            Thumbnail {
                width,
                height,
                rgba,
            },
            max_w.min(MAX_SAMPLE_DIM),
            max_h.min(MAX_SAMPLE_DIM),
        ))
    }
}

#[cfg(not(feature = "gstreamer"))]
mod imp {
    use std::path::Path;

    use super::Thumbnail;
    use crate::MediaError;

    /// 未编入 `gstreamer` feature 时无法抽帧。
    pub fn extract(_path: &Path, _max_w: u32, _max_h: u32) -> Result<Thumbnail, MediaError> {
        Err(MediaError::Decode(
            "视频缩略图需要 --features gstreamer 构建".to_string(),
        ))
    }
}

/// 抽首帧并缩放到不超过 `max_w` × `max_h`（保持宽高比）。
///
/// # Errors
///
/// 解码失败、超时、尺寸非法时返回 [`MediaError`]。
pub fn extract(path: &Path, max_w: u32, max_h: u32) -> Result<Thumbnail, MediaError> {
    imp::extract(path, max_w, max_h)
}

/// 等比缩放到不超过 `max_w` × `max_h`（**面积平均**，不是最近邻）。
///
/// 已小于目标框则原样返回。纯函数，便于单测（不依赖 GStreamer）。
#[must_use]
pub fn scale_to_fit(src: Thumbnail, max_w: u32, max_h: u32) -> Thumbnail {
    if max_w == 0 || max_h == 0 || src.width == 0 || src.height == 0 {
        return src;
    }
    let expected = (src.width as usize)
        .checked_mul(src.height as usize)
        .and_then(|px| px.checked_mul(4));
    if expected.is_none_or(|need| src.rgba.len() < need) {
        return src;
    }
    let scale = (max_w as f64 / f64::from(src.width)).min(max_h as f64 / f64::from(src.height));
    if scale >= 1.0 {
        return src;
    }
    let dst_w = ((f64::from(src.width) * scale).round() as u32).max(1);
    let dst_h = ((f64::from(src.height) * scale).round() as u32).max(1);
    let mut out = vec![0u8; dst_w as usize * dst_h as usize * 4];
    let src_w = u64::from(src.width);
    let src_h = u64::from(src.height);
    let dw = u64::from(dst_w);
    let dh = u64::from(dst_h);
    // 面积平均：每个目标像素覆盖一个源矩形，避免缩小后出现摩尔纹。
    for dy in 0..dh {
        let y0 = dy * src_h / dh;
        let y1 = ((dy + 1) * src_h).div_ceil(dh).min(src_h);
        for dx in 0..dw {
            let x0 = dx * src_w / dw;
            let x1 = ((dx + 1) * src_w).div_ceil(dw).min(src_w);
            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1.max(y0 + 1) {
                for sx in x0..x1.max(x0 + 1) {
                    let i = ((sy * src_w + sx) * 4) as usize;
                    for (c, acc) in sum.iter_mut().enumerate() {
                        *acc += u32::from(src.rgba[i + c]);
                    }
                    count += 1;
                }
            }
            let o = ((dy * dw + dx) * 4) as usize;
            for (c, acc) in sum.iter().enumerate() {
                out[o + c] = u8::try_from(acc / count.max(1)).unwrap_or(0);
            }
        }
    }
    Thumbnail {
        width: dst_w,
        height: dst_h,
        rgba: out,
    }
}

#[cfg(test)]
mod tests {
    use super::{Thumbnail, scale_to_fit};

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Thumbnail {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            data.extend_from_slice(&rgba);
        }
        Thumbnail {
            width: w,
            height: h,
            rgba: data,
        }
    }

    #[test]
    fn small_source_is_returned_unchanged() {
        let src = solid(10, 10, [1, 2, 3, 4]);
        let out = scale_to_fit(src.clone(), 320, 200);
        assert_eq!(out.width, 10);
        assert_eq!(out.height, 10);
        assert_eq!(out.rgba, src.rgba);
    }

    #[test]
    fn wide_source_is_letterboxed_to_width_limit() {
        let out = scale_to_fit(solid(1000, 100, [9, 9, 9, 255]), 320, 200);
        assert_eq!(out.width, 320);
        assert_eq!(out.height, 32);
        assert_eq!(out.rgba.len(), 320 * 32 * 4);
    }

    #[test]
    fn tall_source_is_limited_by_height() {
        let out = scale_to_fit(solid(100, 1000, [0, 0, 0, 0]), 320, 200);
        assert_eq!(out.width, 20);
        assert_eq!(out.height, 200);
    }

    #[test]
    fn downscale_preserves_uniform_colour() {
        let out = scale_to_fit(solid(640, 480, [200, 100, 50, 255]), 320, 200);
        assert_eq!((out.width, out.height), (267, 200));
        assert!(out.rgba.chunks_exact(4).all(|p| p == [200, 100, 50, 255]));
    }

    #[test]
    fn zero_target_is_ignored() {
        let out = scale_to_fit(solid(100, 100, [1, 1, 1, 1]), 0, 0);
        assert_eq!(out.width, 100);
    }
}
