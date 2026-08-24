#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
crate_root="$repo_root"
target_root=${CARGO_TARGET_DIR:-"$repo_root/target"}
static_target_root="$target_root/static"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$crate_root/Cargo.toml" | head -1)

case "$(uname -s)" in
  Darwin) platform=macos; library=libturbovec_sqlite.dylib; static_library=libturbovec_sqlite.a ;;
  Linux) platform=linux; library=libturbovec_sqlite.so; static_library=libturbovec_sqlite.a ;;
  MINGW*|MSYS*|CYGWIN*) platform=windows; library=turbovec_sqlite.dll; static_library=turbovec_sqlite.lib ;;
  *) echo "Unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64|aarch64) architecture=aarch64 ;;
  x86_64|amd64) architecture=x86_64 ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

cd "$repo_root"
cargo build --release --locked
cargo build --release --locked --no-default-features --target-dir "$static_target_root"

package="turbovec-sqlite-$version-$platform-$architecture"
archive="$repo_root/dist/$package.tar.gz"
checksum="$archive.sha256"
stage=$(mktemp -d "${TMPDIR:-/tmp}/turbovec-sqlite-package.XXXXXX")
trap 'rm -rf "$stage"' EXIT

mkdir -p "$repo_root/dist" "$stage/$package"
cp "$target_root/release/$library" "$static_target_root/release/$static_library" "$stage/$package/"
cp "$crate_root/README.md" \
  "$crate_root/CHANGELOG.md" \
  "$crate_root/SECURITY.md" \
  "$crate_root/THIRD_PARTY_LICENSES.html" \
  "$repo_root/LICENSE" \
  "$stage/$package/"
mkdir -p "$stage/$package/docs" "$stage/$package/examples" "$stage/$package/include"
cp "$crate_root/docs/"*.md "$stage/$package/docs/"
cp "$crate_root/examples/demo.sql" "$stage/$package/examples/"
cp "$crate_root/include/turbovec_sqlite.h" "$stage/$package/include/"
mkdir -p "$stage/$package/examples/clients/go"
cp "$crate_root/examples/clients/python.py" \
  "$crate_root/examples/clients/javascript.mjs" \
  "$stage/$package/examples/clients/"
cp "$crate_root/examples/clients/go/go.mod" \
  "$crate_root/examples/clients/go/go.sum" \
  "$crate_root/examples/clients/go/main.go" \
  "$stage/$package/examples/clients/go/"
tar -czf "$archive" -C "$stage" "$package"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$repo_root/dist" && sha256sum "$(basename "$archive")") > "$checksum"
  (cd "$repo_root/dist" && sha256sum -c "$(basename "$checksum")")
else
  (cd "$repo_root/dist" && shasum -a 256 "$(basename "$archive")") > "$checksum"
  (cd "$repo_root/dist" && shasum -a 256 -c "$(basename "$checksum")")
fi

tar -tzf "$archive" > "$stage/archive-contents.txt"
for required in \
  "$library" \
  "$static_library" \
  README.md \
  CHANGELOG.md \
  SECURITY.md \
  THIRD_PARTY_LICENSES.html \
  LICENSE \
  docs/DESIGN.md \
  docs/BENCHMARKS.md \
  docs/PERFORMANCE_EXPERIMENTS.md \
  examples/demo.sql \
  include/turbovec_sqlite.h \
  examples/clients/python.py \
  examples/clients/javascript.mjs \
  examples/clients/go/main.go
do
  grep -Fqx "$package/$required" "$stage/archive-contents.txt" || {
    echo "Package archive is missing $required" >&2
    exit 1
  }
done

printf '%s\n%s\n' "$archive" "$checksum"
