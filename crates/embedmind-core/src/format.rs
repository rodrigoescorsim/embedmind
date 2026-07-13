//! Binary layout of the `.mind` file: constants, header, page framing,
//! checksums, WAL framing. Normative spec: `docs/FORMAT.md` — this module
//! implements it and must never drift from it. Everything here is explicitly
//! (de)serialized, little-endian, and fuzzable; no struct is ever written to
//! disk as raw memory.

use crate::error::{Error, Result};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

/// Magic bytes at offset 0 of every `.mind` file (`docs/FORMAT.md` §4).
pub const MAGIC: [u8; 8] = *b"MINDFMT1";

/// Magic bytes at offset 0 of the WAL sidecar (`docs/FORMAT.md` §8).
pub const WAL_MAGIC: [u8; 8] = *b"MINDWAL1";

/// Current on-disk format version written by this build.
///
/// - `1` (v0.1): header + records + vectors + HNSW.
/// - `2` (M2, `docs/adr/0011`): adds the paged inverted full-text index
///   (`FtsDict`/`FtsPostings` pages + the `fts_root_page` header field). A
///   version-1 file has no full-text index; opened by a version-2 build it
///   reads and writes fine — `fts_root_page` is 0 (bytes reserved as zero in
///   v1, `docs/FORMAT.md` §4), so `recall` degrades to vector-only until the
///   file is rewritten. The layout of every pre-existing field is unchanged,
///   so this is an additive bump, not a breaking one (`docs/FORMAT.md` §10
///   rule 1: new meaning carried in previously-reserved bytes).
/// - `3` (M3, `docs/adr/0012`): adds the graph layer (`GraphDict`/
///   `GraphOverflow` pages + the `graph_root_page` header field). Same
///   additive pattern: an older file decodes with `graph_root_page` 0 = no
///   graph, and `related`/recall expansion degrade to empty.
/// - `4` (S26, `docs/adr/0021`): full-text postings bodies switch from
///   fixed-width entries to delta+varint encoding (`docs/FORMAT.md` §11). The
///   layout is selected by the file's `format_version`, never mixed within a
///   file: a version-≤3 file keeps reading **and writing** the fixed-width
///   layout under this build (degrades in size/speed, never in correctness),
///   and `vacuum`'s rebuild-by-copy re-encodes it into a fresh version-4
///   file. No header field or page type changes.
pub const FORMAT_VERSION: u32 = 4;

/// Default page size in bytes. The authoritative value for an existing file is
/// the one recorded in its header.
pub const DEFAULT_PAGE_SIZE: u32 = 4096;

/// Smallest supported page size (must fit the fixed header fields + trailer).
pub const MIN_PAGE_SIZE: u32 = 512;

/// Largest supported page size.
pub const MAX_PAGE_SIZE: u32 = 65536;

/// Size of the per-page checksum trailer (xxh3_64), in bytes.
pub const PAGE_TRAILER_LEN: usize = 8;

/// Maximum byte length of `embedding_model_id` in the header (`docs/FORMAT.md` §4).
pub const MAX_MODEL_ID_LEN: usize = 64;

/// Size of the WAL file header (`docs/FORMAT.MD` §8).
pub const WAL_HEADER_LEN: usize = 32;

/// Size of a WAL frame header (`docs/FORMAT.md` §8). Each frame is this header
/// followed by one full page image.
pub const WAL_FRAME_HEADER_LEN: usize = 32;

/// Header `flags` bit 0: file is encrypted (reserved for the future, must be 0 in v1).
pub const FLAG_ENCRYPTED: u32 = 1;

/// Page types (`docs/FORMAT.md` §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// B-tree interior node.
    BtreeInner = 0x01,
    /// B-tree leaf holding memory records.
    BtreeLeaf = 0x02,
    /// Embedding vector blocks.
    Vector = 0x03,
    /// HNSW graph nodes.
    HnswNode = 0x04,
    /// HNSW index parameters and entry point.
    HnswMeta = 0x05,
    /// Free page list.
    Freelist = 0x06,
    /// Continuation of an oversized record.
    Overflow = 0x07,
    /// Full-text dictionary node (term → postings), slotted B-tree
    /// (`docs/FORMAT.md` §11, `docs/adr/0011`). Inner and leaf are
    /// distinguished by the `is_leaf` byte inside the page body, not by the
    /// page type, so both share this one type.
    FtsDict = 0x08,
    /// Full-text postings continuation: an oversized postings list spilled
    /// out of its dictionary leaf cell, chained like [`PageType::Overflow`]
    /// but carrying FTS payload (`docs/FORMAT.md` §11).
    FtsPostings = 0x09,
    /// Graph dictionary node (entity/memory key → value), same slotted
    /// B-tree layout as [`PageType::FtsDict`] with meta/inner/leaf told
    /// apart by the node-kind byte in the page body (`docs/FORMAT.md` §12,
    /// `docs/adr/0012`).
    GraphDict = 0x0A,
    /// Graph value continuation: an oversized entity-members or adjacency
    /// body spilled out of its dictionary leaf cell, chained like
    /// [`PageType::FtsPostings`] (`docs/FORMAT.md` §12).
    GraphOverflow = 0x0B,
}

impl PageType {
    /// Parses the on-disk `page_type` byte. `None` = unknown type (corrupt
    /// page or a future minor-compatible type this build must not guess at).
    pub fn from_u8(v: u8) -> Option<PageType> {
        match v {
            0x01 => Some(PageType::BtreeInner),
            0x02 => Some(PageType::BtreeLeaf),
            0x03 => Some(PageType::Vector),
            0x04 => Some(PageType::HnswNode),
            0x05 => Some(PageType::HnswMeta),
            0x06 => Some(PageType::Freelist),
            0x07 => Some(PageType::Overflow),
            0x08 => Some(PageType::FtsDict),
            0x09 => Some(PageType::FtsPostings),
            0x0A => Some(PageType::GraphDict),
            0x0B => Some(PageType::GraphOverflow),
            _ => None,
        }
    }
}

/// Returns `true` if `page_size` is a supported value (power of two within
/// [`MIN_PAGE_SIZE`], [`MAX_PAGE_SIZE`]).
pub fn page_size_is_valid(page_size: u32) -> bool {
    (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) && page_size.is_power_of_two()
}

/// Size of the common page header on every page except page 0
/// (`docs/FORMAT.md` §3).
pub const PAGE_HEADER_LEN: usize = 16;

/// Common 16-byte header of every page except page 0 (`docs/FORMAT.md` §3):
/// `page_type` (1) · reserved (3, zero) · `entry_count` (u32) ·
/// `next_page` (u64). The meaning of `entry_count` and `next_page` depends on
/// the page type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    /// What this page holds.
    pub page_type: PageType,
    /// Type-dependent count (slots in a B-tree leaf, keys in an inner node,
    /// payload bytes in an overflow page, …).
    pub entry_count: u32,
    /// Type-dependent chain pointer (overflow/freelist); 0 = none.
    pub next_page: u64,
}

impl PageHeader {
    /// Writes the header into the first [`PAGE_HEADER_LEN`] bytes of `page`
    /// (reserved bytes zeroed). `page` must be at least that long.
    pub fn encode_into(&self, page: &mut [u8]) {
        write_bytes(page, 0, &[self.page_type as u8, 0, 0, 0]);
        write_u32(page, 4, self.entry_count);
        write_u64(page, 8, self.next_page);
    }

    /// Decodes the common header from the start of a page. `None` = not a
    /// valid page header (unknown type or short buffer); the caller attaches
    /// the page number to the resulting typed error.
    pub fn decode(page: &[u8]) -> Option<Self> {
        if page.len() < PAGE_HEADER_LEN {
            return None;
        }
        Some(PageHeader {
            page_type: PageType::from_u8(*page.first()?)?,
            entry_count: read_u32(page, 4)?,
            next_page: read_u64(page, 8)?,
        })
    }
}

/// Computes the checksum of a full page: xxh3_64 over `[0, len - 8)`
/// (`docs/FORMAT.md` §3). `page` must be at least [`PAGE_TRAILER_LEN`] bytes.
pub fn page_checksum(page: &[u8]) -> u64 {
    let body_len = page.len().saturating_sub(PAGE_TRAILER_LEN);
    xxh3_64(page.get(..body_len).unwrap_or_default())
}

/// Writes the checksum trailer into the last 8 bytes of `page`.
pub fn stamp_page_checksum(page: &mut [u8]) {
    let sum = page_checksum(page).to_le_bytes();
    let len = page.len();
    if let Some(trailer) = page.get_mut(len.saturating_sub(PAGE_TRAILER_LEN)..) {
        trailer.copy_from_slice(&sum);
    }
}

/// Verifies the checksum trailer of a full page.
pub fn page_checksum_is_valid(page: &[u8]) -> bool {
    if page.len() < PAGE_TRAILER_LEN {
        return false;
    }
    let stored = read_u64(page, page.len() - PAGE_TRAILER_LEN);
    stored == Some(page_checksum(page))
}

// ---------------------------------------------------------------------------
// Header (page 0) — docs/FORMAT.md §4
// ---------------------------------------------------------------------------

// Fixed field offsets within page 0 (docs/FORMAT.md §4).
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_PAGE_SIZE: usize = 12;
const OFF_PAGE_COUNT: usize = 16;
const OFF_ROOT_BTREE: usize = 24;
const OFF_FREELIST: usize = 32;
const OFF_HNSW_META: usize = 40;
const OFF_TXN_COUNTER: usize = 48;
const OFF_DIMS: usize = 56;
const OFF_QUANT: usize = 58;
const OFF_MODEL_ID: usize = 60;
const OFF_FLAGS: usize = 128;
// kdf_salt (132..148) and kdf_params (148..156) are reserved for encryption.
// `fts_root_page` (docs/adr/0011) lives at offset 156, which was reserved and
// written as zero by format_version 1 — so a v1 file reads back with no
// full-text index (root 0), exactly the intended degradation.
const OFF_FTS_ROOT: usize = 156;
// `graph_root_page` (docs/adr/0012) lives at offset 164, reserved-and-zero
// through format_version 2 — an older file reads back with no graph (root 0).
const OFF_GRAPH_ROOT: usize = 164;

/// Minimum prefix of page 0 needed by [`Header::peek_page_size`].
pub const HEADER_PEEK_LEN: usize = 16;

/// Decoded `.mind` header (page 0). The `kdf_salt`/`kdf_params` reservation is
/// not represented: v1 writes zeros and refuses files with the encrypted flag
/// set (see `docs/adr/0007`), so there is nothing to carry around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// On-disk format version (`FORMAT_VERSION` for files written by this build).
    pub format_version: u32,
    /// Page size in bytes; authoritative for the whole file.
    pub page_size: u32,
    /// Total pages including this header page.
    pub page_count: u64,
    /// Root page of the record B-tree; 0 = none yet.
    pub root_btree_page: u64,
    /// Head of the freelist chain; 0 = empty.
    pub freelist_page: u64,
    /// HNSW meta page; 0 = no vector index yet.
    pub hnsw_meta_page: u64,
    /// Root page of the full-text dictionary B-tree; 0 = no full-text index
    /// yet (`docs/adr/0011`, `docs/FORMAT.md` §11). Always 0 in a file written
    /// by format_version 1 (the bytes were reserved), which is how a v1 file
    /// is detected as lacking the index and degrades to vector-only recall.
    pub fts_root_page: u64,
    /// Graph meta page; 0 = no graph yet (`docs/adr/0012`, `docs/FORMAT.md`
    /// §12). Always 0 in a file written by format_version ≤ 2 (the bytes were
    /// reserved), so older files degrade to "no related memories".
    pub graph_root_page: u64,
    /// Last committed transaction id.
    pub txn_counter: u64,
    /// Embedding dimensions (0 = embeddings not configured yet).
    pub embedding_dims: u16,
    /// Embedding quantization: 0 = f32, 1 = i8 (reserved for M3).
    pub embedding_quant: u16,
    /// Embedding model identifier, at most [`MAX_MODEL_ID_LEN`] bytes of UTF-8.
    pub embedding_model_id: String,
    /// Header flags; bit 0 = encrypted (must be 0 in v1).
    pub flags: u32,
}

impl Header {
    /// A fresh header for a newly created store: one page (the header itself),
    /// no roots, no transactions, embeddings unconfigured.
    pub fn new(page_size: u32) -> Result<Self> {
        if !page_size_is_valid(page_size) {
            return Err(Error::InvalidArgument("unsupported page size"));
        }
        Ok(Header {
            format_version: FORMAT_VERSION,
            page_size,
            page_count: 1,
            root_btree_page: 0,
            freelist_page: 0,
            hnsw_meta_page: 0,
            fts_root_page: 0,
            graph_root_page: 0,
            txn_counter: 0,
            embedding_dims: 0,
            embedding_quant: 0,
            embedding_model_id: String::new(),
            flags: 0,
        })
    }

    /// Reads the recorded `page_size` from the first bytes of page 0 so the
    /// caller can then read the full header page. Needs at least
    /// [`HEADER_PEEK_LEN`] bytes. Validates magic and page-size sanity only.
    pub fn peek_page_size(prefix: &[u8]) -> Result<u32> {
        if prefix.get(OFF_MAGIC..OFF_MAGIC + 8) != Some(&MAGIC[..]) {
            return Err(Error::BadHeader);
        }
        let page_size = read_u32(prefix, OFF_PAGE_SIZE).ok_or(Error::BadHeader)?;
        if !page_size_is_valid(page_size) {
            return Err(Error::BadHeader);
        }
        Ok(page_size)
    }

    /// Encodes the header into `page`, which must be exactly `page_size` bytes.
    /// Reserved regions are zeroed; the checksum trailer is stamped.
    pub fn encode(&self, page: &mut [u8]) -> Result<()> {
        if page.len() != self.page_size as usize || !page_size_is_valid(self.page_size) {
            return Err(Error::InvalidArgument("header buffer must be one page"));
        }
        let id = self.embedding_model_id.as_bytes();
        if id.len() > MAX_MODEL_ID_LEN {
            return Err(Error::InvalidArgument(
                "embedding_model_id exceeds 64 bytes",
            ));
        }
        if self.flags & FLAG_ENCRYPTED != 0 {
            return Err(Error::InvalidArgument("encrypted flag is reserved in v1"));
        }
        page.fill(0);
        write_bytes(page, OFF_MAGIC, &MAGIC);
        write_u32(page, OFF_VERSION, self.format_version);
        write_u32(page, OFF_PAGE_SIZE, self.page_size);
        write_u64(page, OFF_PAGE_COUNT, self.page_count);
        write_u64(page, OFF_ROOT_BTREE, self.root_btree_page);
        write_u64(page, OFF_FREELIST, self.freelist_page);
        write_u64(page, OFF_HNSW_META, self.hnsw_meta_page);
        write_u64(page, OFF_TXN_COUNTER, self.txn_counter);
        write_u16(page, OFF_DIMS, self.embedding_dims);
        write_u16(page, OFF_QUANT, self.embedding_quant);
        write_u32(page, OFF_MODEL_ID, id.len() as u32);
        write_bytes(page, OFF_MODEL_ID + 4, id);
        write_u32(page, OFF_FLAGS, self.flags);
        // kdf_salt (132..148) and kdf_params (148..156) stay zero in v1.
        write_u64(page, OFF_FTS_ROOT, self.fts_root_page);
        write_u64(page, OFF_GRAPH_ROOT, self.graph_root_page);
        stamp_page_checksum(page);
        Ok(())
    }

    /// Decodes and validates page 0. `page` must be the full header page at
    /// the recorded page size (use [`Header::peek_page_size`] first).
    ///
    /// Check order implements the format guarantees: magic (`BadHeader`),
    /// version policy G4 (`UnsupportedVersion`, checked before the checksum so
    /// a future layout is never misreported as corruption), checksum G1
    /// (`CorruptPage`), then the encrypted-flag refusal (`Encrypted`).
    pub fn decode(page: &[u8]) -> Result<Self> {
        if page.get(OFF_MAGIC..OFF_MAGIC + 8) != Some(&MAGIC[..]) {
            return Err(Error::BadHeader);
        }
        let format_version = read_u32(page, OFF_VERSION).ok_or(Error::BadHeader)?;
        if format_version == 0 {
            return Err(Error::BadHeader);
        }
        if format_version > FORMAT_VERSION {
            return Err(Error::UnsupportedVersion {
                found: format_version,
                supported: FORMAT_VERSION,
            });
        }
        let page_size = read_u32(page, OFF_PAGE_SIZE).ok_or(Error::BadHeader)?;
        if !page_size_is_valid(page_size) || page.len() != page_size as usize {
            return Err(Error::BadHeader);
        }
        if !page_checksum_is_valid(page) {
            return Err(Error::CorruptPage { page_no: 0 });
        }
        let flags = read_u32(page, OFF_FLAGS).ok_or(Error::BadHeader)?;
        if flags & FLAG_ENCRYPTED != 0 {
            return Err(Error::Encrypted);
        }
        let id_len = read_u32(page, OFF_MODEL_ID).ok_or(Error::BadHeader)? as usize;
        if id_len > MAX_MODEL_ID_LEN {
            return Err(Error::BadHeader);
        }
        let id_bytes = page
            .get(OFF_MODEL_ID + 4..OFF_MODEL_ID + 4 + id_len)
            .ok_or(Error::BadHeader)?;
        let embedding_model_id =
            String::from_utf8(id_bytes.to_vec()).map_err(|_| Error::BadHeader)?;
        let page_count = read_u64(page, OFF_PAGE_COUNT).ok_or(Error::BadHeader)?;
        if page_count == 0 {
            return Err(Error::BadHeader);
        }
        Ok(Header {
            format_version,
            page_size,
            page_count,
            root_btree_page: read_u64(page, OFF_ROOT_BTREE).ok_or(Error::BadHeader)?,
            freelist_page: read_u64(page, OFF_FREELIST).ok_or(Error::BadHeader)?,
            hnsw_meta_page: read_u64(page, OFF_HNSW_META).ok_or(Error::BadHeader)?,
            // Reserved-and-zero under format_version 1, so a v1 file decodes
            // with no full-text index — the deliberate degradation path.
            fts_root_page: read_u64(page, OFF_FTS_ROOT).ok_or(Error::BadHeader)?,
            // Reserved-and-zero through format_version 2: older files decode
            // with no graph, same degradation pattern (docs/adr/0012).
            graph_root_page: read_u64(page, OFF_GRAPH_ROOT).ok_or(Error::BadHeader)?,
            txn_counter: read_u64(page, OFF_TXN_COUNTER).ok_or(Error::BadHeader)?,
            embedding_dims: read_u16(page, OFF_DIMS).ok_or(Error::BadHeader)?,
            embedding_quant: read_u16(page, OFF_QUANT).ok_or(Error::BadHeader)?,
            embedding_model_id,
            flags,
        })
    }
}

// ---------------------------------------------------------------------------
// WAL framing — docs/FORMAT.md §8
// ---------------------------------------------------------------------------

/// Decoded WAL file header. An unparsable WAL header means "no valid WAL":
/// callers treat it as an empty log, never as an error (a torn header can only
/// exist if no commit was ever acknowledged from that WAL generation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalHeader {
    /// Format version of the store this WAL belongs to.
    pub format_version: u32,
    /// Page size of the store; frame images are exactly this long.
    pub page_size: u32,
    /// Random per-generation salt; seeds every frame checksum so frames from
    /// an earlier, truncated WAL generation can never replay.
    pub salt: u64,
}

impl WalHeader {
    /// Encodes the 32-byte WAL header.
    pub fn encode(&self) -> [u8; WAL_HEADER_LEN] {
        let mut buf = [0u8; WAL_HEADER_LEN];
        write_bytes(&mut buf, 0, &WAL_MAGIC);
        write_u32(&mut buf, 8, self.format_version);
        write_u32(&mut buf, 12, self.page_size);
        write_u64(&mut buf, 16, self.salt);
        buf
    }

    /// Decodes and validates a WAL header. `None` = not a valid WAL.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.get(..8) != Some(&WAL_MAGIC[..]) {
            return None;
        }
        let format_version = read_u32(buf, 8)?;
        let page_size = read_u32(buf, 12)?;
        if format_version == 0 || format_version > FORMAT_VERSION || !page_size_is_valid(page_size)
        {
            return None;
        }
        Some(WalHeader {
            format_version,
            page_size,
            salt: read_u64(buf, 16)?,
        })
    }
}

/// Decoded WAL frame header. A frame is this header + one page image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalFrameHeader {
    /// Page this image belongs to.
    pub page_no: u64,
    /// Transaction that wrote it.
    pub txn_id: u64,
    /// `true` on the last frame of a transaction (the commit frame).
    pub commit: bool,
}

impl WalFrameHeader {
    /// Encodes the 32-byte frame header, computing the checksum over the
    /// header prefix plus `page_image`, seeded with the WAL `salt`.
    pub fn encode(&self, page_image: &[u8], salt: u64) -> [u8; WAL_FRAME_HEADER_LEN] {
        let mut buf = [0u8; WAL_FRAME_HEADER_LEN];
        write_u64(&mut buf, 0, self.page_no);
        write_u64(&mut buf, 8, self.txn_id);
        buf[16] = u8::from(self.commit);
        let sum = frame_checksum(&buf, page_image, salt);
        write_u64(&mut buf, 24, sum);
        buf
    }

    /// Decodes a frame header and verifies its checksum against `page_image`.
    /// `None` = invalid frame; per §8 this ends the valid WAL prefix.
    pub fn decode(buf: &[u8], page_image: &[u8], salt: u64) -> Option<Self> {
        if buf.len() < WAL_FRAME_HEADER_LEN {
            return None;
        }
        let stored = read_u64(buf, 24)?;
        if stored != frame_checksum(buf, page_image, salt) {
            return None;
        }
        let commit_byte = *buf.get(16)?;
        if commit_byte > 1 {
            return None;
        }
        Some(WalFrameHeader {
            page_no: read_u64(buf, 0)?,
            txn_id: read_u64(buf, 8)?,
            commit: commit_byte == 1,
        })
    }
}

/// Frame checksum: xxh3_64 seeded with the WAL salt over frame-header bytes
/// `[0, 24)` plus the page image (`docs/FORMAT.md` §8).
pub fn frame_checksum(frame_header: &[u8], page_image: &[u8], salt: u64) -> u64 {
    let mut hasher = Xxh3::with_seed(salt);
    hasher.update(frame_header.get(..24).unwrap_or_default());
    hasher.update(page_image);
    hasher.digest()
}

// ---------------------------------------------------------------------------
// Vector pages — docs/FORMAT.md §6
// ---------------------------------------------------------------------------

/// Bytes per dimension for the only representation v1 writes (f32; i8
/// quantization is reserved for M3, `embedding_quant = 1`).
const VECTOR_STRIDE_F32: usize = 4;

/// Slot capacity of a VECTOR page at the given page size and embedding
/// dimensionality. `entry_count` counts occupied slots, filled in order (a
/// bump allocator — vectors are never removed in place, only orphaned like
/// overflow chains until `vacuum`), so a page is full once `entry_count`
/// reaches this value.
pub fn vector_slots_per_page(page_size: u32, dims: u16) -> usize {
    let stride = usize::from(dims) * VECTOR_STRIDE_F32;
    if stride == 0 {
        return 0;
    }
    (page_size as usize - PAGE_HEADER_LEN - PAGE_TRAILER_LEN) / stride
}

/// Appends one L2-normalized vector to a VECTOR page at its next free slot.
/// `page` must already be a valid (possibly empty/fresh) VECTOR page of the
/// recorded `dims`. Returns the slot index. `None` = page is full.
pub fn vector_page_push(page: &mut [u8], dims: u16, vector: &[f32]) -> Result<Option<u16>> {
    if vector.len() != usize::from(dims) {
        return Err(Error::InvalidArgument("vector length != header dims"));
    }
    let page_size = page.len() as u32;
    let capacity = vector_slots_per_page(page_size, dims);
    let header = PageHeader::decode(page).ok_or(Error::InvalidArgument("not a valid page"))?;
    if header.page_type != PageType::Vector {
        return Err(Error::InvalidArgument("not a VECTOR page"));
    }
    let used = header.entry_count as usize;
    if used >= capacity {
        return Ok(None);
    }
    let stride = usize::from(dims) * VECTOR_STRIDE_F32;
    let offset = PAGE_HEADER_LEN + used * stride;
    for (i, v) in vector.iter().enumerate() {
        write_bytes(page, offset + i * VECTOR_STRIDE_F32, &v.to_le_bytes());
    }
    PageHeader {
        page_type: PageType::Vector,
        entry_count: used as u32 + 1,
        next_page: header.next_page,
    }
    .encode_into(page);
    Ok(Some(used as u16))
}

/// Reads back the vector at `slot` of a VECTOR page.
pub fn vector_page_get(page: &[u8], dims: u16, slot: u16, page_no: u64) -> Result<Vec<f32>> {
    let header = PageHeader::decode(page).ok_or(Error::MalformedPage {
        page_no,
        what: "page header",
    })?;
    if header.page_type != PageType::Vector {
        return Err(Error::MalformedPage {
            page_no,
            what: "not a VECTOR page",
        });
    }
    if u32::from(slot) >= header.entry_count {
        return Err(Error::MalformedPage {
            page_no,
            what: "vector slot out of range",
        });
    }
    let stride = usize::from(dims) * VECTOR_STRIDE_F32;
    let offset = PAGE_HEADER_LEN + usize::from(slot) * stride;
    let bytes = page
        .get(offset..offset + stride)
        .ok_or(Error::MalformedPage {
            page_no,
            what: "vector slot bounds",
        })?;
    Ok(bytes
        .chunks_exact(VECTOR_STRIDE_F32)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Initializes a fresh, empty VECTOR page in place.
pub fn init_vector_page(page: &mut [u8]) {
    PageHeader {
        page_type: PageType::Vector,
        entry_count: 0,
        next_page: 0,
    }
    .encode_into(page);
}

// ---------------------------------------------------------------------------
// HNSW pages — docs/FORMAT.md §7
// ---------------------------------------------------------------------------

/// Neighbor cap per layer (`docs/adr/0002`): `M` at layers >= 1, `2*M` at
/// layer 0 (the HNSW paper's standard doubling for the base layer).
pub const HNSW_DEFAULT_M: u16 = 16;
/// Default `ef_construction` (`docs/adr/0002`).
pub const HNSW_DEFAULT_EF_CONSTRUCTION: u16 = 200;
/// Default `ef_search` (`docs/adr/0002`); callers may raise it per query.
pub const HNSW_DEFAULT_EF_SEARCH: u16 = 64;

/// Decoded HNSW_META page: index parameters and the graph entry point.
/// **Fixed size** — it never grows with the index. There is no node location
/// table: graph adjacency addresses HNSW_NODE pages directly by `page_no`
/// (`docs/adr/0008`), so the index scales to any node count with O(1) meta
/// I/O per insert and one page read per traversal hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HnswMeta {
    /// Max neighbors per node at layers >= 1 (layer 0 uses `2 * m`).
    pub m: u16,
    /// Candidate list size during insertion.
    pub ef_construction: u16,
    /// Highest occupied layer across all nodes; `0` = empty or single-layer.
    pub max_layer: u8,
    /// Page of the current entry-point node; meaningful only if
    /// `node_count > 0` (and then it is never 0).
    pub entry_point_page: u64,
    /// Total nodes in the graph. Also seeds the deterministic level
    /// assignment for the next insert.
    pub node_count: u64,
}

impl HnswMeta {
    /// A fresh, empty index at the default parameters.
    pub fn new() -> Self {
        HnswMeta {
            m: HNSW_DEFAULT_M,
            ef_construction: HNSW_DEFAULT_EF_CONSTRUCTION,
            max_layer: 0,
            entry_point_page: 0,
            node_count: 0,
        }
    }

    /// Encodes into `page`, which must be exactly one page.
    pub fn encode(&self, page: &mut [u8]) -> Result<()> {
        if self.m == 0 {
            return Err(Error::InvalidArgument("hnsw m must be >= 1"));
        }
        if self.node_count > 0 && self.entry_point_page == 0 {
            return Err(Error::InvalidArgument(
                "non-empty hnsw index requires an entry point",
            ));
        }
        page.fill(0);
        PageHeader {
            page_type: PageType::HnswMeta,
            entry_count: 0, // reserved (FORMAT.md §2)
            next_page: 0,
        }
        .encode_into(page);
        let mut off = PAGE_HEADER_LEN;
        write_u16(page, off, self.m);
        off += 2;
        write_u16(page, off, self.ef_construction);
        off += 2;
        page[off] = self.max_layer;
        off += 1;
        write_u64(page, off, self.entry_point_page);
        off += 8;
        write_u64(page, off, self.node_count);
        stamp_page_checksum(page);
        Ok(())
    }

    /// Decodes and validates an HNSW_META page.
    pub fn decode(page: &[u8], page_no: u64) -> Result<Self> {
        let header = PageHeader::decode(page).ok_or(Error::MalformedPage {
            page_no,
            what: "page header",
        })?;
        if header.page_type != PageType::HnswMeta {
            return Err(Error::MalformedPage {
                page_no,
                what: "not an HNSW_META page",
            });
        }
        let mut off = PAGE_HEADER_LEN;
        let m = read_u16(page, off).ok_or(Error::MalformedPage { page_no, what: "m" })?;
        off += 2;
        if m == 0 {
            return Err(Error::MalformedPage {
                page_no,
                what: "hnsw m is zero",
            });
        }
        let ef_construction = read_u16(page, off).ok_or(Error::MalformedPage {
            page_no,
            what: "ef_construction",
        })?;
        off += 2;
        let max_layer = *page.get(off).ok_or(Error::MalformedPage {
            page_no,
            what: "max_layer",
        })?;
        off += 1;
        let entry_point_page = read_u64(page, off).ok_or(Error::MalformedPage {
            page_no,
            what: "entry_point_page",
        })?;
        off += 8;
        let node_count = read_u64(page, off).ok_or(Error::MalformedPage {
            page_no,
            what: "node_count",
        })?;
        if node_count > 0 && entry_point_page == 0 {
            return Err(Error::MalformedPage {
                page_no,
                what: "hnsw entry point missing",
            });
        }
        Ok(HnswMeta {
            m,
            ef_construction,
            max_layer,
            entry_point_page,
            node_count,
        })
    }
}

/// Highest level a node may be assigned so that a **full** node (every layer
/// at its neighbor cap) still fits one page. `None` = even a full layer-0
/// node does not fit — this `(page_size, m)` combination cannot host an
/// index (a misconfiguration or hostile meta page, reported as a typed error
/// by the index layer). At the default page size and `M` the level cap is far
/// above what the level distribution ever produces (29 at 4 KiB, M=16), so
/// level assignment clamps to it and `HnswNode::encode` can never fail for a
/// well-formed index.
pub fn max_hnsw_level(page_size: u32, m: u16) -> Option<usize> {
    let usable = page_size as usize - PAGE_HEADER_LEN - PAGE_TRAILER_LEN;
    let fixed = 16 + 8 + 2 + 1; // record_id + vec_page + vec_slot + layer_count
    let layer0 = 2 + usize::from(m) * 2 * 8; // u16 count + 2*M neighbors (u64)
    let upper = 2 + usize::from(m) * 8; // u16 count + M neighbors (u64)
    usable
        .checked_sub(fixed + layer0)
        .map(|rest| (rest / upper).min(31))
}

impl Default for HnswMeta {
    fn default() -> Self {
        Self::new()
    }
}

/// Decoded HNSW_NODE: the embedding a graph node indexes and its per-layer
/// adjacency (`docs/FORMAT.md` §7). Neighbors are **HNSW_NODE page numbers**
/// (u64) — direct addressing, no id-to-page table (`docs/adr/0008`).
/// Adjacency is bounded (`<= m` per layer, `<= 2*m` at layer 0) and the level
/// is capped by [`max_hnsw_level`], so a node always fits one page. The
/// vector location is duplicated here (also reachable via the memory
/// record's `vec_ref`) so search reads one page per candidate instead of a
/// B-tree lookup per hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HnswNode {
    /// The memory record this node embeds.
    pub record_id: ulid::Ulid,
    /// VECTOR page holding this node's embedding.
    pub vec_page: u64,
    /// Slot within that page.
    pub vec_slot: u16,
    /// Per-layer neighbor lists (HNSW_NODE page numbers), layer 0 first.
    pub layers: Vec<Vec<u64>>,
}

impl HnswNode {
    /// Encodes into a fresh page. `None` = does not fit at this page size —
    /// impossible for nodes built by the engine (level is clamped to
    /// [`max_hnsw_level`] and adjacency to the layer caps), so callers treat
    /// it as an internal error.
    pub fn encode(&self, page_size: u32) -> Option<Vec<u8>> {
        if self.layers.len() > u8::MAX as usize {
            return None;
        }
        let mut body = Vec::with_capacity(16 + 8 + 2 + 1 + self.layers.len() * 8);
        body.extend_from_slice(&self.record_id.to_bytes());
        body.extend_from_slice(&self.vec_page.to_le_bytes());
        body.extend_from_slice(&self.vec_slot.to_le_bytes());
        body.push(self.layers.len() as u8);
        for layer in &self.layers {
            let count = u16::try_from(layer.len()).ok()?;
            body.extend_from_slice(&count.to_le_bytes());
            for &n in layer {
                body.extend_from_slice(&n.to_le_bytes());
            }
        }
        let total = PAGE_HEADER_LEN + body.len();
        if total > page_size as usize - PAGE_TRAILER_LEN {
            return None;
        }
        let mut page = vec![0u8; page_size as usize];
        PageHeader {
            page_type: PageType::HnswNode,
            entry_count: 0, // reserved (FORMAT.md §2)
            next_page: 0,
        }
        .encode_into(&mut page);
        page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + body.len()].copy_from_slice(&body);
        stamp_page_checksum(&mut page);
        Some(page)
    }

    /// Decodes an HNSW_NODE page.
    pub fn decode(page: &[u8], page_no: u64) -> Result<Self> {
        let header = PageHeader::decode(page).ok_or(Error::MalformedPage {
            page_no,
            what: "page header",
        })?;
        if header.page_type != PageType::HnswNode {
            return Err(Error::MalformedPage {
                page_no,
                what: "not an HNSW_NODE page",
            });
        }
        let mut off = PAGE_HEADER_LEN;
        let record_id_bytes: [u8; 16] = page
            .get(off..off + 16)
            .and_then(|b| b.try_into().ok())
            .ok_or(Error::MalformedPage {
                page_no,
                what: "hnsw record_id",
            })?;
        off += 16;
        let vec_page = read_u64(page, off).ok_or(Error::MalformedPage {
            page_no,
            what: "hnsw vec_page",
        })?;
        off += 8;
        let vec_slot = read_u16(page, off).ok_or(Error::MalformedPage {
            page_no,
            what: "hnsw vec_slot",
        })?;
        off += 2;
        let layer_count = *page.get(off).ok_or(Error::MalformedPage {
            page_no,
            what: "hnsw layer_count",
        })? as usize;
        off += 1;
        let mut layers = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            let count = read_u16(page, off).ok_or(Error::MalformedPage {
                page_no,
                what: "hnsw neighbor count",
            })? as usize;
            off += 2;
            // Guard before allocating: `count` neighbors need `count * 8`
            // bytes of remaining page (fuzz rule, docs/TESTING.md §3).
            if count * 8 > page.len().saturating_sub(off) {
                return Err(Error::MalformedPage {
                    page_no,
                    what: "hnsw neighbor count exceeds page",
                });
            }
            let mut neighbors = Vec::with_capacity(count);
            for _ in 0..count {
                let n = read_u64(page, off).ok_or(Error::MalformedPage {
                    page_no,
                    what: "hnsw neighbor page",
                })?;
                if n == 0 {
                    return Err(Error::MalformedPage {
                        page_no,
                        what: "hnsw null neighbor page",
                    });
                }
                neighbors.push(n);
                off += 8;
            }
            layers.push(neighbors);
        }
        Ok(HnswNode {
            record_id: ulid::Ulid::from_bytes(record_id_bytes),
            vec_page,
            vec_slot,
            layers,
        })
    }
}

// ---------------------------------------------------------------------------
// Little-endian field helpers (bounds-checked; no panics, no raw memcpy)
// ---------------------------------------------------------------------------

fn read_u16(buf: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(off..off + 2)?.try_into().ok()?))
}

fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

fn read_u64(buf: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(buf.get(off..off + 8)?.try_into().ok()?))
}

fn write_bytes(buf: &mut [u8], off: usize, val: &[u8]) {
    if let Some(dst) = buf.get_mut(off..off + val.len()) {
        dst.copy_from_slice(val);
    }
}

fn write_u16(buf: &mut [u8], off: usize, val: u16) {
    write_bytes(buf, off, &val.to_le_bytes());
}

fn write_u32(buf: &mut [u8], off: usize, val: u32) {
    write_bytes(buf, off, &val.to_le_bytes());
}

fn write_u64(buf: &mut [u8], off: usize, val: u64) {
    write_bytes(buf, off, &val.to_le_bytes());
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample_header() -> Header {
        Header {
            format_version: FORMAT_VERSION,
            page_size: DEFAULT_PAGE_SIZE,
            page_count: 42,
            root_btree_page: 3,
            freelist_page: 7,
            hnsw_meta_page: 9,
            fts_root_page: 11,
            graph_root_page: 13,
            txn_counter: 1234,
            embedding_dims: 384,
            embedding_quant: 0,
            embedding_model_id: "all-MiniLM-L6-v2-int8".to_owned(),
            flags: 0,
        }
    }

    #[test]
    fn magic_values_match_spec() {
        assert_eq!(&MAGIC, b"MINDFMT1");
        assert_eq!(&WAL_MAGIC, b"MINDWAL1");
    }

    #[test]
    fn header_roundtrip() {
        let h = sample_header();
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        h.encode(&mut page).unwrap();
        assert_eq!(Header::peek_page_size(&page).unwrap(), DEFAULT_PAGE_SIZE);
        assert_eq!(Header::decode(&page).unwrap(), h);
    }

    #[test]
    fn header_roundtrip_min_page_size() {
        let mut h = sample_header();
        h.page_size = MIN_PAGE_SIZE;
        let mut page = vec![0u8; MIN_PAGE_SIZE as usize];
        h.encode(&mut page).unwrap();
        assert_eq!(Header::decode(&page).unwrap(), h);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        sample_header().encode(&mut page).unwrap();
        page[0] = b'X';
        assert!(matches!(Header::decode(&page), Err(Error::BadHeader)));
        assert!(matches!(
            Header::peek_page_size(&page),
            Err(Error::BadHeader)
        ));
    }

    #[test]
    fn version_1_file_decodes_with_no_fts_index() {
        // A format_version 1 file had offset 156 reserved-and-zero. Simulate
        // one and confirm it decodes cleanly with `fts_root_page == 0` — the
        // degradation path that lets a v2 build read pre-full-text files
        // (docs/adr/0011).
        let mut h = sample_header();
        h.format_version = 1;
        h.fts_root_page = 0;
        h.graph_root_page = 0;
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        h.encode(&mut page).unwrap();
        // Bytes at OFF_FTS_ROOT must be zero — nothing a v1 writer would touch.
        assert_eq!(read_u64(&page, OFF_FTS_ROOT), Some(0));
        let back = Header::decode(&page).unwrap();
        assert_eq!(back.format_version, 1);
        assert_eq!(back.fts_root_page, 0);
        assert_eq!(back.graph_root_page, 0);
    }

    #[test]
    fn version_2_file_decodes_with_no_graph() {
        // A format_version 2 file had offset 164 reserved-and-zero. Simulate
        // one and confirm it decodes cleanly with `graph_root_page == 0` — the
        // degradation path that lets a v3 build read pre-graph files
        // (docs/adr/0012).
        let mut h = sample_header();
        h.format_version = 2;
        h.graph_root_page = 0;
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        h.encode(&mut page).unwrap();
        assert_eq!(read_u64(&page, OFF_GRAPH_ROOT), Some(0));
        let back = Header::decode(&page).unwrap();
        assert_eq!(back.format_version, 2);
        assert_eq!(back.fts_root_page, 11, "v2 keeps its full-text index");
        assert_eq!(back.graph_root_page, 0);
    }

    #[test]
    fn header_refuses_future_version_before_checksum() {
        // G4: a future version must be reported as such even if the checksum
        // (whose location a future layout might move) no longer matches.
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        sample_header().encode(&mut page).unwrap();
        write_u32(&mut page, OFF_VERSION, FORMAT_VERSION + 1);
        assert!(matches!(
            Header::decode(&page),
            Err(Error::UnsupportedVersion { found, supported })
                if found == FORMAT_VERSION + 1 && supported == FORMAT_VERSION
        ));
    }

    #[test]
    fn header_detects_corruption() {
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        sample_header().encode(&mut page).unwrap();
        page[100] ^= 0xff;
        assert!(matches!(
            Header::decode(&page),
            Err(Error::CorruptPage { page_no: 0 })
        ));
    }

    #[test]
    fn header_refuses_encrypted_flag() {
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        sample_header().encode(&mut page).unwrap();
        write_u32(&mut page, OFF_FLAGS, FLAG_ENCRYPTED);
        stamp_page_checksum(&mut page);
        assert!(matches!(Header::decode(&page), Err(Error::Encrypted)));
    }

    #[test]
    fn header_decode_never_panics_on_arbitrary_bytes() {
        // Cheap fuzz-shaped smoke test; the real fuzz_header target follows
        // (docs/TESTING.md §3). Seeded, deterministic.
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 8192) as usize;
            let mut buf = vec![0u8; len];
            for b in &mut buf {
                *b = next() as u8;
            }
            let _ = Header::decode(&buf); // must return, never panic
            let _ = Header::peek_page_size(&buf);
            let _ = WalHeader::decode(&buf);
            let _ = WalFrameHeader::decode(&buf, &buf, next());
        }
    }

    #[test]
    fn page_header_roundtrip_and_rejects() {
        let h = PageHeader {
            page_type: PageType::BtreeLeaf,
            entry_count: 17,
            next_page: 99,
        };
        let mut page = vec![0u8; 64];
        h.encode_into(&mut page);
        assert_eq!(PageHeader::decode(&page), Some(h));
        page[0] = 0xEE; // unknown page type
        assert_eq!(PageHeader::decode(&page), None);
        assert_eq!(PageHeader::decode(&[0u8; 4]), None); // short buffer
    }

    #[test]
    fn page_checksum_roundtrip() {
        let mut page = vec![7u8; 1024];
        stamp_page_checksum(&mut page);
        assert!(page_checksum_is_valid(&page));
        page[3] ^= 1;
        assert!(!page_checksum_is_valid(&page));
    }

    #[test]
    fn wal_header_roundtrip() {
        let h = WalHeader {
            format_version: 1,
            page_size: 4096,
            salt: 0xDEADBEEF,
        };
        assert_eq!(WalHeader::decode(&h.encode()), Some(h));
        assert_eq!(WalHeader::decode(b"NOTAWAL0................"), None);
    }

    #[test]
    fn wal_frame_roundtrip_and_salt_binding() {
        let image = vec![9u8; 4096];
        let fh = WalFrameHeader {
            page_no: 5,
            txn_id: 8,
            commit: true,
        };
        let enc = fh.encode(&image, 111);
        assert_eq!(WalFrameHeader::decode(&enc, &image, 111), Some(fh));
        // Wrong salt (stale generation) must invalidate the frame.
        assert_eq!(WalFrameHeader::decode(&enc, &image, 112), None);
        // Corrupt image must invalidate the frame.
        let mut bad = image.clone();
        bad[0] ^= 1;
        assert_eq!(WalFrameHeader::decode(&enc, &bad, 111), None);
    }

    // -----------------------------------------------------------------
    // Vector pages
    // -----------------------------------------------------------------

    #[test]
    fn vector_page_push_get_roundtrip_and_fills() {
        const DIMS: u16 = 384;
        let page_size = DEFAULT_PAGE_SIZE;
        let mut page = vec![0u8; page_size as usize];
        init_vector_page(&mut page);
        let cap = vector_slots_per_page(page_size, DIMS);
        assert!(cap > 0);

        let v0: Vec<f32> = (0..DIMS).map(|i| i as f32 * 0.001).collect();
        let slot0 = vector_page_push(&mut page, DIMS, &v0).unwrap().unwrap();
        assert_eq!(slot0, 0);
        assert_eq!(vector_page_get(&page, DIMS, slot0, 1).unwrap(), v0);

        let v1: Vec<f32> = (0..DIMS).map(|i| -(i as f32)).collect();
        let slot1 = vector_page_push(&mut page, DIMS, &v1).unwrap().unwrap();
        assert_eq!(slot1, 1);
        assert_eq!(vector_page_get(&page, DIMS, slot1, 1).unwrap(), v1);
        // First vector still intact after the second push.
        assert_eq!(vector_page_get(&page, DIMS, slot0, 1).unwrap(), v0);

        // Fill to capacity, then the next push reports "full" (None), not an error.
        let mut fresh = vec![0u8; page_size as usize];
        init_vector_page(&mut fresh);
        let filler = vec![1.0f32; DIMS as usize];
        for _ in 0..cap {
            assert!(
                vector_page_push(&mut fresh, DIMS, &filler)
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(vector_page_push(&mut fresh, DIMS, &filler).unwrap(), None);
    }

    #[test]
    fn vector_page_rejects_wrong_dims_and_bad_page() {
        const DIMS: u16 = 8;
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        init_vector_page(&mut page);
        assert!(matches!(
            vector_page_push(&mut page, DIMS, &[0.0; 4]),
            Err(Error::InvalidArgument(_))
        ));
        let mut not_vector = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        PageHeader {
            page_type: PageType::BtreeLeaf,
            entry_count: 0,
            next_page: 0,
        }
        .encode_into(&mut not_vector);
        assert!(matches!(
            vector_page_push(&mut not_vector, DIMS, &[0.0; 8]),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            vector_page_get(&not_vector, DIMS, 0, 9),
            Err(Error::MalformedPage { page_no: 9, .. })
        ));
    }

    // -----------------------------------------------------------------
    // HNSW pages
    // -----------------------------------------------------------------

    #[test]
    fn hnsw_meta_roundtrip() {
        let meta = HnswMeta {
            m: 16,
            ef_construction: 200,
            max_layer: 3,
            entry_point_page: 42,
            node_count: 100_000,
        };
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        meta.encode(&mut page).unwrap();
        assert_eq!(HnswMeta::decode(&page, 7).unwrap(), meta);
    }

    #[test]
    fn hnsw_meta_empty_roundtrip() {
        let meta = HnswMeta::new();
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        meta.encode(&mut page).unwrap();
        assert_eq!(HnswMeta::decode(&page, 0).unwrap(), meta);
    }

    #[test]
    fn hnsw_meta_rejects_inconsistent_state() {
        // node_count > 0 without an entry point: invalid to encode…
        let mut meta = HnswMeta::new();
        meta.node_count = 5;
        let mut page = vec![0u8; DEFAULT_PAGE_SIZE as usize];
        assert!(matches!(
            meta.encode(&mut page),
            Err(Error::InvalidArgument(_))
        ));

        // …and malformed to decode (tampered on-disk bytes).
        let valid = HnswMeta {
            m: 16,
            ef_construction: 200,
            max_layer: 0,
            entry_point_page: 9,
            node_count: 1,
        };
        valid.encode(&mut page).unwrap();
        write_u64(&mut page, PAGE_HEADER_LEN + 5, 0); // zero the entry_point_page
        stamp_page_checksum(&mut page);
        assert!(matches!(
            HnswMeta::decode(&page, 3),
            Err(Error::MalformedPage { page_no: 3, .. })
        ));
    }

    #[test]
    fn hnsw_node_roundtrip_multi_layer() {
        let node = HnswNode {
            record_id: ulid::Ulid::from_parts(1_700_000_000_000, 7),
            vec_page: 5,
            vec_slot: 3,
            layers: vec![vec![1, 2, 3, 4], vec![5, 6], vec![7]],
        };
        let page = node.encode(DEFAULT_PAGE_SIZE).unwrap();
        assert_eq!(HnswNode::decode(&page, 1).unwrap(), node);
    }

    #[test]
    fn hnsw_node_roundtrip_no_neighbors() {
        let node = HnswNode {
            record_id: ulid::Ulid::from_parts(0, 0),
            vec_page: 1,
            vec_slot: 0,
            layers: vec![vec![]],
        };
        let page = node.encode(DEFAULT_PAGE_SIZE).unwrap();
        assert_eq!(HnswNode::decode(&page, 1).unwrap(), node);
    }

    #[test]
    fn hnsw_node_rejects_when_too_large_for_page() {
        let node = HnswNode {
            record_id: ulid::Ulid::from_parts(0, 0),
            vec_page: 1,
            vec_slot: 0,
            layers: vec![(1..=2000u64).collect()],
        };
        assert_eq!(node.encode(MIN_PAGE_SIZE), None);
    }

    #[test]
    fn max_hnsw_level_guarantees_full_nodes_fit() {
        for page_size in [MIN_PAGE_SIZE, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE] {
            for m in [4u16, 16, 48] {
                let Some(level) = max_hnsw_level(page_size, m) else {
                    // Combination cannot host an index (e.g. M=48 at 512 B):
                    // reported, not guessed at.
                    assert_eq!((page_size, m), (MIN_PAGE_SIZE, 48));
                    continue;
                };
                // Build a node with every layer at its cap; it must encode.
                let mut layers: Vec<Vec<u64>> = vec![(1..=u64::from(m) * 2).collect()];
                for _ in 0..level {
                    layers.push((1..=u64::from(m)).collect());
                }
                let node = HnswNode {
                    record_id: ulid::Ulid::from_parts(1, 1),
                    vec_page: 1,
                    vec_slot: 0,
                    layers,
                };
                assert!(
                    node.encode(page_size).is_some(),
                    "full node at page_size {page_size}, m {m}, level {level} must fit"
                );
            }
        }
        // Default configuration leaves ample headroom (levels beyond ~10 are
        // astronomically unlikely with mL = 1/ln(16)).
        assert!(max_hnsw_level(DEFAULT_PAGE_SIZE, HNSW_DEFAULT_M).unwrap() >= 16);
    }

    #[test]
    fn hnsw_page_decode_never_panics_on_arbitrary_bytes() {
        let mut state = 0xA5A5A5A5A5A5A5A5u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = [64usize, 512, 4096][(next() % 3) as usize];
            let mut buf = vec![0u8; len];
            for b in &mut buf {
                *b = next() as u8;
            }
            let _ = HnswMeta::decode(&buf, 1);
            let _ = HnswNode::decode(&buf, 1);
            let _ = vector_page_get(&buf, 384, (next() % 8) as u16, 1);
        }
    }
}
