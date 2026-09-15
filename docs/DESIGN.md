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

`xBegin` starts an empty change log and `xSavepoint` records only its current
position. Insert rollback removes appended rowids in reverse order. Before the
first delete or replacement, the module takes one lazy serialized checkpoint;
rollback restores it and replays only later changes. This keeps SQLite's
statement-savepoint churn independent of index size without retaining raw
vectors for an insert-only bulk load. Exact compressed-row undo remains an
upstream TurboVec API opportunity.

At `xSync`, `IdMapIndex::write_to_writer` streams the index through a 4 MiB
buffer. Each completed chunk is compared with one stored chunk, fetched by ID.
Unchanged chunks are skipped; same-size chunks use incremental BLOB I/O for
only the differing byte range; resized and new chunks use ordinary SQL. The
final partial chunk, excess old chunks, and metadata are handled before `xSync`
returns. SQLite errors survive the writer interface, and a guard restores the
application's last-insert rowid on both success and failure. Commit discards the
change log and lazy checkpoint; rollback applies them to the warm copy while
SQLite rolls back the shadow-table writes.

Reads check chunk IDs, types, and lengths against metadata before reserving the
full payload buffer. Allocation is fallible, so impossible length metadata is
reported as an error rather than aborting the host process.

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
- Loadable builds export conventional `sqlite3_extension_init` and named
  `sqlite3_turbovec_init` entry points. Static release archives export only the
  named entry point, avoiding collisions with other statically linked
  extensions.
- The build produces native dynamic and static libraries per OS/architecture.
  SQLite's stable extension ABI avoids tying the loadable library to one SQLite
  release.
- The extension pins TurboVec disk format v7 revision 2 independently of the
  crate version. Load rejects another magic or revision before deserialization.

Rusqlite 0.40 does not expose savepoint, shadow-name, or integrity module
callbacks through its builder. This crate pins 0.40.2 and locally fills those
callbacks in its `sqlite3_module`. That small compatibility seam should be
removed when Rusqlite exposes them. Its configuration wrapper also omits the
variadic value required by `SQLITE_VTAB_CONSTRAINT_SUPPORT`, so this crate
calls that host API-table function directly.

## Current limits

- Contentless table only: callers keep documents and raw vectors separately.
- Explicit rowids. Insert, delete, and `INSERT OR REPLACE` are supported;
  ordinary `UPDATE` is not.
- Commit serialization compares bounded chunks, but still traverses the complete
  index. Loads and destructive rollback checkpoints still materialize full images.
- Allowlist pushdown accepts SQLite INTEGER rowids, not arbitrary virtual-table
  column predicates. Express metadata filters as a rowid subquery.
- No automatic content triggers or WASM build.
- A long-running crash/fuzz campaign remains release work.

## Next performance work

1. Share immutable committed indexes across pooled connections. A prototype
   made later 20,000-row, 1,536-dimensional loads about 20x faster, but changes
   to shared-state layout destabilized the `INSERT OR FAIL` savepoint contract.
   Require a minimal ABI reproducer before retrying it.
2. Add changed-unit serialization and chunk-granular reload in TurboVec's core
   format. SQLite already writes changed byte spans, and serialization is now
   streamed, but the engine still visits the whole index at commit.
3. Make the serialized form directly or lazily searchable so a one-shot CLI
   does not pay a full transform before its first query.

Opt-in trigger use is a separate security/API decision. `DIRECTONLY` remains
the safe default.
