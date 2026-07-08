# embedmind-bench

The benchmark harness for `embedmind-core`, implementing the methodology in
[`docs/BENCHMARKS.md`](../docs/BENCHMARKS.md). Not published to crates.io — it
holds no product logic, only measurement.

This is **Part 1** (M1 item 1.7): the reproducible foundation. Part 2 adds the
remaining metrics (latency p50/p99, ingest throughput, file size, RSS,
cold-open), the sqlite-vec / zvec comparisons, the results-table renderer, and
the CI regression guard.

## What's here

| Module | Role |
|---|---|
| `corpus` | Deterministic synthetic agent-memory text: `(seed, count) → corpus`. Bilingual pt-BR + en, categories decisions/facts/preferences/code-notes (BENCHMARKS.md §2). (Named `corpus`, not `gen` — `gen` is a reserved keyword in edition 2024.) |
| `dataset` | The committed dataset **specs** (`agent-mem-10k`, `agent-mem-100k`) and their materialization into vectors + a `.mind` store through the shipped ONNX model. |
| `baseline` | Brute-force exact top-k: the recall ceiling and latency floor everything else is graded against. |
| `recall` | recall@k of the HNSW index vs. the brute-force baseline (set overlap, since HNSW is approximate). |

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
```

The fast end-to-end smoke test (small in-memory run, real model + real index)
lives in `tests/harness.rs` and runs under `cargo test --workspace`.
