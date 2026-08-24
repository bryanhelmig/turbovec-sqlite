#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

find_sqlite() {
  local candidate
  for candidate in \
    "${SQLITE3:-}" \
    /opt/homebrew/opt/sqlite/bin/sqlite3 \
    /usr/local/opt/sqlite/bin/sqlite3 \
    "$(command -v sqlite3 2>/dev/null || true)"
  do
    if [[ -n "$candidate" && -x "$candidate" ]] && \
       "$candidate" :memory: '.help .load' 2>/dev/null | grep -q '^\.load '; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

sqlite_bin=$(find_sqlite) || {
  echo "No sqlite3 CLI with loadable-extension support was found." >&2
  echo "On macOS: brew install sqlite" >&2
  exit 1
}

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

test_dir=$(mktemp -d "${TMPDIR:-/tmp}/turbovec-sqlite-test.XXXXXX")
trap 'rm -rf "$test_dir"' EXIT
database="$test_dir/smoke.db"

"$sqlite_bin" "$database" \
  -cmd ".load $extension" \
  < tests/smoke.sql

persisted=$(
  "$sqlite_bin" "$database" \
    -cmd ".load $extension" \
    "select turbovec_len(payload) from vector_indexes where name='documents'"
)
[[ "$persisted" == "3" ]]

chunked_rows=$(
  "$sqlite_bin" "$database" \
    -cmd ".load $extension" \
    "select count(*) from renamed_vectors"
)
[[ "$chunked_rows" == "3" ]]

if python3 - <<'PY'
import sqlite3
supported = (
    sqlite3.sqlite_version_info >= (3, 44, 0)
    and hasattr(sqlite3.Connection, "enable_load_extension")
)
raise SystemExit(not supported)
PY
then
  python3 -X faulthandler tests/multiconnection.py \
    "$test_dir/multiconnection.db" \
    "$extension"
  transaction_repetitions=${TRANSACTION_TEST_REPETITIONS:-1}
  if ! [[ "$transaction_repetitions" =~ ^[1-9][0-9]*$ ]]; then
    echo "TRANSACTION_TEST_REPETITIONS must be a positive integer" >&2
    exit 1
  fi
  for ((run = 1; run <= transaction_repetitions; run++)); do
    python3 -X faulthandler tests/transactions.py "$extension"
  done
  python3 -X faulthandler tests/allowlist.py "$extension"
  python3 -X faulthandler tests/model_check.py "$extension" --seeds 20 --steps 20
else
  python_sqlite_version=$(python3 -c 'import sqlite3; print(sqlite3.sqlite_version)')
  python_extension_loading=$(python3 - <<'PY'
import sqlite3
print("yes" if hasattr(sqlite3.Connection, "enable_load_extension") else "no")
PY
)
  if [[ "${REQUIRE_PYTHON_TESTS:-0}" == "1" ]]; then
    echo "Python tests require SQLite 3.44+ with extension loading; found SQLite $python_sqlite_version, extension loading: $python_extension_loading" >&2
    exit 1
  fi
  echo "Skipping Python-bound tests: SQLite $python_sqlite_version, extension loading: $python_extension_loading"
fi

if "$sqlite_bin" :memory: \
  -cmd ".load $extension" \
  "select turbovec_new(7, 4)" >/dev/null 2>&1
then
  echo "Expected invalid dimensions to fail" >&2
  exit 1
fi

if "$sqlite_bin" "$database" \
  -cmd ".load $extension" \
  "select * from turbovec_knn((select payload from vector_indexes), '[1, 2]', 2)" \
  >/dev/null 2>&1
then
  echo "Expected a query dimension mismatch to fail" >&2
  exit 1
fi

if "$sqlite_bin" "$database" \
  -cmd ".load $extension" \
  "update renamed_vectors set embedding='[0,0,0,1,0,0,0,0]' where rowid=1" \
  >/dev/null 2>&1
then
  echo "Expected contentless virtual-table UPDATE to fail" >&2
  exit 1
fi

if "$sqlite_bin" "$database" \
  -cmd ".load $extension" \
  "create view forbidden_turbovec_use as select count(*) from renamed_vectors; \
   select * from forbidden_turbovec_use" >/dev/null 2>&1
then
  echo "Expected DIRECTONLY virtual-table use from a view to fail" >&2
  exit 1
fi

echo "turbovec-sqlite smoke tests passed with $($sqlite_bin --version | awk '{print $1}')"
