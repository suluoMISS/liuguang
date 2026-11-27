//! 状态模型：数据 + 纯逻辑（不做 I/O、不渲染、不碰终端）。
//!
//! 数据形状**直接复用 daemon 的线上类型**（`liuguang_ipc::protocol::*`），
//! 避免 TUI 侧再定义一套"看起来一样"的类型而产生第二事实源。

pub use liuguang_ipc::protocol::MediaItem;
use liuguang_ipc::protocol::{
    CompatInfo, ListMediaResult, ListSourcesResult, MetricsResult, OutputsResult,
    SetOutputTransformParams, StatusResult,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// 媒体列表分页大小（服务端上限 [`MAX_MEDIA_PAGE_SIZE`]）。
pub const MEDIA_PAGE_SIZE: usize = 50;
/// 服务端单页上限（与 `liuguang_ipc::protocol::MAX_MEDIA_PAGE_SIZE` 一致）。
pub const MAX_MEDIA_PAGE_SIZE: usize = 200;
/// 搜索防抖窗口（规范 §8）。
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// 状态栏指标刷新上限：2 Hz（规范 §8）。
pub const METRICS_INTERVAL: Duration = Duration::from_millis(500);
/// `WALLPAPER_APPLIED` 未到达时的"仍在等待确认"超时（**不得**据此假称已应用）。
pub const APPLY_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);
/// 事件循环最长阻塞时间（**不是**忙循环）。
pub const POLL_TIMEOUT: Duration = Duration::from_millis(250);
/// 状态栏最多保留的通知条数。
pub const MAX_NOTIFICATIONS: usize = 20;
/// 系统页最多展示的最近错误条数。
pub const MAX_RECENT_ERRORS: usize = 5;

/// 六个页面。
/// 播放列表编辑缓冲（t24）。
///
/// **只改内存**：`Apply` 把 `to_params()` 下发给 daemon（`update_playlist`，只改运行时），
/// `Save` 才走 `save_config` 落盘；**放弃编辑不产生任何调用**（有测试证明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEdit {
    /// `None` = 新建（由 daemon 生成 id）。
    pub id: Option<String>,
    /// 名称（可编辑文本）。
    pub name: String,
    /// `sequential` / `random`。
    pub mode: String,
    /// 轮换间隔（秒）。
    pub interval_seconds: u32,
    /// `next` / `loop`。
    pub on_video_end: String,
    /// 目标输出（`None` = 默认调度）。
    pub output: Option<String>,
    /// 是否禁止连续重复。
    pub no_repeat: bool,
    /// 条目（媒体 id 顺序即播放顺序）。
    pub entries: Vec<String>,
    /// 条目光标。
    pub cursor: usize,
}

impl PlaylistEdit {
    /// 由已有播放列表进入编辑。
    #[must_use]
    pub fn from_playlist(p: &liuguang_ipc::protocol::Playlist) -> Self {
        Self {
            id: Some(p.id.clone()),
            name: p.name.clone(),
            mode: p.mode.clone(),
            interval_seconds: p.interval_seconds,
            on_video_end: p.on_video_end.clone(),
            output: p.output.clone(),
            no_repeat: p.no_repeat,
            entries: p.entries.iter().map(|e| e.media_id.clone()).collect(),
            cursor: 0,
        }
    }

    /// 新建的默认值（与服务端默认一致：顺序、600s、播完切下一个）。
    #[must_use]
    pub fn new_default() -> Self {
        Self {
            id: None,
            name: "新播放列表".to_string(),
            mode: "sequential".to_string(),
            interval_seconds: 600,
            on_video_end: "next".to_string(),
            output: None,
            no_repeat: true,
            entries: Vec::new(),
            cursor: 0,
        }
    }

    /// 生成 `update_playlist` 参数（**唯一**的线上下发点）。
    #[must_use]
    pub fn to_params(&self) -> liuguang_ipc::protocol::UpdatePlaylistParams {
        liuguang_ipc::protocol::UpdatePlaylistParams {
            playlist_id: self.id.clone(),
            name: self.name.clone(),
            entries: self
                .entries
                .iter()
                .map(|id| liuguang_ipc::protocol::PlaylistEntry {
                    media_id: id.clone(),
                })
                .collect(),
            mode: self.mode.clone(),
            interval_seconds: self.interval_seconds,
            on_video_end: self.on_video_end.clone(),
            output: self.output.clone(),
            no_repeat: self.no_repeat,
        }
    }
}

/// 播放模式（**三选一**）—— 把"两个协议字段的组合"收敛成一个用户概念（t67）。
///
/// 治的病：以前表达"顺序循环"要按 `m`（切顺序）**再**按 `v`（切 next），
/// 三种模式全靠两个键的组合，而且用户得先知道这两个字段的耦合关系。
/// 现在选择器（←/→）**一次**写好两个字段（见 [`PlayMode::fields`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayMode {
    /// 单视频循环：`on_video_end = "loop"`（同一个视频反复播）。
    SingleLoop,
    /// 顺序循环：`mode = "sequential"` + `on_video_end = "next"`。
    SequentialLoop,
    /// 随机播放：`mode = "random"` + `on_video_end = "next"`。
    Shuffle,
}

impl PlayMode {
    /// 选择器里的显示顺序（也是 ←/→ 的循环顺序）。
    pub const ALL: [Self; 3] = [Self::SingleLoop, Self::SequentialLoop, Self::Shuffle];

    /// 给用户看的名字。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SingleLoop => "单视频循环",
            Self::SequentialLoop => "顺序循环",
            Self::Shuffle => "随机播放",
        }
    }

    /// **唯一**的协议字段映射点：`(mode, on_video_end)`。
    ///
    /// 测试直接断言这张表与"实际下发的两个字段"一致 —— 只改一半会被抓出来。
    #[must_use]
    pub const fn fields(self) -> (&'static str, &'static str) {
        match self {
            Self::SingleLoop => ("sequential", "loop"),
            Self::SequentialLoop => ("sequential", "next"),
            Self::Shuffle => ("random", "next"),
        }
    }

    /// 由协议字段**反推**当前模式（未知组合按"顺序循环"显示，绝不 panic）。
    #[must_use]
    pub fn from_fields(mode: &str, on_video_end: &str) -> Self {
        if on_video_end == "loop" {
            Self::SingleLoop
        } else if mode == "random" {
            Self::Shuffle
        } else {
            Self::SequentialLoop
        }
    }

    /// 在选择器里的下标（高亮用）。
    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(1)
    }

    /// ←/→：循环切到上/下一个模式。
    #[must_use]
    pub fn shift(self, forward: bool) -> Self {
        let n = Self::ALL.len() as i32;
        let i = self.index() as i32;
        let step = if forward { 1 } else { -1 };
        Self::ALL[(i + step).rem_euclid(n) as usize]
    }
}

/// 由一条播放列表 + 指定字段拼出 `update_playlist` 参数（**唯一**拼装点）。
///
/// 模式选择器与"媒体库页 `a` 加入"都走这里：保证**整条列表**被完整下发
/// （只改一个字段会把其它字段清空——daemon 的 `update_playlist` 是整体替换）。
fn playlist_params(
    p: &liuguang_ipc::protocol::Playlist,
    mode: &str,
    on_video_end: &str,
    entries: Vec<String>,
) -> liuguang_ipc::protocol::UpdatePlaylistParams {
    liuguang_ipc::protocol::UpdatePlaylistParams {
        playlist_id: Some(p.id.clone()),
        name: p.name.clone(),
        entries: entries
            .into_iter()
            .map(|media_id| liuguang_ipc::protocol::PlaylistEntry { media_id })
            .collect(),
        mode: mode.to_string(),
        interval_seconds: p.interval_seconds,
        on_video_end: on_video_end.to_string(),
        output: p.output.clone(),
        no_repeat: p.no_repeat,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Page {
    /// 总览 Dashboard。
    Dashboard,
    /// 媒体库 Library。
    Library,
    /// 播放列表 Playlists。
    Playlists,
    /// 显示器 Outputs。
    Outputs,
    /// 转场 Effects。
    Effects,
    /// 系统 System。
    System,
    /// **静态壁纸**（t70，第 7 页）：单独设置静态图并显示当前播放源。
    Static,
}

impl Page {
    /// 全部页面（顺序 = `1`–`7` 热键顺序）。
    pub const ALL: [Self; 7] = [
        Self::Dashboard,
        Self::Library,
        Self::Playlists,
        Self::Outputs,
        Self::Effects,
        Self::System,
        Self::Static,
    ];

    /// 页面标题。
    pub const fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "总览",
            Self::Library => "媒体库",
            Self::Playlists => "播放列表",
            Self::Outputs => "显示器",
            Self::Effects => "转场",
            Self::System => "系统",
            Self::Static => "静态壁纸",
        }
    }

    /// 热键字符。
    pub const fn hotkey(self) -> char {
        match self {
            Self::Dashboard => '1',
            Self::Library => '2',
            Self::Playlists => '3',
            Self::Outputs => '4',
            Self::Effects => '5',
            Self::System => '6',
            Self::Static => '7',
        }
    }

    /// 在 [`Page::ALL`] 中的下标。
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }

    /// 由热键取页面。
    pub fn from_hotkey(c: char) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.hotkey() == c)
    }
}

/// 焦点区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Focus {
    /// 导航。
    Nav,
    /// 列表。
    List,
    /// 详情。
    Detail,
    /// 操作。
    Actions,
}

impl Focus {
    /// 焦点名。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Nav => "导航",
            Self::List => "列表",
            Self::Detail => "详情",
            Self::Actions => "操作",
        }
    }

    /// 焦点循环。
    pub const fn next(self, forward: bool) -> Self {
        const ORDER: [Focus; 4] = [Focus::Nav, Focus::List, Focus::Detail, Focus::Actions];
        let i = match self {
            Focus::Nav => 0,
            Self::List => 1,
            Self::Detail => 2,
            Self::Actions => 3,
        };
        ORDER[if forward { (i + 1) % 4 } else { (i + 3) % 4 }]
    }
}

/// 小档单栏子页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubPage {
    /// 列表。
    List,
    /// 详情。
    Detail,
    /// 操作。
    Actions,
}

impl SubPage {
    /// 子页名。
    pub const fn label(self) -> &'static str {
        match self {
            Self::List => "列表",
            Self::Detail => "详情",
            Self::Actions => "操作",
        }
    }

    /// 左右键切换。
    pub const fn shift(self, forward: bool) -> Self {
        const ORDER: [SubPage; 3] = [SubPage::List, SubPage::Detail, SubPage::Actions];
        let i = match self {
            Self::List => 0,
            Self::Detail => 1,
            Self::Actions => 2,
        };
        ORDER[if forward { (i + 1) % 3 } else { (i + 2) % 3 }]
    }
}

/// 连接状态（断线态必须显式可见）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// 已连接。
    Online,
    /// 断开（携带原因）。
    Offline(String),
    /// 重连中。
    Reconnecting,
}

impl Connection {
    /// 展示文本。
    pub fn label(&self) -> String {
        match self {
            Self::Online => "在线".to_string(),
            Self::Offline(reason) => format!("断线（{reason}）"),
            Self::Reconnecting => "重连中".to_string(),
        }
    }

    /// 是否可用。
    pub const fn is_online(&self) -> bool {
        matches!(self, Self::Online)
    }
}

/// 媒体筛选（**下推给 daemon**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaFilter {
    /// 全部。
    #[default]
    All,
    /// 仅图片。
    Image,
    /// 仅视频。
    Video,
}

impl MediaFilter {
    /// 线上值：**「全部」必须是 `None`（不筛选）**。
    ///
    /// T39-F1：这里曾返回 `Some("all")`，而 daemon 对 `kind` 做精确比较 ⇒
    /// "all" 匹配不到任何条目 ⇒ 默认筛选下 `total=0`（`ok=true`，静默空结果）。
    /// 不发无效参数，是"一个无效值被当成合法空结果"这类缺陷的根治办法。
    pub const fn wire(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Image => Some("image"),
            Self::Video => Some("video"),
        }
    }

    /// 循环切换。
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Image,
            Self::Image => Self::Video,
            Self::Video => Self::All,
        }
    }

    /// 展示名。
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Image => "图片",
            Self::Video => "视频",
        }
    }
}

/// 媒体排序（**下推给 daemon**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaSort {
    /// 按名称。
    #[default]
    Name,
    /// 按大小。
    Size,
    /// 按修改时间。
    Modified,
}

impl MediaSort {
    /// 线上值。
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Modified => "modified",
        }
    }

    /// 循环切换。
    pub const fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Modified,
            Self::Modified => Self::Name,
        }
    }

    /// 展示名。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Size => "大小",
            Self::Modified => "修改时间",
        }
    }
}

/// 该媒体项是不是视频（线上用字符串 `kind`）。
pub fn is_video(item: &MediaItem) -> bool {
    item.kind.eq_ignore_ascii_case("video")
}

/// 人类可读大小。
pub fn size_label(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// 整数帧率展示（与 GUI 一致：`60 fps` 而不是 `59.94 fps`）。
#[must_use]
pub fn format_fps_int(fps: f64) -> String {
    if !fps.is_finite() || fps <= 0.0 {
        "—".to_string()
    } else {
        format!("{} fps", fps.round() as u32)
    }
}

/// daemon 暂停原因码 → 中文标签（与 GUI 一致）。
#[must_use]
pub fn pause_reason_label(reason: &str) -> &'static str {
    match reason {
        "fullscreen" => "全屏让路",
        "game_running" | "game" => "游戏让路",
        "gpu_memory" => "显存让路",
        "battery" => "电池省电",
        "screen_off" | "display_off" => "熄屏暂停",
        "user" => "手动暂停",
        _ => "已暂停",
    }
}

/// 总览页的状态分类（**按 daemon 的实际状态/暂停位推导**，不猜配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiStatus {
    /// 正在播放视频。
    Playing,
    /// 已暂停。
    Paused,
    /// 正在加载/切换（转场、初始化）。
    Loading,
    /// 出错/降级。
    Error,
    /// 无输出（Headless）。
    HeadlessReady,
    /// 静态壁纸。
    Static,
    /// 其它（原样展示 daemon 的值）。
    Unknown,
}

impl UiStatus {
    /// 展示名。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Loading => "Loading",
            Self::Error => "Error",
            Self::HeadlessReady => "HeadlessReady",
            Self::Static => "Static",
            Self::Unknown => "Unknown",
        }
    }

    /// 由 daemon 的 `state`/`paused` 推导（**唯一**分类入口）。
    pub fn from_daemon(state: &str, paused: bool) -> Self {
        if paused {
            return Self::Paused;
        }
        match state {
            "VideoPlaying" => Self::Playing,
            "Paused" => Self::Paused,
            "StaticIdle" | "StaticShown" => Self::Static,
            "InitializingDisplay" | "Transitioning" | "PreparingNext" | "SwitchRequested" => {
                Self::Loading
            }
            "DegradedStatic" | "Error" => Self::Error,
            "HeadlessReady" | "NoOutput" => Self::HeadlessReady,
            _ => Self::Unknown,
        }
    }
}

/// 扫描状态（daemon 侧异步扫描）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ScanState {
    /// 空闲。
    #[default]
    Idle,
    /// 已受理（等事件）。
    Requested {
        /// `scan_id`。
        scan_id: u64,
    },
    /// 进行中（收到 `SCAN_PROGRESS`）。
    Running {
        /// `scan_id`。
        scan_id: u64,
    },
    /// 完成。
    Finished {
        /// `scan_id`。
        scan_id: u64,
        /// 发现条目数。
        found: usize,
    },
    /// 失败（携带原因）。
    Failed(String),
}

impl ScanState {
    /// 展示文本。
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "空闲".to_string(),
            Self::Requested { scan_id } => format!("已受理（scan {scan_id}）"),
            Self::Running { scan_id } => format!("扫描中（scan {scan_id}）"),
            Self::Finished { scan_id, found } => format!("完成（scan {scan_id}，{found} 项）"),
            Self::Failed(reason) => format!("失败（{reason}）"),
        }
    }
}

/// 应用目标（**不得偷偷应用到所有屏幕**）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ApplyTarget {
    /// daemon 的默认输出。
    #[default]
    Default,
    /// 指定输出。
    Output(String),
    /// 显式的"全部显示器"（批量下发，逐屏反馈）。
    AllOutputs,
}

impl ApplyTarget {
    /// 展示文本。
    pub fn label(&self) -> String {
        match self {
            Self::Default => "默认输出".to_string(),
            Self::Output(name) => name.clone(),
            Self::AllOutputs => "全部显示器".to_string(),
        }
    }
}

/// 模态框。
#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    /// 服务器绝对路径输入。
    PathInput {
        /// 用途说明。
        purpose: String,
    },
    /// 来源管理。
    SourceManage,
    /// 帮助。
    Help,
    /// **可选中文本**（OSC52 复制失败时的回退，规范要求不得假设复制成功）。
    PathText {
        /// 标题。
        title: String,
        /// 完整文本（不截断）。
        text: String,
    },
    /// **文本输入**（播放列表重命名 / 新增来源路径）。
    TextInput {
        /// 标题。
        title: String,
        /// 目标（决定提交后写到哪）。
        target: TextTarget,
        /// 提示行。
        hint: String,
    },
    /// 确认。
    Confirm {
        /// 问题文本。
        question: String,
        /// 待确认动作标识。
        action: String,
    },
}

impl Modal {
    /// 标题。
    pub fn title(&self) -> &str {
        match self {
            Self::PathInput { .. } => "输入服务器绝对路径",
            Self::SourceManage => "来源管理",
            Self::Help => "帮助",
            Self::PathText { title, .. } => title,
            Self::TextInput { title, .. } => title,
            Self::Confirm { .. } => "确认",
        }
    }
}

/// 文本输入的目标（提交后写到哪）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTarget {
    /// 播放列表名称（写到 `playlist_edit.name`）。
    PlaylistName,
    /// 新增来源的服务器绝对路径（下发给 daemon 校验）。
    SourceNewPath,
}

/// 通知级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    /// 信息。
    Info,
    /// 警告。
    Warn,
    /// 错误。
    Error,
}

/// 一条通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// 级别。
    pub kind: NotificationKind,
    /// 文本。
    pub text: String,
}

/// 一次"应用壁纸"的全生命周期（`accepted` 只是受理）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyPhase {
    /// 已发出，等应答。
    Sending,
    /// daemon 已受理（`accepted=true`）——**仍不是已上屏**。
    Accepted,
    /// `WALLPAPER_APPLIED{ok=true}`：真的应用完成。
    Applied,
    /// 失败（携带可读原因）。
    Failed(String),
}

impl ApplyPhase {
    /// 展示文本（**不得**把受理写成"已应用"）。
    pub fn label(&self) -> String {
        match self {
            Self::Sending => "Sending…（已发出，等待应答）".to_string(),
            Self::Accepted => "Accepted…（已受理，等待 daemon 确认上屏）".to_string(),
            Self::Applied => "Applied（daemon 已确认）".to_string(),
            Self::Failed(reason) => format!("失败：{reason}"),
        }
    }

    /// 是否已确认上屏。
    pub const fn is_applied(&self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// 进行中的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    /// 请求用途。
    pub what: String,
    /// 状态。
    pub state: PendingState,
}

/// 请求状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingState {
    /// 已发出。
    Sending,
    /// 已受理。
    Accepted,
    /// 已确认生效。
    Applied,
    /// 失败。
    Failed(String),
}

impl PendingState {
    /// 用户可读的状态名（顶栏/通知用；**不得**在 UI 里 `{:?}` Debug 输出）。
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Sending => "发送中".to_string(),
            Self::Accepted => "已受理".to_string(),
            Self::Applied => "已生效".to_string(),
            Self::Failed(reason) => format!("失败：{reason}"),
        }
    }
}

/// `Apply`/`Save` 的语义区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    /// 立即应用（只改运行时）。
    Apply,
    /// 持久保存。
    Save,
}

/// Unicode 安全的单行编辑器（按 `char` 存储）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEditor {
    chars: Vec<char>,
    cursor: usize,
}

impl LineEditor {
    /// 空编辑器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 以初始文本构造（光标在末尾）。
    pub fn with_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    /// 光标位置（以 `char` 计）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 字符数。
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// 文本。
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// 设置文本并把光标放到末尾。
    pub fn set_text(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    /// 在光标处插入（一个 `char` 为最小单位）。
    pub fn insert(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let at = self.cursor.min(self.chars.len());
        self.chars.insert(at, c);
        self.cursor = at + 1;
    }

    /// 退格（删除光标前一个**字符**）。
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.cursor.min(self.chars.len());
        self.chars.remove(at - 1);
        self.cursor = at - 1;
    }

    /// 删除光标处字符。
    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// 光标左移。
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// 光标右移。
    pub fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    /// 行首。
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// 行尾。
    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// 光标前文本。
    pub fn before(&self) -> String {
        self.chars[..self.cursor.min(self.chars.len())]
            .iter()
            .collect()
    }

    /// 光标处字符。
    pub fn at_cursor(&self) -> Option<char> {
        self.chars.get(self.cursor).copied()
    }

    /// 光标后文本。
    pub fn after(&self) -> String {
        let at = self.cursor.min(self.chars.len());
        self.chars[at..].iter().collect()
    }
}

/// 应用状态（唯一数据权威）。
#[derive(Debug, Clone)]
pub struct AppState {
    /// 当前页。
    pub page: Page,
    /// 焦点。
    pub focus: Focus,
    /// 小档子页。
    pub subpage: SubPage,
    /// 连接状态。
    pub connection: Connection,
    /// daemon 回传的**实际配置路径**（来自 `SaveConfigResult.path`；未保存过时为 `None`，
    /// 不得写死规范路径）。
    pub config_path: Option<String>,
    /// 会话描述（`ipc`/`mock` + socket；由 app 在构造时填充）。
    pub session_kind: String,
    /// socket 路径（仅展示）。
    pub socket_path: String,
    /// 重连尝试次数（`Re-connect` 可见）。
    pub reconnect_attempts: u32,
    /// 最近一次 `status`。
    pub status: StatusResult,
    /// 最近一次 `outputs`。
    pub outputs: OutputsResult,
    /// 输出光标。
    pub outputs_cursor: usize,
    /// 选中的输出。
    pub selected_output: Option<String>,
    /// 应用目标（默认/指定/全部）。
    pub apply_target: ApplyTarget,
    /// 媒体当前页（**线上类型**，含 `total`/`page`/`truncated`/`scanning`）。
    pub media: ListMediaResult,
    /// 媒体光标。
    pub media_cursor: usize,
    /// 筛选/排序/搜索/来源（全部下推）。
    pub filter: MediaFilter,
    /// 排序。
    pub sort: MediaSort,
    /// 搜索词。
    pub search: String,
    /// 搜索框是否聚焦。
    pub search_focus: bool,
    /// 搜索防抖时刻。
    pub search_dirty_at: Option<Instant>,
    /// 来源。
    pub sources: ListSourcesResult,
    /// 来源光标。
    pub sources_cursor: usize,
    /// 扫描状态。
    pub scan: ScanState,
    /// 选中项的**完整元数据**（仅选中时经 `get_media` 取）。
    pub detail: Option<MediaItem>,
    /// 选中项的兼容性结论（来自 `get_media` 的 `compat`）。
    pub detail_compat: Option<CompatInfo>,
    /// 取详情失败原因。
    pub detail_error: Option<String>,
    /// 指标（System 页，≤2 Hz）。
    pub metrics: MetricsResult,
    /// 输出变换草稿。
    pub transform: OutputTransform,
    /// 草稿是否有未保存改动（顶栏 `*`）。
    pub dirty: bool,
    /// 播放列表编辑缓冲（t24）：`Some` = 正在编辑（顶栏显示 `*`）。
    pub playlist_edit: Option<PlaylistEdit>,
    /// **当前静态壁纸路径**（t70）：daemon 报告的值，或本轮设置成功后的值。
    pub static_wallpaper: Option<String>,
    /// daemon 报告的**播放源**（`static` / `playlist:<id>` / `directory`）。
    ///
    /// `None` = 这台 daemon 还没报告（旧版本）⇒ 页面会显示**推断值**并标注「推断」，
    /// 绝不假装知道。
    pub playback_source: Option<String>,
    /// daemon 是否支持静态壁纸接口：`None` = 未知，`Some(false)` = 明确回 `UNSUPPORTED_METHOD`。
    pub static_supported: Option<bool>,
    /// 播放列表（t22 `list_playlists` 的真实数据）。
    pub playlists: Vec<liuguang_ipc::protocol::Playlist>,
    /// 播放列表光标。
    pub playlists_cursor: usize,
    /// `[S]` 来源管理：新来源类型（`image` / `video` / `all`）。
    pub source_kind: String,
    /// `[S]` 来源管理：新增来源是否递归（下推给 daemon）。
    pub source_recursive: bool,
    /// `[S]` 来源管理的运行时改动是否尚未持久化（需显式保存）。
    pub sources_dirty: bool,
    /// 转场草稿。
    pub transition: TransitionDraft,
    /// 模态框。
    pub modal: Option<Modal>,
    /// 通知。
    pub notifications: Vec<Notification>,
    /// 最近错误（系统页展示；有界）。
    pub recent_errors: Vec<String>,
    /// 进行中的请求。
    pub pending_ops: BTreeMap<String, PendingOp>,
    /// 应用壁纸的生命周期（**受理 ≠ 已应用**）。
    pub apply_phase: Option<ApplyPhase>,
    /// 发起应用时刻（用于"等待确认"超时提示）。
    pub apply_started_at: Option<Instant>,
    /// 路径输入编辑器。
    pub editor: LineEditor,
    /// 打开模态框前的焦点。
    pub focus_before_modal: Option<Focus>,
    /// 媒体目录提示（**仅展示**）。
    pub media_dir_hint: String,
    /// 终端尺寸（`Resize` 更新）。
    pub size: (u16, u16),
    /// 是否请求退出。
    pub quit: bool,
}

/// 输出变换草稿（`Apply` 只改运行时；`Save` 才落盘）。
#[derive(Debug, Clone, PartialEq)]
pub struct OutputTransform {
    /// 目标输出。
    pub output: String,
    /// Cover / Contain / Stretch / Center。
    pub fit: String,
    /// 缩放（**daemon 单位**，1.0 = 无缩放）。
    pub zoom: f32,
    /// 水平偏移（daemon 单位）。
    pub offset_x: f32,
    /// 垂直偏移（daemon 单位）。
    pub offset_y: f32,
    /// 旋转角（度）。
    pub rotation: u32,
    /// 画面亮度（`0.1..=1.0`，默认 `1.0` = 100%）。
    pub brightness: f32,
}

impl Default for OutputTransform {
    fn default() -> Self {
        Self {
            output: String::new(),
            fit: "cover".to_string(),
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            rotation: 0,
            brightness: 1.0,
        }
    }
}

/// 支持的 fit 模式（与 GUI 一致）。
pub const FIT_MODES: [&str; 4] = ["cover", "contain", "stretch", "center"];

/// fit 模式的中文名称。
#[must_use]
pub fn fit_label(fit: &str) -> &'static str {
    match fit {
        "cover" => "裁切填充",
        "contain" => "完整显示",
        "stretch" => "拉伸铺满",
        "center" => "居中原始",
        _ => "未知",
    }
}

impl OutputTransform {
    /// 转成线上参数（**沿用 daemon 单位，不引入第二套**）。
    pub fn to_params(&self, output: Option<String>) -> SetOutputTransformParams {
        SetOutputTransformParams {
            output,
            fit: Some(self.fit.clone()),
            zoom: Some(self.zoom),
            offset_x: Some(self.offset_x),
            offset_y: Some(self.offset_y),
            rotation: Some(self.rotation),
            brightness: Some(self.brightness),
        }
    }

    /// 循环切换 fit。
    pub fn cycle_fit(&mut self, forward: bool) {
        let i = FIT_MODES.iter().position(|f| *f == self.fit).unwrap_or(0);
        let n = FIT_MODES.len();
        self.fit = FIT_MODES[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
        .to_string();
    }

    /// 调整 zoom（钳制在 daemon 接受的范围）。
    pub fn adjust_zoom(&mut self, delta: f32) {
        self.zoom = (self.zoom + delta).clamp(0.1, 10.0);
    }

    /// 调整偏移。
    pub fn adjust_offset_x(&mut self, delta: f32) {
        self.offset_x = (self.offset_x + delta).clamp(-4096.0, 4096.0);
    }

    /// 调整纵向偏移。
    pub fn adjust_offset_y(&mut self, delta: f32) {
        self.offset_y = (self.offset_y + delta).clamp(-4096.0, 4096.0);
    }

    /// 循环旋转角。
    pub fn cycle_rotation(&mut self) {
        self.rotation = (self.rotation + 90) % 360;
    }

    /// 调整画面亮度（`0.10..=1.00`，步进干净四舍五入）。
    pub fn adjust_brightness(&mut self, delta: f32) {
        let next = (self.brightness + delta).clamp(0.1, 1.0);
        self.brightness = (next * 100.0).round() / 100.0;
    }

    /// 亮度百分比（10..=100）。
    #[must_use]
    pub fn brightness_percent(&self) -> u8 {
        (self.brightness.clamp(0.1, 1.0) * 100.0).round() as u8
    }

    /// 重置适配参数与亮度回默认值（保留已选输出名）。
    pub fn reset_defaults(&mut self) {
        self.fit = "cover".to_string();
        self.zoom = 1.0;
        self.offset_x = 0.0;
        self.offset_y = 0.0;
        self.rotation = 0;
        self.brightness = 1.0;
    }
}

/// 转场草稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionDraft {
    /// 效果。
    pub effect: String,
    /// 时长（毫秒）。
    pub duration_ms: u32,
    /// 缓动。
    pub easing: String,
    /// 方向。
    pub direction: Option<String>,
}

impl Default for TransitionDraft {
    fn default() -> Self {
        Self {
            effect: "fade".to_string(),
            duration_ms: 350,
            easing: "ease-in-out".to_string(),
            direction: None,
        }
    }
}

/// 可选效果。
pub const EFFECTS: [&str; 6] = ["fade", "slide", "wipe", "circle", "random", "none"];
/// **真正被 daemon/renderer 实现**的效果（事实来源：`apps/liuguang` 的效果名映射
/// `fade|slide|wipe|circle`，其它名字会被**回退成 fade**；`crates/liuguang-render`
/// 实现 Fade/Slide/Wipe/Circle 四种）。
///
/// 规范 §11：未实现的能力不得呈现为可用选项——`random`/`none` 在 UI 里必须标
/// `Disabled` 并说明原因，且**不得**把它们下发给 daemon（那只会被静默回退成 fade）。
pub const EFFECTS_IMPLEMENTED: [&str; 4] = ["fade", "slide", "wipe", "circle"];

/// 该效果名是否**真的可用**（daemon 会执行它，而不是回退成 drop-in 的 fade）。
#[must_use]
pub fn effect_is_available(name: &str) -> bool {
    EFFECTS_IMPLEMENTED.contains(&name)
}

/// "随机效果"只能从**已实现且可用**的列表里选（`seed` 由调用方给，便于测试确定性）。
///
/// `random`/`none` 本身未实现，因此**绝不**出现在返回值里。
#[must_use]
pub fn next_random_effect(seed: u64) -> &'static str {
    let n = EFFECTS_IMPLEMENTED.len() as u64;
    EFFECTS_IMPLEMENTED[(seed % n) as usize]
}
/// 可选缓动。
pub const EASINGS: [&str; 4] = ["linear", "ease-in", "ease-out", "ease-in-out"];
/// 时长步进。
pub const DURATION_STEP_MS: u32 = 100;
/// 时长上限。
pub const DURATION_MAX_MS: u32 = 10_000;

impl TransitionDraft {
    /// 循环切换效果。
    pub fn cycle_effect(&mut self, forward: bool) {
        let i = EFFECTS.iter().position(|e| *e == self.effect).unwrap_or(0);
        let n = EFFECTS.len();
        self.effect = EFFECTS[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
        .to_string();
    }

    /// 循环切换缓动。
    pub fn cycle_easing(&mut self, forward: bool) {
        let i = EASINGS.iter().position(|e| *e == self.easing).unwrap_or(3);
        let n = EASINGS.len();
        self.easing = EASINGS[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
        .to_string();
    }

    /// 调整时长。
    pub fn adjust_duration(&mut self, delta: i32) {
        let step = i64::from(DURATION_STEP_MS) * i64::from(delta);
        self.duration_ms =
            (i64::from(self.duration_ms) + step).clamp(0, i64::from(DURATION_MAX_MS)) as u32;
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            page: Page::Dashboard,
            focus: Focus::Nav,
            subpage: SubPage::List,
            connection: Connection::Online,
            config_path: None,
            session_kind: "mock".to_string(),
            socket_path: "/run/liuguang/control.sock".to_string(),
            reconnect_attempts: 0,
            status: StatusResult {
                state: "未知".to_string(),
                output: None,
                media: None,
                paused: false,
                display_backend: "-".to_string(),
                decoder: "none".to_string(),
                mode: None,
                uptime_seconds: 0,
                channels: Vec::new(),
                fullscreen: None,
                commit: None,
                master: None,
                playback_source: None,
                state_hint: None,
            },
            outputs: OutputsResult {
                outputs: Vec::new(),
            },
            outputs_cursor: 0,
            selected_output: None,
            apply_target: ApplyTarget::Default,
            media: ListMediaResult {
                items: Vec::new(),
                total: 0,
                page: 0,
                page_size: MEDIA_PAGE_SIZE,
                truncated: false,
                scanning: false,
                scan_id: 0,
                generated_unix_ms: 0,
                skipped_oversize: 0,
                skipped_examples: Vec::new(),
            },
            media_cursor: 0,
            filter: MediaFilter::All,
            sort: MediaSort::Name,
            search: String::new(),
            search_focus: false,
            search_dirty_at: None,
            sources: ListSourcesResult {
                root: String::new(),
                sources: Vec::new(),
                missing: 0,
            },
            sources_cursor: 0,
            scan: ScanState::Idle,
            detail: None,
            detail_compat: None,
            detail_error: None,
            metrics: MetricsResult {
                thumb_requests: 3,
                thumb_hits: 2,
                thumb_misses: 1,
                thumb_decoded: 1,
                thumb_avg_ms: 42,
                thumb_queue_depth: 0,
                channels: Vec::new(),
                cpu_percent: 0.0,
                mem_bytes: 0,
                threads: 0,
                fps: 0.0,
                uptime_seconds: 0,
                commit: None,
                master: None,
                preload_cache_bytes: 0,
                preload_cache_items: 0,
                cpu_pressure: None,
            },
            playlist_edit: None,
            static_wallpaper: None,
            playback_source: None,
            static_supported: None,
            playlists: Vec::new(),
            playlists_cursor: 0,
            source_kind: "image".to_string(),
            source_recursive: false,
            sources_dirty: false,
            transform: OutputTransform::default(),
            dirty: false,
            transition: TransitionDraft::default(),
            modal: None,
            notifications: Vec::new(),
            recent_errors: Vec::new(),
            pending_ops: BTreeMap::new(),
            apply_phase: None,
            apply_started_at: None,
            editor: LineEditor::new(),
            focus_before_modal: None,
            media_dir_hint: String::new(),
            size: (80, 24),
            quit: false,
        }
    }
}

impl AppState {
    /// 新建状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前选中的媒体项（本页内）。
    pub fn selected_media(&self) -> Option<&MediaItem> {
        self.media.items.get(self.media_cursor)
    }

    /// 选中项的完整元数据（仅当 `get_media` 已返回且 id 匹配时）。
    pub fn selected_detail(&self) -> Option<&MediaItem> {
        let sel = self.selected_media()?;
        self.detail.as_ref().filter(|d| d.id == sel.id)
    }

    /// 总览页状态分类。
    pub fn ui_status(&self) -> UiStatus {
        UiStatus::from_daemon(&self.status.state, self.status.paused)
    }

    /// 当前活跃的暂停原因（优先取选定输出，否则取任一已暂停通道）。
    #[must_use]
    pub fn active_pause_reason(&self) -> Option<&'static str> {
        if let Some(sel) = self.selected_output.as_deref()
            && let Some(ch) = self.status.channels.iter().find(|c| c.output == sel)
            && ch.paused
        {
            return Some(
                ch.pause_reason
                    .as_deref()
                    .map(pause_reason_label)
                    .unwrap_or("已暂停"),
            );
        }
        if let Some(reason) = self
            .status
            .channels
            .iter()
            .filter(|c| c.paused)
            .find_map(|c| c.pause_reason.as_deref())
        {
            return Some(pause_reason_label(reason));
        }
        if self.status.paused {
            return Some("已暂停");
        }
        None
    }

    /// 该媒体是否正在任意输出上播放/显示（与 GUI `.media-card.in-use` 一致）。
    #[must_use]
    pub fn is_media_active(&self, item: &MediaItem) -> bool {
        let match_path = |p: &str| p == item.path || p == item.id;
        self.status.media.as_deref().is_some_and(match_path)
            || self
                .status
                .channels
                .iter()
                .any(|c| c.media.as_deref().is_some_and(match_path))
    }

    /// 总页数。
    pub fn page_count(&self) -> usize {
        self.media
            .total
            .div_ceil(self.media.page_size.max(1))
            .max(1)
    }

    /// 页码展示。
    pub fn page_label(&self) -> String {
        format!("第 {}/{} 页", self.media.page + 1, self.page_count())
    }

    /// 移动媒体光标（不跨页）。
    pub fn move_media_cursor(&mut self, delta: i32) {
        let len = self.media.items.len();
        if len == 0 {
            self.media_cursor = 0;
            return;
        }
        let cur = self.media_cursor.min(len - 1) as i32;
        self.media_cursor = (cur + delta).clamp(0, len as i32 - 1) as usize;
    }

    /// 移动输出光标。
    pub fn move_outputs_cursor(&mut self, delta: i32) {
        let len = self.outputs.outputs.len();
        if len == 0 {
            self.outputs_cursor = 0;
            self.selected_output = None;
            return;
        }
        let cur = self.outputs_cursor.min(len - 1) as i32;
        self.outputs_cursor = (cur + delta).clamp(0, len as i32 - 1) as usize;
        self.selected_output = Some(self.outputs.outputs[self.outputs_cursor].name.clone());
    }

    /// 移动播放列表光标。
    /// 新建（进入编辑缓冲；不发任何请求）。
    pub fn playlist_begin_new(&mut self) {
        self.playlist_edit = Some(PlaylistEdit::new_default());
        self.dirty = true;
    }

    /// 编辑当前选中的列表（不发任何请求）。
    pub fn playlist_begin_edit(&mut self) {
        if let Some(p) = self.playlists.get(self.playlists_cursor) {
            self.playlist_edit = Some(PlaylistEdit::from_playlist(p));
            self.dirty = true;
        }
    }

    /// 放弃编辑（**不落盘、不下发**）。
    pub fn playlist_discard(&mut self) {
        if self.playlist_edit.take().is_some() {
            // 只有"没有其它未保存改动"时才清 dirty：避免把别处的未保存状态吞掉。
            self.dirty = false;
        }
    }

    /// 切换 `sequential` / `random`。
    pub fn playlist_cycle_mode(&mut self) {
        if let Some(e) = self.playlist_edit.as_mut() {
            e.mode = if e.mode == "sequential" {
                "random".to_string()
            } else {
                "sequential".to_string()
            };
            self.dirty = true;
        }
    }

    /// 间隔 ±（clamp 到服务端允许的 1..=86400）。
    pub fn playlist_interval_delta(&mut self, delta: i32) {
        if let Some(e) = self.playlist_edit.as_mut() {
            let v = i64::from(e.interval_seconds) + i64::from(delta);
            e.interval_seconds = v.clamp(1, 86_400) as u32;
            self.dirty = true;
        }
    }

    /// 视频播完行为 `next` ↔ `loop`。
    pub fn playlist_toggle_video_end(&mut self) {
        if let Some(e) = self.playlist_edit.as_mut() {
            e.on_video_end = if e.on_video_end == "next" {
                "loop".to_string()
            } else {
                "next".to_string()
            };
            self.dirty = true;
        }
    }

    /// 禁止连续重复开关。
    pub fn playlist_toggle_no_repeat(&mut self) {
        if let Some(e) = self.playlist_edit.as_mut() {
            e.no_repeat = !e.no_repeat;
            self.dirty = true;
        }
    }

    /// 输出绑定轮换：默认 → 各输出 → 默认（单输出绑定由 daemon 落库）。
    pub fn playlist_cycle_output(&mut self) {
        let names: Vec<String> = self
            .outputs
            .outputs
            .iter()
            .map(|o| o.name.clone())
            .collect();
        if let Some(e) = self.playlist_edit.as_mut() {
            e.output = match e.output.as_deref() {
                None => names.first().cloned(),
                Some(cur) => {
                    let idx = names.iter().position(|n| n == cur);
                    match idx {
                        Some(i) if i + 1 < names.len() => Some(names[i + 1].clone()),
                        _ => None,
                    }
                }
            };
            self.dirty = true;
        }
    }

    /// 把**当前选中的媒体项**加入编辑缓冲（`p`）。
    pub fn playlist_add_selected(&mut self) -> bool {
        let Some(id) = self.selected_media().map(|m| m.id.clone()) else {
            return false;
        };
        if let Some(e) = self.playlist_edit.as_mut() {
            e.entries.push(id);
            e.cursor = e.entries.len().saturating_sub(1);
            self.dirty = true;
            return true;
        }
        false
    }

    /// 当前要操作的播放列表（**编辑缓冲优先**：有缓冲时不改已存在的列表）。
    #[must_use]
    pub fn playlist_target(&self) -> Option<&liuguang_ipc::protocol::Playlist> {
        if self.playlist_edit.is_some() {
            return None;
        }
        self.playlists.get(self.playlists_cursor)
    }

    /// 当前播放模式（编辑态看缓冲；否则看选中的列表；都没有 ⇒ `None`）。
    #[must_use]
    pub fn playlist_mode(&self) -> Option<PlayMode> {
        if let Some(e) = &self.playlist_edit {
            return Some(PlayMode::from_fields(&e.mode, &e.on_video_end));
        }
        self.playlists
            .get(self.playlists_cursor)
            .map(|p| PlayMode::from_fields(&p.mode, &p.on_video_end))
    }

    /// ←/→ 切模式：**一次性**写好 `mode` + `on_video_end`（t67 的核心）。
    ///
    /// - 编辑态：只改缓冲（高级面板里的 `A` 才下发），返回新模式；
    /// - 非编辑态：返回新模式，调用方用 [`Self::playlist_patch_for_mode`]
    ///   拼出"整条列表"的参数并**立即**下发（选择器"立即生效"）。
    pub fn playlist_shift_mode(&mut self, forward: bool) -> Option<PlayMode> {
        let next = self.playlist_mode()?.shift(forward);
        let (mode, end) = next.fields();
        if let Some(e) = self.playlist_edit.as_mut() {
            e.mode = mode.to_string();
            e.on_video_end = end.to_string();
            self.dirty = true;
        }
        Some(next)
    }

    /// 非编辑态：把"当前列表 + 新模式"拼成一次完整的 `update_playlist` 参数。
    #[must_use]
    pub fn playlist_patch_for_mode(
        &self,
        mode: PlayMode,
    ) -> Option<liuguang_ipc::protocol::UpdatePlaylistParams> {
        let p = self.playlist_target()?;
        let (m, end) = mode.fields();
        Some(playlist_params(
            p,
            m,
            end,
            p.entries.iter().map(|e| e.media_id.clone()).collect(),
        ))
    }

    /// 媒体库页 `a`：把选中媒体**追加到当前播放列表**（非编辑态）。
    ///
    /// 返回 `(列表名, 新条数, 待下发参数)`；错误信息必须**可操作**（不静默失败）。
    pub fn playlist_append_selected(
        &self,
    ) -> Result<(String, usize, liuguang_ipc::protocol::UpdatePlaylistParams), String> {
        let Some(media) = self.selected_media() else {
            return Err("媒体库还没有选中条目：先到【媒体库】页用 ↑↓ 选中一项".to_string());
        };
        let Some(p) = self.playlist_target() else {
            return Err(
                "还没有播放列表：先到【播放列表】页按 n 新建（再按 A 应用），然后回来按 a 加入"
                    .to_string(),
            );
        };
        let mut entries: Vec<String> = p.entries.iter().map(|e| e.media_id.clone()).collect();
        if entries.iter().any(|e| e == &media.id) {
            return Err(format!(
                "「{}」里已经有这条了（{} 条）",
                p.name,
                entries.len()
            ));
        }
        entries.push(media.id.clone());
        let n = entries.len();
        let params = playlist_params(p, &p.mode, &p.on_video_end, entries);
        Ok((p.name.clone(), n, params))
    }

    // ---- t70：静态壁纸页 ----

    /// **当前播放源**的人类可读文案 + 「是否推断」。
    ///
    /// 优先用 daemon 报告（`status.playback_source`，t69 落地后才有）；
    /// 没有报告时**按现有事实推断**并标注推断 —— 不假装知道：
    /// 1. 任一通道 `decoder == "static"` ⇒ 画面就是静态图（daemon 现有字段）；
    /// 2. 否则有播放列表 ⇒ 播放列表（取当前选中/第一条的 id）；
    /// 3. 否则 ⇒ 目录轮换。
    #[must_use]
    pub fn playback_source_label(&self) -> (String, bool) {
        if let Some(src) = self.playback_source.as_deref() {
            let label = if src == "static" {
                "静态壁纸".to_string()
            } else if let Some(id) = src.strip_prefix("playlist:") {
                format!("播放列表 {id}")
            } else if src == "directory" {
                "目录轮换".to_string()
            } else {
                src.to_string()
            };
            return (label, false);
        }
        if self.status.channels.iter().any(|c| c.decoder == "static") {
            return ("静态壁纸".to_string(), true);
        }
        if let Some(p) = self
            .playlists
            .get(self.playlists_cursor)
            .or_else(|| self.playlists.first())
        {
            return (format!("播放列表 {}", p.id), true);
        }
        ("目录轮换".to_string(), true)
    }

    /// `a`（设为静态壁纸）的前置校验：媒体库选中项必须是**图片**（用户选定）。
    ///
    /// 返回媒体 id（= 规范化绝对路径）；拒绝时给**可操作**原因。
    pub fn static_set_guard(&self) -> Result<String, String> {
        let Some(m) = self.selected_media() else {
            return Err("媒体库还没有选中条目：先到【媒体库】页用 ↑↓ 选中一张图片".to_string());
        };
        if m.kind != "image" {
            return Err(format!(
                "只能把**图片**设为静态壁纸：当前选中的是「{}」（{}）。请改用图片，或按 `W` 把它应用到输出",
                m.name, m.kind
            ));
        }
        Ok(m.id.clone())
    }

    /// 从编辑缓冲移除光标处条目（`d`）。
    pub fn playlist_remove_entry(&mut self) {
        if let Some(e) = self.playlist_edit.as_mut() {
            if e.cursor < e.entries.len() {
                e.entries.remove(e.cursor);
                e.cursor = e.cursor.min(e.entries.len().saturating_sub(1));
                self.dirty = true;
            }
        }
    }

    /// 编辑缓冲内的条目光标移动（`j/k`）。
    pub fn playlist_move_entry_cursor(&mut self, delta: i32) {
        if let Some(e) = self.playlist_edit.as_mut() {
            let len = e.entries.len();
            if len == 0 {
                e.cursor = 0;
                return;
            }
            let cur = e.cursor.min(len - 1) as i32;
            e.cursor = (cur + delta).clamp(0, len as i32 - 1) as usize;
        }
    }

    pub fn move_playlists_cursor(&mut self, delta: i32) {
        let len = self.playlists.len();
        if len == 0 {
            self.playlists_cursor = 0;
            return;
        }
        let cur = self.playlists_cursor.min(len - 1) as i32;
        self.playlists_cursor = (cur + delta).clamp(0, len as i32 - 1) as usize;
    }

    /// 移动来源光标。
    pub fn move_sources_cursor(&mut self, delta: i32) {
        let len = self.sources.sources.len();
        if len == 0 {
            self.sources_cursor = 0;
            return;
        }
        let cur = self.sources_cursor.min(len - 1) as i32;
        self.sources_cursor = (cur + delta).clamp(0, len as i32 - 1) as usize;
    }

    /// 打开模态框（记录焦点；清空编辑器）。
    pub fn open_modal(&mut self, modal: Modal) {
        if self.modal.is_none() {
            self.focus_before_modal = Some(self.focus);
        }
        self.editor.clear();
        self.modal = Some(modal);
    }

    /// 关闭模态框并恢复焦点。
    pub fn close_modal(&mut self) {
        self.modal = None;
        self.editor.clear();
        if let Some(f) = self.focus_before_modal.take() {
            self.focus = f;
        }
    }

    /// 是否有弹窗。
    pub fn modal_open(&self) -> bool {
        self.modal.is_some()
    }

    /// 记录通知（有界）。
    pub fn notify(&mut self, kind: NotificationKind, text: impl Into<String>) {
        self.notifications.push(Notification {
            kind,
            text: text.into(),
        });
        if self.notifications.len() > MAX_NOTIFICATIONS {
            let drop = self.notifications.len() - MAX_NOTIFICATIONS;
            self.notifications.drain(..drop);
        }
    }

    /// 只记进"最近错误"（系统页/排查用），**不弹通知**。
    ///
    /// 为什么需要它：底栏通知栏只有**一行**，长文本会被截断（真机实测：
    /// `UNSUPPORTED_METHOD` 的完整报文会把"临时替代办法"挤掉）。所以
    /// **通知放短句 + 可操作动作**，长报文进这里，供系统页查看。
    pub fn record_error(&mut self, text: impl Into<String>) {
        self.recent_errors.push(text.into());
        while self.recent_errors.len() > MAX_RECENT_ERRORS {
            self.recent_errors.remove(0);
        }
    }

    /// 记录一条错误（同时进通知与"最近错误"列表，供系统页只读诊断）。
    pub fn push_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.notify(NotificationKind::Error, text.clone());
        self.recent_errors.push(text);
        while self.recent_errors.len() > MAX_RECENT_ERRORS {
            self.recent_errors.remove(0);
        }
    }

    /// 最近一条通知。
    pub fn last_notification(&self) -> Option<&Notification> {
        self.notifications.last()
    }

    /// 记录进行中的请求。
    pub fn begin_op(&mut self, id: impl Into<String>, what: impl Into<String>) {
        self.pending_ops.insert(
            id.into(),
            PendingOp {
                what: what.into(),
                state: PendingState::Sending,
            },
        );
    }

    /// 更新请求状态。
    pub fn set_op_state(&mut self, id: &str, state: PendingState) {
        if let Some(op) = self.pending_ops.get_mut(id) {
            op.state = state;
        }
    }

    /// 切页（并把焦点放到该页的合理默认值）。
    pub fn goto(&mut self, page: Page) {
        self.page = page;
        self.focus = match page {
            Page::Playlists | Page::Outputs => Focus::List,
            Page::Library => Focus::List,
            Page::Dashboard | Page::Effects | Page::System => Focus::Actions,
            // 静态壁纸页：没有列表可选，动作即全部（a/c）
            Page::Static => Focus::Actions,
        };
    }

    /// 搜索防抖是否到期。
    pub fn search_debounce_due(&self, now: Instant) -> bool {
        match self.search_dirty_at {
            Some(at) => now.duration_since(at) >= SEARCH_DEBOUNCE,
            None => false,
        }
    }

    /// 标记搜索词刚变更。
    pub fn mark_search_dirty(&mut self, now: Instant) {
        self.search_dirty_at = Some(now);
    }

    /// 应用是否仍在等待确认（用于"等待确认"提示，**不得**当作已应用）。
    pub fn apply_waiting(&self, now: Instant) -> bool {
        matches!(
            self.apply_phase,
            Some(ApplyPhase::Accepted) | Some(ApplyPhase::Sending)
        ) && self
            .apply_started_at
            .is_none_or(|t| now.duration_since(t) < APPLY_CONFIRM_TIMEOUT)
    }
}

/// 针对总数与页码做边界钳制（纯函数）。
pub fn clamp_page(page: usize, total: usize, page_size: usize) -> usize {
    let size = page_size.max(1);
    let last = total.div_ceil(size).saturating_sub(1);
    page.min(last)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn pages_have_stable_hotkeys_and_titles() {
        assert_eq!(Page::ALL.len(), 7);
        assert_eq!(
            Page::ALL.iter().map(|p| p.title()).collect::<Vec<_>>(),
            vec![
                "总览",
                "媒体库",
                "播放列表",
                "显示器",
                "转场",
                "系统",
                "静态壁纸"
            ]
        );
        for (i, p) in Page::ALL.into_iter().enumerate() {
            assert_eq!(p.index(), i);
            assert_eq!(Page::from_hotkey(p.hotkey()), Some(p));
        }
        assert_eq!(Page::from_hotkey('8'), None);
    }

    #[test]
    fn focus_and_subpages_cycle() {
        assert_eq!(Focus::Nav.next(true), Focus::List);
        assert_eq!(Focus::Nav.next(false), Focus::Actions);
        let mut f = Focus::Nav;
        for _ in 0..4 {
            f = f.next(true);
        }
        assert_eq!(f, Focus::Nav);
        assert_eq!(SubPage::List.shift(true), SubPage::Detail);
        assert_eq!(SubPage::List.shift(false), SubPage::Actions);
    }

    #[test]
    fn editor_is_unicode_safe() {
        let mut e = LineEditor::new();
        for c in "中文abc".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "中文abc");
        assert_eq!(e.len(), 5, "按字符计数");
        e.home();
        e.delete();
        assert_eq!(e.text(), "文abc");
        e.end();
        e.backspace();
        assert_eq!(e.text(), "文ab");
        e.insert('\n');
        assert_eq!(e.text(), "文ab", "控制字符被拒绝");
    }

    #[test]
    fn filter_and_sort_have_wire_values() {
        assert_eq!(
            MediaFilter::All.wire(),
            None,
            "「全部」必须发 None（不筛选）"
        );
        assert_eq!(MediaFilter::Image.wire(), Some("image"));
        assert_eq!(MediaFilter::Video.wire(), Some("video"));
        assert_eq!(MediaSort::Name.wire(), "name");
        assert_eq!(MediaSort::Size.wire(), "size");
        assert_eq!(MediaSort::Modified.wire(), "modified");
        assert_eq!(MediaFilter::All.next(), MediaFilter::Image);
        assert_eq!(MediaSort::Modified.next(), MediaSort::Name);
    }

    #[test]
    fn ui_status_maps_daemon_values_without_guessing() {
        assert_eq!(
            UiStatus::from_daemon("VideoPlaying", false),
            UiStatus::Playing
        );
        assert_eq!(
            UiStatus::from_daemon("VideoPlaying", true),
            UiStatus::Paused
        );
        assert_eq!(UiStatus::from_daemon("Paused", false), UiStatus::Paused);
        assert_eq!(UiStatus::from_daemon("StaticIdle", false), UiStatus::Static);
        assert_eq!(
            UiStatus::from_daemon("StaticShown", false),
            UiStatus::Static
        );
        assert_eq!(
            UiStatus::from_daemon("Transitioning", false),
            UiStatus::Loading
        );
        assert_eq!(
            UiStatus::from_daemon("PreparingNext", false),
            UiStatus::Loading
        );
        assert_eq!(
            UiStatus::from_daemon("DegradedStatic", false),
            UiStatus::Error
        );
        assert_eq!(
            UiStatus::from_daemon("HeadlessReady", false),
            UiStatus::HeadlessReady
        );
        assert_eq!(UiStatus::from_daemon("Whatever", false), UiStatus::Unknown);
    }

    #[test]
    fn apply_phase_never_claims_applied_before_event() {
        assert!(!ApplyPhase::Sending.is_applied());
        assert!(!ApplyPhase::Accepted.is_applied());
        assert!(ApplyPhase::Applied.is_applied());
        assert!(!ApplyPhase::Failed("x".into()).is_applied());
        assert!(ApplyPhase::Accepted.label().contains("已受理"));
        assert!(!ApplyPhase::Accepted.label().contains("Applied"));
    }

    #[test]
    fn apply_target_is_explicit() {
        assert_eq!(ApplyTarget::default(), ApplyTarget::Default);
        assert_eq!(ApplyTarget::Output("DP-1".into()).label(), "DP-1");
        assert_eq!(ApplyTarget::AllOutputs.label(), "全部显示器");
    }

    #[test]
    fn media_helpers() {
        let item = MediaItem {
            id: "id".into(),
            source_id: "s".into(),
            name: "城市夜景.png".into(),
            path: "/srv/城市夜景.png".into(),
            kind: "image".into(),
            size_bytes: 5 * 1024 * 1024,
            modified_unix: 0,
            details: None,
        };
        assert!(!is_video(&item));
        assert_eq!(size_label(item.size_bytes), "5.0 MiB");
        assert_eq!(size_label(512), "512 B");
        let mut v = item.clone();
        v.kind = "video".into();
        assert!(is_video(&v));
    }

    #[test]
    fn transform_to_params_uses_daemon_units() {
        let mut t = OutputTransform::default();
        t.cycle_fit(true);
        assert_eq!(t.fit, "contain");
        t.adjust_zoom(0.5);
        assert!((t.zoom - 1.5).abs() < f32::EPSILON);
        t.adjust_offset_x(10.0);
        assert!((t.offset_x - 10.0).abs() < f32::EPSILON);
        t.cycle_rotation();
        assert_eq!(t.rotation, 90);
        let p = t.to_params(Some("DP-1".to_string()));
        assert_eq!(p.output.as_deref(), Some("DP-1"));
        assert_eq!(p.fit.as_deref(), Some("contain"));
        assert_eq!(p.zoom, Some(1.5));
        assert_eq!(p.rotation, Some(90));
    }

    #[test]
    fn transform_clamps_zoom() {
        let mut t = OutputTransform::default();
        t.adjust_zoom(1000.0);
        assert_eq!(t.zoom, 10.0);
        t.adjust_zoom(-1000.0);
        assert_eq!(t.zoom, 0.1);
    }

    #[test]
    fn transform_brightness_clamps_and_resets() {
        let mut t = OutputTransform::default();
        assert_eq!(t.brightness_percent(), 100);
        t.adjust_brightness(-0.25);
        assert_eq!(t.brightness_percent(), 75);
        t.adjust_brightness(-5.0);
        assert_eq!(t.brightness_percent(), 10);
        t.adjust_brightness(5.0);
        assert_eq!(t.brightness_percent(), 100);
        let p = t.to_params(Some("DP-1".to_string()));
        assert_eq!(p.brightness, Some(1.0));

        t.fit = "stretch".to_string();
        t.zoom = 2.0;
        t.offset_x = 120.0;
        t.rotation = 180;
        t.adjust_brightness(-0.5);
        t.reset_defaults();
        assert_eq!(t.fit, "cover");
        assert_eq!(t.zoom, 1.0);
        assert_eq!(t.offset_x, 0.0);
        assert_eq!(t.rotation, 0);
        assert_eq!(t.brightness_percent(), 100);
    }

    #[test]
    fn pause_reason_and_active_media_helpers() {
        assert_eq!(pause_reason_label("battery"), "电池省电");
        assert_eq!(pause_reason_label("game_running"), "游戏让路");
        assert_eq!(pause_reason_label("gpu_memory"), "显存让路");
        assert_eq!(pause_reason_label("fullscreen"), "全屏让路");
        assert_eq!(format_fps_int(59.94), "60 fps");
        assert_eq!(format_fps_int(0.0), "—");

        let mut st = AppState::new();
        st.status.paused = true;
        st.status
            .channels
            .push(liuguang_ipc::protocol::ChannelStatus {
                output: "eDP-1".into(),
                state: "Paused".into(),
                media: Some("/srv/a.mp4".into()),
                mode: Some("2560x1600@60".into()),
                paused: true,
                decoder: "vaapi:nvdec(dmabuf)".into(),
                video: None,
                pinned: false,
                pause_reason: Some("battery".into()),
            });
        st.selected_output = Some("eDP-1".into());
        assert_eq!(st.active_pause_reason(), Some("电池省电"));
        let item = MediaItem {
            id: "/srv/a.mp4".into(),
            source_id: "s".into(),
            name: "a.mp4".into(),
            path: "/srv/a.mp4".into(),
            kind: "video".into(),
            size_bytes: 1024,
            modified_unix: 0,
            details: None,
        };
        assert!(st.is_media_active(&item));
    }

    #[test]
    fn page_count_and_clamp() {
        let mut st = AppState::new();
        st.media.total = 10_000;
        st.media.page_size = 50;
        assert_eq!(st.page_count(), 200);
        assert_eq!(clamp_page(999, 10_000, 50), 199);
        assert_eq!(clamp_page(3, 0, 50), 0);
        assert_eq!(clamp_page(0, 10, 0), 0);
    }

    #[test]
    fn modal_captures_and_restores_focus() {
        let mut st = AppState::new();
        st.focus = Focus::List;
        st.open_modal(Modal::PathInput {
            purpose: "设为壁纸".into(),
        });
        assert!(st.modal_open());
        st.editor.insert('/');
        st.close_modal();
        assert!(!st.modal_open());
        assert_eq!(st.focus, Focus::List);
        assert!(st.editor.is_empty());
    }

    #[test]
    fn path_text_modal_carries_full_text() {
        let long = "/srv/wallpapers/收藏/城市夜景_最终版.png";
        let st = {
            let mut st = AppState::new();
            st.open_modal(Modal::PathText {
                title: "复制路径".into(),
                text: long.into(),
            });
            st
        };
        match st.modal.clone().unwrap() {
            Modal::PathText { text, title } => {
                assert_eq!(text, long);
                assert_eq!(title, "复制路径");
            }
            other => panic!("期望 PathText，得到 {other:?}"),
        }
    }

    #[test]
    fn notifications_bounded() {
        let mut st = AppState::new();
        for i in 0..(MAX_NOTIFICATIONS + 5) {
            st.notify(NotificationKind::Info, format!("n{i}"));
        }
        assert_eq!(st.notifications.len(), MAX_NOTIFICATIONS);
    }

    #[test]
    fn search_debounce_window() {
        let mut st = AppState::new();
        let t0 = Instant::now();
        assert!(!st.search_debounce_due(t0));
        st.mark_search_dirty(t0);
        assert!(!st.search_debounce_due(t0 + SEARCH_DEBOUNCE / 2));
        assert!(st.search_debounce_due(t0 + SEARCH_DEBOUNCE));
    }

    #[test]
    fn apply_waiting_expires_without_claiming_applied() {
        let mut st = AppState::new();
        let t0 = Instant::now();
        st.apply_phase = Some(ApplyPhase::Accepted);
        st.apply_started_at = Some(t0);
        assert!(st.apply_waiting(t0));
        assert!(!st.apply_waiting(t0 + APPLY_CONFIRM_TIMEOUT));
        st.apply_phase = Some(ApplyPhase::Applied);
        assert!(!st.apply_waiting(t0));
    }

    #[test]
    fn text_input_modal_carries_target_and_hint() {
        let mut st = AppState::new();
        st.open_modal(Modal::TextInput {
            title: "重命名播放列表".into(),
            target: TextTarget::PlaylistName,
            hint: "Enter 提交 · Esc 取消".into(),
        });
        match st.modal.clone().unwrap() {
            Modal::TextInput { target, hint, .. } => {
                assert_eq!(target, TextTarget::PlaylistName);
                assert!(hint.contains("Enter"));
            }
            other => panic!("期望 TextInput，得到 {other:?}"),
        }
        assert_eq!(st.modal.clone().unwrap().title(), "重命名播放列表");
    }

    #[test]
    fn recent_errors_are_bounded_and_visible() {
        let mut st = AppState::new();
        for i in 0..(MAX_RECENT_ERRORS + 3) {
            st.push_error(format!("e{i}"));
        }
        assert_eq!(st.recent_errors.len(), MAX_RECENT_ERRORS);
        assert_eq!(st.recent_errors.last().map(String::as_str), Some("e7"));
        assert_eq!(
            st.last_notification().map(|n| n.kind),
            Some(NotificationKind::Error)
        );
    }

    #[test]
    fn source_edit_defaults_are_conservative() {
        let st = AppState::new();
        assert_eq!(st.source_kind, "image");
        assert!(!st.source_recursive, "默认不递归（避免意外扫全盘）");
        assert!(!st.sources_dirty);
    }

    #[test]
    fn scan_state_labels() {
        assert_eq!(ScanState::Idle.label(), "空闲");
        assert!(ScanState::Running { scan_id: 3 }.label().contains("3"));
        assert!(
            ScanState::Finished {
                scan_id: 3,
                found: 12
            }
            .label()
            .contains("12")
        );
        assert!(ScanState::Failed("超时".into()).label().contains("超时"));
    }
}
