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
version=0.1.4
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
.load ./turbovec-sqlite-0.1.4-macos-aarch64/libturbovec_sqlite
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
-- 0.1.4

select json(turbovec_info('document_vectors'));
-- {"table":"document_vectors","generation":1,"count":370000,
--  "bit_width":4,"dimensions":1536,"serialized_bytes":...,
--  "format_version":7,"format_revision":2}
```

Applications can parse `turbovec_version()` at connection setup and fail fast
when a required fix is absent. `turbovec_info()` reads the complete serialized
index, so treat it as diagnostics rather than a hot-path query.

See [`examples/demo.sql`](examples/demo.sql) for a complete CLI example.

## What things cost

The current warm index is per connection. Measurements from a 370,000-vector,
1,536-dimensional, 4-bit integration produced this operating model at roughly
280 MB serialized:

| Operation | Current cost | Observed time |
|---|---|---:|
| First query on a connection | O(index) load | about 0.3 s |
| Commit after a vector write | O(index) serialization | about 0.25 s |
| First delete or replacement in a transaction | one lazy O(index) checkpoint | about 85 ms |
| Insert and savepoint bookkeeping | O(changes), since 0.1.2 | near-zero fixed cost |

These values describe one integration, not a hardware promise. The consequences
are simple:

1. Hold a connection open when the host permits it. A one-shot CLI pays the
   load on every run.
2. Batch vector writes in one transaction. A one-row commit can cost nearly as
   much as a large batch.

```sql
begin immediate;
insert into document_vectors(rowid, embedding) values (:id1, :embedding1);
insert into document_vectors(rowid, embedding) values (:id2, :embedding2);
delete from document_vectors where rowid = :old_id;
commit;
```

Put inserts before deletes or replacements in a large mixed transaction. The
first destructive write creates the lazy rollback checkpoint.

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

The crate version and disk format are separate. Version 0.1.4 writes TurboVec
format v7, revision 2. During 0.x, a release may intentionally break disk
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
