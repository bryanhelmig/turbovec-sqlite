#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target_root=${CARGO_TARGET_DIR:-"$repo_root/target"}
static_target_root="$target_root/static"

case "$(uname -s)" in
  Darwin)
    archive="$static_target_root/release/libturbovec_sqlite.a"
    sqlite_prefix=${SQLITE_PREFIX:-"$(brew --prefix sqlite)"}
    sqlite_flags=(-I"$sqlite_prefix/include" -L"$sqlite_prefix/lib" -lsqlite3 -liconv)
    check_archive_symbols=0
    ;;
  Linux)
    archive="$static_target_root/release/libturbovec_sqlite.a"
    sqlite_flags=(-lsqlite3 -ldl -lpthread -lm)
    check_archive_symbols=1
    ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "Static-link smoke test is not configured for Windows; archive export is checked by packaging."
    exit 0
    ;;
  *) echo "Unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

cd "$repo_root"
cargo build --release --locked --no-default-features --target-dir "$static_target_root"
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/turbovec-static-test.XXXXXX")
trap 'rm -rf "$test_dir"' EXIT
if [[ "$check_archive_symbols" == 1 ]]; then
  nm -g "$archive" > "$test_dir/archive-symbols.txt"
  if grep -q 'sqlite3_extension_init' "$test_dir/archive-symbols.txt"; then
    echo "Static archive must not export the generic sqlite3_extension_init symbol" >&2
    exit 1
  fi
  grep -q 'sqlite3_turbovec_init' "$test_dir/archive-symbols.txt"
fi

cc -std=c11 -Wall -Wextra -Werror \
  -DTURBOVEC_SQLITE_VERSION="\"$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)\"" \
  -Iinclude tests/static_link.c "$archive" "${sqlite_flags[@]}" \
  -o "$test_dir/static-link-smoke"
"$test_dir/static-link-smoke"
echo "static-link smoke test passed"
