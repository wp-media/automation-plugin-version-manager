//! Validation of inventory records and documents.
//!
//! A document is checked as a whole (every record, unique ids, references
//! between records); a record handed to the inventory is checked alone
//! against the records already there. Both report every problem with its
//! location, through the same per-record rules.

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::model::{Defaults, Document, Group, Owner, Record, RunAs, Server, Site, SshSettings};
use crate::allowlist::{self, Rule};
use crate::problem::{FirstSeen, Problem, Scope, quoted};

/// Most characters in a record's `name`.
pub(crate) const NAME_MAX_CHARS: usize = 128;
/// Most characters in a record's `notes`.
pub(crate) const NOTES_MAX_CHARS: usize = 1024;
/// Longest SSH connect timeout, in seconds.
pub(crate) const CONNECT_TIMEOUT_MAX_SECS: u32 = 3600;
/// Longest remote command timeout, in seconds.
pub(crate) const COMMAND_TIMEOUT_MAX_SECS: u32 = 86_400;
/// Fields whose JSON value is a list. Anywhere else, a JSON array stands
/// for a record read by field position.
const LIST_FIELDS: [&str; 4] = ["servers", "sites", "groups", "wp_cli"];

/// Every problem in a document, in document order.
pub(crate) fn document(doc: &Document) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut root = Scope::root(&mut out);
    if doc.version == 0 {
        root.problem("version", "must be at least 1");
    }
    defaults(&doc.defaults, &mut root.child("defaults"));
    let servers = records(&mut root, "servers", &doc.servers, server);
    let sites = records(&mut root, "sites", &doc.sites, |record, scope| {
        site(record, scope, &servers);
    });
    records(&mut root, "groups", &doc.groups, |record, scope| {
        group(record, scope, &sites);
    });
    out
}

/// Records written as JSON arrays instead of objects, sorted by location.
/// serde reads a record from an array by field position, which bypasses
/// field names (and the refusal of unknown fields), so a document holding
/// one is refused. Any array outside [`LIST_FIELDS`], and any array inside
/// one, is such a record.
///
/// # Arguments
///
/// * `root` - The document, already known to parse as an inventory
pub(crate) fn json_shape(root: &Value) -> Vec<Problem> {
    let mut out = Vec::new();
    match root {
        Value::Object(fields) => shape(fields, &mut Scope::root(&mut out)),
        _ => out.push(Problem::new("", "the document must be a JSON object")),
    }
    out.sort_by(|a, b| a.location.cmp(&b.location));
    out
}

/// Walk the fields of a JSON object (see [`json_shape`]).
fn shape(fields: &Map<String, Value>, scope: &mut Scope<'_>) {
    for (name, field) in fields {
        match field {
            Value::Array(items) if LIST_FIELDS.contains(&name.as_str()) => {
                for (index, item) in items.iter().enumerate() {
                    let at = format!("{name}[{index}]");
                    match item {
                        Value::Array(_) => scope.problem(&at, "must be a JSON object"),
                        Value::Object(inner) => shape(inner, &mut scope.child(&at)),
                        _ => {}
                    }
                }
            }
            Value::Array(_) => scope.problem(name, "must be a JSON object"),
            Value::Object(inner) => shape(inner, &mut scope.child(name)),
            _ => {}
        }
    }
}

/// Problems in `defaults` handed to the inventory.
pub(crate) fn defaults_record(record: &Defaults) -> Vec<Problem> {
    let mut out = Vec::new();
    defaults(record, &mut Scope::root(&mut out));
    out
}

/// Problems in a server handed to the inventory.
pub(crate) fn server_record(record: &Server) -> Vec<Problem> {
    let mut out = Vec::new();
    server(record, &mut Scope::root(&mut out));
    out
}

/// Problems in a site handed to the inventory.
///
/// # Arguments
///
/// * `record` - The site
/// * `servers` - Ids of the servers in the inventory
pub(crate) fn site_record(record: &Site, servers: &HashSet<&str>) -> Vec<Problem> {
    let mut out = Vec::new();
    site(record, &mut Scope::root(&mut out), servers);
    out
}

/// Problems in a group handed to the inventory.
///
/// # Arguments
///
/// * `record` - The group
/// * `sites` - Ids of the sites in the inventory
pub(crate) fn group_record(record: &Group, sites: &HashSet<&str>) -> Vec<Problem> {
    let mut out = Vec::new();
    group(record, &mut Scope::root(&mut out), sites);
    out
}

/// Check each record of `items` with `check`, under `list[index]`, and
/// that ids are unique.
///
/// # Arguments
///
/// * `root` - The document's scope
/// * `list` - The list's field name (`servers`, …)
/// * `items` - The records
/// * `check` - The record rules
///
/// # Returns
///
/// The ids of the records, so later lists can check references to them.
fn records<'d, T: Record>(
    root: &mut Scope<'_>,
    list: &str,
    items: &'d [T],
    mut check: impl FnMut(&T, &mut Scope<'_>),
) -> HashSet<&'d str> {
    let mut seen = FirstSeen::default();
    for (index, item) in items.iter().enumerate() {
        let mut scope = root.child(&format!("{list}[{index}]"));
        check(item, &mut scope);
        scope.unique(&mut seen, "id", "id", item.id());
    }
    seen.keys()
}

/// Rules for `defaults`.
fn defaults(record: &Defaults, scope: &mut Scope<'_>) {
    ssh(&record.ssh, &mut scope.child("ssh"), true);
}

/// Rules for SSH settings; `binary` only where `in_defaults`.
fn ssh(record: &SshSettings, scope: &mut Scope<'_>, in_defaults: bool) {
    scope.check_opt("user", record.user.as_deref(), allowlist::user);
    if let Some(port) = record.port {
        scope.check("port", allowlist::port(port));
    }
    scope.check_opt(
        "identity_file",
        record.identity_file.as_deref(),
        allowlist::ssh_path,
    );
    if let Some(secs) = record.connect_timeout_secs {
        let verdict = allowlist::seconds(secs, CONNECT_TIMEOUT_MAX_SECS);
        scope.check("connect_timeout_secs", verdict);
    }
    let known_hosts = record.known_hosts_file.as_deref();
    scope.check_opt("known_hosts_file", known_hosts, allowlist::known_hosts_file);
    let config = record.ssh_config_file.as_deref();
    scope.check_opt("ssh_config_file", config, allowlist::ssh_path);
    scope.check_opt(
        "proxy_jump",
        record.proxy_jump.as_deref(),
        allowlist::proxy_jump,
    );
    scope.check_opt("binary", record.binary.as_deref(), |binary| {
        if in_defaults {
            allowlist::ssh_path(binary)
        } else {
            Err("is only allowed in defaults.ssh".into())
        }
    });
}

/// Rules for a server.
fn server(record: &Server, scope: &mut Scope<'_>) {
    scope.check("id", allowlist::id(&record.id));
    scope.check_opt("name", record.name.as_deref(), name);
    scope.check("host", allowlist::host(&record.host));
    ssh(&record.ssh, &mut scope.child("ssh"), false);
    scope.check_opt("notes", record.notes.as_deref(), notes);
}

/// Rules for a site; `servers` holds the ids it may reference.
fn site(record: &Site, scope: &mut Scope<'_>, servers: &HashSet<&str>) {
    scope.check("id", allowlist::id(&record.id));
    scope.check_opt("name", record.name.as_deref(), name);
    if !servers.contains(record.server.as_str()) {
        let message = format!("unknown server {}", quoted(&record.server));
        scope.problem("server", message);
    }
    scope.check("path", allowlist::remote_path(&record.path));
    scope.check_opt("url", record.url.as_deref(), allowlist::url);
    site_execution(record, scope);
    scope.check_opt("notes", record.notes.as_deref(), notes);
}

/// Rules for how apvm works on a site: the WP-CLI command, the identity,
/// the directories and the timeout.
fn site_execution(record: &Site, scope: &mut Scope<'_>) {
    if let Some(argv) = &record.wp_cli {
        allowlist::wp_cli(argv, scope);
    }
    let allow_root = record.allow_root == Some(true);
    let run_as = |run_as: &RunAs| run_as_rule(run_as, allow_root);
    scope.check_opt("run_as", record.run_as.as_ref(), run_as);
    let owner = |owner: &Owner| owner_rule(owner, allow_root);
    scope.check_opt("owner", record.owner.as_ref(), owner);
    let plugins_dir = record.plugins_dir.as_deref();
    scope.check_opt("plugins_dir", plugins_dir, allowlist::remote_path);
    let tmp = record.remote_tmp_dir.as_deref();
    scope.check_opt("remote_tmp_dir", tmp, allowlist::remote_tmp_dir);
    if let Some(secs) = record.command_timeout_secs {
        let verdict = allowlist::seconds(secs, COMMAND_TIMEOUT_MAX_SECS);
        scope.check("command_timeout_secs", verdict);
    }
}

/// Rules for `run_as`: a valid user that is not a keyword (in any letter
/// case), and root only with `allow_root`.
fn run_as_rule(run_as: &RunAs, allow_root: bool) -> Rule {
    let RunAs::User(user) = run_as else {
        return Ok(());
    };
    if is_keyword(user, "auto") || is_keyword(user, "login") {
        return Err(format!(
            "a user cannot be named {}: 'auto' and 'login' are keywords here{}",
            quoted(user),
            case_note(user)
        ));
    }
    allowlist::user(user)?;
    root_rule(user, allow_root)
}

/// Rules for `owner`: valid user and group names, a user that is not the
/// `auto` keyword (in any letter case), and root (user or group) only with
/// `allow_root`.
fn owner_rule(owner: &Owner, allow_root: bool) -> Rule {
    let Owner::Explicit { user, group } = owner else {
        return Ok(());
    };
    if is_keyword(user, "auto") {
        return Err(auto_owner_problem(user, group.is_some()));
    }
    let user_rule = allowlist::user(user).and_then(|()| root_rule(user, allow_root));
    user_rule.map_err(|reason| format!("user {reason}"))?;
    let Some(group) = group else {
        return Ok(());
    };
    let group_rule = allowlist::user(group).and_then(|()| root_rule(group, allow_root));
    group_rule.map_err(|reason| format!("group {reason}"))
}

/// Whether `name` is `keyword` in any letter case. The keywords are
/// matched like `root`: a user `Auto` or `LOGIN` would read as the keyword
/// to a person, so it is refused rather than run as a distinct account.
fn is_keyword(name: &str, keyword: &str) -> bool {
    name.eq_ignore_ascii_case(keyword)
}

/// What a keyword refusal adds when the name differs from the keyword in
/// letter case only (`Auto`), so the message explains the match.
fn case_note(name: &str) -> &'static str {
    if name.chars().any(|c| c.is_ascii_uppercase()) {
        ", in any letter case"
    } else {
        ""
    }
}

/// Why an explicit owner named `auto` (in any letter case) is refused:
/// with a group it reads as `auto:<group>`, which is no form; alone it is
/// the keyword. The exact keyword keeps its short message.
///
/// # Arguments
///
/// * `user` - The owner's user part
/// * `has_group` - Whether a group follows it
fn auto_owner_problem(user: &str, has_group: bool) -> String {
    let name = if user == "auto" {
        "'auto'".to_string()
    } else {
        format!("{} (the keyword 'auto' in any letter case)", quoted(user))
    };
    if has_group {
        return format!("{name} takes no group: use 'auto', or name the owner as user:group");
    }
    if user == "auto" {
        return "a user cannot be named 'auto': it is the keyword for the plugins directory's owner"
            .into();
    }
    format!(
        "a user cannot be named {}: 'auto', in any letter case, is the keyword for the plugins \
         directory's owner",
        quoted(user)
    )
}

/// Running or owning as root needs the site's explicit `allow_root`
/// (plan §6.2). Names only: aliases of uid 0 are caught where ids resolve.
fn root_rule(name: &str, allow_root: bool) -> Rule {
    if allowlist::is_root(name) && !allow_root {
        return Err(format!("{} needs allow_root: true", quoted(name)));
    }
    Ok(())
}

/// Rules for a group; `sites` holds the ids it may reference.
fn group(record: &Group, scope: &mut Scope<'_>, sites: &HashSet<&str>) {
    scope.check("id", allowlist::id(&record.id));
    let mut listed = HashSet::new();
    for (index, member) in record.sites.iter().enumerate() {
        let field = format!("sites[{index}]");
        if !listed.insert(member.as_str()) {
            scope.problem(&field, format!("lists site {} twice", quoted(member)));
        } else if !sites.contains(member.as_str()) {
            scope.problem(&field, format!("unknown site {}", quoted(member)));
        }
    }
    scope.check_opt("notes", record.notes.as_deref(), notes);
}

/// Rule for a record's `name`.
fn name(value: &str) -> Rule {
    allowlist::text(value, NAME_MAX_CHARS)
}

/// Rule for a record's `notes`.
fn notes(value: &str) -> Rule {
    allowlist::text(value, NOTES_MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::super::model::{INVENTORY_VERSION, OwnershipPolicy};
    use super::*;

    /// A valid document with every kind of record.
    fn valid() -> Document {
        let mut doc = Document::empty();
        doc.defaults.ssh.user = Some("deploy".into());
        doc.defaults.ssh.binary = Some("/usr/bin/ssh".into());
        doc.servers = vec![
            Server::new("eu-1", "203.0.113.10"),
            Server::new("staging-box", "staging"),
        ];
        doc.sites = vec![
            Site::new("shop", "eu-1", "/var/www/shop/public"),
            Site::new("blog", "staging-box", "/var/www/blog"),
        ];
        doc.groups = vec![Group::new("qa", vec!["shop".into(), "blog".into()])];
        doc
    }

    /// A document with every field of every record set, all valid.
    fn full() -> Document {
        let mut doc = valid();
        doc.defaults.ssh = SshSettings {
            user: Some("deploy".into()),
            port: Some(22),
            identity_file: Some("~/.ssh/id_ed25519".into()),
            connect_timeout_secs: Some(15),
            known_hosts_file: Some("~/.ssh/known_hosts".into()),
            ssh_config_file: Some("~/.ssh/config".into()),
            proxy_jump: Some("bastion,deploy@[::1]:2222".into()),
            multiplex: Some(true),
            binary: Some("/usr/bin/ssh".into()),
        };
        doc.servers[0].name = Some("EU".into());
        doc.servers[0].notes = Some("primary".into());
        doc.servers[0].ssh.user = Some("ubuntu".into());
        doc.sites[0] = Site {
            name: Some("Shop".into()),
            url: Some("https://shop.example.com".into()),
            wp_cli: Some(vec![
                "/usr/bin/php8.3".into(),
                "-n".into(),
                "/opt/wp-cli.phar".into(),
            ]),
            run_as: Some(RunAs::User("www-data".into())),
            owner: Some(Owner::from("ftpuser:www-data")),
            ownership: Some(OwnershipPolicy::MirrorRoot),
            plugins_dir: Some("/var/www/shop/app/plugins".into()),
            allow_root: Some(false),
            remote_tmp_dir: Some("/var/tmp".into()),
            command_timeout_secs: Some(900),
            notes: Some("note".into()),
            ..Site::new("shop", "eu-1", "/var/www/shop/public")
        };
        doc.groups[0].notes = Some("QA".into());
        doc
    }

    /// The locations of a document's problems, in order.
    fn locations(doc: &Document) -> Vec<String> {
        document(doc).into_iter().map(|p| p.location).collect()
    }

    /// The problems in `problems`, as `location: message`.
    fn texts(problems: Vec<Problem>) -> Vec<String> {
        problems.iter().map(Problem::to_string).collect()
    }

    /// The problems of a site handed to an inventory with server `eu-1`.
    fn site_problems(site: &Site) -> Vec<String> {
        texts(site_record(site, &HashSet::from(["eu-1"])))
    }

    #[test]
    fn valid_documents_have_no_problems() {
        assert_eq!(document(&valid()), []);
        assert_eq!(document(&full()), []);
        assert_eq!(document(&Document::empty()), []);
    }

    #[test]
    fn records_must_be_json_objects() {
        let value = serde_json::json!({
            "version": 1,
            "defaults": {"ssh": ["deploy"]},
            "servers": [{"id": "a", "host": "h", "ssh": ["u", 22]}, ["b", null, "h"]],
            "sites": [{"id": "s", "server": "a", "path": "/p", "wp_cli": ["wp"]}],
            "groups": [["qa", []], {"id": "g", "sites": ["s"]}]
        });
        assert_eq!(
            texts(json_shape(&value)),
            [
                "defaults.ssh: must be a JSON object",
                "groups[0]: must be a JSON object",
                "servers[0].ssh: must be a JSON object",
                "servers[1]: must be a JSON object",
            ]
        );
        assert_eq!(
            json_shape(&serde_json::json!([1])),
            [Problem::new("", "the document must be a JSON object")]
        );
        assert_eq!(
            texts(json_shape(
                &serde_json::json!({"version": 1, "defaults": [{}]})
            )),
            ["defaults: must be a JSON object"]
        );
    }

    #[test]
    fn every_list_field_of_the_model_is_known_to_the_shape_check() {
        // A fully populated document, serialized, holds every list the model
        // has; none may be mistaken for a positional record.
        let value = serde_json::to_value(full()).unwrap();
        assert_eq!(json_shape(&value), []);
    }

    #[test]
    fn version_zero_is_refused() {
        let mut doc = valid();
        doc.version = 0;
        assert_eq!(texts(document(&doc)), ["version: must be at least 1"]);
        // Newer versions are not a validation problem (the loader decides).
        doc.version = INVENTORY_VERSION + 1;
        assert_eq!(document(&doc), []);
    }

    #[test]
    fn dangling_references_are_located() {
        let mut doc = valid();
        doc.sites[1].server = "eu-2".into();
        doc.groups[0].sites.push("ghost".into());
        assert_eq!(
            texts(document(&doc)),
            [
                "sites[1].server: unknown server 'eu-2'",
                "groups[0].sites[2]: unknown site 'ghost'",
            ]
        );
    }

    #[test]
    fn duplicate_ids_name_the_first_occurrence() {
        let mut doc = valid();
        doc.servers.push(Server::new("eu-1", "other"));
        doc.sites.push(Site::new("shop", "eu-1", "/srv/shop"));
        doc.groups.push(Group::new("qa", Vec::new()));
        assert_eq!(
            texts(document(&doc)),
            [
                "servers[2].id: duplicate id 'eu-1' (first at servers[0])",
                "sites[2].id: duplicate id 'shop' (first at sites[0])",
                "groups[1].id: duplicate id 'qa' (first at groups[0])",
            ]
        );
    }

    #[test]
    fn ids_are_unique_per_kind_only() {
        let mut doc = valid();
        doc.sites[0].id = "eu-1".into();
        doc.groups[0].sites[0] = "eu-1".into();
        doc.groups[0].id = "eu-1".into();
        assert_eq!(document(&doc), []);
    }

    #[test]
    fn group_members_are_listed_once() {
        let mut doc = valid();
        doc.groups[0].sites = vec!["shop".into(), "blog".into(), "shop".into()];
        assert_eq!(
            texts(document(&doc)),
            ["groups[0].sites[2]: lists site 'shop' twice"]
        );
    }

    #[test]
    fn every_problem_is_reported_in_document_order() {
        let mut doc = valid();
        doc.version = 0;
        doc.defaults.ssh.port = Some(0);
        doc.servers[0].host = "-example.com".into();
        doc.servers[1].ssh.binary = Some("/usr/bin/ssh".into());
        doc.sites[0].path = "var/www".into();
        doc.sites[0].url = Some("ftp://x".into());
        doc.sites[1].wp_cli = Some(vec!["wp".into(), "eval-file".into()]);
        doc.groups[0].id = "QA".into();
        assert_eq!(
            locations(&doc),
            [
                "version",
                "defaults.ssh.port",
                "servers[0].host",
                "servers[1].ssh.binary",
                "sites[0].path",
                "sites[0].url",
                "sites[1].wp_cli[0]",
                "sites[1].wp_cli[1]",
                "groups[0].id",
            ]
        );
    }

    #[test]
    fn every_ssh_field_is_checked() {
        let mut doc = Document::empty();
        doc.defaults.ssh = SshSettings {
            user: Some("-l".into()),
            port: Some(0),
            identity_file: Some("%h_key".into()),
            connect_timeout_secs: Some(0),
            known_hosts_file: Some("/dev/null".into()),
            ssh_config_file: Some("c$(id)".into()),
            proxy_jump: Some("none".into()),
            multiplex: Some(true),
            binary: Some("b`id`".into()),
        };
        assert_eq!(
            locations(&doc),
            [
                "defaults.ssh.user",
                "defaults.ssh.port",
                "defaults.ssh.identity_file",
                "defaults.ssh.connect_timeout_secs",
                "defaults.ssh.known_hosts_file",
                "defaults.ssh.ssh_config_file",
                "defaults.ssh.proxy_jump",
                "defaults.ssh.binary",
            ]
        );
    }

    #[test]
    fn connect_timeouts_are_bounded() {
        let check = |secs| {
            let mut doc = Document::empty();
            doc.defaults.ssh.connect_timeout_secs = Some(secs);
            texts(document(&doc))
        };
        assert!(check(1).is_empty());
        assert!(check(CONNECT_TIMEOUT_MAX_SECS).is_empty());
        assert_eq!(
            check(CONNECT_TIMEOUT_MAX_SECS + 1),
            ["defaults.ssh.connect_timeout_secs: must be between 1 and 3600 seconds"]
        );
    }

    #[test]
    fn binary_is_only_allowed_in_defaults() {
        let mut server = Server::new("eu-1", "h");
        server.ssh.binary = Some("/usr/bin/ssh".into());
        assert_eq!(
            texts(server_record(&server)),
            ["ssh.binary: is only allowed in defaults.ssh"]
        );
        let defaults = Defaults {
            ssh: SshSettings {
                binary: Some("/usr/bin/ssh".into()),
                ..SshSettings::default()
            },
        };
        assert_eq!(defaults_record(&defaults), []);
    }

    #[test]
    fn every_server_field_is_checked() {
        let server = Server {
            id: "EU".into(),
            name: Some(String::new()),
            host: "a b".into(),
            ssh: SshSettings {
                port: Some(0),
                ..SshSettings::default()
            },
            notes: Some("\u{202e}".into()),
        };
        assert_eq!(
            server_record(&server)
                .into_iter()
                .map(|p| p.location)
                .collect::<Vec<_>>(),
            ["id", "name", "host", "ssh.port", "notes"]
        );
    }

    #[test]
    fn names_and_notes_are_bounded() {
        let check = |name: String, notes: String| {
            let mut server = Server::new("eu-1", "h");
            server.name = Some(name);
            server.notes = Some(notes);
            texts(server_record(&server))
        };
        let at_most = |chars: usize| "é".repeat(chars);
        assert!(check(at_most(NAME_MAX_CHARS), at_most(NOTES_MAX_CHARS)).is_empty());
        assert_eq!(
            check(at_most(NAME_MAX_CHARS + 1), at_most(NOTES_MAX_CHARS + 1)),
            [
                "name: must be at most 128 characters",
                "notes: must be at most 1024 characters",
            ]
        );
    }

    #[test]
    fn every_site_field_is_checked() {
        let site = Site {
            id: "-shop".into(),
            name: Some("x\u{200b}".into()),
            server: "eu-2".into(),
            path: "/var/www/".into(),
            url: Some("https://example.com/?x".into()),
            wp_cli: Some(vec!["-S".into()]),
            run_as: Some(RunAs::User("a b".into())),
            owner: Some(Owner::from("ftpuser:www data")),
            ownership: Some(OwnershipPolicy::Check),
            plugins_dir: Some("plugins".into()),
            allow_root: Some(false),
            remote_tmp_dir: Some("/tmp/my dir".into()),
            command_timeout_secs: Some(0),
            notes: Some("\u{202e}".into()),
        };
        assert_eq!(
            site_record(&site, &HashSet::from(["eu-1"]))
                .into_iter()
                .map(|p| p.location)
                .collect::<Vec<_>>(),
            [
                "id",
                "name",
                "server",
                "path",
                "url",
                "wp_cli[0]",
                "run_as",
                "owner",
                "plugins_dir",
                "remote_tmp_dir",
                "command_timeout_secs",
                "notes",
            ]
        );
    }

    #[test]
    fn command_timeouts_are_bounded() {
        let check = |secs| {
            let mut site = Site::new("s", "eu-1", "/p");
            site.command_timeout_secs = Some(secs);
            site_problems(&site)
        };
        assert!(check(1).is_empty());
        assert!(check(COMMAND_TIMEOUT_MAX_SECS).is_empty());
        assert_eq!(
            check(COMMAND_TIMEOUT_MAX_SECS + 1),
            ["command_timeout_secs: must be between 1 and 86400 seconds"]
        );
    }

    #[test]
    fn wp_cli_is_one_program_or_php_running_a_script() {
        let with = |argv: &[&str]| {
            let mut site = Site::new("s", "eu-1", "/p");
            site.wp_cli = Some(argv.iter().map(|token| token.to_string()).collect());
            site_problems(&site)
        };
        for accepted in [
            &["wp"][..],
            &["/usr/local/bin/wp"],
            &["/usr/bin/php8.3", "/opt/wp-cli.phar"],
            &["php", "-n", "/opt/wp-cli.phar"],
            &["php", "-n", "-q", "/opt/wp-cli.phar"],
            &["/usr/local/bin/ea-php81", "/usr/local/bin/wp"],
        ] {
            assert!(
                with(accepted).is_empty(),
                "{accepted:?}: {:?}",
                with(accepted)
            );
        }
        let refused: [(&[&str], &[&str]); 9] = [
            (&[], &["wp_cli: must name the WP-CLI program"]),
            (
                &["sudo", "-n", "/usr/bin/chown", "deploy", "/etc/shadow"],
                &[
                    "wp_cli: must have at most 4 tokens: a PHP interpreter, its -n and -q \
                   options, and the WP-CLI script",
                ],
            ),
            (
                &["env", "-u", "WP_CLI_CONFIG_PATH", "wp"],
                &[
                    "wp_cli[0]: must be a PHP interpreter (php, php8.3, php-8.3, ea-php81) \
                     when more tokens follow",
                    "wp_cli[1]: must be the WP-CLI script (last) or one of the PHP options \
                     -n, -q",
                    "wp_cli[2]: must be the WP-CLI script (last) or one of the PHP options \
                     -n, -q",
                    "wp_cli[3]: must be an absolute path (WP-CLI runs from '/')",
                ],
            ),
            (
                &["wp", "eval-file", "/tmp/x.php"],
                &[
                    "wp_cli[0]: must be a PHP interpreter (php, php8.3, php-8.3, ea-php81) \
                     when more tokens follow",
                    "wp_cli[1]: must be the WP-CLI script (last) or one of the PHP options \
                     -n, -q",
                ],
            ),
            (
                &["php", "-r", "/x"],
                &[
                    "wp_cli[1]: must be the WP-CLI script (last) or one of the PHP options \
                   -n, -q",
                ],
            ),
            (
                &["php", "wp-cli.phar"],
                &["wp_cli[1]: must be an absolute path (WP-CLI runs from '/')"],
            ),
            (
                &["WP_CLI_CONFIG_PATH=/tmp/x"],
                &[
                    "wp_cli[0]: contains '='; only letters, digits, '.', '_', '/', '+' and \
                   '-' are allowed",
                ],
            ),
            (
                &["-S"],
                &["wp_cli[0]: must start with a character other than '-'"],
            ),
            (
                &["bin/wp"],
                &["wp_cli[0]: must be a command name or an absolute path"],
            ),
        ];
        for (argv, expected) in refused {
            assert_eq!(with(argv), expected, "{argv:?}");
        }
    }

    #[test]
    fn keywords_cannot_name_users() {
        let check = |run_as: Option<RunAs>, owner: Option<Owner>| {
            let mut site = Site::new("s", "eu-1", "/p");
            site.run_as = run_as;
            site.owner = owner;
            site_problems(&site)
        };
        for keyword in ["auto", "login"] {
            assert_eq!(
                check(Some(RunAs::User(keyword.into())), None),
                [format!(
                    "run_as: a user cannot be named '{keyword}': 'auto' and 'login' are \
                     keywords here"
                )]
            );
        }
        let auto_user = Owner::Explicit {
            user: "auto".into(),
            group: None,
        };
        assert_eq!(
            check(None, Some(auto_user)),
            [
                "owner: a user cannot be named 'auto': it is the keyword for the plugins \
              directory's owner"
            ]
        );
        assert_eq!(
            check(None, Some(Owner::from("auto:www-data"))),
            ["owner: 'auto' takes no group: use 'auto', or name the owner as user:group"]
        );
        // `login` is no keyword for `owner`, and the keywords themselves pass.
        assert!(check(Some(RunAs::Login), Some(Owner::from("login"))).is_empty());
        assert!(check(Some(RunAs::Auto), Some(Owner::Auto)).is_empty());
    }

    #[test]
    fn keywords_in_other_letter_cases_cannot_name_users_either() {
        let check = |run_as: Option<&str>, owner: Option<&str>| {
            let mut site = Site::new("s", "eu-1", "/p");
            site.run_as = run_as.map(RunAs::from);
            site.owner = owner.map(Owner::from);
            site_problems(&site)
        };
        for variant in ["Auto", "AUTO", "Login", "lOgIn"] {
            assert_eq!(
                check(Some(variant), None),
                [format!(
                    "run_as: a user cannot be named '{variant}': 'auto' and 'login' are \
                     keywords here, in any letter case"
                )],
                "{variant}"
            );
        }
        assert_eq!(
            check(None, Some("Auto")),
            [
                "owner: a user cannot be named 'Auto': 'auto', in any letter case, is the \
              keyword for the plugins directory's owner"
            ]
        );
        assert_eq!(
            check(None, Some("AUTO:www-data")),
            [
                "owner: 'AUTO' (the keyword 'auto' in any letter case) takes no group: use \
              'auto', or name the owner as user:group"
            ]
        );
        // Names that merely contain a keyword are users, and `Login` is no
        // keyword for `owner` (as `login` is not).
        assert!(check(Some("autobot"), Some("Login:www-data")).is_empty());
        assert!(check(Some("login2"), Some("Login")).is_empty());
    }

    #[test]
    fn a_document_with_a_keyword_variant_is_invalid_like_a_root_variant() {
        let mut doc = valid();
        let mut site = Site::new("odd", "eu-1", "/var/www/odd");
        site.run_as = Some(RunAs::from("Login"));
        site.owner = Some(Owner::from("Auto"));
        doc.sites.push(site);
        let mut rooted = Site::new("rooted", "eu-1", "/var/www/rooted");
        rooted.run_as = Some(RunAs::from("Root"));
        doc.sites.push(rooted);
        let at = doc.sites.len();
        assert_eq!(
            locations(&doc),
            [
                format!("sites[{}].run_as", at - 2),
                format!("sites[{}].owner", at - 2),
                format!("sites[{}].run_as", at - 1),
            ]
        );
    }

    #[test]
    fn root_needs_allow_root() {
        let check = |run_as: Option<&str>, owner: Option<&str>, allow_root: Option<bool>| {
            let mut site = Site::new("s", "eu-1", "/p");
            site.run_as = run_as.map(RunAs::from);
            site.owner = owner.map(Owner::from);
            site.allow_root = allow_root;
            site_problems(&site)
        };
        assert_eq!(
            check(Some("root"), None, None),
            ["run_as: 'root' needs allow_root: true"]
        );
        assert_eq!(
            check(Some("ROOT"), None, Some(false)),
            ["run_as: 'ROOT' needs allow_root: true"]
        );
        assert_eq!(
            check(None, Some("Root:www-data"), None),
            ["owner: user 'Root' needs allow_root: true"]
        );
        assert_eq!(
            check(None, Some("www-data:root"), None),
            ["owner: group 'root' needs allow_root: true"]
        );
        assert_eq!(
            check(None, Some("0:0"), None),
            ["owner: user must not be all digits (tools read it as a numeric id)"]
        );
        assert!(check(Some("root"), Some("root:root"), Some(true)).is_empty());
        assert!(check(Some("login"), Some("auto"), None).is_empty());
        assert!(check(Some("www-data"), Some("ftpuser:www-data"), None).is_empty());
    }

    #[test]
    fn owner_parts_are_checked() {
        let check = |owner: &str| {
            let mut site = Site::new("s", "eu-1", "/p");
            site.owner = Some(Owner::from(owner));
            site_problems(&site)
        };
        assert!(check("ftpuser").is_empty());
        assert!(check("ftpuser:www-data").is_empty());
        assert_eq!(check(""), ["owner: user must not be empty"]);
        assert_eq!(check("ftpuser:"), ["owner: group must not be empty"]);
        assert_eq!(check(":www-data"), ["owner: user must not be empty"]);
        assert_eq!(
            check("a:b:c"),
            // "ASCII letters": the user/group message names the class exactly.
            ["owner: group contains ':'; only ASCII letters, digits, '.', '_' and '-' are allowed"]
        );
        assert_eq!(
            check("ftpuser:.."),
            ["owner: group must not be '.' or '..'"]
        );
    }

    #[test]
    fn group_records_check_members_against_the_inventory() {
        let sites = HashSet::from(["shop", "blog"]);
        let check = |group: &Group| texts(group_record(group, &sites));
        assert!(check(&Group::new("qa", vec!["shop".into(), "blog".into()])).is_empty());
        assert!(check(&Group::new("empty", Vec::new())).is_empty());
        assert_eq!(
            check(&Group::new("qa", vec!["nope".into()])),
            ["sites[0]: unknown site 'nope'"]
        );
        let mut group = Group::new("qa", Vec::new());
        group.notes = Some(String::new());
        assert_eq!(
            check(&group),
            ["notes: must not be empty (omit it instead)"]
        );
    }
}
