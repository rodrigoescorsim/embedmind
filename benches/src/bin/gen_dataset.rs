//! Materializes a committed benchmark dataset (`docs/BENCHMARKS.md` §2) from
//! its seed: generates the deterministic text corpus, embeds it with the
//! shipped ONNX model, and writes both the `.mind` store and the `.vec`
//! baseline sidecar into `benches/data/` (git-ignored).
//!
//! ```text
//! cargo run -p embedmind-bench --bin gen_dataset -- agent-mem-10k
//! cargo run -p embedmind-bench --release --bin gen_dataset -- agent-mem-100k
//! ```
//!
//! With no argument it lists the available datasets. The 100k set is large —
//! embedding it is minutes of CPU — so `--release` is strongly advised.

// The harness binaries surface fatal setup errors by returning `Err` from
// `main`; the workspace's `expect`/`unwrap`/`panic` denials still apply to
// everything else.
#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::process::ExitCode;

use embedmind_bench::dataset::{DATASETS, DatasetSpec};
use embedmind_bench::default_data_dir;

fn main() -> ExitCode {
    let Some(name) = std::env::args().nth(1) else {
        eprintln!("usage: gen_dataset <dataset>");
        eprintln!("available datasets:");
        for d in DATASETS {
            eprintln!("  {} ({} memories, seed {:#018x})", d.name, d.count, d.seed);
        }
        return ExitCode::FAILURE;
    };

    let Some(spec) = DatasetSpec::by_name(&name) else {
        eprintln!("unknown dataset '{name}' — run with no argument to list them");
        return ExitCode::FAILURE;
    };

    let data_dir = default_data_dir();
    println!(
        "materializing {} ({} memories, seed {:#018x}) into {}",
        spec.name,
        spec.count,
        spec.seed,
        data_dir.display()
    );
    let started = std::time::Instant::now();
    match embedmind_bench::dataset::materialize(spec, &data_dir) {
        Ok(set) => {
            println!(
                "done: {} vectors, {} dims, in {:.1}s",
                set.entries.len(),
                set.dims,
                started.elapsed().as_secs_f64()
            );
            println!("  store:   {}", spec.mind_path(&data_dir).display());
            println!("  vectors: {}", spec.vec_path(&data_dir).display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("materialization failed: {e}");
            ExitCode::FAILURE
        }
    }
}
