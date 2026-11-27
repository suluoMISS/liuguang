//! 全屏状态的"兜底确认"：直接问合成器，而不是只信 `zwlr_foreign_toplevel_*`。
//!
//! 为什么需要它（真机复现，niri 26.04）：
//!
//! - 进入全屏时协议会发 `state = [3, …]`（fullscreen），**退出全屏时什么都不发**。
//!   于是守护进程会一直认为"还在打游戏"，壁纸永久停在暂停 —— 比不暂停更糟。
//! - niri 的 IPC（`niri msg --json windows|outputs`）能如实给出几何：全屏窗口的
//!   `layout.window_size` 等于输出的逻辑尺寸，退出后回到布局里的平铺尺寸。
//!
//! 约束与取舍：
//!
//! - 只在 `NIRI_SOCKET` 存在（niri 会话，systemd 用户实例会带上这个变量）且
//!   `niri` 在 PATH 里时才走这条路；其它合成器（sway 等）协议本身是好的，不需要。
//! - 每次探测都有硬超时（`PROBE_TIMEOUT`），拿不到就当作"不知道"，**绝不因为探测
//!   失败而恢复视频**（宁可多暂停，也不要和游戏抢 CPU）。
//! - 只按 `app_id` + `title` 匹配我们记录的那个窗口；匹配不到就认为它已经不再全屏。
//! - 几何比较带 3px 容差：分数缩放下 niri 报 1707.33 而输出逻辑尺寸是 1706。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use liuguang_display::FullscreenToplevel;

/// 单次探测的硬超时。`niri msg` 走 unix socket，正常在毫秒级返回。
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);

/// 几何容差（像素）。分数缩放下对方会给出小数，取整误差 1~2px 属正常。
const SIZE_TOLERANCE: f64 = 3.0;

/// 兜底探测是否可用：niri 会话 + PATH 里有 `niri` 可执行文件。
pub fn available() -> bool {
    std::env::var_os("NIRI_SOCKET").is_some() && which("niri")
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

/// 问一次 niri：`target` 描述的那个窗口现在还是全屏吗。
pub fn probe(target: &FullscreenToplevel) -> Probe {
    let Some(windows) = run_json(&["msg", "--json", "windows"]) else {
        return Probe::Unknown;
    };
    let Some(outputs) = run_json(&["msg", "--json", "outputs"]) else {
        return Probe::Unknown;
    };
    let Some(item) = find_window(&windows, target) else {
        return Probe::WindowGone;
    };
    let size = window_size(&item);
    let Some(cover) = largest_output_logical(&outputs) else {
        return Probe::Unknown;
    };
    match size {
        Some((w, h)) if covers(w, h, cover.0, cover.1) => Probe::StillFullscreen,
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

/// 所有输出里最大的那个逻辑尺寸（多显示器时全屏窗口只会覆盖其中之一）。
fn largest_output_logical(outputs: &serde_json::Value) -> Option<(f64, f64)> {
    let map = outputs.as_object()?;
    let mut best: Option<(f64, f64)> = None;
    for output in map.values() {
        let logical = output.get("logical")?;
        let w = logical.get("width")?.as_f64()?;
        let h = logical.get("height")?.as_f64()?;
        best = match best {
            Some((bw, bh)) if bw * bh >= w * h => Some((bw, bh)),
            _ => Some((w, h)),
        };
    }
    best
}

/// 跑一条 `niri …` 命令并解析 JSON；超时/失败/非 UTF-8 都返回 `None`。
fn run_json(args: &[&str]) -> Option<serde_json::Value> {
    let mut child = Command::new("niri")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) if status.success() => break,
            Some(_) => return None,
            None => {}
        }
        if Instant::now() >= deadline {
            // 探测卡住时不能拖住主循环：杀掉子进程，按"不知道"处理。
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().ok()?;
    serde_json::from_slice(&output.stdout).ok()
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
    fn largest_output_wins_among_multiple_displays() {
        let outputs = serde_json::json!({
            "eDP-1": {"logical": {"width": 1706.0, "height": 1066.0}},
            "HDMI-A-1": {"logical": {"width": 2560.0, "height": 1440.0}}
        });
        assert_eq!(largest_output_logical(&outputs), Some((2560.0, 1440.0)));
    }

    #[test]
    fn missing_fields_are_unknown_not_fuller_screen() {
        assert_eq!(window_size(&serde_json::json!({"id": 1})), None);
        assert_eq!(largest_output_logical(&serde_json::json!({})), None);
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
}
