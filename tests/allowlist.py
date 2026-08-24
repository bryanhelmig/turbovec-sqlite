#!/usr/bin/env python3
"""Selective rowid allowlist pushdown and planner contract."""

from __future__ import annotations

import math
import sqlite3
import struct
import sys
import tempfile
from pathlib import Path


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension.resolve()))
    connection.enable_load_extension(False)
    return connection


def vector(rowid: int) -> bytes:
    state = rowid | 1
    values = []
    for _ in range(8):
        state ^= (state << 13) & 0xFFFFFFFFFFFFFFFF
        state ^= state >> 7
        state ^= (state << 17) & 0xFFFFFFFFFFFFFFFF
        state &= 0xFFFFFFFFFFFFFFFF
        values.append(((state >> 40) / float(1 << 23)) - 0.5)
    norm = math.sqrt(sum(value * value for value in values))
    return struct.pack("<8f", *(value / norm for value in values))


def search(
    connection: sqlite3.Connection,
    query: bytes,
    where: str | None,
    parameters: tuple[object, ...] = (),
    tail: str = "limit 10",
) -> list[tuple[int, float]]:
    predicate = f" and {where}" if where else ""
    return connection.execute(
        "select rowid, score from vectors "
        f"where embedding match ?{predicate} "
        f"order by score desc {tail}",
        (query, *parameters),
    ).fetchall()


def main() -> None:
    extension = Path(sys.argv[1])
    with tempfile.TemporaryDirectory(prefix="turbovec-allowlist-") as directory:
        database = Path(directory) / "allowlist.db"
        connection = connect(database, extension)
        connection.execute(
            "create table documents(id integer primary key, path text not null)"
        )
        connection.execute(
            "create virtual table vectors using turbovec0(dimensions=8, bit_width=4)"
        )
        with connection:
            connection.executemany(
                "insert into documents(id, path) values (?, ?)",
                ((rowid, f"notes/{rowid}.txt") for rowid in range(1, 257)),
            )
            connection.executemany(
                "insert into vectors(rowid, embedding) values (?, ?)",
                ((rowid, vector(rowid)) for rowid in range(1, 257)),
            )

        query = vector(10_001)
        full = search(connection, query, None, tail="limit 256")
        allowed = {rowid for rowid, _ in full[180:220]}
        with connection:
            connection.executemany(
                "update documents set path=? where id=?",
                ((f"config/{rowid}.yaml", rowid) for rowid in allowed),
            )
            # A content row without a vector is a normal stale allowlist entry.
            connection.execute(
                "insert into documents(id, path) values (9999, 'config/stale.yaml')"
            )

        predicate = (
            "rowid in (select id from documents where path glob '*.yaml')"
        )
        actual = search(connection, query, predicate)
        expected = full[180:190]
        assert [rowid for rowid, _ in actual] == [rowid for rowid, _ in expected]
        assert all(
            math.isclose(actual_score, expected_score, rel_tol=0.0, abs_tol=1e-6)
            for (_, actual_score), (_, expected_score) in zip(actual, expected)
        )
        # This fixture deliberately defeats a fixed 10x post-filter overfetch.
        assert not allowed.intersection(rowid for rowid, _ in full[:100])

        plan = connection.execute(
            "explain query plan select rowid, score from vectors "
            f"where embedding match ? and {predicate} "
            "order by score desc limit 10",
            (query,),
        ).fetchall()
        assert any("knn+rowid-allowlist" in row[3] for row in plan), plan

        assert search(
            connection, query, "rowid in (select id from documents where 0)"
        ) == []
        static_ids = tuple(rowid for rowid, _ in expected[:2])
        assert [
            row[0]
            for row in search(
                connection,
                query,
                "rowid in (?, ?, null, -1, 99999)",
                static_ids,
            )
        ] == list(static_ids)

        chosen = expected[0][0]
        assert search(connection, query, "rowid=?", (chosen,))[0][0] == chosen
        assert [
            row[0]
            for row in search(
                connection, query, predicate, tail="limit 5 offset 3"
            )
        ] == [rowid for rowid, _ in full[183:188]]
        assert [
            row[0]
            for row in search(connection, query, f"k=10 and {predicate}", tail="")
        ] == [rowid for rowid, _ in expected]

        connection.close()
        reopened = connect(database, extension)
        assert [row[0] for row in search(reopened, query, predicate)] == [
            rowid for rowid, _ in expected
        ]
        reopened.execute("delete from vectors where rowid=?", (expected[0][0],))
        reopened.commit()
        assert [row[0] for row in search(reopened, query, predicate)] == [
            rowid for rowid, _ in full[181:191]
        ]
        reopened.close()

    print("turbovec0 rowid allowlist pushdown passed")


if __name__ == "__main__":
    main()
