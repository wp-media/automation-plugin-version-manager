//! `apvm server` — the SSH hosts of the site inventory (plan §5.2). It
//! never connects to them (no SSH): it reads or edits the inventory only.
//!
//! ```text
//! apvm server defaults [SSH flags] [--ssh-binary …] [--unset <FIELD>]… [--json]
//!                                                   # no change flags: show
//! apvm server add <ID> --host <HOST> [SSH flags] [--name …] [--notes …]
//! apvm server edit <ID> [--host …] [SSH flags] [--name …] [--notes …] [--unset <FIELD>]…
//! apvm server list [--json]
//! apvm server show <ID> [--json]
//! apvm server remove <ID> [--cascade] [-y]
//!
//! SSH flags: --user --port --key --connect-timeout --jump --known-hosts
//!            --ssh-config --multiplex|--no-multiplex
//! ```
//!
//! Every setting is declared once, in [`fields`]; the flags only say which
//! [`Setting`]s they carry. Each change is one `io::update` (lock → load →
//! change → save), so concurrent edits all land; a refused change writes
//! nothing. Refused input names the flag the user typed (`--port: …`). The
//! connection test (`server test`, and on `add`) arrives with the probe
//! (plan P4.3).

use std::path::Path;

use super::inventory::field_table::{self, Change};
use super::inventory::{
    Console, Failure, Removal, count, load, name_flags, not_found, print_json_failure, quoted_ids,
    remove_confirmed,
};
use apvm_sites::inventory::io;
use apvm_sites::{Defaults, EntityKind, Inventory, Server, ServerRemoval};
use clap::{ArgGroup, Args, Subcommand};
use fields::{Field, Record, Scope, Setting};

mod fields;
mod render;

// ─────────────────────────────────────────────────────────────────────────────
// Arguments
// ─────────────────────────────────────────────────────────────────────────────

/// Arguments for `apvm server`.
#[derive(Args, Debug)]
pub struct ServerArgs {
    /// What to do with the servers.
    #[command(subcommand)]
    action: ServerAction,
}

/// `apvm server` subcommands.
#[derive(Subcommand, Debug)]
enum ServerAction {
    /// Show or change the SSH defaults every server inherits (no change flags: show)
    Defaults(DefaultsArgs),
    /// Add a server: an SSH host that holds WordPress sites
    Add(AddArgs),
    /// Change a server's settings (at least one option)
    Edit(EditArgs),
    /// List the servers
    List(ListArgs),
    /// Show a server with its effective (merged) SSH settings
    Show(ShowArgs),
    /// Remove a server (refused while sites use it, unless --cascade)
    Remove(RemoveArgs),
}

/// `--multiplex` / `--no-multiplex`.
#[derive(Args, Debug, Default)]
struct MultiplexFlags {
    /// Reuse one SSH connection per server (OpenSSH multiplexing; Unix only)
    #[arg(long, conflicts_with = "no_multiplex")]
    multiplex: bool,
    /// Open a new SSH connection for every command
    #[arg(long)]
    no_multiplex: bool,
}

impl MultiplexFlags {
    /// The requested setting, when either flag was passed.
    fn setting(&self) -> Option<Setting> {
        match (self.multiplex, self.no_multiplex) {
            (true, _) => Some(Setting::Multiplex(true)),
            (_, true) => Some(Setting::Multiplex(false)),
            _ => None,
        }
    }
}

/// SSH flags shared by `server add` and `server edit`.
#[derive(Args, Debug, Default)]
struct SshFlags {
    /// SSH login user (unset: the default user, else your ssh_config)
    #[arg(long, value_name = "USER")]
    user: Option<String>,
    /// SSH port (unset: the default port, else your ssh_config)
    #[arg(long, value_name = "N")]
    port: Option<u16>,
    /// Private key for this server, instead of the default key
    #[arg(long, value_name = "PATH")]
    key: Option<String>,
    /// Seconds allowed to establish a connection (1–3600; unset: the default)
    #[arg(long, value_name = "SECS")]
    connect_timeout: Option<u32>,
    /// Jump hosts, as ssh -J takes them (at most 8, comma-separated)
    #[arg(long, value_name = "[USER@]HOST[:PORT][,…]")]
    jump: Option<String>,
    /// known_hosts file to use instead of yours
    #[arg(long, value_name = "PATH")]
    known_hosts: Option<String>,
    /// ssh_config file to use instead of yours
    #[arg(long, value_name = "PATH")]
    ssh_config: Option<String>,
    /// Multiplexing.
    #[command(flatten)]
    multiplex: MultiplexFlags,
}

impl SshFlags {
    /// The settings these flags carry. Every field is destructured, so a new
    /// flag does not compile until it is listed here.
    #[deny(unused_variables)]
    fn settings(&self) -> Vec<Setting> {
        let Self {
            user,
            port,
            key,
            connect_timeout,
            jump,
            known_hosts,
            ssh_config,
            multiplex,
        } = self;
        [
            user.clone().map(Setting::User),
            port.map(Setting::Port),
            key.clone().map(Setting::Key),
            connect_timeout.map(Setting::ConnectTimeout),
            jump.clone().map(Setting::Jump),
            known_hosts.clone().map(Setting::KnownHosts),
            ssh_config.clone().map(Setting::SshConfig),
            multiplex.setting(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// `--name` / `--notes`.
#[derive(Args, Debug, Default)]
struct TextFlags {
    /// Display name (1–128 characters)
    #[arg(long, value_name = "TEXT")]
    name: Option<String>,
    /// Free-form notes (1–1024 characters)
    #[arg(long, value_name = "TEXT")]
    notes: Option<String>,
}

impl TextFlags {
    /// The settings these flags carry (see [`SshFlags::settings`]).
    #[deny(unused_variables)]
    fn settings(&self) -> Vec<Setting> {
        let Self { name, notes } = self;
        [
            name.clone().map(Setting::Name),
            notes.clone().map(Setting::Notes),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Arguments for `apvm server add`.
#[derive(Args, Debug)]
struct AddArgs {
    /// Server id: lowercase letters, digits, '.', '_' and '-' (e.g. eu-1)
    id: String,
    /// Host name, IP address, or an alias from your ~/.ssh/config
    #[arg(long, value_name = "HOST")]
    host: String,
    /// SSH settings.
    #[command(flatten)]
    ssh: SshFlags,
    /// Name and notes.
    #[command(flatten)]
    text: TextFlags,
}

/// Arguments for `apvm server edit`: at least one change.
#[derive(Args, Debug)]
#[command(
    override_usage = "apvm server edit <ID> <OPTION>…",
    group(
        ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args([
                "host", "user", "port", "key", "connect_timeout", "jump", "known_hosts",
                "ssh_config", "multiplex", "no_multiplex", "name", "notes", "unset",
            ])
    )
)]
struct EditArgs {
    /// Id of the server to change
    id: String,
    /// Host name, IP address, or an alias from your ~/.ssh/config
    #[arg(long, value_name = "HOST")]
    host: Option<String>,
    /// SSH settings.
    #[command(flatten)]
    ssh: SshFlags,
    /// Name and notes.
    #[command(flatten)]
    text: TextFlags,
    /// Clear a setting (repeatable, or comma-separated)
    #[arg(
        long,
        value_name = "FIELD",
        value_delimiter = ',',
        value_parser = fields::unset_parser(Scope::Server)
    )]
    unset: Vec<Field>,
}

/// Arguments for `apvm server defaults`.
#[derive(Args, Debug)]
struct DefaultsArgs {
    /// Default SSH login user
    #[arg(long, value_name = "USER")]
    user: Option<String>,
    /// Default SSH port
    #[arg(long, value_name = "N")]
    port: Option<u16>,
    /// The one main private key every server uses unless it sets its own
    #[arg(long, value_name = "PATH")]
    key: Option<String>,
    /// Seconds allowed to establish a connection (1–3600; built-in: 15)
    #[arg(long, value_name = "SECS")]
    connect_timeout: Option<u32>,
    /// Default jump hosts, as ssh -J takes them (at most 8, comma-separated)
    #[arg(long, value_name = "[USER@]HOST[:PORT][,…]")]
    jump: Option<String>,
    /// Default known_hosts file, instead of yours
    #[arg(long, value_name = "PATH")]
    known_hosts: Option<String>,
    /// Default ssh_config file, instead of yours
    #[arg(long, value_name = "PATH")]
    ssh_config: Option<String>,
    /// The ssh program: a path or a command name (built-in: ssh)
    #[arg(long, value_name = "PATH")]
    ssh_binary: Option<String>,
    /// Multiplexing.
    #[command(flatten)]
    multiplex: MultiplexFlags,
    /// Clear a default (repeatable, or comma-separated)
    #[arg(
        long,
        value_name = "FIELD",
        value_delimiter = ',',
        value_parser = fields::unset_parser(Scope::Defaults)
    )]
    unset: Vec<Field>,
    /// Print the defaults as JSON
    #[arg(long)]
    json: bool,
}

/// Arguments for `apvm server list`.
#[derive(Args, Debug)]
struct ListArgs {
    /// Print the servers as JSON
    #[arg(long)]
    json: bool,
}

/// Arguments for `apvm server show`.
#[derive(Args, Debug)]
struct ShowArgs {
    /// Id of the server to show
    id: String,
    /// Print the server as JSON
    #[arg(long)]
    json: bool,
}

/// Arguments for `apvm server remove`.
#[derive(Args, Debug)]
struct RemoveArgs {
    /// Id of the server to remove
    id: String,
    /// Also remove the server's sites (they leave their groups too)
    #[arg(long)]
    cascade: bool,
    /// Skip the confirmation prompt (required when stdin is not a terminal)
    #[arg(short = 'y', long = "yes")]
    yes: bool,
}

impl AddArgs {
    /// The server these flags describe (validated by the inventory; see
    /// [`SshFlags::settings`]).
    #[deny(unused_variables)]
    fn to_server(&self) -> Server {
        let Self {
            id,
            host,
            ssh,
            text,
        } = self;
        let settings = [ssh.settings(), text.settings()].concat();
        fields::apply(&Server::new(id.clone(), host.clone()), &settings, &[])
    }
}

impl EditArgs {
    /// The settings the flags carry (`--unset` aside; see
    /// [`SshFlags::settings`]).
    #[deny(unused_variables)]
    fn settings(&self) -> Vec<Setting> {
        let Self {
            id: _,
            host,
            ssh,
            text,
            unset: _,
        } = self;
        let host = host.clone().map(Setting::Host);
        host.into_iter()
            .chain(ssh.settings())
            .chain(text.settings())
            .collect()
    }
}

impl DefaultsArgs {
    /// The settings the flags carry (see [`SshFlags::settings`]).
    #[deny(unused_variables)]
    fn settings(&self) -> Vec<Setting> {
        let Self {
            user,
            port,
            key,
            connect_timeout,
            jump,
            known_hosts,
            ssh_config,
            ssh_binary,
            multiplex,
            unset: _,
            json: _,
        } = self;
        [
            user.clone().map(Setting::User),
            port.map(Setting::Port),
            key.clone().map(Setting::Key),
            connect_timeout.map(Setting::ConnectTimeout),
            jump.clone().map(Setting::Jump),
            known_hosts.clone().map(Setting::KnownHosts),
            ssh_config.clone().map(Setting::SshConfig),
            ssh_binary.clone().map(Setting::SshBinary),
            multiplex.setting(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

impl ServerArgs {
    /// Execute the command on the real terminal against the inventory file
    /// `inventory` (`APVM_INVENTORY` or `~/.apvm/inventory.json`).
    ///
    /// # Errors
    ///
    /// The command's [`Failure`]; with `--json` it is also printed as a
    /// JSON document on stdout.
    pub fn execute(&self, inventory: &Path) -> Result<(), Failure> {
        Console::with_terminal(|console| self.run(inventory, console))
    }

    /// Run the command with all I/O through `console`.
    fn run(&self, path: &Path, console: &mut Console<'_>) -> Result<(), Failure> {
        let result = match &self.action {
            ServerAction::Defaults(args) => defaults(path, args, console),
            ServerAction::Add(args) => add(path, args, console),
            ServerAction::Edit(args) => edit(path, args, console),
            ServerAction::List(args) => list(path, args, console),
            ServerAction::Show(args) => show(path, args, console),
            ServerAction::Remove(args) => remove(path, args, console),
        };
        print_json_failure(&result, self.json(), console);
        result
    }

    /// Whether the user asked for JSON output.
    fn json(&self) -> bool {
        match &self.action {
            ServerAction::Defaults(args) => args.json,
            ServerAction::List(args) => args.json,
            ServerAction::Show(args) => args.json,
            ServerAction::Add(_) | ServerAction::Edit(_) | ServerAction::Remove(_) => false,
        }
    }
}

/// `apvm server defaults`: show the defaults, or change them first.
fn defaults(path: &Path, args: &DefaultsArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let settings = args.settings();
    if settings.is_empty() && args.unset.is_empty() {
        let inventory = load(path, console)?;
        return print_defaults(inventory.defaults(), args.json, console);
    }
    let (before, after) = Change::new(Scope::Defaults, &settings, &args.unset)?.save(
        path,
        |inventory| Ok(inventory.defaults().clone()),
        |inventory, defaults| inventory.set_defaults(defaults),
    )?;
    warn_relative(Some(&before), &after, console);
    if args.json {
        return print_defaults(&after, true, console);
    }
    let changed = fields::changed(&before, &after);
    console.print(&change_line(DEFAULTS_UPDATED, DEFAULTS_UNCHANGED, &changed));
    Ok(())
}

/// What `server defaults` reports after a change (then the changed fields).
const DEFAULTS_UPDATED: &str = "Updated the SSH defaults";

/// What `server defaults` reports when a change changed nothing.
const DEFAULTS_UNCHANGED: &str = "The SSH defaults are unchanged.";

/// Print the defaults as text or JSON.
fn print_defaults(
    defaults: &Defaults,
    json: bool,
    console: &mut Console<'_>,
) -> Result<(), Failure> {
    console.print(&render::defaults_view(defaults, json)?);
    Ok(())
}

/// `apvm server add`.
fn add(path: &Path, args: &AddArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let server = args.to_server();
    io::update(path, |inventory| {
        inventory
            .add_server(server.clone())
            .map_err(|error| name_flags(error, &fields::flag_map(Scope::Server)))
    })?;
    warn_relative(None, &server, console);
    console.print(&format!("Added server '{}'.\n", args.id));
    Ok(())
}

/// `apvm server edit`.
fn edit(path: &Path, args: &EditArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let settings = args.settings();
    let change = Change::new(Scope::Server, &settings, &args.unset)?;
    let (before, after) = change.save(
        path,
        |inventory| {
            let server = inventory.server(&args.id).cloned();
            server.ok_or_else(|| not_found(EntityKind::Server, &args.id))
        },
        |inventory, server| inventory.update_server(server),
    )?;
    warn_relative(Some(&before), &after, console);
    console.print(&change_line(
        &format!("Updated server '{}'", args.id),
        &format!("Server '{}' is unchanged.", args.id),
        &fields::changed(&before, &after),
    ));
    Ok(())
}

/// `apvm server list`.
fn list(path: &Path, args: &ListArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let inventory = load(path, console)?;
    console.print(&render::list_view(&inventory, args.json)?);
    Ok(())
}

/// `apvm server show`.
fn show(path: &Path, args: &ShowArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let inventory = load(path, console)?;
    let server = inventory
        .server(&args.id)
        .ok_or_else(|| not_found(EntityKind::Server, &args.id))?;
    let effective = inventory.resolve_server(&args.id)?.ssh;
    console.print(&render::show_view(
        server, &inventory, &effective, args.json,
    )?);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Removal
// ─────────────────────────────────────────────────────────────────────────────

/// `apvm server remove`: confirm what will go (unless `-y`), then remove
/// it, refusing when it changed in between.
fn remove(path: &Path, args: &RemoveArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    if let Some(removal) = remove_confirmed(path, args, args.yes, console)? {
        console.print(&removed_text(&removal));
    }
    Ok(())
}

/// The server, its sites (with `--cascade`) and their group memberships,
/// asked about first and re-checked under the lock.
impl Removal for RemoveArgs {
    type Removed = ServerRemoval;
    type Scope = RemovalScope;

    fn remove(&self, inventory: &mut Inventory) -> Result<ServerRemoval, apvm_sites::Error> {
        inventory.remove_server(&self.id, self.cascade)
    }

    fn scope(removed: &ServerRemoval) -> RemovalScope {
        RemovalScope::of(removed)
    }

    fn question(removed: &ServerRemoval) -> String {
        removal_question(removed)
    }

    fn refusal(&self) -> String {
        confirmation_refusal(&self.id)
    }

    fn changed(&self) -> Failure {
        changed_since_confirmation(&self.id)
    }
}

/// What a removal takes with it — the server record, its sites and the
/// groups they leave — compared before and after the confirmation.
#[derive(Debug, PartialEq, Eq)]
struct RemovalScope {
    /// The server record (its host and settings included).
    server: Server,
    /// Removed site ids.
    sites: Vec<String>,
    /// Ids of the groups they left.
    groups: Vec<String>,
}

impl RemovalScope {
    /// The scope of `removal`.
    fn of(removal: &ServerRemoval) -> Self {
        Self {
            server: removal.server.clone(),
            sites: removal.sites.iter().map(|site| site.id.clone()).collect(),
            groups: removal.groups.clone(),
        }
    }
}

/// The confirmation question for a removal.
fn removal_question(removal: &ServerRemoval) -> String {
    let id = &removal.server.id;
    let sites: Vec<&str> = removal.sites.iter().map(|site| site.id.as_str()).collect();
    if sites.is_empty() {
        return format!("Remove server '{id}'?");
    }
    let mut question = format!(
        "Remove server '{id}' and its {}: {}",
        count(sites.len(), "site"),
        quoted_ids(&sites)
    );
    if !removal.groups.is_empty() {
        question.push_str(&format!(
            " (they also leave {}: {})",
            count(removal.groups.len(), "group"),
            quoted_ids(&removal.groups)
        ));
    }
    question.push('?');
    question
}

/// Why a removal is refused without a terminal to confirm on.
fn confirmation_refusal(id: &str) -> String {
    format!(
        "not removing server '{id}': it needs a confirmation, and stdin is not a terminal; \
         pass -y (--yes) to remove it without asking"
    )
}

/// Why a removal is refused after the inventory changed under the prompt.
fn changed_since_confirmation(id: &str) -> Failure {
    Failure::Changed(format!(
        "server '{id}' or its sites changed while you were confirming; nothing was removed: \
         run the command again"
    ))
}

/// What a removal reports.
fn removed_text(removal: &ServerRemoval) -> String {
    let id = &removal.server.id;
    let sites: Vec<&str> = removal.sites.iter().map(|site| site.id.as_str()).collect();
    if sites.is_empty() {
        return format!("Removed server '{id}'.\n");
    }
    let mut text = format!(
        "Removed server '{id}' and its {}: {}.\n",
        count(sites.len(), "site"),
        quoted_ids(&sites)
    );
    if !removal.groups.is_empty() {
        text.push_str(&format!(
            "They were removed from {}: {}.\n",
            count(removal.groups.len(), "group"),
            quoted_ids(&removal.groups)
        ));
    }
    text
}

// ─────────────────────────────────────────────────────────────────────────────
// Reports
// ─────────────────────────────────────────────────────────────────────────────

/// `"<done>: a, b.\n"`, or `"<unchanged>\n"` when nothing changed
/// (the shared report, typed for this table).
fn change_line(done: &str, unchanged: &str, changed: &[Field]) -> String {
    field_table::change_line(done, unchanged, changed)
}

/// Warn about each local path the change stored that ssh would resolve
/// from the directory apvm runs in (stored as given, so it breaks from
/// anywhere else). A path the record already held is not warned about
/// again.
///
/// # Arguments
///
/// * `before` - The record before the change (`None`: a new record)
/// * `after` - The record as saved
/// * `console` - Where the warnings go
fn warn_relative<R: Record<Setting = Setting>>(
    before: Option<&R>,
    after: &R,
    console: &mut Console<'_>,
) {
    for (flag, value) in fields::new_relative_paths(before, after) {
        console.warn(&format!(
            "{flag} '{value}' is a relative path, resolved from the directory apvm \
             runs in each time; prefer an absolute path or ~/…"
        ));
    }
}

#[cfg(test)]
mod tests;
