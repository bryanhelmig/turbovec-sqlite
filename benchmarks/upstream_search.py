#!/usr/bin/env python3
"""Compare upstream search at 2/4 bits using a pinned real-embedding fixture.

Requires numpy. Build each variant in a separate process; measure five fresh
processes per variant in alternating order. Exact float32 neighbors are computed
outside the timed processes. This is a public corpus, not IDX's private data.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import sqlite3
import statistics
import subprocess
import sys
import time
from pathlib import Path

import numpy as np

from fresh_commit import connect, peak_rss

CASES = {"all_k10": (10, False), "all_k40": (40, False), "filtered_k10": (10, True)}


def args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--extension", type=Path, required=True)
    parser.add_argument("--baseline-extension", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--workdir", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--json", type=Path)
    parser.add_argument("--child", choices=("build", "query"), help=argparse.SUPPRESS)
    parser.add_argument("--database", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--bits", type=int, default=4, help=argparse.SUPPRESS)
    parsed = parser.parse_args()
    if parsed.repeat < 1 or parsed.threads < 1:
        parser.error("--repeat and --threads must be positive")
    return parsed


def child(a: argparse.Namespace) -> dict:
    if a.child == "build":
        vectors = np.load(a.data_dir / "vectors.npy")
        db = connect(a.database, a.extension)
        db.execute(f"create virtual table v using turbovec0(dimensions={vectors.shape[1]},bit_width={a.bits})")
        db.execute("begin")
        db.executemany("insert into v(rowid,embedding) values(?,?)",
                       ((i + 1, row.tobytes()) for i, row in enumerate(vectors)))
        db.execute("commit")
        assert db.execute("pragma integrity_check").fetchall() == [("ok",)]
        info = json.loads(db.execute("select turbovec_info('v')").fetchone()[0])
        db.close()
        return info
    queries = np.load(a.data_dir / "queries.npy")
    query_blobs = [query.tobytes() for query in queries]
    db = connect(a.database, a.extension)
    count = db.execute("select count(*) from v").fetchone()[0]
    sql = "select rowid,score from v where embedding match ? and k=?"
    started = time.perf_counter()
    db.execute(sql, (query_blobs[0], 10)).fetchall()
    first_ms = (time.perf_counter() - started) * 1000
    db.execute("create temp table allowed(id integer primary key)")
    db.executemany("insert into allowed values(?)", ((i,) for i in range(1, count + 1, 10)))
    result = {"first_search_ms": first_ms, "cases": {}}
    for name, (k, filtered) in CASES.items():
        query_sql = sql + (" and rowid in (select id from allowed)" if filtered else "")
        elapsed = []
        results = []
        for query in query_blobs:
            started = time.perf_counter()
            rows = db.execute(query_sql, (query, k)).fetchall()
            elapsed.append((time.perf_counter() - started) * 1000)
            assert len(rows) == k
            if filtered:
                assert all((rowid - 1) % 10 == 0 for rowid, _ in rows)
            results.append(rows)
        result["cases"][name] = {"median_ms": statistics.median(elapsed), "rows": results}
    db.close()
    result["peak_rss_bytes"] = peak_rss()
    return result


def run(a: argparse.Namespace, mode: str, extension: Path, database: Path, bits: int,
        classic: bool = False) -> dict:
    env = {**os.environ, "RAYON_NUM_THREADS": str(a.threads),
           "TURBOVEC_2BIT_PLANES": "0" if classic else "1",
           "TURBOVEC_4BIT_PLANES": "0" if classic else "1"}
    output = subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), "--child", mode,
         "--extension", str(extension.resolve()), "--baseline-extension", str(a.baseline_extension.resolve()),
         "--data-dir", str(a.data_dir.resolve()), "--workdir", str(a.workdir.resolve()),
         "--database", str(database.resolve()), "--bits", str(bits)],
        env=env, capture_output=True, text=True,
    )
    if output.returncode:
        raise RuntimeError(output.stderr)
    return json.loads(output.stdout)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main() -> None:
    a = args()
    if a.child:
        print(json.dumps(child(a)))
        return
    a.workdir.mkdir(parents=True, exist_ok=True)
    source = json.loads((a.data_dir / "fixture.json").read_text())
    assert digest(a.data_dir / "vectors.npy") == source["vectors_sha256"]
    assert digest(a.data_dir / "queries.npy") == source["queries_sha256"]
    vectors = np.load(a.data_dir / "vectors.npy")
    queries = np.load(a.data_dir / "queries.npy")
    truth = {}
    for name, (k, filtered) in CASES.items():
        allowed = np.arange(0, len(vectors), 10 if filtered else 1)
        products = queries @ vectors[allowed].T
        top = np.argpartition(products, -k, axis=1)[:, -k:]
        truth[name] = [set(allowed[indices] + 1) for indices in top]
    del vectors, queries, products
    report = {"fixture": source, "platform": platform.platform(), "sqlite": sqlite3.sqlite_version,
              "numpy": np.__version__, "python": platform.python_version(), "threads": a.threads,
              "repeat": a.repeat, "page_cache": "warm", "command": [sys.executable, *sys.argv],
              "baseline_sha256": digest(a.baseline_extension), "candidate_sha256": digest(a.extension),
              "results": {}}
    for bits in (2, 4):
        variants = {"0.3.1": (a.baseline_extension, False), "0.4.0": (a.extension, False),
                    "0.4.0_classic": (a.extension, True)}
        databases = {name: a.workdir / f"{name}-{bits}.db" for name in variants}
        builds = {}
        samples = {name: [] for name in variants}
        canonical = {}
        for name, (extension, classic) in variants.items():
            databases[name].unlink(missing_ok=True)
            builds[name] = run(a, "build", extension, databases[name], bits, classic)
            canonical[name] = run(a, "query", extension, databases[name], bits, classic)
        for repetition in range(a.repeat):
            names = list(variants) if repetition % 2 == 0 else list(reversed(variants))
            for name in names:
                extension, classic = variants[name]
                sample = run(a, "query", extension, databases[name], bits, classic)
                for case in CASES:
                    assert sample["cases"][case]["rows"] == canonical[name]["cases"][case]["rows"]
                    del sample["cases"][case]["rows"]
                samples[name].append(sample)
                print(f"{bits}-bit {repetition + 1} {name}: {sample['cases']}", flush=True)
        bit_report = {}
        for name in variants:
            cases = {}
            for case, (k, _) in CASES.items():
                rows = canonical[name]["cases"][case]["rows"]
                baseline = canonical["0.3.1"]["cases"][case]["rows"]
                recall = sum(len({r[0] for r in actual} & expected)
                             for actual, expected in zip(rows, truth[case])) / (len(rows) * k)
                agreement = sum({r[0] for r in actual} == {r[0] for r in old}
                                for actual, old in zip(rows, baseline)) / len(rows)
                for actual, old in zip(rows, baseline):
                    old_scores = dict(old)
                    assert all(score == old_scores[rowid] for rowid, score in actual if rowid in old_scores)
                cases[case] = {"median_ms": statistics.median(s["cases"][case]["median_ms"] for s in samples[name]),
                               "recall": recall, "fraction_queries_matching_baseline_ids": agreement,
                               "result_sha256": hashlib.sha256(json.dumps(rows).encode()).hexdigest()}
            bit_report[name] = {"cases": cases, "build": builds[name], "samples": samples[name],
                                "first_search_ms_median": statistics.median(s["first_search_ms"] for s in samples[name]),
                                "peak_rss_mib_median": statistics.median(s["peak_rss_bytes"] for s in samples[name]) / 2**20}
        report["results"][str(bits)] = bit_report
        if a.json:
            a.json.write_text(json.dumps(report, indent=2) + "\n")
        for case in CASES:
            assert bit_report["0.4.0"]["cases"][case]["recall"] >= bit_report["0.3.1"]["cases"][case]["recall"] - 0.005
    print(json.dumps({bits: {name: result["cases"] for name, result in variants.items()}
                      for bits, variants in report["results"].items()}, indent=2))


if __name__ == "__main__":
    main()
