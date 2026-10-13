//! End-to-end tests of the site-inventory commands (`apvm server …`,
//! `apvm site …`) through the real binary: the file on disk
//! (`~/.apvm/inventory.json` or `APVM_INVENTORY`), its `0600` mode, exit
//! codes, `--json`, `--cascade`, confirmation, and the inventory states that
//! must never be rewritten (invalid, newer, locked, symlinked).
//!
//! Each test runs inside a hermetic [`common::Sandbox`] whose fake `$HOME`
//! holds the default inventory, so the developer's real one is never
//! touched.
#![cfg(unix)]

mod common;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

use common::{Sandbox, assert_exit, stderr, stdout};

/// A hand-written inventory: `eu-1` with sites `shop` and `blog` (`shop` in
/// group `qa`), and `bare` without sites.
const SAMPLE: &str = r#"{
  "version": 1,
  "servers": [
    {"id": "eu-1", "host": "203.0.113.10"},
    {"id": "bare", "host": "bare.example.com"}
  ],
  "sites": [
    {"id": "shop", "server": "eu-1", "path": "/var/www/shop"},
    {"id": "blog", "server": "eu-1", "path": "/var/www/blog"}
  ],
  "groups": [{"id": "qa", "sites": ["shop"]}]
}
"#;

/// Write `text` as the sandbox's default inventory.
fn write_inventory(sandbox: &Sandbox, text: &str) {
    let path = sandbox.inventory_file();
    fs::create_dir_all(path.parent().expect("has a parent")).expect("create ~/.apvm");
    fs::write(&path, text).expect("write inventory");
}

/// The default inventory parsed as JSON.
fn inventory_json(sandbox: &Sandbox) -> serde_json::Value {
    let text = fs::read_to_string(sandbox.inventory_file()).expect("inventory exists");
    serde_json::from_str(&text).expect("inventory is JSON")
}

/// stdout parsed as JSON.
fn json_stdout(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out))
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", stdout(out)))
}

/// Unix permission bits of `path`.
fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("exists").permissions().mode() & 0o777
}

// ── lifecycle ────────────────────────────────────────────────────────────

#[test]
fn add_edit_show_list_remove_round_trip_in_the_default_inventory() {
    let sandbox = Sandbox::new();
    let file = sandbox.inventory_file();

    let out = sandbox.run(&[
        "server",
        "add",
        "eu-1",
        "--host",
        "203.0.113.10",
        "--name",
        "EU VPS",
    ]);
    assert_exit(&out, 0);
    assert_eq!(stdout(&out), "Added server 'eu-1'.\n");
    assert_eq!(mode(&file), 0o600, "the inventory is private");
    assert_eq!(
        mode(file.parent().expect("dir")),
        0o700,
        "~/.apvm is created private"
    );

    let out = sandbox.run(&[
        "server", "edit", "eu-1", "--user", "ubuntu", "--port", "2222",
    ]);
    assert_exit(&out, 0);
    assert_eq!(stdout(&out), "Updated server 'eu-1': user, port.\n");
    assert_eq!(mode(&file), 0o600, "a rewrite stays private");

    let out = sandbox.run(&["server", "list"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "ID    DESTINATION               SITES  NAME\n\
         eu-1  ubuntu@203.0.113.10:2222  0      EU VPS\n"
    );

    let out = sandbox.run(&["server", "show", "eu-1"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(
        text.starts_with("Server 'eu-1'\n  name   EU VPS\n"),
        "{text}"
    );
    assert!(
        text.contains("  user             ubuntu   server\n"),
        "{text}"
    );
    assert!(
        text.contains("  connect timeout  15 s     built-in\n"),
        "{text}"
    );

    let out = sandbox.run(&["server", "remove", "eu-1", "-y"]);
    assert_exit(&out, 0);
    assert_eq!(stdout(&out), "Removed server 'eu-1'.\n");
    assert_eq!(inventory_json(&sandbox)["servers"], serde_json::json!([]));
}

#[test]
fn reads_of_a_missing_inventory_create_nothing() {
    let sandbox = Sandbox::new();
    for args in [
        &["server", "list"][..],
        &["server", "list", "--json"],
        &["server", "defaults"],
        &["server", "defaults", "--json"],
    ] {
        assert_exit(&sandbox.run(args), 0);
    }
    let out = sandbox.run(&["server", "show", "eu-1"]);
    assert_exit(&out, 1);
    assert!(
        !sandbox.home().join(".apvm").exists(),
        "nothing was created"
    );
}

#[test]
fn bare_server_command_prints_usage_with_exit_2() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["server"]);
    assert_exit(&out, 2);
    for sub in ["defaults", "add", "edit", "list", "show", "remove"] {
        assert!(
            stderr(&out).contains(sub),
            "usage lists `{sub}`:\n{}",
            stderr(&out)
        );
    }
}

// ── defaults ─────────────────────────────────────────────────────────────

#[test]
fn defaults_set_show_and_unset() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&[
        "server",
        "defaults",
        "--user",
        "deploy",
        "--key",
        "~/.ssh/id_ed25519",
        "--connect-timeout",
        "20",
        "--ssh-binary",
        "/usr/bin/ssh",
        "--no-multiplex",
    ]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "Updated the SSH defaults: user, key, connect-timeout, multiplex, ssh-binary.\n"
    );
    assert_eq!(mode(&sandbox.inventory_file()), 0o600);

    let out = sandbox.run(&["server", "defaults"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    for line in [
        "  user             deploy\n",
        "  key              ~/.ssh/id_ed25519\n",
        "  connect timeout  20 s\n",
        "  multiplex        off\n",
        "  ssh program      /usr/bin/ssh\n",
        "  port             not set (left to ssh_config)\n",
    ] {
        assert!(text.contains(line), "{line:?} in:\n{text}");
    }

    let out = sandbox.run(&[
        "server",
        "defaults",
        "--unset",
        "key,multiplex",
        "--unset",
        "user",
    ]);
    assert_exit(&out, 0);
    let out = sandbox.run(&["server", "defaults", "--json"]);
    assert_exit(&out, 0);
    assert_eq!(
        json_stdout(&out),
        serde_json::json!({
            "schema": 1,
            "defaults": {"ssh": {"connect_timeout_secs": 20, "binary": "/usr/bin/ssh"}}
        })
    );
}

#[test]
fn defaults_and_servers_take_every_ssh_setting_and_clear_it() {
    // The defaults take jump hosts, a known_hosts and an ssh_config file;
    // a server takes its own connect timeout. Each can be cleared again.
    let sandbox = Sandbox::new();
    let out = sandbox.run(&[
        "server",
        "defaults",
        "--jump",
        "bastion.example.com",
        "--known-hosts",
        "/etc/apvm/known_hosts",
        "--ssh-config",
        "/etc/apvm/ssh_config",
    ]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "Updated the SSH defaults: jump, known-hosts, ssh-config.\n"
    );
    let out = sandbox.run(&[
        "server",
        "add",
        "eu-1",
        "--host",
        "h",
        "--connect-timeout",
        "30",
    ]);
    assert_exit(&out, 0);
    let out = sandbox.run(&["server", "show", "eu-1"]);
    let text = stdout(&out);
    for line in [
        "  connect timeout  30 s                   server\n",
        "  known hosts      /etc/apvm/known_hosts  defaults\n",
        "  ssh config       /etc/apvm/ssh_config   defaults\n",
        "  jump hosts       bastion.example.com    defaults\n",
    ] {
        assert!(text.contains(line), "{line:?} in:\n{text}");
    }

    let out = sandbox.run(&[
        "server",
        "defaults",
        "--unset",
        "jump,known-hosts,ssh-config",
    ]);
    assert_exit(&out, 0);
    let out = sandbox.run(&["server", "edit", "eu-1", "--unset", "connect-timeout"]);
    assert_eq!(stdout(&out), "Updated server 'eu-1': connect-timeout.\n");
    assert_eq!(
        inventory_json(&sandbox),
        serde_json::json!({
            "version": 1,
            "servers": [{"id": "eu-1", "host": "h"}],
            "sites": [],
            "groups": []
        })
    );
}

#[test]
fn an_empty_value_names_the_unset_that_clears_it() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run(&["server", "edit", "eu-1", "--notes", ""]);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: invalid server 'eu-1':\n  --notes: must not be empty (omit it instead)\n  \
         to clear it: --unset notes\n"
    );
    assert_eq!(
        fs::read_to_string(sandbox.inventory_file()).expect("read"),
        SAMPLE
    );
}

#[test]
fn servers_inherit_the_defaults_field_by_field() {
    let sandbox = Sandbox::new();
    assert_exit(
        &sandbox.run(&["server", "defaults", "--user", "deploy", "--port", "2200"]),
        0,
    );
    assert_exit(
        &sandbox.run(&["server", "add", "a", "--host", "h", "--port", "22"]),
        0,
    );
    let out = sandbox.run(&["server", "show", "a", "--json"]);
    assert_exit(&out, 0);
    let doc = json_stdout(&out);
    assert_eq!(
        doc["effective"]["user"],
        serde_json::json!({"value": "deploy", "source": "defaults"})
    );
    assert_eq!(
        doc["effective"]["port"],
        serde_json::json!({"value": 22, "source": "server"})
    );
    assert_eq!(doc["effective"]["identity_file"], serde_json::Value::Null);
}

// ── --json ───────────────────────────────────────────────────────────────

#[test]
fn json_output_of_list_and_show() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);

    let out = sandbox.run(&["server", "list", "--json"]);
    assert_exit(&out, 0);
    let doc = json_stdout(&out);
    assert_eq!(doc["schema"], 1);
    assert_eq!(
        doc["servers"],
        serde_json::json!([
            {"id": "eu-1", "host": "203.0.113.10", "sites": ["shop", "blog"]},
            {"id": "bare", "host": "bare.example.com", "sites": []}
        ])
    );

    let out = sandbox.run(&["server", "show", "eu-1", "--json"]);
    assert_exit(&out, 0);
    let doc = json_stdout(&out);
    assert_eq!(doc["server"]["sites"], serde_json::json!(["shop", "blog"]));
    assert_eq!(
        doc["effective"]["connect_timeout_secs"],
        serde_json::json!({"value": 15, "source": "built-in"})
    );
    assert!(stderr(&out).is_empty(), "{}", stderr(&out));
}

#[test]
fn json_failures_print_a_document_and_exit_1() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run(&["server", "show", "nope", "--json"]);
    assert_exit(&out, 1);
    assert_eq!(
        json_stdout(&out),
        serde_json::json!({
            "schema": 1,
            "error": {
                "code": "invalid_input",
                "message": "server 'nope' does not exist",
                "problems": []
            }
        })
    );
    assert_eq!(
        stderr(&out),
        "Error: server 'nope' does not exist\n  'apvm server list' shows the servers\n"
    );

    let out = sandbox.run(&["server", "defaults", "--port", "0", "--json"]);
    assert_exit(&out, 1);
    let doc = json_stdout(&out);
    assert_eq!(doc["error"]["problems"][0]["location"], "--port");
}

// ── invalid input ────────────────────────────────────────────────────────

#[test]
fn invalid_input_exits_1_naming_the_flags_and_writes_nothing() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&[
        "server",
        "add",
        "eu-1",
        "--host",
        "h",
        "--port",
        "0",
        "--key",
        "~/my key",
        "--known-hosts",
        "/dev/null",
    ]);
    assert_exit(&out, 1);
    let err = stderr(&out);
    assert!(
        err.starts_with("Error: invalid server 'eu-1':\n  --port: "),
        "{err}"
    );
    assert!(err.contains("\n  --key: "), "{err}");
    assert!(err.contains("\n  --known-hosts: "), "{err}");
    assert!(stdout(&out).is_empty());
    assert!(!sandbox.inventory_file().exists(), "nothing was written");
}

#[test]
fn control_characters_are_refused_and_escaped_and_unicode_is_kept() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&[
        "server",
        "add",
        "a",
        "--host",
        "h",
        "--name",
        "x\u{1b}[2J\u{202e}y",
    ]);
    assert_exit(&out, 1);
    let err = stderr(&out);
    assert!(
        !err.contains('\u{1b}') && !err.contains('\u{202e}'),
        "{err:?}"
    );
    assert!(err.contains("--name: contains U+001B"), "{err}");

    let out = sandbox.run(&[
        "server",
        "add",
        "a",
        "--host",
        "h",
        "--name",
        "Servidor Ñandú 🚀",
    ]);
    assert_exit(&out, 0);
    let out = sandbox.run(&["server", "list"]);
    assert!(
        stdout(&out).contains("Servidor Ñandú 🚀"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn duplicate_and_unknown_ids_exit_1_with_a_remedy() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let before = fs::read(sandbox.inventory_file()).expect("read");

    let out = sandbox.run(&["server", "add", "eu-1", "--host", "x"]);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: server 'eu-1' already exists\n  \
         change it with 'apvm server edit', or choose another id\n"
    );
    for args in [
        &["server", "edit", "nope", "--user", "u"][..],
        &["server", "remove", "nope", "-y"],
    ] {
        let out = sandbox.run(args);
        assert_exit(&out, 1);
        assert!(
            stderr(&out).contains("server 'nope' does not exist"),
            "{}",
            stderr(&out)
        );
    }
    assert_eq!(fs::read(sandbox.inventory_file()).expect("read"), before);
}

#[test]
fn contradicting_and_malformed_arguments() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run(&["server", "edit", "eu-1", "--user", "u", "--unset", "user"]);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: --user and --unset user contradict each other: pass only one\n"
    );
    // Argument errors are clap's: exit 2, nothing written.
    for args in [
        &["server", "edit", "eu-1"][..],
        &["server", "edit", "eu-1", "--unset", "host"],
        &["server", "add", "x", "--host", "h", "--port", "70000"],
        &["server", "add", "x"],
        &[
            "server",
            "add",
            "x",
            "--host",
            "h",
            "--multiplex",
            "--no-multiplex",
        ],
        &["server", "defaults", "--connect-timeout", "soon"],
    ] {
        assert_exit(&sandbox.run(args), 2);
    }
    assert_eq!(
        fs::read_to_string(sandbox.inventory_file()).expect("read"),
        SAMPLE
    );
}

// ── remove and --cascade ─────────────────────────────────────────────────

#[test]
fn remove_refuses_a_server_with_sites_unless_it_cascades() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);

    let out = sandbox.run(&["server", "remove", "eu-1", "-y"]);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: server 'eu-1' is used by 2 site(s): 'shop', 'blog'\n  \
         pass --cascade to remove those sites with it\n"
    );
    assert_eq!(
        fs::read_to_string(sandbox.inventory_file()).expect("read"),
        SAMPLE
    );

    let out = sandbox.run(&["server", "remove", "eu-1", "--cascade", "--yes"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "Removed server 'eu-1' and its 2 sites: 'shop', 'blog'.\n\
         They were removed from 1 group: 'qa'.\n"
    );
    let doc = inventory_json(&sandbox);
    assert_eq!(
        doc["servers"],
        serde_json::json!([{"id": "bare", "host": "bare.example.com"}])
    );
    assert_eq!(doc["sites"], serde_json::json!([]));
    assert_eq!(
        doc["groups"],
        serde_json::json!([{"id": "qa", "sites": []}])
    );
}

#[test]
fn remove_without_a_terminal_needs_yes_even_with_a_piped_answer() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run_binary(
        Path::new(common::APVM),
        &["server", "remove", "bare"],
        "y\n",
    );
    assert_exit(&out, 1);
    assert!(
        stderr(&out).contains("stdin is not a terminal; pass -y (--yes)"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(sandbox.inventory_file()).expect("read"),
        SAMPLE
    );
}

// ── APVM_INVENTORY ───────────────────────────────────────────────────────

#[test]
fn apvm_inventory_points_every_command_at_another_file() {
    let sandbox = Sandbox::new();
    let other = sandbox.scratch().join("nested").join("inv.json");
    let env = [("APVM_INVENTORY", other.as_os_str())];

    let out = sandbox.run_with_env(&["server", "add", "a", "--host", "h"], &env);
    assert_exit(&out, 0);
    assert_eq!(mode(&other), 0o600);
    assert!(
        !sandbox.inventory_file().exists(),
        "the default stays untouched"
    );

    let out = sandbox.run_with_env(&["server", "list", "--json"], &env);
    assert_eq!(json_stdout(&out)["servers"][0]["id"], "a");
    let out = sandbox.run(&["server", "list", "--json"]);
    assert_eq!(json_stdout(&out)["servers"], serde_json::json!([]));
}

#[test]
fn a_relative_apvm_inventory_is_relative_to_the_working_directory() {
    let sandbox = Sandbox::new();
    let env = [("APVM_INVENTORY", OsStr::new("inv/inventory.json"))];
    assert_exit(
        &sandbox.run_with_env(&["server", "add", "a", "--host", "h"], &env),
        0,
    );
    assert!(sandbox.cwd().join("inv").join("inventory.json").is_file());
}

#[test]
fn an_empty_apvm_inventory_means_the_default() {
    let sandbox = Sandbox::new();
    let env = [("APVM_INVENTORY", OsStr::new(""))];
    assert_exit(
        &sandbox.run_with_env(&["server", "add", "a", "--host", "h"], &env),
        0,
    );
    assert!(sandbox.inventory_file().is_file());
}

// ── files that are never rewritten ───────────────────────────────────────

#[test]
fn an_invalid_inventory_lists_its_problems_and_is_never_rewritten() {
    let sandbox = Sandbox::new();
    let text =
        r#"{"version": 1, "servers": [{"id": "a", "host": "-h"}, {"id": "a", "host": "h"}]}"#;
    write_inventory(&sandbox, text);
    let out = sandbox.run(&["server", "add", "b", "--host", "h"]);
    assert_exit(&out, 1);
    let err = stderr(&out);
    let lines: Vec<&str> = err.lines().collect();
    assert!(
        lines[0].starts_with("Error: ") && lines[0].ends_with("inventory.json: invalid inventory:"),
        "{err}"
    );
    assert!(lines[1].starts_with("  servers[0].host: "), "{err}");
    assert!(
        lines[2].starts_with("  servers[1].id: duplicate id 'a'"),
        "{err}"
    );
    assert!(lines[3].starts_with("  fix the file by hand"), "{err}");
    assert_eq!(
        fs::read_to_string(sandbox.inventory_file()).expect("read"),
        text
    );

    let out = sandbox.run(&["server", "list", "--json"]);
    assert_exit(&out, 1);
    assert_eq!(json_stdout(&out)["error"]["code"], "inventory_invalid");
}

#[test]
fn a_newer_inventory_is_read_only() {
    let sandbox = Sandbox::new();
    let text = r#"{"version": 9, "servers": [{"id": "a", "host": "h"}]}"#;
    write_inventory(&sandbox, text);

    let out = sandbox.run(&["server", "list"]);
    assert_exit(&out, 0);
    assert!(stdout(&out).contains("a   h"), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("format version 9, newer than this apvm writes (1)"),
        "{}",
        stderr(&out)
    );
    for args in [
        &["server", "edit", "a", "--name", "N"][..],
        &["server", "add", "b", "--host", "h"],
        &["server", "defaults", "--user", "u"],
        &["server", "remove", "a", "-y"],
    ] {
        let out = sandbox.run(args);
        assert_exit(&out, 1);
        assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    }
    assert_eq!(
        fs::read_to_string(sandbox.inventory_file()).expect("read"),
        text
    );
}

#[test]
fn a_symlinked_inventory_is_refused() {
    let sandbox = Sandbox::new();
    let target = sandbox.scratch().join("real.json");
    fs::write(&target, SAMPLE).expect("write");
    let link = sandbox.scratch().join("link.json");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let env = [("APVM_INVENTORY", link.as_os_str())];
    for args in [
        &["server", "list"][..],
        &["server", "add", "x", "--host", "h"],
    ] {
        let out = sandbox.run_with_env(args, &env);
        assert_exit(&out, 1);
        assert!(
            stderr(&out).contains("is a symbolic link"),
            "{}",
            stderr(&out)
        );
    }
    assert_eq!(fs::read_to_string(&target).expect("read"), SAMPLE);
}

#[test]
fn an_inventory_path_under_a_file_is_an_io_error() {
    let sandbox = Sandbox::new();
    let blocker = sandbox.scratch().join("file");
    fs::write(&blocker, "x").expect("write");
    let path = blocker.join("inventory.json");
    let env = [("APVM_INVENTORY", path.as_os_str())];
    let out = sandbox.run_with_env(&["server", "add", "a", "--host", "h"], &env);
    assert_exit(&out, 1);
    assert!(stderr(&out).starts_with("Error: "), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&blocker).expect("read"), "x");
}

#[test]
fn a_locked_inventory_fails_after_the_wait_and_writes_nothing() {
    // Another process (here: this test) holds the inventory's lock for the
    // whole 10 s wait: the edit gives up with `inventory_locked`.
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let lock_path = sandbox.home().join(".apvm").join("inventory.json.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .expect("open lock");
    lock.lock().expect("hold the lock");

    let out = sandbox.run(&["server", "add", "x", "--host", "h"]);
    assert_exit(&out, 1);
    assert!(
        stderr(&out).contains("cannot lock the inventory")
            && stderr(&out).contains("held it for 10 s\n  edits hold the lock for milliseconds"),
        "{}",
        stderr(&out)
    );
    // Reads take no lock.
    assert_exit(&sandbox.run(&["server", "list"]), 0);
    lock.unlock().expect("release");
    assert_eq!(
        fs::read_to_string(sandbox.inventory_file()).expect("read"),
        SAMPLE
    );
}

// ── concurrency ──────────────────────────────────────────────────────────

#[test]
fn concurrent_adds_from_many_processes_all_land() {
    // Each command is one locked read-modify-write, so no edit is lost.
    let sandbox = Sandbox::new();
    let ids: Vec<String> = (0..8).map(|n| format!("s{n}")).collect();
    let children: Vec<_> = ids
        .iter()
        .map(|id| {
            common::spawn_retrying_busy(|| {
                sandbox
                    .command_for(Path::new(common::APVM))
                    .args(["server", "add", id, "--host", "h"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            })
            .expect("spawn")
        })
        .collect();
    for child in children {
        assert_exit(&child.wait_with_output().expect("finish"), 0);
    }
    let doc = inventory_json(&sandbox);
    let mut stored: Vec<&str> = doc["servers"]
        .as_array()
        .expect("servers")
        .iter()
        .filter_map(|server| server["id"].as_str())
        .collect();
    stored.sort_unstable();
    assert_eq!(stored, ids);
}

// ── apvm site ────────────────────────────────────────────────────────────

/// The current default inventory's text.
fn inventory_text(sandbox: &Sandbox) -> String {
    fs::read_to_string(sandbox.inventory_file()).expect("read inventory")
}

#[test]
fn site_add_edit_show_list_remove_round_trip() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let file = sandbox.inventory_file();

    let out = sandbox.run(&[
        "site",
        "add",
        "legacy",
        "--server",
        "bare",
        "--path",
        "/srv/legacy",
        "--wp-cli",
        "/usr/bin/php8.3 -n /opt/wp-cli.phar",
        "--run-as",
        "login",
        "--owner",
        "ftpuser:www-data",
        "--ownership",
        "check",
        "--group",
        "qa",
    ]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "Added site 'legacy'.\nIt joined 1 group: 'qa'.\n"
    );
    assert!(stderr(&out).is_empty(), "{}", stderr(&out));
    assert_eq!(mode(&file), 0o600, "a rewrite stays private");
    let doc = inventory_json(&sandbox);
    assert_eq!(
        doc["sites"][2],
        serde_json::json!({
            "id": "legacy",
            "server": "bare",
            "path": "/srv/legacy",
            "wp_cli": ["/usr/bin/php8.3", "-n", "/opt/wp-cli.phar"],
            "run_as": "login",
            "owner": "ftpuser:www-data",
            "ownership": "check"
        })
    );
    assert_eq!(
        doc["groups"],
        serde_json::json!([{"id": "qa", "sites": ["shop", "legacy"]}])
    );

    let out = sandbox.run(&[
        "site",
        "edit",
        "legacy",
        "--timeout",
        "600",
        "--unset",
        "owner,wp-cli",
    ]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "Updated site 'legacy': wp-cli, owner, timeout.\n"
    );

    let out = sandbox.run(&["site", "list"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "ID      SERVER  PATH           URL  NAME\n\
         shop    eu-1    /var/www/shop\n\
         blog    eu-1    /var/www/blog\n\
         legacy  bare    /srv/legacy\n"
    );

    let out = sandbox.run(&["site", "show", "legacy"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(
        text.starts_with("Site 'legacy'\n  server  bare (bare.example.com)\n"),
        "{text}"
    );
    assert!(text.contains("\n  groups  qa\n"), "{text}");
    // VALUE is as wide as the longest value, the plugins directory.
    let width = "/srv/legacy/wp-content/plugins".len();
    for (setting, value, from) in [
        ("wp-cli", "wp", "built-in"),
        ("run as", "login", "site"),
        ("ownership", "check", "site"),
        ("timeout", "600 s", "site"),
    ] {
        let line = format!("\n  {setting:<11}  {value:<width$}  {from}\n");
        assert!(text.contains(&line), "{line:?} in:\n{text}");
    }

    let out = sandbox.run(&["site", "remove", "legacy", "-y"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "Removed site 'legacy'.\nIt was removed from 1 group: 'qa'.\n"
    );
    assert_eq!(
        inventory_json(&sandbox),
        serde_json::from_str::<serde_json::Value>(SAMPLE).expect("JSON")
    );
}

#[test]
fn site_reads_of_a_missing_inventory_create_nothing() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["site", "list"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "No sites yet. Add one with: apvm site add <ID> --server <SERVER> --path <WP_ROOT>\n"
    );
    assert_exit(&sandbox.run(&["site", "list", "--json"]), 0);
    assert_exit(&sandbox.run(&["site", "show", "shop"]), 1);
    assert_exit(&sandbox.run(&["site", "list", "--server", "eu-1"]), 1);
    assert!(
        !sandbox.home().join(".apvm").exists(),
        "nothing was created"
    );
}

#[test]
fn bare_site_command_prints_usage_with_exit_2() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["site"]);
    assert_exit(&out, 2);
    for sub in ["add", "edit", "list", "show", "remove"] {
        assert!(
            stderr(&out).contains(sub),
            "usage lists `{sub}`:\n{}",
            stderr(&out)
        );
    }
}

#[test]
fn site_list_filters_and_json() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run(&[
        "site", "list", "--server", "eu-1", "--group", "qa", "--json",
    ]);
    assert_exit(&out, 0);
    assert_eq!(
        json_stdout(&out),
        serde_json::json!({
            "schema": 1,
            "sites": [{"id": "shop", "server": "eu-1", "path": "/var/www/shop", "groups": ["qa"]}]
        })
    );
    let out = sandbox.run(&["site", "list", "--server", "bare"]);
    assert_exit(&out, 0);
    assert_eq!(stdout(&out), "No site matches the filters.\n");

    let out = sandbox.run(&["site", "list", "--group", "nope", "--json"]);
    assert_exit(&out, 1);
    assert_eq!(
        json_stdout(&out),
        serde_json::json!({
            "schema": 1,
            "error": {"code": "invalid_input", "message": "group 'nope' does not exist", "problems": []}
        })
    );
    assert_eq!(
        stderr(&out),
        format!(
            "Error: group 'nope' does not exist\n  apvm has no group commands yet: add the group \
             to the 'groups' list of the inventory file by hand, as {{\"id\": \"<ID>\", \
             \"sites\": []}}, or from Node with SiteInventory\n  the inventory file is {:?}\n",
            sandbox.inventory_file()
        )
    );
}

#[test]
fn site_show_json_holds_the_effective_settings() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run(&["site", "show", "shop", "--json"]);
    assert_exit(&out, 0);
    let doc = json_stdout(&out);
    assert_eq!(
        doc["site"],
        serde_json::json!({"id": "shop", "server": "eu-1", "path": "/var/www/shop", "groups": ["qa"]})
    );
    assert_eq!(
        doc["effective"]["plugins_dir"],
        serde_json::json!({"value": "/var/www/shop/wp-content/plugins", "source": "built-in"})
    );
    assert_eq!(doc["effective"]["server"]["host"], "203.0.113.10");
    assert_eq!(
        doc["effective"]["server"]["ssh"]["multiplex"],
        serde_json::json!({"value": true, "source": "built-in"})
    );
}

#[test]
fn site_invalid_input_exits_1_naming_the_flags_and_writes_nothing() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run(&[
        "site", "add", "x", "--server", "eu-9", "--path", "/srv/x/", "--wp-cli", "sudo wp",
        "--run-as", "root",
    ]);
    assert_exit(&out, 1);
    let err = stderr(&out);
    assert!(err.starts_with("Error: invalid site 'x':\n"), "{err}");
    for line in [
        "\n  --server: unknown server 'eu-9'",
        "\n  --path: ",
        "\n  --wp-cli[0]: must be a PHP interpreter",
        "\n  --wp-cli[1]: must be an absolute path",
        "\n  --run-as: 'root' needs allow_root: true",
    ] {
        assert!(err.contains(line), "{line:?} in:\n{err}");
    }
    assert!(
        err.ends_with(
            "\n  root needs --allow-root: pass it, or name another user in --run-as/--owner\n"
        ),
        "{err}"
    );
    assert!(stdout(&out).is_empty());

    let out = sandbox.run(&["site", "edit", "shop", "--url", ""]);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: invalid site 'shop':\n  --url: must start with 'http://' or 'https://'\n  \
         to clear it: --unset url\n"
    );
    assert_eq!(inventory_text(&sandbox), SAMPLE);
}

#[test]
fn site_ids_and_groups_exit_1_with_a_remedy() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let group_error = format!(
        "Error: group 'nope' does not exist\n  apvm has no group commands yet: add the group to \
         the 'groups' list of the inventory file by hand, as {{\"id\": \"<ID>\", \"sites\": []}}, \
         or from Node with SiteInventory\n  the inventory file is {:?}\n",
        sandbox.inventory_file()
    );
    for (args, message) in [
        (
            &["site", "add", "shop", "--server", "eu-1", "--path", "/x"][..],
            "Error: site 'shop' already exists\n  \
             change it with 'apvm site edit', or choose another id\n",
        ),
        (
            &["site", "edit", "nope", "--notes", "n"],
            "Error: site 'nope' does not exist\n  'apvm site list' shows the sites\n",
        ),
        (
            &[
                "site", "add", "x", "--server", "eu-1", "--path", "/x", "--group", "nope",
            ],
            group_error.as_str(),
        ),
        (
            &["site", "remove", "nope", "-y"],
            "Error: site 'nope' does not exist\n  'apvm site list' shows the sites\n",
        ),
    ] {
        let out = sandbox.run(args);
        assert_exit(&out, 1);
        assert_eq!(stderr(&out), message, "{args:?}");
    }
    assert_eq!(inventory_text(&sandbox), SAMPLE);
}

#[test]
fn site_contradicting_and_malformed_arguments() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run(&[
        "site",
        "edit",
        "shop",
        "--allow-root",
        "--unset",
        "allow-root",
    ]);
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: --allow-root and --unset allow-root contradict each other: pass only one\n"
    );
    for args in [
        &["site", "edit", "shop"][..],
        &["site", "edit", "shop", "--unset", "path"],
        &["site", "edit", "shop", "--unset", "server"],
        &["site", "add", "x", "--server", "eu-1"],
        &["site", "add", "x", "--path", "/x"],
        &["site", "edit", "shop", "--ownership", "always"],
        &["site", "edit", "shop", "--timeout", "soon"],
        &[
            "site", "add", "x", "--server", "eu-1", "--path", "/x", "--json",
        ],
    ] {
        assert_exit(&sandbox.run(args), 2);
    }
    assert_eq!(inventory_text(&sandbox), SAMPLE);
}

#[test]
fn site_remove_without_a_terminal_needs_yes_even_with_a_piped_answer() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let out = sandbox.run_binary(Path::new(common::APVM), &["site", "remove", "blog"], "y\n");
    assert_exit(&out, 1);
    assert_eq!(
        stderr(&out),
        "Error: not removing site 'blog': it needs a confirmation, and stdin is not a terminal; \
         pass -y (--yes) to remove it without asking\n"
    );
    assert_eq!(inventory_text(&sandbox), SAMPLE);
}

#[test]
fn site_commands_on_a_newer_inventory_are_read_only() {
    let sandbox = Sandbox::new();
    let text = r#"{"version": 9, "servers": [{"id": "a", "host": "h"}],
        "sites": [{"id": "s", "server": "a", "path": "/var/www/s"}]}"#;
    write_inventory(&sandbox, text);
    let out = sandbox.run(&["site", "list"]);
    assert_exit(&out, 0);
    assert!(stdout(&out).contains("s   a"), "{}", stdout(&out));
    assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    for args in [
        &["site", "add", "t", "--server", "a", "--path", "/q"][..],
        &["site", "edit", "s", "--notes", "n"],
        &["site", "remove", "s", "-y"],
    ] {
        let out = sandbox.run(args);
        assert_exit(&out, 1);
        assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    }
    assert_eq!(inventory_text(&sandbox), text);
}

#[test]
fn apvm_inventory_points_site_commands_at_another_file() {
    let sandbox = Sandbox::new();
    let other = sandbox.scratch().join("inv.json");
    fs::write(&other, SAMPLE).expect("write");
    let env = [("APVM_INVENTORY", other.as_os_str())];
    let out = sandbox.run_with_env(
        &["site", "add", "x", "--server", "bare", "--path", "/srv/x"],
        &env,
    );
    assert_exit(&out, 0);
    assert_eq!(mode(&other), 0o600);
    assert!(
        !sandbox.inventory_file().exists(),
        "the default stays untouched"
    );
    let out = sandbox.run_with_env(&["site", "show", "x", "--json"], &env);
    assert_eq!(json_stdout(&out)["site"]["server"], "bare");
}

#[test]
fn concurrent_site_adds_from_many_processes_all_land() {
    let sandbox = Sandbox::new();
    write_inventory(&sandbox, SAMPLE);
    let ids: Vec<String> = (0..8).map(|n| format!("s{n}")).collect();
    let children: Vec<_> = ids
        .iter()
        .map(|id| {
            let path = format!("/var/www/{id}");
            common::spawn_retrying_busy(|| {
                sandbox
                    .command_for(Path::new(common::APVM))
                    .args([
                        "site", "add", id, "--server", "bare", "--path", &path, "--group", "qa",
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
            })
            .expect("spawn")
        })
        .collect();
    for child in children {
        assert_exit(&child.wait_with_output().expect("finish"), 0);
    }
    let doc = inventory_json(&sandbox);
    let mut stored: Vec<&str> = doc["sites"]
        .as_array()
        .expect("sites")
        .iter()
        .filter_map(|site| site["id"].as_str())
        .filter(|id| id.starts_with('s') && *id != "shop")
        .collect();
    stored.sort_unstable();
    assert_eq!(stored, ids);
    let members = doc["groups"][0]["sites"].as_array().map(Vec::len);
    assert_eq!(members, Some(1 + ids.len()), "every group join landed too");
}
