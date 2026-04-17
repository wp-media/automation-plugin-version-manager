//! Build command implementation.
//!
//! Handles building plugins from git references (PR, branch, tag, commit).
//! Provides visual progress via an indicatif spinner in normal mode, or
//! full output in verbose mode (`-v`/`--verbose`).

use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};

use apvm_core::build::plugins::VersionRequirement;
use apvm_core::build::progress::{BuildEvent, ClosureReporter};
use apvm_core::{Apvm, Result};

/// Arguments for the build command.
#[derive(Args, Debug)]
#[command(after_long_help = "\
\x1b[1;4mGit Reference Formats:\x1b[0m

  The <GIT_REF> argument accepts many formats. APVM auto-detects the type,
  or you can use an explicit prefix for disambiguation.

  \x1b[1mAutomatic detection (no prefix):\x1b[0m
    123               PR #123 (or branch if PR doesn't exist)
    develop           Branch name
    v1.0.0            Tag (if exists) or branch
    abc1234           Commit SHA (7-40 hex chars)
    5.6.8             Version → tries GitHub Release first, then tag/branch

  \x1b[1mExplicit prefixes:\x1b[0m
    pr:123            Force PR interpretation
    branch:main       Force branch interpretation
    tag:v1.0.0        Force tag interpretation
    commit:abc1234    Force commit interpretation
    release:v5.6.8    Download pre-built assets from a specific GitHub Release

\x1b[1;4mSpecial Keywords (tags):\x1b[0m

  Tags are sorted by creation date (most recent first).

  \x1b[1mStable (excludes -alpha, -beta, -rc tags):\x1b[0m
    tag:latest-stable       Latest stable tag
    tag:previous-stable     Previous stable tag

  \x1b[1mAny (includes prereleases):\x1b[0m
    tag:latest              Very latest tag (any kind)
    tag:previous-latest     Tag right before the latest

\x1b[1;4mSpecial Keywords (releases):\x1b[0m

  Releases are fetched from the GitHub Releases API.
  Drafts are always excluded.

  \x1b[1mStable (excludes prereleases):\x1b[0m
    release:latest-stable   Latest stable release (non-prerelease, non-draft)
    release:previous-stable Previous stable release

  \x1b[1mAny (includes prereleases):\x1b[0m
    release:latest          Very latest non-draft release
    release:previous-latest Previous non-draft release
")]
pub struct BuildArgs {
    /// Plugin name (e.g., "backwpup")
    pub plugin: String,

    /// Git reference: PR number, branch, tag, commit, or release
    #[arg(long_help = "Git reference to build from.\n\n\
            Supports automatic detection or explicit prefixes:\n\
            \x20 123, #123             → Build from PR #123\n\
            \x20 develop               → Build from branch\n\
            \x20 tag:v5.0.0            → Build from specific tag\n\
            \x20 tag:latest-stable     → Latest stable tag (no alpha/beta/rc)\n\
            \x20 tag:previous-stable   → Previous stable tag\n\
            \x20 tag:latest            → Very latest tag (including prereleases)\n\
            \x20 tag:previous-latest   → Tag before the very latest\n\
            \x20 abc1234               → Build from commit SHA\n\
            \x20 release:5.6.8         → Download pre-built GitHub Release\n\
            \x20 release:latest-stable → Latest stable release\n\
            \x20 release:latest        → Very latest non-draft release\n\n\
            Run 'apvm build --help' for the full reference guide.")]
    pub git_ref: String,

    /// Package version (default: plugin-specific, e.g., 9.99.99 for BackWPup)
    #[arg(id = "pkg_version", short = 'v', long = "ver")]
    pub version: Option<String>,

    /// Comma-separated variants to build (default: plugin-specific)
    ///
    /// Example: --variants free,pro-en
    #[arg(long, value_delimiter = ',')]
    pub variants: Option<Vec<String>>,

    /// Output directory for build artifacts
    #[arg(default_value = ".")]
    pub output: PathBuf,
}

impl BuildArgs {
    /// Normalize git ref input.
    ///
    /// Strips `#` prefix from PR numbers (e.g., `#123` → `123`).
    /// Everything else passes through unchanged — the library handles
    /// automatic detection of PRs, branches, tags, and commits.
    pub fn normalize_ref(&self) -> String {
        let trimmed = self.git_ref.trim();

        // Strip #123 → 123 (library auto-detects bare digits as PR)
        if let Some(num) = trimmed.strip_prefix('#')
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit())
        {
            return num.to_string();
        }

        trimmed.to_string()
    }

    /// Execute the build command.
    ///
    /// In normal mode, displays a spinner with step descriptions.
    /// In verbose mode (`-v`), prints full command details and output.
    pub async fn execute(&self, apvm: &Apvm, verbose: bool) -> Result<()> {
        // 1. Look up the project to access builder defaults
        let project = apvm.registry.get(&self.plugin)?;
        let builder = project.builder.as_ref();

        // 2. Determine version: pass only the user's explicit --ver to core.
        //    Builder defaults (e.g., 9.99.99) are handled by core's resolve_version()
        //    so that download_release() can distinguish "user provided" from "no version".
        let version = self.version.clone();

        // 3. Check for unnecessary version parameter
        if self.version.is_some() && builder.version_requirement() == VersionRequirement::Embedded {
            eprintln!(
                "Warning: --ver ignored for '{}' (version is embedded in source)",
                self.plugin
            );
        }

        // 4. Determine variants (CLI arg > builder default > all)
        let variants = self.resolve_variants(builder);

        // 5. Check for unnecessary variants parameter
        if self.variants.is_some() && !builder.has_variants() {
            eprintln!(
                "Warning: --variants ignored for '{}' (single-variant plugin)",
                self.plugin
            );
        }

        // 6. Resolve output directory to absolute path
        //    canonicalize() expands "." to full path and resolves symlinks
        let output_dir = self.output.canonicalize().unwrap_or_else(|_| {
            // If canonicalize fails (path doesn't exist), use absolute path
            std::env::current_dir()
                .map(|cwd| cwd.join(&self.output))
                .unwrap_or_else(|_| self.output.clone())
        });

        // 7. Normalize the git ref
        let git_ref = self.normalize_ref();

        // 8. Show what we're building
        println!("Building {} from {}", self.plugin, git_ref);
        if let Some(ref v) = version {
            println!("  Version: {}", v);
        }
        if !variants.is_empty() {
            println!("  Variants: {}", variants.join(", "));
        }
        println!("  Output: {}", output_dir.display());
        println!();

        // 9. Execute the build with appropriate reporter
        let variants_refs: Vec<&str> = variants.iter().map(|s| s.as_str()).collect();

        let result = if verbose {
            // Verbose mode: print everything, no spinner
            let reporter = ClosureReporter::new(|event| match event {
                BuildEvent::PhaseStarted { phase, message } => {
                    eprintln!("[{}] {}", phase, message);
                }
                BuildEvent::StepStarted { step } => {
                    eprintln!("  > {} ({})", step.label, step.command);
                }
                BuildEvent::StepCompleted { step } => {
                    eprintln!("  ✓ {}", step.label);
                }
                BuildEvent::CommandOutput { stream, line } => {
                    eprintln!("  [{}] {}", stream, line);
                }
                BuildEvent::Warning(msg) => {
                    eprintln!("  ⚠ {}", msg);
                }
                BuildEvent::BuildSucceeded { .. } => {
                    eprintln!("[Done] Build succeeded");
                }
                BuildEvent::BuildFailed { reason } => {
                    eprintln!("[FAIL] {}", reason);
                }
                _ => {}
            });

            apvm.build(
                &self.plugin,
                version.as_deref(),
                &git_ref,
                if variants_refs.is_empty() {
                    None
                } else {
                    Some(&variants_refs)
                },
                &output_dir,
                &reporter,
            )
            .await?
        } else {
            // Normal mode: spinner with step descriptions
            let spinner = ProgressBar::new_spinner();
            spinner.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg}")
                    .unwrap()
                    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
            );
            spinner.enable_steady_tick(Duration::from_millis(80));
            spinner.set_message("Starting build...");

            let reporter = ClosureReporter::new({
                let sp = spinner.clone();
                move |event| match event {
                    BuildEvent::PhaseStarted { message, .. } => {
                        sp.set_message(message.clone());
                    }
                    BuildEvent::StepStarted { step } => {
                        sp.set_message(step.label.clone());
                    }
                    BuildEvent::Warning(msg) => {
                        sp.suspend(|| {
                            eprintln!("⚠ {}", msg);
                        });
                    }
                    BuildEvent::BuildFailed { reason } => {
                        sp.finish_and_clear();
                        eprintln!("Build failed: {}", reason);
                    }
                    _ => {}
                }
            });

            let result = apvm
                .build(
                    &self.plugin,
                    version.as_deref(),
                    &git_ref,
                    if variants_refs.is_empty() {
                        None
                    } else {
                        Some(&variants_refs)
                    },
                    &output_dir,
                    &reporter,
                )
                .await;

            spinner.finish_and_clear();
            result?
        };

        // 10. Show results
        println!("Build complete: {}", result.description());
        println!("  Commit: {}", result.commit_short);
        println!("  Version: {}", result.result.version);
        println!("  Artifacts:");
        for artifact in &result.result.artifacts {
            println!("    - {}", artifact.path.display());
        }

        Ok(())
    }

    /// Resolve variants to use.
    ///
    /// Priority: CLI argument > builder default > empty (all)
    fn resolve_variants(&self, builder: &dyn apvm_core::build::plugins::Builder) -> Vec<String> {
        self.variants.clone().unwrap_or_else(|| {
            builder
                .default_variants()
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_args(git_ref: &str) -> BuildArgs {
        BuildArgs {
            plugin: "test".to_string(),
            git_ref: git_ref.to_string(),
            version: None,
            variants: None,
            output: PathBuf::from("."),
        }
    }

    #[test]
    fn test_normalize_ref_hash_pr() {
        assert_eq!(build_args("#123").normalize_ref(), "123");
        assert_eq!(build_args("#1").normalize_ref(), "1");
        assert_eq!(build_args("#99999").normalize_ref(), "99999");
    }

    #[test]
    fn test_normalize_ref_bare_number() {
        // Bare numbers pass through — library auto-detects as PR
        assert_eq!(build_args("123").normalize_ref(), "123");
        assert_eq!(build_args("1").normalize_ref(), "1");
        assert_eq!(build_args("99999").normalize_ref(), "99999");
    }

    #[test]
    fn test_normalize_ref_passthrough() {
        // Branches
        assert_eq!(build_args("develop").normalize_ref(), "develop");
        assert_eq!(build_args("feature/foo").normalize_ref(), "feature/foo");

        // Already prefixed
        assert_eq!(build_args("pr:123").normalize_ref(), "pr:123");
        assert_eq!(build_args("tag:v1.0.0").normalize_ref(), "tag:v1.0.0");
        assert_eq!(build_args("branch:main").normalize_ref(), "branch:main");
        assert_eq!(build_args("commit:abc123").normalize_ref(), "commit:abc123");

        // Tags
        assert_eq!(build_args("v1.0.0").normalize_ref(), "v1.0.0");

        // Commits
        assert_eq!(build_args("abc1234").normalize_ref(), "abc1234");
    }

    #[test]
    fn test_normalize_ref_edge_cases() {
        // Hash with non-numeric part (not a PR)
        assert_eq!(build_args("#abc").normalize_ref(), "#abc");

        // Empty after hash
        assert_eq!(build_args("#").normalize_ref(), "#");

        // Mixed content
        assert_eq!(build_args("123abc").normalize_ref(), "123abc");
    }
}
