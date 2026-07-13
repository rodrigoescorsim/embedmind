//! Results rendering + NFR validation (`docs/BENCHMARKS.md` §3/§4, spec §NFR).
//!
//! Turns [`crate::harness::SuiteResult`]s (EmbedMind) and
//! [`crate::competitors`] outcomes into:
//!
//! - a **README-ready markdown table**, with the competitor columns and their
//!   pinned versions, rendering "not measured on this run" honestly rather than
//!   inventing numbers, and an explicit **"where EmbedMind loses"** section
//!   (BENCHMARKS.md §4 rule 1: publish losses);
//! - an **NFR verdict** for the spec's hard numbers (recall p99 < 50 ms @ 100k,
//!   `remember` p99 < 200 ms, RAM < 300 MB @ 100k), reported even when missed
//!   (§4 rule 1);
//! - a **run environment header** (machine, OS, date, versions) so every table
//!   states its provenance (BENCHMARKS.md §3).
//!
//! No product logic; pure formatting + threshold checks over measured numbers.

use std::fmt::Write as _;

use crate::competitors::{Competitor, CompetitorOutcome};
use crate::fts_compare::{EMBEDMIND_FTS_SCOPE, FtsOutcome, TANTIVY_SCOPE, TANTIVY_VERSION};
use crate::harness::SuiteResult;

/// The spec's numeric NFRs (docs/01-spec.md §NFR / DESIGN targets). Kept here so
/// the pass/fail thresholds are version-controlled next to the checker.
pub mod nfr {
    /// `recall` p99 latency ceiling at 100k memories, CPU-only (ms).
    pub const RECALL_P99_MS_AT_100K: f64 = 50.0;
    /// `remember` p99 latency ceiling (ms) — dominated by embedding.
    pub const REMEMBER_P99_MS: f64 = 200.0;
    /// Peak RAM ceiling at 100k memories (MiB).
    pub const RAM_MIB_AT_100K: f64 = 300.0;
    /// Dataset size at which the latency/RAM NFRs are stated.
    pub const NFR_DATASET_COUNT: usize = 100_000;
}

/// One NFR check: what it targets, the measured value, and whether it passed.
#[derive(Debug, Clone)]
pub struct NfrCheck {
    pub name: &'static str,
    pub target: String,
    pub measured: String,
    pub passed: bool,
    /// True when the NFR could not be evaluated on this run (e.g. the 100k
    /// dataset was not part of the run) — reported as "n/a", not a pass.
    pub not_applicable: bool,
}

/// Recorded facts about the run environment (BENCHMARKS.md §3: "every results
/// table states machine, OS, versions, date"). Captured from the host at run
/// time; the version is the workspace crate version.
#[derive(Debug, Clone)]
pub struct RunEnv {
    pub os: String,
    pub arch: String,
    pub cpus: usize,
    pub embedmind_version: String,
    pub date_utc: String,
}

impl RunEnv {
    /// Captures the current environment. `date_utc` is the run date; kept as a
    /// caller-supplied string (the harness passes an ISO date) so this module
    /// stays free of a time dependency.
    pub fn capture(date_utc: impl Into<String>) -> Self {
        RunEnv {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            embedmind_version: env!("CARGO_PKG_VERSION").to_string(),
            date_utc: date_utc.into(),
        }
    }
}

/// Validates the spec NFRs against the measured results. Latency/RAM NFRs are
/// stated at 100k, so they are checked against that dataset's result if present
/// and reported `n/a` otherwise (never silently passed). `remember` p99 is
/// size-independent, so the largest available run is used.
pub fn check_nfrs(results: &[SuiteResult]) -> Vec<NfrCheck> {
    let at_100k = results.iter().find(|r| r.count == nfr::NFR_DATASET_COUNT);
    // Largest run available, for the size-independent remember NFR.
    let largest = results.iter().max_by_key(|r| r.count);

    let mut checks = Vec::new();

    // recall p99 < 50 ms @ 100k
    checks.push(match at_100k {
        Some(r) => NfrCheck {
            name: "recall p99 @ 100k (CPU-only)",
            target: format!("< {:.0} ms", nfr::RECALL_P99_MS_AT_100K),
            measured: format!("{:.2} ms", r.query_p99_ms),
            passed: r.query_p99_ms < nfr::RECALL_P99_MS_AT_100K,
            not_applicable: false,
        },
        None => NfrCheck {
            name: "recall p99 @ 100k (CPU-only)",
            target: format!("< {:.0} ms", nfr::RECALL_P99_MS_AT_100K),
            measured: "n/a (100k not in this run)".to_string(),
            passed: false,
            not_applicable: true,
        },
    });

    // remember p99 < 200 ms (any size; report the largest run)
    checks.push(match largest {
        Some(r) => NfrCheck {
            name: "remember p99 (end-to-end, incl. embedding)",
            target: format!("< {:.0} ms", nfr::REMEMBER_P99_MS),
            measured: format!("{:.2} ms (@ {})", r.remember_p99_ms, human_count(r.count)),
            passed: r.remember_p99_ms < nfr::REMEMBER_P99_MS,
            not_applicable: false,
        },
        None => NfrCheck {
            name: "remember p99 (end-to-end, incl. embedding)",
            target: format!("< {:.0} ms", nfr::REMEMBER_P99_MS),
            measured: "n/a (no run)".to_string(),
            passed: false,
            not_applicable: true,
        },
    });

    // RAM < 300 MB @ 100k (peak of ingest/query phases)
    checks.push(match at_100k {
        Some(r) => {
            let peak = r.peak_rss_ingest_mib.max(r.peak_rss_query_mib);
            NfrCheck {
                name: "peak RAM @ 100k",
                target: format!("< {:.0} MiB", nfr::RAM_MIB_AT_100K),
                measured: format!("{peak:.1} MiB"),
                passed: peak < nfr::RAM_MIB_AT_100K,
                not_applicable: false,
            }
        }
        None => NfrCheck {
            name: "peak RAM @ 100k",
            target: format!("< {:.0} MiB", nfr::RAM_MIB_AT_100K),
            measured: "n/a (100k not in this run)".to_string(),
            passed: false,
            not_applicable: true,
        },
    });

    checks
}

/// Renders the whole README-ready markdown report: environment header, the
/// metric table (EmbedMind + competitor columns), the honest losses section,
/// and the NFR verdict. `competitors` is the outcome list from
/// [`crate::competitors::run_all`], assumed identical across datasets (same
/// pins), so its versions are taken from the first dataset's run.
pub fn render_markdown(
    env: &RunEnv,
    results: &[SuiteResult],
    competitors: &[(&'static Competitor, CompetitorOutcome)],
    compared_on: Option<&str>,
) -> String {
    render_markdown_with_fts(env, results, competitors, compared_on, None)
}

/// Same as [`render_markdown`], plus the full-text-only (BM25) comparison
/// section when the caller ran it (`crate::fts_compare`, founder review
/// 2026-07-13, external measurement for ADR 0011). `fts` is `None` when the
/// caller did not run that comparison on this invocation — the section is
/// simply omitted, never rendered with fabricated numbers.
pub fn render_markdown_with_fts(
    env: &RunEnv,
    results: &[SuiteResult],
    competitors: &[(&'static Competitor, CompetitorOutcome)],
    compared_on: Option<&str>,
    fts: Option<(&FtsOutcome, &FtsOutcome)>,
) -> String {
    let mut out = String::new();

    // --- provenance header ---
    let _ = writeln!(out, "## Benchmark results\n");
    let _ = writeln!(
        out,
        "_Machine: {} / {}, {} logical CPUs · EmbedMind {} · {} · CPU-only, single-thread._\n",
        env.os, env.arch, env.cpus, env.embedmind_version, env.date_utc
    );

    // The dataset the competitor comparison ran on: the pinned `compared_on` if
    // given, else the largest in the run (matching run_all's default). Both the
    // comparison table and the losses section are stated against this exact
    // dataset so the head-to-head is apples-to-apples.
    let compare_result = compared_on
        .and_then(|name| results.iter().find(|r| r.dataset == name))
        .or_else(|| results.iter().max_by_key(|r| r.count));

    // --- metric table (one column per dataset) ---
    render_metric_table(&mut out, results);

    // --- full-text lift: lexical queries (founder review 2026-07-13) ---
    render_lexical_lift_table(&mut out, results);

    // --- competitor comparison: two labeled planes (BENCHMARKS.md §1, S17) ---
    render_index_only_table(&mut out, compare_result, competitors);
    render_text_to_result_table(&mut out, compare_result, competitors);

    // --- full-text-only (BM25): EmbedMind vs. tantivy (founder review
    // 2026-07-13, external measurement for ADR 0011) ---
    if let Some((embedmind, tantivy)) = fts {
        render_fts_only_table(&mut out, embedmind, tantivy);
    }

    // --- honesty: where EmbedMind loses ---
    render_losses(&mut out, compare_result, competitors);

    // --- NFR verdict ---
    render_nfr_table(&mut out, results);

    out
}

fn render_metric_table(out: &mut String, results: &[SuiteResult]) {
    let _ = writeln!(out, "### EmbedMind\n");
    // Header: Metric | dataset1 | dataset2 | ...
    let mut header = String::from("| Metric |");
    let mut sep = String::from("|---|");
    for r in results {
        let _ = write!(header, " {} |", r.dataset);
        sep.push_str("---:|");
    }
    let _ = writeln!(out, "{header}");
    let _ = writeln!(out, "{sep}");

    row(out, "memories", results, |r| human_count(r.count));
    row(out, "recall@10 (vs brute-force)", results, |r| {
        format!("{:.4}", r.recall.recall_at_k)
    });
    row(out, "recall@10 min (worst query)", results, |r| {
        format!("{:.4}", r.recall.min_recall)
    });
    row(out, "recall@10 p10 / p50 (per query)", results, |r| {
        format!("{:.4} / {:.4}", r.recall.p10_recall, r.recall.p50_recall)
    });
    row(out, "query p50 (warm)", results, |r| {
        format!("{:.2} ms", r.query_p50_ms)
    });
    row(out, "query p99 (warm)", results, |r| {
        format!("{:.2} ms", r.query_p99_ms)
    });
    row(out, "↳ query embed p50 / p99", results, |r| {
        format!(
            "{:.2} / {:.2} ms",
            r.query_embed_p50_ms, r.query_embed_p99_ms
        )
    });
    row(out, "↳ query engine p50 / p99", results, |r| {
        format!(
            "{:.2} / {:.2} ms",
            r.query_engine_p50_ms, r.query_engine_p99_ms
        )
    });
    row(
        out,
        "↳ query vector-only p50 / p99 (no FTS/fusion)",
        results,
        |r| {
            format!(
                "{:.2} / {:.2} ms",
                r.query_vector_p50_ms, r.query_vector_p99_ms
            )
        },
    );
    row(out, "query first (cold-open)", results, |r| {
        format!("{:.2} ms", r.cold_first_query_ms)
    });
    row(out, "cold open (Store::open)", results, |r| {
        format!("{:.2} ms", r.cold_open_ms)
    });
    row(out, "remember p50 (e2e, w/ embed)", results, |r| {
        format!("{:.2} ms", r.remember_p50_ms)
    });
    row(out, "remember p99 (e2e, w/ embed)", results, |r| {
        format!("{:.2} ms", r.remember_p99_ms)
    });
    row(out, "ingest throughput", results, |r| {
        format!("{:.0} mem/s", r.ingest_per_sec)
    });
    row(out, "file size on disk", results, |r| {
        human_bytes(r.file_bytes)
    });
    row(out, "peak RSS (ingest)", results, |r| {
        format!("{:.1} MiB", r.peak_rss_ingest_mib)
    });
    row(out, "peak RSS (query)", results, |r| {
        format!("{:.1} MiB", r.peak_rss_query_mib)
    });
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "_Warm query latency is end-to-end (the engine embeds the query text itself). The `↳` rows decompose it per query (S17): `embed` = embedding the query with the built-in ONNX model; `engine` = hybrid search + RRF fusion + record load with the vector ready — the number comparable to vector-in baselines. Percentiles are computed per component, so embed + engine need not equal the total percentile._\n"
    );
    let _ = writeln!(
        out,
        "_`remember` latency is end-to-end and includes embedding — the baselines below don't embed, so their ingest is vectors-only and not comparable to this row (BENCHMARKS.md §1)._\n"
    );
    let _ = writeln!(
        out,
        "_`query vector-only` is `Store::recall_vector` (HNSW half only, no BM25/RRF fusion) on the same query set, timed right after the hybrid call for cache parity — comparable to `engine` above (both exclude embed time). The delta between the two isolates the FTS+fusion cost from everything the vector half pays too._\n"
    );
}

/// Full-text "lift" on lexical queries (founder review 2026-07-13): recall@10
/// and p99 latency of hybrid (`Store::recall`) vs. vector-only
/// (`Store::recall_vector`) over the *same* ground-truth-by-construction
/// lexical queries (`crate::lexical`) — exact code identifiers, ULIDs, error
/// strings, CLI flags. The `recall@10` row above only ever measures the
/// vector half against semantic-paraphrase queries (by design, to isolate
/// HNSW quality); this table is the other half of the question: what does
/// full-text actually buy on queries an embedding tends to get wrong. The
/// delta row makes that benefit explicit rather than left for a reader to
/// subtract by eye.
fn render_lexical_lift_table(out: &mut String, results: &[SuiteResult]) {
    let _ = writeln!(
        out,
        "### Full-text lift: lexical queries (hybrid vs. vector-only)\n"
    );
    let _ = writeln!(
        out,
        "_Ground truth by construction: each query is the exact literal (a code identifier, ULID, hex hash, CLI flag, or error-message fragment) of one ingested memory; a hit is that memory appearing in the top-10. Same queries, same `k`, run through both recall strategies on the same materialized dataset (`crate::lexical`)._\n"
    );

    let mut header = String::from("| Metric |");
    let mut sep = String::from("|---|");
    for r in results {
        let _ = write!(header, " {} |", r.dataset);
        sep.push_str("---:|");
    }
    let _ = writeln!(out, "{header}");
    let _ = writeln!(out, "{sep}");

    row(out, "lexical cases", results, |r| {
        r.lexical_lift.hybrid.queries.to_string()
    });
    row(
        out,
        "recall@10 — hybrid (BM25+vector+RRF)",
        results,
        |r| format!("{:.4}", r.lexical_lift.hybrid.recall_at_k),
    );
    row(out, "recall@10 — vector-only", results, |r| {
        format!("{:.4}", r.lexical_lift.vector_only.recall_at_k)
    });
    row(out, "↳ full-text lift (Δ recall@10)", results, |r| {
        format!(
            "{:+.4}",
            r.lexical_lift.hybrid.recall_at_k - r.lexical_lift.vector_only.recall_at_k
        )
    });
    row(out, "query p50 / p99 — hybrid", results, |r| {
        format!(
            "{:.2} / {:.2} ms",
            r.lexical_lift.hybrid.latency.p50_ms, r.lexical_lift.hybrid.latency.p99_ms
        )
    });
    row(out, "query p50 / p99 — vector-only", results, |r| {
        format!(
            "{:.2} / {:.2} ms",
            r.lexical_lift.vector_only.latency.p50_ms, r.lexical_lift.vector_only.latency.p99_ms
        )
    });
    let _ = writeln!(out);
}

/// Full-text-only (BM25) comparison: EmbedMind's own inverted index
/// (`Store::search_text`) vs. tantivy (`crate::fts_compare`), the external
/// measurement gap ADR 0011 never had a number for (founder review
/// 2026-07-13). Unlike the two vector planes above, both sides here are
/// full-text engines — recall is graded against the same lexical
/// ground-truth-by-construction cases `crate::lexical` uses, not a
/// brute-force vector baseline.
fn render_fts_only_table(out: &mut String, embedmind: &FtsOutcome, tantivy: &FtsOutcome) {
    let _ = writeln!(
        out,
        "### Full-text only (BM25): EmbedMind vs. tantivy (same corpus, same queries, same k)\n"
    );
    let _ = writeln!(
        out,
        "_ADR 0011 rejected embedding tantivy for an **architectural** reason — it writes its own \
         segments outside the `.mind` file with its own commit schedule, which would give the engine \
         two independent sources of commit truth (CLAUDE.md decision 4, \"crash-safety before features\"). \
         That decision does not depend on which engine is faster, and this table does not reopen it — it \
         only puts a number on the tradeoff already made. What to do with the number (optimize the \
         caseworn BM25 further, accept the gap, or revisit) is left to the founder. Ground truth is by \
         construction (`crate::lexical`): each query is the exact literal of one ingested document, so \
         recall is graded against an unambiguous target, not a brute-force oracle. Rows that could not \
         run on this machine say so explicitly — never fabricated (BENCHMARKS.md §4 rule 6 scope notes \
         apply here too)._\n"
    );
    let _ = writeln!(
        out,
        "| System | Version | recall@10 (lexical) | query p50 | query p99 | ingest (docs/sec) | on-disk size | returns | persists |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|---:|---:|---|---|");

    render_fts_row(
        out,
        "EmbedMind",
        env!("CARGO_PKG_VERSION"),
        embedmind,
        &EMBEDMIND_FTS_SCOPE,
    );
    render_fts_row(out, "tantivy", TANTIVY_VERSION, tantivy, &TANTIVY_SCOPE);
    let _ = writeln!(out);
}

fn render_fts_row(
    out: &mut String,
    name: &str,
    version: &str,
    outcome: &FtsOutcome,
    scope: &crate::fts_compare::FtsScope,
) {
    match outcome {
        FtsOutcome::Measured(m) => {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                name,
                version,
                opt_f4(m.recall_at_k),
                opt_ms(m.query_p50_ms),
                opt_ms(m.query_p99_ms),
                opt_per_sec(m.ingest_per_sec),
                m.index_bytes.map(human_bytes).unwrap_or_else(|| "—".into()),
                scope.returns,
                scope.persists,
            );
        }
        FtsOutcome::NotMeasured { reason } => {
            let _ = writeln!(
                out,
                "| {name} | {version} (target) | _not measured_ | _not measured_ | _not measured_ | _not measured_ | _not measured_ | {} | {} |",
                scope.returns, scope.persists
            );
            let _ = writeln!(out, "|   ↳ | | | | | | _{reason}_ | | |");
        }
    }
}

/// Plane 1 (BENCHMARKS.md §1, S17): **index-only** — pre-computed vectors in,
/// ids out. Isolates index quality; the only plane where a vector-only store
/// can legitimately win, since it never pays an embedding cost here. EmbedMind
/// is shown via its `query engine` split (search + fusion + record load, no
/// embed) — the number comparable to a baseline that receives ready-made
/// vectors.
fn render_index_only_table(
    out: &mut String,
    compare_result: Option<&SuiteResult>,
    competitors: &[(&'static Competitor, CompetitorOutcome)],
) {
    let _ = writeln!(
        out,
        "### vs. baselines — index-only (same vectors, same queries, same k)\n"
    );
    let biggest = compare_result;
    let ds = biggest.map(|r| r.dataset).unwrap_or("—");
    let _ = writeln!(
        out,
        "_Comparison on `{ds}`. Competitor versions are pinned in `benches/src/competitors.rs` and recorded here (BENCHMARKS.md §1). Rows that could not run on this machine say so explicitly — never fabricated. This plane hands every system, including EmbedMind, the identical pre-computed vector — EmbedMind's row is its `query engine` split (search + fusion + record load, embed time excluded, S17), the like-for-like number against a baseline that never embeds. It answers \"whose index is better\", the plane where a vector-only store can legitimately win (see the text→result plane below for the product workload, where it can't skip the embedding toll). Each row states its **scope** — what it returns and what it persists (BENCHMARKS.md §4 rule 6): a smaller file or a faster query that does less is not a win row._\n"
    );
    let _ = writeln!(
        out,
        "| System | Version | recall@10 | query p50 | query p99 | ingest (vec-only) | on-disk size | returns | persists |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|---:|---:|---|---|");

    // EmbedMind's own row on the biggest dataset, for side-by-side reading.
    if let Some(r) = biggest {
        let _ = writeln!(
            out,
            "| **EmbedMind** | {} | {:.4} | {:.2} ms | {:.2} ms | — (embeds; see note) | {} | {} | {} |",
            env!("CARGO_PKG_VERSION"),
            r.recall.recall_at_k,
            r.query_engine_p50_ms,
            r.query_engine_p99_ms,
            human_bytes(r.file_bytes),
            EMBEDMIND_SCOPE.returns,
            EMBEDMIND_SCOPE.persists,
        );
    }

    for (c, outcome) in competitors {
        match outcome {
            CompetitorOutcome::Measured(m) => {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    c.name,
                    c.version,
                    opt_f4(m.recall_at_10),
                    opt_ms(m.query_p50_ms),
                    opt_ms(m.query_p99_ms),
                    opt_per_sec(m.ingest_vecs_per_sec),
                    m.file_bytes.map(human_bytes).unwrap_or_else(|| "—".into()),
                    c.scope.returns,
                    c.scope.persists,
                );
            }
            CompetitorOutcome::NotMeasured { reason } => {
                let _ = writeln!(
                    out,
                    "| {} | {} (target) | _not measured_ | _not measured_ | _not measured_ | _not measured_ | _not measured_ | {} | {} |",
                    c.name, c.version, c.scope.returns, c.scope.persists
                );
                let _ = writeln!(out, "|   ↳ | | | | | | _{reason}_ | | |");
            }
        }
    }
    let _ = writeln!(out);
    for (c, _) in competitors {
        let _ = writeln!(out, "- **{}** ({}): {}", c.name, c.version, c.note);
    }
    let _ = writeln!(out);
}

/// Plane 2 (BENCHMARKS.md §1, S17): **text→result** — text in, results out,
/// the product workload. Every system pays the same embedding toll: the query
/// is embedded once with the shared ONNX pipeline (measured *outside* every
/// competitor, via [`SuiteResult::query_embed_p50_ms`]/`_p99_ms`) and that cost
/// is added to the competitor's own index-only query time, so its row is
/// genuinely end-to-end — the same shape as EmbedMind's own `query_p50/p99_ms`,
/// which already embeds internally. Percentiles are summed per component (not
/// recomposed from raw samples), the same approximation the metric table above
/// already documents for embed+engine.
fn render_text_to_result_table(
    out: &mut String,
    compare_result: Option<&SuiteResult>,
    competitors: &[(&'static Competitor, CompetitorOutcome)],
) {
    let _ = writeln!(
        out,
        "### vs. baselines — text→result (same embedding toll, same queries, same k)\n"
    );
    let biggest = compare_result;
    let ds = biggest.map(|r| r.dataset).unwrap_or("—");
    let _ = writeln!(
        out,
        "_Comparison on `{ds}`. The product question: an agent developer hands text in and gets results out. Every system here pays for embedding the query with the same all-MiniLM-L6-v2 pipeline (BENCHMARKS.md §1) — for the baselines it is measured separately (outside their own timing, via EmbedMind's `query embed` split on this run) and added to their index-only query time; EmbedMind's own `query p50/p99` already include it end-to-end. This is the plane index-only comparisons hide: a vector-only store cannot skip this toll in real use. recall@10 is EmbedMind's own end-to-end figure against each competitor's index-only recall — the index quality question is answered by the plane above, not repeated here. Rows that could not run on this machine say so explicitly — never fabricated._\n"
    );
    let _ = writeln!(
        out,
        "| System | Version | recall@10 | query p50 (embed + query) | query p99 (embed + query) |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|");

    if let Some(r) = biggest {
        let _ = writeln!(
            out,
            "| **EmbedMind** | {} | {:.4} | {:.2} ms | {:.2} ms |",
            env!("CARGO_PKG_VERSION"),
            r.recall.recall_at_k,
            r.query_p50_ms,
            r.query_p99_ms,
        );
    }

    for (c, outcome) in competitors {
        match outcome {
            CompetitorOutcome::Measured(m) => {
                let (p50, p99) = match biggest {
                    Some(r) => (
                        sum_opt(m.query_p50_ms, Some(r.query_embed_p50_ms)),
                        sum_opt(m.query_p99_ms, Some(r.query_embed_p99_ms)),
                    ),
                    None => (None, None),
                };
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} |",
                    c.name,
                    c.version,
                    opt_f4(m.recall_at_10),
                    opt_ms(p50),
                    opt_ms(p99),
                );
            }
            CompetitorOutcome::NotMeasured { reason } => {
                let _ = writeln!(
                    out,
                    "| {} | {} (target) | _not measured_ | _not measured_ | _not measured_ |",
                    c.name, c.version
                );
                let _ = writeln!(out, "|   ↳ | | | _{reason}_ | |");
            }
        }
    }
    let _ = writeln!(out);
}

/// Adds two optional latencies (the query embed cost, measured once outside
/// every competitor, plus the competitor's own index-only query time). `None`
/// propagates — a component that could not be measured must never silently
/// render as if it were zero.
fn sum_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        _ => None,
    }
}

/// EmbedMind's own scope for the comparison table (`docs/BENCHMARKS.md` §4
/// rule 6) — kept next to [`crate::competitors::Scope`] so both sides of the
/// comparison state their scope the same way.
const EMBEDMIND_SCOPE: crate::competitors::Scope = crate::competitors::Scope {
    returns: "full content + metadata + provenance",
    persists: "text + metadata + full-text index + vectors",
};

fn render_losses(
    out: &mut String,
    compare_result: Option<&SuiteResult>,
    competitors: &[(&'static Competitor, CompetitorOutcome)],
) {
    let _ = writeln!(out, "### Where EmbedMind loses (honesty contract)\n");
    let biggest = compare_result;
    let mut any = false;

    if let Some(r) = biggest {
        for (c, outcome) in competitors {
            if let CompetitorOutcome::Measured(m) = outcome {
                if let Some(cr) = m.recall_at_10
                    && cr > r.recall.recall_at_k + 1e-6
                {
                    let _ = writeln!(
                        out,
                        "- **recall@10**: {} {:.4} beats EmbedMind {:.4} on `{}`.",
                        c.name, cr, r.recall.recall_at_k, r.dataset
                    );
                    any = true;
                }
                if let Some(cp) = m.query_p99_ms
                    && cp < r.query_engine_p99_ms - 1e-6
                {
                    let _ = writeln!(
                        out,
                        "- **query p99 (index-only)**: {} {:.2} ms beats EmbedMind's engine time {:.2} ms on `{}`.",
                        c.name, cp, r.query_engine_p99_ms, r.dataset
                    );
                    any = true;
                }
                if let Some(cp) = sum_opt(m.query_p99_ms, Some(r.query_embed_p99_ms))
                    && cp < r.query_p99_ms - 1e-6
                {
                    let _ = writeln!(
                        out,
                        "- **query p99 (text→result)**: {} {:.2} ms (embed + query) beats EmbedMind {:.2} ms on `{}`.",
                        c.name, cp, r.query_p99_ms, r.dataset
                    );
                    any = true;
                }
                if let Some(cb) = m.file_bytes
                    && cb < r.file_bytes
                {
                    let _ = writeln!(
                        out,
                        "- **on-disk size**: {} {} beats EmbedMind {} on `{}`.",
                        c.name,
                        human_bytes(cb),
                        human_bytes(r.file_bytes),
                        r.dataset
                    );
                    any = true;
                }
            }
        }
    }

    if !any {
        let _ = writeln!(
            out,
            "- No competitor was measured on this run (see the note above), so no head-to-head loss can be reported yet. When a baseline is measured and wins a metric, it is listed here automatically — the harness computes this section, it is never hand-edited (BENCHMARKS.md §4 rule 3)."
        );
    }
    let _ = writeln!(out);
}

fn render_nfr_table(out: &mut String, results: &[SuiteResult]) {
    let _ = writeln!(out, "### NFR verdict (spec §NFR)\n");
    let _ = writeln!(out, "| NFR | Target | Measured | Verdict |");
    let _ = writeln!(out, "|---|---|---:|:---:|");
    for c in check_nfrs(results) {
        let verdict = if c.not_applicable {
            "n/a"
        } else if c.passed {
            "✅ pass"
        } else {
            "❌ **miss**"
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            c.name, c.target, c.measured, verdict
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "_Missed NFRs are reported, not hidden (BENCHMARKS.md §4 rule 1); they are tracked in CHANGELOG.md / docs/BENCHMARKS.md until met._\n"
    );
}

/// Emits a JSON results object (BENCHMARKS.md §4 rule 3: results are a
/// CI-generated file, `benches/results/<version>.json`). Hand-rolled to avoid
/// pulling serde into the harness (the engine forbids it in the core, and the
/// harness keeps its dep surface tiny).
pub fn render_json(
    env: &RunEnv,
    results: &[SuiteResult],
    competitors: &[(&'static Competitor, CompetitorOutcome)],
    compared_on: Option<&str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "  \"env\": {{");
    let _ = writeln!(out, "    \"os\": {},", jstr(&env.os));
    let _ = writeln!(out, "    \"arch\": {},", jstr(&env.arch));
    let _ = writeln!(out, "    \"cpus\": {},", env.cpus);
    let _ = writeln!(
        out,
        "    \"embedmind_version\": {},",
        jstr(&env.embedmind_version)
    );
    let _ = writeln!(out, "    \"date_utc\": {}", jstr(&env.date_utc));
    let _ = writeln!(out, "  }},");

    // Which dataset the competitor comparison ran on (BENCHMARKS.md §1: the
    // comparison is a single shared workload; recorded so the table's "on X"
    // header always traces back to a value in the results file).
    let compare_ds = compared_on
        .or_else(|| results.iter().max_by_key(|r| r.count).map(|r| r.dataset))
        .unwrap_or("");
    let _ = writeln!(out, "  \"compared_on\": {},", jstr(compare_ds));

    let _ = writeln!(out, "  \"datasets\": [");
    for (i, r) in results.iter().enumerate() {
        let comma = if i + 1 < results.len() { "," } else { "" };
        let _ = writeln!(out, "    {{");
        let _ = writeln!(out, "      \"dataset\": {},", jstr(r.dataset));
        let _ = writeln!(out, "      \"count\": {},", r.count);
        let _ = writeln!(out, "      \"dims\": {},", r.dims);
        let _ = writeln!(out, "      \"model_id\": {},", jstr(&r.model_id));
        let _ = writeln!(out, "      \"recall_at_10\": {:.6},", r.recall.recall_at_k);
        let _ = writeln!(
            out,
            "      \"recall_at_10_min\": {:.6},",
            r.recall.min_recall
        );
        let _ = writeln!(
            out,
            "      \"recall_at_10_p10\": {:.6},",
            r.recall.p10_recall
        );
        let _ = writeln!(
            out,
            "      \"recall_at_10_p50\": {:.6},",
            r.recall.p50_recall
        );
        let _ = writeln!(out, "      \"query_p50_ms\": {:.4},", r.query_p50_ms);
        let _ = writeln!(out, "      \"query_p99_ms\": {:.4},", r.query_p99_ms);
        let _ = writeln!(
            out,
            "      \"query_embed_p50_ms\": {:.4},",
            r.query_embed_p50_ms
        );
        let _ = writeln!(
            out,
            "      \"query_embed_p99_ms\": {:.4},",
            r.query_embed_p99_ms
        );
        let _ = writeln!(
            out,
            "      \"query_engine_p50_ms\": {:.4},",
            r.query_engine_p50_ms
        );
        let _ = writeln!(
            out,
            "      \"query_engine_p99_ms\": {:.4},",
            r.query_engine_p99_ms
        );
        let _ = writeln!(
            out,
            "      \"query_vector_p50_ms\": {:.4},",
            r.query_vector_p50_ms
        );
        let _ = writeln!(
            out,
            "      \"query_vector_p99_ms\": {:.4},",
            r.query_vector_p99_ms
        );
        let _ = writeln!(out, "      \"cold_open_ms\": {:.4},", r.cold_open_ms);
        let _ = writeln!(
            out,
            "      \"cold_first_query_ms\": {:.4},",
            r.cold_first_query_ms
        );
        let _ = writeln!(out, "      \"remember_p50_ms\": {:.4},", r.remember_p50_ms);
        let _ = writeln!(out, "      \"remember_p99_ms\": {:.4},", r.remember_p99_ms);
        let _ = writeln!(out, "      \"ingest_per_sec\": {:.2},", r.ingest_per_sec);
        let _ = writeln!(out, "      \"file_bytes\": {},", r.file_bytes);
        let _ = writeln!(
            out,
            "      \"peak_rss_ingest_mib\": {:.2},",
            r.peak_rss_ingest_mib
        );
        let _ = writeln!(
            out,
            "      \"peak_rss_query_mib\": {:.2},",
            r.peak_rss_query_mib
        );
        let _ = writeln!(out, "      \"lexical_lift\": {{");
        let _ = writeln!(out, "        \"cases\": {},", r.lexical_lift.hybrid.queries);
        let _ = writeln!(
            out,
            "        \"hybrid_recall_at_10\": {:.6},",
            r.lexical_lift.hybrid.recall_at_k
        );
        let _ = writeln!(
            out,
            "        \"vector_only_recall_at_10\": {:.6},",
            r.lexical_lift.vector_only.recall_at_k
        );
        let _ = writeln!(
            out,
            "        \"hybrid_query_p50_ms\": {:.4},",
            r.lexical_lift.hybrid.latency.p50_ms
        );
        let _ = writeln!(
            out,
            "        \"hybrid_query_p99_ms\": {:.4},",
            r.lexical_lift.hybrid.latency.p99_ms
        );
        let _ = writeln!(
            out,
            "        \"vector_only_query_p50_ms\": {:.4},",
            r.lexical_lift.vector_only.latency.p50_ms
        );
        let _ = writeln!(
            out,
            "        \"vector_only_query_p99_ms\": {:.4}",
            r.lexical_lift.vector_only.latency.p99_ms
        );
        let _ = writeln!(out, "      }}");
        let _ = writeln!(out, "    }}{comma}");
    }
    let _ = writeln!(out, "  ],");

    let _ = writeln!(out, "  \"competitors\": [");
    for (i, (c, outcome)) in competitors.iter().enumerate() {
        let comma = if i + 1 < competitors.len() { "," } else { "" };
        let _ = writeln!(out, "    {{");
        let _ = writeln!(out, "      \"name\": {},", jstr(c.name));
        let _ = writeln!(out, "      \"version\": {},", jstr(c.version));
        match outcome {
            CompetitorOutcome::Measured(m) => {
                // Emit the measured numbers too, so `<version>.json` (the
                // CI-generated source of truth, BENCHMARKS.md §4 rule 3) carries
                // every competitor value that the rendered table shows — a
                // claim in the table always traces back to a field here.
                let _ = writeln!(out, "      \"measured\": true,");
                let _ = writeln!(out, "      \"metrics\": {{");
                let _ = writeln!(out, "        \"recall_at_10\": {},", jnum(m.recall_at_10));
                let _ = writeln!(out, "        \"query_p50_ms\": {},", jnum(m.query_p50_ms));
                let _ = writeln!(out, "        \"query_p99_ms\": {},", jnum(m.query_p99_ms));
                let _ = writeln!(
                    out,
                    "        \"ingest_vecs_per_sec\": {},",
                    jnum(m.ingest_vecs_per_sec)
                );
                let _ = writeln!(
                    out,
                    "        \"file_bytes\": {}",
                    m.file_bytes
                        .map(|b| b.to_string())
                        .unwrap_or_else(|| "null".into())
                );
                let _ = writeln!(out, "      }},");
                let _ = writeln!(out, "      \"reason\": {}", jstr(""));
            }
            CompetitorOutcome::NotMeasured { reason } => {
                let _ = writeln!(out, "      \"measured\": false,");
                let _ = writeln!(out, "      \"metrics\": null,");
                let _ = writeln!(out, "      \"reason\": {}", jstr(reason));
            }
        }
        let _ = writeln!(out, "    }}{comma}");
    }
    let _ = writeln!(out, "  ],");

    let _ = writeln!(out, "  \"nfrs\": [");
    let checks = check_nfrs(results);
    for (i, c) in checks.iter().enumerate() {
        let comma = if i + 1 < checks.len() { "," } else { "" };
        let _ = writeln!(out, "    {{");
        let _ = writeln!(out, "      \"name\": {},", jstr(c.name));
        let _ = writeln!(out, "      \"target\": {},", jstr(&c.target));
        let _ = writeln!(out, "      \"measured\": {},", jstr(&c.measured));
        let _ = writeln!(out, "      \"passed\": {},", c.passed);
        let _ = writeln!(out, "      \"not_applicable\": {}", c.not_applicable);
        let _ = writeln!(out, "    }}{comma}");
    }
    let _ = writeln!(out, "  ]");
    let _ = writeln!(out, "}}");
    out
}

// --- small formatting helpers ---

fn row(out: &mut String, label: &str, results: &[SuiteResult], f: impl Fn(&SuiteResult) -> String) {
    let mut line = format!("| {label} |");
    for r in results {
        let _ = write!(line, " {} |", f(r));
    }
    let _ = writeln!(out, "{line}");
}

/// Renders an optional number as a JSON value: the number itself, or `null`
/// when absent (a competitor metric the adapter could not measure).
fn jnum(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.6}"))
        .unwrap_or_else(|| "null".into())
}

fn opt_f4(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.4}")).unwrap_or_else(|| "—".into())
}
fn opt_ms(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2} ms"))
        .unwrap_or_else(|| "—".into())
}
fn opt_per_sec(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.0}/s")).unwrap_or_else(|| "—".into())
}

/// Human-readable count: `10000` → `10k`, `100000` → `100k`.
fn human_count(n: usize) -> String {
    if n >= 1000 && n.is_multiple_of(1000) {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// Human-readable byte size in MiB/GiB (on-disk file sizes).
fn human_bytes(b: u64) -> String {
    let mib = b as f64 / (1024.0 * 1024.0);
    if mib >= 1024.0 {
        format!("{:.2} GiB", mib / 1024.0)
    } else {
        format!("{mib:.1} MiB")
    }
}

/// Minimal JSON string escaping (quotes + backslash + control chars) — enough
/// for the values this harness emits (names, versions, os strings).
fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::competitors::COMPETITORS;
    use crate::recall::RecallReport;

    fn fake_result(name: &'static str, count: usize, p99: f64, rss: f64) -> SuiteResult {
        SuiteResult {
            dataset: name,
            count,
            dims: 384,
            model_id: "all-MiniLM-L6-v2-int8".into(),
            recall: RecallReport {
                k: 10,
                queries: 200,
                recall_at_k: 0.994,
                min_recall: 0.9,
                p10_recall: 0.95,
                p50_recall: 1.0,
            },
            query_p50_ms: 1.2,
            query_p99_ms: p99,
            query_mean_ms: 1.5,
            warm_queries: 200,
            query_embed_p50_ms: 0.9,
            query_embed_p99_ms: 1.8,
            query_engine_p50_ms: 0.3,
            query_engine_p99_ms: 0.7,
            query_vector_p50_ms: 0.8,
            query_vector_p99_ms: p99 * 0.6,
            cold_open_ms: 12.0,
            cold_first_query_ms: 30.0,
            remember_p50_ms: 40.0,
            remember_p99_ms: 120.0,
            remember_samples: 500,
            ingest_per_sec: 25.0,
            file_bytes: 86_000_000,
            peak_rss_ingest_mib: rss,
            peak_rss_query_mib: rss - 10.0,
            query_vectors: vec![],
            lexical_lift: crate::lexical::LexicalLift {
                hybrid: crate::lexical::LexicalReport {
                    k: 10,
                    queries: 100,
                    recall_at_k: 0.98,
                    latency: crate::lexical::LatencySummary {
                        p50_ms: 1.0,
                        p99_ms: 2.0,
                    },
                },
                vector_only: crate::lexical::LexicalReport {
                    k: 10,
                    queries: 100,
                    recall_at_k: 0.4,
                    latency: crate::lexical::LatencySummary {
                        p50_ms: 0.5,
                        p99_ms: 1.0,
                    },
                },
            },
        }
    }

    #[test]
    fn nfrs_pass_when_under_target() {
        let r = fake_result("agent-mem-100k", 100_000, 12.0, 250.0);
        let checks = check_nfrs(&[r]);
        assert!(checks.iter().all(|c| c.passed && !c.not_applicable));
    }

    #[test]
    fn nfrs_report_miss_not_hide() {
        // p99 over 50 ms and RSS over 300 MiB at 100k → misses, not hidden.
        let r = fake_result("agent-mem-100k", 100_000, 80.0, 400.0);
        let checks = check_nfrs(&[r]);
        let recall_p99 = checks
            .iter()
            .find(|c| c.name.starts_with("recall p99"))
            .unwrap();
        assert!(!recall_p99.passed && !recall_p99.not_applicable);
        let ram = checks
            .iter()
            .find(|c| c.name.starts_with("peak RAM"))
            .unwrap();
        assert!(!ram.passed);
    }

    #[test]
    fn nfrs_are_na_when_100k_absent() {
        // Only a 10k run: the 100k-stated NFRs are n/a, never a silent pass.
        let r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        let checks = check_nfrs(&[r]);
        let recall_p99 = checks
            .iter()
            .find(|c| c.name.starts_with("recall p99"))
            .unwrap();
        assert!(recall_p99.not_applicable && !recall_p99.passed);
        // remember p99 is size-independent → still evaluated on the 10k run.
        let rem = checks
            .iter()
            .find(|c| c.name.starts_with("remember p99"))
            .unwrap();
        assert!(!rem.not_applicable);
    }

    #[test]
    fn markdown_contains_all_sections_and_pins() {
        let env = RunEnv::capture("2026-07-08");
        let r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        let competitors: Vec<_> = COMPETITORS
            .iter()
            .map(|c| {
                (
                    c,
                    CompetitorOutcome::NotMeasured {
                        reason: "feature disabled".into(),
                    },
                )
            })
            .collect();
        let md = render_markdown(&env, &[r], &competitors, None);
        assert!(md.contains("## Benchmark results"));
        assert!(md.contains("Where EmbedMind loses"));
        assert!(md.contains("NFR verdict"));
        // Pinned competitor versions must appear even when not measured.
        assert!(md.contains("sqlite-vec"));
        assert!(md.contains("0.1.10-alpha.4"));
        assert!(md.contains("_not measured_"));
    }

    #[test]
    fn markdown_has_both_labeled_comparison_planes() {
        // S17/BQ3: index-only (pre-computed vectors in, ids out) and
        // text→result (text in, results out, same embedding toll) must both
        // appear, clearly labeled, alongside the existing recall-quality
        // index-only section.
        let env = RunEnv::capture("2026-07-11");
        let r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        let competitors: Vec<_> = COMPETITORS
            .iter()
            .map(|c| {
                (
                    c,
                    CompetitorOutcome::NotMeasured {
                        reason: "feature disabled".into(),
                    },
                )
            })
            .collect();
        let md = render_markdown(&env, &[r], &competitors, None);
        assert!(md.contains("vs. baselines — index-only"));
        assert!(md.contains("vs. baselines — text→result"));
        // index-only precedes text→result (index quality first, then the
        // product workload).
        let idx_pos = md.find("vs. baselines — index-only").unwrap();
        let ttr_pos = md.find("vs. baselines — text→result").unwrap();
        assert!(idx_pos < ttr_pos);
    }

    #[test]
    fn text_to_result_sums_embed_plus_competitor_query() {
        // The text→result plane must add the query-embed cost (measured
        // outside every competitor, via the shared TimingEmbedder split) to
        // each competitor's own index-only query time — never just the bare
        // index-only number, which would silently hide the embedding toll.
        let env = RunEnv::capture("2026-07-11");
        let mut r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        r.query_embed_p50_ms = 3.0;
        r.query_embed_p99_ms = 7.0;
        let competitors: Vec<_> = COMPETITORS
            .iter()
            .map(|c| {
                (
                    c,
                    CompetitorOutcome::Measured(crate::competitors::CompetitorMetrics {
                        recall_at_10: Some(0.98),
                        query_p50_ms: Some(1.0),
                        query_p99_ms: Some(2.0),
                        cold_open_ms: None,
                        ingest_vecs_per_sec: Some(1000.0),
                        file_bytes: Some(1024),
                        peak_rss_mib: None,
                    }),
                )
            })
            .collect();
        let md = render_markdown(&env, std::slice::from_ref(&r), &competitors, None);

        // 3.0 (embed) + 1.0 (competitor query) = 4.0 ms p50;
        // 7.0 (embed) + 2.0 (competitor query) = 9.0 ms p99.
        let ttr_section = &md[md.find("vs. baselines — text→result").unwrap()..];
        assert!(
            ttr_section.contains("4.00 ms"),
            "expected summed p50 in text→result section:\n{ttr_section}"
        );
        assert!(
            ttr_section.contains("9.00 ms"),
            "expected summed p99 in text→result section:\n{ttr_section}"
        );
        // The index-only section must NOT show the summed numbers — it
        // reports the bare competitor query time (its own index-only plane).
        let idx_section = &md[md.find("vs. baselines — index-only").unwrap()
            ..md.find("vs. baselines — text→result").unwrap()];
        assert!(idx_section.contains("1.00 ms"));
        assert!(idx_section.contains("2.00 ms"));
    }

    #[test]
    fn text_to_result_never_fabricates_a_number_for_not_measured() {
        // BENCHMARKS.md §4 rule 1: a competitor whose adapter didn't run must
        // report "not measured" in the text→result plane too, never a
        // fabricated sum.
        let env = RunEnv::capture("2026-07-11");
        let r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        let competitors: Vec<_> = COMPETITORS
            .iter()
            .map(|c| {
                (
                    c,
                    CompetitorOutcome::NotMeasured {
                        reason: "feature disabled".into(),
                    },
                )
            })
            .collect();
        let md = render_markdown(&env, &[r], &competitors, None);
        let ttr_section = &md[md.find("vs. baselines — text→result").unwrap()..];
        assert!(ttr_section.contains("_not measured_"));
        assert!(ttr_section.contains("feature disabled"));
    }

    #[test]
    fn markdown_states_scope_per_system() {
        // BENCHMARKS.md §4 rule 6: every comparison row states what it returns
        // and what it persists — a smaller/faster row is never a silent win.
        let env = RunEnv::capture("2026-07-10");
        let r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        let competitors: Vec<_> = COMPETITORS
            .iter()
            .map(|c| {
                (
                    c,
                    CompetitorOutcome::NotMeasured {
                        reason: "feature disabled".into(),
                    },
                )
            })
            .collect();
        let md = render_markdown(&env, &[r], &competitors, None);
        assert!(
            md.contains("returns"),
            "table header must have a returns column"
        );
        assert!(
            md.contains("persists"),
            "table header must have a persists column"
        );
        // EmbedMind's own scope.
        assert!(md.contains("full content + metadata + provenance"));
        assert!(md.contains("text + metadata + full-text index + vectors"));
        // Every pinned competitor's scope must appear too, even when not measured.
        for c in COMPETITORS {
            assert!(
                md.contains(c.scope.returns),
                "{} scope.returns missing from markdown",
                c.name
            );
            assert!(
                md.contains(c.scope.persists),
                "{} scope.persists missing from markdown",
                c.name
            );
        }
    }

    #[test]
    fn markdown_and_json_carry_the_query_decomposition() {
        // S17: the renderer must emit the embed/engine split, in both outputs.
        let env = RunEnv::capture("2026-07-10");
        let r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        let md = render_markdown(&env, std::slice::from_ref(&r), &[], None);
        assert!(md.contains("query embed p50 / p99"));
        assert!(md.contains("query engine p50 / p99"));
        assert!(md.contains("0.90 / 1.80 ms"), "embed values rendered");
        assert!(md.contains("0.30 / 0.70 ms"), "engine values rendered");

        let js = render_json(&env, &[r], &[], None);
        assert!(js.contains("\"query_embed_p50_ms\": 0.9000"));
        assert!(js.contains("\"query_embed_p99_ms\": 1.8000"));
        assert!(js.contains("\"query_engine_p50_ms\": 0.3000"));
        assert!(js.contains("\"query_engine_p99_ms\": 0.7000"));
        // Still parseable by the regression guard (which ignores the new
        // fields — older baselines without them must keep parsing too).
        let parsed = crate::regression::parse_run_summary(&js).unwrap();
        assert_eq!(parsed.datasets.len(), 1);
    }

    #[test]
    fn markdown_and_json_carry_the_recall_distribution() {
        // S16: the harness must report the per-query recall distribution
        // (min/p10/p50), not just the mean — in both rendered outputs.
        let env = RunEnv::capture("2026-07-10");
        let r = fake_result("agent-mem-100k", 100_000, 40.0, 250.0);
        let md = render_markdown(&env, std::slice::from_ref(&r), &[], None);
        assert!(md.contains("recall@10 min (worst query)"));
        assert!(md.contains("recall@10 p10 / p50 (per query)"));
        assert!(md.contains("0.9500 / 1.0000"), "p10/p50 values rendered");

        let js = render_json(&env, &[r], &[], None);
        assert!(js.contains("\"recall_at_10_min\": 0.900000"));
        assert!(js.contains("\"recall_at_10_p10\": 0.950000"));
        assert!(js.contains("\"recall_at_10_p50\": 1.000000"));
        // The regression guard still parses the extended JSON.
        let parsed = crate::regression::parse_run_summary(&js).unwrap();
        assert_eq!(parsed.datasets.len(), 1);
    }

    #[test]
    fn json_is_wellformed_ish() {
        let env = RunEnv::capture("2026-07-08");
        let r = fake_result("agent-mem-10k", 10_000, 5.0, 100.0);
        let competitors: Vec<_> = COMPETITORS
            .iter()
            .map(|c| (c, CompetitorOutcome::NotMeasured { reason: "x".into() }))
            .collect();
        let js = render_json(&env, &[r], &competitors, None);
        assert_eq!(js.matches('{').count(), js.matches('}').count());
        assert_eq!(js.matches('[').count(), js.matches(']').count());
        assert!(js.contains("\"embedmind_version\""));
        assert!(js.contains("\"compared_on\""));
    }
}
