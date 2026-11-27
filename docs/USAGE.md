# 使用指南

安装完成后可直接使用以下三个命令：

| 命令 | 定位 | 说明 |
|---|---|---|
| `liuguang` | 核心守护进程 | 建议由 `systemd` 托管运行；也支持 `check-config` / `show` 离线工具 |
| `liuguangctl` | CLI + TUI 控制端 | 支持本机与通过 SSH 远程管理 |
| `liuguang-gui` | GTK4 图形界面 | 曜晶流光画报墙与可视化控制中心（详见 [GUI.md](GUI.md)） |

---

## 1. 常用 CLI 操作

```bash
# 查看当前运行状态、解码路径、输出通道及暂停原因
liuguangctl status

# 查看显示输出名（配置 [outputs."<名>"] 时需使用这里报出的真实输出名）
liuguangctl outputs

# 应用视频壁纸并锁定单曲循环（--pin）
liuguangctl set ~/Pictures/Wallpapers/loop.mp4 --pin

# 应用图片壁纸（可同时指定适配参数）
liuguangctl set ~/Pictures/Wallpapers/scenery.png --fit cover --zoom 1.0

# 解除单曲锁定 / 手动切换下一张
liuguangctl pin off
liuguangctl next

# 手动暂停 / 恢复播放（暂停原因记为 user）
liuguangctl pause
liuguangctl resume

# 设置切换转场效果与时长（毫秒）
liuguangctl transition slide --duration 600 --easing ease-in-out

# 热重载配置文件（不中断播放）
liuguangctl reload

# 启动远程终端控制台（TUI）
liuguangctl tui
```

### 控制特定形态或特定输出

- 多显示器独立控制：`liuguangctl set a.mp4 --output DP-1`、`liuguangctl set b.png --output HDMI-A-1`
- 管理**无桌面系统服务**：在命令前附加 `--persona system`（如 `liuguangctl --persona system status`）
- 手工指定 Socket：`liuguangctl --socket /run/liuguang/control.sock status`

### 退出码约定

| 退出码 | 含义 |
|---|---|
| `0` | 命令成功执行 |
| `1` | 守护进程拒绝请求或业务错误（文件不存在、需转码、无权限等） |
| `2` | 命令行参数格式/取值越界（由 clap 直接校验拦截） |
| `3` | 无法连接 `liuguang` socket（服务未启动或缺少 `liuguang-ctl` 组权限） |

---

## 2. 守护进程独立工具命令

```bash
# 离线校验 TOML 配置合法性
liuguang check-config ~/.config/liuguang/config.toml

# 脱离守护进程单独在屏幕上点亮一张 PNG（Ctrl+C 退出并释放设备）
liuguang show /path/to/test.png
```

---

## 3. 解读 `liuguangctl status`

```text
state:      VideoPlaying
output:     eDP-1
media:      /home/user/Pictures/Wallpapers/demo.mp4
mode:       2560x1600@165.002
paused:     false
backend:    wayland
decoder:    vaapi:nvdec(dmabuf-rgb)
channel[eDP-1]: state=VideoPlaying media=... decoder=vaapi:nvdec(dmabuf-rgb) paused=false pinned=true
  video: presented=32815 dropped_late=0 dropped_queue=0 queue_len=2
  present: ok=365539 failures=0 consecutive_failures=0
  dmabuf:  is_dmabuf=true fallbacks=0 keepalive_len=0
uptime:     6128s
```

- **`decoder`**：
  - `vaapi:nvdec(dmabuf)` / `vaapi:vah264dec(dmabuf)`：零拷贝 DMA-BUF 直通；
  - `vaapi:nvdec(dmabuf-rgb)`：GPU 端完成 NV12→RGB 后仍保持零拷贝；
  - `software:...(rgba)`：CPU 共享内存回退路径；
  - `static` / `none`：静态图片展示（此时不占用视频解码器与高频轮询）。
- **`paused_by`**：展示当前触发暂停的所有原因（例如 `battery`、`fullscreen`、`game_running`、`gpu_memory`、`screen_off`、`user`）。

---

## 4. 常见问题

| 现象 | 原因 | 处理方式 |
|---|---|---|
| 返回码 `3` 连不上服务 | 服务未起或不在 `liuguang-ctl` 组 | 检查 `systemctl --user status liuguang` 或系统模式加组后重登 |
| 媒体列表返回 `NOT_CONFIGURED` | 配置缺少 `[library]` 段 | 补全 `[library] root = "..."` 或重跑 `./scripts/install.sh --mode user` |
| 视频应用返回 `NEEDS_PREPARATION` | 超出硬件解码规格（8K H.264 / 10bit） | 在 GUI 点击「准备」或等待自动转码完成通知 `MEDIA_PREPARED` |
| 壁纸整屏发红 | 合成器只读取多平面 YUV 的 Y 通道 | 确保 `[media] video_dmabuf_rgb = "auto"`（或设为 `"always"`） |
