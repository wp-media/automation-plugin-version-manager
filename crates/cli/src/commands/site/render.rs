//! How `apvm site` shows the inventory: tables and settings for people,
//! documents for `--json` (records as the inventory stores them).

use apvm_sites::{Inventory, ResolvedSite, Setting, Site};
use serde::Serialize;

use crate::commands::inventory::{Failure, destination, json_document, table};

/// What `list` prints when the inventory has no sites.
const NO_SITES: &str =
    "No sites yet. Add one with: apvm site add <ID> --server <SERVER> --path <WP_ROOT>\n";

/// What `list` prints when the filters match no site.
const NO_MATCH: &str = "No site matches the filters.\n";

/// `site list`: the sites as a table, or as a `--json` document.
///
/// # Arguments
///
/// * `inventory` - The inventory the sites are in (for their groups)
/// * `sites` - The sites to list, in inventory order
/// * `filtered` - Whether filters chose them (for the empty message)
/// * `json` - Whether to render JSON
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub(super) fn list_view(
    inventory: &Inventory,
    sites: &[&Site],
    filtered: bool,
    json: bool,
) -> Result<String, Failure> {
    if json {
        let sites = sites
            .iter()
            .map(|site| SiteJson::new(site, inventory))
            .collect();
        return json_document(&ListJson { sites });
    }
    Ok(list_text(sites, filtered))
}

/// `site show`: one site with its effective settings, as text or as a
/// `--json` document.
///
/// # Arguments
///
/// * `site` - The site record
/// * `inventory` - The inventory it is in (for its groups)
/// * `effective` - Its effective settings
/// * `json` - Whether to render JSON
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub(super) fn show_view(
    site: &Site,
    inventory: &Inventory,
    effective: &ResolvedSite,
    json: bool,
) -> Result<String, Failure> {
    let site = SiteJson::new(site, inventory);
    if json {
        return json_document(&ShowJson { site, effective });
    }
    Ok(show_text(&site, effective))
}

/// `--json` body of `site list`.
#[derive(Serialize)]
struct ListJson<'a> {
    /// The listed sites, in inventory order.
    sites: Vec<SiteJson<'a>>,
}

/// `--json` body of `site show`.
#[derive(Serialize)]
struct ShowJson<'a> {
    /// The site record and its groups.
    site: SiteJson<'a>,
    /// Everything apvm uses for the site (`ResolvedSite`). Plain values:
    /// `id`, `path`, `url` (or `null`), `command_timeout_secs` (a number,
    /// or `null`: each operation's default), and the server's `id` and
    /// `host`. `{value, source}`: every execution setting and every SSH
    /// setting, except SSH fields nobody sets, which are `null` (left to
    /// ssh_config).
    effective: &'a ResolvedSite,
}

/// A site in `--json` output: its record, as the inventory stores it,
/// plus the ids of its groups.
#[derive(Serialize)]
struct SiteJson<'a> {
    /// The site record.
    #[serde(flatten)]
    record: &'a Site,
    /// Ids of the groups that list it, in inventory order.
    groups: Vec<&'a str>,
}

impl<'a> SiteJson<'a> {
    /// `site` with its groups from `inventory`.
    fn new(site: &'a Site, inventory: &'a Inventory) -> Self {
        Self {
            record: site,
            groups: inventory
                .groups_of(&site.id)
                .map(|group| group.id.as_str())
                .collect(),
        }
    }
}

/// `site list` as a table: id, server, path, URL, name.
fn list_text(sites: &[&Site], filtered: bool) -> String {
    if sites.is_empty() {
        return if filtered { NO_MATCH } else { NO_SITES }.to_string();
    }
    let header = ["ID", "SERVER", "PATH", "URL", "NAME"]
        .map(String::from)
        .to_vec();
    let rows = sites.iter().map(|site| {
        vec![
            site.id.clone(),
            site.server.clone(),
            site.path.clone(),
            site.url.clone().unwrap_or_default(),
            site.name.clone().unwrap_or_default(),
        ]
    });
    table("", &std::iter::once(header).chain(rows).collect::<Vec<_>>())
}

/// `site show` as text: the record, then each effective setting with
/// where it comes from.
fn show_text(site: &SiteJson<'_>, effective: &ResolvedSite) -> String {
    format!(
        "Site '{}'\n{}\nEffective settings\n{}",
        site.record.id,
        table("  ", &record_rows(site, effective)),
        table("  ", &effective_rows(effective))
    )
}

/// The record's rows: name and notes when set, the server with where ssh
/// connects, the path, the URL when set, and the groups.
fn record_rows(site: &SiteJson<'_>, effective: &ResolvedSite) -> Vec<Vec<String>> {
    let record = site.record;
    let server = &effective.server;
    let groups = if site.groups.is_empty() {
        "none".to_string()
    } else {
        site.groups.join(", ")
    };
    let rows = [
        ("name", record.name.clone()),
        (
            "server",
            Some(format!(
                "{} ({})",
                server.id,
                destination(&server.host, &server.ssh)
            )),
        ),
        ("path", Some(record.path.clone())),
        ("url", record.url.clone()),
        ("groups", Some(groups)),
        ("notes", record.notes.clone()),
    ];
    rows.into_iter()
        .filter_map(|(label, value)| Some(vec![label.to_string(), value?]))
        .collect()
}

/// Column titles of the effective settings.
const EFFECTIVE_HEADER: [&str; 3] = ["SETTING", "VALUE", "FROM"];

/// What an unset command timeout says applies instead.
const OPERATION_DEFAULT: &str = "each operation's default";

/// The effective execution settings as `label, value, source` rows, under
/// [`EFFECTIVE_HEADER`].
fn effective_rows(site: &ResolvedSite) -> Vec<Vec<String>> {
    let timeout = match site.command_timeout_secs {
        Some(secs) => vec!["timeout".into(), format!("{secs} s"), "site".into()],
        None => vec!["timeout".into(), "not set".into(), OPERATION_DEFAULT.into()],
    };
    vec![
        EFFECTIVE_HEADER.map(String::from).to_vec(),
        row("wp-cli", &site.wp_cli, |argv| argv.join(" ")),
        row("run as", &site.run_as, ToString::to_string),
        row("owner", &site.owner, ToString::to_string),
        row("ownership", &site.ownership, ToString::to_string),
        row("plugins dir", &site.plugins_dir, Clone::clone),
        row("allow root", &site.allow_root, |allow| yes_no(*allow)),
        row("tmp dir", &site.remote_tmp_dir, Clone::clone),
        timeout,
    ]
}

/// A row of an effective setting: label, value as text, where it comes
/// from.
fn row<T>(label: &str, setting: &Setting<T>, text: impl Fn(&T) -> String) -> Vec<String> {
    vec![
        label.to_string(),
        text(&setting.value),
        setting.source.to_string(),
    ]
}

/// `yes` / `no`.
fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apvm_sites::{Group, OwnershipPolicy, RunAs, Server};

    /// `eu-1` (user `ubuntu`, port 2222) with `shop` (every field set, in
    /// groups `qa` and `prod`) and `blog` (nothing optional set).
    fn sample() -> Inventory {
        let mut inventory = Inventory::new();
        let mut eu = Server::new("eu-1", "203.0.113.10");
        eu.ssh.user = Some("ubuntu".into());
        eu.ssh.port = Some(2222);
        inventory.add_server(eu).expect("valid server");
        let mut shop = Site::new("shop", "eu-1", "/var/www/shop");
        shop.name = Some("Shop".into());
        shop.url = Some("https://shop.example.com".into());
        shop.wp_cli = Some(vec!["/usr/bin/php8.3".into(), "/opt/wp-cli.phar".into()]);
        shop.run_as = Some(RunAs::Login);
        shop.ownership = Some(OwnershipPolicy::Check);
        shop.command_timeout_secs = Some(600);
        shop.notes = Some("main store".into());
        inventory.add_site(shop).expect("valid site");
        inventory
            .add_site(Site::new("blog", "eu-1", "/var/www/blog"))
            .expect("valid site");
        for id in ["qa", "prod"] {
            inventory
                .add_group(Group::new(id, vec!["shop".into()]))
                .expect("valid group");
        }
        inventory
    }

    /// The sites of `inventory`, all of them.
    fn all(inventory: &Inventory) -> Vec<&Site> {
        inventory.sites().iter().collect()
    }

    /// Parse a `--json` document.
    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("valid JSON")
    }

    /// `show_view` of `id` in `inventory`.
    fn show(inventory: &Inventory, id: &str, as_json: bool) -> String {
        let site = inventory.site(id).expect("known");
        let effective = inventory.resolve_site(id).expect("resolves");
        show_view(site, inventory, &effective, as_json).expect("renders")
    }

    #[test]
    fn list_says_how_to_start_or_that_nothing_matched() {
        let empty = Inventory::new();
        assert_eq!(
            list_view(&empty, &[], false, false).expect("renders"),
            NO_SITES
        );
        assert_eq!(
            list_view(&empty, &[], true, false).expect("renders"),
            NO_MATCH
        );
        assert_eq!(
            json(&list_view(&empty, &[], true, true).expect("renders")),
            serde_json::json!({"schema": 1, "sites": []})
        );
    }

    #[test]
    fn list_aligns_one_row_per_site() {
        let inventory = sample();
        assert_eq!(
            list_view(&inventory, &all(&inventory), false, false).expect("renders"),
            "ID    SERVER  PATH           URL                       NAME\n\
             shop  eu-1    /var/www/shop  https://shop.example.com  Shop\n\
             blog  eu-1    /var/www/blog\n"
        );
    }

    #[test]
    fn list_json_holds_each_sparse_record_and_its_groups() {
        let inventory = sample();
        let doc = json(&list_view(&inventory, &all(&inventory), false, true).expect("renders"));
        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["sites"][0]["groups"], serde_json::json!(["qa", "prod"]));
        assert_eq!(doc["sites"][0]["run_as"], "login");
        assert_eq!(doc["sites"][0]["wp_cli"][1], "/opt/wp-cli.phar");
        assert_eq!(
            doc["sites"][1],
            serde_json::json!({"id": "blog", "server": "eu-1", "path": "/var/www/blog", "groups": []})
        );
    }

    #[test]
    fn show_lists_the_record_then_every_effective_setting_and_its_source() {
        assert_eq!(
            show(&sample(), "shop", false),
            "Site 'shop'\n\
             \x20 name    Shop\n\
             \x20 server  eu-1 (ubuntu@203.0.113.10:2222)\n\
             \x20 path    /var/www/shop\n\
             \x20 url     https://shop.example.com\n\
             \x20 groups  qa, prod\n\
             \x20 notes   main store\n\
             \n\
             Effective settings\n\
             \x20 SETTING      VALUE                             FROM\n\
             \x20 wp-cli       /usr/bin/php8.3 /opt/wp-cli.phar  site\n\
             \x20 run as       login                             site\n\
             \x20 owner        auto                              built-in\n\
             \x20 ownership    check                             site\n\
             \x20 plugins dir  /var/www/shop/wp-content/plugins  built-in\n\
             \x20 allow root   no                                built-in\n\
             \x20 tmp dir      /tmp                              built-in\n\
             \x20 timeout      600 s                             site\n"
        );
    }

    #[test]
    fn show_omits_unset_record_fields_and_names_the_built_ins() {
        let text = show(&sample(), "blog", false);
        assert!(
            text.starts_with("Site 'blog'\n  server  eu-1 (ubuntu@203.0.113.10:2222)\n"),
            "{text}"
        );
        assert!(text.contains("\n  groups  none\n"), "{text}");
        for absent in ["name", "url", "notes"] {
            assert!(!text.contains(&format!("  {absent} ")), "{absent}: {text}");
        }
        assert!(text.contains("  wp-cli       wp "), "{text}");
        assert!(
            text.ends_with(
                "  timeout      not set                           each operation's default\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn show_json_holds_the_record_its_groups_and_every_effective_setting() {
        let doc = json(&show(&sample(), "blog", true));
        assert_eq!(doc["schema"], 1);
        assert_eq!(
            doc["site"],
            serde_json::json!({"id": "blog", "server": "eu-1", "path": "/var/www/blog", "groups": []})
        );
        let effective = &doc["effective"];
        assert_eq!(
            effective["wp_cli"],
            serde_json::json!({"value": ["wp"], "source": "built-in"})
        );
        assert_eq!(
            effective["run_as"],
            serde_json::json!({"value": "auto", "source": "built-in"})
        );
        assert_eq!(
            effective["ownership"],
            serde_json::json!({"value": "mirror", "source": "built-in"})
        );
        assert_eq!(effective["command_timeout_secs"], serde_json::Value::Null);
        assert_eq!(effective["server"]["id"], "eu-1");
        assert_eq!(
            effective["server"]["ssh"]["port"],
            serde_json::json!({"value": 2222, "source": "server"})
        );
    }

    #[test]
    fn booleans_read_as_words() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }
}
