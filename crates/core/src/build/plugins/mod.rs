//! Project-specific builders.

mod bwu;

pub use bwu::BackWPupBuilder;

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

    /// Get build commands for a specific version.
    fn build_commands(&self, version: &str) -> Vec<String>;

    /// Get the subdirectory where the build should run (relative to repo root).
    fn build_subdirectory(&self) -> Option<&'static str> {
        None
    }
}

/// An optional command that can be installed if missing.
pub struct OptionalCommand {
    /// Command name to check.
    pub name: &'static str,
    /// Installation command if missing.
    pub install_cmd: &'static str,
}
