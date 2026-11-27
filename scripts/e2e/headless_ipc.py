#!/usr/bin/env python3
"""无显示器（HeadlessReady）下的 IPC 行为验收：成功的必须真成功，失败的必须明确失败。

用法：LIUGUANG_SOCKET=/run/liuguang/control.sock python3 scripts/e2e/headless_ipc.py
前置：daemon 在运行且当前**无已连接输出**（否则 status 不是 HeadlessReady）。
"""
import json, os, socket

SOCK = os.environ.get("LIUGUANG_SOCKET", "/run/liuguang/control.sock")
FAILS = []

def call(method, params=None, timeout=20):
    req = {"v": 1, "id": f"verify-{method}", "method": method, "params": params or {}}
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
        return json.loads(buf.split(b"\n")[0].decode())
    finally:
        s.close()

def expect_ok(name, params=None, cond=None):
    r = call(name, params)
    if not r.get("ok"):
        FAILS.append(f"{name}: 期望成功，实际 {r.get('error')}")
        print(f"  ✗ {name}: {r.get('error')}")
        return None
    res = r.get("result", {})
    if cond and not cond(res):
        FAILS.append(f"{name}: 结果不满足断言: {json.dumps(res, ensure_ascii=False)[:200]}")
        print(f"  ✗ {name}: 结果不符合预期")
    else:
        print(f"  ✓ {name}")
    return res

def expect_err(name, params, code):
    r = call(name, params)
    got = (r.get("error") or {}).get("code")
    if r.get("ok") or got != code:
        FAILS.append(f"{name}: 期望 {code}，实际 ok={r.get('ok')} code={got}")
        print(f"  ✗ {name}: 期望 {code}，实际 {got}")
    else:
        print(f"  ✓ {name} → {code}")

st = expect_ok("status", cond=lambda r: r.get("state") == "HeadlessReady")
if st:
    if st.get("channels"):
        FAILS.append(f"status.channels 应为空，实际 {st['channels']}")
        print("  ✗ status.channels 非空")
    else:
        print("  ✓ status.channels 为空")
    if (st.get("master") or {}).get("master") is True:
        print("  ✓ master=true（本进程持有 DRM master）")
    else:
        print(f"  ? master={st.get('master')}")

expect_ok("outputs", cond=lambda r: all(not o.get("connected") for o in r.get("outputs", [])))
expect_ok("list_media", {"page": 0, "page_size": 20},
          cond=lambda r: r.get("total") is not None)
expect_ok("list_sources")
expect_ok("list_playlists")
expect_ok("reload", {}, cond=lambda r: r.get("accepted") is True)

# 无输出时写操作必须明确失败（不能假装受理）
for m, p in [
    ("set_wallpaper", {"path": "/srv/wallpapers/sample_video.mp4", "output": "DP-1", "fit": "cover"}),
    ("pause", {"output": "DP-1"}),
    ("resume", {"output": "DP-1"}),
    ("next", {"output": "DP-1"}),
]:
    expect_err(m, p, "UNKNOWN_OUTPUT")

# 不存在的输出/越界路径仍必须是明确错误码
expect_err("set_static_wallpaper", {"path": "/etc/passwd", "output": "DP-1"}, "UNKNOWN_OUTPUT")

print()
if FAILS:
    print(f"判定：失败 {len(FAILS)} 项")
    for f in FAILS:
        print(" -", f)
    raise SystemExit(1)
print("判定：全部通过")
