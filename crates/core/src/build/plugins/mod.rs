//! Project-specific builders.

mod bwu;

pub use bwu::BackWPupBuilder;

/// A buildable variant of a project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildVariant {
    /// Unique identifier for this variant (e.g., "free", "pro-en", "pro-de").
    pub id: &'static str,
    /// Human-readable name for display.
    pub name: &'static str,
    /// Description of what this variant produces.
    pub description: &'static str,
}

/// An optional command that can be installed if missing.
pub struct OptionalCommand {
    /// Command name to check.
    pub name: &'static str,
    /// Installation command if missing.
    pub install_cmd: &'static str,
}

/// Trait for project-specific build logic.
pub trait Builder: Send + Sync {
    /// Get required system commands that must be available.
    fn required_commands(&self) -> Vec<&'static str>;

    /// Get optional commands that will be installed if missing.
    fn optional_commands(&self) -> Vec<OptionalCommand> {
        vec![]
    }

    /// Get setup commands (run once before building).
    fn setup_commands(&self) -> Vec<String>;

    /// Get all available build variants for this project.
    fn available_variants(&self) -> Vec<BuildVariant>;

    /// Get build commands for specific variants.
    /// If `variants` is empty, build all available variants.
    fn build_commands(&self, version: &str, variants: &[&str]) -> Vec<String>;

    /// Get the subdirectory where the build should run (relative to repo root).
    fn build_subdirectory(&self) -> Option<&'static str> {
        None
    }

    /// Validate that the requested variants are available.
    fn validate_variants(&self, variants: &[&str]) -> Result<(), String> {
        let available: Vec<&str> = self.available_variants().iter().map(|v| v.id).collect();
        
        for variant in variants {
            if !available.contains(variant) {
                return Err(format!(
                    "Unknown variant '{}'. Available variants: {}",
                    variant,
                    available.join(", ")
                ));
            }
        }
        
        Ok(())
    }
}
