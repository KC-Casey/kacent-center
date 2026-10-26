#!/bin/bash
# 使用 source scripts/dev-env.sh，可在当前终端启用项目内的开发工具。
# 根据本文件的位置计算路径，项目移动后无需修改用户的 shell 配置。
# Zsh 与 Bash 获取被 source 文件路径的语法不同，不能用调用者的 $0 定位。
if [[ -n "${ZSH_VERSION:-}" ]]; then
  kacent_env_file="${(%):-%x}"
else
  kacent_env_file="${BASH_SOURCE[0]}"
fi
kacent_tools_dir="$(cd -- "$(dirname -- "$kacent_env_file")/.." && pwd)/.local-dev/tools"

# Rustup 的代理程序依赖这两个目录共同定位实际编译器；仅添加 PATH 不够。
# 用户显式配置任意一个目录时不自动混用项目工具链，避免缓存与工具链错配。
if [[ -z "${CARGO_HOME:-}" && -z "${RUSTUP_HOME:-}" \
      && -x "$kacent_tools_dir/cargo/bin/rustup" \
      && -d "$kacent_tools_dir/rustup/toolchains" ]]; then
  export CARGO_HOME="$kacent_tools_dir/cargo"
  export RUSTUP_HOME="$kacent_tools_dir/rustup"
  export PATH="$CARGO_HOME/bin:$PATH"
fi

# 直接使用发行包内的原生 CMake，避免依赖 pip 生成的 Python 启动器和绝对路径。
# 工具只对当前 shell 及其子进程生效，不写入全局 PATH 或用户配置文件。
if [[ -x "$kacent_tools_dir/cmake/cmake/data/bin/cmake" ]]; then
  export PATH="$kacent_tools_dir/cmake/cmake/data/bin:$PATH"
fi
unset kacent_tools_dir kacent_env_file
