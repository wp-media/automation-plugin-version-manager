//! CLI path management.
//!
//! Defines the standard directory layout for the APVM CLI.
//! This module is CLI-specific and not part of the core library.
//!
//! # Directory Layout
//!
//! ```text
//! ~/.apvm/                    (base APVM directory)
//! ├── config.json             (config_file)
//! ├── update-check.json       (update_state_file — background notifier state)
//! └── cache/                  (cache_dir — the artifact cache store)
//! ```
//!
//! Note: Repository cloning uses automatic temp directories that are
//! cleaned up after builds complete. The artifact cache under `cache_dir`
//! is the only persistent store.

use std::path::PathBuf;

use apvm_config::Config;

/// CLI application paths.
///
/// Defines the standard directory layout for APVM CLI.
/// This is a CLI-specific type - the core library does not depend on it.
///
/// # Example
///
/// ```rust,ignore
/// use crate::paths::Paths;
///
/// let paths = Paths::new(PathBuf::from("/home/user/.apvm"));
///
/// // Convert to Config for core library
/// let config = paths.to_config();
/// ```
#[derive(Debug, Clone)]
pub struct Paths {
    /// Configuration file path.
    config_file: PathBuf,
    /// Background update-notifier state file path.
    update_state_file: PathBuf,
    /// Artifact cache directory.
    cache_dir: PathBuf,
}

impl Paths {
    /// Create paths from an APVM directory.
    ///
    /// Derives the child paths:
    /// - `config_file` = `{apvm_dir}/config.json`
    /// - `update_state_file` = `{apvm_dir}/update-check.json`
    /// - `cache_dir` = `{apvm_dir}/cache`
    ///
    /// # Arguments
    ///
    /// * `apvm_dir` - Base APVM directory
    pub fn new(apvm_dir: PathBuf) -> Self {
        Self {
            config_file: apvm_dir.join("config.json"),
            update_state_file: apvm_dir.join("update-check.json"),
            cache_dir: apvm_dir.join("cache"),
        }
    }

    /// Get the configuration file path.
    pub fn config_file(&self) -> &PathBuf {
        &self.config_file
    }

    /// Get the background update-notifier state file path.
    ///
    /// This file lives alongside `config.json` (never inside the artifact
    /// cache) so it is independent of `APVM_CACHE_DIR` and never mixes internal
    /// bookkeeping into the user-facing configuration.
    pub fn update_state_file(&self) -> &PathBuf {
        &self.update_state_file
    }

    /// Convert to a Config for the core library.
    ///
    /// This creates a Config with the paths needed by the core library.
    pub fn to_config(&self) -> Config {
        Config::new(self.cache_dir.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_derives_internal_paths() {
        let paths = Paths::new(PathBuf::from("/var/lib/apvm"));

        assert_eq!(
            paths.config_file(),
            &PathBuf::from("/var/lib/apvm/config.json")
        );
        assert_eq!(
            paths.update_state_file(),
            &PathBuf::from("/var/lib/apvm/update-check.json")
        );
        assert_eq!(
            paths.to_config().cache_dir,
            PathBuf::from("/var/lib/apvm/cache")
        );
    }

    #[test]
    fn update_state_file_sits_beside_config() {
        // The notifier state lives in the APVM home next to config.json, not
        // in the cache directory — so it is unaffected by APVM_CACHE_DIR.
        let paths = Paths::new(PathBuf::from("/base"));
        assert_eq!(
            paths.update_state_file().parent(),
            paths.config_file().parent()
        );
    }

    #[test]
    fn to_config_creates_valid_config() {
        let paths = Paths::new(PathBuf::from("/base"));

        let config = paths.to_config();
        assert_eq!(config.cache_dir, PathBuf::from("/base/cache"));
        assert!(config.cache_enabled);
    }
}
