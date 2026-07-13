//! Crash harness for the filter-meta sidecar (FTOPT-1, `docs/adr/0027`):
//! the injection sweep of `docs/TESTING.md` §2 over a workload that touches
//! every sidecar write path — `remember` (new entries + symbol interning),
//! `forget` (tombstone re-append) and supersede (superseded re-append) — so
//! the sidecar pages ride the same injected transactions as the records they
//! mirror.
//!
//! The invariant on top of I1–I5: after recovery the sidecar **agrees with
//! the records** — every record (tombstoned included) has an entry whose
//! liveness decision, scope symbols and `doc_len` match the record itself
//! (`Store::verify_filter_meta_invariant`). Because both are written in one
//! transaction, no crash point may leave a record without its entry or an
//! entry contradicting its record.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use embedmind_core::Result;
use embedmind_core::api::{MemoryDraft, Query, Store, StoreOptions};
use embedmind_core::storage::sim::{CrashMode, SimVfs};

const STORE: &str = "memory.mind";

/// Small pages so the sidecar chains grow real pages within a short
/// workload; no embedder — this sweep exercises pages and WAL, not vectors.
fn options() -> StoreOptions {
    StoreOptions {
        page_size: 512,
        checkpoint_threshold: 16 * 1024,
        ..StoreOptions::default()
    }
}

/// Deterministic workload across every sidecar write path. Ok(()) only if
/// every operation succeeded (an armed crash surfaces as an `Err`).
fn run_workload(store: &mut Store) -> Result<()> {
    let mut live = Vec::new();
    for i in 0..10u32 {
        let project = match i % 3 {
            0 => None,
            1 => Some("alpha"),
            _ => Some("beta"),
        };
        let mut draft = MemoryDraft::new(format!("crash sidecar memory {i}"))
            .agent(if i % 2 == 0 { "cli" } else { "mcp" });
        if let Some(p) = project {
            draft = draft.project(p);
        }
        let memory = store.remember(draft)?;
        live.push((memory.id, project));
        // Interleave the flag-rewriting paths so their sidecar re-appends
        // land at varied chain states.
        if i == 4 {
            store.forget(live[0].0)?;
        }
        if i == 7 {
            let (target, project) = live[2];
            let mut draft = MemoryDraft::new("crash sidecar superseding memory")
                .agent("cli")
                .supersede(target);
            if let Some(p) = project {
                draft = draft.project(p);
            }
            store.remember(draft)?;
        }
    }
    Ok(())
}

/// After recovery: the store must open, the sidecar must agree with the
/// records, and a search through it must run clean.
fn check_invariants(vfs: &SimVfs, ctx: &str) {
    let store = Store::open_with(Arc::new(vfs.clone()), Path::new(STORE), options())
        .unwrap_or_else(|e| panic!("recovered store must open ({ctx}): {e}"));
    store
        .verify_filter_meta_invariant()
        .unwrap_or_else(|e| panic!("sidecar must agree with the records ({ctx}): {e}"));
    store
        .search_text(Query::new("crash sidecar memory").limit(20))
        .unwrap_or_else(|e| panic!("search through the sidecar must run ({ctx}): {e}"));
}

#[test]
fn crash_sweep_filter_meta_stays_consistent_with_records() {
    // Dry run: measure the mutating-I/O range and validate the workload.
    let vfs = SimVfs::new();
    let mut store = Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
    let base_ops = vfs.op_count();
    run_workload(&mut store).expect("dry run must succeed");
    let total_ops = vfs.op_count();
    store.verify_filter_meta_invariant().unwrap();
    store.close().expect("clean close");
    assert!(total_ops > base_ops, "workload performed no mutating I/O");

    for p in base_ops..total_ops {
        for mode in [CrashMode::Before, CrashMode::Torn] {
            let seed = p ^ (u64::from(mode == CrashMode::Torn) << 16) ^ 0xF117E12;
            let ctx = format!("P {p}, mode {mode:?}, seed {seed:#x}");

            let vfs = SimVfs::new();
            let mut store =
                Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), options()).unwrap();
            vfs.arm_crash(p, mode, seed);
            let result = run_workload(&mut store);
            if result.is_ok() {
                // ULID randomness shifted the op count and the armed crash
                // never fired: the clean end state must still hold.
                store.verify_filter_meta_invariant().unwrap();
                continue;
            }
            drop(result);
            drop(store); // releases handles of the crashed store

            vfs.power_fail(seed.rotate_left(17));
            check_invariants(&vfs, &ctx);
        }
    }
}
