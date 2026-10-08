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
    /// the corrupt file and rebuild a fresh index. The message names no
    /// remedy: each front end adds its own (`apvm cache repair`, …).
    #[error("storage database corrupted at {}: {details}", path.display())]
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
    /// database; [`crate::ArtifactStore::repair`] quarantines it and rebuilds
    /// a clean index.
    #[error("corrupt stored metadata: {details}")]
    Data {
        /// Description of the inconsistency.
        details: String,
    },

    /// The directory has no store database and no sign of ever having held
    /// a store, yet contains `entry`, a subdirectory named like a store
    /// project. The store refuses to initialize there: its garbage
    /// collection could later delete that data.
    #[error(
        "refusing to use {} as an artifact store: it has no apvm database but already \
         contains '{entry}' (use a new or empty directory)",
        path.display()
    )]
    ForeignDirectory {
        /// The directory that was refused.
        path: PathBuf,
        /// The first project-named subdirectory found in it.
        entry: String,
    },

    /// The store database is missing — or blank (0 bytes) — while store
    /// content remains on disk (a store whose database was deleted or
    /// truncated, or a repair that was interrupted). Opening would silently
    /// orphan that content, so it is refused; [`crate::ArtifactStore::repair`]
    /// rebuilds the index. The message names no remedy: each front end adds
    /// its own.
    #[error(
        "the store database at {} is missing or empty but store content remains \
         ({adoptable_builds} re-indexable build(s))",
        path.display()
    )]
    MissingDatabase {
        /// The store directory.
        path: PathBuf,
        /// Build directories repair would re-index.
        adoptable_builds: u64,
    },

    /// The database is held open by another process or store handle, so it
    /// cannot be reset in place. Repair refuses rather than swap the file
    /// out from under them (which can crash them or mix their files): close
    /// the other users and run repair again.
    #[error(
        "the store database at {} is in use by another process or store handle; \
         close them and try again",
        path.display()
    )]
    DatabaseInUse {
        /// Path of the database file.
        path: PathBuf,
    },

    /// The file at the database path is an SQLite database the store did not
    /// create (no schema version, yet tables). It is never migrated or
    /// written to.
    #[error(
        "{} is an SQLite database that apvm did not create; refusing to use it",
        path.display()
    )]
    ForeignDatabase {
        /// Path of the database file.
        path: PathBuf,
    },

    /// This handle's database file is no longer the store's: it was renamed
    /// aside (a repair quarantining it), deleted or replaced after the handle
    /// opened it. Mutations refuse, since they would update a database the
    /// store no longer uses; open the store again.
    #[error(
        "the store database at {} was replaced, moved or deleted after this handle opened it; \
         open the store again",
        path.display()
    )]
    StaleHandle {
        /// Path of the database file.
        path: PathBuf,
    },
}

impl Error {
    /// The message, plus the I/O cause that [`Error::Io`]'s message leaves
    /// out (e.g. "failed to open x: Permission denied (os error 13)"), for
    /// reports and warnings that keep only text.
    pub fn detail(&self) -> String {
        match self {
            Self::Io { context, source } => format!("{context}: {source}"),
            other => other.to_string(),
        }
    }

    /// Whether the store database is damaged in a way
    /// [`crate::ArtifactStore::repair`] fixes: [`Error::DatabaseCorrupted`],
    /// unreadable rows ([`Error::Data`]), or an SQLite "corrupt" / "not a
    /// database" failure met after opening ([`Error::Database`]).
    /// [`Error::MissingDatabase`] is not included: it is a lost database,
    /// which repair also rebuilds.
    pub fn is_corruption(&self) -> bool {
        use rusqlite::ErrorCode;
        match self {
            Self::DatabaseCorrupted { .. } | Self::Data { .. } => true,
            Self::Database(rusqlite::Error::SqliteFailure(err, _)) => matches!(
                err.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
            ),
            _ => false,
        }
    }

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
