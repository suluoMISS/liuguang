#!/usr/bin/env bash
# 构建 GTK4 + libadwaita 图形界面（apps/liuguang-gui）。
#
# 为什么单独一个脚本：GUI 不在 cargo workspace 里（GTK4 / libadwaita 需要系统
# 开发包），而守护进程 / CLI / CI 必须能在**没有 GTK 的服务器**上构建。
# 所以它有自己的 Cargo.toml 与目标目录，构建入口也独立。
#
# 系统依赖：gtk4 >= 4.12、libadwaita-1 >= 1.5（crate 侧锁定 libadwaita 0.8 / v1_5）。
#
# 用法：
#   bash scripts/build-gui.sh                # 构建（release 加 --release）
#   bash scripts/build-gui.sh --release
#   bash scripts/build-gui.sh --check        # 只做依赖检查，不编译
#   bash scripts/build-gui.sh --release --install   # 装到 ~/.local/bin + 桌面项
#
# 产物：.work/build-gui/{debug,release}/liuguang-gui
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
profile=debug
check_only=0
do_install=0
for arg in "$@"; do
    case "$arg" in
        --release) profile=release ;;
        --check) check_only=1 ;;
        --install) do_install=1 ;;
        -h|--help) sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "未知参数：$arg" >&2; exit 2 ;;
    esac
done

# 1) 系统依赖：GTK4 开发包（pkg-config 能报版本就说明头文件可用）
if ! command -v pkg-config >/dev/null 2>&1; then
    echo "缺少 pkg-config；请先安装（Fedora: sudo dnf install pkgconf-pkg-config）" >&2
    exit 1
fi
if ! version=$(pkg-config --modversion gtk4 2>/dev/null); then
    cat >&2 <<'EOF'
缺少 GTK4 开发包（pkg-config 找不到 gtk4）。
  Fedora: sudo dnf install gtk4-devel
  Arch:   sudo pacman -S gtk4
  Debian: sudo apt install libgtk-4-dev
EOF
    exit 1
fi
echo "gtk4 = $version"

# 2) libadwaita 开发包：界面外壳（NavigationSplitView / HeaderBar / ToastOverlay /
#    PreferencesPage）与深色主题都来自它，缺了无法构建。
if ! adw_version=$(pkg-config --modversion libadwaita-1 2>/dev/null); then
    cat >&2 <<'EOF'
缺少 libadwaita 开发包（pkg-config 找不到 libadwaita-1）。
  Fedora: sudo dnf install libadwaita-devel
  Arch:   sudo pacman -S libadwaita
  Debian: sudo apt install libadwaita-1-dev
EOF
    exit 1
fi
echo "libadwaita = $adw_version"
if [ "$check_only" = 1 ]; then
    echo "依赖检查通过（未编译）"
    exit 0
fi

# 3) 构建：CARGO_HOME / CARGO_TARGET_DIR 可外部覆盖（CI 里指向工作区内目录）。
export CARGO_HOME=${CARGO_HOME:-$repo/.cargo-home}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/.work/build-gui}
flags=()
[ "$profile" = release ] && flags+=(--release)
echo "构建 liuguang-gui（profile=$profile，target=$CARGO_TARGET_DIR）"
(cd "$repo/apps/liuguang-gui" && cargo build "${flags[@]}")
echo "完成：$CARGO_TARGET_DIR/$profile/liuguang-gui"

# 4) 可选安装：放进 PATH 并挂一个桌面项，就能从应用菜单启动。
if [ "$do_install" = 1 ]; then
    bin_dir=$HOME/.local/bin
    app_dir=$HOME/.local/share/applications
    mkdir -p "$bin_dir" "$app_dir"
    install -m0755 "$CARGO_TARGET_DIR/$profile/liuguang-gui" "$bin_dir/liuguang-gui"
    # 桌面项的单一来源是 packaging/liuguang-gui.desktop（Arch 包也用它），
    # 这里只做安装 —— 不要再复制一份内容，否则两处必然漂移。
    desktop_src="$repo/packaging/liuguang-gui.desktop"
    if [[ -f "$desktop_src" ]]; then
        install -m 0644 "$desktop_src" "$app_dir/liuguang-gui.desktop"
    else
        echo "警告：找不到 $desktop_src，跳过桌面项" >&2
    fi
    command -v update-desktop-database >/dev/null 2>&1 && \
        update-desktop-database "$app_dir" >/dev/null 2>&1 || true
    # 图标：SVG 是单一来源（packaging/icons/liuguang.svg），PNG 由它渲染。
    icon_dir=$HOME/.local/share/icons/hicolor
    if [[ -f "$repo/packaging/icons/liuguang.svg" ]]; then
        install -Dm0644 "$repo/packaging/icons/liuguang.svg" \
            "$icon_dir/scalable/apps/liuguang.svg"
        for png in "$repo"/packaging/icons/liuguang-*.png; do
            [[ -f "$png" ]] || continue
            size=$(basename "$png" .png); size=${size##*-}
            install -Dm0644 "$png" "$icon_dir/${size}x${size}/apps/liuguang.png"
        done
        command -v gtk-update-icon-cache >/dev/null 2>&1 && \
            gtk-update-icon-cache -f -t "$icon_dir" >/dev/null 2>&1 || true
    else
        echo "警告：找不到 packaging/icons/liuguang.svg，跳过图标" >&2
    fi
    echo "已安装：$bin_dir/liuguang-gui"
    echo "桌面项：$app_dir/liuguang-gui.desktop（应用菜单里搜「流光」）"
    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *) echo "注意：$bin_dir 不在 PATH 里，先把它加进 shell 配置。" ;;
    esac
else
    echo "运行：$CARGO_TARGET_DIR/$profile/liuguang-gui --socket <liuguang 的 socket>"
    echo "（想装进 PATH 与应用菜单：加 --install）"
fi
