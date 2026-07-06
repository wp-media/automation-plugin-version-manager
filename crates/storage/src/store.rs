//! The [`ArtifactStore`]: open the store, put builds in, get builds out.
//!
//! # Consistency model
//!
//! The invariant is **"database row ⇒ files on disk"**:
//!
//! - `store()` copies files first (atomically) and commits metadata last —
//!   a crash mid-store leaves at most invisible files, never a dangling
//!   record.
//! - deletes remove metadata first and files second — a crash mid-delete
//!   leaves at most orphan files, never a record pointing at nothing.
//!
//! Orphans from either direction are reclaimed by
//! [`ArtifactStore::gc`](crate::ArtifactStore::gc). Read paths additionally
//! verify presence + size of every file before reporting a cache hit, so
//! even externally deleted files degrade to a clean cache miss.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::Utc;
use rusqlite::Connection;

use crate::db;
use crate::error::{Error, IoContext, Result};
use crate::fsx::{self, FileDigest};
use crate::lock::StoreLock;
use crate::paths;
use crate::types::{
    BuildMetadata, BuildSource, SourceArtifact, SourceLink, StoreResult, StoredArtifact,
    StoredBuild,
};

/// Tuning knobs for [`ArtifactStore::open_with`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StoreOptions {
    /// How long concurrent processes wait on a busy database before failing
    /// (SQLite `busy_timeout`). Default: 5 seconds.
    pub busy_timeout: Duration,
    /// `true` fsyncs the database on every commit (`synchronous = FULL`);
    /// the default `false` uses `NORMAL`, which in WAL mode still guarantees
    /// integrity across power loss and can at worst roll back the very last
    /// transaction — the right trade-off for a rebuildable cache.
    pub full_durability: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
            full_durability: false,
        }
    }
}

impl StoreOptions {
    /// SQLite `busy_timeout` in milliseconds, saturating on overflow.
    pub(crate) fn busy_timeout_ms(&self) -> u64 {
        u64::try_from(self.busy_timeout.as_millis()).unwrap_or(u64::MAX)
    }
}

/// SQLite-backed store for build artifacts and cached release assets.
///
/// All metadata lives in `{base_dir}/apvm.db`; artifact files live under
/// `{base_dir}/{project}/commits/{version}/{commit}/` and
/// `{base_dir}/{project}/releases/{tag}/`. Directory paths are recorded
/// relative to `base_dir`, so the store survives being moved.
///
/// The store is `Send + Sync`; methods take `&self` and serialize database
/// access internally. All I/O is blocking — from async code, wrap calls in
/// `tokio::task::spawn_blocking`.
#[derive(Debug)]
pub struct ArtifactStore {
    pub(crate) base_dir: PathBuf,
    pub(crate) db_path: PathBuf,
    pub(crate) conn: Mutex<Connection>,
}

impl ArtifactStore {
    /// Open (creating if needed) the store at `base_dir` with default
    /// [`StoreOptions`].
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the directory cannot be created,
    /// [`Error::DatabaseCorrupted`] if the database file is damaged (see
    /// [`ArtifactStore::repair`]), [`Error::UnsupportedSchema`] if it was
    /// written by a newer apvm.
    pub fn open(base_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with(base_dir, StoreOptions::default())
    }

    /// Open (creating if needed) the store at `base_dir` with explicit
    /// options. See [`ArtifactStore::open`] for the error contract.
    pub fn open_with(base_dir: impl Into<PathBuf>, options: StoreOptions) -> Result<Self> {
        let base_dir = base_dir.into();
        std::fs::create_dir_all(&base_dir)
            .io_ctx(|| format!("failed to create store directory {}", base_dir.display()))?;
        let db_path = base_dir.join(paths::DB_FILE_NAME);
        let conn = db::open(&db_path, options.busy_timeout_ms(), options.full_durability)?;
        Ok(Self {
            base_dir,
            db_path,
            conn: Mutex::new(conn),
        })
    }

    /// The store's base directory.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Path of the metadata database file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Lock the connection. A poisoned mutex is recovered rather than
    /// propagated: SQLite transactions guarantee the database itself is
    /// consistent even if a previous holder panicked mid-operation.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ========================================================================
    // Storing builds
    // ========================================================================

    /// Store a build's artifacts and link its source, idempotently.
    ///
    /// Re-storing the same build (same project + version + prefix-compatible
    /// commit) is cheap: files already present with the recorded size are
    /// reused, damaged or missing ones are re-copied (self-healing), and the
    /// source link is refreshed. Storing a longer commit hash than before
    /// upgrades the recorded hash.
    ///
    /// Files are copied before metadata is committed, so a crash can never
    /// produce a record without its files.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] for malformed metadata or duplicate/unsafe
    /// filenames, [`Error::SourceFileMissing`] if an input file does not
    /// exist, [`Error::Io`]/[`Error::Database`] for I/O and SQLite failures.
    pub fn store(
        &self,
        metadata: &BuildMetadata,
        artifacts: &[SourceArtifact],
    ) -> Result<StoreResult> {
        let commit = validate_build_metadata(metadata)?;
        validate_artifact_inputs(artifacts)?;

        let _lock = StoreLock::acquire(&self.base_dir)?;
        let now = Utc::now();

        // Resolve identity and any already-recorded artifacts (short borrow).
        let (existing, known_artifacts, dir_rel) = {
            let conn = self.conn();
            match find_compatible_row(&conn, &metadata.project, &metadata.version, &commit)? {
                Some(row) => {
                    let known = db::builds::artifacts_for(&conn, row.id)?;
                    let dir_rel = row.dir_rel.clone();
                    (Some(row), known, dir_rel)
                }
                None => {
                    let dir_rel =
                        pick_build_dir(&conn, &metadata.project, &metadata.version, &commit)?;
                    (None, Vec::new(), dir_rel)
                }
            }
        };
        let dir_abs = paths::rel_to_abs(&self.base_dir, &dir_rel);

        // Copy phase — the database stays unlocked while files stream in.
        let (records, newly_stored, reused) =
            copy_artifacts(artifacts, &dir_abs, &known_artifacts)?;

        // Metadata phase — one transaction covers row + artifacts + source.
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let build_id = match &existing {
            Some(row) => {
                if commit.len() > row.commit.len() {
                    db::builds::update_commit_hash(&tx, row.id, &commit)?;
                }
                db::builds::touch(&tx, row.id, db::to_ms(now))?;
                row.id
            }
            None => {
                let built_at_ms = db::to_ms(metadata.built_at.unwrap_or(now));
                db::builds::insert(
                    &tx,
                    &metadata.project,
                    &metadata.version,
                    &commit,
                    &dir_rel,
                    built_at_ms,
                )?
            }
        };
        for (variant, filename, digest) in &records {
            db::builds::upsert_artifact(
                &tx,
                build_id,
                variant.as_deref(),
                filename,
                db::size_to_db(digest.size_bytes)?,
                &digest.sha256,
            )?;
        }
        db::builds::upsert_source(
            &tx,
            build_id,
            metadata.source.kind(),
            &metadata.source.reference(),
            metadata.branch.as_deref(),
            db::to_ms(now),
        )?;
        tx.commit()?;

        let build = self.assemble_build(&conn, build_id)?;
        Ok(StoreResult {
            build,
            newly_stored,
            reused,
        })
    }

    // ========================================================================
    // Finding builds (cache decisions: health-checked, usage-touching)
    // ========================================================================

    /// Find a healthy build by commit (short or full SHA) at an exact
    /// version. Returns `None` when nothing is cached **or** when the cached
    /// files are damaged — either way the caller should rebuild, and the
    /// next [`ArtifactStore::store`] heals the entry.
    pub fn find_by_commit(
        &self,
        project: &str,
        version: &str,
        commit: &str,
    ) -> Result<Option<StoredBuild>> {
        paths::validate_project(project)?;
        paths::validate_version(version)?;
        let commit = paths::validate_commit(commit)?;
        let conn = self.conn();
        let rows = db::builds::by_commit(&conn, project, &commit, Some(version))?;
        self.first_healthy(&conn, rows)
    }

    /// Find a healthy build by commit across **all** versions of a project,
    /// newest first. Useful when the caller does not care which version the
    /// commit was built as.
    pub fn find_commit_in_versions(
        &self,
        project: &str,
        commit: &str,
    ) -> Result<Option<StoredBuild>> {
        paths::validate_project(project)?;
        let commit = paths::validate_commit(commit)?;
        let conn = self.conn();
        let rows = db::builds::by_commit(&conn, project, &commit, None)?;
        self.first_healthy(&conn, rows)
    }

    /// All healthy builds a source has been linked to, most recently linked
    /// first. Damaged builds are skipped with a warning.
    pub fn find_by_source(&self, project: &str, source: &BuildSource) -> Result<Vec<StoredBuild>> {
        paths::validate_project(project)?;
        let conn = self.conn();
        let rows = db::builds::by_source(&conn, project, source.kind(), &source.reference())?;
        let now_ms = db::to_ms(Utc::now());
        let mut builds = Vec::new();
        for row in rows {
            match self.build_from_row(&conn, &row) {
                Ok(build) if artifacts_healthy(&build) => {
                    touch_build_quiet(&conn, row.id, now_ms);
                    builds.push(build);
                }
                Ok(build) => {
                    tracing::warn!(
                        project,
                        commit = %build.commit,
                        "cached build failed health check; skipping"
                    );
                }
                Err(err) => tracing::warn!(project, error = %err, "skipping unreadable build row"),
            }
        }
        Ok(builds)
    }

    /// The most recently linked healthy build for a source, if any.
    pub fn find_latest_by_source(
        &self,
        project: &str,
        source: &BuildSource,
    ) -> Result<Option<StoredBuild>> {
        paths::validate_project(project)?;
        let conn = self.conn();
        let rows = db::builds::by_source(&conn, project, source.kind(), &source.reference())?;
        self.first_healthy(&conn, rows)
    }

    /// Variant ids already stored **and healthy on disk** for a build
    /// (`None` = the variant-less artifact of single-output plugins).
    /// Returns an empty vector when the build is not cached at all.
    pub fn get_existing_variants(
        &self,
        project: &str,
        version: &str,
        commit: &str,
    ) -> Result<Vec<Option<String>>> {
        paths::validate_project(project)?;
        paths::validate_version(version)?;
        let commit = paths::validate_commit(commit)?;
        let conn = self.conn();
        let Some(row) = find_compatible_row(&conn, project, version, &commit)? else {
            return Ok(Vec::new());
        };
        let build = self.build_from_row(&conn, &row)?;
        let mut variants: Vec<Option<String>> = Vec::new();
        for artifact in &build.artifacts {
            let healthy = fsx::file_size(&artifact.path) == Some(artifact.size_bytes);
            if healthy && !variants.contains(&artifact.variant_id) {
                variants.push(artifact.variant_id.clone());
            }
        }
        Ok(variants)
    }

    // ========================================================================
    // Listing (inventory: no health filtering, no usage touching)
    // ========================================================================

    /// Every project with at least one build or cached release, sorted.
    pub fn list_projects(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        Ok(db::maintenance::projects(&conn)?)
    }

    /// Versions a project has builds for, newest first.
    pub fn list_versions(&self, project: &str) -> Result<Vec<String>> {
        paths::validate_project(project)?;
        let conn = self.conn();
        let mut versions = db::builds::distinct_versions(&conn, project)?;
        versions.sort_by(|a, b| paths::cmp_versions(b, a));
        Ok(versions)
    }

    /// Inventory of stored builds, optionally filtered by project and
    /// version, newest first. Unreadable rows are skipped with a warning so
    /// one corrupt record cannot hide the rest of the inventory.
    pub fn list_builds(
        &self,
        project: Option<&str>,
        version: Option<&str>,
    ) -> Result<Vec<StoredBuild>> {
        if let Some(project) = project {
            paths::validate_project(project)?;
        }
        if let Some(version) = version {
            paths::validate_version(version)?;
        }
        let conn = self.conn();
        let rows = db::builds::list(&conn, project, version)?;
        let mut builds = Vec::with_capacity(rows.len());
        for row in rows {
            match self.build_from_row(&conn, &row) {
                Ok(build) => builds.push(build),
                Err(err) => tracing::warn!(error = %err, "skipping unreadable build row"),
            }
        }
        Ok(builds)
    }

    // ========================================================================
    // Deleting builds
    // ========================================================================

    /// Delete one build (metadata first, then files). Returns `false` when
    /// no matching build exists.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the artifact directory could not be removed — the
    /// metadata is already gone by then, so the files are orphans and the
    /// next [`ArtifactStore::gc`](crate::ArtifactStore::gc) reclaims them.
    pub fn delete_build(&self, project: &str, version: &str, commit: &str) -> Result<bool> {
        paths::validate_project(project)?;
        paths::validate_version(version)?;
        let commit = paths::validate_commit(commit)?;

        let _lock = StoreLock::acquire(&self.base_dir)?;
        let row = {
            let conn = self.conn();
            let Some(row) = find_compatible_row(&conn, project, version, &commit)? else {
                return Ok(false);
            };
            db::builds::delete(&conn, row.id)?;
            row
        };
        // Only remove a directory that is genuinely inside the store; a
        // tampered `dir_path` is dropped from the index but never followed
        // into a destructive removal (see `resolve_within_base`).
        match paths::resolve_within_base(&self.base_dir, &row.dir_rel) {
            Some(dir_abs) => {
                fsx::remove_dir_all_if_exists(&dir_abs)?;
                if let Some(parent) = dir_abs.parent() {
                    fsx::remove_empty_parents(parent, &self.base_dir);
                }
            }
            None => tracing::warn!(
                dir_rel = %row.dir_rel,
                "build record had an out-of-store directory; dropped record without removing files"
            ),
        }
        Ok(true)
    }

    // ========================================================================
    // Internal assembly helpers
    // ========================================================================

    /// Load a full [`StoredBuild`] by row id.
    pub(crate) fn assemble_build(&self, conn: &Connection, id: i64) -> Result<StoredBuild> {
        let row = db::builds::get(conn, id)?.ok_or_else(|| Error::Data {
            details: format!("build row {id} disappeared mid-operation"),
        })?;
        self.build_from_row(conn, &row)
    }

    /// Convert a database row (plus its artifacts and sources) into the
    /// public [`StoredBuild`] with absolute paths and UTC timestamps.
    pub(crate) fn build_from_row(
        &self,
        conn: &Connection,
        row: &db::BuildRow,
    ) -> Result<StoredBuild> {
        let dir = paths::rel_to_abs(&self.base_dir, &row.dir_rel);

        let mut artifacts = Vec::new();
        for artifact in db::builds::artifacts_for(conn, row.id)? {
            artifacts.push(StoredArtifact {
                path: dir.join(&artifact.filename),
                variant_id: artifact.variant,
                size_bytes: db::size_from_db(artifact.size_bytes)?,
                sha256: artifact.sha256,
                filename: artifact.filename,
            });
        }

        let mut sources = Vec::new();
        for link in db::builds::sources_for(conn, row.id)? {
            match BuildSource::from_kind_reference(&link.kind, &link.reference) {
                Some(source) => sources.push(SourceLink {
                    source,
                    branch: link.branch,
                    linked_at: db::from_ms(link.linked_at_ms)?,
                }),
                None => tracing::warn!(
                    kind = %link.kind,
                    reference = %link.reference,
                    "skipping unrepresentable source link"
                ),
            }
        }

        Ok(StoredBuild {
            id: row.id,
            project: row.project.clone(),
            version: row.version.clone(),
            commit: row.commit.clone(),
            built_at: db::from_ms(row.built_at_ms)?,
            last_used_at: db::from_ms(row.last_used_at_ms)?,
            dir,
            artifacts,
            sources,
        })
    }

    /// Walk candidate rows newest-first and return the first healthy build,
    /// touching its usage timestamp. Damaged candidates are skipped with a
    /// warning (they will be healed by the next store).
    fn first_healthy(
        &self,
        conn: &Connection,
        rows: Vec<db::BuildRow>,
    ) -> Result<Option<StoredBuild>> {
        let now_ms = db::to_ms(Utc::now());
        for row in rows {
            match self.build_from_row(conn, &row) {
                Ok(build) if artifacts_healthy(&build) => {
                    touch_build_quiet(conn, row.id, now_ms);
                    return Ok(Some(build));
                }
                Ok(build) => tracing::warn!(
                    project = %build.project,
                    commit = %build.commit,
                    "cached build failed health check; treating as cache miss"
                ),
                Err(err) => tracing::warn!(error = %err, "skipping unreadable build row"),
            }
        }
        Ok(None)
    }
}

impl Drop for ArtifactStore {
    fn drop(&mut self) {
        // SQLite's recommended pre-close hook: refresh planner statistics.
        // Purely an optimization — failure is irrelevant.
        let conn = self.conn.get_mut().unwrap_or_else(PoisonError::into_inner);
        let _ = conn.execute_batch("PRAGMA optimize;");
    }
}

// ============================================================================
// Free helpers (also used by release/lookup/maintenance modules)
// ============================================================================

/// Whether every recorded artifact exists on disk with its recorded size.
/// (Checksums are only verified by [`crate::ArtifactStore::verify`] — a
/// size probe per file keeps cache hits cheap.)
pub(crate) fn artifacts_healthy(build: &StoredBuild) -> bool {
    !build.artifacts.is_empty()
        && build
            .artifacts
            .iter()
            .all(|artifact| fsx::file_size(&artifact.path) == Some(artifact.size_bytes))
}

/// Best-effort usage-timestamp bump; failure to touch must never turn a
/// cache hit into an error.
pub(crate) fn touch_build_quiet(conn: &Connection, id: i64, now_ms: i64) {
    if let Err(err) = db::builds::touch(conn, id, now_ms) {
        tracing::warn!(build_id = id, error = %err, "failed to update last-used timestamp");
    }
}

/// Find the row this (project, version, commit) belongs to, treating two
/// commits as the same build when one is a prefix of the other (short vs
/// full SHA of the same commit).
pub(crate) fn find_compatible_row(
    conn: &Connection,
    project: &str,
    version: &str,
    commit: &str,
) -> Result<Option<db::BuildRow>> {
    let rows = db::builds::by_project_version(conn, project, version)?;
    Ok(rows
        .into_iter()
        .find(|row| commit.starts_with(&row.commit) || row.commit.starts_with(commit)))
}

/// Choose the on-disk directory name for a new build: the 7-char short
/// commit, lengthened to 12 or the full hash if another commit already owns
/// that prefix within the same project + version, with a hashed fallback
/// for the pathological case.
fn pick_build_dir(conn: &Connection, project: &str, version: &str, commit: &str) -> Result<String> {
    let mut lengths: Vec<usize> = vec![7, 12, commit.len()];
    lengths.retain(|&len| len <= commit.len());
    lengths.dedup();

    for len in lengths {
        let candidate = paths::build_dir_rel(project, version, &commit[..len]);
        if !db::builds::dir_taken(conn, &candidate)? {
            return Ok(candidate);
        }
    }
    let disambiguator = paths::hash8(&format!("{project}/{version}/{commit}"));
    Ok(paths::build_dir_rel(
        project,
        version,
        &format!("{}-{disambiguator}", &commit[..7]),
    ))
}

/// Validate build metadata and return the normalized (lowercase) commit.
fn validate_build_metadata(metadata: &BuildMetadata) -> Result<String> {
    paths::validate_project(&metadata.project)?;
    paths::validate_version(&metadata.version)?;
    let commit = paths::validate_commit(&metadata.commit)?;
    paths::validate_reference("source reference", &metadata.source.reference())?;
    if let Some(branch) = &metadata.branch {
        paths::validate_reference("branch", branch)?;
    }
    Ok(commit)
}

/// Validate artifact inputs: safe unique filenames (case-insensitively, for
/// case-insensitive filesystems), valid variants, existing source files.
pub(crate) fn validate_artifact_inputs(artifacts: &[SourceArtifact]) -> Result<()> {
    if artifacts.is_empty() {
        return Err(Error::invalid(
            "artifacts",
            "[]",
            "at least one artifact is required",
        ));
    }
    let mut seen: HashSet<String> = HashSet::with_capacity(artifacts.len());
    for artifact in artifacts {
        paths::validate_filename(&artifact.target_name)?;
        if let Some(variant) = &artifact.variant_id {
            paths::validate_variant(variant)?;
        }
        if !seen.insert(artifact.target_name.to_ascii_lowercase()) {
            return Err(Error::invalid(
                "filename",
                &artifact.target_name,
                "duplicate target filename within one store call",
            ));
        }
        if !artifact.path.is_file() {
            return Err(Error::SourceFileMissing {
                path: artifact.path.clone(),
            });
        }
    }
    Ok(())
}

/// Copy artifacts into `dir_abs`, reusing files that are already present
/// with their recorded size. Returns `(variant, filename, digest)` records
/// for the metadata transaction plus the newly-stored / reused filename
/// lists.
type CopyOutcome = (
    Vec<(Option<String>, String, FileDigest)>,
    Vec<String>,
    Vec<String>,
);

fn copy_artifacts(
    artifacts: &[SourceArtifact],
    dir_abs: &Path,
    known: &[db::ArtifactRow],
) -> Result<CopyOutcome> {
    let mut records = Vec::with_capacity(artifacts.len());
    let mut newly_stored = Vec::new();
    let mut reused = Vec::new();

    for artifact in artifacts {
        let dest = dir_abs.join(&artifact.target_name);
        let recorded = known
            .iter()
            .find(|row| row.filename == artifact.target_name);
        let intact = recorded.and_then(|row| {
            let expected = u64::try_from(row.size_bytes).ok()?;
            (fsx::file_size(&dest) == Some(expected)).then_some((expected, row.sha256.clone()))
        });

        match intact {
            Some((size_bytes, sha256)) => {
                reused.push(artifact.target_name.clone());
                records.push((
                    artifact.variant_id.clone(),
                    artifact.target_name.clone(),
                    FileDigest { size_bytes, sha256 },
                ));
            }
            None => {
                let digest = fsx::copy_file_hashed(&artifact.path, &dest)?;
                newly_stored.push(artifact.target_name.clone());
                records.push((
                    artifact.variant_id.clone(),
                    artifact.target_name.clone(),
                    digest,
                ));
            }
        }
    }
    Ok((records, newly_stored, reused))
}
