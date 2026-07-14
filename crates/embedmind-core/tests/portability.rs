//! Cross-platform portability of the `.mind` file (format guarantee **G3**,
//! `docs/FORMAT.md` §1): the on-disk bytes are byte-identical across platforms
//! because every multi-byte integer is written **fixed little-endian**, never
//! in the host's native byte order.
//!
//! A pure byte-for-byte golden of a full store is not reproducible through the
//! public API (ULIDs and `created_at` timestamps are generated inside
//! `remember` and are not injectable), so this test proves G3 where it is both
//! observable *and* host-endianness-independent: the header (page 0), whose
//! layout is pinned byte-for-byte in `docs/FORMAT.md` §4. If the engine ever
//! wrote native-endian instead of little-endian, these bytes would differ on a
//! big-endian host — the assertions below would then be wrong there, which is
//! exactly the regression G3 forbids.
//!
//! We read the header straight out of the in-memory VFS snapshot (the same
//! bytes that would hit disk), so the check is hermetic and parallel-safe.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use embedmind_core::api::{MemoryDraft, Store, StoreOptions};
use embedmind_core::storage::sim::SimVfs;
use embedmind_core::storage::vfs::Vfs;

const STORE: &str = "memory.mind";

// Header field offsets, verbatim from docs/FORMAT.md §4. Kept local to the
// test so a silent offset change in the engine is caught here, not masked by
// importing the engine's own constants.
const OFF_MAGIC: usize = 0;
const OFF_FORMAT_VERSION: usize = 8;
const OFF_PAGE_SIZE: usize = 12;
const OFF_PAGE_COUNT: usize = 16;
const OFF_FTS_ROOT: usize = 156;
const MAGIC: &[u8; 8] = b"MINDFMT1";

/// Builds a small store (no embedder — the header layout under test is
/// independent of the vector index) and returns page 0's bytes as they sit in
/// the VFS, i.e. as they would sit on disk.
fn header_bytes() -> Vec<u8> {
    // Keep the concrete SimVfs to reach `snapshot`; hand `Store` the same
    // allocation upcast to `dyn Vfs`.
    let sim = Arc::new(SimVfs::new());
    let vfs: Arc<dyn Vfs> = Arc::clone(&sim) as Arc<dyn Vfs>;
    let opts = StoreOptions {
        page_size: 4096,
        ..StoreOptions::default()
    };
    let mut store = Store::create_with(vfs, Path::new(STORE), opts).unwrap();
    // A couple of committed writes so page_count and txn_counter advance past
    // their initial values — the header is genuinely populated, not pristine.
    store
        .remember(MemoryDraft::new("portable memory one"))
        .unwrap();
    store
        .remember(MemoryDraft::new("portable memory two"))
        .unwrap();
    store.close().unwrap();

    let snapshot = sim
        .snapshot(Path::new(STORE))
        .expect("store file exists in the VFS");
    assert!(
        snapshot.len() >= 4096,
        "page 0 must be a full page: got {} bytes",
        snapshot.len()
    );
    snapshot
}

/// Reads a little-endian u32 the way a portable reader on *any* host must.
fn le_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
}
fn le_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
}

#[test]
fn header_is_fixed_little_endian_per_format_spec() {
    let header = header_bytes();

    // Magic: raw ASCII, no endianness — but its presence anchors the offsets.
    assert_eq!(
        &header[OFF_MAGIC..OFF_MAGIC + 8],
        MAGIC,
        "magic must be ASCII MINDFMT1 at offset 0 (FORMAT §4)"
    );

    // format_version = 7 (filter-meta sidecar, ADR 0027), written
    // little-endian: bytes must be 07 00 00 00, NOT 00 00 00 07
    // (big-endian). This is the concrete G3 assertion — it would fail on a
    // big-endian host if the engine used native byte order.
    assert_eq!(
        &header[OFF_FORMAT_VERSION..OFF_FORMAT_VERSION + 4],
        &[0x07, 0x00, 0x00, 0x00],
        "format_version must be little-endian 7"
    );
    assert_eq!(le_u32(&header, OFF_FORMAT_VERSION), 7);

    // page_size = 4096 = 0x1000, little-endian: 00 10 00 00.
    assert_eq!(
        &header[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4],
        &[0x00, 0x10, 0x00, 0x00],
        "page_size must be little-endian 4096"
    );
    assert_eq!(le_u32(&header, OFF_PAGE_SIZE), 4096);

    // page_count is a u64 that grew past 1; whatever it is, reading it
    // little-endian must yield a sane small count and the high bytes must be
    // zero (a native-endian write on big-endian would smear the value into the
    // high bytes).
    let page_count = le_u64(&header, OFF_PAGE_COUNT);
    assert!(
        (2..1024).contains(&page_count),
        "page_count read little-endian must be a small sane value, got {page_count}"
    );

    // fts_root_page (ADR 0011, FORMAT §4/§11): the two `remember`s indexed
    // content, so the full-text meta page exists — a small, sane page number
    // read little-endian, with the high bytes zero (a native-endian write on a
    // big-endian host would smear it). This also pins the field's offset (156).
    let fts_root = le_u64(&header, OFF_FTS_ROOT);
    assert!(
        (2..1024).contains(&fts_root),
        "fts_root_page read little-endian must be a small sane page number, got {fts_root}"
    );
}

/// The reverse guarantee: a `.mind` written here reopens with identical
/// logical content — the round-trip that portability ultimately protects.
#[test]
fn written_store_reopens_with_identical_content() {
    let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
    let opts = StoreOptions {
        page_size: 4096,
        ..StoreOptions::default()
    };
    let mut store = Store::create_with(Arc::clone(&vfs), Path::new(STORE), opts.clone()).unwrap();
    let id = store
        .remember(MemoryDraft::new("survives a reopen unchanged").project("portability"))
        .unwrap()
        .id;
    store.close().unwrap();

    let store = Store::open_with(Arc::clone(&vfs), Path::new(STORE), opts).unwrap();
    let got = store.get(id).unwrap().expect("memory must survive reopen");
    assert_eq!(got.content, "survives a reopen unchanged");
    assert_eq!(got.project.as_deref(), Some("portability"));
}
