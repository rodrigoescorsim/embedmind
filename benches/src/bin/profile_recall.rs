//! FTOPT-5 (`docs/adr/0017`): confirmatory phase-by-phase profiling of the
//! **post-sidecar** hybrid `recall` path on a real, already-vacuumed
//! (`format_version` 7) dataset.
//!
//! FTOPT-0 profiled the full-text half in isolation, on the pre-FTOPT-1
//! linear scan ([`embedmind_core::index::fts::search_profiled`]) — that scan
//! is no longer what production `recall` runs on a `format_version` ≥ 6
//! file, and its `keep`/`doc_len` closures no longer reload the whole record
//! (FTOPT-1 moved both onto the filter-meta sidecar, `docs/adr/0027`). This
//! binary instruments the *actual* hot path instead:
//! `Store::recall_profiled` mirrors `Store::recall_detailed` exactly (HNSW
//! vector search, `index::fts::search_bmw_profiled` — the BlockMax-WAND path
//! `search` dispatches to on this dataset, RRF fusion, final hit load), so
//! this measures where post-sidecar hybrid recall spends its time rather
//! than assuming FTOPT-0's finding still applies.
//!
//! Uses the same general (non-lexical-only) query sample the bench harness
//! and `profile_fts` already draw from (`recall::query_texts`) — not the
//! lexical-only ground-truth queries — because the ADR 0017 open question
//! this measures is why the *hybrid* p99 (135.74 ms, official 2026-07-14
//! run) is ~4x the lexical-only p99 (37.71 ms).
//!
//! ```text
//! cargo run -p embedmind-bench --release --bin profile_recall -- agent-mem-100k
//! ```
//!
//! Read-only measurement: no engine behavior changes, nothing here is called
//! by `Store::recall`/`Store::search_text` in production.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use embedmind_bench::dataset::DatasetSpec;
use embedmind_bench::{default_data_dir, recall};
use embedmind_core::api::{Query, RecallPhaseTimings, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::storage::vfs::RealVfs;

/// Same `k` the harness reports (`docs/BENCHMARKS.md` §3).
const K: usize = 10;

/// Warm-up queries before timing starts — grades warm-cache latency, matching
/// the harness and `profile_fts` methodology.
const WARMUP_QUERIES: usize = 50;

/// Timed queries — matches `profile_fts`'s `TIMED_QUERIES` so aggregate
/// phase totals are directly comparable to `query_engine_p50/p99_ms` from a
/// `run_all.sh --full` run.
const TIMED_QUERIES: usize = 1000;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("profile_recall failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "agent-mem-100k".to_owned());
    let Some(spec) = DatasetSpec::by_name(&name) else {
        eprintln!("unknown dataset '{name}'");
        return Ok(ExitCode::FAILURE);
    };

    let data_dir = default_data_dir();
    let mind_path = spec.mind_path(&data_dir);
    if !mind_path.exists() {
        eprintln!(
            "{} not materialized — run `cargo run -p embedmind-bench --release --bin gen_dataset -- {}` first",
            mind_path.display(),
            spec.name
        );
        return Ok(ExitCode::FAILURE);
    }

    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::load()?);
    let opts = StoreOptions {
        embedder: Some(Arc::clone(&embedder)),
        ..StoreOptions::default()
    };
    let store = Store::open_with(Arc::new(RealVfs), &mind_path, opts)?;

    let texts = recall::query_texts(spec, WARMUP_QUERIES + TIMED_QUERIES);
    let (warmup, timed) = texts.split_at(WARMUP_QUERIES);

    eprintln!("[{}] warming up ({WARMUP_QUERIES} queries)...", spec.name);
    for t in warmup {
        let _ = store.recall_profiled(Query::new(t.clone()).limit(K))?;
    }

    eprintln!("[{}] timing {TIMED_QUERIES} queries...", spec.name);
    let mut totals = PhaseTotals::default();
    let mut wall_ns: Vec<u64> = Vec::with_capacity(TIMED_QUERIES);
    for (i, t) in timed.iter().enumerate() {
        let started = Instant::now();
        let (_, timings) = store.recall_profiled(Query::new(t.clone()).limit(K))?;
        wall_ns.push(started.elapsed().as_nanos() as u64);
        totals.add(&timings);
        if (i + 1) % 250 == 0 {
            eprintln!("  {}/{TIMED_QUERIES}", i + 1);
        }
    }
    store.close()?;

    totals.report(spec.name, TIMED_QUERIES, &mut wall_ns);
    Ok(ExitCode::SUCCESS)
}

/// Sums of every phase across all timed queries — aggregate share, not a
/// per-query distribution (same reporting shape as `profile_fts`).
#[derive(Default)]
struct PhaseTotals {
    vector_ns: u64,
    cursor_open_ns: u64,
    bound_ns: u64,
    decode_ns: u64,
    keep_ns: u64,
    doc_len_ns: u64,
    scoring_ns: u64,
    fuse_ns: u64,
    hit_load_ns: u64,
    docs_evaluated: u64,
    pivot_skips: u64,
    blocks_decoded: u64,
    blocks_skipped: u64,
}

impl PhaseTotals {
    fn add(&mut self, t: &RecallPhaseTimings) {
        self.vector_ns += t.vector_ns;
        self.cursor_open_ns += t.fts.cursor_open_ns;
        self.bound_ns += t.fts.bound_ns;
        self.decode_ns += t.fts.decode_ns;
        self.keep_ns += t.fts.keep_ns;
        self.doc_len_ns += t.fts.doc_len_ns;
        self.scoring_ns += t.fts.scoring_ns;
        self.fuse_ns += t.fuse_ns;
        self.hit_load_ns += t.hit_load_ns;
        self.docs_evaluated += t.fts.docs_evaluated;
        self.pivot_skips += t.fts.pivot_skips;
        self.blocks_decoded += t.fts.blocks_decoded;
        self.blocks_skipped += t.fts.blocks_skipped;
    }

    fn report(&self, dataset: &str, queries: usize, wall_ns: &mut [u64]) {
        let phases_ns = self.vector_ns
            + self.cursor_open_ns
            + self.bound_ns
            + self.decode_ns
            + self.keep_ns
            + self.doc_len_ns
            + self.scoring_ns
            + self.fuse_ns
            + self.hit_load_ns;
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        let pct = |ns: u64| {
            if phases_ns == 0 {
                0.0
            } else {
                100.0 * ns as f64 / phases_ns as f64
            }
        };

        wall_ns.sort_unstable();
        let p50 = wall_ns[wall_ns.len() / 2];
        let p99 = wall_ns[(wall_ns.len() * 99 / 100).min(wall_ns.len() - 1)];

        println!(
            "# FTOPT-5 post-sidecar hybrid recall phase profile — {dataset}, {queries} queries (ADR 0017)\n"
        );
        println!(
            "`Store::recall_profiled` wall time — p50 {:.3} ms, p99 {:.3} ms\n",
            ms(p50),
            ms(p99)
        );
        println!("| phase | total ms | share of measured phases |");
        println!("|---|---:|---:|");
        println!(
            "| vector (embed query + HNSW search) | {:.3} | {:.1}% |",
            ms(self.vector_ns),
            pct(self.vector_ns)
        );
        println!(
            "| fts: cursor open (dict lookup + small-list decode) | {:.3} | {:.1}% |",
            ms(self.cursor_open_ns),
            pct(self.cursor_open_ns)
        );
        println!(
            "| fts: WAND/block-max bound loop (skip vs. evaluate) | {:.3} | {:.1}% |",
            ms(self.bound_ns),
            pct(self.bound_ns)
        );
        println!(
            "| fts: block decode (postings materialized) | {:.3} | {:.1}% |",
            ms(self.decode_ns),
            pct(self.decode_ns)
        );
        println!(
            "| fts: keep (sidecar/record re-check) | {:.3} | {:.1}% |",
            ms(self.keep_ns),
            pct(self.keep_ns)
        );
        println!(
            "| fts: doc_len (sidecar/record lookup) | {:.3} | {:.1}% |",
            ms(self.doc_len_ns),
            pct(self.doc_len_ns)
        );
        println!(
            "| fts: scoring (BM25 + top-k insert) | {:.3} | {:.1}% |",
            ms(self.scoring_ns),
            pct(self.scoring_ns)
        );
        println!(
            "| RRF fusion (vector + text lists) | {:.3} | {:.1}% |",
            ms(self.fuse_ns),
            pct(self.fuse_ns)
        );
        println!(
            "| hit load (final record fetch for returned hits) | {:.3} | {:.1}% |",
            ms(self.hit_load_ns),
            pct(self.hit_load_ns)
        );
        println!("| **sum of phases** | {:.3} | 100.0% |", ms(phases_ns));

        println!(
            "\nBlockMax-WAND work: {} docs evaluated exactly, {} pivot candidates skipped by the block-max check, {} blocks decoded, {} blocks skipped (skip rate {:.2}%).",
            self.docs_evaluated,
            self.pivot_skips,
            self.blocks_decoded,
            self.blocks_skipped,
            100.0 * self.blocks_skipped as f64
                / (self.blocks_decoded + self.blocks_skipped).max(1) as f64
        );
    }
}
