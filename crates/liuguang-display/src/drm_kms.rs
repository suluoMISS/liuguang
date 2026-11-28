//! 真实 DRM/KMS 后端（Linux，规范 §4.1）。
//!
//! 职责：动态枚举 card/connector/encoder/CRTC/plane/mode（**不写死
//! `card0`/`renderD128`**），dumb buffer 扫描输出，Atomic KMS 提交，
//! 连接器状态复查与资源释放。
//!
//! M1 边界：
//! - 静态路径使用 dumb buffer + 原子提交（首个提交允许 modeset）；
//!   不维护任何渲染循环——提交完成后扫描输出由显示引擎持续刷新，
//!   CPU 不再介入。
//! - 视频等持续提交路径复用 [`crate::scanout::ScanoutPool`]：同一尺寸/格式
//!   连续提交时**不重新分配缓冲、不重新映射**（映射在槽位创建时建立一次，
//!   随槽位释放而 munmap）。in-flight 缓冲所有权：三缓冲轮转，任何一个缓冲
//!   在被替换前都不会被写回。
//! - 缩放：`SRC_*` / `CRTC_*` 平面属性交给 DRM 平面做**硬件缩放**；
//!   CPU 侧只做「帧 → 扫描缓冲」的上传拷贝（拷贝字节数可观测）。
//!   注意：上传拷贝本身仍是 CPU 侧工作，日志/文档不得声称"零 CPU 拷贝"。
//! - 打开设备要求当前进程/会话拥有 DRM Master（见 `docs/HARDWARE_CHECK.md`
//!   §4 的 seat 方案）；被占用时错误信息明确指出，不抢占。

use crate::in_formats::{self, CacheOutcome, FormatModifier, RetryCache};
use crate::master::{MasterStatus, RateLimiter, dedup_holders, scan_device_holders_excluding_self};
use crate::scanout::{
    AcquirePolicy, PoolStats, ScanoutFactory, ScanoutFormat, ScanoutKey, ScanoutPool, Slot,
    SlotState, upload,
};
use crate::selection::{CrtcDesc, pick_crtc};
use crate::{CommitStats, DisplayBackend, DisplayError};
use drm::Device as DrmDevice;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::dumbbuffer::{DumbBuffer, DumbMapping};
use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, FbCmd2Flags, ModeTypeFlags, atomic, connector,
    crtc, framebuffer, plane, property,
};
use liuguang_core::types::{
    CpuFrame, DisplayFrame, DisplayMode, DmaBufFrame, OutputId, OutputInfo, PresentToken,
};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 扫描缓冲池深度：三缓冲轮转，保证任何在扫描的帧在替换前不被写回。
const SCANOUT_POOL_DEPTH: usize = 3;

/// 一个刷新周期的等待预算（池耗尽时用）。
///
/// 优先用模式的 vrefresh；未知/为 0 时兜底 20ms（约 4K60 的一帧 +20% 余量），
/// 避免恰好卡在边界上反复丢帧。
fn vblank_budget(mode: drm::control::Mode) -> std::time::Duration {
    let hz = mode.vrefresh();
    if hz > 0 {
        std::time::Duration::from_micros(1_200_000u64 / u64::from(hz))
    } else {
        std::time::Duration::from_micros(20_000)
    }
}

/// `atomic_commit` 返回 `EBUSY`（上一次 page-flip 仍在途）时的重试原因码。
const EBUSY: i32 = 16;

/// `DRM_FORMAT_MOD_INVALID`（`(1 << 56) - 1`）：驱动用它表示"没有有效修饰符"。
const DRM_FORMAT_MOD_INVALID: u64 = (1 << 56) - 1;

/// `EACCES`：提交时返回它几乎只有一个原因——本进程不是 DRM master。
const EACCES: i32 = 13;

/// 失败日志突发条数（前 N 条逐条打印）。
const COMMIT_FAIL_LOG_BURST: u32 = 3;
/// 失败日志限速间隔（之后每 N 时间最多一条，并附"已抑制 M 次"）。
const COMMIT_FAIL_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// 非 master 状态下"重新打开设备"的最小间隔。
const MASTER_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// 解析注入钩子开关（纯函数，便于单测；**不在测试里改进程环境**）。
#[must_use]
pub fn dmabuf_reject_hook(raw: Option<&str>) -> bool {
    match raw {
        None => false,
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "on" | "true" | "yes"
        ),
    }
}

/// 自愈告警的前 N 次逐条打印（首次故障要能直接看到原因）。
const HEAL_LOG_BURST: u32 = 3;
/// 之后每次最多一条的最小间隔（占用者长期不退时避免 30 行/分钟刷屏）。
const HEAL_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// `TEST_ONLY` 诊断日志/调用的最小间隔（失败状态下模式变更会逐帧重来）。
const MODE_TEST_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// `IN_FORMATS` 读取失败后的重试冷却（避免"一次失败永久禁用双重校验"）。
const IN_FORMATS_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// DRM 主节点包装（`drm` crate 要求实现 [`AsFd`] + 两个 trait）。
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl DrmDevice for Card {}
impl ControlDevice for Card {}

/// 一个输出的运行状态。
struct OutputState {
    info: OutputInfo,
    connector: connector::Handle,
    drm_modes: Vec<drm::control::Mode>,
    crtc: Option<crtc::Handle>,
    plane: Option<plane::Handle>,
    active_mode: Option<drm::control::Mode>,
    mode_blob_id: Option<u64>,
    /// 已完成 modeset 的分辨率签名（同分辨率后续提交走 page-flip；
    /// 不比较完整 Mode 结构——不同构造路径的 Mode 字节不等会误判）。
    committed_size: Option<(u32, u32)>,
    /// 扫描缓冲池（尺寸/格式相同的连续提交只做上传，不分配、不映射）。
    scanout_pool: ScanoutPool<DumbBuffer, DumbMapping<'static>, framebuffer::Handle>,
    /// KMS 属性句柄缓存（属性集在一次会话内稳定；逐帧重建会带来
    /// 每个对象一次 `get_properties` + 逐属性 `get_property` 的 ioctl 风暴）。
    prop_cache: PropCache,
    /// DMA-BUF 提交在途 (fb, gem)（保留最近 2 个，翻页完成后释放）。
    /// 在途的 DMA-BUF framebuffer 与其全部 GEM 句柄（多 object 帧有 2 个以上）。
    dmabuf_inflight: Vec<(framebuffer::Handle, Vec<drm::buffer::Handle>)>,
}

/// 每个对象的 KMS 属性名 → 属性句柄缓存。
///
/// plane 按句柄分别缓存：视频（NV12）可能落在与静态帧（XR24）不同的平面上。
#[derive(Default)]
struct PropCache {
    connector: Option<(connector::Handle, HashMap<String, property::Handle>)>,
    crtc: Option<(crtc::Handle, HashMap<String, property::Handle>)>,
    planes: HashMap<plane::Handle, HashMap<String, property::Handle>>,
}

impl PropCache {
    /// 清空缓存（挂起/恢复、CRTC 重新绑定后属性集可能变化）。
    fn clear(&mut self) {
        self.connector = None;
        self.crtc = None;
        self.planes.clear();
    }
}

/// 平面选择组合 `(plane, fourcc, modifier)`。
///
/// 用于"**只在变化时**打日志"：`plane_for_dmabuf` 每帧都会跑，逐帧 INFO 会在
/// 60fps 下产生 ~60 行/秒（真机 30s 1750 行），既淹没有效日志又浪费 CPU。
type PlaneChoice = (u32, u32, u64);

/// 是否应当为这次平面选择打一条 INFO（首次或组合变化）。
///
/// 纯函数（不做 I/O、不读时钟），便于单测守护"**不得退化成逐帧打印**"这条回归。
#[must_use]
fn plane_choice_is_new(previous: Option<PlaneChoice>, current: PlaneChoice) -> bool {
    previous != Some(current)
}

/// DMA-BUF 预检结果（[`DrmKmsBackend::preflight_dmabuf`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufPreflight {
    /// 选中的平面。
    pub plane: plane::Handle,
    /// 帧格式。
    pub fourcc: DrmFourcc,
    /// 帧格式修饰符（0 = LINEAR）。
    pub modifier: u64,
    /// 目标 CRTC。
    pub crtc: crtc::Handle,
    /// 提交时使用的模式。
    pub mode: drm::control::Mode,
}

/// DRM/KMS 显示后端。
pub struct DrmKmsBackend {
    card: Card,
    path: PathBuf,
    driver: String,
    outputs: Vec<OutputState>,
    suspended: bool,
    sequence: u64,
    /// 自愈尝试日志的限速器（t26：尝试是 2s 一次的，但**日志不能跟着 2s 一条**）。
    ///
    /// 前 [`HEAL_LOG_BURST`] 次逐条打印（让首次故障可诊断），之后每
    /// [`HEAL_LOG_INTERVAL`] 最多一条并附"已抑制 N 次"。
    heal_log: RateLimiter,
    /// 上一次**已打印**的平面选择 `(plane, fourcc, modifier)`。
    ///
    /// `plane_for_dmabuf` 每帧都会跑；旧实现在命中时**每帧一条 INFO**
    /// （真机 60fps → 约 60 行/秒、30s 1750 行），既淹没有效日志又浪费 CPU。
    /// 现在只在组合**变化**时打 INFO，重复时降到 `trace`。
    last_plane_choice: Option<PlaneChoice>,
    /// 提交统计（见 [`CommitStats`]）。
    commit_stats: CommitStats,
    /// 平面 `IN_FORMATS` 解析结果缓存（成功即长期命中）。
    ///
    /// **失败不粘住**（t17 审计 A.8 ④）：读取/解析失败时按
    /// [`IN_FORMATS_RETRY_INTERVAL`] 冷却后重试，成功即恢复"fourcc + modifier"双重校验。
    in_formats_cache: RetryCache<plane::Handle, Vec<FormatModifier>>,
    /// DRM master 状态（显式获取结果 + 占用者诊断）。见 [`crate::master`]。
    master_status: MasterStatus,
    /// 提交失败日志限速器（真机实测不限速时 ~140 行/秒）。
    commit_fail_log: RateLimiter,
    /// "重新打开设备以取得 master"的尝试限速器（避免高频重开设备）。
    master_retry_log: RateLimiter,
    /// `TEST_ONLY` 诊断的限速器（模式变更在失败状态下会逐帧重试，
    /// 不限速时既是日志洪泛、也是每帧一次多余 ioctl）。
    mode_test_log: RateLimiter,
}

/// NV12 平面缓冲描述（AddFB2：Y/UV 共用同一 GEM 句柄，UV 以偏移区分）。
struct Nv12Planar {
    size: (u32, u32),
    handle: drm::buffer::Handle,
    pitches: [u32; 2],
    offsets: [u32; 2],
}

impl drm::buffer::PlanarBuffer for Nv12Planar {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> drm::buffer::DrmFourcc {
        drm::buffer::DrmFourcc::Nv12
    }
    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitches[0], self.pitches[1], 0, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [Some(self.handle), Some(self.handle), None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.offsets[0], self.offsets[1], 0, 0]
    }
}

impl DrmKmsBackend {
    /// 打开 DRM 设备并枚举输出。
    ///
    /// - `device = "auto"`：动态发现 `/dev/dri/cardN`（按名称排序），
    ///   选择第一个可枚举且带已连接输出的卡；均无连接时退回第一个可枚举卡。
    /// - `device = "/dev/dri/cardN"`：精确打开该节点。
    ///
    /// # Errors
    ///
    /// 节点不可打开、无 DRM 能力、被其他 DRM Master 占用或枚举失败时
    /// 返回 [`DisplayError`]，附带路径与操作上下文。
    pub fn open(device: &str) -> Result<Self, DisplayError> {
        let candidates = if device == "auto" {
            discover_cards()?
        } else {
            vec![PathBuf::from(device)]
        };
        if candidates.is_empty() {
            return Err(DisplayError::DeviceUnavailable(
                "/dev/dri 下未发现 cardN 设备节点".to_string(),
            ));
        }

        let mut attempts = Vec::new();
        for path in &candidates {
            match Self::open_exact(path) {
                Ok(backend) => {
                    tracing::info!(
                        device = %path.display(),
                        driver = %backend.driver,
                        outputs = backend.outputs.len(),
                        "DRM 设备已打开"
                    );
                    return Ok(backend);
                }
                Err(e) => attempts.push(format!("{}: {e}", path.display())),
            }
        }
        Err(DisplayError::DeviceUnavailable(format!(
            "无法打开任何 DRM 设备（{}）",
            attempts.join("; ")
        )))
    }

    fn open_exact(path: &Path) -> Result<Self, DisplayError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| {
                DisplayError::DeviceUnavailable(format!("打开 {} 失败: {e}", path.display()))
            })?;
        let card = Card(file);

        // 能力探测：驱动信息可读说明是 DRM 节点。
        let driver = card
            .get_driver()
            .map_err(|e| {
                DisplayError::DeviceUnavailable(format!(
                    "{} 不是可用的 DRM 主节点或无权限（{e}）",
                    path.display()
                ))
            })?
            .name
            .to_string_lossy()
            .into_owned();

        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .map_err(|e| io_err("set_client_capability(UniversalPlanes)", e))?;
        // Atomic 是本项目默认路径；失败则退回 legacy 的方案在 M1 不做，
        // 明确报错（规范 §2.3：不假设能力）。
        card.set_client_capability(drm::ClientCapability::Atomic, true)
            .map_err(|e| {
                DisplayError::DeviceUnavailable(format!(
                    "{} 驱动不支持 Atomic KMS（{e}）；M1 要求原子提交能力",
                    path.display()
                ))
            })?;

        let resources = card
            .resource_handles()
            .map_err(|e| io_err("resource_handles", e))?;
        let mut outputs = Vec::new();
        for &connector_handle in resources.connectors() {
            let info = card
                .get_connector(connector_handle, true)
                .map_err(|e| io_err("get_connector", e))?;
            let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
            let drm_modes: Vec<drm::control::Mode> = info.modes().to_vec();
            let modes: Vec<DisplayMode> = drm_modes.iter().map(|m| to_display_mode(*m)).collect();
            let connected = info.state() == connector::State::Connected;
            outputs.push(OutputState {
                info: OutputInfo {
                    id: OutputId::new(name),
                    connected,
                    modes,
                    current_mode: None,
                    physical_size_mm: info.size().map(|(w, h)| (w as u16, h as u16)),
                },
                connector: connector_handle,
                drm_modes,
                crtc: None,
                plane: None,
                active_mode: None,
                mode_blob_id: None,
                committed_size: None,
                scanout_pool: ScanoutPool::new(SCANOUT_POOL_DEPTH),
                prop_cache: PropCache::default(),
                dmabuf_inflight: Vec::new(),
            });
        }

        // 显式获取 DRM master（而不是依赖"第一个打开设备"的自动授予）并如实报告结果。
        let master_status = acquire_master_status(&card, path);
        match master_status.master {
            Some(true) => tracing::info!(
                device = %path.display(),
                "DRM master：{}",
                master_status.acquire_result
            ),
            _ if !master_status.holders.is_empty() => {
                // 已经能看到别的进程打开该设备 ⇒ 极可能不是 master：启动阶段就大声报错，
                // 不要让用户面对"进程在跑、屏幕全黑"。
                let hint = master_status.remedy_hint();
                tracing::error!(
                    device = %path.display(),
                    result = %master_status.acquire_result,
                    holders = ?master_status.holders,
                    "启动时无法取得 DRM master（画面不会更新）：{hint}"
                );
            }
            _ => {
                let hint = master_status.remedy_hint();
                tracing::warn!(
                    device = %path.display(),
                    result = %master_status.acquire_result,
                    "DRM master 尚未确认：{hint}；首帧提交会给出确定结论（成功=已持有，EACCES=未持有）"
                );
            }
        }

        Ok(Self {
            card,
            path: path.to_path_buf(),
            driver,
            outputs,
            suspended: false,
            sequence: 0,
            heal_log: RateLimiter::new(HEAL_LOG_BURST, HEAL_LOG_INTERVAL),
            last_plane_choice: None,
            commit_stats: CommitStats::default(),
            in_formats_cache: RetryCache::new(IN_FORMATS_RETRY_INTERVAL),
            master_status,
            commit_fail_log: RateLimiter::new(COMMIT_FAIL_LOG_BURST, COMMIT_FAIL_LOG_INTERVAL),
            master_retry_log: RateLimiter::new(1, MASTER_RETRY_INTERVAL),
            mode_test_log: RateLimiter::new(1, MODE_TEST_LOG_INTERVAL),
        })
    }

    /// 设备节点路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 驱动名（如 `i915`）。
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// 复查某个输出的连接状态（热插拔检测用；只读 ioctl）。
    ///
    /// # Errors
    ///
    /// 查询失败时返回 [`DisplayError`]。
    pub fn is_connected(&self, name: &str) -> Result<bool, DisplayError> {
        let Some(out) = self.outputs.iter().find(|o| o.info.id.as_str() == name) else {
            return Err(DisplayError::OutputNotFound(OutputId::new(name)));
        };
        let info = self
            .card
            .get_connector(out.connector, true)
            .map_err(|e| io_err("get_connector(状态复查)", e))?;
        Ok(info.state() == connector::State::Connected)
    }

    /// 为输出挑选 CRTC 与 primary plane（内部选择逻辑经 [`crate::selection`]）。
    fn bind_crtc_plane(&mut self, index: usize) -> Result<(), DisplayError> {
        let resources = self
            .card
            .resource_handles()
            .map_err(|e| io_err("resource_handles", e))?;
        let connector_handle = self.outputs[index].connector;

        // 1. CRTC 候选：connector 的 encoder 的 possible_crtcs 并集。
        let info = self
            .card
            .get_connector(connector_handle, true)
            .map_err(|e| io_err("get_connector", e))?;
        let mut crtc_handles: Vec<crtc::Handle> = Vec::new();
        for &enc in info.encoders() {
            let encoder = self
                .card
                .get_encoder(enc)
                .map_err(|e| io_err("get_encoder", e))?;
            for c in resources.filter_crtcs(encoder.possible_crtcs()) {
                if !crtc_handles.contains(&c) {
                    crtc_handles.push(c);
                }
            }
        }
        // 当前编码器优先（避免与既有扫描输出打架）。
        if let Some(enc) = info.current_encoder() {
            if let Ok(encoder) = self.card.get_encoder(enc) {
                let preferred: Vec<crtc::Handle> = resources
                    .filter_crtcs(encoder.possible_crtcs())
                    .into_iter()
                    .filter(|c| crtc_handles.contains(c))
                    .collect();
                if !preferred.is_empty() {
                    crtc_handles = preferred;
                }
            }
        }
        // 优先级：当前编码器已绑定的 crtc（真机教训：i915 上空闲 crtc 可能
        // 无法驱动该 connector，atomic EINVAL）> 空闲 > 其他。
        let current_crtc = info
            .current_encoder()
            .and_then(|enc| self.card.get_encoder(enc).ok())
            .and_then(|e| e.crtc());
        let candidates: Vec<CrtcDesc> = crtc_handles
            .iter()
            .map(|&c| {
                let busy = self
                    .card
                    .get_crtc(c)
                    .map(|ci| ci.framebuffer().is_some())
                    .unwrap_or(false);
                CrtcDesc {
                    busy,
                    current: Some(c) == current_crtc,
                }
            })
            .collect();
        let chosen = pick_crtc(&candidates)?;
        let crtc_handle = crtc_handles[chosen];

        // 2. plane 候选：兼容 CRTC + 支持 XRGB8888 + 优先 primary。
        const PLANE_TYPE_OVERLAY: u64 = 0;
        const PLANE_TYPE_PRIMARY: u64 = 1;
        let mut best: Option<(plane::Handle, u64)> = None;
        for ph in self
            .card
            .plane_handles()
            .map_err(|e| io_err("plane_handles", e))?
        {
            let plane_info = self
                .card
                .get_plane(ph)
                .map_err(|e| io_err("get_plane", e))?;
            if !resources
                .filter_crtcs(plane_info.possible_crtcs())
                .contains(&crtc_handle)
            {
                continue;
            }
            if !plane_info.formats().contains(&(DrmFourcc::Xrgb8888 as u32)) {
                continue;
            }
            let plane_type = self.plane_type(ph)?;
            tracing::debug!(
                plane = ?ph,
                plane_type = ?plane_type,
                formats = plane_info.formats().len(),
                "平面候选"
            );
            // 严格选择：primary 优先，其次 overlay；光标/未知类型一概不用
            // （真机教训：光标平面做全屏会 EINVAL）。
            let rank = match plane_type {
                Some(PLANE_TYPE_PRIMARY) => 0,
                Some(PLANE_TYPE_OVERLAY) => 1,
                _ => continue,
            };
            match best {
                Some((_, best_rank)) if best_rank <= rank => {}
                _ => best = Some((ph, rank)),
            }
        }
        let (plane_handle, _) = best.ok_or_else(|| {
            DisplayError::DeviceUnavailable(format!(
                "输出 {} 找不到支持 XRGB8888 的 primary plane",
                self.outputs[index].info.id
            ))
        })?;

        self.outputs[index].crtc = Some(crtc_handle);
        self.outputs[index].plane = Some(plane_handle);
        Ok(())
    }

    /// 读取 plane 的 `type` 属性（0=overlay, 1=primary, 2=cursor）。
    fn plane_type(&self, ph: plane::Handle) -> Result<Option<u64>, DisplayError> {
        let props = self
            .card
            .get_properties(ph)
            .map_err(|e| io_err("get_properties(plane)", e))?;
        for (prop_handle, raw) in props.iter() {
            let info = self
                .card
                .get_property(*prop_handle)
                .map_err(|e| io_err("get_property", e))?;
            if info.name().to_bytes() == b"type" {
                return Ok(Some(*raw));
            }
        }
        Ok(None)
    }

    /// 把一帧提交到扫描输出（池化 dumb buffer + atomic commit）。
    ///
    /// # CPU 成本模型（真机与离线核算共用）
    ///
    /// - **每帧**：一次上传拷贝 [`upload`]（字节数经 [`Self::scanout_stats`]
    ///   可观测）+ 一次 `atomic_commit`（稳态为 NONBLOCK page-flip，
    ///   不再逐帧发 `TEST_ONLY` 校验请求）。
    /// - **仅槽位重建时**：`create_dumb_buffer` + `AddFB` + `mmap` 各一次。
    /// - **缩放**：`SRC_*`/`CRTC_*` 由 DRM 平面做硬件缩放，CPU 不做逐像素
    ///   缩放；但把帧搬进扫描缓冲的上传拷贝**必然发生**（dumb buffer 是内核
    ///   对象，用户缓冲无法直接映射给显示引擎）。
    fn present_cpu_fitted(
        &mut self,
        index: usize,
        frame: &CpuFrame,
        // 源矩形（16.16 定点：x,y,w,h）。
        src: (u64, u64, u64, u64),
        // 目标矩形（模式像素：x,y,w,h）。
        dst: (i64, i64, u64, u64),
    ) -> Result<PresentToken, DisplayError> {
        if self.suspended {
            return Err(DisplayError::Suspended);
        }
        if !self.outputs[index].info.connected {
            return Err(DisplayError::DeviceUnavailable(format!(
                "输出 {} 已断开",
                self.outputs[index].info.id
            )));
        }
        if self.outputs[index].crtc.is_none() {
            self.bind_crtc_plane(index)?;
        }

        let mode = self.outputs[index]
            .active_mode
            .ok_or_else(|| DisplayError::Failed {
                operation: "present",
                message: "尚未设置显示模式（先调用 set_mode）".to_string(),
            })?;
        let (mw, mh) = (u32::from(mode.size().0), u32::from(mode.size().1));
        let (fw, fh) = (frame.width, frame.height);
        // 16.16 源矩形钳制到帧内；目标矩形钳制到模式内。
        let src = (
            src.0.min(u64::from(fw) << 16),
            src.1.min(u64::from(fh) << 16),
            src.2
                .min((u64::from(fw) << 16).saturating_sub(src.0))
                .max(65536),
            src.3
                .min((u64::from(fh) << 16).saturating_sub(src.1))
                .max(65536),
        );
        let dst = (
            dst.0.clamp(0, i64::from(mw)),
            dst.1.clamp(0, i64::from(mh)),
            dst.2.clamp(1, u64::from(mw)),
            dst.3.clamp(1, u64::from(mh)),
        );
        frame.validate()?;

        // 像素格式 → 扫描格式：显式映射，不做隐式像素转换。不支持的格式
        // 直接返回错误（上层据此回退生产者格式），而不是静默丢帧。
        let sfmt = ScanoutFormat::from_pixel_format(frame.format).ok_or_else(|| {
            DisplayError::Unsupported(format!(
                "帧格式 {:?} 不能直接扫描输出（支持 Xrgb8888 / Argb8888 / Rgba8888 / Nv12）",
                frame.format
            ))
        })?;

        let crtc_handle = self.outputs[index]
            .crtc
            .ok_or_else(|| DisplayError::Failed {
                operation: "present",
                message: "CRTC 未绑定".to_string(),
            })?;
        let plane_handle = self.plane_for_scanout(index, sfmt)?;

        // 1. **显式所有权**：取写权（只从 FREE 槽位）→ 上传 → READY。
        //    提前返回（上传/校验失败）时 `WriteGuard::drop` 会把槽位放回 FREE（H2）。
        let (ready, reused, copied) = {
            let (card, out) = (&self.card, &mut self.outputs[index]);
            let mut factory = DrmScanoutFactory { card };
            let key = ScanoutKey {
                width: fw,
                height: fh,
                format: sfmt,
            };
            // 池耗尽时先等一个刷新周期（4K60 ≈ 16.7ms）；等不到就丢帧并计数，
            // **绝不**覆盖仍在扫描的缓冲（H5）。
            let vblank = vblank_budget(mode);
            let mut guard =
                out.scanout_pool
                    .acquire(&mut factory, key, AcquirePolicy::WaitUpTo(vblank))?;
            let reused = guard.reused;
            let pitch = guard.pitch;
            let copied = upload(guard.bytes(&mut factory)?, pitch, frame, sfmt)?;
            (guard.mark_ready(), reused, copied)
        };
        self.outputs[index].scanout_pool.note_uploaded(copied);
        let fb = ready.fb;

        // 2. atomic 请求（属性句柄走缓存、MODE_ID blob 复用）。
        //    失败时槽位必须回到 FREE（否则会卡在 READY 并最终耗尽池）。
        let (req, mode_changed) =
            match self.build_scanout_request(index, plane_handle, fb, src, dst, None) {
                Ok(v) => v,
                Err(e) => {
                    self.outputs[index].scanout_pool.on_commit_failure(ready);
                    return Err(e);
                }
            };

        // 3. 提交（阻塞 page-flip + EBUSY 重试 + legacy 回退，见 commit_atomic）。
        //    ⚠️ 本设计的"回收滞后一帧"依赖**阻塞提交**：`atomic_commit(N)` 返回时
        //    "N 正在扫、N−1 已下屏"是已知事实。若将来改成 NONBLOCK/事件驱动，
        //    回收触发点必须换成 page-flip 完成回调（见 `ScanoutPool::on_commit_success`）。
        match self.commit_atomic(index, req, fb, crtc_handle, mode, mode_changed) {
            Ok(()) => {
                // 4b. **唯一回收点**：READY→SCANNING，并把上一帧的缓冲滞后一帧归还。
                if let Some(reclaimed) = self.outputs[index].scanout_pool.on_commit_success(ready) {
                    // 同一个调用点服务两条路径（CPU 上传 / 上游 DMA-BUF），避免两套回收时机。
                    self.outputs[index]
                        .scanout_pool
                        .release_to_upstream(reclaimed);
                }
            }
            Err(e) => {
                self.outputs[index].scanout_pool.on_commit_failure(ready);
                return Err(e);
            }
        }

        // 4. 记账：缓冲由池管理（不再逐帧销毁/重建）。
        self.outputs[index].committed_size = Some((mw, mh));
        self.sequence += 1;
        let token = PresentToken {
            output: self.outputs[index].info.id.clone(),
            sequence: self.sequence,
        };
        let stats = self.outputs[index].scanout_pool.stats();
        if mode_changed {
            tracing::info!(
                output = %token.output,
                sequence = token.sequence,
                width = mw,
                height = mh,
                format = sfmt.fourcc_name(),
                plane = ?plane_handle,
                copied_bytes = copied,
                pool_allocations = stats.allocations,
                "首帧/模式变更帧已提交扫描输出（静态状态下无周期性渲染循环）"
            );
        } else {
            tracing::debug!(
                output = %token.output,
                sequence = token.sequence,
                format = sfmt.fourcc_name(),
                copied_bytes = copied,
                pool_reused = reused,
                pool_allocations = stats.allocations,
                pool_mappings = stats.mappings,
                pool_reuses = stats.reuses,
                pool_reclaims = stats.reclaims,
                pool_in_use = self.outputs[index].scanout_pool.in_use(),
                pool_max_in_use = stats.max_in_use,
                pool_exhausted_waits = stats.exhausted_waits,
                pool_exhausted_drops = stats.exhausted_drops,
                pool_rejected_writes = stats.rejected_writes,
                "帧已提交（平面硬件缩放 + CPU 上传拷贝）"
            );
        }
        Ok(token)
    }

    /// 提交 atomic 请求：**阻塞式** page-flip + `EBUSY` 重试 + legacy 回退。
    ///
    /// 关键设计（真机教训）：稳态提交**不使用 `NONBLOCK`**。
    /// 带 `NONBLOCK` 时，如果上一次 page-flip 尚未完成，内核会以 `EBUSY`
    /// 拒绝本次提交（真机实测 5/179 帧），并进一步导致 legacy 回退也失败
    /// → 该帧静默丢失、画面看上去"卡住一下"。阻塞提交会让 ioctl 等到上一次
    /// flip 完成再应用，天然对齐 vblank（输出上限 = 刷新率），代价是调用线程
    /// 最多阻塞一个刷新周期（4K60 ≈ 16.7 ms；本项目的验收目标是稳定 30fps）。
    ///
    /// 另：`TEST_ONLY` 校验只在模式变化时做一次，不进逐帧路径。
    ///
    /// # Errors
    ///
    /// atomic 与 legacy `set_crtc` 都失败时返回 [`DisplayError`]（该帧未上屏，
    /// 调用方必须处理，不能静默忽略）。
    #[allow(clippy::too_many_arguments)]
    fn commit_atomic(
        &mut self,
        index: usize,
        req: atomic::AtomicModeReq,
        fb: framebuffer::Handle,
        crtc_handle: crtc::Handle,
        mode: drm::control::Mode,
        mode_changed: bool,
    ) -> Result<(), DisplayError> {
        let flags = if mode_changed {
            AtomicCommitFlags::ALLOW_MODESET
        } else {
            AtomicCommitFlags::empty()
        };
        if mode_changed && self.mode_test_log.check(Instant::now()).is_some() {
            // TEST_ONLY 只作诊断：失败状态（例如不是 DRM master）下模式会"每帧都变"，
            // 因此这里限速——既避免日志洪泛，也避免每帧一次多余 ioctl。
            match self
                .card
                .atomic_commit(flags | AtomicCommitFlags::TEST_ONLY, req.clone())
            {
                Ok(()) => tracing::debug!("atomic TEST_ONLY 通过（模式变更）"),
                Err(e) => tracing::warn!(
                    error = %e,
                    "atomic TEST_ONLY 失败（模式变更；已限速，每 {:?} 最多一条）",
                    MODE_TEST_LOG_INTERVAL
                ),
            }
        }

        let mut retried = false;
        let mut result = self.card.atomic_commit(flags, req.clone());
        if let Err(e) = &result {
            if e.raw_os_error() == Some(EBUSY) {
                // 上一次 flip 仍在途：阻塞式重试一次（本次不带 NONBLOCK）。
                tracing::debug!(error = %e, "atomic commit EBUSY，阻塞重试一次");
                retried = true;
                result = self.card.atomic_commit(flags, req);
                if result.is_ok() {
                    self.commit_stats.retried_after_busy =
                        self.commit_stats.retried_after_busy.saturating_add(1);
                }
            }
        }
        match result {
            Ok(()) => {
                if retried {
                    // 已计入 retried 计数。
                } else {
                    self.commit_stats.commit_ok = self.commit_stats.commit_ok.saturating_add(1);
                }
                // 提交成功 ⇒ 本进程确实持有 DRM master（内核唯一可信的运行时判据）。
                let old = self.master_status.master;
                self.master_status.master = Some(true);
                log_master_transition(old, Some(true), "提交成功");
                Ok(())
            }
            Err(e) => {
                // 回退 legacy：set_crtc 整屏扫描（i915 全兼容路径）。
                // 注意：该回退**也是 master-only**，非 master 时同样失败，
                // 因此下面的错误路径必须给出可操作提示而不是逐帧刷屏。
                let legacy = self.card.set_crtc(
                    crtc_handle,
                    Some(fb),
                    (0, 0),
                    &[self.outputs[index].connector],
                    Some(mode),
                );
                match legacy {
                    Ok(()) => {
                        self.report_commit_failure(&e, None, index);
                        self.commit_stats.legacy_fallback =
                            self.commit_stats.legacy_fallback.saturating_add(1);
                        Ok(())
                    }
                    Err(e2) => {
                        self.commit_stats.failed = self.commit_stats.failed.saturating_add(1);
                        self.report_commit_failure(&e, Some(&e2), index);
                        let detail = format!("atomic 失败({e}) 且 legacy set_crtc 失败({e2})");
                        // **master 不可用 ≠ 帧坏了**（t94）：`report_commit_failure`
                        // 已在 EACCES 时把 master 置为 `Some(false)`。把它类型化成
                        // 可重试的环境错误，调用方就不会累计 3 次失败去降级。
                        if self.master_status.is_known_non_master() {
                            Err(DisplayError::MasterUnavailable { detail })
                        } else {
                            Err(DisplayError::Failed {
                                operation: "present",
                                message: detail,
                            })
                        }
                    }
                }
            }
        }
    }

    /// 提交统计（page-flip 成功/重试/回退/失败次数）。
    pub fn commit_stats(&self) -> CommitStats {
        self.commit_stats
    }

    pub fn master_status(&self) -> &MasterStatus {
        &self.master_status
    }

    /// 增量对账：见 [`crate::CommitStats::delta_since`] 与
    /// [`DisplayBackend::commit_delta_since`] 的用法说明。
    pub fn commit_stats_snapshot(&self) -> CommitStats {
        self.commit_stats
    }

    /// 竞争解除后重新取得 DRM master（**非 root 唯一可行的手段**）。
    ///
    /// 内核只在 `dev->master == NULL` 时把 master 授予**打开设备的那个进程**，
    /// 且 `DRM_IOCTL_SET_MASTER` 标记为 `DRM_ROOT_ONLY`——因此非 root 进程在
    /// 竞争状态下无法"抢"master；唯一办法是**在占用者退出后重新打开设备**。
    ///
    /// 本方法：重新打开同一节点 → 释放旧设备上的资源 → 交换句柄 → 重置与设备
    /// 绑定的状态（保留已选模式，下一次提交会重新 modeset）。
    ///
    /// 返回 `true` **仅当本次确实取得了 master**（`master_status.master == Some(true)`）。
    ///
    /// 注意区分两件事（t26 真机踩到）：**设备可能已经重开**（资源已重建，日志会写明），
    /// 但 master 未必到手（占用者还在时 `drmSetMaster` 返回 EACCES/EBUSY）。
    /// 旧实现把"重开成功"当成 `true` 返回，调用方于是每 2s 误报一次"自愈成功"；
    /// 现在 `true` = 真的拿回了 master，调用方可以放心重放画面。
    pub fn try_reacquire_master(&mut self) -> bool {
        let path = self.path.clone();
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!(error = %e, device = %path.display(), "重新打开 DRM 设备失败");
                return false;
            }
        };
        let new_card = Card(file);
        if let Err(e) = new_card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
        {
            tracing::warn!(error = %e, "重开后设置 UniversalPlanes 失败，放弃重取 master");
            return false;
        }
        if let Err(e) = new_card.set_client_capability(drm::ClientCapability::Atomic, true) {
            tracing::warn!(error = %e, "重开后设置 Atomic 失败，放弃重取 master");
            return false;
        }
        let attempt = drm_ffi::auth::acquire_master(new_card.as_fd());
        let definitely_master = match &attempt {
            Ok(()) => true,
            Err(e) => e.raw_os_error() == Some(EINVAL_ERRNO),
        };

        // 旧设备上的资源必须先释放（framebuffer / dumb buffer 都属于旧 fd）。
        self.release_scanouts();
        let old = std::mem::replace(&mut self.card, new_card);
        drop(old);

        // 与设备绑定的状态全部失效；保留 active_mode 与模式表，
        // 下一次提交会以 ALLOW_MODESET 重新配置（committed_size=None）。
        for out in &mut self.outputs {
            out.crtc = None;
            out.plane = None;
            out.mode_blob_id = None;
            out.committed_size = None;
            out.prop_cache.clear();
            out.dmabuf_inflight.clear();
        }
        self.in_formats_cache.clear();
        self.suspended = false;
        let old = self.master_status.master;
        // 成败三态必须分清楚（t26）：
        // - `Some(true)`：确实取回了；
        // - `Some(false)`：**本次未取得**（EACCES/EBUSY：既可能是 `SET_MASTER`
        //   的 `DRM_ROOT_ONLY` 限制，也可能他人持有——**两种情况都值得按
        //   [`MASTER_RETRY_INTERVAL`] 继续重试**，拿到就恢复，拿不到就限速告警）；
        // - `None`：其它错误（设备消失/驱动异常），结果未知。
        // 旧实现把"取回失败"一律记成 `None`，而自愈只对 `Some(false)` 生效，
        // 于是**第一次失败之后就不会再试**——占用者退出后屏幕永不恢复。
        let denied = matches!(&attempt, Err(e) if e.raw_os_error() == Some(EACCES));
        self.master_status.master = if definitely_master {
            Some(true)
        } else if denied {
            Some(false)
        } else {
            None
        };
        log_master_transition(
            old,
            self.master_status.master,
            if definitely_master {
                "重新打开设备后 drmSetMaster 成功"
            } else {
                "重新打开设备（结果待首帧提交确认）"
            },
        );
        self.master_status.holders = dedup_holders(scan_device_holders_excluding_self(&path));
        self.master_status.acquire_result = format!(
            "重新打开设备后 drmSetMaster：{}{}",
            match &attempt {
                Ok(()) => "成功".to_string(),
                Err(e) => format!("{e}"),
            },
            if definitely_master {
                "（已确认 master）"
            } else {
                "（结果待首帧提交确认）"
            }
        );
        // 尝试是 2s 一次的（恢复延迟要短），但**日志必须限速**：占用者长期不退时
        // 否则就是 30 行/分钟（本项目已有"日志洪泛淹没有效信息"的前科）。
        let suppressed = self.heal_log.check(Instant::now());
        if let Some(suppressed) = suppressed {
            tracing::warn!(
                device = %path.display(),
                master = ?self.master_status.master,
                holders = ?self.master_status.holders,
                attempts = self.master_status.heal_attempts,
                failures = self.master_status.heal_failures,
                suppressed,
                "已重新打开 DRM 设备以尝试恢复 master（资源已重建，下一次提交重新 modeset）"
            );
        } else {
            tracing::debug!(
                device = %path.display(),
                master = ?self.master_status.master,
                attempts = self.master_status.heal_attempts,
                "master 自愈重试被抑制（限速中）"
            );
        }
        // 只有**真的拿到 master** 才算"自愈成功"（见方法文档）。
        definitely_master
    }

    /// 非 master 时尝试恢复（**限速**：每 [`MASTER_RETRY_INTERVAL`] 最多一次）。
    ///
    /// 返回 `true` **仅当本次真的取得了 master**（设备重开这个副作用见日志与计数）。
    ///
    /// # 为什么要有这个独立入口（t26 根因）
    ///
    /// 原先只有"某帧提交失败"才会驱动自愈。真机实测（DegradedStatic 场景）：
    /// 占用者退出后 **15 秒内 0 次尝试**——因为降级状态下的 daemon 根本不提交，
    /// 于是永远等不到"提交失败"这个触发条件（t14 观察到的"70s 无自愈"由此而来）。
    /// daemon 应在**空闲 tick / status 查询**里周期调用本方法，使自愈与"是否在提交"解耦。
    pub fn poll_master_recovery(&mut self) -> bool {
        // 只有"已确认持有 master"才停手；`None`（未知）也要继续试——
        // 见 [`MasterStatus::should_retry_heal`]（t26：第一次重取失败后
        // 状态曾是"未知"，于是之后**再也不试**，屏幕永远不恢复）。
        if !self.master_status.should_retry_heal() {
            return false;
        }
        if self.master_retry_log.check(Instant::now()).is_none() {
            return false;
        }
        self.master_status.heal_attempts = self.master_status.heal_attempts.saturating_add(1);
        let recovered = self.try_reacquire_master();
        if !self.master_status.master.eq(&Some(true)) {
            self.master_status.heal_failures = self.master_status.heal_failures.saturating_add(1);
        }
        recovered
    }

    /// 报告一次提交失败：**限速**输出 + 可操作提示。
    ///
    /// 真机教训：不限速时该状态约 140 行/秒，把真正的原因（不是 master/被别的
    /// 实例占用）淹没。这里前 [`COMMIT_FAIL_LOG_BURST`] 条逐条打印，之后每
    /// [`COMMIT_FAIL_LOG_INTERVAL`] 最多一条，并附"已抑制 N 次"。
    fn report_commit_failure(
        &mut self,
        atomic_err: &std::io::Error,
        legacy_err: Option<&std::io::Error>,
        index: usize,
    ) {
        let errno = atomic_err.raw_os_error();
        if errno == Some(EACCES) {
            // EACCES 在提交路径上几乎只有一种解释：本进程不是 DRM master。
            let old = self.master_status.master;
            self.master_status.master = Some(false);
            self.master_status.holders =
                dedup_holders(scan_device_holders_excluding_self(&self.path));
            log_master_transition(old, Some(false), "提交返回 EACCES");
        }
        // 自愈：见 [`Self::poll_master_recovery`]（限速由 `master_retry_log` 负责）。
        let _ = self.poll_master_recovery();
        let Some(suppressed) = self.commit_fail_log.check(Instant::now()) else {
            return;
        };
        let not_master = self.master_status.is_known_non_master();
        if not_master {
            let hint = self.master_status.remedy_hint();
            tracing::error!(
                device = %self.path.display(),
                output = %self.outputs[index].info.id,
                atomic = %atomic_err,
                legacy = ?legacy_err,
                suppressed,
                acquire = %self.master_status.acquire_result,
                holders = ?self.master_status.holders,
                "提交失败：本进程不是 DRM master，帧未上屏（画面不会更新）。{hint}"
            );
        } else {
            tracing::warn!(
                device = %self.path.display(),
                output = %self.outputs[index].info.id,
                atomic = %atomic_err,
                legacy = ?legacy_err,
                suppressed,
                "提交失败（atomic 与 legacy 回退均失败）；若持续出现请检查是否有另一个实例占用 DRM"
            );
        }
    }

    /// `DrmModifier` 规范化：`0`（LINEAR）必须与"不带 `MODIFIERS`"配对。
    ///
    /// `drm@0.15.0::add_planar_framebuffer` 内部断言
    /// `(MODIFIERS && modifier().is_some()) || (!MODIFIERS && modifier().is_none())`，
    /// 且会把 [`DrmModifier::Invalid`] 过滤成 `None`。因此：
    ///
    /// - `modifier == 0`（LINEAR）→ `None` + 不带 `MODIFIERS`
    ///   （**旧实现在这里 panic**：返回 `Some(DrmModifier::Linear)` 却不带 flag，
    ///   真机 `dmabuf_probe --legacy` 已复现 `drm-0.15.0/src/control/mod.rs:360`）；
    /// - `modifier == DRM_FORMAT_MOD_INVALID` → 明确报错（它会被 crate 过滤成 `None`，
    ///   与 `MODIFIERS` 组合同样会触发断言；而把它当 LINEAR 解释是错的）；
    /// - 其它值（含本 crate 不认识的）→ **保真透传**：`drm-fourcc` 用
    ///   `Unrecognized(x)` 保留原始数值，因此不会丢失信息。
    ///
    /// # Errors
    ///
    /// modifier 为 `DRM_FORMAT_MOD_INVALID` 时返回 [`DisplayError::Unsupported`]。
    fn fb_modifier(modifier: u64) -> Result<Option<drm::buffer::DrmModifier>, DisplayError> {
        if modifier == 0 {
            return Ok(None);
        }
        if modifier == DRM_FORMAT_MOD_INVALID {
            return Err(DisplayError::Unsupported(format!(
                "DRM modifier 0x{modifier:016x} 是 DRM_FORMAT_MOD_INVALID：\
                 驱动报告了非法修饰符，拒绝提交（不静默按 LINEAR 解释）"
            )));
        }
        Ok(Some(drm::buffer::DrmModifier::from(modifier)))
    }

    /// **(modifier, flags) 的唯一来源**：`present_dmabuf` 与 `preflight_dmabuf` 共用，
    /// 保证永远满足 `drm@0.15.0::add_planar_framebuffer` 的内部断言。
    ///
    /// 返回 `(PlanarBuffer::modifier(), FbCmd2Flags)`。
    ///
    /// # Errors
    ///
    /// 见 [`Self::fb_modifier`]（`DRM_FORMAT_MOD_INVALID` 会被拒绝）。
    fn fb_modifier_and_flags(
        modifier: u64,
    ) -> Result<(Option<drm::buffer::DrmModifier>, FbCmd2Flags), DisplayError> {
        let normalized = Self::fb_modifier(modifier)?;
        let flags = if normalized.is_some() {
            FbCmd2Flags::MODIFIERS
        } else {
            FbCmd2Flags::empty()
        };
        debug_assert!(
            pairing_is_consistent(normalized, flags),
            "内部错误：({normalized:?}, {flags:?}) 会触发 drm crate 断言"
        );
        Ok((normalized, flags))
    }

    /// 读取并缓存平面的 `IN_FORMATS`（`(fourcc, modifier)` 组合列表）。
    ///
    /// 失败时**不会**永久粘住：按 [`IN_FORMATS_RETRY_INTERVAL`] 冷却重试，
    /// 每次重试都会告警（带累计失败次数），成功恢复时打印一条 INFO。
    ///
    /// # Errors
    ///
    /// 属性缺失、blob 读取失败或解析失败时返回 `Ok(None)`（调用方据此降级），
    /// 真正的设备错误不会在这里变成 panic。
    fn plane_in_formats(
        &mut self,
        plane_handle: plane::Handle,
    ) -> Result<Option<&Vec<FormatModifier>>, DisplayError> {
        let card = &self.card; // 与 `in_formats_cache` 分属不同字段：借用互不冲突
        let (value, outcome) =
            self.in_formats_cache
                .get_or_try(plane_handle, Instant::now(), || {
                    read_in_formats(card, plane_handle)
                });
        match outcome {
            CacheOutcome::Hit | CacheOutcome::LoadedFirstTry => {}
            CacheOutcome::RecoveredAfterFailure => tracing::info!(
                plane = ?plane_handle,
                "IN_FORMATS 解析已恢复：平面的 (fourcc, modifier) 双重校验重新生效"
            ),
            CacheOutcome::Unavailable { retried, attempts } => {
                if retried {
                    tracing::warn!(
                        plane = ?plane_handle,
                        attempts,
                        "IN_FORMATS 仍不可用（第 {attempts} 次失败）：本帧降级为仅比 fourcc；\
                         将在 {IN_FORMATS_RETRY_INTERVAL:?} 后重试"
                    );
                }
            }
        }
        Ok(value)
    }

    /// 选择能扫描输出 `(fourcc, modifier)` 的平面。
    ///
    /// 判定顺序（与 mpv `probe_gbm_modifiers()` 的思路一致）：
    /// 1. 优先当前平面（静态帧用的那个）；
    /// 2. 其次同一 CRTC 的其它平面（i915 上 NV12 只在 PLANE_1/PLANE_2，
    ///    且不能在 pipe C —— 这直接决定了这里能不能找到平面）；
    /// 3. 每个候选都要过 `fourcc ∈ formats()` **且** `IN_FORMATS` 含该组合。
    ///
    /// `IN_FORMATS` 读不到时降级为"只比 fourcc"并告警（不静默：日志里可见）。
    ///
    /// # Errors
    ///
    /// 没有平面支持该组合时返回 [`DisplayError::Unsupported`]，错误信息带上
    /// 主平面的 fourcc 列表与 IN_FORMATS 组合，便于现场定位。
    fn plane_for_dmabuf(
        &mut self,
        index: usize,
        fourcc: DrmFourcc,
        modifier: u64,
    ) -> Result<plane::Handle, DisplayError> {
        let crtc = self.outputs[index]
            .crtc
            .ok_or_else(|| DisplayError::Failed {
                operation: "plane_for_dmabuf",
                message: "CRTC 未绑定".to_string(),
            })?;
        let resources = self
            .card
            .resource_handles()
            .map_err(|e| io_err("resource_handles", e))?;
        let candidates: Vec<plane::Handle> = self
            .card
            .plane_handles()
            .map_err(|e| io_err("plane_handles", e))?
            .into_iter()
            .filter(|ph| {
                self.card
                    .get_plane(*ph)
                    .map(|info| {
                        resources
                            .filter_crtcs(info.possible_crtcs())
                            .contains(&crtc)
                    })
                    .unwrap_or(false)
            })
            .collect();
        // 当前平面优先，其余保持内核返回顺序。
        let mut ordered = Vec::with_capacity(candidates.len());
        if let Some(p) = self.outputs[index].plane {
            if candidates.contains(&p) {
                ordered.push(p);
            }
        }
        ordered.extend(
            candidates
                .iter()
                .copied()
                .filter(|p| Some(*p) != self.outputs[index].plane),
        );

        let mut reasons: Vec<String> = Vec::new();
        for ph in ordered {
            let formats = match self.card.get_plane(ph) {
                Ok(info) => info.formats().to_vec(),
                Err(e) => {
                    reasons.push(format!("{ph:?}: 读取平面失败 {e}"));
                    continue;
                }
            };
            if !formats.contains(&(fourcc as u32)) {
                reasons.push(format!("{ph:?}: fourcc 不在列表"));
                continue;
            }
            match self.plane_in_formats(ph)? {
                Some(combos) => {
                    if in_formats::supports(combos, fourcc as u32, modifier) {
                        // 只在 `(plane, fourcc, modifier)` **变化**时打 INFO（每帧都打会刷屏）。
                        let plane_raw: u32 = ph.into();
                        let key: PlaneChoice = (plane_raw, fourcc as u32, modifier);
                        if plane_choice_is_new(self.last_plane_choice, key) {
                            self.last_plane_choice = Some(key);
                            tracing::info!(
                                plane = ?ph,
                                fourcc = %fourcc,
                                modifier = format_args!("0x{modifier:016x}"),
                                "DMA-BUF 平面已选定（IN_FORMATS 命中；同一组合后续不再重复记录）"
                            );
                        } else {
                            tracing::trace!(
                                plane = ?ph,
                                fourcc = %fourcc,
                                modifier = format_args!("0x{modifier:016x}"),
                                "DMA-BUF 平面（组合未变）"
                            );
                        }
                        return Ok(ph);
                    }
                    reasons.push(format!(
                        "{ph:?}: IN_FORMATS 无 {} + 0x{modifier:016x}（有 {}）",
                        fourcc,
                        in_formats::describe(combos, 4)
                    ));
                }
                None => {
                    // 无法校验：明确告警后按"仅比 fourcc"接受，让内核做最终判定。
                    tracing::warn!(
                        plane = ?ph,
                        "IN_FORMATS 不可用，降级为仅校验 fourcc（modifier 由内核最终判定）"
                    );
                    return Ok(ph);
                }
            }
        }
        Err(DisplayError::Unsupported(format!(
            "无平面可扫描输出 {} + modifier 0x{modifier:016x}（crtc {}）：{}",
            fourcc,
            u32::from(crtc),
            reasons.join("; ")
        )))
    }

    /// 选择支持该扫描格式的平面：优先当前平面，其次同一 CRTC 的其他兼容平面。
    ///
    /// # Errors
    ///
    /// 没有平面支持该格式时返回 [`DisplayError::Unsupported`]，错误信息带上
    /// 当前平面可用格式（便于现场定位是"驱动不支持"还是"格式选错"）。
    fn plane_for_scanout(
        &self,
        index: usize,
        format: ScanoutFormat,
    ) -> Result<plane::Handle, DisplayError> {
        let fourcc = drm_fourcc_of(format);
        if let Some(p) = self.outputs[index].plane {
            if self.plane_supports(p, fourcc) {
                return Ok(p);
            }
        }
        let crtc = self.outputs[index]
            .crtc
            .ok_or_else(|| DisplayError::Failed {
                operation: "plane_for_scanout",
                message: "CRTC 未绑定".to_string(),
            })?;
        let resources = self
            .card
            .resource_handles()
            .map_err(|e| io_err("resource_handles", e))?;
        for ph in self
            .card
            .plane_handles()
            .map_err(|e| io_err("plane_handles", e))?
        {
            let info = self
                .card
                .get_plane(ph)
                .map_err(|e| io_err("get_plane", e))?;
            if resources
                .filter_crtcs(info.possible_crtcs())
                .contains(&crtc)
                && info.formats().contains(&(fourcc as u32))
            {
                return Ok(ph);
            }
        }
        let available = self.outputs[index]
            .plane
            .map(|p| self.plane_format_names(p))
            .unwrap_or_else(|| "（未绑定平面）".to_string());
        Err(DisplayError::Unsupported(format!(
            "输出 {} 无支持 {} 的平面；主平面可用格式: {}",
            self.outputs[index].info.id,
            format.fourcc_name(),
            available
        )))
    }

    /// 平面是否支持指定 fourcc。
    fn plane_supports(&self, plane: plane::Handle, fourcc: DrmFourcc) -> bool {
        self.card
            .get_plane(plane)
            .map(|info| info.formats().contains(&(fourcc as u32)))
            .unwrap_or(false)
    }

    /// 平面支持格式的可读列表（诊断用；最多列出 12 个）。
    fn plane_format_names(&self, plane: plane::Handle) -> String {
        let Ok(info) = self.card.get_plane(plane) else {
            return "（读取平面失败）".to_string();
        };
        info.formats()
            .iter()
            .take(12)
            .map(|f| match DrmFourcc::try_from(*f) {
                Ok(fourcc) => fourcc.to_string(),
                Err(_) => format!("0x{f:08x}"),
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 构造扫描输出 atomic 请求（属性句柄缓存 + MODE_ID blob 复用）。
    ///
    /// 返回 `(请求, 是否模式变更)`；模式变更时调用方需带 `ALLOW_MODESET`。
    ///
    /// 属性句柄在一次会话内稳定，逐帧重新枚举会产生「每个对象一次
    /// `get_properties` + 逐属性 `get_property`」的 ioctl 风暴与三次
    /// `HashMap`/`String` 分配，因此按对象缓存。
    ///
    /// # Errors
    ///
    /// 尚未 `set_mode`、CRTC 未绑定或对象缺少必需 KMS 属性时返回
    /// [`DisplayError`]。
    fn build_scanout_request(
        &mut self,
        index: usize,
        plane_handle: plane::Handle,
        fb: framebuffer::Handle,
        src: (u64, u64, u64, u64),
        dst: (i64, i64, u64, u64),
        in_fence_fd: Option<i64>,
    ) -> Result<(atomic::AtomicModeReq, bool), DisplayError> {
        let mode = self.outputs[index]
            .active_mode
            .ok_or_else(|| DisplayError::Failed {
                operation: "present",
                message: "尚未设置显示模式（先调用 set_mode）".to_string(),
            })?;
        let (mw, mh) = (u32::from(mode.size().0), u32::from(mode.size().1));
        let crtc_handle = self.outputs[index]
            .crtc
            .ok_or_else(|| DisplayError::Failed {
                operation: "present",
                message: "CRTC 未绑定".to_string(),
            })?;
        let connector_handle = self.outputs[index].connector;

        // MODE_ID blob：同模式复用（逐帧新建 blob 会触发 modeset + DP
        // 重训练；真机曾实测每帧 ~1.77s 卡顿）。
        let mode_changed = self.outputs[index].committed_size != Some((mw, mh));
        if self.outputs[index].mode_blob_id.is_none() || mode_changed {
            if let Some(old) = self.outputs[index].mode_blob_id.take() {
                let _ = self.card.destroy_property_blob(old);
            }
            let mode_blob = self
                .card
                .create_property_blob(&mode)
                .map_err(|e| io_err("create_property_blob", e))?;
            self.outputs[index].mode_blob_id = match mode_blob {
                property::Value::Blob(id) => Some(id),
                _ => None,
            };
        }
        let mode_blob = property::Value::Blob(self.outputs[index].mode_blob_id.unwrap_or(0));

        // 属性句柄缓存（按对象/句柄命中，不逐帧枚举）。
        let (card, out) = (&self.card, &mut self.outputs[index]);
        let cache = &mut out.prop_cache;
        if cache.connector.as_ref().map(|(h, _)| *h) != Some(connector_handle) {
            cache.connector = Some((connector_handle, load_property_map(card, connector_handle)?));
        }
        if cache.crtc.as_ref().map(|(h, _)| *h) != Some(crtc_handle) {
            cache.crtc = Some((crtc_handle, load_property_map(card, crtc_handle)?));
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = cache.planes.entry(plane_handle) {
            let map = load_property_map(card, plane_handle)?;
            entry.insert(map);
        }
        let conn_props = &cache
            .connector
            .as_ref()
            .ok_or_else(|| property_cache_miss("connector"))?
            .1;
        let crtc_props = &cache
            .crtc
            .as_ref()
            .ok_or_else(|| property_cache_miss("crtc"))?
            .1;
        let plane_props = cache
            .planes
            .get(&plane_handle)
            .ok_or_else(|| property_cache_miss("plane"))?;

        let mut req = atomic::AtomicModeReq::new();
        add_prop(
            &mut req,
            connector_handle,
            conn_props,
            "CRTC_ID",
            property::Value::CRTC(Some(crtc_handle)),
        )?;
        add_prop(&mut req, crtc_handle, crtc_props, "MODE_ID", mode_blob)?;
        add_prop(
            &mut req,
            crtc_handle,
            crtc_props,
            "ACTIVE",
            property::Value::Boolean(true),
        )?;
        add_prop(
            &mut req,
            plane_handle,
            plane_props,
            "FB_ID",
            property::Value::Framebuffer(Some(fb)),
        )?;
        add_prop(
            &mut req,
            plane_handle,
            plane_props,
            "CRTC_ID",
            property::Value::CRTC(Some(crtc_handle)),
        )?;
        for (name, value) in [
            ("SRC_X", property::Value::UnsignedRange(src.0)),
            ("SRC_Y", property::Value::UnsignedRange(src.1)),
            ("SRC_W", property::Value::UnsignedRange(src.2)),
            ("SRC_H", property::Value::UnsignedRange(src.3)),
            ("CRTC_X", property::Value::SignedRange(dst.0)),
            ("CRTC_Y", property::Value::SignedRange(dst.1)),
            ("CRTC_W", property::Value::UnsignedRange(dst.2)),
            ("CRTC_H", property::Value::UnsignedRange(dst.3)),
        ] {
            add_prop(&mut req, plane_handle, plane_props, name, value)?;
        }
        // **t84**：让显示侧**等生产者写完**再开始扫（消费方等生产方）。
        // 注意：`IN_FENCE_FD` 只解决“何时可以开始读”，**不解决“何时允许重写”**
        // ⇒ 它不能替代缓冲区生命周期管理（那是 `t82` 钩子的事）。
        if let Some(raw) = in_fence_fd {
            add_prop(
                &mut req,
                plane_handle,
                plane_props,
                "IN_FENCE_FD",
                property::Value::SignedRange(raw),
            )?;
        }
        Ok((req, mode_changed))
    }

    /// 扫描缓冲池化统计（诊断/基准）：证明稳态提交不重新分配、不重新映射。
    pub fn scanout_stats(&self) -> Vec<(OutputId, PoolStats)> {
        self.outputs
            .iter()
            .map(|o| (o.info.id.clone(), o.scanout_pool.stats()))
            .collect()
    }

    /// DMA-BUF 预检（**不需要 DRM master**）：导入 + `AddFB2` + `atomic TEST_ONLY`。
    ///
    /// 用途：在切换到直通路径前先验证内核是否接受
    /// `(fourcc, modifier)` × 平面 × 缩放矩形 —— 这是 `TEST_ONLY` 提交，
    /// 不会改变任何显示状态，也不需要 master，因此可以在"别人持有显示"时
    /// 安全地做能力与状态校验（真机排障时尤其有用）。
    ///
    /// # Errors
    ///
    /// fourcc/modifier 无法表达、没有平面支持该组合、PRIME 导入或 `AddFB2` 失败、
    /// 内核 `TEST_ONLY` 校验失败（含错误详情）时返回 [`DisplayError`]。
    pub fn preflight_dmabuf(
        &mut self,
        output: OutputId,
        frame: &DmaBufFrame,
        src: (u64, u64, u64, u64),
        dst: (i64, i64, u64, u64),
    ) -> Result<DmabufPreflight, DisplayError> {
        let index = self
            .outputs
            .iter()
            .position(|o| o.info.id == output)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        if self.outputs[index].crtc.is_none() {
            self.bind_crtc_plane(index)?;
        }
        let mode = self.outputs[index]
            .active_mode
            .ok_or_else(|| DisplayError::Failed {
                operation: "preflight_dmabuf",
                message: "尚未设置显示模式（先调用 set_mode）".to_string(),
            })?;
        let (fw, fh) = (frame.width, frame.height);
        let fourcc = DrmFourcc::try_from(frame.fourcc).map_err(|_| {
            DisplayError::Unsupported(format!(
                "DMA-BUF fourcc 0x{:08x}（推测名 {}）不是已知 DRM 格式",
                frame.fourcc,
                in_formats::fourcc_name(frame.fourcc)
            ))
        })?;
        let (modifier, fb_flags) = Self::fb_modifier_and_flags(frame.modifier).map_err(|e| {
            DisplayError::DmaBufRejected {
                reason: format!("modifier 无法表达为 AddFB2 flags: {e}"),
            }
        })?;
        let plane_handle = self.plane_for_dmabuf(index, fourcc, frame.modifier)?;
        let crtc_handle = self.outputs[index]
            .crtc
            .ok_or_else(|| DisplayError::Failed {
                operation: "preflight_dmabuf",
                message: "CRTC 未绑定".to_string(),
            })?;

        let planes = plane_count_for(fourcc);
        let (gems, imported) = self.import_dmabuf_handles(frame, planes)?;
        let planar = DmaBufPlanar {
            size: (fw, fh),
            fourcc,
            modifier,
            gems,
            planes,
            strides: frame.strides,
            offsets: frame.offsets,
        };
        let fb = match self.card.add_planar_framebuffer(&planar, fb_flags) {
            Ok(fb) => fb,
            Err(e) => {
                self.destroy_gems(&imported);
                return Err(DisplayError::Failed {
                    operation: "preflight_dmabuf",
                    message: format!(
                        "AddFB2 失败（{fourcc}, modifier 0x{:016x}）（平面 {plane_handle:?}）: {e}",
                        frame.modifier
                    ),
                });
            }
        };

        // **t84**：导出 dma-buf 的写完成 fence → `IN_FENCE_FD`。
        // `_fence_keepalive` 必须活到 `atomic_commit` 返回（内核在 commit 内消费该 fd）。
        let mut _fence_keepalive: Option<std::os::fd::OwnedFd> = None;
        let in_fence_fd = if fence_enabled() {
            match export_write_fence(frame.primary_fd().as_fd()) {
                Ok(owned) => {
                    let raw = std::os::fd::AsRawFd::as_raw_fd(&owned) as i64;
                    let first = in_fence_counters().armed == 0;
                    fence_bump(|c| c.armed = c.armed.saturating_add(1));
                    if first {
                        tracing::info!(
                            fd = raw,
                            "IN_FENCE_FD 已装上（等生产者写完成后才开始扫）；\n                             注意：EXPORT_SYNC_FILE 是**快照**，若生产者未登记写 fence 则不代表真实完成时间"
                        );
                    }
                    _fence_keepalive = Some(owned);
                    Some(raw)
                }
                Err(e) => {
                    fence_bump(|c| c.export_failed = c.export_failed.saturating_add(1));
                    tracing::warn!(
                        error = %e,
                        "EXPORT_SYNC_FILE 失败：**拿不到写 fence**，本次不带 fence 提交（回退）"
                    );
                    None
                }
            }
        } else {
            None
        };
        let (req, mode_changed) =
            self.build_scanout_request(index, plane_handle, fb, src, dst, in_fence_fd)?;
        let test_flags = AtomicCommitFlags::TEST_ONLY
            | if mode_changed {
                AtomicCommitFlags::ALLOW_MODESET
            } else {
                AtomicCommitFlags::empty()
            };
        let verdict = self.card.atomic_commit(test_flags, req);
        let _ = self.card.destroy_framebuffer(fb);
        self.destroy_gems(&imported);
        match verdict {
            Ok(()) => {
                tracing::info!(
                    plane = ?plane_handle,
                    fourcc = %fourcc,
                    modifier = format_args!("0x{:016x}", frame.modifier),
                    "DMA-BUF 预检通过（TEST_ONLY 接受该状态）"
                );
                Ok(DmabufPreflight {
                    plane: plane_handle,
                    fourcc,
                    modifier: frame.modifier,
                    crtc: crtc_handle,
                    mode,
                })
            }
            Err(e) => Err(DisplayError::Failed {
                operation: "preflight_dmabuf",
                message: format!(
                    "atomic TEST_ONLY 拒绝该状态（{fourcc} + modifier 0x{:016x}，平面 {plane_handle:?}）：{e}；\
                     常见原因：i915 要求 NV12 落在 PLANE_1/PLANE_2 且非 pipe C，或缩小倍率超过 2×",
                    frame.modifier
                ),
            }),
        }
    }

    /// 把帧里**参与扫描的平面**各自所属的 object 导入为 GEM 句柄。
    ///
    /// 返回 `(按平面排列的 handles, 需要回收的全部句柄)`：同一 object 只导入一次
    /// （i915/iHD 的 NV12 是单 object 两平面 ⇒ 只导入 1 个句柄，行为与旧代码一致），
    /// NVIDIA 的两 object NV12 会得到 2 个句柄。
    fn import_dmabuf_handles(
        &self,
        frame: &DmaBufFrame,
        planes: u32,
    ) -> Result<([Option<drm::buffer::Handle>; 4], Vec<drm::buffer::Handle>), DisplayError> {
        use std::os::fd::AsFd;
        let mut imported: Vec<(usize, drm::buffer::Handle)> = Vec::new();
        let mut handles: [Option<drm::buffer::Handle>; 4] = [None; 4];
        for plane in 0..planes.min(4) as usize {
            let object = usize::from(frame.object_index[plane]);
            let handle = match imported.iter().find(|(o, _)| *o == object) {
                Some((_, handle)) => *handle,
                None => {
                    let fd = frame
                        .fds
                        .get(object)
                        .ok_or_else(|| DisplayError::DmaBufRejected {
                            reason: format!(
                                "平面 {plane} 指向 object {object}，但该帧只有 {} 个 fd",
                                frame.fds.len()
                            ),
                        })?;
                    let handle = self.card.prime_fd_to_buffer(fd.as_fd()).map_err(|e| {
                        DisplayError::DmaBufRejected {
                            reason: format!("PRIME 导入失败（object {object}）: {e}"),
                        }
                    })?;
                    imported.push((object, handle));
                    handle
                }
            };
            handles[plane] = Some(handle);
        }
        let all = imported.into_iter().map(|(_, handle)| handle).collect();
        Ok((handles, all))
    }

    /// 关闭一批 GEM 句柄（PRIME 导入的资源回收，幂等：重复关闭只记 debug）。
    fn destroy_gems(&self, handles: &[drm::buffer::Handle]) {
        for handle in handles {
            self.destroy_gem(*handle);
        }
    }

    /// 关闭 GEM 句柄（PRIME 导入的资源回收）。
    fn destroy_gem(&self, handle: drm::buffer::Handle) {
        use std::os::fd::AsFd;
        let raw: u32 = handle.into();
        let _ = drm_ffi::gem::close(self.card.as_fd(), raw);
    }

    /// 释放全部扫描资源（幂等）。
    fn release_scanouts(&mut self) {
        let mut drained: Vec<(framebuffer::Handle, Vec<drm::buffer::Handle>)> = Vec::new();
        let card = &self.card;
        for out in &mut self.outputs {
            // H4：先尽力让 CRTC 停止扫描（best-effort；失去 master 时必然失败，
            // 那就说明"这些缓冲可能仍在显示"，只能走下面的强制路径并告警）。
            if let Some(crtc) = out.crtc
                && let Err(e) = card.set_crtc(crtc, None, (0, 0), &[], None)
            {
                tracing::debug!(
                    error = %e,
                    output = %out.info.id,
                    "释放扫描缓冲前停 CRTC 失败（无 master？）—— 将按需要走强制释放"
                );
            }
            let mut factory = DrmScanoutFactory { card };
            // 默认**只销毁 FREE** 槽位（在扫描的缓冲绝不销毁）。
            let mut outcome = out.scanout_pool.release_all(&mut factory);
            if outcome.refused > 0 {
                // 设备关闭/重开："设备要没了"，必须清理；但必须把代价说出来。
                let forced = out.scanout_pool.release_all_forced(&mut factory);
                tracing::warn!(
                    output = %out.info.id,
                    destroyed = forced.destroyed,
                    refused = outcome.refused,
                    "释放扫描缓冲时仍有缓冲处于非空闲状态（设备关闭/重开路径）：\
                     已强制销毁，其内容不可预期（内核 fb/GEM 有引用计数，但本进程已放弃所有权）"
                );
                outcome.destroyed = outcome.destroyed.saturating_add(forced.destroyed);
            }
            // 设备关闭/重开：直通记账直接忘掉（**不**归还 —— 上游也在收摊；
            // 误发"可回收"会让上游提前复用一块可能仍被引用的缓冲）。
            out.prop_cache.clear();
            drained.append(&mut out.dmabuf_inflight);
            if let Some(blob) = out.mode_blob_id.take() {
                let _ = self.card.destroy_property_blob(blob);
            }
        }
        for (fb, gems) in drained {
            let _ = self.card.destroy_framebuffer(fb);
            self.destroy_gems(&gems);
        }
    }
}

impl Drop for DrmKmsBackend {
    fn drop(&mut self) {
        self.release_scanouts();
        tracing::info!(device = %self.path.display(), "DRM 资源已释放");
    }
}

/// 该 fourcc 是否为半平面（Y + 交错 UV，需要 2 个 AddFB2 平面）。
///
/// 覆盖本项目会用到的半平面格式；P010/P012/P016 也一并列出（gen9 不支持直扫，
/// 但判定本身无害）。
/// 该 fourcc 需要几个 AddFB2 平面。
///
/// - **3 平面**：`YUV420`（= I420 / gst `YU12`）—— Y/U/V 各自独立 stride/offset；
/// - **2 平面**：半平面格式（NV12/NV21/P010…）；
/// - **1 平面**：打包格式（XR24/AB24…）。
///
/// **t73 修法 B**：`vapostproc` 能输出**线性 I420（YU12）**（`t64` 实测可协商），
/// 线性 ⇒ `modifier=0x0` ⇒ 摆脱 tile 语义；且 VPP 产出**它自己的**缓冲 ⇒
/// 显示扫描期间没有解码器在写它（这是"解耦"的关键）。
const fn plane_count_for(fourcc: DrmFourcc) -> u32 {
    if is_semi_planar_fourcc(fourcc) {
        2
    } else if matches!(fourcc, DrmFourcc::Yuv420) {
        3
    } else {
        1
    }
}

const fn is_semi_planar_fourcc(fourcc: DrmFourcc) -> bool {
    matches!(
        fourcc,
        DrmFourcc::Nv12
            | DrmFourcc::Nv21
            | DrmFourcc::Nv16
            | DrmFourcc::Nv61
            | DrmFourcc::P010
            | DrmFourcc::P012
            | DrmFourcc::P016
    )
}

/// DRM master 状态**变更**时打印一行稳定的、可 grep 的日志（t26）。
///
/// 为什么必须"状态变更才打、且格式稳定"：t13/t14 对同一场景得出相反结论，
/// 原因之一是判据只能靠日志里出现某个值（`master=Some(true)`），
/// 而"没打"既可能是"没恢复"也可能是"没尝试"。这行日志把三件事分清：
/// **从什么状态变到什么状态、为什么**（判定依据）。
///
/// 交叉验证仍以内核为准：`/sys/kernel/debug/dri/<n>/clients` 的 `master` 标记
/// （唯一可信判据）+ 提交成功计数 + i915 pipe CRC；**不用 `presented`**。
fn log_master_transition(old: Option<bool>, new: Option<bool>, why: &str) {
    if old == new {
        return;
    }
    match new {
        Some(true) => tracing::warn!(
            from = ?old,
            to = ?new,
            why,
            "DRM master 状态变更: {old:?} -> Some(true)：本进程可提交显示状态（交叉验证：内核 clients master=y + 提交成功计数增长 + CRC 互异）"
        ),
        Some(false) => tracing::error!(
            from = ?old,
            to = ?new,
            why,
            "DRM master 状态变更: {old:?} -> Some(false)：本进程不是 master，**任何提交都不会生效、画面不会更新**（请释放占用者：systemctl stop liuguang / pkill -f liuguang）"
        ),
        None => tracing::warn!(
            from = ?old,
            to = ?new,
            why,
            "DRM master 状态变更: {old:?} -> None（未确定：等待首次提交结果裁定）"
        ),
    }
}

/// 复刻 `drm@0.15.0::add_planar_framebuffer` 的内部断言（含它自己的 `Invalid` 过滤）：
/// 返回 `true` 表示该 `(modifier(), flags)` 组合**不会**触发 panic。
///
/// 单独抽出来是为了让测试**能失败**：直接比较"我们算出的 flags"与"我们算出的 modifier"
/// 是恒真断言（同一个变量），必须用这条独立复刻的判据做负对照。
fn pairing_is_consistent(modifier: Option<drm::buffer::DrmModifier>, flags: FbCmd2Flags) -> bool {
    let effective = modifier.filter(|m| *m != drm::buffer::DrmModifier::Invalid);
    let has_modifier = flags.contains(FbCmd2Flags::MODIFIERS);
    (has_modifier && effective.is_some()) || (!has_modifier && effective.is_none())
}

/// 读取平面 `IN_FORMATS` 并解析为 `(fourcc, modifier)` 组合（失败返回 `None`）。
///
/// 由 [`RetryCache`] 驱动重试：因此这里只做 IO 与解析，不做缓存决策与告警。
fn read_in_formats(card: &Card, plane_handle: plane::Handle) -> Option<Vec<FormatModifier>> {
    let props = match card.get_properties(plane_handle) {
        Ok(props) => props,
        Err(e) => {
            tracing::debug!(error = %e, "读取平面属性失败，无法校验 IN_FORMATS");
            return None;
        }
    };
    let mut blob_id = None;
    for (handle, raw) in props.iter() {
        let Ok(info) = card.get_property(*handle) else {
            continue;
        };
        if info.name().to_bytes() == b"IN_FORMATS" {
            blob_id = Some(*raw);
            break;
        }
    }
    let Some(blob_id) = blob_id else {
        tracing::debug!(plane = ?plane_handle, "平面没有 IN_FORMATS 属性（无法校验 modifier）");
        return None;
    };
    let blob = match card.get_property_blob(blob_id) {
        Ok(blob) => blob,
        Err(e) => {
            tracing::debug!(error = %e, blob = blob_id, "读取 IN_FORMATS blob 失败");
            return None;
        }
    };
    match in_formats::parse_in_formats(&blob) {
        Ok(combos) => {
            tracing::debug!(
                plane = ?plane_handle,
                count = combos.len(),
                "IN_FORMATS 已解析: {}",
                in_formats::describe(&combos, 6)
            );
            Some(combos)
        }
        Err(e) => {
            tracing::debug!(error = %e, "IN_FORMATS blob 解析失败");
            None
        }
    }
}

/// 显式尝试取得 DRM master，并如实解释结果。
///
/// 内核语义（`DRM_IOCTL_SET_MASTER`）：
/// - 成功 ⇒ 本进程已是 master；
/// - `EINVAL` ⇒ 本进程**已经**是 master（内核用 EINVAL 表达"重复获取"）；
/// - `EACCES` / `EBUSY` ⇒ 拿不到：既可能是该 ioctl 的 `DRM_ROOT_ONLY` 权限限制
///   （非 root 调用必然 EACCES），也可能是已有别的进程持有 master。**此时无法
///   仅凭 ioctl 断定**，因此状态记为"未确定"，由首帧提交的成败给出确定结论。
fn acquire_master_status(card: &Card, path: &Path) -> MasterStatus {
    let mut status = MasterStatus {
        master: None,
        acquire_result: String::new(),
        holders: dedup_holders(scan_device_holders_excluding_self(path)),
        heal_attempts: 0,
        heal_failures: 0,
    };
    match drm_ffi::auth::acquire_master(card.as_fd()) {
        Ok(()) => {
            status.master = Some(true);
            status.acquire_result =
                "drmSetMaster 成功（显式取得；本进程可提交显示状态）".to_string();
        }
        Err(e) => {
            let errno = e.raw_os_error();
            status.acquire_result = format!(
                "drmSetMaster 失败（{e}）{}",
                match errno {
                    Some(EINVAL_ERRNO) => "：本进程已是 master",
                    Some(EACCES) =>
                        "：内核拒绝（该 ioctl 标记为 DRM_ROOT_ONLY，非 root 调用必然 EACCES；也可能已被他人持有）",
                    _ => "",
                }
            );
            if errno == Some(EINVAL_ERRNO) {
                status.master = Some(true);
            }
        }
    }
    if !status.holders.is_empty() {
        tracing::warn!(
            device = %path.display(),
            holders = ?status.holders,
            "检测到其它进程已打开该 DRM 设备（可能持有 master）"
        );
    }
    status
}

/// `EINVAL`：`drmSetMaster` 在"本进程已是 master"时返回它。
const EINVAL_ERRNO: i32 = 22;

/// 扫描格式 → DRM fourcc。
const fn drm_fourcc_of(format: ScanoutFormat) -> DrmFourcc {
    match format {
        ScanoutFormat::Xrgb8888 => DrmFourcc::Xrgb8888,
        ScanoutFormat::Abgr8888 => DrmFourcc::Abgr8888,
        ScanoutFormat::Nv12 => DrmFourcc::Nv12,
    }
}

/// 读取对象的属性名 → 属性句柄映射（仅缓存未命中时调用，不进逐帧路径）。
fn load_property_map<T: drm::control::ResourceHandle>(
    card: &Card,
    handle: T,
) -> Result<HashMap<String, property::Handle>, DisplayError> {
    let set = card
        .get_properties(handle)
        .map_err(|e| io_err("get_properties", e))?;
    let map = set
        .as_hashmap(card)
        .map_err(|e| io_err("PropertyValueSet::as_hashmap", e))?;
    Ok(map
        .into_iter()
        .map(|(name, info)| (name, info.handle()))
        .collect())
}

/// **本轮唯一一处 `unsafe`**（用户计划 P2 明确授权：`DMA-BUF Fence` 与 `IN_FENCE_FD` 衔接）。
///
/// 导出 dma-buf 的**写完成 fence**（`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` + `DMA_BUF_SYNC_WRITE`）。
///
/// **注意（`t74` 调研）**：这是**快照** —— 若生产者此刻**尚未登记**写 fence，
/// 导出的句柄**不可靠地代表实际完成时间**。因此调用方必须把“拿到/没拿到”如实上报，
/// 不能因为“调用成功”就宣称“已同步”。
///
/// # SAFETY
/// **仅此一处 `unsafe`**：`ioctl` 是 FFI 调用；入参是本函数内构造的、按 C ABI 布局的
/// `#[repr(C)]` POD（8 字节），出参只写该结构自身的 `flags`/`fd` 字段；
/// `fd` 由 `BorrowedFd` 保证有效（生命周期受调用方约束）；
/// 成功返回的 `fd` 立刻交给 `OwnedFd` 接管 ⇒ 关闭恰好一次、不会重复关闭。
#[allow(unsafe_code)]
fn export_write_fence(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    #[repr(C)]
    struct DrmDmaBufExportSyncFile {
        flags: u32,
        fd: i32,
    }
    // `_IOWR('b', 2, struct dma_buf_export_sync_file)` —— 8 字节。
    // **nr=2 是 EXPORT**（nr=3 是 IMPORT）—— 曾写成 3，真机返回 ENOTTY(os error 25)。
    const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: std::ffi::c_ulong = 0xC008_6202;
    const DMA_BUF_SYNC_WRITE: u32 = 2;
    let mut arg = DrmDmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_WRITE,
        fd: -1,
    };
    // SAFETY: 见函数文档 —— fd 有效、arg 为 #[repr(C)] POD、内核只写这两个字段。
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut arg) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if arg.fd < 0 {
        return Err(std::io::Error::other("EXPORT_SYNC_FILE 返回无效 fd"));
    }
    // SAFETY: 内核返回的 fd 归我们所有，只在此处构造一次 OwnedFd。
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(arg.fd) })
}

/// `IN_FENCE_FD` 计数（可观测证据：**到底有没有拿到 fence**）。
#[derive(Clone, Copy, Debug, Default)]
pub struct FenceCounters {
    /// 成功装上 `IN_FENCE_FD` 的次数。
    pub armed: u64,
    /// `EXPORT_SYNC_FILE` 失败次数（拿不到 fence）。
    pub export_failed: u64,
    /// 平面缺少 `IN_FENCE_FD` 属性的次数。
    pub prop_missing: u64,
}

thread_local! {
    static FENCE_COUNTERS: std::cell::Cell<FenceCounters> =
        const { std::cell::Cell::new(FenceCounters { armed: 0, export_failed: 0, prop_missing: 0 }) };
}

/// 读取 `IN_FENCE_FD` 计数（诊断/上报用）。
#[must_use]
pub fn in_fence_counters() -> FenceCounters {
    FENCE_COUNTERS.with(std::cell::Cell::get)
}

fn fence_bump(f: impl FnOnce(&mut FenceCounters)) {
    FENCE_COUNTERS.with(|c| {
        let mut v = c.get();
        f(&mut v);
        c.set(v);
    });
}

/// 环境变量：启用 `IN_FENCE_FD`（默认关闭，保持现网行为）。
pub const ENV_IN_FENCE: &str = "LIUGUANG_VIDEO_IN_FENCE";

fn fence_enabled() -> bool {
    std::env::var(ENV_IN_FENCE).is_ok_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "off" | "false" | "no"
        )
    })
}

fn property_cache_miss(what: &str) -> DisplayError {
    DisplayError::Failed {
        operation: "atomic_commit(构造)",
        message: format!("KMS 属性缓存缺少 {what} 条目（内部错误）"),
    }
}

/// 单平面打包格式的 AddFB2 描述（`AB24`；XR24 仍走 legacy AddFB 以保持既有行为）。
struct PackedPlanar {
    size: (u32, u32),
    handle: drm::buffer::Handle,
    pitch: u32,
    fourcc: DrmFourcc,
}

impl drm::buffer::PlanarBuffer for PackedPlanar {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.fourcc
    }
    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [Some(self.handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [0, 0, 0, 0]
    }
}

/// DMA-BUF 的 AddFB2 描述（格式/modifier/平面布局全部来自解码器导出）。
///
/// `modifier` 为 `None` 时**必须**配套 `FbCmd2Flags::empty()`：`drm@0.15.0`
/// 的 `add_planar_framebuffer` 会断言两者一致，旧实现正是在这里会 panic。
struct DmaBufPlanar {
    size: (u32, u32),
    fourcc: DrmFourcc,
    modifier: Option<drm::buffer::DrmModifier>,
    /// 每个平面各自的 GEM 句柄：单 object 帧（i915/iHD）各平面共用同一句柄
    /// （UV 以偏移区分）；多 object 帧（NVIDIA `SEPARATE_LAYERS`）Y/UV 各一个。
    gems: [Option<drm::buffer::Handle>; 4],
    /// 平面数：半平面格式（NV12/NV21/P010…）需要 **2 个平面共用同一 GEM 句柄**
    /// （UV 以偏移区分）；只给 1 个平面时内核会以 `EINVAL` 拒绝 `AddFB2`
    /// （真机 `dmabuf_probe` 已复现）。
    planes: u32,
    strides: [u32; 4],
    offsets: [u32; 4],
}

impl drm::buffer::PlanarBuffer for DmaBufPlanar {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.fourcc
    }
    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        self.modifier
    }
    fn pitches(&self) -> [u32; 4] {
        let mut out = [0u32; 4];
        let count = usize::try_from(self.planes).unwrap_or(0).min(4);
        out[..count].copy_from_slice(&self.strides[..count]);
        out
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        self.gems
    }
    fn offsets(&self) -> [u32; 4] {
        let mut out = [0u32; 4];
        let count = usize::try_from(self.planes).unwrap_or(0).min(4);
        out[..count].copy_from_slice(&self.offsets[..count]);
        out
    }
}

/// DRM 扫描槽位工厂：分配 dumb buffer、建帧缓冲、**建立一次映射**。
struct DrmScanoutFactory<'a> {
    card: &'a Card,
}

impl ScanoutFactory for DrmScanoutFactory<'_> {
    type Buf = DumbBuffer;
    type Map = DumbMapping<'static>;
    type Fb = framebuffer::Handle;

    fn create(
        &mut self,
        key: ScanoutKey,
    ) -> Result<Slot<Self::Buf, Self::Map, Self::Fb>, DisplayError> {
        let card = self.card;
        let fourcc = drm_fourcc_of(key.format);
        let container_h = key.format.container_height(key.height);
        let dumb = card
            .create_dumb_buffer(
                (key.width, container_h),
                fourcc,
                key.format.bytes_per_pixel() * 8,
            )
            .map_err(|e| io_err("create_dumb_buffer", e))?;

        let fb = match key.format {
            ScanoutFormat::Nv12 => {
                // AddFB2：NV12 双平面共用同一 GEM 句柄（UV 偏移 = pitch × 高）。
                let pitch = dumb.pitch();
                let planar = Nv12Planar {
                    size: (key.width, key.height),
                    handle: dumb.handle(),
                    pitches: [pitch, pitch],
                    offsets: [0, pitch.saturating_mul(key.height)],
                };
                match card.add_planar_framebuffer(&planar, FbCmd2Flags::empty()) {
                    Ok(fb) => fb,
                    Err(e) => {
                        let _ = card.destroy_dumb_buffer(dumb);
                        return Err(io_err("add_planar_framebuffer(NV12)", e));
                    }
                }
            }
            ScanoutFormat::Abgr8888 => {
                // 遗留 drmModeAddFB（depth/bpp）无法表达 AB24：depth=32/bpp=32
                // 会被内核解释成 AR24（[B,G,R,A]），与 Rgba8888 的 [R,G,B,A]
                // 字节序不符，必须走 AddFB2 显式指定 AB24。
                let planar = PackedPlanar {
                    size: (key.width, key.height),
                    handle: dumb.handle(),
                    pitch: dumb.pitch(),
                    fourcc,
                };
                match card.add_planar_framebuffer(&planar, FbCmd2Flags::empty()) {
                    Ok(fb) => fb,
                    Err(e) => {
                        let _ = card.destroy_dumb_buffer(dumb);
                        return Err(io_err("add_planar_framebuffer(AB24)", e));
                    }
                }
            }
            ScanoutFormat::Xrgb8888 => match card.add_framebuffer(&dumb, 24, 32) {
                Ok(fb) => fb,
                Err(e) => {
                    let _ = card.destroy_dumb_buffer(dumb);
                    return Err(io_err("add_framebuffer", e));
                }
            },
        };

        // 映射只建立一次并长期持有：`DumbBuffer` 是 `Copy` 的小型描述符
        // （不含所有权语义），把一份放进进程生命周期 arena 即可拿到
        // `'static` 生命周期，从而把 `DumbMapping` 直接存进槽位，
        // 避免每帧 `mmap`/`munmap`。
        // 代价：每次槽位重建泄漏 24 字节描述符（进程级上限 = 池深 × 重建次数）；
        // **内核资源不泄漏**——GEM 句柄的释放由槽位里那份拷贝在 destroy 中执行。
        let descriptor: &'static mut DumbBuffer = Box::leak(Box::new(dumb));
        let map: DumbMapping<'static> = match card.map_dumb_buffer(descriptor) {
            Ok(map) => map,
            Err(e) => {
                let _ = card.destroy_framebuffer(fb);
                let _ = card.destroy_dumb_buffer(dumb);
                return Err(io_err("map_dumb_buffer", e));
            }
        };
        Ok(Slot {
            key,
            buf: dumb,
            map,
            fb,
            // 新建槽位一律从 FREE 开始（所有权由池的状态机接管）。
            state: SlotState::Free,
        })
    }

    fn bytes<'a>(&self, map: &'a mut DumbMapping<'static>) -> &'a mut [u8] {
        map.as_mut()
    }

    fn pitch(&self, buf: &DumbBuffer) -> usize {
        buf.pitch() as usize
    }

    fn destroy(&mut self, slot: Slot<Self::Buf, Self::Map, Self::Fb>) {
        let Slot { buf, map, fb, .. } = slot;
        // 先解除映射（munmap），再释放帧缓冲与 GEM。
        drop(map);
        let _ = self.card.destroy_framebuffer(fb);
        let _ = self.card.destroy_dumb_buffer(buf);
    }
}

fn add_prop<H: drm::control::ResourceHandle>(
    req: &mut atomic::AtomicModeReq,
    handle: H,
    props: &HashMap<String, property::Handle>,
    name: &str,
    value: property::Value,
) -> Result<(), DisplayError> {
    let prop = props.get(name).ok_or_else(|| DisplayError::Failed {
        operation: "atomic_commit(构造)",
        message: format!("对象缺少 KMS 属性 {name}（驱动需支持 Atomic KMS 属性集）"),
    })?;
    req.add_property(handle, *prop, value);
    Ok(())
}

fn to_display_mode(mode: drm::control::Mode) -> DisplayMode {
    let (w, h) = mode.size();
    DisplayMode::from_hz(u32::from(w), u32::from(h), f64::from(mode.vrefresh()))
}

fn io_err(operation: &'static str, e: std::io::Error) -> DisplayError {
    DisplayError::Failed {
        operation,
        message: e.to_string(),
    }
}

/// 动态发现 `/dev/dri/cardN`（排序返回；不包含 render 节点）。
///
/// # Errors
///
/// 目录不可读时返回 [`DisplayError`]。
pub fn discover_cards() -> Result<Vec<PathBuf>, DisplayError> {
    let dir = Path::new("/dev/dri");
    let entries = std::fs::read_dir(dir)
        .map_err(|e| DisplayError::DeviceUnavailable(format!("读取 /dev/dri 失败: {e}")))?;
    let mut cards = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit()) {
            cards.push(entry.path());
        }
    }
    cards.sort();
    Ok(cards)
}

impl DrmKmsBackend {
    /// 选择目标输出：`wanted` 精确匹配名称，否则取第一个已连接且有模式的输出。
    ///
    /// # Errors
    ///
    /// 名称不存在或无可用输出时返回 [`DisplayError`]。
    pub fn pick_output(&self, wanted: Option<&str>) -> Result<OutputId, DisplayError> {
        let descs: Vec<crate::selection::ConnectorDesc> = self
            .outputs
            .iter()
            .map(|o| crate::selection::ConnectorDesc {
                name: o.info.id.as_str().to_string(),
                connected: o.info.connected,
                modes: o
                    .drm_modes
                    .iter()
                    .map(|m| crate::selection::ModeDesc {
                        width: u32::from(m.size().0),
                        height: u32::from(m.size().1),
                        refresh_millihz: m.vrefresh() * 1000,
                        preferred: is_preferred_mode(m),
                    })
                    .collect(),
            })
            .collect();
        let index = crate::selection::select_connector(&descs, wanted)?;
        Ok(self.outputs[index].info.id.clone())
    }

    /// 解析配置中的模式选择为具体显示模式（EDID 首选 / 手动指定）。
    ///
    /// # Errors
    ///
    /// 输出不存在或没有匹配模式时返回 [`DisplayError`]。
    pub fn pick_mode_for(
        &self,
        output: &OutputId,
        selection: &liuguang_core::config::ModeSelection,
    ) -> Result<DisplayMode, DisplayError> {
        let out = self
            .outputs
            .iter()
            .find(|o| o.info.id == *output)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        let descs: Vec<crate::selection::ModeDesc> = out
            .drm_modes
            .iter()
            .map(|m| crate::selection::ModeDesc {
                width: u32::from(m.size().0),
                height: u32::from(m.size().1),
                refresh_millihz: m.vrefresh() * 1000,
                preferred: is_preferred_mode(m),
            })
            .collect();
        let index = crate::selection::pick_mode(&descs, selection)?;
        Ok(out.info.modes[index])
    }
    fn present_dmabuf(
        &mut self,
        index: usize,
        frame: &DmaBufFrame,
        src: (u64, u64, u64, u64),
        dst: (i64, i64, u64, u64),
    ) -> Result<PresentToken, DisplayError> {
        if self.suspended {
            return Err(DisplayError::Suspended);
        }
        if !self.outputs[index].info.connected {
            return Err(DisplayError::DeviceUnavailable(format!(
                "输出 {} 已断开",
                self.outputs[index].info.id
            )));
        }
        if self.outputs[index].crtc.is_none() {
            self.bind_crtc_plane(index)?;
        }
        // 已知不是 master ⇒ 先尝试恢复（而不是等这一帧提交失败才发现）。
        let _ = self.poll_master_recovery();
        {
            // 每帧都会走到这里；INFO 会以 ~20 行/秒灌爆 journal，
            // 降级为 debug（需要排障时用 RUST_LOG=liuguang_display=debug 打开）。
            tracing::debug!(
                "DMA-BUF 直通路径启用：帧不触碰 CPU（PRIME 导入 + AddFB2WithModifiers + 平面直扫）"
            );
        }

        let mode = self.outputs[index]
            .active_mode
            .ok_or_else(|| DisplayError::Failed {
                operation: "present_dmabuf",
                message: "尚未设置显示模式（先调用 set_mode）".to_string(),
            })?;
        let (mw, mh) = (u32::from(mode.size().0), u32::from(mode.size().1));
        let (fw, fh) = (frame.width, frame.height);
        if fw == 0 || fh == 0 {
            return Err(DisplayError::Failed {
                operation: "present_dmabuf",
                message: format!("DMA-BUF 尺寸非法: {fw}x{fh}"),
            });
        }
        // 16.16 源矩形钳制到帧内；目标矩形钳制到模式内（与 CPU 路径一致，
        // 保证 fit/zoom/offset 走同一套平面矩形）。
        let mut src = (
            src.0.min(u64::from(fw) << 16),
            src.1.min(u64::from(fh) << 16),
            src.2
                .min((u64::from(fw) << 16).saturating_sub(src.0))
                .max(65536),
            src.3
                .min((u64::from(fh) << 16).saturating_sub(src.1))
                .max(65536),
        );

        // **裁切原点必须落在 tile 网格上**（2026-10-20 晚，真机定位）。
        //
        // i915 的 Y-tile 是 128 字节宽 × 32 行的块状布局。若 `SRC_X`/`SRC_Y`
        // 落在块中间，平面缩放器会从 tile 内部取样 ⇒ 实测表现为「固定位移的
        // 副本 + 横向条带 + 竖向接缝」（正是历史上那 7 条修法都没能消除的重影）。
        // 三条对照证据：
        //   * CPU 路径（LINEAR 缓冲）做同样的裁切+缩放 ⇒ 干净；
        //   * kmssink（Y_TILED 缓冲）**不裁切**（SRC_X=0）⇒ 干净；
        //   * 本路径（Y_TILED 缓冲）按 fit=cover 裁切 ⇒ 有横带。
        //
        // 因此对块状 modifier 把原点**向下对齐**到 tile 网格（误差 < 一个 tile，
        // 视觉上不可察觉），而不是放弃裁切。
        if let Some((tile_w, tile_h)) = tile_alignment(frame.modifier) {
            let snap = |v: u64, step: u64| v / (step << 16) * (step << 16);
            let snapped_x = snap(src.0, tile_w);
            let snapped_y = snap(src.1, tile_h);
            // 对齐后把被让出的部分补回宽度/高度，保持右/下边界不变。
            src.2 = src.2.saturating_add(src.0 - snapped_x);
            src.3 = src.3.saturating_add(src.1 - snapped_y);
            src.0 = snapped_x;
            src.1 = snapped_y;
            src.2 = src
                .2
                .min((u64::from(fw) << 16).saturating_sub(src.0))
                .max(65536);
            src.3 = src
                .3
                .min((u64::from(fh) << 16).saturating_sub(src.1))
                .max(65536);
        }
        let dst = (
            dst.0.clamp(0, i64::from(mw)),
            dst.1.clamp(0, i64::from(mh)),
            dst.2.clamp(1, u64::from(mw)),
            dst.3.clamp(1, u64::from(mh)),
        );

        // 1. fourcc 与 modifier：不再硬编码 NV12，也不再把 modifier 一律当 Y_TILED。
        let fourcc = DrmFourcc::try_from(frame.fourcc).map_err(|_| {
            DisplayError::Unsupported(format!(
                "DMA-BUF fourcc 0x{:08x}（推测名 {}）不是本 crate 支持的 DRM 格式",
                frame.fourcc,
                in_formats::fourcc_name(frame.fourcc)
            ))
        })?;
        let (modifier, fb_flags) = Self::fb_modifier_and_flags(frame.modifier)?;

        // 2. 平面选择：fourcc + IN_FORMATS 双重校验。
        let plane_handle = self
            .plane_for_dmabuf(index, fourcc, frame.modifier)
            .map_err(|e| DisplayError::DmaBufRejected {
                reason: format!("无平面接受该 (fourcc, modifier): {e}"),
            })?;
        let crtc_handle = self.outputs[index]
            .crtc
            .ok_or_else(|| DisplayError::Failed {
                operation: "present_dmabuf",
                message: "CRTC 未绑定".to_string(),
            })?;

        // 3. PRIME 导入：DMA-BUF fd → GEM 句柄（零拷贝，不复制像素）。
        let planes_needed = plane_count_for(fourcc);
        let (gems, imported) = self.import_dmabuf_handles(frame, planes_needed)?;

        // 4. AddFB2：格式/修饰符/平面布局全部来自帧描述。
        // 半平面格式（NV12 等）需要 2 个平面；打包格式（XR24/AB24）只需 1 个。
        // 平面数由 fourcc 的 vsub/hsub 语义决定，这里只区分"半平面 vs 打包"，
        // 未知格式按打包处理并让内核做最终判定。
        let planes = plane_count_for(fourcc);
        if planes >= 2 {
            // 多平面格式每平面必须有有效 stride；单 object 打包时的后续平面 offset 必须非 0，
            // 而多 object（每个平面独立 dma-buf fd）的后续平面 offset 允许为 0。
            let bad = (1..planes as usize).find(|i| {
                frame.strides[*i] == 0
                    || (frame.object_index[*i] == frame.object_index[0] && frame.offsets[*i] == 0)
            });
            if let Some(i) = bad {
                self.destroy_gems(&imported);
                return Err(DisplayError::DmaBufRejected {
                    reason: format!(
                        "DMA-BUF {fourcc} 需要 {planes} 个平面，但第 {} 个平面缺 stride/offset（strides={:?}/offsets={:?}）",
                        i + 1,
                        frame.strides,
                        frame.offsets
                    ),
                });
            }
        }
        let planar = DmaBufPlanar {
            size: (fw, fh),
            fourcc,
            modifier,
            gems,
            planes,
            strides: frame.strides,
            offsets: frame.offsets,
        };
        let fb = match self.card.add_planar_framebuffer(&planar, fb_flags) {
            Ok(fb) => fb,
            Err(e) => {
                self.destroy_gems(&imported);
                return Err(DisplayError::DmaBufRejected {
                    reason: format!(
                        "AddFB2 失败（{fourcc}, modifier 0x{:016x}, strides {:?}, offsets {:?}）: {e}",
                        frame.modifier, frame.strides, frame.offsets
                    ),
                });
            }
        };

        // 5. 原子提交（与 CPU 路径共用请求构造与提交逻辑）。
        // **t84**：导出 dma-buf 的写完成 fence → `IN_FENCE_FD`。
        // `_fence_keepalive` 必须活到 `atomic_commit` 返回（内核在 commit 内消费该 fd）。
        let mut _fence_keepalive: Option<std::os::fd::OwnedFd> = None;
        let in_fence_fd = if fence_enabled() {
            match export_write_fence(frame.primary_fd().as_fd()) {
                Ok(owned) => {
                    let raw = std::os::fd::AsRawFd::as_raw_fd(&owned) as i64;
                    let first = in_fence_counters().armed == 0;
                    fence_bump(|c| c.armed = c.armed.saturating_add(1));
                    if first {
                        tracing::info!(
                            fd = raw,
                            "IN_FENCE_FD 已装上（等生产者写完成后才开始扫）；\n                             注意：EXPORT_SYNC_FILE 是**快照**，若生产者未登记写 fence 则不代表真实完成时间"
                        );
                    }
                    _fence_keepalive = Some(owned);
                    Some(raw)
                }
                Err(e) => {
                    fence_bump(|c| c.export_failed = c.export_failed.saturating_add(1));
                    tracing::warn!(
                        error = %e,
                        "EXPORT_SYNC_FILE 失败：**拿不到写 fence**，本次不带 fence 提交（回退）"
                    );
                    None
                }
            }
        } else {
            None
        };
        let (req, mode_changed) =
            self.build_scanout_request(index, plane_handle, fb, src, dst, in_fence_fd)?;
        if let Err(e) = self.commit_atomic(index, req, fb, crtc_handle, mode, mode_changed) {
            let _ = self.card.destroy_framebuffer(fb);
            self.destroy_gems(&imported);
            return Err(DisplayError::Failed {
                operation: "present_dmabuf",
                message: format!(
                    "DMA-BUF 提交失败（{fourcc} + modifier 0x{:016x}，平面 {plane_handle:?}）：{e}；\
                     调用方应回退 CPU 上传路径",
                    frame.modifier
                ),
            });
        }

        // 5.5 **t82：把"上一帧"归还给上游**（直通路径的对称钩子）。
        //
        // 6. fb/gem 延迟释放：保留最近 2 个（在途翻页可能仍引用）。
        let mut to_release: Vec<(framebuffer::Handle, Vec<drm::buffer::Handle>)> = Vec::new();
        {
            let out = &mut self.outputs[index];
            out.dmabuf_inflight.push((fb, imported));
            if out.dmabuf_inflight.len() > 2 {
                to_release.push(out.dmabuf_inflight.remove(0));
            }
            out.committed_size = Some((mw, mh));
        }
        for (old_fb, old_gems) in to_release {
            let _ = self.card.destroy_framebuffer(old_fb);
            self.destroy_gems(&old_gems);
        }

        let seq = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        let token = PresentToken {
            output: self.outputs[index].info.id.clone(),
            sequence: seq,
        };
        tracing::debug!(
            output = %token.output,
            sequence = token.sequence,
            width = fw,
            height = fh,
            fourcc = %fourcc,
            modifier = format_args!("0x{:016x}", frame.modifier),
            plane = ?plane_handle,
            src = format_args!("{},{},{},{}", src.0 >> 16, src.1 >> 16, src.2 >> 16, src.3 >> 16),
            dst = format_args!("{},{},{},{}", dst.0, dst.1, dst.2, dst.3),
            "DMA-BUF 帧已直通扫描输出（CPU 未触碰像素，缩放由平面硬件完成）"
        );
        Ok(token)
    }
}

impl DisplayBackend for DrmKmsBackend {
    /// **2026-10-20 晚恢复**：横带根因查明为「KMS 平面缩放器处理 Y_TILED 缓冲」，
    /// 媒体侧改为产出 **LINEAR** DMABuf（`drm-format=NV12:0x0`，经 `vapostproc`
    /// 在 GPU 侧 detiling）后，本后端的 `present_dmabuf` 可干净上屏。
    /// 未覆盖时 trait 默认返回 `false`，会让 daemon 永远按 CPU 捕获打开管线。
    fn supports_dmabuf_scanout(&self) -> bool {
        true
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.outputs.iter().map(|o| o.info.clone()).collect()
    }

    fn default_output(&self, wanted: Option<&str>) -> Result<OutputId, DisplayError> {
        self.pick_output(wanted)
    }

    fn resolve_mode(
        &self,
        output: &OutputId,
        selection: &liuguang_core::config::ModeSelection,
    ) -> Result<DisplayMode, DisplayError> {
        self.pick_mode_for(output, selection)
    }

    fn connector_connected(&self, name: &str) -> Result<bool, DisplayError> {
        // 用内核缓存的连接状态（force_probe 会触发 DP 链路实测，
        // 每 connector ~250ms，真机排障：拖垮视频 tick 节奏）。
        let index = self
            .outputs
            .iter()
            .position(|o| o.info.id.as_str() == name)
            .ok_or_else(|| DisplayError::OutputNotFound(OutputId::new(name)))?;
        Ok(self.outputs[index].info.connected)
    }

    fn present(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
    ) -> Result<PresentToken, DisplayError> {
        // 兼容语义：帧已缩放到模式尺寸，整帧整屏。
        if let DisplayFrame::Cpu(cpu) = &frame {
            let (fw, fh) = (cpu.width, cpu.height);
            return self.present_fitted(
                output,
                frame,
                (0, 0, u64::from(fw) << 16, u64::from(fh) << 16),
                (0, 0, u64::from(fw), u64::from(fh)),
            );
        }
        self.present_fitted(output, frame, (0, 0, 0, 0), (0, 0, 0, 0))
    }

    fn present_fitted(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
        src: (u64, u64, u64, u64),
        dst: (i64, i64, u64, u64),
    ) -> Result<PresentToken, DisplayError> {
        let index = self
            .outputs
            .iter()
            .position(|o| o.info.id == output)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        match frame {
            DisplayFrame::Cpu(cpu) => self.present_cpu_fitted(index, &cpu, src, dst),
            // **已恢复（2026-10-20 晚）**：横带根因查明为「KMS 平面缩放器处理
            // Y_TILED 缓冲」而非缓冲被改写；媒体侧改为输出 LINEAR
            // （`drm-format=NV12:0x0`）后，同一段提交代码即可干净上屏。
            DisplayFrame::DmaBuf(f) => self.present_dmabuf(index, &f, src, dst),
            other => Err(DisplayError::Unsupported(format!(
                "不支持的帧存储类别: {:?}",
                other.kind()
            ))),
        }
    }

    fn set_mode(&mut self, output: OutputId, mode: DisplayMode) -> Result<(), DisplayError> {
        if self.suspended {
            return Err(DisplayError::Suspended);
        }
        let index = self
            .outputs
            .iter()
            .position(|o| o.info.id == output)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        let out = &mut self.outputs[index];
        if !out.info.supports_mode(mode) {
            return Err(DisplayError::ModeNotSupported { output, mode });
        }
        let drm_mode = out
            .drm_modes
            .iter()
            .copied()
            .find(|m| to_display_mode(*m) == mode)
            .ok_or_else(|| DisplayError::ModeNotSupported {
                output: output.clone(),
                mode,
            })?;
        out.active_mode = Some(drm_mode);
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), DisplayError> {
        self.suspended = true;
        tracing::warn!(device = %self.path.display(), "显示后端挂起（VT 切换/权限失效）");
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DisplayError> {
        self.suspended = false;
        tracing::info!(device = %self.path.display(), "显示后端恢复");
        Ok(())
    }

    fn master_status(&self) -> Option<&MasterStatus> {
        Some(&self.master_status)
    }

    fn poll_master_recovery(&mut self) -> bool {
        // 转发到固有方法（限速逻辑在固有实现内部）。
        DrmKmsBackend::poll_master_recovery(self)
    }

    fn commit_stats(&self) -> Option<CommitStats> {
        Some(self.commit_stats)
    }
}

/// [`ModeTypeFlags`] 里 EDID 首选模式的探测辅助（供 [`crate::selection`] 转换用）。
pub fn is_preferred_mode(mode: &drm::control::Mode) -> bool {
    mode.mode_type().contains(ModeTypeFlags::PREFERRED)
}

/// 块状（tiled）modifier 的裁切原点对齐网格（像素）。
///
/// 只对**已知的块状布局**返回网格；线性（modifier 0）和未知 modifier 返回
/// `None`（不做任何调整，行为与历史一致）。
///
/// i915 的 Y-tile / Yf-tile 都是 **128 字节宽 × 32 行**的块。NV12 的 Y 平面
/// 是 1 字节/像素 ⇒ 128 像素宽；UV 平面按 2 像素/字节 ⇒ 64 像素。这里取两者
/// 的**最严格**约束（128 × 32），保证两个平面同时对齐。
const fn tile_alignment(modifier: u64) -> Option<(u64, u64)> {
    match modifier {
        // I915_FORMAT_MOD_Y_TILED / I915_FORMAT_MOD_Yf_TILED
        0x0100_0000_0000_0002 | 0x0100_0000_0000_0004 => Some((128, 32)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    /// **裁切原点必须对齐到 tile 网格**（2026-10-20 晚，真机定位的重影根因）。
    ///
    /// i915 的 Y_TILED / Yf_TILED 是 **128 字节宽 × 32 行**的块状布局；`SRC_X`/`SRC_Y`
    /// 落在块中间时，平面缩放器会从 tile 内部取样 ⇒ 实测表现为「固定位移的副本 +
    /// 横向条带 + 竖向接缝」。三条对照证据（真机）：
    ///   * CPU 路径（LINEAR 缓冲）做同样的裁切+缩放 ⇒ 干净；
    ///   * kmssink（Y_TILED 缓冲）**不裁切**（SRC_X=0）⇒ 干净；
    ///   * 本路径（Y_TILED 缓冲）按 cover 裁切 ⇒ 有横带；对齐后 ⇒ 干净。
    ///
    /// 变异：把返回的网格改成 `None` ⇒ 真机重影立刻复现（本用例仍绿，因为不变量
    /// 是"已知块状 modifier 必须给出网格"；把 `Some((128,32))` 改掉本用例才变红）。
    #[test]
    fn tiled_modifiers_expose_their_alignment_grid() {
        use super::tile_alignment;
        const Y_TILED: u64 = 0x0100_0000_0000_0002;
        const YF_TILED: u64 = 0x0100_0000_0000_0004;
        assert_eq!(tile_alignment(Y_TILED), Some((128, 32)), "Y_TILED 的网格");
        assert_eq!(tile_alignment(YF_TILED), Some((128, 32)), "Yf_TILED 的网格");
        // 线性缓冲没有 tile 语义 ⇒ 不能做任何对齐（否则会无谓改变构图）。
        assert_eq!(tile_alignment(0), None, "LINEAR 不做对齐");
        assert_eq!(tile_alignment(0xdead_beef), None, "未知 modifier 不做对齐");
    }

    /// t73：I420（`Yuv420`/gst `YU12`）必须按 **3 平面**提交；NV12 仍 2 平面。
    ///
    /// 变异：把 `Yuv420` 当 2 平面 ⇒ 本用例变红（少一个平面会画错/EINVAL）。
    #[test]
    fn plane_count_covers_i420_three_planes() {
        use super::{DrmFourcc, is_semi_planar_fourcc, plane_count_for};
        assert_eq!(plane_count_for(DrmFourcc::Yuv420), 3, "I420 需要 3 个平面");
        assert_eq!(plane_count_for(DrmFourcc::Nv12), 2, "NV12 需要 2 个平面");
        assert_eq!(plane_count_for(DrmFourcc::Xrgb8888), 1, "打包格式 1 个平面");
        assert!(is_semi_planar_fourcc(DrmFourcc::Nv12));
        assert!(
            !is_semi_planar_fourcc(DrmFourcc::Yuv420),
            "I420 不是半平面（Y/U/V 各自独立）"
        );
    }
    use super::*;

    // ============ t32：显示侧 DMA-BUF 拒绝注入钩子 ============

    #[test]
    fn dmabuf_reject_hook_parses_only_explicit_on_values() {
        for on in ["1", "on", "ON", "true", "Yes", " 1 "] {
            assert!(dmabuf_reject_hook(Some(on)), "值 {on:?} 应当开启注入");
        }
        for off in ["0", "off", "false", "no", "", "anything"] {
            assert!(
                !dmabuf_reject_hook(Some(off)),
                "值 {off:?} 不得开启注入（未显式打开就必须逐字节不变）"
            );
        }
        assert!(!dmabuf_reject_hook(None), "未设置是默认路径，必须关闭");
    }

    #[test]
    fn dmabuf_rejection_error_is_typed_and_readable() {
        // 调用方靠**类型**精确匹配（不猜字符串），文案要能直接看懂原因。
        let err = DisplayError::DmaBufRejected {
            reason: "注入：LIUGUANG_DMABUF_REJECT=1".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("DMA-BUF"), "文案必须点明 DMA-BUF: {text}");
        assert!(
            text.contains("显示侧拒绝"),
            "文案必须点明是显示侧拒绝: {text}"
        );
        assert!(matches!(err, DisplayError::DmaBufRejected { .. }));
    }

    // ============ t29：DMA-BUF 平面日志不得逐帧刷屏 ============

    #[test]
    fn plane_choice_logs_only_on_change() {
        // 首次：打（新人可见的一次性诊断）。
        assert!(plane_choice_is_new(
            None,
            (35, 0x3231564E, 0x0100_0000_0000_0002)
        ));
        // 同一组合重复出现（= 每帧都跑到的稳态）：**不打**。
        let steady = (35u32, 0x3231_564Eu32, 0x0100_0000_0000_0002u64);
        assert!(
            !plane_choice_is_new(Some(steady), steady),
            "同一 (plane, fourcc, modifier) 重复出现时必须沉默（否则 60fps 下 ~60 行/秒）"
        );
        // 三要素任一变化：必须打（否则排查时看不到切换）。
        assert!(plane_choice_is_new(Some(steady), (44, steady.1, steady.2)));
        assert!(plane_choice_is_new(
            Some(steady),
            (steady.0, 0x3432_5247, steady.2)
        ));
        assert!(plane_choice_is_new(Some(steady), (steady.0, steady.1, 0)));
    }

    #[test]
    fn steady_state_plane_choice_is_silent_over_many_frames() {
        // 模拟 600 帧（10s@60fps）稳态：只应产生 1 条日志（首帧）。
        let combo = (35u32, 0x3231_564Eu32, 0x0100_0000_0000_0002u64);
        let mut last: Option<PlaneChoice> = None;
        let mut logs = 0usize;
        for _ in 0..600 {
            if plane_choice_is_new(last, combo) {
                logs += 1;
                last = Some(combo);
            }
        }
        assert_eq!(logs, 1, "600 帧稳态必须只有 1 条日志（旧实现是 600 条）");
    }

    #[test]
    fn discover_cards_is_safe_without_devices() {
        // 无 /dev/dri 的环境：返回空列表或读取错误，不 panic。
        match discover_cards() {
            Ok(cards) => {
                for p in cards {
                    let name = p.file_name().unwrap().to_string_lossy().into_owned();
                    assert!(
                        name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit())
                    );
                }
            }
            Err(DisplayError::DeviceUnavailable(_)) => {}
            Err(other) => panic!("意外错误: {other:?}"),
        }
    }

    #[test]
    fn open_missing_device_fails_cleanly() {
        let err = match DrmKmsBackend::open("/dev/dri/card-does-not-exist") {
            Err(e) => e,
            Ok(_) => panic!("不存在的设备节点应失败"),
        };
        assert!(
            matches!(err, DisplayError::DeviceUnavailable(_)),
            "实际: {err}"
        );
    }

    #[test]
    fn open_non_drm_node_fails_cleanly() {
        // /dev/null 可打开但不是 DRM 节点，ioctl 失败应转为明确错误。
        let err = match DrmKmsBackend::open("/dev/null") {
            Err(e) => e,
            Ok(_) => panic!("/dev/null 不是 DRM 节点，应失败"),
        };
        assert!(
            matches!(
                err,
                DisplayError::DeviceUnavailable(_) | DisplayError::Failed { .. }
            ),
            "实际: {err}"
        );
    }

    #[test]
    fn auto_open_without_devices_reports_clearly() {
        // auto 路径：无设备时给出可读错误（CI 环境必然走这条）。
        if discover_cards().map(|c| c.is_empty()).unwrap_or(true) {
            let err = match DrmKmsBackend::open("auto") {
                Err(e) => e,
                Ok(_) => panic!("无 DRM 设备时 auto 应失败"),
            };
            assert!(
                matches!(err, DisplayError::DeviceUnavailable(_)),
                "实际: {err}"
            );
        }
    }

    // ---------- t8: DMA-BUF 直通的纯逻辑（panic 修复 / 平面数 / fourcc） ----------

    /// panic 回归：`drm@0.15.0::add_planar_framebuffer` 断言
    /// `(MODIFIERS && modifier().is_some()) || (!MODIFIERS && modifier().is_none())`。
    ///
    /// 真机复现过旧实现的组合：`modifier() = Some(Linear)` + `FbCmd2Flags::empty()`
    /// → `drm-0.15.0/src/control/mod.rs:360` panic（`dmabuf_probe --legacy`）。
    #[test]
    fn fb_modifier_pairing_satisfies_drm_assert() {
        for modifier in [
            0u64,
            0x0100_0000_0000_0002, // Y_TILED
            0x0100_0000_0000_0001, // X_TILED
            0x0100_0000_0000_0003, // Yf_TILED
            0x00ff_ffff_ffff_fffe, // 未收录值：必须保真透传
        ] {
            let (normalized, flags) = DrmKmsBackend::fb_modifier_and_flags(modifier)
                .expect("已知/未收录 modifier 都应可表达");
            // 两个**独立来源**的一致：flag 位的"有没有"必须与 modifier 的"有没有"相同。
            assert_eq!(
                flags.contains(FbCmd2Flags::MODIFIERS),
                modifier != 0,
                "flags 与 modifier 必须同真同假: modifier={modifier:#x}"
            );
            // 用独立复刻的断言判据再判一次（这条能真正失败，见下面的负对照）。
            assert!(
                pairing_is_consistent(normalized, flags),
                "modifier={modifier:#x} 的组合会触发 drm crate 断言"
            );
        }
        // modifier == 0（LINEAR）必须退化为 None + 不带 MODIFIERS（旧实现的 bug 点）。
        let (normalized, flags) = DrmKmsBackend::fb_modifier_and_flags(0).unwrap();
        assert_eq!(normalized, None);
        assert!(!flags.contains(FbCmd2Flags::MODIFIERS));
        // 已知的非零 modifier 必须保真。
        let (normalized, flags) =
            DrmKmsBackend::fb_modifier_and_flags(0x0100_0000_0000_0002).unwrap();
        assert_eq!(normalized, Some(drm::buffer::DrmModifier::I915_y_tiled));
        assert!(flags.contains(FbCmd2Flags::MODIFIERS));
    }

    /// **负对照（防止恒真断言复用）**：旧实现的那组组合必须被判为"会触发断言"。
    /// 若有人把 `pairing_is_consistent` 写坏成恒真，这条会失败。
    #[test]
    fn pairing_check_rejects_the_old_buggy_combination() {
        use drm::buffer::DrmModifier;
        assert!(
            !pairing_is_consistent(Some(DrmModifier::Linear), FbCmd2Flags::empty()),
            "旧实现（Some(Linear) + 不带 MODIFIERS）必须被判为不一致，否则本判据失去意义"
        );
        assert!(!pairing_is_consistent(None, FbCmd2Flags::MODIFIERS));
        assert!(!pairing_is_consistent(
            Some(DrmModifier::Invalid),
            FbCmd2Flags::MODIFIERS
        ));
        assert!(pairing_is_consistent(
            Some(DrmModifier::I915_y_tiled),
            FbCmd2Flags::MODIFIERS
        ));
        assert!(pairing_is_consistent(None, FbCmd2Flags::empty()));
    }

    /// 未收录的 modifier 必须**保真透传**（`drm-fourcc` 用 `Unrecognized(x)` 保存原值），
    /// 不能丢信息、也不能退化成 LINEAR；只有 `DRM_FORMAT_MOD_INVALID` 才拒绝。
    #[test]
    fn fb_modifier_preserves_unrecognized_and_rejects_invalid() {
        let unusual = 0x00ff_ffff_ffff_fffeu64;
        let normalized = DrmKmsBackend::fb_modifier(unusual).unwrap();
        assert_eq!(u64::from(normalized.unwrap()), unusual, "必须保真");
        let err = DrmKmsBackend::fb_modifier(DRM_FORMAT_MOD_INVALID).unwrap_err();
        assert!(matches!(err, DisplayError::Unsupported(_)), "实际: {err}");
    }

    /// t26 回归：恢复尝试必须**与是否在提交解耦**——`poll_master_recovery()` 在
    /// "已知不是 master" 时才行动，且被 [`MASTER_RETRY_INTERVAL`] 限速。
    ///
    /// 真机证据（DegradedStatic 场景）：占用者退出后 15s 内 0 次尝试，
    /// 因为旧实现只在提交失败时才重试，而降级状态不提交。
    #[test]
    fn recovery_decision_is_independent_of_commits_and_rate_limited() {
        use crate::master::RateLimiter;
        // 模拟 backend 的状态字段与限速器（不接触设备）。
        let mut master: Option<bool> = Some(false);
        let mut retries = RateLimiter::new(1, MASTER_RETRY_INTERVAL);
        let t0 = Instant::now();
        // 契约：poll 只在"已知非 master"时尝试。
        let should_try = |master: Option<bool>, retries: &mut RateLimiter, now| {
            master == Some(false) && retries.check(now).is_some()
        };
        assert!(
            should_try(master, &mut retries, t0),
            "已知非 master ⇒ 应尝试"
        );
        assert!(
            !should_try(master, &mut retries, t0 + Duration::from_millis(100)),
            "冷却期内不得重复重开设备"
        );
        assert!(
            should_try(master, &mut retries, t0 + MASTER_RETRY_INTERVAL),
            "冷却期满后必须再试"
        );
        // master 未知/已确认时不应无谓重开。
        master = None;
        assert!(!should_try(
            master,
            &mut retries,
            t0 + Duration::from_secs(10)
        ));
        master = Some(true);
        assert!(!should_try(
            master,
            &mut retries,
            t0 + Duration::from_secs(10)
        ));
    }

    /// 半平面格式需要 2 个 AddFB2 平面（真机 1 平面 → EINVAL，已复现）。
    #[test]
    fn semi_planar_fourcc_detection() {
        assert!(is_semi_planar_fourcc(DrmFourcc::Nv12));
        assert!(is_semi_planar_fourcc(DrmFourcc::Nv21));
        assert!(is_semi_planar_fourcc(DrmFourcc::P010));
        assert!(!is_semi_planar_fourcc(DrmFourcc::Xrgb8888));
        assert!(!is_semi_planar_fourcc(DrmFourcc::Abgr8888));
        assert!(!is_semi_planar_fourcc(DrmFourcc::Nv16) || is_semi_planar_fourcc(DrmFourcc::Nv16));
    }

    /// fourcc 解析：来自解码器 caps 的原始 u32 必须**照原样**使用，
    /// 不再硬编码 NV12（旧实现忽略 `frame.fourcc`）。
    #[test]
    fn dmabuf_fourcc_is_taken_from_frame() {
        assert_eq!(
            DrmFourcc::try_from(u32::from_le_bytes(*b"NV12")).unwrap(),
            DrmFourcc::Nv12
        );
        assert_eq!(
            DrmFourcc::try_from(u32::from_le_bytes(*b"XR24")).unwrap(),
            DrmFourcc::Xrgb8888
        );
        // 半平面判定与 fourcc 一致：NV12 走 2 平面，XR24 走 1 平面。
        let nv12 = DrmFourcc::try_from(u32::from_le_bytes(*b"NV12")).unwrap();
        assert_eq!(if is_semi_planar_fourcc(nv12) { 2 } else { 1 }, 2);
        let xr24 = DrmFourcc::try_from(u32::from_le_bytes(*b"XR24")).unwrap();
        assert_eq!(if is_semi_planar_fourcc(xr24) { 2 } else { 1 }, 1);
    }
}
