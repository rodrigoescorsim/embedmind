//! Pager, WAL, page cache and record B-tree — the durability layer.
//!
//! First code to land in M1 (item 1.1), together with the crash-test harness
//! (item 1.8): all file I/O goes through `trait Vfs` so the harness can inject
//! kill points and torn writes (`docs/TESTING.md` §2). Durability protocol:
//! `docs/FORMAT.md` §8; decision record: `docs/adr/0001`.
