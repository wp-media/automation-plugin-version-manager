//! Default paths for APVM CLI.
//!
//! This module defines the default directory locations for the CLI.
//! Libraries do not have defaults - they are provided here.
//!
//! # Default Locations
//!
//! - **APVM directory**: `~/.apvm` (stores config and the artifact cache)
//! - **Cache directory**: `~/.apvm/cache` (the artifact cache store)

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
/// Contains the configuration file and the artifact cache.
static DEFAULT_APVM_DIR: LazyLock<PathBuf> = LazyLock::new(|| HOME_DIR.join(".apvm"));

/// Default cache directory: `~/.apvm/cache`
///
/// Base directory of the artifact cache (the `apvm-storage` store), kept
/// inside the APVM directory alongside the config file.
static DEFAULT_CACHE_DIR: LazyLock<PathBuf> = LazyLock::new(|| DEFAULT_APVM_DIR.join("cache"));

/// Get the default APVM directory (`~/.apvm`).
pub fn default_apvm_dir() -> &'static PathBuf {
    &DEFAULT_APVM_DIR
}

/// Get the default cache directory (`~/.apvm/cache`).
pub fn default_cache_dir() -> &'static PathBuf {
    &DEFAULT_CACHE_DIR
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
    fn cache_dir_is_absolute() {
        let cache = default_cache_dir();
        assert!(cache.is_absolute());
        assert!(cache.ends_with("cache"));
    }

    #[test]
    fn cache_dir_lives_inside_apvm_dir() {
        let apvm = default_apvm_dir();
        let cache = default_cache_dir();
        // The cache directory is nested directly under the APVM directory.
        assert_eq!(cache.parent(), Some(apvm.as_path()));
    }
}
