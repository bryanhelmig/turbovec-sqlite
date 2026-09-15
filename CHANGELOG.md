# Changelog

## 0.1.5 — 2026-09-15

- Upgrade from 0.1.4 without rebuilding indexes. The SQL API and TurboVec
  format v7, revision 2 remain compatible.
- Reject invalid shadow-table lengths, chunk sequences, and chunk types before
  allocating an index read buffer; report allocation failure as a SQLite error.
- Preserve `last_insert_rowid()` across shadow-table writes, including failed
  commits that have already written some chunks.
- Let SQLite handle duplicate `INSERT OR IGNORE` constraints so affected-row
  counts and last-insert rowids reflect successful inserts only. Document the
  host SQLite `RETURNING` limitation and a query that avoids it.
- Stream committed indexes through a 4 MiB buffer and compare one old chunk at
  a time. Preserve the serialized bytes and changed-byte-span writes. Commit
  work still scales with index size; the full rollback checkpoint for destructive
  writes is unchanged.
- Add storage regressions for corruption, duplicate bookkeeping, chunk growth
  and shrinkage, byte compatibility, WAL snapshots, and partial commit failures.

## 0.1.4 — 2026-08-24

- Prepared the repository and install path for public use.
- Added a private security-reporting policy and third-party license notices.
- Made the static-link smoke portable across Apple and GNU archive tools.
- Reduced routine pull-request CI to Linux quality and SQLite compatibility
  gates; tags and manual runs retain the complete five-platform release matrix.
- Normalized contributor identity display through `.mailmap`.

## 0.1.3 — 2026-08-24

- Added `turbovec_info(table)` diagnostics and explicit format v7 revision 2
  checks alongside `turbovec_version()`.
- Added static libraries, a C header, and a static-registration smoke test.
- Added a fixed real-embedding recall gate using a public-domain GloVe fixture.
- Updated the Go client to load every pooled connection with `ConnectHook` and
  demonstrate filtered KNN.
- Reworked the README around install, core SQL, operating costs, drift recovery,
  compatibility, and release consumption.
- Moved architecture and benchmark notes under `docs/` and the CLI demo under
  `examples/`.

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
