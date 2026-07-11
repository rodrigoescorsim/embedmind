//! Deterministic synthetic agent-memory text generator (`docs/BENCHMARKS.md`
//! §2). Given a fixed seed, produces the exact same corpus every run on every
//! platform — the reproducibility guarantee the benchmark methodology rests
//! on. No network, no external corpus download: the templates live here and
//! the only randomness is a seeded splitmix64.
//!
//! The distribution mimics what an agent actually accumulates: **decisions**,
//! **facts**, **preferences** and **code notes**, mixed pt-BR + en (agents in
//! this project speak both), 1–3 short sentences each — not Wikipedia
//! passages. The embeddings are produced downstream by the shipped ONNX model
//! (`embedmind-core`), identical for every benchmarked system, so this module
//! never touches vectors: text in, text out.

/// Deterministic PRNG (splitmix64). Kept local to the harness so dataset
/// generation depends on nothing but the seed — never on the wall clock, a
/// crate default, or `embedmind-core`'s own internal RNG (which is seeded from
/// insertion ordinals, a different concern).
pub struct Rng(u64);

impl Rng {
    /// Seeds the generator. The same seed yields the same corpus forever.
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `[0, n)`. `n == 0` yields 0 (callers never pass 0).
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    /// Picks one element by value. Every bank holds `Copy` elements (`&str`
    /// or `&[&str]`), so returning by value sidesteps the double-reference
    /// (`&&str`) that `&items[i]` would produce and keeps call sites clean.
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// One generated memory: content plus the project it is scoped to. Provenance
/// (agent/session) is stamped by the store on `remember`; the benchmark only
/// needs the searchable text and a project for scope-filtering realism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenMemory {
    /// The memory text (1–3 sentences, pt-BR + en mixed).
    pub content: String,
    /// Project scope — a handful of synthetic projects, so `Scope::Project`
    /// filtering is exercised the way a real multi-project store would be.
    pub project: String,
}

/// The synthetic projects a generated memory can belong to. A small fixed set
/// keeps per-project cardinality high enough for scope-filtered recall to
/// return meaningful result sets.
const PROJECTS: &[&str] = &["embedmind", "cachesnap", "consultoria", "infra", "notes"];

// The template banks. Each category has an en and a pt-BR flavor so the corpus
// is genuinely bilingual (docs/BENCHMARKS.md §2), and slots (`{a}`/`{b}`) are
// filled from the noun/verb banks to widen lexical variety without exploding
// the template count.

const DECISIONS: &[&str] = &[
    "Decided to use {a} instead of {b} for the {c} layer.",
    "We are going with {a} over {b} because it is simpler to {v}.",
    "Decidido: adotar {a} no lugar de {b} para {c}.",
    "Optamos por {a} em vez de {b} — mais fácil de {v}.",
    "Chose {a} for {c}; {b} was rejected after benchmarking.",
];

const FACTS: &[&str] = &[
    "The {c} runs on {a} and depends on {b}.",
    "{a} is deployed in the {c} environment.",
    "O {c} usa {a} e depende de {b}.",
    "{a} está em produção no ambiente de {c}.",
    "The {a} service talks to {b} over the internal network.",
];

const PREFERENCES: &[&str] = &[
    "The founder prefers {a} over {b}.",
    "Always {v} the {c} before shipping.",
    "Prefiro {a} a {b} sempre que possível.",
    "Regra do projeto: {v} o {c} antes de qualquer release.",
    "Never use {b}; standardize on {a}.",
];

const CODE_NOTES: &[&str] = &[
    "The {a} module wraps {b}; see {c} for the call site.",
    "Bug: {a} panics when {b} is empty — handle it in {c}.",
    "O módulo {a} encapsula {b}; ver {c}.",
    "TODO: refatorar {a} para não depender de {b}.",
    "Note: {v} the {c} cache when {a} changes.",
];

const NOUNS_A: &[&str] = &[
    "Rust",
    "HNSW",
    "WAL",
    "ONNX",
    "sqlite-vec",
    "tokio",
    "the pager",
    "the embedder",
    "o índice",
    "o cache de páginas",
    "MiniLM",
    "a B-tree",
];

const NOUNS_B: &[&str] = &[
    "an external database",
    "a cloud service",
    "unwrap()",
    "a background thread",
    "uma API de rede",
    "uma fila",
    "the old format",
    "reflection",
    "panics",
    "o formato antigo",
];

const CONTEXTS: &[&str] = &[
    "storage",
    "recall",
    "indexação",
    "produção",
    "the MCP server",
    "o CLI",
    "crash recovery",
    "benchmark",
    "staging",
    "a camada de rede",
];

const VERBS: &[&str] = &[
    "maintain",
    "test",
    "vacuum",
    "flush",
    "manter",
    "testar",
    "auditar",
    "reproduce",
];

fn fill(template: &str, rng: &mut Rng) -> String {
    let slots = Slots::draw(rng);
    fill_with(template, &slots)
}

/// One draw of the four slot banks — the *facts* of a memory, independent of
/// the template that words them. Two templates filled with the same [`Slots`]
/// state the same thing in different words: a synthetic near-duplicate.
struct Slots {
    a: &'static str,
    b: &'static str,
    c: &'static str,
    v: &'static str,
}

impl Slots {
    fn draw(rng: &mut Rng) -> Slots {
        Slots {
            a: rng.pick(NOUNS_A),
            b: rng.pick(NOUNS_B),
            c: rng.pick(CONTEXTS),
            v: rng.pick(VERBS),
        }
    }
}

fn fill_with(template: &str, slots: &Slots) -> String {
    template
        .replace("{a}", slots.a)
        .replace("{b}", slots.b)
        .replace("{c}", slots.c)
        .replace("{v}", slots.v)
}

/// Generates `count` synthetic memories from `seed`, deterministically. The
/// same `(seed, count)` pair always produces byte-identical content in the
/// same order — that is what lets `agent-mem-10k`/`-100k` be "committed" as a
/// tiny spec (seed + count) rather than a giant text blob.
pub fn generate(seed: u64, count: usize) -> Vec<GenMemory> {
    let mut rng = Rng::new(seed);
    let banks: &[&[&str]] = &[DECISIONS, FACTS, PREFERENCES, CODE_NOTES];
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let bank: &[&str] = rng.pick(banks);
        // 1–3 sentences, each from the same category, joined into one memory.
        let sentences = 1 + rng.below(3);
        let mut content = String::new();
        for s in 0..sentences {
            if s > 0 {
                content.push(' ');
            }
            let template: &str = rng.pick(bank);
            content.push_str(&fill(template, &mut rng));
        }
        let project = rng.pick(PROJECTS).to_owned();
        out.push(GenMemory { content, project });
    }
    out
}

/// How the second half of a [`duplicate_pairs`] pair restates the first —
/// the two shapes a real agent's near-duplicate re-`remember` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateKind {
    /// Same facts (identical slot fills, same category), worded by a
    /// *different* template — a genuine paraphrase, possibly crossing the
    /// pt-BR/en language boundary.
    Paraphrase,
    /// The same text with small edits around it (a prefix like "Update:",
    /// a trailing note) — the "agent pastes the fact again with framing"
    /// case.
    NoisyCopy,
}

/// One synthetic near-duplicate pair for threshold calibration (story S21):
/// `original` is a corpus-distribution memory, `duplicate` restates it per
/// `kind`. Deterministic in `seed`, like [`generate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicatePair {
    pub original: String,
    pub duplicate: String,
    pub kind: DuplicateKind,
}

/// Prefixes/suffixes for [`DuplicateKind::NoisyCopy`] — small framing an
/// agent adds when re-stating a fact it already stored.
const NOISE_PREFIXES: &[&str] = &["Update: ", "Nota: ", "Reminder: ", "Confirmado: "];
const NOISE_SUFFIXES: &[&str] = &[
    " (still true.)",
    " Confirmed again today.",
    " — sem mudanças.",
];

/// Generates `count` near-duplicate pairs from `seed`, deterministically,
/// alternating [`DuplicateKind::Paraphrase`] and [`DuplicateKind::NoisyCopy`].
/// The calibration binary (`calibrate_near_dup`) embeds both sides with the
/// shipped model and measures the cosine-score distribution duplicates
/// occupy vs. unrelated corpus pairs — the measurement behind the S21
/// near-duplicate threshold (ADR 0016).
pub fn duplicate_pairs(seed: u64, count: usize) -> Vec<DuplicatePair> {
    let mut rng = Rng::new(seed);
    let banks: &[&[&str]] = &[DECISIONS, FACTS, PREFERENCES, CODE_NOTES];
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bank: &[&str] = rng.pick(banks);
        let slots = Slots::draw(&mut rng);
        let template_a = rng.pick(bank);
        let original = fill_with(template_a, &slots);
        if i % 2 == 0 {
            // Different template, same slots: the same fact reworded. Draw
            // until the template differs (every bank has >= 2 templates).
            let mut template_b = rng.pick(bank);
            while template_b == template_a {
                template_b = rng.pick(bank);
            }
            out.push(DuplicatePair {
                original,
                duplicate: fill_with(template_b, &slots),
                kind: DuplicateKind::Paraphrase,
            });
        } else {
            let duplicate = format!(
                "{}{}{}",
                rng.pick(NOISE_PREFIXES),
                original,
                rng.pick(NOISE_SUFFIXES)
            );
            out.push(DuplicatePair {
                original,
                duplicate,
                kind: DuplicateKind::NoisyCopy,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn generation_is_deterministic_for_a_fixed_seed() {
        let a = generate(0xBE7C_2026, 500);
        let b = generate(0xBE7C_2026, 500);
        assert_eq!(a, b, "same seed must reproduce the same corpus");
    }

    #[test]
    fn different_seeds_diverge() {
        let a = generate(1, 200);
        let b = generate(2, 200);
        assert_ne!(a, b, "different seeds should not collide");
    }

    #[test]
    fn a_prefix_of_a_larger_run_matches_the_smaller_run() {
        // splitmix64 is a pure function of a monotonic counter, so the first
        // N of a length-M>N run are identical to a length-N run — the
        // property that makes `-10k` a genuine prefix of `-100k`.
        let small = generate(42, 100);
        let large = generate(42, 1000);
        assert_eq!(
            &large[..100],
            &small[..],
            "the 10k set must prefix the 100k set"
        );
    }

    #[test]
    fn content_is_nonempty_and_bounded() {
        for m in generate(7, 300) {
            assert!(!m.content.is_empty());
            // 3 sentences of one template + fills stays well under any window.
            assert!(m.content.len() < 800, "unexpectedly long: {}", m.content);
            assert!(PROJECTS.contains(&m.project.as_str()));
        }
    }

    #[test]
    fn corpus_is_bilingual() {
        // A large-enough sample must contain both an unmistakably pt-BR token
        // and an en one — the "pt-BR + en mixed" contract (docs/BENCHMARKS.md).
        let corpus = generate(99, 2000);
        let joined: String = corpus.iter().map(|m| m.content.as_str()).collect();
        assert!(
            joined.contains("Decidido") || joined.contains("Optamos") || joined.contains("Prefiro")
        );
        assert!(
            joined.contains("Decided") || joined.contains("Chose") || joined.contains("prefers")
        );
    }
}
