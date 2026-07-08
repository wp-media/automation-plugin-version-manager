//! Cache maintenance command: `apvm cache <action>`.
//!
//! A thin CLI over the `apvm-storage` maintenance API — usage reporting,
//! cleaning (by age / project / kind, with dry-run), database↔disk
//! reconciliation (`gc`), integrity verification, corruption recovery
//! (`repair`), and full clear.
//!
//! These operate directly on the configured cache directory and are
//! **independent of the `cache` on/off setting** — you can inspect or clean a
//! cache you've turned off. Unlike the build path (where cache failures are
//! swallowed so a build still succeeds), errors here surface normally: the
//! user explicitly asked for the operation.

use std::io::{self, BufRead, Write};
use std::path::Path;

use chrono::Utc;
use clap::{Args, Subcommand};

use apvm_core::error::Error;
use apvm_storage::{ArtifactStore, CleanOptions, CleanTarget, VerifyMode, VerifyProblem};

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

impl CacheArgs {
    /// Execute the cache command against `cache_dir` (the resolved cache
    /// directory — config override or default).
    pub fn execute(&self, cache_dir: &Path) -> apvm_core::Result<()> {
        // Nothing has ever been cached — don't create the store just to report
        // emptiness; give a friendly note instead (applies to every action,
        // including repair: there is nothing to recover).
        if !cache_dir.exists() {
            println!(
                "Cache is empty — nothing has been cached yet.\n  Location: {}",
                cache_dir.display()
            );
            return Ok(());
        }

        match &self.action {
            CacheAction::Info => info(cache_dir),
            CacheAction::Clean(args) => clean(cache_dir, args),
            CacheAction::Gc => gc(cache_dir),
            CacheAction::Verify { checksum } => verify(cache_dir, *checksum),
            CacheAction::Repair => repair(cache_dir),
            CacheAction::Clear { yes } => clear(cache_dir, *yes),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Actions
// ─────────────────────────────────────────────────────────────────────────────

/// Open the store, attaching a recovery hint when the database is corrupt
/// (every action except `repair`, which exists to handle exactly that case).
fn open_store(cache_dir: &Path) -> apvm_core::Result<ArtifactStore> {
    ArtifactStore::open(cache_dir).map_err(|e| match e {
        apvm_storage::Error::DatabaseCorrupted { .. } => Error::Cache(format!(
            "{e}\n  The cache database is corrupt — run `apvm cache repair` to recover it."
        )),
        other => other.into(),
    })
}

/// `apvm cache info` — usage totals and per-project breakdown.
fn info(cache_dir: &Path) -> apvm_core::Result<()> {
    let store = open_store(cache_dir)?;
    let usage = store.usage()?;

    println!("Artifact cache: {}", cache_dir.display());
    println!(
        "  Builds:   {:>4}  ({})",
        usage.build_count,
        human_bytes(usage.builds_bytes)
    );
    println!(
        "  Releases: {:>4}  ({})",
        usage.release_count,
        human_bytes(usage.releases_bytes)
    );
    println!(
        "  Files:    {:>4}   Database: {}",
        usage.file_count,
        human_bytes(usage.database_bytes)
    );
    println!("  Total artifacts: {}", human_bytes(usage.total_bytes));

    if usage.projects.is_empty() {
        println!("\n  (cache is empty)");
    } else {
        println!("\n  By project:");
        for project in &usage.projects {
            println!(
                "    {:<16} {} builds, {} releases  ({})",
                project.project,
                project.build_count,
                project.release_count,
                human_bytes(project.builds_bytes.saturating_add(project.releases_bytes)),
            );
        }
    }
    Ok(())
}

/// `apvm cache clean` — delete entries by age / project / kind.
fn clean(cache_dir: &Path, args: &CleanArgs) -> apvm_core::Result<()> {
    let store = open_store(cache_dir)?;

    let mut options = CleanOptions::default().dry_run(args.dry_run);
    if let Some(project) = &args.project {
        options = options.project(project.clone());
    }
    if let Some(spec) = &args.older_than {
        let duration = parse_duration(spec).map_err(Error::Config)?;
        let cutoff = Utc::now()
            .checked_sub_signed(duration)
            .ok_or_else(|| Error::Config(format!("--older-than value '{spec}' is out of range")))?;
        options = options.older_than(cutoff);
    }
    options = options.target(if args.builds {
        CleanTarget::Builds
    } else if args.releases {
        CleanTarget::Releases
    } else {
        CleanTarget::All
    });

    let report = store.clean(&options)?;

    let verb = if report.dry_run {
        "Would remove"
    } else {
        "Removed"
    };
    println!(
        "{verb} {} build(s) and {} release(s), freeing {}.",
        report.builds_deleted,
        report.releases_deleted,
        human_bytes(report.bytes_freed),
    );
    if report.dry_run {
        println!("  (dry run — nothing was deleted)");
    }
    if !report.failures.is_empty() {
        eprintln!(
            "  {} director(ies) could not be removed (orphaned until `apvm cache gc`):",
            report.failures.len()
        );
        for failure in &report.failures {
            eprintln!("    - {failure}");
        }
    }
    Ok(())
}

/// `apvm cache gc` — reconcile the database with disk.
fn gc(cache_dir: &Path) -> apvm_core::Result<()> {
    let store = open_store(cache_dir)?;
    let report = store.gc()?;
    println!("Cache garbage collection complete:");
    println!(
        "  Dropped stale records: {} build(s), {} release(s)",
        report.stale_build_rows, report.stale_release_rows
    );
    println!(
        "  Removed {} orphan director(ies) ({})",
        report.orphan_dirs_removed,
        human_bytes(report.orphan_bytes_removed)
    );
    println!(
        "  Swept {} stale temp file(s)",
        report.stale_temp_files_removed
    );
    Ok(())
}

/// `apvm cache verify` — report integrity problems.
///
/// Exits non-zero (returns an error) when issues are found, so scripts and
/// CI can gate on cache health.
fn verify(cache_dir: &Path, checksum: bool) -> apvm_core::Result<()> {
    let store = open_store(cache_dir)?;
    let mode = if checksum {
        VerifyMode::Checksum
    } else {
        VerifyMode::Size
    };
    let issues = store.verify(mode)?;

    if issues.is_empty() {
        let depth = if checksum { "checksum" } else { "size" };
        println!("Cache verified ({depth} check): no issues found.");
        return Ok(());
    }

    eprintln!("Found {} cache issue(s):", issues.len());
    for issue in &issues {
        let problem = match &issue.problem {
            VerifyProblem::Missing => "missing".to_string(),
            VerifyProblem::SizeMismatch { expected, actual } => {
                format!("size mismatch (recorded {expected}, on disk {actual})")
            }
            VerifyProblem::ChecksumMismatch { .. } => "checksum mismatch".to_string(),
            VerifyProblem::Unreadable { details } => format!("unreadable: {details}"),
        };
        eprintln!("  - [{}] {}: {}", issue.project, issue.filename, problem);
    }
    eprintln!("\nRun `apvm cache gc` to drop records for missing files, or rebuild to heal them.");
    Err(Error::Cache(format!(
        "verification found {} issue(s)",
        issues.len()
    )))
}

/// `apvm cache repair` — recover a corrupt database.
fn repair(cache_dir: &Path) -> apvm_core::Result<()> {
    let (_store, report) = ArtifactStore::repair(cache_dir)?;
    match report.quarantined_database {
        None => println!("Cache database is healthy — nothing to repair."),
        Some(path) => {
            println!("Recovered a corrupt cache database.");
            println!("  Quarantined the damaged file at: {}", path.display());
            println!(
                "  Re-indexed {} build(s) / {} artifact(s) from disk.",
                report.builds_adopted, report.artifacts_adopted
            );
            if report.entries_skipped > 0 {
                println!(
                    "  Skipped {} unadoptable entr(ies).",
                    report.entries_skipped
                );
            }
            if report.orphan_release_dirs > 0 {
                println!(
                    "  {} cached release(s) could not be re-indexed (re-download or `apvm cache gc`).",
                    report.orphan_release_dirs
                );
            }
        }
    }
    Ok(())
}

/// `apvm cache clear` — remove everything (with confirmation).
fn clear(cache_dir: &Path, yes: bool) -> apvm_core::Result<()> {
    if !yes
        && !confirm(&format!(
            "Remove ALL cached artifacts in {}? [y/N] ",
            cache_dir.display()
        ))?
    {
        println!("Aborted. Nothing was removed.");
        return Ok(());
    }
    let store = open_store(cache_dir)?;
    let report = store.clear_all()?;
    println!(
        "Cleared the cache: removed {} build(s) and {} release(s), freeing {}.",
        report.builds_deleted,
        report.releases_deleted,
        human_bytes(report.bytes_freed),
    );
    if !report.failures.is_empty() {
        eprintln!(
            "  {} director(ies) could not be removed (run `apvm cache gc`):",
            report.failures.len()
        );
        for failure in &report.failures {
            eprintln!("    - {failure}");
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

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
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Parse a human duration like `30d`, `12h`, `2w`, `45m` into a
/// [`chrono::Duration`]. Units: `m` minutes, `h` hours, `d` days, `w` weeks.
fn parse_duration(spec: &str) -> Result<chrono::Duration, String> {
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
}
