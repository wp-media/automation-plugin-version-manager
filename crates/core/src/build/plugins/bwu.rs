//! BackWPup project builder.
//!
//! BackWPup is a WordPress backup plugin that requires the version to be passed
//! to its build commands (`gulp --packageVersion="{version}"`).
//!
//! # Version Handling
//!
//! This builder **requires** an explicit version because:
//! - The gulp tasks embed the version into the output zip filename
//! - The version is injected into plugin files during build
//!
//! Auto-detection is not supported because the version in source files
//! may not match the intended release version.
//!
//! # Variants
//!
//! BackWPup produces three variants:
//! - `free` - Free version for WordPress.org
//! - `pro-de` - Pro version with German translations
//! - `pro-en` - Pro version with English translations

use super::super::BuildContext;
use super::super::progress::{BuildEvent, BuildStep, ProgressReporter};
use super::{BuildArtifact, BuildVariant, Builder, ToolDependency, VersionRequirement};
use crate::Result;
use std::path::Path;

/// Builder for the BackWPup project.
pub struct BackWPupBuilder;

impl BackWPupBuilder {
    /// Variant ID for free version.
    pub const VARIANT_FREE: &'static str = "free";
    /// Variant ID for Pro German version.
    pub const VARIANT_PRO_DE: &'static str = "pro-de";
    /// Variant ID for Pro English version.
    pub const VARIANT_PRO_EN: &'static str = "pro-en";
}

impl Builder for BackWPupBuilder {
    // =========================================================================
    // Version Handling
    // =========================================================================

    /// BackWPup requires version for building.
    ///
    /// The gulp tasks use `--packageVersion` to:
    /// 1. Name the output zip file (e.g., `backwpup-5.1.0-abc123.zip`)
    /// 2. Inject version into plugin headers
    fn version_requirement(&self) -> VersionRequirement {
        VersionRequirement::Required
    }

    /// BackWPup does not support version auto-detection.
    ///
    /// While the source files contain version headers, the build process
    /// is designed to receive the version externally to support building
    /// pre-release versions from branches.
    fn detect_version(&self, _working_dir: &Path) -> Result<Option<String>> {
        Ok(None)
    }

    // =========================================================================
    // Commands and Setup
    // =========================================================================

    fn tool_dependencies(&self) -> Vec<ToolDependency> {
        vec![
            ToolDependency::required("npm"),
            ToolDependency::required("composer"),
            ToolDependency::required_with_install("gulp", vec!["npm install --global gulp-cli"]),
        ]
    }

    fn setup_commands(&self) -> Vec<BuildStep> {
        vec![
            BuildStep::new(
                "Installing PHP dependencies",
                "composer install --no-dev --prefer-dist --no-progress --no-interaction",
            ),
            BuildStep::new(
                "Installing JS dependencies",
                "npm install --no-audit --no-fund --no-progress",
            ),
        ]
    }

    // =========================================================================
    // Variants
    // =========================================================================

    fn variants(&self) -> Vec<BuildVariant> {
        vec![
            BuildVariant {
                id: Self::VARIANT_FREE,
                name: "Free",
                description: "BackWPup Free version",
            },
            BuildVariant {
                id: Self::VARIANT_PRO_DE,
                name: "Pro (German)",
                description: "BackWPup Pro German version",
            },
            BuildVariant {
                id: Self::VARIANT_PRO_EN,
                name: "Pro (English)",
                description: "BackWPup Pro English version",
            },
        ]
    }

    // =========================================================================
    // Defaults
    // =========================================================================

    /// Default version for BackWPup builds.
    ///
    /// Uses `9.99.99` as a development version marker to indicate
    /// this is not an official release.
    fn default_version(&self) -> Option<&'static str> {
        Some("9.99.99")
    }

    /// Default variants for BackWPup builds.
    ///
    /// Builds `free` and `pro-en` by default, skipping `pro-de`
    /// which is rarely needed in QA testing.
    fn default_variants(&self) -> Vec<&'static str> {
        vec![Self::VARIANT_FREE, Self::VARIANT_PRO_EN]
    }

    // =========================================================================
    // Build Hooks
    // =========================================================================

    fn pre_build_hook(
        &self,
        context: &BuildContext,
        _version: &str,
        _variants: &[&str],
        reporter: &dyn ProgressReporter,
    ) -> Result<()> {
        // Remove previous build artifacts matching backwpup-*.zip pattern
        reporter.report(&BuildEvent::StepStarted {
            step: BuildStep::new("Cleaning previous artifacts", "rm backwpup-*.zip"),
        });
        let pattern = context.repo_dir().join("backwpup-*.zip");
        let pattern_str = pattern.to_string_lossy();

        let entries = glob::glob(&pattern_str).map_err(|_| {
            crate::error::Error::Build(format!(
                "Failed to read glob pattern in backwpup pre_build_hook: {}",
                pattern_str
            ))
        })?;

        for entry in entries.flatten() {
            std::fs::remove_file(&entry).map_err(|e| {
                crate::error::Error::Build(format!(
                    "Failed to remove previous artifact {}: {}",
                    entry.display(),
                    e
                ))
            })?;
        }

        reporter.report(&BuildEvent::StepCompleted {
            step: BuildStep::new("Cleaning previous artifacts", "rm backwpup-*.zip"),
        });

        Ok(())
    }

    // =========================================================================
    // Build Execution
    // =========================================================================

    fn build_commands(
        &self,
        _context: &BuildContext,
        version: &str,
        variants: &[&str],
    ) -> Vec<BuildStep> {
        // Determine which variants to build
        let to_build: Vec<&str> = if variants.is_empty() {
            // Build all variants if none specified
            self.variants().iter().map(|v| v.id).collect()
        } else {
            variants.to_vec()
        };

        // Common asset build commands (always run first)
        let mut commands = vec![
            BuildStep::new("Building assets", "gulp buildAssets"),
            BuildStep::new(
                "Compiling Tailwind CSS",
                "npx tailwindcss -i ./src/input.css -o ./assets/css/backwpup-admin.css",
            ),
        ];

        // Add variant-specific commands
        for variant in to_build {
            match variant {
                Self::VARIANT_FREE => {
                    commands.push(BuildStep::new(
                        "Building Free variant",
                        format!(
                            "gulp free --packageVersion=\"{}\" --compressPath=.",
                            version
                        ),
                    ));
                }
                Self::VARIANT_PRO_DE => {
                    commands.push(BuildStep::new(
                        "Building Pro (German) variant",
                        format!(
                            "gulp pro --packageVersion=\"{}\" --compressPath=. --language=de",
                            version
                        ),
                    ));
                }
                Self::VARIANT_PRO_EN => {
                    commands.push(BuildStep::new(
                        "Building Pro (English) variant",
                        format!(
                            "gulp pro --packageVersion=\"{}\" --compressPath=. --language=en",
                            version
                        ),
                    ));
                }
                _ => {} // Unknown variants are ignored (already validated)
            }
        }

        commands
    }

    fn artifacts(
        &self,
        context: &BuildContext,
        version: &str,
        variants: &[&str],
    ) -> crate::Result<Vec<BuildArtifact>> {
        let to_build = if variants.is_empty() {
            vec![
                Self::VARIANT_FREE,
                Self::VARIANT_PRO_DE,
                Self::VARIANT_PRO_EN,
            ]
        } else {
            variants.to_vec()
        };

        // Actual build output format (includes 8-char commit hash):
        // - Free:   backwpup-{version}-{commit8}.zip
        // - Pro DE: backwpup-pro-de-{version}-{commit8}.zip
        // - Pro EN: backwpup-pro-en-{version}-{commit8}.zip
        //
        // We use glob to find the actual files since commit hash is embedded in filename.
        // pre_build_hook cleans all backwpup-*.zip, so only current build artifacts exist.
        let mut artifacts = Vec::new();

        for variant in to_build {
            let pattern = match variant {
                Self::VARIANT_FREE => format!("backwpup-{}-*.zip", version),
                Self::VARIANT_PRO_DE => format!("backwpup-pro-de-{}-*.zip", version),
                Self::VARIANT_PRO_EN => format!("backwpup-pro-en-{}-*.zip", version),
                _ => continue,
            };

            let full_pattern = context.repo_dir().join(&pattern);
            let pattern_str = full_pattern.to_string_lossy();

            let matches: Vec<_> = glob::glob(&pattern_str)
                .map_err(|e| {
                    crate::error::Error::Build(format!("Invalid glob pattern '{}': {}", pattern, e))
                })?
                .filter_map(|r| r.ok())
                .filter(|p| p.is_file())
                .collect();

            match matches.len() {
                0 => {
                    return Err(crate::error::Error::Build(format!(
                        "No artifact found matching '{}'. Build may have failed.",
                        pattern
                    )));
                }
                1 => {
                    let path = &matches[0];
                    let filename = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| pattern.clone());

                    // source_path is relative to working_dir
                    artifacts.push(BuildArtifact {
                        variant_id: Some(variant.to_string()),
                        source_path: filename.clone(),
                        target_name: filename, // Keep original name with commit hash
                    });
                }
                n => {
                    return Err(crate::error::Error::Build(format!(
                        "Pattern '{}' matched {} files (expected 1): {}",
                        pattern,
                        n,
                        matches
                            .iter()
                            .filter_map(|p| p.file_name())
                            .map(|s| s.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }

        Ok(artifacts)
    }

    // =========================================================================
    // Release Asset Matching
    // =========================================================================

    /// Match BackWPup release assets.
    ///
    /// Release assets follow the pattern `backwpup-{variant}-{version}.zip`:
    /// - `backwpup-free-5.6.8.zip`
    /// - `backwpup-pro-de-5.6.8.zip`
    /// - `backwpup-pro-en-5.6.8.zip`
    fn matches_release_asset(&self, asset_name: &str) -> bool {
        asset_name.starts_with("backwpup-") && asset_name.ends_with(".zip")
    }

    /// Determine which variant a BackWPup release asset belongs to.
    fn variant_from_release_asset(&self, asset_name: &str) -> Option<String> {
        if asset_name.starts_with("backwpup-pro-de-") {
            Some(Self::VARIANT_PRO_DE.to_string())
        } else if asset_name.starts_with("backwpup-pro-en-") {
            Some(Self::VARIANT_PRO_EN.to_string())
        } else if asset_name.starts_with("backwpup-free-") || asset_name.starts_with("backwpup-") {
            // "backwpup-free-X.Y.Z.zip" or legacy "backwpup-X.Y.Z.zip"
            Some(Self::VARIANT_FREE.to_string())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildContext;
    use crate::build::progress::NullReporter;
    use tempfile::TempDir;

    fn builder() -> BackWPupBuilder {
        BackWPupBuilder
    }

    /// Create a test build context. Returns (TempDir, BuildContext).
    fn test_context() -> (TempDir, BuildContext) {
        let workspace = TempDir::new().unwrap();
        let repo_dir = workspace.path().join("backwpup");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let context = BuildContext::new(repo_dir, workspace.path().to_path_buf());
        (workspace, context)
    }

    // =========================================================================
    // 1.1 – Version Handling
    // =========================================================================

    #[test]
    fn test_version_requirement_is_required() {
        assert_eq!(
            builder().version_requirement(),
            VersionRequirement::Required
        );
    }

    #[test]
    fn test_detect_version_returns_none() {
        let dir = TempDir::new().unwrap();
        let version = builder().detect_version(dir.path()).unwrap();
        assert_eq!(version, None);
    }

    // =========================================================================
    // 1.3 – Tool Dependencies
    // =========================================================================

    #[test]
    fn test_tool_dependencies() {
        let deps = builder().tool_dependencies();
        assert_eq!(deps.len(), 3);

        let names: Vec<&str> = deps.iter().map(|d| d.name).collect();
        assert!(names.contains(&"npm"));
        assert!(names.contains(&"composer"));
        assert!(names.contains(&"gulp"));

        // npm and composer are required without install commands
        let npm = deps.iter().find(|d| d.name == "npm").unwrap();
        assert!(npm.required);
        assert!(npm.install_commands.is_empty());

        let composer = deps.iter().find(|d| d.name == "composer").unwrap();
        assert!(composer.required);
        assert!(composer.install_commands.is_empty());

        // gulp is required WITH install commands
        let gulp = deps.iter().find(|d| d.name == "gulp").unwrap();
        assert!(gulp.required);
        assert!(!gulp.install_commands.is_empty());
        assert!(gulp.install_commands[0].contains("npm install --global gulp-cli"));
    }

    // =========================================================================
    // 1.4 – Setup Commands
    // =========================================================================

    #[test]
    fn test_setup_commands() {
        let cmds = builder().setup_commands();
        assert_eq!(cmds.len(), 2);

        // First: composer install
        assert!(cmds[0].command.contains("composer install"));
        assert!(cmds[0].command.contains("--no-dev"));
        assert!(cmds[0].label.contains("PHP"));

        // Second: npm install
        assert!(cmds[1].command.contains("npm install"));
        assert!(cmds[1].label.contains("JS"));
    }

    // =========================================================================
    // 1.5–1.6 – Variants
    // =========================================================================

    #[test]
    fn test_variants() {
        let variants = builder().variants();
        assert_eq!(variants.len(), 3);

        let ids: Vec<&str> = variants.iter().map(|v| v.id).collect();
        assert!(ids.contains(&BackWPupBuilder::VARIANT_FREE));
        assert!(ids.contains(&BackWPupBuilder::VARIANT_PRO_DE));
        assert!(ids.contains(&BackWPupBuilder::VARIANT_PRO_EN));

        // Verify names are non-empty
        for v in &variants {
            assert!(!v.name.is_empty());
            assert!(!v.description.is_empty());
        }
    }

    #[test]
    fn test_has_variants_true() {
        assert!(builder().has_variants());
    }

    // =========================================================================
    // 1.7–1.8 – Defaults
    // =========================================================================

    #[test]
    fn test_default_version() {
        assert_eq!(builder().default_version(), Some("9.99.99"));
    }

    #[test]
    fn test_default_variants() {
        let defaults = builder().default_variants();
        assert_eq!(defaults.len(), 2);
        assert!(defaults.contains(&BackWPupBuilder::VARIANT_FREE));
        assert!(defaults.contains(&BackWPupBuilder::VARIANT_PRO_EN));
        // pro-de excluded by default
        assert!(!defaults.contains(&BackWPupBuilder::VARIANT_PRO_DE));
    }

    // =========================================================================
    // 1.9–1.13 – Build Commands
    // =========================================================================

    #[test]
    fn test_build_commands_all_variants() {
        let (_ws, context) = test_context();
        let commands = builder().build_commands(&context, "5.1.0", &[]);

        // 2 common (buildAssets + tailwind) + 3 variant commands = 5
        assert_eq!(commands.len(), 5);

        // First two are always the asset builds
        assert!(commands[0].command.contains("gulp buildAssets"));
        assert!(commands[1].command.contains("tailwindcss"));

        // Then one command per variant
        let cmd_strs: Vec<&str> = commands[2..].iter().map(|c| c.command.as_str()).collect();
        assert!(cmd_strs.iter().any(|c| c.contains("gulp free")));
        assert!(
            cmd_strs
                .iter()
                .any(|c| c.contains("gulp pro") && c.contains("--language=de"))
        );
        assert!(
            cmd_strs
                .iter()
                .any(|c| c.contains("gulp pro") && c.contains("--language=en"))
        );
    }

    #[test]
    fn test_build_commands_specific_variants() {
        let (_ws, context) = test_context();
        let commands = builder().build_commands(&context, "5.1.0", &["free"]);

        // 2 common + 1 variant = 3
        assert_eq!(commands.len(), 3);
        assert!(commands[2].command.contains("gulp free"));
    }

    #[test]
    fn test_build_commands_version_in_command() {
        let (_ws, context) = test_context();
        let commands = builder().build_commands(&context, "5.1.0", &["free"]);

        // Version appears in --packageVersion
        assert!(commands[2].command.contains("--packageVersion=\"5.1.0\""));
    }

    #[test]
    fn test_build_commands_pro_de_language_flag() {
        let (_ws, context) = test_context();
        let commands = builder().build_commands(&context, "5.1.0", &["pro-de"]);

        assert!(commands[2].command.contains("--language=de"));
        assert!(commands[2].command.contains("gulp pro"));
    }

    #[test]
    fn test_build_commands_pro_en_language_flag() {
        let (_ws, context) = test_context();
        let commands = builder().build_commands(&context, "5.1.0", &["pro-en"]);

        assert!(commands[2].command.contains("--language=en"));
        assert!(commands[2].command.contains("gulp pro"));
    }

    // =========================================================================
    // 1.14–1.15 – Pre-build Hook
    // =========================================================================

    #[test]
    fn test_pre_build_hook_removes_artifacts() {
        let (_ws, context) = test_context();

        // Create dummy artifacts in repo_dir
        let zip1 = context.repo_dir().join("backwpup-5.1.0-abcd1234.zip");
        let zip2 = context
            .repo_dir()
            .join("backwpup-pro-en-5.1.0-abcd1234.zip");
        std::fs::File::create(&zip1).unwrap();
        std::fs::File::create(&zip2).unwrap();
        assert!(zip1.exists());
        assert!(zip2.exists());

        builder()
            .pre_build_hook(&context, "5.1.0", &[], &NullReporter)
            .unwrap();

        assert!(!zip1.exists());
        assert!(!zip2.exists());
    }

    #[test]
    fn test_pre_build_hook_noop_no_artifacts() {
        let (_ws, context) = test_context();
        // No artifacts exist — should not error
        builder()
            .pre_build_hook(&context, "5.1.0", &[], &NullReporter)
            .unwrap();
    }

    // =========================================================================
    // 1.16–1.18 – Artifacts
    // =========================================================================

    #[test]
    fn test_artifacts_found() {
        let (_ws, context) = test_context();
        let version = "5.1.0";

        // Create expected artifact files in repo_dir (simulates gulp output)
        let free = context
            .repo_dir()
            .join(format!("backwpup-{}-abcd1234.zip", version));
        let pro_en = context
            .repo_dir()
            .join(format!("backwpup-pro-en-{}-abcd1234.zip", version));
        std::fs::File::create(&free).unwrap();
        std::fs::File::create(&pro_en).unwrap();

        let artifacts = builder()
            .artifacts(&context, version, &["free", "pro-en"])
            .unwrap();
        assert_eq!(artifacts.len(), 2);

        let filenames: Vec<&str> = artifacts.iter().map(|a| a.target_name.as_str()).collect();
        assert!(
            filenames
                .iter()
                .any(|f| f.starts_with("backwpup-5.1.0-") && !f.contains("pro"))
        );
        assert!(
            filenames
                .iter()
                .any(|f| f.starts_with("backwpup-pro-en-5.1.0-"))
        );

        // All have variant_id set
        for a in &artifacts {
            assert!(a.variant_id.is_some());
        }
    }

    #[test]
    fn test_artifacts_not_found_error() {
        let (_ws, context) = test_context();
        let result = builder().artifacts(&context, "5.1.0", &["free"]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No artifact found"));
    }

    #[test]
    fn test_artifacts_multiple_matches_error() {
        let (_ws, context) = test_context();
        let version = "5.1.0";

        // Create two files matching the same glob pattern
        let dup1 = context
            .repo_dir()
            .join(format!("backwpup-{}-aaaa1111.zip", version));
        let dup2 = context
            .repo_dir()
            .join(format!("backwpup-{}-bbbb2222.zip", version));
        std::fs::File::create(&dup1).unwrap();
        std::fs::File::create(&dup2).unwrap();

        let result = builder().artifacts(&context, version, &["free"]);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("matched"));
        assert!(err.contains("2"));
    }

    // =========================================================================
    // 1.19–1.22 – Release Asset Matching
    // =========================================================================

    #[test]
    fn test_matches_release_asset_valid() {
        let b = builder();
        assert!(b.matches_release_asset("backwpup-free-5.6.8.zip"));
        assert!(b.matches_release_asset("backwpup-pro-de-5.6.8.zip"));
        assert!(b.matches_release_asset("backwpup-pro-en-5.6.8.zip"));
        assert!(b.matches_release_asset("backwpup-5.6.8.zip")); // legacy format
    }

    #[test]
    fn test_matches_release_asset_invalid() {
        let b = builder();
        assert!(!b.matches_release_asset("unrelated.zip"));
        assert!(!b.matches_release_asset("backwpup-free-5.6.8.tar.gz"));
        assert!(!b.matches_release_asset("wp-rocket-3.17.4.zip"));
        assert!(!b.matches_release_asset(""));
    }

    #[test]
    fn test_variant_from_release_asset() {
        let b = builder();
        assert_eq!(
            b.variant_from_release_asset("backwpup-free-5.6.8.zip"),
            Some(BackWPupBuilder::VARIANT_FREE.to_string())
        );
        assert_eq!(
            b.variant_from_release_asset("backwpup-pro-de-5.6.8.zip"),
            Some(BackWPupBuilder::VARIANT_PRO_DE.to_string())
        );
        assert_eq!(
            b.variant_from_release_asset("backwpup-pro-en-5.6.8.zip"),
            Some(BackWPupBuilder::VARIANT_PRO_EN.to_string())
        );
        // Legacy format (no variant prefix) → mapped to free
        assert_eq!(
            b.variant_from_release_asset("backwpup-5.6.8.zip"),
            Some(BackWPupBuilder::VARIANT_FREE.to_string())
        );
    }

    #[test]
    fn test_variant_from_release_asset_unknown() {
        let b = builder();
        assert_eq!(b.variant_from_release_asset("unrelated.zip"), None);
        assert_eq!(b.variant_from_release_asset(""), None);
    }
}
