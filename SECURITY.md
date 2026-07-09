# Security Policy

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Report privately via
GitHub Security Advisories ("Report a vulnerability" on the repo) or by email to
**rodrigo.escorsim@gmail.com** with subject `[EmbedMind security]`.

Best-effort response target: 72 hours for acknowledgment. Solo maintainer — coordinated
disclosure timelines are negotiated per issue, defaulting to 90 days.

## Scope

EmbedMind is a local, in-process engine with **no network surface by design** — the core
makes no network calls, has no telemetry, and requires no API keys. The attack surface
that matters:

- **File parsing:** a crafted `.mind` or `.mind-wal` file causing memory unsafety,
  panics in release builds, resource exhaustion, or misparsing (e.g., a file that opens
  "successfully" with silently wrong contents). These parsers are continuously fuzzed
  (see [docs/TESTING.md](docs/TESTING.md)), but reports are very welcome.
- **Data integrity:** any sequence of operations or crash timing that produces silent
  data loss or corruption that recovery does not detect. We treat integrity violations
  as security-grade bugs.
- **MCP tool misuse hardening:** ways an agent could be induced to destructive actions
  bypassing the built-in guards (e.g., `forget`-by-query without `confirm: true`).
- Dependency vulnerabilities (`cargo audit` runs in CI).

Out of scope: attacks requiring an attacker who already has write access to your files
with your privileges (they can simply delete the file), and encryption at rest until it
ships (the format reserves it, but it is not implemented yet).

## Supported versions

Pre-1.0: only the latest release receives fixes.
