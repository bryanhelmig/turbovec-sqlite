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
