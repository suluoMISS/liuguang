//! 可注入的模拟显示后端（无硬件单元测试用）。
//!
//! 不接触 `/dev/dri`；行为尽量贴近真实语义：
//! 输出插拔、模式支持检查、挂起期间拒绝提交、提交序列号递增。
//!
//! 自 2026-10 起，模拟后端复用**真实**的扫描缓冲池与上传实现
//! （[`crate::scanout`]），因此可以在没有 GPU/DRM 的环境里复现并断言：
//!
//! - 缓冲分配次数与映射次数（稳态下应等于池深，**不随帧数增长**）；
//! - 每帧真实写入扫描缓冲的字节数（例如 4K NV12 = 12.4 MB/帧）；
//! - 不支持的像素格式与真实后端一样**明确报错**（不静默丢帧）。

use crate::scanout::{
    AcquirePolicy, ScanoutFactory, ScanoutFormat, ScanoutKey, ScanoutPool, Slot, SlotState, upload,
};
use crate::{CommitStats, DisplayBackend, DisplayError, FullscreenToplevel, PoolStats};
use liuguang_core::types::{DisplayFrame, DisplayMode, OutputId, OutputInfo, PresentToken};

/// 一次被记录的提交（仅元数据，不保留像素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedFrame {
    /// 目标输出。
    pub output: OutputId,
    /// 提交序列号。
    pub sequence: u64,
    /// 帧宽。
    pub width: u32,
    /// 帧高。
    pub height: u32,
}

/// 模拟显示后端。
#[derive(Debug)]
pub struct MockDisplayBackend {
    outputs: Vec<OutputInfo>,
    suspended: bool,
    next_sequence: u64,
    presented_log: Vec<PresentedFrame>,
    /// 每个输出一个扫描缓冲池（与真实后端同构，仅底层存储换成 `Vec<u8>`）。
    pools: Vec<ScanoutPool<MockBuf, MockMap, ()>>,
    /// 提交计数（模拟后端不会真的失败，用于上层/测试的一致性）。
    commits_ok: u64,
    /// 模拟"有一个窗口全屏"（`None` = 没有）。测试用它驱动
    /// `performance.pause_on_fullscreen` 的两条分支。
    ///
    /// 用 `Arc<Mutex<..>>`：后端的 trait 方法拿 `&self`（真实后端也是靠
    /// `RefCell` 观测事件快照），而测试需要**在 Runtime 持有后端之后**再翻转
    /// 这个状态——句柄克隆出去就行了。
    fullscreen: std::sync::Arc<std::sync::Mutex<Option<FullscreenToplevel>>>,
}

/// 模拟槽位工厂的底层缓冲（只需 pitch）。
#[derive(Debug)]
struct MockBuf {
    pitch: usize,
}

/// 模拟映射（宿主内存，代替 mmap）。
struct MockMap(Vec<u8>);

/// 模拟槽位工厂：不接触任何设备，但**与真实工厂同样的分配/映射计数语义**。
struct MockScanoutFactory;

impl ScanoutFactory for MockScanoutFactory {
    type Buf = MockBuf;
    type Map = MockMap;
    type Fb = ();

    fn create(&mut self, key: ScanoutKey) -> Result<Slot<MockBuf, MockMap, ()>, DisplayError> {
        let pitch = key.width as usize * key.format.bytes_per_pixel() as usize;
        let len = pitch * key.format.container_height(key.height) as usize;
        Ok(Slot {
            key,
            buf: MockBuf { pitch },
            map: MockMap(vec![0u8; len]),
            fb: (),
            // 新建槽位一律从 FREE 开始（所有权由池的状态机接管）。
            state: SlotState::Free,
        })
    }

    fn bytes<'a>(&self, map: &'a mut MockMap) -> &'a mut [u8] {
        &mut map.0
    }

    fn pitch(&self, buf: &MockBuf) -> usize {
        buf.pitch
    }

    fn destroy(&mut self, slot: Slot<MockBuf, MockMap, ()>) {
        drop(slot);
    }
}

impl Default for MockDisplayBackend {
    fn default() -> Self {
        Self::with_default_output()
    }
}

impl MockDisplayBackend {
    /// 无输出的空后端。
    pub fn empty() -> Self {
        Self {
            outputs: Vec::new(),
            suspended: false,
            next_sequence: 0,
            presented_log: Vec::new(),
            pools: Vec::new(),
            commits_ok: 0,
            fullscreen: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// 按名称列表构造多输出后端（每个输出 1080p60 + 720p60）。
    pub fn with_outputs(names: &[&str]) -> Self {
        let mut this = Self::empty();
        for name in names {
            this.outputs.push(OutputInfo {
                id: OutputId::new(*name),
                connected: true,
                modes: vec![
                    DisplayMode::from_hz(1920, 1080, 60.0),
                    DisplayMode::from_hz(1280, 720, 60.0),
                ],
                current_mode: Some(DisplayMode::from_hz(1920, 1080, 60.0)),
                physical_size_mm: Some((520, 290)),
            });
            this.pools.push(ScanoutPool::new(3));
        }
        this
    }

    /// 带一个 `HDMI-A-1`（1080p60 + 720p60）输出的后端。
    pub fn with_default_output() -> Self {
        Self::with_outputs(&["HDMI-A-1"])
    }

    /// 记录到的提交。
    pub fn presented(&self) -> &[PresentedFrame] {
        &self.presented_log
    }

    /// 扫描缓冲池化统计（离线复现"不每帧分配/映射"的核心证据）。
    ///
    /// `allocations`/`mappings` 在尺寸与格式不变时**不随提交次数增长**；
    /// `uploaded_bytes` 是真实写入扫描缓冲的字节数（与 `upload` 同源）。
    ///
    /// 各输出的池计数**本身就是累计值**，这里按输出求和（不做逐帧累加，
    /// 否则会把累计值再累加一次）。
    pub fn scanout_stats(&self) -> PoolStats {
        let mut agg = PoolStats::default();
        for pool in &self.pools {
            let stats = pool.stats();
            agg.allocations = agg.allocations.saturating_add(stats.allocations);
            agg.mappings = agg.mappings.saturating_add(stats.mappings);
            agg.reuses = agg.reuses.saturating_add(stats.reuses);
            agg.uploaded_bytes = agg.uploaded_bytes.saturating_add(stats.uploaded_bytes);
            // 所有权计数同样按输出求和（累计值，不逐帧累加）。
            agg.reclaims = agg.reclaims.saturating_add(stats.reclaims);
            agg.rejected_writes = agg.rejected_writes.saturating_add(stats.rejected_writes);
            agg.blocked_recreates = agg
                .blocked_recreates
                .saturating_add(stats.blocked_recreates);
            agg.exhausted_waits = agg.exhausted_waits.saturating_add(stats.exhausted_waits);
            agg.exhausted_drops = agg.exhausted_drops.saturating_add(stats.exhausted_drops);
            agg.max_in_use = agg.max_in_use.max(stats.max_in_use);
            agg.forced_releases = agg.forced_releases.saturating_add(stats.forced_releases);
        }
        agg
    }

    /// 模拟显示器拔出：标记输出断开（保留模式枚举）。
    pub fn unplug(&mut self, name: &str) {
        if let Some(out) = self.outputs.iter_mut().find(|o| o.id.as_str() == name) {
            out.connected = false;
            out.current_mode = None;
        }
    }

    /// 模拟显示器插入。
    pub fn plug(&mut self, name: &str) {
        if let Some(out) = self.outputs.iter_mut().find(|o| o.id.as_str() == name) {
            out.connected = true;
            out.current_mode = out.modes.first().copied();
        }
    }

    /// 当前是否挂起。
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }
}

impl MockDisplayBackend {
    /// 模拟"屏幕上有一个全屏窗口"（`None` = 没有）。
    ///
    /// 只影响 [`DisplayBackend::fullscreen_toplevel`]：daemon 的全屏暂停逻辑
    /// 因此可以在没有合成器、没有窗口的测试里被驱动。
    pub fn set_fullscreen(&mut self, toplevel: Option<FullscreenToplevel>) {
        if let Ok(mut slot) = self.fullscreen.lock() {
            *slot = toplevel;
        }
    }

    /// 拿一个可共享的全屏开关句柄：测试可以在 Runtime 建好之后翻转它。
    pub fn fullscreen_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<Option<FullscreenToplevel>>> {
        std::sync::Arc::clone(&self.fullscreen)
    }
}

impl DisplayBackend for MockDisplayBackend {
    fn outputs(&self) -> Vec<OutputInfo> {
        self.outputs.clone()
    }

    fn fullscreen_toplevel(&self) -> Option<FullscreenToplevel> {
        self.fullscreen.lock().ok().and_then(|slot| slot.clone())
    }

    fn default_output(&self, wanted: Option<&str>) -> Result<OutputId, DisplayError> {
        let descs: Vec<crate::selection::ConnectorDesc> = self
            .outputs
            .iter()
            .map(|o| crate::selection::ConnectorDesc {
                name: o.id.as_str().to_string(),
                connected: o.connected,
                modes: o
                    .modes
                    .iter()
                    .enumerate()
                    .map(|(i, m)| crate::selection::ModeDesc {
                        width: m.width,
                        height: m.height,
                        refresh_millihz: m.refresh_millihz,
                        preferred: i == 0,
                    })
                    .collect(),
            })
            .collect();
        let index = crate::selection::select_connector(&descs, wanted)?;
        Ok(self.outputs[index].id.clone())
    }

    fn resolve_mode(
        &self,
        output: &OutputId,
        selection: &liuguang_core::config::ModeSelection,
    ) -> Result<DisplayMode, DisplayError> {
        let out = self
            .outputs
            .iter()
            .find(|o| o.id == *output)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        let descs: Vec<crate::selection::ModeDesc> = out
            .modes
            .iter()
            .enumerate()
            .map(|(i, m)| crate::selection::ModeDesc {
                width: m.width,
                height: m.height,
                refresh_millihz: m.refresh_millihz,
                preferred: i == 0,
            })
            .collect();
        let index = crate::selection::pick_mode(&descs, selection)?;
        Ok(out.modes[index])
    }

    fn connector_connected(&self, name: &str) -> Result<bool, DisplayError> {
        self.outputs
            .iter()
            .find(|o| o.id.as_str() == name)
            .map(|o| o.connected)
            .ok_or_else(|| DisplayError::OutputNotFound(OutputId::new(name)))
    }

    fn present(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
    ) -> Result<PresentToken, DisplayError> {
        self.present_fitted(output, frame, (0, 0, 0, 0), (0, 0, 0, 0))
    }

    fn present_fitted(
        &mut self,
        output: OutputId,
        frame: DisplayFrame,
        _src: (u64, u64, u64, u64),
        _dst: (i64, i64, u64, u64),
    ) -> Result<PresentToken, DisplayError> {
        if self.suspended {
            return Err(DisplayError::Suspended);
        }
        let info = self
            .outputs
            .iter()
            .find(|o| o.id == output)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        if !info.connected {
            return Err(DisplayError::DeviceUnavailable(format!(
                "输出 {output} 已断开"
            )));
        }
        let (copied, pool_index) = match &frame {
            DisplayFrame::Cpu(cpu) => {
                cpu.validate()?;
                let sfmt = ScanoutFormat::from_pixel_format(cpu.format).ok_or_else(|| {
                    DisplayError::Unsupported(format!(
                        "帧格式 {:?} 不能直接扫描输出（支持 Xrgb8888 / Argb8888 / Rgba8888 / Nv12）",
                        cpu.format
                    ))
                })?;
                let index = self
                    .outputs
                    .iter()
                    .position(|o| o.id == output)
                    .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
                let key = ScanoutKey {
                    width: cpu.width,
                    height: cpu.height,
                    format: sfmt,
                };
                let mut factory = MockScanoutFactory;
                // 显式所有权：取写权 → 上传 → READY → 模拟提交成功 → 归还。
                let mut guard =
                    self.pools[index].acquire(&mut factory, key, AcquirePolicy::TryOnce)?;
                let pitch = guard.pitch;
                let copied = upload(guard.bytes(&mut factory)?, pitch, cpu, sfmt)?;
                let ready = guard.mark_ready();
                if let Some(reclaimed) = self.pools[index].on_commit_success(ready) {
                    self.pools[index].release_to_upstream(reclaimed);
                }
                (copied, index)
            }
            DisplayFrame::DmaBuf(_) => {
                // 与真实后端同构的类型化拒绝：调用方据此重开 CPU 捕获管线，
                // 而不是把它当普通提交失败（连续几次就会误降级成静态图）。
                return Err(DisplayError::DmaBufRejected {
                    reason: "模拟后端不接受 DMA-BUF 直通帧".to_string(),
                });
            }
            other => {
                // 纹理帧等其它类别不属于模拟后端的语义范围：明确报错，不静默通过。
                return Err(DisplayError::Unsupported(format!(
                    "模拟后端不支持帧类别 {:?}",
                    other.kind()
                )));
            }
        };
        self.pools[pool_index].note_uploaded(copied);
        self.commits_ok = self.commits_ok.saturating_add(1);
        self.next_sequence += 1;
        let sequence = self.next_sequence;
        self.presented_log.push(PresentedFrame {
            output: output.clone(),
            sequence,
            width: frame.width(),
            height: frame.height(),
        });
        tracing::debug!(%output, sequence, width = frame.width(), height = frame.height(), "mock present");
        Ok(PresentToken { output, sequence })
    }

    fn set_mode(&mut self, output: OutputId, mode: DisplayMode) -> Result<(), DisplayError> {
        if self.suspended {
            return Err(DisplayError::Suspended);
        }
        let info = self
            .outputs
            .iter_mut()
            .find(|o| o.id == output)
            .ok_or_else(|| DisplayError::OutputNotFound(output.clone()))?;
        if !info.supports_mode(mode) {
            return Err(DisplayError::ModeNotSupported { output, mode });
        }
        if !info.connected {
            return Err(DisplayError::DeviceUnavailable(format!(
                "输出 {output} 已断开"
            )));
        }
        info.current_mode = Some(mode);
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), DisplayError> {
        self.suspended = true;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DisplayError> {
        self.suspended = false;
        Ok(())
    }

    fn commit_stats(&self) -> Option<CommitStats> {
        Some(CommitStats {
            commit_ok: self.commits_ok,
            ..CommitStats::default()
        })
    }
}

/// 无可用显示设备时的占位后端（§4.3 HeadlessReady）。
///
/// 服务保持在线、IPC 可用；一切显示操作返回 [`DisplayError::DeviceUnavailable`]。
/// 设备恢复后由上层替换为真实后端。
#[derive(Debug, Default)]
pub struct UnavailableBackend;

impl DisplayBackend for UnavailableBackend {
    fn outputs(&self) -> Vec<OutputInfo> {
        Vec::new()
    }

    fn default_output(&self, _wanted: Option<&str>) -> Result<OutputId, DisplayError> {
        Err(DisplayError::DeviceUnavailable(
            "无可用显示设备（HeadlessReady）".to_string(),
        ))
    }

    fn resolve_mode(
        &self,
        _output: &OutputId,
        _selection: &liuguang_core::config::ModeSelection,
    ) -> Result<DisplayMode, DisplayError> {
        Err(DisplayError::DeviceUnavailable(
            "无可用显示设备（HeadlessReady）".to_string(),
        ))
    }

    fn connector_connected(&self, _name: &str) -> Result<bool, DisplayError> {
        Err(DisplayError::DeviceUnavailable(
            "无可用显示设备（HeadlessReady）".to_string(),
        ))
    }

    fn present(
        &mut self,
        _output: OutputId,
        _frame: DisplayFrame,
    ) -> Result<PresentToken, DisplayError> {
        Err(DisplayError::DeviceUnavailable(
            "无可用显示设备（HeadlessReady）".to_string(),
        ))
    }

    fn present_fitted(
        &mut self,
        _output: OutputId,
        _frame: DisplayFrame,
        _src: (u64, u64, u64, u64),
        _dst: (i64, i64, u64, u64),
    ) -> Result<PresentToken, DisplayError> {
        Err(DisplayError::DeviceUnavailable(
            "无可用显示设备（HeadlessReady）".to_string(),
        ))
    }

    fn set_mode(&mut self, _output: OutputId, _mode: DisplayMode) -> Result<(), DisplayError> {
        Err(DisplayError::DeviceUnavailable(
            "无可用显示设备（HeadlessReady）".to_string(),
        ))
    }

    fn suspend(&mut self) -> Result<(), DisplayError> {
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DisplayError> {
        Ok(())
    }
}

#[cfg(test)]
mod headless_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use liuguang_core::types::{CpuFrame, PixelFormat};
    use std::sync::Arc;

    #[test]
    fn unavailable_backend_reports_clearly() {
        let mut b = UnavailableBackend;
        assert!(b.outputs().is_empty());
        assert!(matches!(
            b.default_output(None),
            Err(DisplayError::DeviceUnavailable(_))
        ));
        let frame = DisplayFrame::Cpu(CpuFrame {
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Xrgb8888,
            data: Arc::from(vec![0u8; 4].into_boxed_slice()),
        });
        assert!(matches!(
            b.present(OutputId::new("any"), frame),
            Err(DisplayError::DeviceUnavailable(_))
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liuguang_core::types::{CpuFrame, PixelFormat};
    use std::sync::Arc;

    fn frame(w: u32, h: u32, format: PixelFormat) -> DisplayFrame {
        let stride = if format.is_semi_planar() { w } else { w * 4 };
        let len = if format.is_semi_planar() {
            (stride as usize) * (h as usize) * 3 / 2
        } else {
            (stride as usize) * (h as usize)
        };
        DisplayFrame::Cpu(CpuFrame {
            width: w,
            height: h,
            stride,
            format,
            data: Arc::from(vec![0u8; len].into_boxed_slice()),
        })
    }

    /// 离线验收证据（无硬件）：连续 180 帧 4K NV12 提交，
    /// 分配/映射次数恒为池深，上传字节数 = 帧数 × 每帧字节。
    #[test]
    fn mock_reuses_scanout_buffers_across_180_frames_nv12_4k() {
        let mut backend = MockDisplayBackend::with_default_output();
        let output = OutputId::new("HDMI-A-1");
        let f = frame(3840, 2160, PixelFormat::Nv12);
        let per_frame = 3840u64 * 2160 + 3840 * 1080; // Y + UV
        for i in 0..180 {
            backend
                .present(output.clone(), f.clone())
                .unwrap_or_else(|e| {
                    panic!("第 {i} 帧提交失败: {e}");
                });
        }
        let stats = backend.scanout_stats();
        assert_eq!(stats.allocations, 3, "180 帧只允许 3 次分配（池深）");
        assert_eq!(stats.mappings, 3, "180 帧只允许 3 次映射");
        assert_eq!(stats.reuses, 177);
        assert_eq!(stats.uploaded_bytes, 180 * per_frame);
        assert_eq!(backend.presented().len(), 180);
    }

    /// 4K XRGB（静态图路径）同样不做逐帧分配/映射。
    #[test]
    fn mock_reuses_scanout_buffers_across_frames_xrgb_4k() {
        let mut backend = MockDisplayBackend::with_default_output();
        let output = OutputId::new("HDMI-A-1");
        let f = frame(3840, 2400, PixelFormat::Xrgb8888);
        for _ in 0..60 {
            backend.present(output.clone(), f.clone()).unwrap();
        }
        let stats = backend.scanout_stats();
        assert_eq!(stats.allocations, 3);
        assert_eq!(stats.mappings, 3);
        assert_eq!(stats.uploaded_bytes, 60 * 3840 * 2400 * 4);
    }

    /// 尺寸变化只重建被轮到的槽位（不是"每帧重建"）。
    #[test]
    fn mock_rebuilds_only_rotated_slot_on_resolution_change() {
        let mut backend = MockDisplayBackend::with_default_output();
        let output = OutputId::new("HDMI-A-1");
        for _ in 0..3 {
            backend
                .present(output.clone(), frame(1920, 1080, PixelFormat::Xrgb8888))
                .unwrap();
        }
        assert_eq!(backend.scanout_stats().allocations, 3);
        backend
            .present(output.clone(), frame(3840, 2400, PixelFormat::Xrgb8888))
            .unwrap();
        assert_eq!(backend.scanout_stats().allocations, 4);
    }

    /// 不支持的帧类别必须明确报错（不静默丢帧）；DMA-BUF 用**类型化**拒绝，
    /// 与真实后端一致 —— 调用方靠变体（不是字符串）判定"重开 CPU 捕获管线"。
    #[test]
    fn mock_rejects_non_cpu_frames() {
        let mut backend = MockDisplayBackend::with_default_output();
        let output = OutputId::new("HDMI-A-1");
        let dmabuf = DisplayFrame::DmaBuf(liuguang_core::types::DmaBufFrame {
            width: 16,
            height: 16,
            stride: 16,
            format: PixelFormat::Nv12,
            fds: vec![Arc::new(
                std::fs::File::open("/dev/null")
                    .expect("/dev/null 可打开")
                    .into(),
            )],
            object_index: [0; 4],
            keepalive: None,
            modifier: 0,
            fourcc: u32::from_le_bytes(*b"NV12"),
            strides: [16, 16, 0, 0],
            offsets: [0, 0, 0, 0],
        });
        let err = backend.present(output, dmabuf).unwrap_err();
        assert!(
            matches!(err, DisplayError::DmaBufRejected { .. }),
            "DMA-BUF 必须是类型化拒绝，实际: {err}"
        );
    }

    /// **能力与行为必须一致**：后端自称不支持 DMA-BUF 扫描输出时，提交 DMA-BUF 帧
    /// 只能得到类型化拒绝（`DmaBufRejected`），不能是 `Unsupported`/`Failed`——
    /// 后者会被调用方计入"提交失败"，连续几次就把视频误降级成静态图。
    #[test]
    fn advertised_capability_matches_rejection_variant() {
        let backend = MockDisplayBackend::with_default_output();
        assert!(
            !backend.supports_dmabuf_scanout(),
            "模拟后端不提供直通，能力位必须如实为 false"
        );
        assert!(
            !UnavailableBackend.supports_dmabuf_scanout(),
            "占位后端同样不得声称支持直通"
        );
    }
}
