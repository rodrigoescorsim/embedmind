# Fuzz regressions

Every crash any fuzzer ever finds lands here as a **minimized input**
(`cargo fuzz tmin <target> <artifact>`), committed together with the fix and
a changelog entry — the brutal-honesty policy from `docs/TESTING.md` §3.

Empty so far. May it stay that way, but don't bet the brand on it: bet on
this directory being embarrassingly public and the fixes being fast.

Naming: `<target>-<short-description>` (e.g. `fuzz_record-oom-metadata-count`).
Each file is also wired into `embedmind-core`'s tests so regressions stay
fixed even when the fuzzers aren't running.
