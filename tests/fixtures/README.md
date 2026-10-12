# Recall fixture

`glove-2024-50d-4224.json.gz` contains 4,096 index vectors and 128 query
vectors from Stanford's GloVe 2024 Wikipedia + Gigaword 5 50-dimensional
model. Each vector is normalized and padded with six zeros because TurboVec
requires dimensions divisible by eight.

Stanford releases the pretrained vectors under the Public Domain Dedication
and License 1.0. The source is
<https://nlp.stanford.edu/projects/glove/>. Rebuild the fixture with:

```sh
python3 scripts/build_glove_fixture.py
```

The checked-in compressed fixture is the CI input. Rebuilding it is not part of
the test run. Its SHA-256 is
`ee258df4ad9b49df6109e174eb4147b161eb4a00843be1f652c84c4a0c37902d`;
the generator also verifies the Stanford archive checksum before reading it.

## TurboVec v7 compatibility fixtures

`v7-0.3.1-{2,3,4}bit.db.gz` are SQLite databases written by the released
0.3.1 code with TurboVec 1.0.0. Each has 33,000 synthetic 128-dimensional
vectors, a v7 revision 2 base, committed insert/delete/replacement deltas,
and a scalar index BLOB. The vectors alternate between two unit vectors;
there is no external dataset or private data. `v7-0.3.1.json` records the
uncompressed database SHA-256 hashes and expected selected rows and scores.

Regenerate only with a 0.3.1 binary:

```sh
python3 tests/upstream_upgrade.py /path/to/0.3.1/libturbovec_sqlite.dylib --generate
```

CI reads the frozen fixtures using the new extension and verifies rollback,
delta replay, old WAL snapshots, compaction to v8, scalar writes, reopening,
and integrity. These are regression inputs, not a consumer migration tool.

CI passes `--baseline-release`, downloads the native 0.3.1 archive, verifies
its pinned SHA-256, and queries each frozen database in a separate process.
The new engine must exactly match those same-CPU results before the upgrade;
subsequent rollback and compaction checks retain exact scores. Offline runs
check the frozen IDs and permit a 0.005 cross-CPU quantized-score difference.
Use `--baseline-extension /path/to/0.3.1/library` for the exact check offline.
Connections close even after a failed assertion so Windows reports the
original failure instead of a locked-file cleanup error.
