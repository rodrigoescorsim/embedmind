//! Paged inverted full-text index with BM25 scoring (`docs/adr/0011`,
//! `docs/FORMAT.md` §11) — the engine half of story S9 (roadmap item 2.3).
//!
//! Everything lives in the `.mind` file's own pages and every mutation goes
//! through a [`Txn`], so the index is durable and crash-safe on exactly the
//! same terms as the record B-tree and the HNSW graph: touched pages enter
//! the WAL, recovery replays them, no separate index journal. This is the
//! reason ADR 0011 rejects embedding tantivy — an external segment store
//! would sit outside the single file and outside the WAL, breaking both
//! product promises ("one file", "never corrupts").
//!
//! ## Structure
//!
//! - **`fts_root_page`** (header) points at one fixed-size **meta page**:
//!   corpus statistics for BM25 (`doc_count`, `total_tokens`) plus the root
//!   page of the dictionary. Fixed size forever, like `HNSW_META`.
//! - The **dictionary** is the shared byte-keyed paged B-tree
//!   ([`crate::index::dict`], also used by the graph layer — ADR 0012),
//!   instantiated with the `FtsDict`/`FtsPostings` page types and keyed by
//!   term bytes. Its leaf values hold the term's **postings**: `doc_freq`
//!   then a list of `(record_id, term_freq)` sorted by id. A postings list
//!   too large to inline spills to an `FTS_POSTINGS` overflow chain, exactly
//!   like an oversized record spills to `OVERFLOW`.
//! - Meta / inner / leaf dictionary nodes share the one `FtsDict` page type,
//!   told apart by a node-kind byte at the start of the page body — so the
//!   index adds only two page types (`docs/FORMAT.md` §3.1).
//!
//! ## Scoring
//!
//! BM25 (`k1 = 1.2`, `b = 0.75`) over the postings. `N` and `avgdl` come from
//! the meta page; a document's length `|D|` is **recomputed by tokenizing its
//! content at query time** rather than persisted per document — recall already
//! reads each candidate record to re-check tombstone/scope (see `api.rs`), so
//! the token count is free there and one fewer thing can drift on disk. The
//! caller supplies a `doc_len` closure so this module never reads records
//! itself (layering: `index` sits below `api`).
//!
//! ## Deletion
//!
//! There is no delete, matching the rest of the engine: `forget` is a
//! tombstone (`docs/adr/0003`) and postings for tombstoned/rescoped records
//! are filtered by the caller's `keep` closure at query time, then physically
//! reclaimed by `embedmind vacuum` (which rebuilds this index like it rebuilds
//! the HNSW graph). Postings are therefore append-/update-only.

use std::collections::HashMap;
use std::time::Instant;

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::format::{PAGE_HEADER_LEN, PageHeader, PageType, stamp_page_checksum};
use crate::index::dict;
use crate::storage::btree::PageSource;
use crate::storage::pager::Txn;

/// BM25 term-frequency saturation parameter (standard default).
const BM25_K1: f32 = 1.2;
/// BM25 length-normalization parameter (standard default).
const BM25_B: f32 = 0.75;

/// Longest indexed term, in bytes. Longer tokens are truncated on the byte
/// boundary nearest below the cap (kept valid UTF-8) — a defensive bound so a
/// pathological token can never blow a dictionary cell past a page. Ordinary
/// words are far shorter; this only clips hostile input.
const MAX_TERM_LEN: usize = 128;

/// The full-text dictionary instance: `FtsDict` nodes, `FtsPostings`
/// overflow, keys bounded by [`MAX_TERM_LEN`] (`docs/FORMAT.md` §11).
const FTS_DICT: dict::DictSpec = dict::DictSpec {
    dict: PageType::FtsDict,
    overflow: PageType::FtsPostings,
    max_key_len: MAX_TERM_LEN,
};

/// Bytes per posting entry on disk in the fixed-width layout: `record_id`
/// (16) + `term_freq` (u32).
const POSTING_LEN: usize = 20;

/// First `format_version` whose postings bodies use the delta+varint layout
/// (S26, `docs/adr/0021`, `docs/FORMAT.md` §11). Older files keep the
/// fixed-width layout for both reads and writes, so a file never mixes
/// layouts and stays readable by the build that wrote it.
const DELTA_VARINT_MIN_FORMAT_VERSION: u32 = 4;

/// First `format_version` whose postings bodies carry a skip index when large
/// (S26 part 2, `docs/adr/0022`, `docs/FORMAT.md` §11).
const SKIP_MIN_FORMAT_VERSION: u32 = 5;

/// First `format_version` whose skip entries carry the per-block `last_id`
/// (block max doc id) alongside `max_term_freq` — the `(block_max_docid,
/// block_max_impact)` pair BlockMax-WAND skips a block by (BMW-1,
/// `docs/adr/0024`, `docs/FORMAT.md` §11).
const SKIP_BOUND_MIN_FORMAT_VERSION: u32 = 6;

/// Postings per skip block. A lookup by id decodes at most one block of this
/// many entries instead of the whole list. Picked so the per-block skip-entry
/// overhead (24 bytes: 16-byte `first_id` + `u32` offset + `u32` max_tf) stays
/// a small fraction of a full block (`SKIP_BLOCK_SIZE` × ~2–14 delta+varint
/// bytes) and a block is a few hundred bytes — coarse enough to skip cheaply,
/// fine enough that decoding one is far less work than the list. Justified by
/// measurement in `docs/adr/0022`.
const SKIP_BLOCK_SIZE: usize = 128;

/// A postings list shorter than this keeps the plain (skip-less) delta+varint
/// body: below it the skip index costs more bytes and branches than the linear
/// scan it would save. Set to 4 × [`SKIP_BLOCK_SIZE`] so a skip index only
/// appears once there are at least a few blocks to skip over. Threshold chosen
/// by measurement on the test corpus (`docs/adr/0022`).
const SKIP_MIN_DOC_FREQ: usize = 4 * SKIP_BLOCK_SIZE;

/// Bytes per skip-index entry in the version-5 layout: `first_id` (16) ·
/// `byte_offset` (u32) · `max_term_freq` (u32).
const SKIP_ENTRY_LEN_V5: usize = 24;

/// Bytes per skip-index entry in the version-6 layout: `first_id` (16) ·
/// `last_id` (16) · `byte_offset` (u32) · `max_term_freq` (u32). The extra
/// `last_id` (block max doc id) is the BlockMax-WAND block-skip key (BMW-1,
/// `docs/adr/0024`).
const SKIP_ENTRY_LEN_V6: usize = 40;

/// Longest legal LEB128 varint for a u128: ⌈128 / 7⌉ bytes. A longer run of
/// continuation bits is malformed, which also bounds the decode loop.
const MAX_VARINT_LEN: usize = 19;

/// On-disk encoding of a postings body — selected by the *file's*
/// `format_version` (`docs/FORMAT.md` §11), never stored per body: every body
/// in one file uses one layout, and `vacuum`'s rebuild-by-copy re-encodes an
/// old file into the current layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostingsLayout {
    /// `format_version` ≤ 3: `doc_freq` (u32) then fixed 20-byte entries of
    /// `record_id` (16 raw bytes) · `term_freq` (u32).
    FixedWidth,
    /// `format_version` 4: `doc_freq` (u32) then per entry the varint
    /// **delta** of `record_id` from the previous entry (the list is sorted
    /// strictly ascending, so deltas after the first are ≥ 1; the first is
    /// the id's raw u128 value) followed by `term_freq` as a varint.
    DeltaVarint,
    /// `format_version` ≥ 5: same delta+varint entries, but a list with at
    /// least [`SKIP_MIN_DOC_FREQ`] entries is prefixed by a **skip index** —
    /// `block_count` (u32) then, per block, a [`SkipEntry`] followed by the
    /// blocks region. Each block re-bases its delta chain (the block's first
    /// entry's delta is its absolute id) so a block decodes on its own. A
    /// shorter list writes `block_count = 0` and the plain delta+varint body,
    /// identical to [`PostingsLayout::DeltaVarint`]'s bytes after the count.
    /// The [`SkipEntry`] width/fields depend on the version (v5 vs v6).
    DeltaVarintSkip(SkipEntry),
}

/// The per-block skip-entry shape, selected by the file's `format_version`.
/// v5 and v6 share every block/delta code path and differ only here: v6 adds
/// the block's `last_id` (block max doc id) for BlockMax-WAND (BMW-1,
/// `docs/adr/0024`, `docs/FORMAT.md` §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipEntry {
    /// `format_version` 5: `first_id` (u128 LE) · `byte_offset` (u32) ·
    /// `max_term_freq` (u32) — [`SKIP_ENTRY_LEN_V5`] bytes.
    V5,
    /// `format_version` ≥ 6: `first_id` (u128 LE) · `last_id` (u128 LE) ·
    /// `byte_offset` (u32) · `max_term_freq` (u32) — [`SKIP_ENTRY_LEN_V6`]
    /// bytes. The extra `last_id` is the block max doc id BMW skips a block by.
    V6,
}

impl SkipEntry {
    /// Bytes this entry occupies on disk.
    fn len(self) -> usize {
        match self {
            SkipEntry::V5 => SKIP_ENTRY_LEN_V5,
            SkipEntry::V6 => SKIP_ENTRY_LEN_V6,
        }
    }

    /// Byte offset of the `byte_offset`/`max_term_freq` fields within one entry
    /// (after `first_id`, and after `last_id` too when present).
    fn tail_off(self) -> usize {
        match self {
            SkipEntry::V5 => 16,
            SkipEntry::V6 => 32,
        }
    }
}

impl PostingsLayout {
    fn for_format_version(version: u32) -> Self {
        if version >= SKIP_BOUND_MIN_FORMAT_VERSION {
            PostingsLayout::DeltaVarintSkip(SkipEntry::V6)
        } else if version >= SKIP_MIN_FORMAT_VERSION {
            PostingsLayout::DeltaVarintSkip(SkipEntry::V5)
        } else if version >= DELTA_VARINT_MIN_FORMAT_VERSION {
            PostingsLayout::DeltaVarint
        } else {
            PostingsLayout::FixedWidth
        }
    }
}

/// Appends `v` as an LEB128 varint (7 data bits per byte, low bits first,
/// high bit = continuation). Minimal-length by construction, so the encoding
/// is deterministic (G3).
fn put_varint(out: &mut Vec<u8>, mut v: u128) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads one LEB128 varint at `*off`, advancing it. Rejects truncation, more
/// than [`MAX_VARINT_LEN`] bytes, and data bits shifted past 128 — so a
/// hostile body can neither loop forever nor silently wrap a value.
fn read_varint(body: &[u8], off: &mut usize, page_no: u64) -> Result<u128> {
    let mut value: u128 = 0;
    for i in 0..MAX_VARINT_LEN {
        let byte = *body
            .get(*off + i)
            .ok_or_else(|| malformed(page_no, "fts varint truncated"))?;
        let bits = u128::from(byte & 0x7F);
        let shift = 7 * i as u32;
        if bits != 0 && (bits << shift) >> shift != bits {
            return Err(malformed(page_no, "fts varint overflow"));
        }
        value |= bits << shift;
        if byte & 0x80 == 0 {
            *off += i + 1;
            return Ok(value);
        }
    }
    Err(malformed(page_no, "fts varint too long"))
}

fn malformed(page_no: u64, what: &'static str) -> Error {
    Error::MalformedPage { page_no, what }
}

/// Reads a little-endian u128 at `off`. Bounds-checked (fuzz rule).
fn read_u128_le(body: &[u8], off: usize, page_no: u64) -> Result<u128> {
    let bytes: [u8; 16] = body
        .get(off..off + 16)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| malformed(page_no, "fts u128 truncated"))?;
    Ok(u128::from_le_bytes(bytes))
}

/// Appends a delta+varint run of `entries`, re-based at `prev = 0` (the first
/// entry's delta is its absolute id). Used both for a whole plain body and for
/// one self-contained skip block.
fn encode_delta_run(out: &mut Vec<u8>, entries: &[Posting]) {
    let mut prev: u128 = 0;
    for p in entries {
        let id = u128::from(p.record_id);
        // The list is strictly ascending, so this never wraps and deltas after
        // the first entry are always ≥ 1.
        put_varint(out, id.wrapping_sub(prev));
        put_varint(out, u128::from(p.term_freq));
        prev = id;
    }
}

/// Rejects a `count` a hostile body cannot back: every delta+varint entry is at
/// least two bytes (one-byte delta + one-byte term_freq), so we bound the count
/// against the buffer *before* allocating a `Vec` of it (fuzz rule,
/// `docs/TESTING.md` §3). `body` starts at the 4-byte count prefix.
fn reject_hostile_delta_count(body: &[u8], count: usize, page_no: u64) -> Result<()> {
    let min_need = 4usize
        .checked_add(
            count
                .checked_mul(2)
                .ok_or_else(|| malformed(page_no, "fts postings count overflow"))?,
        )
        .ok_or_else(|| malformed(page_no, "fts postings length overflow"))?;
    if body.len() < min_need {
        return Err(malformed(page_no, "fts postings truncated"));
    }
    Ok(())
}

/// Decodes `count` delta+varint entries starting at `*off` (re-based: the first
/// entry's delta is its absolute id), appending to `entries` and advancing
/// `*off`. Enforces the strict-ascending order *within* this run; a caller that
/// splits the list into blocks checks the order across block seams itself.
fn decode_delta_run(
    body: &[u8],
    off: &mut usize,
    count: usize,
    page_no: u64,
    entries: &mut Vec<Posting>,
) -> Result<()> {
    let mut prev: u128 = 0;
    for i in 0..count {
        let delta = read_varint(body, off, page_no)?;
        if i > 0 && delta == 0 {
            return Err(malformed(page_no, "unsorted fts postings"));
        }
        let id = if i == 0 {
            delta
        } else {
            prev.checked_add(delta)
                .ok_or_else(|| malformed(page_no, "fts posting id overflow"))?
        };
        prev = id;
        let term_freq = u32::try_from(read_varint(body, off, page_no)?)
            .map_err(|_| malformed(page_no, "fts posting term_freq overflow"))?;
        if term_freq == 0 {
            return Err(malformed(page_no, "fts posting zero term_freq"));
        }
        entries.push(Posting {
            record_id: Ulid::from(id),
            term_freq,
        });
    }
    Ok(())
}

/// Finds `target`'s `term_freq` in a version-5/6 skip body **without decoding
/// the whole list**: binary-searches the skip index for the one block whose id
/// range can contain `target`, then decodes just that block (≤
/// [`SKIP_BLOCK_SIZE`] entries). Returns `None` when the term does not cover
/// `target`. This is the block-skipping lookup the skip index exists for; it is
/// verified against the linear `binary_search` over a fully decoded list by the
/// equivalence tests, so wiring it into the hot path never changes a result.
///
/// `entry` selects the skip-entry width (v5 vs v6) so the offsets into the skip
/// index match the file that wrote it. A body without a skip index
/// (`block_count = 0`, a small term) falls back to a full decode + search — no
/// skip is possible or worthwhile there.
fn lookup_via_skip(
    body: &[u8],
    page_no: u64,
    target: Ulid,
    entry: SkipEntry,
) -> Result<Option<u32>> {
    let count = dict::read_u32(body, 0, page_no)? as usize;
    let block_count = dict::read_u32(body, 4, page_no)? as usize;
    if block_count == 0 {
        let decoded = Postings::decode_delta_varint_skip(body, page_no, entry)?;
        return Ok(decoded
            .entries
            .binary_search_by(|p| p.record_id.cmp(&target))
            .ok()
            .map(|i| decoded.entries[i].term_freq));
    }
    // Same invariant the decoder enforces (`decode_delta_varint_skip`): a skip
    // index only exists when `block_count` matches `count` at the fixed block
    // size. Without this check a hostile `count`/`block_count` pair lets the
    // last-block-length arithmetic below underflow.
    let expected_blocks = count.div_ceil(SKIP_BLOCK_SIZE);
    if count < SKIP_MIN_DOC_FREQ || block_count != expected_blocks {
        return Err(malformed(page_no, "fts skip block_count mismatch"));
    }
    let entry_len = entry.len();
    let index_len = block_count
        .checked_mul(entry_len)
        .ok_or_else(|| malformed(page_no, "fts skip index overflow"))?;
    let blocks_start = 8usize
        .checked_add(index_len)
        .ok_or_else(|| malformed(page_no, "fts skip index overflow"))?;
    if body.len() < blocks_start {
        return Err(malformed(page_no, "fts skip index truncated"));
    }
    let target_id = u128::from(target);

    // The block that may hold `target` is the last one whose `first_id` is
    // ≤ target (blocks are id-ordered and contiguous). `partition_point`
    // counts blocks starting at or before target; the candidate is the one
    // before that boundary. None before it → target precedes the whole list.
    let first_id_of = |b: usize| -> Result<u128> { read_u128_le(body, 8 + b * entry_len, page_no) };
    let mut lo = 0usize;
    let mut hi = block_count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if first_id_of(mid)? <= target_id {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 {
        return Ok(None); // target is below the first block's first id
    }
    let b = lo - 1;
    let stored_offset =
        dict::read_u32(body, 8 + b * entry_len + entry.tail_off(), page_no)? as usize;
    let block_len = if b + 1 == block_count {
        count - b * SKIP_BLOCK_SIZE
    } else {
        SKIP_BLOCK_SIZE
    };
    let mut block = Vec::with_capacity(block_len);
    let mut off = blocks_start
        .checked_add(stored_offset)
        .ok_or_else(|| malformed(page_no, "fts skip byte_offset overflow"))?;
    decode_delta_run(body, &mut off, block_len, page_no, &mut block)?;
    Ok(block
        .binary_search_by(|p| p.record_id.cmp(&target))
        .ok()
        .map(|i| block[i].term_freq))
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/// Splits `text` into lowercased alphanumeric tokens. Unicode-aware
/// (`char::is_alphanumeric`), so accented Portuguese words tokenize whole
/// ("memória" stays one token) — important for the founder's dogfooding
/// (DESIGN §6). No stemming or stopword removal in M2: both are lossy and
/// language-specific, and BM25's IDF already down-weights common words.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Token count of `text` under [`tokenize`] — the document length BM25 needs.
/// Kept separate so the recall path can compute `|D|` without allocating the
/// token vector.
pub fn doc_len(text: &str) -> u32 {
    let mut count = 0u32;
    let mut in_token = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            in_token = true;
        } else if in_token {
            count = count.saturating_add(1);
            in_token = false;
        }
    }
    if in_token {
        count = count.saturating_add(1);
    }
    count
}

/// Truncates a term to [`MAX_TERM_LEN`] bytes without splitting a UTF-8 char.
fn clip_term(term: &str) -> &str {
    if term.len() <= MAX_TERM_LEN {
        return term;
    }
    let mut end = MAX_TERM_LEN;
    while end > 0 && !term.is_char_boundary(end) {
        end -= 1;
    }
    &term[..end]
}

// ---------------------------------------------------------------------------
// Meta page (fixed size)
// ---------------------------------------------------------------------------

/// Corpus statistics for BM25 plus the dictionary root, in one fixed-size
/// page (`docs/FORMAT.md` §11). Reached through the header's `fts_root_page`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FtsMeta {
    /// Number of documents indexed (each `index_document` call adds one).
    doc_count: u64,
    /// Sum of every indexed document's token length — `avgdl = total_tokens /
    /// doc_count`.
    total_tokens: u64,
    /// Root page of the dictionary B-tree; 0 = empty dictionary.
    dict_root: u64,
}

impl FtsMeta {
    fn empty() -> Self {
        FtsMeta {
            doc_count: 0,
            total_tokens: 0,
            dict_root: 0,
        }
    }

    fn encode(&self, page_size: u32) -> Result<Vec<u8>> {
        let mut page = vec![0u8; page_size as usize];
        PageHeader {
            page_type: PageType::FtsDict,
            entry_count: 0,
            next_page: 0,
        }
        .encode_into(&mut page);
        let mut off = PAGE_HEADER_LEN;
        page[off] = dict::NODE_META;
        off += 1;
        dict::put_u64(&mut page, &mut off, self.doc_count);
        dict::put_u64(&mut page, &mut off, self.total_tokens);
        dict::put_u64(&mut page, &mut off, self.dict_root);
        stamp_page_checksum(&mut page);
        Ok(page)
    }

    fn decode(page: &[u8], page_no: u64) -> Result<Self> {
        let header =
            PageHeader::decode(page).ok_or_else(|| malformed(page_no, "fts page header"))?;
        if header.page_type != PageType::FtsDict {
            return Err(malformed(page_no, "not an FTS page"));
        }
        let mut off = PAGE_HEADER_LEN;
        if page.get(off).copied() != Some(dict::NODE_META) {
            return Err(malformed(page_no, "not an FTS meta page"));
        }
        off += 1;
        let doc_count = dict::get_u64(page, &mut off, page_no)?;
        let total_tokens = dict::get_u64(page, &mut off, page_no)?;
        let dict_root = dict::get_u64(page, &mut off, page_no)?;
        Ok(FtsMeta {
            doc_count,
            total_tokens,
            dict_root,
        })
    }
}

/// Loads the meta page, or `None` when no index exists yet (`root == 0`).
fn load_meta(src: &dyn PageSource, root: u64) -> Result<Option<FtsMeta>> {
    if root == 0 {
        return Ok(None);
    }
    let page = src.page(root)?;
    Ok(Some(FtsMeta::decode(&page, root)?))
}

/// Persists `meta`, allocating the meta page on first use; moves the txn's
/// `fts_root_page` pointer so the change is durable with the commit frame.
fn save_meta(txn: &mut Txn<'_>, meta: &FtsMeta) -> Result<()> {
    let page_no = match txn.fts_root_page() {
        0 => txn.allocate_page()?,
        p => p,
    };
    let page = meta.encode(txn.page_size())?;
    txn.write_page(page_no, &page)?;
    txn.set_fts_root_page(page_no);
    Ok(())
}

// ---------------------------------------------------------------------------
// Postings
// ---------------------------------------------------------------------------

/// One posting: a document and how often the term occurs in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Posting {
    record_id: Ulid,
    term_freq: u32,
}

/// A term's full postings list, sorted by `record_id` (so updates merge in
/// O(log n) and the encoding is deterministic across platforms — G3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Postings {
    entries: Vec<Posting>,
}

impl Postings {
    /// Inserts or updates the posting for `record_id`, keeping the list sorted.
    fn upsert(&mut self, record_id: Ulid, term_freq: u32) {
        match self
            .entries
            .binary_search_by(|p| p.record_id.cmp(&record_id))
        {
            Ok(i) => self.entries[i].term_freq = term_freq,
            Err(i) => self.entries.insert(
                i,
                Posting {
                    record_id,
                    term_freq,
                },
            ),
        }
    }

    /// Serialized body: `doc_freq` (u32) + `doc_freq` × posting entries in
    /// `layout`'s encoding (`docs/FORMAT.md` §11).
    fn encode(&self, layout: PostingsLayout) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.entries.len() * POSTING_LEN);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        match layout {
            PostingsLayout::FixedWidth => {
                for p in &self.entries {
                    out.extend_from_slice(&p.record_id.to_bytes());
                    out.extend_from_slice(&p.term_freq.to_le_bytes());
                }
            }
            PostingsLayout::DeltaVarint => {
                encode_delta_run(&mut out, &self.entries);
            }
            PostingsLayout::DeltaVarintSkip(entry) => {
                self.encode_skip(&mut out, entry);
            }
        }
        out
    }

    /// Appends the skip-index body for the version-5/6 layout after the already
    /// written `doc_freq`. A list shorter than [`SKIP_MIN_DOC_FREQ`] writes
    /// `block_count = 0` and a plain delta+varint run — byte-identical to the
    /// version-4 body past the count — so small terms pay only 4 extra bytes.
    /// `entry` selects the skip-entry width: v6 additionally records each
    /// block's `last_id` (block max doc id) for BlockMax-WAND (BMW-1).
    fn encode_skip(&self, out: &mut Vec<u8>, entry: SkipEntry) {
        if self.entries.len() < SKIP_MIN_DOC_FREQ {
            out.extend_from_slice(&0u32.to_le_bytes());
            encode_delta_run(out, &self.entries);
            return;
        }
        let blocks: Vec<&[Posting]> = self.entries.chunks(SKIP_BLOCK_SIZE).collect();
        out.extend_from_slice(&(blocks.len() as u32).to_le_bytes());

        // Encode each block's entries first (into a scratch buffer) so we know
        // its byte offset and its max term_freq before writing the skip index.
        let mut blocks_body: Vec<u8> = Vec::new();
        let mut skip_index: Vec<u8> = Vec::with_capacity(blocks.len() * entry.len());
        for block in &blocks {
            let first_id = u128::from(block[0].record_id);
            let byte_offset = blocks_body.len() as u32;
            let max_tf = block.iter().map(|p| p.term_freq).max().unwrap_or(0);
            skip_index.extend_from_slice(&first_id.to_le_bytes());
            if entry == SkipEntry::V6 {
                // Block max doc id — the BMW block-skip key. The list is sorted
                // ascending, so the block's last entry holds it.
                let last_id = u128::from(block[block.len() - 1].record_id);
                skip_index.extend_from_slice(&last_id.to_le_bytes());
            }
            skip_index.extend_from_slice(&byte_offset.to_le_bytes());
            skip_index.extend_from_slice(&max_tf.to_le_bytes());
            // Each block re-bases (`prev = 0`), so its first delta is the
            // absolute id and the block decodes without the preceding ones.
            encode_delta_run(&mut blocks_body, block);
        }
        out.extend_from_slice(&skip_index);
        out.extend_from_slice(&blocks_body);
    }

    /// Parses a postings body in `layout`'s encoding. Validates the count
    /// against the buffer before allocating (fuzz rule, `docs/TESTING.md` §3)
    /// and rejects unsorted or duplicate ids (a corrupt or hostile page).
    fn decode(body: &[u8], page_no: u64, layout: PostingsLayout) -> Result<Self> {
        match layout {
            PostingsLayout::FixedWidth => Self::decode_fixed_width(body, page_no),
            PostingsLayout::DeltaVarint => Self::decode_delta_varint(body, page_no),
            PostingsLayout::DeltaVarintSkip(entry) => {
                Self::decode_delta_varint_skip(body, page_no, entry)
            }
        }
    }

    fn decode_fixed_width(body: &[u8], page_no: u64) -> Result<Self> {
        let count = dict::read_u32(body, 0, page_no)? as usize;
        let need = 4usize
            .checked_add(
                count
                    .checked_mul(POSTING_LEN)
                    .ok_or_else(|| malformed(page_no, "fts postings count overflow"))?,
            )
            .ok_or_else(|| malformed(page_no, "fts postings length overflow"))?;
        if body.len() < need {
            return Err(malformed(page_no, "fts postings truncated"));
        }
        let mut entries = Vec::with_capacity(count);
        let mut prev: Option<Ulid> = None;
        let mut off = 4;
        for _ in 0..count {
            let id_bytes: [u8; 16] = body
                .get(off..off + 16)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| malformed(page_no, "fts posting id"))?;
            let record_id = Ulid::from_bytes(id_bytes);
            if prev.is_some_and(|p| p >= record_id) {
                return Err(malformed(page_no, "unsorted fts postings"));
            }
            prev = Some(record_id);
            let term_freq = dict::read_u32(body, off + 16, page_no)?;
            if term_freq == 0 {
                return Err(malformed(page_no, "fts posting zero term_freq"));
            }
            entries.push(Posting {
                record_id,
                term_freq,
            });
            off += POSTING_LEN;
        }
        Ok(Postings { entries })
    }

    fn decode_delta_varint(body: &[u8], page_no: u64) -> Result<Self> {
        let count = dict::read_u32(body, 0, page_no)? as usize;
        reject_hostile_delta_count(body, count, page_no)?;
        let mut entries = Vec::with_capacity(count);
        let mut off = 4;
        decode_delta_run(body, &mut off, count, page_no, &mut entries)?;
        Ok(Postings { entries })
    }

    /// Parses the version-5/6 skip layout: `doc_freq` (u32), `block_count`
    /// (u32), the skip index (`block_count` × [`SkipEntry::len`]), then the
    /// blocks. `block_count = 0` is the plain delta+varint body (a small term)
    /// and is byte-identical across v5 and v6. Every block re-bases its delta
    /// chain, so it decodes independently, and the skip entry's
    /// `first_id`/`byte_offset`/`max_term_freq` (plus `last_id` in v6) are
    /// re-derived from the decoded entries and checked against what was written
    /// — a corrupt index can never point past the body or misreport a block.
    fn decode_delta_varint_skip(body: &[u8], page_no: u64, entry: SkipEntry) -> Result<Self> {
        let count = dict::read_u32(body, 0, page_no)? as usize;
        let block_count = dict::read_u32(body, 4, page_no)? as usize;
        if block_count == 0 {
            reject_hostile_delta_count(body, count, page_no)?;
            let mut entries = Vec::with_capacity(count);
            let mut off = 8;
            decode_delta_run(body, &mut off, count, page_no, &mut entries)?;
            return Ok(Postings { entries });
        }
        // A skip index only appears when the writer chose to (large lists);
        // `block_count` must match the fixed block size and the count, and the
        // index itself must fit before we trust any offset in it.
        let expected_blocks = count.div_ceil(SKIP_BLOCK_SIZE);
        if count < SKIP_MIN_DOC_FREQ || block_count != expected_blocks {
            return Err(malformed(page_no, "fts skip block_count mismatch"));
        }
        let entry_len = entry.len();
        let index_len = block_count
            .checked_mul(entry_len)
            .ok_or_else(|| malformed(page_no, "fts skip index overflow"))?;
        let blocks_start = 8usize
            .checked_add(index_len)
            .ok_or_else(|| malformed(page_no, "fts skip index overflow"))?;
        if body.len() < blocks_start {
            return Err(malformed(page_no, "fts skip index truncated"));
        }
        reject_hostile_delta_count(&body[blocks_start.saturating_sub(4)..], count, page_no)?;

        let mut entries = Vec::with_capacity(count);
        let mut prev_block_last: Option<u128> = None;
        let mut off = blocks_start;
        for b in 0..block_count {
            let block_len = if b + 1 == block_count {
                count - b * SKIP_BLOCK_SIZE
            } else {
                SKIP_BLOCK_SIZE
            };
            // Re-derive the skip entry from the block bytes and check it against
            // the stored index: byte offset, first id, max term_freq, and (v6)
            // last id. `tail_off` skips past `first_id` (and `last_id` in v6).
            let idx_off = 8 + b * entry_len;
            let stored_first = read_u128_le(body, idx_off, page_no)?;
            let stored_last = match entry {
                SkipEntry::V6 => Some(read_u128_le(body, idx_off + 16, page_no)?),
                SkipEntry::V5 => None,
            };
            let tail = idx_off + entry.tail_off();
            let stored_offset = dict::read_u32(body, tail, page_no)? as usize;
            let stored_max_tf = dict::read_u32(body, tail + 4, page_no)?;
            if blocks_start + stored_offset != off {
                return Err(malformed(page_no, "fts skip byte_offset mismatch"));
            }
            let block_start_entry = entries.len();
            decode_delta_run(body, &mut off, block_len, page_no, &mut entries)?;
            let block = &entries[block_start_entry..];
            let first_id = u128::from(block[0].record_id);
            if stored_first != first_id {
                return Err(malformed(page_no, "fts skip first_id mismatch"));
            }
            let last_id = u128::from(block[block.len() - 1].record_id);
            if stored_last.is_some_and(|s| s != last_id) {
                return Err(malformed(page_no, "fts skip last_id mismatch"));
            }
            let max_tf = block.iter().map(|p| p.term_freq).max().unwrap_or(0);
            if stored_max_tf != max_tf {
                return Err(malformed(page_no, "fts skip max_term_freq mismatch"));
            }
            // Blocks re-base independently; enforce the strict global order at
            // the seam between one block's last id and the next block's first.
            if prev_block_last.is_some_and(|prev_last| first_id <= prev_last) {
                return Err(malformed(page_no, "unsorted fts postings"));
            }
            prev_block_last = Some(last_id);
        }
        Ok(Postings { entries })
    }
}

/// Reads a term's postings from the dictionary, or `None` if absent. The
/// body layout follows the file's `format_version`, so an older file keeps
/// decoding with its own (fixed-width) layout.
fn postings_for(src: &dyn PageSource, root: u64, term: &[u8]) -> Result<Option<Postings>> {
    let layout = PostingsLayout::for_format_version(src.format_version());
    match dict::get(src, FTS_DICT, root, term)? {
        Some((body, page_no)) => Ok(Some(Postings::decode(&body, page_no, layout)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Public index / search API
// ---------------------------------------------------------------------------

/// Indexes one document's content under `record_id`: tokenizes, computes each
/// term's frequency, and merges the resulting postings into the dictionary,
/// bumping the corpus statistics. Idempotent per `(term, record_id)` — a
/// re-index overwrites that record's term frequency instead of double-counting
/// — but a fresh `record_id` is assumed (records are immutable except for the
/// tombstone, so content never changes after `remember`). All within `txn`, so
/// it is durable atomically with the record insert (`docs/adr/0011`).
pub fn index_document(txn: &mut Txn<'_>, record_id: Ulid, content: &str) -> Result<()> {
    let tokens = tokenize(content);
    let mut freqs: HashMap<String, u32> = HashMap::new();
    for token in tokens {
        let term = clip_term(&token).to_owned();
        if term.is_empty() {
            continue;
        }
        *freqs.entry(term).or_insert(0) += 1;
    }

    let mut meta = load_meta(txn, txn.fts_root_page())?.unwrap_or_else(FtsMeta::empty);
    let doc_tokens: u64 = freqs.values().map(|&f| u64::from(f)).sum();

    // Deterministic term order keeps the write sequence reproducible (helps
    // crash-test and property-test determinism, DESIGN §9).
    let mut terms: Vec<(String, u32)> = freqs.into_iter().collect();
    terms.sort_by(|a, b| a.0.cmp(&b.0));

    // Writes use the file's own layout (not this build's newest), so an
    // older file stays uniform and readable by the build that created it.
    let layout = PostingsLayout::for_format_version(txn.format_version());
    let mut root = meta.dict_root;
    for (term, tf) in terms {
        let term_bytes = term.as_bytes();
        let mut postings = postings_for(txn, root, term_bytes)?.unwrap_or_default();
        postings.upsert(record_id, tf);
        root = dict::upsert(txn, FTS_DICT, root, term_bytes, &postings.encode(layout))?;
    }

    meta.dict_root = root;
    meta.doc_count = meta.doc_count.saturating_add(1);
    meta.total_tokens = meta.total_tokens.saturating_add(doc_tokens);
    save_meta(txn, &meta)?;
    Ok(())
}

/// One full-text hit: a document and its BM25 score (higher = more relevant).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// The matched memory.
    pub record_id: Ulid,
    /// BM25 relevance score; always > 0 for a returned hit.
    pub score: f32,
}

/// BM25 search for `query` over the full-text index. Returns up to `k` hits,
/// best score first.
///
/// `fts_root_page` is the caller's own view of the pointer (0 = no index yet →
/// empty result, never an error, so a version-1 file degrades cleanly).
/// `keep` filters record ids the caller no longer wants returned (tombstoned
/// or out of scope — the same re-check the vector path does, `docs/adr/0003`,
/// DESIGN §7). `doc_len` yields a candidate's current token length for BM25
/// length normalization; returning `None` drops the candidate (its record is
/// gone). Both closures are called only for candidates that get evaluated
/// exactly, at most once per candidate record (`docs/adr/0018` semantics).
///
/// Scan strategy: on a `format_version` ≥ 6 file — whose skip index carries
/// the per-block `(last_id, max_term_freq)` bounds of BMW-1 — this runs
/// BlockMax-WAND ([`search_bmw_counted`], BMW-2, `docs/adr/0025`), skipping
/// whole postings blocks whose summed impact bounds cannot beat the current
/// top-k. Older files (v4/v5, no per-block bounds) keep the linear two-pass
/// scan of FT2 ([`search_linear`], `docs/adr/0018`) unchanged. Either way the
/// result is identical — same hits, same scores, same order — to the
/// exhaustive scan ([`search_profiled`] keeps that scan as the test oracle);
/// the equivalence tests below compare all three paths directly.
pub fn search(
    src: &dyn PageSource,
    fts_root_page: u64,
    query: &str,
    k: usize,
    keep: impl FnMut(Ulid) -> bool,
    doc_len: impl FnMut(Ulid) -> Result<Option<u32>>,
) -> Result<Vec<Hit>> {
    // BMW navigates by the per-block bounds only the version-6 skip entries
    // carry; a v4/v5 file has no (complete) bounds, so it stays on the linear
    // path — never a silently-wrong result from missing metadata.
    if PostingsLayout::for_format_version(src.format_version())
        == PostingsLayout::DeltaVarintSkip(SkipEntry::V6)
    {
        Ok(search_bmw_counted(src, fts_root_page, query, k, keep, doc_len)?.0)
    } else {
        search_linear(src, fts_root_page, query, k, keep, doc_len)
    }
}

/// The linear two-pass scan (FT2, `docs/adr/0018`): Pass 1 decodes every
/// matched term's postings and accumulates a per-candidate upper bound, Pass 2
/// evaluates candidates exactly, best bound first, stopping when the remaining
/// bounds fall strictly below the k-th exact score. Production path for
/// `format_version` ≤ 5 files (their skip entries lack the per-block bounds
/// BMW needs) and the reference oracle the BMW-2 equivalence tests compare
/// [`search_bmw_counted`] against — do not fold it into the BMW path.
fn search_linear(
    src: &dyn PageSource,
    fts_root_page: u64,
    query: &str,
    k: usize,
    mut keep: impl FnMut(Ulid) -> bool,
    mut doc_len: impl FnMut(Ulid) -> Result<Option<u32>>,
) -> Result<Vec<Hit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let Some(meta) = load_meta(src, fts_root_page)? else {
        return Ok(Vec::new());
    };
    if meta.doc_count == 0 || meta.dict_root == 0 {
        return Ok(Vec::new());
    }

    // Distinct query terms (a repeated query word should not multiply weight).
    let mut query_terms: Vec<String> = tokenize(query)
        .into_iter()
        .map(|t| clip_term(&t).to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    query_terms.sort();
    query_terms.dedup();
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }

    let n = meta.doc_count as f32;
    let avgdl = if meta.doc_count == 0 {
        0.0
    } else {
        meta.total_tokens as f32 / meta.doc_count as f32
    };

    // Pass 1 — decode every matched term's postings once and accumulate a
    // per-candidate upper bound on its BM25 score, without calling `keep` or
    // `doc_len`. `dl = 0` minimizes the length norm, so each term's bound
    // dominates its true contribution for any document length; the bound
    // stays sound under f32 rounding because +, ×, ÷ round monotonically and
    // the exact score below sums the same terms in the same order.
    let mut matched: Vec<(f32, Postings)> = Vec::with_capacity(query_terms.len());
    let mut bounds: HashMap<Ulid, f32> = HashMap::new();
    for term in &query_terms {
        let Some(postings) = postings_for(src, meta.dict_root, term.as_bytes())? else {
            continue;
        };
        let df = postings.entries.len() as f32;
        if df == 0.0 {
            continue;
        }
        // Standard BM25 IDF with the +0.5 smoothing; always positive here
        // because df <= N (a term's postings are a subset of the corpus).
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        for p in &postings.entries {
            *bounds.entry(p.record_id).or_insert(0.0) += bound_contribution(idf, p.term_freq);
        }
        matched.push((idf, postings));
    }

    // Best bound first; ties by id so the evaluation order is deterministic.
    let mut candidates: Vec<(Ulid, f32)> = bounds.into_iter().collect();
    candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    // Pass 2 — evaluate candidates exactly, best bound first. Once k exact
    // hits exist and the next bound is *strictly* below the k-th best exact
    // score, no unevaluated candidate can enter the top k (its true score is
    // at most its bound) nor displace an equal-score hit (a tie would need
    // bound == k-th score, which is not strictly below), so stop.
    // At most k hits live here (plus one transiently during insert); a huge
    // caller `k` (e.g. "no limit") must neither overflow nor preallocate.
    let mut hits: Vec<Hit> = Vec::with_capacity(k.saturating_add(1).min(candidates.len()));
    for (id, bound) in candidates {
        if hits.len() == k && bound < hits[k - 1].score {
            break;
        }
        if !keep(id) {
            continue;
        }
        let Some(dl) = doc_len(id)? else {
            continue; // record vanished; skip it
        };
        // Exact BM25 — same expression and same (sorted) term order as the
        // exhaustive scan, so scores are bit-identical to it.
        let mut score = 0.0f32;
        for (idf, postings) in &matched {
            let Ok(i) = postings.entries.binary_search_by(|p| p.record_id.cmp(&id)) else {
                continue;
            };
            let tf = postings.entries[i].term_freq as f32;
            let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl as f32 / avgdl.max(1.0));
            score += idf * (tf * (BM25_K1 + 1.0)) / norm.max(f32::MIN_POSITIVE);
        }
        if score <= 0.0 {
            continue;
        }
        // Insert keeping (score desc, id asc) — the same order the exhaustive
        // scan sorts by (G3) — and keep only the best k.
        let pos =
            hits.partition_point(|h| h.score > score || (h.score == score && h.record_id < id));
        if pos < k {
            hits.insert(
                pos,
                Hit {
                    record_id: id,
                    score,
                },
            );
            hits.truncate(k);
        }
    }
    Ok(hits)
}

// ---------------------------------------------------------------------------
// BlockMax-WAND search (BMW-2, `docs/adr/0025`)
// ---------------------------------------------------------------------------

/// One term's largest possible BM25 contribution to any document holding it
/// with term frequency `tf`: the exact-score expression evaluated at `dl = 0`,
/// which minimizes the length norm. Shared by the linear Pass 1 and the BMW
/// block bounds so both paths reason from bit-identical f32 values; sound
/// under f32 rounding because the exact score's `norm` is ≥ this one and
/// division rounds monotonically.
fn bound_contribution(idf: f32, tf: u32) -> f32 {
    let tf = tf as f32;
    let norm = tf + BM25_K1 * (1.0 - BM25_B);
    idf * (tf * (BM25_K1 + 1.0)) / norm.max(f32::MIN_POSITIVE)
}

/// Multiplicative slack that makes the f64 bound-sum comparisons in
/// [`search_bmw_counted`] safe against f32 rounding. The oracle sums a
/// document's exact f32 contributions in term order; a round-to-nearest f32
/// sum of `m` non-negative terms exceeds the exact real sum by at most a
/// factor `(1 + 2^-24)^(m-1)`, and the f64 accumulation error here is orders
/// of magnitude below that. `1.2e-7 > 2 × 2^-24` per term over-covers both,
/// so `sum_f64 × slack ≤ θ` proves no skipped document's oracle score can
/// exceed `θ` — the "bound that under-estimates" silent-recall bug the BMW-2
/// equivalence suite exists to catch (`docs/adr/0025`).
fn bound_slack(terms: usize) -> f64 {
    1.0 + terms as f64 * 1.2e-7
}

/// Per-block metadata one BMW cursor navigates by, parsed once from the
/// version-6 skip index (or derived from the decoded list for a small,
/// skip-less term, which becomes one synthetic block).
struct BmwBlock {
    /// First posting id in the block (a real id — skip entry field).
    first_id: u128,
    /// Last posting id in the block — the block max doc id (BMW-1).
    last_id: u128,
    /// Byte offset of the block's delta run, relative to the blocks region.
    offset: usize,
    /// Number of postings in the block.
    len: usize,
    /// Stored per-block `max_term_freq` (BMW-1's impact bound).
    max_tf: u32,
    /// [`bound_contribution`] at `max_tf` — the block max impact, idf-scaled.
    ub: f32,
}

/// Work counters for one BlockMax-WAND search — bench-only surface for BMW-3's
/// measurement (`docs/adr/0025`), same pattern as [`SearchPhaseTimings`]:
/// production callers go through [`search`], which throws the counters away;
/// keeping them always-on costs four u64 bumps, nothing else.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BmwCounters {
    /// Blocks across every matched term's postings list (small lists = 1).
    pub blocks_total: u64,
    /// Blocks actually decoded. `blocks_total - blocks_decoded` were skipped —
    /// the decode work BMW saved over the linear Pass 1, which always decodes
    /// every block.
    pub blocks_decoded: u64,
    /// Documents evaluated exactly (`keep`/`doc_len` called, BM25 scored).
    pub docs_evaluated: u64,
    /// Pivot candidates discarded by the block-max check without evaluation.
    pub pivot_skips: u64,
}

impl BmwCounters {
    /// Blocks never decoded — what the skip index actually cut.
    pub fn blocks_skipped(&self) -> u64 {
        self.blocks_total.saturating_sub(self.blocks_decoded)
    }
}

/// A document-at-a-time cursor over one term's version-6 postings body
/// (BMW-2, `docs/adr/0025`): walks the list in id order decoding **only the
/// blocks it lands inside**, navigating by the skip index alone. Landing
/// exactly on a block's `first_id` needs no decode — the skip entry carries
/// it — so a block jumped over or merely landed-on stays undecoded.
///
/// Every block it does decode is re-verified against its skip entry exactly
/// like the full decoder (`decode_delta_varint_skip`) does, and the skip
/// index itself is sanity-checked at open — a fast read path replicates all
/// of the decoder's invariant checks (the `lookup_via_skip` fuzz lesson).
struct BmwCursor {
    idf: f32,
    body: Vec<u8>,
    page_no: u64,
    /// Start of the blocks region within `body` (0 for a small decoded list).
    blocks_start: usize,
    blocks: Vec<BmwBlock>,
    /// Index of the block `cur_id` lives in.
    cur_block: usize,
    /// Decoded entries of `cur_block`; empty = not decoded yet (the cursor
    /// then sits on the block's `first_id`, known from the skip entry alone).
    entries: Vec<Posting>,
    /// Position of `cur_id` within `entries` (meaningful only when decoded).
    pos: usize,
    /// The id the cursor currently sits on — always a real posting id.
    cur_id: u128,
    /// Largest [`bound_contribution`] over the whole list (max of block ubs) —
    /// the WAND term upper bound.
    term_ub: f32,
    exhausted: bool,
}

impl BmwCursor {
    /// Opens a cursor over `body`; `None` when the list is empty. A small list
    /// (`block_count = 0`, no skip index) is fully decoded through the normal
    /// layout decoder and served as one synthetic block; a blocked list parses
    /// and validates the skip index only, deferring block decodes to
    /// navigation.
    fn open(
        idf: f32,
        body: Vec<u8>,
        page_no: u64,
        counters: &mut BmwCounters,
    ) -> Result<Option<BmwCursor>> {
        let count = dict::read_u32(&body, 0, page_no)? as usize;
        let block_count = dict::read_u32(&body, 4, page_no)? as usize;
        if block_count == 0 {
            let decoded = Postings::decode_delta_varint_skip(&body, page_no, SkipEntry::V6)?;
            let Some((first, last)) = decoded.entries.first().zip(decoded.entries.last()) else {
                return Ok(None);
            };
            let max_tf = decoded
                .entries
                .iter()
                .map(|p| p.term_freq)
                .max()
                .unwrap_or(0);
            let ub = bound_contribution(idf, max_tf);
            let block = BmwBlock {
                first_id: u128::from(first.record_id),
                last_id: u128::from(last.record_id),
                offset: 0,
                len: decoded.entries.len(),
                max_tf,
                ub,
            };
            counters.blocks_total += 1;
            counters.blocks_decoded += 1;
            let cur_id = block.first_id;
            return Ok(Some(BmwCursor {
                idf,
                body,
                page_no,
                blocks_start: 0,
                blocks: vec![block],
                cur_block: 0,
                entries: decoded.entries,
                pos: 0,
                cur_id,
                term_ub: ub,
                exhausted: false,
            }));
        }
        // Blocked list: the same header sanity the full decoder enforces,
        // before trusting any offset in the index.
        let expected_blocks = count.div_ceil(SKIP_BLOCK_SIZE);
        if count < SKIP_MIN_DOC_FREQ || block_count != expected_blocks {
            return Err(malformed(page_no, "fts skip block_count mismatch"));
        }
        let entry_len = SkipEntry::V6.len();
        let index_len = block_count
            .checked_mul(entry_len)
            .ok_or_else(|| malformed(page_no, "fts skip index overflow"))?;
        let blocks_start = 8usize
            .checked_add(index_len)
            .ok_or_else(|| malformed(page_no, "fts skip index overflow"))?;
        if body.len() < blocks_start {
            return Err(malformed(page_no, "fts skip index truncated"));
        }
        reject_hostile_delta_count(&body[blocks_start.saturating_sub(4)..], count, page_no)?;

        let mut blocks: Vec<BmwBlock> = Vec::with_capacity(block_count);
        let mut term_ub = 0.0f32;
        for b in 0..block_count {
            let idx = 8 + b * entry_len;
            let first_id = read_u128_le(&body, idx, page_no)?;
            let last_id = read_u128_le(&body, idx + 16, page_no)?;
            let tail = idx + SkipEntry::V6.tail_off();
            let offset = dict::read_u32(&body, tail, page_no)? as usize;
            let max_tf = dict::read_u32(&body, tail + 4, page_no)?;
            let len = if b + 1 == block_count {
                count - b * SKIP_BLOCK_SIZE
            } else {
                SKIP_BLOCK_SIZE
            };
            // The metadata-level invariants navigation relies on before any
            // block is decoded: ids ordered within and across blocks, offsets
            // strictly increasing from 0, a positive impact bound (every
            // posting has term_freq ≥ 1). A block that is decoded later is
            // still re-checked against its bytes (`decode_block`).
            if first_id > last_id
                || max_tf == 0
                || (b == 0 && offset != 0)
                || blocks
                    .last()
                    .is_some_and(|p| p.last_id >= first_id || p.offset >= offset)
            {
                return Err(malformed(page_no, "fts skip index inconsistent"));
            }
            let ub = bound_contribution(idf, max_tf);
            term_ub = term_ub.max(ub);
            blocks.push(BmwBlock {
                first_id,
                last_id,
                offset,
                len,
                max_tf,
                ub,
            });
        }
        counters.blocks_total += block_count as u64;
        let cur_id = blocks[0].first_id;
        Ok(Some(BmwCursor {
            idf,
            body,
            page_no,
            blocks_start,
            blocks,
            cur_block: 0,
            entries: Vec::new(),
            pos: 0,
            cur_id,
            term_ub,
            exhausted: false,
        }))
    }

    /// Decodes block `b` into `entries` and re-verifies its skip entry against
    /// the decoded bytes — the same `first_id`/`last_id`/`max_term_freq`
    /// checks the full decoder performs.
    fn decode_block(&mut self, b: usize, counters: &mut BmwCounters) -> Result<()> {
        let (offset, len, first_id, last_id, max_tf) = {
            let m = &self.blocks[b];
            (m.offset, m.len, m.first_id, m.last_id, m.max_tf)
        };
        let mut off = self
            .blocks_start
            .checked_add(offset)
            .ok_or_else(|| malformed(self.page_no, "fts skip byte_offset overflow"))?;
        let mut entries = Vec::with_capacity(len);
        decode_delta_run(&self.body, &mut off, len, self.page_no, &mut entries)?;
        if entries.first().map(|p| u128::from(p.record_id)) != Some(first_id) {
            return Err(malformed(self.page_no, "fts skip first_id mismatch"));
        }
        if entries.last().map(|p| u128::from(p.record_id)) != Some(last_id) {
            return Err(malformed(self.page_no, "fts skip last_id mismatch"));
        }
        if entries.iter().map(|p| p.term_freq).max() != Some(max_tf) {
            return Err(malformed(self.page_no, "fts skip max_term_freq mismatch"));
        }
        self.entries = entries;
        self.cur_block = b;
        counters.blocks_decoded += 1;
        Ok(())
    }

    /// Moves the cursor to the first posting with id ≥ `target` (no-op when
    /// already there), decoding a block only when the landing position falls
    /// strictly inside one. Blocks passed over are never decoded — that jump
    /// is the whole point of the skip index.
    fn advance_to(&mut self, target: u128, counters: &mut BmwCounters) -> Result<()> {
        if self.exhausted || target <= self.cur_id {
            return Ok(());
        }
        let rel = self.blocks[self.cur_block..].partition_point(|m| m.last_id < target);
        let b = self.cur_block + rel;
        if b >= self.blocks.len() {
            self.exhausted = true;
            return Ok(());
        }
        if self.blocks[b].first_id >= target {
            // Land on the block's first posting without decoding it.
            if b != self.cur_block {
                self.entries.clear();
                self.cur_block = b;
            }
            self.pos = 0;
            self.cur_id = self.blocks[b].first_id;
            return Ok(());
        }
        if b != self.cur_block || self.entries.is_empty() {
            self.decode_block(b, counters)?;
        }
        let pos = self
            .entries
            .partition_point(|p| u128::from(p.record_id) < target);
        // Unreachable when the verified `last_id` ≥ target held, but a typed
        // error beats an index panic on a byzantine body (G4).
        let posting = self
            .entries
            .get(pos)
            .ok_or_else(|| malformed(self.page_no, "fts skip cursor past block"))?;
        self.pos = pos;
        self.cur_id = u128::from(posting.record_id);
        Ok(())
    }

    /// Advances to the first posting with id strictly greater than `id`.
    fn advance_past(&mut self, id: u128, counters: &mut BmwCounters) -> Result<()> {
        match id.checked_add(1) {
            Some(next) => self.advance_to(next, counters),
            None => {
                self.exhausted = true;
                Ok(())
            }
        }
    }

    /// The block that could contain `target` — the first block at or after the
    /// cursor whose `last_id` ≥ `target` — without decoding anything. `None`
    /// when the list ends before `target`: the term cannot contribute to it or
    /// anything past it.
    fn covering_block(&self, target: u128) -> Option<&BmwBlock> {
        let rel = self.blocks[self.cur_block..].partition_point(|m| m.last_id < target);
        self.blocks.get(self.cur_block + rel)
    }

    /// Term frequency at the cursor's current posting, decoding the current
    /// block if the cursor sits on an undecoded block's `first_id`.
    fn current_tf(&mut self, counters: &mut BmwCounters) -> Result<u32> {
        if self.entries.is_empty() {
            self.decode_block(self.cur_block, counters)?;
            self.pos = 0; // the undecoded cursor always sits on first_id
        }
        let posting = self
            .entries
            .get(self.pos)
            .ok_or_else(|| malformed(self.page_no, "fts skip cursor past block"))?;
        if u128::from(posting.record_id) != self.cur_id {
            return Err(malformed(self.page_no, "fts skip cursor desync"));
        }
        Ok(posting.term_freq)
    }
}

/// BlockMax-WAND BM25 search (BMW-2, `docs/adr/0025`) over a `format_version`
/// ≥ 6 file, returning the same `(hits, counters)` the bench harness needs —
/// [`search`] is the production entry and discards the counters.
///
/// Document-at-a-time over one [`BmwCursor`] per matched term: WAND pivot
/// selection over term upper bounds, then the block-max refinement — if the
/// summed per-block impact bounds of the blocks that could contain the pivot
/// cannot beat the current k-th exact score, the whole id range up to the
/// shallowest covering block's `last_id` is skipped without decoding a single
/// block. Evaluated documents are scored with the exact oracle expression, in
/// sorted term order, so scores are bit-identical to the linear scan; ties
/// break by `record_id` before the cut on every path, and all bound
/// comparisons carry [`bound_slack`], so the top-k is provably identical to
/// [`search_linear`]/[`search_profiled`] — the equivalence suite (unit +
/// property tests below) verifies exactly that.
#[doc(hidden)]
pub fn search_bmw_counted(
    src: &dyn PageSource,
    fts_root_page: u64,
    query: &str,
    k: usize,
    mut keep: impl FnMut(Ulid) -> bool,
    mut doc_len: impl FnMut(Ulid) -> Result<Option<u32>>,
) -> Result<(Vec<Hit>, BmwCounters)> {
    let mut counters = BmwCounters::default();
    if k == 0 {
        return Ok((Vec::new(), counters));
    }
    let Some(meta) = load_meta(src, fts_root_page)? else {
        return Ok((Vec::new(), counters));
    };
    if meta.doc_count == 0 || meta.dict_root == 0 {
        return Ok((Vec::new(), counters));
    }

    let mut query_terms: Vec<String> = tokenize(query)
        .into_iter()
        .map(|t| clip_term(&t).to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    query_terms.sort();
    query_terms.dedup();
    if query_terms.is_empty() {
        return Ok((Vec::new(), counters));
    }

    let n = meta.doc_count as f32;
    let avgdl = if meta.doc_count == 0 {
        0.0
    } else {
        meta.total_tokens as f32 / meta.doc_count as f32
    };

    // One cursor per matched term, in sorted term order — the exact-score
    // loop below then visits terms in the same order as the oracle, keeping
    // scores bit-identical. `df` is the stored posting count, which is what
    // the oracle's decoded `entries.len()` equals, so idf matches bit-for-bit.
    let mut cursors: Vec<BmwCursor> = Vec::with_capacity(query_terms.len());
    for term in &query_terms {
        let Some((body, page_no)) = dict::get(src, FTS_DICT, meta.dict_root, term.as_bytes())?
        else {
            continue;
        };
        let df = dict::read_u32(&body, 0, page_no)? as usize;
        if df == 0 {
            continue;
        }
        let idf = (1.0 + (n - df as f32 + 0.5) / (df as f32 + 0.5)).ln();
        if let Some(cursor) = BmwCursor::open(idf, body, page_no, &mut counters)? {
            cursors.push(cursor);
        }
    }
    if cursors.is_empty() {
        return Ok((Vec::new(), counters));
    }
    let slack = bound_slack(cursors.len());

    // Top-k under (score desc, id asc) — the same order and insert rule as
    // the linear Pass 2, so the boundary tie-break is identical by
    // construction. No preallocation: a huge caller `k` must not allocate.
    let mut hits: Vec<Hit> = Vec::new();
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    loop {
        order.retain(|&i| !cursors[i].exhausted);
        if order.is_empty() {
            break;
        }
        // Deterministic id order; ties by term index (evaluation order never
        // depends on iteration incidentals — G3).
        order.sort_by(|&a, &b| {
            cursors[a]
                .cur_id
                .cmp(&cursors[b].cur_id)
                .then_with(|| a.cmp(&b))
        });
        let theta: Option<f32> = (hits.len() == k).then(|| hits[k - 1].score);

        // WAND pivot: the shortest id-ordered prefix whose summed term bounds
        // could beat θ. No such prefix ⇒ nothing left can enter the top k.
        let mut acc = 0.0f64;
        let mut pivot: Option<usize> = None;
        for (oi, &ci) in order.iter().enumerate() {
            acc += f64::from(cursors[ci].term_ub);
            if theta.is_none_or(|t| acc * slack > f64::from(t)) {
                pivot = Some(oi);
                break;
            }
        }
        let Some(p) = pivot else {
            break;
        };
        let pivot_id = cursors[order[p]].cur_id;
        // Cursors past `p` sitting on the same id contribute to the same
        // document; the block-max check must bound its *full* potential score.
        let prefix_end = p + order[p + 1..]
            .iter()
            .take_while(|&&ci| cursors[ci].cur_id == pivot_id)
            .count();

        // BMW refinement: re-bound the pivot document by the per-block impact
        // bounds of the blocks that could contain it (BMW-1's `last_id` +
        // `max_term_freq`), instead of whole-list maxima.
        let mut block_acc = 0.0f64;
        let mut min_block_last = u128::MAX;
        for &ci in &order[..=prefix_end] {
            if let Some(block) = cursors[ci].covering_block(pivot_id) {
                block_acc += f64::from(block.ub);
                min_block_last = min_block_last.min(block.last_id);
            }
        }
        if theta.is_some_and(|t| block_acc * slack <= f64::from(t)) {
            // No document in [pivot_id, min_block_last] can beat θ: every
            // term that could touch one is in the prefix (later cursors sit
            // beyond), and its contribution is bounded by this same covering
            // block's impact bound. Jump past the shallowest covering block —
            // or to the next cursor's id, whichever is nearer — without
            // decoding anything.
            counters.pivot_skips += 1;
            let next = match (min_block_last.checked_add(1), order.get(prefix_end + 1)) {
                (Some(n), Some(&after)) => Some(n.min(cursors[after].cur_id)),
                (None, Some(&after)) => Some(cursors[after].cur_id),
                (n, None) => n,
            };
            match next {
                Some(n) => {
                    for &ci in &order[..=prefix_end] {
                        cursors[ci].advance_to(n, &mut counters)?;
                    }
                }
                // The covering blocks reach u128::MAX and no cursor sits
                // beyond: nothing after the pivot range is left in these
                // lists.
                None => {
                    for &ci in &order[..=prefix_end] {
                        cursors[ci].exhausted = true;
                    }
                }
            }
            continue;
        }
        if cursors[order[0]].cur_id != pivot_id {
            // Not aligned yet: bring the earlier cursors up to the pivot.
            // Documents jumped over are ruled out by the pivot choice — the
            // terms that could touch them form a proper prefix, whose slacked
            // bound sum was ≤ θ (the pivot is the *first* prefix exceeding
            // it).
            for &ci in &order[..p] {
                cursors[ci].advance_to(pivot_id, &mut counters)?;
            }
            continue;
        }
        // Aligned on the pivot: evaluate it exactly — identical expression
        // and (sorted) term order to the oracle, so the score is
        // bit-identical, then insert under the same (score desc, id asc)
        // boundary rule.
        counters.docs_evaluated += 1;
        let id = Ulid::from(pivot_id);
        if keep(id)
            && let Some(dl) = doc_len(id)?
        {
            let mut score = 0.0f32;
            for c in cursors.iter_mut() {
                if c.exhausted || c.cur_id != pivot_id {
                    continue;
                }
                let tf = c.current_tf(&mut counters)? as f32;
                let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl as f32 / avgdl.max(1.0));
                score += c.idf * (tf * (BM25_K1 + 1.0)) / norm.max(f32::MIN_POSITIVE);
            }
            if score > 0.0 {
                let pos = hits
                    .partition_point(|h| h.score > score || (h.score == score && h.record_id < id));
                if pos < k {
                    hits.insert(
                        pos,
                        Hit {
                            record_id: id,
                            score,
                        },
                    );
                    hits.truncate(k);
                }
            }
        }
        for &ci in &order[..=prefix_end] {
            cursors[ci].advance_past(pivot_id, &mut counters)?;
        }
    }
    Ok((hits, counters))
}

/// Per-phase timings for one [`search_bmw_profiled`] call, in nanoseconds —
/// measurement-only surface for story FTOPT-5 (`docs/adr/0017` §"Resultado do
/// profiling confirmatório pós-sidecar"). FTOPT-0 profiled the pre-FTOPT-1,
/// pre-BMW linear scan ([`search_profiled`]); that scan is no longer what
/// production `Store::recall`/`Store::search_text` run on a `format_version`
/// ≥ 6 file (`search` dispatches to [`search_bmw_counted`] instead), so its
/// 88.8%-in-`keep` finding no longer describes the live path after FTOPT-1
/// moved `keep`/`doc_len` onto the filter-meta sidecar. This struct
/// instruments the *actual* hot path so FTOPT-5 measures, rather than
/// assumes, where the post-sidecar time goes.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BmwPhaseTimings {
    /// Opening every matched term's cursor: `dict::get` (page I/O) plus, for
    /// small skip-less lists, a full decode ([`BmwCursor::open`]).
    pub cursor_open_ns: u64,
    /// The WAND pivot/block-max bound loop: summing term/block upper bounds,
    /// selecting the pivot, and the block-max refinement check that decides
    /// skip vs. evaluate — pure bound arithmetic, no block decode.
    pub bound_ns: u64,
    /// Block decodes triggered by cursor advances (`advance_to`/`advance_past`
    /// landing inside a block) or the aligned pivot's `current_tf` — the
    /// postings bytes BMW actually had to materialize, as opposed to bounds
    /// it could reason about from the skip index alone.
    pub decode_ns: u64,
    /// The `keep` closure — now sidecar-backed (`docs/adr/0027`) for aligned
    /// pivots that made it past the block-max check.
    pub keep_ns: u64,
    /// The `doc_len` closure — sidecar-backed for ids the sidecar has seen.
    pub doc_len_ns: u64,
    /// Exact BM25 scoring of the aligned pivot plus top-k insert.
    pub scoring_ns: u64,
    /// Documents evaluated exactly (mirrors [`BmwCounters::docs_evaluated`]).
    pub docs_evaluated: u64,
    /// Pivot candidates the block-max check skipped without evaluation
    /// (mirrors [`BmwCounters::pivot_skips`]).
    pub pivot_skips: u64,
    /// Blocks decoded across every cursor (mirrors [`BmwCounters::blocks_decoded`]).
    pub blocks_decoded: u64,
    /// Blocks whose bounds were consulted but never decoded (mirrors
    /// [`BmwCounters::blocks_total`] minus `blocks_decoded`).
    pub blocks_skipped: u64,
}

/// [`search_bmw_counted`] — the production BlockMax-WAND path a
/// `format_version` ≥ 6 file actually runs — instrumented phase-by-phase with
/// [`std::time::Instant`], same method and rationale as [`search_profiled`]
/// (`docs/adr/0017` §1): a separate function so production `search` never
/// pays a single extra `Instant::now()`, `#[doc(hidden)]` like
/// [`fuzz_decode_page`]. Exists for FTOPT-5, which needs the *current* hot
/// path's phase breakdown, not the pre-BMW, pre-sidecar scan FTOPT-0 measured.
///
/// Mirrors [`search_bmw_counted`]'s algorithm exactly (bit-identical
/// results — this is the same code with `Instant` calls interleaved, not a
/// reimplementation) so its timings describe production behavior, not an
/// approximation of it.
#[doc(hidden)]
pub fn search_bmw_profiled(
    src: &dyn PageSource,
    fts_root_page: u64,
    query: &str,
    k: usize,
    mut keep: impl FnMut(Ulid) -> bool,
    mut doc_len: impl FnMut(Ulid) -> Result<Option<u32>>,
) -> Result<(Vec<Hit>, BmwPhaseTimings)> {
    let mut timings = BmwPhaseTimings::default();
    let mut counters = BmwCounters::default();
    if k == 0 {
        return Ok((Vec::new(), timings));
    }
    let Some(meta) = load_meta(src, fts_root_page)? else {
        return Ok((Vec::new(), timings));
    };
    if meta.doc_count == 0 || meta.dict_root == 0 {
        return Ok((Vec::new(), timings));
    }

    let mut query_terms: Vec<String> = tokenize(query)
        .into_iter()
        .map(|t| clip_term(&t).to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    query_terms.sort();
    query_terms.dedup();
    if query_terms.is_empty() {
        return Ok((Vec::new(), timings));
    }

    let n = meta.doc_count as f32;
    let avgdl = if meta.doc_count == 0 {
        0.0
    } else {
        meta.total_tokens as f32 / meta.doc_count as f32
    };

    let cursor_open_started = Instant::now();
    let mut cursors: Vec<BmwCursor> = Vec::with_capacity(query_terms.len());
    for term in &query_terms {
        let Some((body, page_no)) = dict::get(src, FTS_DICT, meta.dict_root, term.as_bytes())?
        else {
            continue;
        };
        let df = dict::read_u32(&body, 0, page_no)? as usize;
        if df == 0 {
            continue;
        }
        let idf = (1.0 + (n - df as f32 + 0.5) / (df as f32 + 0.5)).ln();
        if let Some(cursor) = BmwCursor::open(idf, body, page_no, &mut counters)? {
            cursors.push(cursor);
        }
    }
    timings.cursor_open_ns += cursor_open_started.elapsed().as_nanos() as u64;
    if cursors.is_empty() {
        return Ok((Vec::new(), timings));
    }
    let slack = bound_slack(cursors.len());

    let mut hits: Vec<Hit> = Vec::new();
    let mut order: Vec<usize> = (0..cursors.len()).collect();
    loop {
        let bound_started = Instant::now();
        order.retain(|&i| !cursors[i].exhausted);
        if order.is_empty() {
            timings.bound_ns += bound_started.elapsed().as_nanos() as u64;
            break;
        }
        order.sort_by(|&a, &b| {
            cursors[a]
                .cur_id
                .cmp(&cursors[b].cur_id)
                .then_with(|| a.cmp(&b))
        });
        let theta: Option<f32> = (hits.len() == k).then(|| hits[k - 1].score);

        let mut acc = 0.0f64;
        let mut pivot: Option<usize> = None;
        for (oi, &ci) in order.iter().enumerate() {
            acc += f64::from(cursors[ci].term_ub);
            if theta.is_none_or(|t| acc * slack > f64::from(t)) {
                pivot = Some(oi);
                break;
            }
        }
        let Some(p) = pivot else {
            timings.bound_ns += bound_started.elapsed().as_nanos() as u64;
            break;
        };
        let pivot_id = cursors[order[p]].cur_id;
        let prefix_end = p + order[p + 1..]
            .iter()
            .take_while(|&&ci| cursors[ci].cur_id == pivot_id)
            .count();

        let mut block_acc = 0.0f64;
        let mut min_block_last = u128::MAX;
        for &ci in &order[..=prefix_end] {
            if let Some(block) = cursors[ci].covering_block(pivot_id) {
                block_acc += f64::from(block.ub);
                min_block_last = min_block_last.min(block.last_id);
            }
        }
        let skip = theta.is_some_and(|t| block_acc * slack <= f64::from(t));
        timings.bound_ns += bound_started.elapsed().as_nanos() as u64;
        if skip {
            counters.pivot_skips += 1;
            let next = match (min_block_last.checked_add(1), order.get(prefix_end + 1)) {
                (Some(n), Some(&after)) => Some(n.min(cursors[after].cur_id)),
                (None, Some(&after)) => Some(cursors[after].cur_id),
                (n, None) => n,
            };
            let decode_started = Instant::now();
            match next {
                Some(n) => {
                    for &ci in &order[..=prefix_end] {
                        cursors[ci].advance_to(n, &mut counters)?;
                    }
                }
                None => {
                    for &ci in &order[..=prefix_end] {
                        cursors[ci].exhausted = true;
                    }
                }
            }
            timings.decode_ns += decode_started.elapsed().as_nanos() as u64;
            continue;
        }
        if cursors[order[0]].cur_id != pivot_id {
            let decode_started = Instant::now();
            for &ci in &order[..p] {
                cursors[ci].advance_to(pivot_id, &mut counters)?;
            }
            timings.decode_ns += decode_started.elapsed().as_nanos() as u64;
            continue;
        }
        counters.docs_evaluated += 1;
        let id = Ulid::from(pivot_id);
        let keep_started = Instant::now();
        let kept = keep(id);
        timings.keep_ns += keep_started.elapsed().as_nanos() as u64;
        if kept {
            let doc_len_started = Instant::now();
            let dl = doc_len(id)?;
            timings.doc_len_ns += doc_len_started.elapsed().as_nanos() as u64;
            if let Some(dl) = dl {
                let scoring_started = Instant::now();
                let mut score = 0.0f32;
                for c in cursors.iter_mut() {
                    if c.exhausted || c.cur_id != pivot_id {
                        continue;
                    }
                    let decode_started = Instant::now();
                    let tf = c.current_tf(&mut counters)? as f32;
                    timings.decode_ns += decode_started.elapsed().as_nanos() as u64;
                    let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl as f32 / avgdl.max(1.0));
                    score += c.idf * (tf * (BM25_K1 + 1.0)) / norm.max(f32::MIN_POSITIVE);
                }
                if score > 0.0 {
                    let pos = hits.partition_point(|h| {
                        h.score > score || (h.score == score && h.record_id < id)
                    });
                    if pos < k {
                        hits.insert(
                            pos,
                            Hit {
                                record_id: id,
                                score,
                            },
                        );
                        hits.truncate(k);
                    }
                }
                timings.scoring_ns += scoring_started.elapsed().as_nanos() as u64;
            }
        }
        let decode_started = Instant::now();
        for &ci in &order[..=prefix_end] {
            cursors[ci].advance_past(pivot_id, &mut counters)?;
        }
        timings.decode_ns += decode_started.elapsed().as_nanos() as u64;
    }
    timings.docs_evaluated = counters.docs_evaluated;
    timings.pivot_skips = counters.pivot_skips;
    timings.blocks_decoded = counters.blocks_decoded;
    timings.blocks_skipped = counters.blocks_skipped();
    Ok((hits, timings))
}

/// Number of documents recorded in the full-text index (`embedmind stats`).
/// 0 when no index exists yet.
pub fn indexed_documents(src: &dyn PageSource, fts_root_page: u64) -> Result<u64> {
    Ok(load_meta(src, fts_root_page)?.map_or(0, |m| m.doc_count))
}

/// Per-phase timings for one [`search_profiled`] call, in nanoseconds —
/// measurement-only surface for story FT1 (`docs/adr/0017`). Never called
/// from `api.rs`/production `recall`; exists purely so `benches/` can isolate
/// where the full-text half of hybrid recall spends its time without
/// guessing from code reading (ADR 0017 §1 lists the same four candidates
/// this struct's fields name).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchPhaseTimings {
    /// `dict::get` + `Postings::decode`: term lookup page I/O (cache hit or
    /// disk) *and* bytes-to-`Vec<Posting>` decode are one call in the current
    /// dictionary API, so this phase intentionally bundles both — a probe
    /// that needs to split page-cache-miss I/O from decode CPU would need a
    /// `PageSource` wrapper, out of scope for this read-only measurement.
    pub postings_lookup_ns: u64,
    /// The `keep` closure: tombstone/scope/filter re-check per distinct
    /// candidate id (memoized in `kept`, so this is paid once per id even
    /// though a candidate may appear in several terms' postings).
    pub keep_ns: u64,
    /// The `doc_len` closure: re-loads the candidate's record and
    /// re-tokenizes its content for BM25 length normalization (memoized in
    /// `lengths`, once per id) — the cost the ADR 0011 trade-off (not
    /// persisting `doc_len`) puts on the read path.
    pub doc_len_ns: u64,
    /// `HashMap<Ulid, f32>` insert/accumulate into `scores`, plus the final
    /// sort-and-truncate into ranked `Hit`s.
    pub scoring_ns: u64,
    /// Number of query terms that had a non-empty postings list (informs
    /// whether `postings_lookup_ns` reflects one term or several).
    pub terms_matched: u32,
    /// Number of `(term, posting)` pairs visited across every matched term —
    /// the raw work size `postings_lookup_ns`/`keep_ns`/`doc_len_ns` scale
    /// with, so two runs can be compared per-pair, not just in aggregate.
    pub postings_visited: u64,
    /// Breakdown of what `keep_ns` was spent on, by outcome — measurement-only
    /// surface for story FTOPT-0 (`docs/adr/0017` §"Resultado do profiling
    /// (FTOPT-0/S29)"). FT1 measured that `keep` costs 88.8% of the middle,
    /// but not how much of that is spent on candidates ultimately rejected
    /// (wasted I/O a lighter-metadata optimization could skip) vs. accepted
    /// (I/O the caller needs anyway, since the content must load to return the
    /// hit) — this field is what answers that question. One distinct
    /// candidate id counted once (same memoization as `keep_ns` itself).
    pub keep_outcomes: KeepOutcomeCounts,
}

/// Why a distinct candidate id's `keep` re-check ended the way it did —
/// counted once per id (matching `keep_ns`'s memoization), not once per
/// posting occurrence. `search_profiled`'s own `keep: FnMut(Ulid) -> bool`
/// signature can't carry this — the caller (`Store::search_text_profiled`,
/// `api.rs`) is the only place that knows *why* a candidate failed
/// (tombstone/scope vs. metadata filter), so it reports the outcome back via
/// [`SearchProbe::record_keep_outcome`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepOutcome {
    /// Passed liveness, scope, and every metadata filter — the content will
    /// be loaded anyway to build the returned `Hit`, so this I/O was never
    /// avoidable by a lighter-metadata index.
    Accepted,
    /// Record is missing, tombstoned, or superseded.
    Tombstoned,
    /// Record exists and is live but outside the query's project/agent scope.
    OutOfScope,
    /// Record is live and in scope but failed a metadata filter
    /// (`Query::record_passes_filters`).
    FilteredOut,
}

/// Aggregate counts of [`KeepOutcome`] across every distinct candidate id
/// `keep` was asked about in one [`search_profiled`] call.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KeepOutcomeCounts {
    pub accepted: u64,
    pub tombstoned: u64,
    pub out_of_scope: u64,
    pub filtered_out: u64,
}

impl KeepOutcomeCounts {
    fn record(&mut self, outcome: KeepOutcome) {
        match outcome {
            KeepOutcome::Accepted => self.accepted += 1,
            KeepOutcome::Tombstoned => self.tombstoned += 1,
            KeepOutcome::OutOfScope => self.out_of_scope += 1,
            KeepOutcome::FilteredOut => self.filtered_out += 1,
        }
    }

    /// Total distinct candidates `keep` was asked about.
    pub fn total(&self) -> u64 {
        self.accepted + self.tombstoned + self.out_of_scope + self.filtered_out
    }

    /// Total rejected (any reason) — the I/O a lighter-metadata `keep` could
    /// hope to avoid.
    pub fn rejected(&self) -> u64 {
        self.tombstoned + self.out_of_scope + self.filtered_out
    }
}

/// The **exhaustive** BM25 scan (the pre-FT2 `search` algorithm: every
/// posting of every matched term is scored, `keep`/`doc_len` memoized per
/// candidate), instrumented phase-by-phase with [`std::time::Instant`]
/// (`docs/adr/0017` §1 method: manual instrumentation is the accepted
/// fallback when native flamegraph tooling — `perf`/`samply` — is
/// unavailable on the box). Kept as a **separate** function rather than
/// adding timing to `search` itself so the production path (`Store::recall`,
/// `Store::search_text`) never pays a single extra `Instant::now()` call —
/// this is read-only profiling surface, not a production code path change
/// (`#[doc(hidden)]`, same visibility pattern as [`fuzz_decode_page`]).
///
/// Since FT2 (`docs/adr/0018`) this doubles as the **equivalence oracle**:
/// [`search`] terminates its scan early but must return exactly what this
/// full scan returns — same hits, same scores, same order — in any regime;
/// the equivalence tests below compare the two directly.
#[doc(hidden)]
pub fn search_profiled(
    src: &dyn PageSource,
    fts_root_page: u64,
    query: &str,
    k: usize,
    mut keep: impl FnMut(Ulid) -> KeepOutcome,
    mut doc_len: impl FnMut(Ulid) -> Result<Option<u32>>,
) -> Result<(Vec<Hit>, SearchPhaseTimings)> {
    let mut timings = SearchPhaseTimings::default();
    if k == 0 {
        return Ok((Vec::new(), timings));
    }
    let Some(meta) = load_meta(src, fts_root_page)? else {
        return Ok((Vec::new(), timings));
    };
    if meta.doc_count == 0 || meta.dict_root == 0 {
        return Ok((Vec::new(), timings));
    }

    let mut query_terms: Vec<String> = tokenize(query)
        .into_iter()
        .map(|t| clip_term(&t).to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    query_terms.sort();
    query_terms.dedup();
    if query_terms.is_empty() {
        return Ok((Vec::new(), timings));
    }

    let n = meta.doc_count as f32;
    let avgdl = if meta.doc_count == 0 {
        0.0
    } else {
        meta.total_tokens as f32 / meta.doc_count as f32
    };

    let mut scores: HashMap<Ulid, f32> = HashMap::new();
    let mut lengths: HashMap<Ulid, Option<u32>> = HashMap::new();
    let mut kept: HashMap<Ulid, bool> = HashMap::new();

    for term in &query_terms {
        let lookup_started = std::time::Instant::now();
        let postings = postings_for(src, meta.dict_root, term.as_bytes())?;
        timings.postings_lookup_ns += lookup_started.elapsed().as_nanos() as u64;
        let Some(postings) = postings else {
            continue;
        };
        let df = postings.entries.len() as f32;
        if df == 0.0 {
            continue;
        }
        timings.terms_matched += 1;
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        for p in &postings.entries {
            timings.postings_visited += 1;
            let id = p.record_id;
            let is_kept = match kept.get(&id) {
                Some(&v) => v,
                None => {
                    let keep_started = std::time::Instant::now();
                    let outcome = keep(id);
                    timings.keep_ns += keep_started.elapsed().as_nanos() as u64;
                    timings.keep_outcomes.record(outcome);
                    let v = outcome == KeepOutcome::Accepted;
                    kept.insert(id, v);
                    v
                }
            };
            if !is_kept {
                continue;
            }
            let dl = match lengths.get(&id) {
                Some(&v) => v,
                None => {
                    let doc_len_started = std::time::Instant::now();
                    let v = doc_len(id)?;
                    timings.doc_len_ns += doc_len_started.elapsed().as_nanos() as u64;
                    lengths.insert(id, v);
                    v
                }
            };
            let Some(dl) = dl else {
                continue;
            };
            let scoring_started = std::time::Instant::now();
            let tf = p.term_freq as f32;
            let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl as f32 / avgdl.max(1.0));
            let contribution = idf * (tf * (BM25_K1 + 1.0)) / norm.max(f32::MIN_POSITIVE);
            *scores.entry(id).or_insert(0.0) += contribution;
            timings.scoring_ns += scoring_started.elapsed().as_nanos() as u64;
        }
    }

    let scoring_started = std::time::Instant::now();
    let mut hits: Vec<Hit> = scores
        .into_iter()
        .filter(|&(_, s)| s > 0.0)
        .map(|(record_id, score)| Hit { record_id, score })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    hits.truncate(k);
    timings.scoring_ns += scoring_started.elapsed().as_nanos() as u64;

    Ok((hits, timings))
}

/// Fuzz-only surface: decode one page as each FTS node kind and as postings
/// — in **every** postings layout (fixed-width for `format_version` ≤ 3
/// files, delta+varint for 4, delta+varint+skip v5/v6 for ≥ 5), exercising
/// every parser branch. Also drives the block-skipping [`lookup_via_skip`]
/// over the same bytes so its offset/bounds handling is fuzzed too — for both
/// skip-entry widths, since a hostile v6 body reinterpreted as v5 (or vice
/// versa) must still never panic. Must return, never panic (`fuzz_fts_page`
/// target, `docs/TESTING.md` §3).
#[doc(hidden)]
pub fn fuzz_decode_page(page: &[u8]) {
    dict::fuzz_decode_node(page, FTS_DICT);
    let _ = FtsMeta::decode(page, 1);
    for layout in [
        PostingsLayout::FixedWidth,
        PostingsLayout::DeltaVarint,
        PostingsLayout::DeltaVarintSkip(SkipEntry::V5),
        PostingsLayout::DeltaVarintSkip(SkipEntry::V6),
    ] {
        // Postings bodies live at the page content region; try the body too.
        if page.len() > PAGE_HEADER_LEN {
            let _ = Postings::decode(&page[PAGE_HEADER_LEN..], 1, layout);
        }
        let _ = Postings::decode(page, 1, layout);
    }
    // The skip lookup parses the same hostile bytes on its own path (skip index
    // offsets, block bounds) — it must also never panic, under either entry
    // width.
    let target = Ulid::from_parts(0x1234_5678, 0x9abc_def0);
    for entry in [SkipEntry::V5, SkipEntry::V6] {
        let _ = lookup_via_skip(page, 1, target, entry);
        if page.len() > PAGE_HEADER_LEN {
            let _ = lookup_via_skip(&page[PAGE_HEADER_LEN..], 1, target, entry);
        }
    }
    // The BMW cursor (BMW-2) navigates the same hostile bytes by the skip
    // metadata alone — opening and walking it must never panic or loop.
    fuzz_bmw_cursor(page);
    if page.len() > PAGE_HEADER_LEN {
        fuzz_bmw_cursor(&page[PAGE_HEADER_LEN..]);
    }
}

/// Drives a [`BmwCursor`] over one hostile body: open, step posting by
/// posting (forcing block decodes via `current_tf`), and take a shallow jump —
/// every navigation path the BMW search uses. Must return, never panic; the
/// step count is bounded so a huge (but valid) body cannot stall the fuzzer.
fn fuzz_bmw_cursor(body: &[u8]) {
    let mut counters = BmwCounters::default();
    let Ok(Some(mut cursor)) = BmwCursor::open(1.0, body.to_vec(), 1, &mut counters) else {
        return;
    };
    let _ = cursor.covering_block(cursor.cur_id);
    for _ in 0..64 {
        if cursor.exhausted {
            return;
        }
        let _ = cursor.current_tf(&mut counters);
        if cursor.advance_past(cursor.cur_id, &mut counters).is_err() {
            return;
        }
    }
    // A long valid list: finish with one shallow far jump instead of a walk.
    let _ = cursor.advance_to(u128::MAX, &mut counters);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::storage::pager::{Pager, PagerOptions};
    use crate::storage::sim::{SimVfs, SplitMix64};
    use crate::storage::vfs::Vfs;

    fn pager(page_size: u32) -> Pager {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        Pager::create(
            vfs,
            Path::new("memory.mind"),
            PagerOptions {
                page_size,
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// Indexes documents, returning their ids in order.
    fn index_all(pager: &mut Pager, docs: &[&str]) -> Vec<Ulid> {
        let mut ids = Vec::new();
        let mut txn = pager.begin().unwrap();
        for doc in docs {
            let id = Ulid::new();
            index_document(&mut txn, id, doc).unwrap();
            ids.push(id);
        }
        txn.commit().unwrap();
        ids
    }

    /// `doc_len` closure backed by an id → content map, for tests.
    fn len_of<'a>(
        map: &'a std::collections::HashMap<Ulid, String>,
    ) -> impl FnMut(Ulid) -> Result<Option<u32>> + 'a {
        move |id| Ok(map.get(&id).map(|c| doc_len(c)))
    }

    #[test]
    fn tokenize_is_lowercase_unicode_and_splits_on_punctuation() {
        assert_eq!(tokenize("Hello, WORLD!"), vec!["hello", "world"]);
        // Accented Portuguese words stay whole and lowercased.
        assert_eq!(
            tokenize("Memória número 1 — teste"),
            vec!["memória", "número", "1", "teste"]
        );
        assert!(tokenize("   ...  ").is_empty());
        assert_eq!(doc_len("Hello, WORLD! foo"), 3);
        assert_eq!(doc_len(""), 0);
    }

    #[test]
    fn postings_roundtrip_and_reject_unsorted() {
        let mut p = Postings::default();
        p.upsert(Ulid::from_parts(2, 0), 3);
        p.upsert(Ulid::from_parts(1, 0), 1);
        p.upsert(Ulid::from_parts(1, 0), 5); // update, not duplicate
        assert_eq!(p.entries.len(), 2);
        for layout in [PostingsLayout::FixedWidth, PostingsLayout::DeltaVarint] {
            let body = p.encode(layout);
            assert_eq!(Postings::decode(&body, 1, layout).unwrap(), p);

            // A hostile count with no payload must fail before allocating.
            let mut bad = 1_000_000u32.to_le_bytes().to_vec();
            bad.extend_from_slice(&[0u8; 4]);
            assert!(matches!(
                Postings::decode(&bad, 1, layout),
                Err(Error::MalformedPage { .. })
            ));
        }
    }

    #[test]
    fn delta_varint_roundtrips_and_shrinks_realistic_postings() {
        // Realistic ids: ULIDs minted over time — sorted, random low bits —
        // exactly what a real postings list holds (FORMAT §11: sorted by id).
        let mut rng = SplitMix64(0x5EED_5EED);
        let mut p = Postings::default();
        for i in 0..500u64 {
            let id = Ulid::from_parts(1_700_000_000_000 + i * 37, rng.next_u64().into());
            p.upsert(id, 1 + (rng.next_u64() % 7) as u32);
        }
        let compact = p.encode(PostingsLayout::DeltaVarint);
        let fixed = p.encode(PostingsLayout::FixedWidth);
        assert_eq!(
            Postings::decode(&compact, 1, PostingsLayout::DeltaVarint).unwrap(),
            p
        );
        assert_eq!(
            Postings::decode(&fixed, 1, PostingsLayout::FixedWidth).unwrap(),
            p
        );
        // The whole point of S26: fewer bytes per entry than the fixed 20.
        assert!(
            compact.len() < fixed.len(),
            "delta+varint ({}) must beat fixed-width ({})",
            compact.len(),
            fixed.len()
        );

        // Edge ids round-trip too: 0, adjacent, and u128::MAX.
        let mut edge = Postings::default();
        edge.upsert(Ulid::from(0u128), 1);
        edge.upsert(Ulid::from(1u128), 2);
        edge.upsert(Ulid::from(u128::MAX), u32::MAX);
        let body = edge.encode(PostingsLayout::DeltaVarint);
        assert_eq!(
            Postings::decode(&body, 1, PostingsLayout::DeltaVarint).unwrap(),
            edge
        );
    }

    #[test]
    fn delta_varint_rejects_hostile_bodies() {
        let reject = |body: &[u8], what: &str| {
            assert!(
                matches!(
                    Postings::decode(body, 1, PostingsLayout::DeltaVarint),
                    Err(Error::MalformedPage { .. })
                ),
                "must reject: {what}"
            );
        };
        // Entry count promised but body truncated mid-entry.
        let mut body = 2u32.to_le_bytes().to_vec();
        body.push(0x05); // entry 0: id 5
        body.push(0x01); // entry 0: tf 1
        body.push(0x03); // entry 1: delta 3, then missing tf
        reject(&body, "truncated after last delta");
        // Zero delta after the first entry = duplicate/unsorted id.
        let mut body = 2u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[0x05, 0x01, 0x00, 0x01]);
        reject(&body, "zero delta (duplicate id)");
        // Zero term_freq.
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[0x05, 0x00]);
        reject(&body, "zero term_freq");
        // term_freq beyond u32.
        let mut body = 1u32.to_le_bytes().to_vec();
        body.push(0x05);
        put_varint(&mut body, u128::from(u32::MAX) + 1);
        reject(&body, "term_freq overflow");
        // id accumulation past u128::MAX.
        let mut body = 2u32.to_le_bytes().to_vec();
        put_varint(&mut body, u128::MAX);
        body.push(0x01);
        body.extend_from_slice(&[0x01, 0x01]); // delta 1 wraps
        reject(&body, "id overflow");
        // A varint longer than 19 bytes must terminate the loop with an error.
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[0x80; 20]);
        reject(&body, "varint too long");
        // Data bits shifted past 128 (19th byte with high data bits).
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[0x80; 18]);
        body.push(0x7F); // 7 data bits at shift 126 — overflow
        reject(&body, "varint overflow bits");
    }

    /// Builds a realistic sorted postings list of `n` entries (ULIDs over
    /// time, random low bits — FORMAT §11 order), for the skip-layout tests.
    fn realistic_postings(n: u64, seed: u64) -> Postings {
        let mut rng = SplitMix64(seed);
        let mut p = Postings::default();
        for i in 0..n {
            let id = Ulid::from_parts(1_700_000_000_000 + i * 37, rng.next_u64().into());
            p.upsert(id, 1 + (rng.next_u64() % 9) as u32);
        }
        p
    }

    /// The two skip-entry widths to exercise every equivalence/round-trip test
    /// under both the version-5 (24-byte) and version-6 (40-byte, with the
    /// per-block `last_id` impact-bound field) skip entry.
    const SKIP_ENTRIES: [SkipEntry; 2] = [SkipEntry::V5, SkipEntry::V6];

    #[test]
    fn skip_layout_roundtrips_small_and_large() {
        for entry in SKIP_ENTRIES {
            let layout = PostingsLayout::DeltaVarintSkip(entry);
            // Small (< threshold): block_count = 0, body identical past the
            // count to the plain delta+varint body — skip index costs 4 bytes.
            let small = realistic_postings(10, 0xA1);
            let skip_body = small.encode(layout);
            assert_eq!(Postings::decode(&skip_body, 1, layout).unwrap(), small);
            let plain = small.encode(PostingsLayout::DeltaVarint);
            assert_eq!(
                &skip_body[8..],
                &plain[4..],
                "small body is plain past count ({entry:?})"
            );
            assert_eq!(le_block_count(&skip_body), 0);
            // Small bodies are byte-identical across v5/v6 (no skip index).
            assert_eq!(
                skip_body,
                small.encode(PostingsLayout::DeltaVarintSkip(SkipEntry::V5)),
                "small body must not depend on entry width"
            );

            // Large (≥ threshold): a real skip index, multiple blocks.
            let large = realistic_postings(SKIP_MIN_DOC_FREQ as u64 + 55, 0xB2);
            let body = large.encode(layout);
            assert_eq!(Postings::decode(&body, 1, layout).unwrap(), large);
            let expected_blocks = large.entries.len().div_ceil(SKIP_BLOCK_SIZE);
            assert_eq!(le_block_count(&body) as usize, expected_blocks);
            assert!(
                expected_blocks >= 4,
                "test corpus should span several blocks"
            );
        }
    }

    /// The version-6 skip entry is 40 bytes (16 wider than v5), so a v6 body is
    /// exactly `16 × block_count` bytes larger than the v5 body of the same
    /// postings — proof the `last_id` field is the only added cost.
    #[test]
    fn v6_skip_entry_is_16_bytes_wider_per_block() {
        let large = realistic_postings(SKIP_MIN_DOC_FREQ as u64 + 200, 0xF6);
        let v5 = large.encode(PostingsLayout::DeltaVarintSkip(SkipEntry::V5));
        let v6 = large.encode(PostingsLayout::DeltaVarintSkip(SkipEntry::V6));
        let blocks = large.entries.len().div_ceil(SKIP_BLOCK_SIZE);
        assert_eq!(v6.len(), v5.len() + 16 * blocks);
        assert_eq!(SKIP_ENTRY_LEN_V6 - SKIP_ENTRY_LEN_V5, 16);
    }

    fn le_block_count(body: &[u8]) -> u32 {
        u32::from_le_bytes(body[4..8].try_into().unwrap())
    }

    #[test]
    fn lookup_via_skip_matches_linear_scan_for_every_id() {
        for entry in SKIP_ENTRIES {
            let layout = PostingsLayout::DeltaVarintSkip(entry);
            // Absolute equivalence: the block-skipping lookup returns exactly
            // what a full decode + binary_search returns, present and absent.
            let large = realistic_postings(SKIP_MIN_DOC_FREQ as u64 + 200, 0xC3);
            let body = large.encode(layout);

            for p in &large.entries {
                assert_eq!(
                    lookup_via_skip(&body, 1, p.record_id, entry).unwrap(),
                    Some(p.term_freq),
                    "present id must be found via skip ({entry:?})"
                );
            }
            // Absent ids: below the first, above the last, and in the gaps.
            let below = Ulid::from(0u128);
            assert_eq!(lookup_via_skip(&body, 1, below, entry).unwrap(), None);
            let above = Ulid::from(u128::MAX);
            assert_eq!(lookup_via_skip(&body, 1, above, entry).unwrap(), None);
            // A value strictly between two consecutive ids, if a gap exists.
            for w in large.entries.windows(2) {
                let a = u128::from(w[0].record_id);
                let b = u128::from(w[1].record_id);
                if b - a > 1 {
                    let mid = Ulid::from(a + 1);
                    assert_eq!(lookup_via_skip(&body, 1, mid, entry).unwrap(), None);
                    break;
                }
            }

            // The small path (block_count = 0) also answers correctly.
            let small = realistic_postings(20, 0xD4);
            let sbody = small.encode(layout);
            for p in &small.entries {
                assert_eq!(
                    lookup_via_skip(&sbody, 1, p.record_id, entry).unwrap(),
                    Some(p.term_freq)
                );
            }
            assert_eq!(
                lookup_via_skip(&sbody, 1, Ulid::from(u128::MAX), entry).unwrap(),
                None
            );
        }
    }

    #[test]
    fn skip_layout_rejects_hostile_bodies() {
        for entry in SKIP_ENTRIES {
            let layout = PostingsLayout::DeltaVarintSkip(entry);
            let large = realistic_postings(SKIP_MIN_DOC_FREQ as u64 + 10, 0xE5);
            let good = large.encode(layout);
            let reject = |body: &[u8], what: &str| {
                assert!(
                    matches!(
                        Postings::decode(body, 1, layout),
                        Err(Error::MalformedPage { .. })
                    ),
                    "must reject: {what} ({entry:?})"
                );
            };

            // block_count that does not match count / block size.
            let mut bad = good.clone();
            bad[4..8].copy_from_slice(&1u32.to_le_bytes());
            reject(&bad, "block_count mismatch");

            // A corrupted stored first_id in the skip index (flip a byte).
            let mut bad = good.clone();
            bad[8] ^= 0xFF;
            reject(&bad, "first_id mismatch");

            // A corrupted stored byte_offset (points off the block seam). The
            // `byte_offset`/`max_term_freq` tail sits after `first_id` (and
            // `last_id` in v6), so its position is entry-width dependent.
            let tail = 8 + entry.tail_off();
            let mut bad = good.clone();
            bad[tail] = bad[tail].wrapping_add(1);
            reject(&bad, "byte_offset mismatch");

            // A corrupted stored max_term_freq (right after byte_offset).
            let mut bad = good.clone();
            bad[tail + 4] = bad[tail + 4].wrapping_add(1);
            reject(&bad, "max_term_freq mismatch");

            // v6 only: a corrupted stored last_id (the added block-max-docid).
            if entry == SkipEntry::V6 {
                let mut bad = good.clone();
                bad[8 + 16] ^= 0xFF; // first byte of last_id
                reject(&bad, "last_id mismatch");
            }

            // A skip index promising blocks the body cannot hold.
            let mut bad = 10_000_000u32.to_le_bytes().to_vec();
            bad.extend_from_slice(&78_125u32.to_le_bytes()); // block_count huge
            reject(&bad, "skip index truncated");
        }
    }

    /// Regression for the `fuzz_fts_page` crash committed at
    /// `fuzz/corpus/fuzz_fts_page/crash-9116d630a5fae3ac97551c97104213cd2f5f4e9a`:
    /// a `count`/`block_count` pair where `block_count * SKIP_BLOCK_SIZE >
    /// count` made `lookup_via_skip`'s last-block-length arithmetic
    /// (`count - b * SKIP_BLOCK_SIZE`) underflow and panic. Must now return a
    /// typed `malformed` error, never panic, on both entry points the fuzz
    /// target drives.
    #[test]
    fn lookup_via_skip_rejects_corpus_crash_input() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus/fuzz_fts_page/crash-9116d630a5fae3ac97551c97104213cd2f5f4e9a");
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        crate::fuzz::fuzz_fts_page(&data); // must not panic

        // Both entry points `fuzz_decode_page` drives over this body must
        // return, not panic; the result itself (`Err` or `Ok(None)`) is fine.
        // Both skip-entry widths reinterpret the same bytes and must be safe.
        let target = Ulid::from_parts(0x1234_5678, 0x9abc_def0);
        for entry in SKIP_ENTRIES {
            let _ = lookup_via_skip(&data, 1, target, entry);
            if data.len() > PAGE_HEADER_LEN {
                let _ = lookup_via_skip(&data[PAGE_HEADER_LEN..], 1, target, entry);
            }
        }
    }

    /// Every seed in the `fuzz_fts_page` corpus — including the new v6 skip
    /// seeds committed with this change — must decode through the fuzz entry
    /// without panicking (`docs/TESTING.md` §3). This gives the v6 layout the
    /// same "same-commit fuzz coverage" the crash-safety rule demands, inside
    /// `cargo test` (no nightly libFuzzer needed).
    #[test]
    fn fuzz_fts_page_survives_every_corpus_seed() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/fuzz_fts_page");
        let mut seen_v6 = false;
        for ent in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = ent.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let data = std::fs::read(&path).unwrap();
            crate::fuzz::fuzz_fts_page(&data); // must not panic under any layout
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("v6"))
            {
                seen_v6 = true;
            }
        }
        assert!(seen_v6, "corpus must ship at least one v6 skip seed");
    }

    #[test]
    fn index_and_search_ranks_by_relevance() {
        let mut pager = pager(4096);
        let ids = index_all(
            &mut pager,
            &[
                "the rust compiler enforces memory safety",
                "python is a dynamic language",
                "rust rust rust is about memory and safety in rust",
            ],
        );
        let mut contents = std::collections::HashMap::new();
        contents.insert(
            ids[0],
            "the rust compiler enforces memory safety".to_owned(),
        );
        contents.insert(ids[1], "python is a dynamic language".to_owned());
        contents.insert(
            ids[2],
            "rust rust rust is about memory and safety in rust".to_owned(),
        );

        let root = pager.header().fts_root_page;
        let hits = search(&pager, root, "rust memory", 10, |_| true, len_of(&contents)).unwrap();
        // Doc 2 mentions "rust" four times → should outrank doc 0; doc 1 has
        // neither query term and must not appear.
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].record_id, ids[2]);
        assert_eq!(hits[1].record_id, ids[0]);
        assert!(hits.iter().all(|h| h.score > 0.0));
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn search_profiled_matches_search_exactly() {
        // FT1 (`docs/adr/0017`): the profiled duplicate must never diverge
        // from the production scan it mirrors, or the phase timings would be
        // measuring a different algorithm.
        let mut pager = pager(4096);
        let ids = index_all(
            &mut pager,
            &[
                "the rust compiler enforces memory safety",
                "python is a dynamic language",
                "rust rust rust is about memory and safety in rust",
            ],
        );
        let mut contents = std::collections::HashMap::new();
        contents.insert(
            ids[0],
            "the rust compiler enforces memory safety".to_owned(),
        );
        contents.insert(ids[1], "python is a dynamic language".to_owned());
        contents.insert(
            ids[2],
            "rust rust rust is about memory and safety in rust".to_owned(),
        );
        let root = pager.header().fts_root_page;

        let plain = search(&pager, root, "rust memory", 10, |_| true, len_of(&contents)).unwrap();
        let (profiled, timings) = search_profiled(
            &pager,
            root,
            "rust memory",
            10,
            |_| KeepOutcome::Accepted,
            len_of(&contents),
        )
        .unwrap();
        assert_eq!(plain, profiled);
        assert_eq!(timings.terms_matched, 2); // "memory" and "rust"
        assert!(timings.postings_visited >= 3); // doc 0 + doc 2 for "rust", doc 0 + doc 2 for "memory"
    }

    #[test]
    fn keep_outcomes_are_counted_once_per_distinct_candidate_by_reason() {
        // FTOPT-0 (`docs/adr/0017` §"Resultado do profiling (FTOPT-0)"): the
        // breakdown must attribute each distinct candidate id to exactly one
        // outcome, matching `keep_ns`'s own once-per-id memoization — a
        // candidate appearing in both matched terms' postings ("rust" is in
        // doc 0 and doc 2) must not be double-counted.
        let mut pager = pager(4096);
        let ids = index_all(
            &mut pager,
            &[
                "the rust compiler enforces memory safety",  // accepted
                "python is a dynamic language",              // filtered out
                "rust rust rust is about memory and safety", // tombstoned
                "rust engines are fast",                     // out of scope
            ],
        );
        let mut contents = std::collections::HashMap::new();
        for (i, text) in [
            "the rust compiler enforces memory safety",
            "python is a dynamic language",
            "rust rust rust is about memory and safety",
            "rust engines are fast",
        ]
        .into_iter()
        .enumerate()
        {
            contents.insert(ids[i], text.to_owned());
        }
        let root = pager.header().fts_root_page;

        let (accepted_id, filtered_id, tombstoned_id, out_of_scope_id) =
            (ids[0], ids[1], ids[2], ids[3]);
        let (_, timings) = search_profiled(
            &pager,
            root,
            "rust memory python engines",
            10,
            |id| {
                if id == accepted_id {
                    KeepOutcome::Accepted
                } else if id == filtered_id {
                    KeepOutcome::FilteredOut
                } else if id == tombstoned_id {
                    KeepOutcome::Tombstoned
                } else if id == out_of_scope_id {
                    KeepOutcome::OutOfScope
                } else {
                    KeepOutcome::Tombstoned
                }
            },
            len_of(&contents),
        )
        .unwrap();

        assert_eq!(timings.keep_outcomes.accepted, 1);
        assert_eq!(timings.keep_outcomes.filtered_out, 1);
        assert_eq!(timings.keep_outcomes.tombstoned, 1);
        assert_eq!(timings.keep_outcomes.out_of_scope, 1);
        assert_eq!(timings.keep_outcomes.total(), 4);
        assert_eq!(timings.keep_outcomes.rejected(), 3);
    }

    #[test]
    fn early_termination_matches_exhaustive_scan_on_larger_corpus() {
        // FT2 (`docs/adr/0018`) / BMW-2 (`docs/adr/0025`) hard invariant: the
        // production `search` — BlockMax-WAND here, since the default file is
        // `format_version` 6 — must return exactly what the exhaustive scan
        // *and* the linear two-pass scan return: same hits, same
        // (bit-identical) scores, same order — including when the cut
        // actually triggers: k far below the candidate count, common + rare
        // terms, varied document lengths and term frequencies, exact-tie
        // duplicates, a keep filter, and vanished records.
        let mut pager = pager(4096);
        let mut docs: Vec<String> = Vec::new();
        // Above SKIP_MIN_DOC_FREQ so "common"'s postings carry a real skip
        // index (several blocks) under the version-5 layout: this equivalence
        // test then also proves `search` matches the exhaustive oracle when the
        // skip layout is the on-disk form (DoD: identical results with skip).
        let corpus = SKIP_MIN_DOC_FREQ + 133;
        for i in 0..corpus {
            // Every doc shares "common"; its tf and the doc length vary so
            // bounds and exact scores disagree in both directions, and the
            // cycles (7, 23) repeat content so exact score ties occur.
            let mut d = "common ".repeat(1 + i % 7);
            d.push_str(&"filler ".repeat(i % 23));
            if i % 11 == 0 {
                d.push_str("rare ");
            }
            if i % 37 == 0 {
                d.push_str("rarest");
            }
            docs.push(d);
        }
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ids = index_all(&mut pager, &doc_refs);
        let mut contents = std::collections::HashMap::new();
        for (id, d) in ids.iter().zip(&docs) {
            contents.insert(*id, d.clone());
        }
        // Every 5th record "vanished": indexed, but `doc_len` yields None.
        let mut partial = contents.clone();
        for id in ids.iter().step_by(5) {
            partial.remove(id);
        }
        // A keep filter that rejects a third of the corpus, including
        // candidates holding the best bounds.
        let dropped: std::collections::HashSet<Ulid> = ids
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(i, id)| (i % 3 == 0).then_some(id))
            .collect();

        let root = pager.header().fts_root_page;
        for query in [
            "common",
            "common rare",
            "rare rarest common",
            "filler common",
        ] {
            for k in [1, 3, 10, 500] {
                let plain = search(&pager, root, query, k, |_| true, len_of(&contents)).unwrap();
                let (full, _) = search_profiled(
                    &pager,
                    root,
                    query,
                    k,
                    |_| KeepOutcome::Accepted,
                    len_of(&contents),
                )
                .unwrap();
                assert_eq!(plain, full, "query={query:?} k={k}");
                let lin =
                    search_linear(&pager, root, query, k, |_| true, len_of(&contents)).unwrap();
                assert_eq!(plain, lin, "query={query:?} k={k} (linear)");

                let plain = search(
                    &pager,
                    root,
                    query,
                    k,
                    |id| !dropped.contains(&id),
                    len_of(&contents),
                )
                .unwrap();
                let (full, _) = search_profiled(
                    &pager,
                    root,
                    query,
                    k,
                    |id| {
                        if dropped.contains(&id) {
                            KeepOutcome::Tombstoned
                        } else {
                            KeepOutcome::Accepted
                        }
                    },
                    len_of(&contents),
                )
                .unwrap();
                assert_eq!(plain, full, "query={query:?} k={k} (keep filter)");
                let lin = search_linear(
                    &pager,
                    root,
                    query,
                    k,
                    |id| !dropped.contains(&id),
                    len_of(&contents),
                )
                .unwrap();
                assert_eq!(plain, lin, "query={query:?} k={k} (keep filter, linear)");

                let plain = search(&pager, root, query, k, |_| true, len_of(&partial)).unwrap();
                let (full, _) = search_profiled(
                    &pager,
                    root,
                    query,
                    k,
                    |_| KeepOutcome::Accepted,
                    len_of(&partial),
                )
                .unwrap();
                assert_eq!(plain, full, "query={query:?} k={k} (vanished records)");
                let lin =
                    search_linear(&pager, root, query, k, |_| true, len_of(&partial)).unwrap();
                assert_eq!(plain, lin, "query={query:?} k={k} (vanished, linear)");
            }
        }
    }

    /// BMW-2 boundary-tie regression (`docs/adr/0025` risk #1): identical
    /// documents produce bit-identical BM25 scores, so the top-k boundary is
    /// decided purely by the `record_id` tie-break — which must match the
    /// oracle exactly even when the tie group spans the cut, with enough
    /// duplicates that the shared term carries a real multi-block skip index.
    #[test]
    fn bmw_breaks_boundary_ties_exactly_like_the_oracle() {
        let mut pager = pager(4096);
        let corpus = SKIP_MIN_DOC_FREQ + 64;
        let docs: Vec<String> = (0..corpus)
            .map(|_| "twin memory entry".to_owned())
            .collect();
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ids = index_all(&mut pager, &doc_refs);
        let mut contents = std::collections::HashMap::new();
        for (id, d) in ids.iter().zip(&docs) {
            contents.insert(*id, d.clone());
        }
        let root = pager.header().fts_root_page;
        for k in [1, 2, 5, 100, corpus] {
            let bmw = search(&pager, root, "twin entry", k, |_| true, len_of(&contents)).unwrap();
            let (oracle, _) = search_profiled(
                &pager,
                root,
                "twin entry",
                k,
                |_| KeepOutcome::Accepted,
                len_of(&contents),
            )
            .unwrap();
            assert_eq!(bmw, oracle, "k={k}");
            let lin =
                search_linear(&pager, root, "twin entry", k, |_| true, len_of(&contents)).unwrap();
            assert_eq!(bmw, lin, "k={k} (linear)");
            // All scores tie, so the cut must be exactly id-ascending.
            let mut expected = ids.clone();
            expected.sort();
            expected.truncate(k);
            let got: Vec<Ulid> = bmw.iter().map(|h| h.record_id).collect();
            assert_eq!(got, expected, "k={k} boundary ids");
        }
    }

    /// BMW must actually *skip* (`docs/adr/0025`): on a corpus where one early
    /// short document dominates (high tf, low length norm) and every later
    /// block's impact bound cannot beat it, the search decodes only the block
    /// it evaluates in and discards every later block via the block-max check
    /// — while still returning exactly the oracle's top-k. This pins the
    /// counters BMW-3 will measure with, so the instrumentation cannot rot.
    #[test]
    fn bmw_skips_blocks_and_still_matches_the_oracle() {
        let mut pager = pager(4096);
        let corpus = SKIP_MIN_DOC_FREQ * 3;
        let mut docs: Vec<String> = Vec::new();
        for i in 0..corpus {
            if i == 3 {
                // Short and term-heavy: its exact score beats the tf-1 blocks'
                // upper bound, so once it is in the heap, later blocks skip.
                docs.push("common ".repeat(30));
            } else {
                // Long and term-light: high length norm, weak everywhere.
                docs.push(format!("common {}", "filler ".repeat(50)));
            }
        }
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ids = index_all(&mut pager, &doc_refs);
        let mut contents = std::collections::HashMap::new();
        for (id, d) in ids.iter().zip(&docs) {
            contents.insert(*id, d.clone());
        }
        let root = pager.header().fts_root_page;

        let (hits, counters) =
            search_bmw_counted(&pager, root, "common", 1, |_| true, len_of(&contents)).unwrap();
        let (oracle, _) = search_profiled(
            &pager,
            root,
            "common",
            1,
            |_| KeepOutcome::Accepted,
            len_of(&contents),
        )
        .unwrap();
        assert_eq!(hits, oracle);
        assert_eq!(hits[0].record_id, ids[3]);

        let total_blocks = (corpus.div_ceil(SKIP_BLOCK_SIZE)) as u64;
        assert_eq!(counters.blocks_total, total_blocks);
        assert!(
            counters.pivot_skips > 0,
            "block-max check never fired: {counters:?}"
        );
        assert!(
            counters.blocks_skipped() > counters.blocks_total / 2,
            "BMW must skip most blocks: {counters:?}"
        );
        assert!(
            counters.docs_evaluated <= (SKIP_BLOCK_SIZE + 8) as u64,
            "documents outside the dominant block must never be evaluated: {counters:?}"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(40))]
        /// BMW-2 property equivalence (`docs/adr/0025`): on arbitrary corpora
        /// and queries, the BMW path returns exactly — same ids, bit-identical
        /// scores, same order — what the exhaustive oracle and the linear scan
        /// return, under arbitrary keep filters and vanished records. This is
        /// the detector for both known BMW failure modes: a bound that
        /// under-estimates (silent recall loss) and a boundary tie broken
        /// differently.
        #[test]
        fn bmw_equals_oracle_on_random_corpora(
            docs in proptest::collection::vec(
                proptest::collection::vec(0usize..8, 1..24),
                1..48,
            ),
            query in proptest::collection::vec(0usize..8, 1..5),
            k in 1usize..12,
            keep_mask in proptest::prelude::any::<u64>(),
            vanish_mask in proptest::prelude::any::<u64>(),
        ) {
            const VOCAB: [&str; 8] = [
                "alpha", "beta", "gamma", "delta", "memo", "rust", "engine", "wal",
            ];
            let mut pager = pager(4096);
            let rendered: Vec<String> = docs
                .iter()
                .map(|d| d.iter().map(|&w| VOCAB[w]).collect::<Vec<_>>().join(" "))
                .collect();
            let doc_refs: Vec<&str> = rendered.iter().map(String::as_str).collect();
            let ids = index_all(&mut pager, &doc_refs);
            let mut contents = std::collections::HashMap::new();
            for (i, (id, d)) in ids.iter().zip(&rendered).enumerate() {
                if vanish_mask & (1 << (i % 64)) == 0 {
                    contents.insert(*id, d.clone());
                }
            }
            let kept: std::collections::HashSet<Ulid> = ids
                .iter()
                .enumerate()
                .filter_map(|(i, id)| (keep_mask & (1 << (i % 64)) != 0).then_some(*id))
                .collect();
            let q = query.iter().map(|&w| VOCAB[w]).collect::<Vec<_>>().join(" ");
            let root = pager.header().fts_root_page;
            let keep = |id: Ulid| kept.contains(&id);

            let bmw = search(&pager, root, &q, k, keep, len_of(&contents)).unwrap();
            let (oracle, _) = search_profiled(
                &pager,
                root,
                &q,
                k,
                |id| {
                    if keep(id) {
                        KeepOutcome::Accepted
                    } else {
                        KeepOutcome::Tombstoned
                    }
                },
                len_of(&contents),
            )
            .unwrap();
            proptest::prop_assert_eq!(&bmw, &oracle);
            let lin = search_linear(&pager, root, &q, k, keep, len_of(&contents)).unwrap();
            proptest::prop_assert_eq!(&bmw, &lin);
        }
    }

    #[test]
    fn search_on_empty_or_missing_index_is_empty_not_error() {
        let pager = pager(4096);
        let none = std::collections::HashMap::new();
        // fts_root 0 = no index yet.
        let hits = search(&pager, 0, "anything", 5, |_| true, len_of(&none)).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn keep_filter_excludes_tombstoned_documents() {
        let mut pager = pager(4096);
        let ids = index_all(&mut pager, &["shared term here", "shared term also"]);
        let mut contents = std::collections::HashMap::new();
        contents.insert(ids[0], "shared term here".to_owned());
        contents.insert(ids[1], "shared term also".to_owned());

        let excluded = ids[0];
        let root = pager.header().fts_root_page;
        let hits = search(
            &pager,
            root,
            "shared term",
            10,
            |id| id != excluded,
            len_of(&contents),
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, ids[1]);
    }

    #[test]
    fn large_vocabulary_forces_splits_and_stays_correct() {
        // Small pages + many distinct terms force dictionary node splits.
        let mut pager = pager(512);
        let mut docs = Vec::new();
        for i in 0..200 {
            docs.push(format!("term{i:04} common"));
        }
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ids = index_all(&mut pager, &doc_refs);
        let mut contents = std::collections::HashMap::new();
        for (id, doc) in ids.iter().zip(&docs) {
            contents.insert(*id, doc.clone());
        }

        let root = pager.header().fts_root_page;
        // Every unique term finds exactly its document as the top hit.
        for (i, id) in ids.iter().enumerate() {
            let q = format!("term{i:04}");
            let hits = search(&pager, root, &q, 5, |_| true, len_of(&contents)).unwrap();
            assert!(!hits.is_empty(), "term{i:04} not found");
            assert_eq!(hits[0].record_id, *id, "term{i:04} ranked wrong");
        }
        // "common" appears in every doc → df == N → postings list is huge and
        // must have gone through an overflow chain; it still returns all 200.
        let hits = search(&pager, root, "common", 500, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), 200);
        assert_eq!(indexed_documents(&pager, root).unwrap(), 200);
    }

    #[test]
    fn survives_reopen() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut pager = Pager::create(
            Arc::clone(&vfs),
            Path::new("memory.mind"),
            PagerOptions::default(),
        )
        .unwrap();
        let ids = index_all(&mut pager, &["alpha beta", "beta gamma", "gamma delta"]);
        let mut contents = std::collections::HashMap::new();
        contents.insert(ids[0], "alpha beta".to_owned());
        contents.insert(ids[1], "beta gamma".to_owned());
        contents.insert(ids[2], "gamma delta".to_owned());
        pager.close().unwrap();

        let pager = Pager::open(vfs, Path::new("memory.mind"), PagerOptions::default()).unwrap();
        let root = pager.header().fts_root_page;
        let hits = search(&pager, root, "beta", 10, |_| true, len_of(&contents)).unwrap();
        let found: std::collections::HashSet<Ulid> = hits.iter().map(|h| h.record_id).collect();
        assert!(found.contains(&ids[0]));
        assert!(found.contains(&ids[1]));
        assert!(!found.contains(&ids[2]));
    }

    /// Cross-version round-trip (S26, `docs/adr/0021`): a `format_version` 3
    /// file — created with the pre-S26 write path, which this build preserves
    /// verbatim as the fixed-width layout — must keep working under this
    /// build for reads *and* writes, staying uniform in its own layout, so
    /// the build that wrote it could still read it back.
    #[test]
    fn format_version_3_file_reads_and_writes_fixed_width_postings() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let opts = PagerOptions {
            page_size: 4096,
            format_version: 3,
            ..Default::default()
        };
        let mut pager = Pager::create(Arc::clone(&vfs), Path::new("memory.mind"), opts).unwrap();
        let ids = index_all(&mut pager, &["rust memory engine", "python memory model"]);
        let mut contents = std::collections::HashMap::new();
        contents.insert(ids[0], "rust memory engine".to_owned());
        contents.insert(ids[1], "python memory model".to_owned());

        // The stored body is the fixed-width layout, byte-exactly what the
        // version-3 build wrote: 4 (doc_freq) + 20 per entry.
        let meta = load_meta(&pager, pager.header().fts_root_page)
            .unwrap()
            .unwrap();
        let (body, page_no) = dict::get(&pager, FTS_DICT, meta.dict_root, b"memory")
            .unwrap()
            .unwrap();
        assert_eq!(body.len(), 4 + 2 * POSTING_LEN);
        let decoded = Postings::decode(&body, page_no, PostingsLayout::FixedWidth).unwrap();
        assert_eq!(decoded.entries.len(), 2);

        // Search works on the old layout, and the file reopens still as v3
        // (the version is a property of the file, never silently upgraded).
        let root = pager.header().fts_root_page;
        let hits = search(&pager, root, "memory", 10, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), 2);
        pager.close().unwrap();

        let mut pager = Pager::open(
            Arc::clone(&vfs),
            Path::new("memory.mind"),
            PagerOptions::default(),
        )
        .unwrap();
        assert_eq!(pager.header().format_version, 3);
        // A write by this build extends the old file in its own layout.
        let new_ids = index_all(&mut pager, &["fresh memory entry"]);
        contents.insert(new_ids[0], "fresh memory entry".to_owned());
        let meta = load_meta(&pager, pager.header().fts_root_page)
            .unwrap()
            .unwrap();
        let (body, page_no) = dict::get(&pager, FTS_DICT, meta.dict_root, b"memory")
            .unwrap()
            .unwrap();
        assert_eq!(body.len(), 4 + 3 * POSTING_LEN);
        assert!(Postings::decode(&body, page_no, PostingsLayout::FixedWidth).is_ok());
        let root = pager.header().fts_root_page;
        let hits = search(&pager, root, "memory", 10, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), 3);
    }

    /// A file created by this build (`format_version` 7 — the filter-meta
    /// sidecar bump, which left postings untouched) stores postings in the
    /// version-6 skip layout — verified on the raw dictionary body. A small
    /// term carries `block_count = 0` (no skip index, plain delta+varint
    /// entries), so it costs just 4 bytes over the version-4 body and is
    /// byte-identical to the v5 small body.
    #[test]
    fn new_files_store_postings_as_delta_varint_skip() {
        let mut pager = pager(4096);
        assert_eq!(pager.header().format_version, crate::format::FORMAT_VERSION);
        assert_eq!(pager.header().format_version, 7);
        let ids = index_all(&mut pager, &["memory one", "memory two", "memory three"]);
        let meta = load_meta(&pager, pager.header().fts_root_page)
            .unwrap()
            .unwrap();
        let (body, page_no) = dict::get(&pager, FTS_DICT, meta.dict_root, b"memory")
            .unwrap()
            .unwrap();
        // Small term → block_count = 0, and still smaller than the fixed-width
        // footprint for 3 entries even with the 4-byte skip-count prefix.
        assert_eq!(le_block_count(&body), 0);
        assert!(body.len() < 4 + 3 * POSTING_LEN);
        // Decodes as the version-6 skip layout to exactly those ids.
        let decoded = Postings::decode(
            &body,
            page_no,
            PostingsLayout::DeltaVarintSkip(SkipEntry::V6),
        )
        .unwrap();
        let got: Vec<Ulid> = decoded.entries.iter().map(|p| p.record_id).collect();
        let mut expected = ids.clone();
        expected.sort();
        assert_eq!(got, expected);
    }

    /// Cross-version round-trip (BMW-1, `docs/adr/0024`): a `format_version` 5
    /// file — created before the per-block impact bound existed — keeps reading
    /// and writing the 24-byte-skip-entry layout under this (version-6) build,
    /// with a real skip index (large shared term), staying uniform so the build
    /// that wrote it can still read it back.
    #[test]
    fn format_version_5_file_reads_and_writes_v5_skip_entries() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let opts = PagerOptions {
            page_size: 4096,
            format_version: 5,
            ..Default::default()
        };
        let mut pager = Pager::create(Arc::clone(&vfs), Path::new("memory.mind"), opts).unwrap();
        let mut docs = Vec::new();
        for i in 0..(SKIP_MIN_DOC_FREQ + 20) {
            docs.push(format!("shared unique{i:04}"));
        }
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        index_all(&mut pager, &doc_refs);

        let meta = load_meta(&pager, pager.header().fts_root_page)
            .unwrap()
            .unwrap();
        let (body, page_no) = dict::get(&pager, FTS_DICT, meta.dict_root, b"shared")
            .unwrap()
            .unwrap();
        // A real skip index under the v5 (24-byte entry) layout, not v6.
        assert!(
            le_block_count(&body) > 0,
            "v5 file should carry a skip index"
        );
        let decoded = Postings::decode(
            &body,
            page_no,
            PostingsLayout::DeltaVarintSkip(SkipEntry::V5),
        )
        .unwrap();
        assert_eq!(decoded.entries.len(), SKIP_MIN_DOC_FREQ + 20);
        // Every id resolves via the v5 skip lookup, matching the linear scan.
        for p in &decoded.entries {
            assert_eq!(
                lookup_via_skip(&body, page_no, p.record_id, SkipEntry::V5).unwrap(),
                Some(p.term_freq)
            );
        }
    }

    /// Cross-version round-trip (S26 part 2, `docs/adr/0022`): a
    /// `format_version` 4 file — created before the skip layout existed —
    /// keeps reading and writing the skip-less delta+varint layout under this
    /// (version-5) build, staying uniform so the build that wrote it can still
    /// read it back. This is the "old file always readable" guarantee.
    #[test]
    fn format_version_4_file_reads_and_writes_skipless_postings() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let opts = PagerOptions {
            page_size: 4096,
            format_version: 4,
            ..Default::default()
        };
        let mut pager = Pager::create(Arc::clone(&vfs), Path::new("memory.mind"), opts).unwrap();
        // Enough shared-term entries that a version-5 file *would* carry a skip
        // index — proving the v4 file deliberately does not.
        let mut docs = Vec::new();
        for i in 0..(SKIP_MIN_DOC_FREQ + 20) {
            docs.push(format!("shared unique{i:04}"));
        }
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ids = index_all(&mut pager, &doc_refs);
        let mut contents = std::collections::HashMap::new();
        for (id, d) in ids.iter().zip(&docs) {
            contents.insert(*id, d.clone());
        }

        // The stored "shared" body is plain delta+varint (no block_count
        // prefix): it decodes as DeltaVarint and must *fail* as DeltaVarintSkip
        // only by coincidence — instead we assert it decodes to the full list
        // under the v4 layout, which is the file's declared layout.
        let meta = load_meta(&pager, pager.header().fts_root_page)
            .unwrap()
            .unwrap();
        let (body, page_no) = dict::get(&pager, FTS_DICT, meta.dict_root, b"shared")
            .unwrap()
            .unwrap();
        let decoded = Postings::decode(&body, page_no, PostingsLayout::DeltaVarint).unwrap();
        assert_eq!(decoded.entries.len(), SKIP_MIN_DOC_FREQ + 20);

        let root = pager.header().fts_root_page;
        let hits = search(&pager, root, "shared", 5000, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), SKIP_MIN_DOC_FREQ + 20);
        pager.close().unwrap();

        // Reopen: still v4, and a write extends it in the same skip-less layout.
        let mut pager = Pager::open(
            Arc::clone(&vfs),
            Path::new("memory.mind"),
            PagerOptions::default(),
        )
        .unwrap();
        assert_eq!(pager.header().format_version, 4);
        let new_ids = index_all(&mut pager, &["shared fresh entry"]);
        contents.insert(new_ids[0], "shared fresh entry".to_owned());
        let meta = load_meta(&pager, pager.header().fts_root_page)
            .unwrap()
            .unwrap();
        let (body, page_no) = dict::get(&pager, FTS_DICT, meta.dict_root, b"shared")
            .unwrap()
            .unwrap();
        assert!(Postings::decode(&body, page_no, PostingsLayout::DeltaVarint).is_ok());
        let root = pager.header().fts_root_page;
        let hits = search(&pager, root, "shared", 5000, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), SKIP_MIN_DOC_FREQ + 21);
    }

    /// A version-5 file's large postings body genuinely carries a skip index
    /// (block_count > 0), and `search` returns the full list through it.
    #[test]
    fn new_files_store_large_postings_with_a_skip_index() {
        let mut pager = pager(4096);
        let mut docs = Vec::new();
        for i in 0..(SKIP_MIN_DOC_FREQ + 40) {
            docs.push(format!("shared unique{i:04}"));
        }
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ids = index_all(&mut pager, &doc_refs);
        let mut contents = std::collections::HashMap::new();
        for (id, d) in ids.iter().zip(&docs) {
            contents.insert(*id, d.clone());
        }
        let meta = load_meta(&pager, pager.header().fts_root_page)
            .unwrap()
            .unwrap();
        let (body, _) = dict::get(&pager, FTS_DICT, meta.dict_root, b"shared")
            .unwrap()
            .unwrap();
        assert!(
            le_block_count(&body) > 0,
            "large term must carry a skip index"
        );
        let root = pager.header().fts_root_page;
        let hits = search(&pager, root, "shared", 5000, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), SKIP_MIN_DOC_FREQ + 40);
    }

    /// FTS_POSTINGS overflow chains under the new layout: enough shared-term
    /// entries to spill past the inline cap, then read back intact.
    #[test]
    fn delta_varint_postings_survive_overflow_chains() {
        let mut pager = pager(512);
        let mut docs = Vec::new();
        for i in 0..120 {
            docs.push(format!("shared corpus word plus unique{i:03}"));
        }
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ids = index_all(&mut pager, &doc_refs);
        let mut contents = std::collections::HashMap::new();
        for (id, doc) in ids.iter().zip(&docs) {
            contents.insert(*id, doc.clone());
        }
        let root = pager.header().fts_root_page;
        // "shared" appears in all 120 docs: at 512-byte pages its postings
        // body far exceeds the inline cap, so it lives in an FTS_POSTINGS
        // chain — and must come back complete.
        let hits = search(&pager, root, "shared", 500, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), 120);
    }

    #[test]
    fn index_within_txn_is_searchable_before_commit() {
        let mut pager = pager(4096);
        let id = Ulid::new();
        let mut txn = pager.begin().unwrap();
        index_document(&mut txn, id, "in flight transaction text").unwrap();
        let root = txn.fts_root_page();
        let mut contents = std::collections::HashMap::new();
        contents.insert(id, "in flight transaction text".to_owned());
        let hits = search(&txn, root, "flight", 5, |_| true, len_of(&contents)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, id);
        drop(txn); // rollback
        assert_eq!(pager.header().fts_root_page, 0);
    }

    #[test]
    fn decode_never_panics_on_arbitrary_bytes() {
        let mut rng = SplitMix64(0xF75_u64);
        for _ in 0..3000 {
            let len = [64usize, 200, 512, 4096][(rng.next_u64() % 4) as usize];
            let mut page = vec![0u8; len];
            for b in &mut page {
                *b = rng.next_u64() as u8;
            }
            fuzz_decode_page(&page); // must return, never panic
        }
        // Mutated valid leaf/meta pages exercise deeper branches.
        let entry = dict::LeafEntry {
            key: b"rust".to_vec(),
            value: dict::Value::Overflow {
                total_len: 40,
                first_page: 5,
            },
        };
        let valid = dict::encode_leaf(std::slice::from_ref(&entry), 512, FTS_DICT).unwrap();
        let meta = FtsMeta {
            doc_count: 3,
            total_tokens: 30,
            dict_root: 7,
        }
        .encode(512)
        .unwrap();
        for base in [valid, meta] {
            for _ in 0..2000 {
                let mut page = base.clone();
                let i = (rng.next_u64() as usize) % page.len();
                page[i] ^= (rng.next_u64() as u8) | 1;
                fuzz_decode_page(&page);
            }
        }
    }
}
