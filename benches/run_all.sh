#!/usr/bin/env bash
# Full EmbedMind benchmark suite (docs/BENCHMARKS.md §3/§4).
#
# Runs the whole harness end-to-end: materializes the committed datasets from
# their seeds (corpus -> ONNX embeddings -> .mind store + .vec baseline), then
# measures recall@10, warm + cold-open query latency (p50/p99), remember
# latency, ingest throughput, file size and peak RSS; compares against the
# pinned sqlite-vec / zvec baselines; validates the spec NFRs; and writes a
# README-ready markdown table plus benches/results/<version>.json.
#
# Usage:
#   ./benches/run_all.sh                 # fast: the 10k set only
#   ./benches/run_all.sh --full          # both 10k and 100k (minutes of CPU)
#   ./benches/run_all.sh agent-mem-100k  # a specific dataset
#   BENCH_DATE=2026-07-08 ./benches/run_all.sh   # stamp the run date
#
# Competitor columns: pass the feature to enable an adapter on a box that has
# the native toolchain, e.g.
#   COMPARE="--features compare-sqlite-vec" ./benches/run_all.sh
#
# Exit code is non-zero if any applicable NFR was missed (so this doubles as the
# CI performance guard, BENCHMARKS.md §5).

set -euo pipefail

# Repo root = parent of this script's dir, so the script works from anywhere.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

# Date stamped into the results header/filename if the caller didn't set one.
export BENCH_DATE="${BENCH_DATE:-$(date -u +%Y-%m-%d)}"

# Dataset selection.
DATASETS=()
COMPARE="${COMPARE:-}"
for arg in "$@"; do
  case "$arg" in
    --full) DATASETS=(agent-mem-10k agent-mem-100k) ;;
    *)      DATASETS+=("$arg") ;;
  esac
done

echo ">> EmbedMind benchmark suite"
echo ">> date=${BENCH_DATE}  datasets=${DATASETS[*]:-agent-mem-10k (default)}  features='${COMPARE}'"

# --release is mandatory: LTO/opt matters for honest latency numbers, and the
# 100k embedding pass is impractically slow in debug.
# shellcheck disable=SC2086
cargo run -p embedmind-bench --release ${COMPARE} --bin run_all -- "${DATASETS[@]}"
