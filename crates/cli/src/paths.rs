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
    /// Artifact cache directory.
    cache_dir: PathBuf,
}

impl Paths {
    /// Create paths from an APVM directory.
    ///
    /// Derives the child paths:
    /// - `config_file` = `{apvm_dir}/config.json`
    /// - `cache_dir` = `{apvm_dir}/cache`
    ///
    /// # Arguments
    ///
    /// * `apvm_dir` - Base APVM directory
    pub fn new(apvm_dir: PathBuf) -> Self {
        Self {
            config_file: apvm_dir.join("config.json"),
            cache_dir: apvm_dir.join("cache"),
        }
    }

    /// Get the configuration file path.
    pub fn config_file(&self) -> &PathBuf {
        &self.config_file
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
            paths.to_config().cache_dir,
            PathBuf::from("/var/lib/apvm/cache")
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
