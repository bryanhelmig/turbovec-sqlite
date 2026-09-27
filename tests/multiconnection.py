#!/usr/bin/env python3
"""WAL and cross-connection cache invalidation smoke test."""

from __future__ import annotations

import json
import math
import random
import sqlite3
import struct
import sys
from pathlib import Path

DIMENSIONS = 1_536
# Enough 4-bit rows to put the base image over the 1 MiB floor below which
# every commit compacts. Delta batches, and therefore the reader's incremental
# catch-up, only exist above it.
BASE_ROWS = 2_000
# Insert deltas cost 9 + 4 * dimensions bytes each, so this many rows exceeds
# the base image and forces one compaction.
COMPACTION_ROWS = 700


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension))
    connection.enable_load_extension(False)
    return connection


def vector(seed: int) -> bytes:
    """A normalized vector that is near-orthogonal to every other seed's, so a
    stale stored vector scores visibly worse than the right one."""
    generator = random.Random(seed)
    values = [generator.gauss(0.0, 1.0) for _ in range(DIMENSIONS)]
    norm = math.sqrt(sum(value * value for value in values))
    return struct.pack(f"<{DIMENSIONS}f", *(value / norm for value in values))


def info(connection: sqlite3.Connection, table: str) -> dict[str, object]:
    return json.loads(connection.execute(f"select turbovec_info('{table}')").fetchone()[0])


def ids(connection: sqlite3.Connection, table: str) -> set[int]:
    return {row[0] for row in connection.execute(f"select rowid from {table}")}


def top(connection: sqlite3.Connection, table: str, query: bytes, limit: int = 1):
    return connection.execute(
        f"select rowid, score from {table} "
        "where embedding match ? order by score desc limit ?",
        (query, limit),
    ).fetchall()


def check_incremental_catch_up(database: Path, extension: Path) -> None:
    """A warm reader must track committed deltas without rebuilding, and must
    still agree with a cold reader once a compaction rewrites the base."""
    writer = connect(database, extension)
    writer.execute(
        "create virtual table deltas using "
        f"turbovec0(dimensions={DIMENSIONS}, bit_width=4)"
    )
    filler = vector(0)
    with writer:
        for rowid in range(1, BASE_ROWS + 1):
            writer.execute(
                "insert into deltas(rowid, embedding) values (?, ?)", (rowid, filler)
            )
    base = info(writer, "deltas")
    assert base["base_bytes"] >= 1024 * 1024, base
    # A high-dimensional index pays a fixed rotation and codebook cost, so an
    # empty 1,536-dim base already clears the 1 MiB floor. The delta budget must
    # still be capped at the base image, or this bulk load would be stored as
    # raw f32 deltas several times larger than the 4-bit base and replayed on
    # every cold open.
    assert base["delta_bytes"] == 0, base

    # Warm the reader before any of the changes below exist.
    reader = connect(database, extension)
    model = set(range(1, BASE_ROWS + 1))
    assert ids(reader, "deltas") == model

    # An insert the reader has never seen, committed as a delta batch.
    added = vector(BASE_ROWS + 1)
    with writer:
        writer.execute(
            "insert into deltas(rowid, embedding) values (?, ?)",
            (BASE_ROWS + 1, added),
        )
    model.add(BASE_ROWS + 1)
    assert info(reader, "deltas")["delta_bytes"] > 0, "expected a delta batch"
    assert ids(reader, "deltas") == model
    assert top(reader, "deltas", added)[0][0] == BASE_ROWS + 1

    # A replacement must reach the reader as the *new* vector, not the old one.
    replaced = vector(BASE_ROWS + 2)
    with writer:
        writer.execute(
            "insert or replace into deltas(rowid, embedding) values (1, ?)", (replaced,)
        )
    assert top(reader, "deltas", replaced)[0][0] == 1
    matched = reader.execute(
        "select score from deltas where embedding match ? and rowid=1 "
        "order by score desc limit 1",
        (replaced,),
    ).fetchone()
    assert matched is not None and matched[0] > 0.9, matched
    stale = reader.execute(
        "select score from deltas where embedding match ? and rowid=1 "
        "order by score desc limit 1",
        (filler,),
    ).fetchone()
    assert stale is not None and stale[0] < 0.5, stale

    # And a delete must remove it.
    with writer:
        writer.execute("delete from deltas where rowid=2")
    model.discard(2)
    assert ids(reader, "deltas") == model

    # Compaction rewrites the base and drops every delta, so the reader cannot
    # catch up incrementally and must fall back to a full reload.
    with writer:
        for rowid in range(10_000, 10_000 + COMPACTION_ROWS):
            writer.execute(
                "insert into deltas(rowid, embedding) values (?, ?)", (rowid, filler)
            )
            model.add(rowid)
    compacted = info(writer, "deltas")
    assert compacted["delta_bytes"] == 0, compacted
    assert compacted["base_bytes"] > base["base_bytes"], compacted
    assert ids(reader, "deltas") == model
    assert top(reader, "deltas", replaced)[0][0] == 1

    cold = connect(database, extension)
    assert ids(cold, "deltas") == ids(reader, "deltas")
    assert top(cold, "deltas", added) == top(reader, "deltas", added)
    cold.close()
    reader.close()
    writer.close()


def main() -> None:
    database = Path(sys.argv[1])
    extension = Path(sys.argv[2])
    writer = connect(database, extension)
    assert writer.execute("pragma journal_mode=wal").fetchone()[0] == "wal"
    writer.execute(
        "create virtual table vectors using "
        "turbovec0(dimensions=8, bit_width=4)"
    )
    writer.execute(
        "insert into vectors(rowid, embedding) values (1, ?)",
        ("[1,0,0,0,0,0,0,0]",),
    )
    writer.commit()

    reader = connect(database, extension)
    assert reader.execute("select count(*) from vectors").fetchone()[0] == 1

    writer.execute(
        "insert into vectors(rowid, embedding) values (2, ?)",
        ("[0,1,0,0,0,0,0,0]",),
    )
    writer.commit()

    # The reader connected and warmed its cache before row 2 existed. The
    # persisted generation must make it reload after the writer commits.
    assert reader.execute("select count(*) from vectors").fetchone()[0] == 2
    result = reader.execute(
        "select rowid from vectors "
        "where embedding match ? order by score desc limit 1",
        ("[0,1,0,0,0,0,0,0]",),
    ).fetchone()
    assert result == (2,)

    reader.close()
    writer.close()

    check_incremental_catch_up(database, extension)
    print("turbovec0 cross-connection cache invalidation passed")


if __name__ == "__main__":
    main()
