//! 统一视觉语言：曜晶流光（Obsidian Crystal）调色板与样式的**单一来源**。
//!
//! 与 GTK4 GUI（`apps/liuguang-gui/src/ui.rs`）共享同一套语义色彩体系：
//! 深邃墨玉底色（`#0b0d14` / `#0e111a`）、流光紫强调（`#9c88ff`）、
//! 翡翠绿实时运行态（`#72e6aa`）、琥珀金告警与玫瑰红错误态。
//!
//! 全部函数都是纯函数，仅依赖 `model` 的只读类型，可无终端单测。

use crate::model::{ApplyPhase, Connection, NotificationKind};
use ratatui::style::{Color, Modifier, Style};

// ---- 曜晶流光（Obsidian Crystal）调色板 -----------------------------------

/// 窗口主画布底色（`#0b0d14`）。
pub const SURFACE_BG: Color = Color::Rgb(11, 13, 20);
/// 卡片/面板底色（`#0e111a`）。
pub const PANEL_BG: Color = Color::Rgb(14, 17, 26);
/// 边框/分隔线（微弱玻璃描边）。
pub const BORDER: Color = Color::Rgb(46, 52, 72);
/// 聚焦面板边框（流光紫）。
pub const BORDER_ACTIVE: Color = Color::Rgb(156, 136, 255);
/// 次要信息（标签、提示、页脚 `#8a92aa`）。
pub const MUTED: Color = Color::Rgb(138, 146, 170);
/// 普通文本（高对比冷白 `#edf0f7`）。
pub const TEXT: Color = Color::Rgb(237, 240, 247);
/// 强调色（流光紫 `#9c88ff`）。
pub const ACCENT: Color = Color::Rgb(156, 136, 255);
/// 成功/在线/正在使用（翡翠绿 `#72e6aa`）。
pub const OK: Color = Color::Rgb(114, 230, 170);
/// 警告/进行中/自动让路（琥珀金 `#f5c26b`）。
pub const WARN: Color = Color::Rgb(245, 194, 107);
/// 错误/断线（玫瑰红 `#f38ba8`）。
pub const ERROR: Color = Color::Rgb(243, 139, 168);
/// 视频类条目（星云靛 `#a5b4fc`）。
pub const VIDEO: Color = Color::Rgb(165, 180, 252);
/// 选中行背景（流光深紫 `#3e3080`）。
pub const SELECT_BG: Color = Color::Rgb(62, 48, 128);
/// 选中行前景（纯白）。
pub const SELECT_FG: Color = Color::Rgb(255, 255, 255);
/// 编辑器光标背景（保留高对比白块，与终端测试契约一致）。
pub const CURSOR_BG: Color = Color::White;
/// 编辑器光标前景。
pub const CURSOR_FG: Color = Color::Black;
/// 顶栏底色（深曜石 `#10131e`）。
pub const BAR_BG: Color = Color::Rgb(16, 19, 30);
/// 断线时顶栏底色（一眼可见）。
pub const OFFLINE_BG: Color = Color::Rgb(140, 36, 58);

// ---- 标记字符 ------------------------------------------------------------

/// 列表/输出的"选中行"标记。
pub const SELECT_ROW: &str = "▍";
/// 导航的"当前页"标记。
pub const CURRENT_PAGE: &str = "▸";
/// 未选中占位（显式命名，避免各处裸写空格）。
pub const UNSELECTED: &str = " ";

// ---- 样式（纯函数） ------------------------------------------------------

/// 主画布背景。
#[must_use]
pub fn surface_bg() -> Style {
    Style::default().bg(SURFACE_BG).fg(TEXT)
}

/// 面板背景。
#[must_use]
pub fn panel_bg() -> Style {
    Style::default().bg(PANEL_BG).fg(TEXT)
}

/// 视频类条目样式。
#[must_use]
pub fn video() -> Style {
    Style::default().fg(VIDEO)
}

/// 边框样式（默认）。
#[must_use]
pub fn border() -> Style {
    Style::default().fg(BORDER)
}

/// 按聚焦态区分边框颜色。
#[must_use]
pub fn border_focused(active: bool) -> Style {
    if active {
        Style::default().fg(BORDER_ACTIVE)
    } else {
        Style::default().fg(BORDER)
    }
}

/// 次要文本。
#[must_use]
pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

/// 普通文本。
#[must_use]
pub fn text() -> Style {
    Style::default().fg(TEXT)
}

/// 强调文本。
#[must_use]
pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// 强调文本 + 加粗。
#[must_use]
pub fn accent_bold() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// 成功。
#[must_use]
pub fn ok() -> Style {
    Style::default().fg(OK)
}

/// 成功 + 加粗。
#[must_use]
pub fn ok_bold() -> Style {
    Style::default().fg(OK).add_modifier(Modifier::BOLD)
}

/// 正在使用胶囊标签（深翡翠底 + 亮绿字，与 GUI `.status-chip.live` 对齐）。
#[must_use]
pub fn chip_live() -> Style {
    Style::default()
        .fg(OK)
        .bg(Color::Rgb(20, 56, 40))
        .add_modifier(Modifier::BOLD)
}

/// 琥珀警告胶囊标签。
#[must_use]
pub fn chip_warn() -> Style {
    Style::default()
        .fg(WARN)
        .bg(Color::Rgb(58, 44, 18))
        .add_modifier(Modifier::BOLD)
}

/// 玫瑰红错误胶囊标签。
#[must_use]
pub fn chip_error() -> Style {
    Style::default()
        .fg(ERROR)
        .bg(Color::Rgb(62, 24, 34))
        .add_modifier(Modifier::BOLD)
}

/// 警告。
#[must_use]
pub fn warn() -> Style {
    Style::default().fg(WARN)
}

/// 警告 + 加粗。
#[must_use]
pub fn warn_bold() -> Style {
    Style::default().fg(WARN).add_modifier(Modifier::BOLD)
}

/// 错误。
#[must_use]
pub fn error() -> Style {
    Style::default().fg(ERROR)
}

/// 错误 + 加粗。
#[must_use]
pub fn error_bold() -> Style {
    Style::default().fg(ERROR).add_modifier(Modifier::BOLD)
}

/// 选中行（曜晶深紫底 + 亮白加粗：持久高亮，不只靠标记字符）。
#[must_use]
pub fn selected_row() -> Style {
    Style::default()
        .fg(SELECT_FG)
        .bg(SELECT_BG)
        .add_modifier(Modifier::BOLD)
}

/// 编辑器光标（高对比反显块）。
#[must_use]
pub fn cursor() -> Style {
    Style::default()
        .bg(CURSOR_BG)
        .fg(CURSOR_FG)
        .add_modifier(Modifier::BOLD)
}

/// 顶栏底色。
#[must_use]
pub fn bar_bg() -> Style {
    Style::default().bg(BAR_BG).fg(TEXT)
}

/// 断线顶栏底色。
#[must_use]
pub fn offline_bar_bg() -> Style {
    Style::default().bg(OFFLINE_BG).fg(SELECT_FG)
}

/// 连接状态样式（顶栏/底栏共用）。
#[must_use]
pub fn connection_style(conn: &Connection) -> Style {
    match conn {
        Connection::Online => ok_bold(),
        Connection::Offline(_) => error_bold(),
        Connection::Reconnecting => warn_bold(),
    }
}

/// 底栏通知样式（按级别着色，无通知为默认文本）。
#[must_use]
pub fn notification_style(kind: Option<NotificationKind>) -> Style {
    match kind {
        Some(NotificationKind::Error) => error(),
        Some(NotificationKind::Warn) => warn(),
        _ => text(),
    }
}

/// 应用生命周期样式（Sending/Accepted=警告，Applied=成功，Failed=错误加粗）。
#[must_use]
pub fn apply_phase_style(phase: &ApplyPhase) -> Style {
    match phase {
        ApplyPhase::Sending | ApplyPhase::Accepted => warn(),
        ApplyPhase::Applied => ok_bold(),
        ApplyPhase::Failed(_) => error_bold(),
    }
}

/// 等宽进度条（`ratio` 自动钳制在 `0.0..=1.0`）。
#[must_use]
pub fn progress_bar(ratio: f32, width: usize) -> String {
    let w = width.max(1);
    let r = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((r * w as f32).round() as usize).min(w);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(w - filled))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn selected_and_cursor_styles_are_distinct_from_plain() {
        assert_ne!(selected_row(), Style::default());
        assert_ne!(cursor(), Style::default());
        assert_ne!(selected_row(), cursor());
    }

    #[test]
    fn connection_styles_differ_per_state() {
        let online = connection_style(&Connection::Online);
        let offline = connection_style(&Connection::Offline("x".into()));
        let retry = connection_style(&Connection::Reconnecting);
        assert_ne!(online, offline);
        assert_ne!(online, retry);
        assert_ne!(offline, retry);
    }

    #[test]
    fn apply_phase_styles_follow_lifecycle() {
        assert_eq!(apply_phase_style(&ApplyPhase::Sending), warn());
        assert_eq!(apply_phase_style(&ApplyPhase::Accepted), warn());
        assert_eq!(apply_phase_style(&ApplyPhase::Applied), ok_bold());
        assert_ne!(apply_phase_style(&ApplyPhase::Failed("x".into())), warn());
    }

    #[test]
    fn progress_bar_renders_clamped_width() {
        assert_eq!(progress_bar(1.0, 10), "[██████████]");
        assert_eq!(progress_bar(0.0, 10), "[░░░░░░░░░░]");
        assert_eq!(progress_bar(0.5, 10), "[█████░░░░░]");
        assert_eq!(progress_bar(f32::NAN, 4), "[░░░░]");
    }
}
