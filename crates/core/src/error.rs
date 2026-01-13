//! Error types for the APVM core library.

use std::io;
use std::path::PathBuf;

/// Main error type for APVM operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// GitHub API error.
    #[error("GitHub API error: {0}")]
    GitHub(#[from] octocrab::Error),

    /// Git operation error.
    #[error("Git error: {0}")]
    Git(String),

    /// Repository not found error.
    #[error("Repository not found: {0}")]
    RepositoryNotFound(PathBuf),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Project error.
    #[error("Project error: {0}")]
    Project(String),

    /// Project not found error.
    #[error("Project not found: {0}")]
    ProjectNotFound(String),

    /// Build error.
    #[error("Build error: {0}")]
    Build(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// JSON serialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result type alias using our Error type.
pub type Result<T> = std::result::Result<T, Error>;
