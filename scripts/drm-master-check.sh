#!/bin/bash
# drm-master-check.sh — 判定"某种运行方式能否**真正**持有 DRM master 并上屏"。
#
# 为什么需要它：DRM master 是**独占**的。只要有另一个实例（残留进程、手工调试实例、
# 另一份 systemd 单元）先持有它，后启动的实例会**打开设备成功、但每次提交都 EACCES**，
# 表现为**静默黑屏**（日志里是刷屏的 `atomic/legacy … Permission denied`）。
# 2026-10-18 就发生过一次误判：一次对照实验里机器上还有别的实例占着 master，
# 于是得出了"systemd 服务拿不到 master"的错误结论（实为竞争）。
#
# 本脚本因此**强制先做排他性预检**：只要还有别的 DRM 持有者，就拒绝测量（exit 3）。
#
# 判定证据（**不得**用 `status.video.presented` —— 它在提交全部失败时仍会增长）：
#   1. 内核视角的 master 标记：`/sys/kernel/debug/dri/<dev>/clients` 的 `master` 列（需要 root）；
#   2. 成功上屏：日志 `首帧/模式变更帧已提交扫描输出` 计数 ≥ 1；
#   3. 失败计数必须为 0：`提交彻底失败` 与 `Permission denied`。
#
# 用法（在目标机上，需要 root；本脚本只读内核状态 + 管理测试实例，不写配置）：
#   scripts/drm-master-check.sh preflight                 # 只做排他性预检
#   scripts/drm-master-check.sh measure service           # 测 systemd 单元（部署形态）
#   scripts/drm-master-check.sh measure manual <user>     # 测"在会话内手工运行"
#   scripts/drm-master-check.sh contention                # 先有持有者时，再起一个 → 应失败
#   scripts/drm-master-check.sh verify [秒数]             # 对**当前正在运行**的实例做健康判定
#
# 退出码：0 = 判定成功（拿到 master 并上屏）；1 = 判定失败；3 = 预检不通过（有竞争，拒绝测量）
set -u

DRM_CARD="${DRM_CARD:-/dev/dri/card1}"
# debugfs 里的设备目录名可能是 PCI 地址或 index，自动发现
find_debugfs_dir() {
  for d in /sys/kernel/debug/dri/*/; do
    [ -f "$d/clients" ] || continue
    # 选择与 DRM_CARD 对应的（通过 major:minor 匹配 /sys/class/drm/<card>/dev）
    if [ -r "/sys/class/drm/$(basename "$DRM_CARD")/dev" ]; then
      echo "$d"; return 0
    fi
  done
  return 1
}
DBG="$(find_debugfs_dir || true)"
CLIENTS="${DBG:+$DBG/clients}"

WORKDIR="${WORKDIR:-/run/drm-master-check}"
UNIT="${UNIT:-liuguang}"
BIN="${BIN:-/usr/local/bin/liuguang}"
CTL="${CTL:-/usr/local/bin/liuguangctl}"
MEDIA="${MEDIA:-/srv/wallpapers/t15-test.png}"
SOCK="${SOCK:-/run/liuguang/control.sock}"

die() { echo "❌ $*" >&2; exit 1; }
info() { echo "   $*"; }

holders() {
  # 输出持有 $DRM_CARD 的 PID（fuser 可能不可用，退回 /proc 扫描）
  if command -v fuser >/dev/null 2>&1; then
    fuser "$DRM_CARD" 2>/dev/null | tr -s ' ' '\n' | grep -E '^[0-9]+$' || true
  else
    for p in /proc/[0-9]*; do
      [ -e "$p/fd" ] || continue
      for fd in "$p"/fd/*; do
        if [ "$(readlink "$fd" 2>/dev/null)" = "$DRM_CARD" ]; then
          basename "$p"; break
        fi
      done
    done
  fi
}

prepare_workdir() {
  mkdir -p "$WORKDIR" && chmod 0755 "$WORKDIR" || die "无法创建工作目录 $WORKDIR"
}

preflight() {
  prepare_workdir
  # 宽限期：上一个实例可能正在退出（DRM 资源释放需要一点时间）
  local grace="${PREFLIGHT_WAIT:-12}" waited=0
  echo "── 排他性预检 ──────────────────────────────────────────────"
  local hs
  while :; do
    hs="$(holders | sort -u | tr '\n' ' ')"
    [ -z "${hs// /}" ] && break
    [ "$waited" -ge "$grace" ] && break
    sleep 1; waited=$((waited+1))
  done
  [ "$waited" -gt 0 ] && echo "   （等待 ${waited}s 让上一个实例释放设备）"
  echo "   持有 $DRM_CARD 的 PID: ${hs:-（无）}"
  local strays
  strays="$(pgrep -af 'liuguang' | grep -v " $0" || true)"
  if [ -n "$strays" ]; then echo "   liuguang 进程:"; echo "$strays" | sed 's/^/     /'; fi
  if [ -n "${hs// /}" ]; then
    echo "⚠ 设备已被占用。DRM master 只能有一个：请先停掉其它实例（systemctl stop $UNIT / pkill 手工实例）。"
    return 3
  fi
  if [ -n "$strays" ]; then
    echo "⚠ 仍有 liuguang 进程存在（可能尚未持有设备）。为可复现性请先清理。"
    return 3
  fi
  echo "   ✅ 无竞争：当前没有进程持有 $DRM_CARD"
  return 0
}

master_flag() {
  # $1 = pid；打印 y/n/single
  [ -n "$CLIENTS" ] && [ -r "$CLIENTS" ] || { echo "?"; return; }
  awk -v p="$1" '$2==p {print $4; f=1} END {if (!f) print "n"}' "$CLIENTS" | head -1
}

count() {
  [ -f "$2" ] || { echo 0; return; }
  grep -c "$1" "$2" 2>/dev/null || true
}

report() {
  # $1 = 日志文件, $2 = pid, $3 = 场景名
  local log="$1" pid="$2" what="$3"
  local mf ok fail eacces
  mf="$(master_flag "$pid")"
  ok="$(count '首帧/模式变更帧已提交扫描输出' "$log")"
  fail="$(count '提交彻底失败' "$log")"
  eacces="$(count 'Permission denied' "$log")"
  echo
  echo "── 判定：$what ─────────────────────────────────────────────"
  printf '   %-34s %s\n' "内核 master 标记（tgid $pid）" "$mf"
  printf '   %-34s %s\n' "首帧/模式变更帧已提交（成功）" "$ok"
  printf '   %-34s %s\n' "提交彻底失败" "$fail"
  printf '   %-34s %s\n' "Permission denied (EACCES)" "$eacces"
  if [ "$mf" = "y" ] && [ "${ok:-0}" -ge 1 ] && [ "${fail:-0}" -eq 0 ] && [ "${eacces:-0}" -eq 0 ]; then
    echo "   ✅ 结论：该运行方式**真正持有 DRM master 且已上屏**"
    return 0
  fi
  echo "   ❌ 结论：该运行方式**没有真正上屏**（见上表；master=${mf} 成功=${ok} 失败=${fail} EACCES=${eacces}）"
  return 1
}

wait_socket() { local s="$1"; for _ in $(seq 1 40); do [ -S "$s" ] && return 0; sleep 0.25; done; return 1; }

measure_service() {
  local log=$WORKDIR/service.log
  preflight || exit 3
  systemctl start "$UNIT" || die "systemctl start $UNIT 失败"
  wait_socket "$SOCK" || die "socket 未就绪: $SOCK"
  local pid; pid="$(systemctl show -p MainPID --value "$UNIT")"
  # 只统计**本次实例**的日志行（`_PID=` 精确到进程，避免把历史 EACCES 一起算进来）
  : > "$log"
  sleep 2
  "$CTL" --socket "$SOCK" set "$MEDIA" >/dev/null 2>&1 || true
  sleep 3
  journalctl -u "$UNIT" --no-pager _PID="$pid" > "$log" 2>&1
  report "$log" "$pid" "systemd 单元（部署形态，无会话）"
  local rc=$?
  systemctl stop "$UNIT" 2>/dev/null || true
  return $rc
}

measure_manual() {
  local user="$1" dir=/run/liuguang-manual log=$WORKDIR/manual.log
  preflight || exit 3
  rm -rf "$dir"; mkdir -p "$dir"; chown "$user" "$dir"
  cat > "$dir/config.toml" <<EOF
[media]
allowed_roots = ["$(dirname "$MEDIA")"]
[slideshow]
directory = "$(dirname "$MEDIA")"
interval_seconds = 3600
[ipc]
socket_path = "$dir/ctl.sock"
[runtime]
state_file = "$dir/state.json"
[outputs."DP-1"]
fit = "cover"
EOF
  : > "$log"   # 由 root 持有；子进程继承该 fd，无需被 chown
  setsid runuser -u "$user" -- env RUST_LOG="liuguang=info,liuguang_display=info" \
    "$BIN" --config "$dir/config.toml" --socket "$dir/ctl.sock" --state-file "$dir/state.json" \
    >> "$log" 2>&1 &
  wait_socket "$dir/ctl.sock" || die "手工实例 socket 未就绪"
  local pid; pid="$(pgrep -f "socket $dir/ctl.sock" | head -1)"
  "$CTL" --socket "$dir/ctl.sock" set "$MEDIA" >/dev/null 2>&1 || true
  sleep 3
  report "$log" "$pid" "会话内手工运行（用户 $user）"
  local rc=$?
  pkill -f "socket $dir/ctl.sock" 2>/dev/null || true
  sleep 1
  return $rc
}

contention() {
  prepare_workdir
  # 要求当前**已有**一个持有 master 的实例；再起一个，验证后者失败（复现"静默黑屏"签名）
  local user="${1:-${SUDO_USER:-$USER}}" dir=/run/liuguang-contend log=$WORKDIR/contend.log
  local hs; hs="$(holders | sort -u | tr '\n' ' ')"
  if [ -z "${hs// /}" ]; then
    echo "❌ 竞争测试要求先有一个持有 master 的实例；当前无持有者。" >&2
    return 3
  fi
  echo "   当前持有者 PID: $hs（保持不动）"
  rm -rf "$dir"; mkdir -p "$dir"; chown "$user" "$dir"
  sed -e "s#^socket_path.*#socket_path = \"$dir/ctl.sock\"#" \
      -e "s#^state_file.*#state_file = \"$dir/state.json\"#" \
      /dev/null >/dev/null 2>&1 || true
  cat > "$dir/config.toml" <<EOF
[media]
allowed_roots = ["$(dirname "$MEDIA")"]
[slideshow]
directory = "$(dirname "$MEDIA")"
interval_seconds = 3600
[ipc]
socket_path = "$dir/ctl.sock"
[runtime]
state_file = "$dir/state.json"
[outputs."DP-1"]
fit = "cover"
EOF
  : > "$log"   # 由 root 持有；子进程继承该 fd，无需被 chown
  setsid runuser -u "$user" -- env RUST_LOG="liuguang=info,liuguang_display=info" \
    "$BIN" --config "$dir/config.toml" --socket "$dir/ctl.sock" --state-file "$dir/state.json" \
    >> "$log" 2>&1 &
  wait_socket "$dir/ctl.sock" || die "第二实例 socket 未就绪"
  local pid; pid="$(pgrep -f "socket $dir/ctl.sock" | head -1)"
  "$CTL" --socket "$dir/ctl.sock" set "$MEDIA" >/dev/null 2>&1 || true
  sleep 5
  echo
  echo "── 竞争场景判定：后启动者（PID $pid）───────────────────────"
  printf '   %-34s %s\n' "提交彻底失败" "$(count '提交彻底失败' "$log")"
  printf '   %-34s %s\n' "Permission denied (EACCES)" "$(count 'Permission denied' "$log")"
  printf '   %-34s %s\n' "首帧/模式变更帧已提交（成功）" "$(count '首帧/模式变更帧已提交扫描输出' "$log")"
  echo "   预期：失败>0、成功=0 ⇒ 后启动者**静默黑屏**（这就是'看起来能自启、实则黑屏'的成因）"
  pkill -f "socket $dir/ctl.sock" 2>/dev/null || true
  sleep 1
  echo "   （已清理第二实例；第一个持有者未受影响）"
  return 0
}

verify_running() {
  prepare_workdir
  # 对当前实例做健康判定（不重启服务）。判据：master=y 且窗口内**没有**失败。
  # 注意：静止画面已上屏时，窗口内可能没有任何新提交——这是正常的，不算失败。
  local secs="${1:-10}" log=$WORKDIR/verify.log
  local pid; pid="$(pgrep -x liuguang | head -1)"
  [ -n "$pid" ] || die "没有正在运行的 liuguang"
  echo "── 观察 ${secs}s 内的提交行为（tgid $pid）─────────────────"
  journalctl -u "$UNIT" --no-pager --since "-${secs}s" _PID="$pid" > "$log" 2>&1
  local mf ok fail eacces
  mf="$(master_flag "$pid")"
  ok="$(count '首帧/模式变更帧已提交扫描输出' "$log")"
  fail="$(count '提交彻底失败' "$log")"
  eacces="$(count 'Permission denied' "$log")"
  printf '   %-34s %s\n' "内核 master 标记" "$mf"
  printf '   %-34s %s\n' "窗口内新提交（0=静止画面正常）" "$ok"
  printf '   %-34s %s\n' "窗口内提交彻底失败" "$fail"
  printf '   %-34s %s\n' "窗口内 Permission denied" "$eacces"
  if [ "$mf" = "y" ] && [ "${fail:-0}" -eq 0 ] && [ "${eacces:-0}" -eq 0 ]; then
    echo "   ✅ 结论：当前实例持有 DRM master 且窗口内无提交失败"
    [ -n "$CTL" ] && "$CTL" --socket "$SOCK" status 2>/dev/null | sed -n '1,5p' | sed 's/^/     /'
    return 0
  fi
  echo "   ❌ 结论：当前实例异常（master=${mf} 失败=${fail} EACCES=${eacces}）"
  return 1
}

case "${1:-}" in
  preflight) preflight ;;
  measure)
    case "${2:-}" in
      service) measure_service ;;
      manual)  measure_manual "${3:?用法: measure manual <user>}" ;;
      *) die "用法: measure service | measure manual <user>" ;;
    esac ;;
  contention) contention "${2:-${SUDO_USER:-$USER}}" ;;
  verify) verify_running "${2:-10}" ;;
  *) sed -n '2,26p' "$0"; exit 2 ;;
esac
