//! Shared byte-keyed paged dictionary B-tree — the structure behind both the
//! full-text dictionary (`docs/FORMAT.md` §11, ADR 0011) and the graph
//! dictionary (`docs/FORMAT.md` §12, ADR 0012).
//!
//! Extracted from the full-text index so the graph layer reuses one
//! battle-tested tree instead of a second implementation of the most delicate
//! code in the format (slotted pages, provably-safe midpoint splits, overflow
//! chains). The extraction is byte-identical for FTS pages: only the page
//! types, the key space, and the value bodies differ between instances, all
//! carried by [`DictSpec`].
//!
//! Layout per node (after the common 16-byte page header):
//!
//! - a **node-kind byte**: [`NODE_META`] (caller-owned meta page sharing the
//!   dict page type), [`NODE_INNER`], or [`NODE_LEAF`];
//! - **inner** (kind 1): `rightmost_child` (u64), then `entry_count` entries
//!   of `key_len` (u16) · key · `child` (u64), sorted strictly ascending.
//!   `child` covers keys `<= key`; `rightmost_child` covers the rest.
//! - **leaf** (kind 2): `entry_count` entries of `key_len` (u16) · key ·
//!   value. A value is a 1-byte tag + payload: tag `0` = inline (`u32` body
//!   length + body); tag `1` = overflow (`total_len` u32 · `first_page` u64
//!   into a chain of `spec.overflow` pages).
//!
//! An entry's footprint is capped at `usable/4` (bodies above the matching
//! inline limit spill to overflow), which makes leaf splits provably safe by
//! the same midpoint argument as the record B-tree (`docs/FORMAT.md` §5.1).
//! Replacing a value that had an overflow chain orphans the old chain until
//! `vacuum` — the same documented leak as the record B-tree.

use crate::error::{Error, Result};
use crate::format::{PAGE_HEADER_LEN, PAGE_TRAILER_LEN, PageHeader, PageType, stamp_page_checksum};
use crate::storage::btree::PageSource;
use crate::storage::pager::Txn;

/// One dictionary instance's page types and key bound. The `max_key_len`
/// feeds the inline-value cap: the guarantee "every leaf entry fits in
/// `usable/4`" holds for any key up to this length, so [`upsert`] rejects
/// longer keys with a typed error instead of corrupting the invariant.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DictSpec {
    /// Page type of meta/inner/leaf dictionary nodes.
    pub dict: PageType,
    /// Page type of the value overflow chain.
    pub overflow: PageType,
    /// Longest key the owning index ever inserts, in bytes.
    pub max_key_len: usize,
}

/// Node-kind byte at the first body offset of every dictionary page. Meta
/// pages belong to the owning index (corpus stats etc.) but share the dict
/// page type; [`decode_node`] only accepts inner/leaf.
pub(crate) const NODE_META: u8 = 0;
pub(crate) const NODE_INNER: u8 = 1;
pub(crate) const NODE_LEAF: u8 = 2;

/// Leaf value tags (first byte of a leaf entry's value).
const VALUE_INLINE: u8 = 0;
const VALUE_OVERFLOW: u8 = 1;

/// Depth cap while descending — a healthy tree is a handful of levels deep;
/// anything past this is a corrupt file (pointer cycle), reported as a typed
/// error instead of looping forever (mirrors the record B-tree's `MAX_DEPTH`).
const MAX_DEPTH: usize = 64;

fn malformed(page_no: u64, what: &'static str) -> Error {
    Error::MalformedPage { page_no, what }
}

// ---------------------------------------------------------------------------
// Node model
// ---------------------------------------------------------------------------

/// A leaf value: body inline, or a pointer to its overflow chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
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
pub(crate) struct LeafEntry {
    pub key: Vec<u8>,
    pub value: Value,
}

impl LeafEntry {
    /// key-len (u16) + key bytes + value.
    fn footprint(&self) -> usize {
        2 + self.key.len() + self.value.encoded_len()
    }
}

#[derive(Debug)]
struct InnerNode {
    /// `(separator_key, child)`: child covers keys `<= separator`. Sorted.
    entries: Vec<(Vec<u8>, u64)>,
    /// Child for keys greater than every separator.
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
/// two halves after any upsert provably fit (`docs/FORMAT.md` §5.1).
fn max_entry_footprint(page_size: u32) -> usize {
    usable(page_size) / 4
}

/// Largest value body stored inline; larger ones overflow. Derived from
/// [`max_entry_footprint`] minus the worst-case key and framing overhead.
fn max_inline_body(spec: DictSpec, page_size: u32) -> usize {
    // footprint = 2 (key len) + key + 1 (tag) + 4 (len) + body.
    max_entry_footprint(page_size).saturating_sub(2 + spec.max_key_len + 1 + 4)
}

// ---------------------------------------------------------------------------
// Node (de)serialization
// ---------------------------------------------------------------------------

fn decode_node(page: &[u8], page_no: u64, spec: DictSpec) -> Result<Node> {
    let header = PageHeader::decode(page).ok_or_else(|| malformed(page_no, "dict page header"))?;
    if header.page_type != spec.dict {
        return Err(malformed(page_no, "not a dict page"));
    }
    let content_end = page.len() - PAGE_TRAILER_LEN;
    let mut off = PAGE_HEADER_LEN;
    let kind = *page
        .get(off)
        .ok_or_else(|| malformed(page_no, "dict node kind"))?;
    off += 1;
    let n = header.entry_count as usize;
    match kind {
        NODE_LEAF => {
            let mut entries = Vec::with_capacity(n.min(1024));
            let mut prev: Option<Vec<u8>> = None;
            for _ in 0..n {
                let key = get_bytes_u16(page, &mut off, content_end, page_no, "dict leaf key")?;
                if prev.as_ref().is_some_and(|p| p.as_slice() >= key) {
                    return Err(malformed(page_no, "unsorted dict leaf keys"));
                }
                let tag = *page
                    .get(off)
                    .ok_or_else(|| malformed(page_no, "dict value tag"))?;
                off += 1;
                let value = match tag {
                    VALUE_INLINE => {
                        let body = get_bytes_u32(
                            page,
                            &mut off,
                            content_end,
                            page_no,
                            "dict inline value",
                        )?;
                        Value::Inline(body.to_vec())
                    }
                    VALUE_OVERFLOW => {
                        let total_len = get_u32(page, &mut off, page_no)?;
                        let first_page = get_u64(page, &mut off, page_no)?;
                        if first_page == 0 {
                            return Err(malformed(page_no, "dict null overflow page"));
                        }
                        Value::Overflow {
                            total_len,
                            first_page,
                        }
                    }
                    _ => return Err(malformed(page_no, "dict value tag")),
                };
                prev = Some(key.to_vec());
                entries.push(LeafEntry {
                    key: key.to_vec(),
                    value,
                });
            }
            Ok(Node::Leaf(entries))
        }
        NODE_INNER => {
            if n == 0 {
                return Err(malformed(page_no, "empty dict inner node"));
            }
            let rightmost = get_u64(page, &mut off, page_no)?;
            if rightmost == 0 {
                return Err(malformed(page_no, "dict null rightmost child"));
            }
            let mut entries = Vec::with_capacity(n.min(1024));
            let mut prev: Option<Vec<u8>> = None;
            for _ in 0..n {
                let key = get_bytes_u16(page, &mut off, content_end, page_no, "dict inner key")?;
                if prev.as_ref().is_some_and(|p| p.as_slice() >= key) {
                    return Err(malformed(page_no, "unsorted dict inner keys"));
                }
                let sep = key.to_vec();
                let child = get_u64(page, &mut off, page_no)?;
                if child == 0 {
                    return Err(malformed(page_no, "dict null child"));
                }
                prev = Some(sep.clone());
                entries.push((sep, child));
            }
            Ok(Node::Inner(InnerNode { entries, rightmost }))
        }
        _ => Err(malformed(page_no, "unexpected dict node kind")),
    }
}

/// Encodes a leaf. `None` = does not fit at this page size (caller splits).
pub(crate) fn encode_leaf(
    entries: &[LeafEntry],
    page_size: u32,
    spec: DictSpec,
) -> Option<Vec<u8>> {
    let mut body_len = 1; // node kind
    for e in entries {
        body_len += e.footprint();
    }
    if PAGE_HEADER_LEN + body_len > page_size as usize - PAGE_TRAILER_LEN {
        return None;
    }
    let mut page = vec![0u8; page_size as usize];
    PageHeader {
        page_type: spec.dict,
        entry_count: entries.len() as u32,
        next_page: 0,
    }
    .encode_into(&mut page);
    let mut off = PAGE_HEADER_LEN;
    page[off] = NODE_LEAF;
    off += 1;
    for e in entries {
        put_bytes_u16(&mut page, &mut off, &e.key);
        match &e.value {
            Value::Inline(body) => {
                page[off] = VALUE_INLINE;
                off += 1;
                put_bytes_u32(&mut page, &mut off, body);
            }
            Value::Overflow {
                total_len,
                first_page,
            } => {
                page[off] = VALUE_OVERFLOW;
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
fn encode_inner(node: &InnerNode, page_size: u32, spec: DictSpec) -> Option<Vec<u8>> {
    if node.entries.is_empty() {
        return None;
    }
    let mut body_len = 1 + 8; // node kind + rightmost
    for (key, _) in &node.entries {
        body_len += 2 + key.len() + 8;
    }
    if PAGE_HEADER_LEN + body_len > page_size as usize - PAGE_TRAILER_LEN {
        return None;
    }
    let mut page = vec![0u8; page_size as usize];
    PageHeader {
        page_type: spec.dict,
        entry_count: node.entries.len() as u32,
        next_page: 0,
    }
    .encode_into(&mut page);
    let mut off = PAGE_HEADER_LEN;
    page[off] = NODE_INNER;
    off += 1;
    put_u64(&mut page, &mut off, node.rightmost);
    for (key, child) in &node.entries {
        put_bytes_u16(&mut page, &mut off, key);
        put_u64(&mut page, &mut off, *child);
    }
    stamp_page_checksum(&mut page);
    Some(page)
}

// ---------------------------------------------------------------------------
// Overflow chains
// ---------------------------------------------------------------------------

/// Overflow-chain payload capacity of one page.
fn overflow_capacity(page_size: u32) -> usize {
    page_size as usize - PAGE_HEADER_LEN - PAGE_TRAILER_LEN
}

/// Writes a value body into a fresh overflow chain; returns the head page.
fn write_overflow_chain(txn: &mut Txn<'_>, body: &[u8], spec: DictSpec) -> Result<u64> {
    let cap = overflow_capacity(txn.page_size());
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
            page_type: spec.overflow,
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
        .ok_or(Error::Internal("empty dict overflow chain"))
}

/// Reads an overflow chain of exactly `total_len` bytes. Bounded: each hop
/// consumes at least one payload byte, so a cycle cannot loop forever.
fn read_overflow_chain(
    src: &dyn PageSource,
    first_page: u64,
    total_len: u32,
    spec: DictSpec,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut remaining = total_len as usize;
    let mut page_no = first_page;
    loop {
        let page = src.page(page_no)?;
        let header =
            PageHeader::decode(&page).ok_or_else(|| malformed(page_no, "dict overflow header"))?;
        if header.page_type != spec.overflow {
            return Err(malformed(page_no, "not a dict overflow page"));
        }
        let used = header.entry_count as usize;
        if used > remaining || used > overflow_capacity(src.page_size()) {
            return Err(malformed(page_no, "dict overflow payload length"));
        }
        let payload = page
            .get(PAGE_HEADER_LEN..PAGE_HEADER_LEN + used)
            .ok_or_else(|| malformed(page_no, "dict overflow payload bounds"))?;
        out.extend_from_slice(payload);
        remaining -= used;
        if remaining == 0 {
            break;
        }
        if header.next_page == 0 {
            return Err(malformed(page_no, "broken dict overflow chain"));
        }
        page_no = header.next_page;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Lookup / upsert
// ---------------------------------------------------------------------------

/// Reads a key's value body from the dictionary, or `None` if absent. The
/// returned page number (the leaf for inline values, the chain head for
/// overflowed ones) gives the caller error context when decoding the body.
pub(crate) fn get(
    src: &dyn PageSource,
    spec: DictSpec,
    root: u64,
    key: &[u8],
) -> Result<Option<(Vec<u8>, u64)>> {
    if root == 0 {
        return Ok(None);
    }
    let mut page_no = root;
    for _ in 0..MAX_DEPTH {
        let page = src.page(page_no)?;
        match decode_node(&page, page_no, spec)? {
            Node::Inner(node) => page_no = child_for(&node, key),
            Node::Leaf(entries) => {
                return match entries.binary_search_by(|e| e.key.as_slice().cmp(key)) {
                    Ok(i) => match &entries[i].value {
                        Value::Inline(b) => Ok(Some((b.clone(), page_no))),
                        Value::Overflow {
                            total_len,
                            first_page,
                        } => Ok(Some((
                            read_overflow_chain(src, *first_page, *total_len, spec)?,
                            *first_page,
                        ))),
                    },
                    Err(_) => Ok(None),
                };
            }
        }
    }
    Err(malformed(page_no, "dict tree deeper than MAX_DEPTH"))
}

fn child_for(node: &InnerNode, key: &[u8]) -> u64 {
    match node.entries.iter().find(|(sep, _)| key <= sep.as_slice()) {
        Some((_, child)) => *child,
        None => node.rightmost,
    }
}

/// Builds the leaf [`Value`] for a body, spilling to an overflow chain when
/// it is too large to inline.
fn make_value(txn: &mut Txn<'_>, body: &[u8], spec: DictSpec) -> Result<Value> {
    if body.len() <= max_inline_body(spec, txn.page_size()) {
        Ok(Value::Inline(body.to_vec()))
    } else {
        let total_len = u32::try_from(body.len())
            .map_err(|_| Error::InvalidArgument("dict value exceeds u32"))?;
        let first_page = write_overflow_chain(txn, body, spec)?;
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

/// Inserts or replaces `key → body`, returning the (possibly moved) root.
/// Replacing a value that had an overflow chain orphans the old chain until
/// `vacuum` (same documented leak as the record B-tree).
pub(crate) fn upsert(
    txn: &mut Txn<'_>,
    spec: DictSpec,
    root: u64,
    key: &[u8],
    body: &[u8],
) -> Result<u64> {
    if key.len() > spec.max_key_len {
        return Err(Error::InvalidArgument("dict key exceeds its length cap"));
    }
    let value = make_value(txn, body, spec)?;
    let page_size = txn.page_size();
    if root == 0 {
        let page_no = txn.allocate_page()?;
        let entry = LeafEntry {
            key: key.to_vec(),
            value,
        };
        let page = encode_leaf(std::slice::from_ref(&entry), page_size, spec)
            .ok_or(Error::Internal("fresh dict leaf does not fit"))?;
        txn.write_page(page_no, &page)?;
        return Ok(page_no);
    }
    match insert_rec(txn, root, key, value, 0, spec)? {
        Ins::Fit => Ok(root),
        Ins::Split { sep, right } => {
            let new_root = txn.allocate_page()?;
            let node = InnerNode {
                entries: vec![(sep, root)],
                rightmost: right,
            };
            let page = encode_inner(&node, page_size, spec)
                .ok_or(Error::Internal("fresh dict root does not fit"))?;
            txn.write_page(new_root, &page)?;
            Ok(new_root)
        }
    }
}

fn insert_rec(
    txn: &mut Txn<'_>,
    page_no: u64,
    key: &[u8],
    value: Value,
    depth: usize,
    spec: DictSpec,
) -> Result<Ins> {
    if depth >= MAX_DEPTH {
        return Err(malformed(page_no, "dict tree deeper than MAX_DEPTH"));
    }
    let page_size = txn.page_size();
    let page = txn.read_page(page_no)?;
    match decode_node(&page, page_no, spec)? {
        Node::Leaf(mut entries) => {
            match entries.binary_search_by(|e| e.key.as_slice().cmp(key)) {
                Ok(i) => entries[i].value = value,
                Err(i) => entries.insert(
                    i,
                    LeafEntry {
                        key: key.to_vec(),
                        value,
                    },
                ),
            }
            if let Some(encoded) = encode_leaf(&entries, page_size, spec) {
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
                return Err(Error::Internal("dict leaf split produced an empty half"));
            }
            let sep = entries
                .last()
                .ok_or(Error::Internal("dict leaf split empty left"))?
                .key
                .clone();
            let (Some(left_page), Some(right_page)) = (
                encode_leaf(&entries, page_size, spec),
                encode_leaf(&right_entries, page_size, spec),
            ) else {
                return Err(Error::Internal("dict leaf split does not fit"));
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
                .position(|(sep, _)| key <= sep.as_slice())
                .unwrap_or(node.entries.len());
            let child = match node.entries.get(idx) {
                Some((_, c)) => *c,
                None => node.rightmost,
            };
            match insert_rec(txn, child, key, value, depth + 1, spec)? {
                Ins::Fit => Ok(Ins::Fit),
                Ins::Split { sep, right } => {
                    match node.entries.get_mut(idx) {
                        Some(entry) => entry.1 = right,
                        None => node.rightmost = right,
                    }
                    node.entries.insert(idx, (sep, child));
                    if let Some(encoded) = encode_inner(&node, page_size, spec) {
                        txn.write_page(page_no, &encoded)?;
                        return Ok(Ins::Fit);
                    }
                    let m = node.entries.len() / 2;
                    let right_entries = node.entries.split_off(m + 1);
                    let (promoted_key, promoted_child) = node
                        .entries
                        .pop()
                        .ok_or(Error::Internal("dict inner split underflow"))?;
                    let right_node = InnerNode {
                        entries: right_entries,
                        rightmost: node.rightmost,
                    };
                    node.rightmost = promoted_child;
                    let (Some(left_page), Some(right_page)) = (
                        encode_inner(&node, page_size, spec),
                        encode_inner(&right_node, page_size, spec),
                    ) else {
                        return Err(Error::Internal("dict inner split does not fit"));
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

/// Fuzz surface: decode one page as an inner/leaf node of `spec`. Must
/// return, never panic, on arbitrary bytes (`docs/TESTING.md` §3).
pub(crate) fn fuzz_decode_node(page: &[u8], spec: DictSpec) {
    let _ = decode_node(page, 1, spec);
}

// ---------------------------------------------------------------------------
// Little-endian read/write helpers (bounds-checked; no panics). Shared with
// the owning indexes for their meta pages and value bodies.
// ---------------------------------------------------------------------------

pub(crate) fn put_u16(buf: &mut [u8], off: &mut usize, v: u16) {
    if let Some(dst) = buf.get_mut(*off..*off + 2) {
        dst.copy_from_slice(&v.to_le_bytes());
    }
    *off += 2;
}

pub(crate) fn put_u32(buf: &mut [u8], off: &mut usize, v: u32) {
    if let Some(dst) = buf.get_mut(*off..*off + 4) {
        dst.copy_from_slice(&v.to_le_bytes());
    }
    *off += 4;
}

pub(crate) fn put_u64(buf: &mut [u8], off: &mut usize, v: u64) {
    if let Some(dst) = buf.get_mut(*off..*off + 8) {
        dst.copy_from_slice(&v.to_le_bytes());
    }
    *off += 8;
}

pub(crate) fn put_bytes_u16(buf: &mut [u8], off: &mut usize, v: &[u8]) {
    let len = v.len() as u16;
    put_u16(buf, off, len);
    if let Some(dst) = buf.get_mut(*off..*off + v.len()) {
        dst.copy_from_slice(v);
    }
    *off += v.len();
}

pub(crate) fn put_bytes_u32(buf: &mut [u8], off: &mut usize, v: &[u8]) {
    let len = v.len() as u32;
    put_u32(buf, off, len);
    if let Some(dst) = buf.get_mut(*off..*off + v.len()) {
        dst.copy_from_slice(v);
    }
    *off += v.len();
}

pub(crate) fn read_u32(buf: &[u8], off: usize, page_no: u64) -> Result<u32> {
    buf.get(off..off + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| malformed(page_no, "dict short read"))
}

pub(crate) fn get_u32(buf: &[u8], off: &mut usize, page_no: u64) -> Result<u32> {
    let v = read_u32(buf, *off, page_no)?;
    *off += 4;
    Ok(v)
}

pub(crate) fn get_u64(buf: &[u8], off: &mut usize, page_no: u64) -> Result<u64> {
    let v = buf
        .get(*off..*off + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| malformed(page_no, "dict short read"))?;
    *off += 8;
    Ok(v)
}

/// Reads a u16-length-prefixed byte slice, validating the length against the
/// content bound before returning it (fuzz rule, `docs/TESTING.md` §3).
pub(crate) fn get_bytes_u16<'a>(
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
pub(crate) fn get_bytes_u32<'a>(
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::storage::pager::{Pager, PagerOptions};
    use crate::storage::sim::SimVfs;
    use crate::storage::vfs::Vfs;

    const SPEC: DictSpec = DictSpec {
        dict: PageType::FtsDict,
        overflow: PageType::FtsPostings,
        max_key_len: 128,
    };

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

    #[test]
    fn upsert_get_roundtrip_and_replace() {
        let mut pager = pager(4096);
        let mut txn = pager.begin().unwrap();
        let root = upsert(&mut txn, SPEC, 0, b"alpha", b"one").unwrap();
        let root = upsert(&mut txn, SPEC, root, b"beta", b"two").unwrap();
        let root = upsert(&mut txn, SPEC, root, b"alpha", b"replaced").unwrap();
        assert_eq!(
            get(&txn, SPEC, root, b"alpha").unwrap().unwrap().0,
            b"replaced"
        );
        assert_eq!(get(&txn, SPEC, root, b"beta").unwrap().unwrap().0, b"two");
        assert_eq!(get(&txn, SPEC, root, b"gamma").unwrap(), None);
        txn.commit().unwrap();
    }

    #[test]
    fn large_values_overflow_and_read_back() {
        let mut pager = pager(512);
        let big = vec![0xAB; 5000]; // several overflow pages at 512 B
        let mut txn = pager.begin().unwrap();
        let root = upsert(&mut txn, SPEC, 0, b"big", &big).unwrap();
        assert_eq!(get(&txn, SPEC, root, b"big").unwrap().unwrap().0, big);
        txn.commit().unwrap();
    }

    #[test]
    fn many_keys_force_splits_and_stay_findable() {
        let mut pager = pager(512);
        let mut txn = pager.begin().unwrap();
        let mut root = 0;
        for i in 0..300u32 {
            let key = format!("key{i:05}");
            root = upsert(&mut txn, SPEC, root, key.as_bytes(), &i.to_le_bytes()).unwrap();
        }
        for i in 0..300u32 {
            let key = format!("key{i:05}");
            let (body, _) = get(&txn, SPEC, root, key.as_bytes()).unwrap().unwrap();
            assert_eq!(body, i.to_le_bytes());
        }
        txn.commit().unwrap();
    }

    #[test]
    fn oversized_key_is_a_typed_error() {
        let mut pager = pager(4096);
        let mut txn = pager.begin().unwrap();
        let long_key = vec![b'k'; SPEC.max_key_len + 1];
        assert!(matches!(
            upsert(&mut txn, SPEC, 0, &long_key, b"v"),
            Err(Error::InvalidArgument(_))
        ));
    }
}
