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
use apvm_core::{Apvm, ArtifactOrigin, ProducedArtifact, RefSource, ResolvedRef, Result};

use crate::color::{self, Color};

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

    /// Bypass the artifact cache for this build (still builds and warms the
    /// cache unless caching is disabled in config)
    #[arg(long)]
    pub no_cache: bool,

    /// Require a cache hit to match the requested version (`--ver`) exactly,
    /// rebuilding instead of serving a different cached version
    ///
    /// Modifies how the cache is read, so it cannot be combined with
    /// `--no-cache` (which skips cache reads entirely).
    #[arg(long, conflicts_with = "no_cache")]
    pub strict_version: bool,
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
        // The resolved reference (branch/tag/commit/PR/release) is printed once
        // it is known — inside the build, via the ReferenceResolved event — so
        // it joins this header block above the spinner. A blank line is emitted
        // before the results summary instead of here.

        // 9. Assemble the build request, including per-invocation cache
        //    overrides.
        let request =
            apvm_core::BuildRequest::new(self.plugin.clone(), git_ref, output_dir.clone())
                .version(version.clone())
                .variants(variants.clone())
                .no_cache(self.no_cache)
                .strict_version(self.strict_version);

        let result = if verbose {
            // Verbose mode: print everything, no spinner
            let reporter = ClosureReporter::new(|event| match event {
                BuildEvent::ReferenceResolved { resolved } => {
                    eprintln!("Reference: {}", reference_line(resolved));
                }
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

            apvm.build(request, &reporter).await?
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
                    BuildEvent::ReferenceResolved { resolved } => {
                        // Print above the spinner (suspend hides the bar, prints,
                        // then redraws) so the line persists in the header block.
                        let line = reference_line(resolved);
                        sp.suspend(|| {
                            println!("  Reference: {}", line);
                        });
                    }
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

            let result = apvm.build(request, &reporter).await;

            spinner.finish_and_clear();
            result?
        };

        // 10. Show results, including where each artifact came from.
        //     Blank line separates the live/header area from the summary.
        println!();
        println!("Build complete: {}", result.description());
        println!("  Commit:  {}", result.commit_short);
        println!("  Version: {}", result.result.version);
        println!("  Artifacts:");
        for artifact in &result.result.artifacts {
            // Pad the plain label first so ANSI codes never affect alignment.
            let label = format!("{:<10}", artifact.origin.to_string());
            println!(
                "    {} {}",
                color::paint(&label, origin_color(artifact.origin)),
                artifact.filename
            );
        }

        // A cache hit that returned a different version than the user pinned
        // is the one genuinely surprising case — draw attention to it (yellow)
        // and offer the remedy, above the neutral provenance summary.
        if result.cache_version_mismatch {
            let requested = result.requested_version.as_deref().unwrap_or("(requested)");
            let got = &result.result.version;
            let msg = format!(
                "⚠ Requested version {requested} but the cache holds this commit as {got}.\n  \
                 Returning the cached {got} artifacts. Pass --strict-version to rebuild at {requested}, if specific version is required.",
            );
            println!("{}", color::paint(&msg, Color::Yellow));
        }

        println!("  Source: {}", source_summary(&result.result.artifacts));

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

/// One-line, human-readable form of a resolved reference for the pre-build
/// header, e.g. `branch 'develop' @ 3fb4102`, `tag 'v2.2.9' @ 2c3b49d`,
/// `PR #123 (branch: feature/x) @ abc1234`, or `release 'v2.3.0'`.
///
/// Mirrors the results summary's `description()` format. The short commit is
/// appended only when it adds information: a `commit` reference already shows
/// its SHA in the description, and a `release` carries no commit at this stage.
fn reference_line(resolved: &ResolvedRef) -> String {
    let description = resolved.detailed_description();
    match (&resolved.source, resolved.commit_sha.as_deref()) {
        (RefSource::Commit(_), _) | (_, None) => description,
        (_, Some(sha)) => {
            let short: String = sha.chars().take(7).collect();
            format!("{description} @ {short}")
        }
    }
}

/// The informational color for an artifact's provenance. Cache reuse is
/// expected/good news, so it is a neutral accent — not a warning color.
fn origin_color(origin: ArtifactOrigin) -> Color {
    match origin {
        ArtifactOrigin::Built => Color::Green,
        ArtifactOrigin::Cache => Color::Cyan,
        ArtifactOrigin::Downloaded => Color::Blue,
    }
}

/// One-line provenance summary for the delivered artifacts, e.g.
/// `all from cache`, `all built`, or `1 built, 2 from cache`.
fn source_summary(artifacts: &[ProducedArtifact]) -> String {
    let (mut built, mut cached, mut downloaded) = (0usize, 0usize, 0usize);
    for artifact in artifacts {
        match artifact.origin {
            ArtifactOrigin::Built => built += 1,
            ArtifactOrigin::Cache => cached += 1,
            ArtifactOrigin::Downloaded => downloaded += 1,
        }
    }
    let total = artifacts.len();
    if total == 0 {
        return "no artifacts".to_string();
    }
    if built == total {
        return "all built".to_string();
    }
    if cached == total {
        return "all from cache".to_string();
    }
    if downloaded == total {
        return "all downloaded".to_string();
    }
    let mut parts = Vec::new();
    if built > 0 {
        parts.push(format!("{built} built"));
    }
    if cached > 0 {
        parts.push(format!("{cached} from cache"));
    }
    if downloaded > 0 {
        parts.push(format!("{downloaded} downloaded"));
    }
    parts.join(", ")
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
            no_cache: false,
            strict_version: false,
        }
    }

    fn artifact(origin: ArtifactOrigin) -> ProducedArtifact {
        ProducedArtifact::new(None, PathBuf::from("/x.zip"), "x.zip".to_string(), 1)
            .with_origin(origin)
    }

    #[test]
    fn source_summary_all_one_kind() {
        assert_eq!(
            source_summary(&[artifact(ArtifactOrigin::Built)]),
            "all built"
        );
        assert_eq!(
            source_summary(&[artifact(ArtifactOrigin::Cache)]),
            "all from cache"
        );
        assert_eq!(
            source_summary(&[artifact(ArtifactOrigin::Downloaded)]),
            "all downloaded"
        );
    }

    #[test]
    fn source_summary_mixed_partial() {
        let mixed = [
            artifact(ArtifactOrigin::Built),
            artifact(ArtifactOrigin::Cache),
            artifact(ArtifactOrigin::Cache),
        ];
        assert_eq!(source_summary(&mixed), "1 built, 2 from cache");
    }

    #[test]
    fn source_summary_empty() {
        assert_eq!(source_summary(&[]), "no artifacts");
    }

    // =========================================================================
    // reference_line — pre-build resolved-reference display
    // =========================================================================

    fn resolved(source: RefSource, git_ref: &str, commit: Option<&str>) -> ResolvedRef {
        ResolvedRef {
            input: git_ref.to_string(),
            source,
            git_ref: git_ref.to_string(),
            commit_sha: commit.map(str::to_string),
        }
    }

    #[test]
    fn reference_line_branch_with_commit_appends_short_sha() {
        let r = resolved(
            RefSource::Branch("develop".into()),
            "develop",
            Some("3fb4102e5967d1a088916fc8f70e8034765b8455"),
        );
        assert_eq!(reference_line(&r), "branch 'develop' @ 3fb4102");
    }

    #[test]
    fn reference_line_tag_with_commit_appends_short_sha() {
        let r = resolved(
            RefSource::Tag("v2.2.9".into()),
            "v2.2.9",
            Some("2c3b49d0000000000000000000000000000000a"),
        );
        assert_eq!(reference_line(&r), "tag 'v2.2.9' @ 2c3b49d");
    }

    #[test]
    fn reference_line_commit_does_not_duplicate_sha() {
        // The commit description already contains the short SHA — no " @ ..." suffix.
        let r = resolved(
            RefSource::Commit("a1b2c3d4e5f6".into()),
            "a1b2c3d4e5f6",
            Some("a1b2c3d4e5f6"),
        );
        assert_eq!(reference_line(&r), "commit a1b2c3d");
    }

    #[test]
    fn reference_line_release_without_commit() {
        let r = resolved(RefSource::Release("v2.3.0".into()), "v2.3.0", None);
        assert_eq!(reference_line(&r), "release 'v2.3.0'");
    }

    #[test]
    fn reference_line_pr_includes_branch_and_commit() {
        let r = resolved(
            RefSource::PullRequest(123),
            "feature/x",
            Some("abc1234def56"),
        );
        assert_eq!(reference_line(&r), "PR #123 (branch: feature/x) @ abc1234");
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
