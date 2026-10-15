#!/usr/bin/env bash
set -euo pipefail
if [[ $(uname -s) != Linux ]]; then
    echo 'Use remeasure_windows.ps1 on native Windows.' >&2
    exit 2
fi
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
exec python3 "$script_dir/remeasure.py" "$@"
