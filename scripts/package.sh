#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
crate_root="$repo_root"
target_root=${CARGO_TARGET_DIR:-"$repo_root/target"}
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$crate_root/Cargo.toml" | head -1)

case "$(uname -s)" in
  Darwin) platform=macos; library=libturbovec_sqlite.dylib ;;
  Linux) platform=linux; library=libturbovec_sqlite.so ;;
  MINGW*|MSYS*|CYGWIN*) platform=windows; library=turbovec_sqlite.dll ;;
  *) echo "Unsupported platform: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64|aarch64) architecture=aarch64 ;;
  x86_64|amd64) architecture=x86_64 ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

cd "$repo_root"
cargo build --release --locked

package="turbovec-sqlite-$version-$platform-$architecture"
archive="$repo_root/dist/$package.tar.gz"
checksum="$archive.sha256"
stage=$(mktemp -d "${TMPDIR:-/tmp}/turbovec-sqlite-package.XXXXXX")
trap 'rm -rf "$stage"' EXIT

mkdir -p "$repo_root/dist" "$stage/$package"
cp "$target_root/release/$library" "$stage/$package/"
cp "$crate_root/README.md" "$crate_root/DESIGN.md" \
  "$crate_root/BENCHMARKS.md" "$crate_root/PERFORMANCE_EXPERIMENTS.md" \
  "$crate_root/demo.sql" \
  "$repo_root/LICENSE" "$stage/$package/"
mkdir -p "$stage/$package/examples"
cp -R "$crate_root/examples/clients" "$stage/$package/examples/"
tar -czf "$archive" -C "$stage" "$package"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$repo_root/dist" && sha256sum "$(basename "$archive")") > "$checksum"
else
  (cd "$repo_root/dist" && shasum -a 256 "$(basename "$archive")") > "$checksum"
fi

printf '%s\n%s\n' "$archive" "$checksum"
