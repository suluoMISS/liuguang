//! 模态框：路径输入（Unicode 安全编辑器 + 光标）、帮助、来源管理、确认。
//!
//! 弹窗**捕获焦点**（事件层保证），关闭后恢复原焦点（事件层保证）；
//! 这里只负责绘制，并且保证任何尺寸下都不越界。

use crate::model::{AppState, Modal};
use crate::ui::theme as th;
use crate::ui::widgets::text::{display_width, truncate_display, wrap_display};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// 光标显示的最小列宽（一个反显块）。
const CURSOR_COLS: usize = 1;

/// 计算弹窗矩形：居中、不超过终端、至少 1x1（不产生负宽度）。
pub fn modal_rect(area: Rect, wanted_h: u16) -> Rect {
    let width = (u32::from(area.width) * 3 / 4).clamp(1, 88) as u16;
    let width = width.min(area.width).max(1).min(area.width.max(1));
    let height = wanted_h.min(area.height).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 3,
        width: if area.width == 0 { 0 } else { width },
        height: if area.height == 0 { 0 } else { height },
    }
}

/// 必需高度（按内容估算）。
pub fn modal_height(modal: &Modal) -> u16 {
    match modal {
        Modal::PathInput { .. } => 10,
        // t67：帮助页现在还要承载"高级操作"分区（播放列表页共 ~25 条），
        // 20 行会把后半截（重命名 / A / S）截掉 ⇒ 用户看不到却以为没有。
        // 取值会再被 `min(area.height)` 夹住，小终端不致溢出。
        Modal::Help => 30,
        Modal::SourceManage => 12,
        Modal::PathText { text, .. } => (4 + text.lines().count() as u16).clamp(5, 16),
        Modal::TextInput { hint, .. } => (5 + u16::from(!hint.is_empty())).clamp(5, 8),
        Modal::Confirm { .. } => 7,
    }
}

/// 编辑行：返回 `(光标前的可见片段, 光标处字符, 光标后的可见片段)`。
///
/// 保证：整体显示宽度 ≤ `cols`；光标一定可见；**不切断任何 `char`**。
pub fn visible_window(text: &str, cursor: usize, cols: usize) -> (String, Option<char>, String) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let budget = cols.saturating_sub(CURSOR_COLS);
    // 先向前收集（光标左侧），最多用满预算
    let mut before: Vec<char> = Vec::new();
    let mut used = 0usize;
    for i in (0..cursor).rev() {
        let w = unicode_width::UnicodeWidthChar::width(chars[i]).unwrap_or(0);
        if used + w > budget {
            break;
        }
        before.push(chars[i]);
        used += w;
    }
    before.reverse();
    // 再向后收集剩余预算
    let mut after: Vec<char> = Vec::new();
    for c in chars.iter().skip(cursor + 1) {
        let w = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        after.push(*c);
        used += w;
    }
    (
        before.into_iter().collect(),
        chars.get(cursor).copied(),
        after.into_iter().collect(),
    )
}

/// 输入框内容行（含反显光标）。
pub fn editor_line(st: &AppState, cols: usize) -> Line<'static> {
    let text = st.editor.text();
    // 先组装（含光标），再整体按显示宽度裁剪：保证任何 cols 下都不超宽
    let (before, at, after) = visible_window(&text, st.editor.cursor(), cols.max(1));
    let head = truncate_display(&format!(" > {before}"), cols.saturating_sub(CURSOR_COLS));
    let used = display_width(&head);
    let remain = cols.saturating_sub(used + CURSOR_COLS);
    let tail = truncate_display(&after, remain);
    Line::from(vec![
        Span::styled(head, th::muted()),
        Span::styled(
            match at {
                Some(c) => c.to_string(),
                None => " ".to_string(),
            },
            th::cursor(),
        ),
        Span::raw(tail),
    ])
}

/// 绘制当前模态框（无模态框时不绘制）。
pub fn render_modal(f: &mut Frame, area: Rect, st: &AppState) {
    let Some(modal) = st.modal.clone() else {
        return;
    };
    if area.width < 4 || area.height < 3 {
        return; // 太小就整体不画（由 Tiny 档兜底），不尝试挤
    }
    let rect = modal_rect(area, modal_height(&modal));
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(th::accent())
        .style(th::panel_bg())
        .title(format!(" {} ", modal.title()));
    let inner_cols = rect.width.saturating_sub(2) as usize;
    let lines = modal_lines(&modal, st, inner_cols);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        rect,
    );
}

/// 模态框内容行（与渲染分离，便于单测）。
///
/// **保证**：返回的每一行显示宽度都 ≤ `cols`（用 [`clamp_lines`] 统一收口，
/// 新增内容时不会再漏掉宽度安全）。
pub fn modal_lines(modal: &Modal, st: &AppState, cols: usize) -> Vec<Line<'static>> {
    let lines = match modal {
        Modal::PathInput { purpose } => {
            let mut lines = vec![
                Line::from(Span::styled(format!(" 用途：{purpose}"), th::muted())),
                Line::from(Span::raw(" 服务器绝对路径（例：/srv/wallpapers/a.png）")),
                editor_line(st, cols),
                Line::from(""),
                Line::from(Span::styled(
                    " ←→ 移动 · Home/End 行首尾 · Backspace/Delete 删除",
                    th::muted(),
                )),
                Line::from(Span::styled(
                    " Enter 提交（仅提交给 daemon；路径校验由 daemon 执行）",
                    th::muted(),
                )),
                Line::from(Span::styled(
                    " Esc 取消（不写入配置、不改变当前壁纸）",
                    th::muted(),
                )),
            ];
            // 形状提示（不是安全边界，只帮助用户少犯低级错误）
            let text = st.editor.text();
            if !text.is_empty() {
                let note = if crate::event::is_absolute_server_path(&text) {
                    (" 形状检查：服务器绝对路径 ✓", th::OK)
                } else if text.starts_with("http") {
                    (
                        " 形状检查：这是 URL，不是本机路径 ✗（首版只支持本机文件）",
                        th::ERROR,
                    )
                } else {
                    (" 形状检查：需要以 / 开头的绝对路径 ✗", th::WARN)
                };
                lines.push(Line::from(Span::styled(
                    truncate_display(note.0, cols),
                    Style::default().fg(note.1),
                )));
            }
            lines
        }
        Modal::Help => {
            // **唯一来源**：`event::page_help`（按页键位 + 全局键），
            // 避免"两套 help 文本共存、改一处忘一处"（t24 去重）。
            let mut lines = vec![Line::from(Span::styled(
                truncate_display(
                    &format!(
                        " 当前页：{}（键位与 docs/TUI_DESIGN.md §6 一致；各页不冲突）",
                        st.page.title()
                    ),
                    cols,
                ),
                th::muted(),
            ))];
            for (k, v) in crate::event::page_help(st) {
                let keytext = truncate_display(&format!("  {}", pad(k, 16)), cols);
                let used = display_width(&keytext);
                let value = if used + 1 < cols {
                    truncate_display(v, cols - used - 1)
                } else {
                    String::new()
                };
                lines.push(Line::from(vec![
                    Span::styled(keytext, th::accent_bold()),
                    Span::raw(value),
                ]));
            }
            lines
        }
        Modal::SourceManage => {
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    " 媒体来源（daemon 权威；root={}，缺失 {}）",
                    truncate_display(&st.sources.root, cols.saturating_sub(28)),
                    st.sources.missing
                ),
                th::muted(),
            ))];
            if st.sources.sources.is_empty() {
                lines.push(Line::from("  （daemon 未配置来源；需 [library] 段）"));
            } else {
                for s in &st.sources.sources {
                    lines.push(Line::from(format!(
                        "  {} {} ({}) {}",
                        if s.available { "●" } else { "○" },
                        truncate_display(&s.name, cols.saturating_sub(20)),
                        s.kind,
                        if s.available { "" } else { "缺失" }
                    )));
                }
            }
            lines.push(Line::from(format!(
                " 新增将用：类型 {}（k 切换）· 递归 {}（r 切换）",
                st.source_kind,
                if st.source_recursive { "是" } else { "否" }
            )));
            lines.push(Line::from(Span::styled(
                " a 新增（服务器绝对路径，daemon 校验在允许根内） · d 删除 · R 重扫",
                th::muted(),
            )));
            lines.push(Line::from(Span::styled(
                " s 保存到配置（**需确认**） · Esc 关闭（运行时改动不会落盘）",
                th::muted(),
            )));
            if st.sources_dirty {
                lines.push(Line::from(Span::styled(
                    " ⚠ 有未保存的来源改动（只在运行时生效）",
                    th::warn(),
                )));
            }
            lines.push(Line::from(Span::styled(
                match &st.scan {
                    crate::model::ScanState::Finished { scan_id, found } => {
                        format!(" 上次扫描：{found} 项（scan {scan_id}）· 再按 R 重扫")
                    }
                    other => format!(" 扫描状态：{}", other.label()),
                },
                th::muted(),
            )));
            lines
        }
        Modal::TextInput { hint, .. } => {
            let mut lines = Vec::new();
            if !hint.is_empty() {
                lines.push(Line::from(Span::styled(format!(" {hint}"), th::muted())));
            }
            lines.push(editor_line(st, cols));
            lines.push(Line::from(Span::styled(
                " ←→ 移动 · Home/End 行首尾 · Backspace/Delete 删除",
                th::muted(),
            )));
            lines.push(Line::from(Span::styled(
                " Enter 提交 · Esc 取消（不写配置）",
                th::muted(),
            )));
            lines
        }
        Modal::PathText { text, .. } => {
            let mut lines = Vec::new();
            lines.push(Line::from(Span::styled(
                " 终端不支持 OSC52：请手动选择复制（文本完整，未截断）",
                th::warn(),
            )));
            for l in wrap_display(text, cols.max(8)) {
                lines.push(Line::from(l));
            }
            lines.push(Line::from(Span::styled(" Esc / Enter 关闭", th::muted())));
            lines
        }
        Modal::Confirm { question, .. } => {
            let mut lines = Vec::new();
            for l in wrap_display(question, cols.max(8)) {
                lines.push(Line::from(format!("  {l}")));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Enter 确认 · Esc 取消",
                th::muted(),
            )));
            lines
        }
    };
    clamp_lines(lines, cols)
}

/// 把每一行按显示宽度裁剪到 `cols`（多余 span 直接丢弃，不产生负宽度）。
fn clamp_lines(lines: Vec<Line<'static>>, cols: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let mut used = 0usize;
            let mut spans: Vec<Span<'static>> = Vec::new();
            for span in line.spans {
                if used >= cols {
                    break;
                }
                let content = span.content.to_string();
                if content.is_empty() {
                    continue;
                }
                let budget = cols - used;
                let clipped = truncate_display(&content, budget);
                let width = display_width(&clipped);
                if width == 0 {
                    break;
                }
                used += width;
                spans.push(Span::styled(clipped, span.style));
            }
            Line::from(spans)
        })
        .collect()
}

fn pad(s: &str, cols: usize) -> String {
    let w = display_width(s);
    if w >= cols {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(cols - w))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::Modal;
    use ratatui::style::Color;

    fn path_state(text: &str) -> AppState {
        let mut st = AppState::new();
        st.open_modal(Modal::PathInput {
            purpose: "设为壁纸".to_string(),
        });
        st.editor.set_text(text);
        st
    }

    #[test]
    fn modal_rect_is_inside_area_and_never_zero() {
        for (w, h) in [
            (140u16, 40u16),
            (100, 30),
            (70, 22),
            (50, 15),
            (10, 4),
            (0, 0),
            (1, 1),
        ] {
            let area = Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            };
            for want in [6u16, 8, 20, 60] {
                let r = modal_rect(area, want);
                assert!(
                    r.x.saturating_add(r.width) <= w,
                    "{w}x{h} want={want} {r:?}"
                );
                assert!(
                    r.y.saturating_add(r.height) <= h,
                    "{w}x{h} want={want} {r:?}"
                );
            }
        }
    }

    #[test]
    fn editor_line_shows_cursor_and_text() {
        let st = path_state("/srv/中文.png");
        let line = editor_line(&st, 40);
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("/srv/中文.png"), "{text}");
        // 光标列必须有一个反显块
        let cursor = line
            .spans
            .iter()
            .find(|s| s.style.bg == Some(Color::White))
            .expect("应存在光标块");
        assert!(display_width(cursor.content.as_ref()) <= 1);
    }

    #[test]
    fn editor_line_keeps_cursor_visible_for_long_text() {
        let long = format!("/srv/wallpapers/{}", "很长的目录名".repeat(8));
        let st = path_state(&long);
        for cols in [10usize, 20, 40] {
            let line = editor_line(&st, cols);
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(
                display_width(&text) <= cols.max(4),
                "cols={cols} 超宽: {text}"
            );
            assert!(
                line.spans.iter().any(|s| s.style.bg == Some(Color::White)),
                "cols={cols} 光标必须可见"
            );
            assert!(!text.contains('\u{FFFD}'));
        }
    }

    #[test]
    fn visible_window_never_splits_chars() {
        let text = "中文abc混合";
        for cursor in 0..=text.chars().count() {
            for cols in [1usize, 2, 3, 5, 8] {
                let (b, c, a) = visible_window(text, cursor, cols);
                let joined: String = format!("{b}{}{a}", c.map(String::from).unwrap_or_default());
                assert!(
                    text.contains(&joined.replace(' ', ""))
                        || joined.is_empty()
                        || text.contains(&joined)
                );
                let width = display_width(&b) + CURSOR_COLS + display_width(&a);
                assert!(width <= cols.max(1), "cols={cols} 窗口超宽 {width}");
            }
        }
    }

    #[test]
    fn path_input_modal_lines_contain_purpose_and_editor() {
        let st = path_state("/srv/城市.png");
        let lines = modal_lines(
            &Modal::PathInput {
                purpose: "设为壁纸".to_string(),
            },
            &st,
            80,
        );
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("设为壁纸"), "{text}");
        assert!(text.contains("/srv/城市.png"), "{text}");
        assert!(text.contains("Esc 取消"), "{text}");
        assert!(
            text.contains("路径校验由 daemon 执行"),
            "必须写明校验责任在 daemon：{text}"
        );
    }

    #[test]
    fn help_is_page_aware_and_keys_do_not_conflict() {
        // `?` 帮助必须**按页**列出键位（规范 §6：各页不冲突）
        let mut st = AppState::new();
        let mut help_for = |page: crate::model::Page| -> String {
            st.goto(page);
            modal_lines(&Modal::Help, &st, 120)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let library = help_for(crate::model::Page::Library);
        let playlists = help_for(crate::model::Page::Playlists);
        let outputs = help_for(crate::model::Page::Outputs);
        let effects = help_for(crate::model::Page::Effects);
        let system = help_for(crate::model::Page::System);
        let dashboard = help_for(crate::model::Page::Dashboard);

        // 每页都声明自己
        assert!(library.contains("媒体库"), "{library}");
        assert!(playlists.contains("播放列表"), "{playlists}");
        assert!(system.contains("系统"), "{system}");

        // 本页专属键位出现
        assert!(
            library.contains("输入路径") && library.contains("来源管理"),
            "{library}"
        );
        assert!(
            playlists.contains("重命名") && playlists.contains("轮换间隔"),
            "{playlists}"
        );
        assert!(
            outputs.contains("fit") && outputs.contains("旋转"),
            "{outputs}"
        );
        assert!(
            effects.contains("试用") && effects.contains("时长"),
            "{effects}"
        );
        assert!(system.contains("Re-connect"), "{system}");
        assert!(dashboard.contains("下一张"), "{dashboard}");

        // 别页专属键位不得出现（各页不冲突）
        assert!(
            !library.contains("重命名"),
            "媒体库帮助不应含播放列表键位：\n{library}"
        );
        assert!(
            !playlists.contains("输入路径"),
            "播放列表帮助不应含媒体库键位：\n{playlists}"
        );
        assert!(
            !system.contains("输入路径") && !system.contains("重命名"),
            "{system}"
        );
        assert!(
            !effects.contains("fit"),
            "转场帮助不应含显示器键位：\n{effects}"
        );

        // 全局键每页都有，且写明退出不停 daemon
        for (name, page_help) in [
            ("dashboard", &dashboard),
            ("library", &library),
            ("playlists", &playlists),
            ("outputs", &outputs),
            ("effects", &effects),
            ("system", &system),
        ] {
            for needle in ["1-7", "Tab", "?", "Q"] {
                assert!(
                    page_help.contains(needle),
                    "{name} 缺少全局键 {needle}：\n{page_help}"
                );
            }
            assert!(
                page_help.contains("不停 daemon"),
                "{name} 应写明退出不停 daemon：\n{page_help}"
            );
        }
    }

    #[test]
    fn source_manage_lists_sources_and_marks_readonly() {
        let mut st = AppState::new();
        st.sources.root = "/srv/wallpapers".into();
        st.sources.sources = vec![liuguang_ipc::protocol::MediaSource {
            id: "s1".into(),
            name: "images".into(),
            kind: "image".into(),
            available: true,
        }];
        let text: String = modal_lines(&Modal::SourceManage, &st, 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("images"));
        assert!(text.contains("扫描状态"));
    }

    #[test]
    fn modal_lines_are_width_safe() {
        let mut st = path_state("/srv/城市夜景.png");
        st.sources.root = "/srv/一个非常长的中文目录名字".into();
        st.sources.sources = vec![liuguang_ipc::protocol::MediaSource {
            id: "s1".into(),
            name: "一个非常长的中文目录名字".into(),
            kind: "image".into(),
            available: false,
        }];
        for cols in [8usize, 16, 40, 88] {
            for modal in [
                Modal::PathInput {
                    purpose: "设为壁纸".into(),
                },
                Modal::Help,
                Modal::SourceManage,
                Modal::Confirm {
                    question: "确定要把这个非常长的中文文件名设为壁纸吗？".into(),
                    action: "x".into(),
                },
                Modal::PathText {
                    title: "复制路径".into(),
                    text: "/srv/wallpapers/城市夜景_最终版.png".into(),
                },
            ] {
                for line in modal_lines(&modal, &st, cols) {
                    let text = line.to_string();
                    assert!(
                        display_width(&text) <= cols.max(8) + 8,
                        "cols={cols} {modal:?} 行超宽({}): {text}",
                        display_width(&text)
                    );
                }
            }
        }
    }
}
