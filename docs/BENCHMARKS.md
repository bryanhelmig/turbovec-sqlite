# `sqlite-vec` comparison

This benchmark compares the loadable extensions through the same Python
`sqlite3` connection and the same SQLite virtual-table operations. It pins the
official `sqlite-vec` v0.1.9 release (`e9f598abfa0c06b328d8fe5da9c3760cce74be10`)
and verifies the platform archive against its published SHA-256 checksum.

## Run it

For the fixed moderate-scale factory score:

```sh
./scripts/score.sh
```

The score is recall@10 multiplied by queries per second, with a recall floor
of 0.75 and mandatory correctness checks. See
[PERFORMANCE_EXPERIMENTS.md](PERFORMANCE_EXPERIMENTS.md) for the definition and
experiment log.

For the write-path factory:

```sh
./scripts/write_score.sh
```

It first checks deterministic random transactions against a Python set model,
then measures a 200-mutation transaction on a 50,000-row, 384-dimensional
index. Its one score is committed mutations per second. Mutation time, commit
time, single-vector commit time, rollback time, reopen time, WAL bytes,
database size, and correctness remain visible guardrails.

For SQLite statement-savepoint churn after a vector write:

```sh
python3 tests/savepoint_cost.py target/release/libturbovec_sqlite.dylib
```

The default builds 50,000 1,536-dimensional vectors, then compares 50 ordinary
inserts with FTS5 triggers before and after a `turbovec0` write. On the Apple M1
used here, the post-write sequence fell from 453.9 ms in 0.1.1 to 0.67 ms in
0.1.2. The check permits timing noise but rejects index-sized savepoint work.

For filtered search:

```sh
./scripts/allowlist_bench.sh \
  --rows 10000 --dimensions 1536 --queries 100
```

This includes the cost of selecting IDs from an indexed SQLite table. It also
checks each selective top-k against a filtered full compressed ranking.

On the Apple M1 used below, that shape produced:

| eligible rows | query p50 | query p95 | vs full |
|---|---:|---:|---:|
| all rows | 0.520 ms | 0.644 ms | 1.00x |
| clustered 10% | 0.213 ms | 0.246 ms | 2.44x |
| scattered 10% | 0.648 ms | 0.774 ms | 0.80x |
| clustered 1% | 0.084 ms | 0.091 ms | 6.17x |

The block mask rewards locality: clustered IDs skip compressed 32-row blocks;
scattered IDs can touch nearly all of them. The pushdown is still the correct
way to ask for a filtered top-k even when selection overhead makes it slower.

For a custom comparison shape:

```sh
./scripts/compare_sqlite_vec.sh
```

The first run downloads the pinned `sqlite-vec` library into the ignored
`target/sqlite-vec/` cache. Override the shape without editing the script:

```sh
./scripts/compare_sqlite_vec.sh \
  --rows 10000 --dimensions 1536 --queries 100 --k 10 \
  --repetitions 3 --warmup 10 --json /tmp/comparison.json
```

For a single-thread control:

```sh
RAYON_NUM_THREADS=1 ./scripts/compare_sqlite_vec.sh \
  --rows 10000 --dimensions 1536 --queries 100 --repetitions 3
```

Two narrower checks remain available:

```sh
cargo run --release --example compare -- 10000 1536 100 10
python3 scripts/bench_sqlite.py target/release/libturbovec_sqlite.dylib
```

The first compares TurboVec with exact float32 brute force. The second compares
the whole-BLOB reference API with chunked `turbovec0` persistence.

## What is compared

- One seeded xorshift generator creates identical normalized float32 database
  and query vectors for all engines.
- `sqlite-vec` performs exact float32 cosine search. `turbovec0` performs
  approximate inner-product search over compressed codes. Unit normalization
  makes those rankings equivalent.
- A small direct dot-product oracle first proves that `sqlite-vec` returns the
  exact order. Its exhaustive results are then the truth set for the larger
  recall calculation.
- The common correctness contract covers insert, `count(*)`, result length and
  order, transaction rollback, delete, persistence, and identical results after
  reopening the database.
- Insert timing includes the committing transaction. Query latency is measured
  after warmup. Reopen timing covers the first query through a new connection.
  Database size is SQLite page count times page size.
- Runs use `journal_mode=DELETE`, `synchronous=FULL`, a 64 MiB SQLite page
  cache, and alternating engine order. There are no performance pass/fail
  thresholds; CI runs only the compact correctness shape.

## Apple M1 results

Measured on macOS 26.6.1 with SQLite 3.53.4 and Python 3.14.7. Values are the
median of repeated inserts/reopens and the p50/p95 of all warmed queries.

10,000 vectors, 256 dimensions, 100 queries, k=10, five repetitions:

| engine | insert ms | DB MiB | query p50 ms | query p95 ms | reopen ms | recall@10 | top1 |
|---|---:|---:|---:|---:|---:|---:|---:|
| sqlite-vec 0.1.9 | 83.87 | 10.26 | 3.779 | 3.986 | 3.937 | 1.000 | 1.000 |
| turbovec0 4-bit | 30.15 | 1.95 | 0.085 | 0.096 | 1.177 | 0.805 | 0.720 |
| turbovec0 2-bit | 24.45 | 1.08 | 0.051 | 0.056 | 0.541 | 0.395 | 0.220 |

10,000 vectors, 1,536 dimensions, 100 queries, k=10, three repetitions:

| engine | insert ms | DB MiB | query p50 ms | query p95 ms | reopen ms | recall@10 | top1 |
|---|---:|---:|---:|---:|---:|---:|---:|
| sqlite-vec 0.1.9 | 373.46 | 60.32 | 25.971 | 27.102 | 25.728 | 1.000 | 1.000 |
| turbovec0 4-bit | 138.28 | 9.04 | 0.509 | 0.604 | 7.110 | 0.791 | 0.710 |
| turbovec0 2-bit | 101.07 | 4.83 | 0.192 | 0.209 | 2.930 | 0.404 | 0.230 |

At 1,536 dimensions, the 4-bit extension used 6.7x less database space and its
median warmed query was 51x faster, at 0.791 recall@10. Forcing
`RAYON_NUM_THREADS=1` produced essentially the same medians at this shape, so
the difference was not explained by multicore search.

## Interpretation

`sqlite-vec` is the exact baseline: it retains float32 vectors and returns full
recall. TurboVec intentionally exchanges recall for substantially smaller
scans. The 4-bit mode is the useful default in these tests. The 2-bit mode is a
specialized size/latency choice whose recall loss must be validated against a
real corpus.

These are deterministic synthetic vectors, not semantic embeddings, and the
current `turbovec0` virtual table is uncalibrated plain TurboQuant. The numbers
are integration smoke measurements rather than general performance claims.
Before choosing a bit width, repeat the comparison with representative vectors
and queries; adding an explicit TQ+ calibration lifecycle is a separate design
decision.
