//! `apvm site` — the WordPress sites of the site inventory (plan §5.2). It
//! never connects to them (no SSH): it reads or edits the inventory only,
//! and removing a site changes nothing on its server.
//!
//! ```text
//! apvm site add <ID> --server <ID> --path <WP_ROOT> [site flags] [--group <ID>]…
//! apvm site edit <ID> [--server …] [--path …] [site flags] [--group <ID>]… [--unset <FIELD>]…
//! apvm site list [--server <ID>]… [--group <ID>]… [--json]
//! apvm site show <ID> [--json]
//! apvm site remove <ID> [-y]
//!
//! site flags: --url --wp-cli --run-as --owner --ownership --plugins-dir
//!             --allow-root --tmp-dir --timeout --name --notes
//! ```
//!
//! Every setting is declared once, in [`fields`]; the flags only say which
//! [`Setting`]s they carry. Each change is one `io::update` (lock → load →
//! change → save), so concurrent edits all land; a refused change writes
//! nothing. Refused input names the flag the user typed (`--run-as: …`).
//! `--group` joins existing groups and never leaves one (`apvm group edit
//! <GROUP> --remove <SITE>` does). The probe on `add` and `site test`
//! arrive with the probe (plan P4.3): `add` builds the record
//! ([`AddArgs::to_site`]) before saving it, which is where the probe will
//! run.

use std::path::Path;

use super::inventory::field_table::{self, Change, TableField};
use super::inventory::{
    Console, Failure, Removal, count, load, name_flags, not_found, print_json_failure, quoted_ids,
    remove_confirmed,
};
use apvm_sites::inventory::io;
use apvm_sites::{EntityKind, Inventory, OwnershipPolicy, Site, SiteRemoval};
use clap::{ArgGroup, Args, Subcommand};
use fields::{Field, Scope, Setting};

mod fields;
mod render;

// ─────────────────────────────────────────────────────────────────────────────
// Arguments
// ─────────────────────────────────────────────────────────────────────────────

/// Arguments for `apvm site`.
#[derive(Args, Debug)]
pub struct SiteArgs {
    /// What to do with the sites.
    #[command(subcommand)]
    action: SiteAction,
}

/// `apvm site` subcommands.
#[derive(Subcommand, Debug)]
enum SiteAction {
    /// Add a site: a WordPress install on a server
    Add(AddArgs),
    /// Change a site's settings (at least one option)
    Edit(EditArgs),
    /// List the sites, optionally only those on some servers or in some groups
    List(ListArgs),
    /// Show a site with its effective settings
    Show(ShowArgs),
    /// Remove a site from the inventory (nothing on its server changes)
    Remove(RemoveArgs),
}

/// The optional site settings, shared by `site add` and `site edit`.
#[derive(Args, Debug, Default)]
struct SiteFlags {
    /// Site URL, passed to WP-CLI as --url (needed on multisite)
    #[arg(long, value_name = "URL")]
    url: Option<String>,
    /// WP-CLI command (built-in: wp), e.g. "/usr/bin/php8.3 /opt/wp-cli.phar"
    #[arg(long, value_name = "CMD")]
    wp_cli: Option<String>,
    /// Who WP-CLI runs as: auto (the plugins directory's owner), login (the SSH user) or a user
    #[arg(long, value_name = "auto|login|USER")]
    run_as: Option<String>,
    /// Who must own the plugin files: auto (the plugins directory's owner), USER or USER:GROUP
    #[arg(long, value_name = "auto|USER[:GROUP]")]
    owner: Option<String>,
    /// How plugin ownership is checked and repaired (built-in: mirror)
    #[arg(long, value_name = "POLICY", value_parser = fields::ownership_parser())]
    ownership: Option<OwnershipPolicy>,
    /// Plugins directory, for a site that sets WP_PLUGIN_DIR (absolute)
    #[arg(long, value_name = "PATH")]
    plugins_dir: Option<String>,
    /// Allow root: WP-CLI as root, or root-owned plugin files
    #[arg(long)]
    allow_root: bool,
    /// Remote temporary directory for uploads and the site lock (built-in: /tmp)
    #[arg(long, value_name = "PATH")]
    tmp_dir: Option<String>,
    /// Seconds a remote command may run (1–86400; unset: each operation's default)
    #[arg(long, value_name = "SECS")]
    timeout: Option<u32>,
    /// Display name (1–128 characters)
    #[arg(long, value_name = "TEXT")]
    name: Option<String>,
    /// Free-form notes (1–1024 characters)
    #[arg(long, value_name = "TEXT")]
    notes: Option<String>,
}

impl SiteFlags {
    /// The settings these flags carry. Every field is destructured, so a new
    /// flag does not compile until it is listed here.
    #[deny(unused_variables)]
    fn settings(&self) -> Vec<Setting> {
        let Self {
            url,
            wp_cli,
            run_as,
            owner,
            ownership,
            plugins_dir,
            allow_root,
            tmp_dir,
            timeout,
            name,
            notes,
        } = self;
        [
            url.clone().map(Setting::Url),
            wp_cli
                .as_deref()
                .map(|command| Setting::WpCli(fields::wp_cli_tokens(command))),
            run_as.clone().map(Setting::RunAs),
            owner.clone().map(Setting::Owner),
            ownership.map(Setting::Ownership),
            plugins_dir.clone().map(Setting::PluginsDir),
            allow_root.then_some(Setting::AllowRoot(true)),
            tmp_dir.clone().map(Setting::TmpDir),
            timeout.map(Setting::Timeout),
            name.clone().map(Setting::Name),
            notes.clone().map(Setting::Notes),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Arguments for `apvm site add`.
#[derive(Args, Debug)]
struct AddArgs {
    /// Site id: lowercase letters, digits, '.', '_' and '-' (e.g. shop)
    id: String,
    /// Id of the server the site is on
    #[arg(long, value_name = "ID")]
    server: String,
    /// The WordPress root on the server (absolute)
    #[arg(long, value_name = "WP_ROOT")]
    path: String,
    /// Optional settings.
    #[command(flatten)]
    flags: SiteFlags,
    /// Add the site to an existing group (repeatable, or comma-separated)
    #[arg(long = "group", value_name = "ID", value_delimiter = ',')]
    groups: Vec<String>,
}

/// Arguments for `apvm site edit`: at least one change.
#[derive(Args, Debug)]
#[command(
    override_usage = "apvm site edit <ID> <OPTION>…",
    group(
        ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args([
                "server", "path", "url", "wp_cli", "run_as", "owner", "ownership", "plugins_dir",
                "allow_root", "tmp_dir", "timeout", "name", "notes", "groups", "unset",
            ])
    )
)]
struct EditArgs {
    /// Id of the site to change
    id: String,
    /// Move the site to another server
    #[arg(long, value_name = "ID")]
    server: Option<String>,
    /// The WordPress root on the server (absolute)
    #[arg(long, value_name = "WP_ROOT")]
    path: Option<String>,
    /// Optional settings.
    #[command(flatten)]
    flags: SiteFlags,
    /// Also add the site to an existing group (repeatable, or comma-separated; it leaves none)
    #[arg(long = "group", value_name = "ID", value_delimiter = ',')]
    groups: Vec<String>,
    /// Clear a setting (repeatable, or comma-separated)
    #[arg(
        long,
        value_name = "FIELD",
        value_delimiter = ',',
        value_parser = fields::unset_parser()
    )]
    unset: Vec<Field>,
}

/// Arguments for `apvm site list`.
#[derive(Args, Debug)]
struct ListArgs {
    /// Only sites on this server (repeatable, or comma-separated)
    #[arg(long = "server", value_name = "ID", value_delimiter = ',')]
    servers: Vec<String>,
    /// Only sites in this group (repeatable, or comma-separated)
    #[arg(long = "group", value_name = "ID", value_delimiter = ',')]
    groups: Vec<String>,
    /// Print the sites as JSON
    #[arg(long)]
    json: bool,
}

/// Arguments for `apvm site show`.
#[derive(Args, Debug)]
struct ShowArgs {
    /// Id of the site to show
    id: String,
    /// Print the site as JSON
    #[arg(long)]
    json: bool,
}

/// Arguments for `apvm site remove`.
#[derive(Args, Debug)]
struct RemoveArgs {
    /// Id of the site to remove
    id: String,
    /// Skip the confirmation prompt (required when stdin is not a terminal)
    #[arg(short = 'y', long = "yes")]
    yes: bool,
}

impl AddArgs {
    /// The site these flags describe (validated by the inventory; see
    /// [`SiteFlags::settings`]).
    #[deny(unused_variables)]
    fn to_site(&self) -> Site {
        let Self {
            id,
            server,
            path,
            flags,
            groups: _,
        } = self;
        let site = Site::new(id.clone(), server.clone(), path.clone());
        field_table::apply(&site, &flags.settings(), &[])
    }
}

impl EditArgs {
    /// The settings the flags carry (`--group` and `--unset` aside; see
    /// [`SiteFlags::settings`]).
    #[deny(unused_variables)]
    fn settings(&self) -> Vec<Setting> {
        let Self {
            id: _,
            server,
            path,
            flags,
            groups: _,
            unset: _,
        } = self;
        let required = [
            server.clone().map(Setting::Server),
            path.clone().map(Setting::Path),
        ];
        required
            .into_iter()
            .flatten()
            .chain(flags.settings())
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

impl SiteArgs {
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
            SiteAction::Add(args) => add(path, args, console),
            SiteAction::Edit(args) => edit(path, args, console),
            SiteAction::List(args) => list(path, args, console),
            SiteAction::Show(args) => show(path, args, console),
            SiteAction::Remove(args) => remove(path, args, console),
        };
        print_json_failure(&result, self.json(), console);
        result
    }

    /// Whether the user asked for JSON output.
    fn json(&self) -> bool {
        match &self.action {
            SiteAction::List(args) => args.json,
            SiteAction::Show(args) => args.json,
            SiteAction::Add(_) | SiteAction::Edit(_) | SiteAction::Remove(_) => false,
        }
    }
}

/// `apvm site add`: the site, then its groups, in one `io::update`.
fn add(path: &Path, args: &AddArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let site = args.to_site();
    let joined = io::update(path, |inventory| {
        if let Err(error) = inventory.add_site(site.clone()) {
            return Err(refused_new_site(inventory, error, &site));
        }
        join_groups(inventory, &site.id, &args.groups)
    })?;
    console.print(&format!("Added site '{}'.\n", args.id));
    console.print(&joined_text(&joined));
    Ok(())
}

/// `apvm site edit`: the settings, then the groups, in one `io::update`.
fn edit(path: &Path, args: &EditArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let settings = args.settings();
    let change = Change::new(Scope::Site, &settings, &args.unset)?;
    let (before, after, joined) = io::update(path, |inventory| {
        let (before, after) = change.apply_with(
            inventory,
            |inventory| current(inventory, &args.id),
            |inventory, site| inventory.update_site(site),
            |inventory, site, error| {
                let change = RootChange::of_edit(inventory, &args.id, &args.unset);
                root_remedy(inventory, site, error, change)
            },
        )?;
        let joined = join_groups(inventory, &args.id, &args.groups)?;
        Ok::<_, Failure>((before, after, joined))
    })?;
    console.print(&edit_report(&args.id, &before, &after, &joined));
    Ok(())
}

/// What an edit reports: the changed settings in field order, then
/// `groups` when it joined any, and which groups those are.
///
/// # Arguments
///
/// * `id` - The site id
/// * `before` - The site before the edit
/// * `after` - The site as saved
/// * `joined` - The groups it joined
fn edit_report(id: &str, before: &Site, after: &Site, joined: &[String]) -> String {
    let mut names: Vec<&str> = field_table::changed(before, after)
        .into_iter()
        .map(TableField::name)
        .collect();
    if !joined.is_empty() {
        names.push(GROUPS_CHANGE);
    }
    let line = field_table::names_line(
        &format!("Updated site '{id}'"),
        &format!("Site '{id}' is unchanged."),
        &names,
    );
    line + &joined_text(joined)
}

/// The library's refusal of a new site: the flags named, and the root
/// remedy when the refusal includes the root rule.
fn refused_new_site(inventory: &Inventory, error: apvm_sites::Error, site: &Site) -> Failure {
    let remedy = root_remedy(inventory, site, &error, RootChange::Kept);
    let failure = name_flags(error, &fields::flag_map());
    match remedy {
        Some(remedy) => failure.with_remedy(remedy),
        None => failure,
    }
}

/// What the command did to `allow_root`, which decides how the root
/// remedy reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootChange {
    /// `site add`, or an edit without `--unset allow-root`.
    Kept,
    /// `--unset allow-root` on a site that allowed root.
    Revoked,
    /// `--unset allow-root` on a site that never allowed root.
    NeverAllowed,
}

impl RootChange {
    /// What an edit of site `id` with `unset` does to `allow_root`.
    ///
    /// # Arguments
    ///
    /// * `inventory` - The inventory before the edit
    /// * `id` - The site id
    /// * `unset` - What `--unset` clears
    fn of_edit(inventory: &Inventory, id: &str, unset: &[Field]) -> Self {
        if !unset.contains(&Field::AllowRoot) {
            return Self::Kept;
        }
        let allowed = inventory
            .site(id)
            .is_some_and(|site| site.allow_root == Some(true));
        if allowed {
            Self::Revoked
        } else {
            Self::NeverAllowed
        }
    }

    /// The remedy for a site refused for naming root without
    /// `allow_root`: the library says `'root' needs allow_root: true`,
    /// which names the file's field, not the flag.
    fn remedy(self) -> &'static str {
        match self {
            Self::Kept => {
                "root needs --allow-root: pass it, or name another user in --run-as/--owner"
            }
            Self::Revoked => {
                "--run-as/--owner name root, which needs --allow-root: keep it \
                 (drop --unset allow-root), or name another user there first"
            }
            Self::NeverAllowed => {
                "root needs --allow-root: pass --allow-root instead of --unset allow-root, \
                 or name another user in --run-as/--owner"
            }
        }
    }
}

/// The root remedy, when the library's refusal of `site` includes its root
/// rule (see [`root_is_refused`]); `None` for any other refusal.
///
/// # Arguments
///
/// * `inventory` - The inventory the site was refused by (unchanged)
/// * `site` - The refused site record
/// * `error` - The library's refusal
/// * `change` - What the command did to `allow_root`
fn root_remedy(
    inventory: &Inventory,
    site: &Site,
    error: &apvm_sites::Error,
    change: RootChange,
) -> Option<String> {
    let apvm_sites::Error::InvalidInput { problems, .. } = error else {
        return None;
    };
    root_is_refused(inventory, site, problems.len()).then(|| change.remedy().to_string())
}

/// Whether the root rule is among the `refused` problems of `site`: the
/// same record with `allow_root` set has fewer problems. Asking the
/// library again keeps this structural: no message text is parsed, and a
/// value refused before the root check (`auto:root`) does not count.
///
/// # Arguments
///
/// * `inventory` - The inventory the site was refused by (unchanged)
/// * `site` - The refused site record
/// * `refused` - How many problems the library found
fn root_is_refused(inventory: &Inventory, site: &Site, refused: usize) -> bool {
    if site.allow_root == Some(true) {
        return false;
    }
    let allowed = Site {
        allow_root: Some(true),
        ..site.clone()
    };
    let mut scratch = inventory.clone();
    let outcome = if scratch.site(&site.id).is_some() {
        scratch.update_site(allowed).map(drop)
    } else {
        scratch.add_site(allowed)
    };
    match outcome {
        Ok(()) => true,
        Err(apvm_sites::Error::InvalidInput { problems, .. }) => problems.len() < refused,
        Err(_) => false,
    }
}

/// How an edit's report names joined groups among the changed settings.
const GROUPS_CHANGE: &str = "groups";

/// The site `id` of `inventory`.
///
/// # Errors
///
/// A [`not_found`] failure when there is none.
fn current(inventory: &Inventory, id: &str) -> Result<Site, Failure> {
    let site = inventory.site(id).cloned();
    site.ok_or_else(|| not_found(EntityKind::Site, id))
}

/// Add site `site` to each group of `groups` that does not list it yet.
///
/// # Arguments
///
/// * `inventory` - The (locked) inventory, which already holds the site
/// * `site` - The site id
/// * `groups` - The group ids, as the user named them (repeats count once)
///
/// # Returns
///
/// The groups it joined, in the order named.
///
/// # Errors
///
/// A [`not_found`] failure for an unknown group; the library's refusal
/// (not expected: the site exists and is listed once).
fn join_groups(
    inventory: &mut Inventory,
    site: &str,
    groups: &[String],
) -> Result<Vec<String>, Failure> {
    let mut joined = Vec::new();
    for id in groups {
        let mut group = inventory
            .group(id)
            .cloned()
            .ok_or_else(|| not_found(EntityKind::Group, id))?;
        if group.sites.iter().any(|member| member == site) {
            continue;
        }
        group.sites.push(site.to_string());
        inventory.update_group(group)?;
        joined.push(id.clone());
    }
    Ok(joined)
}

/// What a change reports about the groups the site joined (nothing when
/// it joined none).
fn joined_text(joined: &[String]) -> String {
    if joined.is_empty() {
        return String::new();
    }
    format!(
        "It joined {}: {}.\n",
        count(joined.len(), "group"),
        quoted_ids(joined)
    )
}

/// `apvm site list`.
fn list(path: &Path, args: &ListArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let inventory = load(path, console)?;
    let sites = filter_sites(&inventory, &args.servers, &args.groups)?;
    let filtered = !args.servers.is_empty() || !args.groups.is_empty();
    console.print(&render::list_view(&inventory, &sites, filtered, args.json)?);
    Ok(())
}

/// The sites on any of `servers` and in any of `groups` (an empty filter
/// keeps every site), in inventory order.
///
/// # Errors
///
/// Those of [`ensure_known`]: a filter never silently matches nothing.
fn filter_sites<'a>(
    inventory: &'a Inventory,
    servers: &[String],
    groups: &[String],
) -> Result<Vec<&'a Site>, Failure> {
    ensure_known(inventory, servers, groups)?;
    let on_servers = |site: &Site| servers.is_empty() || servers.contains(&site.server);
    let in_groups = |site: &Site| {
        groups.is_empty()
            || inventory
                .groups_of(&site.id)
                .any(|g| groups.contains(&g.id))
    };
    Ok(inventory
        .sites()
        .iter()
        .filter(|site| on_servers(site) && in_groups(site))
        .collect())
}

/// Refuse a filter that names a server or group the inventory lacks.
///
/// # Errors
///
/// A [`not_found`] failure for the first unknown server, else the first
/// unknown group.
fn ensure_known(
    inventory: &Inventory,
    servers: &[String],
    groups: &[String],
) -> Result<(), Failure> {
    if let Some(unknown) = servers.iter().find(|id| inventory.server(id).is_none()) {
        return Err(not_found(EntityKind::Server, unknown));
    }
    match groups.iter().find(|id| inventory.group(id).is_none()) {
        Some(unknown) => Err(not_found(EntityKind::Group, unknown)),
        None => Ok(()),
    }
}

/// `apvm site show`.
fn show(path: &Path, args: &ShowArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let inventory = load(path, console)?;
    let site = inventory
        .site(&args.id)
        .ok_or_else(|| not_found(EntityKind::Site, &args.id))?;
    let effective = inventory.resolve_site(&args.id)?;
    console.print(&render::show_view(site, &inventory, &effective, args.json)?);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Removal
// ─────────────────────────────────────────────────────────────────────────────

/// `apvm site remove`: confirm what will go (unless `-y`), then remove it,
/// refusing when it changed in between.
fn remove(path: &Path, args: &RemoveArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    if let Some(removal) = remove_confirmed(path, args, args.yes, console)? {
        console.print(&removed_text(&removal));
    }
    Ok(())
}

/// The site and its group memberships, asked about first and re-checked
/// under the lock.
impl Removal for RemoveArgs {
    type Removed = SiteRemoval;
    type Scope = RemovalScope;

    fn remove(&self, inventory: &mut Inventory) -> Result<SiteRemoval, apvm_sites::Error> {
        inventory.remove_site(&self.id)
    }

    fn scope(removed: &SiteRemoval) -> RemovalScope {
        RemovalScope {
            site: removed.site.clone(),
            groups: removed.groups.clone(),
        }
    }

    fn question(removed: &SiteRemoval) -> String {
        removal_question(removed)
    }

    fn refusal(&self) -> String {
        format!(
            "not removing site '{}': it needs a confirmation, and stdin is not a terminal; \
             pass -y (--yes) to remove it without asking",
            self.id
        )
    }

    fn changed(&self) -> Failure {
        Failure::Changed(format!(
            "site '{}' or its groups changed while you were confirming; nothing was removed: \
             run the command again",
            self.id
        ))
    }
}

/// What a site removal takes with it — the record and the groups it
/// leaves — compared before and after the confirmation.
#[derive(Debug, PartialEq, Eq)]
struct RemovalScope {
    /// The site record (its server, path and settings included).
    site: Site,
    /// Ids of the groups it leaves.
    groups: Vec<String>,
}

/// The confirmation question for a removal.
fn removal_question(removal: &SiteRemoval) -> String {
    let id = &removal.site.id;
    if removal.groups.is_empty() {
        return format!("Remove site '{id}' from the inventory?");
    }
    format!(
        "Remove site '{id}' from the inventory (it also leaves {}: {})?",
        count(removal.groups.len(), "group"),
        quoted_ids(&removal.groups)
    )
}

/// What a removal reports.
fn removed_text(removal: &SiteRemoval) -> String {
    let mut text = format!("Removed site '{}'.\n", removal.site.id);
    if !removal.groups.is_empty() {
        text.push_str(&format!(
            "It was removed from {}: {}.\n",
            count(removal.groups.len(), "group"),
            quoted_ids(&removal.groups)
        ));
    }
    text
}

#[cfg(test)]
mod tests;
