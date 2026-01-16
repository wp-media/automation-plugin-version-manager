//! APVM Configuration.
//!
//! Contains user-configurable settings separate from paths.
//! File I/O (load/save) is not provided - that's the consumer's responsibility.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// Main application configuration.
///
/// This struct contains user-configurable settings. Path defaults
/// are provided but can be overridden.
///
/// # File I/O
///
/// This crate intentionally does NOT provide `load()` or `save()` methods.
/// File operations are the consumer's responsibility. This keeps the library
/// pure and allows consumers to choose their own serialization format,
/// error handling strategy, and storage location.
///
/// # Example (CLI implementation)
///
/// ```rust,ignore
/// use std::fs;
/// use apvm_config::Config;
///
/// fn load_config(path: &Path) -> Result<Config, Error> {
///     if path.exists() {
///         let content = fs::read_to_string(path)?;
///         Ok(serde_json::from_str(&content)?)
///     } else {
///         Ok(Config::default())
///     }
/// }
///
/// fn save_config(config: &Config, path: &Path) -> Result<(), Error> {
///     if let Some(parent) = path.parent() {
///         fs::create_dir_all(parent)?;
///     }
///     let content = serde_json::to_string_pretty(config)?;
///     fs::write(path, content)?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// GitHub Personal Access Token for API requests.
    ///
    /// Optional for public repositories, required for:
    /// - Private repositories
    /// - Higher rate limits (5000 vs 60 requests/hour)
    /// - Accessing draft PRs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,

    /// Cache directory for cloned repositories.
    ///
    /// Default: `~/.apvm/cache`
    #[serde(default = "Paths::default_cache_dir")]
    pub cache_dir: PathBuf,

    /// Directory for storing built artifacts.
    ///
    /// Default: `~/apvm-builds`
    #[serde(default = "default_builds_dir")]
    pub builds_dir: PathBuf,
}

/// Helper function for serde default (needs to return owned value).
fn default_builds_dir() -> PathBuf {
    Paths::default_builds_dir().clone()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            github_token: None,
            cache_dir: Paths::default_cache_dir(),
            builds_dir: Paths::default_builds_dir().clone(),
        }
    }
}

impl Config {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a configuration from a `Paths` instance.
    ///
    /// This copies the relevant paths from the `Paths` struct.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::{Config, Paths};
    ///
    /// let paths = Paths::builder()
    ///     .cache_dir("/fast-ssd/cache")
    ///     .builds_dir("/large-hdd/builds")
    ///     .build();
    ///
    /// let config = Config::with_paths(paths);
    /// assert_eq!(config.cache_dir.to_str(), Some("/fast-ssd/cache"));
    /// ```
    pub fn with_paths(paths: Paths) -> Self {
        Self {
            github_token: None,
            cache_dir: paths.cache_dir().clone(),
            builds_dir: paths.builds_dir().clone(),
        }
    }

    /// Create a configuration with a GitHub token.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Config;
    ///
    /// let config = Config::with_token("ghp_xxxxxxxxxxxx");
    /// assert!(config.github_token.is_some());
    /// ```
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            github_token: Some(token.into()),
            ..Self::default()
        }
    }

    /// Set the GitHub token.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Config;
    ///
    /// let config = Config::default()
    ///     .set_token("ghp_xxxxxxxxxxxx");
    /// ```
    pub fn set_token(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
        self
    }

    /// Set the cache directory.
    pub fn set_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_dir = path.into();
        self
    }

    /// Set the builds directory.
    pub fn set_builds_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.builds_dir = path.into();
        self
    }

    /// Check if a GitHub token is configured.
    pub fn has_token(&self) -> bool {
        self.github_token.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_token() {
        let config = Config::default();
        assert!(config.github_token.is_none());
        assert!(!config.has_token());
    }

    #[test]
    fn default_config_has_default_paths() {
        let config = Config::default();
        assert!(config.cache_dir.ends_with("cache"));
        assert!(config.builds_dir.ends_with("apvm-builds"));
    }

    #[test]
    fn with_token_sets_token() {
        let config = Config::with_token("test-token");
        assert_eq!(config.github_token, Some("test-token".to_string()));
        assert!(config.has_token());
    }

    #[test]
    fn builder_pattern_works() {
        let config = Config::default()
            .set_token("my-token")
            .set_cache_dir("/custom/cache")
            .set_builds_dir("/custom/builds");

        assert_eq!(config.github_token, Some("my-token".to_string()));
        assert_eq!(config.cache_dir, PathBuf::from("/custom/cache"));
        assert_eq!(config.builds_dir, PathBuf::from("/custom/builds"));
    }

    #[test]
    fn with_paths_copies_paths() {
        let paths = Paths::builder()
            .cache_dir("/fast/cache")
            .builds_dir("/big/builds")
            .build();

        let config = Config::with_paths(paths);

        assert_eq!(config.cache_dir, PathBuf::from("/fast/cache"));
        assert_eq!(config.builds_dir, PathBuf::from("/big/builds"));
    }

    #[test]
    fn serialization_round_trip() {
        let config = Config::default()
            .set_token("test-token")
            .set_cache_dir("/test/cache")
            .set_builds_dir("/test/builds");

        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();

        assert_eq!(config.github_token, restored.github_token);
        assert_eq!(config.cache_dir, restored.cache_dir);
        assert_eq!(config.builds_dir, restored.builds_dir);
    }

    #[test]
    fn deserialization_uses_defaults_for_missing_fields() {
        // Config with only github_token
        let json = r#"{"github_token": "my-token"}"#;
        let config: Config = serde_json::from_str(json).unwrap();

        assert_eq!(config.github_token, Some("my-token".to_string()));
        // Paths should have defaults
        assert!(config.cache_dir.ends_with("cache"));
        assert!(config.builds_dir.ends_with("apvm-builds"));
    }

    #[test]
    fn serialization_skips_none_token() {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();

        // Should not contain "github_token": null
        assert!(!json.contains("github_token"));
    }
}
