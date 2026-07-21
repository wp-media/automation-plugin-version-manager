//! Background update notifier.
//!
//! Checks GitHub for a newer APVM release **without blocking** the command the
//! user actually ran, and — if a newer version exists — prints a compact,
//! colored notice as the very last thing on screen. It never updates anything;
//! it only reports (run `apvm update` to upgrade).
//!
//! # Design
//!
//! Two concerns are deliberately decoupled:
//!
//! - **Checking** is rate-limited (at most once per [`CHECK_INTERVAL_HOURS`])
//!   and runs in the background. On an eligible invocation the check is spawned
//!   as a Tokio task that overlaps the command's own work; its result is
//!   persisted to a small state file ([`Paths::update_state_file`]). Because it
//!   is throttled, the vast majority of invocations perform **no** network work
//!   and stay instant.
//!
//! - **Notifying** happens on every eligible invocation and is instant: the
//!   notice is driven by the *cached* latest version from the state file, so a
//!   newer release is surfaced on each run until the user updates — even when
//!   this run's own background check has not finished (or did not run).
//!
//! The network fetch itself reuses [`crate::commands::update`] so the notifier
//! and the `update` command always agree on "the latest version".
//!
//! # Safety / opt-out
//!
//! The whole feature is skipped when stderr is not a TTY (pipes, CI), when
//! [`OPT_OUT_ENV`] is set, and for the `update`/`uninstall` commands. A bounded
//! [`GRACE`] means a slow or hung network can never stall the CLI.

use std::io::{IsTerminal, Write};
use std::path::Path;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

use crate::paths::Paths;

// ─────────────────────────────────────────────────────────────────────────────
// Tunables
// ─────────────────────────────────────────────────────────────────────────────

/// Minimum interval, in hours, between two background update checks.
///
/// Running `apvm` repeatedly within this window performs the network check at
/// most once; every other invocation notifies from cached state only.
const CHECK_INTERVAL_HOURS: i64 = 1;

/// Upper bound on how long the report step waits for an in-flight check to
/// finish before falling back to the cached version.
///
/// This only ever applies on the rare (≤ once/[`CHECK_INTERVAL_HOURS`]) run
/// that actually performs a network check *and* whose command finished before
/// that check did (e.g. a fast `apvm list`). Slow commands (builds) overlap the
/// check entirely and pay nothing here.
const GRACE: Duration = Duration::from_millis(1500);

/// Opt-out environment variable. When set to any non-empty value, the entire
/// background update check and notice are disabled.
const OPT_OUT_ENV: &str = "APVM_NO_UPDATE_CHECK";

/// Releases page shown in the notice as a secondary, copy-pasteable action.
const RELEASES_URL: &str = "https://github.com/wp-media/automation-plugin-version-manager/releases";

// ─────────────────────────────────────────────────────────────────────────────
// Persisted state
// ─────────────────────────────────────────────────────────────────────────────

/// On-disk state for the background notifier (`~/.apvm/update-check.json`).
///
/// Both fields are optional and skipped when absent, so a fresh install has no
/// file and a corrupt file degrades to defaults rather than an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct UpdateState {
    /// When the last check was *attempted* (RFC 3339 UTC). Drives the throttle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_check: Option<DateTime<Utc>>,

    /// Latest version observed at the last successful check. Drives the notice
    /// on runs that do not perform a fresh check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_version: Option<String>,
}

/// Read the state file, tolerating a missing or corrupt file by returning the
/// default (empty) state. This path must never surface an error to the user —
/// a broken notifier cache is not worth failing a command over.
fn load_state(path: &Path) -> UpdateState {
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => UpdateState::default(),
    }
}

/// Write the state file, creating the parent directory if needed.
///
/// The write is a single truncating [`std::fs::write`], intentionally **not**
/// atomic. This file is a self-healing throttle cache: a torn concurrent read
/// simply fails to parse and [`load_state`] degrades to the default (costing at
/// most one extra check), and the next writer overwrites cleanly. So the
/// temp-then-rename dance is unwarranted here — matching `apvm_core::config_io`.
///
/// Returns an `io::Error` so callers can log it at debug level; no caller
/// treats a persistence failure as fatal (see [`persist`]).
fn save_state(path: &Path, state: &UpdateState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

/// Persist state best-effort: log a failure at debug level, never propagate it.
///
/// The single choke point through which both the notifier ([`Checker::
/// finish_and_report`]) and the `update` command ([`record_check`]) write
/// state, so the "never fail the command over a broken cache" policy lives in
/// exactly one place.
fn persist(path: &Path, state: &UpdateState) {
    if let Err(e) = save_state(path, state) {
        tracing::debug!("failed to persist update-check state: {e}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure predicates (unit-tested without touching the environment or clock)
// ─────────────────────────────────────────────────────────────────────────────

/// Whether a fresh check is due given the last-check timestamp.
///
/// Due when no prior check exists, when at least `interval` has elapsed, or when
/// the stored timestamp lies in the future (clock changed / corrupt state) so
/// the throttle self-heals instead of blocking checks forever.
fn is_due(last_check: Option<DateTime<Utc>>, now: DateTime<Utc>, interval: TimeDelta) -> bool {
    match last_check {
        None => true,
        Some(prev) => {
            let elapsed = now.signed_duration_since(prev);
            elapsed >= interval || elapsed < TimeDelta::zero()
        }
    }
}

/// Whether the notifier should run at all this invocation.
///
/// All three conditions must hold: the command is one we notify for, stderr is
/// a terminal (so we never pollute pipes/CI), and the user has not opted out.
fn should_check(command_eligible: bool, stderr_is_tty: bool, opted_out: bool) -> bool {
    command_eligible && stderr_is_tty && !opted_out
}

/// Read the opt-out environment variable. Any non-empty value disables the
/// feature (an accidental empty `APVM_NO_UPDATE_CHECK=` does not).
fn opted_out() -> bool {
    std::env::var_os(OPT_OUT_ENV).is_some_and(|v| !v.is_empty())
}

// ─────────────────────────────────────────────────────────────────────────────
// Notice rendering
// ─────────────────────────────────────────────────────────────────────────────

/// Wrap `text` in an SGR color code when `color` is enabled; otherwise return
/// it unchanged so `NO_COLOR` and non-TTY output stay plain.
fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// SGR color code for the notice's border.
const BORDER_COLOR: &str = "33";

/// Render the boxed update notice, or `None` when already up to date.
///
/// Returns `None` unless `latest > current`. Composes the notice's content
/// lines; [`draw_box`] does the framing.
fn render_notice(
    current: &semver::Version,
    latest: &semver::Version,
    color: bool,
) -> Option<String> {
    if latest <= current {
        return None;
    }

    // SGR codes: bold-yellow title, dim current, bold-green latest, bold-cyan command, dim URL.
    let (title, dim, green, cyan) = ("1;33", "2", "1;32", "1;36");

    // (plain, colored) pairs — the plain text drives the box width.
    let lines = [
        (
            format!("Update available   {current} → {latest}"),
            format!(
                "{}   {} → {}",
                paint("Update available", title, color),
                paint(&current.to_string(), dim, color),
                paint(&latest.to_string(), green, color),
            ),
        ),
        (
            "Run  apvm update  to upgrade".to_string(),
            format!("Run  {}  to upgrade", paint("apvm update", cyan, color)),
        ),
        (RELEASES_URL.to_string(), paint(RELEASES_URL, dim, color)),
    ];

    Some(draw_box(&lines, color))
}

/// Frame `(plain, colored)` text lines in a rounded box, preceded and followed
/// by a blank line.
///
/// Each line's width is measured from its `plain` text, so the ANSI escapes in
/// `colored` never skew the padding — colored and plain renderings align
/// identically. Callers must keep `plain` equal to the visible text of
/// `colored`.
fn draw_box(lines: &[(String, String)], color: bool) -> String {
    const PAD: usize = 2;
    let inner = lines
        .iter()
        .map(|(plain, _)| plain.chars().count())
        .max()
        .unwrap_or(0);
    let rule = "─".repeat(inner + PAD * 2);
    let bar = paint("│", BORDER_COLOR, color);

    let mut out = String::new();
    out.push('\n');
    out.push_str(&paint(&format!("┌{rule}┐"), BORDER_COLOR, color));
    out.push('\n');
    for (plain, colored) in lines {
        let right = inner - plain.chars().count() + PAD;
        out.push_str(&format!(
            "{bar}{lpad}{colored}{rpad}{bar}\n",
            lpad = " ".repeat(PAD),
            rpad = " ".repeat(right),
        ));
    }
    out.push_str(&paint(&format!("└{rule}┘"), BORDER_COLOR, color));
    out.push('\n');
    out
}

/// Decide which update notice to show, if any.
///
/// Pure decision logic: the freshest known version wins (this run's `fresh`
/// result, else the `cached` one), and a notice is produced only when that
/// version is strictly newer than `current`. Separated from I/O so the
/// precedence and "only when newer" rules are unit-testable.
fn resolve_notice(
    current: &semver::Version,
    fresh: Option<&semver::Version>,
    cached: Option<&semver::Version>,
    color: bool,
) -> Option<String> {
    let latest = fresh.or(cached)?;
    render_notice(current, latest, color)
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// A started (or skipped) update check, finalized once the command completes.
pub struct Checker {
    /// In-flight network check — `Some` only when the throttle interval elapsed
    /// and a fresh check was spawned this invocation.
    handle: Option<JoinHandle<Option<semver::Version>>>,
    /// Latest version known from a previous run; lets us notify even when no
    /// fresh check runs (or it does not finish in time).
    cached_latest: Option<semver::Version>,
    /// This binary's version, parsed once up front.
    current: semver::Version,
    /// Where refreshed state is persisted.
    state_path: std::path::PathBuf,
    /// Whether the notice should be colored.
    color: bool,
}

/// Start the background update check, or return `None` when it should not run.
///
/// Call this as early as possible so the network check overlaps the command's
/// own work. `command_eligible` should be `false` for commands that must not
/// trigger a notice (`update`, `uninstall`).
///
/// The returned [`Checker`] must later be driven with
/// [`Checker::finish_and_report`] to persist state and print the notice.
pub fn maybe_start(paths: &Paths, command_eligible: bool) -> Option<Checker> {
    let stderr_is_tty = std::io::stderr().is_terminal();
    if !should_check(command_eligible, stderr_is_tty, opted_out()) {
        return None;
    }

    // If our own version does not parse we simply disable the notifier rather
    // than risk interfering with the command. (Practically unreachable.)
    let current = crate::commands::update::current_version().ok()?;

    let state_path = paths.update_state_file().clone();
    let state = load_state(&state_path);
    let cached_latest = state
        .latest_version
        .as_deref()
        .and_then(|v| semver::Version::parse(v).ok());

    let handle = if is_due(
        state.last_check,
        Utc::now(),
        TimeDelta::hours(CHECK_INTERVAL_HOURS),
    ) {
        Some(tokio::spawn(async {
            match crate::commands::update::fetch_latest_version().await {
                Ok(version) => Some(version),
                Err(e) => {
                    tracing::debug!("background update check failed: {e}");
                    None
                }
            }
        }))
    } else {
        None
    };

    // `should_check` already guaranteed stderr is a TTY, so color depends only
    // on NO_COLOR here.
    let color = std::env::var_os("NO_COLOR").is_none();

    Some(Checker {
        handle,
        cached_latest,
        current,
        state_path,
        color,
    })
}

impl Checker {
    /// Finalize the check and, if a newer version is known, print the notice.
    ///
    /// Waits at most [`GRACE`] for an in-flight check to finish, persists the
    /// refreshed state (bumping the throttle even on failure), then prints the
    /// notice to stderr — the last output of the program.
    pub async fn finish_and_report(self) {
        let ran_check = self.handle.is_some();

        // Collect this run's fresh result without blocking beyond GRACE.
        let fresh = match self.handle {
            Some(handle) => match timeout(GRACE, handle).await {
                Ok(Ok(version)) => version,
                // `Ok(Err(_))` is a JoinError — only a panic here, since we
                // never abort the task; `Err(_)` is the grace window elapsing.
                // Either way, fall back to the cached version below.
                Ok(Err(_)) | Err(_) => None,
            },
            None => None,
        };

        // Persist only when we actually attempted a check this run. Stamping
        // `last_check` even on a failed/timed-out fetch keeps us to at most one
        // network attempt per interval; `latest_version` keeps its prior value
        // when no fresh result arrived.
        if ran_check {
            persist(
                &self.state_path,
                &UpdateState {
                    last_check: Some(Utc::now()),
                    latest_version: fresh
                        .as_ref()
                        .or(self.cached_latest.as_ref())
                        .map(|v| v.to_string()),
                },
            );
        }

        // Notify from the freshest version available (this run's, else cache).
        if let Some(notice) = resolve_notice(
            &self.current,
            fresh.as_ref(),
            self.cached_latest.as_ref(),
            self.color,
        ) {
            eprint!("{notice}");
            let _ = std::io::stderr().flush();
        }
    }
}

/// Record a completed check performed *outside* the notifier (by `apvm update`).
///
/// Keeps the notifier's state file in sync with the `update` command so a run
/// of `apvm update` both respects the throttle and prevents an immediate
/// re-notification on the next invocation. Best-effort: persistence failures
/// are logged at debug level and never propagated.
pub fn record_check(paths: &Paths, latest: &semver::Version) {
    persist(
        paths.update_state_file(),
        &UpdateState {
            last_check: Some(Utc::now()),
            latest_version: Some(latest.to_string()),
        },
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    // ── is_due ───────────────────────────────────────────────────────────

    #[test]
    fn is_due_true_when_never_checked() {
        assert!(is_due(None, Utc::now(), TimeDelta::hours(1)));
    }

    #[test]
    fn is_due_false_within_interval() {
        let now = Utc::now();
        let recent = now - TimeDelta::minutes(30);
        assert!(!is_due(Some(recent), now, TimeDelta::hours(1)));
    }

    #[test]
    fn is_due_true_after_interval() {
        let now = Utc::now();
        let old = now - TimeDelta::hours(2);
        assert!(is_due(Some(old), now, TimeDelta::hours(1)));
    }

    #[test]
    fn is_due_true_exactly_at_interval() {
        let now = Utc::now();
        let exactly = now - TimeDelta::hours(1);
        assert!(is_due(Some(exactly), now, TimeDelta::hours(1)));
    }

    #[test]
    fn is_due_true_when_timestamp_in_future() {
        // A corrupt/skewed future timestamp must not disable checks forever.
        let now = Utc::now();
        let future = now + TimeDelta::hours(5);
        assert!(is_due(Some(future), now, TimeDelta::hours(1)));
    }

    // ── should_check ─────────────────────────────────────────────────────

    #[test]
    fn should_check_requires_all_conditions() {
        assert!(should_check(true, true, false));
        assert!(!should_check(false, true, false)); // ineligible command
        assert!(!should_check(true, false, false)); // not a TTY
        assert!(!should_check(true, true, true)); // opted out
    }

    // ── state round-trip ─────────────────────────────────────────────────

    #[test]
    fn load_missing_state_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        assert_eq!(load_state(&path), UpdateState::default());
    }

    #[test]
    fn load_corrupt_state_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        std::fs::write(&path, "{ not valid json ").unwrap();
        assert_eq!(load_state(&path), UpdateState::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path also proves the parent directory is created.
        let path = dir.path().join("nested").join("update-check.json");
        let now = Utc::now();
        let state = UpdateState {
            last_check: Some(now),
            latest_version: Some("9.9.9".to_string()),
        };
        save_state(&path, &state).unwrap();

        let loaded = load_state(&path);
        assert_eq!(loaded.latest_version.as_deref(), Some("9.9.9"));
        assert!(loaded.last_check.is_some());
    }

    #[test]
    fn save_omits_absent_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        save_state(&path, &UpdateState::default()).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("last_check"));
        assert!(!contents.contains("latest_version"));
    }

    // ── record_check ─────────────────────────────────────────────────────

    #[test]
    fn record_check_persists_version_and_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        record_check(&paths, &ver("4.5.6"));

        let loaded = load_state(paths.update_state_file());
        assert_eq!(loaded.latest_version.as_deref(), Some("4.5.6"));
        assert!(loaded.last_check.is_some());
    }

    // ── render_notice ────────────────────────────────────────────────────

    #[test]
    fn render_notice_none_when_up_to_date() {
        assert!(render_notice(&ver("3.1.1"), &ver("3.1.1"), false).is_none());
        assert!(render_notice(&ver("3.1.1"), &ver("3.0.0"), false).is_none());
    }

    #[test]
    fn render_notice_some_when_newer() {
        let notice = render_notice(&ver("3.1.1"), &ver("3.2.0"), false).unwrap();
        assert!(notice.contains("Update available"));
        assert!(notice.contains("3.1.1"));
        assert!(notice.contains("3.2.0"));
        assert!(notice.contains("apvm update"));
        assert!(notice.contains(RELEASES_URL));
    }

    #[test]
    fn render_notice_plain_has_no_ansi() {
        let notice = render_notice(&ver("1.0.0"), &ver("2.0.0"), false).unwrap();
        assert!(!notice.contains('\x1b'), "plain notice must be escape-free");
    }

    #[test]
    fn render_notice_colored_has_ansi() {
        let notice = render_notice(&ver("1.0.0"), &ver("2.0.0"), true).unwrap();
        assert!(
            notice.contains('\x1b'),
            "colored notice must contain escapes"
        );
        // Both versions still appear as visible text between the escapes.
        assert!(notice.contains("2.0.0"));
    }

    #[test]
    fn render_notice_box_borders_are_balanced() {
        // The top and bottom rules must be the same width, and every interior
        // line must be framed by the vertical bar. Checked on the plain form.
        let notice = render_notice(&ver("1.0.0"), &ver("2.0.0"), false).unwrap();
        let lines: Vec<&str> = notice.lines().filter(|l| !l.is_empty()).collect();
        let top = lines.first().unwrap();
        let bottom = lines.last().unwrap();
        assert!(top.starts_with('┌') && top.ends_with('┐'));
        assert!(bottom.starts_with('└') && bottom.ends_with('┘'));
        assert_eq!(top.chars().count(), bottom.chars().count());
        for line in &lines[1..lines.len() - 1] {
            assert!(
                line.starts_with('│') && line.ends_with('│'),
                "interior line not framed: {line:?}"
            );
            // Interior lines share the border's visible width.
            assert_eq!(line.chars().count(), top.chars().count());
        }
    }

    /// Strip ANSI SGR escape sequences (`ESC [ … m`) so a colored line can be
    /// measured by its *visible* width.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for d in chars.by_ref() {
                    if d == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn render_notice_colored_lines_align_when_stripped() {
        // The alignment invariant: padding is measured on plain text, so once
        // the ANSI escapes are removed every framed line has identical width.
        // A regression that measured the *colored* length would fail here.
        let colored = render_notice(&ver("1.2.3"), &ver("10.20.30"), true).unwrap();
        let widths: Vec<usize> = strip_ansi(&colored)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.chars().count())
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "colored lines misalign after stripping ANSI: {widths:?}"
        );
    }

    // ── draw_box ─────────────────────────────────────────────────────────

    #[test]
    fn draw_box_pads_uneven_lines_to_one_width() {
        // Two plain lines of different widths must frame to the same width.
        let lines = [
            ("short".to_string(), "short".to_string()),
            (
                "a much longer line".to_string(),
                "a much longer line".to_string(),
            ),
        ];
        let boxed = draw_box(&lines, false);
        let widths: Vec<usize> = boxed
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.chars().count())
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "widths: {widths:?}"
        );
    }

    // ── resolve_notice (precedence + only-when-newer) ────────────────────

    #[test]
    fn resolve_notice_prefers_fresh_over_cached() {
        // Both present → the fresh version wins (and appears in the notice).
        let notice = resolve_notice(
            &ver("1.0.0"),
            Some(&ver("3.0.0")),
            Some(&ver("2.0.0")),
            false,
        )
        .expect("newer than current → Some");
        assert!(notice.contains("3.0.0"));
        assert!(!notice.contains("2.0.0"), "cached must not override fresh");
    }

    #[test]
    fn resolve_notice_falls_back_to_cached_when_no_fresh() {
        let notice = resolve_notice(&ver("1.0.0"), None, Some(&ver("2.5.0")), false).unwrap();
        assert!(notice.contains("2.5.0"));
    }

    #[test]
    fn resolve_notice_none_when_neither_present() {
        assert!(resolve_notice(&ver("1.0.0"), None, None, false).is_none());
    }

    #[test]
    fn resolve_notice_none_when_freshest_not_newer() {
        // Freshest known (fresh) is not newer than current → no notice, even
        // though a stale cached value would be.
        assert!(
            resolve_notice(
                &ver("2.0.0"),
                Some(&ver("2.0.0")),
                Some(&ver("9.0.0")),
                false
            )
            .is_none()
        );
    }

    // ── Checker::finish_and_report (branch coverage) ─────────────────────

    fn checker(
        handle: Option<JoinHandle<Option<semver::Version>>>,
        cached: Option<&str>,
        current: &str,
        state_path: std::path::PathBuf,
    ) -> Checker {
        Checker {
            handle,
            cached_latest: cached.map(ver),
            current: ver(current),
            state_path,
            color: false, // keep any emitted notice escape-free under test capture
        }
    }

    #[tokio::test(start_paused = true)]
    async fn finish_and_report_times_out_and_falls_back_to_cache() {
        // A check that never completes within GRACE must not stall the CLI: the
        // bounded wait elapses (virtual clock), we fall back to the cached
        // version, and the throttle timestamp is still stamped. If the timeout
        // failed, the 1-hour task would resolve to "100.0.0" and be persisted —
        // asserting the cached "2.0.0" instead proves the fallback fired.
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("update-check.json");
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Some(ver("100.0.0"))
        });

        checker(Some(handle), Some("2.0.0"), "1.0.0", state_path.clone())
            .finish_and_report()
            .await;

        let state = load_state(&state_path);
        assert!(state.last_check.is_some(), "throttle must be stamped");
        assert_eq!(
            state.latest_version.as_deref(),
            Some("2.0.0"),
            "must persist the cached version on timeout, not the never-arrived fresh one"
        );
    }

    #[tokio::test]
    async fn finish_and_report_persists_fresh_version() {
        // A completed check persists its fresh result even with no prior cache.
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("update-check.json");
        let handle = tokio::spawn(async { Some(ver("5.0.0")) });

        checker(Some(handle), None, "1.0.0", state_path.clone())
            .finish_and_report()
            .await;

        let state = load_state(&state_path);
        assert!(state.last_check.is_some());
        assert_eq!(state.latest_version.as_deref(), Some("5.0.0"));
    }

    #[tokio::test]
    async fn finish_and_report_without_check_does_not_write_state() {
        // When no check ran this invocation (throttle not due), the state file
        // must be left untouched even though a cached newer version triggers a
        // (cache-driven) notice.
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("update-check.json");

        checker(None, Some("9.0.0"), "1.0.0", state_path.clone())
            .finish_and_report()
            .await;

        assert!(
            !state_path.exists(),
            "no check ran → state file must not be created/rewritten"
        );
    }
}
