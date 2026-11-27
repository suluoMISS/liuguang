//! t67：播放页顶部"播放模式"选择器（三选一）的**渲染层**证据。
//!
//! 为什么要单独一条渲染测试：真机 pty 抓屏是"增量重绘"的字节流，切换模式时
//! ratatui 只重画变化的单元格，抓出来的文本里旧帧会残留 ⇒ 用 TestBackend 直接
//! 断言"最终缓冲区"才可靠（也顺带证明高亮确实跟着状态走）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::{MockControl, MockScenario, Page};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render_state(st: &liuguang_tui::AppState, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("TestBackend");
    term.draw(|f| liuguang_tui::ui::draw(f, st)).expect("draw");
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut skip = 0usize;
        for x in 0..buf.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = buf[(x, y)].symbol();
            out.push_str(sym);
            let w = unicode_width::UnicodeWidthStr::width(sym);
            if w > 1 {
                skip = w - 1;
            }
        }
        out.push('\n');
    }
    out
}

fn playlist(mode: &str, on_video_end: &str) -> liuguang_ipc::protocol::Playlist {
    liuguang_ipc::protocol::Playlist {
        id: "pl-1".into(),
        name: "晚间".into(),
        entries: Vec::new(),
        mode: mode.into(),
        interval_seconds: 600,
        on_video_end: on_video_end.into(),
        output: None,
        no_repeat: true,
    }
}

fn state_with(mode: &str, on_video_end: &str) -> liuguang_tui::AppState {
    let mut st = liuguang_tui::AppState::new();
    st.page = Page::Playlists;
    st.playlists = vec![playlist(mode, on_video_end)];
    st
}

#[test]
fn mode_selector_highlights_the_current_mode() {
    // 顺序循环（默认）
    let text = render_state(&state_with("sequential", "next"), 140, 40);
    assert!(text.contains("播放模式"), "页顶必须有模式选择器：\n{text}");
    assert!(
        text.contains("[ 顺序循环 ]"),
        "当前项（顺序循环）必须高亮：\n{text}"
    );
    assert!(
        text.contains("单视频循环") && text.contains("随机播放"),
        "三项都要显示"
    );

    // 随机播放
    let text = render_state(&state_with("random", "next"), 140, 40);
    assert!(
        text.contains("[ 随机播放 ]"),
        "高亮必须跟着状态走：\n{text}"
    );
    assert!(!text.contains("[ 顺序循环 ]"));

    // 单视频循环（只看 on_video_end 也能认出来）
    let text = render_state(&state_with("sequential", "loop"), 140, 40);
    assert!(
        text.contains("[ 单视频循环 ]"),
        "on_video_end=loop ⇒ 单视频循环：\n{text}"
    );
}

#[test]
fn mode_selector_without_playlist_gives_actionable_hint() {
    let mut st = liuguang_tui::AppState::new();
    st.page = Page::Playlists;
    let text = render_state(&st, 140, 40);
    assert!(text.contains("播放模式"), "\n{text}");
    assert!(
        text.contains("按 n 新建"),
        "没有播放列表时必须给出可操作提示（不静默）：\n{text}"
    );
}

/// 主界面（底栏）不再宣传高级键；`?` 的高级面板里能看到它们。
#[test]
fn advanced_keys_live_in_the_help_panel() {
    let mut app = liuguang_tui::App::new(MockControl::from_scenario(MockScenario::default()));
    app.state.page = Page::Playlists;
    app.state.playlists = vec![playlist("sequential", "next")];
    let main = render_state(&app.state, 140, 40);
    for leaked in ["[m]", "[v]", "[+/-]", "[A]", "[S]"] {
        assert!(
            !main.contains(leaked),
            "主界面不得再宣传高级键 {leaked}：\n{main}"
        );
    }
    assert!(main.contains("[←→]"), "主界面必须宣传 ←→ 切模式：\n{main}");
    assert!(main.contains("[a]"), "主界面必须宣传 a 加入条目：\n{main}");

    app.on_action(liuguang_tui::Action::Help);
    let help = render_state(&app.state, 140, 40);
    assert!(
        help.contains("高级操作"),
        "帮助面板必须有高级分区：\n{help}"
    );
    for k in ["m", "v", "r", "o", "+ / -", "p", "x", "A", "S"] {
        assert!(help.contains(k), "高级面板必须列出 {k:?}：\n{help}");
    }
}
