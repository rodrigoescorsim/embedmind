# Dogfooding EmbedMind on itself: memory for the agent building the memory engine

> **STATUS: DRAFT — [MANUAL — founder].** This is the prepared input for task C4
> (3rd post, real dogfooding use case). It has **not** been reviewed or published.
> Every number below comes from a real, reproducible session, not a simulation:
> 16 `embedmind remember` calls and 5 `embedmind recall` calls run against a fresh
> `.mind` file with the actual release CLI (`0.1.0-dev`, commit `6037c7b`, built
> 2026-07-10, Windows x86_64, CPU-only), each memory a real fact about this project's
> own recent history (drawn from `git log`, ADRs, and `docs/FORMAT.md`). Reproduce
> with `embedmind stats --file <the file used in this run>` — see the provenance
> appendix for the exact commands. Publication venue, title, and final cuts are
> founder decisions. This draft is also the **raw input for the PRD §4 go/no-go
> decision** — see the closing section.

---

The first post was the pitch. The second was the engine internals, benchmarked
against synthetic datasets (`agent-mem-10k`, `agent-mem-100k`). This one is neither:
it's what happened when I pointed EmbedMind at itself — using the CLI to store real
facts about EmbedMind's own development as memories, then querying them back, the
way an agent working on this codebase would.

CLAUDE.md has required this since M1 started: *"Dogfooding obrigatório: o próprio
fluxo de trabalho do founder com agentes usa EmbedMind desde a semana 2 do M1."*
This is the first batch of numbers from actually doing that, end to end with the
release binary — not the in-process Criterion benchmarks in `benches/`.

## The setup

A clean `.mind` file, one project scope (`embedmind`), 16 memories, each one a real
fact pulled from this repository's own history — the kind of thing an agent
would want to remember about the codebase it's working in:

- *"Recovery from the physical page WAL is dumb by design: scan frames from the
  start, validate checksums, apply only transactions with a valid commit frame,
  truncate a torn tail."*
- *"The HNSW index lives directly in file pages — a node's identifier is its
  physical page number, no node_id-to-page lookup table, because the table capped
  at ~405 nodes per 4 KiB page and rewrote whole on every insert (ADR 0008)."*
- *"CI had three red workflows on main fixed in one pass: the manylinux
  compatibility tag had to be the bare tag string, not an image reference, and ort
  was switched to the rustls TLS backend to drop the openssl-sys build dependency
  on Windows runners."*

Each `remember` was a separate CLI process invocation — this matters for the
numbers below, and it's the honest way to read them.

## What it costs to remember something, for real

16 sequential `embedmind remember "<fact>" --file dogfood.mind --project embedmind`
calls, each timed wall-clock from process start to exit:

| | value |
|---|---:|
| fastest | 177 ms |
| slowest | 205 ms |
| median | ~185 ms |
| all 16 succeeded | yes |

This is **not** the 7.37 ms p50 from the engine-internals post, and that gap is the
whole point of this section: that number is the in-process `remember` cost measured
by Criterion, with the ONNX runtime and the store already warm. This number is the
**CLI's** cost — a fresh OS process, cold ONNX Runtime session load, model init,
*then* the same embed-and-write path, `fsync` included. For a coding agent that
shells out to `embedmind remember` once per fact, process-and-model-load overhead
is the dominant term, not the storage engine. It's the reason the MCP server
(`embedmind serve`) exists as the primary integration path — one long-lived process,
model loaded once, every `remember`/`recall` after that pays only the engine cost,
not the cold start. The CLI is for scripting and one-offs; the numbers here quantify
why an agent talking to a persistent MCP connection should see latencies far closer
to the Criterion numbers than to these.

## What it costs to recall something, for real

5 `embedmind recall "<query>" --file dogfood.mind --project embedmind --limit 3`
calls, same process-cost caveat as above:

| | value |
|---|---:|
| fastest | 171 ms |
| slowest | 180 ms |
| median | ~175 ms |

More interesting than the latency is whether the answers were *right*. They were.
Query *"how does the WAL recovery work"* returned the WAL-recovery memory first
(score 0.033), ahead of an unrelated graph-layer memory that merely mentions "WAL"
in passing. Query *"why did we drop the node_id lookup table for HNSW"* returned
the ADR 0008 memory first, even though the query never uses the words "page" or
"physical" that dominate the stored text. Query *"what fixed the CI red workflows"*
correctly ranked the CI memory over an unrelated Python-bindings memory that also
mentions CI. Semantic recall over a 16-memory, single-session corpus is a low bar,
but it's the right low bar: does the thing an agent would actually type get the
right memory back, not a keyword-adjacent one.

## The file itself

```
$ embedmind stats --file dogfood.mind
file:               dogfood.mind
size:               176.0 KiB (44 pages × 4096 bytes)
live memories:      16
forgotten:          0 (space reclaimed by vacuum)
index entries:      16
embedding model:    all-MiniLM-L6-v2-int8 (384 dims)
by agent:
  cli               16 memories
```

176 KiB for 16 memories works out to ~11 KiB/memory — most of that is the HNSW
node pages and the embedding vectors (384 × int8 dims plus f32 working copy),
not the text. That ratio is expected to drop as the corpus grows past this
single-session scale, since fixed-cost pages (header, meta, freelist) amortize;
the 100k-memory numbers in the engine-internals post (`819.8 MiB`, i.e. ~8.2 KiB/
memory) are the better predictor of steady-state size than this 16-memory sample.
The `by agent: cli 16 memories` line is the basic provenance feature (S14) doing
its job — every memory in this file is attributable to the CLI process that wrote
it, no extra bookkeeping required to ask "what did this agent store."

## What this run does and doesn't prove

Sixteen memories in one sitting is a smoke test, not a dogfooding *habit*. It
proves the CLI path works end-to-end against a real, freshly-created file — no
staged data, no synthetic dataset — and that recall quality holds up on real
project facts, not just the curated benchmark queries. It does not yet prove
retention over weeks, behavior at the scale an actual project accumulates, or
whether recall stays useful once memories compete with hundreds of others on
similar topics. The honest next step is running this for real, continuously,
through the MCP server rather than one-off CLI calls, and re-measuring after
enough elapsed time that the numbers describe a habit instead of a demo.

## Why this matters for the go/no-go decision

This post is not itself a go/no-go signal — it's evidence for the founder, one
input among several for the decision that actually matters. **[PRD §4](../00-prd.md#4-métricas-de-sucesso-mensuráveis-com-prazo)**
defines the real decision: the go/no-go snapshot ~7 weeks post-launch (day 90 ≈
2026-10-05), produced on demand by `./tools/go-no-go-report.sh`, graded against
stars, third-party issues/discussions, accepted external PRs, and recurring
weekly downloads — with the decision rule already committed (2+ 🟢 columns incl.
issues → GO for M4–M6; mostly 🟡 → 90 more days in OSS core with a repositioning;
mostly 🔴 with a well-executed launch → repackage the same engine behind a
different entry point). This dogfooding write-up is qualitative, single-session,
founder-produced input for that decision — real community usage signal, not this
post, is what actually moves the needle on day 90.

---

## Appendix — number provenance (delete before publishing)

Every figure above traces to a command run in this session; none are from memory
or simulation.

| Figure in post | Source |
|---|---|
| 16 memories, 0 failures, `remember` 177–205 ms (median ~185 ms) | timed loop of `embedmind remember` calls against `dogfood.mind`, this session |
| `recall` 171–180 ms (median ~175 ms), correct top-1 on all 3 sample queries shown | timed loop of `embedmind recall` calls against `dogfood.mind`, this session |
| `stats` output (176.0 KiB, 44 pages, 16 live, 0 forgotten, 16 index entries, all-MiniLM-L6-v2-int8/384 dims, `cli: 16 memories`) | `embedmind stats --file dogfood.mind` — reproducible; re-run to verify |
| 7.37 ms p50 / 22.43 ms p99 in-process `remember` (comparison baseline) | `docs/launch/post-2-engine-internals.md`, sourced from `benches/results/latest.md` |
| 819.8 MiB / 100k memories ≈ 8.2 KiB/memory (steady-state size comparison) | `docs/launch/post-2-engine-internals.md` → `benches/results/latest.md` |
| Binary version/commit/platform (`0.1.0-dev`, `6037c7b`, 2026-07-10, Windows x86_64) | `git rev-parse HEAD`, `Cargo.toml` workspace version, this session |
| `remember` has no `--agent` flag; agent is fixed to `"cli"` for CLI-originated memories | `crates/embedmind-cli/src/main.rs`, `remember()` — `MemoryDraft::new(content).agent("cli")` |
| PRD §4 go/no-go table, thresholds, decision rule, `tools/go-no-go-report.sh` | `docs/00-prd.md` §4 |
