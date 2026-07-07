# EmbedMind

> **Your agent forgets everything between sessions. This fixes that.**
> One file, local, fast, no server. Rust.

EmbedMind is **persistent memory for AI agents** — an embedded storage engine (vector + full-text + graph) packaged as an **MCP memory server + CLI**. Think *SQLite for agent memory*: a single crash-safe file on your machine, no server process, no cloud, no Python environment.

```
┌─────────────────────────────────────────────────┐
│  Your agent (Claude Code, Cursor, custom, ...)  │
│        │ MCP: remember / recall / forget        │
│  ┌─────▼─────────────────────────────────────┐  │
│  │  EmbedMind engine (Rust, in-process)      │  │
│  │  vector (HNSW) + full-text + graph        │  │
│  │  → one file: project.mind                 │  │
│  └───────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

## Why

Every agent product needs local memory, but today's options are server-based vector DBs (heavy, another process to babysit) or vector-only embedded stores. There is no equivalent of SQLite for agent memory: embeddable, single-file, vector + graph + text together, encrypted, from desktop to mobile. Meanwhile the #1 pain of coding agents — **amnesia between sessions** — keeps being re-solved with fragile markdown files.

EmbedMind's answer:

- **Single file** — your agent's entire memory is one portable, crash-safe file (WAL-backed).
- **In-process** — no server, no Docker, no ports. The engine lives inside the MCP server binary.
- **Hybrid retrieval** — vector similarity (HNSW) + full-text + metadata filters; a lightweight graph layer (entities and relations) on the roadmap.
- **Local by default** — nothing ever leaves your machine. Built for the local-first wave, usable in air-gapped environments.
- **Rust** — predictable memory footprint, one static binary per platform.

## Quickstart

> ⚠️ **Status: pre-v0.1 — under active development.** The commands below describe the target UX for v0.1.

```bash
# Install (one command, no dependencies)
cargo install embedmind
# or grab a prebuilt binary from Releases

# Add to your agent as an MCP server (example: Claude Code)
claude mcp add embedmind -- embedmind serve --file ~/.embedmind/memory.mind
```

Your agent now has three tools:

| Tool | What it does |
|---|---|
| `remember` | Store a memory (text + metadata; embedded and indexed automatically) |
| `recall` | Hybrid search over everything remembered (vector + full-text + filters) |
| `forget` | Delete memories by id, query, or age |

Plus **automatic project-context memory**: EmbedMind tracks which project each memory came from, so `recall` is scoped by default to what's relevant.

The CLI works standalone too:

```bash
embedmind remember "We decided to use tokio for async, see ADR-003"
embedmind recall "why tokio?"
embedmind stats   # size, counts, index health
```

## Core dependencies

- **Rust** (stable) — the engine and MCP server are pure Rust, one binary.
- **Embedded embedding model** (ONNX, quantized, CPU-only) — no API key required; bring-your-own-embeddings supported.
- No Python, no GPU, no external database, no network access required.

Bindings (Python, TypeScript) are planned once the engine API stabilizes — see [ROADMAP.md](ROADMAP.md).

## Free vs. Pro

The core is and will remain **MIT**. Paid tiers target teams and regulated environments:

| | Free (MIT) | Pro / Team / Enterprise |
|---|---|---|
| Engine, MCP server, CLI | ✅ Full | ✅ |
| Vector + full-text + graph, single file, WAL | ✅ Full | ✅ |
| Basic provenance (which agent/session wrote what) | ✅ | ✅ |
| **History** — time-travel, memory timeline | — | ✅ |
| **Compliance** — encryption at rest, RBAC, audit trail, air-gap support | — | ✅ |
| **Traceability** — full per-memory provenance and attestation | — | ✅ |
| **Integrations** — team sync, shared memory, connectors | — | ✅ |

Interested in Pro/Team or embedding EmbedMind in your product (commercial license)? Watch the repo — a sign-up page is coming.

## Benchmarks

Honest benchmarks vs. `sqlite-vec`, `zvec` and friends (including where we lose) will be published with v0.1. In embedded infrastructure, trust is the product — we'd rather show you real numbers. The methodology is fixed *before* the numbers exist: see [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Documentation

- [docs/FORMAT.md](docs/FORMAT.md) — the `.mind` file format, byte by byte (public, versioned contract)
- [docs/TESTING.md](docs/TESTING.md) — how "never corrupts your memory" is enforced: crash harness, fuzzing, CI
- [docs/BENCHMARKS.md](docs/BENCHMARKS.md) — benchmark methodology and honesty rules
- [CONTRIBUTING.md](CONTRIBUTING.md) — how to contribute, support expectations, release cadence
- [SECURITY.md](SECURITY.md) — reporting vulnerabilities

## License

Core: [MIT](LICENSE). Premium modules: commercial license. Contributions to this repo are MIT — see [CONTRIBUTING.md](CONTRIBUTING.md) for the open-core boundary.
