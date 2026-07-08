//! Brute-force exact nearest-neighbor baseline (`docs/BENCHMARKS.md` §1).
//!
//! This is the **recall ceiling and latency floor** every other system is
//! measured against: a linear scan computes the true top-k by cosine
//! similarity, so HNSW's approximate results are graded as a set overlap
//! against it (`recall@10`), and its p50/p99 latency is the honest "no index,
//! just scan" reference. Deliberately the simplest correct implementation —
//! it is the definition of correct, not a thing to optimize.

use ulid::Ulid;

use crate::dataset::VectorSet;

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
fn dot(a: &[f32], b: &[f32]) -> f32 {
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
