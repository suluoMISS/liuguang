//! 桌面模式（`Persona::User`）下的 `systemd --user` 服务管理与首次运行初始化。
//!
//! 只调用当前登录用户的 `systemctl --user`，不涉及 root 权限，全部同步命令在后台工作线程执行，
//! 避免阻塞 GTK 主线程事件循环。

use std::path::{Path, PathBuf};
use std::process::Command;

const USER_UNIT: &str = "liuguang.service";

/// 用户后台服务与配置当前状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub active: bool,
    pub enabled: bool,
    pub unit_found: bool,
    pub config_exists: bool,
}

/// GUI 可触发的用户级服务操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    EnableAndStart,
    Disable,
    InitConfig,
}

/// 解析 `systemctl --user is-active` 输出。
#[must_use]
pub fn parse_is_active(stdout: &str) -> bool {
    matches!(stdout.trim(), "active" | "activating" | "reloading")
}

/// 解析 `systemctl --user is-enabled` 输出。
#[must_use]
pub fn parse_is_enabled(stdout: &str) -> bool {
    matches!(
        stdout.trim(),
        "enabled" | "enabled-runtime" | "static" | "indirect"
    )
}

/// 解析桌面默认壁纸目录（遵循 `user-dirs.dirs` 中的本地化图片目录）。
#[must_use]
pub fn default_wallpapers_dir() -> Option<PathBuf> {
    liuguang_core::persona::default_pictures_dir().map(|pics| pics.join("Wallpapers"))
}

/// 生成桌面模式默认 `config.toml` 内容。
#[must_use]
pub fn default_desktop_config_toml(media_dir: &Path) -> String {
    let cfg = liuguang_core::config::Config {
        display: liuguang_core::config::DisplayConfig {
            backend: liuguang_core::config::DisplayBackendKind::Auto,
            ..Default::default()
        },
        library: Some(liuguang_core::library::LibraryConfig {
            root: media_dir.to_path_buf(),
            image_dirs: vec![media_dir.to_path_buf()],
            video_dirs: vec![media_dir.to_path_buf()],
            recursive: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let escaped = media_dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    cfg.to_toml_string().unwrap_or_else(|_| {
        format!(
            "[display]\nbackend = \"auto\"\n\n[library]\nroot = \"{escaped}\"\nimage_dirs = [\"{escaped}\"]\nvideo_dirs = [\"{escaped}\"]\nrecursive = true\n"
        )
    })
}

fn desktop_env_vars_to_import() -> Vec<&'static str> {
    const CANDIDATES: &[&str] = &[
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "XDG_SESSION_TYPE",
        "DESKTOP_SESSION",
        "XDG_RUNTIME_DIR",
        "NIRI_SOCKET",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "SWAYSOCK",
        "DISPLAY",
    ];
    CANDIDATES
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
        .collect()
}

/// 查询用户级 systemd 单元与配置现状。
#[must_use]
pub fn query_user_service(config_path: &Path) -> ServiceStatus {
    let config_exists = config_path.is_file();
    let active =
        run_systemctl(&["--user", "is-active", USER_UNIT]).is_ok_and(|out| parse_is_active(&out));
    let enabled =
        run_systemctl(&["--user", "is-enabled", USER_UNIT]).is_ok_and(|out| parse_is_enabled(&out));
    let unit_found = has_user_unit_file() || run_systemctl(&["--user", "cat", USER_UNIT]).is_ok();
    ServiceStatus {
        active,
        enabled,
        unit_found,
        config_exists,
    }
}

fn has_user_unit_file() -> bool {
    let sys_user = Path::new("/usr/lib/systemd/user/liuguang.service");
    let local_user = Path::new("/usr/local/lib/systemd/user/liuguang.service");
    if sys_user.exists() || local_user.exists() {
        return true;
    }
    let cfg_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join(".config"))
        });
    cfg_dir.is_some_and(|dir| dir.join("systemd/user/liuguang.service").exists())
}

/// 确保桌面运行所需目录、`config.toml`（若缺失）与用户会话环境变量就绪。
///
/// 返回当前有效媒体库根目录（或配置路径），供界面提示。
pub fn ensure_user_setup(config_path: &Path) -> Result<PathBuf, String> {
    let media_dir = default_wallpapers_dir()
        .ok_or_else(|| "找不到 HOME 环境变量，无法确定默认壁纸目录".to_owned())?;
    std::fs::create_dir_all(&media_dir)
        .map_err(|e| format!("创建默认壁纸目录失败（{}）：{e}", media_dir.display()))?;

    if let Some(cache_home) = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
    {
        let _ = std::fs::create_dir_all(cache_home.join("liuguang"));
    }

    if let Some(state_home) = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })
    {
        let _ = std::fs::create_dir_all(state_home.join("liuguang"));
    }

    if !config_path.exists() {
        let mut cfg =
            liuguang_core::config::Config::from_toml_str(&default_desktop_config_toml(&media_dir))
                .map_err(|e| format!("构造默认配置失败：{e}"))?;
        cfg.apply_persona_defaults(liuguang_core::persona::Persona::User);
        cfg.slideshow.directory = media_dir.clone();
        cfg.save_atomic(config_path)
            .map_err(|e| format!("写入默认配置失败（{}）：{e}", config_path.display()))?;
    }

    // 把当前 Wayland/合成器会话变量同步给 systemd --user，避免首次安装/登录后单元找不到环境。
    let env_vars = desktop_env_vars_to_import();
    if !env_vars.is_empty() {
        let mut import_args = Vec::with_capacity(2 + env_vars.len());
        import_args.push("--user");
        import_args.push("import-environment");
        import_args.extend(env_vars);
        let _ = run_systemctl(&import_args);
    }
    let _ = run_systemctl(&["--user", "daemon-reload"]);
    Ok(media_dir)
}

/// 执行一项用户服务或配置初始化动作，并返回可读反馈与最新状态。
pub fn run_action(
    action: ServiceAction,
    config_path: &Path,
) -> Result<(String, ServiceStatus), String> {
    let msg = match action {
        ServiceAction::InitConfig => {
            let media_dir = ensure_user_setup(config_path)?;
            format!("桌面配置与壁纸目录已就绪（{}）", media_dir.display())
        }
        ServiceAction::Start => {
            ensure_user_setup(config_path)?;
            run_systemctl_checked(&["--user", "start", USER_UNIT])?;
            "已启动后台壁纸服务".to_owned()
        }
        ServiceAction::Stop => {
            run_systemctl_checked(&["--user", "stop", USER_UNIT])?;
            "已停止后台壁纸服务".to_owned()
        }
        ServiceAction::Restart => {
            ensure_user_setup(config_path)?;
            run_systemctl_checked(&["--user", "restart", USER_UNIT])?;
            "已重启后台壁纸服务".to_owned()
        }
        ServiceAction::EnableAndStart => {
            ensure_user_setup(config_path)?;
            run_systemctl_checked(&["--user", "enable", "--now", USER_UNIT])?;
            "已启动后台壁纸服务并设为登录自启".to_owned()
        }
        ServiceAction::Disable => {
            run_systemctl_checked(&["--user", "disable", USER_UNIT])?;
            "已关闭后台壁纸服务登录自启".to_owned()
        }
    };
    let status = query_user_service(config_path);
    Ok((msg, status))
}

fn run_systemctl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| format!("无法执行 systemctl：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("systemctl {:?} 退出码 {}", args, out.status)
        } else {
            stderr
        })
    }
}

fn run_systemctl_checked(args: &[&str]) -> Result<(), String> {
    run_systemctl(args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_systemctl_active_and_enabled_states() {
        assert!(parse_is_active("active\n"));
        assert!(parse_is_active("activating"));
        assert!(!parse_is_active("inactive\n"));
        assert!(!parse_is_active("failed"));

        assert!(parse_is_enabled("enabled\n"));
        assert!(parse_is_enabled("static"));
        assert!(!parse_is_enabled("disabled\n"));
        assert!(!parse_is_enabled("masked"));
    }

    #[test]
    fn default_desktop_config_round_trips() {
        let dir = PathBuf::from("/tmp/liuguang-wallpapers");
        let text = default_desktop_config_toml(&dir);
        let cfg = liuguang_core::config::Config::from_toml_str(&text)
            .expect("生成的桌面默认配置必须合法可读");
        assert_eq!(
            cfg.display.backend,
            liuguang_core::config::DisplayBackendKind::Auto
        );
        let lib = cfg.library.expect("桌面默认配置必须包含 [library]");
        assert_eq!(lib.root, dir);
        assert!(lib.recursive);
        assert_eq!(lib.image_dirs, vec![dir.clone()]);
        assert_eq!(lib.video_dirs, vec![dir]);
    }
}
