//! 四档终端尺寸的**渲染 dump**（给人看的工具，不参与逻辑）。
//!
//! 运行：`CARGO_HOME=... cargo run -p liuguang-tui --example tier_dump`
//!
//! 用 `TestBackend` 渲染四档尺寸 × 两个页面（总览 / 媒体库），并把缓冲区
//! **按可见文本**打印出来（全角字符的续格会被跳过，否则会读成"总 览"）。
//! 数据全部来自 `MockControl`，**不依赖 daemon、不碰 DRM、不读磁盘**。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::model::{MediaItem, SubPage};
use liuguang_tui::{App, MockControl, MockScenario, Page};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// 四个目标尺寸（与验收一致）。
const SIZES: [(u16, u16); 4] = [(140, 40), (100, 30), (70, 22), (50, 15)];

/// 把缓冲区读成可见文本（按显示宽度跳过全角续格）。
fn buffer_lines(term: &Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    let area = buf.area;
    (0..area.height)
        .map(|y| {
            let mut line = String::new();
            let mut skip = 0usize;
            for x in 0..area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(sym);
                let w = unicode_width::UnicodeWidthStr::width(sym);
                if w > 1 {
                    skip = w - 1;
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// 构造一个"像真的"场景：视频播放中 + 中文长文件名（含全角标点）+ 未保存改动。
fn app() -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(
        MockScenario::playing().with_media(24),
    ));
    // 前三条用真实世界的长中文名（含《》全角标点）展示截断与详情换行
    let samples = [
        (
            "/srv/wallpapers/videos/万千孤单焰火让这虚构灵魂鲜活_《光与影的对白》洛天依十三周年生日会.mp4",
            "万千孤单焰火让这虚构灵魂鲜活_《光与影的对白》洛天依十三周年生日会.mp4",
            "video",
        ),
        (
            "/srv/wallpapers/images/世界很温柔—《龙族》上杉绘梨衣.png",
            "世界很温柔—《龙族》上杉绘梨衣.png",
            "image",
        ),
        (
            "/srv/wallpapers/videos/光与影的对白_洛天依_十三周年生日会_4K60_收藏版.mp4",
            "光与影的对白_洛天依_十三周年生日会_4K60_收藏版.mp4",
            "video",
        ),
    ];
    for (i, (path, name, kind)) in samples.into_iter().enumerate() {
        let path = path.to_string();
        app.client.library[i] = MediaItem {
            id: path.clone(),
            source_id: "src-1".to_string(),
            name: name.to_string(),
            path,
            kind: kind.to_string(),
            size_bytes: 128 * 1024 * 1024,
            modified_unix: 1_760_000_000,
            details: None,
        };
    }
    app.refresh_all();
    app.state.page = Page::Library;
    app.state.focus = liuguang_tui::Focus::List;
    app.state.dirty = true; // 展示顶栏的未保存标记 `*`
    app
}

/// 打印一帧（含标题行）。
fn dump(w: u16, h: u16, title: &str, page: Page, subpage: Option<SubPage>) {
    let mut app = app();
    app.state.page = page;
    if let Some(sp) = subpage {
        app.state.subpage = sp;
    }
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    println!("┌── {w}x{h} · {title} ─────────────────────────────────────");
    for line in buffer_lines(&term) {
        println!("│{line}");
    }
    println!("└──────────────────────────────────────────────────────────────");
    println!();
}

fn main() {
    let with_subpages = std::env::args().any(|a| a == "--subpages");
    for (w, h) in SIZES {
        let tier = match (w, h) {
            (140, 40) => "宽档三栏",
            (100, 30) => "中档 Tab+两栏",
            (70, 22) => "小档单栏",
            _ => "极小档",
        };
        // 8 段 = 4 尺寸 × 2 页（总览 / 媒体库）
        dump(
            w,
            h,
            &format!("{tier} · 总览 Dashboard"),
            Page::Dashboard,
            None,
        );
        dump(
            w,
            h,
            &format!("{tier} · 媒体库 Library"),
            Page::Library,
            None,
        );
        // 可选：小档三个子页（左右键切换）
        if with_subpages && (w, h) == (70, 22) {
            for sp in [SubPage::List, SubPage::Detail, SubPage::Actions] {
                dump(
                    w,
                    h,
                    &format!("70x22 小档 · 媒体库 [{}]", sp.label()),
                    Page::Library,
                    Some(sp),
                );
            }
        }
    }
}
