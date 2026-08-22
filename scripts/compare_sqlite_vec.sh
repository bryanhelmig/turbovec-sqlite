#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
version=0.1.9

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    target=macos-aarch64
    library=vec0.dylib
    checksum=8282126333399ddfe98bbbcc7a1936e7252625aac49df056a98be602e46bfd29
    turbovec_library=libturbovec_sqlite.dylib
    ;;
  Darwin:x86_64)
    target=macos-x86_64
    library=vec0.dylib
    checksum=53ad76e400786515e2edcaed2f01271dda846316390b761fadbd2dcf56aa4713
    turbovec_library=libturbovec_sqlite.dylib
    ;;
  Linux:aarch64)
    target=linux-aarch64
    library=vec0.so
    checksum=ea03d39541e478fab5974253c461e1cb5d77742f69e40cf96e3fad5bc309a37c
    turbovec_library=libturbovec_sqlite.so
    ;;
  Linux:x86_64)
    target=linux-x86_64
    library=vec0.so
    checksum=b959baa1d8dc88861b1edb337b8587178cdcb12d60b4998f9d10b6a82052d5d7
    turbovec_library=libturbovec_sqlite.so
    ;;
  MINGW*:x86_64|MSYS*:x86_64|CYGWIN*:x86_64)
    target=windows-x86_64
    library=vec0.dll
    checksum=51581189d52066b4dfc6631f6d7a3eab7dedc2260656ab09ca97ab3fb8165983
    turbovec_library=turbovec_sqlite.dll
    ;;
  *)
    echo "No pinned sqlite-vec benchmark binary for $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

cache="$repo_root/target/sqlite-vec/$version/$target"
archive="$cache/sqlite-vec.tar.gz"
sqlite_vec="$cache/$library"

mkdir -p "$cache"
if [[ ! -f "$archive" ]]; then
  url="https://github.com/asg017/sqlite-vec/releases/download/v$version/sqlite-vec-$version-loadable-$target.tar.gz"
  curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$archive"
fi
actual=$(python3 -c \
  'import hashlib, pathlib, sys; print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())' \
  "$archive")
if [[ "$actual" != "$checksum" ]]; then
  echo "sqlite-vec archive checksum mismatch: expected $checksum, got $actual" >&2
  exit 1
fi
tar -xzf "$archive" -C "$cache"
[[ -f "$sqlite_vec" ]] || { echo "sqlite-vec archive did not contain $library" >&2; exit 1; }

cd "$repo_root"
cargo build --release --locked
turbovec_extension="$repo_root/target/release/$turbovec_library"
if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* || "$(uname -s)" == CYGWIN* ]]; then
  turbovec_extension=$(cygpath -m "$turbovec_extension")
  sqlite_vec=$(cygpath -m "$sqlite_vec")
fi
python3 benchmarks/compare_sqlite_vec.py \
  --turbovec-extension "$turbovec_extension" \
  --sqlite-vec-extension "$sqlite_vec" \
  "$@"
