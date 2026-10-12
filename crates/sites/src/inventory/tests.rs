//! Tests of [`Inventory`]: parsing, versions, changes and their integrity.

use super::*;
use crate::error::Code;

/// eu-1 (shop, blog), us-1 (api); groups qa (shop, api) and blogs (blog).
fn sample() -> Inventory {
    let mut inventory = Inventory::new();
    for server in [
        Server::new("eu-1", "203.0.113.10"),
        Server::new("us-1", "us"),
    ] {
        inventory.add_server(server).unwrap();
    }
    for site in [
        Site::new("shop", "eu-1", "/var/www/shop"),
        Site::new("blog", "eu-1", "/var/www/blog"),
        Site::new("api", "us-1", "/srv/api"),
    ] {
        inventory.add_site(site).unwrap();
    }
    inventory
        .add_group(Group::new("qa", vec!["shop".into(), "api".into()]))
        .unwrap();
    inventory
        .add_group(Group::new("blogs", vec!["blog".into()]))
        .unwrap();
    inventory
}

/// Ids of records, in order.
fn ids<T: Record>(items: &[T]) -> Vec<&str> {
    items.iter().map(Record::id).collect()
}

/// An inventory with a newer format version, read-only.
fn newer() -> Inventory {
    let text = format!(
        r#"{{"version":{},"servers":[{{"id":"eu-1","host":"h"}}]}}"#,
        INVENTORY_VERSION + 1
    );
    Inventory::from_json(&text).unwrap()
}

/// Assert the inventory is still valid as a whole.
fn assert_valid(inventory: &Inventory) {
    assert_eq!(validate::document(&inventory.doc), []);
}

/// The problems of an error, as `location: message`.
fn texts(error: &Error) -> Vec<String> {
    error.problems().iter().map(Problem::to_string).collect()
}

#[test]
fn a_new_inventory_is_empty_and_current() {
    let inventory = Inventory::new();
    assert_eq!(inventory.version(), INVENTORY_VERSION);
    assert!(!inventory.is_read_only());
    assert!(inventory.servers().is_empty());
    assert!(inventory.sites().is_empty());
    assert!(inventory.groups().is_empty());
    assert!(inventory.defaults().is_empty());
    assert_eq!(Inventory::default(), inventory);
    assert_eq!(
        inventory.to_json().unwrap(),
        "{\n  \"version\": 1,\n  \"servers\": [],\n  \"sites\": [],\n  \"groups\": []\n}\n"
    );
}

#[test]
fn documents_round_trip() {
    let mut inventory = sample();
    let mut site = inventory.site("shop").unwrap().clone();
    site.run_as = Some(RunAs::Login);
    site.owner = Some(Owner::from("ftpuser:www-data"));
    site.wp_cli = Some(vec!["/usr/bin/php8.3".into(), "/opt/wp-cli.phar".into()]);
    inventory.update_site(site).unwrap();
    inventory
        .set_defaults(Defaults {
            ssh: SshSettings {
                user: Some("deploy".into()),
                multiplex: Some(false),
                ..SshSettings::default()
            },
        })
        .unwrap();
    let text = inventory.to_json().unwrap();
    assert!(text.ends_with("}\n"));
    assert_eq!(Inventory::from_json(&text).unwrap(), inventory);
    // Saving again produces the same text.
    assert_eq!(
        Inventory::from_json(&text).unwrap().to_json().unwrap(),
        text
    );
}

#[test]
fn a_byte_order_mark_is_ignored() {
    let inventory = Inventory::from_json("\u{feff}{\"version\": 1}").unwrap();
    assert_eq!(inventory, Inventory::new());
    // Only one, and only first.
    assert!(Inventory::from_json("\u{feff}\u{feff}{\"version\": 1}").is_err());
    assert!(Inventory::from_json(" \u{feff}{\"version\": 1}").is_err());
}

#[test]
fn null_reads_as_unset_for_optional_records() {
    let inventory = Inventory::from_json(
        r#"{"version":1,"defaults":null,"servers":[{"id":"e","host":"h","ssh":null}],
            "sites":null,"groups":null}"#,
    )
    .unwrap();
    assert_eq!(inventory.servers(), [Server::new("e", "h")]);
    assert!(inventory.defaults().is_empty());
    assert!(inventory.sites().is_empty());
}

#[test]
fn parse_errors_carry_the_position() {
    for (text, expected) in [
        ("", "EOF while parsing"),
        ("{", "EOF while parsing"),
        ("[]", "invalid length 0"),
        ("{}", "missing field `version`"),
        (r#"{"version":"1"}"#, "invalid type: string \"1\""),
        (r#"{"version":-1}"#, "invalid value: integer `-1`"),
        (r#"{"version":1,"version":1}"#, "duplicate field `version`"),
        (
            r#"{"version":1,"servers":[{"id":"a"}]}"#,
            "missing field `host`",
        ),
        ("{\"version\":1,\n\"site\":[]}", "unknown field `site`"),
        (r#"{"version":1} trailing"#, "trailing characters"),
    ] {
        let err = Inventory::from_json(text).unwrap_err();
        assert!(
            matches!(err, Error::InventoryJson { .. }),
            "{text:?} → {err:?}"
        );
        assert_eq!(err.code(), Code::InventoryInvalid);
        let message = err.to_string();
        assert!(
            message.starts_with("cannot parse the inventory: "),
            "{message}"
        );
        assert!(message.contains(expected), "{text:?}: {message}");
        assert!(message.contains(" at line "), "{message}");
    }
    let err = Inventory::from_json("{\"version\":1,\n\"site\":[]}").unwrap_err();
    assert!(err.to_string().contains("line 2"), "{err}");
}

#[test]
fn invalid_documents_list_every_problem() {
    let text = r#"{
        "version": 1,
        "servers": [{"id": "eu-1", "host": "h"}],
        "sites": [
            {"id": "shop", "server": "eu-1", "path": "/var/www/shop"},
            {"id": "blog", "server": "eu-2", "path": "var/www/blog"}
        ],
        "groups": [{"id": "qa", "sites": ["shop", "nope"]}]
    }"#;
    let err = Inventory::from_json(text).unwrap_err();
    assert!(matches!(err, Error::InventoryInvalid { .. }), "{err:?}");
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(
        texts(&err),
        [
            "sites[1].server: unknown server 'eu-2'",
            "sites[1].path: must be an absolute path (start with '/')",
            "groups[0].sites[1]: unknown site 'nope'",
        ]
    );
    let err = Inventory::from_json(r#"{"version":0}"#).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid inventory: version: must be at least 1"
    );
}

#[test]
fn records_written_as_arrays_are_refused() {
    // serde would read these by field position, ignoring field names.
    for (text, location) in [
        ("[1]", ""),
        // A positional document has no version field, so even `[2]` is
        // judged by this build's rules, not as a newer version.
        ("[2]", ""),
        (
            r#"{"version":1,"servers":[["eu-1",null,"h",{},null]]}"#,
            "servers[0]",
        ),
        (
            r#"{"version":1,"defaults":{"ssh":["deploy",22,null,null,null,null,null,null,null]}}"#,
            "defaults.ssh",
        ),
        (
            r#"{"version":1,"servers":[{"id":"e","host":"h","ssh":["u",null,null,null,null,null,null,null,null]}]}"#,
            "servers[0].ssh",
        ),
    ] {
        let err = Inventory::from_json(text).unwrap_err();
        assert!(
            matches!(err, Error::InventoryInvalid { .. }),
            "{text}: {err:?}"
        );
        assert_eq!(err.problems().len(), 1, "{err}");
        assert_eq!(err.problems()[0].location, location, "{err}");
    }
}

#[test]
fn a_newer_version_is_read_only() {
    let mut inventory = newer();
    assert!(inventory.is_read_only());
    assert_eq!(inventory.version(), INVENTORY_VERSION + 1);
    assert_eq!(ids(inventory.servers()), ["eu-1"]);
    assert!(inventory.server("eu-1").is_some());

    let read_only = |result: Result<()>| {
        let err = result.unwrap_err();
        assert!(
            matches!(err, Error::InventoryReadOnly { found, supported }
                if found == INVENTORY_VERSION + 1 && supported == INVENTORY_VERSION),
            "{err:?}"
        );
        assert_eq!(err.code(), Code::InventoryReadOnly);
    };
    read_only(inventory.to_json().map(drop));
    read_only(inventory.set_defaults(Defaults::default()).map(drop));
    read_only(inventory.add_server(Server::new("us-1", "h")));
    read_only(inventory.update_server(Server::new("eu-1", "h2")).map(drop));
    read_only(inventory.remove_server("eu-1", true).map(drop));
    read_only(inventory.add_site(Site::new("s", "eu-1", "/p")));
    read_only(
        inventory
            .update_site(Site::new("s", "eu-1", "/p"))
            .map(drop),
    );
    read_only(inventory.remove_site("s").map(drop));
    read_only(inventory.add_group(Group::new("g", Vec::new())));
    read_only(
        inventory
            .update_group(Group::new("g", Vec::new()))
            .map(drop),
    );
    read_only(inventory.remove_group("g").map(drop));
    assert_eq!(inventory, newer());
}

#[test]
fn a_newer_version_this_build_cannot_read_is_unsupported() {
    let next = INVENTORY_VERSION + 1;
    for (text, cause) in [
        // An unknown field.
        (
            format!(r#"{{"version":{next},"servers":[{{"id":"a","host":"h","sudo":false}}]}}"#),
            "unknown field `sudo`",
        ),
        // An unknown value.
        (
            format!(
                r#"{{"version":{next},"servers":[{{"id":"e","host":"h"}}],
                    "sites":[{{"id":"s","server":"e","path":"/p","ownership":"strict"}}]}}"#
            ),
            "unknown ownership policy 'strict'",
        ),
        // An unknown top-level section.
        (
            format!(r#"{{"version":{next},"sites":[],"policies":{{}}}}"#),
            "unknown field `policies`",
        ),
    ] {
        let err = Inventory::from_json(&text).unwrap_err();
        assert!(
            matches!(&err, Error::InventoryUnsupported { found, supported, source }
                if *found == next && *supported == INVENTORY_VERSION
                    && matches!(**source, Error::InventoryJson { .. })),
            "{err:?}"
        );
        assert_eq!(err.code(), Code::InventoryUnsupported);
        assert!(err.to_string().contains(cause), "{err}");
        assert!(err.to_string().ends_with("; upgrade apvm"), "{err}");
    }
}

#[test]
fn a_newer_version_breaking_this_builds_rules_keeps_its_problems() {
    let text = format!(
        r#"{{"version":{},"servers":[{{"id":"a","host":"h"}}],
            "sites":[{{"id":"s","server":"b","path":"/p"}}]}}"#,
        INVENTORY_VERSION + 1
    );
    let err = Inventory::from_json(&text).unwrap_err();
    assert_eq!(err.code(), Code::InventoryUnsupported);
    assert_eq!(texts(&err), ["sites[0].server: unknown server 'b'"]);
}

#[test]
fn lookups_find_records_and_relations() {
    let inventory = sample();
    assert_eq!(inventory.server("us-1").unwrap().host, "us");
    assert_eq!(inventory.site("blog").unwrap().server, "eu-1");
    assert_eq!(inventory.group("qa").unwrap().sites, ["shop", "api"]);
    assert!(inventory.server("nope").is_none());
    assert!(inventory.site("nope").is_none());
    assert!(inventory.group("nope").is_none());
    assert_eq!(
        inventory
            .sites_on("eu-1")
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["shop", "blog"]
    );
    assert_eq!(inventory.sites_on("nope").count(), 0);
    assert_eq!(
        inventory
            .groups_of("api")
            .map(|g| g.id.as_str())
            .collect::<Vec<_>>(),
        ["qa"]
    );
    assert_eq!(inventory.groups_of("nope").count(), 0);
}

#[test]
fn adding_a_taken_id_is_refused() {
    let mut inventory = sample();
    let before = inventory.clone();
    let taken = |result: Result<()>, expected: EntityKind, expected_id: &str| {
        let err = result.unwrap_err();
        assert!(
            matches!(&err, Error::AlreadyExists { kind, id } if *kind == expected && id == expected_id),
            "{err:?}"
        );
        assert_eq!(err.code(), Code::InvalidInput);
    };
    taken(
        inventory.add_server(Server::new("eu-1", "x")),
        EntityKind::Server,
        "eu-1",
    );
    taken(
        inventory.add_site(Site::new("shop", "eu-1", "/x")),
        EntityKind::Site,
        "shop",
    );
    taken(
        inventory.add_group(Group::new("qa", Vec::new())),
        EntityKind::Group,
        "qa",
    );
    assert_eq!(
        inventory
            .add_server(Server::new("eu-1", "x"))
            .unwrap_err()
            .to_string(),
        "server 'eu-1' already exists"
    );
    assert_eq!(inventory, before);
}

#[test]
fn updating_or_removing_a_missing_id_is_refused() {
    let mut inventory = sample();
    let before = inventory.clone();
    let missing = |result: Result<()>, expected: EntityKind| {
        let err = result.unwrap_err();
        assert!(
            matches!(&err, Error::NotFound { kind, id } if *kind == expected && id == "nope"),
            "{err:?}"
        );
        assert_eq!(err.code(), Code::InvalidInput);
    };
    missing(
        inventory.update_server(Server::new("nope", "h")).map(drop),
        EntityKind::Server,
    );
    missing(
        inventory
            .update_site(Site::new("nope", "eu-1", "/p"))
            .map(drop),
        EntityKind::Site,
    );
    missing(
        inventory
            .update_group(Group::new("nope", Vec::new()))
            .map(drop),
        EntityKind::Group,
    );
    missing(
        inventory.remove_server("nope", true).map(drop),
        EntityKind::Server,
    );
    missing(inventory.remove_site("nope").map(drop), EntityKind::Site);
    missing(inventory.remove_group("nope").map(drop), EntityKind::Group);
    assert_eq!(
        inventory.remove_site("nope").unwrap_err().to_string(),
        "site 'nope' does not exist"
    );
    assert_eq!(inventory, before);
}

#[test]
fn invalid_records_are_refused_and_change_nothing() {
    let mut inventory = sample();
    let before = inventory.clone();

    let err = inventory
        .add_server(Server::new("eu-2", "-example.com"))
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidInput);
    assert_eq!(
        err.to_string(),
        "invalid server 'eu-2': host: must start with a letter, digit, '.', '_' or ':'"
    );

    let err = inventory
        .add_site(Site::new("new", "eu-2", "/var/www/new"))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid site 'new': server: unknown server 'eu-2'"
    );
    let err = inventory
        .update_site(Site::new("shop", "eu-1", "relative"))
        .unwrap_err();
    assert_eq!(
        texts(&err),
        ["path: must be an absolute path (start with '/')"]
    );

    let err = inventory
        .add_group(Group::new("g", vec!["shop".into(), "shop".into()]))
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid group 'g': sites[1]: lists site 'shop' twice"
    );
    let err = inventory
        .update_group(Group::new("qa", vec!["ghost".into()]))
        .unwrap_err();
    assert_eq!(texts(&err), ["sites[0]: unknown site 'ghost'"]);
    let err = inventory
        .update_server(Server::new("eu-1", ""))
        .unwrap_err();
    assert_eq!(texts(&err), ["host: must not be empty"]);

    let err = inventory
        .set_defaults(Defaults {
            ssh: SshSettings {
                port: Some(0),
                ..SshSettings::default()
            },
        })
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid defaults: ssh.port: must be between 1 and 65535"
    );

    // A hostile id is escaped in the subject.
    let err = inventory
        .add_server(Server::new("a\u{1b}", "h"))
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid server 'a\\u{1b}': id: "),
        "{err}"
    );
    assert_eq!(inventory, before);
}

#[test]
fn keyword_users_are_refused_so_what_is_saved_reloads_unchanged() {
    let mut inventory = sample();
    let before = inventory.clone();
    let mut site = Site::new("new", "eu-1", "/srv/new");
    site.run_as = Some(RunAs::User("login".into()));
    assert!(inventory.add_site(site).is_err());
    let mut site = Site::new("new", "eu-1", "/srv/new");
    site.owner = Some(Owner::Explicit {
        user: "auto".into(),
        group: None,
    });
    assert!(inventory.add_site(site).is_err());
    assert_eq!(inventory, before);
}

#[test]
fn updates_replace_records_in_place() {
    let mut inventory = sample();
    let mut server = Server::new("eu-1", "198.51.100.7");
    server.name = Some("EU".into());
    let previous = inventory.update_server(server.clone()).unwrap();
    assert_eq!(previous.host, "203.0.113.10");
    assert_eq!(inventory.server("eu-1"), Some(&server));
    assert_eq!(ids(inventory.servers()), ["eu-1", "us-1"]);

    // A site may move to another server.
    let previous = inventory
        .update_site(Site::new("shop", "us-1", "/srv/shop"))
        .unwrap();
    assert_eq!(previous.server, "eu-1");
    assert_eq!(ids(inventory.sites()), ["shop", "blog", "api"]);
    assert_eq!(inventory.sites_on("us-1").count(), 2);

    let previous = inventory
        .update_group(Group::new("qa", vec!["blog".into()]))
        .unwrap();
    assert_eq!(previous.sites, ["shop", "api"]);
    assert_eq!(inventory.group("qa").unwrap().sites, ["blog"]);

    let previous = inventory
        .set_defaults(Defaults {
            ssh: SshSettings {
                binary: Some("/usr/local/bin/ssh".into()),
                ..SshSettings::default()
            },
        })
        .unwrap();
    assert!(previous.is_empty());
    assert_valid(&inventory);
}

#[test]
fn removing_a_site_removes_it_from_its_groups() {
    let mut inventory = sample();
    inventory
        .add_group(Group::new(
            "all",
            vec!["api".into(), "shop".into(), "blog".into()],
        ))
        .unwrap();
    let removal = inventory.remove_site("shop").unwrap();
    assert_eq!(removal.site.id, "shop");
    assert_eq!(removal.groups, ["qa", "all"]);
    assert_eq!(inventory.group("qa").unwrap().sites, ["api"]);
    assert_eq!(inventory.group("all").unwrap().sites, ["api", "blog"]);
    assert_eq!(ids(inventory.sites()), ["blog", "api"]);
    assert_valid(&inventory);

    // A site in no group leaves none.
    inventory
        .add_site(Site::new("solo", "eu-1", "/srv/solo"))
        .unwrap();
    assert!(inventory.remove_site("solo").unwrap().groups.is_empty());
}

#[test]
fn a_server_in_use_is_kept_unless_the_removal_cascades() {
    let mut inventory = sample();
    let before = inventory.clone();
    let err = inventory.remove_server("eu-1", false).unwrap_err();
    assert!(matches!(
        &err,
        Error::ServerInUse { id, sites } if id == "eu-1" && sites == &["shop", "blog"]
    ));
    assert_eq!(err.code(), Code::InvalidInput);
    assert_eq!(inventory, before);

    let removal = inventory.remove_server("eu-1", true).unwrap();
    assert_eq!(removal.server.id, "eu-1");
    assert_eq!(ids(&removal.sites), ["shop", "blog"]);
    assert_eq!(removal.groups, ["qa", "blogs"]);
    assert_eq!(ids(inventory.servers()), ["us-1"]);
    assert_eq!(ids(inventory.sites()), ["api"]);
    assert_eq!(inventory.group("qa").unwrap().sites, ["api"]);
    // An emptied group stays: removing groups is a separate decision.
    assert!(inventory.group("blogs").unwrap().sites.is_empty());
    assert_valid(&inventory);
}

#[test]
fn an_unused_server_is_removed_without_cascading() {
    let mut inventory = sample();
    inventory.add_server(Server::new("spare", "h")).unwrap();
    let removal = inventory.remove_server("spare", false).unwrap();
    assert_eq!(removal.server.id, "spare");
    assert!(removal.sites.is_empty());
    assert!(removal.groups.is_empty());
    assert_eq!(ids(inventory.servers()), ["eu-1", "us-1"]);
    assert_valid(&inventory);
}

#[test]
fn removing_a_group_keeps_its_sites() {
    let mut inventory = sample();
    let group = inventory.remove_group("qa").unwrap();
    assert_eq!(group.sites, ["shop", "api"]);
    assert_eq!(ids(inventory.groups()), ["blogs"]);
    assert_eq!(inventory.sites().len(), 3);
    assert_valid(&inventory);
}

#[test]
fn a_scripted_sequence_of_changes_keeps_the_inventory_valid() {
    let mut inventory = Inventory::new();
    inventory.add_server(Server::new("a", "h1")).unwrap();
    inventory.add_server(Server::new("b", "h2")).unwrap();
    for (index, server) in ["a", "b", "a", "b"].into_iter().enumerate() {
        let id = format!("s{index}");
        inventory
            .add_site(Site::new(id.clone(), server, format!("/srv/{id}")))
            .unwrap();
        assert_valid(&inventory);
    }
    inventory
        .add_group(Group::new("g1", vec!["s0".into(), "s1".into()]))
        .unwrap();
    inventory
        .add_group(Group::new(
            "g2",
            vec!["s3".into(), "s2".into(), "s1".into()],
        ))
        .unwrap();
    inventory
        .update_site(Site::new("s1", "a", "/srv/s1"))
        .unwrap();
    assert_valid(&inventory);
    inventory.remove_site("s2").unwrap();
    assert_valid(&inventory);
    inventory.remove_server("a", true).unwrap();
    assert_valid(&inventory);
    assert_eq!(ids(inventory.sites()), ["s3"]);
    assert!(inventory.group("g1").unwrap().sites.is_empty());
    assert_eq!(inventory.group("g2").unwrap().sites, ["s3"]);
    // The result reloads unchanged.
    let text = inventory.to_json().unwrap();
    assert_eq!(Inventory::from_json(&text).unwrap(), inventory);
}

/// A deterministic 64-bit linear congruential generator (Knuth's MMIX
/// constants): the property test needs no dependency and replays exactly.
struct Lcg(u64);

impl Lcg {
    /// The next 31 random bits.
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // The high bits are the random ones.
        usize::try_from(self.0 >> 33).unwrap()
    }

    /// One of `items`, at random.
    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.next() % items.len()].clone()
    }
}

/// Ids the generator uses: three valid, one invalid.
const IDS: [&str; 4] = ["a", "b", "c", "Bad"];

/// A random site, sometimes invalid (a relative path, an unknown server, a
/// keyword user, a root owner).
fn random_site(rng: &mut Lcg, id: &str) -> Site {
    let mut site = Site::new(id, rng.pick(&IDS), rng.pick(&["/srv/x", "/srv/y", "rel"]));
    site.run_as = rng.pick(&[
        None,
        Some(RunAs::Login),
        Some(RunAs::User("deploy".into())),
        Some(RunAs::User("login".into())),
    ]);
    site.owner = rng.pick(&[
        None,
        Some(Owner::from("ftpuser:www-data")),
        Some(Owner::Explicit {
            user: "auto".into(),
            group: None,
        }),
        Some(Owner::from("root")),
    ]);
    site
}

/// One random change, valid or not.
fn random_change(inventory: &mut Inventory, rng: &mut Lcg) -> Result<()> {
    let id = rng.pick(&IDS);
    let members: Vec<String> = (0..rng.next() % 4)
        .map(|_| rng.pick(&IDS).to_string())
        .collect();
    match rng.next() % 10 {
        0 => inventory.add_server(Server::new(id, rng.pick(&["h", "-h"]))),
        1 => inventory
            .update_server(Server::new(id, rng.pick(&["h2", ""])))
            .map(drop),
        2 => inventory
            .remove_server(id, rng.next().is_multiple_of(2))
            .map(drop),
        3 => inventory.add_site(random_site(rng, id)),
        4 => inventory.update_site(random_site(rng, id)).map(drop),
        5 => inventory.remove_site(id).map(drop),
        6 => inventory.add_group(Group::new(id, members)),
        7 => inventory.update_group(Group::new(id, members)).map(drop),
        8 => inventory.remove_group(id).map(drop),
        _ => {
            let mut defaults = Defaults::default();
            defaults.ssh.port = rng.pick(&[None, Some(0), Some(22)]);
            inventory.set_defaults(defaults).map(drop)
        }
    }
}

#[test]
fn random_changes_keep_the_inventory_valid_and_reloadable() {
    let mut rng = Lcg(0x5EED);
    let (mut accepted, mut refused) = (0, 0);
    for _ in 0..300 {
        let mut inventory = Inventory::new();
        for _ in 0..40 {
            let before = inventory.clone();
            match random_change(&mut inventory, &mut rng) {
                Ok(()) => {
                    accepted += 1;
                    assert_valid(&inventory);
                }
                Err(err) => {
                    refused += 1;
                    assert_eq!(inventory, before, "a refused change ({err}) changed it");
                }
            }
        }
        let text = inventory.to_json().unwrap();
        assert_eq!(Inventory::from_json(&text).unwrap(), inventory, "{text}");
    }
    // Both paths are exercised substantially.
    assert!(
        accepted > 1000 && refused > 1000,
        "{accepted} accepted, {refused} refused"
    );
}
