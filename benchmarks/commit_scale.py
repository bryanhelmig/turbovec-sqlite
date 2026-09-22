#!/usr/bin/env python3
"""Measure turbovec0 write amplification at production geometry.

The reusable base database is deliberately built with the first extension and
copied for every case. This keeps mutations independent and makes a candidate
prove that it can open the previous implementation's index.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sqlite3
import statistics
import struct
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Iterable


@dataclass
class Result:
    name: str
    mutations: int
    mutation_ms: float
    commit_ms: float
    reopen_ms: float
    wal_bytes: int
    row_count: int
    search_digest: str


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extension", required=True, type=Path)
    parser.add_argument("--baseline-extension", type=Path)
    parser.add_argument("--workdir", required=True, type=Path)
    parser.add_argument("--rows", type=int, default=700_000)
    parser.add_argument("--dimensions", type=int, default=1536)
    parser.add_argument("--churn-rounds", type=int, default=20)
    parser.add_argument(
        "--case",
        action="append",
        help="run one named case (repeatable); use 'churn' for the churn case",
    )
    parser.add_argument("--rebuild", action="store_true")
    parser.add_argument("--keep-databases", action="store_true")
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    for extension in filter(None, (args.extension, args.baseline_extension)):
        if not extension.is_file():
            parser.error(f"extension does not exist: {extension}")
    if args.rows < 10_000:
        parser.error("rows must be at least 10,000")
    if args.dimensions < 8 or args.dimensions % 8:
        parser.error("dimensions must be a positive multiple of 8")
    if args.churn_rounds < 1:
        parser.error("churn-rounds must be positive")
    return args


def vector(rowid: int, dimensions: int, *, revision: int = 0) -> bytes:
    """Cheap, deterministic full-width input with 16 non-zero coordinates."""
    payload = bytearray(dimensions * 4)
    state = (rowid * 0x9E3779B185EBCA87 + revision * 0xD1B54A32D192ED03) & ((1 << 64) - 1)
    for index in range(16):
        state ^= (state << 13) & ((1 << 64) - 1)
        state ^= state >> 7
        state ^= (state << 17) & ((1 << 64) - 1)
        coordinate = state % dimensions
        value = ((state >> 40) / float(1 << 23)) - 0.5
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


def remove_database(database: Path) -> None:
    database.unlink(missing_ok=True)
    database.with_name(database.name + "-wal").unlink(missing_ok=True)
    database.with_name(database.name + "-shm").unlink(missing_ok=True)


def geometry_matches(database: Path, extension: Path, rows: int, dimensions: int) -> bool:
    if not database.exists():
        return False
    try:
        connection = connect(database, extension)
        info = json.loads(connection.execute("select turbovec_info('vectors')").fetchone()[0])
        connection.close()
        return info["count"] == rows and info["dimensions"] == dimensions and info["bit_width"] == 4
    except (sqlite3.Error, KeyError, json.JSONDecodeError):
        return False


def build_base(database: Path, extension: Path, rows: int, dimensions: int) -> float:
    remove_database(database)
    connection = connect(database, extension)
    if connection.execute("pragma journal_mode=wal").fetchone()[0] != "wal":
        raise AssertionError("WAL mode was not enabled")
    connection.execute(
        f"create virtual table vectors using turbovec0(dimensions={dimensions},bit_width=4)"
    )
    started = time.perf_counter_ns()
    connection.execute("begin immediate")
    connection.executemany(
        "insert into vectors(rowid,embedding) values(?,?)",
        ((rowid, vector(rowid, dimensions)) for rowid in range(1, rows + 1)),
    )
    connection.commit()
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if connection.execute("select count(*) from vectors").fetchone()[0] != rows:
        raise AssertionError("base row count mismatch")
    connection.execute("pragma wal_checkpoint(truncate)")
    connection.close()
    return elapsed_ms


def scattered(rows: int, count: int, *, offset: int = 0) -> list[int]:
    return [((index * rows) // count + offset) % rows + 1 for index in range(count)]


def digest_search(connection: sqlite3.Connection, dimensions: int) -> str:
    digest = hashlib.sha256()
    for query_id in (7, 101, 10_003, 900_001):
        query = vector(query_id, dimensions, revision=9)
        for rowid, score in connection.execute(
            "select rowid,score from vectors where embedding match ? "
            "order by score desc limit 40",
            (query,),
        ):
            digest.update(struct.pack("<Qf", rowid, score))
    return digest.hexdigest()


Mutation = Callable[[sqlite3.Connection], int]


def run_case(
    source: Path,
    database: Path,
    extension: Path,
    dimensions: int,
    name: str,
    mutate: Mutation,
) -> Result:
    shutil.copyfile(source, database)
    connection = connect(database, extension)
    connection.execute("select count(*) from vectors").fetchone()
    connection.execute("pragma wal_checkpoint(truncate)")
    connection.execute("begin immediate")
    started = time.perf_counter_ns()
    mutations = mutate(connection)
    mutation_ms = (time.perf_counter_ns() - started) / 1_000_000
    started = time.perf_counter_ns()
    connection.commit()
    commit_ms = (time.perf_counter_ns() - started) / 1_000_000
    wal = database.with_name(database.name + "-wal")
    wal_bytes = wal.stat().st_size if wal.exists() else 0
    row_count = connection.execute("select count(*) from vectors").fetchone()[0]
    search_digest = digest_search(connection, dimensions)
    connection.close()
    reopened = connect(database, extension)
    started = time.perf_counter_ns()
    reopened_count = reopened.execute("select count(*) from vectors").fetchone()[0]
    reopen_ms = (time.perf_counter_ns() - started) / 1_000_000
    if reopened_count != row_count:
        raise AssertionError(f"row count changed after reopening {name}")
    if digest_search(reopened, dimensions) != search_digest:
        raise AssertionError(f"search results changed after reopening {name}")
    if reopened.execute("pragma integrity_check").fetchall() != [("ok",)]:
        raise AssertionError(f"integrity check failed after {name}")
    reopened.close()
    return Result(
        name,
        mutations,
        mutation_ms,
        commit_ms,
        reopen_ms,
        wal_bytes,
        row_count,
        search_digest,
    )


def mutations(rows: int, dimensions: int) -> list[tuple[str, Mutation]]:
    delete_ids = scattered(rows, 200)
    large_delete_ids = scattered(rows, 5_000, offset=43)
    neighboring_delete_ids = list(range(rows // 2, rows // 2 + 200))
    replace_200 = scattered(rows, 200, offset=17)
    replace_5000 = scattered(rows, 5000, offset=31)

    def no_change(_connection: sqlite3.Connection) -> int:
        return 0

    def insert(count: int) -> Mutation:
        def apply(connection: sqlite3.Connection) -> int:
            connection.executemany(
                "insert into vectors(rowid,embedding) values(?,?)",
                (
                    (rows + index, vector(rows + index, dimensions))
                    for index in range(1, count + 1)
                ),
            )
            return count

        return apply

    def delete(ids: Iterable[int]) -> Mutation:
        ids = list(ids)

        def apply(connection: sqlite3.Connection) -> int:
            connection.executemany(
                "delete from vectors where rowid=?", ((rowid,) for rowid in ids)
            )
            return len(ids)

        return apply

    def replace(ids: Iterable[int], revision: int) -> Mutation:
        ids = list(ids)

        def apply(connection: sqlite3.Connection) -> int:
            connection.executemany(
                "insert or replace into vectors(rowid,embedding) values(?,?)",
                ((rowid, vector(rowid, dimensions, revision=revision)) for rowid in ids),
            )
            return len(ids)

        return apply

    return [
        ("no_change", no_change),
        ("insert_1", insert(1)),
        ("insert_200", insert(200)),
        ("delete_1", delete([rows // 2])),
        ("delete_200_scattered", delete(delete_ids)),
        ("delete_200_neighboring", delete(neighboring_delete_ids)),
        ("delete_5000_scattered", delete(large_delete_ids)),
        ("replace_200_scattered", replace(replace_200, 1)),
        ("replace_5000_scattered", replace(replace_5000, 2)),
    ]


def run_churn(
    source: Path,
    database: Path,
    extension: Path,
    rows: int,
    dimensions: int,
    rounds: int,
) -> dict[str, object]:
    shutil.copyfile(source, database)
    connection = connect(database, extension)
    connection.execute("select count(*) from vectors").fetchone()
    commit_ms: list[float] = []
    wal_bytes: list[int] = []
    for round_index in range(rounds):
        victims = scattered(rows, 200, offset=round_index * 977)
        first_new = rows + round_index * 200 + 1
        connection.execute("pragma wal_checkpoint(truncate)")
        connection.execute("begin immediate")
        connection.executemany("delete from vectors where rowid=?", ((rowid,) for rowid in victims))
        connection.executemany(
            "insert into vectors(rowid,embedding) values(?,?)",
            (
                (rowid, vector(rowid, dimensions, revision=round_index + 10))
                for rowid in range(first_new, first_new + 200)
            ),
        )
        started = time.perf_counter_ns()
        connection.commit()
        commit_ms.append((time.perf_counter_ns() - started) / 1_000_000)
        wal = database.with_name(database.name + "-wal")
        wal_bytes.append(wal.stat().st_size if wal.exists() else 0)
    result = {
        "rounds": rounds,
        "mutations_per_round": 400,
        "commit_ms_median": statistics.median(commit_ms),
        "commit_ms_max": max(commit_ms),
        "wal_bytes_median": statistics.median(wal_bytes),
        "wal_bytes_max": max(wal_bytes),
        "row_count": connection.execute("select count(*) from vectors").fetchone()[0],
        "search_digest": digest_search(connection, dimensions),
    }
    connection.close()
    reopened = connect(database, extension)
    reopened_digest = digest_search(reopened, dimensions)
    reopened.close()
    if reopened_digest != result["search_digest"]:
        raise AssertionError("search results changed after reopening churn database")
    return result


def main() -> None:
    args = arguments()
    args.workdir.mkdir(parents=True, exist_ok=True)
    baseline = (args.baseline_extension or args.extension).resolve()
    candidate = args.extension.resolve()
    base = args.workdir / f"base-{args.rows}x{args.dimensions}.db"
    build_ms = None
    if args.rebuild or not geometry_matches(base, baseline, args.rows, args.dimensions):
        print(f"building {args.rows:,} x {args.dimensions} base at {base}", flush=True)
        build_ms = build_base(base, baseline, args.rows, args.dimensions)
        print(f"base built in {build_ms / 1000:.1f} s", flush=True)

    variants = [("candidate", candidate)]
    if args.baseline_extension:
        variants.insert(0, ("baseline", baseline))
    output: dict[str, object] = {
        "rows": args.rows,
        "dimensions": args.dimensions,
        "base_bytes": base.stat().st_size,
        "build_ms": build_ms,
        "variants": {},
    }
    for variant, extension in variants:
        print(f"\n{variant}: {extension}", flush=True)
        results = []
        selected = set(args.case or ())
        for name, mutate in mutations(args.rows, args.dimensions):
            if selected and name not in selected:
                continue
            result = run_case(
                base,
                args.workdir / f"{variant}-{name}.db",
                extension,
                args.dimensions,
                name,
                mutate,
            )
            results.append(result)
            print(
                f"{name:24} commit={result.commit_ms:9.2f} ms "
                f"WAL={result.wal_bytes / 1_048_576:9.2f} MiB "
                f"reopen={result.reopen_ms:8.2f} ms",
                flush=True,
            )
            if not args.keep_databases:
                remove_database(args.workdir / f"{variant}-{name}.db")
        churn = None
        if not selected or "churn" in selected:
            churn = run_churn(
                base,
                args.workdir / f"{variant}-churn.db",
                extension,
                args.rows,
                args.dimensions,
                args.churn_rounds,
            )
            print(
                f"{'churn':24} commit p50={churn['commit_ms_median']:9.2f} ms "
                f"WAL p50={churn['wal_bytes_median'] / 1_048_576:9.2f} MiB",
                flush=True,
            )
            if not args.keep_databases:
                remove_database(args.workdir / f"{variant}-churn.db")
        output["variants"][variant] = {
            "extension": str(extension),
            "cases": [asdict(result) for result in results],
            "churn": churn,
        }

    if args.baseline_extension:
        before = output["variants"]["baseline"]
        after = output["variants"]["candidate"]
        for left, right in zip(before["cases"], after["cases"], strict=True):
            if (left["row_count"], left["search_digest"]) != (
                right["row_count"],
                right["search_digest"],
            ):
                raise AssertionError(f"semantic mismatch in {left['name']}")
        if (
            before["churn"] is not None
            and before["churn"]["search_digest"]
            != after["churn"]["search_digest"]
        ):
            raise AssertionError("semantic mismatch after churn")
        print("\nCorrectness: baseline and candidate counts and search results are identical.")

    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(output, indent=2) + "\n")


if __name__ == "__main__":
    main()
