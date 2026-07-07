# Testing Strategy — the reliability moat, operationalized

> EmbedMind's brand promise is "never corrupts your memory". One data-loss bug kills that
> brand (see [ROADMAP.md](../ROADMAP.md), risks). So testing is not a phase — the crash
> harness and fuzzers are built **with** the storage layer (M1 item 1.8), run in CI on
> every platform, and gate every release. This document expands [DESIGN.md](../DESIGN.md) §9.

## 1. The four pillars

| Pillar | Question it answers | Tooling |
|---|---|---|
| Crash tests | "Does `kill -9` at the worst byte lose data?" | custom harness over `trait Vfs` |
| Fuzzing | "Can a corrupt/hostile file crash or confuse the parser?" | `cargo-fuzz` (libFuzzer) |
| Property tests | "Does the engine behave like the obvious in-memory model?" | `proptest` |
| Benchmark guard | "Did this commit regress latency/recall/size?" | harness from [BENCHMARKS.md](BENCHMARKS.md) |

## 2. Crash tests (deterministic fault injection)

All file I/O in `embedmind-core::storage` goes through `trait Vfs` (the SQLite trick):

```rust
trait Vfs {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    fn truncate(&self, len: u64) -> io::Result<()>;
    // lock/unlock elided
}
```

Production uses `RealVfs` (thin passthrough). Tests use `FaultVfs`, which wraps a real or
in-memory file and can be scripted to fail:

- **Kill points:** abort the "process" (panic + catch at test boundary, or actual subprocess kill in the integration variant) before/after every `sync`, and mid-`write_at` (write only the first N bytes — simulated torn write).
- **Torn writes:** a page write may persist any prefix, or interleaved old/new sectors (512-byte granularity), matching real disk behavior on power loss.
- **Lying fsync mode:** `sync` returns Ok but buffers are dropped on crash — documents which guarantees survive broken hardware (answer: integrity always, durability of the last commits no; same stance as SQLite).

### Harness loop

```
for each workload W in {insert-heavy, mixed, forget+vacuum, reopen-loop}:
    for each injection point P (enumerated automatically by counting I/O ops in a dry run):
        run W against FaultVfs, crash at P
        reopen the store (recovery runs)
        check invariants I1–I5
```

Deterministic: `(W, P, seed)` fully reproduces a failure; the tuple is printed on any
invariant violation and becomes a regression test.

### Invariants checked after every simulated crash

| # | Invariant |
|---|---|
| I1 | The store opens (recovery never fails on harness-generated files). |
| I2 | Every transaction the workload saw *confirmed* is fully present. |
| I3 | No effect of an unconfirmed transaction is visible (no half-records, no dangling vec_refs, no orphan HNSW nodes). |
| I4 | Every page checksum in main file + surviving WAL prefix validates. |
| I5 | `recall` over the survivors returns the same results as the in-memory reference model (§4) fed only the confirmed operations. |

Windows runs the **same harness** in CI — `FlushFileBuffers` and `LockFileEx` paths are
exactly the dogfooding-where-nobody-tests advantage, so they are first-class, not a port.

## 3. Fuzzing (`cargo-fuzz`)

Targets (each a `fuzz_target!` over arbitrary bytes):

| target | attacks |
|---|---|
| `fuzz_header` | header parse: magic, versions, bogus page counts/offsets, flag bit 0 set |
| `fuzz_page` | each page type's parser, including slot directories and overflow chains |
| `fuzz_wal_replay` | full recovery: arbitrary WAL bytes against a valid base file |
| `fuzz_record` | `MemoryRecord` deserialization, incl. tagged scalars and huge length prefixes |
| `fuzz_open_full` | end-to-end: arbitrary bytes as a whole `.mind` file → `Store::open` must return `Ok` or a typed error, never panic/UB/OOM |

Rules:

- Length prefixes are validated against remaining page bytes **before** allocation (fuzzers find unchecked-length OOMs in minutes).
- The corpus lives in `fuzz/corpus/` in the repo, seeded from harness-generated valid files, and grows with every CI run's new coverage.
- Every fuzz crash gets: minimized input committed to `fuzz/regressions/`, fix, and a changelog entry (brutal honesty policy).

CI: short fuzz pass (~2 min/target) on every PR; nightly job runs 1h/target.

## 4. Property tests (`proptest`)

Reference model = `HashMap<Ulid, MemoryRecord>` + brute-force search (linear scan; exact
cosine for vector queries). Strategy generates operation sequences
(`remember` / `recall` / `forget` / `reopen` / `vacuum`) and asserts model ≡ engine:

- Exact equality for KV reads, existence, counts, metadata filters, tombstone visibility.
- **Vector recall compared as sets with tolerance** (HNSW is approximate): assert
  `recall@10 ≥ 0.9` against the model's exact top-10 per query, averaged over the sequence
  — checks the index isn't broken without demanding exactness.
- `reopen` inside sequences catches "works until you restart" bugs; `vacuum` inside
  sequences catches index-rebuild divergence.

## 5. CI matrix

| Job | Platforms | When |
|---|---|---|
| unit + property tests | Linux, macOS, **Windows** | every PR |
| crash harness (full injection sweep) | Linux, **Windows** | every PR |
| fuzz (short) | Linux | every PR |
| fuzz (1h/target) | Linux | nightly |
| benchmark guard (see BENCHMARKS.md §5) | fixed runner | every PR, trend graph nightly |
| clippy + fmt + `#![forbid(unsafe_code)]` audit | Linux | every PR |

## 6. Release gate (v0.1 onward)

A release tag requires: green full matrix · nightly fuzz clean for 7 consecutive days ·
benchmark guard within thresholds · zero known corruption issues open. No exceptions —
this list is the moat.
