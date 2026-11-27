//! 控制客户端抽象：TUI 与 liuguang 之间**唯一**的数据入口。
//!
//! - [`IpcControl`]：真实实现（`liuguang_ipc::IpcClient`，全程不碰 `/dev/dri`）；
//! - [`MockControl`]：纯内存实现，**不依赖 daemon** 即可跑通全部页面与交互；
//! - [`EventSource`]：`subscribe` 事件流（`WallpaperApplied`/`Scan*`/`OutputChanged`/`Error`）。
//!
//! **安全边界（规范 §8）**：媒体目录/路径的规范化与白名单校验必须由 daemon 执行。
//! 因此 TUI **不做任何 `std::fs` 扫描**，也**不把客户端路径当服务端路径**：
//! 用户输入的字符串原样交给 daemon，daemon 返回的 `detail_code`（`PATH_OUTSIDE_ROOT`
//! 等）才是权威判定。

use liuguang_ipc::protocol::{
    AddSourceParams, CompatInfo, ErrorKind, EventKind, EventMessage, GetMediaParams,
    GetMediaResult, ListMediaParams, ListMediaResult, ListSourcesResult, MediaItem, MediaSource,
    Method, MetricsResult, OutputsResult, Playlist, RemoveSourceParams, Request, Response,
    ScanSourceParams, ScanSourceResult, SetOutputTransformParams, SetWallpaperParams, StatusResult,
    TransitionParams, UpdatePlaylistParams,
};
use liuguang_ipc::{NdjsonDecoder, decode_message, encode_message};
use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::model::{MediaFilter, MediaSort};

/// `list_media` 的查询参数（**全部下推给 daemon**，TUI 不做本地过滤）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaQuery {
    /// 0 基页码。
    pub page: usize,
    /// 页大小（服务端上限 `MAX_MEDIA_PAGE_SIZE`）。
    pub page_size: usize,
    /// 图片/视频筛选。
    pub filter: MediaFilter,
    /// 排序。
    pub sort: MediaSort,
    /// 搜索词（服务端过滤）。
    pub search: String,
    /// 来源筛选。
    pub source_id: Option<String>,
}

impl Default for MediaQuery {
    fn default() -> Self {
        Self {
            page: 0,
            page_size: crate::model::MEDIA_PAGE_SIZE,
            filter: MediaFilter::All,
            sort: MediaSort::Name,
            search: String::new(),
            source_id: None,
        }
    }
}

/// `set_wallpaper` 的应答：**`accepted=true` 只表示受理**，不是已上屏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetWallpaperAck {
    /// 是否受理。
    pub accepted: bool,
    /// 对账用的请求 ID（`WALLPAPER_APPLIED` 事件里回传同一个值）。
    pub request_id: Option<String>,
}

/// 订阅事件流的读取端（非阻塞）。
pub trait EventSource {
    /// 取出当前可用的事件（没有则返回空 `Vec`）。
    ///
    /// # Errors
    ///
    /// 连接断开/协议错误时返回错误，由调用方置为 `Offline`。
    fn try_next(&mut self) -> Result<Vec<EventMessage>, String>;
}

/// 静态壁纸调用失败的两类（t70）—— 区分它们是为了**不把“未实现”说成“失败”**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticCallError {
    /// daemon 明确回 `UNSUPPORTED_METHOD`：接口还没落地（t69）。
    ///
    /// 调用方必须给出**可操作**提示（升级 daemon / 等版本），不得静默失败。
    Unsupported(String),
    /// 其它失败（路径非法、IO、协议错误）。
    Failed(String),
}

/// TUI 需要的数据/命令接口。
pub trait ControlClient {
    /// 查询 daemon 与输出状态。
    fn status(&mut self) -> Result<StatusResult, String>;
    /// 枚举输出。
    fn outputs(&mut self) -> Result<OutputsResult, String>;
    /// 设置壁纸（**服务器绝对路径**）；返回受理结果与 `request_id`。
    fn set_wallpaper(&mut self, params: SetWallpaperParams) -> Result<SetWallpaperAck, String>;
    /// 切换到下一个媒体项（`output` 为空 = daemon 默认输出）。
    fn next_media(&mut self, output: Option<String>) -> Result<(), String>;
    /// 暂停播放。
    fn pause(&mut self, output: Option<String>) -> Result<(), String>;
    /// 恢复播放。
    fn resume(&mut self, output: Option<String>) -> Result<(), String>;
    /// 重载配置。
    fn reload(&mut self) -> Result<(), String>;
    /// 设置转场效果。
    fn set_transition(&mut self, params: TransitionParams) -> Result<(), String>;
    /// 列出媒体来源（daemon 侧规范化 + 根边界校验）。
    fn list_sources(&mut self) -> Result<ListSourcesResult, String>;
    /// 启动一次**有界后台扫描**（异步；结果经事件返回）。
    fn scan_source(
        &mut self,
        source_id: Option<String>,
        full: bool,
    ) -> Result<ScanSourceResult, String>;
    /// 分页列出媒体（服务端过滤/排序；**不得一次拉全量**）。
    fn list_media(&mut self, query: &MediaQuery) -> Result<ListMediaResult, String>;
    /// 按 id 取单条（仅选中项才取，规范 §8）。
    fn get_media(&mut self, id: &str) -> Result<MediaItem, String>;
    /// 按 id 取单条详情与本机兼容性结论（默认回退为无兼容性结论）。
    fn get_media_compat(&mut self, id: &str) -> Result<(MediaItem, Option<CompatInfo>), String> {
        self.get_media(id).map(|item| (item, None))
    }
    /// 追加来源（daemon 校验路径）。
    fn add_source(&mut self, kind: &str, path: &str, recursive: bool) -> Result<(), String>;
    /// 取对账计数（提交/master/逐通道）。
    fn metrics(&mut self) -> Result<MetricsResult, String>;
    /// **只改运行时**的输出变换（持久化走 [`ControlClient::save_config`]）。
    fn set_output_transform(&mut self, params: &SetOutputTransformParams) -> Result<(), String>;
    /// **持久保存**配置（原子写入）。
    /// **持久保存**配置；返回值里只有 `verified` 代表"改动真的进了配置"
    /// （`saved` 仅表示字节写出去了 —— T25-F1 的教训）。
    fn save_config(&mut self) -> Result<liuguang_ipc::protocol::SaveConfigResult, String>;
    /// 列出播放列表（t22 已提供）。
    fn list_playlists(&mut self) -> Result<Vec<liuguang_ipc::protocol::Playlist>, String>;
    /// 新增/更新播放列表（**只改运行时**；持久化走 [`ControlClient::save_config`]）。
    ///
    /// # Errors
    ///
    /// daemon 校验失败（`mode`/`on_video_end`/`interval_seconds`/条目越界）时返回可读错误。
    fn update_playlist(&mut self, params: UpdatePlaylistParams) -> Result<Playlist, String>;
    /// 删除来源（**只改运行时**；持久化走 [`ControlClient::save_config`]）。
    fn remove_source(&mut self, source_id: &str) -> Result<(), String>;
    /// **设为静态壁纸**（t70；daemon 侧由 t69 落地）。
    ///
    /// 成功时返回 daemon 的原始结果（字段由 daemon 定）；
    /// 未实现时返回 [`StaticCallError::Unsupported`]（**不是**静默失败）。
    ///
    /// # Errors
    ///
    /// 见 [`StaticCallError`]：`Unsupported` = 接口未落地，`Failed` = 其它。
    fn set_static_wallpaper(&mut self, path: &str) -> Result<serde_json::Value, StaticCallError>;
    /// **清除静态壁纸**（恢复播放列表，从暂停处继续）。
    ///
    /// # Errors
    ///
    /// 见 [`StaticCallError`]。
    fn clear_static_wallpaper(&mut self) -> Result<serde_json::Value, StaticCallError>;
    /// 订阅事件流。
    fn subscribe(&mut self) -> Result<Box<dyn EventSource>, String>;
}

/// **前向兼容的裸请求**（t70 的静态壁纸 shim）。
///
/// 为什么不用类型化调用：`liuguang_ipc::Method` 里还没有 `set_static_wallpaper` /
/// `clear_static_wallpaper`（`crates/liuguang-ipc` 不在 t70 的 inScope），而 daemon 侧
/// 由 t69 落地。这里按**同一 NDJSON 协议**发一条原始请求：
///
/// - daemon 已实现 ⇒ 正常返回 `result`；
/// - daemon 未实现 ⇒ 服务端回 `ERROR{code: UNSUPPORTED_METHOD}`（**可操作提示的来源**）；
/// - 连不上/超时 ⇒ `Failed`。
///
/// 响应仍用 [`liuguang_ipc::Response`] 解析（不自己造协议类型）。
///
/// TODO(t69)：`Method` 增加变体后改回类型化调用，删掉本函数。
fn raw_request(
    socket: &Path,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, StaticCallError> {
    use std::io::{Read, Write};
    let fail =
        |what: &str, e: &dyn std::fmt::Display| StaticCallError::Failed(format!("{what}：{e}"));
    let mut stream =
        UnixStream::connect(socket).map_err(|e| fail("IPC 连接失败（daemon 未运行？）", &e))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let request = serde_json::json!({
        "v": liuguang_ipc::PROTOCOL_VERSION,
        "id": format!("tui-{method}-1"),
        "method": method,
        "params": params,
    });
    let bytes = encode_message(&request).map_err(|e| fail("请求编码失败", &e))?;
    stream.write_all(&bytes).map_err(|e| fail("发送失败", &e))?;
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|e| fail("读取响应失败", &e))?;
        if n == 0 {
            break;
        }
        line.extend_from_slice(&chunk[..n]);
        if line.contains(&b'\n') {
            break;
        }
        if line.len() > liuguang_ipc::MAX_MESSAGE_BYTES {
            return Err(StaticCallError::Failed(
                "响应超过协议上限（MAX_MESSAGE_BYTES）".to_string(),
            ));
        }
    }
    let response: Response = decode_message(&line).map_err(|e| fail("响应解码失败", &e))?;
    if response.ok {
        return Ok(response.result.unwrap_or(serde_json::Value::Null));
    }
    match response.error {
        Some(e) if e.code == ErrorKind::UnsupportedMethod => {
            Err(StaticCallError::Unsupported(e.message))
        }
        Some(e) => Err(StaticCallError::Failed(format!(
            "{}: {}",
            e.code.as_str(),
            e.message
        ))),
        None => Err(StaticCallError::Failed(
            "daemon 返回失败但没有错误详情".to_string(),
        )),
    }
}

/// 把 [`ErrorKind`] 转为线上稳定错误码（SCREAMING_SNAKE_CASE）。
fn error_code_str(code: ErrorKind) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "INTERNAL".to_string())
}

/// `ok=false` 响应 → `错误码[/detail_code]: 说明`（detail_code 是 daemon 的细分原因）。
fn error_body_to_string(body: &liuguang_ipc::ErrorBody) -> String {
    let code = error_code_str(body.code);
    let head = match body.detail_code.as_deref() {
        Some(detail) if !detail.is_empty() => format!("{code}/{detail}: {}", body.message),
        _ => format!("{code}: {}", body.message),
    };
    // t37：daemon 会给出"仍被哪些配置容器覆盖"的**权威名字**（不自行推断容器）
    if body.conflicts.is_empty() {
        head
    } else {
        format!("{head}（仍被覆盖：{}）", body.conflicts.join("、"))
    }
}

fn error_response_to_string(error: Option<liuguang_ipc::ErrorBody>) -> String {
    match error {
        Some(body) => error_body_to_string(&body),
        None => "INTERNAL: ok=false 但响应缺少 error 字段".to_string(),
    }
}

/// 真实 IPC 控制客户端（Unix socket + NDJSON）。
pub struct IpcControl {
    client: Option<liuguang_ipc::IpcClient>,
    socket: std::path::PathBuf,
    last_error: Option<String>,
}

impl std::fmt::Debug for IpcControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcControl")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl IpcControl {
    /// 连接到 liuguang 控制 socket。
    ///
    /// **惰性连接**：daemon 没运行时也构造成功（TUI 进入 `Offline` 态并可 `e` 重连），
    /// 与规范"断线可恢复"一致。
    pub fn connect(socket: &Path) -> Self {
        let mut me = Self {
            client: None,
            socket: socket.to_path_buf(),
            last_error: None,
        };
        let _ = me.ensure_client();
        me
    }

    /// 确保有可用连接（失败时记录原因并返回错误）。
    fn ensure_client(&mut self) -> Result<&mut liuguang_ipc::IpcClient, String> {
        if self.client.is_none() {
            match liuguang_ipc::IpcClient::connect(&self.socket) {
                Ok(c) => {
                    self.client = Some(c);
                    self.last_error = None;
                }
                Err(e) => {
                    let msg = match e {
                        liuguang_ipc::ClientError::Connect { socket, message } => {
                            format!("无法连接 {socket}: {message}")
                        }
                        other => other.to_string(),
                    };
                    self.last_error = Some(msg.clone());
                    return Err(msg);
                }
            }
        }
        self.client
            .as_mut()
            .ok_or_else(|| "IPC 客户端不可用".to_string())
    }

    /// 最近一次连接错误。
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// socket 路径（重连用）。
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    fn request(
        &mut self,
        method: Method,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let client = self.ensure_client()?;
        let response = client.request(method, params);
        // 传输层失败 ⇒ 连接已不可用，丢弃以免后续请求复用坏连接
        if response.is_err() {
            self.client = None;
        }
        match response {
            Ok(Response {
                ok: true, result, ..
            }) => Ok(result.unwrap_or(serde_json::Value::Null)),
            Ok(resp) => Err(error_response_to_string(resp.error)),
            Err(e) => Err(format!("IPC: {e}")),
        }
    }
}

impl ControlClient for IpcControl {
    fn status(&mut self) -> Result<StatusResult, String> {
        let value = self.request(Method::Status, serde_json::json!({}))?;
        serde_json::from_value(value).map_err(|e| format!("INTERNAL: status 结果解析失败: {e}"))
    }

    fn outputs(&mut self) -> Result<OutputsResult, String> {
        let value = self.request(Method::Outputs, serde_json::json!({}))?;
        serde_json::from_value(value).map_err(|e| format!("INTERNAL: outputs 结果解析失败: {e}"))
    }

    fn set_wallpaper(&mut self, params: SetWallpaperParams) -> Result<SetWallpaperAck, String> {
        let value = self.request(
            Method::SetWallpaper,
            serde_json::to_value(params).map_err(|e| e.to_string())?,
        )?;
        let accepted: liuguang_ipc::AcceptedResult = serde_json::from_value(value)
            .map_err(|e| format!("INTERNAL: set_wallpaper 结果解析失败: {e}"))?;
        Ok(SetWallpaperAck {
            accepted: accepted.accepted,
            request_id: accepted.request_id,
        })
    }

    fn next_media(&mut self, output: Option<String>) -> Result<(), String> {
        self.request(Method::Next, serde_json::json!({ "output": output }))
            .map(|_| ())
    }

    fn pause(&mut self, output: Option<String>) -> Result<(), String> {
        self.request(Method::Pause, serde_json::json!({ "output": output }))
            .map(|_| ())
    }

    fn resume(&mut self, output: Option<String>) -> Result<(), String> {
        self.request(Method::Resume, serde_json::json!({ "output": output }))
            .map(|_| ())
    }

    fn reload(&mut self) -> Result<(), String> {
        self.request(Method::Reload, serde_json::json!({}))
            .map(|_| ())
    }

    fn set_transition(&mut self, params: TransitionParams) -> Result<(), String> {
        self.request(
            Method::Transition,
            serde_json::to_value(params).map_err(|e| e.to_string())?,
        )
        .map(|_| ())
    }

    fn list_sources(&mut self) -> Result<ListSourcesResult, String> {
        let value = self.request(Method::ListSources, serde_json::json!({}))?;
        serde_json::from_value(value)
            .map_err(|e| format!("INTERNAL: list_sources 结果解析失败: {e}"))
    }

    fn scan_source(
        &mut self,
        source_id: Option<String>,
        full: bool,
    ) -> Result<ScanSourceResult, String> {
        let params = ScanSourceParams { source_id, full };
        let value = self.request(
            Method::ScanSource,
            serde_json::to_value(params).map_err(|e| e.to_string())?,
        )?;
        serde_json::from_value(value)
            .map_err(|e| format!("INTERNAL: scan_source 结果解析失败: {e}"))
    }

    fn list_media(&mut self, query: &MediaQuery) -> Result<ListMediaResult, String> {
        let params = ListMediaParams {
            page: query.page,
            page_size: query.page_size,
            source_id: query.source_id.clone(),
            search: (!query.search.trim().is_empty()).then(|| query.search.clone()),
            // T39-F1：默认「全部」发 `None`（不筛选），不发明知无效的 "all"
            filter: query.filter.wire().map(str::to_string),
            sort: Some(query.sort.wire().to_string()),
        };
        let value = self.request(
            Method::ListMedia,
            serde_json::to_value(params).map_err(|e| e.to_string())?,
        )?;
        serde_json::from_value(value).map_err(|e| format!("INTERNAL: list_media 结果解析失败: {e}"))
    }

    fn get_media(&mut self, id: &str) -> Result<MediaItem, String> {
        self.get_media_compat(id).map(|(item, _)| item)
    }

    fn get_media_compat(&mut self, id: &str) -> Result<(MediaItem, Option<CompatInfo>), String> {
        let params = GetMediaParams { id: id.to_string() };
        let value = self.request(
            Method::GetMedia,
            serde_json::to_value(params).map_err(|e| e.to_string())?,
        )?;
        if value.get("item").is_some() {
            let res: GetMediaResult = serde_json::from_value(value)
                .map_err(|e| format!("INTERNAL: get_media 结果解析失败: {e}"))?;
            Ok((res.item, res.compat))
        } else {
            let item: MediaItem = serde_json::from_value(value)
                .map_err(|e| format!("INTERNAL: get_media 结果解析失败: {e}"))?;
            Ok((item, None))
        }
    }

    fn add_source(&mut self, kind: &str, path: &str, recursive: bool) -> Result<(), String> {
        let params = AddSourceParams {
            kind: kind.to_string(),
            path: path.to_string(),
            recursive,
        };
        self.request(
            Method::AddSource,
            serde_json::to_value(params).map_err(|e| e.to_string())?,
        )
        .map(|_| ())
    }

    fn metrics(&mut self) -> Result<MetricsResult, String> {
        let value = self.request(Method::GetMetrics, serde_json::json!({}))?;
        serde_json::from_value(value)
            .map_err(|e| format!("INTERNAL: get_metrics 结果解析失败: {e}"))
    }

    fn set_output_transform(&mut self, params: &SetOutputTransformParams) -> Result<(), String> {
        self.request(
            Method::SetOutputTransform,
            serde_json::to_value(params).map_err(|e| e.to_string())?,
        )
        .map(|_| ())
    }

    fn save_config(&mut self) -> Result<liuguang_ipc::protocol::SaveConfigResult, String> {
        let value = self.request(Method::SaveConfig, serde_json::json!({}))?;
        serde_json::from_value(value)
            .map_err(|e| format!("INTERNAL: save_config 结果解析失败: {e}"))
    }

    fn list_playlists(&mut self) -> Result<Vec<liuguang_ipc::protocol::Playlist>, String> {
        let value = self.request(Method::ListPlaylists, serde_json::json!({}))?;
        let result: liuguang_ipc::protocol::ListPlaylistsResult = serde_json::from_value(value)
            .map_err(|e| format!("INTERNAL: list_playlists 结果解析失败: {e}"))?;
        Ok(result.playlists)
    }

    fn update_playlist(&mut self, params: UpdatePlaylistParams) -> Result<Playlist, String> {
        let value = serde_json::to_value(&params).map_err(|e| e.to_string())?;
        let reply = self.request(Method::UpdatePlaylist, value)?;
        serde_json::from_value(reply)
            .map_err(|e| format!("INTERNAL: update_playlist 结果解析失败: {e}"))
    }

    fn remove_source(&mut self, source_id: &str) -> Result<(), String> {
        let params = RemoveSourceParams {
            source_id: source_id.to_string(),
        };
        let value = serde_json::to_value(params).map_err(|e| e.to_string())?;
        self.request(Method::RemoveSource, value).map(|_| ())
    }

    fn set_static_wallpaper(&mut self, path: &str) -> Result<serde_json::Value, StaticCallError> {
        raw_request(
            self.socket_path(),
            "set_static_wallpaper",
            serde_json::json!({ "path": path }),
        )
    }

    fn clear_static_wallpaper(&mut self) -> Result<serde_json::Value, StaticCallError> {
        raw_request(
            self.socket_path(),
            "clear_static_wallpaper",
            serde_json::json!({}),
        )
    }

    fn subscribe(&mut self) -> Result<Box<dyn EventSource>, String> {
        self.ensure_client()?;
        Ok(Box::new(RealEventSource::connect(&self.socket)?))
    }
}

/// 会话描述（系统页展示"我们连的是什么"，不猜）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// 客户端种类（`ipc` / `mock`）。
    pub kind: &'static str,
    /// socket 路径（mock 为 `None`）。
    pub socket: Option<String>,
}

/// 会话能力：断线后可重建连接（`docs/TUI_DESIGN.md` §5「断线可恢复」）。
pub trait Session: ControlClient {
    /// 描述当前会话（系统页只读诊断用）。
    fn info(&self) -> SessionInfo;
    /// 重建连接（成功返回 `Ok`）。
    ///
    /// # Errors
    ///
    /// 仍连不上时返回可读原因。
    fn reconnect(&mut self) -> Result<(), String>;
}

impl Session for IpcControl {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            kind: "ipc",
            socket: Some(self.socket.to_string_lossy().to_string()),
        }
    }

    fn reconnect(&mut self) -> Result<(), String> {
        self.client = None;
        self.ensure_client().map(|_| ())
    }
}

impl Session for MockControl {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            kind: "mock",
            socket: None,
        }
    }

    fn reconnect(&mut self) -> Result<(), String> {
        // 假客户端：清掉断线注入即视为恢复（并可模拟"仍断线"）
        if self.scenario.offline.is_some() && self.scenario.reconnect_fails {
            return Err(format!(
                "DEVICE_UNAVAILABLE: {}",
                self.scenario.offline.clone().unwrap_or_default()
            ));
        }
        self.scenario.offline = None;
        Ok(())
    }
}

/// 真实事件流：独立的 `subscribe` 连接（非阻塞读取，接入事件驱动循环）。
pub struct RealEventSource {
    stream: UnixStream,
    decoder: NdjsonDecoder,
}

impl std::fmt::Debug for RealEventSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealEventSource").finish_non_exhaustive()
    }
}

impl RealEventSource {
    /// 建立订阅并把 socket 切到非阻塞模式。
    ///
    /// # Errors
    ///
    /// 连接失败、ack 非 `ok`、或 ack 超时返回错误消息。
    pub fn connect(socket: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(socket).map_err(|e| format!("订阅失败：{e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(1500)))
            .map_err(|e| format!("订阅失败（设置超时）：{e}"))?;
        let req = Request::new("sub-1", Method::Subscribe, serde_json::json!({}));
        let bytes = encode_message(&req).map_err(|e| format!("订阅失败（编码）：{e}"))?;
        {
            use std::io::Write as _;
            let mut w = &stream;
            w.write_all(&bytes)
                .and_then(|()| w.flush())
                .map_err(|e| format!("订阅失败（发送）：{e}"))?;
        }
        let mut me = Self {
            stream,
            decoder: NdjsonDecoder::new(),
        };
        // ack 先于事件到达（服务端保证顺序）；读不到就明确失败，不静默降级
        if !me.read_ack()? {
            return Err("订阅被拒绝".to_string());
        }
        me.stream
            .set_nonblocking(true)
            .map_err(|e| format!("订阅失败（非阻塞）：{e}"))?;
        Ok(me)
    }

    /// 读取 `subscribe` 的 ack（阻塞，带 1.5s 超时）。
    fn read_ack(&mut self) -> Result<bool, String> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(resp) = self
                .decoder
                .next_message::<Response>()
                .map_err(|e| format!("订阅失败（解码）：{e}"))?
            {
                return Ok(resp.ok);
            }
            match std::io::Read::read(&mut self.stream, &mut buf) {
                Ok(0) => return Err("订阅失败：连接被关闭".to_string()),
                Ok(n) => self
                    .decoder
                    .feed(&buf[..n])
                    .map_err(|e| format!("订阅失败（缓冲）：{e}"))?,
                Err(e) => return Err(format!("订阅失败（读取 ack）：{e}")),
            }
        }
    }
}

impl EventSource for RealEventSource {
    fn try_next(&mut self) -> Result<Vec<EventMessage>, String> {
        let mut events = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            while let Some(ev) = self
                .decoder
                .next_message::<EventMessage>()
                .map_err(|e| format!("事件流解码失败：{e}"))?
            {
                events.push(ev);
            }
            match std::io::Read::read(&mut self.stream, &mut buf) {
                Ok(0) => return Err("事件流已关闭（daemon 退出？）".to_string()),
                Ok(n) => {
                    self.decoder
                        .feed(&buf[..n])
                        .map_err(|e| format!("事件流缓冲失败：{e}"))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(events),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("事件流读取失败：{e}")),
            }
            if events.len() >= 64 {
                return Ok(events); // 单次消费有界
            }
        }
    }
}

/// 一次假调用记录（供测试断言 TUI 的交互**真的**到达了客户端）。
#[derive(Debug, Clone, PartialEq)]
pub enum MockCall {
    /// [`ControlClient::status`]。
    Status,
    /// [`ControlClient::outputs`]。
    Outputs,
    /// `set_wallpaper`，携带**实际下发的参数**（含目标输出）。
    SetWallpaper(SetWallpaperParams),
    /// `next_media`。
    NextMedia(Option<String>),
    /// `set_static_wallpaper`（**只允许图片**；携带实际下发的路径）。
    SetStaticWallpaper(String),
    /// `clear_static_wallpaper`。
    ClearStaticWallpaper,
    /// `pause`。
    Pause(Option<String>),
    /// `resume`。
    Resume(Option<String>),
    /// `reload`。
    Reload,
    /// `set_transition`。
    SetTransition(TransitionParams),
    /// `list_sources`。
    ListSources,
    /// `scan_source`（id, full）。
    ScanSource(Option<String>, bool),
    /// `list_media`，携带**完整查询参数**（用于断言下推，而不是本地过滤）。
    ListMedia {
        /// 0 基页码。
        page: usize,
        /// 页大小。
        page_size: usize,
        /// 筛选。
        filter: MediaFilter,
        /// 排序。
        sort: MediaSort,
        /// 搜索词。
        search: String,
        /// 来源。
        source_id: Option<String>,
    },
    /// `get_media`。
    GetMedia(String),
    /// `add_source`。
    AddSource(String, String, bool),
    /// `get_metrics`。
    Metrics,
    /// `set_output_transform`。
    SetOutputTransform(SetOutputTransformParams),
    /// `save_config`。
    SaveConfig,
    /// `list_playlists`。
    ListPlaylists,
    /// `update_playlist`，携带**实际下发的参数**（含新建/更新与条目）。
    UpdatePlaylist(UpdatePlaylistParams),
    /// `remove_source`。
    RemoveSource(String),
    /// `subscribe`。
    Subscribe,
}

/// Mock 场景：用少量参数覆盖规范 §8 要求的全部数据形态。
#[derive(Debug, Clone, PartialEq)]
pub struct MockScenario {
    /// 输出个数（0 / 1 / 3）。
    pub outputs: usize,
    /// 媒体条数（0 / 10 / 10000）。
    pub media: usize,
    /// 顶层状态名（`StaticIdle` / `VideoPlaying` / `HeadlessReady` / `DegradedStatic` …）。
    pub state: String,
    /// 是否暂停。
    pub paused: bool,
    /// 解码器展示值（daemon 实际值；TUI **不得**按配置猜）。
    pub decoder: String,
    /// `Some(reason)` = 断线（所有调用返回该原因）。
    pub offline: Option<String>,
    /// 是否生成中文长文件名。
    pub long_names: bool,
    /// `Some(code)`：`set_wallpaper` 返回该错误（模拟 daemon 拒绝）。
    pub set_error: Option<String>,
    /// `set_wallpaper` 是否**受理但随后失败**（`WALLPAPER_APPLIED{ok=false}`）。
    pub apply_fails: bool,
    /// `set_wallpaper` 是否**受理但不发事件**（模拟事件丢失/未接入）。
    pub swallow_applied_event: bool,
    /// `list_media` 首次调用是否先报"正在扫描"。
    pub scanning_first: bool,
    /// `scan_source` 是否被拒绝（模拟 daemon 未配置来源）。
    pub scan_rejected: Option<String>,
    /// 断线后重连是否**仍然失败**（模拟 daemon 未启动）。
    pub reconnect_fails: bool,
    /// `save_config` 的 `saved`（默认 `true`）。
    pub save_saved: bool,
    /// `save_config` 的 **`verified`**（默认 `true`；`false` = 回读校验未通过）。
    pub save_verified: bool,
    /// daemon 是否**已支持静态壁纸接口**（t69）。
    ///
    /// 默认 `false` —— 与"本版真实 daemon 还没有这个方法"保持一致；
    /// 测试要验证成功路径时显式置 `true`。
    pub static_supported: bool,
    /// `save_config` 的 `merged_sources`（默认 0）。
    pub save_merged_sources: usize,
    /// `save_config` 回传的配置路径（默认规范路径；测试可改成别的以证明 UI 用的是响应值）。
    pub save_path: String,
    /// 模拟"来源被其它**配置容器**覆盖（如 `[slideshow].directory`）、**运行时删不掉**"：
    /// `remove_source` 记录调用但不真的移除；`save_config` 回读校验必然失败（`verified=false`）。
    pub root_derived: bool,
}

impl Default for MockScenario {
    fn default() -> Self {
        Self {
            outputs: 1,
            media: 10,
            state: "StaticIdle".to_string(),
            paused: false,
            decoder: "vaapi:vah264dec(nv12)".to_string(),
            offline: None,
            long_names: true,
            set_error: None,
            apply_fails: false,
            swallow_applied_event: false,
            scanning_first: false,
            scan_rejected: None,
            reconnect_fails: false,
            save_saved: true,
            save_verified: true,
            static_supported: false,
            save_merged_sources: 0,
            save_path: "/etc/liuguang/config.toml".to_string(),
            root_derived: false,
        }
    }
}

impl MockScenario {
    /// 无输出（HeadlessReady）。
    pub fn headless() -> Self {
        Self {
            outputs: 0,
            state: "HeadlessReady".to_string(),
            decoder: "none".to_string(),
            ..Self::default()
        }
    }

    /// 三输出。
    pub fn three_outputs() -> Self {
        Self {
            outputs: 3,
            ..Self::default()
        }
    }

    /// 断线。
    pub fn disconnected() -> Self {
        Self {
            offline: Some("connection refused（socket 不存在）".to_string()),
            ..Self::default()
        }
    }

    /// 视频播放中。
    pub fn playing() -> Self {
        Self {
            state: "VideoPlaying".to_string(),
            decoder: "vaapi:vah264dec(nv12)".to_string(),
            media: 10,
            ..Self::default()
        }
    }

    /// 暂停。
    pub fn paused() -> Self {
        Self {
            paused: true,
            ..Self::playing()
        }
    }

    /// 降级（视频失败、无后备画面）—— **不得**谎称 StaticIdle。
    pub fn degraded() -> Self {
        Self {
            state: "DegradedStatic".to_string(),
            decoder: "none".to_string(),
            ..Self::default()
        }
    }

    /// 指定媒体条数。
    pub fn with_media(mut self, media: usize) -> Self {
        self.media = media;
        self
    }

    /// 指定输出个数。
    pub fn with_outputs(mut self, outputs: usize) -> Self {
        self.outputs = outputs;
        self
    }
}

/// 测试/演示用假客户端：内存假数据 + 调用序列 + **可注入事件**。
#[derive(Debug, Clone)]
pub struct MockControl {
    /// 场景。
    pub scenario: MockScenario,
    /// 内部媒体库（**线上类型**，与真实 daemon 同构）。
    pub library: Vec<MediaItem>,
    /// 来源。
    pub sources: ListSourcesResult,
    /// 假状态。
    pub status: StatusResult,
    /// 假输出。
    pub outputs: OutputsResult,
    /// 假指标。
    pub metrics: MetricsResult,
    /// 按发生顺序记录的调用。
    pub calls: Vec<MockCall>,
    /// 等推送的事件（测试可随时注入）。
    pub events: Arc<Mutex<VecDeque<EventMessage>>>,
    /// 已订阅次数。
    pub subscriptions: usize,
    /// 假播放列表（`list_playlists` / `update_playlist` 的**同一份**状态）。
    pub playlists: Vec<Playlist>,
    /// 假静态壁纸（`set_static_wallpaper` 成功后记住；`clear` 清掉）。
    pub static_wallpaper: Option<String>,
}

impl Default for MockControl {
    fn default() -> Self {
        Self::from_scenario(MockScenario::default())
    }
}

fn make_library(count: usize, long_names: bool) -> Vec<MediaItem> {
    (0..count)
        .map(|i| {
            let kind = if i % 3 == 2 { "video" } else { "image" };
            let ext = if kind == "video" { "mp4" } else { "png" };
            let name = if long_names {
                format!(
                    "{i:05}_城市夜景_最终版_3840x2400_第三批修订_请勿删除_{}.{ext}",
                    "很长很长的文件名".repeat(2)
                )
            } else {
                format!("wallpaper_{i:05}.{ext}")
            };
            MediaItem {
                id: format!("/srv/wallpapers/收藏/2026/{name}"),
                source_id: "src-1".to_string(),
                name,
                path: format!("/srv/wallpapers/收藏/2026/{i:05}_x.{ext}"),
                kind: kind.to_string(),
                size_bytes: 1024 * 1024 * (1 + (i as u64 % 40)),
                modified_unix: 1_760_000_000 + i as i64,
                // t4：`MediaItem.details` 是新增的可选字段（旧服务端不带）。
                details: None,
            }
        })
        .collect()
}

impl MockControl {
    /// 由场景构造假客户端（构造即生成媒体库，**不读磁盘**）。
    pub fn from_scenario(scenario: MockScenario) -> Self {
        let media_is_empty = scenario.media == 0;
        let library = make_library(scenario.media, scenario.long_names);
        let outputs = Self::make_outputs(scenario.outputs);
        let status = Self::make_status(&scenario, &library, &outputs);
        Self {
            scenario,
            library,
            sources: ListSourcesResult {
                root: "/srv/wallpapers".to_string(),
                sources: vec![
                    MediaSource {
                        id: "src-1".to_string(),
                        name: "images".to_string(),
                        kind: "image".to_string(),
                        available: true,
                    },
                    MediaSource {
                        id: "src-2".to_string(),
                        name: "videos".to_string(),
                        kind: "video".to_string(),
                        available: true,
                    },
                ],
                missing: 0,
            },
            status,
            outputs,
            metrics: MetricsResult {
                thumb_requests: 3,
                thumb_hits: 2,
                thumb_misses: 1,
                thumb_decoded: 1,
                thumb_avg_ms: 42,
                thumb_queue_depth: 0,
                channels: Vec::new(),
                cpu_percent: 3.5,
                mem_bytes: 42 * 1024 * 1024,
                threads: 9,
                fps: 30.0,
                uptime_seconds: 4_242,
                commit: None,
                master: None,
                preload_cache_bytes: 0,
                preload_cache_items: 0,
                cpu_pressure: None,
            },
            playlists: if media_is_empty {
                Vec::new()
            } else {
                vec![Playlist {
                    id: "pl-1".to_string(),
                    name: "默认轮换".to_string(),
                    entries: Vec::new(),
                    mode: "sequential".to_string(),
                    interval_seconds: 600,
                    on_video_end: "next".to_string(),
                    output: None,
                    no_repeat: false,
                }]
            },
            calls: Vec::new(),
            static_wallpaper: None,
            events: Arc::new(Mutex::new(VecDeque::new())),
            subscriptions: 0,
        }
    }

    fn make_outputs(count: usize) -> OutputsResult {
        let names = ["DP-1", "HDMI-A-1", "DP-2"];
        OutputsResult {
            outputs: (0..count)
                .map(|i| liuguang_ipc::OutputStatus {
                    name: names[i % names.len()].to_string(),
                    connected: true,
                    mode: Some("3840x2400@60.000".to_string()),
                    mode_count: 6,
                })
                .collect(),
        }
    }

    fn make_status(
        scenario: &MockScenario,
        library: &[MediaItem],
        outputs: &OutputsResult,
    ) -> StatusResult {
        let first = outputs.outputs.first().map(|o| o.name.clone());
        let media = library.first().map(|m| m.id.clone());
        StatusResult {
            state: scenario.state.clone(),
            output: first,
            media: media.clone(),
            paused: scenario.paused,
            display_backend: "mock".to_string(),
            decoder: scenario.decoder.clone(),
            mode: Some("3840x2400@60.000".to_string()),
            uptime_seconds: 4_242,
            commit: None,
            master: None,
            // mock 后端不报告播放源（= "未报告"，页面按事实推断并标注）。
            playback_source: None,
            state_hint: None,
            channels: outputs
                .outputs
                .iter()
                .map(|o| liuguang_ipc::ChannelStatus {
                    output: o.name.clone(),
                    state: scenario.state.clone(),
                    media: media.clone(),
                    mode: o.mode.clone(),
                    paused: scenario.paused,
                    decoder: scenario.decoder.clone(),
                    video: None,
                    pinned: false,
                    pause_reason: None,
                })
                .collect(),
            fullscreen: None,
        }
    }

    /// 便捷：默认场景。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取记录的调用序列。
    pub fn calls(&self) -> &[MockCall] {
        &self.calls
    }

    /// 是否调用过 `save_config`。
    pub fn called_save_config(&self) -> bool {
        self.calls.contains(&MockCall::SaveConfig)
    }

    /// 是否调用过 `set_output_transform`（`Apply`）。
    pub fn called_apply(&self) -> bool {
        self.calls
            .iter()
            .any(|c| matches!(c, MockCall::SetOutputTransform(_)))
    }

    /// 最后一次 `set_wallpaper` 的目标输出（`None` = 未下发）。
    pub fn last_wallpaper_output(&self) -> Option<Option<String>> {
        self.calls.iter().rev().find_map(|c| match c {
            MockCall::SetWallpaper(p) => Some(p.output.clone()),
            _ => None,
        })
    }

    /// 所有 `set_wallpaper` 的目标输出（用于断言批量行为）。
    pub fn wallpaper_outputs(&self) -> Vec<Option<String>> {
        self.calls
            .iter()
            .filter_map(|c| match c {
                MockCall::SetWallpaper(p) => Some(p.output.clone()),
                _ => None,
            })
            .collect()
    }

    /// 注入一个事件（测试用）。
    pub fn push_event(&self, event: EventMessage) {
        if let Ok(mut q) = self.events.lock() {
            q.push_back(event);
        }
    }

    /// 注入 `WALLPAPER_APPLIED`。
    pub fn push_wallpaper_applied(&self, request_id: &str, output: Option<&str>, ok: bool) {
        let mut ev = EventMessage::new(EventKind::WallpaperApplied);
        ev.request_id = Some(request_id.to_string());
        ev.output = output.map(str::to_string);
        ev.ok = Some(ok);
        if !ok {
            ev.error = Some("解码失败：UNSUPPORTED_CODEC".to_string());
        }
        self.push_event(ev);
    }

    /// 注入 `ERROR`。
    pub fn push_error(&self, code: &str, message: &str) {
        let mut ev = EventMessage::new(EventKind::Error);
        ev.code = Some(code.to_string());
        ev.message = Some(message.to_string());
        self.push_event(ev);
    }

    /// 静态壁纸调用的记录 + 断线模拟（错误类型是 [`StaticCallError`]）。
    fn step_static(&mut self, call: MockCall) -> Result<(), StaticCallError> {
        self.calls.push(call);
        match &self.scenario.offline {
            Some(reason) => Err(StaticCallError::Failed(format!(
                "DEVICE_UNAVAILABLE: {reason}"
            ))),
            None => Ok(()),
        }
    }

    fn step(&mut self, call: MockCall) -> Result<(), String> {
        self.calls.push(call);
        match &self.scenario.offline {
            Some(reason) => Err(format!("DEVICE_UNAVAILABLE: {reason}")),
            None => Ok(()),
        }
    }

    /// 过滤 + 搜索 + 排序 + 分页（**与服务端语义一致**，用于验证下推参数生效）。
    fn page_of(&self, q: &MediaQuery) -> ListMediaResult {
        let size = q.page_size.max(1);
        let needle = q.search.trim().to_lowercase();
        let mut items: Vec<MediaItem> = self
            .library
            .iter()
            .filter(|m| match q.filter {
                MediaFilter::All => true,
                MediaFilter::Image => m.kind == "image",
                MediaFilter::Video => m.kind == "video",
            })
            .filter(|m| needle.is_empty() || m.name.to_lowercase().contains(&needle))
            .filter(|m| q.source_id.as_deref().is_none_or(|sid| m.source_id == sid))
            .cloned()
            .collect();
        match q.sort {
            MediaSort::Name => items.sort_by(|a, b| a.name.cmp(&b.name)),
            MediaSort::Size => items.sort_by_key(|m| std::cmp::Reverse(m.size_bytes)),
            MediaSort::Modified => items.sort_by_key(|m| std::cmp::Reverse(m.modified_unix)),
        }
        let total = items.len();
        let page = crate::model::clamp_page(q.page, total, size);
        let start = page * size;
        let end = (start + size).min(total);
        let truncated = total > crate::model::MAX_MEDIA_PAGE_SIZE;
        ListMediaResult {
            items: items[start..end].to_vec(),
            total,
            page,
            page_size: size,
            truncated,
            scanning: self.scenario.scanning_first && self.calls.len() <= 1,
            scan_id: 1,
            generated_unix_ms: 1_760_000_000_000,
            skipped_oversize: 0,
            skipped_examples: Vec::new(),
        }
    }
}

impl ControlClient for MockControl {
    fn status(&mut self) -> Result<StatusResult, String> {
        self.step(MockCall::Status)?;
        Ok(self.status.clone())
    }

    fn outputs(&mut self) -> Result<OutputsResult, String> {
        self.step(MockCall::Outputs)?;
        Ok(self.outputs.clone())
    }

    fn set_wallpaper(&mut self, params: SetWallpaperParams) -> Result<SetWallpaperAck, String> {
        self.step(MockCall::SetWallpaper(params.clone()))?;
        if let Some(code) = &self.scenario.set_error {
            return Err(code.clone());
        }
        let request_id = format!("set-{}", self.calls.len());
        if !self.scenario.swallow_applied_event {
            let ok = !self.scenario.apply_fails;
            self.push_wallpaper_applied(&request_id, params.output.as_deref(), ok);
        }
        self.status.media = Some(params.path);
        Ok(SetWallpaperAck {
            accepted: true,
            request_id: Some(request_id),
        })
    }

    fn next_media(&mut self, output: Option<String>) -> Result<(), String> {
        self.step(MockCall::NextMedia(output))
    }

    fn pause(&mut self, output: Option<String>) -> Result<(), String> {
        self.step(MockCall::Pause(output))?;
        self.status.paused = true;
        Ok(())
    }

    fn resume(&mut self, output: Option<String>) -> Result<(), String> {
        self.step(MockCall::Resume(output))?;
        self.status.paused = false;
        Ok(())
    }

    fn reload(&mut self) -> Result<(), String> {
        self.step(MockCall::Reload)
    }

    fn set_transition(&mut self, params: TransitionParams) -> Result<(), String> {
        self.step(MockCall::SetTransition(params))
    }

    fn list_sources(&mut self) -> Result<ListSourcesResult, String> {
        self.step(MockCall::ListSources)?;
        Ok(self.sources.clone())
    }

    fn scan_source(
        &mut self,
        source_id: Option<String>,
        full: bool,
    ) -> Result<ScanSourceResult, String> {
        self.step(MockCall::ScanSource(source_id.clone(), full))?;
        if let Some(err) = &self.scenario.scan_rejected {
            return Err(err.clone());
        }
        let mut ev = EventMessage::new(EventKind::ScanFinished);
        ev.scan_id = Some(2);
        ev.found = Some(self.library.len() as u64);
        self.push_event(ev);
        Ok(ScanSourceResult {
            accepted: true,
            scan_id: 2,
            superseded_running: false,
        })
    }

    fn list_media(&mut self, query: &MediaQuery) -> Result<ListMediaResult, String> {
        self.step(MockCall::ListMedia {
            page: query.page,
            page_size: query.page_size,
            filter: query.filter,
            sort: query.sort,
            search: query.search.clone(),
            source_id: query.source_id.clone(),
        })?;
        Ok(self.page_of(query))
    }

    fn get_media(&mut self, id: &str) -> Result<MediaItem, String> {
        self.step(MockCall::GetMedia(id.to_string()))?;
        self.library
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| format!("NOT_FOUND/detail_missing: 找不到 {id}"))
    }

    fn add_source(&mut self, kind: &str, path: &str, recursive: bool) -> Result<(), String> {
        self.step(MockCall::AddSource(
            kind.to_string(),
            path.to_string(),
            recursive,
        ))?;
        // 运行时真的新增（daemon 侧同样立即生效）
        let name = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .to_string();
        if !self
            .sources
            .sources
            .iter()
            .any(|s| s.id == path || s.name == name)
        {
            self.sources.sources.push(MediaSource {
                id: path.to_string(),
                name,
                kind: kind.to_string(),
                available: true,
            });
        }
        Ok(())
    }

    fn metrics(&mut self) -> Result<MetricsResult, String> {
        self.step(MockCall::Metrics)?;
        Ok(self.metrics.clone())
    }

    fn set_output_transform(&mut self, params: &SetOutputTransformParams) -> Result<(), String> {
        self.step(MockCall::SetOutputTransform(params.clone()))?;
        Ok(())
    }

    fn save_config(&mut self) -> Result<liuguang_ipc::protocol::SaveConfigResult, String> {
        self.step(MockCall::SaveConfig)?;
        let verified = if self.scenario.root_derived {
            false // 回读时它又回来了 ⇒ 校验必然不过（真机 F4）
        } else {
            self.scenario.save_verified
        };
        Ok(liuguang_ipc::protocol::SaveConfigResult {
            saved: self.scenario.save_saved,
            verified,
            merged_sources: self.scenario.save_merged_sources,
            path: self.scenario.save_path.clone(),
            bytes: 1234,
        })
    }

    fn list_playlists(&mut self) -> Result<Vec<liuguang_ipc::protocol::Playlist>, String> {
        self.step(MockCall::ListPlaylists)?;
        Ok(self.playlists.clone())
    }

    fn update_playlist(&mut self, params: UpdatePlaylistParams) -> Result<Playlist, String> {
        self.step(MockCall::UpdatePlaylist(params.clone()))?;
        // 与服务端同义：校验域值（UI 不应伪造合法值）。
        if !matches!(params.mode.as_str(), "sequential" | "random") {
            return Err(format!("INVALID_PARAMS: mode 非法（{}）", params.mode));
        }
        if !matches!(params.on_video_end.as_str(), "next" | "loop") {
            return Err(format!(
                "INVALID_PARAMS: on_video_end 非法（{}）",
                params.on_video_end
            ));
        }
        if params.interval_seconds == 0 || params.interval_seconds > 86_400 {
            return Err("INVALID_PARAMS: interval_seconds 必须在 1..=86400".to_string());
        }
        let id = params
            .playlist_id
            .clone()
            .unwrap_or_else(|| format!("pl-{}", self.playlists.len() + 1));
        let pl = Playlist {
            id: id.clone(),
            name: params.name,
            entries: params.entries,
            mode: params.mode,
            interval_seconds: params.interval_seconds,
            on_video_end: params.on_video_end,
            output: params.output,
            no_repeat: params.no_repeat,
        };
        match self.playlists.iter_mut().find(|p| p.id == id) {
            Some(slot) => *slot = pl.clone(),
            None => self.playlists.push(pl.clone()),
        }
        Ok(pl)
    }

    fn remove_source(&mut self, source_id: &str) -> Result<(), String> {
        self.step(MockCall::RemoveSource(source_id.to_string()))?;
        // 被其它**配置容器**覆盖的来源：daemon 记下了运行时删除，但它仍被覆盖 ⇒ 列表里还在
        if self.scenario.root_derived {
            return Ok(());
        }
        self.sources.sources.retain(|s| s.id != source_id);
        Ok(())
    }

    fn set_static_wallpaper(&mut self, path: &str) -> Result<serde_json::Value, StaticCallError> {
        self.step_static(MockCall::SetStaticWallpaper(path.to_string()))?;
        if !self.scenario.static_supported {
            return Err(StaticCallError::Unsupported(format!(
                "未知方法 `set_static_wallpaper`（服务端 mock 未实现该接口）：{path}"
            )));
        }
        self.static_wallpaper = Some(path.to_string());
        Ok(serde_json::json!({ "path": path }))
    }

    fn clear_static_wallpaper(&mut self) -> Result<serde_json::Value, StaticCallError> {
        self.step_static(MockCall::ClearStaticWallpaper)?;
        if !self.scenario.static_supported {
            return Err(StaticCallError::Unsupported(
                "未知方法 `clear_static_wallpaper`（服务端 mock 未实现该接口）".to_string(),
            ));
        }
        self.static_wallpaper = None;
        Ok(serde_json::json!({
            "restored_playlist": self.playlists.first().map(|p| p.id.clone()),
        }))
    }

    fn subscribe(&mut self) -> Result<Box<dyn EventSource>, String> {
        self.step(MockCall::Subscribe)?;
        self.subscriptions += 1;
        Ok(Box::new(MockEventSource {
            queue: Arc::clone(&self.events),
        }))
    }
}

/// 假事件流：直接消费注入的事件队列。
#[derive(Debug, Clone, Default)]
pub struct MockEventSource {
    queue: Arc<Mutex<VecDeque<EventMessage>>>,
}

impl EventSource for MockEventSource {
    fn try_next(&mut self) -> Result<Vec<EventMessage>, String> {
        let mut q = self
            .queue
            .lock()
            .map_err(|_| "事件队列锁中毒".to_string())?;
        Ok(q.drain(..).collect())
    }
}

/// 校验一条事件是否是"版本兼容"的（供真实/假事件流共用）。
pub fn event_is_supported(ev: &EventMessage) -> bool {
    ev.v == liuguang_ipc::PROTOCOL_VERSION
}

/// 便捷：把 NDJSON 一行解析成事件（供测试构造/断言）。
pub fn parse_event_line(line: &str) -> Result<EventMessage, String> {
    decode_message::<EventMessage>(line.as_bytes()).map_err(|e| e.to_string())
}
