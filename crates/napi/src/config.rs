//! Configuration bindings for Node.js.
//!
//! Provides a JavaScript-friendly configuration object that maps to the
//! internal [`apvm_config::Config`] type. All paths are represented as
//! strings since JavaScript doesn't have a native `Path` type.
//!
//! # Optional `cacheDir`
//!
//! When `cacheDir` is omitted, the artifact cache defaults to `~/.apvm/cache`
//! (the same location the CLI uses), so builds persist and share a cache
//! across processes. Callers that want an isolated or ephemeral cache can set
//! `cacheDir` explicitly or disable caching with `cacheEnabled: false`.
//!
//! The cache directory is distinct from a build's per-invocation `outputDir`.

use std::path::PathBuf;

use napi_derive::napi;

/// Configuration options for creating an APVM instance.
///
/// This is a plain JavaScript object (not a class) that you pass
/// to [`Apvm.create()`] or [`Apvm.createWithTokenResolution()`].
///
/// All fields are optional — when `cacheDir` is omitted, the cache defaults
/// to `~/.apvm/cache`.
///
/// # TypeScript
///
/// ```typescript
/// // Minimal — default cache dir (~/.apvm/cache), caching on
/// const apvm = Apvm.create({});
///
/// // With an explicit cache directory
/// const apvm = Apvm.create({ cacheDir: '/var/lib/apvm/cache' });
///
/// // Full configuration
/// const config: ApvmConfig = {
///   cacheDir: '/var/lib/apvm/cache',
///   cacheEnabled: true,
///   githubToken: 'ghp_xxxxxxxxxxxx',
/// };
/// ```
#[derive(Default)]
#[napi(object)]
pub struct ApvmConfig {
    /// Base directory of the artifact cache (the `apvm-storage` store).
    ///
    /// When omitted (`null` or `undefined`), defaults to `~/.apvm/cache`.
    /// This is where cached build artifacts and release assets live; it is
    /// distinct from a build's per-invocation `outputDir`.
    ///
    /// When provided, should be an absolute path to an existing (or creatable)
    /// directory.
    pub cache_dir: Option<String>,

    /// Whether the artifact cache is active.
    ///
    /// Defaults to `true`. When `false`, builds always run and nothing is
    /// read from or written to the cache.
    pub cache_enabled: Option<bool>,

    /// GitHub Personal Access Token for API requests.
    ///
    /// Required for private repositories (e.g., BackWPup).
    /// Optional for public repositories (e.g., WP Rocket), but recommended
    /// for higher API rate limits (5000 vs 60 requests/hour).
    ///
    /// The token needs the `repo` scope for private repositories.
    pub github_token: Option<String>,
}

/// Resolve the default cache directory: `~/.apvm/cache`.
///
/// Falls back to a `apvm/cache` folder under the system temp directory only
/// when the home directory cannot be determined (extremely rare on supported
/// platforms) so instance creation never fails on a missing home.
fn default_cache_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".apvm").join("cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("apvm").join("cache"))
}

impl From<ApvmConfig> for apvm_config::Config {
    fn from(js_config: ApvmConfig) -> Self {
        let cache_dir = js_config
            .cache_dir
            .map(PathBuf::from)
            .unwrap_or_else(default_cache_dir);

        let mut config = apvm_config::Config::new(cache_dir);
        if let Some(token) = js_config.github_token {
            config = config.set_token(token);
        }
        // Absent means "use the default" (caching on); an explicit value wins.
        if let Some(enabled) = js_config.cache_enabled {
            config = config.set_cache_enabled(enabled);
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_enables_cache_with_default_dir() {
        let config: apvm_config::Config = ApvmConfig::default().into();
        assert!(config.cache_enabled);
        assert!(config.cache_dir.ends_with("cache"));
        assert!(config.github_token.is_none());
    }

    #[test]
    fn explicit_values_are_applied() {
        let js = ApvmConfig {
            cache_dir: Some("/custom/cache".to_string()),
            cache_enabled: Some(false),
            github_token: Some("ghp_test".to_string()),
        };
        let config: apvm_config::Config = js.into();
        assert_eq!(config.cache_dir, PathBuf::from("/custom/cache"));
        assert!(!config.cache_enabled);
        assert_eq!(config.github_token.as_deref(), Some("ghp_test"));
    }
}
