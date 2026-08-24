#!/usr/bin/env python3
"""Seeded correctness, quality, size, and latency comparison with sqlite-vec."""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import sqlite3
import statistics
import struct
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Engine:
    name: str
    extension: Path
    create_sql: str
    query_sql: str
    smaller_is_better: bool


@dataclass
class Run:
    insert_ms: float
    query_ms: list[float]
    reopen_ms: float
    database_bytes: int
    query_ids: list[list[int]]


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare sqlite-vec exact cosine search with turbovec0"
    )
    parser.add_argument("--turbovec-extension", required=True, type=Path)
    parser.add_argument("--sqlite-vec-extension", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=10_000)
    parser.add_argument("--dimensions", type=int, default=256)
    parser.add_argument("--queries", type=int, default=100)
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=10)
    parser.add_argument("--seed", type=lambda value: int(value, 0), default=0x5EED)
    parser.add_argument(
        "--score-engine",
        choices=("turbovec0-4bit", "turbovec0-3bit", "turbovec0-2bit"),
        default="turbovec0-4bit",
    )
    parser.add_argument("--minimum-recall", type=float, default=0.75)
    parser.add_argument("--json", type=Path, help="also write machine-readable results")
    args = parser.parse_args()
    if args.rows < 128:
        parser.error("rows must be at least 128")
    if args.dimensions < 8 or args.dimensions % 8:
        parser.error("dimensions must be a positive multiple of 8")
    if not 0 < args.k <= args.rows:
        parser.error("k must be between 1 and rows")
    if args.queries < 1 or args.repetitions < 1 or args.warmup < 0:
        parser.error("queries and repetitions must be positive; warmup cannot be negative")
    if not 0.0 <= args.minimum_recall <= 1.0:
        parser.error("minimum recall must be between zero and one")
    for extension in (args.turbovec_extension, args.sqlite_vec_extension):
        if not extension.is_file():
            parser.error(f"extension does not exist: {extension}")
    return args


def normalized_rows(count: int, dimensions: int, seed: int) -> list[bytes]:
    """Generate stable float32 unit vectors with a small xorshift PRNG."""
    state = seed | 1
    rows: list[bytes] = []
    for _ in range(count):
        values: list[float] = []
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


def connect(path: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(path)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension.resolve()))
    connection.enable_load_extension(False)
    connection.execute("pragma journal_mode=delete")
    connection.execute("pragma synchronous=full")
    connection.execute("pragma cache_size=-65536")
    connection.execute("pragma temp_store=memory")
    return connection


def query(connection: sqlite3.Connection, engine: Engine, vector: bytes, k: int):
    rows = connection.execute(engine.query_sql, (vector, k)).fetchall()
    ids = [int(row[0]) for row in rows]
    values = [float(row[1]) for row in rows]
    if len(ids) != k or len(ids) != len(set(ids)):
        raise AssertionError(f"{engine.name} returned invalid top-k IDs: {ids}")
    ordered = all(
        (a <= b if engine.smaller_is_better else a >= b)
        for a, b in zip(values, values[1:])
    )
    if not ordered:
        raise AssertionError(f"{engine.name} returned scores out of order")
    return ids


def exact_top_k(database: list[bytes], vector: bytes, dimensions: int, k: int):
    needle = struct.unpack(f"<{dimensions}f", vector)
    scored = []
    for rowid, blob in enumerate(database, 1):
        candidate = struct.unpack(f"<{dimensions}f", blob)
        score = sum(a * b for a, b in zip(needle, candidate))
        scored.append((score, rowid))
    scored.sort(key=lambda item: (-item[0], item[1]))
    return [rowid for _, rowid in scored[:k]]


def common_contract(engine: Engine, dimensions: int, vectors: list[bytes], queries: list[bytes], k: int):
    """Exercise the common portable subset independently of benchmark timing."""
    count = 128
    check_k = min(k, 10)
    with tempfile.TemporaryDirectory(prefix="turbovec-vec-correctness-") as directory:
        database = Path(directory) / f"{engine.name}.db"
        connection = connect(database, engine.extension)
        connection.execute(engine.create_sql)
        with connection:
            connection.executemany(
                "insert into vectors(rowid, embedding) values (?, ?)",
                ((rowid, blob) for rowid, blob in enumerate(vectors[:count], 1)),
            )
        if connection.execute("select count(*) from vectors").fetchone()[0] != count:
            raise AssertionError(f"{engine.name} count mismatch after insert")

        before = query(connection, engine, queries[0], check_k)
        if engine.name == "sqlite-vec":
            expected = exact_top_k(vectors[:count], queries[0], dimensions, check_k)
            if before != expected:
                raise AssertionError(
                    f"sqlite-vec disagrees with direct exact cosine truth: {before} != {expected}"
                )

        connection.execute("begin immediate")
        connection.execute(
            "insert into vectors(rowid, embedding) values (?, ?)",
            (count + 1, queries[1]),
        )
        connection.rollback()
        if connection.execute("select count(*) from vectors").fetchone()[0] != count:
            raise AssertionError(f"{engine.name} did not roll back an insert")
        if count + 1 in query(connection, engine, queries[1], check_k):
            raise AssertionError(f"{engine.name} searched a rolled-back row")

        connection.execute("delete from vectors where rowid = ?", (count,))
        connection.commit()
        if connection.execute("select count(*) from vectors").fetchone()[0] != count - 1:
            raise AssertionError(f"{engine.name} did not persist a delete")
        if count in query(connection, engine, vectors[count - 1], check_k):
            raise AssertionError(f"{engine.name} searched a deleted row")
        expected_after_reopen = query(connection, engine, queries[0], check_k)
        connection.close()

        reopened = connect(database, engine.extension)
        actual_after_reopen = query(reopened, engine, queries[0], check_k)
        if actual_after_reopen != expected_after_reopen:
            raise AssertionError(f"{engine.name} changed results after reopen")
        reopened.close()


def run_engine(
    engine: Engine,
    database: Path,
    vectors: list[bytes],
    queries: list[bytes],
    k: int,
    warmup: int,
) -> Run:
    connection = connect(database, engine.extension)
    connection.execute(engine.create_sql)
    started = time.perf_counter_ns()
    with connection:
        connection.executemany(
            "insert into vectors(rowid, embedding) values (?, ?)",
            ((rowid, blob) for rowid, blob in enumerate(vectors, 1)),
        )
    insert_ms = (time.perf_counter_ns() - started) / 1_000_000

    if connection.execute("select count(*) from vectors").fetchone()[0] != len(vectors):
        raise AssertionError(f"{engine.name} row count mismatch")
    for vector in queries[:warmup]:
        query(connection, engine, vector, k)

    latencies: list[float] = []
    results: list[list[int]] = []
    for vector in queries:
        started = time.perf_counter_ns()
        ids = query(connection, engine, vector, k)
        if any(rowid < 1 or rowid > len(vectors) for rowid in ids):
            raise AssertionError(f"{engine.name} returned an unknown rowid")
        latencies.append((time.perf_counter_ns() - started) / 1_000_000)
        results.append(ids)

    page_size = connection.execute("pragma page_size").fetchone()[0]
    pages = connection.execute("pragma page_count").fetchone()[0]
    database_bytes = int(page_size * pages)
    expected = results[0]
    connection.close()

    reopened = connect(database, engine.extension)
    started = time.perf_counter_ns()
    after_reopen = query(reopened, engine, queries[0], k)
    reopen_ms = (time.perf_counter_ns() - started) / 1_000_000
    reopened.close()
    if after_reopen != expected:
        raise AssertionError(f"{engine.name} changed top-k after reopen")
    if os.path.getsize(database) != database_bytes:
        raise AssertionError(f"{engine.name} page count and file size disagree")
    return Run(insert_ms, latencies, reopen_ms, database_bytes, results)


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    index = max(0, math.ceil(len(ordered) * fraction) - 1)
    return ordered[index]


def recall(expected: list[list[int]], actual: list[list[int]]) -> float:
    matches = sum(
        len(set(wanted).intersection(found))
        for wanted, found in zip(expected, actual)
    )
    return matches / sum(map(len, expected))


def top1(expected: list[list[int]], actual: list[list[int]]) -> float:
    return sum(a[0] == b[0] for a, b in zip(expected, actual)) / len(expected)


def aggregate(engine: Engine, runs: list[Run], truth: list[list[int]]) -> dict[str, Any]:
    canonical = runs[0].query_ids
    if any(run.query_ids != canonical for run in runs[1:]):
        raise AssertionError(f"{engine.name} produced nondeterministic rankings")
    query_ms = [latency for run in runs for latency in run.query_ms]
    return {
        "engine": engine.name,
        "insert_ms_median": statistics.median(run.insert_ms for run in runs),
        "query_ms_median": statistics.median(query_ms),
        "query_ms_p95": percentile(query_ms, 0.95),
        "reopen_ms_median": statistics.median(run.reopen_ms for run in runs),
        "database_mib_median": statistics.median(run.database_bytes for run in runs)
        / 1_048_576,
        "recall_at_k": recall(truth, canonical),
        "top1_agreement": top1(truth, canonical),
        "quality_adjusted_qps": recall(truth, canonical)
        * 1_000.0
        / statistics.median(query_ms),
    }


def main() -> None:
    args = arguments()
    vectors = normalized_rows(args.rows, args.dimensions, args.seed)
    queries = normalized_rows(args.queries, args.dimensions, args.seed ^ 0xBAD5EED)
    engines = [
        Engine(
            "sqlite-vec",
            args.sqlite_vec_extension,
            f"create virtual table vectors using vec0(embedding float[{args.dimensions}] distance_metric=cosine)",
            "select rowid, distance from vectors where embedding match ? order by distance limit ?",
            True,
        ),
        Engine(
            "turbovec0-4bit",
            args.turbovec_extension,
            f"create virtual table vectors using turbovec0(dimensions={args.dimensions}, bit_width=4)",
            "select rowid, score from vectors where embedding match ? order by score desc limit ?",
            False,
        ),
        Engine(
            "turbovec0-3bit",
            args.turbovec_extension,
            f"create virtual table vectors using turbovec0(dimensions={args.dimensions}, bit_width=3)",
            "select rowid, score from vectors where embedding match ? order by score desc limit ?",
            False,
        ),
        Engine(
            "turbovec0-2bit",
            args.turbovec_extension,
            f"create virtual table vectors using turbovec0(dimensions={args.dimensions}, bit_width=2)",
            "select rowid, score from vectors where embedding match ? order by score desc limit ?",
            False,
        ),
    ]

    for engine in engines:
        common_contract(engine, args.dimensions, vectors, queries, args.k)

    runs: dict[str, list[Run]] = {engine.name: [] for engine in engines}
    with tempfile.TemporaryDirectory(prefix="turbovec-vec-benchmark-") as directory:
        root = Path(directory)
        for repetition in range(args.repetitions):
            order = engines if repetition % 2 == 0 else list(reversed(engines))
            for engine in order:
                database = root / f"{engine.name}-{repetition}.db"
                runs[engine.name].append(
                    run_engine(engine, database, vectors, queries, args.k, args.warmup)
                )

    truth = runs["sqlite-vec"][0].query_ids
    summaries = [aggregate(engine, runs[engine.name], truth) for engine in engines]
    scored = next(result for result in summaries if result["engine"] == args.score_engine)
    score_eligible = scored["recall_at_k"] >= args.minimum_recall
    metadata = {
        "rows": args.rows,
        "dimensions": args.dimensions,
        "queries": args.queries,
        "k": args.k,
        "repetitions": args.repetitions,
        "warmup_queries": min(args.warmup, args.queries),
        "seed": args.seed,
        "sqlite_version": sqlite3.sqlite_version,
        "python_version": platform.python_version(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "rayon_num_threads": os.environ.get("RAYON_NUM_THREADS", "default"),
        "sqlite_vec_version": "0.1.9",
        "turbovec_sqlite_version": "0.1.3",
        "metric": {
            "name": "quality-adjusted queries per second",
            "engine": args.score_engine,
            "minimum_recall": args.minimum_recall,
            "eligible": score_eligible,
            "score": scored["quality_adjusted_qps"] if score_eligible else None,
        },
        "results": summaries,
    }

    print(
        f"rows={args.rows}, dim={args.dimensions}, queries={args.queries}, "
        f"k={args.k}, repetitions={args.repetitions}, seed={args.seed:#x}, "
        f"rayon_threads={metadata['rayon_num_threads']}"
    )
    print()
    print("| engine | insert ms | DB MiB | query p50 ms | query p95 ms | reopen ms | recall@k | top1 |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for result in summaries:
        print(
            f"| {result['engine']} | {result['insert_ms_median']:.2f} | "
            f"{result['database_mib_median']:.2f} | {result['query_ms_median']:.3f} | "
            f"{result['query_ms_p95']:.3f} | {result['reopen_ms_median']:.3f} | "
            f"{result['recall_at_k']:.3f} | {result['top1_agreement']:.3f} |"
        )
    print()
    print(
        "Correctness: direct exact-ranking oracle, rollback, delete, count, "
        "ordering, and reopen checks passed."
    )
    eligibility = "eligible" if score_eligible else "INELIGIBLE"
    print(
        f"Factory score ({args.score_engine}): "
        f"{scored['quality_adjusted_qps']:.1f} quality-adjusted QPS; "
        f"{eligibility} at recall >= {args.minimum_recall:.2f}."
    )

    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
