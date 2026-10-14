//! Who runs a remote command: the identity prefixes, the closed set of
//! commands that run as root (plan §6.1 item 4, §6.2, §6.8), the identity
//! table that picks a site's prefix ([`plan_identity`]), and the
//! classification of sudo's refusals ([`SudoRefusal`]).
//!
//! No shell parses data under `sudo` or `runuser`: the prefixes are
//! followed by a fixed program (`env` for WP-CLI, `tee` for uploads, `stat`
//! or `chown` for root). The one shell is the probe's directory walk
//! (`env -u BASH_ENV -u ENV -u CDPATH sh -c '<constant>' sh <params…>`,
//! [`inspect`](super::inspect)): constant code, data only as positional
//! parameters, like a constant `wp eval` snippet. It runs as root only when
//! the site's `allow_root` makes root the identity (whose sudo rule is
//! then `(root) NOPASSWD: ALL` anyway), never through [`RootCommand`].

mod decision;
pub(crate) mod sudo;

use super::invalid;
use super::quote::Word;
use super::statement::Statement;
use crate::allowlist;
use crate::error::Result;

pub use decision::{
    Account, Candidate, Creation, DirOwnership, IdentityFacts, IdentityPlan, IdentityRefusal,
    LoginAccount, NamedRole, Reference, RootRole, SwitchTools, plan_identity,
};
pub use sudo::{SudoRefusal, SudoRefusalKind};

/// A Unix user or group name that may appear in a remote command:
/// `[A-Za-z0-9._-]`, no leading `-`, at most 64 characters, not made only
/// of digits (tools read those as ids, so `0` would mean root), and not
/// `.` or `..` (plan §7). Names read from a server (`stat %U %G`) must pass
/// this before they reach a command, since names like `a b` or `x'y` exist.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountName(String);

impl AccountName {
    /// Check and wrap a user or group name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`](crate::Error::InvalidInput) naming the rule
    /// it breaks.
    pub fn new(name: &str) -> Result<Self> {
        allowlist::user(name).map_err(|reason| invalid("account name", "name", reason))?;
        Ok(Self(name.to_string()))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who a command runs as on the server (plan §6.2). Deciding which one a
/// site needs (by owner and group ids, never by name) is the probe's job;
/// this type only renders the chosen prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Identity {
    /// The SSH login user itself: no prefix.
    Login,
    /// `sudo -n -u <user> [-g <group>] --`: another user, with that primary
    /// group when given. `-n` never prompts for a password.
    Sudo {
        /// The target user.
        user: AccountName,
        /// The primary group, when the site's group differs from the
        /// user's own (sudo allows any group the user belongs to).
        group: Option<AccountName>,
    },
    /// `runuser -u <user> -g <group> --`: for a root login without sudo.
    Runuser {
        /// The target user.
        user: AccountName,
        /// The primary group.
        group: AccountName,
    },
}

impl Identity {
    /// `sudo -n -u <user> [-g <group>] --`.
    ///
    /// # Arguments
    ///
    /// * `user` - The target user
    /// * `group` - The primary group, if any
    ///
    /// # Errors
    ///
    /// `invalid_input` when a name breaks the [`AccountName`] rule.
    pub fn sudo(user: &str, group: Option<&str>) -> Result<Self> {
        Ok(Self::Sudo {
            user: AccountName::new(user)?,
            group: group.map(AccountName::new).transpose()?,
        })
    }

    /// `runuser -u <user> -g <group> --`.
    ///
    /// # Arguments
    ///
    /// * `user` - The target user
    /// * `group` - The primary group
    ///
    /// # Errors
    ///
    /// `invalid_input` when a name breaks the [`AccountName`] rule.
    pub fn runuser(user: &str, group: &str) -> Result<Self> {
        Ok(Self::Runuser {
            user: AccountName::new(user)?,
            group: AccountName::new(group)?,
        })
    }

    /// The prefix words, ending with `--` (none for [`Identity::Login`]).
    pub(crate) fn prefix(&self) -> Vec<Word> {
        match self {
            Self::Login => Vec::new(),
            Self::Sudo { user, group } => switch_words(&["sudo", "-n"], user, group.as_ref()),
            Self::Runuser { user, group } => switch_words(&["runuser"], user, Some(group)),
        }
    }
}

/// `<program…> -u <user> [-g <group>] --`.
///
/// # Arguments
///
/// * `program` - The literal program and its fixed options
/// * `user` - The target user
/// * `group` - The primary group, if any
fn switch_words(
    program: &[&'static str],
    user: &AccountName,
    group: Option<&AccountName>,
) -> Vec<Word> {
    let mut words: Vec<Word> = program.iter().map(|text| Word::lit(text)).collect();
    words.extend([Word::lit("-u"), Word::checked(user.as_str())]);
    if let Some(group) = group {
        words.extend([Word::lit("-g"), Word::checked(group.as_str())]);
    }
    words.push(Word::lit("--"));
    words
}

/// How a [`RootCommand`] gets root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RootAccess {
    /// `sudo -n -u root --`: the login user's password-less sudo rule for
    /// that one program (plan §6.2 documents the sudoers lines). `-u root`
    /// is explicit because a sudoers `runas_default` changes who a bare
    /// `sudo` runs as.
    Sudo,
    /// The SSH login is root already: no prefix.
    RootLogin,
}

impl RootAccess {
    /// The prefix words (none for [`RootAccess::RootLogin`]).
    pub(crate) fn prefix(self) -> Vec<Word> {
        match self {
            Self::Sudo => ["sudo", "-n", "-u", "root", "--"].map(Word::lit).to_vec(),
            Self::RootLogin => Vec::new(),
        }
    }
}

/// A numeric owner: user id and group id. Commands only ever get numeric
/// ids, written `+<id>` so that GNU tools never read them as names: a name
/// cannot resolve to another account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnerIds {
    /// The user id.
    pub uid: u32,
    /// The group id.
    pub gid: u32,
}

/// An entry directly inside a plugins directory: a plugin folder
/// (`<plugins_dir>/<slug>`) or a single-file plugin (`hello.php`). It is
/// never the plugins directory itself, a deeper path, or `/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginEntry {
    /// `<plugins_dir>/<name>`.
    path: String,
}

impl PluginEntry {
    /// Check and join a plugins directory and an entry name.
    ///
    /// # Arguments
    ///
    /// * `plugins_dir` - The plugins directory: absolute and canonical (the
    ///   site path rule, plan §7)
    /// * `name` - The entry: a plugin slug or file name,
    ///   `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$`
    ///
    /// # Errors
    ///
    /// `invalid_input` naming the value and the rule it breaks.
    pub fn new(plugins_dir: &str, name: &str) -> Result<Self> {
        let subject = "plugin entry";
        allowlist::remote_path(plugins_dir)
            .map_err(|reason| invalid(subject, "plugins_dir", reason))?;
        allowlist::slug(name).map_err(|reason| invalid(subject, "name", reason))?;
        let path = format!("{plugins_dir}/{name}");
        allowlist::remote_path(&path).map_err(|reason| invalid(subject, "path", reason))?;
        Ok(Self { path })
    }

    /// The entry's absolute path.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// A command that runs as root: a closed set (plan §6.1 item 4, §10). The
/// only privileged needs are reading a directory the login user cannot
/// reach, and the opt-in guarded ownership repair of `mirror-root` sites
/// (§6.8). Root never deletes, and never runs a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCommand(RootKind);

/// The `stat` format of [`RootCommand::stat_ownership`]: one field per
/// line, names last. The newlines are literal (GNU `-c` and busybox `-c`
/// both keep them; neither has `-L` here, so a symbolic link is described,
/// never followed).
const OWNERSHIP_FORMAT: &str = "'%u\n%g\n%a\n%F\n%U\n%G'";

/// The commands [`RootCommand`] can hold.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RootKind {
    /// `stat -c '<OWNERSHIP_FORMAT>' -- <dir>`.
    StatOwnership(String),
    /// `stat -c %F -- <entry>`.
    StatFileType(PluginEntry),
    /// `chown -R -P -h --from=+<F>:+<FG> -- +<U>:+<G> <entry>`.
    Chown {
        /// The tree to repair.
        entry: PluginEntry,
        /// Only entries owned by this pair change.
        from: OwnerIds,
        /// The owner they get.
        to: OwnerIds,
    },
}

impl RootCommand {
    /// `stat -c '%u⏎%g⏎%a⏎%F⏎%U⏎%G' -- <dir>` (⏎ = a newline): owner and
    /// group of a directory the login user cannot reach (the probe's
    /// fallback, §6.2). GNU and busybox `stat` print exactly six lines:
    ///
    /// 1. the owner's uid and 2. the group's gid, in decimal;
    /// 3. the mode in octal, without a leading zero (`2775`, `755`;
    ///    [`FileMode::from_octal`](super::FileMode::from_octal) reads it);
    /// 4. the file type: the caller must require `directory` (a symbolic
    ///    link prints `symbolic link` and is described, never followed);
    /// 5. the owner's and 6. the group's name, or `UNKNOWN` when the id has
    ///    none. Names may hold spaces; they come last so they cannot shift
    ///    the other fields. Identity decisions use the ids; a name is only
    ///    used after it passes [`AccountName::new`], and `UNKNOWN` means "no
    ///    name".
    ///
    /// Anything else (more or fewer lines, a non-numeric id) must be
    /// treated as a failed probe.
    ///
    /// # Arguments
    ///
    /// * `dir` - An absolute, canonical directory (the site path rule)
    ///
    /// # Errors
    ///
    /// `invalid_input` when `dir` breaks the rule.
    pub fn stat_ownership(dir: &str) -> Result<Self> {
        allowlist::remote_path(dir).map_err(|reason| invalid("root command", "dir", reason))?;
        Ok(Self(RootKind::StatOwnership(dir.to_string())))
    }

    /// `stat -c %F -- <entry>` (GNU stat): prints `directory` for a real
    /// plugin folder and `symbolic link` for a linked one, which the root
    /// repair refuses (§6.8).
    ///
    /// # Arguments
    ///
    /// * `entry` - The plugin entry
    pub fn stat_file_type(entry: PluginEntry) -> Self {
        Self(RootKind::StatFileType(entry))
    }

    /// `chown -R -P -h --from=+<F>:+<FG> -- +<U>:+<G> <entry>` (GNU chown;
    /// busybox has no `--from` and fails closed): the guarded root repair
    /// of a `mirror-root` site (§6.8). `--from` changes only entries owned
    /// by that user and group, `-P -h` never follow links (P0 verified
    /// both), and the `+` keeps a user or group *named* like a number from
    /// standing in for the id. The other preconditions (a real directory,
    /// no multi-linked files, `protected_hardlinks`) are separate checks
    /// right before it.
    ///
    /// # Arguments
    ///
    /// * `entry` - The plugin tree to repair
    /// * `from` - The foreign owner the check found
    /// * `to` - The owner the plugins directory expects
    /// * `login_uid` - The SSH login user's id
    ///
    /// # Errors
    ///
    /// `invalid_input` when `from` is root, the login user or already `to`'s
    /// user, or when any id is 4294967295 (`(uid_t) -1`, which chown reads
    /// as "unchanged"). Such trees are reported to the operator instead.
    pub fn chown(entry: PluginEntry, from: OwnerIds, to: OwnerIds, login_uid: u32) -> Result<Self> {
        let refuse = |location: &str, reason: &str| Err(invalid("root chown", location, reason));
        if [from.uid, from.gid, to.uid, to.gid].contains(&u32::MAX) {
            return refuse("ids", "4294967295 means 'unchanged' to chown");
        }
        if from.uid == 0 {
            return refuse(
                "from",
                "must not be root: root-owned files are never handed over",
            );
        }
        if from.uid == login_uid {
            return refuse("from", "must not be the login user");
        }
        if from.uid == to.uid {
            return refuse("from", "must be a foreign owner, not the expected one");
        }
        Ok(Self(RootKind::Chown { entry, from, to }))
    }

    /// The command as a statement run as root through `access`.
    ///
    /// # Arguments
    ///
    /// * `access` - How root is reached
    pub(crate) fn statement(&self, access: RootAccess) -> Statement {
        match &self.0 {
            RootKind::StatOwnership(dir) => stat(access, OWNERSHIP_FORMAT, dir),
            RootKind::StatFileType(entry) => stat(access, "%F", entry.path()),
            RootKind::Chown { entry, from, to } => {
                let mut args = ["-R", "-P", "-h"].map(Word::lit).to_vec();
                args.extend([
                    Word::ids("--from=", from.uid, from.gid),
                    Word::lit("--"),
                    Word::ids("", to.uid, to.gid),
                    Word::checked(entry.path()),
                ]);
                Statement::as_root(access, "chown", args)
            }
        }
    }
}

/// `stat -c <format> -- <path>`, as root.
///
/// # Arguments
///
/// * `access` - How root is reached
/// * `format` - The literal format word (quoted when it needs it)
/// * `path` - A checked absolute path
fn stat(access: RootAccess, format: &'static str, path: &str) -> Statement {
    let args = vec![
        Word::lit("-c"),
        Word::lit(format),
        Word::lit("--"),
        Word::checked(path),
    ];
    Statement::as_root(access, "stat", args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;
    use crate::remote::quote::join;

    /// The rendered prefix of `identity`.
    ///
    /// # Arguments
    ///
    /// * `identity` - The identity
    fn prefix(identity: &Identity) -> String {
        join(&identity.prefix())
    }

    /// A plugin entry `name` under a fixed plugins directory.
    ///
    /// # Arguments
    ///
    /// * `name` - The entry name
    fn entry(name: &str) -> PluginEntry {
        PluginEntry::new("/var/www/shop/wp-content/plugins", name).unwrap()
    }

    /// An owner of `uid` and `gid`.
    ///
    /// # Arguments
    ///
    /// * `uid` - The user id
    /// * `gid` - The group id
    fn ids(uid: u32, gid: u32) -> OwnerIds {
        OwnerIds { uid, gid }
    }

    /// The shell text of `command` run without a prefix (a root login).
    ///
    /// # Arguments
    ///
    /// * `command` - The root command
    fn render(command: &RootCommand) -> String {
        command.statement(RootAccess::RootLogin).render()
    }

    #[test]
    fn identity_prefixes() {
        let cases = [
            (Identity::Login, ""),
            (
                Identity::sudo("www-data", Some("www-data")).unwrap(),
                "sudo -n -u 'www-data' -g 'www-data' --",
            ),
            (
                Identity::sudo("ftpuser", None).unwrap(),
                "sudo -n -u 'ftpuser' --",
            ),
            (
                Identity::runuser("ftpuser", "www-data").unwrap(),
                "runuser -u 'ftpuser' -g 'www-data' --",
            ),
        ];
        for (identity, expected) in cases {
            assert_eq!(prefix(&identity), expected, "{identity:?}");
        }
    }

    #[test]
    fn identities_refuse_names_outside_the_user_rule() {
        let bad = [
            "", "-u", "a b", "x'y", "a\\b", "#0", "0", "1000", ".", "..", "a:b", "a$b", "é", "a\nb",
        ];
        for name in bad {
            for result in [
                Identity::sudo(name, None),
                Identity::sudo("www-data", Some(name)),
                Identity::runuser(name, "www-data"),
                Identity::runuser("ftpuser", name),
            ] {
                let error = result.unwrap_err();
                assert_eq!(error.code(), Code::InvalidInput, "{name:?}");
                assert!(
                    error
                        .to_string()
                        .starts_with("invalid account name: name: "),
                    "{error}"
                );
            }
        }
        let longest = "u".repeat(64);
        assert!(AccountName::new(&longest).is_ok());
        assert!(AccountName::new(&"u".repeat(65)).is_err());
        // Root is a valid name; whether a site may run as root is decided by
        // id, with `allow_root` (plan §6.2).
        assert_eq!(AccountName::new("root").unwrap().as_str(), "root");
    }

    #[test]
    fn root_access_prefixes() {
        assert_eq!(join(&RootAccess::Sudo.prefix()), "sudo -n -u root --");
        assert_eq!(join(&RootAccess::RootLogin.prefix()), "");
    }

    #[test]
    fn root_command_goldens() {
        let dir = "/home/ftp user/it's $HOME/*/wp-content/plugins";
        let cases = [
            (
                RootCommand::stat_ownership(dir).unwrap(),
                "stat -c '%u\n%g\n%a\n%F\n%U\n%G' -- \
                 '/home/ftp user/it'\\''s $HOME/*/wp-content/plugins'",
            ),
            (
                RootCommand::stat_file_type(entry("backwpup-pro")),
                "stat -c %F -- '/var/www/shop/wp-content/plugins/backwpup-pro'",
            ),
            (
                RootCommand::chown(entry("backwpup"), ids(1001, 1001), ids(33, 33), 1000).unwrap(),
                "chown -R -P -h --from=+1001:+1001 -- +33:+33 \
                 '/var/www/shop/wp-content/plugins/backwpup'",
            ),
            (
                RootCommand::chown(entry("hello.php"), ids(1002, 0), ids(1001, 33), 0).unwrap(),
                "chown -R -P -h --from=+1002:+0 -- +1001:+33 \
                 '/var/www/shop/wp-content/plugins/hello.php'",
            ),
        ];
        for (command, expected) in cases {
            assert_eq!(render(&command), expected);
        }
    }

    #[test]
    fn root_chown_refuses_root_the_login_user_and_unchanged_ids() {
        let cases = [
            (ids(0, 0), ids(33, 33), 1000, "from: must not be root"),
            (ids(0, 33), ids(1001, 33), 1000, "from: must not be root"),
            (
                ids(1000, 1000),
                ids(33, 33),
                1000,
                "from: must not be the login user",
            ),
            (
                ids(33, 1000),
                ids(33, 33),
                1000,
                "from: must be a foreign owner",
            ),
            (ids(u32::MAX, 1), ids(33, 33), 1000, "ids: 4294967295"),
            (ids(1001, u32::MAX), ids(33, 33), 1000, "ids: 4294967295"),
            (ids(1001, 1), ids(u32::MAX, 33), 1000, "ids: 4294967295"),
            (ids(1001, 1), ids(33, u32::MAX), 1000, "ids: 4294967295"),
        ];
        for (from, to, login, reason) in cases {
            let error = RootCommand::chown(entry("x"), from, to, login).unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput);
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("invalid root chown: {reason}")),
                "{error}"
            );
        }
        // A root login (uid 0) may repair any foreign owner but root.
        assert!(RootCommand::chown(entry("x"), ids(1001, 1001), ids(33, 33), 0).is_ok());
    }

    #[test]
    fn root_paths_must_be_absolute_and_canonical() {
        for dir in [
            "",
            "relative",
            "/",
            "/var/www/",
            "/var/../etc",
            "/a\nb",
            "/a\\b",
        ] {
            let error = RootCommand::stat_ownership(dir).unwrap_err();
            assert!(
                error.to_string().starts_with("invalid root command: dir: "),
                "{error}"
            );
        }
    }

    #[test]
    fn plugin_entries_are_direct_children_of_a_canonical_dir() {
        let entry = PluginEntry::new("/srv/a'b c/plugins", "akismet").unwrap();
        assert_eq!(entry.path(), "/srv/a'b c/plugins/akismet");
        let cases = [
            ("plugins", "akismet", "plugins_dir: "),
            ("/", "akismet", "plugins_dir: "),
            ("/srv/plugins/", "akismet", "plugins_dir: "),
            ("/srv/plugins", "", "name: "),
            ("/srv/plugins", "..", "name: "),
            ("/srv/plugins", ".hidden", "name: "),
            ("/srv/plugins", "a/b", "name: "),
            ("/srv/plugins", "-rf", "name: "),
            ("/srv/plugins", "a b", "name: "),
        ];
        for (dir, name, location) in cases {
            let error = PluginEntry::new(dir, name).unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("invalid plugin entry: {location}")),
                "{dir:?} {name:?}: {error}"
            );
        }
        // The joined path must still fit the path length limit.
        let dir = format!("/{}", "d".repeat(4000));
        let error = PluginEntry::new(&dir, &"n".repeat(100)).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("invalid plugin entry: path: must be at most"),
            "{error}"
        );
    }
}
