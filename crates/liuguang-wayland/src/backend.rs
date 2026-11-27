//! `DisplayBackend` 的 Wayland/wlroots 实现。
//!
//! 架构（见 docs/ARCHITECTURE.md §Wayland）：
//!
//! - `wl_shm` CPU 帧三槽池（每输出一个，[`super::pool`]）；
//! - `zwlr_layer_shell_v1` Background 层、anchor 四边、exclusive_zone=-1；
//! - **单帧 in-flight 背压**：commit 前等待上一个 `wl_surface.frame`
//!   回调（100ms 超时放行，丢帧而非堆积）；
//! - 输出键 = `wl_output.name`（niri/Hyprland/sway 与 DRM connector 名一致，
//!   如 `eDP-1`），取不到回退 `output-<global>`；
//! - 事件泵由 `present_fitted` 与 `outputs()` 驱动（合成器回调 → RefCell
//!   内部可变性，所有协议交互走 `&self`）；
//! - 合成器断连 → `exited=true`：`outputs()` 返回空（daemon 转入
//!   HeadlessReady），`present` 返回 `Failed`。

use std::cell::RefCell;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use liuguang_core::config::ModeSelection;
use liuguang_core::types::{
    DisplayFrame, DisplayMode, DmaBufFrame, OutputId, OutputInfo, PixelFormat, PresentToken,
};
use liuguang_display::{CommitStats, DisplayBackend, DisplayError, FullscreenToplevel};
use wayland_client::{
    Connection, EventQueue, Proxy, QueueHandle,
    globals::{BindError, GlobalList, registry_queue_init},
    protocol::{wl_compositor, wl_output, wl_shm},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1;
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, Layer},
    zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity},
};

use super::pool::Pool;
use super::state::{
    DmabufParamsUdata, FractionalUdata, FrameUdata, LayerUdata, OutputEntry, OutputUdata,
    SurfaceUdata, WlRef, WlState,
};
use super::{blit, sys};
use blit::{DstRect, SrcRect};

/// frame 回调背压等待上限；超时放行（丢帧不堆积）。
const FRAME_WAIT: Duration = Duration::from_millis(100);
/// 事件泵的单片时长（t19）。
///
/// `wait_for` 的谓词只能在**泵之外**检查，所以单片时长直接决定"谓词成立 → 返回"
/// 的延迟：以前每片 10ms，于是 120fps（8.33ms/帧）的每一帧都要固定空等
/// ~10ms ⇒ 呈现被压到 ~82fps。现在每片 ≤1ms，片间立刻复检谓词。
///
/// **不要**把 `FRAME_WAIT`/`SLOT_WAIT`/`DMABUF_WAIT` 跟着改小：那些是超时兜底，
/// 改小会把"慢合成器"变成假失败。这里缩短的只是轮询粒度，不是等待上限。
const PUMP_SLICE: Duration = Duration::from_millis(1);
/// DMA-BUF 导入（`created`/`failed` 事件）的等待上限。
///
/// 合成器一般在同一个事件批次里回 `created`；给 250ms 是为了覆盖 Wayland
/// 往返 + 一次 EGL 导入检查。超时按「被拒」处理（daemon 会闩死直通并回 CPU）。
const DMABUF_WAIT: Duration = Duration::from_millis(250);
/// 空槽等待上限；超时视为合成器持锁异常。
const SLOT_WAIT: Duration = Duration::from_millis(250);
/// 初始化等待全部 configure 的截止。
const INIT_CONFIGURE_WAIT: Duration = Duration::from_secs(1);

/// Wayland/wlroots 合成器的显示后端。
///
/// `globals` 保留在字段里：`GlobalList` 持有 registry 代理，Drop 不发
/// destructor，但保留可让未来按需重新查询单例（v2 GUI 的合成器能力探测）。
#[derive(Debug)]
pub struct WaylandBackend {
    conn: Connection,
    #[allow(dead_code)] // 所有权句柄（见上）；初始绑定后不再读取
    globals: GlobalList,
    qh: QueueHandle<WlState>,
    queue: RefCell<EventQueue<WlState>>,
    wl: WlRef,
}

/// 选出与帧内存字节序一致的 `wl_shm` 格式（零转换上传），且必须由合成器广播过。
///
/// 字节序对照（little-endian 内存布局）：
/// - `PixelFormat::Xrgb8888` 内存 `[B,G,R,X]` ↔ `wl_shm::Format::Xrgb8888`（`[31:0] x:R:G:B`）
/// - `PixelFormat::Argb8888` 内存 `[B,G,R,A]` ↔ `wl_shm::Format::Argb8888`
/// - `PixelFormat::Rgba8888` 内存 `[R,G,B,A]` ↔ `wl_shm::Format::Abgr8888`（`[31:0] A:B:G:R`）
///
/// 拿不到匹配格式（合成器没广播 / NV12 等）就返回 `None`，调用方回退 CPU 重采样。
fn pick_upload_format(wl: &WlState, format: PixelFormat) -> Option<wl_shm::Format> {
    pick_upload_format_from(&wl.shm_formats, format)
}

/// [`pick_upload_format`] 的纯逻辑部分（便于单测，不需要活连接）。
fn pick_upload_format_from(
    advertised: &[wl_shm::Format],
    format: PixelFormat,
) -> Option<wl_shm::Format> {
    let candidate = match format {
        PixelFormat::Xrgb8888 => wl_shm::Format::Xrgb8888,
        PixelFormat::Argb8888 => wl_shm::Format::Argb8888,
        PixelFormat::Rgba8888 => wl_shm::Format::Abgr8888,
        PixelFormat::Nv12 => return None,
    };
    advertised.contains(&candidate).then_some(candidate)
}

impl WaylandBackend {
    /// 连接当前会话的合成器并完成初始握手。
    ///
    /// 步骤：connect → registry → bind 单例（compositor/shm/layer-shell）
    /// → 绑全部 `wl_output`（v≤4）→ 泵至输出事件齐 → 给每个输出建
    /// Background layer surface → 泵至全部 configure → ack + 建池。
    ///
    /// # Errors
    ///
    /// - 无 `WAYLAND_DISPLAY` / 合成器不可达 → [`DisplayError::DeviceUnavailable`]；
    /// - 合成器缺 `zwlr_layer_shell_v1` → [`DisplayError::DeviceUnavailable`]。
    pub fn connect() -> Result<Self, DisplayError> {
        let conn = connect_wayland_with_fallback()?;
        let (globals, queue) = registry_queue_init::<WlState>(&conn).map_err(|e| {
            DisplayError::DeviceUnavailable(format!("Wayland 初始 registry 握手失败: {e}"))
        })?;
        let qh = queue.handle();

        // 单例全局。wl_compositor XML v7 → 绑 1..=6；wl_shm v1；
        // zwlr_layer_shell_v1 XML v5 → 绑 1..=4。
        let bind_err = |what: &str, e: BindError| {
            DisplayError::DeviceUnavailable(format!("绑定 {what} 失败: {e}"))
        };
        let compositor: wl_compositor::WlCompositor = globals
            .bind(&qh, 1..=6, ())
            .map_err(|e| bind_err("wl_compositor", e))?;
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| bind_err("wl_shm", e))?;
        let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 =
            globals.bind(&qh, 1..=4, ()).map_err(|e| match e {
                BindError::NotPresent => DisplayError::DeviceUnavailable(
                    "合成器不支持 wlr-layer-shell（GNOME 或纯 X11 会话？）".into(),
                ),
                other => bind_err("zwlr_layer_shell_v1", other),
            })?;

        let mut wl = WlState {
            shm_formats: Vec::new(),
            compositor: Some(compositor),
            shm: Some(shm),
            shell: Some(shell),
            ..WlState::default()
        };

        // 分数缩放（可选）：wp_viewporter + wp_fractional_scale_manager_v1。
        // 缺任一个就回退到整数 `wl_output.scale` 路径（会略模糊或留边）。
        wl.viewporter = match globals.bind::<_, WlState, _>(&qh, 1..=1, ()) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("合成器无 wp_viewporter（走整数 scale 回退）: {e}");
                None
            }
        };
        wl.fractional_manager = match globals.bind::<_, WlState, _>(&qh, 1..=1, ()) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!(
                    "合成器无 wp_fractional_scale_manager_v1（走整数 scale 回退）: {e}"
                );
                None
            }
        };
        if wl.viewporter.is_some() && wl.fractional_manager.is_some() {
            tracing::debug!("分数缩放协议可用（wp_fractional_scale_v1 + wp_viewport）");
        }

        // 零拷贝（可选）：zwp_linux_dmabuf_v1。只绑到 v3 —— v4+ 把格式广播换成
        // 需要主动 `get_default_feedback` 的 feedback 对象，本后端没有实现那套
        // tranche 解析；绑 v3 时合成器必须按旧语义发 `format` + `modifier` 事件。
        let dmabuf_global: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1> =
            match globals.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, WlState, _>(&qh, 1..=3, ())
            {
                Ok(v) => {
                    tracing::debug!("zwp_linux_dmabuf_v1 可用（版本 {}）", v.version());
                    Some(v)
                }
                Err(e) => {
                    tracing::debug!(
                        "合成器无 zwp_linux_dmabuf_v1（零拷贝不可用，走 CPU 上传）: {e}"
                    );
                    None
                }
            };
        let dmabuf_present = dmabuf_global.is_some();
        wl.dmabuf = dmabuf_global;
        if dmabuf_present {
            // 一屏日志换一次「到底支持什么」的确定性：格式/modifier 组合是本后端
            // 唯一会**静默拒绝**零拷贝的判据，运维排障必须能直接看到。
            for f in &wl.dmabuf_formats {
                tracing::debug!(
                    fourcc = format_args!("0x{:08X}", f.fourcc),
                    modifiers = ?f.modifiers.iter().map(|m| format!("0x{m:016X}")).collect::<Vec<_>>(),
                    "合成器广播的 DMA-BUF 格式"
                );
            }
        }

        // 顶层窗口列表（可选）：zwlr_foreign_toplevel_management_unstable_v1。
        // 用来判断"有没有游戏/播放器全屏"，从而暂停壁纸。niri/sway/wlroots 系
        // 都有；GNOME/KDE 没有 ⇒ 只打 debug，功能静默降级（不阻断启动）。
        wl.toplevel_manager = match globals
            .bind::<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, WlState, _>(
            &qh,
            1..=3,
            (),
        ) {
            Ok(manager) => {
                tracing::debug!("合成器支持 zwlr_foreign_toplevel_management（可检测全屏窗口）");
                Some(manager)
            }
            Err(e) => {
                tracing::debug!("合成器无 zwlr_foreign_toplevel_management（无法检测全屏）: {e}");
                None
            }
        };

        // 多实例 wl_output：显式遍历初始 globals 绑定（后续热插走
        // Dispatch<WlRegistry> 转发，见 state.rs）。
        let registry = globals.registry().clone();
        for g in globals.contents().clone_list() {
            if g.interface != "wl_output" {
                continue;
            }
            let output: wl_output::WlOutput =
                registry.bind(g.name, g.version.min(4), &qh, OutputUdata(g.name));
            wl.outputs.insert(
                g.name,
                OutputEntry {
                    global: g.name,
                    output,
                    name: None,
                    description: None,
                    physical_mm: None,
                    scale: 1,
                    modes: Vec::new(),
                    current_mode: None,
                    preferred_mode: None,
                    surface: None,
                    layer: None,
                    viewport: None,
                    fractional: None,
                    fractional_factor: None,
                    cfg: None,
                    pending_configure: None,
                    pool: None,
                    frame_pool: None,
                    frame_pending: false,
                    sequence: 0,
                    dead: false,
                },
            );
        }

        let backend = Self {
            conn,
            globals,
            qh,
            queue: RefCell::new(queue),
            wl: RefCell::new(wl),
        };

        // 泵到安静：收集 wl_output 的 Geometry/Mode/Scale/Name 事件。
        backend.pump_until(Instant::now() + INIT_CONFIGURE_WAIT);

        if backend.wl.borrow().exited {
            return Err(DisplayError::DeviceUnavailable(
                "Wayland 连接在初始握手期间断开".into(),
            ));
        }
        if backend.wl.borrow().outputs.is_empty() {
            return Err(DisplayError::DeviceUnavailable(
                "合成器没有报告任何 wl_output（无可用显示器输出）".into(),
            ));
        }

        // 给每个活输出建 Background layer surface。
        backend.ensure_layer_surfaces();
        // 泵至全部 configure 到达并 ack + 建池。
        backend.pump_until(Instant::now() + INIT_CONFIGURE_WAIT);
        backend.apply_configures();
        Ok(backend)
    }

    // -----------------------------------------------------------------------
    // 事件泵
    // -----------------------------------------------------------------------

    /// 泵事件直到安静（无已到达事件且 fd 暂不可读）或 deadline。
    ///
    /// 单线程安全：`queue` 与 `wl` 是不同 RefCell，允许同时借用；
    /// handler 只碰 `&mut WlState`，绝不回调 backend（无重入）。
    fn pump_until(&self, deadline: Instant) {
        self.pump_until_with(deadline, |_| false);
    }

    /// 泵事件，直到 `stop` 成立、安静、或 deadline（t19）。
    ///
    /// `stop` 在**每批 dispatch 之后**立即检查：frame callback 一到就停泵，
    /// 不再像旧实现那样把剩余片时长空等完（那是 120fps 只到 ~82fps 的根因）。
    /// 注意 `stop` 只看 `WlState`，不会回调 backend（无重入）。
    fn pump_until_with(&self, deadline: Instant, stop: impl Fn(&WlState) -> bool) {
        if self.wl.borrow().exited {
            return;
        }
        if self.conn.flush().is_err() {
            self.wl.borrow_mut().exited = true;
            return;
        }
        loop {
            let dispatched = {
                let mut queue = self.queue.borrow_mut();
                let mut wl = self.wl.borrow_mut();
                match queue.dispatch_pending(&mut *wl) {
                    Ok(n) => n,
                    Err(e) => {
                        wl.exited = true;
                        tracing::debug!("wayland 事件泵 dispatch 失败（视为合成器断连）: {e}");
                        return;
                    }
                }
            };
            // 热插的新输出先补 layer surface，再处理 configure。
            self.ensure_layer_surfaces();
            self.apply_configures();
            // 本批事件可能已经让谓词成立（例如 frame callback 到达）⇒ 立刻停泵。
            if stop(&self.wl.borrow()) {
                return;
            }
            if dispatched > 0 {
                // 事件队列还有积压，立刻处理下一批。
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            // 无待处理事件：prepare_read → poll fd → read → 回到循环顶部。
            if let Some(guard) = self.conn.prepare_read() {
                let remaining = deadline.saturating_duration_since(now);
                let timeout_ms = remaining.as_millis().min(1000) as i32;
                if sys::poll_readable(guard.connection_fd().as_raw_fd(), timeout_ms) {
                    if let Err(err) = guard.read() {
                        let would_block = matches!(
                            &err,
                            wayland_client::backend::WaylandError::Io(io_err)
                                if io_err.kind() == std::io::ErrorKind::WouldBlock
                        );
                        if !would_block {
                            self.wl.borrow_mut().exited = true;
                            tracing::debug!("wayland 读取失败（视为合成器断连）: {err}");
                            return;
                        }
                    }
                }
                // guard Drop 自动撤销 read 声明。
            } else {
                // 罕见：另有读者持有 read 声明（本后端单线程，理论不可达）。
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// 处理 pending_configure：ack + 更新 cfg + 尺寸变化时失效旧池（新池在 `present_cpu_fitted` 按需懒建）。
    ///
    /// 在每次 pump 批次后调用；也是 `present_fitted` 前的保险步骤。
    fn apply_configures(&self) {
        let mut wl = self.wl.borrow_mut();
        for entry in wl.outputs.values_mut() {
            if entry.dead {
                continue;
            }
            if let Some((serial, cfg_w, cfg_h)) = entry.pending_configure.take() {
                if let Some(layer) = entry.layer.as_ref() {
                    if layer.is_alive() {
                        layer.ack_configure(serial);
                    }
                }
                entry.cfg = Some((cfg_w, cfg_h));
                // viewport 的 destination 是**逻辑**尺寸（surface 坐标）；
                // buffer 尺寸可以不同（fractional 时 = 逻辑 × 1.5）。
                if let Some(viewport) = entry.viewport.as_ref() {
                    if entry.fractional_factor.is_some() && viewport.is_alive() {
                        viewport.set_destination(
                            i32::try_from(cfg_w).unwrap_or(i32::MAX),
                            i32::try_from(cfg_h).unwrap_or(i32::MAX),
                        );
                    }
                }
            }
            // 尺寸或分数缩放变化时只标记旧池失效，真正的建池延后到下一帧 CPU 提交：
            // 纯 DMA-BUF 视频期间完全不需要常驻 16–48 MiB 的 memfd 共享内存池。
            if let Some(pool) = entry.pool.as_ref() {
                if Some(pool.dims()) != entry.buffer_dims() {
                    entry.pool = None;
                }
            }
        }
    }

    /// 给所有无 surface 的活输出建 layer surface（初始 + 热插共用）。
    fn ensure_layer_surfaces(&self) {
        let (compositor, shell, viewporter, fractional_manager) = {
            let wl = self.wl.borrow();
            (
                wl.compositor.clone(),
                wl.shell.clone(),
                wl.viewporter.clone(),
                wl.fractional_manager.clone(),
            )
        };
        let (Some(compositor), Some(shell)) = (compositor, shell) else {
            return;
        };
        let mut created = false;
        {
            let mut wl = self.wl.borrow_mut();
            for (global, entry) in wl.outputs.iter_mut() {
                if entry.dead || entry.surface.is_some() {
                    continue;
                }
                let surface = compositor.create_surface(&self.qh, SurfaceUdata(*global));
                // 壁纸层**绝不能吃掉指针事件**。wl_surface 的 input region 默认是
                // 整个 surface：一张全屏 layer surface 若不动它，指针落在壁纸上的
                // 点击会被这层接走。wlroots 会按层排序把事件交给上层，但 Smithay /
                // denial 的 layer-shell 实现不保证同样的顺序（真机表现：denial 顶栏的
                // 工作区指示与电池按钮点不动）。显式设成空区域 ⇒ 全部穿透。
                let empty_region = compositor.create_region(&self.qh, ());
                surface.set_input_region(Some(&empty_region));
                empty_region.destroy();
                let output = entry.output.clone();
                let layer = shell.get_layer_surface(
                    &surface,
                    Some(&output),
                    Layer::Background,
                    "liuguang".into(),
                    &self.qh,
                    LayerUdata(*global),
                );
                // scanner 对 bitfield 枚举生成 camelCase 关联常量。
                layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                layer.set_exclusive_zone(-1);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                // **必须**：layer surface 的首次 commit 不能带 buffer —— 这是
                // 触发合成器发 configure 的唯一信号。少了这句就永远收不到
                // configure，池建不出来，present 会卡在"等待 configure"。
                surface.commit();
                // 分数缩放与 viewport 互相独立：只要有 viewporter 就建 viewport，
                // 避免只有 wp_viewporter、没有 fractional_scale 时 DMA-BUF 与快路径退化。
                entry.viewport = viewporter
                    .as_ref()
                    .map(|vp| vp.get_viewport(&surface, &self.qh, ()));
                entry.fractional = fractional_manager.as_ref().map(|fs| {
                    fs.get_fractional_scale(&surface, &self.qh, FractionalUdata(*global))
                });
                entry.surface = Some(surface);
                entry.layer = Some(layer);
                created = true;
            }
        }
        if created {
            let _ = self.conn.flush();
        }
    }

    /// 按输出键解析 registry 全局名（活的）。
    fn resolve_global(&self, output: &OutputId) -> Result<u32, DisplayError> {
        let key = output.as_str();
        self.wl
            .borrow()
            .find_global(key)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))
    }

    /// 等待谓词成立（pump 驱动），返回是否在 deadline 前成立。
    ///
    /// 两层保证"谓词成立即返回"：
    /// 1. `pump_until_with` 在**每次 dispatch 批次之后**检查谓词，成立立刻停泵
    ///    （延迟 ≈ 一次事件分发，微秒级）；
    /// 2. 外层 `wait_until` 的切片（≤`PUMP_SLICE`）只作兜底——即使谓词靠的是
    ///    别的东西（比如另一条路径改了状态），也不会超过一片才发现。
    fn wait_for(&self, deadline: Instant, pred: impl Fn(&WlState) -> bool) -> bool {
        wait_until(
            deadline,
            PUMP_SLICE,
            || pred(&self.wl.borrow()),
            |slice_deadline| self.pump_until_with(slice_deadline, |wl| pred(wl)),
        )
    }

    /// 泵 + 检查断连，统一报错文本。
    fn ensure_alive(&self) -> Result<(), DisplayError> {
        self.pump_until(Instant::now());
        if self.wl.borrow().exited {
            return Err(DisplayError::Failed {
                operation: "present",
                message: "Wayland 合成器连接已断开".into(),
            });
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // present 主路径
    // -----------------------------------------------------------------------

    fn present_cpu_fitted(
        &self,
        output: OutputId,
        frame: liuguang_core::types::CpuFrame,
        src: SrcRect,
        dst: DstRect,
    ) -> Result<PresentToken, DisplayError> {
        // 先收一轮已到达回调（release/frame），再解析输出。
        self.ensure_alive()?;
        let global = self.resolve_global(&output)?;

        // 快路径候选格式：帧内存字节序与 `wl_shm` 格式码一致、且合成器广播了
        // 这个格式 ⇒ 本帧可以零转换上传，缩放交给合成器 GPU。
        // 真正是否走快路径还要等 configure（要求 dst 铺满整个 surface）。
        let (upload_format, shm, qh) = {
            let wl = self.wl.borrow();
            (
                pick_upload_format(&wl, frame.format),
                wl.shm.clone(),
                self.qh.clone(),
            )
        };

        // 单帧 in-flight 背压：等上一个 frame 回调（超时放行，丢帧不堆积）。
        let pending = |wl: &WlState| wl.outputs.get(&global).is_some_and(|e| e.frame_pending);
        if !self.wait_for(Instant::now() + FRAME_WAIT, |wl| !pending(wl)) {
            tracing::trace!("wayland frame 背压超时（100ms），放行提交");
        }

        // 等待 layer surface 完成首次 configure。
        let cfg_ready = |wl: &WlState| wl.outputs.get(&global).is_some_and(|e| e.cfg.is_some());
        if !self.wait_for(Instant::now() + SLOT_WAIT, cfg_ready) {
            return Err(DisplayError::Failed {
                operation: "present",
                message: "layer surface 未收到 configure（合成器未确认壁纸层尺寸）".to_string(),
            });
        }

        // 若目标 wl_shm 池已经存在且全部槽位都被合成器占着，等它归还一个空槽；
        // 若池尚未按需创建（或需要重建），无需等待，下面直接建池并取第 0 槽。
        let slot_free = |wl: &WlState| {
            let Some(e) = wl.outputs.get(&global) else {
                return false;
            };
            if e.cfg.is_none() {
                return false;
            };
            let fills_surface = entry_fills_surface(e, dst);
            let viewport_ok = e.viewport.as_ref().is_some_and(Proxy::is_alive);
            if let Some(format) = upload_format.filter(|_| fills_surface && viewport_ok) {
                if let Some(fp) = e
                    .frame_pool
                    .as_ref()
                    .filter(|p| p.dims() == (frame.width, frame.height) && p.format() == format)
                {
                    return fp.free_slot().is_some();
                }
                return e.pool.as_ref().is_none_or(|p| p.free_slot().is_some());
            }
            match (e.pool.as_ref(), e.buffer_dims()) {
                (Some(pool), Some(want)) if pool.dims() == want => pool.free_slot().is_some(),
                _ => true,
            }
        };
        if !self.wait_for(Instant::now() + SLOT_WAIT, slot_free) {
            return Err(DisplayError::Failed {
                operation: "present",
                message: "wl_shm 缓冲未释放（合成器 250ms 内未归还）".to_string(),
            });
        }

        let sequence = {
            let mut wl = self.wl.borrow_mut();
            let entry = wl
                .entry_mut(global)
                .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
            let (cfg_w, cfg_h) = entry.cfg.ok_or_else(|| DisplayError::Failed {
                operation: "present",
                message: "layer surface 尚未完成首次 configure".into(),
            })?;

            // dst 是否铺满整个 surface（daemon 的 fit 已把 cover/zoom 算成整屏）。
            let fills_surface = entry_fills_surface(entry, dst);
            let viewport_ok = entry.viewport.as_ref().is_some_and(Proxy::is_alive);
            let mut fast = upload_format.filter(|_| fills_surface && viewport_ok);

            // 快路径：帧池按帧尺寸/格式懒建（configure 变了也不需要重建）。
            if let Some(format) = fast {
                let dims = (frame.width, frame.height);
                let stale = entry
                    .frame_pool
                    .as_ref()
                    .is_none_or(|p| p.dims() != dims || p.format() != format);
                if stale {
                    entry.frame_pool = None;
                    if let Some(shm) = shm.as_ref() {
                        match Pool::new(shm, &qh, global, dims.0, dims.1, format, true) {
                            Ok(pool) => entry.frame_pool = Some(pool),
                            Err(e) => {
                                tracing::warn!(
                                    output = %output,
                                    error = %e,
                                    "帧尺寸 wl_shm 池创建失败，回退 CPU 合成"
                                );
                                fast = None;
                            }
                        }
                    } else {
                        fast = None;
                    }
                }
            }

            // 写槽：快路径整帧原样上传，回退路径 CPU 重采样。
            let (buffer, pw, ph) = if fast.is_some() {
                let pool = entry
                    .frame_pool
                    .as_mut()
                    .ok_or_else(|| DisplayError::Failed {
                        operation: "present",
                        message: "帧池不可用（重建失败？）".into(),
                    })?;
                let (pw, ph) = pool.dims();
                let stride = pw.saturating_mul(4);
                let slot = pool.free_slot().ok_or_else(|| DisplayError::Failed {
                    operation: "present",
                    message: "帧池空槽竞争丢失（内部不变量破坏）".into(),
                })?;
                blit::upload_frame(pool.slot_data(slot), stride, &frame)?;
                let buffer = pool
                    .buffer(slot)
                    .cloned()
                    .ok_or_else(|| DisplayError::Failed {
                        operation: "present",
                        message: "wl_buffer 代理不可用".into(),
                    })?;
                pool.mark_busy(slot);
                (buffer, pw, ph)
            } else {
                let want = entry.buffer_dims().ok_or_else(|| DisplayError::Failed {
                    operation: "present",
                    message: "layer surface 尚未完成首次 configure".into(),
                })?;
                if entry.pool.as_ref().map(Pool::dims) != Some(want) {
                    entry.pool = None;
                    let shm_proxy = shm.as_ref().ok_or_else(|| DisplayError::Failed {
                        operation: "present",
                        message: "合成器未提供 wl_shm 全局对象".into(),
                    })?;
                    let created = Pool::new(
                        shm_proxy,
                        &qh,
                        global,
                        want.0,
                        want.1,
                        wl_shm::Format::Xrgb8888,
                        false,
                    )
                    .map_err(|e| DisplayError::Failed {
                        operation: "present",
                        message: format!("wl_shm 池创建失败: {e}"),
                    })?;
                    entry.pool = Some(created);
                }
                let pool = entry.pool.as_mut().ok_or_else(|| DisplayError::Failed {
                    operation: "present",
                    message: "wl_shm 池不可用（重建失败？）".into(),
                })?;
                let (pw, ph) = pool.dims();
                let stride = pw.saturating_mul(4);
                let slot = pool.free_slot().ok_or_else(|| DisplayError::Failed {
                    operation: "present",
                    message: "wl_shm 空槽竞争丢失（内部不变量破坏）".into(),
                })?;
                blit::write_frame(pool.slot_data(slot), stride, pw, ph, &frame, src, dst)?;
                let buffer = pool
                    .buffer(slot)
                    .cloned()
                    .ok_or_else(|| DisplayError::Failed {
                        operation: "present",
                        message: "wl_buffer 代理不可用".into(),
                    })?;
                pool.mark_busy(slot);
                (buffer, pw, ph)
            };

            let surface = entry.surface.clone().ok_or_else(|| DisplayError::Failed {
                operation: "present",
                message: "wl_surface 代理不可用".into(),
            })?;
            // 快路径：buffer 就是帧本身（未缩放），用 viewport 表达
            // 「帧内矩形 → 整屏」的映射；buffer_scale 必须是 1，否则
            // source/destination 的单位会被再除一次。
            let (sx, sy, sw, sh) = clamp_viewport_source(src, pw, ph);
            if let Some(viewport) = entry.viewport.as_ref().filter(|v| v.is_alive()) {
                if fast.is_some() {
                    viewport.set_source(sx, sy, sw, sh);
                    viewport.set_destination(
                        i32::try_from(cfg_w).unwrap_or(i32::MAX),
                        i32::try_from(cfg_h).unwrap_or(i32::MAX),
                    );
                } else {
                    // 切回慢路径时必须清空上一帧残留的 source 裁切（否则 out_of_buffer 断连）。
                    viewport.set_source(-1.0, -1.0, -1.0, -1.0);
                    if entry.has_fractional_scale() {
                        viewport.set_destination(
                            i32::try_from(cfg_w).unwrap_or(i32::MAX),
                            i32::try_from(cfg_h).unwrap_or(i32::MAX),
                        );
                    } else {
                        viewport.set_destination(-1, -1);
                    }
                }
            }
            let scale = if fast.is_some() {
                1
            } else {
                entry.buffer_scale()
            };
            surface.set_buffer_scale(scale);
            surface.attach(Some(&buffer), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, pw as i32, ph as i32);
            } else {
                surface.damage(0, 0, cfg_w as i32, cfg_h as i32);
            }
            let _callback = surface.frame(&qh, FrameUdata(global));
            surface.commit();
            entry.frame_pending = true;
            entry.sequence = entry.sequence.wrapping_add(1);
            entry.sequence
        };
        let _ = self.conn.flush();

        Ok(PresentToken { output, sequence })
    }
}

// ---------------------------------------------------------------------------
// DMA-BUF 零拷贝路径
// ---------------------------------------------------------------------------

/// 帧里真实存在的平面数（DmaBufFrame 只填有效平面，其余留 0）。
fn plane_count(frame: &DmaBufFrame) -> u32 {
    frame
        .strides
        .iter()
        .filter(|stride| **stride > 0)
        .count()
        .clamp(1, 4) as u32
}

impl WaylandBackend {
    /// 零拷贝提交：把解码器的 DMA-BUF 直接导入成 `wl_buffer` 交给合成器。
    ///
    /// 与 CPU 路径的区别只有"缓冲从哪来"：不做任何像素拷贝，缩放仍交给合成器
    /// （`wp_viewport.set_source/set_destination`），因此 30fps 1080p 的 CPU
    /// 从「两趟 16MB 拷贝」降到近似 0。
    ///
    /// 失败语义：任何"这块缓冲合成器吃不下"的情况都返回
    /// [`DisplayError::DmaBufRejected`]（而不是 `Failed`），daemon 据此闩死直通
    /// 并重开 CPU 管线 —— 这条路径必须能被区分出来，否则视频会被误降级成静态图。
    fn present_dmabuf_fitted(
        &self,
        output: OutputId,
        frame: DmaBufFrame,
        src: SrcRect,
        dst: DstRect,
    ) -> Result<PresentToken, DisplayError> {
        self.ensure_alive()?;
        let global = self.resolve_global(&output)?;

        if frame.width == 0 || frame.height == 0 {
            return Err(DisplayError::InvalidFrame(
                liuguang_core::types::FrameError::ZeroDimension,
            ));
        }
        // 前置校验：格式 + modifier 必须在合成器广播的表里。
        // 绑到 v3 时未广播的组合会产生 `failed` 事件（v4+ 直接是协议错误），
        // 提前判断能省一次往返，并给出人能读懂的原因。
        {
            let wl = self.wl.borrow();
            if wl.dmabuf.is_none() {
                return Err(DisplayError::DmaBufRejected {
                    reason: "合成器不支持 zwp_linux_dmabuf_v1".into(),
                });
            }
            if !wl.dmabuf_supports(frame.fourcc, frame.modifier) {
                let advertised = wl
                    .dmabuf_formats
                    .iter()
                    .find(|f| f.fourcc == frame.fourcc)
                    .map(|f| {
                        if f.modifiers.is_empty() {
                            "仅隐式 modifier".to_string()
                        } else {
                            f.modifiers
                                .iter()
                                .map(|m| format!("0x{m:016X}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    })
                    .unwrap_or_else(|| "该 fourcc 未广播".to_string());
                return Err(DisplayError::DmaBufRejected {
                    reason: format!(
                        "合成器不接受 fourcc=0x{:08X} modifier=0x{:016X}（该格式已广播: {advertised}；共 {} 个格式）",
                        frame.fourcc,
                        frame.modifier,
                        wl.dmabuf_formats.len()
                    ),
                });
            }
        }

        // 单帧 in-flight 背压（与 CPU 路径同语义：等上一个 frame 回调）。
        let pending = |wl: &WlState| wl.outputs.get(&global).is_some_and(|e| e.frame_pending);
        if !self.wait_for(Instant::now() + FRAME_WAIT, |wl| !pending(wl)) {
            tracing::trace!("wayland frame 背压超时（100ms），放行 DMA-BUF 提交");
        }

        // 等待 layer surface 完成首次 configure（热插拔刚建立的输出需要等一轮事件）。
        let cfg_ready = |wl: &WlState| wl.outputs.get(&global).is_some_and(|e| e.cfg.is_some());
        if !self.wait_for(Instant::now() + SLOT_WAIT, cfg_ready) {
            return Err(DisplayError::Failed {
                operation: "present_dmabuf",
                message: "layer surface 未收到 configure（合成器未确认壁纸层尺寸）".into(),
            });
        }
        let (cfg, fills, needs_viewport, viewport_ok) = {
            let wl = self.wl.borrow();
            let entry = wl
                .outputs
                .get(&global)
                .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
            let cfg = entry.cfg.ok_or_else(|| DisplayError::Failed {
                operation: "present_dmabuf",
                message: "layer surface 尚未完成首次 configure".into(),
            })?;
            let fills = entry_fills_surface(entry, dst);
            let src_is_full = src
                == (
                    0,
                    0,
                    u64::from(frame.width) << 16,
                    u64::from(frame.height) << 16,
                );
            let scale_1 = u32::try_from(entry.scale.max(1)).unwrap_or(1);
            let buffer_matches_cfg = frame.width == cfg.0.saturating_mul(scale_1)
                && frame.height == cfg.1.saturating_mul(scale_1)
                && !entry.has_fractional_scale();
            let needs_viewport = !src_is_full || !buffer_matches_cfg;
            let viewport_ok = entry.viewport.as_ref().is_some_and(Proxy::is_alive);
            (cfg, fills, needs_viewport, viewport_ok)
        };

        // `wp_viewport.set_destination` 只能表达「整 surface 铺满」，没有偏移量；
        // Fit=contain 这类要留黑边的布局只能走 CPU 合成（daemon 收到拒绝后闩死
        // 直通并重开 CPU 管线）。判据与 CPU 路径的 `fills_surface` 一致。
        if !fills {
            return Err(DisplayError::DmaBufRejected {
                reason: format!(
                    "目标矩形不铺满屏幕（dst=({},{}) {}x{}，逻辑 {}x{}），需要 CPU 合成",
                    dst.0, dst.1, dst.2, dst.3, cfg.0, cfg.1
                ),
            });
        }
        if needs_viewport && !viewport_ok {
            return Err(DisplayError::DmaBufRejected {
                reason: "合成器不支持 wp_viewporter，DMA-BUF 无法缩放或裁切".into(),
            });
        }

        // 导入：params → add(每平面) → create / create_immed。
        //
        // 两条路：
        // * **预热过的格式**（同一组 `fourcc + modifier` 已经用异步路径导入成功过）走
        //   `create_immed`（协议 v2 起）：同步返回 `wl_buffer`，省掉一次「发 create →
        //   泵事件直到 created」的往返。真机实测这次往返约 20ms，正是 60fps 源只能
        //   提交 30~36fps 的原因（CPU 只有 12%，不是算力不够）。
        // * 其余情况仍走异步 `create` + 等 `created`：`create_immed` 的失败不再是
        //   `failed` 事件，而是**协议错误**（直接断连接），所以第一次必须用能拿到
        //   明确结果的路径验过，才敢走同步路径。
        let (dmabuf, serial, warm) = {
            let mut wl = self.wl.borrow_mut();
            let Some(dmabuf) = wl.dmabuf.clone() else {
                return Err(DisplayError::DmaBufRejected {
                    reason: "合成器不支持 zwp_linux_dmabuf_v1".into(),
                });
            };
            let serial = wl.dmabuf_serial.wrapping_add(1);
            wl.dmabuf_serial = serial;
            wl.dmabuf_result = None;
            // 逃生开关：真机上如果 `create_immed` 表现异常（协议错误会直接断连接），
            // 设 `LIUGUANG_DMABUF_IMMED=0` 就能退回"create + 等 created"的老路。
            let immed_enabled = !matches!(
                std::env::var("LIUGUANG_DMABUF_IMMED").as_deref(),
                Ok("0" | "off" | "false" | "no")
            );
            let warm = immed_enabled && wl.dmabuf_warm.contains(&(frame.fourcc, frame.modifier));
            (dmabuf, serial, warm)
        };
        let qh_handle = self.qh.clone();
        // 寿命延长器：DMA-BUF 的 fd 只是句柄，内存仍归供应商所有。把供应商的
        // 保活对象寄存到 `WlState`，等合成器发 `wl_buffer.release`（即用完这一帧）
        // 再随 buffer 一起释放。
        //
        // **两条路的存放位置不同，而且不能都放**：
        // * 异步 `create` 只有导入序号 ⇒ 放 `dmabuf_keepalives[serial]`；
        // * `create_immed` 同步拿到 `wl_buffer` ⇒ 放 `dmabuf_buffer_keepalive[buffer.id()]`。
        // 早期实现先把每帧都塞进 `dmabuf_keepalives[serial]`，immed 分支再塞一份到
        // buffer 表；release 只清得掉 buffer 表那份，序号表那份成为永久泄漏 ⇒
        // 表面永不归还、`inflight` 涨到上限后整条供给停摆（真机表现：跑 7 帧就停）。
        let params = dmabuf.create_params(&qh_handle, DmabufParamsUdata(serial));
        let planes = plane_count(&frame);
        let modifier_hi = (frame.modifier >> 32) as u32;
        let modifier_lo = (frame.modifier & 0xffff_ffff) as u32;
        for plane in 0..planes {
            let idx = plane as usize;
            // 多 object 帧（NVIDIA SEPARATE_LAYERS）：每个平面用**自己** object 的 fd；
            // 单 object 帧（i915/iHD）所有平面共用 fd 0，行为与旧实现完全一致。
            let Some(fd) = frame.fd_for_plane(idx) else {
                params.destroy();
                return Err(DisplayError::DmaBufRejected {
                    reason: format!(
                        "平面 {idx} 指向的 object 不在该帧的 fds 里（objects={}, object_index={:?}）",
                        frame.object_count(),
                        frame.object_index
                    ),
                });
            };
            params.add(
                fd.as_fd(),
                plane,
                frame.offsets[idx],
                frame.strides[idx],
                modifier_hi,
                modifier_lo,
            );
        }
        let width = i32::try_from(frame.width).unwrap_or(i32::MAX);
        let height = i32::try_from(frame.height).unwrap_or(i32::MAX);
        let use_immed = warm && params.version() >= 2;
        // 异步路径的延长器必须在 `create` 之前寄存：`created` 可能在等待期间就到。
        if !use_immed {
            if let Some(keepalive) = frame.keepalive.clone() {
                self.wl
                    .borrow_mut()
                    .dmabuf_keepalives
                    .insert(serial, keepalive);
            }
        }
        // 同步路径：预热过的格式。
        let buffer = if use_immed {
            let buffer = params.create_immed(
                width,
                height,
                frame.fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &qh_handle,
                (),
            );
            params.destroy();
            let _ = self.conn.flush();
            // 延长器改挂到 buffer 对象上（这一路没有导入序号）。
            if let Some(keepalive) = frame.keepalive.clone() {
                self.wl
                    .borrow_mut()
                    .dmabuf_buffer_keepalive
                    .insert(buffer.id(), keepalive);
            }
            buffer
        } else {
            // 导入是异步的：`created` 表示合成器**已经**成功导入（自己持有 GEM 句柄），
            // 因此本函数返回、`frame` 丢掉 fd 之后缓冲依然有效；`failed` 则不成立。
            params.create(
                width,
                height,
                frame.fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
            );
            let _ = self.conn.flush();

            // **params 必须活到 created/failed 到达之后**：`create` 是异步的，提前
            // destroy 会让合成器回的事件落到已销毁对象上，客户端永远等不到结果
            // （真机表现：250ms 超时、`created` 从未出现）。
            let imported = self.wait_for(
                Instant::now() + DMABUF_WAIT,
                |wl| matches!(wl.dmabuf_result, Some((done, _)) if done == serial),
            );
            params.destroy();
            let _ = self.conn.flush();

            if !imported {
                self.wl.borrow_mut().dmabuf_keepalives.remove(&serial);
                return Err(DisplayError::DmaBufRejected {
                    reason: "DMA-BUF 导入超时（250ms 内没有 created/failed 事件）".into(),
                });
            }
            let mut wl = self.wl.borrow_mut();
            match wl.dmabuf_result.take() {
                Some((_, Ok(buffer))) => {
                    // 这一组格式被证明能导入 ⇒ 下一帧起走同步路径。
                    wl.dmabuf_warm.insert((frame.fourcc, frame.modifier));
                    buffer
                }
                Some((_, Err(()))) => {
                    wl.dmabuf_keepalives.remove(&serial);
                    return Err(DisplayError::DmaBufRejected {
                        reason: format!(
                            "合成器拒绝导入（fourcc=0x{:08X} modifier=0x{:016X} {}x{}）",
                            frame.fourcc, frame.modifier, frame.width, frame.height
                        ),
                    });
                }
                None => {
                    wl.dmabuf_keepalives.remove(&serial);
                    return Err(DisplayError::DmaBufRejected {
                        reason: "DMA-BUF 导入结果丢失".into(),
                    });
                }
            }
        };

        // 提交：viewport 表达「帧矩形 → 逻辑屏幕」，缩放由合成器 GPU 做。
        let seq = {
            let mut wl = self.wl.borrow_mut();
            let entry_surface = wl
                .outputs
                .get(&global)
                .and_then(|entry| entry.surface.clone());
            let Some(surface) = entry_surface else {
                if buffer.is_alive() {
                    buffer.destroy();
                }
                wl.dmabuf_buffer_keepalive.remove(&buffer.id());
                if let Some(imported_serial) = wl.dmabuf_buffer_serial.remove(&buffer.id()) {
                    wl.dmabuf_keepalives.remove(&imported_serial);
                }
                return Err(DisplayError::OutputNotFound(output));
            };
            let Some(entry) = wl.outputs.get_mut(&global) else {
                if buffer.is_alive() {
                    buffer.destroy();
                }
                wl.dmabuf_buffer_keepalive.remove(&buffer.id());
                if let Some(imported_serial) = wl.dmabuf_buffer_serial.remove(&buffer.id()) {
                    wl.dmabuf_keepalives.remove(&imported_serial);
                }
                return Err(DisplayError::OutputNotFound(output));
            };
            if let Some(viewport) = entry.viewport.as_ref().filter(|v| v.is_alive()) {
                let (sx, sy, sw, sh) = clamp_viewport_source(src, frame.width, frame.height);
                viewport.set_source(sx, sy, sw, sh);
                viewport.set_destination(
                    i32::try_from(cfg.0).unwrap_or(i32::MAX),
                    i32::try_from(cfg.1).unwrap_or(i32::MAX),
                );
                // 有 viewport 时 buffer_scale 必须是 1（否则单位被再除一次）。
                surface.set_buffer_scale(1);
            } else {
                surface.set_buffer_scale(entry.buffer_scale());
            }
            surface.attach(Some(&buffer), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, width, height);
            } else {
                surface.damage(
                    0,
                    0,
                    i32::try_from(cfg.0).unwrap_or(i32::MAX),
                    i32::try_from(cfg.1).unwrap_or(i32::MAX),
                );
            }
            let _callback = surface.frame(&qh_handle, FrameUdata(global));
            surface.commit();
            entry.frame_pending = true;
            entry.sequence = entry.sequence.wrapping_add(1);
            // DMA-BUF 稳态下合成器已不再持有此前的 CPU 共享内存缓冲时，释放空闲池以收回 memfd 内存。
            if entry
                .pool
                .as_ref()
                .is_some_and(|p| p.free_slot_count() == crate::pool::SLOT_COUNT as usize)
            {
                entry.pool = None;
                tracing::debug!(output = %global, "DMA-BUF 稳态：已释放空闲整屏 wl_shm 池");
            }
            if entry
                .frame_pool
                .as_ref()
                .is_some_and(|p| p.free_slot_count() == crate::pool::SLOT_COUNT as usize)
            {
                entry.frame_pool = None;
                tracing::debug!(output = %global, "DMA-BUF 稳态：已释放空闲帧级 wl_shm 池");
            }
            entry.sequence
        };
        let _ = self.conn.flush();

        Ok(PresentToken {
            output,
            sequence: seq,
        })
    }
}

// ---------------------------------------------------------------------------
// DisplayBackend
// ---------------------------------------------------------------------------

impl DisplayBackend for WaylandBackend {
    fn outputs(&self) -> Vec<OutputInfo> {
        if self.wl.borrow().exited {
            return Vec::new();
        }
        // 顺带泵：热插/热拔事件在这里被发现（daemon 的 check_connectors
        // 周期调用本方法比对通道）。
        self.pump_until(Instant::now());
        let wl = self.wl.borrow();
        if wl.exited {
            return Vec::new();
        }
        wl.live_entries()
            .iter()
            .map(|(_, e)| OutputInfo {
                id: OutputId::new(e.key()),
                connected: true,
                modes: e.modes.clone(),
                current_mode: e.current_mode,
                physical_size_mm: e.physical_mm.map(|(w, h)| {
                    (
                        w.clamp(0, u16::MAX as i32) as u16,
                        h.clamp(0, u16::MAX as i32) as u16,
                    )
                }),
            })
            .collect()
    }

    fn default_output(&self, wanted: Option<&str>) -> Result<OutputId, DisplayError> {
        self.pump_until(Instant::now());
        let wl = self.wl.borrow();
        if wl.exited {
            return Err(DisplayError::DeviceUnavailable(
                "Wayland 合成器连接已断开".into(),
            ));
        }
        let live = wl.live_entries();
        match wanted {
            Some(name) => {
                if live.iter().any(|(_, e)| e.key() == name) {
                    Ok(OutputId::new(name))
                } else {
                    let names: Vec<String> = live.iter().map(|(_, e)| e.key()).collect();
                    Err(DisplayError::DeviceUnavailable(format!(
                        "找不到输出 {name}；可用输出: {}",
                        names.join(", ")
                    )))
                }
            }
            None => live
                .first()
                .map(|(_, e)| OutputId::new(e.key()))
                .ok_or_else(|| {
                    DisplayError::DeviceUnavailable(
                        "没有任何已连接的显示器输出（无可用 wl_output）".into(),
                    )
                }),
        }
    }

    fn resolve_mode(
        &self,
        output: &OutputId,
        selection: &ModeSelection,
    ) -> Result<DisplayMode, DisplayError> {
        let global = self.resolve_global(output)?;
        let wl = self.wl.borrow();
        let entry = wl
            .outputs
            .get(&global)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        let current = entry.current_mode.or(entry.preferred_mode).ok_or_else(|| {
            DisplayError::DeviceUnavailable("合成器尚未报告该输出的当前模式".into())
        })?;
        match selection {
            ModeSelection::Preferred => Ok(current),
            ModeSelection::Manual {
                width,
                height,
                refresh_millihz,
            } => {
                let found = entry
                    .modes
                    .iter()
                    .find(|m| {
                        m.width == *width
                            && m.height == *height
                            && refresh_millihz.is_none_or(|r| m.refresh_millihz == r)
                    })
                    .copied();
                found.ok_or(DisplayError::ModeNotSupported {
                    output: output.clone(),
                    mode: DisplayMode {
                        width: *width,
                        height: *height,
                        refresh_millihz: refresh_millihz.unwrap_or(0),
                    },
                })
            }
        }
    }

    fn connector_connected(&self, name: &str) -> Result<bool, DisplayError> {
        let wl = self.wl.borrow();
        if wl.outputs.values().any(|e| !e.dead && e.key() == name) {
            Ok(true)
        } else if wl.outputs.values().any(|e| e.key() == name) {
            Ok(false)
        } else {
            Err(DisplayError::OutputNotFound(OutputId::new(name)))
        }
    }

    fn present(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
    ) -> Result<PresentToken, DisplayError> {
        self.present_fitted(
            output,
            frame,
            (0, 0, 0, 0), // identity：由 present_fitted 内部处理
            (0, 0, 0, 0),
        )
    }

    fn present_fitted(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
        src: SrcRect,
        dst: DstRect,
    ) -> Result<PresentToken, DisplayError> {
        // daemon 的 present_frame 对 CPU 帧转发 identity src/dst；
        // 这里把 identity 特殊化为「帧矩形铺满 dst 矩形」语义。
        let (src, dst) = normalize_rects(&frame, src, dst);
        match frame {
            DisplayFrame::Cpu(f) => self.present_cpu_fitted(output, f, src, dst),
            DisplayFrame::DmaBuf(f) => self.present_dmabuf_fitted(output, f, src, dst),
            DisplayFrame::GpuTexture(_) => Err(DisplayError::Unsupported(
                "wl_shm 后端不支持 GPU 纹理提交".into(),
            )),
        }
    }

    fn set_mode(&mut self, output: OutputId, mode: DisplayMode) -> Result<(), DisplayError> {
        let global = self.resolve_global(&output)?;
        let wl = self.wl.borrow();
        let entry = wl
            .outputs
            .get(&global)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        // Wayland 客户端不能改输出模式；合成器当前模式即唯一合法模式。
        if entry.current_mode == Some(mode) {
            Ok(())
        } else {
            Err(DisplayError::ModeNotSupported { output, mode })
        }
    }

    fn suspend(&mut self) -> Result<(), DisplayError> {
        // 合成器接管全部上屏节奏；无需挂起。
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DisplayError> {
        Ok(())
    }

    /// 零拷贝能力：`zwp_linux_dmabuf_v1` 在位且合成器已广播格式表。
    ///
    /// 返回 `true` 后 daemon 会让媒体侧产出 DMA-BUF 帧；导入失败（modifier
    /// 不被接受等）会返回 [`DisplayError::DmaBufRejected`]，daemon 据此闩死
    /// 直通并重开 CPU 管线 —— 因此这里的"乐观"是安全的。
    fn accepts_dmabuf_format(&self, fourcc: u32, modifier: u64) -> bool {
        if self.ensure_alive().is_err() {
            return false;
        }
        let wl = self.wl.borrow();
        wl.dmabuf_supports(fourcc, modifier)
    }

    fn dmabuf_modifiers_for(&self, fourcc: u32) -> Vec<u64> {
        if self.ensure_alive().is_err() {
            return Vec::new();
        }
        let wl = self.wl.borrow();
        wl.dmabuf_formats
            .iter()
            .find(|f| f.fourcc == fourcc)
            .map(|f| f.modifiers.clone())
            .unwrap_or_default()
    }

    fn supports_dmabuf_scanout(&self) -> bool {
        self.ensure_alive().is_ok() && self.wl.borrow().dmabuf_available()
    }

    /// wl_shm 只有 RGB 格式：NV12 帧无法上传（会被 blit 层以 `Unsupported` 拒绝）。
    ///
    /// 声明为 `false` 后，媒体侧改用 RGBA 捕获模式；硬解时由 `vapostproc`
    /// 在 GPU 侧把 NV12 转成 RGBA（真机 2026-11-21：不声明则硬解视频
    /// 稳定被降级成 `DegradedStatic`）。
    fn accepts_yuv_cpu_frames(&self) -> bool {
        false
    }

    /// 有没有窗口正全屏（有就返回它，供上层"暂停壁纸"）。
    ///
    /// 判定口径：`state` 里带 `fullscreen` 位**且** `minimized` 为假。
    /// 最小化 + fullscreen 的组合（窗口切到别的桌面）不该让壁纸停——
    /// 用户看不见它，壁纸却停着，那才是 bug。
    fn fullscreen_toplevel(&self) -> Option<FullscreenToplevel> {
        if self.ensure_alive().is_err() {
            return None;
        }
        let wl = self.wl.borrow();
        // 同时有多个全屏窗口时（游戏 + 全屏浏览器），优先报告**当前激活**的那个：
        // 它是用户正在看的，也是日志/界面里最不该报错的对象。没有激活的就随便取
        // 一个 —— 对"要不要暂停"这件事两者等价。
        let mut candidates = wl
            .toplevels
            .values()
            .filter(|entry| entry.state.fullscreen && !entry.state.minimized);
        let chosen = candidates
            .clone()
            .find(|entry| entry.state.activated)
            .or_else(|| candidates.next());
        chosen.map(|entry| FullscreenToplevel {
            app_id: entry.app_id.clone(),
            title: entry.title.clone(),
            outputs: entry.outputs.clone(),
        })
    }

    fn commit_stats(&self) -> Option<CommitStats> {
        None
    }
}

// ---------------------------------------------------------------------------
// rect 归一化 + 单元测试
// ---------------------------------------------------------------------------

/// 判断 `dst`（模式物理像素矩形）是否铺满整张层表面，决定能否走 fast-path/DMA-BUF。
fn entry_fills_surface(entry: &OutputEntry, dst: DstRect) -> bool {
    let (tw, th) = entry
        .current_mode
        .map(|m| (m.width, m.height))
        .or_else(|| entry.buffer_dims())
        .or(entry.cfg)
        .unwrap_or((0, 0));
    dst.0 == 0
        && dst.1 == 0
        && dst.2.saturating_add(1) >= u64::from(tw)
        && dst.3.saturating_add(1) >= u64::from(th)
}

/// 将 16.16 定点 `src` 钳制并量化到 24.8 `wl_fixed` 合法区间，确保 `x + w <= frame_w` 且 `y + h <= frame_h`。
fn clamp_viewport_source(src: SrcRect, frame_w: u32, frame_h: u32) -> (f64, f64, f64, f64) {
    let max_x = i64::from(frame_w.max(1)) * 256;
    let max_y = i64::from(frame_h.max(1)) * 256;
    let x24 = ((src.0 >> 8) as i64).clamp(0, max_x.saturating_sub(1));
    let y24 = ((src.1 >> 8) as i64).clamp(0, max_y.saturating_sub(1));
    let w24 = ((src.2 >> 8) as i64).clamp(1, (max_x - x24).max(1));
    let h24 = ((src.3 >> 8) as i64).clamp(1, (max_y - y24).max(1));
    (
        x24 as f64 / 256.0,
        y24 as f64 / 256.0,
        w24 as f64 / 256.0,
        h24 as f64 / 256.0,
    )
}

/// daemon 的 `present`（未拟合）路径给 (0,0,0,0) 矩形；展开为帧/缓冲全幅。
/// `present_fitted` 收到的 src 是 16.16 定点；dst 是模式像素。
fn normalize_rects(frame: &DisplayFrame, src: SrcRect, dst: DstRect) -> (SrcRect, DstRect) {
    let (fw, fh) = (frame.width(), frame.height());
    let src = if src == (0, 0, 0, 0) {
        (0, 0, u64::from(fw) << 16, u64::from(fh) << 16)
    } else {
        src
    };
    let dst = if dst == (0, 0, 0, 0) {
        (0, 0, u64::from(fw), u64::from(fh))
    } else {
        dst
    };
    (src, dst)
}

impl Drop for WaylandBackend {
    fn drop(&mut self) {
        // 代理 Drop 不发 destructor 请求；显式销毁避免合成器端悬挂
        // surface（断开 socket 也会清理，这里只是礼貌收尾）。
        let mut wl = self.wl.borrow_mut();
        for entry in wl.outputs.values_mut() {
            if let Some(viewport) = entry.viewport.take() {
                if viewport.is_alive() {
                    viewport.destroy();
                }
            }
            if let Some(fractional) = entry.fractional.take() {
                if fractional.is_alive() {
                    fractional.destroy();
                }
            }
            if let Some(layer) = entry.layer.take() {
                if layer.is_alive() {
                    layer.destroy();
                }
            }
            if let Some(surface) = entry.surface.take() {
                if surface.is_alive() {
                    surface.destroy();
                }
            }
            entry.pool = None;
            if entry.output.is_alive() && entry.output.version() >= 3 {
                entry.output.release();
            }
        }
        let _ = self.conn.flush();
    }
}

/// 分片等待：谓词成立即返回，最长等到 `deadline`（t19）。
///
/// `pump` 只负责"推进一小段时间"（真实实现是 `pump_until(slice_deadline)`，
/// 单片 ≤ `slice`）。每次泵完**立刻**复检谓词，所以"谓词在第 N 次 dispatch 后为
/// 真"的延迟是**一片**而不是"一整段 deadline"——这正是 120fps 上限的成因
/// （旧实现每片固定 10ms，120fps 的 8.33ms 帧预算全被空等吃掉）。
///
/// 写成泛型是为了单测：真实泵需要 Wayland 连接，而这里要断言的恰恰是
/// "谓词成立后多快返回"和"永不成立时按 deadline 超时"。
fn wait_until<Pred, Pump>(deadline: Instant, slice: Duration, pred: Pred, mut pump: Pump) -> bool
where
    Pred: Fn() -> bool,
    Pump: FnMut(Instant),
{
    loop {
        if pred() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return pred();
        }
        pump((now + slice).min(deadline));
        // 片间复检：谓词已成立就不再泵——旧实现缺的就是这两行。
        if pred() {
            return true;
        }
    }
}

/// 优先按环境变量连接 Wayland；若 `WAYLAND_DISPLAY` 未设置，自动在 `$XDG_RUNTIME_DIR`
/// 扫描 `wayland-<数字>` socket 兜底连接（解决桌面会话先起、systemd --user 未继承环境的问题）。
fn connect_wayland_with_fallback() -> Result<Connection, DisplayError> {
    match Connection::connect_to_env() {
        Ok(conn) => Ok(conn),
        Err(env_err) => {
            let explicit_empty = std::env::var_os("WAYLAND_DISPLAY").is_none_or(|v| v.is_empty());
            if explicit_empty
                && let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.is_absolute())
            {
                for sock in discover_wayland_sockets_in(&runtime_dir) {
                    if let Ok(stream) = std::os::unix::net::UnixStream::connect(&sock)
                        && let Ok(conn) = Connection::from_socket(stream)
                    {
                        tracing::info!(
                            socket = %sock.display(),
                            "WAYLAND_DISPLAY 未设置，已自动从 XDG_RUNTIME_DIR 发现并连接 Wayland 合成器"
                        );
                        return Ok(conn);
                    }
                }
            }
            Err(DisplayError::DeviceUnavailable(format!(
                "无法连接 Wayland 合成器（WAYLAND_DISPLAY 未设置或合成器不可达）: {env_err}"
            )))
        }
    }
}

fn discover_wayland_sockets_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut numbered: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(num_str) = name_str.strip_prefix("wayland-") else {
            continue;
        };
        if num_str.is_empty() || !num_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(num) = num_str.parse::<u32>() {
            numbered.push((num, entry.path()));
        }
    }
    numbered.sort_by_key(|(num, _)| *num);
    numbered.into_iter().map(|(_, path)| path).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use liuguang_core::types::{CpuFrame, PixelFormat};
    use std::sync::Arc;

    /// t19：谓词在第 N 次 dispatch 后成立 ⇒ 立即返回（一片之内），不再空等到 deadline。
    ///
    /// 旧实现 `wait_for` 每轮都 `pump_until(now + 10ms)`，谓词只在泵**之后**
    /// 的下一轮才复检 ⇒ 120fps（8.33ms/帧）固定等到 ~10ms，呈现被压到 ~82fps。
    #[test]
    fn wait_until_returns_as_soon_as_the_predicate_holds() {
        const DISPATCHES: u32 = 3;
        const SLICE: Duration = Duration::from_millis(1);
        // 两个闭包同时存活（谓词只读、泵可写），所以用 Cell 而不是 &mut。
        let dispatches = std::cell::Cell::new(0u32);
        let start = Instant::now();
        let deadline = start + Duration::from_millis(100);
        let ok = wait_until(
            deadline,
            SLICE,
            || dispatches.get() >= DISPATCHES,
            |_slice_deadline| {
                dispatches.set(dispatches.get() + 1);
                // 模拟"泵了一片"：真实泵会 poll 到片 deadline。
                std::thread::sleep(Duration::from_micros(200));
            },
        );
        let elapsed = start.elapsed();
        assert!(ok, "谓词已成立却报超时");
        assert_eq!(dispatches.get(), DISPATCHES, "谓词成立后不得继续泵");
        assert!(
            elapsed <= Duration::from_millis(10),
            "谓词成立后必须一片内返回（旧实现这里是 ~10ms/片），实际 {elapsed:?}"
        );
    }

    /// 谓词永不成立 ⇒ 按 deadline 超时返回 false（慢合成器不能被当成"成功"）。
    #[test]
    fn wait_until_times_out_at_the_deadline() {
        let deadline = Instant::now() + Duration::from_millis(20);
        let pumps = std::cell::Cell::new(0u32);
        let start = Instant::now();
        let ok = wait_until(
            deadline,
            Duration::from_millis(1),
            || false,
            |_slice_deadline| {
                pumps.set(pumps.get() + 1);
                std::thread::sleep(Duration::from_micros(500));
            },
        );
        let elapsed = start.elapsed();
        assert!(!ok, "谓词永不成立时不得返回 true");
        assert!(pumps.get() > 0, "超时前必须真的泵过");
        assert!(
            elapsed >= Duration::from_millis(20),
            "必须等到 deadline 才放弃（否则慢合成器会变成假失败），实际 {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(500),
            "超时不能远超 deadline，实际 {elapsed:?}"
        );
    }

    /// NV12 帧的平面数必须按 `strides` 里非零的项数算（媒体侧只填有效平面）。
    #[test]
    fn plane_count_counts_nonzero_strides() {
        let fd = std::fs::File::open("/dev/null").unwrap();
        let frame = DmaBufFrame {
            width: 1920,
            height: 1080,
            stride: 1920,
            format: PixelFormat::Nv12,
            fds: vec![Arc::new(std::os::fd::OwnedFd::from(fd))],
            object_index: [0; 4],
            keepalive: None,
            modifier: 0,
            fourcc: 0x3231_564E,
            strides: [1920, 1920, 0, 0],
            offsets: [0, 1920 * 1080, 0, 0],
        };
        assert_eq!(plane_count(&frame), 2);
        // 单平面（RGB）：只有 strides[0] 非零。
        let rgb = DmaBufFrame {
            strides: [1920 * 4, 0, 0, 0],
            offsets: [0, 0, 0, 0],
            ..frame
        };
        assert_eq!(plane_count(&rgb), 1);
    }

    #[test]
    fn plane_count_is_clamped_to_four() {
        // 协议最多 4 个平面；畸形帧不能把 add() 越界。
        let fd = std::fs::File::open("/dev/null").unwrap();
        let frame = DmaBufFrame {
            width: 16,
            height: 16,
            stride: 16,
            format: PixelFormat::Nv12,
            fds: vec![Arc::new(std::os::fd::OwnedFd::from(fd))],
            object_index: [0; 4],
            keepalive: None,
            modifier: 0,
            fourcc: 0x3231_564E,
            strides: [16, 16, 16, 16],
            offsets: [0, 0, 0, 0],
        };
        assert_eq!(plane_count(&frame), 4);
    }

    fn cpu(w: u32, h: u32) -> DisplayFrame {
        DisplayFrame::Cpu(CpuFrame {
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; (w * h * 4) as usize].into_boxed_slice()),
        })
    }

    #[test]
    fn zero_rects_expand_to_full_frame() {
        let f = cpu(4, 2);
        let (src, dst) = normalize_rects(&f, (0, 0, 0, 0), (0, 0, 0, 0));
        assert_eq!(src, (0, 0, 4 << 16, 2 << 16));
        assert_eq!(dst, (0, 0, 4, 2));
    }

    #[test]
    fn explicit_rects_pass_through() {
        let f = cpu(4, 2);
        let (src, dst) = normalize_rects(&f, (1, 2, 3, 4), (5, 6, 7, 8));
        assert_eq!(src, (1, 2, 3, 4));
        assert_eq!(dst, (5, 6, 7, 8));
    }

    #[test]
    fn upload_format_maps_memory_layout_to_shm_format() {
        // 帧内存 [R,G,B,A] ⇒ wl_shm abgr8888；[B,G,R,X] ⇒ xrgb8888。
        let all = [
            wl_shm::Format::Xrgb8888,
            wl_shm::Format::Argb8888,
            wl_shm::Format::Abgr8888,
        ];
        assert_eq!(
            pick_upload_format_from(&all, PixelFormat::Rgba8888),
            Some(wl_shm::Format::Abgr8888)
        );
        assert_eq!(
            pick_upload_format_from(&all, PixelFormat::Xrgb8888),
            Some(wl_shm::Format::Xrgb8888)
        );
        assert_eq!(
            pick_upload_format_from(&all, PixelFormat::Argb8888),
            Some(wl_shm::Format::Argb8888)
        );
    }

    #[test]
    fn upload_format_requires_compositor_announcement() {
        // 合成器只广播 xrgb8888 ⇒ RGBA 帧不能零转换上传（必须回退 CPU 路径）。
        let only_xrgb = [wl_shm::Format::Xrgb8888];
        assert_eq!(
            pick_upload_format_from(&only_xrgb, PixelFormat::Rgba8888),
            None
        );
        assert_eq!(
            pick_upload_format_from(&[], PixelFormat::Xrgb8888),
            None,
            "空广播列表不允许任何格式"
        );
        // NV12 无论广播什么都不走快路径。
        assert_eq!(
            pick_upload_format_from(&[wl_shm::Format::Abgr8888], PixelFormat::Nv12),
            None
        );
    }

    #[test]
    fn zero_sized_frame_expands_to_zero() {
        // 零尺寸帧（异常路径）→ 全零矩形 → blit 报 InvalidFrame。
        let f = cpu(0, 0);
        let (src, dst) = normalize_rects(&f, (0, 0, 0, 0), (0, 0, 0, 0));
        assert_eq!(src, (0, 0, 0, 0));
        let _ = dst;
    }

    #[test]
    fn discovers_and_orders_wayland_sockets_skipping_locks() {
        let dir = std::env::temp_dir().join(format!("liuguang-wl-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "wayland-10",
            "wayland-1",
            "wayland-0",
            "wayland-0.lock",
            "wayland-foo",
            "other-socket",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let found: Vec<String> = discover_wayland_sockets_in(&dir)
            .into_iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(found, vec!["wayland-0", "wayland-1", "wayland-10"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clamp_viewport_source_never_overflows_24_8_fixed() {
        let (x, y, w, h) =
            clamp_viewport_source((128, 128, (3840 << 16), (2160 << 16)), 3840, 2160);
        assert!(x >= 0.0 && y >= 0.0);
        assert!(w >= 1.0 / 256.0 && h >= 1.0 / 256.0);
        assert!((x * 256.0).round() + (w * 256.0).round() <= 3840.0 * 256.0);
        assert!((y * 256.0).round() + (h * 256.0).round() <= 2160.0 * 256.0);
    }
}
