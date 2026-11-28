//! CPU 光栅化：把 RGBA 解码帧按 [`FitParams`] 变换到输出尺寸的扫描缓冲。
//!
//! M1 正确性优先路径（规范 §5.2）：双线性缩放 + 裁切/留白；
//! GPU shader 路径属 M4。输出为 [`PixelFormat::Xrgb8888`]（内存
//! `[B, G, R, X]`），可直接写入 DRM dumb buffer。
//!
//! 纯函数实现，无硬件即可验证任意比例（含 21:9、32:9、4K、非整数比率）。

use crate::RenderError;
use liuguang_core::fit::{FitParams, compute_fit};
use liuguang_core::types::{CpuFrame, PixelFormat};
use std::sync::Arc;

/// 输出扫描缓冲的最大字节数（防止畸形输出尺寸耗尽内存）。
pub const MAX_OUTPUT_BYTES: usize = 512 * 1024 * 1024;

/// 将 `src`（RGBA8888）按 `params` 渲染到 `output` 尺寸的 XRGB8888 缓冲。
///
/// 目标矩形之外（contain 留白、center 溢出裁切留白）填充 `background`
/// （RGB 顺序）。1:1 映射时逐像素精确复制。
///
/// # Errors
///
/// 源格式不支持、输出尺寸非法、fit 计算失败或输出缓冲超预算时返回
/// [`RenderError`]。
pub fn render_fit(
    src: &CpuFrame,
    output: (u32, u32),
    params: &FitParams,
    background: [u8; 3],
) -> Result<CpuFrame, RenderError> {
    // NV12 需硬件平面采样 + 硬件 YUV→RGB（不走 CPU 光栅化）；
    // 现有 `!= Rgba8888` 检查已覆盖，这里只需保证语义显式。
    if src.format != PixelFormat::Rgba8888 {
        return Err(RenderError::UnsupportedSourceFormat { format: src.format });
    }
    src.validate().map_err(RenderError::InvalidFrame)?;

    let (ow, oh) = output;
    if ow == 0 || oh == 0 {
        return Err(RenderError::InvalidOutput {
            width: ow,
            height: oh,
        });
    }
    let stride = ow.checked_mul(4).ok_or(RenderError::OutputTooLarge {
        needed: u64::MAX,
        limit: MAX_OUTPUT_BYTES as u64,
    })?;
    let needed = (stride as usize)
        .checked_mul(oh as usize)
        .ok_or(RenderError::OutputTooLarge {
            needed: u64::MAX,
            limit: MAX_OUTPUT_BYTES as u64,
        })?;
    if needed > MAX_OUTPUT_BYTES {
        return Err(RenderError::OutputTooLarge {
            needed: needed as u64,
            limit: MAX_OUTPUT_BYTES as u64,
        });
    }

    let fit = compute_fit((src.width, src.height), output, params)?;
    let mut out = vec![0u8; needed];

    let bg_b = background[2];
    let bg_g = background[1];
    let bg_r = background[0];

    for y in 0..oh {
        let row = (y as usize) * (stride as usize);
        for x in 0..ow {
            // 留白判断基于目标矩形（输出像素中心是否落在矩形内）。
            let px = f64::from(x) + 0.5;
            let py = f64::from(y) + 0.5;
            let in_rect = px >= fit.dst_x
                && px <= fit.dst_x + fit.dst_width
                && py >= fit.dst_y
                && py <= fit.dst_y + fit.dst_height;
            let (r, g, b) = if in_rect {
                // 目标像素中心映射回源坐标（像素索引空间），边缘钳制。
                let sx =
                    ((px - fit.dst_x) / fit.scale_x - 0.5).clamp(0.0, f64::from(src.width - 1));
                let sy =
                    ((py - fit.dst_y) / fit.scale_y - 0.5).clamp(0.0, f64::from(src.height - 1));
                sample_bilinear(src, sx, sy)
            } else {
                (bg_r, bg_g, bg_b)
            };
            // XRGB8888 内存布局：[B, G, R, X]
            out[row + x as usize * 4] = b;
            out[row + x as usize * 4 + 1] = g;
            out[row + x as usize * 4 + 2] = r;
            out[row + x as usize * 4 + 3] = 0;
        }
    }

    Ok(CpuFrame {
        width: ow,
        height: oh,
        stride,
        format: PixelFormat::Xrgb8888,
        data: Arc::from(out.into_boxed_slice()),
    })
}

/// 双线性采样（源为 RGBA8888），返回 RGB。
fn sample_bilinear(src: &CpuFrame, sx: f64, sy: f64) -> (u8, u8, u8) {
    let x0 = sx.floor();
    let y0 = sy.floor();
    let fx = (sx - x0) as f32;
    let fy = (sy - y0) as f32;
    let x0i = (x0 as u32).min(src.width - 1);
    let y0i = (y0 as u32).min(src.height - 1);
    let x1i = (x0i + 1).min(src.width - 1);
    let y1i = (y0i + 1).min(src.height - 1);

    let p = |xi: u32, yi: u32| -> (f32, f32, f32) {
        let off = (yi as usize) * (src.stride as usize) + (xi as usize) * 4;
        (
            f32::from(src.data[off]),
            f32::from(src.data[off + 1]),
            f32::from(src.data[off + 2]),
        )
    };
    let (r00, g00, b00) = p(x0i, y0i);
    let (r10, g10, b10) = p(x1i, y0i);
    let (r01, g01, b01) = p(x0i, y1i);
    let (r11, g11, b11) = p(x1i, y1i);

    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let top = (lerp(r00, r10, fx), lerp(g00, g10, fx), lerp(b00, b10, fx));
    let bot = (lerp(r01, r11, fx), lerp(g01, g11, fx), lerp(b01, b11, fx));
    (
        lerp(top.0, bot.0, fy).round().clamp(0.0, 255.0) as u8,
        lerp(top.1, bot.1, fy).round().clamp(0.0, 255.0) as u8,
        lerp(top.2, bot.2, fy).round().clamp(0.0, 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::fit::FitMode;

    fn rgba_frame(width: u32, height: u32, fill: [u8; 4]) -> CpuFrame {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&fill);
        }
        CpuFrame {
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    fn rgba_pixel(frame: &CpuFrame, x: u32, y: u32) -> [u8; 4] {
        let off = (y * frame.stride + x * 4) as usize;
        [
            frame.data[off],
            frame.data[off + 1],
            frame.data[off + 2],
            frame.data[off + 3],
        ]
    }

    fn xrgb_pixel(frame: &CpuFrame, x: u32, y: u32) -> [u8; 3] {
        let off = (y * frame.stride + x * 4) as usize;
        // 内存 [B,G,R,X] → 返回 RGB
        [frame.data[off + 2], frame.data[off + 1], frame.data[off]]
    }

    fn default_params(mode: FitMode) -> FitParams {
        FitParams {
            mode,
            ..FitParams::default()
        }
    }

    #[test]
    fn one_to_one_copy_is_exact() {
        let mut data = Vec::new();
        for i in 0..4u8 {
            data.extend_from_slice(&[10 * i, 20 * i, 30 * i, 255]);
        }
        let src = CpuFrame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data.into_boxed_slice()),
        };
        let out = render_fit(&src, (2, 2), &default_params(FitMode::Cover), [0, 0, 0]).unwrap();
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let s = rgba_pixel(&src, x, y);
            let d = xrgb_pixel(&out, x, y);
            assert_eq!([d[0], d[1], d[2]], [s[0], s[1], s[2]], "像素 {x},{y}");
        }
        assert_eq!(out.format, PixelFormat::Xrgb8888);
        assert_eq!(out.stride, 8);
    }

    #[test]
    fn solid_color_fills_cover() {
        let src = rgba_frame(3, 5, [255, 128, 64, 255]);
        let out = render_fit(&src, (20, 20), &default_params(FitMode::Cover), [0, 0, 0]).unwrap();
        for y in 0..20 {
            for x in 0..20 {
                assert_eq!(xrgb_pixel(&out, x, y), [255, 128, 64]);
            }
        }
    }

    #[test]
    fn contain_letterbox_uses_background() {
        // 2:1 源 → 1:1 输出 contain：上下留白。
        let src = rgba_frame(4, 2, [0, 255, 0, 255]);
        let out = render_fit(&src, (8, 8), &default_params(FitMode::Contain), [1, 2, 3]).unwrap();
        // 顶行是背景。
        assert_eq!(xrgb_pixel(&out, 0, 0), [1, 2, 3]);
        // 中间行是内容。
        assert_eq!(xrgb_pixel(&out, 4, 4), [0, 255, 0]);
        // 底行是背景。
        assert_eq!(xrgb_pixel(&out, 7, 7), [1, 2, 3]);
    }

    #[test]
    fn stretch_fills_without_background() {
        let src = rgba_frame(4, 2, [9, 9, 9, 255]);
        let out = render_fit(&src, (10, 10), &default_params(FitMode::Stretch), [7, 7, 7]).unwrap();
        for y in 0..10 {
            for x in 0..10 {
                assert_eq!(xrgb_pixel(&out, x, y), [9, 9, 9], "{x},{y}");
            }
        }
    }

    #[test]
    fn center_crops_overflow_with_background_padding() {
        // 8x8 源 → 4x4 输出 center：只显示中心 4x4，四周无留白。
        let mut src = rgba_frame(8, 8, [0, 0, 0, 255]);
        // 给中心 4x4 填色
        {
            let data = std::sync::Arc::get_mut(&mut src.data).unwrap();
            for y in 2..6 {
                for x in 2..6 {
                    let off = (y * src.stride + x * 4) as usize;
                    data[off] = 200;
                    data[off + 1] = 100;
                    data[off + 2] = 50;
                }
            }
        }
        let out = render_fit(&src, (4, 4), &default_params(FitMode::Center), [0, 0, 0]).unwrap();
        // center 不缩放：输出 4x4 应映射源中心 4x4（源坐标 2..6）
        assert_eq!(xrgb_pixel(&out, 0, 0), [200, 100, 50]);
        assert_eq!(xrgb_pixel(&out, 3, 3), [200, 100, 50]);

        // 小输出 + 大偏移 → 部分区域落到背景。
        let params = FitParams {
            mode: FitMode::Center,
            offset_x: 100.0,
            offset_y: 100.0,
            ..FitParams::default()
        };
        let out = render_fit(&src, (4, 4), &params, [11, 22, 33]).unwrap();
        assert_eq!(xrgb_pixel(&out, 0, 0), [11, 22, 33]);
    }

    #[test]
    fn bilinear_blends_between_pixels() {
        // 2x1：左黑右白，放大到 2x1 不变；用 zoom 放大 4x1 观察中间渐变。
        let mut data = vec![0u8; 8];
        data[0..4].copy_from_slice(&[0, 0, 0, 255]);
        data[4..8].copy_from_slice(&[255, 255, 255, 255]);
        let src = CpuFrame {
            width: 2,
            height: 1,
            stride: 8,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data.into_boxed_slice()),
        };
        let out = render_fit(&src, (4, 1), &default_params(FitMode::Stretch), [0, 0, 0]).unwrap();
        let left = xrgb_pixel(&out, 0, 0);
        let mid_l = xrgb_pixel(&out, 1, 0);
        let mid_r = xrgb_pixel(&out, 2, 0);
        let right = xrgb_pixel(&out, 3, 0);
        assert_eq!(left[0], 0);
        assert_eq!(right[0], 255);
        assert!(mid_l[0] > 0 && mid_l[0] < 255, "中间应有混合: {}", mid_l[0]);
        assert!(mid_r[0] > mid_l[0], "渐变应单调: {mid_l:?} vs {mid_r:?}");
    }

    #[test]
    fn rejects_bad_input() {
        let src = rgba_frame(4, 4, [0, 0, 0, 255]);
        assert!(matches!(
            render_fit(&src, (0, 10), &default_params(FitMode::Cover), [0, 0, 0]),
            Err(RenderError::InvalidOutput { .. })
        ));

        let wrong_format = CpuFrame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; 16].into_boxed_slice()),
        };
        assert!(matches!(
            render_fit(
                &wrong_format,
                (2, 2),
                &default_params(FitMode::Cover),
                [0, 0, 0]
            ),
            Err(RenderError::UnsupportedSourceFormat { .. })
        ));
    }
}
