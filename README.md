# turbovec-sqlite

Compressed approximate vector search for SQLite, backed by
[TurboVec](https://github.com/RyanCodrai/turbovec).

- Writable `turbovec0` virtual tables
- 2-, 3-, or 4-bit inner-product search
- Transactional persistence in ordinary SQLite shadow tables

Larger scores are better. Start with 4-bit indexes. SQLite 3.44 or newer is
supported.

## Install

Build the native library from a source checkout:

```sh
cargo build --release
```

The library is `libturbovec_sqlite.so` on Linux,
`libturbovec_sqlite.dylib` on macOS, and `turbovec_sqlite.dll` on Windows.
Release archives contain the same library and the client examples.

Apple's `/usr/bin/sqlite3` omits extension loading. Use Homebrew SQLite or
load the library from your application.

## Quick start

```sh
sqlite3 demo.db
```

```sql
-- SQLite supplies .so, .dylib, or .dll when the suffix is omitted.
.load ./target/release/libturbovec_sqlite

create virtual table document_vectors using turbovec0(
  dimensions=8,
  bit_width=4
);

insert into document_vectors(rowid, embedding) values
  (1, '[1, 0, 0, 0, 0, 0, 0, 0]'),
  (2, '[0, 1, 0, 0, 0, 0, 0, 0]'),
  (3, '[0.9, 0.1, 0, 0, 0, 0, 0, 0]');

select rowid, score
from document_vectors
where embedding match '[1, 0, 0, 0, 0, 0, 0, 0]'
order by score desc
limit 2;
```

Vectors may be JSON arrays or little-endian float32 BLOBs. `turbovec_f32()`
converts JSON to the BLOB form. Dimensions must be a positive multiple of 8.

KNN queries using `LIMIT` require `ORDER BY score DESC`. Put the KNN query in
a CTE or subquery before joining it to a content table. See [demo.sql](demo.sql)
for a complete example.

## Language clients

Load the library, then use ordinary SQL. No wrapper API is required.

| Language | SQLite API | Example |
|---|---|---|
| Python | standard `sqlite3` module | [python.py](examples/clients/python.py) |
| JavaScript | Node 22.13+ built-in `node:sqlite` | [javascript.mjs](examples/clients/javascript.mjs) |
| Go | `database/sql` and `mattn/go-sqlite3` | [main.go](examples/clients/go/main.go) |

The examples insert the same vectors and return the same joined KNN result:

```sh
./scripts/test_clients.sh
```

The Go example requires CGO and a C compiler.

## Writes

Use explicit non-negative rowids. `INSERT` and `DELETE` are supported. Replace
a vector with `DELETE` followed by `INSERT`; `UPDATE` is rejected. Batch writes
in one transaction:

```sql
begin immediate;
insert into document_vectors(rowid, embedding) values (4, :embedding);
delete from document_vectors where rowid = 2;
commit;
```

Batching matters because each commit serializes the in-memory index. Commit
CPU scales with total index size, so a one-row commit can cost nearly as much
as a larger batch. Chunking keeps WAL writes small; it does not make
serialization incremental. [BENCHMARKS.md](BENCHMARKS.md) includes the
repeatable write measurements.

The extension keeps a warm index per connection. SQLite shadow tables store
metadata and 4 MiB chunks. Commits write only changed chunk spans. SQLite owns
WAL, backup, atomic commit, and crash recovery. [DESIGN.md](DESIGN.md) explains
the contracts and implementation.

## Performance

On an Apple M1, a deterministic smoke benchmark with 10,000 vectors at 1,536
dimensions produced:

| engine | database | query p50 | recall@10 |
|---|---:|---:|---:|
| sqlite-vec exact | 60.32 MiB | 25.971 ms | 1.000 |
| turbovec0 4-bit | 9.04 MiB | 0.509 ms | 0.791 |
| turbovec0 2-bit | 4.83 MiB | 0.192 ms | 0.404 |

See [BENCHMARKS.md](BENCHMARKS.md) for commands, methodology, full results,
and limitations.

## Test

```sh
./scripts/test.sh
./scripts/compare_sqlite_vec.sh
./scripts/write_score.sh
./scripts/test_clients.sh
```

The suite covers the extension ABI, persistence, transactions, savepoints,
conflict policies, WAL readers, rename, defensive mode, integrity checks,
randomized transaction model checks, cross-language loading, and comparison
with pinned `sqlite-vec` 0.1.9 and an exact-search oracle.

CI runs the native Rust tests on the declared Rust 1.89 minimum and checks the
current stable toolchain. The extension suite, client examples, strict Clippy,
and release packaging run across Linux, macOS, and Windows on x86-64 and ARM64.
Python is pinned in CI so SQLite-bound transaction tests cannot be silently
skipped on supported runners; Ubuntu ARM is explicitly native-only because its
Python is linked to SQLite 3.37. Dependabot and a weekly RustSec audit monitor
locked dependencies.

## Package

```sh
./scripts/package.sh
```

The archive and SHA-256 checksum land in `dist/`. CI builds Linux and macOS
archives on x86-64 and ARM64, plus Windows x86-64. A tag named `v<VERSION>`
publishes them as a GitHub release.

After extraction, load `./libturbovec_sqlite` on macOS or Linux and
`./turbovec_sqlite` on Windows. The crate pins TurboVec and the Rusqlite module
layout so upstream changes require an explicit compatibility review.

## Reference BLOB API

The extension also exposes `turbovec_new`, `turbovec_build`, `turbovec_add`,
`turbovec_remove`, index metadata functions, and `turbovec_knn`. This whole-BLOB
path is a small correctness oracle. Each mutation rewrites the BLOB; use
`turbovec0` for write workloads.

This crate is experimental and is not yet published.

## Acknowledgments

The vector index and quantization engine come from
[TurboVec](https://github.com/RyanCodrai/turbovec), created by Ryan Codrai and
used under the MIT License. This repository contains the SQLite extension,
its transactional persistence layer, tests, benchmarks, client examples, and
release packaging.
