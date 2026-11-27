//! t24：播放列表编辑 + **Apply/Save 分离**。
//!
//! 关键纪律：`A` 只把编辑缓冲下发给 daemon（`update_playlist`，**只改运行时**），
//! `S` 才 `save_config` 落盘；**放弃编辑（`x`）既不下发也不落盘**。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_tui::{
    Action, App, AppState, Effect, MockCall, MockControl, MockScenario, Page, apply,
};

fn app() -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(MockScenario {
        outputs: 1,
        media: 3,
        ..MockScenario::default()
    }));
    app.state.page = Page::Playlists;
    app
}

fn has_update(app: &App<MockControl>) -> bool {
    app.client
        .calls
        .iter()
        .any(|c| matches!(c, MockCall::UpdatePlaylist(_)))
}

/// 编辑缓冲的字段必须**完整**映射到线上参数（否则 UI 改了但下发没改）。
#[test]
fn playlist_edit_maps_every_field_to_wire_params() {
    let mut st = AppState::new();
    apply(&mut st, Action::PlaylistNew);
    assert!(st.playlist_edit.is_some(), "n 应进入编辑缓冲");
    assert!(st.dirty, "进入编辑即应显示未保存标记");
    apply(&mut st, Action::PlaylistCycleMode);
    apply(&mut st, Action::PlaylistInterval(60));
    apply(&mut st, Action::PlaylistToggleVideoEnd);
    apply(&mut st, Action::PlaylistToggleNoRepeat);
    apply(&mut st, Action::PlaylistCycleOutput);
    let edit = st.playlist_edit.clone().expect("编辑缓冲");
    let p = edit.to_params();
    assert_eq!(p.mode, "random", "m 必须切换模式");
    assert_eq!(p.interval_seconds, 660, "+ 应加 60s");
    assert_eq!(p.on_video_end, "loop", "v 必须切换播完行为");
    assert!(!p.no_repeat, "r 必须切换（默认开 → 关）");
    assert!(p.playlist_id.is_none(), "新建时不下发 playlist_id");
    // A → 只在**运行时**下发；不得顺手 save。
    let fx = apply(&mut st, Action::Apply);
    assert!(
        matches!(fx.as_slice(), [Effect::UpdatePlaylist(_)]),
        "A 应只产生 UpdatePlaylist：{fx:?}"
    );
    assert!(
        !fx.iter().any(|e| matches!(e, Effect::SaveConfig)),
        "A 不得顺手写配置"
    );
    // S → 才落盘。
    let fx = apply(&mut st, Action::Save);
    assert!(
        fx.iter().any(|e| matches!(e, Effect::SaveConfig)),
        "S 必须落盘：{fx:?}"
    );
}

/// 放弃编辑：**不下发、不落盘**（这就是"改动后放弃不能悄悄写配置"的验收）。
#[test]
fn discarding_playlist_edit_calls_nothing() {
    let mut app = app();
    app.on_action(Action::PlaylistNew);
    app.on_action(Action::PlaylistCycleMode);
    app.on_action(Action::PlaylistInterval(60));
    app.on_action(Action::PlaylistDiscard);
    assert!(app.state.playlist_edit.is_none(), "x 应清掉编辑缓冲");
    assert!(!has_update(&app), "放弃编辑不得下发 update_playlist");
    assert!(
        !app.client.called_save_config(),
        "放弃编辑不得写入配置（save_config 调用次数必须为 0）"
    );
    assert!(!app.state.dirty, "放弃后不应再有未保存标记");
}

/// 应用 → 调用 update_playlist（运行时）；**仍不得** save_config；保存才 save_config。
#[test]
fn apply_then_save_separates_runtime_from_persistence() {
    let mut app = app();
    app.on_action(Action::PlaylistNew);
    app.on_action(Action::PlaylistToggleVideoEnd);
    app.on_action(Action::Apply);
    assert!(has_update(&app), "A 必须下发 update_playlist");
    assert!(
        !app.client.called_save_config(),
        "A 之后仍不得写配置（Apply/Save 分离）"
    );
    assert!(app.state.playlist_edit.is_none(), "下发成功后应退出编辑态");
    app.on_action(Action::Save);
    assert!(app.client.called_save_config(), "S 必须写配置");
}

/// 没有选中媒体项时 `p` 必须**明确提示**而不是静默无操作。
#[test]
fn add_entry_without_selection_warns() {
    let mut st = AppState::new();
    apply(&mut st, Action::PlaylistNew);
    let before = st.playlist_edit.as_ref().map(|e| e.entries.len());
    apply(&mut st, Action::PlaylistAddEntry);
    assert_eq!(
        st.playlist_edit.as_ref().map(|e| e.entries.len()),
        before,
        "没有选中项时不得加入条目"
    );
}

/// 编辑缓冲内新增/移除条目的顺序语义（顺序即播放顺序）。
#[test]
fn entry_add_and_remove_keeps_order() {
    use liuguang_ipc::protocol::{ListMediaResult, MediaItem};
    let mut st = AppState::new();
    apply(&mut st, Action::PlaylistNew);
    let mk = |id: &str| MediaItem {
        id: id.to_string(),
        source_id: "src-1".to_string(),
        name: id.to_string(),
        path: format!("/srv/wallpapers/{id}"),
        kind: "image".to_string(),
        size_bytes: 1,
        modified_unix: 0,
        details: None,
    };
    st.media = ListMediaResult {
        items: vec![mk("a.png"), mk("b.png"), mk("c.png")],
        total: 3,
        page: 0,
        page_size: 10,
        truncated: false,
        scanning: false,
        generated_unix_ms: 0,
        scan_id: 1,
        skipped_oversize: 0,
        skipped_examples: Vec::new(),
    };
    for i in 0..3 {
        st.media_cursor = i;
        apply(&mut st, Action::PlaylistAddEntry);
    }
    let e = st.playlist_edit.clone().expect("编辑缓冲");
    assert_eq!(e.entries, vec!["a.png", "b.png", "c.png"], "顺序即播放顺序");
    assert_eq!(e.cursor, 2, "加入后光标落在新条目");
    apply(&mut st, Action::PlaylistRemoveEntry);
    let e = st.playlist_edit.clone().expect("编辑缓冲");
    assert_eq!(e.entries, vec!["a.png", "b.png"], "d 应移除光标处条目");
}
