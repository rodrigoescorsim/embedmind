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
}

/// Reads a term's postings from the dictionary, or `None` if absent.
fn postings_for(src: &dyn PageSource, root: u64, term: &[u8]) -> Result<Option<Postings>> {
    match dict::get(src, FTS_DICT, root, term)? {
        Some((body, page_no)) => Ok(Some(Postings::decode(&body, page_no)?)),
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

    let mut root = meta.dict_root;
    for (term, tf) in terms {
        let term_bytes = term.as_bytes();
        let mut postings = postings_for(txn, root, term_bytes)?.unwrap_or_default();
        postings.upsert(record_id, tf);
        root = dict::upsert(txn, FTS_DICT, root, term_bytes, &postings.encode())?;
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

/// Fuzz-only surface: decode one page as each FTS node kind and as postings,
/// exercising every parser branch. Must return, never panic (`fuzz_fts_page`
/// target, `docs/TESTING.md` §3).
#[doc(hidden)]
pub fn fuzz_decode_page(page: &[u8]) {
    dict::fuzz_decode_node(page, FTS_DICT);
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
