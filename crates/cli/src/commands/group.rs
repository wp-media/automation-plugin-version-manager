//! `apvm group` — named lists of sites in the site inventory (plan §5.2),
//! which `--group` targets. It never connects to the sites (no SSH): it
//! reads or edits the inventory only, and removing a group or a member
//! keeps the sites themselves.
//!
//! ```text
//! apvm group add <ID> [<SITE>…] [--notes <TEXT>]
//! apvm group edit <ID> [--add <SITE>]… [--remove <SITE>]… [--notes <TEXT>] [--unset notes]
//! apvm group list [--json]
//! apvm group show <ID> [--json]
//! apvm group remove <ID> [-y]
//! ```
//!
//! Sites are named by id, repeatable and comma-separated, and must exist:
//! the first unknown one is `site '…' does not exist`. A repeat counts once,
//! adding a member or removing a non-member changes nothing, and members
//! keep their order (new ones go last). The notes are declared once, in
//! [`fields`]. Each change is one `io::update` (lock → load → change →
//! save), so concurrent edits all land; a refused change writes nothing.
//! Removing a site (`apvm site remove`, `apvm server remove --cascade`)
//! takes it out of every group in the same change.

use std::path::Path;

use super::inventory::field_table::{self, Change, TableField};
use super::inventory::{
    Console, Failure, Removal, count, load, name_flags, not_found, print_json_failure, quoted_ids,
    remove_confirmed,
};
use apvm_sites::inventory::io;
use apvm_sites::{EntityKind, Group, Inventory};
use clap::{ArgGroup, Args, Subcommand};
use fields::{Field, Scope, Setting};

mod fields;
mod render;

// ─────────────────────────────────────────────────────────────────────────────
// Arguments
// ─────────────────────────────────────────────────────────────────────────────

/// Arguments for `apvm group`.
#[derive(Args, Debug)]
pub struct GroupArgs {
    /// What to do with the groups.
    #[command(subcommand)]
    action: GroupAction,
}

/// `apvm group` subcommands.
#[derive(Subcommand, Debug)]
enum GroupAction {
    /// Add a group: a named list of sites
    Add(AddArgs),
    /// Change a group's sites or notes (at least one option)
    Edit(EditArgs),
    /// List the groups and their sites
    List(ListArgs),
    /// Show a group
    Show(ShowArgs),
    /// Remove a group from the inventory (its sites stay)
    Remove(RemoveArgs),
}

/// The optional group settings, shared by `group add` and `group edit`.
#[derive(Args, Debug, Default)]
struct GroupFlags {
    /// Free-form notes (1–1024 characters)
    #[arg(long, value_name = "TEXT")]
    notes: Option<String>,
}

impl GroupFlags {
    /// The settings these flags carry. Every field is destructured, so a new
    /// flag does not compile until it is listed here.
    #[deny(unused_variables)]
    fn settings(&self) -> Vec<Setting> {
        let Self { notes } = self;
        [notes.clone().map(Setting::Notes)]
            .into_iter()
            .flatten()
            .collect()
    }
}

/// Arguments for `apvm group add`.
#[derive(Args, Debug)]
struct AddArgs {
    /// Group id: lowercase letters, digits, '.', '_' and '-' (e.g. qa)
    id: String,
    /// Sites of the group, in order (existing site ids; comma-separated lists allowed)
    #[arg(value_name = "SITE", value_delimiter = ',')]
    sites: Vec<String>,
    /// Optional settings.
    #[command(flatten)]
    flags: GroupFlags,
}

/// Arguments for `apvm group edit`: at least one change.
#[derive(Args, Debug)]
#[command(
    override_usage = "apvm group edit <ID> <OPTION>…",
    group(
        ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args(["add", "remove", "notes", "unset"])
    )
)]
struct EditArgs {
    /// Id of the group to change
    id: String,
    /// Add an existing site to the group, after its members (repeatable, or comma-separated)
    #[arg(long, value_name = "SITE", value_delimiter = ',')]
    add: Vec<String>,
    /// Take a site out of the group; the site stays in the inventory (repeatable, or comma-separated)
    #[arg(long, value_name = "SITE", value_delimiter = ',')]
    remove: Vec<String>,
    /// Optional settings.
    #[command(flatten)]
    flags: GroupFlags,
    /// Clear a setting (repeatable, or comma-separated)
    #[arg(
        long,
        value_name = "FIELD",
        value_delimiter = ',',
        value_parser = fields::unset_parser()
    )]
    unset: Vec<Field>,
}

/// Arguments for `apvm group list`.
#[derive(Args, Debug)]
struct ListArgs {
    /// Print the groups as JSON
    #[arg(long)]
    json: bool,
}

/// Arguments for `apvm group show`.
#[derive(Args, Debug)]
struct ShowArgs {
    /// Id of the group to show
    id: String,
    /// Print the group as JSON
    #[arg(long)]
    json: bool,
}

/// Arguments for `apvm group remove`.
#[derive(Args, Debug)]
struct RemoveArgs {
    /// Id of the group to remove
    id: String,
    /// Skip the confirmation prompt (required when stdin is not a terminal)
    #[arg(short = 'y', long = "yes")]
    yes: bool,
}

impl AddArgs {
    /// The group these arguments describe (validated by the inventory):
    /// the sites in the order named, each once.
    #[deny(unused_variables)]
    fn to_group(&self) -> Group {
        let Self { id, sites, flags } = self;
        let group = Group::new(id.clone(), distinct(sites));
        field_table::apply(&group, &flags.settings(), &[])
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

impl GroupArgs {
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
            GroupAction::Add(args) => add(path, args, console),
            GroupAction::Edit(args) => edit(path, args, console),
            GroupAction::List(args) => list(path, args, console),
            GroupAction::Show(args) => show(path, args, console),
            GroupAction::Remove(args) => remove(path, args, console),
        };
        print_json_failure(&result, self.json(), console);
        result
    }

    /// Whether the user asked for JSON output.
    fn json(&self) -> bool {
        match &self.action {
            GroupAction::List(args) => args.json,
            GroupAction::Show(args) => args.json,
            GroupAction::Add(_) | GroupAction::Edit(_) | GroupAction::Remove(_) => false,
        }
    }
}

/// `apvm group add`, in one `io::update`: the group without its sites
/// first, so a read-only inventory, a taken or invalid id and invalid
/// notes are reported before an unknown site (as `site add` reports its
/// own record first); then the sites are checked and stored. A refusal at
/// any step writes nothing.
fn add(path: &Path, args: &AddArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let group = args.to_group();
    let refused = |error| name_flags(error, &fields::flag_map());
    io::update(path, |inventory| {
        let bare = Group {
            sites: Vec::new(),
            ..group.clone()
        };
        inventory.add_group(bare).map_err(refused)?;
        ensure_sites(inventory, &group.sites)?;
        inventory.update_group(group.clone()).map_err(refused)
    })?;
    console.print(&added_text(&group));
    Ok(())
}

/// What `group add` reports: the group, then its sites when it has any.
fn added_text(group: &Group) -> String {
    let mut text = format!("Added group '{}'.\n", group.id);
    if !group.sites.is_empty() {
        text.push_str(&format!(
            "It lists {}: {}.\n",
            count(group.sites.len(), "site"),
            quoted_ids(&group.sites)
        ));
    }
    text
}

/// `apvm group edit`, in one `io::update`: the settings first, so a
/// read-only inventory and invalid notes are reported before an unknown
/// site (as `site edit` reports its own record first); then the members.
/// A refusal at any step writes nothing.
fn edit(path: &Path, args: &EditArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let settings = args.flags.settings();
    let change = Change::new(Scope::Group, &settings, &args.unset)?;
    let (before, after, members) = io::update(path, |inventory| {
        let (before, mut after) = change.apply_to(
            inventory,
            |inventory| current(inventory, &args.id),
            |inventory, group| inventory.update_group(group),
        )?;
        ensure_sites(inventory, args.add.iter().chain(&args.remove))?;
        let members = Membership::edit(&before.sites, &args.add, &args.remove)?;
        after.sites.clone_from(&members.sites);
        if let Err(error) = inventory.update_group(after.clone()) {
            return Err(change.refused(error));
        }
        Ok::<_, Failure>((before, after, members))
    })?;
    console.print(&edit_report(&args.id, &before, &after, &members));
    Ok(())
}

/// How an edit changes a group's members.
#[derive(Debug, Default, PartialEq, Eq)]
struct Membership {
    /// The members after the edit: those that stay, in their order, then
    /// the added ones, in the order named.
    sites: Vec<String>,
    /// The sites that joined, in the order named.
    added: Vec<String>,
    /// The sites that left, in the group's order.
    removed: Vec<String>,
}

impl Membership {
    /// The members of a group after adding `add` and removing `remove`.
    /// Adding a member or removing a non-member changes nothing; a repeat
    /// counts once.
    ///
    /// # Arguments
    ///
    /// * `members` - The group's current members
    /// * `add` - The sites to add, as named
    /// * `remove` - The sites to remove, as named
    ///
    /// # Errors
    ///
    /// [`Failure::Usage`] when a site is both added and removed (the ids
    /// are known sites by then, so the message can show them).
    fn edit(members: &[String], add: &[String], remove: &[String]) -> Result<Self, Failure> {
        if let Some(both) = add.iter().find(|id| remove.contains(id)) {
            return Err(Failure::Usage(format!(
                "--add '{both}' and --remove '{both}' contradict each other: pass only one"
            )));
        }
        let (removed, mut sites): (Vec<String>, Vec<String>) =
            members.iter().cloned().partition(|id| remove.contains(id));
        let mut added = Vec::new();
        for id in distinct(add) {
            if !sites.contains(&id) {
                sites.push(id.clone());
                added.push(id);
            }
        }
        Ok(Self {
            sites,
            added,
            removed,
        })
    }

    /// Whether the members change.
    fn changed(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }
}

/// How an edit's report names a change of the members among the changed
/// settings.
const SITES_CHANGE: &str = "sites";

/// What an edit reports: `sites` when the members changed, then the
/// changed settings in field order, then which sites joined and left.
///
/// # Arguments
///
/// * `id` - The group id
/// * `before` - The group before the edit
/// * `after` - The group as saved
/// * `members` - How the members changed
fn edit_report(id: &str, before: &Group, after: &Group, members: &Membership) -> String {
    let settings = field_table::changed(before, after)
        .into_iter()
        .map(TableField::name);
    let names: Vec<&str> = members
        .changed()
        .then_some(SITES_CHANGE)
        .into_iter()
        .chain(settings)
        .collect();
    let mut text = field_table::names_line(
        &format!("Updated group '{id}'"),
        &format!("Group '{id}' is unchanged."),
        &names,
    );
    text.push_str(&sites_line("Added", "to", &members.added));
    text.push_str(&sites_line("Removed", "from", &members.removed));
    text
}

/// `"Added 1 site to it: 'blog'.\n"`, or nothing when `sites` is empty.
///
/// # Arguments
///
/// * `verb` - What happened (`Added`, `Removed`)
/// * `preposition` - How the sites relate to the group (`to`, `from`)
/// * `sites` - The sites (inventory ids)
fn sites_line(verb: &str, preposition: &str, sites: &[String]) -> String {
    if sites.is_empty() {
        return String::new();
    }
    format!(
        "{verb} {} {preposition} it: {}.\n",
        count(sites.len(), "site"),
        quoted_ids(sites)
    )
}

/// The group `id` of `inventory`.
///
/// # Errors
///
/// A [`not_found`] failure when there is none.
fn current(inventory: &Inventory, id: &str) -> Result<Group, Failure> {
    let group = inventory.group(id).cloned();
    group.ok_or_else(|| not_found(EntityKind::Group, id))
}

/// Refuse a site the inventory lacks, so a group never names one (the
/// library would also refuse it, but located in the record: `sites[2]`).
///
/// # Arguments
///
/// * `inventory` - The (locked) inventory
/// * `sites` - The site ids, as named
///
/// # Errors
///
/// A [`not_found`] failure for the first unknown site.
fn ensure_sites<'a>(
    inventory: &Inventory,
    sites: impl IntoIterator<Item = &'a String>,
) -> Result<(), Failure> {
    match sites.into_iter().find(|id| inventory.site(id).is_none()) {
        Some(unknown) => Err(not_found(EntityKind::Site, unknown)),
        None => Ok(()),
    }
}

/// `ids` in their order, each once (a repeat counts once, as for
/// `site --group`).
fn distinct(ids: &[String]) -> Vec<String> {
    let mut seen = Vec::with_capacity(ids.len());
    for id in ids {
        if !seen.contains(id) {
            seen.push(id.clone());
        }
    }
    seen
}

/// `apvm group list`.
fn list(path: &Path, args: &ListArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let inventory = load(path, console)?;
    console.print(&render::list_view(inventory.groups(), args.json)?);
    Ok(())
}

/// `apvm group show`.
fn show(path: &Path, args: &ShowArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    let inventory = load(path, console)?;
    let group = current(&inventory, &args.id)?;
    console.print(&render::show_view(&group, args.json)?);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Removal
// ─────────────────────────────────────────────────────────────────────────────

/// `apvm group remove`: confirm (unless `-y`), then remove the group,
/// refusing when it changed in between. Its sites stay.
fn remove(path: &Path, args: &RemoveArgs, console: &mut Console<'_>) -> Result<(), Failure> {
    if let Some(group) = remove_confirmed(path, args, args.yes, console)? {
        console.print(&removed_text(&group));
    }
    Ok(())
}

/// The group record (its members and notes), asked about first and
/// re-checked under the lock.
impl Removal for RemoveArgs {
    type Removed = Group;
    type Scope = Group;

    fn remove(&self, inventory: &mut Inventory) -> Result<Group, apvm_sites::Error> {
        inventory.remove_group(&self.id)
    }

    fn scope(removed: &Group) -> Group {
        removed.clone()
    }

    fn question(removed: &Group) -> String {
        removal_question(removed)
    }

    fn refusal(&self) -> String {
        format!(
            "not removing group '{}': it needs a confirmation, and stdin is not a terminal; \
             pass -y (--yes) to remove it without asking",
            self.id
        )
    }

    fn changed(&self) -> Failure {
        Failure::Changed(format!(
            "group '{}' changed while you were confirming; nothing was removed: \
             run the command again",
            self.id
        ))
    }
}

/// The confirmation question for a removal.
fn removal_question(group: &Group) -> String {
    let id = &group.id;
    if group.sites.is_empty() {
        return format!("Remove group '{id}' from the inventory?");
    }
    format!(
        "Remove group '{id}' from the inventory (the sites it lists stay: {})?",
        quoted_ids(&group.sites)
    )
}

/// What a removal reports.
fn removed_text(group: &Group) -> String {
    let mut text = format!("Removed group '{}'.\n", group.id);
    if !group.sites.is_empty() {
        text.push_str(&format!(
            "The sites it listed stay in the inventory: {}.\n",
            quoted_ids(&group.sites)
        ));
    }
    text
}

#[cfg(test)]
mod tests;
