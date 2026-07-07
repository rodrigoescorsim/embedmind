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
pub const FORMAT_VERSION: u32 = 1;

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

/// Header `flags` bit 0: file is encrypted (premium; reserved, must be 0 in v1).
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
}

/// Returns `true` if `page_size` is a supported value (power of two within
/// [`MIN_PAGE_SIZE`], [`MAX_PAGE_SIZE`]).
pub fn page_size_is_valid(page_size: u32) -> bool {
    (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) && page_size.is_power_of_two()
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
}
