#!/usr/bin/env python3
"""Build the small, deterministic real-embedding recall fixture."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import math
import struct
import urllib.request
import zipfile
from pathlib import Path


SOURCE_URL = "https://downloads.cs.stanford.edu/nlp/data/glove.2024.wikigiga.50d.zip"
SOURCE_MEMBER = "wiki_giga_2024_50_MFT20_vectors_seed_123_alpha_0.75_eta_0.075_combined.txt"
SOURCE_SHA256 = "afa5e258ee38272db6394547c4b075ecbb7b2164e98542c8d1237b6029b35a65"
DATABASE_ROWS = 4_096
QUERY_ROWS = 128
PADDED_DIMENSIONS = 56


def f32(value: float) -> float:
    return struct.unpack("<f", struct.pack("<f", value))[0]


def normalized(values: list[float]) -> list[float]:
    norm = math.sqrt(sum(value * value for value in values))
    return [f32(value / norm) for value in values] + [0.0] * 6


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--source",
        type=Path,
        help="existing Stanford GloVe ZIP (downloaded when omitted)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("tests/fixtures/glove-2024-50d-4224.json.gz"),
    )
    args = parser.parse_args()

    source = args.source
    if source is None:
        source = Path(".cache/glove.2024.wikigiga.50d.zip")
        if not source.exists():
            source.parent.mkdir(parents=True, exist_ok=True)
            print(f"Downloading {SOURCE_URL}")
            urllib.request.urlretrieve(SOURCE_URL, source)

    checksum = hashlib.sha256()
    with source.open("rb") as source_bytes:
        while chunk := source_bytes.read(1024 * 1024):
            checksum.update(chunk)
    digest = checksum.hexdigest()
    if digest != SOURCE_SHA256:
        raise RuntimeError(f"unexpected source SHA-256: {digest}")

    words: list[str] = []
    vectors: list[list[float]] = []
    with zipfile.ZipFile(source) as archive:
        with archive.open(SOURCE_MEMBER) as lines:
            for raw_line in lines:
                fields = raw_line.decode("utf-8").rstrip().split(" ")
                if len(fields) != 51:
                    continue
                words.append(fields[0])
                vectors.append(normalized([float(value) for value in fields[1:]]))
                if len(vectors) == DATABASE_ROWS + QUERY_ROWS:
                    break

    if len(vectors) != DATABASE_ROWS + QUERY_ROWS:
        raise RuntimeError(f"source contained only {len(vectors)} usable vectors")

    fixture = {
        "source": SOURCE_URL,
        "model": "GloVe 2024 Wikipedia + Gigaword 5, 50 dimensions",
        "dimensions": PADDED_DIMENSIONS,
        "database_words": words[:DATABASE_ROWS],
        "database_vectors": vectors[:DATABASE_ROWS],
        "query_words": words[DATABASE_ROWS:],
        "query_vectors": vectors[DATABASE_ROWS:],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("wb") as raw_output:
        with gzip.GzipFile(
            filename="", mode="wb", fileobj=raw_output, mtime=0
        ) as compressed:
            with io.TextIOWrapper(compressed, encoding="utf-8") as output:
                json.dump(fixture, output, separators=(",", ":"), sort_keys=True)
    print(args.output)


if __name__ == "__main__":
    main()
