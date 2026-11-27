#!/usr/bin/env python3
"""全屏应用时暂停视频的端到端检查（0.2.0-6）。

做的事：
  1. 在临时目录里起一个**独立**的 `liuguang`（自己的 socket/state/config，
     媒体目录指向临时目录），不碰你正在用的服务；
  2. `set` 一段视频，等它进入 `VideoPlaying`；
  3. 用 `liuguang-gui` 开一个窗口，交给 niri 聚焦 + 全屏；
  4. 轮询 `status`：必须出现 `fullscreen`（窗口名）+ 通道 `paused=true`
     + `pause_reason="fullscreen"`；
  5. 退出全屏，再轮询：`fullscreen` 变 null、`paused` 变 false（只恢复它自己停的）。

依赖：
  * `WAYLAND_DISPLAY` 与 `niri`（`zwlr_foreign_toplevel_management` 由合成器提供）；
  * `liuguang` / `liuguang-gui` 在 PATH，或通过 `--liuguang` / `--liuguang-gui` 指定；
  * 一段测试视频（默认取仓库 `.audit/wall-1080p-30s.mp4`，可用 `--video` 换）。

用法：
  python3 scripts/e2e/fullscreen_pause_check.py
  python3 scripts/e2e/fullscreen_pause_check.py --keep      # 保留临时目录与进程现场

退出码：0 = 通过或跳过（没有合成器/没有素材），1 = 检查失败。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

SETTLE = 6.0  # 全屏判定的滞回是 1s，采样 500ms；留 6s 余量
VIDEO_WAIT = 12.0


class Ipc:
    """最小的 NDJSON 客户端（只用于自检，不追求通用性）。"""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.sock: socket.socket | None = None
        self.buf = b""
        self.next_id = 0

    def connect(self, timeout: float = 10.0) -> None:
        deadline = time.monotonic() + timeout
        last: Exception | None = None
        while time.monotonic() < deadline:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.settimeout(5.0)
                s.connect(str(self.path))
                self.sock = s
                return
            except OSError as exc:  # 守护进程还没起来
                last = exc
                time.sleep(0.1)
        raise SystemExit(f"连不上 {self.path}: {last}")

    def call(self, method: str, params: dict | None = None) -> dict:
        assert self.sock is not None
        self.next_id += 1
        req = {
            "v": 1,
            "id": f"chk-{self.next_id}",
            "method": method,
            "params": params or {},
        }
        self.sock.sendall(json.dumps(req).encode() + b"\n")
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise SystemExit("守护进程关闭了连接")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        resp = json.loads(line)
        if not resp.get("ok"):
            raise SystemExit(f"{method} 失败: {json.dumps(resp.get('error'), ensure_ascii=False)}")
        return resp.get("result") or {}

    def close(self) -> None:
        if self.sock is not None:
            self.sock.close()
            self.sock = None


def wait_for(predicate, timeout: float, interval: float = 0.25, what: str = ""):
    """轮询直到 predicate 返回真值；超时返回 None。"""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = predicate()
        if last:
            return last
        time.sleep(interval)
    print(f"  ✗ 超时（{timeout:.1f}s）：{what}，最后一次观测 = {last!r}")
    return None


def niri(*args: str) -> str:
    return subprocess.run(
        ["niri", "msg", *args], capture_output=True, text=True, timeout=10
    ).stdout


def toggle_fullscreen(win: int) -> None:
    """按窗口 id 切全屏。

    `niri msg action fullscreen-window` 只作用于**焦点窗口**（不支持 --id），
    所以每次都要先聚焦。第一版漏了这一步：退出全屏时焦点已被别的窗口拿走，
    于是"退出全屏"实际全屏了另一个窗口，恢复断言永远等不到。
    """
    niri("action", "focus-window", "--id", str(win))
    time.sleep(0.25)
    niri("action", "fullscreen-window")


def find_window(app_id: str, title: str, deadline: float = 10.0) -> int | None:
    """在 niri 的窗口列表里找我们刚开的那个窗口。"""
    end = time.monotonic() + deadline
    while time.monotonic() < end:
        try:
            raw = subprocess.run(
                ["niri", "msg", "--json", "windows"],
                capture_output=True,
                text=True,
                timeout=10,
            ).stdout
            for win in json.loads(raw or "[]"):
                if win.get("app_id") == app_id or title in (win.get("title") or ""):
                    return int(win["id"])
        except (json.JSONDecodeError, KeyError, ValueError):
            pass
        time.sleep(0.25)
    return None


def first_channel(status: dict) -> dict:
    channels = status.get("channels") or []
    return channels[0] if channels else {}


def main() -> int:
    ap = argparse.ArgumentParser(description="全屏应用时暂停视频的端到端检查")
    ap.add_argument("--video", default=str(REPO / ".audit" / "wall-1080p-30s.mp4"))
    ap.add_argument("--liuguang", default=os.environ.get("LIUGUANG", "liuguang"))
    ap.add_argument("--liuguang-gui", default=os.environ.get("LIUGUANG_GUI", "liuguang-gui"))
    ap.add_argument("--keep", action="store_true", help="保留临时目录与进程现场")
    args = ap.parse_args()

    if not os.environ.get("WAYLAND_DISPLAY"):
        print("SKIP: 没有 WAYLAND_DISPLAY（需要在一个 Wayland 会话里跑）")
        return 0
    if shutil.which("niri") is None:
        print("SKIP: 没有 niri（本检查靠 zwlr_foreign_toplevel_management 判定全屏）")
        return 0
    video = Path(args.video)
    if not video.exists():
        print(f"SKIP: 没有测试视频 {video}（用 --video 指定）")
        return 0
    if shutil.which(args.liuguang) is None and not Path(args.liuguang).exists():
        print(f"SKIP: 找不到 {args.liuguang}")
        return 0
    gui = Path(args.liuguang_gui)
    if shutil.which(args.liuguang_gui) is None and not gui.exists():
        print(f"SKIP: 找不到 {args.liuguang_gui}（没有 GUI 也可以手工验证，这里需要它当全屏窗口）")
        return 0

    tmp = Path(tempfile.mkdtemp(prefix="liuguang-fullscreen-"))
    media = tmp / "media"
    media.mkdir()
    # 复制而不是软链：媒体守卫按真实路径判定，软链会落到 .audit 之外。
    target = media / video.name
    shutil.copy2(video, target)
    cfg = tmp / "config.toml"
    cfg.write_text(
        f"""[display]
backend = "wayland"

[media]
allowed_roots = ["{media}"]

[slideshow]
directory = "{media}"
interval_seconds = 3600

[library]
root = "{media}"
image_dirs = ["{media}"]
video_dirs = ["{media}"]
recursive = true
""",
        encoding="utf-8",
    )
    sock_path = tmp / "control.sock"

    env = dict(os.environ)
    daemon = subprocess.Popen(
        [
            args.liuguang,
            "--config",
            str(cfg),
            "--socket",
            str(sock_path),
            "--state-file",
            str(tmp / "state.json"),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    gui_proc: subprocess.Popen | None = None
    ok = False
    try:
        ipc = Ipc(sock_path)
        ipc.connect()
        print(f"✓ 沙箱守护进程已启动（{tmp}）")

        # 先看有没有**别人的**全屏窗口：本检查只能验证"我们自己制造的全屏"，
        # 用户正在全屏看视频/打游戏时任何结论都不成立（也不该去关人家的窗口）。
        before = wait_for(
            lambda: (lambda s: s if s.get("state") else None)(ipc.call("status")),
            5.0,
            what="等待守护进程报告状态",
        )
        if before and before.get("fullscreen"):
            fs = before["fullscreen"]
            who = fs.get("app_id") or fs.get("title") or "某个窗口"
            print(f"SKIP: 已有窗口处于全屏（{who}），无法隔离验证；退出全屏后重跑")
            return 0

        ipc.call("set_wallpaper", {"path": str(target), "pin": True})
        status = wait_for(
            lambda: (lambda s: s if s.get("state") == "VideoPlaying" else None)(
                ipc.call("status")
            ),
            VIDEO_WAIT,
            what="等待 VideoPlaying",
        )
        if not status:
            return 1
        print(f"✓ 视频正在播放（输出 {status.get('output')}）")

        gui_proc = subprocess.Popen(
            [args.liuguang_gui, "--socket", str(sock_path)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        win = find_window("io.github.suluoMISS.Liuguang", "流光")
        if win is None:
            print("✗ 没找到 GUI 窗口（无法制造全屏窗口）")
            return 1
        toggle_fullscreen(win)
        print(f"✓ 已把窗口 {win} 切到全屏")

        # 滞回 1s + 采样 500ms：给 6s
        hit = wait_for(
            lambda: (lambda s: s if s.get("fullscreen") else None)(ipc.call("status")),
            SETTLE,
            what="等待 fullscreen 出现在 status",
        )
        if not hit:
            return 1
        fs = hit["fullscreen"]
        if fs.get("app_id") != "io.github.suluoMISS.Liuguang":
            print(f"SKIP: 判定到的全屏窗口不是本检查开的（{fs.get('app_id')}），无法归因")
            return 0
        ch = first_channel(hit)
        print(
            f"✓ 检测到全屏：{fs.get('app_id') or fs.get('title')}"
            f"（暂停通道 {fs.get('paused_channels')} 个）"
        )
        if not ch.get("paused"):
            print(f"✗ 通道没有暂停：{json.dumps(ch, ensure_ascii=False)}")
            return 1
        if ch.get("pause_reason") != "fullscreen":
            print(f"✗ pause_reason 应为 fullscreen，实际 {ch.get('pause_reason')!r}")
            return 1
        print("✓ 视频已暂停，原因标为 fullscreen")

        toggle_fullscreen(win)
        back = wait_for(
            lambda: (lambda s: s if not s.get("fullscreen") and not first_channel(s).get("paused") else None)(
                ipc.call("status")
            ),
            SETTLE,
            what="等待退出全屏后恢复",
        )
        if not back:
            # 常见原因：检查期间用户又全屏了别的窗口（壁纸本来就该继续暂停）。
            now = ipc.call("status")
            if now.get("fullscreen"):
                who = now["fullscreen"].get("app_id") or now["fullscreen"].get("title")
                print(f"SKIP: 退出全屏后仍有别的窗口全屏（{who}），无法验证恢复")
                return 0
            return 1
        print("✓ 退出全屏后视频已自动恢复")

        ok = True
        ipc.close()
    finally:
        if gui_proc is not None and gui_proc.poll() is None:
            gui_proc.terminate()
            try:
                gui_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                gui_proc.kill()
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
        if args.keep:
            print(f"（保留现场：{tmp}）")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    print("✓ 全屏暂停检查通过" if ok else "✗ 全屏暂停检查失败")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
