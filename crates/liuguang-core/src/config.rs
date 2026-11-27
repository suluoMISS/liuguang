//! 配置模型与校验（规范 §10）。
//!
//! 默认配置路径：`/etc/liuguang/config.toml`。解析与语义校验分离：
//! 语法/类型错误由 `toml` 报告行列位置；语义错误以字段路径
//! （如 `transition.duration_ms`）报告。**校验失败不产生部分配置**——
//! 调用方应保留当前有效配置（M2 起的 reload 语义）。
//!
//! # 未知字段与"静默失效"
//!
//! 所有段都带 `deny_unknown_fields`：**字段名写错会直接报错并带行列位置**，
//! 而不是被静默忽略（规范 §10"配置字段验证时返回具体错误位置"）。
//! 另一方面，§10 里有些字段**尚未被 `apps/liuguang` 消费**；这类字段不会被
//! 静默接受，而会出现在 [`Config::pending_implementation`] 里（见该方法的文档）。
//! 两张清单必须保持同步：新增/移除实现时同时更新
//! [`PENDING_IMPLEMENTATION_OPTIONS`] 与 `config/example.toml` 的注释。

use crate::error::ConfigError;
pub use crate::fit::{FitMode, MAX_ZOOM};
use crate::transform::Rotation;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 显示后端种类。
///
/// 两种**运行时人格**（见 `docs/DESIGN-DESKTOP-PIVOT-0.2.md`）：
/// - [`Drm`](Self::Drm)：无桌面（TTY/cage 类 kiosk）直接接管 DRM/KMS，
///   配置与状态在 `/etc/liuguang`、`/var/lib/liuguang`；
/// - [`Wayland`](Self::Wayland)：桌面会话内的壁纸（wlroots 系合成器
///   Background 层），配置与状态走 `$XDG_CONFIG_HOME`/`$XDG_STATE_HOME`。
///
/// [`Auto`](Self::Auto)（默认）在启动时按环境决定：有 `WAYLAND_DISPLAY`
/// 且连得上合成器 → `Wayland`，否则 → `Drm`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DisplayBackendKind {
    /// 按环境自动选择（有 Wayland 会话 → wayland，否则 drm）。
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// Wayland（wlroots 系合成器）桌面壁纸层。
    #[serde(rename = "wayland")]
    Wayland,
    /// DRM/KMS 直接输出（无桌面）。
    #[serde(rename = "drm")]
    Drm,
    /// 模拟后端（**测试钩子**，非生产；E2E 与开发用）。
    #[serde(rename = "mock")]
    Mock,
}

impl DisplayBackendKind {
    /// 配置字符串（`auto`/`wayland`/`drm`），用于日志与 IPC。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Wayland => "wayland",
            Self::Drm => "drm",
            Self::Mock => "mock",
        }
    }

    /// 解析配置字符串（大小写不敏感）；未知值返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "wayland" | "wl" => Some(Self::Wayland),
            "drm" | "kms" => Some(Self::Drm),
            "mock" => Some(Self::Mock),
            _ => None,
        }
    }

    /// `Auto` 在当前环境下的具体后端：有 `WAYLAND_DISPLAY` → wayland，
    /// 否则 drm。
    ///
    /// 只做**环境判定**，不尝试连接（连接失败由调用方回退）。
    pub fn resolved(self) -> Self {
        match self {
            Self::Auto => {
                let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
                if has_wayland {
                    Self::Wayland
                } else {
                    Self::Drm
                }
            }
            other => other,
        }
    }
}

/// 模式选择：`preferred` 或 `宽x高[@刷新率Hz]`（如 `1920x1080@60`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModeSelection {
    /// 显示器首选模式。
    #[default]
    Preferred,
    /// 手动指定 `宽x高[@刷新率Hz]`。
    Manual {
        /// 宽（像素）。
        width: u32,
        /// 高（像素）。
        height: u32,
        /// 刷新率毫赫兹；未指定时由驱动选择最接近值。
        refresh_millihz: Option<u32>,
    },
}

impl ModeSelection {
    /// 解析 `preferred` 或 `宽x高[@刷新率Hz]`。
    ///
    /// # Errors
    ///
    /// 语法错误时返回错误消息。
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s == "preferred" {
            return Ok(Self::Preferred);
        }
        let (wh, refresh) = match s.split_once('@') {
            Some((wh, r)) => {
                let hz: f64 = r
                    .trim()
                    .parse()
                    .map_err(|_| format!("刷新率不是数字: {r}"))?;
                if !(hz.is_finite() && hz > 0.0 && hz <= 1000.0) {
                    return Err(format!("刷新率超出范围 (0, 1000]: {hz}"));
                }
                (wh, Some((hz * 1000.0).round() as u32))
            }
            None => (s, None),
        };
        let (w, h) = wh
            .split_once('x')
            .ok_or_else(|| format!("模式应为 `preferred` 或 `宽x高[@Hz]`: {s}"))?;
        let width: u32 = w.trim().parse().map_err(|_| format!("宽度不是数字: {w}"))?;
        let height: u32 = h.trim().parse().map_err(|_| format!("高度不是数字: {h}"))?;
        if width == 0 || height == 0 {
            return Err(format!("模式宽高必须大于 0: {s}"));
        }
        Ok(Self::Manual {
            width,
            height,
            refresh_millihz: refresh,
        })
    }
}

impl std::fmt::Display for ModeSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preferred => f.write_str("preferred"),
            Self::Manual {
                width,
                height,
                refresh_millihz,
            } => match refresh_millihz {
                Some(mhz) => write!(f, "{width}x{height}@{:.3}", f64::from(*mhz) / 1000.0),
                None => write!(f, "{width}x{height}"),
            },
        }
    }
}

/// `[display]` 段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    /// 显示后端。
    pub backend: DisplayBackendKind,
    /// DRM 设备：`auto`（动态发现）或 `/dev/dri/cardN` 绝对路径。
    pub device: String,
    /// 模式选择字符串，见 [`ModeSelection`]。
    pub mode: String,
    /// 是否等待垂直同步再提交。
    ///
    /// **实现状态（2026-10-28 更正）**：
    /// - `true`（默认）**已生效**：DRM 提交路径用的是**阻塞** atomic commit
    ///   （稳态不带 `NONBLOCK`），ioctl 在 flip 完成后才返回 ⇒ 天然对齐 vblank、
    ///   每次提交最多阻塞一个刷新周期（见 `drm_kms::commit_atomic` 的文档）。
    /// - `false`（要求非阻塞/不等 vblank）**未实现** —— 非阻塞提交需要 in-flight
    ///   缓冲所有权管理，当前不提供；此时 [`PENDING_IMPLEMENTATION_OPTIONS`] 会
    ///   如实报出 `display.vsync`，不会假装已经关掉垂直同步。
    pub vsync: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            backend: DisplayBackendKind::Auto,
            device: "auto".to_string(),
            mode: "preferred".to_string(),
            vsync: true,
        }
    }
}

impl DisplayConfig {
    /// 解析后的模式选择。
    ///
    /// # Errors
    ///
    /// 模式字符串非法时返回 [`ConfigError::InvalidField`]。
    pub fn mode_selection(&self) -> Result<ModeSelection, ConfigError> {
        ModeSelection::parse(&self.mode)
            .map_err(|message| ConfigError::invalid_field("display.mode", message))
    }
}

/// 硬解策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HardwareDecode {
    /// 自动协商，失败按 `software_decode_fallback` 处理。
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// 必须硬解，否则显示 fallback 并报错。
    #[serde(rename = "required")]
    Required,
    /// 禁用硬解。
    #[serde(rename = "disabled")]
    Disabled,
}

/// `[media]` 段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// 硬解策略。
    ///
    /// **实现状态：生效** —— 映射为解码器选择策略
    /// （`liuguang-media::gstreamer::DecoderPolicy`，由 `apps/liuguang` 构造）。
    pub hardware_decode: HardwareDecode,
    /// 首选解码器名（如 `vaapi`）。
    ///
    /// **实现状态：未生效** —— 解码器由元素可用性 + VA 能力探测决定，
    /// 该字段当前不参与选择。
    pub preferred_decoder: String,
    /// 每输出视频帧率上限。
    ///
    /// **实现状态：生效**（`VideoOptions::fps_limit`）。
    ///
    /// 建议与面板刷新率一致（60Hz 面板 ⇒ 60）。设得比源帧率低会触发跳帧，
    /// 而解码器仍按**源帧率**解码 ⇒ 白费 CPU（真机实测：4K60 源 + 上限 30 ⇒ 82%）。
    pub video_fps_limit: u32,
    /// 静音（首版忽略音频输出）。
    ///
    /// **实现状态：部分** —— 管线本就不输出音频，故 `mute = true` 天然满足；
    /// `mute = false`（要求放音）无法满足，见 [`Config::pending_implementation`]。
    pub mute: bool,
    /// 允许软件解码回退。
    ///
    /// **实现状态：生效**（与 `hardware_decode` 共同构成解码策略）。
    pub software_decode_fallback: bool,
    /// VA-API 零拷贝直通的设备选择：`auto`（探测）、`off`（禁用）或渲染节点路径。
    ///
    /// **实现状态：生效**（需以 `--features vaapi` 构建 `liuguang`）——
    /// 由 `apps/liuguang` 读取，环境变量 `LIUGUANG_VIDEO_VAAPI` 优先。
    /// 选中的节点必须在**与合成器同一块 GPU** 上，否则导出的 DMA-BUF
    /// 修饰符不会被接受（跨 GPU 时如实拒绝并回退 GStreamer 管线）。
    pub video_vaapi: String,
    /// 是否先把解码帧转成 RGB 再交给合成器：`auto`（按已知缺陷名单）、
    /// `always`（一律转）、`never`（一律不转）。
    ///
    /// **实现状态：生效**（需以 `--features vaapi` 构建 `liuguang`）。
    /// 起因：某些渲染栈把**多平面 YUV** 的 DMA-BUF 当普通纹理采样，只取到
    /// plane 0（亮度）⇒ 整屏发红。名单见 `apps/liuguang/src/compositor_profile.rs`，
    /// 环境变量 `LIUGUANG_DMABUF_RGB` 可临时覆盖本项。
    pub video_dmabuf_rgb: String,
    /// 允许 IPC 读取的媒体根目录白名单（必须为绝对路径）。
    /// 为空时使用 `slideshow.directory` 作为唯一允许根。
    ///
    /// **边界说明**：这是**防误用的名字空间白名单**，不是完备的安全边界——
    /// 白名单目录内的硬链接仍可指向白名单外的 inode（符号链接会被
    /// `canonicalize` 解析并拒绝）。见 `docs/DEPLOYMENT_FEDORA.md` §6。
    pub allowed_roots: Vec<PathBuf>,
    /// 硬件解不动素材的**自动转码**策略：`auto`（默认，自动转）、`off`（禁用）
    /// 或 `ffmpeg` 可执行文件路径。
    ///
    /// **实现状态：生效**（2026-11-22，t15）—— 扫描/`get_media` 时按本机硬件
    /// 能力分类；`set_wallpaper` 碰到需要转换的素材不再静默软解，而是后台把它
    /// 转成可硬解的 8bit HEVC 缓存（NVENC 优先、失败回退 libx265），完成即自动播放。
    /// **源文件永不修改**，缓存落在 `$XDG_CACHE_HOME/liuguang/transcode/`。
    /// 环境变量 `LIUGUANG_TRANSCODE=off` 可临时关闭（优先级高于本项）。
    pub transcode: String,
    /// 转码输出的最大宽度（`0` = 保持原分辨率）。
    ///
    /// **实现状态：生效**（t15）。非 `0` 时按比例缩到该宽度（取偶数，避免
    /// 奇数宽高让编码器报错）。真机上 8K H.264 就算软解也只有 0.086 fps，
    /// 把它降到 4K 既能让 NVENC 编码、也让后续解码回到硬解路径。
    pub transcode_max_width: u32,
    /// 转码用的 ffmpeg 编码器与质量参数，写法是 `<encoder> [args…]`。
    ///
    /// **实现状态：生效**（t15）。默认 `hevc_nvenc -b:v 20M`（NVENC 硬编，几乎不吃
    /// CPU）；NVENC 不可用/失败时自动回退 `libx265`（CPU 编码，慢但一定在）。
    /// 目标像素格式固定 `yuv420p`（我方解码/RGB 转换链只认 8bit NV12）。
    pub transcode_crf_or_bitrate: String,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            hardware_decode: HardwareDecode::Auto,
            preferred_decoder: "vaapi".to_string(),
            // **60**：面板就是 60Hz，设 60 ⇒ 60fps 源无需跳帧（1 拉 1 呈）。
            // 设 30 会让 60fps 源每两帧跳一次，而解码器仍按源帧率解码 ⇒ 白费 CPU。
            video_fps_limit: 60,
            mute: true,
            software_decode_fallback: true,
            video_vaapi: "auto".to_string(),
            video_dmabuf_rgb: "auto".to_string(),
            allowed_roots: Vec::new(),
            transcode: "auto".to_string(),
            transcode_max_width: 0,
            transcode_crf_or_bitrate: "hevc_nvenc -b:v 20M".to_string(),
        }
    }
}

/// `[performance]` 段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerformanceConfig {
    /// 是否预加载下一个媒体项。
    ///
    /// **实现状态：生效**（2026-10-20）—— `apps/liuguang` 在每次成功显示图片后记下
    /// "下一张"，并在随后的 tick 里至多解码一张进 `PreloadCache`；命中即跳过展示
    /// 时刻的同步解码。降载档（[`PerformanceConfig::reduce_load_on_cpu_pressure`]）
    /// 与后台扫描期间**暂停**预加载（规范 §11.2）。
    pub preload_next: bool,
    /// 应用缓存软预算（MiB），非整机内存限制。
    ///
    /// **实现状态：生效**（2026-10-20）—— 它就是 `PreloadCache` 的字节预算：
    /// 超出即按 LRU 淘汰最久未用的解码帧；单张解码后超过预算会大声告警并
    /// **永不命中**（该图退回同步解码）。
    pub max_cache_mb: u32,
    /// 有界帧队列长度。
    ///
    /// **实现状态：生效**（`VideoOptions::queue_size`）。
    pub frame_queue_size: u32,
    /// 显示器熄屏/断开时暂停播放。
    ///
    /// **实现状态：未生效** —— 断开只做通道移除 + 缓存释放，没有"熄屏即暂停"：
    /// **已实现（2026-10-20）**：读 `/sys/class/drm/card*-<连接器>/dpms` 与
    /// `.../status`（真机 i915/DP-1 验证存在且可读）⇒ 熄屏即停拉帧（CPU 归零），
    /// 亮屏自动恢复。只恢复"因熄屏而暂停"的通道，不碰用户手动暂停的。
    ///
    /// **局限（如实说明）**：只**观测**不**设置**。真机 `consoleblank=0` 且无常驻
    /// 合成器 ⇒ 没有进程会主动熄屏，因此"手动关显示器电源"不会改变 `dpms`，
    /// 本项对它无效；对"被其他组件熄屏/输出断开/内核 blanking 生效"有效。
    pub pause_when_display_off: bool,
    /// 有窗口全屏时暂停视频（游戏、播放器、演示文稿）。
    ///
    /// **实现状态：生效**（2026-11-21）—— Wayland 后端通过
    /// `zwlr_foreign_toplevel_management_unstable_v1` 观察顶层窗口的
    /// `state` 位（协议 since 2 的 `fullscreen`），进入全屏时暂停所有正在播视频
    /// 的通道，退出全屏后只恢复**自己暂停**的那些（不碰用户手动暂停的）。
    ///
    /// **边界（如实说明）**：
    /// - GNOME/KDE/纯 X11 **没有**这个协议 ⇒ 该项自动降级为"不检测"，不会报错；
    /// - 只认协议报告的 fullscreen；"无边框最大化"的游戏不会被判全屏；
    /// - v1 口径是"任一处全屏即全屏"，不按输出过滤（多显示器下偏保守，
    ///   宁可多停一次也不要在游戏里偷偷解码视频）。
    #[serde(default = "default_true")]
    pub pause_on_fullscreen: bool,
    /// CPU 压力高时降低负载。
    ///
    /// **实现状态：生效**（2026-10-20）—— `apps/liuguang/src/loadshed.rs` 读
    /// PSI `some avg10`（回退 loadavg/核数），带滞回地切档；降载档把每一路在播
    /// 视频的**提交频率上限减半**（保底 1fps）并暂停预加载。
    /// **未测量项**：真机上"降载省下多少 CPU"仍未实测（`docs/PROGRESS.md`）。
    pub reduce_load_on_cpu_pressure: bool,
    /// 显存占用百分比超过该值时**暂停视频供给**（让路给游戏/渲染任务）。
    ///
    /// **为什么是显存而不是全屏**：2026-11-23 真机实测——游戏把显存吃到 91%，
    /// 我们仍 4K60 解码 + GPU 转 RGB，`vaExportSurfaceHandle` 分配失败并触发
    /// NVIDIA 驱动 `gpu_vaspace.c` 断言、桌面卡死。denial 合成器不提供窗口全屏
    /// 状态（只有 `ext_foreign_toplevel_list_v1`，没有 fullscreen 位），显存是
    /// 唯一可靠的等价信号。读数来自 `nvidia-smi`（3s 一次，独立线程）。
    ///
    /// `0` 或 `<= resume_when_gpu_memory_below` = 关闭；支持 NVIDIA (`nvidia-smi`) 与 AMD sysfs。
    #[serde(default = "default_gpu_pause_above")]
    pub pause_when_gpu_memory_above: u8,
    /// 显存占用回落到该值以下时恢复此前因显存压力暂停的通道（滞回下沿）。
    #[serde(default = "default_gpu_resume_below")]
    pub resume_when_gpu_memory_below: u8,
    /// 是否探测「有游戏在跑」并在其运行期间暂停视频供给。
    ///
    /// **为什么需要**：denial 合成器不提供窗口全屏状态（`ext_foreign_toplevel_list_v1`
    /// 没有 fullscreen 位，也没有 `zwlr_foreign_toplevel_management`），所以只能从
    /// 进程表判断。判据见 `apps/liuguang/src/game_watch.rs`：支持 Windows `*.exe`
    /// （从 cmdline 找回被 15 字符截断的 `comm`）、Steam `reaper AppId`、
    /// Proton `waitforexitandrun`、`steamapps/common/<游戏>`（排除工具/运行时）、
    /// `gamescope`/`umu-run`/模拟器/Minecraft Java 以及可读的环境变量兜底。
    ///
    /// 滞回：每 2s 采样一次，连续 2 次命中（≈4s）暂停，连续 4 次未命中（≈8s）恢复；
    /// 启动器/过图/短时切出不会让壁纸反复启停。
    #[serde(default = "default_detect_games")]
    pub detect_games: bool,
    /// 额外的游戏特征（不区分大小写的子串，匹配 `comm` 或 cmdline）。
    ///
    /// 用于原生 Linux 游戏：官方启动方式没有可识别环境变量时，把进程名写进来，
    /// 例如 `game_processes = ["factorio", "dota2"]`。
    #[serde(default)]
    pub game_processes: Vec<String>,
    /// 明确**不是**游戏的进程特征（不区分大小写）。
    ///
    /// 例：`game_ignore_processes = ["QQ", "chromium", "electron"]`。忽略名单优先于
    /// 所有命中判据（包括环境变量）。
    #[serde(default)]
    pub game_ignore_processes: Vec<String>,
    /// 笔记本切换到电池供电时是否自动暂停视频供给（默认 true；台式机无电池恒不触发）。
    #[serde(default = "default_true")]
    pub pause_on_battery: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            preload_next: true,
            max_cache_mb: 128,
            frame_queue_size: 3,
            pause_when_display_off: true,
            pause_on_fullscreen: true,
            reduce_load_on_cpu_pressure: true,
            pause_when_gpu_memory_above: default_gpu_pause_above(),
            resume_when_gpu_memory_below: default_gpu_resume_below(),
            detect_games: default_detect_games(),
            game_processes: Vec::new(),
            game_ignore_processes: Vec::new(),
            pause_on_battery: true,
        }
    }
}

/// 轮换顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlaylistOrder {
    /// 顺序播放。
    #[serde(rename = "sequential")]
    Sequential,
    /// shuffle bag 随机（当前轮尽量不重复）。
    #[default]
    #[serde(rename = "shuffle")]
    Shuffle,
}

/// `[slideshow]` 段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlideshowConfig {
    /// 是否启用轮换。
    ///
    /// **实现状态：部分** —— `false`（关闭轮换）尚未被执行：调度器照常排期。
    /// 见 [`Config::pending_implementation`]。
    pub enabled: bool,
    /// 媒体目录（必须为绝对路径）。
    pub directory: PathBuf,
    /// 轮换间隔（秒）。
    ///
    /// **实现状态：生效**，但注意首帧显示是否会重置间隔由 `apps/liuguang` 的
    /// 实现决定；本字段本身只被 `IntervalScheduler` 读取。
    pub interval_seconds: u64,
    /// 播放顺序。
    pub order: PlaylistOrder,
    /// 随机模式避免立即重复。
    pub avoid_repeat: bool,
}

impl Default for SlideshowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: PathBuf::from("/srv/wallpapers"),
            interval_seconds: 600,
            order: PlaylistOrder::Shuffle,
            avoid_repeat: true,
        }
    }
}

/// 切换效果（规范 §7.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TransitionEffect {
    /// 淡入淡出。
    #[default]
    #[serde(rename = "fade")]
    Fade,
    /// 滑动。
    #[serde(rename = "slide")]
    Slide,
    /// 方向性擦除。
    #[serde(rename = "wipe")]
    Wipe,
    /// 圆形揭示。
    #[serde(rename = "circle")]
    Circle,
    /// 每次随机。
    #[serde(rename = "random")]
    Random,
    /// 无转场（立即切换）。
    #[serde(rename = "none")]
    None,
}

/// 缓动函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Easing {
    /// 线性。
    #[serde(rename = "linear")]
    Linear,
    /// 先慢后快。
    #[serde(rename = "ease-in")]
    EaseIn,
    /// 先快后慢。
    #[serde(rename = "ease-out")]
    EaseOut,
    /// 两头慢中间快。
    #[default]
    #[serde(rename = "ease-in-out")]
    EaseInOut,
}

/// 转场方向（适用于 slide / wipe 等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionDirection {
    /// 向左。
    #[serde(rename = "left")]
    Left,
    /// 向右。
    #[serde(rename = "right")]
    Right,
    /// 向上。
    #[serde(rename = "up")]
    Up,
    /// 向下。
    #[serde(rename = "down")]
    Down,
}

/// 转场时长合法上限（毫秒），见 [`TransitionConfig`]。
pub const MAX_TRANSITION_DURATION_MS: u32 = 10_000;

/// `[transition]` 段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransitionConfig {
    /// 是否启用转场。
    pub enabled: bool,
    /// 效果。
    pub effect: TransitionEffect,
    /// 时长（毫秒），合法范围 `[0, 10000]`。
    pub duration_ms: u32,
    /// 缓动。
    pub easing: Easing,
    /// 方向（适用效果使用）。
    pub direction: Option<TransitionDirection>,
}

impl Default for TransitionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            effect: TransitionEffect::Fade,
            duration_ms: 350,
            easing: Easing::EaseInOut,
            direction: None,
        }
    }
}

/// `[ipc]` 段（M2 扩展：socket 路径可配置，规范 §9.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IpcConfig {
    /// 控制 socket 路径（必须为绝对路径）。
    pub socket_path: PathBuf,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/liuguang/control.sock"),
        }
    }
}

/// `[runtime]` 段（M2 扩展：运行状态持久化路径）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// 运行状态文件（必须为绝对路径）。
    pub state_file: PathBuf,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            state_file: PathBuf::from("/var/lib/liuguang/state.json"),
        }
    }
}

/// 单输出显示适配配置（`[outputs."HDMI-A-1"]`）。
///
/// M6：`media_directory`/`interval_seconds`/`order` 覆盖全局 `[slideshow]` 段，
/// 实现每输出独立播放列表/间隔/顺序。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// 适配模式。
    pub fit: FitMode,
    /// 用户缩放（(0, 10]）。
    pub zoom: f64,
    /// 水平偏移（输出像素）。
    pub offset_x: f64,
    /// 垂直偏移（输出像素）。
    pub offset_y: f64,
    /// 输出旋转角度（顺时针），配置值为 `"0" | "90" | "180" | "270"`，默认不旋转。
    #[serde(default)]
    pub rotation: Rotation,
    /// 亮度倍率（`0.1..=1.5`，`1.0` = 原图亮度），默认 `1.0`。
    #[serde(default = "default_brightness")]
    pub brightness: f64,
    /// 后备图片（必须为绝对路径；缺省时不设后备图）。
    pub fallback_image: Option<PathBuf>,
    /// 本输出的媒体目录（缺省用 `slideshow.directory`）。
    pub media_directory: Option<PathBuf>,
    /// 本输出的轮换间隔秒（缺省用 `slideshow.interval_seconds`）。
    pub interval_seconds: Option<u64>,
    /// 本输出的播放顺序（缺省用 `slideshow.order`）。
    pub order: Option<PlaylistOrder>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            fit: FitMode::Cover,
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            rotation: Rotation::Deg0,
            brightness: 1.0,
            fallback_image: None,
            media_directory: None,
            interval_seconds: None,
            order: None,
        }
    }
}

/// 播放列表里的一个条目（落盘形态，`[[playlists]]` 的 `entries`）。
///
/// `media_id` 是**媒体库里的规范化绝对路径**（与 `MediaItem.id` 同一取值），
/// 但配置文件是**用户可写**的 ⇒ 装载时必须重新过一遍
/// `MediaRoot::resolve()`（见 `apps/liuguang` 的 `load_playlists_from_config`），
/// 否则配置就成了任意路径注入点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistEntryConfig {
    /// 媒体 id（= 媒体库规范化后的绝对路径）。
    pub media_id: String,
}

/// 一条播放列表（`[[playlists]]`；`save_config` 落盘，启动时装载）。
///
/// **T62**：此前播放列表只活在 daemon 内存里 —— `save_config` 回
/// `saved=true` 但配置**零变化**，重启后 `list_playlists` 回到 0
/// （与 T25-F1 同族：**报告成功但没做**）。现在它是配置的一部分。
///
/// 字段与 IPC 的 `liuguang_ipc::protocol::Playlist` 一一对应（IPC crate 不依赖
/// core，故两边各有一份结构；转换在 `apps/liuguang`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistConfig {
    /// 播放列表 id（daemon 新建时生成为 `pl-N`）。
    pub id: String,
    /// 名称。
    #[serde(default)]
    pub name: String,
    /// 条目（缺省 = 空列表）。
    #[serde(default)]
    pub entries: Vec<PlaylistEntryConfig>,
    /// 轮换模式：`sequential` / `random`。
    #[serde(default = "default_playlist_mode")]
    pub mode: String,
    /// 切换间隔（秒，1..=86400）。
    #[serde(default = "default_playlist_interval_seconds")]
    pub interval_seconds: u32,
    /// 视频结束行为：`next` / `loop`。
    #[serde(default = "default_playlist_on_video_end")]
    pub on_video_end: String,
    /// 目标输出（缺省 = 全部/默认调度）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 是否不重复。
    #[serde(default)]
    pub no_repeat: bool,
}

fn default_gpu_pause_above() -> u8 {
    85
}

fn default_gpu_resume_below() -> u8 {
    75
}

fn default_detect_games() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_brightness() -> f64 {
    1.0
}

/// 播放列表缺省轮换模式（与 IPC 的 `default_playlist_mode` 保持一致）。
fn default_playlist_mode() -> String {
    "sequential".to_string()
}

/// 播放列表缺省间隔（与 IPC 的 `default_interval_seconds` 保持一致）。
fn default_playlist_interval_seconds() -> u32 {
    30
}

/// 播放列表缺省"视频结束行为"（与 IPC 的 `default_on_video_end` 保持一致）。
fn default_playlist_on_video_end() -> String {
    "next".to_string()
}

/// **播放模式**：把 `mode` + `on_video_end` 两个自由字符串收敛成一个穷尽枚举（T68）。
///
/// # 为什么需要这一层
///
/// 配置/IPC 里"播什么、什么时候换"由**两个**字符串表达
/// （`mode ∈ {sequential, random}`、`on_video_end ∈ {next, loop}`）。若把它们
/// 分别喂给不同代码路径，就会出现"两个字段各说各话"的组合歧义：视频播完之后
/// 到底看哪个字段？T68 之前的实现里 **谁都没看** —— 两个字段照常落盘、重启后
/// 仍在，但播放行为完全由 `[slideshow].directory` 的目录轮换决定。用户看到的
/// 正是"加了播放列表，播的还是原来那堆东西"。
///
/// 本枚举是**唯一判定点**：字符串 → 模式只有 [`playback_mode_from`] 一处映射，
/// daemon 只 match 枚举（不再各自解释字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackMode {
    /// 单视频循环：视频播到结尾**不前进**，同一项继续播
    /// （`on_video_end = "loop"`）。
    SingleLoop,
    /// 顺序循环：按条目顺序前进，末尾回卷到第一项（`mode = "sequential"`）。
    Sequential,
    /// 随机播放：随机前进，`no_repeat` 为真时避免相邻两项相同
    /// （`mode = "random"`）。
    Random {
        /// 是否避免"上一项刚播过就又抽到它"（相邻去重，不是全量去重）。
        no_repeat: bool,
    },
}

impl PlaybackMode {
    /// 稳定名字（只用于日志/诊断，**不是** IPC 契约；IPC 仍用原字符串）。
    pub fn as_str(self) -> &'static str {
        match self {
            PlaybackMode::SingleLoop => "single_loop",
            PlaybackMode::Sequential => "sequential",
            PlaybackMode::Random { .. } => "random",
        }
    }

    /// 视频播到结尾时是否**前进**到下一项 —— T68 的语义核心。
    ///
    /// 只有 `SingleLoop`（`on_video_end = "loop"`）不前进；`Sequential` /
    /// `Random` 都必须前进，否则"顺序/随机"永远停在第 1 项。
    pub fn advances_on_video_end(self) -> bool {
        !matches!(self, PlaybackMode::SingleLoop)
    }
}

/// 从两个配置字符串推导播放模式（**唯一映射点**，T68）。
///
/// 优先级：`on_video_end = "loop"` > `mode = "random"` > `"sequential"`。
///
/// 理由：`on_video_end` 是"这一条播完怎么办"的直接表述。用户显式写 `loop`
/// 就是要循环当前项 —— 此时"顺序/随机"没有意义（不前进就永远停在这一项）。
///
/// 未知取值按**缺省**处理（`sequential`）：校验层（`Config::validate` /
/// `update_playlist`）已经拒绝非法取值，这里只是不给 panic 的兜底，
/// 绝不让一个拼写错误把通道变成"什么都不播"。
pub fn playback_mode_from(on_video_end: &str, mode: &str, no_repeat: bool) -> PlaybackMode {
    if on_video_end.eq_ignore_ascii_case("loop") {
        return PlaybackMode::SingleLoop;
    }
    if mode.eq_ignore_ascii_case("random") {
        return PlaybackMode::Random { no_repeat };
    }
    PlaybackMode::Sequential
}

impl PlaylistConfig {
    /// 本播放列表的播放模式（[`playback_mode_from`] 的薄封装）。
    pub fn playback_mode(&self) -> PlaybackMode {
        playback_mode_from(&self.on_video_end, &self.mode, self.no_repeat)
    }
}

/// 顶层配置。所有段缺失时使用默认值（与 `config/example.toml` 一致）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 媒体库（`[library]`；缺省 `None` = 不启用媒体库，旧行为不变）。
    #[serde(default)]
    pub library: Option<crate::library::LibraryConfig>,
    /// 显示段。
    pub display: DisplayConfig,
    /// 媒体段。
    pub media: MediaConfig,
    /// 性能段。
    pub performance: PerformanceConfig,
    /// 轮换段。
    pub slideshow: SlideshowConfig,
    /// 转场段。
    pub transition: TransitionConfig,
    /// IPC 段。
    pub ipc: IpcConfig,
    /// 运行时持久化段。
    pub runtime: RuntimeConfig,
    /// 按输出名的显示参数。
    pub outputs: BTreeMap<String, OutputConfig>,
    /// 播放列表（`[[playlists]]`；`save_config` 落盘、启动时装载）。
    ///
    /// **T62**：此前播放列表只活在内存里（`save_config` 回 `saved=true` 但配置
    /// 零变化、重启即丢）。`#[serde(default)]` 保证**不含该段的旧配置照样解析**。
    ///
    /// **安全**：`entries[].media_id` 是用户可写的路径 ⇒ 装载时必须过
    /// `MediaRoot::resolve()`（`apps/liuguang::load_playlists_from_config`）。
    ///
    /// 放在字段末尾是刻意的：`to_toml_string` 按声明顺序输出，保持既有键的位置
    /// 不变能让 `save_config` 重写出的文件与用户原来的文件尽量只差新增段。
    /// `skip_serializing_if` 让**空列表完全不写出**（旧配置文件被 `save_config`
    /// 重写时不会凭空多出一行 `playlists = []`，也就不会有"值写在表之后"的排版问题）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlists: Vec<PlaylistConfig>,
}

impl Config {
    /// 规范 §10 中**尚未被 `apps/liuguang` 消费**的配置项清单。
    ///
    /// 这是一份"诚实的缺口清单"：这些字段能被解析、`validate()` 会通过，
    /// 但运行期不会因此发生任何变化。**新增实现时必须同时从本清单删除**，
    /// 并同步更新 `config/example.toml` 的注释。
    ///
    /// 设计理由：避免"写了配置却什么都没发生"的静默失效配置面；
    /// 既然当前不能全部实现，就必须让缺口可被程序读出，而不是只写在文档里。
    pub const PENDING_IMPLEMENTATION_OPTIONS: &'static [&'static str] = &[
        "display.vsync",
        "media.mute",
        "media.preferred_decoder",
        "slideshow.enabled",
    ];

    /// 从 TOML 文本解析（不做 I/O）。
    ///
    /// # Errors
    ///
    /// 语法/类型错误返回 [`ConfigError::Parse`]；语义非法返回 [`ConfigError::InvalidField`]。
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 从文件加载。
    ///
    /// # Errors
    ///
    /// 文件不可读返回 [`ConfigError::Io`]；其余同 [`Self::from_toml_str`]。
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// 把**仍是内置默认值**的路径字段替换为人格默认值。
    ///
    /// 为什么需要：`Config::default()` 里 `ipc.socket_path` /
    /// `runtime.state_file` 是系统人格的绝对路径（`/run/...`、`/var/lib/...`）。
    /// 用户服务读自己的配置时，若这两段没写，拿到的就是系统路径 ——
    /// 非 root 用户根本建不出来。这里只替换"等于内置默认值"的字段，
    /// 用户显式写过的路径原样保留。
    ///
    /// 已知取舍：用户若**刻意**把 socket 指回 `/run/liuguang/control.sock`，
    /// 会被当成"没写"而替换。这种写法在用户人格下本就不可用，无需支持。
    pub fn apply_persona_defaults(&mut self, persona: crate::persona::Persona) {
        if self.ipc.socket_path == std::path::Path::new("/run/liuguang/control.sock") {
            self.ipc.socket_path = persona.socket_path();
        }
        if self.runtime.state_file == std::path::Path::new("/var/lib/liuguang/state.json") {
            self.runtime.state_file = persona.state_path();
        }
    }

    /// 序列化为 TOML 文本（`save_config` 用；**不含注释**，规范化输出）。
    ///
    /// # Errors
    ///
    /// 序列化失败返回 [`ConfigError::Serialize`]（正常配置不会发生）。
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        let body =
            toml::to_string_pretty(self).map_err(|e| ConfigError::Serialize(e.to_string()))?;
        // 头注释如实说明：这是 daemon 生成的规范化输出，原文件里的注释/未知键不会保留。
        Ok(format!(
            "# 由流光（Liuguang）`save_config` 原子写入（规范化输出：注释与未知键不保留）\n{body}"
        ))
    }

    /// **原子**写回配置文件：同目录临时文件 → `sync_all` → `rename` → sync 目录。
    ///
    /// 语义保证：
    /// - 写入过程中崩溃/断电**不会**留下半截配置（rename 在同一文件系统内原子）；
    /// - 未授权用户（如 root 所有的 `/etc/liuguang/config.toml` + 非 root 进程）
    ///   得到 [`ConfigError::Io`] 且 `source.kind() == PermissionDenied`，
    ///   原文件**不被破坏**；
    /// - 返回写入字节数。
    ///
    /// # Errors
    ///
    /// 权限不足、目录不可写、rename 失败等返回 [`ConfigError::Io`]。
    pub fn save_atomic(&self, path: &Path) -> Result<usize, ConfigError> {
        let text = self.to_toml_string()?;
        write_atomic(path, text.as_bytes())?;
        Ok(text.len())
    }

    /// **本配置要求了、但当前不会发生**的行为清单（字段路径，顺序固定）。
    ///
    /// 空列表表示：当前配置提出的要求全部有实现——这不代表运行期没有别的缺陷，
    /// 只代表配置面没有"静默失效"。
    ///
    /// 判据（每项对应一条具体的未实现行为，而不是"字段是否存在"）：
    ///
    /// | 返回项 | 触发条件 | 未实现的行为 |
    /// |---|---|---|
    /// | `display.vsync` | `vsync = false`（要求非阻塞） | 非阻塞提交/跳过 vblank 未实现（`true` 默认已由阻塞提交满足） |
    /// | `media.mute` | `mute = false` | 无音频输出，无法满足"放音" |
    /// | `media.preferred_decoder` | 取值非默认（非 `vaapi`） | 解码器选择不使用该字段 |
    /// | `performance.pause_when_display_off` | `= true`（默认） | ——（2026-10-20 已实现：熄屏即停拉帧） |
    /// | `slideshow.enabled` | `= false` | 关闭轮换不生效（调度器照常排期） |
    ///
    /// **曾经在这里、现在已实现的**（2026-10-20 接线，别再放回来）：
    /// `performance.preload_next`、`performance.max_cache_mb`（`PreloadCache` 已接入
    /// `apps/liuguang`，预算即其淘汰依据）、`performance.reduce_load_on_cpu_pressure`
    /// （`apps/liuguang/src/loadshed.rs`：压力档 → 提交频率上限减半 + 暂停预加载）。
    ///
    /// 回报方式：`apps/liuguang` 在启动与 `reload` 后对非空结果打一条 `WARN`
    /// （逐项列出字段路径）——清单必须**能被用户看见**，否则"可被程序读出"
    /// 只是把静默失效挪进了代码里。
    pub fn pending_implementation(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        // `true`（默认）已由**阻塞原子提交**满足（等待 flip ⇒ 对齐 vblank）；
        // 只有用户显式要求 `false`（不等 vblank）时才是未实现的行为。
        if !self.display.vsync {
            out.push("display.vsync");
        }
        if !self.media.mute {
            out.push("media.mute");
        }
        if self.media.preferred_decoder != MediaConfig::default().preferred_decoder {
            out.push("media.preferred_decoder");
        }
        if !self.slideshow.enabled {
            out.push("slideshow.enabled");
        }
        out
    }

    /// 语义校验。错误携带字段路径。
    ///
    /// # Errors
    ///
    /// 任一字段越界/格式非法时返回 [`ConfigError::InvalidField`]。
    pub fn validate(&self) -> Result<(), ConfigError> {
        // display
        let dev = self.display.device.trim();
        if dev != "auto" && !dev.starts_with("/dev/dri/") {
            return Err(ConfigError::invalid_field(
                "display.device",
                format!("应为 `auto` 或 /dev/dri/ 下的绝对路径，实际: {dev}"),
            ));
        }
        self.display.mode_selection()?;

        // media
        if self.media.video_fps_limit == 0 || self.media.video_fps_limit > 240 {
            return Err(ConfigError::invalid_field(
                "media.video_fps_limit",
                format!("应在 1..=240，实际: {}", self.media.video_fps_limit),
            ));
        }
        if self.media.preferred_decoder.trim().is_empty() {
            return Err(ConfigError::invalid_field(
                "media.preferred_decoder",
                "不能为空",
            ));
        }

        // performance
        if self.performance.max_cache_mb == 0 {
            return Err(ConfigError::invalid_field(
                "performance.max_cache_mb",
                "必须 >= 1",
            ));
        }
        if !(1..=16).contains(&self.performance.frame_queue_size) {
            return Err(ConfigError::invalid_field(
                "performance.frame_queue_size",
                format!("应在 1..=16，实际: {}", self.performance.frame_queue_size),
            ));
        }

        // slideshow
        if self.slideshow.interval_seconds == 0 {
            return Err(ConfigError::invalid_field(
                "slideshow.interval_seconds",
                "必须 >= 1",
            ));
        }
        if self.slideshow.enabled && !self.slideshow.directory.is_absolute() {
            return Err(ConfigError::invalid_field(
                "slideshow.directory",
                format!(
                    "必须为绝对路径，实际: {}",
                    self.slideshow.directory.display()
                ),
            ));
        }

        // transition
        if self.transition.duration_ms > MAX_TRANSITION_DURATION_MS {
            return Err(ConfigError::invalid_field(
                "transition.duration_ms",
                format!(
                    "应在 0..={MAX_TRANSITION_DURATION_MS}，实际: {}",
                    self.transition.duration_ms
                ),
            ));
        }

        // media.allowed_roots
        for root in &self.media.allowed_roots {
            if !root.is_absolute() {
                return Err(ConfigError::invalid_field(
                    "media.allowed_roots",
                    format!("必须为绝对路径，实际: {}", root.display()),
                ));
            }
        }

        // media.transcode：`auto` / `off` / ffmpeg 可执行文件（绝对路径或 PATH 里的名字）
        let transcode = self.media.transcode.trim();
        if transcode.is_empty() {
            return Err(ConfigError::invalid_field(
                "media.transcode",
                "不能为空：应为 `auto`、`off` 或 ffmpeg 可执行文件路径".to_string(),
            ));
        }
        if self.media.transcode_max_width != 0
            && !(320..=7680).contains(&self.media.transcode_max_width)
        {
            return Err(ConfigError::invalid_field(
                "media.transcode_max_width",
                format!(
                    "应为 0（保持原分辨率）或 320..=7680，实际: {}",
                    self.media.transcode_max_width
                ),
            ));
        }
        if self.media.transcode_crf_or_bitrate.trim().is_empty() {
            return Err(ConfigError::invalid_field(
                "media.transcode_crf_or_bitrate",
                "不能为空：应为 `<encoder> [args…]`，如 `hevc_nvenc -b:v 20M`".to_string(),
            ));
        }

        // ipc / runtime
        if !self.ipc.socket_path.is_absolute() {
            return Err(ConfigError::invalid_field(
                "ipc.socket_path",
                format!("必须为绝对路径，实际: {}", self.ipc.socket_path.display()),
            ));
        }
        if !self.runtime.state_file.is_absolute() {
            return Err(ConfigError::invalid_field(
                "runtime.state_file",
                format!(
                    "必须为绝对路径，实际: {}",
                    self.runtime.state_file.display()
                ),
            ));
        }

        // outputs
        for (name, out) in &self.outputs {
            if name.trim().is_empty() {
                return Err(ConfigError::invalid_field("outputs", "输出名不能为空"));
            }
            if !(out.zoom.is_finite() && out.zoom > 0.0 && out.zoom <= MAX_ZOOM) {
                return Err(ConfigError::invalid_field(
                    format!("outputs.{name}.zoom"),
                    format!("应有限、> 0 且 <= {MAX_ZOOM}，实际: {}", out.zoom),
                ));
            }
            if !(out.offset_x.is_finite() && out.offset_y.is_finite()) {
                return Err(ConfigError::invalid_field(
                    format!("outputs.{name}.offset_x/offset_y"),
                    format!(
                        "必须为有限数值，实际: offset_x={}, offset_y={}",
                        out.offset_x, out.offset_y
                    ),
                ));
            }
            if !(out.brightness.is_finite()
                && (crate::transform::MIN_BRIGHTNESS..=crate::transform::MAX_BRIGHTNESS)
                    .contains(&out.brightness))
            {
                return Err(ConfigError::invalid_field(
                    format!("outputs.{name}.brightness"),
                    format!(
                        "应有限且位于 {}..={}，实际: {}",
                        crate::transform::MIN_BRIGHTNESS,
                        crate::transform::MAX_BRIGHTNESS,
                        out.brightness
                    ),
                ));
            }
            if let Some(fallback) = &out.fallback_image {
                if !fallback.is_absolute() {
                    return Err(ConfigError::invalid_field(
                        format!("outputs.{name}.fallback_image"),
                        format!("必须为绝对路径，实际: {}", fallback.display()),
                    ));
                }
            }
            if let Some(dir) = &out.media_directory {
                if !dir.is_absolute() {
                    return Err(ConfigError::invalid_field(
                        format!("outputs.{name}.media_directory"),
                        format!("必须为绝对路径，实际: {}", dir.display()),
                    ));
                }
            }
            if let Some(interval) = out.interval_seconds {
                if interval == 0 {
                    return Err(ConfigError::invalid_field(
                        format!("outputs.{name}.interval_seconds"),
                        "必须 >= 1",
                    ));
                }
            }
        }

        // playlists（T62：落盘数据结构）
        //
        // 这里只校验**结构**（枚举取值/区间/id 唯一/条目非空）——`entries[].media_id`
        // 的**路径边界**不在配置解析期判：它需要媒体库上下文（`MediaRoot`），
        // 且"越界"的处理方式是**丢弃该条目并告警**（而不是让整份配置/壁纸起不来）。
        // 见 `apps/liuguang::load_playlists_from_config`。
        let mut seen_ids: Vec<&str> = Vec::new();
        for (i, pl) in self.playlists.iter().enumerate() {
            if pl.id.trim().is_empty() {
                return Err(ConfigError::invalid_field(
                    format!("playlists[{i}].id"),
                    "不能为空",
                ));
            }
            if seen_ids.contains(&pl.id.as_str()) {
                return Err(ConfigError::invalid_field(
                    format!("playlists[{i}].id"),
                    format!("重复的播放列表 id: {}", pl.id),
                ));
            }
            seen_ids.push(pl.id.as_str());
            if !matches!(pl.mode.as_str(), "sequential" | "random") {
                return Err(ConfigError::invalid_field(
                    format!("playlists[{i}].mode"),
                    format!("必须是 sequential 或 random，实际: {}", pl.mode),
                ));
            }
            if !matches!(pl.on_video_end.as_str(), "next" | "loop") {
                return Err(ConfigError::invalid_field(
                    format!("playlists[{i}].on_video_end"),
                    format!("必须是 next 或 loop，实际: {}", pl.on_video_end),
                ));
            }
            if pl.interval_seconds == 0 || pl.interval_seconds > 86_400 {
                return Err(ConfigError::invalid_field(
                    format!("playlists[{i}].interval_seconds"),
                    format!("应在 1..=86400，实际: {}", pl.interval_seconds),
                ));
            }
            for (j, entry) in pl.entries.iter().enumerate() {
                if entry.media_id.trim().is_empty() {
                    return Err(ConfigError::invalid_field(
                        format!("playlists[{i}].entries[{j}].media_id"),
                        "不能为空",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// 同目录原子写（临时文件 + rename）。
///
/// 目标目录不可写 / 无权限 → `PermissionDenied`；失败时清理临时文件，**原文件不变**。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    use std::io::Write as _;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let io_err = |source: std::io::Error| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(io_err)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        // 目录项也要落盘，否则崩溃后可能"文件内容在但目录项指旧 inode"。
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = r#"
[display]
backend = "drm"
device = "auto"
mode = "preferred"
vsync = true

[media]
hardware_decode = "auto"
preferred_decoder = "vaapi"
video_fps_limit = 60
mute = true
software_decode_fallback = true

[performance]
preload_next = true
max_cache_mb = 128
frame_queue_size = 3
pause_when_display_off = true
reduce_load_on_cpu_pressure = true

[slideshow]
enabled = true
directory = "/srv/wallpapers"
interval_seconds = 600
order = "shuffle"
avoid_repeat = true

[transition]
enabled = true
effect = "fade"
duration_ms = 350
easing = "ease-in-out"

[outputs."HDMI-A-1"]
fit = "cover"
zoom = 1.0
offset_x = 0.0
offset_y = 0.0
rotation = "90"
fallback_image = "/srv/wallpapers/fallback.png"
"#;
        let cfg = Config::from_toml_str(text).unwrap();
        // 与**默认值**对齐而不是写死数字：默认值调整（30→60）不应让本测试变成
        // "忘了同步"的负担，但示例一旦与默认漂移仍要红。
        assert_eq!(
            cfg.media.video_fps_limit,
            Config::default().media.video_fps_limit
        );
        assert_eq!(cfg.transition.duration_ms, 350);
        assert!(cfg.outputs.contains_key("HDMI-A-1"));
        assert_eq!(cfg.outputs["HDMI-A-1"].rotation, Rotation::Deg90);
        assert_eq!(
            cfg.display.mode_selection().unwrap(),
            ModeSelection::Preferred
        );
    }

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.display.device, "auto");
        assert_eq!(cfg.performance.frame_queue_size, 3);
    }

    // ---- D1：显示后端选择（auto/wayland/drm） ----

    /// 三种后端都能从 TOML 解析；未知值被拒绝（不静默回退）。
    ///
    /// 配置文件是**严格**的：只接受规范写法 `auto`/`wayland`/`drm`，
    /// 便于人读与 grep。宽松别名（`wl`/`kms`/大小写）只走
    /// [`DisplayBackendKind::parse`]（CLI/env 用）。
    #[test]
    fn display_backend_kinds_parse_from_toml() {
        for (text, want) in [
            ("auto", DisplayBackendKind::Auto),
            ("wayland", DisplayBackendKind::Wayland),
            ("drm", DisplayBackendKind::Drm),
        ] {
            let cfg = Config::from_toml_str(&format!("[display]\nbackend = \"{text}\"\n")).unwrap();
            assert_eq!(cfg.display.backend, want, "backend = {text:?}");
        }
        for bad in ["x11", "wl", "kms", "WAYLAND", ""] {
            assert!(
                Config::from_toml_str(&format!("[display]\nbackend = \"{bad}\"\n")).is_err(),
                "非法后端 {bad:?} 必须报错，不能静默回退到默认值"
            );
        }
    }

    /// 默认值必须是 `auto`：同一份配置在桌面会话与 TTY 上各自选中正确人格。
    #[test]
    fn display_backend_default_is_auto() {
        assert_eq!(DisplayBackendKind::default(), DisplayBackendKind::Auto);
        assert_eq!(Config::default().display.backend, DisplayBackendKind::Auto);
    }

    /// `as_str`/`parse` 往返；`resolved` 只把 `auto` 具体化。
    #[test]
    fn display_backend_round_trips_and_resolves() {
        for kind in [
            DisplayBackendKind::Auto,
            DisplayBackendKind::Wayland,
            DisplayBackendKind::Drm,
        ] {
            assert_eq!(DisplayBackendKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(DisplayBackendKind::parse("nope"), None);
        // 非 auto 原样返回，与运行环境无关。
        assert_eq!(
            DisplayBackendKind::Wayland.resolved(),
            DisplayBackendKind::Wayland
        );
        assert_eq!(DisplayBackendKind::Drm.resolved(), DisplayBackendKind::Drm);
    }

    #[test]
    fn output_rotation_parses_and_defaults() {
        // 缺省时默认 0°。
        let cfg = Config::from_toml_str("[outputs.\"HDMI-A-1\"]\n").unwrap();
        assert_eq!(cfg.outputs["HDMI-A-1"].rotation, Rotation::Deg0);
        assert_eq!(OutputConfig::default().rotation, Rotation::Deg0);

        for (text, expect) in [
            ("\"0\"", Rotation::Deg0),
            ("\"90\"", Rotation::Deg90),
            ("\"180\"", Rotation::Deg180),
            ("\"270\"", Rotation::Deg270),
        ] {
            let cfg =
                Config::from_toml_str(&format!("[outputs.\"HDMI-A-1\"]\nrotation = {text}\n"))
                    .unwrap();
            assert_eq!(cfg.outputs["HDMI-A-1"].rotation, expect, "配置值: {text}");
        }

        // 非法值（含旧 snake_case 写法）报解析错误。
        for bad in ["\"deg90\"", "\"90deg\"", "90"] {
            let err = Config::from_toml_str(&format!("[outputs.\"HDMI-A-1\"]\nrotation = {bad}\n"))
                .unwrap_err();
            assert!(
                matches!(err, ConfigError::Parse(_)),
                "配置值 {bad} 应解析失败，实际: {err:?}"
            );
        }
    }

    #[test]
    fn parse_error_reports_location() {
        let err = Config::from_toml_str("[display]\nvsync = \"not-a-bool\"\n").unwrap_err();
        match err {
            ConfigError::Parse(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("1") || msg.contains("line"),
                    "应含行列位置: {msg}"
                );
            }
            other => panic!("期望 Parse 错误，实际: {other:?}"),
        }
    }

    #[test]
    fn semantic_errors_carry_field_paths() {
        let cases: &[(&str, &str)] = &[
            ("[display]\ndevice = \"card0\"", "display.device"),
            ("[display]\nmode = \"1920x\"", "display.mode"),
            ("[media]\nvideo_fps_limit = 0", "media.video_fps_limit"),
            ("[media]\nvideo_fps_limit = 300", "media.video_fps_limit"),
            (
                "[performance]\nframe_queue_size = 99",
                "performance.frame_queue_size",
            ),
            (
                "[performance]\nmax_cache_mb = 0",
                "performance.max_cache_mb",
            ),
            (
                "[slideshow]\ninterval_seconds = 0",
                "slideshow.interval_seconds",
            ),
            (
                "[slideshow]\ndirectory = \"relative/dir\"",
                "slideshow.directory",
            ),
            (
                "[transition]\nduration_ms = 60000",
                "transition.duration_ms",
            ),
            (
                "[outputs.\"HDMI-A-1\"]\nzoom = 0.0",
                "outputs.HDMI-A-1.zoom",
            ),
            (
                "[outputs.\"HDMI-A-1\"]\nzoom = 99.0",
                "outputs.HDMI-A-1.zoom",
            ),
            (
                "[outputs.\"HDMI-A-1\"]\nfallback_image = \"rel.png\"",
                "outputs.HDMI-A-1.fallback_image",
            ),
            (
                "[outputs.\"HDMI-A-1\"]\noffset_x = nan",
                "outputs.HDMI-A-1.offset_x/offset_y",
            ),
        ];
        for (toml_text, field) in cases {
            let err = Config::from_toml_str(toml_text).unwrap_err();
            match &err {
                ConfigError::InvalidField { field: f, .. } => {
                    assert_eq!(f, field, "配置: {toml_text}");
                }
                other => panic!("期望 InvalidField，实际 {other:?}；配置: {toml_text}"),
            }
        }
    }

    #[test]
    fn nan_offset_rejected_in_toml() {
        // TOML 不支持裸 `nan` 关键字时走 Parse 错误，两者皆可接受（都带位置/字段信息）。
        let err = Config::from_toml_str("[outputs.\"HDMI-A-1\"]\noffset_x = nan\n").unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidField { .. } | ConfigError::Parse(_)
        ));
    }

    #[test]
    fn mode_selection_parsing() {
        assert_eq!(
            ModeSelection::parse("preferred").unwrap(),
            ModeSelection::Preferred
        );
        assert_eq!(
            ModeSelection::parse("1920x1080").unwrap(),
            ModeSelection::Manual {
                width: 1920,
                height: 1080,
                refresh_millihz: None
            }
        );
        assert_eq!(
            ModeSelection::parse("3840x2160@60").unwrap(),
            ModeSelection::Manual {
                width: 3840,
                height: 2160,
                refresh_millihz: Some(60_000)
            }
        );
        assert!(ModeSelection::parse("best").is_err());
        assert!(ModeSelection::parse("0x1080").is_err());
        assert!(ModeSelection::parse("1920x1080@0").is_err());
    }

    #[test]
    fn defaults_match_documented_example() {
        let cfg = Config::default();
        assert_eq!(cfg.transition.effect, TransitionEffect::Fade);
        assert_eq!(cfg.transition.duration_ms, 350);
        assert_eq!(cfg.transition.easing, Easing::EaseInOut);
        assert_eq!(cfg.slideshow.directory, PathBuf::from("/srv/wallpapers"));
        assert_eq!(cfg.slideshow.interval_seconds, 600);
        assert_eq!(cfg.slideshow.order, PlaylistOrder::Shuffle);
        assert_eq!(cfg.media.hardware_decode, HardwareDecode::Auto);
        assert!(cfg.media.mute);
    }

    #[test]
    fn load_reports_missing_file() {
        let err = Config::load(Path::new("/nonexistent/liuguang.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    /// 真实随附配置必须可解析：`config/example.toml` 是文档与部署的入口，
    /// 它一旦与 `Config` 漂移（字段改名/漏字段），这里就会红。
    #[test]
    fn shipped_example_config_parses() {
        let text = include_str!("../../../config/example.toml");
        let cfg = Config::from_toml_str(text)
            .unwrap_or_else(|e| panic!("config/example.toml 无法解析: {e}"));
        assert_eq!(
            cfg.media.video_fps_limit,
            Config::default().media.video_fps_limit,
            "随附示例必须与默认值一致（否则部署后行为与文档不同）"
        );
        assert_eq!(cfg.transition.duration_ms, 350);
        // 示例必须给出输出段，确保默认示例即可覆盖常见多显示接口。
        assert!(
            cfg.outputs.contains_key("DP-1") && cfg.outputs.contains_key("HDMI-A-1"),
            "示例配置应同时覆盖 DP-1 与 HDMI-A-1，实际: {:?}",
            cfg.outputs.keys().collect::<Vec<_>>()
        );
    }

    /// **示例配置必须声明 `[library]`**（t61）。
    ///
    /// 为什么单独立一条：`[library]` 是**可选段**（`Config.library: Option<LibraryConfig>`），
    /// 缺了它 `liuguang check-config` **仍报"配置有效"**（已实测）、服务也照常启动，
    /// 但媒体库相关 IPC（`list_media`/`list_sources`/`scan_source`）会返回
    /// `INVALID_PARAMS` + `detail_code=NOT_CONFIGURED`，TUI 媒体库页显示红字
    /// ⇒ **部署后用户会以为"没有这个功能"**。
    /// 因此随附示例**必须**带上它；本断言就是防它再被删掉。
    /// **变异**：删掉 `config/example.toml` 里的 `[library]` 段 ⇒ 本测试变红。
    #[test]
    fn shipped_example_config_declares_library() {
        let text = include_str!("../../../config/example.toml");
        let cfg = Config::from_toml_str(text)
            .unwrap_or_else(|e| panic!("config/example.toml 无法解析: {e}"));
        assert!(
            cfg.library.is_some(),
            "config/example.toml 必须声明 [library]：该段可选 ⇒ 缺段不会让 check-config 报错，\
             但会让媒体库在部署后不可用（list_media 返回 NOT_CONFIGURED、TUI 红字），\
             用户会误以为功能不存在"
        );
    }

    /// 未知字段必须**报错并带行列位置**，而不是被静默忽略。
    ///
    /// 这条测试是"静默失效"的守门员：删掉 `deny_unknown_fields` 它就会红。
    #[test]
    fn unknown_field_is_rejected_with_location() {
        let cases: &[(&str, &str)] = &[
            ("[display]\nvync = true\n", "vync"),
            ("[media]\nhardware_decod = \"auto\"\n", "hardware_decod"),
            (
                "[outputs.\"DP-1\"]\nfit = \"cover\"\nzoomm = 2.0\n",
                "zoomm",
            ),
            (
                "[performance]\nmax_cache_mb = 64\nmax_cache = 64\n",
                "max_cache",
            ),
        ];
        for (toml_text, bad_key) in cases {
            let err = Config::from_toml_str(toml_text).unwrap_err();
            let ConfigError::Parse(e) = &err else {
                panic!("期望 Parse 错误（未知字段），实际: {err:?}；配置: {toml_text}");
            };
            let msg = e.to_string();
            assert!(msg.contains(bad_key), "错误信息应含字段名 {bad_key}: {msg}");
            // toml 的错误带行列位置（span）——"明确错误位置"的验收要求。
            assert!(
                msg.contains("line") || msg.contains('行') || msg.contains("1:"),
                "错误信息应含位置: {msg}"
            );
        }
    }

    /// `pending_implementation()` 的判定矩阵：只有"配置要求了但不会发生"才出现。
    #[test]
    fn pending_implementation_reports_only_unsatisfied_requests() {
        // 默认配置：`vsync = true` 已由**阻塞原子提交**满足（ioctl 等 flip 完成后才
        // 返回 ⇒ 对齐 vblank），因此**不得**再报成缺口（2026-10-28 更正：旧实现
        // 把已生效的默认值报成未实现，属于反向的"文档说谎"）。
        let dflt = Config::default();
        let pending = dflt.pending_implementation();
        assert!(
            !pending.contains(&"display.vsync"),
            "vsync=true 已生效，不得报为缺口，实际: {pending:?}"
        );
        assert!(
            pending.is_empty(),
            "默认配置不应包含任何未实现项，实际: {pending:?}"
        );
        assert!(
            !pending.contains(&"performance.pause_when_display_off"),
            "熄屏暂停已实现，不得再报为缺口：{pending:?}"
        );
        // 2026-10-20 已接线的三项**不得**再出现在缺口清单里：
        // 它们各自有运行期行为（预加载 / 字节预算淘汰 / 压力降载），
        // 留着就是"实现已完成但忘记摘掉"的僵尸项（与 `every_registered_gap_is_reachable` 同族）。
        for done in [
            "performance.preload_next",
            "performance.max_cache_mb",
            "performance.reduce_load_on_cpu_pressure",
            // 2026-10-20：熄屏暂停已接线（display_power 读 sysfs 的 dpms/status）。
            "performance.pause_when_display_off",
        ] {
            assert!(
                !pending.contains(&done) && !Config::PENDING_IMPLEMENTATION_OPTIONS.contains(&done),
                "{done} 已实现，仍被当作缺口上报"
            );
        }
        // 默认值本身没有"要求放音"或"关闭轮换"，因此不该出现在列表里。
        assert!(!pending.contains(&"media.mute"));
        assert!(!pending.contains(&"slideshow.enabled"));
        assert!(!pending.contains(&"media.preferred_decoder"));

        // 每一项都必须是已登记的缺口（防止返回未登记/拼错的字段路径）。
        for p in &pending {
            assert!(
                Config::PENDING_IMPLEMENTATION_OPTIONS.contains(p),
                "{p} 未登记在 PENDING_IMPLEMENTATION_OPTIONS"
            );
        }

        // 反向：用户显式要求的"未实现行为"必须出现。
        let mut cfg = Config::default();
        cfg.media.mute = false;
        cfg.slideshow.enabled = false;
        cfg.media.preferred_decoder = "openh264".into();
        cfg.display.vsync = false;
        let pending = cfg.pending_implementation();
        // `vsync = false` 要求非阻塞提交（不等 flip）：这是**未实现**的行为，
        // 必须报出来，而不是静默继续用阻塞提交。
        for expect in [
            "display.vsync",
            "media.mute",
            "slideshow.enabled",
            "media.preferred_decoder",
        ] {
            assert!(pending.contains(&expect), "应报告 {expect}: {pending:?}");
        }
    }

    /// 清单本身不允许留下"实现已完成但忘记摘掉"的僵尸项：
    /// 每一项都必须能被某个配置触发（否则就是过期的文档）。
    #[test]
    fn every_registered_gap_is_reachable() {
        let mut cfg = Config::default();
        cfg.media.mute = false;
        cfg.slideshow.enabled = false;
        cfg.media.preferred_decoder = "x".into();
        cfg.display.vsync = false;
        let mut covered: Vec<&'static str> = cfg.pending_implementation();
        cfg.display.vsync = true;
        for p in cfg.pending_implementation() {
            if !covered.contains(&p) {
                covered.push(p);
            }
        }
        for registered in Config::PENDING_IMPLEMENTATION_OPTIONS {
            assert!(
                covered.contains(registered),
                "{registered} 无法被任何配置触发，说明已实现却从未从清单摘除"
            );
        }
    }

    /// 文档同步守卫：`config/example.toml` 里带 `[未生效]` 标记的字段集合，必须
    /// **恰好等于** [`Config::PENDING_IMPLEMENTATION_OPTIONS`]。
    ///
    /// 这条规则原本只写在注释里（"两张清单必须保持同步"），靠人 remember ⇒ 一定会漂。
    /// 接线一个配置项时若忘了摘标记，用户会看到"这字段没用"；反之若忘了打标记，
    /// 用户会以为改了它就有效果。两个方向都是静默失效。
    #[test]
    fn example_toml_markers_match_the_registered_gap_list() {
        let text = include_str!("../../../config/example.toml");
        let mut section = "";
        let mut marked: Vec<String> = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some(head) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = head;
                continue;
            }
            if line.contains("[未生效]")
                && let Some((key, _)) = line.split_once('=')
            {
                marked.push(format!("{section}.{}", key.trim()));
            }
        }
        marked.sort();
        marked.dedup();
        let mut registered: Vec<String> = Config::PENDING_IMPLEMENTATION_OPTIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        registered.sort();
        assert_eq!(
            marked, registered,
            "example.toml 的 `[未生效]` 标记与 PENDING_IMPLEMENTATION_OPTIONS 不一致：\
             接线后忘摘标记（左多）或实现前忘打标记（右多）都会让配置文档继续骗人"
        );
    }

    // ---- T62：播放列表落盘（`[[playlists]]`） ----

    /// `[[playlists]]` 能往返：解析 → 序列化 → 再解析，内容不变。
    ///
    /// 这是"配置文件是人读的"底线：`save_config` 写出的东西必须能被自己读回来
    /// （否则 `verified` 永远是 false，或更糟——写进去但读不回来）。
    #[test]
    fn playlists_round_trip_through_toml() {
        let text = r#"
[[playlists]]
id = "pl-1"
name = "晚间"
mode = "random"
interval_seconds = 600
on_video_end = "loop"
no_repeat = true

[[playlists.entries]]
media_id = "/srv/wallpapers/a.mp4"

[[playlists.entries]]
media_id = "/srv/wallpapers/b.jpg"
"#;
        let cfg = Config::from_toml_str(text).expect("含 [[playlists]] 的配置必须能解析");
        assert_eq!(cfg.playlists.len(), 1);
        let pl = &cfg.playlists[0];
        assert_eq!(pl.id, "pl-1");
        assert_eq!(pl.mode, "random");
        assert_eq!(pl.interval_seconds, 600);
        assert_eq!(pl.on_video_end, "loop");
        assert!(pl.no_repeat);
        assert_eq!(pl.entries.len(), 2);
        assert_eq!(pl.entries[1].media_id, "/srv/wallpapers/b.jpg");

        let written = cfg.to_toml_string().expect("必须能序列化");
        assert!(
            written.contains("[[playlists]]"),
            "写出的配置必须含 [[playlists]] 段：\n{written}"
        );
        assert!(
            written.contains("/srv/wallpapers/a.mp4"),
            "写出的配置必须含条目路径：\n{written}"
        );
        let back = Config::from_toml_str(&written).expect("自己写出的配置必须能被自己读回");
        assert_eq!(back.playlists, cfg.playlists, "往返后播放列表必须一致");
    }

    /// 播放列表的**缺省值**与 IPC 侧一致（`mode=sequential` / 30s / `next`）。
    #[test]
    fn playlist_defaults_match_ipc_contract() {
        let text = "[[playlists]]\nid = \"pl-1\"\n";
        let cfg = Config::from_toml_str(text).expect("只写 id 也应合法");
        let pl = &cfg.playlists[0];
        assert_eq!(pl.mode, "sequential");
        assert_eq!(pl.interval_seconds, 30);
        assert_eq!(pl.on_video_end, "next");
        assert!(pl.entries.is_empty());
        assert!(!pl.no_repeat);
        assert_eq!(pl.output, None);
    }

    /// **向后兼容**：不含 `[[playlists]]` 的旧配置必须照常解析，且写回时**不**凭空
    /// 多出一个空段（否则老用户的配置会被无谓改写）。
    #[test]
    fn legacy_config_without_playlists_still_parses() {
        let text = r#"
[display]
device = "auto"
mode = "preferred"
vsync = false
"#;
        let cfg = Config::from_toml_str(text).expect("旧配置必须仍能解析");
        assert!(
            cfg.playlists.is_empty(),
            "旧配置里没有播放列表 ⇒ 必须是空列表（`#[serde(default)]`）"
        );
        let written = cfg.to_toml_string().expect("必须能序列化");
        assert!(
            !written.contains("playlists"),
            "空播放列表不应写出 [[playlists]] 段：\n{written}"
        );
    }

    /// 结构非法的播放列表在 `validate()` 就被拒（**带字段路径**，不是静默忽略）。
    ///
    /// 覆盖四种：`mode` 取值、`on_video_end` 取值、`interval_seconds` 区间、
    /// 重复 `id`（重复会让 `update_playlist` 的"按 id 更新"语义变得不确定）。
    #[test]
    fn invalid_playlists_are_rejected_with_field_path() {
        for (name, body, want) in [
            (
                "mode",
                "[[playlists]]\nid=\"p\"\nmode=\"shuffle\"\n",
                "playlists[0].mode",
            ),
            (
                "on_video_end",
                "[[playlists]]\nid=\"p\"\non_video_end=\"stop\"\n",
                "playlists[0].on_video_end",
            ),
            (
                "interval",
                "[[playlists]]\nid=\"p\"\ninterval_seconds=0\n",
                "playlists[0].interval_seconds",
            ),
            (
                "dup-id",
                "[[playlists]]\nid=\"p\"\n[[playlists]]\nid=\"p\"\n",
                "playlists[1].id",
            ),
            (
                "empty-media-id",
                "[[playlists]]\nid=\"p\"\n[[playlists.entries]]\nmedia_id=\"\"\n",
                "playlists[0].entries[0].media_id",
            ),
        ] {
            let err =
                Config::from_toml_str(body).expect_err(&format!("{name}：非法播放列表必须被拒"));
            let msg = err.to_string();
            assert!(
                msg.contains(want),
                "{name}：错误信息必须带字段路径 {want}，实际: {msg}"
            );
        }
    }

    /// T68：三种播放模式由 `mode` + `on_video_end` 的**唯一映射点**决定。
    ///
    /// 这条测试就是"两个字符串到底怎么解释"的规格：改映射必然改这里。
    #[test]
    fn playback_mode_maps_the_three_documented_combinations() {
        use super::{PlaybackMode, playback_mode_from};
        // ① 单视频循环：`on_video_end = "loop"` —— **不前进**。
        for mode in ["sequential", "random"] {
            let m = playback_mode_from("loop", mode, true);
            assert_eq!(
                m,
                PlaybackMode::SingleLoop,
                "on_video_end=loop + mode={mode} 必须是单视频循环"
            );
            assert!(
                !m.advances_on_video_end(),
                "单视频循环在视频播完时**不得**前进（否则永远看不到「重复同一项」）"
            );
        }
        // ② 顺序循环。
        let m = playback_mode_from("next", "sequential", true);
        assert_eq!(m, PlaybackMode::Sequential);
        assert!(m.advances_on_video_end(), "顺序循环播完必须前进");
        // ③ 随机播放，`no_repeat` 必须原样带过去（不是恒真/恒假）。
        assert_eq!(
            playback_mode_from("next", "random", true),
            PlaybackMode::Random { no_repeat: true }
        );
        assert_eq!(
            playback_mode_from("next", "random", false),
            PlaybackMode::Random { no_repeat: false },
            "no_repeat=false 不得被悄悄改成 true"
        );
        // 大小写不敏感（配置文件是手写的，不该因为 "Loop" 就静默变语义）。
        assert_eq!(
            playback_mode_from("LOOP", "RANDOM", false),
            PlaybackMode::SingleLoop
        );
        // 未知取值 ⇒ 缺省（sequential），且**必须**前进（不能什么都不播）。
        let m = playback_mode_from("garbage", "garbage", false);
        assert_eq!(m, PlaybackMode::Sequential);
        assert!(m.advances_on_video_end());
        // `as_str` 是日志契约（三个取值互不相同，便于 grep）。
        assert_eq!(PlaybackMode::SingleLoop.as_str(), "single_loop");
        assert_eq!(PlaybackMode::Sequential.as_str(), "sequential");
        assert_eq!(PlaybackMode::Random { no_repeat: true }.as_str(), "random");
    }

    /// T68：`PlaylistConfig::playback_mode()` 必须用**它自己的**三个字段，
    /// 而不是任何全局默认（否则"改了一条播放列表"会影响到别的通道）。
    #[test]
    fn playlist_config_playback_mode_uses_its_own_fields() {
        use super::PlaybackMode;
        let mut pl = super::PlaylistConfig {
            id: "p1".into(),
            name: String::new(),
            entries: Vec::new(),
            mode: "random".into(),
            interval_seconds: 5,
            on_video_end: "next".into(),
            output: None,
            no_repeat: true,
        };
        assert_eq!(pl.playback_mode(), PlaybackMode::Random { no_repeat: true });
        pl.on_video_end = "loop".into();
        assert_eq!(pl.playback_mode(), PlaybackMode::SingleLoop);
        assert!(!pl.playback_mode().advances_on_video_end());
    }

    #[test]
    fn save_atomic_creates_missing_parent_directory() {
        let dir = std::env::temp_dir().join(format!("liuguang-save-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg_path = dir.join("nested").join("liuguang").join("config.toml");
        let cfg = Config::default();
        let bytes = cfg
            .save_atomic(&cfg_path)
            .expect("missing parent dir should be created automatically");
        assert!(bytes > 0);
        assert!(cfg_path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
