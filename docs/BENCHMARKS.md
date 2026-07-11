# Benchmark Methodology

> The README promises **honest benchmarks, including where we lose** (M1 item 1.7).
> Credibility requires the methodology to be fixed *before* the numbers exist — this
> document is that commitment. Results will be published with v0.1; the harness lives in
> `benches/` and doubles as the CI performance-regression guard.

## 1. What we compare against

| Baseline | Why it's the fair comparison |
|---|---|
| `sqlite-vec` (latest release, inside SQLite) | the incumbent "embedded vector search in one file" — index-layer baseline |
| `zvec` | the closest new embedded vector store — index-layer baseline |
| Chroma (local/embedded mode, pinned version) | the product-category competitor: a local store that also embeds (same all-MiniLM-L6-v2) — the alternative an agent developer actually weighs |
| Brute-force exact scan (our own, in-memory) | recall ceiling + sanity floor for latency claims |

Two comparison planes, always labeled (S17): **index-only** — pre-computed vectors in,
ids out; isolates index quality, the only plane where vector-only stores can appear —
and **text→result** — text in, results out; the product workload, where every system
pays the same embedding toll (measured with the same ONNX pipeline and added to the
systems that don't embed themselves).

Rules of engagement: pinned versions (recorded in results), default/recommended settings
for each baseline (no de-tuning the competition), same hardware, same dataset, same
embeddings fed to all systems (embedding time measured separately — baselines don't embed,
so end-to-end `remember` latency is reported for EmbedMind only and labeled as such).

Each baseline's adapter is gated behind its own build feature (`compare-sqlite-vec`,
`compare-zvec`, `compare-chroma`) so the default harness — and the CI regression guard —
builds and runs everywhere without any native toolchain or interpreter. A box with the
toolchain present flips the feature on to fill that row; without it, the row reports
"not measured" with the reason, never a fabricated number (§4 rule 1). Chroma is pinned
to `chromadb==1.5.9` (recorded in `benches/src/competitors.rs`) and driven through a
Python subprocess (`benches/chroma_bench.py`, needs Python 3 + `pip install
chromadb==1.5.9` on the box — an external, founder-managed dependency, the same shape as
the sqlite-vec/zvec native toolchains) so the adapter never re-embeds: it receives the
same pre-computed vectors every other system in the comparison does.

## 2. Datasets

| Dataset | Size | Purpose |
|---|---|---|
| `agent-mem-10k` / `-100k` | 10k / 100k short memories (1–3 sentences), synthetic but realistic agent-memory distribution (decisions, facts, preferences, code notes), pt-BR + en mixed | the actual product workload |
| Public STS/retrieval subset (fixed, versioned) | 100k passages + queries with relevance labels | recall@k against ground truth, reproducible by outsiders |

Embeddings: all-MiniLM-L6-v2 int8, 384 dims (the shipped default), f32 vectors handed
identically to every system. Datasets + generation scripts are committed (or fetched by
pinned hash) so anyone can re-run everything with `cargo bench` / `benches/run_all.sh`.

## 3. Metrics

| Metric | How measured |
|---|---|
| `recall@10` | vs. brute-force exact top-10 (and vs. labels on the public set); mean **and** per-query distribution (min/p10/p50) — a good mean can hide a catastrophic tail (S16) |
| query latency p50 / p99 | single-thread, 1k queries, warm cache; **and** cold-open first-query (file just opened — the "no server" scenario). Reported **decomposed**: `embed` (query embedding) vs. `engine` (search + fusion + record load) — our embed-inclusive total vs. a vector-only system's search time is exactly the asymmetry this decomposition prevents (S17) |
| ingest throughput | memories/sec, batch and one-at-a-time (agent pattern), fsync `full` |
| file size on disk | after ingest, and after `vacuum` |
| peak RSS | during ingest and during query load |
| cold open time | `Store::open` on the 100k file, including recovery scan |
| crash-recovery time | open after simulated crash with 4 MB WAL |

Hardware: one fixed reference machine (the founder's Windows dev box — specs published)
+ one pinned GitHub Actions Linux runner class. Every results table states machine, OS,
versions, date.

## 4. Reporting rules (the honesty contract)

1. **Publish every metric we measure, including losses.** Expected example: sqlite-vec
   will likely beat us on raw ingest throughput in v0.1 — if so, that's in the README table.
2. No cherry-picked dataset sizes: 10k and 100k always reported side by side.
3. Numbers are regenerated per release by the harness, never hand-edited; the results
   file is CI-generated (`benches/results/<version>.json` → rendered table). The JSON
   and the rendered `latest.md` come from the **same invocation** — two artifacts
   disagreeing about what was measured is itself a violation of this section.
4. If a baseline's result looks wrong, we open an issue on their repo asking for review
   *before* publishing, and link it.
5. Claims in marketing copy must trace to a row in a published results table.
6. Every comparison row states the system's **scope**: what it returns (ids vs. full
   content + metadata) and what it persists (vectors only vs. text + metadata +
   full-text + graph). A smaller on-disk file that stores less is not a win row.

## 5. CI regression guard

Every PR runs the 10k suite on the pinned runner and fails if, vs. the last release
baseline: `recall@10` drops > 1 pt · p99 query latency regresses > 15% · file size grows
> 10% · peak RSS grows > 15%. Nightly runs the 100k suite and plots trends. Thresholds
are deliberately loose (shared-runner noise); the reference machine confirms before a
release is cut.

Implementation: `.github/workflows/bench.yml` (path-filtered to engine/harness changes)
runs the harness and then `compare_baseline` (`benches/src/regression.rs`) against a
baseline. The baseline is *rolling*: the results of the last guard-passing run on
`main`, kept in the CI cache so it comes from the same runner and every check is
enforced; when no rolling baseline exists yet, it falls back to the committed
`benches/results/<version>.json` release baseline — which may come from another
platform, in which case the machine-dependent latency/RSS checks degrade to loud
warnings and only the deterministic recall@10 + file-size checks fail the job.
Locally, `BASELINE=<results.json> ./benches/run_all.sh` runs the same comparison.
