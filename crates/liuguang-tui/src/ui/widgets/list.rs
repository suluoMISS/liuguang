//! 媒体列表与详情（宽/中/小档共用；只读 [`AppState`]）。
//!
//! 列表**只渲染当前页**（分页在 daemon 侧完成，10k 条也不会拖慢渲染）；
//! 文件名按**显示宽度**截断，完整路径只在详情面板展示（换行不丢字符）。

use crate::model::{AppState, is_video, size_label};
use crate::ui::theme as th;
use crate::ui::widgets::text::{display_width, truncate_display, wrap_display};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// 选中行标记。
pub const SELECT_MARKER: &str = th::SELECT_ROW;

/// 列表表头（筛选/排序/搜索/页码/扫描状态）。
pub fn list_header(st: &AppState, cols: usize) -> String {
    let search = if st.search.is_empty() {
        String::new()
    } else {
        format!(" 搜索「{}」", st.search)
    };
    let scanning = if st.media.scanning {
        " · 正在扫描…"
    } else {
        ""
    };
    let truncated = if st.media.truncated {
        " · 已截断（服务端上限）"
    } else {
        ""
    };
    truncate_display(
        &format!(
            "筛选 {} · 排序 {} · {} · 共 {} 项{search}{scanning}{truncated}",
            st.filter.label(),
            st.sort.label(),
            st.page_label(),
            st.media.total
        ),
        cols,
    )
}

/// 列表正文行。
pub fn media_lines(st: &AppState, rows: usize, cols: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(list_header(st, cols), th::muted())));

    if !st.connection.is_online() {
        out.push(Line::from(Span::styled(
            format!(
                " ⚠ {} — 显示最近一次已知数据（不是实时）",
                st.connection.label()
            ),
            th::error_bold(),
        )));
    }
    if st.media.items.is_empty() {
        out.push(Line::from(Span::styled(
            if st.connection.is_online() {
                if st.media.scanning {
                    " （正在扫描，稍后按 R 或等待 SCAN_FINISHED 事件）"
                } else {
                    " （空）没有匹配的媒体；按 I 输入服务器绝对路径，或 R 重扫来源"
                }
            } else {
                " （断线）无法读取媒体列表；按 e 重连"
            },
            th::warn(),
        )));
        return out;
    }

    let footer_rows = usize::from(st.page_count() > 1);
    let body_rows = rows.saturating_sub(1 + usize::from(!st.connection.is_online()) + footer_rows);
    let start = st.media_cursor.saturating_sub(body_rows.saturating_sub(1));
    for idx in start..st.media.items.len().min(start + body_rows.max(1)) {
        let item = &st.media.items[idx];
        let selected = idx == st.media_cursor;
        let active = st.is_media_active(item);
        let marker = if selected { SELECT_MARKER } else { " " };
        let abs_idx = st.media.page * st.media.page_size + idx + 1;
        let idx_badge = format!("{abs_idx:02}");
        let live_badge = if active { "● " } else { "" };
        let fps_badge = item
            .details
            .as_ref()
            .and_then(|d| d.fps_millihz)
            .filter(|&mhz| mhz > 0)
            .map(|mhz| format!(" ▶{}", (f64::from(mhz) / 1000.0).round() as u32))
            .unwrap_or_default();
        let kind = if is_video(item) { "视频" } else { "图片" };
        let size = size_label(item.size_bytes);
        let suffix = format!("{kind}{fps_badge} · {size}");
        let prefix = format!("{marker}{idx_badge} {live_badge}");
        let name_cols = cols.saturating_sub(display_width(&prefix) + display_width(&suffix) + 2);
        let text = format!(
            "{prefix}{} {suffix}",
            truncate_display(&item.name, name_cols.max(4))
        );
        let style = if selected {
            th::selected_row()
        } else if active {
            th::ok_bold()
        } else if is_video(item) {
            th::video()
        } else {
            Style::default()
        };
        out.push(Line::from(Span::styled(
            truncate_display(&text, cols),
            style,
        )));
    }
    if st.page_count() > 1 {
        out.push(Line::from(Span::styled(
            truncate_display(
                &format!(
                    " 第 {}–{} 项 / 共 {} 项 · PgUp/PgDn 翻页",
                    st.media.page * st.media.page_size + 1,
                    st.media.page * st.media.page_size + st.media.items.len(),
                    st.media.total
                ),
                cols,
            ),
            th::muted(),
        )));
    }
    out
}

/// 渲染媒体列表。
pub fn render_media_list(f: &mut Frame, area: Rect, st: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = media_lines(st, area.height as usize, area.width as usize);
    f.render_widget(Paragraph::new(lines), area);
}

/// 选中项的详情（完整路径**不截断**、按显示宽度换行；含正在使用/兼容性/元数据面板）。
pub fn media_detail_lines(st: &AppState, cols: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let Some(item) = st.selected_media() else {
        out.push(Line::from(Span::styled(" （无选中项）", th::muted())));
        return out;
    };
    out.push(Line::from(Span::styled(
        truncate_display(&item.name, cols),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    out.push(Line::from(Span::styled(
        format!(
            "{} · {} · 修改 {}",
            if is_video(item) { "视频" } else { "图片" },
            size_label(item.size_bytes),
            item.modified_unix
        ),
        th::muted(),
    )));
    let active = st.is_media_active(item);
    let mut chips: Vec<Span<'static>> = Vec::new();
    if active {
        chips.push(Span::styled(" ● 正在使用 ", th::chip_live()));
    }
    let detail_item = st
        .detail
        .as_ref()
        .filter(|d| d.id == item.id)
        .unwrap_or(item);
    if let Some(compat) = st
        .detail_compat
        .as_ref()
        .filter(|_| st.detail.as_ref().is_some_and(|d| d.id == item.id))
    {
        if !chips.is_empty() {
            chips.push(Span::raw(" "));
        }
        match compat.state {
            liuguang_ipc::protocol::CompatState::Playable => {
                chips.push(Span::styled(" ✓ 可直接播放 ", th::chip_live()));
            }
            liuguang_ipc::protocol::CompatState::NeedsConversion => {
                chips.push(Span::styled(" ⚠ 需转码 ", th::chip_warn()));
            }
            liuguang_ipc::protocol::CompatState::Unsupported => {
                chips.push(Span::styled(" ✘ 不支持 ", th::chip_error()));
            }
        }
    }
    if !chips.is_empty() {
        out.push(Line::from(chips));
    }
    if let Some(compat) = st
        .detail_compat
        .as_ref()
        .filter(|_| st.detail.as_ref().is_some_and(|d| d.id == item.id))
        && matches!(
            compat.state,
            liuguang_ipc::protocol::CompatState::NeedsConversion
                | liuguang_ipc::protocol::CompatState::Unsupported
        )
    {
        if let Some(reason) = compat.reason.as_deref() {
            out.push(Line::from(Span::styled(
                truncate_display(&format!(" 原因：{reason}"), cols),
                th::warn(),
            )));
        }
        if let Some(target) = compat.target.as_deref() {
            out.push(Line::from(Span::styled(
                truncate_display(&format!(" 目标：{target}"), cols),
                th::muted(),
            )));
        }
    }
    if let Some(d) = detail_item.details.as_ref() {
        out.push(Line::from(""));
        let fps = d
            .fps_millihz
            .map(|mhz| crate::model::format_fps_int(f64::from(mhz) / 1000.0))
            .unwrap_or_else(|| "—".to_string());
        let dur = d
            .duration_ms
            .map(|ms| format!("{}s", ms / 1000))
            .unwrap_or_else(|| "—".to_string());
        out.push(Line::from(Span::styled(
            truncate_display(
                &format!(
                    "规格：{}×{} · {} · 时长 {dur} · {} · {}",
                    d.width,
                    d.height,
                    fps,
                    d.codec.as_deref().unwrap_or("—"),
                    d.pix_fmt.as_deref().unwrap_or("—"),
                ),
                cols,
            ),
            th::text(),
        )));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        "服务器绝对路径（不截断，按宽度换行）：",
        th::muted(),
    )));
    for l in wrap_display(&item.path, cols.max(8)) {
        out.push(Line::from(l));
    }
    out.push(Line::from(""));
    match (&st.detail, &st.detail_error) {
        (Some(d), _) if d.id == item.id => {
            out.push(Line::from(Span::styled(
                format!("完整元数据：id={} source={}", d.id, d.source_id),
                th::ok(),
            )));
        }
        (_, Some(err)) => out.push(Line::from(Span::styled(
            format!("元数据获取失败：{err}"),
            th::error(),
        ))),
        _ => out.push(Line::from(Span::styled(
            "（完整元数据待取：仅选中项异步获取）",
            th::muted(),
        ))),
    }
    out
}

/// 渲染详情面板。
pub fn render_media_detail(f: &mut Frame, area: Rect, st: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = media_detail_lines(st, area.width.saturating_sub(2) as usize);
    f.render_widget(
        Paragraph::new(lines).style(th::panel_bg()).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(th::border())
                .style(th::panel_bg()),
        ),
        area,
    );
}

/// 输出详情（Outputs 页右栏）。
pub fn render_output_detail(f: &mut Frame, area: Rect, st: &AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "输出与实时适配",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    match st.selected_output.clone() {
        None => lines.push(Line::from(" （未选输出；按 4 到显示器页用 ↑↓ 选择）")),
        Some(name) => {
            let out = st.outputs.outputs.iter().find(|o| o.name == name);
            match out {
                Some(o) => {
                    lines.push(Line::from(format!(
                        " {name} · {} · 模式 {} · {} 个可用模式",
                        if o.connected {
                            "已连接"
                        } else {
                            "已断开"
                        },
                        o.mode.as_deref().unwrap_or("-"),
                        o.mode_count
                    )));
                    if let Some((w, h, hz)) = parse_mode(o.mode.as_deref()) {
                        lines.push(Line::from(format!(
                            " 分辨率 {w}x{h} @ {hz} Hz（daemon 原始模式串解析）"
                        )));
                    }
                }
                None => lines.push(Line::from(format!(" {name}（daemon 已不再报告该输出）"))),
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "实时画质与适配（A 立即应用 / S 保存 / 0 重置）",
                th::muted(),
            )));
            lines.push(Line::from(format!(
                "  fit      = {} ({})",
                crate::model::fit_label(&st.transform.fit),
                st.transform.fit
            )));
            lines.push(Line::from(format!(
                "  zoom     = {:.2}×",
                st.transform.zoom
            )));
            lines.push(Line::from(format!(
                "  offset   = ({:.0}, {:.0})",
                st.transform.offset_x, st.transform.offset_y
            )));
            lines.push(Line::from(format!(
                "  rotation = {}°  (X 旋转)",
                st.transform.rotation
            )));
            lines.push(Line::from(vec![
                Span::raw("  亮度     = "),
                Span::styled(th::progress_bar(st.transform.brightness, 10), th::accent()),
                Span::raw(format!(" {}% (b/B)", st.transform.brightness_percent())),
            ]));
            lines.push(Line::from(Span::styled(
                "  支持：裁切填充 / 完整显示 / 拉伸铺满 / 居中原始",
                th::muted(),
            )));
            if st.dirty {
                lines.push(Line::from(Span::styled(
                    "  * 有未保存改动（S 才会写入配置）",
                    th::warn(),
                )));
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(th::panel_bg()).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(th::border())
                .style(th::panel_bg()),
        ),
        area,
    );
}

/// 解析 `WxH@R.RRR`（daemon 模式串）。
pub fn parse_mode(mode: Option<&str>) -> Option<(u32, u32, String)> {
    let mode = mode?;
    let (wh, hz) = mode.split_once('@')?;
    let (w, h) = wh.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, hz.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::MEDIA_PAGE_SIZE;
    use crate::ui::widgets::text::display_width;
    use liuguang_ipc::protocol::MediaItem;

    fn item(i: usize, name: &str) -> MediaItem {
        MediaItem {
            id: format!("/srv/{i}"),
            source_id: "s".into(),
            name: name.to_string(),
            path: format!("/srv/wallpapers/收藏/{name}"),
            kind: if i % 3 == 2 {
                "video".into()
            } else {
                "image".into()
            },
            size_bytes: 1024 * 1024,
            modified_unix: 1_760_000_000,
            details: None,
        }
    }

    fn state(n: usize, name: &str) -> AppState {
        let mut st = AppState::new();
        st.media.items = (0..n)
            .map(|i| item(i, &format!("{name}_{i}.png")))
            .collect();
        st.media.total = n;
        st.media.page_size = MEDIA_PAGE_SIZE;
        st
    }

    #[test]
    fn empty_library_is_actionable() {
        let st = state(0, "x");
        let text: String = media_lines(&st, 10, 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("空"));
        assert!(text.contains("输入服务器绝对路径"));
    }

    #[test]
    fn scanning_state_is_visible() {
        let mut st = state(0, "x");
        st.media.scanning = true;
        let text: String = media_lines(&st, 10, 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("正在扫描"), "{text}");
    }

    #[test]
    fn long_chinese_names_are_width_safe() {
        let st = state(
            3,
            "城市夜景_最终版_3840x2400_第三批修订_请勿删除_很长很长的文件名",
        );
        for cols in [20usize, 30, 40, 60] {
            for line in media_lines(&st, 10, cols) {
                let s = line.to_string();
                assert!(
                    display_width(&s) <= cols.max(4) + 2,
                    "cols={cols} 超宽: {s}"
                );
                assert!(!s.contains('\u{FFFD}'));
            }
        }
    }

    #[test]
    fn selected_row_marked_and_styled() {
        let mut st = state(5, "a");
        st.media_cursor = 2;
        let lines = media_lines(&st, 10, 80);
        let body: Vec<&Line> = lines
            .iter()
            .filter(|l| l.to_string().starts_with(SELECT_MARKER))
            .collect();
        assert_eq!(body.len(), 1);
        assert!(body[0].to_string().contains("a_2.png"));
    }

    #[test]
    fn offline_state_visible() {
        let mut st = state(3, "a");
        st.connection = crate::model::Connection::Offline("boom".into());
        let text: String = media_lines(&st, 10, 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("断线"));
        assert!(text.contains("最近一次"));
    }

    #[test]
    fn paging_hint_only_when_needed() {
        let mut st = state(10, "a");
        let text: String = media_lines(&st, 40, 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("PgUp/PgDn"));
        st.media.total = 10_000;
        let text: String = media_lines(&st, 40, 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("PgUp/PgDn"));
        assert!(text.contains("共 10000 项"));
    }

    #[test]
    fn detail_keeps_full_path() {
        let st = state(
            1,
            "城市夜景_最终版_3840x2400_第三批修订_请勿删除_很长很长的文件名.png",
        );
        let expected = st.media.items[0].path.clone();
        for cols in [20usize, 40, 100] {
            let joined: String = media_detail_lines(&st, cols)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("");
            let compact: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
            for c in expected.chars().filter(|c| !c.is_whitespace()) {
                assert!(compact.contains(c), "cols={cols} 丢失 {c:?}");
            }
        }
    }

    #[test]
    fn detail_shows_metadata_state() {
        let mut st = state(1, "a.png");
        let text: String = media_detail_lines(&st, 60)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("待取"), "{text}");
        st.detail = Some(st.media.items[0].clone());
        let text: String = media_detail_lines(&st, 60)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("完整元数据"), "{text}");
        st.detail = None;
        st.detail_error = Some("NOT_FOUND: x".into());
        let text: String = media_detail_lines(&st, 60)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("获取失败"), "{text}");
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(
            parse_mode(Some("3840x2400@60.000")),
            Some((3840, 2400, "60.000".to_string()))
        );
        assert_eq!(parse_mode(Some("bad")), None);
        assert_eq!(parse_mode(None), None);
    }
}
