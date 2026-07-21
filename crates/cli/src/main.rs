//! APVM CLI - Automation Plugin Version Manager
//!
//! Command-line interface for building and managing WordPress plugin versions.

mod color;
mod commands;
mod defaults;
mod paths;
mod sanitize;
mod update_check;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use apvm_core::config_io::load_config_file;
use apvm_core::{Apvm, Result};

use crate::commands::{BuildArgs, CacheArgs, ConfigArgs, InfoArgs, SkillArgs};
use crate::paths::Paths;

// ─────────────────────────────────────────────────────────────────────────────
// CLI Definition
// ─────────────────────────────────────────────────────────────────────────────

/// Combined version string used by `--version`.
///
/// `CARGO_PKG_VERSION` and `CARGO_PKG_AUTHORS` are set by Cargo at compile time
/// from `[package]` in `Cargo.toml`.
/// Source: <https://doc.rust-lang.org/cargo/reference/environment-variables.html>
///
/// Produces output like:
/// ```text
/// apvm 1.2.1
/// Sandy Figueroa <sandy@wp-media.me>
/// ```
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\n", env!("CARGO_PKG_AUTHORS"));

/// Automation Plugin Version Manager - Build and manage WordPress plugins
#[derive(Parser, Debug)]
#[command(name = "apvm")]
#[command(version = VERSION, about, before_help = concat!("Author: ", env!("CARGO_PKG_AUTHORS")))]
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
    /// Build a plugin from a git reference (PR, branch, tag, commit, release)
    Build(BuildArgs),
    /// List all available plugins
    List,
    /// Show detailed information about a plugin
    Info(InfoArgs),
    /// Inspect and maintain the artifact cache
    Cache(CacheArgs),
    /// View or change configuration settings
    Config(ConfigArgs),
    /// Install or remove the Claude Code skill for apvm (project-local or global)
    Skill(SkillArgs),
    /// Update apvm to the latest version
    Update,
    /// Uninstall apvm from this system
    Uninstall {
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
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

    run().await
}

/// Run one CLI invocation end to end.
///
/// Kicks off the non-blocking background update check up front (so it overlaps
/// the command's own work), dispatches the requested command, reports its
/// error if any, and — as the very last output — surfaces an available-update
/// notice via [`update_check`]. Owns the final [`ExitCode`] so ordering between
/// the command's error and the update notice is guaranteed.
async fn run() -> ExitCode {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Load paths (config + notifier state live under the APVM home)
    let paths = Paths::new(defaults::default_apvm_dir().clone());

    // Start the background update check before running the command so the two
    // overlap. `update` (does its own check) and `uninstall` (about to remove
    // apvm) never trigger a notice.
    let command_eligible = !matches!(cli.command, Commands::Update | Commands::Uninstall { .. });
    let checker = update_check::maybe_start(&paths, command_eligible);

    // Run the requested command.
    let result = dispatch(cli, &paths).await;

    // Report the command's own error first...
    if let Err(e) = &result {
        eprintln!("Error: {e}");
    }

    // ...then the update notice, as the last thing printed. Skipped entirely
    // when no check was started (ineligible command, non-TTY, or opted out).
    if let Some(checker) = checker {
        checker.finish_and_report().await;
    }

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Dispatch to the appropriate command handler.
///
/// Separated from [`run`] so the update-notifier orchestration stays free of
/// per-command branching. Each handler owns its user-facing success output;
/// this returns the command's `Result` for [`run`] to map to an exit code.
async fn dispatch(cli: Cli, paths: &Paths) -> Result<()> {
    // Default configuration derived from the standard paths.
    let default_config = paths.to_config();

    // Config command doesn't need APVM instance — handle it early
    if let Commands::Config(args) = &cli.command {
        return args.execute(paths);
    }

    // Update command has its own GitHub client — handle it early
    if matches!(cli.command, Commands::Update) {
        return commands::update::execute(paths).await;
    }

    // Skill command is self-contained (embedded files, no network) — handle
    // it early
    if let Commands::Skill(args) = &cli.command {
        return args.execute(cli.verbose);
    }

    // Uninstall command is self-contained — handle it early
    if let Commands::Uninstall { yes } = &cli.command {
        return commands::uninstall::execute(*yes);
    }

    // Load config: file values override defaults, missing fields use defaults.
    // Then let APVM_CACHE_DIR (if set) override the cache directory — applied
    // here, before any command runs, so both the `cache` command (which uses
    // the directory directly) and builds see the same effective location.
    let config = apvm_core::config_io::apply_env_overrides(load_config_file(
        paths.config_file(),
        &default_config,
    )?);

    // Cache maintenance operates on the resolved cache directory and needs no
    // GitHub client — handle it before creating the APVM instance. It works
    // regardless of the `cache` on/off setting.
    if let Commands::Cache(args) = &cli.command {
        return args.execute(&config.cache_dir);
    }

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
        Commands::Cache(_) => unreachable!("handled above"),
        Commands::Config(_) => unreachable!("handled above"),
        Commands::Skill(_) => unreachable!("handled above"),
        Commands::Update => unreachable!("handled above"),
        Commands::Uninstall { .. } => unreachable!("handled above"),
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
