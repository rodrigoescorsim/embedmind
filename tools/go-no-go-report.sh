#!/usr/bin/env bash
# Go/no-go metrics report (PRD §4) — thin wrapper around go_no_go_report.py.
# Loads GITHUB_TOKEN from .env at the repo root if not already exported.
# Usage: ./tools/go-no-go-report.sh [--launch-date YYYY-MM-DD] [--repo owner/name]
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"

if [ -z "${GITHUB_TOKEN:-}" ] && [ -f "$repo_root/.env" ]; then
    # Export only the variable we need, never the whole file.
    GITHUB_TOKEN="$(grep -E '^GITHUB_TOKEN=' "$repo_root/.env" | head -1 | cut -d= -f2-)"
    export GITHUB_TOKEN
fi

python="$(command -v python3 || command -v python)"
exec "$python" "$repo_root/tools/go_no_go_report.py" "$@"
