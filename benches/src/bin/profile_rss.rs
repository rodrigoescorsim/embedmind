//! Phase-by-phase RSS profiling of the benchmark process (S28 / task FT5).
//!
//! The suite's peak-RSS numbers (`harness::run_suite`) are process-wide, so
//! they include everything resident when the measured phase runs — the engine
//! *and* the harness's own apparatus (the brute-force [`VectorSet`], the ONNX
//! session). At 100k the NFR headroom is ~2%, which makes "who owns the
//! bytes?" the whole question. This binary answers it with measurements, not
//! code reading (same method rule as S24's `profile_fts`): it walks the exact
//! resident-set lifecycle of a suite run and prints RSS at every phase
//! boundary, plus the peak observed inside each measured phase.
//!
//! ```text
//! cargo run -p embedmind-bench --release --bin profile_rss -- agent-mem-100k
//! ```
//!
//! Phases (deltas between consecutive lines attribute the bytes):
//!
//! 1. process start
//! 2. ONNX embedder loaded              — product-side cost (ships in the MCP server)
//! 3. `.vec` VectorSet loaded           — harness-only (brute-force baseline data)
//! 4. store opened                      — engine handle (pager, no page cache)
//! 5. recall measurement (peak)         — brute-force top-k vs `recall_vector`
//! 6. warm hybrid+vector queries (peak) — the phase `peak_rss_query_mib` measures
//! 7. VectorSet dropped                 — does the allocator return the bytes?
//! 8. warm queries again (peak)         — engine+ONNX only: the product-shaped number
//! 9. scratch ingest (peak)             — the phase `peak_rss_ingest_mib` measures
//!
//! Query/sample counts are trimmed-down defaults (RSS stabilizes within a few
//! operations because the big allocations are per-phase, not cumulative) and
//! overridable via `RSS_RECALL_QUERIES` / `RSS_QUERIES` / `RSS_INGEST`.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use embedmind_bench::dataset::{self, DatasetSpec, VectorSet};
use embedmind_bench::sysmem::RssSampler;
use embedmind_bench::{baseline, corpus, default_data_dir, recall};
use embedmind_core::api::{MemoryDraft, Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::index::normalize;
use embedmind_core::storage::vfs::RealVfs;

/// k for the query phases — same as the suite (`harness::K`).
const K: usize = 10;

fn env_count(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("profile_rss failed: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "agent-mem-100k".to_string());
    let spec = DatasetSpec::by_name(&name).ok_or_else(|| format!("unknown dataset '{name}'"))?;
    let data_dir = default_data_dir();
    let vec_path = spec.vec_path(&data_dir);
    let mind_path = spec.mind_path(&data_dir);
    if !vec_path.exists() || !mind_path.exists() {
        return Err(format!(
            "dataset '{name}' not materialized (run gen_dataset or run_all first)"
        )
        .into());
    }

    let recall_queries = env_count("RSS_RECALL_QUERIES", 50);
    let warm_queries = env_count("RSS_QUERIES", 40);
    let ingest_samples = env_count("RSS_INGEST", 200);

    // One sampler for the whole run: `sample()` returns the *current* RSS for
    // the boundary lines, while phase peaks come from dedicated samplers so
    // each phase's number matches what the suite would report for it.
    let mut rss = RssSampler::new();
    println!("phase-by-phase RSS, dataset {name} (MiB):");
    let report = |label: &str, current_bytes: u64| {
        println!("  {:<44} {:>8.1}", label, mib(current_bytes));
    };
    report("process start", rss.sample());

    // 2. ONNX session — this is product-side memory: the MCP server ships it.
    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::load()?);
    report("after ONNX embedder load", rss.sample());

    // 3. The harness's brute-force baseline data — NOT product memory.
    let set = dataset::load_vec_file(spec, &vec_path, embedder.dims(), embedder.id())?;
    report(
        &format!("after VectorSet load ({} vectors)", set.entries.len()),
        rss.sample(),
    );

    // 4. Engine handle. The pager has no page cache and the HNSW graph is
    //    fully paged (docs/adr/0008), so this should be near-flat.
    let opts = StoreOptions {
        embedder: Some(Arc::clone(&embedder)),
        ..StoreOptions::default()
    };
    let store = Store::open_with(Arc::new(RealVfs), &mind_path, opts)?;
    report("after Store::open", rss.sample());

    // 5. Recall measurement (brute-force top-k + recall_vector per query).
    let texts = recall::query_texts(spec, recall_queries.max(warm_queries));
    let peak = phase_recall(&store, &set, embedder.as_ref(), &texts[..recall_queries])?;
    report(
        &format!("recall phase peak ({recall_queries} queries)"),
        peak,
    );

    // 6. The suite's query-RSS phase: hybrid recall + vector-only, per text.
    let peak = phase_warm_queries(&store, &texts[..warm_queries])?;
    report(
        &format!("warm-query phase peak ({warm_queries} queries)"),
        peak,
    );

    // 7. Drop the harness-only baseline data. If RSS falls by ~the VectorSet
    //    delta, the allocator returns it and phase ordering alone can fix the
    //    suite; if it does not, the fix must avoid the resident copy entirely.
    drop(set);
    report("after VectorSet drop", rss.sample());

    // 8. Same query phase again, now with only product-shaped memory resident
    //    (ONNX session + engine). This is what a real MCP server would show.
    let peak = phase_warm_queries(&store, &texts[..warm_queries])?;
    report(
        &format!("warm-query phase peak, set dropped ({warm_queries} queries)"),
        peak,
    );
    store.close()?;

    // 9. The suite's ingest-RSS phase: scratch store, one-at-a-time
    //    remember_detailed (embed + near-dup scan + WAL commit).
    let peak = phase_ingest(&data_dir, &embedder, ingest_samples)?;
    report(
        &format!("ingest phase peak ({ingest_samples} remembers)"),
        peak,
    );

    println!("(deltas between consecutive lines attribute the bytes)");
    Ok(())
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// The recall phase's allocation pattern: embed, brute-force exact top-k over
/// the resident set, `recall_vector` against the store.
fn phase_recall(
    store: &Store,
    set: &VectorSet,
    embedder: &dyn Embedder,
    texts: &[String],
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut rss = RssSampler::new();
    for text in texts {
        let mut qv = embedder.embed(text)?;
        normalize(&mut qv);
        let _ = baseline::top_k(set, &qv, K, |_| true);
        let _ = store.recall_vector(Query::new(text.clone()).limit(K))?;
        rss.sample();
    }
    Ok(rss.peak_bytes())
}

/// The warm-query phase's allocation pattern (mirrors
/// `harness::measure_warm_queries`): hybrid recall + vector-only per text.
fn phase_warm_queries(store: &Store, texts: &[String]) -> Result<u64, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut rss = RssSampler::new();
    for text in texts {
        let _ = store.recall(Query::new(text.clone()).limit(K))?;
        let _ = store.recall_vector(Query::new(text.clone()).limit(K))?;
        rss.sample();
    }
    eprintln!(
        "    ({} hybrid+vector queries in {:.1}s)",
        texts.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(rss.peak_bytes())
}

/// The ingest phase's allocation pattern (mirrors `harness::measure_ingest`):
/// fresh scratch store, seed-disjoint corpus, `remember_detailed` one at a
/// time.
fn phase_ingest(
    data_dir: &Path,
    embedder: &Arc<dyn Embedder>,
    samples: usize,
) -> Result<u64, Box<dyn std::error::Error>> {
    let corpus = corpus::generate(0x1465_3701_2026_0708, samples);
    let scratch = data_dir.join("_rss_scratch.mind");
    let _ = std::fs::remove_file(&scratch);
    let _ = std::fs::remove_file(scratch.with_extension("mind-wal"));

    let opts = StoreOptions {
        embedder: Some(Arc::clone(embedder)),
        ..StoreOptions::default()
    };
    let mut store = Store::create_with(Arc::new(RealVfs), &scratch, opts)?;
    let mut rss = RssSampler::new();
    for mem in &corpus {
        let _ = store.remember_detailed(
            MemoryDraft::new(mem.content.clone())
                .project(mem.project.clone())
                .agent("rss-profile"),
        )?;
        rss.sample();
    }
    store.close()?;
    let _ = std::fs::remove_file(&scratch);
    let _ = std::fs::remove_file(scratch.with_extension("mind-wal"));
    Ok(rss.peak_bytes())
}
