//! IPC 服务端：Unix Domain Socket + NDJSON + peer credential 校验。
//!
//! 边界（规范 §3.3/§9.1/§11.3）：
//! - socket 文件权限 0660 + peer credential 校验（同 uid、root，或**socket 文件组**成员）；
//! - 每连接独立处理线程，并有**并发上限**与**握手超时**，失控客户端不能耗尽线程；
//! - 处理器（handler）只做请求路由——**阻塞解码必须在 daemon 主线程**，
//!   handler 通过通道转发后等待结果；
//! - 单条消息超过 [`crate::MAX_MESSAGE_BYTES`] 时返回 `MESSAGE_TOO_LARGE`
//!   并关闭该连接（流已不可信，无法再定位消息边界）。
//!
//! # 授权模型（实际生效的那一套）
//!
//! 允许访问的 peer 必须满足**任一**条件：
//!
//! 1. `uid == 服务进程 uid`；
//! 2. `uid == 0`（root）；
//! 3. `[allow_group]` 中显式列出的组；默认包含 **socket 文件自身所属的组**
//!    （`allow_socket_group = true`）—— 部署方用 systemd 的 `Group=` 决定这个组，
//!    因此"谁能控制 liuguang"是**部署时显式选择**的，而不是代码里的隐含约定。
//!
//! 补充组（supplementary groups）通过 `/proc/<peer_pid>/status` 的 `Groups:` 行读取；
//! 读取失败时**只退化为比较主组**（不会放宽授权）。读取凭证失败一律拒绝（fail-closed）。
//!
//! 默认部署不把任何人类用户放进该组时，授权模型就是"服务同 uid 或 root"，
//! 与 M2 的行为一致（向后兼容）。

use crate::framing::{FramingError, NdjsonDecoder, encode_message};
use crate::protocol::{
    ErrorBody, ErrorKind, EventMessage, Method, PROTOCOL_VERSION, Request, Response,
};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 默认并发连接上限（含 `subscribe` 长连接）。
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// 默认握手超时：连接建立后必须在该时间内送出第一条**完整**消息。
///
/// 这是绝对期限（不因"每次 read 都读到一点字节"而延长），
/// 因此"连上就不说话"和逐字节慢速发送都无法长期占用线程。
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// IPC 服务端错误。
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// socket 绑定/IO 失败。
    #[error("IPC socket 操作失败: {0}")]
    Io(#[from] std::io::Error),
    /// framing/消息错误。
    #[error("IPC 消息错误: {0}")]
    Framing(#[from] crate::framing::FramingError),
    /// socket 路径已被其他实例占用。
    #[error("IPC socket 已被占用: {0}")]
    AlreadyBound(PathBuf),
}

/// 服务端配置。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// socket 路径（默认 `/run/liuguang/control.sock`）。
    pub socket_path: PathBuf,
    /// 是否要求 peer 通过授权检查（默认 true）。
    ///
    /// 置为 `false` 表示**不做任何授权**（整机可访问），仅用于受控测试环境。
    pub require_same_uid: bool,
    /// socket 文件权限（默认 0o660）。
    pub mode: u32,
    /// 除"服务同 uid / root / socket 文件组"外，**额外**允许访问的组（gid）。
    pub allow_group: Option<u32>,
    /// 是否允许 socket 文件自身所属的组访问（默认 `true`）。
    ///
    /// 部署语义：systemd 单元的 `Group=` 决定了这个组，因此
    /// "把用户加入该组"即可获得控制权限；不加入则该用户被拒绝。
    pub allow_socket_group: bool,
    /// 最大并发连接数（默认 [`DEFAULT_MAX_CONNECTIONS`]）。
    pub max_connections: usize,
    /// 首条完整消息的绝对期限（默认 [`DEFAULT_HANDSHAKE_TIMEOUT`]）。
    pub handshake_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/liuguang/control.sock"),
            require_same_uid: true,
            mode: 0o660,
            allow_group: None,
            allow_socket_group: true,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

/// 对端进程身份（用于授权判定，可独立于 socket 构造，便于测试）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeerId {
    /// 对端 uid。
    pub uid: u32,
    /// 对端主组 gid。
    pub gid: u32,
    /// 对端 pid（`/proc/<pid>/status` 读取补充组用；不可用时为 0）。
    pub pid: i32,
    /// 对端补充组（best-effort；读取失败时为空）。
    pub supplementary_gids: Vec<u32>,
}

/// peer 授权策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPolicy {
    /// 是否启用授权检查；`false` 时对一切 peer 放行。
    pub require_same_uid: bool,
    /// 允许访问的组（gid）；空表示只允许"服务同 uid 或 root"。
    pub allow_groups: Vec<u32>,
}

impl Default for PeerPolicy {
    fn default() -> Self {
        Self {
            require_same_uid: true,
            allow_groups: Vec::new(),
        }
    }
}

impl PeerPolicy {
    /// 人类可读的允许清单（错误信息与日志共用）。
    fn describe(&self, server_uid: u32) -> String {
        let mut parts = vec![format!("服务同 uid（{server_uid}）"), "root".to_string()];
        if !self.allow_groups.is_empty() {
            let groups = self
                .allow_groups
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("组 {groups} 成员"));
        }
        parts.join("、")
    }
}

/// 纯函数授权判定：`Ok(())` 放行，`Err(原因)` 拒绝。
///
/// 抽成纯函数是为了让"不同 uid 被拒 / 组内被放行"这些**核心安全行为可被单测覆盖**，
/// 而不是只能靠同进程的 `UnixStream::pair()` 自证（审计 §5.2）。
#[must_use = "授权判定结果必须被处理"]
pub fn peer_decision(policy: &PeerPolicy, server_uid: u32, peer: &PeerId) -> Result<(), String> {
    if !policy.require_same_uid {
        return Ok(());
    }
    if peer.uid == server_uid || peer.uid == 0 {
        return Ok(());
    }
    let in_group = policy
        .allow_groups
        .iter()
        .any(|g| peer.gid == *g || peer.supplementary_gids.contains(g));
    if in_group {
        return Ok(());
    }
    Err(format!(
        "peer uid {} (pid {}) 未授权（允许：{}）",
        peer.uid,
        peer.pid,
        policy.describe(server_uid)
    ))
}

/// 解析 `/proc/<pid>/status` 文本中的 `Groups:` 行。
///
/// 抽出来是为了能用**真实的 status 文本样本**做单测（不需要另一个 uid）。
fn parse_groups_line(status_text: &str) -> Vec<u32> {
    status_text
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_uid_line(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("Uid:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// 读取对端补充组；若 `/proc/<pid>/status` 已属别的 UID（PID 复用竞态），拒绝授权。
fn peer_supplementary_gids(pid: i32, expected_uid: u32) -> Result<Vec<u32>, String> {
    if pid <= 0 {
        return Ok(Vec::new());
    }
    match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(text) => {
            if let Some(status_uid) = parse_uid_line(&text)
                && status_uid != expected_uid
            {
                return Err(format!(
                    "对端 PID {pid} 的 UID 已变更为 {status_uid}（期望 {expected_uid}）"
                ));
            }
            Ok(parse_groups_line(&text))
        }
        Err(e) => {
            tracing::debug!(
                pid,
                error = %e,
                "无法读取对端 /proc/<pid>/status，补充组判定退化为仅主组"
            );
            Ok(Vec::new())
        }
    }
}

/// 读取对端身份；**读取失败一律拒绝**（fail-closed）。
fn read_peer_id(stream: &UnixStream) -> Result<PeerId, String> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let cred = getsockopt(stream, PeerCredentials)
        .map_err(|e| format!("无法读取 peer credential: {e}"))?;
    let pid = cred.pid();
    let uid = cred.uid();
    Ok(PeerId {
        uid,
        gid: cred.gid(),
        pid,
        supplementary_gids: peer_supplementary_gids(pid, uid)?,
    })
}

/// 校验对端凭证（见模块文档的授权模型）。
fn peer_allowed(stream: &UnixStream, policy: &PeerPolicy) -> Result<(), String> {
    if !policy.require_same_uid {
        return Ok(());
    }
    let peer = read_peer_id(stream)?;
    peer_decision(policy, nix::unistd::Uid::current().as_raw(), &peer)
}

/// accept 循环的轮询间隔（t39：原来是 100ms，**每个新连接都要白等最多 100ms**）。
///
/// 5ms 是"发现新连接"的延迟，而不是请求处理延迟：接受的连接立刻交给自己的
/// 线程处理，所以新连接的握手/首帧应答不再受这个间隔影响（真机实测 99.9ms → <1ms）。
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// 延迟应答的最长等待（后台任务卡死时的兜底；正常情况下毫秒级返回）。
const DEFERRED_TIMEOUT: Duration = Duration::from_secs(120);

/// 处理结果：普通应答，或订阅确认 + 事件流。
#[derive(Debug)]
pub enum Dispatch {
    /// 普通请求-应答。
    Reply(Response),
    /// 请求已交给后台任务（**连接线程**在这里等，运行时线程继续跑）。
    ///
    /// t39：缩略图解码这类"重活"不能在运行时线程上同步做（会把渲染和其它请求
    /// 一起卡住）。处理方在 `handle()` 里立刻返回本变体，连接线程阻塞在 `rx`
    /// 上，后台线程算出结果后把 `Response` 发过来 —— 期间锁已释放、画面照常
    /// 出帧，其它连接也不受影响。
    Deferred {
        /// 应答投递端（后台任务完成后发一次）。
        rx: std::sync::mpsc::Receiver<Response>,
    },
    /// `subscribe`：先回确认帧，随后持续推送事件直到事件通道关闭。
    Subscribe {
        /// 确认应答（`accepted=true`）。
        ack: Response,
        /// 事件接收端；关闭即结束推送并断开本连接。
        events: std::sync::mpsc::Receiver<EventMessage>,
    },
}

/// 请求处理器：输入一条请求，返回 [`Dispatch`]。
///
/// 实现方通常把请求转发给 daemon 主线程并等待结果（不在本线程解码/渲染）。
pub trait RequestHandler: Send + Sync + 'static {
    /// 处理一条请求。
    fn handle(&self, request: Request) -> Dispatch;
}

impl<F> RequestHandler for F
where
    F: Fn(Request) -> Dispatch + Send + Sync + 'static,
{
    fn handle(&self, request: Request) -> Dispatch {
        (self)(request)
    }
}

/// 已绑定并可接受连接的 IPC 服务。
pub struct IpcServer {
    listener: UnixListener,
    config: ServerConfig,
    /// socket 文件的组（授权用；取不到时为 `None`）。
    socket_gid: Option<u32>,
}

impl IpcServer {
    /// 绑定 socket（清理陈旧 socket 文件；失败时报明确错误）。
    ///
    /// # Errors
    ///
    /// 路径被占用、目录不可写或权限设置失败时返回 [`IpcError`]。
    pub fn bind(config: ServerConfig) -> Result<Self, IpcError> {
        let path = &config.socket_path;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            use std::os::unix::fs::FileTypeExt as _;
            if !meta.file_type().is_socket() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("路径已存在且不是 Unix socket，拒绝清理: {}", path.display()),
                )
                .into());
            }
            // 陈旧 socket：能连上说明有实例在跑，拒绝；连不上则清理。
            match UnixStream::connect(path) {
                Ok(_) => return Err(IpcError::AlreadyBound(path.clone())),
                Err(_) => {
                    tracing::warn!(socket = %path.display(), "清理陈旧 IPC socket");
                    std::fs::remove_file(path)?;
                }
            }
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(config.mode))?;
        // socket 的组由进程 egid / 目录决定；部署用 systemd `Group=` 选择它。
        let socket_gid = std::fs::metadata(path).ok().map(|m| m.gid());
        let uid = nix::unistd::Uid::current().as_raw();
        let socket = Self {
            listener,
            config,
            socket_gid,
        };
        let policy = socket.policy();
        tracing::info!(
            socket = %socket.config.socket_path.display(),
            mode = format!("{:o}", socket.config.mode),
            socket_gid = ?socket_gid,
            server_uid = uid,
            authorization = %policy.describe(uid),
            "IPC socket 已监听"
        );
        Ok(socket)
    }

    /// socket 路径。
    pub fn path(&self) -> &Path {
        &self.config.socket_path
    }

    /// 由配置推导出的授权策略（socket 文件组 + 显式组）。
    fn policy(&self) -> PeerPolicy {
        let mut allow_groups = Vec::new();
        if let Some(g) = self.config.allow_group {
            allow_groups.push(g);
        }
        if self.config.allow_socket_group {
            if let Some(g) = self.socket_gid {
                if !allow_groups.contains(&g) {
                    allow_groups.push(g);
                }
            }
        }
        PeerPolicy {
            require_same_uid: self.config.require_same_uid,
            allow_groups,
        }
    }

    /// 在新线程启动 accept 循环；返回句柄用于停机。
    pub fn spawn<H: RequestHandler>(self, handler: H) -> ServerHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let path = self.config.socket_path.clone();
        let path_for_log = path.clone();
        let policy = self.policy();
        let max_connections = self.config.max_connections.max(1);
        let handshake_timeout = self.config.handshake_timeout;
        let live = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        let thread = std::thread::spawn(move || {
            let _ = self.listener.set_nonblocking(true);
            while !stop2.load(Ordering::SeqCst) {
                match self.listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let handler = Arc::clone(&handler);
                        let policy = policy.clone();
                        let live = Arc::clone(&live);
                        let count = live.fetch_add(1, Ordering::SeqCst) + 1;
                        let guard = ConnectionGuard {
                            live: Arc::clone(&live),
                        };
                        if count > max_connections {
                            // 超限：立即应答后关闭，避免线程/内存被耗尽。
                            tracing::warn!(
                                live = count,
                                max_connections,
                                "IPC 并发连接数超限，拒绝新连接"
                            );
                            let resp = Response::err(
                                "",
                                ErrorBody::new(
                                    ErrorKind::ServerBusy,
                                    format!(
                                        "服务繁忙：并发连接数已达上限 {max_connections}；请稍后重试或断开闲置客户端"
                                    ),
                                ),
                            );
                            let _ = write_message(&mut stream, &resp);
                            drop(guard);
                            continue;
                        }
                        // 每连接一个线程；线程内循环读消息直到对端断开。
                        std::thread::spawn(move || {
                            let _guard = guard;
                            handle_connection(stream, &*handler, &policy, handshake_timeout);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(ACCEPT_POLL_INTERVAL);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept 失败，继续");
                        std::thread::sleep(ACCEPT_POLL_INTERVAL);
                    }
                }
            }
            tracing::info!(socket = %path_for_log.display(), "IPC accept 循环已停止");
        });
        ServerHandle {
            stop,
            thread: Some(thread),
            socket_path: path,
        }
    }
}

/// 连接计数守卫：无论线程从哪条路径退出都会归还名额。
struct ConnectionGuard {
    live: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 服务线程句柄。
pub struct ServerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    socket_path: PathBuf,
}

impl ServerHandle {
    /// 请求停机并等待线程退出；清理 socket 文件。
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        tracing::info!(socket = %self.socket_path.display(), "IPC 服务已停止");
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// 一条已解码行的处置结果。
enum LineOutcome {
    /// 结构完整、版本受支持的请求。
    Ready(Request),
    /// 被拒绝的行：带可回传的 `id`（可能为空）、错误码与说明。
    Rejected {
        /// 请求 ID（能取到就回传，取不到为空）。
        id: String,
        /// 稳定错误码。
        code: ErrorKind,
        /// 面向人的说明。
        message: String,
    },
}

/// **版本优先**地解释一行 JSON。
///
/// 顺序很重要（审计 §4.1b）：先判 `v`，再做方法/结构校验。否则"新版本客户端新增
/// 一个方法名"会先在 JSON 层失败并返回 `INVALID_MESSAGE`，而不是
/// `UNSUPPORTED_VERSION`——客户端据此无法区分"版本太新"与"消息写错了"。
fn interpret_line(value: serde_json::Value) -> LineOutcome {
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    match value.get("v") {
        None => {
            return LineOutcome::Rejected {
                id,
                code: ErrorKind::InvalidMessage,
                message: "缺少必需字段 `v`（协议版本）".to_string(),
            };
        }
        Some(v) => {
            if v.as_u64() != Some(u64::from(PROTOCOL_VERSION)) {
                return LineOutcome::Rejected {
                    id,
                    code: ErrorKind::UnsupportedVersion,
                    message: format!("协议版本 {v} 不受支持（当前 v{PROTOCOL_VERSION}）"),
                };
            }
        }
    }

    if let Some(name) = value.get("method").and_then(|m| m.as_str()) {
        if Method::from_wire(name).is_none() {
            return LineOutcome::Rejected {
                id,
                code: ErrorKind::UnsupportedMethod,
                message: format!(
                    "未知方法 `{name}`（服务端为协议 v{PROTOCOL_VERSION}；同一版本内方法只增不改，\
                     请检查客户端与服务端版本是否匹配）"
                ),
            };
        }
    }

    match serde_json::from_value::<Request>(value) {
        Ok(req) => LineOutcome::Ready(req),
        Err(e) => LineOutcome::Rejected {
            id,
            code: ErrorKind::InvalidMessage,
            message: format!("JSON 编解码失败: {e}"),
        },
    }
}

fn handle_connection<H: RequestHandler>(
    mut stream: UnixStream,
    handler: &H,
    policy: &PeerPolicy,
    handshake_timeout: Duration,
) {
    if let Err(reason) = peer_allowed(&stream, policy) {
        tracing::warn!(reason = %reason, "拒绝未授权连接（peer credential）");
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let resp = Response::err(
            "",
            ErrorBody::new(ErrorKind::PermissionDenied, format!("权限不足：{reason}")),
        );
        let _ = write_message(&mut stream, &resp);
        return;
    }
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut decoder = NdjsonDecoder::new();
    let mut buf = [0u8; 4096];
    let mut handshake_done = false;
    // 绝对期限：不因"每次都能读到一点字节"而延长。
    let handshake_deadline = Instant::now() + handshake_timeout;
    loop {
        if !handshake_done {
            let remaining = handshake_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                tracing::debug!("IPC 握手超时（未送出完整消息），关闭连接");
                return;
            }
            let _ = stream.set_read_timeout(Some(remaining));
        }
        match stream.read(&mut buf) {
            Ok(0) => return, // 对端正常断开
            Ok(n) => {
                if let Err(e) = decoder.feed(&buf[..n]) {
                    // 缓冲已被清空、消息边界不可恢复 → 必须关连接。
                    let resp = Response::err(
                        "",
                        ErrorBody::new(ErrorKind::MessageTooLarge, e.to_string()),
                    );
                    let _ = write_message(&mut stream, &resp);
                    return;
                }
                loop {
                    match decoder.next_message::<serde_json::Value>() {
                        Ok(Some(value)) => {
                            handshake_done = true;
                            // 握手结束：不再对空闲连接设限期（长连客户端如 TUI 会保持连接）。
                            let _ = stream.set_read_timeout(None);
                            match interpret_line(value) {
                                LineOutcome::Rejected { id, code, message } => {
                                    let resp = Response::err(id, ErrorBody::new(code, message));
                                    if write_message(&mut stream, &resp).is_err() {
                                        return;
                                    }
                                }
                                LineOutcome::Ready(req) => {
                                    let req_id = req.id.clone();
                                    match dispatch(req, handler) {
                                        Dispatch::Reply(resp) => {
                                            if write_message(&mut stream, &resp).is_err() {
                                                return; // 写失败：对端已断开，不影响守护进程
                                            }
                                        }
                                        Dispatch::Deferred { rx } => {
                                            // 后台任务完成前只占住本连接线程（不占运行时锁）。
                                            // 超时兜底：宁可给一条明确错误，也不让客户端干等。
                                            let mut resp = match rx.recv_timeout(DEFERRED_TIMEOUT) {
                                                Ok(resp) => resp,
                                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                                    Response::err(
                                                        &req_id,
                                                        ErrorBody::new(
                                                            ErrorKind::Internal,
                                                            format!(
                                                                "后台任务超过 {}s 未完成",
                                                                DEFERRED_TIMEOUT.as_secs()
                                                            ),
                                                        ),
                                                    )
                                                }
                                                Err(
                                                    std::sync::mpsc::RecvTimeoutError::Disconnected,
                                                ) => Response::err(
                                                    &req_id,
                                                    ErrorBody::new(
                                                        ErrorKind::Internal,
                                                        "后台任务已被丢弃（服务正在退出）",
                                                    ),
                                                ),
                                            };
                                            ensure_request_id(&mut resp, &req_id);
                                            if write_message(&mut stream, &resp).is_err() {
                                                return;
                                            }
                                        }
                                        Dispatch::Subscribe { ack, events } => {
                                            if write_message(&mut stream, &ack).is_err() {
                                                return;
                                            }
                                            // 事件推送：直到事件通道关闭或写失败。
                                            // 订阅连接不再处理后续请求（命令请走独立连接）。
                                            loop {
                                                match events
                                                    .recv_timeout(Duration::from_secs(1))
                                                {
                                                    Ok(event) => {
                                                        if write_event(&mut stream, &event)
                                                            .is_err()
                                                        {
                                                            return;
                                                        }
                                                    }
                                                    Err(
                                                        std::sync::mpsc::RecvTimeoutError::Timeout,
                                                    ) => continue,
                                                    Err(
                                                        std::sync::mpsc::RecvTimeoutError::Disconnected,
                                                    ) => return,
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            // 超限的"完整行"必须与超限的半行同码，并同样关闭连接
                            // （审计 §4.1a：同一语义不允许两个错误码）。
                            let (code, close) = match e {
                                FramingError::MessageTooLarge { .. } => {
                                    (ErrorKind::MessageTooLarge, true)
                                }
                                FramingError::Json(_) => (ErrorKind::InvalidMessage, false),
                            };
                            let resp = Response::err("", ErrorBody::new(code, e.to_string()));
                            if write_message(&mut stream, &resp).is_err() {
                                return;
                            }
                            if close {
                                return;
                            }
                        }
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !handshake_done {
                    tracing::debug!("IPC 握手超时（读超时），关闭连接");
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "连接读取结束");
                return;
            }
        }
    }
}

fn dispatch<H: RequestHandler>(req: Request, handler: &H) -> Dispatch {
    let id = req.id.clone();
    match handler.handle(req) {
        Dispatch::Reply(mut resp) => {
            ensure_request_id(&mut resp, &id);
            Dispatch::Reply(resp)
        }
        Dispatch::Subscribe { mut ack, events } => {
            ensure_request_id(&mut ack, &id);
            Dispatch::Subscribe { ack, events }
        }
        Dispatch::Deferred { rx } => Dispatch::Deferred { rx },
    }
}

/// 处理器忘记回填 `id` 时由服务端补齐。
///
/// 否则客户端按 `id` 配对会报"响应 ID 不匹配"，把真实原因（超时/繁忙/权限）
/// 完全掩盖（审计 §3.7/§4.2）。
fn ensure_request_id(resp: &mut Response, id: &str) {
    if resp.id.is_empty() && !id.is_empty() {
        resp.id = id.to_string();
    }
}

/// 便捷：只返回普通应答的处理器包装。
pub fn reply_only<F>(f: F) -> impl RequestHandler
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    move |req: Request| Dispatch::Reply(f(req))
}

fn write_message(stream: &mut UnixStream, response: &Response) -> std::io::Result<()> {
    let bytes = encode_message(response).map_err(std::io::Error::other)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn write_event(stream: &mut UnixStream, event: &EventMessage) -> std::io::Result<()> {
    let bytes = encode_message(event).map_err(std::io::Error::other)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::protocol::{AcceptedResult, ErrorKind};
    use std::sync::atomic::AtomicU32;

    fn test_socket(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "liuguang-ipc-{name}-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        p
    }

    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn echo_handler() -> impl RequestHandler {
        |req: Request| -> Dispatch {
            match req.method {
                Method::Status => {
                    Dispatch::Reply(Response::ok(req.id, serde_json::json!({"pong": true})))
                }
                Method::Subscribe => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let mut ev = crate::protocol::EventMessage::new(
                        crate::protocol::EventKind::StateChanged,
                    )
                    .with_state("StaticIdle");
                    ev.v = crate::protocol::PROTOCOL_VERSION;
                    let _ = tx.send(ev);
                    drop(tx); // 单事件后结束推送
                    Dispatch::Subscribe {
                        ack: Response::ok(
                            req.id,
                            serde_json::json!({"accepted": true, "stream": "events"}),
                        ),
                        events: rx,
                    }
                }
                _ => Dispatch::Reply(Response::err(
                    req.id,
                    ErrorBody::new(
                        crate::protocol::ErrorKind::NotImplemented,
                        "测试 handler 仅实现 status",
                    ),
                )),
            }
        }
    }

    /// 模拟"忘记回填 id"的处理器（daemon 60s 超时路径过去就是这样）。
    fn forgetful_id_handler() -> impl RequestHandler {
        |_req: Request| -> Dispatch {
            Dispatch::Reply(Response::err(
                "",
                ErrorBody::new(ErrorKind::Internal, "处理器超时"),
            ))
        }
    }

    fn spawn_server_with(
        name: &str,
        config: impl FnOnce(&mut ServerConfig),
        handler: impl RequestHandler,
    ) -> (PathBuf, ServerHandle) {
        let path = test_socket(name);
        let mut cfg = ServerConfig {
            socket_path: path.clone(),
            ..ServerConfig::default()
        };
        config(&mut cfg);
        let server = IpcServer::bind(cfg).unwrap();
        let handle = server.spawn(handler);
        (path, handle)
    }

    fn read_one_response(stream: &mut UnixStream, decoder: &mut NdjsonDecoder) -> Response {
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(resp) = decoder.next_message::<Response>().unwrap() {
                return resp;
            }
            assert!(Instant::now() < deadline, "等待响应超时");
            match stream.read(&mut buf) {
                Ok(0) => panic!("连接被关闭，未收到响应"),
                Ok(n) => decoder.feed(&buf[..n]).unwrap(),
                Err(e) => panic!("读取失败: {e}"),
            }
        }
    }

    /// **t39**：`Dispatch::Deferred` —— 运行时线程不阻塞，由连接线程等后台任务。
    ///
    /// 缩略图请求走这条路径：`handle()` 立即返回（不占运行时线程），
    /// 解码线程完成后把响应送回。这里用"30ms 后才 send"验证连接线程确实在等。
    #[test]
    fn deferred_dispatch_answers_when_the_sender_fires() {
        let handler = |req: Request| -> Dispatch {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                let _ = tx.send(Response::ok(req.id, serde_json::json!({"delayed": true})));
            });
            Dispatch::Deferred { rx }
        };
        let (path, handle) = spawn_server_with("deferred", |_| {}, handler);
        let mut client = crate::client::IpcClient::connect(&path).unwrap();
        let started = Instant::now();
        let resp = client
            .request(Method::Status, serde_json::json!({}))
            .unwrap();
        assert!(resp.ok, "延迟响应也必须成功: {:?}", resp.error);
        assert_eq!(resp.result, Some(serde_json::json!({"delayed": true})));
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "必须真的等后台任务完成"
        );
        handle.shutdown();
    }

    /// **t39**：accept 轮询必须短于 UI 可感知阈值。
    ///
    /// 旧值 100ms 让**每条新连接**（GUI 每次刷新、每次 `liuguangctl status`）
    /// 白等最多 100ms —— 真机探针 `gui_latency.py --rtt` 中位 99.9ms 就是它。
    #[test]
    fn accept_poll_interval_is_short_enough_for_ui() {
        assert!(
            ACCEPT_POLL_INTERVAL <= Duration::from_millis(5),
            "accept 轮询必须 ≤5ms，实际 {ACCEPT_POLL_INTERVAL:?}"
        );
    }

    #[test]
    fn bind_and_shutdown_cleans_socket() {
        let (path, handle) = spawn_server_with("bind", |_| {}, echo_handler());
        assert!(path.exists());
        handle.shutdown();
        assert!(!path.exists(), "停机后应清理 socket 文件");
    }

    #[test]
    fn double_bind_is_rejected() {
        let path = test_socket("double");
        let server = IpcServer::bind(ServerConfig {
            socket_path: path.clone(),
            ..ServerConfig::default()
        })
        .unwrap();
        let err = match IpcServer::bind(ServerConfig {
            socket_path: path.clone(),
            ..ServerConfig::default()
        }) {
            Err(e) => e,
            Ok(_) => panic!("重复绑定应失败"),
        };
        assert!(matches!(err, IpcError::AlreadyBound(_)));
        drop(server);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn request_response_round_trip() {
        let (path, handle) = spawn_server_with("roundtrip", |_| {}, echo_handler());

        let mut client = crate::client::IpcClient::connect(&path).unwrap();
        let resp = client
            .request(Method::Status, serde_json::json!({}))
            .unwrap();
        assert!(resp.ok);
        assert_eq!(resp.result, Some(serde_json::json!({"pong": true})));

        let resp = client.request(Method::Next, serde_json::json!({})).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.map(|e| e.code), Some(ErrorKind::NotImplemented));

        handle.shutdown();
    }

    #[test]
    fn client_survives_server_side_error_and_server_survives_client_disconnect() {
        let (path, handle) = spawn_server_with("resilience", |_| {}, echo_handler());

        // 客户端 1：请求后直接断开（不发 EOF 优雅关闭）。
        {
            let mut client = crate::client::IpcClient::connect(&path).unwrap();
            client
                .request(Method::Status, serde_json::json!({}))
                .unwrap();
        }

        // 客户端 2：仍可正常使用。
        let mut client = crate::client::IpcClient::connect(&path).unwrap();
        let resp = client
            .request(Method::Status, serde_json::json!({}))
            .unwrap();
        assert!(resp.ok);
        handle.shutdown();
    }

    #[test]
    fn version_mismatch_is_rejected_by_server() {
        let (path, handle) = spawn_server_with("version", |_| {}, echo_handler());

        let mut stream = UnixStream::connect(&path).unwrap();
        let bad = Request {
            v: 99,
            id: "x".into(),
            method: Method::Status,
            params: serde_json::json!({}),
        };
        let bytes = encode_message(&bad).unwrap();
        stream.write_all(&bytes).unwrap();

        let mut decoder = NdjsonDecoder::new();
        let resp = read_one_response(&mut stream, &mut decoder);
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, ErrorKind::UnsupportedVersion);
        // 版本错误的应答也必须带回请求 ID（审计 §4.1b 的 id 为空问题）。
        assert_eq!(resp.id, "x");
        handle.shutdown();
    }

    /// 回归（审计 §4.1b）：**新版本客户端 + 未知方法**必须报 `UNSUPPORTED_VERSION`，
    /// 而不是在 JSON 层先失败成 `INVALID_MESSAGE`。
    #[test]
    fn unknown_method_on_newer_version_reports_version_mismatch() {
        let (path, handle) = spawn_server_with("preparse-version", |_| {}, echo_handler());

        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .write_all(b"{\"v\":2,\"id\":\"nv-1\",\"method\":\"set_fit\",\"params\":{}}\n")
            .unwrap();

        let mut decoder = NdjsonDecoder::new();
        let resp = read_one_response(&mut stream, &mut decoder);
        assert!(!resp.ok);
        assert_eq!(
            resp.error.as_ref().unwrap().code,
            ErrorKind::UnsupportedVersion,
            "报文体: {resp:?}"
        );
        assert_eq!(resp.id, "nv-1");
        handle.shutdown();
    }

    /// 回归（审计 §4.1b 的"同一版本只增不改"）：v=1 + 未知方法 → `UNSUPPORTED_METHOD`。
    #[test]
    fn unknown_method_on_current_version_reports_unsupported_method() {
        let (path, handle) = spawn_server_with("preparse-method", |_| {}, echo_handler());

        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .write_all(b"{\"v\":1,\"id\":\"um-1\",\"method\":\"set_fit\",\"params\":{}}\n")
            .unwrap();

        let mut decoder = NdjsonDecoder::new();
        let resp = read_one_response(&mut stream, &mut decoder);
        assert!(!resp.ok);
        assert_eq!(
            resp.error.as_ref().unwrap().code,
            ErrorKind::UnsupportedMethod
        );
        assert_eq!(resp.id, "um-1");
        handle.shutdown();
    }

    #[test]
    fn malformed_line_gets_error_but_connection_continues() {
        let (path, handle) = spawn_server_with("malformed", |_| {}, echo_handler());

        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"{not json}\n").unwrap();
        let good = Request::new("ok-1", Method::Status, serde_json::json!({}));
        stream.write_all(&encode_message(&good).unwrap()).unwrap();

        let mut decoder = NdjsonDecoder::new();
        let mut buf = [0u8; 4096];
        let mut responses = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while responses.len() < 2 && std::time::Instant::now() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    decoder.feed(&buf[..n]).unwrap();
                    while let Some(resp) = decoder.next_message::<Response>().unwrap() {
                        responses.push(resp);
                    }
                }
                Err(_) => break,
            }
        }
        assert_eq!(responses.len(), 2, "坏行错误 + 正常响应");
        assert!(!responses[0].ok);
        assert_eq!(
            responses[0].error.as_ref().unwrap().code,
            ErrorKind::InvalidMessage
        );
        assert!(responses[1].ok);
        assert_eq!(responses[1].id, "ok-1");
        handle.shutdown();
    }

    #[test]
    fn subscribe_streams_events_then_closes() {
        let (path, handle) = spawn_server_with("subscribe", |_| {}, echo_handler());

        let mut stream = UnixStream::connect(&path).unwrap();
        let sub = Request::new("sub-1", Method::Subscribe, serde_json::json!({}));
        stream.write_all(&encode_message(&sub).unwrap()).unwrap();

        let mut decoder = NdjsonDecoder::new();
        let mut buf = [0u8; 4096];
        let mut frames = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while frames.len() < 2 && std::time::Instant::now() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    decoder.feed(&buf[..n]).unwrap();
                    while let Some(v) = decoder.next_message::<serde_json::Value>().unwrap() {
                        frames.push(v);
                    }
                }
                Err(_) => break,
            }
        }
        assert_eq!(frames.len(), 2, "确认帧 + 事件帧: {frames:?}");
        assert_eq!(frames[0]["ok"], true);
        assert_eq!(frames[0]["id"], "sub-1");
        assert_eq!(frames[1]["kind"], "STATE_CHANGED");
        assert_eq!(frames[1]["state"], "StaticIdle");
        handle.shutdown();
    }

    #[test]
    fn accepted_result_shape() {
        let v = serde_json::to_value(AcceptedResult::accepted()).unwrap();
        assert_eq!(v, serde_json::json!({"accepted": true}));
    }

    // ---- 授权判定：用纯函数覆盖"不同 uid / 组"这些真实分支 ----

    #[test]
    fn same_uid_and_root_are_allowed() {
        let policy = PeerPolicy::default();
        let same = PeerId {
            uid: 1000,
            gid: 1000,
            pid: 42,
            supplementary_gids: vec![],
        };
        assert!(peer_decision(&policy, 1000, &same).is_ok());
        let root = PeerId {
            uid: 0,
            ..same.clone()
        };
        assert!(peer_decision(&policy, 1000, &root).is_ok());
    }

    /// 核心安全行为（审计 §5.2：同进程 `UnixStream::pair()` 永远测不到这条）。
    #[test]
    fn different_uid_without_group_is_denied() {
        let policy = PeerPolicy::default();
        let other = PeerId {
            uid: 65534,
            gid: 65534,
            pid: 7,
            supplementary_gids: vec![65534],
        };
        let err = peer_decision(&policy, 1000, &other).unwrap_err();
        assert!(err.contains("65534"), "错误信息应含 uid: {err}");
        assert!(err.contains("root"), "错误信息应说明允许谁: {err}");
    }

    #[test]
    fn group_membership_allows_including_supplementary_groups() {
        let policy = PeerPolicy {
            require_same_uid: true,
            allow_groups: vec![3900],
        };
        // 主组命中
        let primary = PeerId {
            uid: 1000,
            gid: 3900,
            pid: 8,
            supplementary_gids: vec![],
        };
        assert!(peer_decision(&policy, 388, &primary).is_ok());
        // 补充组命中（`usermod -aG` 的常见结果）
        let supplementary = PeerId {
            uid: 1000,
            gid: 1000,
            pid: 9,
            supplementary_gids: vec![10, 39, 3900],
        };
        assert!(peer_decision(&policy, 388, &supplementary).is_ok());
        // 与之无关的组仍然被拒
        let unrelated = PeerId {
            uid: 1000,
            gid: 1000,
            pid: 10,
            supplementary_gids: vec![10, 39],
        };
        assert!(peer_decision(&policy, 388, &unrelated).is_err());
    }

    #[test]
    fn require_same_uid_false_opens_everything() {
        let policy = PeerPolicy {
            require_same_uid: false,
            allow_groups: vec![],
        };
        let anyone = PeerId {
            uid: 12345,
            gid: 12345,
            pid: 1,
            supplementary_gids: vec![],
        };
        assert!(peer_decision(&policy, 1000, &anyone).is_ok());
    }

    /// socket 文件组默认被授权（部署用 systemd `Group=` 选择这个组）。
    #[test]
    fn socket_group_becomes_authorized_group() {
        let (path, handle) = spawn_server_with("socket-group", |_| {}, echo_handler());
        let gid = std::fs::metadata(&path).unwrap().gid();
        let server = IpcServer::bind(ServerConfig {
            socket_path: test_socket("socket-group-probe"),
            ..ServerConfig::default()
        })
        .unwrap();
        assert_eq!(server.socket_gid, Some(gid), "socket 组应被记录用于授权");
        drop(server);
        handle.shutdown();
    }

    /// socket 上记录不到的组不会被凭空授权。
    #[test]
    fn missing_socket_gid_yields_no_group_authorization() {
        let mut path = std::env::temp_dir();
        path.push("liuguang-nonexistent-probe.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = IpcServer {
            listener,
            config: ServerConfig {
                socket_path: path.clone(),
                ..ServerConfig::default()
            },
            socket_gid: None,
        };
        assert_eq!(server.policy().allow_groups, Vec::<u32>::new());
        drop(server);
        let _ = std::fs::remove_file(&path);
    }

    /// 真实 `/proc/<pid>/status` 片段（Fedora 44 实测格式，注意行尾空格）。
    #[test]
    fn groups_line_from_real_proc_status_parses() {
        let sample = "Name:\tsleep\nUmask:\t0022\nState:\tS (sleeping)\n\
                      Tgid:\t28553\nPid:\t28553\nPPid:\t1\n\
                      Uid:\t1000\t1000\t1000\t1000\n\
                      Gid:\t1000\t1000\t1000\t1000\n\
                      Groups:\t10 39 1000 \n\
                      NStgid:\t28553\n";
        assert_eq!(parse_groups_line(sample), vec![10, 39, 1000]);

        // 没有 Groups 行 / 空列表 / 垃圾内容都不能 panic。
        assert_eq!(parse_groups_line("Name:\tx\n"), Vec::<u32>::new());
        assert_eq!(parse_groups_line("Groups:\n"), Vec::<u32>::new());
        assert_eq!(parse_groups_line("Groups:\t10 abc 39\n"), vec![10, 39]);
    }

    // ---- 连接上限 / 握手超时 / id 回填 ----

    /// 回归（审计 §3.7）：handler 用空 id 应答时，服务端必须回填请求 id，
    /// 否则客户端只看到"响应 ID 不匹配"，真实原因被掩盖。
    #[test]
    fn server_backfills_request_id_on_handler_error() {
        let (path, handle) = spawn_server_with("id-backfill", |_| {}, forgetful_id_handler());

        let mut client = crate::client::IpcClient::connect(&path).unwrap();
        let resp = client
            .request(Method::Status, serde_json::json!({}))
            .expect("客户端不应因 ID 不匹配而失败");
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, ErrorKind::Internal);
        assert_eq!(resp.id, "req-1");
        handle.shutdown();
    }

    /// 回归（审计 §3.7）：并发连接数必须有上限，超限连接得到明确错误而不是被静默丢弃。
    #[test]
    fn connection_limit_is_enforced() {
        let (path, handle) =
            spawn_server_with("conn-limit", |cfg| cfg.max_connections = 1, echo_handler());

        // 连接 1：连上但不发言（占住名额；握手超时未到）。
        let _idle = UnixStream::connect(&path).unwrap();

        // 连接 2：超限 → 立即收到 SERVER_BUSY。
        let mut second = UnixStream::connect(&path).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut decoder = NdjsonDecoder::new();
        let resp = read_one_response(&mut second, &mut decoder);
        assert!(!resp.ok);
        assert_eq!(
            resp.error.as_ref().unwrap().code,
            ErrorKind::ServerBusy,
            "报文体: {resp:?}"
        );

        // 名额归还后新连接仍可用。
        drop(_idle);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut client = crate::client::IpcClient::connect(&path).unwrap();
            if let Ok(resp) = client.request(Method::Status, serde_json::json!({})) {
                assert!(resp.ok, "名额释放后应恢复服务: {resp:?}");
                break;
            }
            assert!(Instant::now() < deadline, "等待名额释放超时");
            std::thread::sleep(Duration::from_millis(50));
        }
        handle.shutdown();
    }

    /// 回归（审计 §3.7）：只 `connect()` 不发送任何字节的客户端不能永久占住线程。
    #[test]
    fn silent_connection_is_closed_by_handshake_timeout() {
        let (path, handle) = spawn_server_with(
            "handshake-timeout",
            |cfg| cfg.handshake_timeout = Duration::from_millis(300),
            echo_handler(),
        );

        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let started = Instant::now();
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "服务端应主动关闭静默连接");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "应在握手超时附近关闭，实际 {:?}",
            started.elapsed()
        );
        handle.shutdown();
    }

    /// 回归（审计 §4.1a）：**超限的完整行**必须与超限的半行同码，
    /// 并且连接被关闭（流不可信）。
    #[test]
    fn oversize_complete_line_reports_too_large_and_closes() {
        let (path, handle) = spawn_server_with(
            "oversize-complete",
            |cfg| cfg.handshake_timeout = Duration::from_secs(5),
            echo_handler(),
        );

        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        // 分片发送，避免触发"未完成行"上限路径，从而真正走到"完整超限行"。
        let big = vec![b' '; crate::protocol::MAX_MESSAGE_BYTES + 1];
        let (head, tail) = big.split_at(crate::protocol::MAX_MESSAGE_BYTES - 1);
        let mut first = Vec::from(head);
        first.extend_from_slice(tail);
        first.push(b'\n');
        stream.write_all(&first).unwrap();

        let mut decoder = NdjsonDecoder::new();
        let resp = read_one_response(&mut stream, &mut decoder);
        assert!(!resp.ok);
        assert_eq!(
            resp.error.as_ref().unwrap().code,
            ErrorKind::MessageTooLarge,
            "报文体: {resp:?}"
        );

        // 连接必须被关闭（再读得到 EOF）。
        let mut buf = [0u8; 64];
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while Instant::now() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        assert!(closed, "超限后连接应被关闭");
        handle.shutdown();
    }

    /// 单向覆盖：同进程 `UnixStream::pair()` 只能走到"同 uid 放行"这一分支。
    /// 不同 uid 的分支由 [`different_uid_without_group_is_denied`] 用纯函数覆盖。
    #[test]
    fn peer_check_same_uid_passes_via_real_socketpair() {
        let policy = PeerPolicy::default();
        let (a, _b) = UnixStream::pair().unwrap();
        // 同一进程内 sockpair 的 peer 必然是自身 uid。
        assert!(peer_allowed(&a, &policy).is_ok());
        let open = PeerPolicy {
            require_same_uid: false,
            allow_groups: vec![],
        };
        assert!(peer_allowed(&a, &open).is_ok());
    }
}
