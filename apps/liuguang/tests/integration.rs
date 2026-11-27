//! M2–M6 端到端集成测试：真实 daemon 子进程（mock 显示后端）+ 真实 socket。
//!
//! 全程无硬件依赖，可在 CI/开发容器运行；真机显示验收见
//! docs/HARDWARE_CHECK.md（阻塞 B1）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use liuguang_ipc::{IpcClient, Method, NdjsonDecoder, Request, SetWallpaperParams, encode_message};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    socket: PathBuf,
    state_file: PathBuf,
    media_dir: PathBuf,
    dir: PathBuf,
}

impl Daemon {
    fn spawn(name: &str, config_toml: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("liuguang-it-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("media")).unwrap();
        let media_dir = dir.join("media");
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "{config_toml}\n[media]\nallowed_roots = [\"{}\"]\n\n[slideshow]\ndirectory = \"{}\"\ninterval_seconds = 3600\n",
                media_dir.display(),
                media_dir.display()
            ),
        )
        .unwrap();
        let socket = dir.join("control.sock");
        let state_file = dir.join("state.json");
        let child = Command::new(env!("CARGO_BIN_EXE_liuguang"))
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "--socket",
                socket.to_str().unwrap(),
                "--state-file",
                state_file.to_str().unwrap(),
                "--backend",
                "mock",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self {
            child,
            socket,
            state_file,
            media_dir,
            dir,
        };
        daemon.wait_ready();
        daemon
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if IpcClient::connect(&self.socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon 未在 10s 内就绪");
    }

    fn client(&self) -> IpcClient {
        IpcClient::connect(&self.socket).unwrap()
    }

    fn write_png(&self, name: &str) -> PathBuf {
        let path = self.media_dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ])
            .unwrap();
        path
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// 杀掉进程但保留目录（重启测试用）。
    fn kill_keep_dir(mut self) -> (PathBuf, PathBuf, PathBuf) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        (
            self.dir.clone(),
            self.socket.clone(),
            self.state_file.clone(),
        )
    }
}

fn status_value(client: &mut IpcClient) -> serde_json::Value {
    let resp = client
        .request(Method::Status, serde_json::json!({}))
        .unwrap();
    assert!(resp.ok, "{:?}", resp.error);
    resp.result.unwrap()
}

/// 按输出名取通道状态（BTreeMap 顺序按名称，勿用下标）。
fn channel<'a>(st: &'a serde_json::Value, output: &str) -> &'a serde_json::Value {
    st["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["output"] == output)
        .unwrap_or_else(|| panic!("找不到输出 {output} 的通道: {st}"))
}

#[test]
fn m3_set_next_pause_resume_transition_reload() {
    let d = Daemon::spawn("m3", "");
    d.write_png("a.png");
    d.write_png("b.png");
    let mut c = d.client();

    // outputs：mock 双输出（M6）
    let resp = c.request(Method::Outputs, serde_json::json!({})).unwrap();
    let outs = resp.result.unwrap();
    assert_eq!(outs["outputs"][0]["name"], "HDMI-A-1");
    assert_eq!(outs["outputs"][1]["name"], "DP-1");

    // set（显式输出）
    let p = d.media_dir.join("a.png");
    let resp = c
        .request(
            Method::SetWallpaper,
            serde_json::to_value(SetWallpaperParams {
                path: p.display().to_string(),
                output: Some("HDMI-A-1".into()),
                fit: Some("cover".into()),
                zoom: None,
                offset_x: None,
                offset_y: None,
                rotation: None,
                brightness: None,
                pin: None,
            })
            .unwrap(),
        )
        .unwrap();
    assert!(resp.ok, "{:?}", resp.error);
    let st = status_value(&mut c);
    let ch = channel(&st, "HDMI-A-1").clone();
    assert_eq!(ch["state"], "StaticIdle");
    assert!(ch["media"].as_str().unwrap().ends_with("a.png"));

    // next（M3 轮换）
    let resp = c
        .request(Method::Next, serde_json::json!({"output": "HDMI-A-1"}))
        .unwrap();
    assert!(resp.ok, "{:?}", resp.error);
    let st = status_value(&mut c);
    assert!(
        channel(&st, "HDMI-A-1")["media"]
            .as_str()
            .unwrap()
            .ends_with("b.png")
    );

    // pause / resume
    let resp = c
        .request(Method::Pause, serde_json::json!({"output": "HDMI-A-1"}))
        .unwrap();
    assert!(resp.ok);
    assert_eq!(channel(&status_value(&mut c), "HDMI-A-1")["paused"], true);
    let resp = c
        .request(Method::Resume, serde_json::json!({"output": "HDMI-A-1"}))
        .unwrap();
    assert!(resp.ok);
    assert_eq!(channel(&status_value(&mut c), "HDMI-A-1")["paused"], false);

    // transition（M3 记录）
    let resp = c
        .request(
            Method::Transition,
            serde_json::json!({"effect": "slide", "duration_ms": 400, "easing": "linear", "direction": "left"}),
        )
        .unwrap();
    assert!(resp.ok, "{:?}", resp.error);

    // reload
    let resp = c.request(Method::Reload, serde_json::json!({})).unwrap();
    assert!(resp.ok, "{:?}", resp.error);

    d.stop();
}

#[test]
fn m6_channels_are_independent() {
    let d = Daemon::spawn("m6", "");
    let a = d.write_png("a.png");
    let b = d.write_png("b.png");
    let mut c = d.client();

    for (out, media) in [("HDMI-A-1", &a), ("DP-1", &b)] {
        let resp = c
            .request(
                Method::SetWallpaper,
                serde_json::json!({"path": media.display().to_string(), "output": out}),
            )
            .unwrap();
        assert!(resp.ok, "{out}: {:?}", resp.error);
    }
    let st = status_value(&mut c);
    assert_eq!(st["channels"].as_array().unwrap().len(), 2);
    assert!(
        channel(&st, "HDMI-A-1")["media"]
            .as_str()
            .unwrap()
            .ends_with("a.png")
    );
    assert!(
        channel(&st, "DP-1")["media"]
            .as_str()
            .unwrap()
            .ends_with("b.png")
    );

    // 一个通道 pause 不影响另一个。
    let resp = c
        .request(Method::Pause, serde_json::json!({"output": "DP-1"}))
        .unwrap();
    assert!(resp.ok);
    let st = status_value(&mut c);
    assert_eq!(channel(&st, "HDMI-A-1")["paused"], false);
    assert_eq!(channel(&st, "DP-1")["paused"], true);
    d.stop();
}

#[test]
fn m3_subscribe_streams_events() {
    let d = Daemon::spawn("sub", "");
    d.write_png("a.png");

    // 订阅连接：先等待确认帧（消除订阅注册与事件发布的竞态）。
    let mut stream = UnixStream::connect(&d.socket).unwrap();
    let sub = Request::new("sub-1", Method::Subscribe, serde_json::json!({}));
    stream.write_all(&encode_message(&sub).unwrap()).unwrap();
    let mut decoder = NdjsonDecoder::new();
    let mut buf = [0u8; 4096];
    let mut frames = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && frames.is_empty() {
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decoder.feed(&buf[..n]).unwrap();
                while let Some(v) = decoder.next_message::<serde_json::Value>().unwrap() {
                    frames.push(v);
                }
            }
            Err(_) => {}
        }
    }
    assert_eq!(frames[0]["ok"], true, "订阅确认帧");

    // 另一连接触发媒体切换。
    let mut c = d.client();
    let p = d.media_dir.join("a.png");
    let resp = c
        .request(
            Method::SetWallpaper,
            serde_json::json!({"path": p.display().to_string(), "output": "HDMI-A-1"}),
        )
        .unwrap();
    assert!(resp.ok, "{:?}", resp.error);

    // 读取订阅帧：至少一条 MEDIA_CHANGED。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && frames.len() < 3 {
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                decoder.feed(&buf[..n]).unwrap();
                while let Some(v) = decoder.next_message::<serde_json::Value>().unwrap() {
                    frames.push(v);
                }
            }
            Err(_) => {}
        }
    }
    let kinds: Vec<&str> = frames
        .iter()
        .skip(1)
        .filter_map(|f| f["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"MEDIA_CHANGED"),
        "订阅流应含 MEDIA_CHANGED: {frames:?}"
    );
    d.stop();
}

#[test]
fn security_path_checks() {
    let d = Daemon::spawn("sec", "");
    let mut c = d.client();
    // 相对路径拒绝。
    let resp = c
        .request(
            Method::SetWallpaper,
            serde_json::json!({"path": "relative.png"}),
        )
        .unwrap();
    assert!(!resp.ok);
    assert_eq!(
        resp.error.unwrap().code,
        liuguang_ipc::ErrorKind::PermissionDenied
    );
    // 白名单外拒绝。
    let outside = std::env::temp_dir().join("liuguang-it-outside.png");
    let file = std::fs::File::create(&outside).unwrap();
    let mut encoder = png::Encoder::new(file, 1, 1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&[0, 0, 0, 255])
        .unwrap();
    let resp = c
        .request(
            Method::SetWallpaper,
            serde_json::json!({"path": outside.display().to_string()}),
        )
        .unwrap();
    assert!(!resp.ok);
    assert_eq!(
        resp.error.unwrap().code,
        liuguang_ipc::ErrorKind::PermissionDenied
    );
    // 不存在的输出拒绝。
    let p = d.write_png("a.png");
    let resp = c
        .request(
            Method::SetWallpaper,
            serde_json::json!({"path": p.display().to_string(), "output": "VGA-9"}),
        )
        .unwrap();
    assert!(!resp.ok);
    assert_eq!(
        resp.error.unwrap().code,
        liuguang_ipc::ErrorKind::UnknownOutput
    );
    d.stop();
}

#[test]
fn headless_when_device_unavailable() {
    // 强制不可用设备：无 /dev/dri 与有 /dev/dri 的机器行为一致。
    let dir = std::env::temp_dir().join(format!("liuguang-it-headless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("media")).unwrap();
    std::fs::write(dir.join("config.toml"), b"").unwrap();
    let socket = dir.join("control.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_liuguang"))
        .args([
            "--config",
            dir.join("config.toml").to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--state-file",
            dir.join("state.json").to_str().unwrap(),
            "--device",
            "/dev/dri/card-does-not-exist",
            // 显式 drm：本测试断言的是"DRM 设备不可用 ⇒ HeadlessReady"，
            // 不能让它跟着开发机的 WAYLAND_DISPLAY 漂到 wayland 后端。
            "--backend",
            "drm",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if IpcClient::connect(&socket).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut c = IpcClient::connect(&socket).unwrap();
    let st = status_value(&mut c);
    assert_eq!(st["state"], "HeadlessReady");
    let p = dir.join("media").join("x.png");
    let file = std::fs::File::create(&p).unwrap();
    let mut encoder = png::Encoder::new(file, 1, 1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&[0, 0, 0, 255])
        .unwrap();
    let resp = c
        .request(
            Method::SetWallpaper,
            serde_json::json!({"path": p.display().to_string()}),
        )
        .unwrap();
    assert!(!resp.ok);
    assert_eq!(
        resp.error.unwrap().code,
        liuguang_ipc::ErrorKind::DeviceUnavailable
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn m2_state_persistence_across_restart() {
    let d = Daemon::spawn("persist", "");
    let p = d.write_png("keep.png");
    let mut c = d.client();
    let resp = c
        .request(
            Method::SetWallpaper,
            serde_json::json!({"path": p.display().to_string(), "output": "HDMI-A-1"}),
        )
        .unwrap();
    assert!(resp.ok);
    assert!(d.state_file.exists());
    drop(c);
    // 模拟重启：杀掉再用同一 state/socket 目录拉起。
    let (dir, socket, state_file) = d.kill_keep_dir();
    std::thread::sleep(Duration::from_millis(100));

    let mut child = Command::new(env!("CARGO_BIN_EXE_liuguang"))
        .args([
            "--config",
            dir.join("config.toml").to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--state-file",
            state_file.to_str().unwrap(),
            "--backend",
            "mock",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if IpcClient::connect(&socket).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut c = IpcClient::connect(&socket).unwrap();
    let st = status_value(&mut c);
    assert!(
        channel(&st, "HDMI-A-1")["media"]
            .as_str()
            .unwrap()
            .ends_with("keep.png"),
        "重启应恢复持久化媒体: {st}"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
