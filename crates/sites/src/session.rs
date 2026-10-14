//! A session with one site (plan §8, P4.2): the site probed once, then
//! every WP-CLI command run as its identity through the same transport,
//! with transport failures turned into errors and the parsers' inputs
//! built faithfully.
//!
//! - [`Session::open`] probes the site ([`probe`](crate::wp::probe::probe));
//!   every command then runs behind the probed identity, with the site's
//!   hermetic WP-CLI base (`--allow-root` exactly when its uid is 0).
//! - [`Session::run`] returns a [`CommandRun`] whose
//!   [`output`](CommandRun::output) carries the transport's truncation flag
//!   ([`CommandOutput::with_stdout_truncated`]), so a parser never reads a
//!   cut stdout as complete. A call that did not run to its end is an
//!   error: `ssh_*` (after the script started, `ssh_connection_lost`),
//!   `timeout` (saying whether the script started), `ssh_failed` when the
//!   login shell did not run the script.
//! - A plugin list read here is bound to its session ([`SitePlugins`]),
//!   and commands that name a plugin run only through
//!   [`Session::run_on`] with a [`SiteTarget`] of this session's lists:
//!   [`Session::run`] refuses them, so a plugin resolved on another site,
//!   or in a list parsed outside a session, never reaches this site's
//!   command. ([`Script`] and [`Transport`] stay public for other
//!   transports' tests; code outside a session is outside this guard.)
//!
//! Retries of reads on transport failures (plan §6.1 item 8) and the site
//! lock (§6.13) belong to the runner and the executor (P5.1, P6.4).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::inventory::ResolvedSite;
use crate::problem::{Problem, quoted};
use crate::remote::call::{call, framed_stdout};
use crate::remote::{Exit, Identity, Nonce, Script, Transport};
use crate::wp::parse::{self, CommandOutput, Notices};
use crate::wp::probe::{self, SiteProbe};
use crate::wp::{
    ActivationScope, CommandKind, DeactivateMode, PluginList, Resolved, Slug, WpCommand,
};

/// Hands every session a distinct token.
static TOKENS: AtomicU64 = AtomicU64::new(1);

/// A probed site and the transport to its server.
pub struct Session<'t> {
    /// Where the server is.
    transport: &'t dyn Transport,
    /// What the probe learned (identity, WP-CLI base).
    probe: SiteProbe,
    /// Each command's timeout.
    timeout: Duration,
    /// This session's token, carried by the plugin lists it reads.
    token: u64,
}

impl std::fmt::Debug for Session<'_> {
    /// The site and its identity (the transport has no `Debug`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("site", &self.probe.site_id)
            .field("identity", &self.probe.identity.identity)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl<'t> Session<'t> {
    /// Probe `site` and open a session with it.
    ///
    /// # Arguments
    ///
    /// * `transport` - Where the site's server is
    /// * `site` - The site, resolved
    ///
    /// # Errors
    ///
    /// The probe's errors ([`probe::probe`]).
    pub async fn open(transport: &'t dyn Transport, site: &ResolvedSite) -> Result<Self> {
        let probe = probe::probe(transport, site).await?;
        Ok(Self::new(transport, probe, probe::site_timeout(site)))
    }

    /// A session from a probe of the site through `transport`. Crate-only:
    /// a public caller opens sessions with [`Session::open`], which probes
    /// through the transport the session then uses, so a probe (an
    /// identity) is never paired with another server's transport.
    ///
    /// # Arguments
    ///
    /// * `transport` - Where the site's server is
    /// * `probe` - The site's probe, made through `transport`
    /// * `timeout` - Each command's timeout
    pub(crate) fn new(transport: &'t dyn Transport, probe: SiteProbe, timeout: Duration) -> Self {
        Self {
            transport,
            probe,
            timeout,
            token: TOKENS.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// What the probe learned.
    pub fn probe(&self) -> &SiteProbe {
        &self.probe
    }

    /// The site's id.
    pub fn site_id(&self) -> &str {
        &self.probe.site_id
    }

    /// Who every command runs as.
    pub fn identity(&self) -> &Identity {
        &self.probe.identity.identity
    }

    /// Run `command` on the site as its identity.
    ///
    /// # Arguments
    ///
    /// * `command` - The WP-CLI command
    ///
    /// # Returns
    ///
    /// The run, whatever its exit status: parse
    /// [`CommandRun::output`] with the command's parser.
    ///
    /// # Errors
    ///
    /// A transport failure (`ssh_*`, `timeout`, `ssh_failed`,
    /// `process_failed`), or [`Error::InvalidInput`] if the command cannot
    /// be built (never for this crate's commands).
    pub async fn run(&self, command: &WpCommand) -> Result<CommandRun> {
        if command.kind().targets_plugin() {
            return Err(Error::InvalidInput {
                subject: "session command".into(),
                problems: vec![Problem::new(
                    "command",
                    format!(
                        "'wp {}' names a plugin: run it with run_on and a target this \
                         session resolved",
                        command.kind().label()
                    ),
                )],
            });
        }
        self.execute(command).await
    }

    /// Run `op` on `target`, a plugin resolved in one of this session's
    /// lists, as the site's identity.
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin, bound to this session
    /// * `op` - What to run on it
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when another session resolved `target`;
    /// otherwise [`Session::run`]'s errors.
    pub async fn run_on(&self, target: &SiteTarget<'_>, op: TargetOp) -> Result<CommandRun> {
        let resolved = self.target(target)?;
        self.execute(&op.command(resolved)).await
    }

    /// Run any command as the site's identity.
    ///
    /// # Arguments
    ///
    /// * `command` - The WP-CLI command
    async fn execute(&self, command: &WpCommand) -> Result<CommandRun> {
        let wp = self.probe.wp().command(command)?;
        let nonce = Nonce::random();
        let script = Script::wp(nonce.clone(), self.identity(), &wp);
        let execution = call(self.transport, script, self.timeout).await?;
        let Exit::Status(exit_code) = execution.exit() else {
            return Err(Error::Killed {
                exit: execution.exit(),
            });
        };
        Ok(CommandRun {
            kind: command.kind(),
            exit_code,
            stdout: framed_stdout(&execution, &nonce).to_vec(),
            stdout_truncated: execution.stdout().is_truncated(),
            stderr: execution.stderr().bytes().to_vec(),
            elapsed: execution.elapsed(),
        })
    }

    /// The site's plugins (`plugin list`, plan §6.4), bound to this
    /// session.
    ///
    /// # Errors
    ///
    /// [`Session::run`]'s errors; [`Error::WpOutput`] when the output
    /// cannot be read with certainty.
    pub async fn plugin_list(&self) -> Result<SitePlugins> {
        let run = self.run(&WpCommand::plugin_list()).await?;
        let parsed = parse::plugin_list(&run.output())?;
        Ok(self.bind(parsed.value, parsed.notices))
    }

    /// The site's regular plugins as `get_plugins()` sees them, none hidden
    /// by a must-use plugin or drop-in (resolves a
    /// [`Lookup::Shadowed`](crate::wp::Lookup::Shadowed)), bound to this
    /// session.
    ///
    /// # Errors
    ///
    /// As [`Session::plugin_list`].
    pub async fn regular_plugins(&self) -> Result<SitePlugins> {
        let run = self.run(&WpCommand::regular_plugins()).await?;
        let parsed = parse::regular_plugins(&run.output())?;
        Ok(self.bind(parsed.value, parsed.notices))
    }

    /// The plugin `target` names, when this session read it.
    ///
    /// # Arguments
    ///
    /// * `target` - A plugin resolved in one of this session's lists
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when another session (another site, or
    /// another probe of it) resolved it.
    pub fn target<'a, 'b>(&self, target: &'b SiteTarget<'a>) -> Result<&'b Resolved<'a>> {
        if target.token != self.token {
            return Err(Error::InvalidInput {
                subject: "plugin target".into(),
                problems: vec![Problem::new(
                    "target",
                    format!(
                        "{} was resolved in a plugin list of another session, not site {}'s",
                        quoted(target.resolved.slug().as_str()),
                        quoted(self.site_id())
                    ),
                )],
            });
        }
        Ok(&target.resolved)
    }

    /// `list` bound to this session.
    ///
    /// # Arguments
    ///
    /// * `list` - The parsed list
    /// * `notices` - The PHP notices around it
    fn bind(&self, list: PluginList, notices: Notices) -> SitePlugins {
        SitePlugins {
            token: self.token,
            site_id: self.site_id().to_string(),
            list,
            notices,
        }
    }
}

/// One WP-CLI command's run on a site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRun {
    /// What ran.
    kind: CommandKind,
    /// Its exit status (a command's 255 arrives as 254).
    exit_code: i32,
    /// stdout after the nonce line.
    stdout: Vec<u8>,
    /// Whether the transport cut stdout at its limit.
    stdout_truncated: bool,
    /// stderr (login noise included).
    stderr: Vec<u8>,
    /// How long it took.
    elapsed: Duration,
}

impl CommandRun {
    /// The parsers' input, with the transport's truncation flag.
    pub fn output(&self) -> CommandOutput<'_> {
        CommandOutput::new(self.exit_code, &self.stdout, &self.stderr)
            .with_stdout_truncated(self.stdout_truncated)
    }

    /// What ran.
    pub fn kind(&self) -> CommandKind {
        self.kind
    }

    /// Its exit status.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// How long it took.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// A plugin list read in one session, bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitePlugins {
    /// The session's token.
    token: u64,
    /// The site's id.
    site_id: String,
    /// The plugins.
    list: PluginList,
    /// The PHP notices around it.
    notices: Notices,
}

impl SitePlugins {
    /// The site's id.
    pub fn site_id(&self) -> &str {
        &self.site_id
    }

    /// The plugins.
    pub fn list(&self) -> &PluginList {
        &self.list
    }

    /// The PHP notices around the list.
    pub fn notices(&self) -> &Notices {
        &self.notices
    }

    /// `slug` resolved for a command ([`PluginList::resolve`]), bound to
    /// the session that read this list.
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin
    pub fn resolve(&self, slug: &Slug) -> Option<SiteTarget<'_>> {
        let resolved = self.list.resolve(slug)?;
        Some(SiteTarget {
            token: self.token,
            resolved,
        })
    }
}

/// What [`Session::run_on`] runs on a bound plugin: every [`WpCommand`]
/// that names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TargetOp {
    /// [`WpCommand::requires_plugins`].
    RequiresPlugins,
    /// [`WpCommand::activate`].
    Activate(ActivationScope),
    /// [`WpCommand::deactivate`].
    Deactivate(ActivationScope, DeactivateMode),
    /// [`WpCommand::uninstall`].
    Uninstall,
    /// [`WpCommand::reset`].
    Reset,
    /// [`WpCommand::delete`].
    Delete,
}

impl TargetOp {
    /// The command for `target`.
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin
    fn command(self, target: &Resolved<'_>) -> WpCommand {
        match self {
            Self::RequiresPlugins => WpCommand::requires_plugins(target),
            Self::Activate(scope) => WpCommand::activate(target, scope),
            Self::Deactivate(scope, mode) => WpCommand::deactivate(target, scope, mode),
            Self::Uninstall => WpCommand::uninstall(target),
            Self::Reset => WpCommand::reset(target),
            Self::Delete => WpCommand::delete(target),
        }
    }
}

/// A plugin resolved in one session's list: commands take it through
/// [`Session::run_on`] (or [`Session::target`]), which only that session
/// accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteTarget<'a> {
    /// The session's token.
    token: u64,
    /// The plugin.
    resolved: Resolved<'a>,
}

impl SiteTarget<'_> {
    /// The plugin's slug.
    pub fn slug(&self) -> &Slug {
        self.resolved.slug()
    }
}

#[cfg(test)]
mod tests;
