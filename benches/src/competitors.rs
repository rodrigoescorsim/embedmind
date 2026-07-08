//! Pinned competitor registry and comparison adapters (`docs/BENCHMARKS.md`
//! §1/§4).
//!
//! The methodology's contract is strict: competitors are compared "in **pinned
//! and recorded** versions, under the same load", and we "publish every metric
//! we measure, **including where EmbedMind loses**" — but equally we never
//! fabricate a number. So this module does two things:
//!
//! 1. **Pins the versions in version-controlled constants** ([`COMPETITORS`]),
//!    so the target version is recorded in the repo and rendered into every
//!    results table, whether or not the competitor actually ran on a given
//!    machine.
//! 2. Runs each competitor **only when its build feature is enabled and its
//!    native dependency is present**; otherwise it reports
//!    [`CompetitorOutcome::NotMeasured`] with the reason, so the table shows an
//!    honest "not measured on this run (target vX.Y)" instead of a made-up row.
//!
//! sqlite-vec (a SQLite extension, C) and zvec (Zig) both need a native
//! toolchain that a pure-`cargo bench` box may not have. Gating them behind
//! `--features compare-sqlite-vec` / `compare-zvec` keeps the default harness
//! buildable everywhere (the CI regression guard, BENCHMARKS.md §5, runs the
//! EmbedMind-only metrics), while a release-run box with the toolchains flips
//! the features on to fill the comparison columns.
//!
//! When a real adapter is wired (see [`run_sqlite_vec`]/[`run_zvec`]), it must
//! obey the same rules the EmbedMind path does: **same normalized vectors, same
//! queries, same k**, default/recommended settings for the competitor (no
//! de-tuning), and it fills a [`CompetitorMetrics`] measured identically.

use crate::dataset::VectorSet;

/// A benchmarked competitor and the exact version this harness targets. The
/// version string is what lands in the results table's "version" cell — it is
/// the recorded pin, independent of whether the adapter ran.
#[derive(Debug, Clone, Copy)]
pub struct Competitor {
    /// Display name for the results table.
    pub name: &'static str,
    /// Pinned target version (`docs/BENCHMARKS.md` §1: "pinned versions,
    /// recorded in results"). Update this in lockstep with the adapter.
    pub version: &'static str,
    /// The build feature that enables this competitor's adapter. When the
    /// feature is off, the harness records [`CompetitorOutcome::NotMeasured`].
    pub feature: &'static str,
    /// One-line note on settings used / why it is the fair comparison, shown
    /// under the table.
    pub note: &'static str,
}

/// The competitors the methodology names (`docs/BENCHMARKS.md` §1), with the
/// versions this harness is pinned to. **Version-controlled** — bumping a
/// competitor is a reviewed commit, and the number in the table always traces
/// back to this constant.
pub const COMPETITORS: &[Competitor] = &[
    Competitor {
        name: "sqlite-vec",
        // The incumbent "embedded vector search in one file". Pinned to the
        // release the v0.1 comparison was run against; recorded here so the
        // table's version cell is reproducible.
        version: "0.1.6",
        feature: "compare-sqlite-vec",
        note: "SQLite extension, default page size, vec0 virtual table, brute-force KNN (its recommended small-scale path).",
    },
    Competitor {
        name: "zvec",
        // The closest new embedded vector store.
        version: "0.2.0",
        feature: "compare-zvec",
        note: "Zig embedded vector store, default HNSW settings.",
    },
];

/// The metrics a competitor is graded on — the same shape as EmbedMind's own
/// row so the renderer can lay them side by side. All optional: an adapter that
/// cannot measure something (e.g. a store that does not embed) leaves it
/// `None`, rendered as `—`.
#[derive(Debug, Clone, Default)]
pub struct CompetitorMetrics {
    /// recall@10 vs. the shared brute-force baseline.
    pub recall_at_10: Option<f64>,
    /// Warm query latency p50 / p99, milliseconds.
    pub query_p50_ms: Option<f64>,
    pub query_p99_ms: Option<f64>,
    /// Cold-open first-query latency, milliseconds.
    pub cold_open_ms: Option<f64>,
    /// Ingest throughput, memories/sec (vectors only — competitors don't embed).
    pub ingest_vecs_per_sec: Option<f64>,
    /// On-disk file size after ingest, bytes.
    pub file_bytes: Option<u64>,
    /// Peak RSS during the run, mebibytes.
    pub peak_rss_mib: Option<f64>,
}

/// Outcome of attempting a competitor comparison: either real numbers, or an
/// honest record of why they are absent (never a fabricated row).
#[derive(Debug, Clone)]
pub enum CompetitorOutcome {
    /// The adapter ran and produced numbers.
    Measured(CompetitorMetrics),
    /// The adapter did not run; `reason` is shown in the table
    /// (e.g. "feature `compare-sqlite-vec` disabled").
    NotMeasured { reason: String },
}

/// Runs every registered competitor over the same `set`/`queries`/`k` and
/// returns each outcome paired with its pin. Adapters that are not compiled in
/// return [`CompetitorOutcome::NotMeasured`] — so the caller always gets one
/// entry per competitor and the table is complete and honest.
pub fn run_all(
    set: &VectorSet,
    queries: &[Vec<f32>],
    k: usize,
) -> Vec<(&'static Competitor, CompetitorOutcome)> {
    COMPETITORS
        .iter()
        .map(|c| {
            let outcome = match c.name {
                "sqlite-vec" => run_sqlite_vec(c, set, queries, k),
                "zvec" => run_zvec(c, set, queries, k),
                _ => CompetitorOutcome::NotMeasured {
                    reason: "no adapter".to_string(),
                },
            };
            (c, outcome)
        })
        .collect()
}

/// sqlite-vec adapter. Real implementation lives behind
/// `--features compare-sqlite-vec` (needs the SQLite `vec0` extension /
/// `rusqlite` bundled build). Without the feature it records why it is absent.
#[cfg(not(feature = "compare-sqlite-vec"))]
fn run_sqlite_vec(
    c: &Competitor,
    _set: &VectorSet,
    _queries: &[Vec<f32>],
    _k: usize,
) -> CompetitorOutcome {
    CompetitorOutcome::NotMeasured {
        reason: format!(
            "feature `{}` disabled (build with it + the sqlite-vec {} extension to fill this row)",
            c.feature, c.version
        ),
    }
}

/// zvec adapter. Real implementation lives behind `--features compare-zvec`
/// (needs the Zig toolchain to build zvec). Without the feature it records why.
#[cfg(not(feature = "compare-zvec"))]
fn run_zvec(
    c: &Competitor,
    _set: &VectorSet,
    _queries: &[Vec<f32>],
    _k: usize,
) -> CompetitorOutcome {
    CompetitorOutcome::NotMeasured {
        reason: format!(
            "feature `{}` disabled (build with it + a zvec {} build to fill this row)",
            c.feature, c.version
        ),
    }
}

// The real adapters are compiled only when their feature is on. They are
// intentionally left as a wired stub returning NotMeasured until the native
// toolchain is available on the release box: the point of Part 2 is that the
// *harness shape* is complete and the pins are recorded, so flipping the
// feature and dropping in the native calls is a localized change, not a
// re-architecture. The methodology (same vectors/queries/k) is enforced by the
// signature these must satisfy.
#[cfg(feature = "compare-sqlite-vec")]
fn run_sqlite_vec(
    _c: &Competitor,
    _set: &VectorSet,
    _queries: &[Vec<f32>],
    _k: usize,
) -> CompetitorOutcome {
    CompetitorOutcome::NotMeasured {
        reason: "compare-sqlite-vec enabled but native adapter not yet wired on this box"
            .to_string(),
    }
}

#[cfg(feature = "compare-zvec")]
fn run_zvec(
    _c: &Competitor,
    _set: &VectorSet,
    _queries: &[Vec<f32>],
    _k: usize,
) -> CompetitorOutcome {
    CompetitorOutcome::NotMeasured {
        reason: "compare-zvec enabled but native adapter not yet wired on this box".to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_competitor_has_a_pinned_version() {
        for c in COMPETITORS {
            assert!(!c.version.is_empty(), "{} has no pinned version", c.name);
            assert!(!c.feature.is_empty());
        }
    }

    #[test]
    fn run_all_returns_one_outcome_per_competitor() {
        let set = VectorSet {
            dims: 2,
            entries: vec![],
        };
        let outcomes = run_all(&set, &[], 10);
        assert_eq!(outcomes.len(), COMPETITORS.len());
        // With no features enabled, all are honestly NotMeasured — never fake.
        for (_, o) in &outcomes {
            assert!(matches!(o, CompetitorOutcome::NotMeasured { .. }));
        }
    }
}
