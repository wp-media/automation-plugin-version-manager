//! WP Rocket project builder.
//!
//! WP Rocket is a WordPress performance plugin. It produces a single artifact
//! (`wp-rocket-{version}.zip`) by packaging the repository contents with
//! production Composer dependencies.
//!
//! # Version Handling
//!
//! This builder uses [`VersionRequirement::Optional`]:
//! - When no version is passed, the version is auto-detected from the
//!   `wp-rocket.php` plugin header (`Version: X.Y.Z`) — the build packages the
//!   source as-is.
//! - When a version is passed and the build path is taken,
//!   [`apply_version_override`](Builder::apply_version_override) rewrites both
//!   the `Version:` header and the `WP_ROCKET_VERSION` runtime constant in
//!   `wp-rocket.php` (never the unrelated `WP_ROCKET_LASTVERSION`) so the
//!   produced artifact carries the requested version everywhere it is declared.
//!
//! # Build Process
//!
//! The build replicates the official release script:
//!
//! 1. Remove any previous `wp-rocket-*.zip` artifacts
//! 2. Copy the repository into a temporary directory via `rsync`, excluding
//!    development-only files (tests, node_modules, .git, etc.)
//! 3. Install production Composer dependencies inside the copy
//! 4. Create a zip archive from the copy, excluding dotfiles and root-level
//!    build tooling (see [`ZIP_EXCLUSIONS`])
//! 5. Clean up the temporary directory
//!
//! # Variants
//!
//! WP Rocket is a single-variant plugin — it produces one zip file.

use std::path::Path;

use crate::Result;
use crate::error::Error;

use super::super::BuildContext;
use super::super::fs::ExclusionPattern;
#[cfg(any(windows, test))]
use super::super::fs::{copy_dir_with_exclusions, create_zip_archive};
use super::super::progress::{BuildEvent, BuildStep, ProgressReporter};
use super::{
    BuildArtifact, Builder, ToolDependency, VersionOverride, VersionRequirement,
    detect_wordpress_plugin_version, rewrite_wordpress_plugin_version,
};

/// Builder for the WP Rocket project.
pub struct WpRocketBuilder;

/// Main plugin PHP file used for version detection and override.
const PLUGIN_FILE: &str = "wp-rocket.php";

/// Runtime version constant defined in [`PLUGIN_FILE`], rewritten alongside the
/// header when a version override is applied. The unrelated
/// `WP_ROCKET_LASTVERSION` constant is intentionally left untouched.
const VERSION_CONSTANT: &str = "WP_ROCKET_VERSION";

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

/// Patterns excluded from the zip archive, on every platform.
///
/// Single source of truth: the Unix build renders these into `zip -x` flags via
/// [`zip_exclude_flags`], while the Windows build hands the same slice to
/// [`create_zip_archive`]. Both platforms therefore produce identical archives
/// by construction.
///
/// These catch dev/tooling files that survive the rsync step, which drops only
/// the names listed in [`RSYNC_EXCLUDES`] (matched at any depth, whole subtree):
///
/// - `Prefix(".")` → `-x "*/.*"` — dotfiles at any depth
/// - `Exact("gulpfile.js")` → `-x "*/gulpfile.js"` — the gulp entry point
/// - `RootPrefix("package")` → `-x "wp-rocket/package*"` — root `package.json`,
///   `package-lock.json`
/// - `RootPrefix("php")` → `-x "wp-rocket/php*"` — root `phpcs.xml`,
///   `phpstan.neon.dist`, `phpstan-baseline.neon`
///
/// # Why the `php`/`package` rules are root-anchored
///
/// A depth-independent `-x "*/php*"` also matches **nested** paths, because
/// `zip`'s `*` spans `/`. That silently dropped the entire
/// `vendor/wordpress/php-mcp-schema/` tree from the artifact, leaving
/// `wordpress/mcp-adapter` with a declared but unloadable
/// `WP\McpSchema\…` namespace and a fatal error at plugin load:
///
/// ```text
/// PHP Fatal error: Could not check compatibility between
/// WP\MCP\Domain\Tools\McpTool::get_protocol_dto(): WP\McpSchema\Server\Tools\DTO\Tool
/// and WP\MCP\Domain\Contracts\McpComponentInterface::get_protocol_dto(): …,
/// because class WP\McpSchema\Server\Tools\DTO\Tool is not available
/// ```
///
/// `*/package*` had the same latent defect for any `vendor/*/package*/` package.
/// Anchoring both to the plugin root keeps the intended tooling files out while
/// leaving vendor subtrees intact.
const ZIP_EXCLUSIONS: &[ExclusionPattern<'static>] = &[
    ExclusionPattern::Prefix("."),
    ExclusionPattern::Exact("gulpfile.js"),
    ExclusionPattern::RootPrefix("package"),
    ExclusionPattern::RootPrefix("php"),
];

/// Render [`ZIP_EXCLUSIONS`] as `zip -x "<pattern>"` flags.
///
/// Root-anchored patterns are prefixed with [`PLUGIN_DIR_NAME`] so they match
/// only the archive's top level; depth-independent patterns keep the leading
/// `*/` wildcard.
#[cfg(any(unix, test))]
fn zip_exclude_flags() -> String {
    ZIP_EXCLUSIONS
        .iter()
        .map(|pattern| match pattern {
            ExclusionPattern::Prefix(prefix) => format!("-x \"*/{prefix}*\""),
            ExclusionPattern::Exact(name) => format!("-x \"*/{name}\""),
            ExclusionPattern::RootPrefix(prefix) => {
                format!("-x \"{PLUGIN_DIR_NAME}/{prefix}*\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Builder for WpRocketBuilder {
    // =========================================================================
    // Version Handling
    // =========================================================================

    /// WP Rocket's version lives in the `wp-rocket.php` plugin header, so it is
    /// [`Optional`](VersionRequirement::Optional): auto-detected when no version
    /// is passed, or overridden into the source when one is (see
    /// [`apply_version_override`](Self::apply_version_override)).
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Optional
    }

    /// Detect version from the `wp-rocket.php` plugin header.
    ///
    /// Reads the standard WordPress `Version:` header from the main
    /// plugin file in the checked-out repository.
    fn detect_version(&self, working_dir: &Path) -> Result<Option<String>> {
        detect_wordpress_plugin_version(&working_dir.join(PLUGIN_FILE))
    }

    /// The single file [`detect_version`](Self::detect_version) reads, enabling
    /// pre-clone version detection.
    fn version_source_files(&self) -> Vec<&'static str> {
        vec![PLUGIN_FILE]
    }

    /// Rewrite the requested version into `wp-rocket.php` before packaging.
    ///
    /// Overrides both the `Version:` header and the `WP_ROCKET_VERSION` runtime
    /// constant (leaving `WP_ROCKET_LASTVERSION` untouched) so the produced zip
    /// carries `version` everywhere it is declared. A no-op when the source
    /// already matches; errors if either declaration is missing.
    fn apply_version_override(
        &self,
        working_dir: &Path,
        version: &str,
    ) -> Result<Option<VersionOverride>> {
        rewrite_wordpress_plugin_version(
            &working_dir.join(PLUGIN_FILE),
            version,
            Some(VERSION_CONSTANT),
        )
    }

    // =========================================================================
    // Commands and Setup
    // =========================================================================

    fn tool_dependencies(&self) -> Vec<ToolDependency> {
        // On Unix, rsync and zip are required as external shell tools.
        // On Windows, they are replaced by pure Rust implementations
        // (walkdir + std::fs::copy for rsync, zip crate for archiving).
        #[cfg(unix)]
        {
            vec![
                ToolDependency::required("rsync"),
                ToolDependency::required("composer"),
                ToolDependency::required("zip"),
            ]
        }

        #[cfg(windows)]
        {
            vec![ToolDependency::required("composer")]
        }
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
    /// 3. (Windows only) Creates the staging directory and copies source files,
    ///    replacing the `mkdir -p` + `rsync` shell commands with pure Rust equivalents.
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
                    entry.display(),
                    e
                ))
            })?;
        }

        // Remove leftover staging directory from workspace
        let staging = context.workspace_dir().join(STAGING_DIR);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|e| {
                Error::Build(format!(
                    "Failed to remove leftover staging directory '{}': {}",
                    staging.display(),
                    e
                ))
            })?;
        }

        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new("Cleaning previous artifacts", "rm wp-rocket-*.zip"),
        });

        // Windows: create staging directory and copy source files (replaces mkdir + rsync).
        // Uses pure Rust via build::fs utilities — no external tools needed.
        #[cfg(windows)]
        self.prepare_staging(context, reporter)?;

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
                    staging.display(),
                    e
                ))
            })?;
        }
        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new("Cleaning staging directory", "rm -rf wp-rocket-tmp"),
        });

        Ok(())
    }

    /// Hook called during the build to create the zip archive (Windows only).
    ///
    /// On Unix, the zip is created by a shell command in `build_commands()`.
    /// On Windows, we use the pure Rust `create_zip_archive` utility.
    fn build_hook(
        &self,
        context: &BuildContext,
        version: &str,
        _variants: &[&str],
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        #[cfg(windows)]
        self.create_archive(context, version, reporter)?;

        // Suppress unused parameter warnings on Unix
        let _ = (context, version, reporter);

        Ok(())
    }

    // =========================================================================
    // Build Execution
    // =========================================================================

    /// Generate the build commands that replicate the release script.
    ///
    /// On **Unix**, generates 4 shell commands (mkdir, rsync, composer, zip).
    /// On **Windows**, generates only 1 shell command (composer install),
    /// because mkdir/rsync/zip are handled by pure Rust in hooks.
    fn build_commands(
        &self,
        context: &BuildContext,
        version: &str,
        _variants: &[&str],
    ) -> Vec<BuildStep> {
        #[cfg(unix)]
        {
            self.build_commands_unix(context, version)
        }

        #[cfg(windows)]
        {
            // `version` is only consumed by the Unix command set; the Windows
            // path derives everything it needs from `context`.
            let _ = version;
            self.build_commands_windows(context)
        }
    }

    /// Return the single artifact produced by the build.
    ///
    /// The artifact lives in `workspace_dir` (outside the repo), so an
    /// absolute `source_path` is returned for the runner to resolve.
    fn artifacts(
        &self,
        context: &BuildContext,
        version: &str,
        _variants: &[&str],
    ) -> Result<Vec<BuildArtifact>> {
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

// =============================================================================
// Platform-Specific Helpers
// =============================================================================

impl WpRocketBuilder {
    /// Generate Unix build commands using shell tools (mkdir, rsync, composer, zip).
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
    #[cfg(unix)]
    fn build_commands_unix(&self, context: &BuildContext, version: &str) -> Vec<BuildStep> {
        let repo_dir = context.repo_dir().display();
        let workspace_dir = context.workspace_dir().display();
        let artifact = artifact_name(version);

        // Build the rsync exclude flags
        let rsync_excludes: String = RSYNC_EXCLUDES
            .iter()
            .map(|e| format!("--exclude {e}"))
            .collect::<Vec<_>>()
            .join(" ");

        let zip_excludes = zip_exclude_flags();

        let staging_dir = format!("{workspace_dir}/{STAGING_DIR}");
        let staging_plugin_dir = format!("{staging_dir}/{PLUGIN_DIR_NAME}");

        vec![
            // 1. Create staging directory structure
            BuildStep::new(
                "Creating staging directory",
                format!("mkdir -p {staging_plugin_dir}/"),
            ),
            // 2. Copy repo contents into staging, excluding dev files
            //    rsync trailing slash on source means "copy contents of"
            BuildStep::new(
                "Copying source files to staging",
                format!("rsync -av {repo_dir}/ {staging_plugin_dir}/ {rsync_excludes}"),
            ),
            // 3. Install production Composer dependencies in the staging copy
            BuildStep::new(
                "Installing production dependencies",
                format!(
                    "cd {staging_plugin_dir} && composer install --no-dev --no-scripts --no-interaction"
                ),
            ),
            // 4. Create the zip archive from the staging directory
            //    Output goes to workspace_dir to keep the repo clean
            BuildStep::new(
                "Creating plugin archive",
                format!(
                    "cd {staging_dir} && zip -r {workspace_dir}/{artifact} {PLUGIN_DIR_NAME} {zip_excludes}"
                ),
            ),
        ]
    }

    /// Generate Windows build commands.
    ///
    /// Only includes the composer install step. The mkdir + rsync equivalent
    /// is handled by [`prepare_staging`](Self::prepare_staging) in `pre_build_hook`,
    /// and the zip creation by [`create_archive`](Self::create_archive) in `build_hook`.
    ///
    /// `cd /d` is used instead of `cd` to support changing drives on Windows.
    /// Source: <https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/cd>
    #[cfg(windows)]
    fn build_commands_windows(&self, context: &BuildContext) -> Vec<BuildStep> {
        let staging_plugin_dir = context
            .workspace_dir()
            .join(STAGING_DIR)
            .join(PLUGIN_DIR_NAME);

        vec![BuildStep::new(
            "Installing production dependencies",
            format!(
                "cd /d {} && composer install --no-dev --no-scripts --no-interaction",
                staging_plugin_dir.display(),
            ),
        )]
    }

    /// Create the staging directory and copy repository contents, excluding dev files.
    ///
    /// This is the pure Rust equivalent of:
    /// ```sh
    /// mkdir -p staging_dir/wp-rocket/
    /// rsync -av repo_dir/ staging_dir/wp-rocket/ --exclude node_modules ...
    /// ```
    ///
    /// Uses [`copy_dir_with_exclusions`] from the [`fs`](super::super::fs) module
    /// which relies on [`walkdir::WalkDir::filter_entry`] to skip excluded directories
    /// entirely (preventing descent into large dirs like `node_modules`).
    ///
    /// Compiled under `cfg(test)` on every platform so the Windows staging step
    /// is covered by the suite rather than only by Windows CI.
    #[cfg(any(windows, test))]
    fn prepare_staging(
        &self,
        context: &BuildContext,
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        let staging_plugin_dir = context
            .workspace_dir()
            .join(STAGING_DIR)
            .join(PLUGIN_DIR_NAME);

        // mkdir -p equivalent
        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new(
                "Creating staging directory",
                staging_plugin_dir.display().to_string(),
            ),
        });
        std::fs::create_dir_all(&staging_plugin_dir).map_err(|e| {
            Error::Build(format!(
                "Failed to create staging directory '{}': {e}",
                staging_plugin_dir.display()
            ))
        })?;
        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new(
                "Creating staging directory",
                staging_plugin_dir.display().to_string(),
            ),
        });

        // rsync -av equivalent
        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new("Copying source files to staging", "copy with exclusions"),
        });
        let files_copied =
            copy_dir_with_exclusions(context.repo_dir(), &staging_plugin_dir, RSYNC_EXCLUDES)?;
        tracing::info!("Copied {} files to staging directory", files_copied);
        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new("Copying source files to staging", "copy with exclusions"),
        });

        Ok(())
    }

    /// Create a zip archive from the staging directory.
    ///
    /// This is the pure Rust equivalent of:
    /// ```sh
    /// cd staging_dir && zip -r workspace_dir/wp-rocket-3.17.4.zip wp-rocket -x "*/.*" ...
    /// ```
    ///
    /// Uses [`create_zip_archive`] from the [`fs`](super::super::fs) module
    /// with Deflate compression, matching the default behavior of the `zip`
    /// command-line tool.
    ///
    /// Compiled under `cfg(test)` on every platform so the Windows archive step
    /// is covered by the suite rather than only by Windows CI.
    #[cfg(any(windows, test))]
    fn create_archive(
        &self,
        context: &BuildContext,
        version: &str,
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        let staging_plugin_dir = context
            .workspace_dir()
            .join(STAGING_DIR)
            .join(PLUGIN_DIR_NAME);
        let archive_path = context.workspace_dir().join(artifact_name(version));

        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new(
                "Creating plugin archive",
                archive_path.display().to_string(),
            ),
        });

        let entries_written = create_zip_archive(
            &staging_plugin_dir,
            &archive_path,
            PLUGIN_DIR_NAME,
            ZIP_EXCLUSIONS,
        )?;
        tracing::info!(
            "Created archive '{}' with {} entries",
            archive_path.display(),
            entries_written
        );

        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new(
                "Creating plugin archive",
                archive_path.display().to_string(),
            ),
        });

        Ok(())
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
    fn test_version_requirement_is_optional() {
        assert_eq!(
            builder().version_requirement(),
            VersionRequirement::Optional
        );
    }

    #[test]
    fn test_version_source_files_is_the_plugin_file() {
        // Must be exactly the file `detect_version` reads, so pre-clone
        // detection agrees with a full checkout.
        assert_eq!(builder().version_source_files(), vec![PLUGIN_FILE]);
    }

    #[test]
    fn test_apply_version_override_rewrites_header_and_constant() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(PLUGIN_FILE),
            "<?php\n * Version: 3.23\ndefine( 'WP_ROCKET_VERSION', '3.23' );\n\
             define( 'WP_ROCKET_LASTVERSION', '3.22.1' );\n",
        )
        .unwrap();

        let applied = builder()
            .apply_version_override(dir.path(), "3.99.0")
            .unwrap()
            .expect("override applied");
        assert_eq!(applied.file, PLUGIN_FILE);
        assert_eq!(applied.from, "3.23");
        assert_eq!(applied.to, "3.99.0");

        let rewritten = std::fs::read_to_string(dir.path().join(PLUGIN_FILE)).unwrap();
        assert!(rewritten.contains(" * Version: 3.99.0"));
        assert!(rewritten.contains("define( 'WP_ROCKET_VERSION', '3.99.0' );"));
        assert!(rewritten.contains("define( 'WP_ROCKET_LASTVERSION', '3.22.1' );"));
    }

    #[test]
    fn test_apply_version_override_noop_when_matching() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(PLUGIN_FILE),
            "<?php\n * Version: 3.23\ndefine( 'WP_ROCKET_VERSION', '3.23' );\n",
        )
        .unwrap();
        let applied = builder()
            .apply_version_override(dir.path(), "3.23")
            .unwrap();
        assert!(applied.is_none());
    }

    #[test]
    fn test_apply_version_override_errors_without_source() {
        // No wp-rocket.php present → cannot honor the requested version.
        let dir = TempDir::new().unwrap();
        assert!(
            builder()
                .apply_version_override(dir.path(), "3.99.0")
                .is_err()
        );
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
    #[cfg(unix)]
    fn test_tool_dependencies_unix() {
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

    #[test]
    #[cfg(windows)]
    fn test_tool_dependencies_windows() {
        let deps = builder().tool_dependencies();
        assert_eq!(deps.len(), 1);

        let names: Vec<&str> = deps.iter().map(|d| d.name).collect();
        assert!(names.contains(&"composer"));
        // rsync and zip are NOT required on Windows (pure Rust replacements)
        assert!(!names.contains(&"rsync"));
        assert!(!names.contains(&"zip"));

        assert!(deps.iter().all(|d| d.required));
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
    #[cfg(unix)]
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
            commands[1]
                .command
                .contains(&context.repo_dir().display().to_string()),
            "rsync should reference absolute repo dir"
        );
        for exclude in RSYNC_EXCLUDES {
            assert!(
                commands[1]
                    .command
                    .contains(&format!("--exclude {}", exclude)),
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
            commands[3]
                .command
                .contains(&context.workspace_dir().display().to_string()),
            "zip output should reference absolute workspace dir"
        );
        assert!(
            commands[3].command.contains(&zip_exclude_flags()),
            "zip command should carry the rendered exclude flags"
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_build_commands_structure_windows() {
        let (_ws, context) = test_context();
        let version = "3.17.4";
        let commands = builder().build_commands(&context, version, &[]);

        // Windows: only composer install (mkdir + rsync + zip handled by Rust)
        assert_eq!(commands.len(), 1);
        assert!(commands[0].command.contains("composer install"));
        assert!(commands[0].command.contains("--no-dev"));
        assert!(commands[0].command.contains("--no-scripts"));
        assert!(commands[0].command.contains("cd /d"));
    }

    #[test]
    #[cfg(unix)]
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

        builder()
            .pre_build_hook(&context, "3.17.4", &[], &NullReporter)
            .unwrap();
        assert!(!artifact.exists());
    }

    #[test]
    fn test_pre_build_removes_artifacts_from_other_versions() {
        let (_ws, context) = test_context();
        let old = context.workspace_dir().join("wp-rocket-3.16.0.zip");
        let current = context.workspace_dir().join("wp-rocket-3.17.4.zip");
        std::fs::File::create(&old).unwrap();
        std::fs::File::create(&current).unwrap();

        builder()
            .pre_build_hook(&context, "3.17.4", &[], &NullReporter)
            .unwrap();
        assert!(!old.exists(), "old version artifact should be removed");
        assert!(
            !current.exists(),
            "current version artifact should be removed"
        );
    }

    /// On Unix, `pre_build_hook` only cleans artifacts and removes leftover staging.
    /// On Windows, it additionally calls `prepare_staging` which recreates the staging
    /// directory — so after the hook, staging **exists** on Windows but not on Unix.
    #[test]
    #[cfg(unix)]
    fn test_pre_build_removes_leftover_staging() {
        let (_ws, context) = test_context();
        let staging = context.workspace_dir().join(STAGING_DIR);
        std::fs::create_dir_all(&staging).unwrap();
        assert!(staging.exists());

        builder()
            .pre_build_hook(&context, "3.17.4", &[], &NullReporter)
            .unwrap();
        assert!(!staging.exists());
    }

    /// Windows: `pre_build_hook` removes leftovers then calls `prepare_staging`,
    /// which recreates the staging directory with the plugin subdirectory.
    #[test]
    #[cfg(windows)]
    fn test_pre_build_removes_leftover_staging() {
        let (_ws, context) = test_context();
        let staging = context.workspace_dir().join(STAGING_DIR);
        std::fs::create_dir_all(&staging).unwrap();
        assert!(staging.exists());

        builder()
            .pre_build_hook(&context, "3.17.4", &[], &NullReporter)
            .unwrap();

        // On Windows, prepare_staging recreates the staging directory structure
        let staging_plugin = staging.join(PLUGIN_DIR_NAME);
        assert!(
            staging_plugin.exists(),
            "prepare_staging should recreate staging/wp-rocket"
        );
    }

    #[test]
    fn test_pre_build_noop_when_clean() {
        let (_ws, context) = test_context();
        builder()
            .pre_build_hook(&context, "3.17.4", &[], &NullReporter)
            .unwrap();
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

        builder()
            .post_build_hook(&context, "3.17.4", &[], &NullReporter)
            .unwrap();
        assert!(!staging.exists());
    }

    #[test]
    fn test_post_build_noop_when_no_staging() {
        let (_ws, context) = test_context();
        builder()
            .post_build_hook(&context, "3.17.4", &[], &NullReporter)
            .unwrap();
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

    // =========================================================================
    // ZIP Exclusion Patterns
    // =========================================================================

    #[test]
    fn test_zip_exclusions_match_expected_root_files() {
        use crate::build::fs::matches_exclusion_at_depth;

        // Depth of entries at the top level of the archive.
        const ROOT: usize = 1;

        // Dotfiles — `*/.*`
        assert!(matches_exclusion_at_depth(".git", ROOT, ZIP_EXCLUSIONS));
        assert!(matches_exclusion_at_depth(".env", ROOT, ZIP_EXCLUSIONS));
        assert!(matches_exclusion_at_depth(
            ".gitignore",
            ROOT,
            ZIP_EXCLUSIONS
        ));

        // gulpfile.js — `*/gulpfile.js`
        assert!(matches_exclusion_at_depth(
            "gulpfile.js",
            ROOT,
            ZIP_EXCLUSIONS
        ));

        // package* — `wp-rocket/package*`
        assert!(matches_exclusion_at_depth(
            "package.json",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(matches_exclusion_at_depth(
            "package-lock.json",
            ROOT,
            ZIP_EXCLUSIONS
        ));

        // php* — `wp-rocket/php*`. These are WP Rocket's real root tooling files.
        assert!(matches_exclusion_at_depth(
            "phpcs.xml",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(matches_exclusion_at_depth(
            "phpstan.neon.dist",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(matches_exclusion_at_depth(
            "phpstan-baseline.neon",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(matches_exclusion_at_depth(
            "phpunit.xml",
            ROOT,
            ZIP_EXCLUSIONS
        ));
    }

    #[test]
    fn test_zip_exclusions_do_not_match_production_files() {
        use crate::build::fs::matches_exclusion_at_depth;

        const ROOT: usize = 1;

        assert!(!matches_exclusion_at_depth(
            "wp-rocket.php",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(!matches_exclusion_at_depth(
            "index.php",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(!matches_exclusion_at_depth(
            "readme.txt",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(!matches_exclusion_at_depth(
            "style.css",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(!matches_exclusion_at_depth("inc", ROOT, ZIP_EXCLUSIONS));
        // Root-level production files that merely share the `php`/`package`
        // prefix boundary must survive.
        assert!(!matches_exclusion_at_depth(
            "licence-data.php",
            ROOT,
            ZIP_EXCLUSIONS
        ));
        assert!(!matches_exclusion_at_depth(
            "uninstall.php",
            ROOT,
            ZIP_EXCLUSIONS
        ));
    }

    /// Regression: `-x "*/php*"` also matched **nested** paths (zip's `*` spans
    /// `/`), which dropped the whole `vendor/wordpress/php-mcp-schema/` tree and
    /// made the shipped plugin fatal on load with
    /// "class WP\McpSchema\Server\Tools\DTO\Tool is not available".
    #[test]
    fn test_zip_exclusions_keep_nested_vendor_packages_sharing_a_prefix() {
        use crate::build::fs::matches_exclusion_at_depth;

        // wp-rocket/vendor/wordpress/php-mcp-schema → depth 3
        assert!(
            !matches_exclusion_at_depth("php-mcp-schema", 3, ZIP_EXCLUSIONS),
            "nested vendor dir starting with 'php' must not be excluded"
        );
        // wp-rocket/vendor/ocramius/package-versions → depth 3
        assert!(
            !matches_exclusion_at_depth("package-versions", 3, ZIP_EXCLUSIONS),
            "nested vendor dir starting with 'package' must not be excluded"
        );
        // A nested tooling config is likewise kept — the rsync step already
        // prunes the directories that would carry one.
        assert!(!matches_exclusion_at_depth(
            "phpunit.xml",
            2,
            ZIP_EXCLUSIONS
        ));

        // Dotfiles and gulpfile.js stay depth-independent.
        assert!(matches_exclusion_at_depth(".gitignore", 4, ZIP_EXCLUSIONS));
        assert!(matches_exclusion_at_depth("gulpfile.js", 4, ZIP_EXCLUSIONS));
    }

    #[test]
    fn test_zip_exclude_flags_render_anchored_patterns() {
        let flags = zip_exclude_flags();

        assert_eq!(
            flags,
            r#"-x "*/.*" -x "*/gulpfile.js" -x "wp-rocket/package*" -x "wp-rocket/php*""#
        );
        // The unanchored form is what caused the vendor-tree regression.
        assert!(
            !flags.contains(r#""*/php*""#),
            "php rule must stay anchored to the plugin root: {flags}"
        );
        assert!(
            !flags.contains(r#""*/package*""#),
            "package rule must stay anchored to the plugin root: {flags}"
        );
    }

    // =========================================================================
    // Windows Build Path (pure Rust staging + archiving)
    // =========================================================================

    /// Populate a repo dir mirroring the parts of WP Rocket that matter here.
    ///
    /// Deliberately has no `vendor/` — it is `.gitignore`d in the real repo and
    /// listed in [`RSYNC_EXCLUDES`]. Production dependencies appear only later,
    /// inside the staging copy (see [`seed_staging_vendor_like_composer`]).
    fn seed_repo_like_wp_rocket(repo_dir: &Path) {
        std::fs::create_dir_all(repo_dir.join("inc")).unwrap();
        std::fs::write(repo_dir.join("inc/bootstrap.php"), "<?php // inc").unwrap();

        // Root-level tooling that must not ship.
        std::fs::write(repo_dir.join("phpcs.xml"), "<ruleset/>").unwrap();
        std::fs::write(repo_dir.join("phpstan.neon.dist"), "params:").unwrap();
        std::fs::write(repo_dir.join("phpstan-baseline.neon"), "params:").unwrap();
        std::fs::write(repo_dir.join("package.json"), "{}").unwrap();
        std::fs::write(repo_dir.join("gulpfile.js"), "// gulp").unwrap();
        std::fs::write(repo_dir.join(".gitignore"), "vendor").unwrap();

        // Root-level production files that must ship.
        std::fs::write(repo_dir.join("wp-rocket.php"), "<?php // plugin").unwrap();
        std::fs::write(repo_dir.join("licence-data.php"), "<?php // licence").unwrap();
        std::fs::write(repo_dir.join("uninstall.php"), "<?php // uninstall").unwrap();

        // rsync-excluded dev directories.
        std::fs::create_dir_all(repo_dir.join("tests/Unit")).unwrap();
        std::fs::write(repo_dir.join("tests/Unit/FooTest.php"), "<?php").unwrap();
        std::fs::create_dir_all(repo_dir.join("src")).unwrap();
        std::fs::write(repo_dir.join("src/dev.js"), "// dev").unwrap();
    }

    /// Stand in for `composer install --no-dev` inside the staging copy.
    ///
    /// Includes the two vendor package names that collide with the root tooling
    /// prefixes — `php-mcp-schema` and `package-versions` — which is what the
    /// unanchored `-x "*/php*"` / `-x "*/package*"` patterns used to delete.
    fn seed_staging_vendor_like_composer(staging_plugin_dir: &Path) {
        let vendor = staging_plugin_dir.join("vendor");

        let schema_dto = vendor.join("wordpress/php-mcp-schema/src/Server/Tools/DTO");
        std::fs::create_dir_all(&schema_dto).unwrap();
        std::fs::write(schema_dto.join("Tool.php"), "<?php // Tool DTO").unwrap();

        let schema_common = vendor.join("wordpress/php-mcp-schema/src/Common");
        std::fs::create_dir_all(&schema_common).unwrap();
        std::fs::write(
            schema_common.join("AbstractDataTransferObject.php"),
            "<?php // base DTO",
        )
        .unwrap();

        let adapter = vendor.join("wordpress/mcp-adapter/includes/Domain/Tools");
        std::fs::create_dir_all(&adapter).unwrap();
        std::fs::write(adapter.join("McpTool.php"), "<?php // McpTool").unwrap();

        let pkg_versions = vendor.join("ocramius/package-versions/src");
        std::fs::create_dir_all(&pkg_versions).unwrap();
        std::fs::write(pkg_versions.join("Versions.php"), "<?php // versions").unwrap();

        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("autoload.php"), "<?php // autoload").unwrap();

        // Vendor packages routinely ship their own tooling manifests; those are
        // nested, so they must survive the root-anchored rules.
        std::fs::write(vendor.join("wordpress/php-mcp-schema/composer.json"), "{}").unwrap();
    }

    /// Path of the staging plugin directory for a context.
    fn staging_plugin_dir(context: &BuildContext) -> std::path::PathBuf {
        context
            .workspace_dir()
            .join(STAGING_DIR)
            .join(PLUGIN_DIR_NAME)
    }

    /// Read every entry name from a zip archive.
    fn archive_entry_names(archive_path: &Path) -> Vec<String> {
        let file = std::fs::File::open(archive_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    /// Read every file entry as `(name, bytes)`, sorted by name.
    ///
    /// Directory entries carry no payload and are covered by
    /// [`archive_entry_names`], so they are skipped here.
    fn archive_file_contents(archive_path: &Path) -> Vec<(String, Vec<u8>)> {
        let file = std::fs::File::open(archive_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entries = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let name = entry.name().to_string();
            if name.ends_with('/') {
                continue;
            }
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            entries.push((name, bytes));
        }
        entries.sort();
        entries
    }

    /// Drives the Windows pipeline (`prepare_staging` → `create_archive`) and
    /// asserts the produced archive matches what the Unix `zip -x` flags yield.
    ///
    /// Regression: the vendor trees under `php-mcp-schema/` and
    /// `package-versions/` were previously dropped, which made the shipped
    /// plugin fatal on load with "class WP\McpSchema\Server\Tools\DTO\Tool is
    /// not available".
    #[test]
    fn test_windows_pipeline_keeps_vendor_trees_and_drops_root_tooling() {
        let (_ws, context) = test_context();
        seed_repo_like_wp_rocket(context.repo_dir());

        let version = "3.17.4";
        builder()
            .prepare_staging(&context, &NullReporter)
            .expect("staging copy should succeed");
        seed_staging_vendor_like_composer(&staging_plugin_dir(&context));
        builder()
            .create_archive(&context, version, &NullReporter)
            .expect("archive creation should succeed");

        let archive = context.workspace_dir().join(artifact_name(version));
        let names = archive_entry_names(&archive);

        // The classes whose absence produced the PHP fatal error.
        for kept in [
            "wp-rocket/vendor/wordpress/php-mcp-schema/src/Server/Tools/DTO/Tool.php",
            "wp-rocket/vendor/wordpress/php-mcp-schema/src/Common/AbstractDataTransferObject.php",
            "wp-rocket/vendor/wordpress/mcp-adapter/includes/Domain/Tools/McpTool.php",
            "wp-rocket/vendor/ocramius/package-versions/src/Versions.php",
            "wp-rocket/vendor/wordpress/php-mcp-schema/composer.json",
            "wp-rocket/vendor/autoload.php",
            "wp-rocket/inc/bootstrap.php",
            "wp-rocket/wp-rocket.php",
            "wp-rocket/licence-data.php",
            "wp-rocket/uninstall.php",
        ] {
            assert!(
                names.contains(&kept.to_string()),
                "'{kept}' must ship; archive held {names:?}"
            );
        }

        // Root tooling and dotfiles must not ship.
        for dropped in [
            "wp-rocket/phpcs.xml",
            "wp-rocket/phpstan.neon.dist",
            "wp-rocket/phpstan-baseline.neon",
            "wp-rocket/package.json",
            "wp-rocket/gulpfile.js",
            "wp-rocket/.gitignore",
        ] {
            assert!(
                !names.contains(&dropped.to_string()),
                "'{dropped}' must not ship; archive held {names:?}"
            );
        }

        // rsync drops dev directories before the archive step runs.
        for pruned in ["wp-rocket/tests", "wp-rocket/src"] {
            assert!(
                !names.iter().any(|n| n.starts_with(pruned)),
                "'{pruned}' must be pruned by staging: {names:?}"
            );
        }
    }

    /// The Unix and Windows paths must agree. Rendering the shared
    /// [`ZIP_EXCLUSIONS`] through real `zip` and through [`create_zip_archive`]
    /// must yield the same entry set for the same input tree.
    #[test]
    #[cfg(unix)]
    fn test_unix_and_windows_archives_have_identical_entries() {
        use std::process::Command;

        // Skip when the `zip` binary is unavailable rather than failing the run.
        let zip_available = Command::new("zip")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !zip_available {
            eprintln!("skipping: `zip` binary not available");
            return;
        }

        let (_ws, context) = test_context();
        seed_repo_like_wp_rocket(context.repo_dir());

        // Windows path: stage with pure Rust, archive with the zip crate.
        let version = "3.17.4";
        builder().prepare_staging(&context, &NullReporter).unwrap();
        seed_staging_vendor_like_composer(&staging_plugin_dir(&context));
        builder()
            .create_archive(&context, version, &NullReporter)
            .unwrap();
        let rust_archive = context.workspace_dir().join(artifact_name(version));
        let mut rust_names: Vec<String> = archive_entry_names(&rust_archive);
        rust_names.sort();

        // Unix path: archive the same staging tree with the real `zip` binary,
        // using the flags the builder emits.
        let staging_dir = context.workspace_dir().join(STAGING_DIR);
        let shell_archive = context.workspace_dir().join("shell.zip");
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "cd {} && zip -r {} {} {}",
                staging_dir.display(),
                shell_archive.display(),
                PLUGIN_DIR_NAME,
                zip_exclude_flags()
            ))
            .stdout(std::process::Stdio::null())
            .status()
            .expect("zip should run");
        assert!(status.success(), "shell zip failed");

        let mut shell_names: Vec<String> = archive_entry_names(&shell_archive);
        shell_names.sort();

        // Complete entry sets — files *and* directory entries, including the
        // archive root. No filtering, so any future divergence fails here.
        assert_eq!(
            rust_names, shell_names,
            "Windows (pure Rust) and Unix (shell zip) archives must contain identical entries"
        );
        assert!(
            rust_names.contains(&format!("{PLUGIN_DIR_NAME}/")),
            "both archives must record the plugin root entry: {rust_names:?}"
        );

        // Matching names alone would not catch truncated or mis-written payloads.
        // (The two paths may pick different per-entry compression methods; the
        // decompressed bytes must still be identical.)
        assert_eq!(
            archive_file_contents(&rust_archive),
            archive_file_contents(&shell_archive),
            "file payloads must match byte-for-byte across both archive paths"
        );
    }
}
