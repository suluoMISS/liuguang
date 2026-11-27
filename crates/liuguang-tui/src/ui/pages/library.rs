//! 媒体库 Library：列表 + 详情 + 筛选/搜索/分页（数据全部来自 IPC 抽象）。
//!
//! **本页不做任何目录扫描**：`std::fs` 已从 TUI 移除，路径校验由 daemon 负责。

use crate::model::{AppState, ApplyPhase};
use crate::ui::theme as th;
use crate::ui::widgets::list::media_lines;
use crate::ui::widgets::{disabled_note, section, section_inner_cols, text_line};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

/// 主体：分页列表（只渲染当前页；10k 条也不拖慢）。
pub fn render(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines = media_lines(st, area.height.saturating_sub(2) as usize, cols);
    if st.search_focus {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!(
                    " 搜索中（防抖 250ms）：{}▏  Enter 结束 · Esc 取消",
                    st.search
                ),
                th::warn(),
            )),
        );
    }
    if !st.connection.is_online() {
        lines.push(disabled_note(
            "断线：媒体列表来自最近一次成功响应，可能已过期",
            cols,
        ));
    }
    // T25-F2：操作页自身必须能看到最终失败态（不能只在总览页可见）
    if let Some(phase) = &st.apply_phase {
        let text = match phase {
            ApplyPhase::Sending => "已发出：等待 daemon 应答…".to_string(),
            ApplyPhase::Accepted => format!(
                "已受理，等待 daemon 确认上屏…（{}）——**尚未**上屏",
                st.apply_target.label()
            ),
            ApplyPhase::Applied => {
                format!("已应用（daemon 已确认）：{}", st.apply_target.label())
            }
            ApplyPhase::Failed(reason) => {
                // 保证语**前置**：`truncate_display` 只截尾，保证语必须永远在屏上
                // （旧写法先拼 reason 再截断 ⇒ 120/200 列下"（当前壁纸未改变）"被截掉）
                format!("应用失败（当前壁纸未改变）：{reason}")
            }
        };
        lines.insert(
            0,
            Line::from(Span::styled(
                crate::ui::widgets::text::truncate_display(
                    &format!(
                        " {} {text}",
                        match phase {
                            ApplyPhase::Sending | ApplyPhase::Accepted => "⏳",
                            ApplyPhase::Applied => "✔",
                            ApplyPhase::Failed(_) => "✘",
                        }
                    ),
                    cols,
                ),
                th::apply_phase_style(phase),
            )),
        );
    }
    let title = format!("媒体库 · {}", st.filter.label());
    section(f, area, &title, lines);
}

/// 详情栏。
pub fn render_detail(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines = crate::ui::widgets::list::media_detail_lines(st, cols);
    lines.push(text_line("", cols));
    lines.push(disabled_note(
        "缩略图/预览属首版不做：无图像终端同样完全可用",
        cols,
    ));
    section(f, area, "详情", lines);
}
