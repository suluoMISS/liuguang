//! 媒体头探测：读文件头 / 容器索引块，填 [`MediaDetails`]。
//!
//! 为什么自己写解析而不用 gstreamer 的 discoverer：工作区没有
//! `gstreamer-pbutils` 依赖（离线构建，registry 缓存里也没有），而且扫描线程要
//! **可预测、可超时**——discoverer 会拉起完整 pipeline 解码，几百个文件就能把
//! 扫描拖爆。这里只读头部/索引块，字节数有硬上限，失败一律 `None`。
//!
//! 覆盖范围（够 GUI 的属性面板用）：
//! - 图片：[`parse_image`] 认 PNG / JPEG / WebP（VP8、VP8L、VP8X）/ GIF / BMP；
//! - 视频：[`probe_video`] 认 ISO-BMFF（mp4/mov/m4v）与 Matroska/WebM。
//!
//! 语义约定：解析不出来 ⇒ `None`，媒体**照旧入索引**（宁可"详情未知"也不让一个
//! 畸形文件把整次扫描变成错误）。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use liuguang_ipc::protocol::MediaDetails;

/// 图片最多读这么多字节：所有常见头都在前几十字节，留足余量给大 EXIF。
pub const IMAGE_HEAD_BYTES: u64 = 64 * 1024;

/// 视频索引块（mp4 的 `moov`）字节上限。长片的 `moov` 可达数百 KB，16 MiB 足够；
/// 超过就当探测失败——绝不为了元数据把整部电影读进内存。
pub const VIDEO_INDEX_BYTES: u64 = 16 * 1024 * 1024;

/// Matroska 头部扫描上限：`Info` / `Tracks` 都在第一个 Cluster 之前。
const MKV_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// 按**扩展名**推断种类并探测；解析失败返回 `None`。
#[must_use]
pub fn probe(path: &Path, kind: &str) -> Option<MediaDetails> {
    if kind == "image" {
        probe_image(path)
    } else {
        probe_video(path)
    }
}

/// 图片：读头部若干字节后解析。
#[must_use]
pub fn probe_image(path: &Path) -> Option<MediaDetails> {
    let file = File::open(path).ok()?;
    let mut head = Vec::new();
    file.take(IMAGE_HEAD_BYTES).read_to_end(&mut head).ok()?;
    parse_image(&head)
}

/// 视频：按容器特征分派到 ISO-BMFF / Matroska。
#[must_use]
pub fn probe_video(path: &Path) -> Option<MediaDetails> {
    let mut file = File::open(path).ok()?;
    let mut magic = [0u8; 8];
    let n = file.read(&mut magic).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    if n >= 8 && (magic[4..8] == *b"ftyp" || magic[4..8] == *b"moov" || magic[4..8] == *b"free") {
        return probe_iso_bmff(file);
    }
    if magic[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        let mut head = Vec::new();
        file.take(MKV_SCAN_BYTES).read_to_end(&mut head).ok()?;
        return parse_mkv(&head);
    }
    None
}

// ---------------------------------------------------------------- 通用小工具

fn be16(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*buf.get(at)?, *buf.get(at + 1)?]))
}

fn be32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *buf.get(at)?,
        *buf.get(at + 1)?,
        *buf.get(at + 2)?,
        *buf.get(at + 3)?,
    ]))
}

fn be64(buf: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes([
        *buf.get(at)?,
        *buf.get(at + 1)?,
        *buf.get(at + 2)?,
        *buf.get(at + 3)?,
        *buf.get(at + 4)?,
        *buf.get(at + 5)?,
        *buf.get(at + 6)?,
        *buf.get(at + 7)?,
    ]))
}

fn le16(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*buf.get(at)?, *buf.get(at + 1)?]))
}

fn le24(buf: &[u8], at: usize) -> Option<u32> {
    let b = [*buf.get(at)?, *buf.get(at + 1)?, *buf.get(at + 2)?];
    Some(u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
}

fn le32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *buf.get(at)?,
        *buf.get(at + 1)?,
        *buf.get(at + 2)?,
        *buf.get(at + 3)?,
    ]))
}

fn image_details(
    width: u32,
    height: u32,
    codec: &str,
    pix_fmt: Option<&str>,
) -> Option<MediaDetails> {
    if width == 0 || height == 0 {
        return None;
    }
    Some(MediaDetails {
        width,
        height,
        fps_millihz: None,
        duration_ms: None,
        codec: Some(codec.to_string()),
        pix_fmt: pix_fmt.map(str::to_string),
    })
}

// ---------------------------------------------------------------- 图片

/// PNG 签名。
const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 从图片头解析尺寸（不认识就 `None`）。
#[must_use]
pub fn parse_image(buf: &[u8]) -> Option<MediaDetails> {
    if buf.len() >= 16 && buf[..8] == PNG_SIG && &buf[12..16] == b"IHDR" {
        return parse_png(buf);
    }
    if buf.starts_with(&[0xFF, 0xD8]) {
        return parse_jpeg(buf);
    }
    if buf.len() > 12 && buf.starts_with(b"RIFF") && &buf[8..12] == b"WEBP" {
        return parse_webp(buf);
    }
    if buf.starts_with(b"GIF87a") || buf.starts_with(b"GIF89a") {
        return parse_gif(buf);
    }
    if buf.starts_with(b"BM") {
        return parse_bmp(buf);
    }
    None
}

/// PNG：`IHDR` 里的宽高 + 位深/颜色类型。
fn parse_png(buf: &[u8]) -> Option<MediaDetails> {
    let width = be32(buf, 16)?;
    let height = be32(buf, 20)?;
    let depth = *buf.get(24)?;
    let color = *buf.get(25)?;
    let pix = match color {
        0 => Some(format!("gray{depth}")),
        2 => Some(format!("rgb{depth}")),
        3 => Some(format!("palette{depth}")),
        4 => Some(format!("graya{depth}")),
        6 => Some(format!("rgba{depth}")),
        _ => None,
    };
    image_details(width, height, "png", pix.as_deref())
}

/// JPEG：走标记段找 `SOFn`（顺带跳过量化/Huffman 段）。
fn parse_jpeg(buf: &[u8]) -> Option<MediaDetails> {
    let mut at = 2usize; // 跳过 SOI
    while at + 4 <= buf.len() {
        if buf[at] != 0xFF {
            at += 1;
            continue;
        }
        let marker = buf[at + 1];
        // 无长度的独立标记。
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) || marker == 0xFF {
            at += if marker == 0xFF { 1 } else { 2 };
            continue;
        }
        let len = usize::from(be16(buf, at + 2)?);
        if len < 2 {
            return None;
        }
        let is_sof = matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        );
        if is_sof {
            let height = u32::from(be16(buf, at + 5)?);
            let width = u32::from(be16(buf, at + 7)?);
            let components = *buf.get(at + 9)?;
            let pix = match components {
                1 => Some("gray8"),
                4 => Some("cmyk8"),
                // 三通道的上采样方式写在后续 SOS 里，这里不猜。
                _ => None,
            };
            return image_details(width, height, "jpeg", pix);
        }
        at = at.checked_add(2 + len)?;
    }
    None
}

/// WebP：`VP8 `（有损）/ `VP8L`（无损）/ `VP8X`（扩展）三种载荷各自编码尺寸。
fn parse_webp(buf: &[u8]) -> Option<MediaDetails> {
    let chunk = buf.get(12..16)?;
    match chunk {
        b"VP8 " => {
            // 帧标签(3) + 起始码 9D 01 2A(3) + 宽(2) + 高(2)，各 14 位。
            if buf.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            let width = u32::from(le16(buf, 26)? & 0x3FFF);
            let height = u32::from(le16(buf, 28)? & 0x3FFF);
            image_details(width, height, "webp", Some("yuv420p"))
        }
        b"VP8L" => {
            if *buf.get(20)? != 0x2F {
                return None;
            }
            let bits = le32(buf, 21)?;
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            image_details(width, height, "webp", Some("argb8"))
        }
        b"VP8X" => {
            let width = le24(buf, 24)? + 1;
            let height = le24(buf, 27)? + 1;
            image_details(width, height, "webp", None)
        }
        _ => None,
    }
}

/// GIF：逻辑屏幕描述符。
fn parse_gif(buf: &[u8]) -> Option<MediaDetails> {
    let width = u32::from(le16(buf, 6)?);
    let height = u32::from(le16(buf, 8)?);
    image_details(width, height, "gif", Some("palette8"))
}

/// BMP：`BITMAPINFOHEADER`（≥40）或 `BITMAPCOREHEADER`（12）。
fn parse_bmp(buf: &[u8]) -> Option<MediaDetails> {
    let header = le32(buf, 14)?;
    let (width, height, pix) = if header >= 40 {
        let w = le32(buf, 18)?;
        let h = le32(buf, 22)?;
        let bits = le16(buf, 28)?;
        let pix = match bits {
            24 => Some("bgr8"),
            32 => Some("bgra8"),
            _ => None,
        };
        (w, h, pix)
    } else {
        (u32::from(le16(buf, 18)?), u32::from(le16(buf, 20)?), None)
    };
    // 高度为负表示自上而下存储，尺寸取绝对值。
    let height = if height > i32::MAX as u32 {
        (!height).wrapping_add(1)
    } else {
        height
    };
    image_details(width, height, "bmp", pix)
}

// ---------------------------------------------------------------- ISO-BMFF (mp4/mov)

/// 顶层 box 遍历：只把 `moov` 读进内存，其余按 size 跳过。
fn probe_iso_bmff(mut file: File) -> Option<MediaDetails> {
    let file_len = file.metadata().ok()?.len();
    let mut at = 0u64;
    while at + 8 <= file_len {
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut header = [0u8; 16];
        let n = file.read(&mut header).ok()?;
        if n < 8 {
            return None;
        }
        let small = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let name = [header[4], header[5], header[6], header[7]];
        let mut payload_at = at + 8;
        let mut box_len = u64::from(small);
        if small == 1 {
            if n < 16 {
                return None;
            }
            box_len = u64::from_be_bytes([
                header[8], header[9], header[10], header[11], header[12], header[13], header[14],
                header[15],
            ]);
            payload_at = at + 16;
        } else if small == 0 {
            box_len = file_len - at;
        }
        let header_len = payload_at - at;
        if box_len < header_len || box_len > file_len.saturating_sub(at) {
            return None;
        }
        let payload_len = box_len - header_len;
        if &name == b"moov" {
            if payload_len > VIDEO_INDEX_BYTES {
                return None;
            }
            let mut payload = Vec::with_capacity(payload_len as usize);
            file.seek(SeekFrom::Start(payload_at)).ok()?;
            file.take(payload_len).read_to_end(&mut payload).ok()?;
            return parse_moov(&payload);
        }
        at = at.checked_add(box_len)?;
    }
    None
}

/// 在 `buf` 里找第一个名为 `name` 的子 box 的**载荷**。
fn find_box<'a>(buf: &'a [u8], name: &[u8; 4]) -> Option<&'a [u8]> {
    let mut at = 0usize;
    while at + 8 <= buf.len() {
        let small = be32(buf, at)?;
        let this = buf.get(at + 4..at + 8)?;
        let mut payload_at = at + 8;
        let mut len = usize::try_from(small).ok()?;
        if small == 1 {
            len = usize::try_from(be64(buf, at + 8)?).ok()?;
            payload_at = at + 16;
        } else if small == 0 {
            len = buf.len() - at;
        }
        if len < payload_at - at || payload_at > buf.len() {
            return None;
        }
        let end = at.saturating_add(len).min(buf.len());
        if this == name {
            return buf.get(payload_at..end);
        }
        at = at.checked_add(len)?;
    }
    None
}

/// 沿路径下钻（每级取第一个同名 box）。
fn dig<'a>(buf: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let mut cur = buf;
    for name in path {
        cur = find_box(cur, name)?;
    }
    Some(cur)
}

/// 解析 `moov` 载荷：`mvhd` 出时长，`trak`→`mdhd`/`stsd`/`stts` 出视频参数。
#[must_use]
pub fn parse_moov(moov: &[u8]) -> Option<MediaDetails> {
    // 时长：`mvhd` 的 timescale 与 duration（64 位版本字段位移不同）。
    let (movie_scale, movie_duration) = match dig(moov, &[b"mvhd"]) {
        Some(mvhd) if *mvhd.first()? == 1 => (be32(mvhd, 20)?, be64(mvhd, 24)?),
        Some(mvhd) => (
            be32(mvhd, 12)?,
            u64::from(be32(mvhd, 16).unwrap_or_default()),
        ),
        None => (0, 0),
    };
    let duration_ms = if movie_scale > 0 && movie_duration > 0 {
        u64::try_from(u128::from(movie_duration) * 1000 / u128::from(movie_scale)).ok()
    } else {
        None
    };

    // 视频轨：`hdlr` 的 handler_type == "vide"。
    let video_trak = video_trak(moov)?;
    let (width, height) = match dig(video_trak, &[b"tkhd"]) {
        Some(tkhd) => {
            // tkhd 的宽高是 16.16 定点；很多文件写 0（真实尺寸在 stsd 里）。
            let w = be32(tkhd, tkhd.len().checked_sub(8)?)? >> 16;
            let h = be32(tkhd, tkhd.len().checked_sub(4)?)? >> 16;
            (w, h)
        }
        None => (0, 0),
    };
    let (stsd_w, stsd_h, codec) = sample_description(video_trak).unwrap_or((0, 0, None));
    let width = if stsd_w > 0 { stsd_w } else { width };
    let height = if stsd_h > 0 { stsd_h } else { height };

    let fps_millihz = track_fps_millihz(video_trak);
    if width == 0 || height == 0 {
        return None;
    }
    Some(MediaDetails {
        width,
        height,
        fps_millihz,
        duration_ms,
        codec,
        pix_fmt: None,
    })
}

/// 找出第一条 handler_type 为 `vide` 的 `trak` 载荷。
fn video_trak(moov: &[u8]) -> Option<&[u8]> {
    let mut at = 0usize;
    while let Some((name, payload, len)) = box_at(moov, at) {
        if &name == b"trak"
            && let Some(hdlr) = dig(payload, &[b"mdia", b"hdlr"])
            && hdlr.get(8..12) == Some(b"vide".as_slice())
        {
            return Some(payload);
        }
        at = at.checked_add(len)?;
    }
    None
}

/// 从 `at` 处读一个 box：返回（名字、载荷、整段长度）。
fn box_at(buf: &[u8], at: usize) -> Option<([u8; 4], &[u8], usize)> {
    let small = be32(buf, at)?;
    let name: [u8; 4] = buf.get(at + 4..at + 8)?.try_into().ok()?;
    let (payload_at, len) = if small == 1 {
        (at + 16, usize::try_from(be64(buf, at + 8)?).ok()?)
    } else if small == 0 {
        (at + 8, buf.len() - at)
    } else {
        (at + 8, usize::try_from(small).ok()?)
    };
    if len < payload_at - at || payload_at > buf.len() {
        return None;
    }
    let end = at.saturating_add(len).min(buf.len());
    Some((name, buf.get(payload_at..end)?, len))
}

/// `stsd` 第一个采样描述：宽、高、编码四字符码。
fn sample_description(trak: &[u8]) -> Option<(u32, u32, Option<String>)> {
    let stsd = dig(trak, &[b"mdia", b"minf", b"stbl", b"stsd"])?;
    // version/flags(4) + entry_count(4)，随后是第一个 entry box。
    let entry = box_at(stsd, 8)?;
    let codec = codec_name(&entry.0);
    // VisualSampleEntry：width 在载荷 +24、height 在 +26。
    let width = u32::from(be16(entry.1, 24)?);
    let height = u32::from(be16(entry.1, 26)?);
    Some((width, height, codec))
}

/// 四字符码 → 常见编码名（认不出来就原样给出四字符码）。
fn codec_name(fourcc: &[u8; 4]) -> Option<String> {
    let name = match fourcc {
        b"avc1" | b"avc3" => "h264",
        b"hvc1" | b"hev1" => "hevc",
        b"av01" => "av1",
        b"vp09" => "vp9",
        b"mp4v" => "mpeg4",
        b"ap4h" | b"apcn" => "prores",
        b"mjpg" => "mjpeg",
        _ => {
            let text: String = fourcc
                .iter()
                .filter(|b| b.is_ascii_graphic())
                .map(|b| char::from(*b))
                .collect();
            if text.len() != 4 {
                return None;
            }
            return Some(text);
        }
    };
    Some(name.to_string())
}

/// 帧率 = 总样本数 × 轨道 timescale ÷ 总时长（`stts` 求和，整数运算）。
fn track_fps_millihz(trak: &[u8]) -> Option<u32> {
    let timescale = match dig(trak, &[b"mdia", b"mdhd"]) {
        Some(mdhd) if *mdhd.first()? == 1 => be32(mdhd, 20)?,
        Some(mdhd) => be32(mdhd, 12)?,
        None => return None,
    };
    let stts = dig(trak, &[b"mdia", b"minf", b"stbl", b"stts"])?;
    let count = be32(stts, 4)?;
    let mut samples = 0u128;
    let mut units = 0u128;
    for i in 0..usize::try_from(count).ok()? {
        let base = 8 + i * 8;
        let n = u128::from(be32(stts, base)?);
        let delta = u128::from(be32(stts, base + 4)?);
        samples += n;
        units += n * delta;
    }
    if samples == 0 || units == 0 || timescale == 0 {
        return None;
    }
    let millihz = samples * u128::from(timescale) * 1000 / units;
    u32::try_from(millihz).ok()
}

// ---------------------------------------------------------------- Matroska / WebM

/// EBML 元素 ID（保留标记位，直接用原始数值比较）。
const EBML_HEADER: u64 = 0x1A45_DFA3;
const SEGMENT: u64 = 0x1853_8067;
const INFO: u64 = 0x1549_A966;
const TRACKS: u64 = 0x1654_AE6B;
const TRACK_ENTRY: u64 = 0xAE;
const VIDEO: u64 = 0xE0;
const TRACK_TYPE: u64 = 0x83;
const CODEC_ID: u64 = 0x86;
// EBML ID 是 1–4 字节的变长整数；这三个是 3 字节 ID，按 4 位一组写出完整宽度，
// 避免 `0x23E3_83` 这类"看起来像两字节"的分组。
const DEFAULT_DURATION: u64 = 0x0023_E383;
const FRAME_RATE: u64 = 0x0023_83E3;
const PIXEL_WIDTH: u64 = 0xB0;
const PIXEL_HEIGHT: u64 = 0xBA;
const DISPLAY_WIDTH: u64 = 0x54B0;
const DISPLAY_HEIGHT: u64 = 0x54BA;
const TIMECODE_SCALE: u64 = 0x002A_D7B1;
const DURATION: u64 = 0x4489;

/// 一条轨道的关键元数据。
#[derive(Default)]
struct Track {
    kind: Option<u64>,
    codec: Option<String>,
    default_duration_ns: Option<u64>,
    frame_rate: Option<f64>,
    width: Option<u64>,
    height: Option<u64>,
    display_width: Option<u64>,
    display_height: Option<u64>,
}

/// Matroska 头部扫描的状态。
#[derive(Default)]
struct Mkv {
    timecode_scale: Option<u64>,
    duration: Option<f64>,
    tracks: Vec<Track>,
    current: Option<Track>,
}

/// 解析 Matroska/WebM 头部（`Info` + `Tracks`）；只认第一帧之前的内容。
#[must_use]
pub fn parse_mkv(buf: &[u8]) -> Option<MediaDetails> {
    let mut mkv = Mkv::default();
    walk_ebml(buf, 0, buf.len(), 0, &mut mkv)?;
    let video = mkv.tracks.into_iter().find(|t| t.kind == Some(1))?;
    let width = video
        .display_width
        .filter(|w| *w > 0)
        .or(video.width)
        .filter(|w| *w > 0)?;
    let height = video
        .display_height
        .filter(|h| *h > 0)
        .or(video.height)
        .filter(|h| *h > 0)?;
    let fps_millihz = video
        .default_duration_ns
        .filter(|ns| *ns > 0)
        .and_then(|ns| u32::try_from(1_000_000_000_000u64 / ns).ok())
        .or_else(|| {
            video
                .frame_rate
                .filter(|f| *f > 0.0)
                .map(|f| (f * 1000.0).round() as u32)
        })
        .filter(|fps| *fps > 0);
    let duration_ms = mkv.duration.filter(|d| *d > 0.0).map(|d| {
        // Matroska 的 Duration 是以 TimecodeScale 为单位的浮点数（默认 1_000_000ns）。
        let scale = mkv.timecode_scale.unwrap_or(1_000_000);
        let ms = d * scale as f64 / 1_000_000.0;
        ms.max(0.0) as u64
    });
    Some(MediaDetails {
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        fps_millihz,
        duration_ms,
        codec: video.codec,
        pix_fmt: None,
    })
}

/// 递归遍历 EBML 主元素；只认我们关心的那几个，其它按 size 跳过。
fn walk_ebml(buf: &[u8], mut at: usize, end: usize, depth: u32, mkv: &mut Mkv) -> Option<()> {
    if depth > 8 {
        return Some(());
    }
    let end = end.min(buf.len());
    while at < end {
        let (id, id_len) = read_vint(buf, at, end, true)?;
        let (size, size_len) = read_vint(buf, at + id_len, end, false)?;
        let payload_at = at.checked_add(id_len + size_len)?;
        let payload_end = if size == u64::MAX || size > (end - payload_at) as u64 {
            // 未知长度（流式写入的 Segment/Cluster）：吃到当前缓冲末尾。
            end
        } else {
            payload_at + size as usize
        };
        if payload_end > end || payload_at > payload_end {
            return Some(());
        }
        let payload = buf.get(payload_at..payload_end)?;
        match id {
            EBML_HEADER => {}
            SEGMENT | INFO | TRACKS => {
                walk_ebml(buf, payload_at, payload_end, depth + 1, mkv)?;
            }
            TRACK_ENTRY => {
                mkv.current = Some(Track::default());
                walk_ebml(buf, payload_at, payload_end, depth + 1, mkv)?;
                if let Some(track) = mkv.current.take() {
                    mkv.tracks.push(track);
                }
            }
            VIDEO => {
                walk_ebml(buf, payload_at, payload_end, depth + 1, mkv)?;
            }
            TRACK_TYPE => {
                if let Some(t) = mkv.current.as_mut() {
                    t.kind = read_uint(payload);
                }
            }
            CODEC_ID => {
                if let Some(t) = mkv.current.as_mut() {
                    t.codec = std::str::from_utf8(payload)
                        .ok()
                        .map(|s| s.trim_end_matches('\0').to_string())
                        .map(|s| matroska_codec(&s));
                }
            }
            DEFAULT_DURATION => {
                if let Some(t) = mkv.current.as_mut() {
                    t.default_duration_ns = read_uint(payload);
                }
            }
            FRAME_RATE => {
                if let Some(t) = mkv.current.as_mut() {
                    t.frame_rate = read_float(payload);
                }
            }
            PIXEL_WIDTH => {
                if let Some(t) = mkv.current.as_mut() {
                    t.width = read_uint(payload);
                }
            }
            PIXEL_HEIGHT => {
                if let Some(t) = mkv.current.as_mut() {
                    t.height = read_uint(payload);
                }
            }
            DISPLAY_WIDTH => {
                if let Some(t) = mkv.current.as_mut() {
                    t.display_width = read_uint(payload);
                }
            }
            DISPLAY_HEIGHT => {
                if let Some(t) = mkv.current.as_mut() {
                    t.display_height = read_uint(payload);
                }
            }
            TIMECODE_SCALE => mkv.timecode_scale = read_uint(payload),
            DURATION => mkv.duration = read_float(payload),
            _ => {}
        }
        at = payload_end;
    }
    Some(())
}

/// EBML 可变长整数：`keep_marker` 时把长度标记位也算进值（元素 ID 的表示法）。
fn read_vint(buf: &[u8], at: usize, end: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *buf.get(at).filter(|_| at < end)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || at + len > end {
        return None;
    }
    let mut value = if keep_marker {
        u64::from(first)
    } else {
        // 掩码必须算在 u64 上：8 字节 vint（`first == 0x01` ⇒ `len == 8`）的掩码是 0，
        // 值完全来自后续 7 字节。`0xFFu8 >> 8` 在 debug 下是移位溢出 panic（release 回绕成 0
        // 恰好"碰巧正确"，所以这个 bug 只在 debug 构建暴露）。
        let mask = 0xFFu64 >> len;
        u64::from(first) & mask
    };
    for i in 1..len {
        value = (value << 8) | u64::from(*buf.get(at + i)?);
    }
    // 长度未知：全 1 的 size 字段。
    if !keep_marker && len == 8 && value == 0x00FF_FFFF_FFFF_FFFF {
        return Some((u64::MAX, len));
    }
    Some((value, len))
}

/// 无符号整数（1..=8 字节，大端）。
fn read_uint(buf: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for b in buf.iter().take(8) {
        value = (value << 8) | u64::from(*b);
    }
    Some(value)
}

/// 浮点（4 或 8 字节，大端；其它长度视为未知）。
fn read_float(buf: &[u8]) -> Option<f64> {
    match buf.len() {
        4 => Some(f64::from(f32::from_be_bytes(buf.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(buf.try_into().ok()?)),
        _ => None,
    }
}

/// Matroska 的 `CodecID` → 友好的编码名。
fn matroska_codec(id: &str) -> String {
    let upper = id.to_ascii_uppercase();
    match upper.as_str() {
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "hevc",
        "V_AV1" => "av1",
        "V_VP9" => "vp9",
        "V_VP8" => "vp8",
        "V_MPEG4/ISO/ASP" => "mpeg4",
        "V_MPEG2" => "mpeg2video",
        "V_THEORA" => "theora",
        _ => return id.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 组一个 ISO-BMFF box。
    fn bx(name: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 8);
        out.extend_from_slice(&(u32::try_from(payload.len() + 8).unwrap()).to_be_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(payload);
        out
    }

    /// 拼一个 `moov`：mvhd(时长 12.345s) + trak(hdlr=vide, stsd=avc1 1920x1080,
    /// stts 合计 300 帧 × 1000 单位 @ 30000 单位/秒 ⇒ 30fps)。
    fn synthetic_moov() -> Vec<u8> {
        // mvhd v0: version/flags(4) creation(4) modification(4) timescale(4) duration(4)…
        let mut mvhd = vec![0u8; 96];
        mvhd[12..16].copy_from_slice(&1_000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&12_345u32.to_be_bytes());

        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");

        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&30_000u32.to_be_bytes());

        // stsd: version/flags(4) count(4) + avc1 entry（载荷 78 字节）。
        let mut avc1 = vec![0u8; 78];
        avc1[24..26].copy_from_slice(&1_920u16.to_be_bytes());
        avc1[26..28].copy_from_slice(&1_080u16.to_be_bytes());
        let mut stsd = vec![0u8; 8];
        stsd.extend_from_slice(&bx(b"avc1", &avc1));

        // stts: version/flags(4) count(4) + 1 项 (300 帧, 每帧 100 个时基单位)。
        let mut stts = vec![0u8; 8];
        stts[4..8].copy_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&300u32.to_be_bytes());
        stts.extend_from_slice(&1_000u32.to_be_bytes());

        let mut stbl = Vec::new();
        stbl.extend_from_slice(&bx(b"stsd", &stsd));
        stbl.extend_from_slice(&bx(b"stts", &stts));

        let mut minf = Vec::new();
        minf.extend_from_slice(&bx(b"stbl", &stbl));

        let mut mdia = Vec::new();
        mdia.extend_from_slice(&bx(b"mdhd", &mdhd));
        mdia.extend_from_slice(&bx(b"hdlr", &hdlr));
        mdia.extend_from_slice(&bx(b"minf", &minf));

        let mut trak = Vec::new();
        trak.extend_from_slice(&bx(b"tkhd", &[0u8; 84]));
        trak.extend_from_slice(&bx(b"mdia", &mdia));

        let mut moov = Vec::new();
        moov.extend_from_slice(&bx(b"mvhd", &mvhd));
        moov.extend_from_slice(&bx(b"trak", &trak));
        moov
    }

    #[test]
    fn png_header_gives_size_and_pixel_format() {
        // 用真实 PNG 编码器写一份，验证我们是按真实字节而不是"按文档幻想"解析。
        let dir = std::env::temp_dir().join(format!("liuguang-probe-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut enc = png::Encoder::new(file, 64, 32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&vec![0u8; 64 * 32 * 4]).unwrap();
        }
        let d = probe(&path, "image").unwrap();
        assert_eq!((d.width, d.height), (64, 32));
        assert_eq!(d.codec.as_deref(), Some("png"));
        assert_eq!(d.pix_fmt.as_deref(), Some("rgba8"));
        assert_eq!(d.fps_millihz, None);
        assert_eq!(d.duration_ms, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_sof_and_gif_and_bmp_headers_parse() {
        // JPEG：SOI + APP0(长度 4) + SOF0（高 120、宽 160、3 分量）。
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpeg.extend_from_slice(&120u16.to_be_bytes());
        jpeg.extend_from_slice(&160u16.to_be_bytes());
        jpeg.extend_from_slice(&[0x03]);
        let d = parse_image(&jpeg).unwrap();
        assert_eq!((d.width, d.height), (160, 120));
        assert_eq!(d.codec.as_deref(), Some("jpeg"));
        assert_eq!(d.pix_fmt, None, "三通道的上采样方式还没读到，不许猜");

        // GIF：逻辑屏幕 320x200。
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        gif.extend_from_slice(&[0, 0, 0]);
        let d = parse_image(&gif).unwrap();
        assert_eq!((d.width, d.height), (320, 200));
        assert_eq!(d.codec.as_deref(), Some("gif"));

        // BMP：BITMAPINFOHEADER，-240 表示自上而下。
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&[0, 0, 0, 0]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&400u32.to_le_bytes());
        bmp.extend_from_slice(&(-240i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        let d = parse_image(&bmp).unwrap();
        assert_eq!((d.width, d.height), (400, 240));
        assert_eq!(d.codec.as_deref(), Some("bmp"));
        assert_eq!(d.pix_fmt.as_deref(), Some("bgr8"));
    }

    #[test]
    fn webp_variants_parse() {
        // VP8X 扩展格式：画布 1200x900。
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&30u32.to_le_bytes());
        webp.extend_from_slice(b"WEBPVP8X");
        webp.extend_from_slice(&10u32.to_le_bytes());
        webp.extend_from_slice(&[0, 0, 0, 0]); // flags + reserved
        webp.extend_from_slice(&1199u32.to_le_bytes()[..3]);
        webp.extend_from_slice(&899u32.to_le_bytes()[..3]);
        let d = parse_image(&webp).unwrap();
        assert_eq!((d.width, d.height), (1200, 900));

        // VP8L 无损：宽 100、高 50 存在前 28 位里。
        let bits: u32 = (100 - 1) | ((50 - 1) << 14);
        let mut lossless = b"RIFF".to_vec();
        lossless.extend_from_slice(&20u32.to_le_bytes());
        lossless.extend_from_slice(b"WEBPVP8L");
        lossless.extend_from_slice(&5u32.to_le_bytes());
        lossless.push(0x2F);
        lossless.extend_from_slice(&bits.to_le_bytes());
        let d = parse_image(&lossless).unwrap();
        assert_eq!((d.width, d.height), (100, 50));

        // 有损 VP8：起始码对不上就必须拒绝，而不是给出垃圾尺寸。
        let mut broken = b"RIFF".to_vec();
        broken.extend_from_slice(&30u32.to_le_bytes());
        broken.extend_from_slice(b"WEBPVP8 ");
        broken.extend_from_slice(&10u32.to_le_bytes());
        broken.extend_from_slice(&[0, 0, 0, 1, 2, 3]);
        broken.extend_from_slice(&100u16.to_le_bytes());
        broken.extend_from_slice(&100u16.to_le_bytes());
        assert!(parse_image(&broken).is_none());
    }

    #[test]
    fn mp4_moov_gives_size_fps_duration_codec() {
        let moov = synthetic_moov();
        let d = parse_moov(&moov).unwrap();
        assert_eq!((d.width, d.height), (1_920, 1_080));
        assert_eq!(d.codec.as_deref(), Some("h264"));
        // 300 帧 / (300×1000 单位 ÷ 30000 单位每秒) = 30fps。
        assert_eq!(d.fps_millihz, Some(30_000));
        // 12345 / 1000 = 12.345s。
        assert_eq!(d.duration_ms, Some(12_345));
        assert_eq!(d.pix_fmt, None, "SPS 里的像素格式不在这里解");
    }

    #[test]
    fn mp4_probe_seeks_past_leading_boxes() {
        let dir = std::env::temp_dir().join(format!("liuguang-probe-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.mp4");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&bx(b"ftyp", b"isomisom"));
        bytes.extend_from_slice(&bx(b"moov", &synthetic_moov()));
        bytes.extend_from_slice(&bx(b"mdat", &[0u8; 16]));
        std::fs::write(&path, &bytes).unwrap();
        let d = probe(&path, "video").unwrap();
        assert_eq!((d.width, d.height), (1_920, 1_080));
        assert_eq!(d.codec.as_deref(), Some("h264"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 组一个 EBML 元素（ID 用 1/2/4 字节的标准表示，size 用手写 vint）。
    fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        // 用 4 字节 size vint（0x10 前缀），足够覆盖测试里的载荷。
        let size = payload.len() as u32;
        out.extend_from_slice(&(0x1000_0000 | size).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn uint_el(id: &[u8], value: u64) -> Vec<u8> {
        // EBML 无符号整数用**最短**大端表示：截断成固定 2 字节会把
        // 41708333（23.976fps 的 ns 值）压成 32557，测出来就成了假数据。
        let bytes = value.to_be_bytes();
        let first = bytes
            .iter()
            .position(|b| *b != 0)
            .unwrap_or(bytes.len() - 1);
        el(id, &bytes[first..])
    }

    #[test]
    fn mkv_tracks_and_info_parse() {
        let mut video = Vec::new();
        video.extend_from_slice(&uint_el(&[0xB0], 2_560));
        video.extend_from_slice(&uint_el(&[0xBA], 1_440));

        let mut entry = Vec::new();
        entry.extend_from_slice(&uint_el(&[0x83], 1)); // TrackType=video
        entry.extend_from_slice(&el(&[0x86], b"V_MPEG4/ISO/AVC"));
        entry.extend_from_slice(&uint_el(&[0x23, 0xE3, 0x83], 41_708_333)); // 23.976fps
        entry.extend_from_slice(&el(&[0xE0], &video));

        let mut tracks = Vec::new();
        tracks.extend_from_slice(&el(&[0xAE], &entry));

        let mut info = Vec::new();
        info.extend_from_slice(&uint_el(&[0x2A, 0xD7, 0xB1], 1_000_000));
        info.extend_from_slice(&el(&[0x44, 0x89], &60_000.0f64.to_be_bytes()));

        let mut segment = Vec::new();
        segment.extend_from_slice(&el(&[0x15, 0x49, 0xA9, 0x66], &info));
        segment.extend_from_slice(&el(&[0x16, 0x54, 0xAE, 0x6B], &tracks));

        let mut file = vec![0x1A, 0x45, 0xDF, 0xA3];
        file.extend_from_slice(&[0x80]); // 头部 size = 0（单字节 vint）
        file.extend_from_slice(&el(&[0x18, 0x53, 0x80, 0x67], &segment));

        let d = parse_mkv(&file).unwrap();
        assert_eq!((d.width, d.height), (2_560, 1_440));
        assert_eq!(d.codec.as_deref(), Some("h264"));
        assert_eq!(d.fps_millihz, Some(23_976));
        // Duration 60_000 个单位 × 1ms/单位 = 60s。
        assert_eq!(d.duration_ms, Some(60_000));
    }

    #[test]
    fn unrecognized_or_truncated_input_is_none_not_panic() {
        assert!(parse_image(b"not an image at all").is_none());
        assert!(parse_image(&[]).is_none());
        assert!(parse_moov(b"").is_none());
        assert!(parse_moov(&[0u8; 32]).is_none());
        assert!(parse_mkv(&[]).is_none());
        assert!(parse_mkv(b"\x1A\x45\xDF\xA3").is_none());
        // 声明 size 超出实际字节：不许 panic，也不许编出尺寸。
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3, 0x90, 0, 0, 0];
        mkv.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(parse_mkv(&mkv).is_none());
        // 不存在的文件：探测失败但扫描不受影响。
        assert!(probe(std::path::Path::new("/nonexistent/x.png"), "image").is_none());
    }

    /// 8 字节 vint 的掩码是 0（值全部来自后续 7 字节）。
    ///
    /// 回归用例：旧实现把掩码算在 `u8` 上（`0xFFu8 >> 8`），debug 构建直接
    /// 「attempt to shift right with overflow」panic——扫描 t15 的转码缓存（.mkv，
    /// EBML 常用 8 字节 size）就会打挂整个 daemon。release 只是碰巧回绕成 0 才没暴露。
    #[test]
    fn vint_handles_eight_byte_sizes() {
        let zero = [0x01, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(read_vint(&zero, 0, zero.len(), false), Some((0, 8)));

        // 值来自后续 7 字节（不是首字节里的标记位）。
        let mut small = zero;
        small[7] = 0x05;
        assert_eq!(read_vint(&small, 0, small.len(), false), Some((5, 8)));

        // 全 1 的 size 字段 = EBML 的「长度未知」。
        let unknown = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(
            read_vint(&unknown, 0, unknown.len(), false),
            Some((u64::MAX, 8))
        );

        // 元素 ID 分支（keep_marker）语义不变：标记位也算进值。
        assert_eq!(
            read_vint(&zero, 0, zero.len(), true),
            Some((0x0100_0000_0000_0000, 8))
        );
    }

    /// 1..7 字节 vint 的既有语义（掩码 = `0xFF >> len`）不得回归。
    #[test]
    fn vint_keeps_one_to_seven_byte_semantics() {
        // len == 1：标记位 0x80，掩码 0x7F。
        assert_eq!(read_vint(&[0x81], 0, 1, false), Some((1, 1)));
        // len == 2：标记位 0x40，掩码 0x3F。
        assert_eq!(read_vint(&[0x40, 0x01], 0, 2, false), Some((1, 2)));
        // len == 3。
        assert_eq!(
            read_vint(&[0x20, 0x01, 0x00], 0, 3, false),
            Some((0x0100, 3))
        );
        // len == 7：首字节 0x02（leading_zeros = 6），掩码 0x01。
        assert_eq!(
            read_vint(&[0x02, 0, 0, 0, 0, 0, 0x07], 0, 7, false),
            Some((7, 7))
        );
        // 元素 ID（keep_marker）4 字节形式原样保留。
        assert_eq!(
            read_vint(&[0x1A, 0x45, 0xDF, 0xA3], 0, 4, true),
            Some((0x1A45_DFA3, 4))
        );
        // 字节不够：None，不 panic。
        assert_eq!(read_vint(&[0x01, 0, 0], 0, 3, false), None);
        assert_eq!(read_vint(&[], 0, 0, false), None);
    }

    /// 8 字节 size 的 MKV 必须照常解析（未知长度吃到缓冲末尾；显式长度按值算）。
    #[test]
    fn mkv_with_eight_byte_segment_size_parses() {
        let mut video = Vec::new();
        video.extend_from_slice(&uint_el(&[0xB0], 1_920));
        video.extend_from_slice(&uint_el(&[0xBA], 1_080));
        let mut entry = Vec::new();
        entry.extend_from_slice(&uint_el(&[0x83], 1));
        entry.extend_from_slice(&el(&[0x86], b"V_MPEGH/ISO/HEVC"));
        entry.extend_from_slice(&el(&[0xE0], &video));
        let segment = el(&[0x16, 0x54, 0xAE, 0x6B], &el(&[0xAE], &entry));
        let header = [0x1A, 0x45, 0xDF, 0xA3, 0x80];

        // ① size 是 8 字节的「长度未知」（全 1）：吃到缓冲末尾。
        let mut file = header.to_vec();
        file.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        file.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        file.extend_from_slice(&segment);
        let details = parse_mkv(&file).expect("8 字节 size（未知长度）也要解析出尺寸");
        assert_eq!((details.width, details.height), (1_920, 1_080));
        assert_eq!(details.codec.as_deref(), Some("hevc"));
        assert_eq!(details.pix_fmt, None);

        // ② size 是 8 字节的显式长度：值来自后续 7 字节。
        let mut file = header.to_vec();
        file.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
        let mut size = [0x01u8, 0, 0, 0, 0, 0, 0, 0];
        size[1..].copy_from_slice(&(segment.len() as u64).to_be_bytes()[1..]);
        file.extend_from_slice(&size);
        file.extend_from_slice(&segment);
        let details = parse_mkv(&file).expect("8 字节 size（显式长度）也要解析出尺寸");
        assert_eq!(details.width, 1_920);
    }

    #[test]
    fn truncated_png_13_to_15_bytes_does_not_panic() {
        for len in 8..16 {
            let mut buf = vec![0u8; len];
            buf[..8].copy_from_slice(&PNG_SIG);
            assert!(parse_image(&buf).is_none(), "len={len}");
        }
    }

    #[test]
    fn crafted_u64_max_iso_bmff_box_does_not_overflow_or_loop() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "liuguang-probe-bmff-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.mp4");
        let mut data = Vec::new();
        // 8-byte ftyp box
        data.extend_from_slice(&8u32.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        // 16-byte box with small=1 and 64-bit size = u64::MAX - 7
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(b"free");
        data.extend_from_slice(&(u64::MAX - 7).to_be_bytes());
        std::fs::write(&path, &data).unwrap();
        assert!(probe_video(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
