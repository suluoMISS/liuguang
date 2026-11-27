#!/usr/bin/env bash
# 兼容入口：桌面形态的安装已并入 `scripts/install.sh`。
#
#   ./scripts/install-user-session.sh            # 等价于 install.sh --mode user
#   ./scripts/install-user-session.sh --dry-run
#   ./scripts/install-user-session.sh --uninstall
#
# 保留这个文件名是因为 README、docs/DEPLOYMENT.md 与打包脚本里都引用了它；
# 真正的逻辑只有一份（install.sh），避免两份脚本各自漂移。
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
args=()
for arg in "$@"; do
  case "$arg" in
    --uninstall) args+=(--uninstall) ;;
    --dry-run) args+=(--dry-run) ;;
    -h|--help) exec "$HERE/install.sh" --help ;;
    *) echo "未识别的参数：$arg（用 $HERE/install.sh --help 看完整参数）" >&2; exit 2 ;;
  esac
done
exec "$HERE/install.sh" --mode user "${args[@]}"
