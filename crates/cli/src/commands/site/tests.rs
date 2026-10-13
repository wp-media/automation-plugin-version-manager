//! Unit tests of `apvm site`: argument parsing, the pure helpers, and
//! whole commands run against temporary inventory files through a
//! recording [`Console`].

use std::path::PathBuf;

use apvm_sites::{Group, Inventory, Owner, RunAs, Server, Site};
use clap::{ArgAction, CommandFactory, Parser};
use tempfile::TempDir;

use super::*;
use crate::commands::inventory::Recorder;
use crate::commands::inventory::field_table::TableRecord;

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

    /// The site `id` in the file.
    fn site(&self, id: &str) -> Site {
        self.load().site(id).cloned().expect("the site exists")
    }

    /// The file's bytes (empty when it is missing).
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap_or_default()
    }

    /// Run `apvm site <args>`, answering any question with `answer`.
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
            panic!("`site {}` failed: {failure}", args.join(" "));
        }
        recorder
    }

    /// [`Fixture::run`], expecting a failure; the file must be unchanged.
    #[track_caller]
    fn fails(&self, args: &[&str]) -> (Failure, Recorder) {
        let before = self.bytes();
        let (result, recorder) = self.run(args, None);
        let Err(failure) = result else {
            panic!("`site {}` must fail", args.join(" "));
        };
        assert_eq!(self.bytes(), before, "a failed command wrote the file");
        (failure, recorder)
    }
}

/// Parse `apvm site <args>`.
#[track_caller]
fn parse(args: &[&str]) -> SiteArgs {
    let argv = ["apvm", "site"].iter().chain(args).copied();
    match crate::Cli::try_parse_from(argv).map(|cli| cli.command) {
        Ok(crate::Commands::Site(command)) => command,
        Ok(other) => panic!("parsed as {other:?}"),
        Err(error) => panic!("`site {}` does not parse: {error}", args.join(" ")),
    }
}

/// The clap error kind of `apvm site <args>`, when it does not parse.
fn parse_error(args: &[&str]) -> Option<clap::error::ErrorKind> {
    let argv = ["apvm", "site"].iter().chain(args).copied();
    crate::Cli::try_parse_from(argv)
        .err()
        .map(|error| error.kind())
}

/// Servers `eu-1` (sites `shop` and `blog`) and `us-1` (site `api`);
/// group `qa` lists `shop` and `api`, group `empty` lists none.
fn sample() -> Inventory {
    let mut inventory = Inventory::new();
    for (id, host) in [("eu-1", "203.0.113.10"), ("us-1", "198.51.100.7")] {
        inventory
            .add_server(Server::new(id, host))
            .expect("valid server");
    }
    for (id, server) in [("shop", "eu-1"), ("blog", "eu-1"), ("api", "us-1")] {
        inventory
            .add_site(Site::new(id, server, format!("/var/www/{id}")))
            .expect("valid site");
    }
    inventory
        .add_group(Group::new("qa", vec!["shop".into(), "api".into()]))
        .expect("valid group");
    inventory
        .add_group(Group::new("empty", Vec::new()))
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

/// `ids` as owned strings.
fn owned(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

/// A newer, read-only inventory with site `s` on server `a`.
const NEWER: &str = r#"{"version": 9, "servers": [{"id": "a", "host": "h"}],
    "sites": [{"id": "s", "server": "a", "path": "/var/www/s"}]}"#;

// ── parsing ──────────────────────────────────────────────────────────────

#[test]
fn add_needs_a_server_and_a_path() {
    for args in [
        &["add", "x", "--path", "/p"][..],
        &["add", "x", "--server", "eu-1"],
    ] {
        assert_eq!(
            parse_error(args),
            Some(clap::error::ErrorKind::MissingRequiredArgument),
            "{args:?}"
        );
    }
}

#[test]
fn edit_needs_at_least_one_change() {
    assert_eq!(
        parse_error(&["edit", "shop"]),
        Some(clap::error::ErrorKind::MissingRequiredArgument)
    );
    for change in [
        &["--path", "/p"][..],
        &["--allow-root"],
        &["--group", "qa"],
        &["--unset", "notes"],
        &["--ownership", "off"],
    ] {
        let mut args = vec!["edit", "shop"];
        args.extend(change);
        assert_eq!(parse_error(&args), None, "{args:?}");
    }
}

#[test]
fn unset_accepts_optional_fields_only() {
    let action = parse(&["edit", "s", "--unset", "url,run-as", "--unset", "timeout"]).action;
    let SiteAction::Edit(args) = action else {
        panic!("an edit");
    };
    assert_eq!(args.unset, [Field::Url, Field::RunAs, Field::Timeout]);
    for refused in ["server", "path", "id", "group", "nope"] {
        assert_eq!(
            parse_error(&["edit", "s", "--unset", refused]),
            Some(clap::error::ErrorKind::InvalidValue),
            "{refused}"
        );
    }
}

#[test]
fn malformed_values_are_argument_errors() {
    use clap::error::ErrorKind;
    let base = ["add", "x", "--server", "eu-1", "--path", "/p"];
    for (extra, kind) in [
        (&["--timeout", "soon"][..], ErrorKind::ValueValidation),
        (&["--timeout", "-1"], ErrorKind::UnknownArgument),
        (&["--timeout", "4294967296"], ErrorKind::ValueValidation),
        (&["--ownership", "root"], ErrorKind::InvalidValue),
        (&["--ownership", "Mirror"], ErrorKind::InvalidValue),
        (&["--allow-root=no"], ErrorKind::TooManyValues),
    ] {
        let args: Vec<&str> = base.iter().chain(extra).copied().collect();
        assert_eq!(parse_error(&args), Some(kind), "{args:?}");
    }
}

#[test]
fn only_reads_take_json() {
    assert!(parse(&["list", "--json"]).json());
    assert!(parse(&["show", "a", "--json"]).json());
    assert!(!parse(&["list"]).json());
    for args in [
        &["add", "a", "--server", "s", "--path", "/p", "--json"][..],
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

#[test]
fn group_flags_take_repeats_and_comma_lists() {
    let action = parse(&[
        "add", "x", "--server", "s", "--path", "/p", "--group", "a,b", "--group", "c",
    ])
    .action;
    let SiteAction::Add(args) = action else {
        panic!("an add");
    };
    assert_eq!(args.groups, ["a", "b", "c"]);
    let SiteAction::List(args) = parse(&["list", "--server", "a,b", "--group", "qa"]).action else {
        panic!("a list");
    };
    assert_eq!((args.servers.len(), args.groups.len()), (2, 1));
}

// ── pure helpers ─────────────────────────────────────────────────────────

#[test]
fn every_flag_of_add_lands_in_the_site_record() {
    let action = parse(&[
        "add",
        "shop",
        "--server",
        "eu-1",
        "--path",
        "/var/www/shop",
        "--url",
        "https://shop.example.com",
        "--wp-cli",
        "/usr/bin/php8.3 -q /opt/wp-cli.phar",
        "--run-as",
        "deploy",
        "--owner",
        "ftp:www-data",
        "--ownership",
        "mirror-root",
        "--plugins-dir",
        "/var/www/shop/app/plugins",
        "--allow-root",
        "--tmp-dir",
        "/var/tmp",
        "--timeout",
        "600",
        "--name",
        "Shop",
        "--notes",
        "n",
    ])
    .action;
    let SiteAction::Add(args) = action else {
        panic!("an add");
    };
    let expected = Site {
        name: Some("Shop".into()),
        url: Some("https://shop.example.com".into()),
        wp_cli: Some(owned(&["/usr/bin/php8.3", "-q", "/opt/wp-cli.phar"])),
        run_as: Some(RunAs::User("deploy".into())),
        owner: Some(Owner::Explicit {
            user: "ftp".into(),
            group: Some("www-data".into()),
        }),
        ownership: Some(OwnershipPolicy::MirrorRoot),
        plugins_dir: Some("/var/www/shop/app/plugins".into()),
        allow_root: Some(true),
        remote_tmp_dir: Some("/var/tmp".into()),
        command_timeout_secs: Some(600),
        notes: Some("n".into()),
        ..Site::new("shop", "eu-1", "/var/www/shop")
    };
    assert_eq!(args.to_site(), expected);
}

#[test]
fn flags_not_passed_leave_the_record_sparse() {
    let SiteAction::Add(args) = parse(&["add", "s", "--server", "e", "--path", "/p"]).action else {
        panic!("an add");
    };
    assert_eq!(args.to_site(), Site::new("s", "e", "/p"));
}

/// A site whose every field the library refuses.
fn refused_site() -> Site {
    Site {
        id: "Bad Id".into(),
        name: Some(String::new()),
        server: "nope".into(),
        path: "relative".into(),
        url: Some("ftp://x".into()),
        wp_cli: Some(Vec::new()),
        run_as: Some(RunAs::User("-x".into())),
        owner: Some(Owner::from("auto:g")),
        ownership: None,
        plugins_dir: Some("rel".into()),
        allow_root: None,
        remote_tmp_dir: Some("/tmp/a b".into()),
        command_timeout_secs: Some(0),
        notes: Some(String::new()),
    }
}

#[test]
fn the_flag_map_names_every_location_a_refused_site_can_have() {
    let mut inventory = Inventory::new();
    inventory
        .add_server(Server::new("eu-1", "h"))
        .expect("valid server");
    let error = inventory.add_site(refused_site()).expect_err("refused");
    let mut named = locations(&name_flags(error, &fields::flag_map()));
    named.sort();
    assert_eq!(
        named,
        [
            "--name",
            "--notes",
            "--owner",
            "--path",
            "--plugins-dir",
            "--run-as",
            "--server",
            "--timeout",
            "--tmp-dir",
            "--url",
            "--wp-cli",
            "<ID>",
        ]
    );
}

#[test]
fn removal_messages_name_the_groups() {
    let mut inventory = sample();
    let lone = inventory.clone().remove_site("blog").expect("removable");
    assert_eq!(
        removal_question(&lone),
        "Remove site 'blog' from the inventory?"
    );
    assert_eq!(removed_text(&lone), "Removed site 'blog'.\n");
    inventory
        .add_group(Group::new("prod", vec!["shop".into()]))
        .expect("valid group");
    let grouped = inventory.remove_site("shop").expect("removable");
    assert_eq!(
        removal_question(&grouped),
        "Remove site 'shop' from the inventory (it also leaves 2 groups: 'qa', 'prod')?"
    );
    assert_eq!(
        removed_text(&grouped),
        "Removed site 'shop'.\nIt was removed from 2 groups: 'qa', 'prod'.\n"
    );
}

#[test]
fn edit_reports_name_the_settings_then_the_groups() {
    let before = Site::new("s", "e", "/p");
    let mut after = before.clone();
    after.notes = Some("n".into());
    after.url = Some("https://a.example".into());
    assert_eq!(
        edit_report("s", &before, &after, &[]),
        "Updated site 's': url, notes.\n"
    );
    assert_eq!(
        edit_report("s", &before, &after, &owned(&["qa"])),
        "Updated site 's': url, notes, groups.\nIt joined 1 group: 'qa'.\n"
    );
    assert_eq!(
        edit_report("s", &before, &before, &owned(&["qa", "b"])),
        "Updated site 's': groups.\nIt joined 2 groups: 'qa', 'b'.\n"
    );
    assert_eq!(
        edit_report("s", &before, &before, &[]),
        "Site 's' is unchanged.\n"
    );
}

#[test]
fn joined_text_names_the_groups_or_says_nothing() {
    assert_eq!(joined_text(&[]), "");
    assert_eq!(joined_text(&owned(&["qa"])), "It joined 1 group: 'qa'.\n");
}

#[test]
fn join_groups_skips_memberships_and_repeats_and_refuses_unknown_groups() {
    let mut inventory = sample();
    let joined = join_groups(&mut inventory, "shop", &owned(&["qa", "empty", "empty"]))
        .expect("known groups");
    assert_eq!(joined, ["empty"], "already in qa; empty joined once");
    assert_eq!(
        inventory.group("empty").map(|g| g.sites.clone()),
        Some(owned(&["shop"]))
    );
    let failure =
        join_groups(&mut inventory, "blog", &owned(&["qa", "nope"])).expect_err("unknown group");
    assert_eq!(
        failure.to_string(),
        "group 'nope' does not exist\n  apvm has no group commands yet: add the group to \
         the 'groups' list of the inventory file by hand, as {\"id\": \"<ID>\", \"sites\": []}, \
         or from Node with SiteInventory"
    );
}

/// The ids `filter_sites` keeps in [`sample`].
fn filtered(servers: &[&str], groups: &[&str]) -> Result<Vec<String>, String> {
    let inventory = sample();
    filter_sites(&inventory, &owned(servers), &owned(groups))
        .map(|sites| sites.into_iter().map(|site| site.id.clone()).collect())
        .map_err(|failure| failure.to_string())
}

#[test]
fn filters_intersect_servers_and_groups_in_inventory_order() {
    let ok = |servers, groups| filtered(servers, groups).expect("known ids");
    assert_eq!(ok(&[], &[]), ["shop", "blog", "api"]);
    assert_eq!(ok(&["eu-1"], &[]), ["shop", "blog"]);
    assert_eq!(ok(&["us-1", "eu-1"], &[]), ["shop", "blog", "api"]);
    assert_eq!(ok(&[], &["qa"]), ["shop", "api"]);
    assert_eq!(ok(&[], &["qa", "empty"]), ["shop", "api"]);
    assert_eq!(ok(&["eu-1"], &["qa"]), ["shop"], "both filters apply");
    assert!(ok(&[], &["empty"]).is_empty());
}

#[test]
fn filters_refuse_unknown_ids() {
    let error = |servers, groups| filtered(servers, groups).expect_err("unknown id");
    assert!(error(&["eu-1", "x"], &["y"]).starts_with("server 'x' does not exist"));
    assert!(error(&[], &["qa", "y"]).starts_with("group 'y' does not exist"));
    assert!(error(&["EU-1"], &[]).starts_with("server 'EU-1' does not exist"));
}

// ── add ──────────────────────────────────────────────────────────────────

#[test]
fn add_saves_the_site_and_reports_it() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["add", "news", "--server", "us-1", "--path", "/srv/news"]);
    assert_eq!(recorder.out, "Added site 'news'.\n");
    assert_eq!(recorder.err, "");
    assert_eq!(fixture.site("news"), Site::new("news", "us-1", "/srv/news"));
}

#[test]
fn add_joins_the_named_groups_in_the_same_change() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&[
        "add",
        "news",
        "--server",
        "us-1",
        "--path",
        "/srv/news",
        "--group",
        "qa,empty",
    ]);
    assert_eq!(
        recorder.out,
        "Added site 'news'.\nIt joined 2 groups: 'qa', 'empty'.\n"
    );
    let inventory = fixture.load();
    let groups: Vec<&str> = inventory.groups_of("news").map(|g| g.id.as_str()).collect();
    assert_eq!(groups, ["qa", "empty"]);
}

#[test]
fn add_with_an_unknown_group_writes_nothing() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&[
        "add",
        "news",
        "--server",
        "us-1",
        "--path",
        "/srv/news",
        "--group",
        "qa,nope",
    ]);
    assert_eq!(failure.code(), "invalid_input");
    assert!(
        failure
            .to_string()
            .starts_with("group 'nope' does not exist"),
        "{failure}"
    );
}

#[test]
fn add_refuses_a_taken_id_and_an_unknown_server() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["add", "shop", "--server", "eu-1", "--path", "/x"]);
    assert_eq!(
        failure.to_string(),
        "site 'shop' already exists\n  change it with 'apvm site edit', or choose another id"
    );
    let (failure, _) = fixture.fails(&["add", "x", "--server", "eu-9", "--path", "/x"]);
    assert_eq!(locations(&failure), ["--server"]);
    assert!(
        failure
            .to_string()
            .contains("--server: unknown server 'eu-9'"),
        "{failure}"
    );
}

#[test]
fn add_refuses_invalid_input_naming_the_flags_and_creates_nothing() {
    let fixture = Fixture::empty();
    let (failure, recorder) = fixture.fails(&[
        "add",
        "Shop",
        "--server",
        "eu-1",
        "--path",
        "var/www",
        "--url",
        "http://user:secret@x.example",
        "--timeout",
        "86401",
        "--name",
        "a\u{202e}b",
    ]);
    assert_eq!(failure.code(), "invalid_input");
    assert_eq!(
        locations(&failure),
        ["<ID>", "--name", "--server", "--path", "--url", "--timeout"]
    );
    let text = failure.to_string();
    assert!(
        !text.contains("secret"),
        "user info is never echoed: {text}"
    );
    assert!(
        !text.contains('\u{202e}'),
        "invisible characters are escaped: {text}"
    );
    assert!(!text.contains("--unset"), "add has no --unset: {text}");
    assert_eq!(recorder.out, "");
    assert!(!fixture.path.exists(), "nothing was created");
}

// ── --wp-cli, --run-as, --owner, --ownership ─────────────────────────────

#[test]
fn wp_cli_is_saved_as_tokens() {
    let fixture = Fixture::with(&sample());
    for (command, tokens) in [
        ("wp", &["wp"][..]),
        ("/usr/local/bin/wp", &["/usr/local/bin/wp"]),
        ("php8.3 /opt/wp-cli.phar", &["php8.3", "/opt/wp-cli.phar"]),
        (
            "  /usr/bin/php -n -q /opt/wp-cli.phar ",
            &["/usr/bin/php", "-n", "-q", "/opt/wp-cli.phar"],
        ),
        ("ea-php81 /opt/wp", &["ea-php81", "/opt/wp"]),
    ] {
        fixture.ok(&["edit", "shop", "--wp-cli", command]);
        let saved = fixture.site("shop").wp_cli.expect("set");
        assert_eq!(saved, tokens, "{command:?}");
    }
}

#[test]
fn wp_cli_refuses_wrappers_and_names_the_token() {
    let fixture = Fixture::with(&sample());
    for (command, location) in [
        ("sudo wp", "--wp-cli[0]"),
        ("wp eval-file", "--wp-cli[0]"),
        ("env -u X wp", "--wp-cli[0]"),
        ("php -d x /opt/wp", "--wp-cli[1]"),
        ("php wp-cli.phar", "--wp-cli[1]"),
        ("'wp'", "--wp-cli[0]"),
        ("wp;id", "--wp-cli[0]"),
        ("-wp", "--wp-cli[0]"),
        ("php -n -q -n /opt/wp", "--wp-cli"),
    ] {
        // `=` passes a value that starts with '-'.
        let flag = format!("--wp-cli={command}");
        let (failure, _) = fixture.fails(&["edit", "shop", &flag]);
        assert_eq!(failure.code(), "invalid_input", "{command:?}");
        assert!(
            locations(&failure).iter().any(|at| at == location),
            "{command:?}: {:?}",
            locations(&failure)
        );
        assert!(
            !failure.to_string().contains("--unset"),
            "{command:?}: not empty"
        );
    }
}

#[test]
fn an_empty_wp_cli_is_refused_with_the_unset_remedy() {
    let fixture = Fixture::with(&sample());
    for command in ["", "  \t "] {
        let (failure, _) = fixture.fails(&["edit", "shop", "--wp-cli", command]);
        assert_eq!(locations(&failure), ["--wp-cli"], "{command:?}");
        assert!(
            failure
                .to_string()
                .ends_with("\n  to clear it: --unset wp-cli"),
            "{failure}"
        );
    }
}

#[test]
fn run_as_takes_keywords_and_users_and_root_only_with_allow_root() {
    let fixture = Fixture::with(&sample());
    for (value, expected) in [
        ("auto", RunAs::Auto),
        ("login", RunAs::Login),
        ("deploy", RunAs::User("deploy".into())),
    ] {
        fixture.ok(&["edit", "shop", "--run-as", value]);
        assert_eq!(fixture.site("shop").run_as, Some(expected), "{value}");
    }
    for refused in ["root", "ROOT", "1001", "-u", "a b", ".", ""] {
        let flag = format!("--run-as={refused}");
        let (failure, _) = fixture.fails(&["edit", "shop", &flag]);
        assert_eq!(locations(&failure), ["--run-as"], "{refused:?}");
    }
    fixture.ok(&["edit", "shop", "--run-as", "root", "--allow-root"]);
    let site = fixture.site("shop");
    assert_eq!(
        (site.run_as, site.allow_root),
        (Some(RunAs::User("root".into())), Some(true))
    );
    // Root stays allowed only while --allow-root does.
    let (failure, _) = fixture.fails(&["edit", "shop", "--unset", "allow-root"]);
    assert_eq!(locations(&failure), ["--run-as"]);
}

/// The root remedy of `apvm site add|edit` without the flag.
const ROOT_REMEDY: &str =
    "\n  root needs --allow-root: pass it, or name another user in --run-as/--owner";

/// A site of [`sample`]'s `eu-1` with these identity settings.
fn identity(run_as: Option<&str>, owner: Option<&str>, allow_root: Option<bool>) -> Site {
    Site {
        run_as: run_as.map(RunAs::from),
        owner: owner.map(Owner::from),
        allow_root,
        ..Site::new("new", "eu-1", "/var/www/new")
    }
}

/// The library's refusal of adding `site` to [`sample`].
fn refusal(site: &Site) -> apvm_sites::Error {
    sample().add_site(site.clone()).expect_err("refused")
}

#[test]
fn root_is_refused_only_when_allowing_root_removes_a_problem() {
    let inventory = sample();
    for (run_as, owner) in [
        (Some("Root"), None),
        (None, Some("ROOT")),
        (None, Some("ftp:root")),
        (Some("root"), Some("-bad")),
    ] {
        let site = identity(run_as, owner, None);
        let problems = refusal(&site).problems().len();
        assert!(
            root_is_refused(&inventory, &site, problems),
            "{run_as:?} {owner:?}"
        );
    }
    // Refused before the root check, or for something else entirely.
    for owner in ["auto:root", "AUTO:root", "-x"] {
        let site = identity(None, Some(owner), None);
        let problems = refusal(&site).problems().len();
        assert!(!root_is_refused(&inventory, &site, problems), "{owner}");
    }
    let allowed = identity(Some("root"), None, Some(true));
    assert!(!root_is_refused(&inventory, &allowed, 1));
}

#[test]
fn root_remedy_needs_a_refused_root_rule() {
    let inventory = sample();
    let rooted = identity(Some("root"), None, None);
    assert_eq!(
        root_remedy(&inventory, &rooted, &refusal(&rooted), RootChange::Kept).as_deref(),
        Some(&ROOT_REMEDY[3..])
    );
    // Another refusal of a root site: no root remedy.
    let taken = Site {
        id: "shop".into(),
        ..rooted.clone()
    };
    let exists = refusal(&taken);
    assert_eq!(exists.code(), apvm_sites::Code::InvalidInput);
    assert_eq!(
        root_remedy(&inventory, &taken, &exists, RootChange::Kept),
        None
    );
}

#[test]
fn root_change_wording_follows_what_the_edit_did() {
    let mut inventory = sample();
    let mut shop = inventory.site("shop").cloned().expect("known");
    shop.allow_root = Some(true);
    inventory.update_site(shop).expect("valid");
    let unset = [Field::Url, Field::AllowRoot];
    assert_eq!(
        RootChange::of_edit(&inventory, "shop", &[]),
        RootChange::Kept
    );
    assert_eq!(
        RootChange::of_edit(&inventory, "shop", &unset),
        RootChange::Revoked
    );
    assert_eq!(
        RootChange::of_edit(&inventory, "blog", &unset),
        RootChange::NeverAllowed
    );
    assert_eq!(
        RootChange::Revoked.remedy(),
        "--run-as/--owner name root, which needs --allow-root: keep it \
         (drop --unset allow-root), or name another user there first"
    );
    assert_eq!(
        RootChange::NeverAllowed.remedy(),
        "root needs --allow-root: pass --allow-root instead of --unset allow-root, \
         or name another user in --run-as/--owner"
    );
}

#[test]
fn root_remedy_is_absent_when_root_is_not_the_cause() {
    let fixture = Fixture::with(&sample());
    for args in [
        &[
            "add", "shop", "--server", "eu-1", "--path", "/x", "--run-as", "root",
        ][..],
        &[
            "add",
            "x",
            "--server",
            "eu-1",
            "--path",
            "/x",
            "--owner",
            "auto:root",
        ],
        &[
            "add",
            "x",
            "--server",
            "eu-1",
            "--path",
            "/x",
            "--owner",
            "AUTO:root",
        ],
        &["edit", "shop", "--owner", "auto:root"],
    ] {
        let (failure, _) = fixture.fails(args);
        assert!(
            !failure.to_string().contains("--allow-root"),
            "{args:?}: {failure}"
        );
    }
}

#[test]
fn unset_allow_root_on_a_site_that_never_allowed_it_says_to_pass_it() {
    let fixture = Fixture::with(&sample());
    let (failure, _) =
        fixture.fails(&["edit", "shop", "--run-as", "root", "--unset", "allow-root"]);
    assert!(
        failure.to_string().ends_with(
            "\n  root needs --allow-root: pass --allow-root instead of --unset allow-root, \
             or name another user in --run-as/--owner"
        ),
        "{failure}"
    );
}

#[test]
fn an_unknown_group_names_the_inventory_file() {
    let fixture = Fixture::with(&sample());
    for args in [
        &[
            "add", "x", "--server", "eu-1", "--path", "/x", "--group", "nope",
        ][..],
        &["edit", "shop", "--group", "nope"],
        &["list", "--group", "nope"],
    ] {
        let (failure, _) = fixture.fails(args);
        let text = failure.to_string();
        let line = format!("\n  the inventory file is {:?}", fixture.path);
        assert!(text.ends_with(&line), "{args:?}: {text}");
        assert!(text.contains(r#"as {"id": "<ID>", "sites": []}"#), "{text}");
    }
    // Other failures are untouched.
    let (failure, _) = fixture.fails(&["show", "nope"]);
    assert!(
        !failure.to_string().contains("inventory file is"),
        "{failure}"
    );
    let plain = with_inventory_path(not_found(EntityKind::Site, "x"), &fixture.path);
    assert!(!plain.to_string().contains("inventory file"), "{plain}");
}

#[test]
fn the_inventory_path_is_escaped() {
    let failure = with_inventory_path(
        not_found(EntityKind::Group, "g"),
        std::path::Path::new("/tmp/a\u{1b}[2J\u{202e}b.json"),
    );
    let text = failure.to_string();
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{202e}'),
        "{text:?}"
    );
}

#[test]
fn refused_root_names_allow_root_as_a_flag() {
    let fixture = Fixture::with(&sample());
    for args in [
        &["edit", "shop", "--run-as", "root"][..],
        &["edit", "shop", "--owner", "ftp:Root"],
        &[
            "add", "x", "--server", "eu-1", "--path", "/x", "--run-as", "ROOT",
        ],
    ] {
        let (failure, _) = fixture.fails(args);
        assert!(
            failure.to_string().ends_with(ROOT_REMEDY),
            "{args:?}: {failure}"
        );
    }
    // After the `--unset` remedy, on its own line.
    let (failure, _) = fixture.fails(&["edit", "shop", "--run-as", "root", "--notes", ""]);
    let text = failure.to_string();
    assert!(
        text.ends_with(&format!("\n  to clear it: --unset notes{ROOT_REMEDY}")),
        "{text}"
    );
    // Another refusal on a root site gets no root remedy.
    fixture.ok(&["edit", "blog", "--run-as", "root", "--allow-root"]);
    let (failure, _) = fixture.fails(&["edit", "blog", "--timeout", "0"]);
    assert!(!failure.to_string().contains("--allow-root"), "{failure}");
    // Turning it off under a root identity says what that flag did.
    let (failure, _) = fixture.fails(&["edit", "blog", "--unset", "allow-root"]);
    assert!(
        failure.to_string().ends_with(
            "\n  --run-as/--owner name root, which needs --allow-root: keep it \
             (drop --unset allow-root), or name another user there first"
        ),
        "{failure}"
    );
}

#[test]
fn keyword_case_variants_are_refused_not_saved_as_users() {
    let fixture = Fixture::with(&sample());
    for (flag, value) in [
        ("--run-as", "Auto"),
        ("--run-as", "LOGIN"),
        ("--owner", "Auto"),
        ("--owner", "AUTO:www-data"),
    ] {
        let (failure, _) = fixture.fails(&["edit", "shop", flag, value]);
        assert_eq!(locations(&failure), [flag], "{flag} {value}");
        assert!(
            failure.to_string().contains("in any letter case"),
            "{failure}"
        );
    }
}

#[test]
fn owner_takes_auto_a_user_or_user_and_group() {
    let fixture = Fixture::with(&sample());
    for value in ["auto", "ftpuser", "ftpuser:www-data"] {
        fixture.ok(&["edit", "shop", "--owner", value]);
        assert_eq!(
            fixture.site("shop").owner,
            Some(Owner::from(value)),
            "{value}"
        );
    }
    for refused in [
        "auto:www-data",
        "auto:",
        "a:b:c",
        ":",
        "ftp:root",
        "root",
        "x:",
    ] {
        let (failure, _) = fixture.fails(&["edit", "shop", "--owner", refused]);
        assert_eq!(locations(&failure), ["--owner"], "{refused:?}");
    }
    fixture.ok(&["edit", "shop", "--owner", "ftp:root", "--allow-root"]);
}

#[test]
fn ownership_saves_every_policy_and_unset_restores_the_default() {
    let fixture = Fixture::with(&sample());
    for policy in OwnershipPolicy::ALL {
        let recorder = fixture.ok(&["edit", "blog", "--ownership", policy.as_str()]);
        assert_eq!(fixture.site("blog").ownership, Some(policy));
        assert_eq!(recorder.out, "Updated site 'blog': ownership.\n");
    }
    fixture.ok(&["edit", "blog", "--unset", "ownership"]);
    assert_eq!(fixture.site("blog").ownership, None);
}

// ── edit ─────────────────────────────────────────────────────────────────

#[test]
fn edit_changes_sets_and_unsets_and_reports_what_changed() {
    let fixture = Fixture::with(&sample());
    fixture.ok(&[
        "edit",
        "shop",
        "--url",
        "https://a.example",
        "--name",
        "Shop",
    ]);
    let recorder = fixture.ok(&[
        "edit",
        "shop",
        "--server",
        "us-1",
        "--unset",
        "url,name",
        "--timeout",
        "30",
    ]);
    assert_eq!(
        recorder.out,
        "Updated site 'shop': server, url, timeout, name.\n"
    );
    let site = fixture.site("shop");
    assert_eq!(site.server, "us-1", "moved to another server");
    assert_eq!((site.url, site.name), (None, None));
    assert_eq!(site.command_timeout_secs, Some(30));
    assert_eq!(site.path, "/var/www/shop", "untouched settings stay");
}

#[test]
fn edit_without_a_real_change_writes_nothing() {
    let fixture = Fixture::with(&sample());
    let before = fixture.bytes();
    let recorder = fixture.ok(&[
        "edit",
        "shop",
        "--path",
        "/var/www/shop",
        "--unset",
        "url",
        "--group",
        "qa",
    ]);
    assert_eq!(recorder.out, "Site 'shop' is unchanged.\n");
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn edit_joins_groups_and_reports_them() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["edit", "blog", "--group", "qa", "--notes", "n"]);
    assert_eq!(
        recorder.out,
        "Updated site 'blog': notes, groups.\nIt joined 1 group: 'qa'.\n"
    );
    let recorder = fixture.ok(&["edit", "blog", "--group", "empty"]);
    assert_eq!(
        recorder.out,
        "Updated site 'blog': groups.\nIt joined 1 group: 'empty'.\n"
    );
    let inventory = fixture.load();
    let groups: Vec<&str> = inventory.groups_of("blog").map(|g| g.id.as_str()).collect();
    assert_eq!(groups, ["qa", "empty"]);
}

#[test]
fn edit_with_an_unknown_group_changes_nothing_else() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "blog", "--notes", "n", "--group", "nope"]);
    assert!(
        failure
            .to_string()
            .starts_with("group 'nope' does not exist"),
        "{failure}"
    );
}

#[test]
fn edit_refuses_an_unknown_site() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "nope", "--notes", "n"]);
    assert_eq!(
        failure.to_string(),
        "site 'nope' does not exist\n  'apvm site list' shows the sites"
    );
    let empty = Fixture::empty();
    empty.fails(&["edit", "nope", "--notes", "n"]);
    assert!(!empty.path.exists());
}

#[test]
fn edit_refuses_invalid_input_naming_the_flags() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&[
        "edit",
        "shop",
        "--server",
        "nope",
        "--tmp-dir",
        "/tmp/a b",
        "--plugins-dir",
        "/x/../y",
    ]);
    assert_eq!(
        locations(&failure),
        ["--server", "--plugins-dir", "--tmp-dir"]
    );
}

// ── list / show ──────────────────────────────────────────────────────────

#[test]
fn list_and_show_read_without_creating_the_file() {
    let fixture = Fixture::empty();
    assert!(fixture.ok(&["list"]).out.starts_with("No sites yet."));
    let (failure, recorder) = fixture.fails(&["show", "shop", "--json"]);
    assert_eq!(failure.code(), "invalid_input");
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(doc["error"]["message"], "site 'shop' does not exist");
    let (_, recorder) = fixture.fails(&["list", "--server", "eu-1", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(doc["error"]["message"], "server 'eu-1' does not exist");
    assert!(!fixture.path.exists());
}

#[test]
fn list_filters_and_renders() {
    let fixture = Fixture::with(&sample());
    let all = fixture.ok(&["list"]).out;
    assert_eq!(all.lines().count(), 4, "{all}");
    let out = fixture
        .ok(&["list", "--server", "eu-1", "--group", "qa", "--json"])
        .out;
    let doc: serde_json::Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(doc["sites"][0]["id"], "shop");
    assert_eq!(doc["sites"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        fixture.ok(&["list", "--group", "empty"]).out,
        "No site matches the filters.\n"
    );
}

#[test]
fn show_renders_the_site() {
    let fixture = Fixture::with(&sample());
    let text = fixture.ok(&["show", "shop"]).out;
    assert!(
        text.starts_with("Site 'shop'\n  server  eu-1 (203.0.113.10)\n"),
        "{text}"
    );
    assert!(text.contains("\n  groups  qa\n"), "{text}");
    let out = fixture.ok(&["show", "api", "--json"]).out;
    let doc: serde_json::Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(doc["site"]["groups"], serde_json::json!(["qa"]));
    assert_eq!(doc["effective"]["server"]["host"], "198.51.100.7");
}

#[test]
fn an_invalid_file_fails_every_command_and_is_never_rewritten() {
    let fixture = Fixture::with_text(
        r#"{"version": 1, "servers": [{"id": "a", "host": "h"}],
            "sites": [{"id": "s", "server": "b", "path": "/p"}]}"#,
    );
    for args in [
        &["list"][..],
        &["show", "s"],
        &["add", "t", "--server", "a", "--path", "/q"],
        &["edit", "s", "--notes", "n"],
        &["remove", "s", "-y"],
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "inventory_invalid", "{args:?}");
    }
    let (_, recorder) = fixture.fails(&["list", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(doc["error"]["problems"][0]["location"], "sites[0].server");
}

#[test]
fn a_newer_inventory_is_shown_with_a_warning_but_never_changed() {
    let fixture = Fixture::with_text(NEWER);
    let recorder = fixture.ok(&["list"]);
    assert!(recorder.out.contains("s   a"), "{}", recorder.out);
    assert!(recorder.err.contains("read-only"), "{}", recorder.err);
    assert!(fixture.ok(&["show", "s"]).out.starts_with("Site 's'"));
    for args in [
        &["add", "t", "--server", "a", "--path", "/q"][..],
        &["edit", "s", "--notes", "n"],
        &["edit", "s", "--notes", ""],
        &["remove", "s", "-y"],
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "inventory_read_only", "{args:?}");
        assert!(!failure.to_string().contains("--unset"), "{args:?}");
    }
    let (result, recorder) = fixture.run(&["remove", "s"], Some(true));
    assert_eq!(result.map_err(|f| f.code()), Err("inventory_read_only"));
    assert!(recorder.asked.is_empty(), "refused before asking");
    assert_eq!(fixture.bytes(), NEWER.as_bytes());
}

// ── remove ───────────────────────────────────────────────────────────────

#[test]
fn remove_with_yes_removes_the_site_and_its_memberships() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["remove", "shop", "-y"]);
    assert_eq!(
        recorder.out,
        "Removed site 'shop'.\nIt was removed from 1 group: 'qa'.\n"
    );
    assert!(recorder.asked.is_empty());
    let inventory = fixture.load();
    assert!(inventory.site("shop").is_none());
    assert_eq!(
        inventory.group("qa").map(|g| g.sites.clone()),
        Some(owned(&["api"]))
    );
    assert!(inventory.server("eu-1").is_some(), "the server stays");
}

#[test]
fn remove_asks_and_a_no_changes_nothing() {
    let fixture = Fixture::with(&sample());
    let before = fixture.bytes();
    let (result, recorder) = fixture.run(&["remove", "shop"], Some(false));
    assert!(result.is_ok());
    assert_eq!(
        recorder.asked,
        ["Remove site 'shop' from the inventory (it also leaves 1 group: 'qa')? [y/N]"]
    );
    assert_eq!(recorder.out, "Aborted. Nothing changed.\n");
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn remove_asks_and_a_yes_removes() {
    let fixture = Fixture::with(&sample());
    let (result, recorder) = fixture.run(&["remove", "blog"], Some(true));
    assert!(result.is_ok());
    assert_eq!(
        recorder.asked,
        ["Remove site 'blog' from the inventory? [y/N]"]
    );
    assert_eq!(recorder.out, "Removed site 'blog'.\n");
    assert!(fixture.load().site("blog").is_none());
}

#[test]
fn remove_without_a_terminal_needs_yes() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["remove", "blog"]);
    assert_eq!(failure.code(), "confirmation_required");
    assert_eq!(
        failure.to_string(),
        "not removing site 'blog': it needs a confirmation, and stdin is not a terminal; \
         pass -y (--yes) to remove it without asking"
    );
}

#[test]
fn remove_refuses_an_unknown_site_before_asking() {
    let fixture = Fixture::with(&sample());
    let (result, recorder) = fixture.run(&["remove", "nope"], Some(true));
    assert_eq!(result.map_err(|f| f.code()), Err("invalid_input"));
    assert!(recorder.asked.is_empty());
}

/// Remove `blog` (asking) while another process applies `edit` during the
/// question; the result and the inventory after.
fn remove_blog_while(
    edit: impl Fn(&mut Inventory) -> Result<(), apvm_sites::Error>,
) -> (Result<(), Failure>, Inventory) {
    let fixture = Fixture::with(&sample());
    let command = parse(&["remove", "blog"]);
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
fn remove_refuses_when_the_site_changed_while_confirming() {
    // The question named the site; another process repoints it to another
    // WordPress meanwhile: what was confirmed is not what would go.
    let (result, inventory) = remove_blog_while(|inventory| {
        let mut site = inventory.site("blog").cloned().expect("known");
        site.path = "/var/www/other".into();
        inventory.update_site(site).map(drop)
    });
    let failure = result.expect_err("refused");
    assert_eq!(failure.code(), "inventory_changed");
    assert_eq!(
        failure.to_string(),
        "site 'blog' or its groups changed while you were confirming; nothing was removed: \
         run the command again"
    );
    assert!(inventory.site("blog").is_some(), "nothing was removed");
}

#[test]
fn remove_refuses_when_its_groups_changed_while_confirming() {
    let (result, inventory) =
        remove_blog_while(|inventory| inventory.add_group(Group::new("late", owned(&["blog"]))));
    assert_eq!(result.map_err(|f| f.code()), Err("inventory_changed"));
    assert!(inventory.site("blog").is_some(), "nothing was removed");
}

#[test]
fn remove_ignores_changes_elsewhere_while_confirming() {
    let (result, inventory) = remove_blog_while(|inventory| {
        inventory.add_site(Site::new("late", "eu-1", "/var/www/late"))
    });
    assert!(result.is_ok(), "{result:?}");
    assert!(inventory.site("blog").is_none());
    assert!(inventory.site("late").is_some(), "the other edit is kept");
}

#[cfg(unix)]
#[test]
fn every_write_leaves_a_private_file() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::empty();
    io::update(&fixture.path, |inventory| {
        inventory.add_server(Server::new("a", "h"))
    })
    .expect("seed");
    fixture.ok(&["add", "s", "--server", "a", "--path", "/p"]);
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
// with its `--unset`, and get the `--unset` remedy when empty.

/// The `apvm site <command>` definition.
fn command_def(command: &str) -> clap::Command {
    let cli = crate::Cli::command();
    let site = cli.find_subcommand("site").expect("site").clone();
    site.find_subcommand(command).expect("subcommand").clone()
}

/// Flags that are not settings of the site record (`--group` joins
/// groups; `--verbose` is global).
const NOT_SETTINGS: [&str; 5] = ["unset", "json", "yes", "group", "verbose"];

/// The flags that a site must have, so `--unset` cannot clear them.
const REQUIRED: [&str; 2] = ["server", "path"];

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

/// The `--unset` values clap accepts for `site edit`.
fn unset_values() -> Vec<String> {
    let def = command_def("edit");
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

/// A valid sample value for a value flag, unlike any value of [`sample`]
/// and [`everything_set`].
fn sample_value(flag: &str) -> &'static str {
    match flag {
        "server" => "us-1",
        "path" => "/srv/sample",
        "url" => "https://sample.example.com",
        "wp-cli" => "/opt/sample/wp",
        "run-as" => "sampleuser",
        "owner" => "sampleowner:samplegroup",
        "ownership" => "off",
        "plugins-dir" => "/srv/sample/plugins",
        "tmp-dir" => "/srv/sample-tmp",
        "timeout" => "4321",
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

/// Whether `json` holds the value `flag` was given: its sample (as a
/// string, a number, or the tokens of `--wp-cli`), or for a switch `true`
/// in the field of the same name.
fn holds(json: &serde_json::Value, flag: &str, switch: bool) -> bool {
    if switch {
        return json[flag.replace('-', "_")] == serde_json::json!(true);
    }
    let sample = sample_value(flag);
    match json {
        serde_json::Value::Object(map) => map.values().any(|value| holds(value, flag, false)),
        serde_json::Value::Array(items) => {
            let tokens: Vec<&str> = items.iter().filter_map(serde_json::Value::as_str).collect();
            tokens.join(" ") == sample
        }
        serde_json::Value::String(text) => text == sample,
        serde_json::Value::Number(number) => number.to_string() == sample,
        _ => false,
    }
}

/// `args` as `&str`s.
fn strs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// The site `id` in the fixture's file, as JSON.
fn site_json(fixture: &Fixture, id: &str) -> serde_json::Value {
    serde_json::to_value(fixture.site(id)).expect("serializes")
}

#[test]
fn every_flag_of_add_saves_its_value() {
    let flags = setting_flags("add");
    assert!(flags.len() >= 13, "{flags:?}");
    for (flag, switch) in flags {
        if REQUIRED.contains(&flag.as_str()) {
            continue;
        }
        let fixture = Fixture::with(&sample());
        let mut args = owned(&["add", "x", "--server", "eu-1", "--path", "/p"]);
        args.extend(flag_args(&flag, switch));
        let recorder = fixture.ok(&strs(&args));
        assert_eq!(recorder.out, "Added site 'x'.\n", "--{flag}");
        assert_eq!(recorder.err, "", "--{flag}: no warning");
        assert!(
            holds(&site_json(&fixture, "x"), &flag, switch),
            "--{flag} was not saved"
        );
    }
}

#[test]
fn every_flag_of_edit_alone_is_a_saved_and_reported_change() {
    let flags = setting_flags("edit");
    assert!(flags.len() >= 13, "{flags:?}");
    for (flag, switch) in flags {
        let fixture = Fixture::with(&sample());
        let mut args = owned(&["edit", "shop"]);
        args.extend(flag_args(&flag, switch));
        let recorder = fixture.ok(&strs(&args));
        assert_eq!(
            recorder.out,
            format!("Updated site 'shop': {flag}.\n"),
            "--{flag}"
        );
        assert_eq!(recorder.err, "", "--{flag}: no warning");
        assert!(
            holds(&site_json(&fixture, "shop"), &flag, switch),
            "--{flag} was not saved"
        );
    }
}

#[test]
fn unset_offers_exactly_the_optional_settings() {
    let mut settable: Vec<String> = setting_flags("edit")
        .into_iter()
        .map(|(flag, _)| flag)
        .filter(|flag| !REQUIRED.contains(&flag.as_str()))
        .collect();
    let mut unset = unset_values();
    settable.sort();
    unset.sort();
    assert_eq!(unset, settable);
}

/// The sample inventory with every optional setting of `shop` set.
fn everything_set() -> Inventory {
    let mut inventory = sample();
    let mut shop = inventory.site("shop").cloned().expect("known");
    shop.name = Some("N".into());
    shop.url = Some("https://shop.example.com".into());
    shop.wp_cli = Some(owned(&["/usr/bin/wp"]));
    shop.run_as = Some(RunAs::Login);
    shop.owner = Some(Owner::from("ftp:www-data"));
    shop.ownership = Some(OwnershipPolicy::Check);
    shop.plugins_dir = Some("/var/www/shop/app/plugins".into());
    shop.allow_root = Some(true);
    shop.remote_tmp_dir = Some("/var/tmp".into());
    shop.command_timeout_secs = Some(60);
    shop.notes = Some("n".into());
    inventory.update_site(shop).expect("valid site");
    inventory
}

#[test]
fn every_unset_value_alone_clears_its_setting_only() {
    for field in unset_values() {
        let fixture = Fixture::with(&everything_set());
        let before = fixture.site("shop");
        let recorder = fixture.ok(&["edit", "shop", "--unset", &field]);
        assert_eq!(
            recorder.out,
            format!("Updated site 'shop': {field}.\n"),
            "{field}"
        );
        let after = fixture.site("shop");
        let changed = field_table::changed(&before, &after);
        assert_eq!(changed.len(), 1, "{field}: {changed:?}");
        assert_eq!(after.get(changed[0]), None, "{field} is cleared");
    }
}

#[test]
fn setting_and_unsetting_one_field_is_refused_for_every_field() {
    for (flag, switch) in setting_flags("edit") {
        if REQUIRED.contains(&flag.as_str()) {
            continue;
        }
        let fixture = Fixture::with(&sample());
        let mut args = owned(&["edit", "shop"]);
        args.extend(flag_args(&flag, switch));
        args.extend(["--unset".to_string(), flag.clone()]);
        let (failure, _) = fixture.fails(&strs(&args));
        assert_eq!(
            failure.to_string(),
            format!("--{flag} and --unset {flag} contradict each other: pass only one"),
            "{args:?}"
        );
        assert_eq!(failure.code(), "invalid_input");
    }
}

#[test]
fn every_text_flag_refuses_an_empty_value_naming_the_remedy() {
    for (flag, switch) in setting_flags("edit") {
        if switch || ["ownership", "timeout"].contains(&flag.as_str()) {
            continue;
        }
        let fixture = Fixture::with(&sample());
        let long = format!("--{flag}");
        let (failure, _) = fixture.fails(&["edit", "shop", &long, ""]);
        assert_eq!(failure.code(), "invalid_input", "{long}");
        assert_eq!(locations(&failure), [long.as_str()], "{long}");
        let remedy = format!("\n  to clear it: --unset {flag}");
        let required = REQUIRED.contains(&flag.as_str());
        assert_eq!(
            failure.to_string().ends_with(&remedy),
            !required,
            "{long}: {failure}"
        );
    }
}

#[test]
fn relative_remote_paths_are_refused_not_warned_about() {
    let fixture = Fixture::with(&sample());
    for flag in ["--path", "--plugins-dir", "--tmp-dir"] {
        let (failure, recorder) = fixture.fails(&["edit", "shop", flag, "rel/dir"]);
        assert_eq!(locations(&failure), [flag], "{flag}");
        assert_eq!(recorder.err, "", "{flag}: an error, not a warning");
    }
}
