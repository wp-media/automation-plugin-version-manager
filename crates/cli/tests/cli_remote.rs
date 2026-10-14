//! End-to-end tests of the commands that connect (`apvm server test`,
//! `apvm site test`, and the test `server add` / `site add` run first,
//! plan P4.3) through the real binary, hermetically (plan §12): the
//! inventory points `defaults.ssh.binary` at `fake-ssh`, which runs the
//! remote command on this machine with `PATH` set to a lab directory holding
//! links to the real `sh`, `id`, `stat`, … plus `fake-sudo` as `sudo` and
//! `fake-wp` as `wp` (`tests/support/fake-remote/`). Every ssh failure class
//! comes from fake-ssh's `fail` directive (OpenSSH's own messages).
//!
//! The site is a lab directory owned by the user running the tests, under
//! `/tmp` (its links are root's, so the probe's walk trusts them).
//! Multiplexing is off (`--no-multiplex` in the defaults), so nothing is
//! written outside the sandbox and the lab. Unix only, like the sandbox.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Sandbox, assert_exit, stderr, stdout};

/// The real tools the "server" may run.
const TOOLS: [&str; 8] = [
    "sh", "id", "stat", "readlink", "printenv", "env", "test", "df",
];

/// A committed fake program.
///
/// # Arguments
///
/// * `name` - `fake-ssh`, `fake-sudo` or `fake-wp`
fn fake(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/support/fake-remote")
        .join(name);
    fs::canonicalize(path).expect("the fake programs are committed")
}

/// Where `program` is on this machine's `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// `id <flag>`'s answer for the user running the tests.
fn id(flag: &str) -> String {
    let out = std::process::Command::new("id")
        .arg(flag)
        .output()
        .expect("id runs");
    String::from_utf8(out.stdout)
        .expect("UTF-8")
        .trim()
        .to_string()
}

/// Give `path` exactly `mode`, whatever the umask.
fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// One hermetic server (fake-ssh) and WordPress root, with an inventory
/// in the sandbox's default location.
struct Lab {
    /// The sandbox the binary runs in.
    sandbox: Sandbox,
    /// The lab directory (deleted on drop).
    _dir: tempfile::TempDir,
    /// Its canonical path.
    base: PathBuf,
}

impl Lab {
    /// A lab with every tool linked, and a WordPress root at `site/`.
    /// `None` when a tool is missing on this machine.
    fn new() -> Option<Self> {
        let dir = tempfile::Builder::new()
            .prefix("apvm-cli-remote-")
            .tempdir_in("/tmp")
            .expect("lab dir");
        let base = fs::canonicalize(dir.path()).expect("canonical");
        set_mode(&base, 0o700);
        let bin = base.join("bin");
        fs::create_dir(&bin).expect("bin");
        for tool in TOOLS {
            let Some(real) = which(tool) else {
                eprintln!("skipped: no {tool} on PATH");
                return None;
            };
            std::os::unix::fs::symlink(real, bin.join(tool)).expect("link");
        }
        std::os::unix::fs::symlink(fake("fake-sudo"), bin.join("sudo")).expect("link");
        std::os::unix::fs::symlink(fake("fake-wp"), bin.join("wp")).expect("link");
        fs::write(bin.join("sudo-policy"), format!("allow {}:*\n", id("-un"))).expect("policy");
        let lab = Self {
            sandbox: Sandbox::new(),
            _dir: dir,
            base,
        };
        lab.wordpress("site");
        fs::write(lab.path("id_ed25519"), "not a real key\n").expect("key");
        lab.ssh_config("ssh_config", "");
        lab.inventory("");
        Some(lab)
    }

    /// `name` in the lab.
    fn path(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }

    /// `name` in the lab, as a string.
    fn text(&self, name: &str) -> String {
        self.path(name).to_str().expect("UTF-8 path").to_string()
    }

    /// A WordPress root at `name`, owned by this user and its group, whose
    /// facts report its own plugins directory.
    fn wordpress(&self, name: &str) {
        let root = self.path(name);
        for sub in ["wp-content/plugins", "wp-includes", ".fake-wp"] {
            fs::create_dir_all(root.join(sub)).expect("site dirs");
        }
        fs::write(root.join("wp-includes/version.php"), "<?php\n").expect("version.php");
        // BSD systems give new entries their parent's group (wheel under
        // /tmp): give the site the user's own, as a hosting account has.
        let gid: u32 = id("-g").parse().expect("gid");
        for sub in ["", "wp-content", "wp-content/plugins"] {
            std::os::unix::fs::chown(root.join(sub), None, Some(gid)).expect("chgrp");
            set_mode(&root.join(sub), 0o755);
        }
        let plugins = format!("{}/wp-content/plugins", root.display());
        let content = format!("{}/wp-content", root.display());
        let facts = format!(
            r#"{{"wp_cli_version":"2.12.0","php_version":"8.4.26","wp_version":"7.1.3","multisite":false,"plugin_dir":"{plugins}","plugin_dir_realpath":"{plugins}","content_dir":"{content}","content_dir_realpath":"{content}","siteurl":"http://lab.test","home":"http://lab.test","disallow_file_mods":false}}"#
        );
        fs::write(root.join(".fake-wp/facts.json"), facts).expect("facts");
    }

    /// An ssh_config file `name` (`0600`) with fake-ssh's `path` directive
    /// and `extra` directives (`#fake-ssh: fail auth\n`, …).
    fn ssh_config(&self, name: &str, extra: &str) {
        let file = self.path(name);
        let text = format!("#fake-ssh: path {}\n{extra}", self.path("bin").display());
        fs::write(&file, text).expect("ssh_config");
        set_mode(&file, 0o600);
    }

    /// Write the inventory: defaults pointing at fake-ssh, server `lab`,
    /// site `lab` on it, then `extra` JSON members of `servers`/`sites`
    /// (each with a leading comma) before the closing brackets.
    fn inventory(&self, extra_servers: &str) {
        self.write_inventory(extra_servers, &self.site_json("lab", "site"));
    }

    /// The inventory text with `servers` added to server `lab` and
    /// `sites` as the sites.
    fn write_inventory(&self, extra_servers: &str, sites: &str) {
        let text = format!(
            r#"{{
  "version": 1,
  "defaults": {{"ssh": {{
    "user": "deploy",
    "identity_file": "{key}",
    "known_hosts_file": "{known}",
    "ssh_config_file": "{config}",
    "binary": "{ssh}",
    "multiplex": false
  }}}},
  "servers": [{{"id": "lab", "host": "lab.test"}}{extra_servers}],
  "sites": [{sites}],
  "groups": []
}}
"#,
            key = self.text("id_ed25519"),
            known = self.text("known_hosts"),
            config = self.text("ssh_config"),
            ssh = fake("fake-ssh").display(),
        );
        let path = self.sandbox.inventory_file();
        fs::create_dir_all(path.parent().expect("parent")).expect("~/.apvm");
        fs::write(path, text).expect("inventory");
    }

    /// A site record `id` on server `lab` at the lab's `root`.
    fn site_json(&self, id: &str, root: &str) -> String {
        format!(
            r#"{{"id": "{id}", "server": "lab", "path": "{}", "command_timeout_secs": 60}}"#,
            self.text(root)
        )
    }

    /// Run `apvm <args>` in the sandbox.
    fn run(&self, args: &[&str]) -> Output {
        self.sandbox.run(args)
    }

    /// The inventory's text.
    fn inventory_text(&self) -> String {
        fs::read_to_string(self.sandbox.inventory_file()).expect("inventory")
    }
}

/// stdout parsed as one JSON document.
fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out))
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}):\n{}", stdout(out)))
}

// ── server test ──────────────────────────────────────────────────────────

#[test]
fn server_test_shows_the_login_user_its_groups_and_tools() {
    let Some(lab) = Lab::new() else { return };
    let out = lab.run(&["server", "test", "lab"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(
        text.starts_with("Server 'lab' (deploy@lab.test)\n  ✓ connected\n"),
        "{text}"
    );
    let user = format!("  login     {} ({}), group ", id("-un"), id("-u"));
    assert!(text.contains(&user), "{text}");
    assert!(
        text.contains(&format!("  sudo      {}/sudo\n", lab.path("bin").display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("  wp        {}/wp\n", lab.path("bin").display())),
        "{text}"
    );
    assert!(text.contains("  runuser   not found\n"), "{text}");
    assert_eq!(stderr(&out), "");

    let out = lab.run(&["server", "test", "lab", "--json"]);
    assert_exit(&out, 0);
    let doc = json(&out);
    assert_eq!(doc["schema"], 1);
    assert_eq!(doc["ok"], true);
    let server = &doc["servers"][0];
    assert_eq!(server["destination"], "deploy@lab.test");
    assert_eq!(server["login"]["user"]["name"], id("-un"));
    assert_eq!(server["error"], serde_json::Value::Null);
}

#[test]
fn every_ssh_failure_class_has_its_code_and_remedy() {
    let Some(lab) = Lab::new() else { return };
    let classes = [
        ("auth", "ssh_auth_failed", "--user or --key"),
        (
            "host-key-unknown",
            "ssh_host_key_unknown",
            "--trust-new-host-key",
        ),
        (
            "host-key-changed",
            "ssh_host_key_changed",
            "ssh-keygen -R 'lab.test'",
        ),
        ("refused", "ssh_unreachable", "--host, --port or --jump"),
        ("dns", "ssh_unreachable", "--host, --port or --jump"),
        ("timeout", "ssh_timeout", "--connect-timeout <SECS>"),
    ];
    let mut servers = String::new();
    for (class, _, _) in classes {
        lab.ssh_config(class, &format!("#fake-ssh: fail {class}\n"));
        servers.push_str(&format!(
            r#", {{"id": "{class}", "host": "lab.test", "ssh": {{"ssh_config_file": "{}"}}}}"#,
            lab.text(class)
        ));
    }
    lab.inventory(&servers);
    let mut args = vec!["server", "test"];
    args.extend(classes.iter().map(|(class, _, _)| *class));
    args.push("--json");
    let out = lab.run(&args);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: 6 of 6 servers failed the test\n",
        "the report is the document"
    );
    let doc = json(&out);
    assert_eq!(doc["ok"], false);
    for (index, (class, code, remedy)) in classes.iter().enumerate() {
        let error = &doc["servers"][index]["error"];
        assert_eq!(error["code"], *code, "{class}");
        let remedies = error["remedies"].to_string();
        assert!(remedies.contains(remedy), "{class}: {remedies}");
    }

    // Text: the code, ssh's message and the remedy, then the summary.
    let out = lab.run(&["server", "test", "host-key-changed"]);
    assert_exit(&out, 1);
    let text = stdout(&out);
    assert!(
        text.contains(
            "  ✗ ssh_host_key_changed: the server's host key has changed: Host key for lab.test \
             has changed and you have requested strict checking.\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "ssh-keygen -R 'lab.test' -f {}",
            lab.text("known_hosts")
        )),
        "{text}"
    );
    assert_eq!(
        stderr(&out),
        "Error: server 'host-key-changed' failed its test (ssh_host_key_changed)\n"
    );
}

#[test]
fn a_new_host_key_is_trusted_only_with_the_flag_and_then_recorded() {
    let Some(lab) = Lab::new() else { return };
    lab.ssh_config("ssh_config", "#fake-ssh: host-key new\n");
    let out = lab.run(&["server", "test", "lab"]);
    assert_exit(&out, 1);
    assert!(
        stdout(&out).contains("  ✗ ssh_host_key_unknown: "),
        "{}",
        stdout(&out)
    );
    assert!(!lab.path("known_hosts").exists(), "nothing recorded");

    let out = lab.run(&["server", "test", "lab", "--trust-new-host-key"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains(&format!(
            "  host key  trusted the new key of lab.test and recorded it in {}\n",
            lab.text("known_hosts")
        )),
        "{text}"
    );
    let known = fs::read_to_string(lab.path("known_hosts")).expect("recorded");
    assert!(known.starts_with("lab.test ssh-ed25519 "), "{known}");

    let out = lab.run(&["site", "test", "lab", "--trust-new-host-key", "--json"]);
    assert_exit(&out, 0);
    assert_eq!(
        json(&out)["sites"][0]["host_keys"]["recorded"][0],
        "lab.test"
    );
}

// ── server add ───────────────────────────────────────────────────────────

#[test]
fn server_add_tests_first_and_without_a_terminal_saves_only_a_passing_server() {
    let Some(lab) = Lab::new() else { return };
    let out = lab.run(&["server", "add", "second", "--host", "lab.test"]);
    assert_exit(&out, 0);
    assert!(
        stdout(&out).starts_with("Server 'second' (deploy@lab.test)\n  ✓ connected\n"),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).ends_with("\nAdded server 'second'.\n"));
    assert!(lab.inventory_text().contains(r#""id": "second""#));

    lab.ssh_config("refusing", "#fake-ssh: fail refused\n");
    let before = lab.inventory_text();
    let config = lab.text("refusing");
    let out = lab.run(&[
        "server",
        "add",
        "third",
        "--host",
        "lab.test",
        "--ssh-config",
        &config,
    ]);
    assert_exit(&out, 1);
    assert!(
        stdout(&out).contains("  ✗ ssh_unreachable: "),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("pass --host, --port or --jump"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        stderr(&out),
        "Error: server 'third' was not added: its test failed (ssh_unreachable), and stdin is \
         not a terminal to ask whether to save it anyway\n  fix the problem and add it again, \
         or pass --no-test to save it without testing\n"
    );
    assert_eq!(lab.inventory_text(), before, "nothing written");

    let out = lab.run(&[
        "server",
        "add",
        "third",
        "--host",
        "lab.test",
        "--ssh-config",
        &config,
        "--no-test",
    ]);
    assert_exit(&out, 0);
    assert_eq!(stdout(&out), "Added server 'third'.\n");
}

// ── site test ────────────────────────────────────────────────────────────

#[test]
fn site_test_probes_the_site_end_to_end() {
    let Some(lab) = Lab::new() else { return };
    let out = lab.run(&["site", "test", "lab"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(
        text.starts_with("Site 'lab' on server 'lab' (deploy@lab.test)\n  ✓ ready\n"),
        "{text}"
    );
    let plugins = format!("{}/wp-content/plugins", lab.text("site"));
    assert!(
        text.contains(&format!("  plugins dir  {plugins} (")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "  runs as      the login user (uid {})\n",
            id("-u")
        )),
        "{text}"
    );
    assert!(text.contains("  WP-CLI       2.12.0 ("), "{text}");
    assert!(
        text.contains("  WordPress    7.1.3, PHP 8.4.26, single site\n"),
        "{text}"
    );
    assert!(text.contains("  site URL     http://lab.test\n"), "{text}");
    assert!(
        text.contains("  writable     plugins dir yes, upgrade dir "),
        "{text}"
    );

    let out = lab.run(&["site", "test", "--all-sites", "--json"]);
    assert_exit(&out, 0);
    let doc = json(&out);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["warnings"], serde_json::json!([]));
    let probe = &doc["sites"][0]["probe"];
    assert_eq!(probe["plugins_dir"]["path"], plugins);
    assert_eq!(probe["identity"]["switch"], "login");
    assert_eq!(
        probe["identity"]["uid"],
        id("-u").parse::<u64>().expect("uid")
    );
    assert_eq!(probe["wp_cli"]["version"], "2.12.0");
    assert_eq!(probe["wordpress"]["multisite"], false);
    assert_eq!(probe["capabilities"]["plugins_dir_writable"], true);
    // The probe ran WP-CLI in apvm's hermetic shape.
    let log = fs::read_to_string(lab.path("site/.fake-wp/log")).expect("fake-wp log");
    assert!(log.contains("arg: cli\narg: version\n"), "{log}");
    assert!(
        log.contains("env: WP_CLI_CONFIG_PATH=/dev/null WP_CLI_REQUIRE=unset"),
        "{log}"
    );
}

#[test]
fn site_test_reports_probe_failures_with_their_remedies() {
    let Some(lab) = Lab::new() else { return };
    // `nowp`: a directory without WordPress; `sudo`: run as another user,
    // whom fake-sudo refuses (no rule for them).
    fs::create_dir(lab.path("empty")).expect("empty dir");
    let sites = [
        lab.site_json("lab", "site"),
        lab.site_json("nowp", "empty"),
        r#"{"id": "sudo", "server": "lab", "path": "SITE", "run_as": "daemon"}"#
            .replace("SITE", &lab.text("site")),
    ]
    .join(", ");
    lab.write_inventory("", &sites);
    fs::write(lab.path("bin/sudo-policy"), "").expect("no rules");

    let out = lab.run(&["site", "test", "--all-sites", "-j", "2"]);
    assert_exit(&out, 1);
    let text = stdout(&out);
    assert!(text.contains("Site 'lab' on server 'lab'"), "{text}");
    assert!(text.contains("  ✗ not_wordpress: "), "{text}");
    assert!(
        text.contains("check the site's WordPress root: apvm site edit nowp --path <WP_ROOT>"),
        "{text}"
    );
    assert!(text.contains("  ✗ sudo_password_required: "), "{text}");
    assert!(text.contains("NOPASSWD"), "{text}");
    assert!(text.contains("visudo"), "{text}");
    assert!(
        text.ends_with("\n1 of 3 sites ready; 2 failed.\n"),
        "{text}"
    );
    // `lab` and `sudo` are one install: a warning, both still probed.
    let err = stderr(&out);
    assert!(
        err.contains("sites 'lab', 'sudo' are the same WordPress install"),
        "{err}"
    );
    assert!(
        err.ends_with("Error: 2 of 3 sites failed the test\n"),
        "{err}"
    );

    let out = lab.run(&["site", "test", "sudo", "--json"]);
    assert_exit(&out, 1);
    let error = &json(&out)["sites"][0]["error"];
    assert_eq!(error["code"], "sudo_password_required");
    assert!(
        error["message"]
            .as_str()
            .expect("message")
            .contains("NOPASSWD"),
        "{error}"
    );
}

#[test]
fn site_test_refuses_unknown_targets_before_connecting() {
    let Some(lab) = Lab::new() else { return };
    lab.ssh_config("ssh_config", "#fake-ssh: fail refused\n");
    let out = lab.run(&["site", "test", "nope", "--json"]);
    assert_exit(&out, 1);
    let doc = json(&out);
    assert_eq!(doc["error"]["code"], "invalid_input");
    assert_eq!(doc["error"]["problems"][0]["location"], "<ID>");
    let out = lab.run(&["site", "test"]);
    assert_exit(&out, 2);
    let out = lab.run(&["site", "test", "lab", "-j", "0"]);
    assert_exit(&out, 2);
}

// ── site add ─────────────────────────────────────────────────────────────

#[test]
fn site_add_probes_first_and_saves_a_ready_site() {
    let Some(lab) = Lab::new() else { return };
    lab.wordpress("second");
    let root = lab.text("second");
    let out = lab.run(&["site", "add", "second", "--server", "lab", "--path", &root]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(
        text.starts_with("Site 'second' on server 'lab' (deploy@lab.test)\n  ✓ ready\n"),
        "{text}"
    );
    assert!(text.ends_with("\nAdded site 'second'.\n"), "{text}");
    assert!(lab.inventory_text().contains(r#""id": "second""#));
}

#[test]
fn site_add_without_a_terminal_saves_nothing_when_the_probe_fails() {
    let Some(lab) = Lab::new() else { return };
    fs::create_dir(lab.path("empty")).expect("empty dir");
    let before = lab.inventory_text();
    let root = lab.text("empty");
    let out = lab.run(&["site", "add", "nowp", "--server", "lab", "--path", &root]);
    assert_exit(&out, 1);
    assert!(
        stdout(&out).contains("  ✗ not_wordpress: "),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("check the site's WordPress root: pass --path <WP_ROOT>"),
        "{}",
        stdout(&out)
    );
    assert!(
        stderr(&out).starts_with(
            "Error: site 'nowp' was not added: its test failed (not_wordpress), and stdin is not \
             a terminal"
        ),
        "{}",
        stderr(&out)
    );
    assert_eq!(lab.inventory_text(), before, "nothing written");

    // Refused input never connects: fake-ssh would fail every call here.
    lab.ssh_config("ssh_config", "#fake-ssh: fail refused\n");
    let out = lab.run(&[
        "site", "add", "x", "--server", "lab", "--path", &root, "--group", "nope",
    ]);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: group 'nope' does not exist\n  'apvm group list' shows the groups\n"
    );
    assert_eq!(stdout(&out), "", "no report: nothing was probed");

    let out = lab.run(&[
        "site",
        "add",
        "nowp",
        "--server",
        "lab",
        "--path",
        &root,
        "--no-test",
    ]);
    assert_exit(&out, 0);
    assert_eq!(stdout(&out), "Added site 'nowp'.\n");
}

// ── shared connections ───────────────────────────────────────────────────

#[test]
fn shared_connections_live_in_the_apvm_homes_ssh_mux_directory() {
    let Some(lab) = Lab::new() else { return };
    // Multiplexing on, with a home short enough for socket paths (a
    // macOS temporary directory is not: apvm would fall back to /tmp).
    let text = lab
        .inventory_text()
        .replace(r#""multiplex": false"#, r#""multiplex": true"#);
    fs::write(lab.sandbox.inventory_file(), text).expect("inventory");
    let home = tempfile::Builder::new()
        .prefix("apvmh")
        .tempdir_in("/tmp")
        .expect("short home");
    fs::create_dir(home.path().join(".apvm")).expect(".apvm");
    fs::copy(
        lab.sandbox.inventory_file(),
        home.path().join(".apvm/inventory.json"),
    )
    .expect("inventory in the short home");
    let out = lab.sandbox.run_with_env(
        &["server", "test", "lab"],
        &[("HOME", home.path().as_os_str())],
    );
    assert_exit(&out, 0);
    let mux = home.path().join(".apvm/ssh-mux");
    let mode = fs::metadata(&mux).expect("created").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "private control directory");
}
