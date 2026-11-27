#!/usr/bin/env python3
"""验证「用户会话人格」的路径解析与 CLI 免参数可用性。

要证明的三件事（都是桌面用户实际会踩的）：

1. 不带任何 `--config/--socket/--state-file` 启动时，daemon 把配置、状态、
   控制 socket 全部落到 XDG 目录，**不碰** `/etc`、`/var/lib`、`/run`；
2. `liuguangctl` 不带 `--socket` 也能找到 daemon（同一套人格判定），
   因此桌面用户**不需要**加入 `liuguang-ctl` 组，也不用记路径；
3. daemon 退出后 socket 被清理。

用法：

    python3 scripts/e2e/user_persona_check.py
    python3 scripts/e2e/user_persona_check.py --keep   # 失败时保留临时目录

需要：一个可用的 Wayland 会话（`WAYLAND_DISPLAY`）、已构建的 `liuguang`/`liuguangctl`
（用 `LIUGUANG`/`LIUGUANGCTL` 指向二进制，默认找 PATH）。
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

SOCKET_REL = "liuguang/control.sock"
STATE_REL = "liuguang/state.json"
CONFIG_REL = "liuguang/config.toml"


def make_png(path: Path, width: int, height: int, rgb: tuple[int, int, int]) -> None:
    """写一张纯色 PNG（只用标准库，避免依赖 Pillow）。"""
    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 6))
        + chunk(b"IEND", b"")
    )


def build_tree(root: Path) -> dict[str, Path]:
    media = root / "media"
    media.mkdir(parents=True)
    make_png(media / "teal.png", 320, 200, (0, 150, 160))
    config = root / "config" / CONFIG_REL
    config.parent.mkdir(parents=True)
    config.write_text(
        f"""[display]
backend = "auto"

[media]
allowed_roots = ["{media}"]

[slideshow]
directory = "{media}"
interval_seconds = 300

[outputs."eDP-1"]
fit = "cover"
zoom = 1.0
offset_x = 0.0
offset_y = 0.0
""",
        encoding="utf-8",
    )
    for name in ("state", "run", "home"):
        (root / name).mkdir(parents=True, exist_ok=True)
    return {
        "root": root,
        "media": media,
        "config": config,
        "state_dir": root / "state",
        "runtime_dir": root / "run",
    }


def env_for(tree: dict[str, Path]) -> dict[str, str]:
    env = dict(os.environ)
    env["HOME"] = str(tree["root"] / "home")
    env["XDG_CONFIG_HOME"] = str(tree["root"] / "config")
    env["XDG_STATE_HOME"] = str(tree["state_dir"])
    env["XDG_RUNTIME_DIR"] = str(tree["runtime_dir"])
    # 合成器 socket 还在真会话里：用绝对路径，不随 XDG_RUNTIME_DIR 漂移。
    display = os.environ.get("WAYLAND_DISPLAY", "")
    if display and not display.startswith("/"):
        session_runtime = os.environ.get("XDG_RUNTIME_DIR", "")
        if session_runtime:
            env["WAYLAND_DISPLAY"] = str(Path(session_runtime) / display)
    env["RUST_LOG"] = "liuguang=info,liuguang_wayland=info"
    return env


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="失败时保留临时目录排查")
    args = ap.parse_args()

    binary = os.environ.get("LIUGUANG") or shutil.which("liuguang")
    ctl = os.environ.get("LIUGUANGCTL") or shutil.which("liuguangctl")
    if not binary or not ctl:
        print("✗ 找不到 liuguang/liuguangctl（用 LIUGUANG/LIUGUANGCTL 指定）", file=sys.stderr)
        return 2
    if not os.environ.get("WAYLAND_DISPLAY"):
        print("✗ 需要 Wayland 会话（WAYLAND_DISPLAY 为空）", file=sys.stderr)
        return 2

    root = Path(tempfile.mkdtemp(prefix="liuguang-userpersona-"))
    tree = build_tree(root)
    env = env_for(tree)
    log = (root / "daemon.log").open("w+b")
    proc = subprocess.Popen(  # noqa: S603 - 受控调用
        [binary],  # 故意**不带任何参数**
        env=env,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    failures: list[str] = []

    def fail(msg: str) -> None:
        failures.append(msg)
        print(f"  ✗ {msg}")

    try:
        sock = tree["runtime_dir"] / SOCKET_REL
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and not sock.exists():
            if proc.poll() is not None:
                break
            time.sleep(0.2)
        if sock.exists():
            print(f"  ✓ 控制 socket 落在 XDG：{sock.relative_to(root)}")
        else:
            fail("XDG_RUNTIME_DIR 下没有出现 control.sock（可能回落到了 /run）")

        # CLI 同样只靠环境（不带 --socket）
        first = ""
        settled = False
        readiness = time.monotonic() + 20
        while time.monotonic() < readiness:
            out = subprocess.run(  # noqa: S603
                [ctl, "status"],
                env=env,
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
            )
            if out.returncode != 0:
                fail(
                    "liuguangctl 无法连接（说明人格判定不一致）："
                    + (out.stderr or out.stdout).strip()[:200]
                )
                break
            lines = out.stdout.strip().splitlines()
            first = lines[0] if lines else ""
            if "/run/liuguang/control.sock" in (out.stderr or ""):
                fail("liuguangctl 仍在使用系统 socket 路径")
                break
            if "InitializingDisplay" not in first:
                settled = True
                break
            time.sleep(0.5)
        if first:
            suffix = "" if settled else "（仍停在 InitializingDisplay：合成器未给 configure？）"
            print(f"  ✓ liuguangctl 免 --socket 连上：{first}{suffix}")
            if not settled:
                fail("状态没有离开 InitializingDisplay，壁纸层可能没上屏")

        # 状态文件在第一次落定后才写入（InitializingDisplay 期间不写盘）
        state = tree["state_dir"] / STATE_REL
        if state.exists():
            print(f"  ✓ 状态文件落在 XDG：{state.relative_to(root)}")
        else:
            fail("XDG_STATE_HOME 下没有出现 state.json")

        # 显式 --persona system 必须回到系统路径（证明覆盖生效且不静默）
        out_sys = subprocess.run(  # noqa: S603
            [ctl, "--persona", "system", "status"],
            env=env,
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )
        if out_sys.returncode != 0 and "/run/liuguang/control.sock" in (
            out_sys.stderr or ""
        ):
            print("  ✓ --persona system 明确指向 /run/liuguang/control.sock")
        else:
            fail("--persona system 没有回落到系统 socket（覆盖失效？）")
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        log.close()
        sock = tree["runtime_dir"] / SOCKET_REL
        if sock.exists():
            fail("daemon 退出后 socket 仍残留")
        else:
            print("  ✓ 退出后 socket 已清理")

    if failures:
        print(f"\n✗ 用户人格检查失败（{len(failures)} 项）")
        print(f"  临时目录保留在 {root}（日志：daemon.log）")
        return 1
    print("\n✓ 用户人格检查通过：路径全在 XDG，CLI 免参数可用，退出不留 socket")
    if not args.keep:
        shutil.rmtree(root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
