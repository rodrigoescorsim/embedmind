//! CI regression guard, comparison-only entry point (docs/BENCHMARKS.md §5,
//! spec S15).
//!
//! ```text
//! compare_baseline <baseline.json> <current.json>
//! ```
//!
//! Both files are harness results (`benches/results/<version>.json`). Prints
//! the regression report as markdown (the CI job appends it to the step
//! summary) and exits non-zero when any metric regressed beyond the §5
//! thresholds on a comparable baseline. `run_all` embeds the same comparison
//! via the `BASELINE` env var; this binary exists to compare two existing
//! results files without re-running the suite.

#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::process::ExitCode;

use embedmind_bench::regression;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("compare_baseline failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [baseline_path, current_path] = args.as_slice() else {
        eprintln!("usage: compare_baseline <baseline.json> <current.json>");
        return Ok(ExitCode::FAILURE);
    };

    let baseline_text = std::fs::read_to_string(baseline_path)
        .map_err(|e| format!("reading baseline '{baseline_path}': {e}"))?;
    let current_text = std::fs::read_to_string(current_path)
        .map_err(|e| format!("reading current '{current_path}': {e}"))?;

    let baseline = regression::parse_run_summary(&baseline_text)
        .map_err(|e| format!("parsing baseline '{baseline_path}': {e}"))?;
    let current = regression::parse_run_summary(&current_text)
        .map_err(|e| format!("parsing current '{current_path}': {e}"))?;

    let checks = regression::check_regressions(&baseline, &current)?;
    print!(
        "{}",
        regression::render_markdown(&baseline, &current, &checks)
    );

    if regression::has_failures(&checks) {
        eprintln!("performance regression vs baseline (BENCHMARKS.md §5) — see report above");
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
