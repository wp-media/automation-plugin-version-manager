//! How tests are shown: one block per server or site for people, one
//! `--json` document (`"schema": 1`) for scripts. A failure shows its
//! stable code, the library's message and the CLI's remedies
//! ([`super::remedy`]); host keys ssh recorded on the way, and those it
//! could only trust for this run, are always shown.

use apvm_sites::remote::HostKeyNotices;
use apvm_sites::{Error, ResolvedServer, TargetWarning};
use serde::Serialize;

use super::check::{Login, ServerCheck, SiteCheck, SiteSummary};
use super::remedy::{Context, Subject, remedies};
use crate::commands::inventory::{Failure, count, destination, json_document, table};

/// One tested server, with what the test found.
pub struct ServerResult<'a> {
    /// The server, with its effective SSH settings.
    pub server: &'a ResolvedServer,
    /// The test's result.
    pub check: &'a ServerCheck,
    /// Whether `server add` is adding it (remedies are flags to pass).
    pub adding: bool,
}

impl ServerResult<'_> {
    /// Whether the test passed.
    pub fn ok(&self) -> bool {
        self.check.outcome.is_ok()
    }

    /// The remedies' context.
    fn context(&self) -> Context<'_> {
        Context {
            server: self.server,
            subject: Subject::Server {
                adding: self.adding,
            },
        }
    }
}

/// One tested site, with what the probe found.
pub struct SiteResult<'a> {
    /// The site id.
    pub id: &'a str,
    /// Its server, with its effective SSH settings.
    pub server: &'a ResolvedServer,
    /// The test's result.
    pub check: &'a SiteCheck,
    /// Whether `site add` is adding it (remedies are flags to pass).
    pub adding: bool,
}

impl SiteResult<'_> {
    /// Whether the probe passed.
    pub fn ok(&self) -> bool {
        self.check.outcome.is_ok()
    }

    /// The remedies' context.
    fn context(&self) -> Context<'_> {
        Context {
            server: self.server,
            subject: Subject::Site {
                id: self.id,
                adding: self.adding,
            },
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Text
// ─────────────────────────────────────────────────────────────────────────────

/// The text report of server tests: one block each, a blank line between
/// them, and a count when there are several.
///
/// # Arguments
///
/// * `results` - The tests, in the order asked
pub fn servers_text(results: &[ServerResult<'_>]) -> String {
    let blocks: Vec<String> = results.iter().map(server_text).collect();
    let passed = results.iter().filter(|result| result.ok()).count();
    joined(blocks, passed, results.len(), "server", "connected")
}

/// The text report of site tests (see [`servers_text`]).
///
/// # Arguments
///
/// * `results` - The tests, in the order asked
pub fn sites_text(results: &[SiteResult<'_>]) -> String {
    let blocks: Vec<String> = results.iter().map(site_text).collect();
    let passed = results.iter().filter(|result| result.ok()).count();
    joined(blocks, passed, results.len(), "site", "ready")
}

/// Blocks a blank line apart, then `N of M <noun>s <passed>; K failed.`
/// when there is more than one.
///
/// # Arguments
///
/// * `blocks` - The blocks, each ending with a newline
/// * `passed` - How many tests passed
/// * `total` - How many ran
/// * `noun` - `server` or `site`
/// * `state` - What a passed test is (`connected`, `ready`)
fn joined(blocks: Vec<String>, passed: usize, total: usize, noun: &str, state: &str) -> String {
    let mut text = blocks.join("\n");
    if total > 1 {
        text.push_str(&format!(
            "\n{passed} of {} {state}; {} failed.\n",
            count(total, noun),
            total - passed
        ));
    }
    text
}

/// One server's block.
///
/// # Arguments
///
/// * `result` - The test
fn server_text(result: &ServerResult<'_>) -> String {
    let server = result.server;
    let mut text = format!(
        "Server '{}' ({})\n",
        server.id,
        destination(&server.host, &server.ssh)
    );
    text.push_str(&host_keys_text(&result.check.host_keys, server));
    match &result.check.outcome {
        Ok(login) => {
            text.push_str("  ✓ connected\n");
            text.push_str(&table("  ", &login_rows(login, true)));
        }
        Err(error) => text.push_str(&failure_text(error, &result.context())),
    }
    text
}

/// One site's block.
///
/// # Arguments
///
/// * `result` - The test
fn site_text(result: &SiteResult<'_>) -> String {
    let server = result.server;
    let mut text = format!(
        "Site '{}' on server '{}' ({})\n",
        result.id,
        server.id,
        destination(&server.host, &server.ssh)
    );
    text.push_str(&host_keys_text(&result.check.host_keys, server));
    match &result.check.outcome {
        Ok(summary) => {
            text.push_str("  ✓ ready\n");
            text.push_str(&table("  ", &summary_rows(summary)));
            text.push_str(&summary_notes(summary));
        }
        Err(error) => text.push_str(&failure_text(error, &result.context())),
    }
    text
}

/// A failure: `✗ <code>: <message>`, then one remedy per line.
///
/// # Arguments
///
/// * `error` - Why the test failed
/// * `context` - The server and the record tested
fn failure_text(error: &Error, context: &Context<'_>) -> String {
    let mut text = format!("  ✗ {}: {error}\n", error.code());
    for remedy in remedies(error, context) {
        text.push_str(&format!("    {remedy}\n"));
    }
    text
}

/// What ssh said about host keys: each key recorded, a key it could not
/// save, a host certificate it refused before trusting the plain key.
///
/// # Arguments
///
/// * `notices` - The notices
/// * `server` - The server (for the known_hosts file)
fn host_keys_text(notices: &HostKeyNotices, server: &ResolvedServer) -> String {
    let file = server
        .ssh
        .known_hosts_file
        .as_ref()
        .map_or("~/.ssh/known_hosts", |setting| setting.value.as_str());
    let mut text = String::new();
    for host in &notices.recorded {
        text.push_str(&format!(
            "  host key  trusted the new key of {host} and recorded it in {file}\n"
        ));
    }
    for warning in host_key_warnings(notices) {
        text.push_str(&format!("  warn  {warning}\n"));
    }
    text
}

/// The host key notices that are warnings, as sentences.
///
/// # Arguments
///
/// * `notices` - The notices
fn host_key_warnings(notices: &HostKeyNotices) -> Vec<String> {
    let mut warnings = Vec::new();
    if notices.not_saved {
        warnings.push(
            "a new host key was trusted for this run only: ssh could not save it in the \
             known_hosts file, so it will be unknown again next time"
                .to_string(),
        );
    }
    if let Some(certificate) = &notices.refused_certificate {
        warnings.push(format!(
            "ssh refused the server's host certificate ({certificate}) and trusted its plain \
             key as a new one, which bypassed your certificate authority: check the \
             certificate, and remove the recorded key if it is not genuine"
        ));
    }
    warnings
}

/// The login user and tools as label/value rows.
///
/// # Arguments
///
/// * `login` - The login facts
/// * `tools` - Whether to list the tools (a server's test does)
fn login_rows(login: &Login, tools: bool) -> Vec<Vec<String>> {
    let groups: Vec<String> = login.groups.iter().map(u32::to_string).collect();
    let mut rows = vec![
        row(
            "login",
            format!("{}, group {}", login.user.text(), login.group.text()),
        ),
        row("groups", groups.join(", ")),
    ];
    if tools {
        let found = |path: &Option<String>| path.clone().unwrap_or_else(|| "not found".into());
        rows.extend([
            row("shell", found(&login.shell)),
            row("sudo", found(&login.sudo)),
            row("runuser", found(&login.runuser)),
            row("checksum", found(&login.checksum)),
            row("wp", found(&login.wp_cli)),
        ]);
    }
    rows
}

/// A probe's findings as label/value rows.
///
/// # Arguments
///
/// * `summary` - What the probe found
fn summary_rows(summary: &SiteSummary) -> Vec<Vec<String>> {
    let dir = &summary.plugins_dir;
    let identity = &summary.identity;
    let reference = format!("{}:{}", identity.owner.short(), identity.group.short());
    let new_files = if identity.creation == "at_creation" {
        format!("owned by {reference} at creation")
    } else {
        format!("checked for {reference} after creation, and repaired")
    };
    let mut rows = login_rows(&summary.login, false);
    rows.extend([
        row(
            "plugins dir",
            format!(
                "{} ({}:{} {}{})",
                dir.path,
                dir.owner.short(),
                dir.group.short(),
                dir.mode,
                if dir.read_by_root {
                    ", read by root"
                } else {
                    ""
                }
            ),
        ),
        row(
            "runs as",
            format!("{} (uid {})", identity.runs_as, identity.uid),
        ),
        row("new files", new_files),
        row("WP-CLI", wp_cli_text(summary)),
        row("WordPress", wordpress_text(summary)),
        row("site URL", summary.wordpress.siteurl.clone()),
        row("writable", writable_text(summary)),
        row("free space", space_text(summary)),
    ]);
    if summary.wordpress.disallow_file_mods {
        rows.push(row(
            "file mods",
            "disallowed (DISALLOW_FILE_MODS): installs and removals need --allow-file-mods".into(),
        ));
    }
    rows
}

/// The probe's warnings and PHP notices, as lines after the rows.
///
/// # Arguments
///
/// * `summary` - What the probe found
fn summary_notes(summary: &SiteSummary) -> String {
    let mut text = String::new();
    for warning in &summary.warnings {
        text.push_str(&format!("  warn  {}\n", warning.message));
    }
    if !summary.php_notices.is_empty() {
        text.push_str(&format!(
            "  warn  PHP printed notices around WordPress's answer: {}\n",
            summary.php_notices.join(" | ")
        ));
    }
    text
}

/// `2.12.0 (/usr/local/bin/wp)`.
///
/// # Arguments
///
/// * `summary` - What the probe found
fn wp_cli_text(summary: &SiteSummary) -> String {
    match &summary.wp_cli.path {
        Some(path) => format!("{} ({path})", summary.wp_cli.version),
        None => summary.wp_cli.version.clone(),
    }
}

/// `7.1.3, PHP 8.4.26, single site`.
///
/// # Arguments
///
/// * `summary` - What the probe found
fn wordpress_text(summary: &SiteSummary) -> String {
    let wordpress = &summary.wordpress;
    let kind = if wordpress.multisite {
        "multisite"
    } else {
        "single site"
    };
    format!(
        "{}, PHP {}, {kind}",
        wordpress.version, wordpress.php_version
    )
}

/// `plugins dir yes, upgrade dir yes`.
///
/// # Arguments
///
/// * `summary` - What the probe found
fn writable_text(summary: &SiteSummary) -> String {
    let capabilities = &summary.capabilities;
    let plugins = if capabilities.plugins_dir_writable {
        "yes"
    } else {
        "no"
    };
    let upgrade = match capabilities.upgrade_dir {
        "writable" => "yes",
        "not_writable" => "no",
        "missing" => "missing (WordPress can create it)",
        "missing_not_creatable" => "missing (WordPress cannot create it)",
        _ => "unknown",
    };
    format!("plugins dir {plugins}, upgrade dir {upgrade}")
}

/// `temp dir 71.5 GiB, content 71.5 GiB`.
///
/// # Arguments
///
/// * `summary` - What the probe found
fn space_text(summary: &SiteSummary) -> String {
    let capabilities = &summary.capabilities;
    format!(
        "temp dir {}, content {}",
        size_text(capabilities.tmp_free_kib),
        size_text(capabilities.content_free_kib)
    )
}

/// A size in KiB for people: GiB or MiB with one decimal, `unknown`
/// without a value.
///
/// # Arguments
///
/// * `kib` - The size, if known
pub fn size_text(kib: Option<u64>) -> String {
    const MIB: u64 = 1024;
    const GIB: u64 = 1024 * 1024;
    match kib {
        None => "unknown".into(),
        Some(kib) if kib >= GIB => format!("{:.1} GiB", tenths(kib, GIB)),
        Some(kib) => format!("{:.1} MiB", tenths(kib, MIB)),
    }
}

/// `value / unit`, for display with one decimal (sizes far below 2^53
/// KiB convert to `f64` exactly).
///
/// # Arguments
///
/// * `value` - The size in KiB
/// * `unit` - The unit in KiB
#[expect(
    clippy::cast_precision_loss,
    reason = "display only, one decimal of a disk size"
)]
fn tenths(value: u64, unit: u64) -> f64 {
    value as f64 / unit as f64
}

/// A label/value row.
///
/// # Arguments
///
/// * `label` - The label
/// * `value` - The value
fn row(label: &str, value: String) -> Vec<String> {
    vec![label.to_string(), value]
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON
// ─────────────────────────────────────────────────────────────────────────────

/// `--json` body of `server test`.
#[derive(Serialize)]
struct ServersJson<'a> {
    /// Whether every test passed.
    ok: bool,
    /// The servers, in the order asked.
    servers: Vec<ServerJson<'a>>,
}

/// One server in `--json` output.
#[derive(Serialize)]
struct ServerJson<'a> {
    /// Its id.
    server: &'a str,
    /// Where ssh connects (`[user@]host[:port]`).
    destination: String,
    /// Whether its test passed.
    ok: bool,
    /// What ssh said about host keys.
    host_keys: HostKeysJson<'a>,
    /// The login user and tools, when the test passed.
    login: Option<&'a Login>,
    /// Why it failed, when it did.
    error: Option<ErrorJson>,
}

/// `--json` body of `site test`.
#[derive(Serialize)]
struct SitesJson<'a> {
    /// Whether every test passed.
    ok: bool,
    /// What to know about the selection (groups without sites, sites that
    /// are the same WordPress install).
    warnings: Vec<String>,
    /// The sites, in inventory order.
    sites: Vec<SiteJson<'a>>,
}

/// One site in `--json` output.
#[derive(Serialize)]
struct SiteJson<'a> {
    /// Its id.
    site: &'a str,
    /// Its server's id.
    server: &'a str,
    /// Where ssh connects (`[user@]host[:port]`).
    destination: String,
    /// Whether its probe passed.
    ok: bool,
    /// What ssh said about host keys.
    host_keys: HostKeysJson<'a>,
    /// What the probe found, when it passed.
    probe: Option<&'a SiteSummary>,
    /// Why it failed, when it did.
    error: Option<ErrorJson>,
}

/// Host key notices in `--json` output.
#[derive(Serialize)]
struct HostKeysJson<'a> {
    /// Hosts whose new key ssh recorded, as it names them.
    recorded: &'a [String],
    /// Whether a new key was trusted without being saved.
    not_saved: bool,
    /// The host certificate ssh refused before trusting the plain key.
    refused_certificate: Option<&'a str>,
}

impl<'a> HostKeysJson<'a> {
    /// The notices as shown.
    ///
    /// # Arguments
    ///
    /// * `notices` - What ssh said
    fn of(notices: &'a HostKeyNotices) -> Self {
        Self {
            recorded: &notices.recorded,
            not_saved: notices.not_saved,
            refused_certificate: notices.refused_certificate.as_deref(),
        }
    }
}

/// A failed test in `--json` output.
#[derive(Serialize)]
struct ErrorJson {
    /// The stable code (plan §4.5).
    code: &'static str,
    /// The library's one-line message.
    message: String,
    /// Located problems (for refused settings).
    problems: Vec<ProblemJson>,
    /// What the user can do, one entry per remedy.
    remedies: Vec<String>,
}

/// A located problem in `--json` output.
#[derive(Serialize)]
struct ProblemJson {
    /// Where.
    location: String,
    /// What is wrong.
    message: String,
}

impl ErrorJson {
    /// `error` as shown, with its remedies.
    ///
    /// # Arguments
    ///
    /// * `error` - Why the test failed
    /// * `context` - The server and the record tested
    fn of(error: &Error, context: &Context<'_>) -> Self {
        Self {
            code: error.code().as_str(),
            message: error.to_string(),
            problems: error
                .problems()
                .iter()
                .map(|problem| ProblemJson {
                    location: problem.location.clone(),
                    message: problem.message.clone(),
                })
                .collect(),
            remedies: remedies(error, context),
        }
    }
}

/// The `--json` document of server tests.
///
/// # Arguments
///
/// * `results` - The tests, in the order asked
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub fn servers_json(results: &[ServerResult<'_>]) -> Result<String, Failure> {
    let servers = results
        .iter()
        .map(|result| ServerJson {
            server: &result.server.id,
            destination: destination(&result.server.host, &result.server.ssh),
            ok: result.ok(),
            host_keys: HostKeysJson::of(&result.check.host_keys),
            login: result.check.outcome.as_ref().ok(),
            error: result
                .check
                .outcome
                .as_ref()
                .err()
                .map(|error| ErrorJson::of(error, &result.context())),
        })
        .collect();
    json_document(&ServersJson {
        ok: results.iter().all(ServerResult::ok),
        servers,
    })
}

/// The `--json` document of site tests.
///
/// # Arguments
///
/// * `results` - The tests, in inventory order
/// * `warnings` - What to know about the selection
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub fn sites_json(
    results: &[SiteResult<'_>],
    warnings: &[TargetWarning],
) -> Result<String, Failure> {
    let sites = results
        .iter()
        .map(|result| SiteJson {
            site: result.id,
            server: &result.server.id,
            destination: destination(&result.server.host, &result.server.ssh),
            ok: result.ok(),
            host_keys: HostKeysJson::of(&result.check.host_keys),
            probe: result.check.outcome.as_ref().ok(),
            error: result
                .check
                .outcome
                .as_ref()
                .err()
                .map(|error| ErrorJson::of(error, &result.context())),
        })
        .collect();
    json_document(&SitesJson {
        ok: results.iter().all(SiteResult::ok),
        warnings: warnings.iter().map(ToString::to_string).collect(),
        sites,
    })
}
