//! `liuguang-core` — Liuguang 共享数据类型、状态机、配置与纯逻辑。
//!
//! 模块边界（见 `docs/ARCHITECTURE.md`）：
//! 本 crate **禁止**依赖 DRM/KMS、GBM、EGL、GStreamer 等系统/媒体库；
//! 它只提供可被 `liuguang-display`、`liuguang-render`、`liuguang-media`、
//! `liuguang-ipc`、`liuguang-tui` 共同引用的数据类型与纯逻辑。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod error;
pub mod fit;
pub mod library;
pub mod lru;
pub mod pathguard;
pub mod persona;
pub mod playlist;
pub mod scheduler;
pub mod state;
pub mod transform;
pub mod types;
