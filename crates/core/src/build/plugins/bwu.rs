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

use std::path::{Path, PathBuf};
use crate::Result;
use super::{BuildArtifact, BuildVariant, Builder, OptionalCommand, VersionRequirement};

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

    fn required_commands(&self) -> Vec<&'static str> {
        vec!["npm", "composer"]
    }

    fn optional_commands(&self) -> Vec<OptionalCommand> {
        vec![OptionalCommand {
            name: "gulp",
            install_cmd: "npm install --global gulp-cli",
        }]
    }

    fn setup_commands(&self) -> Vec<String> {
        vec![
            "composer install --no-dev --prefer-dist --no-progress --no-interaction".to_string(),
            "npm install --no-audit --no-fund --no-progress".to_string(),
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
    // Build Hooks
    // =========================================================================

    fn pre_build_hook(&self, working_dir: &PathBuf, _version: &str, _variants: &[&str]) -> Result<()> {
        // Remove previous build artifacts matching backwpup-*.zip pattern
        let pattern = working_dir.join("backwpup-*.zip");
        let pattern_str = pattern.to_string_lossy();

        let entries = glob::glob(&pattern_str).map_err(|_| {
            crate::error::Error::Build(format!("Failed to read glob pattern in backwpup pre_build_hook: {}", pattern_str))
        })?;

        for entry in entries.flatten() {
            std::fs::remove_file(&entry).map_err(|e| {
                crate::error::Error::Build(format!("Failed to remove previous artifact {}: {}", entry.display(), e))
            })?;
        }

        Ok(())
    }

    // =========================================================================
    // Build Execution
    // =========================================================================

    fn build_commands(&self, version: &str, variants: &[&str]) -> Vec<String> {
        // Determine which variants to build
        let to_build: Vec<&str> = if variants.is_empty() {
            // Build all variants if none specified
            self.variants().iter().map(|v| v.id).collect()
        } else {
            variants.to_vec()
        };

        // Common asset build commands (always run first)
        let mut commands = vec![
            "gulp buildAssets".to_string(),
            "npx tailwindcss -i ./src/input.css -o ./assets/css/backwpup-admin.css".to_string(),
        ];

        // Add variant-specific commands
        for variant in to_build {
            match variant {
                Self::VARIANT_FREE => {
                    commands.push(format!(
                        "gulp free --packageVersion=\"{}\" --compressPath=.",
                        version
                    ));
                }
                Self::VARIANT_PRO_DE => {
                    commands.push(format!(
                        "gulp pro --packageVersion=\"{}\" --compressPath=. --language=de",
                        version
                    ));
                }
                Self::VARIANT_PRO_EN => {
                    commands.push(format!(
                        "gulp pro --packageVersion=\"{}\" --compressPath=. --language=en",
                        version
                    ));
                }
                _ => {} // Unknown variants are ignored (already validated)
            }
        }

        commands
    }

    fn artifacts(&self, working_dir: &PathBuf, version: &str, variants: &[&str]) -> crate::Result<Vec<BuildArtifact>> {
        let to_build = if variants.is_empty() {
            vec![Self::VARIANT_FREE, Self::VARIANT_PRO_DE, Self::VARIANT_PRO_EN]
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

            let full_pattern = working_dir.join(&pattern);
            let pattern_str = full_pattern.to_string_lossy();

            let matches: Vec<_> = glob::glob(&pattern_str)
                .map_err(|e| crate::error::Error::Build(format!(
                    "Invalid glob pattern '{}': {}", pattern, e
                )))?
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
                        matches.iter()
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
}
