#!/usr/bin/env python3
"""Regression check for SQLite statement-savepoint churn after a vtab write."""

from __future__ import annotations

import argparse
import sqlite3
import struct
import tempfile
import time
from pathlib import Path


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("extension", type=Path)
    parser.add_argument("--rows", type=int, default=50_000)
    parser.add_argument("--dimensions", type=int, default=1_536)
    parser.add_argument("--statements", type=int, default=50)
    args = parser.parse_args()
    if args.rows < 1 or args.dimensions < 8 or args.dimensions % 8:
        parser.error("rows must be positive and dimensions a positive multiple of 8")
    if args.statements < 1:
        parser.error("statements must be positive")
    return args


def insert_chunks(connection: sqlite3.Connection, count: int) -> float:
    started = time.perf_counter()
    for rowid in range(1, count + 1):
        connection.execute(
            "insert into chunks(id, body) values (?, ?)",
            (rowid, f"document {rowid}"),
        )
    return time.perf_counter() - started


def main() -> None:
    args = arguments()
    vector = struct.pack(
        f"<{args.dimensions}f", 1.0, *([0.0] * (args.dimensions - 1))
    )
    with tempfile.TemporaryDirectory(prefix="turbovec-savepoint-cost-") as directory:
        connection = sqlite3.connect(Path(directory) / "cost.db", isolation_level=None)
        connection.enable_load_extension(True)
        connection.load_extension(str(args.extension.resolve()))
        connection.enable_load_extension(False)
        connection.execute(
            "create virtual table vectors using "
            f"turbovec0(dimensions={args.dimensions}, bit_width=4)"
        )
        connection.execute("create table chunks(id integer primary key, body text)")
        connection.execute(
            "create virtual table chunks_fts using "
            "fts5(body, content='chunks', content_rowid='id')"
        )
        connection.execute(
            "create trigger chunks_ai after insert on chunks begin "
            "insert into chunks_fts(rowid, body) values (new.id, new.body); end"
        )
        connection.execute(
            "with recursive ids(id) as ("
            "  values(1) union all select id + 1 from ids where id < ?"
            ") insert into vectors(rowid, embedding) select id, ? from ids",
            (args.rows, vector),
        )

        connection.execute("begin immediate")
        baseline = insert_chunks(connection, args.statements)
        connection.rollback()

        connection.execute("begin immediate")
        connection.execute(
            "insert into vectors(rowid, embedding) values (?, ?)",
            (args.rows + 1, vector),
        )
        after_vector_write = insert_chunks(connection, args.statements)
        connection.rollback()
        connection.close()

    limit = max(0.05, baseline * 3.0)
    if after_vector_write > limit:
        raise AssertionError(
            "statement-savepoint churn is too expensive after a turbovec0 write: "
            f"baseline={baseline:.6f}s after_write={after_vector_write:.6f}s "
            f"limit={limit:.6f}s"
        )
    print(
        "savepoint cost passed: "
        f"rows={args.rows} dimensions={args.dimensions} statements={args.statements} "
        f"baseline_ms={baseline * 1000:.3f} "
        f"after_write_ms={after_vector_write * 1000:.3f}"
    )


if __name__ == "__main__":
    main()
