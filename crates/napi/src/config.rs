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

use napi::bindgen_prelude::{JsObjectValue, JsValue, Object, Unknown};
use napi::{Env, ValueType};
use napi_derive::napi;

/// Configuration options for creating an APVM instance.
///
/// This is a plain JavaScript object (not a class) that you pass
/// to `Apvm.create()` or `Apvm.createWithTokenResolution()`.
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
    /// When omitted, defaults to `~/.apvm/cache`; `APVM_CACHE_DIR`, when set,
    /// overrides either. `Apvm.create` also takes `undefined` as omitted (but
    /// throws `StringExpected` for `null`); `ApvmCache.open` refuses both, so
    /// an unset variable cannot silently select the default cache.
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

/// Resolve a JS config into the core [`apvm_config::Config`] every factory
/// uses (`Apvm.create`, `Apvm.createWithTokenResolution`, `ApvmCache.open`):
/// absent config → defaults, then `APVM_CACHE_DIR` (when set and non-empty)
/// overrides `cacheDir`, so one env var isolates a whole run (e.g. tests)
/// from `~/.apvm/cache`. A relative directory is then made absolute against
/// the current directory, so a later `process.chdir()` cannot point an
/// instance and its `cache()` handle at different caches. Touches nothing
/// on disk.
///
/// `APVM_CACHE_DIR` is read from `process.env` as the calling JS thread
/// sees it: a worker thread's `process.env` is its own copy, which the
/// process environment never sees.
pub fn resolve_config(env: &Env, config: Option<ApvmConfig>) -> apvm_config::Config {
    resolve_config_with(config, js_cache_dir_override(env))
}

/// `APVM_CACHE_DIR` from the calling thread's `process.env` (`None` when
/// unset or empty, as in the core); the process environment where there is
/// no readable `process.env`.
fn js_cache_dir_override(env: &Env) -> Option<PathBuf> {
    match read_process_env(env, apvm_core::config_io::CACHE_DIR_ENV) {
        Ok(value) => value.filter(|dir| !dir.is_empty()).map(PathBuf::from),
        Err(_) => apvm_core::config_io::cache_dir_env_override(),
    }
}

/// `process.env[name]` as a string, `None` when it is not set.
///
/// # Errors
///
/// When `process` or `process.env` is not an object.
fn read_process_env(env: &Env, name: &str) -> napi::Result<Option<String>> {
    let process: Unknown<'_> = env.get_global()?.get_named_property("process")?;
    let process = object(process, "process")?;
    let variables = object(process.get_named_property("env")?, "process.env")?;
    let value: Unknown<'_> = variables.get_named_property(name)?;
    match value.get_type()? {
        ValueType::String => Ok(Some(value.coerce_to_string()?.into_utf8()?.into_owned()?)),
        _ => Ok(None),
    }
}

/// `value` as an object; `what` names it in the error.
///
/// # Errors
///
/// `InvalidArg` when `value` is not an object.
fn object<'env>(value: Unknown<'env>, what: &str) -> napi::Result<Object<'env>> {
    if value.get_type()? != ValueType::Object {
        return Err(napi::Error::new(
            napi::Status::InvalidArg,
            format!("{what} is not an object"),
        ));
    }
    value.coerce_to_object()
}

/// [`resolve_config`] with the `APVM_CACHE_DIR` value passed in (`None` =
/// unset or empty): pure, so both branches are tested without touching the
/// process environment. Mirrors `config_io::apply_env_overrides`, whose only
/// override is the cache directory.
fn resolve_config_with(
    config: Option<ApvmConfig>,
    env_cache_dir: Option<PathBuf>,
) -> apvm_config::Config {
    let config =
        apvm_core::config_io::override_cache_dir(config.unwrap_or_default().into(), env_cache_dir);
    apvm_core::config_io::pin_cache_dir(config)
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

    /// A JS config setting every field, with `dir` as `cacheDir`.
    fn explicit(dir: &str) -> Option<ApvmConfig> {
        Some(ApvmConfig {
            cache_dir: Some(dir.to_string()),
            cache_enabled: Some(false),
            github_token: Some("ghp_test".to_string()),
        })
    }

    #[test]
    fn env_override_wins_over_an_explicit_dir() {
        let env = std::env::temp_dir().join("from-env");
        let config = resolve_config_with(explicit("/explicit/cache"), Some(env.clone()));
        assert_eq!(config.cache_dir, env);
        // The override touches only the directory.
        assert!(!config.cache_enabled);
        assert_eq!(config.github_token.as_deref(), Some("ghp_test"));
    }

    #[test]
    fn explicit_dir_is_used_without_an_env_override() {
        let dir = std::env::temp_dir().join("explicit");
        let config = resolve_config_with(explicit(&dir.to_string_lossy()), None);
        assert_eq!(config.cache_dir, dir);
    }

    #[test]
    fn no_config_and_no_override_is_the_default_dir() {
        let config = resolve_config_with(None, None);
        assert_eq!(config.cache_dir, default_cache_dir());
        assert!(config.cache_enabled);
        assert!(config.github_token.is_none());
    }

    #[test]
    fn relative_dirs_are_pinned_from_either_source() {
        let cwd = std::env::current_dir().unwrap();
        let from_config = resolve_config_with(explicit("rel/cache"), None);
        assert_eq!(from_config.cache_dir, cwd.join("rel/cache"));
        let from_env = resolve_config_with(None, Some(PathBuf::from("rel/env")));
        assert_eq!(from_env.cache_dir, cwd.join("rel/env"));
    }
}
