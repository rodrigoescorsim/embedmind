//! Indexes over the record store.
//!
//! M1: paged HNSW (own implementation — `docs/adr/0002`, direct page
//! addressing — `docs/adr/0008`, layout in `docs/FORMAT.md` §7). M2:
//! full-text (inverted index) and metadata filters. Tombstones are filtered
//! at search time until `vacuum` (`docs/adr/0003`).
//!
//! Graph mutations happen inside a [`Txn`]: every touched HNSW page (meta,
//! new/updated nodes, new vector pages) is buffered like any other page write
//! and becomes durable atomically with the record insert (`docs/FORMAT.md`
//! §7 — "no separate index journal"). Search reads through
//! [`btree::PageSource`] so it works against either committed state
//! ([`Pager`](crate::storage::Pager)) or an in-flight [`Txn`]'s own writes;
//! callers pass the `hnsw_meta_page` they already have on hand (from the
//! header or the txn), keeping this module's read path agnostic to which
//! concrete source it is.
//!
//! Algorithm: standard HNSW (Malkov & Yashunin 2016) — greedy descent through
//! upper layers to find an entry point, then a bounded best-first search at
//! layer 0 with `ef_search` candidates. Design choices at the state of the
//! art for a disk-resident graph:
//!
//! - **Direct page addressing** (`docs/adr/0008`): adjacency lists hold
//!   HNSW_NODE page numbers, not logical node ids, so there is no
//!   id-to-page location table to grow, rewrite per insert, or load on
//!   open. The meta page is fixed-size forever; one traversal hop is one
//!   page read (same idea that makes DiskANN-style graphs work on disk).
//! - **Diversity-aware neighbor selection** (the paper's Algorithm 4, as in
//!   hnswlib/faiss): a candidate is linked only if it is closer to the base
//!   point than to any already-selected neighbor, with pruned candidates
//!   backfilling remaining capacity (`keepPrunedConnections`). On clustered
//!   real-world data (text embeddings!) this measurably beats "keep the M
//!   closest" recall at the same graph size.
//! - **Adaptive `ef_search`** (DESIGN §5): when the caller's filter
//!   (tombstones, project scope) leaves fewer than `k` hits and the index
//!   holds more nodes than were examined, the search retries with a larger
//!   candidate list (×4 per round, capped at the node count) instead of
//!   silently under-returning. Heavily-tombstoned stores degrade toward a
//!   scan until `vacuum` — honest and documented, never wrong.
//! - **Per-operation read cache**: nodes and vectors touched during one
//!   insert/search are memoized (`Rc`-shared, dropped with the operation),
//!   so the heuristic's pairwise distance checks do not re-read pages. This
//!   is an in-memory memo over [`PageSource`], not a page cache — it can
//!   never go stale across operations because it never outlives one.

pub(crate) mod dict;
pub mod filter_meta;
pub mod fts;
pub mod graph;

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap};
use std::rc::Rc;

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::format::{
    HNSW_DEFAULT_EF_SEARCH, HnswMeta, HnswNode, init_vector_page, max_hnsw_level, vector_page_get,
    vector_page_push,
};
use crate::storage::btree::PageSource;
use crate::storage::pager::Txn;

/// One search hit: the underlying memory record and its similarity score
/// (cosine similarity in `[-1, 1]`, higher is more similar).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// The memory this vector belongs to.
    pub record_id: Ulid,
    /// Cosine similarity to the query vector.
    pub score: f32,
}

/// Search-time tuning; `ef_search` trades recall for latency (`docs/adr/0002`).
#[derive(Debug, Clone, Copy)]
pub struct SearchParams {
    /// Initial candidate list size at layer 0. Grows adaptively when the
    /// caller's filter leaves fewer than `k` hits (DESIGN §5).
    pub ef_search: u16,
}

impl Default for SearchParams {
    fn default() -> Self {
        SearchParams {
            ef_search: HNSW_DEFAULT_EF_SEARCH,
        }
    }
}

/// The **default** `ef_search` for an index holding `node_count` nodes
/// (`docs/adr/0015`, story S16). A fixed `ef_search = 64` is fine at 10k but
/// under-recalls at 100k: recall@10 fell to ~0.93 mean / 0.20 worst-query at
/// 100k while query p99 sat far under the 50 ms ceiling — there is latency
/// budget to buy recall with, but only where the index is actually big.
///
/// So the default *scales with the graph size* instead of staying flat. It is
/// only the default: an explicit [`SearchParams::ef_search`] chosen by the
/// caller (`Query::ef_search(n)`) is passed through untouched — the scaling
/// governs solely the value used when the caller expressed no preference.
///
/// The shape is a step ladder keyed on `node_count`, not a continuous formula:
/// the sweep (`benches/sweep_ef_*`, recorded in ADR 0015) showed recall@10 at
/// 100k rising with `ef` up to a knee and then flattening, so a few measured
/// plateaus are both simpler to reason about and cheaper to keep honest than a
/// fitted curve pretending to a precision the noisy tail does not have. Small
/// indexes keep the fast 64 (their recall is already ≥0.99); the value only
/// climbs as the corpus grows into the regime where the flat default failed.
///
/// This sets the *starting* `ef`; the anti-under-return widening in [`search`]
/// (×4 per round when a filter leaves the result short, DESIGN §5) still runs
/// on top of whatever this returns, so a heavily-filtered query is unaffected.
pub fn default_ef_search(node_count: u64) -> u16 {
    // Thresholds (inclusive lower bound → ef) chosen from the ef sweep in ADR
    // 0015. Ordered ascending; the last band whose bound `node_count` clears
    // wins. Values stay well inside the p99 < 50 ms budget at each size.
    const LADDER: &[(u64, u16)] = &[
        (0, HNSW_DEFAULT_EF_SEARCH), // < 25k: the flat default already ≥0.99
        (25_000, 96),                // 25k–50k: first modest bump
        (50_000, 160),               // 50k–100k: climbing toward the knee
        (100_000, 256),              // ≥100k: at/after the recall knee at 100k
    ];
    let mut ef = HNSW_DEFAULT_EF_SEARCH;
    for &(bound, value) in LADDER {
        if node_count >= bound {
            ef = value;
        }
    }
    ef
}

/// L2-normalizes `v` in place (cosine similarity becomes inner product on
/// normalized vectors, `docs/FORMAT.md` §6). A zero vector is left as-is —
/// its similarity to anything is then 0.0 by construction, never a NaN
/// from a 0/0 division.
pub fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Inserts one embedding into the HNSW index, creating it (the meta page) on
/// the first call. `vector` must already be L2-normalized (callers normalize
/// once at the API boundary — `docs/FORMAT.md` §6). Allocates a VECTOR page
/// slot, then a graph node page, then rewires affected neighbors. Returns the
/// `(page_no, slot)` the vector was stored at, so the caller can stamp the
/// memory record's `vec_ref` with the same location.
pub fn insert(txn: &mut Txn<'_>, dims: u16, record_id: Ulid, vector: &[f32]) -> Result<(u64, u16)> {
    if vector.len() != usize::from(dims) {
        return Err(Error::InvalidArgument("vector length != store dims"));
    }

    let mut meta = load_meta(txn, txn.hnsw_meta_page())?.unwrap_or_default();
    let level_cap = max_hnsw_level(txn.page_size(), meta.m).ok_or(Error::InvalidArgument(
        "page size too small for this index's M parameter",
    ))?;
    let vec_ref = store_vector(txn, dims, vector)?;
    let level = random_level(meta.node_count, meta.m).min(level_cap);

    if meta.node_count == 0 {
        let node = HnswNode {
            record_id,
            vec_page: vec_ref.0,
            vec_slot: vec_ref.1,
            layers: vec![Vec::new(); level + 1],
        };
        let node_page = alloc_node_page(txn, &node)?;
        meta.entry_point_page = node_page;
        meta.max_layer = level as u8;
        meta.node_count = 1;
        save_meta(txn, &meta)?;
        return Ok(vec_ref);
    }

    let mut ctx = Ctx::new(dims);

    // 1) Greedy descent from the current entry point down to layer
    // `level + 1`, narrowing to the single closest node at each layer
    // (ef = 1) — cheap because only one candidate is tracked.
    let mut entry = meta.entry_point_page;
    let mut entry_sim = ctx.sim_to(txn, entry, vector)?;
    for layer in ((level + 1)..=meta.max_layer as usize).rev() {
        let (best, best_sim) = greedy_closest(txn, &mut ctx, entry, entry_sim, layer, vector)?;
        entry = best;
        entry_sim = best_sim;
    }

    // 2) Allocate the new node's page up front (empty adjacency) so
    // `connect` can back-link to it by page number; the fully-wired node is
    // written once at the end.
    let mut new_node = HnswNode {
        record_id,
        vec_page: vec_ref.0,
        vec_slot: vec_ref.1,
        layers: vec![Vec::new(); level + 1],
    };
    let new_node_page = alloc_node_page(txn, &new_node)?;
    meta.node_count += 1;

    // 3) At each layer from min(level, max_layer) down to 0: bounded
    // best-first search, then diversity-aware selection of at most M
    // neighbors (the paper's Algorithm 1 + 4; the 2M layer-0 cap bounds how
    // many links a node may *hold*, not how many a new node picks).
    let mut entry_points = vec![(entry, entry_sim)];
    for layer in (0..=level.min(meta.max_layer as usize)).rev() {
        let ef = meta.ef_construction.max(meta.m);
        let candidates = search_layer(txn, &mut ctx, &entry_points, layer, vector, ef)?;
        let selected = select_neighbors(txn, &mut ctx, &candidates, usize::from(meta.m))?;
        new_node.layers[layer] = selected.clone();
        for &neighbor_page in &selected {
            connect(txn, &mut ctx, &meta, neighbor_page, new_node_page, layer)?;
        }
        entry_points = candidates;
    }
    write_node_at(txn, &mut ctx, new_node_page, &new_node)?;

    if level > meta.max_layer as usize {
        meta.max_layer = level as u8;
        meta.entry_point_page = new_node_page;
    }
    save_meta(txn, &meta)?;
    Ok(vec_ref)
}

/// Nearest-neighbor search against committed or in-flight state. `query`
/// must already be L2-normalized. `hnsw_meta_page` is the caller's own view
/// of the pointer (`Pager::header().hnsw_meta_page` or `Txn::hnsw_meta_page()`
/// — 0 means "no index yet", which yields an empty result rather than an
/// error). `filter` decides which record ids are eligible (tombstone/project
/// filtering lives above the index — `docs/adr/0003`, DESIGN §7); rejected
/// candidates are skipped but still traversed, and `ef_search` grows
/// adaptively while the filter leaves the result under-filled (DESIGN §5).
pub fn search(
    src: &dyn PageSource,
    hnsw_meta_page: u64,
    dims: u16,
    query: &[f32],
    k: usize,
    params: SearchParams,
    mut filter: impl FnMut(Ulid) -> bool,
) -> Result<Vec<Hit>> {
    if query.len() != usize::from(dims) {
        return Err(Error::InvalidArgument("query length != store dims"));
    }
    let Some(meta) = load_meta(src, hnsw_meta_page)? else {
        return Ok(Vec::new());
    };
    if meta.node_count == 0 {
        return Ok(Vec::new());
    }
    let mut ctx = Ctx::new(dims);

    // Upper-layer descent is filter-independent; do it once.
    let mut current = meta.entry_point_page;
    let mut current_sim = ctx.sim_to(src, current, query)?;
    for layer in (1..=meta.max_layer as usize).rev() {
        let (best, best_sim) = greedy_closest(src, &mut ctx, current, current_sim, layer, query)?;
        current = best;
        current_sim = best_sim;
    }

    // Layer-0 search with adaptive ef: filtered-out candidates (tombstones,
    // out-of-scope projects) may leave fewer than k hits even though live
    // ones exist further out; widen the beam and retry until the result is
    // filled or the whole graph has been examined.
    let ef_ceiling = u16::try_from(meta.node_count).unwrap_or(u16::MAX);
    let mut ef = params.ef_search.max(k as u16).max(1);
    loop {
        let candidates = search_layer(src, &mut ctx, &[(current, current_sim)], 0, query, ef)?;
        let mut hits = Vec::with_capacity(k);
        let mut seen: BTreeSet<Ulid> = BTreeSet::new();
        for &(page_no, sim) in &candidates {
            let node = ctx.node(src, page_no)?;
            // A chunked memory has several index nodes sharing one record id
            // (DESIGN §6); candidates are best-first, so the first chunk seen
            // carries the record's best score and later ones are dropped.
            if !seen.insert(node.record_id) {
                continue;
            }
            if filter(node.record_id) {
                hits.push(Hit {
                    record_id: node.record_id,
                    score: sim,
                });
                if hits.len() >= k {
                    break;
                }
            }
        }
        let exhausted = ef >= ef_ceiling || candidates.len() as u64 >= meta.node_count;
        if hits.len() >= k || exhausted {
            return Ok(hits);
        }
        ef = ef.saturating_mul(4).min(ef_ceiling);
    }
}

/// Number of entries in the HNSW graph — one per indexed chunk, so a chunked
/// memory (DESIGN §6) counts once per chunk. 0 when no index exists yet.
/// Feeds `Store::stats` (`embedmind stats`).
pub fn node_count(src: &dyn PageSource, hnsw_meta_page: u64) -> Result<u64> {
    Ok(load_meta(src, hnsw_meta_page)?.map_or(0, |meta| meta.node_count))
}

// ---------------------------------------------------------------------------
// Per-operation read cache
// ---------------------------------------------------------------------------

/// Memoizes nodes and vectors read during one insert/search. Write-through on
/// node writes; never outlives the operation, so it cannot go stale.
struct Ctx {
    dims: u16,
    nodes: HashMap<u64, Rc<HnswNode>>,
    vectors: HashMap<(u64, u16), Rc<Vec<f32>>>,
}

impl Ctx {
    fn new(dims: u16) -> Self {
        Ctx {
            dims,
            nodes: HashMap::new(),
            vectors: HashMap::new(),
        }
    }

    /// Loads (or recalls) the node stored at `page_no`.
    fn node(&mut self, src: &dyn PageSource, page_no: u64) -> Result<Rc<HnswNode>> {
        if let Some(node) = self.nodes.get(&page_no) {
            return Ok(Rc::clone(node));
        }
        let page = src.page(page_no)?;
        let node = Rc::new(HnswNode::decode(&page, page_no)?);
        self.nodes.insert(page_no, Rc::clone(&node));
        Ok(node)
    }

    /// Loads (or recalls) the embedding of the node at `page_no`.
    fn vector(&mut self, src: &dyn PageSource, page_no: u64) -> Result<Rc<Vec<f32>>> {
        let node = self.node(src, page_no)?;
        let key = (node.vec_page, node.vec_slot);
        if let Some(vec) = self.vectors.get(&key) {
            return Ok(Rc::clone(vec));
        }
        let page = src.page(node.vec_page)?;
        let vec = Rc::new(vector_page_get(
            &page,
            self.dims,
            node.vec_slot,
            node.vec_page,
        )?);
        self.vectors.insert(key, Rc::clone(&vec));
        Ok(vec)
    }

    /// Cosine similarity (inner product of normalized vectors) between the
    /// node at `page_no` and `query`.
    fn sim_to(&mut self, src: &dyn PageSource, page_no: u64, query: &[f32]) -> Result<f32> {
        let vec = self.vector(src, page_no)?;
        Ok(dot(&vec, query))
    }
}

// ---------------------------------------------------------------------------
// Graph internals
// ---------------------------------------------------------------------------

/// Max neighbors a node may *hold* at `layer` (`docs/adr/0002`): `M` at
/// layers `>= 1`, `2*M` at layer 0 (the paper's `M_max` / `M_max0`).
fn layer_cap(m: u16, layer: usize) -> usize {
    if layer == 0 {
        usize::from(m) * 2
    } else {
        usize::from(m)
    }
}

/// Frontier entry, ordered so `BinaryHeap` (a max-heap) pops the *highest
/// similarity* (best match) first — the standard best-first-search
/// expansion order.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Frontier {
    page_no: u64,
    sim: f32,
}
impl Eq for Frontier {}
impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sim.partial_cmp(&other.sim).unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Result-set entry, ordered so `BinaryHeap::peek`/`pop` surface the *worst*
/// (lowest similarity) member — the one to evict when the result set grows
/// past `ef`. Reversed relative to [`Frontier`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct Worst {
    page_no: u64,
    sim: f32,
}
impl Eq for Worst {}
impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.sim.partial_cmp(&self.sim).unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Descends greedily within one layer from `(start, start_sim)`, moving to
/// closer neighbors until none improves the similarity (ef = 1). Used for
/// the upper-layer descent, where only the single best entry point matters.
fn greedy_closest(
    src: &dyn PageSource,
    ctx: &mut Ctx,
    start: u64,
    start_sim: f32,
    layer: usize,
    query: &[f32],
) -> Result<(u64, f32)> {
    let mut best = start;
    let mut best_sim = start_sim;
    loop {
        let node = ctx.node(src, best)?;
        let Some(neighbors) = node.layers.get(layer) else {
            return Ok((best, best_sim));
        };
        let neighbors = neighbors.clone();
        let mut improved = false;
        for cand in neighbors {
            let sim = ctx.sim_to(src, cand, query)?;
            if sim > best_sim {
                best = cand;
                best_sim = sim;
                improved = true;
            }
        }
        if !improved {
            return Ok((best, best_sim));
        }
    }
}

/// Best-first search at `layer` starting from `entry_points`, expanding up to
/// `ef` candidates (`docs/adr/0002`). Returns candidates sorted best-first
/// (highest cosine similarity first), at most `ef` of them.
fn search_layer(
    src: &dyn PageSource,
    ctx: &mut Ctx,
    entry_points: &[(u64, f32)],
    layer: usize,
    query: &[f32],
    ef: u16,
) -> Result<Vec<(u64, f32)>> {
    let ef = usize::from(ef).max(1);
    let mut visited: BTreeSet<u64> = entry_points.iter().map(|&(p, _)| p).collect();
    let mut frontier: BinaryHeap<Frontier> = entry_points
        .iter()
        .map(|&(page_no, sim)| Frontier { page_no, sim })
        .collect();
    let mut results: BinaryHeap<Worst> = entry_points
        .iter()
        .map(|&(page_no, sim)| Worst { page_no, sim })
        .collect();

    while let Some(Frontier { page_no, sim }) = frontier.pop() {
        if results.len() >= ef && results.peek().is_some_and(|w| sim < w.sim) {
            break; // nothing left in the frontier can improve the result set
        }
        let node = ctx.node(src, page_no)?;
        let Some(neighbors) = node.layers.get(layer) else {
            continue;
        };
        let neighbors = neighbors.clone();
        for cand in neighbors {
            if !visited.insert(cand) {
                continue;
            }
            let cand_sim = ctx.sim_to(src, cand, query)?;
            let should_add = results.len() < ef || results.peek().is_some_and(|w| cand_sim > w.sim);
            if should_add {
                frontier.push(Frontier {
                    page_no: cand,
                    sim: cand_sim,
                });
                results.push(Worst {
                    page_no: cand,
                    sim: cand_sim,
                });
                if results.len() > ef {
                    results.pop();
                }
            }
        }
    }

    let mut out: Vec<(u64, f32)> = results.into_iter().map(|w| (w.page_no, w.sim)).collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    Ok(out)
}

/// Diversity-aware neighbor selection — the HNSW paper's Algorithm 4 with
/// `keepPrunedConnections`, as implemented by hnswlib/faiss. `candidates`
/// are `(page, similarity to the base point)`, sorted best-first. A candidate
/// is selected only if it is closer to the base point than to every neighbor
/// already selected (keeping links that span different directions/clusters
/// instead of piling onto one dense cluster); pruned candidates then backfill
/// any remaining capacity in similarity order.
fn select_neighbors(
    src: &dyn PageSource,
    ctx: &mut Ctx,
    candidates: &[(u64, f32)],
    cap: usize,
) -> Result<Vec<u64>> {
    let mut selected: Vec<(u64, f32)> = Vec::with_capacity(cap);
    let mut pruned: Vec<u64> = Vec::new();
    for &(cand, cand_sim) in candidates {
        if selected.len() >= cap {
            break;
        }
        let cand_vec = ctx.vector(src, cand)?;
        let mut diverse = true;
        for &(sel, _) in &selected {
            let sel_vec = ctx.vector(src, sel)?;
            // Higher similarity = closer: candidate dominated by `sel` when
            // it is closer to `sel` than to the base point.
            if dot(&cand_vec, &sel_vec) > cand_sim {
                diverse = false;
                break;
            }
        }
        if diverse {
            selected.push((cand, cand_sim));
        } else {
            pruned.push(cand);
        }
    }
    let mut out: Vec<u64> = selected.into_iter().map(|(p, _)| p).collect();
    for cand in pruned {
        if out.len() >= cap {
            break;
        }
        out.push(cand);
    }
    Ok(out)
}

/// Adds `new_page` as a neighbor of the node at `node_page` at `layer`,
/// then, if over the layer cap, re-selects the adjacency with the same
/// diversity heuristic used at insert (base = the node's own vector) —
/// standard HNSW bidirectional linking with heuristic shrinking.
fn connect(
    txn: &mut Txn<'_>,
    ctx: &mut Ctx,
    meta: &HnswMeta,
    node_page: u64,
    new_page: u64,
    layer: usize,
) -> Result<()> {
    let mut node = (*ctx.node(txn, node_page)?).clone();
    if node.layers.len() <= layer {
        // Defensive: a neighbor found at `layer` must have that layer.
        node.layers.resize(layer + 1, Vec::new());
    }
    if node.layers[layer].contains(&new_page) {
        return Ok(());
    }
    node.layers[layer].push(new_page);

    let cap = layer_cap(meta.m, layer);
    if node.layers[layer].len() > cap {
        let self_vec = ctx.vector(txn, node_page)?;
        let mut scored: Vec<(u64, f32)> = Vec::with_capacity(node.layers[layer].len());
        for &page in &node.layers[layer] {
            scored.push((page, ctx.sim_to(txn, page, &self_vec)?));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        node.layers[layer] = select_neighbors(txn, ctx, &scored, cap)?;
    }
    write_node_at(txn, ctx, node_page, &node)
}

// ---------------------------------------------------------------------------
// Page I/O helpers
// ---------------------------------------------------------------------------

/// Loads the HNSW meta page, if the index has been created (`page_no != 0`).
fn load_meta(src: &dyn PageSource, page_no: u64) -> Result<Option<HnswMeta>> {
    if page_no == 0 {
        return Ok(None);
    }
    let page = src.page(page_no)?;
    Ok(Some(HnswMeta::decode(&page, page_no)?))
}

/// Persists `meta`, allocating its page on first use. Moves the transaction's
/// `hnsw_meta_page` pointer; durable atomically with the commit like any
/// other header field (`storage::pager::Txn::set_hnsw_meta_page`).
fn save_meta(txn: &mut Txn<'_>, meta: &HnswMeta) -> Result<()> {
    let page_no = match txn.hnsw_meta_page() {
        0 => txn.allocate_page()?,
        p => p,
    };
    let mut page = vec![0u8; txn.page_size() as usize];
    meta.encode(&mut page)?;
    txn.write_page(page_no, &page)?;
    txn.set_hnsw_meta_page(page_no);
    Ok(())
}

/// Allocates a fresh page for a brand-new node and writes it.
fn alloc_node_page(txn: &mut Txn<'_>, node: &HnswNode) -> Result<u64> {
    let page_no = txn.allocate_page()?;
    let page = node
        .encode(txn.page_size())
        .ok_or(Error::Internal("hnsw node does not fit in one page"))?;
    txn.write_page(page_no, &page)?;
    Ok(page_no)
}

/// Writes `node` at `page_no`, keeping the per-operation cache coherent
/// (write-through).
fn write_node_at(txn: &mut Txn<'_>, ctx: &mut Ctx, page_no: u64, node: &HnswNode) -> Result<()> {
    let page = node
        .encode(txn.page_size())
        .ok_or(Error::Internal("hnsw node does not fit in one page"))?;
    txn.write_page(page_no, &page)?;
    ctx.nodes.insert(page_no, Rc::new(node.clone()));
    Ok(())
}

/// Allocates a fresh VECTOR page and stores one vector on it. v1 always
/// starts a new page per insert (no packing of multiple memories' vectors
/// onto shared pages yet); packing is a space optimization, not a
/// correctness concern, and is deferred until the benchmark harness
/// (`docs/BENCHMARKS.md`) shows file-size pressure from it.
fn store_vector(txn: &mut Txn<'_>, dims: u16, vector: &[f32]) -> Result<(u64, u16)> {
    let page_no = txn.allocate_page()?;
    let mut page = vec![0u8; txn.page_size() as usize];
    init_vector_page(&mut page);
    let slot = vector_page_push(&mut page, dims, vector)?
        .ok_or(Error::Internal("vector does not fit in one page"))?;
    txn.write_page(page_no, &page)?;
    Ok((page_no, slot))
}

// ---------------------------------------------------------------------------
// Random level assignment
// ---------------------------------------------------------------------------

/// Deterministic PRNG (splitmix64) seeded from the insertion ordinal
/// (`node_count` at insert time), so level assignment needs no external
/// randomness source and insert sequences are reproducible for property
/// tests. Distinct from `storage::sim::SplitMix64`, which is a test-harness
/// type this production path must not depend on.
fn random_level(seed: u64, m: u16) -> usize {
    let state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
    let state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;

    // Uniform(0,1) from the top 53 bits, standard HNSW level formula with
    // mL = 1 / ln(M).
    let u = ((z >> 11) as f64) / ((1u64 << 53) as f64);
    let u = u.max(f64::MIN_POSITIVE); // guard ln(0)
    let m_l = 1.0 / f64::from(m.max(2)).ln();
    let level = (-u.ln() * m_l).floor() as usize;
    level.min(31) // callers additionally clamp to max_hnsw_level(page_size, m)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::storage::pager::{Pager, PagerOptions};
    use crate::storage::sim::{SimVfs, SplitMix64};
    use crate::storage::vfs::Vfs;

    const DIMS: u16 = 16;

    fn pager() -> Pager {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        Pager::create(vfs, Path::new("memory.mind"), PagerOptions::default()).unwrap()
    }

    fn random_unit_vector(rng: &mut SplitMix64) -> Vec<f32> {
        let mut v: Vec<f32> = (0..DIMS)
            .map(|_| (rng.next_u64() as i64 as f64 / i64::MAX as f64) as f32)
            .collect();
        normalize(&mut v);
        v
    }

    fn brute_force_top_k(vectors: &[(Ulid, Vec<f32>)], query: &[f32], k: usize) -> Vec<Ulid> {
        let mut scored: Vec<(Ulid, f32)> =
            vectors.iter().map(|(id, v)| (*id, dot(v, query))).collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        scored.into_iter().take(k).map(|(id, _)| id).collect()
    }

    #[test]
    fn default_ef_search_scales_up_with_node_count_and_is_monotonic() {
        // Small indexes keep the fast flat default; the value climbs by band as
        // the corpus grows into the regime where a flat 64 under-recalled (S16,
        // docs/adr/0015). Exact band values are the tuned ADR ladder.
        assert_eq!(default_ef_search(0), HNSW_DEFAULT_EF_SEARCH);
        assert_eq!(default_ef_search(10_000), HNSW_DEFAULT_EF_SEARCH);
        assert_eq!(default_ef_search(24_999), HNSW_DEFAULT_EF_SEARCH);
        assert_eq!(default_ef_search(25_000), 96);
        assert_eq!(default_ef_search(50_000), 160);
        assert_eq!(default_ef_search(100_000), 256);
        assert_eq!(default_ef_search(500_000), 256, "top band holds past 100k");

        // Never decreases as the index grows — a bigger index must never get a
        // smaller default beam.
        let mut prev = 0u16;
        for n in [
            0u64, 24_999, 25_000, 49_999, 50_000, 99_999, 100_000, 1_000_000,
        ] {
            let ef = default_ef_search(n);
            assert!(
                ef >= prev,
                "ef must be monotonic in node_count; {ef} < {prev} at n={n}"
            );
            prev = ef;
        }
    }

    #[test]
    fn normalize_is_unit_length_and_handles_zero() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);

        let mut zero = vec![0.0, 0.0, 0.0];
        normalize(&mut zero);
        assert_eq!(zero, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn single_vector_insert_and_search() {
        let mut pager = pager();
        let id = Ulid::new();
        let mut v = vec![1.0; DIMS as usize];
        normalize(&mut v);

        let mut txn = pager.begin().unwrap();
        insert(&mut txn, DIMS, id, &v).unwrap();
        txn.commit().unwrap();

        let meta_page = pager.header().hnsw_meta_page;
        let hits = search(
            &pager,
            meta_page,
            DIMS,
            &v,
            5,
            SearchParams::default(),
            |_| true,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, id);
        assert!(hits[0].score > 0.99);
    }

    #[test]
    fn empty_index_search_returns_empty() {
        let pager = pager();
        let q = vec![0.5; DIMS as usize];
        let hits = search(&pager, 0, DIMS, &q, 5, SearchParams::default(), |_| true).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn recall_at_10_matches_brute_force_with_high_overlap() {
        let mut pager = pager();
        let mut rng = SplitMix64(0xC0FFEE_u64);
        let mut all: Vec<(Ulid, Vec<f32>)> = Vec::new();

        let mut txn = pager.begin().unwrap();
        for _ in 0..300 {
            let id = Ulid::new();
            let v = random_unit_vector(&mut rng);
            insert(&mut txn, DIMS, id, &v).unwrap();
            all.push((id, v));
        }
        txn.commit().unwrap();

        let meta_page = pager.header().hnsw_meta_page;
        let mut total_recall = 0.0;
        let queries = 20;
        for _ in 0..queries {
            let q = random_unit_vector(&mut rng);
            let approx = search(
                &pager,
                meta_page,
                DIMS,
                &q,
                10,
                SearchParams { ef_search: 64 },
                |_| true,
            )
            .unwrap();
            let approx_ids: HashSet<Ulid> = approx.iter().map(|h| h.record_id).collect();
            let exact = brute_force_top_k(&all, &q, 10);
            let exact_set: HashSet<Ulid> = exact.into_iter().collect();
            let overlap = approx_ids.intersection(&exact_set).count();
            total_recall += overlap as f64 / 10.0;
        }
        let avg_recall = total_recall / f64::from(queries);
        assert!(
            avg_recall >= 0.9,
            "recall@10 = {avg_recall} below 0.9 threshold (docs/TESTING.md §4)"
        );
    }

    /// The old single-page meta table capped the index at ~405 nodes at the
    /// default page size; direct page addressing (`docs/adr/0008`) removes
    /// the cap entirely. 600 inserts across several transactions must work
    /// and stay searchable.
    #[test]
    fn scales_past_the_old_meta_table_cap() {
        let mut pager = pager();
        let mut rng = SplitMix64(0x5CA1E);
        let mut all: Vec<(Ulid, Vec<f32>)> = Vec::new();

        for _ in 0..6 {
            let mut txn = pager.begin().unwrap();
            for _ in 0..100 {
                let id = Ulid::new();
                let v = random_unit_vector(&mut rng);
                insert(&mut txn, DIMS, id, &v).unwrap();
                all.push((id, v));
            }
            txn.commit().unwrap();
        }

        let meta_page = pager.header().hnsw_meta_page;
        // Every stored vector finds itself as the top hit.
        for (id, v) in all.iter().step_by(37) {
            let hits = search(
                &pager,
                meta_page,
                DIMS,
                v,
                1,
                SearchParams::default(),
                |_| true,
            )
            .unwrap();
            assert_eq!(hits[0].record_id, *id);
            assert!(hits[0].score > 0.999);
        }
    }

    /// DESIGN §5: when the filter rejects most candidates (tombstones), the
    /// search widens `ef` adaptively instead of under-returning. With 30
    /// nodes (below the layer-0 prune threshold the graph stays fully
    /// connected), filtering down to 8 live ids must still return all 8,
    /// even from a deliberately tiny initial `ef`.
    #[test]
    fn adaptive_ef_fills_results_under_heavy_filtering() {
        let mut pager = pager();
        let mut rng = SplitMix64(0xDEAD);
        let mut ids = Vec::new();
        let mut txn = pager.begin().unwrap();
        for _ in 0..30 {
            let id = Ulid::new();
            let v = random_unit_vector(&mut rng);
            insert(&mut txn, DIMS, id, &v).unwrap();
            ids.push(id);
        }
        txn.commit().unwrap();

        let live: HashSet<Ulid> = ids[..8].iter().copied().collect();
        let meta_page = pager.header().hnsw_meta_page;
        let q = random_unit_vector(&mut rng);
        let hits = search(
            &pager,
            meta_page,
            DIMS,
            &q,
            8,
            SearchParams { ef_search: 2 },
            |id| live.contains(&id),
        )
        .unwrap();
        assert_eq!(hits.len(), 8, "adaptive ef must find every live node");
        assert!(hits.iter().all(|h| live.contains(&h.record_id)));
    }

    /// DESIGN §6: a chunked memory has several index nodes sharing one
    /// record id; search must return that id once (best chunk's score) and
    /// still fill `k` with other records.
    #[test]
    fn duplicate_record_ids_are_deduped_in_results() {
        let mut pager = pager();
        let mut rng = SplitMix64(0xD0D0);
        let chunked = Ulid::new();
        let mut others = Vec::new();

        let mut txn = pager.begin().unwrap();
        // One record indexed under 5 nearby "chunk" vectors...
        let mut base = random_unit_vector(&mut rng);
        for i in 0..5 {
            let mut v = base.clone();
            v[i] += 0.05;
            normalize(&mut v);
            insert(&mut txn, DIMS, chunked, &v).unwrap();
        }
        // ...plus 20 distinct records.
        for _ in 0..20 {
            let id = Ulid::new();
            let v = random_unit_vector(&mut rng);
            insert(&mut txn, DIMS, id, &v).unwrap();
            others.push(id);
        }
        txn.commit().unwrap();

        normalize(&mut base);
        let meta_page = pager.header().hnsw_meta_page;
        let hits = search(
            &pager,
            meta_page,
            DIMS,
            &base,
            10,
            SearchParams { ef_search: 64 },
            |_| true,
        )
        .unwrap();
        assert_eq!(hits.len(), 10, "dedupe must not under-fill the result");
        let chunked_hits = hits.iter().filter(|h| h.record_id == chunked).count();
        assert_eq!(chunked_hits, 1, "chunked record must appear exactly once");
        assert_eq!(
            hits[0].record_id, chunked,
            "the chunked record's best chunk should rank first for its own base vector"
        );
    }

    #[test]
    fn tombstone_style_filter_skips_but_does_not_break_traversal() {
        let mut pager = pager();
        let mut rng = SplitMix64(0xBEEF);
        let mut ids = Vec::new();
        let mut txn = pager.begin().unwrap();
        for _ in 0..50 {
            let id = Ulid::new();
            let v = random_unit_vector(&mut rng);
            insert(&mut txn, DIMS, id, &v).unwrap();
            ids.push(id);
        }
        txn.commit().unwrap();

        let excluded = ids[0];
        let meta_page = pager.header().hnsw_meta_page;
        let q = random_unit_vector(&mut rng);
        let hits = search(
            &pager,
            meta_page,
            DIMS,
            &q,
            10,
            SearchParams::default(),
            |id| id != excluded,
        )
        .unwrap();
        assert!(hits.iter().all(|h| h.record_id != excluded));
    }

    #[test]
    fn insert_within_txn_is_searchable_before_commit() {
        let mut pager = pager();
        let id = Ulid::new();
        let mut v = vec![2.0; DIMS as usize];
        normalize(&mut v);

        let mut txn = pager.begin().unwrap();
        insert(&mut txn, DIMS, id, &v).unwrap();
        let meta_page = txn.hnsw_meta_page();
        let hits = search(
            &txn,
            meta_page,
            DIMS,
            &v,
            1,
            SearchParams::default(),
            |_| true,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, id);
        drop(txn); // rollback

        assert_eq!(pager.header().hnsw_meta_page, 0);
    }

    #[test]
    fn survives_reopen() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut pager = Pager::create(
            Arc::clone(&vfs),
            Path::new("memory.mind"),
            PagerOptions::default(),
        )
        .unwrap();
        let mut rng = SplitMix64(0xF00D_u64);
        let mut ids = Vec::new();
        let mut txn = pager.begin().unwrap();
        for _ in 0..40 {
            let id = Ulid::new();
            let v = random_unit_vector(&mut rng);
            insert(&mut txn, DIMS, id, &v).unwrap();
            ids.push((id, v));
        }
        txn.commit().unwrap();
        pager.close().unwrap();

        let pager = Pager::open(vfs, Path::new("memory.mind"), PagerOptions::default()).unwrap();
        let meta_page = pager.header().hnsw_meta_page;
        for (id, v) in &ids {
            let hits = search(
                &pager,
                meta_page,
                DIMS,
                v,
                1,
                SearchParams::default(),
                |_| true,
            )
            .unwrap();
            assert_eq!(hits[0].record_id, *id);
        }
    }

    #[test]
    fn rejects_wrong_dims() {
        let mut pager = pager();
        let mut txn = pager.begin().unwrap();
        let bad = vec![0.0; (DIMS - 1) as usize];
        assert!(matches!(
            insert(&mut txn, DIMS, Ulid::new(), &bad),
            Err(Error::InvalidArgument(_))
        ));
    }
}
