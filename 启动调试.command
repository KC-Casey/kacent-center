#!/bin/bash
# Finder 双击时也以项目目录为基准；路径中的中文和空格不会被拆分。
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)" || exit 1
/bin/bash "$project_dir/scripts/start-dev.sh" "$@"
launch_status=$?

# 双击后保留日志，便于查看依赖缺失或编译错误；命令行带参数检查时直接返回。
if [[ $# -eq 0 && -t 0 ]]; then
  printf '\n按回车关闭此窗口…'
  read -r _
fi
exit "$launch_status"
