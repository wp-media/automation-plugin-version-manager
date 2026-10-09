//! APVM CLI - Automation Plugin Version Manager
//!
//! Command-line interface for building and managing WordPress plugin versions.

mod broken_pipe;
mod color;
mod commands;
mod defaults;
mod paths;
mod sanitize;
mod status;
mod update_check;

use std::io::Write;
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

/// Main entry point: builds the async runtime and runs the command.
///
/// A `current_thread` Tokio runtime is enough for a CLI (git, GitHub API,
/// builds) and has lower overhead. It is built by hand rather than with
/// `#[tokio::main]` so the whole run sits inside [`broken_pipe::run`]: a
/// closed output pipe (`apvm list | head -1`) then unwinds — dropping build
/// workspaces and temp files — and exits quietly with
/// [`broken_pipe::EXIT_CODE`].
fn main() -> ExitCode {
    broken_pipe::install();

    // Initialize tracing (only if RUST_LOG is set)
    init_tracing();

    broken_pipe::run(|| {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(run()),
            Err(e) => {
                eprintln!("Error: could not start the async runtime: {e}");
                ExitCode::FAILURE
            }
        }
    })
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

    // Report the command's own error first... Written without panicking:
    // the command already failed, and that exit status (1) must survive a
    // closed stderr instead of becoming the broken-pipe status.
    if let Err(e) = &result {
        let _ = writeln!(std::io::stderr(), "Error: {e}");
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
/// Enabled only when the `RUST_LOG` environment variable is set (`--verbose`
/// is separate: it shows the build commands' own output). Logs go to stderr,
/// so stdout keeps only the command's output, and are colored only when
/// stderr is a terminal and `NO_COLOR` is not set to a non-empty value.
///
/// # Examples
///
/// ```bash
/// # Normal usage (quiet — spinner only)
/// apvm build backwpup 123
///
/// # Verbose (shows commands and output)
/// apvm build backwpup 123 --verbose
///
/// # Debug mode (full tracing)
/// RUST_LOG=debug apvm build backwpup 123
///
/// # Trace mode (maximum verbosity)
/// RUST_LOG=trace apvm build backwpup 123
/// ```
fn init_tracing() {
    if tracing_requested() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_target(false) // Cleaner output without module paths
            // Diagnostics are not the command's output: keep them off stdout
            // (`RUST_LOG=debug apvm list | grep …` must still work), styled
            // by the same rules as apvm's other stderr text.
            .with_writer(std::io::stderr)
            .with_ansi(color::colors_enabled(color::Stream::Stderr))
            // A failed log write (closed stderr) is dropped: tracing would
            // otherwise report it with `eprintln!` on that same stream and
            // panic — possibly while unwinding, which aborts.
            .log_internal_errors(false)
            .init();
    }
}

/// Whether `RUST_LOG` diagnostics are enabled for this run (the variable is
/// set). Other output adapts to it, e.g. the build spinner steps aside.
pub(crate) fn tracing_requested() -> bool {
    std::env::var("RUST_LOG").is_ok()
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        // clap's own self-check: catches duplicate short/long flags (e.g. a
        // second `-v` next to `--ver`), invalid defaults, broken
        // `conflicts_with` targets and similar wiring mistakes that would
        // otherwise only panic at runtime when the affected command is parsed.
        Cli::command().debug_assert();
    }

    #[test]
    fn update_and_uninstall_parse_without_arguments() {
        // `run` keys the update-notifier opt-out on these exact variants.
        assert!(matches!(
            Cli::try_parse_from(["apvm", "update"]).map(|c| c.command),
            Ok(Commands::Update)
        ));
        assert!(matches!(
            Cli::try_parse_from(["apvm", "uninstall", "--yes"]).map(|c| c.command),
            Ok(Commands::Uninstall { yes: true })
        ));
        assert!(matches!(
            Cli::try_parse_from(["apvm", "uninstall"]).map(|c| c.command),
            Ok(Commands::Uninstall { yes: false })
        ));
    }

    #[test]
    fn verbose_is_global_and_accepted_after_the_subcommand() {
        // `apvm build … --verbose` is the documented form, so the flag must
        // stay `global = true`.
        let cli = Cli::try_parse_from(["apvm", "list", "--verbose"]).expect("parses");
        assert!(cli.verbose);
    }
}
