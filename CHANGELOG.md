# Changelog

All notable changes to EmbedMind are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org) (pre-1.0: minor bumps may break APIs, but **never** the
file format without a migration path).

**Honesty policy:** regressions, data-integrity incidents, and benchmark losses are
recorded here, not buried. If a release fixes a corruption bug, the entry links a
postmortem.

## [Unreleased]

Pre-v0.1 — under active development, repo private until M1 completes
(see [ROADMAP.md](ROADMAP.md)).

### Added
- KV store + public Rust API (M1 item 1.2):
  - `record`: on-disk `MemoryRecord` encoding exactly per
    [docs/FORMAT.md](docs/FORMAT.md) §5 — ULID ids, tombstone flag, project
    scope, basic provenance (agent/session/timestamp), typed metadata
    scalars. Every length prefix is validated before allocation; decoding
    arbitrary bytes never panics.
  - `storage::btree`: record B-tree per FORMAT.md §5.1 — slotted leaves,
    fixed-entry inner nodes, provably-safe byte-midpoint splits, overflow
    chains for records above ~usable/4 bytes (hard cap 32 MiB), in-order
    scan. No delete: `forget` is a tombstone update; orphaned overflow
    chains wait for `embedmind vacuum` (documented leak).
  - `api::Store`: `create`/`open`/`open_or_create`, `remember` (one durable
    transaction per call), `get`, `forget` (tombstone; no-op forgets write
    zero bytes), timeline iteration (`iter`/`iter_all`), clean `close`.
    Custom `Vfs` injection stays available for tests and embedders.
- Fuzzing infrastructure (rest of M1 item 1.8, per
  [docs/TESTING.md](docs/TESTING.md) §3):
  - The five planned targets — `fuzz_header`, `fuzz_page`, `fuzz_record`,
    `fuzz_wal_replay`, `fuzz_open_full` — as thin wrappers over
    `embedmind-core::fuzz` bodies, which also run as stable smoke tests in
    `cargo test` on every platform (libFuzzer itself is Linux-only in CI).
  - Seed corpus generated from real encoder output
    (`cargo run --example gen_fuzz_corpus`), committed under `fuzz/corpus/`;
    `fuzz/regressions/` reserved for minimized crash inputs.
  - CI: short pass (2 min/target) on every PR, nightly scheduled job
    (1h/target), corpus accumulated across runs via cache.
- Record-level crash harness (`tests/crash_records.rs`): the §2 injection
  sweep re-run against the public API (remember/forget/reopen workloads over
  splits, overflow chains and tombstones), with invariant I5 checked against
  a content-keyed reference model. Verified to catch a deliberately injected
  missing-fsync bug (all three sweeps fail with reproducing tuples).
- New dependency: `ulid` (id generation — already in the DESIGN §10 budget).
- Storage layer foundation (M1 items 1.1 + 1.8, built together as
  [docs/TESTING.md](docs/TESTING.md) mandates):
  - `format`: `.mind` header (page 0) and WAL framing exactly per
    [docs/FORMAT.md](docs/FORMAT.md) — little-endian, explicitly serialized,
    xxh3 checksums on every page, version policy (G4) and encrypted-flag
    refusal implemented as typed errors.
  - `storage::vfs`: `trait Vfs`/`VfsFile`, the I/O seam; `RealVfs` with
    positional I/O and advisory locking (`LockFileEx` on Windows).
  - `storage::wal` + `storage::pager`: physical page WAL (commit = append
    frames + fsync + valid commit frame), automatic recovery on every open
    (torn tails discarded, committed prefix applied), checkpointing at 4 MB
    or clean close, single-writer lock, transactions with rollback-by-drop.
  - `storage::sim`: in-memory fault-injecting VFS — kill points before/after
    every mutating I/O op, sector-granular torn writes, lying-fsync mode.
  - Crash-test harness (`tests/crash.rs`): full injection sweep over four
    workloads, invariants I1–I5 checked against an in-memory reference model
    after every simulated power loss; failures print the reproducing
    `(workload, P, mode, seed)` tuple. Runs in `cargo test` on all CI
    platforms. Verified to catch a deliberately injected missing-fsync bug.
- Project documentation: README, ROADMAP, DESIGN, file-format spec
  ([docs/FORMAT.md](docs/FORMAT.md)), testing strategy
  ([docs/TESTING.md](docs/TESTING.md)), benchmark methodology
  ([docs/BENCHMARKS.md](docs/BENCHMARKS.md)), contribution guide, security policy,
  ADRs ([docs/adr/](docs/adr/)).
