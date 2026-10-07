//! Store maintenance: disk usage reporting, cleaning (all / by age / by
//! project, with dry-run), database↔disk reconciliation (`gc`), integrity
//! verification, and corruption recovery (`repair`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::TransactionBehavior;

use crate::db;
use crate::error::{Error, IoContext, Result};
use crate::fsx;
use crate::layout::{self, StoreState};
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

    /// Check the filters without touching any store.
    ///
    /// [`ArtifactStore::clean`] runs this itself. Call it directly to reject
    /// bad input *before* deciding whether a store exists at all — so the
    /// same input fails the same way whether or not anything was cached.
    ///
    /// # Errors
    ///
    /// [`crate::Error::InvalidInput`] when the project filter is not a valid
    /// project identifier.
    pub fn validate(&self) -> Result<()> {
        match &self.project {
            Some(project) => paths::validate_project(project),
            None => Ok(()),
        }
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
    /// Where the damaged database is kept: moved there when corrupt, copied
    /// there when only its rows were unreadable. `None` when it was healthy
    /// (nothing done) or missing (see `rebuilt_missing_database`).
    pub quarantined_database: Option<PathBuf>,
    /// `true` when the database was missing while store content remained,
    /// and a fresh one was built from the files on disk.
    pub rebuilt_missing_database: bool,
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
        options.validate()?;
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

    /// Open the store at `base_dir`, recovering whatever can be recovered.
    ///
    /// - **Healthy database:** equivalent to [`ArtifactStore::open`]; the
    ///   report says nothing was done.
    /// - **Corrupt database:** the damaged file is **quarantined** (renamed
    ///   to `apvm.db.corrupt-<timestamp>`, WAL sidecars included), a fresh
    ///   database is created, and builds found on disk are **adopted** —
    ///   their files re-hashed and re-indexed. Handles opened before the
    ///   rename keep using the quarantined file; open them again.
    /// - **Metadata that cannot be read back** ([`crate::Error::Data`]): the
    ///   database is copied to `apvm.db.corrupt-<timestamp>`, emptied in
    ///   place and re-filled the same way, so other open handles keep working
    ///   on the repaired index.
    /// - **Missing database with store content left**
    ///   ([`StoreState::Orphaned`]): a fresh database is created and builds
    ///   adopted the same way (`rebuilt_missing_database`).
    ///
    /// Variant labels and source links are not reconstructible; adopted
    /// builds re-learn them on the next store. Release directories are
    /// counted but not adopted (tags are not reliably reconstructible from
    /// directory names) — re-fetch or [`gc`](ArtifactStore::gc) them.
    ///
    /// # Errors
    ///
    /// [`crate::Error::ForeignDirectory`] for a directory holding someone
    /// else's data (nothing is created). [`crate::Error::Io`] /
    /// [`crate::Error::Database`] when quarantine or the fresh database
    /// fails. [`crate::Error::UnsupportedSchema`] is passed through
    /// untouched — a newer schema is not corruption.
    pub fn repair(base_dir: impl Into<PathBuf>) -> Result<(Self, RepairReport)> {
        let base_dir: PathBuf = base_dir.into();
        // Refuse before creating anything, then classify again under the
        // lock, where the answer is authoritative.
        refuse_foreign(&base_dir, layout::inspect(&base_dir)?)?;
        std::fs::create_dir_all(&base_dir)
            .io_ctx(|| format!("failed to create store directory {}", base_dir.display()))?;
        let _lock = StoreLock::acquire(&base_dir)?;
        match layout::inspect(&base_dir)? {
            StoreState::Orphaned { .. } => Self::rebuild(base_dir, None),
            state => {
                refuse_foreign(&base_dir, state)?;
                Self::repair_database(base_dir)
            }
        }
    }

    // ========================================================================
    // Private helpers
    // ========================================================================

    /// Repair path for a directory with (or without, if empty) a database:
    /// open it, and quarantine + rebuild when it is corrupt or its metadata
    /// cannot be read back. The caller holds the store lock.
    fn repair_database(base_dir: PathBuf) -> Result<(Self, RepairReport)> {
        let store = match Self::open_locked(&base_dir) {
            Ok(store) => store,
            Err(Error::DatabaseCorrupted { .. }) => return Self::quarantine_and_rebuild(base_dir),
            Err(other) => return Err(other),
        };
        match store.check_metadata() {
            Ok(()) => Ok((store, RepairReport::default())),
            Err(Error::Data { details }) => {
                tracing::warn!(%details, "unreadable store metadata; rebuilding the index");
                store.rebuild_in_place()
            }
            Err(other) => Err(other),
        }
    }

    /// Rebuild an index whose rows cannot be read, keeping the database
    /// file: copy it aside, empty it, and re-index from disk.
    ///
    /// Other handles (a long-lived `Apvm`, a running build) keep one
    /// connection for their lifetime; renaming the file would leave them on
    /// the quarantined copy, where their writes are lost and their builds
    /// later collected as orphans. Clearing through SQL keeps them on the
    /// live database. A base path that is not UTF-8 cannot be named to
    /// SQLite; that database is renamed aside instead, like a corrupt one.
    fn rebuild_in_place(self) -> Result<(Self, RepairReport)> {
        let target = quarantine_target(&self.base_dir);
        let Some(target_sql) = target.to_str() else {
            let base_dir = self.base_dir.clone();
            drop(self); // an open file cannot be renamed on Windows
            return Self::quarantine_and_rebuild(base_dir);
        };
        {
            let mut conn = self.conn();
            db::maintenance::snapshot_into(&conn, target_sql)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            db::maintenance::clear_index(&tx)?;
            tx.commit()?;
        }
        tracing::warn!(copy = %target.display(), "unreadable store index copied aside and cleared");
        Ok(self.reindex(Some(target), false))
    }

    /// Move the damaged database aside, then rebuild the index from disk.
    fn quarantine_and_rebuild(base_dir: PathBuf) -> Result<(Self, RepairReport)> {
        let db_path = base_dir.join(paths::DB_FILE_NAME);
        let quarantined = quarantine_database(&base_dir, &db_path)?;
        Self::rebuild(base_dir, Some(quarantined))
    }

    /// Create a fresh database and re-index the builds found on disk.
    /// `quarantined` is where the old database went — `None` when it was
    /// missing.
    fn rebuild(base_dir: PathBuf, quarantined: Option<PathBuf>) -> Result<(Self, RepairReport)> {
        let missing = quarantined.is_none();
        Ok(Self::open_locked(&base_dir)?.reindex(quarantined, missing))
    }

    /// Re-index the builds found on disk into this empty index and report
    /// it.
    fn reindex(self, quarantined: Option<PathBuf>, missing: bool) -> (Self, RepairReport) {
        let mut report = RepairReport {
            quarantined_database: quarantined,
            rebuilt_missing_database: missing,
            ..RepairReport::default()
        };
        self.adopt_builds_from_disk(&mut report);
        self.count_orphan_release_dirs(&mut report);
        (self, report)
    }

    /// Open (creating if needed) the database of a store whose lock the
    /// caller holds. Skips [`ArtifactStore::open`]'s ownership check: repair
    /// has already classified the directory.
    fn open_locked(base_dir: &Path) -> Result<Self> {
        let db_path = base_dir.join(paths::DB_FILE_NAME);
        let options = StoreOptions::default();
        let conn = db::open(
            &db_path,
            options.busy_timeout_ms(),
            options.full_durability,
            db::OpenMode::CreateIfMissing,
        )?;
        Ok(Self::from_parts(base_dir, db_path, conn))
    }

    /// Read every stored record back the way lookups and reports do, so
    /// repair can tell an index that opens but cannot be read
    /// ([`Error::Data`]) from a healthy one.
    fn check_metadata(&self) -> Result<()> {
        {
            let conn = self.conn();
            for row in db::builds::list(&conn, None, None)? {
                self.build_from_row(&conn, &row)?;
            }
            for project in db::maintenance::projects(&conn)? {
                for row in db::releases::list_for_project(&conn, &project)? {
                    self.release_from_row(&conn, &row)?;
                }
            }
        }
        self.usage().map(|_| ())
    }

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
    /// files, touching only what the store itself could have created:
    ///
    /// - every path component must be a store-produced name (project and
    ///   version identifiers, build and release directory names);
    /// - symlinks are never followed, so gc cannot reach outside the store
    ///   through a linked `commits`/`releases` directory or child;
    /// - an empty project directory is pruned only if it had that layout.
    ///
    /// Anything else — a hand-placed `My-Backups/commits/...`, a
    /// `tools/commits/2024/q1`, an empty `notes/` — is left alone.
    fn gc_sweep_disk(&self, live: &HashSet<String>, report: &mut GcReport) {
        report.stale_temp_files_removed +=
            fsx::remove_stale_temp_files(&self.base_dir, TEMP_MAX_AGE);
        for project in layout::project_dirs(&self.base_dir) {
            let project_path = self.base_dir.join(&project);
            let commits = layout::managed_dir(&project_path.join(paths::COMMITS_DIR));
            let releases = layout::managed_dir(&project_path.join(paths::RELEASES_DIR));
            if commits.is_none() && releases.is_none() {
                tracing::debug!(name = %project, "gc: skipping non-store directory");
                continue;
            }
            report.stale_temp_files_removed +=
                fsx::remove_stale_temp_files(&project_path, TEMP_MAX_AGE);
            if let Some(commits) = &commits {
                self.gc_sweep_commits(&project, commits, live, report);
            }
            if let Some(releases) = &releases {
                self.gc_sweep_releases(&project, releases, live, report);
            }
            let _ = fs::remove_dir(&project_path);
        }
    }

    /// Sweep one project's `commits` tree (rules: [`Self::gc_sweep_disk`]).
    fn gc_sweep_commits(
        &self,
        project: &str,
        commits: &Path,
        live: &HashSet<String>,
        report: &mut GcReport,
    ) {
        for version in layout::subdirectories(commits) {
            if paths::validate_version(&version).is_err() {
                tracing::debug!(project, name = %version, "gc: skipping non-store version directory");
                continue;
            }
            let version_path = commits.join(&version);
            report.stale_temp_files_removed +=
                fsx::remove_stale_temp_files(&version_path, TEMP_MAX_AGE);
            for entry in layout::subdirectories(&version_path) {
                if !layout::is_build_dir_name(&entry) {
                    tracing::debug!(project, name = %entry, "gc: skipping non-store build directory");
                    continue;
                }
                let rel = paths::build_dir_rel(project, &version, &entry);
                self.gc_visit_leaf(&version_path.join(&entry), &rel, live, report);
            }
            let _ = fs::remove_dir(&version_path);
        }
        let _ = fs::remove_dir(commits);
    }

    /// Sweep one project's `releases` directory (rules:
    /// [`Self::gc_sweep_disk`]).
    fn gc_sweep_releases(
        &self,
        project: &str,
        releases: &Path,
        live: &HashSet<String>,
        report: &mut GcReport,
    ) {
        for entry in layout::subdirectories(releases) {
            if !layout::is_release_dir_name(&entry) {
                tracing::debug!(project, name = %entry, "gc: skipping non-store release directory");
                continue;
            }
            let rel = paths::release_dir_rel(project, &entry);
            self.gc_visit_leaf(&releases.join(&entry), &rel, live, report);
        }
        let _ = fs::remove_dir(releases);
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
        for build in layout::build_dirs(&self.base_dir, &mut report.entries_skipped) {
            match self.adopt_one_build(&build) {
                Ok(0) => report.entries_skipped += 1,
                Ok(files) => {
                    report.builds_adopted += 1;
                    report.artifacts_adopted += files;
                }
                Err(err) => {
                    tracing::warn!(dir = %build.path.display(), error = %err, "failed to adopt build");
                    report.entries_skipped += 1;
                }
            }
        }
    }

    /// Hash and index the files of one on-disk build directory. Returns the
    /// number of adopted files (0 = nothing adoptable, no record created).
    fn adopt_one_build(&self, build: &layout::BuildDir) -> Result<u64> {
        let mut files: Vec<(String, fsx::FileDigest)> = Vec::new();
        for (name, path) in layout::adoptable_files(&build.path) {
            files.push((name, fsx::hash_file(&path)?));
        }
        if files.is_empty() {
            return Ok(0);
        }

        let built_at = fs::metadata(&build.path)
            .and_then(|meta| meta.modified())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        let dir_rel = paths::build_dir_rel(&build.project, &build.version, &build.name);

        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let build_id = db::builds::insert(
            &tx,
            &build.project,
            &build.version,
            &build.commit,
            &dir_rel,
            db::to_ms(built_at),
        )?;
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
        for project in layout::project_dirs(&self.base_dir) {
            let releases = self.base_dir.join(&project).join(paths::RELEASES_DIR);
            if let Some(releases) = layout::managed_dir(&releases) {
                report.orphan_release_dirs += layout::subdirectories(&releases).len() as u64;
            }
        }
    }
}

// ============================================================================
// Free helpers
// ============================================================================

/// Refuse a directory that holds someone else's data — see
/// [`StoreState::Foreign`].
fn refuse_foreign(base_dir: &Path, state: StoreState) -> Result<()> {
    match state {
        StoreState::Foreign { entry } => Err(Error::ForeignDirectory {
            path: base_dir.to_path_buf(),
            entry,
        }),
        _ => Ok(()),
    }
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

/// Where a damaged database is kept: `apvm.db.corrupt-<unix ms>`.
fn quarantine_target(base_dir: &Path) -> PathBuf {
    let stamp = Utc::now().timestamp_millis();
    base_dir.join(format!("{}.corrupt-{stamp}", paths::DB_FILE_NAME))
}

/// Move the corrupt database (and WAL sidecars) aside; returns where the
/// main file went. Connections in this process cannot start meanwhile (see
/// `db::swapping_files`).
fn quarantine_database(base_dir: &Path, db_path: &Path) -> Result<PathBuf> {
    let target = quarantine_target(base_dir);
    let _swapping = db::swapping_files();
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
        let mut sidecar_target = target.clone().into_os_string();
        sidecar_target.push(suffix);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantine_waits_for_connections_that_are_starting() {
        // A connection that opened the old file must finish its first read
        // before the rename, or it would share the new database's memory.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(paths::DB_FILE_NAME);
        fs::write(&db_path, b"damaged").unwrap();

        let starting = db::connection_starting();
        let quarantine = {
            let (base, db_path) = (dir.path().to_path_buf(), db_path.clone());
            std::thread::spawn(move || quarantine_database(&base, &db_path))
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(db_path.exists(), "renamed while a connection was starting");
        drop(starting);
        let target = quarantine
            .join()
            .expect("quarantine thread panicked")
            .unwrap();
        assert!(!db_path.exists());
        assert_eq!(fs::read(target).unwrap(), b"damaged");
    }
}
