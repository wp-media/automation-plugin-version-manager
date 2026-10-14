//! Probe step H (plan §6.2), pure: what the login user is asked, and how
//! its answers are read. Ids decide; names count only when they pass the
//! user rule ([`AccountName`]).

use crate::allowlist;
use crate::inventory::{Owner, ResolvedSite, RunAs};
use crate::problem::{printable, quoted};
use crate::remote::call::last_line;
use crate::remote::inspect::{Inspect, StatFlavor, WALK_UNREACHABLE, walk_refused};
use crate::remote::steps::StepOutput;
use crate::remote::{Account, AccountName, DirOwnership, FileMode, LoginAccount, SwitchTools};

use super::failure::ProbeFailure;

/// The tools step H looks for, in its order after the `id` commands.
const TOOLS: [&str; 5] = ["sudo", "runuser", "sha256sum", "shasum", "openssl"];
/// Where each answer is in step H's outputs.
mod at {
    /// `id -u`.
    pub(super) const UID: usize = 0;
    /// `id -g`.
    pub(super) const GID: usize = 1;
    /// `id -un`.
    pub(super) const USER_NAME: usize = 2;
    /// `id -gn`.
    pub(super) const GROUP_NAME: usize = 3;
    /// `id -G`.
    pub(super) const GROUPS: usize = 4;
    /// `printenv SHELL`.
    pub(super) const SHELL: usize = 5;
    /// The first `command -v` of [`super::TOOLS`].
    pub(super) const TOOLS: usize = 6;
    /// `command -v <wp_cli program>`.
    pub(super) const WP_CLI: usize = TOOLS + super::TOOLS.len();
    /// The plugins directory walk.
    pub(super) const WALK: usize = WP_CLI + 1;
    /// `stat -L` of the plugins directory: only says why the walk failed.
    pub(super) const STAT: usize = WALK + 1;
    /// The first `id -u <name>`.
    pub(super) const USERS: usize = STAT + 1;
}

/// The step name in [`ProbeFailure::Unreadable`].
const STEP: &str = "H";
/// What `stat` prints for an id without a name (GNU, busybox).
const UNKNOWN_NAME: &str = "UNKNOWN";

/// What step H learned about the login user and the server's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostFacts {
    /// The login user, its groups, and their usable names.
    pub login: LoginAccount,
    /// `$SHELL`, escaped, when set.
    pub shell: Option<String>,
    /// Where `sudo` is, if installed.
    pub sudo: Option<String>,
    /// Where `runuser` is, if installed.
    pub runuser: Option<String>,
    /// The first checksum tool found: `sha256sum`, `shasum` or `openssl`
    /// (plan §6.6).
    pub checksum: Option<String>,
    /// Where the login user finds the `wp_cli` program (informational: the
    /// identity's `PATH` may differ; step I decides).
    pub wp_cli_path: Option<String>,
}

impl HostFacts {
    /// What the server has to switch users.
    pub fn switch_tools(&self) -> SwitchTools {
        SwitchTools {
            sudo: self.sudo.is_some(),
            runuser: self.runuser.is_some(),
        }
    }
}

/// The names `run_as` and `owner` give, which step H resolves with
/// `id -u <name>` (names that break the user rule are left out: the
/// identity table then reports them unknown).
///
/// # Arguments
///
/// * `site` - The site
pub(super) fn named_users(site: &ResolvedSite) -> Vec<AccountName> {
    let run_as = match &site.run_as.value {
        RunAs::User(name) => Some(name.as_str()),
        _ => None,
    };
    let owner = match &site.owner.value {
        Owner::Explicit { user, .. } => Some(user.as_str()),
        Owner::Auto => None,
    };
    let mut names: Vec<AccountName> = Vec::new();
    for name in [run_as, owner].into_iter().flatten() {
        if let Ok(name) = AccountName::new(name)
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// The login user's commands: its ids, names, groups and shell, the
/// tools, and `command -v` of `wp_cli`; the start of step H, in the order
/// [`at`] reads them, and all of a server's test
/// ([`probe_server`](super::probe_server), which [`host_facts`] reads too).
///
/// # Arguments
///
/// * `wp_cli` - The `wp_cli` program (its first token)
pub(super) fn login_checks(wp_cli: &str) -> Vec<Inspect<'_>> {
    let mut checks = vec![
        Inspect::Uid,
        Inspect::Gid,
        Inspect::UserName,
        Inspect::GroupName,
        Inspect::Groups,
        Inspect::Shell,
    ];
    checks.extend(TOOLS.iter().map(|tool| Inspect::Which(tool)));
    checks.push(Inspect::Which(wp_cli));
    checks
}

/// Step H's commands, in the order [`at`] reads them: [`login_checks`],
/// then the plugins directory and the named users.
///
/// # Arguments
///
/// * `wp_cli` - The `wp_cli` program (its first token)
/// * `dir` - The plugins directory
/// * `users` - The names to resolve
pub(super) fn checks<'a>(
    wp_cli: &'a str,
    dir: &'a str,
    users: &'a [AccountName],
) -> Vec<Inspect<'a>> {
    let mut checks = login_checks(wp_cli);
    checks.extend([
        Inspect::Walk {
            path: dir,
            login_uid: None,
            flavor: StatFlavor::Gnu,
        },
        Inspect::StatGnu(dir),
    ]);
    checks.extend(users.iter().map(Inspect::UserId));
    checks
}

/// The output at `index`.
///
/// # Arguments
///
/// * `steps` - Step H's outputs
/// * `index` - Which one
fn output(steps: &[StepOutput], index: usize) -> Result<&StepOutput, ProbeFailure> {
    steps
        .get(index)
        .ok_or_else(|| unreadable("a command's output is missing"))
}

/// The walk's and the diagnostic `stat`'s outputs.
///
/// # Arguments
///
/// * `steps` - Step H's outputs
///
/// # Errors
///
/// [`ProbeFailure::Unreadable`] when one is missing.
pub(super) fn dir_outputs(
    steps: &[StepOutput],
) -> Result<(&StepOutput, &StepOutput), ProbeFailure> {
    Ok((output(steps, at::WALK)?, output(steps, at::STAT)?))
}

/// Read the login user and the tools.
///
/// # Arguments
///
/// * `steps` - Step H's outputs
///
/// # Errors
///
/// [`ProbeFailure::Unreadable`] when an id cannot be read.
pub(super) fn host_facts(steps: &[StepOutput]) -> Result<HostFacts, ProbeFailure> {
    let uid = number(output(steps, at::UID)?, "id -u")?;
    let gid = number(output(steps, at::GID)?, "id -g")?;
    let groups = groups(output(steps, at::GROUPS)?, gid)?;
    let login = LoginAccount {
        user: Account::new(uid, name(output(steps, at::USER_NAME)?)),
        group: Account::new(gid, name(output(steps, at::GROUP_NAME)?)),
        groups,
    };
    let found = |index: usize| -> Result<Option<String>, ProbeFailure> {
        Ok(found_line(output(steps, index)?))
    };
    let tool = |offset: usize| found(at::TOOLS + offset);
    let checksum = [tool(2)?, tool(3)?, tool(4)?].into_iter().flatten().next();
    Ok(HostFacts {
        login,
        shell: found(at::SHELL)?,
        sudo: tool(0)?,
        runuser: tool(1)?,
        checksum,
        wp_cli_path: found(at::WP_CLI)?,
    })
}

/// The uids step H resolved for `users` (missing users left out).
///
/// # Arguments
///
/// * `steps` - Step H's outputs
/// * `users` - The names, in [`checks`]' order
pub(super) fn user_ids(steps: &[StepOutput], users: &[AccountName]) -> Vec<(String, u32)> {
    users
        .iter()
        .enumerate()
        .filter_map(|(offset, name)| {
            let step = steps.get(at::USERS + offset)?;
            let uid = number(step, "id -u <name>").ok()?;
            Some((name.as_str().to_string(), uid))
        })
        .collect()
}

/// How the plugins directory read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DirRead {
    /// A directory reached without a link the site could plant: its
    /// physical path and its ownership, from the same walk.
    Found {
        /// `pwd -P` at the end of the walk.
        path: String,
        /// Owner, group and mode.
        ownership: DirOwnership,
    },
    /// The login user may not reach it (`Permission denied`).
    Denied,
    /// It does not exist.
    Missing,
    /// The walk met a link the site could have planted or replaced, or
    /// landed elsewhere than the path says.
    Linked {
        /// The walk's message, escaped.
        detail: Option<String>,
    },
    /// It is something else than a directory.
    NotDirectory {
        /// What `stat` called it, escaped.
        found: String,
    },
    /// `stat -c` was refused as an option: a BSD `stat`, retried with `-f`.
    BsdStat,
    /// It could not be read otherwise.
    Other {
        /// The last message, escaped.
        message: Option<String>,
    },
}

/// The plugins directory from step H's walk, or why it failed (from the
/// walk's status and the diagnostic `stat -L`). The identity comes from
/// the walked directory's owner, and the walk follows no link the site
/// could plant ([`Inspect::Walk`]), so the site cannot hand the identity
/// to the owner of another directory.
///
/// # Arguments
///
/// * `walk` - The walk's output
/// * `stat` - The diagnostic `stat -L`'s output
///
/// # Errors
///
/// [`ProbeFailure::Unreadable`] when the walk answered but its output
/// cannot be read.
pub(super) fn dir_read(walk: &StepOutput, stat: &StepOutput) -> Result<DirRead, ProbeFailure> {
    if walk.success() {
        return walked(walk);
    }
    if walk_refused(walk.status) {
        return Ok(DirRead::Linked {
            detail: last_line(&walk.stderr),
        });
    }
    if !stat.success() {
        return Ok(failed_read(stat));
    }
    let lines = stat_lines(stat)?;
    if !lines.is_directory() {
        return Ok(DirRead::NotDirectory {
            found: printable(&lines.kind),
        });
    }
    // The directory exists, but the walk could not enter a component:
    // dash says only "can't cd to plugins" for a 0750 directory, so read it
    // as denied and let root's `stat` read it (audit 3).
    if walk.status == WALK_UNREACHABLE {
        return Ok(DirRead::Denied);
    }
    Ok(DirRead::Other {
        message: last_line(&walk.stderr),
    })
}

/// A successful walk's answer: the physical path, then `stat`'s six lines.
///
/// # Arguments
///
/// * `walk` - The walk's output
///
/// # Errors
///
/// [`ProbeFailure::Unreadable`] for any other shape.
pub(super) fn walked(walk: &StepOutput) -> Result<DirRead, ProbeFailure> {
    let split = walk.stdout.iter().position(|&byte| byte == b'\n');
    let (path, rest) = match split {
        Some(at) => walk.stdout.split_at(at),
        None => return Err(unreadable("the walk printed no path")),
    };
    let path = std::str::from_utf8(path).map_err(|_| unreadable("the walk printed non-UTF-8"))?;
    allowlist::remote_path(path).map_err(|reason| {
        unreadable(&format!(
            "the walk printed {}, which {reason}",
            quoted(path)
        ))
    })?;
    let lines = stat_text(rest.get(1..).unwrap_or_default())?;
    if !lines.is_directory() {
        return Err(unreadable(
            "the walk ended in something else than a directory",
        ));
    }
    Ok(DirRead::Found {
        path: path.to_string(),
        ownership: lines.ownership(),
    })
}

/// How a failed `stat` failed.
///
/// # Arguments
///
/// * `step` - Its output
pub(super) fn failed_read(step: &StepOutput) -> DirRead {
    if step.stderr_has("Permission denied") {
        return DirRead::Denied;
    }
    if step.stderr_has("No such file or directory") {
        return DirRead::Missing;
    }
    // GNU: "invalid option"; BSD and macOS: "illegal option -- c".
    if step.stderr_has("illegal option") || step.stderr_has("invalid option") {
        return DirRead::BsdStat;
    }
    DirRead::Other {
        message: last_line(&step.stderr),
    }
}

/// The six lines of `stat`'s ownership format (GNU, busybox or BSD).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatLines {
    /// The owner.
    owner: Account,
    /// The group.
    group: Account,
    /// The mode.
    mode: FileMode,
    /// The file type: `directory` (GNU), `Directory` (BSD), `symbolic
    /// link`, …
    kind: String,
}

impl StatLines {
    /// Whether the file type is a directory.
    pub(super) fn is_directory(&self) -> bool {
        self.kind.eq_ignore_ascii_case("directory")
    }

    /// The file type as `stat` printed it.
    pub(super) fn kind(&self) -> &str {
        &self.kind
    }

    /// Owner, group and mode.
    pub(super) fn ownership(&self) -> DirOwnership {
        DirOwnership {
            owner: self.owner.clone(),
            group: self.group.clone(),
            mode: self.mode,
        }
    }
}

/// Read `stat`'s six lines: uid, gid, octal mode, type, owner name, group
/// name (`UNKNOWN`, or a BSD `stat`'s bare id: no name).
///
/// # Arguments
///
/// * `step` - `stat`'s output
///
/// # Errors
///
/// [`ProbeFailure::Unreadable`] for any other shape.
pub(super) fn stat_lines(step: &StepOutput) -> Result<StatLines, ProbeFailure> {
    stat_text(&step.stdout)
}

/// [`stat_lines`] of `stdout`.
///
/// # Arguments
///
/// * `stdout` - The six lines
fn stat_text(stdout: &[u8]) -> Result<StatLines, ProbeFailure> {
    let text = std::str::from_utf8(stdout).map_err(|_| unreadable("stat printed non-UTF-8"))?;
    let lines: Vec<&str> = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect();
    let [uid, gid, mode, kind, user, group] = lines.as_slice() else {
        return Err(unreadable(&format!(
            "stat printed {} lines instead of 6",
            lines.len()
        )));
    };
    let id =
        |text: &str| parse_u32(text).ok_or_else(|| unreadable("stat printed a non-numeric id"));
    let mode =
        FileMode::from_octal(mode).map_err(|_| unreadable("stat printed an invalid mode"))?;
    Ok(StatLines {
        owner: Account::new(id(uid)?, stat_name(user)),
        group: Account::new(id(gid)?, stat_name(group)),
        mode,
        kind: (*kind).to_string(),
    })
}

/// A name `stat` printed: none for `UNKNOWN`, or one that breaks the user
/// rule (a BSD `stat` prints the id itself, all digits).
///
/// # Arguments
///
/// * `text` - The line
fn stat_name(text: &str) -> Option<AccountName> {
    if text == UNKNOWN_NAME {
        return None;
    }
    AccountName::new(text).ok()
}

/// A name `id -un`/`id -gn` printed, when it exited 0 and passes the user
/// rule.
///
/// # Arguments
///
/// * `step` - Its output
fn name(step: &StepOutput) -> Option<AccountName> {
    if !step.success() {
        return None;
    }
    AccountName::new(step.line()?).ok()
}

/// A decimal id from a command that must succeed.
///
/// # Arguments
///
/// * `step` - Its output
/// * `command` - The command, for the message
fn number(step: &StepOutput, command: &str) -> Result<u32, ProbeFailure> {
    if !step.success() {
        return Err(unreadable(&format!("{command} exited {}", step.status)));
    }
    step.line()
        .and_then(parse_u32)
        .ok_or_else(|| unreadable(&format!("{command} printed no decimal id")))
}

/// The login user's groups (`id -G`); its primary group alone when `id -G`
/// failed.
///
/// # Arguments
///
/// * `step` - `id -G`'s output
/// * `gid` - The primary group
fn groups(step: &StepOutput, gid: u32) -> Result<Vec<u32>, ProbeFailure> {
    if !step.success() {
        return Ok(vec![gid]);
    }
    let text = step
        .line()
        .ok_or_else(|| unreadable("id -G printed non-UTF-8"))?;
    text.split_whitespace()
        .map(|id| parse_u32(id).ok_or_else(|| unreadable("id -G printed a non-numeric id")))
        .collect()
}

/// The line a command printed when it succeeded and printed one, escaped.
///
/// # Arguments
///
/// * `step` - Its output
fn found_line(step: &StepOutput) -> Option<String> {
    let line = step.line()?.trim();
    (step.success() && !line.is_empty()).then(|| printable(line))
}

/// 1 to 10 ASCII digits as a `u32`.
///
/// # Arguments
///
/// * `text` - The digits
fn parse_u32(text: &str) -> Option<u32> {
    let digits = (1..=10).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten()
}

/// An unreadable step H.
///
/// # Arguments
///
/// * `reason` - What is wrong
fn unreadable(reason: &str) -> ProbeFailure {
    ProbeFailure::Unreadable {
        step: STEP,
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests;
