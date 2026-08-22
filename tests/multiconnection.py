#!/usr/bin/env python3
"""WAL and cross-connection cache invalidation smoke test."""

from __future__ import annotations

import sqlite3
import sys
from pathlib import Path


def connect(database: Path, extension: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(database)
    connection.enable_load_extension(True)
    connection.load_extension(str(extension))
    connection.enable_load_extension(False)
    return connection


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


if __name__ == "__main__":
    main()
