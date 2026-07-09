//! Crash harness for `embedmind vacuum` (story S11, `docs/adr/0003`).
//!
//! `vacuum` rebuilds the store by copy and swaps the result in with a single
//! atomic rename. The safety promise is absolute: a crash at **any** point of
//! the vacuum leaves the *original* file fully intact — never a torn mix, never
//! a half-applied compaction. This sweep proves it by arming a kill point at
//! every mutating I/O the vacuum performs and, after each simulated power
//! failure, reopening the store and checking its live/tombstone state is one of
//! exactly two legal outcomes:
//!
//! - **pre-vacuum**: the crash fired before the final rename → the original
//!   survives with its tombstones still present, and
//! - **post-vacuum**: the crash fired at/after the rename → the compacted file
//!   is in place, tombstones gone.
//!
//! Anything else — a missing live memory, a resurrected forgotten one, a
//! corrupt file, an orphan temp/scratch left adopted — is a bug.
//!
//! No embedder here: the vacuum's copy + FTS rebuild + atomic swap is what we
//! are testing, and loading a real ONNX model per injection point would make
//! the sweep far too slow. Vector rebuild is covered by the `api` unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use embedmind_core::api::{MemoryDraft, Query, Store, StoreOptions};
use embedmind_core::storage::Vfs;
use embedmind_core::storage::sim::{CrashMode, SimVfs};
use embedmind_core::{Memory, Result};

const STORE: &str = "memory.mind";

/// Small pages so a modest workload spans several data + overflow pages; the
/// vacuum then has real pages to reclaim and real structure to rebuild.
fn options() -> StoreOptions {
    StoreOptions {
        page_size: 512,
        checkpoint_threshold: 16 * 1024,
        embedder: None,
    }
}

/// Content sized to touch inline cells and multi-page overflow chains, so the
/// rebuilt file exercises the same code paths the original did.
fn content(n: usize) -> String {
    let size = match n % 4 {
        0 => 10,
        1 => 250,
        2 => 1200,
        _ => 40,
    };
    format!("mem-{n}-{}", "x".repeat(size))
}

/// Live/tombstone state keyed by content — the reference model. Vacuum drops
/// tombstones entirely, so a memory is either present-and-live or absent.
type State = BTreeMap<String, /* live */ bool>;

/// Populates a fresh store: `total` memories, every third one forgotten.
/// Returns the pre-vacuum state (with tombstones) and the post-vacuum state
/// (tombstones dropped) — the two legal outcomes of a crashed vacuum.
fn populate(store: &mut Store, total: usize) -> (State, State) {
    let mut ids = Vec::new();
    for n in 0..total {
        let c = content(n);
        let m = store
            .remember(MemoryDraft::new(c.clone()).agent("crash-test"))
            .unwrap();
        ids.push((m.id, c));
    }
    let mut pre = State::new();
    let mut post = State::new();
    for (i, (id, c)) in ids.iter().enumerate() {
        if i % 3 == 0 {
            store.forget(*id).unwrap();
            pre.insert(c.clone(), false); // tombstoned, still on disk pre-vacuum
        } else {
            pre.insert(c.clone(), true);
            post.insert(c.clone(), true); // survives the vacuum
        }
    }
    (pre, post)
}

/// Populates a store, then cleanly closes and reopens it so the populate
/// phase's WAL is fully checkpointed into the main file before any crash is
/// armed. The vacuum sweep is about the *copy + atomic swap*; settling first
/// makes the vacuum's own leading checkpoint a genuine no-op, so the sweep
/// isolates vacuum's I/O rather than re-testing the general checkpoint path.
fn populate_and_settle(vfs: &SimVfs) -> (Store, State, State) {
    let mut store = Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
    let (pre, post) = populate(&mut store, 18);
    store.close().unwrap();
    let store = Store::open_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
    (store, pre, post)
}

/// Reads the live/tombstone state of a store back through the public API.
/// `iter_all` yields tombstones too, so we can tell "tombstone still present"
/// (pre-vacuum) from "gone entirely" (post-vacuum).
fn observe(store: &Store) -> State {
    store
        .iter_all()
        .map(|m| m.map(|m: Memory| (m.content, !m.tombstone)))
        .collect::<Result<_>>()
        .expect("iter_all on a healthy store")
}

/// Which of the two legal outcomes a crashed vacuum recovered into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Original intact (crash before the swap): tombstones still present.
    PreVacuum,
    /// Compacted file in place (crash at/after the atomic rename).
    PostVacuum,
}

/// After a crash + power failure, the reopened store must equal exactly one of
/// the two legal outcomes, be fully readable, and carry no adopted orphan.
fn check_after_crash(vfs: &SimVfs, pre: &State, post: &State, ctx: &str) -> Outcome {
    let main_len = vfs.snapshot(Path::new(STORE)).map(|v| v.len());
    let wal_present = vfs.exists(Path::new("memory.mind-wal"));
    let mut store = Store::open_with(Arc::new(vfs.clone()), Path::new(STORE), options())
        .unwrap_or_else(|e| panic!("recovery failed ({ctx}): {e}"));

    let got = store
        .iter_all()
        .map(|m| m.map(|m: Memory| (m.content, !m.tombstone)))
        .collect::<Result<State>>()
        .unwrap_or_else(|e| {
            panic!(
                "iter_all after crash ({ctx}): {e}; main_len={main_len:?} wal_present={wal_present}"
            )
        });
    let outcome = if &got == pre {
        Outcome::PreVacuum
    } else if &got == post {
        Outcome::PostVacuum
    } else {
        panic!(
            "vacuum crash left an illegal state ({ctx}):\n got:  {got:?}\n pre:  {pre:?}\n post: {post:?}"
        );
    };

    // Whichever outcome, the full-text index must be consistent with the
    // records: every hit for any token must be a *live* memory — never a
    // dangling posting to a tombstoned or vacuumed-away record (I3).
    let live: std::collections::BTreeSet<&String> = got
        .iter()
        .filter(|&(_, &live)| live)
        .map(|(c, _)| c)
        .collect();
    assert!(!live.is_empty(), "workload had live memories ({ctx})");
    for c in got.keys() {
        let token = c.split('-').nth(1).unwrap_or("mem");
        let hits = store
            .search_text(Query::new(token))
            .unwrap_or_else(|e| panic!("fts search after crash ({ctx}): {e}"));
        for hit in &hits {
            assert!(
                live.contains(&hit.content),
                "fts returned a non-live memory {:?} ({ctx})",
                hit.content
            );
        }
    }

    // Orphan temp/scratch files must have been swept, never adopted.
    assert!(
        !vfs.exists(Path::new("memory.mind-vacuum-tmp")),
        "temp orphan not swept ({ctx})"
    );
    assert!(
        !vfs.exists(Path::new("memory.mind-vacuum-scratch")),
        "scratch orphan not swept ({ctx})"
    );

    // The recovered store must be fully usable: a fresh remember round-trips.
    let m = store
        .remember(MemoryDraft::new("post-crash write"))
        .unwrap_or_else(|e| panic!("post-crash remember ({ctx}): {e}"));
    assert_eq!(
        store.get(m.id).unwrap().unwrap().content,
        "post-crash write",
        "({ctx})"
    );
    outcome
}

#[test]
fn crash_sweep_vacuum() {
    // Dry run: build a store, measure exactly which mutating ops the *vacuum*
    // performs, so we sweep only those (not the populate phase).
    let vfs = SimVfs::new();
    let (mut store, _, dry_post) = populate_and_settle(&vfs);
    let base = vfs.op_count();
    store.vacuum().expect("dry-run vacuum must succeed");
    let end = vfs.op_count();
    // Post-vacuum state is what a clean run yields.
    assert_eq!(observe(&store), dry_post, "dry-run post-vacuum state");
    store.close().unwrap();
    assert!(end > base, "vacuum performed no mutating I/O");

    // Sweep a kill point at every mutating op the vacuum makes.
    let mut fired = 0u32;
    let mut saw_pre = false;
    let mut saw_post = false;
    for p in base..end {
        for mode in [CrashMode::Before, CrashMode::Torn] {
            let seed = p ^ ((mode == CrashMode::Torn) as u64) << 40;
            let ctx = format!("P {p}, mode {mode:?}, seed {seed:#x}");

            let vfs = SimVfs::new();
            let (mut store, pre, post) = populate_and_settle(&vfs);
            assert_eq!(pre.len(), 18);

            vfs.arm_crash(p, mode, seed);
            let result = store.vacuum();
            if result.is_ok() {
                // ULIDs shifted the op count and the armed crash never fired:
                // this is a clean vacuum — verify the compacted end state.
                assert_eq!(observe(&store), post, "clean vacuum ({ctx})");
                drop(store);
                continue;
            }
            drop(store); // release the crashed store's handles

            vfs.power_fail(seed.rotate_left(17));
            fired += 1;
            match check_after_crash(&vfs, &pre, &post, &ctx) {
                Outcome::PreVacuum => saw_pre = true,
                Outcome::PostVacuum => saw_post = true,
            }
        }
    }

    // The sweep must actually exercise crashes, and both legal outcomes: an
    // early crash that leaves the original intact (pre-vacuum) and a late one
    // that lands after the atomic swap (post-vacuum). If we never saw one of
    // them the sweep is not covering the transition it claims to.
    assert!(
        fired > 0,
        "no armed crash ever fired — sweep covered nothing"
    );
    assert!(
        saw_pre,
        "no crash preserved the original — pre-swap window untested"
    );
    assert!(
        saw_post,
        "no crash landed after the swap — post-swap window untested"
    );
}
