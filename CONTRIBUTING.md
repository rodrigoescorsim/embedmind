# Contributing to EmbedMind

Thanks for your interest! EmbedMind is built and maintained by a solo developer
([@rodrigoescorsim](https://github.com/rodrigoescorsim)). This document sets honest
expectations so contributing here stays pleasant for everyone — including me.

## Support expectations (read this first)

- **Support is best-effort.** I aim to respond to new issues within 24h around releases,
  but there is no SLA. This is sustainable-solo-maintainer policy, stated upfront.
- **Releases ship on a fixed biweekly cadence**, not on demand. A merged PR rides the
  next release train.
- **Large features need demand:** a big feature enters the roadmap only after being
  requested by 2+ distinct users (issues/discussions). This keeps the project focused —
  see [ROADMAP.md](ROADMAP.md). Small fixes and docs improvements are always welcome.

## Ground rules for changes

1. **Crash-safety beats features.** Anything touching `embedmind-core::storage` or the
   file format must come with crash-harness coverage and, if it changes parsing, fuzz
   corpus updates. Read [docs/FORMAT.md](docs/FORMAT.md) and
   [docs/TESTING.md](docs/TESTING.md) before touching those areas.
2. **The file format is a public contract.** No change may break existing `.mind` files
   without a `format_version` bump and an `embedmind migrate` path. PRs that break the
   format without one will be declined regardless of the feature.
3. **No network, no telemetry in the core.** "Nothing leaves your machine" is auditable
   in the code and is part of the product. PRs adding network dependencies to
   `embedmind-core` will be declined.
4. **Domain logic stays in the core.** `embedmind-mcp` and `embedmind-cli` are thin
   shells: parse → core API call → serialize.

## Code standards

- Rust **stable** only. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must
  pass (CI enforces).
- No `unwrap()` / `panic!` on production paths in the engine; errors are typed
  (`thiserror`) in the lib, with rich context in the CLI.
- `#![forbid(unsafe_code)]` in the engine (the only exception, if it ever exists, is an
  isolated mmap module).
- Tests accompany code: unit tests per module; storage changes add crash-test coverage;
  parser changes add fuzz seeds.
- Commits: small, descriptive, in English. Code, identifiers, and public docs are in
  English.

## Getting started

```bash
git clone https://github.com/<org>/embedmind && cd embedmind
cargo build --workspace
cargo test --workspace          # unit + property tests
cargo test -p embedmind-core --test crash_harness   # fault-injection suite
```

Good first contributions: docs fixes, error-message improvements, new fuzz seeds,
benchmark harness portability, testing EmbedMind with an MCP host I haven't tried.

## Licensing of contributions

The project is MIT. By submitting a PR you agree your contribution is licensed under MIT.
Some directions (history/time-travel, encryption at rest, RBAC/audit, team sync) are
deliberately out of scope for now — PRs implementing them will be declined until the
project's roadmap opens those areas up.

## Reporting bugs

- **Data corruption or data loss: highest priority.** File an issue with the
  `corruption` label; if you can share the `.mind` file (it may contain your data —
  check first) or reproduce with `embedmind stats --verify`, recovery of *your* data
  comes first, the fix second, and an honest postmortem lands in the CHANGELOG.
- Security issues: see [SECURITY.md](SECURITY.md) — please don't open public issues.
