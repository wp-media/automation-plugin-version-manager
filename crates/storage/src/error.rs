//! Storage error types.

use std::path::PathBuf;

/// Storage error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Artifact not found at expected path.
    #[error("Artifact not found: {0}")]
    ArtifactNotFound(PathBuf),

    /// Build not found for the given parameters.
    #[error("Build not found: project={0}, version={1}, commit={2}")]
    BuildNotFound(String, String, String),

    /// Source not found (no link exists for the source).
    #[error("Source not found: project={0}, version={1}, source={2}")]
    SourceNotFound(String, String, String),

    /// Failed to create a link (symlink or junction).
    #[error("Link error: {0}")]
    Link(String),

    /// Invalid manifest file.
    #[error("Invalid manifest: {0}")]
    InvalidManifest(String),

    /// Invalid version format.
    #[error("Invalid version format: {0}")]
    InvalidVersion(String),
}

/// Result type alias for storage operations.
pub type Result<T> = std::result::Result<T, Error>;
