//! 游戏进程探测（`performance.detect_games`）。
//!
//! **为什么需要它**：denial 等不支持 `zwlr_foreign_toplevel_management` 的合成器
//! 拿不到窗口全屏状态（见 `docs/ARCHITECTURE.md` §8.4.2），所以需要从 `/proc`
//! 探测游戏进程，在游戏运行期间让壁纸暂停供给。
//!
//! **Linux 游戏的形态**：
//! - Steam 原生 Linux 游戏：`reaper SteamLaunch AppId=<id>`、位于
//!   `steamapps/common/<游戏目录>/` 下的可执行文件，或环境带 `SteamGameId`；
//! - Steam + Proton：`reaper` / `proton waitforexitandrun` / Windows `*.exe`
//!   （注意 Linux `comm` 截断到 15 字符，如 `SB-Win64-Shipping.exe` 会变成
//!   `SB-Win64-Shippi`，因此需要从 `cmdline` 参数与 `exe` 链接恢复完整名）；
//! - umu / Heroic / Lutris / gamescope / 模拟器 / Minecraft Java；
//! - 用户显式名单：`game_processes` 追加，`game_ignore_processes` 排除。
//!
//! **重要限制（Yama ptrace_scope）**：开启 `kernel.yama.ptrace_scope >= 1` 时，
//! 用户服务无权读取非子进程的 `/proc/<pid>/environ`（`EACCES`）。因此环境变量
//! 只作延迟兜底（其余低成本 `stat`/`comm`/`cmdline`/`exe` 判据未命中时再尝试读），
//! 主体依赖全局可读的 `comm`/`cmdline`/`stat`/`exe`。
//!
//! **滞回**：默认每 [`SAMPLE_INTERVAL`]（2s）扫描一次，连续 [`CONFIRM_SAMPLES`]
//! 次命中才暂停（≈4s），连续 [`ABSENT_SAMPLES`] 次未命中才恢复（≈8s）——
//! 启动器/过图/短时切出不会让壁纸反复启停。

use std::path::Path;
use std::time::{Duration, Instant};

/// 采样间隔（单次扫 `/proc` 不主动读 `environ` 时仅微秒～毫秒级）。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// 连续命中多少次才判定「游戏在跑」。
pub const CONFIRM_SAMPLES: u8 = 2;

/// 连续未命中多少次才判定「游戏结束」。
pub const ABSENT_SAMPLES: u8 = 4;

/// Steam 运行时（`steamrt64/pv-runtime`）的容器辅助进程：只要 Steam 客户端开着
/// 就一直在跑，与「有没有游戏」无关。
const STEAM_RUNTIME_MARKERS: &[&str] = &["pv-runtime", "steamrt/", "steam-runtime-tools"];
const STEAM_RUNTIME_COMMS: &[&str] = &["pv-adverb", "srt-bwrap", "srt-logger"];
const STEAM_RUNTIME_COMM_PREFIXES: &[&str] = &["srt-", "pv-"];

/// Steam 已知工具/运行时 AppId：即便偶发通过 `reaper` 启动也不算在玩游戏。
const STEAM_TOOL_APP_IDS: &[&str] = &[
    "1070560", // SteamLinuxRuntime_soldier
    "1391110", // SteamLinuxRuntime_sniper (old)
    "1628350", // SteamLinuxRuntime 3.0 (sniper)
    "228980",  // Steamworks Common Redistributables
    "1493710", // Proton Experimental
    "1887720", // Proton 7.0
    "2348590", // Proton 8.0
    "2805730", // Proton 9.0
    "2180100", // Proton Hotfix
];

/// 游戏包装/启动器二进制（只按 `comm` 或 `argv[0]` basename 精确匹配）。
const GAME_WRAPPER_BINS: &[&str] = &["gamescope", "umu-run", "lutris-wrapper"];

/// Wine / Proton 加载器（只有后面真的跟着非系统、非聊天的 `.exe` 才算命中）。
const WINE_LOADER_BINS: &[&str] = &["wine", "wine64", "wine-preloader", "wine64-preloader"];

/// 常见主机/街机模拟器二进制名（按 `comm` / `argv[0]` basename 匹配）。
const EMULATOR_BINS: &[&str] = &[
    "rpcs3",
    "ryujinx",
    "yuzu",
    "suyu",
    "citron",
    "eden",
    "pcsx2",
    "pcsx2-qt",
    "dolphin-emu",
    "cemu",
    "xemu",
    "ppsspp",
    "ppssppsdl",
    "ppssppqt",
    "duckstation",
    "duckstation-qt",
    "flycast",
    "melonds",
    "azahar",
    "retroarch",
    "mame",
];

/// Minecraft Java 客户端常见启动参数关键字（`java`/`javaw` 进程带其中之一才算游戏）。
const MINECRAFT_MARKERS: &[&str] = &[
    "net.minecraft.",
    "cpw.mods.bootstraplauncher",
    "net.fabricmc.",
    "org.prismlauncher",
    ".minecraft/versions/",
    "quiltmc.",
];

/// 环境变量里出现即视为游戏进程（键名，值不能为空、不能是 `0`）。
const GAME_ENV_KEYS: &[&str] = &[
    "STEAMGAMEID",
    "STEAMAPPID",
    "STEAM_COMPAT_APP_ID",
    "UMU_ID",
    "PROTONPATH",
    "LUTRIS_GAME_UUID",
    "GAMEID",
];

/// Wine 自身的基础设施与辅助工具：任何 Wine/Proton 前缀都会启动，不算游戏。
const WINE_INFRA_EXES: &[&str] = &[
    "explorer.exe",
    "services.exe",
    "winedevice.exe",
    "plugplay.exe",
    "svchost.exe",
    "rpcss.exe",
    "wineboot.exe",
    "conhost.exe",
    "winemenubuilder.exe",
    "tabtip.exe",
    "steam.exe",
    "steamwebhelper.exe",
    "start.exe",
    "rundll32.exe",
    "regedit.exe",
    "msiexec.exe",
    "taskmgr.exe",
    "winecfg.exe",
    "control.exe",
    "uninstaller.exe",
    "xalia.exe",
    "iexplore.exe",
    "winebrowser.exe",
    "wineserver.exe",
    "spoolsv.exe",
    "mscorsvw.exe",
    "ngen.exe",
    "dxdiag.exe",
    "dpnsvr.exe",
    "regsvr32.exe",
    "winepath.exe",
    "winhlp32.exe",
    "wordpad.exe",
    "hh.exe",
    "crashhandler.exe",
];

/// shell / 编辑器 / 浏览器 / 文件管理器等工具：它们的 cmdline 可能恰好出现
/// 游戏路径或 `.exe` 名（比如排查文件或浏览文件夹），绝不能因此暂停壁纸。
const NON_GAME_EXES: &[&str] = &[
    "bash",
    "sh",
    "dash",
    "zsh",
    "fish",
    "sleep",
    "grep",
    "rg",
    "cat",
    "less",
    "more",
    "find",
    "fd",
    "rsync",
    "cp",
    "mv",
    "rm",
    "tar",
    "unzip",
    "7z",
    "vim",
    "nvim",
    "emacs",
    "helix",
    "hx",
    "nano",
    "kate",
    "gedit",
    "code",
    "codium",
    "cursor",
    "zed",
    "subl",
    "python",
    "python3",
    "node",
    "deno",
    "bun",
    "cargo",
    "rustc",
    "git",
    "tmux",
    "screen",
    "ssh",
    "make",
    "ninja",
    "cmake",
    "perl",
    "ruby",
    "ffmpeg",
    "ffprobe",
    "mpv",
    "vlc",
    "obs",
    "steam",
    "steamwebhelper",
    "steamservice",
    "gldriverquery",
    "vulkaninfo",
    "firefox",
    "firefox-bin",
    "chromium",
    "chrome",
    "google-chrome",
    "brave",
    "vivaldi",
    "msedge",
    "dolphin",
    "nautilus",
    "nemo",
    "thunar",
    "pcmanfm",
    "kitty",
    "alacritty",
    "wezterm",
    "wezterm-gui",
    "foot",
    "konsole",
    "gnome-terminal",
    "ghostty",
    "electron",
    "discord",
    "vesktop",
    "obsidian",
    "spotify",
    "systemctl",
    "journalctl",
    "pgrep",
    "pkill",
    "htop",
    "btop",
    "top",
    "ps",
    "curl",
    "wget",
    "xdg-open",
];

/// 常见的「Windows 桌面/办公/聊天应用」：命中 `*.exe` 时排除。
const KNOWN_APP_EXES: &[&str] = &[
    "qq.exe",
    "wechat.exe",
    "weixin.exe",
    "tim.exe",
    "wemeetapp.exe",
    "dingtalk.exe",
    "wxwork.exe",
    "feishu.exe",
    "lark.exe",
    "notepad.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "wps.exe",
    "et.exe",
    "wpp.exe",
    "foxmail.exe",
    "thunderbird.exe",
    "telegram.exe",
    "discord.exe",
    "baidunetdisk.exe",
];

/// 一次命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// 进程号。
    pub pid: u32,
    /// 展示名（优先 `.exe` 完整文件名，否则回落到 `/proc/<pid>/comm`）。
    pub name: String,
    /// 命中依据，便于日志排查：`env:SteamGameId`、`comm:.exe`、`steam:AppId=...` 等。
    pub evidence: String,
}

/// 探测器（无状态：每次采样重新扫 `/proc`）。
#[derive(Debug, Clone, Default)]
pub struct Detector {
    extra: Vec<String>,
    ignore: Vec<String>,
}

impl Detector {
    /// 用配置构造。`extra` / `ignore` 都是**不区分大小写的子串**。
    #[must_use]
    pub fn new(extra: Vec<String>, ignore: Vec<String>) -> Self {
        Self {
            extra: extra
                .into_iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            ignore: ignore
                .into_iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// 扫描真实 `/proc`。
    #[must_use]
    pub fn scan(&self) -> Vec<Hit> {
        self.scan_proc(Path::new("/proc"))
    }

    /// 扫描指定根目录（单测用假 `/proc` 树）。
    #[must_use]
    pub fn scan_proc(&self, root: &Path) -> Vec<Hit> {
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        let is_real_proc = root == Path::new("/proc");
        let self_pid = std::process::id();
        let mut hits = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if is_real_proc && pid == self_pid {
                continue;
            }
            if let Some(hit) = self.inspect(pid, &entry.path()) {
                hits.push(hit);
            }
        }
        hits.sort_by_key(|h| h.pid);
        hits
    }

    fn inspect(&self, pid: u32, dir: &Path) -> Option<Hit> {
        let comm = read_trimmed(&dir.join("comm")).unwrap_or_default();
        let args = read_cmdline_args(&dir.join("cmdline"));
        if let Some((state, ppid)) = read_proc_stat(&dir.join("stat")) {
            if matches!(state, 'Z' | 'X' | 'x') {
                return None;
            }
            if args.is_empty() && (pid == 2 || ppid == 2) {
                return None;
            }
        }
        if comm.is_empty() && args.is_empty() {
            return None;
        }

        let exe = std::fs::read_link(dir.join("exe"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        let cwd = std::fs::read_link(dir.join("cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());

        let comm_lower = comm.to_lowercase();
        let argv0 = args.first().map(String::as_str).unwrap_or("");
        let argv0_base = basename_any(argv0).to_lowercase();
        let exe_base = exe
            .as_deref()
            .map(|e| basename_any(e).to_lowercase())
            .unwrap_or_default();

        if is_self_binary(&comm_lower) || is_self_binary(&argv0_base) || is_self_binary(&exe_base) {
            return None;
        }

        let cmdline_joined = args.join(" ");
        let raw_haystack = format!(
            "{comm} {cmdline_joined} {} {}",
            exe.as_deref().unwrap_or(""),
            cwd.as_deref().unwrap_or("")
        )
        .to_lowercase();
        let norm_haystack = normalize_path(&raw_haystack);

        if self
            .ignore
            .iter()
            .any(|pat| raw_haystack.contains(pat) || norm_haystack.contains(pat))
        {
            return None;
        }

        // Steam 运行时容器辅助进程只在引导/伴随运行时出现，不能当游戏。
        if is_steam_runtime_helper(&comm_lower, &argv0_base, argv0, exe.as_deref()) {
            return None;
        }

        // Proton 启动脚本常见形态：`python3 .../proton waitforexitandrun ...`；
        // umu / lutris 同理以 python 为宿主，因此对 python 保留特例。
        if is_python_bin(&comm_lower, &argv0_base) {
            if is_proton_wait_cmd(&args) {
                return Some(Hit {
                    pid,
                    name: if comm.is_empty() {
                        "proton".to_string()
                    } else {
                        comm
                    },
                    evidence: "proton:waitforexitandrun".to_string(),
                });
            }
            if let Some(wrapper) = python_game_wrapper(&args) {
                return Some(Hit {
                    pid,
                    name: wrapper.to_string(),
                    evidence: format!("wrapper:{wrapper}"),
                });
            }
        }

        // 非游戏应用（shell / 编辑器 / 浏览器 / 文件管理器等）尽早排除。
        if is_non_game_bin(&comm_lower, &argv0_base, &exe_base) {
            return None;
        }

        // 1. 直接的 Windows `.exe`（包含从 cmdline 找回被 15 字符截断的 `comm`）。
        if let Some((exe_lower, exe_display, evidence)) =
            extract_exe_target(&comm, &comm_lower, &args, exe.as_deref())
        {
            if WINE_INFRA_EXES.contains(&exe_lower.as_str())
                || KNOWN_APP_EXES.contains(&exe_lower.as_str())
            {
                return None;
            }
            return Some(Hit {
                pid,
                name: exe_display,
                evidence,
            });
        }

        // 2. Steam reaper 正在托管一个带非零 AppId 的启动会话。
        if let Some(app_id) = extract_steam_reaper_appid(&comm_lower, &argv0_base, &args) {
            if !STEAM_TOOL_APP_IDS.contains(&app_id.as_str()) {
                return Some(Hit {
                    pid,
                    name: format!("SteamApp:{app_id}"),
                    evidence: format!("steam:AppId={app_id}"),
                });
            }
        }

        // 3. Proton 脚本不走 python argv0（直接 shebang 改 comm 为 proton）的场景。
        if is_proton_wait_cmd(&args) {
            return Some(Hit {
                pid,
                name: comm,
                evidence: "proton:waitforexitandrun".to_string(),
            });
        }

        // 4. 安装在 `steamapps/common/<非工具目录>/` 下的原生或托管进程。
        for candidate in [Some(argv0), exe.as_deref()].into_iter().flatten() {
            if let Some(game_dir) = steam_common_game(candidate) {
                return Some(Hit {
                    pid,
                    name: if comm.is_empty() {
                        basename_any(candidate).to_string()
                    } else {
                        comm
                    },
                    evidence: format!("path:steamapps/common/{game_dir}"),
                });
            }
        }

        // 5. 游戏包装器与主机模拟器。
        if let Some(wrapper) = matching_bin(&comm_lower, &argv0_base, GAME_WRAPPER_BINS) {
            return Some(Hit {
                pid,
                name: comm,
                evidence: format!("wrapper:{wrapper}"),
            });
        }
        if let Some(emu) = matching_bin(&comm_lower, &argv0_base, EMULATOR_BINS) {
            return Some(Hit {
                pid,
                name: comm,
                evidence: format!("emulator:{emu}"),
            });
        }

        // 6. Minecraft Java 客户端。
        if is_minecraft_java(&comm_lower, &argv0_base, &norm_haystack) {
            return Some(Hit {
                pid,
                name: "Minecraft".to_string(),
                evidence: "java:minecraft".to_string(),
            });
        }

        // 7. 环境变量（仅前置低成本判据均未命中时读取，避免 Yama 下全表 EACCES 浪费）。
        if let Some(evidence) = self.env_evidence(dir) {
            return Some(Hit {
                pid,
                name: comm,
                evidence,
            });
        }

        // 8. 用户显式配置 `performance.game_processes`。
        for extra in &self.extra {
            if raw_haystack.contains(extra) || norm_haystack.contains(extra) {
                return Some(Hit {
                    pid,
                    name: comm,
                    evidence: format!("config:{extra}"),
                });
            }
        }

        None
    }

    fn env_evidence(&self, dir: &Path) -> Option<String> {
        let raw = std::fs::read(dir.join("environ")).ok()?;
        for entry in raw.split(|b| *b == 0) {
            let Ok(text) = std::str::from_utf8(entry) else {
                continue;
            };
            let Some((key, value)) = text.split_once('=') else {
                continue;
            };
            if !GAME_ENV_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k)) {
                continue;
            }
            let value = value.trim();
            if value.is_empty() || value == "0" {
                continue;
            }
            return Some(format!("env:{key}={value}"));
        }
        None
    }
}

fn normalize_path(s: &str) -> String {
    s.replace('\\', "/").to_lowercase()
}

fn basename_any(s: &str) -> &str {
    let trimmed = s.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    }
}

fn is_self_binary(name_lower: &str) -> bool {
    matches!(
        name_lower,
        "liuguang" | "liuguangctl" | "liuguang-tui" | "liuguang-gui"
    )
}

fn is_steam_runtime_helper(
    comm_lower: &str,
    argv0_base: &str,
    argv0: &str,
    exe: Option<&str>,
) -> bool {
    if STEAM_RUNTIME_COMMS.contains(&comm_lower)
        || STEAM_RUNTIME_COMMS.contains(&argv0_base)
        || STEAM_RUNTIME_COMM_PREFIXES
            .iter()
            .any(|p| comm_lower.starts_with(p) || argv0_base.starts_with(p))
    {
        return true;
    }
    let argv0_norm = normalize_path(argv0);
    let exe_norm = exe.map(normalize_path).unwrap_or_default();
    STEAM_RUNTIME_MARKERS
        .iter()
        .any(|m| argv0_norm.contains(m) || exe_norm.contains(m))
}

fn is_python_bin(comm_lower: &str, argv0_base: &str) -> bool {
    let is_py = |s: &str| s == "python" || s.starts_with("python3");
    is_py(comm_lower) || is_py(argv0_base)
}

fn is_proton_wait_cmd(args: &[String]) -> bool {
    let has_proton = args
        .iter()
        .any(|a| basename_any(a).eq_ignore_ascii_case("proton"));
    let has_wait = args
        .iter()
        .any(|a| a.eq_ignore_ascii_case("waitforexitandrun"));
    has_proton && has_wait
}

fn python_game_wrapper(args: &[String]) -> Option<&'static str> {
    for arg in args.iter().skip(1) {
        if arg.starts_with('-') {
            continue;
        }
        let base = basename_any(arg).to_lowercase();
        if let Some(&w) = GAME_WRAPPER_BINS.iter().find(|&&w| w == base) {
            return Some(w);
        }
        break;
    }
    None
}

fn is_non_game_bin(comm_lower: &str, argv0_base: &str, exe_base: &str) -> bool {
    NON_GAME_EXES.contains(&comm_lower)
        || NON_GAME_EXES.contains(&argv0_base)
        || (!exe_base.is_empty() && NON_GAME_EXES.contains(&exe_base))
}

fn matching_bin<'a>(comm_lower: &str, argv0_base: &str, list: &[&'a str]) -> Option<&'a str> {
    list.iter()
        .copied()
        .find(|&bin| bin == comm_lower || bin == argv0_base)
}

fn extract_exe_target(
    comm: &str,
    comm_lower: &str,
    args: &[String],
    exe: Option<&str>,
) -> Option<(String, String, String)> {
    if comm_lower.ends_with(".exe") {
        return Some((
            comm_lower.to_string(),
            comm.to_string(),
            "comm:.exe".to_string(),
        ));
    }

    if let Some(arg0) = args.first() {
        let base = basename_any(arg0);
        let lower = base.to_lowercase();
        if lower.ends_with(".exe") {
            return Some((
                lower.clone(),
                base.to_string(),
                format!("cmdline-exe:{lower}"),
            ));
        }

        // Wine loader 启动：从紧随其后的参数找出目标 `.exe`。
        if WINE_LOADER_BINS.contains(&lower.as_str()) || WINE_LOADER_BINS.contains(&comm_lower) {
            for arg in args.iter().skip(1).take(6) {
                if arg.starts_with('-') {
                    continue;
                }
                let target_base = basename_any(arg);
                let target_lower = target_base.to_lowercase();
                if target_lower.ends_with(".exe") {
                    return Some((
                        target_lower.clone(),
                        target_base.to_string(),
                        format!("wine-exe:{target_lower}"),
                    ));
                }
            }
        }
    }

    if let Some(exe_path) = exe {
        let base = basename_any(exe_path);
        let lower = base.to_lowercase();
        if lower.ends_with(".exe") {
            return Some((lower.clone(), base.to_string(), format!("exe:{lower}")));
        }
    }

    None
}

fn extract_steam_reaper_appid(
    comm_lower: &str,
    argv0_base: &str,
    args: &[String],
) -> Option<String> {
    if comm_lower != "reaper" && argv0_base != "reaper" {
        return None;
    }
    let has_launch = args.iter().any(|a| a == "SteamLaunch");
    if !has_launch {
        return None;
    }
    for arg in args {
        if let Some(id) = arg.strip_prefix("AppId=") {
            if !id.is_empty() && id != "0" && id.bytes().all(|b| b.is_ascii_digit()) {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn is_steam_tool_dir(dir_lower: &str) -> bool {
    dir_lower == "proton"
        || dir_lower.starts_with("proton ")
        || dir_lower.starts_with("proton-")
        || dir_lower.starts_with("proton_")
        || dir_lower.starts_with("steamlinuxruntime")
        || dir_lower.starts_with("steamworks")
        || dir_lower.starts_with("steam controller")
}

fn steam_common_game(candidate: &str) -> Option<String> {
    if candidate.is_empty() {
        return None;
    }
    let norm = normalize_path(candidate);
    let marker = "steamapps/common/";
    let idx = norm.find(marker)?;
    let rest = &norm[idx + marker.len()..];
    let dir = rest.split('/').next()?.trim();
    if dir.is_empty() || is_steam_tool_dir(dir) {
        return None;
    }
    Some(dir.to_string())
}

fn is_minecraft_java(comm_lower: &str, argv0_base: &str, norm_haystack: &str) -> bool {
    let is_java = |s: &str| s == "java" || s == "javaw" || s.starts_with("java-");
    if !is_java(comm_lower) && !is_java(argv0_base) {
        return false;
    }
    MINECRAFT_MARKERS.iter().any(|m| norm_haystack.contains(m))
}

fn read_proc_stat(path: &Path) -> Option<(char, u32)> {
    let raw = std::fs::read_to_string(path).ok()?;
    // `(comm)` 允许包含空格或右括号，因此锚定最后一个 `)`。
    let close = raw.rfind(')')?;
    let tail = raw.get(close + 1..)?.trim_start();
    let mut fields = tail.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    Some((state, ppid))
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim_end_matches('\n').to_string())
}

fn read_cmdline_args(path: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    raw.split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// 状态机输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 游戏开始：暂停在播的视频。
    Pause,
    /// 游戏结束：恢复此前由本组件暂停的通道。
    Resume,
    /// 不变。
    Unchanged,
}

/// 带滞回与节流的控制器（tick 里调用 [`Controller::poll`]）。
#[derive(Debug)]
pub struct Controller {
    detector: Option<Detector>,
    last_scan: Option<Instant>,
    hits: Vec<Hit>,
    streak: u8,
    misses: u8,
    engaged: bool,
}

impl Controller {
    /// `enabled == false` ⇒ 永不动作。
    #[must_use]
    pub fn new(enabled: bool, extra: Vec<String>, ignore: Vec<String>) -> Self {
        Self {
            detector: enabled.then(|| Detector::new(extra, ignore)),
            last_scan: None,
            hits: Vec::new(),
            streak: 0,
            misses: 0,
            engaged: false,
        }
    }

    /// 当前是否处于「已因游戏在跑而暂停」的滞回态。
    #[must_use]
    pub fn is_engaged(&self) -> bool {
        self.engaged
    }

    /// 热重载配置：更新黑白名单或开关探测；若从开启切为关闭且当前已暂停，立即返回 [`Action::Resume`]。
    pub fn reconfigure(
        &mut self,
        enabled: bool,
        extra: Vec<String>,
        ignore: Vec<String>,
    ) -> Action {
        if !enabled {
            self.detector = None;
            self.last_scan = None;
            self.hits.clear();
            self.streak = 0;
            self.misses = 0;
            if self.engaged {
                self.engaged = false;
                return Action::Resume;
            }
            return Action::Unchanged;
        }
        self.detector = Some(Detector::new(extra, ignore));
        self.last_scan = None;
        Action::Unchanged
    }

    /// 最近一次扫描命中的游戏进程（日志/状态用）。
    #[must_use]
    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    /// 每 tick 调一次；内部按 [`SAMPLE_INTERVAL`] 节流。
    pub fn poll(&mut self, now: Instant) -> Action {
        // 单测里不扫描宿主机真实 `/proc`：防止开发者本机正在跑 Lutris/Steam 时污染无关用例。
        if cfg!(test) {
            return Action::Unchanged;
        }
        let Some(detector) = &self.detector else {
            return Action::Unchanged;
        };
        if let Some(last) = self.last_scan {
            if now.duration_since(last) < SAMPLE_INTERVAL {
                return Action::Unchanged;
            }
        }
        self.last_scan = Some(now);
        let hits = detector.scan();
        self.apply_hits(hits)
    }

    /// 喂一次扫描结果（单测直接调它验证滞回）。
    fn apply_hits(&mut self, hits: Vec<Hit>) -> Action {
        let found = !hits.is_empty();
        self.hits = hits;
        if !found {
            self.streak = 0;
            self.misses = self.misses.saturating_add(1);
            if self.engaged && self.misses >= ABSENT_SAMPLES {
                self.engaged = false;
                self.misses = 0;
                return Action::Resume;
            }
            return Action::Unchanged;
        }
        self.misses = 0;
        self.streak = self.streak.saturating_add(1);
        if !self.engaged && self.streak >= CONFIRM_SAMPLES {
            self.engaged = true;
            self.streak = 0;
            return Action::Pause;
        }
        Action::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_proc(root: &Path, pid: u32, comm: &str, cmdline: &[&str], env: &[&str]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).expect("目录");
        fs::write(dir.join("comm"), format!("{comm}\n")).expect("comm");
        let mut raw = Vec::new();
        for arg in cmdline {
            raw.extend_from_slice(arg.as_bytes());
            raw.push(0);
        }
        fs::write(dir.join("cmdline"), raw).expect("cmdline");
        let mut envraw = Vec::new();
        for kv in env {
            envraw.extend_from_slice(kv.as_bytes());
            envraw.push(0);
        }
        fs::write(dir.join("environ"), envraw).expect("environ");
    }

    fn fake_stat(root: &Path, pid: u32, comm: &str, state: char, ppid: u32) {
        let dir = root.join(pid.to_string());
        fs::write(
            dir.join("stat"),
            format!("{pid} ({comm}) {state} {ppid} 1 1 0 -1 0 0 0 0 0 0 0 20 0 1 0 100 0 0\n"),
        )
        .expect("stat");
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gw-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("根");
        dir
    }

    #[test]
    fn environment_variables_are_used_when_readable() {
        let root = tmpdir("env");
        fake_proc(
            &root,
            100,
            "native-game",
            &["/opt/native-game/bin/game"],
            &["SteamGameId=1966720", "WINEPREFIX=/home/x/.steam"],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].evidence.starts_with("env:"), "{}", hits[0].evidence);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wine_exe_is_detected_without_env() {
        let root = tmpdir("exe");
        fake_proc(&root, 101, "EldenRing.exe", &["wine", "EldenRing.exe"], &[]);
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].evidence, "comm:.exe");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn truncated_comm_exe_is_recovered_from_windows_cmdline() {
        let root = tmpdir("trunc");
        fake_proc(
            &root,
            1011,
            "SB-Win64-Shippi",
            &[
                "Z:\\home\\me\\.local\\share\\Steam\\steamapps\\common\\StellarBlade\\SB\\Binaries\\Win64\\SB-Win64-Shipping.exe",
                "-dx12",
            ],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].name, "SB-Win64-Shipping.exe");
        assert!(
            hits[0].evidence.contains("sb-win64-shipping.exe"),
            "{hits:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn browser_and_qt_apps_are_not_games() {
        let root = tmpdir("noise");
        fake_proc(
            &root,
            102,
            "chromium",
            &["/usr/lib/chromium", "--type=gpu"],
            &[],
        );
        fake_proc(&root, 103, "QQ", &["/opt/QQ/qq", "--no-sandbox"], &[]);
        fake_proc(&root, 104, "steam", &["/usr/bin/steam", "-silent"], &[]);
        fake_proc(
            &root,
            1041,
            "dolphin",
            &[
                "/usr/bin/dolphin",
                "/home/me/.local/share/Steam/steamapps/common/Cyberpunk 2077",
            ],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert!(hits.is_empty(), "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ignore_list_wins_over_env() {
        let root = tmpdir("ignore");
        fake_proc(&root, 105, "proton", &["wine", "x.exe"], &["SteamAppId=1"]);
        let hits = Detector::new(Vec::new(), vec!["proton".to_string()]).scan_proc(&root);
        assert!(hits.is_empty(), "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extra_list_catches_native_games() {
        let root = tmpdir("native");
        fake_proc(
            &root,
            106,
            "factorio",
            &["/games/factorio/bin/x64/factorio"],
            &[],
        );
        let detector = Detector::new(vec!["factorio".to_string()], Vec::new());
        let hits = detector.scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].evidence, "config:factorio");
        fs::remove_dir_all(&root).ok();
    }

    fn hit(pid: u32) -> Hit {
        Hit {
            pid,
            name: "Game.exe".to_string(),
            evidence: "comm:.exe".to_string(),
        }
    }

    #[test]
    fn hysteresis_needs_two_hits_and_four_misses() {
        let mut c = Controller::new(true, Vec::new(), Vec::new());
        assert!(!c.is_engaged());
        assert_eq!(c.apply_hits(vec![hit(1)]), Action::Unchanged);
        assert_eq!(c.apply_hits(vec![hit(1)]), Action::Pause);
        assert!(c.is_engaged());
        assert_eq!(c.apply_hits(vec![hit(1)]), Action::Unchanged);
        assert_eq!(c.apply_hits(Vec::new()), Action::Unchanged);
        assert_eq!(c.apply_hits(Vec::new()), Action::Unchanged);
        assert_eq!(c.apply_hits(Vec::new()), Action::Unchanged);
        assert_eq!(c.apply_hits(Vec::new()), Action::Resume);
        assert!(!c.is_engaged());
        assert_eq!(c.apply_hits(Vec::new()), Action::Unchanged);
    }

    #[test]
    fn steam_library_path_is_detected_without_env_access() {
        let root = tmpdir("steamapps");
        fake_proc(
            &root,
            9001,
            "somegame",
            &[
                "/home/me/.steam/steam/steamapps/common/SomeGame/somegame",
                "--flag",
            ],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].evidence.contains("steamapps/common"), "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn steam_tools_under_common_do_not_pause() {
        let root = tmpdir("steamtools");
        fake_proc(
            &root,
            9010,
            "_v2-entry-point",
            &[
                "/home/me/.local/share/Steam/steamapps/common/SteamLinuxRuntime_sniper/_v2-entry-point",
            ],
            &[],
        );
        fake_proc(
            &root,
            9011,
            "wineserver",
            &[
                "/home/me/.local/share/Steam/steamapps/common/Proton - Experimental/files/bin/wineserver",
            ],
            &[],
        );
        fake_proc(&root, 9012, "wineserver", &["/usr/bin/wineserver"], &[]);
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert!(
            hits.is_empty(),
            "Steam 工具目录与空闲 wineserver 不算游戏: {hits:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wine_preloader_basename_is_detected() {
        let root = tmpdir("winepre");
        fake_proc(
            &root,
            9002,
            "wine64-preloader",
            &["/usr/lib/wine/wine64-preloader", "C:\\game.exe"],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn proton_waitforexitandrun_under_python_and_steam_reaper_hit() {
        let root = tmpdir("protonpy");
        fake_proc(
            &root,
            9020,
            "python3",
            &[
                "/usr/bin/python3",
                "/home/me/.local/share/Steam/steamapps/common/Proton 9.0 (Beta)/proton",
                "waitforexitandrun",
                "/home/me/.local/share/Steam/steamapps/common/Cyberpunk 2077/bin/x64/Cyberpunk2077.exe",
            ],
            &[],
        );
        fake_proc(
            &root,
            9021,
            "reaper",
            &[
                "/home/me/.local/share/Steam/ubuntu12_32/reaper",
                "SteamLaunch",
                "AppId=1091500",
                "--",
                "/home/me/.local/share/Steam/steamapps/common/SteamLinuxRuntime_sniper/pv-wrapper",
            ],
            &[],
        );
        fake_proc(
            &root,
            9022,
            "reaper",
            &[
                "/home/me/.local/share/Steam/ubuntu12_32/reaper",
                "SteamLaunch",
                "AppId=0",
            ],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].evidence, "proton:waitforexitandrun");
        assert_eq!(hits[1].evidence, "steam:AppId=1091500");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn lutris_game_env_is_detected() {
        let root = tmpdir("lutrisenv");
        fake_proc(
            &root,
            9101,
            "ledger",
            &["/opt/ledger"],
            &["LUTRIS_GAME_UUID=deadbeef"],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].evidence.contains("LUTRIS_GAME_UUID"), "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn umu_game_id_is_detected() {
        let root = tmpdir("umu");
        fake_proc(
            &root,
            7,
            "wine64-preloader",
            &["wine64-preloader", "Game.exe"],
            &["GAMEID=umu-1234"],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn steam_runtime_helpers_are_not_games() {
        let root = tmpdir("steamrt");
        let pv = "/home/me/.local/share/Steam/steamrt64/pv-runtime/steam-runtime-steamrt/pressure-vessel/libexec/steam-runtime-tools-0";
        fake_proc(
            &root,
            1280,
            "srt-logger",
            &[&format!("{pv}/srt-logger"), "--sh-syntax"],
            &[],
        );
        fake_proc(
            &root,
            1547,
            "srt-client-cont",
            &[
                &format!("{pv}/srt-client-container"),
                "--pv-option=--terminate-idle-timeout=120",
            ],
            &[],
        );
        fake_proc(
            &root,
            1819,
            "pv-adverb",
            &[
                &format!("{pv}/pv-adverb"),
                "--prefix=/usr/lib/pressure-vessel/from-host",
            ],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert!(hits.is_empty(), "Steam 运行时辅助进程不算游戏: {hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn proton_game_inside_steam_library_still_hits() {
        let root = tmpdir("protongame");
        fake_proc(
            &root,
            2001,
            "somegame.exe",
            &["Z:\\home\\me\\.local\\share\\Steam\\steamapps\\common\\SomeGame\\somegame.exe"],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 1, "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn shell_mentioning_steam_library_is_not_a_game() {
        let root = tmpdir("shellfp");
        fake_proc(
            &root,
            9201,
            "bash",
            &["bash", "-c", "ls /tmp/steamapps/common/SomeGame/somegame"],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert!(hits.is_empty(), "{hits:?}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wine_infrastructure_and_chat_apps_do_not_pause_wallpaper() {
        let root = tmpdir("infra");
        fake_proc(
            &root,
            11,
            "explorer.exe",
            &["C:\\windows\\explorer.exe"],
            &["WINEPREFIX=/home/x/.wine"],
        );
        fake_proc(
            &root,
            12,
            "services.exe",
            &["C:\\windows\\services.exe"],
            &["WINEPREFIX=/home/x/.wine"],
        );
        fake_proc(&root, 13, "QQ.exe", &["C:\\Program Files\\QQ.exe"], &[]);
        fake_proc(
            &root,
            14,
            "WeChat.exe",
            &["wine64", "C:\\Program Files\\Tencent\\WeChat\\WeChat.exe"],
            &[],
        );
        fake_proc(
            &root,
            15,
            "xalia.exe",
            &["C:\\Program Files\\xalia\\xalia.exe"],
            &["WINEPREFIX=/home/x/.wine"],
        );
        fake_proc(
            &root,
            16,
            "iexplore.exe",
            &["C:\\windows\\system32\\iexplore.exe"],
            &["WINEPREFIX=/home/x/.wine"],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert!(
            hits.is_empty(),
            "不该把 wine 基础设施/聊天软件当游戏: {hits:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn zombie_processes_emulators_and_minecraft_work() {
        let root = tmpdir("emuz");
        fake_proc(&root, 2001, "Game.exe", &["Game.exe"], &[]);
        fake_stat(&root, 2001, "Game.exe", 'Z', 1);
        fake_proc(&root, 2002, "pcsx2-qt", &["/usr/bin/pcsx2-qt"], &[]);
        fake_proc(
            &root,
            2003,
            "java",
            &["/usr/bin/java", "-Xmx4G", "net.minecraft.client.main.Main"],
            &[],
        );
        fake_proc(
            &root,
            2004,
            "java",
            &["/usr/bin/java", "-jar", "/opt/idea/lib/idea.jar"],
            &[],
        );
        let hits = Detector::new(Vec::new(), Vec::new()).scan_proc(&root);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].evidence, "emulator:pcsx2-qt");
        assert_eq!(hits[1].evidence, "java:minecraft");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn disabled_controller_never_acts() {
        let mut c = Controller::new(false, Vec::new(), Vec::new());
        let now = Instant::now();
        assert_eq!(c.poll(now), Action::Unchanged);
        assert!(c.hits().is_empty());
    }

    #[test]
    fn reconfigure_disabling_resumes_when_engaged() {
        let mut c = Controller::new(true, Vec::new(), Vec::new());
        assert_eq!(c.apply_hits(vec![hit(1)]), Action::Unchanged);
        assert_eq!(c.apply_hits(vec![hit(1)]), Action::Pause);
        assert!(c.is_engaged());

        assert_eq!(
            c.reconfigure(true, vec!["factorio".into()], Vec::new()),
            Action::Unchanged
        );
        assert!(c.is_engaged());

        assert_eq!(c.reconfigure(false, Vec::new(), Vec::new()), Action::Resume);
        assert!(!c.is_engaged());
        assert!(c.hits().is_empty());
    }
}
