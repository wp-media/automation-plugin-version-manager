//! JavaScript types for cache maintenance ([`ApvmCache`](crate::cache::ApvmCache)
//! and `Apvm.cacheStatus()`), with their conversions from the core types.
//!
//! Conventions, as in [`crate::types`]: byte sizes and counts are `number`
//! (`f64`: exact up to 2⁵³, no truncating casts); paths are strings
//! (lossy UTF-8); timestamps are ISO-8601 (RFC 3339) strings; enum-like
//! fields are lowercase string literals, and fields that do not apply are
//! absent rather than `null`-filled with placeholders.

use std::path::Path;

use apvm_core::CacheStatus;
use apvm_core::maintenance::{
    CleanReport, GcReport, IssueContext, ProjectUsage, RepairReport, UsageReport, VerifyIssue,
    VerifyProblem,
};
use napi_derive::napi;

// =============================================================================
// Inputs
// =============================================================================

/// Which record kinds `clean()` removes — pass a member, e.g.
/// `JsCleanTarget.Builds`.
///
/// Members are not enumerable (napi-rs): read them by name —
/// `Object.values()` returns `[]`.
#[napi(string_enum)]
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "declares the TypeScript type; values are read by `cache_input`"
)]
pub enum JsCleanTarget {
    /// Builds and cached releases (the default).
    All,
    /// Only builds.
    Builds,
    /// Only cached releases.
    Releases,
}

/// Options for `ApvmCache.clean()` — the `apvm cache clean` flags, one field
/// each. With no options, `clean()` removes everything, as in the CLI.
///
/// Read strictly, since the default is destructive: an unknown key, a
/// wrong type, or a key set to `undefined` / `null` rejects with
/// `code: 'InvalidArg'` — omit a key to leave it unset.
///
/// # TypeScript
///
/// ```typescript
/// await cache.clean({ olderThan: '30d', project: 'backwpup', target: JsCleanTarget.Builds });
/// const preview = await cache.clean({ dryRun: true }); // reports, deletes nothing
/// ```
#[napi(object)]
#[derive(Debug, Default)]
#[allow(
    dead_code,
    reason = "declares the TypeScript type; values are read by `cache_input`"
)]
pub struct CleanOptions {
    /// Only entries not used within this window: an amount plus a unit —
    /// `m` minutes, `h` hours, `d` days, `w` weeks (e.g. `30d`, `12h`).
    /// Omitted = any age.
    pub older_than: Option<String>,
    /// Only this project's entries (e.g. `wp-rocket`). Omitted = every
    /// project.
    pub project: Option<String>,
    /// Report what would be removed without deleting anything. Default `false`.
    pub dry_run: Option<bool>,
    /// Which record kinds to remove. Default `All`.
    pub target: Option<JsCleanTarget>,
}

/// Options for `ApvmCache.gc()`. An unknown key or a wrong type rejects
/// with `code: 'InvalidArg'`.
#[napi(object)]
#[derive(Debug, Default)]
#[allow(
    dead_code,
    reason = "declares the TypeScript type; values are read by `cache_input`"
)]
pub struct GcOptions {
    /// Also re-hash every file and remove those whose content no longer
    /// matches (slowest, most thorough). Default `false`: size check only.
    pub checksum: Option<bool>,
}

/// Options for `ApvmCache.verify()`. An unknown key or a wrong type rejects
/// with `code: 'InvalidArg'`.
#[napi(object)]
#[derive(Debug, Default)]
#[allow(
    dead_code,
    reason = "declares the TypeScript type; values are read by `cache_input`"
)]
pub struct VerifyOptions {
    /// Re-hash every file and compare to its recorded checksum (slowest,
    /// most thorough). Default `false`: presence and size only.
    pub checksum: Option<bool>,
}

// =============================================================================
// Usage — info()
// =============================================================================

/// Per-project share of a [`JsCacheUsage`].
#[napi(object)]
#[derive(Debug, Clone, PartialEq)]
pub struct JsProjectUsage {
    /// Project name (e.g. `wp-rocket`).
    pub project: String,
    /// Number of cached builds.
    pub build_count: f64,
    /// Number of cached releases.
    pub release_count: f64,
    /// Bytes of build artifacts.
    pub builds_bytes: f64,
    /// Bytes of release assets.
    pub releases_bytes: f64,
}

impl From<&ProjectUsage> for JsProjectUsage {
    fn from(usage: &ProjectUsage) -> Self {
        Self {
            project: usage.project.clone(),
            build_count: usage.build_count as f64,
            release_count: usage.release_count as f64,
            builds_bytes: usage.builds_bytes as f64,
            releases_bytes: usage.releases_bytes as f64,
        }
    }
}

/// What `ApvmCache.info()` reports (`apvm cache info`).
#[napi(object)]
#[derive(Debug, Clone, PartialEq)]
pub struct JsCacheUsage {
    /// The cache directory inspected.
    pub cache_dir: String,
    /// `false` when no cache exists there yet (the directory is missing or
    /// empty — often a sign of the wrong `cacheDir`); every count is then 0.
    pub exists: bool,
    /// Bytes of all cached files (builds + releases).
    pub total_bytes: f64,
    /// Bytes of build artifacts.
    pub builds_bytes: f64,
    /// Bytes of release assets.
    pub releases_bytes: f64,
    /// Number of cached builds.
    pub build_count: f64,
    /// Number of cached releases.
    pub release_count: f64,
    /// Number of cached files (artifacts + assets).
    pub file_count: f64,
    /// Size of the cache database itself.
    pub database_bytes: f64,
    /// When the oldest cached build was built (ISO-8601). Not its last use:
    /// `clean({ olderThan })` goes by last use.
    pub oldest_build: Option<String>,
    /// When the newest cached build was built (ISO-8601).
    pub newest_build: Option<String>,
    /// Per-project breakdown.
    pub projects: Vec<JsProjectUsage>,
}

impl JsCacheUsage {
    /// The report for the cache at `dir`: `None` (no cache) becomes
    /// `exists: false` with every count 0.
    pub fn new(dir: &Path, report: Option<UsageReport>) -> Self {
        let exists = report.is_some();
        let report = report.unwrap_or_default();
        Self {
            cache_dir: dir.to_string_lossy().into_owned(),
            exists,
            total_bytes: report.total_bytes as f64,
            builds_bytes: report.builds_bytes as f64,
            releases_bytes: report.releases_bytes as f64,
            build_count: report.build_count as f64,
            release_count: report.release_count as f64,
            file_count: report.file_count as f64,
            database_bytes: report.database_bytes as f64,
            oldest_build: report.oldest_build.map(|t| t.to_rfc3339()),
            newest_build: report.newest_build.map(|t| t.to_rfc3339()),
            projects: report.projects.iter().map(JsProjectUsage::from).collect(),
        }
    }
}

// =============================================================================
// Clean / clear
// =============================================================================

/// What `ApvmCache.clean()` and `clear()` removed (or, with `dryRun`,
/// would remove).
#[napi(object)]
#[derive(Debug, Clone, PartialEq)]
pub struct JsCleanReport {
    /// Builds removed.
    pub builds_deleted: f64,
    /// Cached releases removed.
    pub releases_deleted: f64,
    /// Bytes freed.
    pub bytes_freed: f64,
    /// Whether this was a dry run (nothing deleted).
    pub dry_run: bool,
    /// Directories whose removal failed, one line each, with what to do.
    /// Their records are already gone; the next `gc()` retries those inside
    /// the cache layout, and the line says when one must be removed by hand.
    pub failures: Vec<String>,
}

impl JsCleanReport {
    /// The report of a clean, or of nothing to clean (`None`: no cache) —
    /// which still echoes `dry_run`.
    pub fn new(report: Option<CleanReport>, dry_run: bool) -> Self {
        let report = report.unwrap_or(CleanReport {
            dry_run,
            ..CleanReport::default()
        });
        Self {
            builds_deleted: report.builds_deleted as f64,
            releases_deleted: report.releases_deleted as f64,
            bytes_freed: report.bytes_freed as f64,
            dry_run: report.dry_run,
            failures: report.failures,
        }
    }
}

// =============================================================================
// GC
// =============================================================================

/// What `ApvmCache.gc()` removed.
#[napi(object)]
#[derive(Debug, Clone, PartialEq)]
pub struct JsGcReport {
    /// Build records dropped: their directory is gone, or none of their
    /// files survived.
    pub stale_build_rows: f64,
    /// Release records dropped for the same reasons.
    pub stale_release_rows: f64,
    /// Directories without a record removed (including those of builds and
    /// releases dropped by this run).
    pub orphan_dirs_removed: f64,
    /// Bytes reclaimed from those directories.
    pub orphan_bytes_removed: f64,
    /// Stale temporary files swept.
    pub stale_temp_files_removed: f64,
    /// File records dropped as damaged: missing, wrong size, or — with
    /// `checksum` — wrong content. The next build caches them again.
    pub damaged_artifacts: f64,
    /// Bytes of damaged files deleted.
    pub damaged_bytes_removed: f64,
    /// What gc left in place, one line each (unreadable files, failed
    /// deletes, records outside the cache layout).
    pub failures: Vec<String>,
}

impl From<Option<GcReport>> for JsGcReport {
    /// `None` (no cache) is an all-zero report.
    fn from(report: Option<GcReport>) -> Self {
        let report = report.unwrap_or_default();
        Self {
            stale_build_rows: report.stale_build_rows as f64,
            stale_release_rows: report.stale_release_rows as f64,
            orphan_dirs_removed: report.orphan_dirs_removed as f64,
            orphan_bytes_removed: report.orphan_bytes_removed as f64,
            stale_temp_files_removed: report.stale_temp_files_removed as f64,
            damaged_artifacts: report.damaged_artifacts as f64,
            damaged_bytes_removed: report.damaged_bytes_removed as f64,
            failures: report.failures,
        }
    }
}

// =============================================================================
// Verify
// =============================================================================

/// One problem `ApvmCache.verify()` found — a flattened union: `kind` says
/// which context fields are set, `problem` which detail fields are.
///
/// | `kind` | set |
/// |---|---|
/// | `build` | `version`, `commit` |
/// | `release` | `tag` |
///
/// | `problem` | set |
/// |---|---|
/// | `missing` | — |
/// | `size_mismatch` | `expectedSize`, `actualSize` |
/// | `checksum_mismatch` | `expectedSha256`, `actualSha256` |
/// | `unreadable` | `details` — the file cannot be read; `gc()` keeps it |
/// | `invalid_record` | `details` — the record is corrupt; `gc()` removes it |
#[napi(object)]
#[derive(Debug, Clone, PartialEq)]
pub struct JsVerifyIssue {
    /// Project the file belongs to.
    pub project: String,
    /// Whether the file is a build artifact or a release asset.
    #[napi(ts_type = "'build' | 'release'")]
    pub kind: String,
    /// Build version (`kind: 'build'`).
    pub version: Option<String>,
    /// Build commit SHA (`kind: 'build'`).
    pub commit: Option<String>,
    /// Release tag (`kind: 'release'`).
    pub tag: Option<String>,
    /// The affected filename.
    pub filename: String,
    /// Absolute path of the affected file.
    pub path: String,
    /// What is wrong.
    #[napi(
        ts_type = "'missing' | 'size_mismatch' | 'checksum_mismatch' | 'unreadable' | 'invalid_record'"
    )]
    pub problem: String,
    /// Recorded size (`size_mismatch`).
    pub expected_size: Option<f64>,
    /// Size on disk (`size_mismatch`).
    pub actual_size: Option<f64>,
    /// Recorded SHA-256 (`checksum_mismatch`).
    pub expected_sha256: Option<String>,
    /// SHA-256 of the file on disk (`checksum_mismatch`).
    pub actual_sha256: Option<String>,
    /// What is wrong (`unreadable`, `invalid_record`).
    pub details: Option<String>,
}

impl From<&VerifyIssue> for JsVerifyIssue {
    fn from(issue: &VerifyIssue) -> Self {
        let mut js = Self {
            project: issue.project.clone(),
            kind: String::new(),
            version: None,
            commit: None,
            tag: None,
            filename: issue.filename.clone(),
            path: issue.path.to_string_lossy().into_owned(),
            problem: String::new(),
            expected_size: None,
            actual_size: None,
            expected_sha256: None,
            actual_sha256: None,
            details: None,
        };
        match &issue.context {
            IssueContext::Build { version, commit } => {
                js.kind = "build".to_string();
                js.version = Some(version.clone());
                js.commit = Some(commit.clone());
            }
            IssueContext::Release { tag } => {
                js.kind = "release".to_string();
                js.tag = Some(tag.clone());
            }
        }
        match &issue.problem {
            VerifyProblem::Missing => js.problem = "missing".to_string(),
            VerifyProblem::SizeMismatch { expected, actual } => {
                js.problem = "size_mismatch".to_string();
                js.expected_size = Some(*expected as f64);
                js.actual_size = Some(*actual as f64);
            }
            VerifyProblem::ChecksumMismatch { expected, actual } => {
                js.problem = "checksum_mismatch".to_string();
                js.expected_sha256 = Some(expected.clone());
                js.actual_sha256 = Some(actual.clone());
            }
            VerifyProblem::Unreadable { details } => {
                js.problem = "unreadable".to_string();
                js.details = Some(details.clone());
            }
            VerifyProblem::InvalidRecord { details } => {
                js.problem = "invalid_record".to_string();
                js.details = Some(details.clone());
            }
        }
        js
    }
}

// =============================================================================
// Repair
// =============================================================================

/// What `ApvmCache.repair()` did. All zero / absent / `false` = the cache
/// was healthy (or absent) and nothing was done.
#[napi(object)]
#[derive(Debug, Clone, PartialEq)]
pub struct JsRepairReport {
    /// Where a copy of the damaged database was kept, for inspection only —
    /// the database itself is reset (corrupt) or cleared (unreadable rows)
    /// in place. Absent when it was healthy, or missing / blank /
    /// half-repaired (see `rebuiltMissingDatabase`).
    pub quarantined_database: Option<String>,
    /// The database was missing, blank or left half-repaired, and the index
    /// was built again from the files on disk.
    pub rebuilt_missing_database: bool,
    /// Builds re-indexed from disk.
    pub builds_adopted: f64,
    /// Artifact files re-indexed (hashes recomputed).
    pub artifacts_adopted: f64,
    /// Directories or files skipped as not adoptable.
    pub entries_skipped: f64,
    /// Release directories left on disk without records (release metadata
    /// cannot be rebuilt from files); download again or `gc()` them.
    pub orphan_release_dirs: f64,
}

impl From<Option<RepairReport>> for JsRepairReport {
    /// `None` (no cache) is a nothing-done report.
    fn from(report: Option<RepairReport>) -> Self {
        let report = report.unwrap_or_default();
        Self {
            quarantined_database: report
                .quarantined_database
                .map(|p| p.to_string_lossy().into_owned()),
            rebuilt_missing_database: report.rebuilt_missing_database,
            builds_adopted: report.builds_adopted as f64,
            artifacts_adopted: report.artifacts_adopted as f64,
            entries_skipped: report.entries_skipped as f64,
            orphan_release_dirs: report.orphan_release_dirs as f64,
        }
    }
}

// =============================================================================
// Status — Apvm.cacheStatus()
// =============================================================================

/// Whether an `Apvm` instance's builds use the artifact cache right now.
///
/// | `state` | meaning | `reason` |
/// |---|---|---|
/// | `active` | builds read and write the cache | absent |
/// | `disabled` | `cacheEnabled: false` | absent |
/// | `corrupted` | the database needs `repair()`; builds run uncached | set |
/// | `unavailable` | the cache cannot be used (permissions, foreign data, newer schema); builds run uncached | set |
#[napi(object)]
#[derive(Debug, Clone, PartialEq)]
pub struct JsCacheStatus {
    /// The cache state.
    #[napi(ts_type = "'active' | 'disabled' | 'corrupted' | 'unavailable'")]
    pub state: String,
    /// What is wrong (`corrupted`, `unavailable`).
    pub reason: Option<String>,
}

impl From<&CacheStatus> for JsCacheStatus {
    fn from(status: &CacheStatus) -> Self {
        let (state, reason) = match status {
            CacheStatus::Active => ("active", None),
            CacheStatus::Disabled => ("disabled", None),
            CacheStatus::Corrupted { details } => ("corrupted", Some(details.clone())),
            CacheStatus::Unavailable { details } => ("unavailable", Some(details.clone())),
            // `CacheStatus` is non-exhaustive: a future state is not usable
            // as far as this binding knows.
            other => ("unavailable", Some(format!("{other:?}"))),
        };
        Self {
            state: state.to_string(),
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};

    use super::*;

    /// An issue in `context` with `problem`.
    fn issue(context: IssueContext, problem: VerifyProblem) -> VerifyIssue {
        VerifyIssue {
            project: "wp-rocket".to_string(),
            context,
            filename: "wp-rocket.zip".to_string(),
            path: PathBuf::from("/c/wp-rocket.zip"),
            problem,
        }
    }

    /// The context of an issue found in a build.
    fn build_context() -> IssueContext {
        IssueContext::Build {
            version: "3.17".to_string(),
            commit: "abc1234".to_string(),
        }
    }

    /// `rfc3339` as a UTC instant; a malformed literal fails the test.
    fn timestamp(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("a valid RFC 3339 test timestamp")
            .with_timezone(&Utc)
    }

    // ---- inputs ---------------------------------------------------------

    // ---- usage ----------------------------------------------------------

    #[test]
    fn no_cache_is_an_empty_usage_report_that_says_so() {
        let usage = JsCacheUsage::new(Path::new("/c"), None);
        assert_eq!(
            usage,
            JsCacheUsage {
                cache_dir: "/c".to_string(),
                exists: false,
                total_bytes: 0.0,
                builds_bytes: 0.0,
                releases_bytes: 0.0,
                build_count: 0.0,
                release_count: 0.0,
                file_count: 0.0,
                database_bytes: 0.0,
                oldest_build: None,
                newest_build: None,
                projects: vec![],
            }
        );
    }

    #[test]
    fn usage_report_converts_every_field() {
        let report = UsageReport {
            total_bytes: 30,
            builds_bytes: 10,
            releases_bytes: 20,
            build_count: 1,
            release_count: 2,
            file_count: 3,
            database_bytes: 4096,
            oldest_build: Some(timestamp("2024-01-02T03:04:05Z")),
            newest_build: Some(timestamp("2024-06-07T08:09:10Z")),
            projects: vec![ProjectUsage {
                project: "wp-rocket".to_string(),
                builds_bytes: 10,
                releases_bytes: 20,
                build_count: 1,
                release_count: 2,
            }],
        };
        let usage = JsCacheUsage::new(Path::new("/c"), Some(report));
        assert!(usage.exists);
        assert_eq!(
            (usage.total_bytes, usage.builds_bytes, usage.releases_bytes),
            (30.0, 10.0, 20.0)
        );
        assert_eq!(
            (usage.build_count, usage.release_count, usage.file_count),
            (1.0, 2.0, 3.0)
        );
        assert_eq!(usage.database_bytes, 4096.0);
        assert_eq!(
            usage.oldest_build.as_deref(),
            Some("2024-01-02T03:04:05+00:00")
        );
        assert_eq!(
            usage.newest_build.as_deref(),
            Some("2024-06-07T08:09:10+00:00")
        );
        assert_eq!(
            usage.projects,
            vec![JsProjectUsage {
                project: "wp-rocket".to_string(),
                build_count: 1.0,
                release_count: 2.0,
                builds_bytes: 10.0,
                releases_bytes: 20.0,
            }]
        );
    }

    #[test]
    fn byte_counts_stay_exact_up_to_two_to_the_53() {
        let report = UsageReport {
            total_bytes: 1 << 53,
            ..UsageReport::default()
        };
        let usage = JsCacheUsage::new(Path::new("/c"), Some(report));
        assert_eq!(usage.total_bytes, 9_007_199_254_740_992.0);
    }

    // ---- clean / gc / repair ---------------------------------------------

    #[test]
    fn clean_report_converts_every_field() {
        let report = CleanReport {
            builds_deleted: 1,
            releases_deleted: 2,
            bytes_freed: 3,
            dry_run: false,
            failures: vec!["x".to_string()],
        };
        assert_eq!(
            JsCleanReport::new(Some(report), false),
            JsCleanReport {
                builds_deleted: 1.0,
                releases_deleted: 2.0,
                bytes_freed: 3.0,
                dry_run: false,
                failures: vec!["x".to_string()],
            }
        );
    }

    #[test]
    fn no_cache_clean_report_echoes_the_dry_run_flag() {
        for dry_run in [false, true] {
            let js = JsCleanReport::new(None, dry_run);
            assert_eq!(js.dry_run, dry_run);
            assert_eq!(
                (js.builds_deleted, js.releases_deleted, js.bytes_freed),
                (0.0, 0.0, 0.0)
            );
            assert!(js.failures.is_empty());
        }
    }

    #[test]
    fn gc_report_converts_every_field_and_none_is_zero() {
        let report = GcReport {
            stale_build_rows: 1,
            stale_release_rows: 2,
            orphan_dirs_removed: 3,
            orphan_bytes_removed: 4,
            stale_temp_files_removed: 5,
            damaged_artifacts: 6,
            damaged_bytes_removed: 7,
            failures: vec!["kept".to_string()],
        };
        assert_eq!(
            JsGcReport::from(Some(report)),
            JsGcReport {
                stale_build_rows: 1.0,
                stale_release_rows: 2.0,
                orphan_dirs_removed: 3.0,
                orphan_bytes_removed: 4.0,
                stale_temp_files_removed: 5.0,
                damaged_artifacts: 6.0,
                damaged_bytes_removed: 7.0,
                failures: vec!["kept".to_string()],
            }
        );
        assert_eq!(
            JsGcReport::from(None),
            JsGcReport::from(Some(GcReport::default()))
        );
    }

    #[test]
    fn repair_report_converts_every_field_and_none_is_nothing_done() {
        let report = RepairReport {
            quarantined_database: Some(PathBuf::from("/c/apvm.db.corrupt-1")),
            rebuilt_missing_database: true,
            builds_adopted: 1,
            artifacts_adopted: 2,
            entries_skipped: 3,
            orphan_release_dirs: 4,
        };
        assert_eq!(
            JsRepairReport::from(Some(report)),
            JsRepairReport {
                quarantined_database: Some("/c/apvm.db.corrupt-1".to_string()),
                rebuilt_missing_database: true,
                builds_adopted: 1.0,
                artifacts_adopted: 2.0,
                entries_skipped: 3.0,
                orphan_release_dirs: 4.0,
            }
        );
        assert_eq!(
            JsRepairReport::from(None),
            JsRepairReport {
                quarantined_database: None,
                rebuilt_missing_database: false,
                builds_adopted: 0.0,
                artifacts_adopted: 0.0,
                entries_skipped: 0.0,
                orphan_release_dirs: 0.0,
            }
        );
    }

    // ---- verify ---------------------------------------------------------

    #[test]
    fn build_issue_sets_build_fields_only() {
        let js = JsVerifyIssue::from(&issue(build_context(), VerifyProblem::Missing));
        assert_eq!(
            js,
            JsVerifyIssue {
                project: "wp-rocket".to_string(),
                kind: "build".to_string(),
                version: Some("3.17".to_string()),
                commit: Some("abc1234".to_string()),
                tag: None,
                filename: "wp-rocket.zip".to_string(),
                path: "/c/wp-rocket.zip".to_string(),
                problem: "missing".to_string(),
                expected_size: None,
                actual_size: None,
                expected_sha256: None,
                actual_sha256: None,
                details: None,
            }
        );
    }

    #[test]
    fn release_issue_sets_the_tag_only() {
        let context = IssueContext::Release {
            tag: "v5.6.8".to_string(),
        };
        let js = JsVerifyIssue::from(&issue(context, VerifyProblem::Missing));
        assert_eq!(js.kind, "release");
        assert_eq!(js.tag.as_deref(), Some("v5.6.8"));
        assert!(js.version.is_none() && js.commit.is_none());
    }

    #[test]
    fn size_mismatch_sets_the_sizes_only() {
        let problem = VerifyProblem::SizeMismatch {
            expected: 10,
            actual: 7,
        };
        let js = JsVerifyIssue::from(&issue(build_context(), problem));
        assert_eq!(js.problem, "size_mismatch");
        assert_eq!((js.expected_size, js.actual_size), (Some(10.0), Some(7.0)));
        assert!(js.expected_sha256.is_none() && js.actual_sha256.is_none());
        assert!(js.details.is_none());
    }

    #[test]
    fn checksum_mismatch_sets_the_hashes_only() {
        let problem = VerifyProblem::ChecksumMismatch {
            expected: "aa".to_string(),
            actual: "bb".to_string(),
        };
        let js = JsVerifyIssue::from(&issue(build_context(), problem));
        assert_eq!(js.problem, "checksum_mismatch");
        assert_eq!(js.expected_sha256.as_deref(), Some("aa"));
        assert_eq!(js.actual_sha256.as_deref(), Some("bb"));
        assert!(js.expected_size.is_none() && js.actual_size.is_none());
        assert!(js.details.is_none());
    }

    #[test]
    fn invalid_record_sets_the_details_only() {
        let problem = VerifyProblem::InvalidRecord {
            details: "negative size".to_string(),
        };
        let js = JsVerifyIssue::from(&issue(build_context(), problem));
        assert_eq!(js.problem, "invalid_record");
        assert_eq!(js.details.as_deref(), Some("negative size"));
        assert!(js.expected_size.is_none() && js.expected_sha256.is_none());
    }

    #[test]
    fn unreadable_sets_the_details_only() {
        let problem = VerifyProblem::Unreadable {
            details: "Permission denied".to_string(),
        };
        let js = JsVerifyIssue::from(&issue(build_context(), problem));
        assert_eq!(js.problem, "unreadable");
        assert_eq!(js.details.as_deref(), Some("Permission denied"));
        assert!(js.expected_size.is_none() && js.expected_sha256.is_none());
    }

    // ---- status ---------------------------------------------------------

    #[test]
    fn every_cache_status_maps_to_its_state() {
        let cases = [
            (CacheStatus::Active, "active", None),
            (CacheStatus::Disabled, "disabled", None),
            (
                CacheStatus::Corrupted {
                    details: "bad".to_string(),
                },
                "corrupted",
                Some("bad"),
            ),
            (
                CacheStatus::Unavailable {
                    details: "denied".to_string(),
                },
                "unavailable",
                Some("denied"),
            ),
        ];
        for (status, state, reason) in cases {
            let js = JsCacheStatus::from(&status);
            assert_eq!(js.state, state, "{status:?}");
            assert_eq!(js.reason.as_deref(), reason, "{status:?}");
        }
    }
}
