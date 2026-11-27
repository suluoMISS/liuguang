//! 离线基准（无需真机）：3840x2400 下 CPU 转场（fade/slide/wipe/circle）
//! 的**每帧成本**，用于判断播放/切换时是否会掉帧。
//!
//! 用法：`transition_bench [每效果迭代次数=30] [宽=3840] [高=2400]`
//!
//! 预算参照：60 Hz 显示的单帧预算 16.7 ms（转场期间除转场外还要做上传与提交，
//! 因此转场本身应远低于该值）；数值为**本机实测**，真机需用同一二进制复跑。

use liuguang_core::config::{Easing, TransitionDirection};
use liuguang_core::types::{CpuFrame, PixelFormat};
use liuguang_render::transition::{CpuTransition, EffectKind, TransitionSpec};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let tail = stat.rfind(')').map(|i| &stat[i + 2..]).unwrap_or("");
    let f: Vec<&str> = tail.split_whitespace().collect();
    let utime: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    utime + stime
}

fn frame(w: u32, h: u32, seed: u8) -> CpuFrame {
    let stride = w * 4;
    let mut data = vec![0u8; (stride * h) as usize];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(seed);
    }
    CpuFrame {
        width: w,
        height: h,
        stride,
        format: PixelFormat::Xrgb8888,
        data: Arc::from(data.into_boxed_slice()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let iters: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(30);
    let w: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3840);
    let h: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2400);

    let from = frame(w, h, 0);
    let to = frame(w, h, 77);
    let kinds = [
        ("fade", EffectKind::Fade),
        (
            "slide-left",
            EffectKind::Slide {
                direction: TransitionDirection::Left,
            },
        ),
        (
            "wipe-left",
            EffectKind::Wipe {
                direction: TransitionDirection::Left,
            },
        ),
        ("circle", EffectKind::Circle),
    ];

    let progress_steps = [0.05f32, 0.25, 0.5, 0.75, 0.95];
    println!("TRANSITION_BENCH {w}x{h} iters_per_progress={iters}（本机实测，CPU 路径）");
    for (name, kind) in kinds {
        let spec = TransitionSpec {
            kind,
            easing: Easing::EaseInOut,
            duration: Duration::from_millis(300),
        };
        let mut t = CpuTransition::new(spec);
        t.prepare_frames(from.clone(), to.clone())?;
        // 预热（分配内部缓冲）
        let _ = t.render_at(0.5)?;
        let c0 = cpu_ticks();
        let t0 = Instant::now();
        let mut n = 0u32;
        for _ in 0..iters {
            for p in progress_steps {
                let out = t.render_at(p)?;
                std::hint::black_box(out.width);
                n += 1;
            }
        }
        let wall = t0.elapsed().as_secs_f64() * 1e3 / f64::from(n);
        let cpu = cpu_ticks().saturating_sub(c0) as f64 * 10.0 / f64::from(n);
        println!(
            "TRANSITION_BENCH {name}: per_frame_ms={wall:.3} cpu_ms={cpu:.3} \
             (60Hz 预算占比 {:.1}%)",
            cpu / 16.7 * 100.0
        );
    }
    Ok(())
}
