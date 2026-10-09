//! Shared test helpers for git modules (compiled only for tests).
//!
//! Builds real local git repositories that tests query through `file://`
//! URLs — exercising the same transport code paths as a network remote,
//! with zero network access.

use std::process::Command;

use tempfile::TempDir;

/// Run a git command in `dir` with deterministic identity/dates and
/// signing disabled; panics with stderr on failure.
pub(crate) fn git(dir: &std::path::Path, args: &[&str], date: &str) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.email=test@test",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_DATE", date)
        .current_dir(dir)
        .output()
        .expect("git must be runnable in tests");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a git repo with dated commits/tags for remote-resolution tests.
///
/// Layout:
/// - commit c1 (2024-01) tagged `v1.0.0` (lightweight)
/// - commit c2 (2024-06) tagged `v2.0.0-beta1` (annotated, 2024-06-02)
/// - commit c3 (2025-01) tagged `v2.0.0` (annotated, 2025-01-02)
/// - branches `main` (default) and `feature/x`
pub(crate) fn make_remote_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let path = dir.path();

    git(
        path,
        &["init", "--quiet", "--initial-branch=main"],
        "2024-01-01T10:00:00",
    );
    std::fs::write(path.join("f"), "one").unwrap();
    git(path, &["add", "f"], "2024-01-01T10:00:00");
    git(path, &["commit", "-qm", "c1"], "2024-01-01T10:00:00");
    git(path, &["tag", "v1.0.0"], "2024-01-01T10:00:00");

    std::fs::write(path.join("f"), "two").unwrap();
    git(path, &["add", "f"], "2024-06-01T10:00:00");
    git(path, &["commit", "-qm", "c2"], "2024-06-01T10:00:00");
    git(
        path,
        &["tag", "-a", "v2.0.0-beta1", "-m", "beta"],
        "2024-06-02T10:00:00",
    );

    std::fs::write(path.join("f"), "three").unwrap();
    git(path, &["add", "f"], "2025-01-01T10:00:00");
    git(path, &["commit", "-qm", "c3"], "2025-01-01T10:00:00");
    git(
        path,
        &["tag", "-a", "v2.0.0", "-m", "stable"],
        "2025-01-02T10:00:00",
    );

    git(path, &["branch", "feature/x"], "2025-01-02T10:00:00");

    dir
}

/// `file://` URL for a local repo — forces the real git transport, exactly
/// like talking to a network remote.
pub(crate) fn file_url(dir: &TempDir) -> String {
    format!("file://{}", dir.path().display())
}

/// Run a git command in `dir` and return its trimmed stdout; panics with
/// stderr on failure. For reading state (`rev-parse`, `remote get-url`)
/// that tests assert against.
pub(crate) fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be runnable in tests");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Commit a change to `f` in `dir` so the repo gains a new tip, returning the
/// new commit's full SHA. Used to make an "origin" move ahead of a clone.
pub(crate) fn commit_change(dir: &std::path::Path, contents: &str, date: &str) -> String {
    std::fs::write(dir.join("f"), contents).unwrap();
    git(dir, &["add", "f"], date);
    git(dir, &["commit", "-qm", contents], date);
    git_stdout(dir, &["rev-parse", "HEAD"])
}

/// Build a git repo whose tags are exactly `tags` (in creation order, one
/// dated commit each, lightweight). An empty slice yields a repo with one
/// commit and no tags. Used to probe tag-keyword edge cases.
pub(crate) fn make_repo_with_tags(tags: &[&str]) -> TempDir {
    assert!(tags.len() <= 11, "one tag per month of 2024 (Feb..Dec)");
    let dir = TempDir::new().unwrap();
    let path = dir.path();
    git(
        path,
        &["init", "--quiet", "--initial-branch=main"],
        "2024-01-01T10:00:00",
    );
    commit_change(path, "base", "2024-01-01T10:00:00");
    for (i, tag) in tags.iter().enumerate() {
        // One month apart so `--sort=-creatordate` order is unambiguous.
        let date = format!("2024-{:02}-01T10:00:00", i + 2);
        commit_change(path, tag, &date);
        git(path, &["tag", tag], &date);
    }
    dir
}
