#!/usr/bin/env python3
"""The _ids and _info shadow tables, and the upgrade from 0.2.0 storage."""

from __future__ import annotations

import argparse
import sqlite3
import struct
import tempfile
from pathlib import Path


DIMENSIONS = 768
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
    upgrade(extension, legacy)
    print(
        "turbovec0 upgrade from 0.2.0 storage passed"
        + (" (with the 0.2.0 build)" if legacy else " (simulated 0.2.0 layout)")
    )
    format_checks(extension)
    print("turbovec0 storage format checks passed")


if __name__ == "__main__":
    main()
