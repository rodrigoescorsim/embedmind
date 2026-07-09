# EmbedMind

> **Your agent forgets everything between sessions. This fixes that.**
> One file, local, fast, no server. Rust.

**EmbedMind** is persistent memory for AI agents — an embedded storage engine
(vector + full-text + graph) packaged as a CLI **and** MCP memory server. Think
*SQLite for agent memory*: a single crash-safe file on your machine, no server
process, no cloud, no Python environment.

## Install

```bash
# One command, no dependencies
cargo install embedmind

# Add to your agent as an MCP server (example: Claude Code)
claude mcp add embedmind -- embedmind serve --file ~/.embedmind/memory.mind
```

Your agent now has three tools: `remember`, `recall`, `forget`.

The CLI works standalone too:

```bash
embedmind remember "We decided to use tokio for async, see ADR-003"
embedmind recall "why tokio?"
embedmind stats   # size, counts, index health
```

## Why

- **Single file** — the entire memory is one portable, crash-safe file (WAL-backed).
- **In-process** — no server, no Docker, no ports.
- **Local by default** — nothing ever leaves your machine; usable air-gapped.
- **Embedded embeddings** — quantized ONNX model, CPU-only, no API key.
- **Rust** — one self-contained binary per platform.

The engine lives in [`embedmind-core`](https://crates.io/crates/embedmind-core);
the MCP server in [`embedmind-mcp`](https://crates.io/crates/embedmind-mcp).

## Status

Pre-v0.1, under active development.

## License

MIT. See the [repository](https://github.com/rodrigoescorsim/embedmind).
