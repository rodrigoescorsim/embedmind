//! Regenerates the seed corpus for the fuzz targets (`docs/TESTING.md` §3):
//! valid, harness-shaped inputs so the fuzzers start from deep program
//! states instead of rediscovering the magic bytes.
//!
//! Run from the repo root (rewrites `fuzz/corpus/<target>/seed-*`):
//!
//! ```text
//! cargo run -p embedmind-core --example gen_fuzz_corpus
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use embedmind_core::Ulid;
use embedmind_core::api::{MemoryDraft, Store, StoreOptions};
use embedmind_core::format::{DEFAULT_PAGE_SIZE, Header};
use embedmind_core::record::{MemoryRecord, Provenance, Scalar, VecRef};
use embedmind_core::storage::sim::SimVfs;

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus")
}

fn write_seed(target: &str, name: &str, bytes: &[u8]) {
    let dir = corpus_root().join(target);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), bytes).unwrap();
    println!("{target}/{name}: {} bytes", bytes.len());
}

/// A populated store on a SimVfs: some inline records, one overflow record,
/// one tombstone — every page type the v0.1 tree can produce.
fn build_store(page_size: u32) -> (SimVfs, &'static str) {
    let vfs = SimVfs::new();
    let path = Path::new("seed.mind");
    let mut store = Store::create_with(
        Arc::new(vfs.clone()),
        path,
        StoreOptions {
            page_size,
            ..Default::default()
        },
    )
    .unwrap();
    for i in 0..40 {
        store
            .remember(
                MemoryDraft::new(format!("seed memory {i} — conteúdo de exemplo"))
                    .project("embedmind")
                    .agent("gen-corpus")
                    .meta("i", Scalar::I64(i)),
            )
            .unwrap();
    }
    let big = store
        .remember(MemoryDraft::new("big ".repeat(600)))
        .unwrap();
    let victim = store.remember(MemoryDraft::new("to forget")).unwrap();
    store.forget(victim.id).unwrap();
    drop(big);
    drop(store); // no clean close: keeps the WAL sidecar alive for seeds
    (vfs, "seed.mind")
}

fn main() {
    // fuzz_header: a real header page + a fresh minimal one.
    let mut page0 = vec![0u8; DEFAULT_PAGE_SIZE as usize];
    Header::new(DEFAULT_PAGE_SIZE)
        .unwrap()
        .encode(&mut page0)
        .unwrap();
    write_seed("fuzz_header", "seed-fresh-header", &page0);

    // fuzz_record: a fully-populated record and a minimal one.
    let full = MemoryRecord {
        id: Ulid::from_parts(1_751_900_000_000, 42),
        tombstone: false,
        content: "memória de exemplo com acentuação".to_owned(),
        vec_ref: Some(VecRef {
            page_no: 7,
            slot: 2,
        }),
        project: Some("embedmind".to_owned()),
        provenance: Provenance {
            agent: "claude-code".to_owned(),
            session_id: Some("sess-1".to_owned()),
            created_at_micros: 1_751_900_000_000_000,
        },
        metadata: [
            ("k".to_owned(), Scalar::Str("v".to_owned())),
            ("n".to_owned(), Scalar::I64(-1)),
            ("f".to_owned(), Scalar::F64(0.25)),
            ("b".to_owned(), Scalar::Bool(true)),
            ("z".to_owned(), Scalar::Null),
        ]
        .into(),
    };
    write_seed("fuzz_record", "seed-full-record", &full.encode().unwrap());
    let minimal = MemoryRecord {
        id: Ulid::from_parts(0, 0),
        tombstone: true,
        content: String::new(),
        vec_ref: None,
        project: None,
        provenance: Provenance {
            agent: String::new(),
            session_id: None,
            created_at_micros: 0,
        },
        metadata: Default::default(),
    };
    write_seed(
        "fuzz_record",
        "seed-minimal-record",
        &minimal.encode().unwrap(),
    );

    // Small-page store: real leaf/inner/overflow pages + a live WAL.
    let (vfs, path) = build_store(512);
    let wal = vfs.snapshot(Path::new("seed.mind-wal")).unwrap();
    write_seed("fuzz_wal_replay", "seed-live-wal", &wal);
    write_seed(
        "fuzz_header",
        "seed-live-wal-prefix",
        &wal[..wal.len().min(640)],
    );
    // Reopen + close: recovery checkpoints the WAL into the main file, so
    // the page slices below contain the real tree.
    Store::open_with(
        Arc::new(vfs.clone()),
        Path::new(path),
        StoreOptions {
            page_size: 512,
            ..Default::default()
        },
    )
    .unwrap()
    .close()
    .unwrap();
    let file = vfs.snapshot(Path::new(path)).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for page in file.chunks(512).skip(1) {
        let page_type = page[0];
        if (1..=7).contains(&page_type) && seen.insert(page_type) {
            write_seed("fuzz_page", &format!("seed-type-{page_type:02x}"), page);
        }
    }

    // fuzz_open_full: a cleanly closed single-file store (default page size).
    let vfs = SimVfs::new();
    let mind = Path::new("full.mind");
    let mut store =
        Store::create_with(Arc::new(vfs.clone()), mind, StoreOptions::default()).unwrap();
    for i in 0..10 {
        store
            .remember(MemoryDraft::new(format!("full store memory {i}")))
            .unwrap();
    }
    store.close().unwrap();
    write_seed(
        "fuzz_open_full",
        "seed-clean-store",
        &vfs.snapshot(mind).unwrap(),
    );
}
