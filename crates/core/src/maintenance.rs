//! Artifact-cache maintenance — the single implementation of the
//! `apvm cache` actions. The CLI is a thin adapter over it, and any other
//! front end (the Node bindings are next) is meant to wrap it the same way.
//!
//! [`CacheMaintenance`] wraps one cache directory and exposes every action:
//! [`usage`](CacheMaintenance::usage), [`clean`](CacheMaintenance::clean),
//! [`clear`](CacheMaintenance::clear), [`gc`](CacheMaintenance::gc),
//! [`verify`](CacheMaintenance::verify) and
//! [`repair`](CacheMaintenance::repair). It owns every *decision*, so front
//! ends only map input and render output — and cannot drift apart:
//!
//! - **Validate first.** Input is checked before the disk is touched, so bad
//!   input fails the same way whether or not the cache exists yet.
//! - **Only real caches.** A directory is a cache only when it holds the
//!   cache database. A missing directory, or one holding nothing a cache
//!   would own, means nothing was cached: every action returns `Ok(None)`
//!   and creates nothing. Anything else is an error, never reported as
//!   empty: someone else's data ([`apvm_storage::Error::ForeignDirectory`]),
//!   a cache whose database was lost ([`apvm_storage::Error::MissingDatabase`]
//!   — `repair` rebuilds it), or a path that cannot be inspected.
//! - **Surface, don't swallow.** Unlike the build path, errors propagate —
//!   the caller asked for the operation. A corrupt database
//!   ([`apvm_storage::Error::DatabaseCorrupted`], or unreadable rows:
//!   [`apvm_storage::Error::Data`]) comes back untouched inside
//!   [`Error::Storage`], so each front end can add its own recovery hint;
//!   [`CacheMaintenance::repair`] is the remedy.
//!
//! Each action opens its own store and drops it before returning. All I/O is
//! blocking — from async code, run it on `tokio::task::spawn_blocking`. The
//! actions ignore [`Config::cache_enabled`](crate::Config::cache_enabled): a
//! cache that is turned off can still be inspected and cleaned.
//!
//! ```no_run
//! use apvm_core::maintenance::{CacheMaintenance, CleanRequest, CleanTarget};
//!
//! # fn main() -> apvm_core::Result<()> {
//! let cache = CacheMaintenance::new("/home/me/.apvm/cache");
//! let request = CleanRequest::default()
//!     .older_than(Some("30d".to_string()))
//!     .target(CleanTarget::Builds);
//! match cache.clean(&request)? {
//!     Some(report) => println!("removed {} build(s)", report.builds_deleted),
//!     None => println!("nothing has been cached yet"),
//! }
//! # Ok(())
//! # }
//! ```

use std::io;
use std::path::{Path, PathBuf};

use apvm_storage::{ArtifactStore, CleanOptions};
use chrono::{DateTime, Utc};

use crate::error::{Error, Result};

/// The storage types this API takes and returns, so callers need no direct
/// `apvm-storage` dependency.
pub use apvm_storage::{
    CleanReport, CleanTarget, GcReport, IssueContext, ProjectUsage, RepairReport, StoreState,
    UsageReport, VerifyIssue, VerifyMode, VerifyProblem,
};

// ============================================================================
// CacheMaintenance
// ============================================================================

/// Maintenance handle for the artifact cache in one directory.
///
/// Cheap to create and holds no open store; see the [module docs](self) for
/// the rules every action follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMaintenance {
    dir: PathBuf,
}

impl CacheMaintenance {
    /// Create a handle for the cache at `dir`. Touches nothing on disk.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The cache directory this handle operates on.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a cache is here: `Ok(true)` only when the directory holds the
    /// cache database, `Ok(false)` when it is missing or holds nothing a
    /// cache would own. Never creates anything.
    ///
    /// # Errors
    ///
    /// - [`Error::Config`] — the path is empty (the store would resolve it
    ///   to the current directory).
    /// - [`Error::Io`] — the path cannot be inspected. On Unix: a component
    ///   is a regular file, or a parent is not searchable; other platforms
    ///   may report such paths as not found instead.
    /// - [`Error::Storage`] — the path is not a directory
    ///   ([`apvm_storage::Error::Io`]), holds someone else's data
    ///   ([`apvm_storage::Error::ForeignDirectory`]), or is a cache whose
    ///   database was lost ([`apvm_storage::Error::MissingDatabase`]).
    pub fn exists(&self) -> Result<bool> {
        match self.state()? {
            StoreState::Present => Ok(true),
            StoreState::Orphaned { adoptable_builds } => {
                Err(Error::Storage(apvm_storage::Error::MissingDatabase {
                    path: self.dir.clone(),
                    adoptable_builds,
                }))
            }
            StoreState::Foreign { entry } => {
                Err(Error::Storage(apvm_storage::Error::ForeignDirectory {
                    path: self.dir.clone(),
                    entry,
                }))
            }
            // Missing, Empty — and any future state: never operate on it.
            _ => Ok(false),
        }
    }

    /// Usage totals and per-project breakdown (`apvm cache info`).
    ///
    /// Returns `Ok(None)` when there is no cache (see
    /// [`CacheMaintenance::exists`]).
    ///
    /// # Errors
    ///
    /// Those of [`CacheMaintenance::exists`]; storage failures — notably a
    /// corrupt database — as [`Error::Storage`].
    pub fn usage(&self) -> Result<Option<UsageReport>> {
        self.with_store(ArtifactStore::usage)
    }

    /// Remove the entries `request` selects (`apvm cache clean`), or with
    /// `dry_run` only report them. Returns `Ok(None)` when there is no
    /// cache — but only after validating the request, so invalid input
    /// fails either way.
    ///
    /// # Errors
    ///
    /// Those of [`CleanRequest::to_options`], then as for
    /// [`CacheMaintenance::usage`].
    pub fn clean(&self, request: &CleanRequest) -> Result<Option<CleanReport>> {
        let options = request.to_options(Utc::now())?;
        self.with_store(|store| store.clean(&options))
    }

    /// Remove every build and cached release (`apvm cache clear`). Asks for
    /// no confirmation — prompting is the front end's job.
    ///
    /// Returns `Ok(None)` when there is no cache.
    ///
    /// # Errors
    ///
    /// As for [`CacheMaintenance::usage`].
    pub fn clear(&self) -> Result<Option<CleanReport>> {
        self.with_store(ArtifactStore::clear_all)
    }

    /// Reconcile the database with disk (`apvm cache gc [--checksum]`):
    /// remove what [`verify`](CacheMaintenance::verify) at the same `mode`
    /// reports — records whose files are missing or damaged, with the bad
    /// files — plus orphan directories and stale temp files. The next build
    /// caches a removed entry again. Removes only what the store could have
    /// created, never follows symlinks, and keeps files it cannot read
    /// (listed in [`GcReport::failures`]).
    ///
    /// Returns `Ok(None)` when there is no cache.
    ///
    /// # Errors
    ///
    /// As for [`CacheMaintenance::usage`].
    pub fn gc(&self, mode: VerifyMode) -> Result<Option<GcReport>> {
        self.with_store(|store| store.gc_with(mode))
    }

    /// Check stored files at the depth `mode` selects (`apvm cache verify`).
    /// An empty list means healthy. Reports only: nothing is fixed or
    /// deleted — [`gc`](CacheMaintenance::gc) at the same `mode` removes
    /// what it reports.
    ///
    /// Returns `Ok(None)` when there is no cache.
    ///
    /// # Errors
    ///
    /// As for [`CacheMaintenance::usage`].
    pub fn verify(&self, mode: VerifyMode) -> Result<Option<Vec<VerifyIssue>>> {
        self.with_store(|store| store.verify(mode))
    }

    /// Recover the cache (`apvm cache repair`): reset a corrupt database in
    /// place, or clear an unreadable one (keeping a copy of either), or
    /// rebuild a missing, blank or half-repaired one; then re-index the
    /// builds found on disk. A no-op on a healthy database (the report
    /// records nothing done). Crash-safe and never renames the database —
    /// see [`ArtifactStore::repair`]. [`Apvm`](crate::Apvm) instances of
    /// this process first let go of the cache (they reopen it on their next
    /// build), since a corrupt database is only reset once nobody has it
    /// open.
    ///
    /// Returns `Ok(None)` when there is no cache — and then creates nothing.
    ///
    /// # Errors
    ///
    /// Those of [`CacheMaintenance::exists`], except that a lost database is
    /// what repair fixes; [`Error::Storage`] wrapping
    /// [`apvm_storage::Error::DatabaseInUse`] when another process (or a
    /// build running in this one) holds a corrupt database open — nothing is
    /// changed; retry once it is done — and otherwise when copying,
    /// resetting or re-filling fails, or the schema is newer than this build.
    pub fn repair(&self) -> Result<Option<RepairReport>> {
        match self.state()? {
            StoreState::Missing | StoreState::Empty => Ok(None),
            // Present or Orphaned; storage itself refuses a foreign directory.
            _ => {
                // Idle instances in this process would keep a corrupt
                // database open, and repair refuses to reset one in use.
                crate::cache_status::release_stores(&self.dir);
                let (_store, report) = ArtifactStore::repair(&self.dir)?;
                Ok(Some(report))
            }
        }
    }

    /// Classify the directory — the checks every action starts with.
    fn state(&self) -> Result<StoreState> {
        if self.dir.as_os_str().is_empty() {
            return Err(Error::Config(
                "the cache directory path is empty".to_string(),
            ));
        }
        // Surfaced as `Error::Io` with its kind, so callers can classify it.
        let exists = self.dir.try_exists().map_err(|e| {
            Error::Io(io::Error::new(
                e.kind(),
                format!(
                    "cannot access cache directory '{}': {e}",
                    self.dir.display()
                ),
            ))
        })?;
        if !exists {
            return Ok(StoreState::Missing);
        }
        Ok(ArtifactStore::inspect(&self.dir)?)
    }

    /// Run `op` on the cache's store, or return `Ok(None)` — creating
    /// nothing — when there is no cache (including one removed meanwhile).
    ///
    /// # Errors
    ///
    /// Those of [`CacheMaintenance::exists`], of opening the store, and of
    /// `op`, all wrapped as [`Error::Storage`] where they come from storage.
    fn with_store<T>(
        &self,
        op: impl FnOnce(&ArtifactStore) -> apvm_storage::Result<T>,
    ) -> Result<Option<T>> {
        if !self.exists()? {
            return Ok(None);
        }
        let Some(store) = ArtifactStore::open_existing(&self.dir)? else {
            return Ok(None);
        };
        Ok(Some(op(&store)?))
    }
}

// ============================================================================
// CleanRequest
// ============================================================================

/// What [`CacheMaintenance::clean`] removes — the `apvm cache clean` flags,
/// one field each. The default (no filters) removes everything.
///
/// Build it with [`CleanRequest::default`] and the chainable setters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanRequest {
    /// Only entries not used within this window, as a [`parse_duration`]
    /// spec such as `30d`. `None` = any age.
    pub older_than: Option<String>,
    /// Only this project's entries. `None` = every project.
    pub project: Option<String>,
    /// Which record kinds to remove: builds, releases, or both.
    pub target: CleanTarget,
    /// Report what would be removed without deleting anything.
    pub dry_run: bool,
}

impl CleanRequest {
    /// Set the age filter: a [`parse_duration`] spec, or `None` for any age.
    #[must_use]
    pub fn older_than(mut self, spec: Option<String>) -> Self {
        self.older_than = spec;
        self
    }

    /// Set the project filter, or `None` for every project.
    #[must_use]
    pub fn project(mut self, project: Option<String>) -> Self {
        self.project = project;
        self
    }

    /// Choose which record kinds to remove.
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

    /// Resolve into validated storage [`CleanOptions`], measuring
    /// `older_than` back from `now`.
    ///
    /// Touches nothing on disk — which is what lets a bad request fail the
    /// same way whether or not the cache exists.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a malformed or out-of-range `older_than`
    /// (checked first); [`Error::Storage`] wrapping
    /// [`apvm_storage::Error::InvalidInput`] for an invalid `project`.
    pub fn to_options(&self, now: DateTime<Utc>) -> Result<CleanOptions> {
        let mut options = CleanOptions::default()
            .target(self.target)
            .dry_run(self.dry_run);
        if let Some(spec) = &self.older_than {
            options = options.older_than(cutoff(spec, now)?);
        }
        if let Some(project) = &self.project {
            options = options.project(project.clone());
        }
        options.validate()?;
        Ok(options)
    }
}

// ============================================================================
// Durations
// ============================================================================

/// Parse a human duration like `30d`, `12h`, `2w`, `45m` into a
/// [`chrono::Duration`]. Units: `m` minutes, `h` hours, `d` days, `w` weeks
/// (`min`/`hr`/`day`/`wk` and their plurals work too); surrounding
/// whitespace is ignored.
///
/// # Errors
///
/// A human-readable message for a missing unit, an invalid amount, an
/// unknown unit, or an amount too large to represent.
pub fn parse_duration(spec: &str) -> std::result::Result<chrono::Duration, String> {
    let spec = spec.trim();
    let split = spec
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("'{spec}' is missing a unit — use m, h, d, or w (e.g. 30d)"))?;
    let (number, unit) = spec.split_at(split);
    let value: i64 = number
        .parse()
        .map_err(|_| format!("'{spec}' has an invalid amount"))?;
    // Checked constructors: an absurd amount must produce an error, not a
    // panic (chrono's unchecked constructors panic on overflow).
    let duration = match unit {
        "m" | "min" | "mins" => chrono::Duration::try_minutes(value),
        "h" | "hr" | "hrs" => chrono::Duration::try_hours(value),
        "d" | "day" | "days" => chrono::Duration::try_days(value),
        "w" | "wk" | "wks" => chrono::Duration::try_weeks(value),
        other => {
            return Err(format!(
                "unknown time unit '{other}' in '{spec}' — use m, h, d, or w"
            ));
        }
    };
    duration.ok_or_else(|| format!("'{spec}' is out of range"))
}

/// `now − parse_duration(spec)`: entries last used before this instant are
/// "older than" the spec.
///
/// # Errors
///
/// [`Error::Config`] with [`parse_duration`]'s message, or the same "out of
/// range" message when the duration reaches past the earliest instant
/// chrono can represent — one message for any duration that is too large.
fn cutoff(spec: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let duration = parse_duration(spec).map_err(Error::Config)?;
    now.checked_sub_signed(duration)
        .ok_or_else(|| Error::Config(format!("'{}' is out of range", spec.trim())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use apvm_storage::{BuildMetadata, BuildSource, ReleaseMetadata, SourceArtifact};

    const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // ---- helpers ------------------------------------------------------------

    /// A temp root plus the path of a cache directory inside it that does
    /// not exist yet.
    fn temp_cache() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cache");
        (root, dir)
    }

    /// A source file to copy into the store.
    fn source(name: &str) -> (tempfile::TempDir, SourceArtifact) {
        let src = tempfile::tempdir().unwrap();
        let path = src.path().join(name);
        std::fs::write(&path, b"artifact-bytes").unwrap();
        let artifact = SourceArtifact {
            variant_id: None,
            path,
            target_name: name.to_string(),
        };
        (src, artifact)
    }

    /// Store a single-artifact build last used `age` ago; returns the path
    /// of the stored artifact.
    fn seed_build(dir: &Path, project: &str, commit: &str, age: chrono::Duration) -> PathBuf {
        let store = ArtifactStore::open(dir).unwrap();
        let (_src, artifact) = source(&format!("{project}-1.0.0.zip"));
        let metadata = BuildMetadata::new(
            project,
            "1.0.0",
            BuildSource::PullRequest(1),
            commit,
            "develop".to_string(),
        )
        .with_built_at(Utc::now() - age);
        let stored = store.store(&metadata, &[artifact]).unwrap();
        stored.build.artifacts[0].path.clone()
    }

    /// Cache a single-asset release.
    fn seed_release(dir: &Path, project: &str, tag: &str) {
        let store = ArtifactStore::open(dir).unwrap();
        let (_src, asset) = source(&format!("{project}-{tag}.zip"));
        store
            .store_release(&ReleaseMetadata::new(project, tag), &[asset])
            .unwrap();
    }

    /// Put a file SQLite rejects where the database belongs.
    fn corrupt_database(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("apvm.db"), b"not a sqlite database").unwrap();
    }

    /// Whether `err` is a corrupt-database storage error.
    fn is_corrupt(err: &Error) -> bool {
        matches!(
            err,
            Error::Storage(apvm_storage::Error::DatabaseCorrupted { .. })
        )
    }

    /// `(builds, releases)` currently recorded.
    fn counts(cache: &CacheMaintenance) -> (u64, u64) {
        let usage = cache.usage().unwrap().expect("cache exists");
        (usage.build_count, usage.release_count)
    }

    /// Every entry below `dir`, relative and sorted.
    fn tree(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in std::fs::read_dir(&current).unwrap().flatten() {
                let path = entry.path();
                out.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                if entry.file_type().unwrap().is_dir() {
                    stack.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// Run every action; `exists`, then usage, clean, clear, gc, both verify
    /// modes and repair, each reduced to `Ok(is_some)` or its error.
    fn every_action(cache: &CacheMaintenance) -> Vec<(&'static str, Result<bool>)> {
        vec![
            ("exists", cache.exists()),
            ("usage", cache.usage().map(|r| r.is_some())),
            (
                "clean",
                cache.clean(&CleanRequest::default()).map(|r| r.is_some()),
            ),
            ("clear", cache.clear().map(|r| r.is_some())),
            ("gc", cache.gc(VerifyMode::Size).map(|r| r.is_some())),
            (
                "gc --checksum",
                cache.gc(VerifyMode::Checksum).map(|r| r.is_some()),
            ),
            (
                "verify",
                cache.verify(VerifyMode::Size).map(|r| r.is_some()),
            ),
            (
                "verify --checksum",
                cache.verify(VerifyMode::Checksum).map(|r| r.is_some()),
            ),
            ("repair", cache.repair().map(|r| r.is_some())),
        ]
    }

    // ---- parse_duration (moved from the CLI) ----------------------------------

    #[test]
    fn parse_duration_units() {
        assert_eq!(
            parse_duration("45m").unwrap(),
            chrono::Duration::minutes(45)
        );
        assert_eq!(parse_duration("12h").unwrap(), chrono::Duration::hours(12));
        assert_eq!(parse_duration("30d").unwrap(), chrono::Duration::days(30));
        assert_eq!(parse_duration("2w").unwrap(), chrono::Duration::weeks(2));
        assert_eq!(parse_duration(" 7d ").unwrap(), chrono::Duration::days(7));
    }

    #[test]
    fn parse_duration_rejects_bad_input() {
        assert!(parse_duration("30").is_err(), "missing unit");
        assert!(parse_duration("d").is_err(), "missing amount");
        assert!(parse_duration("30y").is_err(), "unknown unit");
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("-5d").is_err(), "negative amount");
        // Absurd amounts must error, never panic (checked chrono constructors).
        assert!(
            parse_duration("9223372036854775807m").is_err(),
            "overflowing amount"
        );
    }

    #[test]
    fn parse_duration_error_messages_are_exact() {
        // These messages are the shared CLI/library contract for bad input.
        let cases = [
            (
                "30",
                "'30' is missing a unit — use m, h, d, or w (e.g. 30d)",
            ),
            ("d", "'d' has an invalid amount"),
            ("30y", "unknown time unit 'y' in '30y' — use m, h, d, or w"),
            (
                "9223372036854775807m",
                "'9223372036854775807m' is out of range",
            ),
        ];
        for (spec, message) in cases {
            assert_eq!(parse_duration(spec).unwrap_err(), message, "{spec}");
        }
    }

    // ---- CleanRequest::to_options (pure) ------------------------------------

    #[test]
    fn to_options_maps_every_field() {
        let now = Utc::now();
        let options = CleanRequest::default()
            .older_than(Some("30d".to_string()))
            .project(Some("backwpup".to_string()))
            .target(CleanTarget::Releases)
            .dry_run(true)
            .to_options(now)
            .unwrap();
        assert_eq!(options.older_than, Some(now - chrono::Duration::days(30)));
        assert_eq!(options.project.as_deref(), Some("backwpup"));
        assert_eq!(options.target, CleanTarget::Releases);
        assert!(options.dry_run);
    }

    #[test]
    fn default_request_is_an_unfiltered_clean() {
        let options = CleanRequest::default().to_options(Utc::now()).unwrap();
        assert_eq!(options.older_than, None);
        assert_eq!(options.project, None);
        assert_eq!(options.target, CleanTarget::All);
        assert!(!options.dry_run);
    }

    #[test]
    fn to_options_reports_the_duration_error_before_the_project_error() {
        let err = CleanRequest::default()
            .older_than(Some("30y".to_string()))
            .project(Some("Bad/Name".to_string()))
            .to_options(Utc::now())
            .unwrap_err();
        assert!(
            matches!(&err, Error::Config(message) if message.contains("unknown time unit")),
            "{err}"
        );
    }

    #[test]
    fn to_options_rejects_an_age_reaching_past_the_earliest_instant() {
        // Parses (fits a chrono Duration) but lands before chrono's minimum
        // date — reported like any other too-large duration.
        let err = CleanRequest::default()
            .older_than(Some(" 100000000w ".to_string()))
            .to_options(Utc::now())
            .unwrap_err();
        assert!(
            matches!(&err, Error::Config(message) if message == "'100000000w' is out of range"),
            "{err}"
        );
    }

    // ---- no cache: nothing created, invalid input still rejected --------------

    #[test]
    fn every_action_on_a_missing_cache_is_none_and_creates_nothing() {
        let (_root, dir) = temp_cache();
        let cache = CacheMaintenance::new(&dir);
        assert_eq!(cache.dir(), dir.as_path());
        for (action, outcome) in every_action(&cache) {
            assert!(matches!(outcome, Ok(false)), "{action}: {outcome:?}");
        }
        assert!(!dir.exists(), "no action may create the cache directory");
    }

    #[test]
    fn an_empty_directory_is_not_a_cache_and_stays_empty() {
        // Audit finding: read-only actions used to create a database here.
        let (_root, dir) = temp_cache();
        std::fs::create_dir_all(&dir).unwrap();
        let cache = CacheMaintenance::new(&dir);
        for (action, outcome) in every_action(&cache) {
            assert!(matches!(outcome, Ok(false)), "{action}: {outcome:?}");
        }
        assert!(tree(&dir).is_empty(), "no action may write into it");
    }

    #[test]
    fn invalid_clean_input_fails_even_without_a_cache() {
        let (_root, dir) = temp_cache();
        let cache = CacheMaintenance::new(&dir);

        let bad_age = CleanRequest::default().older_than(Some("30y".to_string()));
        let err = cache.clean(&bad_age).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");

        let bad_project = CleanRequest::default().project(Some("Bad/Name".to_string()));
        let err = cache.clean(&bad_project).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Storage(apvm_storage::Error::InvalidInput {
                    what: "project",
                    ..
                })
            ),
            "{err}"
        );
        assert!(
            !dir.exists(),
            "validation must not create the cache directory"
        );
    }

    #[test]
    fn an_empty_path_is_rejected_rather_than_meaning_the_current_directory() {
        // The store would resolve "" to the working directory.
        let cache = CacheMaintenance::new("");
        for (action, outcome) in every_action(&cache) {
            assert!(
                matches!(&outcome, Err(Error::Config(message)) if message.contains("empty")),
                "{action}: {outcome:?}"
            );
        }
    }

    // Unix only: ENOTDIR for a path below a regular file is POSIX behavior;
    // other platforms may report such a path as not found.
    #[cfg(unix)]
    #[test]
    fn an_inaccessible_path_is_an_error_for_every_action() {
        let (root, _) = temp_cache();
        let file = root.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let cache = CacheMaintenance::new(file.join("cache"));

        for (action, outcome) in every_action(&cache) {
            match outcome {
                Err(Error::Io(err)) => {
                    assert_eq!(err.kind(), io::ErrorKind::NotADirectory, "{action}");
                    assert!(
                        err.to_string().contains("cannot access cache directory"),
                        "{action}: {err}"
                    );
                }
                other => panic!("{action}: expected an I/O error, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_regular_file_is_not_a_cache() {
        let (root, _) = temp_cache();
        let file = root.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let cache = CacheMaintenance::new(&file);
        for (action, outcome) in every_action(&cache) {
            assert!(
                matches!(outcome, Err(Error::Storage(apvm_storage::Error::Io { .. }))),
                "{action}: {outcome:?}"
            );
        }
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
    }

    #[test]
    fn a_foreign_directory_is_refused_by_every_action_and_left_untouched() {
        // Audit finding: gc used to delete such data.
        let (_root, dir) = temp_cache();
        for rel in [
            "my-plugin/releases/1.0/important.txt",
            "tools/commits/2024/q1/r.md",
        ] {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"user data").unwrap();
        }
        let before = tree(&dir);
        let cache = CacheMaintenance::new(&dir);

        for (action, outcome) in every_action(&cache) {
            assert!(
                matches!(
                    &outcome,
                    Err(Error::Storage(apvm_storage::Error::ForeignDirectory { entry, .. }))
                        if entry == "my-plugin"
                ),
                "{action}: {outcome:?}"
            );
        }
        assert_eq!(tree(&dir), before, "nothing may be created or deleted");
    }

    #[test]
    fn a_lost_database_is_reported_and_repair_rebuilds_it() {
        let (_root, dir) = temp_cache();
        seed_build(&dir, "wp-rocket", COMMIT_A, chrono::Duration::zero());
        for name in ["apvm.db", "apvm.db-wal", "apvm.db-shm"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let cache = CacheMaintenance::new(&dir);

        let missing = |outcome: &Result<bool>| {
            matches!(
                outcome,
                Err(Error::Storage(apvm_storage::Error::MissingDatabase {
                    adoptable_builds: 1,
                    ..
                }))
            )
        };
        // `repair` runs last: every action before it must report the lost
        // database; repair itself rebuilds it.
        for (action, outcome) in every_action(&cache) {
            let expected = if action == "repair" {
                matches!(outcome, Ok(true))
            } else {
                missing(&outcome)
            };
            assert!(expected, "{action}: {outcome:?}");
        }
        assert_eq!(counts(&cache), (1, 0), "repair re-indexed the build");
    }

    // ---- actions on a seeded cache -----------------------------------------

    #[test]
    fn usage_counts_builds_and_releases_per_project() {
        let (_root, dir) = temp_cache();
        seed_build(&dir, "backwpup", COMMIT_A, chrono::Duration::zero());
        seed_release(&dir, "wp-rocket", "v3.17.4");

        let usage = CacheMaintenance::new(&dir).usage().unwrap().unwrap();
        assert_eq!((usage.build_count, usage.release_count), (1, 1));
        let projects: Vec<&str> = usage.projects.iter().map(|p| p.project.as_str()).collect();
        assert_eq!(projects, ["backwpup", "wp-rocket"]);
    }

    #[test]
    fn clean_honors_dry_run_age_project_and_target() {
        let (_root, dir) = temp_cache();
        seed_build(&dir, "backwpup", COMMIT_A, chrono::Duration::days(30));
        seed_build(&dir, "wp-rocket", COMMIT_B, chrono::Duration::zero());
        seed_release(&dir, "wp-rocket", "v3.17.4");
        seed_release(&dir, "backwpup", "v5.7.6");
        let cache = CacheMaintenance::new(&dir);

        // A dry run selects everything but deletes nothing.
        let preview = cache
            .clean(&CleanRequest::default().dry_run(true))
            .unwrap()
            .unwrap();
        assert_eq!((preview.builds_deleted, preview.releases_deleted), (2, 2));
        assert!(preview.dry_run);
        assert_eq!(counts(&cache), (2, 2));

        // Age: only the build unused for 30 days goes.
        let aged = CleanRequest::default().older_than(Some("7d".to_string()));
        let report = cache.clean(&aged).unwrap().unwrap();
        assert_eq!((report.builds_deleted, report.releases_deleted), (1, 0));

        // Project + target: only wp-rocket's release goes — backwpup's stays.
        let scoped = CleanRequest::default()
            .project(Some("wp-rocket".to_string()))
            .target(CleanTarget::Releases);
        let report = cache.clean(&scoped).unwrap().unwrap();
        assert_eq!((report.builds_deleted, report.releases_deleted), (0, 1));
        assert_eq!(counts(&cache), (1, 1));

        // Project alone: the rest of wp-rocket goes, backwpup is untouched.
        let project = CleanRequest::default().project(Some("wp-rocket".to_string()));
        let report = cache.clean(&project).unwrap().unwrap();
        assert_eq!((report.builds_deleted, report.releases_deleted), (1, 0));
        assert_eq!(counts(&cache), (0, 1));
    }

    #[test]
    fn clear_removes_everything() {
        let (_root, dir) = temp_cache();
        seed_build(&dir, "backwpup", COMMIT_A, chrono::Duration::zero());
        seed_release(&dir, "backwpup", "v5.7.6");
        let cache = CacheMaintenance::new(&dir);

        let report = cache.clear().unwrap().unwrap();
        assert_eq!((report.builds_deleted, report.releases_deleted), (1, 1));
        assert_eq!(counts(&cache), (0, 0));
    }

    #[test]
    fn gc_drops_records_whose_directory_vanished() {
        let (_root, dir) = temp_cache();
        let artifact = seed_build(&dir, "wp-rocket", COMMIT_A, chrono::Duration::zero());
        std::fs::remove_dir_all(artifact.parent().unwrap()).unwrap();
        let cache = CacheMaintenance::new(&dir);

        let report = cache.gc(VerifyMode::Size).unwrap().unwrap();
        assert_eq!(report.stale_build_rows, 1);
        assert_eq!(counts(&cache), (0, 0));
    }

    #[test]
    fn gc_removes_what_verify_reports_at_the_same_depth() {
        let (_root, dir) = temp_cache();
        let artifact = seed_build(&dir, "wp-rocket", COMMIT_A, chrono::Duration::zero());
        let mut bytes = std::fs::read(&artifact).unwrap();
        bytes[0] ^= 0xFF; // same size: only a checksum pass sees it
        std::fs::write(&artifact, &bytes).unwrap();
        let cache = CacheMaintenance::new(&dir);

        let report = cache.gc(VerifyMode::Size).unwrap().unwrap();
        assert_eq!(report.damaged_artifacts, 0);
        assert_eq!(
            cache.verify(VerifyMode::Checksum).unwrap().unwrap().len(),
            1
        );

        let report = cache.gc(VerifyMode::Checksum).unwrap().unwrap();
        assert_eq!(report.damaged_artifacts, 1);
        assert!(!artifact.exists());
        assert!(
            cache
                .verify(VerifyMode::Checksum)
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert_eq!(counts(&cache), (0, 0));
    }

    #[test]
    fn verify_depth_follows_the_mode() {
        let (_root, dir) = temp_cache();
        let artifact = seed_build(&dir, "wp-rocket", COMMIT_A, chrono::Duration::zero());
        // Same-size corruption: invisible to a size check, caught by checksums.
        let mut bytes = std::fs::read(&artifact).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(&artifact, &bytes).unwrap();
        let cache = CacheMaintenance::new(&dir);

        assert!(cache.verify(VerifyMode::Size).unwrap().unwrap().is_empty());
        let issues = cache.verify(VerifyMode::Checksum).unwrap().unwrap();
        assert_eq!(issues.len(), 1);
        assert!(matches!(
            issues[0].problem,
            VerifyProblem::ChecksumMismatch { .. }
        ));
    }

    // ---- corruption and repair ----------------------------------------------

    #[test]
    fn a_corrupt_database_is_returned_untouched_until_repaired() {
        let (_root, dir) = temp_cache();
        corrupt_database(&dir);
        let cache = CacheMaintenance::new(&dir);

        assert!(is_corrupt(&cache.usage().unwrap_err()));
        assert!(is_corrupt(
            &cache.clean(&CleanRequest::default()).unwrap_err()
        ));
        assert!(is_corrupt(&cache.clear().unwrap_err()));
        assert!(is_corrupt(&cache.gc(VerifyMode::Size).unwrap_err()));
        assert!(is_corrupt(&cache.verify(VerifyMode::Size).unwrap_err()));
        assert_eq!(
            std::fs::read(dir.join("apvm.db")).unwrap(),
            b"not a sqlite database",
            "the corrupt file must be left exactly as it was"
        );

        let report = cache.repair().unwrap().unwrap();
        assert!(report.quarantined_database.is_some());
        assert_eq!(counts(&cache), (0, 0), "the cache is usable again");
    }

    #[test]
    fn repair_adopts_builds_left_on_disk() {
        // The offline seeding path the Node tests rely on: a garbage database
        // next to a build directory makes repair re-index that build.
        let (_root, dir) = temp_cache();
        let build_dir = dir.join("wp-rocket/commits/3.17.4/aaaaaaa");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("wp-rocket-3.17.4.zip"), b"zip-bytes").unwrap();
        corrupt_database(&dir);
        let cache = CacheMaintenance::new(&dir);

        let report = cache.repair().unwrap().unwrap();
        assert_eq!((report.builds_adopted, report.artifacts_adopted), (1, 1));
        assert_eq!(counts(&cache), (1, 0));
    }

    #[test]
    fn repair_leaves_a_healthy_cache_alone() {
        let (_root, dir) = temp_cache();
        seed_build(&dir, "wp-rocket", COMMIT_A, chrono::Duration::zero());
        let cache = CacheMaintenance::new(&dir);

        let report = cache.repair().unwrap().unwrap();
        assert!(report.quarantined_database.is_none());
        assert!(!report.rebuilt_missing_database);
        assert_eq!(counts(&cache), (1, 0));
    }

    #[test]
    fn the_handle_is_send_and_sync() {
        // The Node bindings run actions inside `spawn_blocking`.
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<CacheMaintenance>();
        assert_send_sync::<CleanRequest>();
    }
}
