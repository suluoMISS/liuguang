//! t70：静态壁纸页（第 7 页）——**接口就绪但可降级**的证据。
//!
//! 设计约束（t70 的硬规矩）：
//!
//! * `a` / `c` 必须发**真实 IPC**（`set_static_wallpaper` / `clear_static_wallpaper`），
//!   **不得** mock 成"看起来能跑"；
//! * daemon 侧尚未实现（t69 已取消）⇒ 服务端回 `UNSUPPORTED_METHOD`，
//!   TUI 必须给**可操作**提示（说清是 daemon 未实现 + 给临时替代），
//!   既不静默失败、也不谎报成功；
//! * 端到端（真机 daemon 真设为静态壁纸）在 t69 落地前 **UNVERIFIED** ——
//!   本文件只证明 TUI 侧的语义与降级路径。
//!
//! 之所以用 `TestBackend` 直接断言缓冲区：真机 pty 抓屏是增量重绘，旧帧会残留。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::{
    Action, App, ControlClient, MockCall, MockControl, MockScenario, NotificationKind, Page,
    StaticCallError, ui,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render<C: liuguang_tui::Session>(app: &App<C>, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("TestBackend");
    term.draw(|f| ui::draw(f, &app.state)).expect("draw");
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

/// 走**真实**数据通路把媒体库装进来（`Goto(Library)` ⇒ `list_media`），
/// 再切回静态壁纸页——不手搓 `state.media`，否则测的就不是真实流程了。
fn app_with(static_supported: bool) -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(MockScenario {
        media: 3,
        outputs: 1,
        long_names: false,
        static_supported,
        ..MockScenario::default()
    }));
    app.on_action(Action::Goto(Page::Library));
    assert_eq!(app.state.media.items.len(), 3, "媒体库必须真加载进来");
    app.state.page = Page::Static;
    // 媒体库索引 0/1 = 图片，2 = 视频（见 make_library）
    app.state.media_cursor = 0;
    app
}

/// 真实 daemon（未来 t69）已实现静态壁纸的场景。
fn supported_app() -> App<MockControl> {
    app_with(true)
}

fn last_text(app: &App<MockControl>) -> String {
    app.state
        .notifications
        .last()
        .map(|n| n.text.clone())
        .unwrap_or_default()
}

#[test]
fn fresh_state_shows_not_set_and_is_explicit() {
    let mut app = supported_app();
    let text = render(&app, 140, 40);
    assert!(text.contains("静态壁纸"), "页面标题缺失：\n{text}");
    assert!(
        text.contains("（未设置）"),
        "未设置时必须显式写出来，不能空白：\n{text}"
    );
    // 页面自述怎么设置（否则用户不知道要去媒体库选图）
    assert!(text.contains('a'), "缺少 `a` 的说明：\n{text}");
    app.state.page = Page::Static;
    assert!(render(&app, 140, 40).contains("当前播放源"));
}

#[test]
fn set_image_records_path_and_switches_playback_source() {
    let mut app = supported_app();
    // 先让播放源有"播放列表"的痕迹（daemon 未回 playback_source 时靠推断）
    app.state.playlists = vec![liuguang_ipc::protocol::Playlist {
        id: "pl-1".into(),
        name: "默认轮换".into(),
        entries: Vec::new(),
        mode: "sequential".into(),
        interval_seconds: 600,
        on_video_end: "next".into(),
        output: None,
        no_repeat: false,
    }];
    app.on_action(Action::StaticSet);

    // 真的下发了 IPC（而不是本地假装）
    assert!(
        app.client.calls.iter().any(
            |c| matches!(c, MockCall::SetStaticWallpaper(p) if p.contains("wallpaper_00000.png"))
        ),
        "必须下发 set_static_wallpaper，实际调用：{:?}",
        app.client.calls
    );
    let want = "/srv/wallpapers/收藏/2026/wallpaper_00000.png";
    assert_eq!(app.state.static_wallpaper.as_deref(), Some(want));

    let text = render(&app, 140, 40);
    assert!(
        text.contains("wallpaper_00000.png"),
        "页面必须显示完整路径：\n{text}"
    );
    // 播放源：静态壁纸（本次由 TUI 自己设为静态，不是推断）
    assert!(text.contains("静态壁纸"), "播放源应显示静态壁纸：\n{text}");
    assert!(
        !text.contains("（推断）"),
        "自己设过的源不该标推断：\n{text}"
    );

    // 成功反馈必须说清"暂停播放、静止显示"（用户要知道 CPU 会降下来）
    let n = app.state.notifications.last().expect("成功必须有提示");
    assert_eq!(n.kind, NotificationKind::Info);
    assert!(n.text.contains("静态壁纸"), "{}", n.text);
}

#[test]
fn clear_restores_playlist_playback() {
    let mut app = supported_app();
    app.on_action(Action::StaticSet);
    assert!(app.state.static_wallpaper.is_some());

    app.on_action(Action::StaticClear);
    assert!(
        app.client
            .calls
            .iter()
            .any(|c| matches!(c, MockCall::ClearStaticWallpaper)),
        "必须下发 clear_static_wallpaper，实际调用：{:?}",
        app.client.calls
    );
    assert_eq!(app.state.static_wallpaper, None);

    let text = render(&app, 140, 40);
    assert!(text.contains("（未设置）"), "清除后回到未设置：\n{text}");
    let last = last_text(&app);
    assert!(
        last.contains("恢复") && last.contains("播放列表"),
        "清除后的反馈要说清恢复了播放列表：{last}"
    );
}

#[test]
fn non_image_selection_is_refused_without_touching_ipc() {
    let mut app = supported_app();
    app.state.media_cursor = 2; // 视频
    let before = app.client.calls.len();
    app.on_action(Action::StaticSet);

    assert_eq!(
        app.client.calls.len(),
        before,
        "被拒绝时**不得**下发任何 IPC：{:?}",
        &app.client.calls[before..]
    );
    assert_eq!(app.state.static_wallpaper, None);

    let n = app.state.notifications.last().expect("拒绝必须有提示");
    assert_eq!(n.kind, NotificationKind::Warn);
    assert!(n.text.contains("图片"), "理由要可操作：{}", n.text);
    assert!(
        n.text.contains('W'),
        "要给出替代动作（`W` 直接应用到输出）：{}",
        n.text
    );
}

#[test]
fn unsupported_daemon_degrades_with_actionable_message() {
    // 默认场景：daemon **没有**这两个方法（=当下真实的 liuguang）
    let mut app = App::new(MockControl::from_scenario(MockScenario {
        media: 3,
        outputs: 1,
        long_names: false,
        ..MockScenario::default()
    }));
    app.on_action(Action::Goto(Page::Library));
    app.state.page = Page::Static;
    app.state.media_cursor = 0;

    app.on_action(Action::StaticSet);

    // ① 没有谎报成功
    assert_eq!(app.state.static_wallpaper, None, "不支持时不得本地记账");
    // ② 记下"不支持"，页面上要能一直看见（不是一闪而过的 toast）
    assert_eq!(app.state.static_supported, Some(false));
    // ③ 提示可操作：说清是 daemon 侧未实现 + 给替代路径
    //    （真机实测：底栏只有一行，长报文会把"临时办法"挤掉 ⇒ 通知是**短句**）
    let n = app.state.notifications.last().expect("必须报错，不得静默");
    assert_eq!(n.kind, NotificationKind::Error);
    assert!(
        n.text.contains("未实现") && n.text.contains("daemon"),
        "要说清是谁没实现：{}",
        n.text
    );
    assert!(
        n.text.contains("不是你的操作问题"),
        "不能让用户以为自己按错了：{}",
        n.text
    );
    assert!(n.text.contains('W'), "要给出替代动作：{}", n.text);
    assert!(
        unicode_width::UnicodeWidthStr::width(n.text.as_str()) <= 96,
        "通知超过一行可显示宽度（会被截断、丢掉动作）：{} 列",
        unicode_width::UnicodeWidthStr::width(n.text.as_str())
    );
    // ④ 完整报文（daemon 的原话）必须留在"最近错误"里，供系统页排查
    let err = app.state.recent_errors.last().expect("完整报文要留档");
    assert!(
        err.contains("set_static_wallpaper"),
        "完整报文要含方法名：{err}"
    );

    let text = render(&app, 140, 40);
    assert!(
        text.contains("不支持"),
        "降级状态要在页面上常驻可见：\n{text}"
    );
}

#[test]
fn clear_also_degrades_honestly() {
    let mut app = App::new(MockControl::from_scenario(MockScenario {
        media: 3,
        outputs: 1,
        long_names: false,
        ..MockScenario::default()
    }));
    app.on_action(Action::Goto(Page::Library));
    app.state.page = Page::Static;
    app.on_action(Action::StaticClear);
    let n = app.state.notifications.last().expect("必须报错");
    assert_eq!(n.kind, NotificationKind::Error);
    assert!(n.text.contains("未实现"), "{}", n.text);
    assert!(
        n.text.contains("播放列表"),
        "要给出恢复播放的替代路径：{}",
        n.text
    );
    assert!(
        app.state
            .recent_errors
            .last()
            .is_some_and(|e| e.contains("clear_static_wallpaper")),
        "完整报文要留档：{:?}",
        app.state.recent_errors
    );
}

#[test]
fn page_hints_match_handled_actions() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use liuguang_tui::{Focus, action_for};
    use std::time::Instant;

    let mut app = supported_app();
    for focus in [Focus::Nav, Focus::List, Focus::Detail, Focus::Actions] {
        app.state.focus = focus;
        for code in [KeyCode::Char('a'), KeyCode::Char('c')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            let got = action_for(&app.state, key);
            let want = match code {
                KeyCode::Char('a') => Some(Action::StaticSet),
                _ => Some(Action::StaticClear),
            };
            assert_eq!(got, want, "focus={focus:?} code={code:?}");
        }
    }
    let _ = Instant::now();
}

#[test]
fn unsupported_is_reported_by_the_client_layer_too() {
    // 直接对客户端断言：默认场景必须返回 `Unsupported`，而不是 Ok/其它错误
    let mut c = MockControl::from_scenario(MockScenario::default());
    match c.set_static_wallpaper("/srv/x.png") {
        Err(StaticCallError::Unsupported(m)) => {
            assert!(m.contains("set_static_wallpaper"), "{m}");
        }
        other => panic!("默认场景必须降级为 Unsupported，实际 {other:?}"),
    }
}
