//! Public API of the engine: [`Store`], [`Memory`], [`MemoryDraft`], [`Query`].
//!
//! This is the only module the shells (`embedmind-mcp`, `embedmind-cli`) and
//! future bindings are allowed to depend on. Data model: `DESIGN.md` §3.2.
//!
//! M1 item 1.2 scope: durable KV over the record B-tree — `remember`, `get`,
//! `forget` (tombstone, `docs/adr/0003`), timeline iteration. M1 item 1.3
//! adds vector recall: when a [`Store`] has an [`Embedder`], `remember`
//! embeds the content and indexes it (`index::insert`); [`Store::recall`]
//! runs a nearest-neighbor search (`index::search`) filtered to live,
//! in-scope memories. A `Store` without an embedder behaves exactly as
//! before — vector recall is a non-breaking addition, not a requirement.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ulid::Ulid;

use crate::embed::{Embedder, OnnxEmbedder};
use crate::error::{Error, Result};
use crate::index::{self, SearchParams};
use crate::record::{Filter, MemoryRecord, Provenance, Scalar, VecRef};
use crate::storage::btree;
use crate::storage::{Pager, PagerOptions, RealVfs, Vfs};

/// Store tuning knobs. The defaults are right for almost everyone.
#[derive(Clone)]
pub struct StoreOptions {
    /// Page size for newly created files (existing files use the size
    /// recorded in their header).
    pub page_size: u32,
    /// WAL size, in bytes, at which a commit triggers a checkpoint.
    pub checkpoint_threshold: u64,
    /// Embedder used to index `remember`ed content for vector recall.
    /// `None` = KV-only store (no embedding, no indexing, no `recall`) —
    /// what the crash harness, fuzzers and KV-focused tests want, since
    /// loading a real model costs real time. [`Store::create`]/[`Store::open`]
    /// default this to the embedded ONNX model; use `*_with` to opt out.
    pub embedder: Option<Arc<dyn Embedder>>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        let p = PagerOptions::default();
        StoreOptions {
            page_size: p.page_size,
            checkpoint_threshold: p.checkpoint_threshold,
            embedder: None,
        }
    }
}

impl StoreOptions {
    fn pager(&self) -> PagerOptions {
        PagerOptions {
            page_size: self.page_size,
            checkpoint_threshold: self.checkpoint_threshold,
            ..PagerOptions::default()
        }
    }
}

/// Lazily-initialized process-wide default embedder, shared across every
/// [`Store::create`]/[`Store::open`] call so opening several stores (or
/// reopening one) does not reinitialize the ONNX Runtime session each time.
fn default_embedder() -> Result<Arc<dyn Embedder>> {
    static DEFAULT: OnceLock<std::result::Result<Arc<dyn Embedder>, String>> = OnceLock::new();
    DEFAULT
        .get_or_init(|| {
            OnnxEmbedder::load()
                .map(|e| Arc::new(e) as Arc<dyn Embedder>)
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|msg| Error::Internal(Box::leak(msg.into_boxed_str())))
}

/// A memory store: one crash-safe `.mind` file. Single writer per file
/// (`docs/adr/0006`); every mutating call is one durable transaction —
/// when it returns `Ok`, the data survives `kill -9` and power loss.
pub struct Store {
    pager: Pager,
    embedder: Option<Arc<dyn Embedder>>,
    /// The VFS and path this store lives on — kept so `vacuum` can build a
    /// sibling temp file and swap it in atomically (`docs/adr/0003`). Every
    /// other operation goes through `pager`, which owns its own handle.
    vfs: Arc<dyn Vfs>,
    path: PathBuf,
    /// The WAL checkpoint threshold this store was opened with, so `vacuum`'s
    /// rebuilt file and post-swap reopen keep the same tuning rather than
    /// silently reverting to a default.
    checkpoint_threshold: u64,
    /// The filter-meta sidecar (`docs/adr/0027`) materialized in memory,
    /// keyed on the `txn_counter` it was built from — any commit bumps the
    /// counter and naturally invalidates it. `RwLock` (not `RefCell`) so
    /// shared `&self` queries can fill it without giving up `Sync`; the
    /// on-disk chains stay authoritative, this is a pure read cache.
    filter_meta_cache: std::sync::RwLock<Option<(u64, Arc<index::filter_meta::Table>)>>,
}

impl Store {
    /// Creates a new store at `path`, with vector recall enabled via the
    /// default embedded ONNX model. Fails if the file exists.
    pub fn create(path: &Path) -> Result<Store> {
        let opts = StoreOptions {
            embedder: Some(default_embedder()?),
            ..StoreOptions::default()
        };
        Self::create_with(Arc::new(RealVfs), path, opts)
    }

    /// Opens an existing store (recovery runs automatically), with vector
    /// recall enabled via the default embedded ONNX model.
    pub fn open(path: &Path) -> Result<Store> {
        let opts = StoreOptions {
            embedder: Some(default_embedder()?),
            ..StoreOptions::default()
        };
        Self::open_with(Arc::new(RealVfs), path, opts)
    }

    /// Opens `path`, creating it first if it does not exist — what the
    /// shells use on startup. Vector recall enabled via the default
    /// embedded ONNX model.
    pub fn open_or_create(path: &Path) -> Result<Store> {
        let vfs: Arc<dyn Vfs> = Arc::new(RealVfs);
        let opts = StoreOptions {
            embedder: Some(default_embedder()?),
            ..StoreOptions::default()
        };
        if vfs.exists(path) {
            Self::open_with(vfs, path, opts)
        } else {
            Self::create_with(vfs, path, opts)
        }
    }

    /// [`Store::create`] with an explicit [`Vfs`] and options — the seam the
    /// crash harness, fuzzers, and tests that don't need embeddings use.
    pub fn create_with(vfs: Arc<dyn Vfs>, path: &Path, opts: StoreOptions) -> Result<Store> {
        let embedder = opts.embedder.clone();
        let pager = Pager::create(Arc::clone(&vfs), path, opts.pager())?;
        let mut store = Store {
            pager,
            embedder,
            vfs,
            path: path.to_path_buf(),
            checkpoint_threshold: opts.checkpoint_threshold,
            filter_meta_cache: std::sync::RwLock::new(None),
        };
        store.init_embedding_header()?;
        Ok(store)
    }

    /// [`Store::open`] with an explicit [`Vfs`] and options.
    pub fn open_with(vfs: Arc<dyn Vfs>, path: &Path, opts: StoreOptions) -> Result<Store> {
        let embedder = opts.embedder.clone();
        // A crash mid-`vacuum` may leave sibling temp/scratch files behind; the
        // original is always intact (the swap is the last, atomic step), so we
        // just sweep those orphans away on open — never adopt them.
        for orphan in [vacuum_tmp_path(path), vacuum_scratch_path(path)] {
            if vfs.exists(&orphan) {
                vfs.delete(&orphan).ok();
            }
            let orphan_wal = wal_sidecar_path(&orphan);
            if vfs.exists(&orphan_wal) {
                vfs.delete(&orphan_wal).ok();
            }
        }
        let pager = Pager::open(Arc::clone(&vfs), path, opts.pager())?;
        let mut store = Store {
            pager,
            embedder,
            vfs,
            path: path.to_path_buf(),
            checkpoint_threshold: opts.checkpoint_threshold,
            filter_meta_cache: std::sync::RwLock::new(None),
        };
        store.init_embedding_header()?;
        Ok(store)
    }

    /// Stamps the header's `embedding_dims`/`embedding_model_id` from this
    /// store's embedder the first time it is used against a fresh file
    /// (`embedding_dims == 0`), and refuses to open a file whose recorded
    /// model does not match — mixing embeddings from different models in one
    /// file is exactly the corruption-by-config-drift `docs/adr/0004` rules
    /// out. A store with no embedder never touches these fields.
    fn init_embedding_header(&mut self) -> Result<()> {
        let Some(embedder) = self.embedder.clone() else {
            return Ok(());
        };
        let header = self.pager.header();
        // One embedding must fit a VECTOR page (`docs/FORMAT.md` §6): fail
        // clearly at open time, not with an internal error mid-`remember`.
        if crate::format::vector_slots_per_page(header.page_size, embedder.dims()) == 0 {
            return Err(Error::InvalidArgument(
                "page size too small for this embedder's dimensionality",
            ));
        }
        if header.embedding_dims == 0 && header.embedding_model_id.is_empty() {
            let mut txn = self.pager.begin()?;
            txn.set_embedding_model(embedder.id(), embedder.dims())?;
            txn.commit()?;
        } else if header.embedding_model_id != embedder.id()
            || header.embedding_dims != embedder.dims()
        {
            return Err(Error::InvalidArgument(
                "store's embedding model does not match this Embedder; use `embedmind reembed`",
            ));
        }
        Ok(())
    }

    /// Stores one memory durably and returns it (with its generated id and
    /// timestamp). If this store has an [`Embedder`], the content is embedded
    /// and indexed for [`Store::recall`] in the same transaction; otherwise
    /// the memory is stored without a vector, exactly as in a KV-only store.
    ///
    /// Content longer than the model's window is embedded in overlapping
    /// chunks ([`Embedder::embed_chunks`], DESIGN §6): the record stays
    /// whole, each chunk becomes one more index entry pointing at it, and
    /// `recall` returns the memory once (deduped by id) if *any* chunk
    /// matches. The record's `vec_ref` points at the first chunk's vector.
    ///
    /// Explicit graph data ([`MemoryDraft::entity`]/[`MemoryDraft::relation`],
    /// S13, `docs/adr/0012`) is written in the same transaction: the memory
    /// enters with its entities and relations complete, or not at all. A
    /// relation whose target does not exist (or was already forgotten) is a
    /// typed error — dangling edges are never born, they can only *become*
    /// dangling via a later [`Store::forget`].
    ///
    /// Versioned knowledge ([`MemoryDraft::supersede`], S19, `docs/adr/0013`):
    /// each supersedes target gets its `superseded` flag set and a
    /// `"supersedes"` graph edge from the new memory, all in this same
    /// transaction. From then on the target is excluded from every
    /// recall/search (re-checked against its record at query time, like a
    /// tombstone) but stays readable via [`Store::get`]/[`Store::related`] as
    /// the previous version. A target that does not exist or was forgotten is
    /// a typed error; so is a target scoped to a *different* project —
    /// superseding never crosses project boundaries.
    pub fn remember(&mut self, draft: MemoryDraft) -> Result<Memory> {
        let chunks = self.embed_draft_content(&draft.content)?;
        self.write_remembered(draft, chunks)
    }

    /// [`Store::remember`] plus write-time curation (S21, `docs/adr/0016`):
    /// before storing, the *same* embedding the write will index is searched
    /// against the existing memories (zero extra embedding cost), and the
    /// ones above [`NEAR_DUP_THRESHOLD`] come back as
    /// [`Remembered::similar`] — hint material for the caller to decide
    /// `forget`, [`MemoryDraft::supersede`], or keep both. The store **always
    /// happens**; near-duplicates inform, they never block.
    ///
    /// Only live, non-superseded memories in the draft's exact scope (same
    /// project, or global for a global draft) are considered — a near-match
    /// in another project is not a duplicate of anything. The first memory of
    /// a file (or a KV-only store) yields an empty list.
    pub fn remember_detailed(&mut self, draft: MemoryDraft) -> Result<Remembered> {
        let chunks = self.embed_draft_content(&draft.content)?;
        let similar = self.near_duplicates(&chunks, draft.project.as_deref())?;
        let memory = self.write_remembered(draft, chunks)?;
        Ok(Remembered { memory, similar })
    }

    /// Embeds one draft's content into its (normalized) chunk vectors —
    /// everything about a `remember` that can run *before* its transaction.
    /// Empty on a KV-only store (no embedder).
    fn embed_draft_content(&self, content: &str) -> Result<Vec<Vec<f32>>> {
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        let mut chunks = embedder.embed_chunks(content)?;
        for vector in &mut chunks {
            index::normalize(vector);
        }
        Ok(chunks)
    }

    /// The near-duplicate scan of [`Store::remember_detailed`]: searches the
    /// committed index with the new content's own chunk vectors (before they
    /// are inserted, so the new memory never matches itself), keeps hits at
    /// or above [`NEAR_DUP_THRESHOLD`] that are live, non-superseded and in
    /// exactly the given project scope, best first, at most
    /// [`NEAR_DUP_LIMIT`]. A multi-chunk draft reports each existing memory
    /// once, under its best-matching chunk's score.
    fn near_duplicates(
        &self,
        chunks: &[Vec<f32>],
        project: Option<&str>,
    ) -> Result<Vec<SimilarMemory>> {
        let Some(embedder) = &self.embedder else {
            return Ok(Vec::new());
        };
        let hnsw_meta_page = self.pager.header().hnsw_meta_page;
        let node_count = index::node_count(&self.pager, hnsw_meta_page)?;
        if chunks.is_empty() || node_count == 0 {
            return Ok(Vec::new());
        }
        let root = self.pager.header().root_btree_page;
        let pager = &self.pager;
        let cache: std::cell::RefCell<BTreeMap<Ulid, Option<MemoryRecord>>> =
            std::cell::RefCell::new(BTreeMap::new());
        let load = |id: Ulid| -> Result<Option<MemoryRecord>> {
            if let Some(rec) = cache.borrow().get(&id) {
                return Ok(rec.clone());
            }
            let rec = match btree::get(pager, root, &id.to_bytes())? {
                Some(bytes) => Some(MemoryRecord::decode(&bytes)?),
                None => None,
            };
            cache.borrow_mut().insert(id, rec.clone());
            Ok(rec)
        };
        // Same-scope only, and exactly: a global draft compares against
        // global memories, a project draft against that project's — never
        // `Scope::All`. Liveness and the superseded flag are re-checked
        // against the record, like every other search (ADR 0003/0013).
        let keep = |id: Ulid| -> bool {
            matches!(
                load(id),
                Ok(Some(rec))
                    if !rec.tombstone && !rec.superseded && rec.project.as_deref() == project
            )
        };

        let mut best: BTreeMap<Ulid, f32> = BTreeMap::new();
        for vector in chunks {
            let hits = index::search(
                pager,
                hnsw_meta_page,
                embedder.dims(),
                vector,
                NEAR_DUP_LIMIT,
                SearchParams {
                    ef_search: index::default_ef_search(node_count),
                },
                &keep,
            )?;
            for hit in hits {
                if hit.score >= NEAR_DUP_THRESHOLD {
                    let entry = best.entry(hit.record_id).or_insert(hit.score);
                    if hit.score > *entry {
                        *entry = hit.score;
                    }
                }
            }
        }
        let mut ranked: Vec<(Ulid, f32)> = best.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(NEAR_DUP_LIMIT);
        let mut out = Vec::with_capacity(ranked.len());
        for (id, score) in ranked {
            if let Some(rec) = load(id)? {
                out.push(SimilarMemory {
                    id,
                    content: near_dup_snippet(&rec.content),
                    score,
                    created_at_micros: rec.provenance.created_at_micros,
                });
            }
        }
        Ok(out)
    }

    /// The transactional half of [`Store::remember`]: validates graph/version
    /// targets and writes the record, its pre-embedded `chunks`, full-text
    /// postings and graph data as one durable transaction.
    fn write_remembered(&mut self, draft: MemoryDraft, chunks: Vec<Vec<f32>>) -> Result<Memory> {
        let MemoryDraft {
            content,
            project,
            metadata,
            agent,
            session_id,
            entities,
            relations,
            supersedes,
        } = draft;
        let mut record = MemoryRecord {
            id: Ulid::new(),
            tombstone: false,
            superseded: false,
            content,
            vec_ref: None,
            project,
            provenance: Provenance {
                agent,
                session_id,
                created_at_micros: now_micros(),
            },
            metadata,
        };

        let mut txn = self.pager.begin()?;
        // Relation targets must exist and be live *now* (ADR 0012). Checked
        // inside the txn, before any write buffers up, so a failure rolls
        // back to exactly nothing.
        for (_, target) in &relations {
            let live = match btree::get(&txn, txn.root_btree_page(), &target.to_bytes())? {
                Some(bytes) => !MemoryRecord::decode(&bytes)?.tombstone,
                None => false,
            };
            if !live {
                return Err(Error::InvalidArgument(
                    "relation target does not exist or was forgotten",
                ));
            }
        }
        // Supersedes targets (S19, ADR 0013): same validate-before-any-write
        // discipline as relations — every target checked (exists, live, same
        // project) before the first flag is set, so a bad target in the list
        // rolls the whole remember back to exactly nothing. Duplicates are
        // deduplicated so one target never gets two "supersedes" edges.
        let mut supersedes_targets: Vec<(Ulid, MemoryRecord)> = Vec::new();
        for target in supersedes {
            if supersedes_targets.iter().any(|(id, _)| *id == target) {
                continue;
            }
            let target_rec = match btree::get(&txn, txn.root_btree_page(), &target.to_bytes())? {
                Some(bytes) => MemoryRecord::decode(&bytes)?,
                None => {
                    return Err(Error::InvalidArgument(
                        "supersedes target does not exist or was forgotten",
                    ));
                }
            };
            if target_rec.tombstone {
                return Err(Error::InvalidArgument(
                    "supersedes target does not exist or was forgotten",
                ));
            }
            if target_rec.project != record.project {
                return Err(Error::InvalidArgument(
                    "supersedes target belongs to a different project",
                ));
            }
            supersedes_targets.push((target, target_rec));
        }
        if let Some(embedder) = &self.embedder {
            // The chunks were embedded (and normalized) by the caller before
            // this transaction began — the same vectors a
            // `remember_detailed` near-duplicate scan already searched with,
            // so embedding happens exactly once per remember (S21).
            for vector in &chunks {
                let (page_no, slot) = index::insert(&mut txn, embedder.dims(), record.id, vector)?;
                if record.vec_ref.is_none() {
                    record.vec_ref = Some(VecRef { page_no, slot });
                }
            }
        }
        // Full-text index (B2, `docs/adr/0011`): same transaction as the
        // record and vector writes, so the memory is either fully indexed or
        // not stored at all — no torn state to recover into. Runs whether or
        // not an embedder is present: full-text is independent of vectors.
        index::fts::index_document(&mut txn, record.id, &record.content)?;
        // The graph gets the caller's relations plus one "supersedes" edge per
        // target (S19), so the version chain is navigable via `related` in
        // both directions. Exclusion is never derived from these edges — the
        // flag on the target's record is the authority (ADR 0013).
        let mut graph_relations = relations;
        for (target, _) in &supersedes_targets {
            graph_relations.push((SUPERSEDES_RELATION.to_owned(), *target));
        }
        index::graph::add_memory(&mut txn, record.id, &entities, &graph_relations)?;
        // Flag each superseded target in this same transaction: the new
        // version and the exclusion of the old one land atomically, or
        // neither does.
        for (target, target_rec) in &mut supersedes_targets {
            target_rec.superseded = true;
            btree::insert(&mut txn, target.to_bytes(), &target_rec.encode()?)?;
        }
        let bytes = record.encode()?;
        btree::insert(&mut txn, record.id.to_bytes(), &bytes)?;
        // Filter-meta sidecar (`docs/adr/0027`): one entry per record this
        // transaction wrote — the new memory and every re-flagged supersede
        // target — so the sidecar and the records commit atomically.
        let mut meta_updates = vec![filter_meta_update(&record)];
        meta_updates.extend(
            supersedes_targets
                .iter()
                .map(|(_, target_rec)| filter_meta_update(target_rec)),
        );
        index::filter_meta::record_updates(&mut txn, &meta_updates)?;
        txn.commit()?;
        Ok(Memory::from_record(record))
    }

    /// Hybrid recall: fuses vector similarity (HNSW) and full-text (BM25) with
    /// Reciprocal Rank Fusion (`k = 60`, `docs/adr/0005`, [`crate::recall`]),
    /// best fused rank first. A hit that appears in only one of the two lists
    /// still makes the result — fusion is a union, never an intersection, so a
    /// rare exact term or a semantic synonym is never dropped for lacking a
    /// match on the other side.
    ///
    /// Requires this store to have an [`Embedder`] (`StoreOptions::embedder` /
    /// [`Store::create`]); returns [`Error::InvalidArgument`] otherwise, since
    /// the vector half is mandatory. On an older `.mind` with **no full-text
    /// index** (a pre-M2 file), recall silently degrades to vector-only rather
    /// than erroring — use [`Store::recall_detailed`] to observe that the
    /// degradation happened. Tombstoned memories are always excluded
    /// (`docs/adr/0003`); `query.scope` additionally filters by project
    /// (DESIGN.md §7); the vector half keeps the adaptive `ef_search`
    /// anti-under-return guarantee of S2 (DESIGN §5).
    ///
    /// Each returned [`Recalled`] carries its **RRF** score (small, e.g.
    /// `~0.016` for a rank-0 hit), not a cosine similarity — the two source
    /// scales are intentionally discarded (`docs/adr/0005`).
    pub fn recall(&self, query: Query) -> Result<Vec<Recalled>> {
        Ok(self.recall_detailed(query)?.hits)
    }

    /// [`Store::recall`] plus a flag telling the caller whether recall had to
    /// fall back to vector-only because the file has no full-text index. The
    /// MCP/CLI shells use the flag to surface the "keyword search unavailable
    /// on this older store" warning; plain [`Store::recall`] hides it.
    pub fn recall_detailed(&self, query: Query) -> Result<RecallOutcome> {
        let Some(embedder) = &self.embedder else {
            return Err(Error::InvalidArgument(
                "this store has no embedder; recall requires one (see StoreOptions::embedder)",
            ));
        };
        let root = self.pager.header().root_btree_page;
        let pager = &self.pager;

        // Shared record cache: the vector filter, the text `keep`/`doc_len`
        // closures, and the final hit reconstruction all read the same records.
        // One B-tree read per id, reused everywhere (RefCell so the several
        // closures can borrow it mutably in turn).
        let cache: std::cell::RefCell<BTreeMap<Ulid, Option<MemoryRecord>>> =
            std::cell::RefCell::new(BTreeMap::new());
        let load = |id: Ulid| -> Result<Option<MemoryRecord>> {
            if let Some(rec) = cache.borrow().get(&id) {
                return Ok(rec.clone());
            }
            let rec = match btree::get(pager, root, &id.to_bytes())? {
                Some(bytes) => Some(MemoryRecord::decode(&bytes)?),
                None => None,
            };
            cache.borrow_mut().insert(id, rec.clone());
            Ok(rec)
        };

        // `keep`: live + in-scope + passes every metadata filter. Feeding the
        // filters into the *same* predicate the two searches use means the
        // adaptive `ef_search` anti-under-return guarantee (S2, DESIGN §5)
        // covers filtered results — a filter that excludes candidates makes
        // the search widen, never silently under-return. A filter type
        // mismatch is a typed error, but `keep` must yield a plain `bool`, so
        // the first such error is stashed here and surfaced after the search.
        //
        // The filter-meta sidecar (`docs/adr/0027`) answers most candidates
        // without touching the record B-tree; only undecidable ones (entry
        // missing, custom metadata filters over a record that has metadata)
        // fall through to the full record predicate below — same result,
        // fewer loads. `None` on a pre-sidecar file: pure record path.
        let meta = self.filter_meta()?;
        let meta = meta.as_ref().map(|t| (t, meta_needs(t, &query)));
        let filter_error: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        let keep = |id: Ulid| -> bool {
            if filter_error.borrow().is_some() {
                return false; // a mismatch already occurred; stop admitting
            }
            if let Some((table, needs)) = &meta {
                match table.decide(id, needs) {
                    index::filter_meta::Decision::Accept => return true,
                    index::filter_meta::Decision::Reject(_) => return false,
                    index::filter_meta::Decision::NeedRecord => {}
                }
            }
            match load(id) {
                Ok(Some(rec)) if in_scope(&query, &rec) => {
                    match query.record_passes_filters(&rec) {
                        Ok(pass) => pass,
                        Err(e) => {
                            *filter_error.borrow_mut() = Some(e);
                            false
                        }
                    }
                }
                _ => false,
            }
        };

        // --- Vector half (HNSW) ------------------------------------------
        let mut vector = embedder.embed(&query.text)?;
        index::normalize(&mut vector);
        let hnsw_meta_page = self.pager.header().hnsw_meta_page;
        // Resolve the effective ef_search here, where the live node count is
        // cheap to read: the caller's explicit override wins, else the default
        // scales with the graph size (S16, `docs/adr/0015`).
        let node_count = index::node_count(&self.pager, hnsw_meta_page)?;
        let vec_hits = index::search(
            &self.pager,
            hnsw_meta_page,
            embedder.dims(),
            &vector,
            query.limit,
            SearchParams {
                ef_search: query.effective_ef_search(node_count),
            },
            // Re-check liveness/scope/filters against the record itself: the
            // HNSW graph stores only record ids, never tombstone/project/
            // metadata state, which can change (forget) after indexing.
            &keep,
        )?;
        if let Some(e) = filter_error.borrow_mut().take() {
            return Err(e);
        }
        let vec_ids: Vec<Ulid> = vec_hits.iter().map(|h| h.record_id).collect();

        // --- Full-text half (BM25) ---------------------------------------
        // A file with no full-text index (fts_root_page == 0) is a pre-M2
        // store: skip the keyword search and degrade to vector-only, silently
        // but reported via the outcome flag. An *empty* index (root set, zero
        // docs) is not degradation — it just contributes nothing.
        let fts_root = self.pager.header().fts_root_page;
        let degraded_to_vector_only = fts_root == 0;
        let text_ids: Vec<Ulid> = if degraded_to_vector_only {
            Vec::new()
        } else {
            let text_hits = index::fts::search(
                &self.pager,
                fts_root,
                &query.text,
                query.limit,
                &keep,
                // BM25 doc_len from the sidecar when it has the id (content is
                // immutable, so the stored count never goes stale); the record
                // itself only for ids the sidecar has never seen.
                |id| {
                    if let Some((table, _)) = &meta
                        && let Some(entry) = table.get(id)
                    {
                        return Ok(Some(entry.doc_len));
                    }
                    Ok(load(id)?.map(|rec| index::fts::doc_len(&rec.content)))
                },
            )?;
            if let Some(e) = filter_error.borrow_mut().take() {
                return Err(e);
            }
            text_hits.iter().map(|h| h.record_id).collect()
        };

        // --- Optional recency list (S20, `docs/adr/0014`) -----------------
        // The exact same content candidates (union of vector + text, each
        // already live/in-scope/filter-checked by `keep`), reordered by
        // `created_at_micros` descending. This can only *break ties* among
        // candidates content search already found — it never introduces an
        // id outside the union, so it can't manufacture a hit out of pure
        // novelty (RRF's own math backs this: a single list's max
        // contribution is `1/(RRF_K + 1)`, always less than two content
        // lists agreeing — `recall.rs` module docs).
        let recency_ids: Vec<Ulid> = if query.recency {
            let mut union: Vec<Ulid> = vec_ids
                .iter()
                .copied()
                .chain(text_ids.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            union.sort_by_key(|id| {
                std::cmp::Reverse(
                    load(*id)
                        .ok()
                        .flatten()
                        .map(|rec| rec.provenance.created_at_micros)
                        .unwrap_or(i64::MIN),
                )
            });
            union
        } else {
            Vec::new()
        };

        // --- Fuse (RRF k=60, union) --------------------------------------
        let fused = crate::recall::fuse_lists(&[&vec_ids, &text_ids, &recency_ids], query.limit);
        let mut hits = Vec::with_capacity(fused.len());
        for f in fused {
            // Every fused id came from a list whose closure already loaded and
            // scope-checked it, so this is a cache hit, not a fresh read.
            if let Some(rec) = load(f.record_id)? {
                hits.push(Recalled {
                    memory: Memory::from_record(rec),
                    score: f.score,
                });
            }
        }

        // --- Optional 1-hop graph expansion (S13, `docs/adr/0012`) --------
        // Each direct hit's relation edges (both directions) pull connected
        // context: neighbors not already in the result, passing the same
        // `keep` (live + in-scope + filters) as the ranked halves, appended
        // *after* the direct hits with score 0.0 — they are connected
        // context, not ranked matches, and never displace one. One hop only:
        // neighbors of neighbors are not followed.
        if query.expand_related {
            let graph_root = self.pager.header().graph_root_page;
            let mut seen: std::collections::BTreeSet<Ulid> =
                hits.iter().map(|h| h.memory.id).collect();
            let direct: Vec<Ulid> = hits.iter().map(|h| h.memory.id).collect();
            for id in direct {
                let Some(adj) = index::graph::memory_graph(&self.pager, graph_root, id)? else {
                    continue;
                };
                for edge in adj.edges {
                    if seen.insert(edge.other)
                        && keep(edge.other)
                        && let Some(rec) = load(edge.other)?
                    {
                        hits.push(Recalled {
                            memory: Memory::from_record(rec),
                            score: 0.0,
                        });
                    }
                }
            }
            if let Some(e) = filter_error.borrow_mut().take() {
                return Err(e);
            }
        }

        Ok(RecallOutcome {
            hits,
            degraded_to_vector_only,
        })
    }

    /// Vector-only recall: the HNSW half of [`Store::recall`] with **no**
    /// full-text fusion — the pure nearest-neighbor list, live + in-scope,
    /// best first. This is the operation the benchmark harness grades against
    /// the brute-force baseline (`docs/BENCHMARKS.md` §3: `recall@10` measures
    /// the *index's* approximation quality, isolated from RRF fusion), and it
    /// is what recall degrades to on a pre-M2 file. Same embedder requirement,
    /// scope, tombstone re-check, and adaptive `ef_search` guarantee as
    /// [`Store::recall`]; the only difference is that BM25 never contributes.
    pub fn recall_vector(&self, query: Query) -> Result<Vec<Recalled>> {
        let Some(embedder) = &self.embedder else {
            return Err(Error::InvalidArgument(
                "this store has no embedder; recall requires one (see StoreOptions::embedder)",
            ));
        };
        let root = self.pager.header().root_btree_page;
        let pager = &self.pager;

        let cache: std::cell::RefCell<BTreeMap<Ulid, Option<MemoryRecord>>> =
            std::cell::RefCell::new(BTreeMap::new());
        let load = |id: Ulid| -> Result<Option<MemoryRecord>> {
            if let Some(rec) = cache.borrow().get(&id) {
                return Ok(rec.clone());
            }
            let rec = match btree::get(pager, root, &id.to_bytes())? {
                Some(bytes) => Some(MemoryRecord::decode(&bytes)?),
                None => None,
            };
            cache.borrow_mut().insert(id, rec.clone());
            Ok(rec)
        };

        // Same live + in-scope + metadata-filter `keep` as `recall_detailed`,
        // with the type-mismatch error stashed and surfaced after the search,
        // and the same sidecar fast path in front of the record load.
        let meta = self.filter_meta()?;
        let meta = meta.as_ref().map(|t| (t, meta_needs(t, &query)));
        let filter_error: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        let keep = |id: Ulid| -> bool {
            if filter_error.borrow().is_some() {
                return false;
            }
            if let Some((table, needs)) = &meta {
                match table.decide(id, needs) {
                    index::filter_meta::Decision::Accept => return true,
                    index::filter_meta::Decision::Reject(_) => return false,
                    index::filter_meta::Decision::NeedRecord => {}
                }
            }
            match load(id) {
                Ok(Some(rec)) if in_scope(&query, &rec) => {
                    match query.record_passes_filters(&rec) {
                        Ok(pass) => pass,
                        Err(e) => {
                            *filter_error.borrow_mut() = Some(e);
                            false
                        }
                    }
                }
                _ => false,
            }
        };

        let mut vector = embedder.embed(&query.text)?;
        index::normalize(&mut vector);
        let hnsw_meta_page = self.pager.header().hnsw_meta_page;
        let node_count = index::node_count(&self.pager, hnsw_meta_page)?;
        let vec_hits = index::search(
            &self.pager,
            hnsw_meta_page,
            embedder.dims(),
            &vector,
            query.limit,
            SearchParams {
                ef_search: query.effective_ef_search(node_count),
            },
            &keep,
        )?;
        if let Some(e) = filter_error.borrow_mut().take() {
            return Err(e);
        }

        // Reuse the RRF scorer with an empty text list so a vector-only hit
        // carries the same score it would in a degraded hybrid recall.
        let vec_ids: Vec<Ulid> = vec_hits.iter().map(|h| h.record_id).collect();
        let fused = crate::recall::fuse(&vec_ids, &[], query.limit);
        let mut hits = Vec::with_capacity(fused.len());
        for f in fused {
            if let Some(rec) = load(f.record_id)? {
                hits.push(Recalled {
                    memory: Memory::from_record(rec),
                    score: f.score,
                });
            }
        }
        Ok(hits)
    }

    /// Full-text (BM25) search over `remember`ed content — the keyword half
    /// of hybrid recall (`docs/adr/0011`, roadmap 2.3). Best score first, each
    /// hit carrying its BM25 score. Needs no embedder (full-text is
    /// independent of vectors); on a store or file with no full-text index
    /// (a pre-M2 `.mind`) it returns an empty result rather than an error, so
    /// the degradation is silent and safe. Tombstoned memories are excluded
    /// and `query`'s scope filters by project, exactly like [`Store::recall`].
    pub fn search_text(&self, query: Query) -> Result<Vec<Recalled>> {
        let root = self.pager.header().root_btree_page;
        let fts_root = self.pager.header().fts_root_page;
        let pager = &self.pager;
        // Cache each candidate's record so `keep` and `doc_len` — two separate
        // closures `fts::search` may both call for one id — share a single
        // B-tree read. `RefCell` because both closures borrow it mutably.
        let cache: std::cell::RefCell<BTreeMap<Ulid, Option<MemoryRecord>>> =
            std::cell::RefCell::new(BTreeMap::new());
        let load = |id: Ulid| -> Result<Option<MemoryRecord>> {
            if let Some(rec) = cache.borrow().get(&id) {
                return Ok(rec.clone());
            }
            let rec = match btree::get(pager, root, &id.to_bytes())? {
                Some(bytes) => Some(MemoryRecord::decode(&bytes)?),
                None => None,
            };
            cache.borrow_mut().insert(id, rec.clone());
            Ok(rec)
        };

        // keep: live + in-scope + metadata filters (same re-check the vector
        // path does); a filter type mismatch is stashed and surfaced below.
        // The filter-meta sidecar answers most candidates without a record
        // load (`docs/adr/0027`); undecidable ones fall through to it.
        let meta = self.filter_meta()?;
        let meta = meta.as_ref().map(|t| (t, meta_needs(t, &query)));
        let filter_error: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        let hits = index::fts::search(
            &self.pager,
            fts_root,
            &query.text,
            query.limit,
            |id| {
                if filter_error.borrow().is_some() {
                    return false;
                }
                if let Some((table, needs)) = &meta {
                    match table.decide(id, needs) {
                        index::filter_meta::Decision::Accept => return true,
                        index::filter_meta::Decision::Reject(_) => return false,
                        index::filter_meta::Decision::NeedRecord => {}
                    }
                }
                match load(id) {
                    Ok(Some(rec)) if in_scope(&query, &rec) => {
                        match query.record_passes_filters(&rec) {
                            Ok(pass) => pass,
                            Err(e) => {
                                *filter_error.borrow_mut() = Some(e);
                                false
                            }
                        }
                    }
                    _ => false,
                }
            },
            // doc_len: BM25 length normalization — from the sidecar when it
            // has the id (content is immutable, the count never goes stale),
            // else from the current content.
            |id| {
                if let Some((table, _)) = &meta
                    && let Some(entry) = table.get(id)
                {
                    return Ok(Some(entry.doc_len));
                }
                Ok(load(id)?.map(|rec| index::fts::doc_len(&rec.content)))
            },
        )?;
        if let Some(e) = filter_error.borrow_mut().take() {
            return Err(e);
        }

        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            if let Some(rec) = load(hit.record_id)? {
                out.push(Recalled {
                    memory: Memory::from_record(rec),
                    score: hit.score,
                });
            }
        }
        Ok(out)
    }

    /// [`Store::search_text`], instrumented phase-by-phase — measurement-only
    /// surface for story FT1 (`docs/adr/0017`), never called by production
    /// `recall`/`search_text`. Exists so `benches/` can isolate where the
    /// full-text half of hybrid recall spends its time on a real `.mind` file
    /// through the same public `Store` boundary the harness already uses,
    /// without exposing the internal pager to the bench crate.
    #[doc(hidden)]
    pub fn search_text_profiled(
        &self,
        query: Query,
    ) -> Result<(Vec<Recalled>, index::fts::SearchPhaseTimings)> {
        let root = self.pager.header().root_btree_page;
        let fts_root = self.pager.header().fts_root_page;
        let pager = &self.pager;
        let cache: std::cell::RefCell<BTreeMap<Ulid, Option<MemoryRecord>>> =
            std::cell::RefCell::new(BTreeMap::new());
        let load = |id: Ulid| -> Result<Option<MemoryRecord>> {
            if let Some(rec) = cache.borrow().get(&id) {
                return Ok(rec.clone());
            }
            let rec = match btree::get(pager, root, &id.to_bytes())? {
                Some(bytes) => Some(MemoryRecord::decode(&bytes)?),
                None => None,
            };
            cache.borrow_mut().insert(id, rec.clone());
            Ok(rec)
        };

        // The same sidecar fast path production `search_text` uses, so the
        // timings this surface reports keep describing the production path;
        // the sidecar's reject reason feeds the same outcome buckets.
        let meta = self.filter_meta()?;
        let meta = meta.as_ref().map(|t| (t, meta_needs(t, &query)));
        let filter_error: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        let (hits, timings) = index::fts::search_profiled(
            &self.pager,
            fts_root,
            &query.text,
            query.limit,
            |id| {
                use index::filter_meta::{Decision, RejectReason};
                use index::fts::KeepOutcome;
                if filter_error.borrow().is_some() {
                    return KeepOutcome::Tombstoned;
                }
                if let Some((table, needs)) = &meta {
                    match table.decide(id, needs) {
                        Decision::Accept => return KeepOutcome::Accepted,
                        Decision::Reject(RejectReason::Dead) => return KeepOutcome::Tombstoned,
                        Decision::Reject(RejectReason::OutOfScope) => {
                            return KeepOutcome::OutOfScope;
                        }
                        Decision::Reject(RejectReason::FilteredOut) => {
                            return KeepOutcome::FilteredOut;
                        }
                        Decision::NeedRecord => {}
                    }
                }
                match load(id) {
                    Ok(Some(rec)) if !rec.tombstone && !rec.superseded => {
                        if !in_scope(&query, &rec) {
                            KeepOutcome::OutOfScope
                        } else {
                            match query.record_passes_filters(&rec) {
                                Ok(true) => KeepOutcome::Accepted,
                                Ok(false) => KeepOutcome::FilteredOut,
                                Err(e) => {
                                    *filter_error.borrow_mut() = Some(e);
                                    KeepOutcome::FilteredOut
                                }
                            }
                        }
                    }
                    _ => KeepOutcome::Tombstoned,
                }
            },
            |id| {
                if let Some((table, _)) = &meta
                    && let Some(entry) = table.get(id)
                {
                    return Ok(Some(entry.doc_len));
                }
                Ok(load(id)?.map(|rec| index::fts::doc_len(&rec.content)))
            },
        )?;
        if let Some(e) = filter_error.borrow_mut().take() {
            return Err(e);
        }

        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            if let Some(rec) = load(hit.record_id)? {
                out.push(Recalled {
                    memory: Memory::from_record(rec),
                    score: hit.score,
                });
            }
        }
        Ok((out, timings))
    }

    /// [`Store::search_text`] via the BlockMax-WAND path directly, returning
    /// [`index::fts::BmwCounters`] alongside the hits — measurement-only
    /// surface for story BMW-3 (`docs/adr/0025`), never called by production
    /// `recall`/`search_text` (those go through [`index::fts::search`], which
    /// dispatches to BMW or the linear scan by `format_version` on its own).
    /// Exists so `benches/` can tell, per query, whether any matched term's
    /// postings list actually carried a skip index (`block_count > 0`) or
    /// every term was small enough to be decoded whole — the question BMW-3
    /// needs answered before reading a flat p99 as "BMW had no effect".
    #[doc(hidden)]
    pub fn search_text_bmw_counted(
        &self,
        query: Query,
    ) -> Result<(Vec<Recalled>, index::fts::BmwCounters)> {
        let root = self.pager.header().root_btree_page;
        let fts_root = self.pager.header().fts_root_page;
        let pager = &self.pager;
        let cache: std::cell::RefCell<BTreeMap<Ulid, Option<MemoryRecord>>> =
            std::cell::RefCell::new(BTreeMap::new());
        let load = |id: Ulid| -> Result<Option<MemoryRecord>> {
            if let Some(rec) = cache.borrow().get(&id) {
                return Ok(rec.clone());
            }
            let rec = match btree::get(pager, root, &id.to_bytes())? {
                Some(bytes) => Some(MemoryRecord::decode(&bytes)?),
                None => None,
            };
            cache.borrow_mut().insert(id, rec.clone());
            Ok(rec)
        };

        // Same sidecar fast path as production `search_text`, so the BMW
        // counters this surface reports keep describing the production path.
        let meta = self.filter_meta()?;
        let meta = meta.as_ref().map(|t| (t, meta_needs(t, &query)));
        let filter_error: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        let (hits, counters) = index::fts::search_bmw_counted(
            &self.pager,
            fts_root,
            &query.text,
            query.limit,
            |id| {
                if filter_error.borrow().is_some() {
                    return false;
                }
                if let Some((table, needs)) = &meta {
                    match table.decide(id, needs) {
                        index::filter_meta::Decision::Accept => return true,
                        index::filter_meta::Decision::Reject(_) => return false,
                        index::filter_meta::Decision::NeedRecord => {}
                    }
                }
                match load(id) {
                    Ok(Some(rec)) if in_scope(&query, &rec) => {
                        match query.record_passes_filters(&rec) {
                            Ok(pass) => pass,
                            Err(e) => {
                                *filter_error.borrow_mut() = Some(e);
                                false
                            }
                        }
                    }
                    _ => false,
                }
            },
            |id| {
                if let Some((table, _)) = &meta
                    && let Some(entry) = table.get(id)
                {
                    return Ok(Some(entry.doc_len));
                }
                Ok(load(id)?.map(|rec| index::fts::doc_len(&rec.content)))
            },
        )?;
        if let Some(e) = filter_error.borrow_mut().take() {
            return Err(e);
        }

        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            if let Some(rec) = load(hit.record_id)? {
                out.push(Recalled {
                    memory: Memory::from_record(rec),
                    score: hit.score,
                });
            }
        }
        Ok((out, counters))
    }

    /// Fetches one memory by id. Tombstoned (forgotten) memories return
    /// `None`, exactly like absent ones. Superseded memories (S19) **are**
    /// returned — they are history, hidden from recall but not from a direct
    /// read; check [`Memory::superseded`] to tell.
    pub fn get(&self, id: Ulid) -> Result<Option<Memory>> {
        let root = self.pager.header().root_btree_page;
        match btree::get(&self.pager, root, &id.to_bytes())? {
            None => Ok(None),
            Some(bytes) => {
                let record = MemoryRecord::decode(&bytes)?;
                Ok((!record.tombstone).then(|| Memory::from_record(record)))
            }
        }
    }

    /// Soft-deletes one memory (sets the tombstone; space is reclaimed by
    /// `embedmind vacuum`, `docs/adr/0003`). Returns `false` if the id does
    /// not exist or was already forgotten — nothing is written in that case.
    pub fn forget(&mut self, id: Ulid) -> Result<bool> {
        let key = id.to_bytes();
        let mut txn = self.pager.begin()?;
        let Some(bytes) = btree::get(&txn, txn.root_btree_page(), &key)? else {
            return Ok(false);
        };
        let mut record = MemoryRecord::decode(&bytes)?;
        if record.tombstone {
            return Ok(false); // txn drops: rollback, nothing written
        }
        record.tombstone = true;
        btree::insert(&mut txn, key, &record.encode()?)?;
        // The sidecar mirrors the tombstone in the same transaction
        // (`docs/adr/0027`), so `keep` never resurrects a forgotten memory.
        index::filter_meta::record_updates(&mut txn, &[filter_meta_update(&record)])?;
        txn.commit()?;
        Ok(true)
    }

    /// Memories related to `id` through explicit relation edges, both
    /// directions (S13, `docs/adr/0012`). Each hit carries the relation kind
    /// and whether the edge points out of `id` or into it. Tombstoned
    /// neighbors are re-checked at query time and never returned — a relation
    /// to a forgotten memory disappears with the tombstone. Superseded
    /// neighbors (S19) **are** returned: the [`SUPERSEDES_RELATION`] edge is
    /// how the version history stays navigable. Empty when `id` has no graph
    /// data or the file predates the graph layer (`graph_root_page == 0`) —
    /// older files degrade, never error.
    pub fn related(&self, id: Ulid) -> Result<Vec<RelatedMemory>> {
        let graph_root = self.pager.header().graph_root_page;
        let Some(adj) = index::graph::memory_graph(&self.pager, graph_root, id)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for edge in adj.edges {
            if let Some(memory) = self.get(edge.other)? {
                out.push(RelatedMemory {
                    memory,
                    kind: edge.kind,
                    outgoing: edge.outgoing,
                });
            }
        }
        Ok(out)
    }

    /// Live memories tagged with `entity`, in id (time) order — the
    /// `related(entity)` navigation of S13. Same liveness re-check and
    /// old-file degradation as [`Store::related`].
    pub fn entity_members(&self, entity: &str) -> Result<Vec<Memory>> {
        let graph_root = self.pager.header().graph_root_page;
        let mut out = Vec::new();
        for id in index::graph::entity_members(&self.pager, graph_root, entity)? {
            if let Some(memory) = self.get(id)? {
                out.push(memory);
            }
        }
        Ok(out)
    }

    /// The entity tags stored for one memory, sorted ascending. Empty when
    /// the memory has none (or the file has no graph layer).
    pub fn entities_of(&self, id: Ulid) -> Result<Vec<String>> {
        let graph_root = self.pager.header().graph_root_page;
        Ok(index::graph::memory_graph(&self.pager, graph_root, id)?
            .map(|g| g.entities)
            .unwrap_or_default())
    }

    /// Reclaims the space held by forgotten memories and rebuilds the indexes
    /// (`docs/adr/0003`, story S11). `forget` only tombstones — the record, its
    /// vector slots and its full-text postings all stay on disk, filtered out
    /// of every read but still occupying pages. `vacuum` is how that space
    /// comes back.
    ///
    /// **Rebuild by copy, never in place.** A fresh `.mind` is built in a
    /// sibling temp file, each *live* memory re-inserted (record preserved
    /// byte-for-byte — same id, provenance, metadata; vectors and full-text
    /// re-derived by the same embedder/tokenizer, so the new indexes hold only
    /// the living), then the temp file is swapped over the original with a
    /// single atomic rename. Consequences:
    ///
    /// - **Crash-safe at every point.** Until the final rename, the original is
    ///   untouched; a crash mid-vacuum leaves it fully intact and orphans the
    ///   temp file, which the next `open`/`vacuum` sweeps away. The rename
    ///   itself is atomic ([`Vfs::rename`]), so there is no torn-file window.
    /// - **Result is ≤ the original.** No tombstones, no orphaned overflow
    ///   chains, freshly packed indexes — the file can only shrink or stay the
    ///   same (a store with nothing forgotten still round-trips smaller-or-equal).
    ///
    /// Requires an embedder when the store has vectors to rebuild — a vacuum
    /// without one could not reconstruct the HNSW graph. On success the store's
    /// own pager is reopened onto the vacuumed file, so the `Store` stays usable.
    pub fn vacuum(&mut self) -> Result<()> {
        let tmp = vacuum_tmp_path(&self.path);
        let tmp_wal = wal_sidecar_path(&tmp);
        let scratch = vacuum_scratch_path(&self.path);
        let orig_wal = wal_sidecar_path(&self.path);
        // Clear any stale temp/scratch files from an earlier crashed vacuum;
        // they are dead by definition (the original is authoritative). A stale
        // temp would otherwise block the `CreateNew` in `build_compacted`.
        for orphan in [&tmp, &tmp_wal, &scratch, &wal_sidecar_path(&scratch)] {
            if self.vfs.exists(orphan) {
                self.vfs.delete(orphan)?;
            }
        }

        // Flush the original's WAL into its main file *first*, so by swap time
        // the original is a single self-consistent file with an empty WAL.
        // This is the same crash-safe checkpoint every clean close performs; it
        // matters here because the swap then never has to rewrite the original
        // (a torn header rewrite with a reset WAL would be unrecoverable). All
        // this touches is the original — a crash here is an ordinary "crash
        // during checkpoint", fully covered by WAL recovery.
        self.pager.checkpoint()?;

        // Build the compacted copy, cleaning up the temp files on any failure so
        // a mid-rebuild error never leaves an orphan behind.
        if let Err(e) = self.build_compacted(&tmp) {
            self.vfs.delete(&tmp).ok();
            if self.vfs.exists(&tmp_wal) {
                self.vfs.delete(&tmp_wal).ok();
            }
            return Err(e);
        }

        // `build_compacted` closed the temp store cleanly, so nothing holds
        // `tmp`. The atomic swap needs *no* open handle on either the original
        // or `tmp` (Windows will not rename over/onto an open file), yet
        // `self.pager` must always own a valid `Pager` — so park it on a
        // throwaway scratch store while both are released.
        let opts = StoreOptions {
            page_size: self.pager.header().page_size,
            checkpoint_threshold: self.checkpoint_threshold,
            embedder: self.embedder.clone(),
        };
        let parked = Pager::create(Arc::clone(&self.vfs), &scratch, opts.pager())?;
        let original = std::mem::replace(&mut self.pager, parked);
        // Release the original *without* checkpointing — its WAL is already
        // empty (we checkpointed above), so dropping only frees the writer
        // lock and writes nothing. Deleting the empty WAL leaves the original
        // path holding a single, fully consistent file.
        drop(original);
        if self.vfs.exists(&orig_wal) {
            self.vfs.delete(&orig_wal).ok();
        }

        // The atomic swap. Until this rename, the original file is fully in
        // place (the vacuumed data lives only in the separate `tmp`); the
        // rename itself is atomic ([`Vfs::rename`]), so a crash yields either
        // the intact original or the finished vacuumed file, never a torn mix.
        self.vfs.rename(&tmp, &self.path).map_err(Error::Io)?;
        if self.vfs.exists(&tmp_wal) {
            self.vfs.delete(&tmp_wal).ok();
        }

        // Reopen on the final path (recovery is a no-op — `tmp` was cleanly
        // closed, so it has no WAL), then tear down the scratch store.
        let reopened = Pager::open(Arc::clone(&self.vfs), &self.path, opts.pager())?;
        let parked = std::mem::replace(&mut self.pager, reopened);
        parked.close().ok();
        self.vfs.delete(&scratch).ok();
        // The rebuilt file restarts its txn_counter, so the counter-keyed
        // filter-meta cache could look "current" while holding the old
        // file's table — drop it explicitly.
        if let Ok(mut guard) = self.filter_meta_cache.write() {
            *guard = None;
        }
        Ok(())
    }

    /// Writes every live memory of this store into a brand-new `.mind` at
    /// `dest`, preserving each record exactly and re-deriving its vector and
    /// full-text entries. The destination is cleanly closed (single file, no
    /// WAL) on success. Shared by [`Store::vacuum`].
    fn build_compacted(&self, dest: &Path) -> Result<()> {
        let opts = StoreOptions {
            page_size: self.pager.header().page_size,
            checkpoint_threshold: self.checkpoint_threshold,
            embedder: self.embedder.clone(),
        };
        let mut dst = Store::create_with(Arc::clone(&self.vfs), dest, opts)?;
        let graph_root = self.pager.header().graph_root_page;
        for memory in self.iter() {
            let memory = memory?;
            // Graph data survives the rebuild filtered to the living (ADR
            // 0012): dead memories' entities never come over, and an edge is
            // kept only when *both* ends are live. Only the outgoing half is
            // re-inserted — `add_memory` mirrors the incoming half at the
            // target, so each surviving relation is written exactly once.
            let (entities, relations) =
                match index::graph::memory_graph(&self.pager, graph_root, memory.id)? {
                    Some(g) => {
                        let mut relations = Vec::new();
                        for edge in g.edges {
                            if edge.outgoing && self.get(edge.other)?.is_some() {
                                relations.push((edge.kind, edge.other));
                            }
                        }
                        (g.entities, relations)
                    }
                    None => (Vec::new(), Vec::new()),
                };
            // Reconstruct the on-disk record; `iter` already filtered
            // tombstones. The superseded flag comes over verbatim — superseded
            // memories are history, not garbage, and vacuum preserves them
            // (S19, ADR 0013).
            let record = MemoryRecord {
                id: memory.id,
                tombstone: false,
                superseded: memory.superseded,
                content: memory.content,
                vec_ref: None,
                project: memory.project,
                provenance: memory.provenance,
                metadata: memory.metadata,
            };
            dst.insert_record(record, &entities, &relations)?;
        }
        dst.close()
    }

    /// Inserts a fully-formed [`MemoryRecord`] verbatim (id, provenance and
    /// metadata preserved), re-deriving its vector, full-text and graph index
    /// entries — the write half of [`Store::vacuum`]'s rebuild. Unlike
    /// [`Store::remember`] it neither mints a new id nor a timestamp, and it
    /// does **not** re-validate relation targets: the caller pre-filtered
    /// them to live memories, which may simply not be re-inserted yet.
    fn insert_record(
        &mut self,
        mut record: MemoryRecord,
        entities: &[String],
        relations: &[(String, Ulid)],
    ) -> Result<Memory> {
        let mut txn = self.pager.begin()?;
        if let Some(embedder) = &self.embedder {
            for mut vector in embedder.embed_chunks(&record.content)? {
                index::normalize(&mut vector);
                let (page_no, slot) = index::insert(&mut txn, embedder.dims(), record.id, &vector)?;
                if record.vec_ref.is_none() {
                    record.vec_ref = Some(VecRef { page_no, slot });
                }
            }
        }
        index::fts::index_document(&mut txn, record.id, &record.content)?;
        index::graph::add_memory(&mut txn, record.id, entities, relations)?;
        let bytes = record.encode()?;
        btree::insert(&mut txn, record.id.to_bytes(), &bytes)?;
        // Sidecar entry in the same transaction (`docs/adr/0027`) — this is
        // also how `vacuum` upgrades a pre-sidecar file: the rebuilt copy is
        // written at the current format_version, sidecar included.
        index::filter_meta::record_updates(&mut txn, &[filter_meta_update(&record)])?;
        txn.commit()?;
        Ok(Memory::from_record(record))
    }

    /// Iterates live memories in id order — which is time order (ULIDs), so
    /// this is the timeline. Yields typed errors on a corrupt file instead
    /// of panicking.
    pub fn iter(&self) -> MemoryIter<'_> {
        MemoryIter {
            inner: btree::scan(&self.pager, self.pager.header().root_btree_page),
            include_tombstones: false,
        }
    }

    /// Like [`Store::iter`], but includes tombstoned memories — for `stats`,
    /// `vacuum` and tests.
    pub fn iter_all(&self) -> MemoryIter<'_> {
        MemoryIter {
            inner: btree::scan(&self.pager, self.pager.header().root_btree_page),
            include_tombstones: true,
        }
    }

    /// Counts and sizes for `embedmind stats` (README quickstart). Walks the
    /// whole record tree — O(N), fine for a diagnostics command, not meant
    /// for hot paths.
    pub fn stats(&self) -> Result<StoreStats> {
        let mut live_memories = 0u64;
        let mut forgotten_memories = 0u64;
        // Provenance breakdown (S14): one bucket per writing agent, counting
        // only live memories (forgotten ones are on their way out and would
        // skew the picture of "who has memories now"). Distinct sessions per
        // agent come along for free.
        let mut by_agent: BTreeMap<String, AgentStats> = BTreeMap::new();
        for memory in self.iter_all() {
            let memory = memory?;
            if memory.tombstone {
                forgotten_memories += 1;
                continue;
            }
            live_memories += 1;
            let bucket = by_agent.entry(memory.provenance.agent).or_default();
            bucket.live_memories += 1;
            if let Some(session) = memory.provenance.session_id {
                bucket.sessions.insert(session);
            }
        }
        let header = self.pager.header();
        let (graph_entities, graph_relations) =
            index::graph::stats(&self.pager, header.graph_root_page)?;
        Ok(StoreStats {
            live_memories,
            forgotten_memories,
            by_agent,
            index_entries: index::node_count(&self.pager, header.hnsw_meta_page)?,
            fts_documents: index::fts::indexed_documents(&self.pager, header.fts_root_page)?,
            graph_entities,
            graph_relations,
            page_size: header.page_size,
            page_count: header.page_count,
            file_bytes: u64::from(header.page_size) * header.page_count,
            embedding_model_id: (!header.embedding_model_id.is_empty())
                .then(|| header.embedding_model_id.clone()),
            embedding_dims: header.embedding_dims,
        })
    }

    /// Cleanly closes the store: checkpoint + WAL removal, leaving a single
    /// file on disk. Dropping without closing is safe (recovery handles it);
    /// closing is just tidier.
    pub fn close(self) -> Result<()> {
        self.pager.close()
    }

    /// Last committed transaction id — diagnostics and the crash harness.
    #[doc(hidden)]
    pub fn txn_counter(&self) -> u64 {
        self.pager.header().txn_counter
    }

    /// Checks the filter-meta sidecar against the records it mirrors — the
    /// crash-harness invariant of `docs/adr/0027`: after any recovery, every
    /// record (tombstoned included) has a sidecar entry whose liveness
    /// decision, scope symbols and `doc_len` agree with the record itself.
    /// A pre-sidecar file (root 0) passes trivially. Never called by
    /// production paths; measurement/test surface only.
    #[doc(hidden)]
    pub fn verify_filter_meta_invariant(&self) -> Result<()> {
        use index::filter_meta::{Decision, QueryNeeds, RejectReason, Want};
        let Some(table) = self.filter_meta()? else {
            return Ok(());
        };
        for memory in self.iter_all() {
            let m = memory?;
            let entry = index::filter_meta::Table::get(&table, m.id)
                .ok_or(Error::Internal("filter-meta entry missing for a record"))?;
            if entry.doc_len != index::fts::doc_len(&m.content) {
                return Err(Error::Internal("filter-meta doc_len disagrees"));
            }
            let dead = m.tombstone || m.superseded;
            let unscoped = QueryNeeds {
                project: Want::Any,
                agent: Want::Any,
                has_metadata_filters: false,
            };
            match table.decide(m.id, &unscoped) {
                Decision::Accept if !dead => {}
                Decision::Reject(RejectReason::Dead) if dead => {}
                _ => return Err(Error::Internal("filter-meta liveness disagrees")),
            }
            if dead {
                continue; // scope wants below only ever see live entries
            }
            // The record's own project/agent must never be rejected; a
            // never-interned string must never be accepted. (A global record
            // has no project of its own — `Scope::All` stands in for it.)
            let own = QueryNeeds {
                project: table.want_project(m.project.as_deref()),
                agent: table.want_agent(Some(&m.provenance.agent)),
                has_metadata_filters: false,
            };
            if matches!(table.decide(m.id, &own), Decision::Reject(_)) {
                return Err(Error::Internal("filter-meta rejects a record's own scope"));
            }
            let foreign = QueryNeeds {
                project: Want::Absent,
                agent: Want::Any,
                has_metadata_filters: false,
            };
            if matches!(table.decide(m.id, &foreign), Decision::Accept) {
                return Err(Error::Internal("filter-meta accepts an absent project"));
            }
        }
        Ok(())
    }

    /// The filter-meta sidecar (`docs/adr/0027`) materialized for the current
    /// committed state, or `None` on a pre-sidecar file (root 0) — the `keep`
    /// closures then fall back to the full record load, exactly the pre-FTOPT-1
    /// behavior. Rebuilt only when `txn_counter` moved since the cached build,
    /// so query bursts between writes pay for one materialization.
    fn filter_meta(&self) -> Result<Option<Arc<index::filter_meta::Table>>> {
        let header = self.pager.header();
        if header.filter_meta_page == 0 {
            return Ok(None);
        }
        let stamp = header.txn_counter;
        if let Ok(guard) = self.filter_meta_cache.read()
            && let Some((cached_stamp, table)) = guard.as_ref()
            && *cached_stamp == stamp
        {
            return Ok(Some(Arc::clone(table)));
        }
        let table = Arc::new(index::filter_meta::load(
            &self.pager,
            header.filter_meta_page,
            header.filter_symbols_page,
        )?);
        if let Ok(mut guard) = self.filter_meta_cache.write() {
            *guard = Some((stamp, Arc::clone(&table)));
        }
        Ok(Some(table))
    }
}

/// Whether a record is live, not superseded, within `query`'s project scope,
/// and written by the queried agent (when an agent filter is set, S14) — the
/// shared liveness/scope half of every `recall`/`search_text` `keep`
/// predicate. Superseded memories (S19, `docs/adr/0013`) are excluded here,
/// re-checked against the record at query time exactly like tombstones —
/// exclusion is never trusted to an index or to the graph. Metadata filters
/// ([`Query::record_passes_filters`]) compose on top of this.
/// The sidecar mirror of one record as it is being written — every write
/// path (`remember`, `forget`, supersede, vacuum's `insert_record`) derives
/// its filter-meta update from the exact record bytes it stores, in the same
/// transaction, so the two can never diverge (`docs/adr/0027`).
fn filter_meta_update(rec: &MemoryRecord) -> index::filter_meta::Update<'_> {
    index::filter_meta::Update {
        id: rec.id,
        tombstone: rec.tombstone,
        superseded: rec.superseded,
        has_metadata: !rec.metadata.is_empty(),
        project: rec.project.as_deref(),
        agent: &rec.provenance.agent,
        doc_len: index::fts::doc_len(&rec.content),
    }
}

/// Resolves a query's scope/agent strings against the sidecar's symbol table
/// **once per query**, so the per-candidate [`index::filter_meta::Table::decide`]
/// compares plain `u32`s.
fn meta_needs(table: &index::filter_meta::Table, query: &Query) -> index::filter_meta::QueryNeeds {
    index::filter_meta::QueryNeeds {
        project: match &query.scope {
            Scope::All => table.want_project(None),
            Scope::Project(p) => table.want_project(Some(p)),
        },
        agent: table.want_agent(query.agent.as_deref()),
        has_metadata_filters: !query.filters.is_empty(),
    }
}

fn in_scope(query: &Query, rec: &MemoryRecord) -> bool {
    !rec.tombstone
        && !rec.superseded
        && match &query.scope {
            Scope::All => true,
            Scope::Project(p) => rec.project.as_deref() == Some(p.as_str()),
        }
        && match &query.agent {
            None => true,
            Some(agent) => &rec.provenance.agent == agent,
        }
}

/// Where [`Store::vacuum`] builds the compacted copy: a sibling of the store
/// file. Kept adjacent (same directory) so the final rename is same-filesystem
/// and therefore atomic.
fn vacuum_tmp_path(path: &Path) -> PathBuf {
    sibling(path, "-vacuum-tmp")
}

/// Throwaway store [`Store::vacuum`] parks its live pager on during the swap,
/// so `self.pager` is never invalid while original and temp are both closed.
fn vacuum_scratch_path(path: &Path) -> PathBuf {
    sibling(path, "-vacuum-scratch")
}

/// WAL sidecar path for `path` — mirrors the pager's own `memory.mind` →
/// `memory.mind-wal` convention (`docs/FORMAT.md` §1), used to sweep a temp
/// store's sidecar after a clean close.
fn wal_sidecar_path(path: &Path) -> PathBuf {
    sibling(path, "-wal")
}

/// `path` with `suffix` appended to its file name (byte-appended, like the WAL
/// convention), keeping it in the same directory.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Current time in microseconds since the Unix epoch (UTC). Saturates
/// instead of failing on absurd clocks.
fn now_micros() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(before_epoch) => {
            i64::try_from(before_epoch.duration().as_micros()).map_or(i64::MIN, |v| -v)
        }
    }
}

/// What the caller provides to [`Store::remember`]. Build with
/// [`MemoryDraft::new`] plus the chainable setters.
#[derive(Debug, Clone)]
pub struct MemoryDraft {
    content: String,
    project: Option<String>,
    metadata: BTreeMap<String, Scalar>,
    agent: String,
    session_id: Option<String>,
    entities: Vec<String>,
    relations: Vec<(String, Ulid)>,
    supersedes: Vec<Ulid>,
}

impl MemoryDraft {
    /// A draft holding just the memory text. Shells should also set
    /// [`MemoryDraft::agent`] — basic provenance is part of the free tier.
    pub fn new(content: impl Into<String>) -> Self {
        MemoryDraft {
            content: content.into(),
            project: None,
            metadata: BTreeMap::new(),
            agent: String::new(),
            session_id: None,
            entities: Vec::new(),
            relations: Vec::new(),
            supersedes: Vec::new(),
        }
    }

    /// Scopes the memory to a project (see DESIGN.md §7).
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Records which agent is writing (`"claude-code"`, `"cli"`, …).
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = agent.into();
        self
    }

    /// Records the agent session id.
    pub fn session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Attaches one typed metadata entry (last write per key wins).
    pub fn meta(mut self, key: impl Into<String>, value: Scalar) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Tags the memory with one explicit entity ("postgres", "auth-service",
    /// …; 1–128 bytes of UTF-8) — S13, `docs/adr/0012`. Entities are
    /// caller-provided, never extracted automatically. Duplicates are
    /// deduplicated at write time. Navigate back with
    /// [`Store::entity_members`].
    pub fn entity(mut self, name: impl Into<String>) -> Self {
        self.entities.push(name.into());
        self
    }

    /// Replaces the whole entity list at once — the seam the shells use
    /// after parsing an `entities` argument.
    pub fn entities(mut self, entities: Vec<String>) -> Self {
        self.entities = entities;
        self
    }

    /// Adds one typed relation (`kind`: 1–64 bytes, e.g. `"refines"`,
    /// `"contradicts"`) from this memory to an **existing, live** memory —
    /// S13, `docs/adr/0012`. [`Store::remember`] verifies the target and
    /// fails with a typed error otherwise. Navigate back (either direction)
    /// with [`Store::related`].
    pub fn relation(mut self, kind: impl Into<String>, target: Ulid) -> Self {
        self.relations.push((kind.into(), target));
        self
    }

    /// Replaces the whole relation list at once — the seam the shells use
    /// after parsing a `relations` argument.
    pub fn relations(mut self, relations: Vec<(String, Ulid)>) -> Self {
        self.relations = relations;
        self
    }

    /// Marks this memory as the new version of an **existing, live** memory
    /// (S19, `docs/adr/0013`): [`Store::remember`] sets the target's
    /// `superseded` flag and writes a [`SUPERSEDES_RELATION`] edge in the same
    /// transaction. The target disappears from every subsequent
    /// recall/search but stays readable via [`Store::get`]/[`Store::related`]
    /// as history — and `vacuum` preserves it. A missing, forgotten, or
    /// different-project target is a typed error and nothing is stored.
    pub fn supersede(mut self, target: Ulid) -> Self {
        self.supersedes.push(target);
        self
    }

    /// Replaces the whole supersedes list at once — the seam the shells use
    /// after parsing a `supersedes` argument.
    pub fn supersedes(mut self, targets: Vec<Ulid>) -> Self {
        self.supersedes = targets;
        self
    }
}

/// How far a [`Store::recall`] looks. Defaults to [`Scope::All`]; the MCP
/// shell narrows it to the current project (DESIGN.md §7) while keeping the
/// explicit global fallback available.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scope {
    /// Every memory in the store, regardless of project.
    #[default]
    All,
    /// Only memories scoped to this exact project.
    Project(String),
}

/// Default number of hits [`Store::recall`] returns when the caller does not
/// set [`Query::limit`] (DESIGN.md §8).
pub const DEFAULT_RECALL_LIMIT: usize = 8;

/// Relation kind of the graph edge [`Store::remember`] writes from a new
/// memory to each of its [`MemoryDraft::supersede`] targets (S19,
/// `docs/adr/0013`). The edge makes the version chain navigable via
/// [`Store::related`]; the *exclusion* comes from the target record's own
/// `superseded` flag, never from this edge.
pub const SUPERSEDES_RELATION: &str = "supersedes";

/// Cosine-similarity floor at or above which an existing memory is reported
/// as a near-duplicate by [`Store::remember_detailed`] (S21). Measured, not
/// guessed (`benches` `calibrate_near_dup`, numbers in `docs/adr/0016`): on
/// the harness corpus with the shipped model, unrelated pairs sit at p99 =
/// 0.639 (max 0.810), while re-statements of the same fact with framing
/// noise sit at p5 = 0.840 — at 0.80, 98.5% of those duplicates are caught
/// and 0.10% of unrelated pairs are flagged. The list informs, never blocks,
/// so a rare spurious hint is the cheap side of the trade.
pub const NEAR_DUP_THRESHOLD: f32 = 0.80;

/// Most near-duplicates one [`Store::remember_detailed`] reports. The list is
/// a write-time hint for a caller deciding forget/supersede/keep, not a
/// search result — past a handful, more entries add noise, not signal.
pub const NEAR_DUP_LIMIT: usize = 5;

/// Character cap of [`SimilarMemory::content`]. The snippet identifies the
/// existing memory; the full text stays one [`Store::get`] away.
pub const NEAR_DUP_SNIPPET_CHARS: usize = 160;

/// [`SimilarMemory::content`]: at most [`NEAR_DUP_SNIPPET_CHARS`] characters
/// (never split mid-character), with an ellipsis marking an actual cut.
fn near_dup_snippet(content: &str) -> String {
    let mut chars = content.char_indices();
    match chars.nth(NEAR_DUP_SNIPPET_CHARS) {
        None => content.to_owned(),
        Some((cut, _)) => format!("{}…", &content[..cut]),
    }
}

/// A nearest-neighbor recall request. Build with [`Query::new`] plus the
/// chainable setters; the defaults (limit 8, all projects, the index's
/// default `ef_search`) match DESIGN.md §8.
#[derive(Debug, Clone)]
pub struct Query {
    /// The text embedded and searched for.
    text: String,
    /// Maximum hits to return.
    limit: usize,
    /// Project filter.
    scope: Scope,
    /// Metadata filters (S10): `key → predicate`, ANDed together. A memory is
    /// kept only when it satisfies every entry. Empty (the default) = no
    /// metadata filtering. Composed with scope/tombstone in the same `keep`
    /// predicate, so the adaptive `ef_search` anti-under-return guarantee of
    /// S2 covers filtered results too.
    filters: BTreeMap<String, Filter>,
    /// Provenance filter by writing agent (S14): when set, only memories whose
    /// [`Provenance::agent`] equals this string are kept. `None` (the default)
    /// = no agent filtering. Agent lives on the record's provenance, not its
    /// metadata, so it is a dedicated field rather than a [`Filter`]; it is
    /// applied in the same `in_scope`/`keep` predicate as scope and tombstone,
    /// so filtered recall keeps the S2 anti-under-return guarantee.
    agent: Option<String>,
    /// HNSW candidate list size at layer 0 (`docs/adr/0002`). `None` (the
    /// default) means "let the index pick": the effective `ef_search` then
    /// scales with the graph size ([`index::default_ef_search`], S16 /
    /// `docs/adr/0015`) so recall holds up as the corpus grows. `Some(n)` is a
    /// caller's explicit override and is honored verbatim — the scaling
    /// governs only the default, never a value the caller set.
    ef_search: Option<u16>,
    /// Optional 1-hop graph expansion (S13): when set, each direct hit's
    /// relation neighbors are appended to the results as connected context.
    expand_related: bool,
    /// Recency as a third RRF list (S20, `docs/adr/0014`): when set, the
    /// content candidates (union of vector + text) are also ranked by
    /// `created_at` descending and fused in as a third list, breaking ties
    /// toward the newer memory without ever displacing a stronger old match
    /// (RRF property, `recall.rs` module docs). Default and rationale
    /// decided by measurement — see `docs/adr/0014`.
    recency: bool,
}

impl Query {
    /// A query for `text` at the defaults (limit 8, [`Scope::All`], default
    /// `ef_search`).
    pub fn new(text: impl Into<String>) -> Self {
        Query {
            text: text.into(),
            limit: DEFAULT_RECALL_LIMIT,
            scope: Scope::All,
            filters: BTreeMap::new(),
            agent: None,
            ef_search: None,
            expand_related: false,
            recency: false,
        }
    }

    /// Caps the number of hits returned.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Restricts the search to a scope (see [`Scope`]).
    pub fn scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }

    /// Convenience for `scope(Scope::Project(project))`.
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.scope = Scope::Project(project.into());
        self
    }

    /// Overrides the HNSW `ef_search` for this query, sovereign over the
    /// size-scaled default (S16): once set, the given `ef_search` is used
    /// verbatim regardless of index size. Left unset, the effective value is
    /// chosen by [`index::default_ef_search`] from the graph's node count.
    pub fn ef_search(mut self, ef_search: u16) -> Self {
        self.ef_search = Some(ef_search);
        self
    }

    /// The `ef_search` to run this query with, given an index of `node_count`
    /// nodes: the caller's explicit override if set, else the size-scaled
    /// default ([`index::default_ef_search`], S16 / `docs/adr/0015`). Callers
    /// in `Store::recall`/`recall_vector` resolve it right before the HNSW
    /// search, where the live node count is on hand.
    fn effective_ef_search(&self, node_count: u64) -> u16 {
        self.ef_search
            .unwrap_or_else(|| index::default_ef_search(node_count))
    }

    /// Adds one metadata filter (S10): a memory is kept only if the value it
    /// stored under `key` satisfies `filter`. Filters are ANDed — call this
    /// once per key. A filter on a key a memory does not have simply excludes
    /// that memory (0 hits, never an error); a filter whose type disagrees
    /// with the stored value's type surfaces a typed error from
    /// [`Store::recall`] (`docs/01-spec.md` S10).
    pub fn filter(mut self, key: impl Into<String>, filter: Filter) -> Self {
        self.filters.insert(key.into(), filter);
        self
    }

    /// Replaces all metadata filters at once — the seam the shells use after
    /// parsing a `filters` argument into a map.
    pub fn filters(mut self, filters: BTreeMap<String, Filter>) -> Self {
        self.filters = filters;
        self
    }

    /// Filters recall to memories written by exactly this agent (S14 basic
    /// provenance, CLAUDE.md decision 3). The agent is compared against the
    /// record's [`Provenance::agent`]; an empty-string agent matches memories
    /// stored with unknown provenance. Composes (AND) with scope and metadata
    /// filters. Pass an empty option to clear it.
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    /// Enables 1-hop graph expansion (S13, `docs/adr/0012`): after ranking,
    /// each direct hit's relation neighbors (both directions) that pass the
    /// same liveness/scope/filter checks are appended as connected context,
    /// with score `0.0` (they matched the graph, not the query) and without
    /// counting against [`Query::limit`]. One hop only. On a file with no
    /// graph layer this is a silent no-op.
    pub fn expand_related(mut self, expand: bool) -> Self {
        self.expand_related = expand;
        self
    }

    /// Enables recency as a third RRF list (S20, `docs/adr/0014`): the
    /// content candidates (union of vector + text) are also ranked by
    /// `created_at` descending and fused in, so ties among equally-relevant
    /// matches break toward the newer memory. Never introduces a candidate
    /// content search didn't already find, and — by RRF's own math — a
    /// single list can't outweigh two content lists agreeing, so a strong
    /// old match is never displaced by novelty alone. Only affects
    /// [`Store::recall`]/[`Store::recall_detailed`]; [`Store::recall_vector`]
    /// (the pure HNSW half) ignores this flag.
    pub fn recency(mut self, recency: bool) -> Self {
        self.recency = recency;
        self
    }

    /// Whether `record`'s metadata passes **every** filter (AND). Returns a
    /// typed error on the first filter whose type disagrees with the stored
    /// value; a filter on an absent key is a plain non-match (`Ok(false)`).
    /// Empty filter set ⇒ always `Ok(true)`.
    fn record_passes_filters(&self, record: &MemoryRecord) -> Result<bool> {
        for (key, filter) in &self.filters {
            if !filter.matches(record.metadata.get(key))? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// One stored memory, as returned by the API.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    /// Time-ordered unique id.
    pub id: Ulid,
    /// The memory text.
    pub content: String,
    /// Project scope; `None` = global.
    pub project: Option<String>,
    /// Typed metadata.
    pub metadata: BTreeMap<String, Scalar>,
    /// Who wrote it, and when.
    pub provenance: Provenance,
    /// `true` only when yielded by [`Store::iter_all`] after a `forget`.
    pub tombstone: bool,
    /// `true` when a newer memory superseded this one (S19, `docs/adr/0013`):
    /// excluded from every recall/search, but still readable here and
    /// navigable via [`Store::related`] as the previous version.
    pub superseded: bool,
}

impl Memory {
    fn from_record(record: MemoryRecord) -> Memory {
        Memory {
            id: record.id,
            content: record.content,
            project: record.project,
            metadata: record.metadata,
            provenance: record.provenance,
            tombstone: record.tombstone,
            superseded: record.superseded,
        }
    }
}

/// One agent's slice of a [`StoreStats`] provenance breakdown (S14): how many
/// live memories it wrote and which sessions it wrote them under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentStats {
    /// Live memories this agent wrote.
    pub live_memories: u64,
    /// Distinct sessions this agent wrote under. Memories with no session id
    /// contribute nothing here, so this can be empty even when
    /// `live_memories > 0`.
    pub sessions: BTreeSet<String>,
}

/// What [`Store::stats`] reports — the numbers behind `embedmind stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStats {
    /// Memories that `iter`/`get`/`recall` can see.
    pub live_memories: u64,
    /// Tombstoned memories awaiting `vacuum` (`docs/adr/0003`).
    pub forgotten_memories: u64,
    /// Live-memory breakdown by writing agent (S14 basic provenance): one
    /// entry per distinct [`Provenance::agent`], keyed by the agent string
    /// (the empty string groups memories with unknown provenance). Empty when
    /// the store has no live memories. Forgotten memories are not counted.
    pub by_agent: BTreeMap<String, AgentStats>,
    /// HNSW graph entries — one per indexed chunk, so a long memory
    /// (DESIGN §6) counts once per chunk. 0 = no vector index yet.
    pub index_entries: u64,
    /// Documents in the full-text index (`docs/adr/0011`); one per live
    /// `remember`. 0 = no full-text index yet (e.g. a pre-M2 file).
    pub fts_documents: u64,
    /// Distinct entities in the graph layer (S13, `docs/adr/0012`).
    /// 0 = no graph data yet (e.g. a pre-M3 file, or one that never used it).
    pub graph_entities: u64,
    /// Stored relations in the graph layer (each counted once, not per end).
    /// Tombstoned ends are still counted until `vacuum` rebuilds the graph.
    pub graph_relations: u64,
    /// Page size recorded in the header.
    pub page_size: u32,
    /// Total pages in the main file.
    pub page_count: u64,
    /// Main file size in bytes (`page_size × page_count`; the WAL sidecar,
    /// when present, is extra and transient).
    pub file_bytes: u64,
    /// Embedding model recorded in the header; `None` = KV-only so far.
    pub embedding_model_id: Option<String>,
    /// Embedding dimensionality (0 until a model is recorded).
    pub embedding_dims: u16,
}

/// The full result of [`Store::recall_detailed`]: the fused hits plus whether
/// recall had to degrade to vector-only for lack of a full-text index.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallOutcome {
    /// Hits, best fused rank first (see [`Recalled`]).
    pub hits: Vec<Recalled>,
    /// `true` when the store had no full-text index (a pre-M2 `.mind`), so the
    /// BM25 half was skipped and these hits are vector-only. Never an error —
    /// old files still recall, just without keyword matching. Shells surface
    /// this as a warning.
    pub degraded_to_vector_only: bool,
}

/// The full result of [`Store::remember_detailed`]: the stored memory plus
/// the near-duplicates found at write time (S21).
#[derive(Debug, Clone, PartialEq)]
pub struct Remembered {
    /// The memory that was stored — storing always happens; the similar list
    /// informs, it never blocks.
    pub memory: Memory,
    /// Existing memories similar to the new content, best first: live,
    /// non-superseded, same scope, cosine ≥ [`NEAR_DUP_THRESHOLD`], at most
    /// [`NEAR_DUP_LIMIT`]. Empty when nothing comes close (or the store is
    /// KV-only / was empty).
    pub similar: Vec<SimilarMemory>,
}

/// One near-duplicate reported by [`Store::remember_detailed`] (S21): enough
/// to decide forget/supersede/keep without another lookup — the id to act
/// on, a content snippet to recognize it by, the similarity that flagged it,
/// and its age.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarMemory {
    /// Id of the existing memory.
    pub id: Ulid,
    /// Its content, truncated to [`NEAR_DUP_SNIPPET_CHARS`] characters (an
    /// `…` marks a cut). Full text via [`Store::get`].
    pub content: String,
    /// Cosine similarity to the new content, in `[-1, 1]` — the raw score,
    /// not an RRF rank (this is a same-scale duplicate check, not fusion;
    /// ADR 0005 applies to recall ranking, not here).
    pub score: f32,
    /// When the existing memory was written (µs since the Unix epoch) — the
    /// caller's cue for which side is stale.
    pub created_at_micros: i64,
}

/// One [`Store::recall`] hit: the memory plus its fused relevance score. Derefs
/// to [`Memory`], so `hit.content`, `hit.id`, … read naturally.
#[derive(Debug, Clone, PartialEq)]
pub struct Recalled {
    /// The recalled memory.
    pub memory: Memory,
    /// Reciprocal Rank Fusion score (`docs/adr/0005`): the sum of `1/(60 +
    /// rank + 1)` over the vector and text lists this memory ranked in. Small
    /// and positive (a rank-0 hit contributes `~0.0164` per list); higher is
    /// more relevant. It is a *rank* score, not a cosine similarity or a BM25
    /// score — those scales are deliberately discarded so there is nothing to
    /// calibrate. When recall degraded to vector-only, only the vector list
    /// contributes. Hits appended by 1-hop graph expansion
    /// ([`Query::expand_related`]) carry exactly `0.0` — they are connected
    /// context, not ranked matches.
    pub score: f32,
}

/// One graph neighbor of a memory, as returned by [`Store::related`] (S13).
/// Derefs to [`Memory`], so `rel.content`, `rel.id`, … read naturally.
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedMemory {
    /// The memory at the other end of the edge (always live — tombstoned
    /// neighbors are filtered at query time).
    pub memory: Memory,
    /// The relation kind ("refines", "contradicts", …).
    pub kind: String,
    /// `true` = the queried memory relates *to* this one; `false` = this one
    /// relates to the queried memory.
    pub outgoing: bool,
}

impl std::ops::Deref for RelatedMemory {
    type Target = Memory;
    fn deref(&self) -> &Memory {
        &self.memory
    }
}

impl std::ops::Deref for Recalled {
    type Target = Memory;
    fn deref(&self) -> &Memory {
        &self.memory
    }
}

/// Iterator over memories in timeline (id) order. See [`Store::iter`].
pub struct MemoryIter<'a> {
    inner: btree::Scan<'a>,
    include_tombstones: bool,
}

impl Iterator for MemoryIter<'_> {
    type Item = Result<Memory>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next()? {
                Err(e) => return Some(Err(e)),
                Ok((key, bytes)) => {
                    let memory = match MemoryRecord::decode(&bytes) {
                        Ok(record) => Memory::from_record(record),
                        Err(e) => return Some(Err(e)),
                    };
                    if memory.id.to_bytes() != key {
                        return Some(Err(Error::MalformedRecord("key/id mismatch")));
                    }
                    if memory.tombstone && !self.include_tombstones {
                        continue;
                    }
                    return Some(Ok(memory));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::storage::sim::SimVfs;

    fn store() -> (Arc<dyn Vfs>, Store) {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let store = Store::create_with(
            Arc::clone(&vfs),
            Path::new("m.mind"),
            StoreOptions::default(),
        )
        .unwrap();
        (vfs, store)
    }

    #[test]
    fn explicit_ef_search_is_sovereign_over_the_scaled_default() {
        // No override: the effective ef is the size-scaled default, so it
        // changes with node count (S16).
        let q = Query::new("x");
        assert_eq!(q.effective_ef_search(0), index::default_ef_search(0));
        assert_eq!(
            q.effective_ef_search(100_000),
            index::default_ef_search(100_000)
        );
        assert_ne!(
            q.effective_ef_search(0),
            q.effective_ef_search(100_000),
            "the default must scale with index size"
        );

        // With an explicit override, the value is honored verbatim regardless
        // of index size — the scaling governs only the default.
        let q = Query::new("x").ef_search(48);
        assert_eq!(q.effective_ef_search(0), 48);
        assert_eq!(q.effective_ef_search(100_000), 48);
        assert_eq!(q.effective_ef_search(10_000_000), 48);
    }

    /// A tiny deterministic [`Embedder`] for the hybrid-recall golden tests.
    /// Each memory/query embeds as the (L2-normalizable) sum of one fixed axis
    /// per *known* word; unknown words contribute nothing. Because the axis is
    /// per-*concept*, synonyms can be made to share an axis, so "carro" and
    /// "automóvel" embed close even though BM25 sees two different tokens —
    /// exactly the semantic-synonym case S9 must handle. No ONNX, no I/O, fully
    /// reproducible: the wrong tool for shipping, the right one for asserting
    /// fusion behaviour without a real model's noise.
    #[derive(Debug)]
    struct WordEmbedder {
        /// word → axis index into a `DIMS`-dimensional space.
        axes: std::collections::HashMap<&'static str, usize>,
    }

    impl WordEmbedder {
        const DIMS: u16 = 16;

        /// `groups`: each inner slice is a set of synonyms that share one axis.
        fn new(groups: &[&[&'static str]]) -> Self {
            let mut axes = std::collections::HashMap::new();
            for (axis, group) in groups.iter().enumerate() {
                assert!(axis < Self::DIMS as usize, "too many concept axes");
                for &word in *group {
                    axes.insert(word, axis);
                }
            }
            WordEmbedder { axes }
        }
    }

    impl Embedder for WordEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut v = vec![0.0f32; Self::DIMS as usize];
            for token in crate::index::fts::tokenize(text) {
                if let Some(&axis) = self.axes.get(token.as_str()) {
                    v[axis] += 1.0;
                }
            }
            Ok(v)
        }
        fn id(&self) -> crate::embed::ModelId {
            "test-word-embedder-v1"
        }
        fn dims(&self) -> u16 {
            Self::DIMS
        }
    }

    /// A store whose recall uses [`WordEmbedder`] over the given synonym
    /// groups — the seam the hybrid golden tests share.
    fn store_with_embedder(groups: &[&[&'static str]]) -> Store {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let opts = StoreOptions {
            embedder: Some(Arc::new(WordEmbedder::new(groups))),
            ..StoreOptions::default()
        };
        Store::create_with(vfs, Path::new("m.mind"), opts).unwrap()
    }

    #[test]
    fn remember_get_roundtrip() {
        let (_, mut store) = store();
        let m = store
            .remember(
                MemoryDraft::new("prefers explicit errors over panics")
                    .project("embedmind")
                    .agent("claude-code")
                    .session("sess-42")
                    .meta("topic", Scalar::Str("conventions".into()))
                    .meta("weight", Scalar::I64(3)),
            )
            .unwrap();
        assert!(!m.tombstone);
        assert!(m.provenance.created_at_micros > 0);

        let got = store.get(m.id).unwrap().unwrap();
        assert_eq!(got, m);
        assert_eq!(got.project.as_deref(), Some("embedmind"));
        assert_eq!(got.provenance.agent, "claude-code");
        assert_eq!(got.metadata["weight"], Scalar::I64(3));
        assert_eq!(store.get(Ulid::new()).unwrap(), None);
    }

    #[test]
    fn forget_is_a_tombstone() {
        let (_, mut store) = store();
        let m = store.remember(MemoryDraft::new("to be forgotten")).unwrap();
        let keep = store.remember(MemoryDraft::new("keep me")).unwrap();

        assert!(store.forget(m.id).unwrap());
        assert!(!store.forget(m.id).unwrap()); // already forgotten
        assert!(!store.forget(Ulid::new()).unwrap()); // never existed

        assert_eq!(store.get(m.id).unwrap(), None);
        assert_eq!(store.get(keep.id).unwrap().unwrap().content, "keep me");

        let live: Vec<Memory> = store.iter().collect::<Result<_>>().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, keep.id);

        let all: Vec<Memory> = store.iter_all().collect::<Result<_>>().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|m| m.tombstone));
    }

    /// FTOPT-2: the sidecar's `doc_len` is what the BM25 length-normalization
    /// reads instead of reloading and re-tokenizing the record. The one thing
    /// that could silently break correctness is the stored count drifting from
    /// the content. Every production write path derives `doc_len` from the very
    /// bytes it stores in the same transaction, so it cannot drift there — but
    /// a stray write bypassing that derivation is an invariant bug, and this
    /// proves the guard actually fires on such a divergence (a negative
    /// counterpart to the always-valid states exercised in `tests/filter_meta`).
    #[test]
    fn a_diverging_doc_len_is_caught_by_the_invariant() {
        let (_, mut store) = store();
        let m = store
            .remember(MemoryDraft::new("rust rust rust content").agent("cli"))
            .unwrap();
        // Sanity: the honestly-written sidecar agrees with the record.
        store.verify_filter_meta_invariant().unwrap();
        let real = index::fts::doc_len("rust rust rust content");

        // Append a fresh entry for the same id with a doc_len the content
        // could never produce. The chain is last-writer-wins, so this shadows
        // the correct entry — exactly the state a buggy write path would leave.
        let mut txn = store.pager.begin().unwrap();
        index::filter_meta::record_updates(
            &mut txn,
            &[index::filter_meta::Update {
                id: m.id,
                tombstone: false,
                superseded: false,
                has_metadata: false,
                project: None,
                agent: "cli",
                doc_len: real + 1,
            }],
        )
        .unwrap();
        txn.commit().unwrap();

        match store.verify_filter_meta_invariant() {
            Err(Error::Internal(msg)) if msg.contains("doc_len") => {}
            other => panic!("a diverging doc_len must be an invariant error, got {other:?}"),
        }
    }

    /// FTOPT-2, the positive half: proves the BM25 length normalization reads
    /// `doc_len` from the sidecar rather than re-tokenizing the record. We
    /// overwrite the sidecar entry with a `doc_len` the content never produced;
    /// if scoring still consulted the content the score would be unchanged, so
    /// a shifted score is proof the stored count is the one that is read. (The
    /// invariant this deliberately violates is what the test above guards.)
    #[test]
    fn bm25_length_normalization_reads_doc_len_from_the_sidecar() {
        let (_, mut store) = store();
        let m = store
            .remember(MemoryDraft::new("rust ownership note"))
            .unwrap();

        let baseline = store.search_text(Query::new("rust")).unwrap();
        let baseline_score = baseline
            .iter()
            .find(|h| h.id == m.id)
            .expect("the doc contains the term")
            .score;

        // Shadow the entry with a much larger doc_len. BM25 penalizes longer
        // documents, so reading this stored value must lower the score below
        // the honest baseline; re-tokenizing the (unchanged) content would not.
        let real = index::fts::doc_len("rust ownership note");
        let mut txn = store.pager.begin().unwrap();
        index::filter_meta::record_updates(
            &mut txn,
            &[index::filter_meta::Update {
                id: m.id,
                tombstone: false,
                superseded: false,
                has_metadata: false,
                project: None,
                agent: "",
                doc_len: real * 100,
            }],
        )
        .unwrap();
        txn.commit().unwrap();

        let after = store.search_text(Query::new("rust")).unwrap();
        let after_score = after
            .iter()
            .find(|h| h.id == m.id)
            .expect("the doc still matches")
            .score;
        assert!(
            after_score < baseline_score,
            "a longer sidecar doc_len must lower the BM25 score \
             ({after_score} !< {baseline_score}) — proof the stored count is read"
        );
    }

    #[test]
    fn iteration_is_in_id_order_and_persists_across_reopen() {
        let (vfs, mut store) = store();
        let mut ids = Vec::new();
        for i in 0..50 {
            ids.push(
                store
                    .remember(MemoryDraft::new(format!("memory {i}")))
                    .unwrap()
                    .id,
            );
        }
        store.close().unwrap();

        let store = Store::open_with(vfs, Path::new("m.mind"), StoreOptions::default()).unwrap();
        let listed: Vec<Ulid> = store
            .iter()
            .map(|m| m.map(|m| m.id))
            .collect::<Result<_>>()
            .unwrap();
        let mut expected = ids.clone();
        expected.sort(); // id order == time order (same-millisecond ties sort by randomness)
        assert_eq!(listed, expected);
        for id in ids {
            assert!(store.get(id).unwrap().is_some());
        }
    }

    #[test]
    fn large_content_goes_through_overflow_chains() {
        let (vfs, mut store) = store();
        let big = "x".repeat(100_000) + " fim";
        let m = store.remember(MemoryDraft::new(big.clone())).unwrap();
        assert_eq!(store.get(m.id).unwrap().unwrap().content, big);
        store.close().unwrap();
        let store = Store::open_with(vfs, Path::new("m.mind"), StoreOptions::default()).unwrap();
        assert_eq!(store.get(m.id).unwrap().unwrap().content, big);
    }

    #[test]
    fn stats_reports_counts_and_layout() {
        let (_, mut store) = store();
        let stats = store.stats().unwrap();
        assert_eq!(stats.live_memories, 0);
        assert_eq!(stats.forgotten_memories, 0);
        assert_eq!(stats.index_entries, 0);
        assert_eq!(stats.fts_documents, 0);
        assert_eq!(stats.embedding_model_id, None, "KV-only store: no model");

        let keep = store.remember(MemoryDraft::new("keep")).unwrap();
        let doomed = store.remember(MemoryDraft::new("doomed")).unwrap();
        store.forget(doomed.id).unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.live_memories, 1);
        assert_eq!(stats.forgotten_memories, 1);
        // Full-text index counts every indexed document (tombstones included;
        // they are filtered at query time, then reclaimed by vacuum).
        assert_eq!(stats.fts_documents, 2);
        assert_eq!(
            stats.file_bytes,
            u64::from(stats.page_size) * stats.page_count
        );
        assert!(stats.page_count >= 2, "header + at least one data page");
        assert!(store.get(keep.id).unwrap().is_some());
    }

    /// A store on a shared [`SimVfs`] with a [`WordEmbedder`], plus the vfs and
    /// path so the vacuum tests can reopen after the swap.
    fn store_on(vfs: &Arc<dyn Vfs>, path: &str, groups: &[&[&'static str]]) -> Store {
        let opts = StoreOptions {
            embedder: Some(Arc::new(WordEmbedder::new(groups))),
            ..StoreOptions::default()
        };
        Store::create_with(Arc::clone(vfs), Path::new(path), opts).unwrap()
    }

    #[test]
    fn vacuum_preserves_live_records_and_drops_tombstones() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut store = store_on(&vfs, "m.mind", &[&["rust"], &["python"]]);

        let keep = store
            .remember(
                MemoryDraft::new("rust ownership is nice")
                    .project("embedmind")
                    .agent("claude-code")
                    .session("s1")
                    .meta("weight", Scalar::I64(7)),
            )
            .unwrap();
        let gone = store
            .remember(MemoryDraft::new("python gil is annoying"))
            .unwrap();
        let keep2 = store
            .remember(MemoryDraft::new("more rust rust content"))
            .unwrap();
        store.forget(gone.id).unwrap();

        store.vacuum().unwrap();

        // Live records survive byte-for-byte (id, provenance, metadata).
        let got = store.get(keep.id).unwrap().unwrap();
        assert_eq!(got.content, "rust ownership is nice");
        assert_eq!(got.project.as_deref(), Some("embedmind"));
        assert_eq!(got.provenance.agent, "claude-code");
        assert_eq!(got.provenance.session_id.as_deref(), Some("s1"));
        assert_eq!(
            got.provenance.created_at_micros,
            keep.provenance.created_at_micros
        );
        assert_eq!(got.metadata["weight"], Scalar::I64(7));
        assert!(store.get(keep2.id).unwrap().is_some());

        // The forgotten memory is gone entirely — not even a tombstone remains.
        assert_eq!(store.get(gone.id).unwrap(), None);
        let all: Vec<Memory> = store.iter_all().collect::<Result<_>>().unwrap();
        assert_eq!(all.len(), 2, "no tombstone left behind after vacuum");
        assert!(all.iter().all(|m| !m.tombstone));

        // Indexes were rebuilt without the dead: fts counts only the living,
        // and recall/search still work against the vacuumed file.
        let stats = store.stats().unwrap();
        assert_eq!(stats.live_memories, 2);
        assert_eq!(stats.forgotten_memories, 0);
        assert_eq!(stats.fts_documents, 2);
        let hits = store.search_text(Query::new("rust")).unwrap();
        let ids: Vec<Ulid> = hits.iter().map(|h| h.id).collect();
        assert!(ids.contains(&keep.id) && ids.contains(&keep2.id));
        assert!(!ids.contains(&gone.id));
        let recalled = store.recall(Query::new("python")).unwrap();
        assert!(
            recalled.iter().all(|r| r.id != gone.id),
            "the forgotten python doc must not resurface in recall"
        );
    }

    #[test]
    fn vacuum_shrinks_the_file_and_reopens_clean() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut store = store_on(&vfs, "m.mind", &[&["rust"]]);
        // Enough memories that forgetting most of them frees real pages.
        let mut ids = Vec::new();
        for i in 0..40 {
            ids.push(
                store
                    .remember(MemoryDraft::new(format!(
                        "rust memory number {i} {}",
                        "content ".repeat(20)
                    )))
                    .unwrap()
                    .id,
            );
        }
        let before = store.stats().unwrap();
        for id in ids.iter().take(30) {
            store.forget(*id).unwrap();
        }
        store.vacuum().unwrap();
        let after = store.stats().unwrap();

        assert!(
            after.file_bytes <= before.file_bytes,
            "vacuum must never grow the file: {} -> {}",
            before.file_bytes,
            after.file_bytes
        );
        assert!(
            after.file_bytes < before.file_bytes,
            "forgetting 75% then vacuuming should reclaim pages"
        );
        assert_eq!(after.live_memories, 10);

        // The store is usable after vacuum, and survives a clean close + reopen
        // (the swap left a single, well-formed file — no orphan temp/scratch).
        let survivor = ids[35];
        assert!(store.get(survivor).unwrap().is_some());
        store.close().unwrap();
        assert!(!vfs.exists(Path::new("m.mind-vacuum-tmp")));
        assert!(!vfs.exists(Path::new("m.mind-vacuum-scratch")));

        let opts = StoreOptions {
            embedder: Some(Arc::new(WordEmbedder::new(&[&["rust"]]))),
            ..StoreOptions::default()
        };
        let store = Store::open_with(Arc::clone(&vfs), Path::new("m.mind"), opts).unwrap();
        assert_eq!(store.stats().unwrap().live_memories, 10);
        assert!(store.get(survivor).unwrap().is_some());
    }

    #[test]
    fn vacuum_with_nothing_forgotten_is_idempotent() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut store = store_on(&vfs, "m.mind", &[&["rust"], &["python"]]);
        for i in 0..12 {
            store
                .remember(MemoryDraft::new(format!("rust and python doc {i}")))
                .unwrap();
        }
        let before = store.stats().unwrap();
        store.vacuum().unwrap();
        let after = store.stats().unwrap();
        assert_eq!(after.live_memories, before.live_memories);
        assert_eq!(after.fts_documents, before.fts_documents);
        assert!(
            after.file_bytes <= before.file_bytes,
            "a no-tombstone vacuum still must not grow the file"
        );
        // Still fully queryable.
        assert!(!store.search_text(Query::new("rust")).unwrap().is_empty());
    }

    #[test]
    fn search_text_ranks_filters_tombstones_and_respects_scope() {
        let (_, mut store) = store();
        let a = store
            .remember(MemoryDraft::new("the rust borrow checker prevents data races").project("x"))
            .unwrap();
        let b = store
            .remember(MemoryDraft::new("python has a global interpreter lock").project("x"))
            .unwrap();
        let c = store
            .remember(MemoryDraft::new("rust rust rust ownership and borrowing").project("y"))
            .unwrap();

        // Keyword search finds both rust docs, ranks the denser one first.
        let hits = store.search_text(Query::new("rust borrow")).unwrap();
        let ids: Vec<Ulid> = hits.iter().map(|h| h.id).collect();
        assert!(ids.contains(&a.id) && ids.contains(&c.id));
        assert!(!ids.contains(&b.id), "python doc has no query term");
        assert!(hits.iter().all(|h| h.score > 0.0));

        // Scope narrows to a single project.
        let scoped = store.search_text(Query::new("rust").project("y")).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, c.id);

        // Forgetting a doc removes it from results (tombstone re-check).
        store.forget(c.id).unwrap();
        let after = store.search_text(Query::new("rust")).unwrap();
        assert!(after.iter().all(|h| h.id != c.id));
        assert!(after.iter().any(|h| h.id == a.id));
    }

    // --- S14 basic provenance exposed (agent filter + stats breakdown) -----

    #[test]
    fn recall_filters_by_agent() {
        let mut store = store_with_embedder(&[&["rust"], &["python"]]);
        let by_cli = store
            .remember(MemoryDraft::new("rust note from the cli").agent("cli"))
            .unwrap();
        let by_claude = store
            .remember(MemoryDraft::new("rust note from claude").agent("claude-code"))
            .unwrap();

        // No agent filter: both surface.
        let all = store.recall(Query::new("rust")).unwrap();
        let ids: Vec<Ulid> = all.iter().map(|h| h.id).collect();
        assert!(ids.contains(&by_cli.id) && ids.contains(&by_claude.id));

        // Filter to one agent: only that agent's memory, through both halves.
        let only_cli = store.recall(Query::new("rust").agent("cli")).unwrap();
        assert_eq!(only_cli.len(), 1);
        assert_eq!(only_cli[0].id, by_cli.id);
        assert_eq!(only_cli[0].provenance.agent, "cli");

        // The agent filter also constrains the pure keyword half.
        let text_cli = store
            .search_text(Query::new("rust").agent("claude-code"))
            .unwrap();
        assert_eq!(text_cli.len(), 1);
        assert_eq!(text_cli[0].id, by_claude.id);

        // An agent nobody used yields nothing, never an error.
        assert!(
            store
                .recall(Query::new("rust").agent("nobody"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stats_breaks_down_live_memories_by_agent_and_session() {
        let (_, mut store) = store();
        store
            .remember(MemoryDraft::new("a").agent("cli").session("s1"))
            .unwrap();
        store
            .remember(MemoryDraft::new("b").agent("cli").session("s1"))
            .unwrap();
        store
            .remember(MemoryDraft::new("c").agent("cli").session("s2"))
            .unwrap();
        store
            .remember(MemoryDraft::new("d").agent("claude-code"))
            .unwrap();
        let doomed = store
            .remember(MemoryDraft::new("e").agent("claude-code"))
            .unwrap();
        store.forget(doomed.id).unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.live_memories, 4);
        assert_eq!(stats.forgotten_memories, 1);

        // Two live agents; the forgotten memory does not inflate its agent.
        let cli = &stats.by_agent["cli"];
        assert_eq!(cli.live_memories, 3);
        assert_eq!(
            cli.sessions,
            ["s1".to_owned(), "s2".to_owned()].into_iter().collect(),
            "distinct sessions, deduplicated"
        );
        let claude = &stats.by_agent["claude-code"];
        assert_eq!(claude.live_memories, 1);
        assert!(
            claude.sessions.is_empty(),
            "no session id ⇒ no session recorded"
        );
        assert_eq!(stats.by_agent.len(), 2);
    }

    #[test]
    fn stats_by_agent_groups_unknown_provenance_under_the_empty_agent() {
        let (_, mut store) = store();
        store.remember(MemoryDraft::new("no agent set")).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.by_agent[""].live_memories, 1);
    }

    // --- S9 hybrid-recall golden cases (RRF fusion, docs/adr/0005) ---------

    /// Synonym groups shared by the golden cases: "carro"/"automóvel"/"veículo"
    /// share a semantic axis; the other content words get their own axes so
    /// they don't accidentally collide. Rare exact tokens (part numbers) are
    /// deliberately *absent* from the embedder — they carry no semantics, only
    /// a keyword match.
    fn golden_store() -> Store {
        store_with_embedder(&[
            &["carro", "automóvel", "veículo"],
            &["rápido", "veloz"],
            &["motor", "elétrico"],
            &["gato", "felino"],
        ])
    }

    #[test]
    fn golden_rare_exact_term_is_found_via_text_half() {
        let mut store = golden_store();
        // A rare exact token no embedder axis covers: only BM25 can match it.
        let target = store
            .remember(MemoryDraft::new("firmware revision zqx-8842 shipped"))
            .unwrap();
        for filler in [
            "the carro is rápido",
            "an elétrico motor hums",
            "a felino naps",
        ] {
            store.remember(MemoryDraft::new(filler)).unwrap();
        }

        let out = store.recall_detailed(Query::new("zqx-8842")).unwrap();
        assert!(!out.degraded_to_vector_only, "store has an fts index");
        assert_eq!(
            out.hits.first().map(|h| h.id),
            Some(target.id),
            "the rare exact term must surface its memory even though the vector \
             half has no axis for it"
        );
    }

    #[test]
    fn golden_semantic_synonym_is_found_via_vector_half() {
        let mut store = golden_store();
        // Content says "automóvel"; the query says "carro" — different tokens,
        // so BM25 alone would miss it. They share a vector axis, so the vector
        // half brings it in.
        let target = store
            .remember(MemoryDraft::new("comprei um automóvel novo"))
            .unwrap();
        for filler in ["o gato dorme", "firmware zqx-8842 note", "a felino naps"] {
            store.remember(MemoryDraft::new(filler)).unwrap();
        }

        let hits = store.recall(Query::new("carro")).unwrap();
        assert!(
            hits.iter().any(|h| h.id == target.id),
            "a semantic synonym must be recalled via the vector half"
        );
    }

    #[test]
    fn golden_both_halves_agree_ranks_first() {
        let mut store = golden_store();
        // This one matches on *both* axes: the exact word "carro" (BM25) and
        // its semantic axis (vector). It must beat a memory that matches only
        // one half.
        let both = store
            .remember(MemoryDraft::new("o carro é rápido"))
            .unwrap();
        // Vector-only: synonym, no shared token with the query "carro rápido".
        store
            .remember(MemoryDraft::new("um veículo veloz"))
            .unwrap();
        // Text-only-ish filler.
        store.remember(MemoryDraft::new("o gato dorme")).unwrap();

        let hits = store.recall(Query::new("carro rápido")).unwrap();
        assert_eq!(
            hits.first().map(|h| h.id),
            Some(both.id),
            "the memory matching both the keyword and the semantic halves \
             must rank first under RRF"
        );
        // Fusion is a union: the vector-only synonym is still present.
        assert!(hits.len() >= 2);
    }

    #[test]
    fn recall_degrades_to_vector_only_without_an_fts_index_with_warning() {
        // With content stored, `remember` has built the fts index, so recall
        // is hybrid and does not report degradation.
        let mut store = golden_store();
        let target = store
            .remember(MemoryDraft::new("comprei um automóvel"))
            .unwrap();
        let normal = store.recall_detailed(Query::new("carro")).unwrap();
        assert!(!normal.degraded_to_vector_only, "index present ⇒ hybrid");
        assert!(normal.hits.iter().any(|h| h.id == target.id));

        // A store on which nothing was ever `remember`ed has no fts index yet
        // (fts_root_page == 0) — the same state a pre-M2 `.mind` presents.
        // Recall must degrade to vector-only, report it via the flag, and
        // never error.
        let empty = store_with_embedder(&[&["carro"]]);
        assert_eq!(empty.pager.header().fts_root_page, 0, "no fts index yet");
        let degraded = empty.recall_detailed(Query::new("carro")).unwrap();
        assert!(
            degraded.degraded_to_vector_only,
            "no fts index must degrade to vector-only and report it"
        );
        assert!(
            degraded.hits.is_empty(),
            "empty store has nothing to recall"
        );
    }

    // --- S13 graph layer (entities + relations, docs/adr/0012) -------------

    #[test]
    fn graph_remember_then_navigate_by_id_and_by_entity() {
        let (_, mut store) = store();
        let a = store
            .remember(MemoryDraft::new("postgres uses mvcc").entity("postgres"))
            .unwrap();
        let b = store
            .remember(
                MemoryDraft::new("the auth service tracks replica lag")
                    .entity("postgres")
                    .entity("auth-service")
                    .relation("refines", a.id),
            )
            .unwrap();

        // related(id): both directions, kind carried, Deref to Memory.
        let from_b = store.related(b.id).unwrap();
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_b[0].id, a.id);
        assert_eq!(from_b[0].kind, "refines");
        assert!(from_b[0].outgoing);
        let from_a = store.related(a.id).unwrap();
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_a[0].id, b.id);
        assert!(!from_a[0].outgoing, "mirrored incoming edge at the target");

        // related(entity): members in id order (sorted — same-ms ULIDs tie
        // on randomness, so sort the expectation too).
        let mut expected = vec![a.id, b.id];
        expected.sort();
        let members: Vec<Ulid> = store
            .entity_members("postgres")
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(members, expected);
        let auth: Vec<Ulid> = store
            .entity_members("auth-service")
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(auth, vec![b.id]);
        assert!(store.entity_members("unknown").unwrap().is_empty());
        assert_eq!(
            store.entities_of(b.id).unwrap(),
            vec!["auth-service".to_owned(), "postgres".to_owned()]
        );
        assert!(store.entities_of(a.id).unwrap() == vec!["postgres".to_owned()]);

        let stats = store.stats().unwrap();
        assert_eq!(stats.graph_entities, 2);
        assert_eq!(stats.graph_relations, 1);
    }

    #[test]
    fn relation_to_missing_or_forgotten_target_is_a_typed_error() {
        let (_, mut store) = store();
        let gone = store.remember(MemoryDraft::new("to forget")).unwrap();
        store.forget(gone.id).unwrap();
        for target in [Ulid::new(), gone.id] {
            let err = store
                .remember(MemoryDraft::new("dangling").relation("refines", target))
                .unwrap_err();
            assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
        }
        // The failed remembers rolled back whole: only the tombstone remains.
        let all: Vec<Memory> = store.iter_all().collect::<Result<_>>().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(store.stats().unwrap().graph_relations, 0);
    }

    #[test]
    fn relation_to_forgotten_memory_disappears_with_the_tombstone() {
        let (_, mut store) = store();
        let a = store
            .remember(MemoryDraft::new("target").entity("shared"))
            .unwrap();
        let b = store
            .remember(
                MemoryDraft::new("source")
                    .entity("shared")
                    .relation("refines", a.id),
            )
            .unwrap();
        assert_eq!(store.related(b.id).unwrap().len(), 1);

        store.forget(a.id).unwrap();

        // The edge and the entity membership vanish with the tombstone —
        // re-checked at query time, no graph rewrite needed (ADR 0012).
        assert!(store.related(b.id).unwrap().is_empty());
        let members: Vec<Ulid> = store
            .entity_members("shared")
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(members, vec![b.id]);
    }

    #[test]
    fn recall_expand_related_pulls_one_hop_of_connected_context() {
        let mut store = store_with_embedder(&[&["carro"], &["gato"]]);
        // The neighbor shares no token and no vector axis with the query —
        // only the explicit relation connects it.
        let neighbor = store.remember(MemoryDraft::new("o gato dorme")).unwrap();
        let hit = store
            .remember(MemoryDraft::new("comprei um carro").relation("context", neighbor.id))
            .unwrap();

        let plain = store.recall(Query::new("carro").limit(1)).unwrap();
        assert_eq!(plain.len(), 1, "limit caps direct hits");
        assert_eq!(plain[0].id, hit.id);

        let expanded = store
            .recall(Query::new("carro").limit(1).expand_related(true))
            .unwrap();
        assert_eq!(expanded.len(), 2, "expansion does not count against limit");
        assert_eq!(expanded[0].id, hit.id);
        assert!(expanded[0].score > 0.0);
        assert_eq!(expanded[1].id, neighbor.id);
        assert_eq!(
            expanded[1].score, 0.0,
            "expansion hits are context, not ranked matches"
        );

        // A forgotten neighbor never comes back through expansion.
        store.forget(neighbor.id).unwrap();
        let after = store
            .recall(Query::new("carro").limit(1).expand_related(true))
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, hit.id);
    }

    #[test]
    fn vacuum_rebuilds_graph_dropping_dead_entities_and_edges() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut store = store_on(&vfs, "m.mind", &[&["rust"]]);
        let a = store
            .remember(MemoryDraft::new("rust a").entity("rust"))
            .unwrap();
        let doomed = store
            .remember(MemoryDraft::new("rust doomed").entity("doomed-only"))
            .unwrap();
        let b = store
            .remember(
                MemoryDraft::new("rust b")
                    .entity("rust")
                    .relation("refines", a.id)
                    .relation("mentions", doomed.id),
            )
            .unwrap();
        store.forget(doomed.id).unwrap();
        let before = store.stats().unwrap();
        assert_eq!(before.graph_entities, 2);
        assert_eq!(before.graph_relations, 2, "tombstoned end still counted");

        store.vacuum().unwrap();

        // Physically rebuilt: the dead memory's entity and the edge with a
        // dead end are gone from the counters, not just filtered.
        let after = store.stats().unwrap();
        assert_eq!(after.graph_entities, 1);
        assert_eq!(after.graph_relations, 1);

        let rel_b = store.related(b.id).unwrap();
        assert_eq!(rel_b.len(), 1);
        assert_eq!(rel_b[0].id, a.id);
        assert!(rel_b[0].outgoing);
        // The mirrored incoming half was regenerated at the live target.
        let rel_a = store.related(a.id).unwrap();
        assert_eq!(rel_a.len(), 1);
        assert_eq!(rel_a[0].id, b.id);
        assert!(!rel_a[0].outgoing);

        let mut expected = vec![a.id, b.id];
        expected.sort();
        let members: Vec<Ulid> = store
            .entity_members("rust")
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(members, expected);
        assert!(store.entity_members("doomed-only").unwrap().is_empty());
    }

    // --- S19 supersedes: versioned knowledge (docs/adr/0013) ----------------

    #[test]
    fn supersede_hides_target_from_recall_but_get_and_related_keep_it() {
        let mut store = store_with_embedder(&[&["rust"], &["python"]]);
        let a = store
            .remember(MemoryDraft::new("rust fact, first version"))
            .unwrap();
        let b = store
            .remember(MemoryDraft::new("rust fact, corrected version").supersede(a.id))
            .unwrap();

        // A is out of every search path: hybrid recall, pure keyword, pure
        // vector — the status is re-read from the record each time (S2 rule).
        for hits in [
            store.recall(Query::new("rust fact")).unwrap(),
            store.search_text(Query::new("rust fact")).unwrap(),
            store.recall_vector(Query::new("rust fact")).unwrap(),
        ] {
            let ids: Vec<Ulid> = hits.iter().map(|h| h.id).collect();
            assert!(ids.contains(&b.id), "the new version must surface");
            assert!(!ids.contains(&a.id), "the superseded version must not");
        }

        // …but A is still readable as history.
        let got = store.get(a.id).unwrap().unwrap();
        assert_eq!(got.content, "rust fact, first version");
        assert!(got.superseded);
        assert!(!store.get(b.id).unwrap().unwrap().superseded);

        // The chain is navigable in both directions via the graph edge.
        let from_b = store.related(b.id).unwrap();
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_b[0].id, a.id);
        assert_eq!(from_b[0].kind, SUPERSEDES_RELATION);
        assert!(from_b[0].outgoing);
        let from_a = store.related(a.id).unwrap();
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_a[0].id, b.id);
        assert!(!from_a[0].outgoing);

        // 1-hop expansion is part of recall: it must not reintroduce A.
        let expanded = store
            .recall(Query::new("rust fact").expand_related(true))
            .unwrap();
        assert!(expanded.iter().all(|h| h.id != a.id));
    }

    #[test]
    fn supersede_chain_only_the_head_recalls_and_is_navigable_stepwise() {
        let mut store = store_with_embedder(&[&["rust"]]);
        let a = store.remember(MemoryDraft::new("rust fact v1")).unwrap();
        let b = store
            .remember(MemoryDraft::new("rust fact v2").supersede(a.id))
            .unwrap();
        let c = store
            .remember(MemoryDraft::new("rust fact v3").supersede(b.id))
            .unwrap();

        let hits = store.recall(Query::new("rust fact")).unwrap();
        let ids: Vec<Ulid> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![c.id], "only the head of the chain recalls");

        // Walk the chain step by step: C → B → A.
        let step1 = store.related(c.id).unwrap();
        assert_eq!(step1.len(), 1);
        assert_eq!(step1[0].id, b.id);
        assert_eq!(step1[0].kind, SUPERSEDES_RELATION);
        let from_b = store.related(b.id).unwrap();
        let down: Vec<Ulid> = from_b.iter().filter(|r| r.outgoing).map(|r| r.id).collect();
        let up: Vec<Ulid> = from_b
            .iter()
            .filter(|r| !r.outgoing)
            .map(|r| r.id)
            .collect();
        assert_eq!(down, vec![a.id]);
        assert_eq!(up, vec![c.id]);
    }

    #[test]
    fn supersede_missing_forgotten_or_cross_project_target_is_typed_error() {
        let (_, mut store) = store();
        let gone = store.remember(MemoryDraft::new("to forget")).unwrap();
        store.forget(gone.id).unwrap();
        for target in [Ulid::new(), gone.id] {
            let err = store
                .remember(MemoryDraft::new("new version").supersede(target))
                .unwrap_err();
            assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
        }

        // Cross-project (including global vs. project): never crosses scope.
        let in_x = store
            .remember(MemoryDraft::new("scoped fact").project("x"))
            .unwrap();
        let global = store.remember(MemoryDraft::new("global fact")).unwrap();
        for (content, project, target) in [
            ("wrong project", Some("y"), in_x.id),
            ("global cannot supersede scoped", None, in_x.id),
            ("scoped cannot supersede global", Some("x"), global.id),
        ] {
            let mut draft = MemoryDraft::new(content).supersede(target);
            if let Some(p) = project {
                draft = draft.project(p);
            }
            let err = store.remember(draft).unwrap_err();
            assert!(matches!(err, Error::InvalidArgument(_)), "{content}: {err}");
        }

        // Every failed remember rolled back whole: no record, no flag set,
        // no dangling supersedes edge.
        assert!(!store.get(in_x.id).unwrap().unwrap().superseded);
        assert!(!store.get(global.id).unwrap().unwrap().superseded);
        assert_eq!(store.stats().unwrap().graph_relations, 0);
        assert_eq!(store.stats().unwrap().live_memories, 2);
    }

    #[test]
    fn forget_of_the_superseder_does_not_resurrect_the_superseded() {
        let mut store = store_with_embedder(&[&["rust"]]);
        let a = store.remember(MemoryDraft::new("rust fact v1")).unwrap();
        let b = store
            .remember(MemoryDraft::new("rust fact v2").supersede(a.id))
            .unwrap();

        store.forget(b.id).unwrap();

        // Exclusion is state on A's own record, not derived from B or from
        // the graph (ADR 0013): with B gone, A stays hidden.
        let hits = store.recall(Query::new("rust fact")).unwrap();
        assert!(hits.is_empty(), "neither version recalls: {hits:?}");
        assert!(store.get(a.id).unwrap().unwrap().superseded);

        // …and the superseded memory itself can still be forgotten.
        assert!(store.forget(a.id).unwrap());
        assert_eq!(store.get(a.id).unwrap(), None);
    }

    #[test]
    fn vacuum_preserves_superseded_memories_and_the_chain() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut store = store_on(&vfs, "m.mind", &[&["rust"]]);
        let a = store.remember(MemoryDraft::new("rust fact v1")).unwrap();
        let b = store
            .remember(MemoryDraft::new("rust fact v2").supersede(a.id))
            .unwrap();
        let doomed = store.remember(MemoryDraft::new("rust doomed")).unwrap();
        store.forget(doomed.id).unwrap();

        store.vacuum().unwrap();

        // Tombstones reclaimed; superseded history preserved, flag intact.
        assert_eq!(store.get(doomed.id).unwrap(), None);
        let a_after = store.get(a.id).unwrap().unwrap();
        assert!(a_after.superseded, "vacuum must keep the superseded flag");
        assert_eq!(a_after.content, "rust fact v1");

        // The supersedes edge survived the rebuild, both directions.
        let from_b = store.related(b.id).unwrap();
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_b[0].id, a.id);
        assert_eq!(from_b[0].kind, SUPERSEDES_RELATION);

        // And recall still excludes A after the rebuild.
        let hits = store.recall(Query::new("rust fact")).unwrap();
        let ids: Vec<Ulid> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![b.id]);
    }

    #[test]
    fn supersede_deduplicates_targets_and_accepts_multiple() {
        let (_, mut store) = store();
        let a = store.remember(MemoryDraft::new("fact a")).unwrap();
        let b = store.remember(MemoryDraft::new("fact b")).unwrap();
        let merged = store
            .remember(
                MemoryDraft::new("merged fact")
                    .supersede(a.id)
                    .supersede(b.id)
                    .supersede(a.id), // duplicate: must not double the edge
            )
            .unwrap();
        assert!(store.get(a.id).unwrap().unwrap().superseded);
        assert!(store.get(b.id).unwrap().unwrap().superseded);
        let edges = store.related(merged.id).unwrap();
        assert_eq!(edges.len(), 2, "one edge per distinct target: {edges:?}");
        assert!(edges.iter().all(|e| e.kind == SUPERSEDES_RELATION));
        assert_eq!(store.stats().unwrap().graph_relations, 2);
    }

    // --- S21 near-duplicates at write time (docs/adr/0016) ------------------

    #[test]
    fn remember_detailed_reports_near_duplicates_and_always_stores() {
        let mut store = store_with_embedder(&[&["rust"], &["wal"], &["python"]]);
        let first = store
            .remember_detailed(MemoryDraft::new("rust wal"))
            .unwrap();
        assert!(
            first.similar.is_empty(),
            "the first memory of a file has nothing to duplicate"
        );

        // Exact restatement: cosine 1.0, well above any measured threshold.
        let second = store
            .remember_detailed(MemoryDraft::new("rust wal"))
            .unwrap();
        assert_eq!(second.similar.len(), 1);
        let hit = &second.similar[0];
        assert_eq!(hit.id, first.memory.id);
        assert_eq!(hit.content, "rust wal", "short content is not truncated");
        assert!(hit.score > 0.99, "identical content: {}", hit.score);
        assert_eq!(
            hit.created_at_micros,
            first.memory.provenance.created_at_micros
        );

        // Informing never blocks: both memories were stored.
        assert!(store.get(first.memory.id).unwrap().is_some());
        assert!(store.get(second.memory.id).unwrap().is_some());
        assert_eq!(store.stats().unwrap().live_memories, 2);

        // An unrelated write reports nothing.
        let unrelated = store.remember_detailed(MemoryDraft::new("python")).unwrap();
        assert!(unrelated.similar.is_empty(), "{:?}", unrelated.similar);
    }

    #[test]
    // allow: o assert sobre a constante é um tripwire deliberado da calibração (ADR 0016)
    #[allow(clippy::assertions_on_constants)]
    fn near_duplicates_respect_the_measured_threshold() {
        // With WordEmbedder, two memories of n distinct single-axis words
        // sharing k of them have cosine exactly k/n. Bracket the threshold
        // with n = 8: 7/8 must sit at-or-above it and 6/8 below — re-craft
        // these pairs if a re-measurement ever moves the constant out of
        // (0.75, 0.875].
        assert!(
            NEAR_DUP_THRESHOLD <= 7.0 / 8.0 && NEAR_DUP_THRESHOLD > 6.0 / 8.0,
            "threshold moved ({NEAR_DUP_THRESHOLD}); re-craft this test's pairs"
        );
        let mut store = store_with_embedder(&[
            &["w0"],
            &["w1"],
            &["w2"],
            &["w3"],
            &["w4"],
            &["w5"],
            &["w6"],
            &["w7"],
            &["w8"],
            &["w9"],
            &["wa"],
        ]);

        let base = store
            .remember_detailed(MemoryDraft::new("w0 w1 w2 w3 w4 w5 w6 w7"))
            .unwrap();

        // 7 of 8 words shared → cosine 0.875 ≥ threshold: reported.
        let above = store
            .remember_detailed(MemoryDraft::new("w0 w1 w2 w3 w4 w5 w6 w8"))
            .unwrap();
        assert_eq!(above.similar.len(), 1, "{:?}", above.similar);
        assert_eq!(above.similar[0].id, base.memory.id);

        // 6 of 8 words shared with *each* stored memory → cosine 0.75 for
        // both < threshold: silent.
        let below = store
            .remember_detailed(MemoryDraft::new("w0 w1 w2 w3 w4 w5 w9 wa"))
            .unwrap();
        assert!(
            below.similar.is_empty(),
            "cosine 0.75 must stay under the threshold: {:?}",
            below.similar
        );
    }

    #[test]
    fn near_duplicates_see_only_live_same_scope_not_superseded() {
        let mut store = store_with_embedder(&[&["rust"], &["wal"]]);
        let in_x = store
            .remember_detailed(MemoryDraft::new("rust wal").project("x"))
            .unwrap();
        let _global = store
            .remember_detailed(MemoryDraft::new("rust wal"))
            .unwrap();
        let _in_y = store
            .remember_detailed(MemoryDraft::new("rust wal").project("y"))
            .unwrap();

        // Same applied scope only: a draft in "x" sees x's memory, never the
        // global or other-project twins.
        let again_x = store
            .remember_detailed(MemoryDraft::new("rust wal").project("x"))
            .unwrap();
        let ids: Vec<Ulid> = again_x.similar.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![in_x.memory.id], "{:?}", again_x.similar);

        // Tombstoned: gone from the report.
        store.forget(in_x.memory.id).unwrap();
        store.forget(again_x.memory.id).unwrap();
        let after_forget = store
            .remember_detailed(MemoryDraft::new("rust wal").project("x"))
            .unwrap();
        assert!(
            after_forget.similar.is_empty(),
            "{:?}",
            after_forget.similar
        );

        // Superseded: the old version never resurfaces as a near-duplicate —
        // only its live successor does (the supersedes flow S21 suggests).
        let old = store
            .remember_detailed(MemoryDraft::new("rust wal").project("z"))
            .unwrap();
        let new = store
            .remember_detailed(
                MemoryDraft::new("rust wal")
                    .project("z")
                    .supersede(old.memory.id),
            )
            .unwrap();
        let third = store
            .remember_detailed(MemoryDraft::new("rust wal").project("z"))
            .unwrap();
        let ids: Vec<Ulid> = third.similar.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![new.memory.id], "{:?}", third.similar);
    }

    #[test]
    fn near_duplicate_snippets_truncate_on_char_boundaries() {
        // Unit level: multibyte content cut at the cap, never mid-character.
        let long = "é".repeat(NEAR_DUP_SNIPPET_CHARS * 2);
        let cut = near_dup_snippet(&long);
        assert_eq!(cut.chars().count(), NEAR_DUP_SNIPPET_CHARS + 1);
        assert!(cut.ends_with('…'));
        let short = "é".repeat(NEAR_DUP_SNIPPET_CHARS);
        assert_eq!(near_dup_snippet(&short), short, "no cut, no ellipsis");

        // End to end: the reported content of a long near-duplicate is the
        // truncated snippet, not the full text.
        let mut store = store_with_embedder(&[&["rust"], &["wal"]]);
        let padding = "x ".repeat(200); // unknown words: no effect on the vector
        let long_content = format!("rust wal {padding}");
        store
            .remember_detailed(MemoryDraft::new(long_content.clone()))
            .unwrap();
        let again = store
            .remember_detailed(MemoryDraft::new(long_content))
            .unwrap();
        assert_eq!(again.similar.len(), 1);
        let snippet = &again.similar[0].content;
        assert!(snippet.ends_with('…'), "{snippet}");
        assert_eq!(snippet.chars().count(), NEAR_DUP_SNIPPET_CHARS + 1);
    }

    /// Counts embedding calls, forwarding to [`WordEmbedder`] — proves the
    /// near-duplicate scan reuses the write's own embedding (S21's zero-
    /// extra-embedding requirement).
    struct CountingEmbedder {
        inner: WordEmbedder,
        embeds: std::sync::atomic::AtomicUsize,
        chunk_embeds: std::sync::atomic::AtomicUsize,
    }

    impl Embedder for CountingEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            self.embeds
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.embed(text)
        }
        fn embed_chunks(&self, text: &str) -> Result<Vec<Vec<f32>>> {
            self.chunk_embeds
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.embed_chunks(text)
        }
        fn id(&self) -> crate::embed::ModelId {
            self.inner.id()
        }
        fn dims(&self) -> u16 {
            self.inner.dims()
        }
    }

    #[test]
    fn remember_detailed_embeds_exactly_once() {
        let counting = Arc::new(CountingEmbedder {
            inner: WordEmbedder::new(&[&["rust"], &["wal"]]),
            embeds: std::sync::atomic::AtomicUsize::new(0),
            chunk_embeds: std::sync::atomic::AtomicUsize::new(0),
        });
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let opts = StoreOptions {
            embedder: Some(Arc::clone(&counting) as Arc<dyn Embedder>),
            ..StoreOptions::default()
        };
        let mut store = Store::create_with(vfs, Path::new("m.mind"), opts).unwrap();

        store
            .remember_detailed(MemoryDraft::new("rust wal"))
            .unwrap();
        store
            .remember_detailed(MemoryDraft::new("rust wal"))
            .unwrap();

        // One embed_chunks per remember — the scan runs on those vectors, it
        // never embeds again.
        assert_eq!(
            counting
                .chunk_embeds
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert_eq!(
            counting.embeds.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the near-duplicate scan must not re-embed"
        );
    }

    #[test]
    fn remember_detailed_on_a_kv_only_store_stores_with_empty_similar() {
        let (_, mut store) = store();
        let first = store
            .remember_detailed(MemoryDraft::new("no vectors here"))
            .unwrap();
        let second = store
            .remember_detailed(MemoryDraft::new("no vectors here"))
            .unwrap();
        assert!(first.similar.is_empty() && second.similar.is_empty());
        assert_eq!(store.stats().unwrap().live_memories, 2);
    }

    #[test]
    fn open_or_create_semantics_via_explicit_calls() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        // Missing file: open fails, create works.
        assert!(
            Store::open_with(
                Arc::clone(&vfs),
                Path::new("m.mind"),
                StoreOptions::default()
            )
            .is_err()
        );
        let store = Store::create_with(
            Arc::clone(&vfs),
            Path::new("m.mind"),
            StoreOptions::default(),
        )
        .unwrap();
        store.close().unwrap();
        // Existing file: create fails, open works.
        assert!(
            Store::create_with(
                Arc::clone(&vfs),
                Path::new("m.mind"),
                StoreOptions::default()
            )
            .is_err()
        );
        Store::open_with(vfs, Path::new("m.mind"), StoreOptions::default())
            .unwrap()
            .close()
            .unwrap();
    }
}
