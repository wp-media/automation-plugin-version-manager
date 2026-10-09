//! Aggregate and bulk queries backing usage reports, cleaning, gc and
//! verification.

use rusqlite::{Connection, params};

/// Per-project usage aggregates for one record kind (builds or releases).
#[derive(Debug, Clone)]
pub(crate) struct UsageRow {
    pub project: String,
    pub count: i64,
    /// Recorded bytes; see [`total_bytes`].
    pub bytes: u64,
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
}

/// A record selected for deletion, with its recorded footprint.
#[derive(Debug, Clone)]
pub(crate) struct Victim {
    pub id: i64,
    pub dir_rel: String,
    /// Recorded bytes; see [`total_bytes`].
    pub bytes: u64,
}

/// Which table a stored file's record lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FileKind {
    /// `build_artifacts`.
    Artifact,
    /// `release_assets`.
    Asset,
}

/// A stored file joined with enough context to verify and report it.
#[derive(Debug, Clone)]
pub(crate) struct FileRecord {
    /// Table of the file's record; with `id`, the record's identity.
    pub kind: FileKind,
    /// Row id within that table.
    pub id: i64,
    pub project: String,
    /// `(version, commit)` for builds, `tag` for releases.
    pub build_context: Option<(String, String)>,
    pub tag_context: Option<String>,
    pub dir_rel: String,
    pub filename: String,
    pub size_bytes: i64,
    pub sha256: String,
}

/// A byte sum computed as `TOTAL(MAX(size_bytes, 0))`, as a byte count.
///
/// `SUM` over recorded sizes fails with "integer overflow" once they add up
/// past `i64::MAX` — only a tampered database does, but then every report
/// and even repair's check failed. `TOTAL` sums in floating point, which
/// cannot overflow and is exact below 2^53 bytes (8 PiB); `MAX(…, 0)` makes
/// a corrupt negative size count as nothing. The conversion saturates.
fn total_bytes(total: f64) -> u64 {
    // `as` saturates (and maps NaN to 0): the intent here.
    total as u64
}

/// Map a `(project, count, bytes, oldest, newest)` aggregate row.
fn row_to_usage(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRow> {
    Ok(UsageRow {
        project: row.get(0)?,
        count: row.get(1)?,
        bytes: total_bytes(row.get(2)?),
        oldest_ms: row.get(3)?,
        newest_ms: row.get(4)?,
    })
}

/// Map an `(id, dir_path, bytes)` row selected for deletion.
fn row_to_victim(row: &rusqlite::Row<'_>) -> rusqlite::Result<Victim> {
    Ok(Victim {
        id: row.get(0)?,
        dir_rel: row.get(1)?,
        bytes: total_bytes(row.get(2)?),
    })
}

/// Write a consistent copy of the whole database to the new file `target`
/// (`VACUUM INTO`), readable while other connections keep working. Fails if
/// `target` exists.
pub(crate) fn snapshot_into(conn: &Connection, target: &str) -> rusqlite::Result<()> {
    conn.execute("VACUUM INTO ?1", [target]).map(|_| ())
}

/// Delete every build and release record; artifact, source and asset rows
/// follow through `ON DELETE CASCADE`.
pub(crate) fn clear_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("DELETE FROM builds; DELETE FROM releases;")
}

/// Every project that has at least one build or cached release, sorted.
pub(crate) fn projects(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT project FROM builds UNION SELECT project FROM releases ORDER BY project",
    )?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    rows.collect()
}

/// Build usage grouped by project.
pub(crate) fn usage_builds(conn: &Connection) -> rusqlite::Result<Vec<UsageRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT b.project, COUNT(DISTINCT b.id), TOTAL(MAX(a.size_bytes, 0)),
                MIN(b.built_at_ms), MAX(b.built_at_ms)
         FROM builds AS b
         LEFT JOIN build_artifacts AS a ON a.build_id = b.id
         GROUP BY b.project ORDER BY b.project",
    )?;
    let rows = stmt.query_map([], row_to_usage)?;
    rows.collect()
}

/// Release usage grouped by project.
pub(crate) fn usage_releases(conn: &Connection) -> rusqlite::Result<Vec<UsageRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT r.project, COUNT(DISTINCT r.id), TOTAL(MAX(s.size_bytes, 0)),
                MIN(r.cached_at_ms), MAX(r.cached_at_ms)
         FROM releases AS r
         LEFT JOIN release_assets AS s ON s.release_id = r.id
         GROUP BY r.project ORDER BY r.project",
    )?;
    let rows = stmt.query_map([], row_to_usage)?;
    rows.collect()
}

/// Total number of stored files (build artifacts + release assets).
pub(crate) fn file_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT (SELECT COUNT(*) FROM build_artifacts) + (SELECT COUNT(*) FROM release_assets)",
        [],
        |row| row.get(0),
    )
}

/// Builds matching the clean filters (`NULL` filter = match everything),
/// with their recorded byte footprint.
pub(crate) fn build_victims(
    conn: &Connection,
    project: Option<&str>,
    older_than_ms: Option<i64>,
) -> rusqlite::Result<Vec<Victim>> {
    let mut stmt = conn.prepare_cached(
        "SELECT b.id, b.dir_path, TOTAL(MAX(a.size_bytes, 0))
         FROM builds AS b
         LEFT JOIN build_artifacts AS a ON a.build_id = b.id
         WHERE (?1 IS NULL OR b.project = ?1)
           AND (?2 IS NULL OR b.last_used_at_ms < ?2)
         GROUP BY b.id",
    )?;
    let rows = stmt.query_map(params![project, older_than_ms], row_to_victim)?;
    rows.collect()
}

/// Releases matching the clean filters, with their recorded byte footprint.
pub(crate) fn release_victims(
    conn: &Connection,
    project: Option<&str>,
    older_than_ms: Option<i64>,
) -> rusqlite::Result<Vec<Victim>> {
    let mut stmt = conn.prepare_cached(
        "SELECT r.id, r.dir_path, TOTAL(MAX(s.size_bytes, 0))
         FROM releases AS r
         LEFT JOIN release_assets AS s ON s.release_id = r.id
         WHERE (?1 IS NULL OR r.project = ?1)
           AND (?2 IS NULL OR r.last_used_at_ms < ?2)
         GROUP BY r.id",
    )?;
    let rows = stmt.query_map(params![project, older_than_ms], row_to_victim)?;
    rows.collect()
}

/// Every stored build artifact with verification context.
pub(crate) fn build_files(conn: &Connection) -> rusqlite::Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT a.id, b.project, b.version, b.commit_hash, b.dir_path, a.filename, a.size_bytes,
                a.sha256
         FROM build_artifacts AS a
         JOIN builds AS b ON b.id = a.build_id
         ORDER BY b.project, b.version, b.commit_hash, a.filename",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(FileRecord {
            kind: FileKind::Artifact,
            id: row.get(0)?,
            project: row.get(1)?,
            build_context: Some((row.get(2)?, row.get(3)?)),
            tag_context: None,
            dir_rel: row.get(4)?,
            filename: row.get(5)?,
            size_bytes: row.get(6)?,
            sha256: row.get(7)?,
        })
    })?;
    rows.collect()
}

/// Every stored release asset with verification context.
pub(crate) fn release_files(conn: &Connection) -> rusqlite::Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.id, r.project, r.tag, r.dir_path, s.filename, s.size_bytes, s.sha256
         FROM release_assets AS s
         JOIN releases AS r ON r.id = s.release_id
         ORDER BY r.project, r.tag, s.filename",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(FileRecord {
            kind: FileKind::Asset,
            id: row.get(0)?,
            project: row.get(1)?,
            build_context: None,
            tag_context: Some(row.get(2)?),
            dir_rel: row.get(3)?,
            filename: row.get(4)?,
            size_bytes: row.get(5)?,
            sha256: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Delete one file record (an artifact or a release asset). Its build or
/// release stays, possibly with no files left (see [`drop_empty_builds`]).
pub(crate) fn delete_file(conn: &Connection, kind: FileKind, id: i64) -> rusqlite::Result<()> {
    let sql = match kind {
        FileKind::Artifact => "DELETE FROM build_artifacts WHERE id = ?1",
        FileKind::Asset => "DELETE FROM release_assets WHERE id = ?1",
    };
    conn.prepare_cached(sql)?.execute(params![id]).map(|_| ())
}

/// Whether any build or release record still uses `dir_rel`, compared
/// ASCII-case-insensitively: on a case-insensitive filesystem two records
/// spelled differently can share one directory, which must then stay.
pub(crate) fn dir_in_use(conn: &Connection, dir_rel: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM builds WHERE dir_path = ?1 COLLATE NOCASE)
             OR EXISTS (SELECT 1 FROM releases WHERE dir_path = ?1 COLLATE NOCASE)",
        params![dir_rel],
        |row| row.get(0),
    )
}

/// Every recorded build and release directory — the set the gc sweep keeps.
pub(crate) fn all_dirs(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare_cached("SELECT dir_path FROM builds UNION ALL SELECT dir_path FROM releases")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    rows.collect()
}

/// Delete every build that has no artifact left (it could never be served)
/// and return their directories. Sources cascade.
pub(crate) fn drop_empty_builds(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "DELETE FROM builds
         WHERE NOT EXISTS (SELECT 1 FROM build_artifacts AS a WHERE a.build_id = builds.id)
         RETURNING dir_path",
    )?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    rows.collect()
}

/// Delete every release that has no asset left and return their
/// directories.
pub(crate) fn drop_empty_releases(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "DELETE FROM releases
         WHERE NOT EXISTS (SELECT 1 FROM release_assets AS s WHERE s.release_id = releases.id)
         RETURNING dir_path",
    )?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    rows.collect()
}
