//! Indexes over the record store.
//!
//! M1: paged HNSW (own implementation — `docs/adr/0002`, layout in
//! `docs/FORMAT.md` §7). M2: full-text (inverted index) and metadata filters.
//! Tombstones are filtered at search time until `vacuum` (`docs/adr/0003`).
