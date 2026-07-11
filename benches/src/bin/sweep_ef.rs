//! `ef_search` sweep over the committed datasets — the measurement behind the
//! adaptive-`ef_search` decision (story S16 / task BQ1; `docs/adr/0013`).
//!
//! `HNSW_DEFAULT_EF_SEARCH` is a fixed 64 today, and the 2026-07-09 run showed
//! it does not scale: recall@10 @ 100k fell to 0.9313 mean / 0.20 worst query
//! while query p99 sat at 15.5 ms against a 50 ms ceiling — there is latency
//! budget to buy recall with. This binary produces the data to decide *how
//! much*: for each candidate `ef_search` it measures, per dataset,
//!
//! - the recall@10 distribution vs. the brute-force baseline (mean, min, p10,
//!   p50 — the same worst-query tail the harness will report),
//! - warm latency of the pure vector path (`recall_vector`, what `ef` acts on)
//!   and of the end-to-end hybrid `recall` (the NFR's p99), p50/p99 each.
//!
//! The exact top-k per query is computed **once** per dataset and reused for
//! every `ef` candidate, so a sweep over the 100k set stays minutes, not hours.
//!
//! ```text
//! # both committed sizes, default ef ladder:
//! cargo run -p embedmind-bench --release --bin sweep_ef -- agent-mem-10k agent-mem-100k
//!
//! # custom ladder / query count:
//! EF_LIST=64,128,256 SWEEP_QUERIES=500 cargo run -p embedmind-bench --release --bin sweep_ef
//! ```
//!
//! Decision-only tooling: nothing here changes engine behavior. The chosen
//! formula lands separately (BQ1 implementation) with this sweep's numbers
//! recorded in the ADR.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use embedmind_bench::dataset::{self, DATASETS, DatasetSpec, VectorSet};
use embedmind_bench::metrics::Latencies;
use embedmind_bench::{baseline, default_data_dir, recall};
use embedmind_core::api::{Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::index::normalize;
use embedmind_core::storage::vfs::RealVfs;
use ulid::Ulid;

/// Same k the harness reports (`docs/BENCHMARKS.md` §3).
const K: usize = 10;

/// Default `ef_search` ladder. Starts at the current fixed default (64, the
/// status-quo row) and roughly doubles; 512 is far past where recall should
/// saturate at 100k, so the knee of the curve falls inside the ladder.
const DEFAULT_EF_LADDER: &[u16] = &[64, 96, 128, 192, 256, 384, 512];

/// Same query-set size as the harness's warm/recall phase, so the ef=64 row
/// reproduces the committed baseline numbers (same seeds, same texts).
const DEFAULT_QUERIES: usize = 1000;

/// Stderr heartbeat cadence inside the per-ef query loops. A full sweep runs
/// for tens of minutes; with stdout fully buffered under redirection, silence
/// is indistinguishable from a hang to whoever is watching the log files.
const PROGRESS_EVERY: usize = 250;

/// Rust's stdout is fully buffered when redirected to a file, so a sweep row
/// would only reach the log when ~8 KiB accumulate — flush after every line
/// that carries a result.
fn flush_stdout() {
    let _ = std::io::stdout().flush();
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sweep_ef failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let names: Vec<String> = std::env::args().skip(1).collect();
    let specs: Vec<&'static DatasetSpec> = if names.is_empty() {
        DATASETS.iter().collect()
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

    let efs: Vec<u16> = match std::env::var("EF_LIST") {
        Ok(list) if !list.is_empty() => {
            let mut v = Vec::new();
            for part in list.split(',') {
                v.push(part.trim().parse::<u16>().map_err(|e| {
                    format!("bad EF_LIST entry '{part}': {e} (expected u16 values)")
                })?);
            }
            v
        }
        _ => DEFAULT_EF_LADDER.to_vec(),
    };
    let queries: usize = match std::env::var("SWEEP_QUERIES") {
        Ok(n) if !n.is_empty() => n
            .parse()
            .map_err(|e| format!("bad SWEEP_QUERIES '{n}': {e}"))?,
        _ => DEFAULT_QUERIES,
    };

    let data_dir = default_data_dir();
    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::load()?);

    println!("# ef_search sweep — k={K}, {queries} queries per dataset");
    println!();
    for spec in &specs {
        sweep_dataset(spec, &data_dir, &embedder, &efs, queries)?;
        println!();
    }
    Ok(ExitCode::SUCCESS)
}

/// Sweeps every `ef` candidate over one dataset and prints one markdown table.
fn sweep_dataset(
    spec: &DatasetSpec,
    data_dir: &Path,
    embedder: &Arc<dyn Embedder>,
    efs: &[u16],
    queries: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("## {} ({} memories)", spec.name, spec.count);
    let set = load_or_materialize(spec, data_dir, embedder.as_ref())?;

    let opts = StoreOptions {
        embedder: Some(Arc::clone(embedder)),
        ..StoreOptions::default()
    };
    let store = Store::open_with(Arc::new(RealVfs), &spec.mind_path(data_dir), opts)?;

    // Exact top-k per query, computed once and shared by every ef candidate —
    // the brute-force pass over 100k vectors is the sweep's fixed cost.
    let texts = recall::query_texts(spec, queries);
    eprintln!(
        "  [{}] embedding {queries} queries + brute-force exact top-{K}...",
        spec.name
    );
    let started = Instant::now();
    let mut exact_sets: Vec<HashSet<Ulid>> = Vec::with_capacity(texts.len());
    for (i, t) in texts.iter().enumerate() {
        let mut qv = embedder.embed(t)?;
        normalize(&mut qv);
        let exact: HashSet<Ulid> = baseline::top_k(&set, &qv, K, |_| true)
            .into_iter()
            .map(|h| h.record_id)
            .collect();
        exact_sets.push(exact);
        if (i + 1) % PROGRESS_EVERY == 0 {
            eprintln!("  [{}] baseline {}/{}", spec.name, i + 1, texts.len());
        }
    }
    eprintln!(
        "  [{}] baseline ready in {:.1}s",
        spec.name,
        started.elapsed().as_secs_f64()
    );

    println!(
        "| ef_search | recall@10 mean | min | p10 | p50 | vec p50 ms | vec p99 ms | hybrid p50 ms | hybrid p99 ms |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    flush_stdout();
    for &ef in efs {
        let ef_started = Instant::now();
        let row = sweep_one_ef(&store, &texts, &exact_sets, ef)?;
        println!(
            "| {ef} | {:.4} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
            row.mean,
            row.min,
            row.p10,
            row.p50,
            row.vec_p50_ms,
            row.vec_p99_ms,
            row.hybrid_p50_ms,
            row.hybrid_p99_ms,
        );
        flush_stdout();
        eprintln!(
            "  [{}] ef={ef} done in {:.1}s",
            spec.name,
            ef_started.elapsed().as_secs_f64()
        );
    }
    store.close()?;
    Ok(())
}

/// One sweep row: the recall@10 distribution and both latency profiles at a
/// fixed `ef_search`.
struct EfRow {
    mean: f64,
    min: f64,
    p10: f64,
    p50: f64,
    vec_p50_ms: f64,
    vec_p99_ms: f64,
    hybrid_p50_ms: f64,
    hybrid_p99_ms: f64,
}

fn sweep_one_ef(
    store: &Store,
    texts: &[String],
    exact_sets: &[HashSet<Ulid>],
    ef: u16,
) -> embedmind_core::Result<EfRow> {
    // --- recall distribution on the pure vector path (what ef acts on),
    //     timing each call as it goes (recall_vector embeds internally, so
    //     this latency is text→ids end-to-end like the harness's) ---
    let mut recalls: Vec<f64> = Vec::with_capacity(texts.len());
    let mut vec_lat = Latencies::with_capacity(texts.len());
    for (i, (t, exact)) in texts.iter().zip(exact_sets).enumerate() {
        let started = Instant::now();
        let hits = store.recall_vector(Query::new(t.clone()).limit(K).ef_search(ef))?;
        vec_lat.push(started.elapsed());
        let approx: HashSet<Ulid> = hits.into_iter().map(|r| r.id).collect();
        let denom = exact.len().max(1);
        recalls.push(exact.intersection(&approx).count() as f64 / denom as f64);
        if (i + 1) % PROGRESS_EVERY == 0 {
            eprintln!("    ef={ef}: vec {}/{}", i + 1, texts.len());
        }
    }
    recalls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = recalls.iter().sum::<f64>() / recalls.len().max(1) as f64;

    // --- warm end-to-end hybrid latency at this ef (the NFR's metric) ---
    for t in texts.iter().take(texts.len().min(32)) {
        let _ = store.recall(Query::new(t.clone()).limit(K).ef_search(ef))?;
    }
    let mut hybrid_lat = Latencies::with_capacity(texts.len());
    for (i, t) in texts.iter().enumerate() {
        let started = Instant::now();
        let _ = store.recall(Query::new(t.clone()).limit(K).ef_search(ef))?;
        hybrid_lat.push(started.elapsed());
        if (i + 1) % PROGRESS_EVERY == 0 {
            eprintln!("    ef={ef}: hybrid {}/{}", i + 1, texts.len());
        }
    }

    Ok(EfRow {
        mean,
        min: recalls.first().copied().unwrap_or(0.0),
        p10: sorted_percentile(&recalls, 10.0),
        p50: sorted_percentile(&recalls, 50.0),
        vec_p50_ms: vec_lat.p50_ms().unwrap_or(0.0),
        vec_p99_ms: vec_lat.p99_ms().unwrap_or(0.0),
        hybrid_p50_ms: hybrid_lat.p50_ms().unwrap_or(0.0),
        hybrid_p99_ms: hybrid_lat.p99_ms().unwrap_or(0.0),
    })
}

/// Nearest-rank percentile over an already-sorted slice (same method as
/// [`Latencies::percentile_ms`] — the reported value always occurred).
fn sorted_percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Loads the `.vec` sidecar (and thus assumes the `.mind` exists), or
/// materializes the dataset fresh when no valid sidecar is present — same
/// logic as `run_all`.
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
                eprintln!(
                    "  [{}] loaded {} cached vectors",
                    spec.name,
                    set.entries.len()
                );
                return Ok(set);
            }
            Err(e) => eprintln!(
                "  [{}] cached vectors unusable ({e}); regenerating",
                spec.name
            ),
        }
    }
    eprintln!(
        "  [{}] materializing (embeds every memory once)...",
        spec.name
    );
    Ok(dataset::materialize(spec, data_dir)?)
}
