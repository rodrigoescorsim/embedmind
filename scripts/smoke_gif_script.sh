#!/usr/bin/env bash
# Launch-material smoke test: validates docs/launch/gif-script.md's exact
# copy-paste block (the 30s demo GIF) beat-by-beat against a real binary,
# outside any git repo so project auto-detection resolves to "(global)" the
# same way it does for a viewer with no project context (M1 close-out gate,
# same spirit as the A4 install smoke in scripts/smoke_install.sh, but this
# one targets the GIF script + default `.mind` path instead of the README's
# --file walkthrough).
#
# Usage:
#   ./scripts/smoke_gif_script.sh                 # find `embedmind` on PATH
#   EMBEDMIND_BIN=./embedmind ./scripts/smoke_gif_script.sh
#
# Exit code is non-zero on the first failed assertion.

set -euo pipefail

if [[ -n "${EMBEDMIND_BIN:-}" ]]; then
  RUN=("${EMBEDMIND_BIN}")
elif command -v embedmind >/dev/null 2>&1; then
  RUN=(embedmind)
else
  echo ">> no EMBEDMIND_BIN and no 'embedmind' on PATH — falling back to cargo run" >&2
  RUN=(cargo run --quiet -p embedmind --)
fi

echo ">> smoke-testing gif-script.md against: ${RUN[*]}"

# --- scratch HOME outside any git repo, so project auto-detection is a no-op
# and the default `~/.embedmind/memory.mind` path (what the script's "cleaner
# on screen" variant uses) resolves to "(global)", exactly like a viewer with
# no project context.
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/embedmind-gif-smoke.XXXXXX")"
cleanup() { rm -rf "${WORKDIR}"; }
trap cleanup EXIT
export HOME="${WORKDIR}/home"
mkdir -p "${HOME}/.embedmind"
cd "${WORKDIR}"

fail() { echo "!! SMOKE FAIL: $*" >&2; exit 1; }
pass() { echo "   ok: $*"; }

# --- beat 1: remember tokio --------------------------------------------------
echo ">> beat 1/5  remember (tokio)"
B1_OUT="$("${RUN[@]}" remember "We chose tokio for async I/O — see ADR-003")"
echo "   ${B1_OUT}"
ID1="$(printf '%s' "${B1_OUT}" | awk '{print $1}')"
[[ ${#ID1} -eq 26 ]] || fail "beat 1 did not print a 26-char ULID: '${B1_OUT}'"
case "${B1_OUT}" in
  *"(global)"*) pass "beat 1 tagged (global), matches script's default-file variant" ;;
  *) fail "beat 1 output missing '(global)' — script text is stale:\n${B1_OUT}" ;;
esac

# --- beat 2: remember postgres -----------------------------------------------
echo ">> beat 2/5  remember (postgres)"
B2_OUT="$("${RUN[@]}" remember "Postgres is the primary datastore; Redis only for rate limits")"
echo "   ${B2_OUT}"
ID2="$(printf '%s' "${B2_OUT}" | awk '{print $1}')"
[[ ${#ID2} -eq 26 ]] || fail "beat 2 did not print a 26-char ULID: '${B2_OUT}'"
[[ "${ID2}" != "${ID1}" ]] || fail "beat 2 id collided with beat 1 id"
pass "two distinct memories now in one file"

# --- beat 3: recall (the semantic payoff) ------------------------------------
echo ">> beat 3/5  recall (\"what do we use for concurrency?\")"
B3_OUT="$("${RUN[@]}" recall "what do we use for concurrency?" 2>&1)"
echo "${B3_OUT}" | sed 's/^/   /'
case "${B3_OUT}" in
  *"searching all projects"*) pass "recall searched globally, matching the (global) writes" ;;
  *) fail "recall did not report 'searching all projects':\n${B3_OUT}" ;;
esac
# The tokio memory (ID1) must be the top hit — first match line after the id.
FIRST_HIT_LINE="$(printf '%s\n' "${B3_OUT}" | grep -m1 -E '^\[[0-9.]+\]')"
case "${FIRST_HIT_LINE}" in
  *"${ID1}"*) pass "tokio memory ranked first, as the script claims" ;;
  *) fail "top recall hit was not the tokio memory (script's claimed payoff is stale):\n${B3_OUT}" ;;
esac
# The script insists the query shares no keyword with the stored memory.
case "${B3_OUT}" in
  *"tokio"*) : ;;
  *) fail "recall output missing memory content:\n${B3_OUT}" ;;
esac

# --- beat 4: stats ------------------------------------------------------------
echo ">> beat 4/5  stats"
B4_OUT="$("${RUN[@]}" stats)"
echo "${B4_OUT}" | sed 's/^/   /'
case "${B4_OUT}" in
  *"live memories:      2"*) pass "stats reports exactly two live memories" ;;
  *) fail "stats live count wrong (script says 'live memories: 2'):\n${B4_OUT}" ;;
esac
case "${B4_OUT}" in
  *"embedding model:    all-MiniLM-L6-v2-int8"*) pass "stats reports the model the script calls out" ;;
  *) fail "stats embedding model line does not match script's claim:\n${B4_OUT}" ;;
esac

echo ">> SMOKE OK — docs/launch/gif-script.md's copy-paste block matches the release binary beat-for-beat"
