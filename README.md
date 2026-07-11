# EmbedMind

> **Your agent forgets everything between sessions. This fixes that.**
> One file, local, fast, no server. Rust.

**Status: v0.1** — hybrid (vector + full-text) memory with metadata filters, a graph
layer (entities/relations, `supersedes` for versioned knowledge) and basic provenance,
crash-safe single file, MCP server + CLI. See [ROADMAP.md](ROADMAP.md) for what shipped
and what's next.

EmbedMind is **persistent memory for AI agents** — an embedded storage engine (vector +
full-text search, metadata filters, and a graph layer, all shipped) packaged as an **MCP
memory server + CLI**. Think *SQLite for agent memory*: a single crash-safe file on your
machine, no server process, no cloud, no Python environment.

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
| `remember` | Store a memory (text + metadata; embedded and indexed automatically, long text chunked transparently). Optionally tag explicit `entities` and typed `relations` to earlier memories, or `supersedes: [id]` to retire an older memory as a correction |
| `recall` | Hybrid search (vector + full-text, RRF-fused) over everything remembered, best match first with scores; `filters: {key: value}` narrows by metadata, `agent` by writing agent, and `expand_related: true` also pulls each hit's explicitly related memories as connected context |
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
CI performance-regression guard, and never hand-edits results — the table below is rendered
straight from `benches/results/`.

Measured on the founder's Windows dev box (x86_64, 20 logical CPUs, CPU-only,
single-thread), EmbedMind `0.1.0-dev`, `agent-mem-10k` (10k short agent memories, 384-dim
all-MiniLM-L6-v2 int8 embeddings):

| Metric | Value |
|---|---:|
| recall@10 (vs. brute-force exact) | 0.9953 |
| recall@10, worst query | 0.9000 |
| query p50 / p99 (warm) | 10.4 ms / 14.3 ms |
| cold open (`Store::open`) + first query | 0.4 ms + 10.6 ms |
| `remember` p50 / p99 (end-to-end, **incl. embedding**) | 7.5 ms / 22.3 ms |
| ingest throughput (end-to-end, incl. embedding) | ~68 mem/s |
| file size on disk | 82 MiB |
| peak RSS (ingest / query) | ~118 MiB |

Notes and honesty caveats:

- **`remember` latency includes embedding on CPU.** That is the real cost your agent pays,
  so we report it — but it means our ingest number is *not* comparable to a vectors-only
  store's ingest. Embedding, not indexing, dominates that ~68 mem/s.
- **The 100k targets pass.** The spec NFRs stated at 100k — recall p99 < 50 ms and peak
  RAM < 300 MiB — are measured on the `agent-mem-100k` dataset and pass (15.5 ms, 281 MiB);
  see [docs/BENCHMARKS.md](docs/BENCHMARKS.md), [`benches/results/latest.md`](benches/results/latest.md)
  and the [CHANGELOG](CHANGELOG.md). `remember` p99 < 200 ms end-to-end passes at 22 ms @ 100k.

### Head-to-head vs. sqlite-vec / zvec

Same vectors, same 1k queries, same `k`, on `agent-mem-10k` (competitor versions pinned in
`benches/src/competitors.rs`; run behind `--features compare-sqlite-vec,compare-zvec`):

| System | Version | recall@10 | query p50 / p99 | ingest (vec-only) | on-disk size |
|---|---|---:|---:|---:|---:|
| **EmbedMind** | 0.1.0-dev | 0.9953 | 10.4 ms / 14.3 ms | — (embeds; see above) | 82 MiB |
| sqlite-vec | 0.1.10-alpha.4 | 0.9984 | 9.8 ms / 13.2 ms | 196/s | 15.3 MiB |
| zvec | 0.5.1 | 0.9912 | 1.1 ms / 1.5 ms | 70905/s | 17.4 MiB |

**Where EmbedMind loses (honesty contract, BENCHMARKS.md §4):** on this 10k set both
baselines are smaller on disk (they store bare vectors; EmbedMind keeps the memory text,
metadata and provenance the product is built around), and both are faster on warm query
p99 — zvec dramatically so, and sqlite-vec's brute-force scan edges out our recall too.
Their ingest is vectors-only and not comparable to EmbedMind's embed-included `remember`
(BENCHMARKS.md §1). These rows are rendered straight from `benches/results/`, never
hand-picked — when a baseline wins a metric it lands in the table and the losses list
automatically.

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
