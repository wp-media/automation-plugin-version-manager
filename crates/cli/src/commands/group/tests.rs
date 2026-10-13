//! Unit tests of `apvm group`: argument parsing, the pure helpers, and
//! whole commands run against temporary inventory files through a
//! recording [`Console`].

use std::path::PathBuf;

use apvm_sites::{Group, Inventory, Server, Site};
use clap::{CommandFactory, Parser};
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

    /// The group `id` in the file.
    fn group(&self, id: &str) -> Group {
        self.load().group(id).cloned().expect("the group exists")
    }

    /// The file's bytes (empty when it is missing).
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap_or_default()
    }

    /// Run `apvm group <args>`, answering any question with `answer`.
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
            panic!("`group {}` failed: {failure}", args.join(" "));
        }
        recorder
    }

    /// [`Fixture::run`], expecting a failure; the file must be unchanged.
    #[track_caller]
    fn fails(&self, args: &[&str]) -> (Failure, Recorder) {
        let before = self.bytes();
        let (result, recorder) = self.run(args, None);
        let Err(failure) = result else {
            panic!("`group {}` must fail", args.join(" "));
        };
        assert_eq!(self.bytes(), before, "a failed command wrote the file");
        (failure, recorder)
    }
}

/// Parse `apvm group <args>`.
#[track_caller]
fn parse(args: &[&str]) -> GroupArgs {
    let argv = ["apvm", "group"].iter().chain(args).copied();
    match crate::Cli::try_parse_from(argv).map(|cli| cli.command) {
        Ok(crate::Commands::Group(command)) => command,
        Ok(other) => panic!("parsed as {other:?}"),
        Err(error) => panic!("`group {}` does not parse: {error}", args.join(" ")),
    }
}

/// The clap error kind of `apvm group <args>`, when it does not parse.
fn parse_error(args: &[&str]) -> Option<clap::error::ErrorKind> {
    let argv = ["apvm", "group"].iter().chain(args).copied();
    crate::Cli::try_parse_from(argv)
        .err()
        .map(|error| error.kind())
}

/// The `apvm group edit` arguments of `args`.
#[track_caller]
fn edit_args(args: &[&str]) -> EditArgs {
    let mut argv = vec!["edit"];
    argv.extend(args);
    match parse(&argv).action {
        GroupAction::Edit(args) => args,
        other => panic!("not an edit: {other:?}"),
    }
}

/// Server `eu-1` with sites `shop`, `blog` and `api`; group `qa` lists
/// `shop` and `api`, group `empty` lists none.
fn sample() -> Inventory {
    let mut inventory = Inventory::new();
    inventory
        .add_server(Server::new("eu-1", "203.0.113.10"))
        .expect("valid server");
    for id in ["shop", "blog", "api"] {
        inventory
            .add_site(Site::new(id, "eu-1", format!("/var/www/{id}")))
            .expect("valid site");
    }
    inventory
        .add_group(Group::new("qa", owned(&["shop", "api"])))
        .expect("valid group");
    inventory
        .add_group(Group::new("empty", Vec::new()))
        .expect("valid group");
    inventory
}

/// `ids` as owned strings.
fn owned(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
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

/// The message of an unknown site, with its remedy.
fn unknown_site(id: &str) -> String {
    format!("site '{id}' does not exist\n  'apvm site list' shows the sites")
}

/// The message of an unknown group, with its remedy.
fn unknown_group(id: &str) -> String {
    format!("group '{id}' does not exist\n  'apvm group list' shows the groups")
}

/// A newer, read-only inventory with site `s` in group `g`.
const NEWER: &str = r#"{"version": 9, "servers": [{"id": "a", "host": "h"}],
    "sites": [{"id": "s", "server": "a", "path": "/var/www/s"}],
    "groups": [{"id": "g", "sites": ["s"]}]}"#;

// ── parsing ──────────────────────────────────────────────────────────────

#[test]
fn add_takes_sites_as_repeats_and_comma_lists() {
    let GroupAction::Add(args) = parse(&["add", "qa", "shop,blog", "api", "--notes", "n"]).action
    else {
        panic!("an add");
    };
    assert_eq!(args.sites, ["shop", "blog", "api"]);
    assert_eq!(args.flags.notes.as_deref(), Some("n"));
    let GroupAction::Add(args) = parse(&["add", "qa"]).action else {
        panic!("an add");
    };
    assert!(args.sites.is_empty(), "a group may start empty");
    assert_eq!(
        parse_error(&["add"]),
        Some(clap::error::ErrorKind::MissingRequiredArgument)
    );
}

#[test]
fn edit_needs_at_least_one_change() {
    assert_eq!(
        parse_error(&["edit", "qa"]),
        Some(clap::error::ErrorKind::MissingRequiredArgument)
    );
    for change in [
        &["--add", "shop"][..],
        &["--remove", "shop"],
        &["--notes", "n"],
        &["--unset", "notes"],
    ] {
        let mut args = vec!["edit", "qa"];
        args.extend(change);
        assert_eq!(parse_error(&args), None, "{args:?}");
    }
}

#[test]
fn edit_takes_repeats_and_comma_lists() {
    let args = edit_args(&["qa", "--add", "a,b", "--add", "c", "--remove", "d,e"]);
    assert_eq!(args.add, ["a", "b", "c"]);
    assert_eq!(args.remove, ["d", "e"]);
}

#[test]
fn unset_accepts_notes_only() {
    assert_eq!(edit_args(&["qa", "--unset", "notes"]).unset, [Field::Notes]);
    for refused in ["sites", "id", "name", "nope"] {
        assert_eq!(
            parse_error(&["edit", "qa", "--unset", refused]),
            Some(clap::error::ErrorKind::InvalidValue),
            "{refused}"
        );
    }
}

#[test]
fn only_reads_take_json() {
    assert!(parse(&["list", "--json"]).json());
    assert!(parse(&["show", "qa", "--json"]).json());
    assert!(!parse(&["list"]).json());
    assert!(!parse(&["show", "qa"]).json());
    for args in [
        &["add", "qa", "--json"][..],
        &["edit", "qa", "--notes", "n", "--json"],
        &["remove", "qa", "--json"],
    ] {
        assert_eq!(
            parse_error(args),
            Some(clap::error::ErrorKind::UnknownArgument),
            "{args:?}"
        );
    }
}

/// The long flags of `apvm group <command>`, as clap defines them.
fn long_flags(command: &str) -> Vec<String> {
    let cli = crate::Cli::command();
    let group = cli.find_subcommand("group").expect("group").clone();
    let def = group.find_subcommand(command).expect("subcommand").clone();
    def.get_arguments()
        .filter_map(|arg| arg.get_long().map(String::from))
        .filter(|long| long != "verbose" && long != "help" && long != "version")
        .collect()
}

#[test]
fn the_flags_are_exactly_those_the_commands_handle() {
    // A new flag fails here until it is handled (and documented).
    assert_eq!(long_flags("add"), ["notes"]);
    assert_eq!(long_flags("edit"), ["add", "remove", "notes", "unset"]);
    assert_eq!(long_flags("list"), ["json"]);
    assert_eq!(long_flags("show"), ["json"]);
    assert_eq!(long_flags("remove"), ["yes"]);
}

// ── pure helpers ─────────────────────────────────────────────────────────

#[test]
fn add_builds_the_group_with_each_site_once_in_order() {
    let GroupAction::Add(args) = parse(&["add", "qa", "b,a", "b", "c,a", "--notes", "n"]).action
    else {
        panic!("an add");
    };
    let group = args.to_group();
    assert_eq!(group.id, "qa");
    assert_eq!(group.sites, ["b", "a", "c"]);
    assert_eq!(group.notes.as_deref(), Some("n"));
}

#[test]
fn distinct_keeps_the_first_of_each_id() {
    assert_eq!(
        distinct(&owned(&["a", "b", "a", "c", "b"])),
        ["a", "b", "c"]
    );
    assert!(distinct(&[]).is_empty());
}

/// [`Membership::edit`] of `members` with `add` and `remove`.
fn membership(members: &[&str], add: &[&str], remove: &[&str]) -> Result<Membership, Failure> {
    Membership::edit(&owned(members), &owned(add), &owned(remove))
}

#[test]
fn membership_keeps_the_order_and_appends_new_sites() {
    let members = membership(&["a", "b", "c"], &["e", "d", "e"], &["b"]).expect("valid");
    assert_eq!(
        members,
        Membership {
            sites: owned(&["a", "c", "e", "d"]),
            added: owned(&["e", "d"]),
            removed: owned(&["b"]),
        }
    );
    assert!(members.changed());
}

#[test]
fn membership_ignores_members_added_and_non_members_removed() {
    let members = membership(&["a", "b"], &["b", "a"], &["c"]).expect("valid");
    assert_eq!(members.sites, ["a", "b"]);
    assert!(!members.changed());
    assert_eq!(
        membership(&[], &[], &[]).expect("valid"),
        Membership::default()
    );
}

#[test]
fn membership_can_empty_a_group() {
    let members = membership(&["a", "b"], &[], &["b", "a", "a"]).expect("valid");
    assert!(members.sites.is_empty());
    assert_eq!(members.removed, ["a", "b"], "in the group's order, once");
}

#[test]
fn membership_refuses_a_site_both_added_and_removed() {
    for (add, remove) in [(&["x"][..], &["x"][..]), (&["a", "x"], &["b", "x"])] {
        let failure = membership(&["a"], add, remove).expect_err("contradiction");
        assert_eq!(failure.code(), "invalid_input");
        assert_eq!(
            failure.to_string(),
            "--add 'x' and --remove 'x' contradict each other: pass only one"
        );
    }
}

#[test]
fn ensure_sites_names_the_first_unknown_site() {
    let inventory = sample();
    assert!(ensure_sites(&inventory, &owned(&["shop", "api"])).is_ok());
    assert!(ensure_sites(&inventory, &[]).is_ok());
    let failure = ensure_sites(&inventory, &owned(&["shop", "x", "y"])).expect_err("unknown");
    assert_eq!(failure.to_string(), unknown_site("x"));
}

#[test]
fn reports_name_the_sites_with_counts() {
    let qa = Group::new("qa", owned(&["shop", "api"]));
    let empty = Group::new("empty", Vec::new());
    assert_eq!(
        added_text(&qa),
        "Added group 'qa'.\nIt lists 2 sites: 'shop', 'api'.\n"
    );
    assert_eq!(added_text(&empty), "Added group 'empty'.\n");
    assert_eq!(
        removal_question(&qa),
        "Remove group 'qa' from the inventory (the sites it lists stay: 'shop', 'api')?"
    );
    assert_eq!(
        removal_question(&empty),
        "Remove group 'empty' from the inventory?"
    );
    assert_eq!(
        removed_text(&qa),
        "Removed group 'qa'.\nThe sites it listed stay in the inventory: 'shop', 'api'.\n"
    );
    assert_eq!(removed_text(&empty), "Removed group 'empty'.\n");
}

#[test]
fn edit_reports_name_the_sites_then_the_settings_then_who_moved() {
    let before = Group::new("qa", owned(&["shop"]));
    let after = Group {
        notes: Some("n".into()),
        ..Group::new("qa", owned(&["api", "blog"]))
    };
    let members = Membership {
        sites: after.sites.clone(),
        added: owned(&["api", "blog"]),
        removed: owned(&["shop"]),
    };
    assert_eq!(
        edit_report("qa", &before, &after, &members),
        "Updated group 'qa': sites, notes.\n\
         Added 2 sites to it: 'api', 'blog'.\n\
         Removed 1 site from it: 'shop'.\n"
    );
    let same = Membership {
        sites: before.sites.clone(),
        ..Membership::default()
    };
    assert_eq!(
        edit_report("qa", &before, &before, &same),
        "Group 'qa' is unchanged.\n"
    );
    assert_eq!(sites_line("Added", "to", &[]), "");
}

// ── add ──────────────────────────────────────────────────────────────────

#[test]
fn add_saves_the_group_and_reports_it() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["add", "web", "blog,shop", "blog", "--notes", "front"]);
    assert_eq!(
        recorder.out,
        "Added group 'web'.\nIt lists 2 sites: 'blog', 'shop'.\n"
    );
    assert!(recorder.err.is_empty(), "{}", recorder.err);
    let group = fixture.group("web");
    assert_eq!(group.sites, ["blog", "shop"]);
    assert_eq!(group.notes.as_deref(), Some("front"));
    let ids: Vec<String> = fixture
        .load()
        .groups()
        .iter()
        .map(|g| g.id.clone())
        .collect();
    assert_eq!(ids, ["qa", "empty", "web"], "added last");
}

#[test]
fn add_without_sites_creates_an_empty_group_even_in_a_new_inventory() {
    let fixture = Fixture::empty();
    assert_eq!(fixture.ok(&["add", "later"]).out, "Added group 'later'.\n");
    assert_eq!(fixture.group("later"), Group::new("later", Vec::new()));
}

#[test]
fn add_refuses_an_unknown_site_and_writes_nothing() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["add", "web", "shop", "nope", "other"]);
    assert_eq!(failure.code(), "invalid_input");
    assert_eq!(failure.to_string(), unknown_site("nope"));
    assert!(fixture.load().group("web").is_none());
    // On a missing inventory too, and the file is not created.
    let empty = Fixture::empty();
    empty.fails(&["add", "web", "shop"]);
    assert!(!empty.path.exists());
}

#[test]
fn add_refuses_a_taken_id() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["add", "qa", "blog"]);
    assert_eq!(
        failure.to_string(),
        "group 'qa' already exists\n  change it with 'apvm group edit', or choose another id"
    );
}

#[test]
fn add_refuses_invalid_input_naming_the_flags() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["add", "QA", "--notes", "a\u{7}b"]);
    assert_eq!(failure.code(), "invalid_input");
    assert_eq!(locations(&failure), ["<ID>", "--notes"]);
    assert!(!failure.to_string().contains('\u{7}'), "escaped: {failure}");
    let (failure, _) = fixture.fails(&["add", "web", "--notes", ""]);
    assert_eq!(locations(&failure), ["--notes"]);
    assert!(
        !failure.to_string().contains("--unset"),
        "add has nothing to unset: {failure}"
    );
}

#[test]
fn an_unknown_site_with_control_characters_is_escaped() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["add", "web", "x\u{1b}[2J\u{202e}"]);
    let text = failure.to_string();
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{202e}'),
        "{text:?}"
    );
}

#[test]
fn add_reports_the_group_itself_before_an_unknown_site() {
    // As `site add` does: the record's own refusal first, then references.
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["add", "qa", "nope"]);
    assert_eq!(
        failure.to_string(),
        "group 'qa' already exists\n  change it with 'apvm group edit', or choose another id"
    );
    let (failure, _) = fixture.fails(&["add", "QA", "nope"]);
    assert_eq!(locations(&failure), ["<ID>"]);
    let (failure, _) = fixture.fails(&["add", "web", "nope", "--notes", ""]);
    assert_eq!(locations(&failure), ["--notes"]);
    let newer = Fixture::with_text(NEWER);
    let (failure, _) = newer.fails(&["add", "h", "nope"]);
    assert_eq!(failure.code(), "inventory_read_only");
}

// ── edit ─────────────────────────────────────────────────────────────────

#[test]
fn edit_adds_and_removes_sites_and_reports_them() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["edit", "qa", "--add", "blog", "--remove", "shop"]);
    assert_eq!(
        recorder.out,
        "Updated group 'qa': sites.\n\
         Added 1 site to it: 'blog'.\n\
         Removed 1 site from it: 'shop'.\n"
    );
    assert_eq!(fixture.group("qa").sites, ["api", "blog"]);
    let inventory = fixture.load();
    assert!(inventory.site("shop").is_some(), "the site stays");
}

#[test]
fn edit_sets_and_clears_the_notes() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["edit", "qa", "--notes", "release candidates"]);
    assert_eq!(recorder.out, "Updated group 'qa': notes.\n");
    assert_eq!(
        fixture.group("qa").notes.as_deref(),
        Some("release candidates")
    );
    let recorder = fixture.ok(&["edit", "qa", "--unset", "notes", "--add", "blog"]);
    assert_eq!(
        recorder.out,
        "Updated group 'qa': sites, notes.\nAdded 1 site to it: 'blog'.\n"
    );
    let group = fixture.group("qa");
    assert_eq!(group.notes, None);
    assert_eq!(group.sites, ["shop", "api", "blog"]);
}

#[test]
fn edit_can_empty_a_group_and_fill_it_again() {
    let fixture = Fixture::with(&sample());
    fixture.ok(&["edit", "qa", "--remove", "api,shop"]);
    assert!(fixture.group("qa").sites.is_empty());
    fixture.ok(&["edit", "qa", "--add", "api", "--add", "shop"]);
    assert_eq!(fixture.group("qa").sites, ["api", "shop"]);
}

#[test]
fn edit_without_a_real_change_writes_nothing() {
    let fixture = Fixture::with(&sample());
    let before = fixture.bytes();
    let recorder = fixture.ok(&[
        "edit", "qa", "--add", "shop", "--remove", "blog", "--unset", "notes",
    ]);
    assert_eq!(recorder.out, "Group 'qa' is unchanged.\n");
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn edit_setting_the_notes_they_already_have_writes_nothing() {
    let fixture = Fixture::with(&sample());
    fixture.ok(&["edit", "qa", "--notes", "rc"]);
    let before = fixture.bytes();
    for args in [
        &["edit", "qa", "--notes", "rc"][..],
        &["edit", "qa", "--notes", "rc", "--add", "api"],
        &["edit", "empty", "--unset", "notes"],
    ] {
        let recorder = fixture.ok(args);
        assert!(
            recorder.out.ends_with("is unchanged.\n"),
            "{args:?}: {}",
            recorder.out
        );
        assert_eq!(fixture.bytes(), before, "{args:?} rewrote the file");
    }
}

#[test]
fn group_ids_are_unique_among_groups_only() {
    // Ids are unique per kind (plan §7): a group may share a site's or a
    // server's id, and `--add` still names the site.
    let fixture = Fixture::with(&sample());
    fixture.ok(&["add", "shop", "shop"]);
    fixture.ok(&["add", "eu-1"]);
    fixture.ok(&["edit", "eu-1", "--add", "shop"]);
    let inventory = fixture.load();
    assert_eq!(fixture.group("shop").sites, ["shop"]);
    assert_eq!(fixture.group("eu-1").sites, ["shop"]);
    assert!(inventory.site("shop").is_some() && inventory.server("eu-1").is_some());
}

#[test]
fn a_hand_edited_group_listing_a_site_twice_is_refused_and_kept() {
    let fixture = Fixture::with_text(
        r#"{"version": 1, "servers": [{"id": "a", "host": "h"}],
        "sites": [{"id": "s", "server": "a", "path": "/var/www/s"}],
        "groups": [{"id": "g", "sites": ["s", "s"]}]}"#,
    );
    for args in [&["show", "g"][..], &["edit", "g", "--remove", "s"]] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "inventory_invalid", "{args:?}");
        assert_eq!(locations(&failure), ["groups[0].sites[1]"], "{args:?}");
    }
}

#[test]
fn edit_refuses_an_unknown_group_or_site() {
    let fixture = Fixture::with(&sample());
    for (args, message) in [
        (
            &["edit", "nope", "--add", "shop"][..],
            unknown_group("nope"),
        ),
        (&["edit", "nope", "--add", "x"], unknown_group("nope")),
        (&["edit", "qa", "--add", "blog,x"], unknown_site("x")),
        (&["edit", "qa", "--remove", "x"], unknown_site("x")),
        (
            &["edit", "qa", "--notes", "n", "--add", "x", "--remove", "y"],
            unknown_site("x"),
        ),
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.to_string(), message, "{args:?}");
        assert_eq!(failure.code(), "invalid_input");
    }
    let empty = Fixture::empty();
    empty.fails(&["edit", "qa", "--notes", "n"]);
    assert!(!empty.path.exists());
}

#[test]
fn edit_refuses_contradictions() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "qa", "--add", "blog", "--remove", "blog"]);
    assert_eq!(
        failure.to_string(),
        "--add 'blog' and --remove 'blog' contradict each other: pass only one"
    );
    let (failure, _) = fixture.fails(&["edit", "qa", "--notes", "n", "--unset", "notes"]);
    assert_eq!(
        failure.to_string(),
        "--notes and --unset notes contradict each other: pass only one"
    );
}

#[test]
fn edit_reports_the_settings_before_an_unknown_site() {
    // As `site edit` does: the record's own refusal first, then references.
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "qa", "--notes", "", "--add", "nope"]);
    assert_eq!(locations(&failure), ["--notes"]);
    assert!(failure.to_string().ends_with("--unset notes"), "{failure}");
    let newer = Fixture::with_text(NEWER);
    for args in [
        &["edit", "g", "--add", "nope"][..],
        &["edit", "g", "--remove", "nope"],
        &["edit", "g", "--add", "s", "--remove", "s"],
    ] {
        let (failure, _) = newer.fails(args);
        assert_eq!(failure.code(), "inventory_read_only", "{args:?}");
    }
}

#[test]
fn edit_refuses_an_empty_note_naming_the_unset_remedy() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["edit", "qa", "--notes", ""]);
    assert_eq!(locations(&failure), ["--notes"]);
    assert!(
        failure
            .to_string()
            .ends_with("\n  to clear it: --unset notes"),
        "{failure}"
    );
}

// ── list / show ──────────────────────────────────────────────────────────

#[test]
fn list_and_show_read_without_creating_the_file() {
    let fixture = Fixture::empty();
    assert!(fixture.ok(&["list"]).out.starts_with("No groups yet."));
    let (failure, recorder) = fixture.fails(&["show", "qa", "--json"]);
    assert_eq!(failure.code(), "invalid_input");
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(doc["error"]["message"], "group 'qa' does not exist");
    assert!(!fixture.path.exists());
}

#[test]
fn list_and_show_render_the_groups() {
    let fixture = Fixture::with(&sample());
    assert_eq!(
        fixture.ok(&["list"]).out,
        "ID     SITES  MEMBERS\nqa     2      shop, api\nempty  0\n"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&fixture.ok(&["list", "--json"]).out).expect("JSON");
    assert_eq!(
        doc["groups"][0]["sites"],
        serde_json::json!(["shop", "api"])
    );
    assert_eq!(
        fixture.ok(&["show", "qa"]).out,
        "Group 'qa'\n  sites  shop, api\n"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&fixture.ok(&["show", "empty", "--json"]).out).expect("JSON");
    assert_eq!(
        doc,
        serde_json::json!({"schema": 1, "group": {"id": "empty", "sites": []}})
    );
    let (failure, _) = fixture.fails(&["show", "nope"]);
    assert_eq!(failure.to_string(), unknown_group("nope"));
}

#[test]
fn an_invalid_file_fails_every_command_and_is_never_rewritten() {
    let fixture =
        Fixture::with_text(r#"{"version": 1, "groups": [{"id": "g", "sites": ["ghost"]}]}"#);
    for args in [
        &["list"][..],
        &["show", "g"],
        &["add", "h"],
        &["edit", "g", "--notes", "n"],
        &["remove", "g", "-y"],
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "inventory_invalid", "{args:?}");
    }
    let (_, recorder) = fixture.fails(&["list", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&recorder.out).expect("JSON error");
    assert_eq!(
        doc["error"]["problems"][0]["location"],
        "groups[0].sites[0]"
    );
}

#[test]
fn a_newer_inventory_is_shown_with_a_warning_but_never_changed() {
    let fixture = Fixture::with_text(NEWER);
    let recorder = fixture.ok(&["list"]);
    assert!(recorder.out.contains("g   1      s"), "{}", recorder.out);
    assert!(recorder.err.contains("read-only"), "{}", recorder.err);
    assert!(fixture.ok(&["show", "g"]).out.starts_with("Group 'g'"));
    for args in [
        &["add", "h"][..],
        &["add", "h", "s"],
        &["edit", "g", "--notes", "n"],
        &["edit", "g", "--notes", ""],
        &["edit", "g", "--remove", "s"],
        &["remove", "g", "-y"],
    ] {
        let (failure, _) = fixture.fails(args);
        assert_eq!(failure.code(), "inventory_read_only", "{args:?}");
        assert!(!failure.to_string().contains("--unset"), "{args:?}");
    }
    let (result, recorder) = fixture.run(&["remove", "g"], Some(true));
    assert_eq!(result.map_err(|f| f.code()), Err("inventory_read_only"));
    assert!(recorder.asked.is_empty(), "refused before asking");
    assert_eq!(fixture.bytes(), NEWER.as_bytes());
}

// ── remove ───────────────────────────────────────────────────────────────

#[test]
fn remove_with_yes_removes_the_group_and_keeps_its_sites() {
    let fixture = Fixture::with(&sample());
    let recorder = fixture.ok(&["remove", "qa", "-y"]);
    assert_eq!(
        recorder.out,
        "Removed group 'qa'.\nThe sites it listed stay in the inventory: 'shop', 'api'.\n"
    );
    assert!(recorder.asked.is_empty());
    let inventory = fixture.load();
    assert!(inventory.group("qa").is_none());
    assert_eq!(inventory.sites().len(), 3, "every site stays");
    assert!(inventory.group("empty").is_some(), "other groups stay");
}

#[test]
fn remove_asks_and_a_no_changes_nothing() {
    let fixture = Fixture::with(&sample());
    let before = fixture.bytes();
    let (result, recorder) = fixture.run(&["remove", "qa"], Some(false));
    assert!(result.is_ok());
    assert_eq!(
        recorder.asked,
        ["Remove group 'qa' from the inventory (the sites it lists stay: 'shop', 'api')? [y/N]"]
    );
    assert_eq!(recorder.out, "Aborted. Nothing changed.\n");
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn remove_asks_and_a_yes_removes() {
    let fixture = Fixture::with(&sample());
    let (result, recorder) = fixture.run(&["remove", "empty"], Some(true));
    assert!(result.is_ok());
    assert_eq!(
        recorder.asked,
        ["Remove group 'empty' from the inventory? [y/N]"]
    );
    assert_eq!(recorder.out, "Removed group 'empty'.\n");
    assert!(fixture.load().group("empty").is_none());
}

#[test]
fn remove_without_a_terminal_needs_yes() {
    let fixture = Fixture::with(&sample());
    let (failure, _) = fixture.fails(&["remove", "qa"]);
    assert_eq!(failure.code(), "confirmation_required");
    assert_eq!(
        failure.to_string(),
        "not removing group 'qa': it needs a confirmation, and stdin is not a terminal; \
         pass -y (--yes) to remove it without asking"
    );
}

#[test]
fn remove_refuses_an_unknown_group_before_asking() {
    let fixture = Fixture::with(&sample());
    let (result, recorder) = fixture.run(&["remove", "nope"], Some(true));
    let failure = result.expect_err("unknown");
    assert_eq!(failure.to_string(), unknown_group("nope"));
    assert!(recorder.asked.is_empty());
}

/// Remove `qa` (asking) while another process applies `edit` during the
/// question; the result and the inventory after.
fn remove_qa_while(
    edit: impl Fn(&mut Inventory) -> Result<(), apvm_sites::Error>,
) -> (Result<(), Failure>, Inventory) {
    let fixture = Fixture::with(&sample());
    let command = parse(&["remove", "qa"]);
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
fn remove_refuses_when_the_members_changed_while_confirming() {
    let (result, inventory) = remove_qa_while(|inventory| {
        inventory
            .update_group(Group::new("qa", owned(&["shop", "api", "blog"])))
            .map(drop)
    });
    let failure = result.expect_err("refused");
    assert_eq!(failure.code(), "inventory_changed");
    assert_eq!(
        failure.to_string(),
        "group 'qa' changed while you were confirming; nothing was removed: run the command again"
    );
    assert!(inventory.group("qa").is_some(), "nothing was removed");
}

#[test]
fn remove_refuses_when_the_notes_changed_while_confirming() {
    let (result, inventory) = remove_qa_while(|inventory| {
        let group = Group {
            notes: Some("keep me".into()),
            ..Group::new("qa", owned(&["shop", "api"]))
        };
        inventory.update_group(group).map(drop)
    });
    assert_eq!(result.map_err(|f| f.code()), Err("inventory_changed"));
    assert!(inventory.group("qa").is_some(), "nothing was removed");
}

#[test]
fn remove_ignores_changes_elsewhere_while_confirming() {
    let (result, inventory) =
        remove_qa_while(|inventory| inventory.add_group(Group::new("late", owned(&["blog"]))));
    assert!(result.is_ok(), "{result:?}");
    assert!(inventory.group("qa").is_none());
    assert!(inventory.group("late").is_some(), "the other edit is kept");
}

// ── site removal ─────────────────────────────────────────────────────────

#[test]
fn a_removed_site_leaves_every_group_and_the_group_commands_see_it() {
    let fixture = Fixture::with(&sample());
    fixture.ok(&["edit", "empty", "--add", "shop"]);
    io::update(&fixture.path, |inventory| inventory.remove_site("shop")).expect("site removal");
    assert_eq!(
        fixture.ok(&["list"]).out,
        "ID     SITES  MEMBERS\nqa     1      api\nempty  0\n"
    );
    let (failure, _) = fixture.fails(&["edit", "qa", "--add", "shop"]);
    assert_eq!(failure.to_string(), unknown_site("shop"));
}

#[cfg(unix)]
#[test]
fn every_write_leaves_a_private_file() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::empty();
    fixture.ok(&["add", "qa"]);
    let mode = std::fs::metadata(&fixture.path)
        .expect("written")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
