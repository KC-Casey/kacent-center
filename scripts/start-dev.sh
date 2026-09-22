#!/bin/bash
set -eo pipefail

fail() { printf '\n启动失败：%s\n' "$1" >&2; exit 1; }
case "${1:-}" in
  '') check_only=0 ;;
  --check) check_only=1 ;;
  --help) printf '用法：./启动调试.command [--check]\n--check 仅检查环境，不安装依赖、不启动应用。\n'; exit 0 ;;
  *) fail '未知参数。可使用 --check 检查环境，或不带参数启动。' ;;
esac
[[ $# -le 1 ]] || fail '只支持一个可选的 --check 参数。'
[[ "$(uname -s)" == Darwin ]] || fail '此脚本用于 macOS；Windows 请双击“启动调试.bat”。'
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
[[ -f package.json && -f src-tauri/Cargo.toml ]] || fail '项目文件不完整，请保留启动脚本与源码的目录结构。'

prepend_path() {
  [[ -d "$1" ]] && export PATH="$1:$PATH"
  return 0
}
node_ready() {
  command -v node >/dev/null 2>&1 \
    && node -e 'const [major, minor] = process.versions.node.split(".").map(Number); process.exit(major > 22 || (major === 22 && minor >= 12) ? 0 : 1)'
}

# Finder 启动的终端可能缺少包管理器路径。优先保留当前 PATH，仅在程序不可用时补齐。
if ! command -v node >/dev/null 2>&1 || ! command -v pnpm >/dev/null 2>&1; then
  for candidate in /usr/local/bin /opt/homebrew/bin "$HOME/.local/share/pnpm" "$HOME/Library/pnpm"; do
    prepend_path "$candidate"
  done
fi
if ! node_ready; then
  nvm_script="${NVM_DIR:-$HOME/.nvm}/nvm.sh"
  if [[ -s "$nvm_script" ]]; then
    # 先启用已安装的默认 Node，版本不足时尝试本机最新版本；不下载或修改默认别名。
    source "$nvm_script" --no-use
    nvm use --silent default >/dev/null 2>&1 || true
    if ! node_ready; then nvm use --silent node >/dev/null 2>&1 || true; fi
  fi
fi
command -v node >/dev/null 2>&1 || fail '未找到 Node.js。请安装 Node.js 22.12 或更高版本，再重新启动。'
node_ready || fail 'Node.js 版本过低，需要 22.12 或更高版本。'
command -v pnpm >/dev/null 2>&1 || fail '未找到 pnpm。请先运行 npm install -g pnpm@11，再重新启动。'
pnpm_version="$(pnpm --version)" || fail 'pnpm 无法运行，请检查 Node.js 与 pnpm 安装。'
pnpm_major="${pnpm_version%%.*}"
[[ "$pnpm_major" =~ ^[0-9]+$ && "$pnpm_major" -ge 11 ]] || fail '需要 pnpm 11 或更高版本。'

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
  prepend_path "${CARGO_HOME:-$HOME/.cargo}/bin"
fi
rust_ready() { cargo --version >/dev/null 2>&1 && rustc --version >/dev/null 2>&1; }
if ! rust_ready; then
  # 首次实现时使用过隔离的临时工具链；本机尚未安装常规 Rust 时可复用它。
  # 只修改本次启动进程的环境，而且绝不覆盖用户显式配置的 Cargo/Rustup 目录。
  if [[ -z "${CARGO_HOME:-}" && -z "${RUSTUP_HOME:-}" \
        && -O /private/tmp/kacent-cargo && -O /private/tmp/kacent-rustup \
        && -x /private/tmp/kacent-cargo/bin/rustc \
        && -d /private/tmp/kacent-rustup/toolchains ]] \
      && env CARGO_HOME=/private/tmp/kacent-cargo RUSTUP_HOME=/private/tmp/kacent-rustup \
         /private/tmp/kacent-cargo/bin/rustc --version >/dev/null 2>&1; then
    export CARGO_HOME=/private/tmp/kacent-cargo
    export RUSTUP_HOME=/private/tmp/kacent-rustup
    prepend_path "$CARGO_HOME/bin"
    printf '使用本机已有的临时 Rust 工具链；临时目录被系统清理后需安装常规 Rust。\n'
  else
    fail '未找到可用的 Rust 工具链。请按 README 安装 Rust 稳定版；已有 rustup 时可运行 rustup default stable。'
  fi
fi
rust_ready || fail 'Rust 工具链仍无法运行，请检查 Cargo 与 rustc。'
xcode-select -p >/dev/null 2>&1 || fail '缺少 Xcode Command Line Tools。请先运行 xcode-select --install 完成安装。'

# 调试数据库与正式安装版分开；用户显式指定 KACENT_DATA_DIR 时沿用其选择。
export KACENT_DATA_DIR="${KACENT_DATA_DIR:-$project_dir/.local-dev/data}"
printf '\n环境检查通过\n项目目录：%s\n调试数据：%s\n' "$project_dir" "$KACENT_DATA_DIR"
node --version
printf 'pnpm %s\n' "$pnpm_version"
cargo --version
rustc --version
[[ "$check_only" -eq 0 ]] || exit 0

mkdir -p "$KACENT_DATA_DIR" || fail '无法创建调试数据目录，请检查目录权限。'
printf '\n同步项目依赖（使用锁文件，已有依赖会复用）…\n'
pnpm install --frozen-lockfile || fail '前端依赖安装失败，请查看上方日志并检查网络或 pnpm 配置。'
printf '\n启动桌面调试，首次 Rust 编译可能需要几分钟。\n'
printf '关闭窗口会隐藏到托盘；结束调试请先在托盘选择“停止并退出”。\n'
if pnpm desktop; then
  printf '\n桌面调试已结束。\n'
else
  launch_status=$?
  printf '\n桌面调试启动或运行失败（退出码 %s）。请查看上方日志；若端口 1420 被占用，请先结束上一次调试。\n' "$launch_status" >&2
  exit "$launch_status"
fi
