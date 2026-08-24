# Changelog

## 0.1.2 — 2026-08-24

- Made `xBegin` and `xSavepoint` metadata-only for insert transactions.
- Added change-log rollback for inserts and lazy checkpoints for deletes and
  replacements.
- Added nested swap-and-pop rollback coverage and an FTS5 savepoint-cost gate.
- Fixed `OR FAIL` and other conflict policies by passing SQLite's required
  `SQLITE_VTAB_CONSTRAINT_SUPPORT` argument explicitly.

## 0.1.1 — 2026-08-24

- Added writable `turbovec0` virtual tables backed by SQLite shadow tables.
- Added transaction, WAL, savepoint, integrity, rename, and reopen coverage.
- Added native `rowid IN (...)` pushdown for filtered nearest-neighbor search.
- Added repeatable correctness, performance, and `sqlite-vec` comparison tools.
- Added Python, JavaScript, and Go examples plus native release packaging.
- Fixed extension loading against the supported SQLite 3.44 runtime API.
- Improved planner errors and documented the supported KNN query shapes.
