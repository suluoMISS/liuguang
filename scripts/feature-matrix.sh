#!/usr/bin/env bash
# Liuguang feature 矩阵 —— **默认回归门槛**（不是可选项）
#
# 为什么必须单独跑：
#   `gstreamer` 是 `liuguang-media` 的**非默认** feature，`gles` 是
#   `liuguang-render` 的非默认 feature。因此
#       cargo test/clippy --workspace          （不带 --features）
#   根本不编译真实视频链路与 GPU 转场链路，无法覆盖真实播放路径。
#
# 输出约定（供人/CI 直接读）：
#   [阶段] 默认 feature 全绿        —— 只证明默认构型
#   [阶段] feature 矩阵全绿         —— 证明 gstreamer/gles 构型
#   两者都要出现才允许判定"全绿"。
#
# 用法：scripts/feature-matrix.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [ -z "${CARGO_HOME:-}" ] && [ -d "$ROOT/.cargo-home" ]; then
  export CARGO_HOME="$ROOT/.cargo-home"
fi

fail() {
  echo "❌ feature 矩阵失败：$*" >&2
  exit 1
}

# 每个构型都必须编译**测试目标**（--all-targets），否则测试代码里的
# feature 门控分支不会被编译，等于没检查。
echo "──────────────────────────────────────────────────────────────"
echo "[矩阵 1/4] liuguang-media --features gstreamer：测试"
CARGO_HOME="${CARGO_HOME:-}" cargo test -p liuguang-media --features gstreamer

echo "──────────────────────────────────────────────────────────────"
echo "[矩阵 2/4] liuguang-media --features gstreamer：clippy -D warnings"
CARGO_HOME="${CARGO_HOME:-}" cargo clippy -p liuguang-media --features gstreamer --all-targets -- -D warnings

echo "──────────────────────────────────────────────────────────────"
echo "[矩阵 3/4] liuguang --features gstreamer：编译（真实视频链路接线）"
CARGO_HOME="${CARGO_HOME:-}" cargo check -p liuguang --features gstreamer

echo "──────────────────────────────────────────────────────────────"
echo "[矩阵 4/4] liuguang-render --features gles：测试 + clippy"
CARGO_HOME="${CARGO_HOME:-}" cargo test -p liuguang-render --features gles
CARGO_HOME="${CARGO_HOME:-}" cargo clippy -p liuguang-render --features gles --all-targets -- -D warnings

echo "──────────────────────────────────────────────────────────────"
echo "✅ feature 矩阵全绿（gstreamer + gles 构型均已编译/测试/静态检查）"
