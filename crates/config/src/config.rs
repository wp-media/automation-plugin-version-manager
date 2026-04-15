//! APVM Configuration.
//!
//! Contains two complementary types:
//!
//! - [`Config`] — the runtime configuration with all values resolved (no `None`
//!   paths). This is what the rest of the application consumes.
//! - [`ConfigFile`] — the on-disk representation where every field is optional.
//!   Only explicitly-set values are serialized, keeping the config file sparse
//!   and human-friendly.
//!
//! # Design Philosophy
//!
//! Libraries should not hardcode default paths. File I/O (load/save) is also
//! not provided - that's the consumer's responsibility.
//!
//! Repository cloning uses temporary directories that are automatically
//! cleaned up after builds complete. No persistent cache directory is needed.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

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

// =============================================================================
// ConfigKey — typed key enum
// =============================================================================

/// Known configuration keys.
///
/// Centralizes the mapping between CLI-facing key names (e.g. `"token"`) and
/// struct fields. Parsing is handled by [`FromStr`], so callers never need to
/// validate raw strings manually.
///
/// # Adding a new key
///
/// 1. Add a variant here.
/// 2. The compiler will guide you to update [`FromStr`], [`Display`],
///    [`ConfigKey::all`], [`ConfigKey::is_sensitive`], and the
///    [`ConfigFile`] methods that match on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigKey {
    /// GitHub Personal Access Token.
    Token,
    /// Builds directory path.
    BuildsDir,
}

const SENSITIVE_KEYS: &[ConfigKey] = &[ConfigKey::Token];

impl ConfigKey {
    /// All known configuration keys.
    pub fn all() -> &'static [ConfigKey] {
        &[ConfigKey::Token, ConfigKey::BuildsDir]
    }

    /// Whether this key holds sensitive data that should be masked in output.
    pub fn is_sensitive(&self) -> bool {
        SENSITIVE_KEYS.contains(self)
    }
}

impl fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigKey::Token => write!(f, "token"),
            ConfigKey::BuildsDir => write!(f, "builds-dir"),
        }
    }
}

impl FromStr for ConfigKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "token" => Ok(ConfigKey::Token),
            "builds-dir" => Ok(ConfigKey::BuildsDir),
            _ => {
                let valid: Vec<_> = ConfigKey::all().iter().map(|k| k.to_string()).collect();
                Err(format!(
                    "Unknown config key '{}'. Valid keys: {}",
                    s,
                    valid.join(", ")
                ))
            }
        }
    }
}

// =============================================================================
// ConfigFile — sparse on-disk representation
// =============================================================================

/// On-disk configuration format.
///
/// Every field is optional so only explicitly-set values appear in the JSON
/// file. When loaded, missing fields are filled from a [`Config`] that
/// carries the defaults (provided by the CLI).
///
/// # Example file with only a token set
///
/// ```json
/// {
///   "github_token": "ghp_xxxxxxxxxxxx"
/// }
/// ```
///
/// Fields absent from the file are silently loaded as `None` and resolved
/// to defaults at runtime via [`ConfigFile::merge`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    /// GitHub Personal Access Token (optional in file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,

    /// Builds directory override (optional in file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_dir: Option<PathBuf>,
}

impl ConfigFile {
    /// Merge this file config with defaults to produce a fully resolved [`Config`].
    ///
    /// Any field present in `self` takes precedence; missing fields fall back
    /// to the corresponding value in `defaults`.
    ///
    /// # Arguments
    ///
    /// * `defaults` - Fallback values for fields not present in the file
    pub fn merge(self, defaults: &Config) -> Config {
        Config {
            github_token: self.github_token.or(defaults.github_token.clone()),
            builds_dir: self
                .builds_dir
                .unwrap_or_else(|| defaults.builds_dir.clone()),
        }
    }

    /// Check whether this file config has any explicit values.
    ///
    /// Returns `false` when every field is `None`, meaning the file
    /// would serialize to `{}`.
    pub fn is_empty(&self) -> bool {
        self.github_token.is_none() && self.builds_dir.is_none()
    }

    /// Set a value by key.
    ///
    /// Since `key` is a [`ConfigKey`], this is infallible — unknown keys
    /// are rejected at parse time.
    /// Values are stored as-is; callers are responsible for sanitization.
    pub fn set(&mut self, key: ConfigKey, value: String) {
        match key {
            ConfigKey::Token => self.github_token = Some(value),
            ConfigKey::BuildsDir => self.builds_dir = Some(PathBuf::from(value)),
        }
    }

    /// Unset a value by key (revert to default on next merge).
    pub fn unset(&mut self, key: ConfigKey) {
        match key {
            ConfigKey::Token => self.github_token = None,
            ConfigKey::BuildsDir => self.builds_dir = None,
        }
    }

    /// Get a value by key, returning a display-friendly string.
    ///
    /// Returns `None` if the value is not set.
    pub fn get(&self, key: ConfigKey) -> Option<String> {
        match key {
            ConfigKey::Token => self.github_token.clone(),
            ConfigKey::BuildsDir => self.builds_dir.as_ref().map(|p| p.display().to_string()),
        }
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
        let config = Config::new(PathBuf::from("/test/builds")).set_token("test-token");

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

    // =========================================================================
    // ConfigFile tests
    // =========================================================================

    #[test]
    fn config_file_default_is_empty() {
        let cf = ConfigFile::default();
        assert!(cf.is_empty());
        assert!(cf.github_token.is_none());
        assert!(cf.builds_dir.is_none());
    }

    #[test]
    fn config_file_merge_uses_defaults_when_empty() {
        let defaults = Config::new(PathBuf::from("/default/builds"));
        let cf = ConfigFile::default();
        let merged = cf.merge(&defaults);

        assert!(merged.github_token.is_none());
        assert_eq!(merged.builds_dir, PathBuf::from("/default/builds"));
    }

    #[test]
    fn config_file_merge_overrides_with_explicit_values() {
        let defaults = Config::new(PathBuf::from("/default/builds"));
        let cf = ConfigFile {
            github_token: Some("ghp_test".to_string()),
            builds_dir: Some(PathBuf::from("/custom/builds")),
        };
        let merged = cf.merge(&defaults);

        assert_eq!(merged.github_token, Some("ghp_test".to_string()));
        assert_eq!(merged.builds_dir, PathBuf::from("/custom/builds"));
    }

    #[test]
    fn config_file_merge_partial_override() {
        let defaults = Config::with_token("default-token", PathBuf::from("/default/builds"));
        let cf = ConfigFile {
            github_token: None,
            builds_dir: Some(PathBuf::from("/custom/builds")),
        };
        let merged = cf.merge(&defaults);

        // Token falls back to default, builds_dir is overridden
        assert_eq!(merged.github_token, Some("default-token".to_string()));
        assert_eq!(merged.builds_dir, PathBuf::from("/custom/builds"));
    }

    #[test]
    fn config_file_serialization_is_sparse() {
        let cf = ConfigFile {
            github_token: Some("ghp_xxx".to_string()),
            builds_dir: None,
        };
        let json = serde_json::to_string_pretty(&cf).unwrap();

        assert!(json.contains("github_token"));
        assert!(!json.contains("builds_dir"));
    }

    #[test]
    fn config_file_empty_serializes_to_empty_object() {
        let cf = ConfigFile::default();
        let json = serde_json::to_string(&cf).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn config_file_set_known_keys() {
        let mut cf = ConfigFile::default();
        cf.set(ConfigKey::Token, "ghp_test".to_string());
        cf.set(ConfigKey::BuildsDir, "/my/builds".to_string());

        assert_eq!(cf.github_token, Some("ghp_test".to_string()));
        assert_eq!(cf.builds_dir, Some(PathBuf::from("/my/builds")));
    }

    #[test]
    fn config_file_unset_known_keys() {
        let mut cf = ConfigFile {
            github_token: Some("ghp_test".to_string()),
            builds_dir: Some(PathBuf::from("/builds")),
        };

        cf.unset(ConfigKey::Token);
        cf.unset(ConfigKey::BuildsDir);
        assert!(cf.is_empty());
    }

    #[test]
    fn config_file_get_known_keys() {
        let cf = ConfigFile {
            github_token: Some("ghp_test".to_string()),
            builds_dir: Some(PathBuf::from("/my/builds")),
        };

        assert_eq!(cf.get(ConfigKey::Token), Some("ghp_test".to_string()));
        assert_eq!(cf.get(ConfigKey::BuildsDir), Some("/my/builds".to_string()));
    }

    #[test]
    fn config_file_get_unset_key_returns_none() {
        let cf = ConfigFile::default();
        assert_eq!(cf.get(ConfigKey::Token), None);
        assert_eq!(cf.get(ConfigKey::BuildsDir), None);
    }

    #[test]
    fn config_file_deserialization_missing_fields() {
        let json = r#"{"github_token": "ghp_test"}"#;
        let cf: ConfigFile = serde_json::from_str(json).unwrap();

        assert_eq!(cf.github_token, Some("ghp_test".to_string()));
        assert!(cf.builds_dir.is_none());
    }

    #[test]
    fn config_file_deserialization_empty_object() {
        let json = "{}";
        let cf: ConfigFile = serde_json::from_str(json).unwrap();
        assert!(cf.is_empty());
    }

    // =========================================================================
    // ConfigKey tests
    // =========================================================================

    #[test]
    fn config_key_from_str_valid() {
        assert_eq!("token".parse::<ConfigKey>().unwrap(), ConfigKey::Token);
        assert_eq!(
            "builds-dir".parse::<ConfigKey>().unwrap(),
            ConfigKey::BuildsDir
        );
    }

    #[test]
    fn config_key_from_str_invalid() {
        let err = "nope".parse::<ConfigKey>().unwrap_err();
        assert!(err.contains("Unknown config key"));
        assert!(err.contains("nope"));
    }

    #[test]
    fn config_key_display_roundtrip() {
        for key in ConfigKey::all() {
            let s = key.to_string();
            let parsed: ConfigKey = s.parse().unwrap();
            assert_eq!(*key, parsed);
        }
    }

    #[test]
    fn config_key_is_sensitive() {
        assert!(ConfigKey::Token.is_sensitive());
        assert!(!ConfigKey::BuildsDir.is_sensitive());
    }

    #[test]
    fn config_key_all_is_exhaustive() {
        // Ensure all() contains every variant by checking we can set+get each
        let mut cf = ConfigFile::default();
        for key in ConfigKey::all() {
            cf.set(*key, "test".to_string());
            assert!(cf.get(*key).is_some());
        }
    }
}
