//! `liuguang-display` — 显示后端抽象。
//!
//! 真实 DRM/KMS 实现位于 [`drm_kms`]（Linux；池化 dumb buffer + Atomic KMS）。
//! 本 crate 提供：
//! - [`DisplayBackend`] trait：上层注入点（模拟或真实设备）；
//! - [`selection`]：输出/模式/CRTC 选择纯逻辑（可完整单测）；
//! - [`mock`]：可注入的模拟设备；**复用真实的池化 + 上传实现**，
//!   因此"不每帧分配/映射""每帧拷贝字节数"可在无硬件环境断言；
//! - [`drm_kms`]：真实 DRM 后端（设备动态发现，不写死 cardN）。
//!
//! # 已实测的关键行为（真机 DP-1 3840x2400@60，详见 `crates/liuguang-display/PERF.md`）
//!
//! - 尺寸/格式相同的连续提交**不重新分配缓冲、不重新映射**
//!   （715 次提交 → `allocations=3, mappings=3`）；
//! - 缩放交给 DRM 平面（`SRC_*`/`CRTC_*`），CPU 只做一次上传拷贝
//!   （4K NV12 = 12 441 600 B/帧，2.79 ms/帧）；
//! - 提交为阻塞 page-flip（对齐 vblank），`EBUSY` 重试，失败必可见；
//!   计数语义与对账用法见 [`CommitStats`]（**不是"上屏"证据**）。
//!
//! # 显示路径：只有「CPU 上传」一条（直通已于 2026-10-20 移除）
//!
//! 决策背景：DMA-BUF 直通路径（`present_dmabuf`）虽然每帧 CPU 更低（实测约 6.4%，
//! 而 CPU 上传约 14%），但在真机上存在**局部横带/重影**：解码器会在该缓冲仍被扫描
//! 输出时重写它（同缓冲提交前后逐字节比对实测，重写只覆盖部分行 ⇒ 稳定混合帧）。
//! 已实测否证 7 条修法（keepalive 容量、VPP 自带池、`IN_FENCE_FD`、tiling 布局本身、
//! 外部池、池配置原因、裸 `GstVaPool`），根因未解。
//!
//! 因此**移除直通，只保留 CPU 上传**：多一次全帧拷贝，换取画面正确。
//! 完整的诊断链与判据方法论见 `docs/PIPELINE_AND_SYNCHRONIZATION.md`（保留为历史分析）。
//!
//! 职责边界（规范 §3.3）：本 crate 不解码 PNG/MP4。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod mock;
pub mod selection;

/// 扫描输出缓冲池化与上传路径（平台无关：不依赖 DRM，可离线验证）。
mod scanout;

/// DRM `IN_FORMATS` blob 解析与 (fourcc, modifier) 支持判定（纯逻辑，可离线验证）。
pub mod in_formats;

/// **帧内容正确性判据**：对 DMA-BUF/CPU 帧做只读 mmap + CRC，用于区分
/// "缓冲内容错"与"显示读取错"（同格式比较，**不把 tiled 缓冲当图片看**）。
pub mod frame_probe;

/// DRM master 状态、占用者诊断与失败日志限速（纯逻辑，可离线验证）。
pub mod master;

/// 扫描缓冲池化统计：`allocations`/`mappings` 在稳态下应与池深相等，
/// 用于证明"连续提交不重新分配、不重新映射"。
pub use scanout::PoolStats;

/// 真实 DRM/KMS 后端（仅 Linux；M1 静态扫描路径）。
#[cfg(target_os = "linux")]
pub mod drm_kms;

use crate::master::MasterStatus;
use liuguang_core::config::ModeSelection;
use liuguang_core::types::{DisplayFrame, DisplayMode, OutputId, OutputInfo, PresentToken};

/// 显示提交计数（诊断用）。
///
/// # 语义（**不是"上屏"证据**，t17 审计 A.8 ②）
///
/// - [`CommitStats::commit_ok`] 在**阻塞** `atomic_commit` 返回 `Ok` 后自增
///   （提交 flags 不含 `NONBLOCK`）——它只说明**内核接受了这次提交**，
///   **不代表**该帧已经被扫描输出到屏幕；
/// - **成功总数 = `commit_ok + retried_after_busy + legacy_fallback`**
///   （`commit_ok` **不含**重试后成功的那部分）；
/// - [`CommitStats::failed`] 只表示"这条路径全部尝试失败、该帧未被提交"。
///
/// # 与媒体侧 `presented` 对账
///
/// `presented`（媒体侧出队计数）与这里的 `failed` **必须能互相验账**：
/// "`presented` 在涨、`failed` 也在涨"就是**异常签名**（帧被取出但没提交成功），
/// 本项目已多次因"只有一个信号、而它恰好在故障下失真"而误判。
///
/// 用法（上层在每次 `present*` 前后各取一次，见 [`CommitStats::delta_since`]）：
///
/// ```no_run
/// # use liuguang_display::{CommitStats, DisplayBackend};
/// # fn demo(backend: &mut dyn DisplayBackend) {
/// let before = backend.commit_stats().unwrap_or_default();
/// let result = backend.present(
///     liuguang_core::types::OutputId::new("DP-1"),
///     // ...帧...
///     unreachable!(),
/// );
/// if result.is_err() {
///     let delta = backend.commit_stats().unwrap_or_default().delta_since(&before);
///     assert!(delta.failed >= 1, "提交失败必须能在计数里看到");
/// }
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitStats {
    /// **直接**成功返回 `Ok` 的提交次数（不含重试后成功者，也不代表"已上屏"）。
    pub commit_ok: u64,
    /// 因 `EBUSY`（上一次 page-flip 仍在途）重试后成功的次数。
    pub retried_after_busy: u64,
    /// 走 legacy `set_crtc` 成功回退的次数。
    pub legacy_fallback: u64,
    /// **该帧未被提交**的次数（>0 时上层必须让用户可见）。
    pub failed: u64,
}

impl CommitStats {
    /// 是否发生过"该帧未提交"的情况。
    pub fn has_failed_frames(&self) -> bool {
        self.failed > 0
    }

    /// 成功提交总数 = 直接成功 + 重试成功 + legacy 回退成功。
    ///
    /// 注意：这是**提交**成功数，不是"上屏/呈现"数。
    pub fn commit_success_total(&self) -> u64 {
        self.commit_ok
            .saturating_add(self.retried_after_busy)
            .saturating_add(self.legacy_fallback)
    }

    /// 总提交尝试次数（成功 + 失败）。
    pub fn attempts(&self) -> u64 {
        self.commit_success_total().saturating_add(self.failed)
    }

    /// 相对更早的快照的**增量**——上层在 `present*` 返回 `Err` 后立即调用它，
    /// 即可同步拿到"刚才这次提交失败了几次"，用于与媒体侧 `presented` 对账。
    pub fn delta_since(&self, earlier: &CommitStats) -> CommitDelta {
        CommitDelta {
            commit_ok: self.commit_ok.saturating_sub(earlier.commit_ok),
            retried_after_busy: self
                .retried_after_busy
                .saturating_sub(earlier.retried_after_busy),
            legacy_fallback: self.legacy_fallback.saturating_sub(earlier.legacy_fallback),
            failed: self.failed.saturating_sub(earlier.failed),
        }
    }
}

/// 两次 [`CommitStats`] 快照之间的**增量**（对账用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitDelta {
    /// 本次区间内直接成功的提交数。
    pub commit_ok: u64,
    /// 本次区间内重试后成功的提交数。
    pub retried_after_busy: u64,
    /// 本次区间内 legacy 回退成功的提交数。
    pub legacy_fallback: u64,
    /// 本次区间内**未提交成功**的次数。
    pub failed: u64,
}

impl CommitDelta {
    /// 区间内提交成功总数。
    pub fn commit_success_total(&self) -> u64 {
        self.commit_ok
            .saturating_add(self.retried_after_busy)
            .saturating_add(self.legacy_fallback)
    }

    /// 区间内提交尝试总数。
    pub fn attempts(&self) -> u64 {
        self.commit_success_total().saturating_add(self.failed)
    }

    /// 是否出现"该帧未提交"。
    pub fn has_failed_frames(&self) -> bool {
        self.failed > 0
    }

    /// 与媒体侧 `presented` 增量对账：**异常签名**判定。
    ///
    /// 返回 `true` 表示出现"媒体侧说取出了/呈现了 `presented_delta` 帧，
    /// 但显示侧同期有 `failed` 次未提交成功"——说明画面并不像 `presented` 显示的那样正常。
    /// 这是把"两个计数互相对账"变成可断言检查的那一步。
    pub fn contradicts_presented(&self, presented_delta: u64) -> bool {
        presented_delta > 0 && self.failed > 0
    }
}

/// 显示后端错误。
#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    /// 设备节点/驱动不可用。
    #[error("显示设备不可用: {0}")]
    DeviceUnavailable(String),
    /// 输出不存在。
    #[error("输出 {0} 不存在")]
    OutputNotFound(OutputId),
    /// 模式不受支持。
    #[error("输出 {output} 不支持模式 {mode:?}")]
    ModeNotSupported {
        /// 目标输出。
        output: OutputId,
        /// 请求模式。
        mode: DisplayMode,
    },
    /// 后端处于挂起状态（VT 切换/权限失效）。
    #[error("显示后端已挂起，需先 resume")]
    Suspended,
    /// 帧不合法。
    #[error("帧不合法: {0}")]
    InvalidFrame(#[from] liuguang_core::types::FrameError),
    /// 后端不支持该操作/格式（调用方可回退替代路径）。
    #[error("不支持: {0}")]
    Unsupported(String),
    /// **显示侧拒绝 DMA-BUF 帧**（modifier/平面组合不被扫描输出接受）。
    ///
    /// 单独成一个变体（而不是塞进 [`DisplayError::Failed`]），是为了让调用方能
    /// **精确匹配**并据此"降级到 CPU 捕获模式重开"，而不是靠字符串猜错误类型。
    /// 真正的拒绝（i915 不接受 tiled modifier、AddFB2 返回 EINVAL、PRIME 导入失败）
    /// 与测试钩子注入的拒绝都走这一个变体。
    #[error("DMA-BUF 被显示侧拒绝（{reason}）")]
    DmaBufRejected {
        /// 具体原因（含内核 errno 语义/注入标记）。
        reason: String,
    },
    /// **本进程当前不是 DRM master**（环境暂时不可用，不是帧/平面的问题）。
    ///
    /// 单独成变体，是为了让调用方把它当**可重试的环境错误**处理：不计入
    /// "提交失败"、不触发降级。典型场景是服务重启时旧进程尚未释放 master
    /// （`drmSetMaster: Permission denied`），几秒后内核重新授予即可自动恢复。
    /// 把它和 [`DisplayError::Failed`] 混为一谈，会让"重启竞态"被误判成
    /// "帧有问题"并降级成静态图（t94）。
    #[error("未持有 DRM master（环境暂时不可用，可重试）: {detail}")]
    MasterUnavailable {
        /// 具体原因（含内核 errno 语义/占用者诊断）。
        detail: String,
    },
    /// 合成器当前节流或遮挡表面（未返回 `wl_surface.frame` 回调）：调用方应跳过本帧且不计为错误。
    #[error("显示输出被合成器节流（{reason}）")]
    Throttled {
        /// 节流/遮挡原因。
        reason: String,
    },
    /// 其他显示操作失败（`operation` 为操作名，如 `present`）。
    #[error("显示操作 {operation} 失败: {message}")]
    Failed {
        /// 操作名。
        operation: &'static str,
        /// 详细信息。
        message: String,
    },
}

/// 一个全屏窗口的最小快照（只用于"要不要暂停壁纸"这一件事）。
///
/// 刻意不带窗口 id / pid / 几何：壁纸进程没有理由知道这些，
/// 少拿一点信息就少一份被误用的可能。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FullscreenToplevel {
    /// 应用 id（`app_id`，例如 `steam`、`mpv`、`gamescope`）。
    pub app_id: Option<String>,
    /// 窗口标题。
    pub title: Option<String>,
    /// 该窗口声称所在的输出名（可能为空：部分合成器不报 output）。
    pub outputs: Vec<String>,
}

impl FullscreenToplevel {
    /// 供日志/状态展示的一行描述（永远非空）。
    pub fn describe(&self) -> String {
        let app = self.app_id.as_deref().unwrap_or("(未命名)");
        match self.title.as_deref() {
            Some(title) if !title.is_empty() => format!("{app}: {title}"),
            _ => app.to_string(),
        }
    }
}

/// 显示后端抽象（规范 §3.4 的固定边界接口）。
///
/// # 实现约定
///
/// - [`Self::present`] 返回 [`PresentToken`] 仅代表提交已受理；
///   in-flight 缓冲的释放以 page-flip/commit 完成事件为准（M1 实现事件回收）。
/// - 单个输出的异常不得无限阻塞其他输出或 IPC 服务。
pub trait DisplayBackend {
    /// 枚举输出。
    fn outputs(&self) -> Vec<OutputInfo>;
    /// 选择目标输出：`wanted` 精确匹配名称，否则取第一个已连接且有模式的输出。
    ///
    /// # Errors
    ///
    /// 名称不存在或无可用输出时返回 [`DisplayError`]。
    fn default_output(&self, wanted: Option<&str>) -> Result<OutputId, DisplayError>;
    /// 解析配置中的模式选择（`preferred` 或 `宽x高[@Hz]`）为具体显示模式。
    ///
    /// # Errors
    ///
    /// 输出不存在或没有匹配模式时返回 [`DisplayError`]。
    fn resolve_mode(
        &self,
        output: &OutputId,
        selection: &ModeSelection,
    ) -> Result<DisplayMode, DisplayError>;
    /// 复查某个输出的连接状态（热插拔检测）。
    ///
    /// # Errors
    ///
    /// 查询失败时返回 [`DisplayError`]。
    fn connector_connected(&self, name: &str) -> Result<bool, DisplayError>;
    /// 提交一帧到输出。
    ///
    /// # Errors
    ///
    /// 输出不存在、后端挂起或帧非法时返回 [`DisplayError`]。
    fn present(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
    ) -> Result<PresentToken, DisplayError>;
    /// 提交一帧到输出，由硬件平面把 `src`（16.16：x,y,w,h）拉伸到
    /// `dst`（模式像素：x,y,w,h）——免 CPU 缩放（M5 性能路径）。
    ///
    /// # Errors
    ///
    /// 输出不存在、后端挂起或帧非法时返回 [`DisplayError`]。
    fn present_fitted(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
        src: (u64, u64, u64, u64),
        dst: (i64, i64, u64, u64),
    ) -> Result<PresentToken, DisplayError>;
    /// 设置输出模式（modeset）。
    ///
    /// # Errors
    ///
    /// 输出/模式不支持或后端挂起时返回 [`DisplayError`]。
    fn set_mode(&mut self, output: OutputId, mode: DisplayMode) -> Result<(), DisplayError>;
    /// 挂起（VT 切换/权限失效时安全释放设备访问）。
    ///
    /// # Errors
    ///
    /// 设备操作失败时返回 [`DisplayError`]。
    fn suspend(&mut self) -> Result<(), DisplayError>;
    /// 恢复并重新获取设备访问。
    ///
    /// # Errors
    ///
    /// 设备操作失败时返回 [`DisplayError`]。
    fn resume(&mut self) -> Result<(), DisplayError>;

    /// 提交计数（`None` = 该后端不提供，如纯占位后端）。
    ///
    /// **语义见 [`CommitStats`]：这不是"上屏"证据**（`commit_ok` 只表示内核
    /// 接受了阻塞原子提交；成功总数 = `commit_ok + retried_after_busy + legacy_fallback`）。
    ///
    /// 上层应把它并入 `status`，使"提交失败 → 帧未上屏"对外可见；
    /// 静默丢弃提交错误会让画面卡顿变成不可诊断的现象。
    /// 本后端**当前能否**扫描输出 `DisplayFrame::DmaBuf`（能力事实，不是配置推断）。
    ///
    /// 媒体侧的生产者必须据此选择捕获模式：后端不接受直通帧却仍产出 DMA-BUF，
    /// 每一帧都会走"被拒 → 重开解码管线"，或在错误未被类型化判别时把视频
    /// 误降级成静态图（真机回归：旧 unit 遗留 `LIUGUANG_VIDEO_DMABUF=1` 升级后
    /// 无法持续播放）。默认 `false` = 不接受直通，生产者应走 CPU 捕获。
    fn supports_dmabuf_scanout(&self) -> bool {
        false
    }

    /// 该后端能否导入指定 `(fourcc, modifier)` 的 DMA-BUF。
    ///
    /// 默认返回 `true`：DRM 直出把修饰符交给 KMS 自己的平面格式协商。
    /// Wayland 后端必须按合成器广播的格式判断——客户端挑了没广播的修饰符，
    /// 导入一定失败，而失败发生在"解码 + 导出"之后，整条链路白跑一次。
    fn accepts_dmabuf_format(&self, _fourcc: u32, _modifier: u64) -> bool {
        true
    }

    /// 返回后端为指定 DMA-BUF `fourcc` 广播的显式 modifier 列表（顺序按后端报告）。
    ///
    /// 用于转换器（如 NV12 → XRGB8888）直接按合成器接受的 modifier 分配输出 BO。
    /// 默认返回空（调用方自行选择默认分配策略）。
    fn dmabuf_modifiers_for(&self, _fourcc: u32) -> Vec<u64> {
        Vec::new()
    }

    /// 后端能否直接消费解码器原生的 **YUV（NV12）CPU 帧**。
    ///
    /// DRM/KMS 平面可以扫 YUV，扫描输出时由硬件做 YUV→RGB；`wl_shm` 只接受
    /// RGB 格式（`Xrgb8888`/`Argb8888`/`Rgba8888`），YUV 帧到了后端只能被拒
    /// （真机：硬解生效后帧是 `Cpu 2720x1530 Nv12`，wayland 后端连续 3 次提交
    /// 失败 ⇒ 视频被降级成 `DegradedStatic`）。
    ///
    /// 默认 `true`（DRM 语义）；wl_shm 类后端覆写为 `false`，媒体侧据此把捕获
    /// 模式钉成 RGB（硬解时走 `vapostproc` 在 GPU 侧转换）。
    fn accepts_yuv_cpu_frames(&self) -> bool {
        true
    }

    /// 当前是否有**全屏窗口**在用这块屏（有就返回它）。
    ///
    /// 用途：游戏/视频播放器独占全屏时，壁纸继续跑只会白白吃 CPU/GPU，
    /// 还可能和游戏的独占全屏抢扫描输出。默认 `None` = "不知道/不支持"，
    /// 调用方据此**不做任何暂停**（宁可多耗一点电，也不要误判把壁纸停了）。
    ///
    /// 只有能观测到窗口状态的合成器后端（实现了 `zwlr_foreign_toplevel_management`）
    /// 才返回 `Some`；GNOME/KDE 没有这个协议 ⇒ 静默降级。
    fn fullscreen_toplevel(&self) -> Option<FullscreenToplevel> {
        None
    }

    /// 显示侧拒绝 DMA-BUF 的累计次数（真实拒绝 + 注入拒绝）。
    ///
    /// `0` = 该后端不适用/从未拒绝。daemon 用它区分"直通一直在用"与
    /// "直通被拒后降级到 CPU 捕获"（t32 的 S2/S7）。
    fn dmabuf_rejections(&self) -> u64 {
        0
    }

    fn commit_stats(&self) -> Option<CommitStats> {
        None
    }

    /// **对账用**：相对 `baseline` 的提交计数增量。
    ///
    /// 推荐用法（在 `present*` 返回 `Err` 后**同步**读取本次提交的失败增量）：
    /// ```ignore
    /// let baseline = backend.commit_stats().unwrap_or_default();
    /// let outcome = backend.present_fitted(output, frame, src, dst);
    /// let delta = backend.commit_delta_since(&baseline).unwrap_or_default();
    /// if outcome.is_err() {
    ///     // delta.failed >= 1；与媒体侧 presented 增量一起写进 status 可互相验账
    ///     // （delta.contradicts_presented(presented_delta) == true 即异常签名）
    /// }
    /// ```
    fn commit_delta_since(&self, baseline: &CommitStats) -> Option<CommitDelta> {
        self.commit_stats().map(|now| now.delta_since(baseline))
    }

    /// 周期性尝试恢复 DRM master（**与"是否在提交"解耦**）。
    ///
    /// 背景（t26 根因）：自愈原先只由"提交失败"驱动 ⇒ daemon 处于降级静止状态
    /// （不提交）时**永不重试**（真机实测 15s 内 0 次尝试）。daemon 应在空闲 tick /
    /// status 查询里周期调用本方法，从而"无提交也能恢复"。
    ///
    /// 返回 `true` 表示本次真的重建了设备访问；默认实现返回 `false`（该后端不适用）。
    ///
    /// 注意（t26 教训）：返回值必须是"**真的拿到 master**"，不能是"重开过设备就算"，
    /// 否则调用方会误报自愈成功并做无意义的重放。
    fn poll_master_recovery(&mut self) -> bool {
        false
    }

    /// DRM master 状态（`None` = 该后端不适用/不提供，如模拟后端）。
    ///
    /// `master == Some(false)` 表示**已确定**本进程不是 master ⇒ 任何提交都不会生效、
    /// 画面不会更新（见 [`crate::master`]）。上层应把它并入 `status`。
    fn master_status(&self) -> Option<&MasterStatus> {
        None
    }

    /// 后端连接是否仍然存活（Wayland 断开后返回 `false`，触发上层重连）。
    fn is_alive(&self) -> bool {
        true
    }

    /// 若显示后端或合成器报告了主渲染设备节点（如 DMA-BUF v4 `main_device`），
    /// 返回对应的 `/dev/dri/renderD*`。
    fn preferred_render_node(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// 若显示后端可通过 socket peer 等方式识别当前合成器进程名，返回小写名。
    fn compositor_name(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockDisplayBackend;
    use super::*;
    use liuguang_core::types::{CpuFrame, PixelFormat};
    use std::sync::Arc;

    fn cpu_frame(w: u32, h: u32) -> DisplayFrame {
        let stride = w * 4;
        DisplayFrame::Cpu(CpuFrame {
            width: w,
            height: h,
            stride,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; (stride * h) as usize].into_boxed_slice()),
        })
    }

    // ---------- t18: 提交计数语义与"对账"API ----------

    #[test]
    fn commit_stats_semantics_are_not_presentation() {
        let stats = CommitStats {
            commit_ok: 10,
            retried_after_busy: 3,
            legacy_fallback: 1,
            failed: 2,
        };
        // 成功总数 = 直接成功 + 重试成功 + legacy 回退成功（`commit_ok` 不含重试）。
        assert_eq!(stats.commit_success_total(), 14);
        assert_eq!(stats.attempts(), 16);
        assert!(stats.has_failed_frames());
        assert!(!CommitStats::default().has_failed_frames());
    }

    #[test]
    fn commit_delta_since_reports_only_the_increment() {
        let before = CommitStats {
            commit_ok: 100,
            retried_after_busy: 5,
            legacy_fallback: 2,
            failed: 1,
        };
        let after = CommitStats {
            commit_ok: 101,
            retried_after_busy: 6,
            legacy_fallback: 2,
            failed: 4,
        };
        let delta = after.delta_since(&before);
        assert_eq!(delta.commit_ok, 1);
        assert_eq!(delta.retried_after_busy, 1);
        assert_eq!(delta.legacy_fallback, 0);
        assert_eq!(delta.failed, 3);
        assert_eq!(delta.commit_success_total(), 2);
        assert_eq!(delta.attempts(), 5);
        assert!(delta.has_failed_frames());
        // 未变化时增量全 0。
        assert_eq!(after.delta_since(&after), CommitDelta::default());
    }

    #[test]
    fn reconciliation_signature_flags_presented_with_failures() {
        // 本项目四次误判的共同点：只有一个信号、而它在故障下失真。
        // 对账判据：presented 在涨 + failed 也在涨 ⇒ 异常签名（帧被取出但没提交成功）。
        let anomaly = CommitDelta {
            commit_ok: 0,
            retried_after_busy: 0,
            legacy_fallback: 0,
            failed: 7,
        };
        assert!(
            anomaly.contradicts_presented(7),
            "presented 涨而 failed 也涨必须判为异常"
        );
        // 健康帧：presented 涨、failed 不涨 ⇒ 不算异常。
        let healthy = CommitDelta {
            commit_ok: 7,
            retried_after_busy: 0,
            legacy_fallback: 0,
            failed: 0,
        };
        assert!(!healthy.contradicts_presented(7));
        // 双方都不动 ⇒ 不算异常（例如暂停）。
        assert!(!CommitDelta::default().contradicts_presented(0));
        // 有失败但媒体侧没呈现任何帧 ⇒ 不构成"presented 说谎"。
        assert!(!anomaly.contradicts_presented(0));
    }

    #[test]
    fn mock_present_records_frames() {
        let mut backend = MockDisplayBackend::with_default_output();
        let token = backend
            .present(OutputId::new("HDMI-A-1"), cpu_frame(64, 64))
            .unwrap();
        assert_eq!(token.output, OutputId::new("HDMI-A-1"));
        assert_eq!(token.sequence, 1);
        assert_eq!(backend.presented().len(), 1);
    }

    #[test]
    fn mock_rejects_unknown_output_and_bad_frames() {
        let mut backend = MockDisplayBackend::with_default_output();
        let err = backend
            .present(OutputId::new("DP-9"), cpu_frame(64, 64))
            .unwrap_err();
        assert!(matches!(err, DisplayError::OutputNotFound(_)));

        let bad = DisplayFrame::Cpu(CpuFrame {
            width: 0,
            height: 64,
            stride: 0,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; 4].into_boxed_slice()),
        });
        let err = backend.present(OutputId::new("HDMI-A-1"), bad).unwrap_err();
        assert!(matches!(err, DisplayError::InvalidFrame(_)));
    }

    #[test]
    fn mock_set_mode_checks_support() {
        let mut backend = MockDisplayBackend::with_default_output();
        let mode = backend.outputs()[0].modes[0];
        backend.set_mode(OutputId::new("HDMI-A-1"), mode).unwrap();

        let err = backend
            .set_mode(
                OutputId::new("HDMI-A-1"),
                DisplayMode::from_hz(1234, 567, 60.0),
            )
            .unwrap_err();
        assert!(matches!(err, DisplayError::ModeNotSupported { .. }));
    }

    #[test]
    fn mock_suspend_resume_blocks_presents() {
        let mut backend = MockDisplayBackend::with_default_output();
        backend.suspend().unwrap();
        let err = backend
            .present(OutputId::new("HDMI-A-1"), cpu_frame(64, 64))
            .unwrap_err();
        assert!(matches!(err, DisplayError::Suspended));
        backend.resume().unwrap();
        backend
            .present(OutputId::new("HDMI-A-1"), cpu_frame(64, 64))
            .unwrap();
    }

    #[test]
    fn mock_hotplug_changes_outputs() {
        let mut backend = MockDisplayBackend::with_default_output();
        backend.unplug("HDMI-A-1");
        assert!(!backend.outputs()[0].connected);
        let err = backend
            .present(OutputId::new("HDMI-A-1"), cpu_frame(64, 64))
            .unwrap_err();
        assert!(matches!(err, DisplayError::DeviceUnavailable(_)));

        backend.plug("HDMI-A-1");
        assert!(backend.outputs()[0].connected);
    }
}
