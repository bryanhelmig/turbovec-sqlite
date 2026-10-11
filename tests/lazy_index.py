#!/usr/bin/env python3
"""Writes that skip the compressed index, and the upgrade from 0.2.0 storage."""

from __future__ import annotations

import argparse
import sqlite3
import struct
import tempfile
from pathlib import Path


DIMENSIONS = 768
# Enough 4-bit rows for a two-chunk base over the 1 MiB floor below which
# every commit compacts. Only above it can a commit skip the index.
ROWS = 12_000
BASE_VECTOR = struct.pack(f"<{DIMENSIONS}f", 1, *([0] * (DIMENSIONS - 1)))
IDS = (
    "with recursive ids(id) as (values(1) union all "
    "select id+1 from ids where id<?) "
)
LEGACY_META = (
    "create table v_meta(id INTEGER PRIMARY KEY CHECK(id=1), "
    "dimensions INTEGER NOT NULL, bit_width INTEGER NOT NULL, "
    "generation INTEGER NOT NULL, byte_len INTEGER NOT NULL)"
)


def connect(extension: Path | None, database: Path) -> sqlite3.Connection:
    db = sqlite3.connect(database)
    if extension is not None:
        db.enable_load_extension(True)
        db.load_extension(str(extension))
        db.enable_load_extension(False)
    return db


def unit(coordinate: int) -> bytes:
    values = [0.0] * DIMENSIONS
    values[coordinate] = 1.0
    return struct.pack(f"<{DIMENSIONS}f", *values)


def build(extension: Path, database: Path, rows: int = ROWS) -> None:
    db = connect(extension, database)
    db.execute(
        f"create virtual table v using turbovec0(dimensions={DIMENSIONS},bit_width=4)"
    )
    db.execute("begin immediate")
    db.execute(IDS + "insert into v(rowid,embedding) select id,? from ids", (rows, BASE_VECTOR))
    db.commit()
    db.close()


def tables(db: sqlite3.Connection) -> set[str]:
    return {
        row[0]
        for row in db.execute(
            "select name from sqlite_schema where type='table' and name like 'v\\_%' escape '\\'"
        )
    }


def nearest(db: sqlite3.Connection, query: bytes) -> tuple[int, float]:
    return db.execute(
        "select rowid,score from v where embedding match ? and k=1 order by score desc",
        (query,),
    ).fetchone()


def all_indexed(db: sqlite3.Connection) -> set[int]:
    """Every rowid as the compressed index sees it: a KNN over all rows."""
    count = db.execute("select count(*) from v").fetchone()[0]
    return {
        row[0]
        for row in db.execute(
            "select rowid from v where embedding match ? and k=?", (BASE_VECTOR, count + 8)
        )
    }


def stored_ids(db: sqlite3.Connection) -> list[int]:
    return [row[0] for row in db.execute("select id from v_ids order by id")]


def expect_error(db: sqlite3.Connection, sql: str, params: tuple, fragment: str) -> None:
    try:
        db.execute(sql, params).fetchall()
    except sqlite3.DatabaseError as cause:
        assert fragment in str(cause), (fragment, str(cause))
    else:
        raise AssertionError(f"expected an error containing {fragment!r}: {sql}")


def lazy_commit(extension: Path) -> None:
    """A delete-and-insert transaction on a fresh connection reads no base chunk.

    The base image's format header is damaged before the transaction. Loading
    the base checks that header first, so any write that loaded it fails.
    """
    with tempfile.TemporaryDirectory(prefix="turbovec-lazy-") as directory:
        database = Path(directory) / "lazy.db"
        build(extension, database)
        plain = connect(None, database)
        chunk_count = plain.execute(
            "select count(*) from v_chunks where chunk_id>=0"
        ).fetchone()[0]
        assert chunk_count >= 2, chunk_count
        assert plain.execute("select count(*) from v_chunks where chunk_id<0").fetchone()[0] == 0
        header = plain.execute(
            "select substr(data,1,8) from v_chunks where chunk_id=0"
        ).fetchone()[0]
        plain.execute(
            "update v_chunks set data=cast(x'00000000' || substr(data,5) as blob) where chunk_id=0"
        )
        plain.commit()

        db = connect(extension, database)
        # Rowid reads come from _ids.
        assert db.execute("select count(*) from v").fetchone() == (ROWS,)
        assert db.execute("select rowid from v where rowid=7").fetchall() == [(7,)]
        assert db.execute("select rowid from v where rowid=?", (ROWS + 1,)).fetchall() == []
        db.execute("begin immediate")
        db.execute("delete from v where rowid=7")
        db.execute("insert into v(rowid,embedding) values(7,?)", (unit(5),))
        db.execute("insert or replace into v(rowid,embedding) values(8,?)", (unit(6),))
        db.execute("insert into v(rowid,embedding) values(?,?)", (ROWS + 1, unit(7)))
        db.execute("delete from v where rowid=9")
        # The SQL contract holds without the index. SQLite finds no row to
        # pass to xUpdate, as with 0.2.0.
        db.execute("delete from v where rowid=9")
        assert db.execute("select changes()").fetchone() == (0,)
        try:
            db.execute("insert into v(rowid,embedding) values(8,?)", (unit(9),))
        except sqlite3.IntegrityError as cause:
            assert cause.sqlite_errorcode == sqlite3.SQLITE_CONSTRAINT_PRIMARYKEY, cause
        else:
            raise AssertionError("duplicate insert succeeded without the index")
        db.execute("insert or ignore into v(rowid,embedding) values(8,?)", (unit(9),))
        assert db.execute("select changes()").fetchone() == (0,)
        assert db.execute("select count(*) from v").fetchone() == (ROWS,)
        db.commit()

        # The base is still damaged, so nothing rewrote it: the commit stored
        # one delta batch.
        assert plain.execute(
            "select substr(data,1,4) from v_chunks where chunk_id=0"
        ).fetchone() == (b"\0\0\0\0",)
        assert plain.execute("select count(*) from v_chunks where chunk_id<0").fetchone() == (1,)
        assert plain.execute(
            "select row_count, delta_operations from v_info"
        ).fetchone() == (ROWS, 5)
        assert db.execute("select count(*) from v").fetchone() == (ROWS,)
        assert db.execute(
            "select rowid from v where rowid in (7,8,9,?) order by rowid", (ROWS + 1,)
        ).fetchall() == [(7,), (8,), (ROWS + 1,)]
        # A search does load the base, and refuses the damaged header.
        expect_error(
            db,
            "select rowid from v where embedding match ? and k=1",
            (unit(5),),
            "unsupported TurboVec index format",
        )
        db.close()

        plain.execute(
            "update v_chunks set data=cast(? || substr(data,9) as blob) where chunk_id=0",
            (header,),
        )
        plain.commit()
        plain.close()
        db = connect(extension, database)
        assert nearest(db, unit(5))[0] == 7
        assert nearest(db, unit(6))[0] == 8
        assert nearest(db, unit(7))[0] == ROWS + 1
        indexed = all_indexed(db)
        assert 9 not in indexed and ROWS + 1 in indexed and len(indexed) == ROWS
        assert stored_ids(db) == sorted(indexed)
        assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
        db.close()


def search_inside_transaction(extension: Path) -> None:
    """A search after unloaded writes loads the index and replays them."""
    with tempfile.TemporaryDirectory(prefix="turbovec-lazy-search-") as directory:
        database = Path(directory) / "search.db"
        build(extension, database)
        db = connect(extension, database)
        db.execute("begin immediate")
        db.execute("delete from v where rowid=10")
        db.execute("insert into v(rowid,embedding) values(?,?)", (ROWS + 2, unit(8)))
        db.execute("insert or replace into v(rowid,embedding) values(11,?)", (unit(10),))
        assert nearest(db, unit(8))[0] == ROWS + 2
        assert nearest(db, unit(10))[0] == 11
        assert db.execute(
            "select rowid from v where embedding match ? and rowid in (10, 11) and k=5",
            (BASE_VECTOR,),
        ).fetchall() == [(11,)]
        # Destructive savepoint rollback once the index is loaded.
        db.execute("savepoint loaded")
        db.execute("delete from v where rowid=?", (ROWS + 2,))
        db.execute("insert into v(rowid,embedding) values(?,?)", (ROWS + 3, unit(9)))
        db.execute("rollback to loaded")
        db.execute("release loaded")
        assert nearest(db, unit(8))[0] == ROWS + 2
        assert nearest(db, unit(9))[0] != ROWS + 3
        db.commit()
        db.close()

        # Savepoint rollback, then a search, while nothing is loaded yet.
        db = connect(extension, database)
        db.execute("begin immediate")
        db.execute("savepoint unloaded")
        db.execute("delete from v where rowid=12")
        db.execute("insert into v(rowid,embedding) values(?,?)", (ROWS + 4, unit(11)))
        db.execute("rollback to unloaded")
        db.execute("insert into v(rowid,embedding) values(?,?)", (ROWS + 5, unit(12)))
        assert db.execute(
            "select rowid from v where rowid in (12, ?, ?) order by rowid", (ROWS + 4, ROWS + 5)
        ).fetchall() == [(12,), (ROWS + 5,)]
        assert nearest(db, unit(12))[0] == ROWS + 5
        assert nearest(db, unit(11))[0] != ROWS + 4
        db.commit()
        # A full rollback of unloaded writes leaves storage and _ids alone.
        db.execute("begin immediate")
        db.execute("delete from v where rowid=13")
        db.execute("insert into v(rowid,embedding) values(?,?)", (ROWS + 6, unit(13)))
        db.rollback()
        expected = (set(range(1, ROWS + 1)) - {10}) | {ROWS + 2, ROWS + 5}
        assert set(stored_ids(db)) == expected
        db.close()
        db = connect(extension, database)
        assert all_indexed(db) == expected
        assert nearest(db, unit(8))[0] == ROWS + 2
        assert nearest(db, unit(12))[0] == ROWS + 5
        assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
        db.close()


def downgrade(database: Path) -> None:
    """Rewrite current storage as 0.2.0 wrote it: the same chunks, metadata in
    `_meta`, and no `_info` or `_ids`."""
    plain = connect(None, database)
    row = plain.execute(
        "select dimensions, bit_width, generation, byte_len from v_info"
    ).fetchone()
    plain.execute(LEGACY_META)
    plain.execute("insert into v_meta values(1,?,?,?,?)", row)
    plain.execute("drop table v_info")
    plain.execute("drop table v_ids")
    plain.commit()
    plain.close()


def upgrade(extension: Path, legacy_extension: Path | None) -> None:
    with tempfile.TemporaryDirectory(prefix="turbovec-upgrade-") as directory:
        database = Path(directory) / "upgrade.db"
        if legacy_extension is None:
            build(extension, database, rows=3_000)
            db = connect(extension, database)
            db.execute("insert into v(rowid,embedding) values(4000,?)", (unit(1),))
            db.commit()
            db.close()
            downgrade(database)
        else:
            build(legacy_extension, database, rows=3_000)
            db = connect(legacy_extension, database)
            db.execute("insert into v(rowid,embedding) values(4000,?)", (unit(1),))
            db.commit()
            db.close()
        expected = set(range(1, 3_001)) | {4000}
        plain = connect(None, database)
        assert tables(plain) == {"v_meta", "v_chunks"}, tables(plain)
        assert plain.execute("select count(*) from v_chunks where chunk_id<0").fetchone() == (1,)

        warm_old = None
        if legacy_extension is not None:
            warm_old = connect(legacy_extension, database)
            assert warm_old.execute("select count(*) from v").fetchone() == (len(expected),)
        # A connection that loaded the legacy index before the upgrade.
        warm_new = connect(extension, database)
        assert warm_new.execute("select count(*) from v").fetchone() == (len(expected),)
        assert nearest(warm_new, unit(1))[0] == 4000

        # Reading does not upgrade.
        reader = connect(extension, database)
        assert all_indexed(reader) == expected
        reader.close()
        assert tables(plain) == {"v_meta", "v_chunks"}

        # An upgrade inside a commit that fails is rolled back with it.
        plain.execute(
            "create trigger fail_delta before insert on v_chunks when new.chunk_id<0 "
            "begin select raise(abort,'forced delta failure'); end"
        )
        plain.commit()
        writer = connect(extension, database)
        writer.execute("insert into v(rowid,embedding) values(4001,?)", (unit(2),))
        try:
            writer.commit()
        except sqlite3.DatabaseError as cause:
            assert "forced delta failure" in str(cause), cause
        else:
            raise AssertionError("injected delta failure did not fire")
        writer.rollback()
        assert tables(plain) == {"v_meta", "v_chunks"}
        assert writer.execute("select count(*) from v").fetchone() == (len(expected),)
        plain.execute("drop trigger fail_delta")
        plain.commit()

        # The first successful write upgrades.
        writer.execute("delete from v where rowid=5")
        writer.execute("insert into v(rowid,embedding) values(4001,?)", (unit(2),))
        writer.commit()
        expected = (expected - {5}) | {4001}
        assert tables(plain) == {"v_meta", "v_info", "v_ids", "v_chunks"}, tables(plain)
        assert plain.execute("select count(*) from v_meta").fetchone() == (0,)
        info = plain.execute("select format, row_count from v_info").fetchone()
        assert info == (1, len(expected)), info
        assert set(stored_ids(plain)) == expected
        assert writer.execute("pragma integrity_check").fetchall() == [("ok",)]
        writer.close()

        # A new-version connection opened before the upgrade keeps working.
        assert warm_new.execute("select count(*) from v").fetchone() == (len(expected),)
        assert nearest(warm_new, unit(2))[0] == 4001
        warm_new.execute("insert into v(rowid,embedding) values(4002,?)", (unit(3),))
        warm_new.commit()
        expected.add(4002)
        assert set(stored_ids(plain)) == expected
        warm_new.close()

        # 0.2.0 refuses the upgraded index, whether it connects now or was
        # already connected: every 0.2.0 path reads the _meta row first.
        if legacy_extension is not None and warm_old is not None:
            expect_error(warm_old, "select count(*) from v", (), "Query returned no rows")
            expect_error(
                warm_old,
                "insert into v(rowid,embedding) values(5000,?)",
                (unit(4),),
                "Query returned no rows",
            )
            warm_old.rollback()
            warm_old.close()
            fresh_old = connect(legacy_extension, database)
            expect_error(fresh_old, "select count(*) from v", (), "Query returned no rows")
            fresh_old.close()
        assert set(stored_ids(plain)) == expected
        plain.close()

        db = connect(extension, database)
        assert all_indexed(db) == expected
        assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
        db.close()


def format_checks(extension: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="turbovec-format-") as directory:
        database = Path(directory) / "format.db"
        db = connect(extension, database)
        db.execute("create virtual table v using turbovec0(dimensions=8,bit_width=4)")
        db.execute("insert into v(rowid,embedding) values(1,'[1,0,0,0,0,0,0,0]')")
        db.commit()
        assert tables(db) == {"v_info", "v_ids", "v_chunks"}, tables(db)
        db.close()

        plain = connect(None, database)
        plain.execute("update v_info set format=2")
        plain.commit()
        db = connect(extension, database)
        expect_error(db, "select count(*) from v", (), "unsupported turbovec0 storage format 2")
        db.close()
        plain.execute("update v_info set format=1")
        plain.execute("drop table v_ids")
        plain.commit()
        db = connect(extension, database)
        expect_error(db, "select count(*) from v", (), "only one of its _info and _ids")
        db.close()
        plain.close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    parser.add_argument(
        "--legacy-extension",
        type=Path,
        help="a 0.2.0 build: write the legacy index with it and check that it refuses the upgrade",
    )
    args = parser.parse_args()
    extension = args.extension.resolve()
    legacy = args.legacy_extension.resolve() if args.legacy_extension else None
    lazy_commit(extension)
    print("turbovec0 unloaded commit passed")
    search_inside_transaction(extension)
    print("turbovec0 search after unloaded writes passed")
    upgrade(extension, legacy)
    print(
        "turbovec0 upgrade from 0.2.0 storage passed"
        + (" (with the 0.2.0 build)" if legacy else " (simulated 0.2.0 layout)")
    )
    format_checks(extension)
    print("turbovec0 storage format checks passed")


if __name__ == "__main__":
    main()
