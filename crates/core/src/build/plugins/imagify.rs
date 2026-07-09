//! Imagify project builder.
//!
//! Imagify is a WordPress image-optimization plugin. It produces a single
//! artifact (`imagify-{version}.zip`) that is ready to install on a WordPress
//! site.
//!
//! # Version Handling
//!
//! This builder uses [`VersionRequirement::Embedded`] because:
//! - The version lives in the `imagify.php` plugin header (`Version: X.Y.Z`)
//! - The build process does not inject or modify the version
//! - The version in the output matches whatever is in the checked-out source
//!
//! Any version passed on the command line is therefore ignored (a warning is
//! logged) and the version detected from source is used instead — identical to
//! the WP Rocket builder.
//!
//! # Build Process
//!
//! Unlike WP Rocket (whose packaging steps are replicated in Rust), Imagify
//! ships an official packaging script at [`BUILD_SCRIPT`] that produces the
//! exact same distribution ZIP the release CI sends to WordPress.org. This
//! builder reuses that script rather than duplicating its logic, so the two
//! never drift apart. The script, in order:
//!
//! 1. Downloads Strauss (`strauss.phar`) if missing
//! 2. Runs `composer install -o --no-dev`
//! 3. Runs `npm install` and `npm run build` (asset compilation)
//! 4. `rsync`s the repository into a temporary directory
//! 5. Applies the repo's `.distignore` exclusions
//! 6. Creates the ZIP under [`ARTIFACT_SUBDIR`]
//! 7. Restores dev dependencies
//!
//! The script accepts the output ZIP filename as its first argument, so this
//! builder passes `imagify-{version}.zip` to get a versioned artifact name
//! (matching WP Rocket's `wp-rocket-{version}.zip` convention) instead of the
//! script's default of `imagify.zip`.
//!
//! # Requirements & Platform Support
//!
//! The packaging script is a Bash script that shells out to `composer`, `npm`,
//! `curl`, `rsync`, and `zip`. These are declared as required
//! [`ToolDependency`]s so a missing tool fails early with a clear message
//! instead of a cryptic error from inside the script.
//!
//! That toolchain (`bash`, `rsync`, `zip`) is not available on Windows, so
//! **Imagify is Unix-only**. [`ensure_platform_supported`](Builder::ensure_platform_supported)
//! rejects a Windows build up front with an actionable
//! [`Error::PlatformUnsupported`](crate::error::Error::PlatformUnsupported)
//! rather than failing partway through the script. Build on macOS or Linux, or
//! use WSL (Windows Subsystem for Linux). This mirrors the Rust
//! `imagify_build_e2e` integration test, which is itself `#![cfg(unix)]`.
//!
//! (WP Rocket, by contrast, reimplements its packaging natively in Rust and so
//! builds on Windows; Imagify deliberately reuses the upstream script to avoid
//! drift, which is what makes it Unix-only.)
//!
//! # Variants
//!
//! Imagify is a single-variant plugin — it produces one ZIP file.
//!
//! # Releases
//!
//! Imagify's project is registered with `has_releases = false`. Although the
//! repository publishes GitHub Releases (git tags with notes), those releases
//! do **not** carry downloadable ZIP assets — distribution goes to the
//! WordPress.org SVN. A specific released version is therefore built from its
//! tag (e.g. `tag:v2.3.0`) rather than downloaded, so this builder does not
//! implement release-asset matching.

use std::path::Path;

use crate::Result;
use crate::error::Error;

use super::super::BuildContext;
use super::super::progress::{BuildEvent, BuildStep, ProgressReporter};
use super::{
    BuildArtifact, Builder, ToolDependency, VersionRequirement, detect_wordpress_plugin_version,
};

/// Builder for the Imagify project.
pub struct ImagifyBuilder;

/// Main plugin PHP file used for version detection.
const PLUGIN_FILE: &str = "imagify.php";

/// Path (relative to the repository root) of the official packaging script.
const BUILD_SCRIPT: &str = "bin/build-zip.sh";

/// Subdirectory (relative to the repository root) where the packaging script
/// writes the finished ZIP.
const ARTIFACT_SUBDIR: &str = "generatedpackages";

/// Glob pattern for matching any versioned Imagify artifact inside
/// [`ARTIFACT_SUBDIR`].
///
/// Used by `pre_build_hook` to clean up artifacts from previous builds
/// regardless of which version produced them.
const ARTIFACT_GLOB: &str = "imagify-*.zip";

/// Build the versioned artifact filename.
///
/// # Examples
///
/// ```ignore
/// assert_eq!(artifact_name("2.3.0"), "imagify-2.3.0.zip");
/// assert_eq!(artifact_name("2.3.0-beta1"), "imagify-2.3.0-beta1.zip");
/// ```
fn artifact_name(version: &str) -> String {
    format!("imagify-{version}.zip")
}

impl Builder for ImagifyBuilder {
    // =========================================================================
    // Version Handling
    // =========================================================================

    /// Imagify uses the embedded version from the plugin header.
    ///
    /// The `imagify.php` file contains a standard WordPress plugin header with a
    /// `Version:` field. The packaging script packages files as-is without
    /// modifying the version, so any provided version is ignored.
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Embedded
    }

    /// Detect version from the `imagify.php` plugin header.
    ///
    /// Reads the standard WordPress `Version:` header from the main plugin file
    /// in the checked-out repository.
    fn detect_version(&self, working_dir: &Path) -> Result<Option<String>> {
        detect_wordpress_plugin_version(&working_dir.join(PLUGIN_FILE))
    }

    // =========================================================================
    // Commands and Setup
    // =========================================================================

    /// Imagify can only be built on Unix-like systems.
    ///
    /// The build delegates to Imagify's official Bash packaging script
    /// (`bin/build-zip.sh`), which requires `bash`, `rsync`, and `zip` — none
    /// of which ship with Windows. Rather than let the build fail partway
    /// through the script (or fail the up-front tool check with a cryptic
    /// "missing rsync/zip"), reject Windows here with an actionable error.
    /// Build on macOS/Linux, or use WSL (Windows Subsystem for Linux).
    fn ensure_platform_supported(&self) -> Result<()> {
        #[cfg(windows)]
        {
            Err(Error::PlatformUnsupported {
                project: "imagify".to_string(),
                platform: std::env::consts::OS.to_string(),
                reason: "Imagify is packaged by its official Bash script \
                         (bin/build-zip.sh), which requires a Unix-like \
                         toolchain (bash, rsync, zip) unavailable on Windows. \
                         Build on macOS or Linux, or use WSL (Windows \
                         Subsystem for Linux)."
                    .to_string(),
            })
        }
        #[cfg(not(windows))]
        {
            Ok(())
        }
    }

    /// Tools the packaging script shells out to.
    ///
    /// All are required: the script calls each one and runs under
    /// `set -euo pipefail`, so a missing tool would abort it mid-way. Declaring
    /// them here surfaces the problem up front with an actionable message.
    fn tool_dependencies(&self) -> Vec<ToolDependency> {
        vec![
            ToolDependency::required("bash"),
            ToolDependency::required("composer"),
            ToolDependency::required("npm"),
            ToolDependency::required("curl"),
            ToolDependency::required("rsync"),
            ToolDependency::required("zip"),
        ]
    }

    /// No setup commands needed — the packaging script installs its own PHP and
    /// JS dependencies internally.
    fn setup_commands(&self) -> Vec<BuildStep> {
        vec![]
    }

    // =========================================================================
    // Build Hooks
    // =========================================================================

    /// Remove any Imagify artifacts left in `ARTIFACT_SUBDIR` by a previous
    /// build.
    ///
    /// Builds normally run in a fresh, isolated workspace, so there is usually
    /// nothing to remove. This guards the case where the same working tree is
    /// reused: `zip` appends to an existing archive rather than replacing it,
    /// which could otherwise leave stale files inside the ZIP.
    fn pre_build_hook(
        &self,
        context: &BuildContext,
        _version: &str,
        _variants: &[&str],
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        let step = BuildStep::new(
            "Cleaning previous artifacts",
            format!("rm {ARTIFACT_SUBDIR}/{ARTIFACT_GLOB}"),
        );
        reporter.report(&BuildEvent::StepStarted { step: step.clone() });

        let pattern = context.repo_dir().join(ARTIFACT_SUBDIR).join(ARTIFACT_GLOB);
        let pattern_str = pattern.to_string_lossy();

        let entries = glob::glob(&pattern_str).map_err(|_| {
            Error::Build(format!(
                "Failed to read glob pattern in imagify pre_build_hook: {pattern_str}"
            ))
        })?;

        for entry in entries.flatten() {
            std::fs::remove_file(&entry).map_err(|e| {
                Error::Build(format!(
                    "Failed to remove previous artifact '{}': {}",
                    entry.display(),
                    e
                ))
            })?;
        }

        reporter.report(&BuildEvent::StepCompleted { step });
        Ok(())
    }

    // =========================================================================
    // Build Execution
    // =========================================================================

    /// Run the official packaging script, requesting a versioned output name.
    ///
    /// The script derives the repository root from its own location, so it does
    /// not matter that the runner executes it from `repo_dir`. It is invoked
    /// via `bash` explicitly (rather than relying on the executable bit or the
    /// shebang) because it uses Bash-only features and the runner's default
    /// shell may be a POSIX `sh`.
    fn build_commands(
        &self,
        _context: &BuildContext,
        version: &str,
        _variants: &[&str],
    ) -> Vec<BuildStep> {
        vec![BuildStep::new(
            "Building Imagify plugin",
            format!("bash {BUILD_SCRIPT} \"{}\"", artifact_name(version)),
        )]
    }

    /// Return the single artifact produced by the build.
    ///
    /// The script writes to `{repo_dir}/{ARTIFACT_SUBDIR}/imagify-{version}.zip`,
    /// so a relative `source_path` is returned for the runner to resolve against
    /// `repo_dir`.
    fn artifacts(
        &self,
        context: &BuildContext,
        version: &str,
        _variants: &[&str],
    ) -> Result<Vec<BuildArtifact>> {
        let name = artifact_name(version);
        let relative = format!("{ARTIFACT_SUBDIR}/{name}");
        let artifact = context.repo_dir().join(&relative);

        if !artifact.exists() {
            return Err(Error::Build(format!(
                "Expected artifact '{}' not found. Build may have failed.",
                artifact.display()
            )));
        }

        Ok(vec![BuildArtifact {
            variant_id: None,
            source_path: relative,
            target_name: name,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildContext;
    use crate::build::progress::NullReporter;
    use std::io::Write;
    use tempfile::TempDir;

    fn builder() -> ImagifyBuilder {
        ImagifyBuilder
    }

    /// Create a test build context with workspace_dir and repo_dir.
    ///
    /// Returns (TempDir, BuildContext) — keep TempDir alive for the test.
    fn test_context() -> (TempDir, BuildContext) {
        let workspace = TempDir::new().unwrap();
        let repo_dir = workspace.path().join("imagify");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let context = BuildContext::new(repo_dir, workspace.path().to_path_buf());
        (workspace, context)
    }

    /// Absolute path to the versioned artifact inside the repo's
    /// `generatedpackages/` directory, creating the directory.
    fn artifact_path(context: &BuildContext, version: &str) -> std::path::PathBuf {
        let dir = context.repo_dir().join(ARTIFACT_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(artifact_name(version))
    }

    // =========================================================================
    // Version Handling
    // =========================================================================

    #[test]
    fn test_version_requirement_is_embedded() {
        assert_eq!(
            builder().version_requirement(),
            VersionRequirement::Embedded
        );
    }

    #[test]
    fn test_detect_version_from_plugin_header() {
        let dir = TempDir::new().unwrap();
        let plugin_file = dir.path().join(PLUGIN_FILE);

        let mut f = std::fs::File::create(&plugin_file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, "/**").unwrap();
        writeln!(f, " * Plugin Name: Imagify").unwrap();
        writeln!(f, " * Version: 2.3.0").unwrap();
        writeln!(f, " */").unwrap();

        let version = builder().detect_version(dir.path()).unwrap();
        assert_eq!(version, Some("2.3.0".to_string()));
    }

    #[test]
    fn test_detect_version_beta() {
        let dir = TempDir::new().unwrap();
        let plugin_file = dir.path().join(PLUGIN_FILE);

        let mut f = std::fs::File::create(&plugin_file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, " * Version: 2.4.0-beta1").unwrap();

        let version = builder().detect_version(dir.path()).unwrap();
        assert_eq!(version, Some("2.4.0-beta1".to_string()));
    }

    #[test]
    fn test_detect_version_missing_file() {
        let dir = TempDir::new().unwrap();
        let version = builder().detect_version(dir.path()).unwrap();
        assert_eq!(version, None);
    }

    #[test]
    fn test_detect_version_no_header() {
        let dir = TempDir::new().unwrap();
        let plugin_file = dir.path().join(PLUGIN_FILE);

        let mut f = std::fs::File::create(&plugin_file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, "// Just some code, no plugin header").unwrap();

        let version = builder().detect_version(dir.path()).unwrap();
        assert_eq!(version, None);
    }

    // =========================================================================
    // Platform Support
    // =========================================================================

    /// On Windows the builder must reject up front with an actionable message
    /// naming the project and pointing at WSL.
    #[test]
    #[cfg(windows)]
    fn test_ensure_platform_supported_errors_on_windows() {
        let err = builder().ensure_platform_supported().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("imagify"), "message should name the project");
        assert!(
            msg.contains("not supported"),
            "message should state it is unsupported"
        );
        assert!(
            msg.contains("WSL"),
            "message should suggest WSL as a remedy"
        );
    }

    /// On Unix (macOS/Linux) the builder is supported, so the gate is a no-op.
    #[test]
    #[cfg(not(windows))]
    fn test_ensure_platform_supported_ok_on_unix() {
        assert!(builder().ensure_platform_supported().is_ok());
    }

    // =========================================================================
    // Tool Dependencies
    // =========================================================================

    #[test]
    fn test_tool_dependencies() {
        let deps = builder().tool_dependencies();

        let names: Vec<&str> = deps.iter().map(|d| d.name).collect();
        for expected in ["bash", "composer", "npm", "curl", "rsync", "zip"] {
            assert!(
                names.contains(&expected),
                "tool dependencies should include '{expected}'"
            );
        }

        // All are required system tools with no auto-install commands.
        assert!(deps.iter().all(|d| d.required));
        assert!(deps.iter().all(|d| d.install_commands.is_empty()));
    }

    // =========================================================================
    // Setup Commands
    // =========================================================================

    #[test]
    fn test_setup_commands_empty() {
        assert!(builder().setup_commands().is_empty());
    }

    // =========================================================================
    // Variants
    // =========================================================================

    #[test]
    fn test_no_variants() {
        assert!(builder().variants().is_empty());
    }

    #[test]
    fn test_has_no_variants() {
        assert!(!builder().has_variants());
    }

    #[test]
    fn test_validate_variants_ignores_input() {
        // No variants → any requested variant list is accepted.
        assert!(builder().validate_variants(&["anything"]).is_ok());
        assert!(builder().validate_variants(&[]).is_ok());
    }

    // =========================================================================
    // Defaults
    // =========================================================================

    #[test]
    fn test_no_default_version() {
        assert_eq!(builder().default_version(), None);
    }

    #[test]
    fn test_no_default_variants() {
        assert!(builder().default_variants().is_empty());
    }

    // =========================================================================
    // Build Commands
    // =========================================================================

    #[test]
    fn test_build_commands_structure() {
        let (_ws, context) = test_context();
        let version = "2.3.0";
        let commands = builder().build_commands(&context, version, &[]);

        // Single step: invoke the packaging script.
        assert_eq!(commands.len(), 1);
        assert!(commands[0].command.contains("bash"));
        assert!(commands[0].command.contains(BUILD_SCRIPT));
        // The versioned artifact name is passed as the script argument.
        assert!(
            commands[0].command.contains(&artifact_name(version)),
            "build command should request versioned zip name '{}'",
            artifact_name(version)
        );
        assert_eq!(commands[0].label, "Building Imagify plugin");
    }

    #[test]
    fn test_build_commands_version_affects_artifact_name() {
        let (_ws, context) = test_context();
        let cmds_a = builder().build_commands(&context, "2.3.0", &[]);
        let cmds_b = builder().build_commands(&context, "2.4.0-beta1", &[]);

        assert!(cmds_a[0].command.contains("imagify-2.3.0.zip"));
        assert!(cmds_b[0].command.contains("imagify-2.4.0-beta1.zip"));
        assert_ne!(cmds_a[0], cmds_b[0]);
    }

    #[test]
    fn test_build_commands_variants_ignored() {
        // Variants are irrelevant for Imagify.
        let (_ws, context) = test_context();
        let cmds_a = builder().build_commands(&context, "2.3.0", &[]);
        let cmds_b = builder().build_commands(&context, "2.3.0", &["nonexistent"]);
        assert_eq!(cmds_a, cmds_b);
    }

    // =========================================================================
    // Pre-build Hook
    // =========================================================================

    #[test]
    fn test_pre_build_removes_previous_artifact() {
        let (_ws, context) = test_context();
        let artifact = artifact_path(&context, "2.3.0");
        std::fs::File::create(&artifact).unwrap();
        assert!(artifact.exists());

        builder()
            .pre_build_hook(&context, "2.3.0", &[], &NullReporter)
            .unwrap();
        assert!(!artifact.exists());
    }

    #[test]
    fn test_pre_build_removes_artifacts_from_other_versions() {
        let (_ws, context) = test_context();
        let old = artifact_path(&context, "2.2.9");
        let current = artifact_path(&context, "2.3.0");
        std::fs::File::create(&old).unwrap();
        std::fs::File::create(&current).unwrap();

        builder()
            .pre_build_hook(&context, "2.3.0", &[], &NullReporter)
            .unwrap();
        assert!(!old.exists(), "old version artifact should be removed");
        assert!(
            !current.exists(),
            "current version artifact should be removed"
        );
    }

    #[test]
    fn test_pre_build_noop_when_no_artifacts_dir() {
        // Fresh checkout: generatedpackages/ does not exist yet — must not error.
        let (_ws, context) = test_context();
        builder()
            .pre_build_hook(&context, "2.3.0", &[], &NullReporter)
            .unwrap();
    }

    // =========================================================================
    // Artifacts
    // =========================================================================

    #[test]
    fn test_artifacts_found() {
        let (_ws, context) = test_context();
        let version = "2.3.0";
        let artifact = artifact_path(&context, version);
        std::fs::File::create(&artifact).unwrap();

        let artifacts = builder().artifacts(&context, version, &[]).unwrap();
        assert_eq!(artifacts.len(), 1);
        // Relative to repo_dir, inside generatedpackages/.
        assert_eq!(
            artifacts[0].source_path,
            format!("{ARTIFACT_SUBDIR}/{}", artifact_name(version))
        );
        assert_eq!(artifacts[0].target_name, artifact_name(version));
        assert!(artifacts[0].variant_id.is_none());
    }

    #[test]
    fn test_artifacts_not_found_errors() {
        let (_ws, context) = test_context();
        let result = builder().artifacts(&context, "2.3.0", &[]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("imagify-2.3.0.zip"));
        assert!(err.contains("not found"));
    }

    // =========================================================================
    // Artifact Name Helper
    // =========================================================================

    #[test]
    fn test_artifact_name_format() {
        assert_eq!(artifact_name("2.3.0"), "imagify-2.3.0.zip");
        assert_eq!(artifact_name("2.4.0-beta1"), "imagify-2.4.0-beta1.zip");
        assert_eq!(artifact_name("1.0.0"), "imagify-1.0.0.zip");
    }

    // =========================================================================
    // Releases
    // =========================================================================

    #[test]
    fn test_does_not_match_release_assets() {
        // Imagify is registered with has_releases = false and downloads no
        // release assets; the default (no matching) must hold.
        let b = builder();
        assert!(!b.matches_release_asset("imagify-2.3.0.zip"));
        assert!(!b.matches_release_asset("imagify.zip"));
        assert!(b.variant_from_release_asset("imagify-2.3.0.zip").is_none());
    }
}
