//! # embedmind-core
//!
//! The engine: single-file crash-safe storage, vector index, embeddings, hybrid
//! recall. This crate is THE asset — the MCP server and CLI are thin shells over
//! its public API, and no domain logic may live outside it.
//!
//! Layering (see `DESIGN.md` §2 — lower layers never depend on upper ones):
//!
//! ```text
//! api      — Memory, Store, Query (public API)
//! recall   — hybrid score fusion (RRF, from M2)
//! index    — HNSW (M1) · full-text, metadata (M2)
//! embed    — ONNX pipeline behind the `Embedder` trait
//! storage  — pager, WAL, page cache, B-tree
//! format   — binary layout, checksums (incl. `record`, FORMAT.md §5)
//! ```
//!
//! Hard rules enforced here: no network access, no telemetry, no `unsafe`
//! (workspace lint), no `unwrap`/`panic` on production paths.

pub mod api;
pub mod embed;
pub mod error;
pub mod format;
pub mod index;
pub mod recall;
pub mod record;
pub mod storage;

#[doc(hidden)]
pub mod fuzz;

pub use api::{
    Memory, MemoryDraft, Query, RecallOutcome, Recalled, Scope, Store, StoreOptions, StoreStats,
};
pub use error::{Error, Result};
pub use record::Scalar;
pub use ulid::Ulid;
