#!/usr/bin/env python3
"""Chroma (local/embedded mode) comparison adapter driver (docs/BENCHMARKS.md
Sec1, story S18).

Invoked as a subprocess by `benches/src/competitors.rs`'s `run_chroma`
(--features compare-chroma), never on its own. Protocol, both directions
newline-delimited JSON on stdin/stdout so the Rust side never shells out to a
network service:

  stdin (one JSON object):
    {"dims": int, "ids": [str, ...], "vectors": [[f32, ...], ...],
     "queries": [[f32, ...], ...], "k": int}

  stdout (one JSON object):
    {"ingest_ms_per_op": [f64, ...],
     "query_ms": [f64, ...],
     "results": [[str, ...], ...],   # ids Chroma returned, per query
     "file_bytes": int}
    or on failure: {"error": str}

Recall is computed on the Rust side against the shared brute-force baseline
(the same pattern `run_sqlite_vec`/`run_zvec` use) so this script's only job
is to report what Chroma actually did — never a fabricated number.

Vectors are pre-normalized, pre-embedded by the same all-MiniLM-L6-v2 pass
every system in the comparison receives (BENCHMARKS.md Sec1: "same embeddings
fed to all systems") -- this script never calls an embedding function itself,
it only stores/queries raw vectors, matching how sqlite-vec/zvec are driven.
"""

import json
import pathlib
import shutil
import sys
import tempfile
import time


def main() -> int:
    try:
        import chromadb
    except ImportError as exc:
        json.dump({"error": f"chromadb not importable: {exc}"}, sys.stdout)
        return 1

    raw = sys.stdin.read()
    try:
        req = json.loads(raw)
    except json.JSONDecodeError as exc:
        json.dump({"error": f"bad request JSON: {exc}"}, sys.stdout)
        return 1

    ids = req["ids"]
    vectors = req["vectors"]
    queries = req["queries"]
    k = int(req["k"])

    # OS temp dir, not next to the script -- a stray `.chroma-run` directory
    # left behind by a crashed/interrupted run must never land in the repo.
    db_dir = str(pathlib.Path(tempfile.gettempdir()) / "embedmind-bench-chroma")
    shutil.rmtree(db_dir, ignore_errors=True)

    try:
        client = chromadb.PersistentClient(path=db_dir)
        # Cosine, matching the normalized-vector / dot-product convention every
        # other adapter in this harness uses (BENCHMARKS.md Sec1: default
        # settings, no de-tuning -- cosine is Chroma's documented default for
        # embedding search).
        collection = client.create_collection(
            name="bench", metadata={"hnsw:space": "cosine"}
        )

        # --- ingest, one vector at a time (fair comparison to `remember`) ---
        ingest_ms = []
        for doc_id, vec in zip(ids, vectors):
            started = time.perf_counter()
            collection.add(ids=[doc_id], embeddings=[vec])
            ingest_ms.append((time.perf_counter() - started) * 1000.0)

        # --- warm query latency + results, same query set/k as everyone else ---
        query_ms = []
        results = []
        for q in queries:
            started = time.perf_counter()
            res = collection.query(query_embeddings=[q], n_results=k)
            query_ms.append((time.perf_counter() - started) * 1000.0)
            results.append(res["ids"][0])

        file_bytes = _dir_size(db_dir)

        json.dump(
            {
                "ingest_ms_per_op": ingest_ms,
                "query_ms": query_ms,
                "results": results,
                "file_bytes": file_bytes,
            },
            sys.stdout,
        )
        return 0
    except Exception as exc:  # noqa: BLE001 - report, never crash silently
        json.dump({"error": f"chroma adapter failed: {exc}"}, sys.stdout)
        return 1
    finally:
        shutil.rmtree(db_dir, ignore_errors=True)


def _dir_size(path: str) -> int:
    import os

    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    return total


if __name__ == "__main__":
    sys.exit(main())
