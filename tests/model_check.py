#!/usr/bin/env python3
"""Deterministic transaction model check for the writable virtual table."""

from __future__ import annotations

import argparse
import math
import random
import sqlite3
import struct
import tempfile
import time
from pathlib import Path


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("extension", type=Path)
    parser.add_argument("--seeds", type=int, default=100)
    parser.add_argument("--steps", type=int, default=40)
    args = parser.parse_args()
    if args.seeds < 1 or args.steps < 1:
        parser.error("seeds and steps must be positive")
    return args


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension.resolve()))
    connection.enable_load_extension(False)
    return connection


def vector(rowid: int, version: int = 0) -> bytes:
    values = [((rowid * 17 + version * 31 + i * 13) % 101) - 50 for i in range(8)]
    norm = math.sqrt(sum(value * value for value in values))
    return struct.pack("<8f", *(value / norm for value in values))


def actual_ids(connection: sqlite3.Connection) -> set[int]:
    return {row[0] for row in connection.execute("select rowid from vectors")}


def assert_model(connection: sqlite3.Connection, expected: set[int]) -> None:
    actual = actual_ids(connection)
    if actual != expected:
        missing = sorted(expected - actual)[:10]
        extra = sorted(actual - expected)[:10]
        raise AssertionError(f"model mismatch: missing={missing}, extra={extra}")
    count = connection.execute("select count(*) from vectors").fetchone()[0]
    if count != len(expected):
        raise AssertionError(f"count mismatch: SQLite={count}, model={len(expected)}")


def main() -> None:
    args = arguments()
    operations = 0
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="turbovec-model-") as directory:
        database = Path(directory) / "model.db"
        connection = connect(database, args.extension)
        connection.execute(
            "create virtual table vectors using turbovec0(dimensions=8, bit_width=4)"
        )
        expected: set[int] = set()

        for seed in range(args.seeds):
            rng = random.Random(0x5EED + seed)
            before = set(expected)
            savepoint: set[int] | None = None
            connection.execute("begin immediate")
            for step in range(args.steps):
                choice = rng.randrange(100)
                rowid = rng.randrange(1, args.seeds * 4 + 65)
                if choice < 30:
                    connection.execute(
                        "insert or ignore into vectors(rowid, embedding) values (?, ?)",
                        (rowid, vector(rowid, step)),
                    )
                    expected.add(rowid)
                elif choice < 50:
                    connection.execute("delete from vectors where rowid=?", (rowid,))
                    expected.discard(rowid)
                elif choice < 65:
                    connection.execute(
                        "insert or replace into vectors(rowid, embedding) values (?, ?)",
                        (rowid, vector(rowid, step)),
                    )
                    expected.add(rowid)
                elif choice < 74 and expected:
                    duplicate = rng.choice(sorted(expected))
                    try:
                        connection.execute(
                            "insert or abort into vectors(rowid, embedding) values (?, ?)",
                            (duplicate, vector(duplicate, step)),
                        )
                    except sqlite3.IntegrityError:
                        pass
                    else:
                        raise AssertionError("duplicate ABORT insert succeeded")
                elif choice < 84 and savepoint is None:
                    connection.execute("savepoint model_point")
                    savepoint = set(expected)
                elif choice < 93 and savepoint is not None:
                    connection.execute("rollback to model_point")
                    expected = set(savepoint)
                elif savepoint is not None:
                    connection.execute("release model_point")
                    savepoint = None
                operations += 1

            if seed % 7 == 0:
                connection.rollback()
                expected = before
            else:
                connection.commit()
            assert_model(connection, expected)

            if seed % 10 == 9:
                connection.close()
                connection = connect(database, args.extension)
                assert_model(connection, expected)

        reports = [row[0] for row in connection.execute("pragma integrity_check")]
        if reports != ["ok"]:
            raise AssertionError(f"integrity check failed: {reports}")
        connection.close()

    elapsed = time.perf_counter() - started
    print(
        f"transaction model passed: seeds={args.seeds}, operations={operations}, "
        f"operations_per_second={operations / elapsed:.0f}"
    )


if __name__ == "__main__":
    main()
