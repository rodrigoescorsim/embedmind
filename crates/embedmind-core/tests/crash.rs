//! Deterministic crash-test harness (`docs/TESTING.md` §2) — M1 item 1.8,
//! born together with the storage layer it tests.
//!
//! For every workload, a dry run counts the mutating I/O operations; the
//! sweep then re-runs the workload once per injection point `P` and crash
//! mode (fail-before / torn write), simulates power loss, reopens the store
//! (recovery runs) and checks the invariants:
//!
//! - I1: the store opens — recovery never fails on harness-generated files.
//! - I2: every transaction the workload saw confirmed is fully present.
//! - I3: no effect of an unconfirmed transaction is visible.
//! - I4: every page checksum validates (reads verify; the WAL prefix was
//!   verified by recovery itself).
//! - I5: page contents equal the in-memory reference model fed only the
//!   surviving operations.
//!
//! Any violation panics with the reproducing `(workload, P, mode, seed)`
//! tuple. A commit whose call failed mid-crash may legitimately be present
//! (it was complete on disk, just never acknowledged) — the model therefore
//! accepts the confirmed state *or* the confirmed state plus the pending
//! commit, decided by the recovered `txn_counter`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use embedmind_core::Error;
use embedmind_core::format::{DEFAULT_PAGE_SIZE, stamp_page_checksum};
use embedmind_core::storage::sim::{CrashMode, SimVfs, SplitMix64};
use embedmind_core::storage::{Pager, PagerOptions};

const PS: usize = DEFAULT_PAGE_SIZE as usize;
const STORE: &str = "memory.mind";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workload {
    /// Many commits, each allocating and writing fresh pages.
    InsertHeavy,
    /// Commits, overwrites, rollbacks and verified reads interleaved.
    Mixed,
    /// Commit → drop the pager → reopen (recovery under injection).
    ReopenLoop,
    /// Tiny checkpoint threshold: every commit checkpoints.
    CheckpointHeavy,
}

impl Workload {
    fn options(self) -> PagerOptions {
        match self {
            // Small threshold so mid-run checkpoints happen organically:
            // roughly every few transactions at 4 KiB pages.
            Workload::Mixed => PagerOptions {
                checkpoint_threshold: 32 * 1024,
                ..Default::default()
            },
            Workload::CheckpointHeavy => PagerOptions {
                checkpoint_threshold: 1,
                ..Default::default()
            },
            _ => PagerOptions::default(),
        }
    }

    fn seed(self) -> u64 {
        match self {
            Workload::InsertHeavy => 0x11,
            Workload::Mixed => 0x22,
            Workload::ReopenLoop => 0x33,
            Workload::CheckpointHeavy => 0x44,
        }
    }
}

/// One page's reference content (checksum-stamped, exactly as the pager
/// stores it).
fn stamped(fill: u8, tag: u64) -> Vec<u8> {
    let mut page = vec![fill; PS];
    // Make images unique per (fill, tag) so torn mixes can never accidentally
    // equal a legitimate image of a *different* write.
    page[..8].copy_from_slice(&tag.to_le_bytes());
    stamp_page_checksum(&mut page);
    page
}

fn zeroed() -> Vec<u8> {
    let mut page = vec![0u8; PS];
    stamp_page_checksum(&mut page);
    page
}

/// Committed store state as the reference model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefState {
    pages: BTreeMap<u64, Vec<u8>>,
    page_count: u64,
}

impl RefState {
    fn initial() -> Self {
        RefState {
            pages: BTreeMap::new(),
            page_count: 1,
        }
    }
}

/// Reference model: `snapshots[t]` = state after transaction id `t`.
struct Model {
    snapshots: Vec<RefState>,
    /// Highest txn id whose commit returned `Ok` to the workload.
    confirmed: u64,
}

impl Model {
    fn new() -> Self {
        Model {
            snapshots: vec![RefState::initial()],
            confirmed: 0,
        }
    }

    /// Highest txn id whose commit was attempted (== confirmed unless the
    /// final commit call died mid-flight).
    fn attempted(&self) -> u64 {
        (self.snapshots.len() - 1) as u64
    }

    fn current(&self) -> &RefState {
        &self.snapshots[self.confirmed as usize]
    }
}

enum TxnOp {
    /// Allocate a page and fill it.
    Alloc(u8),
    /// Overwrite an existing page (index into the allocated ones).
    Overwrite(u64, u8),
}

/// Runs one transaction against pager + model. `commit = false` exercises
/// rollback-by-drop.
fn do_txn(
    pager: &mut Pager,
    model: &mut Model,
    ops: &[TxnOp],
    commit: bool,
    tag: &mut u64,
) -> Result<(), Error> {
    let mut txn = pager.begin()?;
    let mut next = model.current().clone();
    for op in ops {
        *tag += 1;
        match *op {
            TxnOp::Alloc(fill) => {
                let page_no = txn.allocate_page()?;
                let image = stamped(fill, *tag);
                txn.write_page(page_no, &image)?;
                next.pages.insert(page_no, image);
                next.page_count += 1;
            }
            TxnOp::Overwrite(nth, fill) => {
                let keys: Vec<u64> = next.pages.keys().copied().collect();
                if keys.is_empty() {
                    continue;
                }
                let page_no = keys[(nth as usize) % keys.len()];
                let image = stamped(fill, *tag);
                txn.write_page(page_no, &image)?;
                next.pages.insert(page_no, image);
            }
        }
    }
    if !commit {
        drop(txn); // rollback: the model does not change
        return Ok(());
    }
    // Record the attempt BEFORE committing: if the commit call dies but its
    // frames were complete on disk, recovery may legitimately surface it.
    model.snapshots.push(next);
    let id = txn.commit()?;
    assert_eq!(id, model.attempted(), "txn ids must be sequential");
    model.confirmed = id;
    Ok(())
}

/// Drives one workload to completion (or until the armed crash fires).
/// Deterministic for a given workload: identical op sequence on every run.
fn run_workload(
    workload: Workload,
    vfs: &SimVfs,
    mut pager: Pager,
    model: &mut Model,
) -> Result<Pager, Error> {
    let mut rng = SplitMix64(workload.seed());
    let mut tag = 0u64;
    match workload {
        Workload::InsertHeavy | Workload::CheckpointHeavy => {
            for _ in 0..12 {
                let n = 1 + (rng.next_u64() % 3) as usize;
                let ops: Vec<TxnOp> = (0..n).map(|_| TxnOp::Alloc(rng.next_u64() as u8)).collect();
                do_txn(&mut pager, model, &ops, true, &mut tag)?;
            }
        }
        Workload::Mixed => {
            for round in 0..18 {
                match rng.next_u64() % 4 {
                    0 => {
                        // Rollback: write pages, then drop the txn.
                        let ops = [TxnOp::Alloc(0xF0), TxnOp::Overwrite(rng.next_u64(), 0xF1)];
                        do_txn(&mut pager, model, &ops, false, &mut tag)?;
                    }
                    1 => {
                        // Verified read against the model (happy-path I5).
                        let state = model.current();
                        if let Some((&page_no, image)) = state.pages.iter().next() {
                            let got = pager.read_page(page_no)?;
                            assert_eq!(&got, image, "live read diverged at round {round}");
                        }
                    }
                    _ => {
                        let ops = [
                            TxnOp::Alloc(rng.next_u64() as u8),
                            TxnOp::Overwrite(rng.next_u64(), rng.next_u64() as u8),
                        ];
                        do_txn(&mut pager, model, &ops, true, &mut tag)?;
                    }
                }
            }
        }
        Workload::ReopenLoop => {
            for _ in 0..6 {
                let ops = [TxnOp::Alloc(rng.next_u64() as u8)];
                do_txn(&mut pager, model, &ops, true, &mut tag)?;
                drop(pager); // no clean close: leaves the WAL behind
                pager = Pager::open(Arc::new(vfs.clone()), Path::new(STORE), workload.options())?;
            }
        }
    }
    Ok(pager)
}

/// Reopens the recovered store and checks invariants I1–I5.
fn check_invariants(vfs: &SimVfs, workload: Workload, model: &Model, ctx: &str) {
    let mut pager = Pager::open(Arc::new(vfs.clone()), Path::new(STORE), workload.options())
        .unwrap_or_else(|e| panic!("I1 violated ({ctx}): recovery failed: {e}"));

    let t = pager.header().txn_counter;
    assert!(
        t >= model.confirmed && t <= model.attempted(),
        "I2/I3 violated ({ctx}): recovered txn_counter {t}, confirmed {}, attempted {}",
        model.confirmed,
        model.attempted()
    );
    let expected = &model.snapshots[t as usize];
    assert_eq!(
        pager.page_count(),
        expected.page_count,
        "I5 violated ({ctx}): page_count mismatch at txn {t}"
    );
    for page_no in 1..expected.page_count {
        let got = pager
            .read_page(page_no)
            .unwrap_or_else(|e| panic!("I4 violated ({ctx}): page {page_no}: {e}"));
        let want = expected.pages.get(&page_no).cloned().unwrap_or_else(zeroed);
        assert_eq!(
            got, want,
            "I5 violated ({ctx}): page {page_no} content at txn {t}"
        );
    }

    // The recovered store must be fully usable: one more commit + read-back.
    let mut txn = pager
        .begin()
        .unwrap_or_else(|e| panic!("post-recovery begin ({ctx}): {e}"));
    let page_no = txn.allocate_page().expect("post-recovery alloc");
    let image = stamped(0x5A, u64::MAX);
    txn.write_page(page_no, &image)
        .expect("post-recovery write");
    txn.commit()
        .unwrap_or_else(|e| panic!("post-recovery commit ({ctx}): {e}"));
    assert_eq!(
        pager.read_page(page_no).expect("post-recovery read"),
        image,
        "({ctx})"
    );
}

/// Full injection sweep for one workload: crash at every mutating I/O
/// operation, in both modes, then recover and verify.
fn sweep(workload: Workload) {
    // Dry run: measure the op range and validate the workload itself.
    let vfs = SimVfs::new();
    let mut model = Model::new();
    let pager = Pager::create(Arc::new(vfs.clone()), Path::new(STORE), workload.options()).unwrap();
    let base_ops = vfs.op_count();
    let pager = run_workload(workload, &vfs, pager, &mut model).expect("dry run must succeed");
    let total_ops = vfs.op_count();
    check_dry_run(pager, &model);
    assert!(total_ops > base_ops, "workload performed no mutating I/O");

    for p in base_ops..total_ops {
        for mode in [CrashMode::Before, CrashMode::Torn] {
            let seed = p ^ (workload.seed() << 32) ^ (u64::from(mode == CrashMode::Torn) << 16);
            let ctx = format!("workload {workload:?}, P {p}, mode {mode:?}, seed {seed:#x}");

            let vfs = SimVfs::new();
            let mut model = Model::new();
            let pager =
                Pager::create(Arc::new(vfs.clone()), Path::new(STORE), workload.options()).unwrap();
            vfs.arm_crash(p, mode, seed);
            let result = run_workload(workload, &vfs, pager, &mut model);
            assert!(result.is_err(), "armed crash never fired ({ctx})");
            drop(result); // drops the pager if any — releases handles

            vfs.power_fail(seed.rotate_left(17));
            check_invariants(&vfs, workload, &model, &ctx);
        }
    }
}

fn check_dry_run(pager: Pager, model: &Model) {
    let expected = model.current();
    assert_eq!(pager.page_count(), expected.page_count);
    for (&page_no, image) in &expected.pages {
        assert_eq!(&pager.read_page(page_no).unwrap(), image);
    }
    pager.close().expect("clean close");
}

#[test]
fn crash_sweep_insert_heavy() {
    sweep(Workload::InsertHeavy);
}

#[test]
fn crash_sweep_mixed() {
    sweep(Workload::Mixed);
}

#[test]
fn crash_sweep_reopen_loop() {
    sweep(Workload::ReopenLoop);
}

#[test]
fn crash_sweep_checkpoint_heavy() {
    sweep(Workload::CheckpointHeavy);
}

/// Lying-fsync mode (`docs/TESTING.md` §2): `sync` succeeds but nothing is
/// durable. On such hardware EmbedMind keeps its *integrity* guarantees —
/// opening and reading never panics and never returns silent garbage: every
/// page either reads back as a legitimately written image or fails with a
/// typed error — but durability of the last commits is expressly lost
/// (the same stance as SQLite).
#[test]
fn lying_fsync_preserves_integrity_not_durability() {
    // Dry run to size the op range.
    let vfs = SimVfs::new();
    let mut model = Model::new();
    let pager = Pager::create(
        Arc::new(vfs.clone()),
        Path::new(STORE),
        PagerOptions::default(),
    )
    .unwrap();
    let base_ops = vfs.op_count();
    run_workload(Workload::InsertHeavy, &vfs, pager, &mut model).expect("dry run");
    let total_ops = vfs.op_count();

    for p in base_ops..total_ops {
        let vfs = SimVfs::new();
        vfs.set_lying_sync(true);
        let mut model = Model::new();
        let pager = Pager::create(
            Arc::new(vfs.clone()),
            Path::new(STORE),
            PagerOptions::default(),
        )
        .unwrap();
        vfs.arm_crash(p, CrashMode::Before, p);
        let _ = run_workload(Workload::InsertHeavy, &vfs, pager, &mut model);
        vfs.power_fail(p.wrapping_mul(0x9E3779B97F4A7C15));

        // Every image ever handed to the pager (any txn, confirmed or not),
        // plus the zeroed allocation image, is a legitimate page state.
        let mut legit: BTreeMap<u64, Vec<Vec<u8>>> = BTreeMap::new();
        for snapshot in &model.snapshots {
            for (&page_no, image) in &snapshot.pages {
                let entry = legit.entry(page_no).or_default();
                if !entry.contains(image) {
                    entry.push(image.clone());
                }
            }
        }

        let Ok(pager) = Pager::open(
            Arc::new(vfs.clone()),
            Path::new(STORE),
            PagerOptions::default(),
        ) else {
            continue; // typed refusal (e.g. detected corruption) is allowed here
        };
        let page_count = pager.page_count();
        assert!(page_count <= model.snapshots[model.attempted() as usize].page_count);
        for page_no in 1..page_count {
            match pager.read_page(page_no) {
                Err(Error::CorruptPage { .. }) => {} // detected, never propagated
                Err(e) => panic!("unexpected error kind at P {p}, page {page_no}: {e}"),
                Ok(got) => {
                    let ok = got == zeroed()
                        || legit
                            .get(&page_no)
                            .is_some_and(|images| images.contains(&got));
                    assert!(ok, "silent garbage at P {p}, page {page_no}");
                }
            }
        }
    }
}
