#!/usr/bin/env bash
# 零拷贝（DMA-BUF）与 CPU 路径的对照基准：跑在**嵌套的 headless sway** 里。
#
# 为什么需要嵌套合成器：真机桌面（niri）上解码在本机 iGPU、合成器却在另一块 GPU
# 上渲染时，DMA-BUF 直通会被合成器如实拒绝（跨 GPU 无法导入），此时量到的是回退
# 路径的数字。要量零拷贝本身，必须让解码与渲染落在同一块 GPU 上，headless sway
# 正好可以指定渲染器。
#
# 依赖：sway（Arch: `pacman -S sway`）、Mesa、`liuguang`/`liuguangctl`（--release 构建）。
#
# 用法：
#   scripts/e2e/nested_sway_bench.sh --media 1080p.mp4 --media 4k.mp4
#   LIUGUANG=./.work/build/release/liuguang LIUGUANGCTL=./.work/build/release/liuguangctl \
#     scripts/e2e/nested_sway_bench.sh --media clip.mp4 --seconds 15
#
# 混合 GPU 机器（Intel 解码 + NVIDIA 渲染桌面）上，若希望 sway 用 iGPU：
#   sudo unshare -m --propagation private bash -c \
#     'mount --bind /dev/null /dev/dri/card2; mount --bind /dev/null /dev/dri/renderD129;
#      exec runuser -u "$USER" -- scripts/e2e/nested_sway_bench.sh --media clip.mp4'
#   并把 __EGL_VENDOR_LIBRARY_FILENAMES 限制为 Mesa，避免 wlroots 选中打不开的 GPU。
set -euo pipefail

LIUGUANG=${LIUGUANG:-./target/release/liuguang}
LIUGUANGCTL=${LIUGUANGCTL:-./target/release/liuguangctl}
SECONDS_PER_CASE=${SECONDS:-15}
MEDIA=()
OUT=${OUT:-.work/nested-bench}
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --media) MEDIA+=("$2"); shift 2 ;;
    --seconds) SECONDS_PER_CASE="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

[ ${#MEDIA[@]} -gt 0 ] || { echo "至少要一个 --media" >&2; exit 2; }
for m in "${MEDIA[@]}"; do [ -f "$m" ] || { echo "找不到媒体: $m" >&2; exit 2; }; done
command -v sway >/dev/null || { echo "缺少 sway（Arch: pacman -S sway）" >&2; exit 3; }
[ -x "$LIUGUANG" ] || { echo "找不到 liuguang: $LIUGUANG" >&2; exit 3; }
[ -x "$LIUGUANGCTL" ] || { echo "找不到 liuguangctl: $LIUGUANGCTL" >&2; exit 3; }

mkdir -p "$OUT/media"
rm -f "$OUT"/*.sock "$OUT"/state.json
for m in "${MEDIA[@]}"; do cp -f "$m" "$OUT/media/"; done

cat > "$OUT/config.toml" <<EOF
[display]
backend = "wayland"

[media]
allowed_roots = ["$PWD/$OUT"]

[outputs."HEADLESS-1"]
fit = "cover"
EOF
cat > "$OUT/sway.conf" <<'EOF'
output HEADLESS-1 mode 2560x1600
EOF

cleanup() {
  [ -n "${DPID:-}" ] && kill "$DPID" 2>/dev/null || true
  [ -n "${SPID:-}" ] && kill "$SPID" 2>/dev/null || true
  [ "$KEEP" = 1 ] || rm -f "$OUT"/*.sock
}
trap cleanup EXIT

: "${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR 未设置（必须在图形会话里跑）}"
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=gles2 \
  sway --unsupported-gpu -c "$OUT/sway.conf" > "$OUT/sway.log" 2>&1 &
SPID=$!
for _ in $(seq 1 60); do
  DISP=$(sed -n "s/.*wayland display '\(wayland-[0-9]*\)'.*/\1/p" "$OUT/sway.log" | head -1)
  [ -n "${DISP:-}" ] && break
  sleep 0.25
done
[ -n "${DISP:-}" ] || { echo "sway 未启动，见 $OUT/sway.log" >&2; exit 4; }
echo "嵌套 sway: $DISP（渲染器: $(grep -o "Using.*renderer" "$OUT/sway.log" | head -1)）"

run_case() {
  local label="$1" envs="$2" file="$3"
  rm -f "$OUT/control.sock" "$OUT/state.json"
  env $envs "$LIUGUANG" --config "$OUT/config.toml" \
      --socket "$OUT/control.sock" --state-file "$OUT/state.json" > "$OUT/$label.log" 2>&1 &
  DPID=$!
  for _ in $(seq 1 40); do [ -S "$OUT/control.sock" ] && break; sleep 0.25; done
  sleep 2
  WAYLAND_DISPLAY="$DISP" "$LIUGUANGCTL" --socket "$OUT/control.sock" set "$PWD/$OUT/media/$(basename "$file")" >/dev/null 2>&1 || true
  sleep 5
  local dec t0 t1
  dec=$(WAYLAND_DISPLAY="$DISP" "$LIUGUANGCTL" --socket "$OUT/control.sock" status 2>/dev/null | sed -n 's/^decoder: *//p')
  t0=$(awk '{print $14+$15}' "/proc/$DPID/stat")
  sleep "$SECONDS_PER_CASE"
  t1=$(awk '{print $14+$15}' "/proc/$DPID/stat")
  printf '%-16s %6s%% 单核  decoder=%s\n' "$label" "$(( (t1-t0)*100/(SECONDS_PER_CASE*100) ))" "${dec:-?}"
  kill "$DPID" 2>/dev/null || true
  wait "$DPID" 2>/dev/null || true
  DPID=
  sleep 1
}

export WAYLAND_DISPLAY="$DISP"
for m in "${MEDIA[@]}"; do
  base=$(basename "$m")
  run_case "dmabuf:$base" "RUST_LOG=warn" "$OUT/media/$base"
  run_case "cpu:$base" "RUST_LOG=warn LIUGUANG_VIDEO_DMABUF=off" "$OUT/media/$base"
done
echo "日志与配置留在 $OUT"
