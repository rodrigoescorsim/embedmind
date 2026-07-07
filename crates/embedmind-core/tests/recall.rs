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
