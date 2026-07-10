//! The MCP server loop: newline-delimited JSON-RPC 2.0 over any
//! `BufRead`/`Write` pair (`docs/adr/0009`) — stdio in production, in-memory
//! buffers in tests.
//!
//! Implements the tools-only subset of MCP: `initialize`,
//! `notifications/initialized`, `ping`, `tools/list`, `tools/call`. Unknown
//! methods get JSON-RPC `-32601`; malformed JSON gets `-32700`; bad tool
//! arguments get `-32602`. Engine failures during a tool call are reported
//! as a tool result with `isError: true` (per the MCP spec), never a server
//! crash.

use std::io::{BufRead, Write};

use embedmind_core::{Filter, MemoryDraft, Query, Scalar, Store, Ulid};
use serde_json::{Value, json};

/// Protocol revisions this server knows; the handshake echoes the client's
/// requested version when it is one of these, otherwise answers the latest.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The MCP memory server: owns the [`Store`] and serves one client over a
/// read/write pair (stdio is inherently one client per process, ADR 0009).
pub struct McpServer {
    store: Store,
    /// Client name from `initialize` (`clientInfo.name`) — recorded as the
    /// writing agent on every memory, the basic provenance that is free tier
    /// (CLAUDE.md decision 3).
    agent: String,
    /// Detected project context (M1 item 1.5, `crate::project`): stamped on
    /// `remember` and used as the default `recall` scope. `None` = no
    /// context; memories are global, recall searches everything.
    project: Option<String>,
}

impl McpServer {
    /// Wraps an open store. The caller decides where the file lives and
    /// detects the project context ([`crate::project::detect_project`]).
    pub fn new(store: Store, project: Option<String>) -> Self {
        McpServer {
            store,
            agent: "mcp".to_string(),
            project,
        }
    }

    /// Serves until EOF on `reader`. Only transport failures (I/O on the
    /// pipes) end the loop with an error; protocol and engine problems are
    /// answered in-band and the loop continues.
    pub fn serve(&mut self, reader: impl BufRead, mut writer: impl Write) -> std::io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle_line(&line) {
                let mut bytes = serde_json::to_vec(&response)?;
                bytes.push(b'\n');
                writer.write_all(&bytes)?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    /// Handles one raw message; `None` = no response (notification).
    fn handle_line(&mut self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return Some(error_response(Value::Null, PARSE_ERROR, "parse error")),
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        let Some(method) = method else {
            // A message with an id but no method is a client-side response;
            // this server never issues requests, so there is nothing to do.
            return id.map(|id| error_response(id, METHOD_NOT_FOUND, "method missing"));
        };

        // Notifications (no id) get no response, whatever the method.
        let id = id?;

        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools_list()),
            "tools/call" => self.tools_call(&params),
            _ => Err((METHOD_NOT_FOUND, "method not found".to_string())),
        };
        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    fn initialize(&mut self, params: &Value) -> Value {
        if let Some(name) = params
            .pointer("/clientInfo/name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        {
            self.agent = name.to_string();
        }
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = match requested {
            Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
            _ => LATEST_PROTOCOL_VERSION,
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "embedmind",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })
    }

    /// Dispatches one `tools/call`. Unknown tool / malformed arguments are
    /// protocol errors (`-32602`, per the MCP spec); a failure while
    /// *executing* a known tool is a tool result with `isError: true`.
    fn tools_call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "missing tool name".to_string()))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let outcome = match name {
            "remember" => self.tool_remember(&args)?,
            "recall" => self.tool_recall(&args)?,
            "related" => self.tool_related(&args)?,
            "stats" => self.tool_stats(&args)?,
            "forget" => self.tool_forget(&args)?,
            _ => return Err((INVALID_PARAMS, format!("unknown tool: {name}"))),
        };
        Ok(match outcome {
            Ok(structured) => {
                let text = structured.to_string();
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": structured,
                })
            }
            Err(engine_error) => json!({
                "content": [{ "type": "text", "text": engine_error }],
                "isError": true,
            }),
        })
    }

    /// `remember(content, project?, metadata?, entities?, relations?,
    /// supersedes?)` → `{id, project, entities, relations, supersedes}`
    /// (DESIGN §8). `project` omitted = the detected context (item 1.5);
    /// explicit `null` = force a global memory; explicit string = that
    /// project. `entities`/`relations` are the explicit graph layer (S13,
    /// `docs/adr/0012`): caller-provided, never extracted — a relation whose
    /// target does not exist (or was forgotten) is a tool error from the
    /// engine, so nothing is stored. `supersedes` (S19, `docs/adr/0013`)
    /// marks each listed memory as the previous version of this one: hidden
    /// from every later recall, still readable via `related`/history. The
    /// shell only parses — target validation lives in the engine.
    #[allow(clippy::type_complexity)]
    fn tool_remember(&mut self, args: &Value) -> Result<Result<Value, String>, (i64, String)> {
        let content = args.get("content").and_then(Value::as_str).ok_or((
            INVALID_PARAMS,
            "remember: 'content' (string) is required".to_string(),
        ))?;
        let mut draft = MemoryDraft::new(content).agent(self.agent.clone());
        let project = match args.get("project") {
            None => self.project.clone(),
            Some(Value::Null) => None,
            Some(value) => {
                let name = value.as_str().ok_or((
                    INVALID_PARAMS,
                    "remember: 'project' must be a string (or null for global)".to_string(),
                ))?;
                Some(name.to_string())
            }
        };
        if let Some(project) = &project {
            draft = draft.project(project.clone());
        }
        if let Some(metadata) = args.get("metadata") {
            let entries = metadata.as_object().ok_or((
                INVALID_PARAMS,
                "remember: 'metadata' must be an object".to_string(),
            ))?;
            for (key, value) in entries {
                let scalar = json_to_scalar(value).ok_or((
                    INVALID_PARAMS,
                    "remember: metadata values must be string/number/bool/null".to_string(),
                ))?;
                draft = draft.meta(key.clone(), scalar);
            }
        }
        // Explicit graph data (S13): entity tags and typed relations to
        // existing memories. The shell only parses — target-liveness
        // validation lives in the engine (`Store::remember`, ADR 0012).
        let mut entities: Vec<String> = Vec::new();
        if let Some(value) = args.get("entities") {
            let list = value.as_array().ok_or((
                INVALID_PARAMS,
                "remember: 'entities' must be an array of strings".to_string(),
            ))?;
            for item in list {
                let name = item.as_str().ok_or((
                    INVALID_PARAMS,
                    "remember: 'entities' must be an array of strings".to_string(),
                ))?;
                entities.push(name.to_string());
            }
            draft = draft.entities(entities.clone());
        }
        let mut relations: Vec<(String, Ulid)> = Vec::new();
        if let Some(value) = args.get("relations") {
            let list = value.as_array().ok_or((
                INVALID_PARAMS,
                "remember: 'relations' must be an array of {kind, target} objects".to_string(),
            ))?;
            for item in list {
                let kind = item.get("kind").and_then(Value::as_str).ok_or((
                    INVALID_PARAMS,
                    "remember: each relation needs a 'kind' (string)".to_string(),
                ))?;
                let target = item.get("target").and_then(Value::as_str).ok_or((
                    INVALID_PARAMS,
                    "remember: each relation needs a 'target' (memory id string)".to_string(),
                ))?;
                let Ok(target) = Ulid::from_string(target) else {
                    return Err((
                        INVALID_PARAMS,
                        "remember: a relation 'target' is not a valid ULID".to_string(),
                    ));
                };
                relations.push((kind.to_string(), target));
            }
            draft = draft.relations(relations.clone());
        }
        let mut supersedes: Vec<Ulid> = Vec::new();
        if let Some(value) = args.get("supersedes") {
            let list = value.as_array().ok_or((
                INVALID_PARAMS,
                "remember: 'supersedes' must be an array of memory id strings".to_string(),
            ))?;
            for item in list {
                let id = item.as_str().ok_or((
                    INVALID_PARAMS,
                    "remember: 'supersedes' must be an array of memory id strings".to_string(),
                ))?;
                let Ok(id) = Ulid::from_string(id) else {
                    return Err((
                        INVALID_PARAMS,
                        "remember: a 'supersedes' id is not a valid ULID".to_string(),
                    ));
                };
                supersedes.push(id);
            }
            draft = draft.supersedes(supersedes.clone());
        }
        Ok(match self.store.remember(draft) {
            Ok(memory) => Ok(json!({
                "id": memory.id.to_string(),
                "project": project,
                "entities": entities,
                "relations": relations
                    .iter()
                    .map(|(kind, target)| json!({ "kind": kind, "target": target.to_string() }))
                    .collect::<Vec<Value>>(),
                "supersedes": supersedes
                    .iter()
                    .map(|id| Value::String(id.to_string()))
                    .collect::<Vec<Value>>(),
            })),
            Err(e) => Err(e.to_string()),
        })
    }

    /// `recall(query, limit?=8, project?, scope?)` → hits best-first with
    /// scores (DESIGN §8). Default scope is the detected project context
    /// (item 1.5, DESIGN §7); `scope: "all"` is the explicit global
    /// fallback; `project` targets one specific project.
    #[allow(clippy::type_complexity)]
    fn tool_recall(&mut self, args: &Value) -> Result<Result<Value, String>, (i64, String)> {
        let text = args.get("query").and_then(Value::as_str).ok_or((
            INVALID_PARAMS,
            "recall: 'query' (string) is required".to_string(),
        ))?;
        let mut query = Query::new(text);
        if let Some(limit) = args.get("limit") {
            let limit = limit.as_u64().filter(|&l| l >= 1).ok_or((
                INVALID_PARAMS,
                "recall: 'limit' must be a positive integer".to_string(),
            ))?;
            query = query.limit(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        // Optional metadata filters (S10): `{ key: value | {min?, max?} }`, all
        // ANDed. A bare scalar is an equality filter; an object with min/max is
        // a numeric range. Absent = no filtering — the schema stays backward
        // compatible for clients that never send `filters`.
        if let Some(filters) = args.get("filters") {
            let entries = filters.as_object().ok_or((
                INVALID_PARAMS,
                "recall: 'filters' must be an object of key -> value or {min?, max?}".to_string(),
            ))?;
            let mut parsed = std::collections::BTreeMap::new();
            for (key, spec) in entries {
                let filter = json_to_filter(spec).ok_or((
                    INVALID_PARAMS,
                    "recall: each filter must be a string/number/bool/null value or a \
                     {min?, max?} range object with numeric bounds"
                        .to_string(),
                ))?;
                parsed.insert(key.clone(), filter);
            }
            query = query.filters(parsed);
        }

        // Optional 1-hop graph expansion (S13): each direct hit's relation
        // neighbors are appended as connected context with score 0.0 (they
        // matched the graph, not the query). Absent = no expansion.
        if let Some(expand) = args.get("expand_related") {
            let expand = expand.as_bool().ok_or((
                INVALID_PARAMS,
                "recall: 'expand_related' must be a boolean".to_string(),
            ))?;
            query = query.expand_related(expand);
        }

        // Optional provenance filter by writing agent (S14): keep only memories
        // whose recorded agent equals this string. Absent = no agent filtering.
        if let Some(agent) = args.get("agent") {
            let agent = agent.as_str().ok_or((
                INVALID_PARAMS,
                "recall: 'agent' must be a string".to_string(),
            ))?;
            query = query.agent(agent.to_string());
        }

        let scope_all = match args.get("scope").and_then(Value::as_str) {
            None | Some("project") => false,
            Some("all") => true,
            Some(_) => {
                return Err((
                    INVALID_PARAMS,
                    "recall: 'scope' must be \"project\" or \"all\"".to_string(),
                ));
            }
        };
        let project = match args.get("project") {
            None => self.project.clone(),
            Some(value) => {
                let name = value.as_str().ok_or((
                    INVALID_PARAMS,
                    "recall: 'project' must be a string".to_string(),
                ))?;
                Some(name.to_string())
            }
        };
        // The scope actually applied, echoed back so the agent knows what
        // it searched: "all", or the project name.
        let applied_scope = if scope_all {
            json!("all")
        } else if let Some(project) = &project {
            query = query.project(project.clone());
            json!(project)
        } else {
            json!("all")
        };

        Ok(match self.store.recall_detailed(query) {
            Ok(outcome) => {
                let hits: Vec<Value> = outcome
                    .hits
                    .iter()
                    .map(|hit| {
                        json!({
                            "id": hit.id.to_string(),
                            "content": hit.content,
                            "score": hit.score,
                            "project": hit.project,
                            "provenance": {
                                "agent": hit.provenance.agent,
                                "session_id": hit.provenance.session_id,
                            },
                            "created_at_micros": hit.provenance.created_at_micros,
                        })
                    })
                    .collect();
                let mut result = json!({ "hits": hits, "scope": applied_scope });
                // S9 edge: a file with no full-text index (pre-M2) still
                // recalls, vector-only — a warning field, never an error, and
                // absent entirely on a healthy file so existing clients see
                // an unchanged response.
                if outcome.degraded_to_vector_only {
                    result["warning"] = json!(
                        "this memory file has no full-text index (written by an \
                         older EmbedMind); results are vector-only. Run \
                         `embedmind vacuum` to build it"
                    );
                }
                Ok(result)
            }
            Err(e) => Err(e.to_string()),
        })
    }

    /// `related(id | entity)` — graph navigation (S13, `docs/adr/0012`).
    /// Exactly one of the two arguments:
    ///
    /// - `id`: the memory's entity tags plus its relation neighbors, both
    ///   directions, each carrying the relation kind and direction. An
    ///   unknown or forgotten id is a tool error.
    /// - `entity`: every live memory tagged with that entity, in time order.
    ///
    /// Tombstoned neighbors/members are never returned — a relation to a
    /// forgotten memory disappears with the tombstone.
    #[allow(clippy::type_complexity)]
    fn tool_related(&mut self, args: &Value) -> Result<Result<Value, String>, (i64, String)> {
        match (args.get("id"), args.get("entity")) {
            (Some(_), Some(_)) | (None, None) => Err((
                INVALID_PARAMS,
                "related: exactly one of 'id' or 'entity' is required".to_string(),
            )),
            (Some(id), None) => {
                let id = id
                    .as_str()
                    .ok_or((INVALID_PARAMS, "related: 'id' must be a string".to_string()))?;
                let Ok(id) = Ulid::from_string(id) else {
                    return Err((
                        INVALID_PARAMS,
                        "related: 'id' is not a valid ULID".to_string(),
                    ));
                };
                Ok(self.related_by_id(id))
            }
            (None, Some(entity)) => {
                let entity = entity.as_str().ok_or((
                    INVALID_PARAMS,
                    "related: 'entity' must be a string".to_string(),
                ))?;
                Ok(self.related_by_entity(entity))
            }
        }
    }

    /// The `related(id)` half: entity tags + relation neighbors of one memory.
    fn related_by_id(&self, id: Ulid) -> Result<Value, String> {
        if self.store.get(id).map_err(|e| e.to_string())?.is_none() {
            return Err(format!("no live memory with id {id}"));
        }
        let entities = self.store.entities_of(id).map_err(|e| e.to_string())?;
        let related: Vec<Value> = self
            .store
            .related(id)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|rel| {
                json!({
                    "id": rel.id.to_string(),
                    "kind": rel.kind,
                    "outgoing": rel.outgoing,
                    "content": rel.content,
                    "project": rel.project,
                    "provenance": {
                        "agent": rel.provenance.agent,
                        "session_id": rel.provenance.session_id,
                    },
                    "created_at_micros": rel.provenance.created_at_micros,
                })
            })
            .collect();
        Ok(json!({ "id": id.to_string(), "entities": entities, "related": related }))
    }

    /// The `related(entity)` half: live memories tagged with the entity.
    fn related_by_entity(&self, entity: &str) -> Result<Value, String> {
        let members: Vec<Value> = self
            .store
            .entity_members(entity)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|memory| {
                json!({
                    "id": memory.id.to_string(),
                    "content": memory.content,
                    "project": memory.project,
                    "provenance": {
                        "agent": memory.provenance.agent,
                        "session_id": memory.provenance.session_id,
                    },
                    "created_at_micros": memory.provenance.created_at_micros,
                })
            })
            .collect();
        Ok(json!({ "entity": entity, "members": members }))
    }

    /// `stats()` → live/forgotten counts plus a per-agent breakdown of live
    /// memories (S14 basic provenance). Read-only; takes no arguments.
    #[allow(clippy::type_complexity)]
    fn tool_stats(&mut self, _args: &Value) -> Result<Result<Value, String>, (i64, String)> {
        Ok(match self.store.stats() {
            Ok(stats) => {
                let by_agent: Vec<Value> = stats
                    .by_agent
                    .iter()
                    .map(|(agent, s)| {
                        json!({
                            "agent": agent,
                            "live_memories": s.live_memories,
                            "sessions": s.sessions.iter().collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "live_memories": stats.live_memories,
                    "forgotten_memories": stats.forgotten_memories,
                    "by_agent": by_agent,
                }))
            }
            Err(e) => Err(e.to_string()),
        })
    }

    /// `forget(id)` → `{count}` (0 = unknown or already forgotten). Bulk
    /// forget-by-query (with mandatory confirm) is deferred until the
    /// engine grows query-addressed deletion (DESIGN §8).
    #[allow(clippy::type_complexity)]
    fn tool_forget(&mut self, args: &Value) -> Result<Result<Value, String>, (i64, String)> {
        let id = args.get("id").and_then(Value::as_str).ok_or((
            INVALID_PARAMS,
            "forget: 'id' (string) is required".to_string(),
        ))?;
        let Ok(id) = Ulid::from_string(id) else {
            return Err((
                INVALID_PARAMS,
                "forget: 'id' is not a valid ULID".to_string(),
            ));
        };
        Ok(match self.store.forget(id) {
            Ok(forgotten) => Ok(json!({ "count": u8::from(forgotten) })),
            Err(e) => Err(e.to_string()),
        })
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Maps a JSON metadata value to the engine's typed scalar; arrays and
/// objects have no scalar representation and are rejected.
fn json_to_scalar(value: &Value) -> Option<Scalar> {
    match value {
        Value::Null => Some(Scalar::Null),
        Value::Bool(b) => Some(Scalar::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Scalar::I64(i))
            } else {
                n.as_f64().map(Scalar::F64)
            }
        }
        Value::String(s) => Some(Scalar::Str(s.clone())),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Maps one JSON filter spec to the engine's [`Filter`] (S10). A bare scalar
/// (string/number/bool/null) is an equality filter; an object carrying `min`
/// and/or `max` numeric bounds is a range. Anything else (an array, an object
/// with no bounds or non-numeric bounds) has no filter representation and is
/// rejected as a protocol error — the shell parses, it does not interpret.
fn json_to_filter(spec: &Value) -> Option<Filter> {
    match spec {
        Value::Object(map) => {
            // A range object: at least one of min/max, both numeric if present.
            // An empty object or unknown keys are rejected (None) so a typo
            // never silently becomes a match-everything filter.
            let bound = |name: &str| -> Option<Option<f64>> {
                match map.get(name) {
                    None => Some(None),
                    Some(v) => v.as_f64().map(Some),
                }
            };
            let min = bound("min")?;
            let max = bound("max")?;
            if min.is_none() && max.is_none() {
                return None; // {} or only unrecognized keys — not a range
            }
            // Reject stray keys beyond min/max so malformed specs fail loudly.
            if map.keys().any(|k| k != "min" && k != "max") {
                return None;
            }
            Some(Filter::Range { min, max })
        }
        // Bare scalar ⇒ equality. Arrays have no scalar/range meaning.
        Value::Array(_) => None,
        scalar => json_to_scalar(scalar).map(Filter::Eq),
    }
}

/// The `tools/list` response: five stable schemas — they are public API
/// (DESIGN §8) and must only change with versioning.
fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "remember",
                "description": "Store one memory persistently in the local memory file. \
                                Returns the memory's id. Memories are scoped to the \
                                current project automatically; pass project: null to \
                                store a global memory. Optionally tag entities and \
                                relate the memory to existing ones; navigate the graph \
                                later with the related tool.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The memory text to store."
                        },
                        "project": {
                            "type": ["string", "null"],
                            "description": "Project scope. Omit to use the detected \
                                            project context; null forces a global memory."
                        },
                        "metadata": {
                            "type": "object",
                            "description": "Optional metadata; values must be \
                                            string, number, boolean or null."
                        },
                        "entities": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional explicit entity tags (\"postgres\", \
                                            \"auth-service\", ...). Never extracted \
                                            automatically — tag what matters. Query \
                                            back with related({entity})."
                        },
                        "relations": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "description": "Relation type, e.g. \"refines\", \
                                                        \"contradicts\", \"depends-on\"."
                                    },
                                    "target": {
                                        "type": "string",
                                        "description": "Id of an existing, live memory \
                                                        this one relates to."
                                    }
                                },
                                "required": ["kind", "target"]
                            },
                            "description": "Optional typed relations from this memory to \
                                            existing ones. A target that does not exist \
                                            (or was forgotten) fails the whole remember. \
                                            Navigate back with related({id})."
                        },
                        "supersedes": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional ids of memories this one replaces \
                                            (versioned knowledge): each target disappears \
                                            from every later recall but stays readable as \
                                            history via related({id}). Targets must exist, \
                                            be live, and belong to the same project — \
                                            otherwise the whole remember fails."
                        }
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "recall",
                "description": "Semantic search over remembered content. Returns the \
                                closest memories, best match first, with similarity \
                                scores. Searches the current project by default; pass \
                                scope: \"all\" to search every project.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "What to search for."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum memories to return.",
                            "default": 8
                        },
                        "project": {
                            "type": "string",
                            "description": "Search one specific project instead of \
                                            the detected one."
                        },
                        "scope": {
                            "type": "string",
                            "enum": ["project", "all"],
                            "description": "\"project\" (default) = the current \
                                            project's memories; \"all\" = everything.",
                            "default": "project"
                        },
                        "filters": {
                            "type": "object",
                            "description": "Optional metadata filters, all ANDed. \
                                            Each key maps to either an exact value \
                                            (string/number/boolean/null) or a numeric \
                                            range object {\"min\": n, \"max\": n} (either \
                                            bound may be omitted). A memory is returned \
                                            only if it satisfies every filter; a filter \
                                            on a key a memory lacks excludes it.",
                            "additionalProperties": {
                                "oneOf": [
                                    { "type": ["string", "number", "boolean", "null"] },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "min": { "type": "number" },
                                            "max": { "type": "number" }
                                        }
                                    }
                                ]
                            }
                        },
                        "agent": {
                            "type": "string",
                            "description": "Return only memories written by this agent \
                                            (basic provenance). See the stats tool for \
                                            the list of agents that have memories."
                        },
                        "expand_related": {
                            "type": "boolean",
                            "description": "When true, each hit's explicitly related \
                                            memories (1 hop, both directions) are \
                                            appended as connected context with score 0, \
                                            after the ranked hits and without counting \
                                            against limit.",
                            "default": false
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "related",
                "description": "Navigate the explicit memory graph. Pass id to get one \
                                memory's entity tags and its related memories (both \
                                directions, with the relation kind); or pass entity to \
                                list every memory tagged with that entity. Exactly one \
                                of the two.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Memory id whose relations to list."
                        },
                        "entity": {
                            "type": "string",
                            "description": "Entity tag whose memories to list."
                        }
                    }
                }
            },
            {
                "name": "stats",
                "description": "Report memory-file counts: live and forgotten memories, \
                                and a breakdown of live memories by the agent that wrote \
                                them (with the distinct sessions per agent). Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "forget",
                "description": "Delete one memory by id. Returns how many were deleted \
                                (0 if the id is unknown or already forgotten).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "The memory id, as returned by remember/recall."
                        }
                    },
                    "required": ["id"]
                }
            }
        ]
    })
}
