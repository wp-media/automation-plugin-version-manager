//! APVM Configuration.
//!
//! Contains user-configurable settings. This crate does NOT provide default
//! paths - consumers (like CLI) must provide explicit values.
//!
//! # Design Philosophy
//!
//! Libraries should not hardcode default paths. File I/O (load/save) is also
//! not provided - that's the consumer's responsibility.
//!
//! Repository cloning uses temporary directories that are automatically
//! cleaned up after builds complete. No persistent cache directory is needed.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Main application configuration.
///
/// This struct contains user-configurable settings. Paths must be explicitly
/// provided - there are no defaults.
///
/// # File I/O
///
/// This crate intentionally does NOT provide `load()` or `save()` methods.
/// File operations are the consumer's responsibility.
///
/// # Example
///
/// ```rust
/// use apvm_config::Config;
/// use std::path::PathBuf;
///
/// let config = Config::new(PathBuf::from("/var/lib/myapp/builds"));
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

    /// Directory for storing built artifacts.
    pub builds_dir: PathBuf,
}

impl Config {
    /// Create a new configuration with explicit paths.
    ///
    /// # Arguments
    ///
    /// * `builds_dir` - Directory for built artifacts
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Config;
    /// use std::path::PathBuf;
    ///
    /// let config = Config::new(PathBuf::from("/var/lib/myapp/builds"));
    /// ```
    pub fn new(builds_dir: PathBuf) -> Self {
        Self {
            github_token: None,
            builds_dir,
        }
    }

    /// Create a configuration with a GitHub token and paths.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Config;
    /// use std::path::PathBuf;
    ///
    /// let config = Config::with_token("ghp_xxxxxxxxxxxx", PathBuf::from("/builds"));
    /// assert!(config.github_token.is_some());
    /// ```
    pub fn with_token(token: impl Into<String>, builds_dir: PathBuf) -> Self {
        Self {
            github_token: Some(token.into()),
            builds_dir,
        }
    }

    /// Set the GitHub token.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Config;
    /// use std::path::PathBuf;
    ///
    /// let config = Config::new(PathBuf::from("/builds"))
    ///     .set_token("ghp_xxxxxxxxxxxx");
    /// ```
    pub fn set_token(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
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
    fn new_config_has_no_token() {
        let config = Config::new(PathBuf::from("/test/builds"));
        assert!(config.github_token.is_none());
        assert!(!config.has_token());
    }

    #[test]
    fn new_config_uses_provided_paths() {
        let config = Config::new(PathBuf::from("/test/builds"));
        assert_eq!(config.builds_dir, PathBuf::from("/test/builds"));
    }

    #[test]
    fn with_token_sets_token_and_paths() {
        let config = Config::with_token("test-token", PathBuf::from("/test/builds"));
        assert_eq!(config.github_token, Some("test-token".to_string()));
        assert!(config.has_token());
        assert_eq!(config.builds_dir, PathBuf::from("/test/builds"));
    }

    #[test]
    fn builder_pattern_works() {
        let config = Config::new(PathBuf::from("/builds"))
            .set_token("my-token")
            .set_builds_dir("/custom/builds");

        assert_eq!(config.github_token, Some("my-token".to_string()));
        assert_eq!(config.builds_dir, PathBuf::from("/custom/builds"));
    }

    #[test]
    fn serialization_round_trip() {
        let config = Config::new(PathBuf::from("/test/builds"))
            .set_token("test-token");

        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();

        assert_eq!(config.github_token, restored.github_token);
        assert_eq!(config.builds_dir, restored.builds_dir);
    }

    #[test]
    fn serialization_skips_none_token() {
        let config = Config::new(PathBuf::from("/builds"));
        let json = serde_json::to_string(&config).unwrap();

        // Should not contain "github_token": null
        assert!(!json.contains("github_token"));
    }
}
