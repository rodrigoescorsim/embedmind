# Benchmark Methodology

> The README promises **honest benchmarks, including where we lose** (M1 item 1.7).
> Credibility requires the methodology to be fixed *before* the numbers exist — this
> document is that commitment. Results will be published with v0.1; the harness lives in
> `benches/` and doubles as the CI performance-regression guard.

## 1. What we compare against

| Baseline | Why it's the fair comparison |
|---|---|
| `sqlite-vec` (latest release, inside SQLite) | the incumbent "embedded vector search in one file" |
| `zvec` | the closest new embedded vector store |
| Brute-force exact scan (our own, in-memory) | recall ceiling + sanity floor for latency claims |

Rules of engagement: pinned versions (recorded in results), default/recommended settings
for each baseline (no de-tuning the competition), same hardware, same dataset, same
embeddings fed to all systems (embedding time measured separately — baselines don't embed,
so end-to-end `remember` latency is reported for EmbedMind only and labeled as such).

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
| `recall@10` | vs. brute-force exact top-10 (and vs. labels on the public set) |
| query latency p50 / p99 | single-thread, 1k queries, warm cache; **and** cold-open first-query (file just opened — the "no server" scenario) |
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
   file is CI-generated (`benches/results/<version>.json` → rendered table).
4. If a baseline's result looks wrong, we open an issue on their repo asking for review
   *before* publishing, and link it.
5. Claims in marketing copy must trace to a row in a published results table.

## 5. CI regression guard

Every PR runs the 10k suite on the pinned runner and fails if, vs. the last release
baseline: `recall@10` drops > 1 pt · p99 query latency regresses > 15% · file size grows
> 10% · peak RSS grows > 15%. Nightly runs the 100k suite and plots trends. Thresholds
are deliberately loose (shared-runner noise); the reference machine confirms before a
release is cut.
