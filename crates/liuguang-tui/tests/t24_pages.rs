//! t24：系统页——硬解方式**必须来自 daemon 的 status**，不得按配置文件推断。
//!
//! （`?` 帮助页的"按页键位"由 `src/ui/widgets/modal.rs` 的 lib 测试覆盖，
//!  见 `help_is_page_aware_and_keys_do_not_conflict`——不在这里重复实现。）

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use liuguang_tui::{Action, App, Connection, MockCall, MockControl, MockScenario, Page, apply, ui};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

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
            // 全角字符占两格：跳过续格，否则会读成"夜 班 轮 换"
            let w = unicode_width::UnicodeWidthStr::width(sym);
            if w > 1 {
                skip = w - 1;
            }
        }
        out.push('\n');
    }
    out
}

fn app() -> App<MockControl> {
    App::new(MockControl::from_scenario(MockScenario {
        outputs: 1,
        media: 3,
        ..MockScenario::default()
    }))
}

/// 系统页必须显示 **daemon 报告的**硬解方式（而不是"我们认为的"）。
#[test]
fn system_page_shows_decoder_from_daemon() {
    let mut app = app();
    app.state.page = Page::System;
    // daemon 说的话就是唯一来源。
    if let Some(ch) = app.state.status.channels.first_mut() {
        ch.decoder = "vaapi:vah264dec(nv12)".to_string();
    } else {
        app.state.status.decoder = "vaapi:vah264dec(nv12)".to_string();
    }
    let text = render(&app, 140, 40);
    assert!(
        text.contains("vaapi:vah264dec"),
        "系统页必须显示 daemon 报告的硬解方式：\n{text}"
    );
    assert!(
        text.contains("daemon status"),
        "必须注明来源是 daemon status（不是配置推断）：\n{text}"
    );
}

// ───────────────────────── 交互驱动辅助 ─────────────────────────

fn key(app: &mut App<MockControl>, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn char_key(app: &mut App<MockControl>, c: char) {
    key(app, KeyCode::Char(c));
}

fn app_with(scenario: MockScenario) -> App<MockControl> {
    let mut a = App::new(MockControl::from_scenario(scenario));
    a.refresh_all();
    a
}

// ───────────────────────── 播放列表重命名（真正的文本输入） ─────────────────────────

#[test]
fn playlist_rename_uses_text_input_and_marks_unsaved() {
    let mut app = app_with(MockScenario::default());
    app.on_action(Action::Goto(Page::Playlists));
    app.on_action(Action::PlaylistEdit);
    let calls_before = app.client.calls().len();

    app.on_action(Action::PlaylistRename);
    assert!(app.state.modal_open(), "重命名应打开文本输入");
    assert_eq!(
        app.client.calls().len(),
        calls_before,
        "打开输入不应产生任何 IPC 调用"
    );

    app.state.editor.set_text("夜班轮换");
    let shown = render(&app, 140, 40);
    assert!(shown.contains("夜班轮换"), "编辑器内容应可见：\n{shown}");

    key(&mut app, KeyCode::Enter);
    assert!(!app.state.modal_open(), "Enter 应关闭弹窗");
    assert_eq!(
        app.state.playlist_edit.as_ref().map(|e| e.name.as_str()),
        Some("夜班轮换"),
        "名称应写入编辑缓冲"
    );
    assert!(app.state.dirty, "改名后应处于未保存状态");
    assert!(
        !app.client.called_save_config(),
        "改名本身**不得**写配置（save_config 必须为 0 次）"
    );
    let lines: Vec<&str> = render(&app, 140, 40)
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>()
        .leak()
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert!(lines[..2].join(" ").contains('*'), "顶栏应显示未保存标记 *");
}

#[test]
fn playlist_rename_escape_keeps_old_name() {
    let mut app = app_with(MockScenario::default());
    app.on_action(Action::Goto(Page::Playlists));
    app.on_action(Action::PlaylistEdit);
    let old = app
        .state
        .playlist_edit
        .as_ref()
        .map(|e| e.name.clone())
        .unwrap();
    app.on_action(Action::PlaylistRename);
    app.state.editor.set_text("不该生效");
    key(&mut app, KeyCode::Esc);
    assert!(!app.state.modal_open());
    assert_eq!(
        app.state.playlist_edit.as_ref().map(|e| e.name.clone()),
        Some(old),
        "Esc 放弃后名称必须不变"
    );
    assert!(!app.client.called_save_config(), "放弃不得写配置");
}

// ───────────────────────── 来源管理 [S] ─────────────────────────

#[test]
fn source_manage_params_do_not_call_ipc_and_are_pushed_down() {
    let mut app = app_with(MockScenario::default());
    app.on_action(Action::Goto(Page::Library));
    app.on_action(Action::SourceManage);
    assert!(app.state.modal_open(), "S 应打开来源管理");

    let before = app.client.calls().len();
    char_key(&mut app, 'r');
    char_key(&mut app, 'k');
    assert!(app.state.source_recursive, "r 应打开递归");
    assert_ne!(app.state.source_kind, "image", "k 应切换类型");
    assert_eq!(app.client.calls().len(), before, "切换参数本身不得发 IPC");

    char_key(&mut app, 'a');
    assert!(app.state.modal_open(), "a 应打开文本输入");
    app.state.editor.set_text("/srv/wallpapers/新目录");
    key(&mut app, KeyCode::Enter);

    let added = app
        .client
        .calls()
        .iter()
        .find_map(|c| match c {
            MockCall::AddSource(kind, path, recursive) => {
                Some((kind.clone(), path.clone(), *recursive))
            }
            _ => None,
        })
        .expect("应下发 add_source");
    assert_eq!(added.1, "/srv/wallpapers/新目录");
    assert!(added.2, "递归开关必须下推给 daemon");
    assert!(app.state.sources_dirty, "运行时改动应标记为未保存");
    assert!(
        !app.client.called_save_config(),
        "新增来源**不得**自动落盘（需显式确认）"
    );
}

#[test]
fn source_manage_remove_is_runtime_only_and_save_needs_confirmation() {
    let mut app = app_with(MockScenario::default());
    app.on_action(Action::Goto(Page::Library));
    app.on_action(Action::SourceManage);
    let before = app.state.sources.sources.len();

    char_key(&mut app, 'd');
    assert!(
        app.client
            .calls()
            .iter()
            .any(|c| matches!(c, MockCall::RemoveSource(_))),
        "d 应下发 remove_source"
    );
    assert!(
        app.state.sources.sources.len() < before,
        "删除应在运行时生效"
    );
    assert!(app.state.sources_dirty);
    assert!(!app.client.called_save_config(), "删除不得自动落盘");

    // `s` → 弹确认；确认前 / 取消后都不得落盘
    char_key(&mut app, 's');
    assert!(app.state.modal_open(), "s 应弹出保存确认");
    assert!(
        !app.client.called_save_config(),
        "确认之前不得调用 save_config"
    );
    key(&mut app, KeyCode::Esc);
    assert!(!app.client.called_save_config(), "取消确认后仍不得落盘");

    // 再来一次并确认 → 才落盘（t38 起 Esc 会彻底关闭模态，故先按 `S` 重新打开来源管理）
    char_key(&mut app, 'S');
    assert!(app.state.modal_open(), "S 应重新打开来源管理");
    char_key(&mut app, 's');
    key(&mut app, KeyCode::Enter);
    assert!(
        app.client.called_save_config(),
        "确认后应调用 save_config（唯一落盘点）"
    );
}

#[test]
fn source_manage_modal_renders_keys_recursive_and_last_scan() {
    let mut app = app_with(MockScenario::default());
    app.on_action(Action::Goto(Page::Library));
    app.on_action(Action::SourceManage);
    app.state.sources_dirty = true;
    app.state.scan = liuguang_tui::ScanState::Finished {
        scan_id: 2,
        found: 7,
    };
    let shown = render(&app, 130, 34);
    for needle in [
        "/srv/wallpapers",
        "images",
        "递归",
        "需确认",
        "上次扫描",
        "未保存",
    ] {
        assert!(
            shown.contains(needle),
            "来源管理弹窗缺少 {needle}：\n{shown}"
        );
    }
}

// ───────────────────────── 转场 Disabled 标注（渲染层） ─────────────────────────

#[test]
fn effects_page_marks_unimplemented_as_disabled_in_render() {
    for (w, h) in [(140u16, 40u16), (100, 30), (70, 22)] {
        let mut app = app_with(MockScenario::default());
        app.on_action(Action::Goto(Page::Effects));
        if (w, h) == (70, 22) {
            // 小档是单栏：`←→` 切到"详情"子页才看得到可用性列表（真实用户路径）
            app.on_action(Action::SubPageMove(true));
            assert_eq!(app.state.subpage, liuguang_tui::SubPage::Detail);
        }
        let shown = render(&app, w, h);
        assert!(
            shown.contains("Disabled") || shown.contains("未实现"),
            "{w}x{h} 转场页必须区分未实现项：\n{shown}"
        );
        assert!(
            shown.contains("fade"),
            "{w}x{h} 应列出已实现效果：\n{shown}"
        );
    }
}

// ───────────────────────── 帮助页（渲染层，按页） ─────────────────────────

#[test]
fn help_render_is_page_aware() {
    let mut app = app_with(MockScenario::default());
    app.on_action(Action::Goto(Page::Playlists));
    app.on_action(Action::Help);
    let playlists_help = render(&app, 140, 40);
    assert!(playlists_help.contains("重命名"), "{playlists_help}");
    assert!(!playlists_help.contains("fit"), "{playlists_help}");

    app.on_action(Action::Cancel);
    app.on_action(Action::Goto(Page::Outputs));
    app.on_action(Action::Help);
    let outputs_help = render(&app, 140, 40);
    assert!(outputs_help.contains("fit"), "{outputs_help}");
    assert!(!outputs_help.contains("重命名"), "{outputs_help}");
}

// ───────────────────────── 系统页：会话/协议/最近错误 ─────────────────────────

#[test]
fn system_page_shows_session_protocol_and_recent_errors() {
    let mut app = app_with(MockScenario::playing());
    app.state.push_error("提交失败：Permission denied");
    app.state.push_error("媒体列表：DEVICE_UNAVAILABLE");
    app.on_action(Action::Goto(Page::System));
    let shown = render(&app, 140, 40);
    assert!(shown.contains("会话 mock"), "应显示会话种类：\n{shown}");
    assert!(shown.contains("协议 v"), "应显示协议版本：\n{shown}");
    assert!(
        shown.contains("daemon 未提供该字段"),
        "daemon 版本缺失时必须如实说明：\n{shown}"
    );
    assert!(shown.contains("最近错误"), "{shown}");
    assert!(shown.contains("Permission denied"), "{shown}");
    assert!(
        shown.contains("重连尝试") || shown.contains("按 e 重连"),
        "{shown}"
    );
}

#[test]
fn system_page_reconnect_recovers_and_failure_keeps_reason() {
    let mut ok = app_with(MockScenario::disconnected());
    assert!(!ok.state.connection.is_online());
    ok.on_action(Action::Reconnect);
    assert_eq!(ok.state.reconnect_attempts, 1);
    assert!(ok.state.connection.is_online(), "重连成功应回到在线");

    let mut bad = app_with(MockScenario {
        reconnect_fails: true,
        ..MockScenario::disconnected()
    });
    bad.on_action(Action::Reconnect);
    match &bad.state.connection {
        Connection::Offline(reason) => assert!(reason.contains("DEVICE_UNAVAILABLE"), "{reason}"),
        other => panic!("重连失败应保持 Offline，得到 {other:?}"),
    }
    assert!(
        bad.state
            .notifications
            .iter()
            .any(|n| n.text.contains("重连失败")),
        "失败原因必须可见"
    );
}

// ───────────────────────── Apply / Save 分离（跨页再证） ─────────────────────────

#[test]
fn abandoning_all_edits_never_writes_config() {
    let mut app = app_with(MockScenario::default());
    // 播放列表：改名 + 放弃
    app.on_action(Action::Goto(Page::Playlists));
    app.on_action(Action::PlaylistEdit);
    app.on_action(Action::PlaylistRename);
    app.state.editor.set_text("临时名");
    key(&mut app, KeyCode::Esc);
    apply(&mut app.state, Action::PlaylistDiscard);
    // 来源：新增 + 不保存
    app.on_action(Action::Goto(Page::Library));
    app.on_action(Action::SourceManage);
    char_key(&mut app, 'a');
    app.state.editor.set_text("/srv/wallpapers/tmp");
    key(&mut app, KeyCode::Enter);
    key(&mut app, KeyCode::Esc);
    // 显示器：改 fit 不保存
    app.on_action(Action::Goto(Page::Outputs));
    app.state.selected_output = Some("DP-1".to_string());
    apply(&mut app.state, Action::CycleFit(true));

    let saves = app
        .client
        .calls()
        .iter()
        .filter(|c| matches!(c, MockCall::SaveConfig))
        .count();
    assert_eq!(saves, 0, "放弃全部编辑后 save_config 必须是 0 次");
    let _ = Rect::new(0, 0, 80, 24);
}

// ───────────────────────── Save 的真实失败路径（真 IPC + PERMISSION_DENIED） ─────────────────────────

#[test]
fn save_permission_denied_is_surfaced_and_never_claims_success() {
    use liuguang_ipc::protocol::{ErrorBody, ErrorKind, Method, Request, Response};
    use liuguang_ipc::reply_only;
    use liuguang_ipc::server::{IpcServer, ServerConfig};
    use liuguang_tui::IpcControl;

    let mut sock = std::env::temp_dir();
    sock.push(format!(
        "liuguang-tui-t24-perm-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let server = IpcServer::bind(ServerConfig {
        socket_path: sock.clone(),
        ..ServerConfig::default()
    })
    .expect("bind");
    let handle = server.spawn(reply_only(|req: Request| match req.method {
        Method::SaveConfig => {
            let mut body = ErrorBody::new(
                ErrorKind::PermissionDenied,
                "无权写入 /etc/liuguang/config.toml",
            );
            body.detail_code = Some("PERMISSION_DENIED".to_string());
            Response::err(req.id, body)
        }
        Method::Status => Response::ok(
            req.id,
            serde_json::json!({
                "state": "StaticIdle", "paused": false, "display_backend": "drm",
                "decoder": "static", "uptime_seconds": 1
            }),
        ),
        Method::Outputs => Response::ok(req.id, serde_json::json!({"outputs": []})),
        _ => Response::err(req.id, ErrorBody::new(ErrorKind::NotImplemented, "x")),
    }));

    let mut app = App::new(IpcControl::connect(&sock));
    app.refresh_all();
    app.state.dirty = true;
    app.on_action(Action::Save);

    let notes: String = app
        .state
        .notifications
        .iter()
        .map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        notes.contains("PERMISSION_DENIED"),
        "保存失败必须给出明确 PERMISSION_DENIED（可读错误码）：\n{notes}"
    );
    assert!(
        notes.contains("无权写入"),
        "应保留 daemon 的可读说明：\n{notes}"
    );
    assert!(
        app.state.dirty,
        "保存失败后**不得**清除未保存标记（不能假装已保存）"
    );
    let shown = render(&app, 140, 40);
    assert!(
        shown.contains("PERMISSION_DENIED") || shown.contains("保存失败"),
        "失败原因必须对用户可见：\n{shown}"
    );
    handle.shutdown();
}
