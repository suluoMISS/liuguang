#!/usr/bin/env bash
# 构建 Liuguang 的 RPM（源码包：从 tarball 内 vendored crates 离线编译）
#
# 用法（Fedora 上普通用户即可，不需要 root）：
#   bash scripts/rpm-build.sh            # 生成 tarball + rpmbuild -ba
#   bash scripts/rpm-build.sh --tarball  # 只生成 tarball（可在无 rpmbuild 的机器上跑）
#   bash scripts/rpm-build.sh --srpm     # 只构建 SRPM（投 COPR / mock 用）
#   RPM_TOPDIR=/somewhere bash scripts/rpm-build.sh    # 指定 _topdir
#
# 设计要点（都是踩过的坑）：
#   * 仓库根的 `vendor/` 是 **npm 打包**放的预编译二进制，与 cargo vendor 无关。
#     这里用独立目录 `.rpm-vendor/`，两者互不干扰。
#   * `.cargo/config.toml` 只写进 **打包暂存目录**，绝不写进仓库 ——
#     否则会改掉本机正常开发的构建配置。
#   * 入包内容用 **白名单**，不是黑名单：仓库里有 `视频/`（2.1GB 素材）、`target/`（15GB）、
#     `.audit/`、`.git/` 等，黑名单漏一个就是几个 GB。
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT=$PWD

VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
NAME=liuguang
MODE="${1:-}"

# rpmbuild 的 _topdir 需可写；本项目部署环境的 $HOME 常常只读。
if [ -n "${RPM_TOPDIR:-}" ]; then
    TOPDIR="$RPM_TOPDIR"
elif mkdir -p "$HOME/rpmbuild" 2>/dev/null && [ -w "$HOME/rpmbuild" ]; then
    TOPDIR="$HOME/rpmbuild"
else
    # /tmp 在部分环境里跨调用会被清理（踩过），所以退化到【工作区内】的持久目录。
    TOPDIR="$ROOT/.rpmbuild"
    echo "[rpm-build] \$HOME 不可写 ⇒ 使用 $TOPDIR（RPM_TOPDIR=... 可覆盖）"
fi

echo "[rpm-build] $NAME $VERSION  topdir=$TOPDIR"

# ------------------------------------------------------------ 1. vendor crates
VENDOR_DIR="$ROOT/.rpm-vendor"
if [ ! -d "$VENDOR_DIR" ] || [ -z "$(ls -A "$VENDOR_DIR" 2>/dev/null)" ]; then
    echo "[rpm-build] 生成 .rpm-vendor/（cargo vendor；需要 registry 缓存或外网）"
    VENDOR_ARGS=()
    if [ "${CARGO_NET_OFFLINE:-false}" = "true" ]; then
        VENDOR_ARGS+=(--offline)
    fi
    CARGO_HOME="${CARGO_HOME:-$ROOT/.cargo-home}" cargo vendor "${VENDOR_ARGS[@]}" "$VENDOR_DIR" >/dev/null
fi
echo "[rpm-build] .rpm-vendor/ = $(du -sh "$VENDOR_DIR" | cut -f1)"

# ------------------------------------------------------------ 2. staging tree
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
PKGDIR="$STAGE/$NAME-$VERSION"
mkdir -p "$PKGDIR"

# 白名单：只有构建真正需要的东西
for item in Cargo.toml Cargo.lock LICENSE README.md crates apps config packaging docs; do
    [ -e "$ROOT/$item" ] || { echo "[rpm-build] 缺少 $item" >&2; exit 1; }
    cp -a "$ROOT/$item" "$PKGDIR/"
done
mkdir -p "$PKGDIR/scripts"
for script in install.sh install-user-session.sh build-gui.sh gen-outputs-config.sh; do
    [ -e "$ROOT/scripts/$script" ] || { echo "[rpm-build] 缺少 scripts/$script" >&2; exit 1; }
    cp -a "$ROOT/scripts/$script" "$PKGDIR/scripts/"
done
# 内部归档与旧版 npm 目录不属于源码包
rm -rf "$PKGDIR/packaging/npm" "$PKGDIR/docs/archive"
# 构建产物不属于源码包：GUI 是独立 workspace，apps/liuguang-gui/target 可达数百 MB
find "$PKGDIR" -depth -type d -name target -prune -exec rm -rf {} +

cp -a "$VENDOR_DIR" "$PKGDIR/.rpm-vendor"

# 离线构建配置：只存在于 tarball 内
mkdir -p "$PKGDIR/.cargo"
cat > "$PKGDIR/.cargo/config.toml" <<'EOF'
# tarball 内生成：让 rpmbuild 的 %build 用 --offline 复现构建
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = ".rpm-vendor"
EOF

mkdir -p "$TOPDIR/SOURCES"
TARBALL="$TOPDIR/SOURCES/$NAME-$VERSION.tar.gz"
tar -C "$STAGE" -czf "$TARBALL" "$NAME-$VERSION"
echo "[rpm-build] tarball: $TARBALL ($(du -h "$TARBALL" | cut -f1))"

[ "$MODE" = "--tarball" ] && { echo "[rpm-build] 只生成 tarball，结束"; exit 0; }

# ------------------------------------------------------------ 3. rpmbuild
command -v rpmbuild >/dev/null || {
    echo "缺少 rpmbuild：sudo dnf install rpm-build rpmdevtools" >&2; exit 1; }

mkdir -p "$TOPDIR"/{BUILD,BUILDROOT,RPMS,SRPMS}
SPEC_ARGS=(-ba)
[ "$MODE" = "--srpm" ] && SPEC_ARGS=(-bs)

rpmbuild "${SPEC_ARGS[@]}" --define "_topdir $TOPDIR" \
    "$ROOT/packaging/liuguang.spec" 2>&1 | tail -40

echo
echo "[rpm-build] 产物："
find "$TOPDIR/SRPMS" "$TOPDIR/RPMS" -name "$NAME-$VERSION-*.rpm" -newer "$TARBALL" 2>/dev/null | sed 's/^/  /'
echo "安装： sudo dnf install <上面列出的 rpm>"
echo "查询： rpm -q $NAME && rpm -ql $NAME"
echo "卸载： sudo dnf remove $NAME liuguangctl"
