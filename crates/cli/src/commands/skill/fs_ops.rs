//! Filesystem operations for the `apvm skill` command.
//!
//! All functions here are pure with respect to their path arguments (no
//! hardcoded locations), so every code path is unit-testable against
//! temporary directories.

use std::fs;
use std::path::{Component, Path, PathBuf};

use apvm_core::error::{Error, Result};

use super::embedded::EmbeddedFile;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Name of the skill (and of the directory it is installed into).
pub const SKILL_NAME: &str = "apvm-cli";

/// The one file every valid Claude Code skill must contain.
///
/// Source: <https://code.claude.com/docs/en/skills> — "The `SKILL.md` contains
/// the main instructions and is required. Other files are optional."
pub const SKILL_MANIFEST: &str = "SKILL.md";

// ─────────────────────────────────────────────────────────────────────────────
// Path helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the destination skill directory for an install.
///
/// - Global: `<home>/.claude/skills/apvm-cli` (all projects)
/// - Local: `<cwd>/.claude/skills/apvm-cli` (this project only)
///
/// Both layouts are defined by the Claude Code documentation:
/// <https://code.claude.com/docs/en/skills> — "Personal:
/// `~/.claude/skills/<skill-name>/SKILL.md`; Project:
/// `.claude/skills/<skill-name>/SKILL.md`".
pub fn resolve_dest_dir(global: bool, home: &Path, cwd: &Path) -> PathBuf {
    let base = if global { home } else { cwd };
    base.join(".claude").join("skills").join(SKILL_NAME)
}

/// Validate a path that will be joined under a destination directory.
///
/// Rejects anything that could escape the destination: absolute paths, empty
/// paths, and any non-normal component (`..`, `.`, root, or Windows prefix).
/// The embedded file list is compile-time data, so this is defense in depth
/// against a bad edit to that list (a unit test also checks it).
///
/// # Errors
///
/// Returns [`Error::Skill`] when the path is empty, absolute, or contains a
/// non-normal component.
pub fn validate_rel_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(Error::Skill("Skill file has an empty path".to_string()));
    }
    if path.is_absolute() {
        return Err(Error::Skill(format!(
            "Skill file path must be relative, got: {}",
            path.display()
        )));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::Skill(format!(
                "Skill file path contains an unsafe component ('{}'): {}",
                component.as_os_str().to_string_lossy(),
                path.display()
            )));
        }
    }
    Ok(())
}

/// Walk ancestors of `start` (including `start` itself) looking for a git
/// repository root — a directory containing `.git`.
///
/// `.git` may be a directory (normal repository) or a file (worktrees and
/// submodules use a `gitdir:` pointer file), so plain existence is checked.
/// Source: <https://git-scm.com/docs/gitrepository-layout>
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Guard shared by every operation that deletes `dest`: refuse any path that
/// does not end in the expected skill directory name, so a caller bug can
/// never point a recursive delete at an arbitrary directory.
fn ensure_skill_dir_name(dest: &Path, action: &str) -> Result<()> {
    if dest.file_name().map(|n| n.to_string_lossy().into_owned()) != Some(SKILL_NAME.to_string()) {
        return Err(Error::Skill(format!(
            "Refusing to {action} {}: expected a directory named '{SKILL_NAME}'",
            dest.display()
        )));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Installing the skill
// ─────────────────────────────────────────────────────────────────────────────

/// Install `files` into `dest` (the `.../skills/apvm-cli` directory),
/// replacing any existing installation.
///
/// The write is staged: files are first written to a sibling staging
/// directory, then the old installation (if any) is removed and the staging
/// directory is renamed into place. A failure mid-write therefore never
/// leaves a half-written skill at `dest` (the previous copy is only removed
/// once the new one is fully staged).
///
/// # Errors
///
/// Returns [`Error::Skill`] when:
/// - `dest` does not end in the expected skill directory name (guard against
///   caller bugs, since the existing `dest` is deleted),
/// - `files` is empty or missing `SKILL.md`,
/// - any relative path fails [`validate_rel_path`],
/// - any filesystem operation fails.
pub fn install_skill_files(dest: &Path, files: &[EmbeddedFile]) -> Result<()> {
    // Guard: this function deletes `dest`, so refuse anything that is not a
    // skill directory of the expected name.
    ensure_skill_dir_name(dest, "install into")?;
    if files.is_empty() {
        return Err(Error::Skill(
            "No skill files to install (empty file set)".to_string(),
        ));
    }
    if !files.iter().any(|f| f.rel_path == SKILL_MANIFEST) {
        return Err(Error::Skill(format!(
            "Skill file set is missing its manifest ({SKILL_MANIFEST}) — refusing to install"
        )));
    }
    for file in files {
        validate_rel_path(Path::new(file.rel_path))?;
    }

    let parent = dest.parent().ok_or_else(|| {
        Error::Skill(format!(
            "Destination {} has no parent directory",
            dest.display()
        ))
    })?;
    fs::create_dir_all(parent)
        .map_err(|e| Error::Skill(format!("Failed to create {}: {e}", parent.display())))?;

    let staging = parent.join(format!(".{SKILL_NAME}.staging"));
    let result = stage_and_swap(&staging, dest, files);
    if result.is_err() {
        // Best-effort cleanup of the staging directory on failure.
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// Write `files` into `staging`, then swap `staging` into place at `dest`.
fn stage_and_swap(staging: &Path, dest: &Path, files: &[EmbeddedFile]) -> Result<()> {
    if staging.exists() {
        fs::remove_dir_all(staging).map_err(|e| {
            Error::Skill(format!(
                "Failed to remove leftover staging directory {}: {e}",
                staging.display()
            ))
        })?;
    }

    for file in files {
        let target = staging.join(file.rel_path);
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| Error::Skill(format!("Failed to create {}: {e}", dir.display())))?;
        }
        fs::write(&target, file.contents)
            .map_err(|e| Error::Skill(format!("Failed to write {}: {e}", target.display())))?;
    }

    // Remove the previous installation only after the new one is fully staged.
    if dest.is_dir() {
        fs::remove_dir_all(dest).map_err(|e| {
            Error::Skill(format!(
                "Failed to remove the existing skill at {}: {e}",
                dest.display()
            ))
        })?;
    } else if dest.exists() {
        // `dest` exists but is not a directory (unexpected, e.g. a stray file).
        fs::remove_file(dest).map_err(|e| {
            Error::Skill(format!(
                "Failed to remove the existing file at {}: {e}",
                dest.display()
            ))
        })?;
    }

    fs::rename(staging, dest).map_err(|e| {
        Error::Skill(format!(
            "Failed to move the staged skill into {}: {e}",
            dest.display()
        ))
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Uninstalling the skill
// ─────────────────────────────────────────────────────────────────────────────

/// Result of an uninstall attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// The skill directory existed and was removed.
    Removed,
    /// Nothing exists at the destination — already uninstalled.
    NotInstalled,
}

/// Remove the skill installation at `dest` (the `.../skills/apvm-cli`
/// directory), and **only** it — parent directories (`skills/`, `.claude/`)
/// and sibling skills are never touched, even when they end up empty.
///
/// Deletion is held to stricter checks than replacement: the path must be a
/// real directory (not a symlink, not a stray file) that actually looks like
/// an installed skill (contains `SKILL.md`). Anything else is refused with a
/// pointer to remove it manually, so a recursive delete can never hit
/// unexpected content.
///
/// # Returns
///
/// - [`UninstallOutcome::Removed`] — the installation was deleted
/// - [`UninstallOutcome::NotInstalled`] — nothing exists at `dest`
///   (idempotent: the desired state is already reached)
///
/// # Errors
///
/// Returns [`Error::Skill`] when:
/// - `dest` does not end in the expected skill directory name (guard against
///   caller bugs),
/// - `dest` is a symlink or not a directory,
/// - `dest` is a directory without a `SKILL.md`,
/// - the removal itself fails.
pub fn uninstall_skill_dir(dest: &Path) -> Result<UninstallOutcome> {
    ensure_skill_dir_name(dest, "remove")?;

    // symlink_metadata (lstat) sees the entry itself: unlike `exists()` it
    // reports dangling symlinks, and unlike `metadata()` it does not follow
    // links — so a symlinked `dest` is detected instead of traversed.
    // Source: https://doc.rust-lang.org/std/fs/fn.symlink_metadata.html
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UninstallOutcome::NotInstalled);
        }
        Err(e) => {
            return Err(Error::Skill(format!(
                "Failed to inspect {}: {e}",
                dest.display()
            )));
        }
    };

    if meta.file_type().is_symlink() {
        return Err(Error::Skill(format!(
            "{} is a symbolic link — refusing to remove it. \
             Delete it manually if it should go away.",
            dest.display()
        )));
    }
    if !meta.is_dir() {
        return Err(Error::Skill(format!(
            "{} exists but is not a directory — refusing to remove it. \
             Delete it manually if it should go away.",
            dest.display()
        )));
    }
    if !dest.join(SKILL_MANIFEST).is_file() {
        return Err(Error::Skill(format!(
            "{} does not look like an installed skill (no {SKILL_MANIFEST}) — \
             refusing to remove it. Delete it manually if it should go away.",
            dest.display()
        )));
    }

    fs::remove_dir_all(dest)
        .map_err(|e| Error::Skill(format!("Failed to remove {}: {e}", dest.display())))?;
    Ok(UninstallOutcome::Removed)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A small skill file set with a manifest and one nested reference.
    const SAMPLE_FILES: &[EmbeddedFile] = &[
        EmbeddedFile {
            rel_path: "SKILL.md",
            contents: "---\nname: apvm-cli\n---\nbody",
        },
        EmbeddedFile {
            rel_path: "references/git-refs.md",
            contents: "# refs",
        },
    ];

    // ── resolve_dest_dir ─────────────────────────────────────────────────

    #[test]
    fn resolve_dest_dir_global_uses_home() {
        let dest = resolve_dest_dir(true, Path::new("/home/u"), Path::new("/proj"));
        assert_eq!(dest, PathBuf::from("/home/u/.claude/skills/apvm-cli"));
    }

    #[test]
    fn resolve_dest_dir_local_uses_cwd() {
        let dest = resolve_dest_dir(false, Path::new("/home/u"), Path::new("/proj"));
        assert_eq!(dest, PathBuf::from("/proj/.claude/skills/apvm-cli"));
    }

    // ── validate_rel_path ────────────────────────────────────────────────

    #[test]
    fn validate_rel_path_accepts_plain_file() {
        assert!(validate_rel_path(Path::new("SKILL.md")).is_ok());
    }

    #[test]
    fn validate_rel_path_accepts_nested_file() {
        assert!(validate_rel_path(Path::new("references/git-refs.md")).is_ok());
    }

    #[test]
    fn validate_rel_path_rejects_empty() {
        assert!(validate_rel_path(Path::new("")).is_err());
    }

    #[test]
    fn validate_rel_path_rejects_absolute() {
        #[cfg(unix)]
        assert!(validate_rel_path(Path::new("/etc/passwd")).is_err());
        #[cfg(windows)]
        assert!(validate_rel_path(Path::new("C:\\Windows\\a.md")).is_err());
    }

    #[test]
    fn validate_rel_path_rejects_parent_traversal() {
        assert!(validate_rel_path(Path::new("../escape.md")).is_err());
        assert!(validate_rel_path(Path::new("references/../../escape.md")).is_err());
    }

    #[test]
    fn validate_rel_path_rejects_current_dir_component() {
        assert!(validate_rel_path(Path::new("./SKILL.md")).is_err());
    }

    // ── find_git_root ────────────────────────────────────────────────────

    #[test]
    fn find_git_root_detects_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let found = find_git_root(&nested).unwrap();
        assert_eq!(found, repo);
    }

    #[test]
    fn find_git_root_detects_git_file() {
        // Worktrees and submodules use a `.git` *file* with a gitdir pointer.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("worktree");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: /elsewhere").unwrap();

        let found = find_git_root(&repo).unwrap();
        assert_eq!(found, repo);
    }

    #[test]
    fn find_git_root_none_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plain");
        std::fs::create_dir_all(&dir).unwrap();
        // The temp dir itself lives outside any repo; system temp roots are
        // never git repositories.
        assert!(find_git_root(&dir).is_none());
    }

    // ── install_skill_files ──────────────────────────────────────────────

    #[test]
    fn install_creates_full_path_and_writes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(".claude/skills").join(SKILL_NAME);

        install_skill_files(&dest, SAMPLE_FILES).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join(SKILL_MANIFEST)).unwrap(),
            SAMPLE_FILES[0].contents
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("references/git-refs.md")).unwrap(),
            "# refs"
        );
    }

    #[test]
    fn install_replaces_existing_installation() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("skills").join(SKILL_NAME);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.md"), "old").unwrap();
        std::fs::write(dest.join(SKILL_MANIFEST), "old manifest").unwrap();

        install_skill_files(&dest, SAMPLE_FILES).unwrap();

        // Stale files are gone, new content is in place.
        assert!(!dest.join("stale.md").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join(SKILL_MANIFEST)).unwrap(),
            SAMPLE_FILES[0].contents
        );
    }

    #[test]
    fn install_removes_leftover_staging_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("skills").join(SKILL_NAME);
        let staging = tmp.path().join("skills").join(".apvm-cli.staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("leftover"), "x").unwrap();

        install_skill_files(&dest, SAMPLE_FILES).unwrap();

        assert!(!staging.exists());
        assert!(dest.join(SKILL_MANIFEST).exists());
    }

    #[test]
    fn install_rejects_unexpected_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("not-the-skill");
        let result = install_skill_files(&dest, SAMPLE_FILES);
        assert!(result.is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn install_rejects_empty_file_set() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(SKILL_NAME);
        assert!(install_skill_files(&dest, &[]).is_err());
    }

    #[test]
    fn install_rejects_missing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(SKILL_NAME);
        let files = &[EmbeddedFile {
            rel_path: "references/git-refs.md",
            contents: "# refs",
        }];
        assert!(install_skill_files(&dest, files).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn install_rejects_traversal_paths_and_leaves_no_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("inner").join(SKILL_NAME);
        let files = &[
            SAMPLE_FILES[0],
            SAMPLE_FILES[1],
            EmbeddedFile {
                rel_path: "../escape.md",
                contents: "evil",
            },
        ];

        assert!(install_skill_files(&dest, files).is_err());
        assert!(!dest.exists());
        assert!(!tmp.path().join("inner/escape.md").exists());
        assert!(!tmp.path().join("escape.md").exists());
    }

    #[test]
    fn install_replaces_stray_file_at_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let dest = skills.join(SKILL_NAME);
        std::fs::write(&dest, "a stray file, not a directory").unwrap();

        install_skill_files(&dest, SAMPLE_FILES).unwrap();
        assert!(dest.is_dir());
        assert!(dest.join(SKILL_MANIFEST).exists());
    }

    // ── uninstall_skill_dir ──────────────────────────────────────────────

    /// Create an installed skill at `<root>/.claude/skills/apvm-cli` and
    /// return its path.
    fn install_sample(root: &Path) -> PathBuf {
        let dest = root.join(".claude/skills").join(SKILL_NAME);
        install_skill_files(&dest, SAMPLE_FILES).unwrap();
        dest
    }

    #[test]
    fn uninstall_removes_installed_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = install_sample(tmp.path());

        let outcome = uninstall_skill_dir(&dest).unwrap();

        assert_eq!(outcome, UninstallOutcome::Removed);
        assert!(!dest.exists());
    }

    #[test]
    fn uninstall_keeps_parent_directories() {
        // Only the skill goes — `.claude/` and `skills/` stay, even empty.
        let tmp = tempfile::tempdir().unwrap();
        let dest = install_sample(tmp.path());

        uninstall_skill_dir(&dest).unwrap();

        assert!(tmp.path().join(".claude/skills").is_dir());
        assert!(tmp.path().join(".claude").is_dir());
    }

    #[test]
    fn uninstall_keeps_sibling_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = install_sample(tmp.path());
        let sibling = tmp.path().join(".claude/skills/other-skill");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("SKILL.md"), "other").unwrap();

        uninstall_skill_dir(&dest).unwrap();

        assert!(!dest.exists());
        assert!(sibling.join("SKILL.md").is_file());
    }

    #[test]
    fn uninstall_reports_not_installed_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(".claude/skills").join(SKILL_NAME);

        let outcome = uninstall_skill_dir(&dest).unwrap();

        assert_eq!(outcome, UninstallOutcome::NotInstalled);
    }

    #[test]
    fn uninstall_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = install_sample(tmp.path());

        assert_eq!(
            uninstall_skill_dir(&dest).unwrap(),
            UninstallOutcome::Removed
        );
        assert_eq!(
            uninstall_skill_dir(&dest).unwrap(),
            UninstallOutcome::NotInstalled
        );
    }

    #[test]
    fn uninstall_rejects_unexpected_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("not-the-skill");
        std::fs::create_dir_all(&dest).unwrap();

        assert!(uninstall_skill_dir(&dest).is_err());
        assert!(dest.exists());
    }

    #[test]
    fn uninstall_refuses_stray_file() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let dest = skills.join(SKILL_NAME);
        std::fs::write(&dest, "a stray file, not a directory").unwrap();

        let err = uninstall_skill_dir(&dest).unwrap_err().to_string();
        assert!(err.contains("not a directory"), "unexpected error: {err}");
        assert!(dest.exists());
    }

    #[test]
    fn uninstall_refuses_directory_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(SKILL_NAME);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("something-else.md"), "x").unwrap();

        let err = uninstall_skill_dir(&dest).unwrap_err().to_string();
        assert!(
            err.contains("does not look like an installed skill"),
            "unexpected error: {err}"
        );
        assert!(dest.join("something-else.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_refuses_symlinked_destination() {
        // A symlink at the destination must never be followed and deleted
        // through — the link target stays intact.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real-dir");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(SKILL_MANIFEST), "manifest").unwrap();
        let dest = tmp.path().join(SKILL_NAME);
        std::os::unix::fs::symlink(&target, &dest).unwrap();

        let err = uninstall_skill_dir(&dest).unwrap_err().to_string();
        assert!(err.contains("symbolic link"), "unexpected error: {err}");
        assert!(target.join(SKILL_MANIFEST).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_refuses_dangling_symlink() {
        // `exists()` would report false for a dangling link; lstat-based
        // detection still refuses instead of claiming "not installed".
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(SKILL_NAME);
        std::os::unix::fs::symlink(tmp.path().join("gone"), &dest).unwrap();

        let err = uninstall_skill_dir(&dest).unwrap_err().to_string();
        assert!(err.contains("symbolic link"), "unexpected error: {err}");
    }

    #[test]
    fn install_accepts_the_real_embedded_skill() {
        // End-to-end over the actual compile-time data.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(SKILL_NAME);

        install_skill_files(&dest, super::super::embedded::SKILL_FILES).unwrap();

        assert!(dest.join(SKILL_MANIFEST).is_file());
        for file in super::super::embedded::SKILL_FILES {
            assert_eq!(
                std::fs::read_to_string(dest.join(file.rel_path)).unwrap(),
                file.contents
            );
        }
    }
}
