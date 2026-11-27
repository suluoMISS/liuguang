//! 显示器 Outputs：`outputs` + 变换草稿（`A` 立即应用 / `S` 保存）。

use crate::model::{AppState, FIT_MODES};
use crate::ui::theme as th;
use crate::ui::widgets::{disabled_note, section, section_inner_cols, text_line};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// 主体：输出列表（连接状态/模式/可用模式数）。
pub fn render(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines = Vec::new();
    if st.outputs.outputs.is_empty() {
        lines.push(text_line(
            if st.connection.is_online() {
                " （daemon 未报告任何输出；HeadlessReady）"
            } else {
                " （断线：输出列表可能过期；按 e 重连）"
            },
            cols,
        ));
    }
    for (i, o) in st.outputs.outputs.iter().enumerate() {
        let selected = i == st.outputs_cursor || st.selected_output.as_deref() == Some(&o.name);
        let text = format!(
            "{} {} · {} · {} · {} 个模式",
            if selected {
                th::SELECT_ROW
            } else {
                th::UNSELECTED
            },
            o.name,
            if o.connected {
                "已连接"
            } else {
                "已断开"
            },
            o.mode.as_deref().unwrap_or("-"),
            o.mode_count
        );
        let style = if !o.connected {
            th::muted()
        } else if selected {
            th::selected_row()
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            crate::ui::widgets::text::truncate_display(&text, cols),
            style,
        )));
    }
    lines.push(text_line("", cols));
    lines.push(text_line(
        " C/V 切换 fit · +/- zoom · h/l/K/J 偏移 · X 旋转 · b/B 亮度 · 0 重置",
        cols,
    ));
    lines.push(text_line(
        " A 立即应用（运行时） · S 保存配置（落盘）",
        cols,
    ));
    lines.push(text_line(
        &format!(
            " 支持：{} · 亮度 {}% · 10%–100%",
            FIT_MODES.join(" / "),
            st.transform.brightness_percent()
        ),
        cols,
    ));
    if st.selected_output.is_none() {
        lines.push(disabled_note("尚未选择输出：A/S 会被拒绝并给出提示", cols));
    }
    if !st.outputs.outputs.iter().any(|o| o.connected) {
        lines.push(disabled_note(
            "没有**已连接**的输出：应用会被 daemon 拒绝",
            cols,
        ));
    }
    section(f, area, "显示器", lines);
}
