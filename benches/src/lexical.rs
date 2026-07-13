//! Full-text "lift" measurement: lexical queries (`docs/BENCHMARKS.md`,
//! founder review 2026-07-13).
//!
//! [`crate::recall`] measures only the vector half (`Store::recall_vector`)
//! against semantic-paraphrase queries — the workload where a 384-dim
//! embedding (all-MiniLM-L6-v2) already does well, and where fusing in BM25
//! would "contaminate" the index-quality metric it is designed to isolate.
//! That leaves the *other* half of the question unanswered: how much does the
//! full-text side actually buy on queries an embedding tends to get wrong —
//! exact code identifiers, ULIDs/hashes, literal error strings, CLI flags,
//! out-of-vocabulary tokens? This module answers it.
//!
//! Ground truth is **by construction**: a fixed bank of lexical memories is
//! generated deterministically from a seed, each holding exactly one literal
//! token nothing else in the corpus repeats (`docs/BENCHMARKS.md`: every claim
//! traces to a measured number, so the target of each query must be an
//! unambiguous fact, not inferred after the run). The query is the literal
//! itself; the target is the one memory that contains it. [`measure_lift`]
//! then runs the *same* queries through both `Store::recall` (hybrid:
//! BM25+vector+RRF, the full `Store::recall`) and `Store::recall_vector`
//! (vector-only, no FTS/fusion) and reports recall@k + latency for each — the
//! delta between the two *is* the full-text benefit.

use std::time::Instant;

use embedmind_core::api::{MemoryDraft, Query, Store};

use crate::metrics::Latencies;

/// One lexical literal a query targets, plus the sentence it is embedded in
/// (the memory content actually stored). Kept separate so the query text can
/// be exactly the bare literal — the hardest case for an embedding, and the
/// case a keyword index handles trivially.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexicalCase {
    /// The exact literal a caller might search for (function/symbol name,
    /// ULID, hex hash, CLI flag, or a fragment of a literal error message).
    pub literal: String,
    /// The memory text containing `literal` — the ground-truth target.
    pub content: String,
    /// Synthetic project scope, matching [`crate::corpus`]'s convention.
    pub project: String,
}

/// Deterministic PRNG reused from [`crate::corpus::Rng`]'s algorithm
/// (splitmix64) so this module needs no new dependency and stays trivially
/// reproducible. Kept private and separate from `corpus::Rng` (which is not
/// `pub`) rather than exposing that type across modules for one shared use.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";
const ULID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Synthetic projects, matching [`crate::corpus::PROJECTS`] so scope-filtered
/// recall over lexical memories exercises the same realism.
const PROJECTS: &[&str] = &["embedmind", "cachesnap", "consultoria", "infra", "notes"];

/// Code-identifier stems, function/symbol-shaped: the class of token a
/// keyword index matches exactly and a semantic embedding tends to blur into
/// "something about caching/parsing/etc" without pinning the exact symbol.
const IDENT_STEMS: &[&str] = &[
    "lookup_via_skip",
    "fuse_lists",
    "recall_vector",
    "tie_aware_overlap",
    "search_profiled",
    "build_compacted",
    "checked_sub",
    "measure_warm_queries",
    "default_ef_search",
    "vacuum_by_copy",
];

/// CLI flags exactly as a user would type them.
const CLI_FLAGS: &[&str] = &[
    "--recency",
    "--supersedes",
    "--op-log",
    "--ef-search",
    "--compare-chroma",
];

/// Literal error-message fragments (the kind an agent remembers verbatim
/// after debugging a crash).
const ERROR_FRAGMENTS: &[&str] = &[
    "subtract with overflow in lookup_via_skip",
    "attempt to divide by zero in doc_len",
    "called `Option::unwrap()` on a `None` value",
    "stale .vec: seed mismatch — regenerate",
    "not a bench .vec file (bad magic)",
];

/// Sentence frames the literal is embedded into — mirrors
/// [`crate::corpus`]'s "code note" register (bilingual, short), so lexical
/// memories are stylistically indistinguishable from the surrounding corpus,
/// not an obviously synthetic outlier.
const IDENT_FRAMES: &[&str] = &[
    "Bug: {lit} panics on an empty batch — fixed in the next release.",
    "The {lit} function is the hot path profiling flagged.",
    "TODO: refactor {lit} to avoid the extra allocation.",
    "Nota: {lit} foi reescrita para evitar o overflow.",
];
const FLAG_FRAMES: &[&str] = &[
    "Remember: pass {lit} to enable this at the CLI.",
    "{lit} is opt-in, off by default.",
    "Lembrete: o founder usa {lit} no fluxo de release.",
];
const ERROR_FRAMES: &[&str] = &[
    "Crash log showed: \"{lit}\".",
    "The panic message was exactly \"{lit}\".",
    "Reproduzido o erro: \"{lit}\".",
];
const HASH_FRAMES: &[&str] = &[
    "Commit {lit} introduced the fix.",
    "Artifact checksum: {lit}.",
];
const ULID_FRAMES: &[&str] = &[
    "Memory {lit} is the one superseded by the fix.",
    "See record {lit} for the original decision.",
];

fn hex_token(rng: &mut Rng, len: usize) -> String {
    (0..len).map(|_| rng.pick(HEX) as char).collect()
}

fn ulid_token(rng: &mut Rng) -> String {
    (0..26).map(|_| rng.pick(ULID_ALPHABET) as char).collect()
}

fn frame(rng: &mut Rng, frames: &[&str], literal: &str) -> String {
    rng.pick(frames).replace("{lit}", literal)
}

/// Generates `count` lexical ground-truth cases deterministically from `seed`.
/// Cycles through five literal kinds (code identifier, CLI flag, error
/// fragment, hex hash, ULID) so the suite covers the whole class the task
/// describes, not just one shape. Same `(seed, count)` always yields the same
/// output — the reproducibility guarantee the rest of the harness relies on.
pub fn generate_cases(seed: u64, count: usize) -> Vec<LexicalCase> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let project = rng.pick(PROJECTS).to_owned();
        let (literal, content) = match i % 5 {
            0 => {
                // A code identifier, optionally suffixed with a disambiguating
                // number so `count` can exceed the stem bank without literal
                // collisions (each literal must be unique in the corpus —
                // it's the query's ground truth).
                let stem = rng.pick(IDENT_STEMS);
                let literal = format!("{stem}_{i}");
                let content = frame(&mut rng, IDENT_FRAMES, &literal);
                (literal, content)
            }
            1 => {
                let flag = rng.pick(CLI_FLAGS);
                let literal = format!("{flag}-v{i}");
                let content = frame(&mut rng, FLAG_FRAMES, &literal);
                (literal, content)
            }
            2 => {
                let fragment = rng.pick(ERROR_FRAGMENTS);
                let literal = format!("{fragment} (case #{i})");
                let content = frame(&mut rng, ERROR_FRAMES, &literal);
                (literal, content)
            }
            3 => {
                let literal = hex_token(&mut rng, 12);
                let content = frame(&mut rng, HASH_FRAMES, &literal);
                (literal, content)
            }
            _ => {
                let literal = ulid_token(&mut rng);
                let content = frame(&mut rng, ULID_FRAMES, &literal);
                (literal, content)
            }
        };
        out.push(LexicalCase {
            literal,
            content,
            project,
        });
    }
    out
}

/// Recall@k + latency of one recall strategy over the lexical query set: how
/// often the ground-truth memory for each literal lands in the top-`k`, and
/// the per-query latency distribution.
#[derive(Debug, Clone, Copy)]
pub struct LexicalReport {
    pub k: usize,
    pub queries: usize,
    /// Fraction of queries whose ground-truth memory appears in the top-k.
    pub recall_at_k: f64,
    pub latency: LatencySummary,
}

/// Minimal latency summary (p50/p99), independent of [`crate::metrics::Latencies`]
/// storage so [`LexicalReport`] stays `Copy`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencySummary {
    pub p50_ms: f64,
    pub p99_ms: f64,
}

impl LatencySummary {
    fn from_latencies(lat: &Latencies) -> Self {
        LatencySummary {
            p50_ms: lat.p50_ms().unwrap_or(0.0),
            p99_ms: lat.p99_ms().unwrap_or(0.0),
        }
    }
}

/// Both recall strategies measured over the same lexical queries — the
/// hybrid-vs-vector-only comparison the task exists to produce. The delta
/// between `hybrid.recall_at_k` and `vector_only.recall_at_k` is the
/// full-text benefit on this workload.
#[derive(Debug, Clone, Copy)]
pub struct LexicalLift {
    pub hybrid: LexicalReport,
    pub vector_only: LexicalReport,
}

/// Ingests `cases` into `store` (one `remember` per case, same write path the
/// corpus uses) and returns their assigned record ids in order — the
/// ground-truth id set [`measure_lift`] checks each query's top-k against.
pub fn ingest_cases(
    store: &mut Store,
    cases: &[LexicalCase],
) -> embedmind_core::Result<Vec<ulid::Ulid>> {
    let mut ids = Vec::with_capacity(cases.len());
    for case in cases {
        let stored = store.remember(
            MemoryDraft::new(case.content.clone())
                .project(case.project.clone())
                .agent("bench-lexical"),
        )?;
        ids.push(stored.id);
    }
    Ok(ids)
}

/// Measures recall@k and latency of both `Store::recall` (hybrid) and
/// `Store::recall_vector` (vector-only) over `cases`/`ids` (as returned by
/// [`ingest_cases`], same order) — the same queries fed to both, per the
/// task's core rule. `k` bounds the top-k each strategy is graded against.
pub fn measure_lift(
    store: &Store,
    cases: &[LexicalCase],
    ids: &[ulid::Ulid],
    k: usize,
) -> embedmind_core::Result<LexicalLift> {
    assert_eq!(cases.len(), ids.len(), "cases and ids must be parallel");

    let mut hybrid_hits = 0usize;
    let mut vector_hits = 0usize;
    let mut hybrid_lat = Latencies::with_capacity(cases.len());
    let mut vector_lat = Latencies::with_capacity(cases.len());

    for (case, &target_id) in cases.iter().zip(ids) {
        let started = Instant::now();
        let hybrid = store.recall(Query::new(case.literal.clone()).limit(k))?;
        hybrid_lat.push(started.elapsed());
        if hybrid.iter().any(|r| r.id == target_id) {
            hybrid_hits += 1;
        }

        let started = Instant::now();
        let vector = store.recall_vector(Query::new(case.literal.clone()).limit(k))?;
        vector_lat.push(started.elapsed());
        if vector.iter().any(|r| r.id == target_id) {
            vector_hits += 1;
        }
    }

    let n = cases.len().max(1);
    Ok(LexicalLift {
        hybrid: LexicalReport {
            k,
            queries: cases.len(),
            recall_at_k: hybrid_hits as f64 / n as f64,
            latency: LatencySummary::from_latencies(&hybrid_lat),
        },
        vector_only: LexicalReport {
            k,
            queries: cases.len(),
            recall_at_k: vector_hits as f64 / n as f64,
            latency: LatencySummary::from_latencies(&vector_lat),
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use embedmind_core::api::StoreOptions;
    use embedmind_core::embed::Embedder;
    use embedmind_core::storage::sim::SimVfs;
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn generation_is_deterministic_for_a_fixed_seed() {
        let a = generate_cases(0xC0FF_EE01, 50);
        let b = generate_cases(0xC0FF_EE01, 50);
        assert_eq!(a, b, "same seed must reproduce the same cases");
    }

    #[test]
    fn different_seeds_diverge() {
        let a = generate_cases(1, 30);
        let b = generate_cases(2, 30);
        assert_ne!(a, b);
    }

    #[test]
    fn every_case_is_anchored_in_its_own_content() {
        // Ground truth by construction: the literal must actually occur in
        // the content it's paired with, or there is no ground truth.
        for case in generate_cases(0xABCD_1234, 100) {
            assert!(
                case.content.contains(&case.literal),
                "literal {:?} missing from content {:?}",
                case.literal,
                case.content
            );
        }
    }

    #[test]
    fn literals_are_unique_across_the_generated_set() {
        // Ground truth requires each literal to identify exactly one memory;
        // a repeated literal would make more than one record a "correct" hit
        // and silently inflate recall.
        let cases = generate_cases(0x1122_3344, 200);
        let mut seen = std::collections::HashSet::new();
        for case in &cases {
            assert!(
                seen.insert(case.literal.clone()),
                "duplicate literal: {}",
                case.literal
            );
        }
    }

    #[test]
    fn covers_all_five_literal_kinds() {
        let cases = generate_cases(0x9999, 25);
        assert!(
            cases
                .iter()
                .any(|c| IDENT_STEMS.iter().any(|s| c.literal.starts_with(s)))
        );
        assert!(cases.iter().any(|c| c.literal.starts_with("--")));
        assert!(
            cases
                .iter()
                .any(|c| ERROR_FRAGMENTS.iter().any(|f| c.literal.starts_with(f)))
        );
        assert!(
            cases.iter().any(
                |c| c.literal.len() == 12 && c.literal.chars().all(|ch| ch.is_ascii_hexdigit())
            )
        );
        assert!(cases.iter().any(|c| {
            c.literal.len() == 26
                && c.literal
                    .chars()
                    .all(|ch| ULID_ALPHABET.contains(&(ch as u8)))
        }));
    }

    struct StubEmbedder;
    impl Embedder for StubEmbedder {
        fn embed(&self, text: &str) -> embedmind_core::Result<Vec<f32>> {
            // A crude bag-of-hash embedding: enough structure that unrelated
            // lexical literals don't all collide to the same vector, but with
            // no notion of exact tokens — the point is that the *hybrid* path
            // (which also gets BM25) must out-recall this vector alone.
            let mut v = vec![0.0f32; 16];
            for (i, b) in text.bytes().enumerate() {
                v[i % 16] += b as f32;
            }
            embedmind_core::index::normalize(&mut v);
            Ok(v)
        }
        fn id(&self) -> embedmind_core::embed::ModelId {
            "stub-lexical-test"
        }
        fn dims(&self) -> u16 {
            16
        }
    }

    #[test]
    fn measure_lift_runs_both_strategies_over_the_same_queries() {
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder);
        let opts = StoreOptions {
            embedder: Some(Arc::clone(&embedder)),
            ..StoreOptions::default()
        };
        let mut store =
            Store::create_with(Arc::new(SimVfs::new()), Path::new("lex-test.mind"), opts)
                .expect("create store");

        let cases = generate_cases(0x5EED, 8);
        let ids = ingest_cases(&mut store, &cases).unwrap();
        assert_eq!(ids.len(), cases.len());

        let lift = measure_lift(&store, &cases, &ids, 10).unwrap();
        assert_eq!(lift.hybrid.queries, cases.len());
        assert_eq!(lift.vector_only.queries, cases.len());
        // BM25 gives an exact-literal query a perfect anchor: the hybrid path
        // must find every ground-truth memory verbatim-contained in its own
        // query.
        assert_eq!(
            lift.hybrid.recall_at_k, 1.0,
            "hybrid recall must be perfect for literals present verbatim in the corpus"
        );
    }

    #[test]
    fn cases_and_ids_length_mismatch_panics() {
        let cases = generate_cases(1, 3);
        let ids = vec![ulid::Ulid::new()]; // wrong length on purpose
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder);
        let opts = StoreOptions {
            embedder: Some(embedder),
            ..StoreOptions::default()
        };
        let store = Store::create_with(Arc::new(SimVfs::new()), Path::new("lex-test2.mind"), opts)
            .expect("create");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            measure_lift(&store, &cases, &ids, 10)
        }));
        assert!(result.is_err());
    }
}
