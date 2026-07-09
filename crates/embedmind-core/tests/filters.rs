//! Metadata filters on `recall` (S10 / B3), end-to-end against the **public
//! API** with the real embedded ONNX model — the same seam `recall.rs` uses.
//!
//! These prove the four things story S10 asks for: composite `key → value`
//! and `key → range` filters with AND semantics, filters composed with project
//! scope and tombstones, a filter on an absent key returning 0 hits (not an
//! error), and a type-incompatible filter returning a typed error. The
//! adaptive-`ef_search` anti-under-return guarantee is inherited for free by
//! feeding the filters into the same `keep` predicate the scope/tombstone
//! checks already use — a filtered-out candidate widens the search, it never
//! silently under-returns.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use embedmind_core::api::{MemoryDraft, Query, Scope, Store, StoreOptions};
use embedmind_core::storage::sim::SimVfs;
use embedmind_core::storage::vfs::Vfs;
use embedmind_core::{Filter, Scalar, embed::OnnxEmbedder};

const STORE: &str = "memory.mind";

/// A store on an in-memory VFS with the real embedded model.
fn store() -> Store {
    let embedder = Arc::new(OnnxEmbedder::load().expect("embedded model must load"));
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let opts = StoreOptions {
        embedder: Some(embedder),
        ..StoreOptions::default()
    };
    Store::create_with(vfs, Path::new(STORE), opts).unwrap()
}

#[test]
fn filter_by_exact_scalar_value_keeps_only_matches() {
    let mut store = store();
    let ops = store
        .remember(
            MemoryDraft::new("the deploy pipeline runs nightly")
                .meta("topic", Scalar::Str("ops".into())),
        )
        .unwrap();
    store
        .remember(
            MemoryDraft::new("the deploy pipeline notes on architecture")
                .meta("topic", Scalar::Str("design".into())),
        )
        .unwrap();

    let hits = store
        .recall(
            Query::new("deploy pipeline")
                .limit(10)
                .filter("topic", Filter::Eq(Scalar::Str("ops".into()))),
        )
        .unwrap();
    assert!(!hits.is_empty(), "the ops memory must survive the filter");
    assert!(
        hits.iter().all(|h| h.metadata.get("topic") == Some(&Scalar::Str("ops".into()))),
        "only topic=ops memories may pass"
    );
    assert!(hits.iter().any(|h| h.id == ops.id));
}

#[test]
fn range_filter_selects_a_numeric_window() {
    let mut store = store();
    for (label, priority) in [("low", 1i64), ("mid", 5), ("high", 9)] {
        store
            .remember(
                MemoryDraft::new(format!("task marked {label} priority"))
                    .meta("priority", Scalar::I64(priority)),
            )
            .unwrap();
    }

    // priority in [4, 10]: excludes the low (1), keeps mid (5) and high (9).
    let hits = store
        .recall(
            Query::new("task priority").limit(10).filter(
                "priority",
                Filter::Range {
                    min: Some(4.0),
                    max: Some(10.0),
                },
            ),
        )
        .unwrap();
    assert!(!hits.is_empty());
    for hit in &hits {
        let Some(Scalar::I64(p)) = hit.metadata.get("priority") else {
            panic!("every hit carries an i64 priority");
        };
        assert!((4..=10).contains(p), "priority {p} out of the filtered window");
    }
    assert!(
        hits.iter().any(|h| h.metadata.get("priority") == Some(&Scalar::I64(5))),
        "mid-priority must be present"
    );
}

#[test]
fn range_filter_is_open_ended_when_a_bound_is_none() {
    let mut store = store();
    for score in [0.2f64, 0.6, 0.95] {
        store
            .remember(
                MemoryDraft::new(format!("model checkpoint scored {score}"))
                    .meta("score", Scalar::F64(score)),
            )
            .unwrap();
    }
    // score >= 0.5, no upper bound.
    let hits = store
        .recall(
            Query::new("checkpoint score").limit(10).filter(
                "score",
                Filter::Range {
                    min: Some(0.5),
                    max: None,
                },
            ),
        )
        .unwrap();
    for hit in &hits {
        let Some(Scalar::F64(s)) = hit.metadata.get("score") else {
            panic!("hit must carry an f64 score");
        };
        assert!(*s >= 0.5, "score {s} below the open lower bound");
    }
    assert!(hits.iter().any(|h| h.metadata.get("score") == Some(&Scalar::F64(0.95))));
    assert!(!hits.iter().any(|h| h.metadata.get("score") == Some(&Scalar::F64(0.2))));
}

#[test]
fn multiple_filters_are_anded() {
    let mut store = store();
    let both = store
        .remember(
            MemoryDraft::new("critical ops runbook for the deploy")
                .meta("topic", Scalar::Str("ops".into()))
                .meta("priority", Scalar::I64(9)),
        )
        .unwrap();
    // Matches topic but not priority.
    store
        .remember(
            MemoryDraft::new("routine ops chore for the deploy")
                .meta("topic", Scalar::Str("ops".into()))
                .meta("priority", Scalar::I64(1)),
        )
        .unwrap();
    // Matches priority but not topic.
    store
        .remember(
            MemoryDraft::new("critical design decision for the deploy")
                .meta("topic", Scalar::Str("design".into()))
                .meta("priority", Scalar::I64(9)),
        )
        .unwrap();

    let hits = store
        .recall(
            Query::new("deploy")
                .limit(10)
                .filter("topic", Filter::Eq(Scalar::Str("ops".into())))
                .filter(
                    "priority",
                    Filter::Range {
                        min: Some(5.0),
                        max: None,
                    },
                ),
        )
        .unwrap();
    assert_eq!(hits.len(), 1, "only the memory satisfying BOTH filters passes");
    assert_eq!(hits[0].id, both.id);
}

#[test]
fn filters_compose_with_project_scope_and_tombstones() {
    let mut store = store();
    let alpha_keep = store
        .remember(
            MemoryDraft::new("uses tokio for async work")
                .project("alpha")
                .meta("kind", Scalar::Str("lib".into())),
        )
        .unwrap();
    // Same metadata, different project — scope must exclude it.
    store
        .remember(
            MemoryDraft::new("uses tokio for async work")
                .project("beta")
                .meta("kind", Scalar::Str("lib".into())),
        )
        .unwrap();
    // Right project + metadata, but forgotten — tombstone must exclude it.
    let doomed = store
        .remember(
            MemoryDraft::new("uses tokio in a scratch note")
                .project("alpha")
                .meta("kind", Scalar::Str("lib".into())),
        )
        .unwrap();
    store.forget(doomed.id).unwrap();

    let hits = store
        .recall(
            Query::new("async runtime")
                .limit(10)
                .scope(Scope::Project("alpha".into()))
                .filter("kind", Filter::Eq(Scalar::Str("lib".into()))),
        )
        .unwrap();
    assert!(hits.iter().any(|h| h.id == alpha_keep.id));
    assert!(
        hits.iter().all(|h| h.project.as_deref() == Some("alpha")),
        "project scope still applies alongside the filter"
    );
    assert!(
        hits.iter().all(|h| h.id != doomed.id),
        "a forgotten memory is excluded even when it matches the filter"
    );
}

#[test]
fn filter_on_absent_key_yields_zero_hits_not_an_error() {
    let mut store = store();
    store
        .remember(MemoryDraft::new("a memory with no such metadata key"))
        .unwrap();
    store
        .remember(
            MemoryDraft::new("another memory, still no such key")
                .meta("other", Scalar::I64(1)),
        )
        .unwrap();

    let hits = store
        .recall(
            Query::new("memory")
                .limit(10)
                .filter("nonexistent", Filter::Eq(Scalar::Str("whatever".into()))),
        )
        .unwrap();
    assert!(
        hits.is_empty(),
        "a filter on a key no memory has must return 0 hits, never error"
    );
}

#[test]
fn type_incompatible_filter_is_a_typed_error() {
    let mut store = store();
    store
        .remember(
            MemoryDraft::new("this memory stores a string topic")
                .meta("topic", Scalar::Str("ops".into())),
        )
        .unwrap();

    // Eq with a mismatched type: integer filter over a stored string.
    let err = store
        .recall(
            Query::new("topic")
                .limit(10)
                .filter("topic", Filter::Eq(Scalar::I64(3))),
        )
        .unwrap_err();
    assert!(
        matches!(err, embedmind_core::Error::InvalidArgument(_)),
        "type mismatch must be InvalidArgument, got {err:?}"
    );

    // Range over a stored non-numeric value: same typed error.
    let err = store
        .recall(
            Query::new("topic").limit(10).filter(
                "topic",
                Filter::Range {
                    min: Some(0.0),
                    max: Some(10.0),
                },
            ),
        )
        .unwrap_err();
    assert!(
        matches!(err, embedmind_core::Error::InvalidArgument(_)),
        "range over a string must be InvalidArgument, got {err:?}"
    );
}
