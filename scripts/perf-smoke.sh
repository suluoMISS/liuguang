#!/usr/bin/env bash
# 性能冒烟/soak：mock 后端 + 周期 IPC 请求，采样 RSS/CPU。
# 用法：scripts/perf-smoke.sh [秒数]（默认 60；M6 记录用 600）
set -euo pipefail
cd "$(dirname "$0")/.."
DURATION="${1:-60}"
DIR="$PWD/.e2e/soak"
rm -rf "$DIR"; mkdir -p "$DIR/media"
python3 - "$DIR/media/a.png" <<'PY'
import struct, zlib, sys
def chunk(t, d):
    c = struct.pack('>I', len(d)) + t + d
    return c + struct.pack('>I', zlib.crc32(t + d) & 0xffffffff)
ihdr = struct.pack('>IIBBBBB', 2, 2, 8, 6, 0, 0, 0)
raw = b''.join(b'\x00' + bytes([255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,255,255][y*8:(y+1)*8]) for y in range(2))
png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', ihdr) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')
open(sys.argv[1],'wb').write(png)
PY
cp "$DIR/media/a.png" "$DIR/media/b.png"
cat > "$DIR/config.toml" <<CFG
[media]
allowed_roots = ["$DIR/media"]
[slideshow]
directory = "$DIR/media"
interval_seconds = 30
CFG
./target/debug/liuguang --config "$DIR/config.toml" --socket "$DIR/sock" \
    --state-file "$DIR/state.json" --backend mock > "$DIR/daemon.log" 2>&1 &
DPID=$!
sleep 1
CTL="./target/debug/liuguangctl --socket $DIR/sock"
REQS=0
START=$(date +%s)
echo "ts_s,rss_kb,requests" > "$DIR/samples.csv"
while :; do
  NOW=$(( $(date +%s) - START ))
  [ "$NOW" -ge "$DURATION" ] && break
  $CTL status >/dev/null 2>&1 && $CTL next --output HDMI-A-1 >/dev/null 2>&1 || true
  REQS=$((REQS+2))
  RSS=$(awk '/VmRSS/{print $2}' "/proc/$DPID/status" 2>/dev/null || echo 0)
  echo "$NOW,$RSS,$REQS" >> "$DIR/samples.csv"
  sleep 2
done
kill -TERM $DPID; wait $DPID 2>/dev/null || true
echo "requests_total=$REQS duration_s=$DURATION"
head -3 "$DIR/samples.csv"; echo ...; tail -3 "$DIR/samples.csv"
RSS_MIN=$(awk -F, 'NR>1{print $2}' "$DIR/samples.csv" | sort -n | head -1)
RSS_MAX=$(awk -F, 'NR>1{print $2}' "$DIR/samples.csv" | sort -n | tail -1)
echo "rss_kb_min=$RSS_MIN rss_kb_max=$RSS_MAX"
grep -c "已退出" "$DIR/daemon.log" || true
