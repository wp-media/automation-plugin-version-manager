//! SQLite access layer: connection setup, schema migrations, and typed
//! queries.
//!
//! This is the **only** part of the crate that contains SQL. Everything
//! above it (store, lookup, maintenance) works with the typed row structs
//! defined here and never sees a query string.
//!
//! # Robustness settings
//!
//! Connections are opened with:
//!
//! - `journal_mode = WAL` — readers never block the writer and a crash can
//!   never corrupt the database (unfinished transactions roll back).
//! - `synchronous = NORMAL` (or `FULL` on request) — in WAL mode, `NORMAL`
//!   guarantees integrity across power loss; at most the very last
//!   transaction may be rolled back, which for a cache means one re-copy.
//! - `foreign_keys = ON` — deleting a build/release cascades to its
//!   artifact and source rows.
//! - `busy_timeout` — concurrent processes wait instead of failing fast.
//!
//! `PRAGMA quick_check` runs on every open (the database is small), so a
//! damaged file is detected up front and reported as
//! [`Error::DatabaseCorrupted`] instead of surfacing as confusing query
//! errors later.

pub(crate) mod builds;
pub(crate) mod maintenance;
pub(crate) mod releases;

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::error::{Error, Result};

/// Newest schema version this build understands (`PRAGMA user_version`).
pub(crate) const SCHEMA_VERSION: i64 = 1;

/// One DDL batch per schema version, applied in order, each inside its own
/// transaction. Index N migrates a database from version N to N+1.
const MIGRATIONS: &[&str] = &[V1_SCHEMA];

/// Initial schema. All tables are `STRICT` (SQLite 3.37+): column types are
/// enforced, so a corrupted writer cannot slip a string into a size column.
const V1_SCHEMA: &str = "
CREATE TABLE builds (
    id              INTEGER PRIMARY KEY,
    project         TEXT    NOT NULL,
    version         TEXT    NOT NULL,
    commit_hash     TEXT    NOT NULL,
    dir_path        TEXT    NOT NULL UNIQUE,
    built_at_ms     INTEGER NOT NULL,
    last_used_at_ms INTEGER NOT NULL,
    UNIQUE (project, version, commit_hash)
) STRICT;

CREATE INDEX idx_builds_project ON builds (project);
CREATE INDEX idx_builds_last_used ON builds (last_used_at_ms);

CREATE TABLE build_artifacts (
    id         INTEGER PRIMARY KEY,
    build_id   INTEGER NOT NULL REFERENCES builds (id) ON DELETE CASCADE,
    variant    TEXT,
    filename   TEXT    NOT NULL,
    size_bytes INTEGER NOT NULL,
    sha256     TEXT    NOT NULL,
    UNIQUE (build_id, filename)
) STRICT;

CREATE INDEX idx_build_artifacts_build ON build_artifacts (build_id);

CREATE TABLE build_sources (
    id           INTEGER PRIMARY KEY,
    build_id     INTEGER NOT NULL REFERENCES builds (id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL,
    reference    TEXT    NOT NULL,
    branch       TEXT,
    linked_at_ms INTEGER NOT NULL,
    UNIQUE (build_id, kind, reference)
) STRICT;

CREATE INDEX idx_build_sources_ref ON build_sources (kind, reference);

CREATE TABLE releases (
    id               INTEGER PRIMARY KEY,
    project          TEXT    NOT NULL,
    tag              TEXT    NOT NULL,
    version          TEXT,
    prerelease       INTEGER NOT NULL DEFAULT 0 CHECK (prerelease IN (0, 1)),
    draft            INTEGER NOT NULL DEFAULT 0 CHECK (draft IN (0, 1)),
    published_at_ms  INTEGER,
    target_commitish TEXT,
    dir_path         TEXT    NOT NULL UNIQUE,
    cached_at_ms     INTEGER NOT NULL,
    last_used_at_ms  INTEGER NOT NULL,
    UNIQUE (project, tag)
) STRICT;

CREATE INDEX idx_releases_project ON releases (project);
CREATE INDEX idx_releases_last_used ON releases (last_used_at_ms);

CREATE TABLE release_assets (
    id         INTEGER PRIMARY KEY,
    release_id INTEGER NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    filename   TEXT    NOT NULL,
    size_bytes INTEGER NOT NULL,
    sha256     TEXT    NOT NULL,
    UNIQUE (release_id, filename)
) STRICT;

CREATE INDEX idx_release_assets_release ON release_assets (release_id);
";

// ============================================================================
// Row structs (internal currency between the db layer and the store)
// ============================================================================

/// One row of the `builds` table.
#[derive(Debug, Clone)]
pub(crate) struct BuildRow {
    pub id: i64,
    pub project: String,
    pub version: String,
    pub commit: String,
    pub dir_rel: String,
    pub built_at_ms: i64,
    pub last_used_at_ms: i64,
}

/// One row of the `build_artifacts` / `release_assets` tables.
#[derive(Debug, Clone)]
pub(crate) struct ArtifactRow {
    pub variant: Option<String>,
    pub filename: String,
    pub size_bytes: i64,
    pub sha256: String,
}

/// One row of the `build_sources` table.
#[derive(Debug, Clone)]
pub(crate) struct SourceRow {
    pub kind: String,
    pub reference: String,
    pub branch: Option<String>,
    pub linked_at_ms: i64,
}

/// One row of the `releases` table.
#[derive(Debug, Clone)]
pub(crate) struct ReleaseRow {
    pub id: i64,
    pub project: String,
    pub tag: String,
    pub version: Option<String>,
    pub prerelease: bool,
    pub draft: bool,
    pub published_at_ms: Option<i64>,
    pub target_commitish: Option<String>,
    pub dir_rel: String,
    pub cached_at_ms: i64,
    pub last_used_at_ms: i64,
}

// ============================================================================
// Connection lifecycle
// ============================================================================

/// Open (creating if needed) the store database: configure pragmas, verify
/// integrity, and run pending migrations.
///
/// # Errors
///
/// - [`Error::DatabaseCorrupted`] — the file is not a SQLite database or
///   fails `quick_check`.
/// - [`Error::UnsupportedSchema`] — the database was written by a newer
///   version of this crate.
/// - [`Error::Database`] — any other SQLite failure.
pub(crate) fn open(
    db_path: &Path,
    busy_timeout_ms: u64,
    full_durability: bool,
) -> Result<Connection> {
    let mut conn = Connection::open(db_path).map_err(|err| map_corruption(err, db_path))?;
    configure(&conn, busy_timeout_ms, full_durability)
        .map_err(|err| map_corruption(err, db_path))?;
    quick_check(&conn, db_path)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Apply the per-connection pragmas described in the module docs.
fn configure(
    conn: &Connection,
    busy_timeout_ms: u64,
    full_durability: bool,
) -> rusqlite::Result<()> {
    // These two pragmas return a result row, so they must be read as
    // queries rather than executed as statements.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            journal_mode = %mode,
            "SQLite WAL mode unavailable on this filesystem; falling back"
        );
    }
    let _applied: i64 = conn.query_row(
        &format!("PRAGMA busy_timeout = {busy_timeout_ms}"),
        [],
        |row| row.get(0),
    )?;

    conn.pragma_update(
        None,
        "synchronous",
        if full_durability { "FULL" } else { "NORMAL" },
    )?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Run `PRAGMA quick_check` and fail with [`Error::DatabaseCorrupted`] if
/// SQLite reports any problem.
pub(crate) fn quick_check(conn: &Connection, db_path: &Path) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA quick_check")
        .map_err(|err| map_corruption(err, db_path))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| map_corruption(err, db_path))?;

    let mut problems = Vec::new();
    for row in rows {
        let line = row.map_err(|err| map_corruption(err, db_path))?;
        if !line.eq_ignore_ascii_case("ok") {
            problems.push(line);
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::DatabaseCorrupted {
            path: db_path.to_path_buf(),
            details: problems.join("; "),
        })
    }
}

/// Bring `PRAGMA user_version` up to [`SCHEMA_VERSION`], refusing databases
/// from the future.
fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(Error::UnsupportedSchema {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for version in current..SCHEMA_VERSION {
        let ddl = usize::try_from(version)
            .ok()
            .and_then(|idx| MIGRATIONS.get(idx))
            .ok_or_else(|| Error::Data {
                details: format!("no migration registered for schema version {version}"),
            })?;
        let tx = conn.transaction()?;
        tx.execute_batch(ddl)?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// Translate "this is not a usable database file" SQLite errors into
/// [`Error::DatabaseCorrupted`]; pass everything else through.
fn map_corruption(err: rusqlite::Error, db_path: &Path) -> Error {
    if let rusqlite::Error::SqliteFailure(ffi_err, ref message) = err {
        use rusqlite::ErrorCode::{DatabaseCorrupt, NotADatabase};
        if matches!(ffi_err.code, DatabaseCorrupt | NotADatabase) {
            return Error::DatabaseCorrupted {
                path: db_path.to_path_buf(),
                details: message.clone().unwrap_or_else(|| ffi_err.to_string()),
            };
        }
    }
    Error::Database(err)
}

// ============================================================================
// Value conversions
// ============================================================================

/// UTC timestamp → epoch milliseconds (the on-disk representation).
pub(crate) fn to_ms(at: DateTime<Utc>) -> i64 {
    at.timestamp_millis()
}

/// Epoch milliseconds → UTC timestamp; out-of-range values are reported as
/// corrupt metadata rather than silently clamped.
pub(crate) fn from_ms(ms: i64) -> Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(ms).ok_or_else(|| Error::Data {
        details: format!("stored timestamp {ms}ms is out of range"),
    })
}

/// File size → the INTEGER column representation.
pub(crate) fn size_to_db(size: u64) -> Result<i64> {
    i64::try_from(size).map_err(|_| Error::Data {
        details: format!("file size {size} exceeds the storable maximum"),
    })
}

/// INTEGER column → file size; negative values are corrupt metadata.
pub(crate) fn size_from_db(size: i64) -> Result<u64> {
    u64::try_from(size).map_err(|_| Error::Data {
        details: format!("stored file size {size} is negative"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_configures_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");

        let conn = open(&path, 100, false).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        drop(conn);

        // Reopening an initialized database is a no-op.
        let conn = open(&path, 100, false).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn open_rejects_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 999).unwrap();
        }
        let err = open(&path, 100, false).unwrap_err();
        assert!(matches!(err, Error::UnsupportedSchema { found: 999, .. }));
    }

    #[test]
    fn open_reports_garbage_file_as_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        std::fs::write(&path, b"this is definitely not a sqlite database").unwrap();
        let err = open(&path, 100, false).unwrap_err();
        assert!(
            matches!(err, Error::DatabaseCorrupted { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn conversions_round_trip_and_reject_bad_values() {
        let now = Utc::now();
        let restored = from_ms(to_ms(now)).unwrap();
        assert_eq!(restored.timestamp_millis(), now.timestamp_millis());

        assert_eq!(size_from_db(size_to_db(42).unwrap()).unwrap(), 42);
        assert!(size_from_db(-1).is_err());
    }
}
