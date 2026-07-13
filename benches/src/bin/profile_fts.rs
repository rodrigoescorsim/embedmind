//! FT1 (`docs/adr/0017`): phase-by-phase profiling of the full-text half of
//! hybrid `recall` on a real, already-materialized dataset — the evidence
//! ADR 0017 §1 requires before any optimization task (FT2+) starts.
//!
//! Native flamegraph tooling (`perf`/`samply`) is unavailable on the box this
//! ran on, so this uses the accepted fallback: manual `Instant`
//! instrumentation around each phase, via `Store::search_text_profiled`
//! (`#[doc(hidden)]`, `crates/embedmind-core/src/api.rs`), which mirrors
//! `Store::search_text` exactly but returns
//! [`embedmind_core::index::fts::SearchPhaseTimings`] alongside the hits.
//!
//! ```text
//! cargo run -p embedmind-bench --release --bin profile_fts -- agent-mem-100k
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
use embedmind_core::api::{Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::index::fts::SearchPhaseTimings;
use embedmind_core::storage::vfs::RealVfs;

/// Same `k` the harness reports (`docs/BENCHMARKS.md` §3).
const K: usize = 10;

/// Warm-up queries before timing starts — the harness methodology (§3) grades
/// warm-cache latency; a cold first query would blend page-cache-miss cost
/// into every phase instead of isolating it.
const WARMUP_QUERIES: usize = 50;

/// Timed queries. Large enough to compute a stable p50/p99 per phase; the
/// harness's own warm-latency measurement uses 1000, matched here so the
/// aggregate `postings_lookup + keep + doc_len + scoring` total is directly
/// comparable to `query_engine_p50/p99_ms` from a `run_all.sh --full` run.
const TIMED_QUERIES: usize = 1000;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("profile_fts failed: {e}");
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
        let _ = store.search_text_profiled(Query::new(t.clone()).limit(K))?;
    }

    eprintln!("[{}] timing {TIMED_QUERIES} queries...", spec.name);
    let mut totals = PhaseTotals::default();
    let mut wall_ns: Vec<u64> = Vec::with_capacity(TIMED_QUERIES);
    for (i, t) in timed.iter().enumerate() {
        let started = Instant::now();
        let (_, timings) = store.search_text_profiled(Query::new(t.clone()).limit(K))?;
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

/// Sums of every phase across all timed queries — the report is the
/// aggregate share, not a per-query breakdown (ADR 0017 asks for "the
/// fraction of time" spent per phase, not a distribution per query).
#[derive(Default)]
struct PhaseTotals {
    postings_lookup_ns: u64,
    keep_ns: u64,
    doc_len_ns: u64,
    scoring_ns: u64,
    terms_matched: u64,
    postings_visited: u64,
}

impl PhaseTotals {
    fn add(&mut self, t: &SearchPhaseTimings) {
        self.postings_lookup_ns += t.postings_lookup_ns;
        self.keep_ns += t.keep_ns;
        self.doc_len_ns += t.doc_len_ns;
        self.scoring_ns += t.scoring_ns;
        self.terms_matched += u64::from(t.terms_matched);
        self.postings_visited += t.postings_visited;
    }

    fn report(&self, dataset: &str, queries: usize, wall_ns: &mut [u64]) {
        let phases_ns = self.postings_lookup_ns + self.keep_ns + self.doc_len_ns + self.scoring_ns;
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

        println!("# FT1 full-text phase profile — {dataset}, {queries} queries (ADR 0017)\n");
        println!(
            "`Store::search_text_profiled` wall time — p50 {:.3} ms, p99 {:.3} ms\n",
            ms(p50),
            ms(p99)
        );
        println!("| phase | total ms | share of measured phases |");
        println!("|---|---:|---:|");
        println!(
            "| postings lookup (page I/O + decode) | {:.3} | {:.1}% |",
            ms(self.postings_lookup_ns),
            pct(self.postings_lookup_ns)
        );
        println!(
            "| keep (tombstone/scope/filter re-check) | {:.3} | {:.1}% |",
            ms(self.keep_ns),
            pct(self.keep_ns)
        );
        println!(
            "| doc_len (record reload + re-tokenize) | {:.3} | {:.1}% |",
            ms(self.doc_len_ns),
            pct(self.doc_len_ns)
        );
        println!(
            "| scoring (HashMap accumulate + sort) | {:.3} | {:.1}% |",
            ms(self.scoring_ns),
            pct(self.scoring_ns)
        );
        println!("| **sum of phases** | {:.3} | 100.0% |", ms(phases_ns));
        println!(
            "\nterms matched (total across queries): {} · postings visited (total): {}",
            self.terms_matched, self.postings_visited
        );
        println!(
            "avg postings visited per query: {:.1}",
            self.postings_visited as f64 / queries.max(1) as f64
        );
    }
}
