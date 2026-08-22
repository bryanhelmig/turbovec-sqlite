#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

case "$(uname -s)" in
  Darwin) extension="$repo_root/target/release/libturbovec_sqlite.dylib" ;;
  Linux) extension="$repo_root/target/release/libturbovec_sqlite.so" ;;
  MINGW*|MSYS*|CYGWIN*) extension="$repo_root/target/release/turbovec_sqlite.dll" ;;
  *) echo "Unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

cd "$repo_root"
cargo build --release --locked
python3 tests/model_check.py "$extension" \
  --seeds "${MODEL_SEEDS:-100}" --steps "${MODEL_STEPS:-40}"
exec python3 benchmarks/write_factory.py \
  --extension "$extension" "$@"
