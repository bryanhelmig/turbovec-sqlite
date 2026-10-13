#!/usr/bin/env python3
"""Verify all five downloaded release archives before publication."""

import argparse
import hashlib
from pathlib import Path
import tarfile
import tomllib

PLATFORMS = {
    "linux-aarch64": ("libturbovec_sqlite.so", "libturbovec_sqlite.a"),
    "linux-x86_64": ("libturbovec_sqlite.so", "libturbovec_sqlite.a"),
    "macos-aarch64": ("libturbovec_sqlite.dylib", "libturbovec_sqlite.a"),
    "macos-x86_64": ("libturbovec_sqlite.dylib", "libturbovec_sqlite.a"),
    "windows-x86_64": ("turbovec_sqlite.dll", "turbovec_sqlite.lib"),
}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    manifest = Path(__file__).resolve().parents[1] / "Cargo.toml"
    version = tomllib.loads(manifest.read_text())["package"]["version"]
    names = {f"turbovec-sqlite-{version}-{target}.tar.gz" for target in PLATFORMS}
    expected = names | {name + ".sha256" for name in names}
    actual = {p.name for p in args.directory.iterdir()}
    if actual != expected:
        raise ValueError(f"release files differ: missing={expected - actual}, extra={actual - expected}")
    for target, libraries in PLATFORMS.items():
        package = f"turbovec-sqlite-{version}-{target}"
        archive = args.directory / f"{package}.tar.gz"
        expected_hash, filename = Path(str(archive) + ".sha256").read_text().split()
        if filename != archive.name:
            raise ValueError(f"checksum names the wrong archive: {filename}")
        with archive.open("rb") as stream:
            actual_hash = hashlib.file_digest(stream, "sha256").hexdigest()
        if expected_hash != actual_hash:
            raise ValueError(f"checksum mismatch: {archive.name}")
        with tarfile.open(archive) as tar:
            required = (*libraries, "include/turbovec_sqlite.h", "README.md", "CHANGELOG.md",
                        "LICENSE", "THIRD_PARTY_LICENSES.html")
            for filename in required:
                member = tar.getmember(f"{package}/{filename}")
                if not member.isfile() or member.size == 0:
                    raise ValueError(f"missing or empty release file: {member.name}")
        print(f"verified {archive.name}")


if __name__ == "__main__":
    main()
