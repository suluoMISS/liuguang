//! 运行时**人格**：系统服务（无桌面）与用户会话（桌面壁纸）。
//!
//! Liuguang 有两种部署形态，配置、状态、控制 socket 的位置完全不同：
//!
//! | 人格 | 配置 | 状态 | socket | 依赖 |
//! |------|------|------|--------|------|
//! | [`System`](Persona::System) | `/etc/liuguang/config.toml` | `/var/lib/liuguang/state.json` | `/run/liuguang/control.sock` | root + DRM master |
//! | [`User`](Persona::User) | `$XDG_CONFIG_HOME/liuguang/config.toml` | `$XDG_STATE_HOME/liuguang/state.json` | `$XDG_RUNTIME_DIR/liuguang/control.sock` | 会话内的合成器 |
//!
//! 人格**只由环境与 CLI 决定**，不由配置文件决定 —— 否则会出现
//! "配置在 A 处、程序去 B 处找"的自指困境。判定见 [`Persona::detect`]。
//!
//! # 安全语义
//!
//! `auto` 后端在**没有** Wayland 会话时选 DRM。反过来，**有** Wayland
//! 会话时绝不会回退到 DRM：在运行中的合成器下面抢 DRM master 会把桌面
//! 画面撕掉。因此 `wayland` 连接失败时进入 HeadlessReady 并告警，而不是
//! "退回直出"。

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::DisplayBackendKind;

/// 进程的人格（部署形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persona {
    /// 系统服务：无桌面会话，直接接管 DRM/KMS。
    System,
    /// 用户服务：桌面会话内，向合成器推壁纸层。
    User,
}

/// `--persona` 的命令行取值。
///
/// 只暴露三个"决定行为"的取值；`auto` 保留环境判定（有 `WAYLAND_DISPLAY`
/// 就是 user 人格），这样脚本可以不写死人格。
///
/// 放在 `liuguang-core`（而非各个二进制）是为了让 **daemon 与 CLI 共用同一份
/// 取值集合**——两边枚举漂移过一次就会变成"CLI 能启动、daemon 不认"的怪 bug。
#[cfg(feature = "cli")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PersonaArg {
    /// 按环境判定（默认）
    Auto,
    /// 系统服务：/etc、/var/lib、/run
    System,
    /// 桌面会话：$XDG_CONFIG_HOME、$XDG_STATE_HOME、$XDG_RUNTIME_DIR
    User,
}

#[cfg(feature = "cli")]
impl PersonaArg {
    /// 解析成实际人格。`cli_backend` 只在 `auto` 时参与判定
    /// （`--backend drm` 明确说明要系统形态）。
    pub fn resolve(self, cli_backend: Option<DisplayBackendKind>) -> Persona {
        match self {
            Self::Auto => Persona::detect(cli_backend),
            Self::System => Persona::System,
            Self::User => Persona::User,
        }
    }
}

impl Persona {
    /// 从 CLI 后端覆盖与环境推断人格。
    ///
    /// - `cli_backend` 显式给了 `wayland`/`drm` → 直接对应人格；
    /// - 否则（未给或 `auto`）→ 有 `WAYLAND_DISPLAY` 即 [`User`](Self::User)。
    ///
    /// 注意：这里**只看环境变量**，不尝试连接合成器。连接失败的处理见
    /// 模块文档（不会回退到 DRM）。
    #[must_use]
    pub fn detect(cli_backend: Option<DisplayBackendKind>) -> Self {
        Self::detect_with(cli_backend, &|key| std::env::var_os(key))
    }

    /// [`detect`](Self::detect) 的可注入版本（测试用；避免改全局环境）。
    #[must_use]
    pub fn detect_with(
        cli_backend: Option<DisplayBackendKind>,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Self {
        match cli_backend {
            Some(DisplayBackendKind::Wayland) => Self::User,
            Some(DisplayBackendKind::Drm) => Self::System,
            // `Auto` 或未指定：环境说了算。
            _ => {
                if env("WAYLAND_DISPLAY").is_some() {
                    Self::User
                } else {
                    Self::System
                }
            }
        }
    }

    /// 人格名称（日志/`status` 用）。
    /// 解析显式人格写法（CLI `--persona` 用）：`system` / `user`（大小写不敏感），
    /// `auto` 返回 `None`（表示"交给环境判定"，调用方用 [`detect`](Self::detect)）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "system" | "sys" | "root" => Some(Self::System),
            "user" | "session" | "desktop" => Some(Self::User),
            _ => None,
        }
    }

    /// TUI / CLI 默认浏览的媒体目录。
    #[must_use]
    pub fn media_dir(self) -> PathBuf {
        self.media_dir_with(&|key| std::env::var_os(key))
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }

    /// 配置文件的默认路径。
    #[must_use]
    pub fn config_path(self) -> PathBuf {
        self.config_path_with(&|key| std::env::var_os(key))
    }

    /// 状态文件的默认路径。
    #[must_use]
    pub fn state_path(self) -> PathBuf {
        self.state_path_with(&|key| std::env::var_os(key))
    }

    /// 控制 socket 的默认路径。
    #[must_use]
    pub fn socket_path(self) -> PathBuf {
        self.socket_path_with(&|key| std::env::var_os(key))
    }

    /// 缩略图缓存目录（GUI 用）。
    #[must_use]
    pub fn cache_dir(self) -> PathBuf {
        self.cache_dir_with(&|key| std::env::var_os(key))
    }

    // -- 可注入实现 ---------------------------------------------------------

    #[must_use]
    pub fn config_path_with(self, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
        match self {
            Self::System => PathBuf::from("/etc/liuguang/config.toml"),
            Self::User => xdg_dir(env, "XDG_CONFIG_HOME", &[".config"])
                .unwrap_or_else(fallback_config)
                .join("liuguang/config.toml"),
        }
    }

    #[must_use]
    pub fn state_path_with(self, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
        match self {
            Self::System => PathBuf::from("/var/lib/liuguang/state.json"),
            Self::User => xdg_dir(env, "XDG_STATE_HOME", &[".local", "state"])
                .unwrap_or_else(fallback_state)
                .join("liuguang/state.json"),
        }
    }

    #[must_use]
    pub fn socket_path_with(self, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
        match self {
            Self::System => PathBuf::from("/run/liuguang/control.sock"),
            // 会话 socket 必须在 XDG_RUNTIME_DIR（tmpfs、按用户隔离、注销即清）。
            // 环境缺失时退到状态目录：仍属当前用户，功能可用但不会自动清理。
            Self::User => env("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| xdg_dir(env, "XDG_STATE_HOME", &[".local", "state"]))
                .unwrap_or_else(fallback_state)
                .join("liuguang/control.sock"),
        }
    }

    /// 媒体目录：系统服务沿用历史约定 `/srv/wallpapers`；用户会话用
    /// `$HOME/Pictures`（XDG 图片目录由 user-dirs.dirs 定义，不在环境变量里，
    /// 因此这里只做最常见的推断，用户可用 `--media-dir` 覆盖）。
    #[must_use]
    pub fn media_dir_with(self, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
        match self {
            Self::System => PathBuf::from("/srv/wallpapers"),
            Self::User => env("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map_or_else(|| PathBuf::from("Pictures"), |home| home.join("Pictures")),
        }
    }

    #[must_use]
    pub fn cache_dir_with(self, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
        match self {
            // 系统服务没有 GUI，缓存目录只用于 PID 缓存等；放状态目录旁。
            Self::System => PathBuf::from("/var/cache/liuguang"),
            Self::User => xdg_dir(env, "XDG_CACHE_HOME", &[".cache"])
                .unwrap_or_else(fallback_cache)
                .join("liuguang"),
        }
    }
}

impl std::fmt::Display for Persona {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 取一个 XDG 目录：环境变量优先（必须是绝对路径），否则
/// `$HOME/<fallback 各段>`。
fn xdg_dir(
    env: &dyn Fn(&str) -> Option<OsString>,
    var: &str,
    home_segments: &[&str],
) -> Option<PathBuf> {
    if let Some(value) = env(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Some(value);
    }
    let home = env("HOME").map(PathBuf::from).filter(|p| p.is_absolute())?;
    let mut out = home;
    for segment in home_segments {
        out.push(segment);
    }
    Some(out)
}

/// 连 `$HOME` 都没有时的兜底（极端环境：只有 XDG 变量）。
fn fallback_config() -> PathBuf {
    PathBuf::from(".config")
}

fn fallback_state() -> PathBuf {
    PathBuf::from(".local/state")
}

fn fallback_cache() -> PathBuf {
    PathBuf::from(".cache")
}

/// 从 `user-dirs.dirs` 内容中提取指定键（如 `XDG_PICTURES_DIR="$HOME/图片"`）对应的路径。
#[must_use]
pub fn parse_xdg_user_dir(contents: &str, key: &str, home: &std::path::Path) -> Option<PathBuf> {
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            continue;
        };
        if lhs.trim() != key {
            continue;
        }
        let mut val = rhs.trim();
        if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            val = &val[1..val.len() - 1];
        }
        let resolved = if let Some(rest) = val.strip_prefix("$HOME/") {
            home.join(rest)
        } else if val == "$HOME" {
            home.to_path_buf()
        } else {
            PathBuf::from(val)
        };
        if resolved.is_absolute() {
            return Some(resolved);
        }
    }
    None
}

/// 解析桌面用户的「图片」目录（优先 `$XDG_PICTURES_DIR`、其次 `~/.config/user-dirs.dirs`，最后按现存 `~/Pictures` / `~/图片` 兜底）。
#[must_use]
pub fn default_pictures_dir() -> Option<PathBuf> {
    if let Some(pics) = std::env::var_os("XDG_PICTURES_DIR") {
        let p = PathBuf::from(&pics);
        if p.is_absolute() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)?;
    let cfg_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let user_dirs = cfg_dir.join("user-dirs.dirs");
    if let Ok(contents) = std::fs::read_to_string(&user_dirs)
        && let Some(pics) = parse_xdg_user_dir(&contents, "XDG_PICTURES_DIR", &home)
    {
        return Some(pics);
    }
    for rel in ["Pictures", "图片"] {
        let candidate = home.join(rel);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    Some(home.join("Pictures"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// 构造一个假环境。
    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from((*v).to_string())))
            .collect();
        move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn parse_accepts_documented_spellings() {
        assert_eq!(Persona::parse("system"), Some(Persona::System));
        assert_eq!(Persona::parse("  USER "), Some(Persona::User));
        assert_eq!(Persona::parse("desktop"), Some(Persona::User));
        assert_eq!(Persona::parse("root"), Some(Persona::System));
        // `auto` 与拼错都必须返回 None，由调用方回落到环境判定。
        assert_eq!(Persona::parse("auto"), None);
        assert_eq!(Persona::parse(""), None);
        assert_eq!(Persona::parse("server"), None);
    }

    #[test]
    fn media_dir_follows_persona() {
        let env = env_map(&[("HOME", "/home/u")]);
        assert_eq!(
            Persona::System.media_dir_with(&env),
            PathBuf::from("/srv/wallpapers")
        );
        assert_eq!(
            Persona::User.media_dir_with(&env),
            PathBuf::from("/home/u/Pictures")
        );
        // HOME 缺失（异常环境）不能 panic，退成相对路径让 --media-dir 覆盖。
        let empty = env_map(&[]);
        assert_eq!(
            Persona::User.media_dir_with(&empty),
            PathBuf::from("Pictures")
        );
    }

    #[test]
    fn detect_prefers_explicit_cli_backend() {
        let no_wl = env_map(&[("HOME", "/home/u")]);
        assert_eq!(
            Persona::detect_with(Some(DisplayBackendKind::Wayland), &no_wl),
            Persona::User,
            "显式 --backend wayland 即使没有 WAYLAND_DISPLAY 也是用户人格"
        );
        let with_wl = env_map(&[("WAYLAND_DISPLAY", "wayland-1")]);
        assert_eq!(
            Persona::detect_with(Some(DisplayBackendKind::Drm), &with_wl),
            Persona::System,
            "显式 --backend drm 即使有会话也是系统人格"
        );
    }

    #[test]
    fn detect_auto_follows_wayland_display() {
        let with_wl = env_map(&[("WAYLAND_DISPLAY", "wayland-0")]);
        assert_eq!(Persona::detect_with(None, &with_wl), Persona::User);
        assert_eq!(
            Persona::detect_with(Some(DisplayBackendKind::Auto), &with_wl),
            Persona::User
        );
        let without = env_map(&[("HOME", "/home/u")]);
        assert_eq!(Persona::detect_with(None, &without), Persona::System);
    }

    #[test]
    fn system_persona_keeps_legacy_paths() {
        let env = env_map(&[]);
        let p = Persona::System;
        assert_eq!(
            p.config_path_with(&env),
            PathBuf::from("/etc/liuguang/config.toml")
        );
        assert_eq!(
            p.state_path_with(&env),
            PathBuf::from("/var/lib/liuguang/state.json")
        );
        assert_eq!(
            p.socket_path_with(&env),
            PathBuf::from("/run/liuguang/control.sock")
        );
    }

    #[test]
    fn user_persona_uses_xdg_vars() {
        let env = env_map(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/home/u/cfg"),
            ("XDG_STATE_HOME", "/home/u/st"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_CACHE_HOME", "/home/u/cache"),
        ]);
        let p = Persona::User;
        assert_eq!(
            p.config_path_with(&env),
            PathBuf::from("/home/u/cfg/liuguang/config.toml")
        );
        assert_eq!(
            p.state_path_with(&env),
            PathBuf::from("/home/u/st/liuguang/state.json")
        );
        assert_eq!(
            p.socket_path_with(&env),
            PathBuf::from("/run/user/1000/liuguang/control.sock")
        );
        assert_eq!(
            p.cache_dir_with(&env),
            PathBuf::from("/home/u/cache/liuguang")
        );
    }

    #[test]
    fn user_persona_falls_back_to_home_when_xdg_unset() {
        let env = env_map(&[("HOME", "/home/u")]);
        let p = Persona::User;
        assert_eq!(
            p.config_path_with(&env),
            PathBuf::from("/home/u/.config/liuguang/config.toml")
        );
        assert_eq!(
            p.state_path_with(&env),
            PathBuf::from("/home/u/.local/state/liuguang/state.json")
        );
        // 没有 XDG_RUNTIME_DIR：socket 退到状态目录（仍属当前用户）。
        assert_eq!(
            p.socket_path_with(&env),
            PathBuf::from("/home/u/.local/state/liuguang/control.sock")
        );
        assert_eq!(
            p.cache_dir_with(&env),
            PathBuf::from("/home/u/.cache/liuguang")
        );
    }

    #[test]
    fn relative_xdg_values_are_ignored() {
        // XDG 规范：非绝对路径的变量值必须被忽略。
        let env = env_map(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "relative/cfg"),
            ("XDG_RUNTIME_DIR", "rel"),
        ]);
        let p = Persona::User;
        assert_eq!(
            p.config_path_with(&env),
            PathBuf::from("/home/u/.config/liuguang/config.toml")
        );
        assert_eq!(
            p.socket_path_with(&env),
            PathBuf::from("/home/u/.local/state/liuguang/control.sock")
        );
    }

    #[test]
    fn parses_localized_xdg_user_dirs() {
        let home = PathBuf::from("/home/alice");
        let text = r#"
# Commented out: XDG_PICTURES_DIR="$HOME/Ignored"
XDG_DOWNLOAD_DIR="$HOME/下载"
XDG_PICTURES_DIR="$HOME/图片"
XDG_VIDEOS_DIR="/mnt/media/Videos"
"#;
        assert_eq!(
            parse_xdg_user_dir(text, "XDG_PICTURES_DIR", &home),
            Some(PathBuf::from("/home/alice/图片"))
        );
        assert_eq!(
            parse_xdg_user_dir(text, "XDG_VIDEOS_DIR", &home),
            Some(PathBuf::from("/mnt/media/Videos"))
        );
        assert_eq!(parse_xdg_user_dir(text, "XDG_MUSIC_DIR", &home), None);
    }
}
