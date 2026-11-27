//! 后台线程：一个持有 `IpcClient` 的工作线程 + 一个缩略图解码线程。
//!
//! GUI 线程只做两件事：发 [`Cmd`]、消费 [`Upd`]。所有可能阻塞的操作
//! （连 socket、等响应、解码 PNG、读写配置）都不在 GTK 主线程上跑——
//! `list_media` 在万条索引下实测 ~100ms（协议文档 T25-F3），主线程卡一下
//! 就是整窗掉帧。
//!
//! 协议类型直接复用 `liuguang-ipc`：GUI 与 daemon 用同一套结构体，
//! 字段增删只会在这里的解析处报错，而不是散落在各个页面的 `json.get()` 里。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use liuguang_core::config::Config;
use liuguang_ipc::protocol::{
    AddSourceResult, CompatInfo, DeleteMediaResult, DeletePlaylistResult, ErrorKind, EventMessage,
    GetMediaResult, GetThumbnailResult, ListMediaResult, ListPlaylistsResult, ListSourcesResult,
    MediaItem, MetricsResult, OutputStatus, OutputsResult, Playlist, PrepareMediaResult,
    ScanSourceResult, StatusResult,
};
use liuguang_ipc::{IpcClient, Method, Request, Response};
use serde_json::json;

/// 播放列表编辑载荷：GUI 侧的可变字段，交给工作线程转成协议请求。
#[derive(Debug, Clone, Default)]
pub struct PlaylistEdit {
    /// `None` = 新建。
    pub playlist_id: Option<String>,
    pub name: String,
    /// 条目（媒体 id，即规范化后的绝对路径）。
    pub entries: Vec<String>,
    pub mode: String,
    pub interval_seconds: u32,
    pub on_video_end: String,
    pub output: Option<String>,
    pub no_repeat: bool,
}

/// 一次"应用壁纸"的完整请求。
///
/// 单独成 struct 是为了在 `NEEDS_PREPARATION` 时能原样保存、转码完成后
/// 自动重试（参数不能丢），也让 ipc 层能在重试时复用同一套校验。
#[derive(Debug, Clone)]
pub struct ApplyRequest {
    pub path: String,
    pub output: Option<String>,
    pub fit: String,
    pub zoom: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    /// 0 / 90 / 180 / 270。
    pub rotation: u32,
    /// 实时亮度倍率（0.1..=1.5）。
    pub brightness: f64,
    pub pin: bool,
}

/// 实时无闪烁输出变换请求（`set_output_transform`）。
#[derive(Debug, Clone)]
pub struct TransformRequest {
    pub output: Option<String>,
    pub fit: String,
    pub zoom: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    pub rotation: u32,
    pub brightness: f64,
}

/// GUI → 工作线程。
#[derive(Debug, Clone)]
pub enum Cmd {
    /// 重新读一次状态（1s 定时器驱动）。
    Status,
    /// 读媒体列表（只读索引，不碰磁盘）。
    Media,
    /// 让守护进程**重新扫一遍磁盘**，然后刷新列表。
    ///
    /// 关键区别：`list_media` 只在索引为空时才自动起扫描（首屏语义），
    /// 所以"往目录里丢进新文件"之后只发 `Media` 是看不到的——刷新按钮走这里。
    Rescan,
    /// 输出列表（多显示器选择用）。
    Outputs,
    /// CPU / 内存 / 实时 fps。
    Metrics,
    /// 媒体来源（设置页）。
    Sources,
    /// 播放列表（播放列表页）。
    Playlists,
    /// 读配置文件（设置页要显示/回写）。
    Config,
    /// 应用壁纸（带适配参数与旋转）。
    Apply(Box<ApplyRequest>),
    /// 实时无闪烁调整输出变换与亮度。
    SetOutputTransform(Box<TransformRequest>),
    /// 查一个媒体的兼容性（`get_media`，库里逐项补齐角标）。
    MediaCompat {
        id: String,
    },
    /// 准备（后台转码）一个需要转码的素材；`force = false` 时是幂等查询。
    Prepare {
        path: String,
        force: bool,
    },
    /// 只切换"只放这一项"，不动当前媒体（不重开解码管线）。
    ///
    /// `output` 为 `None` 时作用于默认输出；daemon 支持按输出锁定。
    SetPinned {
        pinned: bool,
        output: Option<String>,
    },
    Pause,
    Resume,
    /// 切到下一项（指定输出时只切那一路）。
    Next {
        output: Option<String>,
    },
    /// 让守护进程重读配置文件。
    Reload,
    /// 要一帧画面（**由守护进程解码**：它才有解码器与媒体根边界校验）。
    ///
    /// 尺寸由调用方按"卡片逻辑宽度 × 显示缩放"算好（HiDPI 下取 1.5×，
    /// 否则 400×250 会被放大到 ~483 物理像素、肉眼可见发糊）。
    Thumbnail {
        id: String,
        at_ms: u64,
        max_width: u32,
        max_height: u32,
    },
    /// 设置转场效果。
    Transition {
        effect: String,
        duration_ms: u32,
    },
    /// 添加媒体来源（`kind` = image / video）。
    AddSource {
        kind: String,
        path: String,
        recursive: bool,
    },
    RemoveSource {
        source_id: String,
    },
    /// 重扫某个来源（`None` = 全部）。
    ScanSource {
        source_id: Option<String>,
    },
    /// 从**索引**移除媒体（不删磁盘文件）。
    DeleteMedia {
        path: String,
    },
    UpdatePlaylist(Box<PlaylistEdit>),
    DeletePlaylist {
        playlist_id: String,
    },
    /// 把运行期状态写回配置文件。
    SaveConfig,
}

/// 工作线程 → GUI。
#[derive(Debug)]
pub enum Upd {
    Status(Box<StatusResult>),
    Media {
        items: Vec<MediaItem>,
        total: usize,
        scanning: bool,
    },
    Metrics(Box<MetricsResult>),
    Sources(Box<ListSourcesResult>),
    Playlists(Vec<Playlist>),
    Config(Box<Config>),
    Outputs(Vec<OutputStatus>),
    Applied(String),
    TransformApplied,
    Paused(bool),
    Pinned(bool),
    Transition(String),
    /// 播放列表已保存（daemon 回显）。
    PlaylistSaved(Box<Playlist>),
    /// 播放列表已删除（回显 id）。
    PlaylistDeleted(String),
    /// 媒体已从索引移除。
    MediaRemoved {
        path: String,
        removed: bool,
        removed_count: usize,
    },
    /// 某来源已受理扫描。
    ScanStarted {
        scan_id: u64,
        superseded: bool,
    },
    /// 某媒体的兼容性判定（`get_media` 回包）。
    Compat {
        id: String,
        compat: Option<CompatInfo>,
    },
    /// 转码准备状态（`prepare_media` 回包，幂等查询也走这里）。
    Prepared {
        path: String,
        result: PrepareMediaResult,
    },
    /// 应用时后端要求先转码：保留完整请求，GUI 转成"加载中"并自动准备。
    NeedsPreparation {
        request: Box<ApplyRequest>,
        message: String,
    },
    /// 订阅到的事件（`SUBSCRIBE` 流）。
    Event(Box<EventMessage>),
    /// 一句人话（成功类），GUI 以 toast 显示。
    Notice(String),
    Reloaded,
    /// 守护进程抽好的一帧（PNG，base64）。
    Thumbnail {
        id: String,
        at_ms: u64,
        png_b64: String,
    },
    /// 一句人话（失败），GUI 以红色 toast + 状态栏显示。
    Error(String),
    /// **带结构化错误码的失败**（`error.code` / `error.detail_code` 原样保留）。
    ///
    /// `Upd::Error` 只剩 message，调用方永远无法可靠地区分「后端没有转码能力」
    /// （可 SKIP）和「这条素材真的转不动」（必须 FAIL）——两者文案会变，错误码是契约。
    /// 目前只在需要决策的地方（`prepare_media`）使用，其余命令仍走 `Upd::Error`。
    Failure {
        code: String,
        detail_code: Option<String>,
        message: String,
    },
}

impl Cmd {
    /// 是否算"用户交互命令"，必须走独立连接。
    ///
    /// 缩略图会让普通连接上的响应排到几秒之后（daemon 逐条处理一个连接上的
    /// 请求），而"应用/暂停/继续/下一个/锁定/重载"是用户点下去就要生效的动作。
    /// 给它们留一条专用连接，封面再慢也不会把点击闷在队列里。
    #[must_use]
    pub fn is_urgent(&self) -> bool {
        matches!(
            self,
            Cmd::Apply(_)
                | Cmd::SetOutputTransform(_)
                | Cmd::Pause
                | Cmd::Resume
                | Cmd::Next { .. }
                | Cmd::SetPinned { .. }
                | Cmd::Reload
                | Cmd::Transition { .. }
        )
    }
}

/// 拉起三个线程，返回（普通命令、交互命令、更新）三个通道。
///
/// 交互命令走第二条到 daemon 的连接：daemon 侧对每条连接独立处理，封面解码
/// 在普通连接上排队时，点击「应用」仍然能立刻被读到（t40 验收：≤600ms）。
///
/// # Panics
///
/// 线程创建失败时 panic（GUI 起步阶段无法降级）。
pub fn spawn(
    socket: PathBuf,
    config_path: PathBuf,
) -> (Sender<Cmd>, Sender<Cmd>, async_channel::Receiver<Upd>) {
    let (tx_cmd, rx_cmd) = channel::<Cmd>();
    let (tx_urgent, rx_urgent) = channel::<Cmd>();
    let (tx_upd, rx_upd) = async_channel::unbounded::<Upd>();

    let ipc_tx = tx_upd.clone();
    let ipc_socket = socket.clone();
    let ipc_config = config_path.clone();
    thread::Builder::new()
        .name("liuguang-ipc".into())
        .spawn(move || ipc_loop(&ipc_socket, &ipc_config, rx_cmd, ipc_tx))
        .expect("启动 IPC 线程");

    // 交互连接：只跑 `Cmd::is_urgent()` 的命令，永远不和缩略图排队。
    let urgent_tx = tx_upd.clone();
    let urgent_socket = socket.clone();
    let urgent_config = config_path.clone();
    thread::Builder::new()
        .name("liuguang-urgent".into())
        .spawn(move || ipc_loop(&urgent_socket, &urgent_config, rx_urgent, urgent_tx))
        .expect("启动交互 IPC 线程");

    // 事件流是第二条长连接：`SUBSCRIBE` 先回确认帧，之后同连接持续推事件。
    // 独立线程 + 自动重连，daemon 重启不会让 GUI 丢事件订阅。
    let events_tx = tx_upd.clone();
    let events_socket = socket.clone();
    thread::Builder::new()
        .name("liuguang-events".into())
        .spawn(move || events_loop(&events_socket, events_tx))
        .expect("启动事件订阅线程");

    (tx_cmd, tx_urgent, rx_upd)
}

/// 事件订阅循环：断线（daemon 重启）后退避 1s 重连。
fn events_loop(socket: &Path, tx: async_channel::Sender<Upd>) {
    loop {
        if let Err(error) = subscribe_once(socket, &tx) {
            // 订阅偶尔失败不值得红色 toast；它不影响命令通道，
            // 而且 daemon 重启后 1s 内就会自动恢复。
            let _ = tx.send_blocking(Upd::Notice(format!("事件订阅断开，正在重连：{error}")));
        }
        thread::sleep(Duration::from_millis(1000));
    }
}

/// 一次订阅会话：连接 → 写 `SUBSCRIBE` → 先读确认帧 → 逐行读事件。
fn subscribe_once(socket: &Path, tx: &async_channel::Sender<Upd>) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};

    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| format!("连不上 {}：{e}", socket.display()))?;
    let request = Request::new("gui-events", Method::Subscribe, json!({}));
    let line = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    stream.write_all(&line).map_err(|e| e.to_string())?;
    stream.write_all(b"\n").map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        // 确认帧是 Response（没有 kind 字段）；其余都是 EventMessage。
        if value.get("kind").is_none() {
            let response: Response = serde_json::from_value(value).map_err(|e| e.to_string())?;
            if !response.ok {
                return Err(error_text(&response));
            }
            continue;
        }
        let event: EventMessage = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let _ = tx.send_blocking(Upd::Event(Box::new(event)));
    }
    Ok(())
}

#[derive(Debug)]
enum IpcErr {
    /// 传输层错误（连接断开/超时/协议帧损坏）：连接不可信，需要重建。
    Transport(String),
    /// 业务失败（服务端明确返回 ok=false，或本地配置错误）：socket 仍健康，无需重连。
    Business(String),
}

fn ipc_loop(socket: &Path, config_path: &Path, rx: Receiver<Cmd>, tx: async_channel::Sender<Upd>) {
    let mut client: Option<IpcClient> = None;
    while let Ok(cmd) = rx.recv() {
        // 每次命令前确保连接可用；断了就重连（守护进程重启是常态：
        // 用户改完配置执行 `systemctl --user restart liuguang`）。
        if client.is_none() {
            match IpcClient::connect(socket) {
                Ok(c) => client = Some(c),
                Err(e) => {
                    let _ = tx.send_blocking(Upd::Error(format!(
                        "连不上 liuguang（{}）：{e}",
                        socket.display()
                    )));
                    // 状态查询失败不打印两遍；其余命令失败要立刻可见。
                    continue;
                }
            }
        }
        let Some(c) = client.as_mut() else { continue };
        let result = handle(c, config_path, cmd);
        match result {
            Ok(upd) => {
                let _ = tx.send_blocking(upd);
            }
            Err(IpcErr::Business(message)) => {
                let _ = tx.send_blocking(Upd::Error(message));
            }
            Err(IpcErr::Transport(message)) => {
                // 传输层错误 ⇒ 连接已不可信，丢掉等下次重连。
                client = None;
                let _ = tx.send_blocking(Upd::Error(message));
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// 把 `Response` 压成 `Result<Value, IpcErr>`。
///
/// 协议层刻意把"业务失败"放在 `ok=false` 里而不是传输错误：GUI 要的是那句
/// `message`（例如 `PATH_OUTSIDE_ROOT` 的中文说明），不是 backtrace。
fn into_result(resp: liuguang_ipc::Response) -> Result<serde_json::Value, IpcErr> {
    if resp.ok {
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    } else {
        Err(IpcErr::Business(error_text(&resp)))
    }
}

/// 响应里的业务错误码 + 文案（成功时为 `None`）。
fn error_parts(resp: &Response) -> Option<(ErrorKind, String)> {
    resp.error
        .as_ref()
        .map(|error| (error.code, error.message.clone()))
}

/// 响应失败时的人话错误（没有 error 体时也给出可读兜底）。
fn error_text(resp: &Response) -> String {
    error_parts(resp).map_or_else(
        || "守护进程返回失败但没有说明".to_owned(),
        |(_code, message)| message,
    )
}

/// 把失败响应转成**保留错误码**的 `Upd::Failure`。
///
/// 只给需要按错误码做决策的调用点用（`prepare_media`）：`message` 是给人看的、
/// 会随版本变化，`error.code` / `error.detail_code` 才是稳定契约。
fn failure(resp: &Response) -> Upd {
    resp.error.as_ref().map_or_else(
        || Upd::Failure {
            code: "UNKNOWN".to_owned(),
            detail_code: None,
            message: "守护进程返回失败但没有说明".to_owned(),
        },
        |error| Upd::Failure {
            code: error.code.as_str().to_owned(),
            detail_code: error.detail_code.clone(),
            message: error.message.clone(),
        },
    )
}

/// 把响应解析成协议类型（解析失败或业务错误均归入 `Business`，不重置 socket）。
fn parse<T: serde::de::DeserializeOwned>(
    resp: liuguang_ipc::Response,
    what: &str,
) -> Result<T, IpcErr> {
    let value = into_result(resp)?;
    serde_json::from_value(value).map_err(|e| IpcErr::Business(format!("解析{what}失败：{e}")))
}

/// 分页取全媒体列表。
fn fetch_media(client: &mut IpcClient) -> Result<(Vec<MediaItem>, usize, bool), IpcErr> {
    const PAGE_SIZE: u32 = 200;
    const MAX_PAGES: u32 = 20;
    let mut rows: Vec<MediaItem> = Vec::new();
    let mut total = 0usize;
    let mut scanning = false;
    for page in 0..MAX_PAGES {
        let resp = client
            .request(
                Method::ListMedia,
                json!({"page": page, "page_size": PAGE_SIZE}),
            )
            .map_err(|e| IpcErr::Transport(e.to_string()))?;
        let value = into_result(resp)?;
        // 整页解析失败就退化成逐条解析：不能因为一条坏记录把整面墙变空。
        let parsed: ListMediaResult = serde_json::from_value(value.clone())
            .or_else(|_| {
                serde_json::from_value::<ListMediaResult>(json!({
                    "items": value.get("items").cloned().unwrap_or_default(),
                    "total": value.get("total").cloned().unwrap_or_default(),
                    "page": page,
                    "page_size": PAGE_SIZE,
                    "scanning": value.get("scanning").cloned().unwrap_or_default(),
                }))
            })
            .map_err(|e| IpcErr::Business(format!("解析媒体列表失败：{e}")))?;
        total = parsed.total;
        scanning = parsed.scanning;
        let count = parsed.items.len();
        rows.extend(parsed.items);
        if rows.len() >= total || u32::try_from(count).unwrap_or(0) < PAGE_SIZE {
            break;
        }
    }
    Ok((rows, total, scanning))
}

fn handle(client: &mut IpcClient, config_path: &Path, cmd: Cmd) -> Result<Upd, IpcErr> {
    match cmd {
        Cmd::Status => {
            let resp = client
                .request(Method::Status, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let status: StatusResult = parse(resp, "状态")?;
            Ok(Upd::Status(Box::new(status)))
        }
        Cmd::Rescan => {
            let resp = client
                .request(Method::RescanLibrary, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let value = into_result(resp)?;
            let scan_id = value.get("scan_id").and_then(|v| v.as_u64()).unwrap_or(0);
            let (items, total, scanning) = fetch_media(client)?;
            let _ = scan_id;
            Ok(Upd::Media {
                items,
                total,
                scanning: scanning || scan_id > 0,
            })
        }
        Cmd::Media => {
            let (items, total, scanning) = fetch_media(client)?;
            Ok(Upd::Media {
                items,
                total,
                scanning,
            })
        }
        Cmd::Outputs => {
            let resp = client
                .request(Method::Outputs, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let result: OutputsResult = parse(resp, "输出列表")?;
            Ok(Upd::Outputs(result.outputs))
        }
        Cmd::Metrics => {
            let resp = client
                .request(Method::GetMetrics, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let metrics: MetricsResult = parse(resp, "指标")?;
            Ok(Upd::Metrics(Box::new(metrics)))
        }
        Cmd::Sources => {
            let resp = client
                .request(Method::ListSources, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let sources: ListSourcesResult = parse(resp, "媒体来源")?;
            Ok(Upd::Sources(Box::new(sources)))
        }
        Cmd::Playlists => {
            let resp = client
                .request(Method::ListPlaylists, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let list: ListPlaylistsResult = parse(resp, "播放列表")?;
            Ok(Upd::Playlists(list.playlists))
        }
        Cmd::Config => match Config::load(config_path) {
            Ok(cfg) => Ok(Upd::Config(Box::new(cfg))),
            Err(e) => Err(IpcErr::Business(format!(
                "读取配置失败（{}）：{e}",
                config_path.display()
            ))),
        },
        Cmd::Apply(request) => {
            let request = *request;
            // `set_wallpaper` 已直接接受 `rotation`，一次请求完成壁纸+适配+旋转，
            // 不再紧接着下发一次 `set_output_transform`（避免触发双管线重建）。
            let resp = client
                .request(
                    Method::SetWallpaper,
                    json!({
                        "path": request.path,
                        "output": request.output,
                        "fit": request.fit,
                        "zoom": request.zoom,
                        "offset_x": request.offset_x,
                        "offset_y": request.offset_y,
                        "rotation": request.rotation,
                        "brightness": request.brightness,
                        "pin": request.pin,
                    }),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            if !resp.ok {
                // t15：硬件解不动的素材会回 NEEDS_PREPARATION，而不是失败。
                // 这里**不能**把它当错误丢给用户：保留完整请求，让 GUI 转成
                // "加载中 + 自动准备"，转码完成后用同一组参数重试。
                if let Some((code, message)) = error_parts(&resp) {
                    if code == ErrorKind::NeedsPreparation {
                        return Ok(Upd::NeedsPreparation {
                            request: Box::new(request),
                            message,
                        });
                    }
                }
                return Err(IpcErr::Business(error_text(&resp)));
            }
            Ok(Upd::Applied(request.path))
        }
        Cmd::MediaCompat { id } => {
            let resp = client
                .request(Method::GetMedia, json!({ "id": id }))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let result: GetMediaResult = parse(resp, "媒体详情")?;
            Ok(Upd::Compat {
                id,
                compat: result.compat,
            })
        }
        Cmd::Prepare { path, force } => {
            let resp = client
                .request(
                    Method::PrepareMedia,
                    json!({ "path": path, "force": force }),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            // 失败必须保留错误码：后端「没有转码能力」与「这条素材转不动」是两种结论
            // （自检前者 SKIP、后者 FAIL），不能都压成一句 message。
            if !resp.ok {
                return Ok(failure(&resp));
            }
            let result: PrepareMediaResult = parse(resp, "转码状态")?;
            Ok(Upd::Prepared { path, result })
        }
        Cmd::SetPinned { pinned, output } => {
            let resp = client
                .request(
                    Method::SetPinned,
                    json!({ "pinned": pinned, "output": output }),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::Pinned(pinned))
        }
        Cmd::Pause => {
            let resp = client
                .request(Method::Pause, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::Paused(true))
        }
        Cmd::Resume => {
            let resp = client
                .request(Method::Resume, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::Paused(false))
        }
        Cmd::Next { output } => {
            let mut params = serde_json::Map::new();
            if let Some(output) = output {
                params.insert("output".into(), json!(output));
            }
            let resp = client
                .request(Method::Next, serde_json::Value::Object(params))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::Reloaded) // 只用来触发一次状态刷新
        }
        Cmd::Reload => {
            let resp = client
                .request(Method::Reload, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::Reloaded)
        }
        Cmd::Thumbnail {
            id,
            at_ms,
            max_width,
            max_height,
        } => {
            let resp = client
                .request(
                    Method::GetThumbnail,
                    json!({"id": id, "max_width": max_width, "max_height": max_height, "at_ms": at_ms}),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let thumb: GetThumbnailResult = parse(resp, "缩略图")?;
            Ok(Upd::Thumbnail {
                id,
                at_ms,
                png_b64: thumb.data_b64,
            })
        }
        Cmd::Transition {
            effect,
            duration_ms,
        } => {
            let resp = client
                .request(
                    Method::Transition,
                    json!({"effect": effect, "duration_ms": duration_ms}),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::Transition(effect))
        }
        Cmd::AddSource {
            kind,
            path,
            recursive,
        } => {
            let resp = client
                .request(
                    Method::AddSource,
                    json!({"kind": kind, "path": path, "recursive": recursive}),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let result: AddSourceResult = parse(resp, "媒体来源")?;
            let save_resp = client
                .request(Method::SaveConfig, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(save_resp)?;
            Ok(Upd::Notice(if result.created {
                format!("已添加并保存来源 {}", result.source_id)
            } else {
                format!("来源已存在：{}", result.source_id)
            }))
        }
        Cmd::RemoveSource { source_id } => {
            let resp = client
                .request(Method::RemoveSource, json!({"source_id": source_id}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            let save_resp = client
                .request(Method::SaveConfig, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(save_resp)?;
            Ok(Upd::Notice(format!("已移除并保存来源 {source_id}")))
        }
        Cmd::ScanSource { source_id } => {
            let resp = client
                .request(
                    Method::ScanSource,
                    json!({"source_id": source_id, "full": false}),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let result: ScanSourceResult = parse(resp, "扫描结果")?;
            Ok(Upd::ScanStarted {
                scan_id: result.scan_id,
                superseded: result.superseded_running,
            })
        }
        Cmd::DeleteMedia { path } => {
            let resp = client
                .request(Method::DeleteMedia, json!({"path": path}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let result: DeleteMediaResult = parse(resp, "删除结果")?;
            Ok(Upd::MediaRemoved {
                path,
                removed: result.removed,
                removed_count: result.removed_count,
            })
        }
        Cmd::UpdatePlaylist(edit) => {
            let params = json!({
                "playlist_id": edit.playlist_id,
                "name": edit.name,
                "entries": edit.entries.iter().map(|id| json!({"media_id": id})).collect::<Vec<_>>(),
                "mode": edit.mode,
                "interval_seconds": edit.interval_seconds,
                "on_video_end": edit.on_video_end,
                "output": edit.output,
                "no_repeat": edit.no_repeat,
            });
            let resp = client
                .request(Method::UpdatePlaylist, params)
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let playlist: Playlist = parse(resp, "播放列表")?;
            Ok(Upd::PlaylistSaved(Box::new(playlist)))
        }
        Cmd::DeletePlaylist { playlist_id } => {
            let resp = client
                .request(Method::DeletePlaylist, json!({"playlist_id": playlist_id}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            let result: DeletePlaylistResult = parse(resp, "删除结果")?;
            Ok(Upd::PlaylistDeleted(result.playlist_id))
        }
        Cmd::SaveConfig => {
            let resp = client
                .request(Method::SaveConfig, json!({}))
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::Notice("配置已保存".to_owned()))
        }
        Cmd::SetOutputTransform(req) => {
            let req = *req;
            let resp = client
                .request(
                    Method::SetOutputTransform,
                    json!({
                        "output": req.output,
                        "fit": req.fit,
                        "zoom": req.zoom,
                        "offset_x": req.offset_x,
                        "offset_y": req.offset_y,
                        "rotation": req.rotation,
                        "brightness": req.brightness,
                    }),
                )
                .map_err(|e| IpcErr::Transport(e.to_string()))?;
            into_result(resp)?;
            Ok(Upd::TransformApplied)
        }
    }
}
