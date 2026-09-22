#!/usr/bin/env python3
"""Storage corruption, SQL bookkeeping, and streamed commit regressions."""

from __future__ import annotations

import argparse
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
from pathlib import Path


CHUNK_SIZE = 4 * 1024 * 1024
DIMENSIONS = 768
ROWS = 20_000
VECTOR = struct.pack(f"<{DIMENSIONS}f", 1, *([0] * (DIMENSIONS - 1)))
OTHER = struct.pack(f"<{DIMENSIONS}f", 0, 1, *([0] * (DIMENSIONS - 2)))
IDS = (
    "with recursive ids(id) as (values(1) union all "
    "select id+1 from ids where id<?) "
)


def connect(extension: Path, database: str | Path = ":memory:") -> sqlite3.Connection:
    db = sqlite3.connect(database)
    db.enable_load_extension(True)
    db.load_extension(str(extension))
    db.enable_load_extension(False)
    return db


def create(db: sqlite3.Connection, dimensions: int = DIMENSIONS) -> None:
    db.execute(
        f"create virtual table v using turbovec0(dimensions={dimensions},bit_width=4)"
    )


def insert_many(db: sqlite3.Connection) -> None:
    # Python's legacy transaction detection does not begin a transaction for
    # an INSERT prefixed with WITH. Make the intended mode explicit.
    if db.isolation_level is not None and not db.in_transaction:
        db.execute("begin immediate")
    db.execute(IDS + "insert into v(rowid,embedding) select id,? from ids", (ROWS, VECTOR))


def payload(db: sqlite3.Connection) -> bytes:
    chunks = db.execute(
        "select chunk_id,data from v_chunks where chunk_id>=0 order by chunk_id"
    ).fetchall()
    assert [row[0] for row in chunks] == list(range(len(chunks)))
    assert all(len(row[1]) == CHUNK_SIZE for row in chunks[:-1])
    assert 0 < len(chunks[-1][1]) <= CHUNK_SIZE
    data = b"".join(row[1] for row in chunks)
    stored_len = db.execute("select byte_len from v_meta").fetchone()[0]
    expected_len = -stored_len - 1 if stored_len < 0 else stored_len
    assert len(data) == expected_len
    return data


def last_rowid(db: sqlite3.Connection) -> int:
    return db.execute("select last_insert_rowid()").fetchone()[0]


def ignore_regression(extension: Path) -> None:
    outcomes = []
    for table in ("ordinary", "v"):
        db = connect(extension)
        create(db, 8)
        db.execute("create table ordinary(embedding blob)")
        vector = "[1,0,0,0,0,0,0,0]"
        db.execute(f"insert into {table}(rowid,embedding) values(1,?)", (vector,))
        db.execute(f"insert into {table}(rowid,embedding) values(9,?)", (vector,))
        db.commit()
        initial_last = last_rowid(db)
        # SQLite itself may emit RETURNING rows for ignored virtual inserts
        # (also reproducible with its built-in RTree). Check the extension's
        # affected-row and rowid contract, not that host-generated output.
        db.execute(
            f"insert or ignore into {table}(rowid,embedding) values(1,?) returning rowid",
            (vector,),
        ).fetchall()
        single = (db.execute("select changes()").fetchone()[0], last_rowid(db))
        db.execute(
            f"insert or ignore into {table}(rowid,embedding) "
            "values(1,?),(2,?),(9,?),(3,?) returning rowid",
            (vector,) * 4,
        ).fetchall()
        mixed = (db.execute("select changes()").fetchone()[0], last_rowid(db))
        db.commit()
        outcomes.append((initial_last, single, mixed))
        # Filtering before xUpdate is a portable RETURNING workaround.
        for rowid, expected in ((1, []), (4, [(4,)])):
            actual = db.execute(
                f"insert into {table}(rowid,embedding) select ?,? "
                f"where not exists(select 1 from {table} where rowid=?) returning rowid",
                (rowid, vector, rowid),
            ).fetchall()
            assert actual == expected, actual
        db.commit()
        db.close()
    assert outcomes[0] == outcomes[1], outcomes
    assert outcomes[1] == (9, (0, 9), (2, 3)), outcomes


def corruption_worker(extension: Path, database: Path, query: str) -> None:
    if sys.platform != "win32":
        import resource

        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    db = connect(extension, database)
    try:
        db.execute(query).fetchall()
    except sqlite3.DatabaseError as cause:
        assert "turbovec" in str(cause).lower(), cause
    else:
        raise AssertionError("corrupt storage unexpectedly loaded")
    finally:
        db.close()


def corruption_regression(extension: Path) -> None:
    cases = {
        "huge": "update v_meta set byte_len=9223372036854775807",
        "negative": "update v_meta set byte_len=-1",
        "wrong_length": "update v_meta set byte_len=byte_len+1",
        "missing_chunk": "delete from v_chunks",
        "wrong_id": "update v_chunks set chunk_id=3",
        "empty_chunk": "update v_chunks set data=x''",
        "text_chunk": "update v_chunks set data='invalid'",
        "oversized_chunk": f"update v_chunks set data=zeroblob({CHUNK_SIZE + 1})",
        "interior_partial": "insert into v_chunks select 1,data from v_chunks where chunk_id=0",
    }
    with tempfile.TemporaryDirectory(prefix="turbovec-corruption-") as directory:
        root = Path(directory)
        base = root / "base.db"
        db = connect(extension, base)
        create(db, 8)
        db.close()
        for name, mutation in cases.items():
            database = root / f"{name}.db"
            shutil.copyfile(base, database)
            with sqlite3.connect(database) as db:
                db.execute(mutation)
            db.close()
            for query in ("select count(*) from v", "select turbovec_info('v')"):
                child = subprocess.run(
                    [sys.executable, str(Path(__file__).resolve()), str(extension),
                     "--corrupt-worker", str(database), query],
                    capture_output=True, text=True, timeout=30,
                )
                assert child.returncode == 0, (name, query, child.returncode, child.stderr)

    # Delta records are independently framed and checksummed. Losing the
    # entire log must also fail closed instead of exposing the stale base.
    with tempfile.TemporaryDirectory(prefix="turbovec-delta-corruption-") as directory:
        root = Path(directory)
        base = root / "base.db"
        db = connect(extension, base)
        create(db)
        insert_many(db)
        db.commit()
        db.execute(
            "insert into v(rowid,embedding) values(?,?)", (ROWS + 1, OTHER)
        )
        db.commit()
        assert db.execute(
            "select count(*) from v_chunks where chunk_id<0"
        ).fetchone() == (1,)
        db.execute("pragma wal_checkpoint(truncate)")
        db.close()
        delta_cases = {
            "missing_delta": "delete from v_chunks where chunk_id<0",
            "bad_checksum": (
                "update v_chunks set data=cast(substr(data,1,length(data)-4)||x'00000000' as blob) "
                "where chunk_id<0"
            ),
            "wrong_delta_id": "update v_chunks set chunk_id=chunk_id-7 where chunk_id<0",
        }
        for name, mutation in delta_cases.items():
            database = root / f"{name}.db"
            shutil.copyfile(base, database)
            with sqlite3.connect(database) as db:
                db.execute(mutation)
            child = subprocess.run(
                [
                    sys.executable,
                    str(Path(__file__).resolve()),
                    str(extension),
                    "--corrupt-worker",
                    str(database),
                    "select count(*) from v",
                ],
                capture_output=True,
                text=True,
                timeout=30,
            )
            assert child.returncode == 0, (name, child.returncode, child.stderr)


def streaming_regression(extension: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="turbovec-storage-") as directory:
        database = Path(directory) / "storage.db"
        db = connect(extension, database)
        db.execute("pragma journal_mode=wal")
        create(db)
        empty = db.execute("select turbovec_new(?,4)", (DIMENSIONS,)).fetchone()[0]
        assert payload(db) == empty
        insert_many(db)
        assert last_rowid(db) == ROWS
        assert db.execute("select changes()").fetchone()[0] == ROWS
        db.commit()
        assert last_rowid(db) == ROWS
        # Read changes() immediately after DML. Explicit COMMIT can expose
        # counts from shadow writes (SQLite's built-in FTS5 does this too).
        assert db.execute("select count(*) from v_chunks").fetchone()[0] >= 3
        expected = db.execute(
            IDS + "select turbovec_build(?,4,id,?) from ids", (ROWS, DIMENSIONS, VECTOR)
        ).fetchone()[0]
        assert payload(db) == expected

        reader = connect(extension, database)
        assert reader.execute("select count(*) from v").fetchone()[0] == ROWS
        reader.execute("begin")
        original = reader.execute(
            "select score from v where embedding match ? and rowid=1 order by score desc limit 1",
            (VECTOR,),
        ).fetchone()
        db.execute("insert or replace into v(rowid,embedding) values(1,?)", (OTHER,))
        db.commit()
        assert last_rowid(db) == 1
        expected = db.execute(
            "select turbovec_add(turbovec_remove(?,1),1,?)", (expected, OTHER)
        ).fetchone()[0]
        # A small replacement is an O(changes) delta, not a rewritten base.
        info = db.execute("select turbovec_info('v')").fetchone()[0]
        assert '"delta_operations":1' in info, info
        assert payload(db) != expected
        assert reader.execute(
            "select score from v where embedding match ? and rowid=1 order by score desc limit 1",
            (VECTOR,),
        ).fetchone() == original
        reader.commit()
        assert reader.execute(
            "select score from v where embedding match ? and rowid=1 order by score desc limit 1",
            (VECTOR,),
        ).fetchone() != original
        reader.close()

        # A failed delta commit leaves both SQLite storage and the lazy warm
        # cache at the previous committed generation.
        stable = db.execute(
            "select score from v where embedding match ? and rowid=1 "
            "order by score desc limit 1",
            (OTHER,),
        ).fetchone()
        db.execute(
            "create trigger fail_delta before insert on v_chunks "
            "when new.chunk_id<0 begin select raise(abort,'forced delta failure'); end"
        )
        db.execute("insert or replace into v(rowid,embedding) values(1,?)", (VECTOR,))
        try:
            db.commit()
        except sqlite3.IntegrityError as cause:
            assert "forced delta failure" in str(cause), cause
        else:
            raise AssertionError("injected delta commit failure did not fire")
        db.rollback()
        assert db.execute(
            "select score from v where embedding match ? and rowid=1 "
            "order by score desc limit 1",
            (OTHER,),
        ).fetchone() == stable
        db.execute("drop trigger fail_delta")

        db.execute("delete from v")
        db.commit()
        assert payload(db) == empty
        assert db.execute("select count(*) from v_chunks").fetchone()[0] == 1
        db.close()
        db = connect(extension, database)
        assert db.execute("select count(*) from v").fetchone()[0] == 0
        assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
        # A single autocommit statement also crosses multiple chunk boundaries.
        db.isolation_level = None
        insert_many(db)
        assert last_rowid(db) == ROWS
        assert db.execute("select changes()").fetchone()[0] == ROWS
        db.close()

    # Fail after a full chunk has been inserted: preserve the SQL error and
    # rowid, roll back partial storage writes and the warm index, then retry.
    db = connect(extension)
    create(db)
    initial = payload(db)
    db.execute(
        "create trigger fail_chunk before insert on v_chunks when new.chunk_id=2 "
        "begin select raise(abort,'forced chunk failure'); end"
    )
    insert_many(db)
    try:
        db.commit()
    except sqlite3.IntegrityError as cause:
        # Some hosts reduce an xSync failure to its primary result code.
        assert cause.sqlite_errorcode & 0xFF == sqlite3.SQLITE_CONSTRAINT, cause
        assert "forced chunk failure" in str(cause), cause
    else:
        raise AssertionError("injected commit failure did not fire")
    assert last_rowid(db) == ROWS
    db.rollback()
    assert last_rowid(db) == ROWS
    assert db.execute("select count(*) from v").fetchone()[0] == 0
    assert payload(db) == initial
    assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
    db.execute("drop trigger fail_chunk")
    insert_many(db)
    db.commit()
    assert last_rowid(db) == ROWS
    assert db.execute("select count(*) from v").fetchone()[0] == ROWS
    db.close()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    parser.add_argument("--case", choices=("ignore", "corruption", "streaming"))
    parser.add_argument("--corrupt-worker", nargs=2, metavar=("DATABASE", "QUERY"))
    args = parser.parse_args()
    extension = args.extension.resolve()
    if args.corrupt_worker:
        corruption_worker(extension, Path(args.corrupt_worker[0]), args.corrupt_worker[1])
        return
    for name, test in (("ignore", ignore_regression), ("corruption", corruption_regression),
                       ("streaming", streaming_regression)):
        if args.case is None or args.case == name:
            test(extension)
            print(f"turbovec0 {name} storage regression passed")


if __name__ == "__main__":
    main()
