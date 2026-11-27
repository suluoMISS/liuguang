//! 生产 RGB 转换路径（`VaapiProducer::open_rgb` → `RgbConverter`）的像素探针。
//!
//! 解码 N 帧、每帧走一次真正的 `convert()`，然后把**最后一帧**的输出从 GPU
//! 回读成 PPM。用来做优化前后同一 pts 的逐像素比对（MAE）。
//!
//! ```bash
//! cargo run -p liuguang-vaapi --example rgb_convert_probe -- \
//!     <视频> /dev/dri/renderD129 --frames 3 --ppm /tmp/after.ppm
//! # 按面板尺寸出图（daemon 里的 set_output_size 路径）：
//! cargo run -p liuguang-vaapi --example rgb_convert_probe -- \
//!     <视频> /dev/dri/renderD129 --frames 3 --output-size 2560x1600 --ppm /tmp/after_disp.ppm
//! ```
//!
//! 和 `examples/egl_convert.rs` 的区别：那支是独立原型（自带 Renderer、自己
//! 分配输出 BO），验证不到产品路径；这支用的是产品里的 `RgbConverter` /
//! 输出池 / EGLImage 逻辑。
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use liuguang_core::types::DisplayFrame;
use liuguang_media::video::{FrameProducer, VideoPoll};
use liuguang_vaapi::VaapiProducer;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("失败: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    video: PathBuf,
    node: PathBuf,
    frames: usize,
    ppm: Option<PathBuf>,
    output_size: Option<(u32, u32)>,
}

fn usage() -> &'static str {
    "用法: rgb_convert_probe <视频> <渲染节点> [--frames N] [--output-size WxH] [--ppm <输出.ppm>]"
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args_os().skip(1);
    let mut positional = Vec::new();
    let mut frames = 3usize;
    let mut ppm = None;
    let mut output_size = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--frames") => {
                let value = args.next().ok_or("--frames 缺少参数")?;
                frames = value
                    .to_str()
                    .ok_or("--frames 不是 UTF-8")?
                    .parse()
                    .map_err(|error| format!("--frames 解析失败: {error}"))?;
            }
            Some("--ppm") => {
                let value = args.next().ok_or("--ppm 缺少参数")?;
                ppm = Some(PathBuf::from(value));
            }
            Some("--output-size") => {
                let value = args.next().ok_or("--output-size 缺少参数")?;
                let text = value.to_str().ok_or("--output-size 不是 UTF-8")?;
                let (w, h) = text.split_once('x').ok_or("--output-size 形如 2560x1600")?;
                let parse = |s: &str| -> Result<u32, String> {
                    s.parse()
                        .map_err(|error| format!("--output-size 解析失败: {error}"))
                };
                output_size = Some((parse(w)?, parse(h)?));
            }
            Some("-h" | "--help") => return Err(usage().to_string()),
            _ => positional.push(arg),
        }
    }
    if positional.len() != 2 {
        return Err(usage().to_string());
    }
    if frames == 0 {
        return Err("--frames 必须大于 0".to_string());
    }
    Ok(Args {
        video: PathBuf::from(&positional[0]),
        node: PathBuf::from(&positional[1]),
        frames,
        ppm,
        output_size,
    })
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let mut producer = VaapiProducer::open_rgb(&args.video, &args.node)
        .map_err(|error| format!("打开 RGB 生产者失败: {error}"))?;
    // 解码预读线程会多转换几帧，只回读"转换器最后一次绘制"会拿到还没交付给上层
    // 的那一帧。开启逐帧捕获后，回读的是最近一次 `next_producer_frame` 交出的帧。
    producer.enable_capture();
    if let Some((width, height)) = args.output_size {
        // 与 daemon 相同的入口：走 FrameProducer::set_output_size → 转换器重建输出池。
        producer.set_output_size(width, height);
        println!("已请求输出尺寸 {width}x{height}");
    }

    let mut produced = 0usize;
    let mut last_pts = Duration::ZERO;
    let mut last_size = (0u32, 0u32);
    // 上限 = 帧数 × 600 次轮询（每轮 5ms ⇒ 单帧最多等 3s），防止卡死。
    let mut polls = 0usize;
    let poll_limit = args.frames * 600;
    while produced < args.frames {
        polls += 1;
        if polls > poll_limit {
            return Err(format!("等第 {} 帧超时（已轮询 {polls} 次）", produced + 1));
        }
        match producer
            .next_producer_frame()
            .map_err(|error| format!("取帧失败: {error}"))?
        {
            VideoPoll::Frame { pts, frame } => {
                let DisplayFrame::DmaBuf(dmabuf) = &frame else {
                    return Err("生产者给的不是 DMA-BUF 帧".to_string());
                };
                last_size = (dmabuf.width, dmabuf.height);
                last_pts = pts;
                produced += 1;
                println!(
                    "第 {produced} 帧: pts={:.3}s 输出={}x{} fourcc=0x{:08X} modifier=0x{:016X}",
                    pts.as_secs_f64(),
                    dmabuf.width,
                    dmabuf.height,
                    dmabuf.fourcc,
                    dmabuf.modifier
                );
            }
            VideoPoll::NeedMore => std::thread::sleep(Duration::from_millis(5)),
            VideoPoll::Ended => return Err("解码流提前结束".to_string()),
        }
    }

    let (width, height, data) = producer
        .readback_last_xrgb()
        .map_err(|error| format!("回读失败: {error}"))?;
    if (width, height) != last_size {
        return Err(format!(
            "回读尺寸 {width}x{height} 与帧尺寸 {}x{} 不一致",
            last_size.0, last_size.1
        ));
    }
    println!(
        "回读: {width}x{height} pts={:.3}s 字节={}",
        last_pts.as_secs_f64(),
        data.len()
    );
    if let Some(path) = &args.ppm {
        write_ppm(path, &data, width, height)
            .map_err(|error| format!("写 {} 失败: {error}", path.display()))?;
        println!("已写出 PPM: {}", path.display());
    }
    Ok(())
}

/// 写 P6 PPM：`GL_RGBA` 回读的 4 字节取前 3 个（R、G、B）。
fn write_ppm(path: &PathBuf, rgba: &[u8], width: u32, height: u32) -> io::Result<()> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() < expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "回读字节数 {} 少于 {width}x{height}x4={expected}",
                rgba.len()
            ),
        ));
    }
    let mut out = BufWriter::new(File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    let mut row = Vec::with_capacity(width as usize * 3);
    for line in rgba[..expected].chunks_exact(width as usize * 4) {
        row.clear();
        for pixel in line.chunks_exact(4) {
            row.extend_from_slice(&pixel[..3]);
        }
        out.write_all(&row)?;
    }
    out.flush()
}
