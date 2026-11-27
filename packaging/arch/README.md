# Arch Linux 打包说明（`makepkg`）

提供两个独立软件包以适应不同部署需求：

| 包名 | 内容 | 说明 |
|---|---|---|
| `liuguang` | `liuguang` 守护进程 + `liuguangctl`（CLI/TUI）、systemd 单元、sysusers、tmpfiles、配置与文档 | 默认编译启用 `vaapi` 零拷贝，系统服务默认 `disable` |
| `liuguang-gui` | GTK4 + libadwaita 图形界面 `liuguang-gui`、桌面项与图标 | 依赖 `liuguang`、`gtk4`、`libadwaita`、`gdk-pixbuf2` |

## 构建与安装

```bash
# 1. 构建主包与 GUI 包
(cd packaging/arch     && BUILDDIR="$PWD/../../.work/archpkg"     PKGDEST="$PWD/../../dist/arch" makepkg -f)
(cd packaging/arch/gui && BUILDDIR="$PWD/../../../.work/archpkg-gui" PKGDEST="$PWD/../../../dist/arch" makepkg -f)

# 2. 本地安装
sudo pacman -U dist/arch/liuguang-*.pkg.tar.zst dist/arch/liuguang-gui-*.pkg.tar.zst

# 3. 按当前形态启用服务（二选一）：
# 桌面形态：
/usr/share/doc/liuguang/install-user-session.sh
# 无头展示屏形态：
# sudo systemctl enable --now liuguang && sudo usermod -aG liuguang-ctl "$USER"
```

## 卸载

```bash
sudo pacman -Rns liuguang-gui liuguang
```

配置文件 `/etc/liuguang/`、`~/.config/liuguang/` 与壁纸目录 `/srv/wallpapers/` 默认保留。如需彻底清理系统账号可运行 `sudo userdel liuguang; sudo groupdel liuguang-ctl`。
