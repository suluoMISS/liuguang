# 配置参考

流光使用 TOML 格式配置文件，默认路径取决于运行形态（参见 [DEPLOYMENT.md](DEPLOYMENT.md)）：

| 形态 | 配置文件 | 状态文件 | 控制 Socket |
|---|---|---|---|
| **桌面模式（`user`）** | `$XDG_CONFIG_HOME/liuguang/config.toml` | `$XDG_STATE_HOME/liuguang/state.json` | `$XDG_RUNTIME_DIR/liuguang/control.sock` |
| **无桌面模式（`system`）** | `/etc/liuguang/config.toml` | `/var/lib/liuguang/state.json` | `/run/liuguang/control.sock` |

修改后可用守护进程离线校验配置文件合法性：

```bash
liuguang check-config ~/.config/liuguang/config.toml
```

完整带注释示例见 [`config/example.toml`](../config/example.toml)。配置生效优先级为：`命令行参数 > 环境变量 > 配置文件 > 默认值`。GUI/TUI 或 IPC 产生的运行时修改会在确认保存后保留原注释与格式原子写回配置。

---

## `[display]`

| 键 | 默认值 | 说明 |
|---|---|---|
| `backend` | `"auto"` | `auto` / `wayland` / `drm` / `mock`；`auto` 检测到 `WAYLAND_DISPLAY` 优先走桌面层 |
| `device` | `"auto"` | 仅 `drm` 后端有效，指定 `/dev/dri/cardN` 或 `"auto"` 自动选卡 |
| `mode` | `"preferred"` | `preferred` 或 `宽x高[@刷新率]`（Wayland 下跟随合成器当前输出模式） |
| `vsync` | `true` | `drm` 后端启用垂直同步阻塞 flip |

## `[media]`

| 键 | 默认值 | 说明 |
|---|---|---|
| `hardware_decode` | `"auto"` | `auto` / `required` / `disabled` |
| `preferred_decoder` | `"vaapi"` | 解码器策略标识 |
| `video_fps_limit` | `60` | 呈现帧率上限；高刷屏播放 120fps 素材可设为 `120` 或 `0`（不限制） |
| `software_decode_fallback` | `true` | 硬件无法解码时是否允许退回软解（超规格素材建议配合 `transcode`） |
| `video_vaapi` | `"auto"` | `auto` / `off` / `/dev/dri/renderDNN`；`auto` 优先挑选与显示同卡的渲染节点 |
| `video_dmabuf_rgb` | `"auto"` | `auto` / `always` / `never`；合成器仅采样亮度平面时自动开启 GPU NV12→RGB 转换 |
| `transcode` | `"auto"` | `auto` / `off` / ffmpeg 路径；将 8K H.264 / 10bit 转成可硬解缓存 |
| `transcode_max_width` | `0` | 0 为原尺寸；非 0 时等比限宽转码（如 `3840`） |
| `transcode_crf_or_bitrate` | `"hevc_nvenc -b:v 20M"` | 转码编码器与参数（首词为主编码器，不可用时自动退回 `libx265`） |
| `allowed_roots` | `[]` | IPC `set_wallpaper` 允许访问的额外白名单根目录（空 = 继承媒体库目录） |

## `[performance]`

| 键 | 默认值 | 说明 |
|---|---|---|
| `preload_next` | `true` | 轮换模式自动预解码下一张图片（全部输出单曲锁定时自动释放预读缓存） |
| `max_cache_mb` | `128` | 预加载解码缓存预算上限（MiB） |
| `frame_queue_size` | `3` | 视频帧队列深度（1–16） |
| `pause_when_display_off` | `true` | 熄屏时暂停视频供给并亮屏恢复 |
| `pause_on_fullscreen` | `true` | 有全屏窗口时自动暂停（需合成器支持 `zwlr_foreign_toplevel_management`） |
| `reduce_load_on_cpu_pressure` | `true` | 高 CPU 压力（PSI / loadavg）时降频减载并暂停预读 |
| `pause_when_gpu_memory_above` | `85` | 显存占用超过该百分比时自动暂停视频（NVIDIA / AMD sysfs，0 = 关闭） |
| `resume_when_gpu_memory_below` | `75` | 显存回落至该百分比以下自动恢复（需小于暂停阈值） |
| `pause_on_battery` | `true` | 笔记本拔电切换电池供电时自动暂停，接通电源自动恢复 |
| `detect_games` | `true` | 探测游戏进程（Steam/Proton/Wine/Lutris/模拟器）运行期间自动让路 |
| `game_processes` | `[]` | 额外补充的游戏进程名子串（不区分大小写） |
| `game_ignore_processes` | `[]` | 强制排除的非游戏特征，优先级最高（例如 `["electron"]`） |

## `[slideshow]` 与 `[library]`

| 段/键 | 默认值 | 说明 |
|---|---|---|
| `slideshow.enabled` | `true` | 是否按间隔自动轮换（未锁定时生效） |
| `slideshow.directory` | `"/srv/wallpapers"` | 默认轮换目录，同时自动合并为媒体库来源 |
| `slideshow.interval_seconds` | `600` | 轮换时间间隔（秒） |
| `slideshow.order` | `"shuffle"` | `"sequential"` 或 `"shuffle"` |
| `slideshow.avoid_repeat` | `true` | 避免连续两次播放同一媒体 |
| `library.root` | — | **启用媒体库必填**：所有扫描目录均限制在此根路径内 |
| `library.image_dirs` | `[]` | 图片目录列表 |
| `library.video_dirs` | `[]` | 视频目录列表 |
| `library.recursive` | `true` | 是否递归扫描子目录 |
| `library.max_file_bytes` | `2147483648` | 单文件索引上限（默认 2 GiB） |

## `[transition]` 与 `[outputs."<输出名>"]`

| 段/键 | 默认值 | 说明 |
|---|---|---|
| `transition.enabled` | `true` | 是否开启壁纸切换过渡动画 |
| `transition.effect` | `"fade"` | `fade` / `slide` / `wipe` / `circle` |
| `transition.duration_ms` | `350` | 过渡时长（0–10000 毫秒） |
| `transition.easing` | `"ease-in-out"` | `linear` / `ease-in` / `ease-out` / `ease-in-out` |
| `outputs.<name>.fit` | `"cover"` | `cover`（裁切填充）/ `contain`（完整显示）/ `stretch`（拉伸铺满）/ `center`（居中原始） |
| `outputs.<name>.zoom` | `1.0` | 缩放倍数，区间 `(0, 10]` |
| `outputs.<name>.offset_x` / `offset_y` | `0.0` | 画面输出像素偏移 |
| `outputs.<name>.rotation` | `"0"` | `"0"` / `"90"` / `"180"` / `"270"`（顺时针旋转角度） |
| `outputs.<name>.brightness` | `1.0` | 画面亮度倍率 `0.1..=1.0`（10%–100%，实时生效、无闪烁） |
| `outputs.<name>.fallback_image` | 无 | 视频初始化或解码失败时的静态兜底图片 |

## 环境变量覆盖

| 变量名 | 作用 |
|---|---|
| `LIUGUANG_VIDEO_VAAPI` | 覆盖 `[media].video_vaapi`（`auto` / `off` / 渲染节点路径） |
| `LIUGUANG_DMABUF_RGB` | 覆盖 `[media].video_dmabuf_rgb`（`auto` / `always` / `never`） |
| `LIUGUANG_TRANSCODE` | 覆盖 `[media].transcode`（如设为 `off`） |
| `LIUGUANG_VIDEO_PREFETCH` | 预读深度覆盖（`0` 退回同步解码链路） |
| `LIUGUANG_THUMB_WORKERS` | 缩略图工作线程数（默认 2，上限 8） |
| `LIUGUANG_RGB_STATS=1` | 在日志输出 GPU RGB 转换分段耗时 |
| `LIUGUANG_VAAPI_PROFILE=1` | 在 stderr 输出 VA-API 管线冷启动阶段耗时 |
| `LIUGUANG_GUI_DEBUG=1` | 开启 GUI 详细调试日志 |
