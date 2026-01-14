//! BackWPup project builder.

use std::path::PathBuf;
use crate::Result;
use super::{BuildArtifact, BuildVariant, Builder, OptionalCommand};

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
            "composer install".to_string(),
            "npm install".to_string(),
        ]
    }

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

    fn artifacts(&self, version: &str, variants: &[&str]) -> Vec<BuildArtifact> {
        let to_build = if variants.is_empty() {
            vec![Self::VARIANT_FREE, Self::VARIANT_PRO_DE, Self::VARIANT_PRO_EN]
        } else {
            variants.to_vec()
        };

        to_build
            .iter()
            .filter_map(|variant| {
                let (source, target) = match *variant {
                    Self::VARIANT_FREE => (
                        format!("backwpup.{}.zip", version),
                        format!("backwpup-{}-free.zip", version),
                    ),
                    Self::VARIANT_PRO_DE => (
                        format!("backwpup-pro.{}.de.zip", version),
                        format!("backwpup-{}-pro-de.zip", version),
                    ),
                    Self::VARIANT_PRO_EN => (
                        format!("backwpup-pro.{}.en.zip", version),
                        format!("backwpup-{}-pro-en.zip", version),
                    ),
                    _ => return None,
                };

                Some(BuildArtifact {
                    variant_id: Some(variant.to_string()),
                    source_path: source,
                    target_name: target,
                })
            })
            .collect()
    }
}
