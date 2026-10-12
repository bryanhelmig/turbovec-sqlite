#!/usr/bin/env python3
"""A transaction opened by SAVEPOINT must support rollback and later commit."""

import shutil
import sqlite3
import struct
import sys
import tempfile
from pathlib import Path


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    db = sqlite3.connect(database, isolation_level=None)
    db.enable_load_extension(True)
    db.load_extension(str(extension.resolve()))
    db.enable_load_extension(False)
    return db


def scores(db: sqlite3.Connection, query: bytes, count: int) -> dict[int, float]:
    return dict(db.execute(
        "select rowid,score from v where embedding match ? and k=?", (query, count + 8)
    ))


def check(extension: Path, directory: Path, dimensions: int, count: int) -> None:
    x = struct.pack(f"<{dimensions}f", 1, *([0] * (dimensions - 1)))
    y = struct.pack(f"<{dimensions}f", 0, 1, *([0] * (dimensions - 2)))
    base = directory / f"base-{dimensions}.db"
    db = connect(base, extension)
    db.execute(f"create virtual table v using turbovec0(dimensions={dimensions},bit_width=4)")
    db.execute("begin")
    db.executemany("insert into v(rowid,embedding) values(?,?)",
                   ((i, x if i != 2 else y) for i in range(1, count + 1)))
    db.execute("commit")
    expected = scores(db, x, count)
    db.close()
    for warm in (False, True):
        database = directory / f"case-{dimensions}-{warm}.db"
        shutil.copyfile(base, database)
        db = connect(database, extension)
        if warm:
            assert scores(db, x, count) == expected
        # No preceding BEGIN: SQLite uses xRollbackTo(-1) for this savepoint.
        db.execute("savepoint outer_point")
        db.execute("insert or replace into v(rowid,embedding) values(1,?)", (y,))
        db.execute("delete from v where rowid=2")
        db.execute("savepoint nested_point")
        db.execute("insert into v(rowid,embedding) values(?,?)", (count + 1, y))
        db.execute("rollback to outer_point")
        assert db.in_transaction
        assert scores(db, x, count) == expected
        assert [row[0] for row in db.execute("select id from v_ids order by id")] == sorted(expected)
        try:
            db.execute("release nested_point")
        except sqlite3.OperationalError as exc:
            assert "no such savepoint" in str(exc)
        else:
            raise AssertionError("rollback retained a nested savepoint")
        # The outer savepoint survives; repeated rollback must work too.
        db.execute("delete from v where rowid=1")
        db.execute("rollback to outer_point")
        assert scores(db, x, count) == expected
        db.execute("savepoint new_nested")
        db.execute("insert into v(rowid,embedding) values(?,?)", (count + 2, y))
        db.execute("rollback to new_nested")
        db.execute("release new_nested")
        db.execute("insert into v(rowid,embedding) values(?,?)", (count + 3, x))
        db.execute("release outer_point")
        assert not db.in_transaction
        assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
        committed = scores(db, x, count)
        assert committed.keys() == expected.keys() | {count + 3}
        assert all(committed[rowid] == score for rowid, score in expected.items())
        db.close()
        db = connect(database, extension)
        assert scores(db, x, count) == committed
        db.close()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="turbovec-savepoints-") as directory:
        for dimensions, count in ((8, 3), (512, 5000)):
            check(Path(sys.argv[1]), Path(directory), dimensions, count)
    print("top-level savepoint rollback, repeat, nested and commit checks passed")
