#!/usr/bin/env bash
# Reproduces the dogfooding numbers cited in docs/launch/post-3-dogfooding.md:
# writes the same 16 real facts about this project via `embedmind remember`,
# runs the same 5 `embedmind recall` queries, then prints `embedmind stats`.
# Usage: ./tools/dogfood-sample.sh [path/to/dogfood.mind]
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
file="${1:-$repo_root/.dogfood-run/dogfood.mind}"
bin="$repo_root/target/release/embedmind"
[ -x "$bin" ] || bin="$repo_root/target/release/embedmind.exe"
[ -x "$bin" ] || { echo "build the release binary first: cargo build --release" >&2; exit 1; }

mkdir -p "$(dirname "$file")"
rm -f "$file" "${file}-wal"

memories=(
"S13 graph layer: paged entity/relation graph module over the shared dictionary, wired into remember/related/recall/stats/vacuum. Adjacency stored as page-addressed lists, same WAL as everything else."
"ADR 0012 documents the graph page types and the graph_root_page header field bumping format_version to 3; FORMAT.md §12 has the byte-level spec."
"S9 hybrid recall ships Reciprocal Rank Fusion at k=60 fusing HNSW vector search with our own paged BM25 full-text index; property tests pin the union-never-intersection invariant."
"S10 adds metadata filters on recall: exact match key=value, open bounds key>=n/key<=n, and closed ranges key=lo..hi, all ANDed together."
"S11 vacuum rebuilds the file by copy plus an atomic Vfs::rename swap, crash-safe under a kill-point sweep; forget stays a soft-delete tombstone until vacuum reclaims space."
"S14 basic provenance: recall gained an --agent filter and stats gained a per-agent breakdown showing live memory counts and session counts per writing agent."
"B5 shipped Python bindings via PyO3 and maturin, with a pytest E2E suite doing Rust<->Python round-trip checks; CI builds wheels for three platforms."
"Benchmark honesty commitment paid off: sqlite-vec beats us on recall@10 and file size, zvec beats us ~10x on warm query latency, and both numbers are published in the README next to our own."
"The HNSW index lives directly in file pages — a node's identifier is its physical page number, no node_id-to-page lookup table, because the table capped at ~405 nodes per 4 KiB page and rewrote whole on every insert (ADR 0008)."
"Recovery from the physical page WAL is dumb by design: scan frames from the start, validate checksums, apply only transactions with a valid commit frame, truncate a torn tail. One behavior to verify, verified brutally by a fault-injecting VFS."
"build.rs now re-fetches the ONNX embedding model at build time with checksum verification instead of vendoring it, shrinking the crates.io package to about 452 KiB to clear the 10 MiB cap."
"CI had three red workflows on main fixed in one pass: the manylinux compatibility tag had to be the bare tag string, not an image reference, and ort was switched to the rustls TLS backend to drop the openssl-sys build dependency on Windows runners."
"The crash_sweep_vacuum test was flaky because its dry-run range sometimes had incomplete coverage; the fix retries that range instead of treating a single miss as a hard failure."
"tools/go-no-go-report.sh computes the PRD §4 metrics table (stars, third-party issues, external PRs, recurring downloads) against the documented thresholds and prints the decision rule outcome on demand."
"The RRF fusion degrades gracefully and visibly: a .mind file written before the full-text index existed still recalls, vector-only, but the CLI warns on stderr and the MCP response carries a warning field."
"Direct page addressing for HNSW cost 8-byte neighbor references instead of 4, and any operation that relocates node pages forces an index rebuild — which vacuum already does by design since deletes are tombstones."
)

echo "== remember (${#memories[@]} calls) =="
for m in "${memories[@]}"; do
    "$bin" remember "$m" --file "$file" --project embedmind >/dev/null
done

echo "== recall (5 sample queries) =="
queries=(
"how does the WAL recovery work"
"why did we drop the node_id lookup table for HNSW"
"what fixed the CI red workflows"
"how does vacuum reclaim space safely"
"how is hybrid search fused"
)
for q in "${queries[@]}"; do
    echo "--- $q ---"
    "$bin" recall "$q" --file "$file" --project embedmind --limit 3
done

echo "== stats =="
"$bin" stats --file "$file"
