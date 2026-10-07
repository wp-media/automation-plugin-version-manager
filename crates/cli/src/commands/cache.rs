//! Cache maintenance command: `apvm cache <action>`.
//!
//! Presentation only. Every decision — input validation, what counts as a
//! cache, the `--older-than` grammar, store access — lives in
//! [`apvm_core::maintenance::CacheMaintenance`], built to be shared by every
//! front end. This module maps flags to requests, renders reports, prompts
//! before `clear`, adds the CLI's next-step hints to errors, and turns
//! verification issues into a non-zero exit.
//!
//! All I/O goes through a [`Console`] (report → stdout, diagnostics and
//! prompts → stderr, confirmation), and every report is rendered by a pure
//! function, so each path is unit-testable.
//!
//! These operate directly on the configured cache directory and are
//! **independent of the `cache` on/off setting** — you can inspect or clean a
//! cache you've turned off. Unlike the build path (where cache failures are
//! swallowed so a build still succeeds), errors here surface normally: the
//! user explicitly asked for the operation.

use std::io::{self, BufRead, Write};
use std::path::Path;

use clap::{Args, Subcommand};

use apvm_core::error::Error;
use apvm_core::maintenance::{
    CacheMaintenance, CleanReport, CleanRequest, CleanTarget, GcReport, ProjectUsage, RepairReport,
    UsageReport, VerifyIssue, VerifyMode, VerifyProblem,
};

/// Arguments for the `cache` command.
#[derive(Args, Debug)]
pub struct CacheArgs {
    /// Cache maintenance action.
    #[command(subcommand)]
    pub action: CacheAction,
}

/// Cache maintenance subcommands.
#[derive(Subcommand, Debug)]
pub enum CacheAction {
    /// Show cache usage: totals and a per-project breakdown
    Info,
    /// Remove cached entries (by age, project, or kind)
    Clean(CleanArgs),
    /// Reconcile the database with disk and sweep leftover temp files
    Gc,
    /// Check that cached files are intact (add --checksum to re-hash them)
    Verify {
        /// Re-hash every file and compare to its recorded checksum (slowest,
        /// strongest check). Without this, only presence and size are checked.
        #[arg(long)]
        checksum: bool,
    },
    /// Recover a corrupt cache database (quarantine it, rebuild the index)
    Repair,
    /// Remove everything from the cache
    Clear {
        /// Skip the confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}

/// Filters for `apvm cache clean`.
#[derive(Args, Debug)]
pub struct CleanArgs {
    /// Only remove entries not used within this window, e.g. `30d`, `12h`,
    /// `2w`, `45m`. Recently-used entries are kept (last-use is refreshed on
    /// every cache hit).
    #[arg(long, value_name = "DURATION")]
    pub older_than: Option<String>,

    /// Restrict the clean to one project
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Report what would be removed without deleting anything
    #[arg(long)]
    pub dry_run: bool,

    /// Only remove cached builds (leave release downloads)
    #[arg(long, conflicts_with = "releases")]
    pub builds: bool,

    /// Only remove cached release downloads (leave builds)
    #[arg(long)]
    pub releases: bool,
}

impl CleanArgs {
    /// The core request these flags describe. `--builds` and `--releases`
    /// are mutually exclusive (clap enforces it); neither means both kinds.
    fn to_request(&self) -> CleanRequest {
        let target = if self.builds {
            CleanTarget::Builds
        } else if self.releases {
            CleanTarget::Releases
        } else {
            CleanTarget::All
        };
        CleanRequest::default()
            .older_than(self.older_than.clone())
            .project(self.project.clone())
            .target(target)
            .dry_run(self.dry_run)
    }
}

impl CacheArgs {
    /// Execute the cache command against `cache_dir` (the resolved cache
    /// directory — config override or default) on the real terminal.
    pub fn execute(&self, cache_dir: &Path) -> apvm_core::Result<()> {
        let mut out = |text: &str| print!("{text}");
        let mut err = |text: &str| eprint!("{text}");
        let mut ask = confirm;
        let mut console = Console {
            out: &mut out,
            err: &mut err,
            ask: &mut ask,
        };
        self.run(&CacheMaintenance::new(cache_dir), &mut console)
    }

    /// Run the action against `cache`, doing all I/O through `console`.
    fn run(&self, cache: &CacheMaintenance, console: &mut Console<'_>) -> apvm_core::Result<()> {
        match &self.action {
            CacheAction::Info => info(cache, console),
            CacheAction::Clean(args) => clean(cache, args, console),
            CacheAction::Gc => gc(cache, console),
            CacheAction::Verify { checksum } => verify(cache, *checksum, console),
            CacheAction::Repair => repair(cache, console),
            CacheAction::Clear { yes } => clear(cache, *yes, console),
        }
    }
}

/// Where an action's I/O goes: the report (stdout), diagnostics (stderr),
/// and yes/no confirmation. Injected so tests can capture and script it.
struct Console<'a> {
    /// Receives report text.
    out: &'a mut dyn FnMut(&str),
    /// Receives diagnostic text.
    err: &'a mut dyn FnMut(&str),
    /// Asks the user to confirm `prompt`; `true` means go ahead.
    ask: &'a mut dyn FnMut(&str) -> apvm_core::Result<bool>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Actions — each maps a core result to output. `None` from the core means
// there is no cache: report that instead of an empty report.
// ─────────────────────────────────────────────────────────────────────────────

/// `apvm cache info` — usage totals and per-project breakdown.
fn info(cache: &CacheMaintenance, console: &mut Console<'_>) -> apvm_core::Result<()> {
    let Some(usage) = cache.usage().map_err(hint_with_repair)? else {
        return report_empty(cache, console);
    };
    (console.out)(&usage_text(cache.dir(), &usage));
    Ok(())
}

/// `apvm cache clean` — delete entries by age / project / kind.
fn clean(
    cache: &CacheMaintenance,
    args: &CleanArgs,
    console: &mut Console<'_>,
) -> apvm_core::Result<()> {
    let Some(report) = cache.clean(&args.to_request()).map_err(hint_with_repair)? else {
        return report_empty(cache, console);
    };
    (console.out)(&clean_text(&report));
    (console.err)(&failures_text(
        &report.failures,
        "orphaned until `apvm cache gc`",
    ));
    Ok(())
}

/// `apvm cache gc` — reconcile the database with disk.
fn gc(cache: &CacheMaintenance, console: &mut Console<'_>) -> apvm_core::Result<()> {
    let Some(report) = cache.gc().map_err(hint_with_repair)? else {
        return report_empty(cache, console);
    };
    (console.out)(&gc_text(&report));
    Ok(())
}

/// `apvm cache verify` — report integrity problems.
///
/// Exits non-zero (returns an error) when issues are found, so scripts and
/// CI can gate on cache health.
fn verify(
    cache: &CacheMaintenance,
    checksum: bool,
    console: &mut Console<'_>,
) -> apvm_core::Result<()> {
    let Some(issues) = cache
        .verify(verify_mode(checksum))
        .map_err(hint_with_repair)?
    else {
        return report_empty(cache, console);
    };
    if issues.is_empty() {
        let depth = if checksum { "checksum" } else { "size" };
        (console.out)(&format!(
            "Cache verified ({depth} check): no issues found.\n"
        ));
        return Ok(());
    }
    (console.err)(&issues_text(&issues));
    Err(Error::Cache(format!(
        "verification found {} issue(s)",
        issues.len()
    )))
}

/// `apvm cache repair` — recover the cache. Never offers itself as the
/// remedy for its own errors.
fn repair(cache: &CacheMaintenance, console: &mut Console<'_>) -> apvm_core::Result<()> {
    let Some(report) = cache.repair().map_err(hint_without_repair)? else {
        return report_empty(cache, console);
    };
    (console.out)(&repair_text(&report));
    Ok(())
}

/// `apvm cache clear` — remove everything, after confirmation unless `yes`.
/// The cache is opened (and checked) before asking: no cache is reported,
/// and a broken one fails, without ever prompting.
fn clear(cache: &CacheMaintenance, yes: bool, console: &mut Console<'_>) -> apvm_core::Result<()> {
    if cache.usage().map_err(hint_with_repair)?.is_none() {
        return report_empty(cache, console);
    }
    let prompt = format!(
        "Remove ALL cached artifacts in {}? [y/N] ",
        cache.dir().display()
    );
    if !yes && !(console.ask)(&prompt)? {
        (console.out)("Aborted. Nothing was removed.\n");
        return Ok(());
    }
    let Some(report) = cache.clear().map_err(hint_with_repair)? else {
        return report_empty(cache, console);
    };
    (console.out)(&cleared_text(&report));
    (console.err)(&failures_text(&report.failures, "run `apvm cache gc`"));
    Ok(())
}

/// Print the no-cache note.
fn report_empty(cache: &CacheMaintenance, console: &mut Console<'_>) -> apvm_core::Result<()> {
    (console.out)(&empty_text(cache.dir()));
    Ok(())
}

/// `--checksum` → the verification depth.
fn verify_mode(checksum: bool) -> VerifyMode {
    if checksum {
        VerifyMode::Checksum
    } else {
        VerifyMode::Size
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Rendering — pure: report in, exact terminal text out (each line newline-
// terminated, as `println!` would).
// ─────────────────────────────────────────────────────────────────────────────

/// Terminate each line with a newline and join them.
fn lines_text(lines: &[String]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

/// The note shown instead of a report when there is no cache.
fn empty_text(dir: &Path) -> String {
    lines_text(&[
        "Cache is empty — nothing has been cached yet.".to_string(),
        format!("  Location: {}", dir.display()),
    ])
}

/// Totals, then the per-project breakdown.
fn usage_text(dir: &Path, usage: &UsageReport) -> String {
    let mut lines = vec![
        format!("Artifact cache: {}", dir.display()),
        format!(
            "  Builds:   {:>4}  ({})",
            usage.build_count,
            human_bytes(usage.builds_bytes)
        ),
        format!(
            "  Releases: {:>4}  ({})",
            usage.release_count,
            human_bytes(usage.releases_bytes)
        ),
        format!(
            "  Files:    {:>4}   Database: {}",
            usage.file_count,
            human_bytes(usage.database_bytes)
        ),
        format!("  Total artifacts: {}", human_bytes(usage.total_bytes)),
        String::new(),
    ];
    lines.extend(project_lines(&usage.projects));
    lines_text(&lines)
}

/// The per-project breakdown, or a note when there is none.
fn project_lines(projects: &[ProjectUsage]) -> Vec<String> {
    if projects.is_empty() {
        return vec!["  (cache is empty)".to_string()];
    }
    let mut lines = vec!["  By project:".to_string()];
    lines.extend(projects.iter().map(|project| {
        format!(
            "    {:<16} {} builds, {} releases  ({})",
            project.project,
            project.build_count,
            project.release_count,
            human_bytes(project.builds_bytes.saturating_add(project.releases_bytes)),
        )
    }));
    lines
}

/// What a clean removed (or, dry run, would remove).
fn clean_text(report: &CleanReport) -> String {
    let verb = if report.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    let mut lines = vec![format!(
        "{verb} {} build(s) and {} release(s), freeing {}.",
        report.builds_deleted,
        report.releases_deleted,
        human_bytes(report.bytes_freed),
    )];
    if report.dry_run {
        lines.push("  (dry run — nothing was deleted)".to_string());
    }
    lines_text(&lines)
}

/// What a clear removed.
fn cleared_text(report: &CleanReport) -> String {
    lines_text(&[format!(
        "Cleared the cache: removed {} build(s) and {} release(s), freeing {}.",
        report.builds_deleted,
        report.releases_deleted,
        human_bytes(report.bytes_freed),
    )])
}

/// Directories a clean/clear could not remove (their records are already
/// gone), with the `remedy` the caller suggests; empty when there are none.
fn failures_text(failures: &[String], remedy: &str) -> String {
    if failures.is_empty() {
        return String::new();
    }
    let mut lines = vec![format!(
        "  {} director(ies) could not be removed ({remedy}):",
        failures.len()
    )];
    lines.extend(failures.iter().map(|failure| format!("    - {failure}")));
    lines_text(&lines)
}

/// What garbage collection dropped, removed, and swept.
fn gc_text(report: &GcReport) -> String {
    lines_text(&[
        "Cache garbage collection complete:".to_string(),
        format!(
            "  Dropped stale records: {} build(s), {} release(s)",
            report.stale_build_rows, report.stale_release_rows
        ),
        format!(
            "  Removed {} orphan director(ies) ({})",
            report.orphan_dirs_removed,
            human_bytes(report.orphan_bytes_removed)
        ),
        format!(
            "  Swept {} stale temp file(s)",
            report.stale_temp_files_removed
        ),
    ])
}

/// One line per verification issue, then the remedy hint.
fn issues_text(issues: &[VerifyIssue]) -> String {
    let mut lines = vec![format!("Found {} cache issue(s):", issues.len())];
    lines.extend(issues.iter().map(|issue| {
        format!(
            "  - [{}] {}: {}",
            issue.project,
            issue.filename,
            problem_text(&issue.problem)
        )
    }));
    lines.push(String::new());
    lines.push(
        "Run `apvm cache gc` to drop records for missing files, or rebuild to heal them."
            .to_string(),
    );
    lines_text(&lines)
}

/// A verification problem in words.
fn problem_text(problem: &VerifyProblem) -> String {
    match problem {
        VerifyProblem::Missing => "missing".to_string(),
        VerifyProblem::SizeMismatch { expected, actual } => {
            format!("size mismatch (recorded {expected}, on disk {actual})")
        }
        VerifyProblem::ChecksumMismatch { .. } => "checksum mismatch".to_string(),
        VerifyProblem::Unreadable { details } => format!("unreadable: {details}"),
    }
}

/// What a repair recovered — or that there was nothing to repair.
fn repair_text(report: &RepairReport) -> String {
    let mut lines = match (
        &report.quarantined_database,
        report.rebuilt_missing_database,
    ) {
        (Some(path), _) => vec![
            "Recovered a corrupt cache database.".to_string(),
            format!("  Quarantined the damaged file at: {}", path.display()),
        ],
        (None, true) => vec!["Rebuilt the missing cache database.".to_string()],
        (None, false) => {
            return lines_text(&["Cache database is healthy — nothing to repair.".to_string()]);
        }
    };
    lines.push(format!(
        "  Re-indexed {} build(s) / {} artifact(s) from disk.",
        report.builds_adopted, report.artifacts_adopted
    ));
    if report.entries_skipped > 0 {
        lines.push(format!(
            "  Skipped {} unadoptable entr(ies).",
            report.entries_skipped
        ));
    }
    if report.orphan_release_dirs > 0 {
        lines.push(format!(
            "  {} cached release(s) could not be re-indexed (re-download or `apvm cache gc`).",
            report.orphan_release_dirs
        ));
    }
    lines_text(&lines)
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// [`with_hint`] for every action except `repair`.
fn hint_with_repair(err: Error) -> Error {
    with_hint(err, true)
}

/// [`with_hint`] for `repair`, which must not offer itself as the remedy.
fn hint_without_repair(err: Error) -> Error {
    with_hint(err, false)
}

/// Attach the CLI's next step to errors that have one: `apvm cache repair`
/// for a corrupt, unreadable or lost database (when `offer_repair`), and
/// where the cache location comes from for someone else's directory. Every
/// other error passes through unchanged.
fn with_hint(err: Error, offer_repair: bool) -> Error {
    use apvm_storage::Error as Storage;
    let hint = match &err {
        Error::Storage(Storage::DatabaseCorrupted { .. } | Storage::Data { .. })
            if offer_repair =>
        {
            "The cache database is corrupt — run `apvm cache repair` to recover it."
        }
        Error::Storage(Storage::MissingDatabase { .. }) if offer_repair => {
            "The cache database is missing — run `apvm cache repair` to re-index the cache from disk."
        }
        Error::Storage(Storage::ForeignDirectory { .. }) => {
            "Check the cache location: APVM_CACHE_DIR if set, else config `cache-dir`, else ~/.apvm/cache."
        }
        _ => return err,
    };
    match err {
        Error::Storage(inner) => Error::Cache(format!("{inner}\n  {hint}")),
        other => other,
    }
}

/// Prompt on stderr for a yes/no answer; `true` only for `y`/`yes`.
fn confirm(prompt: &str) -> apvm_core::Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush().map_err(|e| {
        Error::Io(io::Error::new(
            e.kind(),
            format!("failed to flush stderr: {e}"),
        ))
    })?;
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input).map_err(|e| {
        Error::Io(io::Error::new(
            e.kind(),
            format!("failed to read input: {e}"),
        ))
    })?;
    Ok(is_yes(&input))
}

/// Whether a typed answer means yes: `y` or `yes`, any case, surrounding
/// whitespace ignored. Anything else — including nothing — is no.
fn is_yes(answer: &str) -> bool {
    let answer = answer.trim().to_ascii_lowercase();
    answer == "y" || answer == "yes"
}

/// Format a byte count with a binary (KiB/MiB/GiB) unit and one decimal.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    // ---- execute() integration (real temp store, no network) --------------

    use apvm_storage::{ArtifactStore, BuildMetadata, BuildSource, SourceArtifact};

    fn clean_args() -> CleanArgs {
        CleanArgs {
            older_than: None,
            project: None,
            dry_run: false,
            builds: false,
            releases: false,
        }
    }

    /// Seed a build with one artifact into a fresh cache directory.
    fn seed(cache: &Path) {
        let store = ArtifactStore::open(cache).unwrap();
        let src = tempfile::tempdir().unwrap();
        let zip = src.path().join("wp-rocket-3.17.4.zip");
        std::fs::write(&zip, b"artifact-bytes").unwrap();
        store
            .store(
                &BuildMetadata::new(
                    "wp-rocket",
                    "3.17.4",
                    BuildSource::PullRequest(8556),
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "develop".to_string(),
                ),
                &[SourceArtifact {
                    variant_id: None,
                    path: zip,
                    target_name: "wp-rocket-3.17.4.zip".to_string(),
                }],
            )
            .unwrap();
    }

    #[test]
    fn empty_cache_guard_does_not_create() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("never-created");
        let args = CacheArgs {
            action: CacheAction::Info,
        };
        assert!(args.execute(&cache).is_ok());
        assert!(
            !cache.exists(),
            "an inspection command must not create the cache dir"
        );
    }

    #[test]
    fn read_only_actions_run_on_a_seeded_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);

        for action in [
            CacheAction::Info,
            CacheAction::Verify { checksum: true },
            CacheAction::Gc,
            CacheAction::Repair,
        ] {
            assert!(
                CacheArgs { action }.execute(&cache).is_ok(),
                "action should succeed on a healthy cache"
            );
        }

        // A dry-run clean reports without deleting: the build survives.
        let mut dry = clean_args();
        dry.dry_run = true;
        assert!(
            CacheArgs {
                action: CacheAction::Clean(dry),
            }
            .execute(&cache)
            .is_ok()
        );
        assert_eq!(
            ArtifactStore::open(&cache)
                .unwrap()
                .usage()
                .unwrap()
                .build_count,
            1
        );
    }

    #[test]
    fn clear_empties_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);
        assert_eq!(
            ArtifactStore::open(&cache)
                .unwrap()
                .usage()
                .unwrap()
                .build_count,
            1
        );

        CacheArgs {
            action: CacheAction::Clear { yes: true },
        }
        .execute(&cache)
        .unwrap();

        assert_eq!(
            ArtifactStore::open(&cache)
                .unwrap()
                .usage()
                .unwrap()
                .build_count,
            0
        );
    }

    #[test]
    fn verify_errors_when_issues_found() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);

        // Damage the cache: remove the artifact file behind the record.
        let build = ArtifactStore::open(&cache)
            .unwrap()
            .find_by_commit("wp-rocket", "3.17.4", "aaaaaaa")
            .unwrap()
            .expect("seeded build");
        std::fs::remove_file(&build.artifacts[0].path).unwrap();

        let result = CacheArgs {
            action: CacheAction::Verify { checksum: false },
        }
        .execute(&cache);
        let err = result.expect_err("verify must fail when issues are found");
        assert!(
            err.to_string().contains("issue"),
            "error should report the issue count: {err}"
        );
    }

    #[test]
    fn corrupt_database_reports_repair_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        // A file that is definitely not a SQLite database.
        std::fs::write(cache.join("apvm.db"), b"not a sqlite database").unwrap();

        let result = CacheArgs {
            action: CacheAction::Info,
        }
        .execute(&cache);
        let err = result.expect_err("a corrupt database must surface an error");
        assert!(
            err.to_string().contains("apvm cache repair"),
            "error should point at the repair command: {err}"
        );
    }

    #[test]
    fn clean_builds_only_keeps_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);
        // Seed a release too.
        {
            let store = ArtifactStore::open(&cache).unwrap();
            let src = tempfile::tempdir().unwrap();
            let asset = src.path().join("wp-rocket-3.17.4.zip");
            std::fs::write(&asset, b"release-bytes").unwrap();
            store
                .store_release(
                    &apvm_storage::ReleaseMetadata::new("wp-rocket", "v3.17.4"),
                    &[SourceArtifact {
                        variant_id: None,
                        path: asset,
                        target_name: "wp-rocket-3.17.4.zip".to_string(),
                    }],
                )
                .unwrap();
        }

        let mut args = clean_args();
        args.builds = true; // builds only
        CacheArgs {
            action: CacheAction::Clean(args),
        }
        .execute(&cache)
        .unwrap();

        let usage = ArtifactStore::open(&cache).unwrap().usage().unwrap();
        assert_eq!(usage.build_count, 0, "builds removed");
        assert_eq!(usage.release_count, 1, "releases kept");
    }

    // ---- captured console: exact output, streams and prompts --------------

    /// What one captured run produced.
    struct Run {
        result: apvm_core::Result<()>,
        out: String,
        err: String,
        prompts: Vec<String>,
    }

    /// Run `action` on `cache` with captured streams; the prompt answers
    /// from `answers` in order (then "no").
    fn run(action: CacheAction, cache: &Path, answers: &[bool]) -> Run {
        let (mut out, mut err, mut prompts) = (String::new(), String::new(), Vec::new());
        let mut answers = answers.iter().copied();
        let result = {
            let mut to_out = |text: &str| out.push_str(text);
            let mut to_err = |text: &str| err.push_str(text);
            let mut ask = |prompt: &str| {
                prompts.push(prompt.to_string());
                Ok(answers.next().unwrap_or(false))
            };
            let mut console = Console {
                out: &mut to_out,
                err: &mut to_err,
                ask: &mut ask,
            };
            CacheArgs { action }.run(&CacheMaintenance::new(cache), &mut console)
        };
        Run {
            result,
            out,
            err,
            prompts,
        }
    }

    /// Every action, with the flags that matter for the no-cache paths.
    fn all_actions() -> Vec<CacheAction> {
        vec![
            CacheAction::Info,
            CacheAction::Clean(clean_args()),
            CacheAction::Gc,
            CacheAction::Verify { checksum: false },
            CacheAction::Verify { checksum: true },
            CacheAction::Repair,
            CacheAction::Clear { yes: false },
            CacheAction::Clear { yes: true },
        ]
    }

    /// Store an extra build (`project`) and a release so scoped cleans have
    /// something to discriminate.
    fn seed_more(cache: &Path, project: &str) {
        let store = ArtifactStore::open(cache).unwrap();
        let src = tempfile::tempdir().unwrap();
        let zip = src.path().join(format!("{project}.zip"));
        std::fs::write(&zip, b"more-bytes").unwrap();
        let artifact = SourceArtifact {
            variant_id: None,
            path: zip,
            target_name: format!("{project}.zip"),
        };
        store
            .store(
                &BuildMetadata::new(
                    project,
                    "1.0.0",
                    BuildSource::PullRequest(1),
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "develop".to_string(),
                ),
                std::slice::from_ref(&artifact),
            )
            .unwrap();
        store
            .store_release(
                &apvm_storage::ReleaseMetadata::new(project, "v1.0.0"),
                &[artifact],
            )
            .unwrap();
    }

    #[test]
    fn every_action_without_a_cache_prints_the_note_and_never_prompts() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("never-created");
        let note = format!(
            "Cache is empty — nothing has been cached yet.\n  Location: {}\n",
            cache.display()
        );
        for action in all_actions() {
            let label = format!("{action:?}");
            let got = run(action, &cache, &[]);
            assert!(got.result.is_ok(), "{label}: {:?}", got.result);
            assert_eq!(got.out, note, "{label}");
            assert_eq!(got.err, "", "{label}");
            assert!(got.prompts.is_empty(), "{label} must not prompt");
        }
        assert!(!cache.exists());
    }

    #[test]
    fn clean_flags_map_to_the_core_request() {
        let mut args = clean_args();
        assert_eq!(args.to_request(), CleanRequest::default());

        args.older_than = Some("30d".to_string());
        args.project = Some("backwpup".to_string());
        args.dry_run = true;
        args.builds = true;
        let request = args.to_request();
        assert_eq!(request.older_than.as_deref(), Some("30d"));
        assert_eq!(request.project.as_deref(), Some("backwpup"));
        assert!(request.dry_run);
        assert_eq!(request.target, CleanTarget::Builds);

        args.builds = false;
        args.releases = true;
        assert_eq!(args.to_request().target, CleanTarget::Releases);
    }

    #[test]
    fn clean_by_project_removes_only_that_project() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);
        seed_more(&cache, "backwpup");

        let mut args = clean_args();
        args.project = Some("backwpup".to_string());
        let got = run(CacheAction::Clean(args), &cache, &[]);
        assert!(got.result.is_ok(), "{:?}", got.result);
        assert!(
            got.out.starts_with("Removed 1 build(s) and 1 release(s)"),
            "{}",
            got.out
        );
        let usage = ArtifactStore::open(&cache).unwrap().usage().unwrap();
        assert_eq!((usage.build_count, usage.release_count), (1, 0));
        assert_eq!(usage.projects[0].project, "wp-rocket");
    }

    #[test]
    fn clean_rejects_invalid_input_even_without_a_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("never-created");

        let mut bad_age = clean_args();
        bad_age.older_than = Some("30y".to_string());
        let mut bad_project = clean_args();
        bad_project.project = Some("Bad/Name".to_string());

        for (args, expected) in [
            (bad_age, "unknown time unit"),
            (bad_project, "invalid project"),
        ] {
            let err = CacheArgs {
                action: CacheAction::Clean(args),
            }
            .execute(&cache)
            .expect_err("invalid input must fail whether or not a cache exists");
            let message = err.to_string();
            assert!(message.contains(expected), "{message}");
            assert!(
                !message.contains("apvm cache repair"),
                "bad input is not a repair case: {message}"
            );
        }
        assert!(!cache.exists(), "validation must not create the cache dir");
    }

    #[test]
    fn clear_asks_first_and_respects_the_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);
        let prompt = format!("Remove ALL cached artifacts in {}? [y/N] ", cache.display());

        let declined = run(CacheAction::Clear { yes: false }, &cache, &[false]);
        assert!(declined.result.is_ok());
        assert_eq!(declined.prompts, std::slice::from_ref(&prompt));
        assert_eq!(declined.out, "Aborted. Nothing was removed.\n");
        let builds = |cache: &Path| {
            ArtifactStore::open(cache)
                .unwrap()
                .usage()
                .unwrap()
                .build_count
        };
        assert_eq!(builds(&cache), 1, "a declined clear must delete nothing");

        let accepted = run(CacheAction::Clear { yes: false }, &cache, &[true]);
        assert!(accepted.result.is_ok());
        assert_eq!(accepted.prompts, [prompt]);
        assert!(
            accepted
                .out
                .starts_with("Cleared the cache: removed 1 build(s)")
        );
        assert_eq!(builds(&cache), 0);

        seed(&cache);
        let forced = run(CacheAction::Clear { yes: true }, &cache, &[]);
        assert!(forced.result.is_ok());
        assert!(forced.prompts.is_empty(), "-y must not prompt");
        assert_eq!(builds(&cache), 0);
    }

    #[test]
    fn answers_mean_yes_only_for_y_or_yes() {
        for yes in ["y", "Y", "yes", "YES", " yes \n", "y\n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "ye", "y es", "1"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }

    #[test]
    fn checksum_flag_selects_the_checksum_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);
        // Same-size corruption: only a checksum pass can see it.
        let path = ArtifactStore::open(&cache)
            .unwrap()
            .find_by_commit("wp-rocket", "3.17.4", "aaaaaaa")
            .unwrap()
            .expect("seeded build")
            .artifacts[0]
            .path
            .clone();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let size = run(CacheAction::Verify { checksum: false }, &cache, &[]);
        assert!(size.result.is_ok(), "{:?}", size.result);
        assert_eq!(size.out, "Cache verified (size check): no issues found.\n");

        let deep = run(CacheAction::Verify { checksum: true }, &cache, &[]);
        assert!(deep.result.is_err());
        assert_eq!(deep.out, "", "issues go to stderr");
        assert!(
            deep.err.starts_with(
                "Found 1 cache issue(s):\n  - [wp-rocket] wp-rocket-3.17.4.zip: checksum mismatch\n"
            ),
            "{}",
            deep.err
        );
    }

    #[test]
    fn every_action_but_repair_hints_at_repair_on_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("apvm.db"), b"not a sqlite database").unwrap();

        for action in all_actions() {
            if matches!(action, CacheAction::Repair) {
                continue;
            }
            let label = format!("{action:?}");
            let got = run(action, &cache, &[true]);
            let err = got.result.expect_err("a corrupt database must surface");
            assert!(
                err.to_string().contains("apvm cache repair"),
                "{label}: {err}"
            );
            assert!(
                got.prompts.is_empty(),
                "{label}: no prompt for a broken cache"
            );
        }

        // `repair` is the remedy: it recovers instead of erroring, and the
        // cache is usable again afterwards.
        let repaired = run(CacheAction::Repair, &cache, &[]);
        assert!(repaired.result.is_ok());
        assert!(
            repaired
                .out
                .starts_with("Recovered a corrupt cache database.\n")
        );
        assert!(run(CacheAction::Info, &cache, &[]).result.is_ok());
    }

    #[test]
    fn hints_name_the_next_step_only_where_there_is_one() {
        use apvm_storage::Error as Storage;
        let path = std::path::PathBuf::from("/c");
        let corrupt = || {
            Error::Storage(Storage::DatabaseCorrupted {
                path: path.clone(),
                details: "bad".to_string(),
            })
        };
        let data = || {
            Error::Storage(Storage::Data {
                details: "bad row".to_string(),
            })
        };
        let missing = || {
            Error::Storage(Storage::MissingDatabase {
                path: path.clone(),
                adoptable_builds: 2,
            })
        };
        let foreign = || {
            Error::Storage(Storage::ForeignDirectory {
                path: path.clone(),
                entry: "tools".to_string(),
            })
        };

        for err in [corrupt(), data()] {
            let text = hint_with_repair(err).to_string();
            assert!(text.contains("corrupt — run `apvm cache repair`"), "{text}");
        }
        let text = hint_with_repair(missing()).to_string();
        assert!(text.contains("missing — run `apvm cache repair`"), "{text}");
        for err in [hint_with_repair(foreign()), hint_without_repair(foreign())] {
            assert!(
                err.to_string().contains("Check the cache location"),
                "{err}"
            );
        }
        // Repair never offers itself; unrelated errors pass through as-is.
        for err in [corrupt(), data(), missing()] {
            assert!(
                !hint_without_repair(err)
                    .to_string()
                    .contains("apvm cache repair")
            );
        }
        let unrelated = [
            Error::Config("bad".to_string()),
            Error::Io(io::Error::other("disk")),
            Error::Storage(Storage::InvalidInput {
                what: "project",
                value: "X".to_string(),
                reason: "bad".to_string(),
            }),
        ];
        for err in unrelated {
            let before = err.to_string();
            assert_eq!(hint_with_repair(err).to_string(), before);
        }
    }

    #[test]
    fn a_lost_database_is_reported_and_repair_rebuilds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        seed(&cache);
        for name in ["apvm.db", "apvm.db-wal", "apvm.db-shm"] {
            let _ = std::fs::remove_file(cache.join(name));
        }

        let info = run(CacheAction::Info, &cache, &[]);
        let err = info
            .result
            .expect_err("a lost database is not an empty cache");
        assert!(
            err.to_string()
                .contains("missing — run `apvm cache repair`"),
            "{err}"
        );

        let repaired = run(CacheAction::Repair, &cache, &[]);
        assert!(repaired.result.is_ok(), "{:?}", repaired.result);
        assert_eq!(
            repaired.out,
            "Rebuilt the missing cache database.\n  Re-indexed 1 build(s) / 1 artifact(s) from disk.\n"
        );
    }

    #[test]
    fn a_foreign_directory_is_refused_with_a_location_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("home");
        let precious = dir.join("my-plugin/releases/1.0/important.txt");
        std::fs::create_dir_all(precious.parent().unwrap()).unwrap();
        std::fs::write(&precious, b"user data").unwrap();

        for action in all_actions() {
            let label = format!("{action:?}");
            let got = run(action, &dir, &[true]);
            let err = got.result.expect_err("someone else's directory");
            assert!(
                err.to_string().contains("Check the cache location"),
                "{label}: {err}"
            );
            assert!(got.prompts.is_empty(), "{label}");
        }
        assert!(precious.exists());
        assert!(!dir.join("apvm.db").exists() && !dir.join(".apvm.lock").exists());
    }

    // Unix only: ENOTDIR for a path below a regular file is POSIX behavior.
    #[cfg(unix)]
    #[test]
    fn an_inaccessible_path_fails_every_action_without_prompting() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let cache = file.join("cache");

        for action in all_actions() {
            let label = format!("{action:?}");
            let got = run(action, &cache, &[true]);
            let err = got
                .result
                .expect_err("an uninspectable path is not an empty cache");
            assert!(
                err.to_string().contains("cannot access cache directory"),
                "{label}: {err}"
            );
            assert!(got.prompts.is_empty(), "{label}");
            assert_eq!(got.out, "", "{label}");
        }
    }

    // ---- renderers: exact text --------------------------------------------

    #[test]
    fn usage_renders_totals_and_projects() {
        let mut usage = UsageReport {
            total_bytes: 3072,
            builds_bytes: 2048,
            releases_bytes: 1024,
            build_count: 2,
            release_count: 1,
            file_count: 3,
            database_bytes: 4096,
            ..UsageReport::default()
        };
        let header = "Artifact cache: /c\n  Builds:      2  (2.0 KiB)\n  Releases:    1  (1.0 KiB)\n  Files:       3   Database: 4.0 KiB\n  Total artifacts: 3.0 KiB\n\n";
        assert_eq!(
            usage_text(Path::new("/c"), &usage),
            format!("{header}  (cache is empty)\n")
        );
        usage.projects = vec![ProjectUsage {
            project: "backwpup".to_string(),
            builds_bytes: 2048,
            releases_bytes: 1024,
            build_count: 2,
            release_count: 1,
        }];
        assert_eq!(
            usage_text(Path::new("/c"), &usage),
            format!(
                "{header}  By project:\n    backwpup         2 builds, 1 releases  (3.0 KiB)\n"
            )
        );
    }

    #[test]
    fn clean_and_clear_render_counts_dry_runs_and_failures() {
        let mut report = CleanReport {
            builds_deleted: 2,
            releases_deleted: 1,
            bytes_freed: 512,
            ..CleanReport::default()
        };
        assert_eq!(
            clean_text(&report),
            "Removed 2 build(s) and 1 release(s), freeing 512 B.\n"
        );
        assert_eq!(
            cleared_text(&report),
            "Cleared the cache: removed 2 build(s) and 1 release(s), freeing 512 B.\n"
        );
        report.dry_run = true;
        assert_eq!(
            clean_text(&report),
            "Would remove 2 build(s) and 1 release(s), freeing 512 B.\n  (dry run — nothing was deleted)\n"
        );
        assert_eq!(failures_text(&[], "x"), "");
        assert_eq!(
            failures_text(&["a: denied".to_string()], "run `apvm cache gc`"),
            "  1 director(ies) could not be removed (run `apvm cache gc`):\n    - a: denied\n"
        );
    }

    #[test]
    fn gc_and_repair_render_every_outcome() {
        let gc = GcReport {
            stale_build_rows: 1,
            stale_release_rows: 2,
            orphan_dirs_removed: 3,
            orphan_bytes_removed: 2048,
            stale_temp_files_removed: 4,
        };
        assert_eq!(
            gc_text(&gc),
            "Cache garbage collection complete:\n  Dropped stale records: 1 build(s), 2 release(s)\n  Removed 3 orphan director(ies) (2.0 KiB)\n  Swept 4 stale temp file(s)\n"
        );

        let healthy = RepairReport::default();
        assert_eq!(
            repair_text(&healthy),
            "Cache database is healthy — nothing to repair.\n"
        );
        let recovered = RepairReport {
            quarantined_database: Some("/c/apvm.db.corrupt-1".into()),
            builds_adopted: 2,
            artifacts_adopted: 3,
            entries_skipped: 1,
            orphan_release_dirs: 1,
            ..RepairReport::default()
        };
        assert_eq!(
            repair_text(&recovered),
            "Recovered a corrupt cache database.\n  Quarantined the damaged file at: /c/apvm.db.corrupt-1\n  Re-indexed 2 build(s) / 3 artifact(s) from disk.\n  Skipped 1 unadoptable entr(ies).\n  1 cached release(s) could not be re-indexed (re-download or `apvm cache gc`).\n"
        );
        let rebuilt = RepairReport {
            rebuilt_missing_database: true,
            builds_adopted: 1,
            artifacts_adopted: 1,
            ..RepairReport::default()
        };
        assert_eq!(
            repair_text(&rebuilt),
            "Rebuilt the missing cache database.\n  Re-indexed 1 build(s) / 1 artifact(s) from disk.\n"
        );
    }

    #[test]
    fn issues_render_each_problem_then_the_hint() {
        let issue = |problem| VerifyIssue {
            project: "p".to_string(),
            context: apvm_core::maintenance::IssueContext::Release {
                tag: "v1".to_string(),
            },
            filename: "f.zip".to_string(),
            path: "/c/p/releases/v1/f.zip".into(),
            problem,
        };
        let issues = [
            issue(VerifyProblem::Missing),
            issue(VerifyProblem::SizeMismatch {
                expected: 3,
                actual: 4,
            }),
            issue(VerifyProblem::ChecksumMismatch {
                expected: "a".to_string(),
                actual: "b".to_string(),
            }),
            issue(VerifyProblem::Unreadable {
                details: "denied".to_string(),
            }),
        ];
        assert_eq!(
            issues_text(&issues),
            "Found 4 cache issue(s):\n  - [p] f.zip: missing\n  - [p] f.zip: size mismatch (recorded 3, on disk 4)\n  - [p] f.zip: checksum mismatch\n  - [p] f.zip: unreadable: denied\n\nRun `apvm cache gc` to drop records for missing files, or rebuild to heal them.\n"
        );
    }
}
