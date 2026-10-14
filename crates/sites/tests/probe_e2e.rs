//! The probe (plan §6.2, P4.2) against the E2E harness's real WordPress,
//! WP-CLI, sudo and OpenSSH (`tests/e2e/sites/`), through the system ssh
//! client: each harness user's identity on each site, every sudo refusal,
//! a `WP_PLUGIN_DIR` moved in wp-config.php (with and without a pinned
//! `plugins_dir`), an explicit owner that does not own the plugins
//! directory, a hostile WP-CLI config and package for the identity, and
//! login shells that parse the remote command differently or print noise.
//!
//! Gated like `ssh_e2e.rs`: every test returns at once unless
//! `APVM_E2E_SITES=1` and the harness address are set, as
//! `eval "$(tests/e2e/sites/harness.sh env)"` does. Tests that change a
//! site (through the harness's root login) restore it with a drop guard
//! (an undo command, then `harness.sh reset`), also when they fail; the
//! tests run one at a time, so none sees another's change.
//! Nothing here reads or writes the user's own SSH files.
//!
//! Unix only: the harness runs on macOS and Linux clients.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use apvm_sites::remote::{Creation, HostKeyPolicy, Identity, SshOptions, SshTransport};
use apvm_sites::wp::probe::{ProbeWarning, SiteProbe, probe};
use apvm_sites::{Code, Defaults, Inventory, Owner, RunAs, Server, Site, SshSettings};

/// One test at a time: some change the harness's sites.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `single`'s standard plugins directory.
const SINGLE_PLUGINS: &str = "/var/www/single/wp-content/plugins";
/// A plugins directory the wp-config.php tests move WordPress to.
const ALT_PLUGINS: &str = "/var/www/single/wp-content/plugins-alt";

/// The harness, from `harness.sh env`.
struct Harness {
    /// The target's address as seen from here.
    host: String,
    /// Its SSH port.
    port: u16,
    /// The key every harness user accepts.
    key: PathBuf,
    /// The target's host keys.
    known_hosts: PathBuf,
    /// The harness's own ssh_config (aliases `apvm-e2e-<user>`), for setup.
    ssh_config: PathBuf,
}

impl Harness {
    /// The harness, when the E2E tests are enabled.
    fn get() -> Option<Self> {
        if std::env::var("APVM_E2E_SITES").as_deref() != Ok("1") {
            eprintln!("skipped: set APVM_E2E_SITES=1 (eval \"$(tests/e2e/sites/harness.sh env)\")");
            return None;
        }
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        Some(Self {
            host: var("APVM_E2E_SSH_HOST"),
            port: var("APVM_E2E_SSH_PORT").parse().unwrap(),
            key: var("APVM_E2E_SSH_KEY").into(),
            known_hosts: var("APVM_E2E_KNOWN_HOSTS").into(),
            ssh_config: var("APVM_E2E_SSH_CONFIG").into(),
        })
    }

    /// Run `command` as root on the target (test setup), and assert it
    /// succeeded.
    ///
    /// # Arguments
    ///
    /// * `command` - Shell text for the target's root shell
    fn root(&self, command: &str) -> String {
        let out = Command::new("ssh")
            .arg("-F")
            .arg(&self.ssh_config)
            .args(["apvm-e2e-root", command])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Restore `sites` (files, database, WP-CLI user configs, /tmp
    /// leftovers) with `harness.sh reset`.
    ///
    /// # Arguments
    ///
    /// * `sites` - The harness's site ids
    fn reset(&self, sites: &[&str]) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e/sites/harness.sh");
        let status = Command::new(script)
            .arg("reset")
            .args(sites)
            .status()
            .unwrap();
        assert!(status.success(), "harness.sh reset {sites:?}");
    }
}

/// Restores the harness when a test ends, panicking or not: runs an undo
/// command as root (if any) and resets sites, ignoring failures (a test
/// that panics must not cascade into the next one's state).
struct Restore<'a> {
    /// The harness.
    harness: &'a Harness,
    /// Shell text for root, run first (idempotent).
    undo: &'static str,
    /// The sites `harness.sh reset` restores.
    sites: &'static [&'static str],
}

impl Drop for Restore<'_> {
    /// Undo, then reset.
    fn drop(&mut self) {
        if !self.undo.is_empty() {
            let _ = Command::new("ssh")
                .arg("-F")
                .arg(&self.harness.ssh_config)
                .args(["apvm-e2e-root", self.undo])
                .status();
        }
        if !self.sites.is_empty() {
            let script =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e/sites/harness.sh");
            let _ = Command::new(script).arg("reset").args(self.sites).status();
        }
    }
}

/// A guard that restores `sites` (after `undo`) when dropped.
///
/// # Arguments
///
/// * `harness` - The harness
/// * `undo` - Idempotent shell text for root, or `""`
/// * `sites` - The sites to reset
fn restore<'a>(
    harness: &'a Harness,
    undo: &'static str,
    sites: &'static [&'static str],
) -> Restore<'a> {
    Restore {
        harness,
        undo,
        sites,
    }
}

/// A private home and an empty ssh_config file, so the user's own SSH
/// files stay out.
struct Sandbox {
    /// The directory (deleted on drop).
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A new sandbox under `/tmp`.
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("apvm-probe-e2e-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(dir.path().join("home")).unwrap();
        let config = dir.path().join("ssh_config");
        std::fs::write(&config, "").unwrap();
        std::fs::set_permissions(&config, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        Self { dir }
    }

    /// `name` in the sandbox, as text.
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn text(&self, name: &str) -> String {
        self.dir.path().join(name).to_str().unwrap().to_string()
    }
}

/// A harness site, as the inventory records it.
///
/// # Arguments
///
/// * `id` - `single` or `ftpsite`
fn harness_site(id: &str) -> Site {
    let (path, url) = match id {
        "single" => ("/var/www/single", "http://single.test"),
        "ftpsite" => ("/home/ftpuser/public_html", "http://ftpsite.test"),
        other => panic!("unknown harness site {other}"),
    };
    let mut site = Site::new(id, "harness", path);
    site.url = Some(url.into());
    site
}

/// Probe harness site `id` as `user`, the site changed by `edit`.
///
/// # Arguments
///
/// * `harness` - The harness
/// * `user` - The SSH login user
/// * `id` - The site
/// * `edit` - Changes to the site record
async fn probe_as(
    harness: &Harness,
    user: &str,
    id: &str,
    edit: impl FnOnce(&mut Site),
) -> apvm_sites::Result<SiteProbe> {
    let sandbox = Sandbox::new();
    let mut inventory = Inventory::new();
    let defaults = SshSettings {
        ssh_config_file: Some(sandbox.text("ssh_config")),
        connect_timeout_secs: Some(10),
        ..SshSettings::default()
    };
    inventory.set_defaults(Defaults { ssh: defaults }).unwrap();
    let mut server = Server::new("harness", &harness.host);
    server.ssh = SshSettings {
        user: Some(user.into()),
        port: Some(harness.port),
        identity_file: Some(harness.key.to_str().unwrap().into()),
        known_hosts_file: Some(harness.known_hosts.to_str().unwrap().into()),
        ..SshSettings::default()
    };
    inventory.add_server(server).unwrap();
    let mut site = harness_site(id);
    site.command_timeout_secs = Some(120);
    edit(&mut site);
    inventory.add_site(site).unwrap();
    let options = SshOptions::new()
        .with_host_keys(HostKeyPolicy::Strict)
        .with_home(sandbox.dir.path().join("home"))
        .with_config_dir(sandbox.dir.path());
    let transport = SshTransport::new(inventory.resolve_server("harness").unwrap(), options);
    probe(&transport, &inventory.resolve_site(id).unwrap()).await
}

/// `sudo -n -u <user> -g <group>`.
///
/// # Arguments
///
/// * `user` - The user
/// * `group` - The group
fn sudo(user: &str, group: &str) -> Identity {
    Identity::sudo(user, Some(group)).unwrap()
}

#[tokio::test]
async fn deploy_runs_as_www_data_with_its_group() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let probe = probe_as(&harness, "deploy", "single", |_| {})
        .await
        .unwrap();
    assert_eq!(probe.identity.identity, sudo("www-data", "www-data"));
    assert_eq!(
        (probe.identity.uid, probe.identity.creation),
        (33, Creation::AtCreation)
    );
    assert_eq!(probe.plugins_dir.path, SINGLE_PLUGINS);
    assert!(!probe.plugins_dir.read_by_root);
    assert_eq!(probe.plugins_dir.ownership.mode.bits(), 0o755);
    assert_eq!(probe.host.login.user.id, 2001);
    assert_eq!(probe.wp_cli_version.to_string(), "2.12.0");
    assert_eq!(probe.facts.wp_version, "7.1.3");
    assert_eq!(probe.facts.siteurl, "http://single.test");
    assert!(probe.capabilities.plugins_dir_writable);
    assert!(probe.capabilities.tmp_free_kib.is_some());
    assert!(probe.host.checksum.is_some());
    assert_eq!(probe.warnings, [], "{:?}", probe.warnings);
}

#[tokio::test]
async fn deploy_reaches_the_ftp_site_through_roots_stat() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let probe = probe_as(&harness, "deploy", "ftpsite", |_| {})
        .await
        .unwrap();
    assert!(probe.plugins_dir.read_by_root);
    assert_eq!(probe.plugins_dir.ownership.mode.bits(), 0o2775);
    assert_eq!(probe.identity.identity, sudo("ftpuser", "www-data"));
    assert_eq!(probe.identity.creation, Creation::AtCreation);
    assert!(probe.capabilities.plugins_dir_writable);
}

#[tokio::test]
async fn ftpuser_logs_in_as_the_owner_without_a_prefix() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let probe = probe_as(&harness, "ftpuser", "ftpsite", |_| {})
        .await
        .unwrap();
    assert_eq!(probe.identity.identity, Identity::Login);
    // Its primary group is ftpuser, not www-data: the post-install check
    // repairs what the setgid tree does not already fix.
    assert_eq!(probe.identity.creation, Creation::CheckAfter);
    // ftpuser belongs to www-data and the tree is setgid: no sudo is
    // tried (ftpuser has no rules; a refusal would be logged every probe).
    assert_eq!(probe.warnings, [ProbeWarning::CheckedAfterCreation]);
    assert!(probe.capabilities.plugins_dir_writable);
}

#[tokio::test]
async fn root_switches_with_runuser() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let probe = probe_as(&harness, "root", "single", |_| {}).await.unwrap();
    assert_eq!(
        probe.identity.identity,
        Identity::runuser("www-data", "www-data").unwrap()
    );
    assert_eq!(probe.identity.uid, 33);
}

#[tokio::test]
async fn sudo_refusals_get_their_codes() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let cases = [
        ("plain", Code::SudoPasswordRequired),
        ("pwsudo", Code::SudoPasswordRequired),
        // Listed by `sudo -n -l`, but the rule needs a password.
        ("mixed", Code::SudoPasswordRequired),
        ("ttyreq", Code::SudoRequiretty),
    ];
    for (user, code) in cases {
        let error = probe_as(&harness, user, "single", |_| {})
            .await
            .unwrap_err();
        assert_eq!(error.code(), code, "{user}: {error}");
    }
    // deploy has rules, but none for `plain`.
    let error = probe_as(&harness, "deploy", "single", |site| {
        site.run_as = Some(RunAs::from("plain"));
    })
    .await
    .unwrap_err();
    assert_eq!(error.code(), Code::SudoNotAllowed, "{error}");
    assert!(
        error
            .to_string()
            .contains("deploy ALL=(plain : www-data) NOPASSWD: ALL"),
        "{error}"
    );
}

#[tokio::test]
async fn a_moved_wp_plugin_dir_is_a_mismatch_until_pinned() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let _restore = restore(&harness, "", &["single"]);
    harness.root(&format!(
        "install -d -o ftpuser -g www-data -m 2775 {ALT_PLUGINS} && \
         sed -i \"1a define( 'WP_PLUGIN_DIR', '{ALT_PLUGINS}' );\" /var/www/single/wp-config.php"
    ));
    let mismatch = probe_as(&harness, "deploy", "single", |_| {}).await;
    let pinned = probe_as(&harness, "deploy", "single", |site| {
        site.plugins_dir = Some(ALT_PLUGINS.into());
    })
    .await;

    let error = mismatch.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch, "{error}");
    // The identity stays the one the standard directory's owner gave.
    assert!(
        error.to_string().contains(&format!(
            "WordPress uses the plugins directory '{ALT_PLUGINS}', not '{SINGLE_PLUGINS}' whose \
             owner decided the identity (sudo -u www-data -g www-data)"
        )),
        "{error}"
    );
    let probe = pinned.unwrap();
    assert_eq!(probe.plugins_dir.path, ALT_PLUGINS);
    assert_eq!(probe.identity.identity, sudo("ftpuser", "www-data"));
}

#[tokio::test]
async fn an_explicit_owner_must_own_the_plugins_dir() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let error = probe_as(&harness, "deploy", "single", |site| {
        site.owner = Some(Owner::from("ftpuser"));
    })
    .await
    .unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch, "{error}");
    let probe = probe_as(&harness, "deploy", "single", |site| {
        site.owner = Some(Owner::from("www-data:www-data"));
    })
    .await
    .unwrap();
    assert_eq!(probe.identity.identity, sudo("www-data", "www-data"));
}

#[tokio::test]
async fn a_hostile_wp_cli_config_and_package_have_no_effect() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    // Code that would print into the payload and leave a mark, from the
    // identity's user config (`require:`) and an installed package.
    let mark = "/tmp/apvm-e2e-hostile-ran";
    let code = format!("<?php echo \"hostile\"; touch( \"{mark}\" );");
    let _restore = restore(&harness, "", &["ftpsite"]);
    harness.root(&format!(
        "rm -f {mark} && cd /home/ftpuser && install -d -o ftpuser -g ftpuser \
         .wp-cli .wp-cli/packages .wp-cli/packages/vendor && \
         printf '%s\\n' '{code}' > .wp-cli/hostile.php && \
         printf 'require:\\n  - /home/ftpuser/.wp-cli/hostile.php\\n' > .wp-cli/config.yml && \
         cp .wp-cli/hostile.php .wp-cli/packages/vendor/autoload.php && \
         chown -R ftpuser:ftpuser .wp-cli"
    ));
    let result = probe_as(&harness, "deploy", "ftpsite", |_| {}).await;
    let marked = harness.root(&format!("test -e {mark} && echo yes || echo no"));
    // Control: plain WP-CLI as the identity (sudo's HOME) does run it.
    let control = harness.root(&format!(
        "cd / && runuser -u ftpuser -g www-data -- env HOME=/home/ftpuser wp \
         --path=/home/ftpuser/public_html option get siteurl >/dev/null 2>&1; \
         test -e {mark} && echo yes || echo no"
    ));
    let probe = result.unwrap();
    assert_eq!(probe.identity.identity, sudo("ftpuser", "www-data"));
    assert!(probe.notices.is_empty());
    assert_eq!(marked.trim(), "no");
    assert_eq!(control.trim(), "yes", "the hostile setup must work");
}

#[tokio::test]
async fn unusual_login_shells_and_noise_probe_alike() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    for user in ["fishy", "noisy", "zshy", "tcshy", "dashy"] {
        let probe = probe_as(&harness, user, "single", |_| {})
            .await
            .unwrap_or_else(|error| panic!("{user}: {error}"));
        assert_eq!(
            probe.identity.identity,
            sudo("www-data", "www-data"),
            "{user}"
        );
        assert_eq!(probe.plugins_dir.path, SINGLE_PLUGINS, "{user}");
    }
}

#[tokio::test]
async fn wordpress_without_tables_is_not_installed() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let _restore = restore(&harness, "", &["single"]);
    harness.root(
        "sed -i \"s/^\\$table_prefix = .*/\\$table_prefix = 'none_';/\" \
         /var/www/single/wp-config.php",
    );
    let result = probe_as(&harness, "deploy", "single", |_| {}).await;
    harness.reset(&["single"]);
    assert_eq!(result.unwrap_err().code(), Code::WpNotInstalled);
    // A path without WordPress (its plugins directory pinned to one that
    // exists, so step W decides).
    let error = probe_as(&harness, "deploy", "single", |site| {
        site.path = "/var/www".into();
        site.plugins_dir = Some(SINGLE_PLUGINS.into());
    })
    .await
    .unwrap_err();
    assert_eq!(error.code(), Code::NotWordpress, "{error}");
    let error = probe_as(&harness, "deploy", "single", |site| {
        site.wp_cli = Some(vec!["/usr/local/bin/no-such-wp".into()]);
    })
    .await
    .unwrap_err();
    assert_eq!(error.code(), Code::WpCliMissing, "{error}");
}

/// A PHP line for the top of wp-config.php that leaves `mark` behind
/// whenever WordPress loads.
///
/// # Arguments
///
/// * `mark` - The file it creates
fn marker_line(mark: &str) -> String {
    format!("sed -i \"1a touch( '{mark}' );\"")
}

#[tokio::test]
async fn a_plugins_dir_replaced_by_a_link_is_refused_before_wordpress_loads() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    // What site code (www-data owns wp-content) can do: send the probe to
    // the login user's own home, so the identity would be deploy.
    let mark = "/tmp/apvm-e2e-linked-loaded";
    let _restore = restore(
        &harness,
        "cd /var/www/single/wp-content && if [ -L plugins ]; then rm plugins && \
         mv plugins.real plugins; fi",
        &["single"],
    );
    harness.root(&format!(
        "rm -f {mark} && cd /var/www/single/wp-content && mv plugins plugins.real && \
         ln -s /home/deploy plugins && {} /var/www/single/wp-config.php",
        marker_line(mark)
    ));
    let result = probe_as(&harness, "deploy", "single", |_| {}).await;
    let loaded = harness.root(&format!("test -e {mark} && echo yes || echo no"));
    harness.root("cd /var/www/single/wp-content && rm plugins && mv plugins.real plugins");
    // Control: unlinked again, the probe loads WordPress and the mark appears.
    let control = probe_as(&harness, "deploy", "single", |_| {}).await;
    let marked = harness.root(&format!("test -e {mark} && echo yes || echo no"));
    assert!(control.is_ok(), "{control:?}");
    assert_eq!(marked.trim(), "yes", "the mark works");
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch, "{error}");
    assert!(
        error
            .to_string()
            .contains("('apvm: unprotected link plugins')"),
        "{error}"
    );
    assert_eq!(loaded.trim(), "no", "WordPress never loaded");
}

#[tokio::test]
async fn a_linked_content_dir_behind_roots_stat_is_refused_before_wordpress_loads() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    // deploy cannot reach ftpsite, so root's stat reads it, following the
    // linked wp-content; the identity's own walk catches the link.
    let mark = "/tmp/apvm-e2e-linked-loaded";
    let site = "/home/ftpuser/public_html";
    let _restore = restore(
        &harness,
        "cd /home/ftpuser/public_html && if [ -L wp-content ]; then rm wp-content && \
         mv wp-content.real wp-content; fi",
        &["ftpsite"],
    );
    harness.root(&format!(
        "rm -f {mark} && cd {site} && mv wp-content wp-content.real && \
         ln -s {site}/wp-content.real wp-content && {} {site}/wp-config.php",
        marker_line(mark)
    ));
    let result = probe_as(&harness, "deploy", "ftpsite", |_| {}).await;
    let loaded = harness.root(&format!("test -e {mark} && echo yes || echo no"));
    harness.root(&format!(
        "cd {site} && rm wp-content && mv wp-content.real wp-content"
    ));
    let control = probe_as(&harness, "deploy", "ftpsite", |_| {}).await;
    let marked = harness.root(&format!("test -e {mark} && echo yes || echo no"));
    assert!(control.is_ok(), "{control:?}");
    assert_eq!(marked.trim(), "yes", "the mark works");
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch, "{error}");
    assert!(
        error
            .to_string()
            .contains("('apvm: unprotected link wp-content')"),
        "{error}"
    );
    assert_eq!(loaded.trim(), "no", "WordPress never loaded");
}

#[tokio::test]
async fn a_release_link_root_owns_is_followed() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    // A deploy tool's layout: `/var/www/current -> single`, the link and
    // /var/www root's (0755), so the site cannot replace it.
    let _restore = restore(&harness, "rm -f /var/www/apvm-e2e-current", &[]);
    harness.root("ln -sfn single /var/www/apvm-e2e-current");
    let result = probe_as(&harness, "deploy", "single", |site| {
        site.path = "/var/www/apvm-e2e-current".into();
    })
    .await;
    let probe = result.unwrap();
    assert_eq!(probe.plugins_dir.path, SINGLE_PLUGINS);
    assert_eq!(probe.identity.identity, sudo("www-data", "www-data"));
}

#[tokio::test]
async fn a_trusted_link_into_a_site_dir_cannot_reach_through_a_planted_link() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    // Root's `/var/www/apvm-e2e-html -> /home/ftpuser/public_html` (a
    // trusted link), and ftpuser, owner of its home, swapped `public_html`
    // for a link to the `single` site of www-data. deploy cannot enter
    // ftpuser's home, so root's stat (which follows both links) reads
    // single's plugins directory; the identity's own walk resolves the
    // trusted link's target component by component and refuses the
    // planted one before WordPress loads.
    let mark = "/tmp/apvm-e2e-swapped-loaded";
    let _restore = restore(
        &harness,
        "rm -f /var/www/apvm-e2e-html && cd /home/ftpuser && if [ -L public_html ]; then \
         rm public_html && mv public_html.real public_html; fi",
        &["single", "ftpsite"],
    );
    harness.root(&format!(
        "rm -f {mark} && ln -sfn /home/ftpuser/public_html /var/www/apvm-e2e-html && \
         cd /home/ftpuser && mv public_html public_html.real && \
         ln -s /var/www/single public_html && chown -h ftpuser:ftpuser public_html && \
         {} /var/www/single/wp-config.php",
        marker_line(mark)
    ));
    let result = probe_as(&harness, "deploy", "single", |site| {
        site.path = "/var/www/apvm-e2e-html".into();
    })
    .await;
    let loaded = harness.root(&format!("test -e {mark} && echo yes || echo no"));
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch, "{error}");
    assert!(
        error
            .to_string()
            .contains("('apvm: unprotected link public_html')"),
        "{error}"
    );
    assert_eq!(loaded.trim(), "no", "WordPress never loaded");
}

#[tokio::test]
async fn a_plugins_dir_the_login_cannot_enter_is_read_through_roots_stat() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    // www-data's plugins directory without access for others: the walk's
    // `cd` fails without a reason (dash), root's stat reads it.
    let _restore = restore(&harness, "", &["single"]);
    harness.root(&format!("chmod 0750 {SINGLE_PLUGINS}"));
    let probe = probe_as(&harness, "deploy", "single", |_| {})
        .await
        .unwrap();
    assert!(probe.plugins_dir.read_by_root);
    assert_eq!(probe.plugins_dir.path, SINGLE_PLUGINS);
    assert_eq!(probe.plugins_dir.ownership.mode.bits(), 0o750);
    assert_eq!(probe.identity.identity, sudo("www-data", "www-data"));
}
