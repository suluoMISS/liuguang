//! 总览 Dashboard：一眼看清"在放什么、是否真的上屏、有没有异常"。
//!
//! 全部取自 daemon 的 `status`（`decoder` 是**实际生效**的解码方式，不是配置值）。

use crate::model::AppState;
use crate::ui::theme as th;
use crate::ui::widgets::text::truncate_display;
use crate::ui::widgets::{section, section_inner_cols, text_line};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

/// 主体：状态卡。
pub fn render(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    // 本页键最长为「播放列表来源」（显示宽 12）：统一 12 列，数值列纵向对齐
    let kv = |k: &str, v: &str, c: usize| crate::ui::widgets::kv_w(k, v, c, 12);
    let ui = st.ui_status();
    let mut lines = vec![
        kv(
            "状态",
            &format!("{}（daemon: {}）", ui.label(), st.status.state),
            cols,
        ),
        kv(
            "当前显示",
            st.status.media.as_deref().unwrap_or("(无)"),
            cols,
        ),
        kv(
            "下一张",
            // daemon 当前未提供"下一张"；**如实显示未知**，不猜
            "（daemon 未提供该字段）",
            cols,
        ),
        kv("播放列表来源", &st.playlist_source_label(), cols),
        kv(
            "输出",
            st.selected_output.as_deref().unwrap_or("(未选)"),
            cols,
        ),
        kv("模式", st.status.mode.as_deref().unwrap_or("-"), cols),
        kv("Decode", &st.status.decoder, cols),
        kv(
            "暂停原因",
            st.active_pause_reason().unwrap_or("（未暂停）"),
            cols,
        ),
        kv(
            "性能遥测",
            &format!(
                "{} · CPU {:.1}% · {:.0} MiB",
                crate::model::format_fps_int(f64::from(st.metrics.fps)),
                st.metrics.cpu_percent,
                st.metrics.mem_bytes as f64 / (1024.0 * 1024.0)
            ),
            cols,
        ),
        kv("运行", &format!("{} 秒", st.status.uptime_seconds), cols),
    ];
    if let Some(p) = st.selected_media() {
        lines.push(text_line("", cols));
        lines.push(text_line(" 选中项（完整路径，不截断）：", cols));
        for l in crate::ui::widgets::text::wrap_display(&p.path, cols.max(8)) {
            lines.push(Line::from(l));
        }
    }
    lines.push(text_line("", cols));
    lines.push(Line::from(Span::styled(
        truncate_display(
            " 提示：画面是否真的更新请看 System 页的提交计数/master",
            cols,
        ),
        th::muted(),
    )));
    if let Some(phase) = &st.apply_phase {
        lines.push(Line::from(Span::styled(
            truncate_display(&format!(" 应用状态：{}", phase.label()), cols),
            th::apply_phase_style(phase),
        )));
    }
    if !st.connection.is_online() {
        lines.insert(
            0,
            Line::from(Span::styled(
                truncate_display(
                    &format!(
                        " 断线：{}（按 e 重连；以下为最近一次已知状态）",
                        st.connection.label()
                    ),
                    cols,
                ),
                th::error(),
            )),
        );
    }
    section(f, area, "总览", lines);
}

/// 详情栏：逐通道状态。
pub fn render_detail(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines = vec![text_line("各输出通道（daemon 权威）", cols)];
    if st.status.channels.is_empty() {
        lines.push(text_line(" （无输出）", cols));
    }
    for ch in &st.status.channels {
        let play_state = if ch.paused {
            ch.pause_reason
                .as_deref()
                .map(crate::model::pause_reason_label)
                .unwrap_or("已暂停")
        } else {
            "播放中"
        };
        lines.push(text_line(
            &format!(
                " {} · {} · {} · {play_state}",
                ch.output, ch.state, ch.decoder
            ),
            cols,
        ));
    }
    lines.push(text_line("", cols));
    lines.push(text_line("全部输出列表见显示器页（按 4）", cols));
    section(f, area, "输出通道", lines);
}

impl AppState {
    /// 播放列表来源（daemon 未提供"当前播放列表"时如实说明）。
    pub fn playlist_source_label(&self) -> String {
        if self.playlists.is_empty() {
            "(daemon 未报告播放列表)".to_string()
        } else {
            format!("{} 个播放列表（按 3 查看）", self.playlists.len())
        }
    }
}
