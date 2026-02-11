//! Build system.
//!
//! This module provides the build infrastructure for APVM:
//!
//! - [`plugins`] - Project-specific builders (BackWPup, WP Rocket, etc.)
//! - [`BuildRunner`] - Executes build commands and collects artifacts
//! - [`BuildResult`] - Output from a build operation
//!
//! # Version Handling
//!
//! The [`plugins::VersionRequirement`] enum controls how version is handled:
//!
//! - [`plugins::VersionRequirement::Required`] - Version must be provided
//! - [`plugins::VersionRequirement::Embedded`] - Version must NOT be provided (will be ignored)
//! - [`plugins::VersionRequirement::Optional`] - Version can be provided or auto-detected
//!
//! # Version Detection
//!
//! The [`plugins`] module includes helpers for auto-detecting versions:
//!
//! - [`plugins::detect_wordpress_plugin_version`] - Parse PHP plugin headers
//! - [`plugins::detect_wordpress_readme_version`] - Parse readme.txt stable tag

mod context;
pub mod plugins;
pub mod progress;
mod result;
mod runner;

pub use context::BuildContext;
pub use progress::{BuildEvent, BuildPhase, BuildStep, ClosureReporter, NullReporter, ProgressReporter};
pub use result::{BuildResult, ProducedArtifact};
pub use runner::{BuildOutput, BuildRunner};

// Re-export version detection helpers for convenience
pub use plugins::{detect_wordpress_plugin_version, detect_wordpress_readme_version};

// Re-export VersionRequirement for consumers
pub use plugins::VersionRequirement;
