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
# The competitor comparison runs on one shared dataset — by default the largest
# in the run. Building a competitor index (e.g. zvec's HNSW) and re-deriving an
# exact brute-force top-k per query is many minutes on the 100k set, so pin the
# comparison to the cheaper 10k set while still producing the full EmbedMind
# 10k+100k table (NFRs measured) with COMPARE_DATASET:
#   COMPARE="--features compare-sqlite-vec,compare-zvec" \
#     COMPARE_DATASET=agent-mem-10k ./benches/run_all.sh --full
#
# Regression guard (BENCHMARKS.md §5): point BASELINE at a previous results
# JSON to also fail on any metric that regressed beyond the §5 thresholds:
#   BASELINE=benches/results/0.1.0-dev.json ./benches/run_all.sh
# (Latency/RSS checks degrade to warnings when the baseline came from another
# platform; recall@10 and file size are enforced regardless.)
#
# Exit code is non-zero if any applicable NFR was missed, or if BASELINE was
# given and a §5 threshold was crossed (so this doubles as the CI performance
# guard — see .github/workflows/bench.yml).

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

# zvec links against a dynamic libzvec_c_api that its build script places in
# the build's OUT_DIR. Linux/macOS embed an rpath to it; Windows has no rpath,
# so the DLL's directory must be on PATH at run time. Locate the newest one and
# prepend it — harmless on the other platforms and when the feature is off.
if [[ "${COMPARE}" == *compare-zvec* ]]; then
  ZVEC_DLL_DIR="$(ls -td target/release/build/zvec-rust-sys-*/out/zvec-prebuilt 2>/dev/null | head -1 || true)"
  if [[ -n "${ZVEC_DLL_DIR}" ]]; then
    ZVEC_DLL_DIR="$(cd "${ZVEC_DLL_DIR}" && pwd)"
    export PATH="${ZVEC_DLL_DIR}:${PATH}"
    export LD_LIBRARY_PATH="${ZVEC_DLL_DIR}${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"
    export DYLD_LIBRARY_PATH="${ZVEC_DLL_DIR}${DYLD_LIBRARY_PATH:+:${DYLD_LIBRARY_PATH}}"
    echo ">> zvec native lib: ${ZVEC_DLL_DIR}"
  fi
fi

# --release is mandatory: LTO/opt matters for honest latency numbers, and the
# 100k embedding pass is impractically slow in debug. The `+"${..}"` guard makes
# an empty dataset array safe under `set -u` (run_all then uses its own default).
# shellcheck disable=SC2086
cargo run -p embedmind-bench --release ${COMPARE} --bin run_all -- \
  "${DATASETS[@]+"${DATASETS[@]}"}"
