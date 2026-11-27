//! t24：剩余页面所需的 IPC 接线（`update_playlist` / `remove_source`）必须真的可用。
//!
//! 这一层用**线上类型**（`liuguang_ipc::protocol::*`）驱动 `MockControl`，
//! 断言的是"UI 将来会依赖的语义"：同名 id 是**覆盖**而不是追加；非法域值被拒；
//! 删除来源真的移除并留下可断言的调用记录（供"运行时改动 vs 落盘"区分）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_ipc::protocol::{PlaylistEntry, UpdatePlaylistParams};
use liuguang_tui::ipc_client::ControlClient;
use liuguang_tui::{MockCall, MockControl, MockScenario};

fn params(id: Option<&str>, name: &str, mode: &str) -> UpdatePlaylistParams {
    UpdatePlaylistParams {
        playlist_id: id.map(str::to_string),
        name: name.to_string(),
        entries: vec![PlaylistEntry {
            media_id: "/srv/wallpapers/a.png".to_string(),
        }],
        mode: mode.to_string(),
        interval_seconds: 30,
        on_video_end: "loop".to_string(),
        output: Some("DP-1".to_string()),
        no_repeat: true,
    }
}

#[test]
fn update_playlist_upserts_and_validates() {
    let mut c = MockControl::from_scenario(MockScenario {
        media: 3,
        ..MockScenario::default()
    });
    let created = c
        .update_playlist(params(None, "新单", "random"))
        .expect("新建应成功");
    assert_eq!(created.name, "新单");
    assert_eq!(created.mode, "random");
    let n = c.playlists.len();
    assert_eq!(n, 2, "应在 pl-1 之外新增一条：{n}");
    let updated = c
        .update_playlist(params(Some(&created.id), "改名", "sequential"))
        .expect("更新应成功");
    assert_eq!(updated.id, created.id);
    assert_eq!(c.playlists.len(), 2, "同 id 更新必须是覆盖");
    assert_eq!(
        c.playlists
            .iter()
            .find(|p| p.id == created.id)
            .map(|p| p.name.as_str()),
        Some("改名")
    );
    // 非法域值必须被拒（与服务端同义）。
    assert!(c.update_playlist(params(None, "x", "shuffle")).is_err());
    assert!(
        c.calls
            .iter()
            .any(|k| matches!(k, MockCall::UpdatePlaylist(_)))
    );
}

#[test]
fn remove_source_really_removes_and_records_call() {
    let mut c = MockControl::from_scenario(MockScenario {
        media: 3,
        ..MockScenario::default()
    });
    let victim = c
        .sources
        .sources
        .first()
        .map(|s| s.id.clone())
        .expect("mock 至少有一个来源");
    let before = c.sources.sources.len();
    c.remove_source(&victim).expect("删除来源应成功");
    assert_eq!(c.sources.sources.len(), before - 1);
    assert!(!c.sources.sources.iter().any(|s| s.id == victim));
    assert!(
        c.calls
            .iter()
            .any(|k| matches!(k, MockCall::RemoveSource(id) if id == &victim))
    );
}
