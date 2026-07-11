//! Reader/aggregator for the op-log JSONL (S23): turns the raw trace written
//! by `serve --op-log` into the numbers behind `embedmind report` — the
//! user-facing answer to "is the memory actually being used?". Lives next to
//! the writer ([`crate::oplog`]) because the line format is this crate's
//! contract; the CLI only joins the result with the store (previews, dead
//! weight) and prints.
//!
//! Robustness contract, mirroring the writer's: every line is independent —
//! a line that does not parse (partial write, corruption) is counted in
//! [`UsageReport::skipped_lines`] and skipped, never an error. Lines whose
//! `ts` falls before the window are ignored.

use std::collections::BTreeMap;
use std::io::BufRead;

use serde_json::Value;

/// Aggregated usage over one op-log window. All counts are tool *calls*
/// except [`UsageReport::served`], which counts each memory id every time a
/// successful `recall` returned it — the per-memory usage counter.
#[derive(Debug, Default, PartialEq)]
pub struct UsageReport {
    /// `initialize` handshakes (one per agent session connected).
    pub sessions: u64,
    /// `recall` calls, including failed ones.
    pub recalls: u64,
    /// `recall` calls that returned an error.
    pub recall_errors: u64,
    /// Successful `recall` calls that served zero memories.
    pub recalls_empty: u64,
    /// `remember` calls, including failed ones.
    pub remembers: u64,
    /// `remember` calls that returned an error.
    pub remember_errors: u64,
    /// Successful `forget` calls.
    pub forgets: u64,
    /// `related` calls (graph navigation — kept apart from recalls).
    pub related_calls: u64,
    /// Median latency of successful recalls, milliseconds.
    pub recall_latency_p50_ms: Option<f64>,
    /// p99 latency of successful recalls, milliseconds.
    pub recall_latency_p99_ms: Option<f64>,
    /// memory id → times served by a successful `recall` in the window.
    pub served: BTreeMap<String, u64>,
    /// Lines that did not parse as JSON (partial writes, corruption).
    pub skipped_lines: u64,
}

/// Aggregates every op-log line with `ts >= since_micros`. IO errors from
/// the reader end the scan (whatever was read still counts) — the log is
/// observability, a torn tail must not fail the report.
pub fn aggregate(reader: impl BufRead, since_micros: u64) -> UsageReport {
    let mut out = UsageReport::default();
    let mut latencies: Vec<f64> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            out.skipped_lines += 1;
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            out.skipped_lines += 1;
            continue;
        };
        if entry.get("ts").and_then(Value::as_u64).unwrap_or(0) < since_micros {
            continue;
        }
        let is_error = entry
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match entry.get("tool").and_then(Value::as_str).unwrap_or("") {
            "session" => out.sessions += 1,
            "recall" => {
                out.recalls += 1;
                if is_error {
                    out.recall_errors += 1;
                    continue;
                }
                if let Some(ms) = entry.get("latency_ms").and_then(Value::as_f64) {
                    latencies.push(ms);
                }
                let ids: Vec<&str> = entry
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if ids.is_empty() {
                    out.recalls_empty += 1;
                }
                for id in ids {
                    *out.served.entry(id.to_string()).or_insert(0) += 1;
                }
            }
            "remember" => {
                out.remembers += 1;
                if is_error {
                    out.remember_errors += 1;
                }
            }
            "forget" => {
                if !is_error {
                    out.forgets += 1;
                }
            }
            "related" => out.related_calls += 1,
            _ => {}
        }
    }
    latencies.sort_by(|a, b| a.total_cmp(b));
    out.recall_latency_p50_ms = percentile(&latencies, 50);
    out.recall_latency_p99_ms = percentile(&latencies, 99);
    out
}

/// Nearest-rank percentile (rank = ⌈p/100 · N⌉) over an already-sorted
/// slice; `None` when empty. The ceiling matters on small samples: p99 of
/// 3 recalls is the slowest one, not the median.
fn percentile(sorted: &[f64], p: usize) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * p).div_ceil(100);
    Some(sorted[rank.saturating_sub(1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: u64, tool: &str, ids: &[&str], latency: f64, is_error: bool) -> String {
        serde_json::json!({
            "ts": ts, "tool": tool, "args": {}, "ids": ids, "scores": [],
            "latency_ms": latency, "project": "p", "isError": is_error,
        })
        .to_string()
    }

    #[test]
    fn aggregates_sessions_recalls_and_per_memory_counters() {
        let log = [
            line(100, "session", &[], 0.0, false),
            line(110, "recall", &["A", "B"], 10.0, false),
            line(120, "recall", &["A"], 20.0, false),
            line(130, "recall", &[], 5.0, false), // served nothing
            line(140, "remember", &["C"], 8.0, false),
            line(150, "forget", &["B"], 1.0, false),
            line(160, "related", &["A"], 2.0, false),
        ]
        .join("\n");
        let report = aggregate(log.as_bytes(), 0);
        assert_eq!(report.sessions, 1);
        assert_eq!(report.recalls, 3);
        assert_eq!(report.recalls_empty, 1);
        assert_eq!(report.remembers, 1);
        assert_eq!(report.forgets, 1);
        assert_eq!(report.related_calls, 1);
        assert_eq!(report.served.get("A"), Some(&2));
        assert_eq!(report.served.get("B"), Some(&1));
        // `related` ids are navigation, never usage counters.
        assert_eq!(report.served.len(), 2);
        assert_eq!(report.recall_latency_p50_ms, Some(10.0));
        assert_eq!(report.recall_latency_p99_ms, Some(20.0));
    }

    #[test]
    fn window_filters_and_garbage_is_skipped_not_fatal() {
        let log = [
            line(100, "recall", &["OLD"], 1.0, false),
            "{not json".to_string(),
            String::new(),
            line(200, "recall", &["NEW"], 1.0, false),
            line(210, "recall", &[], 1.0, true), // error: counted, not served
        ]
        .join("\n");
        let report = aggregate(log.as_bytes(), 150);
        assert_eq!(report.recalls, 2);
        assert_eq!(report.recall_errors, 1);
        assert_eq!(report.recalls_empty, 0, "error recall is not 'empty'");
        assert!(!report.served.contains_key("OLD"), "outside the window");
        assert_eq!(report.served.get("NEW"), Some(&1));
        assert_eq!(report.skipped_lines, 1);
    }
}
