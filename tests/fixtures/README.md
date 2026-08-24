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
