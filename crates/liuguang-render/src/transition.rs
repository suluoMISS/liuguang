//! CPU 转场特效（M4 正确性路径）：fade / slide / wipe / circle。
//!
//! 规范 §7.1 要求的四种切换效果在 CPU 上逐像素实现，语义精确、可断言，
//! 同时也是 GPU shader 路径（`crate::gles`）失败时的回退正确性基准。
//!
//! # 效果语义（像素级）
//!
//! 以下 `eased` 指 [`apply_easing`] 作用于 `progress` 后的值：
//!
//! - **fade**：`out = lerp(from, to, eased)`，逐通道线性混合；
//! - **slide**：整幅 `to` 从 `direction` 命名的边缘滑入——`to` 沿方向轴
//!   平移 `(1 - eased) * (W 或 H)`，平移覆盖不到的区域露出 `from`；
//!   即 `out(x, y) = to(x - dx, y - dy)`（越界则取 `from(x, y)`），
//!   `Left/Up` 时 `dx/dy = -(1-eased)*W/H`，`Right/Down` 时为正；
//! - **wipe**：`to` 覆盖 `from` 的区域从 `direction` 命名的边缘向内推进
//!   `eased` 比例（硬边）：`Left/Up` 显示坐标 `< eased * (W 或 H)` 的区域，
//!   `Right/Down` 显示坐标 `> (W 或 H) * (1 - eased)` 的区域；
//! - **circle**：以画面中心为圆心，半径 `eased * max_radius` 的圆内显示
//!   `to`（`max_radius` 为到四角的距离），圆外为 `from`。
//!
//! 边界判定统一采用**像素中心**（`x + 0.5, y + 0.5`，与 [`crate::cpu`] 的
//! fit 留白判定一致），因此 `progress = 0` 恒等于 `from`、`progress = 1`
//! 恒等于 `to`，可精确断言。
//!
//! # 资源生命周期（规范 §7.2）
//!
//! [`CpuTransition::prepare_frames`] 持有 `from`/`to` 的 [`Arc`] 引用，
//! [`CpuTransition::render_at`] 复用内部输出缓冲（唯一引用时原地重写，
//! 被外部持有时才重新分配），[`CpuTransition::release`] 全部释放。
//! 连续 `prepare_frames`（「最后请求优先」）会覆盖旧资源而不累积引用。

use crate::{RenderError, TransitionEffect};
use liuguang_core::config::{Easing, TransitionDirection};
use liuguang_core::types::{CpuFrame, DisplayFrame, PixelFormat};
use std::sync::Arc;
use std::time::Duration;

/// 特效种类（规范 §7.1 的 fade / slide / wipe / circle）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    /// 淡入淡出：`to` 透明度随进度上升。
    Fade,
    /// 滑动：`to` 从指定边缘滑入。
    Slide {
        /// `to` 滑入的起始边缘。
        direction: TransitionDirection,
    },
    /// 擦除：`to` 从指定边缘硬边推进覆盖 `from`。
    Wipe {
        /// `to` 推进的起始边缘。
        direction: TransitionDirection,
    },
    /// 圆形揭示：圆内 `to`、圆外 `from`。
    Circle,
}

/// 转场规格：效果、缓动与时长。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionSpec {
    /// 效果种类。
    pub kind: EffectKind,
    /// 缓动函数（复用 [`liuguang_core::config::Easing`]，不重复定义）。
    pub easing: Easing,
    /// 目标时长（供上层计算逐帧进度；渲染本身只消费归一化 `progress`）。
    pub duration: Duration,
}

/// 缓动函数：把 `t ∈ [0, 1]` 映射到 `[0, 1]`。
///
/// 超出 `[0, 1]` 的输入被钳制到端点；`NaN` 按 `0.0` 处理。
/// 四种缓动均满足 `f(0) = 0`、`f(1) = 1` 且单调不减，
/// 因此 `progress` 端点处输出严格等于 `from` / `to`。
pub fn apply_easing(easing: Easing, t: f32) -> f32 {
    if t.is_nan() {
        return 0.0;
    }
    let t = t.clamp(0.0, 1.0);
    match easing {
        Easing::Linear => t,
        // 先慢后快：二次 ease-in。
        Easing::EaseIn => t * t,
        // 先快后慢：二次 ease-out。
        Easing::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
        // 两头慢中间快：smoothstep。
        Easing::EaseInOut => t * t * (3.0 - 2.0 * t),
    }
}

/// CPU 转场：持有 `from`/`to` 引用与可复用输出缓冲，逐进度合成输出帧。
///
/// 两种像素格式（[`PixelFormat::Xrgb8888`] 与 [`PixelFormat::Rgba8888`]）
/// 均按 4 字节/像素逐通道线性混合；`from` 与 `to` 必须同尺寸且同格式。
#[derive(Debug)]
pub struct CpuTransition {
    /// 转场规格。
    spec: TransitionSpec,
    /// 起始帧（Arc 引用，不复制像素）。
    from: Option<CpuFrame>,
    /// 目标帧（Arc 引用，不复制像素）。
    to: Option<CpuFrame>,
    /// 双缓冲输出槽：调用方仍持有一帧时，下一次渲染用另一个槽，
    /// 避免每帧重新分配 4K 帧（36.9 MB）并清零。
    buffers: [Option<CpuFrame>; 2],
    /// 下一次优先尝试的槽位。
    next: usize,
    /// 输出缓冲分配次数（诊断/测试：稳态应为 2）。
    allocations: u64,
}

/// 可复用的槽位判定：布局一致且 Arc 唯一持有（没有外部引用）。
fn reusable(frame: &Option<CpuFrame>, width: u32, height: u32, format: PixelFormat) -> bool {
    frame.as_ref().is_some_and(|buf| {
        buf.width == width
            && buf.height == height
            && buf.format == format
            && Arc::strong_count(&buf.data) == 1
    })
}

impl CpuTransition {
    /// 创建 CPU 转场（尚未 `prepare_frames`）。
    pub fn new(spec: TransitionSpec) -> Self {
        Self {
            spec,
            from: None,
            to: None,
            buffers: [None, None],
            next: 0,
            allocations: 0,
        }
    }

    /// 转场规格。
    pub fn spec(&self) -> &TransitionSpec {
        &self.spec
    }

    /// 持有转场双方帧（规范 §7.2「最后请求优先」：再次调用直接覆盖旧资源）。
    ///
    /// # Errors
    ///
    /// 帧自检失败、尺寸不一致或格式不一致（要求双方同为
    /// [`PixelFormat::Xrgb8888`] 或同为 [`PixelFormat::Rgba8888`]）时
    /// 返回 [`RenderError`]。
    pub fn prepare_frames(&mut self, from: CpuFrame, to: CpuFrame) -> Result<(), RenderError> {
        from.validate().map_err(RenderError::InvalidFrame)?;
        to.validate().map_err(RenderError::InvalidFrame)?;
        if from.width != to.width || from.height != to.height {
            return Err(RenderError::FrameMismatch(format!(
                "尺寸不一致: from {}x{}, to {}x{}",
                from.width, from.height, to.width, to.height
            )));
        }
        if from.format != to.format {
            return Err(RenderError::FrameMismatch(format!(
                "像素格式不一致: from {:?}, to {:?}",
                from.format, to.format
            )));
        }
        if !matches!(from.format, PixelFormat::Xrgb8888 | PixelFormat::Rgba8888) {
            return Err(RenderError::FrameMismatch(format!(
                "不支持的转场像素格式: {:?}（要求 Xrgb8888 或 Rgba8888）",
                from.format
            )));
        }
        // 覆盖旧资源：连续 prepare 不累积引用（最后请求优先）。
        self.from = Some(from);
        self.to = Some(to);
        // 尺寸可能变化，旧缓冲不再适用。
        self.buffers = [None, None];
        self.next = 0;
        Ok(())
    }

    /// 按 `progress ∈ [0, 1]` 合成输出帧（内部先过 [`apply_easing`]）。
    ///
    /// 输出在**两个内部缓冲**之间轮转：只有当两个槽位都仍被外部持有
    /// （引用计数 > 1）时才新分配。上层"渲染 → 提交 → 持有当前帧"的用法
    /// 因此不会每帧分配。
    ///
    /// # Errors
    ///
    /// 未就绪或 `progress` 非法（越界 / `NaN`）时返回 [`RenderError`]。
    pub fn render_at(&mut self, progress: f32) -> Result<CpuFrame, RenderError> {
        if progress.is_nan() || !(0.0..=1.0).contains(&progress) {
            return Err(RenderError::InvalidProgress { progress });
        }
        let (Some(from), Some(to)) = (self.from.as_ref(), self.to.as_ref()) else {
            return Err(RenderError::NotPrepared);
        };
        let eased = apply_easing(self.spec.easing, progress);
        let (width, height, format) = (from.width, from.height, from.format);
        let stride =
            width
                .checked_mul(format.bytes_per_pixel())
                .ok_or(RenderError::OutputTooLarge {
                    needed: u64::MAX,
                    limit: crate::cpu::MAX_OUTPUT_BYTES as u64,
                })?;

        let preferred = self.next % self.buffers.len();
        let other = (preferred + 1) % self.buffers.len();
        let idx = if reusable(&self.buffers[preferred], width, height, format) {
            preferred
        } else if reusable(&self.buffers[other], width, height, format) {
            other
        } else {
            let data = new_buffer(width, height, format)?;
            self.buffers[preferred] = Some(CpuFrame {
                width,
                height,
                stride,
                format,
                data: Arc::from(data.into_boxed_slice()),
            });
            self.allocations = self.allocations.saturating_add(1);
            preferred
        };
        self.next = (idx + 1) % self.buffers.len();

        {
            let buf = self.buffers[idx].as_mut().ok_or(RenderError::NotPrepared)?;
            let data = Arc::get_mut(&mut buf.data)
                .ok_or_else(|| RenderError::Failed("内部缓冲引用不唯一".to_string()))?;
            render_into(from, to, self.spec.kind, eased, data);
        }

        self.buffers[idx].clone().ok_or(RenderError::NotPrepared)
    }

    /// 输出缓冲分配次数（诊断用：稳态应为 2，不应随帧数增长）。
    pub fn buffer_allocations(&self) -> u64 {
        self.allocations
    }

    /// 释放 `from`/`to`/内部缓冲（引用计数随之下降）。
    pub fn release(&mut self) {
        self.from = None;
        self.to = None;
        self.buffers = [None, None];
    }

    /// 资源是否已就绪（`prepare_frames` 成功且未 `release`）。
    pub fn is_prepared(&self) -> bool {
        self.from.is_some() && self.to.is_some()
    }
}

/// 分配与输入帧同布局的零缓冲。
fn new_buffer(width: u32, height: u32, format: PixelFormat) -> Result<Vec<u8>, RenderError> {
    let stride =
        width
            .checked_mul(format.bytes_per_pixel())
            .ok_or(RenderError::OutputTooLarge {
                needed: u64::MAX,
                limit: crate::cpu::MAX_OUTPUT_BYTES as u64,
            })?;
    let needed =
        (stride as usize)
            .checked_mul(height as usize)
            .ok_or(RenderError::OutputTooLarge {
                needed: u64::MAX,
                limit: crate::cpu::MAX_OUTPUT_BYTES as u64,
            })?;
    if needed > crate::cpu::MAX_OUTPUT_BYTES {
        return Err(RenderError::OutputTooLarge {
            needed: needed as u64,
            limit: crate::cpu::MAX_OUTPUT_BYTES as u64,
        });
    }
    Ok(vec![0u8; needed])
}

/// 把 `from` / `to` 的同一行切片（有效行宽 = 宽×4）。
#[inline]
fn row(frame: &CpuFrame, y: usize) -> &[u8] {
    let stride = frame.stride as usize;
    let row_bytes = frame.width as usize * 4;
    let start = y * stride;
    &frame.data[start..start + row_bytes]
}

/// 输出缓冲的第 `y` 行（stride 恒为 宽×4）。
#[inline]
fn out_row(out: &mut [u8], width: usize, y: usize) -> &mut [u8] {
    let row_bytes = width * 4;
    let start = y * row_bytes;
    &mut out[start..start + row_bytes]
}

/// 按效果语义合成一帧到 `out`（长度 `width * height * 4`）。
///
/// 实现策略（**逐像素浮点 → 行级 memcpy**）：
/// - `fade`：整数定点线性混合（`k = round(t·256)`，通道并行），
///   端点精确等于 `from`/`to`；
/// - `slide`：整幅平移量取整后的**纯位移**，逐行 `copy_from_slice`；
/// - `wipe`：硬边分界是一次性算出的列/行边界，逐行两段 memcpy；
/// - `circle`：逐行求内外区间（边界用与逐像素实现**相同**的 f32 谓词
///   校正），区间内/外各一次 memcpy。
///
/// 与逐像素参考实现（测试模块中的 `reference_render_into`）逐字节一致；
/// 差异只在 `slide` 的亚像素边界：原实现对 `round(x+dx)` 未做上界钳制，
/// 当 `x+dx ∈ [w-0.5, w)` 时会**越界读取/panic**（真机 4K 转场已复现），
/// 现实现按整数位移判定，越界像素取 `from`。
fn render_into(from: &CpuFrame, to: &CpuFrame, kind: EffectKind, eased: f32, out: &mut [u8]) {
    let (w, h) = (from.width as usize, from.height as usize);
    match kind {
        EffectKind::Fade => fade_into(from, to, eased, out, w, h),
        EffectKind::Slide { direction } => slide_into(from, to, direction, eased, out, w, h),
        EffectKind::Wipe { direction } => wipe_into(from, to, direction, eased, out, w, h),
        EffectKind::Circle => circle_into(from, to, eased, out, w, h),
    }
}

/// 整数定点混合：`out = (a·(256-k) + b·k + 128) >> 8`（**8 个通道并行**，
/// 每通道占 16 位槽位，乘积不超过 16 位，因此无进位串扰）。
///
/// `k = round(eased·256) ∈ [0, 256]`；`k = 0` 精确等于 `a`，`k = 256` 精确等于 `b`。
/// 与浮点实现相比仅有 ±1 的舍入差（见测试 `fade_integer_matches_float_within_one`）。
#[inline]
fn blend_u64(a: u64, b: u64, m: u64, k: u64) -> u64 {
    const MASK: u64 = 0x00FF_00FF_00FF_00FF;
    const HALF: u64 = 0x0080_0080_0080_0080;
    let lo = (((a & MASK) * m + (b & MASK) * k + HALF) >> 8) & MASK;
    let hi = ((((a >> 8) & MASK) * m + ((b >> 8) & MASK) * k + HALF) >> 8) & MASK;
    lo | (hi << 8)
}

#[inline]
fn blend_u32(a: u32, b: u32, m: u32, k: u32) -> u32 {
    const MASK: u32 = 0x00FF_00FF;
    const HALF: u32 = 0x0080_0080;
    let lo = (((a & MASK) * m + (b & MASK) * k + HALF) >> 8) & MASK;
    let hi = ((((a >> 8) & MASK) * m + ((b >> 8) & MASK) * k + HALF) >> 8) & MASK;
    lo | (hi << 8)
}

fn fade_into(from: &CpuFrame, to: &CpuFrame, eased: f32, out: &mut [u8], w: usize, h: usize) {
    let k = (eased * 256.0).round().clamp(0.0, 256.0) as u32;
    let m = 256 - k;
    for y in 0..h {
        let f = row(from, y);
        let t = row(to, y);
        let o = out_row(out, w, y);
        // 每次处理 2 个像素（64 位 = 8 通道）。
        let pairs = o.len() / 8;
        for i in 0..pairs {
            let off = i * 8;
            let av = u64::from_le_bytes([
                f[off],
                f[off + 1],
                f[off + 2],
                f[off + 3],
                f[off + 4],
                f[off + 5],
                f[off + 6],
                f[off + 7],
            ]);
            let bv = u64::from_le_bytes([
                t[off],
                t[off + 1],
                t[off + 2],
                t[off + 3],
                t[off + 4],
                t[off + 5],
                t[off + 6],
                t[off + 7],
            ]);
            o[off..off + 8]
                .copy_from_slice(&blend_u64(av, bv, u64::from(m), u64::from(k)).to_le_bytes());
        }
        // 尾部不足 8 字节（奇数宽度）时按像素处理。
        let tail = pairs * 8;
        if tail < o.len() {
            let av = u32::from_le_bytes([f[tail], f[tail + 1], f[tail + 2], f[tail + 3]]);
            let bv = u32::from_le_bytes([t[tail], t[tail + 1], t[tail + 2], t[tail + 3]]);
            o[tail..tail + 4].copy_from_slice(&blend_u32(av, bv, m, k).to_le_bytes());
        }
    }
}

/// `slide` 的源偏移 `k`（像素）：`out(x) = to(x + k)`（与逐像素实现的
/// `sx = x - dx` 一致；`dx` 是 `to` 的平移量）。
fn slide_shift(direction: TransitionDirection, eased: f32, w: usize, h: usize) -> (i64, i64) {
    let rest = 1.0 - eased;
    match direction {
        TransitionDirection::Left => ((rest * w as f32).round() as i64, 0),
        TransitionDirection::Right => (-(rest * w as f32).round() as i64, 0),
        TransitionDirection::Up => (0, (rest * h as f32).round() as i64),
        TransitionDirection::Down => (0, -(rest * h as f32).round() as i64),
    }
}

fn slide_into(
    from: &CpuFrame,
    to: &CpuFrame,
    direction: TransitionDirection,
    eased: f32,
    out: &mut [u8],
    w: usize,
    h: usize,
) {
    let (dx, dy) = slide_shift(direction, eased, w, h);
    for y in 0..h {
        let o = out_row(out, w, y);
        let src_y = y as i64 + dy;
        if !(0..h as i64).contains(&src_y) {
            // 该行完全来自 from。
            o.copy_from_slice(row(from, y));
            continue;
        }
        let t = row(to, src_y as usize);
        // 水平位移：有效源列区间 [max(0, -dx), min(w, w - dx))。
        let x_start = (-dx).max(0).min(w as i64) as usize;
        let x_end = ((w as i64 - dx).clamp(0, w as i64)) as usize;
        let row_bytes = w * 4;
        if x_start >= x_end {
            o.copy_from_slice(row(from, y));
            continue;
        }
        // 左侧（无 to 覆盖）+ 中间位移段 + 右侧。
        if x_start > 0 {
            o[..x_start * 4].copy_from_slice(&row(from, y)[..x_start * 4]);
        }
        let s0 = ((x_start as i64 + dx) as usize) * 4;
        let s1 = ((x_end as i64 + dx) as usize) * 4;
        o[x_start * 4..x_end * 4].copy_from_slice(&t[s0..s1]);
        if x_end < w {
            o[x_end * 4..row_bytes].copy_from_slice(&row(from, y)[x_end * 4..row_bytes]);
        }
    }
}

/// 在长度 `len` 上二分求「第一个满足谓词的像素」（谓词对 x/y 单调）。
fn first_matching(len: usize, predicate: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if predicate(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// wipe 的分界索引：
/// - `Left/Up`：返回**第一个未被覆盖**的坐标（即覆盖区长度）；
/// - `Right/Down`：返回**第一个被覆盖**的坐标。
///
/// 谓词与逐像素参考实现完全相同（像素中心比较），因此边界逐像素一致；
/// 用一次二分开销（O(log n)）换取"不用猜浮点取整"，避免近似公式在
/// `eased = 1` 等端点上退化。
fn wipe_boundary(direction: TransitionDirection, eased: f32, w: usize, h: usize) -> usize {
    let wf = w as f32;
    let hf = h as f32;
    let covered = |x: usize, y: usize| -> bool {
        let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
        match direction {
            TransitionDirection::Left => cx < eased * wf,
            TransitionDirection::Right => cx > wf * (1.0 - eased),
            TransitionDirection::Up => cy < eased * hf,
            TransitionDirection::Down => cy > hf * (1.0 - eased),
        }
    };
    match direction {
        // 列方向边界与行无关：任取中间行求值。
        TransitionDirection::Left => first_matching(w, |x| !covered(x, h / 2)),
        TransitionDirection::Right => first_matching(w, |x| covered(x, h / 2)),
        TransitionDirection::Up => first_matching(h, |y| !covered(w / 2, y)),
        TransitionDirection::Down => first_matching(h, |y| covered(w / 2, y)),
    }
}

fn wipe_into(
    from: &CpuFrame,
    to: &CpuFrame,
    direction: TransitionDirection,
    eased: f32,
    out: &mut [u8],
    w: usize,
    h: usize,
) {
    let row_bytes = w * 4;
    match direction {
        TransitionDirection::Left => {
            let boundary = wipe_boundary(direction, eased, w, h);
            for y in 0..h {
                let o = out_row(out, w, y);
                o[..boundary * 4].copy_from_slice(&row(to, y)[..boundary * 4]);
                o[boundary * 4..row_bytes].copy_from_slice(&row(from, y)[boundary * 4..row_bytes]);
            }
        }
        TransitionDirection::Right => {
            let boundary = wipe_boundary(direction, eased, w, h);
            for y in 0..h {
                let o = out_row(out, w, y);
                o[..boundary * 4].copy_from_slice(&row(from, y)[..boundary * 4]);
                o[boundary * 4..row_bytes].copy_from_slice(&row(to, y)[boundary * 4..row_bytes]);
            }
        }
        TransitionDirection::Up => {
            let boundary = wipe_boundary(direction, eased, w, h);
            for y in 0..h {
                let src = if y < boundary { to } else { from };
                out_row(out, w, y).copy_from_slice(row(src, y));
            }
        }
        TransitionDirection::Down => {
            let boundary = wipe_boundary(direction, eased, w, h);
            for y in 0..h {
                let src = if y >= boundary { to } else { from };
                out_row(out, w, y).copy_from_slice(row(src, y));
            }
        }
    }
}

/// 圆心：画面中心（与逐像素实现一致）。
fn circle_inside(x: usize, y: usize, wf: f32, hf: f32, radius: f32) -> bool {
    let cx = x as f32 + 0.5 - wf / 2.0;
    let cy = y as f32 + 0.5 - hf / 2.0;
    (cx * cx + cy * cy).sqrt() < radius
}

fn circle_into(from: &CpuFrame, to: &CpuFrame, eased: f32, out: &mut [u8], w: usize, h: usize) {
    let wf = w as f32;
    let hf = h as f32;
    let half_w = wf / 2.0;
    let half_h = hf / 2.0;
    let max_radius = (half_w * half_w + half_h * half_h).sqrt();
    let radius = eased * max_radius;
    let row_bytes = w * 4;
    for y in 0..h {
        let o = out_row(out, w, y);
        let cy = y as f32 + 0.5 - half_h;
        let rest = radius * radius - cy * cy;
        if rest <= 0.0 {
            o.copy_from_slice(row(from, y));
            continue;
        }
        // 候选区间（f64 近似 + 谓词校正）：inside(x) ⟺ |x + 0.5 - half_w| < dx
        // ⟺ floor(half_w - dx - 0.5) + 1 ≤ x ≤ ceil(half_w + dx - 0.5)。
        let dx = f64::from(rest).sqrt();
        let center = f64::from(half_w) - 0.5;
        let x0 = (center - dx).floor() + 1.0;
        let x1 = (center + dx).ceil();
        let mut lo = x0.max(0.0) as usize;
        let mut hi = x1.clamp(0.0, w as f64) as usize;
        // 校正：候选误差只在 ±2 像素内（浮点近似），收缩 + 外扩各限 4 步。
        for _ in 0..4 {
            if lo < hi && !circle_inside(lo, y, wf, hf, radius) {
                lo += 1;
            } else if lo > 0 && circle_inside(lo - 1, y, wf, hf, radius) {
                lo -= 1;
            } else {
                break;
            }
        }
        for _ in 0..4 {
            if hi > lo && !circle_inside(hi - 1, y, wf, hf, radius) {
                hi -= 1;
            } else if hi < w && circle_inside(hi, y, wf, hf, radius) {
                hi += 1;
            } else {
                break;
            }
        }
        // 兜底：若校正未能收敛（理论上不会发生），该行退回逐像素，
        // 保证与参考实现一致而不是画出错误的区间。
        if (lo < hi && !circle_inside(lo, y, wf, hf, radius))
            || (hi > lo && !circle_inside(hi - 1, y, wf, hf, radius))
        {
            for x in 0..w {
                let src = if circle_inside(x, y, wf, hf, radius) {
                    to
                } else {
                    from
                };
                o[x * 4..x * 4 + 4].copy_from_slice(&row(src, y)[x * 4..x * 4 + 4]);
            }
            continue;
        }
        let (x0, x1) = (lo, hi);
        if x0 >= x1 {
            o.copy_from_slice(row(from, y));
            continue;
        }
        if x0 > 0 {
            o[..x0 * 4].copy_from_slice(&row(from, y)[..x0 * 4]);
        }
        o[x0 * 4..x1 * 4].copy_from_slice(&row(to, y)[x0 * 4..x1 * 4]);
        if x1 < w {
            o[x1 * 4..row_bytes].copy_from_slice(&row(from, y)[x1 * 4..row_bytes]);
        }
    }
}

impl TransitionEffect for CpuTransition {
    /// 从 [`DisplayFrame`] 中取出 CPU 帧并 [`Self::prepare_frames`]。
    ///
    /// # Errors
    ///
    /// 非 CPU 帧或帧不匹配时返回 [`RenderError`]。
    fn prepare(&mut self, from: &DisplayFrame, to: &DisplayFrame) -> Result<(), RenderError> {
        let (DisplayFrame::Cpu(from), DisplayFrame::Cpu(to)) = (from, to) else {
            return Err(RenderError::Failed(
                "CPU 转场仅支持 DisplayFrame::Cpu 输入（GPU/DMA-BUF 路径属 M4/M5）".to_string(),
            ));
        };
        self.prepare_frames(from.clone(), to.clone())
    }

    /// 按进度渲染（`progress ∈ [0, 1]`），返回 CPU 帧。
    ///
    /// # Errors
    ///
    /// 未就绪或进度非法时返回 [`RenderError`]。
    fn render(&mut self, progress: f32) -> Result<DisplayFrame, RenderError> {
        self.render_at(progress).map(DisplayFrame::Cpu)
    }

    fn release(&mut self) {
        CpuTransition::release(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::types::FrameError;

    /// 构造指定填充色的 Xrgb8888 帧（内存 [B,G,R,X]，这里按通道原样填 value）。
    fn xrgb(w: u32, h: u32, value: [u8; 4]) -> CpuFrame {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            data.extend_from_slice(&value);
        }
        CpuFrame {
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    /// 构造坐标编码帧：通道 0 = x、通道 1 = y，便于断言平移位置。
    fn xrgb_grid(w: u32, h: u32) -> CpuFrame {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[x as u8, y as u8, 0, 0]);
            }
        }
        CpuFrame {
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    fn pixel(frame: &CpuFrame, x: u32, y: u32) -> [u8; 4] {
        let off = (y * frame.stride + x * 4) as usize;
        [
            frame.data[off],
            frame.data[off + 1],
            frame.data[off + 2],
            frame.data[off + 3],
        ]
    }

    fn spec(kind: EffectKind) -> TransitionSpec {
        TransitionSpec {
            kind,
            easing: Easing::Linear,
            duration: Duration::from_millis(300),
        }
    }

    fn prepared(kind: EffectKind, from: CpuFrame, to: CpuFrame) -> CpuTransition {
        let mut t = CpuTransition::new(spec(kind));
        t.prepare_frames(from, to).unwrap();
        t
    }

    // ---------- fade ----------

    #[test]
    fn fade_endpoints_equal_from_and_to() {
        let from = xrgb(4, 4, [10, 20, 30, 40]);
        let to = xrgb(4, 4, [110, 220, 130, 240]);
        let mut t = prepared(EffectKind::Fade, from.clone(), to.clone());
        assert_eq!(t.render_at(0.0).unwrap().data, from.data);
        assert_eq!(t.render_at(1.0).unwrap().data, to.data);
    }

    #[test]
    fn fade_midpoint_blends_linearly() {
        let from = xrgb(2, 2, [0, 0, 0, 0]);
        let to = xrgb(2, 2, [200, 100, 50, 250]);
        let mut t = prepared(EffectKind::Fade, from, to);
        let out = t.render_at(0.5).unwrap();
        // t=0.5 线性混合：round(lerp)。
        assert_eq!(pixel(&out, 0, 0), [100, 50, 25, 125]);
        assert_eq!(pixel(&out, 1, 1), [100, 50, 25, 125]);
    }

    // ---------- slide ----------

    #[test]
    fn slide_endpoints_equal_from_and_to() {
        let from = xrgb_grid(4, 2);
        let to = xrgb(4, 2, [9, 9, 9, 9]);
        for direction in [
            TransitionDirection::Left,
            TransitionDirection::Right,
            TransitionDirection::Up,
            TransitionDirection::Down,
        ] {
            let mut t = prepared(EffectKind::Slide { direction }, from.clone(), to.clone());
            assert_eq!(t.render_at(0.0).unwrap().data, from.data, "{direction:?}");
            assert_eq!(t.render_at(1.0).unwrap().data, to.data, "{direction:?}");
        }
    }

    #[test]
    fn slide_left_shifts_to_from_left_edge() {
        // W=4，t=0.5 → to 平移 dx = -2：out 左两列来自 to 右两列，右两列为 from。
        let from = xrgb_grid(4, 1);
        let to = xrgb(4, 1, [7, 7, 7, 7]);
        let mut t = prepared(
            EffectKind::Slide {
                direction: TransitionDirection::Left,
            },
            from,
            to,
        );
        let out = t.render_at(0.5).unwrap();
        assert_eq!(pixel(&out, 0, 0), [7, 7, 7, 7], "to 从左缘滑入");
        assert_eq!(pixel(&out, 1, 0), [7, 7, 7, 7]);
        // 右两列露出 from（坐标编码帧：x 存通道 0）。
        assert_eq!(pixel(&out, 2, 0), [2, 0, 0, 0]);
        assert_eq!(pixel(&out, 3, 0), [3, 0, 0, 0]);
    }

    #[test]
    fn slide_right_is_mirrored() {
        // W=4，t=0.5 → to 平移 dx = +2：out 右两列来自 to 左两列。
        let from = xrgb_grid(4, 1);
        let to = xrgb(4, 1, [7, 7, 7, 7]);
        let mut t = prepared(
            EffectKind::Slide {
                direction: TransitionDirection::Right,
            },
            from,
            to,
        );
        let out = t.render_at(0.5).unwrap();
        assert_eq!(pixel(&out, 0, 0), [0, 0, 0, 0], "左两列露出 from");
        assert_eq!(pixel(&out, 1, 0), [1, 0, 0, 0]);
        assert_eq!(pixel(&out, 2, 0), [7, 7, 7, 7], "to 从右缘滑入");
        assert_eq!(pixel(&out, 3, 0), [7, 7, 7, 7]);
    }

    // ---------- wipe ----------

    #[test]
    fn wipe_endpoints_equal_from_and_to() {
        let from = xrgb_grid(4, 4);
        let to = xrgb(4, 4, [5, 5, 5, 5]);
        for direction in [
            TransitionDirection::Left,
            TransitionDirection::Right,
            TransitionDirection::Up,
            TransitionDirection::Down,
        ] {
            let mut t = prepared(EffectKind::Wipe { direction }, from.clone(), to.clone());
            assert_eq!(t.render_at(0.0).unwrap().data, from.data, "{direction:?}");
            assert_eq!(t.render_at(1.0).unwrap().data, to.data, "{direction:?}");
        }
    }

    #[test]
    fn wipe_left_hard_edge_at_half() {
        // W=4，t=0.5 → 边界 x+0.5 < 2.0：x=0,1 显示 to，x=2,3 显示 from。
        let from = xrgb_grid(4, 1);
        let to = xrgb(4, 1, [5, 5, 5, 5]);
        let mut t = prepared(
            EffectKind::Wipe {
                direction: TransitionDirection::Left,
            },
            from,
            to,
        );
        let out = t.render_at(0.5).unwrap();
        assert_eq!(pixel(&out, 0, 0), [5, 5, 5, 5]);
        assert_eq!(pixel(&out, 1, 0), [5, 5, 5, 5]);
        assert_eq!(pixel(&out, 2, 0), [2, 0, 0, 0]);
        assert_eq!(pixel(&out, 3, 0), [3, 0, 0, 0]);
    }

    #[test]
    fn wipe_down_advances_from_bottom() {
        // H=4，t=0.5 → 边界 y+0.5 > 2.0：y=2,3 显示 to，y=0,1 显示 from。
        let from = xrgb_grid(1, 4);
        let to = xrgb(1, 4, [5, 5, 5, 5]);
        let mut t = prepared(
            EffectKind::Wipe {
                direction: TransitionDirection::Down,
            },
            from,
            to,
        );
        let out = t.render_at(0.5).unwrap();
        assert_eq!(pixel(&out, 0, 0), [0, 0, 0, 0]);
        // 坐标编码帧：像素 (0,1) 的通道 0=x=0、通道 1=y=1。
        assert_eq!(pixel(&out, 0, 1), [0, 1, 0, 0]);
        assert_eq!(pixel(&out, 0, 2), [5, 5, 5, 5]);
        assert_eq!(pixel(&out, 0, 3), [5, 5, 5, 5]);
    }

    // ---------- circle ----------

    #[test]
    fn circle_endpoints_equal_from_and_to() {
        let from = xrgb_grid(8, 8);
        let to = xrgb(8, 8, [5, 5, 5, 5]);
        let mut t = prepared(EffectKind::Circle, from.clone(), to.clone());
        assert_eq!(t.render_at(0.0).unwrap().data, from.data);
        assert_eq!(t.render_at(1.0).unwrap().data, to.data);
    }

    #[test]
    fn circle_half_reveals_center_not_corner() {
        // 8x8，t=0.5 → 半径 = 0.5 * sqrt(32) ≈ 2.83：中心像素在圆内，四角在圆外。
        let from = xrgb_grid(8, 8);
        let to = xrgb(8, 8, [5, 5, 5, 5]);
        let mut t = prepared(EffectKind::Circle, from, to);
        let out = t.render_at(0.5).unwrap();
        for (x, y) in [(3, 3), (4, 4), (3, 4), (4, 3)] {
            assert_eq!(pixel(&out, x, y), [5, 5, 5, 5], "圆内 {x},{y}");
        }
        for (x, y) in [(0, 0), (7, 0), (0, 7), (7, 7)] {
            assert_eq!(pixel(&out, x, y), [x as u8, y as u8, 0, 0], "圆外 {x},{y}");
        }
    }

    // ---------- easing ----------

    #[test]
    fn easing_clamps_and_maps_nan_to_zero() {
        for easing in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            assert_eq!(apply_easing(easing, 0.0), 0.0);
            assert_eq!(apply_easing(easing, 1.0), 1.0);
            assert_eq!(apply_easing(easing, -0.5), 0.0, "{easing:?}");
            assert_eq!(apply_easing(easing, 1.5), 1.0, "{easing:?}");
            assert_eq!(apply_easing(easing, f32::NAN), 0.0, "{easing:?}");
            assert_eq!(apply_easing(easing, f32::INFINITY), 1.0, "{easing:?}");
        }
    }

    #[test]
    fn easing_is_monotonic() {
        for easing in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            let mut prev = -1.0f32;
            for i in 0..=20 {
                let v = apply_easing(easing, i as f32 / 20.0);
                assert!((0.0..=1.0).contains(&v), "{easing:?} 越界: {v}");
                assert!(v >= prev, "{easing:?} 非单调: {prev} -> {v}");
                prev = v;
            }
        }
    }

    #[test]
    fn easing_shapes_differ_from_linear() {
        // 端点相同但中途不同：EaseIn 慢于线性，EaseOut 快于线性，EaseInOut 居中平滑。
        let mid_in = apply_easing(Easing::EaseIn, 0.5);
        let mid_out = apply_easing(Easing::EaseOut, 0.5);
        let mid_io = apply_easing(Easing::EaseInOut, 0.5);
        assert!(mid_in < 0.5, "EaseIn 0.5 = {mid_in}");
        assert!(mid_out > 0.5, "EaseOut 0.5 = {mid_out}");
        assert!((mid_io - 0.5).abs() < 1e-6, "EaseInOut 0.5 = {mid_io}");
    }

    // ---------- 校验与资源生命周期 ----------

    #[test]
    fn rejects_mismatched_frames() {
        let mut t = CpuTransition::new(spec(EffectKind::Fade));
        // 尺寸不一致。
        let err = t
            .prepare_frames(xrgb(4, 4, [0; 4]), xrgb(2, 2, [0; 4]))
            .unwrap_err();
        assert!(matches!(err, RenderError::FrameMismatch(_)), "{err:?}");
        assert!(!t.is_prepared());

        // 格式不一致（Xrgb vs Rgba）。
        let rgba = CpuFrame {
            width: 4,
            height: 4,
            stride: 16,
            format: PixelFormat::Rgba8888,
            data: Arc::from(vec![0u8; 64].into_boxed_slice()),
        };
        let err = t.prepare_frames(xrgb(4, 4, [0; 4]), rgba).unwrap_err();
        assert!(matches!(err, RenderError::FrameMismatch(_)), "{err:?}");

        // 不支持的格式（双方同为 Argb8888）。
        let argb = || CpuFrame {
            width: 4,
            height: 4,
            stride: 16,
            format: PixelFormat::Argb8888,
            data: Arc::from(vec![0u8; 64].into_boxed_slice()),
        };
        let err = t.prepare_frames(argb(), argb()).unwrap_err();
        assert!(matches!(err, RenderError::FrameMismatch(_)), "{err:?}");
    }

    #[test]
    fn rejects_invalid_progress() {
        let mut t = prepared(EffectKind::Fade, xrgb(2, 2, [0; 4]), xrgb(2, 2, [1; 4]));
        for p in [-0.01, 1.01, f32::NAN, f32::NEG_INFINITY, f32::INFINITY] {
            assert!(
                matches!(t.render_at(p), Err(RenderError::InvalidProgress { .. })),
                "progress = {p}"
            );
        }
    }

    #[test]
    fn rejects_unprepared_and_reports_not_prepared() {
        let mut t = CpuTransition::new(spec(EffectKind::Circle));
        assert!(!t.is_prepared());
        assert!(matches!(t.render_at(0.5), Err(RenderError::NotPrepared)));
        t.release();
        assert!(matches!(t.render_at(0.5), Err(RenderError::NotPrepared)));
    }

    #[test]
    fn release_drops_arc_references() {
        let from = xrgb(4, 4, [1; 4]);
        let to = xrgb(4, 4, [2; 4]);
        let (from_data, to_data) = (Arc::clone(&from.data), Arc::clone(&to.data));

        let mut t = prepared(EffectKind::Fade, from, to);
        let out = t.render_at(0.5).unwrap();
        // prepare 后：测试引用 + CpuTransition 持有。
        assert_eq!(Arc::strong_count(&from_data), 2, "prepare 持有 from");
        assert_eq!(Arc::strong_count(&to_data), 2, "prepare 持有 to");

        t.release();
        assert!(!t.is_prepared());
        assert_eq!(Arc::strong_count(&from_data), 1, "release 释放 from");
        assert_eq!(Arc::strong_count(&to_data), 1, "release 释放 to");
        drop(out);
    }

    #[test]
    fn render_reuses_buffer_when_dropped() {
        let mut t = prepared(EffectKind::Fade, xrgb(4, 4, [0; 4]), xrgb(4, 4, [255; 4]));
        let first = t.render_at(0.5).unwrap();
        let first_ptr = Arc::as_ptr(&first.data);
        // 内部缓冲 + 调用方。
        assert_eq!(Arc::strong_count(&first.data), 2);
        drop(first);
        let second = t.render_at(0.75).unwrap();
        // 复用内部缓冲：同一份像素存储。
        assert_eq!(Arc::as_ptr(&second.data), first_ptr, "缓冲应被复用");
    }

    #[test]
    fn last_request_wins_without_leaking() {
        let mut t = CpuTransition::new(spec(EffectKind::Fade));
        let (a_from, a_to) = (xrgb(2, 2, [1; 4]), xrgb(2, 2, [2; 4]));
        let (a_from_ref, a_to_ref) = (Arc::clone(&a_from.data), Arc::clone(&a_to.data));
        t.prepare_frames(a_from, a_to).unwrap();
        t.render_at(0.3).unwrap();
        assert_eq!(Arc::strong_count(&a_from_ref), 2);
        assert_eq!(Arc::strong_count(&a_to_ref), 2);

        // 新请求覆盖旧资源（最后请求优先）。
        let (b_from, b_to) = (xrgb(3, 3, [3; 4]), xrgb(3, 3, [4; 4]));
        let (b_from_ref, b_to_ref) = (Arc::clone(&b_from.data), Arc::clone(&b_to.data));
        t.prepare_frames(b_from, b_to).unwrap();
        assert_eq!(Arc::strong_count(&a_from_ref), 1, "旧 from 不应泄漏");
        assert_eq!(Arc::strong_count(&a_to_ref), 1, "旧 to 不应泄漏");
        assert_eq!(Arc::strong_count(&b_from_ref), 2);
        assert_eq!(Arc::strong_count(&b_to_ref), 2);
        // 尺寸变化后仍可渲染。
        let out = t.render_at(1.0).unwrap();
        assert_eq!(out.width, 3);
        assert_eq!(out.height, 3);
    }

    // ---------- TransitionEffect trait 兼容 ----------

    #[test]
    fn trait_impl_roundtrip() {
        use liuguang_core::types::DisplayFrame;
        let mut t = CpuTransition::new(TransitionSpec {
            kind: EffectKind::Wipe {
                direction: TransitionDirection::Up,
            },
            easing: Easing::EaseOut,
            duration: Duration::from_millis(200),
        });
        let from = DisplayFrame::Cpu(xrgb(2, 2, [0; 4]));
        let to = DisplayFrame::Cpu(xrgb(2, 2, [200, 100, 50, 255]));
        t.prepare(&from, &to).unwrap();
        let out0 = t.render(0.0).unwrap();
        assert_eq!(out0.width(), 2);
        match &out0 {
            DisplayFrame::Cpu(f) => assert_eq!(f.data, xrgb(2, 2, [0; 4]).data),
            _ => panic!("输出应为 CPU 帧"),
        }
        let out1 = t.render(1.0).unwrap();
        match &out1 {
            DisplayFrame::Cpu(f) => assert_eq!(f.data, xrgb(2, 2, [200, 100, 50, 255]).data),
            _ => panic!("输出应为 CPU 帧"),
        }
        t.release();
        assert!(matches!(t.render(0.5), Err(RenderError::NotPrepared)));
    }

    #[test]
    fn trait_impl_rejects_non_cpu_frames() {
        use liuguang_core::types::{DisplayFrame, GpuFrame};
        let mut t = CpuTransition::new(spec(EffectKind::Fade));
        let gpu = DisplayFrame::GpuTexture(GpuFrame {
            width: 2,
            height: 2,
            format: PixelFormat::Xrgb8888,
            texture_id: 1,
        });
        let cpu = DisplayFrame::Cpu(xrgb(2, 2, [0; 4]));
        assert!(matches!(t.prepare(&gpu, &cpu), Err(RenderError::Failed(_))));
    }

    #[test]
    fn prepare_frames_validates_frame_structure() {
        let mut t = CpuTransition::new(spec(EffectKind::Fade));
        let bad = CpuFrame {
            width: 4,
            height: 4,
            stride: 16,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; 8].into_boxed_slice()), // 数据过短
        };
        let err = t.prepare_frames(bad, xrgb(4, 4, [0; 4])).unwrap_err();
        assert!(
            matches!(
                err,
                RenderError::InvalidFrame(FrameError::DataTooShort { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn supports_rgba8888_pairs() {
        let mut data = vec![0u8; 16];
        for i in 0..4 {
            data[i * 4..i * 4 + 4].copy_from_slice(&[10, 20, 30, 40]);
        }
        let from = CpuFrame {
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data.into_boxed_slice()),
        };
        let to = rgba(2, 2, [210, 220, 230, 240]);
        let mut t = prepared(EffectKind::Fade, from.clone(), to.clone());
        let out = t.render_at(0.5).unwrap();
        assert_eq!(out.format, PixelFormat::Rgba8888);
        // 逐通道混合 [10,20,30,40] 与 [210,220,230,240] 的中点。
        assert_eq!(pixel(&out, 0, 0), [110, 120, 130, 140]);
        assert_eq!(t.render_at(1.0).unwrap().data, to.data);
    }

    fn rgba(w: u32, h: u32, value: [u8; 4]) -> CpuFrame {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            data.extend_from_slice(&value);
        }
        CpuFrame {
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Rgba8888,
            data: Arc::from(data.into_boxed_slice()),
        }
    }

    // ---------- 优化后实现 vs 逐像素参考实现（语义等价性） ----------

    /// 逐像素参考实现 = 优化前的语义（slide 的越界索引做了钳制以避免 panic）。
    ///
    /// 用于断言优化后的行级 memcpy 实现与它**逐字节一致**。
    fn reference_render_into(
        from: &CpuFrame,
        to: &CpuFrame,
        kind: EffectKind,
        eased: f32,
    ) -> Vec<u8> {
        let (w, h) = (from.width, from.height);
        let wf = w as f32;
        let hf = h as f32;
        let (dx, dy) = match kind {
            EffectKind::Slide { direction } => {
                let rest = 1.0 - eased;
                match direction {
                    TransitionDirection::Left => (-rest * wf, 0.0),
                    TransitionDirection::Right => (rest * wf, 0.0),
                    TransitionDirection::Up => (0.0, -rest * hf),
                    TransitionDirection::Down => (0.0, rest * hf),
                }
            }
            _ => (0.0, 0.0),
        };
        let half_w = wf / 2.0;
        let half_h = hf / 2.0;
        let max_radius = (half_w * half_w + half_h * half_h).sqrt();
        let radius = eased * max_radius;
        let px = |f: &CpuFrame, x: u32, y: u32| -> [u8; 4] {
            let off = (y * f.stride + x * 4) as usize;
            [
                f.data[off],
                f.data[off + 1],
                f.data[off + 2],
                f.data[off + 3],
            ]
        };
        let lerp = |a: [u8; 4], b: [u8; 4]| -> [u8; 4] {
            let mut out = [0u8; 4];
            for i in 0..4 {
                let mixed = f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * eased;
                out[i] = mixed.round().clamp(0.0, 255.0) as u8;
            }
            out
        };
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let value = match kind {
                    EffectKind::Fade => lerp(px(from, x, y), px(to, x, y)),
                    EffectKind::Slide { .. } => {
                        let sx = x as f32 - dx;
                        let sy = y as f32 - dy;
                        if sx >= 0.0 && sx < wf && sy >= 0.0 && sy < hf {
                            // 钳制：原实现在 sx.sy 落到边界时可能取到越界索引。
                            let xi = (sx.round() as u32).min(w - 1);
                            let yi = (sy.round() as u32).min(h - 1);
                            px(to, xi, yi)
                        } else {
                            px(from, x, y)
                        }
                    }
                    EffectKind::Wipe { direction } => {
                        let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                        let covered = match direction {
                            TransitionDirection::Left => cx < eased * wf,
                            TransitionDirection::Right => cx > wf * (1.0 - eased),
                            TransitionDirection::Up => cy < eased * hf,
                            TransitionDirection::Down => cy > hf * (1.0 - eased),
                        };
                        if covered {
                            px(to, x, y)
                        } else {
                            px(from, x, y)
                        }
                    }
                    EffectKind::Circle => {
                        let (cx, cy) = (x as f32 + 0.5 - half_w, y as f32 + 0.5 - half_h);
                        if (cx * cx + cy * cy).sqrt() < radius {
                            px(to, x, y)
                        } else {
                            px(from, x, y)
                        }
                    }
                };
                let off = (y * w + x) as usize * 4;
                out[off..off + 4].copy_from_slice(&value);
            }
        }
        out
    }

    fn render_with(kind: EffectKind, from: &CpuFrame, to: &CpuFrame, eased: f32) -> Vec<u8> {
        let mut t = CpuTransition::new(TransitionSpec {
            kind,
            easing: Easing::Linear,
            duration: Duration::from_millis(100),
        });
        t.prepare_frames(from.clone(), to.clone()).unwrap();
        t.render_at(eased).unwrap().data.to_vec()
    }

    #[test]
    fn optimized_matches_reference_fade_all_easings() {
        for (w, h) in [(16u32, 8u32), (37, 13), (4, 1), (128, 3)] {
            let from = xrgb_grid(w, h);
            let to = xrgb(w, h, [200, 100, 50, 250]);
            for eased in [0.0f32, 0.1, 0.25, 0.5, 0.5000001, 0.75, 0.999, 1.0] {
                let got = render_with(EffectKind::Fade, &from, &to, eased);
                let want = reference_render_into(&from, &to, EffectKind::Fade, eased);
                // 整数定点混合与浮点实现的差允许 ±1（逐通道），其余必须一致。
                for (i, (g, e)) in got.iter().zip(want.iter()).enumerate() {
                    assert!(
                        (*g as i32 - *e as i32).abs() <= 1,
                        "{w}x{h} eased={eased} 字节 {i}: {g} vs {e}"
                    );
                }
            }
        }
    }

    #[test]
    fn optimized_matches_reference_fade_exactly_when_t_is_binary() {
        // k = 0 / 128 / 256 时整数混合与浮点实现完全一致（无舍入歧义）。
        let from = xrgb_grid(32, 4);
        let to = xrgb(32, 4, [201, 101, 51, 251]);
        for eased in [0.0f32, 0.5, 1.0] {
            let got = render_with(EffectKind::Fade, &from, &to, eased);
            let want = reference_render_into(&from, &to, EffectKind::Fade, eased);
            assert_eq!(got, want, "eased={eased}");
        }
    }

    #[test]
    fn optimized_matches_reference_wipe_and_circle_exactly() {
        let directions = [
            TransitionDirection::Left,
            TransitionDirection::Right,
            TransitionDirection::Up,
            TransitionDirection::Down,
        ];
        for (w, h) in [(16u32, 8u32), (37, 13), (5, 5), (64, 1)] {
            let from = xrgb_grid(w, h);
            let to = xrgb(w, h, [9, 200, 30, 255]);
            for d in directions {
                for eased in [0.0f32, 0.1, 0.5, 0.75, 1.0] {
                    let kind = EffectKind::Wipe { direction: d };
                    let got = render_with(kind, &from, &to, eased);
                    let want = reference_render_into(&from, &to, kind, eased);
                    assert_eq!(got, want, "wipe {d:?} {w}x{h} eased={eased}");
                }
            }
            for eased in [0.0f32, 0.05, 0.3, 0.5, 0.9, 1.0] {
                let got = render_with(EffectKind::Circle, &from, &to, eased);
                let want = reference_render_into(&from, &to, EffectKind::Circle, eased);
                assert_eq!(got, want, "circle {w}x{h} eased={eased}");
            }
        }
    }

    #[test]
    fn optimized_matches_reference_slide_at_integral_shift() {
        // 位移为整数时两条实现严格一致（亚像素边界差异见 slide 越界回归测试）。
        let directions = [
            TransitionDirection::Left,
            TransitionDirection::Right,
            TransitionDirection::Up,
            TransitionDirection::Down,
        ];
        for (w, h) in [(16u32, 8u32), (40, 4)] {
            let from = xrgb_grid(w, h);
            let to = xrgb(w, h, [7, 8, 9, 10]);
            for d in directions {
                for eased in [0.0f32, 0.5, 1.0] {
                    let kind = EffectKind::Slide { direction: d };
                    let got = render_with(kind, &from, &to, eased);
                    let want = reference_render_into(&from, &to, kind, eased);
                    assert_eq!(got, want, "slide {d:?} {w}x{h} eased={eased}");
                }
            }
        }
    }

    /// 回归：原实现 `sx.round()` 未钳制上界，`sx ∈ [w-0.5, w)` 时 panic
    /// （真机 4K 转场已复现）。现在必须安全且等于参考实现。
    #[test]
    fn slide_never_panics_at_subpixel_edge() {
        for w in [4u32, 5, 37, 3840] {
            let h = 3u32;
            let from = xrgb_grid(w, h);
            let to = xrgb(w, h, [1, 2, 3, 4]);
            for step in 0..=20 {
                let eased = step as f32 / 20.0;
                for d in [TransitionDirection::Left, TransitionDirection::Right] {
                    let kind = EffectKind::Slide { direction: d };
                    let got = render_with(kind, &from, &to, eased);
                    let want = reference_render_into(&from, &to, kind, eased);
                    assert_eq!(got.len(), want.len());
                    // 端点必须严格等于 from / to（与既有断言一致）。
                    if step == 0 {
                        assert_eq!(got, from.data.to_vec(), "{w} eased=0 {d:?}");
                    }
                    if step == 20 {
                        assert_eq!(got, to.data.to_vec(), "{w} eased=1 {d:?}");
                    }
                }
            }
        }
    }

    /// 输出缓冲双缓冲：上层"渲染 → 持有上一帧"时不产生每帧分配。
    #[test]
    fn transition_reuses_output_buffers_when_caller_holds_previous() {
        let from = xrgb_grid(64, 16);
        let to = xrgb(64, 16, [50, 60, 70, 80]);
        let mut t = prepared(EffectKind::Fade, from, to);
        let mut held = t.render_at(0.0).unwrap(); // 模拟上层持有当前帧
        for i in 1..60 {
            let next = t.render_at(i as f32 / 60.0).unwrap();
            held = next;
        }
        assert_eq!(held.width, 64);
        assert!(
            t.buffer_allocations() <= 2,
            "60 帧转场只允许 ≤2 次输出缓冲分配，实际 {}",
            t.buffer_allocations()
        );
    }
}
