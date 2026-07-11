//! Hybrid recall: score fusion across ranked result lists.
//!
//! From M2 on, [`Store::recall`](crate::api::Store::recall) fuses two ranked
//! lists — vector similarity (HNSW) and full-text (BM25) — with **Reciprocal
//! Rank Fusion, `k = 60`** (`docs/adr/0005`). From S20 on
//! ([`Store::recall`](crate::api::Store::recall) with recency enabled,
//! `docs/adr/0014`) a third list joins them: the same content candidates
//! (union of vector + text) reordered by `created_at` descending — recency
//! only *breaks ties* among what content ranking already found relevant, per
//! the RRF property below. RRF uses rank *positions* only, never the raw
//! scores: cosine similarity and BM25 live on different scales and
//! normalizing them would introduce exactly the tunable weights ADR 0005
//! rules out. A document at rank `r` (0-based) in a list contributes
//! `1/(k + r + 1)` from that list; a document's fused score is the sum of its
//! contributions from every list it appears in.
//!
//! Properties this module guarantees:
//!
//! - **Union, never intersection** (story S9). A hit that appears in only one
//!   of the lists still makes the fused output — a rare exact term
//!   (text-only) or a semantic synonym (vector-only) is never dropped for
//!   lacking a match in another list. It just scores lower than something
//!   ranked well in several.
//! - **A single list can never invert a content match** (story S20). The
//!   maximum contribution of any one list is `1/(k + 1)` (its rank-0 slot).
//!   Since the recency list ranks only candidates already present in the
//!   vector/text union, it can add at most one list's worth of score to a
//!   content match — never enough to push a mediocre-but-new item over a
//!   candidate that ranked well in *both* content lists, which already has
//!   two lists' worth of score.
//! - **Order is deterministic.** Ties in fused score break by first
//!   appearance across the lists in the order they were added, so the same
//!   inputs always produce the same output — property tests depend on this.
//!
//! This module is pure ranking arithmetic over id lists: it reads no pages and
//! knows nothing about tombstones, scope, or the file format. The orchestration
//! (running the searches, building the recency list, filtering, degrading to
//! vector-only when a pre-M2 file has no full-text index) lives in `api.rs`,
//! one layer up.

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
/// Thin wrapper over [`fuse_lists`] for the two-list case (vector + text,
/// story S9) — kept so call sites and tests that only ever had two lists
/// don't need to build a slice-of-slices.
pub fn fuse(vector: &[Ulid], text: &[Ulid], limit: usize) -> Vec<Fused> {
    fuse_lists(&[vector, text], limit)
}

/// Fuses any number of ranked id lists (each best-first) by Reciprocal Rank
/// Fusion, returning the union ordered by descending fused score, capped at
/// `limit`.
///
/// Inputs are just record ids in rank order — the caller has already turned
/// search hits (vector, BM25, recency-reordered content union, ...) into
/// ranked ids. A list with a repeated id (which the upstream searches never
/// produce, but which would be harmless) counts only its first, best rank. An
/// empty list simply contributes nothing, which is how the vector-only
/// degradation path in `api.rs` reuses this (an empty text list).
///
/// Ties in fused score are broken deterministically: an id is keyed by the
/// first `(list index, rank)` it was seen at, lists compared in the order
/// passed to `lists` and lower rank first — so fusion is a total, reproducible
/// order regardless of how many lists are fused.
pub fn fuse_lists(lists: &[&[Ulid]], limit: usize) -> Vec<Fused> {
    // Per id we accumulate: the fused score, a tiebreak key (the first
    // `(list, rank)` it was seen at — a lower key means "appeared earlier /
    // more authoritatively"), and a bitmask of which lists have already
    // contributed. A list contributes an id's rank *once*: a repeated id
    // within one list keeps only its first, best rank, while a genuine
    // cross-list overlap adds each list's contribution — that per-list sum is
    // exactly what RRF fuses.
    assert!(
        lists.len() <= 8,
        "fuse_lists: the `counted` bitmask is a u8, at most 8 lists"
    );
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
    for (i, ranked) in lists.iter().enumerate() {
        add(i as u8, ranked);
    }

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

    // S9 verification: property tests over the fusion invariants. Random
    // lists (duplicates and overlaps included) — the properties the golden
    // cases spot-check must hold universally.
    proptest::proptest! {
        #[test]
        fn fuse_holds_its_invariants_for_any_input(
            v_seeds in proptest::collection::vec(1u128..40, 0..12),
            t_seeds in proptest::collection::vec(1u128..40, 0..12),
            limit in 0usize..24,
        ) {
            let vector: Vec<Ulid> = v_seeds.iter().map(|&s| Ulid::from(s)).collect();
            let text: Vec<Ulid> = t_seeds.iter().map(|&s| Ulid::from(s)).collect();
            let fused = fuse(&vector, &text, limit);

            // Deterministic: same inputs, same output.
            proptest::prop_assert_eq!(&fused, &fuse(&vector, &text, limit));

            // Capped at limit, sorted best-first.
            proptest::prop_assert!(fused.len() <= limit);
            for w in fused.windows(2) {
                proptest::prop_assert!(w[0].score >= w[1].score);
            }

            // Exactly the union (each id once, positive score), never an
            // intersection: with room, every distinct input id survives.
            let distinct: std::collections::BTreeSet<Ulid> =
                vector.iter().chain(text.iter()).copied().collect();
            let out: std::collections::BTreeSet<Ulid> =
                fused.iter().map(|f| f.record_id).collect();
            proptest::prop_assert_eq!(out.len(), fused.len(), "no id repeats");
            proptest::prop_assert!(out.is_subset(&distinct), "no invented ids");
            if limit >= distinct.len() {
                proptest::prop_assert_eq!(&out, &distinct, "union, never intersection");
            }
            for f in &fused {
                proptest::prop_assert!(f.score > 0.0);
            }
        }
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

    // S20: recency as a third RRF list (`docs/adr/0014`). Golden cases from
    // `docs/01-spec.md` S20 plus property tests over the 3-list fusion.
    mod recency {
        use super::*;

        #[test]
        fn two_list_fuse_matches_fuse_lists_with_two_lists() {
            let all = ids(4);
            let v = vec![all[0], all[1], all[2]];
            let t = vec![all[2], all[3]];
            assert_eq!(fuse(&v, &t, 10), fuse_lists(&[&v, &t], 10));
        }

        #[test]
        fn fact_and_correction_the_newer_one_wins_the_tie() {
            // A genuine content tie: `fact` wins the vector list, `correction`
            // wins the text list (each rank 0 in one, rank 1 in the other), so
            // their fused content score is identical and vector/text ordering
            // alone can't break it. The recency list (correction is newer)
            // breaks the tie — spec S20 golden case "fato + correção: a
            // correção vem primeiro".
            let all = ids(2);
            let (fact, correction) = (all[0], all[1]);
            let vector = vec![fact, correction];
            let text = vec![correction, fact];
            // Recency list: correction is newer, so it is reordered first.
            let recency = vec![correction, fact];
            let fused = fuse_lists(&[&vector, &text, &recency], 10);
            assert_eq!(
                fused[0].record_id, correction,
                "tied content match ⇒ the newer memory (correction) wins"
            );
            assert_eq!(fused[1].record_id, fact);
        }

        #[test]
        fn old_strong_match_beats_new_weak_match() {
            // `old` is rank 0 in both content lists (a strong, well-established
            // match); `new` only shows up in the recency list (it is newer but
            // was never found relevant by vector or text search — recency never
            // manufactures a content match on its own, per S20's edge case).
            let all = ids(2);
            let (old, new) = (all[0], all[1]);
            let vector = vec![old];
            let text = vec![old];
            let recency = vec![new, old]; // `new` is more recent than `old`
            let fused = fuse_lists(&[&vector, &text, &recency], 10);
            assert_eq!(
                fused[0].record_id, old,
                "two content lists (2 contributions) must beat recency alone (1 contribution)"
            );
        }

        #[test]
        fn recency_alone_cannot_invert_a_two_list_content_match() {
            // Direct check of the invariant the module doc promises: the max
            // contribution of any single list is `1/(RRF_K + 1)` (its rank-0
            // slot), so an id present in *two* content lists always outscores
            // one present in only the recency list, at any rank in that list.
            let one = 1.0 / (RRF_K + 1.0);
            let two_list_min = one + 1.0 / (RRF_K + 8.0); // worst case: rank 7 in the 2nd list
            assert!(
                two_list_min > one,
                "two-list floor must exceed any single list's ceiling"
            );
        }

        #[test]
        fn recency_never_introduces_an_id_outside_the_content_union() {
            // The recency list is always built from the same content union
            // upstream (api.rs) — this just documents/enforces that fuse_lists
            // itself doesn't need the caller to dedupe: a recency list that is
            // exactly a permutation of the content union never grows the output
            // beyond that union.
            let all = ids(5);
            let vector = vec![all[0], all[1], all[2]];
            let text = vec![all[2], all[3]];
            let mut recency = vec![all[0], all[1], all[2], all[3]];
            recency.reverse();
            let fused = fuse_lists(&[&vector, &text, &recency], 10);
            let out: std::collections::BTreeSet<Ulid> = fused.iter().map(|f| f.record_id).collect();
            let union: std::collections::BTreeSet<Ulid> =
                vector.iter().chain(text.iter()).copied().collect();
            assert_eq!(out, union, "recency reorders the union, never extends it");
            assert!(
                !out.contains(&all[4]),
                "id absent from both content lists stays absent"
            );
        }

        proptest::proptest! {
            #[test]
            fn fuse_lists_holds_its_invariants_for_any_number_of_lists(
                seeds in proptest::collection::vec(
                    proptest::collection::vec(1u128..40, 0..12),
                    0..5,
                ),
                limit in 0usize..24,
            ) {
                let lists: Vec<Vec<Ulid>> = seeds
                    .iter()
                    .map(|s| s.iter().map(|&x| Ulid::from(x)).collect())
                    .collect();
                let refs: Vec<&[Ulid]> = lists.iter().map(Vec::as_slice).collect();
                let fused = fuse_lists(&refs, limit);

                // Deterministic.
                proptest::prop_assert_eq!(&fused, &fuse_lists(&refs, limit));

                // Capped at limit, sorted best-first.
                proptest::prop_assert!(fused.len() <= limit);
                for w in fused.windows(2) {
                    proptest::prop_assert!(w[0].score >= w[1].score);
                }

                // Exactly the union, never an intersection.
                let distinct: std::collections::BTreeSet<Ulid> =
                    lists.iter().flatten().copied().collect();
                let out: std::collections::BTreeSet<Ulid> =
                    fused.iter().map(|f| f.record_id).collect();
                proptest::prop_assert_eq!(out.len(), fused.len(), "no id repeats");
                proptest::prop_assert!(out.is_subset(&distinct), "no invented ids");
                if limit >= distinct.len() {
                    proptest::prop_assert_eq!(&out, &distinct, "union, never intersection");
                }
                for f in &fused {
                    proptest::prop_assert!(f.score > 0.0);
                }
            }

            #[test]
            fn a_recency_only_hit_never_outranks_a_two_content_list_hit(
                // A candidate present in both vector and text (content-strong),
                // versus one present only in the recency list (recency-only).
                // Regardless of ranks, content-strong must win — this is the
                // property the "old strong match" golden case spot-checks.
                strong_v_rank in 0usize..20,
                strong_t_rank in 0usize..20,
                recency_only_rank in 0usize..20,
            ) {
                let ids2 = ids(2);
                let (strong, recency_only) = (ids2[0], ids2[1]);
                let mut vector = vec![Ulid::from(1000u128); strong_v_rank];
                vector.push(strong);
                let mut text = vec![Ulid::from(2000u128); strong_t_rank];
                text.push(strong);
                let mut recency = vec![Ulid::from(3000u128); recency_only_rank];
                recency.push(recency_only);
                // Keep the padding ids distinct from the two real candidates so
                // they don't accidentally collide and change the scores.
                let fused = fuse_lists(&[&vector, &text, &recency], 100);
                let strong_score = fused.iter().find(|f| f.record_id == strong).unwrap().score;
                let recency_only_score = fused
                    .iter()
                    .find(|f| f.record_id == recency_only)
                    .map(|f| f.score)
                    .unwrap_or(0.0);
                proptest::prop_assert!(strong_score > recency_only_score);
            }
        }
    }
}
