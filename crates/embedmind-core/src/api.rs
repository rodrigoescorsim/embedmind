//! Public API of the engine: [`Store`], [`Memory`], [`MemoryDraft`].
//!
//! This is the only module the shells (`embedmind-mcp`, `embedmind-cli`) and
//! future bindings are allowed to depend on. Data model: `DESIGN.md` §3.2.
//!
//! M1 item 1.2 scope: durable KV over the record B-tree — `remember`, `get`,
//! `forget` (tombstone, `docs/adr/0003`), timeline iteration. Vector recall
//! (`Query`) arrives with the index layer (item 1.3) as a non-breaking
//! addition.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::record::{MemoryRecord, Provenance, Scalar};
use crate::storage::btree;
use crate::storage::{Pager, PagerOptions, RealVfs, Vfs};

/// Store tuning knobs. The defaults are right for almost everyone.
#[derive(Debug, Clone, Copy)]
pub struct StoreOptions {
    /// Page size for newly created files (existing files use the size
    /// recorded in their header).
    pub page_size: u32,
    /// WAL size, in bytes, at which a commit triggers a checkpoint.
    pub checkpoint_threshold: u64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        let p = PagerOptions::default();
        StoreOptions {
            page_size: p.page_size,
            checkpoint_threshold: p.checkpoint_threshold,
        }
    }
}

impl StoreOptions {
    fn pager(self) -> PagerOptions {
        PagerOptions {
            page_size: self.page_size,
            checkpoint_threshold: self.checkpoint_threshold,
        }
    }
}

/// A memory store: one crash-safe `.mind` file. Single writer per file
/// (`docs/adr/0006`); every mutating call is one durable transaction —
/// when it returns `Ok`, the data survives `kill -9` and power loss.
pub struct Store {
    pager: Pager,
}

impl Store {
    /// Creates a new store at `path`. Fails if the file exists.
    pub fn create(path: &Path) -> Result<Store> {
        Self::create_with(Arc::new(RealVfs), path, StoreOptions::default())
    }

    /// Opens an existing store (recovery runs automatically).
    pub fn open(path: &Path) -> Result<Store> {
        Self::open_with(Arc::new(RealVfs), path, StoreOptions::default())
    }

    /// Opens `path`, creating it first if it does not exist — what the
    /// shells use on startup.
    pub fn open_or_create(path: &Path) -> Result<Store> {
        let vfs: Arc<dyn Vfs> = Arc::new(RealVfs);
        if vfs.exists(path) {
            Self::open_with(vfs, path, StoreOptions::default())
        } else {
            Self::create_with(vfs, path, StoreOptions::default())
        }
    }

    /// [`Store::create`] with an explicit [`Vfs`] and options — the seam the
    /// crash harness and fuzzers use; embedders may too.
    pub fn create_with(vfs: Arc<dyn Vfs>, path: &Path, opts: StoreOptions) -> Result<Store> {
        Ok(Store {
            pager: Pager::create(vfs, path, opts.pager())?,
        })
    }

    /// [`Store::open`] with an explicit [`Vfs`] and options.
    pub fn open_with(vfs: Arc<dyn Vfs>, path: &Path, opts: StoreOptions) -> Result<Store> {
        Ok(Store {
            pager: Pager::open(vfs, path, opts.pager())?,
        })
    }

    /// Stores one memory durably and returns it (with its generated id and
    /// timestamp). The embedding is attached by the index layer (M1 1.3);
    /// until then memories are stored without vectors.
    pub fn remember(&mut self, draft: MemoryDraft) -> Result<Memory> {
        let record = MemoryRecord {
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
        let bytes = record.encode()?;
        let mut txn = self.pager.begin()?;
        btree::insert(&mut txn, record.id.to_bytes(), &bytes)?;
        txn.commit()?;
        Ok(Memory::from_record(record))
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
