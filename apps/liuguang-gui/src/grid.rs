//! 按素材原始宽高比排布的「对齐行」（justified rows）磁贴布局。
//!
//! 为什么不用 `gtk::FlowBox`：`FlowBox` 只能等宽等高（或按内容自然尺寸），
//! 想做 Windows 10 磁贴那种「行高一致、宽高各异」的墙，就得自己决定每项的
//! 宽度。这里把**布局计算**做成纯函数（无 GTK 依赖、可单测），渲染层只负责
//! 把算出来的 `(width, height)` 塞进每一行的 `gtk::Box`。
//!
//! 算法（贪心 + 行内拉伸）：
//! 1. 逐项累加宽高比，直到「按目标行高排下来已经超出可用宽度」或达到每行上限；
//! 2. 行高 `ideal = (可用宽度 - 间距) / 比例和`，再夹到目标行高的 0.75–1.35 倍；
//! 3. 以夹过的行高为基准算每项宽度，最后按整行像素和拉伸，**保证整行铺满**
//!    （最多 ±1px 的取整误差，落到最右一项）。
//!
//! 极端比例（超宽全景 / 超长竖构图）统一夹到 [`MIN_ASPECT`]..=[`MAX_ASPECT`]，
//! 磁贴内部仍用 `ContentFit::Cover` 裁切——布局只关心「框」，不关心裁切。

use liuguang_ipc::protocol::MediaItem;

/// 拿不到宽高时的缺省比例（16:9）。
pub const DEFAULT_ASPECT: f32 = 16.0 / 9.0;
/// 允许参与排布的最小宽高比（竖构图上限，约 2.5 倍高的竖条再窄就不像话）。
pub const MIN_ASPECT: f32 = 0.4;
/// 允许参与排布的最大宽高比（超宽全景上限）。
pub const MAX_ASPECT: f32 = 4.0;

/// 一个磁贴在行内的尺寸（逻辑像素）。
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    /// 在输入列表里的下标（渲染层据此找回媒体项）。
    pub index: usize,
    /// 磁贴宽（逻辑像素，≥1）。
    pub width: i32,
    /// 磁贴高（逻辑像素，≥1）。
    pub height: i32,
}

/// 一行磁贴。
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// 行内磁贴（从左到右）。
    pub tiles: Vec<Tile>,
    /// 行高（该行所有磁贴同高）。
    pub height: i32,
}

/// 布局输入。
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub struct LayoutInput {
    /// 可用宽度（ScrolledWindow 的内容宽度，逻辑像素）。
    pub width: i32,
    /// 目标行高（由卡片尺寸滑杆映射而来）。
    pub target_height: f32,
    /// 磁贴之间的间距。
    pub gap: i32,
    /// 每行最多几项（防止宽屏上出现「一行十几张」的细碎格子）。
    pub max_per_row: usize,
}

#[cfg(test)]
impl Default for LayoutInput {
    fn default() -> Self {
        Self {
            width: 1200,
            target_height: 180.0,
            gap: 12,
            max_per_row: 4,
        }
    }
}

/// 媒体项的宽高比：优先用 probe 出来的真实像素尺寸，缺失或异常时退 16:9。
#[must_use]
pub fn aspect_of(item: &MediaItem) -> f32 {
    match item.details.as_ref() {
        Some(details) if details.width > 0 && details.height > 0 => {
            clamp_aspect(details.width as f32 / details.height as f32)
        }
        _ => DEFAULT_ASPECT,
    }
}

/// 把任意比例夹到 [`MIN_ASPECT`]..=[`MAX_ASPECT`]；非有限值退回缺省。
#[must_use]
pub fn clamp_aspect(aspect: f32) -> f32 {
    if !aspect.is_finite() || aspect <= 0.0 {
        return DEFAULT_ASPECT;
    }
    aspect.clamp(MIN_ASPECT, MAX_ASPECT)
}

/// 目标行高允许的浮动区间：太低会显得碎，太高会只剩一行。
#[cfg(test)]
fn height_bounds(target: f32) -> (f32, f32) {
    let target = target.clamp(96.0, 420.0);
    (target * 0.75, target * 1.35)
}

/// 把宽高比序列排成若干行。
///
/// 返回的行顺序就是渲染顺序；每行 `tiles` 的宽度之和 + 间距 **等于** `width`
/// （无法整除时误差落在最右一项，绝对值 ≤ 行内项数）。
#[cfg(test)]
#[must_use]
pub fn plan(aspects: &[f32], input: LayoutInput) -> Vec<Row> {
    let width = input.width.max(1);
    let gap = input.gap.max(0);
    let max_per_row = input.max_per_row.clamp(1, 12);
    let (min_h, max_h) = height_bounds(input.target_height);

    let mut rows: Vec<Row> = Vec::new();
    let mut index = 0usize;
    while index < aspects.len() {
        // 贪心：一直加到「按目标行高已经排不下」为止。
        let mut sum = 0.0f32;
        let mut count = 0usize;
        while index + count < aspects.len() && count < max_per_row {
            let aspect = clamp_aspect(aspects[index + count]);
            let next_sum = sum + aspect;
            let next_count = count + 1;
            let content = (width - gap * (next_count as i32 - 1)).max(1);
            let ideal = content as f32 / next_sum;
            sum = next_sum;
            count = next_count;
            // 已经缩到目标行高以下：本行就到这里（再多会更小）。
            if ideal <= input.target_height.max(1.0) {
                break;
            }
        }
        if count == 0 {
            break;
        }

        let content = (width - gap * (count as i32 - 1)).max(1);
        let ideal = content as f32 / sum;
        let height = ideal.clamp(min_h, max_h).round().max(1.0);
        let mut tiles: Vec<Tile> = Vec::with_capacity(count);
        let mut assigned = 0i32;
        for offset in 0..count {
            let aspect = clamp_aspect(aspects[index + offset]);
            // 按行内宽高比占比分配宽度；最后一项吃掉取整余量，保证整行像素和精确等于可用宽度。
            let tile_width = if offset + 1 == count {
                (content - assigned).max(1)
            } else {
                (content as f32 * (aspect / sum)).round().max(1.0) as i32
            };
            assigned += tile_width;
            tiles.push(Tile {
                index: index + offset,
                width: tile_width,
                height: height as i32,
            });
        }
        rows.push(Row {
            tiles,
            height: height as i32,
        });
        index += count;
    }
    rows
}

/// 卡片尺寸滑杆（160..=360）→ 目标行高的映射。
///
/// 滑杆是「卡片多大」的直觉刻度，行高取它的 0.75 倍左右，让 16:9 素材的
/// 磁贴宽度接近滑杆值（用户拖动时看到的尺寸感一致）。
#[cfg(test)]
#[must_use]
pub fn target_height_for_card_size(card_size: i32) -> f32 {
    let size = f32::from(card_size.clamp(160, 360) as i16);
    (size * 0.75).clamp(120.0, 300.0)
}

/// 每行最多几项：宽屏多放、窄窗少放。
#[cfg(test)]
#[must_use]
pub fn max_per_row_for_width(width: i32) -> usize {
    let columns = width.max(1) / 260;
    usize::try_from(columns.clamp(2, 6)).unwrap_or(4)
}

/// 2D 磁贴网格里的单张已排布卡片（行列坐标与跨度均以基元格为单位）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedTile {
    /// 在输入列表里的下标。
    pub index: usize,
    /// 起始列（0..cols）。
    pub col: i32,
    /// 起始行。
    pub row: i32,
    /// 跨列数。
    pub col_span: i32,
    /// 跨行数。
    pub row_span: i32,
    /// 逻辑像素宽（含中间间隙）。
    pub width: i32,
    /// 逻辑像素高（含中间间隙）。
    pub height: i32,
}

/// 2D Win10 磁贴排布结果。
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BentoLayout {
    pub cols: i32,
    pub cell: i32,
    pub tiles: Vec<PlacedTile>,
}

/// 高定画报墙排布结果（支持宽/高独立调控，100% 满格零黑洞）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorialLayout {
    pub cols: i32,
    pub cell_w: i32,
    pub cell_h: i32,
    pub tiles: Vec<PlacedTile>,
}

type ChapterSlot = (i32, i32, i32, i32);

/// 计算偶数基元列数（2 / 4 / 6 / 8），保证 2×1 横屏磁贴总能整行咬合铺满。
#[cfg(test)]
#[must_use]
pub fn bento_columns(width: i32, card_size: i32) -> i32 {
    let width = width.max(1);
    if width < 460 {
        return 2;
    }
    let unit = f32::from(card_size.clamp(160, 360) as i16) * 0.72;
    let raw = (width as f32 / unit).round() as i32;
    let even = ((raw + 1) / 2) * 2;
    let max_cols = if width < 720 { 4 } else { 8 };
    even.clamp(4.min(max_cols), max_cols)
}

/// 根据壁纸原始宽高比决定 Win10 磁贴的网格跨度 `(col_span, row_span)`。
#[cfg(test)]
#[must_use]
pub fn tile_span_for_aspect(aspect: f32, cols: i32) -> (i32, i32) {
    let aspect = clamp_aspect(aspect);
    let cols = cols.max(1);
    if aspect >= 2.20 {
        if cols <= 4 { (cols, 1) } else { (3, 1) }
    } else if aspect >= 1.52 {
        (2.min(cols), 1)
    } else if aspect >= 1.22 {
        if cols >= 6 { (3, 2) } else { (2.min(cols), 1) }
    } else if aspect >= 0.82 {
        if cols >= 4 { (2, 2) } else { (1, 1) }
    } else {
        (1, 2)
    }
}

/// 用二维密铺装箱把各比例磁贴自动咬合进等格 Bento 网格，并通过后置补缝消除行尾与内部黑洞。
#[cfg(test)]
#[must_use]
pub fn plan_bento(aspects: &[f32], width: i32, card_size: i32, gap: i32) -> BentoLayout {
    let cols = bento_columns(width, card_size);
    let gap = gap.max(0);
    let cell = ((width.max(1) - gap * (cols - 1)) / cols).max(64);
    let cols_usize = cols as usize;
    let mut occupied: Vec<Vec<bool>> = Vec::new();
    let mut tiles = Vec::with_capacity(aspects.len());

    let ensure_rows = |grid: &mut Vec<Vec<bool>>, needed: usize, cols_n: usize| {
        while grid.len() < needed {
            grid.push(vec![false; cols_n]);
        }
    };

    for (index, &aspect) in aspects.iter().enumerate() {
        let (col_span, row_span) = tile_span_for_aspect(aspect, cols);
        let cw = col_span as usize;
        let rh = row_span as usize;
        let mut r = 0usize;
        let (col, row) = 'search: loop {
            ensure_rows(&mut occupied, r + rh, cols_usize);
            for c in 0..=cols_usize - cw {
                let mut free = true;
                'check: for dr in 0..rh {
                    for dc in 0..cw {
                        if occupied[r + dr][c + dc] {
                            free = false;
                            break 'check;
                        }
                    }
                }
                if free {
                    for row in occupied.iter_mut().skip(r).take(rh) {
                        for cell_used in row.iter_mut().skip(c).take(cw) {
                            *cell_used = true;
                        }
                    }
                    break 'search (c as i32, r as i32);
                }
            }
            r += 1;
        };
        tiles.push(PlacedTile {
            index,
            col,
            row,
            col_span,
            row_span,
            width: 0,
            height: 0,
        });
    }

    // 补缝阶段：消除因比例组合在行尾（如 3×1 与 2×1 同行）或大跨度卡旁产生的内部黑洞。
    let total_rows = occupied.len();
    loop {
        let mut expanded = false;
        let mut order: Vec<usize> = (0..tiles.len()).collect();
        order.sort_by(|&a, &b| tiles[b].col.cmp(&tiles[a].col));
        for idx in order {
            let (r_start, r_end, mut c_end) = {
                let t = &tiles[idx];
                (
                    t.row as usize,
                    (t.row + t.row_span) as usize,
                    (t.col + t.col_span) as usize,
                )
            };
            let above_bottom = r_end < total_rows;
            let row_half_full = occupied
                .get(r_start)
                .is_some_and(|row| row.iter().filter(|&&u| u).count() * 2 >= cols_usize);
            if !above_bottom && !row_half_full {
                continue;
            }
            while c_end < cols_usize && (r_start..r_end).all(|r| !occupied[r][c_end]) {
                for row in occupied.iter_mut().take(r_end).skip(r_start) {
                    row[c_end] = true;
                }
                tiles[idx].col_span += 1;
                c_end += 1;
                expanded = true;
            }
        }
        if !expanded {
            break;
        }
    }
    for tile in &mut tiles {
        let c_start = tile.col as usize;
        let c_end = (tile.col + tile.col_span) as usize;
        let mut r_end = (tile.row + tile.row_span) as usize;
        while r_end < total_rows && (c_start..c_end).all(|c| !occupied[r_end][c]) {
            for cell_used in &mut occupied[r_end][c_start..c_end] {
                *cell_used = true;
            }
            tile.row_span += 1;
            r_end += 1;
        }
        tile.width = tile.col_span * cell + (tile.col_span - 1) * gap;
        tile.height = tile.row_span * cell + (tile.row_span - 1) * gap;
    }

    BentoLayout { cols, cell, tiles }
}

fn pick_6col_chapter(
    remaining: usize,
    portraits: usize,
    step: usize,
) -> (i32, &'static [ChapterSlot]) {
    // 6 列蒙德里安画报母版（全部经严格校验 100% 满格，0 空洞）：
    // A (6x3, 6 项): 3x2 主角 + 1x1 + 2x1 + 2x2 方卡 + 1x2 竖幅 + 3x1 宽幅
    const CH_A: [ChapterSlot; 6] = [
        (0, 0, 3, 2),
        (3, 0, 1, 1),
        (4, 0, 2, 1),
        (3, 1, 2, 2),
        (5, 1, 1, 2),
        (0, 2, 3, 1),
    ];
    // B (6x3, 6 项): 4x2 巨幕电影主角 + 五块 2x1 横幅节奏块
    const CH_B: [ChapterSlot; 6] = [
        (0, 0, 2, 1),
        (2, 0, 4, 2),
        (0, 1, 2, 1),
        (0, 2, 2, 1),
        (2, 2, 2, 1),
        (4, 2, 2, 1),
    ];
    // C (6x3, 6 项): 左侧 2x2 方卡 + 右侧 3x2 镜像主图 + 底部 3x1 宽幅
    const CH_C: [ChapterSlot; 6] = [
        (0, 0, 2, 2),
        (2, 0, 2, 1),
        (4, 0, 2, 1),
        (2, 1, 1, 1),
        (3, 1, 3, 2),
        (0, 2, 3, 1),
    ];
    // D (6x3, 8 项): 双 1x2 竖幅 + 双 1x1 小砖 + 2x1 + 双 2x2 焦点主图（高密度杂志画报版式）
    const CH_D: [ChapterSlot; 8] = [
        (0, 0, 1, 2),
        (1, 0, 1, 2),
        (2, 0, 1, 1),
        (3, 0, 1, 1),
        (4, 0, 2, 1),
        (2, 1, 2, 2),
        (4, 1, 2, 2),
        (0, 2, 2, 1),
    ];
    // E (6x2, 4 项): 4x2 横幅巨幕 + 2x1 + 双 1x1 紧凑收尾
    const CH_E: [ChapterSlot; 4] = [(0, 0, 4, 2), (4, 0, 2, 1), (4, 1, 1, 1), (5, 1, 1, 1)];
    const TAIL_5: [ChapterSlot; 5] = [
        (0, 0, 3, 2),
        (3, 0, 2, 1),
        (5, 0, 1, 1),
        (3, 1, 1, 1),
        (4, 1, 2, 1),
    ];
    const TAIL_3: [ChapterSlot; 3] = [(0, 0, 2, 1), (2, 0, 2, 1), (4, 0, 2, 1)];
    const TAIL_2: [ChapterSlot; 2] = [(0, 0, 3, 2), (3, 0, 3, 2)];
    const TAIL_1: [ChapterSlot; 1] = [(0, 0, 6, 2)];

    match remaining {
        1 => (2, &TAIL_1),
        2 => (2, &TAIL_2),
        3 => (1, &TAIL_3),
        4 => (2, &CH_E),
        5 => (2, &TAIL_5),
        6 => match step % 3 {
            0 => (3, &CH_A),
            1 => (3, &CH_C),
            _ => (3, &CH_B),
        },
        7 => (3, &CH_A), // 后余 1 项走 TAIL_1
        _ => {
            if portraits >= 2 && step % 3 == 1 {
                (3, &CH_D)
            } else {
                match step % 4 {
                    0 => (3, &CH_A),
                    1 => (3, &CH_D),
                    2 => (3, &CH_C),
                    _ => (3, &CH_B),
                }
            }
        }
    }
}

fn pick_4col_chapter(
    remaining: usize,
    portraits: usize,
    step: usize,
) -> (i32, &'static [ChapterSlot]) {
    const CH_4A: [ChapterSlot; 5] = [
        (0, 0, 2, 2),
        (2, 0, 2, 1),
        (2, 1, 1, 1),
        (3, 1, 1, 1),
        (0, 2, 4, 1),
    ];
    const CH_4B: [ChapterSlot; 6] = [
        (0, 0, 2, 1),
        (2, 0, 2, 2),
        (0, 1, 1, 1),
        (1, 1, 1, 1),
        (0, 2, 2, 1),
        (2, 2, 2, 1),
    ];
    const CH_4C: [ChapterSlot; 3] = [(0, 0, 1, 2), (1, 0, 3, 1), (1, 1, 3, 1)];
    const TAIL_4: [ChapterSlot; 4] = [(0, 0, 2, 2), (2, 0, 2, 1), (2, 1, 1, 1), (3, 1, 1, 1)];
    const TAIL_3: [ChapterSlot; 3] = [(0, 0, 2, 2), (2, 0, 2, 1), (2, 1, 2, 1)];
    const TAIL_2: [ChapterSlot; 2] = [(0, 0, 2, 1), (2, 0, 2, 1)];
    const TAIL_1: [ChapterSlot; 1] = [(0, 0, 4, 2)];

    match remaining {
        1 => (2, &TAIL_1),
        2 => (1, &TAIL_2),
        3 => (2, &TAIL_3),
        4 => (2, &TAIL_4),
        5 => (3, &CH_4A),
        _ => {
            if portraits >= 1 && step % 3 == 2 {
                (2, &CH_4C)
            } else if step % 2 == 0 {
                (3, &CH_4A)
            } else {
                (3, &CH_4B)
            }
        }
    }
}

/// 自适应蒙德里安画报墙排布（零黑洞 + 大小错落节奏 + 比例智能槽位匹配）。
#[must_use]
pub fn plan_editorial(aspects: &[f32], width: i32, card_size: i32, gap: i32) -> EditorialLayout {
    let width = width.max(1);
    let gap = gap.max(0);
    let cols = if width < 620 || card_size >= 300 {
        4
    } else {
        6
    };
    let cell_w = ((width - gap * (cols - 1)) / cols).max(64);
    let ratio = if cols == 6 {
        let t = (f32::from(card_size.clamp(160, 280) as i16) - 160.0) / 120.0;
        0.82 + t * 0.30
    } else {
        let t = (f32::from(card_size.clamp(160, 360) as i16) - 160.0) / 200.0;
        0.78 + t * 0.30
    };
    let cell_h = (cell_w as f32 * ratio).round().max(64.0) as i32;

    if aspects.is_empty() {
        return EditorialLayout {
            cols,
            cell_w,
            cell_h,
            tiles: Vec::new(),
        };
    }

    let mut tiles = Vec::with_capacity(aspects.len());
    let mut cursor = 0usize;
    let mut row_offset = 0i32;
    let mut step = 0usize;

    while cursor < aspects.len() {
        let remaining = aspects.len() - cursor;
        let window_end = (cursor + 8).min(aspects.len());
        let portraits = aspects[cursor..window_end]
            .iter()
            .filter(|&&a| clamp_aspect(a) < 0.85)
            .count();

        let (chapter_rows, slots) = if cols == 6 {
            pick_6col_chapter(remaining, portraits, step)
        } else {
            pick_4col_chapter(remaining, portraits, step)
        };
        let count = slots.len().min(remaining);

        // 先按槽位比例的“极端度”排序（竖幅 1x2 与宽幅 3x1/4x2 优先匹配最贴合的素材）。
        let mut slot_order: Vec<usize> = (0..count).collect();
        slot_order.sort_by(|&a, &b| {
            let slot_ratio = |idx: usize| -> f32 {
                let (_, _, cw, rh) = slots[idx];
                ((cw * cell_w) as f32 / (rh * cell_h).max(1) as f32)
                    .ln()
                    .abs()
            };
            slot_ratio(b).total_cmp(&slot_ratio(a))
        });

        let mut used_items = vec![false; count];
        let mut assigned = vec![0usize; count];
        for slot_idx in slot_order {
            let (_, _, cw, rh) = slots[slot_idx];
            let s_ratio = (cw * cell_w) as f32 / (rh * cell_h).max(1) as f32;
            let mut best_local = 0usize;
            let mut best_cost = f32::INFINITY;
            for (local_idx, used) in used_items.iter().enumerate() {
                if *used {
                    continue;
                }
                let item_aspect = clamp_aspect(aspects[cursor + local_idx]);
                let cost = (item_aspect / s_ratio).ln().abs();
                if cost < best_cost {
                    best_cost = cost;
                    best_local = local_idx;
                }
            }
            used_items[best_local] = true;
            assigned[slot_idx] = cursor + best_local;
        }

        for (slot_idx, &(col, row, col_span, row_span)) in slots[..count].iter().enumerate() {
            let tile_w = col_span * cell_w + (col_span - 1) * gap;
            let tile_h = row_span * cell_h + (row_span - 1) * gap;
            tiles.push(PlacedTile {
                index: assigned[slot_idx],
                col,
                row: row_offset + row,
                col_span,
                row_span,
                width: tile_w,
                height: tile_h,
            });
        }

        cursor += count;
        row_offset += chapter_rows;
        step += 1;
    }

    EditorialLayout {
        cols,
        cell_w,
        cell_h,
        tiles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(width: u32, height: u32) -> MediaItem {
        MediaItem {
            id: format!("/tmp/{width}x{height}.png"),
            source_id: "test".to_owned(),
            name: format!("{width}x{height}.png"),
            path: format!("/tmp/{width}x{height}.png"),
            kind: "image".to_owned(),
            size_bytes: 1,
            modified_unix: 0,
            details: Some(liuguang_ipc::protocol::MediaDetails {
                width,
                height,
                fps_millihz: None,
                duration_ms: None,
                codec: None,
                pix_fmt: None,
            }),
        }
    }

    fn no_details() -> MediaItem {
        MediaItem {
            id: "/tmp/plain".to_owned(),
            source_id: "test".to_owned(),
            name: "plain".to_owned(),
            path: "/tmp/plain".to_owned(),
            kind: "image".to_owned(),
            size_bytes: 1,
            modified_unix: 0,
            details: None,
        }
    }

    #[test]
    fn aspect_uses_details_and_falls_back_to_16_9() {
        assert!((aspect_of(&item(1920, 1080)) - 16.0 / 9.0).abs() < 1e-3);
        assert!((aspect_of(&no_details()) - DEFAULT_ASPECT).abs() < 1e-6);
        // 竖构图：9:16 保留（在允许区间内）。
        assert!((aspect_of(&item(1080, 1920)) - 0.5625).abs() < 1e-3);
    }

    #[test]
    fn extreme_aspects_are_clamped() {
        assert!((aspect_of(&item(8000, 1000)) - MAX_ASPECT).abs() < 1e-6);
        assert!((aspect_of(&item(500, 5000)) - MIN_ASPECT).abs() < 1e-6);
        assert!((clamp_aspect(f32::NAN) - DEFAULT_ASPECT).abs() < 1e-6);
    }

    #[test]
    fn every_row_exactly_fills_the_width() {
        let aspects = [
            16.0 / 9.0,
            2.35,
            0.5625,
            4.0 / 3.0,
            1.0,
            16.0 / 9.0,
            2.35,
            0.5625,
        ];
        let input = LayoutInput {
            width: 2400,
            target_height: 200.0,
            gap: 12,
            max_per_row: 4,
        };
        let rows = plan(&aspects, input);
        assert!(!rows.is_empty());
        for row in &rows {
            let used: i32 = row.tiles.iter().map(|tile| tile.width).sum();
            let gaps = input.gap * (row.tiles.len() as i32 - 1);
            assert_eq!(used + gaps, input.width, "行没铺满: {row:?}");
        }
    }

    #[test]
    fn row_heights_stay_near_target() {
        let aspects = [0.5, 1.0, 1.33, 1.78, 2.35, 4.0];
        let rows = plan(
            &aspects,
            LayoutInput {
                width: 1600,
                target_height: 180.0,
                gap: 12,
                max_per_row: 3,
            },
        );
        let (min_h, max_h) = height_bounds(180.0);
        for row in &rows {
            let h = f32::from(row.height as i16);
            assert!(h >= min_h - 1.0 && h <= max_h + 1.0, "行高越界: {h}");
            for tile in &row.tiles {
                assert_eq!(tile.height, row.height);
            }
        }
    }

    #[test]
    fn portrait_tiles_keep_nonzero_width() {
        let rows = plan(
            &[0.4, 0.4, 0.4],
            LayoutInput {
                width: 900,
                target_height: 200.0,
                gap: 12,
                max_per_row: 4,
            },
        );
        for row in &rows {
            for tile in &row.tiles {
                assert!(tile.width >= 1);
                assert!(tile.height >= 1);
            }
        }
    }

    #[test]
    fn single_wide_item_still_fills_the_row() {
        let rows = plan(
            &[4.0],
            LayoutInput {
                width: 1600,
                target_height: 180.0,
                gap: 12,
                max_per_row: 4,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tiles.len(), 1);
        assert_eq!(rows[0].tiles[0].width, 1600);
    }

    #[test]
    fn narrow_container_becomes_one_tile_per_row() {
        let rows = plan(
            &[16.0 / 9.0; 4],
            LayoutInput {
                width: 300,
                target_height: 180.0,
                gap: 12,
                max_per_row: 2,
            },
        );
        assert_eq!(rows.len(), 4, "窄容器应每行一项");
        for row in &rows {
            assert_eq!(row.tiles.len(), 1);
            assert_eq!(row.tiles[0].width, 300);
        }
    }

    #[test]
    fn max_per_row_is_respected() {
        let rows = plan(
            &[16.0 / 9.0; 12],
            LayoutInput {
                width: 4000,
                target_height: 120.0,
                gap: 12,
                max_per_row: 3,
            },
        );
        for row in &rows {
            assert!(row.tiles.len() <= 3, "超出行数上限: {}", row.tiles.len());
        }
    }

    #[test]
    fn empty_input_has_no_rows() {
        assert!(plan(&[], LayoutInput::default()).is_empty());
    }

    #[test]
    fn card_size_maps_to_sane_row_height() {
        assert!(target_height_for_card_size(160) >= 120.0);
        assert!(target_height_for_card_size(360) <= 300.0);
        assert!(target_height_for_card_size(240) > target_height_for_card_size(200));
        assert_eq!(max_per_row_for_width(800), 3);
        assert_eq!(max_per_row_for_width(4000), 6);
        assert_eq!(max_per_row_for_width(120), 2);
    }

    #[test]
    fn clamped_row_distributes_width_proportionally_without_stretching_last_tile() {
        let aspects = [0.75, 0.75, 1.78];
        let rows = plan(
            &aspects,
            LayoutInput {
                width: 1000,
                target_height: 180.0,
                gap: 12,
                max_per_row: 3,
            },
        );
        assert_eq!(rows.len(), 1);
        let tiles = &rows[0].tiles;
        assert_eq!(tiles.len(), 3);
        let sum: f32 = aspects.iter().sum();
        let content = 1000 - 12 * 2;
        for (idx, tile) in tiles.iter().enumerate() {
            let expected = (content as f32) * (aspects[idx] / sum);
            assert!(
                (tile.width as f32 - expected).abs() <= 2.0,
                "第 {idx} 项宽度应按比例分配：期望 ~{expected:.1}, 实际 {}",
                tile.width
            );
        }
    }

    #[test]
    fn bento_tiles_never_overlap_and_preserve_aspect_spans() {
        let aspects = [2.4, 1.78, 1.33, 1.0, 0.56, 1.78, 0.65, 1.78];
        let layout = plan_bento(&aspects, 1080, 240, 10);
        assert_eq!(layout.cols, 6);
        assert_eq!(layout.tiles.len(), aspects.len());

        // 验证比例映射正确：21:9→3×1、16:9→2×1、4:3→3×2、1:1→2×2、竖屏→1×2。
        assert_eq!((layout.tiles[0].col_span, layout.tiles[0].row_span), (3, 1));
        assert_eq!((layout.tiles[1].col_span, layout.tiles[1].row_span), (2, 1));
        assert_eq!((layout.tiles[2].col_span, layout.tiles[2].row_span), (3, 2));
        assert_eq!((layout.tiles[3].col_span, layout.tiles[3].row_span), (2, 2));
        assert_eq!((layout.tiles[4].col_span, layout.tiles[4].row_span), (1, 2));

        // 任何两块磁贴在二维格网上都不得重叠，且坐标不越界、像素严格对齐基元格。
        let mut used = std::collections::HashSet::new();
        for tile in &layout.tiles {
            assert!(tile.col >= 0 && tile.col + tile.col_span <= layout.cols);
            assert_eq!(
                tile.width,
                tile.col_span * layout.cell + (tile.col_span - 1) * 10
            );
            assert_eq!(
                tile.height,
                tile.row_span * layout.cell + (tile.row_span - 1) * 10
            );
            for dr in 0..tile.row_span {
                for dc in 0..tile.col_span {
                    assert!(
                        used.insert((tile.col + dc, tile.row + dr)),
                        "磁贴格子重叠于 ({}, {})",
                        tile.col + dc,
                        tile.row + dr
                    );
                }
            }
        }
    }

    #[test]
    fn bento_all_widescreen_fills_rows_without_holes() {
        let aspects = [16.0 / 9.0; 6];
        let layout = plan_bento(&aspects, 1080, 240, 10);
        assert_eq!(layout.cols, 6);
        for (i, tile) in layout.tiles.iter().enumerate() {
            assert_eq!((tile.col_span, tile.row_span), (2, 1));
            assert_eq!(tile.row, (i / 3) as i32);
            assert_eq!(tile.col, ((i % 3) * 2) as i32);
        }
    }

    #[test]
    fn bento_closes_interior_holes_on_4_col_and_6_col_grids() {
        // 4 列网格下超宽图直接占满 4 列，不留行尾黑洞。
        let four = plan_bento(&[1.78, 1.78, 2.5, 1.78, 1.78], 760, 240, 10);
        assert_eq!(four.cols, 4);
        assert_eq!((four.tiles[2].col, four.tiles[2].col_span), (0, 4));
        let mut cells_4 = std::collections::HashSet::new();
        let mut max_r4 = 0;
        for t in &four.tiles {
            max_r4 = max_r4.max(t.row + t.row_span);
            for r in t.row..t.row + t.row_span {
                for c in t.col..t.col + t.col_span {
                    assert!(cells_4.insert((c, r)));
                }
            }
        }
        assert_eq!(cells_4.len(), (max_r4 * four.cols) as usize);

        // 6 列网格下 3×2 大卡旁叠两张 2×1 后自动补满剩余列，消除内部黑洞。
        let six = plan_bento(&[1.78, 1.4, 1.78, 1.78, 1.78, 1.78], 1080, 240, 10);
        assert_eq!(six.cols, 6);
        let mut cells_6 = std::collections::HashSet::new();
        let mut max_r6 = 0;
        for t in &six.tiles {
            max_r6 = max_r6.max(t.row + t.row_span);
            for r in t.row..t.row + t.row_span {
                for c in t.col..t.col + t.col_span {
                    assert!(cells_6.insert((c, r)));
                }
            }
        }
        for r in 0..(max_r6 - 1) {
            for c in 0..six.cols {
                assert!(cells_6.contains(&(c, r)), "内部第 {r} 行第 {c} 列不得留洞");
            }
        }
    }

    #[test]
    fn editorial_chapters_are_100_percent_airtight_for_every_count() {
        for card_size in [240, 320] {
            for count in 1..=35usize {
                let aspects: Vec<f32> = (0..count)
                    .map(|i| match i % 5 {
                        0 => 2.35,
                        1 => 1.78,
                        2 => 1.33,
                        3 => 0.65,
                        _ => 1.0,
                    })
                    .collect();
                let layout = plan_editorial(&aspects, 1100, card_size, 3);
                assert_eq!(layout.tiles.len(), count);
                let mut seen_idx = std::collections::HashSet::new();
                let mut cells = std::collections::HashSet::new();
                let mut max_row = 0;
                for tile in &layout.tiles {
                    assert!(seen_idx.insert(tile.index), "重复下标 {}", tile.index);
                    assert_eq!(
                        tile.width,
                        tile.col_span * layout.cell_w + (tile.col_span - 1) * 3
                    );
                    assert_eq!(
                        tile.height,
                        tile.row_span * layout.cell_h + (tile.row_span - 1) * 3
                    );
                    max_row = max_row.max(tile.row + tile.row_span);
                    for r in tile.row..tile.row + tile.row_span {
                        for c in tile.col..tile.col + tile.col_span {
                            assert!(c >= 0 && c < layout.cols);
                            assert!(cells.insert((c, r)), "格子重叠 ({c}, {r}) count={count}");
                        }
                    }
                }
                assert_eq!(
                    cells.len(),
                    (max_row * layout.cols) as usize,
                    "count={count}, card_size={card_size} 出现黑洞: filled={} expected={}",
                    cells.len(),
                    max_row * layout.cols
                );
            }
        }
    }

    #[test]
    fn editorial_matches_portraits_and_ultrawides_to_fitting_slots() {
        // CH_A (6 个槽位) 包含 1 个竖幅 (1x2) 与超宽/宽屏槽位；输入含 1 张竖图和 1 张超宽图时应准确对位。
        let aspects = [1.78, 0.56, 1.78, 1.33, 2.50, 1.0];
        let layout = plan_editorial(&aspects, 1100, 240, 3);
        let portrait_tile = layout.tiles.iter().find(|t| t.index == 1).unwrap();
        assert_eq!((portrait_tile.col_span, portrait_tile.row_span), (1, 2));
        let wide_tile = layout.tiles.iter().find(|t| t.index == 4).unwrap();
        assert_eq!((wide_tile.col_span, wide_tile.row_span), (3, 1));
    }
}
