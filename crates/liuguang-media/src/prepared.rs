//! 预加载与缓存：静态图片提前解码为 [`CpuFrame`]，避免展示时刻同步解码。
//!
//! [`PreloadCache`] 是**有界字节** LRU 缓存：以解码后缓冲字节数计费，
//! 超出上限时淘汰最久未使用的条目；单帧本身超过上限视为配置/素材错误，
//! 直接报 [`MediaError::DecodedTooLarge`]。
//!
//! 记账与淘汰使用共享的 [`liuguang_core::lru::ByteLru`]；帧值本身保存在
//! 本模块的映射表中（[`ByteLru`] 只记字节数）。
//!
//! [`PreloadCache::take`] 通过 `ByteLru::remove` 正式移除计费条目。

use crate::MediaError;
use crate::png_load;
use liuguang_core::lru::ByteLru;
use liuguang_core::types::CpuFrame;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 预加载一张图片（当前等价于 [`png_load::load_png`]）。
///
/// # Errors
///
/// 文件不可读、超限或解码失败时返回 [`MediaError`]。
pub fn prepare_image(path: &Path) -> Result<CpuFrame, MediaError> {
    png_load::load_png(path)
}

/// 预加载/解码一张图片，并按目标尺寸在**解码过程中**降采样。
///
/// 壁纸最终只上屏到输出分辨率；61 MP 的 PNG 全尺寸解码要 244 MB 的临时缓冲，
/// 用这个函数可以把它压到 `max_width × max_height × 4`（2560×1600 约 16 MB）。
///
/// # Errors
///
/// 与 [`prepare_image`] 相同。
pub fn prepare_image_fitted(
    path: &Path,
    max_width: u32,
    max_height: u32,
) -> Result<CpuFrame, MediaError> {
    png_load::load_png_fitted(path, max_width, max_height)
}

/// 图片预加载缓存：解码并缓存 [`CpuFrame`]，按字节上限 LRU 淘汰。
pub struct PreloadCache {
    /// 帧存储（键：规范化路径）。
    frames: HashMap<PathBuf, CpuFrame>,
    /// 字节记账 + LRU 淘汰（与 `frames` 同键同生命周期）。
    lru: ByteLru<PathBuf>,
}

impl PreloadCache {
    /// 创建缓存；`max_bytes` 为解码后缓冲总字节上限（允许为 0，即禁用缓存）。
    pub fn new(max_bytes: usize) -> Self {
        Self {
            frames: HashMap::new(),
            lru: ByteLru::new(max_bytes),
        }
    }

    /// 解码并缓存一张图片。
    ///
    /// - 已缓存同路径时先移除旧条目再插入（刷新为最近使用）。
    /// - 单帧字节数超过上限 → [`MediaError::DecodedTooLarge`]（不入缓存）。
    /// - 插入后总字节超限 → 淘汰最久未使用的条目。
    ///
    /// # Errors
    ///
    /// 解码失败或单帧超预算时返回 [`MediaError`]。
    pub fn preload(&mut self, path: &Path) -> Result<(), MediaError> {
        self.insert(path, prepare_image(path)?)
    }

    /// 与 [`Self::preload`] 相同，但在解码时按目标尺寸降采样（推荐：壁纸只需输出分辨率）。
    ///
    /// # Errors
    ///
    /// 解码失败或单帧超预算时返回 [`MediaError`]。
    pub fn preload_sized(
        &mut self,
        path: &Path,
        max_width: u32,
        max_height: u32,
    ) -> Result<(), MediaError> {
        self.insert(path, prepare_image_fitted(path, max_width, max_height)?)
    }

    fn insert(&mut self, path: &Path, frame: CpuFrame) -> Result<(), MediaError> {
        let bytes = frame.data.len();
        if bytes > self.lru.max_bytes() {
            return Err(MediaError::DecodedTooLarge {
                needed: bytes as u64,
                limit: self.lru.max_bytes() as u64,
            });
        }
        let evicted = self.lru.put(path.to_path_buf(), bytes);
        for key in evicted {
            self.frames.remove(&key);
        }
        self.frames.insert(path.to_path_buf(), frame);
        Ok(())
    }

    /// 取出并移除缓存帧（缓存命中消费）；未命中返回 `None`。
    pub fn take(&mut self, path: &Path) -> Option<CpuFrame> {
        let frame = self.frames.remove(path)?;
        let _ = self.lru.remove(&path.to_path_buf());
        Some(frame)
    }

    /// 是否已缓存该路径（不改变 LRU 顺序）。
    pub fn peek(&self, path: &Path) -> bool {
        self.frames.contains_key(path)
    }

    /// 当前缓存的解码缓冲总字节数。
    pub fn bytes(&self) -> usize {
        self.lru.bytes()
    }

    /// 字节预算上限（配置reload 时用它判断"预算是否变了"）。
    pub fn max_bytes(&self) -> usize {
        self.lru.max_bytes()
    }

    /// 当前缓存条目数（可观测用，不改变 LRU 顺序）。
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// 清空全部缓存帧与记账。
    ///
    /// 用途：**所有**输出都被拔掉时（计划 §11.2「HDMI/DP 拔除 ⇒ 关闭对应输出流
    /// 与缓存」）。留着已无画面可显示的解码帧只是白占 RSS。
    /// 部分拔除**不**清空：同一张图可能正被另一路输出用着。
    pub fn clear(&mut self) {
        self.frames.clear();
        self.lru.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::types::PixelFormat;
    use std::fs::File;
    use std::path::PathBuf;

    /// 写一张 `width x height` 的 RGBA PNG（纯色），返回路径。
    fn write_png(path: &Path, width: u32, height: u32, rgba: [u8; 4]) {
        let file = File::create(path).unwrap();
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&rgba);
        }
        writer.write_image_data(&data).unwrap();
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "liuguang-preload-test-{name}-{}",
            std::process::id()
        ));
        p
    }

    /// RGBA 4x4 PNG 解码后为 64 字节。
    const FRAME_BYTES: usize = 4 * 4 * 4;

    #[test]
    fn prepare_image_decodes_rgba() {
        let path = temp_path("prepare");
        write_png(&path, 4, 4, [1, 2, 3, 255]);
        let frame = prepare_image(&path).unwrap();
        assert_eq!(frame.width, 4);
        assert_eq!(frame.format, PixelFormat::Rgba8888);
        assert_eq!(&frame.data[0..4], &[1, 2, 3, 255]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preload_hit_then_take_removes() {
        let path = temp_path("hit");
        write_png(&path, 4, 4, [9, 9, 9, 255]);
        let mut cache = PreloadCache::new(1024);
        assert!(!cache.peek(&path));
        cache.preload(&path).unwrap();
        assert!(cache.peek(&path));
        assert_eq!(cache.bytes(), FRAME_BYTES);
        let frame = cache.take(&path).unwrap();
        assert_eq!(frame.height, 4);
        // take 后移除：再次 take 未命中，账目归零。
        assert!(!cache.peek(&path));
        assert!(cache.take(&path).is_none());
        assert_eq!(cache.bytes(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preload_replaces_existing_entry() {
        let path = temp_path("replace");
        write_png(&path, 4, 4, [5, 5, 5, 255]);
        let mut cache = PreloadCache::new(1024);
        cache.preload(&path).unwrap();
        cache.preload(&path).unwrap();
        assert_eq!(cache.bytes(), FRAME_BYTES, "重复 preload 不得叠加计费");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lru_evicts_oldest_when_over_budget() {
        // 上限只够两帧：依次 A、B、C → A 被淘汰。
        let a = temp_path("lru-a");
        let b = temp_path("lru-b");
        let c = temp_path("lru-c");
        write_png(&a, 4, 4, [1, 0, 0, 255]);
        write_png(&b, 4, 4, [2, 0, 0, 255]);
        write_png(&c, 4, 4, [3, 0, 0, 255]);
        let mut cache = PreloadCache::new(FRAME_BYTES * 2);
        cache.preload(&a).unwrap();
        cache.preload(&b).unwrap();
        cache.preload(&c).unwrap();
        assert!(!cache.peek(&a), "最久未使用应被淘汰");
        assert!(cache.peek(&b));
        assert!(cache.peek(&c));
        assert_eq!(cache.bytes(), FRAME_BYTES * 2);
        for p in [&a, &b, &c] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn lru_touch_on_preload_spares_recent() {
        // 上限两帧：A、B 后再次 preload A，再放 C → B 被淘汰（A 更新为最近）。
        let a = temp_path("touch-a");
        let b = temp_path("touch-b");
        let c = temp_path("touch-c");
        write_png(&a, 4, 4, [1, 0, 0, 255]);
        write_png(&b, 4, 4, [2, 0, 0, 255]);
        write_png(&c, 4, 4, [3, 0, 0, 255]);
        let mut cache = PreloadCache::new(FRAME_BYTES * 2);
        cache.preload(&a).unwrap();
        cache.preload(&b).unwrap();
        cache.preload(&a).unwrap();
        cache.preload(&c).unwrap();
        assert!(cache.peek(&a), "刷新过的 A 不应被淘汰");
        assert!(!cache.peek(&b));
        assert!(cache.peek(&c));
        for p in [&a, &b, &c] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn single_frame_over_budget_is_rejected() {
        let path = temp_path("oversize");
        write_png(&path, 4, 4, [7, 7, 7, 255]);
        let mut cache = PreloadCache::new(FRAME_BYTES - 1);
        match cache.preload(&path) {
            Err(MediaError::DecodedTooLarge { needed, limit }) => {
                assert_eq!(needed, FRAME_BYTES as u64);
                assert_eq!(limit, (FRAME_BYTES - 1) as u64);
            }
            other => panic!("期望 DecodedTooLarge，实际 {other:?}"),
        }
        assert!(!cache.peek(&path));
        assert_eq!(cache.bytes(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zero_budget_cache_rejects_everything() {
        let path = temp_path("zero");
        write_png(&path, 4, 4, [7, 7, 7, 255]);
        let mut cache = PreloadCache::new(0);
        assert!(matches!(
            cache.preload(&path),
            Err(MediaError::DecodedTooLarge { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_reports_io() {
        let mut cache = PreloadCache::new(1024);
        let err = cache
            .preload(Path::new("/nonexistent/none.png"))
            .unwrap_err();
        assert!(matches!(err, MediaError::Io { .. }));
    }

    #[test]
    fn clear_drops_frames_and_accounting() {
        // 计划 §11.2「拔除 ⇒ 关闭输出流与缓存」用的那一环：清空必须**连记账一起**清，
        // 否则 ByteLru 里留着幽灵字节，之后每次 preload 都会立刻淘汰新帧（缓存空转）。
        let a = temp_path("clear-a");
        let b = temp_path("clear-b");
        write_png(&a, 4, 4, [1, 0, 0, 255]);
        write_png(&b, 4, 4, [2, 0, 0, 255]);
        let mut cache = PreloadCache::new(FRAME_BYTES * 2);
        cache.preload(&a).unwrap();
        cache.preload(&b).unwrap();
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0, "清空必须同时归零字节记账");

        let mut tight = PreloadCache::new(FRAME_BYTES);
        tight.preload(&a).unwrap();
        tight.clear();
        tight.preload(&a).unwrap();
        tight.preload(&b).unwrap();
        assert_eq!(tight.bytes(), FRAME_BYTES, "clear 后不得留下幽灵计费");
        assert!(!tight.peek(&a), "预算只够一帧时，应是 a 被 b 挤出");
        assert!(tight.peek(&b));
        for p in [&a, &b] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn taken_slot_can_be_filled_again() {
        // take 归账后，预算应能容纳新帧（不得泄漏计费）。
        let a = temp_path("refill-a");
        let b = temp_path("refill-b");
        write_png(&a, 4, 4, [1, 0, 0, 255]);
        write_png(&b, 4, 4, [2, 0, 0, 255]);
        let mut cache = PreloadCache::new(FRAME_BYTES);
        cache.preload(&a).unwrap();
        assert!(cache.take(&a).is_some());
        assert_eq!(cache.bytes(), 0);
        cache.preload(&b).unwrap();
        assert!(cache.peek(&b));
        assert_eq!(cache.bytes(), FRAME_BYTES);
        for p in [&a, &b] {
            let _ = std::fs::remove_file(p);
        }
    }
}
