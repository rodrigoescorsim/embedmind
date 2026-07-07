//! `embedmind` CLI — thin shell over `embedmind_core::api` (no domain logic
//! here). Subcommand surface matches the README quickstart; bodies land with
//! M1 items 1.2–1.6.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "embedmind",
    version,
    about = "Persistent memory for AI agents — one local file, no server"
)]
struct Cli {
    /// Path to the memory file (default: ~/.embedmind/memory.mind)
    #[arg(long, global = true)]
    file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run as an MCP server over stdio (what agents connect to)
    Serve,
    /// Store a memory
    Remember { content: String },
    /// Hybrid search over everything remembered
    Recall {
        query: String,
        /// Maximum results
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Delete memories by id
    Forget { id: String },
    /// Show file size, counts and index health
    Stats,
    /// Reclaim space from forgotten memories and rebuild indexes
    Vacuum,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let name = match cli.command {
        Command::Serve => "serve",
        Command::Remember { .. } => "remember",
        Command::Recall { .. } => "recall",
        Command::Forget { .. } => "forget",
        Command::Stats => "stats",
        Command::Vacuum => "vacuum",
    };
    eprintln!("embedmind {name}: not implemented yet (pre-v0.1 skeleton — see ROADMAP.md M1)");
    ExitCode::FAILURE
}
