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
