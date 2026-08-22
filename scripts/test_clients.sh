#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
node_bin=${NODE:-$(command -v node 2>/dev/null || true)}
go_bin=${GO:-$(command -v go 2>/dev/null || true)}

[[ -n "$node_bin" ]] || { echo "Node.js 22.13 or newer is required" >&2; exit 1; }
[[ -n "$go_bin" ]] || { echo "Go with CGO enabled is required" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) extension="$repo_root/target/release/libturbovec_sqlite.dylib" ;;
  Linux) extension="$repo_root/target/release/libturbovec_sqlite.so" ;;
  MINGW*|MSYS*|CYGWIN*)
    extension=$(cygpath -m "$repo_root/target/release/turbovec_sqlite.dll")
    ;;
  *) echo "Unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

cd "$repo_root"
cargo build --release --locked

python_output=$(python3 examples/clients/python.py "$extension")
javascript_output=$("$node_bin" examples/clients/javascript.mjs "$extension")
go_output=$(
  cd examples/clients/go
  CGO_ENABLED=1 "$go_bin" run . -extension "$extension"
)

if ! printf '%s\n' "$python_output" | awk -F '\t' '
  NR == 1 && $1 == "1" && $2 == "east" && $3 ~ /^-?[0-9]+\.[0-9]{4}$/ { valid++ }
  NR == 2 && $1 == "3" && $2 == "near east" && $3 ~ /^-?[0-9]+\.[0-9]{4}$/ { valid++ }
  END { exit !(NR == 2 && valid == 2) }
'; then
  echo "Client examples returned unexpected ranked rows:" >&2
  echo "$python_output" >&2
  exit 1
fi

if [[ "$javascript_output" != "$python_output" || "$go_output" != "$python_output" ]]; then
  echo "Client examples disagreed:" >&2
  printf 'Python:\n%s\nJavaScript:\n%s\nGo:\n%s\n' \
    "$python_output" "$javascript_output" "$go_output" >&2
  exit 1
fi

echo "Python, JavaScript, and Go client examples passed"
