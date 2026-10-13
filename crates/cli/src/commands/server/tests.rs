//! Unit tests of `apvm server`: argument parsing, the pure helpers, and
//! whole commands run against temporary inventory files through a
//! recording [`Console`].

use std::path::PathBuf;

use apvm_sites::{Group, Inventory, Site, SshSettings};
use clap::{ArgAction, CommandFactory, Parser};
use tempfile::TempDir;

use super::*;
use crate::commands::inventory::Recorder;

/// A temporary directory holding the inventory under test.
struct Fixture {
    /// Keeps the directory alive.
    _dir: TempDir,
    /// `<dir>/inventory.json` (missing until written).
    path: PathBuf,
}

impl Fixture {
    /// A fixture without an inventory file.
    fn empty() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("inventory.json");
        Self { _dir: dir, path }
    }

    /// A fixture whose file holds `inventory`.
    fn with(inventory: &Inventory) -> Self {
        let fixture = Self::empty();
        io::save(&fixture.path, inventory).expect("save");
        fixture
    }

    /// A fixture whose file holds `text` verbatim.
    fn with_text(text: &str) -> Self {
        let fixture = Self::empty();
        std::fs::write(&fixture.path, text).expect("write");
        fixture
    }

    /// The inventory in the file.
    fn load(&self) -> Inventory {
        io::load(&self.path).expect("load")
    }

    /// The file's bytes (empty when it is missing).
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap_or_default()
    }

    /// Run `apvm server <args>`, answering any question with `answer`.
    fn run(&self, args: &[&str], answer: Option<bool>) -> (Result<(), Failure>, Recorder) {
        let command = parse(args);
        let mut recorder = Recorder::new();
        let result = recorder.run(answer, |console| command.run(&self.path, console));
        (result, recorder)
    }

    /// [`Fixture::run`], expecting success.
    #[track_caller]
    fn ok(&self, args: &[&str]) -> Recorder {
        let (result, recorder) = self.run(args, None);
        if let Err(failure) = result {
            panic!("`server {}` failed: {failure}", args.join(" "));
        }
        recorder
    }

    /// [`Fixture::run`], expecting a failure; the file must be unchanged.
    #[track_caller]
    fn fails(&self, args: &[&str]) -> (Failure, Recorder) {
        let before = self.bytes();
        let (result, recorder) = self.run(args, None);
        let Err(failure) = result else {
            panic!("`server {}` must fail", args.join(" "));
        };
        assert_eq!(self.bytes(), before, "a failed command wrote the file");
        (failure, recorder)
    }
}

/// Parse `apvm server <args>`.
#[track_caller]
fn parse(args: &[&str]) -> ServerArgs {
    let argv = ["apvm", "server"].iter().chain(args).copied();
    match crate::Cli::try_parse_from(argv).map(|cli| cli.command) {
        Ok(crate::Commands::Server(command)) => command,
        Ok(other) => panic!("parsed as {other:?}"),
        Err(error) => panic!("`server {}` does not parse: {error}", args.join(" ")),
    }
}

/// The clap error kind of `apvm server <args>`, when it does not parse.
fn parse_error(args: &[&str]) -> Option<clap::error::ErrorKind> {
    let argv = ["apvm", "server"].iter().chain(args).copied();
    crate::Cli::try_parse_from(argv)
        .err()
        .map(|error| error.kind())
}

/// `eu-1` (two sites, one in group `qa`) and `bare` (no sites).
fn sample() -> Inventory {
    let mut inventory = Inventory::new();
    inventory
        .add_server(Server::new("eu-1", "203.0.113.10"))
        .expect("valid server");
    inventory
        .add_server(Server::new("bare", "bare.example.com"))
        .expect("valid server");
    for id in ["shop", "blog"] {
        inventory
            .add_site(Site::new(id, "eu-1", format!("/var/www/{id}")))
            .expect("valid site");
    }
    inventory
        .add_group(Group::new("qa", vec!["shop".into()]))
        .expect("valid group");
    inventory
}

/// The problem locations of a refused input.
fn locations(failure: &Failure) -> Vec<String> {
    match failure {
        Failure::Sites(error) | Failure::Remedied { error, .. } => error
            .problems()
            .iter()
            .map(|problem| problem.location.clone())
            .collect(),
        other => panic!("not a library error: {other:?}"),
    }
}

/// A newer, read-only inventory with server `a`.
const NEWER: &str = r#"{"version": 9, "servers": [{"id": "a", "host": "h"}]}"#;

// ── parsing ──────────────────────────────────────────────────────────────

#[test]
fn edit_needs_at_least_one_change() {
    assert_eq!(
        parse_error(&["edit", "eu-1"]),
        Some(clap::error::ErrorKind::MissingRequiredArgument)
    );
    for change in [
        &["--host", "h"][..],
        &["--no-multiplex"],
        &["--unset", "notes"],
        &["--name", "N"],
    ] {
        let mut args = vec!["edit", "eu-1"];
        args.extend(change);
        assert_eq!(parse_error(&args), None, "{args:?}");
    }
}

#[test]
fn multiplex_flags_conflict_and_map_to_a_setting() {
    assert_eq!(
        parse_error(&["add", "x", "--host", "h", "--multiplex", "--no-multiplex"]),
        Some(clap::error::ErrorKind::ArgumentConflict)
    );
    let flags = |multiplex, no_multiplex| MultiplexFlags {
        multiplex,
        no_multiplex,
    };
    assert_eq!(flags(true, false).setting(), Some(Setting::Multiplex(true)));
    assert_eq!(
        flags(false, true).setting(),
        Some(Setting::Multiplex(false))
    );
    assert_eq!(flags(false, false).setting(), None);
}

#[test]
fn unset_accepts_repeats_and_comma_lists_of_known_fields() {
    let action = parse(&["edit", "a", "--unset", "user,port", "--unset", "notes"]).action;
    let ServerAction::Edit(args) = action else {
        panic!("an edit");
    };
    assert_eq!(args.unset, [Field::User, Field::Port, Field::Notes]);
    assert_eq!(
        parse_error(&["edit", "a", "--unset", "host"]),
        Some(clap::error::ErrorKind::InvalidValue),
        "the host is required, so it cannot be unset"
    );
    for (command, foreign) in [
        ("edit", "ssh-binary"),
        ("defaults", "name"),
        ("defaults", "host"),
    ] {
        let mut args = vec![command];
        if command == "edit" {
            args.push("a");
        }
        args.extend(["--unset", foreign]);
        assert_eq!(
            parse_error(&args),
            Some(clap::error::ErrorKind::InvalidValue),
            "{args:?}: not a field of that record"
        );
    }
}

#[test]
fn numbers_out_of_their_type_are_argument_errors() {
    assert_eq!(
        parse_error(&["add", "x", "--host", "h", "--port", "70000"]),
        Some(clap::error::ErrorKind::ValueValidation)
    );
    assert_eq!(
        parse_error(&["defaults", "--connect-timeout", "soon"]),
        Some(clap::error::ErrorKind::ValueValidation)
    );
}

#[test]
fn only_reads_and_defaults_take_json() {
    assert!(parse(&["list", "--json"]).json());
    assert!(parse(&["show", "a", "--json"]).json());
    assert!(parse(&["defaults", "--json"]).json());
    assert!(!parse(&["list"]).json());
    for args in [
        &["add", "a", "--host", "h", "--json"][..],
        &["edit", "a", "--name", "n", "--json"],
        &["remove", "a", "--json"],
    ] {
        assert_eq!(
            parse_error(args),
            Some(clap::error::ErrorKind::UnknownArgument),
            "{args:?}"
        );
    }
}

// ── pure helpers ─────────────────────────────────────────────────────────

#[test]
fn every_flag_of_add_lands_in_the_server_record() {
    let action = parse(&[
        "add",
        "eu-1",
        "--host",
        "h",
        "--user",
        "u",
        "--port",
        "2222",
        "--key",
        "/k",
        "--connect-timeout",
        "9",
        "--jump",
        "j",
        "--known-hosts",
        "/kh",
        "--ssh-config",
        "/c",
        "--no-multiplex",
        "--name",
        "N",
        "--notes",
        "n",
    ])
    .action;
    let ServerAction::Add(args) = action else {
        panic!("an add");
    };
    let server = args.to_server();
    assert_eq!((server.id.as_str(), server.host.as_str()), ("eu-1", "h"));
    assert_eq!(server.name.as_deref(), Some("N"));
    assert_eq!(server.notes.as_deref(), Some("n"));
    let expected = SshSettings {
        user: Some("u".into()),
        port: Some(2222),
        identity_file: Some("/k".into()),
        connect_timeout_secs: Some(9),
        known_hosts_file: Some("/kh".into()),
        ssh_config_file: Some("/c".into()),
        proxy_jump: Some("j".into()),
        multiplex: Some(false),
        ..SshSettings::default()
    };
    assert_eq!(server.ssh, expected);
}

#[test]
fn every_flag_of_defaults_lands_in_the_defaults_record() {
    let action = parse(&[
        "defaults",
        "--user",
        "u",
        "--port",
        "22",
        "--key",
        "/k",
        "--connect-timeout",
        "30",
        "--jump",
        "j",
        "--known-hosts",
        "/kh",
        "--ssh-config",
        "/c",
        "--ssh-binary",
        "/usr/bin/ssh",
        "--multiplex",
    ])
    .action;
    let ServerAction::Defaults(args) = action else {
        panic!("defaults");
    };
    let defaults = fields::apply(&Defaults::default(), &args.settings(), &args.unset);
    let expected = SshSettings {
        user: Some("u".into()),
        port: Some(22),
        identity_file: Some("/k".into()),
        connect_timeout_secs: Some(30),
        known_hosts_file: Some("/kh".into()),
        ssh_config_file: Some("/c".into()),
        proxy_jump: Some("j".into()),
        binary: Some("/usr/bin/ssh".into()),
        multiplex: Some(true),
    };
    assert_eq!(defaults.ssh, expected);
}

/// SSH settings the library refuses field by field.
fn refused_ssh() -> SshSettings {
    SshSettings {
        user: Some("-u".into()),
        port: Some(0),
        identity_file: Some("a b".into()),
        connect_timeout_secs: Some(0),
        known_hosts_file: Some("none".into()),
        ssh_config_file: Some("a b".into()),
        proxy_jump: Some("none".into()),
        multiplex: None,
        binary: Some("a b".into()),
    }
}

#[test]
fn the_flag_maps_name_every_location_a_refused_record_can_have() {
    // Every field set to a refused value: each problem must name a flag.
    let mut server = Server::new("Bad Id", "-h");
    server.name = Some(String::new());
    server.notes = Some(String::new());
    server.ssh = refused_ssh();
    server.ssh.binary = None;
    let error = Inventory::new().add_server(server).expect_err("refused");
    let mut named = locations(&name_flags(error, &fields::flag_map(Scope::Server)));
    named.sort();
    assert_eq!(
        named,
        [
            "--connect-timeout",
            "--host",
            "--jump",
            "--key",
            "--known-hosts",
            "--name",
            "--notes",
            "--port",
            "--ssh-config",
            "--user",
            "<ID>",
        ]
    );

    let defaults = Defaults { ssh: refused_ssh() };
    let error = Inventory::new()
        .set_defaults(defaults)
        .expect_err("refused");
    let mut named = locations(&name_flags(error, &fields::flag_map(Scope::Defaults)));
    named.sort();
    assert_eq!(
        named,
        [
            "--connect-timeout",
            "--jump",
            "--key",
            "--known-hosts",
            "--port",
            "--ssh-binary",
            "--ssh-config",
            "--user"
        ]
    );
}

#[test]
fn change_lines_name_the_changed_settings() {
    assert_eq!(
        change_line("Updated x", "x is unchanged.", &[]),
        "x is unchanged.\n"
    );
    assert_eq!(
        change_line("Updated x", "x is unchanged.", &[Field::User, Field::Port]),
        "Updated x: user, port.\n"
    );
}

#[test]
fn warn_relative_names_each_new_relative_path_once() {
    let before = Server::new("a", "h");
    let mut after = before.clone();
    after.ssh.identity_file = Some("keys/id".into());
    after.ssh.known_hosts_file = Some("/abs/known_hosts".into());
    let mut recorder = Recorder::new();
    recorder.run(None, |console| {
        warn_relative(Some(&before), &after, console)
    });
    assert_eq!(
        recorder.err.matches("relative path").count(),
        1,
        "{}",
        recorder.err
    );
    assert!(recorder.err.contains("--key 'keys/id'"), "{}", recorder.err);
    let mut recorder = Recorder::new();
    recorder.run(None, |console| warn_relative(Some(&after), &after, console));
    assert_eq!(
        recorder.err, "",
        "an unchanged path is not warned about again"
    );
}

#[test]
fn removal_messages_name_the_sites_and_groups() {
    let mut inventory = sample();
    let bare = inventory
        .clone()
        .remove_server("bare", false)
        .expect("removable");
    assert_eq!(removal_question(&bare), "Remove server 'bare'?");
    assert_eq!(removed_text(&bare), "Removed server 'bare'.\n");

    let cascade = inventory.remove_server("eu-1", true).expect("removable");
    assert_eq!(
        removal_question(&cascade),
        "Remove server 'eu-1' and its 2 sites: 'shop', 'blog' (they also leave 1 group: 'qa')?"
    );
    assert_eq!(
        removed_text(&cascade),
        "Removed server 'eu-1' and its 2 sites: 'shop', 'blog'.\n\
         They were removed from 1 group: 'qa'.\n"
    );
}

// ── add ──────────────────────────────────────────────────────────────────

#[test]
fn add_creates_the_file_with_the_server() {
    let fixture = Fixture::empty();
    let recorder = fixture.ok(&["add", "eu-1", "--host", "203.0.113.10", "--user", "ubuntu"]);
    assert_eq!(recorder.out, "Added server 'eu-1'.\n");
    assert_eq!(recorder.err, "");
    let server = fixture.load().server("eu-1").cloned().expect("added");
    assert_eq!(server.host, "203.0.113.10");
    assert_eq!(server.ssh.user.as_deref(), Some("ubuntu"));
}

#[test]
fn add_refuses_a_taken_id_and_writes_nothing() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["add", "eu-1", "--host", "other"]);
    assert_eq!(failure.code(), "invalid_input");
    assert!(
        failure.to_string().contains("server 'eu-1' already exists"),
        "{failure}"
    );
}

#[test]
fn add_refuses_invalid_input_naming_the_flags_and_creates_nothing() {
    let fixture = Fixture::empty();
    let (failure, _) = fixture.fails(&[
        "add",
        "EU",
        "--host",
        "h",
        "--port",
        "0",
        "--key",
        "a b",
        "--name",
        "a\u{1b}[2Jb",
    ]);
    assert_eq!(failure.code(), "invalid_input");
    assert_eq!(locations(&failure), ["<ID>", "--name", "--port", "--key"]);
    let text = failure.to_string();
    assert!(
        !text.contains('\u{1b}'),
        "control characters are escaped: {text:?}"
    );
    assert!(!fixture.path.exists(), "nothing was created");
}

#[test]
fn add_warns_about_a_relative_key_after_saving() {
    let fixture = Fixture::empty();
    let recorder = fixture.ok(&["add", "a", "--host", "h", "--key", "keys/id"]);
    assert!(
        recorder.err.contains("--key 'keys/id' is a relative path"),
        "{}",
        recorder.err
    );
    assert!(fixture.load().server("a").is_some());
}

// ── edit ─────────────────────────────────────────────────────────────────

#[test]
fn edit_changes_sets_and_unsets_and_reports_what_changed() {
    let fixture = Fixture::with(&sample());
    fixture.ok(&[
        "edit", "eu-1", "--user", "u", "--port", "2222", "--name", "EU",
    ]);
    let recorder = fixture.ok(&[
        "edit",
        "eu-1",
        "--host",
        "new.example.com",
        "--unset",
        "port,name",
        "--multiplex",
    ]);
    assert_eq!(
        recorder.out,
        "Updated server 'eu-1': host, port, multiplex, name.\n"
    );
    let server = fixture.load().server("eu-1").cloned().expect("kept");
    assert_eq!(server.host, "new.example.com");
    assert_eq!(
        server.ssh.user.as_deref(),
        Some("u"),
        "untouched settings stay"
    );
    assert_eq!(server.ssh.port, None);
    assert_eq!(server.ssh.multiplex, Some(true));
    assert_eq!(server.name, None);
}

#[test]
fn edit_without_a_real_change_writes_nothing() {
    let fixture = Fixture::with(&sample());
    let before = fixture.bytes();
    let recorder = fixture.ok(&["edit", "eu-1", "--host", "203.0.113.10", "--unset", "user"]);
    assert_eq!(recorder.out, "Server 'eu-1' is unchanged.\n");
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn edit_refuses_an_unknown_server() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "nope", "--user", "u"]);
    assert_eq!(
        failure.to_string(),
        "server 'nope' does not exist\n  'apvm server list' shows the servers"
    );
    // A missing file has no servers either, and stays missing.
    let empty = Fixture::empty();
    empty.fails(&["edit", "nope", "--user", "u"]);
    assert!(!empty.path.exists());
}

#[test]
fn edit_refuses_a_setting_both_set_and_unset() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "eu-1", "--user", "u", "--unset", "user"]);
    assert_eq!(
        failure.to_string(),
        "--user and --unset user contradict each other: pass only one"
    );
    assert_eq!(failure.code(), "invalid_input");
    let (failure, _) = fixture.fails(&["edit", "eu-1", "--no-multiplex", "--unset", "multiplex"]);
    assert!(
        failure
            .to_string()
            .starts_with("--no-multiplex and --unset multiplex"),
        "{failure}"
    );
}

#[test]
fn edit_refuses_invalid_input_naming_the_flags() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "eu-1", "--jump", "none", "--known-hosts", "NUL"]);
    assert_eq!(locations(&failure), ["--known-hosts", "--jump"]);
}

// ── defaults ─────────────────────────────────────────────────────────────

#[test]
fn defaults_without_change_flags_show_and_create_nothing() {
    let fixture = Fixture::empty();
    let recorder = fixture.ok(&["defaults"]);
    assert!(recorder.out.starts_with("SSH defaults"), "{}", recorder.out);
    let recorder = fixture.ok(&["defaults", "--json"]);
    assert_eq!(recorder.out, "{\n  \"schema\": 1,\n  \"defaults\": {}\n}\n");
    assert!(!fixture.path.exists());
}

#[test]
fn defaults_set_unset_and_report() {
    let fixture = Fixture::empty();
    let recorder = fixture.ok(&[
        "defaults",
        "--user",
        "deploy",
        "--key",
        "~/.ssh/id",
        "--no-multiplex",
    ]);
    assert_eq!(
        recorder.out,
        "Updated the SSH defaults: user, key, multiplex.\n"
    );
    let recorder = fixture.ok(&["defaults", "--unset", "key", "--connect-timeout", "30"]);
    assert_eq!(
        recorder.out,
        "Updated the SSH defaults: key, connect-timeout.\n"
    );
    let ssh = fixture.load().defaults().ssh.clone();
    assert_eq!(ssh.user.as_deref(), Some("deploy"));
    assert_eq!(ssh.identity_file, None);
    assert_eq!(ssh.connect_timeout_secs, Some(30));
    assert_eq!(ssh.multiplex, Some(false));
    let recorder = fixture.ok(&["defaults", "--user", "deploy"]);
    assert_eq!(recorder.out, "The SSH defaults are unchanged.\n");
}

#[test]
fn defaults_with_json_print_the_result() {
    let fixture = Fixture::empty();
    let recorder = fixture.ok(&["defaults", "--port", "2200", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON");
    assert_eq!(
        doc,
        serde_json::json!({"schema": 1, "defaults": {"ssh": {"port": 2200}}})
    );
}

#[test]
fn defaults_refuse_invalid_input_naming_the_flags() {
    let fixture = Fixture::with(&sample());
    let (failure, recorder) =
        fixture.fails(&["defaults", "--connect-timeout", "0", "--ssh-binary", "a b"]);
    assert_eq!(locations(&failure), ["--connect-timeout", "--ssh-binary"]);
    assert_eq!(recorder.out, "", "no --json, no JSON");
    let (failure, recorder) = fixture.fails(&["defaults", "--port", "0", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(doc["error"]["code"], "invalid_input");
    assert_eq!(doc["error"]["problems"][0]["location"], "--port");
    assert_eq!(failure.code(), "invalid_input");
    let (failure, _) = fixture.fails(&["defaults", "--port", "22", "--unset", "port"]);
    assert_eq!(failure.code(), "invalid_input");
}

// ── list / show ──────────────────────────────────────────────────────────

#[test]
fn list_and_show_read_without_creating_the_file() {
    let fixture = Fixture::empty();
    assert!(fixture.ok(&["list"]).out.starts_with("No servers yet."));
    let (failure, recorder) = fixture.fails(&["show", "eu-1", "--json"]);
    assert_eq!(failure.code(), "invalid_input");
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(doc["error"]["message"], "server 'eu-1' does not exist");
    assert!(!fixture.path.exists());
}

#[test]
fn list_and_show_render_the_inventory() {
    let fixture = Fixture::with(&sample());
    let list = fixture.ok(&["list"]).out;
    assert!(list.contains("eu-1  203.0.113.10      2"), "{list}");
    let show = fixture.ok(&["show", "eu-1"]).out;
    assert!(show.contains("  sites  shop, blog\n"), "{show}");
    let doc: serde_json::Value =
        serde_json::from_str(&fixture.ok(&["show", "bare", "--json"]).out).expect("JSON");
    assert_eq!(doc["server"]["host"], "bare.example.com");
}

#[test]
fn an_invalid_file_fails_every_command_and_is_never_rewritten() {
    let fixture = Fixture::with_text(r#"{"version": 1, "servers": [{"id": "a", "host": "-h"}]}"#);
    for args in [
        &["list"][..],
        &["show", "a"],
        &["defaults"],
        &["defaults", "--user", "u"],
        &["add", "b", "--host", "h"],
        &["edit", "a", "--host", "h"],
        &["remove", "a", "-y"],
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "inventory_invalid", "{args:?}");
    }
    let (_, recorder) = fixture.fails(&["list", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(doc["error"]["problems"][0]["location"], "servers[0].host");
}

// ── read-only (newer) inventory ──────────────────────────────────────────

#[test]
fn a_newer_inventory_is_shown_with_a_warning_but_never_changed() {
    let fixture = Fixture::with_text(NEWER);
    let recorder = fixture.ok(&["list"]);
    assert!(recorder.out.contains("a   h"), "{}", recorder.out);
    assert!(recorder.err.contains("read-only"), "{}", recorder.err);
    for args in [
        &["add", "b", "--host", "h"][..],
        &["edit", "a", "--name", "N"],
        &["edit", "a", "--host", "h"],
        &["defaults", "--user", "u"],
        &["remove", "a", "-y"],
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "inventory_read_only", "{args:?}");
    }
    // Refused before any question is asked.
    let (result, recorder) = fixture.run(&["remove", "a"], Some(true));
    assert_eq!(result.map_err(|f| f.code()), Err("inventory_read_only"));
    assert!(recorder.asked.is_empty());
    assert_eq!(fixture.bytes(), NEWER.as_bytes());
}

// ── remove ───────────────────────────────────────────────────────────────

#[test]
fn remove_with_yes_removes_a_server_without_sites() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["remove", "bare", "-y"]);
    assert_eq!(recorder.out, "Removed server 'bare'.\n");
    assert!(recorder.asked.is_empty());
    assert!(fixture.load().server("bare").is_none());
}

#[test]
fn remove_refuses_a_server_in_use_unless_it_cascades() {
    let fixture = Fixture::with(&sample());
    let (failure, recorder) = fixture.fails(&["remove", "eu-1", "-y"]);
    assert_eq!(
        failure.to_string(),
        "server 'eu-1' is used by 2 site(s): 'shop', 'blog'\n  \
         pass --cascade to remove those sites with it"
    );
    assert!(recorder.asked.is_empty());
    let (result, recorder) = fixture.run(&["remove", "eu-1"], Some(true));
    assert!(result.is_err());
    assert!(recorder.asked.is_empty(), "refused before asking");

    let recorder = fixture.ok(&["remove", "eu-1", "--cascade", "--yes"]);
    assert!(
        recorder
            .out
            .starts_with("Removed server 'eu-1' and its 2 sites"),
        "{}",
        recorder.out
    );
    let inventory = fixture.load();
    assert!(inventory.sites().is_empty());
    assert!(
        inventory
            .group("qa")
            .is_some_and(|group| group.sites.is_empty())
    );
}

#[test]
fn remove_asks_and_a_no_changes_nothing() {
    let fixture = Fixture::with(&sample());
    let before = fixture.bytes();
    let (result, recorder) = fixture.run(&["remove", "eu-1", "--cascade"], Some(false));
    assert!(result.is_ok());
    assert_eq!(
        recorder.asked,
        ["Remove server 'eu-1' and its 2 sites: 'shop', 'blog' \
             (they also leave 1 group: 'qa')? [y/N]"]
    );
    assert_eq!(recorder.out, "Aborted. Nothing changed.\n");
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn remove_asks_and_a_yes_removes() {
    let fixture = Fixture::with(&sample());
    let (result, recorder) = fixture.run(&["remove", "bare"], Some(true));
    assert!(result.is_ok());
    assert_eq!(recorder.asked, ["Remove server 'bare'? [y/N]"]);
    assert!(fixture.load().server("bare").is_none());
}

#[test]
fn remove_without_a_terminal_needs_yes() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["remove", "bare"]);
    assert_eq!(failure.code(), "confirmation_required");
    assert!(failure.to_string().contains("pass -y (--yes)"), "{failure}");
}

#[test]
fn remove_refuses_an_unknown_server_before_asking() {
    let fixture = Fixture::with(&sample());
    let (result, recorder) = fixture.run(&["remove", "nope"], Some(true));
    assert_eq!(result.map_err(|f| f.code()), Err("invalid_input"));
    assert!(recorder.asked.is_empty());
}

#[test]
fn remove_refuses_when_the_sites_changed_while_confirming() {
    // Another process adds a site to the server while the user reads the
    // question: what was confirmed is no longer what would be removed.
    let fixture = Fixture::with(&sample());
    let command = parse(&["remove", "eu-1", "--cascade"]);
    let mut recorder = Recorder::new();
    let result = recorder.run_asking(
        |_| {
            io::update(&fixture.path, |inventory| {
                inventory.add_site(Site::new("late", "eu-1", "/var/www/late"))
            })
            .expect("concurrent edit");
            Ok(Some(true))
        },
        |console| command.run(&fixture.path, console),
    );
    let Err(failure) = result else {
        panic!("the removal must be refused");
    };
    assert_eq!(failure.code(), "inventory_changed");
    let inventory = fixture.load();
    assert!(inventory.server("eu-1").is_some(), "nothing was removed");
    assert_eq!(inventory.sites_on("eu-1").count(), 3);
}

/// Remove `id` (asking), while another process applies `edit` to the
/// inventory during the question; the result and the inventory after.
fn remove_while(
    fixture: &Fixture,
    args: &[&str],
    edit: impl Fn(&mut Inventory) -> Result<(), apvm_sites::Error>,
) -> (Result<(), Failure>, Inventory) {
    let command = parse(args);
    let mut recorder = Recorder::new();
    let result = recorder.run_asking(
        |_| {
            io::update(&fixture.path, &edit).expect("concurrent edit");
            Ok(Some(true))
        },
        |console| command.run(&fixture.path, console),
    );
    (result, fixture.load())
}

#[test]
fn remove_refuses_when_the_server_itself_changed_while_confirming() {
    // The question named the server; another process repoints it to
    // another host meanwhile: what was confirmed is not what would go.
    let fixture = Fixture::with(&sample());
    let (result, inventory) = remove_while(&fixture, &["remove", "bare"], |inventory| {
        let mut server = inventory.server("bare").cloned().expect("known");
        server.host = "elsewhere.example.com".into();
        inventory.update_server(server).map(drop)
    });
    let failure = result.expect_err("refused");
    assert_eq!(failure.code(), "inventory_changed");
    assert_eq!(
        failure.to_string(),
        "server 'bare' or its sites changed while you were confirming; nothing was removed: \
         run the command again"
    );
    assert!(inventory.server("bare").is_some(), "nothing was removed");
}

#[test]
fn remove_refuses_when_only_the_groups_changed_while_confirming() {
    // Same sites, but one of them joined another group: the question's
    // "they also leave …" no longer holds.
    let fixture = Fixture::with(&sample());
    let (result, inventory) =
        remove_while(&fixture, &["remove", "eu-1", "--cascade"], |inventory| {
            inventory.add_group(Group::new("late", vec!["blog".into()]))
        });
    assert_eq!(result.map_err(|f| f.code()), Err("inventory_changed"));
    assert_eq!(inventory.sites_on("eu-1").count(), 2, "nothing was removed");
}

#[test]
fn remove_ignores_changes_elsewhere_while_confirming() {
    let fixture = Fixture::with(&sample());
    let command = parse(&["remove", "bare"]);
    let mut recorder = Recorder::new();
    let result = recorder.run_asking(
        |_| {
            io::update(&fixture.path, |inventory| {
                inventory.add_server(Server::new("other", "o"))
            })
            .expect("concurrent edit");
            Ok(Some(true))
        },
        |console| command.run(&fixture.path, console),
    );
    assert!(result.is_ok(), "{result:?}");
    let inventory = fixture.load();
    assert!(inventory.server("bare").is_none());
    assert!(
        inventory.server("other").is_some(),
        "the other edit is kept"
    );
}

#[cfg(unix)]
#[test]
fn every_write_leaves_a_private_file() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::empty();
    fixture.ok(&["add", "a", "--host", "h"]);
    let mode = std::fs::metadata(&fixture.path)
        .expect("written")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

// ── every flag, from clap's own definition ───────────────────────────────
//
// These tests enumerate the flags clap really accepts, independently of the
// field table in `fields`, so a flag the table forgets (or mishandles) is
// caught: it must save its value, report its change, be refused together
// with its `--unset`, and warn about relative paths.

/// The `apvm server <command>` definition.
fn command_def(command: &str) -> clap::Command {
    let cli = crate::Cli::command();
    let server = cli.find_subcommand("server").expect("server").clone();
    server.find_subcommand(command).expect("subcommand").clone()
}

/// Flags that are not settings.
const NOT_SETTINGS: [&str; 4] = ["unset", "json", "yes", "cascade"];

/// The long names of the flags of `command` that set a value (switches
/// included), in definition order.
fn setting_flags(command: &str) -> Vec<(String, bool)> {
    command_def(command)
        .get_arguments()
        .filter_map(|arg| Some((arg.get_long()?, arg)))
        .filter(|(long, _)| !NOT_SETTINGS.contains(long))
        .map(|(long, arg)| {
            (
                long.to_string(),
                matches!(arg.get_action(), ArgAction::SetTrue),
            )
        })
        .collect()
}

/// The `--unset` values clap accepts for `command`.
fn unset_values(command: &str) -> Vec<String> {
    let def = command_def(command);
    let unset = def
        .get_arguments()
        .find(|arg| arg.get_long() == Some("unset"))
        .expect("--unset");
    unset
        .get_possible_values()
        .iter()
        .map(|value| value.get_name().to_string())
        .collect()
}

/// A valid sample value for a value flag: absolute paths, so no warning.
fn sample_value(flag: &str) -> &'static str {
    match flag {
        "host" => "new.example.com",
        "user" => "sample-user",
        "port" => "2345",
        "connect-timeout" => "37",
        "jump" => "bastion.example.com",
        "key" => "/abs/key",
        "known-hosts" => "/abs/known_hosts",
        "ssh-config" => "/abs/ssh_config",
        "ssh-binary" => "/abs/ssh",
        "name" => "Sample name",
        "notes" => "Sample notes",
        other => panic!("no sample value for --{other}: add one to `sample_value`"),
    }
}

/// The arguments that pass `flag` (with its sample value unless a switch).
fn flag_args(flag: &str, switch: bool) -> Vec<String> {
    let mut args = vec![format!("--{flag}")];
    if !switch {
        args.push(sample_value(flag).to_string());
    }
    args
}

/// The setting a flag changes, by its `--unset` / report name.
fn field_of(flag: &str) -> &str {
    flag.strip_prefix("no-").unwrap_or(flag)
}

/// Whether `json` holds the value `flag` was given: its sample (as a string
/// or a number), or for a switch the multiplex setting it implies.
fn holds(json: &serde_json::Value, flag: &str, switch: bool) -> bool {
    if switch {
        return json["ssh"]["multiplex"] == serde_json::json!(flag == "multiplex");
    }
    let sample = sample_value(flag);
    match json {
        serde_json::Value::Object(map) => map.values().any(|value| holds(value, flag, false)),
        serde_json::Value::String(text) => text == sample,
        serde_json::Value::Number(number) => number.to_string() == sample,
        _ => false,
    }
}

/// `args` as `&str`s.
fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// The server `id` in the fixture's file, as JSON.
fn server_json(fixture: &Fixture, id: &str) -> serde_json::Value {
    serde_json::to_value(fixture.load().server(id).expect("exists")).expect("serializes")
}

/// The defaults in the fixture's file, as JSON.
fn defaults_json(fixture: &Fixture) -> serde_json::Value {
    serde_json::to_value(fixture.load().defaults()).expect("serializes")
}

#[test]
fn every_flag_of_add_saves_its_value() {
    let flags = setting_flags("add");
    assert!(flags.len() >= 12, "{flags:?}");
    for (flag, switch) in flags.into_iter().filter(|(flag, _)| flag != "host") {
        let fixture = Fixture::empty();
        let mut args = vec!["add".to_string(), "x".into(), "--host".into(), "h".into()];
        args.extend(flag_args(&flag, switch));
        let recorder = fixture.ok(&strs(&args));
        assert_eq!(recorder.err, "", "--{flag}: no warning for {args:?}");
        assert!(
            holds(&server_json(&fixture, "x"), &flag, switch),
            "--{flag} was not saved"
        );
    }
}

#[test]
fn every_flag_of_edit_alone_is_a_saved_and_reported_change() {
    for (flag, switch) in setting_flags("edit") {
        let fixture = Fixture::with(&sample());
        let mut args = vec!["edit".to_string(), "eu-1".into()];
        args.extend(flag_args(&flag, switch));
        let recorder = fixture.ok(&strs(&args));
        let field = field_of(&flag);
        assert_eq!(
            recorder.out,
            format!("Updated server 'eu-1': {field}.\n"),
            "--{flag}"
        );
        assert_eq!(recorder.err, "", "--{flag}: no warning");
        assert!(
            holds(&server_json(&fixture, "eu-1"), &flag, switch),
            "--{flag} was not saved"
        );
    }
}

#[test]
fn every_flag_of_defaults_alone_is_a_saved_and_reported_change() {
    let flags = setting_flags("defaults");
    assert!(flags.len() >= 10, "{flags:?}");
    for (flag, switch) in flags {
        let fixture = Fixture::with(&sample());
        let recorder = fixture.ok(&strs(&flag_args(&flag, switch)).into_iter().fold(
            vec!["defaults"],
            |mut args, arg| {
                args.push(arg);
                args
            },
        ));
        let field = field_of(&flag);
        assert_eq!(
            recorder.out,
            format!("Updated the SSH defaults: {field}.\n"),
            "--{flag}"
        );
        assert_eq!(recorder.err, "", "--{flag}: no warning");
        assert!(
            holds(&defaults_json(&fixture), &flag, switch),
            "--{flag} was not saved"
        );
    }
}

#[test]
fn unset_offers_exactly_the_optional_settings_of_each_record() {
    for command in ["edit", "defaults"] {
        let mut settable: Vec<String> = setting_flags(command)
            .iter()
            .map(|(flag, _)| field_of(flag).to_string())
            .filter(|field| field != "host")
            .collect();
        settable.dedup();
        let mut unset = unset_values(command);
        settable.sort();
        unset.sort();
        assert_eq!(unset, settable, "{command}");
    }
}

/// `eu-1` and the defaults with every optional setting set (absolute paths).
fn everything_set() -> Inventory {
    let mut inventory = sample();
    let mut full = SshSettings {
        user: Some("u".into()),
        port: Some(22),
        identity_file: Some("/k".into()),
        connect_timeout_secs: Some(9),
        known_hosts_file: Some("/kh".into()),
        ssh_config_file: Some("/c".into()),
        proxy_jump: Some("j".into()),
        multiplex: Some(false),
        binary: Some("/usr/bin/ssh".into()),
    };
    inventory
        .set_defaults(Defaults { ssh: full.clone() })
        .expect("valid defaults");
    let mut server = inventory.server("eu-1").cloned().expect("known");
    full.binary = None;
    server.ssh = full;
    server.name = Some("N".into());
    server.notes = Some("n".into());
    inventory.update_server(server).expect("valid server");
    inventory
}

#[test]
fn every_unset_value_alone_clears_its_setting_only() {
    for (command, target, done) in [
        ("edit", Some("eu-1"), "Updated server 'eu-1'"),
        ("defaults", None, "Updated the SSH defaults"),
    ] {
        for field in unset_values(command) {
            let fixture = Fixture::with(&everything_set());
            let args: Vec<&str> = [command]
                .into_iter()
                .chain(target)
                .chain(["--unset", &field])
                .collect();
            let recorder = fixture.ok(&args);
            assert_eq!(recorder.out, format!("{done}: {field}.\n"), "{args:?}");
        }
    }
}

#[test]
fn setting_and_unsetting_one_field_is_refused_for_every_field() {
    for (command, target) in [("edit", Some("eu-1")), ("defaults", None)] {
        for (flag, switch) in setting_flags(command) {
            let field = field_of(&flag);
            if field == "host" {
                continue;
            }
            let fixture = Fixture::with(&sample());
            let mut args: Vec<String> = [command]
                .into_iter()
                .chain(target)
                .map(String::from)
                .collect();
            args.extend(flag_args(&flag, switch));
            args.extend(["--unset".to_string(), field.to_string()]);
            let (failure, _) = fixture.fails(&strs(&args));
            // Named by the flag typed (`--no-multiplex`, not its field).
            assert_eq!(
                failure.to_string(),
                format!("--{flag} and --unset {field} contradict each other: pass only one"),
                "{args:?}"
            );
            assert_eq!(failure.code(), "invalid_input");
        }
    }
}

/// The flags that take a local path (the warning's oracle).
const PATH_FLAGS: [&str; 4] = ["key", "known-hosts", "ssh-config", "ssh-binary"];

/// Run `apvm server <args> --<flag> <value>` and return its warnings.
fn warnings(fixture: &Fixture, args: &[&str], flag: &str, value: &str) -> String {
    let long = format!("--{flag}");
    let all: Vec<&str> = args.iter().copied().chain([long.as_str(), value]).collect();
    fixture.ok(&all).err
}

#[test]
fn every_relative_path_warns_once_and_only_when_it_changes() {
    for (command, base) in [
        ("add", &["add", "x", "--host", "h"][..]),
        ("edit", &["edit", "eu-1"]),
        ("defaults", &["defaults"]),
    ] {
        let flags = setting_flags(command);
        for flag in PATH_FLAGS
            .iter()
            .filter(|flag| flags.iter().any(|(f, _)| f == *flag))
        {
            let fixture = Fixture::with(&sample());
            let err = warnings(&fixture, base, flag, "rel/file");
            assert_eq!(
                err.matches("is a relative path").count(),
                1,
                "{command} --{flag}: {err}"
            );
            assert!(
                err.contains(&format!("--{flag} 'rel/file'")),
                "{command} --{flag}: {err}"
            );
            if command != "add" {
                let again = warnings(&fixture, base, flag, "rel/file");
                assert_eq!(again, "", "{command} --{flag}: unchanged, so no warning");
            }
        }
    }
    let flags = setting_flags("defaults");
    for flag in PATH_FLAGS {
        assert!(
            flags.iter().any(|(f, _)| f == flag),
            "--{flag} is a defaults flag"
        );
    }
}

#[test]
fn a_bare_ssh_binary_name_is_a_path_lookup_not_a_relative_path() {
    let fixture = Fixture::empty();
    assert_eq!(warnings(&fixture, &["defaults"], "ssh-binary", "ssh"), "");
    let err = warnings(&fixture, &["defaults"], "ssh-binary", "bin/ssh");
    assert!(err.contains("--ssh-binary 'bin/ssh'"), "{err}");
    // A backslash separates paths only on Windows.
    let err = warnings(&fixture, &["defaults"], "ssh-binary", "bin\\ssh");
    assert_eq!(err.is_empty(), cfg!(unix), "{err}");
}

#[test]
fn an_empty_value_is_refused_with_the_unset_remedy() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "eu-1", "--notes", ""]);
    let text = failure.to_string();
    assert!(text.contains("\n  --notes: must not be empty"), "{text}");
    assert!(text.ends_with("\n  to clear it: --unset notes"), "{text}");
    assert_eq!(failure.code(), "invalid_input");
    assert_eq!(locations(&failure), ["--notes"]);

    let (failure, _) = fixture.fails(&["defaults", "--user", "", "--key", ""]);
    let text = failure.to_string();
    assert!(
        text.ends_with("\n  to clear them: --unset user,key"),
        "{text}"
    );

    // `add` has no --unset, and the host cannot be unset: the library's
    // message stands alone.
    for args in [
        &["add", "x", "--host", "h", "--name", ""][..],
        &["edit", "eu-1", "--host", ""],
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "invalid_input", "{args:?}");
        assert!(
            !failure.to_string().contains("--unset"),
            "{args:?}: {failure}"
        );
    }
    // Only refused input gets the remedy, not a read-only inventory.
    let newer = Fixture::with_text(NEWER);
    let (failure, _) = newer.fails(&["edit", "a", "--notes", ""]);
    assert_eq!(failure.code(), "inventory_read_only");
    assert!(!failure.to_string().contains("--unset"), "{failure}");
}
