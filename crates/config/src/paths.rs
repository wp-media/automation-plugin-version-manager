//! Path management for APVM.
//!
//! Provides default paths with platform-appropriate locations
//! and builder pattern for customization.

use std::path::PathBuf;
use std::sync::LazyLock;

use directories::BaseDirs;

/// Lazily computed home directory with fallback.
static HOME_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| {
            // Fallback: try common environment variables
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
        })
});

/// Default APVM directory: `~/.apvm`
static DEFAULT_APVM_DIR: LazyLock<PathBuf> = LazyLock::new(|| HOME_DIR.join(".apvm"));

/// Default builds directory: `~/apvm-builds`
static DEFAULT_BUILDS_DIR: LazyLock<PathBuf> = LazyLock::new(|| HOME_DIR.join("apvm-builds"));

/// APVM directory paths.
///
/// All paths are computed lazily and can be overridden using the builder.
///
/// # Default Paths
///
/// | Path | Default Location |
/// |------|------------------|
/// | `apvm_dir` | `~/.apvm` |
/// | `config_file` | `~/.apvm/config.json` |
/// | `cache_dir` | `~/.apvm/cache` |
/// | `builds_dir` | `~/apvm-builds` |
///
/// # Example
///
/// ```rust
/// use apvm_config::Paths;
///
/// // Use all defaults
/// let paths = Paths::default();
///
/// // Customize specific paths
/// let paths = Paths::builder()
///     .builds_dir("/mnt/large-drive/apvm-builds")
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct Paths {
    /// Base APVM directory (`~/.apvm` by default).
    apvm_dir: PathBuf,
    /// Configuration file path (`~/.apvm/config.json` by default).
    config_file: PathBuf,
    /// Repository cache directory (`~/.apvm/cache` by default).
    cache_dir: PathBuf,
    /// Built artifacts directory (`~/apvm-builds` by default).
    builds_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            apvm_dir: DEFAULT_APVM_DIR.clone(),
            config_file: DEFAULT_APVM_DIR.join("config.json"),
            cache_dir: DEFAULT_APVM_DIR.join("cache"),
            builds_dir: DEFAULT_BUILDS_DIR.clone(),
        }
    }
}

impl Paths {
    /// Create a new builder for customizing paths.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Paths;
    ///
    /// let paths = Paths::builder()
    ///     .apvm_dir("/custom/apvm")
    ///     .builds_dir("/custom/builds")
    ///     .build();
    /// ```
    pub fn builder() -> PathsBuilder {
        PathsBuilder::default()
    }

    /// Create paths with a custom APVM directory.
    ///
    /// All paths under APVM directory are derived from this base:
    /// - `config_file` = `{apvm_dir}/config.json`
    /// - `cache_dir` = `{apvm_dir}/cache`
    ///
    /// The `builds_dir` remains at its default (`~/apvm-builds`) unless
    /// separately overridden.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Paths;
    ///
    /// let paths = Paths::with_apvm_dir("/custom/apvm");
    /// assert_eq!(paths.config_file().to_str(), Some("/custom/apvm/config.json"));
    /// ```
    pub fn with_apvm_dir(apvm_dir: impl Into<PathBuf>) -> Self {
        let apvm_dir = apvm_dir.into();
        Self {
            config_file: apvm_dir.join("config.json"),
            cache_dir: apvm_dir.join("cache"),
            apvm_dir,
            builds_dir: DEFAULT_BUILDS_DIR.clone(),
        }
    }

    /// Get the APVM directory (`~/.apvm` by default).
    ///
    /// This is the base directory for APVM configuration and cache.
    pub fn apvm_dir(&self) -> &PathBuf {
        &self.apvm_dir
    }

    /// Get the configuration file path (`~/.apvm/config.json` by default).
    pub fn config_file(&self) -> &PathBuf {
        &self.config_file
    }

    /// Get the cache directory (`~/.apvm/cache` by default).
    ///
    /// This directory stores cloned repositories.
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    /// Get the builds directory (`~/apvm-builds` by default).
    ///
    /// This directory stores built artifacts with deduplication.
    pub fn builds_dir(&self) -> &PathBuf {
        &self.builds_dir
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Static default accessors (for use without instantiation)
    // ─────────────────────────────────────────────────────────────────────────

    /// Get the default APVM directory (`~/.apvm`).
    ///
    /// This is a static method that returns the computed default.
    pub fn default_apvm_dir() -> &'static PathBuf {
        &DEFAULT_APVM_DIR
    }

    /// Get the default builds directory (`~/apvm-builds`).
    ///
    /// This is a static method that returns the computed default.
    pub fn default_builds_dir() -> &'static PathBuf {
        &DEFAULT_BUILDS_DIR
    }

    /// Get the default config file path (`~/.apvm/config.json`).
    pub fn default_config_file() -> PathBuf {
        DEFAULT_APVM_DIR.join("config.json")
    }

    /// Get the default cache directory (`~/.apvm/cache`).
    pub fn default_cache_dir() -> PathBuf {
        DEFAULT_APVM_DIR.join("cache")
    }

    /// Get the home directory.
    ///
    /// Uses the `directories` crate with fallbacks to `$HOME` or `$USERPROFILE`.
    pub fn home_dir() -> &'static PathBuf {
        &HOME_DIR
    }
}

/// Builder for customizing APVM paths.
///
/// # Example
///
/// ```rust
/// use apvm_config::Paths;
///
/// let paths = Paths::builder()
///     .apvm_dir("/custom/apvm")
///     .cache_dir("/mnt/fast-ssd/cache")
///     .builds_dir("/mnt/large-hdd/builds")
///     .build();
/// ```
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
    /// If not set, defaults to `~/apvm-builds`.
    pub fn builds_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.builds_dir = Some(path.into());
        self
    }

    /// Build the `Paths` instance.
    ///
    /// Paths that were not explicitly set will use defaults:
    /// - `apvm_dir`: `~/.apvm`
    /// - `config_file`: `{apvm_dir}/config.json`
    /// - `cache_dir`: `{apvm_dir}/cache`
    /// - `builds_dir`: `~/apvm-builds`
    pub fn build(self) -> Paths {
        let apvm_dir = self.apvm_dir.unwrap_or_else(|| DEFAULT_APVM_DIR.clone());

        Paths {
            config_file: self
                .config_file
                .unwrap_or_else(|| apvm_dir.join("config.json")),
            cache_dir: self.cache_dir.unwrap_or_else(|| apvm_dir.join("cache")),
            apvm_dir,
            builds_dir: self
                .builds_dir
                .unwrap_or_else(|| DEFAULT_BUILDS_DIR.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_paths_are_under_home() {
        let paths = Paths::default();

        // APVM dir should be ~/.apvm
        assert!(paths.apvm_dir().ends_with(".apvm"));

        // Config file should be under APVM dir
        assert!(paths.config_file().starts_with(paths.apvm_dir()));
        assert!(paths.config_file().ends_with("config.json"));

        // Cache should be under APVM dir
        assert!(paths.cache_dir().starts_with(paths.apvm_dir()));
        assert!(paths.cache_dir().ends_with("cache"));

        // Builds dir should be ~/apvm-builds (NOT under .apvm)
        assert!(paths.builds_dir().ends_with("apvm-builds"));
        assert!(!paths.builds_dir().starts_with(paths.apvm_dir()));
    }

    #[test]
    fn builder_derives_paths_from_apvm_dir() {
        let paths = Paths::builder()
            .apvm_dir("/custom/apvm")
            .build();

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/custom/apvm"));
        assert_eq!(paths.config_file(), &PathBuf::from("/custom/apvm/config.json"));
        assert_eq!(paths.cache_dir(), &PathBuf::from("/custom/apvm/cache"));
        // builds_dir should still be default
        assert!(paths.builds_dir().ends_with("apvm-builds"));
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
    fn with_apvm_dir_derives_internal_paths() {
        let paths = Paths::with_apvm_dir("/my/apvm");

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/my/apvm"));
        assert_eq!(paths.config_file(), &PathBuf::from("/my/apvm/config.json"));
        assert_eq!(paths.cache_dir(), &PathBuf::from("/my/apvm/cache"));
        // builds_dir should still be default
        assert!(paths.builds_dir().ends_with("apvm-builds"));
    }
}
