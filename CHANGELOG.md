# Changelog

All notable changes to EmbedMind are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org) (pre-1.0: minor bumps may break APIs, but **never** the
file format without a migration path).

**Honesty policy:** regressions, data-integrity incidents, and benchmark losses are
recorded here, not buried. If a release fixes a corruption bug, the entry links a
postmortem.

## [Unreleased]

Pre-v0.1 — under active development, repo private until M1 completes
(see [ROADMAP.md](ROADMAP.md)).

### Changed
- **Full-text lift measured on lexical queries, @ 10k and @ 100k — the benefit
  side of the FT phase, not just the cost** (product review 2026-07-13,
  [ADR 0017](docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md) §"O
  benefício do full-text"): `benches/src/lexical.rs` harness generates 100
  deterministic ground-truth-by-construction lexical queries (exact code
  identifiers, ULIDs, hex hashes, CLI flags, literal error fragments) and
  measures hybrid (`Store::recall`) vs. vector-only (`Store::recall_vector`)
  recall@10 + latency over the same queries. Official run, published in
  [`benches/results/0.1.0-dev.json`](benches/results/0.1.0-dev.json): recall@10
  lift **+0.09 @ 10k (1.0000 vs. 0.9100) → +0.18 @ 100k (1.0000 vs. 0.8200)** —
  the lift **doubles** as the corpus grows 10x, because vector-only recall
  degrades with more near-duplicate embeddings colliding, while the hybrid
  holds 100% on both. p99 on the lexical query set: 22.14 ms hybrid vs.
  18.63 ms vector-only @ 10k; 139.38 ms vs. 32.45 ms @ 100k.
- **Full-text optimization phase (FT) closes its accounting — `recall p99 @
  100k` NFR still misses target** (closing task, [ADR 0017](docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md)):
  official `benches/run_all.sh --full` run, 2026-07-13, published in
  [`benches/results/0.1.0-dev.json`](benches/results/0.1.0-dev.json). `recall`
  p99 end-to-end dropped from the phase's original baseline of **1,224.62 ms
  to 255.12 ms @ 100k (~4.8x)** through the accumulated effect of early
  termination (ADR 0018) and delta+varint postings (ADR 0021) — the target
  of **< 50 ms is not met**. @ 10k: ~115 ms → **31.84 ms**. recall@10
  (tie-aware, ADR 0019) stays 1.0000 mean/p10/p50/min on both datasets — no
  regression. Peak RSS stays within the 300 MiB ceiling (117–118 MiB @ 100k,
  consistent with ADR 0020). This run does **not** exercise the FTS skip
  index (ADR 0022, `format_version` 5): the benchmark `.mind` files are v4
  (delta+varint only), and even on a v5 file the skip structure only cuts
  work once `fts::search`'s bounds pass is rewritten as BlockMax-WAND — a
  separate, equivalence-risky task the skip-list story deliberately left
  undone.
- **Decision: keep full-text as default, invest in BlockMax-WAND** ([ADR
  0023](docs/adr/0023-blockmax-wand-decisao-fase-bmw.md), founder decision
  2026-07-13, made with the lexical lift data above in hand): the growing lift
  (+0.09 → +0.18) rejects a vector-only default — it would cut full-text
  exactly where it delivers the most value (large corpus, exact literals).
  Opens the BMW phase ([ROADMAP.md](ROADMAP.md) "Fase BMW") to wire the fv5
  skip index into `fts::search`'s hot path via BlockMax-WAND. Honest rollback
  criterion: if BMW doesn't close the < 50 ms p99 @ 100k NFR or breaks result
  equivalence, the vector-only default option returns to the table.

### Fixed
- **Crash in the FTS skip-index lookup on a malformed `format_version` 5 page**
  ([ADR 0022](docs/adr/0022-postings-fts-skip-lists.md)): the nightly fuzz job
  (`fuzz_fts_page`, CI run 29228455653) found a subtract-with-overflow panic in
  `lookup_via_skip` (`crates/embedmind-core/src/index/fts.rs`) — a hostile
  `count`/`block_count` pair made the last-block-length arithmetic
  (`count - b * SKIP_BLOCK_SIZE`) underflow. `lookup_via_skip` now enforces the
  same `block_count == count.div_ceil(SKIP_BLOCK_SIZE)` invariant the decoder
  already checked, so a corrupted skip index degrades to a typed `malformed`
  error instead of a panic (format policy G4). Crash input committed as a
  regression seed (`fuzz/corpus/fuzz_fts_page/crash-9116d630a5fae3ac97551c97104213cd2f5f4e9a`)
  plus a dedicated unit test
  (`index::fts::tests::lookup_via_skip_rejects_corpus_crash_input`).

### Added
- **Skip lists in large FTS postings — `format_version` 5** (story S26 part 2,
  [ADR 0022](docs/adr/0022-postings-fts-skip-lists.md)): a term whose postings
  list reaches 512 entries now carries a **skip index** in front of the body —
  fixed 128-entry blocks, each preceded by its first `record_id`, byte offset,
  and max `term_freq` — so a lookup by id jumps straight to the one block that
  can contain it and decodes only that block instead of the whole list. Each
  block re-bases its delta chain so it decodes independently. Search results
  are byte-identical with or without the skip index (the `search_profiled`
  oracle, three-layout round-trip, and a dedicated `lookup_via_skip`-vs-linear
  equivalence test — run over a corpus above the threshold so the skip layout
  is the on-disk form — pin it). The layout is selected by the file's
  `format_version`, never mixed: files from older builds (v ≤ 4) keep reading
  **and writing** their own (fixed-width or skip-less delta+varint) layout, and
  `vacuum`'s copy-based rebuild is the upgrade path. `fuzz_fts_page` now decodes
  every input under all three postings layouts and drives the block-skipping
  lookup over the same hostile bytes, with new v5 corpus seeds — including one
  with a real skip index — alongside the preserved v3/v4 ones
  ([FORMAT.md](docs/FORMAT.md) §2/§4/§11). The gain on the hot path needs a
  BlockMax-WAND rewrite of the scan's bound pass (equivalence-risky, split into
  its own task); this ships the format + structure. NFR impact @ 100k is
  measured in the FT-phase closing task, not claimed here.
- **FTS postings compressed with delta+varint — `format_version` 4** (story
  S26 part 1, [ADR 0021](docs/adr/0021-postings-fts-delta-varint.md)): a
  term's postings entries are now stored as the varint-encoded delta of each
  `record_id` from the previous one plus a varint `term_freq`, instead of
  fixed 20-byte entries — fewer bytes per posting through the pager and the
  decode loop, zero change in search semantics (same hits, same scores, same
  order; the `search_profiled` oracle and both-layout round-trip tests pin
  it). The layout is selected by the file's `format_version`, never mixed
  within a file: files written by older builds (v ≤ 3) keep reading **and
  writing** their fixed-width layout under this build — degrading only in
  size/speed, never in correctness — and `vacuum`'s copy-based rebuild is
  the upgrade path. The `fuzz_fts_page` body now decodes every input under
  both layouts, with new v4 corpus seeds alongside the preserved v3 ones
  ([FORMAT.md](docs/FORMAT.md) §2/§4/§11). NFR impact @ 100k is measured in
  the FT-phase closing task, not claimed here.
- **Usage report: `embedmind report`** (story S23) — the trust answer to "is
  the memory actually being used?". Aggregates the op-log written by `serve
  --op-log` over a window (`--since N`, default 7 days): sessions, recalls
  (empty/errors/latency p50-p99), remembers, forgets, per-memory recall
  counters (top recalled, with content previews) and live memories never
  recalled in the window (dead weight, curation candidates). `--json` emits
  the same aggregation as one JSON object — the CLI's first machine-readable
  output (immediate consumer: the Agentic Panel's memory card). Without an
  op-log the report degrades to store totals plus instructions, never an
  error. Design decision: counters are DERIVED from the log, not new record
  columns — the `.mind` format is untouched and `recall` stays a pure read.
  To make sessions countable, `serve` now appends a `{tool: "session"}` line
  (same shape as tool lines, client name in `args`) on each `initialize`.
- **Structured op-log on `serve`** (story S22) — `embedmind serve --op-log
  <path>` (also on the standalone `embedmind-mcp` binary) appends one JSON
  line (JSONL) per tool call: `{ts (epoch µs), tool, args (content/query
  truncated to 200 chars), ids, scores, latency_ms, project, isError}`, plus
  `error` (the message) on failures — both engine errors and protocol errors
  on a dispatched call. Pure observability for operators (immediate consumer:
  the Agentic Panel's memory card, tailing via SSE): append-only, flushed per
  line, every line independently parseable so a reader can tail from any
  point. Without the flag: zero cost, no file created. A write failure NEVER
  fails the tool call — warning on stderr, normal response to the client;
  stdout remains the exclusive MCP protocol channel.
- **`ef_search` default scaled by index size** (story S16, ADR 0015) —
  `HNSW_DEFAULT_EF_SEARCH = 64` no longer applies unconditionally: the
  default now grows in measured steps with the live `node_count` (64 below
  25k, 96 / 160 / 256 at 25k / 50k / 100k). `Query::ef_search(n)` explicit
  still wins at every scale (`Query.ef_search` is now `Option<u16>`
  internally). The harness reports the per-query recall distribution
  (min/p10/p50), not just the mean, so a good average hiding a bad tail can't
  hide again.
  **Honest result (2026-07-11 validation, 1000 queries, `benches/run_all.sh
  --full`):** the mechanism works and 10k is unaffected, but the story's DoD
  is **not met** at 100k: recall@10 mean 0.9360 (target ≥0.95), worst query
  0.20 (target ≥0.70) — the same worst-case number that motivated the story
  in the first place, unmoved by the larger beam — and hybrid query p99
  1224.62 ms (target < 50 ms), dominated by a pre-existing full-text search
  bottleneck (the postings list is decoded whole per term per query, cost
  linear in corpus size) rather than by HNSW itself. Peak RSS @ 100k also
  crossed its ceiling: 307.1 MiB measured vs. the 300 MiB target (the 6%
  headroom noted when the story was scoped is gone at this scale). None of
  this is hidden: it's recorded in ADR 0015 and `docs/03-tasks.md` (BQ1) as
  reproved-DoD debt with concrete follow-ups (a larger/different `ef`
  ladder or index-build tuning for the recall tail; a paginated postings
  index for the FTS latency; RSS investigation at 100k) — none in scope for
  this task.
- **`supersedes` — first-class versioned knowledge** (story S19, ADR 0013) —
  `remember` accepts an optional `supersedes: <id>` pointing at a prior
  memory; the old memory is tombstoned atomically with the new write (same
  WAL transaction — a crash mid-write leaves either both live or neither
  applied, never a half-superseded state) and `recall` never returns a
  superseded memory even when its content would otherwise rank first. `recall`
  results carry `superseded_by` when applicable so callers can see the chain.
  Core, MCP, and CLI all expose the field; a corrective write is now a normal
  operation instead of requiring a separate `forget`.
- **Recency as a third list in the recall RRF fusion** (story S20, ADR 0014) —
  `recall::fuse_lists` generalizes the RRF k=60 fusion from 2 to N ranked
  lists; a third list reorders the *same* content candidates (the vector+text
  union, already filtered by scope/tombstone/`supersedes`/metadata/agent) by
  `created_at_micros` descending, so a genuine content tie breaks toward the
  newer memory without ever pulling in an irrelevant item or displacing a
  strong old match (RRF's own bound: a list's max contribution is
  `1/(k+1)`). Golden cases cover fact+correction (the correction wins a real
  tie) and old-strong-match-vs-new-weak-match (the old one still wins), plus
  property tests over 3-list fusion determinism. **Opt-in, default off**
  (`Query::recency(bool)`, MCP `recency`, CLI `--recency`) — measured on
  `benches/run_all.sh --full` with `EMBEDMIND_BENCH_RECENCY`: recall@10 is
  identical with and without it (10k 0.9953, 100k 0.9360, confirming the
  extra list only reorders, never changes, the result set), but hybrid query
  p99 on `agent-mem-100k` goes from 1224.62 ms to 2063.94 ms (+68.5%, over 4×
  the §5 CI regression guard's 15% threshold) from re-reading `created_at` on
  up to `2·limit` candidates per query; on `agent-mem-10k` the cost is
  negligible (103.09 → 103.13 ms, +0.04%). The regression is recorded in the
  ADR rather than hidden behind a default flip.
- **Write-time near-duplicate hints on `remember`** (story S21) — the response
  now carries `similar: [{id, content (truncated to 160 chars), score,
  created_at_micros}]`: existing live, non-superseded memories in the same
  applied scope whose cosine similarity to the new content clears a *measured*
  threshold (`NEAR_DUP_THRESHOLD = 0.80`, calibrated on the harness corpus
  with the shipped model — ADR 0016). The store **always** happens; the list
  informs the caller's forget / supersedes / keep decision, it never blocks.
  Zero extra embedding cost: the scan reuses the chunk vectors the write
  itself indexes (proved by a counting-embedder test). MCP returns the new
  field (additive — pre-S21 clients unaffected); the CLI prints a legible
  `memória parecida existente: <id> — <snippet>` hint. Honest limitation,
  recorded in the ADR: cross-language paraphrases mostly escape the threshold
  (the embedded MiniLM isn't multilingual); the operationally dominant case —
  an agent re-storing the same fact with framing — is caught at 98.5% with
  0.10% false flags. The ingest benchmark now measures `remember_detailed`,
  so the published `remember` p99 includes the scan.
- **Graceful recall on `.mind` files with no full-text index** (S9 edge,
  roadmap 2.3) — a file written before the full-text index existed (header's
  `fts_root_page == 0`) never fails `recall`: it degrades to vector-only
  automatically (RRF fusion with an empty text list) and the degradation is
  now *visible*, not silent — the CLI prints a stderr warning pointing at
  `embedmind vacuum` (which rebuilds the file with the index) and the MCP
  `recall` response gains a top-level `warning` field, absent on healthy
  files so existing clients see an unchanged shape. Covered end-to-end: a
  core integration test opens a legacy-shaped fixture and asserts valid
  vector hits + the outcome flag, CLI and MCP tests assert the warning on
  their channels, and the S9 fusion invariants (union never intersection,
  deterministic best-first order, limit cap) are now property-tested with
  `proptest` — closing story S9.
- **Real head-to-head benchmark numbers vs. sqlite-vec + zvec** — the comparison
  columns the harness reserved are now filled with measured values (no more
  "not measured on this run"). Ran `benches/run_all.sh` with
  `--features compare-sqlite-vec,compare-zvec` on the founder's Windows box (MSVC
  toolchain), same vectors/queries/`k`, on `agent-mem-10k`: sqlite-vec
  **0.1.10-alpha.4** (recall@10 0.9984, query p99 13.2 ms, 15.3 MiB) and zvec
  **0.5.1** (recall@10 0.9912, query p99 1.5 ms, 17.4 MiB). Both beat EmbedMind
  on warm-query p99 and on-disk size (they store bare vectors; EmbedMind keeps
  the memory text + metadata + provenance), and sqlite-vec edges out recall —
  recorded in the losses list per the honesty contract (BENCHMARKS.md §4).
  Competitor metrics now also land in `benches/results/<version>.json` so every
  table cell traces to a field. A `COMPARE_DATASET` env var pins the (expensive)
  comparison to the 10k set while the full EmbedMind 10k+100k table and its NFR
  verdicts (all passing: recall p99 15.5 ms, peak RAM 281 MiB @ 100k) still run.
  The zvec adapter's directory pre-creation bug (zvec rejects a pre-existing
  collection path) was fixed.
- **Chroma comparison row (S18 / task BQ4)** — sqlite-vec/zvec are index-layer
  baselines; Chroma (local/embedded mode) is the product-category competitor a
  real agent developer weighs, since it also embeds. New `--features
  compare-chroma` adapter (`benches/src/competitors.rs::run_chroma`) drives a
  pinned `chromadb==1.5.9` through a Python subprocess
  (`benches/chroma_bench.py`, JSON over stdin/stdout, no server/network) fed
  the *same* pre-computed all-MiniLM-L6-v2 vectors every other system in the
  comparison receives — Chroma's own embedding function is never called.
  Measured on `agent-mem-10k`: recall@10 0.9936, query p50/p99 0.70/1.29 ms,
  19.7 MiB on disk — beats EmbedMind on query p99 and on-disk size (recorded
  in the losses list, BENCHMARKS.md §4). Without the toolchain (Python 3 +
  `pip install chromadb==1.5.9`) the row honestly reports "not measured".
- **Python bindings** (S12 / task B5, roadmap 2.5) — the multiplier that
  unlocks LangChain and custom agents. New `bindings/python` crate (PyO3 +
  maturin, its own workspace like `fuzz`) exposes `Store` with
  `remember`/`recall`/`forget`/`stats`/`vacuum` at the **same semantics** as the
  MCP tools and CLI: a thin shell over `embedmind_core::api`, no domain logic
  (CLAUDE.md decision 2). Typed metadata maps to native Python scalars
  (`str`/`int`/`float`/`bool`/`None`); recall filters accept an exact-match
  scalar or a `(min, max)` numeric range (S10), the agent filter and per-agent
  stats breakdown (S14) come through unchanged. Because the bindings call the
  *same* engine, `.mind` files are **byte-for-byte interchangeable** with the
  Rust `Store` — a pytest round-trip suite writes in Rust and reads in Python
  (and vice-versa, incl. forget across the boundary) to prove it. Ships as an
  `abi3` wheel (one per platform, CPython 3.9+) with the embedded ONNX model, so
  vector recall works on `pip install` with no download or API key; type stubs +
  `py.typed` included. Release CI builds/tests the three-platform wheels (PyPI
  upload stays MANUAL, like crates.io); CI lints + pytests the bindings on every
  PR.
- **Basic provenance exposed: agent filter on recall + per-agent stats
  breakdown** (S14 / task C2, roadmap 3.2) — the agent/session data every
  memory already carried (core decision 3) is now queryable, with no file-format
  change. `recall` gains an optional agent filter (`Query::agent`), applied in
  the same `keep` predicate as scope/tombstone/metadata filters, so it composes
  with them and keeps the S2 adaptive-`ef_search` anti-under-return guarantee
  across the vector, text and hybrid paths. `stats` gains `StoreStats::by_agent`
  — a breakdown of **live** memories per writing agent (empty agent = unknown
  provenance), each with its distinct session ids. Shells: the MCP `recall` tool
  takes an optional `agent` argument (additive, backward compatible) and a new
  **read-only `stats` tool** reports the live/forgotten counts and the per-agent
  breakdown; the CLI adds `recall --agent <name>` and a "by agent" section in
  `embedmind stats`. Attestation and full history/time-travel stay explicitly
  out of scope (founder decision, post-traction). Spec: `docs/01-spec.md` S14.
- **Graph layer: explicit entities + typed relations between memories** (S13 /
  task C1, roadmap 3.1, **ADR 0012**) — the vector + text + **graph** depth no
  embedded memory engine has complete. `remember` accepts entity tags
  (`MemoryDraft::entity`, 1–128 bytes) and typed relations to existing live
  memories (`MemoryDraft::relation`; a missing or forgotten target is a typed
  error — dangling edges are never born), written in the *same transaction* as
  the record: graph pages (new `GRAPH_DICT`/`GRAPH_OVERFLOW` types, spec in
  FORMAT.md §12, `format_version` 2 → 3, additive) ride the WAL like every
  other page. Navigation: `Store::related(id)` (both directions, kind carried),
  `Store::entity_members(entity)`, `Store::entities_of(id)`; optional 1-hop
  expansion on recall (`Query::expand_related`) appends connected context after
  the ranked hits (score 0.0, outside the limit). Relations to a forgotten
  memory disappear with the tombstone (re-checked at query time) and are
  physically dropped by `vacuum`, which rebuilds the graph keeping only live
  entities and edges with both ends live. The dictionary reuses the same
  slotted B-tree as the full-text index (shared `index::dict` module — one
  fuzzed implementation, not two). Extraction is explicitly *not* in scope:
  entities/relations are caller-provided. New fuzz target `fuzz_graph_page`
  (+ seed corpus); the record crash harness now writes and verifies graph
  pages at every injected kill point. Older (v2) files degrade to "no related
  memories", never an error. The whole layer is exposed through the shells
  (the product surface, CLAUDE.md decision 1): the MCP `remember` tool takes
  `entities` (string array) and `relations` (`{kind, target}` array), `recall`
  takes `expand_related: true`, and a new **`related` tool** navigates by
  `id` **or** `entity` (exactly one); the CLI mirrors it with `remember
  --entity NAME --relation KIND=ID`, `recall --expand-related` (connected
  context printed with a `rel` marker instead of a score) and a `related
  <ID> | --entity NAME` subcommand. Protocol and end-to-end CLI tests cover
  the flow, including the tombstone edge through both shells.
- `embedmind vacuum` reclaims forgotten space for real (S11 / task B4, roadmap
  2.x, **ADR 0003**), replacing the earlier explicit "not implemented" error.
  Rebuild **by copy, never in place**: a fresh `.mind` is built in a sibling temp
  file with every *live* memory re-inserted (record preserved byte-for-byte —
  id, provenance, metadata — while the HNSW and full-text indexes are rebuilt
  from scratch so they hold only the living), then swapped over the original with
  a single **atomic rename**. Crash-safe at every point: until the rename the
  original is untouched, so a crash leaves either the intact original or the
  finished compacted file — never a torn mix; orphan temp/scratch files are swept
  on the next `open`/`vacuum`. Result is always ≤ the original in size. `Store`
  gains a `Vfs::rename` seam and the swap parks its live pager on a throwaway
  scratch store so the file field is never invalid mid-swap. `embedmind vacuum`
  now prints the before/after size and the count reclaimed. New crash harness
  `tests/crash_vacuum.rs` sweeps a kill point at every mutating I/O of the vacuum
  and asserts recovery lands in exactly one of the two legal states (and that
  both are exercised). **Note (pre-existing, unrelated bug found while testing):**
  a crash *during a checkpoint* (independent of vacuum, reproducible via a plain
  `close()` mid-checkpoint) can drop a committed `forget`; tracked separately —
  see the session notes. `docs/adr/0003`.
- Metadata filters on `recall` (S10 / task B3, roadmap 2.4): `recall` accepts a
  `key → filter` map, ANDed — exact typed match (`Filter::Eq`) or numeric range
  (`Filter::Range { min, max }`). Filters ride the same `keep` predicate as the
  tombstone/scope re-check, so the S2 adaptive-`ef_search` anti-under-return
  guarantee covers filtered results (a filtered-out candidate widens the search,
  never silently under-returns). Edges: a filter on an absent key is a plain
  non-match (0 hits, never an error); a type-incompatible filter (`Eq` across
  types, or `Range` over a non-numeric value) is a typed `InvalidArgument`. The
  MCP `recall` tool schema gains an **optional** `filters` object (bare scalar =
  equality, `{min?, max?}` = range) — additive and backward compatible for
  clients that never send it; the CLI adds a repeatable `recall --filter`
  (`key=value`, `key=lo..hi`, `key>=n`, `key<=n`). Shells stay logic-free
  (parse → API → serialize). Spec: `docs/01-spec.md` S10.
- Hybrid recall: fuse the vector and full-text lists via Reciprocal Rank Fusion
  (S9 recall half, roadmap 2.3, ADR 0005, k=60) in `recall::fuse`, wired into
  `Store::recall`/`recall_detailed`. Fusion is a union, never an intersection: a
  rare exact term (text-only hit) or a semantic synonym (vector-only hit) both
  still make the result. A pre-M2 file with no full-text index (`format_version`
  1) degrades to vector-only with a reported flag, never an error; project
  scope, tombstone re-check, and the S2 adaptive-`ef_search` anti-under-return
  guarantee are preserved end-to-end. Fixed `fuse` double-counting a repeated id
  within a single list (tracks per-id list contribution so an intra-list repeat
  keeps only its best rank, while a genuine cross-list overlap still sums). New
  `Store::recall_vector` isolates the pure HNSW path so the benchmark harness
  keeps grading the index's approximation quality on its own
  (`docs/BENCHMARKS.md` §3) now that `recall` itself is hybrid.
- Full-text index in the engine (S9 engine half, roadmap 2.3, **ADR 0011**):
  own paged inverted index with BM25 scoring — **not** an embedded tantivy,
  which would break the single-file promise and the WAL's single commit truth.
  Two new page types (`FTS_DICT` 0x08, `FTS_POSTINGS` 0x09) and a `fts_root_page`
  header field carried in previously-reserved bytes, so `format_version` moves
  1 → 2 as an **additive** bump: a v1 `.mind` stays readable and simply has no
  full-text index (`recall` degrades to vector-only). `remember` indexes content
  in the same transaction as the record and vector writes (crash-safe by the
  same WAL); `Store::search_text` exposes BM25 keyword search (tombstone/scope
  filtered like vector recall) — the list that will fuse with the vector list
  via RRF (ADR 0005) in the recall half of S9. New fuzz target `fuzz_fts_page`;
  the record crash harness now exercises the FTS pages through recovery.
  Spec: `docs/FORMAT.md` §11.
- Launch-ready README + 30s demo GIF script (M1 item 1.7 / A4):
  - README marked **v0.1** (pre-v0.1 warning dropped); a dedicated **Install**
    section (prebuilt binary from Releases, `cargo install embedmind`, source
    build) split from the **Quickstart**; the real `agent-mem-10k` benchmark
    table rendered from `benches/results` (recall@10 0.9953, query p99 17.1 ms,
    `remember` p99 16.7 ms, 82 MiB file, ~112 MiB RSS) with the honesty caveats
    (embed-included ingest, competitors not measured this run, 100k NFRs
    pending); a "When to use sqlite-vec instead" section; and full-text/graph
    claims scoped to the roadmap so nothing unshipped is promised as v0.1.
  - `docs/launch/gif-script.md`: exact command sequence + timing for the 30s
    demo (remember → semantic recall → stats), all commands drawn from the
    shipped quickstart. Recording itself stays `[MANUAL — founder]`.
- Full benchmark harness (M1 item 1.7 / A3 part 2, `docs/BENCHMARKS.md`):
  - `embedmind-bench` now measures the complete metric set — recall@10 vs.
    brute-force, warm query p50/p99, cold-open (`Store::open` + first query),
    `remember` p50/p99 end-to-end (incl. embedding), ingest throughput, on-disk
    file size, and peak RSS — over the committed `agent-mem-10k`/`-100k` datasets.
  - Competitors (sqlite-vec, zvec) are compared in **pinned, recorded versions**
    (`benches/src/competitors.rs`) behind `--features compare-{sqlite-vec,zvec}`.
    When a native toolchain is absent the row reports "not measured on this run
    (target vX.Y)" — the honesty contract forbids fabricated numbers
    (BENCHMARKS.md §4 rule 1).
  - `run_all` binary + `benches/run_all.sh` render a README-ready markdown table
    (with an auto-computed "where EmbedMind loses" section) plus a
    `results/<version>.json`, and exit non-zero on any missed applicable NFR, so
    the same entry point is the CI performance guard (BENCHMARKS.md §5).
  - **Measured v0.1-dev numbers** (founder Windows dev box, CPU-only,
    single-thread): `agent-mem-10k` → recall@10 0.9953, query p99 17.1 ms,
    `remember` p99 16.7 ms (NFR < 200 ms ✅), file 82 MiB, peak RSS ~112 MiB.
    The @100k NFRs (recall p99 < 50 ms, RAM < 300 MB) are validated by the
    `agent-mem-100k` run — see docs/BENCHMARKS.md for the recorded result.
- crates.io publication metadata (M1 item 1.6, story S8): `description`,
  `repository`, `homepage`, `keywords`, `categories`, `readme` and
  `license = "MIT"` on `embedmind-core`, `embedmind-mcp` and `embedmind`
  (the CLI crate), a per-crate `README.md` for each, and inter-crate deps
  pinned with both `path` and `version` in `[workspace.dependencies]`.
  Mandatory publish order (core → mcp → cli), the `[MANUAL — founder]`
  steps, and the crates.io 10 MiB size-limit caveat for the embedded ONNX
  model are documented in [docs/RELEASING.md](docs/RELEASING.md).
- Release pipeline for pre-built binaries (M1 item 1.6, story S8;
  `.github/workflows/release.yml`):
  - Triggered by a `v*` tag; runs the full `cargo test --workspace` suite on
    Linux/Windows/macOS as a gate, then builds the release binary
    (LTO + `codegen-units=1` + strip, from the root `Cargo.toml`) on each
    platform, smoke-tests `embedmind --version`, and attaches one compressed
    artifact per OS to the tag's GitHub Release.
  - `workflow_dispatch` against a tag is a dry run — it produces the same
    workflow artifacts but never mutates a Release (publication is a founder
    action, `docs/04-agents.md` guardrail 7).
  - The job fails if any artifact exceeds the 40 MB ceiling.
- CLI with a working command surface (M1 item 1.6):
  - `embedmind remember / recall / forget / stats` over the default
    `~/.embedmind/memory.mind` (or `--file`); `remember`/`recall` respect
    the detected project context (`--project` / `--global` / `--all`
    override it); `stats` reports counts, file layout, index entries and
    the recorded embedding model (new `Store::stats` / `StoreStats` API).
  - `embedmind serve` runs the same MCP server as the `embedmind-mcp`
    binary — one installed command covers standalone use and the agent
    integration (`claude mcp add embedmind -- embedmind serve`).
  - `embedmind vacuum` fails with an explicit "not implemented, planned
    for v0.2" instead of pretending.
  - End-to-end tests drive the real binary, including a full MCP session
    through `serve` via stdio pipes.
- MCP memory server (M1 items 1.4 + 1.5, `docs/adr/0009`):
  - Direct stdio JSON-RPC implementation — no SDK, no tokio; covers
    `initialize`, `ping`, `tools/list`, `tools/call`. Protocol errors are
    typed JSON-RPC codes; engine failures during a tool call are tool
    results with `isError: true`, never a server crash.
  - Tools `remember` / `recall` / `forget` with stable schemas; zero
    domain logic in the shell. `clientInfo.name` from the handshake is
    recorded as the provenance agent.
  - Automatic project-context scoping: the nearest marker walking up from
    the cwd wins — `.embedmind.toml` with a top-level `project` key
    (explicit override), else a `.git` entry (repo root's directory name).
    `remember` stamps the detected project (`project: null` forces
    global); `recall` scopes to it by default, `scope: "all"` is the
    explicit fallback, and the applied scope is echoed back.
- Vector recall (M1 item 1.3, `docs/adr/0002` + `0004` + `0008`):
  - Paged HNSW with **direct page addressing**: adjacencies store node
    page numbers — no id-to-page table, fixed-size meta page forever,
    O(M) pages touched per insert, no node-count cap. Diversity-aware
    neighbor selection (the paper's Algorithm 4 + keepPrunedConnections)
    and adaptive `ef_search` (grows ×4 while filters leave the result
    under-filled, up to the whole graph).
  - Embedded ONNX embeddings: all-MiniLM-L6-v2 int8 (~23 MB) + tokenizer
    compiled into the binary via `ort` (CPU-only) — no API key, no
    download step, nothing leaves the machine. Model id + dims recorded
    in the header; opening with a mismatched model is refused.
  - Long-content chunking at the index level: text past one 510-token
    window is embedded in overlapping windows (64-token overlap, cap 128
    chunks); each chunk is one more HNSW entry pointing at the same
    record, search dedupes by record id, recall returns the whole memory.
  - `Store::recall(Query)` with `Scope::All`/`Scope::Project` and
    per-query `ef_search`; hits are `Recalled` (memory + cosine score).
    Tombstoned and out-of-scope memories are re-checked against the
    record at search time, never trusted from the graph.
- New dependencies: `ort` + `tokenizers` (embeddings, isolated behind
  `trait Embedder`), `serde_json` (MCP/CLI shells only — the binary
  format still does not use serde). All within the DESIGN §10 budget.
- KV store + public Rust API (M1 item 1.2):
  - `record`: on-disk `MemoryRecord` encoding exactly per
    [docs/FORMAT.md](docs/FORMAT.md) §5 — ULID ids, tombstone flag, project
    scope, basic provenance (agent/session/timestamp), typed metadata
    scalars. Every length prefix is validated before allocation; decoding
    arbitrary bytes never panics.
  - `storage::btree`: record B-tree per FORMAT.md §5.1 — slotted leaves,
    fixed-entry inner nodes, provably-safe byte-midpoint splits, overflow
    chains for records above ~usable/4 bytes (hard cap 32 MiB), in-order
    scan. No delete: `forget` is a tombstone update; orphaned overflow
    chains wait for `embedmind vacuum` (documented leak).
  - `api::Store`: `create`/`open`/`open_or_create`, `remember` (one durable
    transaction per call), `get`, `forget` (tombstone; no-op forgets write
    zero bytes), timeline iteration (`iter`/`iter_all`), clean `close`.
    Custom `Vfs` injection stays available for tests and embedders.
- Fuzzing infrastructure (rest of M1 item 1.8, per
  [docs/TESTING.md](docs/TESTING.md) §3):
  - The five planned targets — `fuzz_header`, `fuzz_page`, `fuzz_record`,
    `fuzz_wal_replay`, `fuzz_open_full` — as thin wrappers over
    `embedmind-core::fuzz` bodies, which also run as stable smoke tests in
    `cargo test` on every platform (libFuzzer itself is Linux-only in CI).
  - Seed corpus generated from real encoder output
    (`cargo run --example gen_fuzz_corpus`), committed under `fuzz/corpus/`;
    `fuzz/regressions/` reserved for minimized crash inputs.
  - CI: short pass (2 min/target) on every PR, nightly scheduled job
    (1h/target), corpus accumulated across runs via cache.
- Record-level crash harness (`tests/crash_records.rs`): the §2 injection
  sweep re-run against the public API (remember/forget/reopen workloads over
  splits, overflow chains and tombstones), with invariant I5 checked against
  a content-keyed reference model. Verified to catch a deliberately injected
  missing-fsync bug (all three sweeps fail with reproducing tuples).
- New dependency: `ulid` (id generation — already in the DESIGN §10 budget).
- Storage layer foundation (M1 items 1.1 + 1.8, built together as
  [docs/TESTING.md](docs/TESTING.md) mandates):
  - `format`: `.mind` header (page 0) and WAL framing exactly per
    [docs/FORMAT.md](docs/FORMAT.md) — little-endian, explicitly serialized,
    xxh3 checksums on every page, version policy (G4) and encrypted-flag
    refusal implemented as typed errors.
  - `storage::vfs`: `trait Vfs`/`VfsFile`, the I/O seam; `RealVfs` with
    positional I/O and advisory locking (`LockFileEx` on Windows).
  - `storage::wal` + `storage::pager`: physical page WAL (commit = append
    frames + fsync + valid commit frame), automatic recovery on every open
    (torn tails discarded, committed prefix applied), checkpointing at 4 MB
    or clean close, single-writer lock, transactions with rollback-by-drop.
  - `storage::sim`: in-memory fault-injecting VFS — kill points before/after
    every mutating I/O op, sector-granular torn writes, lying-fsync mode.
  - Crash-test harness (`tests/crash.rs`): full injection sweep over four
    workloads, invariants I1–I5 checked against an in-memory reference model
    after every simulated power loss; failures print the reproducing
    `(workload, P, mode, seed)` tuple. Runs in `cargo test` on all CI
    platforms. Verified to catch a deliberately injected missing-fsync bug.
- Project documentation: README, ROADMAP, DESIGN, file-format spec
  ([docs/FORMAT.md](docs/FORMAT.md)), testing strategy
  ([docs/TESTING.md](docs/TESTING.md)), benchmark methodology
  ([docs/BENCHMARKS.md](docs/BENCHMARKS.md)), contribution guide, security policy,
  ADRs ([docs/adr/](docs/adr/)).

### Changed
- **Peak RSS @ 100k was a benchmark-harness bug, not an engine problem**
  (story S28, `docs/adr/0020`): ADR 0015 had measured 307.1 MiB (query) /
  305.4 MiB (ingest) against the 300 MiB ceiling and guessed the cause was
  "general index sizing at 100k" — never actually profiled. A new
  phase-by-phase RSS-attribution binary (`profile_rss`, same method as the
  S24 full-text profiling) measured it instead: `Store::open` moves RSS by
  ~0 (the paginated HNSW and the pager hold nothing significant resident,
  confirming ADR 0008), and the dominant structure is the benchmark
  harness's own brute-force baseline `VectorSet` (~153 MiB resident @ 100k),
  kept alive by reference through the RSS-measured phases well past its one
  real use (the recall-accuracy phase). Fix: `harness::run_suite` now takes
  the `VectorSet` by value and drops it right after the recall phase; the
  competitor comparison (which runs later) reloads the `.vec` sidecar. No
  engine code changed. Peak RSS @ 100k measured after the fix: 97.8 MiB
  (query) / 94.9 MiB (ingest) in the isolated diagnostic. The official
  `benches/run_all.sh --full` run (1000 queries, both datasets) confirms it:
  peak RAM @ 100k **120.6 MiB (query) / 120.1 MiB (ingest) — within the 300
  MiB ceiling**. `recall p99 @ 100k` still misses its target in that same run
  (956.80 ms vs. < 50 ms) — the pre-existing full-text bottleneck (ADR
  0017/0018), out of scope for this story and not a regression from this fix.
  ADR 0015's now-incorrect "general index sizing" note is corrected in place
  with a pointer to ADR 0020, per the immutable-ADR convention.
- **Benchmark recall@10 grading is now tie-aware** (story S27,
  `docs/adr/0019`): a returned hit counts when its exact cosine score ties
  (`SCORE_TIE_EPS = 1e-5`) or beats the k-th exact score, instead of only
  when its *id* survived the exact baseline's arbitrary tie-break. The
  agent-memory corpus holds exact duplicate texts by design (23.0% @ 100k),
  which embed to bit-identical vectors, so the exact top-10 boundary is
  routinely a plateau of tied scores wider than k — the diagnostic probe
  (`probe_worst`, 1000 queries) showed every one of the 70 sub-0.70 queries
  @ 100k had score-parity 1.00: the catastrophic tail reported by ADR 0015
  was 100% a metric artifact, not an HNSW miss. Same rule grades every
  competitor. New numbers @ 100k: mean 0.9360 → 1.0000, worst query
  0.20 → 1.00 (@ 10k: 0.9953 → 1.0000, 0.90 → 1.00). Engine, HNSW
  parameters and file format untouched; honest limitation recorded in the
  ADR (with tie-aware grading the committed synthetic datasets no longer
  discriminate index quality at the k boundary).
- **Full-text search is ~10x faster at 100k memories** (story S25,
  `docs/adr/0018`): the BM25 scan now scores a cheap per-candidate upper
  bound first and only reloads/evaluates candidate records in descending
  bound order, stopping as soon as the remaining bounds provably cannot
  enter the top-k. Results are identical to the exhaustive scan — same
  hits, bit-exact scores, same order — verified by equivalence tests and a
  25/25 bit-exact check on the real 100k corpus. Measured (full-text half
  of hybrid recall, no embed, 1000 warm queries @ 100k): p50 994 → 94 ms,
  p99 4,577 → 551 ms. Still above the 50 ms p99 NFR — postings-level work
  (FT3) remains on the roadmap.
- Size NFR (honesty note, `docs/adr/0010`): the "< 40 MB incl. model" ceiling
  now governs the **compressed release artifact** users download (~20 MiB
  today), not the raw binary. The raw release binary is ~45 MiB because `ort`
  links ONNX Runtime statically (~23 MiB) on top of the embedded ~23 MiB model
  + tokenizer + code — ADR 0004 had assumed the model would dominate. No change
  to the "single self-contained file, no external dependency" promise.
