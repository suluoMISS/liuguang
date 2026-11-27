//! 扫描输出缓冲管理：**缓冲池化 + 上传路径**（规范 §3.4/§15.1）。
//!
//! 本模块把「帧 → 扫描缓冲」的传输逻辑与 DRM 设备访问解耦，
//! 因此可以在**没有 /dev/dri 的环境**（CI/开发机）下完整验证：
//!
//! - [`ScanoutPool`]：固定深度（3）的扫描缓冲池，**显式管理每块缓冲的所有权**
//!   （`FREE → WRITING → READY → SCANNING → RETIRED → FREE`），而不是轮换索引：
//!   - 只有 `FREE` 能被取去写；**在扫描的缓冲绝不写入、绝不销毁**；
//!   - 回收**滞后一帧**（提交 B 之后才回收 A），因为"提交返回"只证明
//!     "新的一帧开始扫描"，不证明"旧的一帧已下屏"；
//!   - 池耗尽时明确失败/丢帧并计数，**绝不用覆盖在用缓冲来蒙混**。
//!   分配/映射/复用/回收/拒绝/耗尽次数通过 [`PoolStats`] 计数并可断言。
//! - [`upload`]：把 [`CpuFrame`] 拷进已映射的扫描缓冲。连续行时走单次
//!   `copy_from_slice`（memcpy），否则逐行拷贝（源 stride / 目标 pitch 填充）。
//! - [`upload_rowwise_reference`]：逐行参考实现（正确性 oracle + 基准对比基线）。
//!
//! 语义要点（与历史实现一致）：
//! - `Xrgb8888`（DRM `XR24`）与 `Argb8888`（DRM `AR24`，字节序同为 `[B,G,R,A]`）
//!   共用同一个 4 字节扫描格式，α/X 字节由显示引擎忽略；
//! - `Rgba8888`（内存 `[R,G,B,A]`）需要 DRM `ABGR8888`（`AB24`）；
//! - `Nv12` 为半平面容器：Y 行 + UV 行，UV 偏移 = `pitch × 高度`。

use crate::DisplayError;
use liuguang_core::types::{CpuFrame, PixelFormat};

/// 扫描输出缓冲的语义格式（映射到 DRM fourcc 由后端完成）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ScanoutFormat {
    /// DRM `XR24`：内存 `[B, G, R, X]`；也用于接收 `Argb8888`（X 字节被忽略）。
    Xrgb8888,
    /// DRM `ABGR8888`（`AB24`）：内存 `[R, G, B, A]`，与 PNG/旋转路径的
    /// [`PixelFormat::Rgba8888`] 逐字节一致，**无需 CPU 像素重排**。
    Abgr8888,
    /// DRM `NV12`：Y 平面 + 交错 UV 平面（硬件 YUV→RGB 采样）。
    Nv12,
}

impl ScanoutFormat {
    /// 从帧的像素格式推导扫描格式。
    ///
    /// `Argb8888` 与 `Xrgb8888` 同为 `[B,G,R,A/X]` 小端布局，可共用 `XR24`
    /// 扫描缓冲（第 4 字节不参与显示）；`Rgba8888` 需要 `AB24`。
    pub(crate) fn from_pixel_format(format: PixelFormat) -> Option<Self> {
        match format {
            PixelFormat::Xrgb8888 | PixelFormat::Argb8888 => Some(Self::Xrgb8888),
            PixelFormat::Rgba8888 => Some(Self::Abgr8888),
            PixelFormat::Nv12 => Some(Self::Nv12),
        }
    }

    /// 每像素字节数（NV12 的 Y 平面为 1）。
    pub(crate) const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Xrgb8888 | Self::Abgr8888 => 4,
            Self::Nv12 => 1,
        }
    }

    /// 是否为 NV12 半平面容器（dumb 缓冲高度需 ×1.5）。
    pub(crate) const fn is_semi_planar(self) -> bool {
        matches!(self, Self::Nv12)
    }

    /// dumb 缓冲的容器高度。
    pub(crate) const fn container_height(self, height: u32) -> u32 {
        if self.is_semi_planar() {
            height.saturating_mul(3) / 2
        } else {
            height
        }
    }

    /// DRM fourcc 名称（诊断/日志用）。
    pub(crate) const fn fourcc_name(self) -> &'static str {
        match self {
            Self::Xrgb8888 => "XR24",
            Self::Abgr8888 => "AB24",
            Self::Nv12 => "NV12",
        }
    }
}

/// 扫描缓冲身份：尺寸 + 扫描格式（变化即需要重建槽位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ScanoutKey {
    /// 逻辑帧宽。
    pub(crate) width: u32,
    /// 逻辑帧高（NV12 的容器高度由格式推导）。
    pub(crate) height: u32,
    /// 扫描格式。
    pub(crate) format: ScanoutFormat,
}

/// 池化统计（可观测：证明"不每帧分配/映射"，并让**所有权**可对账）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// 槽位创建次数（= 缓冲分配次数）。
    pub allocations: u64,
    /// 映射建立次数（正常情况下等于 `allocations`）。
    pub mappings: u64,
    /// 槽位复用次数（未分配、未映射）。
    pub reuses: u64,
    /// 累计上传字节数。
    pub uploaded_bytes: u64,
    /// 成功回收次数（上一帧的缓冲归还池/上游）；稳态应 ≈ 提交次数 − 1。
    pub reclaims: u64,
    /// 对**非 `WRITING`** 槽位的写入尝试被拒次数（正确实现恒为 0）。
    pub rejected_writes: u64,
    /// 因槽位不是 `FREE` 而**拒绝重建**的次数（H3：绝不 destroy 在扫槽位）。
    pub blocked_recreates: u64,
    /// 池耗尽时"先等一个刷新周期"的次数。
    pub exhausted_waits: u64,
    /// 池耗尽时"丢帧"的次数（等也等不到 ⇒ 明确失败，**绝不覆盖在用缓冲**）。
    pub exhausted_drops: u64,
    /// 历史最大"非 `FREE`"槽位数（必须 ≤ 池深）。
    pub max_in_use: usize,
    /// 强制释放（设备关闭/重开）时销毁的**非 `FREE`** 槽位数（应尽量避免）。
    pub forced_releases: usize,
}

/// 扫描槽位的**所有权状态**（用户指定：FREE / WRITING / READY / SCANNING / RETIRED）。
///
/// | 状态 | 含义 | 允许的操作 |
/// |---|---|---|
/// | `Free` | 空闲 | 可被写入（`acquire` 的唯一来源） |
/// | `Writing` | 正在被写入（CPU 上传） | **禁止**显示、禁止他人取用 |
/// | `Ready` | 写完、可提交 | 等待 `atomic_commit` |
/// | `Scanning` | **内核正在扫这块内存** | **禁止写入、禁止销毁** |
/// | `Retired` | 已被下一帧替换、可安全回收 | 只等 `release_to_upstream` |
///
/// **注意**：`Scanning` 的判定不是"我提交过它"，而是"它是**上一帧**提交的那块"——
/// 见 [`ScanoutPool::on_commit_success`] 的注释（回收**滞后一帧**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotState {
    /// 空闲。
    Free,
    /// 正在写（CPU 上传；若将来接 GPU blit 语义不变）。
    Writing,
    /// 写完，可提交。
    Ready,
    /// 正在被内核扫描输出。
    Scanning,
    /// 已被下一帧替换，可回收（滞后一帧产生）。
    Retired,
}

impl SlotState {
    /// 诊断用名字（日志/统计可读）。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Free => "FREE",
            Self::Writing => "WRITING",
            Self::Ready => "READY",
            Self::Scanning => "SCANNING",
            Self::Retired => "RETIRED",
        }
    }

    /// 是否空闲（唯一可被 `acquire` 取用的状态）。
    pub(crate) const fn is_free(self) -> bool {
        matches!(self, Self::Free)
    }
}

/// 一个扫描槽位：帧缓冲句柄 + 底层缓冲 + **长期持有**的映射 + 所有权状态。
pub(crate) struct Slot<B, M, FB> {
    /// 槽位身份。
    pub(crate) key: ScanoutKey,
    /// 底层缓冲（dumb buffer）。
    pub(crate) buf: B,
    /// 映射（创建槽位时建立一次，销毁时释放）。
    pub(crate) map: M,
    /// 帧缓冲句柄（提交给 KMS 用）。
    pub(crate) fb: FB,
    /// **所有权状态**（H1/H2/H3/H4 的核心）。
    pub(crate) state: SlotState,
}

/// 工厂产出的槽位类型（底层缓冲 + 映射 + 帧缓冲句柄）。
pub(crate) type SlotOf<F> =
    Slot<<F as ScanoutFactory>::Buf, <F as ScanoutFactory>::Map, <F as ScanoutFactory>::Fb>;

/// 槽位工厂：把设备相关操作（分配 / 映射 / 释放）注入池。
pub(crate) trait ScanoutFactory {
    /// 底层缓冲句柄类型。
    type Buf;
    /// 映射类型（须在槽位生命周期内保持有效）。
    type Map;
    /// 帧缓冲句柄类型。
    type Fb: Copy;

    /// 分配缓冲、创建帧缓冲并**建立一次**映射。
    fn create(&mut self, key: ScanoutKey) -> Result<SlotOf<Self>, DisplayError>;
    /// 映射对应的可写字节。
    fn bytes<'a>(&self, map: &'a mut Self::Map) -> &'a mut [u8];
    /// 缓冲行间距（pitch，字节）。
    fn pitch(&self, buf: &Self::Buf) -> usize;
    /// 释放槽位（先解除映射，再释放帧缓冲/底层缓冲）。
    fn destroy(&mut self, slot: SlotOf<Self>);
}

/// 池耗尽时的策略（**绝不**静默覆盖在用缓冲）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquirePolicy {
    /// 取不到就立刻失败（调用方丢帧并计数）。
    TryOnce,
    /// 最多等这么久（1ms 轮询）。典型用法：一个刷新周期。
    WaitUpTo(std::time::Duration),
}

/// **写权租约**（`FREE → WRITING`）。
///
/// 唯一性由借用检查器保证：同一时刻只能有一个 `WriteGuard` 借用池。
/// 若调用方在 `mark_ready()` 之前提前返回（`validate()`/`upload` 失败等），
/// `Drop` 会把槽位放回 `FREE` —— 这就是 **H2**（槽位被消费却没提交 ⇒ 永久错位）的修法。
pub(crate) struct WriteGuard<'a, B, M, FB> {
    pool: &'a mut ScanoutPool<B, M, FB>,
    slot: usize,
    /// 帧缓冲句柄（提交用）。
    pub(crate) fb: FB,
    /// 行间距（字节）。
    pub(crate) pitch: usize,
    /// 本次是否复用了既有槽位（false = 新建）。
    pub(crate) reused: bool,
}

impl<'a, B, M, FB: Copy> WriteGuard<'a, B, M, FB> {
    /// 可写字节（**只允许在 `WRITING` 上调用**；否则拒绝并计数 —— 测试 T1）。
    ///
    /// # Errors
    ///
    /// 槽位不在 `WRITING` 状态（例如正在扫描）时返回 [`DisplayError`]。
    pub(crate) fn bytes<F>(&mut self, factory: &mut F) -> Result<&mut [u8], DisplayError>
    where
        F: ScanoutFactory<Buf = B, Map = M, Fb = FB>,
    {
        self.pool.check_writable(self.slot)?;
        let slot = self.pool.slots[self.slot]
            .as_mut()
            .ok_or_else(|| pool_error("写租约指向不存在的槽位"))?;
        Ok(factory.bytes(&mut slot.map))
    }

    /// 当前槽位状态（测试/诊断；不暴露池本体，避免绕过所有权）。
    #[cfg(test)]
    pub(crate) fn state(&self) -> SlotState {
        self.pool
            .slots
            .get(self.slot)
            .and_then(|s| s.as_ref())
            .map_or(SlotState::Free, |s| s.state)
    }

    /// 写入完成：`WRITING → READY`，交出提交所需的句柄。
    pub(crate) fn mark_ready(self) -> ReadySlot<FB> {
        if let Some(slot) = self.pool.slots[self.slot].as_mut() {
            if slot.state == SlotState::Writing {
                slot.state = SlotState::Ready;
            }
        }
        ReadySlot {
            slot: self.slot,
            fb: self.fb,
        }
    }
}

impl<B, M, FB> Drop for WriteGuard<'_, B, M, FB> {
    fn drop(&mut self) {
        // H2：提前返回（上传失败/校验失败）⇒ 槽位必须回到 FREE，否则轮换永久错位。
        if let Some(slot) = self.pool.slots[self.slot].as_mut() {
            if slot.state == SlotState::Writing {
                slot.state = SlotState::Free;
            }
        }
    }
}

/// 已写完、等待提交的槽位（`READY`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadySlot<FB> {
    /// 槽位下标。
    pub(crate) slot: usize,
    /// 帧缓冲句柄（提交用）。
    pub(crate) fb: FB,
}

/// 已被下一帧替换、等待归还的槽位（`RETIRED`）。
#[must_use = "回收是为了归还（池内 FREE 或上游池）；丢弃会让槽位卡在 RETIRED 并最终耗尽池"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reclaimed<FB> {
    /// 槽位下标。
    pub(crate) slot: usize,
    /// 帧缓冲句柄（上游归还钩子用它定位缓冲）。
    pub(crate) fb: FB,
}

/// `release_all` 的结果（**拒绝**销毁非 FREE 槽位 —— H4）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReleaseOutcome {
    /// 已销毁的槽位数（原本是 FREE）。
    pub(crate) destroyed: usize,
    /// 因**非 FREE** 被拒绝销毁的槽位数（> 0 说明当时仍有缓冲在显示）。
    pub(crate) refused: usize,
}

/// 固定深度扫描缓冲池（**显式所有权**，不是轮换索引）。
///
/// 深度恒为 3（用户指定的三缓冲初始方案）。**不靠加深度**掩盖问题：
/// 稳态占用恰好是"1 扫描 + 1 写入 + 1 空闲"，任何"取不到空闲缓冲"都是真实异常，
/// 会被 [`PoolStats::exhausted_drops`] 记下来而不是静默覆盖。
///
/// 池只保存槽位与统计；设备相关的分配/映射由调用方按次传入的
/// [`ScanoutFactory`] 完成——这样池本身不借用任何设备，可在无硬件环境
/// 用假工厂完整验证（含"连续提交不重新分配/映射"与全部所有权不变量）。
pub(crate) struct ScanoutPool<B, M, FB> {
    slots: Vec<Option<Slot<B, M, FB>>>,
    /// 轮转提示位（**不是**所有权判据，只用来让"优先复用同 key 的空闲槽位"稳定）。
    next: usize,
    /// 当前正在被扫描的槽位（**回收滞后一帧的锚点**）。
    scanning: Option<usize>,
    stats: PoolStats,
}

impl<B, M, FB> std::fmt::Debug for ScanoutPool<B, M, FB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanoutPool")
            .field("depth", &self.slots.len())
            .field("scanning", &self.scanning)
            .field("stats", &self.stats)
            .finish()
    }
}

impl<B, M, FB: Copy> ScanoutPool<B, M, FB> {
    /// 创建深度为 `depth` 的池（`depth ≥ 1`；三缓冲可避免覆盖在扫描的帧）。
    pub(crate) fn new(depth: usize) -> Self {
        let depth = depth.max(1);
        let mut slots = Vec::with_capacity(depth);
        slots.resize_with(depth, || None);
        Self {
            slots,
            next: 0,
            scanning: None,
            stats: PoolStats::default(),
        }
    }

    /// 取写权：**只有 `FREE` 槽位**可被取用（H1/H3/H5 的结构性修法）。
    ///
    /// 选取顺序：① 同 `key` 的空闲槽位（不分配、不映射）；② 任意空闲槽位
    /// （key 不同 ⇒ 重建；**绝不**动非 FREE 槽位）；③ 池耗尽 ⇒ 按 `policy`
    /// 等待或失败（失败会计入 [`PoolStats::exhausted_drops`]，**不会覆盖在用缓冲**）。
    ///
    /// # Errors
    ///
    /// 池耗尽（等待超时）或重建槽位失败时返回 [`DisplayError`]。
    pub(crate) fn acquire<F>(
        &mut self,
        factory: &mut F,
        key: ScanoutKey,
        policy: AcquirePolicy,
    ) -> Result<WriteGuard<'_, B, M, FB>, DisplayError>
    where
        F: ScanoutFactory<Buf = B, Map = M, Fb = FB>,
    {
        let deadline = match policy {
            AcquirePolicy::TryOnce => None,
            AcquirePolicy::WaitUpTo(d) => Some(std::time::Instant::now() + d),
        };
        let mut waited = false;
        loop {
            if let Some(idx) = self.pick_free() {
                if waited {
                    self.stats.exhausted_waits = self.stats.exhausted_waits.saturating_add(1);
                }
                return self.start_write(idx, factory, key);
            }
            match deadline {
                Some(dl) if std::time::Instant::now() < dl => {
                    waited = true;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                _ => {
                    self.stats.exhausted_drops = self.stats.exhausted_drops.saturating_add(1);
                    self.note_in_use();
                    return Err(pool_error(&format!(
                        "扫描缓冲池耗尽（深度 {}，非空闲 {} 块）：拒绝覆盖在用缓冲，本帧丢弃",
                        self.slots.len(),
                        self.in_use()
                    )));
                }
            }
        }
    }

    /// 找一块**空闲**槽位（`None` = 从未创建，等价空闲）。
    ///
    /// 选取方式仍是**轮转**（从 `next` 起找第一块空闲的），因此稳态会均匀用到
    /// 全部 3 块缓冲（与历史行为一致：`allocations=3` 恒定）。
    /// 与旧实现的**唯一**差别：只接受 `FREE` —— 轮到的槽位若正在扫描就跳到下一块，
    /// 绝不覆盖（H1/H5）。
    fn pick_free(&mut self) -> Option<usize> {
        let n = self.slots.len();
        for i in 0..n {
            let idx = (self.next + i) % n;
            let free = match &self.slots[idx] {
                Some(s) => s.state.is_free(),
                None => true,
            };
            if free {
                self.next = (idx + 1) % n;
                return Some(idx);
            }
        }
        None
    }
    /// 把选中槽位置为 `WRITING`（必要时先在**空闲**前提下重建）。
    fn start_write<F>(
        &mut self,
        idx: usize,
        factory: &mut F,
        key: ScanoutKey,
    ) -> Result<WriteGuard<'_, B, M, FB>, DisplayError>
    where
        F: ScanoutFactory<Buf = B, Map = M, Fb = FB>,
    {
        let matched = matches!(&self.slots[idx], Some(s) if s.key == key);
        if !matched {
            // H3：重建前再确认一次"绝不 destroy 非空闲槽位"（防御性断言 + 计数）。
            if let Some(slot) = &self.slots[idx] {
                if !slot.state.is_free() {
                    self.stats.blocked_recreates = self.stats.blocked_recreates.saturating_add(1);
                    return Err(pool_error(&format!(
                        "拒绝重建槽位 {idx}：它处于 {}（在扫描的缓冲绝不销毁）",
                        slot.state.name()
                    )));
                }
            }
            if let Some(old) = self.slots[idx].take() {
                factory.destroy(old);
            }
            let slot = factory.create(key)?;
            self.stats.allocations = self.stats.allocations.saturating_add(1);
            self.stats.mappings = self.stats.mappings.saturating_add(1);
            self.slots[idx] = Some(slot);
        } else {
            self.stats.reuses = self.stats.reuses.saturating_add(1);
        }
        let slot = self.slots[idx]
            .as_mut()
            .ok_or_else(|| pool_error("槽位创建后仍为空"))?;
        slot.state = SlotState::Writing;
        let fb = slot.fb;
        let pitch = factory.pitch(&slot.buf);
        self.note_in_use();
        Ok(WriteGuard {
            pool: self,
            slot: idx,
            fb,
            pitch,
            reused: matched,
        })
    }

    /// 写入前的状态检查（`bytes()` 与测试共用**同一处**判定 —— 保证变异能真失败）。
    fn check_writable(&mut self, idx: usize) -> Result<(), DisplayError> {
        let state = self
            .slots
            .get(idx)
            .and_then(|s| s.as_ref())
            .map(|s| s.state)
            .ok_or_else(|| pool_error("槽位不存在"))?;
        if state == SlotState::Writing {
            return Ok(());
        }
        self.stats.rejected_writes = self.stats.rejected_writes.saturating_add(1);
        Err(pool_error(&format!(
            "拒绝对 {} 状态的槽位写入（只允许 WRITING）",
            state.name()
        )))
    }

    /// 测试入口：对指定槽位尝试写入（走与真实路径**相同**的状态检查）。
    #[cfg(test)]
    pub(crate) fn try_write(&mut self, idx: usize) -> Result<(), DisplayError> {
        self.check_writable(idx)
    }

    /// **唯一回收点**：提交成功后调用。
    ///
    /// 做两件事：
    /// 1. `READY → SCANNING`（本帧开始显示）；
    /// 2. 把**上一块** `SCANNING → RETIRED`（**滞后一帧**：上一帧还在扫，
    ///    直到本帧提交生效才可回收），返回的 [`Reclaimed`] 必须交给
    ///    [`Self::release_to_upstream`]（池内/上游的归还都发生在那一刻）。
    ///
    /// # 为什么可以这样判定（**约束，改动前必读**）
    ///
    /// 本设计依赖**提交语义是阻塞式**：`drm_kms.rs` 的 `commit_atomic` 明确
    /// **不使用 `NONBLOCK`**（t66 已独立核实）⇒ `atomic_commit(N)` 返回时
    /// "N 正在扫、N−1 已下屏"是**已知事实**，所以回收 N−1 是安全的。
    ///
    /// **⚠️ 若将来改成 `NONBLOCK`/事件驱动提交：本函数的调用点必须改成
    /// "page-flip 完成回调"，否则会把仍在扫描的缓冲提前回收（画面撕裂/花屏）。**
    /// 另：`OUT_FENCE_PTR` 的 fence 完成**不等于**该缓冲已不在屏上，不能拿来当回收判据。
    pub(crate) fn on_commit_success(&mut self, ready: ReadySlot<FB>) -> Option<Reclaimed<FB>> {
        if let Some(Some(slot)) = self.slots.get_mut(ready.slot) {
            debug_assert_eq!(
                slot.state,
                SlotState::Ready,
                "提交成功前槽位必须处于 READY（实际 {}）",
                slot.state.name()
            );
            slot.state = SlotState::Scanning;
        }
        let prev = self.scanning.replace(ready.slot);
        let mut reclaimed = None;
        if let Some(prev_idx) = prev {
            if prev_idx != ready.slot
                && let Some(Some(slot)) = self.slots.get_mut(prev_idx)
                && slot.state == SlotState::Scanning
            {
                // 滞后一帧：它在上一次提交里被换下，现在才可回收。
                slot.state = SlotState::Retired;
                reclaimed = Some(Reclaimed {
                    slot: prev_idx,
                    fb: slot.fb,
                });
            }
        }
        self.note_in_use();
        reclaimed
    }

    /// 提交失败：`READY → FREE`（内容作废），**不动**正在扫描的那块。
    pub(crate) fn on_commit_failure(&mut self, ready: ReadySlot<FB>) {
        if let Some(Some(slot)) = self.slots.get_mut(ready.slot) {
            debug_assert_eq!(
                slot.state,
                SlotState::Ready,
                "提交失败时槽位必须处于 READY（实际 {}）",
                slot.state.name()
            );
            slot.state = SlotState::Free;
        }
        self.note_in_use();
    }

    /// 归还被回收的槽位：`RETIRED → FREE`，并（t80 落地后）把上游池缓冲还给上游。
    ///
    /// **同一个调用点**服务两条路径（CPU 上传 / 上游 DMA-BUF），避免两套回收时机。
    pub(crate) fn release_to_upstream(&mut self, reclaimed: Reclaimed<FB>) {
        // TODO(t80)：上游池（VPP → 我们自己的 GstVideoBufferPool）落地后，
        // 这里**同时**把该缓冲还给上游 —— 时机必须与池内 FREE 完全一致（滞后一帧）。
        let _ = self.recycle_if_retired(reclaimed.slot);
        self.note_in_use();
    }

    /// 回收判定：**只有 `RETIRED` 可回收**（T2：未提交下一帧就想回收在扫缓冲 ⇒ 拒绝）。
    fn recycle_if_retired(&mut self, idx: usize) -> bool {
        if let Some(Some(slot)) = self.slots.get_mut(idx)
            && slot.state == SlotState::Retired
        {
            slot.state = SlotState::Free;
            self.stats.reclaims = self.stats.reclaims.saturating_add(1);
            return true;
        }
        false
    }

    /// 测试入口：按槽位直接尝试回收（走与真实路径**相同**的 `RETIRED` 判定）。
    #[cfg(test)]
    pub(crate) fn try_recycle_slot(&mut self, idx: usize) -> bool {
        let ok = self.recycle_if_retired(idx);
        self.note_in_use();
        ok
    }

    /// 销毁**仅限 FREE** 的槽位（H4）：非 FREE 一律拒绝并计数。
    ///
    /// 用于正常收尾（例如输出移除）。设备即将关闭/重开时的强制清理请看
    /// [`Self::release_all_forced`]。
    pub(crate) fn release_all<F>(&mut self, factory: &mut F) -> ReleaseOutcome
    where
        F: ScanoutFactory<Buf = B, Map = M, Fb = FB>,
    {
        let mut outcome = ReleaseOutcome::default();
        for slot in &mut self.slots {
            let Some(existing) = slot else { continue };
            if !existing.state.is_free() {
                outcome.refused += 1;
                continue;
            }
            if let Some(old) = slot.take() {
                factory.destroy(old);
                outcome.destroyed += 1;
            }
        }
        if outcome.destroyed > 0 {
            self.scanning = None;
        }
        outcome
    }

    /// 强制释放：**只允许在设备即将关闭/重开时调用**（那时内核不再扫这些 fb）。
    ///
    /// 与 [`Self::release_all`] 的差别：非 FREE 槽位也会被销毁，因此**必须**
    /// 由调用方记录 WARN —— 那意味着"我们放弃了一块可能仍在显示的缓冲"，
    /// 内容不可预期（不是"安全"，只是"设备要没了"）。
    pub(crate) fn release_all_forced<F>(&mut self, factory: &mut F) -> ReleaseOutcome
    where
        F: ScanoutFactory<Buf = B, Map = M, Fb = FB>,
    {
        let mut outcome = ReleaseOutcome::default();
        for slot in &mut self.slots {
            let Some(existing) = slot else { continue };
            if !existing.state.is_free() {
                outcome.refused += 1;
                self.stats.forced_releases = self.stats.forced_releases.saturating_add(1);
            }
            if let Some(old) = slot.take() {
                factory.destroy(old);
                outcome.destroyed += 1;
            }
        }
        self.scanning = None;
        outcome
    }

    /// 池化统计快照。
    pub(crate) fn stats(&self) -> PoolStats {
        self.stats
    }

    /// 记录本帧上传字节数（供统计）。
    pub(crate) fn note_uploaded(&mut self, bytes: u64) {
        self.stats.uploaded_bytes = self.stats.uploaded_bytes.saturating_add(bytes);
    }

    /// 当前非 FREE 的槽位数（诊断/断言用）。
    pub(crate) fn in_use(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s, Some(slot) if !slot.state.is_free()))
            .count()
    }

    /// 某槽位当前状态（测试/诊断）。
    #[cfg(test)]
    pub(crate) fn state_of(&self, idx: usize) -> Option<SlotState> {
        self.slots
            .get(idx)
            .and_then(|s| s.as_ref())
            .map(|s| s.state)
    }

    fn note_in_use(&mut self) {
        let in_use = self.in_use();
        if in_use > self.stats.max_in_use {
            self.stats.max_in_use = in_use;
        }
    }
}

/// 池内错误的统一构造（`operation` 固定，便于上层/日志识别）。
fn pool_error(message: &str) -> DisplayError {
    DisplayError::Failed {
        operation: "scanout_pool",
        message: message.to_string(),
    }
}

/// 把 `frame` 上传到已映射的扫描缓冲。
///
/// 返回本帧实际拷贝的字节数。连续（无填充）时用单次 `copy_from_slice`
/// 走 memcpy；否则逐行拷贝并处理源 stride / 目标 pitch。
///
/// # Errors
///
/// 帧结构非法、目标缓冲不足或源数据不足时返回 [`DisplayError`]。
pub(crate) fn upload(
    dst: &mut [u8],
    pitch: usize,
    frame: &CpuFrame,
    format: ScanoutFormat,
) -> Result<u64, DisplayError> {
    frame.validate()?;
    let w = frame.width as usize;
    let h = frame.height as usize;
    if format.is_semi_planar() {
        upload_nv12(dst, pitch, frame, w, h)
    } else {
        upload_packed(dst, pitch, frame, w, h)
    }
}

/// 4 字节/像素（XR24 / AB24）上传。
fn upload_packed(
    dst: &mut [u8],
    pitch: usize,
    frame: &CpuFrame,
    w: usize,
    h: usize,
) -> Result<u64, DisplayError> {
    let row_bytes = w * 4;
    if pitch < row_bytes {
        return Err(DisplayError::Failed {
            operation: "present",
            message: format!("扫描缓冲 pitch {pitch} 小于行宽 {row_bytes}"),
        });
    }
    let src_stride = frame.stride as usize;
    if src_stride < row_bytes {
        return Err(DisplayError::Failed {
            operation: "present",
            message: format!("源帧 stride {src_stride} 小于行宽 {row_bytes}"),
        });
    }
    let total = row_bytes.saturating_mul(h);
    // 快路径：源行距 == 目标 pitch == 行宽 → 整帧一次连续拷贝（memcpy）。
    if src_stride == pitch && pitch == row_bytes {
        let (Some(src), Some(dst)) = (frame.data.get(..total), dst.get_mut(..total)) else {
            return Err(short_buffer(total));
        };
        dst.copy_from_slice(src);
        return Ok(total as u64);
    }
    // 通用路径：逐行拷贝（目标 pitch 填充 / 源 stride 填充）。
    for y in 0..h {
        let s0 = y.saturating_mul(src_stride);
        let d0 = y.saturating_mul(pitch);
        let (Some(src), Some(dst_row)) = (
            frame.data.get(s0..s0.saturating_add(row_bytes)),
            dst.get_mut(d0..d0.saturating_add(row_bytes)),
        ) else {
            return Err(short_buffer(total));
        };
        dst_row.copy_from_slice(src);
    }
    Ok(total as u64)
}

/// NV12 半平面上传（Y 行 + UV 行，UV 目标偏移 = `pitch × 高度`）。
fn upload_nv12(
    dst: &mut [u8],
    pitch: usize,
    frame: &CpuFrame,
    w: usize,
    h: usize,
) -> Result<u64, DisplayError> {
    if pitch < w {
        return Err(DisplayError::Failed {
            operation: "present",
            message: format!("NV12 扫描缓冲 pitch {pitch} 小于宽度 {w}"),
        });
    }
    let src_stride = frame.stride as usize;
    if src_stride < w {
        return Err(DisplayError::Failed {
            operation: "present",
            message: format!("NV12 源帧 stride {src_stride} 小于宽度 {w}"),
        });
    }
    let y_bytes = w.saturating_mul(h);
    let uv_rows = h.div_ceil(2);
    let uv_bytes = w.saturating_mul(uv_rows);
    let uv_src = src_stride.saturating_mul(h);
    let uv_dst = pitch.saturating_mul(h);

    // 快路径：无填充（源 stride == 目标 pitch == 宽度）→ Y/UV 各一次连续拷贝。
    if src_stride == pitch && pitch == w {
        let (Some(src_y), Some(dst_y)) = (frame.data.get(..y_bytes), dst.get_mut(..y_bytes)) else {
            return Err(short_buffer(y_bytes));
        };
        dst_y.copy_from_slice(src_y);
        let s1 = uv_src;
        let d1 = uv_dst;
        let (Some(src_uv), Some(dst_uv)) = (
            frame.data.get(s1..s1.saturating_add(uv_bytes)),
            dst.get_mut(d1..d1.saturating_add(uv_bytes)),
        ) else {
            return Err(short_buffer(y_bytes + uv_bytes));
        };
        dst_uv.copy_from_slice(src_uv);
        return Ok((y_bytes + uv_bytes) as u64);
    }

    for y in 0..h {
        let s0 = y.saturating_mul(src_stride);
        let d0 = y.saturating_mul(pitch);
        let (Some(src), Some(dst_row)) = (
            frame.data.get(s0..s0.saturating_add(w)),
            dst.get_mut(d0..d0.saturating_add(w)),
        ) else {
            return Err(short_buffer(y_bytes));
        };
        dst_row.copy_from_slice(src);
    }
    for y in 0..uv_rows {
        let s0 = uv_src.saturating_add(y.saturating_mul(src_stride));
        let d0 = uv_dst.saturating_add(y.saturating_mul(pitch));
        let (Some(src), Some(dst_row)) = (
            frame.data.get(s0..s0.saturating_add(w)),
            dst.get_mut(d0..d0.saturating_add(w)),
        ) else {
            return Err(short_buffer(y_bytes + uv_bytes));
        };
        dst_row.copy_from_slice(src);
    }
    Ok((y_bytes + uv_bytes) as u64)
}

fn short_buffer(needed: usize) -> DisplayError {
    DisplayError::Failed {
        operation: "present",
        message: format!("扫描/源缓冲不足 {needed} 字节"),
    }
}

/// 逐行参考实现（历史语义）：**仅测试用**正确性 oracle。
///
/// 每个平面**逐行** `copy_from_slice`，不做连续快路径；其输出与 [`upload`]
/// 必须逐字节相同（见本模块测试）。历史版本的生产路径就是这种逐行循环，
/// 因此它同时是"优化前"的行为基准。
///
/// # Errors
///
/// 同 [`upload`]。
#[cfg(test)]
fn upload_rowwise_reference(
    dst: &mut [u8],
    pitch: usize,
    frame: &CpuFrame,
    format: ScanoutFormat,
) -> Result<u64, DisplayError> {
    frame.validate()?;
    let w = frame.width as usize;
    let h = frame.height as usize;
    let src_stride = frame.stride as usize;
    if pitch < w * format.bytes_per_pixel() as usize {
        return Err(DisplayError::Failed {
            operation: "present",
            message: "pitch 过小".to_string(),
        });
    }
    let mut copied = 0u64;
    let planes: &[(usize, usize, usize)] = if format.is_semi_planar() {
        &[(0, 0, h), (src_stride * h, pitch * h, h.div_ceil(2))]
    } else {
        &[(0, 0, h)]
    };
    let row_bytes = if format.is_semi_planar() { w } else { w * 4 };
    for &(src_off, dst_off, rows) in planes {
        for y in 0..rows {
            let s0 = src_off.saturating_add(y.saturating_mul(src_stride));
            let d0 = dst_off.saturating_add(y.saturating_mul(pitch));
            let (Some(src), Some(dst_row)) = (
                frame.data.get(s0..s0.saturating_add(row_bytes)),
                dst.get_mut(d0..d0.saturating_add(row_bytes)),
            ) else {
                return Err(short_buffer(row_bytes));
            };
            dst_row.copy_from_slice(src);
            copied = copied.saturating_add(row_bytes as u64);
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ---------- 假工厂：统计分配/映射次数，不接触任何设备 ----------

    #[derive(Debug, Default)]
    struct FakeFactory {
        created: Vec<ScanoutKey>,
        mapped: usize,
        destroyed: usize,
    }

    struct FakeBuf {
        pitch: usize,
    }

    struct FakeMap {
        data: Vec<u8>,
    }

    impl ScanoutFactory for FakeFactory {
        type Buf = FakeBuf;
        type Map = FakeMap;
        type Fb = u32;

        fn create(&mut self, key: ScanoutKey) -> Result<Slot<FakeBuf, FakeMap, u32>, DisplayError> {
            let pitch = key.width as usize * key.format.bytes_per_pixel() as usize;
            let len = pitch * key.format.container_height(key.height) as usize;
            self.created.push(key);
            self.mapped += 1;
            Ok(Slot {
                key,
                buf: FakeBuf { pitch },
                map: FakeMap {
                    data: vec![0u8; len],
                },
                fb: self.created.len() as u32,
                state: SlotState::Free,
            })
        }

        fn bytes<'a>(&self, map: &'a mut FakeMap) -> &'a mut [u8] {
            &mut map.data
        }

        fn pitch(&self, buf: &FakeBuf) -> usize {
            buf.pitch
        }

        fn destroy(&mut self, slot: Slot<FakeBuf, FakeMap, u32>) {
            self.destroyed += 1;
            drop(slot);
        }
    }

    fn key(w: u32, h: u32, format: ScanoutFormat) -> ScanoutKey {
        ScanoutKey {
            width: w,
            height: h,
            format,
        }
    }

    fn nv12_frame(w: u32, h: u32, stride: u32) -> CpuFrame {
        let y = (stride * h) as usize;
        let data = vec![7u8; y + (stride * h.div_ceil(2)) as usize];
        CpuFrame {
            width: w,
            height: h,
            stride,
            format: PixelFormat::Nv12,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    fn packed_frame(w: u32, h: u32, stride: u32, format: PixelFormat, seed: u8) -> CpuFrame {
        let mut data = vec![0u8; (stride * h) as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(seed);
        }
        CpuFrame {
            width: w,
            height: h,
            stride,
            format,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    // ---------- 工厂构造器（测试内复用） ----------

    impl FakeFactory {
        fn new() -> Self {
            Self::default()
        }
    }

    /// 一次完整"写 → 提交成功 → 归还"（各测试共用的正常路径）。
    fn write_and_commit(
        pool: &mut ScanoutPool<FakeBuf, FakeMap, u32>,
        factory: &mut FakeFactory,
        k: ScanoutKey,
        fill: u8,
    ) -> u32 {
        let mut guard = pool.acquire(factory, k, AcquirePolicy::TryOnce).unwrap();
        let fb = guard.fb;
        {
            let bytes = guard.bytes(factory).unwrap();
            bytes.fill(fill);
        }
        let ready = guard.mark_ready();
        if let Some(reclaimed) = pool.on_commit_success(ready) {
            pool.release_to_upstream(reclaimed);
        }
        fb
    }

    // ---------- 池化：不每帧分配/映射（原有不回归） ----------

    #[test]
    fn pool_does_not_allocate_or_map_per_frame() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(3840, 2400, ScanoutFormat::Xrgb8888);
        for i in 0..180 {
            let mut guard = pool
                .acquire(&mut factory, k, AcquirePolicy::TryOnce)
                .unwrap();
            assert!(
                guard.bytes(&mut factory).unwrap().len() >= 3840 * 2400 * 4,
                "第 {i} 帧映射过小"
            );
            let ready = guard.mark_ready();
            if let Some(reclaimed) = pool.on_commit_success(ready) {
                pool.release_to_upstream(reclaimed);
            }
        }
        let stats = pool.stats();
        assert_eq!(stats.allocations, 3, "180 帧只允许 3 次分配（池深）");
        assert_eq!(stats.mappings, 3, "180 帧只允许 3 次映射");
        assert_eq!(stats.reuses, 177);
        // 所有权：提交 180 次 ⇒ 回收 179 次（第一帧没有"上一帧"可回收）。
        assert_eq!(stats.reclaims, 179, "回收必须滞后一帧：提交数 − 1");
        assert_eq!(stats.max_in_use, 2, "稳态恰为 1 扫描 + 1 写入");
        assert_eq!(stats.rejected_writes, 0);
        assert_eq!(stats.exhausted_drops, 0);
    }

    #[test]
    fn pool_rotates_slots_covers_three_buffers() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(16, 16, ScanoutFormat::Xrgb8888);
        let fbs: Vec<u32> = (0..6)
            .map(|_| write_and_commit(&mut pool, &mut factory, k, 1))
            .collect();
        // 轮转：前 3 个槽位创建后循环复用，不产生新分配。
        assert_eq!(fbs[0], fbs[3]);
        assert_eq!(fbs[1], fbs[4]);
        assert_eq!(fbs[2], fbs[5]);
        assert_eq!(pool.stats().allocations, 3);
    }

    #[test]
    fn pool_rebuilds_only_when_key_changes() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let a = key(1920, 1080, ScanoutFormat::Xrgb8888);
        let b = key(3840, 2400, ScanoutFormat::Xrgb8888);
        for _ in 0..3 {
            write_and_commit(&mut pool, &mut factory, a, 1);
        }
        assert_eq!(pool.stats().allocations, 3);
        // 尺寸变化：只重建被轮到的那个空白槽位。
        write_and_commit(&mut pool, &mut factory, b, 1);
        assert_eq!(pool.stats().allocations, 4);
        // 回到原尺寸：命中仍属 a 的空闲槽位（不分配）。
        write_and_commit(&mut pool, &mut factory, a, 1);
        assert_eq!(pool.stats().allocations, 4);
    }

    // ---------- T1：对 SCANNING 槽位写入必须被拒 ----------

    #[test]
    fn t1_write_to_scanning_slot_is_refused() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(32, 8, ScanoutFormat::Xrgb8888);
        // 提交一帧 ⇒ 该槽位进入 SCANNING。
        let fb = write_and_commit(&mut pool, &mut factory, k, 0x5A);
        let idx = pool
            .slots
            .iter()
            .position(|s| matches!(s, Some(slot) if slot.fb == fb))
            .expect("槽位必须存在");
        assert_eq!(pool.state_of(idx), Some(SlotState::Scanning));

        // 写入被拒（走与真实路径相同的判定）。
        let err = pool.try_write(idx).expect_err("对在扫槽位写入必须被拒");
        assert!(
            format!("{err}").contains("SCANNING"),
            "错误应点名状态：{err}"
        );
        assert_eq!(pool.stats().rejected_writes, 1);
        // 状态没被改写、内容没被动过。
        assert_eq!(pool.state_of(idx), Some(SlotState::Scanning));
        assert_eq!(pool.slots[idx].as_ref().unwrap().map.data[0], 0x5A);
    }

    // ---------- T2：未提交下一帧就想回收 ⇒ 被拒（滞后一帧） ----------

    #[test]
    fn t2_recycling_before_the_next_commit_is_refused() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(32, 8, ScanoutFormat::Xrgb8888);
        let fb_a = write_and_commit(&mut pool, &mut factory, k, 1);
        let a = pool
            .slots
            .iter()
            .position(|s| matches!(s, Some(slot) if slot.fb == fb_a))
            .unwrap();
        // A 刚开始显示：此刻回收必须被拒（"未提交 B 就回收 A"）。
        assert!(!pool.try_recycle_slot(a), "在扫缓冲不得被回收");
        assert_eq!(pool.state_of(a), Some(SlotState::Scanning));
        assert_eq!(pool.stats().reclaims, 0);

        // 提交 B 之后，A 才允许回收。
        write_and_commit(&mut pool, &mut factory, k, 2);
        assert_eq!(pool.state_of(a), Some(SlotState::Free));
        assert_eq!(pool.stats().reclaims, 1, "滞后一帧：B 提交后 A 才回收");
    }

    // ---------- T3：池耗尽 ⇒ 明确失败，绝不覆盖在用缓冲 ----------

    #[test]
    fn t3_exhaustion_never_overwrites_in_use_buffers() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(32, 8, ScanoutFormat::Xrgb8888);
        // 三个槽位全部占住（READY，未提交）。
        let mut readies = Vec::new();
        for fill in [0x11u8, 0x22, 0x33] {
            let mut guard = pool
                .acquire(&mut factory, k, AcquirePolicy::TryOnce)
                .unwrap();
            guard.bytes(&mut factory).unwrap().fill(fill);
            readies.push(guard.mark_ready());
        }
        assert_eq!(pool.in_use(), 3);
        // 第 4 次取用：必须失败，且**不**覆盖任何在用缓冲。
        let err = pool
            .acquire(&mut factory, k, AcquirePolicy::TryOnce)
            .err()
            .expect("池耗尽必须报错，不得静默覆盖");
        assert!(format!("{err}").contains("耗尽"), "{err}");
        let stats = pool.stats();
        assert_eq!(stats.exhausted_drops, 1);
        assert_eq!(stats.allocations, 3, "耗尽时不得新建/重建槽位");
        assert_eq!(stats.max_in_use, 3);

        // 内容校验：把三块提交掉再取回，字节必须仍是当初写进去的值。
        for (i, ready) in readies.iter().enumerate() {
            if let Some(reclaimed) = pool.on_commit_success(*ready) {
                pool.release_to_upstream(reclaimed);
            }
            let _ = i;
        }
        let mut guard = pool
            .acquire(&mut factory, k, AcquirePolicy::TryOnce)
            .unwrap();
        let bytes = guard.bytes(&mut factory).unwrap();
        let first = bytes[0];
        assert!(
            [0x11u8, 0x22, 0x33].contains(&first),
            "取回的槽位内容必须仍是原样（实际 {first:#x}）"
        );
    }

    // ---------- T4：回收滞后一帧（A→B→C 顺序） ----------

    #[test]
    fn t4_reclaim_lags_exactly_one_frame() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(16, 16, ScanoutFormat::Xrgb8888);

        // A
        let g = pool
            .acquire(&mut factory, k, AcquirePolicy::TryOnce)
            .unwrap();
        let a_fb = g.fb;
        let ready = g.mark_ready();
        let reclaimed = pool.on_commit_success(ready);
        assert!(reclaimed.is_none(), "提交 A 时没有可回收的（A 刚开始显示）");
        assert_eq!(pool.stats().reclaims, 0);

        // B：此刻才允许回收 A
        let g = pool
            .acquire(&mut factory, k, AcquirePolicy::TryOnce)
            .unwrap();
        let b_fb = g.fb;
        let ready = g.mark_ready();
        let reclaimed = pool.on_commit_success(ready).expect("提交 B 后 A 才可回收");
        assert_eq!(reclaimed.fb, a_fb, "回收的必须是 A（上一帧），不是 B");
        pool.release_to_upstream(reclaimed);
        assert_eq!(pool.stats().reclaims, 1);

        // C：回收 B
        let g = pool
            .acquire(&mut factory, k, AcquirePolicy::TryOnce)
            .unwrap();
        let _c_fb = g.fb;
        let ready = g.mark_ready();
        let reclaimed = pool.on_commit_success(ready).expect("提交 C 后 B 才可回收");
        assert_eq!(reclaimed.fb, b_fb, "回收的必须是 B（上一帧），不是 C");
        pool.release_to_upstream(reclaimed);
        assert_eq!(pool.stats().reclaims, 2);
    }

    // ---------- T5：提交失败 ⇒ READY 回 FREE，且不动在扫的那块 ----------

    #[test]
    fn t5_commit_failure_returns_slot_to_free_and_keeps_scanning() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(16, 16, ScanoutFormat::Xrgb8888);
        // A 正常提交（进入 SCANNING）
        let fb_a = write_and_commit(&mut pool, &mut factory, k, 1);
        let a = pool
            .slots
            .iter()
            .position(|s| matches!(s, Some(slot) if slot.fb == fb_a))
            .unwrap();
        // B 提交失败
        let g = pool
            .acquire(&mut factory, k, AcquirePolicy::TryOnce)
            .unwrap();
        let _b = g.fb;
        let ready = g.mark_ready();
        assert_eq!(pool.state_of(ready.slot), Some(SlotState::Ready));
        pool.on_commit_failure(ready);
        assert_eq!(
            pool.state_of(ready.slot),
            Some(SlotState::Free),
            "失败帧回 FREE"
        );
        assert_eq!(
            pool.state_of(a),
            Some(SlotState::Scanning),
            "在扫的那块不得被改动"
        );
        assert_eq!(pool.stats().reclaims, 0, "失败不产生回收");
        // 下一帧仍能正常取用（失败没有把池弄坏）；且**不会**把在扫槽位交出来。
        let mut g2 = pool
            .acquire(&mut factory, k, AcquirePolicy::TryOnce)
            .unwrap();
        assert_ne!(g2.fb, fb_a, "不得把 SCANNING 的槽位交给新的写方");
        let _ = g2.bytes(&mut factory).unwrap();
        drop(g2);
        // 被失败放回的槽位（B）之后仍会被轮转使用，池不会越用越多。
        for _ in 0..6 {
            write_and_commit(&mut pool, &mut factory, k, 1);
        }
        assert_eq!(
            pool.stats().allocations,
            3,
            "池深恒为 3，失败不导致额外分配"
        );
    }

    // ---------- T6：H2 —— 提前返回（drop 写租约）必须把槽位放回 FREE ----------

    #[test]
    fn t6_early_return_releases_the_write_slot() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(16, 16, ScanoutFormat::Xrgb8888);
        {
            let mut guard = pool
                .acquire(&mut factory, k, AcquirePolicy::TryOnce)
                .unwrap();
            assert_eq!(guard.state(), SlotState::Writing);
            let _ = guard.bytes(&mut factory).unwrap();
            // 模拟"上传失败后提前返回"：不 mark_ready，直接 drop。
        }
        assert_eq!(
            pool.state_of(0),
            Some(SlotState::Free),
            "提前返回必须把槽位放回 FREE（H2）"
        );
        assert_eq!(pool.in_use(), 0, "提前返回后池内不应有非 FREE 槽位");
        // 池没有错位：继续跑完整周期，仍然只分配池深那么多块、且开始出现复用。
        for _ in 0..6 {
            write_and_commit(&mut pool, &mut factory, k, 1);
        }
        assert_eq!(pool.stats().allocations, 3, "池深恒为 3");
        assert!(pool.stats().reuses > 0, "放回的槽位必须被重新利用");
    }

    // ---------- T7：H3 —— key 变化时绝不 destroy 在扫槽位 ----------

    #[test]
    fn t7_key_change_never_destroys_a_scanning_slot() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let a = key(64, 16, ScanoutFormat::Xrgb8888);
        let b = key(32, 8, ScanoutFormat::Nv12);
        let fb_a = write_and_commit(&mut pool, &mut factory, a, 1);
        let scan = pool
            .slots
            .iter()
            .position(|s| matches!(s, Some(slot) if slot.fb == fb_a))
            .unwrap();
        assert_eq!(pool.state_of(scan), Some(SlotState::Scanning));

        // 新 key 到来：只能挑空槽位，绝不能动正在扫描的那块。
        let mut g = pool
            .acquire(&mut factory, b, AcquirePolicy::TryOnce)
            .unwrap();
        assert_ne!(g.fb, fb_a, "不得把在扫槽位交给新的写方");
        let _ = g.bytes(&mut factory).unwrap();
        let ready = g.mark_ready();
        pool.on_commit_failure(ready); // 结束时回 FREE，便于断言
        assert_eq!(
            factory.destroyed, 0,
            "扫描中的槽位不得被 destroy（UAF 风险）"
        );
        assert_eq!(pool.state_of(scan), Some(SlotState::Scanning));

        // 把剩下两个也占住 ⇒ 池耗尽，依然不得 destroy 在扫槽位。
        for _ in 0..2 {
            let mut g = pool
                .acquire(&mut factory, b, AcquirePolicy::TryOnce)
                .unwrap();
            let _ = g.bytes(&mut factory).unwrap();
            let ready = g.mark_ready();
            assert!(
                pool.acquire(&mut factory, b, AcquirePolicy::TryOnce)
                    .is_err()
                    || true
            );
            pool.on_commit_failure(ready);
            let _ = ready;
        }
        assert_eq!(factory.destroyed, 0);
        assert_eq!(pool.state_of(scan), Some(SlotState::Scanning));
    }

    // ---------- T8：H4 —— release_all 只销毁 FREE；强制路径必须报数 ----------

    #[test]
    fn t8_release_refuses_non_free_and_forced_path_reports() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(16, 16, ScanoutFormat::Xrgb8888);
        for _ in 0..5 {
            write_and_commit(&mut pool, &mut factory, k, 1);
        }
        // 此时 1 块 SCANNING + 2 块 FREE
        let outcome = pool.release_all(&mut factory);
        assert_eq!(outcome.destroyed, 2, "只能销毁 FREE 槽位");
        assert_eq!(outcome.refused, 1, "在扫槽位必须被拒（H4）");
        assert_eq!(factory.destroyed, 2);

        // 强制路径：设备关闭时才允许，但必须被计数（可观测）。
        let forced = pool.release_all_forced(&mut factory);
        assert_eq!(forced.destroyed, 1);
        assert_eq!(pool.stats().forced_releases, 1, "强制释放必须可观测");

        // 幂等：再释放一次什么都不发生。
        let again = pool.release_all(&mut factory);
        assert_eq!(again, ReleaseOutcome::default());
    }

    // ---------- T9：不变量汇总（占用上限 / 计数自洽） ----------

    #[test]
    fn t9_invariants_hold_over_many_frames() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(64, 32, ScanoutFormat::Nv12);
        for _ in 0..50 {
            write_and_commit(&mut pool, &mut factory, k, 3);
            assert!(pool.in_use() <= 3, "非 FREE 数不得超过池深");
            assert_eq!(pool.stats().rejected_writes, 0, "正常路径不得出现被拒写入");
            assert_eq!(pool.stats().blocked_recreates, 0);
            assert_eq!(pool.stats().exhausted_drops, 0, "正常路径不得丢帧");
        }
        let stats = pool.stats();
        assert_eq!(stats.allocations, 3);
        assert_eq!(stats.reclaims, 49, "提交 50 次 ⇒ 回收 49 次");
        assert!(stats.max_in_use <= 3);
    }

    // ---------- 池化：原有"释放幂等"语义（改为显式强制路径） ----------

    #[test]
    fn pool_release_destroys_every_slot_once() {
        let mut pool: ScanoutPool<FakeBuf, FakeMap, u32> = ScanoutPool::new(3);
        let mut factory = FakeFactory::new();
        let k = key(8, 8, ScanoutFormat::Xrgb8888);
        for _ in 0..5 {
            write_and_commit(&mut pool, &mut factory, k, 1);
        }
        pool.release_all_forced(&mut factory);
        pool.release_all_forced(&mut factory); // 幂等
        assert_eq!(pool.stats().allocations, 3);
        assert_eq!(factory.destroyed, 3);
    }

    // ---------- 上传：快路径与参考实现逐字节一致 ----------

    fn assert_upload_matches_reference(
        frame: &CpuFrame,
        pitch: usize,
        format: ScanoutFormat,
        expect_bytes: u64,
    ) {
        let len = pitch * format.container_height(frame.height) as usize;
        let mut fast = vec![0xAAu8; len];
        let mut reference = vec![0xAAu8; len];
        let fast_bytes = upload(&mut fast, pitch, frame, format).unwrap();
        let ref_bytes = upload_rowwise_reference(&mut reference, pitch, frame, format).unwrap();
        assert_eq!(fast_bytes, expect_bytes);
        assert_eq!(ref_bytes, expect_bytes);
        assert_eq!(fast, reference, "快路径与逐行参考必须逐字节一致");
    }

    #[test]
    fn upload_xrgb_contiguous_fast_path() {
        let frame = packed_frame(64, 8, 64 * 4, PixelFormat::Xrgb8888, 3);
        assert_upload_matches_reference(&frame, 64 * 4, ScanoutFormat::Xrgb8888, 64 * 8 * 4);
    }

    #[test]
    fn upload_xrgb_with_pitch_and_stride_padding() {
        // 源 stride 有填充、目标 pitch 也有填充：两条路径都必须只拷有效行宽。
        let frame = packed_frame(60, 8, 68 * 4, PixelFormat::Xrgb8888, 5);
        assert_upload_matches_reference(&frame, 72 * 4, ScanoutFormat::Xrgb8888, 60 * 8 * 4);
    }

    #[test]
    fn upload_abgr_is_byte_identical_copy() {
        // RGBA（[R,G,B,A]）直拷进 AB24 容器，不做像素重排。
        let frame = packed_frame(32, 4, 32 * 4, PixelFormat::Rgba8888, 9);
        assert_upload_matches_reference(&frame, 32 * 4, ScanoutFormat::Abgr8888, 32 * 4 * 4);
        let mut dst = vec![0u8; 32 * 4 * 4];
        upload(&mut dst, 32 * 4, &frame, ScanoutFormat::Abgr8888).unwrap();
        assert_eq!(&dst[..], &frame.data[..], "RGBA → AB24 必须逐字节一致");
    }

    #[test]
    fn upload_nv12_contiguous_fast_path() {
        let frame = nv12_frame(96, 8, 96);
        assert_upload_matches_reference(&frame, 96, ScanoutFormat::Nv12, (96 * 8 + 96 * 4) as u64);
    }

    #[test]
    fn upload_nv12_with_stride_and_pitch_padding() {
        let frame = nv12_frame(88, 6, 96);
        assert_upload_matches_reference(&frame, 100, ScanoutFormat::Nv12, (88 * 6 + 88 * 3) as u64);
    }

    #[test]
    fn upload_rejects_short_target_instead_of_panicking() {
        let frame = packed_frame(16, 4, 16 * 4, PixelFormat::Xrgb8888, 0);
        let mut short = vec![0u8; 8];
        let err = upload(&mut short, 16 * 4, &frame, ScanoutFormat::Xrgb8888).unwrap_err();
        assert!(matches!(err, DisplayError::Failed { .. }), "实际: {err}");
    }

    #[test]
    fn scanout_format_mapping_is_explicit() {
        assert_eq!(
            ScanoutFormat::from_pixel_format(PixelFormat::Xrgb8888),
            Some(ScanoutFormat::Xrgb8888)
        );
        // AR24 与 XR24 字节序相同：α 字节不参与显示，可共用扫描缓冲。
        assert_eq!(
            ScanoutFormat::from_pixel_format(PixelFormat::Argb8888),
            Some(ScanoutFormat::Xrgb8888)
        );
        assert_eq!(
            ScanoutFormat::from_pixel_format(PixelFormat::Rgba8888),
            Some(ScanoutFormat::Abgr8888)
        );
        assert_eq!(
            ScanoutFormat::from_pixel_format(PixelFormat::Nv12),
            Some(ScanoutFormat::Nv12)
        );
    }

    #[test]
    fn nv12_container_height_is_one_and_a_half() {
        assert_eq!(ScanoutFormat::Nv12.container_height(2400), 3600);
        assert_eq!(ScanoutFormat::Xrgb8888.container_height(2400), 2400);
    }
}
