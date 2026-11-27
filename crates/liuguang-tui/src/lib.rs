//! `liuguang-tui` — 远程终端管理界面（ratatui + crossterm）。
//!
//! **硬边界（规范 §3.3 / §9.3 / §11）**：
//! TUI **只经 IPC 与 liuguang 交互**，绝不打开 `/dev/dri`、不解码/播放视频、
//! **不扫描本地目录**（`std::fs` 媒体扫描已移除：路径校验必须由 daemon 执行，
//! TUI 的本地校验不能作为安全边界）。TUI 退出后显示不受影响，也**不会**停掉 daemon。
//!
//! 模块划分（`docs/TUI_DESIGN.md` §5）：
//! - [`model`]：AppState 与纯逻辑（分页/焦点/编辑器/未保存语义）；
//! - [`event`]：按键 → Action → 状态变更 → Effect（纯逻辑，可单测）；
//! - [`ipc_client`]：`ControlClient` 抽象 + `IpcControl`（真实）+ `MockControl`（模拟）；
//! - [`ui`]：`layout`（四档自适应）+ `pages/*`（六页面）+ `widgets/*`（部件）；
//! - [`app`]：终端生命周期 + **事件驱动**事件循环 + IPC 副作用执行。
//!
//! 数据、事件、布局、渲染四层分离：前两层完全不依赖终端，可无 DRM 单测。

pub mod app;
pub mod event;
pub mod ipc_client;
pub mod model;
pub mod ui;

pub use app::{App, TerminalGuard, install_panic_hook, run, run_mock, run_with_client};
pub use event::{Action, EditOp, Effect, action_for, apply};
pub use ipc_client::{
    ControlClient, EventSource, IpcControl, MediaQuery, MockCall, MockControl, MockEventSource,
    MockScenario, RealEventSource, Session, SessionInfo, SetWallpaperAck, StaticCallError,
};
pub use model::{
    APPLY_CONFIRM_TIMEOUT, AppState, ApplyPhase, ApplyTarget, CommitKind, Connection, Focus,
    MediaFilter, MediaItem, MediaSort, Modal, Notification, NotificationKind, OutputTransform,
    Page, ScanState, SubPage, TextTarget, TransitionDraft, UiStatus,
};

use std::path::PathBuf;

/// TUI 启动选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiOptions {
    /// liuguang 控制 socket 路径。
    pub socket_path: PathBuf,
    /// 媒体目录提示（**仅用于在界面上显示**；TUI 不做目录扫描）。
    pub media_dir: PathBuf,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/liuguang/control.sock"),
            media_dir: PathBuf::from("/srv/wallpapers"),
        }
    }
}

/// TUI 错误。
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// IPC 连接失败。
    #[error("无法连接 liuguang（{socket}）: {message}")]
    Connect {
        /// socket 路径。
        socket: String,
        /// 详细信息。
        message: String,
    },
    /// IPC 传输/协议失败（连接建立之后）。
    #[error("IPC 错误: {0}")]
    Ipc(String),
    /// 终端 I/O 失败。
    #[error("终端 I/O 失败: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_is_spec_path() {
        assert_eq!(
            TuiOptions::default().socket_path,
            PathBuf::from("/run/liuguang/control.sock")
        );
    }

    #[test]
    fn default_media_dir_is_spec_dir() {
        assert_eq!(
            TuiOptions::default().media_dir,
            PathBuf::from("/srv/wallpapers")
        );
    }

    #[test]
    fn public_api_is_exported() {
        // 这些名字是 liuguangctl / 集成测试依赖的公开契约
        let _ = std::mem::size_of::<AppState>();
        let _ = std::mem::size_of::<MockControl>();
        let _title: fn(Page) -> &'static str = Page::title;
        let _hint: String = Focus::List.label().to_string();
        let _ = std::mem::size_of::<TuiError>();
    }
}
