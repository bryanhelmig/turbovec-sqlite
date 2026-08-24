#!/usr/bin/env python3
"""SQLite conflict, statement-rollback, defensive, and integrity contracts."""

from __future__ import annotations

import sqlite3
import sys
from pathlib import Path


VECTOR_X = "[1,0,0,0,0,0,0,0]"
VECTOR_Y = "[0,1,0,0,0,0,0,0]"
VECTOR_NEG_X = "[-1,0,0,0,0,0,0,0]"


def ids(connection: sqlite3.Connection) -> list[int]:
    return [row[0] for row in connection.execute("select rowid from v order by rowid")]


def main() -> None:
    extension = Path(sys.argv[1])
    connection = sqlite3.connect(":memory:")
    connection.enable_load_extension(True)
    connection.load_extension(str(extension))
    connection.enable_load_extension(False)
    connection.execute(
        "create virtual table v using turbovec0(dimensions=8, bit_width=4)"
    )
    connection.execute(
        "insert into v(rowid, embedding) values (1, ?)", (VECTOR_X,)
    )
    connection.commit()

    # ABORT rolls back earlier xUpdate calls in the same statement.
    try:
        connection.execute(
            "insert or abort into v(rowid, embedding) values "
            "(2, ?), (1, ?)",
            (VECTOR_Y, VECTOR_Y),
        )
    except sqlite3.IntegrityError as cause:
        assert cause.sqlite_errorcode == sqlite3.SQLITE_CONSTRAINT_PRIMARYKEY
    else:
        raise AssertionError("duplicate ABORT insert unexpectedly succeeded")
    actual_ids = ids(connection)
    assert actual_ids == [1], actual_ids

    # FAIL deliberately preserves rows completed before the constraint.
    try:
        connection.execute(
            "insert or fail into v(rowid, embedding) values "
            "(3, ?), (1, ?)",
            (VECTOR_Y, VECTOR_Y),
        )
    except sqlite3.IntegrityError:
        pass
    else:
        raise AssertionError("duplicate FAIL insert unexpectedly succeeded")
    actual_ids = ids(connection)
    assert actual_ids == [1, 3], actual_ids
    connection.commit()

    # ROLLBACK unwinds the whole explicit transaction, including prior SQL.
    connection.execute("begin immediate")
    connection.execute(
        "insert into v(rowid, embedding) values (4, ?)", (VECTOR_X,)
    )
    try:
        connection.execute(
            "insert or rollback into v(rowid, embedding) values "
            "(5, ?), (1, ?)",
            (VECTOR_Y, VECTOR_Y),
        )
    except sqlite3.IntegrityError:
        pass
    else:
        raise AssertionError("duplicate ROLLBACK insert unexpectedly succeeded")
    assert ids(connection) == [1, 3]

    # Generic input errors must also roll back earlier rows in the statement.
    try:
        connection.execute(
            "insert into v(rowid, embedding) values (6, ?), (7, ?)",
            (VECTOR_X, "[1,2]"),
        )
    except sqlite3.DatabaseError:
        pass
    else:
        raise AssertionError("invalid multi-row insert unexpectedly succeeded")
    assert ids(connection) == [1, 3]
    connection.rollback()

    # A failed statement restores a replacement, including its old score.
    try:
        connection.execute(
            "insert or replace into v(rowid, embedding) values (1, ?), (20, ?)",
            (VECTOR_Y, "[1,2]"),
        )
    except sqlite3.DatabaseError:
        pass
    else:
        raise AssertionError("invalid replacement statement unexpectedly succeeded")
    assert ids(connection) == [1, 3]
    best_x = connection.execute(
        "select rowid from v where embedding match ? order by score desc limit 1",
        (VECTOR_X,),
    ).fetchone()
    assert best_x == (1,)
    connection.rollback()

    # Nested rollback restores interleaved inserts, deletes, and replacements.
    # Deleting row 1 also exercises TurboVec's swap-and-pop slot movement.
    connection.execute("begin immediate")
    connection.execute(
        "insert into v(rowid, embedding) values (10, ?)", (VECTOR_X,)
    )
    connection.execute("savepoint outer_point")
    connection.execute("delete from v where rowid=1")
    connection.execute(
        "insert into v(rowid, embedding) values (11, ?)", (VECTOR_NEG_X,)
    )
    connection.execute("savepoint inner_point")
    connection.execute(
        "insert or replace into v(rowid, embedding) values (3, ?)", (VECTOR_X,)
    )
    connection.execute("delete from v where rowid=10")
    connection.execute(
        "insert into v(rowid, embedding) values (12, ?)", (VECTOR_X,)
    )
    assert ids(connection) == [3, 11, 12]
    connection.execute("rollback to inner_point")
    assert ids(connection) == [3, 10, 11]
    best_y = connection.execute(
        "select rowid from v where embedding match ? order by score desc limit 1",
        (VECTOR_Y,),
    ).fetchone()
    assert best_y == (3,)
    connection.execute("release inner_point")
    connection.execute("delete from v where rowid=11")
    connection.execute("rollback to outer_point")
    assert ids(connection) == [1, 3, 10]
    connection.execute("release outer_point")
    connection.rollback()
    assert ids(connection) == [1, 3]

    # Rolling back before the lazy destructive checkpoint permits another
    # destructive change and rollback in the same outer transaction.
    connection.execute("begin immediate")
    connection.execute("savepoint early_point")
    connection.execute(
        "insert into v(rowid, embedding) values (30, ?)", (VECTOR_X,)
    )
    connection.execute("delete from v where rowid=1")
    connection.execute("rollback to early_point")
    assert ids(connection) == [1, 3]
    connection.execute("delete from v where rowid=3")
    assert ids(connection) == [1]
    connection.execute("rollback to early_point")
    assert ids(connection) == [1, 3]
    connection.execute("release early_point")
    connection.rollback()

    # Ordinary LIMIT is only valid for nearest-first ordering. Accepting an
    # ascending or unordered LIMIT would rank only a truncated candidate set.
    invalid_queries = (
        (
            "select rowid from v where embedding match ? limit 1",
            "unmodified score column DESC",
        ),
        (
            "select rowid from v where embedding match ? order by score asc limit 1",
            "unmodified score column DESC",
        ),
        (
            "select count(*) from v where embedding match ?",
            "single-table scan",
        ),
        (
            "select rowid, round(score, 2) as score from v "
            "where embedding match ? order by score desc limit 1",
            "single-table scan",
        ),
        (
            "with q(embedding) as (values (?)) select v.rowid from v, q "
            "where v.embedding match q.embedding order by v.score desc limit 1",
            "scalar subquery",
        ),
    )
    for invalid_query, expected_message in invalid_queries:
        try:
            connection.execute(invalid_query, (VECTOR_X,)).fetchall()
        except sqlite3.DatabaseError as cause:
            assert expected_message in str(cause), cause
        else:
            raise AssertionError(f"invalid KNN ordering was accepted: {invalid_query}")

    scalar_query = connection.execute(
        "select rowid from v where embedding match (select ?) "
        "order by score desc limit 1",
        (VECTOR_X,),
    ).fetchone()
    assert scalar_query == (1,)

    # xShadowName lets defensive mode reject direct writes while xSync may
    # still maintain the tables through the virtual-table implementation.
    if hasattr(connection, "setconfig") and hasattr(
        sqlite3, "SQLITE_DBCONFIG_DEFENSIVE"
    ):
        connection.setconfig(sqlite3.SQLITE_DBCONFIG_DEFENSIVE, True)
        try:
            connection.execute("update v_meta set generation=99")
        except sqlite3.DatabaseError:
            pass
        else:
            raise AssertionError("defensive mode allowed a shadow-table write")
        connection.rollback()
        connection.execute(
            "insert into v(rowid, embedding) values (8, ?)", (VECTOR_X,)
        )
        connection.commit()
        assert ids(connection) == [1, 3, 8]
        connection.setconfig(sqlite3.SQLITE_DBCONFIG_DEFENSIVE, False)

    # Corrupt a byte after the normal checks, then prove xIntegrity reports it.
    connection.execute(
        "update v_chunks set data=substr(data, 1, length(data)-1) "
        "where chunk_id=(select max(chunk_id) from v_chunks)"
    )
    connection.commit()
    reports = [row[0] for row in connection.execute("pragma integrity_check")]
    assert any("metadata declares" in report for report in reports), reports
    connection.close()


if __name__ == "__main__":
    main()
