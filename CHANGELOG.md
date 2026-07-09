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

### Added
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
- Size NFR (honesty note, `docs/adr/0010`): the "< 40 MB incl. model" ceiling
  now governs the **compressed release artifact** users download (~20 MiB
  today), not the raw binary. The raw release binary is ~45 MiB because `ort`
  links ONNX Runtime statically (~23 MiB) on top of the embedded ~23 MiB model
  + tokenizer + code — ADR 0004 had assumed the model would dominate. No change
  to the "single self-contained file, no external dependency" promise.
