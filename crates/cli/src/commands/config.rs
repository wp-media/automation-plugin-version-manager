//! Config command implementation.
//!
//! Manages APVM configuration through CLI subcommands:
//!
//! ```text
//! apvm config                     # Show all current settings
//! apvm config get <key>           # Get a specific value
//! apvm config set <key> <value>   # Set a value
//! apvm config unset <key>         # Remove a value (revert to default)
//! apvm config path                # Show config file path
//! ```

use clap::{Args, Subcommand};

use apvm_config::ConfigKey;
use apvm_core::config_io::{load_config_file_raw, save_config_file};

use crate::defaults;
use crate::paths::Paths;
use crate::sanitize::{SanitizeResult, sanitize_value};

/// Arguments for the config command.
#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// Config action to perform (omit to show all settings)
    #[command(subcommand)]
    pub action: Option<ConfigAction>,
}

/// Config subcommands.
#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Get a configuration value
    Get {
        /// Configuration key: "token", "cache-dir", or "cache"
        key: String,
    },
    /// Set a configuration value
    Set {
        /// Configuration key: "token", "cache-dir", or "cache"
        key: String,
        /// Value to set
        value: String,
    },
    /// Remove a configuration value (reverts to default)
    Unset {
        /// Configuration key: "token", "cache-dir", or "cache"
        key: String,
    },
    /// Show the config file path
    Path,
}

impl ConfigArgs {
    /// Execute the config command.
    pub fn execute(&self, paths: &Paths) -> apvm_core::Result<()> {
        match &self.action {
            None => show_all(paths),
            Some(ConfigAction::Get { key }) => get_value(key, paths),
            Some(ConfigAction::Set { key, value }) => set_value(key, value, paths),
            Some(ConfigAction::Unset { key }) => unset_value(key, paths),
            Some(ConfigAction::Path) => show_path(paths),
        }
    }
}

/// Parse a user-supplied key string into a [`ConfigKey`].
fn parse_key(key: &str) -> apvm_core::Result<ConfigKey> {
    key.parse::<ConfigKey>()
        .map_err(apvm_core::error::Error::Config)
}

/// Show all current configuration values with their source.
fn show_all(paths: &Paths) -> apvm_core::Result<()> {
    let config_file = load_config_file_raw(paths.config_file())?;

    println!("APVM Configuration");
    println!();

    for key in ConfigKey::all() {
        let file_value = config_file.get(*key);

        match file_value {
            Some(value) => {
                let display = if key.is_sensitive() {
                    mask_token(&value)
                } else {
                    value
                };
                println!("  {:<14} {} (from config file)", key, display);
            }
            None => {
                let default = default_for_key(*key);
                println!("  {:<14} {} (default)", key, default);
            }
        }
    }

    println!();
    println!("Config file: {}", paths.config_file().display());

    Ok(())
}

/// Get a specific configuration value.
fn get_value(key: &str, paths: &Paths) -> apvm_core::Result<()> {
    let key = parse_key(key)?;

    let config_file = load_config_file_raw(paths.config_file())?;

    match config_file.get(key) {
        Some(value) => {
            if key.is_sensitive() {
                println!("{}", mask_token(&value));
            } else {
                println!("{}", value);
            }
        }
        None => {
            let default = default_for_key(key);
            println!("{} (default)", default);
        }
    }

    Ok(())
}

/// Set a configuration value (sanitized).
fn set_value(key: &str, value: &str, paths: &Paths) -> apvm_core::Result<()> {
    let key = parse_key(key)?;

    // Sanitize the value
    let clean_value = match sanitize_value(key, value) {
        SanitizeResult::Ok(v) => v,
        SanitizeResult::Warning { value, message } => {
            eprintln!("Warning: {}", message);
            value
        }
        SanitizeResult::Error(msg) => {
            return Err(apvm_core::error::Error::Config(msg));
        }
    };

    // Load current file config (or empty if no file)
    let mut config_file = load_config_file_raw(paths.config_file())?;

    // Set the value
    config_file.set(key, clean_value.clone());

    // Save (creates file and parent dirs if needed)
    save_config_file(&config_file, paths.config_file())?;

    let display = if key.is_sensitive() {
        mask_token(&clean_value)
    } else {
        clean_value
    };
    println!("Set '{}' = {}", key, display);

    Ok(())
}

/// Unset a configuration value (revert to default).
fn unset_value(key: &str, paths: &Paths) -> apvm_core::Result<()> {
    let key = parse_key(key)?;

    let mut config_file = load_config_file_raw(paths.config_file())?;

    if config_file.get(key).is_none() {
        println!(
            "'{}' is already unset (using default: {})",
            key,
            default_for_key(key)
        );
        return Ok(());
    }

    config_file.unset(key);

    // If the file would be empty, delete it instead of writing `{}`
    if config_file.is_empty() {
        let path = paths.config_file();
        if path.exists() {
            std::fs::remove_file(path).map_err(|e| {
                apvm_core::error::Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("Failed to remove empty config file: {}", e),
                ))
            })?;
            println!("Unset '{}' (config file removed, using defaults)", key);
        } else {
            println!(
                "Unset '{}' (reverted to default: {})",
                key,
                default_for_key(key)
            );
        }
    } else {
        save_config_file(&config_file, paths.config_file())?;
        println!(
            "Unset '{}' (reverted to default: {})",
            key,
            default_for_key(key)
        );
    }

    Ok(())
}

/// Show the config file path.
fn show_path(paths: &Paths) -> apvm_core::Result<()> {
    let path = paths.config_file();
    println!("{}", path.display());

    if path.exists() {
        println!("  (exists)");
    } else {
        println!("  (not created yet — will be created on first 'config set')");
    }

    Ok(())
}

/// Validate that a key is recognized.
///
/// Deprecated: Use `parse_key()` instead, which delegates to `ConfigKey::from_str`.
/// Kept only for test backward compatibility.
#[cfg(test)]
fn validate_key(key: &str) -> apvm_core::Result<()> {
    parse_key(key).map(|_| ())
}

/// Get the default display value for a key.
fn default_for_key(key: ConfigKey) -> String {
    match key {
        ConfigKey::Token => "(not set)".to_string(),
        ConfigKey::CacheDir => defaults::default_cache_dir().display().to_string(),
        ConfigKey::Cache => "true".to_string(),
    }
}

/// Mask a token for display, showing only the prefix and last 4 characters.
///
/// Only reveals a suffix when enough secret characters remain after the prefix
/// to avoid leaking the entire value.
///
/// # Rules
///
/// 1. Tokens ≤ 8 chars → `"***"` (too short to mask meaningfully)
/// 2. Known prefix + secret ≥ 8 chars → `"ghp_***...last4"`
/// 3. Known prefix + secret < 8 chars → `"ghp_***"` (suffix omitted)
/// 4. No known prefix + secret ≥ 8 chars → `"***...last4"`
/// 5. No known prefix + secret < 8 chars → `"***"`
fn mask_token(token: &str) -> String {
    const MIN_SECRET_FOR_SUFFIX: usize = 8;
    const SUFFIX_LEN: usize = 4;

    if token.len() <= 8 {
        return "***".to_string();
    }

    let known_prefixes = ["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"];

    let prefix = known_prefixes
        .iter()
        .find(|p| token.starts_with(*p))
        .copied()
        .unwrap_or("");

    let secret_len = token.len() - prefix.len();

    // Not enough secret chars to safely show a suffix
    if secret_len < MIN_SECRET_FOR_SUFFIX {
        if prefix.is_empty() {
            return "***".to_string();
        }
        return format!("{}***", prefix);
    }

    let suffix = &token[token.len() - SUFFIX_LEN..];

    if prefix.is_empty() {
        format!("***...{}", suffix)
    } else {
        format!("{}***...{}", prefix, suffix)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn mask_token_with_prefix() {
        assert_eq!(mask_token("ghp_abcdefghijklmnop"), "ghp_***...mnop");
    }

    #[test]
    fn mask_token_fine_grained() {
        assert_eq!(
            mask_token("github_pat_abcdefghijklmnop"),
            "github_pat_***...mnop"
        );
    }

    #[test]
    fn mask_token_short() {
        assert_eq!(mask_token("short"), "***");
    }

    #[test]
    fn mask_token_no_known_prefix() {
        assert_eq!(mask_token("sometokenvalue1234"), "***...1234");
    }

    #[test]
    fn mask_token_short_secret_omits_suffix() {
        // "ghp_" (4) + "abcde" (5) = 9 chars, secret < 8 → no suffix
        assert_eq!(mask_token("ghp_abcde"), "ghp_***");
    }

    #[test]
    fn mask_token_fine_grained_short_secret() {
        // "github_pat_" (11) + "x" (1) = 12 chars, secret < 8 → no suffix
        assert_eq!(mask_token("github_pat_x"), "github_pat_***");
    }

    #[test]
    fn mask_token_prefix_with_enough_secret() {
        // "ghp_" (4) + 8 secret chars = 12 → suffix shown
        assert_eq!(mask_token("ghp_12345678"), "ghp_***...5678");
    }

    #[test]
    fn default_for_key_token() {
        assert_eq!(default_for_key(ConfigKey::Token), "(not set)");
    }

    #[test]
    fn default_for_key_cache_dir_is_absolute() {
        let default = default_for_key(ConfigKey::CacheDir);
        let path = PathBuf::from(&default);
        assert!(path.is_absolute());
    }

    #[test]
    fn default_for_key_cache_is_true() {
        assert_eq!(default_for_key(ConfigKey::Cache), "true");
    }

    #[test]
    fn validate_key_known() {
        assert!(validate_key("token").is_ok());
        assert!(validate_key("cache-dir").is_ok());
        assert!(validate_key("cache").is_ok());
    }

    #[test]
    fn validate_key_unknown() {
        assert!(validate_key("bad-key").is_err());
    }
}
