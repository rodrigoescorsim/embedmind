# Launch FAQ

> **STATUS: DRAFT — [MANUAL — founder].** Prepared input for task B1 (launch material,
> PRD §6 risk mitigation). Not reviewed or published. Answers below anticipate the
> questions a Show HN / Reddit / X audience predictably asks, sourced from
> [README.md](../../README.md), [DESIGN.md](../../DESIGN.md), the ADRs, and
> [`benches/results/latest.md`](../../benches/results/latest.md) (0.1.0-dev, 2026-07-13).
> No claim here should say anything the linked source doesn't already say. Publication is
> a founder decision.

---

### Why not just use sqlite-vec / LanceDB / a server-based vector DB?

Short answer: EmbedMind isn't a general vector database, it's memory *for agents*
specifically — a single crash-safe file, embeddings built in, no server, no schema to
design. If your need is closer to "vector search as one more table in my existing app,"
those tools are a better fit, and the README says so directly (see
["When to use sqlite-vec instead"](../../README.md#when-to-use-sqlite-vec-instead)):

- **You already run on SQLite** and want vector search inside your existing schema and
  transaction — sqlite-vec rides your database; EmbedMind owns its own `.mind` file.
- **You bring your own embeddings and want raw ingest throughput.** sqlite-vec doesn't
  embed for you, so its ingest is vectors-only and will likely beat EmbedMind's
  end-to-end (embed-included) ingest if you're already producing vectors in bulk.
- **You need SQL** — joins, arbitrary `WHERE`, aggregates — over the same rows as your
  vectors. EmbedMind gives `recall` with metadata/agent/project filters, not a query
  language.
- **You want a battle-tested dependency today.** sqlite-vec is built on SQLite; EmbedMind
  is pre-1.0.

We didn't embed `tantivy` for full-text either, for the same underlying reason: a
third-party engine brings its own storage and its own commit point, and the whole product
promise is one crash-consistent file (see
[ADR 0011](../adr/0011-full-text-indice-invertido-proprio.md)).

We also implemented HNSW ourselves rather than using `hnsw_rs` or `usearch`, because every
off-the-shelf option assumes the whole graph lives in RAM with monolithic serialization —
that breaks instant cold-open and transactional (WAL-backed) index mutation. Details in
[ADR 0008](../adr/0008-hnsw-enderecamento-direto-de-paginas.md) and the
[engine-internals post](post-2-engine-internals.md).

### How is this actually crash-safe? What's the evidence, not just the claim?

Durability comes from a physical page-level redo WAL (SQLite-style): every transaction
appends full page images to a sidecar `.mind-wal` file, `fsync`s it, and only a fully
valid commit frame (checksummed, salted per WAL generation) makes a transaction durable.
Recovery on every open replays committed frames and truncates any torn tail — no domain
logic re-executes, just checksum-verified `memcpy`.

That's the design; here's how it's checked, not just asserted:

- A **fault-injecting VFS** kills the simulated process before/after every mutating I/O
  operation — sector-granular torn writes, a lying-`fsync` mode — and a crash harness
  sweeps every kill point over real workloads, checking invariants (every committed
  transaction present, no half-transaction, all checksums valid) against a reference
  model. See [docs/TESTING.md](../../docs/TESTING.md).
- **Fuzz targets** on the WAL replay path plus the header/page/record parsers — short
  fuzz pass on every PR, longer runs nightly.
- This runs in CI **on Windows**, where `fsync` means `FlushFileBuffers` — the founder
  develops on Windows specifically because it's the platform durability testing usually
  skips.

`remember` is fsync-per-commit by default (the only mode in v0.1) and still measures
8.52 ms p50 / 21.90 ms p99 end-to-end at 100k memories — durability isn't traded for a
"fast mode" that silently weakens it.

### Does anything leave my machine?

No. Zero network dependencies in the core engine — this is a project rule
(`CLAUDE.md`: "nada sai da máquina é auditável no código"), not a toggle you have to
remember to flip. Embeddings run through a quantized ONNX model bundled inside the
binary; there's no API key because there's nothing to call. You can audit this in the
source: the core crate (`embedmind-core`) has no HTTP client, no telemetry SDK, nothing
that opens a socket.

### How big is the download, given a model ships inside the binary?

The raw release binary is around 45 MiB on Windows (`ort` with statically-linked ONNX
Runtime, ~23 MiB, plus the quantized model, tokenizer, and code). The NFR target — binary
under 40 MB — governs the **compressed release artifact** you actually download, not the
decompressed binary: gzip -9 brings it to ~23.4 MiB, xz -9 to ~20.5 MiB, comfortably under
the target. After decompression it's still a single self-contained file — no separate
`.dll`/`.so` to manage, no install step beyond unzip. Full reasoning in
[ADR 0010](../adr/0010-teto-de-tamanho-governa-artefato-comprimido.md).

### What are the known limits right now? Where does it fall short of its own targets?

Stated plainly, because the project's own rule is that a missed NFR gets reported, not
hidden (`docs/BENCHMARKS.md`):

- **`recall` p99 at 100k memories misses its target**: 224.88 ms measured vs. a 50 ms NFR.
  The vector half alone (HNSW, no full-text) is fast — 29.32 ms p99 at 100k, well inside
  budget. The gap is the full-text fusion path: BM25 postings scanning is the dominant
  cost at this scale. It's an active, tracked optimization
  ([ADR 0017](../adr/0017-otimizacao-do-full-text-escopo-e-metodo.md)) — already cut
  roughly 5x through this phase (early-termination scan, delta+varint postings encoding),
  with a skip-list structure implemented and tested but not yet wired into the hot scan
  path. If you need sub-50ms `recall` at 100k+ memories today, that gap is real; smaller
  corpora (10k memories: 30.15 ms p99) are inside budget now.
- **Where competitors win on raw numbers** (from an earlier benchmark run, see
  [BENCHMARKS.md](../../docs/BENCHMARKS.md) for the current one): sqlite-vec beats
  EmbedMind on recall@10, warm query p99, and on-disk size at 10k, because it stores bare
  vectors and skips embedding; zvec beats EmbedMind on warm latency by roughly 10x for the
  same reason. EmbedMind's file is larger because it also holds memory text, metadata,
  provenance, and both indexes — a different scope, not an apples-to-apples "smaller file
  wins" comparison (see BENCHMARKS.md's honesty-contract rules on scope).
- **Out of scope for v0.1 entirely** (by explicit founder decision, not oversight):
  time-travel/full history beyond `supersedes`, at-rest encryption (reserved in the file
  header format since day 1, ADR 0007, so it can arrive later without a format break),
  RBAC/audit/air-gap, provenance attestation, team sync/connectors.

### What happens to my `.mind` file across upgrades? Will it break?

The format evolves additively. New features arrive as new page types plus a root pointer
carved from reserved header bytes — the full-text index bumped `format_version` from 1 to
2, the graph layer from 2 to 3, and files written under the old version still open; they
just don't get the new feature until you run `embedmind vacuum` to rebuild with the
current format. `docs/FORMAT.md` is the versioned, normative spec for the byte layout.

### Who maintains this, and what's the support model?

Solo, self-funded project — one founder, no team, no outbound sales, no funnel. Support is
best-effort, stated as such in the README. The roadmap is driven by real usage signals
(GitHub issues, external PRs, recurring downloads) rather than a sales pipeline, and a
feature only earns a roadmap slot after being requested by 2+ independent users.

### Is it actually used on a real codebase, or just benchmarked on synthetic data?

Both, and they're reported separately on purpose — synthetic benchmarks
(`agent-mem-10k`/`100k`) measure engine performance at scale; they don't tell you what a
real agent workflow feels like. See the [dogfooding post](post-3-dogfooding.md) for
numbers from actually using the release CLI to store and recall real facts about this
project's own development history.

---

## Short-form variants (same claims, different length/tone per venue)

> All variants must stay consistent with the numbers and claims above — do not restate a
> number here that isn't sourced in `post.md`'s provenance appendix or this file.

### r/rust

Built an embedded memory engine for AI agents in Rust — single-file, crash-safe (WAL +
fault-injecting VFS in CI, fuzzed parsers), `#![forbid(unsafe_code)]` in the core, zero
network deps. HNSW implemented from scratch with direct page addressing (no in-RAM graph,
no node-id lookup table) so a 845 MiB / 100k-memory file cold-opens in 0.27 ms. Ships as an
MCP server + CLI. Benchmarks are honest — including where it currently misses its own
50 ms recall-latency target at 100k memories (224.88 ms; full-text fusion is the
bottleneck, actively being optimized, ADR-tracked in the repo). Repo, format spec, and
benchmark harness are all public — feedback on the WAL/HNSW design especially welcome.

### r/LocalLLaMA

Local-first memory for coding agents: single `.mind` file, no server, no API key, nothing
leaves your machine (embeddings run through a bundled quantized ONNX model, CPU-only).
Wire it into Claude Code or any MCP client with `claude mcp add embedmind -- embedmind
serve`. Semantic + keyword hybrid search (HNSW + BM25, fused with RRF). Honest numbers in
the repo, including where it currently loses to sqlite-vec/zvec on raw vector-search speed
and where its own full-text-fusion latency target isn't met yet at 100k memories — both
documented, not hidden.

### r/ClaudeAI

Gave Claude Code (and any MCP client) persistent memory that survives between sessions:
`remember` a fact, `recall` it later by meaning or keyword, `forget` it if it's wrong —
backed by one local file, not a cloud service. One line to add:
`claude mcp add embedmind -- embedmind serve`. No API key, nothing sent anywhere. Memories
can supersede each other, so an agent recalling project history gets the current fact
instead of a pile of contradictions. Open-source (MIT core), benchmarks and known
limitations posted openly — including a recall-latency target it currently misses at
100k+ memories, being actively worked on.

### X

Built persistent memory for coding agents in Rust: one file, no server, no API key,
nothing leaves your machine. Crash-safe WAL, HNSW + BM25 hybrid recall, MCP server + CLI.
Benchmarks are honest, wins and misses both — including a recall-latency target we don't
hit yet at 100k memories. Repo's public, numbers are reproducible.
