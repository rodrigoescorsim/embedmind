//! Simulated storage for deterministic crash testing (`docs/TESTING.md` §2).
//!
//! [`SimVfs`] is an in-memory [`Vfs`] that models what a real disk does on
//! power loss: each file keeps a *synced* image (durable) and a *current*
//! image (page cache). `sync` promotes current → synced; [`SimVfs::power_fail`]
//! rebuilds every file from a seeded per-sector (512-byte) mix of the two,
//! which covers torn writes, partially persisted appends and reordered sector
//! flushes. Kill points are armed with [`SimVfs::arm_crash`]: the N-th
//! mutating I/O operation fails — either before doing anything
//! ([`CrashMode::Before`]) or, for writes, after persisting a random subset of
//! sectors ([`CrashMode::Torn`]) — and every operation after it fails too, as
//! in a dead process. `(workload, injection point, seed)` fully reproduces a
//! failure.
//!
//! This module is part of the public crate surface so integration tests,
//! fuzz targets and downstream benchmarks can reuse it. It contains no
//! domain logic.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use super::vfs::{OpenMode, Vfs, VfsFile};

/// Sector granularity for torn-write simulation, matching what real disks
/// guarantee (at best) on power loss.
pub const SECTOR_SIZE: usize = 512;

/// Where and how an armed crash fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashMode {
    /// The chosen operation fails before having any effect.
    Before,
    /// A `write_at` persists a seeded random subset of its sectors, then
    /// fails (torn write). For non-write operations this degrades to
    /// [`CrashMode::Before`].
    Torn,
}

/// Deterministic small RNG (splitmix64) used for sector survival choices.
/// Exposed so the crash harness can share it for workload generation.
#[derive(Debug, Clone)]
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    /// Next pseudo-random value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// A coin flip.
    pub fn next_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

#[derive(Default)]
struct SimFileData {
    /// What is durably on "disk" (as of the last honest sync).
    synced: Vec<u8>,
    /// What the process sees (OS page cache).
    current: Vec<u8>,
    /// Owner handle id of the exclusive lock, if held.
    locked_by: Option<u64>,
}

#[derive(Default)]
struct SimState {
    files: Mutex<HashMap<PathBuf, SimFileData>>,
    /// Count of mutating operations (write/sync/truncate) so far.
    ops: AtomicU64,
    /// `(op_index, mode, torn_seed)` to crash at, if armed.
    crash_at: Mutex<Option<(u64, CrashMode, u64)>>,
    /// Once true, every operation fails (the process is "dead").
    crashed: AtomicBool,
    /// When true, `sync` reports success without making anything durable.
    lying_sync: AtomicBool,
    next_handle_id: AtomicU64,
}

/// In-memory fault-injecting VFS. Cheap to clone (shared state).
#[derive(Clone, Default)]
pub struct SimVfs {
    state: Arc<SimState>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Test infrastructure: recover from poison instead of panicking (the
    // engine itself never panics, so poison here means a test bug anyway).
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn crash_error() -> io::Error {
    io::Error::other("simulated power failure")
}

impl SimVfs {
    /// A fresh, empty simulated file system.
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms a crash at mutating operation number `at_op` (0-based, as counted
    /// by [`SimVfs::op_count`]). `torn_seed` drives sector survival in
    /// [`CrashMode::Torn`].
    pub fn arm_crash(&self, at_op: u64, mode: CrashMode, torn_seed: u64) {
        *lock(&self.state.crash_at) = Some((at_op, mode, torn_seed));
    }

    /// Total mutating operations performed so far (dry-run measurement for
    /// automatic kill-point enumeration).
    pub fn op_count(&self) -> u64 {
        self.state.ops.load(Ordering::SeqCst)
    }

    /// Whether an armed crash has fired.
    pub fn crashed(&self) -> bool {
        self.state.crashed.load(Ordering::SeqCst)
    }

    /// Enables lying-fsync mode: `sync` returns `Ok` but nothing becomes
    /// durable. Documents which guarantees survive broken hardware
    /// (integrity always; durability of the last commits, no).
    pub fn set_lying_sync(&self, lying: bool) {
        self.state.lying_sync.store(lying, Ordering::SeqCst);
    }

    /// Simulates power loss and reboot: every file is rebuilt from a seeded
    /// per-sector mix of its synced and current images, all locks are
    /// released and the crash state is cleared. Stale handles from before the
    /// "reboot" must be dropped, not reused.
    pub fn power_fail(&self, seed: u64) {
        let mut rng = SplitMix64(seed);
        let mut files = lock(&self.state.files);
        for data in files.values_mut() {
            let final_len = if rng.next_bool() {
                data.current.len()
            } else {
                data.synced.len()
            };
            let mut rebuilt = vec![0u8; final_len];
            let mut off = 0;
            while off < final_len {
                let end = (off + SECTOR_SIZE).min(final_len);
                let src = if rng.next_bool() {
                    &data.current
                } else {
                    &data.synced
                };
                for (i, byte) in rebuilt[off..end].iter_mut().enumerate() {
                    *byte = src.get(off + i).copied().unwrap_or(0);
                }
                off = end;
            }
            data.synced.clone_from(&rebuilt);
            data.current = rebuilt;
            data.locked_by = None;
        }
        *lock(&self.state.crash_at) = None;
        self.state.crashed.store(false, Ordering::SeqCst);
    }

    /// Returns the current (cache-visible) content of `path`, for assertions.
    pub fn snapshot(&self, path: &Path) -> Option<Vec<u8>> {
        lock(&self.state.files).get(path).map(|d| d.current.clone())
    }

    /// Called at the start of every mutating operation: fails if already
    /// crashed, advances the op counter, and fires an armed crash. Returns
    /// the torn seed when a [`CrashMode::Torn`] crash fires on an operation
    /// that supports tearing (`write_at`); on any other operation `Torn`
    /// degrades to [`CrashMode::Before`].
    fn enter_mut_op(&self, supports_torn: bool) -> io::Result<Option<u64>> {
        if self.crashed() {
            return Err(crash_error());
        }
        let op = self.state.ops.fetch_add(1, Ordering::SeqCst);
        let armed = *lock(&self.state.crash_at);
        if let Some((at_op, mode, torn_seed)) = armed
            && op == at_op
        {
            self.state.crashed.store(true, Ordering::SeqCst);
            return match mode {
                CrashMode::Torn if supports_torn => Ok(Some(torn_seed)),
                CrashMode::Before | CrashMode::Torn => Err(crash_error()),
            };
        }
        Ok(None)
    }

    fn check_alive(&self) -> io::Result<()> {
        if self.crashed() {
            Err(crash_error())
        } else {
            Ok(())
        }
    }
}

impl Vfs for SimVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn VfsFile>> {
        self.check_alive()?;
        let mut files = lock(&self.state.files);
        let exists = files.contains_key(path);
        match mode {
            OpenMode::MustExist if !exists => {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such file"));
            }
            OpenMode::CreateNew if exists => {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "file exists"));
            }
            _ => {}
        }
        if !exists {
            files.insert(path.to_path_buf(), SimFileData::default());
        }
        Ok(Box::new(SimFile {
            vfs: self.clone(),
            path: path.to_path_buf(),
            handle_id: self.state.next_handle_id.fetch_add(1, Ordering::SeqCst),
        }))
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        self.check_alive()?;
        match lock(&self.state.files).remove(path) {
            Some(_) => Ok(()),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        lock(&self.state.files).contains_key(path)
    }
}

struct SimFile {
    vfs: SimVfs,
    path: PathBuf,
    handle_id: u64,
}

impl SimFile {
    fn with_data<R>(&self, f: impl FnOnce(&mut SimFileData) -> io::Result<R>) -> io::Result<R> {
        let mut files = lock(&self.vfs.state.files);
        let data = files
            .get_mut(&self.path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file was deleted"))?;
        f(data)
    }
}

impl VfsFile for SimFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.vfs.check_alive()?;
        self.with_data(|data| {
            let start = usize::try_from(offset)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
            let end = start
                .checked_add(buf.len())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
            let src = data.current.get(start..end).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of file")
            })?;
            buf.copy_from_slice(src);
            Ok(())
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let torn = self.vfs.enter_mut_op(true)?;
        self.with_data(|data| {
            let start = usize::try_from(offset)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
            let end = start.saturating_add(buf.len());
            if data.current.len() < end {
                data.current.resize(end, 0);
            }
            match torn {
                None => {
                    data.current[start..end].copy_from_slice(buf);
                    Ok(())
                }
                Some(seed) => {
                    // Torn write: persist a seeded random subset of sectors,
                    // then die. Sector boundaries are absolute file offsets.
                    let mut rng = SplitMix64(seed);
                    let mut pos = start;
                    while pos < end {
                        let sector_end = ((pos / SECTOR_SIZE) + 1) * SECTOR_SIZE;
                        let chunk_end = sector_end.min(end);
                        if rng.next_bool() {
                            data.current[pos..chunk_end]
                                .copy_from_slice(&buf[pos - start..chunk_end - start]);
                        }
                        pos = chunk_end;
                    }
                    Err(crash_error())
                }
            }
        })
    }

    fn sync(&self) -> io::Result<()> {
        self.vfs.enter_mut_op(false)?;
        if self.vfs.state.lying_sync.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.with_data(|data| {
            data.synced.clone_from(&data.current);
            Ok(())
        })
    }

    fn truncate(&self, len: u64) -> io::Result<()> {
        self.vfs.enter_mut_op(false)?;
        self.with_data(|data| {
            let len = usize::try_from(len)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length too large"))?;
            data.current.resize(len, 0);
            Ok(())
        })
    }

    fn len(&self) -> io::Result<u64> {
        self.vfs.check_alive()?;
        self.with_data(|data| Ok(data.current.len() as u64))
    }

    fn try_lock_exclusive(&self) -> io::Result<bool> {
        self.vfs.check_alive()?;
        self.with_data(|data| match data.locked_by {
            Some(owner) if owner != self.handle_id => Ok(false),
            _ => {
                data.locked_by = Some(self.handle_id);
                Ok(true)
            }
        })
    }
}

impl Drop for SimFile {
    fn drop(&mut self) {
        // Release the lock only if this handle still owns it (a power_fail
        // may already have cleared it and a new handle may hold a new lock).
        let mut files = lock(&self.vfs.state.files);
        if let Some(data) = files.get_mut(&self.path)
            && data.locked_by == Some(self.handle_id)
        {
            data.locked_by = None;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn synced_data_survives_power_fail_unsynced_may_not() {
        let vfs = SimVfs::new();
        let f = vfs.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(&[1u8; SECTOR_SIZE], 0).unwrap();
        f.sync().unwrap();
        f.write_at(&[2u8; SECTOR_SIZE], 0).unwrap(); // dirty, not synced
        drop(f);

        for seed in 0..32 {
            let vfs2 = vfs.clone();
            vfs2.power_fail(seed);
            let content = vfs2.snapshot(&p("a")).unwrap();
            // The sector is either fully old or fully new — never silent
            // garbage from nowhere.
            assert!(content == vec![1u8; SECTOR_SIZE] || content == vec![2u8; SECTOR_SIZE]);
        }
    }

    #[test]
    fn crash_before_makes_all_later_ops_fail() {
        let vfs = SimVfs::new();
        let f = vfs.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(b"x", 0).unwrap(); // op 0
        vfs.arm_crash(1, CrashMode::Before, 0);
        assert!(f.write_at(b"y", 1).is_err()); // op 1 → crash
        assert!(f.sync().is_err());
        let mut buf = [0u8; 1];
        assert!(f.read_at(&mut buf, 0).is_err());
        assert!(vfs.crashed());
        vfs.power_fail(7);
        assert!(!vfs.crashed());
    }

    #[test]
    fn torn_write_persists_only_some_sectors() {
        let vfs = SimVfs::new();
        let f = vfs.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(&vec![0xAAu8; 4 * SECTOR_SIZE], 0).unwrap();
        f.sync().unwrap();
        vfs.arm_crash(2, CrashMode::Torn, 42);
        assert!(f.write_at(&vec![0xBBu8; 4 * SECTOR_SIZE], 0).is_err());
        drop(f);
        vfs.power_fail(43);
        let content = vfs.snapshot(&p("a")).unwrap();
        assert_eq!(content.len(), 4 * SECTOR_SIZE);
        for sector in content.chunks(SECTOR_SIZE) {
            assert!(
                sector.iter().all(|&b| b == 0xAA) || sector.iter().all(|&b| b == 0xBB),
                "sector mixes old and new bytes"
            );
        }
    }

    #[test]
    fn lying_sync_keeps_nothing_durable() {
        let vfs = SimVfs::new();
        vfs.set_lying_sync(true);
        let f = vfs.open(&p("a"), OpenMode::CreateNew).unwrap();
        f.write_at(&[9u8; SECTOR_SIZE], 0).unwrap();
        f.sync().unwrap();
        drop(f);
        // Seed chosen so the "old" (synced) image wins: content must be gone.
        for seed in 0..64 {
            let vfs2 = SimVfs::new();
            vfs2.set_lying_sync(true);
            let f = vfs2.open(&p("a"), OpenMode::CreateNew).unwrap();
            f.write_at(&[9u8; SECTOR_SIZE], 0).unwrap();
            f.sync().unwrap();
            drop(f);
            vfs2.power_fail(seed);
            let content = vfs2.snapshot(&p("a")).unwrap();
            if content.is_empty() {
                return; // observed data loss despite sync → the lie works
            }
        }
        panic!("lying sync never lost data across 64 seeds");
    }

    #[test]
    fn lock_ownership_survives_handle_churn() {
        let vfs = SimVfs::new();
        let f1 = vfs.open(&p("a"), OpenMode::CreateNew).unwrap();
        assert!(f1.try_lock_exclusive().unwrap());
        let f2 = vfs.open(&p("a"), OpenMode::MustExist).unwrap();
        assert!(!f2.try_lock_exclusive().unwrap());
        drop(f2); // must NOT release f1's lock
        let f3 = vfs.open(&p("a"), OpenMode::MustExist).unwrap();
        assert!(!f3.try_lock_exclusive().unwrap());
        drop(f1);
        assert!(f3.try_lock_exclusive().unwrap());
    }
}
