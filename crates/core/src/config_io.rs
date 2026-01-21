//! Configuration file I/O helpers.
//!
//! Provides convenient functions for loading and saving APVM configuration.
//! These are thin wrappers around filesystem operations with proper error handling.
//!
//! # Design Philosophy
//!
//! The `apvm-config` crate intentionally omits file I/O to remain pure and
//! allow consumers full control. This module provides the "batteries included"
//! option for consumers who want simple load/save functionality.
//!
//! # Example
//!
//! ```ignore
//! use apvm_core::config_io::{load_config, save_config};
//! use apvm_config::Paths;
//!
//! // Load from default location
//! let config = load_config(None)?;
//!
//! // Or specify a custom path
//! let config = load_config(Some("/custom/config.json"))?;
//!
//! // Save after modifications
//! config.github_token = Some("ghp_xxx".to_string());
//! save_config(&config, None)?;
//! ```

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use apvm_config::{Config, Paths};

use crate::error::{Error, Result};

/// Load configuration from a file.
///
/// If the file doesn't exist, returns a default configuration.
/// If the file exists but is invalid JSON, returns an error.
///
/// # Arguments
///
/// * `path` - Optional path to config file. If `None`, uses default
///   (`~/.apvm/config.json`).
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
/// // Load from default location
/// let config = load_config(None)?;
///
/// // Load from custom location
/// let config = load_config(Some("/etc/apvm/config.json"))?;
/// ```
pub fn load_config<P: AsRef<Path>>(path: Option<P>) -> Result<Config> {
    let config_path = path
        .map(|p| p.as_ref().to_path_buf())
        .unwrap_or_else(Paths::default_config_file);

    load_config_from_path(&config_path)
}

/// Load configuration from a specific path.
///
/// This is the internal implementation that handles all edge cases.
fn load_config_from_path(path: &Path) -> Result<Config> {
    match fs::read_to_string(path) {
        Ok(content) => {
            // File exists, try to parse it
            if content.trim().is_empty() {
                // Empty file = use defaults
                tracing::debug!("Config file is empty, using defaults");
                return Ok(Config::default());
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
            Ok(Config::default())
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
/// * `path` - Optional path to config file. If `None`, uses default
///   (`~/.apvm/config.json`).
///
/// # Returns
///
/// * `Ok(PathBuf)` - Path where config was saved
/// * `Err(Error::Io)` - Failed to write file
///
/// # Example
///
/// ```ignore
/// let mut config = Config::default();
/// config.github_token = Some("ghp_xxx".to_string());
///
/// // Save to default location
/// let saved_path = save_config(&config, None)?;
/// println!("Config saved to: {}", saved_path.display());
///
/// // Save to custom location
/// save_config(&config, Some("/tmp/apvm-config.json"))?;
/// ```
pub fn save_config<P: AsRef<Path>>(config: &Config, path: Option<P>) -> Result<PathBuf> {
    let config_path = path
        .map(|p| p.as_ref().to_path_buf())
        .unwrap_or_else(Paths::default_config_file);

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
/// - `~/.apvm` (or custom base)
/// - `~/.apvm/cache`
/// - `~/apvm-builds`
///
/// # Arguments
///
/// * `paths` - Paths configuration (use `Paths::default()` for defaults)
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
///
/// // Ensure default directories exist
/// ensure_directories(&Paths::default())?;
///
/// // Or with custom paths
/// let paths = Paths::builder()
///     .builds_dir("/mnt/builds")
///     .build();
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
/// This is a convenience function that combines `load_config` and
/// `ensure_directories` for typical CLI initialization.
///
/// # Arguments
///
/// * `config_path` - Optional path to config file
///
/// # Returns
///
/// A tuple of `(Config, Paths)` ready for use.
///
/// # Example
///
/// ```ignore
/// let (config, paths) = init_config(None)?;
/// let apvm = Apvm::new(config)?;
/// let store = ArtifactStore::new(paths.builds_dir().clone());
/// ```
pub fn init_config<P: AsRef<Path>>(config_path: Option<P>) -> Result<(Config, Paths)> {
    let config = load_config(config_path)?;
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
/// * `path` - Optional path to check. If `None`, uses default location.
///
/// # Example
///
/// ```ignore
/// if !config_exists(None) {
///     println!("No config file found, will use defaults");
/// }
/// ```
pub fn config_exists<P: AsRef<Path>>(path: Option<P>) -> bool {
    let config_path = path
        .map(|p| p.as_ref().to_path_buf())
        .unwrap_or_else(Paths::default_config_file);

    config_path.exists()
}

/// Get the default config file path.
///
/// Convenience function that returns `~/.apvm/config.json`.
pub fn default_config_path() -> PathBuf {
    Paths::default_config_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_nonexistent_returns_default() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nonexistent.json");

        let config = load_config(Some(&path)).unwrap();
        assert!(config.github_token.is_none());
    }

    #[test]
    fn test_load_empty_file_returns_default() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("empty.json");
        fs::write(&path, "").unwrap();

        let config = load_config(Some(&path)).unwrap();
        assert!(config.github_token.is_none());
    }

    #[test]
    fn test_load_valid_config() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");
        fs::write(&path, r#"{"github_token": "test-token"}"#).unwrap();

        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.github_token, Some("test-token".to_string()));
    }

    #[test]
    fn test_load_invalid_json_returns_error() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("invalid.json");
        fs::write(&path, "{ invalid json }").unwrap();

        let result = load_config(Some(&path));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::Config(_)));
    }

    #[test]
    fn test_save_creates_parent_dir() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("subdir").join("config.json");

        let config = Config::with_token("my-token");
        save_config(&config, Some(&path)).unwrap();

        assert!(path.exists());
        let loaded = load_config(Some(&path)).unwrap();
        assert_eq!(loaded.github_token, Some("my-token".to_string()));
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");

        let original = Config::default()
            .set_token("roundtrip-token")
            .set_cache_dir("/custom/cache")
            .set_builds_dir("/custom/builds");

        save_config(&original, Some(&path)).unwrap();
        let loaded = load_config(Some(&path)).unwrap();

        assert_eq!(loaded.github_token, original.github_token);
        assert_eq!(loaded.cache_dir, original.cache_dir);
        assert_eq!(loaded.builds_dir, original.builds_dir);
    }

    #[test]
    fn test_ensure_directories_creates_all() {
        let temp = TempDir::new().unwrap();
        let paths = Paths::builder()
            .apvm_dir(temp.path().join(".apvm"))
            .cache_dir(temp.path().join(".apvm").join("cache"))
            .builds_dir(temp.path().join("builds"))
            .build();

        ensure_directories(&paths).unwrap();

        assert!(paths.apvm_dir().exists());
        assert!(paths.cache_dir().exists());
        assert!(paths.builds_dir().exists());
    }

    #[test]
    fn test_config_exists_false_for_nonexistent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nope.json");
        assert!(!config_exists(Some(&path)));
    }

    #[test]
    fn test_config_exists_true_for_existing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("exists.json");
        fs::write(&path, "{}").unwrap();
        assert!(config_exists(Some(&path)));
    }
}
