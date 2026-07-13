//! BMW-3 (`docs/adr/0025`): does the official benchmark query set actually
//! exercise the BlockMax-WAND skip path, or does every matched term fall
//! under `SKIP_MIN_DOC_FREQ` (512 postings) and get decoded whole as a single
//! synthetic block?
//!
//! The p99 @100k did not move after BMW-2 (`benches/results/latest.md`), and
//! the leading hypothesis is that the synthetic corpus's wide template/paraphrase
//! vocabulary keeps most per-term document frequencies under the skip
//! threshold — so the file is fv6 (skip index present) but most queries never
//! touch a real skip index. This measures that directly via
//! `Store::search_text_bmw_counted` (`#[doc(hidden)]`,
//! `crates/embedmind-core/src/api.rs`), using the *same* query set the
//! official harness times (`benches::recall::query_texts`).
//!
//! ```text
//! cargo run -p embedmind-bench --release --bin bmw_reach -- agent-mem-100k
//! ```
//!
//! Read-only measurement: no engine behavior changes.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::process::ExitCode;
use std::sync::Arc;

use embedmind_bench::dataset::DatasetSpec;
use embedmind_bench::{default_data_dir, recall};
use embedmind_core::api::{Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::storage::vfs::RealVfs;

const K: usize = 10;
const QUERIES: usize = 1000;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bmw_reach failed: {e}");
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

    let texts = recall::query_texts(spec, QUERIES);

    let mut queries_with_skip_term = 0u64;
    let mut queries_all_small = 0u64;
    let mut queries_no_match = 0u64;
    let mut blocks_total = 0u64;
    let mut blocks_decoded = 0u64;
    let mut docs_evaluated = 0u64;
    let mut pivot_skips = 0u64;

    for t in &texts {
        let (_, counters) = store.search_text_bmw_counted(Query::new(t.clone()).limit(K))?;
        blocks_total += counters.blocks_total;
        blocks_decoded += counters.blocks_decoded;
        docs_evaluated += counters.docs_evaluated;
        pivot_skips += counters.pivot_skips;
        if counters.blocks_total == 0 {
            queries_no_match += 1;
        } else if counters.blocks_skipped() > 0 {
            queries_with_skip_term += 1;
        } else {
            queries_all_small += 1;
        }
    }
    store.close()?;

    println!(
        "# BMW-3 reach — {} ({} queries, k={K})\n",
        spec.name,
        texts.len()
    );
    println!(
        "queries with >=1 term that skipped a block (real BMW reach): {queries_with_skip_term} ({:.1}%)",
        100.0 * queries_with_skip_term as f64 / texts.len() as f64
    );
    println!(
        "queries where every matched term decoded whole (no block ever skipped): {queries_all_small} ({:.1}%)",
        100.0 * queries_all_small as f64 / texts.len() as f64
    );
    println!(
        "queries with no matched term at all: {queries_no_match} ({:.1}%)",
        100.0 * queries_no_match as f64 / texts.len() as f64
    );
    println!(
        "\nblocks_total: {blocks_total}, blocks_decoded: {blocks_decoded}, blocks_skipped: {} ({:.1}%)",
        blocks_total.saturating_sub(blocks_decoded),
        100.0 * blocks_total.saturating_sub(blocks_decoded) as f64 / blocks_total.max(1) as f64
    );
    println!("docs_evaluated: {docs_evaluated}, pivot_skips: {pivot_skips}");

    Ok(ExitCode::SUCCESS)
}
