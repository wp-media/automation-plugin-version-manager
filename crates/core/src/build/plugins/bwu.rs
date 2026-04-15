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
