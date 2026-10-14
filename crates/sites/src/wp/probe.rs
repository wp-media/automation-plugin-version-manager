//! The probe (plan §6.2): who WP-CLI runs as on a site, whether it is
//! really WP-CLI, what WordPress reports, and what the identity can do.
//! Read-only, one round trip per step:
//!
//! | Step | Runs as | What |
//! |---|---|---|
//! | H — host | login user | `id` (ids, names, groups), `$SHELL`, `command -v` of sudo, runuser, the checksum tools and `wp_cli`; the walk of the plugins directory (one `sh` process: `cd -P` per component, `pwd -P`, `stat .`; only links root or the login user own, in directories only they may write; a BSD `stat -f` retry when `-c` is refused; on `Permission denied`, root's `stat` through `sudo -n -u root`, which never follows a link, so anything but a directory is refused, and the identity's own walk must confirm its owner and group right before step W); `id -u` of the users `run_as` and `owner` name |
//! | I — identity | — | the identity table ([`plan_identity`]), then the exact command shape: `<prefix> env -u … wp --skip-packages --exec=… [--allow-root] --path=… cli version`, candidate by candidate; sudo's refusals classified ([`SudoRefusal`]) |
//! | W — WordPress | identity | `wp eval` of [`SITE_FACTS`](super::snippets::SITE_FACTS); WP-CLI's own failures read as `not_wordpress` / `wp_not_installed`; `WP_CLI_VERSION` must be the version `cli version` printed |
//! | O — consistency | — | `realpath(WP_PLUGIN_DIR)` must be step H's directory, else `plugins_dir_mismatch` (the identity is never switched to what WordPress reports) |
//! | C — capabilities | identity | `test -w` of the plugins, upgrade and content directories, `df -Pk` of `remote_tmp_dir` and the content directory: warnings, not failures |
//!
//! `--allow-root` is passed exactly when the identity's uid is 0 (which
//! the table allows only with the site's `allow_root`). Each statement of
//! a step reports its own status (independent statements framed by the
//! call's nonce), so one failed command never hides another's answer.
//!
//! Failures are [`Error::Probe`] with a [`ProbeFailure`] and its code;
//! transport failures keep theirs (`ssh_*`, `timeout`).
//!
//! [`probe_server`] runs the login part of step H alone, for a server
//! without a site (`apvm server test`): who the login user is, and which
//! tools the server has.

mod checks;
mod dir;
mod failure;
mod host;

use std::time::Duration;

pub use checks::{Capabilities, UpgradeDir};
pub use failure::{ProbeFailure, ProbeWarning};
pub use host::HostFacts;

use super::command::{WpCommand, WpSite};
use super::parse::{self, CommandOutput, Notices, Parsed, SiteFacts, WpCliVersion};
use crate::error::{Error, Result};
use crate::inventory::{BUILTIN_WP_CLI, ResolvedSite};
use crate::remote::call::{call, last_line};
use crate::remote::identity::sudo::{self, SudoSignal};
use crate::remote::inspect;
use crate::remote::steps::{StepOutput, Steps, StepsError, split};
use crate::remote::{
    Account, AccountName, Candidate, Creation, DirOwnership, FileMode, Identity, IdentityFacts,
    IdentityPlan, IdentityRefusal, Nonce, Reference, RootRole, Script, SudoRefusal, Transport,
    plan_identity,
};
use checks::CheckPaths;

/// How long one probe step may run when the site sets no
/// `command_timeout_secs` (plan §6.1 item 7).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// WP-CLI's message for a path without WordPress.
const NOT_WORDPRESS: &str = "This does not seem to be a WordPress installation.";
/// WP-CLI's messages for WordPress files without an installed site.
const NOT_INSTALLED: [&str; 2] = [
    "'wp-config.php' not found.",
    "The site you have requested is not installed.",
];

/// The plugins directory step H measured.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginsDir {
    /// Its physical path (`pwd -P` at the end of the walk; for a directory
    /// only root could read, the identity's confirming walk's).
    pub path: String,
    /// Its owner, group and mode.
    pub ownership: DirOwnership,
    /// Whether root's `stat` read it (the login user could not).
    pub read_by_root: bool,
}

/// The identity the probe chose.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbedIdentity {
    /// The prefix every command of the site runs behind.
    pub identity: Identity,
    /// The uid WP-CLI runs with (`--allow-root` exactly when 0).
    pub uid: u32,
    /// Whether new files get the reference owner and group.
    pub creation: Creation,
    /// The ownership plugin files must have (plan §6.8).
    pub reference: Reference,
}

/// Everything the probe learned about a site.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SiteProbe {
    /// The site's id.
    pub site_id: String,
    /// The login user and the server's tools.
    pub host: HostFacts,
    /// The plugins directory.
    pub plugins_dir: PluginsDir,
    /// Who WP-CLI runs as.
    pub identity: ProbedIdentity,
    /// What `cli version` printed.
    pub wp_cli_version: WpCliVersion,
    /// What WordPress reports (observations; never trusted for the
    /// identity).
    pub facts: SiteFacts,
    /// What the identity can do.
    pub capabilities: Capabilities,
    /// Findings that do not stop reads.
    pub warnings: Vec<ProbeWarning>,
    /// PHP notices printed around step W's payload.
    pub notices: Notices,
    /// The site's hermetic WP-CLI base, with its URL and `--allow-root`
    /// when the identity is root.
    wp: WpSite,
}

impl SiteProbe {
    /// The site's WP-CLI base, for the commands a session runs.
    pub fn wp(&self) -> &WpSite {
        &self.wp
    }
}

/// Probe `site` through `transport` (the module's steps).
///
/// # Arguments
///
/// * `transport` - Where the site's server is
/// * `site` - The site, resolved
///
/// # Errors
///
/// [`Error::Probe`] with the step's [`ProbeFailure`]; a transport failure
/// ([`Error::Ssh`], [`Error::Timeout`], …); [`Error::WpOutput`] when step W
/// failed for another reason; [`Error::InvalidInput`] when a setting
/// cannot form a command.
pub async fn probe(transport: &dyn Transport, site: &ResolvedSite) -> Result<SiteProbe> {
    Prober::new(transport, site).run().await
}

/// The login part of step H on a server, without a site: the login user's
/// ids, names and groups, its shell, and where `sudo`, `runuser`, the
/// checksum tools and the built-in `wp` are (one round trip, read-only, no
/// sudo).
///
/// # Arguments
///
/// * `transport` - Where the server is
/// * `timeout` - How long the round trip may take
///
/// # Errors
///
/// A transport failure ([`Error::Ssh`], [`Error::Timeout`], …);
/// [`Error::Probe`] with [`ProbeFailure::Unreadable`] when an answer cannot
/// be read.
pub async fn probe_server(transport: &dyn Transport, timeout: Duration) -> Result<HostFacts> {
    let checks = host::login_checks(BUILTIN_WP_CLI);
    let steps = inspect::steps(&Identity::Login, &checks)?;
    let outputs = readable_steps(transport, steps, timeout, "H").await?;
    host::host_facts(&outputs).map_err(fail)
}

/// Run `steps` and split their outputs; a transport failure is the error,
/// an output that cannot be split the inner one.
///
/// # Arguments
///
/// * `transport` - Where they run
/// * `steps` - The statements
/// * `timeout` - How long the call may take
async fn run_steps(
    transport: &dyn Transport,
    steps: Steps,
    timeout: Duration,
) -> Result<std::result::Result<Vec<StepOutput>, StepsError>> {
    let nonce = Nonce::random();
    let count = steps.len();
    let script = Script::steps(nonce.clone(), &steps);
    let execution = call(transport, script, timeout).await?;
    Ok(split(&execution, &nonce, count))
}

/// [`run_steps`], with an output that cannot be split as
/// [`ProbeFailure::Unreadable`] of `step`.
///
/// # Arguments
///
/// * `transport` - Where they run
/// * `steps` - The statements
/// * `timeout` - How long the call may take
/// * `step` - The probe step, for messages
async fn readable_steps(
    transport: &dyn Transport,
    steps: Steps,
    timeout: Duration,
    step: &'static str,
) -> Result<Vec<StepOutput>> {
    run_steps(transport, steps, timeout)
        .await?
        .map_err(|error| {
            fail(ProbeFailure::Unreadable {
                step,
                reason: error.to_string(),
            })
        })
}

/// Each remote command's timeout for `site`: its `command_timeout_secs`,
/// else [`DEFAULT_TIMEOUT`].
///
/// # Arguments
///
/// * `site` - The site
pub fn site_timeout(site: &ResolvedSite) -> Duration {
    site.command_timeout_secs
        .map_or(DEFAULT_TIMEOUT, |secs| Duration::from_secs(secs.into()))
}

/// A [`ProbeFailure`] as an error.
///
/// # Arguments
///
/// * `failure` - The failure
fn fail(failure: ProbeFailure) -> Error {
    Error::Probe(Box::new(failure))
}

/// What step H produced.
struct Host {
    /// The login user and tools.
    facts: HostFacts,
    /// The uids of the users the settings name.
    users: Vec<(String, u32)>,
    /// The plugins directory.
    plugins_dir: PluginsDir,
}

/// The identity step I accepted, and what `cli version` said.
struct Chosen {
    /// The candidate.
    candidate: Candidate,
    /// The WP-CLI version.
    version: WpCliVersion,
    /// The candidates sudo refused before it.
    refused: Vec<Identity>,
}

/// What a candidate's exact-shape check gave.
enum Attempt {
    /// WP-CLI runs as it.
    Ready(WpCliVersion),
    /// sudo refused it.
    Refused(SudoSignal),
}

/// One probe of one site.
struct Prober<'a> {
    /// Where the server is.
    transport: &'a dyn Transport,
    /// The site.
    site: &'a ResolvedSite,
    /// Each step's timeout.
    timeout: Duration,
}

impl<'a> Prober<'a> {
    /// A probe of `site`.
    ///
    /// # Arguments
    ///
    /// * `transport` - Where the server is
    /// * `site` - The site
    fn new(transport: &'a dyn Transport, site: &'a ResolvedSite) -> Self {
        Self {
            transport,
            site,
            timeout: site_timeout(site),
        }
    }

    /// Every step, in order.
    async fn run(self) -> Result<SiteProbe> {
        let mut host = self.host().await?;
        let plan = self.plan(&host)?;
        let mode = host.plugins_dir.ownership.mode;
        let chosen = self.choose(&host.facts, &plan, mode).await?;
        let wp = self.wp_site(mode, chosen.candidate.uid)?;
        let identity = chosen.candidate.identity.clone();
        if host.plugins_dir.read_by_root {
            // Immediately before WordPress loads as the identity.
            host.plugins_dir.path = self.confirm_root_read(&host, &identity).await?;
        }
        let facts = self.wordpress(&wp, &identity, &chosen.version).await?;
        consistent(&host.plugins_dir, &facts.value, &identity)?;
        let (capabilities, mut warnings) =
            self.capabilities(&identity, &host, &facts.value).await?;
        warnings.extend(chosen_warnings(&chosen));
        Ok(SiteProbe {
            site_id: self.site.id.clone(),
            host: host.facts,
            plugins_dir: host.plugins_dir,
            identity: ProbedIdentity {
                identity,
                uid: chosen.candidate.uid,
                creation: chosen.candidate.creation,
                reference: plan.reference,
            },
            wp_cli_version: chosen.version,
            facts: facts.value,
            capabilities,
            warnings,
            notices: facts.notices,
            wp,
        })
    }

    /// Run `steps` and split their outputs.
    ///
    /// # Arguments
    ///
    /// * `steps` - The statements
    /// * `step` - The probe step, for messages
    async fn steps(&self, steps: Steps, step: &'static str) -> Result<Vec<StepOutput>> {
        readable_steps(self.transport, steps, self.timeout, step).await
    }

    /// Run `steps`; a transport failure is the error, an output that cannot
    /// be split is the inner one.
    ///
    /// # Arguments
    ///
    /// * `steps` - The statements
    async fn raw_steps(
        &self,
        steps: Steps,
    ) -> Result<std::result::Result<Vec<StepOutput>, StepsError>> {
        run_steps(self.transport, steps, self.timeout).await
    }

    /// Run a single statement and return its output.
    ///
    /// # Arguments
    ///
    /// * `steps` - One statement
    /// * `step` - The probe step, for messages
    async fn single(&self, steps: Steps, step: &'static str) -> Result<StepOutput> {
        let mut outputs = self.steps(steps, step).await?;
        outputs.pop().ok_or_else(|| {
            fail(ProbeFailure::Unreadable {
                step,
                reason: "no output".into(),
            })
        })
    }

    /// Step H.
    async fn host(&self) -> Result<Host> {
        let dir = &self.site.plugins_dir.value;
        let program = self.site.wp_cli.value.first().map_or("", String::as_str);
        let users = host::named_users(self.site);
        let checks = host::checks(program, dir, &users);
        let outputs = self
            .steps(inspect::steps(&Identity::Login, &checks)?, "H")
            .await?;
        let facts = host::host_facts(&outputs).map_err(fail)?;
        let (walk, stat) = host::dir_outputs(&outputs).map_err(fail)?;
        let read = host::dir_read(walk, stat).map_err(fail)?;
        let plugins_dir = self.plugins_dir(&facts, read).await?;
        Ok(Host {
            users: host::user_ids(&outputs, &users),
            facts,
            plugins_dir,
        })
    }

    /// Classify a sudo refusal, listing the one rule needed when sudo only
    /// said "a password is required".
    ///
    /// # Arguments
    ///
    /// * `facts` - The login user
    /// * `user` - The target user
    /// * `group` - The target group, if any
    /// * `signal` - What the refused command's stderr said
    /// * `command` - The listed command, and what the rule must allow
    async fn sudo_refusal(
        &self,
        facts: &HostFacts,
        user: &AccountName,
        group: Option<&AccountName>,
        signal: SudoSignal,
        command: (&'static str, &'static str),
    ) -> Result<SudoRefusal> {
        let listing = match signal {
            SudoSignal::Password => {
                let steps = inspect::sudo_listing_step(user, group, command.0);
                Some(self.single(steps, "I").await?)
            }
            SudoSignal::Tty | SudoSignal::NotAllowed => None,
        };
        Ok(SudoRefusal {
            kind: sudo::classify(signal, listing.as_ref()),
            login: facts
                .login
                .user
                .name
                .as_ref()
                .map(|n| n.as_str().to_string()),
            user: user.as_str().to_string(),
            group: group.map(|g| g.as_str().to_string()),
            command: command.1,
        })
    }

    /// The identity table for this site.
    ///
    /// # Arguments
    ///
    /// * `host` - Step H's results
    fn plan(&self, host: &Host) -> Result<IdentityPlan> {
        let facts = IdentityFacts {
            run_as: &self.site.run_as.value,
            owner: &self.site.owner.value,
            allow_root: self.site.allow_root.value,
            login: &host.facts.login,
            dir: &host.plugins_dir.ownership,
            tools: host.facts.switch_tools(),
            users: &host.users,
        };
        plan_identity(&facts).map_err(|refusal| fail(ProbeFailure::Identity(refusal)))
    }

    /// The site's WP-CLI base for an identity of `uid`.
    ///
    /// # Arguments
    ///
    /// * `mode` - The plugins directory's mode
    /// * `uid` - The identity's uid (`--allow-root` when 0)
    fn wp_site(&self, mode: FileMode, uid: u32) -> Result<WpSite> {
        let mut wp = WpSite::new(&self.site.wp_cli.value, mode, &self.site.path)?;
        if let Some(url) = &self.site.url {
            wp = wp.url(url)?;
        }
        Ok(if uid == 0 { wp.allow_root() } else { wp })
    }

    /// Step I: the first candidate whose exact command shape runs WP-CLI.
    ///
    /// # Arguments
    ///
    /// * `facts` - The login user
    /// * `plan` - The identity table's answer
    /// * `mode` - The plugins directory's mode
    async fn choose(
        &self,
        facts: &HostFacts,
        plan: &IdentityPlan,
        mode: FileMode,
    ) -> Result<Chosen> {
        let mut refused: Vec<(Identity, SudoSignal)> = Vec::new();
        for candidate in &plan.candidates {
            match self.attempt(candidate, mode).await? {
                Attempt::Ready(version) => {
                    let refused = refused.into_iter().map(|(identity, _)| identity).collect();
                    return Ok(Chosen {
                        candidate: candidate.clone(),
                        version,
                        refused,
                    });
                }
                Attempt::Refused(signal) => refused.push((candidate.identity.clone(), signal)),
            }
        }
        let (identity, signal) = refused.into_iter().next().ok_or_else(|| {
            fail(ProbeFailure::Unreadable {
                step: "I",
                reason: "no identity to try".into(),
            })
        })?;
        Err(fail(ProbeFailure::Sudo(
            self.identity_refusal(facts, &identity, signal).await?,
        )))
    }

    /// Classify sudo's refusal of `identity`.
    ///
    /// # Arguments
    ///
    /// * `facts` - The login user
    /// * `identity` - The refused identity (a sudo one)
    /// * `signal` - What sudo said
    async fn identity_refusal(
        &self,
        facts: &HostFacts,
        identity: &Identity,
        signal: SudoSignal,
    ) -> Result<SudoRefusal> {
        let Identity::Sudo { user, group } = identity else {
            return Err(fail(ProbeFailure::Unreadable {
                step: "I",
                reason: "a refusal of an identity without sudo".into(),
            }));
        };
        self.sudo_refusal(facts, user, group.as_ref(), signal, ("true", "ALL"))
            .await
    }

    /// Step I for one candidate: `cli version` in the exact command shape.
    ///
    /// # Arguments
    ///
    /// * `candidate` - The identity
    /// * `mode` - The plugins directory's mode
    async fn attempt(&self, candidate: &Candidate, mode: FileMode) -> Result<Attempt> {
        if let Some(signal) = self.check_primary_group(candidate).await? {
            return Ok(Attempt::Refused(signal));
        }
        let wp = self.wp_site(mode, candidate.uid)?;
        let command = wp.command(&WpCommand::cli_version())?;
        let steps = inspect::wp_step(&candidate.identity, &command);
        let out = self.single(steps, "I").await?;
        if out.success() {
            return self.version(&out).map(Attempt::Ready);
        }
        if !matches!(candidate.identity, Identity::Sudo { .. }) {
            return Err(fail(self.cannot_run(&candidate.identity, &out)));
        }
        if let Some(signal) = sudo::signal(&out) {
            return Ok(Attempt::Refused(signal));
        }
        if out.stderr_truncated {
            // Login noise past stderr's limit hid what sudo said.
            return Err(fail(ProbeFailure::Unreadable {
                step: "I",
                reason: "stderr was cut at its limit before sudo's message".into(),
            }));
        }
        Err(fail(self.cannot_run(&candidate.identity, &out)))
    }

    /// Refuse a sudo identity without `-g` whose primary group is gid 0,
    /// unless the site allows root: `id -g` as the identity, before WP-CLI
    /// runs as it (the table cannot know a sudo target's primary group).
    /// Only these candidates pay the extra round trip.
    ///
    /// # Arguments
    ///
    /// * `candidate` - The candidate about to be tried
    ///
    /// # Returns
    ///
    /// sudo's refusal of the identity, if it refused `id -g`.
    async fn check_primary_group(&self, candidate: &Candidate) -> Result<Option<SudoSignal>> {
        let no_group = matches!(candidate.identity, Identity::Sudo { group: None, .. });
        if !no_group || self.site.allow_root.value {
            return Ok(None);
        }
        let steps = inspect::steps(&candidate.identity, &[inspect::Inspect::Gid])?;
        let out = self.single(steps, "I").await?;
        if !out.success() {
            return match sudo::signal(&out) {
                Some(signal) => Ok(Some(signal)),
                None => Err(fail(primary_group_unreadable())),
            };
        }
        match out.line().map(str::trim).map(str::parse::<u32>) {
            Some(Ok(0)) => Err(fail(ProbeFailure::Identity(IdentityRefusal::OwnerIsRoot {
                role: RootRole::PrimaryGroup,
                account: Account::new(0, None),
            }))),
            Some(Ok(_)) => Ok(None),
            _ => Err(fail(primary_group_unreadable())),
        }
    }

    /// The version a successful `cli version` printed.
    ///
    /// # Arguments
    ///
    /// * `out` - Its output
    fn version(&self, out: &StepOutput) -> Result<WpCliVersion> {
        let output = CommandOutput::new(0, &out.stdout, &out.stderr);
        parse::cli_version(&output)
            .map(|parsed| parsed.value)
            .map_err(|error| {
                fail(self.wp_cli_missing(format!(
                    "is not WP-CLI: 'cli version' {}",
                    defect_text(&error)
                )))
            })
    }

    /// The failure of a `cli version` that exited non-zero.
    ///
    /// # Arguments
    ///
    /// * `identity` - Who ran it
    /// * `out` - Its output
    fn cannot_run(&self, identity: &Identity, out: &StepOutput) -> ProbeFailure {
        let message = last_line(&out.stderr);
        if matches!(identity, Identity::Runuser { .. }) && out.stderr_has("runuser:") {
            return ProbeFailure::SwitchFailed {
                identity: identity.clone(),
                message,
            };
        }
        let what = if out.status == 127 {
            "was not found"
        } else {
            "failed"
        };
        let message = message.map(|m| format!(": '{m}'")).unwrap_or_default();
        self.wp_cli_missing(format!("{what} (exit {}){message}", out.status))
    }

    /// A [`ProbeFailure::WpCliMissing`] for this site.
    ///
    /// # Arguments
    ///
    /// * `reason` - What went wrong
    fn wp_cli_missing(&self, reason: String) -> ProbeFailure {
        ProbeFailure::WpCliMissing {
            program: self.site.wp_cli.value.clone(),
            reason,
        }
    }

    /// Step W: WordPress's facts, as the identity, with WP-CLI's own
    /// failures classified.
    ///
    /// # Arguments
    ///
    /// * `wp` - The site's WP-CLI base
    /// * `identity` - Who runs it
    /// * `version` - What `cli version` printed
    async fn wordpress(
        &self,
        wp: &WpSite,
        identity: &Identity,
        version: &WpCliVersion,
    ) -> Result<Parsed<SiteFacts>> {
        let command = wp.command(&WpCommand::site_facts())?;
        let out = self
            .single(inspect::wp_step(identity, &command), "W")
            .await?;
        let output = CommandOutput::new(out.status, &out.stdout, &out.stderr);
        let facts = parse::site_facts(&output)
            .map_err(|error| self.wordpress_failure(&output).map_or(error, fail))?;
        if &facts.value.wp_cli_version != version {
            return Err(fail(self.wp_cli_missing(format!(
                "is not one WP-CLI: 'cli version' printed {version}, WordPress's WP_CLI_VERSION \
                 is {}",
                facts.value.wp_cli_version
            ))));
        }
        Ok(facts)
    }

    /// `not_wordpress` or `wp_not_installed` from WP-CLI's own message, if
    /// step W failed with one.
    ///
    /// # Arguments
    ///
    /// * `output` - Step W's output
    fn wordpress_failure(&self, output: &CommandOutput<'_>) -> Option<ProbeFailure> {
        if output.exit_code() == 0 {
            return None;
        }
        let message = output.error_lines().into_iter().next();
        let path = self.site.path.clone();
        let says = |phrase: &str| {
            let wanted = format!("Error: {phrase}");
            output
                .stderr()
                .split(|&byte| byte == b'\n')
                .any(|line| line.starts_with(wanted.as_bytes()))
        };
        if says(NOT_WORDPRESS) {
            return Some(ProbeFailure::NotWordpress { path, message });
        }
        NOT_INSTALLED
            .iter()
            .any(|phrase| says(phrase))
            .then_some(ProbeFailure::WpNotInstalled { path, message })
    }

    /// Step C, and the warnings of the identity and the facts.
    ///
    /// # Arguments
    ///
    /// * `identity` - Who runs the checks
    /// * `host` - Step H's results
    /// * `facts` - Step W's facts
    async fn capabilities(
        &self,
        identity: &Identity,
        host: &Host,
        facts: &SiteFacts,
    ) -> Result<(Capabilities, Vec<ProbeWarning>)> {
        let paths = CheckPaths::new(
            &host.plugins_dir.path,
            &self.site.remote_tmp_dir.value,
            facts.content_dir_realpath.as_deref(),
        );
        let read = self
            .raw_steps(inspect::steps(identity, &paths.checks())?)
            .await?;
        let (capabilities, mut warnings) = match read {
            Ok(outputs) => paths.read(&outputs),
            Err(error) => paths.unknown(&error.to_string()),
        };
        if host.facts.checksum.is_none() {
            warnings.push(ProbeWarning::NoChecksumTool);
        }
        if let Some(url) = &self.site.url
            && url.trim_end_matches('/') != facts.siteurl.trim_end_matches('/')
        {
            warnings.push(ProbeWarning::SiteurlDiffers {
                siteurl: facts.siteurl.clone(),
                url: url.clone(),
            });
        }
        Ok((capabilities, warnings))
    }
}

/// `id -g` as the identity printed no group id.
fn primary_group_unreadable() -> ProbeFailure {
    ProbeFailure::Unreadable {
        step: "I",
        reason: "id -g as the identity printed no group id".into(),
    }
}

/// The warnings of the chosen identity.
///
/// # Arguments
///
/// * `chosen` - Step I's result
fn chosen_warnings(chosen: &Chosen) -> Vec<ProbeWarning> {
    let mut warnings: Vec<ProbeWarning> = chosen
        .refused
        .iter()
        .map(|refused| ProbeWarning::IdentityFallback {
            refused: refused.clone(),
        })
        .collect();
    if chosen.candidate.creation == Creation::CheckAfter {
        warnings.push(ProbeWarning::CheckedAfterCreation);
    }
    warnings
}

/// Step O: WordPress must use the plugins directory step H measured.
///
/// # Arguments
///
/// * `dir` - Step H's directory
/// * `facts` - Step W's facts
/// * `identity` - The identity used
fn consistent(dir: &PluginsDir, facts: &SiteFacts, identity: &Identity) -> Result<()> {
    if facts.plugin_dir_realpath.as_deref() == Some(dir.path.as_str()) {
        return Ok(());
    }
    Err(fail(ProbeFailure::PluginsDirMismatch {
        expected: dir.path.clone(),
        found: facts.plugin_dir_realpath.clone(),
        identity: identity.clone(),
    }))
}

/// What a parser said about `cli version`'s output.
///
/// # Arguments
///
/// * `error` - The parser's error
fn defect_text(error: &Error) -> String {
    match error {
        Error::WpOutput { defect, .. } => defect.to_string(),
        other => other.to_string(),
    }
}

#[cfg(all(test, unix))]
mod hermetic;
#[cfg(test)]
pub(crate) mod tests;
