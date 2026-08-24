#!/usr/bin/env python3
"""Fail when TurboVec recall regresses on a fixed real-embedding fixture."""

from __future__ import annotations

import gzip
import heapq
import json
import sqlite3
import struct
import sys
from pathlib import Path


FIXTURE = Path(__file__).parent / "fixtures/glove-2024-50d-4224.json.gz"
FLOORS = {10: 0.89, 40: 0.91}


def blob(vector: list[float]) -> bytes:
    return struct.pack(f"<{len(vector)}f", *vector)


def exact_ids(
    database: list[list[float]], query: list[float], count: int
) -> set[int]:
    scored = (
        (sum(left * right for left, right in zip(vector, query)), rowid)
        for rowid, vector in enumerate(database, start=1)
    )
    return {rowid for _, rowid in heapq.nlargest(count, scored)}


def main() -> None:
    extension = Path(sys.argv[1]).resolve()
    with gzip.open(FIXTURE, "rt", encoding="utf-8") as source:
        fixture = json.load(source)
    dimensions = fixture["dimensions"]
    database = fixture["database_vectors"]
    queries = fixture["query_vectors"]

    connection = sqlite3.connect(":memory:")
    connection.enable_load_extension(True)
    connection.load_extension(str(extension))
    connection.enable_load_extension(False)
    connection.execute(
        f"create virtual table vectors using turbovec0(dimensions={dimensions}, bit_width=4)"
    )
    with connection:
        connection.executemany(
            "insert into vectors(rowid, embedding) values (?, ?)",
            ((rowid, blob(vector)) for rowid, vector in enumerate(database, start=1)),
        )

    hits = {count: 0 for count in FLOORS}
    for query in queries:
        query_blob = blob(query)
        for count in FLOORS:
            expected = exact_ids(database, query, count)
            actual = {
                rowid
                for (rowid,) in connection.execute(
                    "select rowid from vectors where embedding match ? "
                    "order by score desc limit ?",
                    (query_blob, count),
                )
            }
            hits[count] += len(expected & actual)

    recalls = {
        count: hits[count] / (len(queries) * count) for count in FLOORS
    }
    summary = " ".join(
        f"recall@{count}={recalls[count]:.3f} (floor {floor:.2f})"
        for count, floor in FLOORS.items()
    )
    print(f"real-embedding recall passed: {summary}")
    for count, floor in FLOORS.items():
        assert recalls[count] >= floor, summary


if __name__ == "__main__":
    main()
