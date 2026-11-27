//! CPU 帧 → wl_shm 缓冲的像素写入。
//!
//! 两条路径：
//! 1. **identity**（帧尺寸 == 缓冲尺寸、矩形各自铺满、XRGB/Argb 字节序
//!    一致、stride 相同）：整行 `copy_from_slice`，视频 60fps 的热路径；
//!    daemon 侧 `render_fit` 已把帧预组合到模式尺寸，实际运行恒走这条。
//! 2. **fitted**：双线性重采样 `src`（16.16 定点）→ `dst_rect`（模式像素，
//!    可为负/越界，越界部分裁剪），`dst_rect` 之外整幅填黑。
//!
//! 格式映射（见 `liuguang-render` 与 daemon 的调用契约）：
//! - `Xrgb8888` / `Argb8888`：内存 `[B, G, R, X/A]`，与 wl_shm XRGB8888
//!   字节序一致，直拷；
//! - `Rgba8888`：内存 `[R, G, B, A]`，采样时交换 R/B；
//! - `Nv12`：CPU 光栅化路径不处理，返回 `Unsupported`。

use liuguang_core::types::{CpuFrame, FrameError, PixelFormat};
use liuguang_display::DisplayError;

/// 16.16 定点源矩形（帧内 x, y, w, h；daemon 语义）。
pub(crate) type SrcRect = (u64, u64, u64, u64);
/// 模式像素目标矩形（x, y 可为负，越界裁剪）。
pub(crate) type DstRect = (i64, i64, u64, u64);

/// 16.16 定点的小数精度（1 像素 = 65536）。
const FIXED: f64 = 65_536.0;

/// 把 CPU 帧写入 wl_shm 缓冲。
///
/// `dst_buf` 长度必须 ≥ `dst_stride * dst_h`；`src` 是 16.16 定点的
/// 帧内矩形（x, y, w, h）；`dst_rect` 是模式像素坐标的目标矩形
/// （x, y 可为负）。
///
/// # Errors
///
/// - 帧/矩形零尺寸 → [`DisplayError::InvalidFrame`]；
/// - NV12 帧 → [`DisplayError::Unsupported`]；
/// - dst_buf 长度不足 → [`DisplayError::Failed`]（本 crate 内部不变量，
///   出现即 bug）。
pub(crate) fn write_frame(
    dst_buf: &mut [u8],
    dst_stride: u32,
    dst_w: u32,
    dst_h: u32,
    frame: &CpuFrame,
    src: SrcRect,
    dst_rect: DstRect,
) -> Result<(), DisplayError> {
    let (src_x, src_y, src_w, src_h) = src;
    let (rect_x, rect_y, rect_w, rect_h) = dst_rect;
    frame.validate().map_err(DisplayError::InvalidFrame)?;

    if src_w == 0
        || src_h == 0
        || rect_w == 0
        || rect_h == 0
        || frame.width == 0
        || frame.height == 0
    {
        return Err(DisplayError::InvalidFrame(FrameError::ZeroDimension));
    }
    if !matches!(
        frame.format,
        PixelFormat::Xrgb8888 | PixelFormat::Argb8888 | PixelFormat::Rgba8888
    ) {
        return Err(DisplayError::Unsupported(
            "wl_shm 路径不支持该像素格式（仅 Xrgb8888/Argb8888/Rgba8888）".into(),
        ));
    }
    let need = dst_stride as u64 * dst_h as u64;
    if (dst_buf.len() as u64) < need {
        return Err(DisplayError::Failed {
            operation: "present",
            message: format!("wl_shm 缓冲长度 {} 不足 {}", dst_buf.len(), need),
        });
    }

    // identity 快路径：矩形各自铺满 + 尺寸/stride/字节序一致。
    let frame_w16 = u64::from(frame.width) << 16;
    let frame_h16 = u64::from(frame.height) << 16;
    if src == (0, 0, frame_w16, frame_h16)
        && dst_rect == (0, 0, u64::from(dst_w), u64::from(dst_h))
        && u64::from(frame.width) == u64::from(dst_w)
        && u64::from(frame.height) == u64::from(dst_h)
        && frame.stride == dst_stride
        && matches!(frame.format, PixelFormat::Xrgb8888 | PixelFormat::Argb8888)
    {
        let stride = dst_stride as usize;
        let rows = frame.height as usize;
        for row in 0..rows {
            let s = row * stride;
            dst_buf[s..s + stride].copy_from_slice(&frame.data[s..s + stride]);
        }
        return Ok(());
    }

    // 通用路径：先整幅填黑（含 stride 填充字节），再写可见矩形。
    dst_buf.fill(0);

    // 可见窗口 = dst_rect ∩ 缓冲（缓冲 dst_w × dst_h，模式像素）。
    let vis_x0 = (rect_x.max(0) as u64).min(u64::from(dst_w));
    let vis_y0 = (rect_y.max(0) as u64).min(u64::from(dst_h));
    let vis_x1 = rect_x.saturating_add(rect_w as i64).clamp(0, dst_w as i64) as u64;
    let vis_y1 = rect_y.saturating_add(rect_h as i64).clamp(0, dst_h as i64) as u64;

    let src_px_x = src_x as f64 / FIXED;
    let src_px_y = src_y as f64 / FIXED;
    let src_px_w = src_w as f64 / FIXED;
    let src_px_h = src_h as f64 / FIXED;

    let swizzle = frame.format == PixelFormat::Rgba8888;
    let frame_stride = frame.stride as usize;

    for py in vis_y0..vis_y1 {
        // rect 内的归一化纵坐标 → 源像素坐标（双线性四点）。
        let ty = ((f64::from(py as u32) + 0.5) - rect_y as f64) / rect_h as f64;
        let sy = src_px_y + ty * src_px_h - 0.5;
        for px in vis_x0..vis_x1 {
            let tx = ((f64::from(px as u32) + 0.5) - rect_x as f64) / rect_w as f64;
            let sx = src_px_x + tx * src_px_w - 0.5;
            let [b, g, r] = sample_bilinear(frame, frame_stride, swizzle, sx, sy);
            let off = py as usize * dst_stride as usize + px as usize * 4;
            dst_buf[off] = b;
            dst_buf[off + 1] = g;
            dst_buf[off + 2] = r;
            dst_buf[off + 3] = 0xFF;
        }
    }
    Ok(())
}

/// 双线性采样一个像素（源坐标以像素为单位，越界钳到边缘）。
fn sample_bilinear(
    frame: &CpuFrame,
    frame_stride: usize,
    swizzle: bool,
    sx: f64,
    sy: f64,
) -> [u8; 3] {
    let w_max = f64::from(frame.width - 1);
    let h_max = f64::from(frame.height - 1);
    let x0 = sx.clamp(0.0, w_max);
    let y0 = sy.clamp(0.0, h_max);
    let x1 = (x0 + 1.0).min(w_max);
    let y1 = (y0 + 1.0).min(h_max);
    let fx = (x0.fract()).clamp(0.0, 1.0);
    let fy = (y0.fract()).clamp(0.0, 1.0);
    let xi0 = x0.floor() as usize;
    let yi0 = y0.floor() as usize;
    let xi1 = x1.floor() as usize;
    let yi1 = y1.floor() as usize;

    let fetch = |xi: usize, yi: usize| -> [u8; 3] {
        let off = yi * frame_stride + xi * 4;
        let px = &frame.data[off..off + 4];
        if swizzle {
            // Rgba8888：内存 [R, G, B, A] → 返回 [B, G, R]
            [px[2], px[1], px[0]]
        } else {
            // Xrgb/Argb：内存 [B, G, R, X/A] → 原序
            [px[0], px[1], px[2]]
        }
    };

    let c00 = fetch(xi0, yi0);
    let c10 = fetch(xi1, yi0);
    let c01 = fetch(xi0, yi1);
    let c11 = fetch(xi1, yi1);
    let mut out = [0u8; 3];
    for c in 0..3 {
        let top = f64::from(c00[c]) * (1.0 - fx) + f64::from(c10[c]) * fx;
        let bottom = f64::from(c01[c]) * (1.0 - fx) + f64::from(c11[c]) * fx;
        out[c] = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::Arc;

    fn frame(w: u32, h: u32, format: PixelFormat, fill: [u8; 4]) -> CpuFrame {
        let stride = w * 4;
        let mut data = Vec::with_capacity((stride * h) as usize);
        for _ in 0..h * w {
            data.extend_from_slice(&fill);
        }
        CpuFrame {
            width: w,
            height: h,
            stride,
            format,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    #[test]
    fn identity_copies_full_frame() {
        let f = frame(4, 2, PixelFormat::Xrgb8888, [10, 20, 30, 0xFF]);
        let mut dst = vec![0u8; 4 * 4 * 2];
        write_frame(
            &mut dst,
            16,
            4,
            2,
            &f,
            (0, 0, 4 << 16, 2 << 16),
            (0, 0, 4, 2),
        )
        .unwrap();
        assert!(dst.chunks_exact(4).all(|p| p == [10, 20, 30, 0xFF]));
    }

    #[test]
    fn fitted_letterbox_leaves_black_border() {
        // 4x4 红帧，dst_rect 是中央 (1,1,2,2) → 边框应保持黑。
        let f = frame(4, 4, PixelFormat::Xrgb8888, [0, 0, 255, 0xFF]);
        let mut dst = vec![0u8; 4 * 4 * 4];
        write_frame(
            &mut dst,
            16,
            4,
            4,
            &f,
            (0, 0, 4 << 16, 4 << 16),
            (1, 1, 2, 2),
        )
        .unwrap();
        let px = |x: usize, y: usize| -> [u8; 4] {
            let o = y * 16 + x * 4;
            [dst[o], dst[o + 1], dst[o + 2], dst[o + 3]]
        };
        for y in 0..4 {
            for x in 0..4 {
                let p = px(x, y);
                if (1..3).contains(&x) && (1..3).contains(&y) {
                    assert_eq!(p, [0, 0, 255, 0xFF], "中央 ({x},{y}) 应为红");
                } else {
                    // XRGB 的 X 字节不参与显示，只断言 RGB 为黑。
                    assert_eq!(&p[..3], &[0, 0, 0], "边框 ({x},{y}) 应为黑");
                }
            }
        }
    }

    #[test]
    fn negative_dst_clamps_into_buffer() {
        // dst 从 (-1,-1) 铺 3x3：缓冲左上角覆盖帧对应右下 2x2 区域，
        // (0,0)~(1,1) 有颜色，其余黑。
        let f = frame(3, 3, PixelFormat::Xrgb8888, [7, 8, 9, 0xFF]);
        let mut dst = vec![0u8; 4 * 4 * 4];
        write_frame(
            &mut dst,
            16,
            4,
            4,
            &f,
            (0, 0, 3 << 16, 3 << 16),
            (-1, -1, 3, 3),
        )
        .unwrap();
        let o = 0;
        // (0,0) 对应帧 (1,1) → 同色（双线性落在整数中心）
        assert_eq!(&dst[o..o + 3], &[7, 8, 9]);
        // (3,3) 在可见窗外 → 黑
        let o2 = 3 * 16 + 3 * 4;
        assert_eq!(&dst[o2..o2 + 3], &[0, 0, 0]);
    }

    #[test]
    fn upscale_uses_nearest_at_exact_centers() {
        // 1x1 帧放大到 2x2：所有像素同色（双线性在整数中心退化）。
        let f = frame(1, 1, PixelFormat::Xrgb8888, [1, 2, 3, 0xFF]);
        // 2x2、stride 8 → 恰好 16 字节。
        let mut dst = vec![0u8; 8 * 2];
        write_frame(
            &mut dst,
            8,
            2,
            2,
            &f,
            (0, 0, 1 << 16, 1 << 16),
            (0, 0, 2, 2),
        )
        .unwrap();
        assert!(dst.chunks_exact(4).all(|p| p == [1, 2, 3, 0xFF]));
    }

    #[test]
    fn rgba_swaps_r_b() {
        // Rgba8888 红 [R=255,0,0,A] → XRGB 输出应为 [B=0, G=0, R=255, X]
        let f = frame(2, 1, PixelFormat::Rgba8888, [255, 0, 0, 255]);
        let mut dst = vec![0u8; 8];
        write_frame(
            &mut dst,
            8,
            2,
            1,
            &f,
            (0, 0, 2 << 16, 1 << 16),
            (0, 0, 2, 1),
        )
        .unwrap();
        assert_eq!(&dst[..3], &[0, 0, 255]);
    }

    #[test]
    fn nv12_is_rejected() {
        let f = CpuFrame {
            width: 4,
            height: 2,
            stride: 4,
            format: PixelFormat::Nv12,
            data: Arc::from(vec![0u8; 4 * 2 * 3 / 2].into_boxed_slice()),
        };
        let mut dst = vec![0u8; 8];
        let err = write_frame(
            &mut dst,
            8,
            2,
            1,
            &f,
            (0, 0, 4 << 16, 2 << 16),
            (0, 0, 2, 1),
        )
        .unwrap_err();
        assert!(matches!(err, DisplayError::Unsupported(_)), "{err}");
    }

    #[test]
    fn zero_dimensions_are_invalid() {
        let f = frame(2, 1, PixelFormat::Xrgb8888, [0; 4]);
        let mut dst = vec![0u8; 8];
        let err = write_frame(&mut dst, 8, 2, 1, &f, (0, 0, 0, 0), (0, 0, 2, 1)).unwrap_err();
        assert!(matches!(err, DisplayError::InvalidFrame(_)), "{err}");
    }

    #[test]
    fn short_buffer_is_reported_not_sliced() {
        let f = frame(2, 1, PixelFormat::Xrgb8888, [9; 4]);
        let mut dst = vec![0u8; 3];
        let err = write_frame(
            &mut dst,
            8,
            2,
            1,
            &f,
            (0, 0, 2 << 16, 1 << 16),
            (0, 0, 2, 1),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                DisplayError::Failed { .. } | DisplayError::InvalidFrame(_)
            ),
            "{err}"
        );
    }
}

/// 把整帧**原样**拷进 wl_shm 槽（不做任何重采样）。
///
/// 这是"上传原始尺寸 + 让合成器缩放"快路径的写槽步骤：目标格式必须与
/// 帧内存字节序一致（`Rgba8888` 配 `wl_shm::Format::Abgr8888`、
/// `Xrgb8888` 配 `Xrgb8888`），于是每帧成本 = 一次整帧内存拷贝，缩放与
/// 双线性交给合成器的 GPU。
///
/// 取舍：同一条 1080p→2560x1600 的覆盖，CPU 逐像素双线性实测 ≈105–290ms/帧
/// （release，跑满一个核），整帧拷贝 ≈2ms——壁纸守护进程的立身之本就是低占用，
/// 因此覆盖整屏的常规场景走这里；需要留黑边（Fit）或矩形不铺满时仍走
/// [`write_frame`] 的 CPU 路径。
///
/// # Errors
///
/// 帧数据长度不足（`frame.validate()`）→ [`DisplayError::InvalidFrame`]；
/// 目标缓冲装不下 → [`DisplayError::Failed`]。
pub(crate) fn upload_frame(
    dst_buf: &mut [u8],
    dst_stride: u32,
    frame: &CpuFrame,
) -> Result<(), DisplayError> {
    frame.validate().map_err(DisplayError::InvalidFrame)?;
    let src_stride = frame.stride;
    let row_bytes = (frame.width as usize).saturating_mul(4);
    if dst_stride < frame.width.saturating_mul(4) {
        return Err(DisplayError::Failed {
            operation: "upload",
            message: format!("wl_shm 缓冲 stride {dst_stride} 小于帧宽 {}", frame.width),
        });
    }
    let need = dst_stride as usize * frame.height as usize;
    if dst_buf.len() < need {
        return Err(DisplayError::Failed {
            operation: "upload",
            message: format!("wl_shm 缓冲不足：需要 {need}，实际 {}", dst_buf.len()),
        });
    }
    for y in 0..frame.height as usize {
        let src_off = y * src_stride as usize;
        let dst_off = y * dst_stride as usize;
        dst_buf[dst_off..dst_off + row_bytes]
            .copy_from_slice(&frame.data[src_off..src_off + row_bytes]);
    }
    Ok(())
}

#[cfg(test)]
mod upload_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use liuguang_core::types::PixelFormat;
    use std::sync::Arc;

    fn frame(w: u32, h: u32, seed: u8) -> CpuFrame {
        let data: Vec<u8> = (0..(w * h * 4))
            .map(|i| seed.wrapping_add(i as u8))
            .collect();
        CpuFrame {
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    #[test]
    fn upload_copies_every_row_verbatim() {
        let f = frame(4, 3, 7);
        let mut dst = vec![0u8; 4 * 3 * 4];
        upload_frame(&mut dst, 16, &f).unwrap();
        // 快路径不重采样：目标与源必须逐字节相同（含 alpha）。
        assert_eq!(&dst[..], &f.data[..]);
    }

    #[test]
    fn upload_respects_wider_destination_stride() {
        let f = frame(2, 2, 0x40);
        // 目标行距 16 字节（2 像素 + 8 字节填充），必须只写前 8 字节。
        let mut dst = vec![0xAAu8; 16 * 2];
        upload_frame(&mut dst, 16, &f).unwrap();
        assert_eq!(&dst[0..8], &f.data[0..8], "第一行");
        assert_eq!(&dst[16..24], &f.data[8..16], "第二行");
        assert_eq!(&dst[8..16], &[0xAA; 8], "行尾填充不应被踩");
    }

    #[test]
    fn upload_rejects_short_destination() {
        let f = frame(4, 4, 1);
        let mut narrow = vec![0u8; 4 * 4];
        assert!(matches!(
            upload_frame(&mut narrow, 16, &f),
            Err(DisplayError::Failed { .. })
        ));
    }

    #[test]
    fn upload_rejects_narrow_stride() {
        let f = frame(4, 1, 1);
        let mut dst = vec![0u8; 64];
        assert!(matches!(
            upload_frame(&mut dst, 8, &f),
            Err(DisplayError::Failed { .. })
        ));
    }

    #[test]
    fn upload_rejects_invalid_frame() {
        let mut f = frame(4, 1, 1);
        f.stride = 2; // stride < 宽度 × 4 ⇒ frame.validate() 失败
        let mut dst = vec![0u8; 64];
        assert!(matches!(
            upload_frame(&mut dst, 16, &f),
            Err(DisplayError::InvalidFrame(_))
        ));
    }
}
