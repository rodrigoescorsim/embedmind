//! Peak resident-set-size (RSS) sampling for the benchmark harness
//! (`docs/BENCHMARKS.md` §3: "peak RSS during ingest and during query load").
//!
//! RSS is not a value you can read once at the end — it must be *sampled* while
//! the phase runs, because it rises and falls (the ONNX session, the pager
//! cache, transient buffers). [`RssSampler`] wraps `sysinfo` to poll the
//! current process's memory and keep the maximum seen, so a caller can bracket
//! a phase and ask for its peak afterwards.
//!
//! No `unsafe`: `sysinfo` is pure-Rust process introspection, so this crate
//! keeps the workspace's `unsafe_code = forbid` even though it queries the OS.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Samples this process's RSS on demand and remembers the peak. Construct once,
/// call [`RssSampler::sample`] repeatedly across a measured phase, then read
/// [`RssSampler::peak_bytes`].
pub struct RssSampler {
    system: System,
    pid: Pid,
    peak_bytes: u64,
}

impl RssSampler {
    /// New sampler bound to the current process. Takes an initial sample so the
    /// peak is never zero even if a phase is instantaneous.
    pub fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let mut s = Self {
            system: System::new(),
            pid,
            peak_bytes: 0,
        };
        s.sample();
        s
    }

    /// Refreshes the current process's memory reading and folds it into the
    /// running peak. Cheap enough to call in a loop between operations.
    pub fn sample(&mut self) -> u64 {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            ProcessRefreshKind::new().with_memory(),
        );
        let current = self
            .system
            .process(self.pid)
            .map(|p| p.memory())
            .unwrap_or(0);
        if current > self.peak_bytes {
            self.peak_bytes = current;
        }
        current
    }

    /// The highest RSS observed across all samples, in bytes.
    pub fn peak_bytes(&self) -> u64 {
        self.peak_bytes
    }

    /// The highest RSS observed, in mebibytes (what the NFR is stated in:
    /// "RAM < 300 MB @ 100k").
    pub fn peak_mib(&self) -> f64 {
        self.peak_bytes as f64 / (1024.0 * 1024.0)
    }
}

impl Default for RssSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn peak_is_positive_and_monotonic() {
        let mut s = RssSampler::new();
        let first = s.peak_bytes();
        // Allocate something the sampler should see, then sample again.
        let _big: Vec<u8> = vec![7u8; 32 * 1024 * 1024];
        s.sample();
        assert!(s.peak_bytes() >= first, "peak must never decrease");
        assert!(s.peak_mib() > 0.0, "a running process has nonzero RSS");
    }
}
