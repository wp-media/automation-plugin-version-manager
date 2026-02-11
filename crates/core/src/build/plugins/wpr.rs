//! WP Rocket project builder.
//!
//! WP Rocket is a WordPress performance plugin. It produces a single artifact
//! (`wp-rocket-{version}.zip`) by packaging the repository contents with
//! production Composer dependencies.
//!
//! # Version Handling
//!
//! This builder uses [`VersionRequirement::Embedded`] because:
//! - The version lives in the `wp-rocket.php` plugin header (`Version: X.Y.Z`)
//! - The build process does not inject or modify the version
//! - The version in the output matches whatever is in the checked-out source
//!
//! # Build Process
//!
//! The build replicates the official release script:
//!
//! 1. Remove any previous `wp-rocket-*.zip` artifacts
//! 2. Copy the repository into a temporary directory via `rsync`, excluding
//!    development-only files (tests, node_modules, .git, etc.)
//! 3. Install production Composer dependencies inside the copy
//! 4. Create a zip archive from the copy, excluding dotfiles and JS tooling
//! 5. Clean up the temporary directory
//!
//! # Variants
//!
//! WP Rocket is a single-variant plugin — it produces one zip file.

use std::path::Path;

use crate::Result;
use crate::error::Error;

use super::{BuildArtifact, Builder, ToolDependency, VersionRequirement, detect_wordpress_plugin_version};
use super::super::BuildContext;
use super::super::progress::{BuildEvent, BuildStep, ProgressReporter};

/// Builder for the WP Rocket project.
pub struct WpRocketBuilder;

/// Main plugin PHP file used for version detection.
const PLUGIN_FILE: &str = "wp-rocket.php";

/// Glob pattern for matching any versioned WP Rocket artifact.
///
/// Used by `pre_build_hook` to clean up artifacts from previous builds
/// regardless of which version produced them.
const ARTIFACT_GLOB: &str = "wp-rocket-*.zip";

/// Build the versioned artifact filename.
///
/// # Examples
///
/// ```ignore
/// assert_eq!(artifact_name("3.17.4"), "wp-rocket-3.17.4.zip");
/// assert_eq!(artifact_name("4.0.0-beta1"), "wp-rocket-4.0.0-beta1.zip");
/// ```
fn artifact_name(version: &str) -> String {
    format!("wp-rocket-{version}.zip")
}

/// Temporary staging directory used during the build.
///
/// The build copies repository contents here (minus dev files),
/// installs production dependencies, and zips the result.
const STAGING_DIR: &str = "wp-rocket-tmp";

/// Plugin directory name inside the staging area.
///
/// WordPress expects plugin files inside a directory matching the plugin
/// slug, so the zip contains `wp-rocket/` at the root.
const PLUGIN_DIR_NAME: &str = "wp-rocket";

/// Directories and files excluded from the rsync copy.
///
/// These are development-only resources that must not be shipped
/// in the production zip.
const RSYNC_EXCLUDES: &[&str] = &[
    "node_modules",
    "vendor",
    "bin",
    "src",
    "tests",
    ".git",
    ".github",
    ".tx",
];

/// Patterns excluded from the zip archive.
///
/// These catch any remaining dev/tooling files that made it through
/// the rsync step (e.g., dotfiles inside subdirectories).
const ZIP_EXCLUDES: &[&str] = &[
    "*/.*",
    "*/gulpfile.js",
    "*/package*",
    "*/php*",
];

impl Builder for WpRocketBuilder {
    // =========================================================================
    // Version Handling
    // =========================================================================

    /// WP Rocket uses embedded version from the plugin header.
    ///
    /// The `wp-rocket.php` file contains a standard WordPress plugin header
    /// with a `Version:` field. The build process packages files as-is
    /// without modifying the version.
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Embedded
    }

    /// Detect version from the `wp-rocket.php` plugin header.
    ///
    /// Reads the standard WordPress `Version:` header from the main
    /// plugin file in the checked-out repository.
    fn detect_version(&self, working_dir: &Path) -> Result<Option<String>> {
        detect_wordpress_plugin_version(&working_dir.join(PLUGIN_FILE))
    }

    // =========================================================================
    // Commands and Setup
    // =========================================================================

    fn tool_dependencies(&self) -> Vec<ToolDependency> {
        vec![
            ToolDependency::required("rsync"),
            ToolDependency::required("composer"),
            ToolDependency::required("zip"),
        ]
    }

    /// No setup commands needed — dependencies are installed inside the
    /// staging copy, not in the working directory.
    fn setup_commands(&self) -> Vec<BuildStep> {
        vec![]
    }

    // =========================================================================
    // Build Hooks
    // =========================================================================

    /// Clean previous build artifacts and prepare the staging directory.
    ///
    /// 1. Removes any existing `wp-rocket-*.zip` from the workspace dir.
    /// 2. Removes any leftover staging directory from a previous interrupted build.
    fn pre_build_hook(
        &self,
        context: &BuildContext,
        _version: &str,
        _variants: &[&str],
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        // Remove any previous versioned artifacts from workspace dir
        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new("Cleaning previous artifacts", "rm wp-rocket-*.zip"),
        });
        let pattern = context.workspace_dir().join(ARTIFACT_GLOB);
        let pattern_str = pattern.to_string_lossy();

        let entries = glob::glob(&pattern_str).map_err(|_| {
            Error::Build(format!(
                "Failed to read glob pattern in wpr pre_build_hook: {}",
                pattern_str
            ))
        })?;

        for entry in entries.flatten() {
            std::fs::remove_file(&entry).map_err(|e| {
                Error::Build(format!(
                    "Failed to remove previous artifact '{}': {}",
                    entry.display(), e
                ))
            })?;
        }

        // Remove leftover staging directory from workspace
        let staging = context.workspace_dir().join(STAGING_DIR);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|e| {
                Error::Build(format!(
                    "Failed to remove leftover staging directory '{}': {}",
                    staging.display(), e
                ))
            })?;
        }

        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new("Cleaning previous artifacts", "rm wp-rocket-*.zip"),
        });

        Ok(())
    }

    /// Clean up the staging directory after the build completes.
    fn post_build_hook(
        &self,
        context: &BuildContext,
        _version: &str,
        _variants: &[&str],
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new("Cleaning staging directory", "rm -rf wp-rocket-tmp"),
        });
        let staging = context.workspace_dir().join(STAGING_DIR);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|e| {
                Error::Build(format!(
                    "Failed to clean up staging directory '{}': {}",
                    staging.display(), e
                ))
            })?;
        }
        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new("Cleaning staging directory", "rm -rf wp-rocket-tmp"),
        });

        Ok(())
    }

    // =========================================================================
    // Build Execution
    // =========================================================================

    /// Generate the build commands that replicate the release script.
    ///
    /// The original shell script runs from the **parent** of the repository
    /// directory and references the repo by name. Here we use absolute paths
    /// derived from the build context to achieve the same layout:
    ///
    /// ```text
    /// workspace_dir/                     ← scratch space (staging + artifact)
    /// ├── wp-rocket-3.17.4.zip         ← final artifact (outside repo)
    /// ├── wp-rocket-tmp/
    /// │   └── wp-rocket/               ← rsync'd copy + composer deps
    /// └── wp-rocket/                   ← repo_dir (source code)
    ///     ├── .git/
    ///     └── wp-rocket.php
    /// ```
    fn build_commands(&self, context: &BuildContext, version: &str, _variants: &[&str]) -> Vec<BuildStep> {
        let repo_dir = context.repo_dir().display();
        let workspace_dir = context.workspace_dir().display();
        let artifact = artifact_name(version);

        // Build the rsync exclude flags
        let rsync_excludes: String = RSYNC_EXCLUDES
            .iter()
            .map(|e| format!("--exclude {}", e))
            .collect::<Vec<_>>()
            .join(" ");

        // Build the zip exclude flags
        let zip_excludes: String = ZIP_EXCLUDES
            .iter()
            .map(|e| format!("-x \"{}\"", e))
            .collect::<Vec<_>>()
            .join(" ");

        let staging_dir = format!("{}/{}", workspace_dir, STAGING_DIR);
        let staging_plugin_dir = format!("{}/{}", staging_dir, PLUGIN_DIR_NAME);

        vec![
            // 1. Create staging directory structure
            BuildStep::new(
                "Creating staging directory",
                format!("mkdir -p {}/", staging_plugin_dir),
            ),

            // 2. Copy repo contents into staging, excluding dev files
            //    rsync trailing slash on source means "copy contents of"
            BuildStep::new(
                "Copying source files to staging",
                format!(
                    "rsync -av {repo_dir}/ {staging_plugin_dir}/ {rsync_excludes} --quiet",
                ),
            ),

            // 3. Install production Composer dependencies in the staging copy
            BuildStep::new(
                "Installing production dependencies",
                format!(
                    "cd {staging_plugin_dir} && composer install --no-dev --no-scripts --no-interaction --quiet",
                ),
            ),

            // 4. Create the zip archive from the staging directory
            //    Output goes to workspace_dir to keep the repo clean
            BuildStep::new(
                "Creating plugin archive",
                format!(
                    "cd {staging_dir} && zip -r {workspace_dir}/{artifact} {PLUGIN_DIR_NAME} {zip_excludes} --quiet",
                ),
            ),
        ]
    }

    /// Return the single artifact produced by the build.
    ///
    /// The artifact lives in `workspace_dir` (outside the repo), so an
    /// absolute `source_path` is returned for the runner to resolve.
    fn artifacts(&self, context: &BuildContext, version: &str, _variants: &[&str]) -> Result<Vec<BuildArtifact>> {
        let name = artifact_name(version);
        let artifact = context.workspace_dir().join(&name);

        if !artifact.exists() {
            return Err(Error::Build(format!(
                "Expected artifact '{}' not found. Build may have failed.",
                artifact.display()
            )));
        }

        Ok(vec![BuildArtifact {
            variant_id: None,
            source_path: artifact.to_string_lossy().into_owned(),
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

    fn builder() -> WpRocketBuilder {
        WpRocketBuilder
    }

    /// Create a test build context with workspace_dir and repo_dir.
    ///
    /// Returns (TempDir, BuildContext) — keep TempDir alive for the test.
    fn test_context() -> (TempDir, BuildContext) {
        let workspace = TempDir::new().unwrap();
        let repo_dir = workspace.path().join(PLUGIN_DIR_NAME);
        std::fs::create_dir_all(&repo_dir).unwrap();
        let context = BuildContext::new(repo_dir, workspace.path().to_path_buf());
        (workspace, context)
    }

    // =========================================================================
    // Version Handling
    // =========================================================================

    #[test]
    fn test_version_requirement_is_embedded() {
        assert_eq!(builder().version_requirement(), VersionRequirement::Embedded);
    }

    #[test]
    fn test_detect_version_from_plugin_header() {
        let dir = TempDir::new().unwrap();
        let plugin_file = dir.path().join(PLUGIN_FILE);

        let mut f = std::fs::File::create(&plugin_file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, "/**").unwrap();
        writeln!(f, " * Plugin Name: WP Rocket").unwrap();
        writeln!(f, " * Version: 3.17.4").unwrap();
        writeln!(f, " */").unwrap();

        let version = builder().detect_version(dir.path()).unwrap();
        assert_eq!(version, Some("3.17.4".to_string()));
    }

    #[test]
    fn test_detect_version_beta() {
        let dir = TempDir::new().unwrap();
        let plugin_file = dir.path().join(PLUGIN_FILE);

        let mut f = std::fs::File::create(&plugin_file).unwrap();
        writeln!(f, "<?php").unwrap();
        writeln!(f, "/**").unwrap();
        writeln!(f, " * Plugin Name: WP Rocket").unwrap();
        writeln!(f, " * Version: 4.0.0-beta1").unwrap();
        writeln!(f, " */").unwrap();

        let version = builder().detect_version(dir.path()).unwrap();
        assert_eq!(version, Some("4.0.0-beta1".to_string()));
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
    // Tool Dependencies
    // =========================================================================

    #[test]
    fn test_tool_dependencies() {
        let deps = builder().tool_dependencies();
        assert_eq!(deps.len(), 3);

        let names: Vec<&str> = deps.iter().map(|d| d.name).collect();
        assert!(names.contains(&"rsync"));
        assert!(names.contains(&"composer"));
        assert!(names.contains(&"zip"));

        // All are required
        assert!(deps.iter().all(|d| d.required));

        // None have auto-install commands (system tools)
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
        let version = "3.17.4";
        let commands = builder().build_commands(&context, version, &[]);
        assert_eq!(commands.len(), 4);

        // 1. mkdir
        assert!(commands[0].command.contains("mkdir -p"));
        assert!(commands[0].command.contains(STAGING_DIR));
        assert_eq!(commands[0].label, "Creating staging directory");

        // 2. rsync with excludes and absolute paths
        assert!(commands[1].command.contains("rsync"));
        assert!(
            commands[1].command.contains(&context.repo_dir().display().to_string()),
            "rsync should reference absolute repo dir"
        );
        for exclude in RSYNC_EXCLUDES {
            assert!(
                commands[1].command.contains(&format!("--exclude {}", exclude)),
                "rsync command should exclude '{}'",
                exclude
            );
        }

        // 3. composer install in staging copy
        assert!(commands[2].command.contains("composer install"));
        assert!(commands[2].command.contains("--no-dev"));
        assert!(commands[2].command.contains("--no-scripts"));
        assert!(commands[2].command.contains(STAGING_DIR));

        // 4. zip with versioned filename, output to workspace dir
        let expected_artifact = artifact_name(version);
        assert!(commands[3].command.contains("zip -r"));
        assert!(
            commands[3].command.contains(&expected_artifact),
            "zip command should contain versioned artifact name '{}'",
            expected_artifact
        );
        assert!(
            commands[3].command.contains(&context.workspace_dir().display().to_string()),
            "zip output should reference absolute workspace dir"
        );
        for exclude in ZIP_EXCLUDES {
            assert!(
                commands[3].command.contains(exclude),
                "zip command should exclude '{}'",
                exclude
            );
        }
    }

    #[test]
    fn test_build_commands_version_affects_artifact_name() {
        let (_ws, context) = test_context();
        let cmds_a = builder().build_commands(&context, "3.17.4", &[]);
        let cmds_b = builder().build_commands(&context, "4.0.0-beta1", &[]);

        // Different versions produce different zip filenames
        assert!(cmds_a[3].command.contains("wp-rocket-3.17.4.zip"));
        assert!(cmds_b[3].command.contains("wp-rocket-4.0.0-beta1.zip"));
        assert_ne!(cmds_a[3], cmds_b[3]);

        // But the first 3 commands (mkdir, rsync, composer) are identical
        assert_eq!(cmds_a[..3], cmds_b[..3]);
    }

    #[test]
    fn test_build_commands_variants_ignored() {
        // Variants are irrelevant for WP Rocket
        let (_ws, context) = test_context();
        let cmds_a = builder().build_commands(&context, "3.17.4", &[]);
        let cmds_b = builder().build_commands(&context, "3.17.4", &["nonexistent"]);
        assert_eq!(cmds_a, cmds_b);
    }

    // =========================================================================
    // Pre-build Hook
    // =========================================================================

    #[test]
    fn test_pre_build_removes_previous_artifact() {
        let (_ws, context) = test_context();
        let artifact = context.workspace_dir().join("wp-rocket-3.17.4.zip");
        std::fs::File::create(&artifact).unwrap();
        assert!(artifact.exists());

        builder().pre_build_hook(&context, "3.17.4", &[], &NullReporter).unwrap();
        assert!(!artifact.exists());
    }

    #[test]
    fn test_pre_build_removes_artifacts_from_other_versions() {
        let (_ws, context) = test_context();
        let old = context.workspace_dir().join("wp-rocket-3.16.0.zip");
        let current = context.workspace_dir().join("wp-rocket-3.17.4.zip");
        std::fs::File::create(&old).unwrap();
        std::fs::File::create(&current).unwrap();

        builder().pre_build_hook(&context, "3.17.4", &[], &NullReporter).unwrap();
        assert!(!old.exists(), "old version artifact should be removed");
        assert!(!current.exists(), "current version artifact should be removed");
    }

    #[test]
    fn test_pre_build_removes_leftover_staging() {
        let (_ws, context) = test_context();
        let staging = context.workspace_dir().join(STAGING_DIR);
        std::fs::create_dir_all(&staging).unwrap();
        assert!(staging.exists());

        builder().pre_build_hook(&context, "3.17.4", &[], &NullReporter).unwrap();
        assert!(!staging.exists());
    }

    #[test]
    fn test_pre_build_noop_when_clean() {
        let (_ws, context) = test_context();
        builder().pre_build_hook(&context, "3.17.4", &[], &NullReporter).unwrap();
    }

    // =========================================================================
    // Post-build Hook
    // =========================================================================

    #[test]
    fn test_post_build_cleans_staging() {
        let (_ws, context) = test_context();
        let staging = context.workspace_dir().join(STAGING_DIR);
        std::fs::create_dir_all(&staging).unwrap();
        assert!(staging.exists());

        builder().post_build_hook(&context, "3.17.4", &[], &NullReporter).unwrap();
        assert!(!staging.exists());
    }

    #[test]
    fn test_post_build_noop_when_no_staging() {
        let (_ws, context) = test_context();
        builder().post_build_hook(&context, "3.17.4", &[], &NullReporter).unwrap();
    }

    // =========================================================================
    // Artifacts
    // =========================================================================

    #[test]
    fn test_artifacts_found() {
        let (_ws, context) = test_context();
        let version = "3.17.4";
        let name = artifact_name(version);
        let artifact = context.workspace_dir().join(&name);
        std::fs::File::create(&artifact).unwrap();

        let artifacts = builder().artifacts(&context, version, &[]).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].source_path, artifact.to_string_lossy());
        assert_eq!(artifacts[0].target_name, name);
        assert!(artifacts[0].variant_id.is_none());
    }

    #[test]
    fn test_artifacts_not_found_errors() {
        let (_ws, context) = test_context();
        let result = builder().artifacts(&context, "3.17.4", &[]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("wp-rocket-3.17.4.zip"));
        assert!(err.contains("not found"));
    }

    // =========================================================================
    // Artifact Name Helper
    // =========================================================================

    #[test]
    fn test_artifact_name_format() {
        assert_eq!(artifact_name("3.17.4"), "wp-rocket-3.17.4.zip");
        assert_eq!(artifact_name("4.0.0-beta1"), "wp-rocket-4.0.0-beta1.zip");
        assert_eq!(artifact_name("1.0.0"), "wp-rocket-1.0.0.zip");
    }
}
