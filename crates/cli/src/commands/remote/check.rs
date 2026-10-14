//! What a test found, as the CLI shows it: a server's login user and
//! tools ([`Login`]), a site's probe ([`SiteSummary`]), and the host keys
//! ssh trusted on the way. Built from the library's results once, so the
//! renderers (and the unit tests' fake remotes) work on plain values; the
//! field names are the `--json` keys.

use apvm_sites::remote::{Account, Creation, HostKeyNotices, Identity};
use apvm_sites::wp::probe::{HostFacts, UpgradeDir};
use apvm_sites::wp::{ProbeWarning, SiteProbe};
use serde::Serialize;

/// A server's test: what ssh said about host keys, then the login user
/// and tools, or why the test failed.
#[derive(Debug)]
pub struct ServerCheck {
    /// Host keys ssh recorded, could not save, or whose certificate it
    /// refused, on this test's calls.
    pub host_keys: HostKeyNotices,
    /// The login user and tools, or the failure.
    pub outcome: Result<Login, apvm_sites::Error>,
}

/// A site's test: what ssh said about host keys, then the probe's
/// findings, or why the probe failed.
#[derive(Debug)]
pub struct SiteCheck {
    /// Host keys ssh recorded, could not save, or whose certificate it
    /// refused, on this test's calls.
    pub host_keys: HostKeyNotices,
    /// What the probe found, or the failure.
    pub outcome: Result<SiteSummary, apvm_sites::Error>,
}

/// A user or group on the server: its id and, when usable, its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Named {
    /// The numeric id.
    pub id: u32,
    /// The name, when it has one apvm can use.
    pub name: Option<String>,
}

impl Named {
    /// The account as the library read it.
    ///
    /// # Arguments
    ///
    /// * `account` - The account
    pub fn of(account: &Account) -> Self {
        Self {
            id: account.id,
            name: account.name.as_ref().map(|name| name.as_str().to_string()),
        }
    }

    /// `name (id)`, or `id N` without a name.
    pub fn text(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} ({})", self.id),
            None => format!("id {}", self.id),
        }
    }

    /// The name, else the id: as in `owner:group`.
    pub fn short(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.id.to_string())
    }
}

/// The SSH login user and the server's tools (`server test`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Login {
    /// The login user.
    pub user: Named,
    /// Its primary group.
    pub group: Named,
    /// Every group id it belongs to.
    pub groups: Vec<u32>,
    /// `$SHELL`, when set.
    pub shell: Option<String>,
    /// Where `sudo` is, if installed.
    pub sudo: Option<String>,
    /// Where `runuser` is, if installed.
    pub runuser: Option<String>,
    /// The first checksum tool found (`sha256sum`, `shasum`, `openssl`).
    pub checksum: Option<String>,
    /// Where the login user finds the site's WP-CLI program (`wp` for a
    /// server).
    pub wp_cli: Option<String>,
}

impl Login {
    /// The login facts the library read.
    ///
    /// # Arguments
    ///
    /// * `facts` - Probe step H's login facts
    pub fn of(facts: &HostFacts) -> Self {
        Self {
            user: Named::of(&facts.login.user),
            group: Named::of(&facts.login.group),
            groups: facts.login.groups.clone(),
            shell: facts.shell.clone(),
            sudo: facts.sudo.clone(),
            runuser: facts.runuser.clone(),
            checksum: facts.checksum.clone(),
            wp_cli: facts.wp_cli_path.clone(),
        }
    }
}

/// Who WP-CLI runs as on the site, and the ownership plugin files get.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentitySummary {
    /// `login` (no prefix), `sudo` or `runuser`.
    pub switch: &'static str,
    /// The command prefix, as shown: `sudo -u www-data -g www-data`, or
    /// `the login user`.
    pub runs_as: String,
    /// The uid WP-CLI runs with.
    pub uid: u32,
    /// `at_creation` when new files get the reference owner and group,
    /// `check_after` when the post-install check repairs them.
    pub creation: &'static str,
    /// The owner plugin files must have.
    pub owner: Named,
    /// The group plugin files must have.
    pub group: Named,
}

/// The plugins directory the probe measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirSummary {
    /// Its physical path.
    pub path: String,
    /// Its owner.
    pub owner: Named,
    /// Its group.
    pub group: Named,
    /// Its mode, four octal digits (`2775`).
    pub mode: String,
    /// Whether only root's `stat` could read it.
    pub read_by_root: bool,
}

/// WP-CLI on the site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WpCliSummary {
    /// Its version (`cli version`).
    pub version: String,
    /// Where the login user finds the program, when it does.
    pub path: Option<String>,
}

/// What WordPress reports (observations, never trusted for the identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WordPressSummary {
    /// WordPress's version.
    pub version: String,
    /// PHP's version.
    pub php_version: String,
    /// Whether it is a multisite network.
    pub multisite: bool,
    /// The `siteurl` option.
    pub siteurl: String,
    /// The `home` option.
    pub home: String,
    /// Whether `DISALLOW_FILE_MODS` is set.
    pub disallow_file_mods: bool,
}

/// What the identity can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilitySummary {
    /// Whether it can write the plugins directory.
    pub plugins_dir_writable: bool,
    /// WordPress's upgrade directory: `writable`, `not_writable`,
    /// `missing` (created in a writable content directory),
    /// `missing_not_creatable`, or `unknown`.
    pub upgrade_dir: &'static str,
    /// Free KiB in the remote temporary directory, when known.
    pub tmp_free_kib: Option<u64>,
    /// Free KiB in the content directory, when known.
    pub content_free_kib: Option<u64>,
}

/// A finding that does not stop reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WarningSummary {
    /// Its stable code, when it has one (`plugins_dir_not_writable`).
    pub code: Option<&'static str>,
    /// What it means.
    pub message: String,
}

/// What a site's probe found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SiteSummary {
    /// The login user and tools.
    pub login: Login,
    /// The plugins directory.
    pub plugins_dir: DirSummary,
    /// Who WP-CLI runs as.
    pub identity: IdentitySummary,
    /// WP-CLI.
    pub wp_cli: WpCliSummary,
    /// WordPress.
    pub wordpress: WordPressSummary,
    /// What the identity can do.
    pub capabilities: CapabilitySummary,
    /// Findings that do not stop reads.
    pub warnings: Vec<WarningSummary>,
    /// PHP notices printed around WordPress's facts (escaped excerpts).
    pub php_notices: Vec<String>,
}

impl SiteSummary {
    /// The summary of `probe`.
    ///
    /// # Arguments
    ///
    /// * `probe` - A successful probe
    pub fn of(probe: &SiteProbe) -> Self {
        Self {
            login: Login::of(&probe.host),
            plugins_dir: DirSummary {
                path: probe.plugins_dir.path.clone(),
                owner: Named::of(&probe.plugins_dir.ownership.owner),
                group: Named::of(&probe.plugins_dir.ownership.group),
                mode: format!("{:04o}", probe.plugins_dir.ownership.mode.bits()),
                read_by_root: probe.plugins_dir.read_by_root,
            },
            identity: IdentitySummary {
                switch: switch_name(&probe.identity.identity),
                runs_as: identity_text(&probe.identity.identity),
                uid: probe.identity.uid,
                creation: creation_name(probe.identity.creation),
                owner: Named::of(&probe.identity.reference.owner),
                group: Named::of(&probe.identity.reference.group),
            },
            wp_cli: WpCliSummary {
                version: probe.wp_cli_version.to_string(),
                path: probe.host.wp_cli_path.clone(),
            },
            wordpress: WordPressSummary {
                version: probe.facts.wp_version.clone(),
                php_version: probe.facts.php_version.clone(),
                multisite: probe.facts.multisite,
                siteurl: probe.facts.siteurl.clone(),
                home: probe.facts.home.clone(),
                disallow_file_mods: probe.facts.disallow_file_mods,
            },
            capabilities: CapabilitySummary {
                plugins_dir_writable: probe.capabilities.plugins_dir_writable,
                upgrade_dir: upgrade_dir_name(probe.capabilities.upgrade_dir),
                tmp_free_kib: probe.capabilities.tmp_free_kib,
                content_free_kib: probe.capabilities.content_free_kib,
            },
            warnings: probe.warnings.iter().map(warning).collect(),
            php_notices: probe.notices.lines().to_vec(),
        }
    }
}

/// A probe warning as shown.
///
/// # Arguments
///
/// * `warning` - The library's warning
pub fn warning(warning: &ProbeWarning) -> WarningSummary {
    WarningSummary {
        code: warning.code().map(apvm_sites::Code::as_str),
        message: warning.to_string(),
    }
}

/// How the identity switches users: `login`, `sudo` or `runuser`.
///
/// # Arguments
///
/// * `identity` - The identity
pub fn switch_name(identity: &Identity) -> &'static str {
    match identity {
        Identity::Sudo { .. } => "sudo",
        Identity::Runuser { .. } => "runuser",
        // The login user, and any identity a later library adds that
        // switches nothing apvm knows of.
        _ => "login",
    }
}

/// The identity as shown: `the login user`, or its prefix without the
/// flags that never change (`sudo -u www-data -g www-data`).
///
/// # Arguments
///
/// * `identity` - The identity
pub fn identity_text(identity: &Identity) -> String {
    match identity {
        Identity::Sudo { user, group } => match group {
            Some(group) => format!("sudo -u {} -g {}", user.as_str(), group.as_str()),
            None => format!("sudo -u {}", user.as_str()),
        },
        Identity::Runuser { user, group } => {
            format!("runuser -u {} -g {}", user.as_str(), group.as_str())
        }
        _ => "the login user".to_string(),
    }
}

/// The `--json` name of `creation`.
///
/// # Arguments
///
/// * `creation` - Whether new files get the reference owner and group
pub fn creation_name(creation: Creation) -> &'static str {
    match creation {
        Creation::AtCreation => "at_creation",
        // `CheckAfter`, and whatever a later library adds: the safe
        // reading is that files are checked.
        _ => "check_after",
    }
}

/// The `--json` name of the upgrade directory's state.
///
/// # Arguments
///
/// * `dir` - What step C found
pub fn upgrade_dir_name(dir: UpgradeDir) -> &'static str {
    match dir {
        UpgradeDir::Writable => "writable",
        UpgradeDir::NotWritable => "not_writable",
        UpgradeDir::Missing {
            content_writable: true,
        } => "missing",
        UpgradeDir::Missing {
            content_writable: false,
        } => "missing_not_creatable",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use apvm_sites::remote::AccountName;

    use super::*;

    #[test]
    fn accounts_show_their_name_and_id() {
        let named = Named::of(&Account::new(33, AccountName::new("www-data").ok()));
        assert_eq!(named.text(), "www-data (33)");
        assert_eq!(named.short(), "www-data");
        let bare = Named::of(&Account::new(1005, None));
        assert_eq!(bare.text(), "id 1005");
        assert_eq!(bare.short(), "1005");
    }

    #[test]
    fn identities_read_as_their_prefix() {
        let sudo = Identity::sudo("www-data", Some("www-data")).expect("valid");
        assert_eq!(identity_text(&sudo), "sudo -u www-data -g www-data");
        assert_eq!(switch_name(&sudo), "sudo");
        let plain = Identity::sudo("ftpuser", None).expect("valid");
        assert_eq!(identity_text(&plain), "sudo -u ftpuser");
        let runuser = Identity::runuser("ftpuser", "www-data").expect("valid");
        assert_eq!(identity_text(&runuser), "runuser -u ftpuser -g www-data");
        assert_eq!(switch_name(&runuser), "runuser");
        assert_eq!(identity_text(&Identity::Login), "the login user");
        assert_eq!(switch_name(&Identity::Login), "login");
    }

    #[test]
    fn states_have_stable_json_names() {
        assert_eq!(creation_name(Creation::AtCreation), "at_creation");
        assert_eq!(creation_name(Creation::CheckAfter), "check_after");
        assert_eq!(upgrade_dir_name(UpgradeDir::Writable), "writable");
        assert_eq!(upgrade_dir_name(UpgradeDir::NotWritable), "not_writable");
        assert_eq!(
            upgrade_dir_name(UpgradeDir::Missing {
                content_writable: true
            }),
            "missing"
        );
        assert_eq!(
            upgrade_dir_name(UpgradeDir::Missing {
                content_writable: false
            }),
            "missing_not_creatable"
        );
        assert_eq!(upgrade_dir_name(UpgradeDir::Unknown), "unknown");
    }

    #[test]
    fn warnings_keep_their_code_when_they_have_one() {
        let shown = warning(&ProbeWarning::PluginsDirNotWritable { dir: "/p".into() });
        assert_eq!(shown.code, Some("plugins_dir_not_writable"));
        assert!(shown.message.contains("'/p'"), "{}", shown.message);
        assert_eq!(warning(&ProbeWarning::NoChecksumTool).code, None);
    }
}
