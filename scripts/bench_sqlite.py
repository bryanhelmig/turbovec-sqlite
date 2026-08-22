#!/usr/bin/env python3
"""Compare the reference BLOB path with the chunked virtual table."""

from __future__ import annotations

import argparse
import sqlite3
import struct
import time
from pathlib import Path


def elapsed(operation):
    started = time.perf_counter()
    result = operation()
    return time.perf_counter() - started, result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("extension", type=Path)
    parser.add_argument("--rows", type=int, default=1_000)
    parser.add_argument("--dimensions", type=int, default=256)
    parser.add_argument("--queries", type=int, default=100)
    args = parser.parse_args()
    if args.rows < 1 or args.dimensions < 8 or args.dimensions % 8:
        parser.error("rows must be positive; dimensions must be a positive multiple of 8")

    connection = sqlite3.connect(":memory:")
    connection.enable_load_extension(True)
    connection.load_extension(str(args.extension.resolve()))
    connection.enable_load_extension(False)

    # Distinct, deterministic unit-ish vectors. The benchmark targets SQLite
    # integration overhead; recall quality belongs to examples/compare.rs.
    vectors = []
    for row in range(args.rows):
        values = [0.0] * args.dimensions
        values[row % args.dimensions] = 1.0
        values[(row * 17 + 3) % args.dimensions] += 0.25
        vectors.append((row + 1, struct.pack(f"<{args.dimensions}f", *values)))
    query = vectors[0][1]

    connection.execute(
        "create table blob_index(payload blob not null)"
    )
    connection.execute(
        "insert into blob_index values (turbovec_new(?, 4))",
        (args.dimensions,),
    )

    def insert_blob() -> None:
        with connection:
            for rowid, vector in vectors:
                connection.execute(
                    "update blob_index set payload=turbovec_add(payload, ?, ?)",
                    (rowid, vector),
                )

    blob_insert, _ = elapsed(insert_blob)
    blob_size = connection.execute(
        "select length(payload) from blob_index"
    ).fetchone()[0]

    connection.execute(
        f"create virtual table chunk_index using "
        f"turbovec0(dimensions={args.dimensions}, bit_width=4)"
    )

    def insert_chunked() -> None:
        with connection:
            connection.executemany(
                "insert into chunk_index(rowid, embedding) values (?, ?)", vectors
            )

    chunk_insert, _ = elapsed(insert_chunked)
    chunk_size, chunk_count = connection.execute(
        "select coalesce(sum(length(data)), 0), count(*) from chunk_index_chunks"
    ).fetchone()

    def search_blob() -> None:
        for _ in range(args.queries):
            connection.execute(
                "select id, score from turbovec_knn("
                "(select payload from blob_index), ?, 10) order by score desc",
                (query,),
            ).fetchall()

    def search_chunked() -> None:
        for _ in range(args.queries):
            connection.execute(
                "select rowid, score from chunk_index "
                "where embedding match ? order by score desc limit 10",
                (query,),
            ).fetchall()

    # Warm both paths once before measuring repeated query latency.
    search_blob()
    search_chunked()
    blob_query, _ = elapsed(search_blob)
    chunk_query, _ = elapsed(search_chunked)

    print(
        f"rows={args.rows}, dim={args.dimensions}, queries={args.queries}, k=10"
    )
    print()
    print("| SQLite path | insert ms | index MiB | query ms/query |")
    print("|---|---:|---:|---:|")
    print(
        f"| scalar BLOB rewrite | {blob_insert * 1_000:.2f} | "
        f"{blob_size / 1_048_576:.2f} | "
        f"{blob_query * 1_000 / args.queries:.3f} |"
    )
    print(
        f"| `turbovec0` chunked ({chunk_count} chunks) | "
        f"{chunk_insert * 1_000:.2f} | {chunk_size / 1_048_576:.2f} | "
        f"{chunk_query * 1_000 / args.queries:.3f} |"
    )


if __name__ == "__main__":
    main()
