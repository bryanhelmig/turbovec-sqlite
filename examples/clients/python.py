#!/usr/bin/env python3
"""Load turbovec-sqlite with Python's standard sqlite3 module."""

import json
import sqlite3
import sys
from pathlib import Path


if len(sys.argv) != 2:
    raise SystemExit(f"usage: {Path(sys.argv[0]).name} EXTENSION")

documents = [
    (1, "east", [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    (2, "north", [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    (3, "near east", [0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
]

db = sqlite3.connect(":memory:")
db.enable_load_extension(True)
db.load_extension(str(Path(sys.argv[1]).resolve()))
db.enable_load_extension(False)

db.executescript(
    """
    create table documents(id integer primary key, body text not null);
    create virtual table document_vectors using turbovec0(
      dimensions=8,
      bit_width=4
    );
    """
)
with db:
    db.executemany(
        "insert into documents(id, body) values (?, ?)",
        ((rowid, body) for rowid, body, _ in documents),
    )
    db.executemany(
        "insert into document_vectors(rowid, embedding) values (?, ?)",
        ((rowid, json.dumps(vector)) for rowid, _, vector in documents),
    )

rows = db.execute(
    """
    with matches as (
      select rowid, score
      from document_vectors
      where embedding match ?
      order by score desc
      limit ?
    )
    select d.id, d.body, printf('%.4f', matches.score)
    from matches
    join documents as d on d.id = matches.rowid
    order by matches.score desc
    """,
    (json.dumps(documents[0][2]), 2),
)
for rowid, body, score in rows:
    print(f"{rowid}\t{body}\t{score}")

db.close()
