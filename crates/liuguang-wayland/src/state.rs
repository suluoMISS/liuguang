//! Wayland 事件状态与全部 `Dispatch` 处理器。
//!
//! 单线程模型：所有协议回调只碰 `&mut WlState`，绝不回调
//! `WaylandBackend`（无 RefCell 重入）。backend 侧通过私有 pump
//! 助手驱动队列后读 `WlState` 快照。

use std::cell::RefCell;
use std::collections::HashMap;

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::GlobalListContents,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm,
        wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use liuguang_core::types::FrameKeepalive;
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::pool::Pool;

// ---------------------------------------------------------------------------
// udata 类型
// ---------------------------------------------------------------------------

/// `wl_output` 的用户数据：registry 全局名。
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputUdata(pub u32);

/// `wl_surface` 的用户数据（预留：surface 事件目前无路由需求）。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SurfaceUdata(pub u32);

/// `zwlr_layer_surface_v1` 的用户数据。
#[derive(Clone, Copy, Debug)]
pub(crate) struct LayerUdata(pub u32);

/// `wl_callback`（frame 节拍）的用户数据。
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameUdata(pub u32);

/// `wl_buffer` 的用户数据：槽位回收路由。
#[derive(Clone, Copy, Debug)]
pub(crate) struct BufferUdata {
    pub global: u32,
    pub slot: u32,
    /// 槽属于哪个池：整屏合成池还是快路径帧池（释放事件必须回到同一个池，
    /// 否则帧池槽位永远处于 busy，几十帧后必然「空槽竞争丢失」）。
    pub frame_pool: bool,
}

/// `wp_fractional_scale_v1` 的用户数据：所属输出的 registry 全局名。
#[derive(Clone, Copy, Debug)]
pub(crate) struct FractionalUdata(pub u32);

/// `zwp_linux_buffer_params_v1` 的用户数据：导入序号（与 `WlState::dmabuf_result` 配对）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct DmabufParamsUdata(pub u32);

/// 合成器广播的一个「格式 + 支持的 modifier 列表」。
#[derive(Clone, Debug, Default)]
pub(crate) struct DmabufFormat {
    /// DRM fourcc（如 NV12 = 0x3231564E）。
    pub fourcc: u32,
    /// 该格式支持的 modifier 列表；空 = 只支持隐式 modifier。
    pub modifiers: Vec<u64>,
}

impl DmabufFormat {
    /// 该格式是否可用于 `modifier`（`DRM_FORMAT_MOD_INVALID` = 隐式 modifier）。
    pub(crate) fn accepts(&self, modifier: u64) -> bool {
        /// `DRM_FORMAT_MOD_INVALID`：表示"隐式 modifier"，等价于不指定。
        const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
        if modifier == MOD_INVALID {
            return true;
        }
        // 线性布局：很多合成器只在格式事件里广播（不带 modifier 事件），
        // 此时 LINEAR 仍然可用。
        if modifier == 0 && self.modifiers.is_empty() {
            return true;
        }
        self.modifiers.contains(&modifier)
    }
}

/// `wp_fractional_scale_v1.preferred_scale` 的分母（协议规定 120 = 1.0）。
pub(crate) const FRACTIONAL_SCALE_DENOM: u64 = 120;

// ---------------------------------------------------------------------------
// 输出条目
// ---------------------------------------------------------------------------

/// 一个 wl_output 的全部运行时状态。
#[derive(Debug)]
pub(crate) struct OutputEntry {
    /// registry 全局名（即输出键的后备 id）。
    pub global: u32,
    pub output: wl_output::WlOutput,
    /// `wl_output.name`（如 niri 的 `eDP-1`）；未知时为 `None`。
    pub name: Option<String>,
    pub description: Option<String>,
    pub physical_mm: Option<(i32, i32)>,
    pub scale: i32,
    /// Mode 事件累计（物理模式列表；合成器报告）。
    pub modes: Vec<liuguang_core::types::DisplayMode>,
    pub current_mode: Option<liuguang_core::types::DisplayMode>,
    /// 合成器标了 `preferred` 位但没标 `current` 时的备选（极少数合成器）；
    /// 两者都没有时留在 `None`，由上层如实报错。
    pub preferred_mode: Option<liuguang_core::types::DisplayMode>,
    /// layer surface 与其配置尺寸。
    pub surface: Option<wl_surface::WlSurface>,
    pub layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    /// `wp_viewport`（有 viewporter 时建；配合 fractional scale 使用）。
    pub viewport: Option<wp_viewport::WpViewport>,
    /// `wp_fractional_scale_v1`（有 fractional-scale 时建）。
    pub fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    /// 合成器建议的分数缩放分子（分母 120；1.5 → 180）。没有该协议时为 `None`。
    pub fractional_factor: Option<u32>,
    /// 已 ack 的 configure 尺寸（**逻辑**坐标；buffer 尺寸 = cfg × 有效缩放）。
    pub cfg: Option<(u32, u32)>,
    /// 待 ack 的 configure（serial, w, h）。
    pub pending_configure: Option<(u32, u32, u32)>,
    pub pool: Option<Pool>,
    /// 快路径专用池：尺寸/格式**跟随帧**（不跟 configure），配合
    /// `wp_viewport.set_source/set_destination` 让合成器做缩放。
    /// 与 `pool` 分开是因为两者尺寸语义不同（前者=帧，后者=逻辑屏幕×缩放）。
    pub frame_pool: Option<Pool>,
    /// 上一帧 commit 后是否还在等 frame 回调（单帧 in-flight 背压）。
    pub frame_pending: bool,
    /// 已提交帧计数（PresentToken.sequence 递增源）。
    pub sequence: u64,
    /// 全局被移除 / surface Closed → true；此后输出不再可见。
    pub dead: bool,
}

impl OutputEntry {
    /// 输出键：优先 `wl_output.name`，否则 `output-<global>`。
    pub(crate) fn key(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("output-{}", self.global))
    }

    /// 合成器建议的缩放是否可用（fractional 协议在位且已收到 factor）。
    pub(crate) fn has_fractional_scale(&self) -> bool {
        self.fractional_factor.is_some() && self.viewport.is_some()
    }

    /// `wl_surface.set_buffer_scale` 应设的值。
    ///
    /// 走 fractional 路径时必须是 1（协议要求：缩放交给 `wp_viewport` 的
    /// destination 表达）；否则用 `wl_output.scale` 的整数值。
    pub(crate) fn buffer_scale(&self) -> i32 {
        surface_buffer_scale(self.has_fractional_scale(), self.scale)
    }

    /// 池缓冲尺寸（**像素**）。
    pub(crate) fn buffer_dims(&self) -> Option<(u32, u32)> {
        Some(scaled_dims(
            self.cfg?,
            self.fractional_factor,
            self.scale,
            self.viewport.is_some(),
        ))
    }
}

/// `wl_surface.set_buffer_scale` 的取值：fractional 生效时必须为 1。
pub(crate) fn surface_buffer_scale(fractional_active: bool, integer_scale: i32) -> i32 {
    if fractional_active {
        1
    } else {
        integer_scale.max(1)
    }
}

/// 缓冲像素尺寸 = configure 逻辑尺寸 × 有效缩放。
///
/// - fractional（`factor` 为 120 进制的分子，1.5 → 180）：`round(cfg × factor / 120)`。
///   1.5 屏上 1707 逻辑宽 → 2561 像素，正好是物理分辨率 2560，比整数 scale 2
///   （3414 像素、多占一倍内存）更省也更清晰；
/// - 整数回退：`cfg × wl_output.scale`。
///
/// `has_viewport` 为 false 时即便收到 factor 也不能用（没有 viewport 就没法
/// 把大 buffer 映射回逻辑尺寸），退回整数路径。
pub(crate) fn scaled_dims(
    cfg: (u32, u32),
    fractional_factor: Option<u32>,
    integer_scale: i32,
    has_viewport: bool,
) -> (u32, u32) {
    let (cw, ch) = cfg;
    match fractional_factor {
        Some(factor) if has_viewport && factor > 0 => {
            let scale = |v: u32| -> u32 {
                let num = u64::from(v) * u64::from(factor);
                let rounded = (num + FRACTIONAL_SCALE_DENOM / 2) / FRACTIONAL_SCALE_DENOM;
                u32::try_from(rounded).unwrap_or(u32::MAX).max(1)
            };
            (scale(cw), scale(ch))
        }
        _ => {
            let s = u32::try_from(integer_scale.max(1)).unwrap_or(1);
            (cw.saturating_mul(s), ch.saturating_mul(s))
        }
    }
}

/// 销毁分数缩放相关的 add-on 对象（viewport 必须先于 surface 销毁）。
fn destroy_scale_objects(entry: &mut OutputEntry) {
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
    entry.fractional_factor = None;
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

/// 由 `zwlr_foreign_toplevel_handle_v1.state` 解出的窗口状态位。
///
/// 协议把状态打包成 `array`（一串 **little-endian u32**），而不是若干布尔事件——
/// 所以必须显式解码；直接按字节比较会漏掉"同一数组里 fullscreen+activated"的情况。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ToplevelState {
    /// 最大化（0）。
    pub maximized: bool,
    /// 最小化（1）。
    pub minimized: bool,
    /// 处于激活态（2）。
    pub activated: bool,
    /// 全屏（3，协议 since 2）。
    pub fullscreen: bool,
}

/// 解码 `zwlr_foreign_toplevel_handle_v1.state` 的 `array` 参数。
///
/// 尾部不足 4 字节的残片直接忽略（协议保证是 u32 数组，但**别人的实现**不一定
/// 老实；忽略未知值比 panic 好——这是壁纸进程，不能因为一个畸形事件死掉）。
pub(crate) fn parse_state_flags(bytes: &[u8]) -> ToplevelState {
    let mut state = ToplevelState::default();
    for chunk in bytes.chunks_exact(4) {
        let Ok(raw) = <[u8; 4]>::try_from(chunk) else {
            continue;
        };
        match u32::from_le_bytes(raw) {
            0 => state.maximized = true,
            1 => state.minimized = true,
            2 => state.activated = true,
            3 => state.fullscreen = true,
            // 未来协议版本新增的状态：按位与语义无关，忽略。
            _ => {}
        }
    }
    state
}

/// 一个顶层窗口（`zwlr_foreign_toplevel_handle_v1`）的当前快照。
#[derive(Debug)]
pub(crate) struct ToplevelEntry {
    /// 窗口标题（可能晚于 `toplevel` 事件到达）。
    pub title: Option<String>,
    /// 应用 id（niri 里就是 app_id，例如 `steam`/`gamescope`）。
    pub app_id: Option<String>,
    /// 窗口所在的输出名（`wl_output.name`）。
    ///
    /// 用名字而不是全局号：全局号在同一会话里稳定，但用户看到的是 `eDP-1`，
    /// 日志与 `status` 里必须是能对上的名字。
    pub outputs: Vec<String>,
    /// 状态位。
    pub state: ToplevelState,
}

impl ToplevelEntry {
    fn new() -> Self {
        Self {
            title: None,
            app_id: None,
            outputs: Vec::new(),
            state: ToplevelState::default(),
        }
    }
}

/// backend 的事件状态（被 `RefCell` 包裹，见 `WaylandBackend`）。
#[derive(Debug, Default)]
pub(crate) struct WlState {
    /// 合成器已断连（flush/read 失败或 display error 事件）。
    pub exited: bool,
    /// 单例代理副本（hotplug 处理器建 layer surface 用）。
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub shm: Option<wl_shm::WlShm>,
    pub shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    /// `wp_viewporter`（可选全局；不在位则走整数 scale 回退）。
    pub viewporter: Option<wp_viewporter::WpViewporter>,
    /// `wp_fractional_scale_manager_v1`（可选全局；不在位则走整数 scale 回退）。
    pub fractional_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    /// 合成器在 `wl_shm.format` 里广播的像素格式（决定能否零转换上传）。
    pub shm_formats: Vec<wl_shm::Format>,
    /// `zwp_linux_dmabuf_v1`（可选全局；不在位则零拷贝路径不可用）。
    pub dmabuf: Option<ZwpLinuxDmabufV1>,
    /// 合成器广播的 `格式 + modifier` 表（DMA-BUF 导入前必须校验）。
    pub dmabuf_formats: Vec<DmabufFormat>,
    /// 最近一次 `zwp_linux_buffer_params_v1` 导入结果：
    /// `(序号, Ok(wl_buffer)|Err(()))`。导入是异步的（`created`/`failed` 事件），
    /// present 路径创建参数后泵到结果出现为止。
    pub dmabuf_result: Option<(u32, Result<wl_buffer::WlBuffer, ()>)>,
    /// 导入序号自增器。
    pub dmabuf_serial: u32,
    /// DMA-BUF 帧的寿命延长器（key = 导入序号）：供应商的解码表面要活到
    /// 合成器发 `wl_buffer.release` 为止，否则会被复用去解码下一帧（撕裂/错帧）。
    pub dmabuf_keepalives: std::collections::HashMap<u32, FrameKeepalive>,
    /// `wl_buffer` 对象 → 导入序号：`release` 事件里靠它找到该释放哪个延长器
    /// （`params.create` 没有 udata 参数，buffer 的子对象 udata 只能是 `()`）。
    pub dmabuf_buffer_serial: std::collections::HashMap<wayland_client::backend::ObjectId, u32>,
    /// `wl_buffer` 对象 → 寿命延长器（`create_immed` 路径专用，没有导入序号可用）。
    pub dmabuf_buffer_keepalive:
        std::collections::HashMap<wayland_client::backend::ObjectId, FrameKeepalive>,
    /// 已经**异步导入成功过**的 `(fourcc, modifier)`：只有它们才允许走 `create_immed`。
    ///
    /// `zwp_linux_buffer_params_v1.create_immed`（since 2）同步返回 `wl_buffer`，省掉
    /// "发 create → 等 created 事件"那次往返；代价是失败不再是 `failed` 事件，而是
    /// **协议错误**（直接断连接）。所以先用异步路径验证一次同一组格式真的能导入，
    /// 之后才敢走同步路径——见 `present_dmabuf_fitted` 的注释。
    pub dmabuf_warm: std::collections::HashSet<(u32, u64)>,
    /// global 名 → 输出条目。
    pub outputs: HashMap<u32, OutputEntry>,
    /// `zwlr_foreign_toplevel_manager_v1`（可选；niri/sway 有，GNOME/KDE 没有）。
    pub toplevel_manager: Option<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1>,
    /// 顶层窗口句柄 → 快照（key 用 `ObjectId`，句柄没有稳定序号）。
    pub toplevels: HashMap<wayland_client::backend::ObjectId, ToplevelEntry>,
    /// 句柄 → 它声称所在的 `wl_output` 对象 id（`output_enter` 给的是代理）。
    ///
    /// `output_leave` 只带同一个代理，要把它翻译成"哪个输出的名字"必须反查。
    pub toplevel_output_ids:
        HashMap<wayland_client::backend::ObjectId, Vec<wayland_client::backend::ObjectId>>,
}

impl WlState {
    /// 把 `wl_output` 的对象 id 翻译成输出名（`eDP-1`）；未知返回 `None`。
    ///
    /// 顶层窗口协议只在 `output_enter/leave` 里给代理对象，日志与 `status`
    /// 需要的是人能对上的名字，所以必须反查一次。
    pub(crate) fn output_name_of(
        &self,
        output_id: wayland_client::backend::ObjectId,
    ) -> Option<String> {
        self.outputs
            .values()
            .find(|entry| entry.output.id() == output_id)
            .map(|entry| entry.key().to_string())
    }

    /// 该 `格式 + modifier` 组合是否被合成器广播（DMA-BUF 导入的前置校验）。
    ///
    /// 校验放在客户端是必须的：`zwp_linux_buffer_params_v1.create` 在 v4+ 会对
    /// 未广播的组合发 `invalid_format` **协议错误**，直接把连接打死；绑到 v3 时
    /// 行为是 `failed` 事件，但提前判断仍能省掉一次往返并给出可读原因。
    pub(crate) fn dmabuf_supports(&self, fourcc: u32, modifier: u64) -> bool {
        self.dmabuf.is_some()
            && self
                .dmabuf_formats
                .iter()
                .find(|f| f.fourcc == fourcc)
                .is_some_and(|f| f.accepts(modifier))
    }

    /// 零拷贝是否可用（协议在位 + 至少广播过一个格式）。
    pub(crate) fn dmabuf_available(&self) -> bool {
        self.dmabuf.is_some() && !self.dmabuf_formats.is_empty()
    }

    /// 按 registry 全局名取可变条目。
    pub(crate) fn entry_mut(&mut self, global: u32) -> Option<&mut OutputEntry> {
        self.outputs.get_mut(&global)
    }

    /// 按输出键（daemon 视角的 OutputId）解析 global 名。
    pub(crate) fn find_global(&self, key: &str) -> Option<u32> {
        self.outputs
            .values()
            .find(|e| !e.dead && e.key() == key)
            .map(|e| e.global)
    }

    /// 活着的输出条目（键有序，供 outputs() 排序稳定）。
    pub(crate) fn live_entries(&self) -> Vec<(u32, &OutputEntry)> {
        let mut v: Vec<(u32, &OutputEntry)> = self
            .outputs
            .iter()
            .filter(|(_, e)| !e.dead)
            .map(|(g, e)| (*g, e))
            .collect();
        v.sort_by_key(|(g, _)| *g);
        v
    }
}

// ---------------------------------------------------------------------------
// registry
// ---------------------------------------------------------------------------

/// 后续热插/热拔的 registry 事件（`GlobalListContents` 是 wayland-client
/// globals 模块在初始收集完成后转发事件所用的 udata 类型）。
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WlState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // wayland-client globals 模块在初始 roundtrip 之后才把 registry
        // 事件转发到这里（初始 globals 已被 GlobalList 记录，backend
        // 初始化时显式遍历 bind 单例与 wl_output）。本 handler 只处理
        // **后续**热插/热拔。
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" && !state.outputs.contains_key(&name) {
                    // 新输出热插：绑定 min(版本, 4)（name/description 需要 v4）。
                    let v = version.min(4);
                    let output: wl_output::WlOutput = registry.bind(name, v, qh, OutputUdata(name));
                    state.outputs.insert(
                        name,
                        OutputEntry {
                            global: name,
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
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(entry) = state.outputs.get_mut(&name) {
                    entry.dead = true;
                    // 显式销毁已建资源（代理 Drop 不发 destructor）。
                    destroy_scale_objects(entry);
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
                    entry.pool = None; // Pool Drop 自行 destroy+munmap
                    entry.frame_pool = None;
                    if entry.output.is_alive() {
                        entry.output.release();
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 单例：compositor / shm / layer shell / shm pool（无事件）
// ---------------------------------------------------------------------------

impl Dispatch<wl_compositor::WlCompositor, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// `wl_region` 只用来给壁纸层设空输入区域，没有事件要处理。
impl Dispatch<wl_region::WlRegion, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 广播列表决定"能否零转换上传"：合成器认格式就省掉整帧像素转换。
        // 未知值（部分合成器后续用 DRM 四字符码扩展）我们不用，忽略即可。
        if let wl_shm::Event::Format {
            format: wayland_client::WEnum::Value(value),
        } = event
        {
            state.shm_formats.push(value);
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wp_viewporter::WpViewporter,
        _: wp_viewporter::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_viewport::WpViewport, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wp_viewport::WpViewport,
        _: wp_viewport::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        _: wp_fractional_scale_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, FractionalUdata> for WlState {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        udata: &FractionalUdata,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if let Some(entry) = state.outputs.get_mut(&udata.0) {
                // 分母固定 120（1.5 → 180）；0 是非法值，忽略而非采纳。
                if scale > 0 {
                    entry.fractional_factor = Some(scale);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// wl_output
// ---------------------------------------------------------------------------

impl Dispatch<wl_output::WlOutput, OutputUdata> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        udata: &OutputUdata,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(&udata.0) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                physical_width,
                physical_height,
                ..
            } => {
                entry.physical_mm = Some((physical_width, physical_height));
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                // `mode.flags` 在 wayland.xml 里是 `bitfield="true"`，多个位会同时
                // 置位（niri 实测 current|preferred = 3）。**必须测位，不能比相等**：
                // 比相等会把 3 != 1 判成"不是当前模式"，`current_mode` 永远空，
                // 上层 `resolve_mode(Preferred)` 就会报"合成器尚未报告模式"。
                use wayland_client::WEnum;
                let mode = liuguang_core::types::DisplayMode {
                    width: width as u32,
                    height: height as u32,
                    refresh_millihz: refresh as u32,
                };
                let (current, preferred) = match flags {
                    WEnum::Value(bits) => (
                        bits.contains(wl_output::Mode::Current),
                        bits.contains(wl_output::Mode::Preferred),
                    ),
                    WEnum::Unknown(_) => (false, false),
                };
                if current {
                    entry.current_mode = Some(mode);
                } else if preferred && entry.preferred_mode.is_none() {
                    entry.preferred_mode = Some(mode);
                }
                entry.modes.push(mode);
            }
            wl_output::Event::Scale { factor } => {
                entry.scale = factor;
            }
            wl_output::Event::Name { name } => {
                entry.name = Some(name);
            }
            wl_output::Event::Description { description } => {
                entry.description = Some(description);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// layer surface / surface / callback / buffer
// ---------------------------------------------------------------------------

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, LayerUdata> for WlState {
    fn event(
        state: &mut Self,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        udata: &LayerUdata,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(&udata.0) else {
            return;
        };
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                entry.pending_configure = Some((serial, width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => {
                entry.dead = true;
                destroy_scale_objects(entry);
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
                entry.frame_pool = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, SurfaceUdata> for WlState {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &SurfaceUdata,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, FrameUdata> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        udata: &FrameUdata,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // frame 回调到达 = 上一次 commit 已被合成器消费 → 解除背压。
        if let Some(entry) = state.outputs.get_mut(&udata.0) {
            entry.frame_pending = false;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferUdata> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        udata: &BufferUdata,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(entry) = state.outputs.get_mut(&udata.global) {
                let pool = if udata.frame_pool {
                    entry.frame_pool.as_mut()
                } else {
                    entry.pool.as_mut()
                };
                if let Some(pool) = pool {
                    pool.release_slot(udata.slot);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 给 backend.rs 的辅助：RefCell 借用访问器
// ---------------------------------------------------------------------------

/// `RefCell<WlState>` 的便捷别名。
pub(crate) type WlRef = RefCell<WlState>;

// ---------------------------------------------------------------------------
// zwp_linux_dmabuf_v1：零拷贝导入
// ---------------------------------------------------------------------------

impl Dispatch<ZwpLinuxDmabufV1, ()> for WlState {
    fn event(
        state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            // v1–v3 的格式广播（v4+ 改为 feedback 对象，我们只绑到 v3）。
            zwp_linux_dmabuf_v1::Event::Format { format } => {
                if !state.dmabuf_formats.iter().any(|f| f.fourcc == format) {
                    state.dmabuf_formats.push(DmabufFormat {
                        fourcc: format,
                        modifiers: Vec::new(),
                    });
                }
            }
            // v3 的「格式 + modifier」广播：与 Format 事件可能穿插到达。
            zwp_linux_dmabuf_v1::Event::Modifier {
                format,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
                match state.dmabuf_formats.iter_mut().find(|f| f.fourcc == format) {
                    Some(entry) => {
                        if !entry.modifiers.contains(&modifier) {
                            entry.modifiers.push(modifier);
                        }
                    }
                    None => state.dmabuf_formats.push(DmabufFormat {
                        fourcc: format,
                        modifiers: vec![modifier],
                    }),
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, DmabufParamsUdata> for WlState {
    fn event(
        state: &mut Self,
        _proxy: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        data: &DmabufParamsUdata,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                state.dmabuf_buffer_serial.insert(buffer.id(), data.0);
                state.dmabuf_result = Some((data.0, Ok(buffer)));
            }
            zwp_linux_buffer_params_v1::Event::Failed => {
                state.dmabuf_keepalives.remove(&data.0);
                state.dmabuf_result = Some((data.0, Err(())));
            }
            _ => {}
        }
    }

    // `created` 事件里带 new_id（wl_buffer），wayland-client 需要知道用什么 udata
    // 建这个子对象。不特化会在真机上直接 panic（"Missing event_created_child
    // specialization"）。缓冲的 udata 用 `()`：DMA-BUF 的 fd 在导入成功后即可关闭。
    wayland_client::event_created_child!(WlState, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

impl Dispatch<wl_buffer::WlBuffer, ()> for WlState {
    fn event(
        state: &mut Self,
        proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            // 合成器用完了：销毁代理。
            //
            // udata 是 `()`（`params.create` 没有 udata 参数，子对象按默认值创建），
            // 所以这里没有 fd 要释放——DMA-BUF 的 fd 在 `created` 收到后就可以关：
            // 协议规定合成器在 `create` 阶段就完成导入（自己持有 GEM/DRM 句柄），
            // 这也正是 `failed` 事件存在的原因。
            // 合成器用完了这一帧：**先**丢掉寿命延长器（供应商的解码表面归还），
            // 再销毁代理。顺序不重要，但两者必须成对——否则表面永不复用，
            // 30fps 下几秒钟就把解码器的表面池榨干。
            if let Some(serial) = state.dmabuf_buffer_serial.remove(&proxy.id()) {
                state.dmabuf_keepalives.remove(&serial);
            }
            let released_immed = state.dmabuf_buffer_keepalive.remove(&proxy.id()).is_some();
            tracing::trace!(
                buffer = ?proxy.id(),
                released_immed,
                pending_keepalives = state.dmabuf_buffer_keepalive.len(),
                "DMA-BUF 缓冲已释放"
            );
            if proxy.is_alive() {
                proxy.destroy();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 顶层窗口（zwlr_foreign_toplevel_management_unstable_v1）
// ---------------------------------------------------------------------------

/// `zwlr_foreign_toplevel_manager_v1`：只处理"新窗口"。
impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for WlState {
    // `toplevel` 事件带一个 new_id（每个窗口一个 handle），必须**在 impl 内**特化
    // 子对象类型；放在模块作用域只会变成无人调用的自由函数，真机照样 panic：
    // `Missing event_created_child specialization for event opcode 0 of ...`
    wayland_client::event_created_child!(WlState, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE =>
            (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ())
    ]);

    fn event(
        state: &mut Self,
        _proxy: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                state.toplevels.insert(toplevel.id(), ToplevelEntry::new());
                state.toplevel_output_ids.insert(toplevel.id(), Vec::new());
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                // 合成器要收摊：清空所有快照，避免 `status` 里留下幽灵窗口。
                for (id, _) in state.toplevels.drain() {
                    state.toplevel_output_ids.remove(&id);
                }
                state.toplevel_manager = None;
            }
            _ => {}
        }
    }
}

/// 单个顶层窗口：标题 / app_id / 状态位 / 所在输出 / 关闭。
impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for WlState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let id = proxy.id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(entry) = state.toplevels.get_mut(&id) {
                    entry.title = Some(title);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(entry) = state.toplevels.get_mut(&id) {
                    entry.app_id = Some(app_id);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw } => {
                // **整组替换**而不是"按出现置位"：协议语义是这个数组就是当前全部
                // 状态，窗口退出全屏时新数组里就没有 3，直接置位会导致永远"全屏中"
                // ——壁纸再也不恢复（真机最容易踩的坑）。
                let flags = parse_state_flags(&raw);
                if let Some(entry) = state.toplevels.get_mut(&id) {
                    entry.state = flags;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputEnter { output } => {
                let output_id = output.id();
                // 先按 id 查名字（`ObjectId` 不是 `Copy`，借用完再克隆进表里）。
                let output_name = state.output_name_of(output_id.clone());
                state
                    .toplevel_output_ids
                    .entry(id.clone())
                    .or_default()
                    .push(output_id);
                if let (Some(name), Some(entry)) = (output_name, state.toplevels.get_mut(&id)) {
                    if !entry.outputs.contains(&name) {
                        entry.outputs.push(name);
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output } => {
                let output_id = output.id();
                if let Some(ids) = state.toplevel_output_ids.get_mut(&id) {
                    ids.retain(|known| *known != output_id);
                }
                // 重新算一遍名字表：先收回剩余 id 的借用，再动窗口条目
                // （同时可变借用两个 map 会让借用检查器拒绝）。
                let names: Vec<String> = state
                    .toplevel_output_ids
                    .get(&id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|oid| state.output_name_of(oid))
                    .collect();
                if let Some(entry) = state.toplevels.get_mut(&id) {
                    entry.outputs = names;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                if proxy.is_alive() {
                    proxy.destroy();
                }
                state.toplevels.remove(&id);
                state.toplevel_output_ids.remove(&id);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod toplevel_tests {
    use super::*;

    /// 协议把状态打包成 little-endian u32 数组：2 = activated，3 = fullscreen。
    #[test]
    fn parses_fullscreen_flag() {
        let bytes = [2u32.to_le_bytes(), 3u32.to_le_bytes()].concat();
        let state = parse_state_flags(&bytes);
        assert!(state.activated);
        assert!(state.fullscreen);
        assert!(!state.maximized);
        assert!(!state.minimized);
    }

    #[test]
    fn parses_maximized_and_minimized() {
        let bytes = [0u32.to_le_bytes(), 1u32.to_le_bytes()].concat();
        let state = parse_state_flags(&bytes);
        assert!(state.maximized);
        assert!(state.minimized);
        assert!(!state.fullscreen);
    }

    /// 退出全屏时合成器发的是**新数组**（里面没有 3）——整组替换语义靠调用方，
    /// 这里守住解码器本身不"记住"历史。
    #[test]
    fn empty_array_clears_every_flag() {
        assert_eq!(parse_state_flags(&[]), ToplevelState::default());
    }

    /// 未知状态值（未来协议扩展）忽略即可，不能 panic、也不能当成 fullscreen。
    #[test]
    fn unknown_flags_are_ignored() {
        let bytes = [99u32.to_le_bytes(), 3u32.to_le_bytes()].concat();
        let state = parse_state_flags(&bytes);
        assert!(state.fullscreen);
        assert!(!state.maximized);
    }

    /// 尾部残片（不是 4 的倍数）不能让整个事件失败。
    #[test]
    fn trailing_partial_word_is_ignored() {
        let mut bytes = [3u32.to_le_bytes()].concat();
        bytes.push(0xff);
        let state = parse_state_flags(&bytes);
        assert!(state.fullscreen);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn fractional_scale_uses_120_denominator() {
        // niri 实测：1707x1067 逻辑、factor=180（1.5）→ 物理 2560x1600。
        let dims = scaled_dims((1707, 1067), Some(180), 2, true);
        assert_eq!(dims, (2561, 1601));
    }

    /// `DRM_FORMAT_MOD_INVALID`（隐式 modifier）永远接受；LINEAR 在只广播了
    /// 格式事件（没有 modifier 事件）的合成器上也要可用。
    #[test]
    fn dmabuf_format_accepts_implicit_and_linear() {
        const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
        let bare = DmabufFormat {
            fourcc: 0x3231_564E,
            modifiers: Vec::new(),
        };
        assert!(bare.accepts(MOD_INVALID));
        assert!(bare.accepts(0));
        // 未知的显式 modifier：没有广播过，必须拒绝。
        assert!(!bare.accepts(0x0100_0000_0000_0002));

        let tiled = DmabufFormat {
            fourcc: 0x3231_564E,
            modifiers: vec![0x0100_0000_0000_0002],
        };
        assert!(tiled.accepts(0x0100_0000_0000_0002));
        // 广播了 modifier 列表后 LINEAR 仍需显式列出，否则拒绝。
        assert!(!tiled.accepts(0));
    }

    #[test]
    fn integer_scale_used_when_no_fractional() {
        // 没有 fractional 协议时：cfg × wl_output.scale。
        let dims = scaled_dims((1280, 800), None, 2, false);
        assert_eq!(dims, (2560, 1600));
    }

    #[test]
    fn fractional_requires_viewport() {
        // 收到 factor 但没有 viewport → 大 buffer 无法映射回逻辑尺寸，必须回退。
        let dims = scaled_dims((1280, 800), Some(180), 2, false);
        assert_eq!(dims, (2560, 1600));
    }

    #[test]
    fn zero_factor_is_ignored() {
        let dims = scaled_dims((1000, 500), Some(0), 1, true);
        assert_eq!(dims, (1000, 500));
    }

    #[test]
    fn scaled_dims_never_zero() {
        // 极小逻辑尺寸 × 分数缩放也不能产出 0 像素（wl_shm 拒绝零尺寸缓冲）。
        let dims = scaled_dims((1, 1), Some(120), 1, true);
        assert_eq!(dims, (1, 1));
    }

    #[test]
    fn buffer_scale_is_one_under_fractional() {
        // 协议要求：fractional 下 buffer_scale 必须为 1（缩放交给 viewport）。
        assert_eq!(surface_buffer_scale(true, 2), 1);
        assert_eq!(surface_buffer_scale(false, 2), 2);
        assert_eq!(surface_buffer_scale(false, 0), 1);
    }
}
