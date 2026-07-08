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
//! cleaned up after builds complete. The persistent [`Config::cache_dir`] is
//! the base directory of the artifact cache and must be supplied by the
//! consumer (the CLI and the Node bindings each provide their own default).

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Default value for [`Config::cache_enabled`]: caching is on unless the
/// consumer opts out. Used both by [`Config::new`] and by serde when a config
/// file omits the field.
fn default_cache_enabled() -> bool {
    true
}

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
/// let config = Config::new(PathBuf::from("/var/lib/myapp/cache"));
/// assert!(config.cache_enabled);
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

    /// Base directory of the artifact cache (the `apvm-storage` store).
    ///
    /// This is where cached build artifacts and release assets live. It is
    /// distinct from a build's per-invocation output directory. There is no
    /// default here — the consumer supplies one.
    ///
    /// `alias = "builds_dir"` reads config written by versions prior to the
    /// artifact cache (≤ v2.0.1), where this field held the plain build
    /// output directory under the same on-disk key. Always serializes back
    /// out as `cache_dir`, so the file self-migrates on the next save.
    #[serde(alias = "builds_dir")]
    pub cache_dir: PathBuf,

    /// Whether the artifact cache is active.
    ///
    /// When `true` (the default), builds are served from the cache when
    /// possible and warm it otherwise. When `false`, builds always run and
    /// nothing is written to the cache.
    #[serde(default = "default_cache_enabled")]
    pub cache_enabled: bool,
}

impl Config {
    /// Create a new configuration with an explicit cache directory.
    ///
    /// Caching is enabled by default.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - Base directory of the artifact cache
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Config;
    /// use std::path::PathBuf;
    ///
    /// let config = Config::new(PathBuf::from("/var/lib/myapp/cache"));
    /// ```
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            github_token: None,
            cache_dir,
            cache_enabled: default_cache_enabled(),
        }
    }

    /// Create a configuration with a GitHub token and cache directory.
    ///
    /// # Example
    ///
    /// ```rust
    /// use apvm_config::Config;
    /// use std::path::PathBuf;
    ///
    /// let config = Config::with_token("ghp_xxxxxxxxxxxx", PathBuf::from("/cache"));
    /// assert!(config.github_token.is_some());
    /// ```
    pub fn with_token(token: impl Into<String>, cache_dir: PathBuf) -> Self {
        Self {
            github_token: Some(token.into()),
            cache_dir,
            cache_enabled: default_cache_enabled(),
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
    /// let config = Config::new(PathBuf::from("/cache"))
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

    /// Enable or disable the artifact cache.
    pub fn set_cache_enabled(mut self, enabled: bool) -> Self {
        self.cache_enabled = enabled;
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
/// 2. The compiler will guide you to update [`FromStr`], [`fmt::Display`],
///    [`ConfigKey::all`], [`ConfigKey::is_sensitive`], and the
///    [`ConfigFile`] methods that match on this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigKey {
    /// GitHub Personal Access Token.
    Token,
    /// Artifact cache directory path.
    CacheDir,
    /// Whether the artifact cache is active (`true`/`false`).
    Cache,
}

const SENSITIVE_KEYS: &[ConfigKey] = &[ConfigKey::Token];

impl ConfigKey {
    /// All known configuration keys.
    pub fn all() -> &'static [ConfigKey] {
        &[ConfigKey::Token, ConfigKey::CacheDir, ConfigKey::Cache]
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
            ConfigKey::CacheDir => write!(f, "cache-dir"),
            ConfigKey::Cache => write!(f, "cache"),
        }
    }
}

impl FromStr for ConfigKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "token" => Ok(ConfigKey::Token),
            "cache-dir" => Ok(ConfigKey::CacheDir),
            "cache" => Ok(ConfigKey::Cache),
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

    /// Cache directory override (optional in file).
    ///
    /// `alias = "builds_dir"` reads config files written by versions prior to
    /// the artifact cache (≤ v2.0.1), where a user-set build output directory
    /// lived under this same on-disk key (`apvm config set builds-dir ...`).
    /// Always serializes back out as `cache_dir`; the file self-migrates the
    /// next time any `apvm config set/unset` writes it.
    #[serde(alias = "builds_dir", default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,

    /// Cache on/off override (optional in file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_enabled: Option<bool>,
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
            cache_dir: self.cache_dir.unwrap_or_else(|| defaults.cache_dir.clone()),
            cache_enabled: self.cache_enabled.unwrap_or(defaults.cache_enabled),
        }
    }

    /// Check whether this file config has any explicit values.
    ///
    /// Returns `false` when every field is `None`, meaning the file
    /// would serialize to `{}`.
    pub fn is_empty(&self) -> bool {
        self.github_token.is_none() && self.cache_dir.is_none() && self.cache_enabled.is_none()
    }

    /// Set a value by key.
    ///
    /// Since `key` is a [`ConfigKey`], this is infallible — unknown keys
    /// are rejected at parse time. Values are stored as-is; callers are
    /// responsible for sanitization. For [`ConfigKey::Cache`] the value is
    /// expected to be `"true"` or `"false"` (as produced by the CLI
    /// sanitizer); any other value is treated as `false`.
    pub fn set(&mut self, key: ConfigKey, value: String) {
        match key {
            ConfigKey::Token => self.github_token = Some(value),
            ConfigKey::CacheDir => self.cache_dir = Some(PathBuf::from(value)),
            ConfigKey::Cache => self.cache_enabled = Some(value.eq_ignore_ascii_case("true")),
        }
    }

    /// Unset a value by key (revert to default on next merge).
    pub fn unset(&mut self, key: ConfigKey) {
        match key {
            ConfigKey::Token => self.github_token = None,
            ConfigKey::CacheDir => self.cache_dir = None,
            ConfigKey::Cache => self.cache_enabled = None,
        }
    }

    /// Get a value by key, returning a display-friendly string.
    ///
    /// Returns `None` if the value is not set.
    pub fn get(&self, key: ConfigKey) -> Option<String> {
        match key {
            ConfigKey::Token => self.github_token.clone(),
            ConfigKey::CacheDir => self.cache_dir.as_ref().map(|p| p.display().to_string()),
            ConfigKey::Cache => self.cache_enabled.map(|b| b.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_config_has_no_token() {
        let config = Config::new(PathBuf::from("/test/cache"));
        assert!(config.github_token.is_none());
        assert!(!config.has_token());
    }

    #[test]
    fn new_config_uses_provided_paths() {
        let config = Config::new(PathBuf::from("/test/cache"));
        assert_eq!(config.cache_dir, PathBuf::from("/test/cache"));
    }

    #[test]
    fn new_config_enables_cache_by_default() {
        let config = Config::new(PathBuf::from("/test/cache"));
        assert!(config.cache_enabled);
    }

    #[test]
    fn with_token_sets_token_and_paths() {
        let config = Config::with_token("test-token", PathBuf::from("/test/cache"));
        assert_eq!(config.github_token, Some("test-token".to_string()));
        assert!(config.has_token());
        assert_eq!(config.cache_dir, PathBuf::from("/test/cache"));
        assert!(config.cache_enabled);
    }

    #[test]
    fn builder_pattern_works() {
        let config = Config::new(PathBuf::from("/cache"))
            .set_token("my-token")
            .set_cache_dir("/custom/cache")
            .set_cache_enabled(false);

        assert_eq!(config.github_token, Some("my-token".to_string()));
        assert_eq!(config.cache_dir, PathBuf::from("/custom/cache"));
        assert!(!config.cache_enabled);
    }

    #[test]
    fn serialization_round_trip() {
        let config = Config::new(PathBuf::from("/test/cache")).set_token("test-token");

        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();

        assert_eq!(config.github_token, restored.github_token);
        assert_eq!(config.cache_dir, restored.cache_dir);
        assert_eq!(config.cache_enabled, restored.cache_enabled);
    }

    #[test]
    fn serialization_skips_none_token() {
        let config = Config::new(PathBuf::from("/cache"));
        let json = serde_json::to_string(&config).unwrap();

        // Should not contain "github_token": null
        assert!(!json.contains("github_token"));
    }

    #[test]
    fn deserialization_defaults_cache_enabled_when_absent() {
        // A config document written before `cache_enabled` existed still loads,
        // defaulting the cache to on.
        let json = r#"{"cache_dir": "/test/cache"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.cache_enabled);
        assert_eq!(config.cache_dir, PathBuf::from("/test/cache"));
    }

    #[test]
    fn deserialization_migrates_legacy_builds_dir_key() {
        // A config document written by ≤ v2.0.1 (before the artifact cache
        // existed) used `builds_dir` for what is now `cache_dir`. It must load
        // without data loss.
        let json = r#"{"github_token": "ghp_legacy", "builds_dir": "/home/user/my-builds"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.cache_dir, PathBuf::from("/home/user/my-builds"));
        assert_eq!(config.github_token.as_deref(), Some("ghp_legacy"));
    }

    #[test]
    fn serialization_always_writes_the_new_key_name() {
        // Round-tripping a legacy document upgrades it on disk: re-serializing
        // never writes `builds_dir` back out, only `cache_dir`.
        let json = r#"{"builds_dir": "/home/user/my-builds"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let rewritten = serde_json::to_string(&config).unwrap();
        assert!(rewritten.contains("\"cache_dir\":\"/home/user/my-builds\""));
        assert!(!rewritten.contains("builds_dir"));
    }

    // =========================================================================
    // ConfigFile tests
    // =========================================================================

    #[test]
    fn config_file_default_is_empty() {
        let cf = ConfigFile::default();
        assert!(cf.is_empty());
        assert!(cf.github_token.is_none());
        assert!(cf.cache_dir.is_none());
        assert!(cf.cache_enabled.is_none());
    }

    #[test]
    fn config_file_merge_uses_defaults_when_empty() {
        let defaults = Config::new(PathBuf::from("/default/cache"));
        let cf = ConfigFile::default();
        let merged = cf.merge(&defaults);

        assert!(merged.github_token.is_none());
        assert_eq!(merged.cache_dir, PathBuf::from("/default/cache"));
        assert!(merged.cache_enabled);
    }

    #[test]
    fn config_file_merge_overrides_with_explicit_values() {
        let defaults = Config::new(PathBuf::from("/default/cache"));
        let cf = ConfigFile {
            github_token: Some("ghp_test".to_string()),
            cache_dir: Some(PathBuf::from("/custom/cache")),
            cache_enabled: Some(false),
        };
        let merged = cf.merge(&defaults);

        assert_eq!(merged.github_token, Some("ghp_test".to_string()));
        assert_eq!(merged.cache_dir, PathBuf::from("/custom/cache"));
        assert!(!merged.cache_enabled);
    }

    #[test]
    fn config_file_merge_partial_override() {
        let defaults = Config::with_token("default-token", PathBuf::from("/default/cache"));
        let cf = ConfigFile {
            github_token: None,
            cache_dir: Some(PathBuf::from("/custom/cache")),
            cache_enabled: None,
        };
        let merged = cf.merge(&defaults);

        // Token and cache flag fall back to defaults, cache_dir is overridden.
        assert_eq!(merged.github_token, Some("default-token".to_string()));
        assert_eq!(merged.cache_dir, PathBuf::from("/custom/cache"));
        assert!(merged.cache_enabled);
    }

    #[test]
    fn config_file_serialization_is_sparse() {
        let cf = ConfigFile {
            github_token: Some("ghp_xxx".to_string()),
            cache_dir: None,
            cache_enabled: None,
        };
        let json = serde_json::to_string_pretty(&cf).unwrap();

        assert!(json.contains("github_token"));
        assert!(!json.contains("cache_dir"));
        assert!(!json.contains("cache_enabled"));
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
        cf.set(ConfigKey::CacheDir, "/my/cache".to_string());
        cf.set(ConfigKey::Cache, "false".to_string());

        assert_eq!(cf.github_token, Some("ghp_test".to_string()));
        assert_eq!(cf.cache_dir, Some(PathBuf::from("/my/cache")));
        assert_eq!(cf.cache_enabled, Some(false));
    }

    #[test]
    fn config_file_set_cache_parses_true() {
        let mut cf = ConfigFile::default();
        cf.set(ConfigKey::Cache, "true".to_string());
        assert_eq!(cf.cache_enabled, Some(true));
    }

    #[test]
    fn config_file_unset_known_keys() {
        let mut cf = ConfigFile {
            github_token: Some("ghp_test".to_string()),
            cache_dir: Some(PathBuf::from("/cache")),
            cache_enabled: Some(false),
        };

        cf.unset(ConfigKey::Token);
        cf.unset(ConfigKey::CacheDir);
        cf.unset(ConfigKey::Cache);
        assert!(cf.is_empty());
    }

    #[test]
    fn config_file_get_known_keys() {
        let cf = ConfigFile {
            github_token: Some("ghp_test".to_string()),
            cache_dir: Some(PathBuf::from("/my/cache")),
            cache_enabled: Some(true),
        };

        assert_eq!(cf.get(ConfigKey::Token), Some("ghp_test".to_string()));
        assert_eq!(cf.get(ConfigKey::CacheDir), Some("/my/cache".to_string()));
        assert_eq!(cf.get(ConfigKey::Cache), Some("true".to_string()));
    }

    #[test]
    fn config_file_get_unset_key_returns_none() {
        let cf = ConfigFile::default();
        assert_eq!(cf.get(ConfigKey::Token), None);
        assert_eq!(cf.get(ConfigKey::CacheDir), None);
        assert_eq!(cf.get(ConfigKey::Cache), None);
    }

    #[test]
    fn config_file_deserialization_missing_fields() {
        let json = r#"{"github_token": "ghp_test"}"#;
        let cf: ConfigFile = serde_json::from_str(json).unwrap();

        assert_eq!(cf.github_token, Some("ghp_test".to_string()));
        assert!(cf.cache_dir.is_none());
        assert!(cf.cache_enabled.is_none());
    }

    #[test]
    fn config_file_migrates_legacy_builds_dir_key() {
        // This is the exact shape `apvm config set builds-dir <path>` wrote to
        // `~/.apvm/config.json` in ≤ v2.0.1. It must not be silently dropped.
        let json = r#"{"github_token": "ghp_legacy", "builds_dir": "/home/user/my-builds"}"#;
        let cf: ConfigFile = serde_json::from_str(json).unwrap();

        assert_eq!(cf.cache_dir, Some(PathBuf::from("/home/user/my-builds")));
        assert_eq!(cf.github_token.as_deref(), Some("ghp_legacy"));
    }

    #[test]
    fn config_file_resave_upgrades_legacy_key_to_cache_dir() {
        // The next `apvm config set/unset` after loading a legacy file rewrites
        // it — the old key must never reappear.
        let json = r#"{"builds_dir": "/home/user/my-builds"}"#;
        let cf: ConfigFile = serde_json::from_str(json).unwrap();
        let rewritten = serde_json::to_string(&cf).unwrap();

        assert!(rewritten.contains("\"cache_dir\":\"/home/user/my-builds\""));
        assert!(!rewritten.contains("builds_dir"));
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
            "cache-dir".parse::<ConfigKey>().unwrap(),
            ConfigKey::CacheDir
        );
        assert_eq!("cache".parse::<ConfigKey>().unwrap(), ConfigKey::Cache);
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
        assert!(!ConfigKey::CacheDir.is_sensitive());
        assert!(!ConfigKey::Cache.is_sensitive());
    }

    #[test]
    fn config_key_all_is_exhaustive() {
        // Ensure all() contains every variant by checking we can set+get each
        let mut cf = ConfigFile::default();
        for key in ConfigKey::all() {
            let value = match key {
                ConfigKey::Cache => "true".to_string(),
                _ => "test".to_string(),
            };
            cf.set(*key, value);
            assert!(cf.get(*key).is_some());
        }
    }
}
