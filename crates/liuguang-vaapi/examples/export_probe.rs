//! 真机探针：解码一帧并导出 DMA-BUF，打印描述符。
//!
//! 用法：
//! ```text
//! cargo run -p liuguang-vaapi --example export_probe -- <视频> [渲染节点] [帧数]
//! ```
//!
//! 这是 P2 的验收工具：它只验证"能不能从解码器拿到 fd + modifier"，不涉及显示。

#![allow(unsafe_code)] // mmap 导出出来的 dma-buf 读像素，只在探针里用
//!
//! 加 `--ppm <路径>` 时，还会把**导出的 dma-buf 本身** mmap 进来、按 `strides/offsets`
//! 做一次 NV12→RGB 转换写出 PPM。这是判断"颜色不对到底是我们导错了、还是合成器不会
//! 渲染 NV12"的决定性自检：如果 PPM 与 ffmpeg 直接解码的帧一致，我们的数据就是对的。
use std::path::PathBuf;

use liuguang_media::video::FrameProducer as _;
use liuguang_vaapi::{VaapiProducer, export};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| {
        eprintln!("用法: export_probe <视频> [渲染节点] [帧数]");
        std::process::exit(2);
    }));
    let node = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "/dev/dri/renderD129".to_string()),
    );
    let mut wanted: usize = 3;
    let mut ppm: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ppm" => ppm = args.next().map(PathBuf::from),
            other => {
                if let Ok(value) = other.parse() {
                    wanted = value;
                }
            }
        }
    }
    println!("视频: {}", path.display());
    println!("节点: {}", node.display());
    println!("可用渲染节点: {:?}", liuguang_vaapi::render_nodes());
    println!(
        "显示 GPU 优先: {:?}",
        liuguang_vaapi::render_nodes_preferring_display()
    );
    println!("该节点支持 VA-API: {}", liuguang_vaapi::probe_node(&node));

    let mut producer = match VaapiProducer::open(&path, &node) {
        Ok(producer) => producer,
        Err(error) => {
            eprintln!("打开失败: {error}");
            std::process::exit(1);
        }
    };
    println!("流信息: {:?}", producer.info());

    let mut last_dmabuf: Option<liuguang_core::types::DmaBufFrame> = None;
    let mut shown = 0usize;
    let mut attempts = 0usize;
    while shown < wanted && attempts < 2000 {
        attempts += 1;
        match producer.next_producer_frame() {
            Ok(liuguang_media::video::VideoPoll::Frame { pts, frame }) => {
                shown += 1;
                match frame {
                    liuguang_core::types::DisplayFrame::DmaBuf(frame) => {
                        last_dmabuf = Some(frame.clone());
                        println!(
                            "帧 #{shown}: pts={pts:?} {}x{} fourcc=0x{:08X} modifier=0x{:016X} \
                             fds={} object_index={:?} strides={:?} offsets={:?}",
                            frame.width,
                            frame.height,
                            frame.fourcc,
                            frame.modifier,
                            frame.fds.len(),
                            frame.object_index,
                            frame.strides,
                            frame.offsets,
                        );
                        for (index, fd) in frame.fds.iter().enumerate() {
                            println!(
                                "  object {index}: fd={}",
                                std::os::fd::AsRawFd::as_raw_fd(fd.as_ref())
                            );
                        }
                    }
                    other => println!("帧 #{shown}: 非 DMA-BUF 帧: {other:?}"),
                }
            }
            Ok(liuguang_media::video::VideoPoll::NeedMore) => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(liuguang_media::video::VideoPoll::Ended) => {
                println!("流结束");
                break;
            }
            Err(error) => {
                eprintln!("解码失败: {error}");
                std::process::exit(1);
            }
        }
    }
    println!(
        "导出 {} 帧；未归还={} 已导出={}",
        shown,
        producer.inflight(),
        producer.exported_frames()
    );
    if let (Some(path), Some(frame)) = (ppm, last_dmabuf) {
        match dump_ppm(&path, &frame) {
            Ok(()) => println!(
                "已写出 PPM: {}（这就是我们交给合成器的像素）",
                path.display()
            ),
            Err(error) => eprintln!("写 PPM 失败: {error}"),
        }
    }
    // 显式验证一次导出 API 的错误路径（不依赖真实表面）：未知 fourcc 应报错。
    let _ = export::DRM_FORMAT_NV12;
}

/// 把导出的 NV12 dma-buf 按 `strides/offsets/object_index` 读出来，转成 RGB 写 PPM。
///
/// 只支持 NV12（本机 VA-API 解码输出的就是它）。映射长度按 `stride × height` 取，
/// 足够覆盖一个平面；`mmap` 对 dma-buf 是否可用取决于驱动，失败会如实返回错误。
fn dump_ppm(
    path: &std::path::Path,
    frame: &liuguang_core::types::DmaBufFrame,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    if frame.object_count() < 2 {
        return Err(format!(
            "该帧只有 {} 个 object，不是 NV12 双平面",
            frame.object_count()
        ));
    }
    let width = frame.width as usize;
    let height = frame.height as usize;

    let mut maps: Vec<Vec<u8>> = Vec::new();
    for index in 0..frame.object_count() {
        let fd = frame.fds.get(index).ok_or("fds 数量不足")?;
        let stride = frame.strides[index.min(3)] as usize;
        // NV12：object 0 是 Y 平面（stride × height），object 1 是 UV 平面（stride × height/2）。
        // **不能**都按 stride × height 映射：BO 比这个短，mmap 会直接 EINVAL。
        let len = if index == 0 {
            stride * height
        } else {
            stride * (height / 2).max(1)
        };
        // SAFETY: mmap 一段只读共享内存，长度由我们算出的平面大小决定，地址随后校验。
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "mmap object {index} 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: 上面的 mmap 成功且长度是 len，这里复制成 Vec 后立即不再使用该映射。
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec();
        unsafe { libc::munmap(ptr, len) };
        maps.push(bytes);
    }

    let y_object = frame.object_index[0] as usize;
    let uv_object = frame.object_index[1] as usize;
    let y_stride = frame.strides[0] as usize;
    let uv_stride = frame.strides[1] as usize;
    let y_off = frame.offsets[0] as usize;
    let uv_off = frame.offsets[1] as usize;

    let mut out = Vec::with_capacity(width * height * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for y in 0..height {
        for x in 0..width {
            let yv = maps[y_object][y_off + y * y_stride + x] as i32;
            // NV12（不是 NV21）：色度平面按 U,V 交错存放 ⇒ 第一个字节是 Cb(U)，第二个是 Cr(V)。
            let u = maps[uv_object]
                .get(uv_off + (y / 2) * uv_stride + (x / 2) * 2)
                .copied()
                .unwrap_or(128) as i32;
            let v = maps[uv_object]
                .get(uv_off + (y / 2) * uv_stride + (x / 2) * 2 + 1)
                .copied()
                .unwrap_or(128) as i32;
            let c = yv - 16;
            let d = u - 128;
            let e = v - 128;
            let r = (298 * c + 409 * e + 128) >> 8;
            let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
            let b = (298 * c + 516 * d + 128) >> 8;
            out.push(r.clamp(0, 255) as u8);
            out.push(g.clamp(0, 255) as u8);
            out.push(b.clamp(0, 255) as u8);
        }
    }
    std::fs::write(path, out).map_err(|error| error.to_string())
}
