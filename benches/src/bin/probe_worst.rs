//! Worst-query recall diagnosis @ 100k — the measurement behind story S27
//! (task FT4; follow-up of `docs/adr/0015`, which left the worst harness query
//! at recall@10 = 0.20 against the ≥ 0.70 target even at `ef_search = 256`).
//!
//! Before picking a fix (bigger `ef_search`, `ef_construction`/`M` at build
//! time, or a low-confidence retry heuristic), this binary answers the prior
//! question: **what kind of miss is the tail?** For every harness query it
//! grades the HNSW top-k against the brute-force exact top-k two ways:
//!
//! - **id overlap** — the harness's current recall@k (`benches/src/recall.rs`):
//!   of the exact top-k *ids*, how many did HNSW return?
//! - **score parity (tie-aware)** — of the returned hits, how many have an
//!   exact cosine score at least as good as the k-th exact score (minus a tiny
//!   epsilon)? This is the ann-benchmarks-style grading: on a corpus with
//!   exact duplicates (the synthetic generator produces them by design — same
//!   template + same slot fills → byte-identical text → bit-identical
//!   embedding), the exact top-k boundary can be a plateau of tied vectors,
//!   and *which* tied ids a correct index returns is arbitrary.
//!
//! If a bad query's score-parity recall is high while its id overlap is low,
//! the miss is a **tie artifact** — the index returned equally-near neighbors
//! and the metric punished the coin flip; no search/build parameter can fix
//! that. If score parity is also low, the miss is **real** (the graph never
//! reached the true neighborhood) and the ef ladder rerun below shows whether
//! a bigger beam closes it and at what latency cost.
//!
//! ```text
//! cargo run -p embedmind-bench --release --bin probe_worst -- agent-mem-100k
//! # custom retry ladder / query count / tie epsilon:
//! EF_LIST=512,1024 PROBE_QUERIES=1000 TIE_EPS=1e-4 ... --bin probe_worst
//! ```
//!
//! Decision-only tooling, like `sweep_ef`: nothing here changes engine
//! behavior. The chosen fix lands separately with these numbers in the ADR.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use embedmind_bench::dataset::{self, DATASETS, DatasetSpec, VectorSet};
use embedmind_bench::{baseline, default_data_dir, recall};
use embedmind_core::api::{Query, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::index::normalize;
use embedmind_core::storage::vfs::RealVfs;
use ulid::Ulid;

/// Same k the harness reports (`docs/BENCHMARKS.md` §3).
const K: usize = 10;

/// Same query-set size as the harness, so the default-ef row reproduces the
/// committed numbers (same seeds, same texts).
const DEFAULT_QUERIES: usize = 1000;

/// Queries below this id-overlap recall at the default `ef_search` get the
/// detailed diagnosis + ef-ladder rerun. Matches the S27 worst-query target.
const BAD_THRESHOLD: f64 = 0.70;

/// Default retry ladder for failing queries, past the current 256 ceiling.
const DEFAULT_EF_LADDER: &[u16] = &[384, 512, 1024, 2048];

/// Two vectors whose cosine scores differ by no more than this are treated as
/// tied. Exact duplicate texts embed to bit-identical vectors (delta 0.0);
/// the epsilon only absorbs dot-product summation-order noise.
const DEFAULT_TIE_EPS: f32 = 1e-4;

fn flush_stdout() {
    let _ = std::io::stdout().flush();
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("probe_worst failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let names: Vec<String> = std::env::args().skip(1).collect();
    let specs: Vec<&'static DatasetSpec> = if names.is_empty() {
        vec![DatasetSpec::by_name("agent-mem-100k").ok_or("agent-mem-100k missing from DATASETS")?]
    } else {
        let mut v = Vec::new();
        for n in &names {
            match DatasetSpec::by_name(n) {
                Some(s) => v.push(s),
                None => {
                    eprintln!("unknown dataset '{n}'. available:");
                    for d in DATASETS {
                        eprintln!("  {} ({} memories)", d.name, d.count);
                    }
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        v
    };

    let efs: Vec<u16> = match std::env::var("EF_LIST") {
        Ok(list) if !list.is_empty() => {
            let mut v = Vec::new();
            for part in list.split(',') {
                v.push(part.trim().parse::<u16>().map_err(|e| {
                    format!("bad EF_LIST entry '{part}': {e} (expected u16 values)")
                })?);
            }
            v
        }
        _ => DEFAULT_EF_LADDER.to_vec(),
    };
    let queries: usize = match std::env::var("PROBE_QUERIES") {
        Ok(n) if !n.is_empty() => n
            .parse()
            .map_err(|e| format!("bad PROBE_QUERIES '{n}': {e}"))?,
        _ => DEFAULT_QUERIES,
    };
    let tie_eps: f32 = match std::env::var("TIE_EPS") {
        Ok(s) if !s.is_empty() => s.parse().map_err(|e| format!("bad TIE_EPS '{s}': {e}"))?,
        _ => DEFAULT_TIE_EPS,
    };

    let data_dir = default_data_dir();
    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::load()?);

    println!("# worst-query recall diagnosis — k={K}, {queries} queries, tie_eps={tie_eps}");
    println!();
    for spec in &specs {
        probe_dataset(spec, &data_dir, &embedder, &efs, queries, tie_eps)?;
        println!();
    }
    Ok(ExitCode::SUCCESS)
}

/// The exact top-k of one query, with the score plateau at its boundary.
struct ExactTopK {
    ids: HashSet<Ulid>,
    /// Score of the k-th (worst) exact hit — the boundary a returned hit must
    /// tie or beat to count under score-parity grading.
    kth_score: f32,
    /// How many vectors in the whole set tie the k-th score (within eps) —
    /// the size of the plateau the exact top-k arbitrarily truncates.
    plateau: usize,
}

fn probe_dataset(
    spec: &DatasetSpec,
    data_dir: &Path,
    embedder: &Arc<dyn Embedder>,
    efs: &[u16],
    queries: usize,
    tie_eps: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("## {} ({} memories)", spec.name, spec.count);
    let set = load_set(spec, data_dir, embedder.as_ref())?;

    // Corpus-level duplicate census: how many stored texts are byte-identical
    // to an earlier one? Identical text → identical embedding → exact score
    // ties, the precondition for the tie-artifact hypothesis.
    let corpus = spec.corpus();
    let mut seen: HashSet<&str> = HashSet::with_capacity(corpus.len());
    let dup_texts = corpus
        .iter()
        .filter(|m| !seen.insert(m.content.as_str()))
        .count();
    println!(
        "corpus: {} memories, {} exact duplicate texts ({:.1}%)",
        corpus.len(),
        dup_texts,
        100.0 * dup_texts as f64 / corpus.len().max(1) as f64
    );
    drop(corpus);

    let opts = StoreOptions {
        embedder: Some(Arc::clone(embedder)),
        ..StoreOptions::default()
    };
    let store = Store::open_with(Arc::new(RealVfs), &spec.mind_path(data_dir), opts)?;

    // Vector lookup by record id, to score whatever HNSW returns.
    let by_id: HashMap<Ulid, &[f32]> = set
        .entries
        .iter()
        .map(|e| (e.id, e.vector.as_slice()))
        .collect();

    // --- exact top-k per query, once (the brute-force fixed cost) ---
    let texts = recall::query_texts(spec, queries);
    eprintln!(
        "  [{}] embedding {queries} queries + exact top-{K}...",
        spec.name
    );
    let started = Instant::now();
    let mut query_vecs: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    let mut exacts: Vec<ExactTopK> = Vec::with_capacity(texts.len());
    for (i, t) in texts.iter().enumerate() {
        let mut qv = embedder.embed(t)?;
        normalize(&mut qv);
        let hits = baseline::top_k(&set, &qv, K, |_| true);
        let kth_score = hits.last().map_or(0.0, |h| h.score);
        let plateau = set
            .entries
            .iter()
            .filter(|e| dot(&e.vector, &qv) >= kth_score - tie_eps)
            .count();
        exacts.push(ExactTopK {
            ids: hits.into_iter().map(|h| h.record_id).collect(),
            kth_score,
            plateau,
        });
        query_vecs.push(qv);
        if (i + 1) % 250 == 0 {
            eprintln!("  [{}] baseline {}/{}", spec.name, i + 1, texts.len());
        }
    }
    eprintln!(
        "  [{}] baseline ready in {:.1}s",
        spec.name,
        started.elapsed().as_secs_f64()
    );

    // --- pass 1: the default ef (what the harness grades), both gradings ---
    let mut id_recalls: Vec<f64> = Vec::with_capacity(texts.len());
    let mut score_recalls: Vec<f64> = Vec::with_capacity(texts.len());
    let mut bad: Vec<usize> = Vec::new();
    for (i, t) in texts.iter().enumerate() {
        let hits = store.recall_vector(Query::new(t.clone()).limit(K))?;
        let (idr, scr) = grade(&hits, &exacts[i], &query_vecs[i], &by_id, tie_eps);
        if idr < BAD_THRESHOLD {
            bad.push(i);
        }
        id_recalls.push(idr);
        score_recalls.push(scr);
        if (i + 1) % 250 == 0 {
            eprintln!("  [{}] default-ef {}/{}", spec.name, i + 1, texts.len());
        }
    }
    println!();
    println!("### default ef (harness conditions)");
    println!("| grading | mean | min | p10 | p50 |");
    println!("|---|---|---|---|---|");
    print_dist("id overlap (current metric)", &id_recalls);
    print_dist("score parity (tie-aware)", &score_recalls);
    flush_stdout();

    // --- pass 2: detail every bad query + rerun it up the ef ladder ---
    println!();
    println!(
        "### queries with id-overlap recall < {BAD_THRESHOLD} at default ef: {}",
        bad.len()
    );
    for &i in &bad {
        let e = &exacts[i];
        println!();
        println!("- **query {i}**: {:?}", texts[i]);
        println!(
            "  id-recall {:.2}, score-recall {:.2}, kth exact score {:.6}, plateau {} vectors tie it",
            id_recalls[i], score_recalls[i], e.kth_score, e.plateau
        );
        for &ef in efs {
            let started = Instant::now();
            let hits = store.recall_vector(Query::new(texts[i].clone()).limit(K).ef_search(ef))?;
            let ms = started.elapsed().as_secs_f64() * 1e3;
            let (idr, scr) = grade(&hits, e, &query_vecs[i], &by_id, tie_eps);
            println!("  ef={ef}: id-recall {idr:.2}, score-recall {scr:.2}, {ms:.2} ms");
        }
        flush_stdout();
    }

    store.close()?;
    Ok(())
}

/// Grades one HNSW result both ways: (id overlap, score parity), each in
/// `[0, 1]` over the same denominator the harness uses.
fn grade(
    hits: &[embedmind_core::api::Recalled],
    exact: &ExactTopK,
    query: &[f32],
    by_id: &HashMap<Ulid, &[f32]>,
    tie_eps: f32,
) -> (f64, f64) {
    let denom = exact.ids.len().max(1);
    let overlap = hits.iter().filter(|h| exact.ids.contains(&h.id)).count();
    let parity = hits
        .iter()
        .filter(|h| {
            by_id
                .get(&h.id)
                .is_some_and(|v| dot(v, query) >= exact.kth_score - tie_eps)
        })
        .count()
        .min(denom);
    (overlap as f64 / denom as f64, parity as f64 / denom as f64)
}

fn print_dist(label: &str, recalls: &[f64]) {
    let mut sorted = recalls.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = sorted.iter().sum::<f64>() / sorted.len().max(1) as f64;
    println!(
        "| {label} | {mean:.4} | {:.2} | {:.2} | {:.2} |",
        sorted.first().copied().unwrap_or(0.0),
        sorted_percentile(&sorted, 10.0),
        sorted_percentile(&sorted, 50.0),
    );
}

/// Nearest-rank percentile over an already-sorted slice (same method as the
/// harness's — the reported value always occurred).
fn sorted_percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Loads the `.vec` sidecar (requires the `.mind` to exist too), or
/// materializes fresh — same logic as `sweep_ef`.
fn load_set(
    spec: &DatasetSpec,
    data_dir: &Path,
    embedder: &dyn Embedder,
) -> Result<VectorSet, Box<dyn std::error::Error>> {
    let vec_path = spec.vec_path(data_dir);
    let mind_path = spec.mind_path(data_dir);
    if vec_path.exists() && mind_path.exists() {
        match dataset::load_vec_file(spec, &vec_path, embedder.dims(), embedder.id()) {
            Ok(set) => {
                eprintln!(
                    "  [{}] loaded {} cached vectors",
                    spec.name,
                    set.entries.len()
                );
                return Ok(set);
            }
            Err(e) => eprintln!(
                "  [{}] cached vectors unusable ({e}); regenerating",
                spec.name
            ),
        }
    }
    eprintln!(
        "  [{}] materializing (embeds every memory once)...",
        spec.name
    );
    Ok(dataset::materialize(spec, data_dir)?)
}
