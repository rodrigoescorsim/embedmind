//! `embedmind` CLI — thin shell over `embedmind_core::api` (no domain logic
//! here, CLAUDE.md decision 2). Subcommand surface matches the README
//! quickstart; `serve` runs the same MCP server as the `embedmind-mcp`
//! binary, so one installed command covers standalone use *and* the agent
//! integration (M1 item 1.6).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use std::collections::BTreeMap;

use embedmind_core::{Filter, MemoryDraft, Query, Scalar, Store, Ulid};
use embedmind_mcp::{McpServer, detect_project};

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
    Remember {
        content: String,
        /// Project scope (default: detected from the current directory;
        /// use --global to store without a project)
        #[arg(long, conflicts_with = "global")]
        project: Option<String>,
        /// Store as a global memory even inside a project
        #[arg(long)]
        global: bool,
    },
    /// Semantic search over everything remembered
    Recall {
        query: String,
        /// Maximum results
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Search one specific project (default: detected from the current
        /// directory)
        #[arg(long, conflicts_with = "all")]
        project: Option<String>,
        /// Search every project (explicit global fallback)
        #[arg(long)]
        all: bool,
        /// Metadata filter (repeatable, all ANDed). Forms: `key=value` for an
        /// exact match, `key>=n` / `key<=n` for an open numeric bound, or
        /// `key=lo..hi` for a closed numeric range. Repeat `>=`/`<=` on the
        /// same key to bound both ends.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        filters: Vec<String>,
    },
    /// Delete one memory by id
    Forget { id: String },
    /// Show file size, counts and index health
    Stats,
    /// Reclaim space from forgotten memories and rebuild indexes
    Vacuum,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("embedmind: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let file = match cli.file {
        Some(file) => file,
        None => default_memory_file()?,
    };
    if let Some(parent) = file.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    match cli.command {
        Command::Serve => serve(&file),
        Command::Remember {
            content,
            project,
            global,
        } => remember(&file, content, project, global),
        Command::Recall {
            query,
            limit,
            project,
            all,
            filters,
        } => recall(&file, query, limit, project, all, filters),
        Command::Forget { id } => forget(&file, &id),
        Command::Stats => stats(&file),
        Command::Vacuum => Err(
            "vacuum is not implemented yet (planned for v0.2; forgotten memories are \
             filtered from every read in the meantime, they just still occupy file space)"
                .to_string(),
        ),
    }
}

/// `embedmind serve`: the MCP server over stdio, identical to running the
/// `embedmind-mcp` binary (README: `claude mcp add embedmind -- embedmind
/// serve`). Logs on stderr; stdout is the protocol channel.
fn serve(file: &Path) -> Result<(), String> {
    let store = open(file)?;
    let project = std::env::current_dir()
        .ok()
        .and_then(|cwd| detect_project(&cwd));
    match &project {
        Some(name) => eprintln!(
            "embedmind: serving memories from {} (project: {name})",
            file.display()
        ),
        None => eprintln!(
            "embedmind: serving memories from {} (no project context)",
            file.display()
        ),
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    McpServer::new(store, project)
        .serve(stdin.lock(), stdout.lock())
        .map_err(|e| format!("transport error: {e}"))
}

fn remember(
    file: &Path,
    content: String,
    project: Option<String>,
    global: bool,
) -> Result<(), String> {
    let mut store = open(file)?;
    let project = if global {
        None
    } else {
        project.or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| detect_project(&cwd))
        })
    };
    let mut draft = MemoryDraft::new(content).agent("cli");
    if let Some(project) = &project {
        draft = draft.project(project.clone());
    }
    let memory = store
        .remember(draft)
        .map_err(|e| format!("remember failed: {e}"))?;
    store.close().map_err(|e| format!("close failed: {e}"))?;
    match &project {
        Some(name) => println!("{} (project: {name})", memory.id),
        None => println!("{} (global)", memory.id),
    }
    Ok(())
}

fn recall(
    file: &Path,
    text: String,
    limit: usize,
    project: Option<String>,
    all: bool,
    filters: Vec<String>,
) -> Result<(), String> {
    let store = open(file)?;
    let mut query = Query::new(text).limit(limit);
    if !filters.is_empty() {
        query = query.filters(parse_filters(&filters)?);
    }
    let scope = if all {
        None
    } else {
        project.or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| detect_project(&cwd))
        })
    };
    if let Some(project) = &scope {
        query = query.project(project.clone());
    }
    let hits = store
        .recall(query)
        .map_err(|e| format!("recall failed: {e}"))?;

    match &scope {
        Some(name) => eprintln!("searching project: {name} (use --all for everything)"),
        None => eprintln!("searching all projects"),
    }
    if hits.is_empty() {
        eprintln!("no memories found");
        return Ok(());
    }
    for hit in &hits {
        let project = hit.project.as_deref().unwrap_or("global");
        println!("[{:>5.3}] {}  ({project})", hit.score, hit.id);
        println!("        {}", hit.content.replace('\n', "\n        "));
    }
    Ok(())
}

fn forget(file: &Path, id: &str) -> Result<(), String> {
    let id = Ulid::from_string(id).map_err(|_| format!("'{id}' is not a valid memory id"))?;
    let mut store = open(file)?;
    let forgotten = store
        .forget(id)
        .map_err(|e| format!("forget failed: {e}"))?;
    store.close().map_err(|e| format!("close failed: {e}"))?;
    if forgotten {
        println!("forgotten: {id}");
        Ok(())
    } else {
        Err(format!("no live memory with id {id}"))
    }
}

fn stats(file: &Path) -> Result<(), String> {
    let store = open(file)?;
    let stats = store.stats().map_err(|e| format!("stats failed: {e}"))?;
    println!("file:               {}", file.display());
    println!(
        "size:               {} ({} pages × {} bytes)",
        human_bytes(stats.file_bytes),
        stats.page_count,
        stats.page_size
    );
    println!("live memories:      {}", stats.live_memories);
    println!(
        "forgotten:          {} (space reclaimed by vacuum)",
        stats.forgotten_memories
    );
    println!("index entries:      {}", stats.index_entries);
    match &stats.embedding_model_id {
        Some(model) => println!(
            "embedding model:    {model} ({} dims)",
            stats.embedding_dims
        ),
        None => println!("embedding model:    none (KV-only so far)"),
    }
    Ok(())
}

/// Opens (or creates) the store with the default embedded model.
fn open(file: &Path) -> Result<Store, String> {
    Store::open_or_create(file).map_err(|e| format!("cannot open {}: {e}", file.display()))
}

/// `~/.embedmind/memory.mind`, cross-platform (`USERPROFILE` on Windows,
/// `HOME` elsewhere) — the same default as the `embedmind-mcp` binary.
fn default_memory_file() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("cannot determine home directory (HOME/USERPROFILE unset); pass --file")?;
    Ok(PathBuf::from(home).join(".embedmind").join("memory.mind"))
}

/// Parses `--filter` arguments into the engine's `key -> Filter` map (S10).
/// Pure argument transport — the AND semantics, type-matching and
/// anti-under-return all live in the engine. Supported forms per argument:
///
/// - `key=lo..hi` — closed numeric range `[lo, hi]`.
/// - `key>=n` / `key<=n` — open numeric bound; repeating both on one key
///   bounds both ends (they merge into one range).
/// - `key=value` — exact match; `value` is typed by parse (i64, then f64,
///   then bool `true`/`false`, else string).
fn parse_filters(specs: &[String]) -> Result<BTreeMap<String, Filter>, String> {
    let mut out: BTreeMap<String, Filter> = BTreeMap::new();
    for spec in specs {
        let (key, filter) = if let Some((key, n)) = spec.split_once(">=") {
            (
                key,
                Filter::Range {
                    min: Some(parse_num(n)?),
                    max: None,
                },
            )
        } else if let Some((key, n)) = spec.split_once("<=") {
            (
                key,
                Filter::Range {
                    min: None,
                    max: Some(parse_num(n)?),
                },
            )
        } else if let Some((key, value)) = spec.split_once('=') {
            if let Some((lo, hi)) = value.split_once("..") {
                (
                    key,
                    Filter::Range {
                        min: Some(parse_num(lo)?),
                        max: Some(parse_num(hi)?),
                    },
                )
            } else {
                (key, Filter::Eq(parse_scalar(value)))
            }
        } else {
            return Err(format!(
                "invalid --filter '{spec}': expected key=value, key=lo..hi, key>=n or key<=n"
            ));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("invalid --filter '{spec}': empty key"));
        }
        merge_filter(&mut out, key, filter, spec)?;
    }
    Ok(out)
}

/// Inserts `filter` under `key`, merging two open range bounds on the same key
/// (`key>=n` plus `key<=m`) into one closed range. Any other collision on a
/// key is a conflicting filter and an error.
fn merge_filter(
    out: &mut BTreeMap<String, Filter>,
    key: &str,
    filter: Filter,
    spec: &str,
) -> Result<(), String> {
    match (out.remove(key), filter) {
        (None, f) => {
            out.insert(key.to_string(), f);
        }
        (Some(Filter::Range { min: m1, max: x1 }), Filter::Range { min: m2, max: x2 }) => {
            out.insert(
                key.to_string(),
                Filter::Range {
                    min: m1.or(m2),
                    max: x1.or(x2),
                },
            );
        }
        (Some(_), _) => {
            return Err(format!(
                "conflicting --filter for key '{key}' (from '{spec}')"
            ));
        }
    }
    Ok(())
}

/// Parses a numeric range bound; both integers and floats are accepted and
/// carried as `f64` (the engine compares numeric metadata as `f64`).
fn parse_num(s: &str) -> Result<f64, String> {
    s.trim()
        .parse::<f64>()
        .map_err(|_| format!("invalid numeric bound '{s}' in --filter"))
}

/// Types a bare `key=value` right-hand side: integer, then float, then boolean
/// `true`/`false`, else a string — the natural CLI inference. Metadata values
/// stored as the matching type will compare equal in the engine.
fn parse_scalar(value: &str) -> Scalar {
    if let Ok(i) = value.parse::<i64>() {
        Scalar::I64(i)
    } else if let Ok(f) = value.parse::<f64>() {
        Scalar::F64(f)
    } else if value == "true" {
        Scalar::Bool(true)
    } else if value == "false" {
        Scalar::Bool(false)
    } else {
        Scalar::Str(value.to_string())
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
