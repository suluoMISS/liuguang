//! 播放列表 Playlists：真实数据来自 daemon `list_playlists`（编辑属后续阶段）。

use crate::model::{AppState, PlayMode};
use crate::ui::theme as th;
use crate::ui::widgets::{disabled_note, section, section_inner_cols, text_line};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// 主体。
pub fn render(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines = Vec::new();
    // **t67：页顶三选一模式选择器**（←/→ 立即生效）。
    //
    // 治的病：以前"顺序循环/随机播放"要 `m` + `v` 两个键组合表达。
    lines.push(mode_selector_line(st, cols));
    lines.push(text_line("", cols));
    if st.playlists.is_empty() {
        lines.push(text_line(
            if st.connection.is_online() {
                " （daemon 报告 0 个播放列表）"
            } else {
                " （断线：无法读取播放列表）"
            },
            cols,
        ));
    }
    for (i, p) in st.playlists.iter().enumerate() {
        let selected = i == st.playlists_cursor;
        let text = format!(
            "{} {} · {} 项 · {} · {}s{}",
            if selected { "▍" } else { " " },
            p.name,
            p.entries.len(),
            p.mode,
            p.interval_seconds,
            p.output
                .as_ref()
                .map(|o| format!(" · {o}"))
                .unwrap_or_default()
        );
        let style = if selected {
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
    match &st.playlist_edit {
        Some(edit) => {
            lines.push(text_line(
                &format!(
                    " 编辑中：{}{}",
                    edit.name,
                    edit.id
                        .as_ref()
                        .map(|i| format!("（{i}）"))
                        .unwrap_or_else(|| "（新建）".to_string())
                ),
                cols,
            ));
            lines.push(text_line(&format!("   模式 {}", edit.mode), cols));
            lines.push(text_line(
                &format!(
                    "   间隔 {} 秒 · 播完 {}",
                    edit.interval_seconds, edit.on_video_end
                ),
                cols,
            ));
            lines.push(text_line(
                &format!(
                    "   输出 {} · 禁止连续重复 {}",
                    edit.output.as_deref().unwrap_or("(默认)"),
                    if edit.no_repeat { "开" } else { "关" }
                ),
                cols,
            ));
            lines.push(text_line(
                &format!("   条目 {} 项", edit.entries.len()),
                cols,
            ));
            lines.push(text_line(
                "   编辑中：按 ? 查看高级操作（A 应用 / S 保存 / Esc 放弃 / 其余字段）",
                cols,
            ));
        }
        None => {
            lines.push(text_line(
                "   n 新建 · e 编辑选中 · ←→ 切模式 · Enter 播放 · a 加入（媒体库页）",
                cols,
            ));
            lines.push(disabled_note(
                "拖拽排序尚未实现（用 p/d 增删条目；顺序即播放顺序）",
                cols,
            ));
        }
    }
    section(f, area, "播放列表", lines);
}

/// 详情栏。
pub fn render_detail(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    // 键最长「禁止连续重复」（显示宽 12）：统一 12 列对齐
    let kv = |k: &str, v: &str, c: usize| crate::ui::widgets::kv_w(k, v, c, 12);
    let mut lines = Vec::new();
    if let Some(edit) = &st.playlist_edit {
        lines.push(text_line(&format!(" 编辑：{}", edit.name), cols));
        lines.push(kv("ID", edit.id.as_deref().unwrap_or("(新建)"), cols));
        lines.push(kv("条目", &edit.entries.len().to_string(), cols));
        lines.push(kv("模式", &edit.mode, cols));
        lines.push(kv("间隔", &format!("{} 秒", edit.interval_seconds), cols));
        lines.push(kv("播完行为", &edit.on_video_end, cols));
        lines.push(kv("输出", edit.output.as_deref().unwrap_or("(默认)"), cols));
        lines.push(kv(
            "禁止连续重复",
            if edit.no_repeat { "是" } else { "否" },
            cols,
        ));
        lines.push(text_line("", cols));
        for (i, id) in edit.entries.iter().enumerate() {
            let mark = if i == edit.cursor { "▍" } else { " " };
            lines.push(text_line(
                &format!(
                    "{mark}{}",
                    crate::ui::widgets::text::truncate_display(id, cols - 2)
                ),
                cols,
            ));
        }
        lines.push(text_line("", cols));
        lines.push(disabled_note("A = 下发（只改运行时）；S 才写入配置", cols));
        section(f, area, "列表详情", lines);
        return;
    }
    match st.playlists.get(st.playlists_cursor) {
        Some(p) => {
            lines.push(text_line(&format!(" {}", p.name), cols));
            lines.push(kv("ID", &p.id, cols));
            lines.push(kv("条目", &p.entries.len().to_string(), cols));
            lines.push(kv("模式", &p.mode, cols));
            lines.push(kv("间隔", &format!("{} 秒", p.interval_seconds), cols));
            lines.push(kv("播完行为", &p.on_video_end, cols));
            lines.push(kv("输出", p.output.as_deref().unwrap_or("(默认)"), cols));
        }
        None => lines.push(text_line(" （未选中）", cols)),
    }
    lines.push(text_line("", cols));
    lines.push(disabled_note(
        "本阶段只展示 daemon 已知的列表，不写配置",
        cols,
    ));
    section(f, area, "列表详情", lines);
}

/// 页顶"播放模式"选择器（t67）。
///
/// 当前项高亮 `[ … ]`；没有播放列表时给出**可操作**指引（不静默）。
/// 宽度不够时退化成纯文本截断（不溢出、不 panic）。
fn mode_selector_line(st: &AppState, cols: usize) -> Line<'static> {
    let current = st.playlist_mode();
    let mut plain = String::from(" 播放模式: ");
    for (i, m) in PlayMode::ALL.iter().enumerate() {
        if i > 0 {
            plain.push_str("  ");
        }
        if current == Some(*m) {
            plain.push_str(&format!("[ {} ]", m.label()));
        } else {
            plain.push_str(m.label());
        }
    }
    match current {
        None => plain.push_str("   （还没有播放列表：按 n 新建）"),
        Some(_) => plain.push_str("   ←→ 切换"),
    }
    if crate::ui::widgets::text::display_width(&plain) > cols {
        return text_line(&plain, cols);
    }
    let mut spans: Vec<Span<'static>> = vec![Span::styled(" 播放模式: ".to_string(), th::muted())];
    for (i, m) in PlayMode::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if current == Some(*m) {
            spans.push(Span::styled(
                format!("[ {} ]", m.label()),
                th::selected_row(),
            ));
        } else {
            spans.push(Span::styled(m.label().to_string(), th::text()));
        }
    }
    spans.push(Span::styled(
        if current.is_some() {
            "   ←→ 切换".to_string()
        } else {
            "   （还没有播放列表：按 n 新建）".to_string()
        },
        if current.is_some() {
            th::muted()
        } else {
            th::warn()
        },
    ));
    Line::from(spans)
}
