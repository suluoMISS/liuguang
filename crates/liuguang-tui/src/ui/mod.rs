//! 渲染层：布局判定 + 页面/部件组合。
//!
//! **只读 [`AppState`]**：不做事件处理、不做 IPC、不碰终端能力探测。
//! 入口是 [`draw`]，它按四档布局把顶栏/导航/页面/底栏/弹窗依次画到 `Frame`。

pub mod layout;
pub mod pages;
pub mod theme;
pub mod widgets;

pub use layout::{Regions, Tier, compute, tier};

use crate::model::AppState;
use ratatui::Frame;

/// 渲染一帧（`Frame` 由调用方持有；本函数不做任何 I/O）。
pub fn draw(f: &mut Frame, st: &AppState) {
    let area = f.area();
    // 先清屏并铺上 Obsidian Crystal 深邃墨玉底色，避免透明终端出现灰白底。
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(
        ratatui::widgets::Block::default().style(theme::surface_bg()),
        area,
    );
    let t = tier(area);

    if t == Tier::Tiny {
        // 极小档：只显示"终端过小 + 当前状态 + 最小快捷键"（不画弹窗，避免挤压）
        widgets::status::render_tiny(f, area, st);
        return;
    }

    let r = compute(area, t);
    match t {
        Tier::Wide => {
            widgets::bars::render_top_wide(f, r.top, st);
            widgets::nav::render_nav(f, r.nav, st);
            pages::render(f, r, st, false);
            // 底栏 2 行：状态 + 快捷键（均随页面/焦点变化）
            let row1 = ratatui::layout::Rect {
                height: 1.min(r.bottom.height),
                ..r.bottom
            };
            let row2 = ratatui::layout::Rect {
                y: r.bottom.y.saturating_add(1),
                height: r.bottom.height.saturating_sub(1),
                ..r.bottom
            };
            widgets::bars::render_bottom_status(f, row1, st);
            widgets::bars::render_bottom_keys(f, row2, st);
        }
        Tier::Medium => {
            widgets::nav::render_tabs(f, r.tabs, st);
            pages::render(f, r, st, false);
            widgets::bars::render_bottom_compact(f, r.bottom, st);
        }
        Tier::Small => {
            widgets::bars::render_top_compact(f, r.top, st, true);
            pages::render(f, r, st, true);
            widgets::bars::render_bottom_status(f, r.bottom, st);
        }
        Tier::Tiny => unreachable!("上面已提前返回"),
    }

    // 弹窗最后画（视觉上捕获焦点）
    widgets::modal::render_modal(f, area, st);
}
