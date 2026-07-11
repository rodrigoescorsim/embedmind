//! Near-duplicate threshold calibration (story S21 / task FR3; `docs/adr/0016`).
//!
//! `remember` reports existing memories whose cosine similarity to the new
//! content clears a threshold. That threshold must come from measurement, not
//! a guess: this binary embeds, with the shipped ONNX model,
//!
//! - **duplicate pairs** — corpus-distribution facts restated (same slot
//!   fills, different template = paraphrase; same text with framing noise =
//!   noisy copy; see `corpus::duplicate_pairs`), and
//! - **unrelated pairs** — random distinct memories from the standard corpus
//!   distribution,
//!
//! then prints both cosine-score distributions and a threshold sweep (what
//! fraction of duplicates each candidate catches vs. what fraction of
//! unrelated pairs it would falsely flag). The chosen value lands in
//! `embedmind-core` as `NEAR_DUP_THRESHOLD` with the numbers recorded in the
//! ADR.
//!
//! ```text
//! cargo run -p embedmind-bench --release --bin calibrate_near_dup
//! # custom sizes:
//! DUP_PAIRS=600 UNRELATED_PAIRS=3000 cargo run -p embedmind-bench --release --bin calibrate_near_dup
//! ```
//!
//! Decision-only tooling: nothing here changes engine behavior.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::process::ExitCode;

use embedmind_bench::corpus::{self, DuplicateKind};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::index::normalize;

/// Seeds are arbitrary but fixed: the calibration is reproducible run to run.
/// Disjoint from the dataset seeds so calibration text never mirrors the
/// committed benchmark corpora verbatim.
const DUP_SEED: u64 = 0x5EED_D0B1_2026_0710;
const UNRELATED_SEED: u64 = 0x5EED_0DD1_2026_0710;

const DEFAULT_DUP_PAIRS: usize = 400;
const DEFAULT_UNRELATED_PAIRS: usize = 2000;

/// Candidate thresholds swept for the decision table.
const CANDIDATES: &[f32] = &[
    0.70, 0.725, 0.75, 0.775, 0.80, 0.825, 0.85, 0.875, 0.90, 0.925, 0.95,
];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("calibrate_near_dup failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn env_size(var: &str, default: usize) -> Result<usize, String> {
    match std::env::var(var) {
        Ok(n) if !n.is_empty() => n.parse().map_err(|e| format!("bad {var} '{n}': {e}")),
        _ => Ok(default),
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let dup_pairs = env_size("DUP_PAIRS", DEFAULT_DUP_PAIRS)?;
    let unrelated_pairs = env_size("UNRELATED_PAIRS", DEFAULT_UNRELATED_PAIRS)?;

    let embedder = OnnxEmbedder::load()?;
    eprintln!("model: {} ({} dims)", embedder.id(), embedder.dims());

    // --- duplicate pairs: cosine(original, restatement), split by kind -----
    let pairs = corpus::duplicate_pairs(DUP_SEED, dup_pairs);
    let mut paraphrase: Vec<f32> = Vec::new();
    let mut noisy: Vec<f32> = Vec::new();
    for (i, pair) in pairs.iter().enumerate() {
        let score = cosine(&embedder, &pair.original, &pair.duplicate)?;
        match pair.kind {
            DuplicateKind::Paraphrase => paraphrase.push(score),
            DuplicateKind::NoisyCopy => noisy.push(score),
        }
        if (i + 1) % 100 == 0 {
            eprintln!("duplicates {}/{}", i + 1, pairs.len());
        }
    }

    // --- unrelated pairs: cosine of random distinct corpus memories --------
    // 2 fresh memories per pair (drawn in sequence from one corpus stream),
    // so no vector is reused and pair independence is exact.
    let corpus_texts = corpus::generate(UNRELATED_SEED, unrelated_pairs * 2);
    let mut unrelated: Vec<f32> = Vec::with_capacity(unrelated_pairs);
    for (i, chunk) in corpus_texts.chunks_exact(2).enumerate() {
        unrelated.push(cosine(&embedder, &chunk[0].content, &chunk[1].content)?);
        if (i + 1) % 250 == 0 {
            eprintln!("unrelated {}/{unrelated_pairs}", i + 1);
        }
    }

    let mut all_dups: Vec<f32> = paraphrase.iter().chain(noisy.iter()).copied().collect();
    sort(&mut all_dups);
    sort(&mut paraphrase);
    sort(&mut noisy);
    sort(&mut unrelated);

    println!("# near-duplicate threshold calibration (S21, ADR 0016)");
    println!();
    println!(
        "model: {} — {} duplicate pairs ({} paraphrase / {} noisy-copy), {} unrelated pairs",
        embedder.id(),
        all_dups.len(),
        paraphrase.len(),
        noisy.len(),
        unrelated.len()
    );
    println!();
    println!("| distribution | min | p1 | p5 | p10 | p25 | p50 | p75 | p90 | p95 | p99 | max |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    print_distribution("duplicates (all)", &all_dups);
    print_distribution("duplicates: paraphrase", &paraphrase);
    print_distribution("duplicates: noisy copy", &noisy);
    print_distribution("unrelated", &unrelated);
    println!();
    println!(
        "| threshold | duplicates caught | paraphrase caught | noisy caught | unrelated flagged |"
    );
    println!("|---|---|---|---|---|");
    for &t in CANDIDATES {
        println!(
            "| {t:.3} | {:.1}% | {:.1}% | {:.1}% | {:.2}% |",
            caught(&all_dups, t) * 100.0,
            caught(&paraphrase, t) * 100.0,
            caught(&noisy, t) * 100.0,
            caught(&unrelated, t) * 100.0,
        );
    }
    Ok(())
}

fn cosine(embedder: &dyn Embedder, a: &str, b: &str) -> embedmind_core::Result<f32> {
    let mut va = embedder.embed(a)?;
    let mut vb = embedder.embed(b)?;
    normalize(&mut va);
    normalize(&mut vb);
    Ok(va.iter().zip(&vb).map(|(x, y)| x * y).sum())
}

fn sort(v: &mut [f32]) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
}

/// Fraction of `sorted` at or above `threshold`.
fn caught(sorted: &[f32], threshold: f32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let below = sorted.partition_point(|&s| s < threshold);
    (sorted.len() - below) as f64 / sorted.len() as f64
}

fn print_distribution(name: &str, sorted: &[f32]) {
    let p = |q: f64| -> f32 {
        if sorted.is_empty() {
            return 0.0;
        }
        let rank = ((q / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
        sorted[rank.min(sorted.len()) - 1]
    };
    println!(
        "| {name} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} |",
        sorted.first().copied().unwrap_or(0.0),
        p(1.0),
        p(5.0),
        p(10.0),
        p(25.0),
        p(50.0),
        p(75.0),
        p(90.0),
        p(95.0),
        p(99.0),
        sorted.last().copied().unwrap_or(0.0),
    );
}
