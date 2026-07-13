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

/// A store whose shared term appears in enough documents that its postings
/// body carries a real skip index (block_count > 0) under the version-5
/// layout — for a fuzz seed that exercises the skip-index parser branch
/// (S26 part 2, ADR 0022).
fn build_skip_store(page_size: u32) -> (SimVfs, &'static str) {
    let vfs = SimVfs::new();
    let path = Path::new("skip.mind");
    let mut store = Store::create_with(
        Arc::new(vfs.clone()),
        path,
        StoreOptions {
            page_size,
            ..Default::default()
        },
    )
    .unwrap();
    // ≥ 4 skip blocks of 128 = 512 shared postings; a margin puts it well over.
    for i in 0..700 {
        store
            .remember(MemoryDraft::new(format!(
                "shared corpus token unique{i:04}"
            )))
            .unwrap();
    }
    store.close().unwrap();
    (vfs, "skip.mind")
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
        superseded: true, // exercise the S19 flag bit in the corpus
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
        superseded: false,
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
        // FTS dictionary (0x08) and postings (0x09) pages seed their own
        // target (docs/adr/0011). The dictionary meta/inner/leaf all share
        // 0x08; one real instance of each on-disk type is enough to start.
        // Version-suffixed name (S26, ADR 0021/0022): this build emits the
        // delta+varint+skip postings layout (format_version 5), and the
        // committed unsuffixed `seed-type-08`/`seed-type-09` (fixed-width) and
        // `-v4` (skip-less delta+varint) seeds stay put — every layout keeps a
        // seed, so no decode branch loses corpus coverage.
        if (0x08..=0x09).contains(&page_type) && seen.insert(page_type) {
            write_seed(
                "fuzz_fts_page",
                &format!("seed-type-{page_type:02x}-v5"),
                page,
            );
        }
    }

    // A large-vocabulary store so one term's postings body actually carries a
    // skip index (block_count > 0): seeds the fuzzer with the version-5 skip
    // layout, whose skip-index offsets/bounds are their own parser branch
    // (S26 part 2, ADR 0022). A 512-byte page keeps such a body in an
    // FTS_POSTINGS (0x09) chain; we capture the first chained page.
    let (vfs, path) = build_skip_store(512);
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
    for page in file.chunks(512).skip(1) {
        if page[0] == 0x09 {
            write_seed("fuzz_fts_page", "seed-postings-skip-v5", page);
            break;
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
