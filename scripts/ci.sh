#!/usr/bin/env bash
# Liuguang 回归门禁：默认构型 + feature 矩阵。
#
# **判定标准**：只有同时打印出
#     ✅ [阶段] 默认 feature 全绿
#     ✅ [阶段] 矩阵阶段完成
# 才算通过。仅检查默认 feature 不会编译真实视频解码与 GPU 转场链路：
# `gstreamer` / `gles` 都是**非默认** feature，不带 `--features` 的
# `cargo test/clippy --workspace` 根本不编译真实视频链路与 GPU 转场链路。
#
# 用法：scripts/ci.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [ -z "${CARGO_HOME:-}" ] && [ -d "$ROOT/.cargo-home" ]; then
  export CARGO_HOME="$ROOT/.cargo-home"
fi

echo "══════════════════════════════════════════════════════════════"
echo "阶段 A：默认 feature 构型（不含 gstreamer / gles）"
echo "══════════════════════════════════════════════════════════════"

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test --workspace"
cargo test --workspace

echo "✅ [阶段] 默认 feature 全绿（注意：**这不覆盖**视频链路与 GPU 转场链路）"

echo
echo "══════════════════════════════════════════════════════════════"
echo "阶段 B：feature 矩阵（gstreamer / gles）—— 不可跳过"
echo "══════════════════════════════════════════════════════════════"
scripts/feature-matrix.sh

echo
echo "✅ [阶段] 矩阵阶段完成"
echo "✅ 门禁通过：默认 feature 全绿 **且** feature 矩阵全绿"
