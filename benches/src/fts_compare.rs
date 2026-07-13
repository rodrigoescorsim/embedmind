//! Full-text-only (BM25) comparison: EmbedMind's own inverted index
//! (`Store::search_text`, `docs/adr/0011`) vs. tantivy — the external
//! measurement ADR 0011 never had a number for (founder review 2026-07-13).
//!
//! ADR 0011 rejected embedding tantivy for an **architectural** reason: it
//! writes its own segments outside the `.mind` file and commits on its own
//! schedule, which would give the engine two independent sources of commit
//! truth — exactly the "unrecoverable half-state after a crash" the WAL-single
//! design (CLAUDE.md decision 4) exists to rule out. That decision does not
//! depend on which engine is faster; this module exists only to put a number
//! on the tradeoff already made, not to reopen it. See
//! [`crate::report::render_fts_only_table`] for the wording that keeps this
//! explicit in the rendered table.
//!
//! Scope is full-text **only**: EmbedMind's keyword half in isolation
//! (`Store::search_text`, no RRF/vector fusion — the same isolation
//! `Store::recall_vector` gives the vector half in [`crate::recall`]), against
//! tantivy's own BM25 query, on the *same* corpus and the *same* queries. This
//! is a different plane than [`crate::lexical`] (which measures hybrid vs.
//! vector-only *inside* EmbedMind); here both sides are full-text engines.
//!
//! Ground truth reuses [`crate::lexical::generate_cases`]: each case's literal
//! is guaranteed to occur in exactly one document, so both engines are graded
//! against an unambiguous target — no brute-force oracle needed, unlike the
//! vector comparisons in [`crate::competitors`].

use std::time::Instant;

use embedmind_core::api::{MemoryDraft, Query, Store};

use crate::lexical::LexicalCase;
use crate::metrics::{self, Latencies};

/// One full-text engine's measured numbers on the comparison. Mirrors
/// [`crate::competitors::CompetitorMetrics`]'s shape (optional fields, `None`
/// rendered as `—`) but scoped to what a full-text-only comparison actually
/// measures — no `recall_at_10` vs. a vector baseline, since there is none
/// here; recall is vs. the lexical ground truth instead.
#[derive(Debug, Clone, Default)]
pub struct FtsMetrics {
    /// Fraction of lexical queries whose ground-truth document was returned
    /// (top-k, same `k` both engines are queried with).
    pub recall_at_k: Option<f64>,
    /// Warm query latency p50 / p99, milliseconds.
    pub query_p50_ms: Option<f64>,
    pub query_p99_ms: Option<f64>,
    /// Ingest throughput, documents/sec (text only — no embedding on either
    /// side of this comparison).
    pub ingest_per_sec: Option<f64>,
    /// On-disk index size after ingest, bytes.
    pub index_bytes: Option<u64>,
}

/// Outcome of attempting a full-text engine's measurement: real numbers, or an
/// honest record of why they are absent — same contract as
/// [`crate::competitors::CompetitorOutcome`], never a fabricated row.
#[derive(Debug, Clone)]
pub enum FtsOutcome {
    Measured(FtsMetrics),
    NotMeasured { reason: String },
}

/// What a full-text engine's query returns and what its ingest persists
/// (`docs/BENCHMARKS.md` §4 rule 6) — declared per system, same rule the
/// vector comparisons in [`crate::competitors`] already follow.
#[derive(Debug, Clone, Copy)]
pub struct FtsScope {
    pub returns: &'static str,
    pub persists: &'static str,
}

/// EmbedMind's own scope on this plane: `search_text` returns full
/// `Recalled` records (content + metadata), not just an id/score pair —
/// unlike tantivy, which returns doc id + score and leaves loading the
/// document to the caller.
pub const EMBEDMIND_FTS_SCOPE: FtsScope = FtsScope {
    returns: "full content + metadata (Recalled records)",
    persists: "text + metadata + full-text index (same .mind file, WAL-covered)",
};

/// tantivy's pinned target version, recorded here so the table's version cell
/// always matches the crate this harness targets, whether or not the adapter
/// ran (same pattern as [`crate::competitors::COMPETITORS`]).
pub const TANTIVY_VERSION: &str = "0.26.1";

/// tantivy's scope: a query returns doc id + BM25 score only, never the
/// document content itself — the caller re-fetches from wherever it stored
/// the source text (tantivy is index-only, no document store of its own by
/// default).
pub const TANTIVY_SCOPE: FtsScope = FtsScope {
    returns: "doc id + BM25 score only (no content store)",
    persists: "tokenized postings only (own segment files, outside any .mind)",
};

/// Runs EmbedMind's own full-text-only measurement: ingests `cases` into
/// `store` (one `remember` per case, same write path [`crate::lexical::ingest_cases`]
/// uses) and queries each literal through [`Store::search_text`] — the keyword
/// half in isolation, no RRF/vector fusion. Always [`FtsOutcome::Measured`]:
/// unlike an external competitor, the engine is always present to measure.
pub fn run_embedmind(store: &mut Store, cases: &[LexicalCase], k: usize) -> FtsOutcome {
    let mut run = || -> embedmind_core::Result<FtsMetrics> {
        let mut ingest_lat = Latencies::with_capacity(cases.len());
        let ingest_started = Instant::now();
        let mut ids = Vec::with_capacity(cases.len());
        for case in cases {
            let started = Instant::now();
            let stored = store.remember(
                MemoryDraft::new(case.content.clone())
                    .project(case.project.clone())
                    .agent("bench-fts-compare"),
            )?;
            ingest_lat.push(started.elapsed());
            ids.push(stored.id);
        }
        let ingest_per_sec = metrics::ops_per_sec(cases.len(), ingest_started.elapsed());

        let mut hits = 0usize;
        let mut warm = Latencies::with_capacity(cases.len());
        for (case, &target_id) in cases.iter().zip(&ids) {
            let started = Instant::now();
            let results = store.search_text(Query::new(case.literal.clone()).limit(k))?;
            warm.push(started.elapsed());
            if results.iter().any(|r| r.id == target_id) {
                hits += 1;
            }
        }

        let stats = store.stats()?;

        for id in &ids {
            store.forget(*id)?;
        }

        let n = cases.len().max(1);
        Ok(FtsMetrics {
            recall_at_k: Some(hits as f64 / n as f64),
            query_p50_ms: warm.p50_ms(),
            query_p99_ms: warm.p99_ms(),
            ingest_per_sec: Some(ingest_per_sec),
            index_bytes: Some(stats.file_bytes),
        })
    };

    match run() {
        Ok(m) => FtsOutcome::Measured(m),
        Err(e) => FtsOutcome::NotMeasured {
            reason: format!("EmbedMind search_text measurement failed: {e}"),
        },
    }
}

/// tantivy adapter. Real implementation lives behind `--features
/// compare-tantivy` (pure Rust, no native toolchain — the simplest of the
/// three competitor adapters to build). Without the feature it records why.
#[cfg(not(feature = "compare-tantivy"))]
pub fn run_tantivy(_cases: &[LexicalCase], _k: usize) -> FtsOutcome {
    FtsOutcome::NotMeasured {
        reason: format!(
            "feature `compare-tantivy` disabled (build with it to fill this row; target tantivy {TANTIVY_VERSION})"
        ),
    }
}

/// tantivy adapter: a single-field text schema (`content`, indexed + stored so
/// recall can be checked against the same document set) with its default
/// `TEXT` tokenizer and its built-in BM25 scorer (`Query::search` with a
/// `TopDocs` collector) — no de-tuning, same rule [`crate::competitors`]
/// follows for sqlite-vec/zvec/Chroma. Ingests one document at a time (the
/// fair comparison to EmbedMind's one-at-a-time `remember`), then commits once
/// before querying (tantivy's documented ingest pattern: writes are buffered
/// until `commit`, unlike EmbedMind's per-`remember` WAL commit — noted in the
/// table, not hidden, since it is a real difference in durability guarantees).
#[cfg(feature = "compare-tantivy")]
pub fn run_tantivy(cases: &[LexicalCase], k: usize) -> FtsOutcome {
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::{STORED, Schema, TEXT, Value};
    use tantivy::{Index, IndexWriter, TantivyDocument};

    let dir = std::env::temp_dir().join(format!("embedmind-bench-tantivy-{k}"));
    let _ = std::fs::remove_dir_all(&dir);

    let run = || -> tantivy::Result<FtsMetrics> {
        std::fs::create_dir_all(&dir)?;

        let mut schema_builder = Schema::builder();
        let content_field = schema_builder.add_text_field("content", TEXT | STORED);
        let literal_field = schema_builder.add_text_field("literal", STORED);
        let schema = schema_builder.build();

        let index = Index::create_in_dir(&dir, schema.clone())?;
        let mut writer: IndexWriter = index.writer(50_000_000)?;

        // --- ingest, one document at a time (fair comparison to `remember`) ---
        let mut ingest_lat = Latencies::with_capacity(cases.len());
        let ingest_started = Instant::now();
        for case in cases {
            let started = Instant::now();
            let mut doc = TantivyDocument::default();
            doc.add_text(content_field, &case.content);
            doc.add_text(literal_field, &case.literal);
            writer.add_document(doc)?;
            ingest_lat.push(started.elapsed());
        }
        writer.commit()?;
        let ingest_per_sec = metrics::ops_per_sec(cases.len(), ingest_started.elapsed());

        let reader = index.reader()?;
        let searcher = reader.searcher();
        let query_parser = QueryParser::for_index(&index, vec![content_field]);

        // --- recall@k + warm query latency, same literal queries EmbedMind is
        // graded on ---
        let mut hits = 0usize;
        let mut warm = Latencies::with_capacity(cases.len());
        for case in cases {
            let started = Instant::now();
            // tantivy's query parser treats a literal containing `:`/`-`/etc.
            // (CLI flags, hex hashes) as syntax; escaping keeps every literal a
            // plain phrase query, since this comparison measures BM25 ranking,
            // not query-syntax parsing.
            let escaped: String = case
                .literal
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { ' ' })
                .collect();
            let found = if let Ok(parsed) = query_parser.parse_query(&escaped) {
                let collector = TopDocs::with_limit(k).order_by_score();
                let top_docs = searcher.search(&parsed, &collector)?;
                top_docs.iter().any(|(_, addr)| {
                    searcher
                        .doc::<TantivyDocument>(*addr)
                        .ok()
                        .and_then(|d| {
                            d.get_first(literal_field)
                                .and_then(|v| v.as_str())
                                .map(|s| s == case.literal)
                        })
                        .unwrap_or(false)
                })
            } else {
                false
            };
            warm.push(started.elapsed());
            if found {
                hits += 1;
            }
        }

        let index_bytes = dir_size(&dir);

        let n = cases.len().max(1);
        Ok(FtsMetrics {
            recall_at_k: Some(hits as f64 / n as f64),
            query_p50_ms: warm.p50_ms(),
            query_p99_ms: warm.p99_ms(),
            ingest_per_sec: Some(ingest_per_sec),
            index_bytes,
        })
    };

    let outcome = match run() {
        Ok(m) => FtsOutcome::Measured(m),
        Err(e) => FtsOutcome::NotMeasured {
            reason: format!("tantivy adapter failed: {e} (target {TANTIVY_VERSION})"),
        },
    };
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Total on-disk size of everything tantivy wrote for the index — like zvec
/// (`crate::competitors::dir_size`), it stores segments as a directory of
/// files, not a single file.
#[cfg(feature = "compare-tantivy")]
fn dir_size(dir: &std::path::Path) -> Option<u64> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    stack.push(path);
                } else {
                    total += meta.len();
                }
            }
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use embedmind_core::api::StoreOptions;
    use embedmind_core::storage::sim::SimVfs;
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn embedmind_fts_measures_perfect_recall_on_literal_queries() {
        let opts = StoreOptions::default();
        let mut store = Store::create_with(
            Arc::new(SimVfs::new()),
            Path::new("fts-compare-test.mind"),
            opts,
        )
        .expect("create store");

        let cases = crate::lexical::generate_cases(0xF7C0_1234, 12);
        let outcome = run_embedmind(&mut store, &cases, 10);
        match outcome {
            FtsOutcome::Measured(m) => {
                assert_eq!(
                    m.recall_at_k,
                    Some(1.0),
                    "literal queries must be a perfect BM25 anchor"
                );
                assert!(m.query_p50_ms.is_some());
                assert!(m.ingest_per_sec.is_some());
            }
            FtsOutcome::NotMeasured { reason } => panic!("expected Measured, got: {reason}"),
        }
    }

    #[test]
    fn embedmind_fts_cleans_up_after_itself() {
        // The cases must not linger in the store after measurement — the
        // adapter is meant to be run against a real materialized dataset
        // without polluting it (same contract as `crate::lexical`'s
        // ingest-then-forget in `harness::run_suite`).
        let opts = StoreOptions::default();
        let mut store = Store::create_with(
            Arc::new(SimVfs::new()),
            Path::new("fts-compare-test2.mind"),
            opts,
        )
        .expect("create store");

        let cases = crate::lexical::generate_cases(0xABCD, 5);
        let _ = run_embedmind(&mut store, &cases, 10);

        for case in &cases {
            let results = store
                .search_text(Query::new(case.literal.clone()).limit(10))
                .unwrap();
            assert!(
                results.is_empty(),
                "case for literal {:?} was not forgotten after measurement",
                case.literal
            );
        }
    }

    #[test]
    fn tantivy_feature_off_reports_not_measured_with_pinned_version() {
        if cfg!(feature = "compare-tantivy") {
            return;
        }
        let cases = crate::lexical::generate_cases(0x1, 3);
        let outcome = run_tantivy(&cases, 10);
        match outcome {
            FtsOutcome::NotMeasured { reason } => {
                assert!(reason.contains("compare-tantivy"));
                assert!(reason.contains(TANTIVY_VERSION));
            }
            FtsOutcome::Measured(_) => panic!("expected NotMeasured with the feature off"),
        }
    }

    #[cfg(feature = "compare-tantivy")]
    #[test]
    fn tantivy_measures_perfect_recall_on_literal_queries() {
        let cases = crate::lexical::generate_cases(0xF7C0_5678, 12);
        let outcome = run_tantivy(&cases, 10);
        match outcome {
            FtsOutcome::Measured(m) => {
                assert_eq!(m.recall_at_k, Some(1.0));
                assert!(m.query_p50_ms.is_some());
                assert!(m.index_bytes.is_some());
            }
            FtsOutcome::NotMeasured { reason } => panic!("expected Measured, got: {reason}"),
        }
    }

    #[test]
    fn every_scope_states_returns_and_persists() {
        for scope in [EMBEDMIND_FTS_SCOPE, TANTIVY_SCOPE] {
            assert!(!scope.returns.is_empty());
            assert!(!scope.persists.is_empty());
        }
    }
}
