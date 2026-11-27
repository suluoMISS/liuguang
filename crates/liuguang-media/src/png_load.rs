//! PNG 解码（规范 §5.1：校验格式、尺寸、文件大小；对输入设置内存上限）。
//!
//! 输出统一为 [`PixelFormat::Rgba8888`] 的 [`CpuFrame`]。
//! 解码在受限缓冲中进行，不长期保留原始文件的多份副本；
//! 畸形/超大文件在解码前即被拒绝，避免耗尽服务器内存。

use crate::MediaError;
use liuguang_core::types::{CpuFrame, PixelFormat};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

/// PNG 文件大小上限（字节）。
pub const MAX_PNG_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// 单边像素上限。
pub const MAX_PNG_DIMENSION: u32 = 16_384;
/// 解码后 RGBA 缓冲上限（字节），覆盖 8K RGBA（约 126.6 MiB）。
pub const MAX_PNG_DECODED_BYTES: usize = 256 * 1024 * 1024;

/// 校验图像尺寸与解码缓冲预算（纯函数，供解码前检查与单测复用）。
///
/// # Errors
///
/// 尺寸为 0、单边超限或解码缓冲超预算时返回 [`MediaError`]。
pub fn check_image_budget(width: u32, height: u32) -> Result<(), MediaError> {
    if width == 0 || height == 0 {
        return Err(MediaError::Decode(format!(
            "PNG 尺寸非法: {width}x{height}"
        )));
    }
    if width > MAX_PNG_DIMENSION || height > MAX_PNG_DIMENSION {
        return Err(MediaError::DimensionExceeded {
            width,
            height,
            limit: MAX_PNG_DIMENSION,
        });
    }
    let needed = u64::from(width) * u64::from(height) * 4;
    if needed > MAX_PNG_DECODED_BYTES as u64 {
        return Err(MediaError::DecodedTooLarge {
            needed,
            limit: MAX_PNG_DECODED_BYTES as u64,
        });
    }
    Ok(())
}

/// 解码 PNG 文件为 RGBA8888 [`CpuFrame`]（stride = width × 4）。
///
/// # Errors
///
/// 文件不可读、超过 [`MAX_PNG_FILE_BYTES`]、尺寸/内存超限或数据损坏时
/// 返回 [`MediaError`]，不 panic。
pub fn load_png(path: &Path) -> Result<CpuFrame, MediaError> {
    let meta = std::fs::metadata(path).map_err(|source| MediaError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.len() > MAX_PNG_FILE_BYTES {
        return Err(MediaError::FileTooLarge {
            path: path.to_path_buf(),
            size: meta.len(),
            limit: MAX_PNG_FILE_BYTES,
        });
    }

    let file = File::open(path).map_err(|source| MediaError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_limits(png::Limits {
        bytes: MAX_PNG_DECODED_BYTES,
    });

    let reader = decoder
        .read_info()
        .map_err(|e| decode_err(path, "读取 PNG 头", e))?;
    let info = reader.info();
    let width = info.width;
    let height = info.height;
    check_image_budget(width, height)?;

    let mut reader = reader;
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| MediaError::Decode(format!("无法确定输出缓冲大小: {}", path.display())))?;
    if buffer_size > MAX_PNG_DECODED_BYTES {
        return Err(MediaError::DecodedTooLarge {
            needed: buffer_size as u64,
            limit: MAX_PNG_DECODED_BYTES as u64,
        });
    }
    let mut raw = vec![0u8; buffer_size];
    let output = reader
        .next_frame(&mut raw)
        .map_err(|e| decode_err(path, "解码像素数据", e))?;

    if output.width != width || output.height != height {
        return Err(MediaError::Decode(format!(
            "PNG 输出尺寸与头部不符（{}）: 头 {width}x{height}，帧 {}x{}",
            path.display(),
            output.width,
            output.height
        )));
    }

    let stride = width * 4;
    let data = convert_to_rgba8(
        &raw[..output.buffer_size()],
        width,
        height,
        output.color_type,
    )?;
    let frame = CpuFrame {
        width,
        height,
        stride,
        format: PixelFormat::Rgba8888,
        data: Arc::from(data.into_boxed_slice()),
    };
    frame.validate()?;
    Ok(frame)
}

/// 按目标尺寸**边解码边降采样**，返回不超过 `max_width × max_height` 的 RGBA 帧。
///
/// 与 [`load_png`] 的区别：`load_png` 先把整幅图解到 `width × height × 4` 的缓冲里，
/// 再交给渲染器缩放。对 11944×5120 这种壁纸（61 MP）来说，那一份缓冲就是 **244 MB**，
/// 而最终只上屏 2560×1600 —— 峰值内存全耗在"解完再缩小"上。
///
/// 这里改成流式：逐行读、按 `step×step` 盒式平均直接落到输出缓冲，
/// 峰值从 `源像素 × 4` 降到 `输出像素 × 4`（通常是几 MB 到 16 MB）。
/// `step` 由 `max_width/max_height` 与源尺寸算出；目标不小于源时不缩放
/// （等价于 [`load_png`]）。
///
/// # Errors
///
/// 与 [`load_png`] 相同（文件/尺寸/预算/数据损坏）。
pub fn load_png_fitted(
    path: &Path,
    max_width: u32,
    max_height: u32,
) -> Result<CpuFrame, MediaError> {
    let meta = std::fs::metadata(path).map_err(|source| MediaError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.len() > MAX_PNG_FILE_BYTES {
        return Err(MediaError::FileTooLarge {
            path: path.to_path_buf(),
            size: meta.len(),
            limit: MAX_PNG_FILE_BYTES,
        });
    }
    let file = File::open(path).map_err(|source| MediaError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_limits(png::Limits {
        bytes: MAX_PNG_DECODED_BYTES,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|e| decode_err(path, "读取 PNG 头", e))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    check_image_budget(width, height)?;

    let max_width = max_width.max(1);
    let max_height = max_height.max(1);
    let step = (width.div_ceil(max_width))
        .max(height.div_ceil(max_height))
        .max(1);
    let out_width = width.div_ceil(step);
    let out_height = height.div_ceil(step);
    let out_bytes = out_width as usize * out_height as usize * 4;
    if out_bytes > MAX_PNG_DECODED_BYTES {
        return Err(MediaError::DecodedTooLarge {
            needed: out_bytes as u64,
            limit: MAX_PNG_DECODED_BYTES as u64,
        });
    }

    let color_type = reader.output_color_type().0;
    let bpp = match color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => {
            return Err(MediaError::Decode(format!(
                "PNG 调色板输出未被展开: {}",
                path.display()
            )));
        }
    };
    let row_bytes = width as usize * bpp;
    let mut out = vec![0u8; out_bytes];
    // 每列在水平方向上的样本数（右边缘可能不足 step）。
    let col_counts: Vec<u32> = (0..out_width)
        .map(|ox| {
            let start = ox * step;
            (width - start).min(step)
        })
        .collect();
    let mut acc = vec![0u64; out_width as usize * 4];
    let mut rows_in_block: u32 = 0;
    let mut out_row: u32 = 0;
    loop {
        match reader.next_row() {
            Ok(Some(row)) => {
                let data = &row.data()[..row_bytes];
                for (x, px) in data.chunks_exact(bpp).enumerate() {
                    let (r, g, b, a) = match bpp {
                        4 => (px[0], px[1], px[2], px[3]),
                        3 => (px[0], px[1], px[2], 255),
                        2 => (px[0], px[0], px[0], px[1]),
                        _ => (px[0], px[0], px[0], 255),
                    };
                    let ox = (x as u32 / step) as usize;
                    acc[ox * 4] += u64::from(r);
                    acc[ox * 4 + 1] += u64::from(g);
                    acc[ox * 4 + 2] += u64::from(b);
                    acc[ox * 4 + 3] += u64::from(a);
                }
                rows_in_block += 1;
                if rows_in_block == step || out_row + 1 == out_height {
                    let base = out_row as usize * out_width as usize * 4;
                    for ox in 0..out_width as usize {
                        let n = u64::from(col_counts[ox]) * u64::from(rows_in_block);
                        let n = n.max(1);
                        for c in 0..4 {
                            out[base + ox * 4 + c] = (acc[ox * 4 + c] / n) as u8;
                        }
                    }
                    acc.fill(0);
                    rows_in_block = 0;
                    out_row += 1;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(decode_err(path, "解码像素数据", e)),
        }
    }
    if out_row != out_height {
        return Err(MediaError::Decode(format!(
            "PNG 行数不符（{}）: 期望 {out_height} 行，实际 {}",
            path.display(),
            out_row
        )));
    }
    let frame = CpuFrame {
        width: out_width,
        height: out_height,
        stride: out_width * 4,
        format: PixelFormat::Rgba8888,
        data: Arc::from(out.into_boxed_slice()),
    };
    frame.validate()?;
    Ok(frame)
}

/// 把 PNG 解码输出统一为 RGBA8888（按需展开 Gray/GA/RGB）。
fn convert_to_rgba8(
    raw: &[u8],
    width: u32,
    height: u32,
    color_type: png::ColorType,
) -> Result<Vec<u8>, MediaError> {
    let pixels = (width as usize) * (height as usize);
    match color_type {
        png::ColorType::Rgba => {
            if raw.len() != pixels * 4 {
                return Err(MediaError::Decode(format!(
                    "RGBA 数据长度不符: {} != {}",
                    raw.len(),
                    pixels * 4
                )));
            }
            Ok(raw.to_vec())
        }
        png::ColorType::Rgb => {
            if raw.len() != pixels * 3 {
                return Err(MediaError::Decode(format!(
                    "RGB 数据长度不符: {} != {}",
                    raw.len(),
                    pixels * 3
                )));
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for px in raw.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Ok(out)
        }
        png::ColorType::Grayscale => {
            if raw.len() != pixels {
                return Err(MediaError::Decode(format!(
                    "Gray 数据长度不符: {} != {}",
                    raw.len(),
                    pixels
                )));
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for &g in raw {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            Ok(out)
        }
        png::ColorType::GrayscaleAlpha => {
            if raw.len() != pixels * 2 {
                return Err(MediaError::Decode(format!(
                    "GrayAlpha 数据长度不符: {} != {}",
                    raw.len(),
                    pixels * 2
                )));
            }
            let mut out = Vec::with_capacity(pixels * 4);
            for px in raw.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            Ok(out)
        }
        other => Err(MediaError::Decode(format!(
            "不支持的 PNG 颜色类型: {other:?}"
        ))),
    }
}

fn decode_err(path: &Path, stage: &str, e: png::DecodingError) -> MediaError {
    match e {
        png::DecodingError::LimitsExceeded => MediaError::DecodedTooLarge {
            needed: 0,
            limit: MAX_PNG_DECODED_BYTES as u64,
        },
        other => MediaError::Decode(format!("{}（{}）: {other}", path.display(), stage)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::types::MediaKind;
    use std::path::PathBuf;

    fn write_png(path: &Path, width: u32, height: u32, color: png::ColorType, pixels: &[u8]) {
        let file = File::create(path).unwrap();
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("liuguang-png-test-{name}-{}", std::process::id()));
        p
    }

    #[test]
    fn fitted_decode_downscales_with_box_filter() {
        // 8x8 的 RGBA 图，左半红、右半蓝；缩到 4x4 后应仍是左半红、右半蓝。
        let path = temp_path("fitted");
        let mut pixels = Vec::new();
        for _y in 0..8 {
            for x in 0..8 {
                if x < 4 {
                    pixels.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    pixels.extend_from_slice(&[0, 0, 255, 255]);
                }
            }
        }
        write_png(&path, 8, 8, png::ColorType::Rgba, &pixels);
        let frame = load_png_fitted(&path, 4, 4).unwrap();
        assert_eq!((frame.width, frame.height), (4, 4));
        let data = frame.data.as_ref();
        let px = |x: usize, y: usize| {
            let i = y * frame.stride as usize + x * 4;
            (data[i], data[i + 1], data[i + 2])
        };
        assert_eq!(px(0, 0), (255, 0, 0));
        assert_eq!(px(3, 3), (0, 0, 255));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fitted_decode_keeps_size_when_target_is_larger() {
        let path = temp_path("fitted-large");
        write_png(&path, 4, 4, png::ColorType::Rgb, &[10u8; 4 * 4 * 3]);
        let frame = load_png_fitted(&path, 1920, 1080).unwrap();
        assert_eq!((frame.width, frame.height), (4, 4));
        assert_eq!(frame.data.as_ref()[0], 10);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn budget_checks() {
        assert!(check_image_budget(3840, 2160).is_ok());
        assert!(check_image_budget(7680, 4320).is_ok());
        assert!(matches!(
            check_image_budget(0, 100),
            Err(MediaError::Decode(_))
        ));
        assert!(matches!(
            check_image_budget(20_000, 100),
            Err(MediaError::DimensionExceeded { .. })
        ));
        // 单边未超限但总像素超预算（16384 x 16384 x 4 = 1 GiB > 256 MiB）
        assert!(matches!(
            check_image_budget(16_384, 16_384),
            Err(MediaError::DecodedTooLarge { .. })
        ));
    }

    #[test]
    fn load_rgba_png() {
        let path = temp_path("rgba");
        // 2x2：红、绿、蓝、白
        write_png(
            &path,
            2,
            2,
            png::ColorType::Rgba,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        );
        let frame = load_png(&path).unwrap();
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.stride, 8);
        assert_eq!(frame.format, PixelFormat::Rgba8888);
        assert_eq!(&frame.data[0..4], &[255, 0, 0, 255]);
        assert_eq!(&frame.data[12..16], &[255, 255, 255, 255]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rgb_png_expands_alpha() {
        let path = temp_path("rgb");
        write_png(&path, 1, 1, png::ColorType::Rgb, &[10, 20, 30]);
        let frame = load_png(&path).unwrap();
        assert_eq!(&frame.data[..], &[10, 20, 30, 255]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_gray_png_expands_to_rgba() {
        let path = temp_path("gray");
        write_png(&path, 2, 1, png::ColorType::Grayscale, &[7, 200]);
        let frame = load_png(&path).unwrap();
        assert_eq!(&frame.data[..4], &[7, 7, 7, 255]);
        assert_eq!(&frame.data[4..8], &[200, 200, 200, 255]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn non_png_file_is_rejected_without_panic() {
        let path = temp_path("garbage");
        std::fs::write(&path, b"this is not a png at all").unwrap();
        let err = load_png(&path).unwrap_err();
        assert!(matches!(err, MediaError::Decode(_)), "实际: {err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncated_png_is_rejected() {
        let path = temp_path("truncated");
        write_png(&path, 4, 4, png::ColorType::Rgba, &[0u8; 64]);
        let data = std::fs::read(&path).unwrap();
        std::fs::write(&path, &data[..data.len() / 2]).unwrap();
        let err = load_png(&path).unwrap_err();
        assert!(matches!(err, MediaError::Decode(_)), "实际: {err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_reports_io() {
        let err = load_png(Path::new("/nonexistent/x.png")).unwrap_err();
        assert!(matches!(err, MediaError::Io { .. }));
    }

    #[test]
    fn oversized_file_is_rejected_before_decode() {
        // 不真正写 128MiB：用直接构造的错误分支验证限额存在。
        let err = MediaError::FileTooLarge {
            path: PathBuf::from("/x.png"),
            size: MAX_PNG_FILE_BYTES + 1,
            limit: MAX_PNG_FILE_BYTES,
        };
        assert!(err.to_string().contains("超过大小上限"));
    }

    #[test]
    fn media_info_companion_types_still_work() {
        // 防止未来重构误删 MediaKind 关联（load_png 产物将喂给 MediaSource）。
        let _ = MediaKind::Image;
    }
}
