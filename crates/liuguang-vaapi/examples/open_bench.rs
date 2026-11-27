//! 打开一个 VA-API 生产者并测量各阶段耗时（诊断用，不参与构建门禁）。
//!
//! 用法：`cargo run --release -p liuguang-vaapi --example open_bench -- <视频> <渲染节点> [轮数]`

use std::path::Path;
use std::time::Instant;

use liuguang_media::video::{FrameProducer as _, VideoPoll};

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("缺少视频路径")?;
    let node = args.next().ok_or("缺少渲染节点（/dev/dri/renderDNN）")?;
    let rounds: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(3);
    for round in 1..=rounds {
        let t0 = Instant::now();
        let mut producer =
            liuguang_vaapi::VaapiProducer::open_rgb(Path::new(&path), Path::new(&node))
                .map_err(|error| error.to_string())?;
        let open = t0.elapsed();
        let mut first = None;
        let mut frames = 0usize;
        let t1 = Instant::now();
        for _ in 0..600 {
            match producer.next_producer_frame() {
                Ok(VideoPoll::Frame { .. }) => {
                    if first.is_none() {
                        first = Some(t1.elapsed());
                    }
                    frames += 1;
                    if first.is_some() {
                        break;
                    }
                }
                Ok(VideoPoll::NeedMore) => std::thread::sleep(std::time::Duration::from_millis(2)),
                Ok(VideoPoll::Ended) => break,
                Err(error) => return Err(format!("解码失败: {error}")),
            }
        }
        println!(
            "round {round}: open={:.1}ms first_frame={:.1}ms frames={frames}",
            open.as_secs_f64() * 1000.0,
            first.map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0)
        );
    }
    Ok(())
}
