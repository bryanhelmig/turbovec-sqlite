# `turbovec-sqlite`

Compressed vector search inside SQLite, with writable indexes and ordinary SQL transactions.

[![CI](https://github.com/bryanhelmig/turbovec-sqlite/actions/workflows/ci.yml/badge.svg)](https://github.com/bryanhelmig/turbovec-sqlite/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`turbovec-sqlite` is a loadable SQLite extension powered by
[`turbovec`](https://github.com/RyanCodrai/turbovec). It stores compressed
2-, 3-, or 4-bit vector codes in SQLite shadow tables. SQLite owns the WAL,
backup, atomic commit, and recovery.

> [!IMPORTANT]
> This project is pre-v1. Pin a release and measure recall on your embeddings.

## Install

Release archives contain the loadable library, static library, C header,
examples, license notices, and SHA-256 checksum:

```sh
version=0.1.6
asset=turbovec-sqlite-$version-macos-aarch64.tar.gz
base=https://github.com/bryanhelmig/turbovec-sqlite/releases/download/sqlite-v$version
curl -fLO "$base/$asset"
curl -fLO "$base/$asset.sha256"
shasum -a 256 -c "$asset.sha256"
tar -xzf "$asset"
```

Use `sha256sum -c` on Linux. Archives are published for Linux and macOS on
x86-64 and ARM64, and Windows on x86-64.

Load the dynamic library in SQLite. SQLite supplies the platform suffix when
it is omitted:

```sql
.load ./turbovec-sqlite-0.1.6-macos-aarch64/libturbovec_sqlite
select turbovec_version();
```

Build from source with Rust 1.89 or newer:

```sh
git clone https://github.com/bryanhelmig/turbovec-sqlite.git
cd turbovec-sqlite
cargo build --release --locked
```

The library is `libturbovec_sqlite.so` on Linux,
`libturbovec_sqlite.dylib` on macOS, and `turbovec_sqlite.dll` on Windows.

## The SQL you need

Create an index. Dimensions are fixed and must be divisible by eight. Start
with 4-bit codes.

```sql
create virtual table document_vectors using turbovec0(
  dimensions=1536,
  bit_width=4
);
```

Insert an explicit, non-negative rowid. The embedding may be a JSON array or a
little-endian float32 BLOB.

```sql
insert into document_vectors(rowid, embedding) values (:id, :embedding);
```

Replace a vector:

```sql
insert or replace into document_vectors(rowid, embedding)
values (:id, :embedding);
```

Skip an existing row and check whether an insertion happened:

```sql
insert or ignore into document_vectors(rowid, embedding)
values (:id, :embedding);
select changes();
```

Read `changes()` immediately after the insert, before an explicit `COMMIT`:
commit-time shadow writes can replace that count. `last_insert_rowid()` is
preserved across commit.

Some SQLite hosts emit `RETURNING` rows even for ignored virtual-table inserts.
If returned rows must identify only successful inserts, filter duplicates before
the insert instead:

```sql
insert into document_vectors(rowid, embedding)
select :id, :embedding
where not exists (
  select 1 from document_vectors where rowid = :id
)
returning rowid;
```

Delete a vector:

```sql
delete from document_vectors where rowid = :id;
```

Run filtered KNN. The `rowid IN` subquery is pushed into the compressed scan,
so this returns the true top 10 among eligible rows under TurboVec's score.

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

Do not fetch a fixed 10x candidate set and filter afterward. A selective
metadata filter can discard all of it.

Scores are approximate inner products; larger is better. Normalize vectors
when cosine ranking is desired. Ordinary `UPDATE` is not supported—use
`INSERT OR REPLACE`.

Inspect the loaded build and one index:

```sql
select turbovec_version();
-- 0.1.6

select json(turbovec_info('document_vectors'));
-- {"table":"document_vectors","generation":1,"count":370000,
--  "bit_width":4,"dimensions":1536,"serialized_bytes":...,
--  "base_bytes":...,"delta_bytes":...,"delta_operations":...,
--  "format_version":7,"format_revision":2}
```

Applications can parse `turbovec_version()` at connection setup and fail fast
when a required fix is absent. `turbovec_info()` opens and validates the index,
so treat it as diagnostics rather than a hot-path query.

See [`examples/demo.sql`](examples/demo.sql) for a complete CLI example.

## What things cost

The warm index is per connection. Opening it remains O(index), so hold a
connection open when the host permits it. Since 0.1.6, ordinary write cost is
proportional to the changes:

- deletes append rowids;
- inserts and replacements append rowids plus float32 vectors;
- a no-op transaction writes nothing;
- lazy compaction occasionally rebuilds the base image after deltas reach 25%
  of the base, 16 MiB, or 25% of the row count (with sensible minimums).

An Apple M1 benchmark used SQLite 3.53.4 in WAL/FULL mode and a 700,000-row,
1,536-dimensional, 4-bit index (about 523 MiB). Mutation and commit are timed
separately:

| Operation | 0.1.5 mutation / commit | 0.1.6 mutation / commit | 0.1.5 / 0.1.6 WAL |
|---|---:|---:|---:|
| Delete 1 | 132.9 / 304.4 ms | 10.3 / 0.3 ms | 2.36 MiB / 8 KiB |
| Delete 200 scattered | 129.8 / 876.6 ms | 6.7 / 0.4 ms | 196.01 MiB / 28 KiB |
| Delete 200 neighboring | 160.1 / 311.2 ms | 5.9 / 0.4 ms | 2.35 MiB / 28 KiB |
| Delete 5,000 scattered | 163.6 / 1,273.8 ms | 45.5 / 0.7 ms | 514.17 MiB / 72 KiB |
| Replace 5,000 scattered | 256.9 / 1,327.2 ms | 137.7 / 78.1 ms | 516.52 MiB / 29.57 MiB |

Counts and top-40 rowids and scores were identical between versions for every
case. These are measurements, not hardware promises. Reproduce them with
[`benchmarks/commit_scale.py`](benchmarks/commit_scale.py).

The operational advice is simple:

1. Hold a connection open when the host permits it. A one-shot CLI pays the
   load on every run.
2. Batch related vector writes in one transaction. It reduces SQLite and fsync
   overhead even though small vector commits are now cheap.

```sql
begin immediate;
insert into document_vectors(rowid, embedding) values (:id1, :embedding1);
insert into document_vectors(rowid, embedding) values (:id2, :embedding2);
delete from document_vectors where rowid = :old_id;
commit;
```

Ordinary deletes do not copy the index for rollback. If SQLite actually rolls
back a destructive savepoint, the extension reloads the committed base and
replays the retained transaction prefix. That makes the common path cheap and
moves O(index) work to the rare rollback path.

## Keep content and vectors in sync

`turbovec0` is `DIRECTONLY`. Triggers, views, and schema expressions cannot
invoke it. This is the safe default, but it means triggers cannot maintain the
index. Any code path that changes the content table without also writing the
vector table can silently create drift.

Load the extension in every writer and update content plus vectors in the same
transaction. If the content table retains source embeddings, reconcile with:

```sql
begin immediate;

delete from document_vectors
where rowid not in (select id from documents);

insert into document_vectors(rowid, embedding)
select id, embedding
from documents
where id not in (select rowid from document_vectors);

commit;
```

If source embeddings are not stored, re-embed missing rows before the second
statement. An opt-in trigger mode is future work; `DIRECTONLY` remains the
default.

## Language clients

Load the library, then use ordinary SQL. There is no wrapper API.

| Language | SQLite API | Example |
|---|---|---|
| Python | standard `sqlite3` | [`python.py`](examples/clients/python.py) |
| JavaScript | Node's built-in `node:sqlite` | [`javascript.mjs`](examples/clients/javascript.mjs) |
| Go | `database/sql` + `mattn/go-sqlite3` | [`main.go`](examples/clients/go/main.go) |

The Go example uses `ConnectHook` so every connection opened by
`database/sql` loads the extension. It also demonstrates filtered KNN through
`rowid IN`. Run all three clients with:

```sh
./scripts/test_clients.sh
```

The Go example requires CGO and a C compiler.

## Static linking

Release archives include `libturbovec_sqlite.a` (or
`turbovec_sqlite.lib` on Windows) and `include/turbovec_sqlite.h`. A
single-binary C, C++, Go, or Rust host can register the extension before it
opens any SQLite connection:

```c
#include "turbovec_sqlite.h"

int main(void) {
    if (sqlite3_turbovec_auto_extension() != SQLITE_OK) return 1;
    /* Every SQLite connection opened after this has turbovec0. */
}
```

The exact native system libraries vary by platform. See the repository's
[`scripts/test_static.sh`](https://github.com/bryanhelmig/turbovec-sqlite/blob/main/scripts/test_static.sh)
for the tested C link command. When building a static archive from source, use
`cargo build --release --locked --no-default-features`; this omits SQLite's
generic entry-point symbol so it cannot collide with another static extension.

## Search quality and tradeoffs

TurboVec uses a compressed exhaustive scan, not a graph. Every eligible vector
is scored, but the stored score is quantized. This avoids graph construction
and corpus-specific training, at the cost of imperfect recall.

One production-shaped integration measured OpenAI `text-embedding-3-small` at
370,000 vectors: **0.93 recall@10 and 0.96 recall@40** with 4-bit codes. The
checked-in GloVe gate currently measures **0.907 recall@10 and 0.922
recall@40** over 4,096 index vectors and 128 held-out queries. These are useful
reference points, not guarantees for a different embedding distribution.

The repeatable synthetic comparison on an Apple M1 uses 10,000 normalized
1,536-dimensional vectors:

| Engine | Database | Query p50 | Recall@10 |
|---|---:|---:|---:|
| `sqlite-vec` 0.1.9 exact float32 | 60.32 MiB | 25.971 ms | 1.000 |
| `turbovec0` 4-bit | 9.04 MiB | 0.509 ms | 0.791 |
| `turbovec0` 2-bit | 4.83 MiB | 0.192 ms | 0.404 |

Start at 4 bits. Measure recall on real queries before trying 3 or 2 bits. Use
an exact extension such as [`sqlite-vec`](https://github.com/asg017/sqlite-vec)
when exact ranking is required.

## Compatibility and support

- SQLite 3.44 or newer is required, with loadable-extension support.
- CI tests Linux x86-64 and ARM64, macOS x86-64 and ARM64, and Windows x86-64.
- CI runs the clients on Python 3.13, Node 24, and Go 1.26.
- Apple's system Python/SQLite commonly lacks extension loading. Use a
  Homebrew or uv-managed Python and verify
  `hasattr(sqlite3.Connection, "enable_load_extension")`.
- Content rows and source vectors remain application-owned.
- Rowids must be explicit, unique, non-negative SQLite integers.

The crate version and base disk format are separate. Version 0.1.6 reads the
same TurboVec format v7, revision 2 base image as 0.1.5, so no index rebuild is
needed. Small commits may add extension-owned delta records. Older versions
refuse an index while those deltas are present instead of silently ignoring
them. During 0.x, a release may intentionally break disk
compatibility and will say so in the changelog. The extension checks the header
before deserialization and refuses another format or revision with a specific
error. Keep a recoverable copy of source embeddings.

## Development

```sh
./scripts/test.sh
./scripts/test_static.sh
./scripts/test_clients.sh
./scripts/compare_sqlite_vec.sh
./scripts/write_score.sh
./scripts/package.sh
```

When dependencies change, regenerate the bundled notices with:

```sh
cargo about generate about.hbs -o THIRD_PARTY_LICENSES.html
```

Correctness gates cover transactions, nested swap-and-pop rollback, FTS5
savepoint cost, WAL readers, rowid allowlists, exact-score oracles, and fixed
real-embedding recall. CI also lints, audits dependencies, tests SQLite 3.44,
and builds all five release targets.

Further reading: [design](docs/DESIGN.md),
[benchmarks](docs/BENCHMARKS.md), and
[performance experiments](docs/PERFORMANCE_EXPERIMENTS.md).

## Acknowledgments

TurboVec implements ideas from [*TurboQuant: Online Vector Quantization with
Near-optimal Distortion Rate*](https://arxiv.org/abs/2504.19874). This extension
uses Ryan Codrai's MIT-licensed
[`turbovec`](https://github.com/RyanCodrai/turbovec) crate and compares against
Alex Garcia's [`sqlite-vec`](https://github.com/asg017/sqlite-vec).

MIT. See [`LICENSE`](LICENSE), [third-party licenses](THIRD_PARTY_LICENSES.html),
and the [security policy](SECURITY.md).
