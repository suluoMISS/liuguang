//! 极小终端面板：`<60 列 或 <18 行` 时只显示
//! **终端过小提示 + 当前状态 + 最小快捷键**（`docs/TUI_DESIGN.md` §4）。
//!
//! 这一档存在的意义：宁可明确告诉用户"窗口太小"，也不去挤一个会越界/错位的界面。

use crate::model::AppState;
use crate::ui::theme as th;
use crate::ui::widgets::text::truncate_display;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// 进入极小档的最小可用尺寸（提示用户需要多大）。
pub const MIN_WIDTH: u16 = 60;
/// 进入极小档的最小可用高度。
pub const MIN_HEIGHT: u16 = 18;

/// 极小档内容行（与渲染分离，便于单测）。
///
/// `size` 用**当前帧的实际面积**，而不是状态里缓存的尺寸：
/// 终端刚被缩小时，缓存值可能还没更新。
pub fn tiny_lines(st: &AppState, cols: usize, size: (u16, u16)) -> Vec<Line<'static>> {
    let size = format!(
        "终端过小（当前 {}x{}，至少需要 {}x{}）",
        size.0, size.1, MIN_WIDTH, MIN_HEIGHT
    );
    vec![
        Line::from(Span::styled(truncate_display(&size, cols), th::warn_bold())),
        Line::from(Span::styled(
            truncate_display(
                &format!("状态：{} · 输出：{}", st.status.state, st.status.decoder),
                cols,
            ),
            th::muted(),
        )),
        Line::from(Span::styled(
            truncate_display(&format!("页面：{}（按 1-6 可切）", st.page.title()), cols),
            Style::default(),
        )),
        Line::from(Span::styled(
            truncate_display("Q 退出 · ? 帮助 · 1-6 切页", cols),
            th::accent(),
        )),
    ]
}

/// 渲染极小档（与 [`crate::ui::layout::Tier::Tiny`] 对齐命名）。
pub fn render_tiny(f: &mut Frame, area: Rect, st: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = tiny_lines(st, area.width as usize, (area.width, area.height));
    let take = lines.len().min(area.height as usize);
    f.render_widget(Paragraph::new(lines[..take].to_vec()), area);
}

/// 该尺寸是否需要极小档（供 UI 与其他模块共用）。
pub fn is_too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

/// 一行式提示（宽度不够放下多行时使用）。
pub fn tiny_one_line(st: &AppState, cols: usize, size: (u16, u16)) -> String {
    truncate_display(
        &format!(
            "终端过小 {}x{}（需 {}x{}）│ {} │ Q 退出 ? 帮助",
            size.0, size.1, MIN_WIDTH, MIN_HEIGHT, st.status.state
        ),
        cols,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ui::widgets::text::display_width;

    #[test]
    fn tiny_lines_mention_size_state_and_keys() {
        let mut st = AppState::new();
        st.size = (50, 15);
        let text: String = tiny_lines(&st, 60, (50, 15))
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("终端过小"), "{text}");
        assert!(text.contains("50x15"), "{text}");
        assert!(text.contains("60x18"), "{text}");
        assert!(text.contains("Q 退出"), "{text}");
        assert!(text.contains("1-6"), "{text}");
    }

    #[test]
    fn tiny_lines_respect_width() {
        let mut st = AppState::new();
        st.size = (30, 8);
        for cols in [10usize, 20, 30, 59] {
            for line in tiny_lines(&st, cols, (30, 8)) {
                let text = line.to_string();
                assert!(
                    display_width(&text) <= cols.max(1),
                    "cols={cols} 超宽({}): {text}",
                    display_width(&text)
                );
            }
        }
    }

    #[test]
    fn too_small_threshold_matches_design() {
        let mk = |w, h| Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        assert!(is_too_small(mk(59, 40)));
        assert!(is_too_small(mk(200, 17)));
        assert!(!is_too_small(mk(60, 18)));
        assert!(!is_too_small(mk(140, 40)));
    }
}
