//! Build plugins for different projects.

mod bwu;

pub use bwu::BwuBuilder;

/// Trait for project-specific builders.
pub trait Builder: Send + Sync {
    /// Get the build command for the given version.
    fn build_command(&self, version: &str) -> String;

    /// Get any setup commands to run before the build.
    fn setup_commands(&self) -> Vec<String> {
        vec![]
    }
}
