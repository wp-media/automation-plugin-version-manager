//! APVM CLI - Automation Plugin Version Manager
//!
//! Command-line interface for building and managing WordPress plugin versions.

mod commands;
mod defaults;
mod paths;
mod sanitize;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use apvm_core::config_io::load_config_file;
use apvm_core::{Apvm, Result};

use crate::commands::{BuildArgs, ConfigArgs, InfoArgs};
use crate::paths::Paths;

// ─────────────────────────────────────────────────────────────────────────────
// CLI Definition
// ─────────────────────────────────────────────────────────────────────────────

/// Automation Plugin Version Manager - Build and manage WordPress plugins
#[derive(Parser, Debug)]
#[command(name = "apvm")]
#[command(version, about, before_help = concat!("Author: ", env!("CARGO_PKG_AUTHORS")))]
#[command(propagate_version = true)]
struct Cli {
    /// Enable verbose output (shows commands and full output)
    #[arg(long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

/// Available commands
#[derive(Subcommand, Debug)]
enum Commands {
    /// Build a plugin from a git reference (PR, branch, tag, commit)
    Build(BuildArgs),
    /// List all available plugins
    List,
    /// Show detailed information about a plugin
    Info(InfoArgs),
    /// View or change configuration settings
    Config(ConfigArgs),
}

// ─────────────────────────────────────────────────────────────────────────────
// Main Entry Point
// ─────────────────────────────────────────────────────────────────────────────

/// Main entry point with async runtime.
///
/// Uses `tokio::main` for async operations (git, GitHub API, etc.).
/// The `current_thread` flavor is sufficient for CLI tools and has lower overhead.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Initialize tracing (only if RUST_LOG is set)
    init_tracing();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {}", e);
            ExitCode::FAILURE
        }
    }
}

/// Run the CLI application.
///
/// Separated from main() to allow proper error handling with ExitCode.
async fn run() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Load paths and default configuration
    let paths = Paths::new(
        defaults::default_apvm_dir().clone(),
        defaults::default_builds_dir().clone(),
    );
    let default_config = paths.to_config();

    // Config command doesn't need APVM instance — handle it early
    if let Commands::Config(args) = &cli.command {
        return args.execute(&paths);
    }

    // Load config: file values override defaults, missing fields use defaults
    let config = load_config_file(paths.config_file(), &default_config)?;

    // Create APVM instance with automatic token resolution
    // This will try: config → GITHUB_TOKEN → GH_TOKEN → gh CLI
    let apvm = Apvm::new_with_token_resolution(config).await?;

    // Log token source for debugging
    if let Some(source) = apvm.token_source() {
        tracing::info!("GitHub token from: {}", source);
    } else {
        tracing::debug!("No GitHub token found, using anonymous mode");
    }

    // Dispatch to the appropriate command
    match cli.command {
        Commands::Build(args) => {
            args.execute(&apvm, cli.verbose).await?;
        }
        Commands::List => {
            commands::list::execute(&apvm);
        }
        Commands::Info(args) => {
            args.execute(&apvm)?;
        }
        Commands::Config(_) => unreachable!("handled above"),
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Initialization
// ─────────────────────────────────────────────────────────────────────────────

/// Initialize tracing/logging.
///
/// Enables if `RUST_LOG` environment variable is set OR if `--verbose` flag is used.
/// This keeps normal CLI output clean while allowing debug output when needed.
///
/// # Examples
///
/// ```bash
/// # Normal usage (quiet — spinner only)
/// apvm build backwpup 123
///
/// # Verbose (shows commands and output)
/// apvm -v build backwpup 123
///
/// # Debug mode (full tracing)
/// RUST_LOG=debug apvm build backwpup 123
///
/// # Trace mode (maximum verbosity)
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
