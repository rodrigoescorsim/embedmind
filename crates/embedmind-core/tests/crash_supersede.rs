//! Crash harness for S19 `supersedes` (`docs/adr/0013`): the injection sweep
//! of `docs/TESTING.md` §2 over a workload where every other `remember`
//! supersedes an earlier memory — so the pages a supersede touches (the
//! target's rewritten record cell, the graph pages of the `"supersedes"`
//! edge at both ends, and the new record's insert) all ride the same
//! injected transactions.
//!
//! The invariant on top of I1–I5: a supersede is **atomic** — after recovery
//! the store matches exactly one reference snapshot, so "new memory present
//! but target not flagged" (or the reverse) is impossible. Same accounting
//! as `crash_records.rs`: one snapshot per attempted transaction, the
//! recovered `txn_counter` selects which snapshot the store must equal.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use embedmind_core::api::{MemoryDraft, Query, Store, StoreOptions};
use embedmind_core::storage::sim::{CrashMode, SimVfs, SplitMix64};
use embedmind_core::{Memory, Result, SUPERSEDES_RELATION, Ulid};

const STORE: &str = "memory.mind";

/// Small pages so record rewrites and graph growth span real page splits
/// within a short workload. No embedder — same reasoning as
/// `crash_records.rs`: this sweep exercises pages and WAL, not vectors.
fn options() -> StoreOptions {
    StoreOptions {
        page_size: 512,
        checkpoint_threshold: 16 * 1024,
        ..StoreOptions::default()
    }
}

/// Expected state: `content → superseded` (nothing is forgotten in this
/// workload, so presence in the map means the record exists and is live).
type RefState = BTreeMap<String, bool>;

struct Model {
    /// `snapshots[t]` = expected state after transaction id `t`.
    snapshots: Vec<RefState>,
    /// Highest txn id whose call returned `Ok`.
    confirmed: u64,
    /// `(id, content)` of every confirmed remember.
    ids: Vec<(Ulid, String)>,
    /// `content → content it superseded`, for every *attempted* remember.
    /// Immutable once written, so no per-snapshot tracking.
    superseded_by: BTreeMap<String, Option<String>>,
}

impl Model {
    fn new() -> Self {
        Model {
            snapshots: vec![RefState::new()],
            confirmed: 0,
            ids: Vec::new(),
            superseded_by: BTreeMap::new(),
        }
    }

    fn attempted(&self) -> u64 {
        (self.snapshots.len() - 1) as u64
    }

    fn current(&self) -> &RefState {
        &self.snapshots[self.confirmed as usize]
    }
}

/// One durable `remember`, superseding the most recent confirmed live
/// non-superseded memory when `supersede` is set (and one exists).
fn do_remember(store: &mut Store, model: &mut Model, content: String, supersede: bool) -> Result<()> {
    let target = if supersede {
        model
            .ids
            .iter()
            .rev()
            .find(|(_, c)| model.current().get(c) == Some(&false))
            .map(|(id, c)| (*id, c.clone()))
    } else {
        None
    };

    let mut next = model.current().clone();
    next.insert(content.clone(), false);
    if let Some((_, target_content)) = &target {
        next.insert(target_content.clone(), true);
    }
    model.snapshots.push(next);
    model.superseded_by.insert(
        content.clone(),
        target.as_ref().map(|(_, c)| c.clone()),
    );

    let mut draft = MemoryDraft::new(content.clone()).agent("crash-test");
    if let Some((target_id, _)) = target {
        draft = draft.supersede(target_id);
    }
    let memory = store.remember(draft)?;
    assert_eq!(
        store.txn_counter(),
        model.attempted(),
        "txn ids must be sequential"
    );
    model.confirmed = model.attempted();
    model.ids.push((memory.id, content));
    Ok(())
}

/// Content sized to exercise inline cells and overflow chains at 512-byte
/// pages, with a shared leading token ("fact") for the search check.
fn content(n: u64, rng: &mut SplitMix64) -> String {
    let size = match rng.next_u64() % 3 {
        0 => 0,
        1 => (rng.next_u64() % 60) as usize,        // inline
        _ => 300 + (rng.next_u64() % 400) as usize, // overflow chain
    };
    format!("fact {n} {}", "pad ".repeat(size / 4))
}

fn run_workload(vfs: &SimVfs, mut store: Store, model: &mut Model) -> Result<Store> {
    let _ = vfs;
    let mut rng = SplitMix64(0xD4);
    for n in 1..=10 {
        // Every other remember supersedes; chains form naturally because the
        // target is always the newest live non-superseded memory.
        do_remember(&mut store, model, content(n, &mut rng), n % 2 == 0)?;
    }
    Ok(store)
}

/// Reopens the recovered store and checks the S19 invariants at the level
/// users see.
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

    // Atomicity: the store equals exactly the snapshot at txn t — a new
    // memory without its target flagged (or a flag without the new memory)
    // cannot match any snapshot and fails here.
    let survivors: Vec<Memory> = store
        .iter_all()
        .collect::<Result<_>>()
        .unwrap_or_else(|e| panic!("I4 violated ({ctx}): {e}"));
    let got: RefState = survivors
        .iter()
        .map(|m| (m.content.clone(), m.superseded))
        .collect();
    assert_eq!(got.len(), survivors.len(), "duplicate contents ({ctx})");
    assert_eq!(&got, expected, "supersede atomicity violated ({ctx}) at txn {t}");

    // Superseded memories stay readable by id (history, not deletion).
    for (id, content) in &model.ids {
        if expected.contains_key(content) {
            let memory = store
                .get(*id)
                .unwrap_or_else(|e| panic!("get ({ctx}): {e}"))
                .unwrap_or_else(|| panic!("({ctx}): get({id}) lost a live memory"));
            assert_eq!(
                memory.superseded, expected[content],
                "({ctx}): superseded flag of {content:?}"
            );
        }
    }

    // Search excludes exactly the superseded (the exclusion is re-read from
    // the record, so it must hold on the recovered file too). Keyword search
    // needs no embedder; every content shares the "fact" token.
    let hits = store
        .search_text(Query::new("fact").limit(usize::MAX))
        .unwrap_or_else(|e| panic!("search_text ({ctx}): {e}"));
    let got_hits: std::collections::BTreeSet<String> =
        hits.into_iter().map(|h| h.memory.content).collect();
    let want_hits: std::collections::BTreeSet<String> = expected
        .iter()
        .filter(|&(_, &superseded)| !superseded)
        .map(|(c, _)| c.clone())
        .collect();
    assert_eq!(
        got_hits, want_hits,
        "({ctx}): search must return exactly the non-superseded memories"
    );

    // The "supersedes" edge survived at both ends for every surviving pair.
    let by_content: BTreeMap<&String, &Memory> =
        survivors.iter().map(|m| (&m.content, m)).collect();
    for m in &survivors {
        let target = model.superseded_by[&m.content]
            .as_ref()
            .and_then(|tc| by_content.get(tc));
        let related = store
            .related(m.id)
            .unwrap_or_else(|e| panic!("related ({ctx}): {e}"));
        let outgoing: Vec<_> = related.iter().filter(|r| r.outgoing).collect();
        match target {
            Some(t) => {
                assert_eq!(outgoing.len(), 1, "({ctx}): outgoing of {:?}", m.content);
                assert_eq!(outgoing[0].memory.id, t.id, "({ctx})");
                assert_eq!(outgoing[0].kind, SUPERSEDES_RELATION, "({ctx})");
            }
            None => assert!(
                outgoing.is_empty(),
                "({ctx}): unexpected outgoing edge from {:?}",
                m.content
            ),
        }
    }

    // The recovered store must accept a fresh supersede.
    if let Some((id, content)) = model
        .ids
        .iter()
        .find(|(_, c)| expected.get(c) == Some(&false))
    {
        let memory = store
            .remember(MemoryDraft::new("post-recovery version").supersede(*id))
            .unwrap_or_else(|e| panic!("post-recovery supersede ({ctx}): {e}"));
        assert!(
            store
                .get(*id)
                .expect("post-recovery get")
                .expect("post-recovery presence")
                .superseded,
            "({ctx}): post-recovery supersede of {content:?} must flag it"
        );
        assert!(store.get(memory.id).expect("get new").is_some(), "({ctx})");
    }
}

#[test]
fn crash_sweep_supersede() {
    // Dry run: measure the mutating-I/O range and validate the workload.
    let vfs = SimVfs::new();
    let mut model = Model::new();
    let store = Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
    let base_ops = vfs.op_count();
    let store = run_workload(&vfs, store, &mut model).expect("dry run must succeed");
    let total_ops = vfs.op_count();
    let dry_state: RefState = store
        .iter_all()
        .map(|m| m.map(|m| (m.content, m.superseded)))
        .collect::<Result<_>>()
        .expect("iter_all on a healthy store");
    assert_eq!(model.current(), &dry_state, "dry-run state");
    assert!(
        dry_state.values().any(|&s| s),
        "workload must actually supersede something"
    );
    store.close().expect("clean close");
    assert!(total_ops > base_ops, "workload performed no mutating I/O");

    for p in base_ops..total_ops {
        for mode in [CrashMode::Before, CrashMode::Torn] {
            let seed = p ^ (0xD4 << 32) ^ (u64::from(mode == CrashMode::Torn) << 16);
            let ctx = format!("P {p}, mode {mode:?}, seed {seed:#x}");

            let vfs = SimVfs::new();
            let mut model = Model::new();
            let store =
                Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
            vfs.arm_crash(p, mode, seed);
            let result = run_workload(&vfs, store, &mut model);
            if let Ok(store) = result {
                // ULID randomness shifted the op count and the armed crash
                // never fired: verify the clean end state instead.
                let state: RefState = store
                    .iter_all()
                    .map(|m| m.map(|m| (m.content, m.superseded)))
                    .collect::<Result<_>>()
                    .expect("iter_all on a clean run");
                assert_eq!(model.current(), &state, "({ctx})");
                continue;
            }
            drop(result); // releases handles of the crashed store

            vfs.power_fail(seed.rotate_left(17));
            check_invariants(&vfs, &model, &ctx);
        }
    }
}
