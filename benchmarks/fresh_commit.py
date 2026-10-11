#!/usr/bin/env python3
"""Time a one-row commit from a fresh process, the way a sync tool runs.

Each sample is a new process that loads the extension, deletes one row,
inserts it again with a new vector, and commits. It reports the transaction's
wall time and the process's peak RSS. A second sample shape runs one search
from a fresh process, which is where the index load moves.

The base index is built once with the baseline extension, so the candidate's
first commit on a copy of it is also the one-time storage upgrade, reported
separately. The page cache is warm for every case; a cold or busy disk makes
the baseline's full read slower, not the candidate's.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import sqlite3
import statistics
import struct
import subprocess
import sys
import time
from pathlib import Path


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extension", required=True, type=Path)
    parser.add_argument("--baseline-extension", required=True, type=Path)
    parser.add_argument("--workdir", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=1_000_000)
    parser.add_argument("--dimensions", type=int, default=1536)
    parser.add_argument("--batch", type=int, default=200_000)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--rebuild", action="store_true")
    parser.add_argument("--json", type=Path)
    parser.add_argument("--child", choices=("commit", "search"), help=argparse.SUPPRESS)
    parser.add_argument("--database", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--rowid", type=int, default=1, help=argparse.SUPPRESS)
    parser.add_argument("--revision", type=int, default=1, help=argparse.SUPPRESS)
    return parser.parse_args()


def vector(rowid: int, dimensions: int, *, revision: int = 0) -> bytes:
    """Cheap, deterministic full-width input with 16 non-zero coordinates."""
    payload = bytearray(dimensions * 4)
    state = (rowid * 0x9E3779B185EBCA87 + revision * 0xD1B54A32D192ED03) & ((1 << 64) - 1)
    for _ in range(16):
        state ^= (state << 13) & ((1 << 64) - 1)
        state ^= state >> 7
        state ^= (state << 17) & ((1 << 64) - 1)
        coordinate = state % dimensions
        value = ((state >> 40) / float(1 << 23)) - 0.5
        struct.pack_into("<f", payload, coordinate * 4, value)
    return bytes(payload)


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database, isolation_level=None)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension.resolve()))
    connection.enable_load_extension(False)
    return connection


def peak_rss() -> int:
    import resource

    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return peak if platform.system() == "Darwin" else peak * 1024


def child(args: argparse.Namespace) -> None:
    started = time.perf_counter()
    connection = connect(args.database, args.extension)
    rss_before = peak_rss()
    opened = time.perf_counter()
    if args.child == "commit":
        replacement = vector(args.rowid, args.dimensions, revision=args.revision)
        connection.execute("begin immediate")
        connection.execute("delete from vectors where rowid=?", (args.rowid,))
        connection.execute(
            "insert into vectors(rowid,embedding) values(?,?)", (args.rowid, replacement)
        )
        connection.execute("commit")
    else:
        query = vector(args.rowid, args.dimensions, revision=args.revision)
        rows = connection.execute(
            "select rowid from vectors where embedding match ? and k=10", (query,)
        ).fetchall()
        assert len(rows) == 10, rows
    finished = time.perf_counter()
    connection.close()
    print(json.dumps({
        "open_ms": (opened - started) * 1000,
        "operation_ms": (finished - opened) * 1000,
        "rss_before_bytes": rss_before,
        "peak_rss_bytes": peak_rss(),
    }))


def run_child(kind: str, extension: Path, database: Path, dimensions: int, rowid: int,
              revision: int) -> dict:
    output = subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), "--extension", str(extension),
         "--baseline-extension", str(extension), "--workdir", str(database.parent),
         "--dimensions", str(dimensions), "--child", kind, "--database", str(database),
         "--rowid", str(rowid), "--revision", str(revision)],
        check=True, capture_output=True, text=True,
    )
    return json.loads(output.stdout)


def info(database: Path, extension: Path) -> dict:
    connection = connect(database, extension)
    result = json.loads(connection.execute("select turbovec_info('vectors')").fetchone()[0])
    connection.close()
    return result


def build(database: Path, extension: Path, rows: int, dimensions: int, batch: int) -> float:
    for suffix in ("", "-wal", "-shm"):
        Path(str(database) + suffix).unlink(missing_ok=True)
    started = time.perf_counter()
    connection = connect(database, extension)
    connection.execute("pragma journal_mode=wal")
    connection.execute(
        f"create virtual table vectors using turbovec0(dimensions={dimensions},bit_width=4)"
    )
    for start in range(1, rows + 1, batch):
        stop = min(start + batch, rows + 1)
        connection.execute("begin immediate")
        connection.executemany(
            "insert into vectors(rowid,embedding) values(?,?)",
            ((rowid, vector(rowid, dimensions)) for rowid in range(start, stop)),
        )
        connection.execute("commit")
        print(f"  built {stop - 1:,} rows", file=sys.stderr, flush=True)
    connection.execute("pragma wal_checkpoint(truncate)")
    connection.close()
    return time.perf_counter() - started


def copy(source: Path, destination: Path) -> None:
    for suffix in ("-wal", "-shm"):
        Path(str(destination) + suffix).unlink(missing_ok=True)
    shutil.copyfile(source, destination)


def summarize(samples: list[dict]) -> dict:
    return {
        "operation_ms_median": statistics.median(sample["operation_ms"] for sample in samples),
        "peak_rss_mib_median": statistics.median(
            sample["peak_rss_bytes"] for sample in samples
        ) / 2**20,
        "samples": samples,
    }


def main() -> None:
    args = arguments()
    if args.child:
        child(args)
        return
    args.workdir.mkdir(parents=True, exist_ok=True)
    base = args.workdir / f"base-{args.rows}x{args.dimensions}.db"
    if args.rebuild or not base.exists():
        print(f"building {args.rows:,} x {args.dimensions} with the baseline", file=sys.stderr)
        build_seconds = build(
            base, args.baseline_extension, args.rows, args.dimensions, args.batch
        )
        print(f"  built in {build_seconds:.0f} s", file=sys.stderr)
    base_info = info(base, args.baseline_extension)
    assert base_info["count"] == args.rows, base_info

    legacy = args.workdir / "baseline.db"
    upgraded = args.workdir / "candidate.db"
    copy(base, legacy)
    copy(base, upgraded)
    rowids = [args.rows // 2 + offset * 7_919 for offset in range(args.repeat + 1)]
    results: dict[str, dict] = {}

    baseline = [
        run_child("commit", args.baseline_extension, legacy, args.dimensions, rowid, 1)
        for rowid in rowids[1:]
    ]
    results["baseline commit"] = summarize(baseline)
    upgrade = run_child("commit", args.extension, upgraded, args.dimensions, rowids[0], 1)
    results["candidate one-time upgrade commit"] = summarize([upgrade])
    candidate = [
        run_child("commit", args.extension, upgraded, args.dimensions, rowid, 1)
        for rowid in rowids[1:]
    ]
    results["candidate commit"] = summarize(candidate)
    results["baseline first search"] = summarize([
        run_child("search", args.baseline_extension, legacy, args.dimensions, rowid, 9)
        for rowid in rowids[1:]
    ])
    results["candidate first search"] = summarize([
        run_child("search", args.extension, upgraded, args.dimensions, rowid, 9)
        for rowid in rowids[1:]
    ])

    final = info(upgraded, args.extension)
    connection = connect(upgraded, args.extension)
    integrity = [row[0] for row in connection.execute("pragma integrity_check")]
    connection.close()
    assert integrity == ["ok"], integrity
    report = {
        "rows": args.rows,
        "dimensions": args.dimensions,
        "base_bytes": base_info["base_bytes"],
        "database_bytes": base.stat().st_size,
        "candidate_delta_operations": final["delta_operations"],
        "sqlite": sqlite3.sqlite_version,
        "platform": platform.platform(),
        "processor": platform.processor(),
        "cpu_count": os.cpu_count(),
        "results": results,
    }
    print(f"{args.rows:,} x {args.dimensions}, base {base_info['base_bytes'] / 2**20:.0f} MiB")
    print(f"{'case':38} {'median ms':>10} {'peak RSS MiB':>13}")
    for name, result in results.items():
        print(
            f"{name:38} {result['operation_ms_median']:10.1f} "
            f"{result['peak_rss_mib_median']:13.1f}"
        )
    if args.json:
        args.json.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
