//! End-to-end vector recall (M1 item 1.3) against the **public API**, using
//! the real embedded ONNX model (`Store::create`/`open`). These are the tests
//! that prove `remember` embeds + indexes and `recall` returns semantically
//! near memories, filtered by scope and tombstone — the seams unit tests in
//! `index`/`embed` cannot cover because they stub the vector or the source.
//!
//! Loading the ONNX session is the slow part, so each test builds one store;
//! the crash sweep (`crash_records.rs`) deliberately runs embedder-free.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use embedmind_core::api::{MemoryDraft, Query, Scope, Store, StoreOptions};
use embedmind_core::storage::sim::SimVfs;
use embedmind_core::storage::vfs::Vfs;
use embedmind_core::storage::{Pager, PagerOptions};
use embedmind_core::{Result, embed::OnnxEmbedder};

const STORE: &str = "memory.mind";

/// A store on an in-memory VFS with the real embedded model — the same code
/// path `Store::create` uses, but without touching the real filesystem so the
/// tests stay hermetic and parallel-safe.
fn store() -> (Arc<dyn Vfs>, Store) {
    let embedder = Arc::new(OnnxEmbedder::load().expect("embedded model must load"));
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let opts = StoreOptions {
        embedder: Some(embedder),
        ..StoreOptions::default()
    };
    let store = Store::create_with(Arc::clone(&vfs), Path::new(STORE), opts).unwrap();
    (vfs, store)
}

fn reopen(vfs: Arc<dyn Vfs>) -> Store {
    let embedder = Arc::new(OnnxEmbedder::load().expect("embedded model must load"));
    let opts = StoreOptions {
        embedder: Some(embedder),
        ..StoreOptions::default()
    };
    Store::open_with(vfs, Path::new(STORE), opts).unwrap()
}

#[test]
fn recall_ranks_semantically_closest_first() {
    let (_vfs, mut store) = store();
    store
        .remember(MemoryDraft::new("the cat sat quietly on the warm mat"))
        .unwrap();
    store
        .remember(MemoryDraft::new("quarterly corporate tax filing deadline"))
        .unwrap();
    let feline = store
        .remember(MemoryDraft::new("a kitten was sleeping on the rug"))
        .unwrap();

    let hits = store
        .recall(Query::new("a small feline resting").limit(3))
        .unwrap();
    assert!(!hits.is_empty(), "recall must return the indexed memories");
    // The two cat-related memories should outrank the tax one; the closest
    // hit is one of them.
    assert!(
        hits[0].content.contains("cat") || hits[0].id == feline.id,
        "top hit should be cat-related, got {:?}",
        hits[0].content
    );
    assert!(
        hits.iter().any(|h| h.id == feline.id),
        "the kitten memory should be among the top hits"
    );
}

#[test]
fn recall_excludes_forgotten_memories() {
    let (_vfs, mut store) = store();
    let doomed = store
        .remember(MemoryDraft::new("temporary note about database indexes"))
        .unwrap();
    store
        .remember(MemoryDraft::new("permanent note about database indexes"))
        .unwrap();

    assert!(store.forget(doomed.id).unwrap());
    let hits = store
        .recall(Query::new("database indexes").limit(10))
        .unwrap();
    assert!(
        hits.iter().all(|h| h.id != doomed.id),
        "forgotten memory must never appear in recall"
    );
    assert!(
        hits.iter().any(|h| h.content.starts_with("permanent")),
        "the live memory must still be recallable"
    );
}

#[test]
fn recall_scope_filters_by_project() {
    let (_vfs, mut store) = store();
    store
        .remember(MemoryDraft::new("uses tokio for async").project("alpha"))
        .unwrap();
    store
        .remember(MemoryDraft::new("uses tokio for async").project("beta"))
        .unwrap();

    let alpha = store
        .recall(
            Query::new("async runtime")
                .scope(Scope::Project("alpha".into()))
                .limit(10),
        )
        .unwrap();
    assert!(!alpha.is_empty());
    assert!(
        alpha.iter().all(|h| h.project.as_deref() == Some("alpha")),
        "project scope must exclude other projects"
    );

    let all = store.recall(Query::new("async runtime").limit(10)).unwrap();
    assert!(
        all.len() >= 2,
        "Scope::All must see memories across projects"
    );
}

#[test]
fn recall_survives_reopen() {
    let (vfs, mut store) = store();
    for i in 0..20 {
        store
            .remember(MemoryDraft::new(format!(
                "memory number {i} about rust and systems"
            )))
            .unwrap();
    }
    let probe = store
        .remember(MemoryDraft::new(
            "the founder prefers explicit typed errors over panics",
        ))
        .unwrap();
    store.close().unwrap();

    let store = reopen(vfs);
    let hits = store
        .recall(Query::new("explicit error handling without panic").limit(5))
        .unwrap();
    assert!(
        hits.iter().any(|h| h.id == probe.id),
        "the distinctive memory must be recallable after reopen"
    );
}

/// DESIGN §6: a memory longer than one 512-token window is chunked at the
/// index level — content buried deep in the text (far past the first window)
/// must still be recallable, and the memory must come back whole, exactly
/// once.
#[test]
fn recall_finds_content_past_the_first_window_via_chunking() {
    let (_vfs, mut store) = store();

    // ~700 tokens of filler, then the distinctive fact only the second
    // chunk can see (the first window covers ~510 tokens).
    let filler = "the meeting notes continued with routine status updates ".repeat(100);
    let needle = "the production database password rotation happens every thursday at noon";
    let long = format!("{filler} {needle}");
    let chunked = store.remember(MemoryDraft::new(long.clone())).unwrap();
    store
        .remember(MemoryDraft::new("grocery list: apples, bread, coffee"))
        .unwrap();

    let hits = store
        .recall(Query::new("when does the database password rotate").limit(5))
        .unwrap();
    assert!(
        hits.iter().any(|h| h.id == chunked.id),
        "content past the first token window must be recallable"
    );
    let occurrences = hits.iter().filter(|h| h.id == chunked.id).count();
    assert_eq!(
        occurrences, 1,
        "a chunked memory must be returned once, not once per chunk"
    );
    let hit = hits.iter().find(|h| h.id == chunked.id).unwrap();
    assert_eq!(
        hit.content, long,
        "recall returns the whole memory, never a chunk"
    );
}

/// S9 edge (docs/01-spec.md): a `.mind` written before the full-text index
/// existed presents `fts_root_page == 0` in its header. Opening such a file
/// and calling `recall` must degrade to vector-only — valid hits by vector
/// similarity, the degradation reported via [`RecallOutcome`]
/// (`degraded_to_vector_only`) so the shells can warn, never a typed error.
#[test]
fn legacy_file_without_fts_index_recalls_vector_only_with_warning_flag() {
    let (vfs, mut store) = store();
    let feline = store
        .remember(MemoryDraft::new("a kitten was sleeping on the rug"))
        .unwrap();
    store
        .remember(MemoryDraft::new("quarterly corporate tax filing deadline"))
        .unwrap();
    store.close().unwrap();

    // Rewind the file to the pre-M2 shape: drop the header's full-text root
    // pointer. The fts pages become unreferenced leftovers, which is exactly
    // how detection works on a real legacy file — it keys on the pointer,
    // never on the pages.
    let mut pager =
        Pager::open(Arc::clone(&vfs), Path::new(STORE), PagerOptions::default()).unwrap();
    let mut txn = pager.begin().unwrap();
    txn.set_fts_root_page(0);
    txn.commit().unwrap();
    pager.close().unwrap();

    let store = reopen(vfs);
    let outcome = store
        .recall_detailed(Query::new("a small feline resting").limit(5))
        .expect("recall on a legacy file must degrade, never error");
    assert!(
        outcome.degraded_to_vector_only,
        "the missing fts index must be reported so shells can warn"
    );
    assert!(
        outcome.hits.iter().any(|h| h.id == feline.id),
        "vector similarity must still find the semantically close memory"
    );
    // The plain `recall` path hides the flag but must return the same hits.
    let hits = store
        .recall(Query::new("a small feline resting").limit(5))
        .unwrap();
    assert_eq!(
        hits.iter().map(|h| h.id).collect::<Vec<_>>(),
        outcome.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
        "recall and recall_detailed must agree on a legacy file"
    );
}

#[test]
fn recall_without_embedder_is_a_typed_error() {
    // A KV-only store (no embedder) must reject recall clearly, not panic.
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let mut store = Store::create_with(vfs, Path::new(STORE), StoreOptions::default()).unwrap();
    store
        .remember(MemoryDraft::new("stored without a vector"))
        .unwrap();
    let err = store.recall(Query::new("anything")).unwrap_err();
    assert!(
        matches!(err, embedmind_core::Error::InvalidArgument(_)),
        "recall on an embedder-less store must be InvalidArgument, got {err:?}"
    );
}

/// S20 golden case (`docs/01-spec.md`): a fact and its correction, phrased
/// distinctly enough that each wins rank 0 in a different content list
/// (vector vs. text) — a genuine content tie, since each has one rank-0 and
/// one rank-1 contribution, identical either way. With `recency` on, the tie
/// breaks toward the newer memory (the correction).
#[test]
fn recency_breaks_a_genuine_content_tie_toward_the_newer_memory() {
    let (_vfs, mut store) = store();
    let original = store
        .remember(MemoryDraft::new(
            "the deploy runbook says restart the worker process before the scheduler process",
        ))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let correction = store
        .remember(MemoryDraft::new(
            "deploy runbook correction: restart the scheduler process before the worker process",
        ))
        .unwrap();

    let hits = store
        .recall(
            Query::new("deploy runbook restart order worker scheduler process")
                .recency(true)
                .limit(5),
        )
        .unwrap();
    assert!(hits.len() >= 2, "both memories must be recalled");
    let without_recency = store
        .recall(Query::new("deploy runbook restart order worker scheduler process").limit(5))
        .unwrap();
    // This golden case only holds when the two memories are a genuine content
    // tie (equal fused score without recency) — assert the precondition so a
    // future embedding-model change fails loudly here instead of silently
    // testing the wrong scenario.
    let score_of = |hits: &[_], id| {
        hits.iter()
            .find(|h: &&embedmind_core::api::Recalled| h.id == id)
            .map(|h| h.score)
    };
    assert_eq!(
        score_of(&without_recency, original.id),
        score_of(&without_recency, correction.id),
        "precondition: this golden case needs a genuine content-score tie"
    );
    assert_eq!(
        hits[0].id, correction.id,
        "a genuine content tie must break toward the newer memory when recency is on"
    );
    assert_eq!(hits[1].id, original.id);
}

/// S20 edge case (`docs/01-spec.md`): word-for-word identical content is *not*
/// the "tied relevance" scenario recency is meant to break — with identical
/// text, the store's own vector/BM25 indexes consistently rank the
/// first-inserted memory at rank 0 in *both* content lists (two content
/// contributions), which by the RRF property this story preserves must beat
/// a rank-1-content-plus-rank-0-recency challenger (`recall.rs` module docs,
/// `old_strong_match_beats_new_weak_match`). So identical re-statements are
/// exactly the "strong old match must not be displaced" case, not the
/// "genuine tie" case — this test locks in that observed, correct behavior.
#[test]
fn identical_restatement_does_not_displace_the_first_recorded_one() {
    let (_vfs, mut store) = store();
    let text = "the deploy runbook says restart the worker before the scheduler";
    let original = store.remember(MemoryDraft::new(text)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store.remember(MemoryDraft::new(text)).unwrap();

    let hits = store
        .recall(
            Query::new("deploy runbook restart order worker scheduler")
                .recency(true)
                .limit(5),
        )
        .unwrap();
    assert!(hits.len() >= 2, "both identical memories must be recalled");
    assert_eq!(
        hits[0].id, original.id,
        "the memory ranked first in both content lists must not be displaced by recency alone"
    );
}

/// S20 golden case: a strong old match (semantically on-topic, established)
/// must not be displaced by a newer memory that is only weakly related — the
/// recency list can break ties, never invent relevance.
#[test]
fn recency_never_displaces_a_strong_old_match_with_a_weak_new_one() {
    let (_vfs, mut store) = store();
    let strong_old = store
        .remember(MemoryDraft::new(
            "postgres connection pool exhaustion causes request timeouts under load",
        ))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .remember(MemoryDraft::new(
            "the office coffee machine was replaced last week",
        ))
        .unwrap();

    let hits = store
        .recall(
            Query::new("postgres connection pool exhaustion timeouts")
                .recency(true)
                .limit(5),
        )
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(
        hits[0].id, strong_old.id,
        "a strong content match must win regardless of a newer, unrelated memory"
    );
}

/// S20: recency defaults off — enabling it must never change results for a
/// query whose content ranking already has a clear winner (no tie to break).
#[test]
fn recency_defaults_off_and_is_a_pure_opt_in() {
    let (_vfs, mut store) = store();
    store
        .remember(MemoryDraft::new("the cat sat quietly on the warm mat"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .remember(MemoryDraft::new("quarterly corporate tax filing deadline"))
        .unwrap();

    let without = store
        .recall(Query::new("a small feline resting").limit(5))
        .unwrap();
    let with = store
        .recall(Query::new("a small feline resting").recency(true).limit(5))
        .unwrap();
    assert_eq!(
        without.iter().map(|h| h.id).collect::<Vec<_>>(),
        with.iter().map(|h| h.id).collect::<Vec<_>>(),
        "recency only breaks ties; a clear content winner is unaffected"
    );
}

/// S20 edge: the recency list must respect the superseded exclusion (S19/FR1)
/// like every other list — a superseded memory never resurfaces via recency,
/// no matter how the fusion reorders the candidates.
#[test]
fn recency_never_resurfaces_a_superseded_memory() {
    let (_vfs, mut store) = store();
    let old_fact = store
        .remember(MemoryDraft::new(
            "the api gateway timeout is thirty seconds",
        ))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let correction = store
        .remember(
            MemoryDraft::new("the api gateway timeout is sixty seconds")
                .supersedes(vec![old_fact.id]),
        )
        .unwrap();

    let hits = store
        .recall(Query::new("api gateway timeout").recency(true).limit(10))
        .unwrap();
    assert!(
        hits.iter().all(|h| h.id != old_fact.id),
        "a superseded memory must never come back through the recency list"
    );
    assert!(hits.iter().any(|h| h.id == correction.id));
}

/// S20 edge: the recency list must respect the same scope filter as the rest
/// of recall — a memory excluded by project scope must never resurface just
/// because it is the newest thing in the file.
#[test]
fn recency_respects_scope_and_never_reintroduces_an_excluded_memory() {
    let (_vfs, mut store) = store();
    let alpha = store
        .remember(MemoryDraft::new("uses tokio for async runtime").project("alpha"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    store
        .remember(MemoryDraft::new("uses tokio for async runtime").project("beta"))
        .unwrap();

    let hits = store
        .recall(
            Query::new("async runtime")
                .scope(Scope::Project("alpha".into()))
                .recency(true)
                .limit(10),
        )
        .unwrap();
    assert!(
        hits.iter().all(|h| h.project.as_deref() == Some("alpha")),
        "recency must not pull in the newer beta-scoped memory across scope"
    );
    assert!(hits.iter().any(|h| h.id == alpha.id));
}

#[test]
fn reopening_with_mismatched_model_is_refused() -> Result<()> {
    // Header records model id + dims; opening the same file with a different
    // model must be refused (docs/adr/0004), never silently mixed.
    struct FakeEmbedder;
    impl embedmind_core::embed::Embedder for FakeEmbedder {
        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![0.1; 384])
        }
        fn id(&self) -> embedmind_core::embed::ModelId {
            "totally-different-model"
        }
        fn dims(&self) -> u16 {
            384
        }
    }

    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let real = Arc::new(OnnxEmbedder::load().expect("model must load"));
    let opts = StoreOptions {
        embedder: Some(real),
        ..StoreOptions::default()
    };
    Store::create_with(Arc::clone(&vfs), Path::new(STORE), opts)?.close()?;

    let opts = StoreOptions {
        embedder: Some(Arc::new(FakeEmbedder)),
        ..StoreOptions::default()
    };
    // `Store` is intentionally not `Debug`, so match instead of `unwrap_err`.
    match Store::open_with(vfs, Path::new(STORE), opts) {
        Ok(_) => panic!("mismatched model must be refused"),
        Err(embedmind_core::Error::InvalidArgument(_)) => {}
        Err(other) => panic!("expected InvalidArgument, got {other:?}"),
    }
    Ok(())
}
