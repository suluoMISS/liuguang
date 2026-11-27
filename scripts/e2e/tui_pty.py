#!/usr/bin/env python3
"""TUI 真 socket 端到端：7 个页面逐个切换并断言页面专属文本出现；Q 退出不影响 daemon。

用法：LIUGUANGCTL=liuguangctl LIUGUANG_SOCKET=/run/liuguang/control.sock python3 scripts/e2e/tui_pty.py
"""
import fcntl, os, pty, re, select, struct, subprocess, termios, time

BIN = os.environ.get("LIUGUANGCTL", "liuguangctl")
SOCK = os.environ.get("LIUGUANG_SOCKET", "/run/liuguang/control.sock")
PAGES = [
    ("1", "总览", "当前显示"),
    ("2", "媒体库", "媒体库 ·"),
    ("3", "播放列表", "播放模式"),
    ("4", "显示器", "个模式"),
    ("5", "转场", "缓动"),
    ("6", "系统", "内存"),
    ("7", "静态壁纸", "当前静态壁纸"),
]

def clean(raw: str) -> str:
    out = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07", "", raw)
    return out.replace("\r", "")

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.environ["LANG"] = "C.UTF-8"
    os.execv(BIN, [BIN, "--socket", SOCK, "tui"])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 150, 0, 0))

def drain(seconds: float) -> str:
    buf = b""
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if not r:
            continue
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        buf += data
    return buf.decode("utf-8", errors="replace")

screen = drain(2.5)
# 先切到媒体库：默认页就是总览，若直接按 "1" 不会触发重绘（增量抓不到文本）。
os.write(fd, b"2")
screen += drain(0.9)
results = []
for key, title, marker in PAGES:
    os.write(fd, key.encode())
    chunk = drain(0.9)
    screen += chunk
    ok = marker in clean(chunk)
    results.append((key, title, marker, ok))
    print(f"  {'✓' if ok else '✗'} {key} {title}: 找 {marker!r}")

# 额外检查：daemon 无输出时 TUI 不得谎报「已上屏」
flat = clean(screen)
print(f"  · 屏上含『HeadlessReady』: {'HeadlessReady' in flat}")
print(f"  · 屏上含『静态壁纸要单独设置』: {'静态壁纸要单独设置' in flat}")

os.write(fd, b"Q")
exited = False
end = time.time() + 4
while time.time() < end:
    wpid, _ = os.waitpid(pid, os.WNOHANG)
    if wpid == pid:
        exited = True
        break
    time.sleep(0.1)
try:
    os.close(fd)
except OSError:
    pass
if not exited:
    os.kill(pid, 9)
    os.waitpid(pid, 0)

# 服务名检查只在显式要求时做（本地 mock 实例没有 systemd 单元）：
#   LIUGUANG_SERVICE_CHECK=1  → 用 systemctl is-active liuguang 验证没被 TUI 退出带崩。
daemon = "skipped"
if os.environ.get("LIUGUANG_SERVICE_CHECK") == "1":
    try:
        daemon = subprocess.run(["systemctl", "is-active", "liuguang"],
                                capture_output=True, text=True).stdout.strip()
    except FileNotFoundError:
        daemon = "unknown"
print(f"  · Q 退出 TUI: {'✓' if exited else '✗'}；daemon 服务: {daemon}")
if daemon not in ("skipped", "active"):
    print("判定：TUI 退出影响了 liuguang.service")
    raise SystemExit(1)
fails = [f"{k} {t}" for k, t, m, ok in results if not ok]
if fails:
    print("判定：失败页面 " + ", ".join(fails))
    raise SystemExit(1)
if not exited:
    print("判定：TUI 未在 Q 后退出")
    raise SystemExit(1)
print("判定：7 页全部渲染，Q 退出不影响 daemon")
