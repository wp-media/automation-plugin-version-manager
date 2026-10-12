//! The inventory's records: what `inventory.json` holds (plan §7).
//!
//! Records are plain data. Optional settings are `Option`s, where `None`
//! means "not set here", so a saved file stays sparse; effective values
//! (built-in defaults, inherited SSH settings) are resolved separately.
//! Records carry no invariants of their own: [`Inventory`](super::Inventory)
//! validates each one when a document is loaded or a record is inserted.
//! Every record refuses unknown fields, so a typo is an error, never a
//! silently ignored setting; `null` reads as "not set" wherever a field is
//! optional.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

/// The inventory format version this build reads and writes.
pub const INVENTORY_VERSION: u32 = 1;

/// The kinds of inventory records that have an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    /// A [`Server`].
    Server,
    /// A [`Site`].
    Site,
    /// A [`Group`].
    Group,
}

impl EntityKind {
    /// The lowercase name used in messages: `server`, `site` or `group`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Site => "site",
            Self::Group => "group",
        }
    }
}

impl fmt::Display for EntityKind {
    /// The lowercase name ([`EntityKind::as_str`]).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A record with an id, unique among records of its kind.
pub(crate) trait Record {
    /// The kind of record.
    const KIND: EntityKind;

    /// The record's id.
    fn id(&self) -> &str;
}

/// Read an optional field whose type has a natural "unset" value (empty
/// settings, an empty list): absent and `null` both give `T::default()`.
/// Pair with `#[serde(default)]`.
///
/// # Errors
///
/// Whatever `T` reports for a value that is neither `null` nor valid.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Settings every server inherits unless it sets its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Default SSH settings, applied field by field to every server.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "SshSettings::is_empty"
    )]
    pub ssh: SshSettings,
}

impl Defaults {
    /// Whether nothing is set (a saved document then omits `defaults`).
    pub fn is_empty(&self) -> bool {
        self.ssh.is_empty()
    }
}

/// SSH connection settings, in `defaults.ssh` or a server's `ssh`.
///
/// A server's value wins over the default field by field; anything neither
/// sets is left to the user's ssh_config. The local paths (`identity_file`,
/// `known_hosts_file`, `ssh_config_file`, `binary`) hold only
/// `[A-Za-z0-9._/\:+-]`, with `~` only as a leading `~/`: ssh expands, splits
/// or runs through a shell anything else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshSettings {
    /// Login user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// TCP port, 1–65535.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Private key file on this machine (the "one main key" in
    /// `defaults.ssh`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// Seconds allowed to establish a connection, 1–3600.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u32>,
    /// known_hosts file to use instead of the user's; never `none` or
    /// `/dev/null`, which would let any host key pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_hosts_file: Option<String>,
    /// ssh_config file to use instead of the user's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_config_file: Option<String>,
    /// Jump hosts as `ssh -J` takes them: a comma list of at most 8
    /// `[user@]host[:port]` hops; an IPv6 address in brackets only in the
    /// last hop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_jump: Option<String>,
    /// Whether to reuse one connection per server (OpenSSH multiplexing;
    /// Unix only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiplex: Option<bool>,
    /// The ssh program to run: a path or a command name. Only allowed in
    /// `defaults.ssh`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

impl SshSettings {
    /// Whether no field is set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// An SSH host that holds sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Unique server id: `^[a-z0-9][a-z0-9._-]{0,63}$`.
    pub id: String,
    /// Display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Host name, IP address, or an alias from the user's ssh_config.
    pub host: String,
    /// SSH settings for this server, over `defaults.ssh`. `binary` is not
    /// allowed here.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "SshSettings::is_empty"
    )]
    pub ssh: SshSettings,
    /// Free-form notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Server {
    /// A server with only its required fields set.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique server id
    /// * `host` - Host name, IP address or ssh_config alias
    pub fn new(id: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            host: host.into(),
            ssh: SshSettings::default(),
            notes: None,
        }
    }
}

impl Record for Server {
    const KIND: EntityKind = EntityKind::Server;

    /// The server id.
    fn id(&self) -> &str {
        &self.id
    }
}

/// A WordPress install on a server, and how apvm works on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    /// Unique site id: `^[a-z0-9][a-z0-9._-]{0,63}$`.
    pub id: String,
    /// Display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Id of the server the site is on.
    pub server: String,
    /// The WordPress root on the server: absolute and canonical.
    pub path: String,
    /// The site URL, passed to WP-CLI as `--url` (needed on multisite).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The WP-CLI command (unset: `["wp"]`): one program (a command name or
    /// an absolute path), or a PHP interpreter, its `-n`/`-q` options and
    /// the absolute path of the WP-CLI script, such as
    /// `["/usr/bin/php8.3", "/opt/wp-cli.phar"]`. Nothing else may take
    /// arguments, so WP-CLI cannot be wrapped in another program. The
    /// program itself is trusted configuration: it runs as the site's
    /// identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wp_cli: Option<Vec<String>>,
    /// Who WP-CLI runs as (unset: [`RunAs::Auto`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_as: Option<RunAs>,
    /// Who must own the plugin files (unset: [`Owner::Auto`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
    /// How ownership is checked and repaired (unset:
    /// [`OwnershipPolicy::Mirror`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ownership: Option<OwnershipPolicy>,
    /// Pins a plugins directory other than `<path>/wp-content/plugins`
    /// (a site that sets `WP_PLUGIN_DIR`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins_dir: Option<String>,
    /// Whether WP-CLI may run as root, or the files be owned by root (unset:
    /// `false`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_root: Option<bool>,
    /// Remote temporary directory for staging and the site lock (unset:
    /// `/tmp`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_tmp_dir: Option<String>,
    /// Seconds a remote command may run, 1–86400 (unset: the operation's
    /// default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_timeout_secs: Option<u32>,
    /// Free-form notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Site {
    /// A site with only its required fields set.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique site id
    /// * `server` - Id of the server the site is on
    /// * `path` - The WordPress root on the server
    pub fn new(id: impl Into<String>, server: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            server: server.into(),
            path: path.into(),
            url: None,
            wp_cli: None,
            run_as: None,
            owner: None,
            ownership: None,
            plugins_dir: None,
            allow_root: None,
            remote_tmp_dir: None,
            command_timeout_secs: None,
            notes: None,
        }
    }
}

impl Record for Site {
    const KIND: EntityKind = EntityKind::Site;

    /// The site id.
    fn id(&self) -> &str {
        &self.id
    }
}

/// A named list of sites, for targeting several at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// Unique group id: `^[a-z0-9][a-z0-9._-]{0,63}$`.
    pub id: String,
    /// Ids of the member sites, each listed once, in the order given.
    pub sites: Vec<String>,
    /// Free-form notes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Group {
    /// A group without notes.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique group id
    /// * `sites` - Ids of the member sites
    pub fn new(id: impl Into<String>, sites: Vec<String>) -> Self {
        Self {
            id: id.into(),
            sites,
            notes: None,
        }
    }
}

impl Record for Group {
    const KIND: EntityKind = EntityKind::Group;

    /// The group id.
    fn id(&self) -> &str {
        &self.id
    }
}

/// Who WP-CLI runs as on a site (plan §6.2). Written as `auto`, `login` or
/// a user name. The two keywords cannot name users: validation refuses
/// `User("auto")` and `User("login")`, which would be written back as the
/// keywords and change meaning on reload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RunAs {
    /// The owner and group of the site's plugins directory.
    #[default]
    Auto,
    /// The SSH login user, without sudo.
    Login,
    /// This user, with the plugins directory's group. `root` (in any case)
    /// needs `allow_root`.
    User(String),
}

impl From<String> for RunAs {
    /// Read the written form; any other text names a user (checked by
    /// validation, so a document reports every problem at once).
    fn from(value: String) -> Self {
        match value.as_str() {
            "auto" => Self::Auto,
            "login" => Self::Login,
            _ => Self::User(value),
        }
    }
}

impl From<&str> for RunAs {
    /// Read the written form (see `From<String>`).
    fn from(value: &str) -> Self {
        Self::from(value.to_string())
    }
}

impl From<RunAs> for String {
    /// The written form ([`RunAs`]'s `Display`).
    fn from(value: RunAs) -> Self {
        value.to_string()
    }
}

impl fmt::Display for RunAs {
    /// `auto`, `login` or the user name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Login => f.write_str("login"),
            Self::User(user) => f.write_str(user),
        }
    }
}

/// Who must own a site's plugin files. Written as `auto`, `user` or
/// `user:group`. `auto` cannot name a user: validation refuses an explicit
/// user `auto`, which would be written back as the keyword.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Owner {
    /// The owner and group of the site's plugins directory.
    #[default]
    Auto,
    /// This user, and group when given (else the plugins directory's
    /// group). The user must own the plugins directory. `root` (in any case)
    /// as user or group needs `allow_root`.
    Explicit {
        /// The owning user.
        user: String,
        /// The owning group, when given.
        group: Option<String>,
    },
}

impl From<String> for Owner {
    /// Read the written form, splitting `user:group` at the first `:`;
    /// the parts are checked by validation.
    fn from(value: String) -> Self {
        if value == "auto" {
            return Self::Auto;
        }
        match value.split_once(':') {
            Some((user, group)) => Self::Explicit {
                user: user.to_string(),
                group: Some(group.to_string()),
            },
            None => Self::Explicit {
                user: value,
                group: None,
            },
        }
    }
}

impl From<&str> for Owner {
    /// Read the written form (see `From<String>`).
    fn from(value: &str) -> Self {
        Self::from(value.to_string())
    }
}

impl From<Owner> for String {
    /// The written form ([`Owner`]'s `Display`).
    fn from(value: Owner) -> Self {
        value.to_string()
    }
}

impl fmt::Display for Owner {
    /// `auto`, `user` or `user:group`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Explicit { user, group: None } => f.write_str(user),
            Self::Explicit {
                user,
                group: Some(group),
            } => write!(f, "{user}:{group}"),
        }
    }
}

/// How a site's plugin ownership is checked and repaired (plan §6.8).
/// Written as a string only (serde's `{"mirror": null}` form is refused).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", rename_all = "kebab-case")]
pub enum OwnershipPolicy {
    /// Check owner, group and group bits; repair without root.
    #[default]
    Mirror,
    /// As `mirror`, plus the guarded root `chown` for a foreign owner.
    MirrorRoot,
    /// Report only.
    Check,
    /// Neither check nor repair.
    Off,
}

impl OwnershipPolicy {
    /// Every policy, in documentation order.
    pub const ALL: [Self; 4] = [Self::Mirror, Self::MirrorRoot, Self::Check, Self::Off];

    /// The written form: `mirror`, `mirror-root`, `check` or `off`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::MirrorRoot => "mirror-root",
            Self::Check => "check",
            Self::Off => "off",
        }
    }
}

impl fmt::Display for OwnershipPolicy {
    /// The written form ([`OwnershipPolicy::as_str`]).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OwnershipPolicy {
    type Err = String;

    /// Parse the written form; the error lists every valid value.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|policy| policy.as_str() == value)
            .ok_or_else(|| {
                let valid: Vec<_> = Self::ALL.iter().map(|policy| policy.as_str()).collect();
                format!(
                    "unknown ownership policy {}; valid: {}",
                    crate::problem::quoted(value),
                    valid.join(", ")
                )
            })
    }
}

impl TryFrom<String> for OwnershipPolicy {
    type Error = String;

    /// Parse the written form (see `FromStr`); serde reads the field this
    /// way, so only a string is accepted.
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// The inventory document, as stored. Field order is the saved order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, expecting = "an inventory object")]
pub(crate) struct Document {
    /// Format version; required, so the format can evolve.
    pub(crate) version: u32,
    /// Settings every server inherits.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Defaults::is_empty"
    )]
    pub(crate) defaults: Defaults,
    /// Servers, in inventory order.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) servers: Vec<Server>,
    /// Sites, in inventory order.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) sites: Vec<Site>,
    /// Groups, in inventory order.
    #[serde(default, deserialize_with = "null_as_default")]
    pub(crate) groups: Vec<Group>,
}

impl Document {
    /// An empty document of the current version.
    pub(crate) fn empty() -> Self {
        Self {
            version: INVENTORY_VERSION,
            defaults: Defaults::default(),
            servers: Vec::new(),
            sites: Vec::new(),
            groups: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A site with every field set.
    fn full_site() -> Site {
        Site {
            id: "blog".into(),
            name: Some("Blog".into()),
            server: "eu-1".into(),
            path: "/var/www/blog".into(),
            url: Some("https://blog.example.com".into()),
            wp_cli: Some(vec!["/usr/bin/php8.3".into(), "/opt/wp-cli.phar".into()]),
            run_as: Some(RunAs::Login),
            owner: Some(Owner::Explicit {
                user: "ftpuser".into(),
                group: Some("www-data".into()),
            }),
            ownership: Some(OwnershipPolicy::MirrorRoot),
            plugins_dir: Some("/var/www/blog/app/plugins".into()),
            allow_root: Some(false),
            remote_tmp_dir: Some("/var/tmp".into()),
            command_timeout_secs: Some(600),
            notes: Some("ftpuser:www-data".into()),
        }
    }

    /// SSH settings with every field set.
    fn full_ssh() -> SshSettings {
        SshSettings {
            user: Some("deploy".into()),
            port: Some(2222),
            identity_file: Some("~/.ssh/id_ed25519".into()),
            connect_timeout_secs: Some(15),
            known_hosts_file: Some("~/.ssh/known_hosts_apvm".into()),
            ssh_config_file: Some("~/.ssh/config_apvm".into()),
            proxy_jump: Some("bastion".into()),
            multiplex: Some(true),
            binary: Some("/usr/bin/ssh".into()),
        }
    }

    /// The message of the serde_json error from parsing `json` as `T`.
    fn refusal<T: for<'de> Deserialize<'de> + fmt::Debug>(json: &str) -> String {
        match serde_json::from_str::<T>(json) {
            Ok(value) => panic!("expected {json} to be refused, got {value:?}"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn run_as_reads_and_writes_its_three_forms() {
        assert_eq!(RunAs::from("auto"), RunAs::Auto);
        assert_eq!(RunAs::from("login"), RunAs::Login);
        assert_eq!(RunAs::from("deploy"), RunAs::User("deploy".into()));
        for written in ["auto", "login", "deploy", "Auto"] {
            assert_eq!(RunAs::from(written).to_string(), written);
        }
        assert_eq!(RunAs::default(), RunAs::Auto);
        assert_eq!(String::from(RunAs::Login), "login");
    }

    #[test]
    fn owner_reads_and_writes_user_and_group() {
        assert_eq!(Owner::from("auto"), Owner::Auto);
        assert_eq!(
            Owner::from("ftpuser"),
            Owner::Explicit {
                user: "ftpuser".into(),
                group: None
            }
        );
        assert_eq!(
            Owner::from("ftpuser:www-data"),
            Owner::Explicit {
                user: "ftpuser".into(),
                group: Some("www-data".into())
            }
        );
        // Only the first ':' splits; validation refuses what remains.
        assert_eq!(
            Owner::from("a:b:c"),
            Owner::Explicit {
                user: "a".into(),
                group: Some("b:c".into())
            }
        );
        assert_eq!(
            Owner::from(":"),
            Owner::Explicit {
                user: String::new(),
                group: Some(String::new())
            }
        );
        for written in ["auto", "ftpuser", "ftpuser:www-data", "auto:www-data"] {
            assert_eq!(Owner::from(written).to_string(), written);
        }
        assert_eq!(Owner::default(), Owner::Auto);
        assert_eq!(String::from(Owner::from("a:b")), "a:b");
    }

    #[test]
    fn ownership_policies_are_kebab_case_strings() {
        for policy in OwnershipPolicy::ALL {
            let json = serde_json::to_string(&policy).unwrap();
            assert_eq!(json, format!("\"{policy}\""));
            assert_eq!(
                serde_json::from_str::<OwnershipPolicy>(&json).unwrap(),
                policy
            );
            assert_eq!(policy.as_str().parse::<OwnershipPolicy>(), Ok(policy));
        }
        assert_eq!(OwnershipPolicy::MirrorRoot.as_str(), "mirror-root");
        assert_eq!(OwnershipPolicy::default(), OwnershipPolicy::Mirror);
        assert_eq!(
            "Mirror".parse::<OwnershipPolicy>().unwrap_err(),
            "unknown ownership policy 'Mirror'; valid: mirror, mirror-root, check, off"
        );
        let message = refusal::<OwnershipPolicy>("\"mirror_root\"");
        assert!(
            message.contains("unknown ownership policy 'mirror_root'"),
            "{message}"
        );
    }

    #[test]
    fn ownership_policies_refuse_the_object_form() {
        // serde's externally tagged form of a unit variant.
        for json in [r#"{"mirror": null}"#, r#"{"check": null}"#, "1", "null"] {
            let message = refusal::<OwnershipPolicy>(json);
            assert!(message.contains("expected a string"), "{json}: {message}");
        }
    }

    #[test]
    fn records_round_trip_through_json() {
        let site = full_site();
        let json = serde_json::to_string(&site).unwrap();
        assert_eq!(serde_json::from_str::<Site>(&json).unwrap(), site);
        assert!(json.contains("\"owner\":\"ftpuser:www-data\""), "{json}");
        assert!(json.contains("\"run_as\":\"login\""), "{json}");
        assert!(json.contains("\"ownership\":\"mirror-root\""), "{json}");

        let server = Server {
            ssh: full_ssh(),
            name: Some("EU".into()),
            notes: Some("n".into()),
            ..Server::new("eu-1", "203.0.113.10")
        };
        let json = serde_json::to_string(&server).unwrap();
        assert_eq!(serde_json::from_str::<Server>(&json).unwrap(), server);

        let group = Group {
            notes: Some("QA".into()),
            ..Group::new("qa", vec!["shop".into(), "blog".into()])
        };
        let json = serde_json::to_string(&group).unwrap();
        assert_eq!(serde_json::from_str::<Group>(&json).unwrap(), group);
    }

    #[test]
    fn unset_fields_are_omitted() {
        assert_eq!(
            serde_json::to_string(&Site::new("shop", "eu-1", "/var/www/shop")).unwrap(),
            r#"{"id":"shop","server":"eu-1","path":"/var/www/shop"}"#
        );
        assert_eq!(
            serde_json::to_string(&Server::new("eu-1", "h")).unwrap(),
            r#"{"id":"eu-1","host":"h"}"#
        );
        assert_eq!(serde_json::to_string(&Defaults::default()).unwrap(), "{}");
        assert_eq!(
            serde_json::to_string(&Group::new("qa", Vec::new())).unwrap(),
            r#"{"id":"qa","sites":[]}"#
        );
    }

    #[test]
    fn null_means_unset_for_every_optional_field() {
        let site: Site = serde_json::from_str(
            r#"{"id":"s","server":"e","path":"/p","name":null,"url":null,"wp_cli":null,
                "run_as":null,"owner":null,"ownership":null,"plugins_dir":null,
                "allow_root":null,"remote_tmp_dir":null,"command_timeout_secs":null,
                "notes":null}"#,
        )
        .unwrap();
        assert_eq!(site, Site::new("s", "e", "/p"));
        let server: Server =
            serde_json::from_str(r#"{"id":"e","host":"h","name":null,"ssh":null,"notes":null}"#)
                .unwrap();
        assert_eq!(server, Server::new("e", "h"));
        let ssh: SshSettings = serde_json::from_str(
            r#"{"user":null,"port":null,"identity_file":null,"connect_timeout_secs":null,
                "known_hosts_file":null,"ssh_config_file":null,"proxy_jump":null,
                "multiplex":null,"binary":null}"#,
        )
        .unwrap();
        assert!(ssh.is_empty());
        let defaults: Defaults = serde_json::from_str(r#"{"ssh":null}"#).unwrap();
        assert!(defaults.is_empty());
        let doc: Document = serde_json::from_str(
            r#"{"version":1,"defaults":null,"servers":null,"sites":null,"groups":null}"#,
        )
        .unwrap();
        assert_eq!(doc, Document::empty());
    }

    #[test]
    fn every_record_refuses_unknown_fields() {
        let cases = [
            ("defaults", refusal::<Defaults>(r#"{"ssh":{},"sudo":true}"#)),
            ("ssh", refusal::<SshSettings>(r#"{"usr":"deploy"}"#)),
            (
                "server",
                refusal::<Server>(r#"{"id":"a","host":"h","hostname":"x"}"#),
            ),
            (
                "site",
                refusal::<Site>(r#"{"id":"a","server":"e","path":"/p","owner_group":"x"}"#),
            ),
            (
                "group",
                refusal::<Group>(r#"{"id":"a","sites":[],"members":[]}"#),
            ),
            (
                "document",
                refusal::<Document>(r#"{"version":1,"site":[]}"#),
            ),
        ];
        for (record, message) in cases {
            assert!(
                message.starts_with("unknown field `"),
                "{record}: {message}"
            );
        }
    }

    #[test]
    fn required_fields_are_required() {
        assert!(refusal::<Server>(r#"{"id":"a"}"#).starts_with("missing field `host`"));
        assert!(refusal::<Site>(r#"{"id":"a","server":"e"}"#).starts_with("missing field `path`"));
        assert!(refusal::<Group>(r#"{"id":"a"}"#).starts_with("missing field `sites`"));
        assert!(refusal::<Document>(r#"{"servers":[]}"#).starts_with("missing field `version`"));
        assert!(refusal::<Group>(r#"{"id":"a","sites":null}"#).contains("invalid type: null"));
        let doc: Document = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(doc, Document::empty());
    }

    #[test]
    fn wrong_types_are_refused() {
        let cases = [
            (
                refusal::<SshSettings>(r#"{"port":70000}"#),
                "invalid value: integer `70000`",
            ),
            (
                refusal::<SshSettings>(r#"{"port":"22"}"#),
                "invalid type: string \"22\"",
            ),
            (
                refusal::<SshSettings>(r#"{"port":22.0}"#),
                "invalid type: floating point",
            ),
            (
                refusal::<Document>(r#"{"version":-1}"#),
                "invalid value: integer `-1`",
            ),
            (
                refusal::<Document>(r#"{"version":1e0}"#),
                "invalid type: floating point",
            ),
            (
                refusal::<Site>(r#"{"id":"a","server":"e","path":"/p","run_as":1}"#),
                "invalid type: integer `1`",
            ),
            (
                refusal::<Site>(r#"{"id":"a","server":"e","path":"/p","ownership":"root"}"#),
                "unknown ownership policy 'root'",
            ),
            (
                refusal::<Site>(r#"{"id":"a","server":"e","path":"/p","allow_root":"yes"}"#),
                "invalid type: string \"yes\", expected a boolean",
            ),
            (refusal::<Document>("[]"), "an inventory object"),
        ];
        for (message, expected) in cases {
            assert!(message.contains(expected), "{message:?} lacks {expected:?}");
        }
    }

    #[test]
    fn empty_settings_report_empty() {
        assert!(SshSettings::default().is_empty());
        assert!(!full_ssh().is_empty());
        assert!(Defaults::default().is_empty());
        assert!(
            !Defaults {
                ssh: SshSettings {
                    multiplex: Some(false),
                    ..SshSettings::default()
                }
            }
            .is_empty()
        );
    }

    #[test]
    fn records_report_their_kind_and_id() {
        assert_eq!(Server::new("a", "h").id(), "a");
        assert_eq!(Site::new("b", "a", "/p").id(), "b");
        assert_eq!(Group::new("c", Vec::new()).id(), "c");
        assert_eq!(Server::KIND, EntityKind::Server);
        assert_eq!(Site::KIND.to_string(), "site");
        assert_eq!(Group::KIND.as_str(), "group");
    }
}
