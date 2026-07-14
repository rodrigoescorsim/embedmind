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
| `tantivy` (pinned version) | the mature full-text engine ADR 0011 chose *not* to embed — a full-text-only (BM25) plane, isolated from the three vector comparisons above |
| Brute-force exact scan (our own, in-memory) | recall ceiling + sanity floor for latency claims |

Two comparison planes, always labeled and rendered as separate tables (S17):
**index-only** — pre-computed vectors in, ids out; isolates index quality, the only
plane where vector-only stores can legitimately win, since they never pay an embedding
cost here. EmbedMind's row on this plane is its `query engine` split (search + fusion +
record load, embed time excluded) — the like-for-like number against a baseline that
receives ready-made vectors.

And **text→result** — text in, results out, the product workload an agent developer
actually faces. Every system pays the same embedding toll: the query is embedded once
with the shared ONNX pipeline (measured *outside* every competitor, via EmbedMind's own
`query embed` split on that run) and added to the competitor's index-only query time, so
its row becomes genuinely end-to-end — the same shape as EmbedMind's own `query
p50/p99`, which already embeds internally. recall@10 on this plane is EmbedMind's
end-to-end figure against each competitor's own index-only recall (recall doesn't change
with the embedding toll — it is not re-derived, just placed side by side; the index
quality question belongs to the plane above). Chroma is included in this plane under the
same rule as every other competitor: it receives pre-computed vectors (never re-embeds
on its own), so it pays the identical externally-measured embedding cost the sqlite-vec
and zvec rows do.

Both planes obey the same honesty rule (§4): a competitor whose adapter did not run
reports "not measured" with the reason on *both* tables — the text→result plane never
fabricates a sum from a missing number.

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

### 1a. Full-text-only (BM25) plane: EmbedMind vs. tantivy

A separate, fourth plane (`benches/src/fts_compare.rs`, gated behind `compare-tantivy`,
pure Rust so it needs no native toolchain — the simplest of the four adapters to build):
EmbedMind's own inverted index (`Store::search_text`, the keyword half in isolation, no
RRF/vector fusion) against tantivy's BM25 query, on the same corpus and the same lexical
ground-truth queries `benches/src/lexical.rs` already generates (each query's literal
occurs in exactly one document, so recall is graded against an unambiguous target — no
brute-force oracle needed here, unlike the three vector planes). Rendered as its own
labeled table ("Full-text only (BM25): EmbedMind vs. tantivy"), separate from the
index-only/text→result vector planes, since it compares two full-text engines, not a
vector index against a vector-in baseline. This plane exists to put an external number on
[ADR 0011](adr/0011-full-text-indice-invertido-proprio.md)'s architectural decision to
implement full-text as our own inverted index rather than embed tantivy — **the number does
not reopen that decision**, which was made for crash-safety/single-file reasons independent
of relative speed (CLAUDE.md decision 4). What to do with the measured gap, if any, is a
founder call.

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
| `recall@10` | vs. brute-force exact top-10 (and vs. labels on the public set); mean **and** per-query distribution (min/p10/p50) — a good mean can hide a catastrophic tail (S16). Grading is **tie-aware** (score parity, ADR 0019, S27): a returned hit counts when its exact cosine score ties (`SCORE_TIE_EPS = 1e-5`) or beats the k-th exact score, capped at k. The agent-memory corpus holds exact duplicate texts by design (8.4% @ 10k, 23.0% @ 100k), which embed to bit-identical vectors — the exact top-k boundary is routinely a plateau of tied scores wider than k, and *which* tied ids a correct index returns is arbitrary; grading that coin flip as a miss would measure the tie-break, not the index. The same rule grades every competitor |
| query latency p50 / p99 | single-thread, 1k queries, warm cache; **and** cold-open first-query (file just opened — the "no server" scenario). Reported **decomposed**: `embed` (query embedding) vs. `engine` (search + fusion + record load) — our embed-inclusive total vs. a vector-only system's search time is exactly the asymmetry this decomposition prevents (S17). This split feeds both comparison tables: `engine` is EmbedMind's row on the index-only plane, `embed` is added to each competitor's own query time to build their row on the text→result plane |
| ingest throughput | memories/sec, batch and one-at-a-time (agent pattern), fsync `full` |
| file size on disk | after ingest, and after `vacuum` |
| peak RSS | during ingest and during query load |
| cold open time | `Store::open` on the 100k file, including recovery scan |
| crash-recovery time | open after simulated crash with 4 MB WAL |

Hardware: one fixed reference machine (the founder's Windows dev box — specs published)
+ one pinned GitHub Actions Linux runner class. Every results table states machine, OS,
versions, date.

Engine constants that gate behavior on a similarity score are calibrated here too, not
guessed: `benches/src/bin/calibrate_near_dup.rs` (fixed seeds, deterministic) measures
the cosine distributions of synthetic duplicate pairs vs. unrelated corpus pairs with
the shipped model and sweeps candidate thresholds; the chosen value ships as a constant
with the numbers recorded in an ADR (S21's `NEAR_DUP_THRESHOLD`: ADR 0016). Re-run is
mandatory whenever the embedded model changes. Since S21 the ingest measurement drives
`remember_detailed` — the MCP tool's real write path — so the published `remember`
latency includes the near-duplicate scan.

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
baseline: `recall@10` drops > 1 pt · p99 query latency regresses > 15% **and > 8ms** ·
file size grows > 10% · peak RSS grows > 15%. Nightly runs the 100k suite and plots
trends. Thresholds are deliberately loose (shared-runner noise); the reference machine
confirms before a release is cut.

The two latency checks (query p99, remember p99) carry an **absolute noise floor of 8ms**
on top of the percentage: a metric that clears 15% but grew by less than 8ms is reported
as a warning, not a failure. At 10k the p99s are small (query ~22ms, remember ~12ms),
where 15% is under 2ms — smaller than the runner's own I/O jitter, so a slightly slow
sample "regresses" on identical code (observed on `main` 2026-07-14: an 11.87ms remember
p99 baseline vs. a healthy 15-19ms steady state, plus one 169ms fsync spike, all same
code, failing the guard on every push). The floor keeps small-p99 jitter from failing the
job while a real regression — which clears 8ms — still fails; on the 100k p99s (~130ms,
where 15% is ~20ms) the floor is comfortably below the percentage and changes nothing.
recall@10 and file size are deterministic and stay pure-percentage.

A guard failure re-runs the full harness once before failing the job: a shared runner
can stall on fsync (I/O contention on a noisy neighbor) and spike a single p99 sample
far past any threshold that would still catch a real regression — observed 2026-07-12,
`remember p99` at 109ms vs. a 12-19ms steady state on identical code in the surrounding
runs (run 29209346867). One retry with fresh samples tells a transient stall (passes on
retry) from a real regression (fails again); it does not loosen the thresholds above.

"Same runner" is not a fixed shape: GitHub-hosted `ubuntu-latest` runners vary between 2
and 4 vCPUs, and more CPUs on a shared host means more scheduling contention, not less —
a systematic latency shift, not a code regression. Observed 2026-07-12 (run 29212624981):
both the initial attempt and the retry failed `remember p99` (109.68ms, then 79.26ms)
against an 18.79ms baseline recorded on a 2-CPU runner, while the current runs carried 4
CPUs. `same_env` (`benches/src/regression.rs`) now includes CPU count alongside os/arch,
so a baseline recorded on a different CPU count degrades latency/RSS checks to warnings
instead of failing the job — the same treatment already applied across OS/arch.

Implementation: `.github/workflows/bench.yml` (path-filtered to engine/harness changes)
runs the harness and then `compare_baseline` (`benches/src/regression.rs`) against a
baseline. The baseline is *rolling*: the results of the last guard-passing run on
`main`, kept in the CI cache so it usually comes from the same runner shape and every
check is enforced; when no rolling baseline exists yet, or its CPU count differs, it
falls back to comparable behavior via `same_env` — the machine-dependent latency/RSS
checks degrade to loud warnings and only the deterministic recall@10 + file-size checks
fail the job. Locally, `BASELINE=<results.json> ./benches/run_all.sh` runs the same
comparison.
