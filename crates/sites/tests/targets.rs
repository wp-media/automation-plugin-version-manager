//! Target selection and effective settings through the public API, on the
//! example document of plan §7 (the `-g qa` selection of plan §5.6).

use apvm_sites::{Code, Inventory, OwnershipPolicy, RunAs, Setting, SettingSource, Targets};

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

/// The example inventory.
fn inventory() -> Inventory {
    Inventory::from_json(PLAN_EXAMPLE).expect("the plan's example is valid")
}

#[test]
fn group_qa_resolves_to_its_three_sites() {
    let resolved = inventory()
        .resolve_targets(&Targets::new().with_groups(["qa"]))
        .expect("qa has sites");
    assert_eq!(
        resolved.ids().collect::<Vec<_>>(),
        ["shop", "ftpsite", "blog"]
    );
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
}

#[test]
fn the_server_overrides_the_default_key_and_user() {
    let blog = inventory().resolve_site("blog").expect("blog");
    let ssh = &blog.server.ssh;
    assert_eq!(blog.server.host, "203.0.113.10");
    assert_eq!(
        ssh.user,
        Some(Setting::new("ubuntu".to_string(), SettingSource::Server))
    );
    assert_eq!(
        ssh.identity_file,
        Some(Setting::new(
            "~/.ssh/eu1_ed25519".to_string(),
            SettingSource::Server
        ))
    );
    assert_eq!(ssh.port, Some(Setting::new(22, SettingSource::Defaults)));
    assert_eq!(
        ssh.connect_timeout_secs,
        Setting::new(15, SettingSource::Defaults)
    );
    assert_eq!(ssh.proxy_jump, None);

    assert_eq!(blog.run_as, Setting::new(RunAs::Login, SettingSource::Site));
    assert_eq!(
        blog.ownership,
        Setting::new(OwnershipPolicy::Check, SettingSource::Site)
    );
    assert_eq!(blog.plugins_dir.value, "/var/www/blog/wp-content/plugins");
    assert!(!blog.plugins_dir_is_pinned());
}

#[test]
fn a_server_without_sites_selects_nothing() {
    let error = inventory()
        .resolve_targets(&Targets::new().with_servers(["staging-box"]))
        .expect_err("staging-box has no sites");
    assert_eq!(error.code(), Code::InvalidInput);
    assert_eq!(
        error.to_string(),
        "invalid targets: no site selected: the selection matches no site; \
         servers[0]: server 'staging-box' has no sites"
    );
}
