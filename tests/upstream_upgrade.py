#!/usr/bin/env python3
"""Read real 0.3.1 v7 fixtures, replay deltas, compact to v8, and reopen."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import math
import sqlite3
import struct
import tempfile
from pathlib import Path

FIXTURES = Path(__file__).parent / "fixtures"
DIMENSIONS = 128
ROWS = 33_000
X = struct.pack(f"<{DIMENSIONS}f", 1, *([0] * (DIMENSIONS - 1)))
Y = struct.pack(f"<{DIMENSIONS}f", 0, 1, *([0] * (DIMENSIONS - 2)))


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    db = sqlite3.connect(database, isolation_level=None)
    db.enable_load_extension(True)
    db.load_extension(str(extension.resolve()))
    db.enable_load_extension(False)
    return db


def header(db: sqlite3.Connection) -> bytes:
    return db.execute("select substr(data,1,5) from v_chunks where chunk_id=0").fetchone()[0]


def selected(db: sqlite3.Connection) -> list:
    return [db.execute(
        "select rowid,score from v where embedding match ? and k=3 "
        "and rowid in (1,3,33001) order by score desc, rowid", (query,)
    ).fetchall() for query in (X, Y)]


def generate(extension: Path) -> None:
    manifest = {"writer": "turbovec-sqlite 0.3.1 / TurboVec 1.0.0",
                "dimensions": DIMENSIONS, "rows": ROWS, "fixtures": {}}
    with tempfile.TemporaryDirectory(prefix="turbovec-v7-build-") as directory:
        for bits in (2, 3, 4):
            db_path = Path(directory) / f"v7-{bits}.db"
            db = connect(db_path, extension)
            assert db.execute("select turbovec_version()").fetchone()[0] == "0.3.1"
            db.execute("pragma journal_mode=wal")
            db.execute(f"create virtual table v using turbovec0(dimensions={DIMENSIONS},bit_width={bits})")
            db.execute("begin")
            db.executemany("insert into v(rowid,embedding) values(?,?)",
                           ((i, X if i % 2 else Y) for i in range(1, ROWS + 1)))
            db.execute("commit")
            db.execute("begin")
            db.execute("delete from v where rowid=2")
            db.execute("insert or replace into v(rowid,embedding) values(3,?)", (Y,))
            db.execute("insert into v(rowid,embedding) values(33001,?)", (X,))
            db.execute("commit")
            assert header(db) == b"TV7\0\x02"
            assert db.execute("select delta_operations from v_info").fetchone()[0] > 0
            expected = selected(db)
            # Also exercise the scalar BLOB API's v7 reader and v8 writer.
            db.execute("create table scalar(payload blob)")
            db.execute("insert into scalar values(turbovec_add(turbovec_new(128,?),1,?))", (bits, X))
            db.execute("pragma wal_checkpoint(truncate)")
            db.close()
            payload = db_path.read_bytes()
            name = f"v7-0.3.1-{bits}bit.db.gz"
            (FIXTURES / name).write_bytes(gzip.compress(payload, mtime=0))
            manifest["fixtures"][str(bits)] = {
                "file": name, "database_sha256": hashlib.sha256(payload).hexdigest(),
                "expected": expected,
            }
    (FIXTURES / "v7-0.3.1.json").write_text(json.dumps(manifest, indent=2) + "\n")


def verify(extension: Path) -> None:
    manifest = json.loads((FIXTURES / "v7-0.3.1.json").read_text())
    with tempfile.TemporaryDirectory(prefix="turbovec-upstream-upgrade-") as directory:
        for bits, fixture in manifest["fixtures"].items():
            database = Path(directory) / f"upgrade-{bits}.db"
            payload = gzip.decompress((FIXTURES / fixture["file"]).read_bytes())
            assert hashlib.sha256(payload).hexdigest() == fixture["database_sha256"]
            database.write_bytes(payload)
            expected = [[tuple(row) for row in query] for query in fixture["expected"]]
            db = connect(database, extension)
            assert header(db) == b"TV7\0\x02"
            assert db.execute("select count(*) from v").fetchone()[0] == ROWS
            # Rollback while the index is still unloaded, before any search.
            db.execute("savepoint outer_point")
            db.execute("delete from v where rowid=1")
            db.execute("rollback to outer_point")
            db.execute("release outer_point")
            observed = selected(db)
            # Frozen scores were produced on ARM. Other SIMD kernels may round
            # float32 arithmetic differently; later checks on this host are exact.
            for actual_query, recorded_query in zip(observed, expected):
                assert len(actual_query) == len(recorded_query)
                for (rowid, score), (old_rowid, old_score) in zip(actual_query, recorded_query):
                    assert rowid == old_rowid
                    assert math.isclose(score, old_score, rel_tol=1e-6, abs_tol=1e-6), (score, old_score)
            expected = observed
            info = json.loads(db.execute("select turbovec_info('v')").fetchone()[0])
            assert (info["format_version"], info["format_revision"]) == (7, 2)
            assert db.execute("select turbovec_len(payload) from scalar").fetchone()[0] == 1
            db.execute("update scalar set payload=turbovec_add(payload,2,?)", (Y,))
            assert db.execute("select substr(payload,1,5) from scalar").fetchone()[0] == b"TV8\0\x01"
            reader = connect(database, extension)
            reader.execute("begin")
            assert selected(reader) == expected
            # A small commit keeps v7 and its delta chain readable.
            db.execute("insert into v(rowid,embedding) values(33002,?)", (X,))
            assert header(db) == b"TV7\0\x02"
            db.close()
            db = connect(database, extension)
            assert selected(db) == expected
            # A real compaction rewrites the base with the new writer.
            db.execute("begin")
            db.executemany("insert into v(rowid,embedding) values(?,?)",
                           ((i, X) for i in range(33003, 43003)))
            db.execute("commit")
            assert header(db) == b"TV8\0\x01"
            assert selected(db) == expected
            assert selected(reader) == expected  # old WAL snapshot stays usable
            assert header(reader) == b"TV7\0\x02"
            reader.execute("commit")
            assert selected(reader) == expected  # warm reader crosses compaction
            assert header(reader) == b"TV8\0\x01"
            reader.close()
            db.close()
            db = connect(database, extension)
            assert selected(db) == expected
            assert db.execute("select count(*) from v").fetchone()[0] == ROWS + 10_001
            info = json.loads(db.execute("select turbovec_info('v')").fetchone()[0])
            assert (info["format_version"], info["format_revision"]) == (8, 1)
            assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
            db.close()
    print("v7 to v8: 2/3/4-bit scalar, delta, rollback, WAL, compaction and reopen checks passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("extension", type=Path)
    parser.add_argument("--generate", action="store_true", help="regenerate fixtures using a 0.3.1 binary")
    args = parser.parse_args()
    generate(args.extension) if args.generate else verify(args.extension)
