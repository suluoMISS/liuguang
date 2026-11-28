//! 后台转码（t15）：把硬件解不动的素材转成**能硬解的缓存**。
//!
//! ## 为什么要有这一层
//!
//! t6 把两类素材"产品化"成了明确拒绝（8K H.264 只能软解到 0.086 fps、10bit
//! HEVC 的 P010 表面我方转换链不认）。用户看到的是一句"不支持"，可素材就在
//! 那儿。t15 的答案是**自动修好它**：转成 8bit HEVC（NVDEC 能硬解到 8K），
//! 把结果写进缓存，然后播缓存。
//!
//! ## 约定
//!
//! - **源文件永不修改**：只读源、只写缓存。缓存目录
//!   `$XDG_CACHE_HOME/liuguang/transcode/`（见 [`crate::compat::cache_dir`]）。
//! - 缓存 key = `sha256(源路径 | 大小 | mtime | 转码 profile | 目标宽度)`，
//!   任何一项变了就重新转（例如源文件被替换、`transcode_max_width` 改了）。
//! - **先写 `.part` 再原子 rename**：半成品永远不会被当成可用缓存。
//! - **并发上限 1**：转码吃 CPU/GPU，和播放抢资源没好处；队列按提交顺序跑。
//! - 进度解析 `ffmpeg -progress pipe:1` 的 `out_time_ms=`，**每 500ms** 更新一次
//!   （ffmpeg 自己 0.5s 一行，这里再兜一层节流）。
//! - 失败时把 stderr **尾部 4KB** 留在日志与错误信息里（够看出是编码器缺失
//!   还是素材损坏）。
//!
//! ## 测试/演示用的限长
//!
//! 环境变量 `LIUGUANG_TRANSCODE_LIMIT_SECONDS`（仅 e2e 用）会给 ffmpeg 加
//! `-t <秒>`：验收脚本只转前几秒就够证明链路通了，不必等一整部 8K 素材。

use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::io::Read;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use liuguang_ipc::protocol::PrepareState;

use crate::compat::{Container, DecodeConsumer, H264_HARDWARE_MAX_WIDTH, cache_dir};

/// 进度更新节流（规格：每 500ms 更新一次）。
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// 失败时保留的 stderr 尾巴。
pub const STDERR_TAIL_BYTES: usize = 4096;
/// 单次转码的硬超时（8K 降 4K 也能在这个时间内完成；超时按失败处理）。
pub const JOB_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// 转码 profile（来自 `[media].transcode_crf_or_bitrate` + 目标描述）。
///
/// **产物编码/容器跟着解码路径走**（t26）：VA-API 路径用 `hevc` + `.mkv`，
/// 纯 GStreamer 构型只能吃 `h264` + `.mp4`（管线写死 `qtdemux ! h264parse`）。
/// 两者都在缓存 key 里，避免两种构型互相命中对方的旧缓存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeProfile {
    /// 编码器名（`hevc_nvenc` / `h264_nvenc` / `libx265` …）。
    pub encoder: String,
    /// 编码器参数（`-b:v 20M` …）。
    pub extra_args: Vec<String>,
    /// 目标宽度（0 = 保持源分辨率）。
    pub max_width: u32,
    /// 兜底编码器（硬编失败时重试用；`libx265` / `libx264`）。
    pub fallback_encoder: String,
    /// 目标编码（`hevc` / `h264`）。
    pub codec: &'static str,
    /// 目标容器（`.mkv` / `.mp4`）。
    pub container: Container,
}

impl TranscodeProfile {
    /// 从配置文本解析：`"hevc_nvenc -b:v 20M"`（默认 VA-API 目标：hevc + mkv）。
    ///
    /// 空文本用默认值（不让配置错误变成"静默禁用"）。
    pub fn parse(spec: &str, max_width: u32) -> Self {
        let mut words = spec.split_whitespace();
        let encoder = words.next().unwrap_or("hevc_nvenc").to_string();
        let extra_args: Vec<String> = words.map(str::to_string).collect();
        Self {
            encoder,
            extra_args,
            max_width,
            fallback_encoder: "libx265".to_string(),
            codec: "hevc",
            container: Container::Mkv,
        }
    }

    /// 按**实际解码路径**构造 profile（t26 的核心）。
    ///
    /// - `Vaapi`：沿用配置（默认 `hevc_nvenc -b:v 20M`，`.mkv`）。
    /// - `GStreamer`：强制 H.264 + MP4，宽 ≤ 4096（`transcode_max_width` 更小时听配置）。
    ///   配置里若写的是 HEVC 编码器会被换成对应的 H.264 编码器（`hevc_nvenc` →
    ///   `h264_nvenc`、`libx265` → `libx264`），兜底编码器同步换成 `libx264`。
    pub fn for_consumer(spec: &str, max_width: u32, consumer: DecodeConsumer) -> Self {
        let parsed = Self::parse(spec, max_width);
        match consumer {
            DecodeConsumer::Vaapi => parsed,
            DecodeConsumer::GStreamer => {
                let limit = if max_width == 0 {
                    H264_HARDWARE_MAX_WIDTH
                } else {
                    max_width.min(H264_HARDWARE_MAX_WIDTH)
                };
                Self {
                    encoder: h264_encoder_for(&parsed.encoder),
                    extra_args: parsed.extra_args,
                    max_width: limit,
                    fallback_encoder: "libx264".to_string(),
                    codec: "h264",
                    container: Container::Mp4,
                }
            }
        }
    }

    /// 参与缓存 key 的片段（编码器/参数/宽度/**编码/容器**任一变化都要重转）。
    pub fn key_fragment(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.encoder,
            self.extra_args.join(" "),
            self.max_width,
            self.fallback_encoder,
            self.codec,
            self.container.as_str()
        )
    }
}

/// 把一个（可能是 HEVC 的）编码器名换成 H.264 版本。
fn h264_encoder_for(encoder: &str) -> String {
    let lower = encoder.to_ascii_lowercase();
    if lower.contains("265") || lower.contains("hevc") {
        return encoder
            .replace("hevc", "h264")
            .replace("HEVC", "h264")
            .replace("265", "264");
    }
    encoder.to_string()
}

/// 一个转码任务的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// 源文件（绝对路径，**只读**）。
    pub source: PathBuf,
    /// 缓存目标路径（`<key>.mkv`）。
    pub cache_path: PathBuf,
    /// 缓存 key（暴露出来便于日志/测试）。
    pub key: String,
    /// 源时长（用于把 `out_time_ms` 换算成千分比）。
    pub duration_ms: Option<u64>,
    /// 只转前 N 秒（e2e 用）。
    pub limit_seconds: Option<u32>,
    /// 产出这个任务的 profile（编码/容器/宽度）。
    ///
    /// **跟着实际解码路径走**（t37）：consumer 由调用方按输出求值，`Transcoder`
    /// 不再用设备级 `detect_consumer()` 决定 target。任务自带的 profile 也保证
    /// "提交时算的 key"与"worker 实际跑的编码参数"永远是同一份。
    pub profile: TranscodeProfile,
}

/// 转码任务的对外视图（`prepare_media` 直接映射成 `PrepareMediaResult`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    /// 状态。
    pub state: PrepareState,
    /// 进度千分比。
    pub progress_permille: u32,
    /// 缓存路径（`ready` 时给出）。
    pub cache_path: Option<PathBuf>,
    /// 面向人的说明。
    pub message: Option<String>,
}

/// 转码器的对外事件（`Runtime::poll_transcode` 转成 IPC 事件/自动播放）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscodeEvent {
    /// 转码成功：`cache_path` 可用。
    Finished {
        /// 源文件。
        source: PathBuf,
        /// 缓存文件。
        cache_path: PathBuf,
    },
    /// 转码失败：`message` 是可操作的中文说明。
    Failed {
        /// 源文件。
        source: PathBuf,
        /// 可操作原因。
        message: String,
    },
}

/// 缓存 key：`sha256(路径 | 大小 | mtime | profile | 目标宽度)`。
///
/// 见 [`sha256_hex`]（本仓库没有 `sha2` 依赖，自带一份实现）。
pub fn cache_key(source: &Path, profile: &TranscodeProfile, target_width: u32) -> Option<String> {
    let metadata = std::fs::metadata(source).ok()?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut material = String::new();
    material.push_str(&source.to_string_lossy());
    material.push('\u{0}');
    material.push_str(&metadata.len().to_string());
    material.push('\u{0}');
    material.push_str(&mtime.to_string());
    material.push('\u{0}');
    material.push_str(&profile.key_fragment());
    material.push('\u{0}');
    material.push_str(&target_width.to_string());
    material.push('\u{0}');
    material.push_str(profile.codec);
    material.push('\u{0}');
    material.push_str(profile.container.as_str());
    // key 自带容器扩展名：`cache_path_for` 只拿到 key，也要能还原出正确后缀。
    Some(format!(
        "{}.{}",
        sha256_hex(material.as_bytes()),
        profile.container.extension()
    ))
}

/// 缓存文件路径：`<cache_dir>/<key>`（key 自带 `.mkv` / `.mp4` 后缀）。
///
/// 兼容旧的"纯 hex key"调用：没有已知扩展名时按 `.mkv` 处理。
pub fn cache_path_for(key: &str) -> PathBuf {
    let has_container = Path::new(key)
        .extension()
        .and_then(|value| value.to_str())
        .and_then(Container::from_extension)
        .is_some();
    if has_container {
        cache_dir().join(key)
    } else {
        cache_dir().join(format!("{key}.{}", Container::Mkv.extension()))
    }
}

/// `ffmpeg -progress pipe:1` 的一行里取 `out_time_ms`（微秒单位的历史坑）。
///
/// 返回值单位**毫秒**：ffmpeg 的 `out_time_ms` 实际是微秒，这里除 1000；
/// 负数（ffmpeg 在极端情况会写 `-9223372036854775808`）按 0 处理，
/// 解析失败返回 `None`（忽略这一行）。
pub fn parse_out_time_ms(line: &str) -> Option<i64> {
    let value = line.trim().strip_prefix("out_time_ms=")?;
    let micros: i64 = value.trim().parse().ok()?;
    if micros <= 0 {
        return Some(0);
    }
    Some(micros / 1000)
}

/// `out_time_ms` → 千分比（`duration_ms` 未知时返回 0，交给调用方"未知进度"）。
pub fn progress_permille(out_time_ms: i64, duration_ms: Option<u64>) -> u32 {
    let Some(duration) = duration_ms.filter(|d| *d > 0) else {
        return 0;
    };
    let elapsed = out_time_ms.max(0) as u64;
    let permille = elapsed.saturating_mul(1000) / duration;
    permille.min(1000) as u32
}

/// 拼 ffmpeg 参数（纯函数，便于单测断言）。
///
/// 关键点：`-pix_fmt yuv420p`（我方转换链只认 8bit）、`-progress pipe:1`
/// （进度）、`-f matroska`（输出 `.part` 时扩展名认不出来）、`-an`（壁纸不要音轨）。
pub fn ffmpeg_args(spec: &JobSpec, profile: &TranscodeProfile, encoder: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        spec.source.to_string_lossy().into_owned(),
    ];
    if let Some(limit) = spec.limit_seconds {
        args.push("-t".into());
        args.push(limit.to_string());
    }
    args.extend(["-map".into(), "0:v:0".into(), "-an".into()]);
    args.push("-c:v".into());
    args.push(encoder.to_string());
    if encoder == profile.fallback_encoder {
        // 纯 CPU 兜底：给个体面的预设，别用默认 medium 磨一整天。
        args.extend([
            "-preset".into(),
            "veryfast".into(),
            "-crf".into(),
            "20".into(),
        ]);
    } else {
        args.extend(profile.extra_args.iter().cloned());
    }
    if profile.max_width > 0 {
        // 等比缩放；`min(W, iw)` 保证不把窄素材放大（H.264 目标一律带 4096 上限，
        // 不能因此把 1080p 拉成 4K）。高度取偶数（-2）避免编码器拒绝奇数尺寸。
        // 逗号是 filtergraph 的分隔符，表达式里的逗号必须转义。
        args.extend([
            "-vf".into(),
            format!("scale=min({}\\,iw):-2:flags=lanczos", profile.max_width),
        ]);
    }
    args.extend([
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        "-f".into(),
        profile.container.muxer().to_string(),
        spec.part_path().to_string_lossy().into_owned(),
    ]);
    args
}

impl JobSpec {
    /// `.part` 中间文件路径（原子 rename 的目标）。
    pub fn part_path(&self) -> PathBuf {
        let mut name = self
            .cache_path
            .file_name()
            .unwrap_or_default()
            .to_os_string();
        name.push(".part");
        self.cache_path.with_file_name(name)
    }
}

/// 解析 `[media].transcode` 的值：`auto` / `off` / 可执行文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscodePolicy {
    /// 自动转码（ffmpeg 从 PATH 找）。
    Auto,
    /// 关闭（保持 t6 的行为：只告警 + 明确拒绝）。
    Off,
    /// 用指定可执行文件。
    Program(String),
}

impl TranscodePolicy {
    /// 解析配置值 + 环境变量（`LIUGUANG_TRANSCODE=off` 优先级最高）。
    pub fn resolve(config_value: &str, env_value: Option<&str>) -> Self {
        if let Some(env) = env_value {
            if env.eq_ignore_ascii_case("off") {
                return Self::Off;
            }
        }
        match config_value.trim() {
            "" | "auto" => Self::Auto,
            "off" => Self::Off,
            other => Self::Program(other.to_string()),
        }
    }
}

#[derive(Debug)]
enum WorkerMsg {
    Progress {
        source: PathBuf,
        job_id: u64,
        out_time_ms: i64,
    },
    Done {
        source: PathBuf,
        job_id: u64,
        cache_path: PathBuf,
        stderr_tail: String,
    },
    Failed {
        source: PathBuf,
        job_id: u64,
        message: String,
    },
}

const MAX_QUEUE_LEN: usize = 64;
const MAX_FINISHED_JOBS: usize = 256;
const MAX_STDOUT_LINE_BYTES: usize = 8192;

struct Job {
    id: u64,
    state: PrepareState,
    progress_permille: u32,
    cache_path: Option<PathBuf>,
    message: Option<String>,
    spec: JobSpec,
    started_at: Option<Instant>,
    cancel: Arc<AtomicBool>,
    child_pid: Arc<Mutex<Option<u32>>>,
}

/// 后台转码器（单线程 worker + 队列；`Runtime` 每 tick 调 [`Transcoder::poll`]）。
pub struct Transcoder {
    policy: TranscodePolicy,
    program: Option<String>,
    /// 启动期默认 profile（只用于日志/自检；真正的目标按输出求值，见 [`Self::profile_for`]）。
    profile: TranscodeProfile,
    /// `[media].transcode_crf_or_bitrate` 原值（按输出重算 profile 用）。
    profile_spec: String,
    /// `[media].transcode_max_width` 原值（按输出重算 profile 用）。
    profile_max_width: u32,
    next_job_id: u64,
    jobs: HashMap<PathBuf, Job>,
    queue: VecDeque<PathBuf>,
    running: Option<PathBuf>,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
    /// 是否已经就"找不到 ffmpeg"警告过一次（避免每次播放都刷屏）。
    warned_missing: bool,
    /// 配置里的 `[media].transcode` 原值（错误文案要如实回显）。
    config_value: String,
    /// 环境变量 `LIUGUANG_TRANSCODE` 的原值（`off` 会盖掉配置，文案要区分）。
    env_override: Option<String>,
}

impl Transcoder {
    /// 构造。`program` 是解析出来的 ffmpeg 可执行文件（`None` = 没找到）。
    pub fn new(
        policy: TranscodePolicy,
        program: Option<String>,
        profile: TranscodeProfile,
    ) -> Self {
        let max_width = profile.max_width;
        Self::build(
            policy,
            program,
            profile,
            "auto",
            None,
            "hevc_nvenc -b:v 20M",
            max_width,
        )
    }

    /// 内部构造：带上"配置值 / 环境变量"原值用于错误文案（t21 F8）。
    fn build(
        policy: TranscodePolicy,
        program: Option<String>,
        profile: TranscodeProfile,
        config_value: &str,
        env_override: Option<&str>,
        profile_spec: &str,
        profile_max_width: u32,
    ) -> Self {
        let (tx, rx) = channel();
        Self {
            policy,
            program,
            profile,
            profile_spec: profile_spec.to_string(),
            profile_max_width,
            next_job_id: 1,
            jobs: HashMap::new(),
            queue: VecDeque::new(),
            running: None,
            tx,
            rx,
            warned_missing: false,
            config_value: config_value.to_string(),
            env_override: env_override.map(str::to_string),
        }
    }

    /// 按配置构造（含 PATH 查找与环境变量开关）。
    ///
    /// `consumer` 必须是**调用方按输出求值**的结果（t37 F2）：转码产物要跟着
    /// 实际播放路径走，不能用设备级探测（VAAPI 构建 + `LIUGUANG_VIDEO_VAAPI=off`
    /// 时探测仍是 Vaapi，转出来的 `.mkv/H.265` 播放端打不开）。
    pub fn from_config(
        config_value: &str,
        env_value: Option<&str>,
        profile_spec: &str,
        max_width: u32,
        consumer: DecodeConsumer,
    ) -> Self {
        let policy = TranscodePolicy::resolve(config_value, env_value);
        let program = match &policy {
            TranscodePolicy::Off => None,
            TranscodePolicy::Auto => find_in_path("ffmpeg"),
            TranscodePolicy::Program(path) => Some(path.clone()),
        };
        let profile = TranscodeProfile::for_consumer(profile_spec, max_width, consumer);
        tracing::info!(
            consumer = consumer.as_str(),
            codec = profile.codec,
            container = profile.container.as_str(),
            max_width = profile.max_width,
            "转码产物目标（跟着实际解码路径走；每个任务按输出再确认一次）"
        );
        let mut transcoder = Self::new(policy, program, profile);
        transcoder.profile_spec = profile_spec.to_string();
        transcoder.profile_max_width = max_width;
        transcoder.config_value = config_value.to_string();
        transcoder.env_override = env_value.map(str::to_string);
        // 启动期默认值只用于日志/自检：真正的目标每个任务按输出再算一次（t37）。
        let startup = transcoder.profile();
        tracing::debug!(
            codec = startup.codec,
            container = startup.container.as_str(),
            max_width = startup.max_width,
            "启动期默认转码目标（仅日志；每个任务按输出重算）"
        );
        // 上次运行留下的半成品（可能是被 SIGKILL 的孤儿写的）不能当成缓存候选。
        transcoder.cleanup_stale_parts();
        if matches!(transcoder.policy, TranscodePolicy::Off) {
            tracing::info!("自动转码已关闭（[media].transcode = off / LIUGUANG_TRANSCODE=off）");
        } else if transcoder.program.is_none() {
            tracing::warn!("{}", transcoder.missing_ffmpeg_message());
            transcoder.warned_missing = true;
        }
        transcoder
    }

    /// 转码功能是否可用（配置关了、或 ffmpeg 不存在都算不可用）。
    pub fn enabled(&self) -> bool {
        !matches!(self.policy, TranscodePolicy::Off) && self.program.is_some()
    }

    /// 配置关闭（不是缺 ffmpeg）。
    pub fn disabled_by_config(&self) -> bool {
        matches!(self.policy, TranscodePolicy::Off)
    }

    /// 关掉自动转码的**真实原因**（错误文案要能区分环境变量与配置，t21 F8）。
    ///
    /// 以前无论怎么关，文案都写 `[media].transcode = "auto"` —— 与实际相反。
    pub fn disabled_reason(&self) -> String {
        if let Some(env) = &self.env_override {
            if env.eq_ignore_ascii_case("off") {
                return "LIUGUANG_TRANSCODE=off（环境变量覆盖）".to_string();
            }
        }
        let value = self.config_value.trim();
        if value.is_empty() {
            "[media].transcode = \"\"".to_string()
        } else {
            format!("[media].transcode = \"{value}\"")
        }
    }

    /// 清掉缓存目录里的残留 `.part`（启动时调一次）。
    ///
    /// `.part` 只在转码进行中存在；应用退出/被 SIGKILL 后留下来的都是垃圾，
    /// 保留它们会让下次"看起来像有缓存"（rename 前的同名半成品）。
    /// 返回清掉的个数。
    pub fn cleanup_stale_parts(&self) -> usize {
        cleanup_parts_in(&cache_dir())
    }

    /// 启动期默认 profile（日志/自检用；**不要**拿它算缓存 key —— 见 [`Self::profile_for`]）。
    pub fn profile(&self) -> &TranscodeProfile {
        &self.profile
    }

    /// 按**这个输出实际会用的解码路径**算 profile（t37 的唯一入口）。
    ///
    /// `classify` 判 `NeedsConversion` 时给的 target 就来自同一份计算，
    /// 所以"判定说转成什么"与"worker 真的转成什么"不会再分叉。
    pub fn profile_for(&self, consumer: DecodeConsumer) -> TranscodeProfile {
        TranscodeProfile::for_consumer(&self.profile_spec, self.profile_max_width, consumer)
    }

    /// 热重载转码配置，保留进行中的任务与缓存状态。
    pub fn reconfigure(
        &mut self,
        config_value: &str,
        env_value: Option<&str>,
        profile_spec: &str,
        max_width: u32,
        consumer: DecodeConsumer,
    ) {
        let policy = TranscodePolicy::resolve(config_value, env_value);
        let program = match &policy {
            TranscodePolicy::Off => None,
            TranscodePolicy::Auto => find_in_path("ffmpeg"),
            TranscodePolicy::Program(path) => Some(path.clone()),
        };
        self.policy = policy;
        self.program = program;
        self.config_value = config_value.to_string();
        self.env_override = env_value.map(str::to_string);
        self.profile_spec = profile_spec.to_string();
        self.profile_max_width = max_width;
        self.profile = TranscodeProfile::for_consumer(profile_spec, max_width, consumer);
    }

    /// 找不到 ffmpeg 时的可操作提示。
    pub fn missing_ffmpeg_message(&self) -> String {
        "未找到 ffmpeg，无法自动转换硬件解不动的素材（8K H.264 / 10bit HEVC）：\
         请安装 ffmpeg（例如 `sudo dnf install ffmpeg`），\
         或在 [media].transcode 里写 ffmpeg 的绝对路径，\
         或设 [media].transcode = \"off\" 明确关闭自动转换"
            .to_string()
    }

    /// 同 [`Transcoder::view`]，但**只在缓存的 key 与当前 key 一致时**才回答。
    ///
    /// key 变了说明源文件（大小/mtime）或转码参数变了：旧的 Ready 快照已经过期，
    /// 必须让调用方重新 `submit()` 排队，否则 `prepare_media` 会一直报 ready 却
    /// 指向上一版缓存（t21 F4 的现场）。
    pub fn view_fresh(&self, source: &Path, key: &str) -> Option<JobView> {
        let fresh = self
            .jobs
            .get(source)
            .map(|job| job.spec.key.as_str() == key)
            .unwrap_or(false);
        if fresh { self.view(source) } else { None }
    }

    /// 取缓存 key 对应的任务视图。
    pub fn view(&self, source: &Path) -> Option<JobView> {
        self.jobs.get(source).map(|job| JobView {
            state: job.state,
            progress_permille: job.progress_permille,
            cache_path: job.cache_path.clone(),
            message: job.message.clone(),
        })
    }

    fn alloc_job_id(&mut self) -> u64 {
        let id = self.next_job_id;
        self.next_job_id = self.next_job_id.wrapping_add(1).max(1);
        id
    }

    fn prune_finished_jobs(&mut self) {
        if self.jobs.len() <= MAX_FINISHED_JOBS {
            return;
        }
        let excess = self.jobs.len() - MAX_FINISHED_JOBS;
        let removable: Vec<PathBuf> = self
            .jobs
            .iter()
            .filter(|(p, j)| {
                matches!(j.state, PrepareState::Ready | PrepareState::Failed)
                    && self.running.as_ref() != Some(*p)
            })
            .take(excess)
            .map(|(p, _)| p.clone())
            .collect();
        for p in removable {
            self.jobs.remove(&p);
        }
    }

    fn kill_running(&mut self) {
        if let Some(job) = self.running.as_ref().and_then(|s| self.jobs.get(s)) {
            let pgid = job.child_pid.lock().ok().and_then(|g| *g).unwrap_or(0);
            terminate_process_group(pgid, Arc::clone(&job.child_pid));
        }
    }

    /// 缓存是否已经可用（`ready` + 文件还在 + **key 与当前源一致**）。
    ///
    /// key 里含源文件大小与 mtime（见 [`cache_key`]）：源文件被改动后必须重新转码，
    /// 不能拿旧缓存糊弄（t21 F4：以前只按路径查 Job，`touch` 一下就违约）。
    pub fn ready_cache(&self, source: &Path, key: &str) -> Option<PathBuf> {
        let job = self.jobs.get(source)?;
        if job.state != PrepareState::Ready || job.spec.key != key {
            return None;
        }
        let path = job.cache_path.clone()?;
        path.exists().then_some(path)
    }

    /// 提交任务（幂等）：已在跑/已就绪就不重复排队；`force` 会重转。
    ///
    /// 返回提交后的状态快照。
    pub fn submit(&mut self, spec: JobSpec, force: bool) -> JobView {
        let source = spec.source.clone();
        if !self.enabled() {
            let message = if self.disabled_by_config() {
                "自动转码已在配置里关闭（[media].transcode = \"off\" 或 LIUGUANG_TRANSCODE=off）"
                    .to_string()
            } else {
                self.missing_ffmpeg_message()
            };
            let job_id = self.alloc_job_id();
            self.jobs.insert(
                source.clone(),
                Job {
                    id: job_id,
                    state: PrepareState::Failed,
                    progress_permille: 0,
                    cache_path: None,
                    message: Some(message.clone()),
                    spec,
                    started_at: None,
                    cancel: Arc::new(AtomicBool::new(false)),
                    child_pid: Arc::new(Mutex::new(None)),
                },
            );
            self.prune_finished_jobs();
            return JobView {
                state: PrepareState::Failed,
                progress_permille: 0,
                cache_path: None,
                message: Some(message),
            };
        }

        // 源文件变了（大小/mtime/profile）⇒ cache key 变了：旧 Job 一律作废。
        // 旧缓存文件**留在原地不删**（可能是别的 profile 的成果，也可能用户还要用）。
        let stale = self
            .jobs
            .get(&source)
            .map(|job| job.spec.key != spec.key)
            .unwrap_or(false);
        if stale {
            tracing::info!(
                source = %source.display(),
                "源文件或转码参数已变（缓存 key 变化）：旧任务作废，重新排队"
            );
            if self.running.as_ref() == Some(&source) {
                self.kill_running();
                self.running = None;
            }
            if let Some(old) = self.jobs.remove(&source) {
                old.cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            self.queue.retain(|item| item != &source);
        }

        if let Some(job) = self.jobs.get(&source) {
            let reusable = match job.state {
                PrepareState::Ready | PrepareState::Running | PrepareState::Queued => !force,
                PrepareState::Failed => false,
            };
            if reusable {
                let mut message = job.message.clone();
                if job.state == PrepareState::Ready && message.is_none() {
                    message = Some("缓存已命中（未重新转码）".to_string());
                }
                return JobView {
                    state: job.state,
                    progress_permille: job.progress_permille,
                    cache_path: job.cache_path.clone(),
                    message,
                };
            }
        }

        if !force {
            if let Some(cached) = spec.cache_path.exists().then(|| spec.cache_path.clone()) {
                // 缓存已经在磁盘上（daemon 重启过）：直接可用，不必再转。
                let job_id = self.alloc_job_id();
                self.jobs.insert(
                    source.clone(),
                    Job {
                        id: job_id,
                        state: PrepareState::Ready,
                        progress_permille: 1000,
                        cache_path: Some(cached.clone()),
                        message: Some("缓存已存在，直接复用（未重新转码）".to_string()),
                        spec,
                        started_at: None,
                        cancel: Arc::new(AtomicBool::new(false)),
                        child_pid: Arc::new(Mutex::new(None)),
                    },
                );
                self.prune_finished_jobs();
                return JobView {
                    state: PrepareState::Ready,
                    progress_permille: 1000,
                    cache_path: Some(cached),
                    message: Some("缓存已存在，直接复用（未重新转码）".to_string()),
                };
            }
        }

        if let Some(pos) = self.queue.iter().position(|item| item == &source) {
            self.queue.remove(pos);
        } else if self.queue.len() >= MAX_QUEUE_LEN {
            return JobView {
                state: PrepareState::Failed,
                progress_permille: 0,
                cache_path: None,
                message: Some(format!(
                    "转码队列已满（上限 {MAX_QUEUE_LEN} 项），请稍后再试"
                )),
            };
        }
        if self.running.as_ref() == Some(&source) {
            self.kill_running();
            self.running = None;
        }
        if let Some(old) = self.jobs.get(&source) {
            old.cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        let job_id = self.alloc_job_id();
        self.jobs.insert(
            source.clone(),
            Job {
                id: job_id,
                state: PrepareState::Queued,
                progress_permille: 0,
                cache_path: None,
                message: None,
                spec,
                started_at: None,
                cancel: Arc::new(AtomicBool::new(false)),
                child_pid: Arc::new(Mutex::new(None)),
            },
        );
        self.prune_finished_jobs();
        self.queue.push_back(source.clone());
        self.pump();
        // `pump` 可能已经把任务推进到 running/failed，所以按 jobs 里的真实状态回答；
        // 万一没有（例如刚被别的路径清掉），退回"已排队"这个保守快照，**不 panic**。
        self.jobs.get(&source).map_or(
            JobView {
                state: PrepareState::Queued,
                progress_permille: 0,
                cache_path: None,
                message: None,
            },
            |job| JobView {
                state: job.state,
                progress_permille: job.progress_permille,
                cache_path: job.cache_path.clone(),
                message: job.message.clone(),
            },
        )
    }

    /// 起下一个（并发上限 1）。
    fn pump(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(source) = self.queue.pop_front() else {
            return;
        };
        let Some(job) = self.jobs.get_mut(&source) else {
            return;
        };
        let spec = job.spec.clone();
        let job_id = job.id;
        let cancel = Arc::new(AtomicBool::new(false));
        let child_pid = Arc::new(Mutex::new(None));
        job.cancel = Arc::clone(&cancel);
        job.child_pid = Arc::clone(&child_pid);
        job.state = PrepareState::Running;
        job.progress_permille = 0;
        job.started_at = Some(Instant::now());
        let Some(program) = self.program.clone() else {
            job.state = PrepareState::Failed;
            job.started_at = None;
            return;
        };
        let profile = spec.profile.clone();
        let tx = self.tx.clone();
        self.running = Some(source.clone());
        if let Err(err) = std::thread::Builder::new()
            .name("liuguang-transcode".to_string())
            .spawn(move || worker(program, profile, spec, job_id, tx, child_pid, cancel))
        {
            self.running = None;
            if let Some(job) = self.jobs.get_mut(&source) {
                job.state = PrepareState::Failed;
                job.started_at = None;
                job.message = Some(format!("启动转码线程失败: {err}"));
            }
        }
    }

    /// 把后台线程的消息收进状态机，返回需要对外广播的事件。
    pub fn poll(&mut self) -> Vec<TranscodeEvent> {
        let mut events = Vec::new();
        while let Ok(message) = self.rx.try_recv() {
            match message {
                WorkerMsg::Progress {
                    source,
                    job_id,
                    out_time_ms,
                } => {
                    if let Some(job) = self.jobs.get_mut(&source)
                        && job.id == job_id
                    {
                        job.progress_permille =
                            progress_permille(out_time_ms, job.spec.duration_ms);
                        job.state = PrepareState::Running;
                    }
                }
                WorkerMsg::Done {
                    source,
                    job_id,
                    cache_path,
                    stderr_tail,
                } => {
                    let matches_id = self.jobs.get(&source).is_some_and(|job| job.id == job_id);
                    if self.running.as_ref() == Some(&source) && matches_id {
                        self.running = None;
                    }
                    let is_current = matches_id
                        && self.jobs.get(&source).is_some_and(|job| {
                            !job.cancel.load(std::sync::atomic::Ordering::Acquire)
                        });
                    if !is_current {
                        tracing::debug!(source = %source.display(), job_id, "忽略已作废转码任务的完成消息");
                        continue;
                    }
                    if let Some(job) = self.jobs.get_mut(&source) {
                        job.state = PrepareState::Ready;
                        job.progress_permille = 1000;
                        job.cache_path = Some(cache_path.clone());
                        job.message = None;
                        job.started_at = None;
                    }
                    if !stderr_tail.trim().is_empty() {
                        tracing::debug!(source = %source.display(), "转码 stderr 尾部: {stderr_tail}");
                    }
                    tracing::info!(source = %source.display(), cache = %cache_path.display(), "转码完成");
                    events.push(TranscodeEvent::Finished { source, cache_path });
                }
                WorkerMsg::Failed {
                    source,
                    job_id,
                    message,
                } => {
                    let matches_id = self.jobs.get(&source).is_some_and(|job| job.id == job_id);
                    if self.running.as_ref() == Some(&source) && matches_id {
                        self.running = None;
                    }
                    if !matches_id {
                        tracing::debug!(source = %source.display(), job_id, "忽略旧转码任务的失败消息");
                        continue;
                    }
                    let already_failed = self
                        .jobs
                        .get(&source)
                        .map(|job| {
                            job.state == PrepareState::Failed
                                || job.cancel.load(std::sync::atomic::Ordering::Acquire)
                        })
                        .unwrap_or(true);
                    if let Some(job) = self.jobs.get_mut(&source) {
                        job.state = PrepareState::Failed;
                        job.message = Some(message.clone());
                        job.started_at = None;
                    }
                    if already_failed {
                        // 超时/退出的主动杀进程也会让 worker 报一次失败：别再推一条事件。
                        tracing::debug!(source = %source.display(), "转码 worker 收尾（已判定失败）");
                        continue;
                    }
                    tracing::warn!(source = %source.display(), "转码失败: {message}");
                    events.push(TranscodeEvent::Failed { source, message });
                }
            }
        }
        // 队列里还有活就继续（上一个刚刚失败/完成）。
        self.pump();
        events
    }

    /// 超时看门狗（`Runtime::tick` 调）：把跑太久的任务判失败。
    pub fn enforce_timeouts(&mut self, now: Instant) -> Vec<TranscodeEvent> {
        let mut events = Vec::new();
        let Some(running) = self.running.clone() else {
            return events;
        };
        let Some(job) = self.jobs.get(&running) else {
            return events;
        };
        if let Some(started) = job.started_at {
            if now.duration_since(started) > JOB_TIMEOUT {
                let message = format!(
                    "转码超过 {} 分钟仍未完成，已放弃：请检查 ffmpeg 是否可用，\
                     或把 [media].transcode_max_width 设小一点再试",
                    JOB_TIMEOUT.as_secs() / 60
                );
                if let Some(job) = self.jobs.get_mut(&running) {
                    job.cancel.store(true, std::sync::atomic::Ordering::Release);
                    job.state = PrepareState::Failed;
                    job.message = Some(message.clone());
                    job.started_at = None;
                    // 只标失败不杀进程 = 孤儿继续写 .part（t21 F6）：整组杀掉。
                    let part = job.spec.part_path();
                    let pgid = job.child_pid.lock().ok().and_then(|g| *g).unwrap_or(0);
                    terminate_process_group(pgid, Arc::clone(&job.child_pid));
                    let _ = std::fs::remove_file(&part);
                }
                self.running = None;
                events.push(TranscodeEvent::Failed {
                    source: running,
                    message,
                });
                self.pump();
            }
        }
        events
    }
}

impl Transcoder {
    /// 主动收尾（daemon 退出时调）：杀掉正在跑的 ffmpeg 进程组并清掉它的 `.part`。
    ///
    /// 不杀的话，进程组会活过 daemon：孤儿继续写 `.part`，下次启动还得多扫一遍
    /// （t21 F6 的现场）。这里同步发 SIGTERM，800ms 后补 SIGKILL（见
    /// [`terminate_process_group`]）。
    pub fn shutdown(&mut self) {
        if let Some(source) = &self.running {
            if let Some(job) = self.jobs.get(source) {
                job.cancel.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let spec = self
            .running
            .as_ref()
            .and_then(|source| self.jobs.get(source))
            .map(|job| job.spec.clone());
        self.kill_running();
        if let Some(spec) = spec {
            let _ = std::fs::remove_file(spec.part_path());
        }
        self.running = None;
        self.queue.clear();
    }
}

impl Drop for Transcoder {
    fn drop(&mut self) {
        self.kill_running();
    }
}

/// 清掉某个目录里的 `*.part`（`cleanup_stale_parts` 的纯函数版本，便于单测）。
fn cleanup_parts_in(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("part")
            && std::fs::remove_file(&path).is_ok()
        {
            tracing::info!(part = %path.display(), "清掉上次运行留下的转码半成品");
            removed += 1;
        }
    }
    removed
}

/// 杀一个进程组：先 SIGTERM，800ms 后补 SIGKILL。
///
/// 用 `kill` 命令而不是 `libc::kill`：workspace 的 `unsafe_code = "forbid"` 不允许
/// 直接 FFI，而子进程是用 `process_group(0)` 起的 ⇒ `-<pgid>` 就是它自己那一组。
fn terminate_process_group(pgid: u32, pid_slot: Arc<Mutex<Option<u32>>>) {
    if pgid == 0 {
        return;
    }
    let term = Command::new("kill")
        .arg("-TERM")
        .arg(format!("-{pgid}"))
        .status();
    if term.is_err() {
        tracing::debug!(pgid, "SIGTERM 进程组失败（可能已经退出）");
    }
    std::thread::Builder::new()
        .name("liuguang-transcode-reaper".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            // 只有原任务尚未完成 child.wait() 时才补 SIGKILL，杜绝 PID 回收误杀无关进程组。
            if pid_slot.lock().ok().and_then(|g| *g) == Some(pgid) {
                let _ = Command::new("kill")
                    .arg("-KILL")
                    .arg(format!("-{pgid}"))
                    .status();
            }
        })
        .ok();
}

/// worker 线程：跑 ffmpeg（先主编码器、失败再兜底），把进度/结果发回主线程。
fn worker(
    program: String,
    profile: TranscodeProfile,
    spec: JobSpec,
    job_id: u64,
    tx: Sender<WorkerMsg>,
    child_pid: Arc<Mutex<Option<u32>>>,
    cancel: Arc<AtomicBool>,
) {
    let mut last_error = String::new();
    let mut encoders = vec![profile.encoder.clone()];
    if profile.fallback_encoder != profile.encoder {
        encoders.push(profile.fallback_encoder.clone());
    }
    for encoder in encoders {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        match run_ffmpeg(&program, &profile, &spec, job_id, &encoder, &tx, &child_pid) {
            Ok(stderr_tail) => {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    let _ = std::fs::remove_file(spec.part_path());
                    break;
                }
                if rename_part(&spec).is_ok() {
                    let _ = tx.send(WorkerMsg::Done {
                        source: spec.source.clone(),
                        job_id,
                        cache_path: spec.cache_path.clone(),
                        stderr_tail,
                    });
                } else {
                    let _ = tx.send(WorkerMsg::Failed {
                        source: spec.source.clone(),
                        job_id,
                        message: format!(
                            "转码完成但无法写入缓存 {}：请检查磁盘空间与目录权限",
                            spec.cache_path.display()
                        ),
                    });
                }
                return;
            }
            Err(message) => {
                last_error = message;
                tracing::warn!(
                    source = %spec.source.display(),
                    encoder,
                    "编码器失败，{last_error}"
                );
            }
        }
    }
    let _ = tx.send(WorkerMsg::Failed {
        source: spec.source.clone(),
        job_id,
        message: format!(
            "自动转换失败（{}）：{last_error}\n\
             可尝试：把 [media].transcode_crf_or_bitrate 改成 `libx265 -crf 20`（纯 CPU 编码），\
             或降低 [media].transcode_max_width，或直接改用 8bit 版本素材",
            spec.source.display()
        ),
    });
}

/// 跑一次 ffmpeg；成功返回 stderr 尾部。
fn run_ffmpeg(
    program: &str,
    profile: &TranscodeProfile,
    spec: &JobSpec,
    job_id: u64,
    encoder: &str,
    tx: &Sender<WorkerMsg>,
    child_pid: &Arc<Mutex<Option<u32>>>,
) -> Result<String, String> {
    if let Some(parent) = spec.cache_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "无法创建缓存目录 {}：{error}（请检查 XDG_CACHE_HOME 的权限）",
                parent.display()
            )
        })?;
    }
    let args = ffmpeg_args(spec, profile, encoder);
    tracing::info!(
        source = %spec.source.display(),
        encoder,
        args = ?args,
        "开始后台转码"
    );
    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 独立进程组：退出/超时时可以整组杀掉（t21 F6），不会留下写 `.part` 的孤儿。
        .process_group(0)
        .spawn()
        .map_err(|error| {
            format!(
                "无法启动 ffmpeg（{program}）：{error}（请确认路径可执行，或改 [media].transcode）"
            )
        })?;
    let pid = child.id();
    if let Ok(mut slot) = child_pid.lock() {
        *slot = Some(pid);
    }

    // stderr 单独读（固定块读入尾部 4KB，避免长行 OOM），否则管道写满会让 ffmpeg 卡死。
    let stderr = child.stderr.take();
    let stderr_handle = stderr.map(|mut stderr| {
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut tail = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > STDERR_TAIL_BYTES {
                            let cut = tail.len() - STDERR_TAIL_BYTES;
                            tail.drain(..cut);
                        }
                    }
                }
            }
            String::from_utf8_lossy(&tail).into_owned()
        })
    });

    let stdout = child.stdout.take();
    if let Some(stdout) = stdout {
        use std::io::Read as _;
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::with_capacity(256);
        let mut last_sent = Instant::now()
            .checked_sub(PROGRESS_INTERVAL)
            .unwrap_or_else(Instant::now);
        loop {
            buf.clear();
            let n = (&mut reader)
                .take(MAX_STDOUT_LINE_BYTES as u64)
                .read_until(b'\n', &mut buf)
                .unwrap_or(0);
            if n == 0 {
                break;
            }
            if let Ok(line) = std::str::from_utf8(&buf)
                && let Some(out_time_ms) = parse_out_time_ms(line)
                && last_sent.elapsed() >= PROGRESS_INTERVAL
            {
                last_sent = Instant::now();
                let _ = tx.send(WorkerMsg::Progress {
                    source: spec.source.clone(),
                    job_id,
                    out_time_ms,
                });
            }
        }
    }

    let status = child
        .wait()
        .map_err(|error| format!("等待 ffmpeg 失败：{error}"))?;
    if let Ok(mut slot) = child_pid.lock() {
        *slot = None;
    }
    let stderr_tail = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if status.success() {
        return Ok(stderr_tail);
    }
    let reason = if status.code().is_none() {
        "转码被中断（进程被信号终止：超时或 daemon 退出），未完成的任务已放弃".to_string()
    } else if stderr_tail.contains("Unknown encoder") {
        format!(
            "ffmpeg 没有 {encoder} 编码器（请检查 ffmpeg 构建，或把 \
             [media].transcode_crf_or_bitrate 改成 `libx265 -crf 20`）"
        )
    } else if stderr_tail.contains("No such file") {
        format!("找不到输入文件（{}）", spec.source.display())
    } else {
        format!("ffmpeg 退出码 {:?}", status.code())
    };
    // 失败的半成品必须清掉，否则下次会被误当成缓存。
    let _ = std::fs::remove_file(spec.part_path());
    Err(format!("{reason}；stderr: {}", stderr_tail.trim()))
}

fn rename_part(spec: &JobSpec) -> std::io::Result<()> {
    std::fs::rename(spec.part_path(), &spec.cache_path)
}

/// 在 PATH 里找可执行文件。
pub fn find_in_path(name: &str) -> Option<String> {
    if name.contains('/') {
        return Path::new(name).is_file().then(|| name.to_string());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// 自带的 SHA-256（workspace 没有 `sha2` 依赖，缓存 key 不值得为此加依赖）。
pub fn sha256_hex(input: &[u8]) -> String {
    let digest = sha256(input);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = input.to_vec();
    let bit_len = (input.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// 当前进程能否找到 ffmpeg（`prepare_media` 的快速自检）。
/// `Read` 的便捷封装：把 `-progress` 的输出按行读完（测试用）。
#[cfg(test)]
pub fn read_progress_lines(mut reader: impl Read) -> Vec<i64> {
    let mut out = Vec::new();
    let mut line = String::new();
    let mut buffered = BufReader::new(&mut reader);
    loop {
        line.clear();
        match buffered.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Some(ms) = parse_out_time_ms(&line) {
                    out.push(ms);
                }
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn profile() -> TranscodeProfile {
        TranscodeProfile::parse("hevc_nvenc -b:v 20M", 0)
    }

    fn spec(source: &Path, cache_path: PathBuf) -> JobSpec {
        JobSpec {
            source: source.to_path_buf(),
            cache_path,
            key: "deadbeef".to_string(),
            duration_ms: Some(10_000),
            limit_seconds: None,
            profile: profile(),
        }
    }

    /// 按真实 cache key 造 spec（F4 的"key 变化"场景要用）。
    fn spec_for(source: &Path, dir: &Path) -> JobSpec {
        let profile = profile();
        let key = cache_key(source, &profile, profile.max_width).unwrap();
        JobSpec {
            source: source.to_path_buf(),
            cache_path: dir.join(format!("{key}.mkv")),
            key,
            duration_ms: Some(1000),
            limit_seconds: None,
            profile,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "liuguang-transcode-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn cache_key_reacts_to_every_ingredient() {
        let dir = temp_dir("key");
        let source = dir.join("a.mp4");
        std::fs::write(&source, b"first").unwrap();
        let base = cache_key(&source, &profile(), 0).unwrap();

        // 同一份文件 → 稳定。
        assert_eq!(cache_key(&source, &profile(), 0).unwrap(), base);

        // 大小变化。
        std::fs::write(&source, b"second-and-longer").unwrap();
        let after_size = cache_key(&source, &profile(), 0).unwrap();
        assert_ne!(base, after_size);

        // profile 变化（换编码器 / 换码率 / 换兜底）。
        let mut other = profile();
        other.extra_args = vec!["-b:v".to_string(), "8M".to_string()];
        assert_ne!(base, cache_key(&source, &other, 0).unwrap());
        let mut renamed = profile();
        renamed.encoder = "libx265".to_string();
        assert_ne!(base, cache_key(&source, &renamed, 0).unwrap());

        // 目标宽度变化（8K 降 4K 与不降是两份缓存）。
        assert_ne!(base, cache_key(&source, &profile(), 3840).unwrap());

        // 路径变化（同名不同目录）。
        let copy = dir.join("b.mp4");
        std::fs::copy(&source, &copy).unwrap();
        assert_ne!(
            cache_key(&source, &profile(), 0).unwrap(),
            cache_key(&copy, &profile(), 0).unwrap()
        );

        // mtime 变化。
        let touched = dir.join("c.mp4");
        std::fs::write(&touched, b"first").unwrap();
        let before = cache_key(&touched, &profile(), 0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&touched)
            .unwrap();
        file.set_modified(std::time::SystemTime::now()).unwrap();
        assert_ne!(before, cache_key(&touched, &profile(), 0).unwrap());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_key_is_none_for_missing_files() {
        assert!(cache_key(Path::new("/nonexistent/nope.mp4"), &profile(), 0).is_none());
    }

    #[test]
    fn out_time_ms_bounds() {
        // ffmpeg 的 out_time_ms 是**微秒**，这里换算成毫秒。
        assert_eq!(parse_out_time_ms("out_time_ms=1500000\n"), Some(1500));
        assert_eq!(parse_out_time_ms("out_time_ms=0"), Some(0));
        assert_eq!(
            parse_out_time_ms("out_time_ms=-9223372036854775808"),
            Some(0)
        );
        assert_eq!(parse_out_time_ms("out_time_ms=-500"), Some(0));
        assert_eq!(parse_out_time_ms("out_time_ms="), None);
        assert_eq!(parse_out_time_ms(""), None);
        assert_eq!(parse_out_time_ms("frame=10"), None);
        assert_eq!(parse_out_time_ms("out_time=00:00:01.5"), None);
        assert_eq!(parse_out_time_ms("  out_time_ms=2000000  "), Some(2000));
    }

    #[test]
    fn progress_permille_clamps_and_handles_unknown_duration() {
        assert_eq!(progress_permille(0, Some(10_000)), 0);
        assert_eq!(progress_permille(5_000, Some(10_000)), 500);
        assert_eq!(progress_permille(10_000, Some(10_000)), 1000);
        // 超出总时长（容器报短了）：夹到 1000，不溢出。
        assert_eq!(progress_permille(15_000, Some(10_000)), 1000);
        // 时长未知 / 为 0：维持 0（调用方据此显示"未知进度"）。
        assert_eq!(progress_permille(5_000, None), 0);
        assert_eq!(progress_permille(5_000, Some(0)), 0);
        // 负数（ffmpeg 早中期会写负值）按 0 处理。
        assert_eq!(progress_permille(-1, Some(10_000)), 0);
    }

    #[test]
    fn ffmpeg_args_pin_the_eight_bit_pipeline() {
        let dir = temp_dir("args");
        let args = ffmpeg_args(
            &spec(&dir.join("in.mp4"), dir.join("out.mkv")),
            &profile(),
            "hevc_nvenc",
        );
        let joined = args.join(" ");
        assert!(joined.contains("-pix_fmt yuv420p"), "{joined}");
        assert!(joined.contains("-progress pipe:1"), "{joined}");
        assert!(joined.contains("-f matroska"), "{joined}");
        assert!(joined.contains("-c:v hevc_nvenc"), "{joined}");
        assert!(joined.contains("-b:v 20M"), "{joined}");
        assert!(joined.contains("-an"), "{joined}");
        // 不缩放时不该出现 -vf。
        assert!(!joined.contains("-vf"), "{joined}");
        // 输出是 .part（原子 rename）。
        assert!(joined.contains("out.mkv.part"), "{joined}");

        // 给宽度就会加等比缩放，且高度用 -2 保证偶数。
        let mut scaled = profile();
        scaled.max_width = 3840;
        let args = ffmpeg_args(
            &spec(&dir.join("in.mp4"), dir.join("out.mkv")),
            &scaled,
            "hevc_nvenc",
        );
        // `min(W, iw)`：既缩到上限，又不把窄素材放大（逗号必须转义）。
        assert!(
            args.join(" ")
                .contains("scale=min(3840\\,iw):-2:flags=lanczos"),
            "{}",
            args.join(" ")
        );

        // 兜底编码器用 preset/crf，不带硬编的 -b:v。
        let args = ffmpeg_args(
            &spec(&dir.join("in.mp4"), dir.join("out.mkv")),
            &profile(),
            "libx265",
        );
        let joined = args.join(" ");
        assert!(joined.contains("-preset veryfast"), "{joined}");
        assert!(joined.contains("-crf 20"), "{joined}");
        assert!(!joined.contains("-b:v 20M"), "{joined}");

        // e2e 限长：加 -t。
        let mut limited = spec(&dir.join("in.mp4"), dir.join("out.mkv"));
        limited.limit_seconds = Some(3);
        let args = ffmpeg_args(&limited, &profile(), "hevc_nvenc");
        assert!(args.join(" ").contains("-t 3"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn policy_resolution_prefers_the_environment_switch() {
        assert_eq!(
            TranscodePolicy::resolve("auto", None),
            TranscodePolicy::Auto
        );
        assert_eq!(TranscodePolicy::resolve("", None), TranscodePolicy::Auto);
        assert_eq!(TranscodePolicy::resolve("off", None), TranscodePolicy::Off);
        assert_eq!(
            TranscodePolicy::resolve("/opt/bin/ffmpeg", None),
            TranscodePolicy::Program("/opt/bin/ffmpeg".to_string())
        );
        // 环境变量优先。
        assert_eq!(
            TranscodePolicy::resolve("auto", Some("off")),
            TranscodePolicy::Off
        );
        assert_eq!(
            TranscodePolicy::resolve("auto", Some("OFF")),
            TranscodePolicy::Off
        );
        assert_eq!(
            TranscodePolicy::resolve("auto", Some("on")),
            TranscodePolicy::Auto
        );
    }

    #[test]
    fn missing_ffmpeg_fails_with_an_actionable_message() {
        let mut transcoder = Transcoder::new(TranscodePolicy::Auto, None, profile());
        assert!(!transcoder.enabled());
        let dir = temp_dir("missing");
        let view = transcoder.submit(spec(&dir.join("in.mp4"), dir.join("out.mkv")), false);
        assert_eq!(view.state, PrepareState::Failed);
        let message = view.message.unwrap_or_default();
        assert!(message.contains("未找到 ffmpeg"), "{message}");
        assert!(message.contains("dnf install ffmpeg"), "{message}");
        assert!(message.contains("[media].transcode"), "{message}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_transcoder_never_queues_anything() {
        let mut transcoder =
            Transcoder::new(TranscodePolicy::Off, Some("/bin/true".into()), profile());
        assert!(!transcoder.enabled());
        assert!(transcoder.disabled_by_config());
        let dir = temp_dir("off");
        let view = transcoder.submit(spec(&dir.join("in.mp4"), dir.join("out.mkv")), false);
        assert_eq!(view.state, PrepareState::Failed);
        assert!(view.message.unwrap_or_default().contains("关闭"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn existing_cache_is_reused_without_transcoding() {
        let dir = temp_dir("reuse");
        let source = dir.join("in.mp4");
        std::fs::write(&source, b"x").unwrap();
        // program 指向不存在的路径：真去转码会失败，能返回 ready 就说明复用了缓存。
        let mut transcoder = Transcoder::new(
            TranscodePolicy::Program("/bin/true".into()),
            Some("/bin/true".to_string()),
            profile(),
        );
        let job = spec_for(&source, &dir);
        std::fs::write(&job.cache_path, b"cached").unwrap();
        let view = transcoder.submit(job.clone(), false);
        assert_eq!(view.state, PrepareState::Ready);
        assert_eq!(view.progress_permille, 1000);
        assert_eq!(view.cache_path.as_deref(), Some(job.cache_path.as_path()));
        assert!(
            view.message.unwrap_or_default().contains("未重新转码"),
            "命中缓存的返回要能看出来"
        );
        assert_eq!(
            transcoder.ready_cache(&source, &job.key).as_deref(),
            Some(job.cache_path.as_path())
        );
        // key 不匹配就不算命中。
        assert_eq!(transcoder.ready_cache(&source, "other-key"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn touched_source_is_no_longer_a_cache_hit() {
        // t21 F4：key 含 mtime，但旧实现只按路径复用 Job ⇒ touch 后仍秒回旧缓存。
        let dir = temp_dir("touch");
        let source = dir.join("in.mp4");
        std::fs::write(&source, b"first").unwrap();
        let first = spec_for(&source, &dir);
        std::fs::write(&first.cache_path, b"cached").unwrap();
        let mut transcoder = Transcoder::new(
            TranscodePolicy::Program("/bin/true".into()),
            Some("/bin/true".to_string()),
            profile(),
        );
        let view = transcoder.submit(first.clone(), false);
        assert_eq!(view.state, PrepareState::Ready);
        let ready = transcoder
            .ready_cache(&source, &first.key)
            .expect("首轮应命中");
        assert_eq!(ready, first.cache_path);

        // 改内容（大小 + mtime 一起变）⇒ 新 key、新缓存路径、旧 Job 作废。
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&source, b"second-and-longer").unwrap();
        let second = spec_for(&source, &dir);
        assert_ne!(first.key, second.key);
        assert_ne!(first.cache_path, second.cache_path);
        assert_eq!(transcoder.ready_cache(&source, &second.key), None);

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap();
        file.set_modified(std::time::SystemTime::now()).unwrap();
        let view = transcoder.submit(second.clone(), false);
        assert_ne!(view.state, PrepareState::Ready, "key 变了不能复用旧缓存");
        assert_eq!(view.cache_path, None);
        // 旧缓存文件留在原地（没人删它）。
        assert!(first.cache_path.exists());
        transcoder.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_job_is_replaced_when_the_profile_changes() {
        let dir = temp_dir("stale");
        let source = dir.join("in.mp4");
        std::fs::write(&source, b"x").unwrap();
        let mut transcoder = Transcoder::new(
            TranscodePolicy::Program("/bin/true".into()),
            Some("/bin/true".to_string()),
            profile(),
        );
        let first = spec_for(&source, &dir);
        std::fs::write(&first.cache_path, b"cached").unwrap();
        assert_eq!(
            transcoder.submit(first.clone(), false).state,
            PrepareState::Ready
        );

        let mut other = first.clone();
        other.key = format!("{}-other", first.key);
        other.cache_path = dir.join("other.mkv");
        let view = transcoder.submit(other.clone(), false);
        assert_ne!(view.state, PrepareState::Ready);
        assert_eq!(transcoder.ready_cache(&source, &first.key), None);
        transcoder.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_reason_names_the_real_switch() {
        // t21 F8：环境变量关的不能写成 [media].transcode="auto"。
        let env_off = Transcoder::build(
            TranscodePolicy::Off,
            None,
            profile(),
            "auto",
            Some("off"),
            "hevc_nvenc -b:v 20M",
            0,
        );
        let reason = env_off.disabled_reason();
        assert!(reason.contains("LIUGUANG_TRANSCODE=off"), "{reason}");

        let config_off = Transcoder::build(
            TranscodePolicy::Off,
            None,
            profile(),
            "off",
            None,
            "hevc_nvenc -b:v 20M",
            0,
        );
        let reason = config_off.disabled_reason();
        assert!(reason.contains("[media].transcode"), "{reason}");
        assert!(reason.contains("\"off\""), "{reason}");
        assert!(!reason.contains("auto"), "{reason}");
        assert!(!reason.contains("  "), "文案不该有连续空格：{reason}");
    }

    #[test]
    fn stale_part_files_are_cleaned_on_startup() {
        let dir = temp_dir("parts");
        std::fs::write(dir.join("a.mkv.part"), b"half").unwrap();
        std::fs::write(dir.join("b.mkv"), b"done").unwrap();
        assert_eq!(cleanup_parts_in(&dir), 1);
        assert!(!dir.join("a.mkv.part").exists());
        assert!(dir.join("b.mkv").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_kills_the_ffmpeg_process_group() {
        // t21 F6：daemon 退出/超时必须真的杀掉子进程，否则孤儿继续写 .part。
        let dir = temp_dir("kill");
        let marker = dir.join("pid");
        let script = dir.join("fake-ffmpeg.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > {}\nexec sleep 30\n", marker.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let source = dir.join("in.mp4");
        std::fs::write(&source, b"x").unwrap();
        let mut transcoder = Transcoder::new(
            TranscodePolicy::Program(script.to_string_lossy().into_owned()),
            Some(script.to_string_lossy().into_owned()),
            profile(),
        );
        let view = transcoder.submit(spec_for(&source, &dir), false);
        assert!(
            matches!(view.state, PrepareState::Queued | PrepareState::Running),
            "{view:?}"
        );
        // 等脚本把自己写进 marker。
        let mut pid = None;
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if let Ok(value) = text.trim().parse::<u32>() {
                    pid = Some(value);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = pid.expect("假 ffmpeg 没有启动");
        assert_eq!(
            Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .status()
                .unwrap()
                .code(),
            Some(0),
            "进程应当还活着"
        );

        transcoder.shutdown();
        // SIGTERM → 800ms 后 SIGKILL，给到 4s 的观察窗口。
        let mut dead = false;
        for _ in 0..80 {
            let alive = Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !alive {
                dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(dead, "daemon 退出后 ffmpeg 进程组应已消失");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn progress_lines_are_parsed_from_a_stream() {
        let mut buffer: Vec<u8> = Vec::new();
        buffer.extend_from_slice(b"frame=1\nout_time_ms=500000\nprogress=continue\n");
        buffer.extend_from_slice(b"out_time_ms=2500000\nprogress=end\n");
        buffer.flush().unwrap();
        assert_eq!(read_progress_lines(&buffer[..]), vec![500, 2500]);
    }

    #[test]
    fn profile_spec_parsing_keeps_encoder_and_args() {
        let profile = TranscodeProfile::parse("hevc_vaapi -qp 24", 3840);
        assert_eq!(profile.encoder, "hevc_vaapi");
        assert_eq!(
            profile.extra_args,
            vec!["-qp".to_string(), "24".to_string()]
        );
        assert_eq!(profile.max_width, 3840);
        assert_eq!(profile.fallback_encoder, "libx265");
        assert!(profile.key_fragment().contains("hevc_vaapi"));
        // 空配置回落到默认编码器，而不是把转码关掉。
        assert_eq!(TranscodeProfile::parse("", 0).encoder, "hevc_nvenc");
    }

    #[test]
    fn part_path_is_a_sibling_and_not_the_cache() {
        let dir = temp_dir("part");
        let job = spec(&dir.join("in.mp4"), dir.join("abc.mkv"));
        assert_eq!(job.part_path(), dir.join("abc.mkv.part"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn worker_reports_failure_when_the_program_is_missing() {
        // 直接跑 worker：程序不存在 → 两个编码器都失败 → Failed 事件。
        let dir = temp_dir("worker");
        let (tx, rx) = channel();
        let job = spec(&dir.join("in.mp4"), dir.join("out.mkv"));
        worker(
            "/nonexistent/ffmpeg".to_string(),
            profile(),
            job,
            1,
            tx,
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
        );
        match rx.recv().unwrap() {
            WorkerMsg::Failed { message, .. } => {
                assert!(
                    message.contains("无法启动 ffmpeg") || message.contains("自动转换失败"),
                    "{message}"
                )
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
        assert!(!dir.join("out.mkv.part").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn real_ffmpeg_transcodes_a_short_clip() {
        // 端到端：用真实 ffmpeg 把 matrix 10bit 素材的前 1 秒转成 8bit HEVC，
        // 断言 .part 消失、缓存出现、进度落在 (0, 1000]。
        let Some(ffmpeg) = find_in_path("ffmpeg") else {
            eprintln!("跳过：PATH 里没有 ffmpeg");
            return;
        };
        let media = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.work/corpus/matrix/matrix-3840x2160-30fps-hevc-10bit-p010.mp4"
        ));
        if !media.exists() {
            eprintln!("跳过：没有 matrix 10bit 素材");
            return;
        }
        let dir = temp_dir("e2e");
        // NVENC 可能不在，直接指定 libx265 保证确定性（也顺带验证 fallback 分支的 preset）。
        let profile = TranscodeProfile {
            encoder: "libx265".to_string(),
            extra_args: Vec::new(),
            max_width: 1920,
            fallback_encoder: "libx265".to_string(),
            codec: "hevc",
            container: Container::Mkv,
        };
        let cache = dir.join("cache.mkv");
        let mut job = spec(media, cache.clone());
        job.duration_ms = Some(1000);
        job.limit_seconds = Some(1);
        let mut transcoder = Transcoder::new(
            TranscodePolicy::Program(ffmpeg.clone()),
            Some(ffmpeg),
            profile,
        );
        let view = transcoder.submit(job, false);
        // `submit` 会立刻 pump（并发上限 1，队列空就直接开跑）。
        assert!(matches!(
            view.state,
            PrepareState::Queued | PrepareState::Running
        ));
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut finished = None;
        while Instant::now() < deadline {
            let events = transcoder.poll();
            if let Some(event) = events.into_iter().next() {
                finished = Some(event);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        match finished {
            Some(TranscodeEvent::Finished { cache_path, .. }) => {
                assert_eq!(cache_path, cache);
                assert!(cache.exists(), "缓存未生成");
                assert!(!cache.with_extension("mkv.part").exists(), "半成品未清理");
                let view = transcoder.view(media).unwrap();
                assert_eq!(view.state, PrepareState::Ready);
                assert_eq!(view.progress_permille, 1000);
            }
            other => panic!("期望转码成功，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_file(&cache);
    }

    // ===== t26：产物格式跟着实际解码路径走 =====

    #[test]
    fn gstreamer_profile_switches_to_h264_mp4() {
        // VA-API 构型：配置原样（hevc_nvenc + mkv）。
        let vaapi = TranscodeProfile::for_consumer("hevc_nvenc -b:v 20M", 0, DecodeConsumer::Vaapi);
        assert_eq!(vaapi.codec, "hevc");
        assert_eq!(vaapi.container, Container::Mkv);
        assert_eq!(vaapi.encoder, "hevc_nvenc");
        assert_eq!(vaapi.fallback_encoder, "libx265");
        assert_eq!(vaapi.max_width, 0);

        // 纯 GStreamer 构型：强制 h264 + mp4，宽上限 4096，兜底 libx264。
        let gst =
            TranscodeProfile::for_consumer("hevc_nvenc -b:v 20M", 0, DecodeConsumer::GStreamer);
        assert_eq!(gst.codec, "h264");
        assert_eq!(gst.container, Container::Mp4);
        assert_eq!(gst.encoder, "h264_nvenc");
        assert_eq!(gst.fallback_encoder, "libx264");
        assert_eq!(gst.max_width, H264_HARDWARE_MAX_WIDTH);
        assert_eq!(gst.extra_args, vec!["-b:v".to_string(), "20M".to_string()]);

        // 配置的宽度上限更小时听配置的（但不会超过 4096）。
        let small =
            TranscodeProfile::for_consumer("libx265 -crf 20", 1920, DecodeConsumer::GStreamer);
        assert_eq!(small.max_width, 1920);
        assert_eq!(small.encoder, "libx264");
        assert_eq!(small.fallback_encoder, "libx264");
        let huge =
            TranscodeProfile::for_consumer("libx265 -crf 20", 8192, DecodeConsumer::GStreamer);
        assert_eq!(huge.max_width, H264_HARDWARE_MAX_WIDTH);

        // 已经是 h264 的编码器保持不变。
        let custom =
            TranscodeProfile::for_consumer("libx264 -crf 23", 0, DecodeConsumer::GStreamer);
        assert_eq!(custom.encoder, "libx264");
    }

    #[test]
    fn cache_key_carries_the_target_container() {
        let dir = temp_dir("t26key");
        let source = dir.join("clip.mp4");
        std::fs::write(&source, b"not-a-real-video").unwrap();
        let vaapi = TranscodeProfile::for_consumer("hevc_nvenc -b:v 20M", 0, DecodeConsumer::Vaapi);
        let gst =
            TranscodeProfile::for_consumer("hevc_nvenc -b:v 20M", 0, DecodeConsumer::GStreamer);
        let vaapi_key = cache_key(&source, &vaapi, vaapi.max_width).unwrap();
        let gst_key = cache_key(&source, &gst, gst.max_width).unwrap();
        assert_ne!(vaapi_key, gst_key, "两种构型不得互相命中对方的缓存");
        assert!(vaapi_key.ends_with(".mkv"), "{vaapi_key}");
        assert!(gst_key.ends_with(".mp4"), "{gst_key}");
        assert!(
            cache_path_for(&vaapi_key)
                .to_string_lossy()
                .ends_with(".mkv")
        );
        assert!(cache_path_for(&gst_key).to_string_lossy().ends_with(".mp4"));
        // 旧的纯 hex key 按 mkv 处理（向后兼容）。
        assert!(
            cache_path_for("deadbeef")
                .to_string_lossy()
                .ends_with("deadbeef.mkv")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mp4_targets_use_the_mp4_muxer_and_part_suffix() {
        let dir = temp_dir("t26args");
        let gst =
            TranscodeProfile::for_consumer("hevc_nvenc -b:v 20M", 0, DecodeConsumer::GStreamer);
        let spec = spec(&dir.join("in.mp4"), dir.join("out.mp4"));
        let args = ffmpeg_args(&spec, &gst, "h264_nvenc");
        let joined = args.join(" ");
        assert!(joined.contains("-f mp4"), "{joined}");
        assert!(!joined.contains("matroska"), "{joined}");
        assert!(joined.contains("out.mp4.part"), "{joined}");
        assert!(joined.contains("-pix_fmt yuv420p"), "{joined}");
        assert!(
            joined.contains("scale=min(4096\\,iw):-2:flags=lanczos"),
            "{joined}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// t37：产物 profile 按**调用方给的 consumer** 求值（同一份公式也给 classify 的 target）。
    #[test]
    fn profile_for_follows_the_output_consumer() {
        let transcoder = Transcoder::new(TranscodePolicy::Auto, None, profile());
        let vaapi = transcoder.profile_for(DecodeConsumer::Vaapi);
        assert_eq!(vaapi.codec, "hevc");
        assert_eq!(vaapi.container, Container::Mkv);
        let gstreamer = transcoder.profile_for(DecodeConsumer::GStreamer);
        assert_eq!(gstreamer.codec, "h264");
        assert_eq!(gstreamer.container, Container::Mp4);
        assert_eq!(gstreamer.encoder, "h264_nvenc");
        assert!(gstreamer.max_width <= H264_HARDWARE_MAX_WIDTH);

        // 两种构型的缓存 key 必须不同（否则会命中对方转出来的、播不了的产物）。
        let dir = temp_dir("t37-profile-for");
        let source = dir.join("clip.mp4");
        std::fs::write(&source, b"placeholder").unwrap();
        let mkv = cache_key(&source, &vaapi, vaapi.max_width).unwrap();
        let mp4 = cache_key(&source, &gstreamer, gstreamer.max_width).unwrap();
        assert_ne!(mkv, mp4);
        assert!(mkv.ends_with(".mkv"), "{mkv}");
        assert!(mp4.ends_with(".mp4"), "{mp4}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
