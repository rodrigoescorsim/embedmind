# embedmind-bench

The benchmark harness for `embedmind-core`, implementing the methodology in
[`docs/BENCHMARKS.md`](../docs/BENCHMARKS.md). Not published to crates.io — it
holds no product logic, only measurement.

Both parts of M1 item 1.7 are now here: Part 1 is the reproducible foundation
(committed dataset specs, brute-force baseline, recall@10); Part 2 adds the full
metric suite (latency p50/p99 warm + cold-open, ingest throughput, file size,
peak RSS), the pinned sqlite-vec / zvec comparison, the results-table renderer,
and the CI regression guard.

## What's here

| Module | Role |
|---|---|
| `corpus` | Deterministic synthetic agent-memory text: `(seed, count) → corpus`. Bilingual pt-BR + en, categories decisions/facts/preferences/code-notes (BENCHMARKS.md §2). (Named `corpus`, not `gen` — `gen` is a reserved keyword in edition 2024.) |
| `dataset` | The committed dataset **specs** (`agent-mem-10k`, `agent-mem-100k`) and their materialization into vectors + a `.mind` store through the shipped ONNX model. |
| `baseline` | Brute-force exact top-k: the recall ceiling and latency floor everything else is graded against. |
| `recall` | recall@k of the HNSW index vs. the brute-force baseline (set overlap, since HNSW is approximate). |
| `metrics` | Latency percentiles (nearest-rank p50/p99) and throughput. |
| `sysmem` | Peak-RSS sampling across a measured phase (pinned `sysinfo`, no `unsafe`). |
| `harness` | `run_suite`: the full metric suite over one dataset. |
| `competitors` | Pinned+recorded sqlite-vec/zvec registry and feature-gated **real adapters** (statically-compiled `vec0` via rusqlite; official `zvec-rust` binding); honest "not measured" when a feature is off. |
| `report` | Spec-NFR validation + README-ready markdown / results JSON renderers. |

## Why datasets are "committed" without committing gigabytes

A dataset is committed as a tiny spec — a name, a **fixed seed**, and a memory
count (`dataset::DATASETS`, version-controlled). Two deterministic stages
reproduce the full data anywhere:

1. `corpus::generate(seed, count)` yields byte-identical text on every platform.
2. The embedded ONNX model (CPU-only, no network) turns that text into
   byte-identical vectors — the **same embeddings fed to every benchmarked
   system**, the methodology's core rule.

So `agent-mem-100k` is ~150 MB of vectors that never enter the repo; they are
regenerated on demand into `benches/data/` (git-ignored). The `10k` set shares
the `100k` seed, so it is a genuine prefix, never a different distribution.

## Running

```bash
# List datasets:
cargo run -p embedmind-bench --bin gen_dataset

# Materialize one (embeds every memory once — use --release for 100k):
cargo run -p embedmind-bench --release --bin gen_dataset -- agent-mem-10k

# Brute-force recall@10 reference over it (--generate materializes first):
cargo run -p embedmind-bench --release --bin baseline -- agent-mem-10k --generate

# Full suite (all metrics + competitor comparison + markdown table + NFRs):
./benches/run_all.sh            # fast: the 10k set
./benches/run_all.sh --full     # both 10k and 100k (minutes of CPU)

# Equivalent direct invocation (--release is mandatory for honest numbers):
cargo run -p embedmind-bench --release --bin run_all -- agent-mem-10k agent-mem-100k

# With the competitor adapters enabled (fills the comparison rows for real):
COMPARE="--features compare-sqlite-vec,compare-zvec" ./benches/run_all.sh

# Full EmbedMind table (10k+100k, NFRs) but pin the (expensive) competitor
# comparison to the cheaper 10k set — building zvec's HNSW and re-deriving an
# exact top-k per query is many minutes on 100k:
COMPARE="--features compare-sqlite-vec,compare-zvec" \
  COMPARE_DATASET=agent-mem-10k ./benches/run_all.sh --full
```

On Windows the `compare-*` adapters compile native C/FFI, so run from a shell
with the MSVC toolchain on `PATH` (a "x64 Native Tools" prompt, or after sourcing
`vcvars64.bat`) — otherwise `cc`/the linker cannot find `cl.exe`.

The `compare-*` features are **off by default** and their build scripts are the
only place the harness touches the network (fetching the pinned, SHA-256-verified
`sqlite-vec.c` and zvec's prebuilt native library, respectively — see
`benches/build.rs` and `Cargo.toml`). The engine itself stays network-free; these
are bench-only, opt-in build steps, never runtime calls.

`run_all` writes `benches/results/<version>.json` and `benches/results/latest.md`
(git-ignored dev output; a per-version JSON is force-added only when a release is
cut, BENCHMARKS.md §4 rule 3) and exits non-zero if any **applicable** NFR was
missed — so it doubles as the CI performance guard.

The fast end-to-end smoke test (small in-memory run, real model + real index)
lives in `tests/harness.rs` and runs under `cargo test --workspace`.
