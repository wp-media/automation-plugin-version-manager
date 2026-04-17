//! Self-uninstall command implementation.
//!
//! Removes the APVM binary from disk. On Unix the file is unlinked
//! immediately; on Windows a helper process deletes it after the current
//! process exits.
//!
//! # Cross-platform binary deletion
//!
//! Uses [`self_replace::self_delete_outside_path`] which internally:
//!
//! - **Unix:** Calls `unlink(2)` on the executable.
//!   The kernel keeps the inode alive until all open file descriptors close,
//!   so the running process is unaffected.
//!   Source: <https://man7.org/linux/man-pages/man2/unlink.2.html>
//!
//! - **Windows:** Renames the running `.exe` aside, spawns a helper process
//!   opened with `FILE_FLAG_DELETE_ON_CLOSE`, then exits. The OS deletes
//!   the file once the last handle closes.
//!   Source: <https://docs.rs/self-replace/1/self_replace/#implementation>
//!
//! The `outside_path` variant is used so that no temporary files are placed
//! inside the `bin/` directory, which lets us remove that directory too.
//! Source: <https://docs.rs/self-replace/1.5.0/self_replace/fn.self_delete_outside_path.html>

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use apvm_core::error::{Error, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Current version of this binary, set at compile time.
///
/// Source: <https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates>
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ─────────────────────────────────────────────────────────────────────────────
// ANSI color helpers (same style as update.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Green check mark for success messages.
fn success(msg: &str) {
    eprintln!("  \x1b[32m✓\x1b[0m  {msg}");
}

/// Blue info prefix.
fn info(msg: &str) {
    eprintln!("\x1b[34minfo\x1b[0m  {msg}");
}

/// Dim text.
fn dim(msg: &str) -> String {
    format!("\x1b[2m{msg}\x1b[0m")
}

/// Bold text.
fn bold(msg: &str) -> String {
    format!("\x1b[1m{msg}\x1b[0m")
}

/// Yellow text.
fn yellow(msg: &str) -> String {
    format!("\x1b[33m{msg}\x1b[0m")
}

// ─────────────────────────────────────────────────────────────────────────────
// Binary location
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the path of the currently running executable.
///
/// Returns the full filesystem path. On some platforms this follows symlinks.
///
/// # Sources
///
/// - [`std::env::current_exe`]: returns the full path of the running binary.
///   <https://doc.rust-lang.org/std/env/fn.current_exe.html>
fn resolve_exe_path() -> Result<PathBuf> {
    std::env::current_exe().map_err(|e| {
        Error::Uninstall(format!(
            "Could not determine the path of the running binary: {e}"
        ))
    })
}

/// Derive the `bin/` directory from the executable path.
///
/// The installer places the binary at `~/.apvm/bin/apvm` (or `apvm.exe`),
/// so `parent()` gives us the `bin/` directory.
///
/// # Sources
///
/// - [`std::path::Path::parent`]: returns the parent directory.
///   <https://doc.rust-lang.org/std/path/struct.Path.html#method.parent>
fn resolve_bin_dir(exe_path: &std::path::Path) -> Result<PathBuf> {
    exe_path
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| Error::Uninstall("Binary path has no parent directory".into()))
}

// ─────────────────────────────────────────────────────────────────────────────
// User confirmation
// ─────────────────────────────────────────────────────────────────────────────

/// Prompt the user for yes/no confirmation on stdin.
///
/// Returns `true` if the user types `y` or `yes` (case-insensitive).
/// Returns `false` for anything else (including empty input / just pressing Enter).
///
/// # Sources
///
/// - [`std::io::stdin`]: standard input handle.
///   <https://doc.rust-lang.org/std/io/fn.stdin.html>
/// - [`std::io::BufRead::read_line`]: reads a line from the buffered reader.
///   <https://doc.rust-lang.org/std/io/trait.BufRead.html#method.read_line>
/// - [`std::io::Write::flush`]: ensures the prompt is displayed before blocking.
///   <https://doc.rust-lang.org/std/io/trait.Write.html#tymethod.flush>
fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");

    io::stderr()
        .flush()
        .map_err(|e| Error::Uninstall(format!("Failed to flush stderr: {e}")))?;

    let mut input = String::new();
    io::stdin()
        .lock()
        .read_line(&mut input)
        .map_err(|e| Error::Uninstall(format!("Failed to read user input: {e}")))?;

    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Execute the uninstall command.
///
/// # Arguments
///
/// * `skip_confirm` - If `true`, skip the interactive confirmation prompt
///   (equivalent to the `-y` / `--yes` flag).
///
/// # Steps
///
/// 1. Resolve the path of the running binary
/// 2. Show what will be removed and what will be kept
/// 3. Ask the user for confirmation (unless `--yes`)
/// 4. Delete the binary via `self_replace::self_delete_outside_path`
/// 5. Attempt to remove the now-empty `bin/` directory
/// 6. Print success message
///
/// # Errors
///
/// Returns `Error::Uninstall` for all uninstall-specific failures.
pub fn execute(skip_confirm: bool) -> Result<()> {
    // ── 1. Resolve paths ─────────────────────────────────────────────────
    let exe_path = resolve_exe_path()?;
    let bin_dir = resolve_bin_dir(&exe_path)?;

    // ── 2. Display plan ──────────────────────────────────────────────────
    eprintln!();
    info(&format!(
        "This will uninstall {} {}.",
        bold("apvm"),
        bold(CURRENT_VERSION)
    ));
    eprintln!();

    eprintln!("  The following will be {}:", bold("removed"));
    eprintln!();
    eprintln!("    {}  {}", dim("Binary :"), exe_path.display());
    eprintln!();

    eprintln!(
        "  The following will be {} {}:",
        bold("kept"),
        dim("(harmless, safe to leave)")
    );
    eprintln!();
    eprintln!(
        "    {}  Shell PATH entries in rc files (e.g. ~/.zshrc, ~/.bashrc)",
        dim("PATH   :"),
    );
    eprintln!(
        "    {}  Configuration directory (~/.apvm/)",
        dim("Config :"),
    );
    eprintln!();
    eprintln!(
        "  {}",
        dim("These leftover entries are harmless — shells silently ignore")
    );
    eprintln!(
        "  {}",
        dim("PATH entries that point to non-existent directories.")
    );

    // ── 3. Confirm ───────────────────────────────────────────────────────
    if !skip_confirm {
        eprintln!();
        let confirmed = confirm(&format!(
            "  {} ",
            yellow("? Are you sure you want to uninstall apvm? [y/N]")
        ))?;

        if !confirmed {
            eprintln!();
            info("Uninstall cancelled.");
            return Ok(());
        }
    }

    // ── 4. Delete the binary ─────────────────────────────────────────────
    //
    // self_replace::self_delete_outside_path(bin_dir):
    //   Unix:    calls unlink(2) — immediate deletion; process continues via inode.
    //   Windows: renames exe aside, spawns helper with FILE_FLAG_DELETE_ON_CLOSE.
    //
    // The `outside_path` variant ensures no temp files are created inside
    // `bin_dir`, allowing us to remove that directory afterwards.
    //
    // Source: https://docs.rs/self-replace/1.5.0/self_replace/fn.self_delete_outside_path.html
    eprintln!();
    self_replace::self_delete_outside_path(&bin_dir).map_err(|e| {
        Error::Uninstall(format!(
            "Failed to remove the apvm binary: {e}\n\
             \n\
             This can happen if:\n\
             - The binary is in a read-only location\n\
             - You don't have write permission to {}\n\
             \n\
             Try running with elevated permissions (e.g. sudo).",
            bin_dir.display()
        ))
    })?;

    success("Binary removed successfully.");

    // ── 5. Clean up empty bin/ directory ─────────────────────────────────
    //
    // std::fs::remove_dir removes an *empty* directory only.
    // If the user placed other files inside bin/, this silently fails.
    //
    // Source: https://doc.rust-lang.org/std/fs/fn.remove_dir.html
    //
    // On Windows the binary is not yet physically deleted (happens after
    // process exit), so remove_dir will fail — that is expected and fine.
    match std::fs::remove_dir(&bin_dir) {
        Ok(()) => {
            success(&format!("Removed empty directory: {}", bin_dir.display()));
        }
        Err(e) => {
            tracing::debug!("Could not remove bin directory {}: {e}", bin_dir.display());
        }
    }

    // ── 6. Print success ─────────────────────────────────────────────────
    eprintln!();
    success(&format!("apvm {} has been uninstalled.", CURRENT_VERSION));
    eprintln!();
    eprintln!(
        "  {}",
        dim("To clean up PATH entries from your shell config, remove any line")
    );
    eprintln!(
        "  {}",
        dim("containing \".apvm/bin\" from your shell profile (e.g. ~/.zshrc).")
    );
    eprintln!(
        "  {}",
        dim("This is optional — leftover entries are completely harmless.")
    );
    eprintln!();

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_exe_path ─────────────────────────────────────────────────

    #[test]
    fn resolve_exe_path_returns_absolute_path() {
        // std::env::current_exe() should always return an absolute path on
        // all supported platforms (even in test runners).
        // Source: https://doc.rust-lang.org/std/env/fn.current_exe.html
        let path = resolve_exe_path().expect("current_exe should succeed in test");
        assert!(
            path.is_absolute(),
            "Expected absolute path, got: {}",
            path.display()
        );
    }

    #[test]
    fn resolve_exe_path_points_to_existing_file() {
        let path = resolve_exe_path().expect("current_exe should succeed in test");
        assert!(
            path.exists(),
            "Executable path should exist: {}",
            path.display()
        );
    }

    // ── resolve_bin_dir ──────────────────────────────────────────────────

    #[test]
    fn resolve_bin_dir_returns_parent() {
        let exe = PathBuf::from("/home/user/.apvm/bin/apvm");
        let bin = resolve_bin_dir(&exe).unwrap();
        assert_eq!(bin, PathBuf::from("/home/user/.apvm/bin"));
    }

    #[test]
    #[cfg(windows)]
    fn resolve_bin_dir_works_with_windows_paths() {
        // On Windows, Path::parent() correctly splits on backslash.
        // This test only compiles and runs on Windows — on Unix, backslash
        // is a valid filename character, not a separator.
        // Source: https://doc.rust-lang.org/std/path/struct.Path.html#method.parent
        let exe = PathBuf::from("C:\\Users\\alice\\.apvm\\bin\\apvm.exe");
        let bin = resolve_bin_dir(&exe).unwrap();
        assert_eq!(bin, PathBuf::from("C:\\Users\\alice\\.apvm\\bin"));
    }

    #[test]
    fn resolve_bin_dir_errors_on_root_path() {
        // A root-only path has no parent.
        // On Unix "/" has parent None; on Windows "C:\" has parent Some("C:\").
        // We use a synthetic edge case:
        #[cfg(unix)]
        {
            let exe = PathBuf::from("/");
            // parent() of "/" is None on Unix
            // Source: https://doc.rust-lang.org/std/path/struct.Path.html#method.parent
            // "Returns None if the path terminates in a root or prefix, or if it's the empty string."
            let result = resolve_bin_dir(&exe);
            assert!(result.is_err(), "Root path should have no parent on Unix");
        }
    }

    // ── current_version ──────────────────────────────────────────────────

    #[test]
    fn current_version_is_valid_semver() {
        // The compile-time version must always be valid semver.
        let result = semver::Version::parse(CURRENT_VERSION);
        assert!(
            result.is_ok(),
            "CARGO_PKG_VERSION '{}' should be valid semver, got: {:?}",
            CURRENT_VERSION,
            result.err()
        );
    }

    // ── confirm (pure logic) ─────────────────────────────────────────────
    //
    // We cannot unit-test the `confirm()` function directly because it reads
    // from stdin. Instead we test the core logic: trimming + lowercasing + matching.

    #[test]
    fn confirm_logic_accepts_y() {
        let input = "y";
        let answer = input.trim().to_lowercase();
        assert!(answer == "y" || answer == "yes");
    }

    #[test]
    fn confirm_logic_accepts_yes() {
        let input = "  YES  \n";
        let answer = input.trim().to_lowercase();
        assert!(answer == "y" || answer == "yes");
    }

    #[test]
    fn confirm_logic_rejects_empty() {
        let input = "\n";
        let answer = input.trim().to_lowercase();
        assert!(answer != "y" && answer != "yes");
    }

    #[test]
    fn confirm_logic_rejects_n() {
        let input = "n";
        let answer = input.trim().to_lowercase();
        assert!(answer != "y" && answer != "yes");
    }

    #[test]
    fn confirm_logic_rejects_random() {
        let input = "maybe";
        let answer = input.trim().to_lowercase();
        assert!(answer != "y" && answer != "yes");
    }

    #[test]
    fn confirm_logic_rejects_ye() {
        // Only exact "y" or "yes" should be accepted
        let input = "ye";
        let answer = input.trim().to_lowercase();
        assert!(answer != "y" && answer != "yes");
    }
}
