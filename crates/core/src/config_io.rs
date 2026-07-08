//! Configuration file I/O helpers.
//!
//! Provides functions for loading and saving APVM configuration.
//! These are thin wrappers around filesystem operations with proper error handling.
//!
//! # Design Philosophy
//!
//! The `apvm-config` crate intentionally omits file I/O to remain pure and
//! allow consumers full control. This module provides the "batteries included"
//! option for consumers who want simple load/save functionality.
//!
//! **Note:** This module does NOT provide default paths. Callers must provide
//! explicit paths for all operations.
//!
//! # Example
//!
//! ```ignore
//! use apvm_core::config_io::{load_config, save_config};
//! use std::path::PathBuf;
//!
//! let config_path = PathBuf::from("/etc/myapp/config.json");
//!
//! // Load from explicit path
//! let config = load_config(&config_path)?;
//!
//! // Save after modifications
//! config.github_token = Some("ghp_xxx".to_string());
//! save_config(&config, &config_path)?;
//! ```

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use apvm_config::{Config, ConfigFile};

use crate::error::{Error, Result};

/// Load configuration from a file.
///
/// If the file doesn't exist, returns an error. Use `load_config_or_default`
/// if you want to fall back to defaults.
///
/// # Arguments
///
/// * `path` - Path to config file
///
/// # Returns
///
/// * `Ok(Config)` - Loaded configuration
/// * `Err(Error::Config)` - File exists but couldn't be parsed
/// * `Err(Error::Io)` - File not found or other I/O error
///
/// # Example
///
/// ```ignore
/// use std::path::PathBuf;
///
/// let path = PathBuf::from("/etc/myapp/config.json");
/// let config = load_config(&path)?;
/// ```
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Config> {
    load_config_from_path(path.as_ref())
}

/// Load configuration from a file, or create a default if the file doesn't exist.
///
/// This is a convenience function for CLI applications that want to use
/// defaults when no config file exists.
///
/// # Arguments
///
/// * `path` - Path to config file
/// * `default_config` - Config to use if file doesn't exist
///
/// # Returns
///
/// * `Ok(Config)` - Loaded or default configuration
/// * `Err(Error::Config)` - File exists but couldn't be parsed
/// * `Err(Error::Io)` - Other I/O error (permissions, etc.)
///
/// # Example
///
/// ```ignore
/// use std::path::PathBuf;
/// use apvm_config::Config;
///
/// let path = PathBuf::from("/etc/myapp/config.json");
/// let default = Config::new(PathBuf::from("/var/lib/myapp/builds"));
/// let config = load_config_or_default(&path, default)?;
/// ```
pub fn load_config_or_default<P: AsRef<Path>>(path: P, default_config: Config) -> Result<Config> {
    load_config_from_path_with_default(path.as_ref(), default_config)
}

/// Load a sparse [`ConfigFile`] from disk and merge it with defaults.
///
/// - If the file doesn't exist or is empty, returns the defaults unchanged.
/// - If the file exists, only the fields it contains override the defaults.
///
/// # Arguments
///
/// * `path` - Path to config file
/// * `defaults` - Fallback configuration for fields absent from the file
///
/// # Returns
///
/// * `Ok(Config)` - Resolved configuration with file overrides applied
/// * `Err(Error::Config)` - File exists but has invalid JSON
/// * `Err(Error::Io)` - Other I/O error (permissions, etc.)
pub fn load_config_file<P: AsRef<Path>>(path: P, defaults: &Config) -> Result<Config> {
    let path = path.as_ref();

    match fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                tracing::debug!("Config file is empty, using defaults");
                return Ok(defaults.clone());
            }

            let config_file: ConfigFile = serde_json::from_str(&content).map_err(|e| {
                Error::Config(format!(
                    "Invalid config file '{}': {}. \
                     Delete the file to reset to defaults, or fix the JSON syntax.",
                    path.display(),
                    e
                ))
            })?;

            tracing::debug!("Loaded config file from {:?}", path);
            Ok(config_file.merge(defaults))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            tracing::debug!("Config file not found, using defaults");
            Ok(defaults.clone())
        }
        Err(e) => Err(Error::Io(io::Error::new(
            e.kind(),
            format!("Failed to read config file '{}': {}", path.display(), e),
        ))),
    }
}

// =============================================================================
// Environment overrides
// =============================================================================

/// Environment variable that overrides the resolved artifact-cache directory.
///
/// When set to a non-empty value it takes precedence over the cache directory
/// from the config file, the Node bindings' `cacheDir`, and the built-in
/// default. Consumers apply it via [`apply_env_overrides`] when they resolve
/// their effective configuration.
///
/// Its primary purpose is isolation: pointing a run (tests, CI, a quick local
/// experiment) at a throwaway directory so it never warms a developer's real
/// `~/.apvm/cache`.
pub const CACHE_DIR_ENV: &str = "APVM_CACHE_DIR";

/// The cache-directory override from [`CACHE_DIR_ENV`], if set to a non-empty
/// value.
///
/// An unset **or empty** variable yields `None` — an accidental
/// `APVM_CACHE_DIR=` must not silently redirect the cache to the process's
/// current directory.
pub fn cache_dir_env_override() -> Option<PathBuf> {
    match std::env::var_os(CACHE_DIR_ENV) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

/// Apply environment-variable overrides to an already-resolved [`Config`].
///
/// Currently honors [`CACHE_DIR_ENV`]: when set, it replaces
/// [`Config::cache_dir`]; all other fields are left untouched. Consumers call
/// this at the point they finalize configuration (the CLI after loading the
/// config file, the Node bindings after converting the JS config), so one
/// environment variable redirects the cache everywhere it is used — including
/// the CLI `cache` command, which operates on the directory directly rather
/// than through an [`crate::Apvm`] instance.
///
/// The [`crate::Apvm`] constructors intentionally do NOT call this: a caller
/// that hands the library an explicit [`Config`] gets exactly that config.
/// Apply this yourself first if you want the same environment behavior.
pub fn apply_env_overrides(config: Config) -> Config {
    override_cache_dir(config, cache_dir_env_override())
}

/// Pure core of [`apply_env_overrides`]: replace the cache directory when an
/// override is provided. Separated from the environment read so the precedence
/// logic is testable without mutating process-global state.
fn override_cache_dir(mut config: Config, cache_dir: Option<PathBuf>) -> Config {
    if let Some(dir) = cache_dir {
        config.cache_dir = dir;
    }
    config
}

/// Load a raw [`ConfigFile`] from disk without merging with defaults.
///
/// Returns `Ok(ConfigFile::default())` if the file doesn't exist or is empty.
/// This is useful when you need to inspect or modify the file-level values
/// (e.g., for `config set` / `config unset` commands).
///
/// # Arguments
///
/// * `path` - Path to config file
///
/// # Returns
///
/// * `Ok(ConfigFile)` - Parsed file config, or empty default if file is absent
/// * `Err(Error::Config)` - File exists but has invalid JSON
/// * `Err(Error::Io)` - Other I/O error (permissions, etc.)
pub fn load_config_file_raw<P: AsRef<Path>>(path: P) -> Result<ConfigFile> {
    let path = path.as_ref();

    match fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                return Ok(ConfigFile::default());
            }

            serde_json::from_str(&content).map_err(|e| {
                Error::Config(format!(
                    "Invalid config file '{}': {}. \
                     Delete the file to reset to defaults, or fix the JSON syntax.",
                    path.display(),
                    e
                ))
            })
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(e) => Err(Error::Io(io::Error::new(
            e.kind(),
            format!("Failed to read config file '{}': {}", path.display(), e),
        ))),
    }
}

/// Save a sparse [`ConfigFile`] to disk.
///
/// Creates parent directories if needed. Only fields that are `Some` will
/// appear in the output JSON.
///
/// # Arguments
///
/// * `config_file` - Sparse configuration to save
/// * `path` - Path to config file
///
/// # Returns
///
/// * `Ok(PathBuf)` - Path where config was saved
/// * `Err(Error::Io)` - Failed to write file
pub fn save_config_file<P: AsRef<Path>>(config_file: &ConfigFile, path: P) -> Result<PathBuf> {
    let config_path = path.as_ref().to_path_buf();
    save_to_path_impl(config_file, &config_path)?;
    Ok(config_path)
}

/// Load configuration from a specific path.
///
/// Internal implementation that handles all edge cases.
fn load_config_from_path(path: &Path) -> Result<Config> {
    match fs::read_to_string(path) {
        Ok(content) => {
            // File exists, try to parse it
            if content.trim().is_empty() {
                return Err(Error::Config(format!(
                    "Config file '{}' is empty. Delete it or add valid JSON.",
                    path.display()
                )));
            }

            serde_json::from_str(&content).map_err(|e| {
                Error::Config(format!(
                    "Invalid config file '{}': {}. \
                     Delete the file to reset to defaults, or fix the JSON syntax.",
                    path.display(),
                    e
                ))
            })
        }
        Err(e) => {
            // File doesn't exist or I/O error
            Err(Error::Io(io::Error::new(
                e.kind(),
                format!("Failed to read config file '{}': {}", path.display(), e),
            )))
        }
    }
}

/// Load configuration from a specific path with default fallback.
fn load_config_from_path_with_default(path: &Path, default_config: Config) -> Result<Config> {
    match fs::read_to_string(path) {
        Ok(content) => {
            // File exists, try to parse it
            if content.trim().is_empty() {
                // Empty file = use defaults
                tracing::debug!("Config file is empty, using defaults");
                return Ok(default_config);
            }

            serde_json::from_str(&content).map_err(|e| {
                Error::Config(format!(
                    "Invalid config file '{}': {}. \
                     Delete the file to reset to defaults, or fix the JSON syntax.",
                    path.display(),
                    e
                ))
            })
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // File doesn't exist = use defaults (not an error)
            tracing::debug!("Config file not found, using defaults");
            Ok(default_config)
        }
        Err(e) => {
            // Other I/O error (permissions, etc.)
            Err(Error::Io(io::Error::new(
                e.kind(),
                format!("Failed to read config file '{}': {}", path.display(), e),
            )))
        }
    }
}

/// Save configuration to a file.
///
/// Creates the parent directory if it doesn't exist.
/// Writes with pretty-printed JSON for human readability.
///
/// # Arguments
///
/// * `config` - Configuration to save
/// * `path` - Path to config file
///
/// # Returns
///
/// * `Ok(PathBuf)` - Path where config was saved
/// * `Err(Error::Io)` - Failed to write file
///
/// # Example
///
/// ```ignore
/// use std::path::PathBuf;
/// use apvm_config::Config;
///
/// let config = Config::new(PathBuf::from("/builds"))
///     .set_token("ghp_xxx");
///
/// save_config(&config, PathBuf::from("/etc/myapp/config.json"))?;
/// ```
pub fn save_config<P: AsRef<Path>>(config: &Config, path: P) -> Result<PathBuf> {
    let config_path = path.as_ref().to_path_buf();
    save_to_path_impl(config, &config_path)?;
    Ok(config_path)
}

/// Save a serializable value to a specific path.
///
/// Internal implementation shared by [`save_config`] and [`save_config_file`].
/// Handles directory creation and pretty-printing.
fn save_to_path_impl<T: serde::Serialize>(value: &T, path: &Path) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent).map_err(|e| {
            Error::Io(io::Error::new(
                e.kind(),
                format!(
                    "Failed to create config directory '{}': {}",
                    parent.display(),
                    e
                ),
            ))
        })?;
        tracing::debug!("Created config directory: {}", parent.display());
    }

    // Serialize with pretty printing
    let content = serde_json::to_string_pretty(value)
        .map_err(|e| Error::Config(format!("Failed to serialize config: {}", e)))?;

    // Write atomically (write to temp, then rename) for safety
    // For simplicity, we do a direct write here. Atomic write would be:
    // 1. Write to {path}.tmp
    // 2. Rename {path}.tmp to {path}
    // This is acceptable for config files.
    fs::write(path, &content).map_err(|e| {
        Error::Io(io::Error::new(
            e.kind(),
            format!("Failed to write config file '{}': {}", path.display(), e),
        ))
    })?;

    tracing::debug!("Saved config to: {}", path.display());
    Ok(())
}

/// Ensure a list of directories exist.
///
/// Creates all directories in the provided list if they don't exist.
///
/// # Arguments
///
/// * `directories` - List of directories to create
///
/// # Returns
///
/// * `Ok(())` - Directories created or already exist
/// * `Err(Error::Io)` - Failed to create directories
///
/// # Example
///
/// ```ignore
/// use apvm_core::config_io::ensure_directories;
/// use std::path::PathBuf;
///
/// let dirs = [
///     PathBuf::from("/var/lib/myapp"),
///     PathBuf::from("/var/cache/myapp"),
///     PathBuf::from("/var/lib/myapp/builds"),
/// ];
/// ensure_directories(&dirs)?;
/// ```
pub fn ensure_directories<P: AsRef<Path>>(directories: &[P]) -> Result<()> {
    for dir in directories {
        let dir = dir.as_ref();
        if !dir.exists() {
            fs::create_dir_all(dir).map_err(|e| {
                Error::Io(io::Error::new(
                    e.kind(),
                    format!("Failed to create directory '{}': {}", dir.display(), e),
                ))
            })?;
            tracing::debug!("Created directory: {}", dir.display());
        }
    }

    Ok(())
}

/// Check if a configuration file exists.
///
/// # Arguments
///
/// * `path` - Path to check
///
/// # Example
///
/// ```ignore
/// use std::path::PathBuf;
///
/// let path = PathBuf::from("/etc/myapp/config.json");
/// if !config_exists(&path) {
///     println!("No config file found, will use defaults");
/// }
/// ```
pub fn config_exists<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apvm_config::ConfigKey;
    use tempfile::TempDir;

    fn test_config(cache: PathBuf) -> Config {
        Config::new(cache)
    }

    #[test]
    fn test_load_nonexistent_returns_error() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nonexistent.json");

        let result = load_config(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_or_default_nonexistent_returns_default() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nonexistent.json");
        let default = test_config(PathBuf::from("/default/cache"));

        let config = load_config_or_default(&path, default).unwrap();
        assert!(config.github_token.is_none());
        assert_eq!(config.cache_dir, PathBuf::from("/default/cache"));
    }

    #[test]
    fn test_load_or_default_empty_file_returns_default() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("empty.json");
        fs::write(&path, "").unwrap();

        let default = test_config(PathBuf::from("/default/cache"));

        let config = load_config_or_default(&path, default).unwrap();
        assert!(config.github_token.is_none());
        assert_eq!(config.cache_dir, PathBuf::from("/default/cache"));
    }

    #[test]
    fn test_load_valid_config() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");
        fs::write(
            &path,
            r#"{"github_token": "test-token", "cache_dir": "/test/cache"}"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.github_token, Some("test-token".to_string()));
    }

    #[test]
    fn test_load_invalid_json_returns_error() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("invalid.json");
        fs::write(&path, "{ invalid json }").unwrap();

        let result = load_config(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::Config(_)));
    }

    #[test]
    fn test_save_creates_parent_dir() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("subdir").join("config.json");

        let config = Config::with_token("my-token", PathBuf::from("/cache"));
        save_config(&config, &path).unwrap();

        assert!(path.exists());
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.github_token, Some("my-token".to_string()));
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");

        let original = test_config(PathBuf::from("/original/cache"))
            .set_token("roundtrip-token")
            .set_cache_dir("/custom/cache");

        save_config(&original, &path).unwrap();
        let loaded = load_config(&path).unwrap();

        assert_eq!(loaded.github_token, original.github_token);
        assert_eq!(loaded.cache_dir, original.cache_dir);
    }

    #[test]
    fn test_ensure_directories_creates_all() {
        let temp = TempDir::new().unwrap();
        let dirs = [
            temp.path().join("apvm"),
            temp.path().join("apvm/cache"),
            temp.path().join("builds"),
        ];

        ensure_directories(&dirs).unwrap();

        assert!(dirs[0].exists());
        assert!(dirs[1].exists());
        assert!(dirs[2].exists());
    }

    #[test]
    fn test_config_exists_false_for_nonexistent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nope.json");
        assert!(!config_exists(&path));
    }

    #[test]
    fn test_config_exists_true_for_existing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("exists.json");
        fs::write(&path, "{}").unwrap();
        assert!(config_exists(&path));
    }

    // =========================================================================
    // ConfigFile I/O tests
    // =========================================================================

    #[test]
    fn test_load_config_file_nonexistent_returns_defaults() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nonexistent.json");
        let defaults = test_config(PathBuf::from("/default/cache"));

        let config = load_config_file(&path, &defaults).unwrap();
        assert!(config.github_token.is_none());
        assert_eq!(config.cache_dir, PathBuf::from("/default/cache"));
    }

    #[test]
    fn test_load_config_file_sparse_merges_with_defaults() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");
        // File only has token, no cache_dir
        fs::write(&path, r#"{"github_token": "ghp_test"}"#).unwrap();

        let defaults = test_config(PathBuf::from("/default/cache"));
        let config = load_config_file(&path, &defaults).unwrap();

        assert_eq!(config.github_token, Some("ghp_test".to_string()));
        assert_eq!(config.cache_dir, PathBuf::from("/default/cache"));
    }

    #[test]
    fn test_load_config_file_raw_nonexistent_returns_empty() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nonexistent.json");

        let cf = load_config_file_raw(&path).unwrap();
        assert!(cf.is_empty());
    }

    #[test]
    fn test_load_config_file_raw_returns_only_set_values() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");
        fs::write(&path, r#"{"github_token": "ghp_test"}"#).unwrap();

        let cf = load_config_file_raw(&path).unwrap();
        assert_eq!(cf.github_token, Some("ghp_test".to_string()));
        assert!(cf.cache_dir.is_none());
    }

    #[test]
    fn test_save_config_file_sparse() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");

        let mut cf = ConfigFile::default();
        cf.set(ConfigKey::Token, "ghp_sparse".to_string());

        save_config_file(&cf, &path).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("github_token"));
        assert!(!content.contains("cache_dir"));
    }

    #[test]
    fn test_save_and_load_config_file_roundtrip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");

        let mut cf = ConfigFile::default();
        cf.set(ConfigKey::Token, "ghp_roundtrip".to_string());
        cf.set(ConfigKey::CacheDir, "/custom/cache".to_string());

        save_config_file(&cf, &path).unwrap();
        let loaded = load_config_file_raw(&path).unwrap();

        assert_eq!(loaded.github_token, Some("ghp_roundtrip".to_string()));
        assert_eq!(loaded.cache_dir, Some(PathBuf::from("/custom/cache")));
    }

    // =========================================================================
    // Edge Cases
    // =========================================================================

    #[test]
    fn test_ensure_directories_already_exist() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("existing");
        fs::create_dir(&dir).unwrap();

        // Should not error if directories already exist
        ensure_directories(&[&dir]).unwrap();
        assert!(dir.exists());
    }

    #[test]
    fn test_load_config_or_default_with_invalid_json_returns_error() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("bad.json");
        fs::write(&path, "not json at all {{{").unwrap();

        let default = test_config(PathBuf::from("/builds"));
        let result = load_config_or_default(&path, default);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_config_overwrites_existing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");

        // First save
        let config1 = Config::with_token("token-1", PathBuf::from("/builds"));
        save_config(&config1, &path).unwrap();

        // Second save overwrites
        let config2 = Config::with_token("token-2", PathBuf::from("/builds"));
        save_config(&config2, &path).unwrap();

        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.github_token, Some("token-2".to_string()));
    }

    // =========================================================================
    // Environment overrides (pure precedence logic; no process-env mutation)
    // =========================================================================

    #[test]
    fn override_cache_dir_replaces_when_present() {
        let config =
            Config::with_token("ghp_x", PathBuf::from("/from/config")).set_cache_enabled(false);
        let overridden = override_cache_dir(config, Some(PathBuf::from("/from/env")));

        // cache_dir is overridden...
        assert_eq!(overridden.cache_dir, PathBuf::from("/from/env"));
        // ...while every other field is preserved untouched.
        assert_eq!(overridden.github_token.as_deref(), Some("ghp_x"));
        assert!(!overridden.cache_enabled);
    }

    #[test]
    fn override_cache_dir_noop_when_absent() {
        let config = Config::new(PathBuf::from("/from/config"));
        let unchanged = override_cache_dir(config, None);
        assert_eq!(unchanged.cache_dir, PathBuf::from("/from/config"));
    }
}
