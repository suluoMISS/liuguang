//! 共用的界面小工具与主题样式。
//!
//! 设计口径（对标 Wallpaper Engine 的"小红车"）：深色优先、紫色强调（#7a5cff）、
//! 卡片 12px 圆角 + 悬停上浮阴影、等宽数字、骨架占位、淡入。样式全部走 CSS
//! class，不在 Rust 里散落颜色值——换主题只改 `style.css`。

use gtk::{gdk, gio, glib};

use std::time::Duration;

use gtk::prelude::*;

/// 适配方式（协议 wire 值）。
pub const FIT_IDS: [&str; 4] = ["cover", "contain", "stretch", "center"];
/// 适配方式的中文标签。
pub const FIT_LABELS: [&str; 4] = ["裁切填充", "完整显示", "拉伸铺满", "居中原始"];
/// 旋转角度（协议只接受这四个值）。
pub const ROTATIONS: [u32; 4] = [0, 90, 180, 270];

/// 曜晶流光（Obsidian Crystal）主题样式：深邃墨玉底色 + 细腻镜面高光描边 +
/// 半透分层卡片与悬浮胶囊控制栏，保证高对比文字可读性且完全兼容 GTK4 CSS。
const CSS: &str = r#"

@define-color liuguang_accent #9c88ff;
@define-color liuguang_accent_bg #7a5cff;
@define-color liuguang_accent_fg #ffffff;

window.liuguang {
  background-color: #0b0d14;
  color: #edf0f7;
  background-image:
    radial-gradient(circle at 16% 0%, rgba(122, 92, 255, 0.14), transparent 46%),
    radial-gradient(circle at 86% 18%, rgba(86, 152, 255, 0.08), transparent 42%);
}

window.liuguang headerbar {
  background-color: rgba(14, 17, 26, 0.82);
  border-bottom: 1px solid rgba(255, 255, 255, 0.07);
  box-shadow: inset 0 -1px 0 rgba(0, 0, 0, 0.45);
}

window.liuguang .sidebar-pane {
  background-color: rgba(13, 16, 24, 0.92);
  border-right: 1px solid rgba(255, 255, 255, 0.07);
}
window.liuguang .sidebar-pane .navigation-sidebar {
  background-color: transparent;
  border-right: none;
  padding: 4px 8px;
}
window.liuguang .sidebar-pane .navigation-sidebar row {
  border-radius: 10px;
  margin: 2px 0;
  color: rgba(226, 232, 246, 0.82);
  border: 1px solid transparent;
  transition: background-color 140ms ease, border-color 140ms ease, color 140ms ease;
}
window.liuguang .sidebar-pane .navigation-sidebar row:hover {
  background-color: rgba(255, 255, 255, 0.05);
  color: #f4f7ff;
}
window.liuguang .sidebar-pane .navigation-sidebar row:selected {
  background-color: alpha(@liuguang_accent_bg, 0.24);
  border-color: alpha(@liuguang_accent, 0.36);
  color: #ffffff;
  font-weight: 700;
}

/* 侧栏：标题小一点，选中项跟着强调色 */
.sidebar-title { font-weight: 800; font-size: 13px; letter-spacing: 1px; }
.side-title { font-weight: 700; }

.library-toolbar {
  background-color: rgba(13, 16, 25, 0.65);
}

.tile-wall-scroller,
.tile-grid {
  background-color: #06080d;
}

/* 曜晶画报墙（3px 极细拼接缝 + 静默暗场 / 悬停聚光灯提亮） */
.media-card {
  border-radius: 3px;
  background-color: #0e111a;
  border: none;
  box-shadow: none;
}
.media-card .card-veil {
  border-radius: 3px;
  background-color: rgba(7, 9, 15, 0.22);
  box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.06);
  transition: background-color 200ms ease, box-shadow 200ms ease;
}
.media-card:hover .card-veil {
  background-color: rgba(7, 9, 15, 0.0);
  box-shadow:
    inset 0 0 0 1.5px alpha(@liuguang_accent, 0.92),
    inset 0 0 28px alpha(@liuguang_accent_bg, 0.24);
}
.media-card.selected .card-veil {
  background-color: rgba(7, 9, 15, 0.04);
  box-shadow: inset 0 0 0 2px alpha(@liuguang_accent, 0.95);
}
.media-card.in-use .card-veil {
  background-color: rgba(7, 9, 15, 0.0);
  box-shadow:
    inset 0 0 0 2px rgba(114, 230, 170, 0.92),
    inset 0 0 24px rgba(46, 204, 132, 0.22);
}
.media-card .thumb {
  border-radius: 3px;
  background-color: rgba(6, 8, 12, 0.80);
}
.media-card .card-scrim {
  border-radius: 0 0 3px 3px;
  padding: 34px 12px 10px 12px;
  background-image: linear-gradient(
    to top,
    rgba(5, 7, 11, 0.88) 0%,
    rgba(5, 7, 11, 0.42) 52%,
    rgba(5, 7, 11, 0.0) 100%
  );
}
.card-title-overlay {
  color: #f8faff;
  font-weight: 700;
  font-size: 13.5px;
  text-shadow: 0 1px 4px rgba(0, 0, 0, 0.90);
}
.card-meta-overlay {
  color: rgba(220, 228, 244, 0.70);
  font-size: 11px;
  font-feature-settings: "tnum";
  text-shadow: 0 1px 3px rgba(0, 0, 0, 0.88);
}
.index-tag {
  border-radius: 2px;
  padding: 1px 5px;
  font-size: 10px;
  font-weight: 700;
  font-feature-settings: "tnum";
  background-color: rgba(6, 8, 13, 0.72);
  border: 1px solid rgba(255, 255, 255, 0.14);
  color: rgba(240, 244, 255, 0.86);
}
.video-chip {
  border-radius: 3px;
  padding: 1px 6px;
  font-size: 10px;
  font-weight: 700;
  font-feature-settings: "tnum";
  background-color: rgba(8, 11, 18, 0.76);
  border: 1px solid rgba(255, 255, 255, 0.16);
  color: #eef3ff;
  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.5);
}
.fav-btn {
  min-width: 24px;
  min-height: 24px;
  padding: 0 5px;
  border-radius: 3px;
  background-color: rgba(10, 13, 20, 0.72);
  border: 1px solid rgba(255, 255, 255, 0.16);
  color: rgba(242, 245, 252, 0.85);
  font-size: 11px;
  opacity: 0;
  transition: opacity 150ms ease, background-color 150ms ease, border-color 150ms ease;
}
.media-card:hover .fav-btn,
.fav-btn.active {
  opacity: 1;
}
.fav-btn:hover {
  background-color: rgba(22, 27, 40, 0.88);
  border-color: rgba(255, 255, 255, 0.32);
}
.fav-btn.active {
  color: #ffd166;
  background-color: rgba(34, 28, 12, 0.78);
  border-color: rgba(255, 209, 102, 0.55);
}

/* 角标 / 统一状态胶囊 */
.badge {
  border-radius: 6px;
  padding: 2px 7px;
  font-size: 10px;
  font-weight: 700;
  font-feature-settings: "tnum";
  background-color: rgba(9, 12, 18, 0.76);
  border: 1px solid rgba(255, 255, 255, 0.10);
  color: #f3f6fc;
  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.55);
}
.badge.accent { background-color: @liuguang_accent_bg; color: @liuguang_accent_fg; border-color: transparent; }
.badge.live { background-color: rgba(25, 148, 92, 0.90); color: #ffffff; border-color: rgba(126, 224, 168, 0.45); }
.badge.warn { background-color: rgba(109, 74, 18, 0.90); color: #ffd79a; }
.badge.danger { background-color: rgba(111, 29, 40, 0.90); color: #ffb3bd; }
.badge.ok { background-color: rgba(31, 92, 64, 0.90); color: #b9f5d0; }

.status-chip {
  border-radius: 999px;
  min-height: 22px;
  padding: 2px 10px;
  font-size: 11px;
  font-weight: 600;
  font-feature-settings: "tnum";
  background-color: rgba(255, 255, 255, 0.06);
  border: 1px solid rgba(255, 255, 255, 0.12);
  color: rgba(232, 238, 250, 0.88);
}
.status-chip.live {
  background-color: rgba(28, 178, 114, 0.16);
  border-color: rgba(114, 230, 170, 0.36);
  color: #82f0b8;
}
.status-chip.ok {
  background-color: rgba(255, 255, 255, 0.06);
  border-color: rgba(255, 255, 255, 0.13);
  color: rgba(224, 232, 248, 0.84);
}
.status-chip.warn {
  background-color: rgba(245, 166, 35, 0.16);
  border-color: rgba(255, 215, 154, 0.34);
  color: #ffd79a;
}
.status-chip.danger {
  background-color: rgba(224, 85, 97, 0.18);
  border-color: rgba(255, 179, 189, 0.36);
  color: #ffb3bd;
}

.compat-reason { color: #ffb3bd; }
.compat-note { color: #ffd79a; }
.alert-text { color: #ffb3bd; }
.pill {
  border-radius: 999px;
  padding: 2px 10px;
  font-size: 11px;
  font-weight: 600;
  background-color: alpha(@liuguang_accent_bg, 0.22);
  border: 1px solid alpha(@liuguang_accent, 0.32);
  color: @liuguang_accent;
}
.chip {
  border-radius: 999px;
  padding: 3px 10px;
  font-size: 12px;
  background-color: rgba(22, 26, 39, 0.92);
  border: 1px solid rgba(255, 255, 255, 0.11);
}
.brightness-chip {
  border-radius: 999px;
  padding: 2px 10px;
  min-height: 24px;
  font-size: 11px;
  font-weight: 600;
  font-feature-settings: "tnum";
  background-color: rgba(26, 31, 46, 0.88);
  border: 1px solid rgba(255, 255, 255, 0.10);
  color: #e2e8f5;
}
.brightness-chip:hover {
  border-color: alpha(@liuguang_accent, 0.72);
  background-color: alpha(@liuguang_accent_bg, 0.20);
}

/* 数字统一等宽，表格/状态条不跳动 */
.mono { font-feature-settings: "tnum"; }
.dim { opacity: 0.72; }
.small { font-size: 11px; }
.section-title { font-weight: 700; font-size: 13px; margin-top: 6px; color: #eef2fa; }
.hero-title { font-weight: 800; font-size: 20px; color: #f7f9ff; }

/* 骨架占位：没封面时的静态渐变块。
   刻意不做无限扫光动画——GTK 会为每个骨架每帧重绘，软件渲染下能把 CPU 打满。 */
.skeleton {
  border-radius: 3px;
  background-image: linear-gradient(110deg,
      rgba(255, 255, 255, 0.04) 20%,
      rgba(255, 255, 255, 0.09) 45%,
      rgba(255, 255, 255, 0.04) 70%);
}

/* 属性面板 */
.details-pane {
  background-color: rgba(15, 18, 28, 0.90);
  border-left: 1px solid rgba(255, 255, 255, 0.08);
  box-shadow: inset 1px 0 0 rgba(255, 255, 255, 0.03);
}
.hero-preview {
  border-radius: 12px;
  background-color: rgba(7, 9, 14, 0.85);
  border: 1px solid rgba(255, 255, 255, 0.11);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.08),
    0 8px 20px rgba(0, 0, 0, 0.42);
}
.details-key { color: rgba(222, 229, 245, 0.68); font-size: 12px; }
.details-value { color: #eef2fa; font-size: 12px; font-feature-settings: "tnum"; }
.path-label { font-size: 11px; color: rgba(215, 223, 242, 0.72); }

/* 显示器仿真边框卡片 */
.monitor-bezel {
  border-radius: 12px;
  padding: 6px;
  background-color: #090b10;
  border: 2px solid rgba(255, 255, 255, 0.14);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.10),
    0 10px 24px rgba(0, 0, 0, 0.55);
}
.monitor-screen {
  border-radius: 7px;
  background-color: #05070b;
}
.monitor-stand {
  min-width: 34px;
  min-height: 12px;
  background-color: rgba(255, 255, 255, 0.10);
  border-left: 1px solid rgba(255, 255, 255, 0.12);
  border-right: 1px solid rgba(255, 255, 255, 0.12);
}
.monitor-base {
  min-width: 112px;
  min-height: 5px;
  border-radius: 999px;
  background-color: rgba(255, 255, 255, 0.16);
}

/* 播放列表条目 */
.playlist-row { border-radius: 10px; }
.playlist-row:selected { background-color: alpha(@liuguang_accent_bg, 0.24); }
.drag-handle { opacity: 0.5; }

.sidebar-accent { color: @liuguang_accent; font-weight: 700; }
.brand { font-weight: 800; font-size: 13px; letter-spacing: 0.6px; }
.pad { padding: 14px; }
.card-title { font-weight: 700; font-size: 14px; color: #f5f7fd; }
.active-dot { color: #7ee0a8; }
.warn-dot { color: #f5a623; }
.error-dot { color: #e05561; }

.media-row {
  border-radius: 10px;
  padding: 4px 6px;
}
.media-row:hover { background-color: alpha(@liuguang_accent_bg, 0.10); }
.media-row.in-use {
  background-color: alpha(#7ee0a8, 0.12);
  box-shadow: inset 3px 0 0 #7ee0a8;
}

.notice-row {
  padding: 6px 12px;
  background-color: alpha(#e05561, 0.16);
  border-top: 1px solid alpha(#e05561, 0.35);
}

.pill.live { background-color: alpha(#1f9d63, 0.26); color: #7ee0a8; border-color: rgba(126, 224, 168, 0.40); }
.pill.warn { background-color: alpha(#f5a623, 0.24); color: #ffd79a; border-color: rgba(255, 215, 154, 0.38); }
.pill.dim { background-color: alpha(white, 0.08); color: alpha(@window_fg_color, 0.72); }


/* ---- 形状语言（一轮 GUI 打磨定义，改这里就能全局换形） ----
 * 控件高度只有两档：32（工具栏）、36（页面里的动作按钮）；
 * 圆角只有两档：10（工具控件/图标按钮）、12（动作按钮/分段控件）；
 * 胶囊（999px）只留给状态徽章，不再用于任何可点按钮。 */
.toolbar-pill {
  border-radius: 10px;
  min-height: 32px;
  padding: 0 10px;
  background-color: rgba(22, 26, 39, 0.86);
  border: 1px solid rgba(255, 255, 255, 0.10);
}
.toolbar-pill:hover { background-color: rgba(34, 40, 58, 0.92); }
.toolbar-pill label { font-size: 12.5px; font-weight: 600; }
.toolbar-pill image { color: alpha(@window_fg_color, 0.85); }

.telemetry-pill {
  min-height: 28px;
  padding: 0 10px;
  margin-left: 4px;
}
.telemetry-pill label {
  font-size: 11.5px;
  font-weight: 600;
  color: #d6def0;
}

.icon-btn {
  border-radius: 10px;
  min-width: 32px;
  min-height: 32px;
  padding: 0;
  background-color: rgba(22, 26, 39, 0.86);
  border: 1px solid rgba(255, 255, 255, 0.10);
}
.icon-btn:hover { background-color: rgba(34, 40, 58, 0.92); }
.icon-btn:checked, .icon-btn.active {
  background-color: alpha(@liuguang_accent_bg, 0.30);
  border-color: alpha(@liuguang_accent, 0.45);
}

.action-btn {
  border-radius: 12px;
  min-height: 36px;
  padding: 0 14px;
  font-size: 13px;
  font-weight: 600;
  background-color: rgba(26, 31, 46, 0.88);
  border: 1px solid rgba(255, 255, 255, 0.10);
  color: #edf1fc;
}
.action-btn:hover { background-color: rgba(38, 45, 64, 0.94); }
.action-btn.primary {
  min-height: 38px;
  background-color: @liuguang_accent_bg;
  background-image: linear-gradient(180deg, rgba(255, 255, 255, 0.16), rgba(255, 255, 255, 0.0));
  color: @liuguang_accent_fg;
  border: 1px solid rgba(255, 255, 255, 0.18);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.24),
    0 6px 18px alpha(@liuguang_accent_bg, 0.34);
}
.action-btn.primary:hover {
  background-color: shade(@liuguang_accent_bg, 1.09);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.30),
    0 8px 22px alpha(@liuguang_accent_bg, 0.45);
}

.segmented {
  border-radius: 12px;
  background-color: rgba(20, 24, 36, 0.86);
  border: 1px solid rgba(255, 255, 255, 0.10);
  padding: 2px;
}
.segmented > button {
  border-radius: 10px;
  min-height: 26px;
  min-width: 0;
  padding: 0 10px;
  font-size: 12px;
  font-weight: 600;
  font-feature-settings: "tnum";
  background-color: transparent;
  border: none;
  box-shadow: none;
}
.segmented > button:hover { background-color: rgba(255, 255, 255, 0.06); }
.segmented > button:checked, .segmented > button.active {
  background-color: alpha(@liuguang_accent_bg, 0.32);
  color: @liuguang_accent;
}

.kbd-chip {
  border-radius: 6px;
  padding: 2px 8px;
  font-size: 12px;
  font-weight: 600;
  font-feature-settings: "tnum";
  background-color: rgba(22, 26, 39, 0.90);
  border: 1px solid rgba(255, 255, 255, 0.12);
}
"#;

thread_local! {
    static LIUGUANG_PROVIDER: gtk::CssProvider = gtk::CssProvider::new();
}

/// 安装全局 CSS（在 `Application::connect_startup` 里调用一次）。
pub fn install_css() {
    let Some(display) = gdk::Display::default() else {
        eprintln!("liuguang-gui: 没有可用的显示器，跳过主题安装");
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    LIUGUANG_PROVIDER.with(|provider| {
        gtk::style_context_add_provider_for_display(
            &display,
            provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
    });
}

/// 约束强调背景色的相对亮度，保证浅色主题色下主按钮的白字仍有高对比度；
/// 同时返回更明亮的高光色供边框、胶囊文字与点缀使用。
#[must_use]
pub fn contrast_safe_accent(rgb: (u8, u8, u8)) -> ((u8, u8, u8), (u8, u8, u8)) {
    let (r, g, b) = rgb;
    let lum = 0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b);
    let (bg_r, bg_g, bg_b) = if lum > 125.0 {
        let scale = 125.0 / lum;
        (
            (f64::from(r) * scale).round().clamp(0.0, 255.0) as u8,
            (f64::from(g) * scale).round().clamp(0.0, 255.0) as u8,
            (f64::from(b) * scale).round().clamp(0.0, 255.0) as u8,
        )
    } else {
        (r, g, b)
    };
    let lr = r.saturating_add(((255 - r) / 5).max(16));
    let lg = g.saturating_add(((255 - g) / 5).max(16));
    let lb = b.saturating_add(((255 - b) / 5).max(16));
    ((bg_r, bg_g, bg_b), (lr, lg, lb))
}

/// 根据当前壁纸提取的主氛围色动态刷新界面主题光晕与强调色。
pub fn update_liuguang_css(rgb: (u8, u8, u8)) {
    if gdk::Display::default().is_none() {
        return;
    }
    let ((r, g, b), (lr, lg, lb)) = contrast_safe_accent(rgb);
    let css = format!(
        "@define-color liuguang_accent_bg rgb({r}, {g}, {b});\n\
         @define-color liuguang_accent rgb({lr}, {lg}, {lb});\n\
         window.liuguang {{\n\
           background-image:\n\
             radial-gradient(circle at 16% 0%, rgba({r}, {g}, {b}, 0.18), transparent 48%),\n\
             radial-gradient(circle at 86% 16%, rgba({lr}, {lg}, {lb}, 0.10), transparent 44%);\n\
         }}\n"
    );
    LIUGUANG_PROVIDER.with(|provider| {
        provider.load_from_string(&css);
    });
}

/// 人类可读的字节数。
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 分辨率标签（`2560×1600`），探测失败时给 `—`。
#[must_use]
pub fn resolution_label(item: &liuguang_ipc::protocol::MediaItem) -> String {
    match item
        .details
        .as_ref()
        .filter(|d| d.width > 0 && d.height > 0)
    {
        Some(details) => format!("{}×{}", details.width, details.height),
        None => "—".to_owned(),
    }
}

/// 帧率标签（`59.94 fps`），静态图返回 `None`。
#[must_use]
pub fn fps_label(item: &liuguang_ipc::protocol::MediaItem) -> Option<String> {
    let fps = crate::state::fps(item)?;
    if (fps - fps.round()).abs() < 0.01 {
        Some(format!("{fps:.0} fps"))
    } else {
        Some(format!("{fps:.2} fps"))
    }
}

/// 时长标签（`1:23` / `1:02:03`）。
#[must_use]
pub fn duration_label(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// daemon 状态码 → 中文（底栏、显示器卡片共用）。
///
/// 协议里状态是字符串而不是枚举（新 daemon 加状态时旧客户端不会解析失败），
/// 所以这里做**已知值映射 + 未知值回退原文**：不认识的码仍然可读，不会变成空白。
#[must_use]
pub fn state_text(state: &str) -> String {
    match state {
        "Boot" => "启动中",
        "WaitingForSeat" => "等待会话",
        "WaitingForOutput" => "等待输出",
        "InitializingDisplay" => "初始化显示",
        "StaticIdle" => "静态画面",
        "VideoPlaying" => "播放中",
        "PreparingNext" => "准备下一个",
        "Transitioning" => "转场中",
        "Paused" => "已暂停",
        "HeadlessReady" => "无显示器",
        "Suspended" => "已挂起",
        "DegradedStatic" => "降级静态",
        "Recovering" => "恢复中",
        other => other,
    }
    .to_owned()
}

/// 暂停原因的中文解释（与 daemon 的 `pause_reason` 取值对齐）。
#[must_use]
pub fn pause_reason_text(reason: Option<&str>) -> String {
    match reason {
        Some("fullscreen") => "全屏让路中".to_owned(),
        Some("screen_off") => "熄屏暂停".to_owned(),
        Some("game_running") => "游戏运行暂停".to_owned(),
        Some("gpu_memory") => "显存压力暂停".to_owned(),
        Some("battery") => "电池供电暂停".to_owned(),
        Some("user") => "手动暂停".to_owned(),
        Some(other) if !other.is_empty() => other.to_owned(),
        _ => "无".to_owned(),
    }
}

/// 在文件管理器里选中显示一个文件。
pub fn open_in_file_manager(path: &str) {
    let file = gio::File::for_path(path);
    let Some(parent) = file.parent() else {
        eprintln!("liuguang-gui: {path} 没有父目录");
        return;
    };
    if let Err(e) =
        gio::AppInfo::launch_default_for_uri(parent.uri().as_str(), None::<&gio::AppLaunchContext>)
    {
        eprintln!("liuguang-gui: 打开文件管理器失败：{e}");
    }
}

/// 小角标（黑底白字）。
pub fn badge(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("badge");
    label.set_halign(gtk::Align::Start);
    label.set_valign(gtk::Align::Start);
    label
}

/// 强调色胶囊标签。
/// 兼容性角标：`Playable` 不出角标，其余给（文案, CSS class）。
#[must_use]
pub fn compat_badge(
    compat: &liuguang_ipc::protocol::CompatInfo,
) -> Option<(&'static str, &'static str)> {
    use liuguang_ipc::protocol::CompatState;
    match compat.state {
        CompatState::Playable => None,
        CompatState::NeedsConversion => Some(("需转码", "warn")),
        CompatState::Unsupported => Some(("不支持", "danger")),
    }
}

/// 兼容性角标的 tooltip：原因 + 目标 + 可操作建议。
#[must_use]
pub fn compat_tooltip(compat: &liuguang_ipc::protocol::CompatInfo) -> String {
    use liuguang_ipc::protocol::CompatState;
    let mut parts: Vec<String> = Vec::new();
    if let Some(reason) = &compat.reason {
        parts.push(reason.clone());
    }
    match compat.state {
        CompatState::Playable => {
            parts.push("可直接播放（无需转码）".to_owned());
        }
        CompatState::NeedsConversion => {
            if let Some(target) = &compat.target {
                parts.push(format!("将转成 {target} 缓存后播放（源文件不改）"));
            } else {
                parts.push("需要先生成可硬解的缓存副本（源文件不改）".to_owned());
            }
        }
        CompatState::Unsupported => {
            parts.push("这个素材无法播放，换一版编码/分辨率更低的文件".to_owned());
        }
    }
    if parts.is_empty() {
        "兼容性未知".to_owned()
    } else {
        parts.join("\n")
    }
}

/// 把 daemon 给的转码目标（如 `hevc 8bit`）写成用户看得懂的说法。
#[must_use]
pub fn target_label(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("hevc") && lower.contains("8bit") {
        return "8bit HEVC".to_owned();
    }
    raw.to_owned()
}

/// 转码准备按钮/进度的文案（0–1000 permille）。
///
/// `target` 来自 `CompatInfo.target`（例如 `hevc 8bit`）：就绪时要说清
/// **转成了什么**，不能只写"已就绪"——用户会误以为原文件被原生支持了。
#[must_use]
pub fn prepare_label(
    state: &liuguang_ipc::protocol::PrepareState,
    progress_permille: u32,
    target: Option<&str>,
) -> String {
    use liuguang_ipc::protocol::PrepareState;
    match state {
        PrepareState::Queued => "准备中…".to_owned(),
        PrepareState::Running => format!("转码 {}%", (progress_permille.min(1000) + 5) / 10),
        PrepareState::Ready => match target {
            Some(target) => format!("已转换为 {}（缓存副本）", target_label(target)),
            None => "已转换为缓存副本".to_owned(),
        },
        PrepareState::Failed => "重试准备".to_owned(),
    }
}

/// 页面里的动作按钮（36 高、12 圆角）；`primary` 用强调色底。
#[must_use]
pub fn action_button(label: &str, primary: bool) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("action-btn");
    if primary {
        button.add_css_class("primary");
    }
    button
}

/// 32×32 的纯图标按钮（视图切换、页头小动作）。
#[must_use]
pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("icon-btn");
    button.set_tooltip_text(Some(tooltip));
    button
}

/// 工具栏胶囊按钮：`MenuButton` + 图标 + 当前值文本。
///
/// 返回按钮与文字标签两个句柄——排序/筛选切换后调用方只需改标签文本，
/// 不必重建控件（重建会丢焦点与 popover 状态）。
#[must_use]
pub fn toolbar_pill(icon: &str, label: &str) -> (gtk::MenuButton, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    row.append(&image);
    row.append(&text);
    let button = gtk::MenuButton::new();
    button.add_css_class("toolbar-pill");
    button.set_child(Some(&row));
    (button, text)
}

/// 分段控件：一组互斥的短按钮，选中项高亮。
///
/// 用 `gtk::Button` + `.active` 样式而不是 `ToggleButton`：选中态由调用方通过
/// [`select_segment`] 同步（比如滑杆拖到 87% 时高亮最近的档位），不会因为
/// `set_active` 反过来触发 `on_change` 把滑杆拽回 75%。
pub fn segmented<T, F>(options: &[(&str, T)], on_change: F) -> (gtk::Box, Vec<gtk::Button>)
where
    T: Copy + 'static,
    F: Fn(T) + 'static,
{
    let group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    group.add_css_class("segmented");
    group.set_halign(gtk::Align::Start);
    let on_change = std::rc::Rc::new(on_change);
    let mut buttons: Vec<gtk::Button> = Vec::with_capacity(options.len());
    for (label, value) in options {
        let button = gtk::Button::with_label(label);
        let value = *value;
        let cb = on_change.clone();
        button.connect_clicked(move |_| cb(value));
        group.append(&button);
        buttons.push(button);
    }
    if let Some(first) = buttons.first() {
        first.add_css_class("active");
    }
    (group, buttons)
}

/// 把分段控件的高亮切到 `index`（只改样式，不触发回调）。
pub fn select_segment(buttons: &[gtk::Button], index: usize) {
    for (at, button) in buttons.iter().enumerate() {
        if at == index {
            button.add_css_class("active");
        } else {
            button.remove_css_class("active");
        }
    }
}

/// 按数值找最近档位（分段控件与滑杆联动用）。
#[must_use]
pub fn nearest_index(values: &[f64], value: f64) -> usize {
    let mut best = 0usize;
    let mut best_delta = f64::INFINITY;
    for (index, candidate) in values.iter().enumerate() {
        let delta = (candidate - value).abs();
        if delta < best_delta {
            best_delta = delta;
            best = index;
        }
    }
    best
}

pub fn pill(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("pill");
    label
}

/// 灰字。
/// 区块标题。
pub fn section_title(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("section-title");
    label.set_halign(gtk::Align::Start);
    label.set_xalign(0.0);
    label
}

/// RGBA8 数据 → GDK 纹理（失败返回 `None`，调用方据此显示骨架）。
#[must_use]
pub fn texture_from_rgba(width: i32, height: i32, rgba: &[u8]) -> Option<gdk::Texture> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let expected = usize::try_from(width).ok()? * usize::try_from(height).ok()? * 4;
    if rgba.len() < expected {
        return None;
    }
    let bytes = glib::Bytes::from(&rgba[..expected]);
    Some(
        gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            usize::try_from(width).ok()? * 4,
        )
        .upcast(),
    )
}

/// 丢弃一个「一次性」定时器，**不**按 id 删除。
///
/// `glib::SourceId::remove()` 在源已经触发/销毁后会 `unwrap()` 一个 `Err` 并 panic
/// （glib 文档称之为 "programmer error"）。一次性源（`timeout_add_local_once`）触发后
/// 就自我销毁，所以持有它的 id 再 `remove()` 必崩——`apps/liuguang-gui` 曾在悬停延迟上
/// 踩过这个坑（freed 之后一 hover 一移动就 SIGABRT）。
///
/// 正确做法是：一次性源只需要**丢掉 id**，并靠调用方的代次号/`Option` 状态让迟到的
/// 回调变成空操作。
pub fn cancel_once(slot: &std::cell::RefCell<Option<glib::SourceId>>) {
    let _ = slot.borrow_mut().take();
}

/// 防抖：把一串高频回调（滑杆拖动、搜索输入）收敛成最后一次。
///
/// `slot` 由调用方持有，保存尚未触发的定时器；重复调用会先取消上一个。
/// 回调自己会在触发时把 `slot` 清空，因此这里 `take()` 到的 id 一定还活着（可以安全
/// `remove()`）；为防将来误用，仍走 [`cancel_once`] 语义之外的显式分支。
pub fn debounce<F>(slot: &std::rc::Rc<std::cell::Cell<Option<glib::SourceId>>>, ms: u32, action: F)
where
    F: FnOnce() + 'static,
{
    if let Some(previous) = slot.take() {
        previous.remove();
    }
    let for_timer = slot.clone();
    let source = glib::timeout_add_local_once(Duration::from_millis(u64::from(ms)), move || {
        for_timer.set(None);
        action();
    });
    slot.set(Some(source));
}

/// 适配方式在 `FIT_IDS` 里的下标。
#[must_use]
pub fn fit_index(fit: &str) -> u32 {
    FIT_IDS
        .iter()
        .position(|id| *id == fit)
        .map_or(0, |index| index as u32)
}

/// 生成一张指定尺寸的深色不透明纹理。
///
/// 用途：`gtk::Picture` 的自然尺寸来自 paintable，没有纹理时自然尺寸是 0，
/// `FlowBox` 会先按"没有封面"的宽度排出一套列数，封面到达后重排（打开时
/// 3 列、几秒后变 2 列那种跳变）。先贴一张同尺寸占位纹理把自然尺寸钉住，
/// 真封面只是替换 paintable、尺寸不变，布局就不会再动。
pub fn blank_texture(width: i32, height: i32) -> gdk::Texture {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static CACHE: RefCell<HashMap<(i32, i32), gdk::Texture>> = RefCell::new(HashMap::new());
    }
    let key = (width.max(1), height.max(1));
    CACHE.with(|cache| {
        if let Some(existing) = cache.borrow().get(&key) {
            return existing.clone();
        }
        let (w, h) = (key.0 as usize, key.1 as usize);
        let stride = w * 4;
        let mut data = vec![0u8; stride * h];
        for px in data.chunks_exact_mut(4) {
            px[0] = 0x12;
            px[1] = 0x14;
            px[2] = 0x18;
            px[3] = 0xff;
        }
        let bytes = glib::Bytes::from_owned(data);
        let tex: gdk::Texture =
            gdk::MemoryTexture::new(key.0, key.1, gdk::MemoryFormat::R8g8b8a8, &bytes, stride)
                .upcast();
        cache.borrow_mut().insert(key, tex.clone());
        tex
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn durations_use_hms_where_needed() {
        assert_eq!(duration_label(0), "0:00");
        assert_eq!(duration_label(9_000), "0:09");
        assert_eq!(duration_label(83_000), "1:23");
        assert_eq!(duration_label(3_723_000), "1:02:03");
    }

    #[test]
    fn state_codes_are_localized_with_fallback() {
        assert_eq!(state_text("VideoPlaying"), "播放中");
        assert_eq!(state_text("Paused"), "已暂停");
        assert_eq!(state_text("StaticIdle"), "静态画面");
        assert_eq!(state_text("DegradedStatic"), "降级静态");
        // 未来 daemon 新增状态：原样显示，不能吞掉。
        assert_eq!(state_text("SomeFutureState"), "SomeFutureState");
    }

    #[test]
    fn pause_reasons_are_localized() {
        assert_eq!(pause_reason_text(Some("fullscreen")), "全屏让路中");
        assert_eq!(pause_reason_text(Some("screen_off")), "熄屏暂停");
        assert_eq!(pause_reason_text(Some("game_running")), "游戏运行暂停");
        assert_eq!(pause_reason_text(Some("gpu_memory")), "显存压力暂停");
        assert_eq!(pause_reason_text(Some("user")), "手动暂停");
        assert_eq!(pause_reason_text(None), "无");
        assert_eq!(pause_reason_text(Some("custom")), "custom");
    }

    #[test]
    fn fit_index_maps_wire_values() {
        assert_eq!(fit_index("cover"), 0);
        assert_eq!(fit_index("contain"), 1);
        assert_eq!(fit_index("center"), 3);
        assert_eq!(fit_index("nonsense"), 0);
    }

    #[test]
    fn textures_require_full_rgba_buffer() {
        assert!(texture_from_rgba(2, 2, &[0u8; 16]).is_some());
        assert!(texture_from_rgba(2, 2, &[0u8; 15]).is_none());
        assert!(texture_from_rgba(0, 2, &[]).is_none());
    }

    #[test]
    fn contrast_safe_accent_clamps_bright_backgrounds() {
        let ((br, bg, bb), (lr, lg, lb)) = contrast_safe_accent((185, 220, 255));
        let lum = 0.2126 * f64::from(br) + 0.7152 * f64::from(bg) + 0.0722 * f64::from(bb);
        assert!(lum <= 126.0, "强调按钮底色过亮，白字可读性不足: lum={lum}");
        assert!(lr >= 185 && lg >= 220 && lb == 255);
    }
}
