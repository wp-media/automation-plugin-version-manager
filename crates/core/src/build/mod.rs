//! Build system.

pub mod plugins;
mod result;
mod runner;

pub use result::{BuildResult, ProducedArtifact};
pub use runner::{BuildOutput, BuildRunner};
