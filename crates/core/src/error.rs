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

    /// Private repository requires authentication.
    #[error(
        "Repository '{repo}' is private and requires a GitHub token.\n\
         \n\
         To authenticate, do ONE of the following:\n\
         \n\
         1. Set GITHUB_TOKEN environment variable:\n\
            export GITHUB_TOKEN=ghp_xxxxxxxxxxxx\n\
         \n\
         2. Set GH_TOKEN environment variable:\n\
            export GH_TOKEN=ghp_xxxxxxxxxxxx\n\
         \n\
         3. Login with GitHub CLI (recommended):\n\
            gh auth login\n\
         \n\
         4. Add token to config file (~/.apvm/config.json):\n\
            {{\"github_token\": \"ghp_xxxxxxxxxxxx\", ...}}\n\
         \n\
         Token needs 'repo' scope for private repositories."
    )]
    PrivateRepoNoToken {
        /// The repository that requires authentication.
        repo: String,
    },

    /// Build error.
    #[error("Build error: {0}")]
    Build(String),

    /// No release found for a given tag.
    #[error("No GitHub release found for tag '{tag}' in {repo}")]
    ReleaseNotFound {
        /// The tag that was looked up.
        tag: String,
        /// The repository (owner/repo).
        repo: String,
    },

    /// Releases are not available for this project.
    #[error(
        "Project '{project}' does not have GitHub Releases with downloadable assets.\n\
         \n\
         Use a git reference instead:\n\
         \n\
         - tag:{tag}       → Build from tag\n\
         - branch:develop  → Build from branch\n\
         - pr:123          → Build from pull request"
    )]
    ReleasesNotAvailable {
        /// The project name.
        project: String,
        /// The tag that was attempted.
        tag: String,
    },

    /// No matching release assets found for the requested variants.
    #[error(
        "No matching release assets found for tag '{tag}' in {repo}.\nAvailable assets: {available}"
    )]
    NoMatchingReleaseAssets {
        /// The tag that was looked up.
        tag: String,
        /// The repository (owner/repo).
        repo: String,
        /// Comma-separated list of available asset names.
        available: String,
    },

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// JSON serialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Self-update error (only CLI).
    #[error("Update error: {0}")]
    Update(String),
}

/// Result type alias using our Error type.
pub type Result<T> = std::result::Result<T, Error>;
