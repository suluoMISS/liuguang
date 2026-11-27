//! 静态壁纸页（t70，第 7 页）：**静态壁纸单独设置到一个地方**。
//!
//! 用户诉求："静态壁纸要单独设置到一个地方" + "一眼看出现在是什么在放"。
//!
//! 页面结构：
//!
//! ```text
//! 当前静态壁纸:  /srv/wallpapers/xxx.png      （未设置时显示「（未设置）」）
//! 当前播放源:    静态壁纸 / 播放列表 pl-1 / 目录轮换
//! a  把媒体库选中项设为静态壁纸（只允许图片）
//! c  清除（恢复播放列表，从暂停处继续）
//! ```
//!
//! **诚实性**：daemon 侧的静态壁纸能力（暂停播放列表 / 记住位置 / 持久化）由 t69 实现；
//! 本版 daemon 还没提供时，`a`/`c` 会**明确报错**（带可操作指引），页面也会如实标注
//! ——绝不"看起来成功但其实没做"。

use crate::model::AppState;
use crate::ui::theme as th;
use crate::ui::widgets::text::wrap_display;
use crate::ui::widgets::{disabled_note, section, section_inner_cols, text_line};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

/// 主体。
pub fn render(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines: Vec<Line> = Vec::new();

    // ① 当前静态壁纸（完整路径，折行显示）
    lines.push(text_line(" 当前静态壁纸", cols));
    match st.static_wallpaper.as_deref() {
        Some(path) => {
            let indent = 3usize;
            for chunk in wrap_display(path, cols.saturating_sub(indent)) {
                lines.push(Line::from(Span::styled(
                    format!("   {chunk}"),
                    th::ok_bold(),
                )));
            }
        }
        None => lines.push(Line::from(Span::styled("   （未设置）", th::muted()))),
    }
    lines.push(text_line("", cols));

    // ② 当前播放源（用户这次诉求的核心：优先级要可见）
    let (label, inferred) = st.playback_source_label();
    let shown = if inferred {
        format!("{label}（推断）")
    } else {
        label
    };
    // 「当前播放源」显示宽 10 > 默认 9：用 12 列，与 Dashboard 一致且不截断
    lines.push(crate::ui::widgets::kv_w("当前播放源", &shown, cols, 12));
    if inferred {
        lines.push(disabled_note(
            "daemon 尚未在 status 里报告播放源 ⇒ 这里是按「通道是否暂停」推断的",
            cols,
        ));
    }
    lines.push(text_line("", cols));

    // ③ daemon 能力（如实标注）
    match st.static_supported {
        Some(false) => lines.push(disabled_note(
            "⚠ 当前 daemon 不支持静态壁纸接口（版本较旧或未编译该功能）：a / c 会明确报错，不会静默失败",
            cols,
        )),
        Some(true) => lines.push(text_line("   daemon 已支持静态壁纸接口", cols)),
        None => lines.push(text_line("", cols)),
    }
    section(f, area, "静态壁纸", lines);
}

/// 详情栏：两个动作的语义 + 判定依据（让"为什么点不动"可解释）。
pub fn render_detail(f: &mut Frame, area: Rect, st: &AppState) {
    let cols = section_inner_cols(area);
    let mut lines: Vec<Line> = vec![
        text_line(" a  设为静态壁纸", cols),
        text_line("    取【媒体库】当前选中项；**只允许图片**", cols),
        text_line("    设置后 daemon 会暂停播放列表并记住位置", cols),
        text_line("", cols),
        text_line(" c  清除静态壁纸", cols),
        text_line("    恢复播放列表（从暂停处继续）", cols),
        text_line("", cols),
    ];
    match st.static_set_guard() {
        Ok(path) => {
            let mut first = true;
            for chunk in wrap_display(&path, cols.saturating_sub(4)) {
                lines.push(text_line(
                    &format!(" {} {chunk}", if first { "✔" } else { " " }),
                    cols,
                ));
                first = false;
            }
        }
        Err(why) => lines.push(disabled_note(&format!("✘ {why}"), cols)),
    }
    lines.push(text_line("", cols));
    lines.push(disabled_note(
        "静态壁纸是最高优先级：设置后播放列表不推进；清除后从暂停处继续",
        cols,
    ));
    section(f, area, "操作", lines);
}
