# Changelog

## 0.1.1 — 2026-08-24

- Added writable `turbovec0` virtual tables backed by SQLite shadow tables.
- Added transaction, WAL, savepoint, integrity, rename, and reopen coverage.
- Added native `rowid IN (...)` pushdown for filtered nearest-neighbor search.
- Added repeatable correctness, performance, and `sqlite-vec` comparison tools.
- Added Python, JavaScript, and Go examples plus native release packaging.
- Fixed extension loading against the supported SQLite 3.44 runtime API.
- Improved planner errors and documented the supported KNN query shapes.
