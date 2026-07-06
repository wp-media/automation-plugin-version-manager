//! Typed queries over the `builds`, `build_artifacts` and `build_sources`
//! tables.
//!
//! All functions accept `&Connection`; thanks to deref coercion they work
//! with a [`rusqlite::Transaction`] too, so the store layer decides the
//! transaction boundaries.

use rusqlite::{Connection, OptionalExtension, params};

use super::{ArtifactRow, BuildRow, SourceRow};

/// Column list shared by every `builds` SELECT, keeping row mapping stable.
const BUILD_COLUMNS: &str =
    "id, project, version, commit_hash, dir_path, built_at_ms, last_used_at_ms";

fn row_to_build(row: &rusqlite::Row<'_>) -> rusqlite::Result<BuildRow> {
    Ok(BuildRow {
        id: row.get(0)?,
        project: row.get(1)?,
        version: row.get(2)?,
        commit: row.get(3)?,
        dir_rel: row.get(4)?,
        built_at_ms: row.get(5)?,
        last_used_at_ms: row.get(6)?,
    })
}

fn row_to_artifact(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        variant: row.get(0)?,
        filename: row.get(1)?,
        size_bytes: row.get(2)?,
        sha256: row.get(3)?,
    })
}

/// Fetch a single build by row id.
pub(crate) fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<BuildRow>> {
    conn.prepare_cached(&format!("SELECT {BUILD_COLUMNS} FROM builds WHERE id = ?1"))?
        .query_row(params![id], row_to_build)
        .optional()
}

/// All builds of a project at an exact version, newest first.
pub(crate) fn by_project_version(
    conn: &Connection,
    project: &str,
    version: &str,
) -> rusqlite::Result<Vec<BuildRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {BUILD_COLUMNS} FROM builds
         WHERE project = ?1 AND version = ?2
         ORDER BY built_at_ms DESC"
    ))?;
    let rows = stmt.query_map(params![project, version], row_to_build)?;
    rows.collect()
}

/// Builds whose commit is prefix-compatible with `commit` (either one is a
/// prefix of the other — covers short-vs-full SHA in both directions),
/// optionally pinned to a version. Newest first.
pub(crate) fn by_commit(
    conn: &Connection,
    project: &str,
    commit: &str,
    version: Option<&str>,
) -> rusqlite::Result<Vec<BuildRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {BUILD_COLUMNS} FROM builds
         WHERE project = ?1
           AND (commit_hash LIKE ?2 || '%' OR ?2 LIKE commit_hash || '%')
           AND (?3 IS NULL OR version = ?3)
         ORDER BY built_at_ms DESC"
    ))?;
    let rows = stmt.query_map(params![project, commit, version], row_to_build)?;
    rows.collect()
}

/// Builds a given source has been linked to, most recently linked first.
pub(crate) fn by_source(
    conn: &Connection,
    project: &str,
    kind: &str,
    reference: &str,
) -> rusqlite::Result<Vec<BuildRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT b.id, b.project, b.version, b.commit_hash, b.dir_path, b.built_at_ms, b.last_used_at_ms
         FROM builds AS b
         JOIN build_sources AS s ON s.build_id = b.id
         WHERE b.project = ?1 AND s.kind = ?2 AND s.reference = ?3
         ORDER BY s.linked_at_ms DESC, s.id DESC",
    )?;
    let rows = stmt.query_map(params![project, kind, reference], row_to_build)?;
    rows.collect()
}

/// Inventory listing with optional project/version filters, newest first.
pub(crate) fn list(
    conn: &Connection,
    project: Option<&str>,
    version: Option<&str>,
) -> rusqlite::Result<Vec<BuildRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {BUILD_COLUMNS} FROM builds
         WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR version = ?2)
         ORDER BY built_at_ms DESC"
    ))?;
    let rows = stmt.query_map(params![project, version], row_to_build)?;
    rows.collect()
}

/// Distinct versions a project has builds for (unsorted; the store applies
/// version-aware ordering).
pub(crate) fn distinct_versions(conn: &Connection, project: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT version FROM builds WHERE project = ?1")?;
    let rows = stmt.query_map(params![project], |row| row.get(0))?;
    rows.collect()
}

/// Whether some build already occupies this relative directory.
pub(crate) fn dir_taken(conn: &Connection, dir_rel: &str) -> rusqlite::Result<bool> {
    conn.prepare_cached("SELECT 1 FROM builds WHERE dir_path = ?1 LIMIT 1")?
        .query_row(params![dir_rel], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
}

/// Insert a new build; `built_at_ms` seeds `last_used_at_ms` so age-based
/// cleaning of never-used entries follows the build date.
pub(crate) fn insert(
    conn: &Connection,
    project: &str,
    version: &str,
    commit: &str,
    dir_rel: &str,
    built_at_ms: i64,
) -> rusqlite::Result<i64> {
    conn.prepare_cached(
        "INSERT INTO builds (project, version, commit_hash, dir_path, built_at_ms, last_used_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         RETURNING id",
    )?
    .query_row(
        params![project, version, commit, dir_rel, built_at_ms],
        |row| row.get(0),
    )
}

/// Upgrade a stored commit hash to a longer (more precise) one.
pub(crate) fn update_commit_hash(conn: &Connection, id: i64, commit: &str) -> rusqlite::Result<()> {
    conn.prepare_cached("UPDATE builds SET commit_hash = ?2 WHERE id = ?1")?
        .execute(params![id, commit])
        .map(|_| ())
}

/// Record that a build was just used (returned by a lookup or re-stored).
pub(crate) fn touch(conn: &Connection, id: i64, now_ms: i64) -> rusqlite::Result<()> {
    conn.prepare_cached("UPDATE builds SET last_used_at_ms = ?2 WHERE id = ?1")?
        .execute(params![id, now_ms])
        .map(|_| ())
}

/// Insert or refresh an artifact record. The filename is the physical
/// identity within a build; variant/size/hash follow the latest store.
pub(crate) fn upsert_artifact(
    conn: &Connection,
    build_id: i64,
    variant: Option<&str>,
    filename: &str,
    size_bytes: i64,
    sha256: &str,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO build_artifacts (build_id, variant, filename, size_bytes, sha256)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (build_id, filename) DO UPDATE SET
             variant = excluded.variant,
             size_bytes = excluded.size_bytes,
             sha256 = excluded.sha256",
    )?
    .execute(params![build_id, variant, filename, size_bytes, sha256])
    .map(|_| ())
}

/// Insert or refresh a source link; re-linking updates the branch and the
/// link timestamp.
pub(crate) fn upsert_source(
    conn: &Connection,
    build_id: i64,
    kind: &str,
    reference: &str,
    branch: Option<&str>,
    linked_at_ms: i64,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO build_sources (build_id, kind, reference, branch, linked_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (build_id, kind, reference) DO UPDATE SET
             branch = excluded.branch,
             linked_at_ms = excluded.linked_at_ms",
    )?
    .execute(params![build_id, kind, reference, branch, linked_at_ms])
    .map(|_| ())
}

/// Artifact records of a build, ordered by filename for stable output.
pub(crate) fn artifacts_for(
    conn: &Connection,
    build_id: i64,
) -> rusqlite::Result<Vec<ArtifactRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT variant, filename, size_bytes, sha256
         FROM build_artifacts WHERE build_id = ?1 ORDER BY filename",
    )?;
    let rows = stmt.query_map(params![build_id], row_to_artifact)?;
    rows.collect()
}

/// Source links of a build, most recently linked first.
pub(crate) fn sources_for(conn: &Connection, build_id: i64) -> rusqlite::Result<Vec<SourceRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT kind, reference, branch, linked_at_ms
         FROM build_sources WHERE build_id = ?1
         ORDER BY linked_at_ms DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![build_id], |row| {
        Ok(SourceRow {
            kind: row.get(0)?,
            reference: row.get(1)?,
            branch: row.get(2)?,
            linked_at_ms: row.get(3)?,
        })
    })?;
    rows.collect()
}

/// Delete a build row; artifact and source rows cascade.
pub(crate) fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.prepare_cached("DELETE FROM builds WHERE id = ?1")?
        .execute(params![id])
        .map(|_| ())
}

/// Every build's `(id, dir_path)` — the working set for gc reconciliation.
pub(crate) fn all_dirs(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached("SELECT id, dir_path FROM builds")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}
