//! Memory record encoding (`docs/FORMAT.md` §5) — format layer.
//!
//! This is the on-disk shape of one memory. Every field is explicitly
//! (de)serialized little-endian (the ULID key is the one deliberate
//! big-endian exception, per the ULID spec, so byte order == time order).
//! Decoding validates every length prefix against the remaining input
//! *before* allocating, and never panics — this module is a fuzz target
//! (`fuzz_record`, `docs/TESTING.md` §3).
//!
//! The public API type is [`crate::api::Memory`]; this record type carries
//! the extra storage-only field (`vec_ref`) and the exact byte layout.

use std::collections::BTreeMap;

use ulid::Ulid;

use crate::error::{Error, Result};

/// Hard cap on the encoded size of one record (content included). Guards
/// decoders against hostile length prefixes and keeps any single memory a
/// sane size; callers get a typed error, never an OOM.
pub const MAX_RECORD_LEN: usize = 32 * 1024 * 1024;

/// Maximum number of metadata entries per record (the on-disk count is u16).
pub const MAX_METADATA_ENTRIES: usize = u16::MAX as usize;

/// Reference to an embedding slot inside a VECTOR page (`docs/FORMAT.md` §5).
/// Page 0 is always the header, so the all-zero encoding is reserved for
/// "no embedding" and never collides with a real reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VecRef {
    /// VECTOR page holding the embedding.
    pub page_no: u64,
    /// Slot index within that page.
    pub slot: u16,
}

/// A typed metadata value (`docs/FORMAT.md` §5 tagged scalar).
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    /// Tag 0.
    Null,
    /// Tag 1.
    Bool(bool),
    /// Tag 2.
    I64(i64),
    /// Tag 3.
    F64(f64),
    /// Tag 4.
    Str(String),
}

impl Scalar {
    /// This scalar as an `f64` if it is numeric (`I64`/`F64`), for range
    /// comparisons. `None` for the non-ordered types (`Null`/`Bool`/`Str`).
    fn as_ordered(&self) -> Option<f64> {
        match self {
            Scalar::I64(v) => Some(*v as f64),
            Scalar::F64(v) => Some(*v),
            _ => None,
        }
    }
}

/// One metadata-filter predicate on a single key (S10, `docs/01-spec.md`).
/// A [`crate::api::Query`] carries a map of `key → Filter`; a memory passes
/// only when it satisfies **every** entry (AND semantics).
///
/// Filters read a memory's stored [`Scalar`] for the key and either accept it
/// or reject it. Two kinds:
///
/// - [`Filter::Eq`] — exact match against a scalar of the *same* type. Matching
///   an integer filter against a stored string is a type mismatch (a typed
///   error), not a silent miss: comparing across types is a caller bug worth
///   surfacing (`docs/01-spec.md` S10 edge). A missing key is a plain non-match
///   (0 hits), never an error.
/// - [`Filter::Range`] — half-open-or-closed numeric window `[min?, max?]` over
///   the ordered types (`I64`/`F64`, compared as `f64`). Applying a range to a
///   stored non-numeric value (string/bool/null) is the same typed mismatch.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Stored value must equal this scalar, and be the same type. A stored
    /// value of a different type is a type mismatch (typed error).
    Eq(Scalar),
    /// Stored value (numeric) must fall within the inclusive bounds. `None`
    /// bound = open on that side. A stored non-numeric value is a mismatch.
    Range {
        /// Inclusive lower bound, or `None` for unbounded below.
        min: Option<f64>,
        /// Inclusive upper bound, or `None` for unbounded above.
        max: Option<f64>,
    },
}

impl Filter {
    /// Evaluates this predicate against the value stored under its key, or
    /// `None` when the memory has no such key.
    ///
    /// - Missing key ⇒ `Ok(false)` — a non-match, not an error (S10 edge: a
    ///   filter on an absent key yields 0 hits, never a failure).
    /// - Type mismatch (e.g. an `Eq(I64)` filter over a stored string, or any
    ///   `Range` over a non-numeric value) ⇒ `Err(InvalidArgument)`, so the
    ///   caller learns its filter cannot apply instead of silently dropping
    ///   every hit.
    pub fn matches(&self, stored: Option<&Scalar>) -> Result<bool> {
        let Some(stored) = stored else {
            return Ok(false); // absent key: honest non-match, never an error
        };
        match self {
            Filter::Eq(want) => {
                if std::mem::discriminant(want) != std::mem::discriminant(stored) {
                    return Err(Error::InvalidArgument(
                        "recall filter type mismatch: stored value has a different type",
                    ));
                }
                Ok(want == stored)
            }
            Filter::Range { min, max } => {
                let Some(value) = stored.as_ordered() else {
                    return Err(Error::InvalidArgument(
                        "recall range filter requires a numeric stored value",
                    ));
                };
                Ok(min.is_none_or(|lo| value >= lo) && max.is_none_or(|hi| value <= hi))
            }
        }
    }
}

/// Who wrote a memory, and when. Basic provenance is free (the seed of the
/// premium traceability tier — CLAUDE.md decision 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Writing agent (`"claude-code"`, `"cli"`, …); empty = unknown.
    pub agent: String,
    /// Agent session, if the shell provides one.
    pub session_id: Option<String>,
    /// Creation time, microseconds since the Unix epoch, UTC.
    pub created_at_micros: i64,
}

/// One memory as stored in a B-tree leaf (`docs/FORMAT.md` §5).
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecord {
    /// Time-ordered key (the timeline comes free with the ULID sort order).
    pub id: Ulid,
    /// Soft-delete marker: `forget` sets it, `vacuum` reclaims the space
    /// (`docs/adr/0003`).
    pub tombstone: bool,
    /// The memory text.
    pub content: String,
    /// Embedding location; `None` until the vector layer (M1 item 1.3)
    /// writes one.
    pub vec_ref: Option<VecRef>,
    /// Project scope; `None` = global.
    pub project: Option<String>,
    /// Who/when.
    pub provenance: Provenance,
    /// Free-form typed metadata.
    pub metadata: BTreeMap<String, Scalar>,
}

/// Record flags: bit 0 = tombstone. Other bits are reserved (written zero,
/// ignored on read — FORMAT.md §2).
const FLAG_TOMBSTONE: u8 = 1;

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_I64: u8 = 2;
const TAG_F64: u8 = 3;
const TAG_STR: u8 = 4;

impl MemoryRecord {
    /// Encodes the record. Fails (typed) if any length field overflows its
    /// prefix or the total exceeds [`MAX_RECORD_LEN`].
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.metadata.len() > MAX_METADATA_ENTRIES {
            return Err(Error::InvalidArgument("too many metadata entries"));
        }
        let mut out = Vec::with_capacity(64 + self.content.len());
        out.extend_from_slice(&self.id.to_bytes()); // big-endian per ULID spec
        out.push(if self.tombstone { FLAG_TOMBSTONE } else { 0 });
        put_str(&mut out, &self.content)?;
        let (page_no, slot) = match self.vec_ref {
            Some(v) => (v.page_no, v.slot),
            None => (0, 0),
        };
        out.extend_from_slice(&page_no.to_le_bytes());
        out.extend_from_slice(&slot.to_le_bytes());
        put_str(&mut out, self.project.as_deref().unwrap_or(""))?;
        put_str(&mut out, &self.provenance.agent)?;
        put_str(
            &mut out,
            self.provenance.session_id.as_deref().unwrap_or(""),
        )?;
        out.extend_from_slice(&self.provenance.created_at_micros.to_le_bytes());
        out.extend_from_slice(&(self.metadata.len() as u16).to_le_bytes());
        for (key, value) in &self.metadata {
            put_str(&mut out, key)?;
            match value {
                Scalar::Null => out.push(TAG_NULL),
                Scalar::Bool(b) => {
                    out.push(TAG_BOOL);
                    out.push(u8::from(*b));
                }
                Scalar::I64(v) => {
                    out.push(TAG_I64);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                Scalar::F64(v) => {
                    out.push(TAG_F64);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                Scalar::Str(s) => {
                    out.push(TAG_STR);
                    put_str(&mut out, s)?;
                }
            }
        }
        if out.len() > MAX_RECORD_LEN {
            return Err(Error::InvalidArgument("record exceeds MAX_RECORD_LEN"));
        }
        Ok(out)
    }

    /// Decodes a record, consuming the whole input (trailing bytes are an
    /// error — cells store exact lengths). Never panics; every length prefix
    /// is validated against the remaining input before any allocation.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() > MAX_RECORD_LEN {
            return Err(Error::MalformedRecord("record exceeds MAX_RECORD_LEN"));
        }
        let mut r = Reader { buf, pos: 0 };
        let id = Ulid::from_bytes(
            r.bytes(16)?
                .try_into()
                .map_err(|_| Error::MalformedRecord("id"))?,
        );
        let flags = r.u8()?;
        let tombstone = flags & FLAG_TOMBSTONE != 0;
        // Reserved flag bits are ignored on read (FORMAT.md §2).
        let content = r.string("content")?;
        let page_no = r.u64()?;
        let slot = r.u16()?;
        let vec_ref = if page_no == 0 && slot == 0 {
            None
        } else {
            Some(VecRef { page_no, slot })
        };
        let project = non_empty(r.string("project")?);
        let agent = r.string("provenance.agent")?;
        let session_id = non_empty(r.string("provenance.session_id")?);
        let created_at_micros = r.i64()?;
        let count = r.u16()? as usize;
        let mut metadata = BTreeMap::new();
        for _ in 0..count {
            let key = r.string("metadata key")?;
            let value = match r.u8()? {
                TAG_NULL => Scalar::Null,
                TAG_BOOL => match r.u8()? {
                    0 => Scalar::Bool(false),
                    1 => Scalar::Bool(true),
                    _ => return Err(Error::MalformedRecord("bool payload")),
                },
                TAG_I64 => Scalar::I64(i64::from_le_bytes(
                    r.bytes(8)?
                        .try_into()
                        .map_err(|_| Error::MalformedRecord("i64 payload"))?,
                )),
                TAG_F64 => Scalar::F64(f64::from_le_bytes(
                    r.bytes(8)?
                        .try_into()
                        .map_err(|_| Error::MalformedRecord("f64 payload"))?,
                )),
                TAG_STR => Scalar::Str(r.string("string payload")?),
                _ => return Err(Error::MalformedRecord("unknown scalar tag")),
            };
            if metadata.insert(key, value).is_some() {
                return Err(Error::MalformedRecord("duplicate metadata key"));
            }
        }
        if r.pos != buf.len() {
            return Err(Error::MalformedRecord("trailing bytes"));
        }
        Ok(MemoryRecord {
            id,
            tombstone,
            content,
            vec_ref,
            project,
            provenance: Provenance {
                agent,
                session_id,
                created_at_micros,
            },
            metadata,
        })
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Length-prefixed string writer (u32 length + UTF-8, FORMAT.md §2).
fn put_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = u32::try_from(s.len()).map_err(|_| Error::InvalidArgument("string too long"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Bounds-checked sequential reader. Every accessor validates against the
/// remaining input; nothing here can panic.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or(Error::MalformedRecord("truncated"))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.bytes(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    /// Length-prefixed UTF-8 string. The length is checked against the
    /// remaining bytes before anything is copied (docs/TESTING.md §3 rule:
    /// fuzzers find unchecked-length OOMs in minutes).
    fn string(&mut self, what: &'static str) -> Result<String> {
        let len = self.u32()? as usize;
        if len > self.buf.len().saturating_sub(self.pos) {
            return Err(Error::MalformedRecord(what));
        }
        String::from_utf8(self.bytes(len)?.to_vec()).map_err(|_| Error::MalformedRecord(what))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample() -> MemoryRecord {
        let mut metadata = BTreeMap::new();
        metadata.insert("lang".to_owned(), Scalar::Str("rust".to_owned()));
        metadata.insert("stars".to_owned(), Scalar::I64(-3));
        metadata.insert("score".to_owned(), Scalar::F64(0.5));
        metadata.insert("done".to_owned(), Scalar::Bool(true));
        metadata.insert("nothing".to_owned(), Scalar::Null);
        MemoryRecord {
            id: Ulid::from_parts(1_700_000_000_000, 42),
            tombstone: false,
            content: "the founder prefers explicit errors — memória número 1".to_owned(),
            vec_ref: Some(VecRef {
                page_no: 12,
                slot: 3,
            }),
            project: Some("embedmind".to_owned()),
            provenance: Provenance {
                agent: "claude-code".to_owned(),
                session_id: Some("sess-1".to_owned()),
                created_at_micros: 1_751_900_000_000_000,
            },
            metadata,
        }
    }

    #[test]
    fn roundtrip_full() {
        let rec = sample();
        let bytes = rec.encode().unwrap();
        assert_eq!(MemoryRecord::decode(&bytes).unwrap(), rec);
    }

    #[test]
    fn roundtrip_minimal_and_tombstone() {
        let rec = MemoryRecord {
            id: Ulid::from_parts(0, 0),
            tombstone: true,
            content: String::new(),
            vec_ref: None,
            project: None,
            provenance: Provenance {
                agent: String::new(),
                session_id: None,
                created_at_micros: -1,
            },
            metadata: BTreeMap::new(),
        };
        let bytes = rec.encode().unwrap();
        let back = MemoryRecord::decode(&bytes).unwrap();
        assert_eq!(back, rec);
        assert!(back.tombstone);
    }

    #[test]
    fn ulid_key_order_is_time_order() {
        let older = Ulid::from_parts(1000, u128::MAX); // max randomness, older ms
        let newer = Ulid::from_parts(1001, 0);
        assert!(older.to_bytes() < newer.to_bytes());
    }

    #[test]
    fn rejects_trailing_and_truncated() {
        let mut bytes = sample().encode().unwrap();
        bytes.push(0);
        assert!(matches!(
            MemoryRecord::decode(&bytes),
            Err(Error::MalformedRecord("trailing bytes"))
        ));
        let bytes = sample().encode().unwrap();
        for cut in [0, 5, 16, 17, 20, bytes.len() - 1] {
            assert!(MemoryRecord::decode(&bytes[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn rejects_hostile_length_prefix_without_allocating() {
        // content length = u32::MAX with 5 bytes of input: must fail fast.
        let mut buf = vec![0u8; 17]; // id + flags
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.push(b'x');
        assert!(matches!(
            MemoryRecord::decode(&buf),
            Err(Error::MalformedRecord(_))
        ));
    }

    #[test]
    fn decode_never_panics_on_arbitrary_bytes() {
        // Seeded smoke test; the real fuzz_record target builds on this
        // (docs/TESTING.md §3).
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 512) as usize;
            let mut buf = vec![0u8; len];
            for b in &mut buf {
                *b = next() as u8;
            }
            let _ = MemoryRecord::decode(&buf); // must return, never panic
        }
        // Mutated valid encodings exercise deeper paths.
        let valid = sample().encode().unwrap();
        for _ in 0..2000 {
            let mut buf = valid.clone();
            let i = (next() as usize) % buf.len();
            buf[i] ^= (next() as u8) | 1;
            let _ = MemoryRecord::decode(&buf);
        }
    }

    // --- Metadata filters (S10) --------------------------------------------

    #[test]
    fn filter_eq_matches_same_type_and_value() {
        let hit = Scalar::Str("ops".into());
        assert!(Filter::Eq(hit.clone()).matches(Some(&hit)).unwrap());
        assert!(
            !Filter::Eq(Scalar::Str("ops".into()))
                .matches(Some(&Scalar::Str("design".into())))
                .unwrap(),
            "same type, different value ⇒ no match"
        );
        assert!(
            Filter::Eq(Scalar::I64(3))
                .matches(Some(&Scalar::I64(3)))
                .unwrap()
        );
        assert!(
            Filter::Eq(Scalar::Bool(true))
                .matches(Some(&Scalar::Bool(true)))
                .unwrap()
        );
    }

    #[test]
    fn filter_on_absent_key_is_non_match_not_error() {
        assert!(!Filter::Eq(Scalar::I64(1)).matches(None).unwrap());
        assert!(
            !Filter::Range {
                min: Some(0.0),
                max: Some(1.0)
            }
            .matches(None)
            .unwrap()
        );
    }

    #[test]
    fn filter_eq_type_mismatch_is_typed_error() {
        // Integer filter over a stored string, and vice-versa.
        assert!(matches!(
            Filter::Eq(Scalar::I64(3)).matches(Some(&Scalar::Str("x".into()))),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            Filter::Eq(Scalar::Str("x".into())).matches(Some(&Scalar::I64(3))),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn filter_range_over_numeric_types_and_bounds() {
        let range = |min, max| Filter::Range { min, max };
        // Closed window over an i64.
        assert!(
            range(Some(4.0), Some(10.0))
                .matches(Some(&Scalar::I64(5)))
                .unwrap()
        );
        assert!(
            !range(Some(4.0), Some(10.0))
                .matches(Some(&Scalar::I64(1)))
                .unwrap()
        );
        // Inclusive bounds.
        assert!(
            range(Some(4.0), Some(10.0))
                .matches(Some(&Scalar::I64(4)))
                .unwrap()
        );
        assert!(
            range(Some(4.0), Some(10.0))
                .matches(Some(&Scalar::I64(10)))
                .unwrap()
        );
        // Open-ended.
        assert!(
            range(Some(0.5), None)
                .matches(Some(&Scalar::F64(0.9)))
                .unwrap()
        );
        assert!(
            !range(Some(0.5), None)
                .matches(Some(&Scalar::F64(0.2)))
                .unwrap()
        );
        assert!(
            range(None, Some(0.5))
                .matches(Some(&Scalar::F64(0.2)))
                .unwrap()
        );
        // Numeric filters cross the i64/f64 line: an f64 window over an i64.
        assert!(
            range(Some(2.5), Some(3.5))
                .matches(Some(&Scalar::I64(3)))
                .unwrap()
        );
    }

    #[test]
    fn filter_range_over_non_numeric_is_typed_error() {
        let range = Filter::Range {
            min: Some(0.0),
            max: Some(1.0),
        };
        for stored in [Scalar::Str("x".into()), Scalar::Bool(true), Scalar::Null] {
            assert!(
                matches!(range.matches(Some(&stored)), Err(Error::InvalidArgument(_))),
                "range over {stored:?} must be a typed error"
            );
        }
    }
}
