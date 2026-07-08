//! End-to-end smoke test of the benchmark harness (A3 part 1 done-criterion:
//! "baseline brute-force runs and produces a reference recall@10").
//!
//! Runs the whole pipeline in miniature — deterministic corpus → real ONNX
//! embeddings → HNSW store + brute-force baseline over the same vectors →
//! recall@10 — on a small in-memory store, so `cargo test --workspace` stays
//! fast while still exercising the real model and the real index. The full
//! 10k/100k runs live behind the `gen_dataset`/`baseline` binaries.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use embedmind_bench::baseline;
use embedmind_bench::corpus;
use embedmind_bench::dataset::{VectorSet, ingest_corpus};
use embedmind_bench::recall;
use embedmind_core::api::{Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::index::normalize;
use embedmind_core::storage::sim::SimVfs;
use embedmind_core::storage::vfs::Vfs;

/// A small in-memory store populated from a deterministic corpus, plus the
/// parallel vector set for the brute-force baseline.
fn small_store(n: usize) -> (Arc<dyn Embedder>, Store, VectorSet) {
    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::load().expect("model must load"));
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let opts = StoreOptions {
        embedder: Some(Arc::clone(&embedder)),
        ..StoreOptions::default()
    };
    let mut store = Store::create_with(vfs, Path::new("bench.mind"), opts).unwrap();
    let memories = corpus::generate(0xABCD_1234, n);
    let set = ingest_corpus(&mut store, embedder.as_ref(), &memories).unwrap();
    (embedder, store, set)
}

#[test]
fn brute_force_baseline_produces_a_recall_at_10_reference() {
    let (embedder, store, set) = small_store(400);
    let queries = corpus::generate(0x9999_7777, 30)
        .into_iter()
        .map(|m| m.content)
        .collect::<Vec<_>>();

    let report = recall::measure(&store, &set, embedder.as_ref(), &queries, 10).unwrap();

    assert_eq!(report.queries, 30);
    assert_eq!(report.k, 10);
    // HNSW is approximate but on a few hundred vectors with the default
    // ef_search it should track the exact top-10 closely — the same >= 0.9
    // bar the core's own recall property test holds itself to (TESTING.md §4).
    assert!(
        report.recall_at_k >= 0.9,
        "recall@10 = {} below 0.9 (harness or index regression)",
        report.recall_at_k
    );
    assert!(report.min_recall >= 0.0 && report.recall_at_k <= 1.0);
}

#[test]
fn store_and_baseline_agree_on_a_stored_memory() {
    // Sanity floor: querying with a memory that IS in the store must put that
    // memory at the top for both the exact scan and the HNSW index — if this
    // fails the two are not looking at the same vectors.
    let (embedder, store, set) = small_store(200);
    let probe = &set.entries[42];

    let exact = baseline::top_k(&set, &probe.vector, 1, |_| true);
    assert_eq!(exact[0].record_id, probe.id, "a vector is its own exact NN");

    // Recall the memory's own text (embedded the same way) — it must come back.
    let stored = store.get(probe.id).unwrap().unwrap();
    let mut qv = embedder.embed(&stored.content).unwrap();
    normalize(&mut qv);
    let hits = store
        .recall(Query::new(stored.content.clone()).limit(5))
        .unwrap();
    assert!(
        hits.iter().any(|h| h.id == probe.id),
        "the store must recall a memory by its own content"
    );
}

#[test]
fn recall_query_texts_are_deterministic_and_distinct_from_corpus() {
    let spec = embedmind_bench::dataset::DatasetSpec::by_name("agent-mem-10k").unwrap();
    let a = recall::query_texts(spec, 50);
    let b = recall::query_texts(spec, 50);
    assert_eq!(a, b, "query texts must be reproducible");
    // Different seed namespace than the corpus, so not a verbatim copy.
    let stored: Vec<String> = corpus::generate(spec.seed, 50)
        .into_iter()
        .map(|m| m.content)
        .collect();
    assert_ne!(
        a, stored,
        "queries must not be identical to stored memories"
    );
}
