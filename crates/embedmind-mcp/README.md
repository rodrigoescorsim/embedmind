# embedmind-mcp

The **EmbedMind** MCP memory server: gives any MCP-capable agent (Claude Code,
Cursor, custom) persistent memory over a single local file — `remember`,
`recall`, `forget` — with no server process, no cloud, no Python.

Most users install the all-in-one CLI, which embeds this server as
`embedmind serve`:

```bash
cargo install embedmind
claude mcp add embedmind -- embedmind serve --file ~/.embedmind/memory.mind
```

This crate ships the server as a standalone binary (`embedmind-mcp`) for
integrations that prefer it directly. The memory engine itself lives in
[`embedmind-core`](https://crates.io/crates/embedmind-core).

## Tools exposed

| Tool | What it does |
|---|---|
| `remember` | Store a memory (text + metadata; embedded and indexed automatically) |
| `recall` | Semantic search over everything remembered, best match first |
| `forget` | Delete one memory by id |

The protocol is spoken directly over stdio JSON-RPC — no SDK, no async runtime
(see [ADR 0009](https://github.com/rodrigoescorsim/embedmind/blob/main/docs/adr/0009-mcp-stdio-direto-sem-sdk.md)).

## Status

Pre-v0.1, under active development.

## License

MIT. See the [repository](https://github.com/rodrigoescorsim/embedmind).
