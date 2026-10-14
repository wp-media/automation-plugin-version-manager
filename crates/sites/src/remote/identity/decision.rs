//! The identity table (plan §6.2): who WP-CLI runs as on a site, decided
//! from what probe step H measured. Pure: no I/O.
//!
//! The reference `(U, G, P)` is the plugins directory's owner, group and
//! mode, or the site's explicit `owner` (which must then own that
//! directory). The identity is the login user `L`, `sudo -n -u U [-g G]`,
//! or `runuser -u U -g G` for a root login:
//!
//! | `run_as` | Condition | Candidates, in order |
//! |---|---|---|
//! | `auto` | `U == L`, `G == Lg` | none (at creation) |
//! | `auto` | `U == L`, `G ≠ Lg` | `sudo -u L -g G` (at creation), then none (checked after); none alone on a setgid directory whose group `L` is in |
//! | `auto` | `U ≠ L`, `L` is root, runuser | `runuser -u U -g G` (at creation) |
//! | `auto` | `U ≠ L`, sudo | `sudo -u U -g G` (at creation), then `sudo -u U` (checked after) |
//! | `auto` | `U ≠ L`, no sudo (nor runuser for root) | refused: `sudo_missing` |
//! | any | `U` or the identity is root (uid 0), without `allow_root` | refused: `owner_is_root` |
//! | any | `G` is gid 0, without `allow_root` | no `-g`: `sudo -u U` (checked after); a root login without sudo is refused (`runuser` needs `-g`) |
//! | `login` | — | none (at creation only when `U == L` and `G == Lg`) |
//! | `<user>` | — | as `auto`, with `U := <user>` for the prefix; the reference stays the directory's (or `owner`'s), so files are checked after unless the user is the owner |
//!
//! Root is decided by **ids**, never by name (`toor` is uid 0 too). Names
//! come from the server and are used only when they pass the user rule
//! ([`AccountName`]); an account without one cannot be switched to.
//! Which candidate works is the probe's exact-shape check (step I): the
//! first one sudo accepts wins.

use super::{AccountName, Identity};
use crate::error::Code;
use crate::inventory::{Owner, RunAs};
use crate::problem::quoted;
use crate::remote::FileMode;

/// An account on the server: its numeric id and, when it has one that
/// passes the user rule, its name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Account {
    /// The user or group id.
    pub id: u32,
    /// Its name, if it has one apvm can use (`stat` printed `UNKNOWN`, a
    /// BSD `stat` printed the id, or the name breaks the user rule: none).
    pub name: Option<AccountName>,
}

impl Account {
    /// An account of `id`, named `name` if any.
    ///
    /// # Arguments
    ///
    /// * `id` - The id
    /// * `name` - Its usable name
    pub fn new(id: u32, name: Option<AccountName>) -> Self {
        Self { id, name }
    }

    /// The account for messages: `'name' (id)` or `id N`.
    fn shown(&self) -> String {
        match &self.name {
            Some(name) => format!("{} ({})", quoted(name.as_str()), self.id),
            None => format!("id {}", self.id),
        }
    }
}

/// The SSH login user, as probe step H read it (`id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAccount {
    /// The user (`id -u`, `id -un`).
    pub user: Account,
    /// Its primary group (`id -g`, `id -gn`).
    pub group: Account,
    /// Every group id it belongs to (`id -G`).
    pub groups: Vec<u32>,
}

/// The plugins directory's owner, group and mode (step H's `stat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirOwnership {
    /// The owning user.
    pub owner: Account,
    /// The owning group.
    pub group: Account,
    /// Its mode (`stat %a`).
    pub mode: FileMode,
}

/// The programs the server has to switch users (step H's `command -v`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwitchTools {
    /// `sudo` is installed.
    pub sudo: bool,
    /// `runuser` is installed (it works for a root login only).
    pub runuser: bool,
}

/// The identity table's inputs.
#[derive(Debug, Clone, Copy)]
pub struct IdentityFacts<'a> {
    /// The site's `run_as`.
    pub run_as: &'a RunAs,
    /// The site's `owner`.
    pub owner: &'a Owner,
    /// The site's `allow_root`.
    pub allow_root: bool,
    /// The login user.
    pub login: &'a LoginAccount,
    /// The plugins directory.
    pub dir: &'a DirOwnership,
    /// What the server has to switch users.
    pub tools: SwitchTools,
    /// The user ids `id -u <name>` resolved for the names the settings
    /// give (`run_as`'s user, `owner`'s user); a name missing here does
    /// not exist on the server.
    pub users: &'a [(String, u32)],
}

/// The ownership plugin files must end up with (plan §6.8's reference
/// `R = (U, G, P)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// `U`: the owning user.
    pub owner: Account,
    /// `G`: the owning group.
    pub group: Account,
    /// `P`: the plugins directory's mode (group bits, setgid).
    pub mode: FileMode,
}

/// Whether files an identity creates get the reference owner and group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Creation {
    /// Yes: the identity is `U` with `G` as its group.
    AtCreation,
    /// Not guaranteed: the identity is not `U`, or runs without `-g G`. The
    /// post-install ownership check (plan §6.8) repairs what it finds (a
    /// setgid plugins directory often leaves nothing to repair).
    CheckAfter,
}

/// One identity the probe may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The prefix.
    pub identity: Identity,
    /// The uid WP-CLI runs with: `--allow-root` exactly when it is 0.
    pub uid: u32,
    /// Whether new files get the reference owner and group.
    pub creation: Creation,
}

/// The table's answer: the reference, and the identities to try in
/// order (never empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPlan {
    /// The ownership plugin files must have.
    pub reference: Reference,
    /// The identities, best first.
    pub candidates: Vec<Candidate>,
}

/// What would run as root without the site's `allow_root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RootRole {
    /// The plugins directory's (or the explicit) owner is uid 0.
    Owner,
    /// The plugins directory's (or the explicit) group is gid 0.
    Group,
    /// The identity itself would be uid 0 (a root login with
    /// `run_as: login`, or a `run_as` user of uid 0).
    Identity,
    /// The identity would run with gid 0 as its primary group (a login
    /// user, or a sudo target without `-g`, whose primary group is root).
    PrimaryGroup,
}

/// Which account has no usable name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NamedRole {
    /// The user WP-CLI must run as.
    User,
    /// The reference group.
    Group,
}

/// Why the table refuses a site.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdentityRefusal {
    /// Root would own or run things, and the site does not allow it
    /// (`owner_is_root`).
    #[error("{}", root_message(*role, account))]
    OwnerIsRoot {
        /// What is root.
        role: RootRole,
        /// The account (uid or gid 0).
        account: Account,
    },
    /// The explicit `owner` does not own the plugins directory
    /// (`plugins_dir_mismatch`).
    #[error(
        "the site's owner {} does not own the plugins directory, which belongs to {}",
        quoted(expected),
        found.shown()
    )]
    OwnerMismatch {
        /// The `owner` setting (`user` or `user:group`).
        expected: String,
        /// Who owns the directory (user, or group when only the group differs).
        found: Account,
    },
    /// A user the settings name does not exist on the server: the explicit
    /// owner (`plugins_dir_mismatch`) or the `run_as` user (`invalid_input`).
    #[error(
        "the site's {setting} names the user {}, which does not exist on the server",
        quoted(name)
    )]
    UnknownUser {
        /// `owner` or `run_as`.
        setting: &'static str,
        /// The name.
        name: String,
    },
    /// An account to switch to has no name apvm can use (`invalid_input`):
    /// `stat` printed none, or one that breaks the user rule.
    #[error(
        "the {} has no name apvm can pass to sudo or runuser ({}); set the site's run_as \
         and owner to a valid user name",
        named_role(*role),
        account.shown()
    )]
    Unnamed {
        /// Which account.
        role: NamedRole,
        /// The account.
        account: Account,
    },
    /// Running as another user needs sudo (or runuser for a root login),
    /// and the server has neither (`sudo_missing`).
    #[error(
        "running WP-CLI as {} needs sudo (or runuser, for a root login), and the server has \
         neither: install sudo with a password-less rule for the login user",
        user.shown()
    )]
    SudoMissing {
        /// The user WP-CLI must run as.
        user: Account,
    },
}

impl IdentityRefusal {
    /// The stable code (plan §4.5).
    pub fn code(&self) -> Code {
        match self {
            Self::OwnerIsRoot { .. } => Code::OwnerIsRoot,
            Self::OwnerMismatch { .. } => Code::PluginsDirMismatch,
            Self::UnknownUser { setting, .. } if *setting == OWNER => Code::PluginsDirMismatch,
            Self::UnknownUser { .. } | Self::Unnamed { .. } => Code::InvalidInput,
            Self::SudoMissing { .. } => Code::SudoMissing,
        }
    }
}

/// The `owner` setting's name in messages.
const OWNER: &str = "owner";
/// The `run_as` setting's name in messages.
const RUN_AS: &str = "run_as";

/// The [`IdentityRefusal::OwnerIsRoot`] message.
///
/// # Arguments
///
/// * `role` - What is root
/// * `account` - The root account
fn root_message(role: RootRole, account: &Account) -> String {
    let what = match role {
        RootRole::Owner => "the plugins directory's owner",
        RootRole::Group => "the plugins directory's group",
        RootRole::Identity => "the user WP-CLI would run as",
        RootRole::PrimaryGroup => "the group WP-CLI would run with",
    };
    format!(
        "{what} is root ({}); apvm runs nothing as root unless the site sets allow_root",
        account.shown()
    )
}

/// [`NamedRole`] in messages.
///
/// # Arguments
///
/// * `role` - The role
fn named_role(role: NamedRole) -> &'static str {
    match role {
        NamedRole::User => "user WP-CLI must run as",
        NamedRole::Group => "plugins directory's group",
    }
}

/// Decide the reference and the identities to try (the module's table).
///
/// # Arguments
///
/// * `facts` - The settings and what step H measured
///
/// # Errors
///
/// [`IdentityRefusal`] when the site cannot get an identity.
pub fn plan_identity(facts: &IdentityFacts<'_>) -> Result<IdentityPlan, IdentityRefusal> {
    let reference = reference(facts)?;
    let wanted = wanted(facts, &reference)?;
    check_root(facts, &reference, &wanted)?;
    let candidates = match wanted {
        Wanted::Login => vec![login_candidate(facts.login, &reference)],
        Wanted::User(user) if user.id == facts.login.user.id => same_user(facts, &reference),
        Wanted::User(user) => other_user(facts, &reference, user)?,
    };
    let candidates = without_root_group(facts, candidates)?;
    Ok(IdentityPlan {
        reference,
        candidates,
    })
}

/// `candidates` without the login user when its primary group is gid 0
/// and the site does not allow root (a sudo target's primary group is
/// checked by the probe, which reads it with `id -g`).
///
/// # Arguments
///
/// * `facts` - The inputs
/// * `candidates` - The table's candidates
fn without_root_group(
    facts: &IdentityFacts<'_>,
    candidates: Vec<Candidate>,
) -> Result<Vec<Candidate>, IdentityRefusal> {
    if facts.allow_root || facts.login.group.id != 0 {
        return Ok(candidates);
    }
    let kept: Vec<Candidate> = candidates
        .into_iter()
        .filter(|candidate| candidate.identity != Identity::Login)
        .collect();
    if kept.is_empty() {
        return Err(IdentityRefusal::OwnerIsRoot {
            role: RootRole::PrimaryGroup,
            account: facts.login.group.clone(),
        });
    }
    Ok(kept)
}

/// Who WP-CLI should run as, before the table picks a prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wanted {
    /// The login user, by `run_as: login`.
    Login,
    /// This user (the owner, or `run_as`'s user).
    User(Account),
}

/// The reference: the directory's owner and group, or the explicit owner,
/// which must own the directory.
///
/// # Arguments
///
/// * `facts` - The inputs
fn reference(facts: &IdentityFacts<'_>) -> Result<Reference, IdentityRefusal> {
    let dir = facts.dir;
    let base = Reference {
        owner: dir.owner.clone(),
        group: dir.group.clone(),
        mode: dir.mode,
    };
    let Owner::Explicit { user, group } = facts.owner else {
        return Ok(base);
    };
    let expected = facts.owner.to_string();
    let uid = lookup(facts.users, user).ok_or_else(|| IdentityRefusal::UnknownUser {
        setting: OWNER,
        name: user.clone(),
    })?;
    if uid != dir.owner.id {
        return Err(IdentityRefusal::OwnerMismatch {
            expected,
            found: dir.owner.clone(),
        });
    }
    let group_matches = group
        .as_ref()
        .is_none_or(|group| dir.group.name.as_ref().is_some_and(|n| n.as_str() == group));
    if !group_matches {
        return Err(IdentityRefusal::OwnerMismatch {
            expected,
            found: dir.group.clone(),
        });
    }
    let owner = Account::new(uid, AccountName::new(user).ok().or(base.owner.name));
    Ok(Reference { owner, ..base })
}

/// The user `run_as` asks for.
///
/// # Arguments
///
/// * `facts` - The inputs
/// * `reference` - The reference
fn wanted(facts: &IdentityFacts<'_>, reference: &Reference) -> Result<Wanted, IdentityRefusal> {
    match facts.run_as {
        RunAs::Auto => Ok(Wanted::User(reference.owner.clone())),
        RunAs::Login => Ok(Wanted::Login),
        RunAs::User(name) => {
            let unknown = || IdentityRefusal::UnknownUser {
                setting: RUN_AS,
                name: name.clone(),
            };
            let uid = lookup(facts.users, name).ok_or_else(unknown)?;
            let name = AccountName::new(name).map_err(|_| unknown())?;
            Ok(Wanted::User(Account::new(uid, Some(name))))
        }
    }
}

/// Refuse root (by id) unless the site allows it.
///
/// # Arguments
///
/// * `facts` - The inputs
/// * `reference` - The reference
/// * `wanted` - Who WP-CLI would run as
fn check_root(
    facts: &IdentityFacts<'_>,
    reference: &Reference,
    wanted: &Wanted,
) -> Result<(), IdentityRefusal> {
    if facts.allow_root {
        return Ok(());
    }
    let identity = match wanted {
        Wanted::Login => &facts.login.user,
        Wanted::User(user) => user,
    };
    // A root group alone is not refused: the identity then runs without
    // `-g` ([`switch_group`]), never with gid 0.
    let roots = [
        (RootRole::Owner, &reference.owner),
        (RootRole::Identity, identity),
    ];
    match roots.into_iter().find(|(_, account)| account.id == 0) {
        Some((role, account)) => Err(IdentityRefusal::OwnerIsRoot {
            role,
            account: account.clone(),
        }),
        None => Ok(()),
    }
}

/// The login user without a prefix.
///
/// # Arguments
///
/// * `login` - The login user
/// * `reference` - The reference
fn login_candidate(login: &LoginAccount, reference: &Reference) -> Candidate {
    let exact = login.user.id == reference.owner.id && login.group.id == reference.group.id;
    Candidate {
        identity: Identity::Login,
        uid: login.user.id,
        creation: if exact {
            Creation::AtCreation
        } else {
            Creation::CheckAfter
        },
    }
}

/// `U == L`: no prefix, or `sudo -u L -g G` first when `G ≠ Lg`. Not on
/// a setgid plugins directory whose group the login user belongs to: new
/// entries get `G` there anyway, and a sudo the user may not run would be
/// refused (and logged, and mailed) on every probe.
///
/// # Arguments
///
/// * `facts` - The inputs
/// * `reference` - The reference
fn same_user(facts: &IdentityFacts<'_>, reference: &Reference) -> Vec<Candidate> {
    let login = facts.login;
    let plain = login_candidate(login, reference);
    let setgid_member =
        reference.mode.bits() & SETGID != 0 && login.groups.contains(&reference.group.id);
    if login.group.id == reference.group.id || !facts.tools.sudo || setgid_member {
        return vec![plain];
    }
    let (Some(user), Some(group)) = (&login.user.name, switch_group(facts, reference)) else {
        return vec![plain];
    };
    let with_group = Candidate {
        identity: Identity::Sudo {
            user: user.clone(),
            group: Some(group),
        },
        uid: login.user.id,
        creation: with_group(login.user.id, reference),
    };
    vec![with_group, plain]
}

/// `U ≠ L`: `runuser` for a root login, else sudo with the group, then
/// without it.
///
/// # Arguments
///
/// * `facts` - The inputs
/// * `reference` - The reference
/// * `user` - The user WP-CLI must run as
fn other_user(
    facts: &IdentityFacts<'_>,
    reference: &Reference,
    user: Account,
) -> Result<Vec<Candidate>, IdentityRefusal> {
    let Some(name) = user.name.clone() else {
        return Err(IdentityRefusal::Unnamed {
            role: NamedRole::User,
            account: user,
        });
    };
    let group = switch_group(facts, reference);
    let uid = user.id;
    if facts.login.user.id == 0 && facts.tools.runuser {
        if let Some(group) = group {
            let identity = Identity::Runuser { user: name, group };
            return Ok(vec![candidate(identity, uid, with_group(uid, reference))]);
        }
        if !facts.tools.sudo {
            return Err(runuser_without_group(reference));
        }
    }
    if !facts.tools.sudo {
        return Err(IdentityRefusal::SudoMissing { user });
    }
    let mut candidates = Vec::with_capacity(2);
    if let Some(group) = group {
        let identity = Identity::Sudo {
            user: name.clone(),
            group: Some(group),
        };
        candidates.push(candidate(identity, uid, with_group(uid, reference)));
    }
    let identity = Identity::Sudo {
        user: name,
        group: None,
    };
    candidates.push(candidate(identity, uid, Creation::CheckAfter));
    Ok(candidates)
}

/// The group an identity may switch to with `-g`: the reference group's
/// name, unless it has none, or it is gid 0 and the site does not allow
/// root (the identity then keeps its own primary group, checked after).
///
/// # Arguments
///
/// * `facts` - The inputs
/// * `reference` - The reference
fn switch_group(facts: &IdentityFacts<'_>, reference: &Reference) -> Option<AccountName> {
    if reference.group.id == 0 && !facts.allow_root {
        return None;
    }
    reference.group.name.clone()
}

/// Why a root login cannot use `runuser`: it needs `-g`, and the group is
/// unnamed or root (not allowed).
///
/// # Arguments
///
/// * `reference` - The reference
fn runuser_without_group(reference: &Reference) -> IdentityRefusal {
    if reference.group.id == 0 {
        return IdentityRefusal::OwnerIsRoot {
            role: RootRole::Group,
            account: reference.group.clone(),
        };
    }
    IdentityRefusal::Unnamed {
        role: NamedRole::Group,
        account: reference.group.clone(),
    }
}

/// The setgid bit of a mode.
const SETGID: u32 = 0o2000;

/// Whether an identity of `uid` running with the reference group creates
/// files with the reference ownership: only when it is the reference owner
/// (a `run_as` user other than the owner never is).
///
/// # Arguments
///
/// * `uid` - The identity's uid
/// * `reference` - The reference
fn with_group(uid: u32, reference: &Reference) -> Creation {
    if uid == reference.owner.id {
        Creation::AtCreation
    } else {
        Creation::CheckAfter
    }
}

/// A candidate.
///
/// # Arguments
///
/// * `identity` - The prefix
/// * `uid` - The uid it runs with
/// * `creation` - Whether new files get the reference ownership
fn candidate(identity: Identity, uid: u32, creation: Creation) -> Candidate {
    Candidate {
        identity,
        uid,
        creation,
    }
}

/// The uid `id -u` resolved for `name`.
///
/// # Arguments
///
/// * `users` - The resolved names
/// * `name` - The name
fn lookup(users: &[(String, u32)], name: &str) -> Option<u32> {
    users
        .iter()
        .find(|(known, _)| known == name)
        .map(|(_, uid)| *uid)
}

#[cfg(test)]
mod tests;
