//! 顶栏 / 底栏：页码上下文与"随页面与焦点变化的快捷键"。
//!
//! - 宽档：顶栏 2 行、底栏 2 行（`docs/TUI_DESIGN.md` §4）；
//! - 中/小档：各 1 行紧凑形式（信息不丢，只做压缩）；
//! - 未保存标记 `*` 必须出现在**顶栏**（2 行内）。

use crate::model::{AppState, Focus, NotificationKind, Page};
use crate::ui::theme as th;
use crate::ui::widgets::text::{display_width, pad_display, truncate_display};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// 一条快捷键提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// 键。
    pub key: String,
    /// 说明。
    pub label: String,
}

impl Hint {
    /// 构造。
    pub fn new(key: &str, label: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
        }
    }

    /// `键 说明` 纯文本形式（不涉及渲染；命名不再借 `render`）。
    pub fn plain(&self) -> String {
        format!("{} {}", self.key, self.label)
    }
}

/// **随页面与焦点变化的快捷键**（底栏内容；单测断言其差异）。
pub fn shortcuts(st: &AppState) -> Vec<Hint> {
    let mut out = Vec::new();
    // 全局（任何页面）
    match st.page {
        Page::Dashboard => {
            out.push(Hint::new("Space", "暂停/继续"));
            out.push(Hint::new("N", "下一张"));
            out.push(Hint::new("R", "重载配置"));
        }
        Page::Library => {
            match st.focus {
                Focus::Detail => {
                    out.push(Hint::new("Esc", "返回列表"));
                    out.push(Hint::new("a", "加入播放列表"));
                    out.push(Hint::new("Y", "复制路径"));
                }
                _ => {
                    out.push(Hint::new("Enter", "详情"));
                    out.push(Hint::new("a", "加入播放列表"));
                    out.push(Hint::new("I", "输入路径"));
                    out.push(Hint::new("/", "搜索"));
                    out.push(Hint::new("F", "筛选"));
                    out.push(Hint::new("PgUp/PgDn", "翻页"));
                    out.push(Hint::new("S", "来源管理"));
                }
            }
            // ⚠️ t58：这里原来有一条 `Hint::new("P", "加入播放列表")`，而**大写 P 全仓
            // 没有绑定**（该页 `p` 是小写的"暂停"）——用户按了没反应。不变量测试
            // `every_advertised_hint_key_is_actually_handled` 直接把它抓了出来。
            // 功能（把媒体库选中项加进播放列表）尚未实现，所以**不宣传**它。
        }
        Page::Playlists => {
            // **t67：主界面只留核心操作**（用户原话："这些有点反人类"——一屏 15 个键
            // 等于没有重点）。曾经的 `m`/`v` 两键组合被页顶的模式选择器取代；
            // 其余旧键仍在 keymap 里可用，但只由 `?` 的高级面板（`page_help`）列出。
            out.push(Hint::new("↑↓", "选择"));
            out.push(Hint::new("←→", "切模式"));
            out.push(Hint::new("Enter", "播放"));
            out.push(Hint::new("a", "加入条目"));
            if st.playlist_edit.is_some() {
                out.push(Hint::new("d", "移除条目"));
                out.push(Hint::new("k/j", "移动"));
                out.push(Hint::new("Esc", "放弃编辑"));
            } else {
                out.push(Hint::new("n", "新建"));
                out.push(Hint::new("e", "编辑"));
            }
        }
        Page::Outputs => {
            out.push(Hint::new("↑↓", "选择输出"));
            out.push(Hint::new("b/B", "亮度"));
            out.push(Hint::new("0", "重置"));
            out.push(Hint::new("X", "旋转"));
        }
        Page::Effects => {
            out.push(Hint::new("[ ]", "效果"));
            out.push(Hint::new("{ }", "缓动"));
            out.push(Hint::new("< >", "时长"));
        }
        Page::System => {
            out.push(Hint::new("↑↓", "滚动"));
        }
        // t70：第 7 页只做两件事，两个键都必须真的绑定（不变量测试会核对）。
        Page::Static => {
            out.push(Hint::new("a", "设为静态壁纸"));
            out.push(Hint::new("c", "清除并恢复播放列表"));
        }
    }
    // 上下文敏感的应用/保存（**语义分离**，只在本页可用时显示）
    match st.page {
        Page::Outputs | Page::Effects => {
            out.push(Hint::new("A", "立即应用"));
            out.push(Hint::new("S", "保存配置"));
        }
        // 媒体库页的 `S` 是**来源管理**（已在上面列出）：这里不能再写"保存配置"，
        // 否则又是一条"提示与键位不符"（t58 不变量测试的同类问题）。
        Page::Library => out.push(Hint::new("A", "立即应用")),
        // 播放列表页的 `A`/`S` 属**高级操作**（t67）：主界面不宣传，`?` 里列出。
        // 主界面靠"编辑中…（按 ? 查看高级操作：A 应用 / S 保存）"这一行指路。

        // 播放列表页：编辑态 `A` = 下发 update_playlist，`S` = 保存配置（两者都真的绑定）。
        // 保存对 System 页同样有意义（`S` 在该页 = 保存配置），但那里没有"立即应用"
        Page::System => out.push(Hint::new("S", "保存配置")),
        _ => {}
    }
    out.push(Hint::new("1-7", "切页"));
    out.push(Hint::new("Tab", "焦点"));
    out.push(Hint::new("?", "帮助"));
    out.push(Hint::new("Q", "退出"));
    out
}

/// 底栏快捷键做成一行文本（必要时按宽度截断）。
pub fn shortcuts_line(st: &AppState, cols: usize) -> String {
    let joined = shortcuts(st)
        .into_iter()
        .map(|h| {
            if h.key == "1-7" || h.key == "Tab" || h.key == "?" || h.key == "Q" {
                format!("{} {}", h.key, h.label)
            } else {
                format!("[{}] {}", h.key, h.label)
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    truncate_display(&joined, cols)
}

/// 宽档顶栏（2 行）：第 1 行标题+连接+实时遥测+暂停胶囊+未保存 `*`；第 2 行输出/状态/解码/模式/运行时长。
pub fn render_top_wide(f: &mut Frame, area: Rect, st: &AppState) {
    if area.height == 0 {
        return;
    }
    let unsaved = if st.dirty { " *" } else { "" };
    let mem_mib = st.metrics.mem_bytes as f64 / (1024.0 * 1024.0);
    let telemetry = format!(
        "{} · CPU {:.1}% · {:.0} MiB",
        crate::model::format_fps_int(f64::from(st.metrics.fps)),
        st.metrics.cpu_percent,
        mem_mib
    );
    let mut spans1 = vec![
        Span::styled(" ✦ 流光 ", th::accent_bold()),
        Span::styled(
            format!("— {}{unsaved} ", st.page.title()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", th::muted()),
        Span::styled(st.connection.label(), th::connection_style(&st.connection)),
        Span::styled(" │ ", th::muted()),
        Span::styled(telemetry, th::muted()),
    ];
    if let Some(reason) = st.active_pause_reason() {
        spans1.push(Span::raw(" "));
        spans1.push(Span::styled(format!(" ⏸ {reason} "), th::chip_warn()));
    }
    let line1 = Line::from(spans1);
    let agent = st
        .pending_ops
        .values()
        .next_back()
        .map(|op| format!("│ {}: {}", op.what, op.state.label()))
        .unwrap_or_default();
    let line2 = Line::from(vec![
        Span::raw(format!(
            " 输出 {} │ 状态 {} │ 解码 {} │ 模式 {} │ 亮度 {}% │ 运行 {}s ",
            st.selected_output.as_deref().unwrap_or("(未选)"),
            st.status.state,
            st.status.decoder,
            st.status.mode.as_deref().unwrap_or("-"),
            st.transform.brightness_percent(),
            st.status.uptime_seconds,
        )),
        Span::styled(agent, th::warn()),
    ]);
    f.render_widget(Paragraph::new(vec![line1, line2]).style(th::bar_bg()), area);
}

/// 窄档紧凑顶栏（1 行）。
///
/// `show_subpage`：小档（单栏）需要显示当前子页与 `←→` 提示。
pub fn render_top_compact(f: &mut Frame, area: Rect, st: &AppState, show_subpage: bool) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let unsaved = if st.dirty { " *" } else { "" };
    let sub = if show_subpage {
        format!(" ▸ [{}] ←→", st.subpage.label())
    } else {
        String::new()
    };
    let text = format!(
        " {} {}{} │ {} │ {}{}{}",
        st.page.hotkey(),
        st.page.title(),
        unsaved,
        st.connection.label(),
        st.selected_output.as_deref().unwrap_or("-"),
        match st.status.decoder.as_str() {
            "none" => "".to_string(),
            d => format!(" │ {d}"),
        },
        sub
    );
    f.render_widget(
        Paragraph::new(truncate_display(&text, area.width as usize)).style(
            if st.connection.is_online() {
                th::bar_bg()
            } else {
                th::offline_bar_bg()
            },
        ),
        area,
    );
}

/// 底栏第 1 行：通知 / 进行中请求 / 断线提示。
pub fn render_bottom_status(f: &mut Frame, area: Rect, st: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let cols = area.width as usize;
    let text = match st.last_notification() {
        Some(n) => {
            let prefix = match n.kind {
                NotificationKind::Info => "ℹ ",
                NotificationKind::Warn => "⚠ ",
                NotificationKind::Error => "✖ ",
            };
            format!(" {prefix}{}", n.text)
        }
        None if !st.connection.is_online() => {
            format!(
                " ✖ {}：数据可能不是最新（页面显示最近一次已知状态）",
                st.connection.label()
            )
        }
        None => {
            let online = st.media.total;
            format!(
                " 媒体 {online} 项 │ {} │ 焦点 {}",
                st.page_label(),
                st.focus.label()
            )
        }
    };
    let style = th::notification_style(st.last_notification().map(|n| n.kind));
    f.render_widget(
        Paragraph::new(truncate_display(&text, cols)).style(style),
        area,
    );
}

/// 底栏第 2 行：快捷键（随页面与焦点变化）。
pub fn render_bottom_keys(f: &mut Frame, area: Rect, st: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let cols = area.width as usize;
    f.render_widget(Paragraph::new(shortcuts_styled_line(st, cols)), area);
}

/// 底栏快捷键的**着色**形式：键位用强调色、说明用默认色。
///
/// `shortcuts_line`（纯文本）保留给测试与紧凑底栏；这里负责视觉层级，
/// 让"能按的键"在底栏里一眼可辨。
pub fn shortcuts_styled_line(st: &AppState, cols: usize) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for h in shortcuts(st) {
        let global = matches!(h.key.as_str(), "1-7" | "Tab" | "?" | "Q");
        let key_text = if global {
            h.key.clone()
        } else {
            format!("[{}]", h.key)
        };
        let need = display_width(&key_text) + 1 + display_width(&h.label) + 2;
        if used + need > cols {
            let remain = cols.saturating_sub(used);
            if remain > 4 {
                let label_cols = remain.saturating_sub(display_width(&key_text) + 1);
                let label = truncate_display(&h.label, label_cols);
                if !label.is_empty() {
                    spans.push(Span::styled(format!("{key_text} "), th::accent()));
                    spans.push(Span::raw(label));
                }
            }
            break;
        }
        spans.push(Span::styled(format!("{key_text} "), th::accent()));
        spans.push(Span::raw(format!("{}  ", h.label)));
        used += need;
    }
    Line::from(spans)
}

/// 紧凑底栏（1 行）：状态 + 关键快捷键。
pub fn render_bottom_compact(f: &mut Frame, area: Rect, st: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let cols = area.width as usize;
    let status = match st.last_notification() {
        Some(n) => n.text.clone(),
        None => format!("{} │ 焦点 {}", st.page_label(), st.focus.label()),
    };
    let keys = shortcuts_line(st, cols.saturating_sub(display_width(&status) + 3));
    let line = format!(" {} │ {keys}", truncate_display(&status, cols / 2));
    f.render_widget(
        Paragraph::new(pad_display(&truncate_display(&line, cols), cols)).style(th::accent()),
        area,
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::Page;

    #[test]
    fn shortcuts_change_with_page() {
        let mut st = AppState::new();
        st.page = Page::Dashboard;
        let dash = shortcuts_line(&st, 200);
        assert!(dash.contains("暂停"));
        assert!(!dash.contains("输入路径"));

        st.page = Page::Library;
        st.focus = Focus::List;
        let lib = shortcuts_line(&st, 200);
        assert!(lib.contains("输入路径"));
        assert!(lib.contains("来源管理"));
        assert!(lib.contains("立即应用"), "媒体库有上下文相关的 A/S");

        st.page = Page::System;
        let sys = shortcuts_line(&st, 200);
        assert!(sys.contains("保存配置"));
        assert!(!sys.contains("立即应用"), "System 页不显示即时应用");
        assert!(!sys.contains("输入路径"));
    }

    #[test]
    fn shortcuts_change_with_focus_inside_same_page() {
        let mut st = AppState::new();
        st.page = Page::Library;
        st.focus = Focus::List;
        let list = shortcuts_line(&st, 200);
        let mut st2 = st.clone();
        st2.focus = Focus::Detail;
        let detail = shortcuts_line(&st2, 200);
        assert_ne!(list, detail, "同一页面不同焦点应给出不同快捷键");
        assert!(detail.contains("返回列表"));
    }

    #[test]
    fn shortcuts_line_is_width_safe() {
        let mut st = AppState::new();
        for page in Page::ALL {
            st.page = page;
            for cols in [10usize, 20, 40, 80, 0] {
                let line = shortcuts_line(&st, cols);
                assert!(
                    display_width(&line) <= cols,
                    "{page:?} cols={cols} 超宽: {line}"
                );
            }
        }
    }

    /// **t67**：播放列表页主提示栏 = **核心操作**（入口 + 新选择器 + 加入/播放）。
    ///
    /// t58 的教训仍然适用：提示栏必须**如实反映 keymap**（假提示 `P` 不得回来）；
    /// t67 又把它收窄成"一屏只看得到重点"——旧键进 `?` 的高级面板。
    #[test]
    fn playlists_page_advertises_the_real_entry_points() {
        let mut st = AppState::new();
        st.page = Page::Playlists;
        st.playlist_edit = None;
        let idle = shortcuts(&st);
        let keys: Vec<&str> = idle.iter().map(|h| h.key.as_str()).collect();
        for want in ["n", "e", "←→", "a", "Enter", "↑↓"] {
            assert!(
                keys.contains(&want),
                "主提示栏必须显示 `{want}`（核心操作），实际 {keys:?}"
            );
        }
        assert!(
            !keys.contains(&"P"),
            "不得再宣传未绑定的 `P`（用户按了没反应），实际 {keys:?}"
        );

        // 编辑态：核心编辑键（`d`/`k/j`/`Esc`），其余进高级面板。
        st.playlist_edit = Some(crate::model::PlaylistEdit::new_default());
        let editing = shortcuts(&st);
        let ekeys: Vec<&str> = editing.iter().map(|h| h.key.as_str()).collect();
        for want in ["a", "d", "k/j", "Esc", "←→", "Enter"] {
            assert!(
                ekeys.contains(&want),
                "编辑态主提示栏必须显示 `{want}`，实际 {ekeys:?}"
            );
        }
        assert!(!ekeys.contains(&"P"), "编辑态同样不得出现 `P`");
    }

    /// **t67**：媒体库页必须提示 `a` 加入播放列表（真的绑定），且不出现假提示 `P`。
    #[test]
    fn library_page_advertises_a_to_join_playlist() {
        let mut st = AppState::new();
        st.page = Page::Library;
        st.focus = Focus::List;
        let keys: Vec<String> = shortcuts(&st).iter().map(|h| h.key.clone()).collect();
        assert!(
            keys.iter().any(|k| k == "a"),
            "媒体库页必须提示 `a` 加入播放列表，实际 {keys:?}"
        );
        assert!(!keys.iter().any(|k| k == "P"), "不得出现未绑定的 `P`");
    }

    /// **t58 同类问题**：媒体库页的 `P`（"加入播放列表"）**同样从未绑定**
    /// （全仓无 `Char('P')` 绑定；该页小写 `p` 是"暂停"）。
    ///
    /// 这条与播放列表页的假提示是**同一个 bug 的两个实例**——不变量测试一次抓出两个。
    #[test]
    fn library_page_does_not_advertise_unbound_uppercase_p() {
        let mut st = AppState::new();
        st.page = Page::Library;
        st.focus = Focus::List;
        let keys: Vec<String> = shortcuts(&st).iter().map(|h| h.key.clone()).collect();
        assert!(
            !keys.iter().any(|k| k == "P"),
            "媒体库不得宣传未绑定的 `P`，实际 {keys:?}"
        );
    }

    /// 媒体库页的 `S` 是**来源管理**（不是保存配置）：提示文案必须如实。
    #[test]
    fn library_page_describes_s_as_source_management() {
        let mut st = AppState::new();
        st.page = Page::Library;
        st.focus = Focus::List;
        let hints = shortcuts(&st);
        let s_labels: Vec<&str> = hints
            .iter()
            .filter(|h| h.key == "S")
            .map(|h| h.label.as_str())
            .collect();
        assert!(
            s_labels.iter().all(|l| *l == "来源管理"),
            "媒体库页 `S` 只能标成“来源管理”，实际 {s_labels:?}"
        );
    }

    #[test]
    fn every_shortcut_has_key_and_label() {
        let mut st = AppState::new();
        for page in Page::ALL {
            st.page = page;
            for f in [Focus::Nav, Focus::List, Focus::Detail, Focus::Actions] {
                st.focus = f;
                for h in shortcuts(&st) {
                    assert!(!h.key.is_empty(), "{page:?}/{f:?} 有空键位");
                    assert!(!h.label.is_empty(), "{page:?}/{f:?} 有空说明");
                }
            }
        }
    }
}
