//! 播放列表（M3）：媒体目录扫描与轮换顺序。
//!
//! 两种轮换顺序：
//! - [`OrderMode::Sequential`]：按条目顺序循环；
//! - [`OrderMode::Shuffle`]：**shuffle bag**（洗牌袋）——每个条目在当前袋中
//!   恰好出现一次且不重复，袋耗尽后重新洗牌；`avoid_repeat` 为真时保证
//!   新袋首项不等于上一袋末项（必要时交换）。
//!
//! 随机性不引入第三方依赖：使用确定性 xorshift64 PRNG，种子由调用方显式
//! 提供，同一 seed 产生完全一致的播放序列（可复现、可测试）。

use crate::types::MediaKind;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// 轮换顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderMode {
    /// 顺序循环播放。
    Sequential,
    /// shuffle bag 随机；`seed` 决定确定性序列，`avoid_repeat` 避免跨袋立即重复。
    Shuffle {
        /// 随机种子（显式传入，保证序列可复现）。
        seed: u64,
        /// 是否避免新袋首项等于上一袋末项。
        avoid_repeat: bool,
    },
}

/// 播放列表中的一个媒体条目。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaEntry {
    /// 媒体文件路径。
    pub path: PathBuf,
    /// 媒体种类（图片/视频）。
    pub kind: MediaKind,
}

impl MediaEntry {
    /// 创建媒体条目。
    pub fn new(path: impl Into<PathBuf>, kind: MediaKind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }
}

/// 播放列表错误。
#[derive(Debug, thiserror::Error)]
pub enum PlaylistError {
    /// 无可用媒体项。
    #[error("播放列表为空：没有可用的媒体项")]
    Empty,
    /// 目录读取失败。
    #[error("目录读取失败: {path}: {source}")]
    Io {
        /// 出问题的目录或文件路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: std::io::Error,
    },
    /// 播放顺序参数非法（保留给后续顺序解析/校验扩展）。
    #[error("播放顺序非法: {0}")]
    InvalidOrder(String),
}

/// 确定性 PRNG（xorshift64）。种子为 0 时是不动点，映射到固定非零常量。
#[derive(Debug, Clone, Copy)]
struct Xorshift64(u64);

impl Xorshift64 {
    /// 用显式种子构造。
    fn new(seed: u64) -> Self {
        // xorshift64 的 0 是不动点，映射到黄金比常量保证可迭代。
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// 下一个伪随机 `u64`。
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// 返回 `[0, n)` 的伪随机下标（`n` 必须 > 0）。
    fn below(&mut self, n: usize) -> usize {
        // n <= usize::MAX，u64 取模足够；n == 1 时恒为 0。
        let n = n as u64;
        (self.next_u64() % n.max(1)) as usize
    }
}

/// 播放列表：持有条目与轮换顺序，提供循环/洗牌袋取项。
///
/// 可为空（[`Self::empty`]，对应目录为空/不可读时的兜底通道）；空列表的
/// [`Self::next`] 恒返回 `None`。[`Self::with_entries`] 仍拒绝空 `Vec`
/// （[`PlaylistError::Empty`]）。
#[derive(Debug, Clone)]
pub struct Playlist {
    /// 条目（保持构造时的稳定顺序，供 [`Self::entries`] 读取）。
    entries: Vec<MediaEntry>,
    /// 轮换顺序。
    order: OrderMode,
    /// 下一个要播放的下标（顺序模式下为 `entries` 下标，洗牌模式下为袋内位置）。
    cursor: usize,
    /// shuffle bag（`entries` 下标的排列）。
    bag: Vec<usize>,
    /// 确定性 PRNG。
    rng: Xorshift64,
    /// 上一次播放的 `entries` 下标（`avoid_repeat` 使用）。
    last: Option<usize>,
}

impl Playlist {
    /// 扫描目录（不递归）构造播放列表。
    ///
    /// - `*.png` → [`MediaKind::Image`]，`*.mp4` → [`MediaKind::Video`]，
    ///   其他文件与子目录忽略（扩展名大小写不敏感）；
    /// - 条目按文件名排序，保证顺序稳定、可复现；
    /// - 目录不可读返回 [`PlaylistError::Io`]；无可用项返回 [`PlaylistError::Empty`]。
    ///
    /// # Errors
    ///
    /// 目录读取失败返回 [`PlaylistError::Io`]；没有可用媒体项返回
    /// [`PlaylistError::Empty`]。
    pub fn from_dir(dir: &Path, order: OrderMode) -> Result<Self, PlaylistError> {
        let read = std::fs::read_dir(dir).map_err(|source| PlaylistError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let mut scanned: Vec<(OsString, MediaEntry)> = Vec::new();
        for entry in read {
            let entry = entry.map_err(|source| PlaylistError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| PlaylistError::Io {
                path: path.clone(),
                source,
            })?;
            if !file_type.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let kind = match ext.to_ascii_lowercase().as_str() {
                "png" => MediaKind::Image,
                "mp4" => MediaKind::Video,
                _ => continue,
            };
            scanned.push((entry.file_name(), MediaEntry { path, kind }));
        }
        // 文件名排序，保证扫描结果顺序稳定（确定性）。
        scanned.sort_by(|a, b| a.0.cmp(&b.0));
        let entries = scanned.into_iter().map(|(_, e)| e).collect();
        Self::with_entries(entries, order)
    }

    /// 构造空列表（无媒体项；[`Self::next`] 恒返回 `None`）。
    ///
    /// 用于目录为空/不可读时仍建立轮换通道的兜底场景（daemon 的占位播放）。
    pub fn empty(order: OrderMode) -> Self {
        Self {
            entries: Vec::new(),
            order,
            cursor: 0,
            bag: Vec::new(),
            rng: Xorshift64::new(match order {
                OrderMode::Sequential => 0,
                OrderMode::Shuffle { seed, .. } => seed,
            }),
            last: None,
        }
    }

    /// 用给定条目构造播放列表（条目顺序即顺序模式的播放顺序）。
    ///
    /// # Errors
    ///
    /// `entries` 为空返回 [`PlaylistError::Empty`]（空列表请用 [`Self::empty`]）。
    pub fn with_entries(entries: Vec<MediaEntry>, order: OrderMode) -> Result<Self, PlaylistError> {
        if entries.is_empty() {
            return Err(PlaylistError::Empty);
        }
        let mut playlist = Self::empty(order);
        playlist.entries = entries;
        if matches!(order, OrderMode::Shuffle { .. }) {
            playlist.refill_bag();
        }
        Ok(playlist)
    }

    /// 取出下一个条目（克隆）；空列表返回 `None`。
    ///
    /// - [`OrderMode::Sequential`]：循环顺序；
    /// - [`OrderMode::Shuffle`]：袋内每项恰好出现一次且不重复，袋耗尽后
    ///   重新洗牌；`avoid_repeat` 为真时新袋首项不等于上一袋末项。
    ///
    /// 同一 `seed` 下的取项序列完全确定。
    // 方法名 `next` 由 API 契约指定（非 Iterator 实现），豁免同名标准 trait 误配告警。
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<MediaEntry> {
        if self.entries.is_empty() {
            return None;
        }
        match self.order {
            OrderMode::Sequential => {
                let idx = self.cursor;
                self.cursor = (idx + 1) % self.entries.len();
                self.last = Some(idx);
                self.entries.get(idx).cloned()
            }
            OrderMode::Shuffle { .. } => {
                if self.cursor >= self.bag.len() {
                    self.refill_bag();
                }
                let idx = *self.bag.get(self.cursor)?;
                self.cursor += 1;
                self.last = Some(idx);
                // 袋内最后一项取走后立即重洗，保证 `peek`（&self）也能看到下一项。
                if self.cursor >= self.bag.len() {
                    self.refill_bag();
                }
                self.entries.get(idx).cloned()
            }
        }
    }

    /// 查看下一个条目（不消费）。
    pub fn peek(&self) -> Option<&MediaEntry> {
        match self.order {
            OrderMode::Sequential => self.entries.get(self.cursor),
            OrderMode::Shuffle { .. } => {
                let idx = self.bag.get(self.cursor).copied()?;
                self.entries.get(idx)
            }
        }
    }

    /// 条目数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空（[`Self::empty`] 构造的列表恒为 `true`）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 全部条目（构造时的稳定顺序）。
    pub fn entries(&self) -> &[MediaEntry] {
        &self.entries
    }

    /// 当前轮换顺序。
    pub fn order(&self) -> OrderMode {
        self.order
    }

    /// 把游标对齐到指定路径的条目，使下一次 [`Self::next`] 返回它。
    ///
    /// - 顺序模式：游标直接指向该条目（同一路径出现多次时取第一个）；
    /// - 随机模式：若该条目尚未在当前袋中播放，交换到袋内游标位置（保持袋
    ///   内每项恰好一次）；若已播放过，则开启新袋并把该条目置于袋首——
    ///   用户显式指定优先于 `avoid_repeat`，允许与上一播放项相邻重复；
    /// - 空列表或路径不存在返回 `false`，不改变任何状态。
    ///
    /// daemon 的 `set` 命令用它对齐轮换起点。
    #[must_use]
    pub fn advance_to(&mut self, path: &Path) -> bool {
        let Some(idx) = self.entries.iter().position(|e| e.path == path) else {
            return false;
        };
        match self.order {
            OrderMode::Sequential => {
                self.cursor = idx;
            }
            OrderMode::Shuffle { .. } => {
                if self.bag.is_empty() {
                    self.refill_bag();
                }
                match self.bag[self.cursor..].iter().position(|&i| i == idx) {
                    Some(offset) => {
                        // 尚未播放：交换到游标处，袋仍是完整排列。
                        self.bag.swap(self.cursor, self.cursor + offset);
                    }
                    None => {
                        // 已播放过：开新袋并强制袋首。
                        self.refill_bag();
                        if let Some(pos) = self.bag.iter().position(|&i| i == idx) {
                            self.bag.swap(0, pos);
                        }
                        self.cursor = 0;
                    }
                }
            }
        }
        true
    }

    /// 重新洗一袋（Fisher–Yates），并按需做 `avoid_repeat` 交换。
    fn refill_bag(&mut self) {
        let n = self.entries.len();
        self.bag = (0..n).collect();
        for i in (1..n).rev() {
            let j = self.rng.below(i + 1);
            self.bag.swap(i, j);
        }
        if matches!(
            self.order,
            OrderMode::Shuffle {
                avoid_repeat: true,
                ..
            }
        ) && n > 1
        {
            if let Some(prev) = self.last {
                // 袋是排列，prev 只出现一次；若在袋首则与第二项交换即可避开。
                if self.bag[0] == prev {
                    self.bag.swap(0, 1);
                }
            }
        }
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn entry(name: &str) -> MediaEntry {
        MediaEntry::new(PathBuf::from(format!("/media/{name}")), MediaKind::Image)
    }

    fn entries(names: &[&str]) -> Vec<MediaEntry> {
        names.iter().map(|n| entry(n)).collect()
    }

    /// 简单临时目录工具：在系统临时目录下创建唯一子目录。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "liuguang-playlist-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn from_dir_scans_extensions_sorted_and_non_recursive() {
        let dir = TempDir::new("scan");
        std::fs::write(dir.path.join("b.mp4"), b"v").unwrap();
        std::fs::write(dir.path.join("a.png"), b"i").unwrap();
        std::fs::write(dir.path.join("c.txt"), b"x").unwrap();
        std::fs::write(dir.path.join("z.PNG"), b"i").unwrap(); // 扩展名大小写不敏感
        std::fs::create_dir(dir.path.join("d.png")).unwrap(); // 子目录忽略

        let p = Playlist::from_dir(&dir.path, OrderMode::Sequential).unwrap();
        let names: Vec<_> = p
            .entries()
            .iter()
            .map(|e| e.path.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["a.png", "b.mp4", "z.PNG"]);
        assert_eq!(p.entries()[0].kind, MediaKind::Image);
        assert_eq!(p.entries()[1].kind, MediaKind::Video);
        assert_eq!(p.entries()[2].kind, MediaKind::Image);
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn from_dir_missing_dir_is_io_error() {
        let err = Playlist::from_dir(
            Path::new("/nonexistent/liuguang-playlist"),
            OrderMode::Sequential,
        )
        .unwrap_err();
        assert!(matches!(err, PlaylistError::Io { .. }), "实际: {err:?}");
    }

    #[test]
    fn from_dir_without_media_is_empty_error() {
        let dir = TempDir::new("empty");
        std::fs::write(dir.path.join("note.txt"), b"x").unwrap();
        let err = Playlist::from_dir(&dir.path, OrderMode::Sequential).unwrap_err();
        assert!(matches!(err, PlaylistError::Empty), "实际: {err:?}");
    }

    #[test]
    fn with_entries_rejects_empty() {
        let err = Playlist::with_entries(Vec::new(), OrderMode::Sequential).unwrap_err();
        assert!(matches!(err, PlaylistError::Empty), "实际: {err:?}");
    }

    #[test]
    fn sequential_cycles_and_peek_matches_next() {
        let mut p =
            Playlist::with_entries(entries(&["a", "b", "c"]), OrderMode::Sequential).unwrap();
        for expect in ["a", "b", "c", "a", "b", "c", "a"] {
            let peeked = p.peek().unwrap().path.clone();
            assert!(peeked.to_string_lossy().ends_with(expect));
            let got = p.next().unwrap();
            assert_eq!(got.path, peeked);
        }
    }

    #[test]
    fn shuffle_bag_is_deterministic_for_same_seed() {
        let order = OrderMode::Shuffle {
            seed: 42,
            avoid_repeat: true,
        };
        let mut a = Playlist::with_entries(entries(&["a", "b", "c", "d", "e"]), order).unwrap();
        let mut b = Playlist::with_entries(entries(&["a", "b", "c", "d", "e"]), order).unwrap();
        for i in 0..20 {
            assert_eq!(a.next(), b.next(), "第 {i} 次取项不一致");
        }
    }

    #[test]
    fn shuffle_bag_has_no_repeats_within_bag() {
        let mut p = Playlist::with_entries(
            entries(&["a", "b", "c", "d", "e"]),
            OrderMode::Shuffle {
                seed: 7,
                avoid_repeat: true,
            },
        )
        .unwrap();
        for bag_index in 0..5 {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..5 {
                let item = p.next().unwrap();
                assert!(
                    seen.insert(item.path.clone()),
                    "袋 {bag_index} 内出现重复: {:?}",
                    item.path
                );
            }
        }
    }

    #[test]
    fn shuffle_avoid_repeat_prevents_bag_boundary_repeat() {
        // 重放 50 个袋，逐袋断言：新袋首项不等于上一袋末项。
        let mut p = Playlist::with_entries(
            entries(&["a", "b", "c", "d"]),
            OrderMode::Shuffle {
                seed: 999,
                avoid_repeat: true,
            },
        )
        .unwrap();
        let mut last_of_prev_bag: Option<std::path::PathBuf> = None;
        for _bag in 0..50 {
            let mut played = Vec::new();
            for _ in 0..4 {
                played.push(p.next().unwrap().path);
            }
            if let Some(prev_end) = &last_of_prev_bag {
                assert_ne!(prev_end, &played[0], "袋边界出现立即重复: {:?}", played[0]);
            }
            last_of_prev_bag = played.pop();
        }
    }

    #[test]
    fn shuffle_single_entry_repeats_safely() {
        let mut p = Playlist::with_entries(
            entries(&["only"]),
            OrderMode::Shuffle {
                seed: 5,
                avoid_repeat: true,
            },
        )
        .unwrap();
        for _ in 0..3 {
            assert_eq!(p.next().unwrap().path, PathBuf::from("/media/only"));
        }
    }

    #[test]
    fn peek_does_not_consume_shuffle() {
        let mut p = Playlist::with_entries(
            entries(&["a", "b", "c"]),
            OrderMode::Shuffle {
                seed: 11,
                avoid_repeat: false,
            },
        )
        .unwrap();
        let peeked = p.peek().unwrap().clone();
        assert_eq!(p.peek(), Some(&peeked));
        assert_eq!(p.next(), Some(peeked));
    }

    #[test]
    fn order_accessor_matches() {
        let p = Playlist::with_entries(entries(&["a"]), OrderMode::Sequential).unwrap();
        assert_eq!(p.order(), OrderMode::Sequential);
        let order = OrderMode::Shuffle {
            seed: 1,
            avoid_repeat: false,
        };
        let p = Playlist::with_entries(entries(&["a"]), order).unwrap();
        assert_eq!(p.order(), order);
    }

    #[test]
    fn empty_playlist_yields_nothing() {
        let mut p = Playlist::empty(OrderMode::Sequential);
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert!(p.entries().is_empty());
        assert_eq!(p.peek(), None);
        assert_eq!(p.next(), None);
        assert!(!p.advance_to(Path::new("/media/a")));

        let mut p = Playlist::empty(OrderMode::Shuffle {
            seed: 3,
            avoid_repeat: true,
        });
        assert_eq!(p.next(), None);
        assert_eq!(p.peek(), None);
    }

    #[test]
    fn advance_to_aligns_sequential_cursor() {
        let mut p =
            Playlist::with_entries(entries(&["a", "b", "c"]), OrderMode::Sequential).unwrap();
        assert_eq!(p.next().unwrap().path, PathBuf::from("/media/a"));
        assert!(p.advance_to(Path::new("/media/c")));
        assert_eq!(p.next().unwrap().path, PathBuf::from("/media/c"));
        assert_eq!(p.next().unwrap().path, PathBuf::from("/media/a"));
        // 不存在的路径：返回 false 且不改变游标。
        assert!(!p.advance_to(Path::new("/media/zzz")));
        assert_eq!(p.next().unwrap().path, PathBuf::from("/media/b"));
    }

    #[test]
    fn advance_to_aligns_shuffle_cursor() {
        let order = OrderMode::Shuffle {
            seed: 77,
            avoid_repeat: true,
        };
        let mut p = Playlist::with_entries(entries(&["a", "b", "c", "d"]), order).unwrap();
        for target in ["a", "b", "c", "d", "a"] {
            assert!(p.advance_to(Path::new(&format!("/media/{target}"))));
            assert_eq!(
                p.next().unwrap().path,
                PathBuf::from(format!("/media/{target}")),
                "对齐 {target} 失败"
            );
        }
    }

    #[test]
    fn advance_to_miss_does_not_change_state() {
        let order = OrderMode::Shuffle {
            seed: 77,
            avoid_repeat: true,
        };
        let mut a = Playlist::with_entries(entries(&["a", "b", "c", "d"]), order).unwrap();
        let mut b = Playlist::with_entries(entries(&["a", "b", "c", "d"]), order).unwrap();
        assert!(!a.advance_to(Path::new("/media/zzz")));
        assert_eq!(a.next(), b.next());
    }

    #[test]
    fn advance_to_shuffle_before_play_keeps_permutation() {
        // 未播放项对齐到袋首（交换分支）：整袋仍恰好各出现一次。
        let order = OrderMode::Shuffle {
            seed: 2024,
            avoid_repeat: true,
        };
        let mut p = Playlist::with_entries(entries(&["a", "b", "c", "d", "e"]), order).unwrap();
        assert!(p.advance_to(Path::new("/media/c")));
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..5 {
            assert!(seen.insert(p.next().unwrap().path));
        }
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn advance_to_shuffle_replayed_item_forces_new_bag() {
        // 对齐已播放项：开新袋并置袋首，随后一袋仍恰好各出现一次。
        let order = OrderMode::Shuffle {
            seed: 2024,
            avoid_repeat: true,
        };
        let mut p = Playlist::with_entries(entries(&["a", "b", "c", "d", "e"]), order).unwrap();
        let first = p.next().unwrap().path;
        assert!(p.advance_to(&first));
        let mut seen = std::collections::BTreeSet::new();
        assert_eq!(p.next().unwrap().path, first, "新袋首应对齐到目标");
        assert!(seen.insert(first));
        for _ in 0..4 {
            assert!(seen.insert(p.next().unwrap().path), "新袋内重复");
        }
        assert_eq!(seen.len(), 5);
    }
}
