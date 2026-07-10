//! End-to-end tests of the `embedmind` binary (M1 item 1.6): the README
//! quickstart flow, driven through real processes against a real file —
//! exactly what a user gets after `cargo install embedmind`.
//!
//! Each invocation loads the embedded ONNX model, so the flow is packed
//! into few processes. The working directory is a scratch dir with no
//! project markers, keeping project auto-detection out of the picture
//! except where a test creates markers on purpose.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Scratch directory (unique per test) under the system temp dir, removed
/// on drop. Also serves as a marker-free cwd for the spawned processes.
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("embedmind-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn store(&self) -> PathBuf {
        self.0.join("memory.mind")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `embedmind` with `args`, cwd at `dir`. Returns (exit ok, stdout,
/// stderr).
fn run(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_embedmind"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("binary must spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn quickstart_flow_remember_recall_forget_stats() {
    let scratch = Scratch::new("flow");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    // remember (global: scratch dir has no project markers)
    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "remember",
            "we decided to use tokio for async, see ADR-003",
        ],
    );
    assert!(ok, "remember failed: {stderr}");
    assert!(stdout.contains("(global)"), "no project context: {stdout}");
    let id = stdout.split_whitespace().next().unwrap().to_string();
    assert_eq!(id.len(), 26, "first token must be the ULID: {stdout}");

    // recall finds it, with score and id
    let (ok, stdout, stderr) = run(scratch.path(), &["--file", file, "recall", "why tokio?"]);
    assert!(ok, "recall failed: {stderr}");
    assert!(stdout.contains(&id), "hit must show the id: {stdout}");
    assert!(stdout.contains("tokio for async"), "hit must show content");
    assert!(
        stderr.contains("searching all projects"),
        "scope echoed: {stderr}"
    );

    // stats reflects one live memory and the embedded model
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "stats"]);
    assert!(ok);
    assert!(stdout.contains("live memories:      1"), "{stdout}");
    assert!(stdout.contains("all-MiniLM-L6-v2-int8"), "{stdout}");

    // forget, then recall no longer returns it
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "forget", &id]);
    assert!(ok);
    assert!(stdout.contains("forgotten"));
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "recall", "why tokio?"]);
    assert!(ok);
    assert!(!stdout.contains(&id), "forgotten memory must not appear");

    // forgetting again is a clear error, not silence
    let (ok, _, stderr) = run(scratch.path(), &["--file", file, "forget", &id]);
    assert!(!ok);
    assert!(stderr.contains("no live memory"), "{stderr}");

    // stats now shows the tombstone
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "stats"]);
    assert!(ok);
    assert!(stdout.contains("live memories:      0"), "{stdout}");
    assert!(stdout.contains("forgotten:          1"), "{stdout}");
}

/// S9 edge: a `.mind` written before the full-text index existed (header's
/// `fts_root_page == 0`) must still `recall` — vector-only hits, a warning on
/// stderr, exit 0. Degradation is graceful, never an error.
#[test]
fn recall_on_legacy_file_without_fts_index_warns_and_succeeds() {
    let scratch = Scratch::new("legacy-fts");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    let (ok, stdout, stderr) = run(
        scratch.path(),
        &["--file", file, "remember", "the kitten sleeps on the rug"],
    );
    assert!(ok, "remember failed: {stderr}");
    let id = stdout.split_whitespace().next().unwrap().to_string();

    // Rewind the header to the pre-M2 shape: drop the full-text root pointer,
    // exactly what an old file presents on open.
    {
        use embedmind_core::storage::{Pager, PagerOptions, RealVfs};
        use std::sync::Arc;
        let mut pager = Pager::open(Arc::new(RealVfs), &store, PagerOptions::default()).unwrap();
        let mut txn = pager.begin().unwrap();
        txn.set_fts_root_page(0);
        txn.commit().unwrap();
        pager.close().unwrap();
    }

    let (ok, stdout, stderr) = run(
        scratch.path(),
        &["--file", file, "recall", "a small feline resting"],
    );
    assert!(ok, "recall on a legacy file must succeed: {stderr}");
    assert!(stdout.contains(&id), "vector-only hit expected: {stdout}");
    assert!(
        stderr.contains("no full-text index"),
        "stderr must carry the degradation warning: {stderr}"
    );
}

#[test]
fn project_detection_scopes_cli_remember_and_recall() {
    let scratch = Scratch::new("proj");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    // A fake repo root: directory name is the project.
    let repo = scratch.path().join("myproj");
    fs::create_dir_all(repo.join(".git")).unwrap();

    let (ok, stdout, stderr) = run(&repo, &["--file", file, "remember", "note inside the repo"]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("(project: myproj)"), "{stdout}");

    // From inside the repo: scoped by default.
    let (ok, _, stderr) = run(&repo, &["--file", file, "recall", "note"]);
    assert!(ok);
    assert!(stderr.contains("searching project: myproj"), "{stderr}");

    // From outside with --all: still findable.
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "recall", "note", "--all"]);
    assert!(ok);
    assert!(stdout.contains("note inside the repo"), "{stdout}");

    // --global overrides detection.
    let (ok, stdout, _) = run(
        &repo,
        &["--file", file, "remember", "global note", "--global"],
    );
    assert!(ok);
    assert!(stdout.contains("(global)"), "{stdout}");
}

/// `embedmind vacuum` reclaims the space held by forgotten memories: remember
/// a few, forget most, vacuum, and `stats` must show the file shrink and the
/// tombstones gone while the survivors still recall (S11 / B4).
#[test]
fn vacuum_reclaims_forgotten_space() {
    let scratch = Scratch::new("vacuum");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    // Several sizeable memories so forgetting most of them frees real pages.
    let mut ids = Vec::new();
    for i in 0..6 {
        let text = format!("decision {i}: {}", "context ".repeat(40));
        let (ok, stdout, stderr) = run(scratch.path(), &["--file", file, "remember", &text]);
        assert!(ok, "remember {i} failed: {stderr}");
        ids.push(stdout.split_whitespace().next().unwrap().to_string());
    }

    // Forget all but the last.
    for id in &ids[..5] {
        let (ok, _, stderr) = run(scratch.path(), &["--file", file, "forget", id]);
        assert!(ok, "forget failed: {stderr}");
    }

    let (ok, before, _) = run(scratch.path(), &["--file", file, "stats"]);
    assert!(ok);
    assert!(before.contains("live memories:      1"), "{before}");
    assert!(before.contains("forgotten:          5"), "{before}");
    let before_pages = pages_of(&before);

    // Vacuum: reports the reclaim and leaves a single, smaller file.
    let (ok, stdout, stderr) = run(scratch.path(), &["--file", file, "vacuum"]);
    assert!(ok, "vacuum failed: {stderr}");
    assert!(stdout.contains("vacuumed:"), "{stdout}");
    assert!(stdout.contains("5 forgotten reclaimed"), "{stdout}");

    // stats after: no tombstones, the survivor intact, fewer (or equal) pages.
    let (ok, after, _) = run(scratch.path(), &["--file", file, "stats"]);
    assert!(ok);
    assert!(after.contains("live memories:      1"), "{after}");
    assert!(after.contains("forgotten:          0"), "{after}");
    let after_pages = pages_of(&after);
    assert!(
        after_pages <= before_pages,
        "vacuum must not grow the file: {before_pages} -> {after_pages}"
    );
    assert!(
        after_pages < before_pages,
        "forgetting 5 of 6 then vacuuming should reclaim pages: {before_pages} -> {after_pages}"
    );

    // The survivor still recalls after the swap.
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "recall", "decision 5"]);
    assert!(ok);
    assert!(
        stdout.contains(&ids[5]),
        "survivor must still recall: {stdout}"
    );

    // No orphan temp/scratch left beside the store.
    assert!(!store.with_extension("mind-vacuum-tmp").exists());
    let dir = fs::read_dir(scratch.path()).unwrap();
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.contains("vacuum-tmp") && !name.contains("vacuum-scratch"),
            "orphan left behind: {name}"
        );
    }
}

/// Extracts the page count from a `stats` "size:" line, e.g.
/// `size:  12.0 KiB (24 pages × 512 bytes)` → 24.
fn pages_of(stats: &str) -> u64 {
    let line = stats
        .lines()
        .find(|l| l.trim_start().starts_with("size:"))
        .expect("stats has a size line");
    let (_, rest) = line.split_once('(').expect("size line has a paren");
    rest.split_whitespace()
        .next()
        .expect("page count token")
        .parse()
        .expect("page count is a number")
}

/// `embedmind recall --filter` narrows results by metadata (S10). Metadata is
/// set through the MCP `serve` path (the CLI `remember` has no metadata flag),
/// then a filtered CLI recall must return only the matching memory.
#[test]
fn recall_filter_narrows_by_metadata() {
    let scratch = Scratch::new("filter");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    // Store two memories with distinct metadata via `serve` (one process).
    let mut child = Command::new(env!("CARGO_BIN_EXE_embedmind"))
        .args(["--file", file, "serve"])
        .current_dir(scratch.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve must spawn");
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin
            .write_all(
                concat!(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"remember","arguments":{"content":"deploy runbook for the release","project":null,"metadata":{"topic":"ops","priority":9}}}}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"content":"design notes for the release","project":null,"metadata":{"topic":"design","priority":2}}}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
    }
    drop(child.stdin.take());
    assert!(child.wait_with_output().unwrap().status.success());

    // Filter topic=ops: only the ops memory comes back.
    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "recall",
            "release",
            "--all",
            "--filter",
            "topic=ops",
        ],
    );
    assert!(ok, "filtered recall failed: {stderr}");
    assert!(
        stdout.contains("deploy runbook"),
        "ops memory expected: {stdout}"
    );
    assert!(
        !stdout.contains("design notes"),
        "design memory must be filtered out: {stdout}"
    );

    // Numeric range priority>=5: still only the ops memory (priority 9).
    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "recall",
            "release",
            "--all",
            "--filter",
            "priority>=5",
        ],
    );
    assert!(ok, "range recall failed: {stderr}");
    assert!(stdout.contains("deploy runbook"), "{stdout}");
    assert!(!stdout.contains("design notes"), "{stdout}");

    // A malformed filter is a clear error, not a silent empty result.
    let (ok, _, stderr) = run(
        scratch.path(),
        &["--file", file, "recall", "release", "--filter", "garbage"],
    );
    assert!(!ok);
    assert!(stderr.contains("invalid --filter"), "{stderr}");
}

/// S14: `embedmind recall --agent` narrows results by writing agent, and
/// `embedmind stats` shows the per-agent breakdown of live memories. Two
/// memories are stored under different agents via `serve` (whose agent is the
/// `clientInfo.name`), since the CLI `remember` always writes as "cli".
#[test]
fn recall_by_agent_and_stats_breakdown() {
    let scratch = Scratch::new("provenance");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    // Agent "alpha-agent" and "beta-agent" each remember one memory, in one
    // serve process per agent (agent = clientInfo.name from initialize).
    for (agent, content) in [
        ("alpha-agent", "the cat sat on the warm mat"),
        ("beta-agent", "a feline naps on the soft rug"),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_embedmind"))
            .args(["--file", file, "serve"])
            .current_dir(scratch.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("serve must spawn");
        {
            let stdin = child.stdin.as_mut().unwrap();
            let init = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientInfo":{{"name":"{agent}","version":"0"}}}}}}"#
            );
            let remember = format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"remember","arguments":{{"content":"{content}","project":null}}}}}}"#
            );
            stdin
                .write_all(format!("{init}\n{remember}\n").as_bytes())
                .unwrap();
        }
        drop(child.stdin.take());
        assert!(child.wait_with_output().unwrap().status.success());
    }

    // recall --agent alpha-agent: only alpha's memory, even for a query that
    // semantically matches both.
    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "recall",
            "a resting cat",
            "--all",
            "--agent",
            "alpha-agent",
        ],
    );
    assert!(ok, "agent-filtered recall failed: {stderr}");
    assert!(stdout.contains("cat sat on the warm mat"), "{stdout}");
    assert!(
        !stdout.contains("feline naps"),
        "beta's memory must be filtered out: {stdout}"
    );
    assert!(
        stderr.contains("filtered to agent: alpha-agent"),
        "{stderr}"
    );

    // stats shows both agents, one live memory each.
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "stats"]);
    assert!(ok);
    assert!(stdout.contains("live memories:      2"), "{stdout}");
    assert!(stdout.contains("by agent:"), "{stdout}");
    assert!(stdout.contains("alpha-agent"), "{stdout}");
    assert!(stdout.contains("beta-agent"), "{stdout}");
}

/// S13 through the CLI: `remember --entity/--relation` writes explicit graph
/// data, `related` navigates it by id (both directions) and by entity,
/// `recall --expand-related` pulls the connected neighbor as context, and a
/// forgotten neighbor's relation disappears with the tombstone.
#[test]
fn graph_remember_related_and_expand() {
    let scratch = Scratch::new("graph");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    // A: the base decision.
    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "remember",
            "we chose postgres for the storage layer",
        ],
    );
    assert!(ok, "remember A failed: {stderr}");
    let a_id = stdout.split_whitespace().next().unwrap().to_string();

    // B refines A and is tagged with an entity.
    let relation = format!("refines={a_id}");
    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "remember",
            "specifically postgres sixteen with the pgvector extension",
            "--entity",
            "postgres",
            "--relation",
            &relation,
        ],
    );
    assert!(ok, "remember B failed: {stderr}");
    let b_id = stdout.split_whitespace().next().unwrap().to_string();
    assert!(stdout.contains("entities: postgres"), "{stdout}");
    assert!(
        stdout.contains(&format!("relation: refines -> {a_id}")),
        "{stdout}"
    );

    // related B: outgoing edge to A, with B's entity tags.
    let (ok, stdout, stderr) = run(scratch.path(), &["--file", file, "related", &b_id]);
    assert!(ok, "related B failed: {stderr}");
    assert!(stdout.contains("entities: postgres"), "{stdout}");
    assert!(stdout.contains("-> refines"), "{stdout}");
    assert!(stdout.contains(&a_id), "{stdout}");

    // related A: the same edge, incoming.
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "related", &a_id]);
    assert!(ok);
    assert!(stdout.contains("<- refines"), "{stdout}");
    assert!(stdout.contains(&b_id), "{stdout}");

    // related --entity postgres: B is the only member.
    let (ok, stdout, _) = run(
        scratch.path(),
        &["--file", file, "related", "--entity", "postgres"],
    );
    assert!(ok);
    assert!(stdout.contains(&b_id), "{stdout}");
    assert!(stdout.contains("pgvector"), "{stdout}");
    assert!(
        !stdout.contains("storage layer"),
        "A is not tagged: {stdout}"
    );

    // recall --expand-related: B ranks (limit 1), A comes along as context
    // marked "rel" instead of a score.
    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "recall",
            "the pgvector extension",
            "--limit",
            "1",
            "--expand-related",
        ],
    );
    assert!(ok, "expanded recall failed: {stderr}");
    assert!(stdout.contains(&b_id), "ranked hit expected: {stdout}");
    assert!(
        stdout.contains(&a_id) && stdout.contains("[  rel]"),
        "neighbor must come along marked as related context: {stdout}"
    );

    // Malformed / dangling relations are clear errors.
    let (ok, _, stderr) = run(
        scratch.path(),
        &["--file", file, "remember", "x", "--relation", "garbage"],
    );
    assert!(!ok);
    assert!(stderr.contains("invalid --relation"), "{stderr}");
    let (ok, _, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "remember",
            "x",
            "--relation",
            "refines=01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ],
    );
    assert!(!ok, "dangling relation target must fail the remember");
    assert!(stderr.contains("remember failed"), "{stderr}");

    // Forget A: the relation disappears with the tombstone.
    let (ok, _, _) = run(scratch.path(), &["--file", file, "forget", &a_id]);
    assert!(ok);
    let (ok, stdout, stderr) = run(scratch.path(), &["--file", file, "related", &b_id]);
    assert!(ok);
    assert!(
        !stdout.contains(&a_id),
        "relation to a forgotten memory must disappear: {stdout}"
    );
    assert!(stderr.contains("no related memories"), "{stderr}");
    let (ok, _, stderr) = run(scratch.path(), &["--file", file, "related", &a_id]);
    assert!(!ok, "related on a forgotten id is an error");
    assert!(stderr.contains("no live memory"), "{stderr}");
}

/// `embedmind serve` speaks MCP over stdio — the exact integration the
/// README promises (`claude mcp add embedmind -- embedmind serve`).
#[test]
fn serve_speaks_mcp_over_stdio() {
    let scratch = Scratch::new("serve");
    let store = scratch.store();

    let mut child = Command::new(env!("CARGO_BIN_EXE_embedmind"))
        .args(["--file", store.to_str().unwrap(), "serve"])
        .current_dir(scratch.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve must spawn");

    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin
            .write_all(
                concat!(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli-test","version":"0"}}}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"content":"memory via mcp serve"}}}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall","arguments":{"query":"memory via serve","scope":"all"}}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
    }
    drop(child.stdin.take()); // EOF ends the serve loop cleanly

    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "serve must exit 0 on EOF: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "embedmind");
    let id = responses[1]["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap();
    let hits = responses[2]["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap();
    assert!(
        hits.iter().any(|h| h["id"] == id),
        "served memory must be recallable"
    );
}

/// S19 end to end: `remember --supersedes ID` hides the old version from
/// recall, keeps it navigable as history via `related`, and rejects bad
/// targets with a clear error.
#[test]
fn supersedes_flow_recall_hides_history_stays_navigable() {
    let scratch = Scratch::new("supersedes");
    let store = scratch.store();
    let file = store.to_str().unwrap();

    let (ok, stdout, stderr) = run(
        scratch.path(),
        &["--file", file, "remember", "the launch date is august 4th"],
    );
    assert!(ok, "remember v1 failed: {stderr}");
    let old_id = stdout.split_whitespace().next().unwrap().to_string();

    let (ok, stdout, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "remember",
            "the launch date moved to august 11th",
            "--supersedes",
            &old_id,
        ],
    );
    assert!(ok, "remember v2 failed: {stderr}");
    let new_id = stdout.split_whitespace().next().unwrap().to_string();
    assert!(
        stdout.contains(&format!("supersedes: {old_id}")),
        "supersedes echoed: {stdout}"
    );

    // Only the new version recalls.
    let (ok, stdout, _) = run(
        scratch.path(),
        &["--file", file, "recall", "when is the launch?"],
    );
    assert!(ok);
    assert!(stdout.contains(&new_id), "new version recalls: {stdout}");
    assert!(!stdout.contains(&old_id), "old version hidden: {stdout}");

    // The chain is navigable both ways; the old version is marked history.
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "related", &new_id]);
    assert!(ok);
    assert!(stdout.contains("supersedes"), "{stdout}");
    assert!(stdout.contains(&old_id), "{stdout}");
    assert!(stdout.contains("[superseded]"), "history marked: {stdout}");
    let (ok, stdout, _) = run(scratch.path(), &["--file", file, "related", &old_id]);
    assert!(ok, "related on a superseded memory must work (history)");
    assert!(stdout.contains(&new_id), "{stdout}");

    // Bad targets: malformed id is a CLI parse error; a valid-but-unknown id
    // fails in the engine. Both exit non-zero with a clear message.
    let (ok, _, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "remember",
            "x",
            "--supersedes",
            "not-a-ulid",
        ],
    );
    assert!(!ok);
    assert!(stderr.contains("not a memory id"), "{stderr}");
    let (ok, _, stderr) = run(
        scratch.path(),
        &[
            "--file",
            file,
            "remember",
            "x",
            "--supersedes",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ],
    );
    assert!(!ok);
    assert!(
        stderr.contains("does not exist or was forgotten"),
        "{stderr}"
    );
}
