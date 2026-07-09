# embedmind (Python)

Persistent memory for AI agents — one local file, no server. Python bindings
for the [EmbedMind](https://github.com/rodrigoescorsim/embedmind) engine
(vector + full-text, single crash-safe `.mind` file, embedded ONNX embeddings,
nothing leaves the machine).

These bindings are a thin shell over the same Rust engine the CLI and MCP
server use, so `.mind` files are byte-for-byte interchangeable across Python
and Rust.

## Install

```bash
pip install embedmind
```

The wheel bundles the engine and the embedded embedding model — no separate
download, no API key, CPU-only.

## Usage

```python
import embedmind

store = embedmind.Store("memory.mind")  # created if absent

# Remember: content plus optional project scope, typed metadata, provenance.
mid = store.remember(
    "prefers explicit errors over panics",
    project="embedmind",
    metadata={"topic": "conventions", "weight": 3},
    agent="my-agent",
    session_id="sess-42",
)

# Recall: hybrid (vector + full-text) search, best first.
for hit in store.recall("error handling style", limit=5, project="embedmind"):
    print(f"[{hit.score:.3f}] {hit.id}  {hit.content}")

# Metadata filters (exact match, or a (min, max) numeric range) and agent filter.
store.recall("conventions", filters={"weight": (2, 10)}, agent="my-agent")

# Forget (soft-delete; space returns on vacuum). Returns True if a live memory
# was forgotten.
store.forget(mid)

# Stats — counts, file size, index health, per-agent provenance breakdown.
s = store.stats()
print(s.live_memories, s.forgotten_memories, s.file_bytes)
for agent, a in s.by_agent.items():
    print(agent or "(unknown)", a.live_memories, a.sessions)

# Reclaim space from forgotten memories and rebuild the indexes.
store.vacuum()
```

### Metadata types

Metadata values are typed: `str`, `int`, `float`, `bool`, or `None`. They
round-trip to the same Python type they were stored as.

### Recall filters

`filters` is a `dict[str, value]`:

- a scalar (`str` / `int` / `float` / `bool`) means **exact match**;
- a `(min, max)` tuple of numbers means an **inclusive numeric range** — use
  `None` on a side to leave it open (e.g. `(0, None)` for `>= 0`).

Filters are ANDed together. A filter whose type disagrees with the stored
value raises `ValueError`.

## License

MIT.
