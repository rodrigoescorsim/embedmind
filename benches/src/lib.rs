//! # embedmind-bench
//!
//! The benchmark harness for `embedmind-core` (`docs/BENCHMARKS.md`). This crate
//! is **not published** and holds no product logic — it exists to measure the
//! engine honestly and to guard against regressions (BENCHMARKS.md §5).
//!
//! Part 1 (this crate's current scope, M1 item 1.7) lays the foundation:
//!
//! - [`corpus`] — deterministic synthetic agent-memory text (seed → corpus).
//! - [`dataset`] — the committed dataset specs (`agent-mem-10k`/`-100k`) and
//!   their vector materialization through the shipped ONNX model.
//! - [`baseline`] — brute-force exact top-k: the recall ceiling and latency
//!   floor every other system is graded against.
//! - [`recall`] — recall@k of the HNSW index vs. that baseline (set overlap,
//!   since HNSW is approximate).
//!
//! Part 2 will add the remaining metrics (p50/p99 latency, ingest throughput,
//! file size, RSS, cold-open) and the sqlite-vec/zvec comparisons, plus the
//! results-table renderer and CI regression guard.
//!
//! Binaries: `gen_dataset <name>` materializes a dataset; `baseline <name>`
//! runs the brute-force recall@10 reference over it.

pub mod baseline;
pub mod corpus;
pub mod dataset;
pub mod recall;

use std::path::{Path, PathBuf};

/// Default directory for materialized datasets, relative to the repo root
/// (`benches/data`). Git-ignored — these are large build products regenerated
/// from the committed specs, never committed themselves.
pub fn default_data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("data")
}
