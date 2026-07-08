//! Full-suite measurement over one dataset (`docs/BENCHMARKS.md` §3).
//!
//! This is Part 2's core: given a materialized dataset, measure every metric
//! the methodology lists for EmbedMind, on one fixed machine, single-thread:
//!
//! - `recall@10` vs. the brute-force baseline (delegated to [`crate::recall`]).
//! - **warm** query latency p50 / p99 (1k+ queries, cache hot).
//! - **cold-open** first-query latency (`Store::open` on the file, then the
//!   very first `recall` — the "no server, just opened the file" scenario).
//! - `remember` latency p50 / p99 (one-at-a-time, the agent write pattern;
//!   dominated by embedding, which is why it is reported end-to-end and
//!   labeled as such — BENCHMARKS.md §1).
//! - ingest throughput (memories/sec, one-at-a-time).
//! - file size on disk after ingest.
//! - peak RSS during ingest and during query load ([`crate::sysmem`]).
//! - cold-open time (`Store::open`, including any recovery scan).
//!
//! Every number here is EmbedMind's own; the competitor rows come from
//! [`crate::competitors`] and are joined by the renderer. Vectors, queries and
//! `k` are the *same* ones handed to competitors — the methodology's core rule.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use embedmind_core::api::{MemoryDraft, Query, Store, StoreOptions};
use embedmind_core::embed::Embedder;
use embedmind_core::index::normalize;
use embedmind_core::storage::vfs::RealVfs;

use crate::corpus::GenMemory;
use crate::dataset::{DatasetSpec, VectorSet};
use crate::metrics::{self, Latencies};
use crate::recall::{self, RecallReport};
use crate::sysmem::RssSampler;

/// recall@k the suite reports (`docs/BENCHMARKS.md` §3).
pub const K: usize = 10;

/// Every measured metric for one dataset on this machine. Optional fields are
/// `None` when a phase was skipped (e.g. a tiny smoke run); the renderer shows
/// those as `—`.
#[derive(Debug, Clone)]
pub struct SuiteResult {
    /// Dataset name (`agent-mem-10k`, …).
    pub dataset: &'static str,
    /// Number of memories in the dataset.
    pub count: usize,
    /// Embedding dimensionality.
    pub dims: u16,
    /// Model id recorded in the store header.
    pub model_id: String,

    /// recall@10 vs. brute-force, plus the worst single-query recall.
    pub recall: RecallReport,

    /// Warm query latency, over `warm_queries` queries.
    pub query_p50_ms: f64,
    pub query_p99_ms: f64,
    pub query_mean_ms: f64,
    pub warm_queries: usize,

    /// Cold-open: time to `Store::open` the file, and the latency of the very
    /// first `recall` against the freshly opened store.
    pub cold_open_ms: f64,
    pub cold_first_query_ms: f64,

    /// `remember` latency (end-to-end incl. embedding), one-at-a-time, over a
    /// sampled subset of the corpus.
    pub remember_p50_ms: f64,
    pub remember_p99_ms: f64,
    pub remember_samples: usize,
    /// Ingest throughput of that same sample, memories/sec.
    pub ingest_per_sec: f64,

    /// Store file size on disk after ingest, bytes.
    pub file_bytes: u64,

    /// Peak RSS observed during ingest and during the warm query load, MiB.
    pub peak_rss_ingest_mib: f64,
    pub peak_rss_query_mib: f64,

    /// The query vectors used (normalized), so competitors get the identical
    /// set. Not rendered; handed to [`crate::competitors::run_all`].
    pub query_vectors: Vec<Vec<f32>>,
}

/// Runs the full metric suite for `spec` against an already-materialized
/// dataset: `store` opened from `spec.mind_path`, `set` its parallel vectors.
///
/// `warm_queries` is the fixed query count for the warm latency percentiles
/// (BENCHMARKS.md §3 asks for ~1k); `remember_samples` bounds the one-at-a-time
/// `remember` timing so a 100k run stays a few seconds of extra ingest.
pub fn run_suite(
    spec: &DatasetSpec,
    data_dir: &Path,
    store: Store,
    set: &VectorSet,
    embedder: &Arc<dyn Embedder>,
    warm_queries: usize,
    remember_samples: usize,
) -> embedmind_core::Result<SuiteResult> {
    let stats = store.stats()?;
    let model_id = stats.embedding_model_id.clone().unwrap_or_default();

    // --- recall@10 (fixed query set, same as Part 1's baseline binary) ---
    let recall_texts = recall::query_texts(spec, warm_queries);
    let recall_report = recall::measure(&store, set, embedder.as_ref(), &recall_texts, K)?;

    // Pre-embed the query set once; both the warm-latency loop and the
    // competitors receive these identical normalized vectors.
    let mut query_vectors = Vec::with_capacity(recall_texts.len());
    for t in &recall_texts {
        let mut v = embedder.embed(t)?;
        normalize(&mut v);
        query_vectors.push(v);
    }

    // --- warm query latency: recall each query once, cache hot ---
    // Warm the cache with a throwaway pass so p50/p99 reflect steady state, not
    // first-touch page faults (which the cold-open metric captures separately).
    for t in recall_texts.iter().take(recall_texts.len().min(32)) {
        let _ = store.recall(Query::new(t.clone()).limit(K))?;
    }
    let mut warm = Latencies::with_capacity(recall_texts.len());
    let mut rss_query = RssSampler::new();
    for t in &recall_texts {
        let started = Instant::now();
        let _ = store.recall(Query::new(t.clone()).limit(K))?;
        warm.push(started.elapsed());
        rss_query.sample();
    }

    // --- cold-open: close the benchmarked store first, then open the file
    // fresh and time open + first query. The store must be closed because the
    // engine is single-writer (`docs/adr/0006`): only one open handle may hold
    // the file at a time, so a genuine cold open of the *same* file requires
    // releasing this one — which also drops the pager cache, giving a truly
    // cold read (BENCHMARKS.md §3). ---
    store.close()?;
    let (cold_open_ms, cold_first_query_ms) =
        measure_cold_open(spec, data_dir, embedder, recall_texts.first())?;

    // --- remember latency + ingest throughput (one-at-a-time) ---
    // Ingest a fresh sample corpus into a scratch store so we never mutate the
    // benchmarked dataset. Deterministic but seed-disjoint from the corpus.
    let (remember, ingest_per_sec, rss_ingest_mib) =
        measure_ingest(data_dir, embedder, remember_samples)?;

    Ok(SuiteResult {
        dataset: spec.name,
        count: set.entries.len(),
        dims: set.dims,
        model_id,
        recall: recall_report,
        query_p50_ms: warm.p50_ms().unwrap_or(0.0),
        query_p99_ms: warm.p99_ms().unwrap_or(0.0),
        query_mean_ms: warm.mean_ms().unwrap_or(0.0),
        warm_queries: warm.len(),
        cold_open_ms,
        cold_first_query_ms,
        remember_p50_ms: remember.p50_ms().unwrap_or(0.0),
        remember_p99_ms: remember.p99_ms().unwrap_or(0.0),
        remember_samples: remember.len(),
        ingest_per_sec,
        file_bytes: stats.file_bytes,
        peak_rss_ingest_mib: rss_ingest_mib,
        peak_rss_query_mib: rss_query.peak_mib(),
        query_vectors,
    })
}

/// Times a genuinely cold `Store::open` of the benchmarked file (including any
/// WAL recovery scan) plus the very first `recall` against it — the "no daemon
/// running, just opened the file" latency the methodology singles out
/// (BENCHMARKS.md §3).
///
/// The caller must have **closed** the benchmarked store before calling this:
/// the engine is single-writer (`docs/adr/0006`), so opening the same file
/// requires the previous handle to be gone — which also means the pager cache
/// is cold, exactly the state this metric wants to capture.
fn measure_cold_open(
    spec: &DatasetSpec,
    data_dir: &Path,
    embedder: &Arc<dyn Embedder>,
    first_query: Option<&String>,
) -> embedmind_core::Result<(f64, f64)> {
    let opts = StoreOptions {
        embedder: Some(Arc::clone(embedder)),
        ..StoreOptions::default()
    };
    let open_started = Instant::now();
    let cold = Store::open_with(Arc::new(RealVfs), &spec.mind_path(data_dir), opts)?;
    let cold_open_ms = open_started.elapsed().as_secs_f64() * 1000.0;

    let cold_first_query_ms = match first_query {
        Some(t) => {
            let q_started = Instant::now();
            let _ = cold.recall(Query::new(t.clone()).limit(K))?;
            q_started.elapsed().as_secs_f64() * 1000.0
        }
        None => 0.0,
    };
    cold.close()?;
    Ok((cold_open_ms, cold_first_query_ms))
}

/// Measures `remember` latency and ingest throughput one-at-a-time (the agent
/// write pattern, fsync `full`), plus peak RSS during ingest. Uses a scratch
/// store on disk so real fsync cost is included and the benchmarked dataset is
/// never touched.
fn measure_ingest(
    data_dir: &Path,
    embedder: &Arc<dyn Embedder>,
    samples: usize,
) -> embedmind_core::Result<(Latencies, f64, f64)> {
    // Seed-disjoint corpus so ingest content is realistic but not a copy of the
    // benchmarked dataset.
    let corpus: Vec<GenMemory> = crate::corpus::generate(0x1465_3701_2026_0708, samples);

    let scratch = data_dir.join("_ingest_scratch.mind");
    let _ = std::fs::remove_file(&scratch);
    let _ = std::fs::remove_file(scratch.with_extension("mind-wal"));

    let opts = StoreOptions {
        embedder: Some(Arc::clone(embedder)),
        ..StoreOptions::default()
    };
    let mut store = Store::create_with(Arc::new(RealVfs), &scratch, opts)?;

    let mut lat = Latencies::with_capacity(corpus.len());
    let mut rss = RssSampler::new();
    let wall_started = Instant::now();
    for mem in &corpus {
        let started = Instant::now();
        let _ = store.remember(
            MemoryDraft::new(mem.content.clone())
                .project(mem.project.clone())
                .agent("bench-ingest"),
        )?;
        lat.push(started.elapsed());
        rss.sample();
    }
    let ingest_per_sec = metrics::ops_per_sec(corpus.len(), wall_started.elapsed());
    store.close()?;
    let _ = std::fs::remove_file(&scratch);
    let _ = std::fs::remove_file(scratch.with_extension("mind-wal"));

    Ok((lat, ingest_per_sec, rss.peak_mib()))
}
