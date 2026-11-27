//! NDJSON framing：逐行 JSON，带包大小上限。
//!
//! - 编码：`serde_json` 紧凑序列化 + 结尾 `\n`。
//! - 解码：字节流可任意分片到达；解码器缓冲到换行为止，
//!   单行超过 [`MAX_MESSAGE_BYTES`] 立即报错并丢弃缓冲，防止内存耗尽。

use crate::protocol::MAX_MESSAGE_BYTES;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// framing / 消息级错误。
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// JSON 编解码失败。
    #[error("JSON 编解码失败: {0}")]
    Json(#[from] serde_json::Error),
    /// 单条消息超过上限。
    #[error("消息超过大小上限 {limit} 字节")]
    MessageTooLarge {
        /// 上限。
        limit: usize,
    },
}

/// 编码一条消息（含结尾换行）。
///
/// # Errors
///
/// 序列化失败或编码结果超过 [`MAX_MESSAGE_BYTES`] 时返回 [`FramingError`]。
pub fn encode_message<T: Serialize>(message: &T) -> Result<Vec<u8>, FramingError> {
    let mut buf = serde_json::to_vec(message)?;
    if buf.len() + 1 > MAX_MESSAGE_BYTES {
        return Err(FramingError::MessageTooLarge {
            limit: MAX_MESSAGE_BYTES,
        });
    }
    buf.push(b'\n');
    Ok(buf)
}

/// 从一段完整行字节解码一条消息（不含换行符）。
///
/// # Errors
///
/// JSON 非法或超过 [`MAX_MESSAGE_BYTES`] 时返回 [`FramingError`]。
pub fn decode_message<T: DeserializeOwned>(line: &[u8]) -> Result<T, FramingError> {
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(FramingError::MessageTooLarge {
            limit: MAX_MESSAGE_BYTES,
        });
    }
    Ok(serde_json::from_slice(line)?)
}

/// 有界 NDJSON 解码器：`feed` 任意分片，`next_message` 逐条取出。
#[derive(Debug, Default)]
pub struct NdjsonDecoder {
    buf: Vec<u8>,
}

impl NdjsonDecoder {
    /// 创建空解码器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节流。
    ///
    /// # Errors
    ///
    /// 未完成的单行缓冲超过 [`MAX_MESSAGE_BYTES`] 时返回
    /// [`FramingError::MessageTooLarge`] 并清空缓冲（流已不可信）。
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), FramingError> {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > MAX_MESSAGE_BYTES && !self.buf.contains(&b'\n') {
            self.buf.clear();
            return Err(FramingError::MessageTooLarge {
                limit: MAX_MESSAGE_BYTES,
            });
        }
        Ok(())
    }

    /// 取出下一条完整消息；无完整行时返回 `Ok(None)`。
    ///
    /// # Errors
    ///
    /// JSON 非法或单行超限时返回 [`FramingError`]；该行会被丢弃。
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FramingError> {
        let Some(pos) = self.buf.iter().position(|&b| b == b'\n') else {
            return Ok(None);
        };
        let line: Vec<u8> = self.buf.drain(..=pos).collect();
        let line = &line[..line.len() - 1];
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(FramingError::MessageTooLarge {
                limit: MAX_MESSAGE_BYTES,
            });
        }
        match decode_message::<T>(line) {
            Ok(msg) => Ok(Some(msg)),
            Err(e) => Err(e),
        }
    }

    /// 当前未完成缓冲的字节数（诊断用）。
    pub fn pending_bytes(&self) -> usize {
        self.buf.len()
    }

    /// 清空缓冲。
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Method, Request};

    fn req(id: &str) -> Request {
        Request::new(id, Method::Status, serde_json::json!({}))
    }

    #[test]
    fn encode_decode_round_trip() {
        let bytes = encode_message(&req("a")).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let back: Request = decode_message(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(back.id, "a");
    }

    #[test]
    fn decoder_handles_split_chunks() {
        let mut wire = encode_message(&req("one")).unwrap();
        wire.extend_from_slice(&encode_message(&req("two")).unwrap());

        let mut dec = NdjsonDecoder::new();
        for chunk in wire.chunks(3) {
            dec.feed(chunk).unwrap();
        }
        let first: Request = dec.next_message().unwrap().unwrap();
        let second: Request = dec.next_message().unwrap().unwrap();
        assert_eq!(first.id, "one");
        assert_eq!(second.id, "two");
        assert!(dec.next_message::<Request>().unwrap().is_none());
    }

    #[test]
    fn decoder_reports_pending_between_messages() {
        let mut dec = NdjsonDecoder::new();
        dec.feed(br#"{"partial": true"#).unwrap();
        assert!(dec.next_message::<serde_json::Value>().unwrap().is_none());
        assert!(dec.pending_bytes() > 0);
    }

    #[test]
    fn oversize_line_is_rejected_on_feed() {
        let mut dec = NdjsonDecoder::new();
        let big = vec![b'x'; MAX_MESSAGE_BYTES + 10];
        let err = dec.feed(&big).unwrap_err();
        assert!(matches!(
            err,
            FramingError::MessageTooLarge {
                limit: MAX_MESSAGE_BYTES
            }
        ));
        // 缓冲被清空，流可继续使用。
        assert_eq!(dec.pending_bytes(), 0);
    }

    #[test]
    fn oversize_complete_line_is_rejected() {
        let mut dec = NdjsonDecoder::new();
        let mut big = vec![b' '; MAX_MESSAGE_BYTES + 1];
        big.push(b'\n');
        // 分两次喂入，避免触发 feed 的未完成行上限。
        let (a, b) = big.split_at(MAX_MESSAGE_BYTES - 1);
        dec.feed(a).unwrap();
        dec.feed(b).unwrap();
        let err = dec.next_message::<serde_json::Value>().unwrap_err();
        assert!(matches!(err, FramingError::MessageTooLarge { .. }));
    }

    #[test]
    fn invalid_json_line_drops_only_that_line() {
        let mut dec = NdjsonDecoder::new();
        dec.feed(b"{not json}\n{\"v\":1,\"id\":\"ok\",\"method\":\"status\",\"params\":{}}\n")
            .unwrap();
        assert!(dec.next_message::<Request>().is_err());
        let next: Request = dec.next_message().unwrap().unwrap();
        assert_eq!(next.id, "ok");
    }

    #[test]
    fn encode_rejects_oversize_message() {
        let huge = Request::new(
            "x",
            Method::SetWallpaper,
            serde_json::json!({"blob": "y".repeat(MAX_MESSAGE_BYTES)}),
        );
        assert!(matches!(
            encode_message(&huge),
            Err(FramingError::MessageTooLarge { .. })
        ));
    }
}
