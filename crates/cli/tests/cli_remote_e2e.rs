//! `apvm server test`, `apvm site test` and the probe on `site add` (plan
//! P4.3) against the E2E harness's real WordPress, WP-CLI, sudo and OpenSSH
//! (`tests/e2e/sites/`), through the real binary and the system ssh client.
//!
//! Gated like `crates/sites/tests/probe_e2e.rs`: every test returns at once
//! unless `APVM_E2E_SITES=1` and the harness address are set, as
//! `eval "$(tests/e2e/sites/harness.sh env)"` does. Read-only: no test
//! changes a site. Each runs in a hermetic [`common::Sandbox`] (its own
//! home and inventory), with a known_hosts copy and an empty ssh_config file
//! of its own, so the user's SSH files are never read or written.
//! Multiplexing is off: the sites crate's `ssh_e2e` covers it.
//!
//! Unix only: the harness runs on macOS and Linux clients.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use common::{Sandbox, assert_exit, stderr, stdout};

/// The harness, from `harness.sh env`.
struct Harness {
    /// The target's address as seen from here.
    host: String,
    /// Its SSH port.
    port: String,
    /// The key every harness user accepts.
    key: PathBuf,
    /// The target's host keys.
    known_hosts: PathBuf,
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
            port: var("APVM_E2E_SSH_PORT"),
            key: var("APVM_E2E_SSH_KEY").into(),
            known_hosts: var("APVM_E2E_KNOWN_HOSTS").into(),
        })
    }
}

/// A sandbox whose inventory reaches the harness: servers `deploy` and
/// `plain` (those login users), sites `single` and `ftpsite` as `deploy`,
/// `plainsite` as `plain`.
struct Lab {
    /// The sandbox the binary runs in.
    sandbox: Sandbox,
    /// The known_hosts copy the inventory names.
    known_hosts: PathBuf,
}

impl Lab {
    /// The lab, with the harness's host keys known (or none, `known`
    /// false).
    fn new(harness: &Harness, known: bool) -> Self {
        let sandbox = Sandbox::new();
        let known_hosts = sandbox.scratch().join("known_hosts");
        if known {
            fs::copy(&harness.known_hosts, &known_hosts).expect("copy known_hosts");
        }
        let ssh_config = sandbox.scratch().join("ssh_config");
        fs::write(&ssh_config, "").expect("ssh_config");
        fs::set_permissions(&ssh_config, fs::Permissions::from_mode(0o600)).expect("chmod");
        let server = |id: &str| {
            format!(
                r#"{{"id": "{id}", "host": "{}", "ssh": {{"user": "{id}"}}}}"#,
                harness.host
            )
        };
        let text = format!(
            r#"{{
  "version": 1,
  "defaults": {{"ssh": {{
    "port": {port},
    "identity_file": "{key}",
    "known_hosts_file": "{known}",
    "ssh_config_file": "{config}",
    "multiplex": false
  }}}},
  "servers": [{deploy}, {plain}],
  "sites": [
    {{"id": "single", "server": "deploy", "path": "/var/www/single", "url": "http://single.test"}},
    {{"id": "ftpsite", "server": "deploy", "path": "/home/ftpuser/public_html", "url": "http://ftpsite.test"}},
    {{"id": "plainsite", "server": "plain", "path": "/var/www/single", "url": "http://single.test"}}
  ],
  "groups": [{{"id": "qa", "sites": ["single", "ftpsite"]}}]
}}
"#,
            port = harness.port,
            key = harness.key.display(),
            known = known_hosts.display(),
            config = ssh_config.display(),
            deploy = server("deploy"),
            plain = server("plain"),
        );
        let path = sandbox.inventory_file();
        fs::create_dir_all(path.parent().expect("parent")).expect("~/.apvm");
        fs::write(path, text).expect("inventory");
        Self {
            sandbox,
            known_hosts,
        }
    }

    /// Run `apvm <args>` in the sandbox.
    fn run(&self, args: &[&str]) -> Output {
        self.sandbox.run(args)
    }
}

/// stdout parsed as one JSON document.
fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out))
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}):\n{}", stdout(out)))
}

#[test]
fn server_test_logs_in_and_reads_the_login_user() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let lab = Lab::new(&harness, true);
    let out = lab.run(&["server", "test", "deploy", "plain", "--json"]);
    assert_exit(&out, 0);
    let doc = json(&out);
    assert_eq!(doc["ok"], true);
    let deploy = &doc["servers"][0]["login"];
    assert_eq!(deploy["user"]["name"], "deploy");
    assert!(deploy["sudo"].is_string(), "{deploy}");
    assert!(deploy["wp_cli"].is_string(), "{deploy}");
    assert_eq!(doc["servers"][1]["login"]["user"]["name"], "plain");
    let out = lab.run(&["server", "test", "deploy"]);
    assert_exit(&out, 0);
    assert!(stdout(&out).contains("  ✓ connected\n  login     deploy ("));
}

#[test]
fn site_test_finds_each_sites_identity() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let lab = Lab::new(&harness, true);
    let out = lab.run(&["site", "test", "-g", "qa", "--json"]);
    assert_exit(&out, 0);
    let doc = json(&out);
    assert_eq!(doc["ok"], true);
    let single = &doc["sites"][0]["probe"];
    assert_eq!(
        single["identity"]["runs_as"],
        "sudo -u www-data -g www-data"
    );
    assert_eq!(single["identity"]["creation"], "at_creation");
    assert_eq!(
        single["plugins_dir"]["path"],
        "/var/www/single/wp-content/plugins"
    );
    assert_eq!(single["wp_cli"]["version"], "2.12.0");
    assert_eq!(single["wordpress"]["siteurl"], "http://single.test");
    let ftpsite = &doc["sites"][1]["probe"];
    assert_eq!(
        ftpsite["identity"]["runs_as"],
        "sudo -u ftpuser -g www-data"
    );
    assert_eq!(ftpsite["plugins_dir"]["read_by_root"], true);
    assert_eq!(ftpsite["plugins_dir"]["owner"]["name"], "ftpuser");
    assert_eq!(ftpsite["plugins_dir"]["group"]["name"], "www-data");

    let out = lab.run(&["site", "test", "single"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("  ✓ ready\n"), "{text}");
    assert!(
        text.contains("  runs as      sudo -u www-data -g www-data (uid 33)\n"),
        "{text}"
    );
}

#[test]
fn site_test_names_the_sudoers_rule_a_login_user_lacks() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let lab = Lab::new(&harness, true);
    let out = lab.run(&["site", "test", "plainsite"]);
    assert_exit(&out, 1);
    let text = stdout(&out);
    assert!(text.contains("  ✗ sudo_password_required: "), "{text}");
    assert!(
        text.contains("plain ALL=(www-data : www-data) NOPASSWD: ALL"),
        "{text}"
    );
    assert!(text.contains("visudo"), "{text}");
    assert_eq!(
        stderr(&out),
        "Error: site 'plainsite' failed its test (sudo_password_required)\n"
    );
}

#[test]
fn an_unknown_host_key_is_refused_then_recorded_with_the_flag() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let lab = Lab::new(&harness, false);
    let out = lab.run(&["server", "test", "deploy"]);
    assert_exit(&out, 1);
    let text = stdout(&out);
    assert!(text.contains("  ✗ ssh_host_key_unknown: "), "{text}");
    assert!(text.contains("--trust-new-host-key"), "{text}");
    assert!(!lab.known_hosts.exists(), "nothing recorded");

    let out = lab.run(&["site", "test", "single", "--trust-new-host-key"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains("  host key  trusted the new key of "),
        "{text}"
    );
    assert!(text.contains("  ✓ ready\n"), "{text}");
    let known = fs::read_to_string(&lab.known_hosts).expect("recorded");
    assert!(!known.is_empty());
    // Known now: strict checking passes.
    assert_exit(&lab.run(&["server", "test", "deploy"]), 0);
}

#[test]
fn site_add_probes_then_saves_or_refuses() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let lab = Lab::new(&harness, true);
    let out = lab.run(&[
        "site",
        "add",
        "multi",
        "--server",
        "deploy",
        "--path",
        "/var/www/multi",
        "--url",
        "http://multi.test",
    ]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("  WordPress    "), "{text}");
    assert!(text.contains(", multisite\n"), "{text}");
    assert!(text.ends_with("\nAdded site 'multi'.\n"), "{text}");

    let before = fs::read(lab.sandbox.inventory_file()).expect("inventory");
    let out = lab.run(&[
        "site",
        "add",
        "nowhere",
        "--server",
        "deploy",
        "--path",
        "/var/www/nowhere",
    ]);
    assert_exit(&out, 1);
    assert!(
        stdout(&out).contains("  ✗ not_wordpress: "),
        "{}",
        stdout(&out)
    );
    assert!(
        stderr(&out).starts_with("Error: site 'nowhere' was not added: its test failed"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        fs::read(lab.sandbox.inventory_file()).expect("inventory"),
        before
    );
}

#[test]
fn shared_connections_report_the_new_key_and_carry_every_probe() {
    let Some(harness) = Harness::get() else {
        return;
    };
    // The CLI's default: connections shared, sockets in ~/.apvm/ssh-mux of
    // a home short enough for socket paths (a macOS temporary directory is
    // not: apvm would fall back to /tmp/apvm-mux-<uid>).
    let lab = Lab::new(&harness, false);
    let inventory = fs::read_to_string(lab.sandbox.inventory_file())
        .expect("inventory")
        .replace(r#""multiplex": false"#, r#""multiplex": true"#);
    let home = tempfile::Builder::new()
        .prefix("apvmh")
        .tempdir_in("/tmp")
        .expect("short home");
    fs::create_dir(home.path().join(".apvm")).expect(".apvm");
    fs::write(home.path().join(".apvm/inventory.json"), inventory).expect("inventory");
    let run = |args: &[&str]| {
        lab.sandbox
            .run_with_env(args, &[("HOME", home.path().as_os_str())])
    };

    // The key is recorded by the call that starts the shared connection,
    // and reported once; both sites ride that connection.
    let out = run(&["site", "test", "-g", "qa", "--trust-new-host-key", "--json"]);
    assert_exit(&out, 0);
    let doc = json(&out);
    assert_eq!(doc["ok"], true, "{doc}");
    let recorded: Vec<&serde_json::Value> = doc["sites"]
        .as_array()
        .expect("sites")
        .iter()
        .flat_map(|site| site["host_keys"]["recorded"].as_array().expect("recorded"))
        .collect();
    assert_eq!(recorded.len(), 1, "one site reports the new key: {doc}");
    assert!(
        !fs::read_to_string(&lab.known_hosts)
            .expect("recorded")
            .is_empty()
    );
    let mux = home.path().join(".apvm/ssh-mux");
    let mode = fs::metadata(&mux).expect("created").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "private control directory");
    assert!(
        fs::read_dir(&mux).expect("listed").next().is_some(),
        "a master's socket"
    );

    // Strict checking now passes, over the shared connection.
    let out = run(&["site", "test", "-g", "qa"]);
    assert_exit(&out, 0);
    assert!(
        stdout(&out).ends_with("\n2 of 2 sites ready; 0 failed.\n"),
        "{}",
        stdout(&out)
    );
}
