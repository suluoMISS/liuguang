//! t38：① 提交成功后模态必须关闭 + 列表刷新 + 光标落在新增项，且 `Esc` 永远能关闭模态
//! （用户不得被困在弹窗里）；② 失败保证语"（当前壁纸未改变）"在窄/宽终端下都必须在屏上；
//! ③ 保存确认框用 daemon 回传的实际配置路径；④ daemon 的 `conflicts` 名字直接展示。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use liuguang_tui::{Action, App, ApplyPhase, MockControl, MockScenario, Modal, Page};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render<C: liuguang_tui::Session>(app: &App<C>, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("TestBackend");
    term.draw(|f| liuguang_tui::ui::draw(f, &app.state))
        .expect("draw");
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

fn app_with(scenario: MockScenario) -> App<MockControl> {
    let mut app = App::new(MockControl::from_scenario(scenario));
    app.refresh_all();
    app
}

fn press(app: &mut App<MockControl>, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn release(app: &mut App<MockControl>, code: KeyCode) {
    let mut ev = KeyEvent::new(code, KeyModifiers::NONE);
    ev.kind = KeyEventKind::Release;
    app.on_key(ev);
}

fn char_press(app: &mut App<MockControl>, c: char) {
    press(app, KeyCode::Char(c));
}

fn open_source_manage(app: &mut App<MockControl>) {
    app.on_action(Action::Goto(Page::Library));
    app.on_action(Action::SourceManage);
    assert!(app.state.modal_open(), "来源管理应打开");
}

// ───────── ① 提交成功后：模态关闭 + 列表刷新 + 光标在新增项 ─────────

#[test]
fn submit_closes_modal_refreshes_list_and_focuses_new_source() {
    let mut app = app_with(MockScenario::default());
    open_source_manage(&mut app);
    let before = app.state.sources.sources.len();

    char_press(&mut app, 'a');
    assert!(app.state.modal_open(), "a 应打开文本输入");
    app.state.editor.set_text("/srv/wallpapers/srcB");
    press(&mut app, KeyCode::Enter);

    // ① 模态**必须关闭**（否则用户被困在弹窗里、后续按键被输入框吃掉）
    assert!(
        !app.state.modal_open(),
        "提交成功后模态必须关闭（当前：{:?}）",
        app.state.modal
    );
    // ② 列表必须刷新（daemon 权威结果）
    assert_eq!(
        app.state.sources.sources.len(),
        before + 1,
        "来源列表应刷新出新项：{:?}",
        app.state.sources.sources
    );
    // ③ 光标落在新增项
    let selected = app
        .state
        .sources
        .sources
        .get(app.state.sources_cursor)
        .expect("应有选中项");
    assert_eq!(selected.name, "srcB", "光标应落在新增项");

    // ④ 重新打开来源管理后按 `d` 必须真的执行删除（此前被输入框吃掉）
    char_press(&mut app, 'S');
    assert!(app.state.modal_open(), "S 应重新打开来源管理");
    press(&mut app, KeyCode::Char('d'));
    assert!(
        app.client.calls().iter().any(|c| matches!(
            c,
            liuguang_tui::MockCall::RemoveSource(id) if id == "/srv/wallpapers/srcB"
        )),
        "关闭模态后 `d` 必须能删掉刚新增的来源：{:?}",
        app.client.calls()
    );
}

// ───────── ② `Esc` 在**任意**模态下都能关闭 ─────────

#[test]
fn esc_closes_every_modal_variant() {
    let variants = vec![
        Modal::PathInput {
            purpose: "设为壁纸".to_string(),
        },
        Modal::SourceManage,
        Modal::Help,
        Modal::PathText {
            title: "复制路径".to_string(),
            text: "/srv/a.png".to_string(),
        },
        Modal::Confirm {
            question: "写入配置？".to_string(),
            action: "source_save".to_string(),
        },
        Modal::TextInput {
            title: "重命名".to_string(),
            target: liuguang_tui::TextTarget::PlaylistName,
            hint: "Enter 提交".to_string(),
        },
    ];
    for modal in variants {
        let mut app = app_with(MockScenario::default());
        let name = format!("{modal:?}");
        app.state.open_modal(modal);
        assert!(app.state.modal_open(), "{name} 应已打开");
        press(&mut app, KeyCode::Esc);
        assert!(
            !app.state.modal_open(),
            "Esc 必须能关闭模态（{name}）；不得把用户困在弹窗里"
        );
    }
}

/// `Esc` 连按两次也一定能出来（例如"保存确认"取消后会回到来源管理）。
#[test]
fn esc_twice_always_escapes_nested_flow() {
    let mut app = app_with(MockScenario::default());
    app.state.sources_dirty = true;
    open_source_manage(&mut app);
    char_press(&mut app, 's'); // 弹保存确认
    assert!(app.state.modal_open());
    press(&mut app, KeyCode::Esc); // 取消 → 回到来源管理（仍在模态里，属设计）
    press(&mut app, KeyCode::Esc); // 再按一次必须彻底关闭
    assert!(
        !app.state.modal_open(),
        "连按两次 Esc 必须能完全退出模态（当前：{:?}）",
        app.state.modal
    );
}

// ───────── ③ 终端上报的"松开"事件不得被当成第二次按键 ─────────

#[test]
fn release_key_events_are_ignored() {
    let mut app = app_with(MockScenario::default());
    open_source_manage(&mut app);
    char_press(&mut app, 'a'); // 打开文本输入
    app.state.editor.set_text("/srv/");

    // 松开事件：不得插入字符、不得关闭、不得打开任何东西
    release(&mut app, KeyCode::Char('x'));
    release(&mut app, KeyCode::Enter);
    release(&mut app, KeyCode::Esc);
    assert!(app.state.modal_open(), "松开事件不得关闭模态");
    assert_eq!(
        app.state.editor.text(),
        "/srv/",
        "松开事件不得插入/删除字符"
    );

    // 按下事件仍然正常工作
    char_press(&mut app, 'x');
    assert_eq!(app.state.editor.text(), "/srv/x");
    press(&mut app, KeyCode::Esc);
    assert!(!app.state.modal_open(), "按下 Esc 必须关闭");
}

// ───────── ④ 保证语必须在屏上（120 / 200 / 窄） ─────────

#[test]
fn wallpaper_unchanged_guarantee_is_visible_at_all_widths() {
    let mut app = app_with(MockScenario::default());
    app.state.page = Page::Library;
    // 故意造一个很长的失败原因：截断只能伤到原因，不能吃掉保证语
    app.state.apply_phase = Some(ApplyPhase::Failed(
        "daemon 反馈：解码失败 UNSUPPORTED_CODEC；已回退到上一张；建议检查 LIBVA_DRIVER_NAME 与 i965 驱动是否可用，并查看 journalctl -u liuguang 的完整日志".to_string(),
    ));
    // 50x15 属 Tiny 档（只显示"终端过小"横幅，本就不渲染页面内容），故不在此列
    for (w, h) in [(200u16, 40u16), (120, 40), (70, 22)] {
        let shown = render(&app, w, h);
        assert!(
            shown.contains("当前壁纸未改变"),
            "{w}x{h} 下保证语必须在屏上（含 120/200 列）：\n{shown}"
        );
        assert!(shown.contains("应用失败"), "{w}x{h} 应显示失败：\n{shown}");
        // 保证语必须在**被截断的部分之前**（否则长原因会把它挤掉）
        let guarantee_at = shown.find("当前壁纸未改变").unwrap();
        let reason_at = shown.find("UNSUPPORTED_CODEC").unwrap_or(usize::MAX);
        assert!(
            guarantee_at < reason_at,
            "{w}x{h} 保证语应前置：guarantee@{guarantee_at} reason@{reason_at}"
        );
    }
}

// ───────── ⑤ 保存确认框用 daemon 回传的实际路径（不写死） ─────────

#[test]
fn save_confirm_uses_config_path_returned_by_daemon() {
    let mut app = app_with(MockScenario {
        save_path: "/tmp/cfg-nolegacy.toml".to_string(),
        ..MockScenario::default()
    });
    open_source_manage(&mut app);

    // 第一次保存：确认框还不知道路径（如实说明），保存后 daemon 回传
    app.state.sources_dirty = true;
    char_press(&mut app, 's');
    let first = render(&app, 140, 40);
    assert!(
        first.contains("路径将在本次保存后由 daemon 回传"),
        "首次确认不得写死路径：\n{first}"
    );
    assert!(
        !first.contains("/etc/liuguang/config.toml"),
        "首次确认不得出现写死的规范路径：\n{first}"
    );
    press(&mut app, KeyCode::Enter); // 确认 → save_config
    assert_eq!(
        app.state.config_path.as_deref(),
        Some("/tmp/cfg-nolegacy.toml"),
        "应记住 daemon 回传的实际路径"
    );

    // 第二次确认：必须显示响应里的那个路径
    app.on_action(Action::SourceManage);
    app.state.sources_dirty = true;
    char_press(&mut app, 's');
    let second = render(&app, 140, 40);
    assert!(
        second.contains("/tmp/cfg-nolegacy.toml"),
        "确认框必须显示 daemon 回传的实际路径：\n{second}"
    );
    assert!(
        !second.contains("/etc/liuguang/config.toml"),
        "确认框不得再写死规范路径：\n{second}"
    );
}

// ───────── ⑥ daemon 的 conflicts 名字直接展示（不自行推断容器） ─────────

#[test]
fn conflicts_from_daemon_are_surfaced_verbatim() {
    use liuguang_ipc::protocol::{ErrorBody, ErrorKind, Method, Request, Response};
    use liuguang_ipc::reply_only;
    use liuguang_ipc::server::{IpcServer, ServerConfig};
    use liuguang_tui::IpcControl;

    let mut sock = std::env::temp_dir();
    sock.push(format!(
        "liuguang-tui-t38-conflict-{}-{}.sock",
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
            let mut body = ErrorBody::new(ErrorKind::InvalidParams, "未做任何改动");
            body.detail_code = Some("NO_EFFECTIVE_CHANGE".to_string());
            body.conflicts = vec!["[slideshow].directory".to_string()];
            Response::err(req.id, body)
        }
        Method::Status => Response::ok(
            req.id,
            serde_json::json!({"state":"StaticIdle","paused":false,"display_backend":"drm",
                "decoder":"static","uptime_seconds":1}),
        ),
        Method::Outputs => Response::ok(req.id, serde_json::json!({"outputs": []})),
        _ => Response::err(req.id, ErrorBody::new(ErrorKind::NotImplemented, "x")),
    }));

    let mut app = App::new(IpcControl::connect(&sock));
    app.refresh_all();
    app.on_action(Action::Save);
    let notes: String = app
        .state
        .notifications
        .iter()
        .map(|n| n.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        notes.contains("NO_EFFECTIVE_CHANGE"),
        "应保留 daemon 的细分码：\n{notes}"
    );
    assert!(
        notes.contains("[slideshow].directory"),
        "必须直接用 conflicts 里的容器名（不得自行推断）：\n{notes}"
    );
    assert!(
        !notes.contains("library.root"),
        "已证伪的说法不得再出现：\n{notes}"
    );
    handle.shutdown();
}

// ───────── ⑦ 不依赖屏幕抓取的决定性判据（供复验引用） ─────────

/// `Esc` 在 **Confirm / TextInput / PathText** 三个模态下的决定性判据：
/// 直接看状态（`state.modal`），**不做任何屏幕抓取**。
#[test]
fn esc_closes_confirm_text_input_and_path_text_without_screen_scraping() {
    let variants = [
        (
            "Confirm",
            Modal::Confirm {
                question: "写入配置？".to_string(),
                action: "source_save".to_string(),
            },
        ),
        (
            "TextInput",
            Modal::TextInput {
                title: "重命名".to_string(),
                target: liuguang_tui::TextTarget::PlaylistName,
                hint: "Enter 提交".to_string(),
            },
        ),
        (
            "PathText",
            Modal::PathText {
                title: "复制路径".to_string(),
                text: "/srv/wallpapers/a.png".to_string(),
            },
        ),
    ];
    for (name, modal) in variants {
        let mut app = app_with(MockScenario::default());
        app.state.open_modal(modal);
        assert!(app.state.modal.is_some(), "{name} 应已打开");
        press(&mut app, KeyCode::Esc);
        assert!(
            app.state.modal.is_none(),
            "{name}：Esc 后 state.modal 必须为 None（不得把用户困在弹窗里）"
        );
        // 再确认一次：Esc 不会重新打开任何模态
        press(&mut app, KeyCode::Esc);
        assert!(app.state.modal.is_none(), "{name}：连按 Esc 仍应为 None");
    }
}

/// 确认框路径的**非屏幕判据**：直接读 `state.config_path` 与 `state.modal` 里的问句文本。
#[test]
fn confirm_question_carries_daemon_path_read_from_state() {
    let mut app = app_with(MockScenario {
        save_path: "/tmp/cfg-nolegacy.toml".to_string(),
        ..MockScenario::default()
    });
    // 首次保存前：还不知道路径（state 里必须是 None，而不是写死的规范路径）
    assert_eq!(app.state.config_path, None);

    app.on_action(Action::Save);
    assert_eq!(
        app.state.config_path.as_deref(),
        Some("/tmp/cfg-nolegacy.toml"),
        "保存后必须记下 daemon 回传的路径"
    );

    // 打开来源保存确认，直接读问句文本（不抓屏）
    app.state.sources_dirty = true;
    app.on_action(Action::Goto(Page::Library));
    app.on_action(Action::SourceManage);
    press(&mut app, KeyCode::Char('s'));
    match app.state.modal.clone() {
        Some(Modal::Confirm { question, .. }) => {
            assert!(
                question.contains("/tmp/cfg-nolegacy.toml"),
                "确认框问句必须包含 daemon 回传的路径：{question}"
            );
            assert!(
                !question.contains("/etc/liuguang/config.toml"),
                "确认框不得写死规范路径：{question}"
            );
        }
        other => panic!("期望保存确认弹窗，得到 {other:?}"),
    }
}
