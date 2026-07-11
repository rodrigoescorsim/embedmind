//! Structured operation log (S22, FR4): one JSON line (JSONL) appended per
//! tool call, so an operator — the Agentic Panel tailing via SSE, or the
//! founder with `tail -f` — can watch what agents store and search without
//! touching the `.mind` file (exclusive lock) and without polluting the
//! protocol channel.
//!
//! Contract (docs/01-spec.md S22):
//! - append-only; every line is one independent, self-contained JSON value —
//!   a reader can start tailing at any point and resync at the next newline;
//! - a write failure NEVER fails the tool call: the warning goes to stderr
//!   (stdout stays exclusive to the MCP protocol, S6) and the client gets
//!   its normal response;
//! - no `--op-log` flag, no `OpLog`: the server field stays `None`, no file
//!   is created and the hot path pays nothing.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

/// An append-only JSONL sink for tool-call entries. File-backed in
/// production ([`OpLog::create`]); any writer in tests
/// ([`OpLog::from_writer`]).
pub struct OpLog {
    sink: Box<dyn Write>,
    /// What stderr warnings call this sink — the file path, in production.
    label: String,
}

impl OpLog {
    /// Opens `path` for appending, creating the file if needed. Failing to
    /// *open* is a startup error — the operator explicitly asked for a log
    /// they would silently not get; only *writes* are best-effort.
    pub fn create(path: &Path) -> std::io::Result<OpLog> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(OpLog {
            sink: Box::new(file),
            label: path.display().to_string(),
        })
    }

    /// Wraps any writer — in-memory buffers in tests, same code path as the
    /// file-backed log.
    pub fn from_writer(writer: impl Write + 'static, label: &str) -> OpLog {
        OpLog {
            sink: Box::new(writer),
            label: label.to_string(),
        }
    }

    /// Appends `entry` as one line and flushes, so a tailing reader sees it
    /// immediately. A failure is reported on stderr and swallowed: the log
    /// is observability, never worth failing the tool call over.
    pub(crate) fn append(&mut self, entry: &Value) {
        let mut line = entry.to_string().into_bytes();
        line.push(b'\n');
        if let Err(e) = self.sink.write_all(&line).and_then(|()| self.sink.flush()) {
            eprintln!(
                "embedmind: op-log write to {} failed ({e}); tool call unaffected",
                self.label
            );
        }
    }
}
