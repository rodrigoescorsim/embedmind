//! Project-context detection (M1 item 1.5, DESIGN §7): infer which project
//! the agent is working in from its working directory, so `remember` stamps
//! it and `recall` scopes to it automatically.
//!
//! Walking up from the starting directory, the **nearest** marker wins:
//!
//! 1. `.embedmind.toml` with a top-level `project = "name"` key — the
//!    explicit override (monorepos, or a directory name that is not the
//!    project's name).
//! 2. A `.git` entry (directory, or file — worktrees/submodules): the
//!    repository root's directory name is the project name.
//!
//! No marker up to the filesystem root = no project context; memories are
//! global and recall searches everything.
//!
//! This lives in the shell, not the engine: "which project is the agent in"
//! is an environment question (cwd, config files), and the engine must stay
//! free of environment concerns (CLAUDE.md decision 2).

use std::path::Path;

/// Detects the project for `start` (the agent's cwd). See module docs for
/// the rules. I/O errors while probing are treated as "marker absent" — an
/// unreadable directory must degrade to global scope, never fail startup.
pub fn detect_project(start: &Path) -> Option<String> {
    for dir in start.ancestors() {
        let config = dir.join(".embedmind.toml");
        if config.is_file()
            && let Ok(text) = std::fs::read_to_string(&config)
            && let Some(name) = parse_project_key(&text)
        {
            return Some(name);
        }
        // `.git` is a directory in a normal checkout, a file in worktrees
        // and submodules — `exists` covers both.
        if dir.join(".git").exists() {
            return dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
        }
    }
    None
}

/// Extracts the top-level `project = "name"` key from `.embedmind.toml`.
///
/// Deliberately minimal: one string key, quoted with `"` or `'`, before any
/// `[table]` section; `#` comments and blank lines are skipped. A full TOML
/// parser (and the dependency it costs — DESIGN §10) arrives only when the
/// config file grows real structure.
fn parse_project_key(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            return None; // top-level section ended
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "project" {
            continue;
        }
        let value = value.trim();
        // Strip an inline comment only if it follows the closing quote.
        for quote in ['"', '\''] {
            if let Some(rest) = value.strip_prefix(quote)
                && let Some(end) = rest.find(quote)
            {
                let name = &rest[..end];
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
        return None; // `project =` present but malformed/empty: no override
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A unique scratch directory under the system temp dir, cleaned on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "embedmind-project-test-{tag}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn git_root_directory_name_is_the_project() {
        let scratch = Scratch::new("git");
        let repo = scratch.path().join("myrepo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(detect_project(&nested).as_deref(), Some("myrepo"));
        assert_eq!(detect_project(&repo).as_deref(), Some("myrepo"));
    }

    #[test]
    fn git_file_worktree_marker_also_counts() {
        let scratch = Scratch::new("worktree");
        let repo = scratch.path().join("wt");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join(".git"), "gitdir: elsewhere\n").unwrap();

        assert_eq!(detect_project(&repo).as_deref(), Some("wt"));
    }

    #[test]
    fn config_overrides_git_and_nearest_marker_wins() {
        let scratch = Scratch::new("config");
        let repo = scratch.path().join("monorepo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = repo.join("services").join("billing");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join(".embedmind.toml"),
            "# per-service scope\nproject = \"billing-svc\"\n",
        )
        .unwrap();

        // Inside the subproject: its config wins over the repo root.
        assert_eq!(detect_project(&sub).as_deref(), Some("billing-svc"));
        // At the repo root: no config there, git root name applies.
        assert_eq!(detect_project(&repo).as_deref(), Some("monorepo"));
    }

    #[test]
    fn config_without_project_key_falls_through() {
        let scratch = Scratch::new("nokey");
        let repo = scratch.path().join("fallthrough");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".embedmind.toml"), "# no project key here\n").unwrap();

        assert_eq!(detect_project(&repo).as_deref(), Some("fallthrough"));
    }

    #[test]
    fn no_marker_means_no_project() {
        let scratch = Scratch::new("bare");
        let dir = scratch.path().join("just-a-dir");
        fs::create_dir_all(&dir).unwrap();
        // The scratch dir sits under the system temp dir; no ancestor should
        // carry a marker. If one ever does, this test environment is broken
        // in a way worth knowing about.
        assert_eq!(detect_project(&dir), None);
    }

    #[test]
    fn toml_parsing_is_tolerant_but_strict_about_shape() {
        assert_eq!(
            parse_project_key("project = \"alpha\""),
            Some("alpha".to_string())
        );
        assert_eq!(
            parse_project_key("  project='beta'  # comment"),
            Some("beta".to_string())
        );
        assert_eq!(
            parse_project_key("# header\n\nother = 1\nproject = \"gamma\"\n"),
            Some("gamma".to_string())
        );
        // Key inside a table is not a top-level override.
        assert_eq!(parse_project_key("[section]\nproject = \"nope\""), None);
        // Malformed values: no override.
        assert_eq!(parse_project_key("project = unquoted"), None);
        assert_eq!(parse_project_key("project = \"\""), None);
        assert_eq!(parse_project_key(""), None);
    }
}
