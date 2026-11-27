//! 字节软预算 LRU 缓存（规范 §11.2）。
//!
//! 面向"应用缓存软预算"场景（如解码帧/图片缓存）：按**字节数**而非条目数
//! 控制总量，超预算时按最久未用（LRU）淘汰。被淘汰的键由调用方释放真实
//! 资源（纹理、解码缓冲等）。
//!
//! # 接线状态（重要，避免误判为死代码）
//!
//! 本类型目前**唯一**的消费者是 `liuguang-media::prepared::PreloadCache`
//! （由它记录解码后缓冲的字节数）。`PreloadCache` 已于 **2026-10-20 接入
//! `apps/liuguang`**：`performance.preload_next` 决定要不要预解码下一张图片，
//! `performance.max_cache_mb` 就是这里的字节预算（超出即 LRU 淘汰）。
//! 因此这两个配置项已改变运行期行为，不在
//! [`crate::config::Config::pending_implementation`] 的缺口清单里。
//!
//! 语义要点：
//! - [`ByteLru::put`] 插入或覆盖；覆盖旧值视为一次淘汰，其键出现在返回
//!   列表首部；随后按 LRU 顺序淘汰直至不超预算；
//! - 单项字节数大于预算时不缓存、不淘汰任何已有项，返回空列表；
//! - [`ByteLru::get`] 命中刷新新鲜度并返回字节数。

use std::collections::HashMap;
use std::hash::Hash;

/// 缓存条目：字节数 + 新鲜度序号（越大越新）。
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// 占用字节数。
    bytes: usize,
    /// 新鲜度序号。
    seq: u64,
}

/// 按字节软预算的 LRU 缓存。
#[derive(Debug, Clone)]
pub struct ByteLru<K: Eq + Hash + Clone> {
    /// 键到条目的映射。
    map: HashMap<K, Entry>,
    /// 当前总字节数。
    total: usize,
    /// 字节软预算上限。
    max_bytes: usize,
    /// 新鲜度计数器。
    clock: u64,
}

impl<K: Eq + Hash + Clone> ByteLru<K> {
    /// 创建指定字节软预算的空缓存（`max_bytes` 允许为 0）。
    pub fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            total: 0,
            max_bytes,
            clock: 0,
        }
    }

    /// 插入或覆盖键值，返回被淘汰的键列表（最久未用在前）。
    ///
    /// - 同键覆盖：旧值视为一次淘汰，其键出现在返回列表首部；
    /// - 超预算：按 LRU 淘汰直至总字节不超预算（新项最新鲜，不会淘汰自己）；
    /// - 单项 `bytes` > 预算：**不缓存**且不淘汰任何已有项（含同键旧值），
    ///   返回空列表。
    pub fn put(&mut self, key: K, bytes: usize) -> Vec<K> {
        let mut evicted = Vec::new();
        if bytes > self.max_bytes {
            return evicted;
        }
        if let Some(old) = self.map.remove(&key) {
            self.total -= old.bytes;
            evicted.push(key.clone());
        }
        self.clock += 1;
        self.map.insert(
            key,
            Entry {
                bytes,
                seq: self.clock,
            },
        );
        self.total += bytes;
        while self.total > self.max_bytes {
            let Some(victim_key) = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((k, e)) = self.map.remove_entry(&victim_key) {
                self.total -= e.bytes;
                evicted.push(k);
            }
        }
        evicted
    }

    /// 命中返回字节数并刷新新鲜度；未命中返回 `None`。
    pub fn get(&mut self, key: &K) -> Option<usize> {
        let entry = self.map.get_mut(key)?;
        self.clock += 1;
        entry.seq = self.clock;
        Some(entry.bytes)
    }

    /// 是否包含键（不刷新新鲜度）。
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// 当前缓存占用的总字节数。
    pub fn bytes(&self) -> usize {
        self.total
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 字节软预算上限。
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// 移除指定键并返回其字节数（不存在返回 `None`）。
    ///
    /// 调用方拿到 `Some(bytes)` 即表示**该项的计费已释放**，
    /// 应同时释放对应的真实资源（纹理/解码缓冲）。
    pub fn remove(&mut self, key: &K) -> Option<usize> {
        let entry = self.map.remove(key)?;
        self.total = self.total.saturating_sub(entry.bytes);
        Some(entry.bytes)
    }

    /// 清空缓存（重置字节计数，保留预算上限）。
    pub fn clear(&mut self) {
        self.map.clear();
        self.total = 0;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn put_get_tracks_bytes_and_len() {
        let mut lru: ByteLru<&'static str> = ByteLru::new(100);
        assert!(lru.is_empty());
        assert_eq!(lru.put("a", 30), Vec::<&'static str>::new());
        assert_eq!(lru.put("b", 40), Vec::<&'static str>::new());
        assert_eq!(lru.bytes(), 70);
        assert_eq!(lru.len(), 2);
        assert!(lru.contains(&"a"));
        assert_eq!(lru.get(&"a"), Some(30));
        assert_eq!(lru.get(&"missing"), None);
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let mut lru: ByteLru<char> = ByteLru::new(80);
        lru.put('a', 30);
        lru.put('b', 30);
        lru.put('c', 30); // 总 90 > 80：淘汰最旧的 a
        assert_eq!(lru.get(&'a'), None);
        assert_eq!(lru.bytes(), 60);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut lru: ByteLru<char> = ByteLru::new(80);
        lru.put('a', 30);
        lru.put('b', 30);
        assert_eq!(lru.get(&'a'), Some(30)); // a 变最新
        let evicted = lru.put('c', 30); // 应淘汰 b 而不是 a
        assert_eq!(evicted, vec!['b']);
        assert_eq!(lru.get(&'a'), Some(30));
        assert_eq!(lru.get(&'b'), None);
    }

    #[test]
    fn eviction_order_is_oldest_first() {
        let mut lru: ByteLru<char> = ByteLru::new(50);
        lru.put('a', 20);
        lru.put('b', 20);
        lru.put('c', 20); // 60 > 50：淘汰最旧的 a
        assert_eq!(lru.bytes(), 40);
        let evicted = lru.put('d', 10); // 正好 50，无淘汰
        assert_eq!(evicted, Vec::<char>::new());
        let evicted = lru.put('e', 30); // 80 > 50：按旧→新淘汰 b、c
        assert_eq!(evicted, vec!['b', 'c']);
        assert_eq!(lru.bytes(), 40); // d(10) + e(30)
    }

    #[test]
    fn overwrite_reports_old_key_and_resizes() {
        let mut lru: ByteLru<&'static str> = ByteLru::new(100);
        lru.put("k", 10);
        let evicted = lru.put("k", 20);
        assert_eq!(evicted, vec!["k"]); // 覆盖旧值视为淘汰
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.bytes(), 20);
        assert_eq!(lru.get(&"k"), Some(20));
    }

    #[test]
    fn oversized_item_is_rejected_without_eviction() {
        let mut lru: ByteLru<&'static str> = ByteLru::new(50);
        lru.put("a", 10);
        lru.put("b", 10);
        assert_eq!(lru.put("big", 60), Vec::<&'static str>::new());
        assert_eq!(lru.bytes(), 20);
        assert_eq!(lru.len(), 2);
        assert!(!lru.contains(&"big"));
        // 同键旧值也不受影响。
        assert_eq!(lru.put("a", 60), Vec::<&'static str>::new());
        assert_eq!(lru.get(&"a"), Some(10));
    }

    #[test]
    fn zero_budget_keeps_only_zero_byte_entries() {
        let mut lru: ByteLru<&'static str> = ByteLru::new(0);
        assert_eq!(lru.put("free", 0), Vec::<&'static str>::new());
        assert_eq!(lru.get(&"free"), Some(0));
        assert_eq!(lru.put("costly", 1), Vec::<&'static str>::new());
        assert!(!lru.contains(&"costly"));
    }

    #[test]
    fn clear_resets_everything() {
        let mut lru: ByteLru<&'static str> = ByteLru::new(100);
        lru.put("a", 40);
        lru.put("b", 40);
        lru.clear();
        assert!(lru.is_empty());
        assert_eq!(lru.bytes(), 0);
        assert_eq!(lru.get(&"a"), None);
        assert_eq!(lru.max_bytes(), 100);
    }

    #[test]
    fn exact_budget_fit_is_not_evicted() {
        let mut lru: ByteLru<char> = ByteLru::new(60);
        assert_eq!(lru.put('a', 30), Vec::<char>::new());
        assert_eq!(lru.put('b', 30), Vec::<char>::new()); // 正好 60
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.bytes(), 60);
    }
}
