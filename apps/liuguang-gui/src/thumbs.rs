//! 缩略图解码线程：把守护进程送来的 **base64 PNG** 解成 RGBA，GUI 主线程只负责贴纹理。
//!
//! **所有缩略图都由守护进程解码**（`get_thumbnail`）：解码器、媒体根边界校验、
//! 视频 seek 都在它那一侧；GUI 不该为了封面打开任意路径、也不该为了一张图
//! 拉起解码管线。这里只做"base64 → PNG → RGBA"这一件纯 CPU 的小事。
//!
//! base64 解码也放在这个线程里（t40）：一张 8K 视频帧的 PNG base64 有几十万字符，
//! 在 GTK 主线程解码会让点击「应用」明显延迟，必须离开主线程。
//!
//! 缓存键是 `id@at_ms`：悬停动效会按时刻取多帧，同一张图的封面帧与预览帧
//! 必须能分别存取。

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use gtk::glib;

// `PixbufLoader::{write,close,pixbuf}` 来自扩展 trait（`PixbufLoaderExt`），
// 不导入 prelude 就只有类型没有方法。
use gdk_pixbuf::prelude::*;

/// 缓存键：媒体 id + 时刻（毫秒）。
#[must_use]
pub fn key(id: &str, at_ms: u64) -> String {
    format!("{id}@{at_ms}")
}

/// 已解码的一张图。
#[derive(Debug)]
pub struct Thumb {
    /// [`key`] 生成的缓存键。
    pub key: String,
    pub width: i32,
    pub height: i32,
    /// RGBA8，长度 = width * height * 4。
    pub rgba: Vec<u8>,
    /// 从缩略图提取的主氛围色（用于“曜晶流光”动态点缀与背景光晕）。
    pub accent: (u8, u8, u8),
}

/// 缩略图线程的命令。
#[derive(Debug)]
pub enum ThumbCmd {
    /// 解码守护进程送来的一帧 PNG（base64 原文，解码在本线程完成）。
    LoadPng { key: String, png_b64: String },
    /// 忘掉某个前缀的所有缓存（媒体文件被替换、重新扫描）。
    Forget(String),
}

/// 请求解码；重复请求同一键会被忽略（网格重建会重复触发）。
pub fn spawn() -> (Sender<ThumbCmd>, async_channel::Receiver<Thumb>) {
    let (tx, rx) = channel::<ThumbCmd>();
    let (tx_out, rx_out) = async_channel::unbounded::<Thumb>();
    thread::Builder::new()
        .name("liuguang-thumbs".into())
        .spawn(move || run(rx, tx_out))
        .expect("启动缩略图线程");
    (tx, rx_out)
}

fn run(rx: Receiver<ThumbCmd>, tx: async_channel::Sender<Thumb>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            ThumbCmd::Forget(_prefix) => {}
            ThumbCmd::LoadPng { key, png_b64 } => {
                // 不在工作线程永久记住 `seen`：主线程 LRU 淘汰（>240 条）或快速悬停
                // 取消后，卡片仍需要能重新请求并解码出缩略图。
                let Some(thumb) = decode_png_b64(&key, &png_b64) else {
                    continue;
                };
                if tx.send_blocking(thumb).is_err() {
                    return; // GUI 已退出
                }
            }
        }
    }
}

/// base64 → PNG → RGBA。两步都在缩略图线程里做（t40：主线程只贴纹理）。
fn decode_png_b64(key: &str, png_b64: &str) -> Option<Thumb> {
    let png = glib::base64_decode(png_b64);
    if png.is_empty() {
        return None;
    }
    decode_png(key, &png)
}

/// 解码守护进程送来的 PNG（`gdk-pixbuf` 的 loader 支持流式喂字节，不需要临时文件）。
fn decode_png(key: &str, png: &[u8]) -> Option<Thumb> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(png).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    let (width, height) = (pixbuf.width(), pixbuf.height());
    if width <= 0 || height <= 0 {
        return None;
    }
    let rgba = pixbuf.read_pixel_bytes().to_vec();
    let channels = usize::try_from(pixbuf.n_channels()).ok()?;
    let rowstride = usize::try_from(pixbuf.rowstride()).ok()?;
    let rgba8 = rgba_to_rgba8(&rgba, channels, rowstride, width, height)?;
    let accent = extract_accent_color(&rgba8);
    Some(Thumb {
        key: key.to_owned(),
        width,
        height,
        rgba: rgba8,
        accent,
    })
}

/// 从紧凑 RGBA8 像素提取高可读性的主氛围色（默认回退电光紫 `#7a5cff`）。
#[must_use]
pub fn extract_accent_color(rgba: &[u8]) -> (u8, u8, u8) {
    const FALLBACK: (u8, u8, u8) = (122, 92, 255);
    let total_pixels = rgba.len() / 4;
    if total_pixels == 0 {
        return FALLBACK;
    }
    let stride = (total_pixels / 384).max(1);
    let mut sum_r = 0.0_f64;
    let mut sum_g = 0.0_f64;
    let mut sum_b = 0.0_f64;
    let mut weight_sum = 0.0_f64;

    for idx in (0..total_pixels).step_by(stride) {
        let base = idx * 4;
        let a = rgba[base + 3];
        if a < 128 {
            continue;
        }
        let r = f64::from(rgba[base]);
        let g = f64::from(rgba[base + 1]);
        let b = f64::from(rgba[base + 2]);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let chroma = max - min;
        if max < 28.0 || chroma < 18.0 || (min > 228.0 && chroma < 24.0) {
            continue;
        }
        let weight = chroma * (chroma / max.max(1.0));
        sum_r += r * weight;
        sum_g += g * weight;
        sum_b += b * weight;
        weight_sum += weight;
    }

    if weight_sum <= 1e-6 {
        return FALLBACK;
    }

    let r = (sum_r / weight_sum) / 255.0;
    let g = (sum_g / weight_sum) / 255.0;
    let b = (sum_b / weight_sum) / 255.0;
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsl_to_rgb(h, s.clamp(0.48, 0.78), l.clamp(0.58, 0.68))
}

fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    let d = max - min;
    if d <= 1e-9 {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() <= 1e-9 {
        ((g - b) / d) + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() <= 1e-9 {
        ((b - r) / d) + 2.0
    } else {
        ((r - g) / d) + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    if s <= 1e-9 {
        let v = (l * 255.0).round().clamp(0.0, 255.0) as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let chan = |t: f64| -> u8 {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    (chan(h + 1.0 / 3.0), chan(h), chan(h - 1.0 / 3.0))
}

/// 把任意通道数的行填充位图规整成紧凑 RGBA8。
fn rgba_to_rgba8(
    data: &[u8],
    channels: usize,
    rowstride: usize,
    width: i32,
    height: i32,
) -> Option<Vec<u8>> {
    if channels != 3 && channels != 4 {
        return None;
    }
    let w = usize::try_from(width).ok()?;
    let h = usize::try_from(height).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let min_row = w.checked_mul(channels)?;
    if rowstride < min_row {
        return None;
    }
    let needed = rowstride.checked_mul(h - 1)?.checked_add(min_row)?;
    if data.len() < needed {
        return None;
    }
    let out_cap = w.checked_mul(h)?.checked_mul(4)?;
    let mut out = Vec::with_capacity(out_cap);
    for y in 0..h {
        let row_start = y * rowstride;
        let row = &data[row_start..row_start + min_row];
        for px in row.chunks_exact(channels) {
            match channels {
                4 => out.extend_from_slice(&px[..4]),
                3 => out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]),
                _ => return None,
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_id_and_time() {
        assert_eq!(key("/a/b.png", 0), "/a/b.png@0");
        assert_eq!(key("/a/b.mp4", 2500), "/a/b.mp4@2500");
    }

    #[test]
    fn decodes_base64_png_off_the_main_thread_path() {
        // 1×1 PNG：验证 base64 → PNG → RGBA 这条链在本线程完整可用。
        const ONE_PX: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let thumb = decode_png_b64("/m/a.png@0", ONE_PX).expect("1x1 PNG 应能解码");
        assert_eq!((thumb.width, thumb.height), (1, 1));
        assert_eq!(thumb.rgba.len(), 4);
    }

    #[test]
    fn rgba_to_rgba8_validates_stride_channels_and_tight_last_row() {
        // 末行不带尾部填充的 2x2 RGB24：前一行 stride=8，第二行紧贴 6 字节。
        let buf = [1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12];
        let out = rgba_to_rgba8(&buf, 3, 8, 2, 2).expect("紧致末行应成功");
        assert_eq!(
            out,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
        assert!(
            rgba_to_rgba8(&buf, 3, 5, 2, 2).is_none(),
            "过小行距必须拒绝"
        );
        assert!(
            rgba_to_rgba8(&buf, 2, 8, 2, 2).is_none(),
            "不支持通道数必须拒绝"
        );
        assert!(
            rgba_to_rgba8(&buf, 4, i32::MAX as usize, i32::MAX, i32::MAX).is_none(),
            "乘法溢出必须拒绝"
        );
    }

    #[test]
    fn extract_accent_color_finds_vibrant_hue_and_falls_back_on_gray() {
        let gray = [120, 120, 120, 255].repeat(32);
        assert_eq!(extract_accent_color(&gray), (122, 92, 255));

        let mut scene = [18, 20, 26, 255].repeat(32);
        scene.extend([20, 190, 210, 255].repeat(32));
        let (r, g, b) = extract_accent_color(&scene);
        assert!(
            b > r && g > r,
            "青色主色调应保留 g/b 优势，实际 ({r},{g},{b})"
        );
    }
}
