//! 共享数据类型：输出、模式、帧与展示令牌。
//!
//! 帧类型（[`DisplayFrame`]）显式区分 CPU 内存、GPU 纹理与 DMA-BUF 三种来源，
//! 并携带 stride、像素格式与缓冲所有权信息；跨模块传递使用 `Arc` 共享，
//! 避免高分辨率像素数据被 `clone()` 复制。
//!
//! 注意：`present` 成功返回 [`PresentToken`] 并不意味着 KMS 已释放
//! framebuffer——真实 page-flip / atomic commit 的 in-flight 缓冲生命周期
//! 由 `liuguang-display`（M1 起）负责。

use std::sync::Arc;
use std::time::Duration;

/// 输出标识。首版使用 KMS connector 名称（如 `HDMI-A-1`）；
/// 多卡/多同名显示器的稳定 ID（设备路径 + EDID）留待 M6 前设计。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub String);

impl OutputId {
    /// 创建输出标识。
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// 返回名称字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OutputId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 像素格式。字节序均为**小端内存布局**（与 DRM fourcc 一致）：
///
/// - [`PixelFormat::Xrgb8888`] / [`PixelFormat::Argb8888`]：内存字节为
///   `[B, G, R, X/A]`（DRM `XR24`/`AR24`），用于扫描输出缓冲。
/// - [`PixelFormat::Rgba8888`]：内存字节为 `[R, G, B, A]`，
///   与 PNG 解码输出一致，用于解码中转缓冲。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// DRM XR24：内存 `[B, G, R, X]`。
    Xrgb8888,
    /// DRM AR24：内存 `[B, G, R, A]`。
    Argb8888,
    /// PNG 直出：内存 `[R, G, B, A]`。
    Rgba8888,
    /// NV12（YUV 4:2:0 半平面）：仅 DMA-BUF 传输语义，
    /// CPU 光栅化路径不处理（交由显示平面硬件采样）。
    Nv12,
}

impl PixelFormat {
    /// 每像素字节数。
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            // NV12 的 Y 平面为 1 字节/像素；UV 平面使总需求为 1.5×
            // （见 CpuFrame::validate 的格式感知校验）。
            Self::Nv12 => 1,
            _ => 4,
        }
    }

    /// 该格式的缓冲需求是否按 1.5×（NV12 半平面）。
    pub const fn is_semi_planar(self) -> bool {
        matches!(self, Self::Nv12)
    }
}

/// 显示模式（分辨率 + 刷新率）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayMode {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 刷新率，单位毫赫兹（mHz，如 60 Hz = 60000）。
    pub refresh_millihz: u32,
}

impl DisplayMode {
    /// 便捷构造：以 Hz 浮点指定刷新率。
    pub fn from_hz(width: u32, height: u32, refresh_hz: f64) -> Self {
        Self {
            width,
            height,
            refresh_millihz: (refresh_hz * 1000.0).round() as u32,
        }
    }

    /// 刷新率（Hz）。
    pub fn refresh_hz(self) -> f64 {
        f64::from(self.refresh_millihz) / 1000.0
    }
}

/// 物理输出的静态信息（由 `liuguang-display` 枚举后填充）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// 输出标识。
    pub id: OutputId,
    /// connector 是否有显示器连接。
    pub connected: bool,
    /// 该输出支持的全部模式。
    pub modes: Vec<DisplayMode>,
    /// 当前生效模式（未完成 modeset 时为 `None`）。
    pub current_mode: Option<DisplayMode>,
    /// 物理尺寸（宽、高，毫米），EDID 不提供时为 `None`。
    pub physical_size_mm: Option<(u16, u16)>,
}

impl OutputInfo {
    /// 是否支持给定模式。
    pub fn supports_mode(&self, mode: DisplayMode) -> bool {
        self.modes.contains(&mode)
    }
}

/// CPU 内存中的像素缓冲。`data` 以 `Arc` 共享，`clone` 只增加引用计数。
#[derive(Debug, Clone)]
pub struct CpuFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 行跨度（字节/行），可大于 `width * bytes_per_pixel`（对齐填充）。
    pub stride: u32,
    /// 像素格式。
    pub format: PixelFormat,
    /// 像素数据，长度至少为 `stride * height`。
    pub data: Arc<[u8]>,
}

impl CpuFrame {
    /// 校验 stride 与数据长度是否自洽。
    pub fn validate(&self) -> Result<(), FrameError> {
        if self.width == 0 || self.height == 0 {
            return Err(FrameError::ZeroDimension);
        }
        let min_stride = self
            .width
            .checked_mul(self.format.bytes_per_pixel())
            .ok_or(FrameError::StrideTooSmall {
                stride: self.stride,
                min_stride: u32::MAX,
            })?;
        if self.stride < min_stride {
            return Err(FrameError::StrideTooSmall {
                stride: self.stride,
                min_stride,
            });
        }
        let mut needed = u64::from(self.stride).saturating_mul(u64::from(self.height));
        if self.format.is_semi_planar() {
            // NV12：Y 平面 + 半高 UV 平面（UV 跨度约定等于 Y 跨度）。
            needed = needed.saturating_mul(3) / 2;
        }
        if (self.data.len() as u64) < needed {
            return Err(FrameError::DataTooShort {
                have: self.data.len() as u64,
                need: needed,
            });
        }
        Ok(())
    }
}

/// GPU 纹理帧句柄（M4 渲染路径启用）。纹理生命周期由持有该句柄的渲染上下文管理。
#[derive(Debug, Clone)]
pub struct GpuFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 像素格式。
    pub format: PixelFormat,
    /// 渲染上下文内的纹理标识。
    pub texture_id: u64,
}

/// 帧的**寿命延长器**：把解码器/生产者必须保活的资源（例如 ffmpeg 的
/// `AVFrame`、GStreamer 的 `Sample`）挂在帧上，随最后一个 `Arc` 副本一起释放。
///
/// 用途：DMA-BUF 的 fd 只是"内存的句柄"，**内存本身仍由生产者持有**。
/// 如果生产者在我们上屏之前就把那块表面复用去解码下一帧，合成器会读到撕裂
/// 或错帧。所以"帧"必须能把生产者的所有权一起带出去，直到合成器释放为止
/// （Wayland 侧是 `wl_buffer.release`，DRM 侧是 page-flip 完成）。
#[derive(Clone)]
pub struct FrameKeepalive(Arc<dyn std::any::Any + Send + Sync>);

impl FrameKeepalive {
    /// 包装一个需要随帧保活的资源（`AVFrame` 包装体、`gst::Sample` 等）。
    pub fn new<T: std::any::Any + Send + Sync>(resource: T) -> Self {
        Self(Arc::new(resource))
    }

    /// 从已存在的 `Arc` 构造（共享同一块资源的多帧场景）。
    pub fn from_arc(resource: Arc<dyn std::any::Any + Send + Sync>) -> Self {
        Self(resource)
    }

    /// 底层资源的引用计数（测试用于断言"帧还活着"）。
    pub fn strong_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }
}

impl std::fmt::Debug for FrameKeepalive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameKeepalive(..)")
    }
}

/// DMA-BUF 帧。fd 以 `Arc<OwnedFd>` 表达共享所有权：clone 只增加引用计数，
/// 最后一个持有者释放时关闭描述符。
///
/// **一个 DMA-BUF 帧可能由多个 DRM object 组成**（每个 object 一个 fd）：
/// Intel iHD 导出的 NV12 通常是单 object（UV 用偏移表达），而 NVIDIA 的
/// `vaExportSurfaceHandle(SEPARATE_LAYERS)` 会把 Y 与 UV 放在**两个 object、
/// 两个 fd** 里，两者带同一个 modifier。因此平面到 object 的映射必须显式携带
/// （[`DmaBufFrame::object_index`]），显示后端按 `object_index[plane]` 取 fd。
#[derive(Debug, Clone)]
pub struct DmaBufFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// 主平面行跨度（字节/行）。等价于 `strides[0]`，保留以便旧代码/日志使用。
    pub stride: u32,
    /// 像素格式。
    pub format: PixelFormat,
    /// 每个 DRM object 的 DMA-BUF 文件描述符（至少 1 个）。
    pub fds: Vec<Arc<std::os::fd::OwnedFd>>,
    /// DRM modifier（如 i915 Y_TILED）；0 = LINEAR。
    pub modifier: u64,
    /// DRM fourcc（如 NV12 = 0x3231564E）。
    pub fourcc: u32,
    /// 各平面行跨度（最多 4 平面；NV12 用 [0]/[1]）。
    pub strides: [u32; 4],
    /// 各平面偏移（NV12 UV 平面偏移）。
    pub offsets: [u32; 4],
    /// 每个平面归属哪个 object（下标进 [`Self::fds`]）；单 object 帧全为 0。
    pub object_index: [u8; 4],
    /// 可选的寿命延长器（见 [`FrameKeepalive`]）。
    pub keepalive: Option<FrameKeepalive>,
}

impl DmaBufFrame {
    /// 单 object 帧（最常见的 DRM/i915 与 wl_shm 场景）。
    // 构造器参数就是"一个平面描述"的字段，拆成 builder 只会让调用点更长。
    #[allow(clippy::too_many_arguments)]
    pub fn single_object(
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        fd: Arc<std::os::fd::OwnedFd>,
        modifier: u64,
        fourcc: u32,
        strides: [u32; 4],
        offsets: [u32; 4],
    ) -> Self {
        Self {
            width,
            height,
            stride,
            format,
            fds: vec![fd],
            modifier,
            fourcc,
            strides,
            offsets,
            object_index: [0; 4],
            keepalive: None,
        }
    }

    /// 主 fd（object 0）。显示后端的"单 object 快路径"用。
    pub fn primary_fd(&self) -> &std::os::fd::OwnedFd {
        // 构造时保证非空：`fds` 至少一个元素（见 `single_object` 与各生产者）。
        &self.fds[0]
    }

    /// 取某个平面所属 object 的 fd。
    pub fn fd_for_plane(&self, plane: usize) -> Option<&std::os::fd::OwnedFd> {
        let object = usize::from(*self.object_index.get(plane)?);
        self.fds.get(object).map(|fd| fd.as_ref())
    }

    /// object 数量（≥1）。
    pub fn object_count(&self) -> usize {
        self.fds.len()
    }

    /// 是否多 object（NVIDIA SEPARATE_LAYERS 的典型形态）。
    pub fn is_multi_object(&self) -> bool {
        self.fds.len() > 1
    }
}

/// 帧来源类别（用于可观测状态，见规范 §6.4「GPU 导入模式」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    /// CPU 内存缓冲（上传/扫描）。
    Cpu,
    /// GPU 纹理。
    GpuTexture,
    /// DMA-BUF 导入。
    DmaBuf,
}

/// 交给显示层的完整帧，显式标注存储类别与所有权。
#[derive(Debug, Clone)]
pub enum DisplayFrame {
    /// CPU 像素缓冲。
    Cpu(CpuFrame),
    /// GPU 纹理帧。
    GpuTexture(GpuFrame),
    /// DMA-BUF 帧。
    DmaBuf(DmaBufFrame),
}

impl DisplayFrame {
    /// 帧宽。
    pub fn width(&self) -> u32 {
        match self {
            Self::Cpu(f) => f.width,
            Self::GpuTexture(f) => f.width,
            Self::DmaBuf(f) => f.width,
        }
    }

    /// 帧高。
    pub fn height(&self) -> u32 {
        match self {
            Self::Cpu(f) => f.height,
            Self::GpuTexture(f) => f.height,
            Self::DmaBuf(f) => f.height,
        }
    }

    /// 像素格式。
    pub fn format(&self) -> PixelFormat {
        match self {
            Self::Cpu(f) => f.format,
            Self::GpuTexture(f) => f.format,
            Self::DmaBuf(f) => f.format,
        }
    }

    /// 存储类别。
    pub fn kind(&self) -> FrameKind {
        match self {
            Self::Cpu(_) => FrameKind::Cpu,
            Self::GpuTexture(_) => FrameKind::GpuTexture,
            Self::DmaBuf(_) => FrameKind::DmaBuf,
        }
    }
}

/// 帧结构自检错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    /// 宽或高为 0。
    #[error("帧宽高必须大于 0")]
    ZeroDimension,
    /// stride 小于每行最小字节数。
    #[error("stride {stride} 小于最小值 {min_stride}")]
    StrideTooSmall {
        /// 实际 stride。
        stride: u32,
        /// 需要的最小 stride。
        min_stride: u32,
    },
    /// 像素数据长度不足。
    #[error("帧数据长度 {have} 不足，需要 {need}")]
    DataTooShort {
        /// 实际长度。
        have: u64,
        /// 需要长度。
        need: u64,
    },
    /// 像素格式不支持该操作。
    #[error("不支持的像素格式: {0:?}")]
    UnsupportedFormat(PixelFormat),
}

/// `present` 的受理令牌。返回它只代表提交已受理；
/// 真正的扫描完成以 page-flip 事件为准（M1 起实现事件回收）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PresentToken {
    /// 目标输出。
    pub output: OutputId,
    /// 提交序列号，用于对应回收 in-flight 缓冲。
    pub sequence: u64,
}

/// 媒体种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// 静态图片（PNG）。
    Image,
    /// 视频（MP4）。
    Video,
}

/// 媒体源元数据（见规范 §6.4 可观测状态）。
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    /// 种类。
    pub kind: MediaKind,
    /// 媒体宽（像素）。
    pub width: u32,
    /// 媒体高（像素）。
    pub height: u32,
    /// 时长（静态图片为 `None`）。
    pub duration: Option<Duration>,
    /// 源帧率（静态图片为 `None`）。
    pub source_fps: Option<f64>,
    /// 编码标签（如 `h264`），未知时为 `None`。
    pub codec: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::os::fd::OwnedFd;

    fn devnull() -> Arc<OwnedFd> {
        Arc::new(OwnedFd::from(std::fs::File::open("/dev/null").unwrap()))
    }

    /// 单 object 帧（i915/iHD 形态）：所有平面共用 fd 0。
    #[test]
    fn single_object_frame_maps_every_plane_to_object_zero() {
        let frame = DmaBufFrame::single_object(
            1920,
            1080,
            1920,
            PixelFormat::Nv12,
            devnull(),
            0,
            0x3231_564E,
            [1920, 1920, 0, 0],
            [0, 1920 * 1080, 0, 0],
        );
        assert_eq!(frame.object_count(), 1);
        assert!(!frame.is_multi_object());
        assert!(std::ptr::eq(
            frame.fd_for_plane(0).unwrap(),
            frame.primary_fd()
        ));
        assert!(std::ptr::eq(
            frame.fd_for_plane(1).unwrap(),
            frame.primary_fd()
        ));
        assert_eq!(frame.object_index, [0; 4]);
    }

    /// 多 object 帧（NVIDIA SEPARATE_LAYERS 形态）：Y 在 object 0、UV 在 object 1，
    /// 两个 fd 必须分别取到——这是"跨平面共用 fd"假设被打破的地方。
    #[test]
    fn multi_object_frame_maps_planes_to_their_own_fd() {
        let y = devnull();
        let uv = devnull();
        let frame = DmaBufFrame {
            width: 1920,
            height: 1200,
            stride: 1920,
            format: PixelFormat::Nv12,
            fds: vec![y.clone(), uv.clone()],
            modifier: 0x0300_0000_0060_6014,
            fourcc: 0x3231_564E,
            strides: [1920, 1920, 0, 0],
            offsets: [0, 0, 0, 0],
            object_index: [0, 1, 0, 0],
            keepalive: None,
        };
        assert_eq!(frame.object_count(), 2);
        assert!(frame.is_multi_object());
        assert!(std::ptr::eq(frame.fd_for_plane(0).unwrap(), y.as_ref()));
        assert!(std::ptr::eq(frame.fd_for_plane(1).unwrap(), uv.as_ref()));
        // 平面 2 归属 object 0（I420 的 V 平面在同 object 内用偏移表达）。
        assert!(std::ptr::eq(frame.fd_for_plane(2).unwrap(), y.as_ref()));
        assert!(std::ptr::eq(frame.fd_for_plane(3).unwrap(), y.as_ref()));
    }

    /// object_index 指向不存在的 object 时必须返回 None（而不是 panic / 取到错 fd）。
    #[test]
    fn plane_pointing_at_missing_object_is_none() {
        let frame = DmaBufFrame::single_object(
            4,
            4,
            16,
            PixelFormat::Nv12,
            devnull(),
            0,
            0x3231_564E,
            [16, 16, 0, 0],
            [0, 16, 0, 0],
        );
        let mut broken = frame;
        broken.object_index = [0, 3, 0, 0];
        assert!(broken.fd_for_plane(1).is_none());
    }

    /// 寿命延长器：clone 帧共享同一份资源，最后一个副本释放才销毁——
    /// 这正是"解码表面在合成器 release 之前不能被复用"的保证。
    #[test]
    fn keepalive_lives_as_long_as_any_frame_clone() {
        struct Surface;
        let frame = DmaBufFrame {
            keepalive: Some(FrameKeepalive::new(Surface)),
            ..DmaBufFrame::single_object(
                4,
                4,
                16,
                PixelFormat::Xrgb8888,
                devnull(),
                0,
                0,
                [16, 0, 0, 0],
                [0, 0, 0, 0],
            )
        };
        let keepalive = frame.keepalive.as_ref().unwrap();
        assert_eq!(keepalive.strong_count(), 1);
        let cloned = frame.clone();
        assert_eq!(keepalive.strong_count(), 2);
        drop(cloned);
        assert_eq!(keepalive.strong_count(), 1);
    }
}
