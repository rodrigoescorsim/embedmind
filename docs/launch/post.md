# Show HN: I built persistent memory for coding agents in Rust — single file, no server

> **STATUS: DRAFT — [MANUAL — founder].** This is the prepared input for the launch post
> (task B1, PRD §6 risk mitigation: launch material ready inside M1, not the night before
> day 35). It has **not** been reviewed or published. Every number below is frozen from
> [`benches/results/latest.md`](../../benches/results/latest.md) (0.1.0-dev, 2026-07-13,
> founder's Windows box, 20 logical CPUs, CPU-only, single-thread) and
> [`docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md`](../adr/0017-otimizacao-do-full-text-escopo-e-metodo.md)
> — if the engine changes before publication, re-run `benches/run_all.sh --full` and
> refresh the tables. A number-provenance appendix at the end maps every figure to its
> source. Publication venue (Show HN vs. other), title, and final cuts are founder
> decisions.

---

I built [EmbedMind](../../README.md) <!-- FOUNDER: swap for the public repo URL at publish
time -->: an embedded memory engine for AI coding agents — vector search + full-text +
(soon) a graph layer, one file on disk, crash-safe, written in Rust. It ships as an MCP
server and CLI, not a database you administer: `remember`, `recall`, `forget`, wired into
Claude Code / any MCP client with one command.

## Why this exists

Every agent session today starts from zero. Long-running coding agents re-derive the same
facts about a codebase every conversation — "we rejected tantivy because it brings its own
storage," "the WAL salt kills stale-frame replay," "don't reopen ADR 0008" — because there's
nowhere durable to put them that isn't a cloud service, a vendor API key, or a database you
now have to run and back up.

EmbedMind is memory that lives next to the code: one `.mind` file, no server process to
manage, no API key, nothing leaves the machine. It embeds text with a built-in ONNX model
(CPU-only), indexes it for both semantic (HNSW) and keyword (BM25) search, and fuses the two
with Reciprocal Rank Fusion — so an exact error code and a paraphrased description both
surface.

The differentiator I care most about: memories aren't just stored, they're **versioned
knowledge**. A `remember` call can mark that it `supersedes` an earlier memory — "we now use
delta+varint encoding for postings, superseding the naive scan" — so an agent recalling
project history gets the current fact, not a contradiction pile-up of everything ever
believed true. That's the piece a flat vector table doesn't give you.

## What it is, concretely

- **One file** (`.mind`): 4 KiB pages, xxh3 checksum per page, WAL for durability, additive
  format evolution (a v1 file still opens under v4 code).
- **Crash-safe by construction**: physical page-level redo WAL, fault-injecting VFS in CI
  that kills the process mid-write at every I/O point and checks invariants, fuzz targets on
  every parser and the WAL replay path.
- **Zero network in the core.** No telemetry, no cloud calls, nothing to audit for — it's
  in the code, not a policy.
- **Embedded embeddings**: a quantized ONNX model ships inside the binary (`include_bytes!`),
  no external API, no key.
- **MIT-licensed**, Rust, `#![forbid(unsafe_code)]` in the core engine.

```
claude mcp add embedmind -- embedmind serve
```

## The honest numbers

Machine: Windows x86_64, 20 logical CPUs, CPU-only, single-thread. `agent-mem-10k` /
`agent-mem-100k` are the repo's own synthetic benchmark datasets — same vectors, same
queries, same k, harness in `benches/`.

| Metric | 10k memories | 100k memories |
|---|---:|---:|
| `remember` p50 / p99 (end-to-end, incl. embedding) | 8.31 / 22.64 ms | 8.52 / 21.90 ms |
| cold open (`Store::open`) | 0.35 ms | 0.27 ms |
| `recall` p50 / p99 (end-to-end, incl. embedding) | 19.31 / 30.15 ms | 111.54 / 224.88 ms |
| recall@10 vs. brute-force ground truth | 1.0000 | 1.0000 |
| peak RSS (query) | 99.2 MiB | 118.3 MiB |
| file size on disk | 84.9 MiB | 844.8 MiB |

And where it currently **misses its own target**, stated plainly rather than smoothed over:
the spec's NFR is `recall` p99 < 50 ms at 100k memories on CPU-only hardware. Measured:
**224.88 ms — a miss.** The full breakdown lives in
[ADR 0017](../adr/0017-otimizacao-do-full-text-escopo-e-metodo.md): the vector half alone
(HNSW, no full-text) is fast — 29.32 ms p99 at 100k — the remaining ~190 ms is the full-text
fusion path, a known and actively-tracked bottleneck (already cut roughly 5x from where it
started this optimization phase, via early-termination scan and delta+varint postings
encoding; a skip-list structure is implemented and tested but not yet wired into the hot
path). If your workload is 100k+ memories and you need sub-50ms `recall`, that gap is real
today — track ADR 0017 or open an issue.

No competitor benchmark is included in this post because none was measured on this exact
run (the comparison feature flags were off) — see [BENCHMARKS.md](../../docs/BENCHMARKS.md)
and the [engine-internals post](post-2-engine-internals.md) for a run that does include
sqlite-vec/zvec head-to-head numbers, honestly reported including the metrics where they win
(sqlite-vec beats EmbedMind on recall@10, warm p99, and file size at 10k; zvec beats it on
warm latency by roughly 10x). This is a deliberate house rule: every benchmark table in this
project is generated by the harness, never hand-edited, and a loss is reported the same way
a win is.

## What's deliberately not here yet

Time-travel/history beyond `supersedes`, at-rest encryption (reserved in the file header
format since day 1 so it can arrive later without breaking the format), RBAC/audit, team
sync — none of that is in v0.1. This is a solo, self-funded project; the roadmap is driven by
what's asked for by 2+ users, not a feature checklist. Support is best-effort.

## Try it

```
cargo install embedmind-cli   # or: download a release binary
embedmind serve               # starts the MCP server
claude mcp add embedmind -- embedmind serve
```

Repo, benchmarks, and format spec are all public. Tell me where I'm wrong — especially about
that 224.88 ms number, I'd rather hear it now than after 1.0.

---

*EmbedMind is MIT-licensed, Rust, local-first agent memory. `remember` / `recall` / `forget`
over MCP, single `.mind` file, crash-safe, no server, no API key.*

---

## Appendix — number provenance (delete before publishing)

| Figure in post | Source |
|---|---|
| `remember` p50/p99 @ 10k / 100k | `benches/results/latest.md` — "remember p50/p99 (e2e, w/ embed)" |
| cold open @ 10k / 100k | `benches/results/latest.md` — "cold open (Store::open)" |
| `recall` p50/p99 @ 10k / 100k (end-to-end) | `benches/results/latest.md` — "query p50/p99 (warm)" |
| recall@10 = 1.0000 both datasets | `benches/results/latest.md` — "recall@10 (vs brute-force)"; tie-aware grading per ADR 0019 |
| peak RSS (query) 99.2 / 118.3 MiB | `benches/results/latest.md` — "peak RSS (query)" |
| file size 84.9 MiB / 844.8 MiB | `benches/results/latest.md` — "file size on disk" |
| recall p99 @ 100k NFR miss (224.88 ms vs. 50 ms target) | `benches/results/latest.md` — NFR verdict table; `docs/adr/0017-...md` §"Fechamento da fase FT" |
| vector-only p99 @ 100k = 29.32 ms | `benches/results/latest.md` — "query vector-only p50/p99" |
| ~190 ms full-text fusion cost, 5x reduction, skip-list implemented but not wired | `docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md` §"Fechamento da fase FT" (referencing ADR 0018 early termination, ADR 0021 delta+varint, ADR 0022 skip lists) |
| sqlite-vec/zvec head-to-head numbers (referenced, not restated) | `docs/launch/post-2-engine-internals.md` appendix, sourced from an earlier `benches/results/latest.md` snapshot (2026-07-09) — re-verify against the current file before citing exact figures at publish time |
| encryption reserved in header since day 1 | ADR 0007; `docs/FORMAT.md` |
| `supersedes` / versioned knowledge | `docs/01-spec.md`; CHANGELOG (graph layer entries) |
| MIT core, no telemetry, zero network | `CLAUDE.md` decisão 1/3, "O que NÃO fazer"; `LICENSE` |
| fault-injecting VFS, fuzz targets | `docs/TESTING.md` |
