#!/usr/bin/env bash
# 构建**部署用** release 产物，并做体积 / debug_info / 视频链路自检。
#
# 为什么必须用 release：debug 构建没有优化，循环、拷贝、每帧分配全都慢数倍。
# 交付 debug 二进制会让"低 CPU 占用"这个目标从根上做不到
# （真机曾误部署 84.5 MB 的带 debug_info 二进制，release 只有个位数 MB）。
#
# 为什么必须带 `--features gstreamer`：`gstreamer` 是 liuguang-media 的非默认
# feature。不带它构建出的 liuguang 不含真实视频解码链路，视频壁纸无法播放。
#
# 用法：
#   scripts/build-release.sh              # 构建并自检，产物在 target/release/
#   OUT_DIR=/tmp/liuguang-dist scripts/build-release.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [ -z "${CARGO_HOME:-}" ] && [ -d "$ROOT/.cargo-home" ]; then
  export CARGO_HOME="$ROOT/.cargo-home"
fi

# 体积上限（字节）：release + gstreamer 实测约 6–10 MB；给足余量但仍能拦住
# "不小心部署 debug 构建"（debug 为 80+ MB）。
MAX_BYTES=$((24 * 1024 * 1024))

echo "==> cargo build --release -p liuguang -p liuguangctl --features gstreamer"
cargo build --release -p liuguang -p liuguangctl --features gstreamer

status=0

check_binary() {
  local bin="$1"
  [ -f "$bin" ] || { echo "❌ 缺少产物: $bin"; status=1; return; }
  local bytes size_mb kind
  bytes=$(stat -c %s "$bin")
  size_mb=$(awk -v b="$bytes" 'BEGIN{printf "%.1f", b/1048576}')
  kind=$(file -b "$bin" || true)
  echo "--- $bin: ${size_mb} MB"

  if [ "$bytes" -gt "$MAX_BYTES" ]; then
    echo "❌ 体积 ${size_mb} MB 超过上限 $((MAX_BYTES / 1048576)) MB。"
    echo "   最可能的原因：把 debug 构建（未 strip / 带 debug_info）当成交付产物。"
    echo "   修复：用本脚本（--release）重新构建并部署。"
    status=1
  fi
  if printf '%s' "$kind" | grep -q "debug_info"; then
    echo "❌ 二进制带 debug_info（未优化/未 strip）：$kind"
    echo "   修复：cargo build --release（本脚本已带 --release）。"
    status=1
  fi
}

check_binary target/release/liuguang
check_binary target/release/liuguangctl

# 视频链路自检：管线元素名只会在 `--features gstreamer` 的代码路径里出现。
# 这是启发式检查（不能证明运行时协商成功），但足以拦住"没带 feature 就发布"。
if command -v strings >/dev/null 2>&1; then
  missing=0
  for token in qtdemux h264parse videoconvert; do
    if ! strings -a target/release/liuguang | grep -q "$token"; then
      echo "❌ 二进制中找不到 GStreamer 管线元素名 \`$token\` → 该构建**不含真实视频链路**"
      missing=1
    fi
  done
  if [ "$missing" -eq 0 ]; then
    echo "✅ 视频链路自检通过（找到 qtdemux / h264parse / videoconvert）"
  else
    echo "   修复：cargo build --release --features gstreamer（本脚本已带）。"
    status=1
  fi
else
  echo "⚠ 未找到 \`strings\`，跳过视频链路自检"
fi

if [ "$status" -ne 0 ]; then
  echo "❌ 部署产物自检失败（见上）。"
  exit 1
fi

echo
echo "✅ 部署产物自检通过。"
echo "   部署（建议先停止已有实例，避免与手工实例抢 DRM master）："
echo "     sudo systemctl stop liuguang 2>/dev/null || true"
echo "     sudo install -m 0755 target/release/liuguang  /usr/local/bin/liuguang"
echo "     sudo install -m 0755 target/release/liuguangctl /usr/local/bin/liuguangctl"
if [ -n "${OUT_DIR:-}" ]; then
  mkdir -p "$OUT_DIR"
  install -m 0755 target/release/liuguang "$OUT_DIR/liuguang"
  install -m 0755 target/release/liuguangctl "$OUT_DIR/liuguangctl"
  echo "   已复制到 $OUT_DIR/"
fi
