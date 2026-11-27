//! t42（T39-F1）：媒体库默认筛选「全部」必须发 `filter: None`。
//!
//! 缺陷形态：TUI 恒发 `Some("all")`，daemon 对 `kind` 精确比较 ⇒ `ok=true` + `total=0`
//! ⇒ **媒体库默认看起来是空的**，分页/搜索在默认筛选下不可用。
//! 本测试断言的是**真实上线的请求参数**（用真客户端打进程内 server，抓原始 params）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_ipc::Dispatch;
use liuguang_ipc::protocol::{ErrorBody, ErrorKind, Method, Request, Response};
use liuguang_ipc::server::{IpcServer, ServerConfig};
use liuguang_tui::{Action, App, ControlClient, IpcControl, MediaFilter, MediaQuery, Page};
use std::sync::{Arc, Mutex};

/// 起一个假 daemon：记录收到的**原始 params**，并按"真实条数"回复。
fn recording_server(
    name: &str,
) -> (
    std::path::PathBuf,
    liuguang_ipc::ServerHandle,
    Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let mut sock = std::env::temp_dir();
    sock.push(format!(
        "liuguang-tui-t42-{name}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let server = IpcServer::bind(ServerConfig {
        socket_path: sock.clone(),
        ..ServerConfig::default()
    })
    .expect("bind");
    let handle = server.spawn(move |req: Request| {
        let params = req.params.clone();
        let reply = |r: Response| Dispatch::Reply(r);
        match req.method {
            Method::ListMedia => {
                sink.lock().expect("sink").push(params.clone());
                let raw = params.get("filter").and_then(|v| v.as_str());
                let items: Vec<serde_json::Value> = (0..76)
                    .map(|i| {
                        serde_json::json!({
                            "id": format!("/media/images/i{i}.png"),
                            "source_id": "s1",
                            "name": format!("i{i}.png"),
                            "path": format!("/media/images/i{i}.png"),
                            "kind": "image",
                            "size_bytes": 4,
                            "modified_unix": 0,
                        })
                    })
                    .collect();
                match liuguang_ipc::protocol::parse_media_filter(raw) {
                    Ok(_) => reply(Response::ok(
                        req.id,
                        serde_json::json!({
                            "items": items, "total": 76, "page": 0, "page_size": 50,
                            "truncated": false, "scanning": false
                        }),
                    )),
                    Err(msg) => {
                        let mut body = ErrorBody::new(ErrorKind::InvalidParams, msg);
                        body.detail_code = Some("INVALID_FILTER".to_string());
                        reply(Response::err(req.id, body))
                    }
                }
            }
            Method::Status => reply(Response::ok(
                req.id,
                serde_json::json!({"state":"StaticIdle","paused":false,"display_backend":"drm",
                    "decoder":"static","uptime_seconds":1}),
            )),
            Method::Outputs => reply(Response::ok(req.id, serde_json::json!({"outputs": []}))),
            Method::ListSources => reply(Response::ok(
                req.id,
                serde_json::json!({"root": "/media", "sources": [], "missing": 0}),
            )),
            Method::ListPlaylists => {
                reply(Response::ok(req.id, serde_json::json!({"playlists": []})))
            }
            Method::GetMetrics => reply(Response::ok(req.id, serde_json::json!({}))),
            _ => reply(Response::err(
                req.id,
                ErrorBody::new(ErrorKind::NotImplemented, "x"),
            )),
        }
    });
    (sock, handle, seen)
}

/// **核心用例**：默认筛选发出的请求里**没有** `filter`（等价于 `filter: None`）。
#[test]
fn default_filter_sends_none_on_the_wire() {
    let (sock, handle, seen) = recording_server("default");
    let mut ctl = IpcControl::connect(&sock);
    let res = ctl.list_media(&MediaQuery::default()).expect("list_media");
    assert_eq!(res.total, 76, "默认筛选必须拿到真实条数");

    let seen = seen.lock().expect("sink").clone();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0];
    assert!(
        sent.get("filter").is_none(),
        "默认「全部」必须**不发** filter（收到：{sent}）——发 \"all\" 会让 daemon 精确比较 ⇒ 恒 0 项"
    );
    // 模型层的语义也一致
    assert_eq!(MediaFilter::All.wire(), None);
    assert_eq!(MediaFilter::Image.wire(), Some("image"));
    handle.shutdown();
}

/// 非默认筛选仍按词汇表下发（不因修 bug 而"全都不筛"）。
#[test]
fn explicit_filters_are_sent_as_kind_names() {
    let (sock, handle, seen) = recording_server("explicit");
    let mut ctl = IpcControl::connect(&sock);
    for (filter, want) in [(MediaFilter::Image, "image"), (MediaFilter::Video, "video")] {
        let q = MediaQuery {
            filter,
            ..MediaQuery::default()
        };
        ctl.list_media(&q).expect("list_media");
        let seen = seen.lock().expect("sink").clone();
        let last = seen.last().expect("已记录");
        assert_eq!(
            last.get("filter").and_then(|v| v.as_str()),
            Some(want),
            "筛选 {filter:?} 应下发 {want:?}：{last}"
        );
    }
    handle.shutdown();
}

/// 未知值必须**报错**（而不是静默 0 项）——纵深防御在真实客户端路径上也成立。
#[test]
fn unknown_filter_is_rejected_not_silently_empty() {
    let (sock, handle, _seen) = recording_server("bogus");
    let mut ctl = IpcControl::connect(&sock);
    let q = MediaQuery {
        filter: MediaFilter::All,
        ..MediaQuery::default()
    };
    let _ = (&mut ctl, &q);
    // 手工塞入非法 filter（模拟旧客户端/手写请求）：必须被拒绝，而不是静默 0 项
    let mut raw = liuguang_ipc::IpcClient::connect(&sock).expect("connect");
    let resp = raw
        .request(
            Method::ListMedia,
            serde_json::json!({"page": 0, "page_size": 10, "filter": "bogus"}),
        )
        .expect("transport ok");
    assert!(!resp.ok, "未知 filter 必须 ok=false（不得静默返回空结果）");
    let body = resp.error.expect("应有 error 体");
    assert_eq!(body.code, ErrorKind::InvalidParams);
    assert_eq!(body.detail_code.as_deref(), Some("INVALID_FILTER"));
    handle.shutdown();
}

/// 屏幕层：默认进入媒体库页应显示**真实条数**（不是"共 0 项"），分页提示可用。
#[test]
fn library_page_shows_real_count_by_default() {
    let (sock, handle, _seen) = recording_server("screen");
    let mut app = App::new(IpcControl::connect(&sock));
    app.refresh_all();
    app.on_action(Action::Goto(Page::Library));

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).expect("term");
    term.draw(|f| liuguang_tui::ui::draw(f, &app.state))
        .expect("draw");
    let buf = term.backend().buffer().clone();
    let mut shown = String::new();
    for y in 0..buf.area.height {
        let mut skip = 0usize;
        for x in 0..buf.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = buf[(x, y)].symbol();
            shown.push_str(sym);
            let w = unicode_width::UnicodeWidthStr::width(sym);
            if w > 1 {
                skip = w - 1;
            }
        }
        shown.push('\n');
    }
    assert_eq!(app.state.media.total, 76, "默认筛选下 total 必须是真实条数");
    assert!(
        shown.contains("共 76 项"),
        "媒体库页必须显示真实条数（而非「共 0 项」）：\n{shown}"
    );
    assert!(
        !shown.contains("共 0 项"),
        "默认筛选下不得再显示 0 项：\n{shown}"
    );
    assert!(
        shown.contains("PgUp/PgDn"),
        "有 2 页时默认筛选下就应出现分页提示：\n{shown}"
    );
    handle.shutdown();
}
