//! 可复用渲染部件（只读 [`crate::model::AppState`]，不含业务逻辑）。

pub mod bars;
pub mod list;
pub mod modal;
pub mod nav;
pub mod status;
pub mod text;

use crate::ui::theme as th;
use crate::ui::widgets::text::{pad_display, truncate_display};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// `键: 值` 行的默认键列宽（显示列）。
pub const KEY_COLS: usize = 9;

/// 画一个带标题的分区（所有页面共用，保证边框/标题一致）。
pub fn section(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    if area.width < 2 || area.height < 2 {
        // 太小就退化：不画边框，直接画内容（绝不越界）
        let take = lines.len().min(area.height as usize);
        if area.width > 0 && area.height > 0 {
            f.render_widget(Paragraph::new(lines[..take].to_vec()), area);
        }
        return;
    }
    f.render_widget(
        Paragraph::new(lines).style(th::panel_bg()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(th::border())
                .style(th::panel_bg())
                .title(format!(" {title} ")),
        ),
        area,
    );
}

/// 分区内的内容行宽度（边框占 2 列）。
pub fn section_inner_cols(area: Rect) -> usize {
    area.width.saturating_sub(2).max(1) as usize
}

/// `键: 值` 一行（宽度安全；键按**显示宽度**补齐，中文键不再错位）。
///
/// 旧实现用 `{key:<9}`（按 char 计数），中文键（宽度 2/字）会撑破 9 列导致
/// 同页数值列不对齐；现在统一走 [`pad_display`]。
pub fn kv(key: &str, value: &str, cols: usize) -> Line<'static> {
    kv_w(key, value, cols, KEY_COLS)
}

/// 指定键列宽的 `键: 值` 行（键较长的页面用同一宽度即可纵向对齐）。
///
/// 键先**截断**到 `key_cols`（带省略号）再补齐：键只是短标签，
/// 对齐比完整键更重要；完整语义放在值或详情区，不会被丢掉。
pub fn kv_w(key: &str, value: &str, cols: usize, key_cols: usize) -> Line<'static> {
    let key = pad_display(&truncate_display(key, key_cols), key_cols);
    Line::from(vec![
        Span::styled(format!(" {key}"), th::muted()),
        Span::raw(truncate_display(value, cols.saturating_sub(key_cols + 2))),
    ])
}

/// 说明/禁用标记行（用于"本阶段未实现"的诚实标注）。
pub fn disabled_note(text: &str, cols: usize) -> Line<'static> {
    Line::from(Span::styled(
        truncate_display(&format!(" ⚠ {text}"), cols),
        th::warn(),
    ))
}

/// 一行普通文本（宽度安全）。
pub fn text_line(text: &str, cols: usize) -> Line<'static> {
    Line::from(truncate_display(text, cols))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ui::widgets::text::display_width;

    /// 回归：中文键必须按**显示宽度**补齐，否则同页数值列会错位
    ///（旧实现 `{key:<9}` 按 char 计数，「播放列表来源」宽度 12 会撑破 9 列）。
    #[test]
    fn kv_pads_keys_by_display_width() {
        let cjk = kv("播放列表来源", "v", 60);
        let ascii = kv("Decode", "v", 60);
        assert_eq!(
            display_width(&cjk.spans[0].content),
            display_width(&ascii.spans[0].content),
            "中/英文键补齐后必须等宽：{cjk_key:?} vs {ascii_key:?}",
            cjk_key = cjk.spans[0].content,
            ascii_key = ascii.spans[0].content
        );
    }

    /// `kv_w`：键较长的页面用统一 key_cols 即可纵向对齐。
    #[test]
    fn kv_w_aligns_long_keys() {
        let a = kv_w("CPU", "1", 80, 18);
        let b = kv_w("硬解（daemon status）", "2", 80, 18);
        assert_eq!(
            display_width(&a.spans[0].content),
            display_width(&b.spans[0].content)
        );
        // 键超宽时不截断（宁错位不丢字），值预算只按 key_cols 扣减
        let long = kv_w("播放列表来源（daemon 未提供）", "x", 80, 18);
        assert!(long.spans[0].content.starts_with(" 播放列表来源"));
    }
}
