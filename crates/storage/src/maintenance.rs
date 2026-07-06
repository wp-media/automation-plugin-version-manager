//! Store maintenance: disk usage reporting, cleaning (all / by age / by
//! project, with dry-run), database↔disk reconciliation (`gc`), integrity
//! verification, and corruption recovery (`repair`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::db;
use crate::error::{Error, IoContext, Result};
use crate::fsx;
use crate::lock::StoreLock;
use crate::paths;
use crate::store::{ArtifactStore, StoreOptions};

/// Temp files older than this are considered crash leftovers during gc.
const TEMP_MAX_AGE: Duration = Duration::from_secs(3600);

// ============================================================================
// Report types
// ============================================================================

/// Disk usage of one project (from recorded metadata).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectUsage {
    /// Project identifier.
    pub project: String,
    /// Bytes recorded for build artifacts.
    pub builds_bytes: u64,
    /// Bytes recorded for cached release assets.
    pub releases_bytes: u64,
    /// Number of stored builds.
    pub build_count: u64,
    /// Number of cached releases.
    pub release_count: u64,
}

/// Store-wide disk usage summary.
///
/// Byte counts come from the recorded artifact sizes — exact for healthy
/// stores, instant to compute. (Orphan files invisible to the database are
/// reported and reclaimed by [`ArtifactStore::gc`].)
#[derive(Debug, Clone, Default)]
pub struct UsageReport {
    /// Total recorded bytes (builds + releases).
    pub total_bytes: u64,
    /// Recorded bytes of build artifacts.
    pub builds_bytes: u64,
    /// Recorded bytes of release assets.
    pub releases_bytes: u64,
    /// Number of stored builds.
    pub build_count: u64,
    /// Number of cached releases.
    pub release_count: u64,
    /// Number of stored files (artifacts + assets).
    pub file_count: u64,
    /// Actual size of the metadata database (including WAL sidecars).
    pub database_bytes: u64,
    /// Oldest build timestamp in the store.
    pub oldest_build: Option<DateTime<Utc>>,
    /// Newest build timestamp in the store.
    pub newest_build: Option<DateTime<Utc>>,
    /// Per-project breakdown, sorted by project name.
    pub projects: Vec<ProjectUsage>,
}

/// What a [`clean`](ArtifactStore::clean) call may delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CleanTarget {
    /// Builds and cached releases.
    #[default]
    All,
    /// Builds only.
    Builds,
    /// Cached releases only.
    Releases,
}

/// Filters for [`ArtifactStore::clean`]. Construct with
/// [`CleanOptions::default`] and refine with the builder methods; the
/// struct is `#[non_exhaustive]` so new filters can be added compatibly.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CleanOptions {
    /// Restrict to one project (`None` = every project).
    pub project: Option<String>,
    /// Delete only entries **not used since** this instant (`None` = any
    /// age). "Used" means stored, or returned by a find/lookup — so
    /// recently hit cache entries survive an age-based clean.
    pub older_than: Option<DateTime<Utc>>,
    /// Which record kinds to delete.
    pub target: CleanTarget,
    /// When `true`, report what would be deleted without touching anything.
    pub dry_run: bool,
}

impl CleanOptions {
    /// Restrict the clean to one project.
    #[must_use]
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Only delete entries not used since `cutoff`.
    #[must_use]
    pub fn older_than(mut self, cutoff: DateTime<Utc>) -> Self {
        self.older_than = Some(cutoff);
        self
    }

    /// Choose which record kinds to delete.
    #[must_use]
    pub fn target(mut self, target: CleanTarget) -> Self {
        self.target = target;
        self
    }

    /// Toggle dry-run mode.
    #[must_use]
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}

/// Outcome of a [`clean`](ArtifactStore::clean) call. In dry-run mode the
/// counts describe what *would* be deleted.
#[derive(Debug, Clone, Default)]
pub struct CleanReport {
    /// Builds deleted (or that would be).
    pub builds_deleted: u64,
    /// Releases deleted (or that would be).
    pub releases_deleted: u64,
    /// Recorded bytes freed (or that would be).
    pub bytes_freed: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Directories whose removal failed (metadata is already gone; the
    /// files are orphans until the next [`ArtifactStore::gc`]).
    pub failures: Vec<String>,
}

/// Outcome of [`ArtifactStore::gc`].
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    /// Build records dropped because their directory no longer exists.
    pub stale_build_rows: u64,
    /// Release records dropped because their directory no longer exists.
    pub stale_release_rows: u64,
    /// Orphan directories (files with no record) removed.
    pub orphan_dirs_removed: u64,
    /// Bytes reclaimed from orphan directories.
    pub orphan_bytes_removed: u64,
    /// Stale temporary files swept.
    pub stale_temp_files_removed: u64,
}

/// How deeply [`ArtifactStore::verify`] inspects stored files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Files exist.
    Presence,
    /// Files exist with their recorded size.
    Size,
    /// Files exist, sized correctly, and hash to their recorded SHA-256.
    Checksum,
}

/// Where a verification issue was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueContext {
    /// A build artifact.
    Build {
        /// Version of the build.
        version: String,
        /// Commit of the build.
        commit: String,
    },
    /// A release asset.
    Release {
        /// Tag of the release.
        tag: String,
    },
}

/// What is wrong with a stored file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyProblem {
    /// The file does not exist.
    Missing,
    /// The file exists with the wrong size.
    SizeMismatch {
        /// Recorded size.
        expected: u64,
        /// Size found on disk.
        actual: u64,
    },
    /// The file content does not hash to the recorded SHA-256.
    ChecksumMismatch {
        /// Recorded hash.
        expected: String,
        /// Hash of the on-disk content.
        actual: String,
    },
    /// The file or its record could not be read.
    Unreadable {
        /// Description of the failure.
        details: String,
    },
}

/// One verification finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyIssue {
    /// Project the file belongs to.
    pub project: String,
    /// Build or release context.
    pub context: IssueContext,
    /// The affected filename.
    pub filename: String,
    /// Absolute path of the affected file.
    pub path: PathBuf,
    /// What is wrong.
    pub problem: VerifyProblem,
}

/// Outcome of [`ArtifactStore::repair`].
#[derive(Debug, Clone, Default)]
pub struct RepairReport {
    /// Where the corrupt database was moved, or `None` when the database
    /// was healthy and nothing had to be done.
    pub quarantined_database: Option<PathBuf>,
    /// Builds re-indexed from the files found on disk.
    pub builds_adopted: u64,
    /// Artifact files re-indexed (hashes recomputed).
    pub artifacts_adopted: u64,
    /// Directories/files skipped because their names or contents were not
    /// adoptable.
    pub entries_skipped: u64,
    /// Release directories left on disk without records (release metadata
    /// is not reconstructible from filenames; re-fetch or `gc` them).
    pub orphan_release_dirs: u64,
}

// ============================================================================
// Implementation
// ============================================================================

impl ArtifactStore {
    /// Summarize what the store holds and how much disk it occupies.
    /// Read-only and cheap (a handful of aggregate queries).
    pub fn usage(&self) -> Result<UsageReport> {
        let (build_rows, release_rows, file_count) = {
            let conn = self.conn();
            (
                db::maintenance::usage_builds(&conn)?,
                db::maintenance::usage_releases(&conn)?,
                db::maintenance::file_count(&conn)?,
            )
        };

        let mut report = UsageReport {
            file_count: u64::try_from(file_count).unwrap_or(0),
            database_bytes: self.database_bytes(),
            ..UsageReport::default()
        };
        let mut projects: Vec<ProjectUsage> = Vec::new();

        // Saturating arithmetic throughout: byte/count sums come from the
        // database and should never overflow for a real cache, but a corrupt
        // or hostile row must not be able to panic a read-only report.
        for row in &build_rows {
            let bytes = u64::try_from(row.bytes).unwrap_or(0);
            let count = u64::try_from(row.count).unwrap_or(0);
            report.builds_bytes = report.builds_bytes.saturating_add(bytes);
            report.build_count = report.build_count.saturating_add(count);
            report.oldest_build = min_time(report.oldest_build, row.oldest_ms)?;
            report.newest_build = max_time(report.newest_build, row.newest_ms)?;
            let entry = project_entry(&mut projects, &row.project);
            entry.builds_bytes = bytes;
            entry.build_count = count;
        }
        for row in &release_rows {
            let bytes = u64::try_from(row.bytes).unwrap_or(0);
            let count = u64::try_from(row.count).unwrap_or(0);
            report.releases_bytes = report.releases_bytes.saturating_add(bytes);
            report.release_count = report.release_count.saturating_add(count);
            let entry = project_entry(&mut projects, &row.project);
            entry.releases_bytes = bytes;
            entry.release_count = count;
        }

        projects.sort_by(|a, b| a.project.cmp(&b.project));
        report.total_bytes = report.builds_bytes.saturating_add(report.releases_bytes);
        report.projects = projects;
        Ok(report)
    }

    /// Delete cached entries matching `options`; see [`CleanOptions`].
    ///
    /// Metadata is removed in one transaction first, files second — a crash
    /// in between leaves only orphan files for [`gc`](ArtifactStore::gc).
    /// With `dry_run` the call is read-only and reports what would go.
    ///
    /// # Errors
    ///
    /// [`crate::Error::InvalidInput`] for a malformed project filter,
    /// [`crate::Error::Database`] for SQLite failures. Directory-removal
    /// failures are collected in [`CleanReport::failures`], not returned as
    /// errors, so one stubborn directory cannot abort the rest.
    pub fn clean(&self, options: &CleanOptions) -> Result<CleanReport> {
        if let Some(project) = &options.project {
            paths::validate_project(project)?;
        }
        let older_ms = options.older_than.map(db::to_ms);
        let project = options.project.as_deref();
        let want_builds = matches!(options.target, CleanTarget::All | CleanTarget::Builds);
        let want_releases = matches!(options.target, CleanTarget::All | CleanTarget::Releases);

        // The lock precedes victim selection so a concurrent store/find
        // cannot slip in between "selected" and "deleted".
        let lock = if options.dry_run {
            None
        } else {
            Some(StoreLock::acquire(&self.base_dir)?)
        };

        let (build_victims, release_victims) = {
            let conn = self.conn();
            (
                if want_builds {
                    db::maintenance::build_victims(&conn, project, older_ms)?
                } else {
                    Vec::new()
                },
                if want_releases {
                    db::maintenance::release_victims(&conn, project, older_ms)?
                } else {
                    Vec::new()
                },
            )
        };

        let mut report = CleanReport {
            builds_deleted: build_victims.len() as u64,
            releases_deleted: release_victims.len() as u64,
            bytes_freed: sum_bytes(&build_victims) + sum_bytes(&release_victims),
            dry_run: options.dry_run,
            failures: Vec::new(),
        };
        if options.dry_run {
            return Ok(report);
        }

        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            for victim in &build_victims {
                db::builds::delete(&tx, victim.id)?;
            }
            for victim in &release_victims {
                db::releases::delete(&tx, victim.id)?;
            }
            tx.commit()?;
        }

        for victim in build_victims.iter().chain(release_victims.iter()) {
            // A tampered `dir_path` that escapes the store is dropped from
            // the index (already done above) but never removed from disk.
            let Some(dir) = paths::resolve_within_base(&self.base_dir, &victim.dir_rel) else {
                tracing::warn!(
                    dir_rel = %victim.dir_rel,
                    "record had an out-of-store directory; dropped record without removing files"
                );
                report
                    .failures
                    .push(format!("{}: out-of-store directory", victim.dir_rel));
                report.bytes_freed = report
                    .bytes_freed
                    .saturating_sub(u64::try_from(victim.bytes).unwrap_or(0));
                continue;
            };
            match fsx::remove_dir_all_if_exists(&dir) {
                Ok(()) => {
                    if let Some(parent) = dir.parent() {
                        fsx::remove_empty_parents(parent, &self.base_dir);
                    }
                }
                Err(err) => {
                    tracing::warn!(dir = %dir.display(), error = %err, "failed to remove directory");
                    report.failures.push(format!("{}: {err}", dir.display()));
                    // The record is gone but the bytes are still on disk
                    // (orphans until the next gc) — keep the report honest.
                    report.bytes_freed = report
                        .bytes_freed
                        .saturating_sub(u64::try_from(victim.bytes).unwrap_or(0));
                }
            }
        }
        drop(lock);
        Ok(report)
    }

    /// Delete everything in the store (all builds, all cached releases).
    /// Equivalent to `clean(&CleanOptions::default())`.
    pub fn clear_all(&self) -> Result<CleanReport> {
        self.clean(&CleanOptions::default())
    }

    /// Delete every entry not used since `cutoff` (builds and releases).
    /// Equivalent to `clean(&CleanOptions::default().older_than(cutoff))`.
    pub fn clear_older_than(&self, cutoff: DateTime<Utc>) -> Result<CleanReport> {
        self.clean(&CleanOptions::default().older_than(cutoff))
    }

    /// Reconcile the database with the disk, in both directions:
    ///
    /// 1. records whose directory vanished are dropped;
    /// 2. directories in managed locations with no record are removed
    ///    (reclaiming their bytes);
    /// 3. stale `.apvm-tmp-*` crash leftovers are swept.
    ///
    /// Unrecognized entries outside the managed layout are deliberately
    /// left untouched.
    pub fn gc(&self) -> Result<GcReport> {
        let _lock = StoreLock::acquire(&self.base_dir)?;
        let mut report = GcReport::default();
        let live = self.gc_reconcile_rows(&mut report)?;
        self.gc_sweep_disk(&live, &mut report);
        Ok(report)
    }

    /// Check every stored file at the requested depth and report problems.
    /// Read-only: issues are reported, never auto-deleted. `Checksum` mode
    /// re-hashes every file — thorough but I/O-proportional.
    pub fn verify(&self, mode: VerifyMode) -> Result<Vec<VerifyIssue>> {
        let (build_files, release_files) = {
            let conn = self.conn();
            (
                db::maintenance::build_files(&conn)?,
                db::maintenance::release_files(&conn)?,
            )
        };

        let mut issues = Vec::new();
        for record in build_files.iter().chain(release_files.iter()) {
            let path = paths::rel_to_abs(&self.base_dir, &record.dir_rel).join(&record.filename);
            if let Some(problem) = check_file(mode, &path, record.size_bytes, &record.sha256) {
                issues.push(VerifyIssue {
                    project: record.project.clone(),
                    context: record_context(record),
                    filename: record.filename.clone(),
                    path,
                    problem,
                });
            }
        }
        Ok(issues)
    }

    /// Run SQLite's integrity check on the metadata database.
    ///
    /// # Errors
    ///
    /// [`crate::Error::DatabaseCorrupted`] when SQLite reports damage.
    pub fn integrity_check(&self) -> Result<()> {
        let conn = self.conn();
        db::quick_check(&conn, &self.db_path)
    }

    /// Open the store at `base_dir`, recovering from a corrupt database.
    ///
    /// If the database opens cleanly this is equivalent to
    /// [`ArtifactStore::open`] (the report says nothing was done). If it is
    /// corrupt: the damaged file is **quarantined** (renamed to
    /// `apvm.db.corrupt-<timestamp>`, WAL sidecars included), a fresh
    /// database is created, and builds found on disk are **adopted** — their
    /// files re-hashed and re-indexed. Variant labels and source links are
    /// not reconstructible; adopted builds re-learn them on the next store.
    /// Release directories are counted but not adopted (tags are not
    /// reliably reconstructible from directory names) — re-fetch or
    /// [`gc`](ArtifactStore::gc) them.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`]/[`crate::Error::Database`] when quarantine or
    /// the fresh database creation fails. [`crate::Error::UnsupportedSchema`]
    /// is passed through untouched — a newer schema is not corruption.
    pub fn repair(base_dir: impl Into<PathBuf>) -> Result<(Self, RepairReport)> {
        let base_dir: PathBuf = base_dir.into();
        std::fs::create_dir_all(&base_dir)
            .io_ctx(|| format!("failed to create store directory {}", base_dir.display()))?;
        let _lock = StoreLock::acquire(&base_dir)?;
        let db_path = base_dir.join(paths::DB_FILE_NAME);
        let options = StoreOptions::default();

        match db::open(&db_path, options.busy_timeout_ms(), options.full_durability) {
            Ok(conn) => Ok((
                Self {
                    base_dir,
                    db_path,
                    conn: Mutex::new(conn),
                },
                RepairReport::default(),
            )),
            Err(Error::DatabaseCorrupted { .. }) => {
                let quarantined = quarantine_database(&base_dir, &db_path)?;
                let conn = db::open(&db_path, options.busy_timeout_ms(), options.full_durability)?;
                let store = Self {
                    base_dir,
                    db_path,
                    conn: Mutex::new(conn),
                };
                let mut report = RepairReport {
                    quarantined_database: Some(quarantined),
                    ..RepairReport::default()
                };
                store.adopt_builds_from_disk(&mut report);
                store.count_orphan_release_dirs(&mut report);
                Ok((store, report))
            }
            Err(other) => Err(other),
        }
    }

    // ========================================================================
    // Private helpers
    // ========================================================================

    /// Size of the database file plus its WAL sidecars.
    fn database_bytes(&self) -> u64 {
        ["", "-wal", "-shm"]
            .iter()
            .filter_map(|suffix| {
                let name = format!("{}{suffix}", paths::DB_FILE_NAME);
                fsx::file_size(&self.base_dir.join(name))
            })
            .sum()
    }

    /// Drop records whose directory vanished; return the surviving
    /// (relative) directory set for the disk sweep.
    fn gc_reconcile_rows(&self, report: &mut GcReport) -> Result<HashSet<String>> {
        let mut conn = self.conn();
        let mut live: HashSet<String> = HashSet::new();
        let mut stale_builds: Vec<i64> = Vec::new();
        let mut stale_releases: Vec<i64> = Vec::new();

        for (id, dir_rel) in db::builds::all_dirs(&conn)? {
            if paths::rel_to_abs(&self.base_dir, &dir_rel).is_dir() {
                live.insert(dir_rel);
            } else {
                stale_builds.push(id);
            }
        }
        for (id, dir_rel) in db::releases::all_dirs(&conn)? {
            if paths::rel_to_abs(&self.base_dir, &dir_rel).is_dir() {
                live.insert(dir_rel);
            } else {
                stale_releases.push(id);
            }
        }

        if !stale_builds.is_empty() || !stale_releases.is_empty() {
            let tx = conn.transaction()?;
            for id in &stale_builds {
                db::builds::delete(&tx, *id)?;
            }
            for id in &stale_releases {
                db::releases::delete(&tx, *id)?;
            }
            tx.commit()?;
        }
        report.stale_build_rows = stale_builds.len() as u64;
        report.stale_release_rows = stale_releases.len() as u64;
        Ok(live)
    }

    /// Remove unrecorded directories at managed depths and sweep stale temp
    /// files. Never touches entries outside the managed layout — and a
    /// directory only counts as *inside* it when every path component is a
    /// name the store itself could have produced (valid project and version
    /// identifiers). Foreign trees that merely mimic the layout's shape
    /// (e.g. a hand-placed `My-Backups/commits/...`) are left alone.
    fn gc_sweep_disk(&self, live: &HashSet<String>, report: &mut GcReport) {
        report.stale_temp_files_removed +=
            fsx::remove_stale_temp_files(&self.base_dir, TEMP_MAX_AGE);
        for project in subdirectories(&self.base_dir) {
            if paths::validate_project(&project).is_err() {
                tracing::debug!(name = %project, "gc: skipping non-store directory");
                continue;
            }
            let project_path = self.base_dir.join(&project);
            report.stale_temp_files_removed +=
                fsx::remove_stale_temp_files(&project_path, TEMP_MAX_AGE);

            let commits = project_path.join(paths::COMMITS_DIR);
            for version in subdirectories(&commits) {
                if paths::validate_version(&version).is_err() {
                    tracing::debug!(
                        project = %project,
                        name = %version,
                        "gc: skipping non-store version directory"
                    );
                    continue;
                }
                let version_path = commits.join(&version);
                report.stale_temp_files_removed +=
                    fsx::remove_stale_temp_files(&version_path, TEMP_MAX_AGE);
                for entry in subdirectories(&version_path) {
                    let rel = paths::build_dir_rel(&project, &version, &entry);
                    self.gc_visit_leaf(&version_path.join(&entry), &rel, live, report);
                }
                let _ = fs::remove_dir(&version_path);
            }
            let _ = fs::remove_dir(&commits);

            let releases = project_path.join(paths::RELEASES_DIR);
            for entry in subdirectories(&releases) {
                let rel = paths::release_dir_rel(&project, &entry);
                self.gc_visit_leaf(&releases.join(&entry), &rel, live, report);
            }
            let _ = fs::remove_dir(&releases);
            let _ = fs::remove_dir(&project_path);
        }
    }

    /// Handle one leaf directory: sweep temps when recorded, remove it when
    /// orphaned.
    fn gc_visit_leaf(&self, path: &Path, rel: &str, live: &HashSet<String>, report: &mut GcReport) {
        if live.contains(rel) {
            report.stale_temp_files_removed += fsx::remove_stale_temp_files(path, TEMP_MAX_AGE);
            return;
        }
        let bytes = fsx::dir_size_recursive(path);
        match fsx::remove_dir_all_if_exists(path) {
            Ok(()) => {
                report.orphan_dirs_removed += 1;
                report.orphan_bytes_removed += bytes;
            }
            Err(err) => {
                tracing::warn!(dir = %path.display(), error = %err, "failed to remove orphan directory");
            }
        }
    }

    /// Re-index builds found on disk into a fresh database (repair path).
    /// Best-effort by design: anything unadoptable is skipped and counted,
    /// never fatal — repair must always leave a working store.
    fn adopt_builds_from_disk(&self, report: &mut RepairReport) {
        for project in subdirectories(&self.base_dir) {
            if paths::validate_project(&project).is_err() {
                report.entries_skipped += 1;
                continue;
            }
            let commits = self.base_dir.join(&project).join(paths::COMMITS_DIR);
            for version in subdirectories(&commits) {
                if paths::validate_version(&version).is_err() {
                    report.entries_skipped += 1;
                    continue;
                }
                for entry in subdirectories(&commits.join(&version)) {
                    let commit = match paths::validate_commit(&entry) {
                        Ok(commit) => commit,
                        Err(_) => {
                            report.entries_skipped += 1;
                            continue;
                        }
                    };
                    let dir = commits.join(&version).join(&entry);
                    match self.adopt_one_build(&project, &version, &commit, &entry, &dir) {
                        Ok(0) => report.entries_skipped += 1,
                        Ok(files) => {
                            report.builds_adopted += 1;
                            report.artifacts_adopted += files;
                        }
                        Err(err) => {
                            tracing::warn!(dir = %dir.display(), error = %err, "failed to adopt build");
                            report.entries_skipped += 1;
                        }
                    }
                }
            }
        }
    }

    /// Hash and index the files of one on-disk build directory. Returns the
    /// number of adopted files (0 = nothing adoptable, no record created).
    fn adopt_one_build(
        &self,
        project: &str,
        version: &str,
        commit: &str,
        dir_name: &str,
        dir: &Path,
    ) -> Result<u64> {
        let mut files: Vec<(String, fsx::FileDigest)> = Vec::new();
        let entries = fs::read_dir(dir).io_ctx(|| format!("failed to read {}", dir.display()))?;
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let is_file = entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false);
            if !is_file
                || name.starts_with(paths::TMP_PREFIX)
                || paths::validate_filename(&name).is_err()
            {
                continue;
            }
            files.push((name.clone(), fsx::hash_file(&entry.path())?));
        }
        if files.is_empty() {
            return Ok(0);
        }

        let built_at = fs::metadata(dir)
            .and_then(|meta| meta.modified())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        let dir_rel = paths::build_dir_rel(project, version, dir_name);

        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let build_id =
            db::builds::insert(&tx, project, version, commit, &dir_rel, db::to_ms(built_at))?;
        for (filename, digest) in &files {
            db::builds::upsert_artifact(
                &tx,
                build_id,
                None,
                filename,
                db::size_to_db(digest.size_bytes)?,
                &digest.sha256,
            )?;
        }
        tx.commit()?;
        Ok(files.len() as u64)
    }

    /// Count release directories that now have no record (repair path).
    fn count_orphan_release_dirs(&self, report: &mut RepairReport) {
        for project in subdirectories(&self.base_dir) {
            if paths::validate_project(&project).is_err() {
                continue;
            }
            let releases = self.base_dir.join(&project).join(paths::RELEASES_DIR);
            report.orphan_release_dirs += subdirectories(&releases).len() as u64;
        }
    }
}

// ============================================================================
// Free helpers
// ============================================================================

/// Names of subdirectories of `path` (missing/unreadable dir = empty; names
/// that are not valid UTF-8 are skipped with a warning).
fn subdirectories(path: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        match entry.file_name().into_string() {
            Ok(name) => names.push(name),
            Err(raw) => tracing::warn!(name = ?raw, "skipping non-UTF-8 directory name"),
        }
    }
    names.sort();
    names
}

/// Get (creating if absent) the per-project usage entry for `project`.
fn project_entry<'a>(projects: &'a mut Vec<ProjectUsage>, project: &str) -> &'a mut ProjectUsage {
    if let Some(index) = projects.iter().position(|entry| entry.project == project) {
        return &mut projects[index];
    }
    projects.push(ProjectUsage {
        project: project.to_string(),
        ..ProjectUsage::default()
    });
    let last = projects.len() - 1;
    &mut projects[last]
}

/// Sum recorded bytes of victims, saturating at zero for corrupt negatives
/// and saturating the total so a hostile row cannot overflow the report.
fn sum_bytes(victims: &[db::maintenance::Victim]) -> u64 {
    victims.iter().fold(0u64, |acc, victim| {
        acc.saturating_add(u64::try_from(victim.bytes).unwrap_or(0))
    })
}

/// Fold helper: earliest of an optional running value and an optional row.
fn min_time(current: Option<DateTime<Utc>>, row_ms: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    let Some(ms) = row_ms else { return Ok(current) };
    let at = db::from_ms(ms)?;
    Ok(Some(match current {
        Some(existing) if existing <= at => existing,
        _ => at,
    }))
}

/// Fold helper: latest of an optional running value and an optional row.
fn max_time(current: Option<DateTime<Utc>>, row_ms: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    let Some(ms) = row_ms else { return Ok(current) };
    let at = db::from_ms(ms)?;
    Ok(Some(match current {
        Some(existing) if existing >= at => existing,
        _ => at,
    }))
}

/// Map a db file record to its public issue context.
fn record_context(record: &db::maintenance::FileRecord) -> IssueContext {
    match (&record.build_context, &record.tag_context) {
        (Some((version, commit)), _) => IssueContext::Build {
            version: version.clone(),
            commit: commit.clone(),
        },
        (None, Some(tag)) => IssueContext::Release { tag: tag.clone() },
        (None, None) => IssueContext::Release { tag: String::new() },
    }
}

/// Inspect one file at the requested verification depth.
fn check_file(
    mode: VerifyMode,
    path: &Path,
    recorded_size: i64,
    recorded_sha256: &str,
) -> Option<VerifyProblem> {
    let expected = match db::size_from_db(recorded_size) {
        Ok(size) => size,
        Err(err) => {
            return Some(VerifyProblem::Unreadable {
                details: err.to_string(),
            });
        }
    };
    let Some(actual) = fsx::file_size(path) else {
        return Some(VerifyProblem::Missing);
    };
    if matches!(mode, VerifyMode::Size | VerifyMode::Checksum) && actual != expected {
        return Some(VerifyProblem::SizeMismatch { expected, actual });
    }
    if mode == VerifyMode::Checksum {
        return match fsx::hash_file(path) {
            Ok(digest) if digest.sha256 == recorded_sha256 => None,
            Ok(digest) => Some(VerifyProblem::ChecksumMismatch {
                expected: recorded_sha256.to_string(),
                actual: digest.sha256,
            }),
            Err(err) => Some(VerifyProblem::Unreadable {
                details: err.to_string(),
            }),
        };
    }
    None
}

/// Move the corrupt database (and WAL sidecars) aside; returns where the
/// main file went.
fn quarantine_database(base_dir: &Path, db_path: &Path) -> Result<PathBuf> {
    let stamp = Utc::now().timestamp_millis();
    let target = base_dir.join(format!("{}.corrupt-{stamp}", paths::DB_FILE_NAME));
    fs::rename(db_path, &target).io_ctx(|| {
        format!(
            "failed to quarantine corrupt database {} -> {}",
            db_path.display(),
            target.display()
        )
    })?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = base_dir.join(format!("{}{suffix}", paths::DB_FILE_NAME));
        if !sidecar.exists() {
            continue;
        }
        let sidecar_target =
            base_dir.join(format!("{}.corrupt-{stamp}{suffix}", paths::DB_FILE_NAME));
        if let Err(err) = fs::rename(&sidecar, &sidecar_target) {
            tracing::warn!(file = %sidecar.display(), error = %err, "failed to quarantine WAL sidecar; removing");
            let _ = fs::remove_file(&sidecar);
        }
    }
    tracing::warn!(
        quarantined = %target.display(),
        "corrupt storage database quarantined; rebuilding index"
    );
    Ok(target)
}
