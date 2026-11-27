//! 真机基准：DRM/KMS 每帧提交成本（每帧 CPU 时间 + wall time）。
//!
//! 用法（目标机 DP-1 3840x2400@60）：
//! ```text
//! present_bench [帧数=180] [nv12|xrgb|rgba] [源宽=3840] [源高=2160]
//! ```
//! 复现 Lead 的 `commit_ms` 场景：源帧按 cover 适配（等价 daemon 的
//! `fit_visible_rects`）后经 `present_fitted` 提交，src/dst 矩形交给
//! DRM 平面做硬件缩放。
//!
//! 输出每帧的：
//! - `wall_ms`：`present_fitted` 调用耗时（daemon 的 commit_ms 同源）；
//! - `cpu_ms`：`/proc/self/stat` 的 utime+stime 差值（USER_HZ=100）；
//! - `bytes`：本帧从用户内存拷进扫描缓冲的字节数。

use liuguang_core::config::ModeSelection;
use liuguang_core::types::{CpuFrame, DisplayFrame, OutputId, PixelFormat};
use liuguang_display::DisplayBackend;
use liuguang_display::drm_kms::DrmKmsBackend;
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::time::Instant;

/// `/proc/self/stat` 的 utime+stime（USER_HZ 单位，Linux 恒为 100）。
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let tail = stat.rfind(')').map(|i| &stat[i + 2..]).unwrap_or("");
    let f: Vec<&str> = tail.split_whitespace().collect();
    let utime: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    utime + stime
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 合成 NV12 帧（Y 渐变 + UV 常量），stride = 宽。
fn nv12(w: u32, h: u32) -> CpuFrame {
    let y_plane = (w * h) as usize;
    let mut data = vec![0u8; y_plane * 3 / 2];
    for y in 0..h as usize {
        for x in 0..w as usize {
            data[y * w as usize + x] = ((x + y) & 0xff) as u8;
        }
    }
    for b in &mut data[y_plane..] {
        *b = 128;
    }
    CpuFrame {
        width: w,
        height: h,
        stride: w,
        format: PixelFormat::Nv12,
        data: Arc::from(data.into_boxed_slice()),
    }
}

/// 合成 4 字节/像素帧（XRGB 或 RGBA），stride = 宽×4。
fn rgba_like(w: u32, h: u32, format: PixelFormat) -> CpuFrame {
    let stride = w * 4;
    let mut data = vec![0u8; (stride * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let off = (y * stride + x * 4) as usize;
            data[off] = (x & 0xff) as u8;
            data[off + 1] = (y & 0xff) as u8;
            data[off + 2] = ((x ^ y) & 0xff) as u8;
            data[off + 3] = 0xff;
        }
    }
    CpuFrame {
        width: w,
        height: h,
        stride,
        format,
        data: Arc::from(data.into_boxed_slice()),
    }
}

/// 旧实现的两个分量（基准内复现，非生产代码）：
/// - 旧 `present_cpu_fitted` 每帧都 `map_dumb_buffer`（mmap）→ 写 → drop（munmap）；
/// - 每帧重建 3 个对象的属性名→句柄映射（`get_properties` + `as_hashmap`）。
///
/// 这里用同一个 DRM 设备把这两步单独计时，用来给出"优化前"的分项成本
/// （旧代码不可能再跑一遍，因此按分量测量而非整机 A/B）。
fn legacy_components(device: &str, iters: u32) -> Result<(), Box<dyn std::error::Error>> {
    struct Card(File);
    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl drm::Device for Card {}
    impl drm::control::Device for Card {}

    use drm::buffer::{Buffer, DrmFourcc};
    use drm::control::Device as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)?;
    let card = Card(file);

    // 分量 1：旧路径每帧 mmap/munmap + 逐行拷贝（4K XRGB，36.9 MB）
    let mut dumb = card.create_dumb_buffer((3840, 2400), DrmFourcc::Xrgb8888, 32)?;
    let pitch = dumb.pitch() as usize;
    let src = vec![9u8; 3840 * 2400 * 4];
    // 预热
    {
        let mut m = card.map_dumb_buffer(&mut dumb)?;
        m.as_mut().copy_from_slice(&src);
    }
    let c0 = cpu_ticks();
    let t0 = Instant::now();
    for _ in 0..iters {
        let mut m = card.map_dumb_buffer(&mut dumb)?;
        let dst = m.as_mut();
        for y in 0..2400usize {
            let s = y * 3840 * 4;
            let d = y * pitch;
            dst[d..d + 3840 * 4].copy_from_slice(&src[s..s + 3840 * 4]);
        }
    }
    let legacy_total = t0.elapsed().as_secs_f64() * 1e3 / f64::from(iters);
    let legacy_cpu = cpu_ticks().saturating_sub(c0) as f64 * 10.0 / f64::from(iters);
    println!(
        "BENCH legacy_mmap_plus_rowcopy per_frame_ms={legacy_total:.3} per_frame_cpu_ms={legacy_cpu:.3} (4K XRGB 36.9MB)"
    );

    // 分量 2：旧路径每帧重建属性映射（connector/crtc/plane 各一次）
    let resources = card.resource_handles()?;
    let planes: Vec<_> = card.plane_handles()?.iter().copied().take(8).collect();
    let connectors: Vec<_> = resources.connectors().iter().copied().take(6).collect();
    let crtcs: Vec<_> = resources.crtcs().iter().copied().take(4).collect();
    let c1 = cpu_ticks();
    let t1 = Instant::now();
    for _ in 0..iters {
        for ph in &planes {
            let set = card.get_properties(*ph)?;
            let map = set.as_hashmap(&card)?;
            std::hint::black_box(map.len());
        }
        for ch in &connectors {
            let set = card.get_properties(*ch)?;
            let map = set.as_hashmap(&card)?;
            std::hint::black_box(map.len());
        }
        for ch in &crtcs {
            let set = card.get_properties(*ch)?;
            let map = set.as_hashmap(&card)?;
            std::hint::black_box(map.len());
        }
    }
    let prop_total = t1.elapsed().as_secs_f64() * 1e3 / f64::from(iters);
    let prop_cpu = cpu_ticks().saturating_sub(c1) as f64 * 10.0 / f64::from(iters);
    println!(
        "BENCH legacy_property_enum per_frame_ms={prop_total:.3} per_frame_cpu_ms={prop_cpu:.3} \
         planes={} connectors={} crtcs={} (旧路径每帧重建 3 类对象属性表)",
        planes.len(),
        connectors.len(),
        crtcs.len()
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("legacy") {
        let iters: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(120);
        let device = args.get(2).cloned().unwrap_or_else(|| "auto".to_string());
        let device = if device == "auto" {
            "/dev/dri/card1".to_string()
        } else {
            device
        };
        return legacy_components(&device, iters);
    }
    if args.first().map(String::as_str) == Some("stats") {
        let backend = DrmKmsBackend::open("auto")?;
        for (out, st) in backend.scanout_stats() {
            println!(
                "BENCH scanout_stats={out:?} PoolStats{{allocations: {}, mappings: {}, reuses: {}, \
             reclaims: {}, max_in_use: {}, rejected_writes: {}, blocked_recreates: {}, \
             exhausted_waits: {}, exhausted_drops: {}, forced_releases: {}, uploaded_bytes: {}}}",
                st.allocations,
                st.mappings,
                st.reuses,
                st.reclaims,
                st.max_in_use,
                st.rejected_writes,
                st.blocked_recreates,
                st.exhausted_waits,
                st.exhausted_drops,
                st.forced_releases,
                st.uploaded_bytes,
            );
        }
        return Ok(());
    }
    let frames: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(180);
    let fmt = args.get(1).cloned().unwrap_or_else(|| "nv12".to_string());
    let sw: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3840);
    let sh: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2160);

    let mut backend = DrmKmsBackend::open("auto")?;
    let output: OutputId = match backend.default_output(Some("DP-1")) {
        Ok(o) => o,
        Err(_) => backend.default_output(None)?,
    };
    let mode = backend.resolve_mode(&output, &ModeSelection::Preferred)?;
    backend.set_mode(output.clone(), mode)?;
    let (mw, mh) = (mode.width, mode.height);

    // cover 适配（等价 liuguang_core::fit::fit_visible_rects 的 cover 结果）：
    // 源按 max(mw/sw, mh/sh) 放大，居中裁切。
    let scale = f64::max(f64::from(mw) / f64::from(sw), f64::from(mh) / f64::from(sh));
    let src_w = (f64::from(mw) / scale).min(f64::from(sw));
    let src_h = (f64::from(mh) / scale).min(f64::from(sh));
    let src_x = (f64::from(sw) - src_w) / 2.0;
    let src_y = (f64::from(sh) - src_h) / 2.0;
    let src = (
        (src_x * 65536.0).round().max(0.0) as u64,
        (src_y * 65536.0).round().max(0.0) as u64,
        (src_w * 65536.0).round().max(65536.0) as u64,
        (src_h * 65536.0).round().max(65536.0) as u64,
    );
    let dst = (0i64, 0i64, u64::from(mw), u64::from(mh));

    let frame = match fmt.as_str() {
        "nv12" => nv12(sw, sh),
        "xrgb" => rgba_like(sw, sh, PixelFormat::Xrgb8888),
        "rgba" => rgba_like(sw, sh, PixelFormat::Rgba8888),
        "argb" => rgba_like(sw, sh, PixelFormat::Argb8888),
        other => return Err(format!("未知格式 {other}（nv12|xrgb|rgba|argb）").into()),
    };
    let bytes = frame.data.len() as u64;

    println!(
        "BENCH output={output} mode={mw}x{mh}@{:.1} src={sw}x{sh} fmt={fmt} bytes={bytes} frames={frames}",
        mode.refresh_hz()
    );
    println!(
        "BENCH src_rect={:?} dst_rect={:?}",
        (src.0 >> 16, src.1 >> 16, src.2 >> 16, src.3 >> 16),
        dst
    );

    // 预热：第一帧承担 CRTC/plane 绑定与模式 blob 建立。
    for _ in 0..3 {
        let t = Instant::now();
        let r = backend.present_fitted(output.clone(), DisplayFrame::Cpu(frame.clone()), src, dst);
        match r {
            Ok(_) => println!(
                "BENCH warmup ok wall_ms={:.3}",
                t.elapsed().as_secs_f64() * 1e3
            ),
            Err(e) => {
                println!("BENCH warmup FAILED: {e}");
                return Ok(());
            }
        }
    }

    let mut wall = Vec::with_capacity(frames as usize);
    let mut cpu = Vec::with_capacity(frames as usize);
    let mut errors = 0u32;
    let mut first_err = String::new();
    for _ in 0..frames {
        let c0 = cpu_ticks();
        let t0 = Instant::now();
        let r = backend.present_fitted(output.clone(), DisplayFrame::Cpu(frame.clone()), src, dst);
        let dt = t0.elapsed().as_secs_f64() * 1e3;
        let dc = cpu_ticks().saturating_sub(c0) as f64 * 10.0; // USER_HZ=100 → 10ms/ticks
        match r {
            Ok(_) => {
                wall.push(dt);
                cpu.push(dc);
            }
            Err(e) => {
                errors += 1;
                if first_err.is_empty() {
                    first_err = e.to_string();
                }
            }
        }
    }

    if !wall.is_empty() {
        let mut ws = wall.clone();
        ws.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut cs = cpu.clone();
        cs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        println!(
            "BENCH wall_ms mean={:.3} p50={:.3} p90={:.3} max={:.3}",
            mean(&ws),
            percentile(&ws, 0.5),
            percentile(&ws, 0.9),
            ws[ws.len() - 1]
        );
        println!(
            "BENCH cpu_ms_per_frame mean={:.3} p50={:.3} p90={:.3} max={:.3}",
            mean(&cs),
            percentile(&cs, 0.5),
            percentile(&cs, 0.9),
            cs[cs.len() - 1]
        );
        let fps_capacity = 1000.0 / mean(&cs).max(0.001);
        println!("BENCH cpu_bound_fps={fps_capacity:.1} (单核)");
    }
    if errors > 0 {
        println!("BENCH errors={errors} first={first_err}");
    }
    for (out, st) in backend.scanout_stats() {
        println!(
            "BENCH scanout_stats={out:?} PoolStats{{allocations: {}, mappings: {}, reuses: {}, \
             reclaims: {}, max_in_use: {}, rejected_writes: {}, blocked_recreates: {}, \
             exhausted_waits: {}, exhausted_drops: {}, forced_releases: {}, uploaded_bytes: {}}}",
            st.allocations,
            st.mappings,
            st.reuses,
            st.reclaims,
            st.max_in_use,
            st.rejected_writes,
            st.blocked_recreates,
            st.exhausted_waits,
            st.exhausted_drops,
            st.forced_releases,
            st.uploaded_bytes,
        );
    }
    println!("BENCH commit_stats={:?}", backend.commit_stats());
    Ok(())
}
