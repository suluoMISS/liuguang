# Packaging — 部署物说明

本目录包含面向不同 Linux 发行版与独立安装方式的 systemd 单元、桌面项、图标及打包配置。

> - **Arch Linux（桌面优先）**：见 [`arch/README.md`](arch/README.md)，默认启用 `vaapi` 零拷贝，系统服务预设为 `disable`；
> - **Fedora / RHEL RPM（无头服务器优先）**：见 [`liuguang.spec`](liuguang.spec) 与 [`RPM.md`](RPM.md)。

---

## 目录结构与文件说明

| 文件 | 用途 |
|---|---|
| `liuguang-user.service` | 桌面形态（`user`）systemd 用户级单元（`ProtectHome=read-only` + `CacheDirectory=liuguang` 首启自动授权缓存写入） |
| `liuguang.service` | 无头形态（`system`）systemd 系统级单元（`User=liuguang`、`Group=liuguang-ctl`、`SupplementaryGroups=video render`） |
| `liuguang.sysusers` | 声明系统账号 `liuguang`、控制组 `liuguang-ctl` 以及 `video`/`render` 组成员关系 |
| `liuguang.tmpfiles` | 声明式创建 `/srv/wallpapers/{images,videos}`（`2770 liuguang:liuguang-ctl` setgid） |
| `liuguang.preset` | 系统服务默认预设策略 |
| `liuguang-gui.desktop` | GTK4 图形界面桌面入口（`StartupWMClass=io.github.suluoMISS.Liuguang`） |
| `icons/liuguang.{svg,png}` | 应用矢量图标及多尺寸 PNG（详见 [`icons/README.md`](icons/README.md)） |
| `drm-exclusive.conf.example` | 可选的 DRM master 启动前排他性检查 drop-in 示例 |
| `va-i965-fallback.conf.example` | 老旧 Intel 核显仅支持 `i965` 时的可选 VA-API 回退示例 |
| `liuguang-seat.service` | 专用 VT + logind seat 自动登录引导参考草案 |

---

## 目录与权限约定（无头系统形态）

| 路径 | 用途 | 权限 |
|---|---|---|
| `/etc/liuguang/config.toml` | 主配置（模板见 `../config/example.toml`） | `ConfigurationDirectory=liuguang` |
| `/var/lib/liuguang/state.json` | 运行状态持久化 | `StateDirectory=liuguang`，`0700` |
| `/run/liuguang/control.sock` | IPC 控制 Socket | 目录 `0750 liuguang:liuguang-ctl`，Socket `0660` |
| `/srv/wallpapers/` | 默认壁纸素材库 | `2770 liuguang:liuguang-ctl`（组内成员免 `sudo` 写入） |
