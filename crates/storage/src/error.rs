//! Error types for the storage crate.
//!
//! Every fallible operation returns [`Result<T>`]. The [`Error`] enum is
//! `#[non_exhaustive]`: new variants may be added in minor releases, so
//! consumers must keep a wildcard arm when matching.

use std::path::PathBuf;

/// Convenient result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors the storage crate can produce.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An input failed validation before touching the database or filesystem.
    ///
    /// `what` names the offending field (e.g. `"project"`, `"commit"`),
    /// `value` is the rejected input, and `reason` explains the rule.
    #[error("invalid {what} '{value}': {reason}")]
    InvalidInput {
        /// Which input was rejected (field name).
        what: &'static str,
        /// The rejected value, verbatim.
        value: String,
        /// Why the value was rejected.
        reason: String,
    },

    /// A source file handed to a store operation does not exist or is not a
    /// regular file.
    #[error("artifact source file not found: {}", path.display())]
    SourceFileMissing {
        /// The path that was expected to be a readable file.
        path: PathBuf,
    },

    /// A filesystem operation failed. `context` describes the operation and
    /// the path involved; the underlying [`std::io::Error`] is preserved as
    /// the source.
    #[error("{context}")]
    Io {
        /// Human-readable description of what failed and where.
        context: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The underlying SQLite operation failed.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// The database file is not a valid SQLite database or failed the
    /// integrity check. Use [`crate::ArtifactStore::repair`] to quarantine
    /// the corrupt file and rebuild a fresh index.
    #[error(
        "storage database corrupted at {}: {details} \
         (run ArtifactStore::repair to quarantine and rebuild it)",
        path.display()
    )]
    DatabaseCorrupted {
        /// Path of the corrupt database file.
        path: PathBuf,
        /// Details reported by SQLite.
        details: String,
    },

    /// The database was created by a newer version of this crate.
    ///
    /// Refusing to open (instead of misreading a newer schema) protects the
    /// data; upgrading apvm resolves this.
    #[error(
        "unsupported storage schema version {found} \
         (this build supports up to {supported}); upgrade apvm"
    )]
    UnsupportedSchema {
        /// Schema version recorded in the database.
        found: i64,
        /// Newest schema version this build understands.
        supported: i64,
    },

    /// Stored metadata is internally inconsistent (e.g. a negative file size
    /// or an out-of-range timestamp). Indicates external tampering with the
    /// database; [`crate::ArtifactStore::repair`] rebuilds a clean index.
    #[error("corrupt stored metadata: {details}")]
    Data {
        /// Description of the inconsistency.
        details: String,
    },
}

impl Error {
    /// Shorthand constructor for [`Error::InvalidInput`].
    pub(crate) fn invalid(
        what: &'static str,
        value: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self::InvalidInput {
            what,
            value: value.into(),
            reason: reason.into(),
        }
    }
}

/// Extension trait attaching human-readable context to raw I/O results.
///
/// The closure is only evaluated on the error path, so successful calls pay
/// nothing for the context string.
pub(crate) trait IoContext<T> {
    /// Convert an [`std::io::Result`] into a crate [`Result`], describing
    /// the failed operation via `context`.
    fn io_ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn io_ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: context(),
            source,
        })
    }
}
