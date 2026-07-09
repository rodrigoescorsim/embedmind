# EmbedMind

> **Your agent forgets everything between sessions. This fixes that.**
> One file, local, fast, no server. Rust.

**Status: v0.1** — semantic (vector) memory, crash-safe single file, MCP server + CLI.
Full-text search and metadata filters are next (M2); a graph layer follows (M3). See
[ROADMAP.md](ROADMAP.md).

EmbedMind is **persistent memory for AI agents** — an embedded storage engine (vector search today; full-text and graph on the roadmap) packaged as an **MCP memory server + CLI**. Think *SQLite for agent memory*: a single crash-safe file on your machine, no server process, no cloud, no Python environment.

```
┌─────────────────────────────────────────────────┐
│  Your agent (Claude Code, Cursor, custom, ...)  │
│        │ MCP: remember / recall / forget        │
│  ┌─────▼─────────────────────────────────────┐  │
│  │  EmbedMind engine (Rust, in-process)      │  │
│  │  vector (HNSW) now · full-text + graph →  │  │
│  │  → one file: project.mind                 │  │
│  └───────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

## Why

Every agent product needs local memory, but today's options are server-based vector DBs (heavy, another process to babysit) or vector-only embedded stores. There is no equivalent of SQLite for agent memory: embeddable, single-file, vector + graph + text together, encrypted, from desktop to mobile. Meanwhile the #1 pain of coding agents — **amnesia between sessions** — keeps being re-solved with fragile markdown files.

EmbedMind's answer:

- **Single file** — your agent's entire memory is one portable, crash-safe file (WAL-backed).
- **In-process** — no server, no Docker, no ports. The engine lives inside the MCP server binary.
- **Semantic retrieval today, hybrid on the roadmap** — vector similarity (paged HNSW) with automatic chunking of long memories now; full-text + metadata filters next (M2), then a lightweight graph layer (entities and relations, M3).
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

Your agent now has three tools:

| Tool | What it does |
|---|---|
| `remember` | Store a memory (text + metadata; embedded and indexed automatically, long text chunked transparently) |
| `recall` | Semantic search over everything remembered, best match first with scores (full-text + filters join in M2) |
| `forget` | Delete one memory by id (delete by query/age is planned) |

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
(override with `--file`).

## Core dependencies

- **Rust** (stable) — the engine and MCP server are pure Rust, one binary.
- **Embedded embedding model** (ONNX, quantized, CPU-only) — no API key required; bring-your-own-embeddings supported.
- No Python, no GPU, no external database, no network access required.

Bindings (Python, TypeScript) are planned once the engine API stabilizes — see [ROADMAP.md](ROADMAP.md).

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
| query p50 / p99 (warm) | 10.6 ms / 17.1 ms |
| cold open (`Store::open`) + first query | 0.3 ms + 12.0 ms |
| `remember` p50 / p99 (end-to-end, **incl. embedding**) | 6.7 ms / 16.7 ms |
| ingest throughput (end-to-end, incl. embedding) | ~82 mem/s |
| file size on disk | 82 MiB |
| peak RSS (ingest / query) | ~112 MiB |

Notes and honesty caveats:

- **`remember` latency includes embedding on CPU.** That is the real cost your agent pays,
  so we report it — but it means our ingest number is *not* comparable to a vectors-only
  store's ingest. Embedding, not indexing, dominates that ~82 mem/s.
- **No head-to-head with sqlite-vec / zvec is published yet.** The harness pins their
  versions (sqlite-vec 0.1.6, zvec 0.2.0) and can measure them behind
  `--features compare-sqlite-vec,compare-zvec`, but those rows require the native
  toolchains and are **not measured on the current run** — so we report no number rather
  than a fabricated one (BENCHMARKS.md §4). When they are measured and win a metric, that
  loss goes in the table automatically.
- **The 100k targets are not in this run.** The spec NFRs stated at 100k — recall p99
  < 50 ms and peak RAM < 300 MiB — need the `agent-mem-100k` dataset; see
  [docs/BENCHMARKS.md](docs/BENCHMARKS.md) and the [CHANGELOG](CHANGELOG.md) for the
  recorded result. The one NFR measurable at 10k, `remember` p99 < 200 ms end-to-end,
  passes at 16.7 ms.

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
  your vectors. EmbedMind gives you `recall` + project scoping, not a query language
  (metadata filters land in M2).
- **You want a battle-tested, widely deployed dependency today.** sqlite-vec is built on
  SQLite; EmbedMind is pre-1.0.

Reach for EmbedMind when you want *agent memory* specifically: a single crash-safe file,
in-process with no server, embedding built in (no API key, nothing leaves the machine),
automatic project scoping, and — on the roadmap — a graph layer over your memories that a
vector table alone doesn't give you.

## Documentation

- [docs/FORMAT.md](docs/FORMAT.md) — the `.mind` file format, byte by byte (public, versioned contract)
- [docs/TESTING.md](docs/TESTING.md) — how "never corrupts your memory" is enforced: crash harness, fuzzing, CI
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md) — benchmark methodology and honesty rules
- [CONTRIBUTING.md](CONTRIBUTING.md) — how to contribute, support expectations, release cadence
- [SECURITY.md](SECURITY.md) — reporting vulnerabilities

## License

[MIT](LICENSE). Contributions are MIT too — see [CONTRIBUTING.md](CONTRIBUTING.md).
