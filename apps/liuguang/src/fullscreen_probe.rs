//! 全屏状态的"兜底确认"：直接问合成器（支持 Niri / Hyprland / Sway），而不是只信 `zwlr_foreign_toplevel_*`。
//!
//! 为什么需要它：部分合成器在窗口退出全屏时不一定会可靠撤回 `state = [3]`。
//! 后台线程异步询问合成器原生 IPC，主循环永远不会被子进程阻塞；每次探测带硬超时与字节上限。

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use liuguang_display::FullscreenToplevel;

/// 单次探测的硬超时（走本地 unix socket）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);

/// 单次 IPC JSON 响应读取上限（2 MiB，防止畸形/超大输出拖垮内存）。
const MAX_JSON_BYTES: u64 = 2 * 1024 * 1024;

/// 几何容差（像素）。分数缩放下合成器可能给出小数，取整误差 1~2px 属正常。
const SIZE_TOLERANCE: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendIpc {
    Niri,
    Hyprland,
    Sway,
}

fn detect_backend() -> Option<BackendIpc> {
    if std::env::var_os("NIRI_SOCKET").is_some() && which("niri") {
        Some(BackendIpc::Niri)
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() && which("hyprctl") {
        Some(BackendIpc::Hyprland)
    } else if std::env::var_os("SWAYSOCK").is_some() && which("swaymsg") {
        Some(BackendIpc::Sway)
    } else {
        None
    }
}

/// 兜底探测是否可用：当前会话有可用的合成器 IPC。
pub fn available() -> bool {
    detect_backend().is_some()
}

/// 极简 `which`：只看 PATH 里有没有可执行文件，不调用外部命令。
fn which(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file()
    })
}

/// 探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// 那个窗口仍然全屏（或覆盖整个输出）。
    StillFullscreen,
    /// 窗口还在，但已经不覆盖整个输出了。
    Exited,
    /// 窗口已经不在窗口列表里了。
    WindowGone,
    /// 探测不可用/超时/输出无法解析：调用方应当保持现状。
    Unknown,
}

#[derive(Debug)]
struct AsyncProbeState {
    target_key: String,
    in_flight: bool,
    ready: Option<(Instant, Probe)>,
}

fn async_probe_slot() -> &'static Mutex<AsyncProbeState> {
    static SLOT: OnceLock<Mutex<AsyncProbeState>> = OnceLock::new();
    SLOT.get_or_init(|| {
        Mutex::new(AsyncProbeState {
            target_key: String::new(),
            in_flight: false,
            ready: None,
        })
    })
}

/// 非阻塞探测：首次触发时在后台线程运行合成器 IPC，主循环立刻收到 `Probe::Unknown`；
/// 后台完成后结果缓存 2 秒供下一次 tick 读取（绝不阻塞主循环）。
pub fn probe_nonblocking(target: &FullscreenToplevel) -> Probe {
    let Some(backend) = detect_backend() else {
        return Probe::Unknown;
    };
    let key = format!(
        "{:?}|{:?}|{:?}",
        target.app_id, target.title, target.outputs
    );
    let now = Instant::now();
    let Ok(mut state) = async_probe_slot().lock() else {
        return Probe::Unknown;
    };
    if state.target_key != key {
        state.target_key = key.clone();
        state.ready = None;
    }
    if let Some((at, result)) = state.ready.take() {
        if now.saturating_duration_since(at) <= Duration::from_secs(2) {
            return result;
        }
    }
    if !state.in_flight {
        state.in_flight = true;
        let target_owned = target.clone();
        std::thread::spawn(move || {
            let result = probe_with_backend(backend, &target_owned);
            if let Ok(mut guard) = async_probe_slot().lock() {
                if guard.target_key == key {
                    guard.ready = Some((Instant::now(), result));
                }
                guard.in_flight = false;
            }
        });
    }
    Probe::Unknown
}

/// 同步探测入口（主要供单测或直接调用）。
#[allow(dead_code)]
pub fn probe(target: &FullscreenToplevel) -> Probe {
    let Some(backend) = detect_backend() else {
        return Probe::Unknown;
    };
    probe_with_backend(backend, target)
}

fn probe_with_backend(backend: BackendIpc, target: &FullscreenToplevel) -> Probe {
    match backend {
        BackendIpc::Niri => probe_niri(target),
        BackendIpc::Hyprland => probe_hyprland(target),
        BackendIpc::Sway => probe_sway(target),
    }
}

fn probe_niri(target: &FullscreenToplevel) -> Probe {
    let Some(windows) = run_json("niri", &["msg", "--json", "windows"]) else {
        return Probe::Unknown;
    };
    let Some(outputs) = run_json("niri", &["msg", "--json", "outputs"]) else {
        return Probe::Unknown;
    };
    let workspaces = run_json("niri", &["msg", "--json", "workspaces"]);
    evaluate_niri(&windows, &outputs, workspaces.as_ref(), target)
}

fn evaluate_niri(
    windows: &serde_json::Value,
    outputs: &serde_json::Value,
    workspaces: Option<&serde_json::Value>,
    target: &FullscreenToplevel,
) -> Probe {
    let Some(item) = find_window(windows, target) else {
        return Probe::WindowGone;
    };
    if item.get("is_fullscreen").and_then(|v| v.as_bool()) == Some(false) {
        if let Some((w, h)) = window_size(&item) {
            let all = enabled_output_logicals(outputs);
            if !all.is_empty() && !all.iter().any(|&(ow, oh)| covers(w, h, ow, oh)) {
                return Probe::Exited;
            }
        } else {
            return Probe::Exited;
        }
    }
    let Some((w, h)) = window_size(&item) else {
        return Probe::Unknown;
    };
    // 优先通过 workspace_id -> workspace.output 定位精确输出尺寸；失败时回退到全部已启用输出。
    let ws_output_name = item
        .get("workspace_id")
        .and_then(|v| v.as_i64())
        .and_then(|ws_id| {
            workspaces?.as_array()?.iter().find_map(|ws| {
                (ws.get("id").and_then(|v| v.as_i64()) == Some(ws_id))
                    .then(|| {
                        ws.get("output")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .flatten()
            })
        });
    if let Some(out_name) = ws_output_name {
        if let Some((ow, oh)) = output_logical_by_name(outputs, &out_name) {
            return if covers(w, h, ow, oh) {
                Probe::StillFullscreen
            } else {
                Probe::Exited
            };
        }
    }
    let logical_sizes = enabled_output_logicals(outputs);
    if logical_sizes.is_empty() {
        return Probe::Unknown;
    }
    if logical_sizes.iter().any(|&(ow, oh)| covers(w, h, ow, oh)) {
        Probe::StillFullscreen
    } else {
        Probe::Exited
    }
}

fn output_logical_by_name(outputs: &serde_json::Value, name: &str) -> Option<(f64, f64)> {
    let logical = outputs
        .as_object()?
        .get(name)?
        .get("logical")?
        .as_object()?;
    let w = logical.get("width")?.as_f64()?;
    let h = logical.get("height")?.as_f64()?;
    Some((w, h))
}

fn probe_hyprland(target: &FullscreenToplevel) -> Probe {
    let Some(clients) = run_json("hyprctl", &["-j", "clients"]) else {
        return Probe::Unknown;
    };
    let monitors = run_json("hyprctl", &["-j", "monitors"]);
    evaluate_hyprland(&clients, monitors.as_ref(), target)
}

fn evaluate_hyprland(
    clients: &serde_json::Value,
    monitors: Option<&serde_json::Value>,
    target: &FullscreenToplevel,
) -> Probe {
    let Some(list) = clients.as_array() else {
        return Probe::Unknown;
    };
    let want_app = target.app_id.as_deref();
    let want_title = target.title.as_deref();
    let hit = list.iter().find(|c| {
        let class = c
            .get("class")
            .and_then(|v| v.as_str())
            .or_else(|| c.get("initialClass").and_then(|v| v.as_str()));
        if class != want_app {
            return false;
        }
        match want_title {
            Some(t) => c.get("title").and_then(|v| v.as_str()) == Some(t),
            None => true,
        }
    });
    let Some(client) = hit else {
        return Probe::WindowGone;
    };
    if let Some(fs) = client.get("fullscreen").and_then(|v| v.as_i64()) {
        return if fs > 0 {
            Probe::StillFullscreen
        } else {
            Probe::Exited
        };
    }
    if let (Some(sz), Some(mons)) = (
        client.get("size").and_then(|v| v.as_array()),
        monitors.and_then(|m| m.as_array()),
    ) {
        if let (Some(w), Some(h)) = (
            sz.first().and_then(|v| v.as_f64()),
            sz.get(1).and_then(|v| v.as_f64()),
        ) {
            for mon in mons {
                let scale = mon
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0);
                if let (Some(mw), Some(mh)) = (
                    mon.get("width").and_then(|v| v.as_f64()),
                    mon.get("height").and_then(|v| v.as_f64()),
                ) {
                    if covers(w, h, mw / scale, mh / scale) || covers(w, h, mw, mh) {
                        return Probe::StillFullscreen;
                    }
                }
            }
            return Probe::Exited;
        }
    }
    Probe::Unknown
}

fn probe_sway(target: &FullscreenToplevel) -> Probe {
    let Some(tree) = run_json("swaymsg", &["-t", "get_tree"]) else {
        return Probe::Unknown;
    };
    evaluate_sway(&tree, target)
}

fn evaluate_sway(tree: &serde_json::Value, target: &FullscreenToplevel) -> Probe {
    fn find_node<'a>(
        node: &'a serde_json::Value,
        want_app: Option<&str>,
        want_title: Option<&str>,
    ) -> Option<&'a serde_json::Value> {
        let node_type = node.get("type").and_then(|v| v.as_str());
        if matches!(node_type, Some("con" | "floating_con"))
            && node.get("pid").and_then(|v| v.as_i64()).is_some()
        {
            let app = node.get("app_id").and_then(|v| v.as_str()).or_else(|| {
                node.get("window_properties")
                    .and_then(|wp| wp.get("class"))
                    .and_then(|v| v.as_str())
            });
            let title_ok = match want_title {
                Some(t) => node.get("name").and_then(|v| v.as_str()) == Some(t),
                None => true,
            };
            if app == want_app && title_ok {
                return Some(node);
            }
        }
        for key in ["nodes", "floating_nodes"] {
            if let Some(children) = node.get(key).and_then(|v| v.as_array()) {
                for child in children {
                    if let Some(hit) = find_node(child, want_app, want_title) {
                        return Some(hit);
                    }
                }
            }
        }
        None
    }

    let Some(hit) = find_node(tree, target.app_id.as_deref(), target.title.as_deref()) else {
        return Probe::WindowGone;
    };
    match hit.get("fullscreen_mode").and_then(|v| v.as_i64()) {
        Some(mode) if mode > 0 => Probe::StillFullscreen,
        Some(_) => Probe::Exited,
        None => Probe::Unknown,
    }
}

/// 纯函数：窗口尺寸是否覆盖整个输出（带容差）。
pub fn covers(w: f64, h: f64, out_w: f64, out_h: f64) -> bool {
    (w - out_w).abs() <= SIZE_TOLERANCE && (h - out_h).abs() <= SIZE_TOLERANCE
}

/// 按 `app_id` + `title` 找窗口。`title` 可能为空（有些客户端不报），此时只按 app_id 匹配。
fn find_window(
    windows: &serde_json::Value,
    target: &FullscreenToplevel,
) -> Option<serde_json::Value> {
    let list = windows.as_array()?;
    let want_app = target.app_id.as_deref();
    let want_title = target.title.as_deref();
    list.iter()
        .find(|item| {
            let app = item.get("app_id").and_then(|v| v.as_str());
            if app != want_app {
                return false;
            }
            match want_title {
                Some(title) => item.get("title").and_then(|v| v.as_str()) == Some(title),
                None => true,
            }
        })
        .cloned()
}

/// 从一条窗口记录里取 `layout.window_size`（niri 报的是逻辑像素，可能是小数）。
fn window_size(item: &serde_json::Value) -> Option<(f64, f64)> {
    let arr = item.get("layout")?.get("window_size")?.as_array()?;
    let w = arr.first()?.as_f64()?;
    let h = arr.get(1)?.as_f64()?;
    Some((w, h))
}

/// 收集所有已启用输出的逻辑尺寸（跳过 `logical: null` 的禁用输出）。
fn enabled_output_logicals(outputs: &serde_json::Value) -> Vec<(f64, f64)> {
    let Some(map) = outputs.as_object() else {
        return Vec::new();
    };
    let mut list = Vec::new();
    for output in map.values() {
        let Some(logical) = output.get("logical").and_then(|v| v.as_object()) else {
            continue;
        };
        let (Some(w), Some(h)) = (
            logical.get("width").and_then(|v| v.as_f64()),
            logical.get("height").and_then(|v| v.as_f64()),
        ) else {
            continue;
        };
        list.push((w, h));
    }
    list
}

/// 跑一条合成器 IPC 命令并解析 JSON（带字节上限与硬超时）；超时/失败/非 UTF-8 都返回 `None`。
///
/// 通过辅助线程排空 stdout 避免 64KiB 管道缓冲区写满导致子进程阻塞。
fn run_json(bin: &str, args: &[&str]) -> Option<serde_json::Value> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(MAX_JSON_BYTES).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => {
                let _ = reader.join();
                return None;
            }
            None => {}
        }
        if Instant::now() >= deadline {
            // 探测卡住时不能拖住主循环：杀掉子进程，按"不知道"处理。
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let bytes = reader.join().ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn target(app: &str, title: Option<&str>) -> FullscreenToplevel {
        FullscreenToplevel {
            app_id: Some(app.to_string()),
            title: title.map(str::to_string),
            outputs: Vec::new(),
        }
    }

    #[test]
    fn geometry_tolerance_accepts_rounding_but_not_gaps() {
        // 分数缩放：niri 报 1707.33，输出逻辑尺寸 1706。
        assert!(covers(1707.333, 1067.333, 1706.0, 1066.0));
        // 最大化的窗口会留出间隙（1707 - 1698 = 9px），不能被当成全屏。
        assert!(!covers(1698.0, 1058.0, 1706.0, 1066.0));
    }

    #[test]
    fn probe_finds_window_by_app_and_title() {
        let windows = serde_json::json!([
            {"id": 1, "app_id": "chromium", "title": "x", "layout": {"window_size": [100.0, 100.0]}},
            {"id": 2, "app_id": "steam", "title": "Elden Ring", "layout": {"window_size": [1707.0, 1067.0]}}
        ]);
        let hit = find_window(&windows, &target("steam", Some("Elden Ring"))).expect("命中");
        assert_eq!(hit.get("id").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(window_size(&hit), Some((1707.0, 1067.0)));
    }

    #[test]
    fn probe_matches_app_only_when_title_is_missing() {
        let windows = serde_json::json!([
            {"id": 7, "app_id": "mpv", "title": "任意标题", "layout": {"window_size": [10.0, 10.0]}}
        ]);
        assert!(find_window(&windows, &target("mpv", None)).is_some());
        assert!(find_window(&windows, &target("mpv", Some("别的标题"))).is_none());
    }

    #[test]
    fn enabled_outputs_collect_all_active_displays_and_skip_disabled() {
        let outputs = serde_json::json!({
            "eDP-1": {"logical": {"width": 1706.0, "height": 1066.0}},
            "HDMI-A-1": {"logical": {"width": 2560.0, "height": 1440.0}},
            "DP-DISABLED": {"logical": null}
        });
        let got = enabled_output_logicals(&outputs);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got.contains(&(1706.0, 1066.0)));
        assert!(got.contains(&(2560.0, 1440.0)));
    }

    #[test]
    fn missing_fields_are_unknown_not_fuller_screen() {
        assert_eq!(window_size(&serde_json::json!({"id": 1})), None);
        assert!(enabled_output_logicals(&serde_json::json!({})).is_empty());
        assert_eq!(
            find_window(&serde_json::json!({"not": "a list"}), &target("x", None)),
            None
        );
        assert!(!matches!(Probe::Unknown, Probe::Exited | Probe::WindowGone));
        assert!(matches!(Probe::Exited, Probe::Exited | Probe::WindowGone));
        assert!(matches!(
            Probe::WindowGone,
            Probe::Exited | Probe::WindowGone
        ));
    }

    #[test]
    fn niri_workspace_output_match_works() {
        let windows = serde_json::json!([
            {"id": 1, "app_id": "mpv", "title": "v", "workspace_id": 10, "is_fullscreen": true, "layout": {"window_size": [1920.0, 1080.0]}}
        ]);
        let outputs = serde_json::json!({
            "eDP-1": {"logical": {"width": 2560.0, "height": 1600.0}},
            "HDMI-A-1": {"logical": {"width": 1920.0, "height": 1080.0}}
        });
        let workspaces = serde_json::json!([
            {"id": 10, "output": "eDP-1"}
        ]);
        // 在 eDP-1 (2560x1600) 上只占 1920x1080 -> 已退出全屏，不会误匹配 HDMI-A-1。
        assert_eq!(
            evaluate_niri(
                &windows,
                &outputs,
                Some(&workspaces),
                &target("mpv", Some("v"))
            ),
            Probe::Exited
        );
    }

    #[test]
    fn hyprland_and_sway_evaluators_work() {
        let hypr = serde_json::json!([
            {"class": "steam_app_1", "title": "Game", "fullscreen": 1}
        ]);
        assert_eq!(
            evaluate_hyprland(&hypr, None, &target("steam_app_1", Some("Game"))),
            Probe::StillFullscreen
        );
        let sway = serde_json::json!({
            "type": "root",
            "nodes": [{
                "type": "con",
                "pid": 123,
                "app_id": "mpv",
                "name": "Movie",
                "fullscreen_mode": 0
            }]
        });
        assert_eq!(
            evaluate_sway(&sway, &target("mpv", Some("Movie"))),
            Probe::Exited
        );
    }
}
