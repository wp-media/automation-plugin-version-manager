//! BackWPup project builder.

use super::{Builder, BuildVariant, OptionalCommand};

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

    fn available_variants(&self) -> Vec<BuildVariant> {
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

    fn build_commands(&self, version: &str, variants: &[&str]) -> Vec<String> {
        // Determine which variants to build
        let variants_to_build: Vec<&str> = if variants.is_empty() {
            // Build all variants if none specified
            self.available_variants().iter().map(|v| v.id).collect()
        } else {
            variants.to_vec()
        };

        // Common asset build commands (always run first)
        let mut commands = vec![
            "gulp buildAssets".to_string(),
            "npx tailwindcss -i ./src/input.css -o ./assets/css/backwpup-admin.css".to_string(),
        ];

        // Add variant-specific commands
        for variant in variants_to_build {
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
}
