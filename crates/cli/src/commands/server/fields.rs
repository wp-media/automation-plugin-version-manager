//! Every setting the `server` flags change, declared once.
//!
//! A [`Field`] is a setting (`--user`, `--unset user`, `ssh.user`); a
//! [`Setting`] is a field with its value. Each fact about a field — its
//! `--unset` name, its flags, its place in a record, whether it is a local
//! path, which records have it, how a record stores it — is one arm of an
//! exhaustive `match`, so a new field does not compile until every fact is
//! given. The commands derive the rest from here: applying and clearing
//! settings, set + unset conflicts, what changed, the relative-path
//! warning, the `--unset` values, and the flag names of refused input.

use std::path::{Path, is_separator};

use apvm_sites::{Defaults, Server, SshSettings};
use clap::ValueEnum;
use clap::builder::{PossibleValuesParser, TypedValueParser};

use crate::commands::inventory::Failure;

/// A setting of a server or of the SSH defaults. The variant docs are the
/// `--unset` value help.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    /// Host name or address
    Host,
    /// SSH login user
    User,
    /// SSH port
    Port,
    /// Private key
    Key,
    /// Seconds allowed to connect
    ConnectTimeout,
    /// Jump hosts
    Jump,
    /// known_hosts file
    KnownHosts,
    /// ssh_config file
    SshConfig,
    /// Connection reuse (back to the default)
    Multiplex,
    /// The ssh program
    SshBinary,
    /// Display name
    Name,
    /// Free-form notes
    Notes,
}

/// The record a command changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    /// A server (`server add` / `server edit`).
    Server,
    /// The SSH defaults (`server defaults`).
    Defaults,
}

/// Whether a field holds a path on this machine, which ssh resolves from
/// the directory apvm runs in when it is relative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Local {
    /// Not a local path.
    No,
    /// A file path.
    Path,
    /// A program: a path, or a bare name looked up on `PATH`.
    Program,
}

/// The fixed facts of a [`Field`].
#[derive(Clone, Copy, Debug)]
struct Spec {
    /// The `--unset` value, also used in change reports (`known-hosts`).
    name: &'static str,
    /// The flag(s) that set it (`--multiplex/--no-multiplex`).
    flags: &'static str,
    /// Where the library reports refused input of it (`ssh.user`).
    location: &'static str,
    /// Whether it is a local path.
    local: Local,
    /// The records that have it.
    scopes: &'static [Scope],
    /// Whether it is required, so `--unset` cannot clear it.
    required: bool,
}

/// Both records.
const BOTH: &[Scope] = &[Scope::Server, Scope::Defaults];
/// Servers only.
const SERVER: &[Scope] = &[Scope::Server];
/// The defaults only.
const DEFAULTS: &[Scope] = &[Scope::Defaults];

/// An optional [`Spec`] (only the host is required).
const fn spec(
    name: &'static str,
    flags: &'static str,
    location: &'static str,
    local: Local,
    scopes: &'static [Scope],
) -> Spec {
    Spec {
        name,
        flags,
        location,
        local,
        scopes,
        required: false,
    }
}

impl Field {
    /// The facts of this field: the one table every aspect derives from.
    const fn spec(self) -> Spec {
        use Local::{No, Path, Program};
        match self {
            Self::Host => Spec {
                required: true,
                ..spec("host", "--host", "host", No, SERVER)
            },
            Self::User => spec("user", "--user", "ssh.user", No, BOTH),
            Self::Port => spec("port", "--port", "ssh.port", No, BOTH),
            Self::Key => spec("key", "--key", "ssh.identity_file", Path, BOTH),
            Self::ConnectTimeout => spec(
                "connect-timeout",
                "--connect-timeout",
                "ssh.connect_timeout_secs",
                No,
                BOTH,
            ),
            Self::Jump => spec("jump", "--jump", "ssh.proxy_jump", No, BOTH),
            Self::KnownHosts => spec(
                "known-hosts",
                "--known-hosts",
                "ssh.known_hosts_file",
                Path,
                BOTH,
            ),
            Self::SshConfig => spec(
                "ssh-config",
                "--ssh-config",
                "ssh.ssh_config_file",
                Path,
                BOTH,
            ),
            Self::Multiplex => spec(
                "multiplex",
                "--multiplex/--no-multiplex",
                "ssh.multiplex",
                No,
                BOTH,
            ),
            Self::SshBinary => spec(
                "ssh-binary",
                "--ssh-binary",
                "ssh.binary",
                Program,
                DEFAULTS,
            ),
            Self::Name => spec("name", "--name", "name", No, SERVER),
            Self::Notes => spec("notes", "--notes", "notes", No, SERVER),
        }
    }

    /// The `--unset` value of this field, as change reports name it.
    pub(super) const fn name(self) -> &'static str {
        self.spec().name
    }

    /// The flag(s) that set this field.
    pub(super) const fn flags(self) -> &'static str {
        self.spec().flags
    }

    /// Whether `scope`'s `--unset` can clear this field.
    fn clearable_in(self, scope: Scope) -> bool {
        let spec = self.spec();
        !spec.required && spec.scopes.contains(&scope)
    }

    /// Every field, in report order.
    fn all() -> impl Iterator<Item = Self> {
        Self::value_variants().iter().copied()
    }
}

/// A [`Field`] with its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Setting {
    /// `--host`
    Host(String),
    /// `--user`
    User(String),
    /// `--port`
    Port(u16),
    /// `--key`
    Key(String),
    /// `--connect-timeout`
    ConnectTimeout(u32),
    /// `--jump`
    Jump(String),
    /// `--known-hosts`
    KnownHosts(String),
    /// `--ssh-config`
    SshConfig(String),
    /// `--multiplex` (`true`) / `--no-multiplex` (`false`)
    Multiplex(bool),
    /// `--ssh-binary`
    SshBinary(String),
    /// `--name`
    Name(String),
    /// `--notes`
    Notes(String),
}

impl Setting {
    /// The field this setting sets.
    pub(super) const fn field(&self) -> Field {
        match self {
            Self::Host(_) => Field::Host,
            Self::User(_) => Field::User,
            Self::Port(_) => Field::Port,
            Self::Key(_) => Field::Key,
            Self::ConnectTimeout(_) => Field::ConnectTimeout,
            Self::Jump(_) => Field::Jump,
            Self::KnownHosts(_) => Field::KnownHosts,
            Self::SshConfig(_) => Field::SshConfig,
            Self::Multiplex(_) => Field::Multiplex,
            Self::SshBinary(_) => Field::SshBinary,
            Self::Name(_) => Field::Name,
            Self::Notes(_) => Field::Notes,
        }
    }

    /// The flag that carries this setting (`--no-multiplex` for
    /// `Multiplex(false)`).
    fn flag(&self) -> &'static str {
        match self {
            Self::Multiplex(true) => "--multiplex",
            Self::Multiplex(false) => "--no-multiplex",
            other => other.field().flags(),
        }
    }

    /// The value, when it is text.
    fn text(&self) -> Option<&str> {
        match self {
            Self::Host(text)
            | Self::User(text)
            | Self::Key(text)
            | Self::Jump(text)
            | Self::KnownHosts(text)
            | Self::SshConfig(text)
            | Self::SshBinary(text)
            | Self::Name(text)
            | Self::Notes(text) => Some(text),
            Self::Port(_) | Self::ConnectTimeout(_) | Self::Multiplex(_) => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Records
// ─────────────────────────────────────────────────────────────────────────────

/// A record the `server` flags change: a server, or the SSH defaults.
pub(super) trait Record: Clone {
    /// The value of `field`: `None` when unset, or not a field of this
    /// record.
    fn get(&self, field: Field) -> Option<Setting>;

    /// Store `setting`. A field this record does not have is ignored (the
    /// flags of a command only set fields of its record).
    fn put(&mut self, setting: Setting);

    /// Clear `field`. A required field, or one this record does not have,
    /// is left as is (`--unset` never offers them).
    fn clear(&mut self, field: Field);
}

impl Record for SshSettings {
    fn get(&self, field: Field) -> Option<Setting> {
        match field {
            Field::User => self.user.clone().map(Setting::User),
            Field::Port => self.port.map(Setting::Port),
            Field::Key => self.identity_file.clone().map(Setting::Key),
            Field::ConnectTimeout => self.connect_timeout_secs.map(Setting::ConnectTimeout),
            Field::Jump => self.proxy_jump.clone().map(Setting::Jump),
            Field::KnownHosts => self.known_hosts_file.clone().map(Setting::KnownHosts),
            Field::SshConfig => self.ssh_config_file.clone().map(Setting::SshConfig),
            Field::Multiplex => self.multiplex.map(Setting::Multiplex),
            Field::SshBinary => self.binary.clone().map(Setting::SshBinary),
            Field::Host | Field::Name | Field::Notes => None,
        }
    }

    fn put(&mut self, setting: Setting) {
        match setting {
            Setting::User(user) => self.user = Some(user),
            Setting::Port(port) => self.port = Some(port),
            Setting::Key(key) => self.identity_file = Some(key),
            Setting::ConnectTimeout(secs) => self.connect_timeout_secs = Some(secs),
            Setting::Jump(jump) => self.proxy_jump = Some(jump),
            Setting::KnownHosts(path) => self.known_hosts_file = Some(path),
            Setting::SshConfig(path) => self.ssh_config_file = Some(path),
            Setting::Multiplex(on) => self.multiplex = Some(on),
            Setting::SshBinary(binary) => self.binary = Some(binary),
            Setting::Host(_) | Setting::Name(_) | Setting::Notes(_) => {}
        }
    }

    fn clear(&mut self, field: Field) {
        match field {
            Field::User => self.user = None,
            Field::Port => self.port = None,
            Field::Key => self.identity_file = None,
            Field::ConnectTimeout => self.connect_timeout_secs = None,
            Field::Jump => self.proxy_jump = None,
            Field::KnownHosts => self.known_hosts_file = None,
            Field::SshConfig => self.ssh_config_file = None,
            Field::Multiplex => self.multiplex = None,
            Field::SshBinary => self.binary = None,
            Field::Host | Field::Name | Field::Notes => {}
        }
    }
}

/// A server's own fields, plus its SSH settings except the ssh program,
/// which only the defaults have (the inventory refuses it on a server).
impl Record for Server {
    fn get(&self, field: Field) -> Option<Setting> {
        match field {
            Field::Host => Some(Setting::Host(self.host.clone())),
            Field::Name => self.name.clone().map(Setting::Name),
            Field::Notes => self.notes.clone().map(Setting::Notes),
            Field::SshBinary => None,
            ssh => self.ssh.get(ssh),
        }
    }

    fn put(&mut self, setting: Setting) {
        match setting {
            Setting::Host(host) => self.host = host,
            Setting::Name(name) => self.name = Some(name),
            Setting::Notes(notes) => self.notes = Some(notes),
            Setting::SshBinary(_) => {}
            ssh => self.ssh.put(ssh),
        }
    }

    fn clear(&mut self, field: Field) {
        match field {
            Field::Name => self.name = None,
            Field::Notes => self.notes = None,
            Field::SshBinary => {}
            ssh => self.ssh.clear(ssh),
        }
    }
}

impl Record for Defaults {
    fn get(&self, field: Field) -> Option<Setting> {
        self.ssh.get(field)
    }

    fn put(&mut self, setting: Setting) {
        self.ssh.put(setting);
    }

    fn clear(&mut self, field: Field) {
        self.ssh.clear(field);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// What the commands derive from the table
// ─────────────────────────────────────────────────────────────────────────────

/// `record` with `settings` stored, then the `unset` fields cleared.
///
/// # Arguments
///
/// * `record` - The current record
/// * `settings` - What the flags set
/// * `unset` - What `--unset` clears
pub(super) fn apply<R: Record>(record: &R, settings: &[Setting], unset: &[Field]) -> R {
    let mut record = record.clone();
    for setting in settings {
        record.put(setting.clone());
    }
    for field in unset {
        record.clear(*field);
    }
    record
}

/// Refuse a field that is both set and cleared in one command.
///
/// # Errors
///
/// [`Failure::Usage`] naming the first such field by the flag typed
/// (`--no-multiplex and --unset multiplex contradict each other: pass only
/// one`).
pub(super) fn check(settings: &[Setting], unset: &[Field]) -> Result<(), Failure> {
    match settings
        .iter()
        .find(|setting| unset.contains(&setting.field()))
    {
        Some(setting) => Err(Failure::Usage(format!(
            "{} and --unset {} contradict each other: pass only one",
            setting.flag(),
            setting.field().name()
        ))),
        None => Ok(()),
    }
}

/// The fields whose values differ between two versions of a record, in
/// [`Field`] order.
pub(super) fn changed<R: Record>(before: &R, after: &R) -> Vec<Field> {
    Field::all()
        .filter(|field| before.get(*field) != after.get(*field))
        .collect()
}

/// The local paths `after` holds that are new (not in `before`) and
/// relative to the directory apvm runs in: `(flag, value)` pairs.
///
/// # Arguments
///
/// * `before` - The record before the change (`None`: a new record)
/// * `after` - The record after the change
pub(super) fn new_relative_paths<R: Record>(
    before: Option<&R>,
    after: &R,
) -> Vec<(&'static str, String)> {
    Field::all()
        .filter_map(|field| {
            let value = after.get(field)?;
            let is_new = before.is_none_or(|before| before.get(field).as_ref() != Some(&value));
            let text = value.text()?;
            (is_new && is_relative(field.spec().local, text))
                .then(|| (field.flags(), text.to_string()))
        })
        .collect()
}

/// Whether `value`, a field of kind `local`, is a path relative to the
/// current directory. A program counts only with a path separator (a bare
/// name is looked up on `PATH`).
fn is_relative(local: Local, value: &str) -> bool {
    match local {
        Local::No => false,
        Local::Path => is_cwd_relative(value),
        Local::Program => value.chars().any(is_separator) && is_cwd_relative(value),
    }
}

/// Whether a local path is relative to the current directory: not under
/// the home directory (`~` then a path separator: `~/`, on Windows also
/// `~\`) and without a root (`/…`; on Windows also `C:\…` or `\…`, while a
/// drive-relative `C:x` counts as relative). The inventory refuses any
/// other `~`.
pub(super) fn is_cwd_relative(value: &str) -> bool {
    let home = value
        .strip_prefix('~')
        .and_then(|rest| rest.chars().next())
        .is_some_and(is_separator);
    !home && !Path::new(value).has_root()
}

/// The `--unset` remedy for empty values (`--notes ""`), which the library
/// refuses: `to clear it: --unset notes`. `None` without an empty value
/// that `scope`'s `--unset` can clear (an empty `--host` has no remedy).
pub(super) fn empty_value_remedy(settings: &[Setting], scope: Scope) -> Option<String> {
    let empty: Vec<&str> = settings
        .iter()
        .filter(|setting| setting.text() == Some(""))
        .map(Setting::field)
        .filter(|field| field.clearable_in(scope))
        .map(Field::name)
        .collect();
    let pronoun = match empty.len() {
        0 => return None,
        1 => "it",
        _ => "them",
    };
    Some(format!("to clear {pronoun}: --unset {}", empty.join(",")))
}

/// Where each flag of `scope`'s commands lands in its record, for
/// `inventory::name_flags`: `(location, flag)`.
pub(super) fn flag_map(scope: Scope) -> Vec<(&'static str, &'static str)> {
    let id = (scope == Scope::Server).then_some(("id", "<ID>"));
    let fields = Field::all()
        .map(Field::spec)
        .filter(|spec| spec.scopes.contains(&scope))
        .map(|spec| (spec.location, spec.flags));
    id.into_iter().chain(fields).collect()
}

/// The `--unset` values of `scope`'s command: the fields it can clear,
/// with their help.
pub(super) fn unset_parser(scope: Scope) -> impl TypedValueParser<Value = Field> {
    let values = Field::all()
        .filter(move |field| field.clearable_in(scope))
        .filter_map(|field| field.to_possible_value());
    PossibleValuesParser::new(values).try_map(|name| <Field as ValueEnum>::from_str(&name, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One sample setting of each field.
    fn one_of_each() -> Vec<Setting> {
        vec![
            Setting::Host("h".into()),
            Setting::User("u".into()),
            Setting::Port(22),
            Setting::Key("/k".into()),
            Setting::ConnectTimeout(9),
            Setting::Jump("j".into()),
            Setting::KnownHosts("/kh".into()),
            Setting::SshConfig("/c".into()),
            Setting::Multiplex(false),
            Setting::SshBinary("/ssh".into()),
            Setting::Name("N".into()),
            Setting::Notes("n".into()),
        ]
    }

    #[test]
    fn every_field_has_one_sample_setting() {
        let fields: Vec<Field> = one_of_each().iter().map(Setting::field).collect();
        assert_eq!(fields, Field::all().collect::<Vec<_>>());
    }

    #[test]
    fn each_record_stores_reads_and_clears_exactly_its_own_fields() {
        for setting in one_of_each() {
            let field = setting.field();
            let scopes = field.spec().scopes;
            let mut server = Server::new("a", "old-host");
            let mut defaults = Defaults::default();
            server.put(setting.clone());
            defaults.put(setting.clone());
            let in_server = scopes.contains(&Scope::Server);
            let expect = |held: bool| held.then(|| setting.clone());
            assert_eq!(server.get(field), expect(in_server), "{field:?}");
            assert_eq!(
                defaults.get(field),
                expect(scopes.contains(&Scope::Defaults)),
                "{field:?}"
            );
            let expected: &[Field] = if in_server { &[field] } else { &[] };
            assert_eq!(
                changed(&Server::new("a", "old-host"), &server),
                expected,
                "{field:?}"
            );
            server.clear(field);
            defaults.clear(field);
            let required = field.spec().required;
            assert_eq!(server.get(field), expect(required), "{field:?} after clear");
            assert_eq!(defaults.get(field), None, "{field:?} after clear");
        }
    }

    #[test]
    fn unset_names_are_the_values_clap_accepts() {
        for field in Field::all() {
            let value = field.to_possible_value().expect("listed");
            assert_eq!(value.get_name(), field.name());
            assert!(field.flags().starts_with("--"), "{field:?}");
            assert!(value.get_help().is_some(), "{field:?} has help");
        }
    }

    #[test]
    fn apply_sets_then_clears_and_check_refuses_both() {
        let server = Server::new("a", "h");
        let settings = [Setting::User("u".into()), Setting::Port(22)];
        let applied = apply(&server, &settings, &[Field::Port, Field::Notes]);
        assert_eq!(applied.ssh.user.as_deref(), Some("u"));
        assert_eq!(applied.ssh.port, None, "--unset comes after the flags");
        assert!(check(&settings, &[Field::Notes]).is_ok());
        let refused = check(&settings, &[Field::Notes, Field::Port]).expect_err("contradiction");
        assert_eq!(
            refused.to_string(),
            "--port and --unset port contradict each other: pass only one"
        );
    }

    #[test]
    fn flag_maps_cover_the_fields_of_their_record() {
        let server = flag_map(Scope::Server);
        assert_eq!(server[0], ("id", "<ID>"));
        assert!(server.contains(&("ssh.connect_timeout_secs", "--connect-timeout")));
        assert!(!server.iter().any(|(_, flag)| *flag == "--ssh-binary"));
        let defaults = flag_map(Scope::Defaults);
        assert!(defaults.contains(&("ssh.binary", "--ssh-binary")));
        assert!(defaults.contains(&("ssh.proxy_jump", "--jump")));
        assert!(
            !defaults
                .iter()
                .any(|(location, _)| ["id", "host", "name", "notes"].contains(location))
        );
    }

    #[test]
    fn only_paths_relative_to_the_working_directory_count() {
        for relative in ["id_ed25519", "keys/id", "./k", "~user/k", "~"] {
            assert!(is_cwd_relative(relative), "{relative}");
        }
        for anchored in ["/home/me/.ssh/id", "~/.ssh/id"] {
            assert!(!is_cwd_relative(anchored), "{anchored}");
        }
        // `~\` is the home directory only where `\` separates paths.
        assert_eq!(is_cwd_relative("~\\.ssh\\id"), cfg!(unix));
        // Windows anchors: a root, or a drive with a root.
        assert_eq!(is_cwd_relative("C:\\keys\\id"), cfg!(unix));
        assert_eq!(is_cwd_relative("\\keys\\id"), cfg!(unix));
        assert!(is_cwd_relative("C:keys"), "drive-relative");
        assert!(!is_relative(Local::No, "keys/id"));
        assert!(!is_relative(Local::Program, "ssh"), "a PATH lookup");
        assert!(is_relative(Local::Program, "bin/ssh"));
        assert_eq!(is_relative(Local::Program, "bin\\ssh"), cfg!(windows));
    }

    #[test]
    fn new_relative_paths_skip_values_the_record_already_held() {
        let mut before = Defaults::default();
        before.ssh.identity_file = Some("keys/old".into());
        let mut after = before.clone();
        after.ssh.ssh_config_file = Some("conf/ssh".into());
        assert_eq!(
            new_relative_paths(Some(&before), &after),
            [("--ssh-config", "conf/ssh".to_string())]
        );
        assert_eq!(
            new_relative_paths(None, &after).len(),
            2,
            "a new record: all"
        );
    }

    #[test]
    fn empty_values_get_the_unset_remedy() {
        let server = Scope::Server;
        assert_eq!(empty_value_remedy(&[Setting::Port(22)], server), None);
        assert_eq!(
            empty_value_remedy(&[Setting::Notes(String::new()), Setting::Port(22)], server)
                .as_deref(),
            Some("to clear it: --unset notes")
        );
        let two = [Setting::User(String::new()), Setting::Key(String::new())];
        assert_eq!(
            empty_value_remedy(&two, Scope::Defaults).as_deref(),
            Some("to clear them: --unset user,key")
        );
        // The host is required: nothing to clear.
        let host = [Setting::Host(String::new()), Setting::Name(String::new())];
        assert_eq!(
            empty_value_remedy(&host, server).as_deref(),
            Some("to clear it: --unset name")
        );
        assert_eq!(empty_value_remedy(&host[..1], server), None);
    }
}
