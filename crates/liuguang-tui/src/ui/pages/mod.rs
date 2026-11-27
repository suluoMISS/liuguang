//! 七个页面的渲染（只读状态；不做任何 I/O）。

pub mod dashboard;
pub mod effects;
pub mod library;
pub mod outputs;
pub mod playlists;
pub mod static_wallpaper;
pub mod system;

use crate::model::{AppState, Page, SubPage};
use crate::ui::Regions;
use ratatui::Frame;

/// 渲染当前页到 `regions.main` / `regions.detail`。
///
/// 小档（单栏）时按 [`SubPage`] 决定把哪一块画进 `main`。
pub fn render(f: &mut Frame, regions: Regions, st: &AppState, single_column: bool) {
    if single_column {
        match st.subpage {
            SubPage::List => render_main(f, regions.main, st),
            SubPage::Detail => render_detail(f, regions.main, st),
            SubPage::Actions => render_actions(f, regions.main, st),
        }
        return;
    }
    render_main(f, regions.main, st);
    if regions.detail.width > 0 {
        render_detail(f, regions.detail, st);
    }
}

/// 页面主体。
pub fn render_main(f: &mut Frame, area: ratatui::layout::Rect, st: &AppState) {
    match st.page {
        Page::Dashboard => dashboard::render(f, area, st),
        Page::Library => library::render(f, area, st),
        Page::Playlists => playlists::render(f, area, st),
        Page::Outputs => outputs::render(f, area, st),
        Page::Effects => effects::render(f, area, st),
        Page::System => system::render(f, area, st),
        Page::Static => static_wallpaper::render(f, area, st),
    }
}

/// 页面详情栏。
pub fn render_detail(f: &mut Frame, area: ratatui::layout::Rect, st: &AppState) {
    match st.page {
        Page::Library => crate::ui::widgets::list::render_media_detail(f, area, st),
        Page::Outputs => crate::ui::widgets::list::render_output_detail(f, area, st),
        Page::Effects => effects::render_detail(f, area, st),
        Page::Playlists => playlists::render_detail(f, area, st),
        Page::System => system::render_detail(f, area, st),
        Page::Dashboard => dashboard::render_detail(f, area, st),
        Page::Static => static_wallpaper::render_detail(f, area, st),
    }
}

/// 小档"操作"子页：该页可用的动作清单（含上下文说明）。
pub fn render_actions(f: &mut Frame, area: ratatui::layout::Rect, st: &AppState) {
    let cols = crate::ui::widgets::section_inner_cols(area);
    let mut lines = vec![crate::ui::widgets::text_line(
        &format!("{} · 可用操作", st.page.title()),
        cols,
    )];
    for hint in crate::ui::widgets::bars::shortcuts(st) {
        lines.push(crate::ui::widgets::text_line(
            &format!("  {} — {}", hint.key, hint.label),
            cols,
        ));
    }
    lines.push(crate::ui::widgets::text_line("", cols));
    lines.push(crate::ui::widgets::text_line(
        "  ↑↓ 移动 · Enter 执行/打开 · Esc 返回",
        cols,
    ));
    crate::ui::widgets::section(f, area, "操作", lines);
}
