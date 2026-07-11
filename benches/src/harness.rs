//! Full-suite measurement over one dataset (`docs/BENCHMARKS.md` §3).
//!
//! This is Part 2's core: given a materialized dataset, measure every metric
//! the methodology lists for EmbedMind, on one fixed machine, single-thread:
//!
//! - `recall@10` vs. the brute-force baseline (delegated to [`crate::recall`]).
//! - **warm** query latency p50 / p99 (1k+ queries, cache hot), decomposed
//!   into `embed` (query embedding) vs. `engine` (hybrid search + RRF fusion
//!   plus record load) — S17: baselines receive ready-made vectors, so the
//!   embed-inclusive total alone would compare different work.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use embedmind_core::api::{MemoryDraft, Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, ModelId};
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

    /// Warm query latency **decomposed** (S17): per timed `recall`, `embed`
    /// is the slice spent embedding the query text (measured inside
    /// [`TimingEmbedder`]); `engine` is the exact remainder — hybrid search +
    /// RRF fusion + record load with the query vector ready, the number a
    /// vector-in baseline is comparable to. Percentiles are computed per
    /// component, so embed + engine need not equal the total percentile.
    pub query_embed_p50_ms: f64,
    pub query_embed_p99_ms: f64,
    pub query_engine_p50_ms: f64,
    pub query_engine_p99_ms: f64,

    /// Warm **vector-only** query latency (`Store::recall_vector` — HNSW half,
    /// no BM25 fusion), over the same `recall_texts` as `query_p50/p99_ms`. Not
    /// an NFR by itself; it isolates whether a query-latency miss traces to the
    /// vector half or the full-text half of the hybrid `recall` (BQ, S16
    /// follow-up: the FTS postings-list scan was found to dominate `query_p99_ms`
    /// at 100k — this metric proves/disproves that per run instead of by code
    /// inspection alone). Comparable to `query_engine_p50/p99_ms` (both exclude
    /// embed time): the delta between the two is exactly the FTS+fusion cost.
    pub query_vector_p50_ms: f64,
    pub query_vector_p99_ms: f64,

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

/// Tunables for [`run_suite`] that aren't derived from the dataset or store.
#[derive(Debug, Clone, Copy)]
pub struct SuiteOptions {
    /// Fixed query count for the warm latency percentiles (BENCHMARKS.md §3
    /// asks for ~1k).
    pub warm_queries: usize,
    /// Bounds the one-at-a-time `remember` timing so a 100k run stays a few
    /// seconds of extra ingest.
    pub remember_samples: usize,
    /// Whether warm queries run with `Query::recency` on (S20, `docs/adr/0014`).
    pub recency: bool,
}

/// Runs the full metric suite for `spec` against an already-materialized
/// dataset: `store` opened from `spec.mind_path`, `set` its parallel vectors.
pub fn run_suite(
    spec: &DatasetSpec,
    data_dir: &Path,
    store: Store,
    set: &VectorSet,
    embedder: &Arc<dyn Embedder>,
    opts: SuiteOptions,
) -> embedmind_core::Result<SuiteResult> {
    let stats = store.stats()?;
    let model_id = stats.embedding_model_id.clone().unwrap_or_default();

    // --- recall@10 (fixed query set, same as Part 1's baseline binary) ---
    let recall_texts = recall::query_texts(spec, opts.warm_queries);
    let recall_report = recall::measure(&store, set, embedder.as_ref(), &recall_texts, K)?;

    // Pre-embed the query set once; both the warm-latency loop and the
    // competitors receive these identical normalized vectors.
    let mut query_vectors = Vec::with_capacity(recall_texts.len());
    for t in &recall_texts {
        let mut v = embedder.embed(t)?;
        normalize(&mut v);
        query_vectors.push(v);
    }

    // --- warm query latency, decomposed embed vs. engine vs. vector-only (S17
    // + BQ/S16 follow-up) ---
    // The engine embeds the query text *inside* `recall`, so a single
    // stopwatch would mix embed + search while the vector-in baselines only
    // pay search. Reopen the store with a [`TimingEmbedder`] so each timed
    // recall splits into its embed half (recorded by the wrapper, nested
    // inside the timed call) and its engine half (the exact remainder);
    // `measure_warm_queries` also times a same-query `recall_vector` call
    // right after, isolating the FTS+fusion cost from the vector half's.
    // Closing the benchmarked store first is required anyway: the engine is
    // single-writer (`docs/adr/0006`), only one handle may hold the file.
    store.close()?;
    let timing = Arc::new(TimingEmbedder::new(Arc::clone(embedder)));
    let warm_opts = StoreOptions {
        embedder: Some(Arc::clone(&timing) as Arc<dyn Embedder>),
        ..StoreOptions::default()
    };
    let warm_store = Store::open_with(Arc::new(RealVfs), &spec.mind_path(data_dir), warm_opts)?;
    let warm = measure_warm_queries(&warm_store, &timing, &recall_texts, K, opts.recency)?;

    // --- cold-open: close the warm store, then open the file fresh and time
    // open + first query — releasing the handle also drops the pager cache,
    // giving a truly cold read (BENCHMARKS.md §3). ---
    warm_store.close()?;
    let (cold_open_ms, cold_first_query_ms) =
        measure_cold_open(spec, data_dir, embedder, recall_texts.first())?;

    // --- remember latency + ingest throughput (one-at-a-time) ---
    // Ingest a fresh sample corpus into a scratch store so we never mutate the
    // benchmarked dataset. Deterministic but seed-disjoint from the corpus.
    let (remember, ingest_per_sec, rss_ingest_mib) =
        measure_ingest(data_dir, embedder, opts.remember_samples)?;

    Ok(SuiteResult {
        dataset: spec.name,
        count: set.entries.len(),
        dims: set.dims,
        model_id,
        recall: recall_report,
        query_p50_ms: warm.total.p50_ms().unwrap_or(0.0),
        query_p99_ms: warm.total.p99_ms().unwrap_or(0.0),
        query_mean_ms: warm.total.mean_ms().unwrap_or(0.0),
        warm_queries: warm.total.len(),
        query_embed_p50_ms: warm.embed.p50_ms().unwrap_or(0.0),
        query_embed_p99_ms: warm.embed.p99_ms().unwrap_or(0.0),
        query_engine_p50_ms: warm.engine.p50_ms().unwrap_or(0.0),
        query_engine_p99_ms: warm.engine.p99_ms().unwrap_or(0.0),
        query_vector_p50_ms: warm.vector.p50_ms().unwrap_or(0.0),
        query_vector_p99_ms: warm.vector.p99_ms().unwrap_or(0.0),
        cold_open_ms,
        cold_first_query_ms,
        remember_p50_ms: remember.p50_ms().unwrap_or(0.0),
        remember_p99_ms: remember.p99_ms().unwrap_or(0.0),
        remember_samples: remember.len(),
        ingest_per_sec,
        file_bytes: stats.file_bytes,
        peak_rss_ingest_mib: rss_ingest_mib,
        peak_rss_query_mib: warm.peak_rss_mib,
        query_vectors,
    })
}

/// Wraps an [`Embedder`] and accumulates the wall time spent inside it, so a
/// timed `Store::recall` can be decomposed into its embed half and its engine
/// half (S17, `docs/BENCHMARKS.md` §3): baselines receive ready-made vectors,
/// and EmbedMind's embed-inclusive stopwatch alone would compare different
/// work. One `Instant` + one atomic add per call; the atomic (rather than a
/// `Cell`) is only because `Embedder` requires `Sync`.
pub struct TimingEmbedder {
    inner: Arc<dyn Embedder>,
    /// Nanoseconds spent embedding since the last
    /// [`TimingEmbedder::take_embed_elapsed`].
    spent_nanos: AtomicU64,
}

impl TimingEmbedder {
    /// Wraps `inner`, with the counter at zero.
    pub fn new(inner: Arc<dyn Embedder>) -> Self {
        Self {
            inner,
            spent_nanos: AtomicU64::new(0),
        }
    }

    /// Time spent embedding since the last call (or construction), resetting
    /// the counter — call it right after a timed operation to attribute that
    /// operation's embedding slice.
    pub fn take_embed_elapsed(&self) -> Duration {
        Duration::from_nanos(self.spent_nanos.swap(0, Ordering::Relaxed))
    }

    fn record(&self, started: Instant) {
        self.spent_nanos.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

impl Embedder for TimingEmbedder {
    fn embed(&self, text: &str) -> embedmind_core::Result<Vec<f32>> {
        let started = Instant::now();
        let result = self.inner.embed(text);
        self.record(started);
        result
    }

    fn embed_chunks(&self, text: &str) -> embedmind_core::Result<Vec<Vec<f32>>> {
        let started = Instant::now();
        let result = self.inner.embed_chunks(text);
        self.record(started);
        result
    }

    fn id(&self) -> ModelId {
        self.inner.id()
    }

    fn dims(&self) -> u16 {
        self.inner.dims()
    }
}

/// Measured warm-query latencies, decomposed per query (S17): `total` is the
/// end-to-end `recall` wall time; `embed` the slice of it spent embedding the
/// query text; `engine` the exact remainder — hybrid search + RRF fusion +
/// record load with the query vector ready.
#[derive(Debug, Default, Clone)]
pub struct WarmQueryLatencies {
    pub total: Latencies,
    pub embed: Latencies,
    pub engine: Latencies,
    /// `Store::recall_vector` (HNSW half only, no BM25/RRF fusion) timed right
    /// after the hybrid call for the same query text, so both hit an equally
    /// warm page cache. Comparable to `engine` (both exclude embed time): the
    /// delta isolates the FTS+fusion cost from everything the vector half pays
    /// too (BQ, S16 follow-up — the FTS postings-list scan was found to
    /// dominate `engine` at 100k; this proves/disproves that per run instead
    /// of by code inspection alone).
    pub vector: Latencies,
    /// Peak RSS observed during the query load, MiB.
    pub peak_rss_mib: f64,
}

/// Runs the warm-latency phase over `texts`: a short throwaway warm-up pass,
/// then one timed `recall` per text, each decomposed into embed vs. engine.
///
/// `store` **must** have been opened with `timing` as its embedder — the
/// decomposition subtracts the wrapper's recorded embed time from the timed
/// call, so with a different embedder the engine half would silently equal
/// the total. `run_suite` constructs the pair that way.
///
/// `recency` toggles `Query::recency` (S20, `docs/adr/0014`) on every timed
/// query — the ADR's latency measurement runs this function once with each
/// value and diffs the `engine` percentiles, isolating the extra list's cost
/// from everything else (embedding, cache warmth, dataset).
pub fn measure_warm_queries(
    store: &Store,
    timing: &TimingEmbedder,
    texts: &[String],
    k: usize,
    recency: bool,
) -> embedmind_core::Result<WarmQueryLatencies> {
    // Warm the cache with a throwaway pass so p50/p99 reflect steady state,
    // not first-touch page faults (the cold-open metric captures those
    // separately).
    for t in texts.iter().take(texts.len().min(32)) {
        let _ = store.recall(Query::new(t.clone()).limit(k).recency(recency))?;
    }
    let _ = timing.take_embed_elapsed(); // discard the warm-up's embeds

    let mut out = WarmQueryLatencies {
        total: Latencies::with_capacity(texts.len()),
        embed: Latencies::with_capacity(texts.len()),
        engine: Latencies::with_capacity(texts.len()),
        vector: Latencies::with_capacity(texts.len()),
        peak_rss_mib: 0.0,
    };
    let mut rss = RssSampler::new();
    for t in texts {
        let started = Instant::now();
        let _ = store.recall(Query::new(t.clone()).limit(k).recency(recency))?;
        let total = started.elapsed();
        let embed = timing.take_embed_elapsed();
        out.total.push(total);
        out.embed.push(embed);
        // The embed ran nested inside the timed recall, so it never exceeds
        // the total; `checked_sub` is belt-and-suspenders.
        out.engine
            .push(total.checked_sub(embed).unwrap_or(Duration::ZERO));
        rss.sample();

        // Vector-only half of the same query, right after the hybrid call so
        // both hit an equally warm page cache — isolates the FTS/fusion cost
        // that `engine` includes and this does not. `recall_vector` re-embeds
        // the text through the same `timing`-wrapped store, so subtract that
        // embed slice the same way `engine` does above — otherwise `vector`
        // would include embed time twice (once already in the total it's
        // compared against) and the accumulator would leak into the next
        // loop iteration's `embed`/`engine` reading.
        let v_started = Instant::now();
        let _ = store.recall_vector(Query::new(t.clone()).limit(k))?;
        let v_total = v_started.elapsed();
        let v_embed = timing.take_embed_elapsed();
        out.vector
            .push(v_total.checked_sub(v_embed).unwrap_or(Duration::ZERO));
    }
    out.peak_rss_mib = rss.peak_mib();
    Ok(out)
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
        // `remember_detailed`, not plain `remember`: since S21 the MCP tool's
        // write path includes the near-duplicate scan, so the honest
        // end-to-end `remember` latency includes it too.
        let _ = store.remember_detailed(
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Inner stub with a known, floor-guaranteed cost per call
    /// (`thread::sleep` never sleeps less than asked).
    struct SleepyEmbedder;

    impl Embedder for SleepyEmbedder {
        fn embed(&self, _text: &str) -> embedmind_core::Result<Vec<f32>> {
            std::thread::sleep(Duration::from_millis(2));
            Ok(vec![0.25; 4])
        }
        fn id(&self) -> ModelId {
            "sleepy-stub"
        }
        fn dims(&self) -> u16 {
            4
        }
    }

    #[test]
    fn timing_embedder_accumulates_and_take_resets() {
        let timing = TimingEmbedder::new(Arc::new(SleepyEmbedder));
        assert_eq!(timing.take_embed_elapsed(), Duration::ZERO);

        let v = timing.embed("a").unwrap();
        assert_eq!(v.len(), 4, "the inner result passes through");
        let _ = timing.embed("b").unwrap();
        let spent = timing.take_embed_elapsed();
        assert!(
            spent >= Duration::from_millis(4),
            "two >=2ms embeds must be recorded, got {spent:?}"
        );
        // Taking drains: nothing embedded since ⇒ zero.
        assert_eq!(timing.take_embed_elapsed(), Duration::ZERO);
    }

    #[test]
    fn timing_embedder_forwards_identity_and_times_chunks() {
        let timing = TimingEmbedder::new(Arc::new(SleepyEmbedder));
        assert_eq!(timing.id(), "sleepy-stub");
        assert_eq!(timing.dims(), 4);
        // The default `embed_chunks` forwards to `embed`; the wrapper must
        // still see (and time) it so remember-path embeds are attributed too.
        let chunks = timing.embed_chunks("a").unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(timing.take_embed_elapsed() >= Duration::from_millis(2));
    }
}
