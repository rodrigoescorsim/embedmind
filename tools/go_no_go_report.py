#!/usr/bin/env python3
"""Go/no-go metrics report (PRD §4).

Queries the GitHub API (plus crates.io and pypistats.org for downloads) and
prints a markdown table with the four PRD §4 metrics, each classified as
🔴/🟡/🟢 against the committed thresholds, plus the pre-committed decision
rule evaluated automatically.

Deliberately date-agnostic: the report is an on-demand snapshot with no
deadline or gate logic. The only temporal anchor is the optional
--launch-date, relative to the *actual* public launch.

Stdlib only — this tool lives outside the Cargo workspace and must not pull
dependencies. Requires GITHUB_TOKEN in the environment (the shell wrapper
loads it from .env).
"""

import argparse
import datetime as dt
import json
import os
import sys
import urllib.error
import urllib.request

# Windows consoles default to a legacy codepage that cannot encode the
# 🔴/🟡/🟢 markers; the report is UTF-8 regardless of platform.
sys.stdout.reconfigure(encoding="utf-8")

GITHUB_API = "https://api.github.com"
USER_AGENT = "embedmind-go-no-go-report (https://github.com/rodrigoescorsim/embedmind)"

RED, YELLOW, GREEN = "\U0001f534", "\U0001f7e1", "\U0001f7e2"


def http_json(url, token=None, method="GET", body=None):
    headers = {"User-Agent": USER_AGENT, "Accept": "application/vnd.github+json"}
    if token and url.startswith(GITHUB_API):
        headers["Authorization"] = f"Bearer {token}"
    data = json.dumps(body).encode() if body is not None else None
    if data is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def github_paginate(path, token, params=""):
    """Yield items from a paginated GitHub list endpoint."""
    page = 1
    while True:
        sep = "&" if params else ""
        items = http_json(
            f"{GITHUB_API}{path}?{params}{sep}per_page=100&page={page}", token
        )
        if not items:
            return
        yield from items
        if len(items) < 100:
            return
        page += 1


def is_third_party(item, owner):
    user = item.get("user") or item.get("author") or {}
    login = (user.get("login") or "").lower()
    return login != owner.lower() and user.get("type") != "Bot"


def fetch_stars(repo, token):
    return http_json(f"{GITHUB_API}/repos/{repo}", token)["stargazers_count"]


def fetch_third_party_issues(repo, token):
    """Issues + discussions opened by someone other than the repo owner."""
    owner = repo.split("/")[0]
    issues = sum(
        1
        for it in github_paginate(f"/repos/{repo}/issues", token, "state=all")
        if "pull_request" not in it and is_third_party(it, owner)
    )
    discussions = 0
    gh_owner, gh_name = repo.split("/")
    query = """
    query($owner: String!, $name: String!, $cursor: String) {
      repository(owner: $owner, name: $name) {
        discussions(first: 100, after: $cursor) {
          nodes { author { login } authorAssociation }
          pageInfo { hasNextPage endCursor }
        }
      }
    }"""
    cursor = None
    try:
        while True:
            resp = http_json(
                f"{GITHUB_API}/graphql",
                token,
                method="POST",
                body={
                    "query": query,
                    "variables": {"owner": gh_owner, "name": gh_name, "cursor": cursor},
                },
            )
            conn = resp["data"]["repository"]["discussions"]
            for node in conn["nodes"]:
                author = node.get("author") or {}
                if (author.get("login") or "").lower() != owner.lower():
                    discussions += 1
            if not conn["pageInfo"]["hasNextPage"]:
                break
            cursor = conn["pageInfo"]["endCursor"]
    except (urllib.error.HTTPError, KeyError, TypeError):
        # Discussions disabled or not queryable — issues alone still count.
        pass
    return issues, discussions


def fetch_external_merged_prs(repo, token):
    owner = repo.split("/")[0]
    return sum(
        1
        for pr in github_paginate(f"/repos/{repo}/pulls", token, "state=closed")
        if pr.get("merged_at") and is_third_party(pr, owner)
    )


def fetch_crates_weekly(crate):
    """Downloads in the last 7 days from crates.io per-day data (90-day window)."""
    try:
        data = http_json(f"https://crates.io/api/v1/crates/{crate}/downloads")
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None  # not published yet
        raise
    cutoff = (dt.date.today() - dt.timedelta(days=7)).isoformat()
    total = sum(
        d["downloads"] for d in data.get("version_downloads", []) if d["date"] >= cutoff
    )
    total += sum(
        d["downloads"] for d in data.get("meta", {}).get("extra_downloads", [])
        if d["date"] >= cutoff
    )
    return total


def fetch_pypi_weekly(package):
    try:
        data = http_json(f"https://pypistats.org/api/packages/{package}/recent")
        return data["data"]["last_week"]
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None  # not published yet
        raise


def fetch_release_assets_total(repo, token):
    """Cumulative asset downloads — GitHub exposes no weekly window for these."""
    return sum(
        asset.get("download_count", 0)
        for rel in github_paginate(f"/repos/{repo}/releases", token)
        for asset in rel.get("assets", [])
    )


def try_source(fn, *args):
    """Run an optional download-source fetch; on failure return the error text.

    A transient failure (e.g. pypistats rate-limiting with 429) must not kill
    the whole report — the sum is flagged as partial instead.
    """
    try:
        return fn(*args), None
    except (urllib.error.URLError, OSError, KeyError, ValueError) as e:
        return None, str(e)


def classify(value, yellow_min, green_min):
    if value >= green_min:
        return GREEN
    if value >= yellow_min:
        return YELLOW
    return RED


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", default="rodrigoescorsim/embedmind",
                        help="GitHub repo as owner/name (default: %(default)s)")
    parser.add_argument("--crate", default="embedmind",
                        help="crates.io top-level crate; core/mcp are pulled as deps "
                             "of it, counting them too would double-count "
                             "(default: %(default)s)")
    parser.add_argument("--pypi", default="embedmind",
                        help="PyPI package name (default: %(default)s)")
    parser.add_argument("--launch-date", metavar="YYYY-MM-DD",
                        help="actual public launch date; when given, prints weeks "
                             "since launch alongside the table")
    args = parser.parse_args()

    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        sys.exit("erro: GITHUB_TOKEN não definido no ambiente "
                 "(use tools/go-no-go-report.sh, que carrega o .env).")

    stars = fetch_stars(args.repo, token)
    issues, discussions = fetch_third_party_issues(args.repo, token)
    issues_total = issues + discussions
    prs = fetch_external_merged_prs(args.repo, token)
    crates_wk, crates_err = try_source(fetch_crates_weekly, args.crate)
    pypi_wk, pypi_err = try_source(fetch_pypi_weekly, args.pypi)
    releases_total = fetch_release_assets_total(args.repo, token)
    downloads_wk = (crates_wk or 0) + (pypi_wk or 0)
    downloads_partial = bool(crates_err or pypi_err)

    # Thresholds from the PRD §4 table (🟡 lower bound, 🟢 exclusive lower bound).
    rows = [
        ("Estrelas", stars, "< 300", "300–1.500", "> 1.500",
         classify(stars, 300, 1501)),
        ("Issues/discussões de terceiros", issues_total, "< 10", "10–40", "> 40",
         classify(issues_total, 10, 41)),
        ("PRs externos aceitos", prs, "0", "1–5", "> 5",
         classify(prs, 1, 6)),
        ("Downloads recorrentes/semana", downloads_wk, "< 100", "100–1.000", "> 1.000",
         classify(downloads_wk, 100, 1001)),
    ]

    today = dt.date.today().isoformat()
    print(f"## Métricas de go/no-go — snapshot de {today}")
    print()
    if args.launch_date:
        launch = dt.date.fromisoformat(args.launch_date)
        weeks = (dt.date.today() - launch).days / 7
        if weeks < 0:
            print(f"Launch público informado: {launch.isoformat()} (ainda no futuro).")
        else:
            print(f"Launch público: {launch.isoformat()} — "
                  f"**{weeks:.1f} semanas desde o launch**.")
        print()
    print(f"Repo: `{args.repo}` · crate: `{args.crate}` · PyPI: `{args.pypi}`")
    print()
    print("| Métrica | Valor | 🔴 Fraco | 🟡 Bom | 🟢 Forte | Classificação |")
    print("|---|---:|---|---|---|:---:|")
    for name, value, r, y, g, cls in rows:
        print(f"| {name} | {value:,} | {r} | {y} | {g} | {cls} |".replace(",", "."))
    print()

    notes = [f"issues de terceiros: {issues} · discussões de terceiros: {discussions}"]

    def source_txt(value, err):
        if err:
            return f"indisponível ({err})"
        return "não publicado" if value is None else f"{value}"

    notes.append(f"downloads/semana: crates.io {source_txt(crates_wk, crates_err)} "
                 f"+ PyPI {source_txt(pypi_wk, pypi_err)}")
    if downloads_partial:
        notes.append("⚠️ soma de downloads PARCIAL — uma fonte falhou; "
                     "rode de novo antes de usar a classificação desta linha")
    notes.append(f"assets de releases GitHub: {releases_total} acumulado "
                 "(a API não expõe janela semanal; fora da soma acima)")
    for note in notes:
        print(f"- {note}")
    print()

    counts = {c: sum(1 for r in rows if r[5] == c) for c in (RED, YELLOW, GREEN)}
    issues_cls = rows[1][5]
    print("**Regra de decisão (compromisso prévio, PRD §4):**")
    print()
    if counts[GREEN] >= 2 and issues_cls == GREEN:
        verdict = ("**GO para M4–M6** — 2+ métricas 🟢, incluindo "
                   "issues/discussões de terceiros.")
    elif counts[YELLOW] >= 3:
        verdict = ("**Mais 90 dias no núcleo OSS com reposicionamento** — "
                   "maioria 🟡.")
    elif counts[RED] >= 3:
        verdict = ("**Reempacotar a mesma engine com outra porta de entrada** — "
                   "maioria 🔴 (regra pressupõe launch bem executado; avaliação "
                   "qualitativa do founder).")
    else:
        verdict = ("**Zona cinzenta** — nenhuma regra automática se aplica "
                   f"({counts[GREEN]} 🟢 / {counts[YELLOW]} 🟡 / {counts[RED]} 🔴); "
                   "decisão qualitativa do founder.")
    print(verdict)


if __name__ == "__main__":
    main()
