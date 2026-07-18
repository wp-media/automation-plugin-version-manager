//! Claude Code skill management: `apvm skill <action>`.
//!
//! APVM ships a [Claude Code skill](https://code.claude.com/docs/en/skills)
//! (`apvm-cli`) that teaches Claude the full CLI surface. The skill files are
//! **embedded in the binary at compile time** (see [`embedded`]), so
//! installing them is fully self-contained — no network access, and the
//! installed skill always matches the binary version exactly.
//!
//! `apvm skill install` writes the skill where Claude Code looks for it, and
//! `apvm skill uninstall` removes exactly that directory again (never its
//! parents or sibling skills):
//!
//! - **Project** (default): `./.claude/skills/apvm-cli/`
//! - **Global** (`-g`/`--global`): `~/.claude/skills/apvm-cli/`

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use apvm_core::error::{Error, Result};

mod embedded;
mod fs_ops;

use fs_ops::SKILL_NAME;

pub(crate) use fs_ops::UninstallOutcome;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Current version of this binary, set at compile time.
///
/// Source: <https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates>
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ─────────────────────────────────────────────────────────────────────────────
// ANSI color helpers (same style as update.rs / uninstall.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Green check mark for success messages.
fn success(msg: &str) {
    eprintln!("  \x1b[32m✓\x1b[0m  {msg}");
}

/// Blue info prefix.
fn info(msg: &str) {
    eprintln!("\x1b[34minfo\x1b[0m  {msg}");
}

/// Yellow warn prefix.
fn warn(msg: &str) {
    eprintln!("\x1b[33mwarn\x1b[0m  {msg}");
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI definition
// ─────────────────────────────────────────────────────────────────────────────

/// Arguments for the `skill` command.
#[derive(Args, Debug)]
pub struct SkillArgs {
    /// Skill action.
    #[command(subcommand)]
    pub action: SkillAction,
}

/// Skill subcommands.
#[derive(Subcommand, Debug)]
pub enum SkillAction {
    /// Install the apvm Claude Code skill for the current project (or globally)
    Install {
        /// Install for all projects (~/.claude/skills) instead of the
        /// current directory (./.claude/skills)
        #[arg(short = 'g', long = "global")]
        global: bool,
    },
    /// Remove the apvm Claude Code skill from the current project (or globally)
    Uninstall {
        /// Remove the all-projects installation (~/.claude/skills) instead
        /// of the current directory's (./.claude/skills)
        #[arg(short = 'g', long = "global")]
        global: bool,
    },
}

impl SkillArgs {
    /// Execute the skill command.
    ///
    /// # Arguments
    ///
    /// * `verbose` - Global `--verbose` flag: print the destination and
    ///   per-file detail.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Skill`] for skill-specific failures (unresolvable
    /// home/current directory, filesystem errors while writing).
    pub fn execute(&self, verbose: bool) -> Result<()> {
        match self.action {
            SkillAction::Install { global } => install(global, verbose),
            SkillAction::Uninstall { global } => uninstall(global, verbose),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// install
// ─────────────────────────────────────────────────────────────────────────────

/// Execute `apvm skill install`.
///
/// # Steps
///
/// 1. Resolve the destination (`./.claude/skills/apvm-cli` or
///    `~/.claude/skills/apvm-cli`), creating the full path as needed.
/// 2. For local installs, warn (without aborting) when the current directory
///    is not inside a git repository.
/// 3. Write the embedded skill files, replacing any existing installation.
fn install(global: bool, verbose: bool) -> Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::Skill(format!("Could not determine the current directory: {e}")))?;
    let home = resolve_home_dir()?;
    let dest = fs_ops::resolve_dest_dir(global, &home, &cwd);

    let scope = if global {
        "globally"
    } else {
        "for this project"
    };
    info(&format!(
        "Installing the Claude Code skill '{SKILL_NAME}' {scope}..."
    ));
    if verbose {
        info(&format!("Destination: {}", dest.display()));
        for file in embedded::SKILL_FILES {
            info(&format!(
                "Embedded file: {} ({} bytes)",
                file.rel_path,
                file.contents.len()
            ));
        }
    }

    // Informational only — a missing repository never cancels the install.
    if !global && fs_ops::find_git_root(&cwd).is_none() {
        warn(&format!(
            "{} does not appear to be inside a git repository — \
             installing the project-local skill anyway.",
            cwd.display()
        ));
    }

    fs_ops::install_skill_files(&dest, embedded::SKILL_FILES)?;

    success(&format!(
        "Installed {} file(s) to {} (bundled with apvm {CURRENT_VERSION})",
        embedded::SKILL_FILES.len(),
        dest.display()
    ));
    eprintln!();
    info("Claude Code picks up skills from this directory automatically.");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// uninstall
// ─────────────────────────────────────────────────────────────────────────────

/// Execute `apvm skill uninstall`.
///
/// Removes `./.claude/skills/apvm-cli` (or `~/.claude/skills/apvm-cli` with
/// `--global`) and nothing else — parent directories and other skills are
/// left untouched. Running it when the skill is not installed is a no-op
/// that still succeeds (the desired state is already reached).
fn uninstall(global: bool, verbose: bool) -> Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::Skill(format!("Could not determine the current directory: {e}")))?;
    let home = resolve_home_dir()?;
    let dest = fs_ops::resolve_dest_dir(global, &home, &cwd);

    let scope = if global {
        "globally"
    } else {
        "for this project"
    };
    info(&format!(
        "Removing the Claude Code skill '{SKILL_NAME}' {scope}..."
    ));
    if verbose {
        info(&format!("Destination: {}", dest.display()));
    }

    match fs_ops::uninstall_skill_dir(&dest)? {
        fs_ops::UninstallOutcome::Removed => {
            success(&format!("Removed {}", dest.display()));
        }
        fs_ops::UninstallOutcome::NotInstalled => {
            info(&format!(
                "The skill is not installed at {} — nothing to do.",
                dest.display()
            ));
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Hooks for `apvm uninstall` (binary self-uninstall)
// ─────────────────────────────────────────────────────────────────────────────

/// Detect a **global** skill installation (`~/.claude/skills/apvm-cli`).
///
/// Returns the path when *any* entry exists there (directory, stray file, or
/// symlink — lstat-based, so dangling symlinks count too), letting the
/// caller show it in a removal plan; [`remove_installation`] then applies
/// the strict safety checks. Returns `None` when nothing exists or the home
/// directory cannot be resolved.
pub(crate) fn detect_global_installation() -> Option<PathBuf> {
    let home = resolve_home_dir().ok()?;
    detect_installation_in(&home)
}

/// [`detect_global_installation`] with an explicit home directory
/// (separated for unit testing).
fn detect_installation_in(home: &Path) -> Option<PathBuf> {
    let dir = fs_ops::resolve_dest_dir(true, home, Path::new(""));
    std::fs::symlink_metadata(&dir).is_ok().then_some(dir)
}

/// Remove the skill installation at `dir` (a path previously returned by
/// [`detect_global_installation`]), with all of
/// [`fs_ops::uninstall_skill_dir`]'s safety guards.
pub(crate) fn remove_installation(dir: &Path) -> Result<UninstallOutcome> {
    fs_ops::uninstall_skill_dir(dir)
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the user's home directory (for the global skill location).
///
/// # Errors
///
/// Returns [`Error::Skill`] when no home directory can be determined —
/// unlike [`crate::defaults`], this must not panic, because the failure is
/// reachable from a user-facing command.
fn resolve_home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| Error::Skill("Could not determine the home directory".to_string()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Minimal parser harness so the clap wiring can be tested without the
    /// full `Cli` struct (which lives in main.rs).
    #[derive(Parser, Debug)]
    struct Harness {
        #[command(flatten)]
        args: SkillArgs,
    }

    #[test]
    fn parses_install_without_flags() {
        let h = Harness::try_parse_from(["skill", "install"]).unwrap();
        assert!(matches!(
            h.args.action,
            SkillAction::Install { global: false }
        ));
    }

    #[test]
    fn parses_install_with_short_global_flag() {
        let h = Harness::try_parse_from(["skill", "install", "-g"]).unwrap();
        assert!(matches!(
            h.args.action,
            SkillAction::Install { global: true }
        ));
    }

    #[test]
    fn parses_install_with_long_global_flag() {
        let h = Harness::try_parse_from(["skill", "install", "--global"]).unwrap();
        assert!(matches!(
            h.args.action,
            SkillAction::Install { global: true }
        ));
    }

    #[test]
    fn parses_uninstall_without_flags() {
        let h = Harness::try_parse_from(["skill", "uninstall"]).unwrap();
        assert!(matches!(
            h.args.action,
            SkillAction::Uninstall { global: false }
        ));
    }

    #[test]
    fn parses_uninstall_with_short_global_flag() {
        let h = Harness::try_parse_from(["skill", "uninstall", "-g"]).unwrap();
        assert!(matches!(
            h.args.action,
            SkillAction::Uninstall { global: true }
        ));
    }

    #[test]
    fn parses_uninstall_with_long_global_flag() {
        let h = Harness::try_parse_from(["skill", "uninstall", "--global"]).unwrap();
        assert!(matches!(
            h.args.action,
            SkillAction::Uninstall { global: true }
        ));
    }

    #[test]
    fn rejects_unknown_action() {
        assert!(Harness::try_parse_from(["skill", "frobnicate"]).is_err());
    }

    #[test]
    fn rejects_removed_vendor_action() {
        // `vendor` existed only during development of this feature; the
        // embedded design removed it. Guard against accidental resurrection
        // in help/dispatch without a deliberate decision.
        assert!(Harness::try_parse_from(["skill", "vendor"]).is_err());
    }

    #[test]
    fn current_version_is_valid_semver() {
        assert!(semver::Version::parse(CURRENT_VERSION).is_ok());
    }

    // ── detect_installation_in ───────────────────────────────────────────

    #[test]
    fn detect_installation_finds_installed_skill() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/skills/apvm-cli");
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(detect_installation_in(home.path()), Some(dir));
    }

    #[test]
    fn detect_installation_none_when_absent() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(detect_installation_in(home.path()), None);
    }

    #[test]
    fn detect_installation_reports_stray_entries() {
        // Even a non-directory entry is reported, so the uninstall plan can
        // surface it; remove_installation then refuses it with a manual hint.
        let home = tempfile::tempdir().unwrap();
        let skills = home.path().join(".claude/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("apvm-cli"), "stray").unwrap();

        assert!(detect_installation_in(home.path()).is_some());
    }
}
