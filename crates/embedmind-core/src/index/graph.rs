//! Graph layer: explicit entities and typed relations between memories
//! (`docs/adr/0012`, `docs/FORMAT.md` §12) — story S13 (roadmap item 3.1).
//!
//! Everything lives in the `.mind` file's own pages and every mutation goes
//! through a [`Txn`], so the graph is durable and crash-safe on exactly the
//! same terms as the record B-tree, the HNSW index, and the full-text index:
//! touched pages enter the WAL, recovery replays them, no separate journal.
//! Extraction is **not** done here — entities and relations are explicit,
//! provided by the caller at `remember` time (spec S13).
//!
//! ## Structure
//!
//! - **`graph_root_page`** (header) points at one fixed-size **meta page**:
//!   entity/relation counts plus the root of the dictionary. Fixed size
//!   forever, like `HNSW_META` and the FTS meta.
//! - The **dictionary** is the shared byte-keyed paged B-tree
//!   ([`crate::index::dict`], the same structure behind the full-text
//!   dictionary), instantiated with the `GraphDict`/`GraphOverflow` page
//!   types. Two key families share it, distinguished by a leading tag byte:
//!   - `0x01 + entity name` → the entity's **member list** (ids of the
//!     memories tagged with it, sorted);
//!   - `0x02 + memory ULID` → the memory's **adjacency** (its entities and
//!     its relation edges, both directions).
//! - Each relation is written **at both ends** in the same transaction (an
//!   outgoing edge in the source's adjacency, an incoming one in the
//!   target's), so navigation in either direction is a single value read.
//!
//! ## Deletion
//!
//! There is no delete, matching the rest of the engine: `forget` is a
//! tombstone (`docs/adr/0003`). A relation to a forgotten memory disappears
//! with the tombstone — the callers of [`entity_members`]/[`memory_graph`]
//! re-check each returned id's liveness (the same `keep` discipline as the
//! other indexes) — and the bytes are physically reclaimed by `embedmind
//! vacuum`, which rebuilds the graph keeping only live memories' entities
//! and edges whose both ends are live.

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::format::{PAGE_HEADER_LEN, PageHeader, PageType, stamp_page_checksum};
use crate::index::dict;
use crate::storage::btree::PageSource;
use crate::storage::pager::Txn;

/// Longest entity name, in bytes (`docs/FORMAT.md` §12). Entities are
/// caller-provided identifiers, so an over-long name is a typed error, never
/// a silent truncation (truncating could merge two distinct entities).
pub const MAX_ENTITY_LEN: usize = 128;

/// Longest relation kind, in bytes (`docs/FORMAT.md` §12).
pub const MAX_KIND_LEN: usize = 64;

/// The graph dictionary instance: `GraphDict` nodes, `GraphOverflow`
/// spill chains, keys bounded by the entity-key worst case (tag byte +
/// [`MAX_ENTITY_LEN`]; memory keys are shorter at 1 + 16).
const GRAPH_DICT: dict::DictSpec = dict::DictSpec {
    dict: PageType::GraphDict,
    overflow: PageType::GraphOverflow,
    max_key_len: 1 + MAX_ENTITY_LEN,
};

/// Key tags (`docs/FORMAT.md` §12).
const KEY_ENTITY: u8 = 0x01;
const KEY_MEMORY: u8 = 0x02;

/// Edge direction bytes (`docs/FORMAT.md` §12).
const DIR_OUT: u8 = 0;
const DIR_IN: u8 = 1;

/// Fixed edge overhead on disk: direction (1) + kind_len (2) + other id (16).
/// The kind itself is at least 1 byte, so a hostile `edge_count` is bounds-
/// checked against `count * (EDGE_FIXED_LEN + 1)` before allocating.
const EDGE_FIXED_LEN: usize = 1 + 2 + 16;

fn malformed(page_no: u64, what: &'static str) -> Error {
    Error::MalformedPage { page_no, what }
}

fn entity_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + name.len());
    key.push(KEY_ENTITY);
    key.extend_from_slice(name.as_bytes());
    key
}

fn memory_key(id: Ulid) -> [u8; 17] {
    let mut key = [0u8; 17];
    key[0] = KEY_MEMORY;
    key[1..].copy_from_slice(&id.to_bytes());
    key
}

// ---------------------------------------------------------------------------
// Meta page (fixed size)
// ---------------------------------------------------------------------------

/// Graph counters plus the dictionary root, in one fixed-size page
/// (`docs/FORMAT.md` §12). Reached through the header's `graph_root_page`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GraphMeta {
    /// Distinct entities in the dictionary.
    entity_count: u64,
    /// Stored relations (each explicit relation counts once, not per end).
    relation_count: u64,
    /// Root page of the dictionary B-tree; 0 = empty dictionary.
    dict_root: u64,
}

impl GraphMeta {
    fn empty() -> Self {
        GraphMeta {
            entity_count: 0,
            relation_count: 0,
            dict_root: 0,
        }
    }

    fn encode(&self, page_size: u32) -> Result<Vec<u8>> {
        let mut page = vec![0u8; page_size as usize];
        PageHeader {
            page_type: PageType::GraphDict,
            entry_count: 0,
            next_page: 0,
        }
        .encode_into(&mut page);
        let mut off = PAGE_HEADER_LEN;
        page[off] = dict::NODE_META;
        off += 1;
        dict::put_u64(&mut page, &mut off, self.entity_count);
        dict::put_u64(&mut page, &mut off, self.relation_count);
        dict::put_u64(&mut page, &mut off, self.dict_root);
        stamp_page_checksum(&mut page);
        Ok(page)
    }

    fn decode(page: &[u8], page_no: u64) -> Result<Self> {
        let header =
            PageHeader::decode(page).ok_or_else(|| malformed(page_no, "graph page header"))?;
        if header.page_type != PageType::GraphDict {
            return Err(malformed(page_no, "not a graph page"));
        }
        let mut off = PAGE_HEADER_LEN;
        if page.get(off).copied() != Some(dict::NODE_META) {
            return Err(malformed(page_no, "not a graph meta page"));
        }
        off += 1;
        let entity_count = dict::get_u64(page, &mut off, page_no)?;
        let relation_count = dict::get_u64(page, &mut off, page_no)?;
        let dict_root = dict::get_u64(page, &mut off, page_no)?;
        Ok(GraphMeta {
            entity_count,
            relation_count,
            dict_root,
        })
    }
}

/// Loads the meta page, or `None` when no graph exists yet (`root == 0`).
fn load_meta(src: &dyn PageSource, root: u64) -> Result<Option<GraphMeta>> {
    if root == 0 {
        return Ok(None);
    }
    let page = src.page(root)?;
    Ok(Some(GraphMeta::decode(&page, root)?))
}

/// Persists `meta`, allocating the meta page on first use; moves the txn's
/// `graph_root_page` pointer so the change is durable with the commit frame.
fn save_meta(txn: &mut Txn<'_>, meta: &GraphMeta) -> Result<()> {
    let page_no = match txn.graph_root_page() {
        0 => txn.allocate_page()?,
        p => p,
    };
    let page = meta.encode(txn.page_size())?;
    txn.write_page(page_no, &page)?;
    txn.set_graph_root_page(page_no);
    Ok(())
}

// ---------------------------------------------------------------------------
// Value bodies
// ---------------------------------------------------------------------------

/// An entity's member list: ids of the memories tagged with it, sorted
/// strictly ascending (deterministic across platforms — G3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Members {
    ids: Vec<Ulid>,
}

impl Members {
    /// Adds `id` keeping the list sorted; `false` if it was already present.
    fn insert(&mut self, id: Ulid) -> bool {
        match self.ids.binary_search(&id) {
            Ok(_) => false,
            Err(i) => {
                self.ids.insert(i, id);
                true
            }
        }
    }

    /// Serialized body: `member_count` (u32) + members × 16-byte ids.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.ids.len() * 16);
        out.extend_from_slice(&(self.ids.len() as u32).to_le_bytes());
        for id in &self.ids {
            out.extend_from_slice(&id.to_bytes());
        }
        out
    }

    /// Parses a members body. Validates the count against the buffer before
    /// allocating (fuzz rule, `docs/TESTING.md` §3) and rejects unsorted or
    /// duplicate ids (a corrupt or hostile page).
    fn decode(body: &[u8], page_no: u64) -> Result<Self> {
        let count = dict::read_u32(body, 0, page_no)? as usize;
        let need = 4usize
            .checked_add(
                count
                    .checked_mul(16)
                    .ok_or_else(|| malformed(page_no, "graph members count overflow"))?,
            )
            .ok_or_else(|| malformed(page_no, "graph members length overflow"))?;
        if body.len() < need {
            return Err(malformed(page_no, "graph members truncated"));
        }
        let mut ids = Vec::with_capacity(count);
        let mut prev: Option<Ulid> = None;
        let mut off = 4;
        for _ in 0..count {
            let id_bytes: [u8; 16] = body
                .get(off..off + 16)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| malformed(page_no, "graph member id"))?;
            let id = Ulid::from_bytes(id_bytes);
            if prev.is_some_and(|p| p >= id) {
                return Err(malformed(page_no, "unsorted graph members"));
            }
            prev = Some(id);
            ids.push(id);
            off += 16;
        }
        Ok(Members { ids })
    }
}

/// One relation edge as seen from a memory's adjacency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// `true` = this memory relates *to* [`Edge::other`]; `false` = `other`
    /// relates to this memory (the mirrored end of its outgoing edge).
    pub outgoing: bool,
    /// The relation kind ("refines", "contradicts", …), 1–64 bytes.
    pub kind: String,
    /// The memory at the other end.
    pub other: Ulid,
}

impl Edge {
    /// On-disk (and in-memory) ordering: `(direction, kind, other)`,
    /// strictly ascending — deterministic across platforms (G3).
    fn sort_key(&self) -> (u8, &[u8], [u8; 16]) {
        (
            if self.outgoing { DIR_OUT } else { DIR_IN },
            self.kind.as_bytes(),
            self.other.to_bytes(),
        )
    }
}

/// A memory's adjacency: the entities it is tagged with and its relation
/// edges (both directions), as stored under its `0x02` dictionary key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryGraph {
    /// Entity names, sorted strictly ascending.
    pub entities: Vec<String>,
    /// Edges, sorted strictly ascending by `(direction, kind, other)`.
    pub edges: Vec<Edge>,
}

impl MemoryGraph {
    /// Adds an entity keeping the list sorted; `false` if already present.
    fn insert_entity(&mut self, name: &str) -> bool {
        match self.entities.binary_search_by(|e| e.as_str().cmp(name)) {
            Ok(_) => false,
            Err(i) => {
                self.entities.insert(i, name.to_owned());
                true
            }
        }
    }

    /// Adds an edge keeping the list sorted; `false` if already present.
    fn insert_edge(&mut self, edge: Edge) -> bool {
        match self
            .edges
            .binary_search_by(|e| e.sort_key().cmp(&edge.sort_key()))
        {
            Ok(_) => false,
            Err(i) => {
                self.edges.insert(i, edge);
                true
            }
        }
    }

    /// Serialized adjacency body (`docs/FORMAT.md` §12).
    fn encode(&self) -> Result<Vec<u8>> {
        let entity_count = u16::try_from(self.entities.len())
            .map_err(|_| Error::InvalidArgument("too many entities on one memory"))?;
        let edge_count = u32::try_from(self.edges.len())
            .map_err(|_| Error::InvalidArgument("too many edges on one memory"))?;
        let mut out = Vec::new();
        out.extend_from_slice(&entity_count.to_le_bytes());
        for name in &self.entities {
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
        }
        out.extend_from_slice(&edge_count.to_le_bytes());
        for edge in &self.edges {
            out.push(if edge.outgoing { DIR_OUT } else { DIR_IN });
            out.extend_from_slice(&(edge.kind.len() as u16).to_le_bytes());
            out.extend_from_slice(edge.kind.as_bytes());
            out.extend_from_slice(&edge.other.to_bytes());
        }
        Ok(out)
    }

    /// Parses an adjacency body. Fully bounds-checked, count-guarded before
    /// allocation, ordering and length caps enforced (fuzz rules,
    /// `docs/TESTING.md` §3).
    fn decode(body: &[u8], page_no: u64) -> Result<Self> {
        let mut off = 0usize;
        let entity_count = dict::get_u16(body, &mut off, page_no)? as usize;
        // Each entity needs at least its length prefix + 1 byte of name.
        if entity_count * 3 > body.len().saturating_sub(off) {
            return Err(malformed(page_no, "graph entity count exceeds body"));
        }
        let mut entities = Vec::with_capacity(entity_count);
        let mut prev_name: Option<Vec<u8>> = None;
        for _ in 0..entity_count {
            let name =
                dict::get_bytes_u16(body, &mut off, body.len(), page_no, "graph entity name")?;
            if name.is_empty() || name.len() > MAX_ENTITY_LEN {
                return Err(malformed(page_no, "graph entity name length"));
            }
            if prev_name.as_ref().is_some_and(|p| p.as_slice() >= name) {
                return Err(malformed(page_no, "unsorted graph entities"));
            }
            let owned = std::str::from_utf8(name)
                .map_err(|_| malformed(page_no, "graph entity not utf-8"))?
                .to_owned();
            prev_name = Some(name.to_vec());
            entities.push(owned);
        }
        let edge_count = dict::get_u32(body, &mut off, page_no)? as usize;
        if edge_count
            .checked_mul(EDGE_FIXED_LEN + 1)
            .is_none_or(|n| n > body.len().saturating_sub(off))
        {
            return Err(malformed(page_no, "graph edge count exceeds body"));
        }
        let mut edges: Vec<Edge> = Vec::with_capacity(edge_count);
        for _ in 0..edge_count {
            let dir = *body
                .get(off)
                .ok_or_else(|| malformed(page_no, "graph edge direction"))?;
            if dir != DIR_OUT && dir != DIR_IN {
                return Err(malformed(page_no, "graph edge direction byte"));
            }
            off += 1;
            let kind = dict::get_bytes_u16(body, &mut off, body.len(), page_no, "graph edge kind")?;
            if kind.is_empty() || kind.len() > MAX_KIND_LEN {
                return Err(malformed(page_no, "graph edge kind length"));
            }
            let kind = std::str::from_utf8(kind)
                .map_err(|_| malformed(page_no, "graph edge kind not utf-8"))?
                .to_owned();
            let id_bytes: [u8; 16] = body
                .get(off..off + 16)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| malformed(page_no, "graph edge other id"))?;
            off += 16;
            let edge = Edge {
                outgoing: dir == DIR_OUT,
                kind,
                other: Ulid::from_bytes(id_bytes),
            };
            if edges
                .last()
                .is_some_and(|prev| prev.sort_key() >= edge.sort_key())
            {
                return Err(malformed(page_no, "unsorted graph edges"));
            }
            edges.push(edge);
        }
        Ok(MemoryGraph { entities, edges })
    }
}

fn get_members(src: &dyn PageSource, root: u64, name: &str) -> Result<Option<Members>> {
    match dict::get(src, GRAPH_DICT, root, &entity_key(name))? {
        Some((body, page_no)) => Ok(Some(Members::decode(&body, page_no)?)),
        None => Ok(None),
    }
}

fn get_adjacency(src: &dyn PageSource, root: u64, id: Ulid) -> Result<Option<MemoryGraph>> {
    match dict::get(src, GRAPH_DICT, root, &memory_key(id))? {
        Some((body, page_no)) => Ok(Some(MemoryGraph::decode(&body, page_no)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Public write / read API
// ---------------------------------------------------------------------------

/// Writes one memory's graph data — entity tags and outgoing relations —
/// within `txn`, so it is durable atomically with the record insert
/// (`docs/adr/0012`). Each relation `(kind, target)` is mirrored as an
/// incoming edge on the target's adjacency in the same transaction.
///
/// Validates names and kinds (typed errors) but **not** target existence —
/// that is the caller's job (`Store::remember` checks liveness against the
/// record B-tree; `vacuum`'s rebuild pre-filters to live targets, which may
/// not be re-inserted yet when the edge is written). A memory with no
/// entities and no relations writes nothing — a store that never uses the
/// graph never allocates graph pages.
pub fn add_memory(
    txn: &mut Txn<'_>,
    id: Ulid,
    entities: &[String],
    relations: &[(String, Ulid)],
) -> Result<()> {
    if entities.is_empty() && relations.is_empty() {
        return Ok(());
    }
    for name in entities {
        if name.is_empty() || name.len() > MAX_ENTITY_LEN {
            return Err(Error::InvalidArgument(
                "entity name must be 1–128 bytes of UTF-8",
            ));
        }
    }
    for (kind, to) in relations {
        if kind.is_empty() || kind.len() > MAX_KIND_LEN {
            return Err(Error::InvalidArgument(
                "relation kind must be 1–64 bytes of UTF-8",
            ));
        }
        if *to == id {
            return Err(Error::InvalidArgument("a memory cannot relate to itself"));
        }
    }

    let mut meta = load_meta(txn, txn.graph_root_page())?.unwrap_or_else(GraphMeta::empty);
    let mut root = meta.dict_root;
    let mut adj = get_adjacency(txn, root, id)?.unwrap_or_default();

    // Deterministic write order (sorted, deduped) keeps the page sequence
    // reproducible — same discipline as the FTS term loop (DESIGN §9).
    let mut names: Vec<&String> = entities.iter().collect();
    names.sort();
    names.dedup();
    for name in names {
        if adj.insert_entity(name) {
            let (mut members, existed) = match get_members(txn, root, name)? {
                Some(m) => (m, true),
                None => (Members::default(), false),
            };
            members.insert(id);
            root = dict::upsert(txn, GRAPH_DICT, root, &entity_key(name), &members.encode())?;
            if !existed {
                meta.entity_count = meta.entity_count.saturating_add(1);
            }
        }
    }

    let mut rels: Vec<&(String, Ulid)> = relations.iter().collect();
    rels.sort();
    rels.dedup();
    for (kind, to) in rels {
        let inserted = adj.insert_edge(Edge {
            outgoing: true,
            kind: kind.clone(),
            other: *to,
        });
        if inserted {
            // Mirror the incoming edge at the target, same transaction.
            let mut target = get_adjacency(txn, root, *to)?.unwrap_or_default();
            target.insert_edge(Edge {
                outgoing: false,
                kind: kind.clone(),
                other: id,
            });
            root = dict::upsert(txn, GRAPH_DICT, root, &memory_key(*to), &target.encode()?)?;
            meta.relation_count = meta.relation_count.saturating_add(1);
        }
    }

    root = dict::upsert(txn, GRAPH_DICT, root, &memory_key(id), &adj.encode()?)?;
    meta.dict_root = root;
    save_meta(txn, &meta)?;
    Ok(())
}

/// Ids of the memories tagged with `entity`, sorted ascending. Empty when the
/// entity is unknown or the file has no graph (`graph_root_page == 0`) — an
/// older file degrades to "nothing related", never an error. Liveness is the
/// caller's re-check (`docs/adr/0003`): tombstoned members are still listed
/// here until `vacuum` rebuilds the graph.
pub fn entity_members(src: &dyn PageSource, graph_root: u64, entity: &str) -> Result<Vec<Ulid>> {
    let Some(meta) = load_meta(src, graph_root)? else {
        return Ok(Vec::new());
    };
    Ok(get_members(src, meta.dict_root, entity)?
        .map(|m| m.ids)
        .unwrap_or_default())
}

/// The stored adjacency of one memory — its entity tags and relation edges
/// (both directions) — or `None` when the memory has no graph data (or the
/// file has no graph at all). Same liveness caveat as [`entity_members`].
pub fn memory_graph(
    src: &dyn PageSource,
    graph_root: u64,
    id: Ulid,
) -> Result<Option<MemoryGraph>> {
    let Some(meta) = load_meta(src, graph_root)? else {
        return Ok(None);
    };
    get_adjacency(src, meta.dict_root, id)
}

/// Graph counters for `embedmind stats`: `(distinct entities, relations)`.
/// `(0, 0)` when no graph exists yet.
pub fn stats(src: &dyn PageSource, graph_root: u64) -> Result<(u64, u64)> {
    Ok(load_meta(src, graph_root)?.map_or((0, 0), |m| (m.entity_count, m.relation_count)))
}

/// Fuzz-only surface: decode one page as each graph node kind and both value
/// bodies, exercising every parser branch. Must return, never panic
/// (`fuzz_graph_page` target, `docs/TESTING.md` §3).
#[doc(hidden)]
pub fn fuzz_decode_page(page: &[u8]) {
    dict::fuzz_decode_node(page, GRAPH_DICT);
    let _ = GraphMeta::decode(page, 1);
    // Value bodies live at the page content region; try both decoders there
    // and over the raw buffer.
    if page.len() > PAGE_HEADER_LEN {
        let _ = Members::decode(&page[PAGE_HEADER_LEN..], 1);
        let _ = MemoryGraph::decode(&page[PAGE_HEADER_LEN..], 1);
    }
    let _ = Members::decode(page, 1);
    let _ = MemoryGraph::decode(page, 1);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::storage::pager::{Pager, PagerOptions};
    use crate::storage::sim::{SimVfs, SplitMix64};
    use crate::storage::vfs::Vfs;

    fn pager(page_size: u32) -> Pager {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        Pager::create(
            vfs,
            Path::new("memory.mind"),
            PagerOptions {
                page_size,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn entities_and_relations_roundtrip() {
        let mut pager = pager(4096);
        let a = Ulid::from_parts(1, 1);
        let b = Ulid::from_parts(2, 2);
        let mut txn = pager.begin().unwrap();
        add_memory(&mut txn, a, &["postgres".into()], &[]).unwrap();
        add_memory(
            &mut txn,
            b,
            &["postgres".into(), "auth".into()],
            &[("refines".into(), a)],
        )
        .unwrap();
        txn.commit().unwrap();

        let root = pager.header().graph_root_page;
        assert_ne!(root, 0);
        assert_eq!(
            entity_members(&pager, root, "postgres").unwrap(),
            vec![a, b]
        );
        assert_eq!(entity_members(&pager, root, "auth").unwrap(), vec![b]);
        assert!(entity_members(&pager, root, "unknown").unwrap().is_empty());

        let bg = memory_graph(&pager, root, b).unwrap().unwrap();
        assert_eq!(bg.entities, vec!["auth".to_owned(), "postgres".to_owned()]);
        assert_eq!(
            bg.edges,
            vec![Edge {
                outgoing: true,
                kind: "refines".into(),
                other: a
            }]
        );
        // The mirrored incoming edge at the target.
        let ag = memory_graph(&pager, root, a).unwrap().unwrap();
        assert_eq!(
            ag.edges,
            vec![Edge {
                outgoing: false,
                kind: "refines".into(),
                other: b
            }]
        );
        assert_eq!(stats(&pager, root).unwrap(), (2, 1));
    }

    #[test]
    fn no_graph_data_writes_no_graph_pages() {
        let mut pager = pager(4096);
        let mut txn = pager.begin().unwrap();
        add_memory(&mut txn, Ulid::new(), &[], &[]).unwrap();
        assert_eq!(txn.graph_root_page(), 0, "nothing to store, no meta page");
        drop(txn);
        assert_eq!(pager.header().graph_root_page, 0);
        assert_eq!(stats(&pager, 0).unwrap(), (0, 0));
        assert!(entity_members(&pager, 0, "x").unwrap().is_empty());
        assert_eq!(memory_graph(&pager, 0, Ulid::new()).unwrap(), None);
    }

    #[test]
    fn duplicate_tags_and_relations_are_idempotent() {
        let mut pager = pager(4096);
        let a = Ulid::from_parts(1, 1);
        let b = Ulid::from_parts(2, 2);
        let mut txn = pager.begin().unwrap();
        add_memory(
            &mut txn,
            b,
            &["dup".into(), "dup".into()],
            &[("kind".into(), a), ("kind".into(), a)],
        )
        .unwrap();
        txn.commit().unwrap();
        let root = pager.header().graph_root_page;
        assert_eq!(entity_members(&pager, root, "dup").unwrap(), vec![b]);
        let bg = memory_graph(&pager, root, b).unwrap().unwrap();
        assert_eq!(bg.entities.len(), 1);
        assert_eq!(bg.edges.len(), 1);
        assert_eq!(stats(&pager, root).unwrap(), (1, 1));
    }

    #[test]
    fn invalid_inputs_are_typed_errors() {
        let mut pager = pager(4096);
        let id = Ulid::from_parts(1, 1);
        let mut txn = pager.begin().unwrap();
        assert!(matches!(
            add_memory(&mut txn, id, &[String::new()], &[]),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            add_memory(&mut txn, id, &["x".repeat(MAX_ENTITY_LEN + 1)], &[]),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            add_memory(&mut txn, id, &[], &[(String::new(), Ulid::new())]),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            add_memory(&mut txn, id, &[], &[("self".into(), id)]),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn many_entities_force_splits_and_survive_reopen() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut pager = Pager::create(
            Arc::clone(&vfs),
            Path::new("memory.mind"),
            PagerOptions {
                page_size: 512, // small pages: dictionary splits within the test
                ..Default::default()
            },
        )
        .unwrap();
        let mut ids = Vec::new();
        let mut txn = pager.begin().unwrap();
        for i in 0..150u64 {
            let id = Ulid::from_parts(i + 1, u128::from(i));
            ids.push(id);
            add_memory(
                &mut txn,
                id,
                &[format!("entity{i:04}"), "shared".into()],
                &[],
            )
            .unwrap();
        }
        txn.commit().unwrap();
        pager.close().unwrap();

        let pager = Pager::open(vfs, Path::new("memory.mind"), PagerOptions::default()).unwrap();
        let root = pager.header().graph_root_page;
        for (i, id) in ids.iter().enumerate() {
            let members = entity_members(&pager, root, &format!("entity{i:04}")).unwrap();
            assert_eq!(members, vec![*id], "entity{i:04}");
        }
        // "shared" tags every memory: its member list went through an
        // overflow chain at 512-byte pages and still reads back whole.
        assert_eq!(entity_members(&pager, root, "shared").unwrap(), ids);
        assert_eq!(stats(&pager, root).unwrap(), (151, 0));
    }

    #[test]
    fn adjacency_within_txn_is_visible_before_commit_and_rolls_back() {
        let mut pager = pager(4096);
        let a = Ulid::from_parts(1, 1);
        let mut txn = pager.begin().unwrap();
        add_memory(&mut txn, a, &["inflight".into()], &[]).unwrap();
        let root = txn.graph_root_page();
        assert_eq!(entity_members(&txn, root, "inflight").unwrap(), vec![a]);
        drop(txn); // rollback
        assert_eq!(pager.header().graph_root_page, 0);
    }

    #[test]
    fn decode_never_panics_on_arbitrary_bytes() {
        let mut rng = SplitMix64(0x612A_u64);
        for _ in 0..3000 {
            let len = [64usize, 200, 512, 4096][(rng.next_u64() % 4) as usize];
            let mut page = vec![0u8; len];
            for b in &mut page {
                *b = rng.next_u64() as u8;
            }
            fuzz_decode_page(&page); // must return, never panic
        }
        // Mutated valid meta/adjacency bytes exercise deeper branches.
        let meta = GraphMeta {
            entity_count: 3,
            relation_count: 2,
            dict_root: 7,
        }
        .encode(512)
        .unwrap();
        let adjacency = MemoryGraph {
            entities: vec!["auth".into(), "postgres".into()],
            edges: vec![
                Edge {
                    outgoing: true,
                    kind: "refines".into(),
                    other: Ulid::from_parts(9, 9),
                },
                Edge {
                    outgoing: false,
                    kind: "refines".into(),
                    other: Ulid::from_parts(3, 3),
                },
            ],
        }
        .encode()
        .unwrap();
        for base in [meta, adjacency] {
            for _ in 0..2000 {
                let mut page = base.clone();
                let i = (rng.next_u64() as usize) % page.len();
                page[i] ^= (rng.next_u64() as u8) | 1;
                fuzz_decode_page(&page);
            }
        }
    }
}
