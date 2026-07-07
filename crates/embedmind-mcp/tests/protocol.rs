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

/// Feeds `requests` (one JSON value per line) through the server loop and
/// returns the responses in order.
fn roundtrip(store: Store, requests: &[Value]) -> Vec<Value> {
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let mut output = Vec::new();
    McpServer::new(store)
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
fn tools_list_exposes_the_three_stable_tools() {
    let responses = roundtrip(
        kv_store(),
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })],
    );
    let tools = responses[0]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["remember", "recall", "forget"]);
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
    McpServer::new(kv_store())
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
    let mut server = McpServer::new(store);
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
