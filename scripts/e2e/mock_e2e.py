#!/usr/bin/env python3
"""真二进制的 mock 后端 E2E：set → WALLPAPER_APPLIED 事件 → pause/resume → next → 退出。

用法：LIUGUANG=liuguang python3 scripts/e2e/mock_e2e.py

不接触 DRM：独立的 socket/state/config，`--backend mock`。
"""
import json, os, shutil, socket, struct, subprocess, sys, tempfile, time, zlib

ROOT = tempfile.mkdtemp(prefix="liuguang-e2e-mock-")
SOCK = os.path.join(ROOT, "ctl.sock")
BIN = os.environ.get("LIUGUANG", "liuguang")
CTL = os.environ.get("LIUGUANGCTL", "liuguangctl")
FAILS = []

def png(path, w, h, rgba):
    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    raw = b"".join(b"\x00" + bytes(rgba) * w for _ in range(h))
    body = (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw))
            + chunk(b"IEND", b""))
    with open(path, "wb") as f:
        f.write(body)

def call(method, params=None, timeout=15):
    req = {"v": 1, "id": f"e2e-{method}", "method": method, "params": params or {}}
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        s.connect(SOCK)
        s.sendall((json.dumps(req) + "\n").encode())
        buf = b""
        while b"\n" not in buf:
            c = s.recv(65536)
            if not c:
                break
            buf += c
        return json.loads(buf.split(b"\n")[0])
    finally:
        s.close()

def check(cond, msg):
    if cond:
        print(f"  ✓ {msg}")
    else:
        print(f"  ✗ {msg}")
        FAILS.append(msg)

shutil.rmtree(ROOT, ignore_errors=True)
os.makedirs(os.path.join(ROOT, "media"))
png(os.path.join(ROOT, "media", "a.png"), 64, 36, (220, 30, 30, 255))
png(os.path.join(ROOT, "media", "b.png"), 36, 64, (30, 220, 30, 255))
cfg = os.path.join(ROOT, "config.toml")
with open(cfg, "w") as f:
    f.write(f"""[display]
backend = "drm"
device = "auto"
mode = "preferred"

[media]
allowed_roots = ["{ROOT}/media"]
video_fps_limit = 30

[slideshow]
enabled = true
directory = "{ROOT}/media"
interval_seconds = 3600
order = "sequential"

[transition]
enabled = false
effect = "fade"
duration_ms = 300
""")

log = open(os.path.join(ROOT, "daemon.log"), "wb")
proc = subprocess.Popen(
    [BIN, "--config", cfg, "--socket", SOCK, "--state-file", os.path.join(ROOT, "state.json"), "--backend", "mock"],
    stdout=log, stderr=subprocess.STDOUT)
try:
    for _ in range(80):
        if os.path.exists(SOCK):
            break
        if proc.poll() is not None:
            break
        time.sleep(0.25)
    check(proc.poll() is None, "mock daemon 启动")
    time.sleep(2.0)

    st = call("status").get("result", {})
    check(st.get("display_backend") == "mock", f"backend=mock（实际 {st.get('display_backend')}）")
    check(len(st.get("channels") or []) == 2, f"两个 mock 输出各建通道（{len(st.get('channels') or [])}）")

    # 订阅事件流
    sub = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sub.settimeout(10)
    sub.connect(SOCK)
    sub.sendall(b'{"v":1,"id":"sub","method":"subscribe","params":{}}\n')
    buf = b""
    while b"\n" not in buf:
        buf += sub.recv(65536)
    sub.settimeout(12)

    req_id = "e2e-set-1"
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(SOCK)
    s.sendall((json.dumps({"v":1,"id":req_id,"method":"set_wallpaper",
                           "params":{"path": os.path.join(ROOT,"media","a.png"), "output":"DP-1", "fit":"cover"}})+"\n").encode())
    rb = b""
    while b"\n" not in rb:
        rb += s.recv(65536)
    ack = json.loads(rb.split(b"\n")[0])
    s.close()
    check(ack.get("ok") is True, "set_wallpaper 受理")
    daemon_rid = (ack.get("result") or {}).get("request_id")
    check(isinstance(daemon_rid, str) and daemon_rid.startswith("set-"),
          f"受理响应携带 daemon 生成的 request_id（{daemon_rid}）")

    applied = None
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            data = sub.recv(65536)
        except socket.timeout:
            break
        if not data:
            break
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            ev = json.loads(line)
            if ev.get("kind") == "WALLPAPER_APPLIED":
                applied = ev
                break
        if applied:
            break
    check(applied is not None, "收到 WALLPAPER_APPLIED 事件")
    if applied:
        check(applied.get("ok") is True, "事件 ok=true")
        check(applied.get("request_id") == daemon_rid,
              f"事件 request_id 与受理响应一致（{applied.get('request_id')} vs {daemon_rid}）")

    time.sleep(0.5)
    st = call("status").get("result", {})
    check(st.get("state") == "StaticIdle", f"状态 StaticIdle（实际 {st.get('state')}）")
    check((st.get("media") or "").endswith("a.png"), f"media=a.png（{st.get('media')}）")

    check(call("pause", {"output": "DP-1"}).get("ok") is True, "pause 受理")
    check(call("status").get("result", {}).get("state") == "Paused", "pause 后 state=Paused")
    check(call("resume", {"output": "DP-1"}).get("ok") is True, "resume 受理")
    time.sleep(0.5)
    st = call("status").get("result", {})
    check(st.get("state") in ("StaticIdle", "VideoPlaying"), f"resume 后回到稳定态（{st.get('state')}）")

    check(call("next", {"output": "DP-1"}).get("ok") is True, "next 受理")
    time.sleep(0.8)
    st = call("status").get("result", {})
    check((st.get("media") or "").endswith("b.png"), f"next 后 media=b.png（{st.get('media')}）")

    # 退出：SIGTERM，进程应在 5s 内结束并释放 socket
    sub.close()
    proc.terminate()
    try:
        proc.wait(timeout=5)
        check(True, "SIGTERM 后 5s 内退出")
    except subprocess.TimeoutExpired:
        proc.kill()
        check(False, "SIGTERM 后 5s 内退出")
finally:
    if proc.poll() is None:
        proc.kill()
    log.close()

print()
if FAILS:
    print(f"判定：失败 {len(FAILS)} 项")
    for f in FAILS:
        print(" -", f)
    sys.exit(1)
print("判定：mock E2E 全部通过")
