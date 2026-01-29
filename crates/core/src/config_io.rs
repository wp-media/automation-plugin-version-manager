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

use apvm_config::{Config, Paths};

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
/// let default = Config::new(
///     PathBuf::from("/var/cache/myapp"),
///     PathBuf::from("/var/lib/myapp/builds"),
/// );
/// let config = load_config_or_default(&path, default)?;
/// ```
pub fn load_config_or_default<P: AsRef<Path>>(path: P, default_config: Config) -> Result<Config> {
    load_config_from_path_with_default(path.as_ref(), default_config)
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
/// let config = Config::new(
///     PathBuf::from("/cache"),
///     PathBuf::from("/builds"),
/// ).set_token("ghp_xxx");
///
/// save_config(&config, PathBuf::from("/etc/myapp/config.json"))?;
/// ```
pub fn save_config<P: AsRef<Path>>(config: &Config, path: P) -> Result<PathBuf> {
    let config_path = path.as_ref().to_path_buf();
    save_config_to_path(config, &config_path)?;
    Ok(config_path)
}

/// Save configuration to a specific path.
///
/// This is the internal implementation that handles directory creation
/// and pretty-printing.
fn save_config_to_path(config: &Config, path: &Path) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        if !parent.exists() {
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
    }

    // Serialize with pretty printing
    let content = serde_json::to_string_pretty(config).map_err(|e| {
        Error::Config(format!("Failed to serialize config: {}", e))
    })?;

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

/// Ensure the APVM directory structure exists.
///
/// Creates all required directories if they don't exist:
/// - APVM base directory
/// - Cache directory (under APVM base)
/// - Builds directory
///
/// # Arguments
///
/// * `paths` - Paths configuration with explicit directories
///
/// # Returns
///
/// * `Ok(())` - Directories created or already exist
/// * `Err(Error::Io)` - Failed to create directories
///
/// # Example
///
/// ```ignore
/// use apvm_config::Paths;
/// use apvm_core::config_io::ensure_directories;
/// use std::path::PathBuf;
///
/// // Ensure directories exist with explicit paths
/// let paths = Paths::new(
///     PathBuf::from("/var/lib/myapp"),
///     PathBuf::from("/var/lib/myapp/builds"),
/// );
/// ensure_directories(&paths)?;
/// ```
pub fn ensure_directories(paths: &Paths) -> Result<()> {
    let dirs = [
        paths.apvm_dir(),
        paths.cache_dir(),
        paths.builds_dir(),
    ];

    for dir in dirs {
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

/// Load configuration and ensure directories exist.
///
/// This is a convenience function that combines `load_config_or_default` and
/// `ensure_directories` for typical CLI initialization.
///
/// # Arguments
///
/// * `config_path` - Path to config file
/// * `default_config` - Config to use if file doesn't exist
///
/// # Returns
///
/// A tuple of `(Config, Paths)` ready for use.
///
/// # Example
///
/// ```ignore
/// use std::path::PathBuf;
/// use apvm_config::{Config, Paths};
///
/// let config_path = PathBuf::from("/etc/myapp/config.json");
/// let default = Config::new(
///     PathBuf::from("/var/cache/myapp"),
///     PathBuf::from("/var/lib/myapp/builds"),
/// );
/// let (config, paths) = init_config(&config_path, default)?;
/// ```
pub fn init_config<P: AsRef<Path>>(config_path: P, default_config: Config) -> Result<(Config, Paths)> {
    let config = load_config_or_default(&config_path, default_config)?;
    let paths = Paths::builder()
        .cache_dir(&config.cache_dir)
        .builds_dir(&config.builds_dir)
        .build();

    ensure_directories(&paths)?;

    Ok((config, paths))
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
    use tempfile::TempDir;

    fn test_config(cache: PathBuf, builds: PathBuf) -> Config {
        Config::new(cache, builds)
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
        let default = test_config(
            PathBuf::from("/default/cache"),
            PathBuf::from("/default/builds"),
        );

        let config = load_config_or_default(&path, default).unwrap();
        assert!(config.github_token.is_none());
        assert_eq!(config.cache_dir, PathBuf::from("/default/cache"));
    }

    #[test]
    fn test_load_or_default_empty_file_returns_default() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("empty.json");
        fs::write(&path, "").unwrap();

        let default = test_config(
            PathBuf::from("/default/cache"),
            PathBuf::from("/default/builds"),
        );

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
            r#"{"github_token": "test-token", "cache_dir": "/test/cache", "builds_dir": "/test/builds"}"#,
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

        let config = Config::with_token(
            "my-token",
            PathBuf::from("/cache"),
            PathBuf::from("/builds"),
        );
        save_config(&config, &path).unwrap();

        assert!(path.exists());
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.github_token, Some("my-token".to_string()));
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");

        let original = test_config(
            PathBuf::from("/original/cache"),
            PathBuf::from("/original/builds"),
        )
        .set_token("roundtrip-token")
        .set_cache_dir("/custom/cache")
        .set_builds_dir("/custom/builds");

        save_config(&original, &path).unwrap();
        let loaded = load_config(&path).unwrap();

        assert_eq!(loaded.github_token, original.github_token);
        assert_eq!(loaded.cache_dir, original.cache_dir);
        assert_eq!(loaded.builds_dir, original.builds_dir);
    }

    #[test]
    fn test_ensure_directories_creates_all() {
        let temp = TempDir::new().unwrap();
        let paths = Paths::new(
            temp.path().join(".apvm"),
            temp.path().join("builds"),
        );

        ensure_directories(&paths).unwrap();

        assert!(paths.apvm_dir().exists());
        assert!(paths.cache_dir().exists());
        assert!(paths.builds_dir().exists());
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
}
