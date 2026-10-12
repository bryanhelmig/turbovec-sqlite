#!/usr/bin/env python3
"""Fetch one pinned public embedding shard for upstream_search.py.

Requires numpy and pyarrow. Downloads 350 MiB; keeps 36,000 database vectors
and 256 held-out queries, all normalized float32 at 1,536 dimensions.
"""
import hashlib
import json
import shutil
import sys
import urllib.request
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

REPO = "Qdrant/dbpedia-entities-openai3-text-embedding-3-large-1536-1M"
REVISION = "4a9731217921bc476a0f03544f11f22ae4903fa5"
FILE = "data/train-00000-of-00026.parquet"
SHA256 = "27519d7104c8ac6d0f51ac18e8ac23b29d67cbab8465bc3302b458ca3dcbbb78"
URL = f"https://huggingface.co/datasets/{REPO}/resolve/{REVISION}/{FILE}"


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main() -> None:
    root = Path(sys.argv[1])
    root.mkdir(parents=True, exist_ok=True)
    shard = root / "openai-1536.parquet"
    if not shard.exists():
        temporary = shard.with_suffix(".tmp")
        with urllib.request.urlopen(URL, timeout=60) as source, temporary.open("wb") as dest:
            shutil.copyfileobj(source, dest)
        assert digest(temporary) == SHA256
        temporary.rename(shard)
    assert digest(shard) == SHA256
    column = "text-embedding-3-large-1536-embedding"
    array = pq.read_table(shard, columns=[column])[column].combine_chunks()
    vectors = np.asarray(array.values).reshape(len(array), 1536).astype(np.float32)
    vectors = vectors[:36256].copy()
    vectors /= np.linalg.norm(vectors, axis=1, keepdims=True)
    np.save(root / "vectors.npy", vectors[:36000])
    np.save(root / "queries.npy", vectors[36000:])
    report = {
        "dataset": REPO, "revision": REVISION, "file": FILE, "url": URL,
        "sha256": SHA256, "bytes": shard.stat().st_size,
        "rows": 36000, "dimensions": 1536, "query_rows": 256,
        "selection": "first 36000 rows as database, next 256 as held-out queries; L2-normalized float32",
        "vectors_sha256": digest(root / "vectors.npy"),
        "queries_sha256": digest(root / "queries.npy"),
    }
    (root / "fixture.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
