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
//! Part 2 (this crate's completed scope) adds the rest:
//!
//! - [`metrics`] — latency percentiles (p50/p99, nearest-rank) and throughput.
//! - [`sysmem`] — peak-RSS sampling across a measured phase.
//! - [`harness`] — the full metric suite over one dataset (warm + cold-open
//!   latency, ingest throughput, file size, RSS, recall).
//! - [`competitors`] — the pinned sqlite-vec/zvec registry and comparison
//!   adapters (feature-gated; honest "not measured" when a toolchain is absent).
//! - [`report`] — NFR validation and the README-ready markdown + JSON renderers.
//! - [`regression`] — baseline comparison for the CI regression guard
//!   (BENCHMARKS.md §5 thresholds; spec S15).
//!
//! Binaries: `gen_dataset <name>` materializes a dataset; `baseline <name>`
//! runs the brute-force recall@10 reference over it; `run_all` runs the full
//! suite end-to-end and emits the results table (see `benches/run_all.sh`);
//! `compare_baseline <baseline.json> <current.json>` runs just the §5
//! regression comparison between two results files.

pub mod baseline;
pub mod competitors;
pub mod corpus;
pub mod dataset;
pub mod harness;
pub mod metrics;
pub mod recall;
pub mod regression;
pub mod report;
pub mod sysmem;

use std::path::{Path, PathBuf};

/// Default directory for materialized datasets, relative to the repo root
/// (`benches/data`). Git-ignored — these are large build products regenerated
/// from the committed specs, never committed themselves.
pub fn default_data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("data")
}
