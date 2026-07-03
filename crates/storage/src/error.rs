//! Storage error types.
//!
//! Every error carries enough context (paths, operation description) to be
//! actionable without a debugger. IO errors are never surfaced bare: they are
//! always wrapped with the operation that failed via `IoResultExt`.

use std::path::{Path, PathBuf};

/// Storage error type.
///
/// Marked `#[non_exhaustive]` so future variants can be added without a
/// semver-breaking change for downstream `match` statements.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// IO error enriched with the operation that failed.
    #[error("{context}: {source}")]
    Io {
        /// Human-readable description of the failed operation, including the path.
        context: String,
        /// The underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// JSON serialization error.
    ///
    /// Deserialization problems surface as [`Error::ManifestCorrupted`]
    /// instead, so this variant only occurs when *writing* manifests.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Invalid caller-provided input (project name, version, commit, filename, tag, …).
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// A source artifact to be stored does not exist on disk.
    #[error("Artifact not found: {0}")]
    ArtifactNotFound(PathBuf),

    /// A manifest file that must exist is missing.
    #[error("Manifest not found: {0}")]
    ManifestNotFound(PathBuf),

    /// A manifest file exists but cannot be parsed or violates invariants.
    #[error("Corrupted manifest at {path}: {reason}")]
    ManifestCorrupted {
        /// Path to the corrupted manifest file.
        path: PathBuf,
        /// Why the manifest was rejected.
        reason: String,
    },

    /// A manifest was written by a newer crate version than this one supports.
    #[error(
        "Unsupported manifest schema version {found} at {path} \
         (this build supports up to {supported}); upgrade APVM to read it"
    )]
    UnsupportedSchema {
        /// Path to the manifest file.
        path: PathBuf,
        /// Schema version found in the file.
        found: u32,
        /// Highest schema version this build supports.
        supported: u32,
    },

    /// Two different full commit hashes map to the same short-hash directory.
    ///
    /// Extremely unlikely (7 hex chars scoped per project+version), but
    /// detected explicitly so it can never cause silent cross-commit
    /// deduplication.
    #[error(
        "Short-commit collision at {dir}: stored build is for commit {existing}, \
         incoming build is for commit {incoming}"
    )]
    CommitCollision {
        /// The contested commit directory.
        dir: PathBuf,
        /// Full hash recorded in the existing manifest.
        existing: String,
        /// Full hash of the build being stored.
        incoming: String,
    },

    /// Failed to create or remove a link (symlink on Unix, junction on Windows).
    #[error("Link error: {0}")]
    Link(String),
}

impl Error {
    /// Build an [`Error::Io`] with a formatted context message.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Build an [`Error::ManifestCorrupted`] for the given manifest path.
    pub(crate) fn corrupted(path: &Path, reason: impl Into<String>) -> Self {
        Self::ManifestCorrupted {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }
}

/// Extension trait attaching operation context to `std::io::Result`.
///
/// Keeps call sites terse while guaranteeing no bare IO error escapes:
///
/// ```ignore
/// std::fs::create_dir_all(&dir).io_ctx(|| format!("creating {}", dir.display()))?;
/// ```
pub(crate) trait IoResultExt<T> {
    /// Convert an `io::Result` into a storage [`Result`], lazily building
    /// the context string only on failure.
    fn io_ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn io_ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| Error::io(context(), e))
    }
}

/// Result type alias for storage operations.
pub type Result<T> = std::result::Result<T, Error>;
