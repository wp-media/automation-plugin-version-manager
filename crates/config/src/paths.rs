//! Path management for APVM.
//!
//! Provides path types for APVM. This crate does NOT provide default paths -
//! consumers (like CLI) must provide explicit paths.
//!
//! # Design Philosophy
//!
//! Libraries should not hardcode default paths. This allows:
//! - Consumers to define their own conventions
//! - Better testability (no hidden dependencies on filesystem)
//! - Clear ownership of configuration decisions

use std::path::PathBuf;

/// APVM directory paths.
///
/// All paths must be explicitly provided - there are no defaults.
/// Use [`Paths::new()`] or [`Paths::builder()`] to construct.
///
/// # Example
///
/// ```rust
/// use apvm_config::Paths;
/// use std::path::PathBuf;
///
/// // Create with explicit paths
/// let paths = Paths::new(
///     PathBuf::from("/home/user/.apvm"),
///     PathBuf::from("/home/user/apvm-builds"),
/// );
///
/// // Or use builder for partial construction
/// let paths = Paths::builder()
///     .apvm_dir("/custom/apvm")
///     .builds_dir("/custom/builds")
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct Paths {
    /// Base APVM directory.
    apvm_dir: PathBuf,
    /// Configuration file path.
    config_file: PathBuf,
    /// Repository cache directory.
    cache_dir: PathBuf,
    /// Built artifacts directory.
    builds_dir: PathBuf,
}

impl Paths {
    /// Create a new Paths instance with explicit directories.
    ///
    /// Derives `config_file` and `cache_dir` from `apvm_dir`:
    /// - `config_file` = `{apvm_dir}/config.json`
    /// - `cache_dir` = `{apvm_dir}/cache`
    ///
    /// # Arguments
    ///
    /// * `apvm_dir` - Base APVM directory (e.g., `~/.apvm`)
    /// * `builds_dir` - Directory for built artifacts (e.g., `~/apvm-builds`)
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Paths;
    /// use std::path::PathBuf;
    ///
    /// let paths = Paths::new(
    ///     PathBuf::from("/home/user/.apvm"),
    ///     PathBuf::from("/home/user/apvm-builds"),
    /// );
    /// ```
    pub fn new(apvm_dir: PathBuf, builds_dir: PathBuf) -> Self {
        Self {
            config_file: apvm_dir.join("config.json"),
            cache_dir: apvm_dir.join("cache"),
            apvm_dir,
            builds_dir,
        }
    }

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
    /// **Note**: `builds_dir` is required and has no default.
    ///
    /// # Arguments
    ///
    /// * `apvm_dir` - Base APVM directory
    /// * `builds_dir` - Directory for built artifacts
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Paths;
    /// use std::path::PathBuf;
    ///
    /// let paths = Paths::with_apvm_dir("/custom/apvm", PathBuf::from("/custom/builds"));
    /// assert_eq!(paths.config_file().to_str(), Some("/custom/apvm/config.json"));
    /// ```
    pub fn with_apvm_dir(apvm_dir: impl Into<PathBuf>, builds_dir: PathBuf) -> Self {
        let apvm_dir = apvm_dir.into();
        Self {
            config_file: apvm_dir.join("config.json"),
            cache_dir: apvm_dir.join("cache"),
            apvm_dir,
            builds_dir,
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

    /// Get the builds directory.
    ///
    /// This directory stores built artifacts with deduplication.
    pub fn builds_dir(&self) -> &PathBuf {
        &self.builds_dir
    }
}

/// Builder for customizing APVM paths.
///
/// Both `apvm_dir` and `builds_dir` are required.
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
    ///
    /// # Path Derivation
    ///
    /// - `config_file`: defaults to `{apvm_dir}/config.json` if not set
    /// - `cache_dir`: defaults to `{apvm_dir}/cache` if not set
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
    fn new_creates_paths_with_derived_values() {
        let paths = Paths::new(
            PathBuf::from("/home/user/.apvm"),
            PathBuf::from("/home/user/apvm-builds"),
        );

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/home/user/.apvm"));
        assert_eq!(
            paths.config_file(),
            &PathBuf::from("/home/user/.apvm/config.json")
        );
        assert_eq!(
            paths.cache_dir(),
            &PathBuf::from("/home/user/.apvm/cache")
        );
        assert_eq!(
            paths.builds_dir(),
            &PathBuf::from("/home/user/apvm-builds")
        );
    }

    #[test]
    fn builder_requires_apvm_dir_and_builds_dir() {
        let paths = Paths::builder()
            .apvm_dir("/custom/apvm")
            .builds_dir("/custom/builds")
            .build();

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/custom/apvm"));
        assert_eq!(
            paths.config_file(),
            &PathBuf::from("/custom/apvm/config.json")
        );
        assert_eq!(paths.cache_dir(), &PathBuf::from("/custom/apvm/cache"));
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
    fn with_apvm_dir_derives_internal_paths() {
        let paths = Paths::with_apvm_dir("/my/apvm", PathBuf::from("/my/builds"));

        assert_eq!(paths.apvm_dir(), &PathBuf::from("/my/apvm"));
        assert_eq!(paths.config_file(), &PathBuf::from("/my/apvm/config.json"));
        assert_eq!(paths.cache_dir(), &PathBuf::from("/my/apvm/cache"));
        assert_eq!(paths.builds_dir(), &PathBuf::from("/my/builds"));
    }

    #[test]
    #[should_panic(expected = "apvm_dir is required")]
    fn builder_panics_without_apvm_dir() {
        Paths::builder().builds_dir("/builds").build();
    }

    #[test]
    #[should_panic(expected = "builds_dir is required")]
    fn builder_panics_without_builds_dir() {
        Paths::builder().apvm_dir("/apvm").build();
    }
}
