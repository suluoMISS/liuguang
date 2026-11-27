//! 协议 v1 消息定义（规范 §9.1）。
//!
//! 每条消息必须携带 `v`（协议版本）与 `id`（请求 ID）；错误响应携带
//! 稳定的 `error.code` 与面向人的 `error.message`。`accepted=true`
//! 只代表请求已受理，最终结果以状态查询/事件订阅确认（M2 起提供事件流）。

use serde::{Deserialize, Serialize};

/// 当前协议版本。
pub const PROTOCOL_VERSION: u32 = 1;

/// 单条消息最大字节数（含结尾换行）。超过即拒绝，防止畸形/恶意输入耗尽内存。
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// RPC 方法名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// 查询守护进程与输出状态。
    Status,
    /// 枚举输出。
    Outputs,
    /// 设置壁纸（图片或视频路径）。
    SetWallpaper,
    /// 切换到下一个媒体项。
    Next,
    /// 暂停播放。
    Pause,
    /// 恢复播放。
    Resume,
    /// 只放这一项（`true` = 不跟随轮换，视频单曲循环 / 图片一直显示）。
    SetPinned,
    /// 重载配置。
    Reload,
    /// 设置转场效果。
    Transition,
    /// 订阅事件流。
    Subscribe,
    /// 查询**对账计数**：显示侧提交计数、DRM master 状态、逐通道出队/提交对账。
    GetMetrics,
    /// 列出媒体来源（daemon 侧规范化 + 根边界校验后的结果）。
    ListSources,
    /// 列出媒体项（**必须分页**）。
    ListMedia,
    /// 按 id（规范化后的绝对路径）取单个媒体项。
    GetMedia,
    /// 取媒体缩略图（**视频由 daemon 解码一帧**，图片由调用方自行缩放）。
    ///
    /// 为什么要走 daemon：解码器、媒体根边界校验、路径规范化都只有它有；
    /// GUI 拿到的路径不能无条件当文件读（越界读是安全边界问题）。
    GetThumbnail,
    /// **准备一个硬件解不动的素材**（t15）：把它转码成可硬解的缓存后自动播放。
    ///
    /// 语义：**幂等 + 异步**。缓存已就绪 ⇒ 立刻回 `state = ready`；否则入队并开始
    /// 后台转码，回 `state = queued|running`，进度用本方法重复查询（或订阅
    /// `MEDIA_PREPARED` 事件）。**源文件绝不修改**，缓存写在
    /// `$XDG_CACHE_HOME/liuguang/transcode/` 下。
    ///
    /// `set_wallpaper` 碰到这类素材时**不会**静默软解：它回
    /// `NEEDS_PREPARATION` 并自动开始转码，完成后自动播放（见该错误码的文档）。
    PrepareMedia,
    /// 启动一次**有界后台扫描**（异步；结果经 `SCAN_PROGRESS`/`SCAN_FINISHED` 事件返回）。
    ScanSource,
    /// 重扫整个媒体库（= 按当前配置重新走一遍全部来源，**磁盘上已删除的条目会被丢弃**）。
    ///
    /// 与 `ScanSource` 的区别：不带参数、不区分来源，语义就是"现在重新看一眼磁盘"。
    RescanLibrary,
    /// 追加一个媒体来源（daemon 侧规范化 + 根边界校验）。
    AddSource,
    /// 移除一个媒体来源（`source_id`）。
    RemoveSource,
    /// **从媒体库索引移除**一条媒体项（`path`）。
    ///
    /// ⚠️ 语义边界：**不删磁盘文件**。本方法只让该项从 `list_media`/`get_media`
    /// 的结果里消失（索引是扫描产物，下一次 `rescan_library` 会再次把它扫回来），
    /// 若它正在某个输出上播放，daemon 会先把该输出切到别的画面。
    /// 想真正清理磁盘请由用户/外部工具自行删除文件。
    DeleteMedia,
    /// 列出播放列表。
    ListPlaylists,
    /// 新建/更新播放列表。
    UpdatePlaylist,
    /// **删除**一个播放列表（`playlist_id`）。
    ///
    /// ⚠️ 语义边界：**不删磁盘文件**，也不删播放列表里引用的任何媒体项；
    /// 只删掉这条播放列表定义本身（正在使用它的输出回落到目录轮换）。
    DeletePlaylist,
    /// **只改运行时**的输出变换（持久化走 `save_config`）。
    SetOutputTransform,
    /// 原子写回配置文件（未授权用户明确拒绝）。
    SaveConfig,
    /// 设为**静态壁纸**（最高优先级：暂停播放列表并记住位置，静止显示该图）。
    ///
    /// ⚠️ **协议已声明、运行时行为尚未实现**（见 [`Method::awaits_handler`]）：
    /// 本版 `liuguang` 对它仍返回 `UNSUPPORTED_METHOD` —— 这是**有意**的，
    /// TUI（t70 已按本接口实现）据此显示"暂不可用"的可操作提示。
    SetStaticWallpaper,
    /// 清除静态壁纸并恢复播放（从暂停处继续）。同上：**协议已声明、行为待实现**。
    ClearStaticWallpaper,
}

impl Method {
    /// 由线上方法名解析（`snake_case`，与 serde 表示一致）。
    ///
    /// 用于在**反序列化之前**区分"未知方法"（`UNSUPPORTED_METHOD`）与
    /// "结构非法"（`INVALID_MESSAGE`）——两者对客户端的含义完全不同。
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        serde_json::from_value::<Self>(serde_json::Value::String(name.to_string())).ok()
    }

    /// 本方法的线上名（`snake_case`）。
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Outputs => "outputs",
            Self::SetWallpaper => "set_wallpaper",
            Self::Next => "next",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::SetPinned => "set_pinned",
            Self::Reload => "reload",
            Self::Transition => "transition",
            Self::Subscribe => "subscribe",
            Self::GetMetrics => "get_metrics",
            Self::ListSources => "list_sources",
            Self::ListMedia => "list_media",
            Self::GetMedia => "get_media",
            Self::GetThumbnail => "get_thumbnail",
            Self::PrepareMedia => "prepare_media",
            Self::ScanSource => "scan_source",
            Self::RescanLibrary => "rescan_library",
            Self::AddSource => "add_source",
            Self::RemoveSource => "remove_source",
            Self::DeleteMedia => "delete_media",
            Self::ListPlaylists => "list_playlists",
            Self::UpdatePlaylist => "update_playlist",
            Self::DeletePlaylist => "delete_playlist",
            Self::SetOutputTransform => "set_output_transform",
            Self::SaveConfig => "save_config",
            Self::SetStaticWallpaper => "set_static_wallpaper",
            Self::ClearStaticWallpaper => "clear_static_wallpaper",
        }
    }

    /// 协议**已声明**、但运行时**尚无处理器**的方法（纯增量前移的过渡态）。
    ///
    /// 为什么需要这个谓词：`Method` 是**穷尽枚举**，`daemon` 的 `match` 必须覆盖
    /// 每个变体；在行为落地之前，这些方法必须**仍然**回 `UNSUPPORTED_METHOD`，
    /// 否则客户端会从"善意降级"变成"调用成功但什么也没发生"（更糟：谎报成功）。
    ///
    /// **t69 已接线**：`set_static_wallpaper` / `clear_static_wallpaper` 由
    /// [`crate`] 之外的 daemon 处理器实现（暂停播放列表 + 显示静态图 / 恢复播放）。
    /// 目前协议里**所有**方法都已有处理器，因此本谓词恒为 `false`。
    ///
    /// 保留该函数而不是删掉：它是 daemon 与 TUI 判断"是否该降级"的**单点**，
    /// 未来新增"先声明、后实现"的方法时直接在这里加分支即可，无需改调用方。
    #[must_use]
    pub fn awaits_handler(self) -> bool {
        // 目前无待接线方法。新增"声明但未实现"的方法时改成 matches!(...)。
        let _ = self;
        false
    }
}

/// 请求消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// 协议版本。
    pub v: u32,
    /// 请求 ID（响应原样回传）。
    pub id: String,
    /// 方法。
    pub method: Method,
    /// 方法参数（方法特定结构由 M2 的命令路由定义）。
    pub params: serde_json::Value,
}

impl Request {
    /// 构造请求。
    pub fn new(id: impl Into<String>, method: Method, params: serde_json::Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: id.into(),
            method,
            params,
        }
    }

    /// 版本是否受支持。
    pub fn version_supported(&self) -> bool {
        self.v == PROTOCOL_VERSION
    }
}

/// 错误类别（稳定代码，供脚本判断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorKind {
    /// 协议版本不受支持。
    UnsupportedVersion,
    /// 消息格式非法。
    InvalidMessage,
    /// 方法名未知（同一协议版本内方法只增不改；通常意味着客户端/服务端版本不匹配）。
    UnsupportedMethod,
    /// 消息超过大小上限。
    MessageTooLarge,
    /// 请求参数非法。
    InvalidParams,
    /// 输出不存在。
    UnknownOutput,
    /// 媒体/编码不受支持。
    UnsupportedCodec,
    /// 素材硬件解不动、正在后台转码（**不是失败**）：稍后重试或等 `MEDIA_PREPARED`。
    ///
    /// 出现时机（t15）：`set_wallpaper` 指向本机硬件解不了的素材（8K H.264、
    /// 10bit HEVC 等）且缓存还没转好。daemon 已自动开始转码，转完会推
    /// `MEDIA_PREPARED{path, cache_path}` 并自动播放。
    NeedsPreparation,
    /// 文件不存在或不可读。
    FileUnavailable,
    /// 设备/权限不可用。
    DeviceUnavailable,
    /// 内部错误。
    Internal,
    /// 服务繁忙（并发连接数已达上限，**或**后台任务队列已满）。
    ServerBusy,
    /// 任务队列已满：请求本身合法，只是**此刻**排不进去（t39）。
    ///
    /// 语义：立刻重试无意义，稍后（下一次滚动/下一帧）重试即可；客户端**不应**
    /// 把这类错误当成"素材坏了"。典型场景是 GUI 一次给几十张卡要缩略图，
    /// daemon 的串行解码队列有上限，多出来的请求直接回 `BUSY`（不阻塞连接、
    /// 也不拖慢正在播放的画面），等队列消化后再请求就会命中缓存。
    Busy,
    /// 权限不足（peer credential / 文件权限）。
    PermissionDenied,
    /// 该方法尚未实现（所属里程碑未到）。
    NotImplemented,
}

impl ErrorKind {
    /// 线上稳定字符串（与 serde 表示一致；脚本按它判断）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedVersion => "UNSUPPORTED_VERSION",
            Self::InvalidMessage => "INVALID_MESSAGE",
            Self::UnsupportedMethod => "UNSUPPORTED_METHOD",
            Self::MessageTooLarge => "MESSAGE_TOO_LARGE",
            Self::InvalidParams => "INVALID_PARAMS",
            Self::UnknownOutput => "UNKNOWN_OUTPUT",
            Self::UnsupportedCodec => "UNSUPPORTED_CODEC",
            Self::NeedsPreparation => "NEEDS_PREPARATION",
            Self::FileUnavailable => "FILE_UNAVAILABLE",
            Self::DeviceUnavailable => "DEVICE_UNAVAILABLE",
            Self::Internal => "INTERNAL",
            Self::ServerBusy => "SERVER_BUSY",
            Self::Busy => "BUSY",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::NotImplemented => "NOT_IMPLEMENTED",
        }
    }
}

/// 错误详情。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// 稳定错误码（枚举）。
    pub code: ErrorKind,
    /// 面向人的说明。
    pub message: String,
    /// **更细的稳定机器码**（可选；如 `PATH_OUTSIDE_ROOT` / `NOT_A_DIRECTORY` /
    /// `PERMISSION_DENIED` / `IO_ERROR`）。
    ///
    /// [`ErrorKind`] 是粗粒度、跨版本稳定的枚举；本字段用于表达"同一枚举下的
    /// 不同原因"，**只增不改**。旧客户端忽略它即可（`serde` 默认忽略未知字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_code: Option<String>,
    /// **冲突来源容器**（可选；结构化机器可读，供调用方直接展示/决策）。
    ///
    /// 目前用于 `save_config` 的 `NO_EFFECTIVE_CHANGE`：列出"仍然覆盖着你要删除的
    /// 来源"的配置容器，如 `[slideshow].directory` / `[library].image_dirs`。
    /// 调用方**不应**从 `message` 里解析这些名字（文案会变，容器名是契约）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

impl ErrorBody {
    /// 构造错误详情。
    pub fn new(code: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail_code: None,
            conflicts: Vec::new(),
        }
    }

    /// 附加冲突容器名（见 [`ErrorBody::conflicts`]）。
    #[must_use]
    pub fn with_conflicts(mut self, conflicts: Vec<String>) -> Self {
        self.conflicts = conflicts;
        self
    }

    /// 附加细粒度机器码（见 [`ErrorBody::detail_code`]）。
    pub fn with_detail_code(mut self, detail: impl Into<String>) -> Self {
        self.detail_code = Some(detail.into());
        self
    }
}

/// 响应消息：成功携带 `result`，失败携带 `error`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// 协议版本。
    pub v: u32,
    /// 对应请求 ID。
    pub id: String,
    /// 是否受理成功。
    pub ok: bool,
    /// 成功结果（方法特定结构由 M2 的命令路由定义）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// 失败详情。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    /// 成功响应。
    pub fn ok(id: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// 「协议已声明、运行时尚无处理器」的标准失败响应（`UNSUPPORTED_METHOD`）。
    ///
    /// 单一事实来源：`daemon` 的兜底分支只调用它，不自己拼字符串 ——
    /// 这样"哪些方法还没接线"由 [`Method::awaits_handler`] 决定，文案只有一份。
    pub fn unsupported_method(id: impl Into<String>, method: Method) -> Self {
        debug_assert!(
            method.awaits_handler(),
            "只有 awaits_handler()==true 的方法才应走这个兜底；\
             已接线的方法必须走真实处理器（否则等于谎报不支持）"
        );
        Self::err(
            id,
            ErrorBody::new(
                ErrorKind::UnsupportedMethod,
                format!(
                    "方法 `{}` 已在本版协议中声明，但本版运行时尚无处理器（t69 待接线）；\
                     协议 v{PROTOCOL_VERSION}：同一版本内方法只增不改",
                    method.wire_name()
                ),
            ),
        )
    }

    /// 失败响应。
    pub fn err(id: impl Into<String>, error: ErrorBody) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: id.into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// `set_static_wallpaper` 参数（t76 声明、t69 接线）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetStaticWallpaperParams {
    /// 服务器**绝对路径**（daemon 侧负责校验；TUI 传媒体库条目的 id）。
    pub path: String,
}

/// `set_wallpaper` 参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetWallpaperParams {
    /// 媒体文件绝对路径。
    pub path: String,
    /// 目标输出（省略 = 默认输出）。
    #[serde(default)]
    pub output: Option<String>,
    /// 适配模式（省略 = 配置/默认）。
    #[serde(default)]
    pub fit: Option<String>,
    /// 缩放倍数。
    #[serde(default)]
    pub zoom: Option<f64>,
    /// 水平偏移（输出像素）。
    #[serde(default)]
    pub offset_x: Option<f64>,
    /// 垂直偏移（输出像素）。
    #[serde(default)]
    pub offset_y: Option<f64>,
    /// 顺时针旋转角度（`0`/`90`/`180`/`270`；省略 = 配置/默认）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<u32>,
    /// 亮度倍率（`0.1..=1.5`，`1.0` = 原始亮度；省略 = 配置/默认）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<f64>,
    /// **只放这一项**（`true` = 不跟随轮换：视频单曲循环、图片一直显示）。
    ///
    /// `None` 与 `false` 等价（保持旧语义：设置后仍按 `interval_seconds` 轮换）；
    /// 用 `Option` 只是为了能表达"客户端没提这件事"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<bool>,
}

/// `set_pinned` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetPinnedParams {
    /// `true` = 只放当前这一项（不轮换）；`false` = 恢复按 `interval_seconds` 轮换。
    pub pinned: bool,
    /// 目标输出（省略 = 默认输出）。
    #[serde(default)]
    pub output: Option<String>,
}

/// `set_pinned` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetPinnedResult {
    /// 生效后的状态（原样回显，便于客户端立刻更新界面）。
    pub pinned: bool,
    /// 当前输出名。
    pub output: Option<String>,
}

/// 带可选输出名的参数（pause/resume/next）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutputParams {
    /// 目标输出。
    #[serde(default)]
    pub output: Option<String>,
}

/// `transition` 参数（规范 §7.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionParams {
    /// 效果名（fade/slide/wipe/circle/random/none）。
    pub effect: String,
    /// 时长（毫秒，0..=10000）。
    pub duration_ms: u32,
    /// 缓动名。
    #[serde(default)]
    pub easing: Option<String>,
    /// 方向名。
    #[serde(default)]
    pub direction: Option<String>,
}

/// 通用受理结果（`accepted=true` 仅表示请求已受理）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedResult {
    /// 是否受理（**只表示受理**；最终结果见 `WALLPAPER_APPLIED` 事件或状态查询）。
    pub accepted: bool,
    /// 请求 ID：与随后 `WALLPAPER_APPLIED` 事件里的 `request_id` 对应。
    ///
    /// 只有会产生"完成事件"的方法（`set_wallpaper`）才带；其余方法为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl AcceptedResult {
    /// 已受理（无完成事件的方法）。
    pub fn accepted() -> Self {
        Self {
            accepted: true,
            request_id: None,
        }
    }

    /// 已受理，并给出用于对账 `WALLPAPER_APPLIED` 的 `request_id`。
    pub fn accepted_with_request(request_id: impl Into<String>) -> Self {
        Self {
            accepted: true,
            request_id: Some(request_id.into()),
        }
    }
}

/// 视频通道观测状态（M5 §6.4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStatsStatus {
    /// **已出队**的帧数（媒体侧在帧出队时自增，早于任何显示调用）。
    ///
    /// **这不是"上屏"证据**：提交失败时它照样增长（真机实测：EACCES 风暴下
    /// 90s→1370 持续增长而屏幕未变化）。判断"真的在上屏"必须同时看
    /// [`VideoStatsStatus::present_ok`] 与 [`VideoStatsStatus::present_failures`]，
    /// 并以显示侧计数 + i915 pipe CRC 交叉验证。
    pub presented: u64,
    /// 过期丢帧数。
    pub dropped_late: u64,
    /// 队列满丢弃数。
    pub dropped_queue_full: u64,
    /// 当前队列长度。
    pub queue_len: usize,
    /// 源侧提前丢弃（未做像素转换；高帧率源 + 低输出帧率时发生）。
    #[serde(default)]
    pub skipped: u64,
    /// 源帧率。
    #[serde(default)]
    pub source_fps: Option<f64>,
    /// 最后出队 PTS（毫秒）。
    #[serde(default)]
    pub last_pts_ms: Option<u64>,
    /// **成功提交扫描输出**的帧数（显示调用返回 `Ok`）。
    ///
    /// 与 [`VideoStatsStatus::presented`] 对账：
    /// `presented` 增长而 `present_ok` 不增长 ⇒ 帧被取出但没提交成功（异常签名）。
    #[serde(default)]
    pub present_ok: u64,
    /// **提交失败**的帧数（显示调用返回 `Err`，或返回 `Ok` 但显示侧记账失败）。
    ///
    /// 与 `presented` 同时增长即为异常（"presented 自嗨"）；连续失败达到阈值会
    /// 收敛到降级态（见 [`VideoStatsStatus::present_failures_consecutive`]）。
    #[serde(default)]
    pub present_failures: u64,
    /// 当前连续提交失败次数（成功一次即清零）。
    #[serde(default)]
    pub present_failures_consecutive: u32,
    /// 当前帧是否**真的**在走 DMA-BUF 直通（`false` = 已回退系统内存）。
    ///
    /// 判"零拷贝到底有没有生效"只能看它：`decoder` 字符串里的 `dmabuf`
    /// 只说明**意图**（构造期候选），回退后字符串未必同步刷新。
    #[serde(default)]
    pub is_dmabuf: bool,
    /// 构造期因候选失败/被拒而回退到系统内存的次数（0 = 首次候选就成功）。
    #[serde(default)]
    pub dmabuf_fallbacks: u64,
    /// 直通缓冲保活列表长度（`0` = 没在用直通）。
    #[serde(default)]
    pub dmabuf_keepalive_len: usize,
    /// **解码告警**（`None` = 没有告警）。
    ///
    /// 目前只有一类：源分辨率超过 4096 像素宽且编码是 H.264 —— NVDEC 的
    /// H.264 硬解上限是 4K，8K H.264 只能软解（实测 0.086–0.69 fps、
    /// CPU 166–186%、RSS 2.8–3.2 GB），此时这里给出结论 + 建议（HEVC 版本）。
    /// 不是"暂停/失败"：画面依然在走，只是**用户必须知道为什么这么卡**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_alert: Option<String>,
}

/// 显示侧提交计数（**不是"上屏"证据**）。
///
/// 语义（与 `liuguang_display::CommitStats` 一致）：
/// - `commit_ok`：**阻塞**原子提交直接返回 `Ok` 的次数——只说明内核接受了提交，
///   **不代表该帧已扫描输出**；
/// - `commit_success_total = commit_ok + retried_after_busy + legacy_fallback`
///   （`commit_ok` **不含**重试成功的那部分）；
/// - `failed`：该帧未被提交成功（>0 必须让用户可见）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayCommitStatus {
    /// 直接成功返回 `Ok` 的提交次数（不代表已上屏）。
    pub commit_ok: u64,
    /// `EBUSY` 重试后成功的次数。
    pub retried_after_busy: u64,
    /// 走 legacy `set_crtc` 成功回退的次数。
    pub legacy_fallback: u64,
    /// 该帧未提交成功的次数。
    pub failed: u64,
    /// 成功提交总数 = `commit_ok + retried_after_busy + legacy_fallback`。
    pub commit_success_total: u64,
    /// 总提交尝试次数。
    pub attempts: u64,
}

/// DRM master 状态（"未取得 master ⇒ 画面不会更新"对外可见）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayMasterStatus {
    /// 是否持有 DRM master。`Some(false)` = **已确定不是** ⇒ 任何提交都不会生效。
    pub master: Option<bool>,
    /// 显式获取尝试的结果（含内核 errno 语义）。
    pub acquire_result: String,
    /// 打开该设备的其它进程（尽力而为：当前用户可见范围内）。
    pub holders: Vec<String>,
    /// 面向运维的可操作提示（含"先停止占用进程"的具体命令）。
    pub remedy_hint: String,
}

/// 单输出通道状态（M6 多屏）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelStatus {
    /// 输出名。
    pub output: String,
    /// 通道顶层状态名。
    pub state: String,
    /// 当前媒体路径。
    pub media: Option<String>,
    /// 当前模式。
    pub mode: Option<String>,
    /// 是否暂停。
    pub paused: bool,
    /// 媒体供给方式（static/video(vaapi|software|mock)/none）。
    pub decoder: String,
    /// 视频通道统计（仅视频通道）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoStatsStatus>,
    /// 是否"只放这一项"（`true` = 不跟随轮换）。
    #[serde(default)]
    pub pinned: bool,
    /// 暂停原因（`None` = 未暂停或原因未知）。
    ///
    /// 取值由 daemon 定义、客户端**原样展示**：`"user"`（用户暂停）、
    /// `"screen_off"`（显示器熄灭/断开）、`"fullscreen"`（有窗口全屏）。
    /// 用字符串而不是枚举，理由与 `playback_source` 相同：新 daemon 加原因时
    /// 旧客户端不会解析失败。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
}

/// `status.fullscreen`：当前占据屏幕的全屏窗口（`None` = 没有/检测不到）。
///
/// 只在合成器支持 `zwlr_foreign_toplevel_management_unstable_v1` 时才有值，
/// 因此客户端**不得**把 `None` 当成"确认没有游戏在跑"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullscreenInfo {
    /// 应用 id（可能缺失：有的合成器晚发或从不发）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// 窗口标题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 该窗口声称所在的输出名。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    /// 因为全屏而被暂停的通道数（0 = 检测到全屏但没有视频在播）。
    #[serde(default)]
    pub paused_channels: usize,
}

/// `status` 结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResult {
    /// 顶层状态机状态名（§8.3）。
    pub state: String,
    /// 当前输出名。
    pub output: Option<String>,
    /// 当前媒体路径。
    pub media: Option<String>,
    /// 是否处于暂停状态。
    pub paused: bool,
    /// 显示后端标识（drm/mock）。
    pub display_backend: String,
    /// 媒体供给方式（static/vaapi/software/none）。
    pub decoder: String,
    /// 当前显示模式（如 1920x1080@60.000）。
    pub mode: Option<String>,
    /// daemon 运行时长（秒）。
    pub uptime_seconds: u64,
    /// 各输出通道状态（M6；单屏时恰一项）。
    #[serde(default)]
    pub channels: Vec<ChannelStatus>,
    /// 当前全屏窗口（`None` = 没有，或合成器不提供该信息）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<FullscreenInfo>,
    /// 显示侧提交计数（`None` = 该后端不提供，如 mock）。
    ///
    /// **不是"上屏"证据**：`commit_ok` 只表示内核接受了阻塞原子提交；
    /// 成功总数 = `commit_ok + retried_after_busy + legacy_fallback`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<DisplayCommitStatus>,
    /// DRM master 状态（`None` = 该后端不适用/不提供）。
    ///
    /// `master == Some(false)` ⇒ **已确定不是 master，任何提交都不会生效、画面不会更新**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<DisplayMasterStatus>,
    /// 当前**播放源**：`"static"` | `"playlist:<id>"` | `"directory"`。
    ///
    /// 用**字符串**而不是新类型层级：取值域由 daemon 定义，客户端**原样展示**；
    /// 遇到未知取值**不得 panic/拒绝**（旧客户端要能容忍新 daemon，反之亦然）。
    ///
    /// `None` = 该 daemon 未报告（旧报文）。`#[serde(default)]` ⇒ **旧报文仍可解析**；
    /// `skip_serializing_if` ⇒ 新 daemon 未报告时**不会**给旧客户端塞未知字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_source: Option<String>,
    /// **降级原因的人话说明**（`None` = 正常态，无说明）。
    ///
    /// 用户过去看到 `DegradedStatic` **无法分辨**两种完全不同的情况：
    /// **环境暂时不可用**（DRM master 不可用，自愈后会自动恢复）与
    /// **帧/平面真有错**（不会自动重开）。daemon 用**结构性事实**
    /// （是否存在待恢复的视频通道）判定，客户端原样展示即可。
    ///
    /// `#[serde(default)]` ⇒ 旧报文仍可解析；`skip_serializing_if` ⇒ 正常态不塞字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_hint: Option<String>,
}

/// 单通道的**对账计数**（媒体侧出队 vs 显示侧提交结果）。
///
/// `presented` 增长而 `present_ok` 不增长、`present_failures` 增长 ⇒ **异常签名**
/// （帧被取出但没提交成功）。这是把"两个计数互相对账"变成可断言检查的载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMetrics {
    /// 输出名。
    pub output: String,
    /// 媒体侧已出队帧数（**不等于上屏**）。
    pub presented: u64,
    /// 显示侧成功提交帧数。
    pub present_ok: u64,
    /// 显示侧提交失败帧数。
    pub present_failures: u64,
    /// 当前连续提交失败次数。
    pub present_failures_consecutive: u32,
    /// 当前帧是否真的在走 DMA-BUF 直通（`false` = 已回退系统内存）。
    #[serde(default)]
    pub is_dmabuf: bool,
    /// 直通回退次数。
    #[serde(default)]
    pub dmabuf_fallbacks: u64,
    /// 直通缓冲保活长度。
    #[serde(default)]
    pub dmabuf_keepalive_len: usize,
}

impl ChannelMetrics {
    /// 异常签名：媒体侧说取了帧，但显示侧有提交失败。
    #[must_use]
    pub fn contradicts(&self) -> bool {
        self.presented > 0 && self.present_failures > 0
    }
}

/// 媒体来源条目（daemon 是唯一权威：路径已规范化并通过根边界校验）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaSource {
    /// 稳定 id（规范化后的绝对目录路径）。
    pub id: String,
    /// 人类可读名（目录名）。
    pub name: String,
    /// 来源类型：`images` / `videos` / `legacy`（旧 `slideshow.directory`）。
    pub kind: String,
    /// 目录当前是否存在（缺失来源只 WARN 跳过，仍会列出并标记）。
    pub available: bool,
}

/// `list_sources` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSourcesResult {
    /// 媒体根目录（规范化后）。
    pub root: String,
    /// 全部来源（含缺失项，`available=false`）。
    pub sources: Vec<MediaSource>,
    /// 因缺失被跳过的来源数（诊断用）。
    pub missing: usize,
}

/// `list_media` 的 `filter` 语义（**词汇表在协议层**，daemon 与客户端共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaFilterSpec {
    /// 不筛选：`filter` 缺省、空串、或旧客户端的 `"all"`。
    Any,
    /// 仅该类型（`image` / `video`）。
    Kind(&'static str),
}

/// `list_media.filter` 的唯一合法值集合。
///
/// **容错 + 校验**（T39-F1 的根治）：
/// - `None` / `""` / `"all"` ⇒ [`MediaFilterSpec::Any`]（不筛选）；
/// - `"image"` / `"video"` ⇒ 该类型；
/// - **其它值 ⇒ `Err`**，调用方必须返回 `INVALID_PARAMS`。
///
/// 旧实现把任意未知值当"精确比较"⇒ `total=0` 且 `ok=true`，
/// 即"**一个无效参数被当成合法的空结果**"——那正是本次 bug 的形态。
///
/// # Errors
///
/// 返回可读的非法值说明（调用方据此构造 `INVALID_PARAMS`）。
pub fn parse_media_filter(raw: Option<&str>) -> Result<MediaFilterSpec, String> {
    let v = raw.map(str::trim).filter(|s| !s.is_empty());
    match v {
        None | Some("all") => Ok(MediaFilterSpec::Any),
        Some("image") => Ok(MediaFilterSpec::Kind("image")),
        Some("video") => Ok(MediaFilterSpec::Kind("video")),
        Some(other) => Err(format!(
            "非法 filter 取值 {other:?}：合法值为 \"all\"（等价于不传）/ \"image\" / \"video\""
        )),
    }
}

/// `list_media` 请求参数（**分页 + 过滤**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMediaParams {
    /// 页码（从 0 开始）。
    pub page: usize,
    /// 每页条数（服务端上限 `MAX_MEDIA_PAGE_SIZE`）。
    pub page_size: usize,
    /// 仅列出该来源（`MediaSource::id`）。
    #[serde(default)]
    pub source_id: Option<String>,
    /// 名称子串过滤（大小写不敏感）。
    #[serde(default)]
    pub search: Option<String>,
    /// 类型过滤（**缺省 = 不筛选**）：`image` / `video`。
    ///
    /// 空串与 `"all"` 被容忍为"不筛选"；**其它值**必须被服务端拒绝为
    /// `INVALID_PARAMS`（见 [`parse_media_filter`]）——不得静默返回空结果。
    ///
    /// **`None` 时不下发该字段**（缺省即"不筛选"）：旧客户端曾恒发 `"all"`，
    /// 而服务端对 `kind` 精确比较 ⇒ 恒 `total=0`（T39-F1）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// 排序：`name`（默认）/ `size`。
    #[serde(default)]
    pub sort: Option<String>,
}

/// 媒体的**展示元数据**（GUI 属性面板 / 悬停信息用）。
///
/// 为什么是 `Option`：这些值来自扫描时的**轻量文件头解析**——容器不认识、文件
/// 损坏、编码不在支持列表里，都可能解析不出来。此时宁可不给（`None`），也不猜
/// 一个假的宽高（GUI 拿错尺寸会把封面裁糊，比"没有信息"更糟）。
///
/// `fps_millihz` 用**千分之一赫兹**的整数（`29970` = 29.97 fps）：协议层不放
/// `f64`，避免 NaN / 精度 / 跨语言序列化差异。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDetails {
    /// 像素宽。
    pub width: u32,
    /// 像素高。
    pub height: u32,
    /// 帧率（千分之一赫兹；图片或未知 = `None`）。
    #[serde(default)]
    pub fps_millihz: Option<u32>,
    /// 时长（毫秒；图片或未知 = `None`）。
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// 编码名（`h264` / `hevc` / `av1` / `vp9` / `png` / `jpeg` …；未知 = `None`）。
    #[serde(default)]
    pub codec: Option<String>,
    /// 像素格式（如 `yuv420p` / `yuv420p10le`；只有容器里**能直接读到**时才给）。
    #[serde(default)]
    pub pix_fmt: Option<String>,
}

/// 单条媒体项（**不含文件内容**，只有 TUI/GUI 需要的元数据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaItem {
    /// 稳定 id（规范化后的绝对路径）。
    pub id: String,
    /// 所属来源 id。
    pub source_id: String,
    /// 文件名。
    pub name: String,
    /// 绝对路径（规范化后）。
    pub path: String,
    /// `image` / `video`。
    pub kind: String,
    /// 字节数。
    pub size_bytes: u64,
    /// 秒级修改时间（Unix epoch）。
    pub modified_unix: i64,
    /// 展示元数据（宽高/帧率/时长/编码）。
    ///
    /// `#[serde(default)]`：**旧客户端**的响应里没有这个字段，缺省即 `None`；
    /// 反过来本版 daemon 也不会因为字段缺省而拒绝旧配置/旧测试载荷。
    /// 解析不出来时**不出现**（`skip_serializing_if`），让载荷保持紧凑。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<MediaDetails>,
}

/// `list_media` 结果（分页元信息与总数一起返回，TUI 无需再猜）。
///
/// # 已知限制（T25-F3，如实记录，**未修复**）
///
/// 索引规模到 **10,010 条**时，单次 `list_media` 在真机上耗时 **79.6–101.2 ms**
/// （判据 <500 ms，**通过**）。但 TUI 是**同步**调用：一次查询会占住 UI 线程
/// ~0.1 s，在万条规模下翻页/搜索会有**可感知停顿**。
/// 目前**不做**优化（分页已把单次载荷限到 200 条；瓶颈在服务端全量过滤/排序），
/// 后续若要做，方向是把过滤/排序下推到增量索引或给 TUI 走异步查询。
/// serde 判定：`usize` 为 `0` 时整个字段不出现
/// （`skip_serializing_if` 用；旧客户端/旧日志的载荷保持逐字节一致）。
fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMediaResult {
    /// 当前页条目。
    pub items: Vec<MediaItem>,
    /// 过滤后的总条数。
    pub total: usize,
    /// 当前页码（原样回显）。
    pub page: usize,
    /// 每页条数（已按服务端上限裁剪）。
    pub page_size: usize,
    /// 扫描是否因上限被截断（不隐瞒）。
    #[serde(default)]
    pub truncated: bool,
    /// 是否**正在扫描**（`true` 时本页可能是上一代索引的数据或空）。
    ///
    /// 扫描是异步的（`t22`：有界后台任务）。若为 `true`，TUI 应显示"扫描中"，
    /// 并在收到 `SCAN_FINISHED` 后重新拉取本方法。
    #[serde(default)]
    pub scanning: bool,
    /// 本页数据所属的扫描代次（`0` = 还没有任何扫描完成）。
    #[serde(default)]
    pub scan_id: u64,
    /// 索引生成时刻（Unix 毫秒；`0` = 未生成）。
    #[serde(default)]
    pub generated_unix_ms: u64,
    /// 因**超过单文件体积上限**（`[library].max_file_bytes`）被跳过的文件数。
    ///
    /// 这不是错误，但**必须可见**：旧默认上限（512 MiB）把 8K 原始素材
    /// （900 MB+）挡在索引外时，用户只看到"文件不在库里"。
    /// `0` = 没有被跳过的文件（此时整个字段省略，旧载荷形状不变）。
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub skipped_oversize: usize,
    /// 被体积上限跳过的样例（最多 3 条，含实际上限与路径）。
    ///
    /// 单独给样例是因为"数量"不可操作，"哪些 + 上限多少"才可操作
    /// （调大 `[library].max_file_bytes` 或转码成更小的文件）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_examples: Vec<String>,
}

/// `get_media` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetMediaParams {
    /// 媒体 id（规范化后的绝对路径）。
    pub id: String,
}

/// 本机硬件能不能直接播这个素材（t15 的兼容性分类，线上值为 `snake_case`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatState {
    /// 本机可以直接播（硬解可用）。
    Playable,
    /// 硬解不了，但**能转码成可播的缓存**（daemon 会自动做）。
    NeedsConversion,
    /// 连转码也救不了（例如宽度超出 8K 上限）：只能换素材。
    Unsupported,
}

/// 单个素材的兼容性结论（`get_media` 的附加字段）。
///
/// **纯增量**：旧客户端不认这个字段照旧工作（`serde` 默认忽略未知字段），
/// 老 daemon 也不会发它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatInfo {
    /// 分类结论。
    pub state: CompatState,
    /// 面向人的原因（为什么需要转换 / 为什么不可用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 转码目标（如 `hevc 8bit ≤4096`；`Playable`/`Unsupported` 时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// `get_media` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetMediaResult {
    /// 条目。
    pub item: MediaItem,
    /// 本机硬件兼容性分类（t15）。
    ///
    /// 放在**响应顶层**而不是塞进 [`MediaDetails`]：后者是
    /// "从文件头读出来的事实"，兼容性是"相对本机硬件与配置的判断"，两者生命周期
    /// 不同（换机器/改 `[media].transcode_max_width` 都会让同一份 details 得到
    /// 不同结论）。顶层字段也保证旧的 `details` 载荷逐字节不变。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<CompatInfo>,
}

/// `prepare_media` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareMediaParams {
    /// 媒体路径（规范化后的绝对路径，与 `get_media` 同一命名空间）。
    pub path: String,
    /// 即使缓存已存在也重新转码（省略 = `false`）。
    #[serde(default)]
    pub force: Option<bool>,
}

/// 转码任务状态（线上值为 `snake_case`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrepareState {
    /// 已入队，还没开始跑 ffmpeg。
    Queued,
    /// 正在转码（`progress_permille` 有意义）。
    Running,
    /// 缓存可用了（`cache_path` 给路径）。
    Ready,
    /// 失败（`message` 给可操作原因）。
    Failed,
}

/// `prepare_media` 结果（也是转码进度的查询结果）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareMediaResult {
    /// 任务状态。
    pub state: PrepareState,
    /// 缓存文件路径（`state = ready` 时给出；源文件保持不变）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,
    /// 进度（千分比，0..=1000；`ready` 恒 1000，`queued` 为 0）。
    #[serde(default)]
    pub progress_permille: u32,
    /// 面向人的说明（失败原因 / 是否需要转换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `get_thumbnail` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetThumbnailParams {
    /// 媒体 id（规范化后的绝对路径，与 `get_media` 同一命名空间）。
    pub id: String,
    /// 缩略图最大宽度（省略 = 320）。
    #[serde(default)]
    pub max_width: Option<u32>,
    /// 缩略图最大高度（省略 = 200）。
    #[serde(default)]
    pub max_height: Option<u32>,
    /// **取哪一时刻的画面**（毫秒，自视频开头算；省略 = `0` = 首帧）。
    ///
    /// GUI 的悬停动效要取多点帧（例如 0/25%/50%/75%），所以这里必须能定点：
    /// 视频走 seek 到该时刻再取一帧，图片忽略此参数（图片只有一帧）。
    /// 越界（超过时长）时由 daemon 决定落点——实现上按"seek 到该处取首个可用帧"，
    /// 播到尾端会自然得到末帧，而不是报错。
    #[serde(default)]
    pub at_ms: Option<u64>,
}

/// `get_thumbnail` 结果。
///
/// 图像数据是 **PNG**（daemon 侧编码，GUI 用 gdk-pixbuf 解码），base64 放进
/// JSON 是为了保持协议「一行 JSON 一帧」的形态，不引入二进制分帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetThumbnailResult {
    /// 媒体 id。
    pub id: String,
    /// 像素宽。
    pub width: u32,
    /// 像素高。
    pub height: u32,
    /// 编码格式（当前恒为 `"png"`）。
    pub format: String,
    /// base64 编码的图像数据。
    pub data_b64: String,
}

/// `scan_source` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScanSourceParams {
    /// 只扫描该来源（省略 = 全部来源）。
    #[serde(default)]
    pub source_id: Option<String>,
    /// 是否丢弃旧索引（`false` 仍会重建索引，但保留已有条目的元数据以少刷屏）。
    #[serde(default)]
    pub full: bool,
}

/// `scan_source` 结果。
///
/// `accepted=true` **只表示后台任务已排入**；进度与结果经
/// `SCAN_PROGRESS` / `SCAN_FINISHED` 事件返回（`scan_id` 用于对账）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanSourceResult {
    /// 是否受理（恒为 true；失败走错误响应）。
    pub accepted: bool,
    /// 本次扫描代次（事件里原样回传）。
    pub scan_id: u64,
    /// 是否已有扫描在跑（`true` 时 `scan_id` 是新代次，旧代次的结果会被丢弃）。
    pub superseded_running: bool,
}

/// `rescan_library` 结果。
///
/// 与 `scan_source` 一样只表示**已排入后台**；进度与结果经
/// `SCAN_PROGRESS` / `SCAN_FINISHED` 事件返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescanLibraryResult {
    /// 本次扫描代次（事件里原样回传）。
    pub scan_id: u64,
    /// 是否取代了正在进行的扫描。
    pub superseded_running: bool,
}

/// `add_source` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddSourceParams {
    /// `image` / `video`。
    pub kind: String,
    /// **服务器绝对路径**（daemon 侧规范化 + 根边界校验）。
    pub path: String,
    /// 是否递归。
    #[serde(default)]
    pub recursive: bool,
}

/// `add_source` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddSourceResult {
    /// 新来源 id（规范化后的绝对路径）。
    pub source_id: String,
    /// 是否新建（`false` = 已存在，幂等）。
    pub created: bool,
}

/// `remove_source` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveSourceParams {
    /// 来源 id。
    pub source_id: String,
}

/// `delete_media` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteMediaParams {
    /// 媒体 id（规范化后的绝对路径，与 `get_media` / `get_thumbnail` 同一命名空间）。
    ///
    /// ⚠️ **不删磁盘文件**：这个路径只用于在媒体库索引里定位条目。
    pub path: String,
}

/// `delete_media` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteMediaResult {
    /// 是否真的从索引里移除了（`false` = 本来就不在索引里，幂等）。
    pub removed: bool,
    /// 移除的条目数（路径唯一，因此实际只会是 `0` 或 `1`）。
    pub removed_count: usize,
    /// 因为"正在播放它"而被切走的输出名。
    #[serde(default)]
    pub switched_outputs: Vec<String>,
}

/// 播放列表条目（`update_playlist` 用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    /// 媒体 id（规范化后的绝对路径）。
    pub media_id: String,
}

/// 播放列表（`list_playlists` / `update_playlist` 结果）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    /// 播放列表 id（`playlist_id` 缺省新建时由 daemon 生成）。
    pub id: String,
    /// 名称。
    pub name: String,
    /// 条目。
    pub entries: Vec<PlaylistEntry>,
    /// 轮换模式：`sequential` / `random`。
    pub mode: String,
    /// 切换间隔（秒）。
    pub interval_seconds: u32,
    /// 视频结束行为：`next` / `loop`。
    pub on_video_end: String,
    /// 目标输出（`None` = 全部/默认调度）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 是否不重复。
    #[serde(default)]
    pub no_repeat: bool,
}

/// `list_playlists` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPlaylistsResult {
    /// 全部播放列表。
    pub playlists: Vec<Playlist>,
}

/// `update_playlist` 请求参数（`playlist_id` 缺省 = 新建）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePlaylistParams {
    /// 播放列表 id（省略 = 新建）。
    #[serde(default)]
    pub playlist_id: Option<String>,
    /// 名称。
    pub name: String,
    /// 条目。
    #[serde(default)]
    pub entries: Vec<PlaylistEntry>,
    /// `sequential` / `random`。
    #[serde(default = "default_playlist_mode")]
    pub mode: String,
    /// 间隔（秒）。
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: u32,
    /// `next` / `loop`。
    #[serde(default = "default_on_video_end")]
    pub on_video_end: String,
    /// 目标输出。
    #[serde(default)]
    pub output: Option<String>,
    /// 是否不重复。
    #[serde(default)]
    pub no_repeat: bool,
}

fn default_playlist_mode() -> String {
    "sequential".to_string()
}

/// `delete_playlist` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePlaylistParams {
    /// 播放列表 id。
    ///
    /// ⚠️ **不删磁盘文件**：只删这条播放列表定义，条目引用的媒体项原样保留。
    pub playlist_id: String,
}

/// `delete_playlist` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePlaylistResult {
    /// 是否真的删掉了（`false` = 本来就不存在，幂等）。
    pub removed: bool,
    /// 被删除的播放列表 id（原样回显，便于 GUI 对账）。
    pub playlist_id: String,
    /// 因为该播放列表消失而**重建了播放源**的输出名。
    #[serde(default)]
    pub rebuilt_outputs: Vec<String>,
}

fn default_interval_seconds() -> u32 {
    30
}

fn default_on_video_end() -> String {
    "next".to_string()
}

/// `set_output_transform` 请求参数（**只改运行时**）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SetOutputTransformParams {
    /// 目标输出（省略 = 主输出）。
    #[serde(default)]
    pub output: Option<String>,
    /// `cover` / `contain` / `stretch` / `center`。
    #[serde(default)]
    pub fit: Option<String>,
    /// 缩放倍数。
    #[serde(default)]
    pub zoom: Option<f32>,
    /// 水平偏移（像素）。
    #[serde(default)]
    pub offset_x: Option<f32>,
    /// 垂直偏移（像素）。
    #[serde(default)]
    pub offset_y: Option<f32>,
    /// 旋转：`0` / `90` / `180` / `270`。
    #[serde(default)]
    pub rotation: Option<u32>,
    /// 亮度倍率（`0.1..=1.5`）。
    #[serde(default)]
    pub brightness: Option<f32>,
}

/// `save_config` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveConfigResult {
    /// **字节是否已写出**（`save_atomic` 返回 Ok）。
    ///
    /// ⚠️ 这**不等于**"你的改动进了配置"：旧实现只写 `self.config`、不合并运行期的
    /// `add_source`/`remove_source`，于是 `saved=true` 而新增来源根本没落盘
    /// （T25-F1，真机复现：重启后 `list_sources` 只剩 root）。
    /// **判断"改动是否真的进了配置"请看 [`SaveConfigResult::verified`]。**
    pub saved: bool,
    /// **回读校验**：把刚写出的文件重新解析后，"来源集合"是否与运行时完全一致。
    ///
    /// 只有它为 `true`，调用方才能把 UI 从"已受理"推进到"已保存"。
    /// 旧客户端忽略本字段仍然兼容（`serde(default)`）。
    #[serde(default)]
    pub verified: bool,
    /// 本次保存合并进配置的运行期来源改动条数（新增 + 删除；0 = 无改动）。
    #[serde(default)]
    pub merged_sources: usize,
    /// 写入路径。
    pub path: String,
    /// 字节数。
    pub bytes: usize,
}

/// `list_media` 单页上限（防止一次把数万条元数据推给 TUI）。
pub const MAX_MEDIA_PAGE_SIZE: usize = 200;

/// 单次扫描的条目上限（有界；超出即 `truncated=true`）。
pub const MAX_SCAN_ENTRIES: usize = 20_000;

/// `get_metrics` 结果：显示侧提交计数 + DRM master 状态 + 逐通道对账计数。
///
/// 与 `status` 分开：`status` 保持稳定（TUI 已依赖其结构），
/// 本方法提供**诊断/对账**所需的计数，且新增字段不会破坏既有客户端。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsResult {
    /// 逐通道对账计数。
    pub channels: Vec<ChannelMetrics>,
    /// 进程 CPU 占用（单核百分比；100 = 跑满一核）。首次采样为 0（无基线）。
    #[serde(default)]
    pub cpu_percent: f32,
    /// 常驻内存（字节）。
    #[serde(default)]
    pub mem_bytes: u64,
    /// 线程数。
    #[serde(default)]
    pub threads: u32,
    /// **提交成功**帧率（`presented_ok` 增量 ÷ 墙钟；不是解码出队帧率）。
    #[serde(default)]
    pub fps: f32,
    /// 守护进程已运行秒数。
    #[serde(default)]
    pub uptime_seconds: u64,
    /// 显示侧提交计数（`None` = 该后端不提供，如 mock）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<DisplayCommitStatus>,
    /// DRM master 状态（`None` = 该后端不适用/不提供）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<DisplayMasterStatus>,
    /// 预加载缓存当前占用的**解码后**缓冲字节数（`performance.max_cache_mb` 的实测用量）。
    #[serde(default)]
    pub preload_cache_bytes: u64,
    /// 预加载缓存条目数。
    #[serde(default)]
    pub preload_cache_items: u32,
    /// CPU 压力与降载状态（`None` = `performance.reduce_load_on_cpu_pressure = false`
    /// 或尚未采样）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pressure: Option<CpuPressureStatus>,
    /// 累计缩略图请求数（t39；含命中、解码、排队与 BUSY 拒绝）。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub thumb_requests: u64,
    /// 其中由内存/磁盘缓存直接命中的次数（含同 key 在途合并）。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub thumb_hits: u64,
    /// 其中缓存未命中（需要解码或排队）的次数。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub thumb_misses: u64,
    /// 实际解码的次数（同 key 的多张请求只解码一次）。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub thumb_decoded: u64,
    /// 单次解码的平均墙钟耗时（毫秒；只统计解码次数，不含缓存命中）。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub thumb_avg_ms: u64,
    /// 当前排队的解码任务数（`0` = 空闲；上限见 `ErrorKind::Busy`）。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub thumb_queue_depth: u32,
}

/// `u64` 是否为 0（serde 跳过零值字段用）。
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// `u32` 是否为 0（serde 跳过零值字段用）。
fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// CPU 压力读数与其触发的降载档位（`get_metrics` 诊断用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuPressureStatus {
    /// 归一化压力（0..=100）。
    pub percent: f32,
    /// 读数口径：`"psi"`（`/proc/pressure/cpu` 的 `some avg10`）或
    /// `"loadavg"`（1 分钟均载 ÷ 在线核数；**更钝**，见 `apps/liuguang/src/loadshed.rs`）。
    pub source: String,
    /// 是否处于降载档（视频提交频率已下调、预加载已暂停）。
    pub shedding: bool,
}

/// 单输出状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputStatus {
    /// 输出名（如 HDMI-A-1）。
    pub name: String,
    /// 是否已连接。
    pub connected: bool,
    /// 当前模式描述（如 1920x1080@60.000）。
    pub mode: Option<String>,
    /// 支持的模式数。
    pub mode_count: usize,
}

/// `outputs` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputsResult {
    /// 全部输出。
    pub outputs: Vec<OutputStatus>,
}

/// 事件种类（`subscribe` 推送流）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventKind {
    /// 顶层状态变化。
    StateChanged,
    /// 输出连接/断开。
    OutputChanged,
    /// 当前媒体变化。
    MediaChanged,
    /// 错误/降级事件。
    Error,
    /// **壁纸真的应用完成**（与 `set_wallpaper` 的"受理"分离）。
    WallpaperApplied,
    /// 后台扫描进度。
    ScanProgress,
    /// 后台扫描结束。
    ScanFinished,
    /// 硬件解不动的素材**转码完成**（t15）：`media` 是源路径，`cache_path` 是缓存。
    ///
    /// daemon 收到 `set_wallpaper` 回 `NEEDS_PREPARATION` 后自动开转码，转完推本
    /// 事件并**自动播放缓存**。失败时推 `ERROR`（带可操作原因），不推本事件。
    MediaPrepared,
}

/// `subscribe` 事件流中的推送消息。
///
/// 以 NDJSON 独立帧推送（不是 `Response` 结构）；`v` 为协议版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventMessage {
    /// 协议版本。
    pub v: u32,
    /// 事件种类。
    pub kind: EventKind,
    /// 事件发生时刻（Unix 毫秒）。
    pub at_ms: u64,
    /// 状态名（StateChanged 时给出）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// 输出名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 媒体路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    /// 面向人的说明（Error 时给出）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 请求 ID（`WALLPAPER_APPLIED` 与 `set_wallpaper` 的受理响应配对）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// 是否成功（`WALLPAPER_APPLIED`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// 失败原因（`WALLPAPER_APPLIED` 失败时给）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 稳定错误码（`Error` 事件用，如 `PERMISSION_DENIED`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// 输出是否已连接（`OUTPUT_CHANGED`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
    /// 当前模式（`OUTPUT_CHANGED`，如 `3840x2400@60.000`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// 扫描代次（`SCAN_*`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_id: Option<u64>,
    /// 已发现条目数（`SCAN_*`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found: Option<u64>,
    /// 扫描是否已结束（`SCAN_PROGRESS` 恒 false）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    /// 扫描过程中累计的错误（最多保留前若干条；`SCAN_FINISHED` 给出）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// 转码缓存路径（`MEDIA_PREPARED` 给出；源文件保持不变）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,
}

impl EventMessage {
    /// 构造事件（自动带当前 Unix 毫秒）。
    pub fn new(kind: EventKind) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            kind,
            at_ms: unix_ms(),
            state: None,
            output: None,
            media: None,
            message: None,
            request_id: None,
            ok: None,
            error: None,
            code: None,
            connected: None,
            mode: None,
            scan_id: None,
            found: None,
            done: None,
            errors: Vec::new(),
            cache_path: None,
        }
    }

    /// 附加状态名。
    pub fn with_state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    /// 附加输出名。
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = Some(output.into());
        self
    }

    /// 附加媒体路径。
    pub fn with_media(mut self, media: impl Into<String>) -> Self {
        self.media = Some(media.into());
        self
    }

    /// 附加说明。
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// 附加请求 ID。
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// 附加成功标志。
    pub fn with_ok(mut self, ok: bool) -> Self {
        self.ok = Some(ok);
        self
    }

    /// 附加失败原因。
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    /// 附加稳定错误码。
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// 附加连接状态。
    pub fn with_connected(mut self, connected: bool) -> Self {
        self.connected = Some(connected);
        self
    }

    /// 附加模式串。
    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = Some(mode.into());
        self
    }

    /// 附加扫描代次与已发现条目数。
    pub fn with_scan(mut self, scan_id: u64, found: u64, done: bool) -> Self {
        self.scan_id = Some(scan_id);
        self.found = Some(found);
        self.done = Some(done);
        self
    }

    /// 附加扫描错误列表。
    pub fn with_errors(mut self, errors: Vec<String>) -> Self {
        self.errors = errors;
        self
    }

    /// 附加转码缓存路径（`MEDIA_PREPARED`）。
    pub fn with_cache_path(mut self, cache_path: impl Into<String>) -> Self {
        self.cache_path = Some(cache_path.into());
        self
    }
}

/// 当前 Unix 时间（毫秒）。
#[must_use]
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn media_filter_vocabulary_is_tolerant_and_validating() {
        use super::{MediaFilterSpec, parse_media_filter};
        // 缺省 / 空串 / 旧客户端的 "all" ⇒ 不筛选（容错）
        assert_eq!(parse_media_filter(None), Ok(MediaFilterSpec::Any));
        assert_eq!(parse_media_filter(Some("")), Ok(MediaFilterSpec::Any));
        assert_eq!(parse_media_filter(Some("   ")), Ok(MediaFilterSpec::Any));
        assert_eq!(parse_media_filter(Some("all")), Ok(MediaFilterSpec::Any));
        // 合法类型
        assert_eq!(
            parse_media_filter(Some("image")),
            Ok(MediaFilterSpec::Kind("image"))
        );
        assert_eq!(
            parse_media_filter(Some("video")),
            Ok(MediaFilterSpec::Kind("video"))
        );
        // **未知值必须报错**（不得静默当作空结果）——T39-F1
        for bad in ["bogus", "IMAGE", "images", "all ", "video,image"] {
            let err = parse_media_filter(Some(bad));
            if bad == "all " {
                // 前后空白被规范化 ⇒ 容忍
                assert_eq!(err, Ok(MediaFilterSpec::Any), "{bad:?} 应被容忍");
            } else {
                assert!(err.is_err(), "{bad:?} 必须被拒绝，而不是当成空结果");
            }
        }
    }

    use super::*;

    #[test]
    fn request_round_trip() {
        let req = Request::new(
            "req-001",
            Method::SetWallpaper,
            serde_json::json!({
                "output": "HDMI-A-1",
                "path": "/srv/wallpapers/aurora.png",
                "fit": "cover"
            }),
        );
        let text = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&text).unwrap();
        assert_eq!(back, req);
        assert!(back.version_supported());
        // 线上方法名是 snake_case。
        assert!(text.contains("\"set_wallpaper\""));
    }

    #[test]
    fn response_shapes_match_spec_examples() {
        let ok = Response::ok("req-001", serde_json::json!({"accepted": true}));
        let text = serde_json::to_string(&ok).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({
                "v": 1,
                "id": "req-001",
                "ok": true,
                "result": {"accepted": true}
            })
        );

        let err = Response::err(
            "req-001",
            ErrorBody::new(
                ErrorKind::UnsupportedCodec,
                "No usable video decoder for this file",
            ),
        );
        let text = serde_json::to_string(&err).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!({
                "v": 1,
                "id": "req-001",
                "ok": false,
                "error": {
                    "code": "UNSUPPORTED_CODEC",
                    "message": "No usable video decoder for this file"
                }
            })
        );
    }

    #[test]
    fn unknown_method_is_rejected_at_parse_level() {
        let line = r#"{"v":1,"id":"x","method":"destroy_world","params":{}}"#;
        assert!(serde_json::from_str::<Request>(line).is_err());
        // 但必须能被识别为"未知方法"，而不是"消息非法"。
        assert_eq!(Method::from_wire("destroy_world"), None);
    }

    /// 线上方法名与枚举必须一一对应（`from_wire` / `wire_name` 单一事实来源）。
    #[test]
    fn method_wire_names_round_trip() {
        let all = [
            Method::Status,
            Method::Outputs,
            Method::SetWallpaper,
            Method::Next,
            Method::Pause,
            Method::Resume,
            Method::Reload,
            Method::Transition,
            Method::Subscribe,
            Method::GetMetrics,
            Method::ListSources,
            Method::ListMedia,
            Method::GetMedia,
            Method::GetThumbnail,
            Method::PrepareMedia,
            Method::ScanSource,
            Method::RescanLibrary,
            Method::SetPinned,
            Method::AddSource,
            Method::RemoveSource,
            Method::DeleteMedia,
            Method::ListPlaylists,
            Method::UpdatePlaylist,
            Method::DeletePlaylist,
            Method::SetOutputTransform,
            Method::SaveConfig,
            Method::SetStaticWallpaper,
            Method::ClearStaticWallpaper,
        ];
        for m in all {
            assert_eq!(
                Method::from_wire(m.wire_name()),
                Some(m),
                "{} 的线上名无法解析回来",
                m.wire_name()
            );
            // 与 serde 表示一致。
            let via_serde = serde_json::to_value(m).unwrap();
            assert_eq!(via_serde, serde_json::json!(m.wire_name()));
        }
        assert_eq!(Method::from_wire(""), None);
        assert_eq!(Method::from_wire("Status"), None, "线上名是 snake_case");
    }

    /// t76 声明、t69 接线：两个静态壁纸方法的**线上名与枚举一一对应**。
    ///
    /// 接线完成后它们**不再**标记为"待实现" —— 否则 daemon 会对已实现的方法
    /// 谎报 `UNSUPPORTED_METHOD`，TUI 也会错误降级。
    #[test]
    fn static_wallpaper_methods_round_trip_on_the_wire() {
        for (m, name) in [
            (Method::SetStaticWallpaper, "set_static_wallpaper"),
            (Method::ClearStaticWallpaper, "clear_static_wallpaper"),
        ] {
            assert_eq!(m.wire_name(), name);
            assert_eq!(Method::from_wire(name), Some(m), "{name} 无法解析回来");
            assert_eq!(serde_json::to_value(m).unwrap(), serde_json::json!(name));
            assert!(
                !m.awaits_handler(),
                "{name} 已在 t69 接线，不得再标记为待实现"
            );
        }
        // 已接线的方法**不得**被误标（否则 daemon 会谎报"不支持"）。
        for m in [
            Method::Status,
            Method::SetWallpaper,
            Method::Next,
            Method::Pause,
            Method::Resume,
            Method::Subscribe,
            Method::SaveConfig,
            Method::UpdatePlaylist,
        ] {
            assert!(
                !m.awaits_handler(),
                "{} 已接线，不应标为待实现",
                m.wire_name()
            );
        }
        // 未知方法仍然解析失败（不能因为新增变体就放宽）。
        assert_eq!(Method::from_wire("set_static_wallpaper_v2"), None);
    }

    /// ① 参数往返：中文 / 空格 / 引号与符号 / 超长路径都必须**逐字节**回来。
    #[test]
    fn set_static_wallpaper_params_round_trip_with_awkward_paths() {
        let mut paths = vec![
            "/srv/wallpapers/城市夜景 最终版.png".to_string(),
            "/srv/wallpapers/a b/c  d.png".to_string(),
            "/srv/wallpapers/引号\"与\"反斜杠\\.png".to_string(),
            "/srv/wallpapers/emoji-🌃-壁纸.png".to_string(),
        ];
        // 超长：240 个中文字符（远超文件名上限，但编解码必须无损）。
        paths.push(format!("/srv/wallpapers/{}.png", "很长的名字".repeat(48)));
        for path in paths {
            let params = SetStaticWallpaperParams { path: path.clone() };
            let value = serde_json::to_value(&params).unwrap();
            assert_eq!(
                value,
                serde_json::json!({ "path": path }),
                "参数形状必须是 {{\"path\": …}}"
            );
            let back: SetStaticWallpaperParams = serde_json::from_value(value).unwrap();
            assert_eq!(back.path, path, "路径不得被改写");
            assert!(back.path.chars().count() >= path.chars().count() || path.is_empty());
        }
    }

    /// ② 三种文档取值 + **未知取值不得 panic/拒绝**（旧客户端容忍新 daemon）。
    #[test]
    fn status_playback_source_accepts_documented_and_unknown_values() {
        for value in ["static", "playlist:pl-1", "directory"] {
            let st: StatusResult = from_status_with_source(value).expect("文档取值必须能解析");
            assert_eq!(st.playback_source.as_deref(), Some(value));
        }
        // 未知取值：**照原样接受**（不 panic、不报错），显示层自行决定怎么展示。
        for weird in [
            "from-the-future",
            "playlist:",   // 空 id
            "静态壁纸",    // 中文（daemon 若本地化也必须能过）
            "a b/c\"d\\e", // 带空格/斜杠/引号/反斜杠
            "",            // 空串（语义上无意义，但**不得**崩）
        ] {
            let st: StatusResult = from_status_with_source(weird)
                .unwrap_or_else(|e| panic!("未知取值 {weird:?} 不得导致解析失败：{e}"));
            assert_eq!(st.playback_source.as_deref(), Some(weird));
        }
    }

    /// ③ 向后兼容：**旧报文（没有 `playback_source`）必须仍能解析**，
    /// 且新 daemon 在 `None` 时**不塞未知字段**（旧客户端看到的 JSON 形状不变）。
    #[test]
    fn old_status_payload_without_playback_source_still_parses() {
        let st: StatusResult = serde_json::from_str(&status_json("")).expect("旧报文必须仍可解析");
        assert_eq!(st.playback_source, None, "缺字段 ⇒ None");
        let value = serde_json::to_value(&st).unwrap();
        assert!(
            value.get("playback_source").is_none(),
            "未报告时不得序列化该字段（否则旧客户端会看到未知字段）：{value}"
        );
    }

    /// `Response::unsupported_method` 是"声明了但没接线"的**单一事实来源**：
    /// 错误码必须是 `UNSUPPORTED_METHOD`（TUI 的降级判据）。
    ///
    /// **t69 接线后**：协议里已无"待接线"方法，该构造器对任何已接线方法都会
    /// 触发防谎报守卫（panic）。因此这里验证的是**守卫本身**——它是"不许对已实现
    /// 的方法谎报不支持"这条约束的执行点；把守卫删掉本用例会变红。
    #[test]
    #[should_panic(expected = "只有 awaits_handler()==true 的方法才应走这个兜底")]
    fn unsupported_method_refuses_already_handled_methods() {
        // `set_static_wallpaper` 已由 daemon 实现（t69），绝不能走这条兜底。
        let _ = Response::unsupported_method("id-1", Method::SetStaticWallpaper);
    }

    /// 未知方法（线上名不在枚举里）仍走 `UNSUPPORTED_METHOD`：TUI 的降级判据
    /// 依赖这个码，而它由 `Method::from_wire` 返回 `None` 后的分发路径产生。
    #[test]
    fn unknown_wire_method_maps_to_none() {
        assert_eq!(Method::from_wire("no_such_method"), None);
        assert_eq!(
            Method::from_wire("set_static_wallpaper"),
            Some(Method::SetStaticWallpaper),
            "已接线的方法必须能解析回来"
        );
        assert_eq!(
            Method::from_wire("clear_static_wallpaper"),
            Some(Method::ClearStaticWallpaper)
        );
    }

    /// 最小 `status` 报文（`extra` 用于往顶层插字段）。
    fn status_json(extra: &str) -> String {
        format!(
            r#"{{"state":"StaticIdle","output":null,"media":null,"paused":false,"display_backend":"drm","decoder":"none","mode":null,"uptime_seconds":1,"channels":[]{extra}}}"#
        )
    }

    /// 带 `playback_source` 的最小 `status`：用 `serde` 组装（**自动转义**，
    /// `BUSY` 是**可重试**的临时状态，不是失败：线上名与枚举必须稳定，
    /// 且 `detail_code` 可带重试提示（t39：缩略图队列满）。
    #[test]
    fn busy_error_kind_is_stable_on_the_wire() {
        assert_eq!(ErrorKind::Busy.as_str(), "BUSY");
        let body = ErrorBody::new(ErrorKind::Busy, "缩略图队列已满（8/8），请稍后重试")
            .with_detail_code("THUMB_QUEUE_FULL");
        let wire = serde_json::to_value(&body).expect("序列化");
        assert_eq!(wire["code"], "BUSY");
        assert_eq!(wire["detail_code"], "THUMB_QUEUE_FULL");
        let back: ErrorBody = serde_json::from_value(wire).expect("反序列化");
        assert_eq!(back.code, ErrorKind::Busy);
        // 旧客户端不认识 `BUSY` 也不会解析失败（枚举由服务端决定语义）。
        let old = serde_json::json!({"code": "BUSY", "message": "busy"});
        let parsed: ErrorBody = serde_json::from_value(old).expect("未知码不应炸");
        assert_eq!(parsed.code, ErrorKind::Busy);
    }

    /// 免得测试自己写坏 JSON —— 那会变成"测了个假东西"）。
    fn from_status_with_source(source: &str) -> Result<StatusResult, serde_json::Error> {
        let mut value: serde_json::Value = serde_json::from_str(&status_json(""))?;
        value["playback_source"] = serde_json::Value::String(source.to_string());
        serde_json::from_value(value)
    }

    #[test]
    fn busy_error_kind_is_stable() {
        assert_eq!(
            serde_json::to_value(ErrorKind::Busy).unwrap(),
            serde_json::json!("BUSY")
        );
        assert_eq!(ErrorKind::Busy.as_str(), "BUSY");
    }

    #[test]
    fn thumbnail_metrics_are_additive() {
        // 旧客户端：没有这些字段的 metrics 载荷照旧解析成 0。
        let old: MetricsResult = serde_json::from_value(serde_json::json!({
            "channels": [],
            "cpu_percent": 1.5,
            "mem_bytes": 1024,
            "threads": 4,
            "fps": 60.0,
            "uptime_seconds": 3,
            "preload_cache_bytes": 0,
            "preload_cache_items": 0
        }))
        .unwrap();
        assert_eq!(old.thumb_requests, 0);
        assert_eq!(old.thumb_queue_depth, 0);
        // 零值整个省略（不改变既有载荷形状）。
        let encoded = serde_json::to_value(&old).unwrap();
        assert!(encoded.get("thumb_requests").is_none());
        assert!(encoded.get("thumb_queue_depth").is_none());
    }

    #[test]
    fn error_kind_as_str_matches_serde() {
        for kind in [
            ErrorKind::InvalidParams,
            ErrorKind::PermissionDenied,
            ErrorKind::FileUnavailable,
            ErrorKind::NotImplemented,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.as_str()),
                "as_str 必须与 serde 表示一致"
            );
        }
    }

    #[test]
    fn detail_code_is_optional_and_additive() {
        let plain = ErrorBody::new(ErrorKind::InvalidParams, "x");
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("detail_code").is_none(), "无 detail_code 时不下发");
        let coded = plain.clone().with_detail_code("PATH_OUTSIDE_ROOT");
        let json = serde_json::to_value(&coded).unwrap();
        assert_eq!(json["detail_code"], serde_json::json!("PATH_OUTSIDE_ROOT"));
        // 旧客户端（不认 detail_code）仍能反序列化。
        let mut legacy = json.clone();
        legacy.as_object_mut().unwrap().remove("detail_code");
        let back: ErrorBody = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.code, ErrorKind::InvalidParams);
    }

    #[test]
    fn new_error_codes_are_stable_strings() {
        // 新增错误码属"只增不改"，不得改名（脚本按字符串判断）。
        assert_eq!(
            serde_json::to_value(ErrorKind::UnsupportedMethod).unwrap(),
            serde_json::json!("UNSUPPORTED_METHOD")
        );
        assert_eq!(
            serde_json::to_value(ErrorKind::ServerBusy).unwrap(),
            serde_json::json!("SERVER_BUSY")
        );
    }

    #[test]
    fn new_event_kinds_have_stable_wire_names() {
        // 只增不改：TUI 按这些字符串分支。
        for (kind, wire) in [
            (EventKind::StateChanged, "STATE_CHANGED"),
            (EventKind::OutputChanged, "OUTPUT_CHANGED"),
            (EventKind::MediaChanged, "MEDIA_CHANGED"),
            (EventKind::Error, "ERROR"),
            (EventKind::WallpaperApplied, "WALLPAPER_APPLIED"),
            (EventKind::ScanProgress, "SCAN_PROGRESS"),
            (EventKind::ScanFinished, "SCAN_FINISHED"),
            (EventKind::MediaPrepared, "MEDIA_PREPARED"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(wire));
        }
    }

    #[test]
    fn wallpaper_applied_event_round_trips_with_request_id() {
        let ev = EventMessage::new(EventKind::WallpaperApplied)
            .with_request_id("set-7")
            .with_output("DP-1")
            .with_media("/srv/wallpapers/a.png")
            .with_ok(false)
            .with_error("FILE_UNAVAILABLE: 文件不存在");
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "WALLPAPER_APPLIED");
        assert_eq!(json["request_id"], "set-7");
        assert_eq!(json["ok"], false);
        assert_eq!(json["output"], "DP-1");
        assert!(
            json["error"]
                .as_str()
                .unwrap_or("")
                .starts_with("FILE_UNAVAILABLE")
        );
        // 可选字段缺省时不下发（旧客户端不受影响）。
        let idle = serde_json::to_value(EventMessage::new(EventKind::MediaChanged)).unwrap();
        for absent in ["request_id", "ok", "error", "scan_id", "found", "errors"] {
            assert!(idle.get(absent).is_none(), "{absent} 不该出现");
        }
        let back: EventMessage = serde_json::from_value(json).unwrap();
        assert_eq!(back.kind, EventKind::WallpaperApplied);
        assert_eq!(back.request_id.as_deref(), Some("set-7"));
    }

    #[test]
    fn scan_events_carry_generation_and_counts() {
        let progress = EventMessage::new(EventKind::ScanProgress).with_scan(3, 42, false);
        let json = serde_json::to_value(&progress).unwrap();
        assert_eq!(json["scan_id"], 3);
        assert_eq!(json["found"], 42);
        assert_eq!(json["done"], false);
        let finished = EventMessage::new(EventKind::ScanFinished)
            .with_scan(3, 128, true)
            .with_errors(vec!["/x: denied".to_string()]);
        let json = serde_json::to_value(&finished).unwrap();
        assert_eq!(json["done"], true);
        assert_eq!(json["errors"].as_array().map(|a| a.len()), Some(1));
    }

    #[test]
    fn accepted_result_carries_request_id_only_when_present() {
        let plain = serde_json::to_value(AcceptedResult::accepted()).unwrap();
        assert_eq!(plain["accepted"], true);
        assert!(plain.get("request_id").is_none());
        let with = serde_json::to_value(AcceptedResult::accepted_with_request("set-1")).unwrap();
        assert_eq!(with["request_id"], "set-1");
        // 旧客户端（不认 request_id）仍可反序列化。
        let back: AcceptedResult = serde_json::from_value(plain).unwrap();
        assert_eq!(back, AcceptedResult::accepted());
    }

    #[test]
    fn new_media_params_tolerate_missing_optional_fields() {
        let p: ScanSourceParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p.source_id, None);
        assert!(!p.full);
        let t: SetOutputTransformParams =
            serde_json::from_value(serde_json::json!({"output": "DP-1"})).unwrap();
        assert_eq!(t.output.as_deref(), Some("DP-1"));
        assert!(t.fit.is_none() && t.zoom.is_none() && t.rotation.is_none());
        let u: UpdatePlaylistParams =
            serde_json::from_value(serde_json::json!({"name": "x"})).unwrap();
        assert_eq!(u.mode, "sequential");
        assert_eq!(u.interval_seconds, 30);
        assert_eq!(u.on_video_end, "next");
    }

    #[test]
    fn media_visibility_fields_are_additive() {
        // 旧版本载荷（没有 skipped_oversize/skipped_examples）必须照旧解析。
        let old: ListMediaResult = serde_json::from_value(serde_json::json!({
            "items": [],
            "total": 0,
            "page": 0,
            "page_size": 50,
            "truncated": false,
            "scanning": false,
            "scan_id": 3,
            "generated_unix_ms": 17,
        }))
        .unwrap();
        assert_eq!(old.skipped_oversize, 0);
        assert!(old.skipped_examples.is_empty());

        // 没有跳过时不写字段（旧客户端/日志里的形状保持不变）。
        let fresh = ListMediaResult {
            items: Vec::new(),
            total: 0,
            page: 0,
            page_size: 50,
            truncated: false,
            scanning: false,
            scan_id: 3,
            generated_unix_ms: 17,
            skipped_oversize: 0,
            skipped_examples: Vec::new(),
        };
        let line = serde_json::to_string(&fresh).unwrap();
        assert!(!line.contains("skipped_oversize"), "{line}");
        assert!(!line.contains("skipped_examples"), "{line}");

        // 有跳过时必须如实上报（数量 + 可操作样例）。
        let skipped = ListMediaResult {
            skipped_oversize: 1,
            skipped_examples: vec!["8k.mp4（974 MB > 上限 512 MB）".into()],
            ..fresh
        };
        let line = serde_json::to_string(&skipped).unwrap();
        assert!(line.contains("\"skipped_oversize\":1"), "{line}");
        assert!(line.contains("974 MB"), "{line}");
    }

    #[test]
    fn decode_alert_is_optional_and_additive() {
        let old: VideoStatsStatus = serde_json::from_value(serde_json::json!({
            "presented": 3,
            "dropped_late": 0,
            "dropped_queue_full": 0,
            "queue_len": 1,
        }))
        .unwrap();
        assert!(old.decode_alert.is_none());
        assert_eq!(old.presented, 3);

        let line = serde_json::to_string(&old).unwrap();
        assert!(!line.contains("decode_alert"), "{line}");

        let alert = VideoStatsStatus {
            decode_alert: Some("8K H.264 没有硬件解码".into()),
            ..old
        };
        assert!(serde_json::to_string(&alert).unwrap().contains("8K H.264"));
    }

    /// `prepare_media`（t15）：线上名、参数与结果的往返，以及"只是受理/查询，
    /// 不是同步转码"的语义（`queued`/`running` 都不会带 `cache_path`）。
    #[test]
    fn prepare_media_round_trips_on_the_wire() {
        use super::{
            Method, PrepareMediaParams, PrepareMediaResult, PrepareState, Request, Response,
        };
        assert_eq!(Method::PrepareMedia.wire_name(), "prepare_media");
        assert_eq!(
            Method::from_wire("prepare_media"),
            Some(Method::PrepareMedia)
        );
        assert!(!Method::PrepareMedia.awaits_handler());

        let req = Request::new(
            "p1",
            Method::PrepareMedia,
            serde_json::json!({"path": "/media/8k h264.mp4"}),
        );
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"prepare_media\""));
        let back: Request = serde_json::from_str(&line).unwrap();
        let params: PrepareMediaParams = serde_json::from_value(back.params).unwrap();
        assert_eq!(params.path, "/media/8k h264.mp4");
        assert_eq!(params.force, None, "省略 force = 不强制重转");

        let forced: PrepareMediaParams =
            serde_json::from_value(serde_json::json!({"path": "/m/a.mp4", "force": true})).unwrap();
        assert_eq!(forced.force, Some(true));

        // 排队/运行中：没有缓存路径，只有进度。
        let queued = Response::ok(
            "p1",
            serde_json::to_value(PrepareMediaResult {
                state: PrepareState::Queued,
                cache_path: None,
                progress_permille: 0,
                message: None,
            })
            .unwrap(),
        );
        let json = serde_json::to_value(&queued).unwrap();
        let result = &json["result"];
        assert_eq!(result["state"], serde_json::json!("queued"));
        assert!(
            result.get("cache_path").is_none(),
            "排队时不下发 cache_path"
        );

        // 就绪：带缓存路径与 1000‰。
        let ready = serde_json::to_value(PrepareMediaResult {
            state: PrepareState::Ready,
            cache_path: Some("/home/u/.cache/liuguang/transcode/abc.mkv".into()),
            progress_permille: 1000,
            message: Some("已转换为 8bit HEVC".into()),
        })
        .unwrap();
        assert_eq!(ready["state"], serde_json::json!("ready"));
        assert_eq!(
            ready["cache_path"],
            serde_json::json!("/home/u/.cache/liuguang/transcode/abc.mkv")
        );
        assert_eq!(ready["progress_permille"], serde_json::json!(1000));
    }

    /// 兼容性字段是**纯增量**：旧载荷（没有 `compat`）照旧解析，新载荷多一个字段，
    /// 旧客户端也能读（`serde` 忽略未知字段）。
    #[test]
    fn compat_info_is_additive_and_optional() {
        use super::{CompatInfo, CompatState, GetMediaResult, MediaItem};

        let item = MediaItem {
            id: "/m/a.mp4".into(),
            source_id: "/m".into(),
            name: "a.mp4".into(),
            path: "/m/a.mp4".into(),
            kind: "video".into(),
            size_bytes: 1,
            modified_unix: 2,
            details: None,
        };
        let old = serde_json::json!({"item": item});
        let parsed: GetMediaResult = serde_json::from_value(old.clone()).unwrap();
        assert!(parsed.compat.is_none(), "旧载荷没有 compat ⇒ None");
        let round = serde_json::to_value(&parsed).unwrap();
        assert!(round.get("compat").is_none(), "None 时不下发");
        assert_eq!(round, old, "旧载荷必须逐字节不变");

        let with = GetMediaResult {
            item: parsed.item,
            compat: Some(CompatInfo {
                state: CompatState::NeedsConversion,
                reason: Some("7680 宽 H.264 超出 NVDEC 的 H.264 上限".into()),
                target: Some("hevc 8bit ≤4096".into()),
            }),
        };
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(
            json["compat"]["state"],
            serde_json::json!("needs_conversion")
        );
        assert_eq!(
            json["compat"]["target"],
            serde_json::json!("hevc 8bit ≤4096")
        );
        // 旧客户端（不认 compat）反序列化同一份 JSON 也不出错。
        let mut legacy = json.clone();
        legacy.as_object_mut().unwrap().remove("compat");
        let back: GetMediaResult = serde_json::from_value(legacy).unwrap();
        assert!(back.compat.is_none());
    }

    /// `MEDIA_PREPARED` 事件：稳定线上名 + `media`/`cache_path` 两段关键信息。
    #[test]
    fn media_prepared_event_carries_source_and_cache_path() {
        use super::{EventKind, EventMessage};
        let ev = EventMessage::new(EventKind::MediaPrepared)
            .with_media("/media/8k.mp4")
            .with_cache_path("/home/u/.cache/liuguang/transcode/abc.mkv")
            .with_message("8K H.264 已转成 4K 8bit HEVC");
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains("\"MEDIA_PREPARED\""));
        assert!(line.contains("/media/8k.mp4"));
        assert!(line.contains("/home/u/.cache/liuguang/transcode/abc.mkv"));
        let back: EventMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(back.kind, EventKind::MediaPrepared);
        assert_eq!(back.media.as_deref(), Some("/media/8k.mp4"));
        assert_eq!(
            back.cache_path.as_deref(),
            Some("/home/u/.cache/liuguang/transcode/abc.mkv")
        );
    }

    #[test]
    fn version_detection() {
        let mut req = Request::new("x", Method::Status, serde_json::json!({}));
        req.v = 2;
        assert!(!req.version_supported());
    }

    /// `delete_media` / `delete_playlist`：线上名、参数与结果的往返。
    ///
    /// 注意语义：**只从索引 / 播放列表定义里移除，绝不删除磁盘文件** ——
    /// GUI 的"从库中删除"按钮依赖这一点（用户以为在整理库，不该丢原片）。
    #[test]
    fn delete_methods_round_trip_on_the_wire() {
        use super::{
            DeleteMediaParams, DeleteMediaResult, DeletePlaylistParams, DeletePlaylistResult,
            Method, Request,
        };
        for (m, name) in [
            (Method::DeleteMedia, "delete_media"),
            (Method::DeletePlaylist, "delete_playlist"),
        ] {
            assert_eq!(m.wire_name(), name);
            assert_eq!(Method::from_wire(name), Some(m), "{name} 必须能解析回来");
            assert!(
                !m.awaits_handler(),
                "{name} 已接线，不能再谎报 UNSUPPORTED_METHOD"
            );
        }

        // delete_media：请求行里带 snake_case 方法名，参数可原样解析回同构结构。
        let params = DeleteMediaParams {
            path: "/媒体/带空格 与中文/v.mp4".to_string(),
        };
        let req = Request::new(
            "r-1",
            Method::DeleteMedia,
            serde_json::to_value(&params).unwrap(),
        );
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"delete_media\""), "{line}");
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back.method, Method::DeleteMedia);
        assert_eq!(
            serde_json::from_value::<DeleteMediaParams>(back.params).unwrap(),
            params
        );

        let result = DeleteMediaResult {
            removed: true,
            removed_count: 1,
            switched_outputs: vec!["DP-1".to_string()],
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["removed"], serde_json::json!(true));
        assert_eq!(
            serde_json::from_value::<DeleteMediaResult>(json).unwrap(),
            result
        );
        // 精简载荷（没有 switched_outputs）也要能解析：老/瘦客户端不该 400。
        let minimal = serde_json::json!({"removed": false, "removed_count": 0});
        assert_eq!(
            serde_json::from_value::<DeleteMediaResult>(minimal).unwrap(),
            DeleteMediaResult {
                removed: false,
                removed_count: 0,
                switched_outputs: Vec::new(),
            }
        );

        // delete_playlist：id 原样回显，便于 GUI 对账（不做大小写/前缀归一）。
        let params = DeletePlaylistParams {
            playlist_id: "pl-7".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            serde_json::json!({"playlist_id": "pl-7"})
        );
        let result = DeletePlaylistResult {
            removed: true,
            playlist_id: "pl-7".to_string(),
            rebuilt_outputs: vec!["HDMI-A-1".to_string()],
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["playlist_id"], serde_json::json!("pl-7"));
        assert_eq!(
            serde_json::from_value::<DeletePlaylistResult>(json).unwrap(),
            result
        );
        let minimal = serde_json::json!({"removed": true, "playlist_id": "pl-7"});
        assert_eq!(
            serde_json::from_value::<DeletePlaylistResult>(minimal)
                .unwrap()
                .rebuilt_outputs,
            Vec::<String>::new()
        );
    }

    /// `MediaItem.details` 是**纯增量**：旧载荷照常解析成 `None`，且 `None` 时不写进线上形状
    /// （不然老版本 GUI 会看到 `"details":null` 这种噪音，快照测试也会全线抖动）。
    #[test]
    fn media_details_are_additive_and_optional() {
        use super::{MediaDetails, MediaItem};
        let legacy = serde_json::json!({
            "id": "/m/a.png",
            "source_id": "/m",
            "name": "a.png",
            "path": "/m/a.png",
            "kind": "image",
            "size_bytes": 123u64,
            "modified_unix": 42i64
        });
        let item: MediaItem = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(item.details, None);
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            legacy,
            "details=None 时线上形状必须与旧版逐字节一致"
        );

        let details = MediaDetails {
            width: 3840,
            height: 2160,
            fps_millihz: Some(59_940),
            duration_ms: Some(12_345),
            codec: Some("hevc".to_string()),
            pix_fmt: Some("yuv420p10le".to_string()),
        };
        let item = MediaItem {
            details: Some(details.clone()),
            ..item
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["details"]["width"], serde_json::json!(3840));
        assert_eq!(json["details"]["height"], serde_json::json!(2160));
        let back: MediaItem = serde_json::from_value(json).unwrap();
        assert_eq!(back.details, Some(details));

        // 帧率用整数毫赫兹：29.97fps→29970、23.976fps→23976，不能出现 f64 尾巴。
        let d = MediaDetails {
            width: 1,
            height: 1,
            fps_millihz: Some(23_976),
            duration_ms: None,
            codec: None,
            pix_fmt: None,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["fps_millihz"], serde_json::json!(23_976));
        assert_eq!(json["duration_ms"], serde_json::Value::Null);
        let back: MediaDetails = serde_json::from_value(json).unwrap();
        assert_eq!(back, d);
        // 只有宽高是必填：视频可缺 fps/时长/编码，图片可缺全部可选项。
        let minimal: MediaDetails = serde_json::from_value(serde_json::json!({
            "width": 1920, "height": 1080
        }))
        .unwrap();
        assert_eq!(minimal.fps_millihz, None);
        assert_eq!(minimal.duration_ms, None);
        assert_eq!(minimal.codec, None);
        assert_eq!(minimal.pix_fmt, None);
    }

    /// `get_thumbnail.at_ms`：缺省 = 首帧（`None` 即 0），可显式指定，负值/非法类型必须被拒。
    #[test]
    fn thumbnail_params_carry_at_ms_optionally() {
        use super::GetThumbnailParams;
        let p: GetThumbnailParams =
            serde_json::from_value(serde_json::json!({"id": "/m/v.mp4"})).unwrap();
        assert_eq!(p.at_ms, None, "缺省不取帧时刻 ⇒ daemon 按 0 处理");
        assert_eq!(p.max_width, None);
        assert_eq!(p.max_height, None);

        let p: GetThumbnailParams =
            serde_json::from_value(serde_json::json!({"id": "/m/v.mp4", "at_ms": 0})).unwrap();
        assert_eq!(p.at_ms, Some(0), "显式 0 与缺省等价，但要保留意图");

        let p: GetThumbnailParams = serde_json::from_value(serde_json::json!({
            "id": "/m/v.mp4", "at_ms": 2_500u64, "max_width": 640, "max_height": 360
        }))
        .unwrap();
        assert_eq!(p.at_ms, Some(2_500));
        assert_eq!(p.max_width, Some(640));
        assert_eq!(p.max_height, Some(360));
        // 往返：序列化回来仍是同一个数字（毫秒是整数，不经过浮点）。
        let back: GetThumbnailParams =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);

        assert!(
            serde_json::from_value::<GetThumbnailParams>(
                serde_json::json!({"id": "/m/v.mp4", "at_ms": -1})
            )
            .is_err(),
            "负时刻必须报错，不能静默当成 0"
        );
    }
}
