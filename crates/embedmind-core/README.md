# embedmind-core

The **EmbedMind** memory engine: an embedded, single-file, crash-safe store for
AI-agent memory — vector (HNSW) + full-text + graph — in pure Rust, with zero
network dependencies.

This crate is the engine (the asset). Most users want the ready-to-run tools
instead:

- [`embedmind`](https://crates.io/crates/embedmind) — the CLI and MCP server
  (`cargo install embedmind`).
- [`embedmind-mcp`](https://crates.io/crates/embedmind-mcp) — the standalone MCP
  memory server.

## What it is

> Think *SQLite for agent memory*: one portable, crash-safe file on your
> machine — no server process, no cloud, no Python environment.

- **Single file** (`.mind`), WAL-backed for durability.
- **In-process** — the engine links directly into your binary.
- **Local by default** — nothing leaves the machine; usable air-gapped.
- **Embedded embeddings** — a quantized ONNX model (all-MiniLM-L6-v2, CPU-only)
  is bundled; no API key required. Bring-your-own-embeddings via the `Embedder`
  trait.

## Status

Pre-v0.1, under active development. The `.mind` file format is a versioned,
public contract (see [`docs/FORMAT.md`](https://github.com/rodrigoescorsim/embedmind/blob/main/docs/FORMAT.md)).

## License

MIT. See the [repository](https://github.com/rodrigoescorsim/embedmind).
