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
    parser.add_argument("--dimensions", type=int, default=8)
    parser.add_argument("--initial-rows", type=int, default=0)
    args = parser.parse_args()
    if args.seeds < 1 or args.steps < 1 or args.initial_rows < 0:
        parser.error("seeds and steps must be positive; initial-rows cannot be negative")
    if args.dimensions < 8 or args.dimensions % 8:
        parser.error("dimensions must be a positive multiple of 8")
    return args


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension.resolve()))
    connection.enable_load_extension(False)
    return connection


def vector(rowid: int, dimensions: int, version: int = 0) -> bytes:
    """A vector that is near-orthogonal to every other (rowid, version) pair.

    The model asserts *which* vector each rowid holds, not just that the rowid
    exists, so different versions of one rowid have to be distinguishable under
    4-bit quantization. Correlated versions would let a retained stale vector
    score as a match. Measured self-match is >= 0.996 down to 8 dimensions,
    against a cross-version maximum of 0.66.
    """
    state = (rowid * 0x9E3779B97F4A7C15 + version * 0xBF58476D1CE4E5B9) | 1
    values = []
    for _ in range(dimensions):
        state ^= (state << 13) & 0xFFFFFFFFFFFFFFFF
        state ^= state >> 7
        state ^= (state << 17) & 0xFFFFFFFFFFFFFFFF
        state &= 0xFFFFFFFFFFFFFFFF
        values.append(((state >> 40) / float(1 << 23)) - 0.5)
    norm = math.sqrt(sum(value * value for value in values))
    return struct.pack(f"<{dimensions}f", *(value / norm for value in values))


def actual_ids(connection: sqlite3.Connection) -> set[int]:
    return {row[0] for row in connection.execute("select rowid from vectors")}


# A stored vector that matches its expected version scores ~1.0; any other
# version of the same rowid scores well under this.
SELF_MATCH_FLOOR = 0.9


def assert_model(
    connection: sqlite3.Connection,
    expected: dict[int, int],
    dimensions: int,
    sample: set[int] = frozenset(),
) -> None:
    actual = actual_ids(connection)
    if actual != expected.keys():
        missing = sorted(expected.keys() - actual)[:10]
        extra = sorted(actual - expected.keys())[:10]
        raise AssertionError(f"model mismatch: missing={missing}, extra={extra}")
    count = connection.execute("select count(*) from vectors").fetchone()[0]
    if count != len(expected):
        raise AssertionError(f"count mismatch: SQLite={count}, model={len(expected)}")
    # Content check on the rowids this transaction actually touched: they are
    # the only ones whose stored vector could have gone stale.
    for rowid in sorted(sample & expected.keys())[:12]:
        version = expected[rowid]
        row = connection.execute(
            "select score from vectors where embedding match ? and rowid=? "
            "order by score desc limit 1",
            (vector(rowid, dimensions, version), rowid),
        ).fetchone()
        if row is None:
            raise AssertionError(f"rowid {rowid} is absent from a KNN scan")
        if row[0] < SELF_MATCH_FLOOR:
            raise AssertionError(
                f"rowid {rowid} scores {row[0]:.3f} against version {version}; "
                "it is holding some other version's vector"
            )


def main() -> None:
    args = arguments()
    operations = 0
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="turbovec-model-") as directory:
        database = Path(directory) / "model.db"
        connection = connect(database, args.extension)
        connection.execute(
            "create virtual table vectors using "
            f"turbovec0(dimensions={args.dimensions}, bit_width=4)"
        )
        # rowid -> the version of the vector that rowid must be holding.
        expected = {rowid: 0 for rowid in range(1, args.initial_rows + 1)}
        if expected:
            connection.executemany(
                "insert into vectors(rowid,embedding) values(?,?)",
                (
                    (rowid, vector(rowid, args.dimensions, version))
                    for rowid, version in sorted(expected.items())
                ),
            )
            connection.commit()

        for seed in range(args.seeds):
            rng = random.Random(0x5EED + seed)
            before = dict(expected)
            savepoint: dict[int, int] | None = None
            touched: set[int] = set()
            connection.execute("begin immediate")
            for step in range(args.steps):
                choice = rng.randrange(100)
                rowid = rng.randrange(1, args.seeds * 4 + 65)
                # Versions must not repeat across steps for one rowid, or a
                # stale vector could pass the content check by coincidence.
                version = step + 1
                if choice < 30:
                    connection.execute(
                        "insert or ignore into vectors(rowid, embedding) values (?, ?)",
                        (rowid, vector(rowid, args.dimensions, version)),
                    )
                    # OR IGNORE leaves an existing row, and its vector, alone.
                    expected.setdefault(rowid, version)
                    touched.add(rowid)
                elif choice < 50:
                    connection.execute("delete from vectors where rowid=?", (rowid,))
                    expected.pop(rowid, None)
                elif choice < 65:
                    connection.execute(
                        "insert or replace into vectors(rowid, embedding) values (?, ?)",
                        (rowid, vector(rowid, args.dimensions, version)),
                    )
                    expected[rowid] = version
                    touched.add(rowid)
                elif choice < 74 and expected:
                    duplicate = rng.choice(sorted(expected))
                    try:
                        connection.execute(
                            "insert or abort into vectors(rowid, embedding) values (?, ?)",
                            (duplicate, vector(duplicate, args.dimensions, version)),
                        )
                    except sqlite3.IntegrityError:
                        pass
                    else:
                        raise AssertionError("duplicate ABORT insert succeeded")
                    # A refused insert must leave the stored vector untouched.
                    touched.add(duplicate)
                elif choice < 84 and savepoint is None:
                    connection.execute("savepoint model_point")
                    savepoint = dict(expected)
                elif choice < 93 and savepoint is not None:
                    connection.execute("rollback to model_point")
                    expected = dict(savepoint)
                elif savepoint is not None:
                    connection.execute("release model_point")
                    savepoint = None
                operations += 1

            if seed % 7 == 0:
                connection.rollback()
                expected = before
            else:
                connection.commit()
            assert_model(connection, expected, args.dimensions, touched)

            if seed % 10 == 9:
                connection.close()
                connection = connect(database, args.extension)
                assert_model(connection, expected, args.dimensions, touched)

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
