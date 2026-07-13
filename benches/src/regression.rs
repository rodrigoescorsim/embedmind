//! Baseline comparison for the CI performance-regression guard
//! (docs/BENCHMARKS.md §5, spec S15).
//!
//! Reads two harness results files (`benches/results/<version>.json`, the
//! format emitted by [`crate::report::render_json`]), compares the current run
//! against the baseline, and flags any metric that regressed beyond the §5
//! thresholds. Latency and RSS are machine-dependent, so when the baseline was
//! produced on a different os/arch those checks degrade to **warnings** —
//! recall@10 and on-disk size are deterministic and enforced regardless. The
//! CI job keeps a rolling same-runner baseline (written on `main`) precisely
//! so the latency/RSS checks are normally enforced, with the committed release
//! baseline as the cross-machine fallback.
//!
//! The JSON parser here is deliberately minimal (the harness keeps its dep
//! surface tiny and hand-rolls its JSON output; this is the matching reader) —
//! full JSON grammar, no serde.

use std::fmt::Write as _;

/// The §5 regression thresholds, version-controlled next to the checker like
/// the absolute NFR ceilings in [`crate::report::nfr`]. Deliberately loose
/// (shared-runner noise); the reference machine confirms before a release.
pub mod thresholds {
    /// Max tolerated `recall@10` drop vs baseline, absolute (§5: "> 1 pt").
    pub const RECALL_AT_10_MAX_DROP: f64 = 0.01;
    /// Max tolerated warm query p99 regression (§5: "> 15%").
    pub const QUERY_P99_MAX_REGRESS_PCT: f64 = 15.0;
    /// Max tolerated end-to-end `remember` p99 regression (same class as the
    /// query-latency threshold; spec S15 names both latencies).
    pub const REMEMBER_P99_MAX_REGRESS_PCT: f64 = 15.0;
    /// Max tolerated on-disk file growth (§5: "> 10%").
    pub const FILE_BYTES_MAX_GROWTH_PCT: f64 = 10.0;
    /// Max tolerated peak-RSS growth (§5: "> 15%").
    pub const PEAK_RSS_MAX_GROWTH_PCT: f64 = 15.0;
}

/// The per-dataset numbers the guard compares (subset of the results file).
#[derive(Debug, Clone)]
pub struct DatasetMetrics {
    pub dataset: String,
    pub recall_at_10: f64,
    pub query_p99_ms: f64,
    pub remember_p99_ms: f64,
    pub file_bytes: f64,
    /// Peak of the ingest/query phases — the same reduction `check_nfrs` uses.
    pub peak_rss_mib: f64,
}

/// One parsed results file: enough of the `env` header to decide whether the
/// machine-dependent checks are comparable, plus the per-dataset metrics.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub os: String,
    pub arch: String,
    pub cpus: u64,
    pub version: String,
    pub date_utc: String,
    pub datasets: Vec<DatasetMetrics>,
}

impl RunSummary {
    /// Machine-dependent metrics (latency, RSS) are only *enforced* against a
    /// baseline from the same platform; across platforms they become warnings.
    ///
    /// CPU count is part of that comparability check: GitHub-hosted
    /// `ubuntu-latest` runners are not a fixed shape (observed 2 and 4 vCPUs
    /// across otherwise-identical "same runner" rolling-baseline runs,
    /// 2026-07-11 vs. 2026-07-12), and more CPUs on a shared host means more
    /// scheduling contention, not less — a real, systematic latency shift
    /// that isn't a code regression. Comparing across CPU counts produced two
    /// consecutive false-positive guard failures (remember p99) despite the
    /// retry added for transient fsync stalls.
    pub fn same_env(&self, other: &RunSummary) -> bool {
        self.os == other.os && self.arch == other.arch && self.cpus == other.cpus
    }
}

/// Outcome of one regression check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Within threshold.
    Pass,
    /// Beyond threshold, but the baseline env differs and the metric is
    /// machine-dependent — reported loudly, does not fail the guard.
    Warn,
    /// Beyond threshold on a comparable baseline — fails the guard.
    Fail,
}

/// One compared metric on one dataset.
#[derive(Debug, Clone)]
pub struct RegressionCheck {
    pub dataset: String,
    pub metric: &'static str,
    pub baseline: String,
    pub current: String,
    pub limit: &'static str,
    pub verdict: Verdict,
}

/// Compares every dataset present in **both** runs; errors when the runs share
/// no dataset (a misconfigured guard must not silently pass).
pub fn check_regressions(
    baseline: &RunSummary,
    current: &RunSummary,
) -> Result<Vec<RegressionCheck>, String> {
    let same_env = baseline.same_env(current);
    let mut checks = Vec::new();

    for cur in &current.datasets {
        let Some(base) = baseline.datasets.iter().find(|d| d.dataset == cur.dataset) else {
            continue; // new dataset: no baseline to regress against
        };

        // recall@10: deterministic given the same dataset/index parameters —
        // always enforced, even across machines.
        checks.push(RegressionCheck {
            dataset: cur.dataset.clone(),
            metric: "recall@10",
            baseline: format!("{:.4}", base.recall_at_10),
            current: format!("{:.4}", cur.recall_at_10),
            limit: "drop > 1 pt",
            verdict: if base.recall_at_10 - cur.recall_at_10 > thresholds::RECALL_AT_10_MAX_DROP {
                Verdict::Fail
            } else {
                Verdict::Pass
            },
        });

        // on-disk size: deterministic — always enforced.
        checks.push(pct_check(
            &cur.dataset,
            "file size",
            base.file_bytes,
            cur.file_bytes,
            thresholds::FILE_BYTES_MAX_GROWTH_PCT,
            "growth > 10%",
            true, // enforced regardless of env
        ));

        // Machine-dependent metrics: enforced only on a same-env baseline.
        checks.push(pct_check(
            &cur.dataset,
            "query p99",
            base.query_p99_ms,
            cur.query_p99_ms,
            thresholds::QUERY_P99_MAX_REGRESS_PCT,
            "regress > 15%",
            same_env,
        ));
        checks.push(pct_check(
            &cur.dataset,
            "remember p99",
            base.remember_p99_ms,
            cur.remember_p99_ms,
            thresholds::REMEMBER_P99_MAX_REGRESS_PCT,
            "regress > 15%",
            same_env,
        ));
        checks.push(pct_check(
            &cur.dataset,
            "peak RSS",
            base.peak_rss_mib,
            cur.peak_rss_mib,
            thresholds::PEAK_RSS_MAX_GROWTH_PCT,
            "growth > 15%",
            same_env,
        ));
    }

    if checks.is_empty() {
        return Err(format!(
            "no dataset in common between baseline ({}) and current run ({}) — \
             the guard cannot compare anything",
            names(&baseline.datasets),
            names(&current.datasets),
        ));
    }
    Ok(checks)
}

fn names(ds: &[DatasetMetrics]) -> String {
    let v: Vec<&str> = ds.iter().map(|d| d.dataset.as_str()).collect();
    v.join(", ")
}

/// "grew more than `limit_pct`% over baseline" check. When `enforced` is false
/// (cross-env machine-dependent metric) an over-threshold result is a Warn.
fn pct_check(
    dataset: &str,
    metric: &'static str,
    base: f64,
    cur: f64,
    limit_pct: f64,
    limit: &'static str,
    enforced: bool,
) -> RegressionCheck {
    // A non-positive baseline value can't produce a meaningful percentage;
    // report it as a warning rather than dividing by zero or silently passing.
    let verdict = if base <= 0.0 {
        Verdict::Warn
    } else if (cur - base) / base * 100.0 > limit_pct {
        if enforced {
            Verdict::Fail
        } else {
            Verdict::Warn
        }
    } else {
        Verdict::Pass
    };
    RegressionCheck {
        dataset: dataset.to_string(),
        metric,
        baseline: fmt_metric(metric, base),
        current: fmt_metric(metric, cur),
        limit,
        verdict,
    }
}

fn fmt_metric(metric: &str, v: f64) -> String {
    match metric {
        "file size" => format!("{:.1} MiB", v / (1024.0 * 1024.0)),
        "peak RSS" => format!("{v:.1} MiB"),
        _ => format!("{v:.2} ms"),
    }
}

pub fn has_failures(checks: &[RegressionCheck]) -> bool {
    checks.iter().any(|c| c.verdict == Verdict::Fail)
}

/// Markdown report for the guard: provenance of both runs, whether the env is
/// comparable, and one row per check. Rendered into the CI step summary.
pub fn render_markdown(
    baseline: &RunSummary,
    current: &RunSummary,
    checks: &[RegressionCheck],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "### Regression vs baseline (BENCHMARKS.md §5)\n");
    let _ = writeln!(
        out,
        "_Baseline: EmbedMind {} · {}/{} · {} CPUs · {}._",
        baseline.version, baseline.os, baseline.arch, baseline.cpus, baseline.date_utc
    );
    let _ = writeln!(
        out,
        "_Current:  EmbedMind {} · {}/{} · {} CPUs · {}._\n",
        current.version, current.os, current.arch, current.cpus, current.date_utc
    );
    if !baseline.same_env(current) {
        let _ = writeln!(
            out,
            "> ⚠️ Baseline comes from a different platform or CPU count — latency/RSS\n\
             > checks are reported as warnings, not failures. recall@10 and file size\n\
             > are machine-independent and stay enforced.\n"
        );
    }
    let _ = writeln!(
        out,
        "| Dataset | Metric | Baseline | Current | Limit | Verdict |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---|:---:|");
    for c in checks {
        let verdict = match c.verdict {
            Verdict::Pass => "✅ pass",
            Verdict::Warn => "⚠️ warn (not comparable)",
            Verdict::Fail => "❌ **regression**",
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            c.dataset, c.metric, c.baseline, c.current, c.limit, verdict
        );
    }
    let _ = writeln!(out);
    out
}

// --- results-file parsing (matches `report::render_json` output) ---

/// Parses a harness results file into the comparison summary.
pub fn parse_run_summary(text: &str) -> Result<RunSummary, String> {
    let root = parse_json(text)?;
    let env = root
        .get("env")
        .ok_or_else(|| "missing \"env\" object".to_string())?;
    let datasets_json = root
        .get("datasets")
        .and_then(Json::as_arr)
        .ok_or_else(|| "missing \"datasets\" array".to_string())?;

    let mut datasets = Vec::new();
    for d in datasets_json {
        datasets.push(DatasetMetrics {
            dataset: str_field(d, "dataset")?,
            recall_at_10: num_field(d, "recall_at_10")?,
            query_p99_ms: num_field(d, "query_p99_ms")?,
            remember_p99_ms: num_field(d, "remember_p99_ms")?,
            file_bytes: num_field(d, "file_bytes")?,
            peak_rss_mib: num_field(d, "peak_rss_ingest_mib")?
                .max(num_field(d, "peak_rss_query_mib")?),
        });
    }

    Ok(RunSummary {
        os: str_field(env, "os")?,
        arch: str_field(env, "arch")?,
        cpus: num_field(env, "cpus")? as u64,
        version: str_field(env, "embedmind_version")?,
        date_utc: str_field(env, "date_utc")?,
        datasets,
    })
}

fn str_field(v: &Json, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(Json::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string field \"{key}\""))
}

fn num_field(v: &Json, key: &str) -> Result<f64, String> {
    v.get(key)
        .and_then(Json::as_num)
        .ok_or_else(|| format!("missing numeric field \"{key}\""))
}

/// Minimal JSON value tree — just what the guard needs to read the results
/// files the harness itself writes.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }
}

/// Parses a full JSON document (trailing garbage is an error).
pub fn parse_json(text: &str) -> Result<Json, String> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing characters after top-level value"));
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> String {
        format!("json parse error at byte {}: {msg}", self.pos)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", b as char)))
        }
    }

    fn eat_lit(&mut self, lit: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(lit.as_bytes()) {
            self.pos += lit.len();
            Ok(value)
        } else {
            Err(self.err(&format!("expected literal '{lit}'")))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.eat_lit("true", Json::Bool(true)),
            Some(b'f') => self.eat_lit("false", Json::Bool(false)),
            Some(b'n') => self.eat_lit("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("expected a JSON value")),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.eat(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            let val = self.value()?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(pairs));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.pos += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let Some(esc) = self.peek() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => buf.push(b'"'),
                        b'\\' => buf.push(b'\\'),
                        b'/' => buf.push(b'/'),
                        b'n' => buf.push(b'\n'),
                        b'r' => buf.push(b'\r'),
                        b't' => buf.push(b'\t'),
                        b'b' => buf.push(0x08),
                        b'f' => buf.push(0x0c),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.pos..self.pos + 4)
                                .ok_or_else(|| self.err("truncated \\u escape"))?;
                            let hex = std::str::from_utf8(hex)
                                .map_err(|_| self.err("non-utf8 in \\u escape"))?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| self.err("bad hex in \\u escape"))?;
                            self.pos += 4;
                            // BMP only — the harness never emits surrogate
                            // pairs (it only \u-escapes control characters).
                            let ch = char::from_u32(code)
                                .ok_or_else(|| self.err("invalid \\u code point"))?;
                            let mut tmp = [0u8; 4];
                            buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                        }
                        _ => return Err(self.err("unknown escape")),
                    }
                }
                _ => buf.push(b),
            }
        }
        String::from_utf8(buf).map_err(|_| self.err("string is not valid utf-8"))
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("non-utf8 number"))?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| self.err("invalid number"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn summary(os: &str, datasets: Vec<DatasetMetrics>) -> RunSummary {
        RunSummary {
            os: os.into(),
            arch: "x86_64".into(),
            cpus: 4,
            version: "0.1.0-dev".into(),
            date_utc: "2026-07-09".into(),
            datasets,
        }
    }

    fn metrics(name: &str) -> DatasetMetrics {
        DatasetMetrics {
            dataset: name.into(),
            recall_at_10: 0.995,
            query_p99_ms: 14.0,
            remember_p99_ms: 22.0,
            file_bytes: 86_000_000.0,
            peak_rss_mib: 118.0,
        }
    }

    #[test]
    fn identical_runs_pass_everything() {
        let base = summary("linux", vec![metrics("agent-mem-10k")]);
        let cur = base.clone();
        let checks = check_regressions(&base, &cur).unwrap();
        assert_eq!(checks.len(), 5);
        assert!(checks.iter().all(|c| c.verdict == Verdict::Pass));
        assert!(!has_failures(&checks));
    }

    #[test]
    fn recall_drop_beyond_1pt_fails_even_cross_env() {
        let base = summary("windows", vec![metrics("agent-mem-10k")]);
        let mut m = metrics("agent-mem-10k");
        m.recall_at_10 = 0.98; // 1.5 pt drop
        let cur = summary("linux", vec![m]);
        let checks = check_regressions(&base, &cur).unwrap();
        let recall = checks.iter().find(|c| c.metric == "recall@10").unwrap();
        assert_eq!(recall.verdict, Verdict::Fail);
        assert!(has_failures(&checks));
    }

    #[test]
    fn file_growth_beyond_10pct_fails_even_cross_env() {
        let base = summary("windows", vec![metrics("agent-mem-10k")]);
        let mut m = metrics("agent-mem-10k");
        m.file_bytes *= 1.2;
        let cur = summary("linux", vec![m]);
        let checks = check_regressions(&base, &cur).unwrap();
        let size = checks.iter().find(|c| c.metric == "file size").unwrap();
        assert_eq!(size.verdict, Verdict::Fail);
    }

    #[test]
    fn latency_regression_fails_same_env_but_warns_cross_env() {
        let mut m = metrics("agent-mem-10k");
        m.query_p99_ms *= 1.3; // +30%, over the 15% threshold
        let base = summary("linux", vec![metrics("agent-mem-10k")]);

        let cur_same = summary("linux", vec![m.clone()]);
        let checks = check_regressions(&base, &cur_same).unwrap();
        let q = checks.iter().find(|c| c.metric == "query p99").unwrap();
        assert_eq!(q.verdict, Verdict::Fail);

        let cur_cross = summary("windows", vec![m]);
        let checks = check_regressions(&base, &cur_cross).unwrap();
        let q = checks.iter().find(|c| c.metric == "query p99").unwrap();
        assert_eq!(q.verdict, Verdict::Warn);
        assert!(!has_failures(&checks));
    }

    #[test]
    fn differing_cpu_count_is_not_same_env_even_on_same_os_arch() {
        let mut base = summary("linux", vec![metrics("agent-mem-10k")]);
        base.cpus = 2;
        let mut m = metrics("agent-mem-10k");
        m.remember_p99_ms *= 4.0; // steep regression that would fail if enforced
        let mut cur = summary("linux", vec![m]);
        cur.cpus = 4;

        assert!(!base.same_env(&cur));
        let checks = check_regressions(&base, &cur).unwrap();
        let remember = checks.iter().find(|c| c.metric == "remember p99").unwrap();
        assert_eq!(remember.verdict, Verdict::Warn);
        assert!(!has_failures(&checks));
        // recall@10/file size stay enforced regardless of CPU count.
        assert!(
            checks
                .iter()
                .filter(|c| c.metric == "recall@10" || c.metric == "file size")
                .all(|c| c.verdict != Verdict::Warn)
        );
    }

    #[test]
    fn rss_and_remember_within_threshold_pass() {
        let mut m = metrics("agent-mem-10k");
        m.peak_rss_mib *= 1.10; // +10% < 15%
        m.remember_p99_ms *= 1.10;
        let base = summary("linux", vec![metrics("agent-mem-10k")]);
        let cur = summary("linux", vec![m]);
        let checks = check_regressions(&base, &cur).unwrap();
        assert!(checks.iter().all(|c| c.verdict == Verdict::Pass));
    }

    #[test]
    fn no_common_dataset_is_an_error() {
        let base = summary("linux", vec![metrics("agent-mem-100k")]);
        let cur = summary("linux", vec![metrics("agent-mem-10k")]);
        assert!(check_regressions(&base, &cur).is_err());
    }

    #[test]
    fn parses_the_harness_render_json_output() {
        use crate::competitors::CompetitorOutcome;
        use crate::harness::SuiteResult;
        use crate::recall::RecallReport;
        use crate::report::{RunEnv, render_json};

        let r = SuiteResult {
            dataset: "agent-mem-10k",
            count: 10_000,
            dims: 384,
            model_id: "all-MiniLM-L6-v2-int8".into(),
            recall: RecallReport {
                k: 10,
                queries: 200,
                recall_at_k: 0.9953,
                min_recall: 0.9,
                p10_recall: 0.95,
                p50_recall: 1.0,
            },
            query_p50_ms: 10.4,
            query_p99_ms: 14.27,
            query_mean_ms: 10.9,
            warm_queries: 200,
            query_embed_p50_ms: 8.1,
            query_embed_p99_ms: 11.0,
            query_engine_p50_ms: 2.3,
            query_engine_p99_ms: 3.4,
            query_vector_p50_ms: 6.1,
            query_vector_p99_ms: 8.3,
            cold_open_ms: 0.37,
            cold_first_query_ms: 10.56,
            remember_p50_ms: 7.49,
            remember_p99_ms: 22.28,
            remember_samples: 500,
            ingest_per_sec: 67.5,
            file_bytes: 85_991_424,
            peak_rss_ingest_mib: 118.57,
            peak_rss_query_mib: 117.79,
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
        };
        let competitors: Vec<(&'static crate::competitors::Competitor, CompetitorOutcome)> =
            crate::competitors::COMPETITORS
                .iter()
                .map(|c| (c, CompetitorOutcome::NotMeasured { reason: "x".into() }))
                .collect();
        let env = RunEnv::capture("2026-07-09");
        let json = render_json(&env, &[r], &competitors, None);

        let parsed = parse_run_summary(&json).unwrap();
        assert_eq!(parsed.os, std::env::consts::OS);
        assert_eq!(parsed.datasets.len(), 1);
        let d = &parsed.datasets[0];
        assert_eq!(d.dataset, "agent-mem-10k");
        assert!((d.recall_at_10 - 0.9953).abs() < 1e-4);
        assert!((d.query_p99_ms - 14.27).abs() < 1e-3);
        assert!((d.file_bytes - 85_991_424.0).abs() < 0.5);
        assert!((d.peak_rss_mib - 118.57).abs() < 1e-2);

        // Identical file compared to itself: guard passes.
        let checks = check_regressions(&parsed, &parsed).unwrap();
        assert!(!has_failures(&checks));
        let md = render_markdown(&parsed, &parsed, &checks);
        assert!(md.contains("Regression vs baseline"));
        assert!(md.contains("✅ pass"));
    }

    #[test]
    fn json_parser_handles_escapes_and_nesting() {
        let v = parse_json(r#"{"a": [1, -2.5e1, "x\n\"yA", true, null], "b": {}}"#).unwrap();
        let arr = v.get("a").and_then(Json::as_arr).unwrap();
        assert_eq!(arr[0].as_num(), Some(1.0));
        assert_eq!(arr[1].as_num(), Some(-25.0));
        assert_eq!(arr[2].as_str(), Some("x\n\"yA"));
        assert_eq!(arr[3], Json::Bool(true));
        assert_eq!(arr[4], Json::Null);
        assert!(parse_json("{\"a\": }").is_err());
        assert!(parse_json("[1,]").is_err());
        assert!(parse_json("{} junk").is_err());
    }
}
