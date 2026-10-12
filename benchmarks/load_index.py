#!/usr/bin/env python3
"""Compare first-search time and peak RSS on one unchanged database.

Each sample uses a fresh process and a warm OS page cache. Alternate variant
order and compare exact rowids/scores for every cold and warm query. Inputs are
synthetic sparse vectors: this measures loading, not embedding recall.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import sqlite3
import statistics
import subprocess
import sys
import time
from pathlib import Path

from fresh_commit import build, connect, peak_rss, vector


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extension", required=True, type=Path)
    parser.add_argument("--baseline-extension", required=True, type=Path)
    parser.add_argument("--database", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=1_000_000)
    parser.add_argument("--dimensions", type=int, default=1536)
    parser.add_argument("--repeat", type=int, default=10)
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--json", type=Path)
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if min(args.rows, args.dimensions, args.repeat, args.threads) <= 0:
        parser.error("rows, dimensions, repeat and threads must be positive")
    return args


def child(args: argparse.Namespace) -> None:
    connection = connect(args.database, args.extension)
    connection.execute("pragma query_only=on")
    queries = [vector(i * 7919 + 1, args.dimensions, revision=9) for i in range(10)]
    rss_before = peak_rss()
    elapsed = []
    results = []
    for query in queries:
        started = time.perf_counter()
        rows = connection.execute(
            "select rowid, score from vectors where embedding match ? and k=10",
            (query,),
        ).fetchall()
        elapsed.append((time.perf_counter() - started) * 1000)
        assert len(rows) == min(10, args.rows), rows
        results.append(rows)
    connection.close()
    print(json.dumps({
        "first_search_ms": elapsed[0],
        "warm_search_ms_median": statistics.median(elapsed[1:]),
        "rss_before_bytes": rss_before,
        "peak_rss_bytes": peak_rss(),
        "result_sha256": hashlib.sha256(json.dumps(results).encode()).hexdigest(),
    }))


def run_child(args: argparse.Namespace, extension: Path) -> dict:
    result = subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), "--child",
         "--extension", str(extension.resolve()),
         "--baseline-extension", str(args.baseline_extension.resolve()),
         "--database", str(args.database.resolve()),
         "--rows", str(args.rows), "--dimensions", str(args.dimensions)],
        env={**os.environ, "RAYON_NUM_THREADS": str(args.threads)},
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main() -> None:
    args = arguments()
    if args.child:
        child(args)
        return
    if not args.database.exists():
        args.database.parent.mkdir(parents=True, exist_ok=True)
        build(args.database, args.baseline_extension, args.rows, args.dimensions, 200_000)
    with sqlite3.connect(args.database) as db:
        dimensions, rows, base_bytes = db.execute(
            "select dimensions, row_count, byte_len from vectors_info where id=1"
        ).fetchone()
        assert (dimensions, rows) == (args.dimensions, args.rows)
    db.close()
    before = digest(args.database)  # Also warms the OS page cache.
    variants = {"baseline": args.baseline_extension, "preallocated": args.extension}
    samples = {name: [] for name in variants}
    warmups = {name: run_child(args, path) for name, path in variants.items()}
    expected = warmups["baseline"]["result_sha256"]
    assert all(sample["result_sha256"] == expected for sample in warmups.values())
    order = []
    for repetition in range(args.repeat):
        names = list(variants) if repetition % 2 == 0 else list(reversed(variants))
        for name in names:
            sample = run_child(args, variants[name])
            assert sample["result_sha256"] == expected, (name, repetition, sample)
            samples[name].append(sample)
            order.append(name)
            print(f"{repetition + 1:2} {name:12} {sample['first_search_ms']:.1f} ms "
                  f"{sample['peak_rss_bytes'] / 2**20:.1f} MiB", flush=True)
    assert digest(args.database) == before, "benchmark changed its input database"
    report = {
        "platform": platform.platform(), "cpu_count": os.cpu_count(),
        "python": platform.python_version(), "sqlite": sqlite3.sqlite_version,
        "threads": args.threads, "page_cache": "warm", "rows": args.rows,
        "dimensions": args.dimensions, "base_bytes": base_bytes,
        "database_sha256": before, "result_sha256": expected,
        "extension_sha256": {name: digest(path) for name, path in variants.items()},
        "command": [sys.executable, *sys.argv], "order": order, "warmups": warmups,
        "results": {},
    }
    for name, values in samples.items():
        report["results"][name] = {
            "first_search_ms_median": statistics.median(v["first_search_ms"] for v in values),
            "warm_search_ms_median": statistics.median(v["warm_search_ms_median"] for v in values),
            "peak_rss_mib_median": statistics.median(v["peak_rss_bytes"] for v in values) / 2**20,
            "samples": values,
        }
    if args.json:
        args.json.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({name: {key: value for key, value in result.items() if key != "samples"}
                      for name, result in report["results"].items()}, indent=2))


if __name__ == "__main__":
    main()
