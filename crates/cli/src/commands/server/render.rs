//! How `apvm server` shows the inventory: tables and settings for people,
//! documents for `--json` (records as the inventory stores them).

use apvm_sites::{Defaults, Inventory, ResolvedSsh, Server, Setting, SshSettings};
use serde::Serialize;

use crate::commands::inventory::{Failure, destination, json_document, table};

/// `server defaults`: the defaults as text, or as a `--json` document.
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub(super) fn defaults_view(defaults: &Defaults, json: bool) -> Result<String, Failure> {
    if json {
        return json_document(&DefaultsJson { defaults });
    }
    Ok(defaults_text(&defaults.ssh))
}

/// `server list`: every server as a table, or as a `--json` document.
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub(super) fn list_view(inventory: &Inventory, json: bool) -> Result<String, Failure> {
    if json {
        let servers = inventory
            .servers()
            .iter()
            .map(|server| ServerJson::new(server, inventory))
            .collect();
        return json_document(&ListJson { servers });
    }
    list_text(inventory)
}

/// `server show`: one server with its effective SSH settings, as text or
/// as a `--json` document.
///
/// # Arguments
///
/// * `server` - The server record
/// * `inventory` - The inventory it is in (for its sites)
/// * `effective` - Its effective SSH settings
/// * `json` - Whether to render JSON
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected).
pub(super) fn show_view(
    server: &Server,
    inventory: &Inventory,
    effective: &ResolvedSsh,
    json: bool,
) -> Result<String, Failure> {
    let server = ServerJson::new(server, inventory);
    if json {
        return json_document(&ShowJson { server, effective });
    }
    Ok(show_text(&server, effective))
}

/// `--json` body of `server defaults`.
#[derive(Serialize)]
struct DefaultsJson<'a> {
    /// The defaults record, as the inventory stores it (unset fields
    /// omitted).
    defaults: &'a Defaults,
}

/// `--json` body of `server list`.
#[derive(Serialize)]
struct ListJson<'a> {
    /// Every server, in inventory order.
    servers: Vec<ServerJson<'a>>,
}

/// `--json` body of `server show`.
#[derive(Serialize)]
struct ShowJson<'a> {
    /// The server record and its sites.
    server: ServerJson<'a>,
    /// The effective SSH settings, each `{value, source}` (or `null`: left
    /// to ssh_config).
    effective: &'a ResolvedSsh,
}

/// A server in `--json` output: its record, as the inventory stores it,
/// plus the ids of its sites.
#[derive(Serialize)]
struct ServerJson<'a> {
    /// The server record.
    #[serde(flatten)]
    record: &'a Server,
    /// Ids of the sites on it, in inventory order.
    sites: Vec<&'a str>,
}

impl<'a> ServerJson<'a> {
    /// `server` with its sites from `inventory`.
    fn new(server: &'a Server, inventory: &'a Inventory) -> Self {
        Self {
            record: server,
            sites: inventory
                .sites_on(&server.id)
                .map(|site| site.id.as_str())
                .collect(),
        }
    }
}

/// `server list` as a table: id, ssh destination, site count, name.
///
/// # Errors
///
/// Those of `Inventory::resolve_server` (not expected: every listed
/// server exists).
fn list_text(inventory: &Inventory) -> Result<String, Failure> {
    if inventory.servers().is_empty() {
        return Ok("No servers yet. Add one with: apvm server add <ID> --host <HOST>\n".into());
    }
    let mut rows = vec![vec![
        "ID".to_string(),
        "DESTINATION".to_string(),
        "SITES".to_string(),
        "NAME".to_string(),
    ]];
    for server in inventory.servers() {
        let ssh = inventory.resolve_server(&server.id)?.ssh;
        rows.push(vec![
            server.id.clone(),
            destination(&server.host, &ssh),
            inventory.sites_on(&server.id).count().to_string(),
            server.name.clone().unwrap_or_default(),
        ]);
    }
    Ok(table("", &rows))
}

/// `server show` as text: the record, then each effective SSH setting
/// with where it comes from.
fn show_text(server: &ServerJson<'_>, effective: &ResolvedSsh) -> String {
    let record = server.record;
    let mut rows = Vec::new();
    if let Some(name) = &record.name {
        rows.push(vec!["name".to_string(), name.clone()]);
    }
    rows.push(vec!["host".to_string(), record.host.clone()]);
    let sites = if server.sites.is_empty() {
        "none".to_string()
    } else {
        server.sites.join(", ")
    };
    rows.push(vec!["sites".to_string(), sites]);
    if let Some(notes) = &record.notes {
        rows.push(vec!["notes".to_string(), notes.clone()]);
    }
    format!(
        "Server '{}'\n{}\nEffective SSH settings\n{}",
        record.id,
        table("  ", &rows),
        table("  ", &effective_rows(effective))
    )
}

/// Column titles of the effective settings.
const EFFECTIVE_HEADER: [&str; 3] = ["SETTING", "VALUE", "FROM"];

/// The effective SSH settings as `label, value, source` rows, under
/// [`EFFECTIVE_HEADER`].
fn effective_rows(ssh: &ResolvedSsh) -> Vec<Vec<String>> {
    vec![
        EFFECTIVE_HEADER.map(String::from).to_vec(),
        optional("user", ssh.user.clone()),
        optional(
            "port",
            ssh.port.as_ref().map(|port| as_text(port, u16::to_string)),
        ),
        optional("key", ssh.identity_file.clone()),
        given(
            "connect timeout",
            as_text(&ssh.connect_timeout_secs, |secs| seconds(*secs)),
        ),
        optional("known hosts", ssh.known_hosts_file.clone()),
        optional("ssh config", ssh.ssh_config_file.clone()),
        optional("jump hosts", ssh.proxy_jump.clone()),
        given("multiplex", as_text(&ssh.multiplex, |on| on_off(*on))),
        given("ssh program", ssh.binary.clone()),
    ]
}

/// What an effective setting nobody sets says it is left to.
const LEFT_TO_SSH_CONFIG: &str = "left to ssh_config";

/// A row of an effective setting that may be unset (then left to
/// ssh_config).
fn optional(label: &str, setting: Option<Setting<String>>) -> Vec<String> {
    match setting {
        Some(setting) => given(label, setting),
        None => vec![
            label.to_string(),
            "not set".to_string(),
            LEFT_TO_SSH_CONFIG.to_string(),
        ],
    }
}

/// A row of an effective setting: label, value, where it comes from.
fn given(label: &str, setting: Setting<String>) -> Vec<String> {
    vec![label.to_string(), setting.value, setting.source.to_string()]
}

/// A setting with its value turned into text.
fn as_text<T>(setting: &Setting<T>, text: impl Fn(&T) -> String) -> Setting<String> {
    Setting::new(text(&setting.value), setting.source)
}

/// `server defaults` as text: every field, unset ones with what applies
/// instead.
fn defaults_text(ssh: &SshSettings) -> String {
    let (left, [timeout, multiplex, program]) = (LEFT_TO_SSH_CONFIG, built_ins());
    let rows = [
        ("user", ssh.user.clone(), left),
        ("port", ssh.port.map(|port| port.to_string()), left),
        ("key", ssh.identity_file.clone(), left),
        (
            "connect timeout",
            ssh.connect_timeout_secs.map(seconds),
            &timeout,
        ),
        ("known hosts", ssh.known_hosts_file.clone(), left),
        ("ssh config", ssh.ssh_config_file.clone(), left),
        ("jump hosts", ssh.proxy_jump.clone(), left),
        ("multiplex", ssh.multiplex.map(on_off), &multiplex),
        ("ssh program", ssh.binary.clone(), &program),
    ]
    .map(|(label, value, otherwise)| default_row(label, value, otherwise));
    format!(
        "SSH defaults (every server inherits them unless it sets its own)\n{}",
        table("  ", &rows)
    )
}

/// What applies when no default is set: `built-in: …` for the connect
/// timeout, multiplexing and the ssh program.
fn built_ins() -> [String; 3] {
    use apvm_sites::inventory::{
        BUILTIN_CONNECT_TIMEOUT_SECS, BUILTIN_MULTIPLEX, BUILTIN_SSH_BINARY,
    };
    [
        built_in(seconds(BUILTIN_CONNECT_TIMEOUT_SECS)),
        built_in(on_off(BUILTIN_MULTIPLEX)),
        built_in(BUILTIN_SSH_BINARY.to_string()),
    ]
}

/// A row of `server defaults`: the label and the value, or `not set
/// (<otherwise>)`.
fn default_row(label: &str, value: Option<String>, otherwise: &str) -> Vec<String> {
    let value = value.unwrap_or_else(|| format!("not set ({otherwise})"));
    vec![label.to_string(), value]
}

/// `built-in: <value>`.
fn built_in(value: String) -> String {
    format!("built-in: {value}")
}

/// `15 s`.
fn seconds(secs: u32) -> String {
    format!("{secs} s")
}

/// `on` / `off`.
fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apvm_sites::{Group, Site};

    /// The §7 example's servers, with defaults and two sites on `eu-1`.
    fn sample() -> Inventory {
        let mut inventory = Inventory::new();
        let mut defaults = Defaults::default();
        defaults.ssh.user = Some("deploy".into());
        defaults.ssh.identity_file = Some("~/.ssh/id_ed25519".into());
        defaults.ssh.connect_timeout_secs = Some(20);
        inventory.set_defaults(defaults).expect("valid defaults");
        let mut eu = Server::new("eu-1", "203.0.113.10");
        eu.name = Some("EU VPS".into());
        eu.notes = Some("primary".into());
        eu.ssh.user = Some("ubuntu".into());
        eu.ssh.port = Some(2222);
        inventory.add_server(eu).expect("valid server");
        inventory
            .add_server(Server::new("staging-box", "staging"))
            .expect("valid server");
        for id in ["shop", "blog"] {
            let path = format!("/var/www/{id}");
            inventory
                .add_site(Site::new(id, "eu-1", path))
                .expect("valid site");
        }
        inventory
            .add_group(Group::new("qa", vec!["shop".into()]))
            .expect("valid group");
        inventory
    }

    /// The effective SSH settings of `id` in `inventory`.
    fn effective(inventory: &Inventory, id: &str) -> ResolvedSsh {
        inventory.resolve_server(id).expect("known server").ssh
    }

    /// Parse a `--json` document.
    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("valid JSON")
    }

    #[test]
    fn destination_shows_the_effective_user_and_port() {
        let inventory = sample();
        assert_eq!(
            destination("203.0.113.10", &effective(&inventory, "eu-1")),
            "ubuntu@203.0.113.10:2222"
        );
        assert_eq!(
            destination("staging", &effective(&inventory, "staging-box")),
            "deploy@staging"
        );
        assert_eq!(
            destination("2001:db8::1", &effective(&inventory, "eu-1")),
            "ubuntu@[2001:db8::1]:2222"
        );
        let mut bare = Inventory::new();
        bare.add_server(Server::new("h", "h"))
            .expect("valid server");
        assert_eq!(destination("h", &effective(&bare, "h")), "h");
    }

    #[test]
    fn list_shows_a_hint_when_there_are_no_servers() {
        assert_eq!(
            list_view(&Inventory::new(), false).expect("renders"),
            "No servers yet. Add one with: apvm server add <ID> --host <HOST>\n"
        );
    }

    #[test]
    fn list_aligns_one_row_per_server_in_inventory_order() {
        assert_eq!(
            list_view(&sample(), false).expect("renders"),
            "ID           DESTINATION               SITES  NAME\n\
             eu-1         ubuntu@203.0.113.10:2222  2      EU VPS\n\
             staging-box  deploy@staging            0\n"
        );
    }

    #[test]
    fn list_json_holds_each_record_and_its_sites() {
        let doc = json(&list_view(&sample(), true).expect("renders"));
        assert_eq!(doc["schema"], 1);
        let servers = doc["servers"].as_array().expect("list");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0]["id"], "eu-1");
        assert_eq!(servers[0]["ssh"]["port"], 2222);
        assert_eq!(servers[0]["sites"], serde_json::json!(["shop", "blog"]));
        // Records are sparse, as the inventory stores them.
        assert_eq!(
            servers[1],
            serde_json::json!({"id": "staging-box", "host": "staging", "sites": []})
        );
        assert_eq!(
            json(&list_view(&Inventory::new(), true).expect("renders")),
            serde_json::json!({"schema": 1, "servers": []})
        );
    }

    #[test]
    fn show_lists_the_record_then_every_effective_setting_and_its_source() {
        let inventory = sample();
        let server = inventory.server("eu-1").expect("known");
        let text =
            show_view(server, &inventory, &effective(&inventory, "eu-1"), false).expect("renders");
        assert_eq!(
            text,
            "Server 'eu-1'\n\
             \x20 name   EU VPS\n\
             \x20 host   203.0.113.10\n\
             \x20 sites  shop, blog\n\
             \x20 notes  primary\n\
             \n\
             Effective SSH settings\n\
             \x20 SETTING          VALUE              FROM\n\
             \x20 user             ubuntu             server\n\
             \x20 port             2222               server\n\
             \x20 key              ~/.ssh/id_ed25519  defaults\n\
             \x20 connect timeout  20 s               defaults\n\
             \x20 known hosts      not set            left to ssh_config\n\
             \x20 ssh config       not set            left to ssh_config\n\
             \x20 jump hosts       not set            left to ssh_config\n\
             \x20 multiplex        on                 built-in\n\
             \x20 ssh program      ssh                built-in\n"
        );
    }

    #[test]
    fn show_says_none_for_a_server_without_sites() {
        let inventory = sample();
        let server = inventory.server("staging-box").expect("known");
        let text = show_view(
            server,
            &inventory,
            &effective(&inventory, "staging-box"),
            false,
        )
        .expect("renders");
        assert!(text.contains("\n  sites  none\n"), "{text}");
        assert!(!text.contains("name "), "no name line: {text}");
        assert!(!text.contains("notes"), "no notes line: {text}");
    }

    #[test]
    fn show_json_holds_the_record_its_sites_and_the_effective_settings() {
        let inventory = sample();
        let server = inventory.server("staging-box").expect("known");
        let doc = json(
            &show_view(
                server,
                &inventory,
                &effective(&inventory, "staging-box"),
                true,
            )
            .expect("renders"),
        );
        assert_eq!(doc["schema"], 1);
        assert_eq!(
            doc["server"],
            serde_json::json!({"id": "staging-box", "host": "staging", "sites": []})
        );
        let ssh = &doc["effective"];
        assert_eq!(
            ssh["user"],
            serde_json::json!({"value": "deploy", "source": "defaults"})
        );
        assert_eq!(ssh["port"], serde_json::Value::Null);
        assert_eq!(
            ssh["connect_timeout_secs"],
            serde_json::json!({"value": 20, "source": "defaults"})
        );
        assert_eq!(
            ssh["multiplex"],
            serde_json::json!({"value": true, "source": "built-in"})
        );
        assert_eq!(
            ssh["binary"],
            serde_json::json!({"value": "ssh", "source": "built-in"})
        );
    }

    #[test]
    fn defaults_show_every_field_and_what_applies_when_unset() {
        assert_eq!(
            defaults_view(&Defaults::default(), false).expect("renders"),
            "SSH defaults (every server inherits them unless it sets its own)\n\
             \x20 user             not set (left to ssh_config)\n\
             \x20 port             not set (left to ssh_config)\n\
             \x20 key              not set (left to ssh_config)\n\
             \x20 connect timeout  not set (built-in: 15 s)\n\
             \x20 known hosts      not set (left to ssh_config)\n\
             \x20 ssh config       not set (left to ssh_config)\n\
             \x20 jump hosts       not set (left to ssh_config)\n\
             \x20 multiplex        not set (built-in: on)\n\
             \x20 ssh program      not set (built-in: ssh)\n"
        );
        let mut defaults = Defaults::default();
        defaults.ssh.port = Some(2200);
        defaults.ssh.multiplex = Some(false);
        defaults.ssh.binary = Some("/usr/bin/ssh".into());
        defaults.ssh.proxy_jump = Some("bastion".into());
        let text = defaults_view(&defaults, false).expect("renders");
        for line in [
            "  port             2200\n",
            "  multiplex        off\n",
            "  ssh program      /usr/bin/ssh\n",
            "  jump hosts       bastion\n",
        ] {
            assert!(text.contains(line), "{line:?} in {text}");
        }
    }

    #[test]
    fn defaults_json_is_the_sparse_record() {
        assert_eq!(
            json(&defaults_view(&Defaults::default(), true).expect("renders")),
            serde_json::json!({"schema": 1, "defaults": {}})
        );
        let doc = json(&defaults_view(sample().defaults(), true).expect("renders"));
        assert_eq!(
            doc["defaults"],
            serde_json::json!({"ssh": {
                "user": "deploy",
                "identity_file": "~/.ssh/id_ed25519",
                "connect_timeout_secs": 20
            }})
        );
    }

    #[test]
    fn values_read_as_units() {
        assert_eq!(seconds(15), "15 s");
        assert_eq!(on_off(true), "on");
        assert_eq!(on_off(false), "off");
        let port = Setting::new(22u16, apvm_sites::SettingSource::Server);
        assert_eq!(
            as_text(&port, u16::to_string),
            Setting::new("22".to_string(), apvm_sites::SettingSource::Server)
        );
    }
}
