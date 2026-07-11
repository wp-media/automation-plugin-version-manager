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
    ///
    /// Ignored (with a warning) when `--warm-cache` is set: warming delivers
    /// nothing to an output directory.
    #[arg(default_value = ".")]
    pub output: PathBuf,

    /// Bypass the artifact cache for this build (still builds and warms the
    /// cache unless caching is disabled in config)
    #[arg(long)]
    pub no_cache: bool,

    /// Warm the artifact cache instead of producing output
    ///
    /// Runs the exact same pipeline as a normal build — resolve the reference,
    /// reuse whatever is already cached, and build or download only what is
    /// missing — then stores everything into the cache. The one difference is
    /// that it delivers nothing to an output directory; its purpose is to prime
    /// the cache so a later build of the same reference is an instant hit.
    ///
    /// Cannot be combined with `--no-cache` (warming *is* a cache operation). An
    /// output directory is accepted but ignored (with a warning).
    #[arg(long = "warm-cache", conflicts_with = "no_cache")]
    pub warm_cache: bool,
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

    /// Whether `--output`/`[OUTPUT]` was passed explicitly rather than left at
    /// its default (`.`).
    ///
    /// Used to decide whether to warn that `--warm-cache` ignores it. This is a
    /// textual comparison against the default value — a user who explicitly types `.`
    /// is (harmlessly) treated the same as one who passed nothing, since the
    /// resulting behavior is identical either way.
    fn output_was_explicit(&self) -> bool {
        self.output != std::path::Path::new(".")
    }

    /// Whether `--ver` is structurally ignored by `builder` — its version
    /// comes only from source, never from a build parameter. Known before any
    /// network or build work, unlike a release/cache version mismatch (which
    /// depends on ref resolution and is reported later instead).
    fn version_ignored(&self, builder: &dyn apvm_core::build::plugins::Builder) -> bool {
        self.version.is_some() && builder.version_requirement() == VersionRequirement::Embedded
    }

    /// Whether `--variants` is structurally ignored by `builder` — a
    /// single-variant plugin has nothing to select between.
    fn variants_ignored(&self, builder: &dyn apvm_core::build::plugins::Builder) -> bool {
        self.variants.is_some() && !builder.has_variants()
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

        // 3. Check for unnecessary version parameter. Captured as a bool (not
        //    just a warning) so the header below can skip echoing a value that
        //    will not be honored.
        let version_ignored = self.version_ignored(builder);
        if version_ignored {
            eprintln!(
                "Warning: --ver ignored for '{}' (version is embedded in source)",
                self.plugin
            );
        }

        // 4. Determine variants (CLI arg > builder default > all)
        let variants = self.resolve_variants(builder);

        // 5. Check for unnecessary variants parameter (same treatment as
        //    version above).
        let variants_ignored = self.variants_ignored(builder);
        if variants_ignored {
            eprintln!(
                "Warning: --variants ignored for '{}' (single-variant plugin)",
                self.plugin
            );
        }

        // 6. Resolve output directory to an absolute path. Warming delivers
        //    nothing, so an explicitly passed output directory is accepted but
        //    ignored — flagged with a warning rather than rejected outright.
        //    canonicalize() expands "." to a full path and resolves symlinks.
        if self.warm_cache && self.output_was_explicit() {
            eprintln!(
                "Note: output directory '{}' is ignored with --warm-cache (nothing is delivered)",
                self.output.display()
            );
        }
        let output_dir = if self.warm_cache {
            PathBuf::new()
        } else {
            self.output.canonicalize().unwrap_or_else(|_| {
                // If canonicalize fails (path doesn't exist), use absolute path
                std::env::current_dir()
                    .map(|cwd| cwd.join(&self.output))
                    .unwrap_or_else(|_| self.output.clone())
            })
        };

        // 7. Normalize the git ref
        let git_ref = self.normalize_ref();

        // 8. Show what we're doing.
        if self.warm_cache {
            println!("Warming cache for {} from {}", self.plugin, git_ref);
        } else {
            println!("Building {} from {}", self.plugin, git_ref);
        }
        // Only echo parameters that will actually be honored — one that was
        // just flagged as ignored above must not then appear here as if it
        // were in effect.
        if let Some(ref v) = version
            && !version_ignored
        {
            println!("  Version: {}", v);
        }
        if !variants.is_empty() && !variants_ignored {
            println!("  Variants: {}", variants.join(", "));
        }
        // Warming has no output directory to report.
        if !self.warm_cache {
            println!("  Output: {}", output_dir.display());
        }
        // The resolved reference (branch/tag/commit/PR/release) is printed once
        // it is known — inside the build, via the ReferenceResolved event — so
        // it joins this header block above the spinner. A blank line is emitted
        // before the results summary instead of here.

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

            self.dispatch(
                apvm,
                version.clone(),
                variants.clone(),
                git_ref.clone(),
                output_dir.clone(),
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

            let result = self
                .dispatch(
                    apvm,
                    version.clone(),
                    variants.clone(),
                    git_ref.clone(),
                    output_dir.clone(),
                    &reporter,
                )
                .await;

            spinner.finish_and_clear();
            result?
        };

        // 10. Show results. A blank line separates the live/header area from
        //     the summary, which differs for a build vs. a cache warm.
        println!();
        if self.warm_cache {
            Self::print_warm_summary(&result);
        } else {
            Self::print_build_summary(&result);
        }

        Ok(())
    }

    /// Run the requested operation — a normal build, or a cache warm when
    /// `--warm-cache` was passed — sharing the caller's progress reporter.
    ///
    /// Warming builds a [`apvm_core::WarmRequest`] (no output directory, no
    /// `--no-cache`, version always pinned) and delivers nothing to
    /// `output_dir`; a build builds a [`apvm_core::BuildRequest`] with the
    /// per-invocation cache overrides. Both return a
    /// [`apvm_core::BuildOutput`].
    async fn dispatch(
        &self,
        apvm: &Apvm,
        version: Option<String>,
        variants: Vec<String>,
        git_ref: String,
        output_dir: PathBuf,
        reporter: &dyn apvm_core::build::progress::ProgressReporter,
    ) -> Result<apvm_core::BuildOutput> {
        if self.warm_cache {
            let request = apvm_core::WarmRequest::new(self.plugin.clone(), git_ref)
                .version(version)
                .variants(variants);
            apvm.warm_cache(request, reporter).await
        } else {
            let request = apvm_core::BuildRequest::new(self.plugin.clone(), git_ref, output_dir)
                .version(version)
                .variants(variants)
                .no_cache(self.no_cache);
            apvm.build(request, reporter).await
        }
    }

    /// Print a one-line notice when the build rewrote the plugin's source
    /// version to the requested `--ver` (WP Rocket / Imagify). Nothing is
    /// printed when no override happened.
    fn print_version_override(result: &apvm_core::BuildOutput) {
        if let Some(vo) = &result.version_override {
            let msg = format!(
                "  Overrode source version {} → {} in {} ({})",
                vo.from,
                vo.to,
                vo.file,
                vo.sites.join(", ")
            );
            println!("{}", color::paint(&msg, Color::Cyan));
        }
    }

    /// Print the results summary for a normal build.
    fn print_build_summary(result: &apvm_core::BuildOutput) {
        println!("Build complete: {}", result.description());
        println!("  Commit:  {}", result.commit_short);
        println!("  Version: {}", result.result.version);
        Self::print_version_override(result);
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

        println!("  Source: {}", source_summary(&result.result.artifacts));
    }

    /// Print the results summary for a cache warm.
    ///
    /// Nothing was delivered to an output directory — every listed artifact is
    /// now in the cache. The per-artifact label distinguishes what was already
    /// cached (`reused`) from what had to be `built` or `downloaded` to warm it.
    fn print_warm_summary(result: &apvm_core::BuildOutput) {
        println!("Cache warmed: {}", result.description());
        println!("  Commit:  {}", result.commit_short);
        println!("  Version: {}", result.result.version);
        Self::print_version_override(result);
        println!("  Artifacts (now cached):");
        for artifact in &result.result.artifacts {
            // Pad the plain label first so ANSI codes never affect alignment.
            let label = format!("{:<10}", warm_origin_label(artifact.origin));
            println!(
                "    {} {}",
                color::paint(&label, origin_color(artifact.origin)),
                artifact.filename
            );
        }
        println!("  Summary: {}", warm_summary(&result.result.artifacts));
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

/// The label shown for an artifact's provenance in a cache-warm summary.
///
/// A warm always ends with the artifact cached, so the label describes how it
/// got there: `reused` (already cached), `built` (built to warm the cache), or
/// `downloaded` (fetched from a release to warm the cache).
fn warm_origin_label(origin: ArtifactOrigin) -> &'static str {
    match origin {
        ArtifactOrigin::Cache => "reused",
        ArtifactOrigin::Built => "built",
        ArtifactOrigin::Downloaded => "downloaded",
    }
}

/// One-line summary for a cache warm, e.g. `already fully cached`,
/// `2 built (now cached)`, or `1 reused, 1 built (now cached)`.
///
/// When every artifact was `reused` the cache was already complete and the
/// warm was a no-op; otherwise the freshly built/downloaded artifacts are the
/// ones that were added, and the whole set is now cached.
fn warm_summary(artifacts: &[ProducedArtifact]) -> String {
    let (mut built, mut reused, mut downloaded) = (0usize, 0usize, 0usize);
    for artifact in artifacts {
        match artifact.origin {
            ArtifactOrigin::Built => built += 1,
            ArtifactOrigin::Cache => reused += 1,
            ArtifactOrigin::Downloaded => downloaded += 1,
        }
    }
    let total = artifacts.len();
    if total == 0 {
        return "nothing to warm".to_string();
    }
    if reused == total {
        // Everything was already present — the warm changed nothing.
        return "already fully cached".to_string();
    }
    let mut parts = Vec::new();
    if reused > 0 {
        parts.push(format!("{reused} reused"));
    }
    if built > 0 {
        parts.push(format!("{built} built"));
    }
    if downloaded > 0 {
        parts.push(format!("{downloaded} downloaded"));
    }
    format!("{} (now cached)", parts.join(", "))
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
            warm_cache: false,
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
    // warm_origin_label / warm_summary — cache-warm provenance display
    // =========================================================================

    #[test]
    fn warm_origin_label_maps_each_origin() {
        assert_eq!(warm_origin_label(ArtifactOrigin::Cache), "reused");
        assert_eq!(warm_origin_label(ArtifactOrigin::Built), "built");
        assert_eq!(warm_origin_label(ArtifactOrigin::Downloaded), "downloaded");
    }

    #[test]
    fn warm_summary_all_reused_is_already_cached() {
        // Everything was already in the cache → the warm was a no-op.
        let all_cached = [
            artifact(ArtifactOrigin::Cache),
            artifact(ArtifactOrigin::Cache),
        ];
        assert_eq!(warm_summary(&all_cached), "already fully cached");
    }

    #[test]
    fn warm_summary_all_built_now_cached() {
        assert_eq!(
            warm_summary(&[artifact(ArtifactOrigin::Built)]),
            "1 built (now cached)"
        );
    }

    #[test]
    fn warm_summary_mixed_lists_each_and_marks_cached() {
        let mixed = [
            artifact(ArtifactOrigin::Cache),
            artifact(ArtifactOrigin::Built),
            artifact(ArtifactOrigin::Downloaded),
        ];
        assert_eq!(
            warm_summary(&mixed),
            "1 reused, 1 built, 1 downloaded (now cached)"
        );
    }

    #[test]
    fn warm_summary_empty() {
        assert_eq!(warm_summary(&[]), "nothing to warm");
    }

    // =========================================================================
    // --warm-cache CLI wiring (clap conflicts + redundant-value handling)
    // =========================================================================

    use clap::Parser;

    /// Test-only parser wrapper so we can exercise `BuildArgs` clap wiring
    /// (conflicts, defaults) exactly as the real `apvm build` subcommand does.
    #[derive(Parser)]
    struct WarmTestCli {
        #[command(flatten)]
        args: BuildArgs,
    }

    #[test]
    fn warm_cache_conflicts_with_no_cache() {
        // `--no-cache` bypasses reads and `--warm-cache` *is* a cache
        // operation — genuinely contradictory, so this stays a hard error.
        let res = WarmTestCli::try_parse_from([
            "apvm",
            "backwpup",
            "develop",
            "--warm-cache",
            "--no-cache",
        ]);
        assert!(res.is_err(), "--warm-cache must conflict with --no-cache");
    }

    #[test]
    fn warm_cache_accepts_explicit_output_as_redundant() {
        // An explicit output directory alongside --warm-cache is NOT a clap
        // error: it parses fine, is preserved on the struct (execute() is
        // what warns and ignores it — see `output_was_explicit` below), so a
        // typo'd or leftover output arg never blocks the warm.
        let cli =
            WarmTestCli::try_parse_from(["apvm", "backwpup", "develop", "./out", "--warm-cache"])
                .expect("--warm-cache with an explicit output directory must parse");
        assert!(cli.args.warm_cache);
        assert_eq!(cli.args.output, PathBuf::from("./out"));
    }

    #[test]
    fn warm_cache_alone_parses_with_defaulted_output() {
        let cli = WarmTestCli::try_parse_from(["apvm", "backwpup", "develop", "--warm-cache"])
            .expect("--warm-cache with a defaulted output must parse");
        assert!(cli.args.warm_cache);
        assert!(!cli.args.no_cache);
        assert_eq!(cli.args.output, PathBuf::from("."));
    }

    #[test]
    fn build_accepts_explicit_output_without_warm_cache() {
        let cli = WarmTestCli::try_parse_from(["apvm", "backwpup", "develop", "./out"])
            .expect("a normal build accepts an output directory");
        assert!(!cli.args.warm_cache);
        assert_eq!(cli.args.output, PathBuf::from("./out"));
    }

    // =========================================================================
    // output_was_explicit — drives the "ignored with --warm-cache" note
    // =========================================================================

    #[test]
    fn output_was_explicit_false_for_defaulted_output() {
        // Mirrors what clap leaves on the struct when --output/[OUTPUT] is
        // omitted (default_value = ".").
        assert!(!build_args("develop").output_was_explicit());
    }

    #[test]
    fn output_was_explicit_true_for_a_custom_path() {
        let mut args = build_args("develop");
        args.output = PathBuf::from("./dist");
        assert!(args.output_was_explicit());
    }

    #[test]
    fn output_was_explicit_false_when_user_types_the_default_literally() {
        // Known, accepted false-negative: typing "." explicitly is
        // indistinguishable from the default and produces identical behavior
        // either way, so no warning is expected here.
        let mut args = build_args("develop");
        args.output = PathBuf::from(".");
        assert!(!args.output_was_explicit());
    }

    // =========================================================================
    // version_ignored / variants_ignored — drive both the "ignored" warning
    // AND whether the pre-build header echoes the (unhonored) parameter.
    // =========================================================================

    /// Minimal builder with an embedded version, standing in for a plugin
    /// whose version always comes from source (no real registered builder is
    /// `Embedded` since WP Rocket/Imagify moved to `Optional`).
    struct EmbeddedTestBuilder;

    impl apvm_core::build::plugins::Builder for EmbeddedTestBuilder {
        fn version_requirement(&self) -> VersionRequirement {
            VersionRequirement::Embedded
        }
        fn setup_commands(&self) -> Vec<apvm_core::build::progress::BuildStep> {
            vec![]
        }
        fn build_commands(
            &self,
            _: &apvm_core::build::BuildContext,
            _: &str,
            _: &[&str],
        ) -> Vec<apvm_core::build::progress::BuildStep> {
            vec![]
        }
        fn artifacts(
            &self,
            _: &apvm_core::build::BuildContext,
            _: &str,
            _: &[&str],
        ) -> apvm_core::Result<Vec<apvm_core::build::plugins::BuildArtifact>> {
            Ok(vec![])
        }
    }

    #[test]
    fn version_ignored_true_only_when_version_passed_and_embedded() {
        let mut args = build_args("develop");
        args.version = Some("1.2.3".to_string());
        assert!(
            args.version_ignored(&EmbeddedTestBuilder),
            "a passed version must be ignored for an Embedded builder"
        );
    }

    #[test]
    fn version_ignored_false_when_no_version_passed() {
        // Embedded, but nothing was passed to ignore in the first place.
        assert!(!build_args("develop").version_ignored(&EmbeddedTestBuilder));
    }

    #[test]
    fn version_ignored_false_for_optional_builders() {
        // WP Rocket / Imagify are Optional (post-checkout override), so a
        // passed version is honored, not ignored.
        let mut args = build_args("develop");
        args.version = Some("3.99.0".to_string());
        assert!(!args.version_ignored(&apvm_core::build::plugins::WpRocketBuilder));
        assert!(!args.version_ignored(&apvm_core::build::plugins::ImagifyBuilder));
    }

    #[test]
    fn version_ignored_false_for_required_builder() {
        // BackWPup requires a version — passing one is expected, not ignored.
        let mut args = build_args("develop");
        args.version = Some("5.1.0".to_string());
        assert!(!args.version_ignored(&apvm_core::build::plugins::BackWPupBuilder));
    }

    #[test]
    fn variants_ignored_true_for_single_variant_builders() {
        // WP Rocket and Imagify have no variants to select between.
        let mut args = build_args("develop");
        args.variants = Some(vec!["free".to_string()]);
        assert!(args.variants_ignored(&apvm_core::build::plugins::WpRocketBuilder));
        assert!(args.variants_ignored(&apvm_core::build::plugins::ImagifyBuilder));
    }

    #[test]
    fn variants_ignored_false_when_no_variants_passed() {
        let args = build_args("develop");
        assert!(!args.variants_ignored(&apvm_core::build::plugins::ImagifyBuilder));
    }

    #[test]
    fn variants_ignored_false_for_multi_variant_builder() {
        let mut args = build_args("develop");
        args.variants = Some(vec!["free".to_string(), "pro-en".to_string()]);
        assert!(!args.variants_ignored(&apvm_core::build::plugins::BackWPupBuilder));
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
