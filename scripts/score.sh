#!/usr/bin/env bash
set -euo pipefail

script_root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

exec "$script_root/compare_sqlite_vec.sh" \
  --rows 25000 \
  --dimensions 768 \
  --queries 100 \
  --k 10 \
  --repetitions 3 \
  --warmup 20 \
  "$@"
