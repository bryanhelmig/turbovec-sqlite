# `turbovec-sqlite`

[![CI](https://github.com/bryanhelmig/turbovec-sqlite/actions/workflows/ci.yml/badge.svg)](https://github.com/bryanhelmig/turbovec-sqlite/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![TurboQuant paper](https://img.shields.io/badge/paper-ICLR%202026-b31b1b.svg)](https://arxiv.org/abs/2504.19874)

Compressed approximate vector search for SQLite, powered by
[`turbovec`](https://github.com/RyanCodrai/turbovec).

`turbovec-sqlite` is a loadable SQLite extension that adds writable
`turbovec0` virtual tables. It stores 2-, 3-, or 4-bit TurboQuant codes inside
ordinary SQLite shadow tables and searches them with TurboVec's SIMD kernels.
There is no graph to build, no corpus-specific training step, and no sidecar
index file.

> [!IMPORTANT]
> `turbovec-sqlite` is experimental and pre-v1. Expect breaking changes in the
> SQL interface and on-disk format. Pin a release before shipping it.

- Writable, transaction-aware `turbovec0` virtual tables
- Approximate inner-product search over aggressively compressed vectors
- Online inserts and deletes with stable SQLite `rowid` values
- Persistence, WAL, backup, atomic commit, and recovery owned by SQLite
- Native builds for Linux and macOS on x86-64 and ARM64, plus Windows x86-64

## How it differs from an exact vector scan

TurboVec is a **compressed exhaustive index**, not an HNSW or other graph-based
ANN index. Search considers every eligible vector, but scores its compressed
code instead of the original float32 vector. Approximation comes from
quantization rather than candidate pruning.

That tradeoff can make the index much smaller and the scan much faster, at the
cost of imperfect recall. Start with 4-bit indexes and measure recall on your
own embeddings before considering 3- or 2-bit codes. If exact ranking is a hard
requirement, use an exact vector extension such as
[`sqlite-vec`](https://github.com/asg017/sqlite-vec) instead.

Scores are inner products and **larger scores are better**. For normalized
vectors, inner-product and cosine-similarity rankings are equivalent.

## Installing

Download the archive for your operating system and architecture from the
repository's Releases page, or build from source. Source builds require Rust
1.89 or newer and SQLite 3.44 or newer with loadable-extension support.

```sh
git clone https://github.com/bryanhelmig/turbovec-sqlite.git
cd turbovec-sqlite
cargo build --release
```

The native library is written to:

| Platform | Library |
|---|---|
| Linux | `target/release/libturbovec_sqlite.so` |
| macOS | `target/release/libturbovec_sqlite.dylib` |
| Windows | `target/release/turbovec_sqlite.dll` |

Apple's `/usr/bin/sqlite3` is built without extension loading. On macOS, use a
SQLite build that enables it, such as Homebrew SQLite:

```sh
brew install sqlite
$(brew --prefix sqlite)/bin/sqlite3
```

## Sample usage

Load the extension and create a `turbovec0` table with a fixed dimension and
bit width:

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

```text
rowid  score
-----  -----------------
1      0.999775230884552
3      0.900349736213684
```

Run the complete example, including a join back to a content table, with:

```sh
$(brew --prefix sqlite)/bin/sqlite3 demo.db < demo.sql
```

Vectors may be JSON arrays or little-endian float32 BLOBs. `turbovec_f32()`
converts JSON to the BLOB form. Dimensions must be a positive multiple of 8.

KNN queries using `LIMIT` require `ORDER BY score DESC`. Put the KNN query in a
CTE or subquery before joining it to a content table. When the query vector
comes from SQL, use a scalar subquery: `embedding MATCH (SELECT embedding FROM
query)`. A joined column on the right of `MATCH` is not a usable virtual-table
constraint. See [`demo.sql`](demo.sql).

Filter candidates with ordinary SQL rowids:

```sql
select rowid, score
from document_vectors
where embedding match :query
  and rowid in (
    select id from documents where path glob '*.yaml'
  )
order by score desc
limit 10;
```

SQLite passes the integer rowids into the compressed scan. This returns the
true top 10 among those rowids under TurboVec's approximate score. Do not fetch
10x candidates and filter afterward: fixed overfetch cannot guarantee enough
eligible results.

## Writes and transactions

Use explicit non-negative rowids. `INSERT`, `INSERT OR REPLACE`, and `DELETE`
are supported. Ordinary `UPDATE` is rejected.

Batch writes in one transaction:

```sql
begin immediate;
insert into document_vectors(rowid, embedding) values (4, :embedding);
insert or replace into document_vectors(rowid, embedding) values (1, :replacement);
delete from document_vectors where rowid = 2;
commit;
```

Batching matters because each commit serializes the in-memory index. Commit CPU
scales with total index size, so a one-row commit can cost nearly as much as a
larger batch. Chunking keeps WAL writes small; it does not make serialization
incremental. [`BENCHMARKS.md`](BENCHMARKS.md) includes repeatable write
measurements.

The extension keeps a warm index per connection. SQLite shadow tables store
metadata and 4 MiB chunks. Commits write only changed chunk spans, while SQLite
retains responsibility for WAL, backup, atomic commit, and crash recovery. See
[`DESIGN.md`](DESIGN.md) for the virtual-table and persistence contracts.

## Language clients

Load the native library, then use ordinary SQL; no wrapper API is required.

| Language | SQLite API | Example |
|---|---|---|
| Python | standard `sqlite3` module | [`python.py`](examples/clients/python.py) |
| JavaScript | Node 22.13+ built-in `node:sqlite` | [`javascript.mjs`](examples/clients/javascript.mjs) |
| Go | `database/sql` and `mattn/go-sqlite3` | [`main.go`](examples/clients/go/main.go) |

The examples insert the same vectors and verify the same joined KNN result:

```sh
./scripts/test_clients.sh
```

The Go example requires CGO and a C compiler.

## Performance

On an Apple M1, a deterministic integration benchmark with 10,000 normalized
vectors at 1,536 dimensions produced:

| Engine | Database | Query p50 | Recall@10 |
|---|---:|---:|---:|
| `sqlite-vec` 0.1.9 exact float32 | 60.32 MiB | 25.971 ms | 1.000 |
| `turbovec0` 4-bit | 9.04 MiB | 0.509 ms | 0.791 |
| `turbovec0` 2-bit | 4.83 MiB | 0.192 ms | 0.404 |

These are deterministic synthetic vectors and integration measurements, not a
general performance claim. The comparison pins `sqlite-vec` 0.1.9 as an exact
float32 baseline. See [`BENCHMARKS.md`](BENCHMARKS.md) for the commands,
methodology, full results, and limitations.

## Current limitations

- Contentless virtual tables: keep documents and source vectors separately.
- Dimensions must be a positive multiple of 8.
- Rowids must be explicit, unique, non-negative integers.
- Inserts, deletes, and `INSERT OR REPLACE` are supported; ordinary updates are
  not.
- Search is approximate and bit width is an application-level recall choice.
- Allowlist pushdown accepts integer rowids; express metadata filters as a
  rowid subquery.
- Commits still serialize the complete warmed index before comparing chunks.
- One native library is required for each operating system and architecture.
- The SQL interface and serialized format are not yet stable.

## Development

Run the core extension suite and exact-baseline comparison with:

```sh
./scripts/test.sh
./scripts/compare_sqlite_vec.sh
./scripts/write_score.sh
./scripts/allowlist_bench.sh
./scripts/test_clients.sh
```

The suite covers the extension ABI, persistence, transactions, savepoints,
conflict policies, WAL readers, rename, defensive mode, integrity checks,
rowid allowlists, randomized transaction models, cross-language loading, and
exact-search oracles.

CI tests the declared Rust 1.89 minimum and current stable Rust. It builds,
lints, tests, and packages the extension for Linux and macOS on x86-64 and
ARM64, plus Windows x86-64, and separately verifies compatibility with SQLite
3.44. Dependabot and a weekly RustSec audit monitor locked dependencies.

Build a release archive locally with:

```sh
./scripts/package.sh
```

The archive and its SHA-256 checksum are written to `dist/`. A tag named
`sqlite-v<VERSION>` publishes the platform archives through the same tested
workflow. The prefix keeps extension releases distinct from upstream TurboVec
tags retained in the repository history.

## Advanced BLOB API

The extension also exposes `turbovec_new`, `turbovec_build`, `turbovec_add`,
`turbovec_remove`, index metadata functions, and `turbovec_knn`. This whole-BLOB
path is primarily a compact reference and correctness oracle. Each mutation
rewrites the BLOB; use `turbovec0` for normal write workloads.

## References and acknowledgments

- [*TurboQuant: Online Vector Quantization with Near-optimal Distortion
  Rate*](https://arxiv.org/abs/2504.19874), by Amir Zandieh, Majid Daliri,
  Majid Hadian, and Vahab Mirrokni (ICLR 2026), introduces the TurboQuant
  algorithms this project builds on. Google Research also provides a
  [plain-language overview](https://research.google/blog/turboquant-redefining-ai-efficiency-with-extreme-compression/).
- [`turbovec`](https://github.com/RyanCodrai/turbovec), created by Ryan Codrai,
  is the MIT-licensed Rust implementation and SIMD vector index used directly
  by this extension.
- [`sqlite-vec`](https://github.com/asg017/sqlite-vec), created by Alex Garcia,
  provides the exact float32 baseline used by this repository's comparison
  harness.

`turbovec-sqlite` adds the SQLite virtual table, transaction-aware persistence,
tests, benchmarks, language examples, and release packaging around the upstream
Rust index.

## License

MIT. See [`LICENSE`](LICENSE). The upstream `turbovec` dependency is also
distributed under the MIT License.
