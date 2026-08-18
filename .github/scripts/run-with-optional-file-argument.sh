#!/bin/bash

# Invoke a non-empty command array and append one optional file argument only
# when its path is present. This avoids expanding an empty array under
# macOS Bash 3.2 with `set -u`, while preserving each argument byte-for-byte.
run_with_optional_file_argument() {
  if [ "$#" -lt 3 ]; then
    echo "usage: run_with_optional_file_argument FLAG PATH COMMAND [ARG ...]" >&2
    return 2
  fi

  local optional_flag="$1"
  local optional_path="$2"
  shift 2
  local command=("$@")
  if [ -n "$optional_path" ]; then
    command+=("$optional_flag" "$optional_path")
  fi
  "${command[@]}"
}
