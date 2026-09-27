# Changelog

## 0.2.0 — 2026-09-26

- Bring a warm reader forward by replaying only the delta batches other
  connections committed, instead of rebuilding the whole index. A reader's first
  query after a concurrent one-row commit dropped from 7.6–12.3 ms to about
  1.4 ms on a 20,000-row by 1,536-dimension index; the saving grows with index
  size. A compaction, a gap in the delta chain, or any other surprise still
  falls back to the full read.
- Cap the delta budget at the base image. A high-dimensional index pays a fixed
  rotation and codebook cost — an empty 1,536-dimension base already serializes
  to 1.67 MiB — so the 16 MiB floor alone let small tables accumulate raw
  float32 deltas several times larger than the 4-bit base they described. A
  fresh 2,000-row by 1,536-dimension table now stores 3.21 MiB instead of
  13.97 MiB, and its cold open plus first query fell from 27 ms to 1 ms. Small
  commits on a large index are unchanged.
- Stream base chunks straight into TurboVec's loader instead of materializing
  the image first. `IdMapIndex::from_bytes` copies what it is handed, so opening
  an index held two full copies; peak growth on a 48.5 MiB image fell from
  104 MiB to 61 MiB.
- Plan joins whose `MATCH` is unusable in one loop order. `xBestIndex` now
  declines those orders instead of failing the statement, so batch KNN over a
  table of queries works: `queries q join v on v.embedding match q.embedding
  and v.k=n`. A query no order can serve reports SQLite's "no query solution"
  rather than a tailored message.
- Report the planned top-k as the KNN plan's estimated row count instead of a
  fixed 10, and keep `xBestIndex` off the blocking path when another statement
  holds the warm index.
- Free the previous virtual-table error message before replacing it. SQLite's
  savepoint callbacks never reclaim it, so a failing `xSavepoint`, `xRelease`,
  or `xRollbackTo` leaked its message.
- Fail the build if a future `libsqlite3-sys` grows `sqlite3_module`, which
  would silently stop `PRAGMA integrity_check` from reaching `turbovec0`.
- Assert in the transaction model check that each rowid holds the vector version
  the model expects, not only that the rowid exists. Exercise `turbovec0`
  creation, insertion, search, and damaged-storage reporting from the
  statically linked C smoke test.
- Say why `turbovec0` is unavailable on a host below SQLite 3.38, naming the
  required and found versions, instead of reporting a missing internal callback.
  A statically linked build inherits whatever `libsqlite3` it was linked
  against, so this is the message that path produces; the scalar `turbovec_*`
  functions keep working there.

## 0.1.6 — 2026-09-22

- Make small commits proportional to changed vectors instead of total index
  size. Commits append checksummed operation batches; bounded lazy compaction
  occasionally folds them into the existing format v7 base image.
- Remove the full-index copy from ordinary deletes and replacements. Actual
  destructive savepoint rollback rebuilds from committed storage and replays
  the retained transaction prefix.
- Extend `turbovec_info()` with base, delta, and pending-operation byte counts.
- Add a 700,000-row by 1,536-dimension WAL benchmark. It times mutation and
  commit separately, compares released and candidate search results exactly,
  and covers single edits, scattered and neighboring deletes, replacements,
  and repeated churn.
- Existing index files open without rebuilding. A database with unmerged 0.1.6
  deltas intentionally refuses to open in 0.1.5 or older instead of silently
  returning stale results.

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
