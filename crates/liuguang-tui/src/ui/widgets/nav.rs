//! 导航：宽档左侧导航列（**持久的当前页高亮**）与中档顶部 Tab。

use crate::model::{AppState, Page};
use crate::ui::theme as th;
use crate::ui::widgets::text::truncate_display;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// 当前页标记（持久高亮：`▸` + 反显）。
///
/// 字符本体移到 [`crate::ui::theme`]；这里保留旧名作为别名，测试与外部
/// 引用不受影响。
pub const CURRENT_MARKER: &str = th::CURRENT_PAGE;

/// 宽档左导航：6 行，当前页高亮；每行 `▸ 1 总览`。
pub fn render_nav(f: &mut Frame, area: Rect, st: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines: Vec<Line> = Page::ALL
        .into_iter()
        .map(|page| nav_line(page, st.page == page, area.width as usize))
        .collect();
    f.render_widget(Paragraph::new(lines).style(th::text()), area);
}

fn nav_line(page: Page, current: bool, cols: usize) -> Line<'static> {
    let text = format!(
        "{} {} {}",
        if current { CURRENT_MARKER } else { " " },
        page.hotkey(),
        page.title()
    );
    let text = truncate_display(&text, cols);
    let style = if current {
        th::selected_row()
    } else {
        Style::default()
    };
    Line::from(Span::styled(text, style))
}

/// 中档顶部 Tab：单行，当前页高亮。
pub fn render_tabs(f: &mut Frame, area: Rect, st: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    let cols = area.width as usize;
    for page in Page::ALL {
        let current = st.page == page;
        let text = format!(
            "{}:{}{}",
            page.hotkey(),
            page.title(),
            if current { "*" } else { "" }
        );
        let text = truncate_display(&text, cols.saturating_sub(used));
        if text.is_empty() {
            break;
        }
        used += crate::ui::widgets::text::display_width(&text) + 1;
        let style = if current {
            th::selected_row()
        } else {
            th::text()
        };
        spans.push(Span::styled(format!("{text} "), style));
        if used >= cols {
            break;
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 小档单行页面指示（含 `←→` 子页提示）。
pub fn render_page_indicator(f: &mut Frame, area: Rect, st: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = format!(
        " {}/{} {} · {} [{}]",
        st.page.index() + 1,
        Page::ALL.len(),
        st.page.title(),
        st.subpage.label(),
        st.focus.label()
    );
    f.render_widget(
        Paragraph::new(truncate_display(&text, area.width as usize)).style(th::accent()),
        area,
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn nav_line_marks_current_page_only() {
        let current = nav_line(Page::Library, true, 20);
        let other = nav_line(Page::Library, false, 20);
        let current_text: String = current
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        let other_text: String = other.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(current_text.starts_with(CURRENT_MARKER));
        assert!(!other_text.starts_with(CURRENT_MARKER));
        assert!(current_text.contains("媒体库"));
        // 高亮不仅仅是文字：样式必须不同（持久高亮的可访问性）
        assert_ne!(current.spans[0].style, other.spans[0].style);
    }

    #[test]
    fn nav_line_is_width_safe() {
        for cols in [0usize, 4, 8, 20] {
            let line = nav_line(Page::Dashboard, true, cols);
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(
                crate::ui::widgets::text::display_width(&text) <= cols.max(1) || cols == 0,
                "cols={cols} 超宽: {text}"
            );
        }
    }
}
