//! CLI path management.
//!
//! Defines the standard directory layout for the APVM CLI.
//! This module is CLI-specific and not part of the core library.
//!
//! # Directory Layout
//!
//! ```text
//! ~/.apvm/                    (apvm_dir)
//! ├── config.json             (config_file)
//! └── cache/                  (cache_dir - for cloned repositories)
//!
//! ~/apvm-builds/              (builds_dir)
//! └── {project}/
//!     └── {artifact}.zip
//! ```

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
/// let paths = Paths::new(
///     PathBuf::from("/home/user/.apvm"),
///     PathBuf::from("/home/user/apvm-builds"),
/// );
///
/// // Convert to Config for core library
/// let config = paths.to_config();
/// ```
#[derive(Debug, Clone)]
pub struct Paths {
    /// Base APVM directory (e.g., `~/.apvm`).
    apvm_dir: PathBuf,
    /// Configuration file path.
    config_file: PathBuf,
    /// Repository cache directory.
    cache_dir: PathBuf,
    /// Built artifacts directory.
    builds_dir: PathBuf,
}

impl Paths {
    /// Create paths from an APVM directory and builds directory.
    ///
    /// Derives `config_file` and `cache_dir` from `apvm_dir`:
    /// - `config_file` = `{apvm_dir}/config.json`
    /// - `cache_dir` = `{apvm_dir}/cache`
    ///
    /// # Arguments
    ///
    /// * `apvm_dir` - Base APVM directory for config and cache
    /// * `builds_dir` - Directory for built artifacts
    pub fn new(apvm_dir: PathBuf, builds_dir: PathBuf) -> Self {
        Self {
            config_file: apvm_dir.join("config.json"),
            cache_dir: apvm_dir.join("cache"),
            apvm_dir,
            builds_dir,
        }
    }

    /// Create a new builder for customizing paths.
    pub fn builder() -> PathsBuilder {
        PathsBuilder::default()
    }

    /// Get the APVM base directory.
    pub fn apvm_dir(&self) -> &PathBuf {
        &self.apvm_dir
    }

    /// Get the configuration file path.
    pub fn config_file(&self) -> &PathBuf {
        &self.config_file
    }

    /// Get the cache directory.
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    /// Get the builds directory.
    pub fn builds_dir(&self) -> &PathBuf {
        &self.builds_dir
    }

    /// Convert to a Config for the core library.
    ///
    /// This creates a Config with the paths needed by the core library.
    pub fn to_config(&self) -> Config {
        Config::new(self.cache_dir.clone(), self.builds_dir.clone())
    }

    /// Get all directories that should be created.
    ///
    /// Returns paths in creation order (parents first).
    pub fn directories(&self) -> Vec<&PathBuf> {
        vec![&self.apvm_dir, &self.cache_dir, &self.builds_dir]
    }
}

/// Builder for customizing CLI paths.
///
/// Both `apvm_dir` and `builds_dir` are required.
///
/// # Example
///
/// ```rust,ignore
/// let paths = Paths::builder()
///     .apvm_dir("/custom/apvm")
///     .builds_dir("/custom/builds")
///     .build();
/// ```
///
/// # Panics
///
/// Panics if `apvm_dir` or `builds_dir` are not set.
#[derive(Debug, Clone, Default)]
pub struct PathsBuilder {
    apvm_dir: Option<PathBuf>,
    config_file: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    builds_dir: Option<PathBuf>,
}

impl PathsBuilder {
    /// Set the APVM directory.
    ///
    /// If `config_file` and `cache_dir` are not explicitly set,
    /// they will be derived from this directory.
    pub fn apvm_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.apvm_dir = Some(path.into());
        self
    }

    /// Set the configuration file path.
    ///
    /// If not set, defaults to `{apvm_dir}/config.json`.
    pub fn config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Set the cache directory.
    ///
    /// If not set, defaults to `{apvm_dir}/cache`.
    pub fn cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(path.into());
        self
    }

    /// Set the builds directory.
    ///
    /// **Required** - must be set before calling `build()`.
    pub fn builds_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.builds_dir = Some(path.into());
        self
    }

    /// Build the `Paths` instance.
    ///
    /// # Panics
    ///
    /// Panics if `apvm_dir` or `builds_dir` are not set.
    pub fn build(self) -> Paths {
        let apvm_dir = self
            .apvm_dir
            .expect("apvm_dir is required - call .apvm_dir() before .build()");
        let builds_dir = self
            .builds_dir
            .expect("builds_dir is required - call .builds_dir() before .build()");

        Paths {
            config_file: self
                .config_file
                .unwrap_or_else(|| apvm_dir.join("config.json")),
            cache_dir: self.cache_dir.unwrap_or_else(|| apvm_dir.join("cache")),
            apvm_dir,
            builds_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_derives_internal_paths() {
        let paths = Paths::new(
            PathBuf::from("/var/lib/apvm"),
            PathBuf::from("/var/lib/apvm/builds"),
        );

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/var/lib/apvm"));
        assert_eq!(
            paths.config_file(),
            &PathBuf::from("/var/lib/apvm/config.json")
        );
        assert_eq!(paths.cache_dir(), &PathBuf::from("/var/lib/apvm/cache"));
        assert_eq!(
            paths.builds_dir(),
            &PathBuf::from("/var/lib/apvm/builds")
        );
    }

    #[test]
    fn builder_derives_paths() {
        let paths = Paths::builder()
            .apvm_dir("/custom/path")
            .builds_dir("/custom/builds")
            .build();

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/custom/path"));
        assert_eq!(
            paths.config_file(),
            &PathBuf::from("/custom/path/config.json")
        );
        assert_eq!(paths.cache_dir(), &PathBuf::from("/custom/path/cache"));
        assert_eq!(paths.builds_dir(), &PathBuf::from("/custom/builds"));
    }

    #[test]
    fn builder_allows_explicit_overrides() {
        let paths = Paths::builder()
            .apvm_dir("/base")
            .config_file("/other/config.json")
            .cache_dir("/fast-ssd/cache")
            .builds_dir("/large-hdd/builds")
            .build();

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/base"));
        assert_eq!(paths.config_file(), &PathBuf::from("/other/config.json"));
        assert_eq!(paths.cache_dir(), &PathBuf::from("/fast-ssd/cache"));
        assert_eq!(paths.builds_dir(), &PathBuf::from("/large-hdd/builds"));
    }

    #[test]
    fn to_config_creates_valid_config() {
        let paths = Paths::new(PathBuf::from("/base"), PathBuf::from("/builds"));

        let config = paths.to_config();
        assert_eq!(config.cache_dir, PathBuf::from("/base/cache"));
        assert_eq!(config.builds_dir, PathBuf::from("/builds"));
    }

    #[test]
    fn directories_returns_all_dirs() {
        let paths = Paths::new(PathBuf::from("/base"), PathBuf::from("/builds"));

        let dirs = paths.directories();
        assert_eq!(dirs.len(), 3);
        assert!(dirs.contains(&&PathBuf::from("/base")));
        assert!(dirs.contains(&&PathBuf::from("/base/cache")));
        assert!(dirs.contains(&&PathBuf::from("/builds")));
    }

    #[test]
    #[should_panic(expected = "apvm_dir is required")]
    fn builder_panics_without_apvm_dir() {
        Paths::builder().builds_dir("/builds").build();
    }

    #[test]
    #[should_panic(expected = "builds_dir is required")]
    fn builder_panics_without_builds_dir() {
        Paths::builder().apvm_dir("/base").build();
    }
}
