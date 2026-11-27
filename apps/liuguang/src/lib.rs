//! liuguang 的**库**入口：CLI 解析 + 守护进程主循环。
//!
//! 为什么要有 `lib` 目标：验收命令是 `cargo test -p liuguang --features vaapi --lib`，
//! 而 bin-only 的包没有 lib 目标（`error: no library targets found`）。把逻辑放进
//! 库里、`main.rs` 只留一行转调，既让单测跑得起来，也避免"库/二进制两份实现"。
//!
//! 模块划分：配置存储、显示电源、全屏探测、降载、媒体扫描、`show` 子命令、
//! 系统统计都只被 `daemon` 使用，因此对外只暴露 [`main_entry`]。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

// 只在 vaapi 构型里被 `daemon::open_vaapi_producer` 使用；默认构型没有调用方。
// 不能直接 `#[cfg(feature = "vaapi")]`：daemon 的单测在默认构型下也会引用这些纯函数
// （`parse` / `needs_rgb_conversion` 等），cfg 掉会让默认构型的测试编译失败。
// 因此保留模块，仅在非 vaapi 构型下放行 dead_code——这样 `scripts/ci.sh` 阶段 A
// 的字面命令 `cargo clippy --workspace --all-targets -- -D warnings` 也是绿的。
#[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
mod compat;
#[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
mod compositor_profile;
mod config_store;
mod daemon;
mod display_power;
mod fullscreen_probe;
mod game_watch;
mod gpu_pressure;
mod loadshed;
mod power_watch;
mod probe;
mod scan;
mod show;
mod sysstat;
mod thumb;
#[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
mod transcode;

use clap::{Parser, Subcommand, ValueEnum};
use liuguang_core::config::{Config, DisplayBackendKind, FitMode};
use liuguang_core::persona::{Persona, PersonaArg};
use std::path::PathBuf;
use std::process::ExitCode;

/// 退出码：成功。
const EXIT_OK: u8 = 0;
/// 退出码：运行错误。
const EXIT_ERROR: u8 = 1;

#[derive(Parser)]
#[command(
    name = "liuguang",
    version,
    about = "流光（Liuguang，工程名 Liuguang）动态壁纸守护进程：桌面 Wayland 壁纸层 / 无桌面 DRM-KMS"
)]
struct Cli {
    /// 配置文件路径（省略时按人格取默认值：系统 `/etc/liuguang/config.toml`、
    /// 用户 `$XDG_CONFIG_HOME/liuguang/config.toml`）
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// 控制 socket 路径（覆盖配置 ipc.socket_path）
    #[arg(long)]
    socket: Option<PathBuf>,
    /// 运行状态文件（覆盖配置 runtime.state_file）
    #[arg(long)]
    state_file: Option<PathBuf>,
    /// DRM 设备（覆盖配置 display.device；仅 drm 后端）
    #[arg(long)]
    device: Option<String>,
    /// 显示后端：auto（默认，按环境）| wayland | drm | mock（测试钩子）
    #[arg(long, value_enum)]
    backend: Option<BackendArg>,
    /// 人格：决定配置/状态/socket 落在系统路径还是 XDG 路径。
    /// auto（默认）按环境判定；systemd 单元一般显式写 system 或 user。
    #[arg(long, value_enum, value_name = "auto|system|user")]
    persona: Option<PersonaArg>,
    /// 子命令
    #[command(subcommand)]
    command: Option<Command>,
}

/// 显示后端选择（CLI 覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BackendArg {
    /// 按环境自动选择（有 WAYLAND_DISPLAY → wayland，否则 drm）
    Auto,
    /// Wayland 桌面壁纸层（wlroots 系合成器）
    Wayland,
    /// 真实 DRM/KMS（无桌面）
    Drm,
    /// 模拟后端（测试钩子）
    Mock,
}

impl BackendArg {
    /// 映射到配置层枚举。
    fn kind(self) -> DisplayBackendKind {
        match self {
            Self::Auto => DisplayBackendKind::Auto,
            Self::Wayland => DisplayBackendKind::Wayland,
            Self::Drm => DisplayBackendKind::Drm,
            Self::Mock => DisplayBackendKind::Mock,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// 解析并校验配置文件，输出诊断结果后退出
    CheckConfig {
        /// 要校验的配置文件（省略时使用 --config）
        path: Option<PathBuf>,
    },
    /// 在显示器上静态显示一张 PNG（M1；按 Ctrl-C 退出并释放设备）
    Show {
        /// PNG 文件路径
        image: PathBuf,
        /// 目标输出（如 HDMI-A-1；省略时自动选择已连接输出）
        #[arg(long)]
        output: Option<String>,
        /// DRM 设备：auto 或 /dev/dri/cardN
        #[arg(long, default_value = "auto")]
        device: String,
        /// 显示模式：preferred 或 宽x高[@Hz]
        #[arg(long, default_value = "preferred")]
        mode: String,
        /// 适配模式（省略时使用配置/默认 cover）
        #[arg(long, value_enum)]
        fit: Option<FitArg>,
        /// 缩放倍数（(0, 10]）
        #[arg(long, default_value_t = 1.0)]
        zoom: f64,
        /// 水平偏移（输出像素）
        #[arg(long, default_value_t = 0.0)]
        offset_x: f64,
        /// 垂直偏移（输出像素）
        #[arg(long, default_value_t = 0.0)]
        offset_y: f64,
    },
}

/// CLI 适配模式。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum FitArg {
    /// 保持比例填满，超出裁切
    Cover,
    /// 保持比例完整显示
    Contain,
    /// 忽略比例填满
    Stretch,
    /// 原始像素居中
    Center,
}

impl From<FitArg> for FitMode {
    fn from(value: FitArg) -> Self {
        match value {
            FitArg::Cover => Self::Cover,
            FitArg::Contain => Self::Contain,
            FitArg::Stretch => Self::Stretch,
            FitArg::Center => Self::Center,
        }
    }
}

/// 进程入口：解析 CLI、初始化日志、按子命令分发；退出码语义见 [`EXIT_OK`]。
///
/// `main.rs` 只调用它一次，真正的逻辑留在这里以便单测直接覆盖。
pub fn main_entry() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    // 新前缀优先：`LIUGUANG_FOO` 视同 `LIUGUANG_FOO`（仅在后者未设置时复制）。
    // 这样文档里可以写 liuguang 前缀，而内部 40+ 处读取不必逐个改。
    alias_env_prefix();
    match run(cli) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(message) => {
            tracing::error!(%message, "liuguang 启动失败");
            eprintln!("liuguang: {message}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// 把 `LIUGUANG_*` 复制成 `LIUGUANG_*`（不覆盖已有的 `LIUGUANG_*`）。
///
/// 这里需要 `set_var`，而 workspace 禁止 unsafe：允许范围最小化到本函数——
/// 它在 `run()` 之前、任何线程/定时器创建之前调用，因此不存在并发读写环境的风险。
#[allow(unsafe_code)]
fn alias_env_prefix() {
    const LEGACY: &str = "LIUGUANG_";
    const CURRENT: &str = "LIUGUANG_";
    let pairs: Vec<(String, String)> = std::env::vars()
        .filter_map(|(key, value)| {
            let suffix = key.strip_prefix(CURRENT)?;
            Some((format!("{LEGACY}{suffix}"), value))
        })
        .collect();
    for (legacy, value) in pairs {
        if std::env::var_os(&legacy).is_none() {
            // SAFETY: 单线程启动阶段，尚无其它线程读取环境（`main_entry` 在 run 之前）。
            unsafe { std::env::set_var(&legacy, value) };
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    // 基础指令保证 liuguang 自身 ≥info 可见；RUST_LOG 追加其后，
    // 用户可整体覆盖（同目标多指令取更宽级别）。
    // 注意：RUST_LOG 只含其他 crate 指令时，try_from_default_env 的
    // 默认指令不会生效，故必须显式并入基础指令。
    let base = "liuguang=info,liuguang_core=info,liuguang_display=info,liuguang_media=info,liuguang_render=info,liuguang_ipc=info,liuguang_tui=info";
    let env = std::env::var("RUST_LOG").unwrap_or_default();
    let spec = if env.trim().is_empty() {
        base.to_string()
    } else {
        format!("{base},{env}")
    };
    // RGB 转换的分段耗时统计（`LIUGUANG_RGB_STATS=1`）打在 `liuguang_vaapi` 上；
    // 该 crate 不在基础指令里（平时不需要它的 info 噪音），开关打开时补一条，
    // 否则"开了开关却看不到统计"。
    let spec = if matches!(
        std::env::var("LIUGUANG_RGB_STATS").as_deref(),
        Ok("1" | "true" | "yes" | "on")
    ) {
        format!("{spec},liuguang_vaapi=info")
    } else {
        spec
    };
    // stderr 行缓冲：stdout 在管道下块缓冲会滞后日志（真机排障教训）。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::new(spec))
        .init();
}

fn run(cli: Cli) -> Result<(), String> {
    // 人格先于配置：它决定配置**在哪**（自指困境的解法），见
    // `liuguang_core::persona` 模块文档。
    let cli_backend = cli.backend.map(BackendArg::kind);
    // 显式 `--persona` 优先于环境判定；`auto` 才回落到 `WAYLAND_DISPLAY`。
    let persona = cli
        .persona
        .map(|arg| arg.resolve(cli_backend))
        .unwrap_or_else(|| Persona::detect(cli_backend));
    let config_path = cli.config.clone().unwrap_or_else(|| persona.config_path());
    tracing::debug!(
        persona = %persona,
        backend = ?cli_backend,
        config = %config_path.display(),
        "启动人格判定完成"
    );
    match cli.command {
        Some(Command::CheckConfig { path }) => {
            let path = path.unwrap_or(config_path);
            let config = Config::load(&path)
                .map_err(|e| format!("配置校验失败（{}）: {e}", path.display()))?;
            println!(
                "配置有效: {}（输出段 {} 个，轮换间隔 {} 秒）",
                path.display(),
                config.outputs.len(),
                config.slideshow.interval_seconds
            );
            Ok(())
        }
        Some(Command::Show {
            image,
            output,
            device,
            mode,
            fit,
            zoom,
            offset_x,
            offset_y,
        }) => {
            show::check_image_path(&image)?;
            let config = if config_path.exists() {
                let mut cfg = Config::load(&config_path)
                    .map_err(|e| format!("配置无效（{}）: {e}", config_path.display()))?;
                cfg.apply_persona_defaults(persona);
                Some(cfg)
            } else {
                tracing::warn!(
                    path = %config_path.display(),
                    "配置文件不存在，使用 CLI 参数与内置默认值"
                );
                None
            };
            show::run(
                show::ShowOptions {
                    image,
                    output,
                    device,
                    mode,
                    fit: fit.map(FitMode::from),
                    zoom,
                    offset_x,
                    offset_y,
                },
                config.as_ref(),
            )
        }
        None => {
            // 常驻守护模式（M2）：IPC 服务 + 显示运行时 + 状态持久化。
            // 配置解析先于 daemon，便于 CLI 覆盖注入。
            let mut config = if config_path.exists() {
                Config::load(&config_path)
                    .map_err(|e| format!("配置无效（{}）: {e}", config_path.display()))?
            } else if persona == Persona::User {
                bootstrap_user_config(&config_path)
            } else {
                tracing::warn!(
                    path = %config_path.display(),
                    "配置文件不存在，使用内置默认值"
                );
                Config::default()
            };
            config.apply_persona_defaults(persona);
            daemon::run(daemon::DaemonOptions {
                socket_path: cli
                    .socket
                    .clone()
                    .unwrap_or_else(|| config.ipc.socket_path.clone()),
                state_file: cli
                    .state_file
                    .clone()
                    .unwrap_or_else(|| config.runtime.state_file.clone()),
                config_path: config_path.clone(),
                device: cli.device.clone(),
                backend: cli_backend.unwrap_or(DisplayBackendKind::Auto),
                persona,
            })
        }
    }
}

/// 解析桌面模式默认媒体目录（优先 `$XDG_PICTURES_DIR/Wallpapers`，回退 `$HOME/Pictures/Wallpapers`）。
fn default_user_wallpapers_dir() -> Option<PathBuf> {
    if let Some(pics) = std::env::var_os("XDG_PICTURES_DIR") {
        let p = PathBuf::from(&pics);
        if p.is_absolute() {
            return Some(p.join("Wallpapers"));
        }
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join("Pictures").join("Wallpapers"))
}

/// 解析桌面模式候选现有目录（用于 `ProtectHome=read-only` 下无法新建目录时的回退）。
fn fallback_existing_user_dir() -> Option<PathBuf> {
    if let Some(pics) = std::env::var_os("XDG_PICTURES_DIR") {
        let p = PathBuf::from(&pics);
        if p.is_absolute() && p.is_dir() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)?;
    for rel in ["Pictures", "图片", "Videos", "视频"] {
        let candidate = home.join(rel);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    home.is_dir().then_some(home)
}

/// 桌面首次运行引导：配置文件尚不存在时自动建立默认壁纸库与配置目录，免去手工建文件和写 TOML。
fn bootstrap_user_config(config_path: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.display.backend = DisplayBackendKind::Auto;
    cfg.apply_persona_defaults(Persona::User);

    let cache_dir = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        });
    if let Some(cache) = cache_dir {
        let _ = std::fs::create_dir_all(cache.join("liuguang"));
    }

    if let Some(media_dir) = default_user_wallpapers_dir() {
        let _ = std::fs::create_dir_all(&media_dir);
        if media_dir.is_dir() {
            cfg.slideshow.directory = media_dir.clone();
            cfg.library = Some(liuguang_core::library::LibraryConfig {
                root: media_dir.clone(),
                image_dirs: vec![media_dir.clone()],
                video_dirs: vec![media_dir],
                recursive: true,
                ..Default::default()
            });
        } else if let Some(existing) = fallback_existing_user_dir() {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let is_home_root = home.as_ref().is_some_and(|h| h == &existing);
            cfg.slideshow.directory = existing.clone();
            cfg.library = Some(liuguang_core::library::LibraryConfig {
                root: existing.clone(),
                image_dirs: if is_home_root {
                    Vec::new()
                } else {
                    vec![existing.clone()]
                },
                video_dirs: if is_home_root {
                    Vec::new()
                } else {
                    vec![existing]
                },
                recursive: !is_home_root,
                ..Default::default()
            });
        }
    }

    match cfg.save_atomic(config_path) {
        Ok(bytes) => tracing::info!(
            path = %config_path.display(),
            bytes,
            "首次启动桌面模式：已自动生成默认配置文件"
        ),
        Err(e) => tracing::warn!(
            path = %config_path.display(),
            error = %e,
            "首次启动生成默认配置文件失败，暂以内存默认配置运行"
        ),
    }
    cfg
}
