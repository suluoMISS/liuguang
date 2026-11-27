#!/usr/bin/env bash
# 打印**本机应填**的 `[outputs."<输出名>"]` 配置段。
#
# 为什么需要它：`[outputs.<名字>]` 段里的名字必须是本机实际输出名
# （例如 DP-1、HDMI-A-1）。若与示例配置不一致，未声明输出会在日志中产生提示。
#
# 用法：
#   scripts/gen-outputs-config.sh                       # 打印建议片段
#   scripts/gen-outputs-config.sh >> /etc/liuguang/config.toml   # 追加（先备份！）
#   scripts/gen-outputs-config.sh --socket /run/liuguang/control.sock
#
# 前置：liuguang 正在运行（socket 可访问）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SOCKET="${SOCKET:-/run/liuguang/control.sock}"
CTL="${LIUGUANGCTL:-}"
if [ -z "$CTL" ]; then
  for cand in /usr/local/bin/liuguangctl target/release/liuguangctl target/debug/liuguangctl; do
    if [ -x "$cand" ]; then CTL="$cand"; break; fi
  done
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --socket) SOCKET="$2"; shift 2 ;;
    --ctl) CTL="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$CTL" ]; then
  echo "❌ 找不到 liuguangctl（用 --ctl <路径> 指定）" >&2
  exit 3
fi

out="$("$CTL" --socket "$SOCKET" outputs 2>&1)" || {
  echo "❌ 无法枚举输出（liuguangctl 退出码 $?）：" >&2
  echo "$out" >&2
  echo "   提示：先确认服务在运行（systemctl status liuguang），并确保当前用户可访问 socket。" >&2
  exit 3
}

connected="$(printf '%s\n' "$out" | awk -F'\t' '$2 == "connected" { print $1 }')"
if [ -z "$connected" ]; then
  echo "❌ 没有任何已连接的输出（全部 disconnected）。先接好显示器再跑本脚本。" >&2
  echo "$out" >&2
  exit 1
fi

echo "# ==== 由 scripts/gen-outputs-config.sh 生成（$(date -Iseconds)）===="
echo "# 已连接的输出：$(printf '%s ' $connected)"
echo "# 下面的段名必须与本机实际输出名一致；追加到 /etc/liuguang/config.toml 后"
echo "#   liuguang check-config /etc/liuguang/config.toml   &&   sudo systemctl restart liuguang"
while IFS= read -r name; do
  [ -n "$name" ] || continue
  cat <<EOF

[outputs."$name"]
fit = "cover"                   # cover | contain | stretch | center
zoom = 1.0                      # (0, 10]
offset_x = 0.0                  # 输出像素
offset_y = 0.0                  # 输出像素
EOF
done <<< "$connected"
