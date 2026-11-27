#!/usr/bin/env bash
# 抓取 liuguang-gui 的界面截图（需要已运行的 liuguang socket + Wayland 会话 + grim）。
#
# 用法：
#   apps/liuguang-gui/screenshot.sh --socket /path/control.sock [--config PATH] \
#       [--out .work/gui] [--page library]
#
# 说明：
#   * 截图通过 grim 抓取整个输出，窗口是否铺满取决于合成器布局；
#   * 目标页面用 --open-page 指定（library/playlists/displays/settings/about）；
#   * 只用于验证/文档配图，不会修改壁纸或配置。
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
socket=""
config=""
out="$repo_root/.work/gui"
page="library"
wait_seconds=4

while [[ $# -gt 0 ]]; do
    case "$1" in
        --socket) socket=${2:?--socket 需要路径}; shift 2 ;;
        --config) config=${2:?--config 需要路径}; shift 2 ;;
        --out) out=${2:?--out 需要目录}; shift 2 ;;
        --page) page=${2:?--page 需要值}; shift 2 ;;
        --wait) wait_seconds=${2:?--wait 需要秒数}; shift 2 ;;
        -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "未知参数：$1" >&2; exit 2 ;;
    esac
done
[[ -n $socket ]] || { echo "缺少 --socket" >&2; exit 2; }
command -v grim >/dev/null || { echo "缺少 grim（Wayland 截图工具）" >&2; exit 2; }

gui="$repo_root/.work/build-gui/release/liuguang-gui"
[[ -x $gui ]] || { echo "找不到 $gui，先跑 scripts/build-gui.sh --release" >&2; exit 2; }
mkdir -p "$out"

args=("$gui" --socket "$socket" --open-page "$page")
[[ -n $config ]] && args+=(--config "$config")

"${args[@]}" >"$out/gui.log" 2>&1 &
gui_pid=$!
trap 'kill "$gui_pid" 2>/dev/null || true' EXIT

sleep "$wait_seconds"
target="$out/${page}.png"
grim "$target"
echo "已保存：$target"
