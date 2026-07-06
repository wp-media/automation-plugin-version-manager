//! Typed queries over the `releases` and `release_assets` tables.

use rusqlite::{Connection, OptionalExtension, params};

use super::{ArtifactRow, ReleaseRow};

/// Column list shared by every `releases` SELECT.
const RELEASE_COLUMNS: &str = "id, project, tag, version, prerelease, draft, published_at_ms, \
                               target_commitish, dir_path, cached_at_ms, last_used_at_ms";

fn row_to_release(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReleaseRow> {
    Ok(ReleaseRow {
        id: row.get(0)?,
        project: row.get(1)?,
        tag: row.get(2)?,
        version: row.get(3)?,
        prerelease: row.get::<_, i64>(4)? != 0,
        draft: row.get::<_, i64>(5)? != 0,
        published_at_ms: row.get(6)?,
        target_commitish: row.get(7)?,
        dir_rel: row.get(8)?,
        cached_at_ms: row.get(9)?,
        last_used_at_ms: row.get(10)?,
    })
}

/// Fetch a single release by row id.
pub(crate) fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<ReleaseRow>> {
    conn.prepare_cached(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases WHERE id = ?1"
    ))?
    .query_row(params![id], row_to_release)
    .optional()
}

/// Fetch a release by its exact tag.
pub(crate) fn by_tag(
    conn: &Connection,
    project: &str,
    tag: &str,
) -> rusqlite::Result<Option<ReleaseRow>> {
    conn.prepare_cached(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases WHERE project = ?1 AND tag = ?2"
    ))?
    .query_row(params![project, tag], row_to_release)
    .optional()
}

/// All cached releases of a project, most recently cached first.
pub(crate) fn list_for_project(
    conn: &Connection,
    project: &str,
) -> rusqlite::Result<Vec<ReleaseRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases WHERE project = ?1 ORDER BY cached_at_ms DESC"
    ))?;
    let rows = stmt.query_map(params![project], row_to_release)?;
    rows.collect()
}

/// Insert a release or refresh its mutable metadata.
///
/// On conflict the GitHub-reported flags are refreshed (an un-drafted
/// release updates its cache entry) while `dir_path` keeps its original
/// value — the asset files already live there. Returns `(id, dir_path)`
/// with the *authoritative* directory.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upsert(
    conn: &Connection,
    project: &str,
    tag: &str,
    version: Option<&str>,
    prerelease: bool,
    draft: bool,
    published_at_ms: Option<i64>,
    target_commitish: Option<&str>,
    dir_rel: &str,
    now_ms: i64,
) -> rusqlite::Result<(i64, String)> {
    conn.prepare_cached(
        "INSERT INTO releases (project, tag, version, prerelease, draft, published_at_ms,
                               target_commitish, dir_path, cached_at_ms, last_used_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
         ON CONFLICT (project, tag) DO UPDATE SET
             version = excluded.version,
             prerelease = excluded.prerelease,
             draft = excluded.draft,
             published_at_ms = excluded.published_at_ms,
             target_commitish = excluded.target_commitish,
             cached_at_ms = excluded.cached_at_ms,
             last_used_at_ms = excluded.last_used_at_ms
         RETURNING id, dir_path",
    )?
    .query_row(
        params![
            project,
            tag,
            version,
            prerelease,
            draft,
            published_at_ms,
            target_commitish,
            dir_rel,
            now_ms
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Insert or refresh an asset record (filename is the identity).
pub(crate) fn upsert_asset(
    conn: &Connection,
    release_id: i64,
    filename: &str,
    size_bytes: i64,
    sha256: &str,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO release_assets (release_id, filename, size_bytes, sha256)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (release_id, filename) DO UPDATE SET
             size_bytes = excluded.size_bytes,
             sha256 = excluded.sha256",
    )?
    .execute(params![release_id, filename, size_bytes, sha256])
    .map(|_| ())
}

/// Asset records of a release, ordered by filename.
pub(crate) fn assets_for(conn: &Connection, release_id: i64) -> rusqlite::Result<Vec<ArtifactRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT NULL, filename, size_bytes, sha256
         FROM release_assets WHERE release_id = ?1 ORDER BY filename",
    )?;
    let rows = stmt.query_map(params![release_id], |row| {
        Ok(ArtifactRow {
            variant: row.get(0)?,
            filename: row.get(1)?,
            size_bytes: row.get(2)?,
            sha256: row.get(3)?,
        })
    })?;
    rows.collect()
}

/// Record that a release was just used.
pub(crate) fn touch(conn: &Connection, id: i64, now_ms: i64) -> rusqlite::Result<()> {
    conn.prepare_cached("UPDATE releases SET last_used_at_ms = ?2 WHERE id = ?1")?
        .execute(params![id, now_ms])
        .map(|_| ())
}

/// Delete a release row; asset rows cascade.
pub(crate) fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.prepare_cached("DELETE FROM releases WHERE id = ?1")?
        .execute(params![id])
        .map(|_| ())
}

/// Every release's `(id, dir_path)` — the working set for gc reconciliation.
pub(crate) fn all_dirs(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached("SELECT id, dir_path FROM releases")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}
