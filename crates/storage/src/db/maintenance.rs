//! Aggregate and bulk queries backing usage reports, cleaning, gc and
//! verification.

use rusqlite::{Connection, params};

/// Per-project usage aggregates for one record kind (builds or releases).
#[derive(Debug, Clone)]
pub(crate) struct UsageRow {
    pub project: String,
    pub count: i64,
    pub bytes: i64,
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
}

/// A record selected for deletion, with its recorded footprint.
#[derive(Debug, Clone)]
pub(crate) struct Victim {
    pub id: i64,
    pub dir_rel: String,
    pub bytes: i64,
}

/// A stored file joined with enough context to verify and report it.
#[derive(Debug, Clone)]
pub(crate) struct FileRecord {
    pub project: String,
    /// `(version, commit)` for builds, `tag` for releases.
    pub build_context: Option<(String, String)>,
    pub tag_context: Option<String>,
    pub dir_rel: String,
    pub filename: String,
    pub size_bytes: i64,
    pub sha256: String,
}

fn row_to_usage(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRow> {
    Ok(UsageRow {
        project: row.get(0)?,
        count: row.get(1)?,
        bytes: row.get(2)?,
        oldest_ms: row.get(3)?,
        newest_ms: row.get(4)?,
    })
}

fn row_to_victim(row: &rusqlite::Row<'_>) -> rusqlite::Result<Victim> {
    Ok(Victim {
        id: row.get(0)?,
        dir_rel: row.get(1)?,
        bytes: row.get(2)?,
    })
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
        "SELECT b.project, COUNT(DISTINCT b.id), COALESCE(SUM(a.size_bytes), 0),
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
        "SELECT r.project, COUNT(DISTINCT r.id), COALESCE(SUM(s.size_bytes), 0),
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
        "SELECT b.id, b.dir_path, COALESCE(SUM(a.size_bytes), 0)
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
        "SELECT r.id, r.dir_path, COALESCE(SUM(s.size_bytes), 0)
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
        "SELECT b.project, b.version, b.commit_hash, b.dir_path, a.filename, a.size_bytes, a.sha256
         FROM build_artifacts AS a
         JOIN builds AS b ON b.id = a.build_id
         ORDER BY b.project, b.version, b.commit_hash, a.filename",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(FileRecord {
            project: row.get(0)?,
            build_context: Some((row.get(1)?, row.get(2)?)),
            tag_context: None,
            dir_rel: row.get(3)?,
            filename: row.get(4)?,
            size_bytes: row.get(5)?,
            sha256: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Every stored release asset with verification context.
pub(crate) fn release_files(conn: &Connection) -> rusqlite::Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT r.project, r.tag, r.dir_path, s.filename, s.size_bytes, s.sha256
         FROM release_assets AS s
         JOIN releases AS r ON r.id = s.release_id
         ORDER BY r.project, r.tag, s.filename",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(FileRecord {
            project: row.get(0)?,
            build_context: None,
            tag_context: Some(row.get(1)?),
            dir_rel: row.get(2)?,
            filename: row.get(3)?,
            size_bytes: row.get(4)?,
            sha256: row.get(5)?,
        })
    })?;
    rows.collect()
}
