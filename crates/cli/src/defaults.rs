//! Default paths for APVM CLI.
//!
//! This module defines the default directory locations for the CLI.
//! Libraries do not have defaults - they are provided here.
//!
//! # Default Locations
//!
//! - **APVM directory**: `~/.apvm` (stores config and cache)
//! - **Builds directory**: `~/apvm-builds` (stores built artifacts)

use std::path::PathBuf;
use std::sync::LazyLock;

use directories::BaseDirs;

/// User's home directory.
///
/// Lazily initialized on first access. Panics if the home directory
/// cannot be determined (extremely rare on supported platforms).
static HOME_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    BaseDirs::new()
        .expect("Failed to determine home directory")
        .home_dir()
        .to_path_buf()
});

/// Default APVM directory: `~/.apvm`
///
/// Contains configuration file and repository cache.
static DEFAULT_APVM_DIR: LazyLock<PathBuf> = LazyLock::new(|| HOME_DIR.join(".apvm"));

/// Default builds directory: `~/apvm-builds`
///
/// Contains built plugin artifacts, separate from APVM internals
/// for easier access and management.
static DEFAULT_BUILDS_DIR: LazyLock<PathBuf> = LazyLock::new(|| HOME_DIR.join("apvm-builds"));

/// Get the default APVM directory (`~/.apvm`).
pub fn default_apvm_dir() -> &'static PathBuf {
    &DEFAULT_APVM_DIR
}

/// Get the default builds directory (`~/apvm-builds`).
pub fn default_builds_dir() -> &'static PathBuf {
    &DEFAULT_BUILDS_DIR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apvm_dir_is_absolute() {
        let apvm = default_apvm_dir();
        assert!(apvm.is_absolute());
        assert!(apvm.ends_with(".apvm"));
    }

    #[test]
    fn builds_dir_is_absolute() {
        let builds = default_builds_dir();
        assert!(builds.is_absolute());
        assert!(builds.ends_with("apvm-builds"));
    }

    #[test]
    fn apvm_dir_and_builds_dir_share_parent() {
        let apvm = default_apvm_dir();
        let builds = default_builds_dir();
        // Both should be under home directory
        let apvm_parent = apvm.parent().expect("apvm should have parent");
        let builds_parent = builds.parent().expect("builds should have parent");
        assert_eq!(apvm_parent, builds_parent);
    }
}
