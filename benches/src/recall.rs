//! Recall@k of the HNSW index against the brute-force baseline
//! (`docs/BENCHMARKS.md` §3).
//!
//! HNSW is *approximate*: its top-k is not guaranteed to equal the exact
//! top-k, so it is graded as a **set overlap** — of the exact top-k the
//! baseline returns, what fraction did HNSW also return? Averaged over a fixed
//! query set, that is `recall@k`. Both systems are handed the *same query
//! vector* (embedded once by the shipped model) and the *same eligible
//! population*, so the only variable measured is the index's approximation
//! quality, nothing else (`docs/BENCHMARKS.md`: "same embeddings for all").
//!
//! Queries are drawn deterministically from the same synthetic distribution as
//! the corpus but under a **different seed**, so they resemble real recall
//! traffic without being verbatim copies of stored memories (which would make
//! recall trivially 1.0 and measure nothing).

use std::collections::HashSet;

use embedmind_core::api::{Query, Store};
use embedmind_core::embed::Embedder;
use embedmind_core::index::normalize;
use ulid::Ulid;

use crate::baseline;
use crate::corpus;
use crate::dataset::{DatasetSpec, VectorSet};

/// A recall measurement over one query set.
///
/// The mean alone can hide a catastrophic tail (`docs/BENCHMARKS.md` §3, S16),
/// so the per-query distribution is reported too: the worst query (`min`) and
/// the low percentiles (`p10`, `p50`). A default that scales `ef_search` with
/// index size (S16) is judged on this whole shape, not just the average.
#[derive(Debug, Clone, Copy)]
pub struct RecallReport {
    /// `k` in recall@k.
    pub k: usize,
    /// Number of queries averaged.
    pub queries: usize,
    /// Mean fraction of the exact top-k that HNSW also returned, in `[0, 1]`.
    pub recall_at_k: f64,
    /// Worst single-query recall — surfaces tail misses an average hides.
    pub min_recall: f64,
    /// 10th-percentile per-query recall (nearest-rank): the bad-but-not-worst
    /// tail. A good mean with a low p10 means a meaningful slice of queries
    /// recall poorly, not just one outlier.
    pub p10_recall: f64,
    /// Median per-query recall (nearest-rank): the typical query, unmoved by a
    /// few tail misses in either direction.
    pub p50_recall: f64,
}

/// Derives `n` query texts for `spec`, deterministic and disjoint-in-seed from
/// the corpus (so queries are near, not identical, to stored memories).
pub fn query_texts(spec: &DatasetSpec, n: usize) -> Vec<String> {
    // XOR the corpus seed into a distinct query-seed namespace.
    let query_seed = spec.seed ^ 0x5171_5945_5259_5551;
    corpus::generate(query_seed, n)
        .into_iter()
        .map(|m| m.content)
        .collect()
}

/// Measures recall@k of `store`'s HNSW against the brute-force `baseline` over
/// `set`, for the given `queries`. `store` and `set` must be the same
/// materialized dataset (`dataset::materialize`), so their vectors match.
///
/// For each query the exact top-k comes from [`baseline::top_k`] and the
/// approximate top-k from [`Store::recall_vector`] — the pure HNSW half, *not*
/// the RRF-fused hybrid recall: this metric isolates the index's approximation
/// quality (`docs/BENCHMARKS.md` §3), which fusing in BM25 keyword hits would
/// contaminate. Both sides use the identical query vector because
/// `recall_vector` embeds the text with the same model this harness embeds it
/// with. Overlap / k is that query's recall; the mean is the report.
pub fn measure(
    store: &Store,
    set: &VectorSet,
    embedder: &dyn Embedder,
    queries: &[String],
    k: usize,
) -> embedmind_core::Result<RecallReport> {
    let mut total = 0.0f64;
    let mut per_query: Vec<f64> = Vec::with_capacity(queries.len());
    for text in queries {
        let mut qv = embedder.embed(text)?;
        normalize(&mut qv);

        let exact: HashSet<Ulid> = baseline::top_k(set, &qv, k, |_| true)
            .into_iter()
            .map(|h| h.record_id)
            .collect();

        // No explicit `ef_search` here: the recall metric grades the *default*,
        // which scales with index size (S16, `docs/adr/0015`) — the value a
        // caller who tunes nothing actually gets.
        let approx: HashSet<Ulid> = store
            .recall_vector(Query::new(text.clone()).limit(k))?
            .into_iter()
            .map(|r| r.id)
            .collect();

        // Guard the degenerate case where the baseline itself returned fewer
        // than k (tiny sets): recall is overlap over what *could* be recalled.
        let denom = exact.len().max(1);
        let overlap = exact.intersection(&approx).count();
        let q_recall = overlap as f64 / denom as f64;
        total += q_recall;
        per_query.push(q_recall);
    }
    let queries_n = queries.len().max(1);
    Ok(RecallReport {
        k,
        queries: queries.len(),
        recall_at_k: total / queries_n as f64,
        min_recall: per_query.iter().copied().fold(1.0f64, f64::min),
        p10_recall: percentile(&per_query, 10.0),
        p50_recall: percentile(&per_query, 50.0),
    })
}

/// Nearest-rank percentile of a per-query recall sample, in `[0, 1]`. Same
/// method as [`crate::metrics::Latencies::percentile_ms`]: the reported value
/// is always one a query actually scored (no interpolation), the honest choice
/// for a fixed, modest query set. Empty input yields 0.0.
fn percentile(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}
