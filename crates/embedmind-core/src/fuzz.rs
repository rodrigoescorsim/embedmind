//! Bodies of the cargo-fuzz targets (`docs/TESTING.md` §3).
//!
//! The `fuzz/` crate's `fuzz_target!`s are one-line wrappers around these
//! functions, so the exact code libFuzzer exercises on Linux CI also
//! compiles and smoke-tests on stable, on every platform (including the
//! founder's Windows machine, where libFuzzer is unavailable).
//!
//! Contract of every entry point: for **arbitrary** input bytes it must
//! return — no panic, no UB, no unbounded allocation, no infinite loop.
//! Failures found here are corpus-committed, fixed, and changelogged
//! (brutal-honesty policy).

use std::path::Path;
use std::sync::Arc;

use crate::api::{Store, StoreOptions};
use crate::format::{Header, WalFrameHeader, WalHeader};
use crate::record::MemoryRecord;
use crate::storage::btree;
use crate::storage::sim::SimVfs;
use crate::storage::vfs::{OpenMode, Vfs};
use crate::storage::{Pager, PagerOptions};

/// `fuzz_header`: header and WAL-framing parsers over raw bytes.
pub fn fuzz_header(data: &[u8]) {
    let _ = Header::decode(data);
    let _ = Header::peek_page_size(data);
    let _ = WalHeader::decode(data);
    let _ = WalFrameHeader::decode(data, data, 0x5EED);
}

/// `fuzz_record`: `MemoryRecord` deserialization, including tagged scalars
/// and hostile length prefixes.
pub fn fuzz_record(data: &[u8]) {
    let _ = MemoryRecord::decode(data);
}

/// `fuzz_page`: B-tree leaf/inner/overflow page parsers (slot directories,
/// cell bounds, key ordering).
pub fn fuzz_page(data: &[u8]) {
    btree::fuzz_decode_page(data);
}

/// `fuzz_fts_page`: full-text dictionary (meta/inner/leaf) and postings
/// parsers over raw bytes — the format that landed with B2 (`docs/adr/0011`).
/// Must return, never panic/OOM, on arbitrary input.
pub fn fuzz_fts_page(data: &[u8]) {
    crate::index::fts::fuzz_decode_page(data);
}

/// `fuzz_graph_page`: graph dictionary nodes (meta/inner/leaf), overflow
/// chains, and the entity-members/adjacency value bodies — the format that
/// landed with S13 (`docs/adr/0012`, `docs/FORMAT.md` §12). Must return,
/// never panic/OOM, on arbitrary input.
pub fn fuzz_graph_page(data: &[u8]) {
    crate::index::graph::fuzz_decode_page(data);
}

/// `fuzz_wal_replay`: full recovery with arbitrary bytes as the WAL sidecar
/// of a valid base store. Recovery must yield an openable store or a typed
/// error; every page read after it must be `Ok` or a typed error.
pub fn fuzz_wal_replay(data: &[u8]) {
    let vfs = SimVfs::new();
    let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
    let store_path = Path::new("m.mind");
    let opts = PagerOptions {
        page_size: 512, // small pages: more of `data` becomes whole frames
        ..Default::default()
    };
    let Ok(mut pager) = Pager::create(Arc::clone(&shared), store_path, opts) else {
        return;
    };
    if commit_two_pages(&mut pager).is_err() {
        return;
    }
    drop(pager); // leaves a live WAL behind; the lock is released

    // Replace the WAL bytes wholesale with the fuzz input.
    if let Ok(wal) = vfs.open(Path::new("m.mind-wal"), OpenMode::OpenOrCreate)
        && (wal.truncate(0).is_err() || wal.write_at(data, 0).is_err())
    {
        return;
    }

    if let Ok(pager) = Pager::open(shared, store_path, opts) {
        read_everything(&pager);
    }
}

/// `fuzz_open_full`: arbitrary bytes as a whole `.mind` file. `Store::open`
/// must return `Ok` or a typed error — never panic/UB/OOM — and everything
/// reachable from an `Ok` store must behave the same way.
pub fn fuzz_open_full(data: &[u8]) {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let path = Path::new("m.mind");
    if let Ok(file) = vfs.open(path, OpenMode::CreateNew)
        && file.write_at(data, 0).is_err()
    {
        return;
    }
    if let Ok(store) = Store::open_with(vfs, path, StoreOptions::default()) {
        for item in store.iter_all().take(10_000) {
            if item.is_err() {
                break; // typed error: fine; keep the bound tight either way
            }
        }
    }
}

fn commit_two_pages(pager: &mut Pager) -> crate::Result<()> {
    let mut txn = pager.begin()?;
    btree::insert(&mut txn, [1u8; 16], b"base record a")?;
    btree::insert(&mut txn, [2u8; 16], b"base record b")?;
    txn.commit()?;
    Ok(())
}

fn read_everything(pager: &Pager) {
    for page_no in 0..pager.page_count().min(1024) {
        let _ = pager.read_page(page_no); // Ok or typed error, never panic
    }
    let _ = btree::scan(pager, pager.header().root_btree_page)
        .take(10_000)
        .count();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::storage::sim::SplitMix64;

    /// Runs every fuzz body over seeded arbitrary inputs — the stable-only
    /// smoke pass. The real fuzzers run these same bodies coverage-guided.
    #[test]
    fn fuzz_bodies_never_panic_on_arbitrary_input() {
        let mut rng = SplitMix64(0xC0FFEE);
        for round in 0..300 {
            let len = (rng.next_u64() % 2048) as usize;
            let mut data = vec![0u8; len];
            for b in &mut data {
                *b = rng.next_u64() as u8;
            }
            fuzz_header(&data);
            fuzz_record(&data);
            fuzz_page(&data);
            fuzz_fts_page(&data);
            fuzz_graph_page(&data);
            // The whole-store bodies are heavier; sample them.
            if round % 10 == 0 {
                fuzz_wal_replay(&data);
                fuzz_open_full(&data);
            }
        }
    }

    /// Replays every seed file committed under `fuzz/corpus/<target>/` through
    /// its matching body (`docs/TESTING.md` §3: "the corpus... grows with
    /// every CI run's new coverage"). This is the part coverage-guided fuzzing
    /// alone doesn't give you on stable/Windows: a fixed regression corpus
    /// that every `cargo test` run replays, so a previously-minimized crash
    /// input can never silently start passing unnoticed.
    #[test]
    #[allow(clippy::type_complexity)]
    fn fuzz_corpus_seeds_never_panic() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus");
        let targets: &[(&str, fn(&[u8]))] = &[
            ("fuzz_header", fuzz_header),
            ("fuzz_record", fuzz_record),
            ("fuzz_page", fuzz_page),
            ("fuzz_fts_page", fuzz_fts_page),
            ("fuzz_graph_page", fuzz_graph_page),
            ("fuzz_wal_replay", fuzz_wal_replay),
            ("fuzz_open_full", fuzz_open_full),
        ];
        let mut total_files = 0usize;
        for (name, body) in targets {
            let dir = root.join(name);
            let entries = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("corpus dir {}: {e}", dir.display()));
            for entry in entries {
                let path = entry
                    .unwrap_or_else(|e| panic!("corpus dir {}: {e}", dir.display()))
                    .path();
                if !path.is_file() {
                    continue;
                }
                let data = std::fs::read(&path)
                    .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
                body(&data);
                total_files += 1;
            }
        }
        assert!(
            total_files >= targets.len(),
            "expected at least one seed per fuzz target"
        );
    }

    /// The whole-file body must also survive a *valid* file prefix followed
    /// by garbage — the shape real corruption takes.
    #[test]
    fn fuzz_open_full_survives_truncated_valid_files() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let path = Path::new("valid.mind");
        let mut store =
            Store::create_with(Arc::clone(&vfs), path, StoreOptions::default()).unwrap();
        store
            .remember(crate::api::MemoryDraft::new("seed memory"))
            .unwrap();
        store.close().unwrap();
        let file = vfs.open(path, OpenMode::MustExist).unwrap();
        let len = file.len().unwrap() as usize;
        let mut bytes = vec![0u8; len];
        file.read_at(&mut bytes, 0).unwrap();
        drop(file);

        let mut rng = SplitMix64(0xBADF00D);
        for _ in 0..100 {
            let mut mutated = bytes.clone();
            let cut = (rng.next_u64() as usize) % (len + 1);
            mutated.truncate(cut);
            if rng.next_bool() && !mutated.is_empty() {
                let i = (rng.next_u64() as usize) % mutated.len();
                mutated[i] ^= (rng.next_u64() as u8) | 1;
            }
            fuzz_open_full(&mutated);
        }
    }
}
