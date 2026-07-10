//! MCP protocol integration tests: drive [`McpServer::serve`] with in-memory
//! pipes (the same loop the stdio binary runs) and assert on the JSON-RPC
//! responses. No subprocess, no real filesystem — the store sits on `SimVfs`.
//!
//! One test (`recall_returns_scored_hits`) uses the real embedded ONNX model
//! to prove the full remember→recall path through the protocol; the rest run
//! embedder-free for speed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use embedmind_core::embed::OnnxEmbedder;
use embedmind_core::storage::sim::SimVfs;
use embedmind_core::storage::vfs::Vfs;
use embedmind_core::{Store, StoreOptions};
use embedmind_mcp::McpServer;
use serde_json::{Value, json};

/// A KV-only store (no embedder): fast, enough for everything but recall.
fn kv_store() -> Store {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    Store::create_with(vfs, Path::new("m.mind"), StoreOptions::default()).unwrap()
}

/// A store with the real embedded model, for the end-to-end recall test.
fn embedding_store() -> Store {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let opts = StoreOptions {
        embedder: Some(Arc::new(OnnxEmbedder::load().expect("model must load"))),
        ..StoreOptions::default()
    };
    Store::create_with(vfs, Path::new("m.mind"), opts).unwrap()
}

/// A store on a `.mind` rewound to the pre-M2 shape (no full-text index):
/// content is remembered normally, then the header's fts root pointer is
/// dropped through the pager — exactly what an old file presents on open.
/// For the S9 graceful-degradation edge.
fn legacy_embedding_store(content: &str) -> Store {
    use embedmind_core::MemoryDraft;
    use embedmind_core::storage::{Pager, PagerOptions};

    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let opts = StoreOptions {
        embedder: Some(Arc::new(OnnxEmbedder::load().expect("model must load"))),
        ..StoreOptions::default()
    };
    let mut store =
        Store::create_with(Arc::clone(&vfs), Path::new("m.mind"), opts.clone()).unwrap();
    store.remember(MemoryDraft::new(content)).unwrap();
    store.close().unwrap();

    let mut pager = Pager::open(
        Arc::clone(&vfs),
        Path::new("m.mind"),
        PagerOptions::default(),
    )
    .unwrap();
    let mut txn = pager.begin().unwrap();
    txn.set_fts_root_page(0);
    txn.commit().unwrap();
    pager.close().unwrap();

    Store::open_with(vfs, Path::new("m.mind"), opts).unwrap()
}

/// Feeds `requests` (one JSON value per line) through the server loop and
/// returns the responses in order. No project context.
fn roundtrip(store: Store, requests: &[Value]) -> Vec<Value> {
    roundtrip_in_project(store, None, requests)
}

/// [`roundtrip`] with a detected project context (M1 item 1.5).
fn roundtrip_in_project(store: Store, project: Option<&str>, requests: &[Value]) -> Vec<Value> {
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let mut output = Vec::new();
    McpServer::new(store, project.map(str::to_string))
        .serve(input.as_bytes(), &mut output)
        .unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test-agent", "version": "0.0.0" },
        },
    })
}

fn call(id: u64, tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
}

#[test]
fn initialize_handshake_and_ping() {
    let responses = roundtrip(
        kv_store(),
        &[
            initialize_request(1),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
        ],
    );
    // The notification produces no response: exactly two lines out.
    assert_eq!(responses.len(), 2);
    let init = &responses[0];
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "embedmind");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert_eq!(responses[1]["id"], 2);
    assert!(responses[1]["result"].is_object());
}

#[test]
fn unsupported_protocol_version_gets_the_latest() {
    let responses = roundtrip(
        kv_store(),
        &[json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" },
        })],
    );
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
}

#[test]
fn tools_list_exposes_the_stable_tools() {
    let responses = roundtrip(
        kv_store(),
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
    );
    let tools = responses[0]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["remember", "recall", "related", "stats", "forget"]);
    for tool in tools {
        assert!(tool["inputSchema"]["type"] == "object");
        assert!(tool["description"].as_str().unwrap().len() > 10);
    }
}

#[test]
fn remember_then_forget_roundtrip_with_provenance() {
    let responses = roundtrip(
        kv_store(),
        &[
            initialize_request(1),
            call(
                2,
                "remember",
                json!({
                    "content": "the deploy script lives in scripts/deploy.ps1",
                    "project": "embedmind",
                    "metadata": { "topic": "ops", "priority": 2, "reviewed": false },
                }),
            ),
            call(3, "forget", json!({ "id": "not-a-ulid" })),
        ],
    );
    let structured = &responses[1]["result"]["structuredContent"];
    let id = structured["id"].as_str().unwrap();
    assert_eq!(id.len(), 26, "remember must return a ULID");
    assert_ne!(
        responses[1]["result"]
            .get("isError")
            .and_then(Value::as_bool),
        Some(true)
    );
    // Malformed id is a protocol error (invalid params), not a tool error.
    assert_eq!(responses[2]["error"]["code"], -32602);
}

#[test]
fn engine_failure_is_a_tool_error_not_a_crash() {
    // recall on a KV-only store is a typed engine error → isError: true,
    // and the server keeps serving afterwards.
    let responses = roundtrip(
        kv_store(),
        &[
            call(1, "recall", json!({ "query": "anything" })),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
        ],
    );
    assert_eq!(responses[0]["result"]["isError"], true);
    let text = responses[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        text.contains("embedder"),
        "error text should explain: {text}"
    );
    assert!(
        responses[1]["result"].is_object(),
        "server must keep serving"
    );
}

#[test]
fn protocol_errors_are_typed_json_rpc_errors() {
    let mut output = Vec::new();
    let input = "this is not json\n\
                 {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"no/such/method\"}\n\
                 {\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"nope\"}}\n\
                 {\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"remember\",\"arguments\":{}}}\n";
    McpServer::new(kv_store(), None)
        .serve(input.as_bytes(), &mut output)
        .unwrap();
    let responses: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses[0]["error"]["code"], -32700, "malformed JSON");
    assert_eq!(responses[1]["error"]["code"], -32601, "unknown method");
    assert_eq!(responses[2]["error"]["code"], -32602, "unknown tool");
    assert_eq!(responses[3]["error"]["code"], -32602, "missing content");
}

#[test]
fn recall_returns_scored_hits_with_provenance() {
    let responses = roundtrip(
        embedding_store(),
        &[
            initialize_request(1),
            call(
                2,
                "remember",
                json!({ "content": "the cat sat on the warm mat" }),
            ),
            call(
                3,
                "remember",
                json!({ "content": "quarterly tax filing deadline" }),
            ),
            call(
                4,
                "recall",
                json!({ "query": "a feline resting", "limit": 2 }),
            ),
        ],
    );
    let cat_id = responses[1]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let hits = responses[3]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(
        hits[0]["id"].as_str().unwrap(),
        cat_id,
        "cat memory must rank first for a feline query"
    );
    let first = hits[0]["score"].as_f64().unwrap();
    let last = hits[hits.len() - 1]["score"].as_f64().unwrap();
    assert!(first >= last, "hits must come best-first");
    assert_eq!(
        hits[0]["provenance"]["agent"], "test-agent",
        "clientInfo.name from initialize must be recorded as provenance"
    );
    assert!(
        responses[3]["result"]["structuredContent"]
            .get("warning")
            .is_none(),
        "a healthy file must not carry a degradation warning"
    );
}

/// S9 edge over the protocol: `recall` against a `.mind` with no full-text
/// index (a pre-M2 file) returns vector-only hits plus a `warning` field —
/// never a tool error, and the response shape is otherwise unchanged.
#[test]
fn recall_on_legacy_file_without_fts_index_returns_hits_with_warning() {
    let responses = roundtrip(
        legacy_embedding_store("the kitten sleeps on the rug"),
        &[
            initialize_request(1),
            call(2, "recall", json!({ "query": "a small feline resting" })),
        ],
    );
    let result = &responses[1]["result"];
    assert!(
        result.get("isError").is_none(),
        "degradation must never be a tool error: {result}"
    );
    let content = &result["structuredContent"];
    let hits = content["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty(),
        "vector similarity must still return the memory: {content}"
    );
    assert!(hits[0]["content"].as_str().unwrap().contains("kitten"));
    let warning = content["warning"].as_str().unwrap();
    assert!(
        warning.contains("no full-text index"),
        "the warning must say what degraded: {warning}"
    );
}

/// M1 item 1.5 (DESIGN §7): with a detected project context, `remember`
/// stamps the project automatically and `recall` scopes to it by default,
/// with `scope: "all"` as the explicit global fallback and `project: null`
/// forcing a global memory.
#[test]
fn project_context_scopes_remember_and_recall_automatically() {
    let responses = roundtrip_in_project(
        embedding_store(),
        Some("alpha"),
        &[
            // Auto-scoped to alpha (no project argument).
            call(
                1,
                "remember",
                json!({ "content": "uses tokio for async runtime work" }),
            ),
            // Explicitly global (project: null).
            call(
                2,
                "remember",
                json!({ "content": "the async runtime notes apply everywhere", "project": null }),
            ),
            // Explicitly another project.
            call(
                3,
                "remember",
                json!({ "content": "async runtime decisions for the beta service", "project": "beta" }),
            ),
            // Default recall: only alpha's memory.
            call(
                4,
                "recall",
                json!({ "query": "async runtime", "limit": 10 }),
            ),
            // Explicit global fallback: all three.
            call(
                5,
                "recall",
                json!({ "query": "async runtime", "limit": 10, "scope": "all" }),
            ),
            // Targeting another project explicitly.
            call(
                6,
                "recall",
                json!({ "query": "async runtime", "limit": 10, "project": "beta" }),
            ),
        ],
    );

    assert_eq!(
        responses[0]["result"]["structuredContent"]["project"], "alpha",
        "remember must stamp the detected project"
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["project"],
        Value::Null,
        "project: null must force a global memory"
    );

    let scoped = &responses[3]["result"]["structuredContent"];
    assert_eq!(scoped["scope"], "alpha");
    let hits = scoped["hits"].as_array().unwrap();
    assert_eq!(
        hits.len(),
        1,
        "default recall must see only the project's memories"
    );
    assert_eq!(hits[0]["project"], "alpha");

    let global = &responses[4]["result"]["structuredContent"];
    assert_eq!(global["scope"], "all");
    assert_eq!(global["hits"].as_array().unwrap().len(), 3);

    let beta = &responses[5]["result"]["structuredContent"];
    assert_eq!(beta["scope"], "beta");
    let hits = beta["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["project"], "beta");
}

#[test]
fn without_project_context_recall_defaults_to_everything() {
    let responses = roundtrip(
        embedding_store(),
        &[
            call(
                1,
                "remember",
                json!({ "content": "note scoped to alpha", "project": "alpha" }),
            ),
            call(2, "remember", json!({ "content": "a global note" })),
            call(3, "recall", json!({ "query": "note", "limit": 10 })),
        ],
    );
    assert_eq!(
        responses[1]["result"]["structuredContent"]["project"],
        Value::Null,
        "no context and no argument = global memory"
    );
    let result = &responses[2]["result"]["structuredContent"];
    assert_eq!(result["scope"], "all");
    assert_eq!(result["hits"].as_array().unwrap().len(), 2);
}

/// S10: the `recall` tool accepts an optional `filters` object — exact-value
/// and numeric-range filters, ANDed — and returns only matching memories. The
/// schema addition is backward compatible: the earlier tests that never send
/// `filters` still pass, and `tools/list` still advertises the same three
/// tools.
#[test]
fn recall_filters_by_metadata_through_the_protocol() {
    let responses = roundtrip(
        embedding_store(),
        &[
            initialize_request(1),
            call(
                2,
                "remember",
                json!({
                    "content": "deploy runbook for the release",
                    "metadata": { "topic": "ops", "priority": 9 },
                }),
            ),
            call(
                3,
                "remember",
                json!({
                    "content": "design notes for the release",
                    "metadata": { "topic": "design", "priority": 2 },
                }),
            ),
            // Exact-value filter: only the ops memory.
            call(
                4,
                "recall",
                json!({ "query": "release", "scope": "all", "filters": { "topic": "ops" } }),
            ),
            // Numeric range: priority >= 5, still only the ops memory.
            call(
                5,
                "recall",
                json!({
                    "query": "release", "scope": "all",
                    "filters": { "priority": { "min": 5 } },
                }),
            ),
            // Two filters ANDed, one of which excludes everything ⇒ no hits.
            call(
                6,
                "recall",
                json!({
                    "query": "release", "scope": "all",
                    "filters": { "topic": "ops", "priority": { "max": 1 } },
                }),
            ),
        ],
    );
    let ops_id = responses[1]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let by_value = responses[3]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert_eq!(by_value.len(), 1, "topic=ops must keep exactly one memory");
    assert_eq!(by_value[0]["id"], ops_id);

    let by_range = responses[4]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert_eq!(
        by_range.len(),
        1,
        "priority>=5 must keep exactly one memory"
    );
    assert_eq!(by_range[0]["id"], ops_id);

    let anded = responses[5]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert!(anded.is_empty(), "AND of disjoint filters yields no hits");
}

/// S10 edges through the protocol: a filter on a key no memory has returns
/// zero hits (not an error), while a type-incompatible filter is surfaced as
/// a tool error (`isError: true`), not a crash.
#[test]
fn recall_filter_edges_absent_key_and_type_mismatch() {
    let responses = roundtrip(
        embedding_store(),
        &[
            call(
                1,
                "remember",
                json!({ "content": "a note", "metadata": { "topic": "ops" } }),
            ),
            // Absent key ⇒ 0 hits, still a normal (non-error) result.
            call(
                2,
                "recall",
                json!({ "query": "note", "scope": "all", "filters": { "missing": "x" } }),
            ),
            // Type mismatch: integer filter over a stored string ⇒ tool error.
            call(
                3,
                "recall",
                json!({ "query": "note", "scope": "all", "filters": { "topic": 3 } }),
            ),
        ],
    );
    let absent = &responses[1]["result"];
    assert_ne!(absent.get("isError").and_then(Value::as_bool), Some(true));
    assert!(
        absent["structuredContent"]["hits"]
            .as_array()
            .unwrap()
            .is_empty(),
        "absent-key filter must yield 0 hits, not an error"
    );
    assert_eq!(
        responses[2]["result"]["isError"], true,
        "type-incompatible filter must be a tool error"
    );
}

/// A malformed `filters` argument (not an object, or a filter that is neither
/// a scalar nor a valid range) is a protocol error (`-32602`), caught before
/// the engine runs.
#[test]
fn malformed_filters_argument_is_a_protocol_error() {
    let responses = roundtrip(
        kv_store(),
        &[
            call(1, "recall", json!({ "query": "x", "filters": [1, 2, 3] })),
            call(
                2,
                "recall",
                json!({ "query": "x", "filters": { "k": { "bogus": 1 } } }),
            ),
        ],
    );
    assert_eq!(
        responses[0]["error"]["code"], -32602,
        "filters must be an object"
    );
    assert_eq!(
        responses[1]["error"]["code"], -32602,
        "a range object needs min/max, not arbitrary keys"
    );
}

/// S14: `recall` accepts an optional `agent` filter and returns only memories
/// written by that agent. The writing agent is the `clientInfo.name` from
/// `initialize`, so this test drives two servers with different client names
/// against the same store, then recalls filtered by one of them.
#[test]
fn recall_filters_by_agent_through_the_protocol() {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let opts = StoreOptions {
        embedder: Some(Arc::new(OnnxEmbedder::load().expect("model must load"))),
        ..StoreOptions::default()
    };
    let store = Store::create_with(vfs, Path::new("m.mind"), opts).unwrap();
    let mut server = McpServer::new(store, None);

    // Agent "cli" remembers one; agent "claude-code" remembers another.
    let feed = |server: &mut McpServer, reqs: &[Value]| -> Vec<Value> {
        let input: String = reqs.iter().map(|r| format!("{r}\n")).collect();
        let mut out = Vec::new();
        server.serve(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };

    let r1 = feed(
        &mut server,
        &[
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "clientInfo": { "name": "cli", "version": "0" } },
            }),
            call(
                2,
                "remember",
                json!({ "content": "the cat sat on the mat" }),
            ),
        ],
    );
    let cli_id = r1[1]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let r2 = feed(
        &mut server,
        &[
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "clientInfo": { "name": "claude-code", "version": "0" } },
            }),
            call(
                2,
                "remember",
                json!({ "content": "a feline naps on the rug" }),
            ),
        ],
    );
    let claude_id = r2[1]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Recall filtered to agent "cli": only that agent's memory.
    let r3 = feed(
        &mut server,
        &[call(
            1,
            "recall",
            json!({ "query": "a resting cat", "scope": "all", "agent": "cli" }),
        )],
    );
    let hits = r3[0]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert_eq!(hits.len(), 1, "agent filter keeps exactly one memory");
    assert_eq!(hits[0]["id"], cli_id);
    assert_eq!(hits[0]["provenance"]["agent"], "cli");
    assert_ne!(hits[0]["id"], Value::String(claude_id));
}

/// S14: the `stats` tool reports live/forgotten counts and a per-agent
/// breakdown of live memories, all through the protocol.
#[test]
fn stats_tool_reports_provenance_breakdown() {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let store = Store::create_with(vfs, Path::new("m.mind"), StoreOptions::default()).unwrap();
    let mut server = McpServer::new(store, None);

    let feed = |server: &mut McpServer, reqs: &[Value]| -> Vec<Value> {
        let input: String = reqs.iter().map(|r| format!("{r}\n")).collect();
        let mut out = Vec::new();
        server.serve(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };

    // "cli" writes two memories.
    feed(
        &mut server,
        &[
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "clientInfo": { "name": "cli", "version": "0" } },
            }),
            call(2, "remember", json!({ "content": "one" })),
            call(3, "remember", json!({ "content": "two" })),
        ],
    );
    // "claude-code" writes one, then forgets it.
    let r = feed(
        &mut server,
        &[
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "clientInfo": { "name": "claude-code", "version": "0" } },
            }),
            call(2, "remember", json!({ "content": "three" })),
        ],
    );
    let doomed = r[1]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    feed(&mut server, &[call(3, "forget", json!({ "id": doomed }))]);

    let stats = feed(&mut server, &[call(1, "stats", json!({}))]);
    let content = &stats[0]["result"]["structuredContent"];
    assert_eq!(content["live_memories"], 2);
    assert_eq!(content["forgotten_memories"], 1);
    let by_agent = content["by_agent"].as_array().unwrap();
    // Only "cli" has live memories; the forgotten claude-code memory drops out.
    assert_eq!(by_agent.len(), 1);
    assert_eq!(by_agent[0]["agent"], "cli");
    assert_eq!(by_agent[0]["live_memories"], 2);
}

/// S13 through the protocol: `remember` accepts explicit `entities` and
/// `relations`, and the `related` tool navigates them — by id (both
/// directions, with kind) and by entity. Forgetting a neighbor makes its
/// relation disappear with the tombstone, per the story's edge case.
#[test]
fn graph_remember_related_and_tombstone_through_the_protocol() {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let store = Store::create_with(vfs, Path::new("m.mind"), StoreOptions::default()).unwrap();
    let mut server = McpServer::new(store, None);

    let feed = |server: &mut McpServer, reqs: &[Value]| -> Vec<Value> {
        let input: String = reqs.iter().map(|r| format!("{r}\n")).collect();
        let mut out = Vec::new();
        server.serve(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };

    // Memory A, then B refining A and tagged with an entity.
    let r = feed(
        &mut server,
        &[call(
            1,
            "remember",
            json!({ "content": "we chose postgres for storage" }),
        )],
    );
    let a_id = r[0]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = feed(
        &mut server,
        &[call(
            2,
            "remember",
            json!({
                "content": "specifically postgres 16 with pgvector",
                "entities": ["postgres"],
                "relations": [{ "kind": "refines", "target": a_id }],
            }),
        )],
    );
    let structured = &r[0]["result"]["structuredContent"];
    let b_id = structured["id"].as_str().unwrap().to_string();
    assert_eq!(structured["entities"], json!(["postgres"]));
    assert_eq!(structured["relations"][0]["kind"], "refines");
    assert_eq!(structured["relations"][0]["target"], a_id);

    // related(B): outgoing "refines" edge to A, plus B's entity tags.
    let r = feed(&mut server, &[call(3, "related", json!({ "id": b_id }))]);
    let by_id = &r[0]["result"]["structuredContent"];
    assert_eq!(by_id["entities"], json!(["postgres"]));
    let neighbors = by_id["related"].as_array().unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0]["id"], a_id);
    assert_eq!(neighbors[0]["kind"], "refines");
    assert_eq!(neighbors[0]["outgoing"], true);

    // related(A): the same edge, incoming.
    let r = feed(&mut server, &[call(4, "related", json!({ "id": a_id }))]);
    let neighbors = r[0]["result"]["structuredContent"]["related"]
        .as_array()
        .unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0]["id"], b_id);
    assert_eq!(neighbors[0]["outgoing"], false);

    // related(entity): B is the only member of "postgres".
    let r = feed(
        &mut server,
        &[call(5, "related", json!({ "entity": "postgres" }))],
    );
    let by_entity = &r[0]["result"]["structuredContent"];
    assert_eq!(by_entity["entity"], "postgres");
    let members = by_entity["members"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["id"], b_id);

    // Forget A: the relation disappears with the tombstone, and related(A)
    // itself becomes a tool error (no live memory).
    feed(&mut server, &[call(6, "forget", json!({ "id": a_id }))]);
    let r = feed(&mut server, &[call(7, "related", json!({ "id": b_id }))]);
    assert!(
        r[0]["result"]["structuredContent"]["related"]
            .as_array()
            .unwrap()
            .is_empty(),
        "relation to a forgotten memory must disappear with the tombstone"
    );
    let r = feed(&mut server, &[call(8, "related", json!({ "id": a_id }))]);
    assert_eq!(r[0]["result"]["isError"], true);
}

/// S13 argument edges: a relation to a nonexistent target is an engine
/// failure (tool error, nothing stored); malformed graph arguments and a
/// `related` call with neither/both selectors are protocol errors.
#[test]
fn graph_argument_edges_through_the_protocol() {
    let ghost = "01ARZ3NDEKTSV4RRFFQ69G5FAV"; // valid ULID, never stored
    let responses = roundtrip(
        kv_store(),
        &[
            call(
                1,
                "remember",
                json!({
                    "content": "points at a ghost",
                    "relations": [{ "kind": "refines", "target": ghost }],
                }),
            ),
            call(
                2,
                "remember",
                json!({ "content": "bad relations", "relations": "not-an-array" }),
            ),
            call(
                3,
                "remember",
                json!({ "content": "bad target", "relations": [{ "kind": "refines", "target": "not-a-ulid" }] }),
            ),
            call(
                4,
                "remember",
                json!({ "content": "bad entities", "entities": [1, 2] }),
            ),
            call(5, "related", json!({})),
            call(6, "related", json!({ "id": ghost, "entity": "postgres" })),
            call(7, "related", json!({ "id": "not-a-ulid" })),
            call(8, "related", json!({ "id": ghost })),
            call(9, "related", json!({ "entity": "nobody-tagged-this" })),
        ],
    );
    assert_eq!(
        responses[0]["result"]["isError"], true,
        "relation to a nonexistent target is a tool error"
    );
    assert_eq!(responses[1]["error"]["code"], -32602);
    assert_eq!(responses[2]["error"]["code"], -32602);
    assert_eq!(responses[3]["error"]["code"], -32602);
    assert_eq!(
        responses[4]["error"]["code"], -32602,
        "neither id nor entity"
    );
    assert_eq!(responses[5]["error"]["code"], -32602, "both id and entity");
    assert_eq!(responses[6]["error"]["code"], -32602, "malformed id");
    assert_eq!(
        responses[7]["result"]["isError"], true,
        "unknown id is a tool error, not a crash"
    );
    // An entity nobody used is an empty member list, not an error.
    let members = responses[8]["result"]["structuredContent"]["members"]
        .as_array()
        .unwrap();
    assert!(members.is_empty());
}

/// S13: `recall` with `expand_related: true` appends each hit's related
/// memories as connected context with score 0, after the ranked hits.
#[test]
fn recall_expand_related_pulls_connected_context() {
    // Relating B to A needs A's id from an earlier response, so this test
    // feeds the server in stages instead of one `roundtrip` batch.
    let mut server = McpServer::new(embedding_store(), None);
    let feed = |server: &mut McpServer, reqs: &[Value]| -> Vec<Value> {
        let input: String = reqs.iter().map(|r| format!("{r}\n")).collect();
        let mut out = Vec::new();
        server.serve(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let r = feed(
        &mut server,
        &[call(
            1,
            "remember",
            json!({ "content": "the cat sat on the warm mat" }),
        )],
    );
    let cat = r[0]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    feed(
        &mut server,
        &[call(
            2,
            "remember",
            json!({
                "content": "quarterly tax filing deadline is in april",
                "relations": [{ "kind": "mentioned-with", "target": cat }],
            }),
        )],
    );
    let r = feed(
        &mut server,
        &[
            call(
                3,
                "recall",
                json!({ "query": "a feline resting", "limit": 1 }),
            ),
            call(
                4,
                "recall",
                json!({ "query": "a feline resting", "limit": 1, "expand_related": true }),
            ),
        ],
    );
    let plain = r[0]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert_eq!(plain.len(), 1, "without expansion: only the ranked hit");
    assert_eq!(plain[0]["id"], cat);

    let expanded = r[1]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert_eq!(
        expanded.len(),
        2,
        "expansion appends the related memory beyond the limit"
    );
    assert_eq!(expanded[0]["id"], cat, "ranked hit stays first");
    assert!(
        expanded[1]["content"]
            .as_str()
            .unwrap()
            .contains("tax filing"),
        "the graph neighbor comes along as context"
    );
    assert_eq!(
        expanded[1]["score"].as_f64().unwrap(),
        0.0,
        "expanded hits carry score 0 — context, not a ranked match"
    );
}

#[test]
fn invalid_scope_is_a_protocol_error() {
    let responses = roundtrip(
        kv_store(),
        &[call(
            1,
            "recall",
            json!({ "query": "x", "scope": "everything" }),
        )],
    );
    assert_eq!(responses[0]["error"]["code"], -32602);
}

#[test]
fn forget_through_protocol_hides_memory_from_recall() {
    // remember → forget(id) → recall finds nothing of it.
    let store = embedding_store();
    let input_1 = format!(
        "{}\n",
        call(
            1,
            "remember",
            json!({ "content": "temporary secret note about the launch date" })
        )
    );
    let mut out_1 = Vec::new();
    let mut server = McpServer::new(store, None);
    server.serve(input_1.as_bytes(), &mut out_1).unwrap();
    let first: Value =
        serde_json::from_str(String::from_utf8(out_1).unwrap().lines().next().unwrap()).unwrap();
    let id = first["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let input_2 = format!(
        "{}\n{}\n",
        call(2, "forget", json!({ "id": id })),
        call(
            3,
            "recall",
            json!({ "query": "launch date note", "limit": 5 })
        ),
    );
    let mut out_2 = Vec::new();
    server.serve(input_2.as_bytes(), &mut out_2).unwrap();
    let responses: Vec<Value> = String::from_utf8(out_2)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses[0]["result"]["structuredContent"]["count"], 1);
    let hits = responses[1]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert!(
        hits.iter().all(|h| h["id"].as_str().unwrap() != id),
        "forgotten memory must not be recalled"
    );
}

/// S19 through the protocol: `remember` accepts `supersedes: [ids]`, echoes
/// them back, the version chain is navigable via `related` in both
/// directions, and argument/engine edges fail the right way (protocol error
/// vs. tool error, nothing stored).
#[test]
fn supersedes_remember_related_and_edges_through_the_protocol() {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let store = Store::create_with(vfs, Path::new("m.mind"), StoreOptions::default()).unwrap();
    let mut server = McpServer::new(store, Some("alpha".to_string()));

    let feed = |server: &mut McpServer, reqs: &[Value]| -> Vec<Value> {
        let input: String = reqs.iter().map(|r| format!("{r}\n")).collect();
        let mut out = Vec::new();
        server.serve(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };

    // A (in project "alpha"), then B superseding A.
    let r = feed(
        &mut server,
        &[call(1, "remember", json!({ "content": "fact, first version" }))],
    );
    let a_id = r[0]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = feed(
        &mut server,
        &[call(
            2,
            "remember",
            json!({ "content": "fact, corrected", "supersedes": [a_id] }),
        )],
    );
    let structured = &r[0]["result"]["structuredContent"];
    let b_id = structured["id"].as_str().unwrap().to_string();
    assert_eq!(structured["supersedes"], json!([a_id]));

    // The chain is navigable both ways with the "supersedes" kind.
    let r = feed(&mut server, &[call(3, "related", json!({ "id": b_id }))]);
    let neighbors = r[0]["result"]["structuredContent"]["related"]
        .as_array()
        .unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0]["id"], a_id);
    assert_eq!(neighbors[0]["kind"], "supersedes");
    assert_eq!(neighbors[0]["outgoing"], true);
    let r = feed(&mut server, &[call(4, "related", json!({ "id": a_id }))]);
    let neighbors = r[0]["result"]["structuredContent"]["related"]
        .as_array()
        .unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0]["id"], b_id);
    assert_eq!(neighbors[0]["outgoing"], false);

    // Edges. Malformed arguments are protocol errors; a ghost target or a
    // cross-project target is an engine failure (tool error, nothing stored).
    let ghost = "01ARZ3NDEKTSV4RRFFQ69G5FAV"; // valid ULID, never stored
    let r = feed(
        &mut server,
        &[
            call(
                5,
                "remember",
                json!({ "content": "bad shape", "supersedes": "not-an-array" }),
            ),
            call(
                6,
                "remember",
                json!({ "content": "bad id", "supersedes": ["not-a-ulid"] }),
            ),
            call(
                7,
                "remember",
                json!({ "content": "ghost target", "supersedes": [ghost] }),
            ),
            call(
                8,
                "remember",
                json!({ "content": "global cannot supersede scoped",
                        "project": null, "supersedes": [a_id] }),
            ),
        ],
    );
    assert_eq!(r[0]["error"]["code"], -32602);
    assert_eq!(r[1]["error"]["code"], -32602);
    assert_eq!(r[2]["result"]["isError"], true, "ghost target: tool error");
    assert_eq!(
        r[3]["result"]["isError"], true,
        "cross-project (global vs. alpha) target: tool error"
    );
}

/// S19 end to end with the real model: after `remember(supersedes: [A])`,
/// recall through the protocol returns only the new version — and forgetting
/// the new version does not resurrect the old one.
#[test]
fn supersedes_hides_old_version_from_recall_through_protocol() {
    let mut server = McpServer::new(embedding_store(), None);
    let feed = |server: &mut McpServer, reqs: &[Value]| -> Vec<Value> {
        let input: String = reqs.iter().map(|r| format!("{r}\n")).collect();
        let mut out = Vec::new();
        server.serve(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };

    let r = feed(
        &mut server,
        &[call(
            1,
            "remember",
            json!({ "content": "the launch date is august 4th" }),
        )],
    );
    let old_id = r[0]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = feed(
        &mut server,
        &[call(
            2,
            "remember",
            json!({ "content": "the launch date moved to august 11th",
                    "supersedes": [old_id] }),
        )],
    );
    let new_id = r[0]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = feed(
        &mut server,
        &[call(3, "recall", json!({ "query": "when is the launch date" }))],
    );
    let hits = r[0]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert!(
        hits.iter().any(|h| h["id"] == new_id.as_str()),
        "the new version must be recalled: {hits:?}"
    );
    assert!(
        hits.iter().all(|h| h["id"] != old_id.as_str()),
        "the superseded version must not be recalled: {hits:?}"
    );

    // Forgetting the superseder does not resurrect the superseded (its
    // exclusion is state on its own record, docs/adr/0013).
    let r = feed(
        &mut server,
        &[
            call(4, "forget", json!({ "id": new_id })),
            call(5, "recall", json!({ "query": "when is the launch date" })),
        ],
    );
    assert_eq!(r[0]["result"]["structuredContent"]["count"], 1);
    let hits = r[1]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert!(
        hits.iter().all(|h| h["id"] != old_id.as_str()),
        "forget of the new version must not resurrect the old: {hits:?}"
    );
}
