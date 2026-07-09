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
//! - The **dictionary** is a slotted B-tree keyed by term bytes (variable
//!   length, sorted lexicographically). Its leaf cells hold the term's
//!   **postings**: `doc_freq` then a list of `(record_id, term_freq)` sorted
//!   by id. A postings list too large to inline spills to an `FTS_POSTINGS`
//!   overflow chain, exactly like an oversized record spills to `OVERFLOW`.
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

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::format::{PAGE_HEADER_LEN, PAGE_TRAILER_LEN, PageHeader, PageType, stamp_page_checksum};
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

/// Node-kind byte at the first body offset of every `FtsDict` page.
const NODE_META: u8 = 0;
const NODE_INNER: u8 = 1;
const NODE_LEAF: u8 = 2;

/// Depth cap while descending the dictionary — a healthy tree is a handful of
/// levels deep; anything past this is a corrupt file (pointer cycle),
/// reported as a typed error instead of looping forever (mirrors the record
/// B-tree's `MAX_DEPTH`).
const MAX_DEPTH: usize = 64;

/// Postings cell tags (first byte of a dictionary leaf value).
const POSTINGS_INLINE: u8 = 0;
const POSTINGS_OVERFLOW: u8 = 1;

/// Bytes per posting entry on disk: `record_id` (16) + `term_freq` (u32).
const POSTING_LEN: usize = 20;

fn malformed(page_no: u64, what: &'static str) -> Error {
    Error::MalformedPage { page_no, what }
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
        page[off] = NODE_META;
        off += 1;
        put_u64(&mut page, &mut off, self.doc_count);
        put_u64(&mut page, &mut off, self.total_tokens);
        put_u64(&mut page, &mut off, self.dict_root);
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
        if page.get(off).copied() != Some(NODE_META) {
            return Err(malformed(page_no, "not an FTS meta page"));
        }
        off += 1;
        let doc_count = get_u64(page, &mut off, page_no)?;
        let total_tokens = get_u64(page, &mut off, page_no)?;
        let dict_root = get_u64(page, &mut off, page_no)?;
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

    /// Serialized body: `doc_freq` (u32) + `doc_freq` × posting entries.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.entries.len() * POSTING_LEN);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for p in &self.entries {
            out.extend_from_slice(&p.record_id.to_bytes());
            out.extend_from_slice(&p.term_freq.to_le_bytes());
        }
        out
    }

    /// Parses a postings body. Validates the count against the buffer before
    /// allocating (fuzz rule, `docs/TESTING.md` §3) and rejects unsorted or
    /// duplicate ids (a corrupt or hostile page).
    fn decode(body: &[u8], page_no: u64) -> Result<Self> {
        let count = read_u32(body, 0, page_no)? as usize;
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
            let term_freq = read_u32(body, off + 16, page_no)?;
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
}

/// Overflow-chain payload capacity of one `FTS_POSTINGS` page.
fn postings_capacity(page_size: u32) -> usize {
    page_size as usize - PAGE_HEADER_LEN - PAGE_TRAILER_LEN
}

/// Writes a postings body into a fresh `FTS_POSTINGS` chain; returns the head.
fn write_postings_chain(txn: &mut Txn<'_>, body: &[u8]) -> Result<u64> {
    let cap = postings_capacity(txn.page_size());
    let chunks: Vec<&[u8]> = if body.is_empty() {
        vec![&body[..0]]
    } else {
        body.chunks(cap).collect()
    };
    let mut pages = Vec::with_capacity(chunks.len());
    for _ in &chunks {
        pages.push(txn.allocate_page()?);
    }
    let page_size = txn.page_size() as usize;
    for (i, chunk) in chunks.iter().enumerate() {
        let mut page = vec![0u8; page_size];
        PageHeader {
            page_type: PageType::FtsPostings,
            entry_count: chunk.len() as u32,
            next_page: pages.get(i + 1).copied().unwrap_or(0),
        }
        .encode_into(&mut page);
        page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        stamp_page_checksum(&mut page);
        txn.write_page(pages[i], &page)?;
    }
    pages
        .first()
        .copied()
        .ok_or(Error::Internal("empty fts postings chain"))
}

/// Reads an `FTS_POSTINGS` chain of exactly `total_len` bytes. Bounded: each
/// hop consumes at least one payload byte, so a cycle cannot loop forever.
fn read_postings_chain(src: &dyn PageSource, first_page: u64, total_len: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut remaining = total_len as usize;
    let mut page_no = first_page;
    loop {
        let page = src.page(page_no)?;
        let header =
            PageHeader::decode(&page).ok_or_else(|| malformed(page_no, "fts postings header"))?;
        if header.page_type != PageType::FtsPostings {
            return Err(malformed(page_no, "not an fts postings page"));
        }
        let used = header.entry_count as usize;
        if used > remaining || used > postings_capacity(src.page_size()) {
            return Err(malformed(page_no, "fts postings payload length"));
        }
        let payload = page
            .get(PAGE_HEADER_LEN..PAGE_HEADER_LEN + used)
            .ok_or_else(|| malformed(page_no, "fts postings payload bounds"))?;
        out.extend_from_slice(payload);
        remaining -= used;
        if remaining == 0 {
            break;
        }
        if header.next_page == 0 {
            return Err(malformed(page_no, "broken fts postings chain"));
        }
        page_no = header.next_page;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Dictionary node (de)serialization
// ---------------------------------------------------------------------------

/// A dictionary leaf value: postings inline, or a pointer to their chain.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Inline(Vec<u8>),
    Overflow { total_len: u32, first_page: u64 },
}

impl Value {
    fn encoded_len(&self) -> usize {
        match self {
            // tag + u32 len + body
            Value::Inline(b) => 1 + 4 + b.len(),
            // tag + u32 total_len + u64 first_page
            Value::Overflow { .. } => 1 + 4 + 8,
        }
    }
}

#[derive(Debug, Clone)]
struct LeafEntry {
    term: Vec<u8>,
    value: Value,
}

impl LeafEntry {
    /// term-len (u16) + term bytes + value.
    fn footprint(&self) -> usize {
        2 + self.term.len() + self.value.encoded_len()
    }
}

#[derive(Debug)]
struct InnerNode {
    /// `(separator_term, child)`: child covers terms `<= separator`. Sorted.
    entries: Vec<(Vec<u8>, u64)>,
    /// Child for terms greater than every separator.
    rightmost: u64,
}

#[derive(Debug)]
enum Node {
    Leaf(Vec<LeafEntry>),
    Inner(InnerNode),
}

/// Usable content bytes on a page (after common header + checksum trailer).
fn usable(page_size: u32) -> usize {
    page_size as usize - PAGE_HEADER_LEN - PAGE_TRAILER_LEN
}

/// Cap on a single leaf entry's footprint: a quarter of the usable space, so
/// two halves after any upsert provably fit (same split argument as the
/// record B-tree, `docs/FORMAT.md` §5.1). Values above the matching inline cap
/// spill to an overflow chain, keeping every entry within this bound.
fn max_entry_footprint(page_size: u32) -> usize {
    usable(page_size) / 4
}

/// Largest postings body stored inline; larger ones overflow. Derived from
/// [`max_entry_footprint`] minus the worst-case term and framing overhead.
fn max_inline_postings(page_size: u32) -> usize {
    // footprint = 2 (term len) + term + 1 (tag) + 4 (len) + body.
    max_entry_footprint(page_size).saturating_sub(2 + MAX_TERM_LEN + 1 + 4)
}

fn decode_node(page: &[u8], page_no: u64) -> Result<Node> {
    let header = PageHeader::decode(page).ok_or_else(|| malformed(page_no, "fts page header"))?;
    if header.page_type != PageType::FtsDict {
        return Err(malformed(page_no, "not an FTS dict page"));
    }
    let content_end = page.len() - PAGE_TRAILER_LEN;
    let mut off = PAGE_HEADER_LEN;
    let kind = *page
        .get(off)
        .ok_or_else(|| malformed(page_no, "fts node kind"))?;
    off += 1;
    let n = header.entry_count as usize;
    match kind {
        NODE_LEAF => {
            let mut entries = Vec::with_capacity(n.min(1024));
            let mut prev: Option<Vec<u8>> = None;
            for _ in 0..n {
                let term = get_bytes_u16(page, &mut off, content_end, page_no, "fts leaf term")?;
                if prev.as_ref().is_some_and(|p| p.as_slice() >= term) {
                    return Err(malformed(page_no, "unsorted fts leaf terms"));
                }
                let tag = *page
                    .get(off)
                    .ok_or_else(|| malformed(page_no, "fts value tag"))?;
                off += 1;
                let value = match tag {
                    POSTINGS_INLINE => {
                        let body = get_bytes_u32(
                            page,
                            &mut off,
                            content_end,
                            page_no,
                            "fts inline value",
                        )?;
                        Value::Inline(body.to_vec())
                    }
                    POSTINGS_OVERFLOW => {
                        let total_len = get_u32(page, &mut off, page_no)?;
                        let first_page = get_u64(page, &mut off, page_no)?;
                        if first_page == 0 {
                            return Err(malformed(page_no, "fts null overflow page"));
                        }
                        Value::Overflow {
                            total_len,
                            first_page,
                        }
                    }
                    _ => return Err(malformed(page_no, "fts value tag")),
                };
                prev = Some(term.to_vec());
                entries.push(LeafEntry {
                    term: term.to_vec(),
                    value,
                });
            }
            Ok(Node::Leaf(entries))
        }
        NODE_INNER => {
            if n == 0 {
                return Err(malformed(page_no, "empty fts inner node"));
            }
            let rightmost = get_u64(page, &mut off, page_no)?;
            if rightmost == 0 {
                return Err(malformed(page_no, "fts null rightmost child"));
            }
            let mut entries = Vec::with_capacity(n.min(1024));
            let mut prev: Option<Vec<u8>> = None;
            for _ in 0..n {
                let term = get_bytes_u16(page, &mut off, content_end, page_no, "fts inner term")?;
                if prev.as_ref().is_some_and(|p| p.as_slice() >= term) {
                    return Err(malformed(page_no, "unsorted fts inner terms"));
                }
                let key = term.to_vec();
                let child = get_u64(page, &mut off, page_no)?;
                if child == 0 {
                    return Err(malformed(page_no, "fts null child"));
                }
                prev = Some(key.clone());
                entries.push((key, child));
            }
            Ok(Node::Inner(InnerNode { entries, rightmost }))
        }
        _ => Err(malformed(page_no, "unexpected fts node kind")),
    }
}

/// Encodes a leaf. `None` = does not fit at this page size (caller splits).
fn encode_leaf(entries: &[LeafEntry], page_size: u32) -> Option<Vec<u8>> {
    let mut body_len = 1; // node kind
    for e in entries {
        body_len += e.footprint();
    }
    if PAGE_HEADER_LEN + body_len > page_size as usize - PAGE_TRAILER_LEN {
        return None;
    }
    let mut page = vec![0u8; page_size as usize];
    PageHeader {
        page_type: PageType::FtsDict,
        entry_count: entries.len() as u32,
        next_page: 0,
    }
    .encode_into(&mut page);
    let mut off = PAGE_HEADER_LEN;
    page[off] = NODE_LEAF;
    off += 1;
    for e in entries {
        put_bytes_u16(&mut page, &mut off, &e.term);
        match &e.value {
            Value::Inline(body) => {
                page[off] = POSTINGS_INLINE;
                off += 1;
                put_bytes_u32(&mut page, &mut off, body);
            }
            Value::Overflow {
                total_len,
                first_page,
            } => {
                page[off] = POSTINGS_OVERFLOW;
                off += 1;
                put_u32(&mut page, &mut off, *total_len);
                put_u64(&mut page, &mut off, *first_page);
            }
        }
    }
    stamp_page_checksum(&mut page);
    Some(page)
}

/// Encodes an inner node. `None` = too many entries for this page size.
fn encode_inner(node: &InnerNode, page_size: u32) -> Option<Vec<u8>> {
    if node.entries.is_empty() {
        return None;
    }
    let mut body_len = 1 + 8; // node kind + rightmost
    for (term, _) in &node.entries {
        body_len += 2 + term.len() + 8;
    }
    if PAGE_HEADER_LEN + body_len > page_size as usize - PAGE_TRAILER_LEN {
        return None;
    }
    let mut page = vec![0u8; page_size as usize];
    PageHeader {
        page_type: PageType::FtsDict,
        entry_count: node.entries.len() as u32,
        next_page: 0,
    }
    .encode_into(&mut page);
    let mut off = PAGE_HEADER_LEN;
    page[off] = NODE_INNER;
    off += 1;
    put_u64(&mut page, &mut off, node.rightmost);
    for (term, child) in &node.entries {
        put_bytes_u16(&mut page, &mut off, term);
        put_u64(&mut page, &mut off, *child);
    }
    stamp_page_checksum(&mut page);
    Some(page)
}

// ---------------------------------------------------------------------------
// Dictionary lookup / insert
// ---------------------------------------------------------------------------

/// Reads a term's postings from the dictionary, or `None` if absent.
fn dict_get(src: &dyn PageSource, root: u64, term: &[u8]) -> Result<Option<Postings>> {
    if root == 0 {
        return Ok(None);
    }
    let mut page_no = root;
    for _ in 0..MAX_DEPTH {
        let page = src.page(page_no)?;
        match decode_node(&page, page_no)? {
            Node::Inner(node) => page_no = child_for(&node, term),
            Node::Leaf(entries) => {
                return match entries.binary_search_by(|e| e.term.as_slice().cmp(term)) {
                    Ok(i) => Ok(Some(resolve_value(src, &entries[i].value)?)),
                    Err(_) => Ok(None),
                };
            }
        }
    }
    Err(malformed(page_no, "fts tree deeper than MAX_DEPTH"))
}

fn child_for(node: &InnerNode, term: &[u8]) -> u64 {
    match node.entries.iter().find(|(sep, _)| term <= sep.as_slice()) {
        Some((_, child)) => *child,
        None => node.rightmost,
    }
}

fn resolve_value(src: &dyn PageSource, value: &Value) -> Result<Postings> {
    let (body, page_no) = match value {
        Value::Inline(b) => (b.clone(), 0),
        Value::Overflow {
            total_len,
            first_page,
        } => (
            read_postings_chain(src, *first_page, *total_len)?,
            *first_page,
        ),
    };
    Postings::decode(&body, page_no)
}

/// Builds the leaf [`Value`] for a postings list, spilling to an overflow
/// chain when it is too large to inline.
fn make_value(txn: &mut Txn<'_>, postings: &Postings) -> Result<Value> {
    let body = postings.encode();
    if body.len() <= max_inline_postings(txn.page_size()) {
        Ok(Value::Inline(body))
    } else {
        let total_len = u32::try_from(body.len())
            .map_err(|_| Error::InvalidArgument("fts postings exceed u32"))?;
        let first_page = write_postings_chain(txn, &body)?;
        Ok(Value::Overflow {
            total_len,
            first_page,
        })
    }
}

enum Ins {
    Fit,
    Split { sep: Vec<u8>, right: u64 },
}

/// Inserts or replaces `term → postings`, updating the dictionary root as
/// needed. Replacing a value that had an overflow chain orphans the old chain
/// until `vacuum` (same documented leak as the record B-tree).
fn dict_upsert(txn: &mut Txn<'_>, root: u64, term: &[u8], postings: &Postings) -> Result<u64> {
    let value = make_value(txn, postings)?;
    let page_size = txn.page_size();
    if root == 0 {
        let page_no = txn.allocate_page()?;
        let entry = LeafEntry {
            term: term.to_vec(),
            value,
        };
        let page = encode_leaf(std::slice::from_ref(&entry), page_size)
            .ok_or(Error::Internal("fresh fts leaf does not fit"))?;
        txn.write_page(page_no, &page)?;
        return Ok(page_no);
    }
    match dict_insert_rec(txn, root, term, value, 0)? {
        Ins::Fit => Ok(root),
        Ins::Split { sep, right } => {
            let new_root = txn.allocate_page()?;
            let node = InnerNode {
                entries: vec![(sep, root)],
                rightmost: right,
            };
            let page = encode_inner(&node, page_size)
                .ok_or(Error::Internal("fresh fts root does not fit"))?;
            txn.write_page(new_root, &page)?;
            Ok(new_root)
        }
    }
}

fn dict_insert_rec(
    txn: &mut Txn<'_>,
    page_no: u64,
    term: &[u8],
    value: Value,
    depth: usize,
) -> Result<Ins> {
    if depth >= MAX_DEPTH {
        return Err(malformed(page_no, "fts tree deeper than MAX_DEPTH"));
    }
    let page_size = txn.page_size();
    let page = txn.read_page(page_no)?;
    match decode_node(&page, page_no)? {
        Node::Leaf(mut entries) => {
            match entries.binary_search_by(|e| e.term.as_slice().cmp(term)) {
                Ok(i) => entries[i].value = value,
                Err(i) => entries.insert(
                    i,
                    LeafEntry {
                        term: term.to_vec(),
                        value,
                    },
                ),
            }
            if let Some(encoded) = encode_leaf(&entries, page_size) {
                txn.write_page(page_no, &encoded)?;
                return Ok(Ins::Fit);
            }
            // Split at the byte midpoint; every entry footprint is capped at
            // usable/4, so both halves provably fit (docs/FORMAT.md §5.1).
            let total: usize = entries.iter().map(LeafEntry::footprint).sum();
            let mut acc = 0;
            let mut cut = entries.len();
            for (i, e) in entries.iter().enumerate() {
                acc += e.footprint();
                if acc >= total / 2 {
                    cut = i + 1;
                    break;
                }
            }
            let right_entries = entries.split_off(cut);
            if right_entries.is_empty() {
                return Err(Error::Internal("fts leaf split produced an empty half"));
            }
            let sep = entries
                .last()
                .ok_or(Error::Internal("fts leaf split empty left"))?
                .term
                .clone();
            let (Some(left_page), Some(right_page)) = (
                encode_leaf(&entries, page_size),
                encode_leaf(&right_entries, page_size),
            ) else {
                return Err(Error::Internal("fts leaf split does not fit"));
            };
            let right = txn.allocate_page()?;
            txn.write_page(page_no, &left_page)?;
            txn.write_page(right, &right_page)?;
            Ok(Ins::Split { sep, right })
        }
        Node::Inner(mut node) => {
            let idx = node
                .entries
                .iter()
                .position(|(sep, _)| term <= sep.as_slice())
                .unwrap_or(node.entries.len());
            let child = match node.entries.get(idx) {
                Some((_, c)) => *c,
                None => node.rightmost,
            };
            match dict_insert_rec(txn, child, term, value, depth + 1)? {
                Ins::Fit => Ok(Ins::Fit),
                Ins::Split { sep, right } => {
                    match node.entries.get_mut(idx) {
                        Some(entry) => entry.1 = right,
                        None => node.rightmost = right,
                    }
                    node.entries.insert(idx, (sep, child));
                    if let Some(encoded) = encode_inner(&node, page_size) {
                        txn.write_page(page_no, &encoded)?;
                        return Ok(Ins::Fit);
                    }
                    let m = node.entries.len() / 2;
                    let right_entries = node.entries.split_off(m + 1);
                    let (promoted_key, promoted_child) = node
                        .entries
                        .pop()
                        .ok_or(Error::Internal("fts inner split underflow"))?;
                    let right_node = InnerNode {
                        entries: right_entries,
                        rightmost: node.rightmost,
                    };
                    node.rightmost = promoted_child;
                    let (Some(left_page), Some(right_page)) = (
                        encode_inner(&node, page_size),
                        encode_inner(&right_node, page_size),
                    ) else {
                        return Err(Error::Internal("fts inner split does not fit"));
                    };
                    let right = txn.allocate_page()?;
                    txn.write_page(page_no, &left_page)?;
                    txn.write_page(right, &right_page)?;
                    Ok(Ins::Split {
                        sep: promoted_key,
                        right,
                    })
                }
            }
        }
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

    let mut root = meta.dict_root;
    for (term, tf) in terms {
        let term_bytes = term.as_bytes();
        let mut postings = dict_get(txn, root, term_bytes)?.unwrap_or_default();
        postings.upsert(record_id, tf);
        root = dict_upsert(txn, root, term_bytes, &postings)?;
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
/// gone). Both closures are called at most once per candidate record.
pub fn search(
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

    // Accumulate BM25 across terms. `scores` sums per candidate; `lengths`
    // and `kept` memoize the per-record closures so each is hit at most once.
    let mut scores: HashMap<Ulid, f32> = HashMap::new();
    let mut lengths: HashMap<Ulid, Option<u32>> = HashMap::new();
    let mut kept: HashMap<Ulid, bool> = HashMap::new();

    for term in &query_terms {
        let Some(postings) = dict_get(src, meta.dict_root, term.as_bytes())? else {
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
            let id = p.record_id;
            if !*kept.entry(id).or_insert_with(|| keep(id)) {
                continue;
            }
            let dl = match lengths.entry(id) {
                std::collections::hash_map::Entry::Occupied(e) => *e.get(),
                std::collections::hash_map::Entry::Vacant(e) => *e.insert(doc_len(id)?),
            };
            let Some(dl) = dl else {
                continue; // record vanished; skip it
            };
            let tf = p.term_freq as f32;
            let norm = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl as f32 / avgdl.max(1.0));
            let contribution = idf * (tf * (BM25_K1 + 1.0)) / norm.max(f32::MIN_POSITIVE);
            *scores.entry(id).or_insert(0.0) += contribution;
        }
    }

    let mut hits: Vec<Hit> = scores
        .into_iter()
        .filter(|&(_, s)| s > 0.0)
        .map(|(record_id, score)| Hit { record_id, score })
        .collect();
    // Best score first; ties broken by id for a deterministic order (G3).
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    hits.truncate(k);
    Ok(hits)
}

/// Number of documents recorded in the full-text index (`embedmind stats`).
/// 0 when no index exists yet.
pub fn indexed_documents(src: &dyn PageSource, fts_root_page: u64) -> Result<u64> {
    Ok(load_meta(src, fts_root_page)?.map_or(0, |m| m.doc_count))
}

// ---------------------------------------------------------------------------
// Little-endian read/write helpers (bounds-checked; no panics)
// ---------------------------------------------------------------------------

fn put_u32(buf: &mut [u8], off: &mut usize, v: u32) {
    if let Some(dst) = buf.get_mut(*off..*off + 4) {
        dst.copy_from_slice(&v.to_le_bytes());
    }
    *off += 4;
}

fn put_u64(buf: &mut [u8], off: &mut usize, v: u64) {
    if let Some(dst) = buf.get_mut(*off..*off + 8) {
        dst.copy_from_slice(&v.to_le_bytes());
    }
    *off += 8;
}

fn put_bytes_u16(buf: &mut [u8], off: &mut usize, v: &[u8]) {
    let len = v.len() as u16;
    if let Some(dst) = buf.get_mut(*off..*off + 2) {
        dst.copy_from_slice(&len.to_le_bytes());
    }
    *off += 2;
    if let Some(dst) = buf.get_mut(*off..*off + v.len()) {
        dst.copy_from_slice(v);
    }
    *off += v.len();
}

fn put_bytes_u32(buf: &mut [u8], off: &mut usize, v: &[u8]) {
    let len = v.len() as u32;
    if let Some(dst) = buf.get_mut(*off..*off + 4) {
        dst.copy_from_slice(&len.to_le_bytes());
    }
    *off += 4;
    if let Some(dst) = buf.get_mut(*off..*off + v.len()) {
        dst.copy_from_slice(v);
    }
    *off += v.len();
}

fn read_u32(buf: &[u8], off: usize, page_no: u64) -> Result<u32> {
    buf.get(off..off + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| malformed(page_no, "fts short read"))
}

fn get_u32(buf: &[u8], off: &mut usize, page_no: u64) -> Result<u32> {
    let v = read_u32(buf, *off, page_no)?;
    *off += 4;
    Ok(v)
}

fn get_u64(buf: &[u8], off: &mut usize, page_no: u64) -> Result<u64> {
    let v = buf
        .get(*off..*off + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| malformed(page_no, "fts short read"))?;
    *off += 8;
    Ok(v)
}

/// Reads a u16-length-prefixed byte slice, validating the length against the
/// page content bound before returning it (fuzz rule, `docs/TESTING.md` §3).
fn get_bytes_u16<'a>(
    buf: &'a [u8],
    off: &mut usize,
    content_end: usize,
    page_no: u64,
    what: &'static str,
) -> Result<&'a [u8]> {
    let len = buf
        .get(*off..*off + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| malformed(page_no, what))? as usize;
    *off += 2;
    let end = off
        .checked_add(len)
        .filter(|&e| e <= content_end && e <= buf.len())
        .ok_or_else(|| malformed(page_no, what))?;
    let out = &buf[*off..end];
    *off = end;
    Ok(out)
}

/// Reads a u32-length-prefixed byte slice with the same bounds guarantee.
fn get_bytes_u32<'a>(
    buf: &'a [u8],
    off: &mut usize,
    content_end: usize,
    page_no: u64,
    what: &'static str,
) -> Result<&'a [u8]> {
    let len = read_u32(buf, *off, page_no)? as usize;
    *off += 4;
    let end = off
        .checked_add(len)
        .filter(|&e| e <= content_end && e <= buf.len())
        .ok_or_else(|| malformed(page_no, what))?;
    let out = &buf[*off..end];
    *off = end;
    Ok(out)
}

/// Fuzz-only surface: decode one page as each FTS node kind and as postings,
/// exercising every parser branch. Must return, never panic (`fuzz_fts_page`
/// target, `docs/TESTING.md` §3).
#[doc(hidden)]
pub fn fuzz_decode_page(page: &[u8]) {
    let _ = decode_node(page, 1);
    let _ = FtsMeta::decode(page, 1);
    // Postings bodies live at the page content region; try decoding the body.
    if page.len() > PAGE_HEADER_LEN {
        let _ = Postings::decode(&page[PAGE_HEADER_LEN..], 1);
    }
    let _ = Postings::decode(page, 1);
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
        let body = p.encode();
        assert_eq!(Postings::decode(&body, 1).unwrap(), p);

        // A hostile count with no payload must fail before allocating.
        let mut bad = 1_000_000u32.to_le_bytes().to_vec();
        bad.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            Postings::decode(&bad, 1),
            Err(Error::MalformedPage { .. })
        ));
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
        let entry = LeafEntry {
            term: b"rust".to_vec(),
            value: Value::Overflow {
                total_len: 40,
                first_page: 5,
            },
        };
        let valid = encode_leaf(std::slice::from_ref(&entry), 512).unwrap();
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
