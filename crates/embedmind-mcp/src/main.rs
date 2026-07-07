//! `embedmind-mcp` binary: opens (or creates) the memory file and serves MCP
//! over stdio (`docs/adr/0009`). Logs go to stderr — stdout is the protocol
//! channel and carries nothing else.
//!
//! Usage: `embedmind-mcp [--file <path>]`. Default file:
//! `~/.embedmind/memory.mind` (README quickstart).

use std::path::PathBuf;
use std::process::ExitCode;

use embedmind_core::Store;
use embedmind_mcp::{McpServer, detect_project};

fn main() -> ExitCode {
    let file = match parse_args() {
        Ok(file) => file,
        Err(message) => {
            eprintln!("embedmind-mcp: {message}");
            eprintln!("usage: embedmind-mcp [--file <path>]");
            return ExitCode::FAILURE;
        }
    };
    let file = match file.map_or_else(default_memory_file, Ok) {
        Ok(file) => file,
        Err(message) => {
            eprintln!("embedmind-mcp: {message}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(parent) = file.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("embedmind-mcp: cannot create {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }

    let store = match Store::open_or_create(&file) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("embedmind-mcp: cannot open {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };

    // Project context (M1 item 1.5): MCP hosts spawn the server with the
    // agent's workspace as cwd, so that is the signal to detect from.
    let project = std::env::current_dir()
        .ok()
        .and_then(|cwd| detect_project(&cwd));
    match &project {
        Some(name) => eprintln!(
            "embedmind-mcp: serving memories from {} (project: {name})",
            file.display()
        ),
        None => eprintln!(
            "embedmind-mcp: serving memories from {} (no project context)",
            file.display()
        ),
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut server = McpServer::new(store, project);
    match server.serve(stdin.lock(), stdout.lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("embedmind-mcp: transport error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parses `[--file <path>]`. Kept by hand: one flag does not justify a clap
/// dependency in the server binary.
fn parse_args() -> Result<Option<PathBuf>, String> {
    let mut args = std::env::args_os().skip(1);
    let mut file = None;
    while let Some(arg) = args.next() {
        if arg == "--file" {
            let value = args.next().ok_or("--file requires a path")?;
            file = Some(PathBuf::from(value));
        } else {
            return Err(format!("unknown argument: {}", arg.to_string_lossy()));
        }
    }
    Ok(file)
}

/// `~/.embedmind/memory.mind`, cross-platform (`USERPROFILE` on Windows,
/// `HOME` elsewhere).
fn default_memory_file() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("cannot determine home directory (HOME/USERPROFILE unset); pass --file")?;
    Ok(PathBuf::from(home).join(".embedmind").join("memory.mind"))
}
