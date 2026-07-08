//! Full benchmark suite end-to-end (`docs/BENCHMARKS.md` §3/§4) — the Part 2
//! done-criterion: "runs end-to-end and produces the markdown table; competitor
//! versions recorded; NFRs measured and reported".
//!
//! ```text
//! # default (fast: the 10k set), materializing if needed:
//! cargo run -p embedmind-bench --release --bin run_all
//!
//! # both committed sizes (the honest side-by-side, BENCHMARKS.md §4 rule 2):
//! cargo run -p embedmind-bench --release --bin run_all -- agent-mem-10k agent-mem-100k
//!
//! # with a competitor toolchain present:
//! cargo run -p embedmind-bench --release --features compare-sqlite-vec --bin run_all
//! ```
//!
//! It materializes (or loads) each requested dataset, runs [`harness::run_suite`]
//! over it, runs the pinned competitors ([`competitors::run_all`]) on the same
//! vectors/queries, then renders the README-ready markdown table and a JSON
//! results file into `benches/results/`. Exit code is non-zero if any
//! **applicable** NFR was missed, so the same binary doubles as the CI guard.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use embedmind_bench::dataset::{self, DATASETS, DatasetSpec, VectorSet};
use embedmind_bench::harness::SuiteResult;
use embedmind_bench::report::{self, RunEnv};
use embedmind_bench::{competitors, default_data_dir, harness};
use embedmind_core::api::{Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::storage::vfs::RealVfs;

/// Warm-latency / recall query-set size (BENCHMARKS.md §3 asks for ~1k; kept
/// modest so the brute-force recall scan over 100k stays a few seconds).
const WARM_QUERIES: usize = 1000;

/// One-at-a-time `remember` samples for the ingest/latency phase — enough for a
/// stable p99 without re-embedding the whole corpus a second time.
const REMEMBER_SAMPLES: usize = 500;

/// The run date, stamped into the results header/filename (BENCHMARKS.md §3:
/// "every results table states … date"). Overridable via `BENCH_DATE` so a CI
/// job can pin it; falls back to a build-time constant otherwise.
fn run_date() -> String {
    std::env::var("BENCH_DATE").unwrap_or_else(|_| "unknown-date".to_string())
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("run_all failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let names: Vec<String> = std::env::args().skip(1).collect();
    let specs: Vec<&'static DatasetSpec> = if names.is_empty() {
        // Default: the fast 10k set. The 100k set is opt-in (minutes of CPU).
        vec![DatasetSpec::by_name("agent-mem-10k").ok_or("missing agent-mem-10k spec")?]
    } else {
        let mut v = Vec::new();
        for n in &names {
            match DatasetSpec::by_name(n) {
                Some(s) => v.push(s),
                None => {
                    eprintln!("unknown dataset '{n}'. available:");
                    for d in DATASETS {
                        eprintln!("  {} ({} memories)", d.name, d.count);
                    }
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        v
    };

    let data_dir = default_data_dir();
    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::load()?);

    let mut results: Vec<SuiteResult> = Vec::new();
    let mut competitor_outcomes = Vec::new();

    for spec in &specs {
        println!("=== {} ({} memories) ===", spec.name, spec.count);
        let set = load_or_materialize(spec, &data_dir, embedder.as_ref())?;

        let opts = StoreOptions {
            embedder: Some(Arc::clone(&embedder)),
            ..StoreOptions::default()
        };
        let store = Store::open_with(Arc::new(RealVfs), &spec.mind_path(&data_dir), opts)?;

        println!("  running metric suite ({WARM_QUERIES} queries)...");
        let started = std::time::Instant::now();
        let result = harness::run_suite(
            spec,
            &data_dir,
            store,
            &set,
            &embedder,
            WARM_QUERIES,
            REMEMBER_SAMPLES,
        )?;
        println!(
            "  done in {:.1}s: recall@10 {:.4}, query p99 {:.2} ms, remember p99 {:.2} ms",
            started.elapsed().as_secs_f64(),
            result.recall.recall_at_k,
            result.query_p99_ms,
            result.remember_p99_ms,
        );

        // Competitors run on the biggest dataset (its query vectors), so the
        // comparison table is against the hardest workload we have here.
        if competitor_outcomes.is_empty()
            || spec.count == specs.iter().map(|s| s.count).max().unwrap_or(0)
        {
            competitor_outcomes = competitors::run_all(&set, &result.query_vectors, harness::K);
        }
        results.push(result);
    }

    // --- render + persist ---
    let env = RunEnv::capture(run_date());
    let markdown = report::render_markdown(&env, &results, &competitor_outcomes);
    let json = report::render_json(&env, &results, &competitor_outcomes);

    let results_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    std::fs::create_dir_all(&results_dir)?;
    let json_path = results_dir.join(format!("{}.json", env.embedmind_version));
    let md_path = results_dir.join("latest.md");
    std::fs::write(&json_path, &json)?;
    std::fs::write(&md_path, &markdown)?;

    println!("\n{markdown}");
    println!("wrote {}", json_path.display());
    println!("wrote {}", md_path.display());

    // Exit non-zero if any applicable NFR missed — makes this the CI guard too.
    let checks = report::check_nfrs(&results);
    let missed: Vec<_> = checks
        .iter()
        .filter(|c| !c.passed && !c.not_applicable)
        .collect();
    if missed.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("\n{} NFR(s) missed (reported above):", missed.len());
        for c in missed {
            eprintln!(
                "  - {}: target {}, measured {}",
                c.name, c.target, c.measured
            );
        }
        Ok(ExitCode::FAILURE)
    }
}

/// Loads the `.vec` sidecar (and thus assumes the `.mind` exists), or
/// materializes the dataset fresh when no valid sidecar is present.
fn load_or_materialize(
    spec: &DatasetSpec,
    data_dir: &Path,
    embedder: &dyn Embedder,
) -> Result<VectorSet, Box<dyn std::error::Error>> {
    let vec_path = spec.vec_path(data_dir);
    let mind_path = spec.mind_path(data_dir);
    if vec_path.exists() && mind_path.exists() {
        match dataset::load_vec_file(spec, &vec_path, embedder.dims(), embedder.id()) {
            Ok(set) => {
                println!("  loaded {} cached vectors", set.entries.len());
                return Ok(set);
            }
            Err(e) => eprintln!("  cached vectors unusable ({e}); regenerating"),
        }
    }
    println!(
        "  materializing {} (embeds every memory once)...",
        spec.name
    );
    Ok(dataset::materialize(spec, data_dir)?)
}
