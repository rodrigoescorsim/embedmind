//! # embedmind-mcp
//!
//! MCP memory server: stdio JSON-RPC exposing `remember` / `recall` /
//! `forget` over one local `.mind` file (M1 item 1.4), with automatic
//! project-context scoping inferred from the agent's working directory
//! (M1 item 1.5, `project` module).
//!
//! The protocol is implemented directly — no SDK, no tokio (`docs/adr/0009`):
//! the subset a tools-only server needs (`initialize`, `ping`, `tools/list`,
//! `tools/call`) is a synchronous read-dispatch-write loop over
//! newline-delimited JSON-RPC 2.0 messages.
//!
//! Architectural rule (CLAUDE.md decision #2): this crate contains ZERO
//! domain logic — parse request → call `embedmind_core::api` → serialize
//! response. Replacing MCP with another protocol must stay a ~300-line job.

pub mod oplog;
pub mod project;
pub mod report;
pub mod server;

pub use oplog::OpLog;
pub use project::detect_project;
pub use report::{UsageReport, aggregate};
pub use server::McpServer;
