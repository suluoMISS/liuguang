//! 转场 Effects：效果/缓动/时长草稿（`A` 立即应用 / `S` 保存）。

use crate::model::{AppState, DURATION_MAX_MS};
use crate::ui::widgets::{disabled_note, kv, section, section_inner_cols, text_line};
use ratatui::Frame;
use ratatui::layout::Rect;

/// 主体：当前草稿。
pub fn render(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let d = &st.transition;
    let mut lines = vec![
        kv("效果", &format!("{}   （[ ] 切换）", d.effect), cols),
        kv("缓动", &format!("{}   （{{ }} 切换）", d.easing), cols),
        kv(
            "时长",
            &format!("{} ms   (< > 调整，0–{DURATION_MAX_MS})", d.duration_ms),
            cols,
        ),
        kv("方向", d.direction.as_deref().unwrap_or("(未指定)"), cols),
        text_line("", cols),
        text_line(" A 立即应用（只改运行时） · S 保存配置（落盘）", cols),
    ];
    if !crate::model::effect_is_available(&d.effect) {
        lines.push(disabled_note(
            &format!(
                "当前选择 `{}` 未实现：`A`/`S` 不会下发（daemon 只会回退成 fade）——请选 {}",
                d.effect,
                crate::model::EFFECTS_IMPLEMENTED.join("/")
            ),
            cols,
        ));
    }
    if st.dirty {
        lines.push(disabled_note("有未保存改动：放弃编辑不会写入配置", cols));
    }
    section(f, area, "转场", lines);
}

/// 详情栏：可选值。
pub fn render_detail(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines = vec![text_line(
        "可选效果（未实现的标 Disabled，不假装可用）",
        cols,
    )];
    for e in crate::model::EFFECTS {
        let mark = if e == st.transition.effect {
            "▸ "
        } else {
            "  "
        };
        if crate::model::effect_is_available(e) {
            lines.push(text_line(&format!("  {mark}{e}  可用"), cols));
        } else {
            lines.push(text_line(
                &format!("  {mark}{e}  Disabled —— daemon 尚未实现（会被回退成 fade）"),
                cols,
            ));
        }
    }
    lines.push(text_line("", cols));
    lines.push(text_line(
        &format!(
            " 随机效果只从已实现的 4 种里选：{}",
            crate::model::EFFECTS_IMPLEMENTED.join("/")
        ),
        cols,
    ));
    lines.push(text_line("", cols));
    lines.push(text_line("可选缓动", cols));
    for e in crate::model::EASINGS {
        lines.push(text_line(
            &format!(
                "  {}{e}",
                if e == st.transition.easing {
                    "▸ "
                } else {
                    "  "
                }
            ),
            cols,
        ));
    }
    section(f, area, "可选值", lines);
}
