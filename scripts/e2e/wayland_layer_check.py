#!/usr/bin/env python3
"""Wayland 壁纸层端到端检查：layer surface 必须铺满整块输出。

这条脚本守的是一个真机上出现过的缺陷（0.1.x → 0.2 收尾时修复）：
分数缩放（fractional scale）桌面上，`wl_output.scale` 报告的是整数 2，
而真实逻辑尺寸是物理 ÷ 1.5；只按整数 scale 出 buffer 时合成器把壁纸层画成
屏宽的 75%，且固定在左上角——**不报错、日志全绿、肉眼要切到空工作区才看得出**。
修复走 `wp_fractional_scale_v1` + `wp_viewport`（见 crates/liuguang-wayland）。

因此判据不是"进程活着 / 日志没 ERROR"，而是**像素**：
生成的标记图在最外圈有一圈亮绿边框、正中有一块纯白方块；截图后要求
  1. 四条屏幕边缘内侧都能扫到亮绿；
  2. 屏幕正中是纯白。
任一不满足即退出码 1 并打印实际取样，避免"看着像好了"。

依赖：python3（仅标准库）+ grim（截图）+ 一个支持 wlr-layer-shell 的合成器。
用法见 scripts/e2e/README.md。

环境变量：
  LIUGUANG / LIUGUANGCTL   二进制路径（默认依次找 target/debug、target/release、PATH）
  LIUGUANG_OUTPUT         要检查的输出名（默认取 `liuguangctl outputs` 第一个）
  LIUGUANG_GRIM           grim 路径（默认 PATH 里的 grim）
"""

from __future__ import annotations

import argparse
import os
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from pathlib import Path

# 标记图配色（RGB）。绿色边框既要在截图里显眼，也要与常见壁纸/主题不撞色。
BORDER = (0, 190, 60)
INNER = (18, 18, 24)
CENTER = (255, 255, 255)

BORDER_PX = 8
CENTER_BOX = 120


# ---------------------------------------------------------------------------
# 纯标准库 PNG 写出（不引入 Pillow：测试机不保证有）
# ---------------------------------------------------------------------------


def write_png(path: Path, width: int, height: int, pixel) -> None:
    """写 8 位 RGB PNG；`pixel(x, y) -> (r, g, b)`。"""
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # 每行的 filter type：None
        for x in range(width):
            raw.extend(pixel(x, y))

    def chunk(tag: bytes, payload: bytes) -> bytes:
        body = tag + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


def write_marker(path: Path, width: int, height: int) -> None:
    cx0, cy0 = (width - CENTER_BOX) // 2, (height - CENTER_BOX) // 2
    cx1, cy1 = cx0 + CENTER_BOX, cy0 + CENTER_BOX

    def pixel(x: int, y: int) -> tuple[int, int, int]:
        if x < BORDER_PX or y < BORDER_PX or x >= width - BORDER_PX or y >= height - BORDER_PX:
            return BORDER
        if cx0 <= x < cx1 and cy0 <= y < cy1:
            return CENTER
        return INNER

    write_png(path, width, height, pixel)


# ---------------------------------------------------------------------------
# PPM 解析（grim -t ppm 输出 P6）
# ---------------------------------------------------------------------------


def parse_ppm(data: bytes) -> tuple[int, int, bytes]:
    fields: list[int] = []
    pos = 0
    magic = b""
    while len(fields) < 3:
        # 跳过空白与注释
        while pos < len(data) and data[pos : pos + 1].isspace():
            pos += 1
        if data[pos : pos + 1] == b"#":
            while pos < len(data) and data[pos : pos + 1] != b"\n":
                pos += 1
            continue
        start = pos
        while pos < len(data) and not data[pos : pos + 1].isspace():
            pos += 1
        token = data[start:pos]
        if token == b"P6":
            magic = token
        else:
            fields.append(int(token))
    pos += 1  # 单个空白字符之后即像素数据
    width, height, maxval = fields
    if magic != b"P6" or maxval != 255:
        raise SystemExit(f"不支持的 PPM 头：{magic!r} {fields}")
    return width, height, data[pos:]


def sample(pixels: bytes, width: int, x: int, y: int) -> tuple[int, int, int]:
    off = (y * width + x) * 3
    return pixels[off], pixels[off + 1], pixels[off + 2]


def close_color(a: tuple[int, int, int], b: tuple[int, int, int], tol: int = 40) -> bool:
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def verify_coverage(width: int, height: int, pixels: bytes) -> list[str]:
    """检查截图是否"壁纸层铺满整块输出"；返回问题列表（空 = 通过）。"""
    failures: list[str] = []

    def edge_found(fixed_axis: str, near: int, scan_range) -> bool:
        for scan in scan_range:
            x, y = (scan, near) if fixed_axis == "x" else (near, scan)
            if close_color(sample(pixels, width, x, y), BORDER):
                return True
        return False

    step_x = max(1, width // 24)
    step_y = max(1, height // 24)
    probes = {
        "上边": [edge_found("x", y, range(0, width, step_x)) for y in range(0, 5)],
        "下边": [edge_found("x", y, range(0, width, step_x)) for y in range(height - 5, height)],
        "左边": [edge_found("y", x, range(0, height, step_y)) for x in range(0, 5)],
        "右边": [edge_found("y", x, range(0, height, step_y)) for x in range(width - 5, width)],
    }
    for name, hits in probes.items():
        if not any(hits):
            failures.append(f"{name}没有扫到壁纸边框（壁纸层没铺到该边缘）")

    center = sample(pixels, width, width // 2, height // 2)
    if not close_color(center, CENTER, tol=25):
        failures.append(f"屏幕正中颜色 {center} 不是标记图中心白块 {CENTER}（位置/缩放不对）")
    return failures


def synthetic_shot(width: int, height: int, scale: float) -> bytes:
    """合成一张"壁纸层按 `scale` 覆盖屏幕"的截图（自检用）。

    `scale=1.0` = 修复后的正确行为（铺满整屏，标记图被拉伸到整屏）；
    `scale=0.75` = 真机缺陷复现（分数缩放桌面只画了 75%、固定在左上角）。
    """
    mw, mh = 1024, 640
    rw, rh = int(width * scale), int(height * scale)
    cx0, cy0 = (mw - CENTER_BOX) // 2, (mh - CENTER_BOX) // 2
    out = bytearray()
    for y in range(height):
        for x in range(width):
            if x < rw and y < rh:
                sx, sy = x * mw // max(rw, 1), y * mh // max(rh, 1)
                border = sx < BORDER_PX or sy < BORDER_PX or sx >= mw - BORDER_PX or sy >= mh - BORDER_PX
                inside = cx0 <= sx < cx0 + CENTER_BOX and cy0 <= sy < cy0 + CENTER_BOX
                color = BORDER if border else (CENTER if inside else INNER)
            else:
                color = (0, 0, 0)
            out.extend(color)
    return bytes(out)


def self_check() -> int:
    """自检：合成图必须判"通过"，复现缺陷的合成图必须判"失败"。"""
    width, height = 2560, 1600
    ok_failures = verify_coverage(width, height, synthetic_shot(width, height, 1.0))
    if ok_failures:
        print("✗ 自检失败：铺满的合成截图被判为不合格：", ok_failures)
        return 1
    bad_failures = verify_coverage(width, height, synthetic_shot(width, height, 0.75))
    if not bad_failures:
        print("✗ 自检失败：75% 覆盖的合成截图竟然通过了（判据不敏感）")
        return 1
    print("✓ 自检通过：铺满→合格；75% 覆盖→不合格（判据对真机缺陷敏感）")
    return 0


# ---------------------------------------------------------------------------
# 环境探测与清理
# ---------------------------------------------------------------------------


def find_binary(env_var: str, name: str, root: Path) -> str:
    value = os.environ.get(env_var)
    if value:
        return value
    for profile in ("debug", "release"):
        candidate = root / "target" / profile / name
        if candidate.is_file():
            return str(candidate)
    found = shutil.which(name)
    if found:
        return found
    raise SystemExit(f"找不到 {name}：请先构建（cargo build）或用环境变量 {env_var} 指定")


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, **kwargs)


def pick_output(liuguangctl: str, socket: str, wanted: str | None) -> str:
    out = run([liuguangctl, "--socket", socket, "outputs"])
    if out.returncode != 0:
        raise SystemExit(f"liuguangctl outputs 失败：{out.stderr.strip()}")
    for line in out.stdout.splitlines():
        token = line.strip().split()
        if token:
            name = token[0]
            if wanted is None or name == wanted:
                return name
    raise SystemExit(f"没有匹配的输出（wanted={wanted}）：\n{out.stdout}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--switch-workspace", action="store_true",
                        help="先用 niri 切到相邻空工作区截图，随后切回（有窗口挡屏时用）")
    parser.add_argument("--keep", action="store_true", help="保留临时目录便于事后取证")
    parser.add_argument("--timeout", type=float, default=20.0, help="等待 StaticIdle 的秒数")
    parser.add_argument("--self-check", action="store_true",
                        help="不做真机检查，只跑判据自检（合成截图：铺满必须过、75%% 覆盖必须不过）")
    args = parser.parse_args()
    if args.self_check:
        return self_check()

    root = Path(__file__).resolve().parents[2]
    liuguang = find_binary("LIUGUANG", "liuguang", root)
    liuguangctl = find_binary("LIUGUANGCTL", "liuguangctl", root)
    grim = os.environ.get("LIUGUANG_GRIM") or shutil.which("grim")
    if not grim:
        raise SystemExit("找不到 grim（截图工具）：pacman -S grim")
    if not os.environ.get("WAYLAND_DISPLAY"):
        raise SystemExit("WAYLAND_DISPLAY 未设置：请在桌面会话里运行本脚本")

    tmp = Path(tempfile.mkdtemp(prefix="liuguang-wayland-e2e-"))
    media = tmp / "media"
    media.mkdir()
    socket = tmp / "control.sock"
    config = tmp / "config.toml"
    config.write_text(
        "\n".join([
            "[media]",
            f'allowed_roots = ["{media}"]',
            "",
            "[slideshow]",
            f'directory = "{media}"',
            "interval_seconds = 3600",
            "",
        ]),
        encoding="utf-8",
    )
    marker = media / "marker.png"
    write_marker(marker, 1024, 640)

    daemon = subprocess.Popen(
        [liuguang, "--backend", "wayland", "--config", str(config),
         "--socket", str(socket), "--state-file", str(tmp / "state.json")],
        stdout=(tmp / "daemon.log").open("wb"),
        stderr=subprocess.STDOUT,
    )

    switched = False
    try:
        deadline = time.time() + args.timeout
        while time.time() < deadline:
            if socket.exists() and run([liuguangctl, "--socket", str(socket), "status"]).returncode == 0:
                break
            if daemon.poll() is not None:
                raise SystemExit(f"liuguang 启动失败，日志：\n{(tmp / 'daemon.log').read_text(errors='replace')}")
            time.sleep(0.2)
        else:
            raise SystemExit("等待 IPC socket 超时")

        output = pick_output(liuguangctl, str(socket), os.environ.get("LIUGUANG_OUTPUT"))
        applied = run([liuguangctl, "--socket", str(socket), "set", str(marker)])
        if applied.returncode != 0:
            raise SystemExit(f"设置壁纸失败：{applied.stderr.strip()}")

        # 等 daemon 自己报 StaticIdle（比固定 sleep 稳）
        deadline = time.time() + args.timeout
        while time.time() < deadline:
            status = run([liuguangctl, "--socket", str(socket), "status"]).stdout
            if "StaticIdle" in status:
                break
            time.sleep(0.2)

        if args.switch_workspace:
            if run(["niri", "msg", "action", "focus-workspace-down"]).returncode != 0:
                raise SystemExit("--switch-workspace 需要 niri")
            switched = True
            time.sleep(0.8)

        shot = tmp / "shot.ppm"
        capture = run([grim, "-o", output, "-t", "ppm", str(shot)])
        if capture.returncode != 0:
            raise SystemExit(f"grim 截图失败：{capture.stderr.strip()}")

        width, height, pixels = parse_ppm(shot.read_bytes())
        failures = verify_coverage(width, height, pixels)

        if failures:
            print(f"✗ Wayland 壁纸层检查失败（输出 {output}，截图 {shot}）：")
            for item in failures:
                print(f"  - {item}")
            print(f"  屏幕 {width}x{height}；daemon 日志：{tmp / 'daemon.log'}")
            if not args.keep:
                pass  # 失败时保留现场，人为删除
            return 1

        print(f"✓ Wayland 壁纸层铺满 {width}x{height}（输出 {output}）：四边都有边框，中心标记就位")
        return 0
    finally:
        if switched:
            run(["niri", "msg", "action", "focus-workspace-up"])
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        if not args.keep and (tmp / "shot.ppm").exists() is False:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
