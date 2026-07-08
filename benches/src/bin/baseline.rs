//! Brute-force baseline reference run (`docs/BENCHMARKS.md` §1/§3): computes
//! recall@10 of the HNSW index against the exact linear scan over a
//! materialized dataset.
//!
//! ```text
//! # materialize first (or pass --generate to do it here):
//! cargo run -p embedmind-bench --release --bin gen_dataset -- agent-mem-10k
//! cargo run -p embedmind-bench --release --bin baseline    -- agent-mem-10k
//!
//! # or in one shot:
//! cargo run -p embedmind-bench --release --bin baseline -- agent-mem-10k --generate
//! ```
//!
//! It loads the `.vec` sidecar (the exact vectors the store was built from),
//! opens the `.mind` store, and reports recall@10 over a fixed query set. This
//! is the reference number Part 2's full results table will sit alongside p50/
//! p99 latency and the sqlite-vec/zvec rows.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use embedmind_bench::dataset::{self, DATASETS, DatasetSpec, VectorSet};
use embedmind_bench::{default_data_dir, recall};
use embedmind_core::api::{Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::storage::vfs::RealVfs;

/// recall@k the reference run reports (`docs/BENCHMARKS.md` §3).
const K: usize = 10;

/// Fixed query-set size — enough to average out per-query noise, small enough
/// that the brute-force scan over 100k stays a few seconds.
const QUERIES: usize = 200;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("baseline failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(name) = args.next() else {
        eprintln!("usage: baseline <dataset> [--generate]");
        eprintln!("available datasets:");
        for d in DATASETS {
            eprintln!("  {} ({} memories)", d.name, d.count);
        }
        return Ok(ExitCode::FAILURE);
    };
    let generate = args.any(|a| a == "--generate");

    let Some(spec) = DatasetSpec::by_name(&name) else {
        eprintln!("unknown dataset '{name}'");
        return Ok(ExitCode::FAILURE);
    };

    let data_dir = default_data_dir();
    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::load()?);

    // Materialize on demand, or load the committed-spec's vectors from disk.
    let set = load_or_materialize(spec, &data_dir, embedder.as_ref(), generate)?;

    // Open the store the vectors were built into, reusing the same embedder so
    // `recall` embeds queries identically to how this harness does.
    let opts = StoreOptions {
        embedder: Some(Arc::clone(&embedder)),
        ..StoreOptions::default()
    };
    let store = Store::open_with(Arc::new(RealVfs), &spec.mind_path(&data_dir), opts)?;

    let queries = recall::query_texts(spec, QUERIES);
    println!(
        "measuring recall@{K} of {} ({} vectors) over {} queries...",
        spec.name,
        set.entries.len(),
        queries.len()
    );
    let started = std::time::Instant::now();
    let report = recall::measure(&store, &set, embedder.as_ref(), &queries, K)?;

    println!("--- recall@{K} vs brute-force ({}) ---", spec.name);
    println!(
        "  dataset:      {} ({} vectors)",
        spec.name,
        set.entries.len()
    );
    println!("  queries:      {}", report.queries);
    println!("  recall@{}:    {:.4}", report.k, report.recall_at_k);
    println!("  min recall:   {:.4}", report.min_recall);
    println!("  wall time:    {:.1}s", started.elapsed().as_secs_f64());
    Ok(ExitCode::SUCCESS)
}

/// Loads the `.vec` sidecar, or (re)materializes the dataset first when asked
/// to or when no valid sidecar exists yet.
fn load_or_materialize(
    spec: &DatasetSpec,
    data_dir: &Path,
    embedder: &dyn Embedder,
    force: bool,
) -> Result<VectorSet, Box<dyn std::error::Error>> {
    let vec_path = spec.vec_path(data_dir);
    if !force && vec_path.exists() {
        match dataset::load_vec_file(spec, &vec_path, embedder.dims(), embedder.id()) {
            Ok(set) => {
                println!(
                    "loaded {} vectors from {}",
                    set.entries.len(),
                    vec_path.display()
                );
                return Ok(set);
            }
            Err(e) => {
                eprintln!("cached {} unusable ({e}); regenerating", vec_path.display());
            }
        }
    }
    println!(
        "materializing {} (this embeds every memory once)...",
        spec.name
    );
    Ok(dataset::materialize(spec, data_dir)?)
}
