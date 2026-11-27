//! `IN_FORMATS` 属性 blob 解析（DRM 平面支持的 (fourcc, modifier) 组合）。
//!
//! 内核把平面支持的**格式 + 内存修饰符**组合通过平面的 `IN_FORMATS` 属性
//! （只读 blob）暴露给用户态。直通扫描输出（DMA-BUF 零拷贝）必须在选平面前
//! 校验解码器导出的 `(fourcc, modifier)` 是否被该平面支持——只比 fourcc
//! 会在 `AddFB2` 或原子检查阶段失败（甚至让内核退回错误的解释）。
//!
//! 载荷结构（内核 UAPI `include/uapi/drm/drm_mode.h`）：
//!
//! ```text
//! struct drm_format_modifier_blob {
//!     __u32 version;          // = 1
//!     __u32 flags;
//!     __u32 count_formats;    // formats[] 元素数
//!     __u32 formats_offset;   // formats[] 的字节偏移
//!     __u32 count_modifiers;  // modifiers[] 元素数
//!     __u32 modifiers_offset; // modifiers[] 的字节偏移
//!     __u32 formats[];
//!     struct drm_format_modifier modifiers[];
//! };
//! struct drm_format_modifier {
//!     __u64 formats;  // 位掩码：第 i 位置位 ⇒ formats[offset + i] 支持本 modifier
//!     __u32 offset;   // 相对 formats[] 的索引偏移
//!     __u32 pad;
//!     __u64 modifier;
//! };
//! ```
//!
//! 本模块是**纯字节解析**（不接触设备），因此可在无硬件环境完整验证；
//! 真机上由 [`crate::drm_kms`] 读取 blob 后调用。

/// 一个 `(fourcc, modifier)` 组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatModifier {
    /// DRM fourcc（如 `NV12` = 0x3231564E）。
    pub fourcc: u32,
    /// DRM format modifier（如 i915 Y_TILED = 0x0100000000000002；0 = LINEAR）。
    pub modifier: u64,
}

/// `IN_FORMATS` blob 头部字节数（6 × u32）。
const HEADER_LEN: usize = 24;
/// 单个 `drm_format_modifier` 描述项字节数（u64 + u32 + u32 + u64）。
const MODIFIER_LEN: usize = 24;
/// 本实现支持的 blob 版本。
const SUPPORTED_VERSION: u32 = 1;
/// 位掩码位数（`formats` 字段是 u64，一次覆盖 64 个格式）。
const MASK_BITS: u32 = 64;

fn read_u32(blob: &[u8], offset: usize) -> Result<u32, String> {
    let bytes = blob
        .get(offset..offset + 4)
        .ok_or_else(|| format!("IN_FORMATS blob 截断：读取 u32 @ {offset} 失败"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(blob: &[u8], offset: usize) -> Result<u64, String> {
    let bytes = blob
        .get(offset..offset + 8)
        .ok_or_else(|| format!("IN_FORMATS blob 截断：读取 u64 @ {offset} 失败"))?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

/// 解析 `IN_FORMATS` blob 为 `(fourcc, modifier)` 列表。
///
/// 返回顺序与 blob 中的 modifier 顺序一致（同一 modifier 下的多个格式按位序）。
///
/// # Errors
///
/// 版本不受支持、长度不足或偏移越界时返回可读错误信息（调用方应把它当作
/// "无法校验该平面"，而不是静默按支持处理）。
pub fn parse_in_formats(blob: &[u8]) -> Result<Vec<FormatModifier>, String> {
    if blob.len() < HEADER_LEN {
        return Err(format!(
            "IN_FORMATS blob 太短：{} 字节（头部需要 {HEADER_LEN}）",
            blob.len()
        ));
    }
    let version = read_u32(blob, 0)?;
    if version != SUPPORTED_VERSION {
        return Err(format!(
            "IN_FORMATS 版本不支持：{version}（本实现只支持 {SUPPORTED_VERSION}）"
        ));
    }
    let count_formats = read_u32(blob, 8)? as usize;
    let formats_offset = read_u32(blob, 12)? as usize;
    let count_modifiers = read_u32(blob, 16)? as usize;
    let modifiers_offset = read_u32(blob, 20)? as usize;

    let formats_end = formats_offset
        .checked_add(count_formats.saturating_mul(4))
        .ok_or_else(|| "IN_FORMATS formats 区间溢出".to_string())?;
    if formats_end > blob.len() {
        return Err(format!(
            "IN_FORMATS formats 越界：需要 {formats_end} 字节，实际 {}",
            blob.len()
        ));
    }
    let modifiers_end = modifiers_offset
        .checked_add(count_modifiers.saturating_mul(MODIFIER_LEN))
        .ok_or_else(|| "IN_FORMATS modifiers 区间溢出".to_string())?;
    if modifiers_end > blob.len() {
        return Err(format!(
            "IN_FORMATS modifiers 越界：需要 {modifiers_end} 字节，实际 {}",
            blob.len()
        ));
    }

    let mut out = Vec::new();
    for m in 0..count_modifiers {
        let base = modifiers_offset + m * MODIFIER_LEN;
        let mask = read_u64(blob, base)?;
        let offset = read_u32(blob, base + 8)? as usize;
        // base + 12 是 pad，跳过。
        let modifier = read_u64(blob, base + 16)?;
        for bit in 0..MASK_BITS {
            if mask & (1u64 << bit) == 0 {
                continue;
            }
            let index = offset + bit as usize;
            if index >= count_formats {
                // 位指向 formats[] 之外：跳过该位（内核不会这样生成，但不 panic）。
                continue;
            }
            let fourcc = read_u32(blob, formats_offset + index * 4)?;
            out.push(FormatModifier { fourcc, modifier });
        }
    }
    Ok(out)
}

/// 平面是否支持给定 `(fourcc, modifier)` 组合。
///
/// `modifier == 0`（LINEAR）与其它值一视同仁：内核在 `IN_FORMATS` 里同样以
/// `modifier = 0` 列出线性布局。
pub fn supports(combos: &[FormatModifier], fourcc: u32, modifier: u64) -> bool {
    combos
        .iter()
        .any(|c| c.fourcc == fourcc && c.modifier == modifier)
}

/// 平面是否支持给定 fourcc（忽略 modifier；用于"至少支持该格式"的诊断）。
pub fn supports_fourcc(combos: &[FormatModifier], fourcc: u32) -> bool {
    combos.iter().any(|c| c.fourcc == fourcc)
}

/// 把组合列表格式化为诊断字符串（形如 `NV12+0x0100000000000002, XR24+0x0`）。
pub fn describe(combos: &[FormatModifier], limit: usize) -> String {
    if combos.is_empty() {
        return "（IN_FORMATS 为空）".to_string();
    }
    let mut parts: Vec<String> = combos
        .iter()
        .take(limit)
        .map(|c| format!("{}+0x{:016x}", fourcc_name(c.fourcc), c.modifier))
        .collect();
    if combos.len() > limit {
        parts.push(format!("…（共 {} 项）", combos.len()));
    }
    parts.join(", ")
}

/// 常见 fourcc 的可读名（未知则 `0xXXXXXXXX`）。
pub fn fourcc_name(fourcc: u32) -> String {
    let bytes = fourcc.to_le_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("0x{fourcc:08x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 `IN_FORMATS` blob。
    fn build_blob(formats: &[u32], modifiers: &[(u64, u32, u64)]) -> Vec<u8> {
        let formats_offset = HEADER_LEN;
        let modifiers_offset = formats_offset + formats.len() * 4;
        let mut blob = Vec::new();
        blob.extend_from_slice(&1u32.to_le_bytes()); // version
        blob.extend_from_slice(&0u32.to_le_bytes()); // flags
        blob.extend_from_slice(&(formats.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(formats_offset as u32).to_le_bytes());
        blob.extend_from_slice(&(modifiers.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(modifiers_offset as u32).to_le_bytes());
        for f in formats {
            blob.extend_from_slice(&f.to_le_bytes());
        }
        for (mask, offset, modifier) in modifiers {
            blob.extend_from_slice(&mask.to_le_bytes());
            blob.extend_from_slice(&offset.to_le_bytes());
            blob.extend_from_slice(&0u32.to_le_bytes()); // pad
            blob.extend_from_slice(&modifier.to_le_bytes());
        }
        blob
    }

    const NV12: u32 = 0x3231_564e; // "NV12"
    const XR24: u32 = 0x3432_5258; // "XR24"
    const LINEAR: u64 = 0;
    const Y_TILED: u64 = 0x0100_0000_0000_0002;

    #[test]
    fn parses_i915_like_blob() {
        // 形如 i915 的 IN_FORMATS：NV12/XR24 各支持 LINEAR 与 Y_TILED。
        let blob = build_blob(
            &[NV12, XR24],
            &[
                (0b11, 0, LINEAR),  // 两个格式都支持 LINEAR
                (0b11, 0, Y_TILED), // 两个格式都支持 Y_TILED
            ],
        );
        let combos = parse_in_formats(&blob).unwrap();
        assert_eq!(combos.len(), 4);
        assert!(supports(&combos, NV12, LINEAR));
        assert!(supports(&combos, NV12, Y_TILED));
        assert!(supports(&combos, XR24, Y_TILED));
        assert!(!supports(&combos, NV12, 0x0100_0000_0000_0003));
        // 未列出的格式组合必须判为不支持（这是选平面的关键判据）。
        assert!(!supports(&combos, 0x3231_564d, Y_TILED));
    }

    #[test]
    fn honors_bitmask_offset_pairing() {
        // 位掩码 + offset 的配对：第 0 位对应 formats[offset]，第 3 位对应 formats[offset+3]。
        let blob = build_blob(
            &[XR24, 0xaaaa_aaaa, 0xbbbb_bbbb, NV12],
            &[(0b1001, 0, Y_TILED)], // 只第 0 与第 3 位
        );
        let combos = parse_in_formats(&blob).unwrap();
        assert_eq!(
            combos,
            vec![
                FormatModifier {
                    fourcc: XR24,
                    modifier: Y_TILED
                },
                FormatModifier {
                    fourcc: NV12,
                    modifier: Y_TILED
                },
            ]
        );
    }

    #[test]
    fn offset_shifts_into_formats_array() {
        // offset = 1：第 0 位 ⇒ formats[1]。
        let blob = build_blob(&[XR24, NV12], &[(0b1, 1, Y_TILED)]);
        let combos = parse_in_formats(&blob).unwrap();
        assert_eq!(
            combos,
            vec![FormatModifier {
                fourcc: NV12,
                modifier: Y_TILED
            }]
        );
    }

    #[test]
    fn bits_beyond_formats_are_ignored_not_panicking() {
        // 位指向 formats[] 之外：不 panic，只是不产生条目。
        let blob = build_blob(&[NV12], &[(0b111, 0, Y_TILED)]);
        let combos = parse_in_formats(&blob).unwrap();
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0].fourcc, NV12);
    }

    #[test]
    fn rejects_short_and_bad_version_blobs() {
        assert!(parse_in_formats(&[]).is_err());
        assert!(parse_in_formats(&[0u8; 23]).is_err());
        let mut blob = build_blob(&[NV12], &[]);
        blob[0] = 2; // version = 2
        let err = parse_in_formats(&blob).unwrap_err();
        assert!(err.contains("版本"), "实际: {err}");
    }

    #[test]
    fn rejects_out_of_range_offsets() {
        let mut blob = build_blob(&[NV12], &[(0b1, 0, Y_TILED)]);
        // 把 formats_offset 改成越界值。
        blob[12..16].copy_from_slice(&0xffff_0000u32.to_le_bytes());
        let err = parse_in_formats(&blob).unwrap_err();
        assert!(err.contains("越界"), "实际: {err}");
    }

    #[test]
    fn describe_is_human_readable() {
        let combos = vec![
            FormatModifier {
                fourcc: NV12,
                modifier: Y_TILED,
            },
            FormatModifier {
                fourcc: XR24,
                modifier: 0,
            },
        ];
        let text = describe(&combos, 8);
        assert!(text.contains("NV12+0x0100000000000002"), "实际: {text}");
        assert!(text.contains("XR24+0x0000000000000000"), "实际: {text}");
        assert_eq!(describe(&[], 8), "（IN_FORMATS 为空）");
        assert!(describe(&combos, 1).contains("共 2 项"));
    }
}

/// 带重试的解析结果缓存：**失败不粘住**。
///
/// 背景（t17 审计 A.8 ④）：`IN_FORMATS` 的读取要经过"取属性 → 读 blob → 解析"
/// 三步，任何一步失败以前都会把 `None` 永久缓存到设备重开为止；而
/// [`crate::drm_kms`] 的 `plane_for_dmabuf()` 在 `None` 时会退化为"仅比 fourcc"——
/// 于是**一次偶发失败会让双重校验在余下整个绑定生命周期内失效**。
///
/// 本缓存把 `None` 视为**临时**结论：距上次尝试超过 `retry_interval` 就重新解析，
/// 成功即恢复双重校验，并把尝试次数暴露给日志。
#[derive(Debug)]
pub struct RetryCache<K, V> {
    entries: std::collections::HashMap<K, Entry<V>>,
    retry_interval: std::time::Duration,
}

#[derive(Debug)]
struct Entry<V> {
    value: Option<V>,
    last_attempt: std::time::Instant,
    attempts: u32,
}

/// 一次 [`RetryCache::get_or_try`] 的结果（附带诊断信息，便于日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// 命中已有成功结果。
    Hit,
    /// 首次尝试即成功。
    LoadedFirstTry,
    /// 此前失败过，本次重试成功（双重校验**已恢复**）。
    RecoveredAfterFailure,
    /// 结果不可用（仍在失败），本次是否真的重新尝试见 `retried`。
    Unavailable {
        /// 本次调用是否真的重新读取（`false` = 处于重试冷却期内）。
        retried: bool,
        /// 累计失败尝试次数。
        attempts: u32,
    },
}

impl<K, V> RetryCache<K, V>
where
    K: std::hash::Hash + Eq + Copy,
{
    /// 构造：`retry_interval` 是失败后的最小重试间隔。
    pub fn new(retry_interval: std::time::Duration) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            retry_interval,
        }
    }

    /// 取缓存值；失败时按 `retry_interval` 冷却后重试 `load`。
    ///
    /// 返回 `(值, 结果分类)`：值借用自缓存（调用方可直接使用）。
    pub fn get_or_try<F>(
        &mut self,
        key: K,
        now: std::time::Instant,
        mut load: F,
    ) -> (Option<&V>, CacheOutcome)
    where
        F: FnMut() -> Option<V>,
    {
        let outcome;
        {
            let retry_interval = self.retry_interval;
            let entry = self.entries.entry(key).or_insert(Entry {
                value: None,
                last_attempt: now,
                attempts: 0,
            });
            if entry.value.is_some() {
                outcome = CacheOutcome::Hit;
            } else {
                let previous_attempts = entry.attempts;
                let due = previous_attempts == 0
                    || now.duration_since(entry.last_attempt) >= retry_interval;
                if !due {
                    outcome = CacheOutcome::Unavailable {
                        retried: false,
                        attempts: previous_attempts,
                    };
                } else {
                    entry.last_attempt = now;
                    entry.attempts = previous_attempts.saturating_add(1);
                    match load() {
                        Some(value) => {
                            entry.value = Some(value);
                            outcome = if previous_attempts == 0 {
                                CacheOutcome::LoadedFirstTry
                            } else {
                                CacheOutcome::RecoveredAfterFailure
                            };
                        }
                        None => {
                            outcome = CacheOutcome::Unavailable {
                                retried: true,
                                attempts: entry.attempts,
                            };
                        }
                    }
                }
            }
        }
        (
            self.entries.get(&key).and_then(|e| e.value.as_ref()),
            outcome,
        )
    }

    /// 丢弃全部缓存（设备重开时调用）。
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 已知成功结果的数量（诊断/测试用）。
    pub fn healthy_entries(&self) -> usize {
        self.entries.values().filter(|e| e.value.is_some()).count()
    }

    /// 累计失败尝试次数（诊断/测试用）。
    pub fn failed_attempts(&self, key: K) -> u32 {
        self.entries
            .get(&key)
            .filter(|e| e.value.is_none())
            .map(|e| e.attempts)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod retry_cache_tests {
    use super::*;
    use std::time::{Duration, Instant};

    const KEY: u32 = 44;

    #[test]
    fn success_is_cached_without_reloading() {
        let mut cache: RetryCache<u32, Vec<FormatModifier>> =
            RetryCache::new(Duration::from_secs(5));
        let t0 = Instant::now();
        let mut loads = 0;
        let (v, outcome) = cache.get_or_try(KEY, t0, || {
            loads += 1;
            Some(vec![FormatModifier {
                fourcc: NV12,
                modifier: Y_TILED,
            }])
        });
        assert_eq!(outcome, CacheOutcome::LoadedFirstTry);
        assert!(v.is_some());
        // 后续 10 次都命中缓存，不再读取。
        for i in 1..10 {
            let (v, outcome) = cache.get_or_try(KEY, t0 + Duration::from_millis(i * 100), || {
                loads += 1;
                None
            });
            assert_eq!(outcome, CacheOutcome::Hit);
            assert!(v.is_some());
        }
        assert_eq!(loads, 1, "成功结果必须缓存，不得重复读取");
    }

    #[test]
    fn failure_is_temporary_not_sticky() {
        // 核心回归（t17 A.8 ④）：失败后**必须**在冷却期满后重试，而不是永久禁用。
        let mut cache: RetryCache<u32, Vec<FormatModifier>> =
            RetryCache::new(Duration::from_secs(5));
        let t0 = Instant::now();
        let mut loads = 0;
        // 第一次：失败。
        let (v, outcome) = cache.get_or_try(KEY, t0, || {
            loads += 1;
            None
        });
        assert!(v.is_none());
        assert_eq!(
            outcome,
            CacheOutcome::Unavailable {
                retried: true,
                attempts: 1
            }
        );
        // 冷却期内：不重读（避免每帧 ioctl + 刷日志）。
        let (v, outcome) = cache.get_or_try(KEY, t0 + Duration::from_secs(1), || {
            loads += 1;
            None
        });
        assert!(v.is_none());
        assert_eq!(
            outcome,
            CacheOutcome::Unavailable {
                retried: false,
                attempts: 1
            }
        );
        assert_eq!(loads, 1, "冷却期内不得重读");
        // 冷却期满：真正重试，并在这次成功 ⇒ 双重校验恢复。
        let (v, outcome) = cache.get_or_try(KEY, t0 + Duration::from_secs(5), || {
            loads += 1;
            Some(vec![FormatModifier {
                fourcc: NV12,
                modifier: Y_TILED,
            }])
        });
        assert!(v.is_some(), "重试成功后必须恢复双重校验");
        assert_eq!(outcome, CacheOutcome::RecoveredAfterFailure);
        assert_eq!(loads, 2);
        assert_eq!(cache.healthy_entries(), 1);
    }

    #[test]
    fn repeated_failures_are_counted_and_cooled() {
        let mut cache: RetryCache<u32, Vec<FormatModifier>> =
            RetryCache::new(Duration::from_secs(2));
        let t0 = Instant::now();
        let mut loads = 0;
        for i in 0..10 {
            let _ = cache.get_or_try(KEY, t0 + Duration::from_millis(i * 500), || {
                loads += 1;
                None
            });
        }
        // 5 秒 / 2 秒间隔 ⇒ 首次 + 2 次到期 ≈ 3 次（不是 10 次）。
        assert!(
            (1..=3).contains(&loads),
            "重试必须被冷却限流，实际 loads={loads}"
        );
        assert_eq!(cache.failed_attempts(KEY), loads);
        cache.clear();
        assert_eq!(cache.failed_attempts(KEY), 0);
    }

    const NV12: u32 = 0x3231_564e;
    const Y_TILED: u64 = 0x0100_0000_0000_0002;
}
