//! APVM CLI - Automation Plugin Version Manager

use std::fs;
use std::path::Path;

use apvm_config::{Config, Paths};
use apvm_core::Result;

fn main() -> Result<()> {
    // Only enable tracing if RUST_LOG is set (for debugging)
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init();
    }

    let paths = Paths::default();
    let config = load_config(paths.config_file())?;

    // User-facing output: simple println
    println!("APVM initialized");

    // Debug output: only shown with RUST_LOG=debug
    tracing::debug!("Config file: {:?}", paths.config_file());
    tracing::debug!("Cache dir: {:?}", config.cache_dir);
    tracing::debug!("Builds dir: {:?}", config.builds_dir);

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Config File I/O (CLI's responsibility)
// ─────────────────────────────────────────────────────────────────────────────

/// Load configuration from a file.
///
/// Returns `Config::default()` if the file doesn't exist.
fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        tracing::debug!("Config file not found, using defaults");
        return Ok(Config::default());
    }

    let content = fs::read_to_string(path)?;
    let config: Config = serde_json::from_str(&content)?;
    tracing::debug!("Loaded config from {:?}", path);
    Ok(config)
}

/// Save configuration to a file.
///
/// Creates parent directories if they don't exist.
#[allow(dead_code)]
fn save_config(config: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let content = serde_json::to_string_pretty(config)?;
    fs::write(path, content)?;
    tracing::debug!("Saved config to {:?}", path);
    Ok(())
}