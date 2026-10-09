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
//!   It also bounds the retries of the WAL switch on a new database, which
//!   SQLite fails without waiting when two openers race.
//!
//! `PRAGMA quick_check` runs on every open (the database is small), so a
//! damaged file is detected up front and reported as
//! [`Error::DatabaseCorrupted`] instead of surfacing as confusing query
//! errors later.

pub(crate) mod builds;
pub(crate) mod maintenance;
pub(crate) mod releases;

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};

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

/// Whether [`open`] may create a missing database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// Create the file when it does not exist.
    CreateIfMissing,
    /// Fail instead of creating — for callers that must leave no trace.
    ExistingOnly,
}

/// Identity of a database file (device and inode on Unix), recorded when a
/// connection opens it so a store can later tell whether the file at its
/// path is still the one it uses.
pub(crate) type FileId = (u64, u64);

/// Open the store database (creating it only under
/// [`OpenMode::CreateIfMissing`]): configure pragmas, verify integrity, and
/// run pending migrations. Returns the connection and the identity of the
/// file it is bound to.
///
/// # Errors
///
/// - [`Error::DatabaseCorrupted`] — the file is not a SQLite database or
///   fails `quick_check`.
/// - [`Error::UnsupportedSchema`] — the database was written by a newer
///   version of this crate.
/// - [`Error::Database`] — any other SQLite failure, including a missing
///   file under [`OpenMode::ExistingOnly`], or a file that kept being
///   replaced while it was opened.
pub(crate) fn open(
    db_path: &Path,
    busy_timeout_ms: u64,
    full_durability: bool,
    mode: OpenMode,
) -> Result<(Connection, FileId)> {
    let (mut conn, file) = connect(db_path, mode, busy_timeout_ms, full_durability, &mut || {})?;
    quick_check(&conn, db_path)?;
    migrate(&mut conn, db_path)?;
    Ok((conn, file))
}

/// Reset the database at `db_path` to an empty one (no schema, no rows), in
/// place: the file keeps its identity, so every connection — in this
/// process or another — stays bound to it, and SQLite's own locking
/// coordinates the change. Works on a badly corrupted file, including one
/// that is not a database at all (SQLite's `SQLITE_DBCONFIG_RESET_DATABASE`
/// procedure).
///
/// Never renaming the database is deliberate: a connection that opened the
/// old file but first read it after a rename would take the new file's WAL
/// index for its own and truncate it under other connections of its
/// process (SIGBUS), or pair the new file with a stale WAL.
///
/// # Errors
///
/// [`Error::DatabaseInUse`] when the file is not readable as a database and
/// another connection keeps it open past `busy_timeout_ms`: SQLite then
/// resets it with rollback-journal locking, which needs exclusive access,
/// and WAL connections hold a shared lock for as long as they are open. (A
/// readable WAL database is reset through its WAL, under its readers.)
/// [`Error::Database`] for other SQLite failures.
pub(crate) fn reset_in_place(db_path: &Path, busy_timeout_ms: u64) -> Result<()> {
    let flags = OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE);
    let conn = Connection::open_with_flags(db_path, flags)?;
    let _applied: i64 = conn.query_row(
        &format!("PRAGMA busy_timeout = {busy_timeout_ms}"),
        [],
        |row| row.get(0),
    )?;
    // The documented procedure reads the schema first; on a damaged file
    // that read fails, which is expected.
    let _ = conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    });
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, true)?;
    let vacuum = conn.execute_batch("VACUUM");
    // Best-effort: the connection is dropped right after.
    let _ = conn.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, false);
    match vacuum {
        Ok(()) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if matches!(
                err.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            Err(Error::DatabaseInUse {
                path: db_path.to_path_buf(),
            })
        }
        Err(err) => Err(err.into()),
    }
}

/// How many times [`connect`] opens a file that is replaced meanwhile. Creating
/// a database takes two (the file appears); the rest cover a racing repair.
const CONNECT_ATTEMPTS: usize = 3;

/// Open and configure a connection bound to the file that is at `db_path`.
///
/// The store never replaces its database file (repair resets it in place),
/// but older apvm versions renamed a damaged one aside, and anyone can
/// replace the file. A connection that opens the old file just before such
/// a rename but first reads it just after pairs the old file with the new
/// database's WAL. So the file's identity is taken before opening and again
/// after the first read (the pragmas), and a connection whose file changed
/// in between is discarded and opened again. `before_first_read` runs
/// between the two; tests use it to replace the file. Returns the connection
/// and that confirmed identity.
fn connect(
    db_path: &Path,
    mode: OpenMode,
    busy_timeout_ms: u64,
    full_durability: bool,
    before_first_read: &mut dyn FnMut(),
) -> Result<(Connection, FileId)> {
    let flags = match mode {
        OpenMode::CreateIfMissing => OpenFlags::default(),
        OpenMode::ExistingOnly => OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE),
    };
    for _ in 0..CONNECT_ATTEMPTS {
        let before = file_id(db_path);
        let conn = Connection::open_with_flags(db_path, flags)
            .map_err(|err| map_corruption(err, db_path))?;
        before_first_read();
        let configured = configure(&conn, db_path, busy_timeout_ms, full_durability);
        if let Some(file) = before
            && file_id(db_path) == before
        {
            return configured.map(|()| (conn, file));
        }
    }
    Err(Error::Database(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some(format!(
            "{} kept being replaced while it was being opened",
            db_path.display()
        )),
    )))
}

/// Identity of the file at `path` (device and inode), `None` when there is
/// none.
#[cfg(unix)]
pub(crate) fn file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

/// Elsewhere only existence counts: SQLite opens a database without
/// `FILE_SHARE_DELETE` on Windows, so an open file cannot be renamed and
/// cannot be replaced mid-open.
#[cfg(not(unix))]
pub(crate) fn file_id(path: &Path) -> Option<FileId> {
    path.exists().then_some((0, 0))
}

/// Apply the per-connection pragmas described in the module docs — after
/// refusing someone else's database, before anything writes to the file.
///
/// # Errors
///
/// [`Error::ForeignDatabase`]; [`Error::DatabaseCorrupted`] when the file
/// is not a usable database; [`Error::Database`] otherwise.
fn configure(
    conn: &Connection,
    db_path: &Path,
    busy_timeout_ms: u64,
    full_durability: bool,
) -> Result<()> {
    let sqlite = |err| map_corruption(err, db_path);
    // This pragma returns a result row, so it must be read as a query
    // rather than executed as a statement. It goes first so that the WAL
    // switch below waits for locks rather than failing.
    let _applied: i64 = conn
        .query_row(
            &format!("PRAGMA busy_timeout = {busy_timeout_ms}"),
            [],
            |row| row.get(0),
        )
        .map_err(sqlite)?;
    refuse_foreign_database(conn, db_path)?;
    let mode = enable_wal(conn, Duration::from_millis(busy_timeout_ms)).map_err(sqlite)?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            journal_mode = %mode,
            "SQLite WAL mode unavailable on this filesystem; falling back"
        );
    }

    conn.pragma_update(
        None,
        "synchronous",
        if full_durability { "FULL" } else { "NORMAL" },
    )
    .map_err(sqlite)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(sqlite)?;
    Ok(())
}

/// Pause between attempts of [`enable_wal`]; a rival switch takes well
/// under a millisecond.
const WAL_RETRY_DELAY: Duration = Duration::from_millis(2);

/// Switch the connection to WAL and return the journal mode now in effect.
///
/// Switching a database that is not yet in WAL mode upgrades a read lock to
/// a write lock. When another connection is switching at the same moment,
/// SQLite fails that upgrade with `SQLITE_BUSY` at once, bypassing the busy
/// handler, because waiting could deadlock. The failed statement has
/// already released its lock, so this retries until `timeout` elapses. The
/// retry finds the file in WAL mode and needs no write lock.
fn enable_wal(conn: &Connection, timeout: Duration) -> rusqlite::Result<String> {
    // No deadline when the timeout is too large to represent.
    let deadline = Instant::now().checked_add(timeout);
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)) {
            Err(err)
                if err.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && deadline.is_none_or(|end| Instant::now() < end) =>
            {
                std::thread::sleep(WAL_RETRY_DELAY);
            }
            outcome => return outcome,
        }
    }
}

/// Run `PRAGMA quick_check` and fail with [`Error::DatabaseCorrupted`] if
/// SQLite reports any problem.
pub(crate) fn quick_check(conn: &Connection, db_path: &Path) -> Result<()> {
    check(conn, db_path, "PRAGMA quick_check")
}

/// Run `PRAGMA integrity_check` — [`quick_check`] plus a comparison of
/// every index with its table, which a damaged index fails while the quick
/// check passes — and fail with [`Error::DatabaseCorrupted`] if SQLite
/// reports any problem. Slower: proportional to the database's size.
pub(crate) fn integrity_check(conn: &Connection, db_path: &Path) -> Result<()> {
    check(conn, db_path, "PRAGMA integrity_check")
}

/// Run a check `pragma` that returns `ok`, or one row per problem.
fn check(conn: &Connection, db_path: &Path, pragma: &str) -> Result<()> {
    let mut stmt = conn
        .prepare(pragma)
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
///
/// Safe when several connections open a new database at once. An
/// up-to-date database costs one read and no lock. Otherwise the version is
/// re-read inside an `IMMEDIATE` transaction, which takes the write lock up
/// front: exactly one opener applies the migrations and the rest find them
/// done. A deferred transaction here would let two openers both run the DDL
/// ("table already exists") or fail upgrading their read lock.
fn migrate(conn: &mut Connection, db_path: &Path) -> Result<()> {
    if supported_version(conn, db_path)? == SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = supported_version(&tx, db_path)?;
    for version in current..SCHEMA_VERSION {
        let ddl = usize::try_from(version)
            .ok()
            .and_then(|idx| MIGRATIONS.get(idx))
            .ok_or_else(|| Error::Data {
                details: format!("no migration registered for schema version {version}"),
            })?;
        tx.execute_batch(ddl)?;
    }
    if current < SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

/// The tables every apvm schema version up to [`SCHEMA_VERSION`] has.
const APVM_TABLES: [&str; 5] = [
    "builds",
    "build_artifacts",
    "build_sources",
    "releases",
    "release_assets",
];

/// Refuse someone else's database. apvm's migrations set the schema version
/// in the transaction that creates the tables, so a version-0 database
/// holding any schema object, or a database at a version apvm knows that
/// lacks one of apvm's tables, was never apvm's. (A newer version is left
/// to [`supported_version`].) Runs before anything writes to the file (the
/// WAL switch does), as one statement so the version and the tables come
/// from the same snapshot even while a concurrent opener migrates a new
/// database.
///
/// # Errors
///
/// [`Error::ForeignDatabase`] for such a database; [`Error::Database`] when
/// it cannot be read.
fn refuse_foreign_database(conn: &Connection, db_path: &Path) -> Result<()> {
    let tables = APVM_TABLES.map(|table| format!("'{table}'")).join(", ");
    let (version, objects, apvm_tables): (i64, i64, i64) = conn
        .query_row(
            &format!(
                "SELECT (SELECT user_version FROM pragma_user_version), \
                        (SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'), \
                        (SELECT count(*) FROM sqlite_master \
                         WHERE type = 'table' AND name IN ({tables}))"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|err| map_corruption(err, db_path))?;
    let known_version = (1..=SCHEMA_VERSION).contains(&version);
    let all_tables = usize::try_from(apvm_tables).is_ok_and(|count| count == APVM_TABLES.len());
    if (version == 0 && objects > 0) || (known_version && !all_tables) {
        return Err(Error::ForeignDatabase {
            path: db_path.to_path_buf(),
        });
    }
    Ok(())
}

/// The database's `user_version`; [`Error::UnsupportedSchema`] when it is
/// newer than this build understands, [`Error::DatabaseCorrupted`] when it
/// is negative — no apvm writes that, so repair should replace the file.
fn supported_version(conn: &Connection, db_path: &Path) -> Result<i64> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current < 0 {
        return Err(Error::DatabaseCorrupted {
            path: db_path.to_path_buf(),
            details: format!("invalid schema version {current}"),
        });
    }
    if current > SCHEMA_VERSION {
        return Err(Error::UnsupportedSchema {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(current)
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

        let (conn, _) = open(&path, 100, false, OpenMode::CreateIfMissing).unwrap();
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
        let (conn, _) = open(&path, 100, false, OpenMode::CreateIfMissing).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn existing_only_never_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        assert!(matches!(
            open(&path, 100, false, OpenMode::ExistingOnly),
            Err(Error::Database(_))
        ));
        assert!(!path.exists(), "ExistingOnly must not create the database");
    }

    #[test]
    fn concurrent_first_opens_all_succeed() {
        // Regression: migrations used to run in a deferred transaction after
        // an unlocked version read, so concurrent first opens of a new
        // database failed ("table builds already exists" / "locked").
        const THREADS: usize = 8;
        for round in 0..25 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("apvm.db");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let (path, barrier) = (path.clone(), std::sync::Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        barrier.wait();
                        open(&path, 5_000, false, OpenMode::CreateIfMissing).map(|_| ())
                    })
                })
                .collect();
            for handle in handles {
                let outcome = handle.join().expect("opener thread panicked");
                assert!(outcome.is_ok(), "round {round}: {outcome:?}");
            }
            let (conn, _) = open(&path, 5_000, false, OpenMode::ExistingOnly).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
        }
    }

    /// A new, not-yet-WAL database whose write lock a rival connection
    /// holds, as while another opener switches it to WAL.
    fn locked_new_database(dir: &Path) -> (std::path::PathBuf, Connection) {
        let path = dir.join("apvm.db");
        let rival = Connection::open(&path).unwrap();
        rival.execute_batch("BEGIN IMMEDIATE").unwrap();
        (path, rival)
    }

    #[test]
    fn wal_switch_waits_for_a_rival_writer_instead_of_failing() {
        // Regression: SQLite fails this lock upgrade at once, without the
        // busy handler, so racing first opens used to fail "database is
        // locked" (16 of 120,000 in a probe).
        let dir = tempfile::tempdir().unwrap();
        let (path, rival) = locked_new_database(dir.path());
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            rival.execute_batch("COMMIT").unwrap();
        });

        let (conn, _) = open(&path, 5_000, false, OpenMode::CreateIfMissing).unwrap();
        release.join().expect("releasing thread panicked");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn wal_switch_gives_up_when_the_busy_timeout_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _rival) = locked_new_database(dir.path());
        let start = Instant::now();
        let err = open(&path, 50, false, OpenMode::CreateIfMissing).unwrap_err();
        assert!(start.elapsed() >= Duration::from_millis(50));
        let Error::Database(err) = err else {
            panic!("expected a SQLite error, got {err:?}");
        };
        assert_eq!(err.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
    }

    /// Create, at `path`, an apvm database told apart by one extra table,
    /// `marker`.
    #[cfg(unix)]
    fn create_marked_database(path: &Path, marker: &str) {
        Connection::open(path)
            .unwrap()
            .execute_batch(&format!(
                "PRAGMA journal_mode = WAL; {V1_SCHEMA} CREATE TABLE {marker} (x); \
                 PRAGMA user_version = {SCHEMA_VERSION};"
            ))
            .unwrap();
    }

    /// Move `path` and its sidecars aside the way older apvm versions'
    /// repair did, then create a different database, marked `marker`
    /// ([`create_marked_database`]), there.
    #[cfg(unix)]
    fn replace_database(path: &Path, marker: &str) {
        for suffix in ["", "-wal", "-shm"] {
            let from = format!("{}{suffix}", path.display());
            if Path::new(&from).exists() {
                std::fs::rename(&from, format!("{}.{marker}-old{suffix}", path.display())).unwrap();
            }
        }
        create_marked_database(path, marker);
    }

    #[cfg(unix)]
    #[test]
    fn a_database_replaced_mid_open_is_opened_again() {
        // Regression: the connection stayed on the renamed file and read it
        // through the new database's WAL (a "ghost" after repair).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        create_marked_database(&path, "old");
        let mut calls = 0;
        let (conn, file) = connect(&path, OpenMode::ExistingOnly, 1_000, false, &mut || {
            calls += 1;
            if calls == 1 {
                replace_database(&path, "new");
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(Some(file), file_id(&path), "the identity is the new file's");
        let markers: String = conn
            .query_row(
                "SELECT group_concat(name) FROM sqlite_master WHERE name IN ('old', 'new')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(markers, "new");
    }

    #[cfg(unix)]
    #[test]
    fn a_database_that_keeps_being_replaced_is_not_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        replace_database(&path, "first");
        let mut calls = 0;
        let err = connect(&path, OpenMode::ExistingOnly, 1_000, false, &mut || {
            calls += 1;
            replace_database(&path, &format!("again{calls}"));
        })
        .unwrap_err();
        assert_eq!(calls, CONNECT_ATTEMPTS);
        let Error::Database(err) = err else {
            panic!("expected a SQLite error, got {err:?}");
        };
        assert_eq!(err.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
        assert!(err.to_string().contains("kept being replaced"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_database_moved_away_mid_open_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        let mut calls = 0;
        let (conn, _) = connect(&path, OpenMode::CreateIfMissing, 1_000, false, &mut || {
            calls += 1;
            if calls == 1 {
                std::fs::rename(&path, dir.path().join("moved.db")).unwrap();
            }
        })
        .unwrap();
        assert_eq!(calls, 3); // moved away, then created again, then confirmed
        conn.execute_batch("CREATE TABLE kept (x);").unwrap();
        let at_path = Connection::open(&path).unwrap();
        let found: i64 = at_path
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'kept'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, 1);
    }

    #[test]
    fn creating_a_database_rechecks_it_once_whatever_the_timeout() {
        // The file appears during the first attempt, so a second one binds
        // to it; that must not depend on the busy timeout.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        let mut calls = 0;
        connect(&path, OpenMode::CreateIfMissing, 0, false, &mut || {
            calls += 1
        })
        .unwrap();
        assert_eq!(calls, 2);
    }

    #[test]
    fn open_rejects_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 999).unwrap();
        }
        let err = open(&path, 100, false, OpenMode::CreateIfMissing).unwrap_err();
        assert!(matches!(err, Error::UnsupportedSchema { found: 999, .. }));
    }

    #[test]
    fn a_negative_schema_version_is_corruption_not_a_dead_end() {
        // Audit: it failed open and repair alike with `Data`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", -1)
            .unwrap();
        let err = open(&path, 100, false, OpenMode::CreateIfMissing).unwrap_err();
        assert!(
            matches!(&err, Error::DatabaseCorrupted { details, .. } if details == "invalid schema version -1"),
            "{err:?}"
        );
    }

    #[test]
    fn open_reports_garbage_file_as_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apvm.db");
        std::fs::write(&path, b"this is definitely not a sqlite database").unwrap();
        let start = Instant::now();
        let err = open(&path, 60_000, false, OpenMode::CreateIfMissing).unwrap_err();
        assert!(
            matches!(err, Error::DatabaseCorrupted { .. }),
            "got: {err:?}"
        );
        // Only lock contention is retried; corruption is reported at once.
        assert!(start.elapsed() < Duration::from_secs(30));
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
