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

/// Outcome of [`ArtifactStore::gc_with`] (and [`ArtifactStore::gc`]).
#[derive(Debug, Clone, Default)]
pub struct GcReport {
    /// Build records dropped because their directory no longer exists, or
    /// because none of their files survived (see `damaged_artifacts`).
    pub stale_build_rows: u64,
    /// Release records dropped for the same reasons.
    pub stale_release_rows: u64,
    /// Orphan directories (files with no record) removed — including those
    /// of builds and releases dropped by this run.
    pub orphan_dirs_removed: u64,
    /// Bytes reclaimed from orphan directories.
    pub orphan_bytes_removed: u64,
    /// Stale temporary files swept.
    pub stale_temp_files_removed: u64,
    /// File records (artifacts and release assets) dropped as damaged: the
    /// file was missing, had the wrong size or — in
    /// [`VerifyMode::Checksum`] — the wrong content, or the record's size
    /// could not be read back.
    pub damaged_artifacts: u64,
    /// Bytes of damaged files deleted from disk.
    pub damaged_bytes_removed: u64,
    /// What gc left in place, one line each: files and directories it could
    /// not read or inspect (their records kept), files or directories it
    /// failed to delete, and damaged records outside the store layout or
    /// behind a symlink (dropped, their files untouched).
    pub failures: Vec<String>,
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
    /// The file could not be read or inspected (e.g. permissions). `gc`
    /// keeps it, and its record.
    Unreadable {
        /// Description of the failure.
        details: String,
    },
    /// The file's record cannot be read back (e.g. a negative size): the
    /// database was tampered with. `gc` drops the record and deletes the
    /// file; the next build caches it again.
    InvalidRecord {
        /// Description of the inconsistency.
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
    /// Where a copy of the damaged database was kept, for inspection only —
    /// the database itself is reset (corrupt) or cleared (unreadable rows)
    /// in place. `None` when it was healthy (nothing done), or missing /
    /// blank / half-repaired (see `rebuilt_missing_database`).
    pub quarantined_database: Option<PathBuf>,
    /// `true` when the database was missing, blank or left half-repaired by
    /// an interrupted repair, and the index was built again from disk.
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
            let bytes = row.bytes;
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
            let bytes = row.bytes;
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
    /// [`crate::Error::StaleHandle`] when this handle's database was replaced
    /// (not for a dry run), [`crate::Error::Database`] for SQLite failures.
    /// Directory-removal failures — and directories outside the store
    /// layout or behind a symlink, which are never removed — are collected in
    /// [`CleanReport::failures`], not returned as errors, so one stubborn
    /// directory cannot abort the rest. A directory another record still
    /// names up to letter case is kept silently: its files are still in use.
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
            let lock = StoreLock::acquire(&self.base_dir)?;
            self.ensure_current()?;
            Some(lock)
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
            bytes_freed: sum_bytes(&build_victims).saturating_add(sum_bytes(&release_victims)),
            dry_run: options.dry_run,
            failures: Vec::new(),
        };
        if options.dry_run {
            return Ok(report);
        }

        let shared = {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            for victim in &build_victims {
                db::builds::delete(&tx, victim.id)?;
            }
            for victim in &release_victims {
                db::releases::delete(&tx, victim.id)?;
            }
            // A directory another record still uses (same path up to case:
            // one directory on a case-insensitive filesystem) must stay.
            let mut shared = HashSet::new();
            for victim in build_victims.iter().chain(&release_victims) {
                if db::maintenance::dir_in_use(&tx, &victim.dir_rel)? {
                    shared.insert(victim.dir_rel.clone());
                }
            }
            tx.commit()?;
            shared
        };

        for victim in build_victims.iter().chain(release_victims.iter()) {
            let bytes = victim.bytes;
            if shared.contains(&victim.dir_rel) {
                report.bytes_freed = report.bytes_freed.saturating_sub(bytes);
                continue;
            }
            // A tampered or symlinked `dir_path` is dropped from the index
            // (already done above) but never removed from disk.
            let dir = match layout::owned_dir(&self.base_dir, &victim.dir_rel) {
                Ok(Some(dir)) => dir,
                Ok(None) => {
                    tracing::warn!(
                        dir_rel = %victim.dir_rel,
                        "record directory is outside the store layout or behind a symlink; left on disk"
                    );
                    report.failures.push(format!(
                        "{}: outside the store layout or behind a symlink; left on disk (remove it by hand)",
                        victim.dir_rel
                    ));
                    report.bytes_freed = report.bytes_freed.saturating_sub(bytes);
                    continue;
                }
                // Orphaned files until the next gc, which retries.
                Err(err) => {
                    report
                        .failures
                        .push(format!("cannot inspect {}: {err}", victim.dir_rel));
                    report.bytes_freed = report.bytes_freed.saturating_sub(bytes);
                    continue;
                }
            };
            match fsx::remove_dir_all_if_exists(&dir) {
                Ok(()) => {
                    if let Some(parent) = dir.parent() {
                        fsx::remove_empty_parents(parent, &self.base_dir);
                    }
                }
                Err(err) => {
                    tracing::warn!(dir = %dir.display(), error = %err, "failed to remove directory");
                    report.failures.push(err.detail());
                    // The record is gone but the bytes are still on disk
                    // (orphans until the next gc) — keep the report honest.
                    report.bytes_freed = report.bytes_freed.saturating_sub(bytes);
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

    /// Reconcile the database with the disk at the default depth:
    /// equivalent to [`gc_with(VerifyMode::Size)`](ArtifactStore::gc_with).
    ///
    /// # Errors
    ///
    /// As for [`ArtifactStore::gc_with`].
    pub fn gc(&self) -> Result<GcReport> {
        self.gc_with(VerifyMode::Size)
    }

    /// Reconcile the database with the disk, in both directions, removing
    /// what [`verify`](ArtifactStore::verify) at the same `mode` reports:
    ///
    /// 1. records whose directory is gone are dropped;
    /// 2. damaged files are deleted, then their records dropped: the file is
    ///    missing, has the wrong size or — with [`VerifyMode::Checksum`] —
    ///    the wrong content, or the record's size cannot be read back. A
    ///    build or release left without files (or that never had any) loses
    ///    its record too;
    /// 3. directories in managed locations with no record are removed
    ///    (reclaiming their bytes), including those of step 2;
    /// 4. stale `.apvm-tmp-*` crash leftovers are swept.
    ///
    /// The next store of a dropped entry caches it again. A damaged file's
    /// record goes only once the file is gone, so one gc fails to delete
    /// stays visible to the next verify and gc. Nothing is deleted outside
    /// the store layout, through a symlink, or that another record still
    /// names up to letter case (one file or directory on a case-insensitive
    /// filesystem). What gc leaves in place — unreadable files and
    /// directories, failed deletions — is listed in [`GcReport::failures`].
    /// `Checksum` mode hashes every file first without the store lock (like
    /// `verify`), then re-checks only the suspects under it, so stores are
    /// not blocked for the whole pass.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] when the store lock cannot be taken,
    /// [`crate::Error::StaleHandle`] when this handle's database was
    /// replaced, [`crate::Error::Database`] for SQLite failures.
    pub fn gc_with(&self, mode: VerifyMode) -> Result<GcReport> {
        let suspects = match mode {
            VerifyMode::Checksum => self.checksum_suspects()?,
            VerifyMode::Presence | VerifyMode::Size => HashSet::new(),
        };
        let _lock = StoreLock::acquire(&self.base_dir)?;
        self.ensure_current()?;
        let mut report = GcReport::default();
        let live = self.gc_reconcile_rows(mode, &suspects, &mut report)?;
        self.gc_sweep_disk(&live, &mut report);
        Ok(report)
    }

    /// Check every stored file at the requested depth and report problems.
    /// Read-only: issues are reported, never deleted —
    /// [`gc_with`](ArtifactStore::gc_with) at the same depth removes them.
    /// `Checksum` mode re-hashes every file — thorough but I/O-proportional.
    pub fn verify(&self, mode: VerifyMode) -> Result<Vec<VerifyIssue>> {
        let mut issues = Vec::new();
        for record in self.file_records()? {
            let (path, problem) = self.check_record(mode, &record);
            if let Some(problem) = problem {
                issues.push(VerifyIssue {
                    project: record.project.clone(),
                    context: record_context(&record),
                    filename: record.filename,
                    path,
                    problem,
                });
            }
        }
        Ok(issues)
    }

    /// Run SQLite's full integrity check (`PRAGMA integrity_check`) on the
    /// metadata database: the quick check every open runs, plus a
    /// comparison of every index with its table. Proportional to the
    /// database's size.
    ///
    /// # Errors
    ///
    /// [`crate::Error::DatabaseCorrupted`] when SQLite reports damage.
    pub fn integrity_check(&self) -> Result<()> {
        let conn = self.conn();
        db::integrity_check(&conn, &self.db_path)
    }

    /// Open the store at `base_dir`, recovering whatever can be recovered.
    ///
    /// - **Healthy database:** equivalent to [`ArtifactStore::open`]; the
    ///   report says nothing was done.
    /// - **Corrupt database:** a copy is kept as `apvm.db.corrupt-<timestamp>`
    ///   (its WAL too), the database is **reset in place**, and the builds
    ///   found on disk are **adopted** — their files re-hashed and
    ///   re-indexed.
    /// - **Metadata that cannot be read back** ([`crate::Error::Data`]): a
    ///   snapshot is kept the same way, then the index is cleared and
    ///   re-filled.
    /// - **Missing or blank database, or an interrupted repair**
    ///   ([`StoreState::Orphaned`]): the index is built (again) from disk
    ///   (`rebuilt_missing_database`).
    ///
    /// **Safe beside other users.** The database file is never renamed or
    /// replaced, so open connections — in this process or another — stay
    /// bound to it and see the repaired index. Only a file SQLite can no
    /// longer read as a database needs exclusive access to be reset: while
    /// another connection holds it open, repair fails with
    /// [`crate::Error::DatabaseInUse`], with nothing changed.
    ///
    /// **Crash-safe.** Files are hashed before anything changes; a marker
    /// (`apvm.db.repairing`) is written before the database is touched and
    /// removed once the re-filled index commits (one transaction). An
    /// interrupted repair therefore leaves the store
    /// [`StoreState::Orphaned`] — opening and `gc` refuse — never a
    /// half-filled index whose unlisted builds `gc` would delete; running
    /// repair again completes it.
    ///
    /// Variant labels and source links are not reconstructible; adopted
    /// builds re-learn them on the next store. Release directories are
    /// counted but not adopted (tags are not reliably reconstructible from
    /// directory names) — re-fetch or [`gc`](ArtifactStore::gc) them.
    ///
    /// # Errors
    ///
    /// [`crate::Error::ForeignDirectory`] for a directory holding someone
    /// else's data, [`crate::Error::ForeignDatabase`] for someone else's
    /// SQLite file (nothing is created or changed).
    /// [`crate::Error::DatabaseInUse`] as above. [`crate::Error::Io`] /
    /// [`crate::Error::Database`] when copying, resetting or re-filling
    /// fails. [`crate::Error::UnsupportedSchema`] is passed through
    /// untouched — a newer schema is not corruption.
    pub fn repair(base_dir: impl Into<PathBuf>) -> Result<(Self, RepairReport)> {
        let base_dir = crate::store::absolute(base_dir.into())?;
        // Refuse before creating anything, then classify again under the
        // lock, where the answer is authoritative.
        let state = layout::inspect(&base_dir)?;
        if matches!(state, StoreState::Empty) {
            // Repair would create a store here.
            crate::store::refuse_occupied(&base_dir)?;
        }
        refuse_foreign(&base_dir, state)?;
        std::fs::create_dir_all(&base_dir)
            .io_ctx(|| format!("failed to create store directory {}", base_dir.display()))?;
        let lock_existed = fs::symlink_metadata(base_dir.join(paths::LOCK_FILE_NAME)).is_ok();
        let outcome = Self::repair_locked(base_dir.clone());
        if outcome.is_err() && !lock_existed {
            forget_lock_file(&base_dir);
        }
        outcome
    }

    // ========================================================================
    // Private helpers
    // ========================================================================

    /// [`ArtifactStore::repair`] once the directory exists: take the store
    /// lock, classify again there, and repair.
    fn repair_locked(base_dir: PathBuf) -> Result<(Self, RepairReport)> {
        let _lock = StoreLock::acquire(&base_dir)?;
        match layout::inspect(&base_dir)? {
            StoreState::Orphaned { .. } => Self::rebuild(base_dir, Damage::Lost),
            state => {
                refuse_foreign(&base_dir, state)?;
                Self::repair_database(base_dir)
            }
        }
    }

    /// Repair path for a directory with (or without, if empty) a database:
    /// open it, and rebuild when it is corrupt — the full integrity check
    /// included: a damaged index passes the quick check of an open, yet
    /// fails writes as corrupt — or its metadata cannot be read back. The
    /// caller holds the store lock.
    fn repair_database(base_dir: PathBuf) -> Result<(Self, RepairReport)> {
        let store = match Self::open_locked(&base_dir) {
            Ok(store) => store,
            Err(err) if err.is_corruption() => return Self::rebuild(base_dir, Damage::Corrupt),
            Err(other) => return Err(other),
        };
        match store.integrity_check() {
            Ok(()) => {}
            Err(err) if err.is_corruption() => {
                tracing::warn!(error = %err, "store database fails its integrity check; rebuilding");
                drop(store);
                return Self::rebuild(base_dir, Damage::Corrupt);
            }
            Err(other) => return Err(other),
        }
        match store.check_metadata() {
            Ok(()) => Ok((store, RepairReport::default())),
            Err(err) if err.is_corruption() => {
                tracing::warn!(error = %err, "unreadable store metadata; rebuilding the index");
                drop(store);
                Self::rebuild(base_dir, Damage::Unreadable)
            }
            Err(other) => Err(other),
        }
    }

    /// Rebuild the index from the builds on disk, in place (see
    /// [`ArtifactStore::repair`] for the guarantees). The caller holds the
    /// store lock.
    fn rebuild(base_dir: PathBuf, damage: Damage) -> Result<(Self, RepairReport)> {
        Self::rebuild_with(base_dir, damage, &mut |_| {})
    }

    /// [`Self::rebuild`], calling `before_refill` once the database is
    /// ready to re-fill and the marker is down — the window a crash must
    /// leave the store [`StoreState::Orphaned`] in (tests check it there).
    fn rebuild_with(
        base_dir: PathBuf,
        damage: Damage,
        before_refill: &mut dyn FnMut(&Path),
    ) -> Result<(Self, RepairReport)> {
        let mut report = RepairReport {
            rebuilt_missing_database: damage == Damage::Lost,
            ..RepairReport::default()
        };
        // The slow part, before anything changes.
        let scanned = scan_builds(&base_dir, &mut report);
        let marker = RepairMarker::create(&base_dir)?;
        let opened = match Self::open_locked(&base_dir) {
            // It opens, yet fails its integrity check: reset it all the same.
            Ok(store) if damage == Damage::Corrupt => {
                drop(store);
                Err(Error::DatabaseCorrupted {
                    path: base_dir.join(paths::DB_FILE_NAME),
                    details: "it fails its integrity check".to_string(),
                })
            }
            other => other,
        };
        let store = match opened {
            Ok(store) => {
                if damage == Damage::Unreadable {
                    // Nothing has changed yet if keeping the copy fails.
                    match store.snapshot() {
                        Ok(copy) => report.quarantined_database = Some(copy),
                        Err(err) => {
                            marker.abandon();
                            return Err(err);
                        }
                    }
                }
                store
            }
            Err(err) if err.is_corruption() => {
                let db_path = base_dir.join(paths::DB_FILE_NAME);
                let copy = match copy_damaged(&base_dir, &db_path) {
                    Ok(copy) => copy,
                    Err(err) => {
                        marker.abandon();
                        return Err(err);
                    }
                };
                if let Err(err) =
                    db::reset_in_place(&db_path, StoreOptions::default().busy_timeout_ms())
                {
                    // Nothing changed: the store is as damaged as before, and
                    // the database itself is the evidence — keep no copy.
                    remove_copy(&copy);
                    marker.abandon();
                    return Err(err);
                }
                report.quarantined_database = Some(copy);
                tracing::warn!(db = %db_path.display(), "corrupt store database reset in place");
                Self::open_locked(&base_dir)?
            }
            Err(other) => {
                marker.abandon();
                return Err(other);
            }
        };
        before_refill(&base_dir);
        store.refill(&scanned, &mut report)?;
        marker.finish()?;
        count_orphan_release_dirs(&base_dir, &mut report);
        Ok((store, report))
    }

    /// Replace the whole index with `scanned`, in one transaction committed
    /// durably — fully synced, with `F_FULLFSYNC` where it exists (macOS) —
    /// since the repair marker is removed right after: a commit lost to a
    /// power cut after that would leave an empty index nothing marks as
    /// incomplete, and gc would delete every build.
    fn refill(&self, scanned: &[ScannedBuild], report: &mut RepairReport) -> Result<()> {
        let mut conn = self.conn();
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "fullfsync", true)?;
        let filled = (|| {
            let mut tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            db::maintenance::clear_index(&tx)?;
            insert_scanned(&mut tx, scanned, report)?;
            tx.commit()?;
            Ok(())
        })();
        // Back to the defaults of every store connection (best-effort: the
        // stricter settings are only slower).
        let _ = conn.pragma_update(None, "fullfsync", false);
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        filled
    }

    /// Keep a consistent copy of this (readable) database beside it, as
    /// `apvm.db.corrupt-<timestamp>`; returns where. A base path that is not
    /// UTF-8 cannot be named to SQLite: its files are copied instead.
    fn snapshot(&self) -> Result<PathBuf> {
        let target = quarantine_target(&self.base_dir);
        let Some(target_sql) = target.to_str() else {
            return copy_damaged(&self.base_dir, &self.db_path);
        };
        db::maintenance::snapshot_into(&self.conn(), target_sql)?;
        Ok(target)
    }

    /// Open (creating if needed) the database of a store whose lock the
    /// caller holds. Skips [`ArtifactStore::open`]'s ownership check: repair
    /// has already classified the directory.
    fn open_locked(base_dir: &Path) -> Result<Self> {
        let db_path = base_dir.join(paths::DB_FILE_NAME);
        let options = StoreOptions::default();
        let (conn, file) = db::open(
            &db_path,
            options.busy_timeout_ms(),
            options.full_durability,
            db::OpenMode::CreateIfMissing,
        )?;
        Ok(Self::from_parts(base_dir, db_path, conn, file))
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

    /// Every stored file record: build artifacts, then release assets.
    fn file_records(&self) -> Result<Vec<db::maintenance::FileRecord>> {
        let conn = self.conn();
        let mut records = db::maintenance::build_files(&conn)?;
        records.extend(db::maintenance::release_files(&conn)?);
        Ok(records)
    }

    /// Check one record's file at `mode`, the way verify and gc agree on:
    /// returns where the file is and what is wrong with it, if anything. A
    /// record whose path the store could not have written — a tampered
    /// directory or filename, e.g. one naming a file outside the store — is
    /// [`VerifyProblem::InvalidRecord`], and that file is never touched.
    fn check_record(
        &self,
        mode: VerifyMode,
        record: &db::maintenance::FileRecord,
    ) -> (PathBuf, Option<VerifyProblem>) {
        // Plain components only, so even a tampered record's path stays
        // inside the store.
        let rel = format!("{}/{}", record.dir_rel, record.filename);
        let path = paths::rel_to_abs(&self.base_dir, &rel);
        if !layout::is_store_rel(&record.dir_rel)
            || paths::validate_filename(&record.filename).is_err()
        {
            let problem = VerifyProblem::InvalidRecord {
                details: format!("the recorded path '{rel}' is not one the store writes"),
            };
            return (path, Some(problem));
        }
        let problem = check_file(mode, &path, record.size_bytes, &record.sha256);
        (path, problem)
    }

    /// The records a checksum pass flags, found without the store lock:
    /// the only files [`Self::gc_with`] re-hashes while holding it.
    fn checksum_suspects(&self) -> Result<HashSet<FileKey>> {
        Ok(self
            .file_records()?
            .iter()
            .filter(|record| self.check_record(VerifyMode::Checksum, record).1.is_some())
            .map(|record| (record.kind, record.id))
            .collect())
    }

    /// Steps 1–2 of [`Self::gc_with`]: delete the damaged files, then drop
    /// stale and damaged records in one transaction. Returns the directories
    /// still recorded, ASCII-lowercased, for the disk sweep. The caller holds
    /// the store lock.
    fn gc_reconcile_rows(
        &self,
        mode: VerifyMode,
        suspects: &HashSet<FileKey>,
        report: &mut GcReport,
    ) -> Result<HashSet<String>> {
        let rows = self.gc_stale_rows(report)?;
        let damaged = self.gc_damaged_files(mode, suspects, &rows, report)?;
        // Files go first here: their records already point at bad data, and
        // a record is dropped only once its file is gone, so a file that
        // cannot be deleted stays visible to the next verify and gc.
        let dropped: Vec<FileKey> = damaged
            .iter()
            .filter(|file| gc_delete_damaged(file, report))
            .map(|file| file.key)
            .collect();
        let (emptied_builds, emptied_releases) = self.gc_commit(&rows, &dropped)?;

        report.stale_build_rows = (rows.builds.len() + emptied_builds.len()) as u64;
        report.stale_release_rows = (rows.releases.len() + emptied_releases.len()) as u64;
        report.damaged_artifacts = dropped.len() as u64;
        let conn = self.conn();
        Ok(db::maintenance::all_dirs(&conn)?
            .iter()
            .map(|dir| dir.to_ascii_lowercase())
            .collect())
    }

    /// Records whose directory is gone, and those still present. A
    /// directory that cannot be inspected is not gone: its records stay,
    /// its files are not judged, and the reason is listed in
    /// `report.failures`.
    fn gc_stale_rows(&self, report: &mut GcReport) -> Result<StaleRows> {
        let conn = self.conn();
        let mut rows = StaleRows::default();
        let builds = db::builds::all_dirs(&conn)?
            .into_iter()
            .map(|row| (true, row));
        let releases = db::releases::all_dirs(&conn)?
            .into_iter()
            .map(|row| (false, row));
        for (is_build, (id, dir_rel)) in builds.chain(releases) {
            let path = paths::rel_to_abs(&self.base_dir, &dir_rel);
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    rows.live.insert(dir_rel);
                }
                // Neither live nor stale: its records stay, unjudged.
                Err(err)
                    if !matches!(
                        err.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    report
                        .failures
                        .push(format!("cannot inspect {}: {err}", path.display()));
                }
                _ if is_build => rows.builds.push(id),
                _ => rows.releases.push(id),
            }
        }
        Ok(rows)
    }

    /// Check the files of present directories as `verify` at `mode` would —
    /// hashing only `suspects` — and return the damaged ones. Files that
    /// cannot be read are kept and listed in `report.failures`.
    fn gc_damaged_files(
        &self,
        mode: VerifyMode,
        suspects: &HashSet<FileKey>,
        rows: &StaleRows,
        report: &mut GcReport,
    ) -> Result<Vec<DamagedFile>> {
        let mut damaged = Vec::new();
        let mut kept: HashSet<String> = HashSet::new();
        for record in self.file_records()? {
            if !rows.live.contains(&record.dir_rel) {
                continue; // dropped with its whole record, or not judged
            }
            let depth = match mode {
                VerifyMode::Checksum if !suspects.contains(&(record.kind, record.id)) => {
                    VerifyMode::Size
                }
                other => other,
            };
            match gc_verdict(self.check_record(depth, &record).1) {
                GcVerdict::Keep => {
                    kept.insert(file_identity(&record));
                }
                // The details name the file ("failed to open <path>: …").
                GcVerdict::Unreadable(details) => {
                    kept.insert(file_identity(&record));
                    report.failures.push(details);
                }
                GcVerdict::Drop { delete_file } => {
                    match self.damaged_file(&record, delete_file, report) {
                        Some(file) => damaged.push(file),
                        None => {
                            kept.insert(file_identity(&record));
                        }
                    }
                }
            }
        }
        // A file a kept record also names (same path up to case: one file on
        // a case-insensitive filesystem) belongs to that record and stays.
        for file in &mut damaged {
            if kept.contains(&file.identity) {
                file.delete = None;
            }
        }
        Ok(damaged)
    }

    /// A record to drop; its file is deleted only when `delete_file` and the
    /// record names a file in a store-produced directory reached without
    /// symlinks ([`layout::owned_dir`]). `None` — keep the record — when that
    /// directory cannot be inspected (listed in `report.failures`).
    fn damaged_file(
        &self,
        record: &db::maintenance::FileRecord,
        delete_file: bool,
        report: &mut GcReport,
    ) -> Option<DamagedFile> {
        let mut delete = None;
        if delete_file {
            let dir = match layout::owned_dir(&self.base_dir, &record.dir_rel) {
                Ok(dir) => dir,
                Err(err) => {
                    report
                        .failures
                        .push(format!("cannot inspect {}: {err}", record.dir_rel));
                    return None;
                }
            };
            delete = dir
                .filter(|_| paths::validate_filename(&record.filename).is_ok())
                .map(|dir| dir.join(&record.filename));
            if delete.is_none() {
                report.failures.push(format!(
                    "{}/{}: outside the store layout or behind a symlink; dropped the record, \
                     left the file",
                    record.dir_rel, record.filename
                ));
            }
        }
        Some(DamagedFile {
            key: (record.kind, record.id),
            identity: file_identity(record),
            delete,
        })
    }

    /// Drop stale rows and the `dropped` file records, then every build or
    /// release left without files, in one transaction. Returns the
    /// directories of the emptied builds and releases.
    fn gc_commit(
        &self,
        rows: &StaleRows,
        dropped: &[FileKey],
    ) -> Result<(Vec<String>, Vec<String>)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for id in &rows.builds {
            db::builds::delete(&tx, *id)?;
        }
        for id in &rows.releases {
            db::releases::delete(&tx, *id)?;
        }
        for (kind, id) in dropped {
            db::maintenance::delete_file(&tx, *kind, *id)?;
        }
        let builds = db::maintenance::drop_empty_builds(&tx)?;
        let releases = db::maintenance::drop_empty_releases(&tx)?;
        tx.commit()?;
        Ok((builds, releases))
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
        if live.contains(&rel.to_ascii_lowercase()) {
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
                report.failures.push(err.detail());
            }
        }
    }
}

// ============================================================================
// Free helpers
// ============================================================================

/// An on-disk build directory hashed for adoption by repair.
struct ScannedBuild {
    build: layout::BuildDir,
    /// The directory's modification time (now, if unreadable).
    built_at: DateTime<Utc>,
    /// Each adoptable file with its digest.
    files: Vec<(String, fsx::FileDigest)>,
}

/// Hash every adoptable build under `base_dir` — the slow part of a repair,
/// done before any index is created or changed. Best-effort by design: a
/// directory with nothing adoptable, or a file that cannot be read, is
/// skipped and counted, never fatal.
fn scan_builds(base_dir: &Path, report: &mut RepairReport) -> Vec<ScannedBuild> {
    let mut scanned = Vec::new();
    for build in layout::build_dirs(base_dir, &mut report.entries_skipped) {
        let mut files = Vec::new();
        let mut unreadable = None;
        for (name, path) in layout::adoptable_files(&build.path) {
            match fsx::hash_file(&path) {
                Ok(digest) => files.push((name, digest)),
                Err(err) => {
                    unreadable = Some(err);
                    break;
                }
            }
        }
        if let Some(err) = unreadable {
            tracing::warn!(dir = %build.path.display(), error = %err, "failed to adopt build");
            report.entries_skipped += 1;
            continue;
        }
        if files.is_empty() {
            report.entries_skipped += 1;
            continue;
        }
        let built_at = fs::metadata(&build.path)
            .and_then(|meta| meta.modified())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        scanned.push(ScannedBuild {
            build,
            built_at,
            files,
        });
    }
    scanned
}

/// Index `scanned` inside `tx`, each build under its own savepoint: one
/// that cannot be recorded is skipped and counted, the others are kept.
///
/// # Errors
///
/// [`Error::Database`] when a savepoint cannot be created or released —
/// the whole transaction is then abandoned.
fn insert_scanned(
    tx: &mut rusqlite::Transaction<'_>,
    scanned: &[ScannedBuild],
    report: &mut RepairReport,
) -> Result<()> {
    for entry in scanned {
        let savepoint = tx.savepoint()?;
        match insert_build(&savepoint, entry) {
            Ok(()) => {
                savepoint.commit()?;
                report.builds_adopted += 1;
                report.artifacts_adopted += entry.files.len() as u64;
            }
            // Dropping the savepoint rolls this build back.
            Err(err) => {
                tracing::warn!(dir = %entry.build.path.display(), error = %err, "failed to adopt build");
                report.entries_skipped += 1;
            }
        }
    }
    Ok(())
}

/// Record one scanned build and its files.
fn insert_build(conn: &rusqlite::Connection, entry: &ScannedBuild) -> Result<()> {
    let build = &entry.build;
    let build_id = db::builds::insert(
        conn,
        &build.project,
        &build.version,
        &build.commit,
        &paths::build_dir_rel(&build.project, &build.version, &build.name),
        db::to_ms(entry.built_at),
    )?;
    for (filename, digest) in &entry.files {
        db::builds::upsert_artifact(
            conn,
            build_id,
            None,
            filename,
            db::size_to_db(digest.size_bytes)?,
            &digest.sha256,
        )?;
    }
    Ok(())
}

/// Count release directories left without a record (repair path).
fn count_orphan_release_dirs(base_dir: &Path, report: &mut RepairReport) {
    for project in layout::project_dirs(base_dir) {
        let releases = base_dir.join(&project).join(paths::RELEASES_DIR);
        if let Some(releases) = layout::managed_dir(&releases) {
            report.orphan_release_dirs += layout::subdirectories(&releases).len() as u64;
        }
    }
}

/// Identity of a file record: its table and row id.
type FileKey = (db::maintenance::FileKind, i64);

/// Records whose directory is gone (`builds`, `releases`: row ids) and the
/// directories still present (`live`). Records whose directory cannot be
/// inspected are in neither: kept, not judged.
#[derive(Default)]
struct StaleRows {
    builds: Vec<i64>,
    releases: Vec<i64>,
    live: HashSet<String>,
}

/// A damaged file record gc drops, and the file to delete with it (`None`
/// when nothing is to be deleted).
struct DamagedFile {
    key: FileKey,
    /// [`file_identity`] of the record.
    identity: String,
    delete: Option<PathBuf>,
}

/// The file a record names, lowercased: records whose paths differ only in
/// case name one file on a case-insensitive filesystem.
fn file_identity(record: &db::maintenance::FileRecord) -> String {
    format!("{}/{}", record.dir_rel, record.filename).to_lowercase()
}

/// What gc does with one file record.
#[derive(Debug, PartialEq, Eq)]
enum GcVerdict {
    /// Healthy at the checked depth.
    Keep,
    /// Damaged: drop the record, and delete the file when one is there.
    Drop { delete_file: bool },
    /// The file cannot be read: keep it and report why.
    Unreadable(String),
}

/// What gc does about a record, given what `verify` at the same depth found
/// wrong with it. A record that cannot be read back is dropped, its file
/// with it when the store owns that file; a file that cannot be read is
/// kept.
fn gc_verdict(problem: Option<VerifyProblem>) -> GcVerdict {
    match problem {
        None => GcVerdict::Keep,
        Some(VerifyProblem::Missing) => GcVerdict::Drop { delete_file: false },
        Some(
            VerifyProblem::SizeMismatch { .. }
            | VerifyProblem::ChecksumMismatch { .. }
            | VerifyProblem::InvalidRecord { .. },
        ) => GcVerdict::Drop { delete_file: true },
        Some(VerifyProblem::Unreadable { details }) => GcVerdict::Unreadable(details),
    }
}

/// Delete a damaged file, counting the bytes freed. Returns whether its
/// record may be dropped: the file is gone (or was never to be deleted).
/// A failed deletion is listed in `report.failures` and keeps the record.
fn gc_delete_damaged(file: &DamagedFile, report: &mut GcReport) -> bool {
    let Some(path) = &file.delete else {
        return true;
    };
    // The link's own size for a symlink: only the link is removed.
    let bytes = fs::symlink_metadata(path)
        .ok()
        .filter(fs::Metadata::is_file)
        .map_or(0, |meta| meta.len());
    match fs::remove_file(path) {
        Ok(()) => {
            report.damaged_bytes_removed += bytes;
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            tracing::warn!(file = %path.display(), error = %err, "failed to delete damaged file");
            report
                .failures
                .push(format!("{}: failed to delete: {err}", path.display()));
            false
        }
    }
}

/// Remove the lock file a failed repair created, unless the failure left a
/// repair marker (the store must then stay [`StoreState::Orphaned`], which
/// the lock file proves). A refused repair — someone else's database, a
/// newer schema, a database in use — thus leaves the directory as it found
/// it: a lock file would make it look like a store that owns whatever
/// store-shaped data later appears there. Best-effort.
fn forget_lock_file(base_dir: &Path) {
    if fs::symlink_metadata(base_dir.join(paths::REPAIR_MARKER_NAME)).is_ok() {
        return;
    }
    let _ = fs::remove_file(base_dir.join(paths::LOCK_FILE_NAME));
}

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

/// Sum recorded bytes of victims, saturating the total so a hostile row
/// cannot overflow the report.
fn sum_bytes(victims: &[db::maintenance::Victim]) -> u64 {
    victims
        .iter()
        .fold(0u64, |acc, victim| acc.saturating_add(victim.bytes))
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
    // Checked first, at every depth: gc relies on it to drop such records.
    let expected = match db::size_from_db(recorded_size) {
        Ok(size) => size,
        Err(err) => {
            return Some(VerifyProblem::InvalidRecord {
                details: err.to_string(),
            });
        }
    };
    let actual = match fsx::probe_file(path) {
        Ok(Some(size)) => size,
        Ok(None) => return Some(VerifyProblem::Missing),
        Err(err) => {
            return Some(VerifyProblem::Unreadable {
                details: format!("cannot inspect {}: {err}", path.display()),
            });
        }
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
                details: err.detail(),
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

/// Copy the damaged database at `db_path` (and its WAL, if any) to
/// `apvm.db.corrupt-<timestamp>` — kept for inspection, never read again;
/// returns the copy's path.
///
/// # Errors
///
/// [`Error::Io`] when the database file cannot be copied. A WAL that cannot
/// be copied is only logged: the copy is for forensics.
fn copy_damaged(base_dir: &Path, db_path: &Path) -> Result<PathBuf> {
    let target = quarantine_target(base_dir);
    fs::copy(db_path, &target).io_ctx(|| {
        format!(
            "failed to keep a copy of the damaged database {} as {}",
            db_path.display(),
            target.display()
        )
    })?;
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    if wal.exists() {
        let mut wal_copy = target.clone().into_os_string();
        wal_copy.push("-wal");
        if let Err(err) = fs::copy(&wal, &wal_copy) {
            tracing::warn!(file = %wal.display(), error = %err, "failed to copy the damaged database's WAL");
        }
    }
    Ok(target)
}

/// Remove a copy [`copy_damaged`] made (and its WAL copy), best-effort.
fn remove_copy(copy: &Path) {
    let mut wal = copy.as_os_str().to_owned();
    wal.push("-wal");
    for file in [copy, Path::new(&wal)] {
        let _ = fs::remove_file(file);
    }
}

/// Why repair rebuilds the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Damage {
    /// The database is missing or blank, or a repair was interrupted.
    Lost,
    /// The database does not open, or SQLite reports it corrupt.
    Corrupt,
    /// It opens, but its records cannot be read back.
    Unreadable,
}

/// The marker of a repair in progress ([`paths::REPAIR_MARKER_NAME`]): while
/// it exists, the store is [`StoreState::Orphaned`].
struct RepairMarker {
    path: PathBuf,
    /// Left by an earlier, interrupted repair: never remove it unless this
    /// one completes.
    inherited: bool,
}

impl RepairMarker {
    /// Write the marker durably (unless an interrupted repair left one).
    /// Never through a symlink: it is created exclusively, and an existing
    /// marker that is not a regular file is refused before anything
    /// changes (repair could never remove it, so it would never complete).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when it cannot be written, or an existing one is not a
    /// regular file: repair must not change the database without it.
    fn create(base_dir: &Path) -> Result<Self> {
        let path = base_dir.join(paths::REPAIR_MARKER_NAME);
        let inherited = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => true,
            Ok(_) => {
                return Err(Error::Io {
                    context: format!(
                        "the repair marker {} is not a regular file; remove it, then repair again",
                        path.display()
                    ),
                    source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
                });
            }
            Err(_) => false,
        };
        if !inherited {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .and_then(|file| file.sync_all().map(|()| file))
                .io_ctx(|| format!("failed to write the repair marker {}", path.display()))?;
            drop(file);
            sync_dir(base_dir);
        }
        Ok(Self { path, inherited })
    }

    /// The repaired index is committed: remove the marker.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when it cannot be removed (the store then still needs
    /// a repair, which completes at once).
    fn finish(self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(Error::Io {
                context: format!("failed to remove the repair marker {}", self.path.display()),
                source: err,
            }),
            _ => {
                sync_dir(self.path.parent().unwrap_or(Path::new(".")));
                Ok(())
            }
        }
    }

    /// Repair stopped before changing the database: remove a marker this
    /// repair created (best-effort), keep an inherited one.
    fn abandon(self) {
        if !self.inherited {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Flush a directory's entries to disk (best-effort; a no-op off Unix), so
/// a marker created or removed survives a power loss in that state.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where the repair marker of the store at `dir` lives.
    fn marker_path(dir: &Path) -> PathBuf {
        dir.join(paths::REPAIR_MARKER_NAME)
    }

    #[test]
    fn a_finished_repair_removes_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = RepairMarker::create(dir.path()).unwrap();
        assert!(marker_path(dir.path()).exists());
        marker.finish().unwrap();
        assert!(!marker_path(dir.path()).exists());
    }

    #[test]
    fn an_abandoned_repair_removes_only_its_own_marker() {
        let dir = tempfile::tempdir().unwrap();
        RepairMarker::create(dir.path()).unwrap().abandon();
        assert!(!marker_path(dir.path()).exists());

        // An interrupted repair's marker stays until a repair completes.
        fs::write(marker_path(dir.path()), b"").unwrap();
        let inherited = RepairMarker::create(dir.path()).unwrap();
        assert!(inherited.inherited);
        inherited.abandon();
        assert!(marker_path(dir.path()).exists());
        RepairMarker::create(dir.path()).unwrap().finish().unwrap();
        assert!(!marker_path(dir.path()).exists());
    }

    #[test]
    fn a_crash_before_the_refill_commits_leaves_the_store_orphaned() {
        // Audit: nothing pinned the marker's crash protection — deleting it
        // right after it is written passed every suite.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("store");
        let build = base.join("wp-rocket/commits/3.17.4/a1b2c3d");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("wp-rocket.zip"), b"zip").unwrap();
        fs::write(base.join(paths::DB_FILE_NAME), b"not a sqlite database").unwrap();
        let _lock = StoreLock::acquire(&base).unwrap();

        let mut mid_repair = None;
        let (store, report) =
            ArtifactStore::rebuild_with(base.clone(), Damage::Corrupt, &mut |dir| {
                mid_repair = Some(layout::inspect(dir).unwrap());
            })
            .unwrap();
        assert_eq!(
            mid_repair,
            Some(StoreState::Orphaned {
                adoptable_builds: 1
            }),
            "a crash here must leave the store needing repair"
        );
        assert_eq!(report.builds_adopted, 1);
        assert!(!marker_path(&base).exists());
        drop(store);
        assert_eq!(layout::inspect(&base).unwrap(), StoreState::Present);
    }

    #[test]
    fn a_marker_that_cannot_be_written_stops_the_repair() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        assert!(matches!(
            RepairMarker::create(&missing),
            Err(Error::Io { .. })
        ));
    }
}
