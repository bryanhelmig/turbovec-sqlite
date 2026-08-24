# TurboVec and SQLite: architecture notes

## The core idea

TurboVec is a compressed exhaustive index, not a graph index. It normalizes and
rotates vectors, quantizes each coordinate to 2, 3, or 4 bits, and arranges codes
in 32-vector blocks for SIMD scoring. Search still considers every eligible
vector. Approximation comes from compressed scoring, not candidate pruning.

TurboVec also supports TQ+ coordinate calibration. The current `turbovec0`
table is uncalibrated; adding calibration requires an explicit lifecycle for
sampling, freezing, persistence, and later inserts.

`IdMapIndex` adds stable `u64` IDs, online inserts, swap-and-pop removal, and an
ID allowlist. `turbovec0` uses it for SQL metadata, tenant, ACL, or FTS
filtering. The mask can skip 32-row blocks with no eligible IDs.

## Why a virtual table

A SQLite virtual table is both a planner protocol and a transaction participant:

- `xBestIndex` selects usable constraints and describes cost and ordering.
- cursor callbacks execute scans and KNN results.
- `xUpdate` handles writes.
- `xBegin`, `xSync`, `xCommit`, and `xRollback` join SQLite's transaction.
- savepoint callbacks keep nested rollback consistent with in-memory state.
- `xShadowName` identifies extension-owned implementation tables.

`turbovec0` consumes `embedding MATCH ?`, integer `rowid IN (...)`, ordinary
`LIMIT`, and descending score order. A hidden `k` constraint remains as
compatibility syntax. It supports point rowid lookup and full rowid scans for
ordinary SQLite operations such as `DELETE` and `count(*)`.

SQLite presents `IN` to `xBestIndex` as an equality constraint. The module uses
SQLite's all-at-once IN API to receive the complete rowid set in one cursor
call, drops IDs without vectors, and hands the remaining IDs to `IdMapIndex`.
This computes the exact top-k within the allowed set under TurboVec's compressed
score. A post-filtered fixed overfetch cannot make that guarantee.

Allowlist speed depends on physical locality. Clustered IDs leave whole
32-vector blocks empty and can avoid most scoring. Widely scattered IDs may
touch nearly every block, so the exact filtered query can cost as much as a
full compressed scan plus selection. Correctness does not depend on locality.

## Persistence

```text
turbovec0 virtual table
    ├── warmed IdMapIndex (per SQLite connection)
    ├── <name>_meta      dimensions, bit width, generation, byte length
    └── <name>_chunks    ordered 4 MiB serialized-index chunks
```

At transaction start, the module snapshots the serialized index. Inserts and
deletes mutate only the warmed copy. At `xSync`, the index is serialized once
and compared with stored chunks. Unchanged chunks are skipped; same-size chunks
use incremental BLOB I/O for only the differing byte range; resized and new
chunks use ordinary SQL.

Commit discards the snapshot; rollback restores it. Savepoints retain their own
snapshots because whole-transaction rollback alone does not make `ROLLBACK TO`
correct.

Every committed write increments `generation`. A reader checks this cheap value
before using its cache and reloads chunks if another connection committed a new
generation. The shadow tables remain normal SQLite storage, so WAL, backup,
atomic commit, and crash recovery stay SQLite's job.

The reference BLOB functions are the simplest oracle, but each mutation and
search deserializes the full BLOB. The virtual table keeps the index warm and
persists it once per transaction.

## Existing extension contrast

`sqlite-vss` wraps Faiss and normally persists a serialized Faiss index BLOB.
It supports trained Faiss structures, but the project now points users toward
`sqlite-vec`. `sqlite-vec` is dependency-free C, stores vectors and mappings in
chunked shadow tables, and runs its own exhaustive distance scan.

TurboVec's SQLite shape is closer to `sqlite-vec`: exhaustive scan and
SQLite-owned chunk storage. Its differentiator is aggressively compressed,
rotated codes and architecture-specific SIMD kernels, with quality controlled
by bit width.

## Safety and distribution choices

- The module and resource-heavy BLOB functions are `DIRECTONLY`, keeping them
  out of schema objects, views, and triggers unless that is deliberately
  revisited.
- Dimensions, bit widths, rowids, vector lengths, serialized structure, and
  result conversions are checked before reaching the core.
- Rust panics are caught at C callback boundaries; unwinding may not cross the
  SQLite ABI.
- Both conventional `sqlite3_extension_init` and named
  `sqlite3_turbovec_init` entry points are exported.
- The build produces one native dynamic library per OS/architecture. SQLite's
  stable extension ABI avoids linking the extension to one SQLite release.

Rusqlite 0.40 does not expose savepoint, shadow-name, or integrity module
callbacks through its builder. This crate pins 0.40.2 and locally fills those
callbacks in its `sqlite3_module`. That small compatibility seam should be
removed when Rusqlite exposes them.

## Current limits

- Contentless table only: callers keep documents and raw vectors separately.
- Explicit rowids. Insert, delete, and `INSERT OR REPLACE` are supported;
  ordinary `UPDATE` is not.
- Serialization still builds one complete in-memory byte buffer before chunk
  comparison; storage writes are chunk-local, peak serialization memory is not.
- Allowlist pushdown accepts SQLite INTEGER rowids, not arbitrary virtual-table
  column predicates. Express metadata filters as a rowid subquery.
- No automatic content triggers or WASM build.
- A long-running crash/fuzz campaign remains release work.

## Next performance work

1. Share immutable committed indexes across pooled connections. A prototype
   made later 20,000-row, 1,536-dimensional loads about 20x faster, but changes
   to shared-state layout destabilized the `INSERT OR FAIL` savepoint contract.
   Require a minimal ABI reproducer before retrying it.
2. Add incremental serialization and chunk-granular reload in TurboVec's core
   format. SQLite already writes changed byte spans, but the extension still
   constructs the whole serialized index before comparing chunks.
3. Make the serialized form directly or lazily searchable so a one-shot CLI
   does not pay a full transform before its first query.

Opt-in trigger use is a separate security/API decision. `DIRECTONLY` remains
the safe default.
