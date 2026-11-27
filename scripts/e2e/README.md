# 端到端验收脚本（E2E）

本目录收录用于真实二进制、真实 Unix Socket 或 Wayland 合成器环境的端到端测试与基准脚本。这些脚本独立于 `./scripts/ci.sh` 默认门禁，按需在本地桌面或测试机上执行。

| 脚本 | 用途 | 前置条件 |
|---|---|---|
| `headless_ipc.py` | 无显示器（`HeadlessReady`）下 IPC 正向/负向矩阵测试 | daemon 在跑，且当前无已连接输出 |
| `tui_pty.py` | 在 PTY 中启动 `liuguangctl tui`，逐页检查渲染与 `Q` 退出 | daemon 在跑；`liuguangctl` 在 PATH 或由 `LIUGUANGCTL` 指定 |
| `mock_e2e.py` | 独立 mock 后端沙箱：验证 `set` → `WALLPAPER_APPLIED` → `pause`/`resume` → `next` | `liuguang` 与 `liuguangctl` 在 PATH 或环境变量指定 |
| `competitor_master.py` | 占住 `/dev/dri/cardN` 的 DRM master，测试 daemon 自愈恢复 | 对目标设备有读写权限（通常需在 `video` 组） |
| `wayland_layer_check.py` | 桌面模式像素验证：确认 layer surface 在分数缩放下铺满全输出无黑边 | 支持 `wlr-layer-shell` 的 Wayland 合成器 + `grim` |
| `user_persona_check.py` | 桌面模式沙箱验证：确认 XDG 配置/状态/socket 路径与退出清理 | Wayland 会话（`WAYLAND_DISPLAY`） |
| `fullscreen_pause_check.py` | 全屏自动暂停与退出恢复端到端检查 | 支持全屏状态或窗口查询的 Wayland 会话 |
| `gui_latency.py` | GUI 连接 RTT、缩略图缓存、应用响应延迟与 CPU 采样 | 运行中的 daemon 与 GTK4 环境 |
| `nested_sway_bench.sh` | 在嵌套 headless sway 中对比 VA-API DMA-BUF 与 `LIUGUANG_VIDEO_DMABUF=off` 的 CPU 占用 | `sway`、Mesa、release 构建产物 |

## 常用示例

```bash
# 1) mock 后端完整状态流转测试（不接触 DRM 与系统配置）
LIUGUANG=./target/release/liuguang LIUGUANGCTL=./target/release/liuguangctl \
  python3 scripts/e2e/mock_e2e.py

# 2) 用户会话人格（临时 XDG 沙箱，不污染 ~/.config）
LIUGUANG=./target/release/liuguang LIUGUANGCTL=./target/release/liuguangctl \
  python3 scripts/e2e/user_persona_check.py

# 3) 桌面层像素铺满校验 + 内置合成自检
python3 scripts/e2e/wayland_layer_check.py --self-check
LIUGUANG=./target/release/liuguang LIUGUANGCTL=./target/release/liuguangctl \
  python3 scripts/e2e/wayland_layer_check.py --switch-workspace

# 4) headless sway 零拷贝基准对照
LIUGUANG=./target/release/liuguang LIUGUANGCTL=./target/release/liuguangctl \
  scripts/e2e/nested_sway_bench.sh --media /path/to/clip.mp4 --seconds 15
```

## 常用环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `LIUGUANG_SOCKET` | 自动按 `--persona` / 环境探测 | 指定控制 socket 路径 |
| `LIUGUANG` | `liuguang` | daemon 二进制路径 |
| `LIUGUANGCTL` | `liuguangctl` | CLI/TUI 二进制路径 |
| `LIUGUANG_OUTPUT` | 首个已连接输出 | `wayland_layer_check.py` 检查的目标输出名 |
| `LIUGUANG_CARD` | `/dev/dri/card1` | `competitor_master.py` 占用的 DRM 设备 |
