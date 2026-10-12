//! Tests of settings resolution and target selection.

use super::*;
use crate::error::Code;
use crate::inventory::Group;

/// Servers eu-1 (shop, blog, lab), us-1 (api, docs) and spare (no sites);
/// sites in that inventory order: shop, blog, api, docs, lab; groups qa
/// (api, shop), blogs (blog), web (shop, blog) and none (empty).
fn sample() -> Inventory {
    let mut inventory = Inventory::new();
    for server in [
        Server::new("eu-1", "eu.example.com"),
        Server::new("us-1", "us.example.com"),
        Server::new("spare", "spare.example.com"),
    ] {
        inventory.add_server(server).unwrap();
    }
    for (id, server, path) in [
        ("shop", "eu-1", "/var/www/shop"),
        ("blog", "eu-1", "/var/www/blog"),
        ("api", "us-1", "/srv/api"),
        ("docs", "us-1", "/srv/docs"),
        ("lab", "eu-1", "/srv/lab"),
    ] {
        inventory.add_site(Site::new(id, server, path)).unwrap();
    }
    for (id, sites) in [
        ("qa", vec!["api", "shop"]),
        ("blogs", vec!["blog"]),
        ("web", vec!["shop", "blog"]),
        ("none", vec![]),
    ] {
        let sites = sites.into_iter().map(String::from).collect();
        inventory.add_group(Group::new(id, sites)).unwrap();
    }
    inventory
}

/// The ids `targets` resolves to in `inventory`.
fn resolved_ids(inventory: &Inventory, targets: &Targets) -> Vec<String> {
    let resolved = inventory.resolve_targets(targets).unwrap();
    resolved.ids().map(String::from).collect()
}

/// The problems of a refused selection, as `location: message`.
fn refusal(inventory: &Inventory, targets: &Targets) -> Vec<String> {
    let error = inventory.resolve_targets(targets).unwrap_err();
    assert_eq!(error.code(), Code::InvalidInput, "{error}");
    assert!(matches!(&error, Error::InvalidInput { subject, .. } if subject == "targets"));
    error.problems().iter().map(Problem::to_string).collect()
}

/// A level of the SSH precedence, for the precedence table.
#[derive(Clone, Copy, Debug)]
enum Level {
    /// `defaults.ssh`.
    Defaults,
    /// The server's `ssh`.
    Server,
}

/// One SSH field in the precedence table: how to set it at a level, how to
/// read it back, and its built-in value.
struct SshField {
    /// Field name, for messages.
    name: &'static str,
    /// Set the field to that level's value.
    set: fn(&mut SshSettings, Level),
    /// The effective value and its source, as text.
    get: fn(&ResolvedSsh) -> Option<(String, SettingSource)>,
    /// The value each level sets, as text.
    values: [&'static str; 2],
    /// The built-in value, if any.
    builtin: Option<&'static str>,
    /// Whether a server may set it (`binary` is defaults-only).
    on_server: bool,
}

/// `setting` as text and source.
fn shown<T: ToString>(setting: &Setting<T>) -> (String, SettingSource) {
    (setting.value.to_string(), setting.source)
}

/// Pick the value of a level from a `[defaults, server]` pair.
fn at<T: Copy>(values: [T; 2], level: Level) -> T {
    match level {
        Level::Defaults => values[0],
        Level::Server => values[1],
    }
}

/// Every SSH field, with distinct values per level.
fn ssh_fields() -> Vec<SshField> {
    vec![
        SshField {
            name: "user",
            set: |ssh, level| ssh.user = Some(at(["deploy", "ubuntu"], level).into()),
            get: |ssh| ssh.user.as_ref().map(shown),
            values: ["deploy", "ubuntu"],
            builtin: None,
            on_server: true,
        },
        SshField {
            name: "port",
            set: |ssh, level| ssh.port = Some(at([2201, 2202], level)),
            get: |ssh| ssh.port.as_ref().map(shown),
            values: ["2201", "2202"],
            builtin: None,
            on_server: true,
        },
        SshField {
            name: "identity_file",
            set: |ssh, level| {
                ssh.identity_file = Some(at(["~/.ssh/main", "~/.ssh/eu1"], level).into());
            },
            get: |ssh| ssh.identity_file.as_ref().map(shown),
            values: ["~/.ssh/main", "~/.ssh/eu1"],
            builtin: None,
            on_server: true,
        },
        SshField {
            name: "connect_timeout_secs",
            set: |ssh, level| ssh.connect_timeout_secs = Some(at([30, 45], level)),
            get: |ssh| Some(shown(&ssh.connect_timeout_secs)),
            values: ["30", "45"],
            builtin: Some("15"),
            on_server: true,
        },
        SshField {
            name: "known_hosts_file",
            set: |ssh, level| {
                ssh.known_hosts_file = Some(at(["~/.ssh/kh_main", "~/.ssh/kh_eu1"], level).into());
            },
            get: |ssh| ssh.known_hosts_file.as_ref().map(shown),
            values: ["~/.ssh/kh_main", "~/.ssh/kh_eu1"],
            builtin: None,
            on_server: true,
        },
        SshField {
            name: "ssh_config_file",
            set: |ssh, level| {
                ssh.ssh_config_file = Some(at(["~/.ssh/cfg_main", "~/.ssh/cfg_eu1"], level).into());
            },
            get: |ssh| ssh.ssh_config_file.as_ref().map(shown),
            values: ["~/.ssh/cfg_main", "~/.ssh/cfg_eu1"],
            builtin: None,
            on_server: true,
        },
        SshField {
            name: "proxy_jump",
            set: |ssh, level| {
                ssh.proxy_jump = Some(at(["bastion", "jump@eu-gw:2222"], level).into());
            },
            get: |ssh| ssh.proxy_jump.as_ref().map(shown),
            values: ["bastion", "jump@eu-gw:2222"],
            builtin: None,
            on_server: true,
        },
        SshField {
            name: "multiplex",
            set: |ssh, level| ssh.multiplex = Some(at([false, true], level)),
            get: |ssh| Some(shown(&ssh.multiplex)),
            values: ["false", "true"],
            builtin: Some("true"),
            on_server: true,
        },
        SshField {
            name: "binary",
            set: |ssh, level| ssh.binary = Some(at(["/opt/ssh/bin/ssh", "unused"], level).into()),
            get: |ssh| Some(shown(&ssh.binary)),
            values: ["/opt/ssh/bin/ssh", "unused"],
            builtin: Some("ssh"),
            on_server: false,
        },
    ]
}

#[test]
fn ssh_settings_follow_the_precedence_table() {
    // Every field × every combination of levels, through a valid inventory.
    for field in ssh_fields() {
        for (in_defaults, in_server) in [(false, false), (true, false), (false, true), (true, true)]
        {
            if in_server && !field.on_server {
                continue;
            }
            let mut defaults = SshSettings::default();
            let mut server = Server::new("eu-1", "eu.example.com");
            if in_defaults {
                (field.set)(&mut defaults, Level::Defaults);
            }
            if in_server {
                (field.set)(&mut server.ssh, Level::Server);
            }
            let mut inventory = Inventory::new();
            inventory.set_defaults(Defaults { ssh: defaults }).unwrap();
            inventory.add_server(server).unwrap();

            let expected = if in_server {
                Some((field.values[1].to_string(), SettingSource::Server))
            } else if in_defaults {
                Some((field.values[0].to_string(), SettingSource::Defaults))
            } else {
                field
                    .builtin
                    .map(|value| (value.to_string(), SettingSource::BuiltIn))
            };
            let resolved = inventory.resolve_server("eu-1").unwrap();
            assert_eq!(
                (field.get)(&resolved.ssh),
                expected,
                "{}: defaults {in_defaults}, server {in_server}",
                field.name
            );
        }
    }
}

#[test]
fn ssh_fields_merge_independently() {
    // The server sets some fields, the defaults others: each field is
    // taken from its own most specific level.
    let defaults = SshSettings {
        user: Some("deploy".into()),
        port: Some(22),
        identity_file: Some("~/.ssh/id_ed25519".into()),
        connect_timeout_secs: Some(20),
        ..SshSettings::default()
    };
    let server = SshSettings {
        user: Some("ubuntu".into()),
        identity_file: Some("~/.ssh/eu1".into()),
        multiplex: Some(false),
        ..SshSettings::default()
    };
    let ssh = ResolvedSsh::merge(&defaults, &server);
    assert_eq!(
        ssh.user,
        Some(Setting::new("ubuntu".to_string(), SettingSource::Server))
    );
    assert_eq!(ssh.port, Some(Setting::new(22, SettingSource::Defaults)));
    assert_eq!(
        ssh.identity_file,
        Some(Setting::new(
            "~/.ssh/eu1".to_string(),
            SettingSource::Server
        ))
    );
    assert_eq!(
        ssh.connect_timeout_secs,
        Setting::new(20, SettingSource::Defaults)
    );
    assert_eq!(ssh.multiplex, Setting::new(false, SettingSource::Server));
    assert_eq!(
        ssh.binary,
        Setting::new("ssh".to_string(), SettingSource::BuiltIn)
    );
    assert_eq!(ssh.known_hosts_file, None);
    assert_eq!(ssh.ssh_config_file, None);
    assert_eq!(ssh.proxy_jump, None);
}

#[test]
fn a_server_binary_is_never_used() {
    // A valid inventory refuses it; the merge ignores it regardless.
    let server = SshSettings {
        binary: Some("/tmp/evil-ssh".into()),
        ..SshSettings::default()
    };
    let ssh = ResolvedSsh::merge(&SshSettings::default(), &server);
    assert_eq!(
        ssh.binary,
        Setting::new(BUILTIN_SSH_BINARY.to_string(), SettingSource::BuiltIn)
    );
}

#[test]
fn nothing_set_gives_the_built_in_values_only() {
    let ssh = ResolvedSsh::merge(&SshSettings::default(), &SshSettings::default());
    assert_eq!(
        ssh.connect_timeout_secs,
        Setting::new(15, SettingSource::BuiltIn)
    );
    assert_eq!(ssh.multiplex, Setting::new(true, SettingSource::BuiltIn));
    assert_eq!(
        ssh.binary,
        Setting::new("ssh".to_string(), SettingSource::BuiltIn)
    );
    // Everything else is left to ssh.
    assert_eq!(
        (ssh.user, ssh.port, ssh.identity_file, ssh.proxy_jump),
        (None, None, None, None)
    );
}

/// One site field in the precedence table.
struct SiteField {
    /// Field name, for messages.
    name: &'static str,
    /// Set the field on the site.
    set: fn(&mut Site),
    /// The effective value and its source, as text.
    get: fn(&ResolvedSite) -> (String, SettingSource),
    /// The value `set` writes, as text.
    set_value: &'static str,
    /// The built-in value, as text (for a site at `/var/www/shop`).
    builtin: &'static str,
}

/// `setting` as debug text and source.
fn debug<T: fmt::Debug>(setting: &Setting<T>) -> (String, SettingSource) {
    (format!("{:?}", setting.value), setting.source)
}

/// Every site setting that has a built-in value.
fn site_fields() -> Vec<SiteField> {
    vec![
        SiteField {
            name: "wp_cli",
            set: |site| {
                site.wp_cli = Some(vec!["/usr/bin/php8.3".into(), "/opt/wp-cli.phar".into()])
            },
            get: |site| debug(&site.wp_cli),
            set_value: r#"["/usr/bin/php8.3", "/opt/wp-cli.phar"]"#,
            builtin: r#"["wp"]"#,
        },
        SiteField {
            name: "run_as",
            set: |site| site.run_as = Some(RunAs::Login),
            get: |site| shown(&site.run_as),
            set_value: "login",
            builtin: "auto",
        },
        SiteField {
            name: "owner",
            set: |site| site.owner = Some(Owner::from("ftpuser:www-data")),
            get: |site| shown(&site.owner),
            set_value: "ftpuser:www-data",
            builtin: "auto",
        },
        SiteField {
            name: "ownership",
            set: |site| site.ownership = Some(OwnershipPolicy::Check),
            get: |site| shown(&site.ownership),
            set_value: "check",
            builtin: "mirror",
        },
        SiteField {
            name: "plugins_dir",
            set: |site| site.plugins_dir = Some("/var/www/shop/app/plugins".into()),
            get: |site| shown(&site.plugins_dir),
            set_value: "/var/www/shop/app/plugins",
            builtin: "/var/www/shop/wp-content/plugins",
        },
        SiteField {
            name: "allow_root",
            set: |site| site.allow_root = Some(true),
            get: |site| shown(&site.allow_root),
            set_value: "true",
            builtin: "false",
        },
        SiteField {
            name: "remote_tmp_dir",
            set: |site| site.remote_tmp_dir = Some("/var/tmp".into()),
            get: |site| shown(&site.remote_tmp_dir),
            set_value: "/var/tmp",
            builtin: "/tmp",
        },
    ]
}

#[test]
fn site_settings_follow_the_precedence_table() {
    for field in site_fields() {
        for set in [false, true] {
            let mut inventory = Inventory::new();
            inventory.add_server(Server::new("eu-1", "h")).unwrap();
            let mut site = Site::new("shop", "eu-1", "/var/www/shop");
            if set {
                (field.set)(&mut site);
            }
            inventory.add_site(site).unwrap();
            let expected = if set {
                (field.set_value.to_string(), SettingSource::Site)
            } else {
                (field.builtin.to_string(), SettingSource::BuiltIn)
            };
            let resolved = inventory.resolve_site("shop").unwrap();
            assert_eq!(
                (field.get)(&resolved),
                expected,
                "{}: set {set}",
                field.name
            );
        }
    }
}

#[test]
fn sites_carry_their_plain_values_and_server() {
    let mut inventory = sample();
    let mut site = inventory.site("blog").unwrap().clone();
    site.url = Some("https://blog.example.com".into());
    site.command_timeout_secs = Some(600);
    site.name = Some("Blog".into());
    inventory.update_site(site).unwrap();

    let blog = inventory.resolve_site("blog").unwrap();
    assert_eq!(blog.id, "blog");
    assert_eq!(blog.path, "/var/www/blog");
    assert_eq!(blog.url.as_deref(), Some("https://blog.example.com"));
    assert_eq!(blog.command_timeout_secs, Some(600));
    assert_eq!(blog.server, inventory.resolve_server("eu-1").unwrap());
    assert_eq!(blog.server.host, "eu.example.com");

    let shop = inventory.resolve_site("shop").unwrap();
    assert_eq!((shop.url, shop.command_timeout_secs), (None, None));
}

#[test]
fn a_pinned_plugins_dir_is_reported() {
    let mut inventory = sample();
    assert!(
        !inventory
            .resolve_site("shop")
            .unwrap()
            .plugins_dir_is_pinned()
    );
    let mut site = inventory.site("shop").unwrap().clone();
    // Pinning the standard directory still counts as pinned.
    site.plugins_dir = Some("/var/www/shop/wp-content/plugins".into());
    inventory.update_site(site).unwrap();
    let shop = inventory.resolve_site("shop").unwrap();
    assert!(shop.plugins_dir_is_pinned());
    assert_eq!(shop.plugins_dir.value, "/var/www/shop/wp-content/plugins");
}

#[test]
fn unknown_ids_are_not_found() {
    let inventory = sample();
    let error = inventory.resolve_site("nope").unwrap_err();
    assert!(matches!(&error, Error::NotFound { kind: EntityKind::Site, id } if id == "nope"));
    assert_eq!(error.code(), Code::InvalidInput);
    let error = inventory.resolve_server("nope").unwrap_err();
    assert!(matches!(
        &error,
        Error::NotFound {
            kind: EntityKind::Server,
            ..
        }
    ));
}

#[test]
fn a_read_only_inventory_resolves() {
    let text = format!(
        r#"{{"version":{},"servers":[{{"id":"eu-1","host":"h"}}],
            "sites":[{{"id":"shop","server":"eu-1","path":"/p"}}]}}"#,
        crate::INVENTORY_VERSION + 1
    );
    let inventory = Inventory::from_json(&text).unwrap();
    assert!(inventory.is_read_only());
    assert_eq!(resolved_ids(&inventory, &Targets::all()), ["shop"]);
}

#[test]
fn source_and_settings_serialize_for_front_ends() {
    for (source, text) in [
        (SettingSource::BuiltIn, "built-in"),
        (SettingSource::Defaults, "defaults"),
        (SettingSource::Server, "server"),
        (SettingSource::Site, "site"),
    ] {
        assert_eq!(source.as_str(), text);
        assert_eq!(source.to_string(), text);
        assert_eq!(
            serde_json::to_string(&source).unwrap(),
            format!("\"{text}\"")
        );
    }
    let shop = sample().resolve_site("shop").unwrap();
    let json = serde_json::to_value(&shop).unwrap();
    assert_eq!(
        json["run_as"],
        serde_json::json!({"value": "auto", "source": "built-in"})
    );
    assert_eq!(json["server"]["ssh"]["user"], serde_json::Value::Null);
    assert_eq!(
        json["server"]["ssh"]["connect_timeout_secs"],
        serde_json::json!({"value": 15, "source": "built-in"})
    );
    assert_eq!(json["wp_cli"]["value"], serde_json::json!(["wp"]));
}

#[test]
fn targets_build_up_and_report_emptiness() {
    let targets = Targets::new()
        .with_sites(["shop"])
        .with_sites(vec!["blog".to_string()])
        .with_groups(["qa"])
        .with_servers(["eu-1"]);
    assert_eq!(targets.sites, ["shop", "blog"]);
    assert_eq!(targets.groups, ["qa"]);
    assert_eq!(targets.servers, ["eu-1"]);
    assert!(!targets.all_sites);
    assert!(!targets.is_empty());

    assert!(Targets::new().is_empty());
    assert!(Targets::default().is_empty());
    assert!(Targets::all().all_sites);
    assert!(!Targets::all().is_empty());
    assert!(Targets::all().with_all_sites(false).is_empty());
    assert!(!Targets::new().with_groups(["none"]).is_empty());
    assert!(!Targets::new().with_servers(["spare"]).is_empty());
}

#[test]
fn the_union_keeps_inventory_order() {
    let inventory = sample();
    // Named in reverse, through a group listing them out of order.
    let cases: [(Targets, &[&str]); 7] = [
        (
            Targets::new().with_sites(["lab", "api", "shop"]),
            &["shop", "api", "lab"],
        ),
        (Targets::new().with_groups(["qa"]), &["shop", "api"]),
        (
            Targets::new().with_servers(["us-1", "eu-1"]),
            &["shop", "blog", "api", "docs", "lab"],
        ),
        (
            Targets::new().with_servers(["eu-1"]),
            &["shop", "blog", "lab"],
        ),
        (
            Targets::new().with_sites(["docs"]).with_groups(["blogs"]),
            &["blog", "docs"],
        ),
        (
            Targets::new().with_groups(["blogs"]).with_servers(["us-1"]),
            &["blog", "api", "docs"],
        ),
        (Targets::all(), &["shop", "blog", "api", "docs", "lab"]),
    ];
    for (targets, expected) in cases {
        assert_eq!(resolved_ids(&inventory, &targets), expected, "{targets:?}");
    }
}

#[test]
fn each_site_is_selected_once() {
    let inventory = sample();
    // shop: by id twice, in two overlapping groups, and on its server.
    let targets = Targets::new()
        .with_sites(["shop", "shop"])
        .with_groups(["qa", "web", "qa"])
        .with_servers(["eu-1", "eu-1"]);
    let resolved = inventory.resolve_targets(&targets).unwrap();
    assert_eq!(
        resolved.ids().collect::<Vec<_>>(),
        ["shop", "blog", "api", "lab"]
    );
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    // Every part together with all sites is every site, once.
    let everything = targets.with_all_sites(true);
    assert_eq!(
        resolved_ids(&inventory, &everything),
        ["shop", "blog", "api", "docs", "lab"]
    );
}

#[test]
fn resolved_sites_carry_their_settings() {
    let inventory = sample();
    let resolved = inventory
        .resolve_targets(&Targets::new().with_groups(["qa"]))
        .unwrap();
    let expected = [
        inventory.resolve_site("shop").unwrap(),
        inventory.resolve_site("api").unwrap(),
    ];
    assert_eq!(resolved.sites, expected);
}

#[test]
fn an_empty_selection_is_an_error() {
    let inventory = sample();
    assert_eq!(
        refusal(&inventory, &Targets::new()),
        ["no site selected: name at least one site, group or server, or all sites"]
    );
    // Even on an empty inventory, nothing named is never "everything".
    assert_eq!(refusal(&Inventory::new(), &Targets::new()).len(), 1);
}

#[test]
fn a_selection_that_matches_no_site_is_an_error() {
    let inventory = sample();
    let targets = Targets::new()
        .with_groups(["none", "none"])
        .with_servers(["spare"]);
    assert_eq!(
        refusal(&inventory, &targets),
        [
            "no site selected: the selection matches no site",
            "groups[0]: group 'none' has no sites",
            "servers[0]: server 'spare' has no sites",
        ]
    );

    let mut empty = Inventory::new();
    assert_eq!(
        refusal(&empty, &Targets::all()),
        [
            "no site selected: the selection matches no site",
            "all_sites: the inventory has no sites",
        ]
    );
    empty.add_server(Server::new("eu-1", "h")).unwrap();
    assert_eq!(
        refusal(&empty, &Targets::all().with_servers(["eu-1"])),
        [
            "no site selected: the selection matches no site",
            "servers[0]: server 'eu-1' has no sites",
            "all_sites: the inventory has no sites",
        ]
    );
}

#[test]
fn parts_without_sites_warn_when_others_match() {
    let inventory = sample();
    let targets = Targets::new()
        .with_sites(["shop"])
        .with_groups(["blogs", "none", "none"])
        .with_servers(["spare"]);
    let resolved = inventory.resolve_targets(&targets).unwrap();
    assert_eq!(resolved.ids().collect::<Vec<_>>(), ["shop", "blog"]);
    assert_eq!(
        resolved.warnings,
        [
            TargetWarning::NoSites {
                kind: EntityKind::Group,
                id: "none".into()
            },
            TargetWarning::NoSites {
                kind: EntityKind::Server,
                id: "spare".into()
            },
        ]
    );
    let messages: Vec<String> = resolved.warnings.iter().map(ToString::to_string).collect();
    assert_eq!(
        messages,
        ["group 'none' has no sites", "server 'spare' has no sites"]
    );
}

#[test]
fn unknown_ids_are_all_reported_with_their_location() {
    let inventory = sample();
    let targets = Targets::new()
        .with_sites(["shop", "nope", "Shop", " blog "])
        .with_groups(["QA", "ghost"])
        .with_servers(["eu-1", "EU-1", "mars"])
        .with_all_sites(true);
    assert_eq!(
        refusal(&inventory, &targets),
        [
            "sites[1]: unknown site 'nope'",
            "sites[2]: unknown site 'Shop'; did you mean 'shop'?",
            "sites[3]: unknown site ' blog '; did you mean 'blog'?",
            "groups[0]: unknown group 'QA'; did you mean 'qa'?",
            "groups[1]: unknown group 'ghost'",
            "servers[1]: unknown server 'EU-1'; did you mean 'eu-1'?",
            "servers[2]: unknown server 'mars'",
        ]
    );
}

#[test]
fn unknown_ids_are_quoted_safely() {
    let inventory = sample();
    let targets = Targets::new().with_sites(["a\u{1b}[2J", "x', 'y", ""]);
    assert_eq!(
        refusal(&inventory, &targets),
        [
            "sites[0]: unknown site 'a\\u{1b}[2J'",
            "sites[1]: unknown site 'x\\', \\'y'",
            "sites[2]: unknown site ''",
        ]
    );
}

#[test]
fn ids_of_another_kind_are_unknown() {
    // A group id is not a site id, and so on: each part names its own kind.
    let inventory = sample();
    let targets = Targets::new()
        .with_sites(["qa"])
        .with_groups(["shop"])
        .with_servers(["qa"]);
    assert_eq!(
        refusal(&inventory, &targets),
        [
            "sites[0]: unknown site 'qa'",
            "groups[0]: unknown group 'shop'",
            "servers[0]: unknown server 'qa'",
        ]
    );
}

/// eu-1 and eu-1-again both reach `eu.example.com` as deploy on port 22;
/// shop, mirror (on eu-1-again) and twin (same server) are /var/www/shop;
/// upper is too, on `EU.example.com`, which ssh may resolve differently.
fn twins() -> Inventory {
    let mut inventory = Inventory::new();
    let ssh = SshSettings {
        user: Some("deploy".into()),
        port: Some(22),
        ..SshSettings::default()
    };
    inventory.set_defaults(Defaults { ssh }).unwrap();
    for (id, host) in [
        ("eu-1", "eu.example.com"),
        ("eu-1-again", "eu.example.com"),
        ("eu-1-upper", "EU.example.com"),
        ("eu-2", "eu2.example.com"),
    ] {
        inventory.add_server(Server::new(id, host)).unwrap();
    }
    for (id, server, path) in [
        ("shop", "eu-1", "/var/www/shop"),
        ("blog", "eu-1", "/var/www/blog"),
        ("mirror", "eu-1-again", "/var/www/shop"),
        ("upper", "eu-1-upper", "/var/www/shop"),
        ("other", "eu-2", "/var/www/shop"),
        ("twin", "eu-1", "/var/www/shop"),
    ] {
        inventory.add_site(Site::new(id, server, path)).unwrap();
    }
    inventory
}

#[test]
fn sites_of_the_same_wordpress_warn_once_per_install() {
    let inventory = twins();
    let resolved = inventory.resolve_targets(&Targets::all()).unwrap();
    assert_eq!(
        resolved.warnings,
        [TargetWarning::SameWordPress {
            sites: vec!["shop".into(), "mirror".into(), "twin".into()],
            host: "eu.example.com".into(),
            port: Some(22),
            user: Some("deploy".into()),
            proxy_jump: None,
            ssh_config_file: None,
            path: "/var/www/shop".into(),
        }]
    );
    assert_eq!(
        resolved.warnings[0].to_string(),
        "sites 'shop', 'mirror', 'twin' are the same WordPress install: host \
         'eu.example.com', port 22, user 'deploy', path '/var/www/shop'; apvm works on it \
         once per site"
    );
}

#[test]
fn only_selected_sites_are_compared() {
    let inventory = twins();
    let one = inventory.resolve_targets(&Targets::new().with_sites(["shop", "blog", "other"]));
    assert!(one.unwrap().warnings.is_empty());
    let two = inventory
        .resolve_targets(&Targets::new().with_sites(["twin", "mirror", "upper"]))
        .unwrap();
    assert!(
        matches!(&two.warnings[..], [TargetWarning::SameWordPress { sites, host, .. }]
            if sites == &["mirror", "twin"] && host == "eu.example.com"),
        "{:?}",
        two.warnings
    );
}

#[test]
fn the_same_jump_hosts_and_ssh_config_are_the_same_destination() {
    // Set on the defaults or on each server alike, they are compared by
    // value and named in the warning.
    let mut inventory = Inventory::new();
    let ssh = SshSettings {
        ssh_config_file: Some("~/.ssh/apvm_config".into()),
        ..SshSettings::default()
    };
    inventory.set_defaults(Defaults { ssh }).unwrap();
    for id in ["a", "b"] {
        let mut server = Server::new(id, "10.0.0.5");
        server.ssh.proxy_jump = Some("deploy@bastion:2222".into());
        inventory.add_server(server).unwrap();
    }
    for (id, server) in [("one", "a"), ("two", "b")] {
        inventory
            .add_site(Site::new(id, server, "/var/www/html"))
            .unwrap();
    }
    let resolved = inventory.resolve_targets(&Targets::all()).unwrap();
    assert_eq!(
        resolved.warnings,
        [TargetWarning::SameWordPress {
            sites: vec!["one".into(), "two".into()],
            host: "10.0.0.5".into(),
            port: None,
            user: None,
            proxy_jump: Some("deploy@bastion:2222".into()),
            ssh_config_file: Some("~/.ssh/apvm_config".into()),
            path: "/var/www/html".into(),
        }]
    );
    assert_eq!(
        resolved.warnings[0].to_string(),
        "sites 'one', 'two' are the same WordPress install: host '10.0.0.5', jump hosts \
         'deploy@bastion:2222', ssh_config '~/.ssh/apvm_config', path '/var/www/html'; apvm \
         works on it once per site"
    );
}

/// A change to a server and its site, for a table row.
type Change = fn(&mut Server, &mut Site);

#[test]
fn a_different_port_user_host_or_path_is_another_install() {
    // Each row changes one part of the second site's connection or path.
    let rows: [(&str, Change); 9] = [
        ("port", |server, _| server.ssh.port = Some(2222)),
        ("user", |server, _| server.ssh.user = Some("ubuntu".into())),
        ("host", |server, _| server.host = "eu.example.org".into()),
        // ssh matches ssh_config `Host` patterns case-sensitively on the host
        // as given (verified with `ssh -G`, OpenSSH 10.0 and 10.3).
        ("host case", |server, _| {
            server.host = "EU.example.com".into()
        }),
        // The same name behind another jump host, or through another
        // ssh_config file, may be another machine.
        ("jump host", |server, _| {
            server.ssh.proxy_jump = Some("bastion".into());
        }),
        ("ssh_config file", |server, _| {
            server.ssh.ssh_config_file = Some("~/.ssh/eu_config".into());
        }),
        ("path", |_, site| site.path = "/var/www/shop2".into()),
        // Paths are case-sensitive on the servers apvm supports.
        ("path case", |_, site| site.path = "/var/www/Shop".into()),
        // A trailing dot is another name for ssh: no guessing.
        ("host spelling", |server, _| {
            server.host = "eu.example.com.".into()
        }),
    ];
    for (what, change) in rows {
        let mut inventory = Inventory::new();
        inventory
            .add_server(Server::new("a", "eu.example.com"))
            .unwrap();
        inventory
            .add_site(Site::new("one", "a", "/var/www/shop"))
            .unwrap();
        let mut server = Server::new("b", "eu.example.com");
        let mut site = Site::new("two", "b", "/var/www/shop");
        change(&mut server, &mut site);
        inventory.add_server(server).unwrap();
        inventory.add_site(site).unwrap();
        let resolved = inventory.resolve_targets(&Targets::all()).unwrap();
        assert!(
            resolved.warnings.is_empty(),
            "{what}: {:?}",
            resolved.warnings
        );
    }
}

#[test]
fn unset_port_and_user_compare_as_unset() {
    // Plan §4.2 compares what apvm passes: an unset port is not port 22,
    // since ssh_config may set another one for that host.
    let mut inventory = Inventory::new();
    inventory.add_server(Server::new("a", "h")).unwrap();
    let mut b = Server::new("b", "h");
    b.ssh.port = Some(22);
    inventory.add_server(b).unwrap();
    inventory.add_server(Server::new("c", "h")).unwrap();
    for (id, server) in [("one", "a"), ("two", "b"), ("three", "c")] {
        inventory.add_site(Site::new(id, server, "/p")).unwrap();
    }
    let resolved = inventory.resolve_targets(&Targets::all()).unwrap();
    assert_eq!(
        resolved.warnings,
        [TargetWarning::SameWordPress {
            sites: vec!["one".into(), "three".into()],
            host: "h".into(),
            port: None,
            user: None,
            proxy_jump: None,
            ssh_config_file: None,
            path: "/p".into(),
        }]
    );
    assert_eq!(
        resolved.warnings[0].to_string(),
        "sites 'one', 'three' are the same WordPress install: host 'h', path '/p'; apvm works on \
         it once per site"
    );
}

#[test]
fn several_installs_warn_in_order_after_empty_parts() {
    let mut inventory = Inventory::new();
    inventory.add_server(Server::new("a", "h")).unwrap();
    inventory.add_server(Server::new("spare", "s")).unwrap();
    for (id, path) in [
        ("p1", "/one"),
        ("q1", "/two"),
        ("p2", "/one"),
        ("q2", "/two"),
    ] {
        inventory.add_site(Site::new(id, "a", path)).unwrap();
    }
    let targets = Targets::all().with_servers(["spare"]);
    let resolved = inventory.resolve_targets(&targets).unwrap();
    let kinds: Vec<String> = resolved
        .warnings
        .iter()
        .map(|warning| match warning {
            TargetWarning::NoSites { id, .. } => format!("empty {id}"),
            TargetWarning::SameWordPress { sites, .. } => sites.join("+"),
        })
        .collect();
    assert_eq!(kinds, ["empty spare", "p1+p2", "q1+q2"]);
}

/// Every subset of `items`, as lists in the given order.
fn subsets<'a>(items: &[&'a str]) -> Vec<Vec<&'a str>> {
    (0..1u32 << items.len())
        .map(|mask| {
            items
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, item)| *item)
                .collect()
        })
        .collect()
}

/// The model of the sample, written out independently of the inventory's
/// lookups: site order, and the members of each group and server.
const ORDER: [&str; 5] = ["shop", "blog", "api", "docs", "lab"];
const GROUPS: [(&str, &[&str]); 4] = [
    ("qa", &["api", "shop"]),
    ("blogs", &["blog"]),
    ("web", &["shop", "blog"]),
    ("none", &[]),
];
const SERVERS: [(&str, &[&str]); 3] = [
    ("eu-1", &["shop", "blog", "lab"]),
    ("us-1", &["api", "docs"]),
    ("spare", &[]),
];

/// The members of `id` in a model table.
fn members(table: &[(&str, &'static [&'static str])], id: &str) -> &'static [&'static str] {
    table
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, sites)| *sites)
        .unwrap()
}

/// What the model expects for one selection: the ids, in inventory order,
/// and a `NoSites` warning per distinct group, then server, without sites.
fn model(
    sites: &[&str],
    groups: &[&str],
    servers: &[&str],
    all: bool,
) -> (Vec<&'static str>, Vec<TargetWarning>) {
    let mut union: HashSet<&str> = sites.iter().copied().collect();
    let mut warnings = Vec::new();
    let parts = [
        (EntityKind::Group, &GROUPS[..], groups),
        (EntityKind::Server, &SERVERS[..], servers),
    ];
    for (kind, table, named) in parts {
        for id in named {
            let found = members(table, id);
            union.extend(found.iter().copied());
            let warning = TargetWarning::NoSites {
                kind,
                id: id.to_string(),
            };
            if found.is_empty() && !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
    }
    if all {
        union.extend(ORDER);
    }
    let ids = ORDER.into_iter().filter(|id| union.contains(id)).collect();
    (ids, warnings)
}

#[test]
fn every_selection_matches_the_union_model() {
    // Exhaustive over the sample: every combination of sites, groups,
    // servers and all-sites (8192) resolves to the union of what each part
    // names, in inventory order, with a warning per part without sites; an
    // empty union is an error that says why.
    let inventory = sample();
    let site_ids = ["lab", "shop", "api", "docs", "blog"];
    let group_ids = ["qa", "web", "blogs", "none"];
    let server_ids = ["us-1", "eu-1", "spare"];
    for sites in subsets(&site_ids) {
        for groups in subsets(&group_ids) {
            for servers in subsets(&server_ids) {
                for all in [false, true] {
                    let (ids, warnings) = model(&sites, &groups, &servers, all);
                    let targets = Targets::new()
                        .with_sites(sites.clone())
                        .with_groups(groups.clone())
                        .with_servers(servers.clone())
                        .with_all_sites(all);
                    match inventory.resolve_targets(&targets) {
                        Ok(resolved) => {
                            assert_eq!(resolved.ids().collect::<Vec<_>>(), ids, "{targets:?}");
                            assert_eq!(resolved.warnings, warnings, "{targets:?}");
                        }
                        Err(error) => {
                            assert!(ids.is_empty(), "{targets:?}: {error}");
                            let first = error.problems().first().map(|p| p.message.as_str());
                            let why = if targets.is_empty() {
                                NOTHING_NAMED
                            } else {
                                "no site selected: the selection matches no site"
                            };
                            assert_eq!(first, Some(why), "{targets:?}");
                        }
                    }
                }
            }
        }
    }
}
