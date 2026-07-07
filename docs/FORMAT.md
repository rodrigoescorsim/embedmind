# EmbedMind `.mind` File Format Specification

> **Status: DRAFT (pre-v0.1).** This spec is written *before* the implementation, on purpose
> (spec-driven development for the on-disk contract). Field offsets and encodings may change
> until v0.1 ships. **From v0.1 on, this document is normative**: the format is public,
> versioned, and never breaks without a migration path (`embedmind migrate`).

Companion documents: [DESIGN.md](../DESIGN.md) explains *why* these choices were made
(see §3–§5); this document defines *exactly what the bytes are*. Fuzz targets and the
crash-test harness ([TESTING.md](TESTING.md)) treat this spec as the source of truth.

---

## 1. Overview and guarantees

An EmbedMind store is:

- **One main file** (`memory.mind`) organized in fixed-size pages.
- **One transient sidecar** (`memory.mind-wal`) that exists only between checkpoints
  (SQLite model). A cleanly closed store is a single file.

Format-level guarantees:

| # | Guarantee |
|---|---|
| G1 | Every page carries a checksum; silent corruption is detected on read, never propagated. |
| G2 | A crash (power loss, `kill -9`) at any byte boundary leaves the store recoverable: all committed transactions present, no partial transaction visible. |
| G3 | The file is byte-identical across platforms (fixed little-endian; no platform-dependent layout is ever written). |
| G4 | A reader that does not know `format_version` N refuses clearly or opens read-only — it never guesses. |

## 2. Encoding conventions

- **Endianness:** all multi-byte integers are **little-endian**, always.
- **Integers:** fixed-width (`u16`/`u32`/`u64`). No varints in v1 (simplicity > space; pages are the compression unit anyway).
- **Strings / blobs:** `u32` byte-length prefix + UTF-8 bytes (strings) or raw bytes (blobs). No NUL terminators.
- **Checksums:** `xxh3_64` (64-bit). Stored little-endian like everything else.
- **Nothing is `memcpy`'d from structs.** Every field is explicitly (de)serialized so the parsers are fuzzable and layout is compiler-independent (DESIGN §3.1).
- **Reserved bytes** MUST be written as zero and MUST be ignored on read.

## 3. Pages

- Default `page_size` = **4096 bytes** (recorded in the header; readers use the recorded value).
- Pages are addressed by `page_no: u64`, byte offset = `page_no * page_size`. Page 0 is the header.
- **Every page** ends with an 8-byte trailer: `xxh3_64` over bytes `[0, page_size - 8)` of that page. A checksum mismatch on read is a hard error (`ErrorKind::CorruptPage`) — never silently skipped.
- Common 16-byte page header (all page types except page 0):

| offset | size | field |
|---|---|---|
| 0 | 1 | `page_type` (see §3.1) |
| 1 | 3 | reserved (zero) |
| 4 | 4 | `entry_count` (u32) — meaning depends on type |
| 8 | 8 | `next_page` (u64) — overflow/freelist chaining; 0 = none |

### 3.1 Page types

| id | type | content |
|---|---|---|
| 0x01 | BTREE_INNER | B-tree interior node (key → child page) |
| 0x02 | BTREE_LEAF | memory records (see §5) |
| 0x03 | VECTOR | embedding blocks, aligned (see §6) |
| 0x04 | HNSW_NODE | HNSW graph nodes and adjacency (see §7) |
| 0x05 | HNSW_META | HNSW index parameters + entry point |
| 0x06 | FREELIST | array of free `page_no`s |
| 0x07 | OVERFLOW | continuation of an oversized record |

## 4. Header (page 0)

| offset | size | field | notes |
|---|---|---|---|
| 0 | 8 | magic | ASCII `MINDFMT1` |
| 8 | 4 | `format_version` (u32) | 1 for v0.1 |
| 12 | 4 | `page_size` (u32) | default 4096 |
| 16 | 8 | `page_count` (u64) | total pages incl. header |
| 24 | 8 | `root_btree_page` (u64) | record B-tree root |
| 32 | 8 | `freelist_page` (u64) | 0 = empty freelist |
| 40 | 8 | `hnsw_meta_page` (u64) | 0 = no vector index yet |
| 48 | 8 | `txn_counter` (u64) | last committed transaction id |
| 56 | 2 | `embedding_dims` (u16) | e.g. 384 |
| 58 | 2 | `embedding_quant` (u16) | 0 = f32, 1 = i8 (reserved for M3) |
| 60 | 4+n | `embedding_model_id` | length-prefixed UTF-8, max 64 bytes |
| 128 | 4 | `flags` (u32) | bit 0 = `encrypted` (**reserved**, must be 0 in v1) |
| 132 | 16 | `kdf_salt` | reserved for encryption (premium), zero in v1 |
| 148 | 8 | `kdf_params` | reserved, zero in v1 |
| 156 | … | reserved (zero) | up to trailer |
| 4088 | 8 | header checksum | `xxh3_64` over bytes `[0, 4088)` |

**Version policy (G4):** a reader seeing `format_version` greater than it understands MUST
refuse to open read-write. It MAY open read-only if the major layout (this table) is
unchanged. Migrations are always copy-based (`embedmind migrate` writes a new file),
never destructive in-place.

**Encryption reservation:** the `encrypted` flag, `kdf_salt`, and `kdf_params` exist so the
premium encryption module (AES-256-GCM per page, nonce = `page_no` + epoch) can ship
without a format break. v1 writers zero them; v1 readers MUST refuse files with bit 0 set.

## 5. Memory records (BTREE_LEAF)

Records are keyed by **ULID** (16 bytes, time-ordered — gives the timeline for free).
Leaf pages use a slotted layout: slot directory grows from the header, record bytes grow
from the tail. A record that does not fit in one page spills to an OVERFLOW page chain.

Record encoding (all fields explicit, in order):

| field | encoding |
|---|---|
| `id` | 16 bytes (ULID, big-endian byte order as per ULID spec — the one deliberate exception, kept for sortability) |
| `flags` | u8 — bit 0 = `tombstone` |
| `content` | length-prefixed UTF-8 |
| `vec_ref` | `page_no: u64` + `slot: u16` into a VECTOR page; all-zero = no embedding. When the content was chunked (§7), this is the **first** chunk's vector |
| `project` | length-prefixed UTF-8; length 0 = none |
| `provenance.agent` | length-prefixed UTF-8 (`"claude-code"`, `"cli"`, …) |
| `provenance.session_id` | length-prefixed UTF-8; length 0 = none |
| `provenance.created_at` | i64, microseconds since Unix epoch, UTC |
| `metadata` | u16 count, then per entry: length-prefixed key + tagged scalar |

Tagged scalar: 1 tag byte (`0` = null, `1` = bool(u8), `2` = i64, `3` = f64, `4` = string) + payload.

`forget` sets the tombstone bit (soft delete). Space and index entries are reclaimed only
by `embedmind vacuum`, which rebuilds pages and the HNSW index (DESIGN decision #3).

### 5.1 B-tree page layout

**Leaf (`BTREE_LEAF`, 0x02).** After the common 16-byte header (`entry_count` = number of
slots, `next_page` = 0, reserved):

| region | layout |
|---|---|
| slot directory | at offset 16, `entry_count` × 20-byte slots, **sorted strictly ascending by key**: `key` (16, ULID bytes) · `cell_offset` (u16) · `cell_length` (u16) |
| cells | anywhere in `[slot directory end, page_size − 8)`; writers pack them from the tail |

Cell encoding (first byte is a tag):

| tag | layout | meaning |
|---|---|---|
| 0x00 | record bytes follow (`cell_length − 1` bytes) | inline record (§5) |
| 0x01 | `total_len` (u32) · `first_page` (u64) — cell_length = 13 | record lives in an OVERFLOW chain |

A value is stored inline iff its slot + cell footprint is at most **usable/4**, where
`usable = page_size − 24` (header + checksum trailer). This cap is what makes leaf
splits provably safe: a leaf holds at most `usable + usable/4` bytes of entries after an
upsert, so cutting at the byte midpoint always yields two halves that fit.

**Inner (`BTREE_INNER`, 0x01).** After the common header (`entry_count` = number of
separators, ≥ 1): `rightmost_child` (u64) at offset 16, then `entry_count` × 24-byte
entries: `key` (16) · `child` (u64), sorted strictly ascending. `child` covers keys
`<= key`; `rightmost_child` covers keys greater than every separator. Null (0) children
are invalid.

**Overflow (`OVERFLOW`, 0x07).** Common header with `entry_count` = payload bytes in
this page (1 ≤ n ≤ usable) and `next_page` chaining; payload starts at offset 16. The
referencing cell records the exact `total_len`; readers stop after consuming it, so
chains are cycle-proof by construction. Hard cap: one record ≤ **32 MiB**
(`MAX_RECORD_LEN`) — a hostile `total_len` is a typed error before any allocation.

**Updates and deletion.** Upsert rewrites the leaf in place (same page number; the WAL
makes it atomic). Replacing a value that had an overflow chain **orphans the old chain**
— that space, like tombstones, is reclaimed only by `embedmind vacuum`. There is no
B-tree delete operation in v1.

## 6. Vector pages

- Embeddings are stored in VECTOR pages as fixed-stride blocks: `embedding_dims × 4` bytes (f32) per slot; slot count per page derives from `page_size`.
- Vectors are **L2-normalized at insert** (cosine ≡ inner product downstream).
- With `embedding_quant = 1` (future), stride becomes `dims × 1` byte plus per-vector scale/offset (8 bytes); the flag lives in the header so a file never mixes representations. Mixing models is likewise forbidden: `embedding_model_id` in the header is authoritative, and changing models requires `embedmind reembed` (new file).

## 7. HNSW index pages

The graph uses **direct page addressing** (ADR 0008): adjacency lists hold HNSW_NODE
*page numbers*, not logical node ids. There is no id-to-page location table — nothing
in the index grows with node count except the node pages themselves, so the meta page
is fixed-size forever, an insert touches O(M) pages regardless of index size, and one
traversal hop costs one page read.

- **HNSW_META** (exactly one page, fixed size): after the common header
  (`entry_count` reserved/zero, `next_page` = 0): `M` (u16) · `ef_construction` (u16) ·
  `max_layer` (u8) · `entry_point_page` (u64, page of the entry-point node; must be
  non-zero iff `node_count > 0`) · `node_count` (u64).
- **HNSW_NODE** pages hold one node each: after the common header (`entry_count`
  reserved/zero): `record_id` (ULID, 16 bytes) · `vec_page: u64` + `vec_slot: u16`
  (the node's embedding location, duplicated from the record's `vec_ref` so search
  reads one page per candidate instead of a B-tree lookup per hop) · `layer_count`
  (u8) · then per layer a `u16` neighbor count + neighbor `page_no: u64` array.
  Neighbor page numbers are never 0 (page 0 is the header).
- Node adjacency is bounded (`≤ M` per layer, `≤ M×2` at layer 0) and a node's level
  is clamped so a **full** node always fits one page (`max_hnsw_level(page_size, M)`);
  nodes never overflow. A `(page_size, M)` combination whose full layer-0 node cannot
  fit one page is invalid and refused.
- Graph mutations during insert are ordinary page writes: touched HNSW pages enter the WAL like any other page (§8). No separate index journal.
- Because adjacency references pages directly, any operation that relocates node pages
  (`embedmind vacuum`) rebuilds the index — which vacuum does anyway (§5, ADR 0003).
- **Chunking (DESIGN §6):** several nodes may share one `record_id` — a memory longer
  than the embedder's window is indexed as one node per chunk. Chunking exists only in
  the graph: the record stays whole, its `vec_ref` (§5) points at the **first** chunk's
  vector, and search dedupes hits by `record_id`. Readers must not assume `record_id`
  is unique across HNSW_NODE pages.

## 8. WAL sidecar (`.mind-wal`)

Physical page-level redo log (DESIGN decision #1). Structure:

```
WAL header (32 bytes):
  magic "MINDWAL1" (8) · format_version u32 · page_size u32 ·
  salt u64 (random per WAL generation, prevents stale-frame replay) · reserved

Then a sequence of frames:
  frame header (32 bytes):
    page_no u64 · txn_id u64 · commit u8 (1 = last frame of txn) ·
    reserved (7) · frame_checksum u64
  page image (page_size bytes)
```

- `frame_checksum` = `xxh3_64` over frame header bytes `[0, 24)` **plus** the page image, seeded with the WAL `salt`. A frame whose checksum fails, or whose `salt` lineage is broken, ends the valid WAL prefix.
- **Commit protocol:** append all frames of the transaction → `fsync(wal)` → the final frame has `commit = 1` and a valid checksum. A transaction is durable iff its commit frame is fully valid on disk.
- **Recovery (on every open):** scan frames from the start; apply pages of transactions whose commit frame is valid, in order; stop at the first invalid frame and truncate the rest (torn tail). Automatic, silent, logged.
- **Checkpoint** (WAL ≥ 4 MB or clean close): copy committed WAL pages into `memory.mind`, `fsync(main)`, then truncate/delete the WAL. On Windows, every "fsync" is `FlushFileBuffers`.
- **fsync policy:** `full` (fsync per commit) is the default and the only mode in v0.1. A `batched` opt-in mode is under evaluation (DESIGN §12) and would relax durability, never integrity.

## 9. Concurrency at the file level

Single-writer / multi-reader. Cross-process exclusion uses advisory file locks
(`flock` semantics on Unix, `LockFileEx` on Windows) on the main file: a second writer
gets a clear error; concurrent readers are allowed and see the last checkpointed +
committed-WAL state.

## 10. Forward compatibility checklist (for any future change)

1. Can it be expressed with reserved bytes/flags? → do that, no version bump.
2. New page type? → old readers must be able to skip it; minor-compatible.
3. Anything that changes the meaning of existing bytes → `format_version` bump + `embedmind migrate` path + fuzz corpus regenerated. There is no option 4.
