//! APVM CLI - Automation Plugin Version Manager
//!
//! Command-line interface for building and managing WordPress plugin versions.

use std::fs;
use std::path::Path;

use apvm_config::{Config, Paths};
use apvm_core::{Apvm, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Main Entry Point
// ─────────────────────────────────────────────────────────────────────────────

/// Main entry point with async runtime.
///
/// Uses `tokio::main` for async operations (git, GitHub API, etc.).
/// The `current_thread` flavor is sufficient for CLI tools and has lower overhead.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Initialize tracing (only if RUST_LOG is set)
    init_tracing();

    // Load paths and configuration
    let paths = Paths::default();
    let config = load_config(paths.config_file())?;

    // Create APVM instance with automatic token resolution
    // This will try: config → GITHUB_TOKEN → GH_TOKEN → gh CLI
    let apvm = Apvm::new_with_token_resolution(config).await?;

    // Log token source for debugging
    if let Some(source) = apvm.token_source() {
        tracing::info!("GitHub token from: {}", source);
    } else {
        tracing::debug!("No GitHub token found, using anonymous mode");
    }

    // TODO: Parse CLI arguments and dispatch commands
    // For now, just show initialization status
    println!("APVM initialized");
    println!("  Projects: {}", apvm.registry.list().count());
    println!("  Cache dir: {}", apvm.config.cache_dir.display());
    println!("  Builds dir: {}", apvm.config.builds_dir.display());
    if apvm.has_token() {
        println!("  GitHub: authenticated");
    } else {
        println!("  GitHub: anonymous (rate limited)");
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Initialization
// ─────────────────────────────────────────────────────────────────────────────

/// Initialize tracing/logging.
///
/// Only enables if `RUST_LOG` environment variable is set.
/// This keeps normal CLI output clean while allowing debug output when needed.
///
/// # Examples
///
/// ```bash
/// # Normal usage (quiet)
/// apvm build backwpup 123
///
/// # Debug mode
/// RUST_LOG=debug apvm build backwpup 123
///
/// # Trace mode (verbose)
/// RUST_LOG=trace apvm build backwpup 123
/// ```
fn init_tracing() {
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_target(false) // Cleaner output without module paths
            .init();
    }
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