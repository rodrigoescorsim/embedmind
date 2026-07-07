//! Pager, WAL, page cache and record B-tree — the durability layer.
//!
//! First code to land in M1 (item 1.1), together with the crash-test harness
//! (item 1.8): all file I/O goes through [`vfs::Vfs`] so the harness can
//! inject kill points and torn writes (`docs/TESTING.md` §2). Durability
//! protocol: `docs/FORMAT.md` §8; decision record: `docs/adr/0001`.
//!
//! - [`vfs`] — the I/O seam: [`vfs::RealVfs`] in production.
//! - [`sim`] — in-memory fault-injecting VFS for deterministic crash tests.
//! - [`wal`] — WAL sidecar: append, recovery scan, reset.
//! - [`pager`] — transactions, page reads with checksum verification,
//!   recovery on open, checkpointing, single-writer lock.
//! - [`btree`] — the record B-tree (M1 item 1.2): ULID keys, slotted leaves,
//!   overflow chains, in-order scan.
//!
//! The page cache (an optimization, not a correctness layer) lands later,
//! under [`pager::Pager`].

pub mod btree;
pub mod pager;
pub mod sim;
pub mod vfs;
pub mod wal;

pub use pager::{DEFAULT_CHECKPOINT_THRESHOLD, Pager, PagerOptions, Txn};
pub use vfs::{OpenMode, RealVfs, Vfs, VfsFile};
