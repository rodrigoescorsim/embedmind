//! Record B-tree (`docs/FORMAT.md` §5) — M1 item 1.2.
//!
//! Keys are 16-byte ULIDs (big-endian per the ULID spec, so lexicographic
//! byte order == time order == timeline). Values are opaque byte strings
//! (encoded [`crate::record::MemoryRecord`]s; the tree never looks inside).
//!
//! Layout on pages (exact bytes in FORMAT.md §5.1):
//! - **Leaves** (`BTREE_LEAF`): slotted — a sorted slot directory grows from
//!   the page header, cells grow from the tail. Values too large to inline
//!   spill to an `OVERFLOW` page chain and the cell keeps `(total_len,
//!   first_page)`.
//! - **Inner nodes** (`BTREE_INNER`): fixed 24-byte entries `(key, child)`
//!   where `child` covers keys `<= key`, plus a rightmost child for keys
//!   greater than every separator.
//!
//! There is no delete: `forget` is a tombstone update (`docs/adr/0003`).
//! Rewriting a value that had an overflow chain reuses the chain's pages in
//! place (allocating only for growth); dead space — tombstones and the tail
//! pages orphaned by shrinking rewrites — is reclaimed only by `embedmind
//! vacuum` (M2+). All decoding is fully
//! bounds-checked and panic-free: these parsers are fuzz targets
//! (`fuzz_page`, `docs/TESTING.md` §3).

use crate::error::{Error, Result};
use crate::format::{PAGE_HEADER_LEN, PAGE_TRAILER_LEN, PageHeader, PageType};
use crate::record::MAX_RECORD_LEN;
use crate::storage::pager::{Pager, Txn};

/// B-tree key: a ULID as bytes.
pub type Key = [u8; 16];

/// Read access to committed or transaction-local pages. Lets `get`/`scan`
/// run against either a [`Pager`] (committed state) or a [`Txn`] (its own
/// writes included).
pub trait PageSource {
    /// Reads one page, checksum-verified.
    fn page(&self, page_no: u64) -> Result<Vec<u8>>;
    /// Page size of the underlying store.
    fn page_size(&self) -> u32;
    /// On-disk `format_version` of the underlying store — version-dependent
    /// encodings (FTS postings, `docs/FORMAT.md` §11) select their layout
    /// from it, so a reader over an older file decodes that file's layout.
    fn format_version(&self) -> u32;
}

impl PageSource for Pager {
    fn page(&self, page_no: u64) -> Result<Vec<u8>> {
        self.read_page(page_no)
    }
    fn page_size(&self) -> u32 {
        self.header().page_size
    }
    fn format_version(&self) -> u32 {
        self.header().format_version
    }
}

impl PageSource for Txn<'_> {
    fn page(&self, page_no: u64) -> Result<Vec<u8>> {
        self.read_page(page_no)
    }
    fn page_size(&self) -> u32 {
        Txn::page_size(self)
    }
    fn format_version(&self) -> u32 {
        Txn::format_version(self)
    }
}

/// Depth cap while descending: a healthy tree is a handful of levels deep;
/// anything past this is a corrupt file (pointer cycle), reported as a typed
/// error instead of looping forever.
const MAX_DEPTH: usize = 64;

/// Slot directory entry size in a leaf: key (16) + cell offset (u16) +
/// cell length (u16).
const SLOT_LEN: usize = 20;

/// Inner-node entry size: key (16) + child page (u64).
const INNER_ENTRY_LEN: usize = 24;

/// Cell tags (first byte of every leaf cell).
const CELL_INLINE: u8 = 0;
const CELL_OVERFLOW: u8 = 1;

/// Encoded size of an overflow cell: tag + total_len (u32) + first_page (u64).
const OVERFLOW_CELL_LEN: usize = 13;

/// Bytes available for content on a page (after the common header and the
/// checksum trailer).
fn usable(page_size: u32) -> usize {
    page_size as usize - PAGE_HEADER_LEN - PAGE_TRAILER_LEN
}

/// Maximum footprint (slot + cell) of one leaf entry: a quarter of the
/// usable space. Keeps the split argument airtight: a page holds at most
/// `usable + usable/4` bytes of entries after an upsert, so cutting at the
/// byte midpoint always yields two halves that each fit (FORMAT.md §5.1).
fn max_entry_footprint(page_size: u32) -> usize {
    usable(page_size) / 4
}

/// Largest value stored inline in a leaf cell; anything bigger overflows.
fn max_inline_value(page_size: u32) -> usize {
    max_entry_footprint(page_size) - SLOT_LEN - 1
}

/// Overflow chain payload capacity per page.
fn overflow_capacity(page_size: u32) -> usize {
    usable(page_size)
}

// ---------------------------------------------------------------------------
// Node (de)serialization
// ---------------------------------------------------------------------------

/// A leaf cell: the value inline, or a pointer to its overflow chain.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cell {
    Inline(Vec<u8>),
    Overflow { total_len: u32, first_page: u64 },
}

impl Cell {
    fn encoded_len(&self) -> usize {
        match self {
            Cell::Inline(v) => 1 + v.len(),
            Cell::Overflow { .. } => OVERFLOW_CELL_LEN,
        }
    }
}

#[derive(Debug, Clone)]
struct LeafEntry {
    key: Key,
    cell: Cell,
}

impl LeafEntry {
    fn footprint(&self) -> usize {
        SLOT_LEN + self.cell.encoded_len()
    }
}

#[derive(Debug)]
struct InnerNode {
    /// `(separator, child)`: `child` covers keys `<= separator`. Sorted.
    entries: Vec<(Key, u64)>,
    /// Child for keys greater than every separator.
    rightmost: u64,
}

#[derive(Debug)]
enum Node {
    Leaf(Vec<LeafEntry>),
    Inner(InnerNode),
}

fn malformed(page_no: u64, what: &'static str) -> Error {
    Error::MalformedPage { page_no, what }
}

/// Decodes a B-tree page (leaf or inner). Every offset/length/order check
/// happens here; `page` has already passed its checksum.
fn decode_node(page: &[u8], page_no: u64) -> Result<Node> {
    let header = PageHeader::decode(page).ok_or_else(|| malformed(page_no, "page header"))?;
    let content_end = page.len() - PAGE_TRAILER_LEN; // page len >= header len > trailer
    let n = header.entry_count as usize;
    match header.page_type {
        PageType::BtreeLeaf => {
            let slots_end = PAGE_HEADER_LEN
                .checked_add(
                    n.checked_mul(SLOT_LEN)
                        .ok_or_else(|| malformed(page_no, "slot count"))?,
                )
                .filter(|&e| e <= content_end)
                .ok_or_else(|| malformed(page_no, "slot directory"))?;
            let mut entries = Vec::with_capacity(n);
            let mut prev: Option<Key> = None;
            for i in 0..n {
                let slot = PAGE_HEADER_LEN + i * SLOT_LEN;
                let key: Key = page
                    .get(slot..slot + 16)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| malformed(page_no, "slot key"))?;
                if prev.is_some_and(|p| p >= key) {
                    return Err(malformed(page_no, "unsorted leaf keys"));
                }
                prev = Some(key);
                let off = read_u16(page, slot + 16, page_no)? as usize;
                let len = read_u16(page, slot + 18, page_no)? as usize;
                if off < slots_end || len == 0 || off + len > content_end {
                    return Err(malformed(page_no, "cell bounds"));
                }
                let cell_bytes = page
                    .get(off..off + len)
                    .ok_or_else(|| malformed(page_no, "cell bounds"))?;
                let cell = decode_cell(cell_bytes, page_no)?;
                entries.push(LeafEntry { key, cell });
            }
            Ok(Node::Leaf(entries))
        }
        PageType::BtreeInner => {
            if n == 0 {
                return Err(malformed(page_no, "empty inner node"));
            }
            (PAGE_HEADER_LEN + 8)
                .checked_add(
                    n.checked_mul(INNER_ENTRY_LEN)
                        .ok_or_else(|| malformed(page_no, "entry count"))?,
                )
                .filter(|&e| e <= content_end)
                .ok_or_else(|| malformed(page_no, "inner entries"))?;
            let rightmost = read_u64(page, PAGE_HEADER_LEN, page_no)?;
            if rightmost == 0 {
                return Err(malformed(page_no, "null rightmost child"));
            }
            let mut entries = Vec::with_capacity(n);
            let mut prev: Option<Key> = None;
            for i in 0..n {
                let base = PAGE_HEADER_LEN + 8 + i * INNER_ENTRY_LEN;
                let key: Key = page
                    .get(base..base + 16)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| malformed(page_no, "inner key"))?;
                if prev.is_some_and(|p| p >= key) {
                    return Err(malformed(page_no, "unsorted inner keys"));
                }
                prev = Some(key);
                let child = read_u64(page, base + 16, page_no)?;
                if child == 0 {
                    return Err(malformed(page_no, "null child"));
                }
                entries.push((key, child));
            }
            Ok(Node::Inner(InnerNode { entries, rightmost }))
        }
        _ => Err(malformed(page_no, "unexpected page type in b-tree")),
    }
}

fn decode_cell(bytes: &[u8], page_no: u64) -> Result<Cell> {
    match bytes.first() {
        Some(&CELL_INLINE) => Ok(Cell::Inline(bytes[1..].to_vec())),
        Some(&CELL_OVERFLOW) if bytes.len() == OVERFLOW_CELL_LEN => {
            let total_len = read_u32(bytes, 1, page_no)?;
            let first_page = read_u64(bytes, 5, page_no)?;
            if first_page == 0 || total_len as usize > MAX_RECORD_LEN {
                return Err(malformed(page_no, "overflow cell"));
            }
            Ok(Cell::Overflow {
                total_len,
                first_page,
            })
        }
        _ => Err(malformed(page_no, "cell tag")),
    }
}

/// Encodes a leaf. `None` = does not fit at this page size (caller splits).
/// Entries must be sorted; cells are packed from the tail.
fn encode_leaf(entries: &[LeafEntry], page_size: u32) -> Option<Vec<u8>> {
    let slots_end = PAGE_HEADER_LEN + entries.len() * SLOT_LEN;
    let cells_len: usize = entries.iter().map(|e| e.cell.encoded_len()).sum();
    let content_end = page_size as usize - PAGE_TRAILER_LEN;
    if slots_end + cells_len > content_end {
        return None;
    }
    let mut page = vec![0u8; page_size as usize];
    PageHeader {
        page_type: PageType::BtreeLeaf,
        entry_count: entries.len() as u32,
        next_page: 0,
    }
    .encode_into(&mut page);
    let mut cursor = content_end;
    for (i, entry) in entries.iter().enumerate() {
        let cell_len = entry.cell.encoded_len();
        cursor -= cell_len;
        match &entry.cell {
            Cell::Inline(v) => {
                page[cursor] = CELL_INLINE;
                page[cursor + 1..cursor + cell_len].copy_from_slice(v);
            }
            Cell::Overflow {
                total_len,
                first_page,
            } => {
                page[cursor] = CELL_OVERFLOW;
                page[cursor + 1..cursor + 5].copy_from_slice(&total_len.to_le_bytes());
                page[cursor + 5..cursor + 13].copy_from_slice(&first_page.to_le_bytes());
            }
        }
        let slot = PAGE_HEADER_LEN + i * SLOT_LEN;
        page[slot..slot + 16].copy_from_slice(&entry.key);
        page[slot + 16..slot + 18].copy_from_slice(&(cursor as u16).to_le_bytes());
        page[slot + 18..slot + 20].copy_from_slice(&(cell_len as u16).to_le_bytes());
    }
    Some(page)
}

/// Encodes an inner node. `None` = too many entries for this page size.
fn encode_inner(node: &InnerNode, page_size: u32) -> Option<Vec<u8>> {
    let end = PAGE_HEADER_LEN + 8 + node.entries.len() * INNER_ENTRY_LEN;
    if node.entries.is_empty() || end > page_size as usize - PAGE_TRAILER_LEN {
        return None;
    }
    let mut page = vec![0u8; page_size as usize];
    PageHeader {
        page_type: PageType::BtreeInner,
        entry_count: node.entries.len() as u32,
        next_page: 0,
    }
    .encode_into(&mut page);
    page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + 8].copy_from_slice(&node.rightmost.to_le_bytes());
    for (i, (key, child)) in node.entries.iter().enumerate() {
        let base = PAGE_HEADER_LEN + 8 + i * INNER_ENTRY_LEN;
        page[base..base + 16].copy_from_slice(key);
        page[base + 16..base + 24].copy_from_slice(&child.to_le_bytes());
    }
    Some(page)
}

// ---------------------------------------------------------------------------
// Overflow chains
// ---------------------------------------------------------------------------

/// Writes `value` into an overflow chain and returns the first page. Pages
/// in `reuse` (the chain a replacement is displacing, in order) are rewritten
/// in place before any fresh page is allocated, so rewriting a value costs
/// only its growth; leftover `reuse` pages (a shrinking rewrite) are orphaned
/// until `vacuum`.
fn write_overflow(txn: &mut Txn<'_>, value: &[u8], reuse: &[u64]) -> Result<u64> {
    let cap = overflow_capacity(txn.page_size());
    let chunks: Vec<&[u8]> = value.chunks(cap).collect();
    let mut pages = Vec::with_capacity(chunks.len());
    for i in 0..chunks.len() {
        match reuse.get(i) {
            Some(&page_no) => pages.push(page_no),
            None => pages.push(txn.allocate_page()?),
        }
    }
    let page_size = txn.page_size() as usize;
    for (i, chunk) in chunks.iter().enumerate() {
        let mut page = vec![0u8; page_size];
        PageHeader {
            page_type: PageType::Overflow,
            entry_count: chunk.len() as u32,
            next_page: pages.get(i + 1).copied().unwrap_or(0),
        }
        .encode_into(&mut page);
        page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        txn.write_page(pages[i], &page)?;
    }
    pages
        .first()
        .copied()
        .ok_or(Error::Internal("empty overflow chain"))
}

/// Page numbers of an existing overflow chain, in order — the pages a
/// replacement value may rewrite in place. Bounded by the page count
/// `total_len` implies, so a corrupt `next_page` cycle cannot loop.
fn chain_pages(txn: &Txn<'_>, first_page: u64, total_len: u32) -> Result<Vec<u64>> {
    let cap = overflow_capacity(txn.page_size());
    let expected = (total_len as usize).div_ceil(cap).max(1);
    let mut pages = Vec::with_capacity(expected);
    let mut page_no = first_page;
    for _ in 0..expected {
        let page = txn.read_page(page_no)?;
        let header =
            PageHeader::decode(&page).ok_or_else(|| malformed(page_no, "overflow header"))?;
        if header.page_type != PageType::Overflow {
            return Err(malformed(page_no, "not an overflow page"));
        }
        pages.push(page_no);
        if header.next_page == 0 {
            break;
        }
        page_no = header.next_page;
    }
    Ok(pages)
}

/// Reads an overflow chain back. Bounded: every hop consumes at least one
/// payload byte of `total_len`, so a cycle cannot loop.
fn read_overflow(src: &dyn PageSource, first_page: u64, total_len: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut remaining = total_len as usize;
    let mut page_no = first_page;
    while remaining > 0 {
        let page = src.page(page_no)?;
        let header =
            PageHeader::decode(&page).ok_or_else(|| malformed(page_no, "overflow header"))?;
        if header.page_type != PageType::Overflow {
            return Err(malformed(page_no, "not an overflow page"));
        }
        let used = header.entry_count as usize;
        if used == 0 || used > remaining || used > overflow_capacity(src.page_size()) {
            return Err(malformed(page_no, "overflow payload length"));
        }
        let payload = page
            .get(PAGE_HEADER_LEN..PAGE_HEADER_LEN + used)
            .ok_or_else(|| malformed(page_no, "overflow payload length"))?;
        out.extend_from_slice(payload);
        remaining -= used;
        if remaining > 0 {
            if header.next_page == 0 {
                return Err(malformed(page_no, "broken overflow chain"));
            }
            page_no = header.next_page;
        }
    }
    Ok(out)
}

fn resolve_cell(src: &dyn PageSource, cell: &Cell) -> Result<Vec<u8>> {
    match cell {
        Cell::Inline(v) => Ok(v.clone()),
        Cell::Overflow {
            total_len,
            first_page,
        } => read_overflow(src, *first_page, *total_len),
    }
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

/// Child index for `key` in an inner node: first entry whose separator is
/// `>= key` (child covers keys `<= separator`), else the rightmost child.
fn child_for(node: &InnerNode, key: &Key) -> u64 {
    match node.entries.iter().find(|(sep, _)| key <= sep) {
        Some((_, child)) => *child,
        None => node.rightmost,
    }
}

/// Point lookup. `root == 0` = empty tree.
pub fn get(src: &dyn PageSource, root: u64, key: &Key) -> Result<Option<Vec<u8>>> {
    if root == 0 {
        return Ok(None);
    }
    let mut page_no = root;
    for _ in 0..MAX_DEPTH {
        let page = src.page(page_no)?;
        match decode_node(&page, page_no)? {
            Node::Inner(node) => page_no = child_for(&node, key),
            Node::Leaf(entries) => {
                return match entries.binary_search_by(|e| e.key.cmp(key)) {
                    Ok(i) => resolve_cell(src, &entries[i].cell).map(Some),
                    Err(_) => Ok(None),
                };
            }
        }
    }
    Err(malformed(page_no, "b-tree deeper than MAX_DEPTH"))
}

// ---------------------------------------------------------------------------
// Insert / upsert
// ---------------------------------------------------------------------------

enum Ins {
    Fit,
    /// The page split: it kept keys `<= sep`; `right` holds the rest.
    Split {
        sep: Key,
        right: u64,
    },
}

/// Builds the leaf [`Cell`] for a value, spilling to an overflow chain when
/// it is too large to inline. `reuse` carries the replaced cell's old chain
/// pages (empty on a fresh insert) for in-place rewriting.
fn make_cell(txn: &mut Txn<'_>, value: &[u8], reuse: &[u64]) -> Result<Cell> {
    if value.len() <= max_inline_value(txn.page_size()) {
        Ok(Cell::Inline(value.to_vec()))
    } else {
        Ok(Cell::Overflow {
            total_len: value.len() as u32,
            first_page: write_overflow(txn, value, reuse)?,
        })
    }
}

/// Inserts or replaces `key → value` and updates the transaction's root
/// pointer as needed. Replacing a value that had an overflow chain rewrites
/// the old chain's pages in place (allocating only for growth); a shrinking
/// rewrite orphans the tail until `vacuum` (`docs/FORMAT.md` §5.1).
pub fn insert(txn: &mut Txn<'_>, key: Key, value: &[u8]) -> Result<()> {
    if value.len() > MAX_RECORD_LEN {
        return Err(Error::InvalidArgument("value exceeds MAX_RECORD_LEN"));
    }
    let page_size = txn.page_size();
    let root = txn.root_btree_page();
    if root == 0 {
        let cell = make_cell(txn, value, &[])?;
        let page_no = txn.allocate_page()?;
        let page = encode_leaf(&[LeafEntry { key, cell }], page_size)
            .ok_or(Error::Internal("fresh leaf does not fit"))?;
        txn.write_page(page_no, &page)?;
        txn.set_root_btree_page(page_no);
        return Ok(());
    }

    if let Ins::Split { sep, right } = insert_rec(txn, root, key, value, 0)? {
        let new_root = txn.allocate_page()?;
        let node = InnerNode {
            entries: vec![(sep, root)],
            rightmost: right,
        };
        let page =
            encode_inner(&node, page_size).ok_or(Error::Internal("fresh root does not fit"))?;
        txn.write_page(new_root, &page)?;
        txn.set_root_btree_page(new_root);
    }
    Ok(())
}

fn insert_rec(
    txn: &mut Txn<'_>,
    page_no: u64,
    key: Key,
    value: &[u8],
    depth: usize,
) -> Result<Ins> {
    if depth >= MAX_DEPTH {
        return Err(malformed(page_no, "b-tree deeper than MAX_DEPTH"));
    }
    let page_size = txn.page_size();
    let page = txn.read_page(page_no)?;
    match decode_node(&page, page_no)? {
        Node::Leaf(mut entries) => {
            // The cell is built here at the leaf, not up in `insert`, so a
            // replacement can hand the old cell's overflow chain to
            // `make_cell` for in-place reuse.
            match entries.binary_search_by(|e| e.key.cmp(&key)) {
                Ok(i) => {
                    let reuse = match &entries[i].cell {
                        Cell::Overflow {
                            total_len,
                            first_page,
                        } => chain_pages(txn, *first_page, *total_len)?,
                        Cell::Inline(_) => Vec::new(),
                    };
                    entries[i].cell = make_cell(txn, value, &reuse)?;
                }
                Err(i) => {
                    let cell = make_cell(txn, value, &[])?;
                    entries.insert(i, LeafEntry { key, cell });
                }
            }
            if let Some(encoded) = encode_leaf(&entries, page_size) {
                txn.write_page(page_no, &encoded)?;
                return Ok(Ins::Fit);
            }
            // Split at the byte midpoint. Every entry footprint is capped at
            // usable/4 (values above max_inline_value overflow), so both
            // halves provably fit — see FORMAT.md §5.1.
            let total: usize = entries.iter().map(LeafEntry::footprint).sum();
            let mut acc = 0;
            let mut cut = entries.len(); // first index of the right half
            for (i, e) in entries.iter().enumerate() {
                acc += e.footprint();
                if acc >= total / 2 {
                    cut = i + 1;
                    break;
                }
            }
            let right_entries = entries.split_off(cut);
            if right_entries.is_empty() {
                // Impossible while max_entry_footprint holds (see FORMAT.md
                // §5.1) — surfaced as a typed error, never silent.
                return Err(Error::Internal("leaf split produced an empty half"));
            }
            let (Some(left_page), Some(right_page), Some(last)) = (
                encode_leaf(&entries, page_size),
                encode_leaf(&right_entries, page_size),
                entries.last(),
            ) else {
                return Err(Error::Internal("leaf split does not fit"));
            };
            let sep = last.key;
            let right = txn.allocate_page()?;
            txn.write_page(page_no, &left_page)?;
            txn.write_page(right, &right_page)?;
            Ok(Ins::Split { sep, right })
        }
        Node::Inner(mut node) => {
            let idx = node
                .entries
                .iter()
                .position(|(sep, _)| key <= *sep)
                .unwrap_or(node.entries.len());
            let child = match node.entries.get(idx) {
                Some((_, c)) => *c,
                None => node.rightmost,
            };
            match insert_rec(txn, child, key, value, depth + 1)? {
                Ins::Fit => Ok(Ins::Fit),
                Ins::Split { sep, right } => {
                    // `child` kept keys <= sep; `right` now covers the upper
                    // part of child's old range.
                    match node.entries.get_mut(idx) {
                        Some(entry) => entry.1 = right,
                        None => node.rightmost = right,
                    }
                    node.entries.insert(idx, (sep, child));
                    if let Some(encoded) = encode_inner(&node, page_size) {
                        txn.write_page(page_no, &encoded)?;
                        return Ok(Ins::Fit);
                    }
                    // Inner split: promote the middle separator.
                    let m = node.entries.len() / 2;
                    let right_entries = node.entries.split_off(m + 1);
                    let (promoted_key, promoted_child) = node
                        .entries
                        .pop()
                        .ok_or(Error::Internal("inner split underflow"))?;
                    let right_node = InnerNode {
                        entries: right_entries,
                        rightmost: node.rightmost,
                    };
                    node.rightmost = promoted_child;
                    let (Some(left_page), Some(right_page)) = (
                        encode_inner(&node, page_size),
                        encode_inner(&right_node, page_size),
                    ) else {
                        return Err(Error::Internal("inner split does not fit"));
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
// Scan (in key order == timeline order)
// ---------------------------------------------------------------------------

/// Iterates every `(key, value)` in ascending key order.
pub fn scan(src: &dyn PageSource, root: u64) -> Scan<'_> {
    Scan {
        src,
        start: (root != 0).then_some(root),
        stack: Vec::new(),
        fused: false,
    }
}

enum Frame {
    Inner {
        children: Vec<u64>,
        next: usize,
    },
    Leaf {
        entries: Vec<LeafEntry>,
        next: usize,
    },
}

/// In-order B-tree iterator. Yields a typed error and fuses on the first
/// malformed page (never panics, never loops on corrupt files).
pub struct Scan<'a> {
    src: &'a dyn PageSource,
    start: Option<u64>,
    stack: Vec<Frame>,
    fused: bool,
}

impl Scan<'_> {
    fn push_page(&mut self, page_no: u64) -> Result<()> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(malformed(page_no, "b-tree deeper than MAX_DEPTH"));
        }
        let page = self.src.page(page_no)?;
        let frame = match decode_node(&page, page_no)? {
            Node::Inner(node) => {
                let mut children: Vec<u64> = node.entries.iter().map(|(_, c)| *c).collect();
                children.push(node.rightmost);
                Frame::Inner { children, next: 0 }
            }
            Node::Leaf(entries) => Frame::Leaf { entries, next: 0 },
        };
        self.stack.push(frame);
        Ok(())
    }

    fn advance(&mut self) -> Result<Option<(Key, Vec<u8>)>> {
        if let Some(root) = self.start.take() {
            self.push_page(root)?;
        }
        loop {
            let Some(top) = self.stack.last_mut() else {
                return Ok(None);
            };
            match top {
                Frame::Leaf { entries, next } => {
                    if let Some(entry) = entries.get(*next) {
                        let key = entry.key;
                        let cell = entry.cell.clone();
                        *next += 1;
                        return Ok(Some((key, resolve_cell(self.src, &cell)?)));
                    }
                    self.stack.pop();
                }
                Frame::Inner { children, next } => {
                    if let Some(&child) = children.get(*next) {
                        *next += 1;
                        self.push_page(child)?;
                    } else {
                        self.stack.pop();
                    }
                }
            }
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(Key, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.fused {
            return None;
        }
        match self.advance() {
            Ok(Some(item)) => Some(Ok(item)),
            Ok(None) => {
                self.fused = true;
                None
            }
            Err(e) => {
                self.fused = true;
                Some(Err(e))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Little-endian helpers (bounds-checked)
// ---------------------------------------------------------------------------

fn read_u16(buf: &[u8], off: usize, page_no: u64) -> Result<u16> {
    buf.get(off..off + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| malformed(page_no, "short read"))
}

fn read_u32(buf: &[u8], off: usize, page_no: u64) -> Result<u32> {
    buf.get(off..off + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| malformed(page_no, "short read"))
}

fn read_u64(buf: &[u8], off: usize, page_no: u64) -> Result<u64> {
    buf.get(off..off + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| malformed(page_no, "short read"))
}

/// Fuzz-only surface: decodes one page as each B-tree node kind, exercising
/// every parser branch. Must return, never panic (`fuzz_page` target).
#[doc(hidden)]
pub fn fuzz_decode_page(page: &[u8]) {
    let _ = decode_node(page, 1);
    let _ = PageHeader::decode(page);
    if page.len() > OVERFLOW_CELL_LEN {
        let _ = decode_cell(page, 1);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::storage::pager::{Pager, PagerOptions};
    use crate::storage::sim::{SimVfs, SplitMix64};
    use crate::storage::vfs::Vfs;

    const SMALL: u32 = 512; // forces deep trees and early splits

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

    fn key(n: u64) -> Key {
        let mut k = [0u8; 16];
        k[8..].copy_from_slice(&n.to_be_bytes());
        k
    }

    #[test]
    fn empty_tree_get_and_scan() {
        let pager = pager(SMALL);
        assert_eq!(get(&pager, 0, &key(1)).unwrap(), None);
        assert_eq!(scan(&pager, 0).count(), 0);
    }

    #[test]
    fn insert_get_within_one_leaf() {
        let mut pager = pager(SMALL);
        let mut txn = pager.begin().unwrap();
        insert(&mut txn, key(2), b"two").unwrap();
        insert(&mut txn, key(1), b"one").unwrap();
        let root = txn.root_btree_page();
        assert_eq!(get(&txn, root, &key(1)).unwrap().unwrap(), b"one");
        assert_eq!(get(&txn, root, &key(2)).unwrap().unwrap(), b"two");
        assert_eq!(get(&txn, root, &key(3)).unwrap(), None);
        txn.commit().unwrap();
        // Committed state visible through the pager.
        let root = pager.header().root_btree_page;
        assert_eq!(get(&pager, root, &key(2)).unwrap().unwrap(), b"two");
    }

    #[test]
    fn growing_rewrites_reuse_the_overflow_chain() {
        // Rewriting one key with ever-larger overflowing values must grow the
        // file linearly with the final value, not once per rewrite (the
        // quadratic pattern fixed alongside the dict — see index::dict tests).
        let mut pager = pager(SMALL);
        let mut txn = pager.begin().unwrap();
        let rounds = 100usize;
        let step = 40usize;
        for i in 1..=rounds {
            insert(&mut txn, key(7), &vec![0xCD; i * step]).unwrap();
        }
        let final_value = vec![0xCD; rounds * step];
        let root = txn.root_btree_page();
        assert_eq!(get(&txn, root, &key(7)).unwrap().unwrap(), final_value);
        let chain_len = final_value.len().div_ceil(overflow_capacity(SMALL)) as u64;
        assert!(
            txn.page_count() <= chain_len + 8,
            "chain reuse regressed: {} pages allocated for a {}-page chain",
            txn.page_count(),
            chain_len
        );
        txn.commit().unwrap();
    }

    #[test]
    fn shrinking_rewrite_truncates_the_chain_and_reads_back() {
        let mut pager = pager(SMALL);
        let mut txn = pager.begin().unwrap();
        insert(&mut txn, key(1), &vec![0xAB; 5000]).unwrap();
        // Shorter but still overflowing: head of the chain reused in place.
        let small = vec![0xEF; 1200];
        insert(&mut txn, key(1), &small).unwrap();
        let root = txn.root_btree_page();
        assert_eq!(get(&txn, root, &key(1)).unwrap().unwrap(), small);
        // Shrink to inline: whole chain orphaned, value still correct.
        insert(&mut txn, key(1), b"tiny").unwrap();
        let root = txn.root_btree_page();
        assert_eq!(get(&txn, root, &key(1)).unwrap().unwrap(), b"tiny");
        txn.commit().unwrap();
    }

    /// Random inserts + updates vs. a `BTreeMap` model at 512-byte pages:
    /// forces multi-level splits; verifies point reads and full-scan order.
    #[test]
    fn model_equivalence_with_splits_and_updates() {
        let mut pager = pager(SMALL);
        let mut model: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        let mut rng = SplitMix64(0xB7E3);

        for round in 0..6 {
            let mut txn = pager.begin().unwrap();
            for _ in 0..80 {
                let k = key(rng.next_u64() % 200); // collisions → updates
                let len = (rng.next_u64() % 90) as usize;
                let value: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
                insert(&mut txn, k, &value).unwrap();
                model.insert(k, value);
            }
            txn.commit().unwrap();

            let root = pager.header().root_btree_page;
            for (k, v) in &model {
                assert_eq!(
                    get(&pager, root, k).unwrap().as_ref(),
                    Some(v),
                    "round {round}"
                );
            }
            assert_eq!(get(&pager, root, &key(10_000)).unwrap(), None);
            let scanned: Vec<(Key, Vec<u8>)> = scan(&pager, root).collect::<Result<_>>().unwrap();
            let expected: Vec<(Key, Vec<u8>)> =
                model.iter().map(|(k, v)| (*k, v.clone())).collect();
            assert_eq!(scanned, expected, "scan order/content, round {round}");
        }
    }

    #[test]
    fn overflow_values_roundtrip_and_update() {
        let mut pager = pager(SMALL);
        let big: Vec<u8> = (0..5000u32).map(|i| i as u8).collect(); // ~10 chain pages
        let bigger: Vec<u8> = (0..12_000u32).map(|i| (i * 7) as u8).collect();

        let mut txn = pager.begin().unwrap();
        insert(&mut txn, key(1), &big).unwrap();
        insert(&mut txn, key(2), b"small").unwrap();
        txn.commit().unwrap();
        let root = pager.header().root_btree_page;
        assert_eq!(get(&pager, root, &key(1)).unwrap().unwrap(), big);

        // Update overflow → overflow (old chain is orphaned until vacuum).
        let mut txn = pager.begin().unwrap();
        insert(&mut txn, key(1), &bigger).unwrap();
        txn.commit().unwrap();
        let root = pager.header().root_btree_page;
        assert_eq!(get(&pager, root, &key(1)).unwrap().unwrap(), bigger);
        // Update overflow → inline.
        let mut txn = pager.begin().unwrap();
        insert(&mut txn, key(1), b"tiny now").unwrap();
        txn.commit().unwrap();
        let root = pager.header().root_btree_page;
        assert_eq!(get(&pager, root, &key(1)).unwrap().unwrap(), b"tiny now");
        assert_eq!(get(&pager, root, &key(2)).unwrap().unwrap(), b"small");
    }

    #[test]
    fn rollback_discards_tree_changes() {
        let mut pager = pager(SMALL);
        let mut txn = pager.begin().unwrap();
        insert(&mut txn, key(1), b"committed").unwrap();
        txn.commit().unwrap();
        let root_before = pager.header().root_btree_page;

        let mut txn = pager.begin().unwrap();
        for n in 2..100 {
            insert(&mut txn, key(n), b"rolled back").unwrap();
        }
        drop(txn);
        assert_eq!(pager.header().root_btree_page, root_before);
        assert_eq!(
            get(&pager, root_before, &key(1)).unwrap().unwrap(),
            b"committed"
        );
        assert_eq!(get(&pager, root_before, &key(50)).unwrap(), None);
    }

    #[test]
    fn survives_reopen() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let opts = PagerOptions {
            page_size: SMALL,
            ..Default::default()
        };
        let mut pager = Pager::create(Arc::clone(&vfs), Path::new("memory.mind"), opts).unwrap();
        let mut txn = pager.begin().unwrap();
        for n in 0..150 {
            insert(&mut txn, key(n), format!("value-{n}").as_bytes()).unwrap();
        }
        txn.commit().unwrap();
        pager.close().unwrap();

        let pager = Pager::open(vfs, Path::new("memory.mind"), opts).unwrap();
        let root = pager.header().root_btree_page;
        for n in 0..150 {
            assert_eq!(
                get(&pager, root, &key(n)).unwrap().unwrap(),
                format!("value-{n}").as_bytes()
            );
        }
        assert_eq!(scan(&pager, root).count(), 150);
    }

    #[test]
    fn decode_never_panics_on_arbitrary_pages() {
        let mut rng = SplitMix64(0xF00D);
        for _ in 0..2000 {
            let len = [64usize, 512, 517, 4096][(rng.next_u64() % 4) as usize];
            let mut page = vec![0u8; len];
            for b in &mut page {
                *b = rng.next_u64() as u8;
            }
            fuzz_decode_page(&page); // must return, never panic
        }
        // Mutated valid leaf pages exercise deeper branches.
        let entries = vec![
            LeafEntry {
                key: key(1),
                cell: Cell::Inline(b"abc".to_vec()),
            },
            LeafEntry {
                key: key(2),
                cell: Cell::Overflow {
                    total_len: 100,
                    first_page: 3,
                },
            },
        ];
        let valid = encode_leaf(&entries, SMALL).unwrap();
        for _ in 0..2000 {
            let mut page = valid.clone();
            let i = (rng.next_u64() as usize) % page.len();
            page[i] ^= (rng.next_u64() as u8) | 1;
            fuzz_decode_page(&page);
        }
    }

    #[test]
    fn corrupt_chain_is_a_typed_error_not_a_hang() {
        let mut pager = pager(SMALL);
        let big = vec![7u8; 3000];
        let mut txn = pager.begin().unwrap();
        insert(&mut txn, key(1), &big).unwrap();
        txn.commit().unwrap();

        // Lie about the total length so the chain "runs out": typed error.
        let err = read_overflow(&pager, 2, 1_000_000).unwrap_err();
        assert!(matches!(
            err,
            Error::MalformedPage { .. } | Error::PageOutOfBounds { .. }
        ));
    }
}
