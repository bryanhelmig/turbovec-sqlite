#!/usr/bin/env python3
"""Measure process peak RSS over bulk-build and append commits on macOS/Linux.

Run each extension variant in a fresh process. RSS includes SQLite, the warm
index, and the Python host, not just the serializer's allocations. Repeated
sparse vectors exercise storage geometry; this is not a recall benchmark.
"""

from __future__ import annotations

import argparse
import json
import platform
import sqlite3
import struct
import tempfile
import time
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extension", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=100_000)
    parser.add_argument("--dimensions", type=int, default=1536)
    args = parser.parse_args()
    if platform.system() not in ("Darwin", "Linux"):
        parser.error("peak RSS measurement is supported on macOS and Linux")
    if args.rows < 1 or not 8 <= args.dimensions <= 16384 or args.dimensions % 8:
        parser.error("rows must be positive; dimensions must be a multiple of 8 from 8 to 16384")
    if not args.extension.is_file():
        parser.error("extension must exist")

    import resource

    def rss() -> int:
        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        return peak if platform.system() == "Darwin" else peak * 1024

    with tempfile.TemporaryDirectory(prefix="turbovec-commit-memory-") as directory:
        db = sqlite3.connect(str(Path(directory) / "probe.db"))
        db.enable_load_extension(True)
        db.load_extension(str(args.extension.resolve()))
        db.enable_load_extension(False)
        db.execute("pragma journal_mode=wal")
        db.execute("pragma synchronous=full")
        db.execute("pragma wal_autocheckpoint=0")
        db.execute(
            f"create virtual table v using turbovec0(dimensions={args.dimensions},bit_width=4)"
        )
        vector = struct.pack(f"<{args.dimensions}f", 1, *([0] * (args.dimensions - 1)))
        db.executemany(
            "insert into v(rowid,embedding) values(?,?)",
            ((rowid, vector) for rowid in range(args.rows)),
        )
        before_initial_commit = rss()
        begin = time.perf_counter()
        db.commit()
        initial_commit_ms = (time.perf_counter() - begin) * 1000
        after_initial_commit = rss()
        db.execute("pragma wal_checkpoint(truncate)")
        db.execute("insert into v(rowid,embedding) values(?,?)", (args.rows, vector))
        begin = time.perf_counter()
        db.commit()
        append_commit_ms = (time.perf_counter() - begin) * 1000
        after_append_commit = rss()
        assert db.execute("select count(*) from v").fetchone()[0] == args.rows + 1
        serialized_bytes = db.execute("select byte_len from v_meta").fetchone()[0]
        db.close()
    print(json.dumps({
        "extension": str(args.extension),
        "rows": args.rows,
        "dimensions": args.dimensions,
        "sqlite": sqlite3.sqlite_version,
        "platform": platform.platform(),
        "rss_before_initial_commit": before_initial_commit,
        "rss_after_initial_commit": after_initial_commit,
        "rss_after_append_commit": after_append_commit,
        "serialized_bytes": serialized_bytes,
        "initial_commit_ms": initial_commit_ms,
        "append_commit_ms": append_commit_ms,
    }, indent=2))


if __name__ == "__main__":
    main()
