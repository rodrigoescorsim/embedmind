//! `embedmind-mcp` binary: opens (or creates) the memory file and serves MCP
//! over stdio (`docs/adr/0009`). Logs go to stderr — stdout is the protocol
//! channel and carries nothing else.
//!
//! Usage: `embedmind-mcp [--file <path>] [--op-log <path>]`. Default file:
//! `~/.embedmind/memory.mind` (README quickstart). `--op-log` (S22) appends
//! one JSON line per tool call to the given file — see `embedmind_mcp::oplog`.

use std::path::PathBuf;
use std::process::ExitCode;

use embedmind_core::Store;
use embedmind_mcp::{McpServer, OpLog, detect_project};

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("embedmind-mcp: {message}");
            eprintln!("usage: embedmind-mcp [--file <path>] [--op-log <path>]");
            return ExitCode::FAILURE;
        }
    };
    let file = match args.file.map_or_else(default_memory_file, Ok) {
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

    let mut server = McpServer::new(store, project);
    // Op-log (S22): opt-in observability. Failing to OPEN the requested log
    // is a startup error (the operator would silently lose what they asked
    // for); write failures later are warnings only (`oplog` module).
    if let Some(path) = args.op_log {
        match OpLog::create(&path) {
            Ok(op_log) => {
                eprintln!("embedmind-mcp: op-log appending to {}", path.display());
                server = server.with_op_log(op_log);
            }
            Err(e) => {
                eprintln!("embedmind-mcp: cannot open op-log {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match server.serve(stdin.lock(), stdout.lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("embedmind-mcp: transport error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The two optional flags of the server binary.
struct Args {
    file: Option<PathBuf>,
    op_log: Option<PathBuf>,
}

/// Parses `[--file <path>] [--op-log <path>]`. Kept by hand: two flags do
/// not justify a clap dependency in the server binary.
fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args_os().skip(1);
    let mut file = None;
    let mut op_log = None;
    while let Some(arg) = args.next() {
        if arg == "--file" {
            let value = args.next().ok_or("--file requires a path")?;
            file = Some(PathBuf::from(value));
        } else if arg == "--op-log" {
            let value = args.next().ok_or("--op-log requires a path")?;
            op_log = Some(PathBuf::from(value));
        } else {
            return Err(format!("unknown argument: {}", arg.to_string_lossy()));
        }
    }
    Ok(Args { file, op_log })
}

/// `~/.embedmind/memory.mind`, cross-platform (`USERPROFILE` on Windows,
/// `HOME` elsewhere).
fn default_memory_file() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("cannot determine home directory (HOME/USERPROFILE unset); pass --file")?;
    Ok(PathBuf::from(home).join(".embedmind").join("memory.mind"))
}
