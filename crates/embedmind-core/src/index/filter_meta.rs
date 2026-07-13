//! Filter-meta sidecar (FTOPT-1, `docs/adr/0027`, `docs/FORMAT.md` §13).
//!
//! A light columnar map `record_id → (tombstone/superseded flags, project,
//! agent, doc_len)` kept **outside** the full record body, so the `keep`
//! predicate of every search and the BM25 `doc_len` callback answer from an
//! in-memory table instead of one full B-tree record load per candidate —
//! the 88.8%-of-query-time hot spot measured in FT1/FTOPT-0 (ADR 0017).
//! The full record is only loaded for the final top-k hits and for custom
//! metadata filters the sidecar cannot answer.
//!
//! Two page chains, both newest-page-first (`next_page` points at the older
//! page, so appends never rewrite more than the head page):
//!
//! - [`PageType::FilterMeta`]: fixed-size entries (`ENTRY_LEN` bytes each).
//!   Updates (forget, supersede) **append** a fresh entry for the same id;
//!   the newest occurrence wins at load time. `vacuum` rewrites the chain
//!   dense.
//! - [`PageType::FilterSymbols`]: the interned project/agent strings entries
//!   reference by `u32` id. Symbol 0 is reserved for "no string" (global
//!   project / empty agent) and never stored.
//!
//! Writes happen inside the **same transaction** as the record they mirror
//! ([`record_updates`]), so the sidecar can never diverge from the records
//! under the WAL's crash guarantees. Files older than format_version 7 have
//! no sidecar (both header roots 0) and every write here is a no-op; reads
//! then fall back to the full record load — degradation, never an error.
//!
//! Every decoder validates lengths/invariants against the raw bytes before
//! allocating and never panics (fuzz target `fuzz_filter_meta_page`,
//! `docs/TESTING.md` §3).

use std::collections::{HashMap, HashSet};

use ulid::Ulid;

use crate::error::{Error, Result};
use crate::format::{PAGE_HEADER_LEN, PAGE_TRAILER_LEN, PageHeader, PageType};
use crate::storage::Txn;
use crate::storage::btree::PageSource;

/// First format_version whose files carry (and write) the sidecar.
pub const MIN_FORMAT_VERSION: u32 = 7;

/// Entry flag bits (reserved bits are written zero, ignored on read).
const FLAG_TOMBSTONE: u8 = 1;
const FLAG_SUPERSEDED: u8 = 1 << 1;
/// The mirrored record has at least one metadata entry — when clear, a query
/// carrying metadata filters can reject without loading the record (a filter
/// over an absent key is a plain non-match, `record.rs`).
const FLAG_HAS_METADATA: u8 = 1 << 2;
/// The project/agent strings could not be interned (longer than a page or the
/// symbol space overflowed): `project_sym`/`agent_sym` are meaningless and a
/// scoped/agent-filtered query must load the record for this id.
const FLAG_SCOPE_OVERFLOW: u8 = 1 << 3;

/// The symbol id meaning "no string" (global project / empty agent).
const SYM_NONE: u32 = 0;

/// On-disk size of one FILTER_META entry:
/// `record_id` (16, big-endian per the ULID spec, like every other key) +
/// `flags` (1) + `project_sym` (u32) + `agent_sym` (u32) + `doc_len` (u32).
const ENTRY_LEN: usize = 16 + 1 + 4 + 4 + 4;

/// One decoded sidecar entry — everything `keep`/`doc_len` need that is not
/// a custom metadata filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    flags: u8,
    project_sym: u32,
    agent_sym: u32,
    /// BM25 token count of the record's content (`fts::doc_len`), captured at
    /// write time — content is immutable after `remember`, so this can never
    /// go stale.
    pub doc_len: u32,
}

/// What a query needs from the sidecar to decide one candidate, resolved
/// **once per query** (strings → symbol ids) by the caller.
#[derive(Debug, Clone, Copy)]
pub struct QueryNeeds {
    /// Project the query is scoped to.
    pub project: Want,
    /// Agent filter (S14).
    pub agent: Want,
    /// The query carries custom metadata filters, which only the full record
    /// can answer (unless the entry says the record has no metadata at all).
    pub has_metadata_filters: bool,
}

/// A resolved string constraint: anything, one specific symbol, or a string
/// that was never interned — which no record can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// No constraint on this field.
    Any,
    /// Must equal this interned symbol.
    Sym(u32),
    /// The wanted string is not in the symbol table, so no record whose entry
    /// carries valid symbols can match it.
    Absent,
}

/// Outcome of a sidecar-only `keep` decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The candidate passes every check the query needs — no record load.
    Accept,
    /// The candidate definitely fails; the reason feeds the profiling
    /// breakdown (`fts::KeepOutcome`).
    Reject(RejectReason),
    /// The sidecar cannot decide (entry missing, scope overflow, or custom
    /// metadata filters over a record that has metadata): load the record
    /// and run the full predicate, exactly as before the sidecar existed.
    NeedRecord,
}

/// Why a candidate was rejected without loading its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Tombstoned or superseded.
    Dead,
    /// Wrong project or wrong agent.
    OutOfScope,
    /// The query has metadata filters and the record has no metadata (every
    /// filter over an absent key is a non-match).
    FilteredOut,
}

/// The whole sidecar materialized in memory: ~[`ENTRY_LEN`] bytes per live
/// id (≈ 2.9 MiB at 100k memories), rebuilt per committed state and cached
/// by the caller (`Store`) keyed on `txn_counter`.
#[derive(Debug, Default)]
pub struct Table {
    entries: HashMap<Ulid, Entry>,
    symbols: HashMap<String, u32>,
}

impl Table {
    /// The entry for `id`, if the sidecar has one.
    pub fn get(&self, id: Ulid) -> Option<&Entry> {
        self.entries.get(&id)
    }

    /// Resolves a query string to its interned symbol. `None` = never
    /// interned, so no record with valid symbols carries it.
    fn resolve(&self, s: &str) -> Option<u32> {
        self.symbols.get(s).copied()
    }

    /// Resolves a `Scope::Project` constraint (`None` = `Scope::All`). An
    /// empty project name can never match: records store `Some("")` as
    /// `None` (global), and `Scope::Project` never matches globals.
    pub fn want_project(&self, scope: Option<&str>) -> Want {
        match scope {
            None => Want::Any,
            Some("") => Want::Absent,
            Some(p) => match self.resolve(p) {
                Some(sym) => Want::Sym(sym),
                None => Want::Absent,
            },
        }
    }

    /// Resolves an agent filter (`None` = no filter). Unlike projects, agent
    /// matching is plain string equality on the record's provenance, so an
    /// empty filter matches exactly the records whose agent is empty —
    /// symbol [`SYM_NONE`].
    pub fn want_agent(&self, agent: Option<&str>) -> Want {
        match agent {
            None => Want::Any,
            Some("") => Want::Sym(SYM_NONE),
            Some(a) => match self.resolve(a) {
                Some(sym) => Want::Sym(sym),
                None => Want::Absent,
            },
        }
    }

    /// Decides one candidate from the sidecar alone. Never wrong, sometimes
    /// undecided: any situation the sidecar cannot prove either way returns
    /// [`Decision::NeedRecord`] and the caller re-runs the full record
    /// predicate — so the result set is byte-identical to the pre-sidecar
    /// path (the FTOPT-1 equivalence property).
    pub fn decide(&self, id: Ulid, needs: &QueryNeeds) -> Decision {
        let Some(e) = self.entries.get(&id) else {
            return Decision::NeedRecord;
        };
        if e.flags & (FLAG_TOMBSTONE | FLAG_SUPERSEDED) != 0 {
            return Decision::Reject(RejectReason::Dead);
        }
        let scope_overflow = e.flags & FLAG_SCOPE_OVERFLOW != 0;
        for (want, sym) in [(needs.project, e.project_sym), (needs.agent, e.agent_sym)] {
            match want {
                Want::Any => {}
                _ if scope_overflow => return Decision::NeedRecord,
                Want::Absent => return Decision::Reject(RejectReason::OutOfScope),
                Want::Sym(s) => {
                    if sym != s {
                        return Decision::Reject(RejectReason::OutOfScope);
                    }
                }
            }
        }
        if needs.has_metadata_filters {
            if e.flags & FLAG_HAS_METADATA == 0 {
                // Every metadata filter over a record without metadata is a
                // plain non-match (`Filter::matches(None)` — record.rs), and
                // no type mismatch can occur against an absent value.
                Decision::Reject(RejectReason::FilteredOut)
            } else {
                Decision::NeedRecord
            }
        } else {
            Decision::Accept
        }
    }
}

/// One sidecar write, mirroring one record as it is stored in this same
/// transaction.
#[derive(Debug)]
pub struct Update<'a> {
    /// The mirrored record's id.
    pub id: Ulid,
    /// `MemoryRecord::tombstone`.
    pub tombstone: bool,
    /// `MemoryRecord::superseded`.
    pub superseded: bool,
    /// Whether the record has any metadata entries.
    pub has_metadata: bool,
    /// `MemoryRecord::project` (`None` = global).
    pub project: Option<&'a str>,
    /// `MemoryRecord::provenance.agent` (may be empty).
    pub agent: &'a str,
    /// `fts::doc_len` of the record's content.
    pub doc_len: u32,
}

// ---------------------------------------------------------------------------
// Load (query side)
// ---------------------------------------------------------------------------

/// Materializes the whole sidecar. `meta_root == 0` yields an empty table
/// (callers treat "no sidecar" via the header roots before getting here).
pub fn load(src: &dyn PageSource, meta_root: u64, symbols_root: u64) -> Result<Table> {
    let mut table = Table {
        entries: HashMap::new(),
        symbols: HashMap::new(),
    };
    let mut ids: HashMap<u32, ()> = HashMap::new();
    for page_no in walk_chain(src, symbols_root, PageType::FilterSymbols)? {
        let page = src.page(page_no)?;
        for (sym, s) in decode_symbols(&page, page_no)? {
            if ids.insert(sym, ()).is_some() || table.symbols.insert(s, sym).is_some() {
                return Err(malformed(page_no, "duplicate filter symbol"));
            }
        }
    }
    // Chain is newest-first and entries within one page are appended oldest-
    // first, so iterating pages head→tail with entries reversed sees each
    // id's newest entry first — first-seen wins.
    for page_no in walk_chain(src, meta_root, PageType::FilterMeta)? {
        let page = src.page(page_no)?;
        for (id, entry) in decode_entries(&page, page_no)?.into_iter().rev() {
            table.entries.entry(id).or_insert(entry);
        }
    }
    Ok(table)
}

/// Collects a chain's page numbers head-first, verifying each page's type and
/// refusing pointer cycles (a corrupt file must yield a typed error, never an
/// infinite loop).
fn walk_chain(src: &dyn PageSource, head: u64, expect: PageType) -> Result<Vec<u64>> {
    let mut pages = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut page_no = head;
    while page_no != 0 {
        if !seen.insert(page_no) {
            return Err(malformed(page_no, "filter-meta chain cycle"));
        }
        let page = src.page(page_no)?;
        let header = PageHeader::decode(&page).ok_or(malformed(page_no, "page header"))?;
        if header.page_type != expect {
            return Err(malformed(page_no, "wrong page type in filter-meta chain"));
        }
        pages.push(page_no);
        page_no = header.next_page;
    }
    Ok(pages)
}

/// Fixed-size entry capacity of one FILTER_META page.
fn entry_capacity(page_size: usize) -> usize {
    page_size.saturating_sub(PAGE_HEADER_LEN + PAGE_TRAILER_LEN) / ENTRY_LEN
}

/// Decodes every entry of a FILTER_META page, oldest first. Every length is
/// validated against the raw bytes before anything is allocated.
fn decode_entries(page: &[u8], page_no: u64) -> Result<Vec<(Ulid, Entry)>> {
    let header = PageHeader::decode(page).ok_or(malformed(page_no, "page header"))?;
    if header.page_type != PageType::FilterMeta {
        return Err(malformed(page_no, "not a FILTER_META page"));
    }
    let count = header.entry_count as usize;
    if count > entry_capacity(page.len()) {
        return Err(malformed(page_no, "filter-meta entry count exceeds page"));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = PAGE_HEADER_LEN + i * ENTRY_LEN;
        let bytes = page
            .get(off..off + ENTRY_LEN)
            .ok_or(malformed(page_no, "filter-meta entry bounds"))?;
        let id_bytes: [u8; 16] = bytes[..16]
            .try_into()
            .map_err(|_| malformed(page_no, "filter-meta id"))?;
        out.push((
            Ulid::from_bytes(id_bytes),
            Entry {
                flags: bytes[16],
                project_sym: read_u32(bytes, 17).ok_or(malformed(page_no, "project_sym"))?,
                agent_sym: read_u32(bytes, 21).ok_or(malformed(page_no, "agent_sym"))?,
                doc_len: read_u32(bytes, 25).ok_or(malformed(page_no, "doc_len"))?,
            },
        ));
    }
    Ok(out)
}

/// Decodes every `(sym_id, string)` of a FILTER_SYMBOLS page, plus nothing
/// else — the caller cross-page-validates uniqueness. Layout per entry:
/// `sym_id` (u32, never [`SYM_NONE`]) + `len` (u16) + UTF-8 bytes.
fn decode_symbols(page: &[u8], page_no: u64) -> Result<Vec<(u32, String)>> {
    let header = PageHeader::decode(page).ok_or(malformed(page_no, "page header"))?;
    if header.page_type != PageType::FilterSymbols {
        return Err(malformed(page_no, "not a FILTER_SYMBOLS page"));
    }
    let count = header.entry_count as usize;
    let body_end = page.len().saturating_sub(PAGE_TRAILER_LEN);
    // Each symbol needs at least its 6-byte prefix: a hostile count is
    // rejected before any allocation (docs/TESTING.md §3).
    if count > body_end.saturating_sub(PAGE_HEADER_LEN) / 6 {
        return Err(malformed(page_no, "filter-symbol count exceeds page"));
    }
    let mut out = Vec::with_capacity(count);
    let mut off = PAGE_HEADER_LEN;
    for _ in 0..count {
        let sym = read_u32(page, off).ok_or(malformed(page_no, "symbol id"))?;
        if sym == SYM_NONE {
            return Err(malformed(page_no, "reserved symbol id 0"));
        }
        let len = read_u16(page, off + 4).ok_or(malformed(page_no, "symbol length"))? as usize;
        off += 6;
        if len > body_end.saturating_sub(off) {
            return Err(malformed(page_no, "symbol length exceeds page"));
        }
        let s = std::str::from_utf8(&page[off..off + len])
            .map_err(|_| malformed(page_no, "symbol utf-8"))?;
        out.push((sym, s.to_owned()));
        off += len;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Write (transaction side)
// ---------------------------------------------------------------------------

/// Appends one sidecar entry per update inside `txn` — the **same**
/// transaction that writes the mirrored records, so both become durable
/// atomically or not at all. A no-op on files older than
/// [`MIN_FORMAT_VERSION`] (they have no sidecar and must keep their layout).
pub fn record_updates(txn: &mut Txn<'_>, updates: &[Update<'_>]) -> Result<()> {
    if txn.format_version() < MIN_FORMAT_VERSION || updates.is_empty() {
        return Ok(());
    }
    // The symbol table is tiny (one string per distinct project/agent), so
    // loading it whole inside the write transaction is cheap and keeps this
    // path free of any shared mutable cache.
    let mut symbols: HashMap<String, u32> = HashMap::new();
    for page_no in walk_chain(txn, txn.filter_symbols_page(), PageType::FilterSymbols)? {
        let page = txn.page(page_no)?;
        for (sym, s) in decode_symbols(&page, page_no)? {
            symbols.insert(s, sym);
        }
    }
    for u in updates {
        let mut flags = 0u8;
        if u.tombstone {
            flags |= FLAG_TOMBSTONE;
        }
        if u.superseded {
            flags |= FLAG_SUPERSEDED;
        }
        if u.has_metadata {
            flags |= FLAG_HAS_METADATA;
        }
        let project_sym = intern(txn, &mut symbols, u.project.unwrap_or(""))?;
        let agent_sym = intern(txn, &mut symbols, u.agent)?;
        let (project_sym, agent_sym) = match (project_sym, agent_sym) {
            (Some(p), Some(a)) => (p, a),
            // A string too large to intern (longer than a page) or a full
            // symbol space: mark the entry so scoped queries fall back to
            // the record — correctness over speed, never an error.
            _ => {
                flags |= FLAG_SCOPE_OVERFLOW;
                (SYM_NONE, SYM_NONE)
            }
        };
        let mut bytes = [0u8; ENTRY_LEN];
        bytes[..16].copy_from_slice(&u.id.to_bytes());
        bytes[16] = flags;
        bytes[17..21].copy_from_slice(&project_sym.to_le_bytes());
        bytes[21..25].copy_from_slice(&agent_sym.to_le_bytes());
        bytes[25..29].copy_from_slice(&u.doc_len.to_le_bytes());
        append_entry(txn, &bytes)?;
    }
    Ok(())
}

/// Resolves `s` to its symbol, interning it (appending to the symbol chain)
/// when new. `Ok(None)` = the string cannot be interned (too long for one
/// page, or the u32 symbol space is exhausted) — the caller flags the entry
/// instead of failing the write.
fn intern(txn: &mut Txn<'_>, symbols: &mut HashMap<String, u32>, s: &str) -> Result<Option<u32>> {
    if s.is_empty() {
        return Ok(Some(SYM_NONE));
    }
    if let Some(&sym) = symbols.get(s) {
        return Ok(Some(sym));
    }
    let page_size = txn.page_size() as usize;
    let body_cap = page_size - PAGE_HEADER_LEN - PAGE_TRAILER_LEN;
    if s.len() > u16::MAX as usize || 6 + s.len() > body_cap {
        return Ok(None);
    }
    let Some(sym) = symbols
        .values()
        .copied()
        .max()
        .unwrap_or(SYM_NONE)
        .checked_add(1)
    else {
        return Ok(None);
    };
    let mut encoded = Vec::with_capacity(6 + s.len());
    encoded.extend_from_slice(&sym.to_le_bytes());
    encoded.extend_from_slice(&(s.len() as u16).to_le_bytes());
    encoded.extend_from_slice(s.as_bytes());

    let head = txn.filter_symbols_page();
    if head != 0 {
        let page = txn.page(head)?;
        let existing = decode_symbols(&page, head)?;
        let used: usize = existing.iter().map(|(_, s)| 6 + s.len()).sum();
        if PAGE_HEADER_LEN + used + encoded.len() <= page_size - PAGE_TRAILER_LEN {
            let mut page = page;
            page[PAGE_HEADER_LEN + used..PAGE_HEADER_LEN + used + encoded.len()]
                .copy_from_slice(&encoded);
            PageHeader {
                page_type: PageType::FilterSymbols,
                entry_count: existing.len() as u32 + 1,
                next_page: PageHeader::decode(&page)
                    .ok_or(malformed(head, "page header"))?
                    .next_page,
            }
            .encode_into(&mut page);
            txn.write_page(head, &page)?;
            symbols.insert(s.to_owned(), sym);
            return Ok(Some(sym));
        }
    }
    // Head full (or no chain yet): prepend a fresh page.
    let page_no = txn.allocate_page()?;
    let mut page = vec![0u8; page_size];
    PageHeader {
        page_type: PageType::FilterSymbols,
        entry_count: 1,
        next_page: head,
    }
    .encode_into(&mut page);
    page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + encoded.len()].copy_from_slice(&encoded);
    txn.write_page(page_no, &page)?;
    txn.set_filter_symbols_page(page_no);
    symbols.insert(s.to_owned(), sym);
    Ok(Some(sym))
}

/// Appends one encoded entry to the FILTER_META chain, rewriting the head
/// page when it has room and prepending a fresh page otherwise.
fn append_entry(txn: &mut Txn<'_>, bytes: &[u8; ENTRY_LEN]) -> Result<()> {
    let page_size = txn.page_size() as usize;
    let head = txn.filter_meta_page();
    if head != 0 {
        let mut page = txn.page(head)?;
        let header = PageHeader::decode(&page).ok_or(malformed(head, "page header"))?;
        if header.page_type != PageType::FilterMeta {
            return Err(malformed(head, "not a FILTER_META page"));
        }
        let count = header.entry_count as usize;
        if count < entry_capacity(page_size) {
            let off = PAGE_HEADER_LEN + count * ENTRY_LEN;
            page[off..off + ENTRY_LEN].copy_from_slice(bytes);
            PageHeader {
                page_type: PageType::FilterMeta,
                entry_count: count as u32 + 1,
                next_page: header.next_page,
            }
            .encode_into(&mut page);
            return txn.write_page(head, &page);
        }
    }
    let page_no = txn.allocate_page()?;
    let mut page = vec![0u8; page_size];
    PageHeader {
        page_type: PageType::FilterMeta,
        entry_count: 1,
        next_page: head,
    }
    .encode_into(&mut page);
    page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + ENTRY_LEN].copy_from_slice(bytes);
    txn.write_page(page_no, &page)?;
    txn.set_filter_meta_page(page_no);
    Ok(())
}

fn malformed(page_no: u64, what: &'static str) -> Error {
    Error::MalformedPage { page_no, what }
}

fn read_u16(buf: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(off..off + 2)?.try_into().ok()?))
}

fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

/// Fuzz entry point (`fuzz_filter_meta_page`, `docs/TESTING.md` §3): both
/// page decoders over raw bytes. Must return — never panic, OOM or loop —
/// on arbitrary input.
#[doc(hidden)]
pub fn fuzz_decode_page(data: &[u8]) {
    let _ = decode_entries(data, 1);
    let _ = decode_symbols(data, 1);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::storage::sim::{SimVfs, SplitMix64};
    use crate::storage::vfs::Vfs;
    use crate::storage::{Pager, PagerOptions};
    use std::path::Path;
    use std::sync::Arc;

    fn pager(page_size: u32) -> Pager {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        Pager::create(
            vfs,
            Path::new("m.mind"),
            PagerOptions {
                page_size,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn update<'a>(id: Ulid, project: Option<&'a str>, agent: &'a str) -> Update<'a> {
        Update {
            id,
            tombstone: false,
            superseded: false,
            has_metadata: false,
            project,
            agent,
            doc_len: 7,
        }
    }

    fn table(pager: &Pager) -> Table {
        load(
            pager,
            pager.header().filter_meta_page,
            pager.header().filter_symbols_page,
        )
        .unwrap()
    }

    #[test]
    fn roundtrips_entries_and_symbols_across_page_boundaries() {
        // 512-byte pages hold few entries, so 100 updates span several pages
        // of both chains.
        let mut pager = pager(512);
        let mut ids = Vec::new();
        let mut txn = pager.begin().unwrap();
        for i in 0..100u64 {
            let id = Ulid::from_parts(i, u128::from(i));
            ids.push(id);
            let project = if i % 3 == 0 { None } else { Some("proj-a") };
            let agent = if i % 2 == 0 { "cli" } else { "claude-code" };
            let mut u = update(id, project, agent);
            u.doc_len = i as u32;
            u.has_metadata = i % 5 == 0;
            record_updates(&mut txn, &[u]).unwrap();
        }
        txn.commit().unwrap();

        let t = table(&pager);
        assert_eq!(t.entries.len(), 100);
        for (i, id) in ids.iter().enumerate() {
            let e = t.get(*id).unwrap();
            assert_eq!(e.doc_len, i as u32);
            let needs = QueryNeeds {
                project: t.want_project(Some("proj-a")),
                agent: Want::Any,
                has_metadata_filters: false,
            };
            let expect = if i % 3 == 0 {
                Decision::Reject(RejectReason::OutOfScope)
            } else {
                Decision::Accept
            };
            assert_eq!(t.decide(*id, &needs), expect, "entry {i}");
        }
        // Both distinct agents interned once each, plus the project.
        assert_eq!(t.symbols.len(), 3);
    }

    #[test]
    fn newest_update_wins_and_dead_flags_reject() {
        let mut pager = pager(4096);
        let id = Ulid::from_parts(1, 1);
        let mut txn = pager.begin().unwrap();
        record_updates(&mut txn, &[update(id, Some("p"), "a")]).unwrap();
        txn.commit().unwrap();

        let needs = QueryNeeds {
            project: Want::Any,
            agent: Want::Any,
            has_metadata_filters: false,
        };
        assert_eq!(table(&pager).decide(id, &needs), Decision::Accept);

        // A forget appends a tombstoned entry for the same id.
        let mut txn = pager.begin().unwrap();
        let mut u = update(id, Some("p"), "a");
        u.tombstone = true;
        record_updates(&mut txn, &[u]).unwrap();
        txn.commit().unwrap();
        assert_eq!(
            table(&pager).decide(id, &needs),
            Decision::Reject(RejectReason::Dead)
        );

        // Superseded rejects the same way.
        let id2 = Ulid::from_parts(2, 2);
        let mut txn = pager.begin().unwrap();
        let mut u = update(id2, None, "a");
        u.superseded = true;
        record_updates(&mut txn, &[u]).unwrap();
        txn.commit().unwrap();
        assert_eq!(
            table(&pager).decide(id2, &needs),
            Decision::Reject(RejectReason::Dead)
        );
    }

    #[test]
    fn metadata_filters_reject_without_metadata_and_defer_with_it() {
        let mut pager = pager(4096);
        let bare = Ulid::from_parts(1, 1);
        let tagged = Ulid::from_parts(2, 2);
        let mut txn = pager.begin().unwrap();
        record_updates(&mut txn, &[update(bare, None, "a")]).unwrap();
        let mut u = update(tagged, None, "a");
        u.has_metadata = true;
        record_updates(&mut txn, &[u]).unwrap();
        txn.commit().unwrap();

        let t = table(&pager);
        let needs = QueryNeeds {
            project: Want::Any,
            agent: Want::Any,
            has_metadata_filters: true,
        };
        assert_eq!(
            t.decide(bare, &needs),
            Decision::Reject(RejectReason::FilteredOut)
        );
        assert_eq!(t.decide(tagged, &needs), Decision::NeedRecord);
        // An id the sidecar has never seen defers too.
        assert_eq!(
            t.decide(Ulid::from_parts(9, 9), &needs),
            Decision::NeedRecord
        );
    }

    #[test]
    fn unresolvable_query_strings_reject_and_agent_want_filters() {
        let mut pager = pager(4096);
        let id = Ulid::from_parts(1, 1);
        let mut txn = pager.begin().unwrap();
        record_updates(&mut txn, &[update(id, Some("p"), "agent-a")]).unwrap();
        txn.commit().unwrap();
        let t = table(&pager);

        // A project no record ever used is Want::Absent ⇒ reject, no load.
        let needs = QueryNeeds {
            project: t.want_project(Some("never-seen")),
            agent: Want::Any,
            has_metadata_filters: false,
        };
        assert_eq!(
            t.decide(id, &needs),
            Decision::Reject(RejectReason::OutOfScope)
        );
        // Agent mismatch rejects; agent match accepts.
        let wrong = QueryNeeds {
            project: Want::Any,
            agent: t.want_agent(Some("agent-a")),
            has_metadata_filters: false,
        };
        assert_eq!(t.decide(id, &wrong), Decision::Accept);
        let other = Ulid::from_parts(2, 2);
        let mut txn = pager.begin().unwrap();
        record_updates(&mut txn, &[update(other, Some("p"), "agent-b")]).unwrap();
        txn.commit().unwrap();
        let t = table(&pager);
        let needs = QueryNeeds {
            project: Want::Any,
            agent: t.want_agent(Some("agent-a")),
            has_metadata_filters: false,
        };
        assert_eq!(
            t.decide(other, &needs),
            Decision::Reject(RejectReason::OutOfScope)
        );
    }

    #[test]
    fn oversized_strings_flag_scope_overflow_and_defer_scoped_queries() {
        // A project longer than a 512-byte page cannot be interned: the entry
        // must flag it and scoped queries fall back to the record, while
        // unscoped liveness checks still work sidecar-only.
        let mut pager = pager(512);
        let id = Ulid::from_parts(1, 1);
        let huge = "p".repeat(600);
        let mut txn = pager.begin().unwrap();
        record_updates(&mut txn, &[update(id, Some(&huge), "a")]).unwrap();
        txn.commit().unwrap();
        let t = table(&pager);

        let unscoped = QueryNeeds {
            project: Want::Any,
            agent: Want::Any,
            has_metadata_filters: false,
        };
        assert_eq!(t.decide(id, &unscoped), Decision::Accept);
        let scoped = QueryNeeds {
            project: t.want_project(Some(huge.as_str())),
            agent: Want::Any,
            has_metadata_filters: false,
        };
        assert_eq!(t.decide(id, &scoped), Decision::NeedRecord);
    }

    #[test]
    fn old_format_versions_write_nothing() {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new());
        let mut pager = Pager::create(
            vfs,
            Path::new("m.mind"),
            PagerOptions {
                format_version: 6,
                ..Default::default()
            },
        )
        .unwrap();
        let mut txn = pager.begin().unwrap();
        record_updates(&mut txn, &[update(Ulid::from_parts(1, 1), Some("p"), "a")]).unwrap();
        // Nothing buffered: the commit is a no-op on a pre-sidecar file.
        assert_eq!(txn.filter_meta_page(), 0);
        assert_eq!(txn.filter_symbols_page(), 0);
        txn.commit().unwrap();
        assert_eq!(pager.header().filter_meta_page, 0);
        assert_eq!(pager.header().filter_symbols_page, 0);
    }

    #[test]
    fn decoders_never_panic_on_arbitrary_bytes() {
        let mut rng = SplitMix64(0xF117E);
        for _ in 0..2000 {
            let len = (rng.next_u64() % 4096) as usize;
            let mut buf = vec![0u8; len];
            for b in &mut buf {
                *b = rng.next_u64() as u8;
            }
            fuzz_decode_page(&buf);
        }
        // Mutated valid pages exercise deeper paths.
        let mut pager = pager(512);
        let mut txn = pager.begin().unwrap();
        for i in 0..40u64 {
            record_updates(
                &mut txn,
                &[update(Ulid::from_parts(i, 1), Some("proj"), "agent")],
            )
            .unwrap();
        }
        txn.commit().unwrap();
        for page_no in 1..pager.page_count() {
            let page = pager.read_page(page_no).unwrap();
            for _ in 0..200 {
                let mut mutated = page.clone();
                let i = (rng.next_u64() as usize) % mutated.len();
                mutated[i] ^= (rng.next_u64() as u8) | 1;
                fuzz_decode_page(&mutated);
            }
        }
    }

    #[test]
    fn load_rejects_chain_cycles_as_typed_errors() {
        // Hand-build a FILTER_META page whose next_page points at itself.
        let mut pager = pager(512);
        let mut txn = pager.begin().unwrap();
        let page_no = txn.allocate_page().unwrap();
        let mut page = vec![0u8; 512];
        PageHeader {
            page_type: PageType::FilterMeta,
            entry_count: 0,
            next_page: page_no,
        }
        .encode_into(&mut page);
        txn.write_page(page_no, &page).unwrap();
        txn.set_filter_meta_page(page_no);
        txn.commit().unwrap();
        assert!(matches!(
            load(&pager, pager.header().filter_meta_page, 0),
            Err(Error::MalformedPage { .. })
        ));
    }
}
