//! JavaScript types for the site inventory ([`SiteInventory`](crate::sites_inventory::SiteInventory)),
//! with their conversions from the `apvm-sites` records.
//!
//! The records mirror `inventory.json` (plan §7) field for field, in
//! camelCase (`identity_file` → `identityFile`). Unset settings are absent,
//! never `null`-filled, so a record read from the inventory, changed and
//! written back keeps every setting it did not touch. Settings whose effect
//! depends on others (`runAs`, `owner`, the SSH fields) are kept as written:
//! effective values are resolved by the library when sites are used.
//!
//! These types declare the TypeScript shapes. Values coming *from* JS are
//! read strictly by [`crate::sites_input`] (unknown keys and wrong types are
//! refused); values going *to* JS are built here.

use apvm_sites::{Defaults, Group, OwnershipPolicy, Server, Site, SshSettings};
use napi_derive::napi;

/// How a site's plugin ownership is checked and repaired. The values are
/// the ones `inventory.json` and the CLI use.
///
/// Members are plain enumerable properties, like a compiled TypeScript
/// enum: `Object.values()` lists every value in declaration order.
#[napi(string_enum = "kebab-case")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsOwnershipPolicy {
    /// Check owner, group and group bits; repair without root (the default).
    Mirror,
    /// As `mirror`, plus the guarded root `chown` for a foreign owner.
    MirrorRoot,
    /// Report only.
    Check,
    /// Neither check nor repair.
    Off,
}

impl From<OwnershipPolicy> for JsOwnershipPolicy {
    /// The JS member of a library policy.
    fn from(policy: OwnershipPolicy) -> Self {
        match policy {
            OwnershipPolicy::Mirror => Self::Mirror,
            OwnershipPolicy::MirrorRoot => Self::MirrorRoot,
            OwnershipPolicy::Check => Self::Check,
            OwnershipPolicy::Off => Self::Off,
        }
    }
}

/// SSH connection settings, in `defaults.ssh` or a server's `ssh`. A
/// server's value wins over the default field by field; anything neither
/// sets is left to the user's ssh_config.
#[napi(object)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsSshSettings {
    /// Login user.
    pub user: Option<String>,
    /// TCP port, 1–65535.
    pub port: Option<u32>,
    /// Private key file on this machine (`~/` is expanded when used).
    pub identity_file: Option<String>,
    /// Seconds allowed to establish a connection, 1–3600.
    pub connect_timeout_secs: Option<u32>,
    /// known_hosts file to use instead of the user's (never `none` or
    /// `/dev/null`).
    pub known_hosts_file: Option<String>,
    /// ssh_config file to use instead of the user's.
    pub ssh_config_file: Option<String>,
    /// Jump hosts as `ssh -J` takes them: at most 8 comma-separated
    /// `[user@]host[:port]` hops.
    pub proxy_jump: Option<String>,
    /// Whether to reuse one connection per server (Unix only).
    pub multiplex: Option<bool>,
    /// The ssh program to run. Only allowed in `defaults.ssh`.
    pub binary: Option<String>,
}

impl From<&SshSettings> for JsSshSettings {
    /// The JS form of library SSH settings.
    fn from(ssh: &SshSettings) -> Self {
        Self {
            user: ssh.user.clone(),
            port: ssh.port.map(u32::from),
            identity_file: ssh.identity_file.clone(),
            connect_timeout_secs: ssh.connect_timeout_secs,
            known_hosts_file: ssh.known_hosts_file.clone(),
            ssh_config_file: ssh.ssh_config_file.clone(),
            proxy_jump: ssh.proxy_jump.clone(),
            multiplex: ssh.multiplex,
            binary: ssh.binary.clone(),
        }
    }
}

/// `ssh` as an optional record field: absent when nothing is set, as in
/// `inventory.json`.
fn sparse_ssh(ssh: &SshSettings) -> Option<JsSshSettings> {
    (!ssh.is_empty()).then(|| JsSshSettings::from(ssh))
}

/// Settings every server inherits unless it sets its own.
///
/// # TypeScript
///
/// ```typescript
/// inventory.setDefaults({ ssh: { user: 'deploy', identityFile: '~/.ssh/id_ed25519' } });
/// ```
#[napi(object)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsDefaults {
    /// Default SSH settings, applied field by field to every server.
    pub ssh: Option<JsSshSettings>,
}

impl From<&Defaults> for JsDefaults {
    /// The JS form of the library defaults.
    fn from(defaults: &Defaults) -> Self {
        Self {
            ssh: sparse_ssh(&defaults.ssh),
        }
    }
}

/// An SSH host that holds sites.
///
/// # TypeScript
///
/// ```typescript
/// inventory.upsertServer({ id: 'eu-1', host: '203.0.113.10', ssh: { user: 'ubuntu' } });
/// ```
#[napi(object)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsServer {
    /// Unique server id: lowercase letters, digits, `.`, `_`, `-` (≤ 64).
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Host name, IP address, or an alias from the user's ssh_config.
    pub host: String,
    /// SSH settings for this server, over `defaults.ssh` (`binary` is not
    /// allowed here).
    pub ssh: Option<JsSshSettings>,
    /// Free-form notes.
    pub notes: Option<String>,
}

impl From<&Server> for JsServer {
    /// The JS form of a library server.
    fn from(server: &Server) -> Self {
        Self {
            id: server.id.clone(),
            name: server.name.clone(),
            host: server.host.clone(),
            ssh: sparse_ssh(&server.ssh),
            notes: server.notes.clone(),
        }
    }
}

/// A WordPress install on a server, and how apvm works on it.
///
/// # TypeScript
///
/// ```typescript
/// inventory.upsertSite({
///   id: 'shop',
///   server: 'eu-1',
///   path: '/var/www/shop/public',
///   url: 'https://shop.example.com',
///   ownership: JsOwnershipPolicy.Check,
/// });
/// ```
#[napi(object)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsSite {
    /// Unique site id: lowercase letters, digits, `.`, `_`, `-` (≤ 64).
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Id of the server the site is on.
    pub server: String,
    /// The WordPress root on the server: absolute and canonical.
    pub path: String,
    /// The site URL, passed to WP-CLI as `--url` (needed on multisite).
    pub url: Option<String>,
    /// The WP-CLI command (unset: `['wp']`): one program, or a PHP
    /// interpreter, its `-n`/`-q` options and the absolute WP-CLI script.
    pub wp_cli: Option<Vec<String>>,
    /// Who WP-CLI runs as: `auto` (unset), `login`, or a user name.
    pub run_as: Option<String>,
    /// Who must own the plugin files: `auto` (unset), `user` or
    /// `user:group`.
    pub owner: Option<String>,
    /// How ownership is checked and repaired (unset: `mirror`).
    pub ownership: Option<JsOwnershipPolicy>,
    /// Pins a plugins directory other than `<path>/wp-content/plugins`.
    pub plugins_dir: Option<String>,
    /// Whether WP-CLI may run as root, or the files be owned by root
    /// (unset: `false`).
    pub allow_root: Option<bool>,
    /// Remote temporary directory for staging and the site lock (unset:
    /// `/tmp`).
    pub remote_tmp_dir: Option<String>,
    /// Seconds a remote command may run, 1–86400 (unset: the operation's
    /// default).
    pub command_timeout_secs: Option<u32>,
    /// Free-form notes.
    pub notes: Option<String>,
}

impl From<&Site> for JsSite {
    /// The JS form of a library site.
    fn from(site: &Site) -> Self {
        Self {
            id: site.id.clone(),
            name: site.name.clone(),
            server: site.server.clone(),
            path: site.path.clone(),
            url: site.url.clone(),
            wp_cli: site.wp_cli.clone(),
            run_as: site.run_as.as_ref().map(ToString::to_string),
            owner: site.owner.as_ref().map(ToString::to_string),
            ownership: site.ownership.map(JsOwnershipPolicy::from),
            plugins_dir: site.plugins_dir.clone(),
            allow_root: site.allow_root,
            remote_tmp_dir: site.remote_tmp_dir.clone(),
            command_timeout_secs: site.command_timeout_secs,
            notes: site.notes.clone(),
        }
    }
}

/// A named list of sites, for targeting several at once.
///
/// # TypeScript
///
/// ```typescript
/// inventory.upsertGroup({ id: 'qa', sites: ['shop', 'blog'] });
/// ```
#[napi(object)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsGroup {
    /// Unique group id: lowercase letters, digits, `.`, `_`, `-` (≤ 64).
    pub id: String,
    /// Ids of the member sites, each listed once.
    pub sites: Vec<String>,
    /// Free-form notes.
    pub notes: Option<String>,
}

impl From<&Group> for JsGroup {
    /// The JS form of a library group.
    fn from(group: &Group) -> Self {
        Self {
            id: group.id.clone(),
            sites: group.sites.clone(),
            notes: group.notes.clone(),
        }
    }
}

/// Options for `SiteInventory.save()`. An unknown key or a wrong type
/// throws `InvalidArg`.
#[napi(object)]
#[derive(Debug, Default)]
#[allow(
    dead_code,
    reason = "declares the TypeScript type; values are read by `sites_input`"
)]
pub struct SaveOptions {
    /// Overwrite the file whatever it holds now, even when another process
    /// changed it since it was loaded (e.g. to restore a backup). Default
    /// `false`: such a save is refused with `InventoryChanged`.
    pub force: Option<bool>,
}

#[cfg(test)]
mod tests {
    use apvm_sites::{Owner, RunAs};

    use super::*;

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
            multiplex: Some(false),
            binary: Some("/usr/bin/ssh".into()),
        }
    }

    #[test]
    fn ownership_policies_map_to_their_namesakes() {
        let pairs = [
            (OwnershipPolicy::Mirror, JsOwnershipPolicy::Mirror),
            (OwnershipPolicy::MirrorRoot, JsOwnershipPolicy::MirrorRoot),
            (OwnershipPolicy::Check, JsOwnershipPolicy::Check),
            (OwnershipPolicy::Off, JsOwnershipPolicy::Off),
        ];
        for (policy, js) in pairs {
            assert_eq!(JsOwnershipPolicy::from(policy), js);
        }
        assert_eq!(pairs.len(), OwnershipPolicy::ALL.len());
    }

    #[test]
    fn ssh_settings_keep_every_field() {
        let js = JsSshSettings::from(&full_ssh());
        assert_eq!(
            js,
            JsSshSettings {
                user: Some("deploy".into()),
                port: Some(2222),
                identity_file: Some("~/.ssh/id_ed25519".into()),
                connect_timeout_secs: Some(15),
                known_hosts_file: Some("~/.ssh/known_hosts_apvm".into()),
                ssh_config_file: Some("~/.ssh/config_apvm".into()),
                proxy_jump: Some("bastion".into()),
                multiplex: Some(false),
                binary: Some("/usr/bin/ssh".into()),
            }
        );
    }

    #[test]
    fn empty_ssh_settings_are_absent() {
        assert_eq!(sparse_ssh(&SshSettings::default()), None);
        assert_eq!(
            JsDefaults::from(&Defaults::default()),
            JsDefaults::default()
        );
        assert_eq!(JsServer::from(&Server::new("eu-1", "h")).ssh, None);
        let set = SshSettings {
            multiplex: Some(false),
            ..SshSettings::default()
        };
        assert_eq!(
            sparse_ssh(&set).and_then(|ssh| ssh.multiplex),
            Some(false),
            "a false flag is a setting, not an empty one"
        );
    }

    #[test]
    fn defaults_carry_their_ssh_settings() {
        let defaults = Defaults { ssh: full_ssh() };
        assert_eq!(
            JsDefaults::from(&defaults).ssh,
            Some(JsSshSettings::from(&full_ssh()))
        );
    }

    #[test]
    fn servers_keep_every_field() {
        let server = Server {
            name: Some("EU".into()),
            ssh: SshSettings {
                user: Some("ubuntu".into()),
                ..SshSettings::default()
            },
            notes: Some("n".into()),
            ..Server::new("eu-1", "203.0.113.10")
        };
        assert_eq!(
            JsServer::from(&server),
            JsServer {
                id: "eu-1".into(),
                name: Some("EU".into()),
                host: "203.0.113.10".into(),
                ssh: Some(JsSshSettings {
                    user: Some("ubuntu".into()),
                    ..JsSshSettings::default()
                }),
                notes: Some("n".into()),
            }
        );
    }

    #[test]
    fn sites_keep_every_field_in_its_written_form() {
        let site = Site {
            name: Some("Blog".into()),
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
            notes: Some("ftp".into()),
            ..Site::new("blog", "eu-1", "/var/www/blog")
        };
        assert_eq!(
            JsSite::from(&site),
            JsSite {
                id: "blog".into(),
                name: Some("Blog".into()),
                server: "eu-1".into(),
                path: "/var/www/blog".into(),
                url: Some("https://blog.example.com".into()),
                wp_cli: Some(vec!["/usr/bin/php8.3".into(), "/opt/wp-cli.phar".into()]),
                run_as: Some("login".into()),
                owner: Some("ftpuser:www-data".into()),
                ownership: Some(JsOwnershipPolicy::MirrorRoot),
                plugins_dir: Some("/var/www/blog/app/plugins".into()),
                allow_root: Some(false),
                remote_tmp_dir: Some("/var/tmp".into()),
                command_timeout_secs: Some(600),
                notes: Some("ftp".into()),
            }
        );
    }

    #[test]
    fn unset_site_settings_stay_absent() {
        let js = JsSite::from(&Site::new("shop", "eu-1", "/var/www/shop"));
        assert_eq!(
            (js.run_as, js.owner, js.ownership, js.wp_cli, js.allow_root),
            (None, None, None, None, None)
        );
    }

    #[test]
    fn explicit_keyword_settings_are_written_out() {
        // `auto` set explicitly is a setting (it survives a round trip as
        // written), unlike an unset field.
        let site = Site {
            run_as: Some(RunAs::Auto),
            owner: Some(Owner::Auto),
            ..Site::new("shop", "eu-1", "/var/www/shop")
        };
        let js = JsSite::from(&site);
        assert_eq!(js.run_as.as_deref(), Some("auto"));
        assert_eq!(js.owner.as_deref(), Some("auto"));
    }

    #[test]
    fn groups_keep_their_members_in_order() {
        let group = Group {
            notes: Some("QA".into()),
            ..Group::new("qa", vec!["shop".into(), "blog".into()])
        };
        assert_eq!(
            JsGroup::from(&group),
            JsGroup {
                id: "qa".into(),
                sites: vec!["shop".into(), "blog".into()],
                notes: Some("QA".into()),
            }
        );
    }
}
