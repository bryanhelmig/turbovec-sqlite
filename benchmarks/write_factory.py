#!/usr/bin/env python3
"""Moderate-scale SQLite write score with persistence guardrails."""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import sqlite3
import statistics
import struct
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any


@dataclass
class Run:
    mutation_ms: float
    commit_ms: float
    total_ms: float
    single_ms: float
    rollback_ms: float
    reopen_ms: float
    wal_bytes: int


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--extension", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=50_000)
    parser.add_argument("--dimensions", type=int, default=384)
    parser.add_argument("--batch", type=int, default=100)
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    if args.rows < 128 or not 0 < args.batch < args.rows:
        parser.error("rows must be at least 128 and batch must be between 1 and rows")
    if args.dimensions < 8 or args.dimensions % 8:
        parser.error("dimensions must be a positive multiple of 8")
    if args.repetitions < 1 or not args.extension.is_file():
        parser.error("repetitions must be positive and extension must exist")
    return args


def vector(rowid: int, dimensions: int) -> bytes:
    """Cheap deterministic input; geometry, not semantic quality, drives this test."""
    payload = bytearray(dimensions * 4)
    for index, value in enumerate((0.70, -0.45, 0.32, -0.24, 0.18, -0.13, 0.09, -0.06)):
        coordinate = (rowid * 17 + index * 53) % dimensions
        struct.pack_into("<f", payload, coordinate * 4, value)
    return bytes(payload)


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension.resolve()))
    connection.enable_load_extension(False)
    connection.execute("pragma synchronous=full")
    connection.execute("pragma wal_autocheckpoint=0")
    return connection


def build_base(database: Path, extension: Path, rows: int, dimensions: int) -> float:
    connection = connect(database, extension)
    if connection.execute("pragma journal_mode=wal").fetchone()[0] != "wal":
        raise AssertionError("WAL mode was not enabled")
    connection.execute(
        f"create virtual table vectors using turbovec0(dimensions={dimensions}, bit_width=4)"
    )
    started = time.perf_counter_ns()
    with connection:
        connection.executemany(
            "insert into vectors(rowid, embedding) values (?, ?)",
            ((rowid, vector(rowid, dimensions)) for rowid in range(1, rows + 1)),
        )
    build_ms = (time.perf_counter_ns() - started) / 1_000_000
    if connection.execute("select count(*) from vectors").fetchone()[0] != rows:
        raise AssertionError("base row count mismatch")
    connection.execute("pragma wal_checkpoint(truncate)")
    connection.close()
    return build_ms


def run_once(
    database: Path,
    extension: Path,
    rows: int,
    dimensions: int,
    batch: int,
) -> Run:
    connection = connect(database, extension)
    connection.execute("select count(*) from vectors").fetchone()
    connection.execute("pragma wal_checkpoint(truncate)")
    inserted = list(range(rows + 1, rows + batch + 1))
    deleted = list(range(1, batch + 1))

    total_started = time.perf_counter_ns()
    connection.execute("begin immediate")
    mutation_started = time.perf_counter_ns()
    connection.executemany(
        "insert into vectors(rowid, embedding) values (?, ?)",
        ((rowid, vector(rowid, dimensions)) for rowid in inserted),
    )
    connection.executemany("delete from vectors where rowid=?", ((rowid,) for rowid in deleted))
    mutation_ms = (time.perf_counter_ns() - mutation_started) / 1_000_000
    commit_started = time.perf_counter_ns()
    connection.commit()
    commit_ms = (time.perf_counter_ns() - commit_started) / 1_000_000
    total_ms = (time.perf_counter_ns() - total_started) / 1_000_000

    wal = database.with_name(database.name + "-wal")
    wal_bytes = os.path.getsize(wal) if wal.exists() else 0
    expected_count = rows
    if connection.execute("select count(*) from vectors").fetchone()[0] != expected_count:
        raise AssertionError("committed row count mismatch")
    if connection.execute("select count(*) from vectors where rowid=1").fetchone()[0] != 0:
        raise AssertionError("deleted row remained visible")
    if connection.execute(
        "select count(*) from vectors where rowid=?", (inserted[-1],)
    ).fetchone()[0] != 1:
        raise AssertionError("inserted row was not visible")

    single_id = rows + batch + 1
    single_started = time.perf_counter_ns()
    connection.execute("begin immediate")
    connection.execute(
        "insert into vectors(rowid, embedding) values (?, ?)",
        (single_id, vector(single_id, dimensions)),
    )
    connection.commit()
    single_ms = (time.perf_counter_ns() - single_started) / 1_000_000
    expected_count += 1

    connection.execute("begin immediate")
    rollback_started = time.perf_counter_ns()
    rollback_id = single_id + 1
    connection.execute(
        "insert into vectors(rowid, embedding) values (?, ?)",
        (rollback_id, vector(rollback_id, dimensions)),
    )
    connection.rollback()
    if connection.execute(
        "select count(*) from vectors where rowid=?", (rollback_id,)
    ).fetchone()[0] != 0:
        raise AssertionError("rolled-back row remained visible")
    rollback_ms = (time.perf_counter_ns() - rollback_started) / 1_000_000
    connection.close()

    reopen_started = time.perf_counter_ns()
    reopened = connect(database, extension)
    if reopened.execute("select count(*) from vectors").fetchone()[0] != expected_count:
        raise AssertionError("reopened row count mismatch")
    reopen_ms = (time.perf_counter_ns() - reopen_started) / 1_000_000
    reports = [row[0] for row in reopened.execute("pragma integrity_check")]
    if reports != ["ok"]:
        raise AssertionError(f"integrity check failed: {reports}")
    reopened.close()
    return Run(
        mutation_ms,
        commit_ms,
        total_ms,
        single_ms,
        rollback_ms,
        reopen_ms,
        wal_bytes,
    )


def main() -> None:
    args = arguments()
    with tempfile.TemporaryDirectory(prefix="turbovec-write-factory-") as directory:
        root = Path(directory)
        base = root / "base.db"
        build_ms = build_base(base, args.extension, args.rows, args.dimensions)
        database_bytes = base.stat().st_size
        runs: list[Run] = []
        for repetition in range(args.repetitions):
            database = root / f"run-{repetition}.db"
            shutil.copyfile(base, database)
            runs.append(
                run_once(database, args.extension, args.rows, args.dimensions, args.batch)
            )

    medians = {
        field: statistics.median(getattr(run, field) for run in runs)
        for field in asdict(runs[0])
    }
    mutations = args.batch * 2
    score = mutations * 1_000.0 / medians["total_ms"]
    result: dict[str, Any] = {
        "rows": args.rows,
        "dimensions": args.dimensions,
        "batch": args.batch,
        "mutations_per_transaction": mutations,
        "repetitions": args.repetitions,
        "build_ms": build_ms,
        "database_mib": database_bytes / 1_048_576,
        "score_mutations_per_second": score,
        "median": medians,
        "sqlite_version": sqlite3.sqlite_version,
        "python_version": platform.python_version(),
        "platform": platform.platform(),
    }

    print(
        f"rows={args.rows}, dim={args.dimensions}, mutations={mutations}, "
        f"repetitions={args.repetitions}"
    )
    print(f"base build: {build_ms:.1f} ms; database: {result['database_mib']:.2f} MiB")
    print()
    print("| score mut/s | transaction ms | mutate ms | commit ms | single ms | rollback ms | reopen ms | WAL KiB |")
    print("|---:|---:|---:|---:|---:|---:|---:|---:|")
    print(
        f"| {score:.1f} | {medians['total_ms']:.3f} | {medians['mutation_ms']:.3f} | "
        f"{medians['commit_ms']:.3f} | {medians['single_ms']:.3f} | "
        f"{medians['rollback_ms']:.3f} | "
        f"{medians['reopen_ms']:.3f} | {medians['wal_bytes'] / 1024:.1f} |"
    )
    print("Correctness: commit, delete, rollback, reopen, row count, and integrity checks passed.")

    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
