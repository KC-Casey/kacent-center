#!/bin/bash
set -eo pipefail

fail() { printf '\n启动失败：%s\n' "$1" >&2; exit 1; }
case "${1:-}" in
  '') check_only=0 ;;
  --check) check_only=1 ;;
  --help) printf '用法：./start-dev.command [--check]\n--check 仅检查环境，不安装依赖、不启动应用。\n'; exit 0 ;;
  *) fail '未知参数。可使用 --check 检查环境，或不带参数启动。' ;;
esac
[[ $# -le 1 ]] || fail '只支持一个可选的 --check 参数。'
[[ "$(uname -s)" == Darwin ]] || fail '此脚本用于 macOS；Windows 请双击“start-dev.bat”。'
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
[[ -f package.json && -f src-tauri/Cargo.toml ]] || fail '项目文件不完整，请保留启动脚本与源码的目录结构。'

# 优先启用项目内持久保存的工具；环境同时传递给 pnpm、Tauri 和 Cargo 子进程。
source "$project_dir/scripts/dev-env.sh"

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
  fail '未找到可用的 Rust 工具链。请按 README 安装项目内 Rust；显式设置过 CARGO_HOME 或 RUSTUP_HOME 时请检查对应目录。'
fi
rust_ready || fail 'Rust 工具链仍无法运行，请检查 Cargo 与 rustc。'
xcode-select -p >/dev/null 2>&1 || fail '缺少 Xcode Command Line Tools。请先运行 xcode-select --install 完成安装。'
# whisper.cpp 通过 CMake 编译 C++ 源码。部分 Command Line Tools 版本不会把
# SDK 中的 libc++ 头文件自动加入搜索路径，因此先用 xcrun 找到当前 SDK，
# 再用一个只包含标准库头文件的 clang++ 探测决定是否需要补 CXXFLAGS。
command -v xcrun >/dev/null 2>&1 || fail '未找到 xcrun。请安装或重新选择 Xcode Command Line Tools。'
macos_sdk="$(xcrun --sdk macosx --show-sdk-path 2>/dev/null)" \
  || fail '无法定位 macOS SDK。请检查 Xcode Command Line Tools 的安装状态。'
[[ -d "$macos_sdk" ]] || fail "macOS SDK 路径无效：$macos_sdk"
clangxx="$(xcrun --sdk macosx --find clang++ 2>/dev/null)" \
  || fail '未找到 clang++。请安装或重新选择 Xcode Command Line Tools。'
[[ -x "$clangxx" ]] || fail "clang++ 路径无效：$clangxx"

# 用户显式设置 SDKROOT 时优先沿用；该路径无法通过探测时回退到 xcrun 的当前 SDK。
selected_sdk="${SDKROOT:-$macos_sdk}"
cpp_headers_probe() {
  local sdk="$1"
  shift
  printf '%s\n' \
    '#include <array>' \
    '#include <mutex>' \
    'int main(){ std::array<int, 1> a{}; std::mutex m; (void)a; (void)m; }' \
    | "$clangxx" -isysroot "$sdk" "$@" -std=c++17 -x c++ -fsyntax-only - >/dev/null 2>&1
}
if ! cpp_headers_probe "$selected_sdk"; then
  selected_sdk="$macos_sdk"
fi
export SDKROOT="$selected_sdk"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"

if ! cpp_headers_probe "$SDKROOT"; then
  cxx_header_dir="$SDKROOT/usr/include/c++/v1"
  [[ -d "$cxx_header_dir" ]] || fail \
    "clang++ 无法找到 C++ 标准库头文件（<array>、<mutex>），且 SDK 中不存在：$cxx_header_dir"
  cpp_headers_probe "$SDKROOT" -isystem "$cxx_header_dir" || fail \
    'clang++ 无法编译 C++ 标准库头文件。请更新 Xcode Command Line Tools 后重试。'
  case " ${CXXFLAGS:-} " in
    *"$cxx_header_dir"*) ;;
    *) export CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-isystem $cxx_header_dir" ;;
  esac
  printf '已为本次调试补充 C++ 标准库头文件路径：%s\n' "$cxx_header_dir"
fi
printf 'macOS SDK：%s\nclang++：%s\n' "$SDKROOT" "$clangxx"
# whisper.cpp 随应用编译，需要 CMake；项目内版本已由 dev-env.sh 加入 PATH。
command -v cmake >/dev/null 2>&1 || fail '本地语音引擎需要 CMake。请安装 CMake 并加入 PATH，再重新启动。'
cmake --version >/dev/null 2>&1 || fail 'CMake 无法运行，请检查项目内或 PATH 中的 CMake 安装。'


# 调试数据库与正式安装版分开；用户显式指定 KACENT_DATA_DIR 时沿用其选择。
export KACENT_DATA_DIR="${KACENT_DATA_DIR:-$project_dir/.local-dev/data}"
printf '\n环境检查通过\n项目目录：%s\n调试数据：%s\n' "$project_dir" "$KACENT_DATA_DIR"
node --version
printf 'pnpm %s\n' "$pnpm_version"
cargo --version
rustc --version
cmake --version
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
