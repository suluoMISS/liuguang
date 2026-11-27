//! `liuguang-ipc` — liuguang 与 liuguangctl 之间的 IPC 协议 v1。
//!
//! 传输为本地 Unix Domain Socket（`/run/liuguang/control.sock`，可配置），
//! 消息为 NDJSON（newline-delimited JSON），带包大小上限与版本号。
//! 本 crate 提供：消息类型（[`protocol`]）、framing（[`framing`]）、
//! 服务端（[`server`]，含 peer credential 校验、并发上限与握手超时）与
//! 客户端（[`client`]）。订阅事件流随 M3 交付；实际授权模型见 [`server`] 模块文档。
//!
//! 职责边界（规范 §3.3）：服务端 handler 只做路由，不在 socket 工作线程
//! 内执行阻塞解码。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod base64;
pub mod client;
pub mod framing;
pub mod protocol;
pub mod server;

pub use client::{IpcClient, IpcError as ClientError};
pub use framing::{NdjsonDecoder, decode_message, encode_message};
pub use protocol::{
    AcceptedResult, ChannelStatus, ErrorBody, ErrorKind, EventKind, EventMessage,
    MAX_MESSAGE_BYTES, Method, OutputParams, OutputStatus, OutputsResult, PROTOCOL_VERSION,
    Request, Response, SetStaticWallpaperParams, SetWallpaperParams, StatusResult,
    TransitionParams, VideoStatsStatus, unix_ms,
};
pub use server::{
    DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_MAX_CONNECTIONS, Dispatch, IpcError, IpcServer, PeerId,
    PeerPolicy, RequestHandler, ServerConfig, ServerHandle, peer_decision, reply_only,
};
