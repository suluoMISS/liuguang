//! 离线基准（**无需 /dev/dri、无需真机**）：4K 帧「帧 → 扫描缓冲」上传成本，
//! 对比历史逐行实现与当前生产实现。
//!
//! 生产侧通过 [`MockDisplayBackend`] 驱动**真实**的池化 + `upload` 代码路径
//! （`crates/liuguang-display/src/scanout.rs`），只有底层存储换成 `Vec<u8>`；
//! 因此本基准的每帧耗时只包含应用侧拷贝与池化逻辑，不含 DRM ioctl / VA 下载。
//!
//! 用法：`upload_bench [每格式迭代次数=60]`
//!
//! 输出：
//! - `prod_ms`：走池化 + `upload`（连续行单次 memcpy）的每帧耗时；
//! - `legacy_ms`：历史逐行 `copy_from_slice` 循环（同一份帧数据、同一个缓冲尺寸）；
//! - 每格式的 `bytes/frame` 与稳态 `allocations`/`mappings`（应恒等于池深 3）。

use liuguang_core::types::{CpuFrame, DisplayFrame, PixelFormat};
use liuguang_display::DisplayBackend;
use liuguang_display::mock::MockDisplayBackend;
use std::sync::Arc;
use std::time::Instant;

fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let tail = stat.rfind(')').map(|i| &stat[i + 2..]).unwrap_or("");
    let f: Vec<&str> = tail.split_whitespace().collect();
    let utime: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    utime + stime
}

fn make_frame(w: u32, h: u32, format: PixelFormat) -> CpuFrame {
    let stride = if format.is_semi_planar() { w } else { w * 4 };
    let len = if format.is_semi_planar() {
        (stride as usize) * (h as usize) * 3 / 2
    } else {
        (stride as usize) * (h as usize)
    };
    let mut data = vec![0u8; len];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i & 0xff) as u8;
    }
    CpuFrame {
        width: w,
        height: h,
        stride,
        format,
        data: Arc::from(data.into_boxed_slice()),
    }
}

/// 历史实现（优化前）：逐行 `copy_from_slice`，无连续快路径。
/// 与生产 `upload` 的语义相同（源 stride / 目标 pitch 填充、NV12 双平面）。
fn legacy_rowwise_upload(dst: &mut [u8], pitch: usize, frame: &CpuFrame) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let src_stride = frame.stride as usize;
    let semi = frame.format.is_semi_planar();
    let row_bytes = if semi { w } else { w * 4 };
    let planes: &[(usize, usize, usize)] = if semi {
        &[(0, 0, h), (src_stride * h, pitch * h, h.div_ceil(2))]
    } else {
        &[(0, 0, h)]
    };
    for &(src_off, dst_off, rows) in planes {
        for y in 0..rows {
            let s0 = src_off + y * src_stride;
            let d0 = dst_off + y * pitch;
            dst[d0..d0 + row_bytes].copy_from_slice(&frame.data[s0..s0 + row_bytes]);
        }
    }
}

/// 纯 std 对照实验：同样的字节数、同样的 3 缓冲轮转下，
/// 「两次大 memcpy」与「逐行小拷贝」的带宽差异（排除 mock/池化开销）。
fn raw_copy_compare(iters: u32) {
    let (w, h) = (3840usize, 2160usize);
    let y_bytes = w * h;
    let uv_bytes = w * (h.div_ceil(2));
    let src = vec![7u8; y_bytes + uv_bytes];
    let mut dsts: Vec<Vec<u8>> = (0..3).map(|_| vec![0u8; y_bytes + uv_bytes]).collect();

    let t0 = Instant::now();
    for i in 0..iters as usize {
        let dst = &mut dsts[i % 3];
        dst[..y_bytes].copy_from_slice(&src[..y_bytes]);
        dst[y_bytes..].copy_from_slice(&src[y_bytes..]);
    }
    let big = t0.elapsed().as_secs_f64() * 1e3 / f64::from(iters);

    let t1 = Instant::now();
    for i in 0..iters as usize {
        let dst = &mut dsts[i % 3];
        for y in 0..h {
            let s0 = y * w;
            dst[s0..s0 + w].copy_from_slice(&src[s0..s0 + w]);
        }
        for y in 0..h.div_ceil(2) {
            let s0 = y_bytes + y * w;
            dst[s0..s0 + w].copy_from_slice(&src[s0..s0 + w]);
        }
    }
    let rowwise = t1.elapsed().as_secs_f64() * 1e3 / f64::from(iters);
    println!(
        "RAW_COPY nv12 bytes={} 2x_memcpy_ms={big:.3} rowwise_ms={rowwise:.3} \
         (带宽 {:.1} vs {:.1} GB/s)",
        y_bytes + uv_bytes,
        (y_bytes + uv_bytes) as f64 / big / 1e6,
        (y_bytes + uv_bytes) as f64 / rowwise / 1e6
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iters: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let (w, h) = (3840u32, 2400u32);
    let cases = [
        ("nv12  3840x2400(视频)", PixelFormat::Nv12, 3840u32, 2160u32),
        ("xrgb  3840x2400(静态图)", PixelFormat::Xrgb8888, w, h),
        ("rgba  3840x2400(旋转/回退)", PixelFormat::Rgba8888, w, h),
    ];

    println!("UPLOAD_BENCH iters={iters}（离线：mock 后端 + 真实池化/上传实现）");
    raw_copy_compare(iters);
    for (name, format, fw, fh) in cases {
        let frame = make_frame(fw, fh, format);
        let bytes = frame.data.len() as u64;

        // --- 生产路径（池化 + upload）---
        let mut backend = MockDisplayBackend::with_default_output();
        let output = liuguang_core::types::OutputId::new("HDMI-A-1");
        // 预热（建立 3 个槽位）
        for _ in 0..3 {
            backend.present(output.clone(), DisplayFrame::Cpu(frame.clone()))?;
        }
        let c0 = cpu_ticks();
        let t0 = Instant::now();
        for _ in 0..iters {
            backend.present(output.clone(), DisplayFrame::Cpu(frame.clone()))?;
        }
        let prod_wall = t0.elapsed().as_secs_f64() * 1e3 / f64::from(iters);
        let prod_cpu = cpu_ticks().saturating_sub(c0) as f64 * 10.0 / f64::from(iters);
        let stats = backend.scanout_stats();

        // --- 历史路径（逐行拷贝）---
        // 公平性：历史生产代码同样维护 3 缓冲轮转（否则会覆盖正在扫描的帧），
        // 因此这里也用 3 个缓冲轮转，避免"单缓冲热缓存"造成的虚假优势。
        let pitch = if format.is_semi_planar() {
            fw as usize
        } else {
            fw as usize * 4
        };
        let mut bufs: Vec<Vec<u8>> = (0..3).map(|_| vec![0u8; bytes as usize]).collect();
        // 预热：先触碰全部 3 个缓冲，避免"首次触碰页错误"污染对比
        // （生产侧的槽位缓冲同样在预热阶段建立，见上面 3 帧预热）。
        for (i, buf) in bufs.iter_mut().enumerate() {
            legacy_rowwise_upload(buf, pitch, &frame);
            let _ = i;
        }
        let mut slot = 0usize;
        let c1 = cpu_ticks();
        let t1 = Instant::now();
        for _ in 0..iters {
            legacy_rowwise_upload(&mut bufs[slot], pitch, &frame);
            slot = (slot + 1) % 3;
        }
        let legacy_wall = t1.elapsed().as_secs_f64() * 1e3 / f64::from(iters);
        let legacy_cpu = cpu_ticks().saturating_sub(c1) as f64 * 10.0 / f64::from(iters);

        let mb = bytes as f64 / (1024.0 * 1024.0);
        println!(
            "{name}: bytes/frame={bytes} ({mb:.2} MiB) \
             prod_ms={prod_wall:.3} legacy_ms={legacy_wall:.3} speedup={:.2}x | \
             prod_cpu_ms={prod_cpu:.3} legacy_cpu_ms={legacy_cpu:.3} | \
             alloc={} map={} reuse={}",
            legacy_wall / prod_wall.max(1e-9),
            stats.allocations,
            stats.mappings,
            stats.reuses,
        );
    }
    println!(
        "UPLOAD_BENCH 备注：此处不含 DRM ioctl 与 VA surface 下载；\
         真机端到端数据见 crates/liuguang-display/examples/present_bench.rs"
    );
    Ok(())
}
