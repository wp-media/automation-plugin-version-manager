//! The probe end to end through the real SSH transport and real shells,
//! hermetically (plan §12, Unix only): `fake-ssh` runs the remote command
//! on this machine with `PATH` set to a sandbox directory holding links to
//! the real `id`, `stat`, `readlink`, `printenv`, `env`, `test`, `df` and
//! `sh`, plus `fake-sudo` as `sudo` and `fake-wp` as `wp`
//! (`tests/support/fake-remote/`). The site is a temporary directory owned
//! by the user running the tests.
//!
//! What this covers that the fake transport cannot: the step scripts
//! through `sh -s` and their markers, this machine's `id`, `sh` (the walk) and
//! `stat` (BSD `stat -f` on macOS after GNU `-c` fails, GNU on Linux), and
//! the exact command shapes through `sudo`, `env` and `wp`.

use std::path::{Path, PathBuf};

use super::*;
use crate::error::Code;
use crate::inventory::{Defaults, Inventory, RunAs, Server, Site, SshSettings};
use crate::remote::{HostKeyPolicy, SshOptions, SshTransport};

/// The real tools the remote side may run.
const TOOLS: [&str; 8] = [
    "sh", "id", "stat", "readlink", "printenv", "env", "test", "df",
];
/// A system user present on macOS and Linux (uid 1 on both).
const OTHER_USER: &str = "daemon";

/// The committed fake programs.
///
/// # Arguments
///
/// * `name` - `fake-ssh`, `fake-sudo` or `fake-wp`
fn fake(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/support/fake-remote")
        .join(name);
    std::fs::canonicalize(path).unwrap()
}

/// Where `program` is on this machine's `PATH`.
///
/// # Arguments
///
/// * `program` - The program's name
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// One hermetic server and site.
struct Lab {
    /// The temporary directory (deleted on drop).
    dir: tempfile::TempDir,
    /// The site's WordPress root (canonical).
    root: PathBuf,
}

impl Lab {
    /// A lab with every tool linked, sudo included, and a WordPress root
    /// whose facts report its own plugins directory. `None` when a tool is
    /// missing on this machine.
    fn new() -> Option<Self> {
        let dir = tempfile::Builder::new()
            .prefix("apvm-probe-")
            .tempdir_in("/tmp")
            .unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        // The walk trusts links only in directories neither group nor
        // others may write: set the modes, whatever the umask (002 too).
        private_dir(&base, 0o700);
        let bin = base.join("bin");
        std::fs::create_dir(&bin).unwrap();
        for tool in TOOLS {
            let Some(real) = which(tool) else {
                eprintln!("skipped: no {tool} on PATH");
                return None;
            };
            std::os::unix::fs::symlink(real, bin.join(tool)).unwrap();
        }
        std::os::unix::fs::symlink(fake("fake-sudo"), bin.join("sudo")).unwrap();
        std::os::unix::fs::symlink(fake("fake-wp"), bin.join("wp")).unwrap();
        let root = base.join("site");
        for sub in ["wp-content/plugins", "wp-includes", ".fake-wp"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::write(root.join("wp-includes/version.php"), "<?php\n").unwrap();
        // BSD systems give new entries their parent's group (wheel under
        // /tmp): give the site the user's own, as a hosting account has.
        let (_, gid) = ids();
        for sub in ["", "wp-content", "wp-content/plugins"] {
            std::os::unix::fs::chown(root.join(sub), None, Some(gid)).unwrap();
            private_dir(&root.join(sub), 0o755);
        }
        let lab = Self { dir, root };
        lab.facts(&lab.plugins());
        lab.ssh_config();
        Some(lab)
    }

    /// `name` in the lab.
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn path(&self, name: &str) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap().join(name)
    }

    /// The site's plugins directory.
    fn plugins(&self) -> String {
        format!("{}/wp-content/plugins", self.root.display())
    }

    /// Make `eval` report `plugin_dir` as WordPress's plugins directory.
    ///
    /// # Arguments
    ///
    /// * `plugin_dir` - The directory
    fn facts(&self, plugin_dir: &str) {
        let content = format!("{}/wp-content", self.root.display());
        let json = format!(
            r#"{{"wp_cli_version":"2.12.0","php_version":"8.4.26","wp_version":"7.1.3","multisite":false,"plugin_dir":"{plugin_dir}","plugin_dir_realpath":"{plugin_dir}","content_dir":"{content}","content_dir_realpath":"{content}","siteurl":"http://lab.test","home":"http://lab.test","disallow_file_mods":false}}"#
        );
        std::fs::write(self.root.join(".fake-wp/facts.json"), json).unwrap();
    }

    /// The ssh_config file: fake-ssh's `path` directive.
    fn ssh_config(&self) {
        use std::os::unix::fs::PermissionsExt;
        let file = self.path("ssh_config");
        let text = format!("#fake-ssh: path {}\n", self.path("bin").display());
        std::fs::write(&file, text).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(self.path("id_ed25519"), "not a real key\n").unwrap();
        std::fs::create_dir(self.path("generated")).unwrap();
        std::fs::create_dir(self.path("home")).unwrap();
    }

    /// Write fake-sudo's policy.
    ///
    /// # Arguments
    ///
    /// * `rules` - Its lines
    fn sudo_policy(&self, rules: &[&str]) {
        let text: String = rules.iter().map(|rule| format!("{rule}\n")).collect();
        std::fs::write(self.path("bin/sudo-policy"), text).unwrap();
    }

    /// The text of `name` in the lab (`""` when missing).
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    /// fake-wp's log.
    fn wp_log(&self) -> String {
        std::fs::read_to_string(self.root.join(".fake-wp/log")).unwrap_or_default()
    }

    /// The transport and the site, changed by `edit`.
    ///
    /// # Arguments
    ///
    /// * `edit` - Changes to the site record
    fn setup(&self, edit: impl FnOnce(&mut Site)) -> (SshTransport, ResolvedSite) {
        let text = |name: &str| Some(self.path(name).to_str().unwrap().to_string());
        let settings = SshSettings {
            user: Some("deploy".into()),
            identity_file: text("id_ed25519"),
            known_hosts_file: text("known_hosts"),
            ssh_config_file: text("ssh_config"),
            binary: Some(fake("fake-ssh").to_str().unwrap().into()),
            ..SshSettings::default()
        };
        let mut inventory = Inventory::new();
        inventory.set_defaults(Defaults { ssh: settings }).unwrap();
        inventory
            .add_server(Server::new("lab", "lab.test"))
            .unwrap();
        let mut site = Site::new("lab", "lab", self.root.to_str().unwrap());
        site.command_timeout_secs = Some(60);
        edit(&mut site);
        inventory.add_site(site).unwrap();
        let options = SshOptions::new()
            .with_host_keys(HostKeyPolicy::Strict)
            .with_home(self.path("home"))
            .with_config_dir(self.path("generated"));
        let server = inventory.resolve_server("lab").unwrap();
        let site = inventory.resolve_site("lab").unwrap();
        (SshTransport::new(server, options), site)
    }

    /// Probe the site, changed by `edit`.
    ///
    /// # Arguments
    ///
    /// * `edit` - Changes to the site record
    async fn probe(&self, edit: impl FnOnce(&mut Site)) -> Result<SiteProbe> {
        let (transport, site) = self.setup(edit);
        probe(&transport, &site).await
    }
}

/// Give `dir` exactly `mode`, whatever the umask.
///
/// # Arguments
///
/// * `dir` - The directory
/// * `mode` - Its mode
fn private_dir(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// This process's uid and primary gid.
fn ids() -> (u32, u32) {
    let id = |flag: &str| -> u32 {
        let out = std::process::Command::new("id").arg(flag).output().unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    (id("-u"), id("-g"))
}

#[tokio::test]
async fn the_owner_of_the_tree_probes_end_to_end() {
    let Some(lab) = Lab::new() else { return };
    lab.sudo_policy(&[&format!("allow {}:*", whoami())]);
    let probe = lab.probe(|_| {}).await.unwrap();
    let (uid, gid) = ids();
    let meta = std::fs::metadata(lab.plugins()).unwrap();
    let dir_gid = std::os::unix::fs::MetadataExt::gid(&meta);
    // GNU `stat -c` on Linux; BSD `stat -f` after it on macOS.
    let mode = std::os::unix::fs::MetadataExt::mode(&meta) & 0o7777;
    assert_eq!(probe.plugins_dir.ownership.mode.bits(), mode);
    assert_eq!(probe.host.login.user.id, uid);
    assert_eq!(probe.host.login.group.id, gid);
    assert_eq!(probe.plugins_dir.path, lab.plugins());
    assert_eq!(probe.plugins_dir.ownership.owner.id, uid);
    assert_eq!(probe.plugins_dir.ownership.group.id, dir_gid);
    assert!(!probe.plugins_dir.read_by_root);
    assert_eq!(probe.identity.uid, uid);
    if dir_gid == gid {
        assert_eq!(probe.identity.identity, Identity::Login);
        assert_eq!(probe.identity.creation, Creation::AtCreation);
    }
    assert!(probe.capabilities.plugins_dir_writable);
    assert!(probe.capabilities.tmp_free_kib.is_some());
    assert_eq!(probe.wp_cli_version.to_string(), "2.12.0");
    let log = lab.wp_log();
    assert!(log.contains("arg: cli\narg: version\n"), "{log}");
    assert!(log.contains("arg: eval\n"), "{log}");
    // The hermetic environment reached WP-CLI; no --allow-root.
    assert!(
        log.contains("env: WP_CLI_CONFIG_PATH=/dev/null WP_CLI_REQUIRE=unset"),
        "{log}"
    );
    assert!(
        log.contains("allow-root: 0") && !log.contains("allow-root: 1"),
        "{log}"
    );
}

/// The user running the tests.
fn whoami() -> String {
    let out = std::process::Command::new("id")
        .arg("-un")
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn another_user_runs_through_sudo() {
    let Some(lab) = Lab::new() else { return };
    lab.sudo_policy(&[&format!("allow {OTHER_USER}:*")]);
    let probe = lab
        .probe(|site| site.run_as = Some(RunAs::from(OTHER_USER)))
        .await
        .unwrap();
    let Identity::Sudo { user, .. } = &probe.identity.identity else {
        panic!("a sudo identity: {:?}", probe.identity);
    };
    assert_eq!(user.as_str(), OTHER_USER);
    let sudo_log = lab.read("bin/sudo.log");
    assert!(
        sudo_log.contains(&format!("-n -u {OTHER_USER}")),
        "{sudo_log}"
    );
    assert!(sudo_log.contains("-- env -u EDITOR"), "{sudo_log}");
    assert!(sudo_log.contains("-- test -w"), "{sudo_log}");
}

#[tokio::test]
async fn sudo_refusals_are_classified_through_real_scripts() {
    let Some(lab) = Lab::new() else { return };
    let cases: [(&[&str], Code); 4] = [
        (&["requiretty"], Code::SudoRequiretty),
        (
            &[&format!("password {OTHER_USER}")],
            Code::SudoPasswordRequired,
        ),
        (&["allow root"], Code::SudoNotAllowed),
        (&[], Code::SudoPasswordRequired),
    ];
    for (rules, expected) in cases {
        lab.sudo_policy(rules);
        let result = lab
            .probe(|site| site.run_as = Some(RunAs::from(OTHER_USER)))
            .await;
        assert_eq!(result.unwrap_err().code(), expected, "{rules:?}");
    }
    // No sudo at all.
    std::fs::remove_file(lab.path("bin/sudo")).unwrap();
    let result = lab
        .probe(|site| site.run_as = Some(RunAs::from(OTHER_USER)))
        .await;
    assert_eq!(result.unwrap_err().code(), Code::SudoMissing);
}

#[tokio::test]
async fn wordpress_failures_are_classified_through_real_scripts() {
    let Some(lab) = Lab::new() else { return };
    lab.sudo_policy(&[&format!("allow {}:*", whoami())]);
    // Not WP-CLI's program.
    let result = lab
        .probe(|site| site.wp_cli = Some(vec!["apvm-no-such-wp".into()]))
        .await;
    assert_eq!(result.unwrap_err().code(), Code::WpCliMissing);
    // WordPress elsewhere.
    lab.facts("/srv/elsewhere/plugins");
    assert_eq!(
        lab.probe(|_| {}).await.unwrap_err().code(),
        Code::PluginsDirMismatch
    );
    lab.facts(&lab.plugins());
    // Not installed.
    std::fs::write(lab.root.join(".fake-wp/not-installed"), "").unwrap();
    assert_eq!(
        lab.probe(|_| {}).await.unwrap_err().code(),
        Code::WpNotInstalled
    );
    // No WordPress.
    std::fs::remove_file(lab.root.join("wp-includes/version.php")).unwrap();
    assert_eq!(
        lab.probe(|_| {}).await.unwrap_err().code(),
        Code::NotWordpress
    );
    // No plugins directory at all.
    std::fs::remove_dir(lab.plugins()).unwrap();
    assert_eq!(
        lab.probe(|_| {}).await.unwrap_err().code(),
        Code::NotWordpress
    );
}

#[tokio::test]
async fn release_links_of_the_login_user_are_followed_others_refused() {
    use std::os::unix::fs::PermissionsExt;
    let Some(lab) = Lab::new() else { return };
    lab.sudo_policy(&[&format!("allow {}:*", whoami())]);
    // `current -> site` in the lab's own 0700 directory: a deploy tool's
    // release link the site cannot replace.
    std::os::unix::fs::symlink("site", lab.path("current")).unwrap();
    let current = lab.path("current");
    let probe = lab
        .probe(|site| site.path = current.to_str().unwrap().into())
        .await
        .unwrap();
    assert_eq!(probe.plugins_dir.path, lab.plugins());
    // The same link in a directory anyone may write: refused.
    let open = lab.path("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::os::unix::fs::symlink("../site", open.join("cur")).unwrap();
    let linked = open.join("cur");
    let error = lab
        .probe(|site| site.path = linked.to_str().unwrap().into())
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch, "{error}");
    assert!(
        error.to_string().contains("('apvm: unprotected link cur')"),
        "{error}"
    );
    // A trusted link into a directory the site may write, whose entry the
    // site replaced by a link elsewhere: the walk checks the target's own
    // components, and refuses the planted link (audit 3).
    std::os::unix::fs::symlink(lab.path("site"), open.join("swapped")).unwrap();
    std::os::unix::fs::symlink("open/swapped", lab.path("trusted")).unwrap();
    let trusted = lab.path("trusted");
    let error = lab
        .probe(|site| site.path = trusted.to_str().unwrap().into())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("('apvm: unprotected link swapped')"),
        "{error}"
    );
    // A relative link through `..`, and a loop.
    std::fs::create_dir(lab.path("deploy")).unwrap();
    private_dir(&lab.path("deploy"), 0o755);
    std::os::unix::fs::symlink("../site", lab.path("deploy/current")).unwrap();
    let up = lab.path("deploy/current");
    let probe = lab
        .probe(|site| site.path = up.to_str().unwrap().into())
        .await
        .unwrap();
    assert_eq!(probe.plugins_dir.path, lab.plugins());
    std::os::unix::fs::symlink("loop-b", lab.path("loop-a")).unwrap();
    std::os::unix::fs::symlink("loop-a", lab.path("loop-b")).unwrap();
    let looped = lab.path("loop-a");
    let error = lab
        .probe(|site| site.path = looped.to_str().unwrap().into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("too many links"), "{error}");
}

#[tokio::test]
async fn a_link_in_a_group_writable_directory_is_refused() {
    let Some(lab) = Lab::new() else { return };
    lab.sudo_policy(&[&format!("allow {}:*", whoami())]);
    let shared = lab.path("shared");
    std::fs::create_dir(&shared).unwrap();
    private_dir(&shared, 0o775);
    std::os::unix::fs::symlink("../site", shared.join("current")).unwrap();
    let current = shared.join("current");
    let error = lab
        .probe(|site| site.path = current.to_str().unwrap().into())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("('apvm: unprotected link current')"),
        "{error}"
    );
}

#[tokio::test]
async fn a_link_target_keeps_its_trailing_newline() {
    let Some(lab) = Lab::new() else { return };
    lab.sudo_policy(&[&format!("allow {}:*", whoami())]);
    // `nl -> "site\n"`: a command substitution would strip the newline and
    // land in `site`; the walk follows the real target, which is missing.
    std::os::unix::fs::symlink("site\n", lab.path("nl")).unwrap();
    let nl = lab.path("nl");
    let error = lab
        .probe(|site| site.path = nl.to_str().unwrap().into())
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::NotWordpress, "{error}");
}

#[tokio::test]
async fn a_component_named_like_an_option_is_walked_as_a_name() {
    let Some(lab) = Lab::new() else { return };
    lab.sudo_policy(&[&format!("allow {}:*", whoami())]);
    std::os::unix::fs::symlink("site", lab.path("-")).unwrap();
    let dash = lab.path("-");
    let probe = lab
        .probe(|site| site.path = dash.to_str().unwrap().into())
        .await
        .unwrap();
    assert_eq!(probe.plugins_dir.path, lab.plugins());
}
