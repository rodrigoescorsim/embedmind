//! Hybrid recall: score fusion across vector and full-text result lists.
//!
//! From M2 on, [`Store::recall`](crate::api::Store::recall) fuses two ranked
//! lists — vector similarity (HNSW) and full-text (BM25) — with **Reciprocal
//! Rank Fusion, `k = 60`** (`docs/adr/0005`). RRF uses rank *positions* only,
//! never the raw scores: cosine similarity and BM25 live on different scales
//! and normalizing them would introduce exactly the tunable weights ADR 0005
//! rules out. A document at rank `r` (0-based) in a list contributes
//! `1/(k + r + 1)` from that list; a document's fused score is the sum of its
//! contributions from every list it appears in.
//!
//! Two properties this module guarantees, both required by story S9:
//!
//! - **Union, never intersection.** A hit that appears in only one of the two
//!   lists still makes the fused output — a rare exact term (text-only) or a
//!   semantic synonym (vector-only) is never dropped for lacking a match in the
//!   other list. It just scores lower than something ranked well in both.
//! - **Order is deterministic.** Ties in fused score break by first appearance
//!   in the vector list, then the text list, so the same inputs always produce
//!   the same output — property tests depend on this.
//!
//! This module is pure ranking arithmetic over id lists: it reads no pages and
//! knows nothing about tombstones, scope, or the file format. The orchestration
//! (running both searches, filtering, degrading to vector-only when a pre-M2
//! file has no full-text index) lives in `api.rs`, one layer up.

use ulid::Ulid;

/// RRF constant from `docs/adr/0005`. The standard value; it damps the
/// influence of deep ranks so the top of each list dominates the fusion.
pub const RRF_K: f32 = 60.0;

/// One fused hit: a record id and its Reciprocal Rank Fusion score. Ordered
/// best (highest) score first by [`fuse`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fused {
    /// The memory this fused rank refers to.
    pub record_id: Ulid,
    /// Sum of `1/(RRF_K + rank + 1)` across the lists the id appears in.
    /// Higher is better. Never negative; a returned id always appears in at
    /// least one input list.
    pub score: f32,
}

/// Fuses two ranked id lists (each best-first) by Reciprocal Rank Fusion,
/// returning the union ordered by descending fused score, capped at `limit`.
///
/// Inputs are just record ids in rank order — the caller has already turned
/// vector/BM25 hits into ranked ids. A list with a repeated id (which the
/// upstream searches never produce, but which would be harmless) counts only
/// its first, best rank. An empty `text` list degenerates to vector-only
/// order and vice-versa, which is exactly how the vector-only degradation path
/// in `api.rs` reuses this function (it passes an empty text list).
///
/// Ties in fused score are broken deterministically: an id seen in the vector
/// list outranks one seen only in the text list, and within a list earlier
/// rank wins — so fusion is a total, reproducible order.
pub fn fuse(vector: &[Ulid], text: &[Ulid], limit: usize) -> Vec<Fused> {
    // Per id we accumulate: the fused score, a tiebreak key (the first
    // `(list, rank)` it was seen at — `list` 0 = vector, 1 = text, so a lower
    // key means "appeared earlier / more authoritatively"), and a bitmask of
    // which lists have already contributed. A list contributes an id's rank
    // *once*: a repeated id within one list keeps only its first, best rank,
    // while a genuine cross-list overlap adds each list's contribution — that
    // per-list sum is exactly what RRF fuses.
    struct Entry {
        id: Ulid,
        score: f32,
        tiebreak: (u8, u32),
        counted: u8,
    }
    let mut acc: Vec<Entry> = Vec::new();

    let mut add = |list: u8, ranked: &[Ulid]| {
        let bit = 1u8 << list;
        for (rank, &id) in ranked.iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f32 + 1.0);
            match acc.iter_mut().find(|e| e.id == id) {
                Some(e) => {
                    if e.counted & bit == 0 {
                        e.score += contribution;
                        e.counted |= bit;
                    }
                }
                None => acc.push(Entry {
                    id,
                    score: contribution,
                    tiebreak: (list, rank as u32),
                    counted: bit,
                }),
            }
        }
    };
    add(0, vector);
    add(1, text);

    // Best fused score first; ties broken by the first-seen key so the order
    // is total and deterministic (property tests rely on this).
    acc.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.tiebreak.cmp(&b.tiebreak))
    });
    acc.truncate(limit);
    acc.into_iter()
        .map(|e| Fused {
            record_id: e.id,
            score: e.score,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ids(n: usize) -> Vec<Ulid> {
        // Deterministic distinct ids (ULID is 128-bit; build from a counter).
        (0..n).map(|i| Ulid::from(i as u128 + 1)).collect()
    }

    #[test]
    fn rank_one_in_both_beats_rank_one_in_one() {
        let all = ids(3);
        let (a, b, c) = (all[0], all[1], all[2]);
        // a: rank 0 in both. b: rank 0 in text only. c: rank 1 in vector only.
        let fused = fuse(&[a, c], &[a, b], 10);
        assert_eq!(fused[0].record_id, a, "top of both lists must win");
        // a's score is the sum of two rank-0 contributions.
        let one = 1.0 / (RRF_K + 1.0);
        assert!((fused[0].score - 2.0 * one).abs() < 1e-6);
    }

    #[test]
    fn union_keeps_singletons_from_each_list() {
        let all = ids(3);
        let (v_only, t_only, _) = (all[0], all[1], all[2]);
        // Disjoint lists: fusion must keep both, never require intersection.
        let fused = fuse(&[v_only], &[t_only], 10);
        let out: Vec<Ulid> = fused.iter().map(|f| f.record_id).collect();
        assert!(out.contains(&v_only), "vector-only hit survives");
        assert!(out.contains(&t_only), "text-only hit survives");
        // Both at rank 0 in their sole list → equal score; vector list wins tie.
        assert_eq!(fused[0].record_id, v_only);
    }

    #[test]
    fn empty_text_list_is_vector_order() {
        let all = ids(4);
        let fused = fuse(&all, &[], 10);
        let out: Vec<Ulid> = fused.iter().map(|f| f.record_id).collect();
        assert_eq!(out, all, "no text list ⇒ vector order preserved");
    }

    #[test]
    fn empty_vector_list_is_text_order() {
        let all = ids(4);
        let fused = fuse(&[], &all, 10);
        let out: Vec<Ulid> = fused.iter().map(|f| f.record_id).collect();
        assert_eq!(out, all);
    }

    #[test]
    fn both_empty_yields_empty() {
        assert!(fuse(&[], &[], 10).is_empty());
    }

    #[test]
    fn limit_caps_the_union() {
        let v = ids(10);
        let mut t = ids(20);
        t.reverse();
        let fused = fuse(&v, &t, 5);
        assert_eq!(fused.len(), 5);
    }

    #[test]
    fn duplicate_id_in_a_list_counts_its_best_rank_once() {
        let all = ids(2);
        let (a, b) = (all[0], all[1]);
        // `a` repeated in the vector list: only its rank-0 contribution counts.
        let fused = fuse(&[a, a, b], &[], 10);
        assert_eq!(fused.len(), 2, "no phantom duplicate id");
        let one = 1.0 / (RRF_K + 1.0);
        assert!((fused[0].score - one).abs() < 1e-6);
        assert_eq!(fused[0].record_id, a);
    }

    #[test]
    fn duplicate_within_a_list_does_not_inflate_cross_list_overlap() {
        // `a` is repeated in the vector list *and* present in the text list.
        // The vector list must contribute exactly once (its best rank), the
        // text list once — a genuine two-list overlap, not three. Guards the
        // union sum against double-counting an intra-list repeat.
        let all = ids(2);
        let (a, b) = (all[0], all[1]);
        let fused = fuse(&[a, a, b], &[a], 10);
        let a_hit = fused.iter().find(|f| f.record_id == a).unwrap();
        let one = 1.0 / (RRF_K + 1.0);
        assert!(
            (a_hit.score - 2.0 * one).abs() < 1e-6,
            "one rank-0 hit per list ⇒ 2×, never 3× for the intra-list repeat"
        );
    }

    #[test]
    fn output_is_sorted_descending_and_deterministic() {
        let all = ids(6);
        let v = vec![all[0], all[1], all[2], all[3]];
        let t = vec![all[3], all[2], all[4], all[5]];
        let a = fuse(&v, &t, 10);
        let b = fuse(&v, &t, 10);
        assert_eq!(a, b, "same inputs ⇒ same output");
        for w in a.windows(2) {
            assert!(w[0].score >= w[1].score, "scores must be non-increasing");
        }
    }
}
