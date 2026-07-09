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

use std::collections::BTreeMap;
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
    pub fn remember(&mut self, draft: MemoryDraft) -> Result<Memory> {
        let mut record = MemoryRecord {
            id: Ulid::new(),
            tombstone: false,
            content: draft.content,
            vec_ref: None,
            project: draft.project,
            provenance: Provenance {
                agent: draft.agent,
                session_id: draft.session_id,
                created_at_micros: now_micros(),
            },
            metadata: draft.metadata,
        };

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
        // Full-text index (B2, `docs/adr/0011`): same transaction as the
        // record and vector writes, so the memory is either fully indexed or
        // not stored at all — no torn state to recover into. Runs whether or
        // not an embedder is present: full-text is independent of vectors.
        index::fts::index_document(&mut txn, record.id, &record.content)?;
        let bytes = record.encode()?;
        btree::insert(&mut txn, record.id.to_bytes(), &bytes)?;
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
        let filter_error: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        let keep = |id: Ulid| -> bool {
            if filter_error.borrow().is_some() {
                return false; // a mismatch already occurred; stop admitting
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
        let vec_hits = index::search(
            &self.pager,
            hnsw_meta_page,
            embedder.dims(),
            &vector,
            query.limit,
            SearchParams {
                ef_search: query.ef_search,
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
                |id| Ok(load(id)?.map(|rec| index::fts::doc_len(&rec.content))),
            )?;
            if let Some(e) = filter_error.borrow_mut().take() {
                return Err(e);
            }
            text_hits.iter().map(|h| h.record_id).collect()
        };

        // --- Fuse (RRF k=60, union) --------------------------------------
        let fused = crate::recall::fuse(&vec_ids, &text_ids, query.limit);
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
        // with the type-mismatch error stashed and surfaced after the search.
        let filter_error: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        let keep = |id: Ulid| -> bool {
            if filter_error.borrow().is_some() {
                return false;
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
        let vec_hits = index::search(
            &self.pager,
            hnsw_meta_page,
            embedder.dims(),
            &vector,
            query.limit,
            SearchParams {
                ef_search: query.ef_search,
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
            // doc_len: BM25 length normalization from the current content.
            |id| Ok(load(id)?.map(|rec| index::fts::doc_len(&rec.content))),
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

    /// Fetches one memory by id. Tombstoned (forgotten) memories return
    /// `None`, exactly like absent ones.
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
        txn.commit()?;
        Ok(true)
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
        for memory in self.iter() {
            let memory = memory?;
            // Reconstruct the on-disk record; `iter` already filtered tombstones.
            let record = MemoryRecord {
                id: memory.id,
                tombstone: false,
                content: memory.content,
                vec_ref: None,
                project: memory.project,
                provenance: memory.provenance,
                metadata: memory.metadata,
            };
            dst.insert_record(record)?;
        }
        dst.close()
    }

    /// Inserts a fully-formed [`MemoryRecord`] verbatim (id, provenance and
    /// metadata preserved), re-deriving its vector and full-text index entries
    /// — the write half of [`Store::vacuum`]'s rebuild. Unlike [`Store::remember`]
    /// it neither mints a new id nor a timestamp; the record is stored as given.
    fn insert_record(&mut self, mut record: MemoryRecord) -> Result<Memory> {
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
        let bytes = record.encode()?;
        btree::insert(&mut txn, record.id.to_bytes(), &bytes)?;
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
        for memory in self.iter_all() {
            if memory?.tombstone {
                forgotten_memories += 1;
            } else {
                live_memories += 1;
            }
        }
        let header = self.pager.header();
        Ok(StoreStats {
            live_memories,
            forgotten_memories,
            index_entries: index::node_count(&self.pager, header.hnsw_meta_page)?,
            fts_documents: index::fts::indexed_documents(&self.pager, header.fts_root_page)?,
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
}

/// Whether a record is live and within `query`'s project scope — the shared
/// liveness/scope half of every `recall`/`search_text` `keep` predicate.
/// Metadata filters ([`Query::record_passes_filters`]) compose on top of this.
fn in_scope(query: &Query, rec: &MemoryRecord) -> bool {
    !rec.tombstone
        && match &query.scope {
            Scope::All => true,
            Scope::Project(p) => rec.project.as_deref() == Some(p.as_str()),
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
    /// HNSW candidate list size at layer 0 (`docs/adr/0002`): higher trades
    /// latency for recall.
    ef_search: u16,
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
            ef_search: crate::format::HNSW_DEFAULT_EF_SEARCH,
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

    /// Overrides the HNSW `ef_search` for this query (default
    /// [`crate::format::HNSW_DEFAULT_EF_SEARCH`]).
    pub fn ef_search(mut self, ef_search: u16) -> Self {
        self.ef_search = ef_search;
        self
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
        }
    }
}

/// What [`Store::stats`] reports — the numbers behind `embedmind stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreStats {
    /// Memories that `iter`/`get`/`recall` can see.
    pub live_memories: u64,
    /// Tombstoned memories awaiting `vacuum` (`docs/adr/0003`).
    pub forgotten_memories: u64,
    /// HNSW graph entries — one per indexed chunk, so a long memory
    /// (DESIGN §6) counts once per chunk. 0 = no vector index yet.
    pub index_entries: u64,
    /// Documents in the full-text index (`docs/adr/0011`); one per live
    /// `remember`. 0 = no full-text index yet (e.g. a pre-M2 file).
    pub fts_documents: u64,
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
    /// contributes.
    pub score: f32,
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
