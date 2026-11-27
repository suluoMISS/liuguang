//! 帧旋转变换（M6）：纯像素几何变换。
//!
//! 顺时针旋转 [`CpuFrame`]，支持 [`PixelFormat::Rgba8888`] 与
//! [`PixelFormat::Xrgb8888`]（以及同为 4 字节/像素的 [`PixelFormat::Argb8888`]）：
//! **字节序含义不变，仅做几何变换**。正确处理源 stride（含对齐填充）；
//! [`Rotation::Deg0`] 直接克隆（共享像素缓冲）。输出 stride 为紧凑
//! `新宽 × 每像素字节数`。
//!
//! 旋转语义：顺时针 90°/180°/270°。例如 2×3 源
//! ```text
//! a b      e c a
//! c d  →   f d b   （顺时针 90°，输出 3×2）
//! e f
//! ```

use crate::types::{CpuFrame, FrameError};
use std::sync::Arc;

/// 输出旋转角度（顺时针）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Rotation {
    /// 不旋转（默认）。
    #[default]
    #[serde(rename = "0")]
    Deg0,
    /// 顺时针 90°。
    #[serde(rename = "90")]
    Deg90,
    /// 180°。
    #[serde(rename = "180")]
    Deg180,
    /// 顺时针 270°（逆时针 90°）。
    #[serde(rename = "270")]
    Deg270,
}

impl Rotation {
    /// 顺时针四分之一圈数（0/1/2/3）。
    pub const fn quarter_turns(self) -> u32 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 1,
            Self::Deg180 => 2,
            Self::Deg270 => 3,
        }
    }

    /// 旋转后的尺寸；90°/270° 交换宽高。
    pub fn rotated_size(self, size: (u32, u32)) -> (u32, u32) {
        match self {
            Self::Deg0 | Self::Deg180 => size,
            Self::Deg90 | Self::Deg270 => (size.1, size.0),
        }
    }
}

/// 把源像素 `(x, y)` 映射到旋转后目标像素 `(dx, dy)`。
/// `sw`/`sh` 为源宽高。
fn map_pixel(rotation: Rotation, x: usize, y: usize, sw: usize, sh: usize) -> (usize, usize) {
    match rotation {
        Rotation::Deg0 => (x, y),
        Rotation::Deg90 => (sh - 1 - y, x),
        Rotation::Deg180 => (sw - 1 - x, sh - 1 - y),
        Rotation::Deg270 => (y, sw - 1 - x),
    }
}

/// 顺时针旋转帧（纯几何变换）。
///
/// - [`Rotation::Deg0`]：返回 `src.clone()`（共享像素数据）；
/// - 其余角度：输出宽高为 [`Rotation::rotated_size`]，stride 为紧凑布局；
/// - 像素格式与字节序含义不变。
///
/// # Errors
///
/// 源帧结构非法（宽高为 0、stride 过小、数据不足）时返回 [`FrameError`]。
pub fn rotate_frame(src: &CpuFrame, rotation: Rotation) -> Result<CpuFrame, FrameError> {
    src.validate()?;
    if rotation == Rotation::Deg0 {
        return Ok(src.clone());
    }
    let bpp = src.format.bytes_per_pixel() as usize;
    let sw = src.width as usize;
    let sh = src.height as usize;
    let src_stride = src.stride as usize;
    let (out_w, out_h) = rotation.rotated_size((src.width, src.height));
    let (dw, dh) = (out_w as usize, out_h as usize);
    let dst_stride = dw * bpp;

    let mut out = vec![0u8; dst_stride * dh];
    for y in 0..sh {
        let row_start = y * src_stride;
        for x in 0..sw {
            let src_off = row_start + x * bpp;
            let (dx, dy) = map_pixel(rotation, x, y, sw, sh);
            let dst_off = dy * dst_stride + dx * bpp;
            out[dst_off..dst_off + bpp].copy_from_slice(&src.data[src_off..src_off + bpp]);
        }
    }

    Ok(CpuFrame {
        width: out_w,
        height: out_h,
        stride: dst_stride as u32,
        format: src.format,
        data: Arc::from(out),
    })
}

/// 亮度合法范围（`1.0` = 原始亮度；只压暗保护桌面可读性，避免画面过曝）。
pub const MIN_BRIGHTNESS: f64 = 0.1;
/// 亮度上限（`1.0` = 原图，不做过曝放大）。
pub const MAX_BRIGHTNESS: f64 = 1.0;

/// 运行期渲染变换（几何适配 + 旋转 + 亮度），GPU 与 CPU 臂共用同一组语义。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderTransform {
    /// 适配模式（`cover`/`contain`/`stretch`/`center`）。
    pub fit: crate::fit::FitMode,
    /// 用户缩放倍数。
    pub zoom: f64,
    /// 水平偏移（输出像素）。
    pub offset_x: f64,
    /// 垂直偏移（输出像素）。
    pub offset_y: f64,
    /// 顺时针旋转角度。
    pub rotation: Rotation,
    /// 亮度倍率（`0.1..=1.0`，`1.0` = 原图）。
    pub brightness: f64,
}

impl Default for RenderTransform {
    fn default() -> Self {
        Self {
            fit: crate::fit::FitMode::Cover,
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            rotation: Rotation::Deg0,
            brightness: 1.0,
        }
    }
}

/// 按 `brightness`（`0.1..=1.5`）线性缩放 CPU 帧的 RGB 通道，保留 alpha / padding。
///
/// `(brightness - 1.0).abs() < 1e-4` 时零拷贝返回 `src.clone()`。
///
/// # Errors
///
/// 源帧结构非法时返回 [`FrameError`]。
pub fn scale_brightness(src: &CpuFrame, brightness: f64) -> Result<CpuFrame, FrameError> {
    src.validate()?;
    if (brightness - 1.0).abs() < 1e-4 {
        return Ok(src.clone());
    }
    let factor = brightness.clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS);
    let mut lut = [0u8; 256];
    for (idx, slot) in lut.iter_mut().enumerate() {
        *slot = (f64::from(idx as u8) * factor).round().clamp(0.0, 255.0) as u8;
    }

    let bpp = src.format.bytes_per_pixel() as usize;
    let w = src.width as usize;
    let h = src.height as usize;
    let src_stride = src.stride as usize;
    let dst_stride = w * bpp;
    let mut out = vec![0u8; dst_stride * h];
    for y in 0..h {
        let src_row = y * src_stride;
        let dst_row = y * dst_stride;
        for x in 0..w {
            let s = src_row + x * bpp;
            let d = dst_row + x * bpp;
            out[d] = lut[usize::from(src.data[s])];
            out[d + 1] = lut[usize::from(src.data[s + 1])];
            out[d + 2] = lut[usize::from(src.data[s + 2])];
            out[d + 3] = src.data[s + 3];
        }
    }

    Ok(CpuFrame {
        width: src.width,
        height: src.height,
        stride: dst_stride as u32,
        format: src.format,
        data: Arc::from(out),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::types::PixelFormat;

    /// 构造测试帧：像素按行主序给出，`stride` 可大于 `width * 4`（填充 0xFF）。
    fn frame(width: u32, height: u32, stride: u32, pixels: &[[u8; 4]]) -> CpuFrame {
        let mut data = vec![0xFFu8; (stride * height) as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let px = pixels[y * width as usize + x];
                let off = y * stride as usize + x * 4;
                data[off..off + 4].copy_from_slice(&px);
            }
        }
        CpuFrame {
            width,
            height,
            stride,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data),
        }
    }

    fn pixel(frame: &CpuFrame, x: u32, y: u32) -> [u8; 4] {
        let off = (y * frame.stride + x * 4) as usize;
        frame.data[off..off + 4].try_into().unwrap()
    }

    /// 2×3 参考帧：
    /// ```text
    /// a b
    /// c d
    /// e f
    /// ```
    fn ref_pixels() -> [[u8; 4]; 6] {
        [
            [10, 0, 0, 255], // a
            [20, 0, 0, 255], // b
            [30, 0, 0, 255], // c
            [40, 0, 0, 255], // d
            [50, 0, 0, 255], // e
            [60, 0, 0, 255], // f
        ]
    }

    #[test]
    fn quarter_turns_and_rotated_size() {
        assert_eq!(Rotation::Deg0.quarter_turns(), 0);
        assert_eq!(Rotation::Deg90.quarter_turns(), 1);
        assert_eq!(Rotation::Deg180.quarter_turns(), 2);
        assert_eq!(Rotation::Deg270.quarter_turns(), 3);
        assert_eq!(Rotation::Deg0.rotated_size((100, 50)), (100, 50));
        assert_eq!(Rotation::Deg90.rotated_size((100, 50)), (50, 100));
        assert_eq!(Rotation::Deg180.rotated_size((100, 50)), (100, 50));
        assert_eq!(Rotation::Deg270.rotated_size((100, 50)), (50, 100));
    }

    #[test]
    fn deg0_clones_shared_buffer() {
        let src = frame(2, 3, 8, &ref_pixels());
        let out = rotate_frame(&src, Rotation::Deg0).unwrap();
        assert_eq!(out.width, src.width);
        assert_eq!(out.height, src.height);
        assert_eq!(out.stride, src.stride);
        assert!(Arc::ptr_eq(&src.data, &out.data), "Deg0 应共享像素缓冲");
    }

    #[test]
    fn deg90_rotates_clockwise_with_padded_stride() {
        let src = frame(2, 3, 8, &ref_pixels()); // stride 8 > 2*4：含填充
        let out = rotate_frame(&src, Rotation::Deg90).unwrap();
        assert_eq!((out.width, out.height), (3, 2));
        assert_eq!(out.stride, 12);
        // 期望 3×2：
        // e c a
        // f d b
        assert_eq!(pixel(&out, 0, 0), [50, 0, 0, 255]); // e
        assert_eq!(pixel(&out, 1, 0), [30, 0, 0, 255]); // c
        assert_eq!(pixel(&out, 2, 0), [10, 0, 0, 255]); // a
        assert_eq!(pixel(&out, 0, 1), [60, 0, 0, 255]); // f
        assert_eq!(pixel(&out, 1, 1), [40, 0, 0, 255]); // d
        assert_eq!(pixel(&out, 2, 1), [20, 0, 0, 255]); // b
    }

    #[test]
    fn deg180_rotates_with_padded_stride() {
        let src = frame(2, 3, 12, &ref_pixels()); // 更宽的填充
        let out = rotate_frame(&src, Rotation::Deg180).unwrap();
        assert_eq!((out.width, out.height), (2, 3));
        // 期望 2×3：
        // f e
        // d c
        // b a
        assert_eq!(pixel(&out, 0, 0), [60, 0, 0, 255]); // f
        assert_eq!(pixel(&out, 1, 0), [50, 0, 0, 255]); // e
        assert_eq!(pixel(&out, 0, 1), [40, 0, 0, 255]); // d
        assert_eq!(pixel(&out, 1, 1), [30, 0, 0, 255]); // c
        assert_eq!(pixel(&out, 0, 2), [20, 0, 0, 255]); // b
        assert_eq!(pixel(&out, 1, 2), [10, 0, 0, 255]); // a
    }

    #[test]
    fn deg270_rotates_clockwise_with_padded_stride() {
        let src = frame(2, 3, 8, &ref_pixels());
        let out = rotate_frame(&src, Rotation::Deg270).unwrap();
        assert_eq!((out.width, out.height), (3, 2));
        // 期望 3×2（顺时针 270°）：
        // b d f
        // a c e
        assert_eq!(pixel(&out, 0, 0), [20, 0, 0, 255]); // b
        assert_eq!(pixel(&out, 1, 0), [40, 0, 0, 255]); // d
        assert_eq!(pixel(&out, 2, 0), [60, 0, 0, 255]); // f
        assert_eq!(pixel(&out, 0, 1), [10, 0, 0, 255]); // a
        assert_eq!(pixel(&out, 1, 1), [30, 0, 0, 255]); // c
        assert_eq!(pixel(&out, 2, 1), [50, 0, 0, 255]); // e
    }

    #[test]
    fn four_turns_compose_to_identity() {
        let src = frame(2, 3, 8, &ref_pixels());
        let mut cur = src.clone();
        for _ in 0..4 {
            cur = rotate_frame(&cur, Rotation::Deg90).unwrap();
        }
        assert_eq!(cur.width, src.width);
        assert_eq!(cur.height, src.height);
        for y in 0..3 {
            for x in 0..2 {
                assert_eq!(pixel(&cur, x, y), pixel(&src, x, y), "({x},{y})");
            }
        }
    }

    #[test]
    fn xrgb_format_rotates_and_keeps_bytes() {
        let mut src = frame(
            2,
            2,
            8,
            &[
                [1, 2, 3, 4],
                [5, 6, 7, 8],
                [9, 10, 11, 12],
                [13, 14, 15, 16],
            ],
        );
        src.format = PixelFormat::Xrgb8888;
        let out = rotate_frame(&src, Rotation::Deg90).unwrap();
        assert_eq!(out.format, PixelFormat::Xrgb8888);
        // 源（行主序）：
        // [1..4]   [5..8]
        // [9..12]  [13..16]
        // 顺时针 90°：
        // [9..12]  [1..4]
        // [13..16] [5..8]
        assert_eq!(pixel(&out, 0, 0), [9, 10, 11, 12]);
        assert_eq!(pixel(&out, 1, 0), [1, 2, 3, 4]);
        assert_eq!(pixel(&out, 0, 1), [13, 14, 15, 16]);
        assert_eq!(pixel(&out, 1, 1), [5, 6, 7, 8]);
    }

    #[test]
    fn invalid_source_frames_are_rejected() {
        let mut bad = frame(2, 2, 8, &[[0, 0, 0, 0]; 4]);
        bad.width = 0;
        assert!(matches!(
            rotate_frame(&bad, Rotation::Deg90),
            Err(FrameError::ZeroDimension)
        ));

        let mut bad = frame(2, 2, 8, &[[0, 0, 0, 0]; 4]);
        bad.stride = 4; // < 2*4
        assert!(matches!(
            rotate_frame(&bad, Rotation::Deg90),
            Err(FrameError::StrideTooSmall { .. })
        ));

        let mut bad = frame(2, 2, 8, &[[0, 0, 0, 0]; 4]);
        bad.data = Arc::from(vec![0u8; 8]); // < stride * height
        assert!(matches!(
            rotate_frame(&bad, Rotation::Deg90),
            Err(FrameError::DataTooShort { .. })
        ));
    }

    #[test]
    fn rotation_serde_uses_degrees_strings() {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Wrapper {
            rotation: Rotation,
        }

        let w: Wrapper = toml::from_str("rotation = \"90\"").unwrap();
        assert_eq!(w.rotation, Rotation::Deg90);
        let w: Wrapper = toml::from_str("rotation = \"0\"").unwrap();
        assert_eq!(w.rotation, Rotation::Deg0);
        let w: Wrapper = toml::from_str("rotation = \"180\"").unwrap();
        assert_eq!(w.rotation, Rotation::Deg180);
        let w: Wrapper = toml::from_str("rotation = \"270\"").unwrap();
        assert_eq!(w.rotation, Rotation::Deg270);
        assert!(toml::from_str::<Wrapper>("rotation = \"deg90\"").is_err());
        assert!(toml::from_str::<Wrapper>("rotation = 90").is_err());
        assert_eq!(
            toml::to_string(&Wrapper {
                rotation: Rotation::Deg180
            })
            .unwrap(),
            "rotation = \"180\"\n"
        );
        // 默认值为 0°。
        assert_eq!(Rotation::default(), Rotation::Deg0);
    }

    #[test]
    fn scale_brightness_dims_brightens_and_shares_identity() {
        let src = frame(2, 1, 8, &[[100, 150, 220, 255], [200, 210, 250, 128]]);
        let same = scale_brightness(&src, 1.0).unwrap();
        assert!(Arc::ptr_eq(&same.data, &src.data));

        let dim = scale_brightness(&src, 0.5).unwrap();
        assert_eq!(&dim.data[0..4], &[50, 75, 110, 255]);
        assert_eq!(&dim.data[4..8], &[100, 105, 125, 128]);

        // 超出上限 (> 1.0) 按 1.0 夹紧，防止过曝。
        let clamped = scale_brightness(&src, 1.5).unwrap();
        assert_eq!(&clamped.data[0..4], &[100, 150, 220, 255]);
        assert_eq!(&clamped.data[4..8], &[200, 210, 250, 128]);

        let mut xrgb = frame(1, 1, 4, &[[80, 120, 200, 0]]);
        xrgb.format = PixelFormat::Xrgb8888;
        let dim_x = scale_brightness(&xrgb, 0.25).unwrap();
        assert_eq!(&dim_x.data[0..4], &[20, 30, 50, 0]);
    }
}
