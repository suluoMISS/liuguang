#!/usr/bin/env bash
# 流光（Liuguang，工程名 Liuguang）一键安装。
#
#   ./scripts/install.sh --mode user           # 桌面形态：当前 Wayland 会话里的壁纸层（必要时会请求权限）
#   ./scripts/install.sh --mode system         # 无头形态：系统用户 + DRM/KMS 直出（自动请求 sudo/pkexec）
#   ./scripts/install.sh --check               # 只体检，不改任何东西
#   ./scripts/install.sh --mode user --dry-run
#   ./scripts/install.sh --mode system --uninstall
#   ./scripts/install.sh -y --mode user        # 非交互确认（仍仅在需要系统权限时提权）
#
# 两种形态**互斥**：桌面机器上再启用系统单元，root 会去抢 DRM master，
# 把用户的桌面画面撕掉（见 docs/ARCHITECTURE.md §8）。
#
# 设计取舍（为什么脚本要"替用户记事"）：
#   * 幂等：能重复跑。已有的配置/单元/用户/组不会被覆盖，只补缺的；
#   * 权限显式可见：需要 root 时说明原因，通过 sudo/pkexec 请求授权，不静默失败；
#   * 可见：每一步都打印"做了什么/跳过了什么"，失败时给出下一条命令；
#   * 可逆：`--uninstall` 只删自己装的东西，配置与媒体一律保留。
set -euo pipefail

ORIG_ARGS=("$@")
MODE=""
DRY_RUN=0
UNINSTALL=0
CHECK_ONLY=0
AUTO_YES=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --mode=*) MODE="${1#--mode=}"; shift ;;
    --mode)
      if [ "$#" -lt 2 ]; then
        echo "--mode 需要参数（user 或 system）" >&2
        exit 2
      fi
      MODE="$2"
      shift 2
      ;;
    user|system) MODE="$1"; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --check|--dry-run-check) CHECK_ONLY=1; shift ;;
    -y|--yes) AUTO_YES=1; shift ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数：$1（看 --help）" >&2; exit 2 ;;
  esac
done

REPO="$(cd "$(dirname "$0")/.." && pwd)"
UNIT_NAME="liuguang.service"

run() {
  if [ "$DRY_RUN" = 1 ]; then
    printf '  [dry-run] %s\n' "$*"
  else
    "$@"
  fi
}

say() { printf '%s\n' "$*"; }
ok() { printf '  ✓ %s\n' "$*"; }
bad() { printf '  ✗ %s\n' "$*"; }
note() { printf '  · %s\n' "$*"; }

# 需要系统权限时的统一提权入口：先说明原因，再优先走 sudo，图形会话无 TTY 时回退 pkexec。
run_as_root() {
  local reason="$1"
  shift
  if [ "$(id -u)" -eq 0 ]; then
    run "$@"
    return $?
  fi
  if [ "$DRY_RUN" = 1 ]; then
    note "需要管理员权限（$reason）"
    printf '  [dry-run] sudo %s\n' "$*"
    return 0
  fi
  note "需要管理员权限：$reason"
  if command -v sudo >/dev/null 2>&1; then
    if [ -t 0 ] || sudo -n true >/dev/null 2>&1; then
      sudo "$@"
      return $?
    fi
  fi
  if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v pkexec >/dev/null 2>&1; then
    pkexec "$@"
    return $?
  fi
  bad "无法在当前非交互环境中请求管理员权限（原因：$reason）"
  say "  请手动执行：sudo $*"
  return 1
}

ask_confirm() {
  local prompt="$1"
  if [ "$AUTO_YES" = 1 ]; then
    return 0
  fi
  if [ "$DRY_RUN" = 1 ]; then
    note "[dry-run] 将询问：$prompt"
    return 0
  fi
  if [ ! -t 0 ]; then
    return 1
  fi
  local ans
  printf '  ? %s [Y/n] ' "$prompt"
  read -r ans || return 1
  case "${ans:-Y}" in
    y|Y|yes|YES|"") return 0 ;;
    *) return 1 ;;
  esac
}

# 定位二进制；若 PATH 里还没有但仓库刚编完 release，桌面模式可直接安装到 ~/.local/bin（无需 root）。
ensure_user_bins_in_path() {
  if command -v liuguang >/dev/null 2>&1 && command -v liuguangctl >/dev/null 2>&1; then
    return 0
  fi
  local local_bin="$HOME/.local/bin"
  for candidate in "$REPO/.work/build/release" "$REPO/target/release"; do
    if [ -x "$candidate/liuguang" ] && [ -x "$candidate/liuguangctl" ]; then
      note "检测到仓库已编译产物：$candidate"
      if ask_confirm "安装 liuguang 与 liuguangctl 到 $local_bin（无需 root）？"; then
        run mkdir -p "$local_bin"
        run install -Dm755 "$candidate/liuguang" "$local_bin/liuguang"
        run install -Dm755 "$candidate/liuguangctl" "$local_bin/liuguangctl"
        if [ -x "$candidate/liuguang-gui" ]; then
          run install -Dm755 "$candidate/liuguang-gui" "$local_bin/liuguang-gui"
        fi
        export PATH="$local_bin:$PATH"
        ok "已安装到 $local_bin"
        return 0
      fi
    fi
  done
  return 1
}

# 检查 /dev/dri 访问权限；首次安装时如果缺 video/render 组权限，显式提示或请求 sudo 加组。
check_dri_permissions() {
  local need_groups=()
  local dev grp
  for dev in /dev/dri/card* /dev/dri/renderD*; do
    [ -e "$dev" ] || continue
    if [ ! -r "$dev" ]; then
      grp="$(stat -c '%G' "$dev" 2>/dev/null || true)"
      case "$grp" in
        video|render)
          if [[ " ${need_groups[*]:-} " != *" $grp "* ]]; then
            need_groups+=("$grp")
          fi
          ;;
      esac
    fi
  done
  if [ "${#need_groups[@]}" -eq 0 ]; then
    ok "/dev/dri 设备可读"
    return 0
  fi
  local target_user="${SUDO_USER:-$(id -un)}"
  local joined
  joined="$(IFS=,; echo "${need_groups[*]}")"
  note "当前用户 $target_user 对部分 /dev/dri 设备缺少读权限（需要组：$joined，用于视频硬解/缩略图）"
  if [ "$CHECK_ONLY" = 1 ]; then
    note "可运行：sudo usermod -aG $joined $target_user（重新登录生效）"
    return 0
  fi
  if ask_confirm "是否请求管理员权限将 $target_user 加入 $joined 组？"; then
    if run_as_root "将 $target_user 加入 $joined 组以访问 GPU 硬件加速" usermod -aG "$joined" "$target_user"; then
      ok "已把 $target_user 加入 $joined（重新登录会话后完全生效）"
    fi
  else
    note "已跳过加组；若后续硬解不可用，请执行：sudo usermod -aG $joined $target_user"
  fi
}

# ---------------------------------------------------------------- 体检
# 只读：不创建、不修改、不启停任何东西。两种模式共用。
diagnose_common() {
  local fail=0
  say "== 体检：二进制 =="
  for bin in liuguang liuguangctl; do
    if command -v "$bin" >/dev/null 2>&1; then
      ok "$bin → $(command -v "$bin")（$("$bin" --version 2>/dev/null | head -1)）"
    else
      bad "找不到 $bin（先装包，或 cargo build --release 后把二进制放进 PATH）"
      fail=1
    fi
  done

  say
  say "== 体检：媒体解码/GPU（只是提示，缺了也能跑 CPU 路径）=="
  if command -v gst-inspect-1.0 >/dev/null 2>&1; then
    ok "gstreamer 工具在（$(gst-inspect-1.0 --version 2>/dev/null | head -1)）"
  else
    note "没有 gst-inspect-1.0：视频可能不可用（图片仍然可显示）"
  fi
  local nodes
  nodes=$(ls /dev/dri 2>/dev/null | tr '\n' ' ' || true)
  if [ -n "$nodes" ]; then
    ok "/dev/dri: $nodes"
    check_dri_permissions
  else
    note "没有 /dev/dri（容器里？视频/壁纸层都会受限）"
  fi
  if command -v vainfo >/dev/null 2>&1; then
    note "VA-API: $(vainfo 2>/dev/null | grep -m1 'vainfo: Driver version' || echo '未报告驱动')"
  fi
  return "$fail"
}

# ---------------------------------------------------------------- 用户模式
user_paths() {
  CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/liuguang"
  CONFIG_FILE="$CONFIG_DIR/config.toml"
  UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  UNIT_FILE="$UNIT_DIR/$UNIT_NAME"
  STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/liuguang"
  CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/liuguang"
  RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/liuguang"
  PICTURES="$HOME/Pictures"
  # 媒体库统一到一个目录（图片和视频都放这里）。`slideshow.directory` 只扫一层，
  # 指向 $PICTURES 根目录会一张都扫不到 —— 用户会看到"服务在跑但屏幕没变"。
  MEDIA_DIR="$PICTURES/Wallpapers"
  for cand in "$PICTURES/Wallpapers" "$PICTURES/wallpapers" "$PICTURES/Wallpaper"; do
    if [ -d "$cand" ]; then
      MEDIA_DIR="$cand"
      break
    fi
  done
}

user_check() {
  local fail=0
  user_paths
  say "== 体检：用户会话（桌面壁纸层）=="
  [ -n "${WAYLAND_DISPLAY:-}" ] && ok "WAYLAND_DISPLAY=$WAYLAND_DISPLAY" || { bad "WAYLAND_DISPLAY 为空：当前不在 Wayland 会话里"; fail=1; }
  [ -n "${XDG_RUNTIME_DIR:-}" ] && ok "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" || { bad "XDG_RUNTIME_DIR 为空：不是标准登录会话"; fail=1; }
  # 壁纸层依赖 wlr-layer-shell；GNOME/KDE 不实现，提前说清楚比事后当 bug 好。
  case "${XDG_CURRENT_DESKTOP:-}" in
    *GNOME*|*KDE*|*Plasma*)
      bad "当前桌面 ${XDG_CURRENT_DESKTOP} 不支持 wlr-layer-shell（需要 niri / sway / Hyprland 等）"
      fail=1
      ;;
    *) [ -n "${XDG_CURRENT_DESKTOP:-}" ] && ok "桌面：$XDG_CURRENT_DESKTOP" || note "XDG_CURRENT_DESKTOP 为空（合成器仍可能是 wlroots 系）" ;;
  esac
  if [ -f "$CONFIG_FILE" ]; then ok "配置已存在：$CONFIG_FILE（不会覆盖）"; else note "配置将生成：$CONFIG_FILE"; fi
  if [ -d "$CACHE_DIR" ]; then ok "缓存目录就绪：$CACHE_DIR"; else note "缓存目录将创建：$CACHE_DIR"; fi
  if systemctl --user is-enabled "$UNIT_NAME" >/dev/null 2>&1; then
    ok "用户单元已启用"
  else
    note "用户单元尚未启用"
  fi
  if systemctl is-enabled "$UNIT_NAME" >/dev/null 2>&1; then
    bad "系统单元 $UNIT_NAME 已启用：两套同时开会抢 DRM master 撕画面，先 sudo systemctl disable --now $UNIT_NAME"
    fail=1
  fi
  return "$fail"
}

user_config_template() {
  cat <<EOF
# 流光（Liuguang）用户会话配置（由 scripts/install.sh --mode user 生成）
# 全部字段说明见仓库 config/example.toml；改完执行：systemctl --user reload liuguang

[display]
backend = "auto"        # auto：有 WAYLAND_DISPLAY 就用桌面壁纸层

[media]
allowed_roots = ["$PICTURES"]
video_vaapi = "auto"    # 需 --features vaapi 构建；auto 会挑与合成器同卡的渲染节点

# 媒体库：**没有这一段，GUI/TUI 的媒体列表永远是空的**
# （list_media 会返回 NOT_CONFIGURED，而守护进程照常运行，很容易误判成"没这功能"）。
[library]
root = "$PICTURES"
image_dirs = ["$MEDIA_DIR"]
video_dirs = ["$MEDIA_DIR"]
recursive = true

[slideshow]
directory = "$MEDIA_DIR"
interval_seconds = 300

[performance]
max_cache_mb = 512      # 4K 大图解码后可能过百 MB，默认 128 只影响预热
pause_on_fullscreen = true
detect_games = true
pause_on_battery = true

# 适配模式要写在具体输出段里（段名 = 合成器报的输出名，见 liuguangctl outputs）。
[outputs."eDP-1"]
fit = "cover"
zoom = 1.0
offset_x = 0.0
offset_y = 0.0
EOF
}

install_user() {
  if [ "$(id -u)" -eq 0 ] && [ "$DRY_RUN" = 0 ]; then
    bad "不要用 sudo 运行 --mode user（用户单元与 ~/.config 属于登录用户自己）"
    say "  请直接执行：$0 --mode user"
    exit 2
  fi
  user_paths
  say "== 环境与权限检查 =="
  if [ "$DRY_RUN" = 0 ]; then
    ensure_user_bins_in_path || true
  fi
  local fail=0
  for bin in liuguang liuguangctl; do
    if command -v "$bin" >/dev/null 2>&1; then ok "$bin → $(command -v "$bin")"; else bad "找不到 $bin"; fail=1; fi
  done
  if [ -z "${WAYLAND_DISPLAY:-}" ]; then bad "WAYLAND_DISPLAY 为空：当前不在 Wayland 会话里"; fail=1; else ok "WAYLAND_DISPLAY=$WAYLAND_DISPLAY"; fi
  if [ -z "${XDG_RUNTIME_DIR:-}" ]; then bad "XDG_RUNTIME_DIR 为空"; fail=1; else ok "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"; fi
  case "${XDG_CURRENT_DESKTOP:-}" in
    *GNOME*|*KDE*|*Plasma*)
      bad "${XDG_CURRENT_DESKTOP} 不支持 wlr-layer-shell：流光的壁纸层需要 niri / sway / Hyprland"
      fail=1 ;;
  esac
  if systemctl is-enabled "$UNIT_NAME" >/dev/null 2>&1; then
    note "检测到系统单元 $UNIT_NAME 已启用，桌面会话中需先停用以免抢占 DRM master"
    if ask_confirm "是否请求管理员权限停用系统单元 $UNIT_NAME？"; then
      run_as_root "停用与桌面会话互斥的系统服务 $UNIT_NAME" systemctl disable --now "$UNIT_NAME" || fail=1
    else
      bad "系统单元已启用，两套会互相抢 DRM master：先 sudo systemctl disable --now $UNIT_NAME"
      fail=1
    fi
  fi
  [ "$fail" = 0 ] || { say; say "环境检查未通过，什么都没改。"; exit 1; }

  local bin
  bin="$(command -v liuguang 2>/dev/null || echo /usr/bin/liuguang)"

  check_dri_permissions

  say
  say "== 目录与配置 =="
  # 先行建好配置、状态、缓存与壁纸目录并设安全权限（避免首次启动 ProtectHome=read-only 下缺路径）。
  if [ "$DRY_RUN" = 0 ]; then
    mkdir -p "$CONFIG_DIR" "$STATE_DIR" "$CACHE_DIR" "$MEDIA_DIR"
    chmod 700 "$STATE_DIR" "$CACHE_DIR"
  else
    run mkdir -p "$CONFIG_DIR" "$STATE_DIR" "$CACHE_DIR" "$MEDIA_DIR"
  fi
  ok "目录就绪：$CACHE_DIR（缓存）/ $MEDIA_DIR（媒体库）"

  if [ -f "$CONFIG_FILE" ]; then
    note "已存在，不覆盖：$CONFIG_FILE"
  else
    note "生成 $CONFIG_FILE（媒体目录 $MEDIA_DIR）"
    if [ "$DRY_RUN" = 0 ]; then
      user_config_template > "$CONFIG_FILE"
      chmod 600 "$CONFIG_FILE"
    fi
  fi

  say
  say "== systemd 用户单元 =="
  # 优先用**包内**单元（随包升级）；没有包装才从仓库复制到用户目录并写入真实二进制路径。
  local packaged="/usr/lib/systemd/user/$UNIT_NAME"
  if [ -f "$packaged" ]; then
    ok "使用包内单元：$packaged"
    if [ -f "$UNIT_FILE" ]; then
      # 用户目录的副本会遮蔽包内单元，导致升级后行为不一致。
      if cmp -s "$packaged" "$UNIT_FILE"; then
        note "用户目录里有一份相同的副本，删除以免遮蔽包内单元"
        run rm -f "$UNIT_FILE"
      else
        note "用户目录里的副本与包内不同，保留并备份为 $UNIT_FILE.bak"
        run cp -f "$UNIT_FILE" "$UNIT_FILE.bak"
        run rm -f "$UNIT_FILE"
      fi
    fi
  else
    local src="$REPO/packaging/liuguang-user.service"
    if [ ! -f "$src" ]; then bad "找不到 $src（从仓库根目录运行，或先安装发行版包）"; exit 1; fi
    note "安装到 $UNIT_FILE（ExecStart=$bin）"
    if [ "$DRY_RUN" = 0 ]; then
      mkdir -p "$UNIT_DIR"
      sed "s#^ExecStart=[^ ]*#ExecStart=$bin#" "$src" > "$UNIT_FILE"
      chmod 644 "$UNIT_FILE"
    fi
  fi
  # 确保用户 systemd 实例能拿到当前 Wayland 会话环境（首次登录刚进会话时常见问题）。
  if [ "$DRY_RUN" = 0 ]; then
    systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR 2>/dev/null || true
  else
    run systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR
  fi
  run systemctl --user daemon-reload
  run systemctl --user enable --now "$UNIT_NAME"

  say
  say "== 自检 =="
  if [ "$DRY_RUN" = 1 ]; then
    note "dry-run 跳过启动后自检"
  else
    # enable --now 之后给它 10 秒把合成器连接与首帧走完；连不上就把诊断方向说清楚。
    local i=0
    while [ "$i" -lt 20 ]; do
      if liuguangctl status >/dev/null 2>&1; then
        ok "守护进程已响应（$(liuguangctl status 2>/dev/null | head -1)）"
        break
      fi
      i=$((i + 1))
      sleep 0.5
    done
    if [ "$i" -ge 20 ]; then
      bad "10 秒内没连上：看 journalctl --user -u $UNIT_NAME -n 50"
      exit 1
    fi
    liuguangctl status 2>/dev/null | sed 's/^/    /' || true
  fi

  say
  say "== 就绪 =="
  say "  放壁纸：把图片/视频放进 $MEDIA_DIR"
  say "  设壁纸：liuguangctl set $MEDIA_DIR/<文件>"
  say "  终端界面：liuguangctl tui"
  say "  图形界面：liuguang-gui（liuguang-gui 包提供）"
  say "  看日志：journalctl --user -u $UNIT_NAME -f"
  say "  卸载：  $0 --mode user --uninstall"
}

uninstall_user() {
  user_paths
  say "== 卸载用户单元 =="
  run systemctl --user disable --now "$UNIT_NAME" || true
  run rm -f "$UNIT_FILE"
  run systemctl --user daemon-reload || true
  say
  say "已移除 systemd 单元。**配置与状态保留**，需要一并清理时："
  say "  rm -rf '$CONFIG_DIR' '$STATE_DIR' '$CACHE_DIR' '$RUNTIME_DIR'"
}

# ---------------------------------------------------------------- 系统模式
system_check() {
  local fail=0
  say "== 体检：无头形态（系统服务 + DRM/KMS 直出）=="
  if [ "$(id -u)" -eq 0 ]; then
    ok "以 root 运行"
  else
    note "未以 root 运行（实际执行 '--mode system' 时会自动通过 sudo/pkexec 请求授权）"
  fi
  if getent passwd liuguang >/dev/null 2>&1; then ok "用户 liuguang 已存在"; else note "将创建系统用户 liuguang"; fi
  if getent group liuguang-ctl >/dev/null 2>&1; then ok "组 liuguang-ctl 已存在"; else note "将创建组 liuguang-ctl"; fi
  if [ -f /etc/liuguang/config.toml ]; then ok "配置已存在：/etc/liuguang/config.toml"; else note "将生成 /etc/liuguang/config.toml"; fi
  local nodes
  nodes=$(ls /dev/dri 2>/dev/null | tr '\n' ' ' || true)
  [ -n "$nodes" ] && ok "/dev/dri: $nodes" || { bad "没有 /dev/dri：无头直出没有设备可用"; fail=1; }
  if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    if systemctl is-enabled "$UNIT_NAME" >/dev/null 2>&1; then
      note "系统单元当前已启用"
    fi
    local user
    user="$SUDO_USER"
    if [ -d "/home/$user" ] && command -v sudo >/dev/null 2>&1; then
      note "若 $user 同时在桌面用壁纸层，请只保留一种形态"
    fi
  fi
  return "$fail"
}

system_config_template() {
  cat <<'EOF'
# 流光（Liuguang）系统服务配置（由 scripts/install.sh --mode system 生成）
# 无头形态：直接用 DRM/KMS 输出，不需要合成器。

[display]
backend = "drm"
# device = "/dev/dri/card0"   # 留空则自动挑第一个有已连接输出的卡

[media]
allowed_roots = ["/srv/wallpapers"]

[library]
root = "/srv/wallpapers"
image_dirs = ["/srv/wallpapers/images"]
video_dirs = ["/srv/wallpapers/videos"]
recursive = true

[slideshow]
directory = "/srv/wallpapers/images"
interval_seconds = 300
EOF
}

escalate_system_if_needed() {
  local action_desc="$1"
  if [ "$(id -u)" -eq 0 ] || [ "$DRY_RUN" = 1 ]; then
    return 0
  fi
  say "系统模式（$action_desc）需要管理员权限以创建系统用户、写入 /etc/liuguang 与 /usr/lib/systemd/system。"
  if command -v sudo >/dev/null 2>&1 && { [ -t 0 ] || sudo -n true >/dev/null 2>&1; }; then
    exec sudo -- "$0" "${ORIG_ARGS[@]}"
  elif [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v pkexec >/dev/null 2>&1; then
    exec pkexec "$0" "${ORIG_ARGS[@]}"
  else
    bad "无法自动请求管理员权限（非交互终端且未配置免密 sudo）。"
    say "  请以管理员身份执行：sudo $0 ${ORIG_ARGS[*]}"
    exit 1
  fi
}

install_system() {
  escalate_system_if_needed "安装系统服务"
  local fail=0
  say "== 环境检查 =="
  if [ "$DRY_RUN" = 1 ] && [ "$(id -u)" -ne 0 ]; then
    note "[dry-run] 真实执行时将自动通过 sudo/pkexec 请求管理员权限"
  else
    [ "$(id -u)" -eq 0 ] && ok "以 root 运行" || { bad "系统模式需要 root"; exit 1; }
  fi
  for bin in liuguang liuguangctl; do
    if command -v "$bin" >/dev/null 2>&1; then ok "$bin → $(command -v "$bin")"; else bad "找不到 $bin"; fail=1; fi
  done
  [ "$fail" = 0 ] || { say; say "环境检查未通过，什么都没改。"; exit 1; }

  local bin
  bin="$(command -v liuguang)"

  say
  say "== 用户与组（幂等）=="
  if getent group liuguang-ctl >/dev/null 2>&1; then
    note "组 liuguang-ctl 已存在"
  else
    note "创建组 liuguang-ctl"
    run groupadd --system liuguang-ctl
  fi
  if getent passwd liuguang >/dev/null 2>&1; then
    note "用户 liuguang 已存在"
  else
    note "创建系统用户 liuguang（home=/var/lib/liuguang，不可登录）"
    run useradd --system --gid liuguang-ctl --home-dir /var/lib/liuguang \
      --no-create-home --shell /usr/sbin/nologin liuguang
  fi
  # DRM 与 VA-API 节点通常分别属于 video 与 render 组，有则加入。
  for gpu_grp in video render; do
    if getent group "$gpu_grp" >/dev/null 2>&1; then
      run usermod -aG "$gpu_grp" liuguang
      ok "liuguang 已加入 $gpu_grp 组"
    else
      note "系统没有 $gpu_grp 组，跳过"
    fi
  done
  # 让调用者的普通用户也能用 liuguangctl 连系统服务（socket 属组是 liuguang-ctl）。
  local target_user="${SUDO_USER:-}"
  if [ -n "$target_user" ] && [ "$target_user" != root ] && id "$target_user" >/dev/null 2>&1; then
    run usermod -aG liuguang-ctl "$target_user"
    ok "已把 $target_user 加入 liuguang-ctl（重新登录后生效）"
  fi

  say
  say "== 目录 =="
  local tmpfiles_src="$REPO/packaging/liuguang.tmpfiles"
  local tmpfiles_dst="/usr/lib/tmpfiles.d/liuguang.conf"
  if [ -f "$tmpfiles_dst" ]; then
    ok "tmpfiles 已存在：$tmpfiles_dst"
  elif [ -f "$tmpfiles_src" ]; then
    note "安装 $tmpfiles_dst"
    run install -Dm644 "$tmpfiles_src" "$tmpfiles_dst"
  fi
  if [ -f "$tmpfiles_dst" ] && [ "$DRY_RUN" = 0 ]; then
    run systemd-tmpfiles --create "$tmpfiles_dst"
  else
    # 没有 tmpfiles 定义（或 dry-run）时手工建（与 tmpfiles 里的权限一致：组可写 + setgid）。
    run install -d -m 2770 -o liuguang -g liuguang-ctl /srv/wallpapers
    run install -d -m 2770 -o liuguang -g liuguang-ctl /srv/wallpapers/images
    run install -d -m 2770 -o liuguang -g liuguang-ctl /srv/wallpapers/videos
  fi
  ok "/srv/wallpapers 就绪"

  say
  say "== 配置 =="
  if [ -f /etc/liuguang/config.toml ]; then
    note "已存在，不覆盖：/etc/liuguang/config.toml"
  else
    note "生成 /etc/liuguang/config.toml"
    if [ "$DRY_RUN" = 0 ]; then
      mkdir -p /etc/liuguang
      system_config_template > /etc/liuguang/config.toml
      chmod 644 /etc/liuguang/config.toml
    fi
  fi

  say
  say "== systemd 系统单元 =="
  local unit_dst="/usr/lib/systemd/system/$UNIT_NAME"
  if [ -f "$unit_dst" ]; then
    ok "单元已存在：$unit_dst"
  else
    local unit_src="$REPO/packaging/liuguang.service"
    if [ ! -f "$unit_src" ]; then bad "找不到 $unit_src（从仓库根目录运行，或先安装发行版包）"; exit 1; fi
    note "安装 $unit_dst（ExecStart=$bin）"
    if [ "$DRY_RUN" = 0 ]; then
      mkdir -p "$(dirname "$unit_dst")"
      # 只替换可执行文件第一段，保留 --config / --socket / --state-file 参数！
      sed "s#^ExecStart=[^ ]*#ExecStart=$bin#" "$unit_src" > "$unit_dst"
      chmod 644 "$unit_dst"
    fi
  fi
  # sysusers 只在包没提供时才补：它和上面 useradd 等价，装了也无害（幂等）。
  if [ ! -f /usr/lib/sysusers.d/liuguang.conf ] && [ -f "$REPO/packaging/liuguang.sysusers" ]; then
    note "安装 /usr/lib/sysusers.d/liuguang.conf"
    run install -Dm644 "$REPO/packaging/liuguang.sysusers" /usr/lib/sysusers.d/liuguang.conf
  fi
  run systemctl daemon-reload
  run systemctl enable --now "$UNIT_NAME"

  say
  say "== 自检 =="
  if [ "$DRY_RUN" = 1 ]; then
    note "dry-run 跳过启动后自检"
  else
    local i=0
    while [ "$i" -lt 20 ]; do
      if liuguangctl status >/dev/null 2>&1; then
        ok "守护进程已响应"
        break
      fi
      i=$((i + 1))
      sleep 0.5
    done
    if [ "$i" -ge 20 ]; then
      bad "10 秒内没连上：看 journalctl -u $UNIT_NAME -n 50"
      exit 1
    fi
    liuguangctl status 2>/dev/null | sed 's/^/    /' || true
  fi

  say
  say "== 就绪 =="
  say "  放壁纸：把图片/视频放进 /srv/wallpapers/images 与 /srv/wallpapers/videos"
  say "  设壁纸：sudo liuguangctl set /srv/wallpapers/images/<文件>"
  say "  终端界面：liuguangctl --persona system tui"
  say "  看日志：journalctl -u $UNIT_NAME -f"
  say "  卸载：  sudo $0 --mode system --uninstall"
  if [ -n "${SUDO_USER:-}" ]; then
    say "  提示：$SUDO_USER 已加入 liuguang-ctl，**重新登录**后免 sudo 用 liuguangctl"
  fi
}

uninstall_system() {
  escalate_system_if_needed "卸载系统服务"
  if [ "$DRY_RUN" = 0 ] && [ "$(id -u)" -ne 0 ]; then bad "卸载系统单元需要 root"; exit 1; fi
  say "== 卸载系统单元 =="
  run systemctl disable --now "$UNIT_NAME" || true
  run rm -f "/usr/lib/systemd/system/$UNIT_NAME"
  run systemctl daemon-reload || true
  say
  say "已移除单元。**用户、组、配置、媒体都保留**，需要彻底清理时："
  say "  rm -rf /etc/liuguang /var/lib/liuguang /srv/wallpapers"
  say "  userdel liuguang; groupdel liuguang-ctl"
}

# ---------------------------------------------------------------- 入口
if [ "$CHECK_ONLY" = 1 ]; then
  diagnose_common || true
  say
  if [ "$MODE" = "system" ]; then system_check || true; else user_check || true; fi
  say
  say "（--check 只读，什么都没改）"
  exit 0
fi

if [ -z "$MODE" ]; then
  if [ -n "${WAYLAND_DISPLAY:-}" ] && [ -d "$HOME" ]; then
    MODE="user"
    say "未指定 --mode，按环境判为 user（有 WAYLAND_DISPLAY）。要用无头形态请显式 --mode system"
  else
    say "未指定 --mode：当前不在 Wayland 会话里，请显式选一种："
    say "  $0 --mode system   # 无头（DRM/KMS 直出，自动请求权限）"
    say "  $0 --mode user     # 桌面（Wallpaper 层）"
    exit 2
  fi
fi

case "$MODE" in
  user)
    if [ "$UNINSTALL" = 1 ]; then uninstall_user; else install_user; fi
    ;;
  system)
    if [ "$UNINSTALL" = 1 ]; then uninstall_system; else install_system; fi
    ;;
  *)
    echo "--mode 只能是 user 或 system（收到 '$MODE'）" >&2
    exit 2
    ;;
esac
