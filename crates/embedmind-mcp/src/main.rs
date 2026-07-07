//! MCP server shell: stdio JSON-RPC exposing `remember` / `recall` / `forget`
//! plus automatic project-context scoping (M1 items 1.4–1.5).
//!
//! Architectural rule (CLAUDE.md decision #2): this crate contains ZERO domain
//! logic — parse request → call `embedmind_core::api` → serialize response.
//! Replacing MCP with another protocol must stay a ~300-line job.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "embedmind-mcp {}: not implemented yet (M1 item 1.4 — blocked on the \
         rmcp-vs-direct decision, DESIGN.md §12)",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::FAILURE
}
