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

use serde::{Deserialize, Deserializer, Serialize};

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
#[serde(try_from = "RawConfig")]
pub struct Config {
    /// GitHub Personal Access Token for API requests.
    ///
    /// Optional for public repositories, required for:
    /// - Private repositories
    /// - Higher rate limits (5000 vs 60 requests/hour)
    /// - Accessing draft PRs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,

    /// Base directory of the artifact cache (the `apvm-storage` store).
    ///
    /// This is where cached build artifacts and release assets live. It is
    /// distinct from a build's per-invocation output directory. There is no
    /// default here — the consumer supplies one.
    ///
    /// Also read from the legacy `builds_dir` key (see `RawConfig`); always
    /// serialized as `cache_dir`, so the file self-migrates on the next save.
    pub cache_dir: PathBuf,

    /// Whether the artifact cache is active.
    ///
    /// When `true` (the default), builds are served from the cache when
    /// possible and warm it otherwise. When `false`, builds always run and
    /// nothing is written to the cache. Defaults to `true` when a document
    /// omits it.
    pub cache_enabled: bool,
}

/// Deserialization shape of [`Config`].
///
/// Accepts the legacy `builds_dir` key next to `cache_dir`. Versions prior to
/// the artifact cache (≤ v2.0.1) stored the build output directory under
/// `builds_dir`; a serde `alias` would read it too, but fails with "duplicate
/// field" when a hand-edited file carries both keys. Here `cache_dir` wins
/// when both are present; `builds_dir` is still parsed (a malformed value is
/// an error) but its value is then unused.
///
/// Field order matters for serde's positional (array) form: `builds_dir`
/// comes last so `[token, cache_dir, cache_enabled]` still deserializes.
/// `expecting` keeps this internal name out of error messages.
#[derive(Deserialize)]
#[serde(expecting = "a JSON object")]
struct RawConfig {
    /// See [`Config::github_token`].
    #[serde(default)]
    github_token: Option<String>,
    /// See [`Config::cache_dir`]. May be absent (then `builds_dir` applies),
    /// but `null` is rejected as a wrong type, with its position.
    #[serde(default, deserialize_with = "present")]
    cache_dir: Option<PathBuf>,
    /// See [`Config::cache_enabled`].
    #[serde(default = "default_cache_enabled")]
    cache_enabled: bool,
    /// Legacy name of `cache_dir`, used only when `cache_dir` is absent.
    #[serde(default, deserialize_with = "present")]
    builds_dir: Option<PathBuf>,
}

/// Deserialize a field that may be absent (pair with `#[serde(default)]`)
/// but must hold a real value when present: `null` fails like any other
/// wrong type, instead of being read as "absent".
///
/// # Arguments
///
/// * `deserializer` - The field's deserializer
///
/// # Returns
///
/// The value, wrapped in `Some`.
///
/// # Errors
///
/// Whatever `T` reports for an invalid value, including `null`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl TryFrom<RawConfig> for Config {
    type Error = String;

    /// Resolve the cache directory (`cache_dir`, else the legacy
    /// `builds_dir`).
    ///
    /// # Errors
    ///
    /// `missing field `cache_dir`` when neither key is present, since the
    /// runtime config has no default directory. (This check runs after
    /// parsing, so serde_json cannot attach a line/column to it.)
    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        let cache_dir = raw
            .cache_dir
            .or(raw.builds_dir)
            .ok_or_else(|| "missing field `cache_dir`".to_string())?;
        Ok(Self {
            github_token: raw.github_token,
            cache_dir,
            cache_enabled: raw.cache_enabled,
        })
    }
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
/// 2. The compiler will guide you to update [`ConfigKey::as_str`] (the name
///    [`FromStr`] and [`fmt::Display`] use) and the [`ConfigFile`] methods
///    that match on this enum.
/// 3. Add it to [`ConfigKey::all`] by hand — the compiler cannot catch a
///    missing entry there, and [`FromStr`] only finds keys listed in it. The
///    exhaustive match in the `all_lists_every_variant_once` test stops
///    compiling until you list the new variant there too. Update
///    [`ConfigKey::is_sensitive`]'s list if the value is secret.
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

    /// The CLI-facing key name (e.g. `"cache-dir"`), as [`FromStr`] parses it.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigKey::Token => "token",
            ConfigKey::CacheDir => "cache-dir",
            ConfigKey::Cache => "cache",
        }
    }
}

impl fmt::Display for ConfigKey {
    /// Write the key name, honoring width/alignment specs such as `{:<14}`
    /// (`pad` applies them; `write!` would ignore them), which `apvm config`
    /// uses to align its columns.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for ConfigKey {
    type Err = String;

    /// Parse an exact key name ([`ConfigKey::as_str`]); the error lists every
    /// valid key.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ConfigKey::all()
            .iter()
            .copied()
            .find(|key| key.as_str() == s)
            .ok_or_else(|| {
                let valid: Vec<_> = ConfigKey::all().iter().map(ConfigKey::as_str).collect();
                format!("Unknown config key '{s}'. Valid keys: {}", valid.join(", "))
            })
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
#[serde(from = "RawConfigFile")]
pub struct ConfigFile {
    /// GitHub Personal Access Token (optional in file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,

    /// Cache directory override (optional in file).
    ///
    /// Also read from the legacy `builds_dir` key that ≤ v2.0.1 wrote for
    /// `apvm config set builds-dir ...` (see `RawConfigFile`). Always
    /// serialized as `cache_dir`; the file self-migrates the next time any
    /// `apvm config set/unset` writes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,

    /// Cache on/off override (optional in file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_enabled: Option<bool>,
}

/// Deserialization shape of [`ConfigFile`]: accepts the legacy `builds_dir`
/// key next to `cache_dir`, with `cache_dir` winning (see [`RawConfig`] for
/// why a serde `alias` is not enough, and for the field order). In this
/// sparse format `null` means "not set", so `builds_dir` applies when
/// `cache_dir` is absent or `null`. Unknown keys are ignored, so a file
/// written by a newer version still loads.
#[derive(Deserialize)]
#[serde(expecting = "a JSON object")]
struct RawConfigFile {
    /// See [`ConfigFile::github_token`].
    #[serde(default)]
    github_token: Option<String>,
    /// See [`ConfigFile::cache_dir`].
    #[serde(default)]
    cache_dir: Option<PathBuf>,
    /// See [`ConfigFile::cache_enabled`].
    #[serde(default)]
    cache_enabled: Option<bool>,
    /// Legacy name of `cache_dir`, used only when `cache_dir` is absent or
    /// `null`.
    #[serde(default)]
    builds_dir: Option<PathBuf>,
}

impl From<RawConfigFile> for ConfigFile {
    /// Keep every explicit value, preferring `cache_dir` over `builds_dir`.
    fn from(raw: RawConfigFile) -> Self {
        Self {
            github_token: raw.github_token,
            cache_dir: raw.cache_dir.or(raw.builds_dir),
            cache_enabled: raw.cache_enabled,
        }
    }
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
    /// Returns `true` when every field is `None`, meaning the file would
    /// serialize to `{}`.
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
    fn config_prefers_cache_dir_over_a_leftover_legacy_key() {
        // A hand-edited file can hold both keys. That must load (the current
        // key wins, in either order), not fail as a "duplicate field".
        for json in [
            r#"{"builds_dir": "/old", "cache_dir": "/new"}"#,
            r#"{"cache_dir": "/new", "builds_dir": "/old"}"#,
        ] {
            let config: Config = serde_json::from_str(json).unwrap();
            assert_eq!(config.cache_dir, PathBuf::from("/new"), "{json}");
        }
    }

    #[test]
    fn config_rejects_a_null_cache_dir_as_a_wrong_type() {
        // `null` is a present-but-invalid value: report it as such, with its
        // position — never as "missing", and never fall back to the legacy key.
        for json in [
            r#"{"cache_dir": null}"#,
            r#"{"cache_dir": null, "builds_dir": "/x"}"#,
        ] {
            let err = serde_json::from_str::<Config>(json)
                .unwrap_err()
                .to_string();
            assert!(err.starts_with("invalid type: null"), "{json}: {err}");
            assert!(err.contains("line 1 column"), "{json}: {err}");
        }
    }

    #[test]
    fn config_errors_never_name_internal_types() {
        for json in ["42", "null", r#""x""#] {
            let err = serde_json::from_str::<Config>(json)
                .unwrap_err()
                .to_string();
            assert!(err.contains("expected a JSON object"), "{json}: {err}");
            assert!(!err.contains("Raw"), "{json}: {err}");
        }
    }

    #[test]
    fn config_still_deserializes_from_its_array_form() {
        // serde structs also accept a positional array; the legacy key must
        // not change the expected length.
        let config: Config = serde_json::from_str(r#"[null, "/x", false]"#).unwrap();
        assert_eq!(config.cache_dir, PathBuf::from("/x"));
        assert!(!config.cache_enabled);
    }

    #[test]
    fn config_without_any_cache_dir_key_is_rejected() {
        // The runtime config has no default cache directory.
        let err = serde_json::from_str::<Config>(r#"{"cache_enabled": true}"#).unwrap_err();
        assert!(
            err.to_string().contains("missing field `cache_dir`"),
            "{err}"
        );
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
    fn config_file_set_cache_is_true_only_for_true_in_any_case() {
        // Documented contract: anything but "true" (case-insensitive) is
        // stored as `false`, so a malformed value can never enable caching
        // by accident.
        for (value, expected) in [
            ("TRUE", true),
            ("True", true),
            ("false", false),
            ("yes", false),
            ("1", false),
            ("", false),
        ] {
            let mut cf = ConfigFile::default();
            cf.set(ConfigKey::Cache, value.to_string());
            assert_eq!(cf.cache_enabled, Some(expected), "value {value:?}");
            assert_eq!(cf.get(ConfigKey::Cache), Some(expected.to_string()));
        }
    }

    #[test]
    fn config_file_tolerates_keys_from_newer_versions() {
        // A config written by a newer apvm (extra keys) must still load in
        // an older one instead of failing every command.
        let json = r#"{"cache_dir": "/cache", "future_setting": {"nested": [1, 2]}}"#;
        let cf: ConfigFile = serde_json::from_str(json).unwrap();
        assert_eq!(cf.cache_dir, Some(PathBuf::from("/cache")));
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.cache_dir, PathBuf::from("/cache"));
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
    fn config_file_treats_null_as_unset_and_falls_back_to_the_legacy_key() {
        // In the sparse file `null` means "not set" — so a legacy value next
        // to it is used, exactly as when `cache_dir` is absent.
        let cf: ConfigFile = serde_json::from_str(r#"{"cache_dir": null}"#).unwrap();
        assert_eq!(cf.cache_dir, None);
        let cf: ConfigFile =
            serde_json::from_str(r#"{"cache_dir": null, "builds_dir": "/x"}"#).unwrap();
        assert_eq!(cf.cache_dir, Some(PathBuf::from("/x")));
    }

    #[test]
    fn config_file_errors_never_name_internal_types() {
        for json in ["42", "null", r#""x""#] {
            let err = serde_json::from_str::<ConfigFile>(json)
                .unwrap_err()
                .to_string();
            assert!(err.contains("expected a JSON object"), "{json}: {err}");
            assert!(!err.contains("Raw"), "{json}: {err}");
        }
    }

    #[test]
    fn config_file_prefers_cache_dir_over_a_leftover_legacy_key() {
        // Both keys in one file (e.g. hand-edited after an upgrade) must load,
        // with the current key winning in either order, and re-saving drops
        // the legacy one.
        for json in [
            r#"{"builds_dir": "/old", "cache_dir": "/new"}"#,
            r#"{"cache_dir": "/new", "builds_dir": "/old"}"#,
        ] {
            let cf: ConfigFile = serde_json::from_str(json).unwrap();
            assert_eq!(cf.cache_dir, Some(PathBuf::from("/new")), "{json}");
            assert_eq!(
                serde_json::to_string(&cf).unwrap(),
                r#"{"cache_dir":"/new"}"#
            );
        }
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
    fn config_key_parsing_is_exact_and_lists_valid_keys() {
        // Keys are matched exactly (no case folding, no `_` for `-`), and
        // the error lists every valid key so the CLI message is actionable.
        for wrong in ["Token", "CACHE", "cache_dir", " cache-dir", "builds-dir"] {
            let err = wrong.parse::<ConfigKey>().unwrap_err();
            assert!(
                err.ends_with("Valid keys: token, cache-dir, cache"),
                "{wrong:?}: {err}"
            );
        }
    }

    #[test]
    fn config_key_display_honors_width_and_alignment() {
        // `apvm config` lays out its key column with `{:<14}`; a Display that
        // ignores the spec collapses the columns.
        assert_eq!(format!("{:<10}|", ConfigKey::Cache), "cache     |");
        assert_eq!(format!("{:>11}|", ConfigKey::CacheDir), "  cache-dir|");
        assert_eq!(format!("{:^7}|", ConfigKey::Token), " token |");
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
    fn all_lists_every_variant_once() {
        // Exhaustive on purpose: a new variant makes this match fail to
        // compile — the reminder to add it below and to `ConfigKey::all()`,
        // which `FromStr` depends on to find it.
        let listed = |key: ConfigKey| match key {
            ConfigKey::Token | ConfigKey::CacheDir | ConfigKey::Cache => key,
        };
        let every = [
            listed(ConfigKey::Token),
            listed(ConfigKey::CacheDir),
            listed(ConfigKey::Cache),
        ];
        assert_eq!(ConfigKey::all(), every);
        for key in every {
            assert_eq!(key.as_str().parse::<ConfigKey>(), Ok(key));
        }
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
