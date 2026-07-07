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
