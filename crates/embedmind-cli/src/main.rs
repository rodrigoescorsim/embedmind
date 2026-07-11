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
use embedmind_mcp::{McpServer, OpLog, detect_project};

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
    Serve {
        /// Append one JSON line (JSONL) per tool call to this file:
        /// {ts, tool, args (content/query truncated), ids, scores,
        /// latency_ms, project, isError}. A write failure never fails the
        /// tool call (warning on stderr); without the flag nothing is
        /// created.
        #[arg(long = "op-log", value_name = "PATH")]
        op_log: Option<PathBuf>,
    },
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
        /// Tag the memory with an explicit entity ("postgres",
        /// "auth-service", ...; repeatable). Query back with
        /// `embedmind related --entity NAME`.
        #[arg(long = "entity", value_name = "NAME")]
        entities: Vec<String>,
        /// Relate this memory to an existing one (repeatable): KIND=ID,
        /// e.g. `refines=01ABC...`. The target must exist and be live.
        /// Navigate back with `embedmind related ID`.
        #[arg(long = "relation", value_name = "KIND=ID")]
        relations: Vec<String>,
        /// Mark this memory as the new version of an existing one
        /// (repeatable): the target disappears from every later recall but
        /// stays readable as history via `embedmind related ID`. The target
        /// must exist, be live, and belong to the same project.
        #[arg(long = "supersedes", value_name = "ID")]
        supersedes: Vec<String>,
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
        /// Only recall memories written by this agent (basic provenance;
        /// see `stats` for which agents have memories)
        #[arg(long)]
        agent: Option<String>,
        /// Also pull each hit's explicitly related memories (1 hop, both
        /// directions), appended after the ranked hits as connected context
        #[arg(long)]
        expand_related: bool,
        /// Break ties among equally-relevant matches toward the newer
        /// memory (a third RRF list, never displaces a stronger old match)
        #[arg(long)]
        recency: bool,
    },
    /// Navigate the explicit memory graph: neighbors of one memory, or
    /// every memory tagged with an entity
    Related {
        /// Memory id whose entity tags and related memories to list
        #[arg(required_unless_present = "entity")]
        id: Option<String>,
        /// List every memory tagged with this entity instead
        #[arg(long, conflicts_with = "id")]
        entity: Option<String>,
    },
    /// Delete one memory by id
    Forget { id: String },
    /// Show file size, counts and index health
    Stats,
    /// Usage report: is the memory actually being used? Aggregates the
    /// op-log written by `serve --op-log` (sessions, recalls served,
    /// per-memory counters, latency) and joins it with the store (top
    /// recalled memories, memories never recalled in the window)
    Report {
        /// Op-log JSONL written by `serve --op-log` — the usage source.
        /// Without it (or if the file does not exist yet) the report
        /// degrades to store totals only
        #[arg(long = "op-log", value_name = "PATH")]
        op_log: Option<PathBuf>,
        /// Window in days
        #[arg(long, default_value_t = 7)]
        since: u64,
        /// Machine-readable JSON instead of the human print
        #[arg(long)]
        json: bool,
    },
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
        Command::Serve { op_log } => serve(&file, op_log),
        Command::Remember {
            content,
            project,
            global,
            entities,
            relations,
            supersedes,
        } => remember(
            &file, content, project, global, entities, relations, supersedes,
        ),
        Command::Recall {
            query,
            limit,
            project,
            all,
            filters,
            agent,
            expand_related,
            recency,
        } => recall(
            &file,
            query,
            limit,
            project,
            all,
            filters,
            agent,
            expand_related,
            recency,
        ),
        Command::Related { id, entity } => related(&file, id, entity),
        Command::Forget { id } => forget(&file, &id),
        Command::Stats => stats(&file),
        Command::Report {
            op_log,
            since,
            json,
        } => report(&file, op_log, since, json),
        Command::Vacuum => vacuum(&file),
    }
}

/// `embedmind serve`: the MCP server over stdio, identical to running the
/// `embedmind-mcp` binary (README: `claude mcp add embedmind -- embedmind
/// serve`). Logs on stderr; stdout is the protocol channel. `--op-log`
/// (S22) appends one JSON line per tool call to the given file — pure
/// observability, opened here and handed to the server; failing to open it
/// is a startup error (the operator would silently lose the log they asked
/// for), while later write failures only warn on stderr.
fn serve(file: &Path, op_log: Option<PathBuf>) -> Result<(), String> {
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
    let mut server = McpServer::new(store, project);
    if let Some(path) = op_log {
        let op_log = OpLog::create(&path)
            .map_err(|e| format!("cannot open op-log {}: {e}", path.display()))?;
        eprintln!("embedmind: op-log appending to {}", path.display());
        server = server.with_op_log(op_log);
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    server
        .serve(stdin.lock(), stdout.lock())
        .map_err(|e| format!("transport error: {e}"))
}

#[allow(clippy::too_many_arguments)] // one CLI flag each; a struct would just rename them
fn remember(
    file: &Path,
    content: String,
    project: Option<String>,
    global: bool,
    entities: Vec<String>,
    relations: Vec<String>,
    supersedes: Vec<String>,
) -> Result<(), String> {
    let relations = parse_relations(&relations)?;
    let supersedes = parse_supersedes(&supersedes)?;
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
    let mut draft = MemoryDraft::new(content)
        .agent("cli")
        .entities(entities.clone())
        .relations(relations.clone())
        .supersedes(supersedes.clone());
    if let Some(project) = &project {
        draft = draft.project(project.clone());
    }
    let remembered = store
        .remember_detailed(draft)
        .map_err(|e| format!("remember failed: {e}"))?;
    store.close().map_err(|e| format!("close failed: {e}"))?;
    match &project {
        Some(name) => println!("{} (project: {name})", remembered.memory.id),
        None => println!("{} (global)", remembered.memory.id),
    }
    if !entities.is_empty() {
        println!("entities: {}", entities.join(", "));
    }
    for (kind, target) in &relations {
        println!("relation: {kind} -> {target}");
    }
    for target in &supersedes {
        println!("supersedes: {target}");
    }
    // Write-time curation (S21): the memory IS stored; these lines only hint
    // that a near-duplicate already exists, so the user can forget it, store
    // again with --supersedes, or keep both. Wording per docs/01-spec.md S21.
    for similar in &remembered.similar {
        println!(
            "memória parecida existente: {} — {}",
            similar.id,
            similar.content.replace(['\r', '\n'], " ")
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // one CLI flag each; a struct would just rename them
fn recall(
    file: &Path,
    text: String,
    limit: usize,
    project: Option<String>,
    all: bool,
    filters: Vec<String>,
    agent: Option<String>,
    expand_related: bool,
    recency: bool,
) -> Result<(), String> {
    let store = open(file)?;
    let mut query = Query::new(text)
        .limit(limit)
        .expand_related(expand_related)
        .recency(recency);
    if !filters.is_empty() {
        query = query.filters(parse_filters(&filters)?);
    }
    if let Some(agent) = &agent {
        query = query.agent(agent.clone());
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
    let outcome = store
        .recall_detailed(query)
        .map_err(|e| format!("recall failed: {e}"))?;
    let hits = outcome.hits;

    // S9 edge: a file written before the full-text index existed still
    // recalls, vector-only — warn (stderr, like every status line here),
    // never fail. `vacuum` rebuilds the file with the index.
    if outcome.degraded_to_vector_only {
        eprintln!(
            "warning: this file has no full-text index (written by an older \
             version); results are vector-only. Run `embedmind vacuum` to build it"
        );
    }
    match &scope {
        Some(name) => eprintln!("searching project: {name} (use --all for everything)"),
        None => eprintln!("searching all projects"),
    }
    if let Some(agent) = &agent {
        eprintln!("filtered to agent: {agent}");
    }
    if hits.is_empty() {
        eprintln!("no memories found");
        return Ok(());
    }
    for hit in &hits {
        let project = hit.project.as_deref().unwrap_or("global");
        // Graph-expanded hits carry exactly 0.0 (connected context, not a
        // ranked match — RRF scores are strictly positive): mark them.
        if expand_related && hit.score == 0.0 {
            println!("[  rel] {}  ({project})", hit.id);
        } else {
            println!("[{:>5.3}] {}  ({project})", hit.score, hit.id);
        }
        println!("        {}", hit.content.replace('\n', "\n        "));
    }
    Ok(())
}

/// `embedmind related <ID>` / `embedmind related --entity NAME`: the graph
/// navigation of S13. By id: the memory's entity tags plus its relation
/// neighbors, both directions. By entity: every live memory tagged with it.
fn related(file: &Path, id: Option<String>, entity: Option<String>) -> Result<(), String> {
    let store = open(file)?;
    if let Some(entity) = entity {
        let members = store
            .entity_members(&entity)
            .map_err(|e| format!("related failed: {e}"))?;
        if members.is_empty() {
            eprintln!("no memories tagged with entity '{entity}'");
            return Ok(());
        }
        eprintln!("memories tagged with entity '{entity}':");
        for memory in &members {
            let project = memory.project.as_deref().unwrap_or("global");
            println!("{}  ({project})", memory.id);
            println!("        {}", memory.content.replace('\n', "\n        "));
        }
        return Ok(());
    }
    // Clap guarantees `id` is present when `--entity` is absent.
    let id = id.ok_or("related: a memory id or --entity is required")?;
    let id = Ulid::from_string(&id).map_err(|_| format!("'{id}' is not a valid memory id"))?;
    if store
        .get(id)
        .map_err(|e| format!("related failed: {e}"))?
        .is_none()
    {
        return Err(format!("no live memory with id {id}"));
    }
    let entities = store
        .entities_of(id)
        .map_err(|e| format!("related failed: {e}"))?;
    if !entities.is_empty() {
        println!("entities: {}", entities.join(", "));
    }
    let related = store
        .related(id)
        .map_err(|e| format!("related failed: {e}"))?;
    if related.is_empty() {
        eprintln!("no related memories");
        return Ok(());
    }
    for rel in &related {
        let project = rel.project.as_deref().unwrap_or("global");
        // `->` = this memory relates to the neighbor; `<-` = the neighbor
        // relates to this memory. A superseded neighbor is history (S19):
        // readable here, excluded from recall — say so.
        let arrow = if rel.outgoing { "->" } else { "<-" };
        let marker = if rel.superseded { "  [superseded]" } else { "" };
        println!("{arrow} {:<14} {}  ({project}){marker}", rel.kind, rel.id);
        println!("        {}", rel.content.replace('\n', "\n        "));
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
    // Provenance breakdown (S14): live memories per writing agent, biggest
    // first. Only shown when there is at least one live memory.
    if !stats.by_agent.is_empty() {
        println!("by agent:");
        let mut agents: Vec<_> = stats.by_agent.iter().collect();
        agents.sort_by(|a, b| b.1.live_memories.cmp(&a.1.live_memories).then(a.0.cmp(b.0)));
        for (agent, agent_stats) in agents {
            let name = if agent.is_empty() { "(unknown)" } else { agent };
            let sessions = match agent_stats.sessions.len() {
                0 => String::new(),
                1 => ", 1 session".to_string(),
                n => format!(", {n} sessions"),
            };
            println!(
                "  {name:<18}{} memories{sessions}",
                agent_stats.live_memories
            );
        }
    }
    Ok(())
}

/// `embedmind report` (S23): the trust answer — "what did the memory do for
/// me this week?". Two sources joined here, no domain logic: the op-log
/// aggregation lives in `embedmind_mcp::report` (next to the writer that
/// owns the line format) and the store supplies previews + the live set for
/// dead-weight detection. Missing/absent op-log degrades to store totals —
/// a user who never passed `--op-log` still gets a useful (and instructive)
/// print, never an error.
fn report(
    file: &Path,
    op_log: Option<PathBuf>,
    since_days: u64,
    json_out: bool,
) -> Result<(), String> {
    let store = open(file)?;

    // Live memories in id order = time order (ULIDs): previews for the top
    // list and the base set for "never recalled". Superseded memories are
    // history (S19) — readable for previews, excluded from dead weight.
    struct Live {
        id: String,
        content: String,
        created_at_micros: i64,
        superseded: bool,
    }
    let mut live: Vec<Live> = Vec::new();
    for memory in store.iter() {
        let m = memory.map_err(|e| format!("cannot read {}: {e}", file.display()))?;
        live.push(Live {
            id: m.id.to_string(),
            content: m.content,
            created_at_micros: m.provenance.created_at_micros,
            superseded: m.superseded,
        });
    }
    let active: Vec<&Live> = live.iter().filter(|m| !m.superseded).collect();

    let since_micros = epoch_micros_now().saturating_sub(since_days * 24 * 3600 * 1_000_000);
    let usage = match &op_log {
        Some(path) if path.exists() => {
            let reader = std::fs::File::open(path)
                .map(std::io::BufReader::new)
                .map_err(|e| format!("cannot open op-log {}: {e}", path.display()))?;
            Some(embedmind_mcp::aggregate(reader, since_micros))
        }
        _ => None,
    };

    // Top recalled: usage counters joined with previews. A served id no
    // longer in the store was forgotten meanwhile — say so, don't hide it.
    let mut top: Vec<(&String, &u64)> = usage.iter().flat_map(|u| u.served.iter()).collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    top.truncate(5);
    let preview_of = |id: &str| -> String {
        live.iter()
            .find(|m| m.id == id)
            .map(|m| preview(&m.content))
            .unwrap_or_else(|| "(forgotten since)".to_string())
    };

    // Dead weight: live, non-superseded memories no successful recall served
    // inside the window. Oldest first — the most likely stale.
    let never: Vec<&&Live> = usage
        .as_ref()
        .map(|u| {
            active
                .iter()
                .filter(|m| !u.served.contains_key(&m.id))
                .collect()
        })
        .unwrap_or_default();

    if json_out {
        let value = serde_json::json!({
            "sinceDays": since_days,
            "opLog": op_log.as_ref().filter(|p| p.exists()).map(|p| p.display().to_string()),
            "liveMemories": active.len(),
            "sessions": usage.as_ref().map(|u| u.sessions),
            "recalls": usage.as_ref().map(|u| serde_json::json!({
                "count": u.recalls,
                "empty": u.recalls_empty,
                "errors": u.recall_errors,
                "latencyP50Ms": u.recall_latency_p50_ms,
                "latencyP99Ms": u.recall_latency_p99_ms,
            })),
            "remembers": usage.as_ref().map(|u| serde_json::json!({
                "count": u.remembers,
                "errors": u.remember_errors,
            })),
            "forgets": usage.as_ref().map(|u| u.forgets),
            "relatedCalls": usage.as_ref().map(|u| u.related_calls),
            "skippedLines": usage.as_ref().map(|u| u.skipped_lines),
            "topMemories": top.iter().map(|(id, count)| serde_json::json!({
                "id": id, "recalls": count, "content": preview_of(id),
            })).collect::<Vec<_>>(),
            "neverRecalled": usage.as_ref().map(|_| serde_json::json!({
                "count": never.len(),
                "sample": never.iter().take(5).map(|m| serde_json::json!({
                    "id": m.id,
                    "createdAtMicros": m.created_at_micros,
                    "content": preview(&m.content),
                })).collect::<Vec<_>>(),
            })),
        });
        println!("{value}");
        return Ok(());
    }

    println!("usage report: last {since_days} days");
    let Some(usage) = usage.as_ref() else {
        match &op_log {
            Some(path) => println!("op-log:            {} (not found)", path.display()),
            None => println!("op-log:            none"),
        }
        println!("live memories:     {}", active.len());
        println!();
        println!(
            "usage needs the op-log: run `embedmind serve --op-log <file>.jsonl` \
             and pass --op-log here"
        );
        return Ok(());
    };
    // `op_log` is Some(existing) whenever `usage` is Some — same match arm.
    if let Some(path) = &op_log {
        println!("op-log:            {}", path.display());
    }
    println!("sessions:          {}", usage.sessions);
    let latency = match (usage.recall_latency_p50_ms, usage.recall_latency_p99_ms) {
        (Some(p50), Some(p99)) => format!(" · latency p50 {p50:.1} ms · p99 {p99:.1} ms"),
        _ => String::new(),
    };
    println!(
        "recalls:           {} ({} empty, {} errors){latency}",
        usage.recalls, usage.recalls_empty, usage.recall_errors
    );
    println!(
        "remembers:         {} ({} errors)",
        usage.remembers, usage.remember_errors
    );
    println!("forgets:           {}", usage.forgets);
    println!("related:           {}", usage.related_calls);
    if usage.skipped_lines > 0 {
        println!("skipped log lines: {}", usage.skipped_lines);
    }
    if !top.is_empty() {
        println!();
        println!("top recalled memories:");
        for (id, count) in &top {
            println!("  {count}×  {id}  {}", preview_of(id));
        }
    }
    println!();
    println!(
        "never recalled in window: {} of {} live",
        never.len(),
        active.len()
    );
    for m in never.iter().take(5) {
        println!("  {}  {}", m.id, preview(&m.content));
    }
    if never.len() > 5 {
        println!("  … and {} more", never.len() - 5);
    }
    Ok(())
}

/// First line of `content`, capped at 96 chars (`…` marks either cut) — the
/// report is a summary, never a second copy of the data.
fn preview(content: &str) -> String {
    let first = content.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(96).collect();
    if first.chars().count() > 96 || content.lines().count() > 1 {
        out.push('…');
    }
    out
}

/// Current time as epoch microseconds — the `created_at_micros` convention.
fn epoch_micros_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn vacuum(file: &Path) -> Result<(), String> {
    let mut store = open(file)?;
    let before = store.stats().map_err(|e| format!("stats failed: {e}"))?;
    if before.forgotten_memories == 0 {
        eprintln!("nothing forgotten; vacuum still repacks and rebuilds the indexes");
    }
    store.vacuum().map_err(|e| format!("vacuum failed: {e}"))?;
    let after = store.stats().map_err(|e| format!("stats failed: {e}"))?;
    store.close().map_err(|e| format!("close failed: {e}"))?;

    let reclaimed = before.file_bytes.saturating_sub(after.file_bytes);
    println!(
        "vacuumed: {} live memories, {} forgotten reclaimed",
        after.live_memories, before.forgotten_memories
    );
    println!(
        "size:     {} -> {} ({} freed)",
        human_bytes(before.file_bytes),
        human_bytes(after.file_bytes),
        human_bytes(reclaimed),
    );
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

/// Parses `--relation KIND=ID` arguments into the engine's `(kind, target)`
/// pairs (S13). Pure argument transport — target existence/liveness is
/// validated by the engine inside the `remember` transaction.
fn parse_relations(specs: &[String]) -> Result<Vec<(String, Ulid)>, String> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let Some((kind, target)) = spec.split_once('=') else {
            return Err(format!(
                "invalid --relation '{spec}': expected KIND=ID (e.g. refines=01ABC...)"
            ));
        };
        let kind = kind.trim();
        if kind.is_empty() {
            return Err(format!("invalid --relation '{spec}': empty kind"));
        }
        let target = Ulid::from_string(target.trim())
            .map_err(|_| format!("invalid --relation '{spec}': '{target}' is not a memory id"))?;
        out.push((kind.to_string(), target));
    }
    Ok(out)
}

/// Parses `--supersedes ID` arguments into memory ids (S19). Pure argument
/// transport — existence/liveness/project checks are the engine's, inside
/// the `remember` transaction.
fn parse_supersedes(specs: &[String]) -> Result<Vec<Ulid>, String> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let id = Ulid::from_string(spec.trim())
            .map_err(|_| format!("invalid --supersedes '{spec}': not a memory id"))?;
        out.push(id);
    }
    Ok(out)
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
