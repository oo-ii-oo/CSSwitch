#!/bin/bash
set -u
ROOT="$(cd "$(dirname "$0")" && pwd)"
bash "$ROOT/scripts/build-macos-intel.sh"
result=$?
if [[ -t 0 ]]; then
  if [[ "$result" == 0 ]]; then
    /usr/bin/open "$ROOT/dist/intel"
  fi
  read -r -p "按回车关闭此窗口… " _reply
fi
exit "$result"
