//! Record-level crash harness: the `docs/TESTING.md` §2 injection sweep run
//! against the **public API** (`Store::remember`/`forget`/`iter`), so
//! invariant I5 is checked at the layer users see, over the real record
//! B-tree (splits, overflow chains, tombstones) — complementing `crash.rs`,
//! which sweeps the raw pager.
//!
//! The reference model tracks memories by their (unique) content, because
//! ULIDs are generated inside `remember` and are not observable when the
//! injected crash kills that very call. Same accounting as the pager
//! harness: one snapshot per attempted transaction; the recovered
//! `txn_counter` selects which snapshot the store must equal — a commit
//! whose call died may legitimately be present (complete on disk, never
//! acknowledged).
//!
//! ULID randomness makes the exact mutating-I/O count drift slightly between
//! runs (split timing), so an armed crash that never fires is treated as a
//! clean run and verified as such, not asserted against.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use embedmind_core::api::{MemoryDraft, Store, StoreOptions};
use embedmind_core::storage::sim::{CrashMode, SimVfs, SplitMix64};
use embedmind_core::{Memory, Result, Scalar, Ulid};

const STORE: &str = "memory.mind";

/// Small pages: leaf splits and overflow chains happen within a short
/// workload, so the sweep stays fast (it runs on every PR).
fn options() -> StoreOptions {
    StoreOptions {
        page_size: 512,
        checkpoint_threshold: 16 * 1024,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workload {
    /// Only remembers, sizes from empty to several overflow pages.
    RememberHeavy,
    /// Remembers, forgets and verified live reads interleaved.
    RememberForget,
    /// Remember → drop the store → reopen (recovery under injection).
    ReopenLoop,
}

impl Workload {
    fn seed(self) -> u64 {
        match self {
            Workload::RememberHeavy => 0xA1,
            Workload::RememberForget => 0xB2,
            Workload::ReopenLoop => 0xC3,
        }
    }
}

/// Expected state of one memory in the reference model, keyed by content.
type RefState = BTreeMap<String, /* tombstone */ bool>;

struct Model {
    /// `snapshots[t]` = expected state after transaction id `t`.
    snapshots: Vec<RefState>,
    /// Highest txn id whose call returned `Ok`.
    confirmed: u64,
    /// `(id, content)` of every confirmed live remember — for `forget`
    /// targets and `get` verification.
    ids: Vec<(Ulid, String)>,
}

impl Model {
    fn new() -> Self {
        Model {
            snapshots: vec![RefState::new()],
            confirmed: 0,
            ids: Vec::new(),
        }
    }

    fn attempted(&self) -> u64 {
        (self.snapshots.len() - 1) as u64
    }

    fn current(&self) -> &RefState {
        &self.snapshots[self.confirmed as usize]
    }
}

/// One durable `remember`, mirrored in the model (snapshot pushed at attempt
/// time, confirmed on `Ok` — see module docs).
fn do_remember(store: &mut Store, model: &mut Model, content: String) -> Result<()> {
    let mut next = model.current().clone();
    next.insert(content.clone(), false);
    model.snapshots.push(next);
    let memory = store.remember(
        MemoryDraft::new(content.clone())
            .project("harness")
            .agent("crash-test")
            .meta("len", Scalar::I64(content.len() as i64)),
    )?;
    assert_eq!(
        store.txn_counter(),
        model.attempted(),
        "txn ids must be sequential"
    );
    model.confirmed = model.attempted();
    model.ids.push((memory.id, content));
    Ok(())
}

/// One durable `forget` of a known-live id.
fn do_forget(store: &mut Store, model: &mut Model, idx: usize) -> Result<()> {
    let (id, content) = model.ids[idx].clone();
    if model.current().get(&content) != Some(&false) {
        return Ok(()); // already tombstoned in a previous round
    }
    let mut next = model.current().clone();
    next.insert(content, true);
    model.snapshots.push(next);
    let forgotten = store.forget(id)?;
    assert!(forgotten, "forget of a live id must report true");
    model.confirmed = model.attempted();
    Ok(())
}

/// Content sized to exercise inline cells, page-spanning records and
/// multi-page overflow chains at 512-byte pages. Unique per (workload, n).
fn content(workload: Workload, n: u64, rng: &mut SplitMix64) -> String {
    let size = match rng.next_u64() % 4 {
        0 => 0,
        1 => (rng.next_u64() % 60) as usize,        // inline
        2 => 200 + (rng.next_u64() % 200) as usize, // one overflow page
        _ => 900 + (rng.next_u64() % 600) as usize, // several overflow pages
    };
    format!("mem-{workload:?}-{n}-{}", "x".repeat(size))
}

fn run_workload(
    workload: Workload,
    vfs: &SimVfs,
    mut store: Store,
    model: &mut Model,
) -> Result<Store> {
    let mut rng = SplitMix64(workload.seed());
    let mut n = 0u64;
    match workload {
        Workload::RememberHeavy => {
            for _ in 0..10 {
                n += 1;
                do_remember(&mut store, model, content(workload, n, &mut rng))?;
            }
        }
        Workload::RememberForget => {
            for round in 0..14 {
                match rng.next_u64() % 4 {
                    0 if !model.ids.is_empty() => {
                        let idx = (rng.next_u64() as usize) % model.ids.len();
                        do_forget(&mut store, model, idx)?;
                    }
                    1 if !model.ids.is_empty() => {
                        // Verified live read (happy-path I5).
                        let idx = (rng.next_u64() as usize) % model.ids.len();
                        let (id, content) = model.ids[idx].clone();
                        let got = store.get(id)?;
                        let tombstoned = model.current()[&content];
                        assert_eq!(
                            got.is_none(),
                            tombstoned,
                            "live read diverged at round {round}"
                        );
                        if let Some(memory) = got {
                            assert_eq!(memory.content, content);
                            assert_eq!(memory.provenance.agent, "crash-test");
                        }
                    }
                    _ => {
                        n += 1;
                        do_remember(&mut store, model, content(workload, n, &mut rng))?;
                    }
                }
            }
        }
        Workload::ReopenLoop => {
            for _ in 0..5 {
                n += 1;
                do_remember(&mut store, model, content(workload, n, &mut rng))?;
                drop(store); // no clean close: leaves the WAL behind
                store = Store::open_with(Arc::new(vfs.clone()), Path::new(STORE), options())?;
            }
        }
    }
    Ok(store)
}

/// Reopens the recovered store and checks I1–I5 at the record level.
fn check_invariants(vfs: &SimVfs, model: &Model, ctx: &str) {
    let mut store = Store::open_with(Arc::new(vfs.clone()), Path::new(STORE), options())
        .unwrap_or_else(|e| panic!("I1 violated ({ctx}): recovery failed: {e}"));

    let t = store.txn_counter();
    assert!(
        t >= model.confirmed && t <= model.attempted(),
        "I2/I3 violated ({ctx}): recovered txn_counter {t}, confirmed {}, attempted {}",
        model.confirmed,
        model.attempted()
    );
    let expected = &model.snapshots[t as usize];

    // I4+I5: every surviving memory reads back exactly; nothing extra,
    // nothing missing, tombstones exact.
    let survivors: Vec<Memory> = store
        .iter_all()
        .collect::<Result<_>>()
        .unwrap_or_else(|e| panic!("I4 violated ({ctx}): {e}"));
    let got: RefState = survivors
        .iter()
        .map(|m| (m.content.clone(), m.tombstone))
        .collect();
    assert_eq!(got.len(), survivors.len(), "duplicate contents ({ctx})");
    assert_eq!(&got, expected, "I5 violated ({ctx}) at txn {t}");
    for memory in &survivors {
        assert_eq!(memory.provenance.agent, "crash-test", "({ctx})");
        assert_eq!(memory.project.as_deref(), Some("harness"), "({ctx})");
        assert_eq!(
            memory.metadata.get("len"),
            Some(&Scalar::I64(memory.content.len() as i64)),
            "({ctx})"
        );
    }
    // Confirmed ids still resolve by id (not just by scan).
    for (id, content) in &model.ids {
        if expected.get(content).is_some() {
            let got = store
                .get(*id)
                .unwrap_or_else(|e| panic!("get ({ctx}): {e}"));
            assert_eq!(
                got.is_none(),
                expected[content],
                "I5 violated ({ctx}): get({id}) vs tombstone state"
            );
        }
    }

    // The recovered store must be fully usable.
    let memory = store
        .remember(MemoryDraft::new("post-recovery write"))
        .unwrap_or_else(|e| panic!("post-recovery remember ({ctx}): {e}"));
    assert_eq!(
        store
            .get(memory.id)
            .expect("post-recovery get")
            .expect("post-recovery presence")
            .content,
        "post-recovery write",
        "({ctx})"
    );
}

fn sweep(workload: Workload) {
    // Dry run: measure the mutating-I/O range and validate the workload.
    let vfs = SimVfs::new();
    let mut model = Model::new();
    let store = Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
    let base_ops = vfs.op_count();
    let store = run_workload(workload, &vfs, store, &mut model).expect("dry run must succeed");
    let total_ops = vfs.op_count();
    assert_eq!(model.current(), &collect_state(&store), "dry-run state");
    store.close().expect("clean close");
    assert!(total_ops > base_ops, "workload performed no mutating I/O");

    for p in base_ops..total_ops {
        for mode in [CrashMode::Before, CrashMode::Torn] {
            let seed = p ^ (workload.seed() << 32) ^ (u64::from(mode == CrashMode::Torn) << 16);
            let ctx = format!("workload {workload:?}, P {p}, mode {mode:?}, seed {seed:#x}");

            let vfs = SimVfs::new();
            let mut model = Model::new();
            let store =
                Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
            vfs.arm_crash(p, mode, seed);
            let result = run_workload(workload, &vfs, store, &mut model);
            if let Ok(store) = result {
                // ULID randomness shifted the op count and the armed crash
                // never fired: verify the clean end state instead.
                assert_eq!(model.current(), &collect_state(&store), "({ctx})");
                continue;
            }
            drop(result); // releases handles of the crashed store

            vfs.power_fail(seed.rotate_left(17));
            check_invariants(&vfs, &model, &ctx);
        }
    }
}

fn collect_state(store: &Store) -> RefState {
    store
        .iter_all()
        .map(|m| m.map(|m| (m.content, m.tombstone)))
        .collect::<Result<_>>()
        .expect("iter_all on a healthy store")
}

#[test]
fn crash_sweep_remember_heavy() {
    sweep(Workload::RememberHeavy);
}

#[test]
fn crash_sweep_remember_forget() {
    sweep(Workload::RememberForget);
}

#[test]
fn crash_sweep_reopen_loop() {
    sweep(Workload::ReopenLoop);
}

/// `forget` of a missing/tombstoned id must write nothing at all (no txn,
/// no WAL growth) — checked here because it is an I3 guarantee in disguise.
#[test]
fn failed_forget_writes_nothing() {
    let vfs = SimVfs::new();
    let mut store = Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
    let memory = store.remember(MemoryDraft::new("only one")).unwrap();
    assert!(store.forget(memory.id).unwrap());
    let txns = store.txn_counter();
    let ops = vfs.op_count();

    assert!(!store.forget(memory.id).unwrap()); // already tombstoned
    assert!(!store.forget(Ulid::new()).unwrap()); // never existed
    assert_eq!(store.txn_counter(), txns);
    assert_eq!(vfs.op_count(), ops, "no mutating I/O for a no-op forget");
}
