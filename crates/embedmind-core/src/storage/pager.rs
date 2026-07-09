//! The pager: transactional page access over a `.mind` file + WAL sidecar.
//!
//! Responsibilities: header (page 0) lifecycle, page reads with checksum
//! verification (guarantee G1), transaction commit through the WAL
//! (guarantee G2), recovery on every open, checkpointing, and the
//! single-writer file lock (`docs/FORMAT.md` §9, `docs/adr/0006`).
//!
//! Write path: a [`Txn`] buffers dirty pages in memory; `commit` appends them
//! plus the updated header page to the WAL and fsyncs — the transaction is
//! durable iff the commit frame is valid on disk. Reads check the
//! transaction's own dirty set, then committed-but-not-checkpointed WAL
//! images, then the main file.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::{
    DEFAULT_PAGE_SIZE, HEADER_PEEK_LEN, Header, MAX_MODEL_ID_LEN, page_checksum_is_valid,
    stamp_page_checksum,
};
use crate::storage::vfs::{OpenMode, Vfs, VfsFile};
use crate::storage::wal::{self, Wal};

/// Default WAL size that triggers a checkpoint (`docs/FORMAT.md` §8).
pub const DEFAULT_CHECKPOINT_THRESHOLD: u64 = 4 * 1024 * 1024;

/// Pager tuning knobs.
#[derive(Debug, Clone, Copy)]
pub struct PagerOptions {
    /// Page size for newly created files ([`Pager::open`] always uses the
    /// value recorded in the header).
    pub page_size: u32,
    /// WAL size, in bytes, at which a commit triggers a checkpoint.
    pub checkpoint_threshold: u64,
}

impl Default for PagerOptions {
    fn default() -> Self {
        PagerOptions {
            page_size: DEFAULT_PAGE_SIZE,
            checkpoint_threshold: DEFAULT_CHECKPOINT_THRESHOLD,
        }
    }
}

/// Transactional pager over one `.mind` file. Single writer per file,
/// enforced with an advisory exclusive lock held for the pager's lifetime.
pub struct Pager {
    vfs: Arc<dyn Vfs>,
    main: Box<dyn VfsFile>,
    wal: Wal,
    wal_path: PathBuf,
    header: Header,
    /// Committed pages that live in the WAL and not yet in the main file:
    /// `page_no → image offset in the WAL`.
    wal_index: HashMap<u64, u64>,
    checkpoint_threshold: u64,
    /// Set after an I/O error during commit/checkpoint left the in-memory
    /// state unreliable. Every later call fails; reopening the file recovers
    /// (the on-disk state is always consistent — that is the WAL's job).
    broken: bool,
}

impl Pager {
    /// Creates a new store at `path` (fails if it exists) and takes the
    /// writer lock.
    pub fn create(vfs: Arc<dyn Vfs>, path: &Path, opts: PagerOptions) -> Result<Self> {
        let header = Header::new(opts.page_size)?;
        let main = vfs.open(path, OpenMode::CreateNew)?;
        if !main.try_lock_exclusive()? {
            return Err(Error::WriteLocked);
        }
        let wal_path = wal_path_for(path);
        if vfs.exists(&wal_path) {
            // Orphan WAL from a deleted store: its generation is dead.
            vfs.delete(&wal_path)?;
        }

        let mut page0 = vec![0u8; opts.page_size as usize];
        header.encode(&mut page0)?;
        main.write_at(&page0, 0)?;
        main.sync()?;

        let wal_file = vfs.open(&wal_path, OpenMode::OpenOrCreate)?;
        Ok(Pager {
            vfs,
            main,
            wal: Wal::new(wal_file, opts.page_size),
            wal_path,
            header,
            wal_index: HashMap::new(),
            checkpoint_threshold: opts.checkpoint_threshold,
            broken: false,
        })
    }

    /// Opens an existing store, running WAL recovery first (`docs/FORMAT.md`
    /// §8): committed WAL pages are checkpointed into the main file and the
    /// WAL is reset, so a freshly opened store is fully materialized.
    pub fn open(vfs: Arc<dyn Vfs>, path: &Path, opts: PagerOptions) -> Result<Self> {
        let main = vfs.open(path, OpenMode::MustExist)?;
        if !main.try_lock_exclusive()? {
            return Err(Error::WriteLocked);
        }
        let wal_path = wal_path_for(path);
        let wal_file = vfs.open(&wal_path, OpenMode::OpenOrCreate)?;

        // Recovery. The scan is self-contained (the WAL header records page
        // size and salt) so it works even when the main header itself was
        // torn by a crash mid-checkpoint — the WAL then contains the
        // committed image of page 0 and the apply below repairs it.
        if let Some(recovered) = wal::scan(&*wal_file)? {
            let expected = peek_main_page_size(&*main)?;
            if expected.is_none_or(|ps| ps == recovered.page_size)
                && !recovered.committed.is_empty()
            {
                let page_size = u64::from(recovered.page_size);
                let mut image = vec![0u8; recovered.page_size as usize];
                for (&page_no, &offset) in &recovered.committed {
                    wal_file.read_at(&mut image, offset)?;
                    let target = page_no
                        .checked_mul(page_size)
                        .ok_or(Error::InvalidArgument("page offset overflow"))?;
                    main.write_at(&image, target)?;
                }
                main.sync()?;
            }
        }
        if wal_file.len()? > 0 {
            wal_file.truncate(0)?;
            wal_file.sync()?;
        }

        // Now the main file is authoritative; read and validate the header.
        let header = read_header(&*main)?;
        Ok(Pager {
            vfs,
            main,
            wal: Wal::new(wal_file, header.page_size),
            wal_path,
            header,
            wal_index: HashMap::new(),
            checkpoint_threshold: opts.checkpoint_threshold,
            broken: false,
        })
    }

    /// The current committed header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Total pages, including the header page.
    pub fn page_count(&self) -> u64 {
        self.header.page_count
    }

    /// Reads one page (committed state), verifying its checksum (G1).
    pub fn read_page(&self, page_no: u64) -> Result<Vec<u8>> {
        self.ensure_usable()?;
        if page_no >= self.header.page_count {
            return Err(Error::PageOutOfBounds {
                page_no,
                page_count: self.header.page_count,
            });
        }
        let mut page = vec![0u8; self.header.page_size as usize];
        if let Some(&offset) = self.wal_index.get(&page_no) {
            self.wal.read_image(offset, &mut page)?;
        } else {
            let offset = page_no
                .checked_mul(u64::from(self.header.page_size))
                .ok_or(Error::InvalidArgument("page offset overflow"))?;
            self.main.read_at(&mut page, offset)?;
        }
        if !page_checksum_is_valid(&page) {
            return Err(Error::CorruptPage { page_no });
        }
        Ok(page)
    }

    /// Starts a transaction. Dropping the returned [`Txn`] without calling
    /// [`Txn::commit`] rolls it back (nothing was written).
    pub fn begin(&mut self) -> Result<Txn<'_>> {
        self.ensure_usable()?;
        let page_count = self.header.page_count;
        Ok(Txn {
            pager: self,
            dirty: BTreeMap::new(),
            page_count,
            root_btree: None,
            hnsw_meta: None,
            fts_root: None,
            embedding: None,
        })
    }

    /// Copies committed WAL pages into the main file, fsyncs it, and resets
    /// the WAL (`docs/FORMAT.md` §8). Runs automatically when the WAL passes
    /// the configured threshold and on [`Pager::close`].
    pub fn checkpoint(&mut self) -> Result<()> {
        self.ensure_usable()?;
        if self.wal_index.is_empty() && self.wal.size() == 0 {
            return Ok(());
        }
        let result = self.checkpoint_inner();
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    fn checkpoint_inner(&mut self) -> Result<()> {
        let page_size = u64::from(self.header.page_size);
        let mut image = vec![0u8; self.header.page_size as usize];
        for (&page_no, &offset) in &self.wal_index {
            self.wal.read_image(offset, &mut image)?;
            let target = page_no
                .checked_mul(page_size)
                .ok_or(Error::InvalidArgument("page offset overflow"))?;
            self.main.write_at(&image, target)?;
        }
        self.main.sync()?;
        self.wal.reset()?;
        self.wal_index.clear();
        Ok(())
    }

    /// Cleanly closes the store: checkpoint, then delete the WAL sidecar so
    /// a cleanly closed store is a single file (`docs/FORMAT.md` §1).
    pub fn close(mut self) -> Result<()> {
        self.checkpoint()?;
        let Pager {
            vfs,
            wal,
            wal_path,
            main,
            ..
        } = self;
        drop(wal); // release the handle before deleting (Windows)
        if vfs.exists(&wal_path) {
            vfs.delete(&wal_path)?;
        }
        drop(main); // releases the writer lock
        Ok(())
    }

    fn ensure_usable(&self) -> Result<()> {
        if self.broken {
            return Err(Error::Io(std::io::Error::other(
                "store is in a failed state after an I/O error; reopen the file",
            )));
        }
        Ok(())
    }

    fn commit_txn(
        &mut self,
        dirty: BTreeMap<u64, Vec<u8>>,
        new_page_count: u64,
        new_root_btree: Option<u64>,
        new_hnsw_meta: Option<u64>,
        new_fts_root: Option<u64>,
        new_embedding: Option<(String, u16)>,
    ) -> Result<u64> {
        self.ensure_usable()?;
        if dirty.is_empty()
            && new_page_count == self.header.page_count
            && new_root_btree.is_none()
            && new_hnsw_meta.is_none()
            && new_fts_root.is_none()
            && new_embedding.is_none()
        {
            return Ok(self.header.txn_counter); // empty transaction: no-op
        }

        let mut new_header = self.header.clone();
        new_header.page_count = new_page_count;
        if let Some(root) = new_root_btree {
            new_header.root_btree_page = root;
        }
        if let Some(hnsw_meta) = new_hnsw_meta {
            new_header.hnsw_meta_page = hnsw_meta;
        }
        if let Some(fts_root) = new_fts_root {
            new_header.fts_root_page = fts_root;
        }
        if let Some((model_id, dims)) = new_embedding {
            new_header.embedding_model_id = model_id;
            new_header.embedding_dims = dims;
        }
        new_header.txn_counter += 1;
        let txn_id = new_header.txn_counter;
        let mut page0 = vec![0u8; self.header.page_size as usize];
        new_header.encode(&mut page0)?;

        // Frames: dirty pages first, then the header page as the commit
        // frame — so the new txn_counter/page_count become durable exactly
        // when the transaction does.
        let mut frames: Vec<(u64, &[u8])> = Vec::with_capacity(dirty.len() + 1);
        for (page_no, image) in &dirty {
            frames.push((*page_no, image));
        }
        frames.push((0, &page0));

        match self.wal.append_txn(txn_id, &frames) {
            Ok(offsets) => {
                self.header = new_header;
                self.wal_index.extend(offsets);
            }
            Err(e) => {
                // The commit may or may not have hit disk; in-memory state no
                // longer matches. Recovery on reopen resolves it.
                self.broken = true;
                return Err(e);
            }
        }

        if self.wal.size() >= self.checkpoint_threshold {
            self.checkpoint()?;
        }
        Ok(txn_id)
    }
}

/// An in-flight transaction. All writes are buffered; nothing touches disk
/// until [`Txn::commit`]. Dropping rolls back.
pub struct Txn<'p> {
    pager: &'p mut Pager,
    /// Dirty pages, already checksum-stamped.
    dirty: BTreeMap<u64, Vec<u8>>,
    /// Working page count (grows with allocations).
    page_count: u64,
    /// Pending B-tree root move; applied to the header atomically with the
    /// commit frame (the root pointer lives in page 0).
    root_btree: Option<u64>,
    /// Pending HNSW meta page move; applied to the header atomically with the
    /// commit frame, same as `root_btree`.
    hnsw_meta: Option<u64>,
    /// Pending full-text dictionary root move (`docs/adr/0011`); applied to
    /// the header atomically with the commit frame, same as `root_btree`.
    fts_root: Option<u64>,
    /// Pending embedding model id + dims stamp (set once on a fresh store,
    /// `docs/adr/0004`); applied to the header atomically with the commit
    /// frame, discarded on rollback.
    embedding: Option<(String, u16)>,
}

impl Txn<'_> {
    /// Allocates a fresh page at the end of the file and returns its number.
    /// The page starts zeroed (with a valid checksum) and may be overwritten
    /// with [`Txn::write_page`] before commit.
    pub fn allocate_page(&mut self) -> Result<u64> {
        let page_no = self.page_count;
        self.page_count = self
            .page_count
            .checked_add(1)
            .ok_or(Error::InvalidArgument("page count overflow"))?;
        let mut page = vec![0u8; self.pager.header.page_size as usize];
        stamp_page_checksum(&mut page);
        self.dirty.insert(page_no, page);
        Ok(page_no)
    }

    /// Buffers a full-page write. `data` must be exactly one page; its
    /// trailer bytes are overwritten with the checksum. Page 0 is managed by
    /// the pager and cannot be written directly.
    pub fn write_page(&mut self, page_no: u64, data: &[u8]) -> Result<()> {
        if data.len() != self.pager.header.page_size as usize {
            return Err(Error::InvalidArgument(
                "page write must be exactly one page",
            ));
        }
        if page_no == 0 {
            return Err(Error::InvalidArgument(
                "page 0 (header) is managed by the pager",
            ));
        }
        if page_no >= self.page_count {
            return Err(Error::PageOutOfBounds {
                page_no,
                page_count: self.page_count,
            });
        }
        let mut page = data.to_vec();
        stamp_page_checksum(&mut page);
        self.dirty.insert(page_no, page);
        Ok(())
    }

    /// Reads a page as seen by this transaction (its own writes included).
    pub fn read_page(&self, page_no: u64) -> Result<Vec<u8>> {
        if let Some(page) = self.dirty.get(&page_no) {
            return Ok(page.clone());
        }
        if page_no >= self.page_count {
            return Err(Error::PageOutOfBounds {
                page_no,
                page_count: self.page_count,
            });
        }
        self.pager.read_page(page_no)
    }

    /// Page count as seen by this transaction.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Page size of the underlying store.
    pub fn page_size(&self) -> u32 {
        self.pager.header.page_size
    }

    /// Record B-tree root as seen by this transaction (its own pending move
    /// included); 0 = no tree yet.
    pub fn root_btree_page(&self) -> u64 {
        self.root_btree.unwrap_or(self.pager.header.root_btree_page)
    }

    /// Moves the record B-tree root. Becomes durable with the commit frame;
    /// discarded on rollback like any other buffered write.
    pub fn set_root_btree_page(&mut self, page_no: u64) {
        self.root_btree = Some(page_no);
    }

    /// HNSW meta page as seen by this transaction (its own pending move
    /// included); 0 = no vector index yet.
    pub fn hnsw_meta_page(&self) -> u64 {
        self.hnsw_meta.unwrap_or(self.pager.header.hnsw_meta_page)
    }

    /// Moves the HNSW meta page pointer. Becomes durable with the commit
    /// frame; discarded on rollback like any other buffered write.
    pub fn set_hnsw_meta_page(&mut self, page_no: u64) {
        self.hnsw_meta = Some(page_no);
    }

    /// Full-text dictionary root as seen by this transaction (its own pending
    /// move included); 0 = no full-text index yet (`docs/adr/0011`).
    pub fn fts_root_page(&self) -> u64 {
        self.fts_root.unwrap_or(self.pager.header.fts_root_page)
    }

    /// Moves the full-text dictionary root pointer. Becomes durable with the
    /// commit frame; discarded on rollback like any other buffered write.
    pub fn set_fts_root_page(&mut self, page_no: u64) {
        self.fts_root = Some(page_no);
    }

    /// Stamps the header's embedding `model_id` + `dims` — done once against a
    /// fresh store so that mixing embeddings from different models in one file
    /// is impossible (`docs/adr/0004`, `docs/FORMAT.md` §6). Becomes durable
    /// with the commit frame; discarded on rollback.
    pub fn set_embedding_model(&mut self, model_id: &str, dims: u16) -> Result<()> {
        if model_id.len() > MAX_MODEL_ID_LEN {
            return Err(Error::InvalidArgument(
                "embedding_model_id exceeds 64 bytes",
            ));
        }
        self.embedding = Some((model_id.to_owned(), dims));
        Ok(())
    }

    /// Commits: appends all dirty pages + the updated header to the WAL and
    /// fsyncs. Returns the transaction id. Durable iff `Ok` (guarantee G2).
    pub fn commit(self) -> Result<u64> {
        let Txn {
            pager,
            dirty,
            page_count,
            root_btree,
            hnsw_meta,
            fts_root,
            embedding,
        } = self;
        pager.commit_txn(
            dirty, page_count, root_btree, hnsw_meta, fts_root, embedding,
        )
    }
}

/// WAL sidecar path: `memory.mind` → `memory.mind-wal` (`docs/FORMAT.md` §1).
fn wal_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

/// Reads the recorded page size from page 0, if the prefix parses at all.
/// `Ok(None)` means the header is unreadable — the caller decides whether the
/// WAL can repair it.
fn peek_main_page_size(main: &dyn VfsFile) -> Result<Option<u32>> {
    if main.len()? < HEADER_PEEK_LEN as u64 {
        return Ok(None);
    }
    let mut prefix = [0u8; HEADER_PEEK_LEN];
    main.read_at(&mut prefix, 0)?;
    Ok(Header::peek_page_size(&prefix).ok())
}

/// Reads and validates the full header from the main file.
fn read_header(main: &dyn VfsFile) -> Result<Header> {
    if main.len()? < HEADER_PEEK_LEN as u64 {
        return Err(Error::BadHeader);
    }
    let mut prefix = [0u8; HEADER_PEEK_LEN];
    main.read_at(&mut prefix, 0)?;
    let page_size = Header::peek_page_size(&prefix)?;
    if main.len()? < u64::from(page_size) {
        return Err(Error::BadHeader);
    }
    let mut page0 = vec![0u8; page_size as usize];
    main.read_at(&mut page0, 0)?;
    Header::decode(&page0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::storage::sim::SimVfs;

    const PS: u32 = DEFAULT_PAGE_SIZE;

    fn opts() -> PagerOptions {
        PagerOptions::default()
    }

    fn sim() -> (Arc<dyn Vfs>, SimVfs) {
        let sim = SimVfs::new();
        (Arc::new(sim.clone()), sim)
    }

    fn path() -> &'static Path {
        Path::new("memory.mind")
    }

    fn filled(byte: u8) -> Vec<u8> {
        vec![byte; PS as usize]
    }

    #[test]
    fn create_write_reopen_roundtrip() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(Arc::clone(&vfs), path(), opts()).unwrap();
        let mut txn = pager.begin().unwrap();
        let a = txn.allocate_page().unwrap();
        let b = txn.allocate_page().unwrap();
        txn.write_page(a, &filled(0xAA)).unwrap();
        txn.write_page(b, &filled(0xBB)).unwrap();
        assert_eq!(txn.commit().unwrap(), 1);
        assert_eq!(pager.page_count(), 3);
        pager.close().unwrap();

        // Cleanly closed store is a single file.
        assert!(!vfs.exists(&wal_path_for(path())));

        let pager = Pager::open(vfs, path(), opts()).unwrap();
        assert_eq!(pager.header().txn_counter, 1);
        assert_eq!(&pager.read_page(a).unwrap()[..16], &[0xAA; 16]);
        assert_eq!(&pager.read_page(b).unwrap()[..16], &[0xBB; 16]);
    }

    #[test]
    fn dropped_txn_rolls_back() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(vfs, path(), opts()).unwrap();
        {
            let mut txn = pager.begin().unwrap();
            let p = txn.allocate_page().unwrap();
            txn.write_page(p, &filled(1)).unwrap();
            // no commit
        }
        assert_eq!(pager.page_count(), 1);
        assert_eq!(pager.header().txn_counter, 0);
    }

    #[test]
    fn txn_sees_own_writes_and_committed_state() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(vfs, path(), opts()).unwrap();
        let mut txn = pager.begin().unwrap();
        let p = txn.allocate_page().unwrap();
        txn.write_page(p, &filled(7)).unwrap();
        assert_eq!(&txn.read_page(p).unwrap()[..8], &[7; 8]);
        txn.commit().unwrap();

        let mut txn = pager.begin().unwrap();
        assert_eq!(&txn.read_page(p).unwrap()[..8], &[7; 8]);
        txn.write_page(p, &filled(8)).unwrap();
        drop(txn); // rollback
        assert_eq!(&pager.read_page(p).unwrap()[..8], &[7; 8]);
    }

    #[test]
    fn reopen_without_close_recovers_from_wal() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(Arc::clone(&vfs), path(), opts()).unwrap();
        let mut txn = pager.begin().unwrap();
        let p = txn.allocate_page().unwrap();
        txn.write_page(p, &filled(0xCC)).unwrap();
        txn.commit().unwrap();
        drop(pager); // simulates a process that never checkpointed

        let pager = Pager::open(vfs, path(), opts()).unwrap();
        assert_eq!(pager.header().txn_counter, 1);
        assert_eq!(&pager.read_page(p).unwrap()[..8], &[0xCC; 8]);
    }

    #[test]
    fn checkpoint_threshold_triggers_and_preserves_data() {
        let (vfs, sim) = sim();
        let mut pager = Pager::create(
            Arc::clone(&vfs),
            path(),
            PagerOptions {
                checkpoint_threshold: 1,
                ..opts()
            },
        )
        .unwrap();
        let mut txn = pager.begin().unwrap();
        let p = txn.allocate_page().unwrap();
        txn.write_page(p, &filled(0xDD)).unwrap();
        txn.commit().unwrap(); // threshold 1 → immediate checkpoint

        // WAL was reset; data must come from the main file.
        assert_eq!(sim.snapshot(&wal_path_for(path())).unwrap().len(), 0);
        assert_eq!(&pager.read_page(p).unwrap()[..8], &[0xDD; 8]);
    }

    #[test]
    fn second_writer_is_rejected() {
        let (vfs, _) = sim();
        let _pager = Pager::create(Arc::clone(&vfs), path(), opts()).unwrap();
        assert!(matches!(
            Pager::open(Arc::clone(&vfs), path(), opts()),
            Err(Error::WriteLocked)
        ));
    }

    #[test]
    fn out_of_bounds_and_bad_args_are_typed_errors() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(vfs, path(), opts()).unwrap();
        assert!(matches!(
            pager.read_page(99),
            Err(Error::PageOutOfBounds {
                page_no: 99,
                page_count: 1
            })
        ));
        let mut txn = pager.begin().unwrap();
        assert!(matches!(
            txn.write_page(0, &filled(0)),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            txn.write_page(5, &filled(0)),
            Err(Error::PageOutOfBounds { .. })
        ));
        assert!(matches!(
            txn.write_page(1, &[0u8; 3]),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn corrupt_main_page_is_detected() {
        let (vfs, sim) = sim();
        let mut pager = Pager::create(Arc::clone(&vfs), path(), opts()).unwrap();
        let mut txn = pager.begin().unwrap();
        let p = txn.allocate_page().unwrap();
        txn.write_page(p, &filled(5)).unwrap();
        txn.commit().unwrap();
        pager.close().unwrap();

        // Flip one byte of the page on "disk".
        let f = sim.open(path(), OpenMode::MustExist).unwrap();
        let mut byte = [0u8; 1];
        let off = u64::from(PS) * p + 10;
        f.read_at(&mut byte, off).unwrap();
        byte[0] ^= 0xff;
        f.write_at(&byte, off).unwrap();
        drop(f);

        let pager = Pager::open(vfs, path(), opts()).unwrap();
        assert!(matches!(pager.read_page(p), Err(Error::CorruptPage { page_no }) if page_no == p));
    }

    #[test]
    fn open_missing_or_invalid_file_fails_clearly() {
        let (vfs, sim) = sim();
        assert!(matches!(
            Pager::open(Arc::clone(&vfs), path(), opts()),
            Err(Error::Io(_))
        ));
        let f = sim.open(path(), OpenMode::CreateNew).unwrap();
        f.write_at(b"definitely not a mind file, but long enough to peek", 0)
            .unwrap();
        drop(f);
        assert!(matches!(
            Pager::open(vfs, path(), opts()),
            Err(Error::BadHeader)
        ));
    }

    #[test]
    fn root_btree_move_commits_rolls_back_and_survives_reopen() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(Arc::clone(&vfs), path(), opts()).unwrap();
        let mut txn = pager.begin().unwrap();
        let p = txn.allocate_page().unwrap();
        txn.write_page(p, &filled(1)).unwrap();
        assert_eq!(txn.root_btree_page(), 0);
        txn.set_root_btree_page(p);
        assert_eq!(txn.root_btree_page(), p);
        txn.commit().unwrap();
        assert_eq!(pager.header().root_btree_page, p);

        // Rollback discards a pending root move.
        let mut txn = pager.begin().unwrap();
        txn.set_root_btree_page(0);
        drop(txn);
        assert_eq!(pager.header().root_btree_page, p);

        drop(pager); // reopen via WAL recovery
        let pager = Pager::open(vfs, path(), opts()).unwrap();
        assert_eq!(pager.header().root_btree_page, p);
    }

    #[test]
    fn empty_txn_is_a_noop() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(vfs, path(), opts()).unwrap();
        let txn = pager.begin().unwrap();
        assert_eq!(txn.commit().unwrap(), 0);
        assert_eq!(pager.header().txn_counter, 0);
    }

    #[test]
    fn hnsw_meta_move_commits_rolls_back_and_survives_reopen() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(Arc::clone(&vfs), path(), opts()).unwrap();
        let mut txn = pager.begin().unwrap();
        let p = txn.allocate_page().unwrap();
        txn.write_page(p, &filled(2)).unwrap();
        assert_eq!(txn.hnsw_meta_page(), 0);
        txn.set_hnsw_meta_page(p);
        assert_eq!(txn.hnsw_meta_page(), p);
        txn.commit().unwrap();
        assert_eq!(pager.header().hnsw_meta_page, p);

        // Rollback discards a pending hnsw_meta move.
        let mut txn = pager.begin().unwrap();
        txn.set_hnsw_meta_page(0);
        drop(txn);
        assert_eq!(pager.header().hnsw_meta_page, p);

        drop(pager); // reopen via WAL recovery
        let pager = Pager::open(vfs, path(), opts()).unwrap();
        assert_eq!(pager.header().hnsw_meta_page, p);
    }

    #[test]
    fn fts_root_move_commits_rolls_back_and_survives_reopen() {
        let (vfs, _) = sim();
        let mut pager = Pager::create(Arc::clone(&vfs), path(), opts()).unwrap();
        let mut txn = pager.begin().unwrap();
        let p = txn.allocate_page().unwrap();
        txn.write_page(p, &filled(3)).unwrap();
        assert_eq!(txn.fts_root_page(), 0);
        txn.set_fts_root_page(p);
        assert_eq!(txn.fts_root_page(), p);
        txn.commit().unwrap();
        assert_eq!(pager.header().fts_root_page, p);

        // Rollback discards a pending fts_root move.
        let mut txn = pager.begin().unwrap();
        txn.set_fts_root_page(0);
        drop(txn);
        assert_eq!(pager.header().fts_root_page, p);

        drop(pager); // reopen via WAL recovery
        let pager = Pager::open(vfs, path(), opts()).unwrap();
        assert_eq!(pager.header().fts_root_page, p);
    }
}
