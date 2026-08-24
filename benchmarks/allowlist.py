#!/usr/bin/env python3
"""End-to-end latency and correctness for rowid allowlist pushdown."""

from __future__ import annotations

import argparse
import math
import sqlite3
import statistics
import struct
import tempfile
import time
from pathlib import Path


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--extension", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=20_000)
    parser.add_argument("--dimensions", type=int, default=256)
    parser.add_argument("--queries", type=int, default=50)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--k", type=int, default=10)
    args = parser.parse_args()
    if args.rows < 1_000:
        parser.error("rows must be at least 1,000")
    if args.dimensions < 8 or args.dimensions % 8:
        parser.error("dimensions must be a positive multiple of 8")
    if not 0 < args.k <= args.rows:
        parser.error("k must be between 1 and rows")
    if args.queries < 1 or args.warmup < 0:
        parser.error("queries must be positive and warmup cannot be negative")
    if not args.extension.is_file():
        parser.error(f"extension does not exist: {args.extension}")
    return args


def normalized_rows(count: int, dimensions: int, seed: int) -> list[bytes]:
    state = seed | 1
    rows: list[bytes] = []
    for _ in range(count):
        values = []
        norm = 0.0
        for _ in range(dimensions):
            state ^= (state << 13) & 0xFFFFFFFFFFFFFFFF
            state ^= state >> 7
            state ^= (state << 17) & 0xFFFFFFFFFFFFFFFF
            state &= 0xFFFFFFFFFFFFFFFF
            value = ((state >> 40) / float(1 << 23)) - 0.5
            values.append(value)
            norm += value * value
        scale = 1.0 / math.sqrt(norm)
        rows.append(struct.pack(f"<{dimensions}f", *(value * scale for value in values)))
    return rows


def percentile(values: list[float], fraction: float) -> float:
    return sorted(values)[round((len(values) - 1) * fraction)]


def main() -> None:
    args = arguments()
    vectors = normalized_rows(args.rows, args.dimensions, 0xA110)
    queries = normalized_rows(args.queries + args.warmup, args.dimensions, 0xC0FFEE)
    clustered_ten = range(1, args.rows // 10 + 1)
    scattered_ten = range(10, args.rows + 1, 10)
    clustered_one = range(1, args.rows // 100 + 1)
    cohorts = {
        "clustered 10%": set(clustered_ten),
        "scattered 10%": set(scattered_ten),
        "clustered 1%": set(clustered_one),
    }

    with tempfile.TemporaryDirectory(prefix="turbovec-allowlist-bench-") as directory:
        connection = sqlite3.connect(Path(directory) / "bench.db")
        connection.enable_load_extension(True)
        connection.load_extension(str(args.extension.resolve()))
        connection.enable_load_extension(False)
        connection.execute("pragma temp_store=memory")
        connection.execute("create table eligible(cohort text, id integer, primary key(cohort,id))")
        connection.execute(
            f"create virtual table vectors using turbovec0(dimensions={args.dimensions}, bit_width=4)"
        )
        with connection:
            connection.executemany(
                "insert into vectors(rowid,embedding) values (?,?)",
                enumerate(vectors, 1),
            )
            connection.executemany(
                "insert into eligible(cohort,id) values (?,?)",
                (
                    (cohort, rowid)
                    for cohort, rowids in cohorts.items()
                    for rowid in rowids
                ),
            )

        full_sql = (
            "select rowid,score from vectors where embedding match ? "
            "order by score desc limit ?"
        )
        allowed_sql = (
            "select rowid,score from vectors where embedding match ? "
            "and rowid in (select id from eligible where cohort=?) "
            "order by score desc limit ?"
        )

        # One exhaustive result proves every selective result is the true top-k
        # under the same compressed score, rather than an overfetch sample.
        full_ranking = connection.execute(full_sql, (queries[0], args.rows)).fetchall()
        for cohort, allowed in cohorts.items():
            expected = [rowid for rowid, _ in full_ranking if rowid in allowed][: args.k]
            actual = [
                rowid
                for rowid, _ in connection.execute(
                    allowed_sql, (queries[0], cohort, args.k)
                )
            ]
            if actual != expected:
                raise AssertionError(f"{cohort} top-k mismatch: {actual} != {expected}")

        cases: list[tuple[str, str, tuple[object, ...]]] = [
            ("all rows", full_sql, (args.k,)),
            *(
                (cohort, allowed_sql, (cohort, args.k))
                for cohort in cohorts
            ),
        ]
        results: list[tuple[str, float, float]] = []
        for name, sql, tail in cases:
            for query in queries[: args.warmup]:
                connection.execute(sql, (query, *tail)).fetchall()
            latencies = []
            for query in queries[args.warmup :]:
                started = time.perf_counter_ns()
                rows = connection.execute(sql, (query, *tail)).fetchall()
                latencies.append((time.perf_counter_ns() - started) / 1_000_000)
                if len(rows) != args.k:
                    raise AssertionError(f"{name} returned {len(rows)} rows")
            p50 = statistics.median(latencies)
            results.append((name, p50, percentile(latencies, 0.95)))

    full_p50 = results[0][1]
    print(
        f"rows={args.rows}, dim={args.dimensions}, queries={args.queries}, "
        f"k={args.k}; selection cost included"
    )
    print()
    print("| eligible rows | p50 ms | p95 ms | vs full |")
    print("|---|---:|---:|---:|")
    for name, p50, p95 in results:
        print(f"| {name} | {p50:.3f} | {p95:.3f} | {full_p50 / p50:.2f}x |")
    print("Correctness: each selective top-k matched the filtered full ranking.")


if __name__ == "__main__":
    main()
