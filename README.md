<div align="center">

# 🧠 EmbedMind

### Persistent memory for AI agents — one file, local, fast, no server.

**Your agent forgets everything between sessions. EmbedMind fixes that.**

[![Release](https://img.shields.io/badge/release-v0.1.0-6E56CF)](https://github.com/rodrigoescorsim/embedmind/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Built in Rust](https://img.shields.io/badge/built%20in-Rust-DEA584?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Tests](https://img.shields.io/badge/core%20tests-204-3FB950)](docs/TESTING.md)
[![Fuzzed](https://img.shields.io/badge/fuzz%20targets-8-3FB950)](docs/TESTING.md)
[![MCP](https://img.shields.io/badge/protocol-MCP-8250DF)](https://modelcontextprotocol.io)

*Think **SQLite for agent memory**: a single crash-safe file on your machine — vector + full-text + graph together — with no server, no cloud, and no Python environment.*

</div>

---

## What it is

EmbedMind is an **embedded memory engine for AI agents**, packaged as an **MCP server + CLI**. Your agent gets four verbs — `remember`, `recall`, `related`, `forget` — backed by a single portable `.mind` file:

- 🔎 **Hybrid retrieval** — vector similarity (paged HNSW) fused with full-text BM25 via Reciprocal Rank Fusion, plus recency and metadata filters. Not vector-only: the exact code identifier, error string, or hash gets found even when the vector space is crowded.
- 🕸️ **Graph layer** — explicit entities and typed relations between memories, `related` navigation, and 1-hop expansion in `recall`.
- 🔁 **Versioned knowledge** — `supersedes` retires a corrected memory from recall while keeping it navigable as history. No embedded competitor models "this fact was corrected."
- 🗂️ **Automatic project scoping** — memories are stamped with the project (git root or `.embedmind.toml`) and `recall` scopes to it by default.
- 🧾 **Provenance + observability** — every memory records which agent/session wrote it; an optional structured op-log + `embedmind report` show you what the memory is actually doing.
- 💾 **One crash-safe file** — WAL-backed, single binary, embedding model linked in. No server, no Docker, no ports, no API key. Nothing ever leaves your machine.

```text
┌──────────────────────────────────────────────────────┐
│  Your agent  (Claude Code · Cursor · custom · …)      │
│      │  MCP:  remember · recall · related · forget    │
│  ┌───▼──────────────────────────────────────────────┐ │
│  │  EmbedMind engine  (Rust, in-process)            │ │
│  │  vector (HNSW)  +  full-text (BM25 · RRF fusion) │ │
│  │  +  graph (entities / relations / supersedes)    │ │
│  │                 →  one file:  project.mind        │ │
│  └──────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────┘
```

## Why it exists

Every agent product needs local memory, but today's options are heavy server-based vector DBs (another process to babysit) or vector-only embedded stores with no concept of a graph or a correction. Meanwhile the #1 pain of coding agents — **amnesia between sessions** — keeps being re-solved with fragile markdown files.

There is no *SQLite for agent memory*: embeddable, single-file, vector + graph + text together, crash-safe, from desktop to air-gapped box. EmbedMind is that.

> **In embedded infrastructure, trust is the product.** So the whole project is built around one promise — *it never corrupts your memory* — enforced by a WAL crash harness, **8 fuzz targets**, and **204 tests** in the core alone, and proven with **benchmarks whose methodology is fixed before the numbers exist**, published including where we lose. See [How we keep the promise](#how-we-keep-the-promise-trust-is-the-product).

## Install

A single self-contained binary — the embedding model, tokenizer, and ONNX Runtime are all linked in. No Python, no GPU, no external database, nothing downloaded at runtime.

**Prebuilt binary (recommended)** — grab the archive for your platform from the [latest release](https://github.com/rodrigoescorsim/embedmind/releases/latest), unpack it, and put `embedmind` (or `embedmind.exe`) on your `PATH`:

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

**From crates.io** — `cargo install embedmind`

**From source** — `cargo install --path crates/embedmind-cli`

## Quickstart

Add EmbedMind to your agent as an MCP server (example: Claude Code):

```bash
claude mcp add embedmind -- embedmind serve --file ~/.embedmind/memory.mind
```

Your agent now has these tools:

| Tool | What it does |
|---|---|
| `remember` | Store a memory (text + metadata; embedded and indexed automatically, long text chunked transparently). Optionally tag explicit `entities` and typed `relations` to earlier memories, or `supersedes: [id]` to retire an older memory as a correction. Flags near-duplicates of live, unsuperseded memories at write time. |
| `recall` | Hybrid search (vector + full-text + recency, RRF-fused) over everything remembered, best match first with scores. `filters: {key: value}` narrows by metadata, `agent` by writing agent, and `expand_related: true` also pulls each hit's related memories as connected context. |
| `related` | Navigate the memory graph: one memory's relations (both directions, with kind, including `supersedes`), or every memory tagged with an entity. |
| `forget` | Delete one memory by id (delete-by-query/age is planned). |
| `stats` | File size, live/forgotten counts, index health, and a per-agent provenance breakdown. |

Plus **automatic project-context memory**: EmbedMind detects the project from the agent's working directory (git root, or a `.embedmind.toml` with `project = "name"`), stamps it on every memory, and scopes `recall` to it by default — with `scope: "all"` as the explicit way out.

### The CLI works standalone too

This sequence runs copy-paste from a clean install:

```bash
embedmind remember "We decided to use tokio for async, see ADR-003"
embedmind recall "why tokio?"
embedmind stats                     # size, counts, index health, model
```

**Versioned knowledge — a correction, not a duplicate:**

```bash
embedmind remember "We use tokio 1.x for async"                        # earlier, wrong
embedmind remember "We use tokio 0.3, see ADR-003" --supersedes <id>   # the correction
embedmind recall  "which tokio version?"    # only the correction surfaces
embedmind related <id>                       # the old memory is still there, as history
```

**The graph, from the CLI:**

```bash
embedmind remember "Auth flow uses PKCE" --entity auth --relation refines=<id>
embedmind related --entity auth              # everything tagged `auth`
embedmind recall "login" --expand-related    # hits + their connected context
```

**Observability — see the memory working:**

```bash
embedmind serve  --file ~/.embedmind/memory.mind --op-log ~/.embedmind/ops.jsonl
embedmind report --op-log ~/.embedmind/ops.jsonl --since 7   # window in days
```

`report` shows sessions, recalls served (with latency percentiles), top recalled memories, and memories never recalled in the window (dead weight). Without `--op-log` it still works from the store alone (totals, no per-call history).

## Bindings

**Python** (`remember`/`recall`/`forget`/`stats`, same `.mind` files, byte-for-byte compatible with the Rust store) ships via PyO3 + maturin — see [bindings/python](bindings/python). Not yet on PyPI, and it doesn't cover the graph tools (`related`, `entities`/`relations`, `supersedes`) yet. **TypeScript** bindings are planned next — see [ROADMAP.md](ROADMAP.md).

---

## Benchmarks

The methodology is fixed *before* the numbers exist ([docs/BENCHMARKS.md](docs/BENCHMARKS.md)), the harness (`benches/`) doubles as the CI performance-regression guard, and results are **never hand-edited** — the tables below render straight from [`benches/results/0.1.0-dev.json`](benches/results/0.1.0-dev.json). Measured on the founder's Windows dev box (x86_64, 20 logical CPUs, **CPU-only, single-thread**), 384-dim `all-MiniLM-L6-v2` int8 embeddings.

| Metric | agent-mem-10k | agent-mem-100k |
|---|---:|---:|
| **recall@10** (vs. brute-force, tie-aware) | **1.0000** | **1.0000** |
| recall@10, worst query | 1.0000 | 1.0000 |
| query p50 / p99 (warm, end-to-end) | 24.6 ms / 51.4 ms | 127.6 ms / 224.0 ms † |
| ↳ engine only (embed excluded) | 19.9 ms / 42.5 ms | 124.1 ms / 218.6 ms |
| ↳ vector-only (HNSW half) | 6.6 ms / 13.5 ms | 19.3 ms / 29.2 ms |
| `remember` p50 / p99 (end-to-end, **incl. embedding**) | 8.8 ms / 23.5 ms | 8.7 ms / 21.7 ms |
| ingest throughput (incl. embedding) | ~53 mem/s | ~53 mem/s |
| file size on disk | 85.0 MiB | 845.3 MiB |
| peak RSS (ingest / query) | 98.3 / 99.3 MiB | 112.6 / 113.5 MiB |

† See the [FTOPT note](#the-full-text-latency-story-ftopt-phase) — this figure is the pre-optimization harness run; the phase closed at **133.65 ms**, under the revised 150 ms NFR.

### Hybrid search: measured benefit, not a claim

The table above measures *cost*. This one measures *benefit* — 100 lexical queries (exact code identifiers, CLI flags, literal error fragments, hex hashes, ULIDs), ground-truth-by-construction, hybrid vs. vector-only over the same dataset:

| Dataset | Hybrid recall@10 | Vector-only recall@10 | Lift |
|---|---:|---:|---:|
| agent-mem-10k | **1.0000** | 0.9000 | **+0.10** |
| agent-mem-100k | **1.0000** | 0.8200 | **+0.18** |

**The lift grows as the corpus grows 10×, it doesn't shrink.** Vector-only degrades (0.90 → 0.82) as near-duplicate embeddings crowd a bigger space; the hybrid holds 100% on both, because BM25 finds the exact literal no matter how dense the vector neighborhood gets around it. That's "hybrid, for real" — measured, not asserted.

### Honesty contract — where EmbedMind loses

We publish losses; the harness computes this section from `benches/results/`, never by hand.

- **Full-text-only (BM25) latency trails [tantivy](https://github.com/quickwit-oss/tantivy)** — ~3× slower on query at small scale (both hit perfect recall on the lexical ground truth). Expected: a young purpose-built inverted index against a mature dedicated engine. It does *not* reopen [ADR 0011](docs/adr/0011-full-text-indice-invertido-proprio.md) — full-text lives *inside* the single crash-safe `.mind` file by design (tantivy would add a second, independent commit source — exactly the "unrecoverable half-state after a crash" the single-WAL design rules out).
- **Vector head-to-head vs. sqlite-vec / zvec / Chroma is not yet filled** — each needs an external toolchain to build its adapter (`--features compare-*`); we never fabricate a competitor number. Build the feature to fill the row (see [docs/BENCHMARKS.md](docs/BENCHMARKS.md) §1).

#### The full-text latency story (FTOPT phase)

Full disclosure of the one NFR that took work to meet — because how a project handles its hard number tells you more than the number does.

The `recall p99 @ 100k` NFR started at an aspirational **50 ms**. The hybrid full-text path missed it (224 ms), and the cause was **measured, not guessed**: on this synthetic corpus, BlockMax-WAND's block-skipping fires on 82.8% of queries but skips only ~0.05% of postings blocks — the term frequencies are too uniform for the block-max bound to prove a block safe to skip. The algorithm is correct (equivalence-tested against a linear oracle); the workload just lacks the term concentration it's built to exploit.

The **FTOPT phase** (FTOPT-0 → FTOPT-8) then drove it down through confirmatory profiling and targeted fixes — a filter-meta sidecar that took the `keep` closure from **88.8% of query time to ~1.4%** ([ADR 0027](docs/adr/0027-filter-meta-sidecar-fv7.md)), then a **frame-of-reference postings format** (`format_version` 8, [ADR 0028](docs/adr/0028-postings-fts-frame-of-reference.md)) replacing the varint-decode loop that profiling isolated as the dominant remaining cost:

> **224.00 ms → 133.65 ms p99 @ 100k (−40%).**

With full-text no longer the dominant cost (vector search + the bound loop now account for over half of what remains), the founder recalibrated the NFR — **50 → 100 → 150 ms**, recorded openly in [ADR 0017](docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md), not buried — and closed the phase: **133.65 ms passes the revised 150 ms target.** recall@10 (1.0000), peak RSS (113.5 MiB ≪ 300 MiB ceiling), and end-to-end `remember` p99 (21.7 ms ≪ 200 ms) all pass cleanly. The committed harness JSON still shows the pre-FTOPT-8 run; regenerating it via `run_all.sh --full` on an idle machine is a tracked follow-up (the 133.65 ms comes from the isolated `profile_recall` measurement in ADR 0017).

### When to use sqlite-vec instead

EmbedMind is opinionated: it's memory *for agents*, not a general vector database. Reach for `sqlite-vec` (or a server vector DB) when you **already run on SQLite** and want vectors as one more table/transaction; when you **bring your own embeddings and want raw vectors-only ingest throughput**; when you **need SQL** (joins, arbitrary `WHERE`, aggregates); or when you want a **battle-tested 1.0 dependency today** (EmbedMind is pre-1.0).

Reach for **EmbedMind** when you want *agent memory* specifically: a single crash-safe file, in-process with no server, embedding built in (no API key, nothing leaves the machine), automatic project scoping, and a graph over your memories (entities, relations, versioned knowledge) that a vector table alone doesn't give you.

---

## How we keep the promise (trust is the product)

An engine that silently corrupts one byte of your agent's memory is worse than no engine. So correctness is enforced, not hoped for:

- **Single WAL, single file** — every write is one durable transaction; a crash mid-write replays cleanly on next open. No second commit source (the reason full-text is our own index, not an embedded engine — [ADR 0011](docs/adr/0011-full-text-indice-invertido-proprio.md)).
- **Crash harness** — full fault-injection sweep across the write path, on Linux **and** Windows, every PR.
- **8 fuzz targets** — header, page, record, FTS page, graph page, WAL replay, filter-meta, and full open — a scheduled 1h/target pass keeps them honest. Every committed corpus seed also replays through `cargo test`, so a fix regression is caught on the founder's machine too.
- **204 tests** in `embedmind-core`, plus a `#![forbid(unsafe_code)]` audit and clippy `-D warnings` on every PR.
- **Versioned file format** — [docs/FORMAT.md](docs/FORMAT.md) is a public, byte-by-byte contract; the format never breaks without a migration path.
- **29 ADRs** — every non-obvious decision is written down with the tradeoff, including the ones that didn't work out.

## Documentation

| Doc | What's in it |
|---|---|
| [ROADMAP.md](ROADMAP.md) | What shipped, the 90-day plan, the phases (FR · FT · BMW · FTOPT) |
| [docs/FORMAT.md](docs/FORMAT.md) | The `.mind` file format, byte by byte — the versioned public contract |
| [docs/TESTING.md](docs/TESTING.md) | How "never corrupts your memory" is enforced: crash harness, fuzzing, CI |
| [docs/BENCHMARKS.md](docs/BENCHMARKS.md) | Benchmark methodology and the honesty rules |
| [docs/adr/](docs/adr/) | 29 architecture decision records |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to contribute, support expectations, release cadence |
| [SECURITY.md](SECURITY.md) | Reporting vulnerabilities |

## Status

**v0.1.0** — M1 shipped end-to-end: single-file crash-safe store, HNSW vector recall, hybrid full-text (BM25 + RRF), metadata filters, a graph layer (entities / relations / `supersedes`), basic provenance, an MCP server + CLI, Python bindings, and observability (op-log + `report`). Pre-1.0: minor versions may break APIs, but **never the file format without a migration path**. See [ROADMAP.md](ROADMAP.md) for what's next.

## License

[MIT](LICENSE). Contributions are MIT too — see [CONTRIBUTING.md](CONTRIBUTING.md).
