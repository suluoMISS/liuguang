//! IPC 客户端：连接 liuguang 控制 socket，发送请求并接收响应。
//!
//! 供 `liuguangctl` 与集成测试使用；阻塞式单连接，命令行场景足够。

use crate::framing::{NdjsonDecoder, encode_message};
use crate::protocol::{Method, PROTOCOL_VERSION, Request, Response};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// IPC 客户端错误。
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// 无法连接/读写 socket。
    #[error("无法连接 liuguang（{socket}）: {message}")]
    Connect {
        /// socket 路径。
        socket: String,
        /// 详细信息。
        message: String,
    },
    /// 传输/消息错误。
    #[error("IPC 传输错误: {0}")]
    Transport(String),
    /// 响应与请求不匹配。
    #[error("响应 ID 不匹配: 期望 {expected}，实际 {got}")]
    MismatchedResponse {
        /// 期望的请求 ID。
        expected: String,
        /// 实际响应 ID。
        got: String,
    },
    /// 服务端在**未关联请求 ID**的情况下给出的错误应答
    /// （例如并发超限：服务端在读请求之前就拒绝了连接）。
    ///
    /// 单独上报，避免被"响应 ID 不匹配"掩盖真实原因。
    #[error("服务端错误（未关联请求 ID）：{code}: {message}")]
    UnidentifiedError {
        /// 稳定错误码（字符串形式，直接来自线上报文）。
        code: String,
        /// 面向人的说明。
        message: String,
    },
    /// 协议版本不受支持。
    #[error("协议版本不受支持: {0}")]
    UnsupportedVersion(String),
}

/// 阻塞式 IPC 客户端。
pub struct IpcClient {
    stream: UnixStream,
    decoder: NdjsonDecoder,
    next_id: u64,
    socket_path: String,
}

impl IpcClient {
    /// 连接到控制 socket。
    ///
    /// # Errors
    ///
    /// 连接失败时返回 [`IpcError::Connect`]。
    pub fn connect(path: &Path) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(path).map_err(|e| IpcError::Connect {
            socket: path.display().to_string(),
            message: e.to_string(),
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| IpcError::Transport(e.to_string()))?;
        Ok(Self {
            stream,
            decoder: NdjsonDecoder::new(),
            next_id: 0,
            socket_path: path.display().to_string(),
        })
    }

    /// 发送一条请求并等待对应响应。
    ///
    /// 业务级失败（`ok=false`）以 [`Response`] 返回；传输/协议错误返回 [`IpcError`]。
    ///
    /// # Errors
    ///
    /// 读写失败、消息非法或响应 ID 不匹配时返回 [`IpcError`]。
    pub fn request(
        &mut self,
        method: Method,
        params: serde_json::Value,
    ) -> Result<Response, IpcError> {
        self.next_id += 1;
        let id = format!("req-{}", self.next_id);
        let request = Request::new(id.clone(), method, params);
        self.send_request(&request)?;
        self.recv_response(&id)
    }

    fn send_request(&mut self, request: &Request) -> Result<(), IpcError> {
        let bytes = encode_message(request).map_err(|e| IpcError::Transport(e.to_string()))?;
        self.stream
            .write_all(&bytes)
            .and_then(|()| self.stream.flush())
            .map_err(|e| IpcError::Transport(e.to_string()))
    }

    fn recv_response(&mut self, expected_id: &str) -> Result<Response, IpcError> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(resp) = self
                .decoder
                .next_message::<Response>()
                .map_err(|e| IpcError::Transport(e.to_string()))?
            {
                if resp.v != PROTOCOL_VERSION {
                    return Err(IpcError::UnsupportedVersion(format!(
                        "响应 v{}（期望 v{PROTOCOL_VERSION}）",
                        resp.v
                    )));
                }
                if resp.id != expected_id {
                    // 服务端在读请求之前就应答（如并发超限）时 id 会为空；
                    // 此时"响应 ID 不匹配"会把真实原因完全掩盖（审计 §3.7）。
                    if resp.id.is_empty() {
                        if let Some(err) = resp.error {
                            return Err(IpcError::UnidentifiedError {
                                code: format!("{:?}", err.code),
                                message: err.message,
                            });
                        }
                    }
                    return Err(IpcError::MismatchedResponse {
                        expected: expected_id.to_string(),
                        got: resp.id,
                    });
                }
                return Ok(resp);
            }
            let n = self
                .stream
                .read(&mut buf)
                .map_err(|e| IpcError::Transport(format!("{}: {e}", self.socket_path)))?;
            if n == 0 {
                return Err(IpcError::Transport(format!(
                    "{}: liuguang 关闭了连接（无响应）",
                    self.socket_path
                )));
            }
            self.decoder
                .feed(&buf[..n])
                .map_err(|e| IpcError::Transport(e.to_string()))?;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::protocol::ErrorBody;
    use crate::server::{Dispatch, IpcServer, RequestHandler, ServerConfig};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn test_socket(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "liuguang-client-{name}-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        p
    }

    fn handler() -> impl RequestHandler {
        |req: Request| match req.method {
            Method::Status => Dispatch::Reply(Response::ok(
                req.id,
                serde_json::json!({"state": "StaticIdle"}),
            )),
            _ => Dispatch::Reply(Response::err(
                req.id,
                ErrorBody::new(crate::protocol::ErrorKind::Internal, "boom"),
            )),
        }
    }

    #[test]
    fn connect_missing_socket_reports_path() {
        let err = match IpcClient::connect(Path::new("/nonexistent/liuguang.sock")) {
            Err(e) => e,
            Ok(_) => panic!("连接不存在的 socket 应失败"),
        };
        match err {
            IpcError::Connect { socket, .. } => assert!(socket.contains("nonexistent")),
            other => panic!("意外错误: {other}"),
        }
    }

    #[test]
    fn request_ok_and_error_responses() {
        let path = test_socket("ok");
        let server = IpcServer::bind(ServerConfig {
            socket_path: path.clone(),
            ..ServerConfig::default()
        })
        .unwrap();
        let handle = server.spawn(handler());

        let mut client = IpcClient::connect(&path).unwrap();
        let ok = client
            .request(Method::Status, serde_json::json!({}))
            .unwrap();
        assert!(ok.ok);
        let err = client
            .request(Method::Reload, serde_json::json!({}))
            .unwrap();
        assert!(!err.ok);
        assert_eq!(err.error.unwrap().message, "boom");

        handle.shutdown();
    }

    #[test]
    fn sequential_requests_get_matching_ids() {
        let path = test_socket("ids");
        let server = IpcServer::bind(ServerConfig {
            socket_path: path.clone(),
            ..ServerConfig::default()
        })
        .unwrap();
        let handle = server.spawn(handler());

        let mut client = IpcClient::connect(&path).unwrap();
        for i in 1..=3 {
            let resp = client
                .request(Method::Status, serde_json::json!({}))
                .unwrap();
            assert_eq!(resp.id, format!("req-{i}"));
        }
        handle.shutdown();
    }
}
