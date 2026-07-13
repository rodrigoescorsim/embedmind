//! FT2 (`docs/adr/0018`): before/after measurement of the early-terminating
//! BM25 scan on a real, already-materialized dataset, plus the large-corpus
//! half of the FT2 equivalence check (the small-corpus half lives in
//! `crates/embedmind-core/src/index/fts.rs` unit tests).
//!
//! Times production `Store::search_text` (the FT2 bounded scan) over the same
//! query set `profile_fts` used for the FT1 baseline, so the two reports are
//! directly comparable. Then, on a subset of queries (`EQ_QUERIES`, default
//! 25 — the exhaustive scan costs ~1 s/query @ 100k), asserts that
//! `search_text` returns exactly what the exhaustive scan
//! (`Store::search_text_profiled`, the FT2 oracle) returns: same ids, same
//! scores, same order.
//!
//! ```text
//! cargo run -p embedmind-bench --release --bin bench_fts -- agent-mem-100k
//! ```
//!
//! Read-only measurement: nothing here mutates the store.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use embedmind_bench::dataset::DatasetSpec;
use embedmind_bench::{default_data_dir, recall};
use embedmind_core::api::{Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::storage::vfs::RealVfs;

/// Same `k` the harness reports (`docs/BENCHMARKS.md` §3).
const K: usize = 10;

/// Warm-up queries before timing starts, matching `profile_fts` (FT1).
const WARMUP_QUERIES: usize = 50;

/// Timed queries, matching `profile_fts` (FT1) and the harness (§3).
const TIMED_QUERIES: usize = 1000;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bench_fts failed: {e}");
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
    let eq_queries: usize = std::env::var("EQ_QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);

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
        let _ = store.search_text(Query::new(t.clone()).limit(K))?;
    }

    eprintln!(
        "[{}] timing {TIMED_QUERIES} queries (bounded scan)...",
        spec.name
    );
    let mut wall_ns: Vec<u64> = Vec::with_capacity(TIMED_QUERIES);
    for (i, t) in timed.iter().enumerate() {
        let started = Instant::now();
        let _ = store.search_text(Query::new(t.clone()).limit(K))?;
        wall_ns.push(started.elapsed().as_nanos() as u64);
        if (i + 1) % 250 == 0 {
            eprintln!("  {}/{TIMED_QUERIES}", i + 1);
        }
    }

    eprintln!(
        "[{}] equivalence check vs. exhaustive scan ({eq_queries} queries)...",
        spec.name
    );
    let mut mismatches = 0usize;
    for t in timed.iter().take(eq_queries) {
        let bounded = store.search_text(Query::new(t.clone()).limit(K))?;
        let (full, _) = store.search_text_profiled(Query::new(t.clone()).limit(K))?;
        let same = bounded.len() == full.len()
            && bounded
                .iter()
                .zip(&full)
                .all(|(a, b)| a.memory.id == b.memory.id && a.score.to_bits() == b.score.to_bits());
        if !same {
            mismatches += 1;
            eprintln!("  MISMATCH on query: {t}");
        }
    }
    store.close()?;

    wall_ns.sort_unstable();
    let ms = |ns: u64| ns as f64 / 1_000_000.0;
    let p50 = wall_ns[wall_ns.len() / 2];
    let p99 = wall_ns[(wall_ns.len() * 99 / 100).min(wall_ns.len() - 1)];

    println!(
        "# FT2 full-text after-measurement — {}, {TIMED_QUERIES} queries (ADR 0018)\n",
        spec.name
    );
    println!(
        "`Store::search_text` (bounded scan) wall time — p50 {:.3} ms, p99 {:.3} ms",
        ms(p50),
        ms(p99)
    );
    println!(
        "\nequivalence vs. exhaustive scan: {} — {}/{} queries identical (ids, bit-exact scores, order)",
        if mismatches == 0 { "OK" } else { "FAILED" },
        eq_queries - mismatches,
        eq_queries
    );
    Ok(if mismatches == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
