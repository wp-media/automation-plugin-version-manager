//! Project-specific builders.

mod bwu;

use std::path::PathBuf;

pub use bwu::BackWPupBuilder;

use crate::Result;
use crate::error::Error;

/// A buildable variant of a project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildVariant {
    /// Unique identifier for this variant.
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

/// Describes a build artifact (output file).
#[derive(Debug, Clone)]
pub struct BuildArtifact {
    /// Variant ID this artifact belongs to (None if single-variant project).
    pub variant_id: Option<String>,
    /// Source path (relative to build dir).
    pub source_path: String,
    /// Target filename.
    pub target_name: String,
}

/// Trait for project-specific build logic.
pub trait Builder: Send + Sync {
    /// Get required system commands.
    fn required_commands(&self) -> Vec<&'static str>;

    /// Get optional commands that will be installed if missing.
    fn optional_commands(&self) -> Vec<OptionalCommand> {
        vec![]
    }

    /// Get setup commands (run once before building).
    fn setup_commands(&self) -> Vec<String>;

    /// Hook called before the build starts.
    fn pre_build_hook(&self, _working_dir: &PathBuf, _version: &str, _variants: &[&str]) -> Result<()> {
        Ok(())
    }

    /// Hook called during the build process. Useful if no build commands are defined.
    fn build_hook(&self, _working_dir: &PathBuf, _version: &str, _variants: &[&str]) -> Result<()> {
        Ok(())
    }

    /// Hook called after the build completes.
    fn post_build_hook(&self, _working_dir: &PathBuf, _version: &str, _variants: &[&str]) -> Result<()> {
        Ok(())
    }

    /// Get all available build variants.
    /// Returns empty Vec if project has no variants (single output).
    fn variants(&self) -> Vec<BuildVariant> {
        vec![]
    }

    /// Check if this project has multiple variants.
    fn has_variants(&self) -> bool {
        self.variants().len() > 1
    }

    /// Get build commands.
    /// If project has variants, only requested ones are built.
    /// If no variants, the `variants` parameter is ignored.
    fn build_commands(&self, version: &str, variants: &[&str]) -> Vec<String>;

    /// Get the artifacts produced by the build.
    ///
    /// This method is called after the build completes. Builders should return
    /// resolved, concrete paths to the produced artifacts. If the builder needs
    /// to use glob patterns to locate files (e.g., when filenames contain commit
    /// hashes), it should resolve them internally before returning.
    ///
    /// # Arguments
    ///
    /// * `working_dir` - The directory where the build ran
    /// * `version` - The version that was built
    /// * `variants` - The variants that were built (empty = all)
    ///
    /// # Returns
    ///
    /// A list of artifacts with resolved source paths.
    fn artifacts(&self, working_dir: &PathBuf, version: &str, variants: &[&str]) -> Result<Vec<BuildArtifact>>;

    /// Get the subdirectory where the build should run.
    fn build_subdirectory(&self) -> Option<&'static str> {
        None
    }

    /// Validate requested variants.
    fn validate_variants(&self, requested: &[&str]) -> Result<()> {
        if !self.has_variants() {
            // No variants = ignore the request, build everything
            return Ok(());
        }

        let available: Vec<&str> = self.variants().iter().map(|v| v.id).collect();
        for variant in requested {
            if !available.contains(variant) {
                return Err(Error::Build(format!(
                    "Unknown variant '{}'. Available: {}",
                    variant,
                    available.join(", ")
                )));
            }
        }
        Ok(())
    }
}
