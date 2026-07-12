//! Brute-force exact nearest-neighbor baseline (`docs/BENCHMARKS.md` §1).
//!
//! This is the **recall ceiling and latency floor** every other system is
//! measured against: a linear scan computes the true top-k by cosine
//! similarity, so approximate results are graded against it (`recall@10`),
//! and its p50/p99 latency is the honest "no index, just scan" reference.
//! Deliberately the simplest correct implementation — it is the definition
//! of correct, not a thing to optimize.
//!
//! Grading is **tie-aware** (`docs/adr/0019`, story S27): a returned hit
//! counts as correct when its exact cosine score ties or beats the k-th
//! exact score, not only when its *id* is one of the k the baseline's
//! deterministic tie-break happened to keep. The agent-memory corpus
//! contains exact duplicate texts by design (23% at 100k — a real agent
//! re-remembers facts), which embed to bit-identical vectors, so the exact
//! top-k boundary is routinely a plateau of tied scores wider than k;
//! *which* tied ids a correct index returns is arbitrary, and grading that
//! coin flip as a miss measures the tie-break, not the index.

use ulid::Ulid;

use crate::dataset::VectorSet;

/// Two hits whose cosine scores differ by at most this are the same result
/// for grading purposes (`docs/adr/0019`). Exact duplicate texts embed to
/// bit-identical vectors (score delta exactly 0.0); the epsilon only absorbs
/// float summation noise, it is far below the score gap between genuinely
/// different neighbors (measured in `probe_worst`: grading is identical at
/// 1e-5 and 1e-4 on both committed datasets).
pub const SCORE_TIE_EPS: f32 = 1e-5;

/// One exact hit: a record id and its cosine similarity to the query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExactHit {
    /// The memory this vector belongs to.
    pub record_id: Ulid,
    /// Cosine similarity to the query (inner product of normalized vectors).
    pub score: f32,
}

/// Cosine similarity of two already-normalized vectors (a plain dot product;
/// `docs/FORMAT.md` §6). Panics-free: mismatched lengths just stop at the
/// shorter one, which the callers never trigger (all vectors share `dims`).
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Exact top-`k` over the whole set, best (highest similarity) first. `query`
/// must be L2-normalized (as the store's queries are). `filter` selects
/// eligible record ids — mirroring HNSW search's tombstone/scope filter so the
/// two are compared on the same eligible population.
pub fn top_k(
    set: &VectorSet,
    query: &[f32],
    k: usize,
    mut filter: impl FnMut(Ulid) -> bool,
) -> Vec<ExactHit> {
    let mut scored: Vec<ExactHit> = set
        .entries
        .iter()
        .filter(|e| filter(e.id))
        .map(|e| ExactHit {
            record_id: e.id,
            score: dot(&e.vector, query),
        })
        .collect();
    // Descending by score; ties broken by id for a deterministic ordering, so
    // the baseline is reproducible run to run (float ties do happen on
    // duplicate/near-duplicate synthetic memories).
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    scored.truncate(k);
    scored
}

/// Tie-aware overlap (`docs/adr/0019`): of the returned hits, how many score
/// at least as well (within [`SCORE_TIE_EPS`]) as the worst exact hit? Every
/// id-overlap hit qualifies by construction (it *is* one of the exact top-k),
/// so this is never below the plain id overlap; it additionally credits hits
/// that tie the k-th exact score without being the ids the baseline's
/// deterministic tie-break kept. Capped at `exact.len()` so recall never
/// exceeds 1.0 when a plateau is wider than k.
pub fn tie_aware_overlap(
    exact: &[ExactHit],
    returned_scores: impl IntoIterator<Item = f32>,
) -> usize {
    let Some(kth) = exact.last().map(|h| h.score) else {
        return 0;
    };
    returned_scores
        .into_iter()
        .filter(|&s| s >= kth - SCORE_TIE_EPS)
        .count()
        .min(exact.len())
}

/// One query's tie-aware recall for a system that returns **positions into
/// `set.entries`** — the id space all three competitor adapters use
/// (sqlite-vec's `rowid`, zvec's stringified pk, Chroma's stringified id).
/// Scores each returned position against `query` and grades with
/// [`tie_aware_overlap`] over the same exact top-k; out-of-range positions
/// score as misses. Returns `None` when `exact` is empty (nothing gradable).
pub fn tie_aware_recall_by_position(
    set: &VectorSet,
    query: &[f32],
    exact: &[ExactHit],
    positions: impl IntoIterator<Item = i64>,
) -> Option<f64> {
    if exact.is_empty() {
        return None;
    }
    let scores = positions.into_iter().map(|p| {
        usize::try_from(p)
            .ok()
            .and_then(|p| set.entries.get(p))
            .map_or(f32::NEG_INFINITY, |e| dot(&e.vector, query))
    });
    Some(tie_aware_overlap(exact, scores) as f64 / exact.len() as f64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::dataset::VectorEntry;
    use embedmind_core::index::normalize;

    fn entry(id: Ulid, v: Vec<f32>) -> VectorEntry {
        let mut vector = v;
        normalize(&mut vector);
        VectorEntry {
            id,
            vector,
            project: String::new(),
        }
    }

    #[test]
    fn returns_closest_first() {
        let ids: Vec<Ulid> = (0..3).map(|_| Ulid::new()).collect();
        let set = VectorSet {
            dims: 2,
            entries: vec![
                entry(ids[0], vec![1.0, 0.0]),
                entry(ids[1], vec![0.0, 1.0]),
                entry(ids[2], vec![1.0, 1.0]),
            ],
        };
        let mut q = vec![1.0, 0.05];
        normalize(&mut q);
        let hits = top_k(&set, &q, 3, |_| true);
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits[0].record_id, ids[0],
            "the axis-aligned vector is closest"
        );
        // Monotonically non-increasing scores.
        assert!(hits[0].score >= hits[1].score && hits[1].score >= hits[2].score);
    }

    #[test]
    fn filter_excludes_ids() {
        let ids: Vec<Ulid> = (0..2).map(|_| Ulid::new()).collect();
        let set = VectorSet {
            dims: 2,
            entries: vec![entry(ids[0], vec![1.0, 0.0]), entry(ids[1], vec![0.9, 0.1])],
        };
        let q = vec![1.0, 0.0];
        let hits = top_k(&set, &q, 10, |id| id != ids[0]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, ids[1]);
    }

    #[test]
    fn tie_aware_overlap_credits_tied_ids_the_exact_top_k_dropped() {
        // Exact top-2 kept scores [1.0, 0.8]; the corpus has a third vector
        // also scoring 0.8 that the deterministic tie-break dropped. A system
        // returning that *other* tied id is as correct as one returning the
        // kept id — both must grade 2/2.
        let exact = vec![
            ExactHit {
                record_id: Ulid::new(),
                score: 1.0,
            },
            ExactHit {
                record_id: Ulid::new(),
                score: 0.8,
            },
        ];
        assert_eq!(tie_aware_overlap(&exact, [1.0, 0.8]), 2, "the kept ids");
        assert_eq!(
            tie_aware_overlap(&exact, [1.0, 0.8 - SCORE_TIE_EPS / 2.0]),
            2,
            "a tied id the exact top-k dropped counts too"
        );
        assert_eq!(
            tie_aware_overlap(&exact, [1.0, 0.5]),
            1,
            "a genuinely worse hit is still a miss"
        );
        assert_eq!(tie_aware_overlap(&exact, []), 0, "no hits, no credit");
        assert_eq!(tie_aware_overlap(&[], [1.0]), 0, "empty exact set");
        assert_eq!(
            tie_aware_overlap(&exact, [1.0, 0.9, 0.85]),
            2,
            "a plateau wider than k never grades above k"
        );
    }

    #[test]
    fn tie_aware_recall_by_position_scores_positions_against_the_set() {
        let ids: Vec<Ulid> = (0..3).map(|_| Ulid::new()).collect();
        let set = VectorSet {
            dims: 2,
            entries: vec![
                entry(ids[0], vec![1.0, 0.0]),
                entry(ids[1], vec![0.0, 1.0]),
                entry(ids[2], vec![1.0, 0.0]), // exact duplicate of entry 0
            ],
        };
        let q = vec![1.0, 0.0];
        let exact = top_k(&set, &q, 2, |_| true);
        assert_eq!(exact.len(), 2, "entries 0 and 2 tie at score 1.0");

        // Returning either duplicate (or both) is a perfect answer.
        assert_eq!(
            tie_aware_recall_by_position(&set, &q, &exact, [0i64, 2]),
            Some(1.0)
        );
        // The orthogonal entry is a real miss; out-of-range is a miss too.
        assert_eq!(
            tie_aware_recall_by_position(&set, &q, &exact, [0i64, 1]),
            Some(0.5)
        );
        assert_eq!(
            tie_aware_recall_by_position(&set, &q, &exact, [0i64, 99]),
            Some(0.5)
        );
        assert_eq!(tie_aware_recall_by_position(&set, &q, &[], [0i64]), None);
    }

    #[test]
    fn k_larger_than_set_returns_all() {
        let ids: Vec<Ulid> = (0..2).map(|_| Ulid::new()).collect();
        let set = VectorSet {
            dims: 2,
            entries: vec![entry(ids[0], vec![1.0, 0.0]), entry(ids[1], vec![0.0, 1.0])],
        };
        let hits = top_k(&set, &[1.0, 0.0], 10, |_| true);
        assert_eq!(hits.len(), 2);
    }
}
