//! 硬件兼容性分类（t15）：这个素材本机能不能直接播？
//!
//! 矩阵复核暴露的两类"解不动"素材：
//!
//! - **8K H.264**：NVDEC 的 H.264 上限是 4096 像素宽，7680 宽只能软解，
//!   实测 0.086 fps、CPU 170%+、RSS 2.9GB —— 客观上没法当壁纸播。
//! - **10bit HEVC（Main10 / P010）**：我方解码后的 RGB 转换链只认 8bit NV12，
//!   VA-API 表面是 P010 时会被明确拒绝（见 `liuguang-vaapi` 的 t6 改动）。
//!
//! t6 只把这两类"产品化"（告警 + 可操作错误）。t15 更进一步：**自动解决** ——
//! 把它们转码成可硬解的 8bit HEVC 缓存再播（转码器见 [`crate::transcode`]）。
//!
//! 本模块只做**判定**，且判定是纯函数（[`classify`]），硬件能力由
//! [`HardwareCaps`] 显式传入 —— 这样判据表可以脱离本机硬件跑单测。
//!
//! ## 判据
//!
//! | 输入 | 结论 |
//! |------|------|
//! | 无 `details`（探测失败/格式未知） | `Playable`（乐观，保持 t15 之前的行为） |
//! | 位深 > 8（`pix_fmt` 带 10/12，或容器头 hvcC/avcC/av1C 读出 >8） | `NeedsConversion`（8bit） |
//! | H.264 且宽 > 4096（NVDEC 上限） | `NeedsConversion`（HEVC 8bit） |
//! | HEVC / AV1 / VP9 且宽 ≤ 8K 且 8bit | `Playable`（AV1 还要看节点有没有 AV1 硬解） |
//! | 宽 > 8192（本机所有硬解上限之外） | `Unsupported`（转码也救不了，建议换素材） |

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use liuguang_ipc::protocol::{CompatInfo, CompatState, MediaDetails};

/// 判位深时读的窗口大小：容器头（moov/EBML Info）通常在前 4MiB；
/// 也有把 moov 写在文件尾的（未 faststart），所以尾部也读一份。
const DEPTH_WINDOW: u64 = 4 * 1024 * 1024;
/// 本机所有硬解路径能接受的最大宽度（NVDEC/VA-API 的 8K 边界）。
pub const MAX_HARDWARE_WIDTH: u32 = 8192;
/// NVDEC 的 H.264 解码上限（像素宽）。
pub const H264_HARDWARE_MAX_WIDTH: u32 = 4096;

/// 本机解码能力（用于 [`classify`]）。
///
/// 默认值按"这颗机器确实有的能力"取（两张卡都有 NVDEC/VA-API）：H.264 到 4K、
/// HEVC 到 8K、AV1 硬解。`av1_hardware` 由 [`HardwareCaps::detect`] 用 `vainfo`
/// 探（两台 GPU 只要有一台宣告 `AV1Profile0 + VLD` 就算有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareCaps {
    /// H.264 硬解的最大宽度（像素）。
    pub h264_max_width: u32,
    /// HEVC 硬解的最大宽度（像素）。
    pub hevc_max_width: u32,
    /// 本机是否有 AV1 硬解（`vainfo` 里出现 `VAProfileAV1Profile0` + `VAEntrypointVLD`）。
    pub av1_hardware: bool,
    /// 转码输出宽度上限（`[media].transcode_max_width`；0 = 保持原分辨率）。
    pub transcode_max_width: u32,
    /// 实际运行的解码路径（决定转码产物的容器/编码，t26）。
    pub consumer: DecodeConsumer,
}

impl Default for HardwareCaps {
    fn default() -> Self {
        Self {
            h264_max_width: H264_HARDWARE_MAX_WIDTH,
            hevc_max_width: MAX_HARDWARE_WIDTH,
            av1_hardware: true,
            transcode_max_width: 0,
            // 默认按 VA-API 路径（ffmpeg）取：单测与"能力未知"时最宽松，
            // 也保持 0.2.0-17 的 `target` 形状不变。真实环境由 `detect` 决定。
            consumer: DecodeConsumer::Vaapi,
        }
    }
}

impl HardwareCaps {
    /// 探测本机能力。
    ///
    /// 只做一件外部调用：`vainfo`（找不到或失败时**乐观**认为有 AV1 硬解 ——
    /// 判成 `NeedsConversion` 会白转一份缓存，判成 `Playable` 最坏是走 t6 的
    /// 明确拒绝，两者都不算误伤）。H.264/HEVC 的宽度上限是 NVDEC 的固定规格，
    /// 不探测（探测只会更脆弱）。
    pub fn detect(transcode_max_width: u32) -> Self {
        let consumer = detect_consumer();
        tracing::debug!(
            consumer = consumer.as_str(),
            "解码路径已确定（决定转码产物格式）"
        );
        Self {
            transcode_max_width,
            av1_hardware: detect_av1_hardware(),
            consumer,
            ..Self::default()
        }
    }

    /// 把"实际解码路径"钉死（生产用它按**输出**改写判定，见 t33）。
    ///
    /// 构造期探测的是"这台机器大体上用什么路径"，但同一个进程里不同的输出、不同的
    /// 时刻可能落到不同路径上（`[media].video_vaapi = "off"`、后端不支持 DMA-BUF
    /// 扫描输出、VA-API 首帧被合成器否掉……）。分类器必须按**这次真正会用的路径**
    /// 判，否则会出现"判 playable、真播时 qtdemux 报 `atom has bogus size`"。
    pub fn with_consumer(mut self, consumer: DecodeConsumer) -> Self {
        self.consumer = consumer;
        self
    }

    /// 单测用：把 AV1 能力钉死，避免测试结果取决于本机显卡。
    #[cfg(test)]
    pub fn with_av1_hardware(mut self, av1_hardware: bool) -> Self {
        self.av1_hardware = av1_hardware;
        self
    }
}

/// `vainfo` 里有没有 AV1 解码入口。
fn detect_av1_hardware() -> bool {
    let output = run_with_timeout("vainfo", &[], Duration::from_secs(3));
    let Some(text) = output else {
        tracing::debug!("没有 vainfo（或执行失败），乐观按「有 AV1 硬解」处理");
        return true;
    };
    let has_profile = text.contains("VAProfileAV1Profile0");
    let has_entrypoint = text.contains("VAEntrypointVLD");
    if !has_profile || !has_entrypoint {
        tracing::info!(
            has_profile,
            "vainfo 没有宣告 AV1 解码能力：AV1 素材将被转码"
        );
    }
    has_profile && has_entrypoint
}

/// 跑一条外部命令，拿到合并后的 stdout+stderr（超时/启动失败 ⇒ `None`）。
///
/// 用 `wait_timeout` 式的轮询实现（不引依赖）：子进程短命（vainfo 几百毫秒），
/// 轮询 50ms 足够。
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    use std::io::Read as _;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    const MAX_OUTPUT_BYTES: u64 = 256 * 1024;
    let out_reader = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.take(MAX_OUTPUT_BYTES).read_to_end(&mut buf);
            buf
        })
    });
    let err_reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.take(MAX_OUTPUT_BYTES).read_to_end(&mut buf);
            buf
        })
    });
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(h) = out_reader {
                    let _ = h.join();
                }
                if let Some(h) = err_reader {
                    let _ = h.join();
                }
                tracing::warn!(program, "命令超时，已终止");
                return None;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(h) = out_reader {
                    let _ = h.join();
                }
                if let Some(h) = err_reader {
                    let _ = h.join();
                }
                tracing::warn!(program, %error, "等待命令失败");
                return None;
            }
        }
    }
    let stdout = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&stderr));
    Some(text)
}

/// 转码产物的容器。
///
/// **跟着解码路径走**（t26）：VA-API 路径用 ffmpeg 解封装，MKV/HEVC 都能吃；
/// 纯 GStreamer 构型的管线是写死的 `filesrc ! qtdemux ! h264parse ! <h264 解码器>`
/// （见 `crates/liuguang-media/src/gstreamer.rs`），只认 **MP4 + H.264** ——
/// 给它一个 `.mkv` 会被 `qtdemux` 报 `atom has bogus size`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// Matroska（`.mkv`）。
    Mkv,
    /// MPEG-4 Part 14（`.mp4`）。
    Mp4,
}

impl Container {
    /// 文件扩展名（不带点）。
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mkv => "mkv",
            Self::Mp4 => "mp4",
        }
    }

    /// 线上/日志里的名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mkv => "mkv",
            Self::Mp4 => "mp4",
        }
    }

    /// 面向人的容器族名字（家族内的扩展名都算进来，`.webm` 也说得清）。
    pub fn describe_zh(self) -> &'static str {
        match self {
            Self::Mkv => "Matroska（mkv/webm）",
            Self::Mp4 => "MP4（mp4/m4v/mov）",
        }
    }

    /// ffmpeg 的 `-f` 封装器名。
    pub fn muxer(self) -> &'static str {
        match self {
            Self::Mkv => "matroska",
            Self::Mp4 => "mp4",
        }
    }

    /// 从扩展名反推（缓存 key 里带着容器，读回来要用）。
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "mkv" => Some(Self::Mkv),
            "mp4" => Some(Self::Mp4),
            _ => None,
        }
    }

    /// 从**源文件**路径推断它的容器（判"解码路径打不打得开它"，见 t30 F1）。
    ///
    /// qtdemux 认的是 ISO-BMFF：`.mp4` / `.m4v` / `.mov` 都算 `Mp4`；
    /// Matroska 家族（`.mkv` / `.webm`）才算 `Mkv`。不认识的扩展名返回 `None`
    /// （调用方按"容器未知"保守处理）。
    pub fn from_source_path(path: &Path) -> Option<Self> {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())?
            .to_ascii_lowercase();
        match extension.as_str() {
            "mp4" | "m4v" | "mov" => Some(Self::Mp4),
            "mkv" | "webm" => Some(Self::Mkv),
            _ => None,
        }
    }
}

/// **实际运行的解码路径**（t26）：转码产物必须能被它打开。
///
/// 同一个二进制在不同环境里可用的解码器不一样，所以产物格式不能写死：
///
/// - `Vaapi`：`liuguang-vaapi` 的 ffmpeg 解封装 + 硬件解码，吃 MKV/HEVC；
/// - `GStreamer`：写死的 `filesrc ! qtdemux ! h264parse ! <h264 解码器>`，
///   只吃 MP4/H.264（`scripts/build-release.sh` 打的包就是这种构型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeConsumer {
    /// VA-API / ffmpeg 解码路径。
    Vaapi,
    /// 纯 GStreamer 解码路径。
    GStreamer,
}

impl DecodeConsumer {
    /// 日志里的名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vaapi => "vaapi",
            Self::GStreamer => "gstreamer",
        }
    }

    /// 这条路径能吃的容器。
    pub fn container(self) -> Container {
        match self {
            Self::Vaapi => Container::Mkv,
            Self::GStreamer => Container::Mp4,
        }
    }

    /// 这条路径能吃的视频编码。
    pub fn codec(self) -> &'static str {
        match self {
            Self::Vaapi => "hevc",
            Self::GStreamer => "h264",
        }
    }

    /// 能力判定：**源**素材（容器 + 编码）能被这条解码路径打开吗？
    ///
    /// 这是分类器的输入之一（t30 F1）：判"能不能播"不能只看编码，还得看容器——
    /// 纯 GStreamer 构型的管线写死 `filesrc ! qtdemux ! h264parse ! …`，
    /// 它只认 ISO-BMFF（mp4/m4v/mov）+ H.264；给它一个 `.mkv` 会在开屏时炸成
    /// `qtdemux … atom has bogus size`，用户看到的是 `UNSUPPORTED_CODEC`。
    /// VA-API 路径用我们自己的 ffmpeg 解封装，容器无关（编码由 ffmpeg 决定）。
    pub fn can_open(self, container: Container, codec: &str) -> bool {
        match self {
            Self::Vaapi => matches!(container, Container::Mkv | Container::Mp4),
            Self::GStreamer => container == Container::Mp4 && codec.eq_ignore_ascii_case("h264"),
        }
    }

    /// 这个输出**此刻真正会用**的解码路径（纯函数，判定表与播放器共用同一份规则）。
    ///
    /// 构造期探测（[`detect_consumer`]）只看"这台机器大体上能不能硬解"；但运行时
    /// 还有几条路会把它推翻（t33，现场见 t28 F2：vaapi 构型 + `video_vaapi = "off"`，
    /// 判 playable 的 `.mkv` 在开屏时炸成 `qtdemux … atom has bogus size`）：
    ///
    /// - 构建里没有 `vaapi` feature ⇒ 只有 GStreamer；
    /// - `LIUGUANG_VIDEO_VAAPI` / `[media].video_vaapi` 写成 `off`（或 `0/false/no/none`）
    ///   ⇒ 用户显式关掉了零拷贝，播放走 GStreamer；
    /// - 显示后端不支持 DMA-BUF 扫描输出（wl_shm 等）⇒ VA-API 生产者直接被跳过；
    /// - 否则用探测结果（设备打不开时它已经是 `GStreamer`）。
    ///
    /// 判据必须与 `daemon::open_vaapi_producer` 的开头一致：**判 playable 的那条路，
    /// 必须就是待会儿真去开管线的那条路**。
    pub fn for_playback(
        vaapi_compiled: bool,
        vaapi_enabled: bool,
        dmabuf_scanout: bool,
        probed: DecodeConsumer,
    ) -> Self {
        if vaapi_compiled && vaapi_enabled && dmabuf_scanout {
            probed
        } else {
            Self::GStreamer
        }
    }
}

/// 探测本机**实际**的解码路径（决定转码产物格式，t26）。
///
/// - 不带 `vaapi` feature 的构建（打包默认）⇒ 只剩 GStreamer；
/// - 带 `vaapi` feature 但设备打不开（驱动缺失/权限/无卡）⇒ 运行时也会回落到
///   GStreamer，所以产物同样按 GStreamer 的要求生成。
pub fn detect_consumer() -> DecodeConsumer {
    // 单测不依赖本机显卡（CI/开发机/无卡机器必须跑出同一份判据表）；
    // 真实的"产物跟着解码路径走"由 `select_consumer` 的单测 + e2e 的沙箱实测覆盖。
    if cfg!(test) {
        return DecodeConsumer::Vaapi;
    }
    let vaapi_compiled = cfg!(feature = "vaapi");
    #[cfg(feature = "vaapi")]
    let device_opens = vaapi_device_opens();
    #[cfg(not(feature = "vaapi"))]
    let device_opens = false;
    let consumer = select_consumer(vaapi_compiled, device_opens);
    if consumer == DecodeConsumer::GStreamer && vaapi_compiled {
        tracing::warn!("VA-API 设备打不开：转码产物改为 H.264 + MP4（匹配 GStreamer 管线的能力）");
    }
    consumer
}

/// 选解码路径的纯函数（单测用；真实调用见 [`detect_consumer`]）。
///
/// - 没编 `vaapi` feature ⇒ 只有 GStreamer；
/// - 编了但设备打不开（驱动/权限/无卡）⇒ 运行时也会回落，按 GStreamer 生成产物。
pub fn select_consumer(vaapi_compiled: bool, device_opens: bool) -> DecodeConsumer {
    if vaapi_compiled && device_opens {
        DecodeConsumer::Vaapi
    } else {
        DecodeConsumer::GStreamer
    }
}

/// 试着打开一个 VA-API 渲染节点（几百微秒，构造期只跑一次）。
#[cfg(feature = "vaapi")]
fn vaapi_device_opens() -> bool {
    const NODES: [&str; 3] = [
        "/dev/dri/renderD128",
        "/dev/dri/renderD129",
        "/dev/dri/renderD130",
    ];
    NODES.iter().any(|node| {
        let opened = liuguang_vaapi::VaDevice::open(std::path::Path::new(node));
        match opened {
            Ok(_device) => {
                tracing::debug!(node, "VA-API 设备可用");
                true
            }
            Err(error) => {
                tracing::debug!(node, %error, "VA-API 节点打不开");
                false
            }
        }
    })
}

/// 转码目标：把素材变成什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionTarget {
    /// 目标编码（`hevc` 给 VA-API 路径，`h264` 给纯 GStreamer 构型）。
    pub codec: &'static str,
    /// 目标位深（恒 8：我方 RGB 转换链只认 8bit NV12）。
    pub bit_depth: u8,
    /// 输出宽度（0 = 保持源分辨率；非 0 时等比缩到该宽度）。
    pub scale_width: u32,
    /// 目标容器（跟着解码路径走，见 [`DecodeConsumer`]）。
    pub container: Container,
}

impl ConversionTarget {
    /// 面向人的目标描述（进 `CompatInfo.target` 与错误文案）。
    ///
    /// 只写编码 + 位深 + 容器；4:2:0 是转码管线的固定行为，写在 `reason` 里。
    /// VA-API 目标保持 0.2.0-17 的形状（`"hevc 8bit"`），避免既有消费者
    /// （GUI/TUI/脚本/单测）的字符串比较莫名其妙地失效；只有新出现的 MP4 目标
    /// 带容器后缀（`"h264 8bit (mp4)"`）。
    pub fn label(&self) -> String {
        let mut label = format!("{} {}bit", self.codec, self.bit_depth);
        if self.scale_width != 0 {
            label.push_str(&format!(" ≤{}px", self.scale_width));
        }
        if self.container != Container::Mkv {
            label.push_str(&format!(" ({})", self.container.as_str()));
        }
        label
    }

    /// 错误/`reason` 里用的中文描述（"8bit HEVC 4:2:0 缓存" 这种）。
    pub fn describe_zh(&self) -> String {
        format!(
            "{}bit {} 4:2:0 {} 缓存",
            self.bit_depth,
            self.codec.to_ascii_uppercase(),
            self.container.as_str().to_ascii_uppercase()
        )
    }
}

/// 本机构建/环境决定的目标（判据表里所有 `NeedsConversion` 共用）。
///
/// `source_width`: 源宽度，用于在 GStreamer 构型下判断是否需要缩到 H.264 的上限。
fn conversion_target(caps: &HardwareCaps, source_width: u32) -> ConversionTarget {
    match caps.consumer {
        DecodeConsumer::Vaapi => ConversionTarget {
            codec: caps.consumer.codec(),
            bit_depth: 8,
            scale_width: 0,
            container: caps.consumer.container(),
        },
        DecodeConsumer::GStreamer => {
            // H.264 只到 4K（NVDEC 上限，也是软解能接受的上限）；配置给了更小的
            // 上限就听配置的。源本来就窄就不缩放（`scale_width = 0`）。
            let limit = if caps.transcode_max_width == 0 {
                H264_HARDWARE_MAX_WIDTH
            } else {
                caps.transcode_max_width.min(H264_HARDWARE_MAX_WIDTH)
            };
            ConversionTarget {
                codec: caps.consumer.codec(),
                bit_depth: 8,
                scale_width: if source_width > limit { limit } else { 0 },
                container: caps.consumer.container(),
            }
        }
    }
}

/// 兼容性结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    /// 本机可以直接播。
    Playable,
    /// 硬解不了，但转码后能播：`reason` 给用户看，`target` 是转码目标。
    NeedsConversion {
        /// 为什么需要转换（中文、可操作）。
        reason: String,
        /// 转成什么。
        target: ConversionTarget,
    },
    /// 连转码也救不了。
    Unsupported {
        /// 为什么不可用（中文、可操作）。
        reason: String,
    },
}

impl Compatibility {
    /// 转成 IPC 的 `compat` 字段。
    pub fn to_info(&self) -> CompatInfo {
        match self {
            Self::Playable => CompatInfo {
                state: CompatState::Playable,
                reason: None,
                target: None,
            },
            Self::NeedsConversion { reason, target } => CompatInfo {
                state: CompatState::NeedsConversion,
                reason: Some(reason.clone()),
                target: Some(target.label()),
            },
            Self::Unsupported { reason } => CompatInfo {
                state: CompatState::Unsupported,
                reason: Some(reason.clone()),
                target: None,
            },
        }
    }

    /// 需要转码吗。
    #[cfg(test)]
    pub fn needs_conversion(&self) -> bool {
        matches!(self, Self::NeedsConversion { .. })
    }

    /// 转码目标（只有 `NeedsConversion` 有）。
    #[cfg(test)]
    pub fn target(&self) -> Option<&ConversionTarget> {
        match self {
            Self::NeedsConversion { target, .. } => Some(target),
            _ => None,
        }
    }
}

/// 色度抽样（chroma subsampling）。
///
/// 位深与 chroma 是**两个独立维度**：8bit 也可能是 4:2:2 / 4:4:4，而 NVDEC/VA-API 的
/// H.264/HEVC 硬解只吃 4:2:0（HEVC 还允许 8/10bit）。t15 只按位深判断，于是 8bit 4:2:2
/// 被当成能播，实际开屏会掉到 `software:avdec_h264` 静默软解——t21 的 F2/F3。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:2:0：唯一能直接硬解的抽样。
    C420,
    /// 4:2:2：硬解不支持，必须转码。
    C422,
    /// 4:4:4：硬解不支持，必须转码。
    C444,
    /// 4:0:0（单色）：硬解不支持，必须转码。
    Mono,
    /// 容器头没写或解析不出来。
    Unknown,
}

impl Chroma {
    /// 面向人的写法（进错误文案）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::C420 => "4:2:0",
            Self::C422 => "4:2:2",
            Self::C444 => "4:4:4",
            Self::Mono => "4:0:0（单色）",
            Self::Unknown => "未知",
        }
    }

    /// 能否直接走硬解：**只有确认的 4:2:0**。
    ///
    /// 以前 `Unknown` 也算乐观（"绝大多数 8bit H.264/HEVC 就是 4:2:0"），t29 F2 的复核
    /// 证明这条乐观是错的：Matroska 只说位深不说色度，于是 8bit 4:2:2 的 `.mkv` 被判成
    /// `Playable`，真去播就掉进软解（`software:avdec_h264`）—— 正是 t21 的红线。
    /// 现在未知色度一律不乐观，要么转码要么明确拒绝。
    pub fn is_hardware_friendly(self) -> bool {
        matches!(self, Self::C420)
    }

    /// H.264 的 `chroma_format_idc` / HEVC 的 `chromaFormat` 都是这套编号。
    fn from_chroma_format_idc(idc: u64) -> Self {
        match idc {
            0 => Self::Mono,
            1 => Self::C420,
            2 => Self::C422,
            3 => Self::C444,
            _ => Self::Unknown,
        }
    }
}

/// 源格式：位深 + chroma 分开表达（不再用一个 `pix_fmt` 字符串兼表两件事）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFormat {
    /// 位深（8/10/12/16），未知为 `None`。
    pub depth: Option<u8>,
    /// 色度抽样。
    pub chroma: Chroma,
}

impl SourceFormat {
    /// 全未知。
    pub const UNKNOWN: Self = Self {
        depth: None,
        chroma: Chroma::Unknown,
    };

    /// 从 `pix_fmt` 名字推断（`probe` 对图片会填，视频通常为空）。
    pub fn from_pix_fmt(pix_fmt: Option<&str>) -> Self {
        match pix_fmt {
            Some(name) => Self {
                depth: depth_from_pix_fmt(name),
                chroma: chroma_from_pix_fmt(name),
            },
            None => Self::UNKNOWN,
        }
    }

    /// 位深与 chroma 都拿到了吗（只有这时才允许回填 `pix_fmt`）。
    pub fn is_complete(self) -> bool {
        self.depth.is_some() && self.chroma != Chroma::Unknown
    }
}

/// 从 `pix_fmt` 名字读位深（`yuv420p10le` / `p010le` / `yuv420p12le` / `nv12` …）。
///
/// 认不出的返回 `None`（交给容器头兜底）。**注意**：`nv12`/`nv21` 的 `12` 是 NV 布局编号、
/// `yuv420p`/`yuv422p`/`yuv444p` 的 `420/422/444` 是 chroma，都不是位深 —— 先把这些已知
/// 写法与 chroma 数字剔掉，再对剩下的数字做位深回退（t21 F7：以前 `nv12` 被判成 12bit）。
pub fn depth_from_pix_fmt(pix_fmt: &str) -> Option<u8> {
    let lower = pix_fmt.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    // 1) 已知的 8bit 写法（NV12/NV21 带数字但仍是 8bit）。
    const EIGHT_BIT: &[&str] = &[
        "nv12", "nv21", "yuv420p", "yuv422p", "yuv444p", "yuva420p", "yuvj420p", "rgb24", "bgr24",
        "rgb8", "rgba8", "bgr8", "bgra8", "gray", "gray8", "monob",
    ];
    if EIGHT_BIT.contains(&lower.as_str()) {
        return Some(8);
    }
    // 2) 明确的半平面/打包多位深写法，直接认记号。
    for (marker, depth) in [
        ("p016", 16u8),
        ("p216", 16),
        ("p012", 12),
        ("p212", 12),
        ("p010", 10),
        ("p210", 10),
        ("p410", 10),
        ("p310", 10),
    ] {
        if lower.contains(marker) {
            return Some(depth);
        }
    }
    // 3) `yuv420p10le` 这类：先剔掉 chroma 数字，再看剩下的位深数字。
    let mut rest = lower.clone();
    for chroma in ["444", "440", "422", "420", "411", "410"] {
        rest = rest.replace(chroma, "");
    }
    let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
    if digits.contains("16") {
        return Some(16);
    }
    if digits.contains("12") {
        return Some(12);
    }
    if digits.contains("10") {
        return Some(10);
    }
    // 4) 无位深数字的常见 8bit 家族。
    if lower.starts_with("yuv")
        || lower.starts_with("yuva")
        || lower.starts_with("rgb")
        || lower.starts_with("bgr")
        || lower.starts_with("gray")
        || lower.starts_with("nv")
        || lower.starts_with("p0")
    {
        return Some(8);
    }
    None
}

/// 从 `pix_fmt` 名字读 chroma（`yuv422p` → 4:2:2）。
pub fn chroma_from_pix_fmt(pix_fmt: &str) -> Chroma {
    let lower = pix_fmt.trim().to_ascii_lowercase();
    if lower.contains("nv12")
        || lower.contains("nv21")
        || lower.contains("420")
        || lower.contains("p010")
        || lower.contains("p210")
        || lower.contains("p012")
        || lower.contains("p212")
        || lower.contains("p016")
        || lower.contains("p216")
    {
        return Chroma::C420;
    }
    if lower.contains("422") {
        return Chroma::C422;
    }
    if lower.contains("444") {
        return Chroma::C444;
    }
    if lower.contains("mono") || lower.contains("gray") {
        return Chroma::Mono;
    }
    Chroma::Unknown
}

/// 由位深 + chroma 生成**真实**的 `pix_fmt` 名字；两者不全就返回 `None`（绝不伪造）。
pub fn pix_fmt_name(depth: u8, chroma: Chroma) -> Option<String> {
    let stem = match chroma {
        Chroma::C420 => "yuv420p",
        Chroma::C422 => "yuv422p",
        Chroma::C444 => "yuv444p",
        Chroma::Mono => "gray",
        Chroma::Unknown => return None,
    };
    let suffix = match depth {
        8 => "",
        10 => "10le",
        12 => "12le",
        16 => "16le",
        _ => return None,
    };
    Some(format!("{stem}{suffix}"))
}

/// `details` 里的源格式（`pix_fmt` 有值才推断）。
pub fn detail_format(details: &MediaDetails) -> SourceFormat {
    SourceFormat::from_pix_fmt(details.pix_fmt.as_deref())
}

/// 分类函数（**纯函数**，判据表的唯一出口）：格式从 `details.pix_fmt` 推断。
///
/// 容器头里能读到 chroma 时用 [`classify_format`]（`classify_path` 走的那条）。
pub fn classify(details: &MediaDetails, caps: &HardwareCaps) -> Compatibility {
    classify_format(details, &detail_format(details), None, caps)
}

/// 分类函数（带显式源格式 + 源容器）：判据表唯一出口。
///
/// 顺序有意义：分辨率上限 → 位深 → chroma → 编码族/分辨率 → **容器能力**。
/// chroma 必须排在编码族之前，否则 8bit 4:2:2 会先被判成 `Playable`，播放时掉进
/// 软解（t21 F2/F3）；容器能力放在最后，因为它是"消费端能不能解封装"的问题，
/// 与素材本身的位深/色度无关（t30 F1：GStreamer 构型打不开 `.mkv`）。
///
/// `container = None` 表示容器未知（罕见扩展名/探测失败）：这种情况下不额外收紧，
/// 由 [`DecodeConsumer::can_open`] 之外的分支处理，保持既有语义。
pub fn classify_format(
    details: &MediaDetails,
    format: &SourceFormat,
    container: Option<Container>,
    caps: &HardwareCaps,
) -> Compatibility {
    let codec = details
        .codec
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let width = details.width;
    let height = details.height;

    if width > MAX_HARDWARE_WIDTH || height > MAX_HARDWARE_WIDTH {
        return Compatibility::Unsupported {
            reason: format!(
                "{width}×{height} 超出本机解码上限（{MAX_HARDWARE_WIDTH} 像素）：转码救不了，请换 8K 以内的素材"
            ),
        };
    }

    if let Some(depth) = format.depth {
        if depth > 8 {
            let target = conversion_target(caps, width);
            return Compatibility::NeedsConversion {
                reason: format!(
                    "该素材是 {depth}bit {}：当前解码/RGB 转换链只支持 8bit NV12，会自动转成 {}",
                    format.chroma.as_str(),
                    target.describe_zh()
                ),
                target,
            };
        }
    }

    // 未知色度不得放行：容器头没写清（Matroska 常见）或解析不出时，保守判成
    // "需要转码"——转出来的产物一定是已知的 8bit 4:2:0，比静默软解安全。
    if format.chroma == Chroma::Unknown {
        let target = conversion_target(caps, width);
        return Compatibility::NeedsConversion {
            reason: format!(
                "无法从容器头确认该素材的色度抽样（容器没写清，可能是 4:2:2/4:4:4）：保守起见会自动转成 {} 再播放",
                target.describe_zh()
            ),
            target,
        };
    }

    // 硬解只吃 4:2:0：8bit 4:2:2 / 4:4:4 一样要转码，绝不允许"判成能播再静默软解"。
    if !format.chroma.is_hardware_friendly() {
        let target = conversion_target(caps, width);
        return Compatibility::NeedsConversion {
            reason: format!(
                "该素材是 {} 色度抽样，解码/RGB 转换链只支持 4:2:0：会自动转成 {}（也可手动 `ffmpeg -i in.mp4 -pix_fmt yuv420p -c:v {} out.{}`）",
                format.chroma.as_str(),
                target.describe_zh(),
                target.codec,
                target.container.extension()
            ),
            target,
        };
    }

    // 编码族判定：`codec` 可能来自 ffprobe/GStreamer（`h264`/`avc`/`H.264`），
    // 也可能来自容器（`avc1`/`hvc1`），统一做包含匹配。
    let is_h264 = codec.contains("h264") || codec.contains("avc");
    let is_av1 = codec.contains("av1") || codec.contains("av01");

    if is_h264 && width > caps.h264_max_width {
        let target = conversion_target(caps, width);
        return Compatibility::NeedsConversion {
            reason: format!(
                "{width}×{height} H.264 超出 NVDEC 的 H.264 解码上限（{} 像素宽），只能软解：会自动转成 {}",
                caps.h264_max_width,
                target.describe_zh()
            ),
            target,
        };
    }

    if is_av1 && !caps.av1_hardware {
        let target = conversion_target(caps, width);
        return Compatibility::NeedsConversion {
            reason: format!("本机没有 AV1 硬件解码：会自动转成 {}", target.describe_zh()),
            target,
        };
    }

    // 纯 GStreamer 构型（打包默认）的管线是写死的 `qtdemux ! h264parse`：
    // HEVC/AV1/VP9 的 MP4/MKV 它根本打不开（t26 的根因：转码产物是 MKV/HEVC）。
    // 所以这类素材在这种构型下也要转成 MP4/H.264。
    if caps.consumer == DecodeConsumer::GStreamer && !is_h264 {
        let target = conversion_target(caps, width);
        return Compatibility::NeedsConversion {
            reason: format!(
                "当前 GStreamer 解码管线只支持 MP4/H.264（无法打开 {codec}）：会自动转成 {}",
                target.describe_zh()
            ),
            target,
        };
    }

    // **容器能力**（t30 F1）：编码对了不等于打得开——纯 GStreamer 构型的
    // `qtdemux` 只解 ISO-BMFF，`.mkv`/`.webm` 即使是 H.264 4:2:0 也会在开屏时
    // 炸成 `atom has bogus size`（用户看到的是一条 `UNSUPPORTED_CODEC`）。
    // 这里把"能不能解封装"一并判掉：判转码 → 产物是 MP4，装得进发行包的解码链。
    let codec_name = if is_h264 {
        "h264"
    } else if codec.is_empty() {
        "unknown"
    } else {
        codec.as_str()
    };
    if let Some(source) = container.filter(|source| !caps.consumer.can_open(*source, codec_name)) {
        let target = conversion_target(caps, width);
        return Compatibility::NeedsConversion {
            reason: format!(
                "当前解码路径（{}）打不开 {} 容器里的 {codec_name}：发行包只带 MP4/H.264 解码链，会自动转成 {}，完成后自动播放",
                caps.consumer.as_str(),
                source.describe_zh(),
                target.describe_zh()
            ),
            target,
        };
    }

    // HEVC/AV1/VP9 在 ≤8K 且 4:2:0 时都走硬解；H.264 ≤4K 同理。
    // 其它编码（mpeg4/mjpeg/未知）保持乐观：让播放路径按既有方式处理，
    // 真的解不了会在开屏时给出可操作错误。
    Compatibility::Playable
}

/// 探测 + 分类（扫描期与 `get_media`/播放前共用）。
///
/// `kind` 与 `scan.rs`/`get_media` 同口径（`"image"` / `"video"`）。
pub fn classify_path(
    path: &Path,
    kind: &str,
    caps: &HardwareCaps,
) -> (Option<MediaDetails>, Compatibility) {
    if kind != "video" {
        // 图片不参与转码（解码走 png/JPEG，与位深无关）。
        return (crate::probe::probe(path, kind), Compatibility::Playable);
    }
    let mut details = crate::probe::probe(path, kind);
    let mut format = SourceFormat::UNKNOWN;
    if let Some(details) = details.as_mut() {
        format = enrich_format(path, details);
    }
    // 源容器：决定"这条解码路径能不能解封装"（t30 F1）。
    let container = Container::from_source_path(path);
    let compatibility = match details.as_ref() {
        // 容器头读到了 chroma + 位深：按真实格式分类（这是 F2/F3 的修复点）。
        Some(details) if format.is_complete() => classify_format(details, &format, container, caps),
        // 容器头没给出完整格式（例如 Matroska 的 CodecPrivate）：只能按 details 里
        // 已有的信息分类，chroma 视为未知。
        // 容器也未知时没有额外信息可加，走"只看 details"的 [`classify`]。
        Some(details) if container.is_none() => classify(details, caps),
        Some(details) => classify_format(details, &detail_format(details), container, caps),
        // 探测失败（罕见容器/损坏文件）：也不再乐观放行——t29 F2 的红线是
        // "`Chroma::Unknown` 不得进入 Playable"。转码是安全的收敛点：产物一定是已知的
        // 8bit 4:2:0，真解不了会给出可操作错误，而不是静默软解。
        None => {
            let target = conversion_target(caps, 0);
            Compatibility::NeedsConversion {
                reason: format!(
                    "无法从容器头识别该视频的编码/色度（罕见容器或文件不完整）：会自动尝试转成 {} 再播放",
                    target.describe_zh()
                ),
                target,
            }
        }
    };
    (details, compatibility)
}

/// 把容器头里的位深与 chroma 补进 `details`（`probe` 对视频不填 `pix_fmt`）。
///
/// 位深与 chroma 分开解析；**只有两者都拿到**才回填 `pix_fmt`（`yuv422p10le` 这种真实
/// 名字），拿不全就保持 `None`——不伪造格式（t21 F2）。只对可能多位深/非 4:2:0 的编码做，
/// 有界读取（head 4MiB + tail 4MiB），失败的代价只是"分类偏乐观"。
pub fn enrich_format(path: &Path, details: &mut MediaDetails) -> SourceFormat {
    let mut format = detail_format(details);
    let codec = details
        .codec
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !is_depth_parsable(&codec) {
        return format;
    }
    let parsed = container_format(path);
    if format.depth.is_none() {
        format.depth = parsed.depth;
    }
    if format.chroma == Chroma::Unknown {
        format.chroma = parsed.chroma;
    }
    if details.pix_fmt.is_none() {
        details.pix_fmt = format
            .depth
            .and_then(|depth| pix_fmt_name(depth, format.chroma));
    }
    format
}

/// 这些编码才值得去容器头里找位深/chroma（其余按沿用逻辑）。
fn is_depth_parsable(codec: &str) -> bool {
    codec.contains("hevc")
        || codec.contains("hvc1")
        || codec.contains("h265")
        || codec.contains("av1")
        || codec.contains("av01")
        || codec.contains("vp9")
        || codec.contains("vp09")
        || codec.contains("h264")
        || codec.contains("avc")
}

/// 从容器头读源格式：ISO-BMFF 的 `hvcC`/`avcC`/`av1C`，Matroska 的 `Colour > BitsPerChannel`。
///
/// 读 head 4MiB，找不到再读 tail 4MiB（moov 可能写在文件尾）。
pub fn container_format(path: &Path) -> SourceFormat {
    let Some(head) = read_window(path, 0, DEPTH_WINDOW) else {
        return SourceFormat::UNKNOWN;
    };
    let from_head = format_from_container_bytes(&head);
    if from_head.is_complete() {
        return from_head;
    }
    let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size <= DEPTH_WINDOW {
        return from_head;
    }
    let Some(tail) = read_window(path, size - DEPTH_WINDOW, DEPTH_WINDOW) else {
        return from_head;
    };
    let from_tail = format_from_container_bytes(&tail);
    SourceFormat {
        depth: from_head.depth.or(from_tail.depth),
        chroma: if from_head.chroma == Chroma::Unknown {
            from_tail.chroma
        } else {
            from_head.chroma
        },
    }
}

/// 从一段容器字节里读源格式（测试直接喂字节）。
pub fn format_from_container_bytes(bytes: &[u8]) -> SourceFormat {
    if let Some(index) = find_bytes(bytes, b"hvcC") {
        if let Some(payload) = bytes.get(index + 4..) {
            return format_from_hvcc(payload);
        }
    }
    if let Some(index) = find_bytes(bytes, b"avcC") {
        if let Some(payload) = bytes.get(index + 4..) {
            return format_from_avcc(payload);
        }
    }
    if let Some(index) = find_bytes(bytes, b"av1C") {
        if let Some(payload) = bytes.get(index + 4..) {
            return format_from_av1c(payload);
        }
    }
    // Matroska 没有 `hvcC`/`avcC` 这类 box 名：配置记录直接躺在 `CodecPrivate`
    // （0x63A2）里，记录类型由 `CodecID` 决定。不解析它只剩
    // `Colour.BitsPerChannel`（位深），色度永远 Unknown ⇒ 每个 .mkv 都要多余转码。
    let matroska = format_from_matroska(bytes);
    if matroska.is_complete() {
        return matroska;
    }
    // Matroska：Colour (0x55B2) 的 BitsPerChannel —— 只有位深，没有 chroma。
    if let Some(index) = find_bytes(bytes, &[0x55, 0xB2]) {
        if let Some(rest) = bytes.get(index + 2..) {
            if let Some(depth) = mkv_bits_per_channel(rest) {
                return SourceFormat {
                    depth: Some(depth),
                    chroma: Chroma::Unknown,
                };
            }
        }
    }
    SourceFormat::UNKNOWN
}

/// HEVCDecoderConfigurationRecord：byte16 低 2 位 chromaFormat、byte17 低 3 位
/// bitDepthLumaMinus8（MP4 的 `hvcC` box 与 Matroska 的 `CodecPrivate` 是同一份记录）。
fn format_from_hvcc(record: &[u8]) -> SourceFormat {
    let chroma = record
        .get(16)
        .map(|byte| Chroma::from_chroma_format_idc(u64::from(byte & 0x03)))
        .unwrap_or(Chroma::Unknown);
    let depth = record
        .get(17)
        .map(|byte| 8 + (byte & 0x07))
        .filter(|depth| (8..=16).contains(depth));
    SourceFormat { depth, chroma }
}

/// AV1CodecConfigurationRecord：byte1 高 3 位 seq_profile、byte2 bit6 = high_bitdepth、
/// bit5 = twelve_bit。
fn format_from_av1c(record: &[u8]) -> SourceFormat {
    let flags = record.get(2).copied().unwrap_or(0);
    let depth = if flags & 0x20 != 0 {
        12
    } else if flags & 0x40 != 0 {
        10
    } else {
        8
    };
    let seq_profile = record.get(1).map(|byte| (byte & 0xE0) >> 5).unwrap_or(0);
    let chroma = match seq_profile {
        0 => Chroma::C420,
        1 => Chroma::C444,
        2 => Chroma::C422,
        _ => Chroma::Unknown,
    };
    SourceFormat {
        depth: Some(depth),
        chroma,
    }
}

/// Matroska 的解码器配置记录类型（由 `CodecID` 决定）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum MatroskaCodec {
    Hevc,
    Avc,
    Av1,
}

/// Matroska：按 `CodecID` 找到 `CodecPrivate` 里的解码器配置记录并解析。
///
/// `CodecPrivate` 的载荷就是裸的 hvcC / avcC / av1C 记录（没有 box 名），元素 ID 是
/// 0x63A2、后面紧跟 EBML size vint。mdat 里可能撞上同样的两字节，所以逐个候选校验
/// 记录头（hvcC 首字节 = configurationVersion 1 且 profile_idc 合理；avcC 有
/// lengthSizeMinusOne/numOfSPS 的保留位；AV1 首字节是 marker+version = 0x81）。
fn format_from_matroska(bytes: &[u8]) -> SourceFormat {
    let kind = if find_bytes(bytes, b"V_MPEGH/ISO/HEVC").is_some() {
        MatroskaCodec::Hevc
    } else if find_bytes(bytes, b"V_MPEG4/ISO/AVC").is_some() {
        MatroskaCodec::Avc
    } else if find_bytes(bytes, b"V_AV1").is_some() {
        MatroskaCodec::Av1
    } else {
        return SourceFormat::UNKNOWN;
    };
    let mut cursor = 0usize;
    while let Some(offset) = find_bytes(&bytes[cursor..], &[0x63, 0xA2]) {
        let at = cursor + offset + 2;
        cursor = at;
        let Some((size, header)) = read_ebml_size(bytes, at) else {
            continue;
        };
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let Some(payload) = bytes.get(at + header..) else {
            continue;
        };
        let record = payload.get(..size).unwrap_or(payload);
        let format = match kind {
            MatroskaCodec::Hevc if looks_like_hvcc(record) => format_from_hvcc(record),
            MatroskaCodec::Avc if looks_like_avcc(record) => format_from_avcc(record),
            MatroskaCodec::Av1 if looks_like_av1c(record) => format_from_av1c(record),
            _ => continue,
        };
        if format.depth.is_some() {
            return format;
        }
    }
    SourceFormat::UNKNOWN
}

fn looks_like_hvcc(record: &[u8]) -> bool {
    record.len() >= 19 && record[0] == 1 && (1..=11).contains(&(record[1] & 0x1F))
}

fn looks_like_avcc(record: &[u8]) -> bool {
    record.len() >= 8 && record[0] == 1 && record[4] & 0x03 == 0x03 && record[5] & 0xE0 == 0xE0
}

fn looks_like_av1c(record: &[u8]) -> bool {
    record.len() >= 4 && record[0] == 0x81
}

/// EBML 元素 size（vint）：返回（值, 长度字节数）。
fn read_ebml_size(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let first = *bytes.get(at)?;
    let length = first.leading_zeros() as usize + 1;
    if length > 8 || at + length > bytes.len() {
        return None;
    }
    let mask = 0xFFu64 >> length;
    let mut value = u64::from(first) & mask;
    for index in 1..length {
        value = (value << 8) | u64::from(*bytes.get(at + index)?);
    }
    if length == 8 && value == 0x00FF_FFFF_FFFF_FFFF {
        return None;
    }
    Some((value, length))
}

/// Matroska `Colour` 里 `BitsPerChannel` 的 vint 长度 + 无符号整数。
fn mkv_bits_per_channel(rest: &[u8]) -> Option<u8> {
    let length_byte = *rest.first()?;
    let length = usize::from(length_byte & 0x7F);
    if !(0x80..=0xFE).contains(&length_byte) || !(1..=8).contains(&length) {
        return None;
    }
    let value = rest.get(1..1 + length)?;
    let mut depth = 0u64;
    for byte in value {
        depth = (depth << 8) | u64::from(*byte);
    }
    (8..=16).contains(&depth).then_some(depth as u8)
}

/// 解析 AVCDecoderConfigurationRecord：优先读 SPS 的 chroma_format_idc/bit_depth_luma_minus8，
/// 拿不到再按 profile_idc 推（122 = High 4:2:2、244/44 = High 4:4:4、110 = High 10）。
fn format_from_avcc(payload: &[u8]) -> SourceFormat {
    // 少于 8 字节连记录头都不完整：保持未知，别把垃圾判成 8bit。
    if payload.len() < 8 {
        return SourceFormat::UNKNOWN;
    }
    let profile_idc = u32::from(payload.get(1).copied().unwrap_or(0));
    if let Some((chroma, depth)) = avcc_first_sps(payload).and_then(parse_sps) {
        return SourceFormat {
            depth: Some(depth),
            chroma,
        };
    }
    let chroma = match profile_idc {
        0 => Chroma::Unknown,
        122 => Chroma::C422,
        244 | 44 => Chroma::C444,
        _ => Chroma::C420,
    };
    let depth = if profile_idc == 110 { 10 } else { 8 };
    SourceFormat {
        depth: Some(depth),
        chroma,
    }
}

/// AVCDecoderConfigurationRecord 里第一条 SPS 的字节范围。
fn avcc_first_sps(payload: &[u8]) -> Option<&[u8]> {
    let count = payload.get(5)? & 0x1F;
    if count == 0 {
        return None;
    }
    let length = (usize::from(*payload.get(6)?) << 8) | usize::from(*payload.get(7)?);
    if length == 0 {
        return None;
    }
    payload.get(8..8 + length)
}

/// H.264 里带 `chroma_format_idc` / `bit_depth_luma_minus8` 的 profile。
const HIGH_PROFILES: &[u32] = &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// 解析 H.264 SPS 头：返回 (chroma, 位深)。
fn parse_sps(nal: &[u8]) -> Option<(Chroma, u8)> {
    let body = if nal.first().map(|byte| byte & 0x1F == 7).unwrap_or(false) {
        nal.get(1..)?
    } else {
        nal
    };
    let rbsp = unescape_rbsp(body);
    let mut bits = BitReader::new(&rbsp);
    let profile_idc = bits.bits(8)? as u32;
    let _ = bits.bits(8)?; // constraint flags
    let _ = bits.bits(8)?; // level_idc
    let _ = bits.ue()?; // seq_parameter_set_id
    if !HIGH_PROFILES.contains(&profile_idc) {
        return Some((Chroma::C420, 8));
    }
    let idc = bits.ue()?;
    let chroma = Chroma::from_chroma_format_idc(idc);
    if idc == 3 {
        let _ = bits.bits(1)?; // separate_colour_plane_flag
    }
    let depth = (bits.ue()? + 8).min(u64::from(u8::MAX)) as u8;
    Some((chroma, depth))
}

/// 去掉 H.264 RBSP 里的防竞争字节（`00 00 03` → `00 00`）。
fn unescape_rbsp(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeros = 0u8;
    for &byte in bytes {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 {
            zeros.saturating_add(1)
        } else {
            0
        };
        out.push(byte);
    }
    out
}

/// 极简按位读取器（只为解析 SPS 头，够用即可）。
struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn bits(&mut self, count: u8) -> Option<u64> {
        let mut value = 0u64;
        for _ in 0..count {
            let byte = self.bytes.get(self.bit / 8)?;
            let shift = 7 - (self.bit % 8);
            value = (value << 1) | u64::from((byte >> shift) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    /// 无符号 Exp-Golomb。
    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0u32;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.bits(zeros as u8)?;
        Some((1u64 << zeros) - 1 + rest)
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_window(path: &Path, offset: u64, len: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut buffer).ok()?;
    Some(buffer)
}

/// 转码缓存目录：`$XDG_CACHE_HOME/liuguang/transcode`（缺省 `~/.cache/...`）。
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        let base = PathBuf::from(dir);
        if base.is_absolute() {
            return base.join("liuguang").join("transcode");
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".cache").join("liuguang").join("transcode")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn details(width: u32, height: u32, codec: &str, pix_fmt: Option<&str>) -> MediaDetails {
        MediaDetails {
            width,
            height,
            fps_millihz: None,
            duration_ms: None,
            codec: Some(codec.to_string()),
            pix_fmt: pix_fmt.map(str::to_string),
        }
    }

    fn caps() -> HardwareCaps {
        HardwareCaps::default()
    }

    #[test]
    fn eight_k_h264_needs_conversion_to_hevc() {
        let verdict = classify(&details(7680, 4320, "h264", Some("yuv420p")), &caps());
        match verdict {
            Compatibility::NeedsConversion { reason, target } => {
                assert!(reason.contains("7680×4320"), "{reason}");
                assert!(reason.contains("4096"), "{reason}");
                assert_eq!(target.codec, "hevc");
                assert_eq!(target.bit_depth, 8);
                assert_eq!(target.label(), "hevc 8bit");
            }
            other => panic!("期望 NeedsConversion，实际 {other:?}"),
        }
    }

    #[test]
    fn four_k_h264_stays_playable() {
        // 4096 是 NVDEC 的 H.264 边界：等于边界仍可硬解。
        assert_eq!(
            classify(&details(4096, 2160, "h264", Some("yuv420p")), &caps()),
            Compatibility::Playable
        );
        assert_eq!(
            classify(&details(3840, 2160, "h264", Some("yuv420p")), &caps()),
            Compatibility::Playable
        );
    }

    #[test]
    fn full_hd_h264_stays_playable() {
        assert_eq!(
            classify(&details(1920, 1080, "avc1", Some("yuv420p")), &caps()),
            Compatibility::Playable
        );
    }

    #[test]
    fn eight_bit_hevc_stays_playable() {
        assert_eq!(
            classify(&details(7680, 4320, "hevc", Some("yuv420p")), &caps()),
            Compatibility::Playable
        );
        // 没有 pix_fmt ⇒ 色度未知 ⇒ 不再乐观放行（t29 F2）。
        assert!(
            classify(&details(3840, 2160, "h265", None), &caps()).needs_conversion(),
            "未知色度不得判成 Playable"
        );
    }

    #[test]
    fn ten_bit_hevc_needs_conversion() {
        for pix_fmt in ["yuv420p10le", "p010le", "yuv420p10be"] {
            let verdict = classify(&details(3840, 2160, "hevc", Some(pix_fmt)), &caps());
            match verdict {
                Compatibility::NeedsConversion { reason, target } => {
                    assert!(reason.contains("10bit"), "{pix_fmt}: {reason}");
                    assert!(reason.contains("8bit"), "{pix_fmt}: {reason}");
                    assert_eq!(target.bit_depth, 8);
                }
                other => panic!("{pix_fmt}: 期望 NeedsConversion，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn av1_depends_on_hardware() {
        let av1 = details(3840, 2160, "av1", Some("yuv420p"));
        assert_eq!(
            classify(&av1, &caps().with_av1_hardware(true)),
            Compatibility::Playable
        );
        match classify(&av1, &caps().with_av1_hardware(false)) {
            Compatibility::NeedsConversion { reason, target } => {
                assert!(reason.contains("AV1"), "{reason}");
                assert_eq!(target.codec, "hevc");
            }
            other => panic!("期望 NeedsConversion，实际 {other:?}"),
        }
    }

    #[test]
    fn unknown_details_need_conversion_instead_of_playable() {
        // t29 F2：探测失败 / 格式不完整一律不再乐观放行——`Chroma::Unknown`
        // 绝不能进 `Playable`（否则 8bit 4:2:2 的 .mkv 会静默软解）。
        let (_, verdict) = classify_path(Path::new("/nonexistent/nowhere.mp4"), "video", &caps());
        match verdict {
            Compatibility::NeedsConversion { reason, .. } => {
                assert!(reason.contains("无法从容器头"), "{reason}");
            }
            other => panic!("探测失败应判转码，实际 {other:?}"),
        }
        // 图片不受影响（解码走 png/JPEG，与位深无关）。
        assert_eq!(
            classify_path(Path::new("/nonexistent/nowhere.png"), "image", &caps()).1,
            Compatibility::Playable
        );
    }

    #[test]
    fn widths_beyond_eight_k_are_unsupported() {
        match classify(&details(10240, 4320, "hevc", Some("yuv420p")), &caps()) {
            Compatibility::Unsupported { reason } => {
                assert!(reason.contains("10240×4320"), "{reason}");
                assert!(reason.contains("8192"), "{reason}");
            }
            other => panic!("期望 Unsupported，实际 {other:?}"),
        }
    }

    #[test]
    fn ten_bit_wins_over_four_k_h264() {
        // 位深判据优先：4K 10bit H.264 也要转（10bit 是硬阻塞，宽度不是）。
        match classify(&details(3840, 2160, "h264", Some("yuv420p10le")), &caps()) {
            Compatibility::NeedsConversion { reason, .. } => assert!(reason.contains("10bit")),
            other => panic!("期望 NeedsConversion，实际 {other:?}"),
        }
    }

    #[test]
    fn compat_info_matches_ipc_shape() {
        let info = classify(&details(7680, 4320, "h264", Some("yuv420p")), &caps()).to_info();
        assert_eq!(info.state, CompatState::NeedsConversion);
        assert!(info.reason.is_some());
        assert_eq!(info.target.as_deref(), Some("hevc 8bit"));
        let playable = Compatibility::Playable.to_info();
        assert_eq!(playable.state, CompatState::Playable);
        assert!(playable.reason.is_none() && playable.target.is_none());
    }

    #[test]
    fn depth_from_pix_fmt_covers_the_common_spellings() {
        assert_eq!(depth_from_pix_fmt("yuv420p"), Some(8));
        assert_eq!(depth_from_pix_fmt("rgba8"), Some(8));
        assert_eq!(depth_from_pix_fmt("yuv420p10le"), Some(10));
        assert_eq!(depth_from_pix_fmt("p010"), Some(10));
        assert_eq!(depth_from_pix_fmt("p010le"), Some(10));
        assert_eq!(depth_from_pix_fmt("p016le"), Some(16));
        assert_eq!(depth_from_pix_fmt("yuv420p12le"), Some(12));
        assert_eq!(depth_from_pix_fmt("weird"), None);
    }

    #[test]
    fn nv12_is_eight_bits_not_twelve() {
        // t21 F7：NV12/NV21 的 "12" 是 NV 布局编号，不是位深；422/444 是 chroma 数字。
        assert_eq!(depth_from_pix_fmt("nv12"), Some(8));
        assert_eq!(depth_from_pix_fmt("nv21"), Some(8));
        assert_eq!(depth_from_pix_fmt("yuv422p"), Some(8));
        assert_eq!(depth_from_pix_fmt("yuv444p"), Some(8));
        assert_eq!(depth_from_pix_fmt("yuv422p10le"), Some(10));
        assert_eq!(depth_from_pix_fmt("yuv444p10le"), Some(10));
        assert_eq!(chroma_from_pix_fmt("nv12"), Chroma::C420);
        assert_eq!(chroma_from_pix_fmt("yuv422p10le"), Chroma::C422);
        assert_eq!(chroma_from_pix_fmt("yuv444p"), Chroma::C444);
        assert_eq!(
            pix_fmt_name(10, Chroma::C422).as_deref(),
            Some("yuv422p10le")
        );
        // chroma 未知时绝不生成名字（不伪造 pix_fmt）。
        assert_eq!(pix_fmt_name(10, Chroma::Unknown), None);
    }

    #[test]
    fn eight_bit_four_two_two_h264_needs_conversion() {
        // 8bit 也可能是 4:2:2：绝不允许判成能播再静默软解。
        let format = SourceFormat {
            depth: Some(8),
            chroma: Chroma::C422,
        };
        match classify_format(&details(1920, 1080, "h264", None), &format, None, &caps()) {
            Compatibility::NeedsConversion { reason, target } => {
                assert!(reason.contains("4:2:2"), "{reason}");
                assert!(reason.contains("4:2:0"), "{reason}");
                assert_eq!(target.label(), "hevc 8bit");
            }
            other => panic!("期望 NeedsConversion，实际 {other:?}"),
        }
    }

    #[test]
    fn eight_bit_four_four_four_hevc_needs_conversion() {
        let format = SourceFormat {
            depth: Some(8),
            chroma: Chroma::C444,
        };
        match classify_format(&details(3840, 2160, "hevc", None), &format, None, &caps()) {
            Compatibility::NeedsConversion { reason, .. } => {
                assert!(reason.contains("4:4:4"), "{reason}");
                assert!(reason.contains("ffmpeg"), "{reason}");
            }
            other => panic!("期望 NeedsConversion，实际 {other:?}"),
        }
    }

    #[test]
    fn real_four_two_zero_eight_bit_stays_playable() {
        let format = SourceFormat {
            depth: Some(8),
            chroma: Chroma::C420,
        };
        assert_eq!(
            classify_format(&details(1920, 1080, "h264", None), &format, None, &caps()),
            Compatibility::Playable
        );
        assert_eq!(
            classify_format(&details(3840, 2160, "hevc", None), &format, None, &caps()),
            Compatibility::Playable
        );
    }

    #[test]
    fn hvc_c_bit_depth_is_read_from_the_record() {
        // payload[17] 低 3 位 = bitDepthLumaMinus8（实测 10bit 的 matrix 素材是 2）。
        let mut payload = [0u8; 32];
        payload[16] = 0b0000_0001; // chromaFormat = 4:2:0
        payload[17] = 0b1111_1010; // bitDepthLumaMinus8 = 2
        let mut bytes = b"\x00\x00\x09\x7a".to_vec();
        bytes.extend_from_slice(b"hvcC");
        bytes.extend_from_slice(&payload);
        assert_eq!(format_from_container_bytes(&bytes).depth, Some(10));
    }

    #[test]
    fn avc_c_chroma_and_depth_come_from_the_sps() {
        // profile_idc = 122（High 4:2:2），SPS 里 chroma_format_idc = 2、bit_depth_luma_minus8 = 0。
        // RBSP 位串：ue(0)="1"、ue(2)="011"、ue(0)="1" → 0b1011_1000。
        let sps = [0x67u8, 0x7A, 0x00, 0x29, 0xB8];
        let mut payload = vec![1u8, 0x7A, 0x00, 0x29, 0xFF, 0xE1];
        payload.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        payload.extend_from_slice(&sps);
        let mut bytes = b"\x00\x00\x00\x10".to_vec();
        bytes.extend_from_slice(b"avcC");
        bytes.extend_from_slice(&payload);
        let format = format_from_container_bytes(&bytes);
        assert_eq!(format.chroma, Chroma::C422, "{format:?}");
        assert_eq!(format.depth, Some(8));
        // 没有 SPS 的 avcC：退回按 profile_idc 推断（244 = High 4:4:4）。
        let fallback = [1u8, 0xF4, 0x00, 0x29, 0xFF, 0xE0, 0x00, 0x00];
        let mut bytes = b"\x00\x00\x00\x10".to_vec();
        bytes.extend_from_slice(b"avcC");
        bytes.extend_from_slice(&fallback);
        let format = format_from_container_bytes(&bytes);
        assert_eq!(format.chroma, Chroma::C444);
        assert_eq!(format.depth, Some(8));
    }

    #[test]
    fn av1_c_bit_depth_is_read_from_the_record() {
        for (flags, expected) in [(0u8, 8u8), (0x40, 10), (0x60, 12)] {
            let mut payload = [0u8; 8];
            payload[2] = flags;
            let mut bytes = b"\x00\x00\x00\x0c".to_vec();
            bytes.extend_from_slice(b"av1C");
            bytes.extend_from_slice(&payload);
            assert_eq!(format_from_container_bytes(&bytes).depth, Some(expected));
        }
    }

    /// 拼一个够用的 Matroska 片段：`CodecID` 元素 + `CodecPrivate` 元素。
    fn matroska_bytes(codec_id: &[u8], record: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x86, 0x81, codec_id.len() as u8];
        bytes.extend_from_slice(codec_id);
        let size = record.len() as u16;
        bytes.extend_from_slice(&[0x63, 0xA2, 0x40 | (size >> 8) as u8, (size & 0xFF) as u8]);
        bytes.extend_from_slice(record);
        bytes
    }

    /// HEVCDecoderConfigurationRecord（`hvcC` / Matroska `CodecPrivate` 同一份）。
    fn hvcc_record(chroma_format_idc: u8, bit_depth_minus8: u8) -> Vec<u8> {
        let mut record = vec![0u8; 32];
        record[0] = 1;
        record[1] = 0x01; // general_profile_idc = 1（Main）
        record[16] = 0xFC | (chroma_format_idc & 0x03);
        record[17] = 0xF8 | (bit_depth_minus8 & 0x07);
        record[18] = 0xF8 | (bit_depth_minus8 & 0x07);
        record
    }

    #[test]
    fn matroska_codec_private_supplies_chroma_and_depth() {
        // .mkv 没有 `hvcC` box 名：配置记录在 CodecPrivate（0x63A2）里，必须能读出来，
        // 否则每个 .mkv 都会因为"色度未知"被白转一次（t29 F2）。
        for (chroma_idc, bit_depth, expected_chroma) in [
            (1u8, 0u8, Chroma::C420),
            (2, 0, Chroma::C422),
            (3, 0, Chroma::C444),
            (1, 2, Chroma::C420),
        ] {
            let bytes = matroska_bytes(b"V_MPEGH/ISO/HEVC", &hvcc_record(chroma_idc, bit_depth));
            let format = format_from_container_bytes(&bytes);
            assert_eq!(format.chroma, expected_chroma, "chroma_idc={chroma_idc}");
            assert_eq!(
                format.depth,
                Some(8 + bit_depth),
                "bit_depth_minus8={bit_depth}"
            );
        }
    }

    #[test]
    fn matroska_without_codec_private_keeps_chroma_unknown_and_needs_conversion() {
        // 只有 Colour.BitsPerChannel（0x55B2）时，位深可读、色度未知。
        let bytes = [0x00, 0x55, 0xB2, 0x81, 0x08, 0x00];
        let format = format_from_container_bytes(&bytes);
        assert_eq!(format.depth, Some(8));
        assert_eq!(format.chroma, Chroma::Unknown);
        let verdict = classify_format(&details(3840, 2160, "hevc", None), &format, None, &caps());
        match verdict {
            Compatibility::NeedsConversion { reason, .. } => {
                assert!(reason.contains("无法从容器头确认"), "{reason}");
                assert!(reason.contains("4:2:0"), "{reason}");
            }
            other => panic!("未知色度的 mkv 不得判 Playable，实际 {other:?}"),
        }
    }

    #[test]
    fn matroska_four_two_two_and_four_four_four_are_never_playable() {
        for (chroma_idc, expected_chroma, marker) in
            [(2u8, Chroma::C422, "4:2:2"), (3, Chroma::C444, "4:4:4")]
        {
            let bytes = matroska_bytes(b"V_MPEGH/ISO/HEVC", &hvcc_record(chroma_idc, 0));
            let format = format_from_container_bytes(&bytes);
            assert_eq!(format.chroma, expected_chroma);
            let verdict =
                classify_format(&details(1920, 1080, "hevc", None), &format, None, &caps());
            match verdict {
                Compatibility::NeedsConversion { reason, target } => {
                    assert!(reason.contains(marker), "{reason}");
                    assert_eq!(target.label(), "hevc 8bit", "{}", target.label());
                }
                other => panic!("8bit {marker} 必须判转码，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn mkv_colour_bits_per_channel_is_read() {
        // Colour (0x55B2) → vint 长度 1 → 值 10。
        let bytes = [0x00, 0x55, 0xB2, 0x81, 0x0A, 0x00];
        assert_eq!(format_from_container_bytes(&bytes).depth, Some(10));
    }

    #[test]
    fn depth_parsing_is_robust_on_garbage() {
        assert_eq!(format_from_container_bytes(b"").depth, None);
        assert_eq!(format_from_container_bytes(b"hvc").depth, None);
        assert_eq!(format_from_container_bytes(b"hvcC").depth, None);
        assert_eq!(format_from_container_bytes(b"avcC\x00").depth, None);
        assert_eq!(format_from_container_bytes(&[0x55]).depth, None);
    }

    #[test]
    fn real_ten_bit_matrix_clip_is_detected() {
        // 真素材（t6/t15 都用这份）：hvcC 在文件头 543 字节处，位深 10。
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.work/corpus/matrix/matrix-3840x2160-30fps-hevc-10bit-p010.mp4"
        ));
        if !path.exists() {
            eprintln!("跳过：没有 matrix 10bit 素材（{}）", path.display());
            return;
        }
        assert_eq!(container_format(path).depth, Some(10));
        let (details, verdict) = classify_path(path, "video", &caps());
        let details = details.expect("probe 应能读出 3840x2160");
        assert_eq!((details.width, details.height), (3840, 2160));
        assert_eq!(details.codec.as_deref(), Some("hevc"));
        assert_eq!(details.pix_fmt.as_deref(), Some("yuv420p10le"));
        assert!(verdict.needs_conversion(), "10bit 素材应判 NeedsConversion");
    }

    #[test]
    fn real_eight_k_h264_clip_needs_conversion() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.work/corpus/matrix/matrix-7680x4320-30fps-h264-8bit.mp4"
        ));
        if !path.exists() {
            eprintln!("跳过：没有 matrix 8K H.264 素材（{}）", path.display());
            return;
        }
        let (details, verdict) = classify_path(path, "video", &caps());
        let details = details.expect("probe 应能读出 7680x4320");
        assert_eq!((details.width, details.height), (7680, 4320));
        assert!(verdict.needs_conversion(), "8K H.264 应判 NeedsConversion");
        assert_eq!(
            verdict.target().map(ConversionTarget::label),
            Some("hevc 8bit".to_string())
        );
    }

    #[test]
    fn cache_dir_is_under_the_xdg_cache_home() {
        let dir = cache_dir();
        let text = dir.to_string_lossy().into_owned();
        assert!(text.ends_with("liuguang/transcode"), "{text}");
    }

    // ===== t26：产物格式跟着实际解码路径走 =====

    /// GStreamer 构型（打包默认）的 caps。
    fn gst_caps() -> HardwareCaps {
        HardwareCaps::default().with_consumer(DecodeConsumer::GStreamer)
    }

    #[test]
    fn select_consumer_follows_the_build_and_the_device() {
        assert_eq!(select_consumer(false, false), DecodeConsumer::GStreamer);
        // 没编 vaapi feature：哪怕 /dev/dri 在也没有 VA-API 解码路径。
        assert_eq!(select_consumer(false, true), DecodeConsumer::GStreamer);
        // 编了但设备打不开：运行时会回落到 GStreamer。
        assert_eq!(select_consumer(true, false), DecodeConsumer::GStreamer);
        assert_eq!(select_consumer(true, true), DecodeConsumer::Vaapi);
    }

    #[test]
    fn gstreamer_pipeline_only_opens_mp4_h264() {
        // 纯 GStreamer 管线是写死的 `qtdemux ! h264parse`：MKV/HEVC 打不开。
        assert!(!DecodeConsumer::GStreamer.can_open(Container::Mkv, "hevc"));
        assert!(!DecodeConsumer::GStreamer.can_open(Container::Mp4, "hevc"));
        assert!(DecodeConsumer::GStreamer.can_open(Container::Mp4, "h264"));
        // VA-API 路径走 ffmpeg 解封装，两种容器都能吃。
        assert!(DecodeConsumer::Vaapi.can_open(Container::Mkv, "hevc"));
        assert!(DecodeConsumer::Vaapi.can_open(Container::Mp4, "h264"));
    }

    #[test]
    fn gstreamer_consumer_converts_eight_k_h264_to_mp4() {
        let verdict = classify(&details(7680, 4320, "h264", Some("yuv420p")), &gst_caps());
        match verdict {
            Compatibility::NeedsConversion { reason, target } => {
                assert_eq!(target.codec, "h264", "{reason}");
                assert_eq!(target.container, Container::Mp4);
                assert_eq!(target.scale_width, 4096);
                assert!(
                    target.label().starts_with("h264 8bit"),
                    "{}",
                    target.label()
                );
                assert!(target.label().contains("(mp4)"), "{}", target.label());
                assert!(reason.contains("MP4"), "{reason}");
                // 产物必须能被当前解码路径打开（这是 t26 的验收判据）。
                assert!(DecodeConsumer::GStreamer.can_open(target.container, target.codec));
            }
            other => panic!("期望 NeedsConversion，实际 {other:?}"),
        }
    }

    #[test]
    fn gstreamer_consumer_also_converts_hevc_and_av1() {
        // 8bit HEVC 在 VA-API 构型能硬解（Playable），但纯 GStreamer 管线打不开。
        let hevc = details(1920, 1080, "hevc", Some("yuv420p"));
        assert!(matches!(classify(&hevc, &caps()), Compatibility::Playable));
        match classify(&hevc, &gst_caps()) {
            Compatibility::NeedsConversion { reason, target } => {
                assert_eq!(target.container, Container::Mp4);
                assert_eq!(target.codec, "h264");
                assert!(
                    reason.contains("MP4/H.264") || reason.contains("无法打开"),
                    "{reason}"
                );
            }
            other => panic!("GStreamer 构型下 HEVC 应需转码，实际 {other:?}"),
        }
        let av1 = details(1920, 1080, "av1", Some("yuv420p"));
        match classify(&av1, &gst_caps()) {
            Compatibility::NeedsConversion { target, .. } => {
                assert_eq!(target.codec, "h264");
                assert_eq!(target.container, Container::Mp4);
            }
            other => panic!("GStreamer 构型下 AV1 应需转码，实际 {other:?}"),
        }
    }

    #[test]
    fn gstreamer_consumer_keeps_four_k_h264_playable() {
        // H.264 ≤4096 是这条管线本来就能播的，不该白转一份。
        assert!(matches!(
            classify(&details(1920, 1080, "h264", Some("yuv420p")), &gst_caps()),
            Compatibility::Playable
        ));
        assert!(matches!(
            classify(&details(4096, 2160, "h264", Some("yuv420p")), &gst_caps()),
            Compatibility::Playable
        ));
    }

    #[test]
    fn gstreamer_target_width_follows_the_config_cap() {
        // 源比上限窄 ⇒ 不缩放（`scale_width = 0`，label 不带 ≤Npx）。
        match classify(&details(1920, 1080, "hevc", Some("yuv420p")), &gst_caps()) {
            Compatibility::NeedsConversion { target, .. } => assert_eq!(target.scale_width, 0),
            other => panic!("{other:?}"),
        }
        // 配置给了更小的上限 ⇒ 听配置的。
        let small = HardwareCaps::default().with_consumer(DecodeConsumer::GStreamer);
        let small = HardwareCaps {
            transcode_max_width: 1920,
            ..small
        };
        match classify(&details(7680, 4320, "h264", Some("yuv420p")), &small) {
            Compatibility::NeedsConversion { target, .. } => {
                assert_eq!(target.scale_width, 1920);
                assert!(target.label().contains("≤1920px"), "{}", target.label());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ten_bit_targets_follow_the_consumer() {
        let ten_bit = details(3840, 2160, "hevc", Some("yuv420p10le"));
        match classify(&ten_bit, &caps()) {
            Compatibility::NeedsConversion { target, reason } => {
                assert_eq!(target.codec, "hevc");
                assert_eq!(target.container, Container::Mkv);
                assert!(reason.contains("HEVC"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        match classify(&ten_bit, &gst_caps()) {
            Compatibility::NeedsConversion { target, reason } => {
                assert_eq!(target.codec, "h264");
                assert_eq!(target.container, Container::Mp4);
                assert!(
                    reason.contains("H264") || reason.contains("h264"),
                    "{reason}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
    // ===== t30：容器也是"消费端能力"的一部分 =====

    /// 已知的 8bit 4:2:0 源格式。
    fn fmt_8bit_420() -> SourceFormat {
        SourceFormat {
            depth: Some(8),
            chroma: Chroma::C420,
        }
    }

    #[test]
    fn source_container_follows_the_extension() {
        // qtdemux 认 ISO-BMFF：mp4/m4v/mov 都算 Mp4；Matroska 家族才算 Mkv。
        for name in ["a.mp4", "a.m4v", "a.mov", "A.MP4"] {
            assert_eq!(
                Container::from_source_path(Path::new(name)),
                Some(Container::Mp4),
                "{name}"
            );
        }
        for name in ["a.mkv", "a.webm", "A.WEBM"] {
            assert_eq!(
                Container::from_source_path(Path::new(name)),
                Some(Container::Mkv),
                "{name}"
            );
        }
        // 不认识的扩展名：交给调用方按"容器未知"处理。
        assert_eq!(Container::from_source_path(Path::new("a.bin")), None);
        assert_eq!(Container::from_source_path(Path::new("noext")), None);
    }

    #[test]
    fn gstreamer_needs_conversion_for_h264_mkv() {
        // t30 F1：编码对（H.264 4:2:0 8bit）也不等于打得开——`.mkv` 会被
        // `qtdemux` 拒掉（`atom has bogus size`），所以必须判转码到 MP4。
        let verdict = classify_format(
            &details(1920, 1080, "h264", None),
            &fmt_8bit_420(),
            Some(Container::Mkv),
            &gst_caps(),
        );
        match verdict {
            Compatibility::NeedsConversion { reason, target } => {
                assert!(reason.contains("mkv"), "{reason}");
                assert!(reason.contains("MP4/H.264"), "{reason}");
                assert_eq!(target.codec, "h264");
                assert_eq!(target.container, Container::Mp4);
                assert_eq!(target.label(), "h264 8bit (mp4)");
            }
            other => panic!("GStreamer + h264 .mkv 必须判转码，实际 {other:?}"),
        }
    }

    #[test]
    fn gstreamer_plays_h264_mp4() {
        // 发行包的解码链（qtdemux ! h264parse）正好吃这一口。
        assert_eq!(
            classify_format(
                &details(1920, 1080, "h264", None),
                &fmt_8bit_420(),
                Some(Container::Mp4),
                &gst_caps(),
            ),
            Compatibility::Playable
        );
        // m4v/mov 也是 ISO-BMFF，同样可播（容器推断在 from_source_path）。
        for name in ["a.m4v", "a.mov"] {
            assert_eq!(
                Container::from_source_path(Path::new(name)),
                Some(Container::Mp4)
            );
        }
    }

    #[test]
    fn vaapi_plays_h264_mkv() {
        // VA-API 路径用我们自己的 ffmpeg 解封装：容器无关，MKV/H.264 直接可播。
        assert_eq!(
            classify_format(
                &details(1920, 1080, "h264", None),
                &fmt_8bit_420(),
                Some(Container::Mkv),
                &caps(),
            ),
            Compatibility::Playable
        );
    }

    #[test]
    fn gstreamer_needs_conversion_for_hevc_mkv() {
        let verdict = classify_format(
            &details(1920, 1080, "hevc", None),
            &fmt_8bit_420(),
            Some(Container::Mkv),
            &gst_caps(),
        );
        match verdict {
            Compatibility::NeedsConversion { target, .. } => {
                assert_eq!(target.codec, "h264");
                assert_eq!(target.container, Container::Mp4);
            }
            other => panic!("GStreamer + hevc .mkv 必须判转码，实际 {other:?}"),
        }
    }

    #[test]
    fn gstreamer_needs_conversion_for_h264_webm() {
        // `.webm` 同样是 Matroska 家族：`qtdemux` 不认，必须转成 MP4。
        let verdict = classify_format(
            &details(1920, 1080, "h264", None),
            &fmt_8bit_420(),
            Container::from_source_path(Path::new("clip.webm")),
            &gst_caps(),
        );
        match verdict {
            Compatibility::NeedsConversion { reason, target } => {
                assert!(reason.contains("webm"), "{reason}");
                assert!(reason.contains("MP4/H.264"), "{reason}");
                assert!(target.label().contains("mp4"), "{target:?}");
            }
            other => panic!("GStreamer + h264 .webm 必须判转码，实际 {other:?}"),
        }
        // VA-API 构型容器无关：同一个 `.webm` 仍然可播。
        assert_eq!(
            classify_format(
                &details(1920, 1080, "h264", None),
                &fmt_8bit_420(),
                Container::from_source_path(Path::new("clip.webm")),
                &caps(),
            ),
            Compatibility::Playable
        );
    }

    #[test]
    fn container_is_ignored_when_unknown() {
        // 容器未知（罕见扩展名）时不额外收紧：保持既有判据（T30 的判据只对
        // "能确定容器"的素材生效，避免把无扩展名的 ISO-BMFF 误转码）。
        assert_eq!(
            classify_format(
                &details(1920, 1080, "h264", None),
                &fmt_8bit_420(),
                None,
                &gst_caps(),
            ),
            Compatibility::Playable
        );
    }

    // ===== t33：判定用的解码路径必须就是播放用的那条 =====

    #[test]
    fn for_playback_falls_back_to_gstreamer_when_vaapi_is_off_the_table() {
        // 机器能硬解、但"这一次"真的会走 GStreamer 的三种情形（t28 F2 的现场是第 2 条）。
        assert_eq!(
            DecodeConsumer::for_playback(false, true, true, DecodeConsumer::Vaapi),
            DecodeConsumer::GStreamer,
            "没编 vaapi feature"
        );
        assert_eq!(
            DecodeConsumer::for_playback(true, false, true, DecodeConsumer::Vaapi),
            DecodeConsumer::GStreamer,
            "[media].video_vaapi = \"off\""
        );
        assert_eq!(
            DecodeConsumer::for_playback(true, true, false, DecodeConsumer::Vaapi),
            DecodeConsumer::GStreamer,
            "后端不支持 DMA-BUF 扫描输出"
        );
        // 设备本来就打不开时探测结果已经是 GStreamer：保持一致。
        assert_eq!(
            DecodeConsumer::for_playback(true, true, true, DecodeConsumer::GStreamer),
            DecodeConsumer::GStreamer
        );
        // 四条全成立才走零拷贝。
        assert_eq!(
            DecodeConsumer::for_playback(true, true, true, DecodeConsumer::Vaapi),
            DecodeConsumer::Vaapi
        );
    }

    #[test]
    fn vaapi_off_makes_the_same_mkv_need_conversion() {
        // 同一个 `.mkv`：设备可用时 Playable；一旦这次实际走 GStreamer 就必须转码。
        let clip = details(1920, 1080, "h264", None);
        let container = Container::from_source_path(Path::new("clip.mkv"));
        let vaapi = caps().with_consumer(DecodeConsumer::for_playback(
            true,
            true,
            true,
            DecodeConsumer::Vaapi,
        ));
        assert_eq!(
            classify_format(&clip, &fmt_8bit_420(), container, &vaapi),
            Compatibility::Playable
        );
        let gst = caps().with_consumer(DecodeConsumer::for_playback(
            true,
            false,
            true,
            DecodeConsumer::Vaapi,
        ));
        match classify_format(&clip, &fmt_8bit_420(), container, &gst) {
            Compatibility::NeedsConversion { reason, target } => {
                assert!(reason.contains("gstreamer"), "{reason}");
                assert!(reason.contains("mkv"), "{reason}");
                assert!(reason.contains("MP4/H.264"), "{reason}");
                assert_eq!(target.label(), "h264 8bit (mp4)");
            }
            other => panic!("VA-API 关掉后 .mkv 必须判转码，实际 {other:?}"),
        }
    }

    #[test]
    fn vaapi_plays_mkv_in_both_codecs() {
        // 判据矩阵的另一半：VA-API 路径容器无关，h264/hevc 的 `.mkv` 都可播。
        for codec in ["h264", "hevc"] {
            assert_eq!(
                classify_format(
                    &details(1920, 1080, codec, None),
                    &fmt_8bit_420(),
                    Some(Container::Mkv),
                    &caps(),
                ),
                Compatibility::Playable,
                "VA-API + {codec} .mkv"
            );
        }
        // 同一个 hevc `.mkv` 在 GStreamer 路径上必须转码（两条路径判据不同）。
        assert!(
            classify_format(
                &details(1920, 1080, "hevc", None),
                &fmt_8bit_420(),
                Some(Container::Mkv),
                &gst_caps(),
            )
            .needs_conversion()
        );
    }

    #[test]
    fn subsampled_and_ten_bit_sources_need_conversion_on_both_paths() {
        // 422 / 444 / 10bit 在**两种构型**下都不得判 Playable（红线：不许静默软解）。
        let cases = [
            ("h264", "yuv422p"),
            ("h264", "yuv444p"),
            ("hevc", "yuv422p"),
            ("hevc", "yuv444p"),
            ("hevc", "yuv420p10le"),
        ];
        for consumer in [DecodeConsumer::Vaapi, DecodeConsumer::GStreamer] {
            let caps = caps().with_consumer(consumer);
            for (codec, pix_fmt) in cases {
                let verdict = classify(&details(1920, 1080, codec, Some(pix_fmt)), &caps);
                assert!(
                    verdict.needs_conversion(),
                    "{} + {codec} {pix_fmt} 必须判转码，实际 {verdict:?}",
                    consumer.as_str()
                );
            }
        }
    }

    #[test]
    fn unescape_rbsp_handles_over_255_consecutive_zeros() {
        let mut raw = vec![0u8; 300];
        raw.push(0x03);
        raw.push(0xAA);
        let unescaped = unescape_rbsp(&raw);
        assert_eq!(unescaped.len(), 301);
        assert_eq!(unescaped.last().copied(), Some(0xAA));
    }
}
