//! liuguangctl — 流光（Liuguang）控制端 CLI / TUI。
//!
//! 命令面按规范 §9.2 定型；通过 Unix socket 与 liuguang 通信（IPC v1，M2）。
//! TUI 属于 M3。退出码约定：
//! - `0` 成功
//! - `1` 请求被服务端拒绝 / 一般错误
//! - `2` 命令行用法错误（clap；**含参数越界**，如 `--duration 99999`、`--zoom 0`）
//! - `3` liuguang 不可达
//!
//! 参数范围一律在 clap 层（`value_parser`）校验，因此越界统一是"用法错误 = 2"，
//! 而不是"服务端拒绝 = 1"（审计 P2-13）。`--zoom nan` / `--zoom -1` 也在此层被拒，
//! 不再落成"静默 accepted"。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use clap::{Parser, Subcommand, ValueEnum};
use liuguang_core::config::{MAX_TRANSITION_DURATION_MS, MAX_ZOOM};
use liuguang_core::persona::{Persona, PersonaArg};
use liuguang_ipc::{
    AcceptedResult, ClientError, ErrorKind, IpcClient, Method, OutputParams, OutputsResult,
    SetWallpaperParams, StatusResult, TransitionParams,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 退出码：成功。
const EXIT_OK: u8 = 0;
/// 退出码：一般错误 / 服务端拒绝。
const EXIT_ERROR: u8 = 1;
/// 退出码：liuguang 不可达。
const EXIT_UNAVAILABLE: u8 = 3;

/// 解析 `--zoom`：必须是有限值且落在 `(0, MAX_ZOOM]`。
///
/// clap 的 `value_parser` 通道 ⇒ 越界/NaN 一律退出码 2，且报错信息指向参数本身。
fn parse_zoom(text: &str) -> Result<f64, String> {
    let value: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("`{text}` 不是合法数字"))?;
    if !value.is_finite() {
        return Err(format!(
            "`{text}` 不是有限数值（拒绝 nan/inf；缩放必须可计算）"
        ));
    }
    if !(value > 0.0 && value <= MAX_ZOOM) {
        return Err(format!("应在 (0, {MAX_ZOOM}] 区间内，实际: {value}"));
    }
    Ok(value)
}

/// 解析 `--offset_x` / `--offset_y`：必须是有限值（单位：输出像素）。
fn parse_offset(text: &str) -> Result<f64, String> {
    let value: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("`{text}` 不是合法数字"))?;
    if !value.is_finite() {
        return Err(format!("`{text}` 不是有限数值（偏移必须可计算）"));
    }
    Ok(value)
}

/// `--duration`：0..=MAX_TRANSITION_DURATION_MS（越界 = 用法错误）。
fn duration_parser() -> impl clap::builder::TypedValueParser<Value = u32> {
    clap::value_parser!(u32).range(0..=i64::from(MAX_TRANSITION_DURATION_MS))
}

#[derive(Debug, Parser)]
#[command(
    name = "liuguangctl",
    version,
    about = "流光（Liuguang）控制端：通过 Unix socket 管理壁纸，TUI 可经 SSH 运行"
)]
struct Cli {
    /// 守护进程控制 socket 路径（省略时按人格推断：
    /// 系统服务 `/run/liuguang/control.sock`，用户会话 `$XDG_RUNTIME_DIR/liuguang/control.sock`）
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// TUI 壁纸浏览目录（省略时按人格推断：`/srv/wallpapers` 或 `$HOME/Pictures`）
    #[arg(long, global = true)]
    media_dir: Option<PathBuf>,
    /// 强制指定人格（auto 交给环境判定：有 WAYLAND_DISPLAY 即用户会话）
    #[arg(long, global = true, value_name = "auto|system|user")]
    persona: Option<PersonaArg>,
    /// 子命令
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// 查询守护进程状态
    Status,
    /// 枚举显示输出
    Outputs,
    /// 设置壁纸（PNG / MP4）
    Set {
        /// 媒体文件路径
        path: PathBuf,
        /// 目标输出（省略时使用默认输出）
        #[arg(long)]
        output: Option<String>,
        /// 适配模式
        #[arg(long, value_enum)]
        fit: Option<FitArg>,
        /// 缩放倍数（(0, 10]；NaN/inf 与越界都是用法错误 = 退出码 2）
        #[arg(long, value_parser = parse_zoom, allow_hyphen_values = true)]
        zoom: Option<f64>,
        /// 水平偏移（输出像素；必须有限）；同时接受 `--offset_x` 写法
        #[arg(long, alias = "offset_x", value_parser = parse_offset, allow_hyphen_values = true)]
        offset_x: Option<f64>,
        /// 垂直偏移（输出像素；必须有限）；同时接受 `--offset_y` 写法
        #[arg(long, alias = "offset_y", value_parser = parse_offset, allow_hyphen_values = true)]
        offset_y: Option<f64>,
        /// **只放这一项**：视频单曲循环 / 图片常驻，不跟随轮换
        #[arg(long, conflicts_with = "no_pin")]
        pin: bool,
        /// 恢复按 `interval_seconds` 轮换（与 `--pin` 互斥）
        #[arg(long)]
        no_pin: bool,
    },
    /// 切换到下一个媒体项
    Next {
        /// 目标输出
        #[arg(long)]
        output: Option<String>,
    },
    /// 只放当前这一项（视频单曲循环 / 图片常驻），不再跟随轮换
    Pin {
        /// `on` = 锁定当前项；`off` = 恢复轮换
        #[arg(value_enum)]
        state: PinArg,
        /// 目标输出（省略时使用默认输出）
        #[arg(long)]
        output: Option<String>,
    },
    /// 暂停播放
    Pause {
        /// 目标输出
        #[arg(long)]
        output: Option<String>,
    },
    /// 恢复播放
    Resume {
        /// 目标输出
        #[arg(long)]
        output: Option<String>,
    },
    /// 设置切换转场效果
    Transition {
        /// 效果
        effect: EffectArg,
        /// 时长（毫秒，0..=10000；越界 = 用法错误 = 退出码 2）
        #[arg(long, default_value_t = 350, value_parser = duration_parser())]
        duration: u32,
        /// 缓动
        #[arg(long, value_enum)]
        easing: Option<EasingArg>,
        /// 方向（适用效果使用）
        #[arg(long, value_enum)]
        direction: Option<DirectionArg>,
    },
    /// 重载配置
    Reload,
    /// 打开远程 TUI（经 `ssh -t` 使用；M3 交付）
    Tui,
}

/// 适配模式。
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

impl FitArg {
    const fn name(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
            Self::Stretch => "stretch",
            Self::Center => "center",
        }
    }
}

/// `pin` 子命令的取值。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum PinArg {
    /// 只放当前这一项
    On,
    /// 恢复按间隔轮换
    Off,
}

impl PinArg {
    const fn pinned(self) -> bool {
        matches!(self, Self::On)
    }
}

/// 切换效果（§7.1）。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum EffectArg {
    /// 淡入淡出
    Fade,
    /// 滑动
    Slide,
    /// 擦除
    Wipe,
    /// 圆形揭示
    Circle,
    /// 随机
    Random,
    /// 无转场
    None,
}

impl EffectArg {
    const fn name(self) -> &'static str {
        match self {
            Self::Fade => "fade",
            Self::Slide => "slide",
            Self::Wipe => "wipe",
            Self::Circle => "circle",
            Self::Random => "random",
            Self::None => "none",
        }
    }
}

/// 缓动函数。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum EasingArg {
    /// 线性
    Linear,
    /// 先慢后快
    EaseIn,
    /// 先快后慢
    EaseOut,
    /// 两头慢
    EaseInOut,
}

impl EasingArg {
    const fn name(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::EaseIn => "ease-in",
            Self::EaseOut => "ease-out",
            Self::EaseInOut => "ease-in-out",
        }
    }
}

/// 转场方向。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum DirectionArg {
    /// 向左
    Left,
    /// 向右
    Right,
    /// 向上
    Up,
    /// 向下
    Down,
}

impl DirectionArg {
    const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(AppError::Unavailable(msg)) => {
            eprintln!("liuguangctl: {msg}");
            ExitCode::from(EXIT_UNAVAILABLE)
        }
        Err(AppError::Failed(msg)) => {
            eprintln!("liuguangctl: {msg}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// 应用级错误（决定退出码）。
enum AppError {
    /// liuguang 不可达 / 能力未实现（退出码 3）。
    Unavailable(String),
    /// 一般错误 / 服务端拒绝（退出码 1）。
    Failed(String),
}

fn connect(socket: &Path) -> Result<IpcClient, AppError> {
    IpcClient::connect(socket).map_err(|e| match e {
        ClientError::Connect { socket, message } => AppError::Unavailable(format!(
            "无法连接 liuguang（{socket}）: {message}；系统服务请确认 liuguang.service 在运行、当前用户已加入 liuguang-ctl 组（sudo usermod -aG liuguang-ctl $USER 后重新登录）；桌面会话请确认用户单元 liuguang.service 在运行（systemctl --user status liuguang）"
        )),
        other => AppError::Failed(other.to_string()),
    })
}

/// 发送请求并把业务失败映射为退出码 1。
fn send(
    client: &mut IpcClient,
    method: Method,
    params: serde_json::Value,
) -> Result<serde_json::Value, AppError> {
    let resp = client.request(method, params).map_err(|e| match e {
        ClientError::Transport(msg) => AppError::Unavailable(msg),
        other => AppError::Failed(other.to_string()),
    })?;
    if resp.ok {
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    } else {
        let err = resp.error.unwrap_or(liuguang_ipc::ErrorBody::new(
            ErrorKind::Internal,
            "未知错误",
        ));
        Err(AppError::Failed(format!("{:?}: {}", err.code, err.message)))
    }
}

/// 把 `--persona` / `--socket` / `--media-dir` 解析成实际目标。
///
/// 与 daemon 共用同一套人格判定（`liuguang_core::persona`）：桌面会话里
/// `liuguangctl status` 不需要 `--socket`，也不需要 `liuguang-ctl` 组。
fn resolve(cli: &Cli) -> (PathBuf, PathBuf) {
    let persona = cli
        .persona
        .map(|arg| arg.resolve(None))
        .unwrap_or_else(|| Persona::detect(None));
    let socket = cli.socket.clone().unwrap_or_else(|| persona.socket_path());
    let media_dir = cli.media_dir.clone().unwrap_or_else(|| persona.media_dir());
    (socket, media_dir)
}

fn run(cli: Cli) -> Result<(), AppError> {
    let (socket, media_dir) = resolve(&cli);
    match cli.command {
        CliCommand::Status => {
            let mut client = connect(&socket)?;
            let v = send(&mut client, Method::Status, serde_json::json!({}))?;
            let status: StatusResult =
                serde_json::from_value(v).map_err(|e| AppError::Failed(e.to_string()))?;
            println!("state:      {}", status.state);
            println!("output:     {}", status.output.as_deref().unwrap_or("-"));
            println!("media:      {}", status.media.as_deref().unwrap_or("-"));
            println!("mode:       {}", status.mode.as_deref().unwrap_or("-"));
            println!("paused:     {}", status.paused);
            // 暂停原因对排障很重要：「熄屏暂停」和「用户手动暂停」的处置完全相反，
            // 没有这一行时只能去翻 journal（前台用户看不到）。
            let reasons: Vec<String> = status
                .channels
                .iter()
                .filter_map(|ch| {
                    ch.pause_reason
                        .as_deref()
                        .map(|r| format!("{}={r}", ch.output))
                })
                .collect();
            if !reasons.is_empty() {
                println!("paused_by:  {}", reasons.join(", "));
            }
            println!("backend:    {}", status.display_backend);
            println!("decoder:    {}", status.decoder);
            for ch in &status.channels {
                println!(
                    "channel[{}]: state={} media={} decoder={} paused={} pinned={}",
                    ch.output,
                    ch.state,
                    ch.media.as_deref().unwrap_or("-"),
                    ch.decoder,
                    ch.paused,
                    ch.pinned
                );
                if let Some(v) = &ch.video {
                    println!(
                        "  video: presented={} dropped_late={} dropped_queue={} queue_len={} last_pts_ms={:?}",
                        v.presented,
                        v.dropped_late,
                        v.dropped_queue_full,
                        v.queue_len,
                        v.last_pts_ms
                    );
                    // 对账：presented 是**出队**计数，不等于上屏；与 present_ok/
                    // present_failures 一起看才有意义。两者同时增长 = 异常。
                    println!(
                        "  present: ok={} failures={} consecutive_failures={}（presented 为出队数，≠上屏；failures>0 即异常）",
                        v.present_ok, v.present_failures, v.present_failures_consecutive
                    );
                    // 零拷贝是否**真的**生效：`is_dmabuf=false` 而 decoder 里写着
                    // dmabuf ⇒ 已回退系统内存（CPU 拷贝，占用会显著上升）。
                    println!(
                        "  dmabuf:  is_dmabuf={} fallbacks={} keepalive_len={}（false = 已回退系统内存）",
                        v.is_dmabuf, v.dmabuf_fallbacks, v.dmabuf_keepalive_len
                    );
                }
            }
            if let Some(c) = &status.commit {
                println!(
                    "commit:     ok={} retried={} legacy_fallback={} failed={} success_total={} attempts={}（提交成功≠上屏）",
                    c.commit_ok,
                    c.retried_after_busy,
                    c.legacy_fallback,
                    c.failed,
                    c.commit_success_total,
                    c.attempts
                );
            }
            match &status.master {
                Some(m) => {
                    println!(
                        "master:     {}（occupied_by={:?}）",
                        match m.master {
                            Some(true) => "yes",
                            Some(false) => "NO —— 画面不会更新",
                            None => "unknown",
                        },
                        m.holders
                    );
                    if !m.acquire_result.is_empty() {
                        println!("  acquire:  {}", m.acquire_result);
                    }
                    if m.master == Some(false) {
                        println!("  remedy:   {}", m.remedy_hint);
                    }
                }
                None => println!("master:     -（该后端不适用）"),
            }
            println!("uptime:     {}s", status.uptime_seconds);
            Ok(())
        }
        CliCommand::Outputs => {
            let mut client = connect(&socket)?;
            let v = send(&mut client, Method::Outputs, serde_json::json!({}))?;
            let outputs: OutputsResult =
                serde_json::from_value(v).map_err(|e| AppError::Failed(e.to_string()))?;
            for o in &outputs.outputs {
                println!(
                    "{}\t{}\t{}",
                    o.name,
                    if o.connected {
                        "connected"
                    } else {
                        "disconnected"
                    },
                    o.mode.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
        CliCommand::Set {
            path,
            output,
            fit,
            zoom,
            offset_x,
            offset_y,
            pin,
            no_pin,
        } => {
            let mut client = connect(&socket)?;
            let params = SetWallpaperParams {
                path: path.display().to_string(),
                output,
                fit: fit.map(|f| f.name().to_string()),
                zoom,
                offset_x,
                offset_y,
                rotation: None,
                brightness: None,
                pin: if pin {
                    Some(true)
                } else if no_pin {
                    Some(false)
                } else {
                    None
                },
            };
            let v = send(
                &mut client,
                Method::SetWallpaper,
                serde_json::to_value(params).map_err(|e| AppError::Failed(e.to_string()))?,
            )?;
            let accepted: AcceptedResult =
                serde_json::from_value(v).map_err(|e| AppError::Failed(e.to_string()))?;
            println!("accepted: {}", accepted.accepted);
            Ok(())
        }
        CliCommand::Next { output } => {
            let mut client = connect(&socket)?;
            send(
                &mut client,
                Method::Next,
                serde_json::to_value(OutputParams { output })
                    .map_err(|e| AppError::Failed(e.to_string()))?,
            )?;
            Ok(())
        }
        CliCommand::Pin { state, output } => {
            let mut client = connect(&socket)?;
            let v = send(
                &mut client,
                Method::SetPinned,
                serde_json::to_value(liuguang_ipc::protocol::SetPinnedParams {
                    pinned: state.pinned(),
                    output,
                })
                .map_err(|e| AppError::Failed(e.to_string()))?,
            )?;
            let result: liuguang_ipc::protocol::SetPinnedResult =
                serde_json::from_value(v).map_err(|e| AppError::Failed(e.to_string()))?;
            println!("pinned: {}", result.pinned);
            Ok(())
        }
        CliCommand::Pause { output } => {
            let mut client = connect(&socket)?;
            send(
                &mut client,
                Method::Pause,
                serde_json::to_value(OutputParams { output })
                    .map_err(|e| AppError::Failed(e.to_string()))?,
            )?;
            println!("paused");
            Ok(())
        }
        CliCommand::Resume { output } => {
            let mut client = connect(&socket)?;
            send(
                &mut client,
                Method::Resume,
                serde_json::to_value(OutputParams { output })
                    .map_err(|e| AppError::Failed(e.to_string()))?,
            )?;
            println!("resumed");
            Ok(())
        }
        CliCommand::Reload => {
            let mut client = connect(&socket)?;
            send(&mut client, Method::Reload, serde_json::json!({}))?;
            println!("config reloaded");
            Ok(())
        }
        CliCommand::Transition {
            effect,
            duration,
            easing,
            direction,
        } => {
            // 范围已由 clap 的 `duration_parser()` 保证（越界=退出码 2），此处无需再判。
            let mut client = connect(&socket)?;
            let params = TransitionParams {
                effect: effect.name().to_string(),
                duration_ms: duration,
                easing: easing.map(|e| e.name().to_string()),
                direction: direction.map(|d| d.name().to_string()),
            };
            send(
                &mut client,
                Method::Transition,
                serde_json::to_value(params).map_err(|e| AppError::Failed(e.to_string()))?,
            )?;
            println!("transition: {} {}ms", effect.name(), duration);
            Ok(())
        }
        CliCommand::Tui => liuguang_tui::run(liuguang_tui::TuiOptions {
            socket_path: socket,
            media_dir,
        })
        .map_err(|e| AppError::Failed(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// 桌面会话里不带 `--socket` 也要能定位到用户人格的 socket；
    /// 显式 `--socket` 永远优先（系统管理员在 SSH 里指定绝对路径的场景）。
    #[test]
    fn resolve_prefers_explicit_targets() {
        let cli = Cli::try_parse_from([
            "liuguangctl",
            "--socket",
            "/tmp/x.sock",
            "--media-dir",
            "/tmp/m",
            "status",
        ])
        .unwrap();
        let (socket, media) = resolve(&cli);
        assert_eq!(socket, PathBuf::from("/tmp/x.sock"));
        assert_eq!(media, PathBuf::from("/tmp/m"));
    }

    /// 人格可用 `--persona` 强制，且不影响子命令解析。
    #[test]
    fn persona_flag_overrides_environment() {
        let cli = Cli::try_parse_from(["liuguangctl", "--persona", "system", "status"]).unwrap();
        assert_eq!(cli.persona, Some(PersonaArg::System));
        assert_eq!(PersonaArg::System.resolve(None), Persona::System);
    }

    #[test]
    fn fit_names_are_stable() {
        assert_eq!(FitArg::Cover.name(), "cover");
        assert_eq!(FitArg::Stretch.name(), "stretch");
    }

    #[test]
    fn effect_names_match_spec() {
        assert_eq!(EffectArg::Fade.name(), "fade");
        assert_eq!(EffectArg::Circle.name(), "circle");
        assert_eq!(EffectArg::None.name(), "none");
    }

    /// 越界的 `--duration` 必须是**用法错误（退出码 2）**，而不是服务端拒绝（1）。
    /// 回归：审计 P2-13。
    #[test]
    fn out_of_range_duration_is_a_usage_error() {
        let err = Cli::try_parse_from(["liuguangctl", "transition", "fade", "--duration", "99999"])
            .expect_err("越界时长必须被 clap 拒绝");
        assert_eq!(err.exit_code(), 2, "{err}");
        // 边界内可用。
        Cli::try_parse_from(["liuguangctl", "transition", "fade", "--duration", "10000"])
            .expect("上界应被接受");
    }

    /// `--zoom nan` 过去会静默 accepted；现在必须退出码 2。回归：审计 P2-13。
    #[test]
    fn non_finite_zoom_is_a_usage_error() {
        for bad in ["nan", "inf", "-inf", "NaN"] {
            let err = match Cli::try_parse_from(["liuguangctl", "set", "a.png", "--zoom", bad]) {
                Err(e) => e,
                Ok(_) => panic!("--zoom {bad} 必须被拒"),
            };
            assert_eq!(err.exit_code(), 2, "--zoom {bad}: {err}");
        }
    }

    /// `--zoom` 越界（含负数）同样是用法错误；负数不再是 "unexpected argument"。
    #[test]
    fn zoom_range_and_negative_values_are_usage_errors() {
        for bad in ["0", "-1", "-0.5", "10.0001", "99"] {
            let err = match Cli::try_parse_from(["liuguangctl", "set", "a.png", "--zoom", bad]) {
                Err(e) => e,
                Ok(_) => panic!("--zoom {bad} 必须被拒"),
            };
            assert_eq!(err.exit_code(), 2, "--zoom {bad}: {err}");
            assert!(
                err.to_string().contains("zoom") || err.to_string().contains("区间"),
                "--zoom {bad} 的报错应指向参数本身: {err}"
            );
        }
    }

    #[test]
    fn zoom_accepts_boundary_values() {
        let cli = Cli::try_parse_from(["liuguangctl", "set", "a.png", "--zoom", "10"]).unwrap();
        match cli.command {
            CliCommand::Set { zoom, .. } => assert_eq!(zoom, Some(10.0)),
            other => panic!("期望 set 子命令: {other:?}"),
        }
        let cli = Cli::try_parse_from(["liuguangctl", "set", "a.png", "--zoom", "0.0001"]).unwrap();
        match cli.command {
            CliCommand::Set { zoom, .. } => assert_eq!(zoom, Some(0.0001)),
            other => panic!("期望 set 子命令: {other:?}"),
        }
    }

    /// 偏移允许负数（对称的左右/上下位移），但必须是有限值。
    #[test]
    fn offsets_accept_negative_but_reject_non_finite() {
        let cli =
            Cli::try_parse_from(["liuguangctl", "set", "a.png", "--offset_x", "-120.5"]).unwrap();
        match cli.command {
            CliCommand::Set { offset_x, .. } => assert_eq!(offset_x, Some(-120.5)),
            other => panic!("期望 set 子命令: {other:?}"),
        }
        for bad in ["inf", "nan"] {
            let err = match Cli::try_parse_from(["liuguangctl", "set", "a.png", "--offset_y", bad])
            {
                Err(e) => e,
                Ok(_) => panic!("--offset_y {bad} 必须被拒"),
            };
            assert_eq!(err.exit_code(), 2, "--offset_y {bad}: {err}");
        }
    }

    /// 非法枚举（效果名）也是用法错误。
    #[test]
    fn invalid_effect_is_a_usage_error() {
        let err = match Cli::try_parse_from(["liuguangctl", "transition", "bogus"]) {
            Err(e) => e,
            Ok(_) => panic!("非法效果名必须被拒"),
        };
        assert_eq!(err.exit_code(), 2, "{err}");
    }
}
