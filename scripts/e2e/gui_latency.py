#!/usr/bin/env python3
"""测量 GUI 相关的 IPC 延迟与 CPU，用来量化「打开 GUI 就卡」。

只依赖标准库。默认连用户会话的 liuguang，也可用 --socket 指向沙箱。

四组测量（每组都可单独跑）：

1. ``--rtt``：新连接与持久连接的 ``status`` 往返；
   新连接慢是因为 server 的 accept 轮询（修复前恒 +100ms）。
2. ``--thumb``：同一张图同一 ``at_ms`` 连请求 5 次；
   修复前每次都重解（无缓存），耗时 400ms 量级且不收敛。
3. ``--apply``：``set_wallpaper`` 的端到端延迟；
   修复前 GUI 正在加载封面时会排队到 1s 以上。
4. ``--cpu PID[,PID] --seconds N``：按秒采样进程 CPU（/proc/<pid>/stat 差分），
   用来抓「一打开 GUI 守护进程就满载 50s」。

用法：:

    # 基线（另开一个终端先启动 GUI 最贴近现场）
    python3 scripts/e2e/gui_latency.py --all --video ~/Pictures/Wallpapers/x.mp4
    python3 scripts/e2e/gui_latency.py --cpu $(pgrep -x liuguang),$(pgrep -x liuguang-gui) --seconds 60

判据（captain 定义）：新连接 status ≤15ms；重复缩略图第 2 次起 ≤15ms；
加载中 apply ≤600ms；GUI 打开后 20s 内 daemon CPU ≤25%。
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
import time
from pathlib import Path

DEFAULT_RUNTIME = os.environ.get("XDG_RUNTIME_DIR", "/run/user/1000")
DEFAULT_SOCKET = os.path.join(DEFAULT_RUNTIME, "liuguang", "control.sock")


class Conn:
    def __init__(self, path: str) -> None:
        self.path = path
        self.sock: socket.socket | None = None
        self.seq = 0

    def connect(self) -> None:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(120.0)
        s.connect(self.path)
        self.sock = s

    def call(self, method: str, params: dict | None = None) -> tuple[float, dict]:
        assert self.sock is not None, "call connect() first"
        self.seq += 1
        req = {"v": 1, "id": f"lat-{self.seq}", "method": method, "params": params or {}}
        payload = (json.dumps(req, ensure_ascii=False) + "\n").encode()
        started = time.perf_counter()
        self.sock.sendall(payload)
        buf = b""
        while b"\n" not in buf:
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise RuntimeError("daemon closed the connection")
            buf += chunk
        elapsed_ms = (time.perf_counter() - started) * 1000.0
        return elapsed_ms, json.loads(buf.split(b"\n", 1)[0].decode())

    def close(self) -> None:
        if self.sock is not None:
            self.sock.close()
            self.sock = None


def fmt(ms: float) -> str:
    return f"{ms:8.1f} ms"


def measure_rtt(path: str, samples: int) -> int:
    print("== 新连接 status（每次都重连）==")
    fresh: list[float] = []
    for _ in range(samples):
        c = Conn(path)
        c.connect()
        ms, resp = c.call("status")
        fresh.append(ms)
        if not resp.get("ok"):
            print(f"  !! status 失败: {resp.get('error')}")
            return 1
        c.close()
    print(f"  样本 {len(fresh)}：min {min(fresh):.1f} / 中位 {sorted(fresh)[len(fresh) // 2]:.1f} / max {max(fresh):.1f} ms")

    print("== 持久连接 status（连一次问多次）==")
    c = Conn(path)
    c.connect()
    warm = [c.call("status")[0] for _ in range(samples)]
    c.close()
    print(f"  样本 {len(warm)}：min {min(warm):.1f} / 中位 {sorted(warm)[len(warm) // 2]:.1f} / max {max(warm):.1f} ms")

    fresh_med = sorted(fresh)[len(fresh) // 2]
    warm_med = sorted(warm)[len(warm) // 2]
    verdict = "PASS" if fresh_med <= 15.0 and warm_med <= 15.0 else "FAIL"
    print(f"  判据 新连接 ≤15ms 且 持久 ≤15ms ⇒ {verdict}（新连接中位 {fresh_med:.1f}，持久中位 {warm_med:.1f}）")
    return 0 if verdict == "PASS" else 2


def pick_video(path: str | None) -> str | None:
    if path:
        return str(Path(path).expanduser())
    roots = [Path.home() / "Pictures" / "Wallpapers"]
    for root in roots:
        if not root.is_dir():
            continue
        for entry in sorted(root.rglob("*")):
            if entry.suffix.lower() in {".mp4", ".mkv", ".webm", ".mov"}:
                return str(entry)
    return None


def measure_thumb(path: str, video: str, samples: int) -> int:
    print(f"== 同一张缩略图连请 {samples} 次（{Path(video).name}）==")
    c = Conn(path)
    c.connect()
    times: list[float] = []
    for i in range(samples):
        ms, resp = c.call(
            "get_thumbnail",
            {"id": video, "at_ms": 1000, "max_width": 320, "max_height": 200},
        )
        times.append(ms)
        ok = resp.get("ok")
        note = "" if ok else f" error={resp.get('error', {}).get('code')}"
        print(f"  #{i + 1}: {fmt(ms)}{note}")
        if not ok:
            c.close()
            return 1
    c.close()
    warm = times[1:]
    verdict = "PASS" if warm and max(warm) <= 15.0 else "FAIL"
    print(f"  判据 第 2 次起 ≤15ms ⇒ {verdict}（第 1 次 {times[0]:.1f} ms，其后 max {max(warm):.1f} ms）")
    return 0 if verdict == "PASS" else 2


def measure_apply(path: str, video: str) -> int:
    print(f"== set_wallpaper 延迟（{Path(video).name}）==")
    c = Conn(path)
    c.connect()
    times: list[float] = []
    for i in range(3):
        ms, resp = c.call("set_wallpaper", {"path": video})
        times.append(ms)
        ok = resp.get("ok")
        print(f"  #{i + 1}: {fmt(ms)} ok={ok}")
        if not ok:
            body = resp.get("error") or {}
            print(f"    {body.get('code')}: {body.get('message')}")
    st_ms, st = c.call("status")
    c.close()
    state = ((st.get("result") or {}).get("state")) or "?"
    print(f"  status {st_ms:.0f} ms ⇒ state={state}")
    worst = max(times)
    verdict = "PASS" if worst <= 600.0 else "FAIL"
    print(f"  判据 单次 ≤600ms ⇒ {verdict}（最差 {worst:.1f} ms）")
    return 0 if verdict == "PASS" else 2


def proc_ticks(pid: int) -> tuple[int, int] | None:
    try:
        data = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    fields = data.rsplit(") ", 1)[1].split()
    # fields[0] 是 state；utime/stime 在整体里的第 14/15 个字段
    utime, stime = int(fields[11]), int(fields[12])
    return utime + stime, os.sysconf("SC_CLK_TCK")


def measure_cpu(pids: list[int], seconds: int, interval: float = 2.0) -> int:
    hz = os.sysconf("SC_CLK_TCK")
    cores = os.cpu_count() or 1
    baseline = {pid: proc_ticks(pid) for pid in pids}
    print(f"== CPU 采样 {seconds}s（间隔 {interval}s，{cores} 核）==")
    last = {pid: (t[0] if t else 0) for pid, t in baseline.items()}
    started = time.time()
    samples = 0
    peak = {pid: 0.0 for pid in pids}
    while time.time() - started < seconds:
        time.sleep(interval)
        samples += 1
        row = []
        for pid in pids:
            ticks = proc_ticks(pid)
            if ticks is None:
                row.append(f"{pid}:gone")
                continue
            delta = ticks[0] - last[pid]
            last[pid] = ticks[0]
            pct = delta / hz / interval * 100.0
            peak[pid] = max(peak[pid], pct)
            row.append(f"{pid}:{pct:5.1f}%")
        print(f"  t={time.time() - started:5.1f}s  " + "  ".join(row))
    print(f"  峰值：" + "  ".join(f"{pid}={peak[pid]:.1f}%" for pid in pids))
    if samples == 0:
        print("  !! 没有采到样本")
        return 1
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--socket", default=DEFAULT_SOCKET)
    ap.add_argument("--video", help="用于缩略图/应用的素材路径（默认在 ~/Pictures/Wallpapers 里找第一个视频）")
    ap.add_argument("--samples", type=int, default=5)
    ap.add_argument("--rtt", action="store_true")
    ap.add_argument("--thumb", action="store_true")
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--cpu", default="")
    ap.add_argument("--seconds", type=int, default=30)
    ap.add_argument("--interval", type=float, default=2.0)
    args = ap.parse_args()

    if not Path(args.socket).exists():
        print(f"!! socket 不存在：{args.socket}", file=sys.stderr)
        return 1

    rc = 0
    if args.all or args.rtt:
        rc |= measure_rtt(args.socket, args.samples)
    if args.all or args.thumb or args.apply:
        video = pick_video(args.video)
        if video is None:
            print("!! 没找到可用视频素材，跳过缩略图/应用测量", file=sys.stderr)
            rc |= 1
        else:
            if args.all or args.thumb:
                rc |= measure_thumb(args.socket, video, max(args.samples, 2))
            if args.all or args.apply:
                rc |= measure_apply(args.socket, video)
    if args.cpu:
        pids = [int(x) for x in args.cpu.split(",") if x.strip().isdigit()]
        rc |= measure_cpu(pids, args.seconds, args.interval)
    return rc


if __name__ == "__main__":
    sys.exit(main())
