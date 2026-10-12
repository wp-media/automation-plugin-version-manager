//! The inventory through the public API: the documented example document
//! (plan §7), its round trip, and how a broken document is reported.

use apvm_sites::{
    Code, Error, INVENTORY_VERSION, Inventory, Owner, OwnershipPolicy, Problem, RunAs,
};

/// The example document of plan §7, verbatim.
const PLAN_EXAMPLE: &str = r#"{
  "version": 1,
  "defaults": {
    "ssh": { "user": "deploy", "port": 22, "identity_file": "~/.ssh/id_ed25519",
             "connect_timeout_secs": 15, "multiplex": true }
  },
  "servers": [
    { "id": "eu-1", "name": "EU VPS", "host": "203.0.113.10",
      "ssh": { "user": "ubuntu", "identity_file": "~/.ssh/eu1_ed25519" } },
    { "id": "staging-box", "host": "staging" }
  ],
  "sites": [
    { "id": "shop", "server": "eu-1", "path": "/var/www/shop/public", "url": "https://shop.example.com" },
    { "id": "ftpsite", "server": "eu-1", "path": "/home/ftpuser/public_html", "notes": "ftpuser:www-data" },
    { "id": "blog", "server": "eu-1", "path": "/var/www/blog",
      "wp_cli": ["/usr/bin/php8.3", "/opt/wp-cli.phar"], "run_as": "login", "ownership": "check" }
  ],
  "groups": [ { "id": "qa", "sites": ["shop", "ftpsite", "blog"] } ]
}"#;

#[test]
fn the_documented_example_loads() {
    let inventory = Inventory::from_json(PLAN_EXAMPLE).expect("the plan's example is valid");
    assert_eq!(inventory.version(), INVENTORY_VERSION);
    assert!(!inventory.is_read_only());

    let defaults = &inventory.defaults().ssh;
    assert_eq!(defaults.user.as_deref(), Some("deploy"));
    assert_eq!(defaults.port, Some(22));
    assert_eq!(defaults.multiplex, Some(true));

    let eu = inventory.server("eu-1").expect("eu-1");
    assert_eq!(eu.name.as_deref(), Some("EU VPS"));
    assert_eq!(eu.ssh.user.as_deref(), Some("ubuntu"));
    // An ssh_config alias is a valid host.
    assert_eq!(
        inventory.server("staging-box").expect("staging-box").host,
        "staging"
    );

    let blog = inventory.site("blog").expect("blog");
    assert_eq!(blog.run_as, Some(RunAs::Login));
    assert_eq!(blog.ownership, Some(OwnershipPolicy::Check));
    assert_eq!(blog.owner, None::<Owner>);
    assert_eq!(
        blog.wp_cli.as_deref(),
        Some(
            [
                "/usr/bin/php8.3".to_string(),
                "/opt/wp-cli.phar".to_string()
            ]
            .as_slice()
        )
    );
    assert_eq!(
        inventory
            .sites_on("eu-1")
            .map(|site| site.id.as_str())
            .collect::<Vec<_>>(),
        ["shop", "ftpsite", "blog"]
    );
    assert_eq!(inventory.group("qa").expect("qa").sites.len(), 3);
}

#[test]
fn the_documented_example_round_trips() {
    let inventory = Inventory::from_json(PLAN_EXAMPLE).expect("valid");
    let saved = inventory.to_json().expect("writable");
    assert_eq!(Inventory::from_json(&saved).expect("reloads"), inventory);
}

#[test]
fn the_documented_error_location() {
    let broken = PLAN_EXAMPLE.replace(
        r#""id": "ftpsite", "server": "eu-1""#,
        r#""id": "ftpsite", "server": "eu-2""#,
    );
    let err = Inventory::from_json(&broken).expect_err("eu-2 does not exist");
    assert_eq!(err.code(), Code::InventoryInvalid);
    assert_eq!(
        err.problems(),
        [Problem {
            location: "sites[1].server".into(),
            message: "unknown server 'eu-2'".into(),
        }]
    );
}

#[test]
fn a_typo_is_an_error_not_an_ignored_setting() {
    let typo = PLAN_EXAMPLE.replace(r#""run_as": "login""#, r#""runas": "login""#);
    let err = Inventory::from_json(&typo).expect_err("unknown field");
    assert!(matches!(err, Error::InventoryJson { .. }), "{err:?}");
    assert!(err.to_string().contains("unknown field `runas`"), "{err}");
}
