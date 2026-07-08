#!/usr/bin/env bash
# Install smoke test for a prebuilt `embedmind` binary (story S8; verifies the
# A1 quickstart + A4 release artifact on a clean machine/folder).
#
# It exercises the exact flow a user gets after unpacking a Release archive or
# running `cargo install embedmind`: `--version`, then a full
# remember -> recall -> stats cycle over a throwaway .mind file, asserting the
# output is coherent at each step. Nothing here touches the user's real
# ~/.embedmind store — everything lives in a temp dir wiped on exit.
#
# Usage:
#   ./scripts/smoke_install.sh                 # find `embedmind` on PATH
#   EMBEDMIND_BIN=./embedmind ./scripts/smoke_install.sh   # test a specific binary
#   EMBEDMIND_BIN=/path/to/embedmind.exe ./scripts/smoke_install.sh   # Windows
#
# If EMBEDMIND_BIN is unset and no `embedmind` is on PATH, the script falls back
# to `cargo run -p embedmind --` so it is runnable from a source checkout
# too (slower — it builds first).
#
# Exit code is non-zero on the first failed assertion, so this doubles as a CI
# gate for the release binary (docs/RELEASING.md "Install smoke test").

set -euo pipefail

# --- locate the binary under test -------------------------------------------
if [[ -n "${EMBEDMIND_BIN:-}" ]]; then
  # A path or command the caller pinned (a downloaded artifact, typically).
  RUN=("${EMBEDMIND_BIN}")
elif command -v embedmind >/dev/null 2>&1; then
  RUN=(embedmind)
else
  echo ">> no EMBEDMIND_BIN and no 'embedmind' on PATH — falling back to cargo run" >&2
  RUN=(cargo run --quiet -p embedmind --)
fi

echo ">> smoke-testing: ${RUN[*]}"

# --- clean scratch state (wiped on any exit) --------------------------------
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/embedmind-smoke.XXXXXX")"
cleanup() { rm -rf "${WORKDIR}"; }
trap cleanup EXIT
MIND="${WORKDIR}/smoke.mind"

# Small helpers. `emb` always pins --file so the real store is never touched;
# --global keeps project auto-detection (git root of this checkout) out of the
# assertions, so the flow is identical from any directory.
emb() { "${RUN[@]}" --file "${MIND}" "$@"; }
fail() { echo "!! SMOKE FAIL: $*" >&2; exit 1; }
pass() { echo "   ok: $*"; }

# --- 1. version -------------------------------------------------------------
echo ">> 1/4  embedmind --version"
VERSION_OUT="$("${RUN[@]}" --version)"
echo "   ${VERSION_OUT}"
# clap prints "embedmind <semver>"; assert the name and a digit are present.
case "${VERSION_OUT}" in
  embedmind*[0-9]*) pass "version string well-formed" ;;
  *) fail "unexpected --version output: '${VERSION_OUT}'" ;;
esac

# --- 2. remember ------------------------------------------------------------
echo ">> 2/4  remember"
NEEDLE="we chose tokio for async, see ADR-003 (smoke $$)"
REM_OUT="$(emb remember "${NEEDLE}" --global)"
echo "   ${REM_OUT}"
# First token is the new memory's ULID (26 chars); output tags it "(global)".
ID="$(printf '%s' "${REM_OUT}" | awk '{print $1}')"
[[ ${#ID} -eq 26 ]] || fail "remember did not print a 26-char ULID: '${REM_OUT}'"
case "${REM_OUT}" in
  *"(global)"*) pass "remembered ${ID} (global)" ;;
  *) fail "remember output missing scope tag: '${REM_OUT}'" ;;
esac

# --- 3. recall --------------------------------------------------------------
echo ">> 3/4  recall"
# Query is semantically related but not identical text — proves real embedding
# + vector search, not a substring match.
REC_OUT="$(emb recall "why did we pick tokio?" --all)"
echo "${REC_OUT}" | sed 's/^/   /'
case "${REC_OUT}" in
  *"${ID}"*) pass "recall returned the remembered id" ;;
  *) fail "recall did not surface ${ID}:\n${REC_OUT}" ;;
esac
case "${REC_OUT}" in
  *"tokio"*) pass "recall echoed the memory content" ;;
  *) fail "recall output missing content:\n${REC_OUT}" ;;
esac

# --- 4. stats ---------------------------------------------------------------
echo ">> 4/4  stats"
STATS_OUT="$(emb stats)"
echo "${STATS_OUT}" | sed 's/^/   /'
case "${STATS_OUT}" in
  *"live memories:      1"*) pass "stats reports exactly one live memory" ;;
  *) fail "stats live count wrong:\n${STATS_OUT}" ;;
esac
# The embedded model must be recorded — proves the ONNX model shipped in the
# binary, not a KV-only degraded build.
case "${STATS_OUT}" in
  *"embedding model:"*[A-Za-z0-9]*)
    case "${STATS_OUT}" in
      *"none (KV-only"*) fail "no embedding model recorded — binary is missing the ONNX model:\n${STATS_OUT}" ;;
      *) pass "embedding model recorded in the file" ;;
    esac ;;
  *) fail "stats missing embedding model line:\n${STATS_OUT}" ;;
esac

echo ">> SMOKE OK — remember -> recall -> stats coherent over a fresh .mind"
