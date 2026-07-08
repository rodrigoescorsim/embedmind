//! Latency and throughput measurement primitives (`docs/BENCHMARKS.md` §3).
//!
//! A [`Latencies`] collects per-operation durations and reports p50/p99 the way
//! the methodology asks for them: **single-thread, over a fixed query count**.
//! Percentiles use the nearest-rank method on the sorted samples — no
//! interpolation, so a reported p99 is always a value that actually occurred
//! (the honest choice for a small, fixed sample: it never invents a latency
//! between two measured ones).
//!
//! Everything here is measurement-only; it holds no product logic.

use std::time::Duration;

/// A collection of single-operation latencies, from which percentiles are
/// drawn. Cheap to fill (one `push` per op) and to summarize.
#[derive(Debug, Default, Clone)]
pub struct Latencies {
    samples: Vec<Duration>,
}

impl Latencies {
    /// Empty collector, pre-sized for `capacity` samples.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
        }
    }

    /// Records one measured operation.
    pub fn push(&mut self, d: Duration) {
        self.samples.push(d);
    }

    /// How many operations were recorded.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Nearest-rank percentile in `[0, 100]`, in milliseconds. `p50` is the
    /// median, `p99` the 99th percentile. Returns `None` for an empty set.
    ///
    /// Nearest-rank (rather than linear interpolation) is deliberate: the
    /// reported number is always a latency that was actually observed, which is
    /// the honest thing to publish for a fixed, modest sample.
    pub fn percentile_ms(&self, p: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.samples.clone();
        sorted.sort_unstable();
        // Nearest-rank: rank = ceil(p/100 * N), clamped to [1, N].
        let n = sorted.len();
        let rank = ((p / 100.0) * n as f64).ceil().max(1.0) as usize;
        let idx = rank.min(n) - 1;
        Some(sorted[idx].as_secs_f64() * 1000.0)
    }

    /// Convenience: p50 in milliseconds.
    pub fn p50_ms(&self) -> Option<f64> {
        self.percentile_ms(50.0)
    }

    /// Convenience: p99 in milliseconds.
    pub fn p99_ms(&self) -> Option<f64> {
        self.percentile_ms(99.0)
    }

    /// Arithmetic mean in milliseconds — context for the percentiles, not a
    /// headline number.
    pub fn mean_ms(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let total: f64 = self.samples.iter().map(|d| d.as_secs_f64()).sum();
        Some(total / self.samples.len() as f64 * 1000.0)
    }
}

/// Throughput of `count` operations that together took `elapsed`, in
/// operations per second. `0.0` when no time elapsed (avoids a divide-by-zero
/// producing infinity in the report).
pub fn ops_per_sec(count: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        0.0
    } else {
        count as f64 / secs
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn nearest_rank_percentiles() {
        let mut l = Latencies::with_capacity(100);
        for i in 1..=100u64 {
            l.push(ms(i));
        }
        // Nearest-rank p50 of 1..=100 is the 50th value = 50 ms.
        assert_eq!(l.p50_ms().unwrap().round() as u64, 50);
        // p99 is the 99th value = 99 ms.
        assert_eq!(l.p99_ms().unwrap().round() as u64, 99);
    }

    #[test]
    fn percentile_is_always_an_observed_value() {
        // Two very different samples: nearest-rank never invents a value in
        // between (unlike interpolation).
        let mut l = Latencies::with_capacity(2);
        l.push(ms(10));
        l.push(ms(1000));
        let p50 = l.p50_ms().unwrap();
        assert!(p50 == 10.0 || p50 == 1000.0, "p50 was {p50}, not observed");
    }

    #[test]
    fn empty_reports_none() {
        let l = Latencies::default();
        assert!(l.p50_ms().is_none());
        assert!(l.p99_ms().is_none());
        assert!(l.mean_ms().is_none());
        assert!(l.is_empty());
    }

    #[test]
    fn throughput_math() {
        assert_eq!(ops_per_sec(1000, Duration::from_secs(2)), 500.0);
        assert_eq!(ops_per_sec(10, Duration::ZERO), 0.0);
    }
}
