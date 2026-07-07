//! Binary layout of the `.mind` file: constants, header, page framing,
//! checksums. Normative spec: `docs/FORMAT.md` — this module implements it and
//! must never drift from it. Everything here is explicitly (de)serialized,
//! little-endian, and fuzzable; no struct is ever written to disk as raw memory.

/// Magic bytes at offset 0 of every `.mind` file (`docs/FORMAT.md` §4).
pub const MAGIC: [u8; 8] = *b"MINDFMT1";

/// Magic bytes at offset 0 of the WAL sidecar (`docs/FORMAT.md` §8).
pub const WAL_MAGIC: [u8; 8] = *b"MINDWAL1";

/// Current on-disk format version written by this build.
pub const FORMAT_VERSION: u32 = 1;

/// Default page size in bytes. The authoritative value for an existing file is
/// the one recorded in its header.
pub const DEFAULT_PAGE_SIZE: u32 = 4096;

/// Size of the per-page checksum trailer (xxh3_64), in bytes.
pub const PAGE_TRAILER_LEN: usize = 8;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_values_match_spec() {
        assert_eq!(&MAGIC, b"MINDFMT1");
        assert_eq!(&WAL_MAGIC, b"MINDWAL1");
    }
}
