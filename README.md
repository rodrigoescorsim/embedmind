# EmbedMind

> **Your agent forgets everything between sessions. This fixes that.**
> One file, local, fast, no server. Rust.

**Status: v0.1** — hybrid (vector + full-text) memory with metadata filters, a graph
layer (entities/relations) and basic provenance, crash-safe single file, MCP server +
CLI. See [ROADMAP.md](ROADMAP.md) for what shipped and what's next.

EmbedMind is **persistent memory for AI agents** — an embedded storage engine (vector +
full-text search, metadata filters, and a graph layer, all shipped) packaged as an **MCP
memory server + CLI**. Think *SQLite for agent memory*: a single crash-safe file on your
machine, no server process, no cloud, no Python environment.

**Versioned knowledge, not just storage.** `remember ... --supersedes <id>` retires an
old memory as a correction — `recall` stops surfacing it, but it stays navigable as
history via `related`. Recall's ranking also weighs recency (a stale-but-similar memory
no longer beats a newer correction), and `remember` flags near-duplicates at write time
before they pile up. No embedded competitor has this — most give you a vector table with
no concept of "this fact was corrected."

```
┌─────────────────────────────────────────────────┐
│  Your agent (Claude Code, Cursor, custom, ...)  │
│   │ MCP: remember / recall / related / forget   │
│  ┌─────▼─────────────────────────────────────┐  │
│  │  EmbedMind engine (Rust, in-process)      │  │
│  │  vector (HNSW) + full-text (BM25, RRF     │  │
│  │  fusion) + graph (entities/relations)     │  │
│  │  → one file: project.mind                 │  │
│  └───────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

## Why

Every agent product needs local memory, but today's options are server-based vector DBs (heavy, another process to babysit) or vector-only embedded stores. There is no equivalent of SQLite for agent memory: embeddable, single-file, vector + graph + text together, encrypted, from desktop to mobile. Meanwhile the #1 pain of coding agents — **amnesia between sessions** — keeps being re-solved with fragile markdown files.

EmbedMind's answer:

- **Single file** — your agent's entire memory is one portable, crash-safe file (WAL-backed).
- **In-process** — no server, no Docker, no ports. The engine lives inside the MCP server binary.
- **Hybrid retrieval, shipped** — vector similarity (paged HNSW, automatic chunking of long memories) fused with full-text BM25 via Reciprocal Rank Fusion, plus metadata filters on `recall`.
- **Graph layer, shipped** — explicit entities and typed relations between memories, `related` navigation, 1-hop expansion in `recall`, and `supersedes` for first-class versioned knowledge (a correction retires the old memory from recall while keeping it navigable as history).
- **Basic provenance, shipped** — every memory records which agent/session wrote it; `recall` can filter by agent and `stats` breaks counts down by agent.
- **Local by default** — nothing ever leaves your machine. Built for the local-first wave, usable in air-gapped environments.
- **Rust** — predictable memory footprint, one static binary per platform.

## Install

EmbedMind is a single self-contained binary — the embedding model, tokenizer and ONNX
Runtime are all linked in. No Python, no GPU, no external database, no download step at
runtime, nothing leaves your machine.

**Prebuilt binary (recommended).** Grab the archive for your platform from the
[latest release](https://github.com/rodrigoescorsim/embedmind/releases/latest), unpack it,
and put `embedmind` (or `embedmind.exe`) on your `PATH`:

| Platform | Asset |
|---|---|
| Linux x86_64 | `embedmind-linux-x86_64.tar.gz` |
| macOS (Apple Silicon) | `embedmind-macos-aarch64.tar.gz` |
| Windows x86_64 | `embedmind-windows-x86_64.zip` |

```bash
# Linux / macOS
tar -xzf embedmind-linux-x86_64.tar.gz
./embedmind --version
```

**From crates.io:**

```bash
cargo install embedmind
```

**From source:**

```bash
cargo install --path crates/embedmind-cli
```

## Quickstart

Add EmbedMind to your agent as an MCP server (example: Claude Code):

```bash
claude mcp add embedmind -- embedmind serve --file ~/.embedmind/memory.mind
```

Your agent now has these tools:

| Tool | What it does |
|---|---|
| `remember` | Store a memory (text + metadata; embedded and indexed automatically, long text chunked transparently). Optionally tag explicit `entities` and typed `relations` to earlier memories, or `supersedes: [id]` to retire an older memory as a correction. Flags near-duplicates of live, unsuperseded memories at write time |
| `recall` | Hybrid search (vector + full-text + recency, RRF-fused) over everything remembered, best match first with scores; `filters: {key: value}` narrows by metadata, `agent` by writing agent, and `expand_related: true` also pulls each hit's explicitly related memories as connected context |
| `related` | Navigate the explicit memory graph: one memory's relations (both directions, with kind, including `supersedes`), or every memory tagged with an entity |
| `forget` | Delete one memory by id (delete by query/age is planned) |
| `stats` | File size, live/forgotten counts, index health, and a per-agent provenance breakdown |

Plus **automatic project-context memory**: EmbedMind detects the project from the agent's working directory (git root, or a `.embedmind.toml` with `project = "name"`), stamps it on every memory, and scopes `recall` to it by default — with `scope: "all"` as the explicit way out.

The CLI works standalone too — this sequence works copy-paste from a clean install:

```bash
embedmind remember "We decided to use tokio for async, see ADR-003"
embedmind recall "why tokio?"
embedmind stats   # size, counts, index health
```

`remember` prints the new memory's id; `recall` prints matches best-first with a cosine
score and the owning project; `stats` reports file size, live/forgotten counts, index
entries and the embedding model. Memories live in `~/.embedmind/memory.mind` by default
(override with `--file`). The graph layer is there too: `remember --entity NAME
--relation refines=ID` tags and links memories explicitly, `embedmind related ID`
(or `--entity NAME`) navigates the links, and `recall --expand-related` pulls
connected context along with the hits.

**Versioned knowledge from the CLI:**

```bash
embedmind remember "We use tokio 1.x for async" # earlier, wrong version
embedmind remember "We use tokio 0.3 for async, see ADR-003" --supersedes <id-above>
embedmind recall "which tokio version?"   # only the correction surfaces
embedmind related <id-above>              # the superseded memory is still there, as history
```

**Observability:** run the server with `--op-log <file>.jsonl` to append a structured
log of every tool call (latency, args, result ids/scores), then inspect usage with
`embedmind report --op-log <file>.jsonl` — sessions, recalls served, top recalled
memories, and memories never recalled in the window. Without `--op-log`, `report` still
works from the store alone (store totals, no per-call history).

```bash
embedmind serve --file ~/.embedmind/memory.mind --op-log ~/.embedmind/ops.jsonl
embedmind report --op-log ~/.embedmind/ops.jsonl --since 7   # window in days
```

## Core dependencies

- **Rust** (stable) — the engine and MCP server are pure Rust, one binary.
- **Embedded embedding model** (ONNX, quantized, CPU-only) — no API key required; bring-your-own-embeddings supported.
- No Python, no GPU, no external database, no network access required.

Python bindings (`remember`/`recall`/`forget`/`stats`, same `.mind` files, byte-for-byte
compatible with the Rust store) ship via PyO3 + maturin — see
[bindings/python](bindings/python); not yet published to PyPI. They don't cover the graph
tools (`related`, `entities`/`relations`, `supersedes`) yet. TypeScript bindings are
planned next — see [ROADMAP.md](ROADMAP.md).

## Benchmarks

In embedded infrastructure, trust is the product — so we show real numbers, including
where we lose, and the methodology is fixed *before* the numbers exist
([docs/BENCHMARKS.md](docs/BENCHMARKS.md)). The harness lives in `benches/`, doubles as the
CI performance-regression guard, and never hand-edits results — the tables below are
rendered straight from [`benches/results/0.1.0-dev.json`](benches/results/0.1.0-dev.json)
(mirrored in [`benches/results/latest.md`](benches/results/latest.md)), the official
`run_all.sh --full` run of 2026-07-13.

Measured on the founder's Windows dev box (x86_64, 20 logical CPUs, CPU-only,
single-thread), EmbedMind `0.1.0-dev`, 384-dim all-MiniLM-L6-v2 int8 embeddings:

| Metric | agent-mem-10k | agent-mem-100k |
|---|---:|---:|
| recall@10 (vs. brute-force, tie-aware) | 1.0000 | 1.0000 |
| recall@10, worst query | 1.0000 | 1.0000 |
| query p50 / p99 (warm, end-to-end) | 19.9 ms / 31.8 ms | 122.7 ms / 255.1 ms |
| ↳ query engine p50 / p99 (no embed) | 16.1 ms / 27.1 ms | 118.2 ms / 249.3 ms |
| ↳ query vector-only p50 / p99 (HNSW half only) | 6.3 ms / 8.5 ms | 20.4 ms / 41.0 ms |
| cold open (`Store::open`) + first query | 14.0 ms + 30.8 ms | 14.6 ms + 236.6 ms |
| `remember` p50 / p99 (end-to-end, **incl. embedding**) | 9.5 ms / 24.5 ms | 8.7 ms / 22.9 ms |
| ingest throughput (end-to-end, incl. embedding) | ~48 mem/s | ~52 mem/s |
| file size on disk | 85.7 MiB | 844.8 MiB |
| peak RSS (ingest / query) | 97.6 MiB / 99.4 MiB | 117.5 MiB / 117.7 MiB |

**Full-text lift — what the hybrid buys you, measured (not assumed)**

The table above measures *cost*. This one measures *benefit*: 100 lexical queries (exact code
identifiers, CLI flags, literal error fragments, hex hashes, ULIDs) with ground-truth-by-construction,
run through `Store::recall` (hybrid) and `Store::recall_vector` (vector-only) over the same
materialized dataset (`benches/src/lexical.rs`, `lexical_lift` in `benches/results/0.1.0-dev.json`):

| Dataset | Hybrid recall@10 | Vector-only recall@10 | Lift | Hybrid p99 | Vector-only p99 |
|---|---:|---:|---:|---:|---:|
| agent-mem-10k | 1.0000 | 0.9100 | **+0.09** | 22.1 ms | 18.6 ms |
| agent-mem-100k | 1.0000 | 0.8200 | **+0.18** | 139.4 ms | 32.5 ms |

**The lift doubles as the corpus grows 10x, it doesn't shrink.** Vector-only degrades (0.9100 →
0.8200) as more near-duplicate embeddings collide in a bigger corpus; the hybrid holds 100% on
both because BM25 finds the exact literal regardless of how crowded the vector space gets around
it. That's the "hybrid, for real" differentiator measured, not asserted — see
[ADR 0023](docs/adr/0023-blockmax-wand-decisao-fase-bmw.md) for the founder's decision (invest in
BlockMax-WAND, keep full-text as default) made with this data in hand. Honesty check: this is the
same full-text path whose p99 misses the latency NFR below — the lift is real, and so is the cost
that BMW is meant to close.

Notes and honesty caveats:

- **`remember` latency includes embedding on CPU.** That is the real cost your agent pays,
  so we report it — but it means our ingest number is *not* comparable to a vectors-only
  store's ingest. Embedding, not indexing, dominates that ~52 mem/s.
- **The `query engine` / `query vector-only` split isolates the full-text cost.** `engine`
  is hybrid search (BM25 + HNSW + RRF fusion + record load) with embedding already
  excluded; `vector-only` is the HNSW half alone on the same query set. The gap between
  them (~208 ms @ 100k) is the full-text scan — see the NFR miss below.
- **recall p99 @ 100k misses the < 50 ms NFR: 255.1 ms, honestly reported, not hidden.**
  Three rounds of optimization (early termination — [ADR 0018](docs/adr/0018-early-termination-no-scan-bm25.md);
  delta+varint postings — [ADR 0021](docs/adr/0021-postings-fts-delta-varint.md); skip-list
  structure — [ADR 0022](docs/adr/0022-postings-fts-skip-lists.md)) cut it ~4.8x from an
  original 1,224.6 ms, but the ceiling wasn't reached. The full-text lift measured above (doubling
  from +0.09 to +0.18 as the corpus grows) is why the founder chose to keep investing rather than
  make full-text opt-in — see [ADR 0017](docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md)
  for the full accounting and [ADR 0023](docs/adr/0023-blockmax-wand-decisao-fase-bmw.md) for the
  BlockMax-WAND decision and its rollback criteria (see [ROADMAP.md](ROADMAP.md) "Fase BMW").
- **recall@10 and peak RSS both pass at 100k**: recall@10 is 1.0000 (tie-aware grading,
  worst query included) and peak RSS is 117.7 MiB — well under the 300 MiB ceiling.
  `remember` p99 < 200 ms end-to-end also passes at 22.9 ms @ 100k.

### vs. baselines — index-only (same pre-computed vectors, same queries, same k)

Same vectors, same 1k queries, same `k`, on `agent-mem-100k` (competitor versions pinned
in `benches/src/competitors.rs`; run behind `--features
compare-sqlite-vec,compare-zvec,compare-chroma`). This plane hands every system,
including EmbedMind, the identical pre-computed vector — EmbedMind's row is its `query
engine` split (embed time excluded), the like-for-like number against a store that never
embeds. Each row states its **scope** — what it returns and what it persists — because a
smaller file or a faster query that does less isn't a win row:

| System | Version | recall@10 | query p50 / p99 | returns | persists |
|---|---|---:|---:|---|---|
| **EmbedMind** | 0.1.0-dev | 1.0000 | 118.2 ms / 249.3 ms | full content + metadata + provenance | text + metadata + full-text index + vectors |
| sqlite-vec | 0.1.10-alpha.4 | _not measured on this run_ | — | rowid + distance only | vectors only |
| zvec | 0.5.1 | _not measured on this run_ | — | primary key + distance only | vectors + primary key only |
| Chroma | 1.5.9 | _not measured on this run_ | — | ids only | vectors + ids |

No baseline was built with its `compare-*` feature on this run (each requires an external
toolchain — the sqlite-vec extension, a zvec build, or a pinned Python + `chromadb`
install), so no head-to-head numbers exist yet for this snapshot — never fabricated. Build
with the relevant feature to fill these rows; see `docs/BENCHMARKS.md` §1.

### vs. baselines — text→result (same embedding toll paid by every system)

The product question: an agent hands text in, gets results out. Every system here would
pay the same embedding toll (all-MiniLM-L6-v2) before it can query — EmbedMind's `query
p50/p99` above already include it end-to-end. This is the plane index-only comparisons
hide: a vector-only store can't skip the embedding cost in real use. No baseline is
measured on this run either, for the same reason as above.

### Where EmbedMind loses (honesty contract, BENCHMARKS.md §4)

No competitor was measured on this run, so no head-to-head loss can be reported yet —
this section is computed by the harness from `benches/results/`, never hand-edited; when
a baseline is measured and wins a metric, it lands here automatically. The one loss we
can already report without a competitor build: **`recall` p99 @ 100k misses its own NFR**
(224.9 ms vs. a 50 ms target), documented above, not smoothed over.

## When to use sqlite-vec instead

EmbedMind is opinionated: it is memory *for agents*, not a general vector database. Reach
for `sqlite-vec` (or a server-based vector DB) instead when:

- **You already run on SQLite** and want vector search as one more table in an existing
  database and transaction — sqlite-vec rides your schema; EmbedMind owns its `.mind` file.
- **You bring your own embeddings and want raw ingest throughput.** sqlite-vec doesn't
  embed for you, so its ingest is vectors-only and will very likely beat EmbedMind's
  end-to-end (embed-included) ingest. If you already have vectors and insert in bulk,
  that's its lane.
- **You need SQL** — joins, arbitrary `WHERE` filters, aggregates — over the same rows as
  your vectors. EmbedMind gives you `recall` with metadata filters, agent filters and
  project scoping, not a general query language.
- **You want a battle-tested, widely deployed dependency today.** sqlite-vec is built on
  SQLite; EmbedMind is pre-1.0.

Reach for EmbedMind when you want *agent memory* specifically: a single crash-safe file,
in-process with no server, embedding built in (no API key, nothing leaves the machine),
automatic project scoping, and a graph layer over your memories (entities, relations,
versioned knowledge via `supersedes`) that a vector table alone doesn't give you.

## Documentation

- [docs/FORMAT.md](docs/FORMAT.md) — the `.mind` file format, byte by byte (public, versioned contract)
- [docs/TESTING.md](docs/TESTING.md) — how "never corrupts your memory" is enforced: crash harness, fuzzing, CI
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md) — benchmark methodology and honesty rules
- [CONTRIBUTING.md](CONTRIBUTING.md) — how to contribute, support expectations, release cadence
- [SECURITY.md](SECURITY.md) — reporting vulnerabilities

## License

[MIT](LICENSE). Contributions are MIT too — see [CONTRIBUTING.md](CONTRIBUTING.md).
