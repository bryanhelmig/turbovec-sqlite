# Performance experiments

`./scripts/score.sh` is the factory benchmark. Its fixed
moderate shape is 25,000 normalized vectors, 768 dimensions, 100 queries,
`k=10`, and three repetitions.

The one optimization metric is:

```text
score = recall@10 * 1000 / median query milliseconds
```

This is quality-adjusted queries per second. Higher is better. A score is
eligible only when recall is at least 0.75 and the harness's exact-ranking,
transaction, delete, ordering, determinism, and reopen checks pass. Insert
time, p95 query latency, reopen time, and database size are guardrails: a
retained change must not materially regress them.

For this local loop, less than a 5% score change is treated as noise unless
repeated runs show otherwise. Correctness failures and clear guardrail
regressions always reject an experiment regardless of score.

Results are local Apple M1 measurements. Absolute timings are not portable;
before-and-after results on the same machine are the useful comparison.

## Log

| # | Idea | Score | Delta | Guardrails | Decision |
|---:|---|---:|---:|---|---|
| 0 | Unmodified 4-bit baseline | 1445.0 | - | 169.85 ms insert; 0.587 ms p95; 7.483 ms reopen; 10.28 MiB | baseline |
| 1 | Use a 3-bit index | 1519.7 raw | +5.2% | recall 0.628 | reject: ineligible |
| 2 | Use a 2-bit index | 1822.1 raw | +26.1% | recall 0.390 | reject: ineligible |
| 3 | Force one Rayon worker | 1448.3 | +0.2% | reopen 9.285 ms | reject: noise |
| 4 | Force two Rayon workers | 1459.7 | +1.0% | insert 196.19 ms | reject: noise/regression |
| 5 | Force four Rayon workers | 1448.7 | +0.3% | reopen 8.242 ms | reject: noise |
| 6 | Compile for the native CPU | 1422.8 | -1.5% | insert 193.13 ms | reject: regression |
| 7 | Eagerly prepare search caches | 1460.0 | +1.0% | p95 0.601 ms; reopen 7.689 ms | reject: noise |
| 8 | Skip the generation check | 1467.9 | +1.6% | multi-connection test failed | reject: incorrect |
| 9 | Gate refresh with `PRAGMA data_version` | not scored | - | transaction test failed | reject: incorrect |
| 10 | Borrow aligned float32 query BLOBs | 1430.0 | -1.0% | p95 0.625 ms; reopen 8.475 ms | reject: regression/complexity |

Detailed notes follow after the ten experiments.

## Notes

### 1. Three-bit index

The smaller code made queries faster, but recall fell from 0.791 to 0.628.
The raw score therefore does not qualify. Database size also stayed at 10.28
MiB because both three- and four-bit payloads cross the same SQLite page-count
boundary at this shape.

### 2. Two-bit index

The raw score and 5.31 MiB database look attractive, but 0.390 recall is far
below the gate. Two-bit remains an explicit space-first choice, not the
factory default.

### 3. One Rayon worker

`RAYON_NUM_THREADS=1` was indistinguishable from automatic selection and made
reopen slower in this run. The query is too small to benefit from changing the
pool policy, so the extension should leave it to TurboVec and the application.

### 4. Two Rayon workers

The 1.0% score change is below a useful signal and insert time regressed by
15.5%. No configuration was retained.

### 5. Four Rayon workers

The result again matched the baseline. Fixed Rayon pool sizes are not a useful
extension-level tuning surface for this workload.

### 6. Native-CPU compilation

`RUSTFLAGS='-C target-cpu=native'` made the score 1.5% worse and insert time
13.7% worse. TurboVec already dispatches its search kernels at runtime; making
release binaries machine-specific adds distribution risk without a benefit.

### 7. Eager cache preparation

Calling `IdMapIndex::prepare()` after load and commit did not improve the
already-warmed score, p95, or reopen measurement. Existing benchmark warmup
already keeps lazy preparation outside the scored window, so the extra code
was removed.

### 8. Skip generation checks

Removing the metadata read barely changed the score, then failed
`tests/multiconnection.py`: an already-open reader did not observe another
connection's commit. Cross-connection visibility is mandatory, so this result
is rejected regardless of speed.

### 9. Gate refresh with SQLite's data version

This preserved external-connection detection but failed the conflict-policy
transaction test before benchmarking. `data_version` does not advance for a
connection's own writes, while the in-memory rollback path still needs the
persisted generation check. Adding exception state would cost more complexity
than this metadata read warrants.

### 10. Borrow aligned query BLOBs

On little-endian systems this avoided allocating and decoding aligned float32
BLOB queries. All correctness checks passed, but the score fell 1.0% and both
p95 and reopen measurements worsened. Vector parsing is not the bottleneck at
this shape, so the unsafe, platform-sensitive path was removed.

## Conclusion

None of the ten ideas produced a material, correct improvement over the
simple four-bit implementation. The experiments rule out bit-width changes,
extension-level thread tuning, machine-specific builds, eager preparation,
weaker invalidation, `data_version` gating, and zero-copy query parsing as
useful next steps at this scale. Keep the current implementation and use this
score before accepting future performance changes.

A clean post-experiment rerun scored 1451.5, 0.4% above the 1445.0 baseline,
which confirms the expected noise band and that the implementation returned
to its original performance.

## Write-path follow-up

`scripts/write_score.sh` adds a transaction model checker and a write score:
committed mutations per second. Its default 50,000-row, 384-dimensional run
measured 13,632.5 mutations per second for a 200-mutation transaction. The
median transaction was 14.671 ms: 4.348 ms mutating and 10.322 ms committing.
A single-vector transaction took 12.766 ms. WAL growth was only 32.2 KiB for
a 10.18 MiB database.

At 100,000 rows and 768 dimensions, the score was 3,205.1. A 200-mutation
transaction took 62.401 ms, a single-vector transaction took 46.507 ms, and
WAL growth was 56.4 KiB for a 38.64 MiB database. Chunking therefore controls
write amplification well, while whole-index transaction and serialization
cost scales with index size.

One KISS optimization was attempted: omit the transaction-start snapshot and
reload SQLite's rolled-back shadow image on demand. The conflict-policy test
rejected it before benchmarking because `INSERT OR FAIL` must preserve earlier
rows from the same statement. A future optimization needs a genuinely cheap
in-memory snapshot or incremental TurboVec persistence; batching writes is the
correct current advice.

The 0.1.2 follow-up retained in-memory rollback but changed its shape: inserts
record rowids, savepoints record change-log positions, and the first destructive
change takes one lazy checkpoint. The conflict suite and a 20,000-operation
random model passed. In the 50,000-row, 1,536-dimensional FTS5 reproducer, 50
post-vector-write statements fell from 453.9 ms to 0.67 ms. Deletes still need
the one lazy full-index checkpoint until TurboVec exposes compressed-row undo.

## Pooled-connection cache follow-up

An immutable process-wide cache was prototyped after the allowlist work. On a
20,000-row, 1,536-dimensional index, the first reader loaded in 8.82 ms and two
sibling connections warmed in 0.40 ms and 0.28 ms. After a writer committed,
the first reader reloaded in 7.65 ms and the siblings reused that generation in
0.24 ms and 0.20 ms.

The prototype was rejected. Shared-state changes made the `INSERT OR FAIL`
savepoint test sensitive to otherwise irrelevant code-layout and diagnostic
changes. A retained cache needs a minimal reproducer, concurrent cold-start
coverage, rollback/savepoint tests, and a pooled memory/load benchmark.

## Streaming commits — September 15, 2026

Version 0.1.5 uses the pinned engine's `write_to_writer` API
with a 4 MiB buffer and loads one old chunk at a time. This removes the complete
new serialized buffer and the collection of every old chunk from `xSync`.
Changed-byte-span BLOB writes and the disk format are preserved. Serialization
still visits the entire index, and destructive rollback still takes its lazy
full-index checkpoint.

Baseline: released 0.1.4 at `f67f564`. Candidate: the 0.1.5 implementation
on `codex/streaming-commits`.
Apple M1, macOS 26.6.1, SQLite 3.53.4, Rust 1.89.0. Raw measurements and the
candidate source checksum are in
[`streaming-commits-20260915.json`](../benchmarks/results/streaming-commits-20260915.json).

### Memory

Three fresh processes per variant, each building 100,000 vectors at 1,536
dimensions / four bits, committing, then appending one vector and committing.
Median process peak RSS through the append commit fell from **312.7 MiB to
174.0 MiB**, a **44.4% reduction**. The three observations were
312.7/312.9/312.7 MiB before and 174.2/174.0/172.5 MiB after.

This is whole-process memory, including Python, SQLite, and the warm index.
Inputs are repeated sparse vectors to exercise storage geometry, not a recall
workload. Reproduce each variant in its own process with:

```sh
python3 benchmarks/commit_memory.py --extension /path/to/libturbovec_sqlite.dylib
```

The benchmark reports bytes and supports macOS and Linux. Peak RSS is comparable
within the same host/environment; these results are not a fleet-capacity claim.

### Write latency and storage

The existing write benchmark ran sequentially for each variant, with five
repetitions at each shape. Each transaction inserts 100 and deletes 100 vectors.

| Shape | Commit before | Commit after | WAL before/after |
|---|---:|---:|---:|
| 50k rows, 384 dimensions | 8.36 ms | 8.66 ms | 32.2 / 32.2 KiB |
| 100k rows, 768 dimensions | 23.71 ms | 23.69 ms | 56.4 / 56.4 KiB |

Commit latency is effectively unchanged at these shapes. Single-insert
transaction latency changed from 7.91 to 7.97 ms and from 22.83 to 23.44 ms,
respectively. These differences are within the existing 5% noise threshold.
Reopen time did not regress. The retained benefit is lower memory use.

### Correctness

Storage regressions compare streamed shadow bytes with the scalar BLOB writer,
including replacement and shrinking a multiple-chunk index to empty. They check
WAL snapshot visibility, explicit/autocommit rowids, corruption rejection, and
rollback plus retry after a forced failure in the third chunk. The full Python
suite passed on both SQLite 3.44.0 and 3.53.4; real-embedding recall remained
0.907 at 10 and 0.922 at 40. Static linking and all three language clients passed.

## Change-proportional commits — September 22, 2026

Version 0.1.6 separates two costs that version 0.1.5 paid on a destructive
write. The mutation itself could serialize a rollback checkpoint, and commit
then serialized and compared the complete index again. The replacement design
logs row-level operations and appends one checksummed delta batch at commit.
Deletes contain IDs only. Inserts and replacements contain IDs plus source
float32 vectors. Lazy compaction periodically folds those batches into the
unchanged format v7 base image.

The benchmark is [`commit_scale.py`](../benchmarks/commit_scale.py). Its default
shape is the production geometry: 700,000 rows, 1,536 dimensions, 4-bit codes,
WAL mode, `synchronous=FULL`, and disabled automatic WAL checkpoints. One base
database is copied for every independent case. Each database is reopened,
checked with `integrity_check`, and searched with four fixed queries. With
`--baseline-extension`, row counts and the complete top-40 rowid/score digest
must match exactly.

```sh
python3 benchmarks/commit_scale.py \
  --baseline-extension /path/to/0.1.5/libturbovec_sqlite.dylib \
  --extension target/release/libturbovec_sqlite.dylib \
  --workdir /fast/local/turbovec-commit-scale \
  --json /tmp/turbovec-commit-scale.json
```

Apple M1, macOS 26.6.1, SQLite 3.53.4. The base database was 523 MiB.

| Case | 0.1.5 mutate | 0.1.5 commit | 0.1.6 mutate | 0.1.6 commit | WAL before / after |
|---|---:|---:|---:|---:|---:|
| Insert 1 | 9.4 ms | 375.2 ms | 9.1 ms | 0.4 ms | 12 KiB / 32 KiB |
| Insert 200 | 17.7 ms | 441.9 ms | 10.8 ms | 3.6 ms | 2.49 MiB / 1.21 MiB |
| Delete 1 | 132.9 ms | 304.4 ms | 10.3 ms | 0.3 ms | 2.36 MiB / 8 KiB |
| Delete 200 scattered | 129.8 ms | 876.6 ms | 6.7 ms | 0.4 ms | 196.01 MiB / 28 KiB |
| Delete 200 neighboring | 160.1 ms | 311.2 ms | 5.9 ms | 0.4 ms | 2.35 MiB / 28 KiB |
| Delete 5,000 scattered | 163.6 ms | 1,273.8 ms | 45.5 ms | 0.7 ms | 514.17 MiB / 72 KiB |
| Replace 200 scattered | 166.5 ms | 797.1 ms | 10.7 ms | 3.3 ms | 187.15 MiB / 1.21 MiB |
| Replace 5,000 scattered | 256.9 ms | 1,327.2 ms | 137.7 ms | 78.1 ms | 516.52 MiB / 29.57 MiB |

Twenty rounds of 200 scattered deletes plus 200 inserts had a median commit of
742.3 ms and 189.49 MiB WAL on 0.1.5, versus 3.3 ms and 1.21 MiB on 0.1.6.
All compared search digests and row counts were identical.

The 5,000-delete case did not reproduce the reported 9-second commit. On the
released implementation it spent 1.27 seconds committing and wrote 514 MiB,
so complete serialization and WAL I/O still dominated. The candidate spent
45.5 ms mutating, 0.7 ms committing, and wrote 72 KiB. The isolated result does
not justify tuning for the one heavily loaded 9-second observation.
