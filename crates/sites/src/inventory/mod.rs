//! The site inventory: servers, the WordPress sites on them, groups of
//! sites, and default SSH settings (plan §4.1, §7).
//!
//! An [`Inventory`] is **always valid**: it is built empty, parsed from a
//! document that passes every rule, or changed through methods that check
//! the record first and change nothing when it is refused. It works on the
//! document's text; [`io`] reads and writes the file (locking, atomic
//! saves, no symbolic links).
//!
//! # Versions
//!
//! A document records its format `version`. One newer than
//! [`INVENTORY_VERSION`] is loaded **read-only** when this build can read
//! it fully: every field and value known, every rule passed. Otherwise it is
//! refused ([`Error::InventoryUnsupported`]) rather than read with parts
//! ignored, which could drop a setting that restricts what apvm does.

pub mod io;
mod model;
mod resolve;
mod validate;

pub use model::{
    Defaults, EntityKind, Group, INVENTORY_VERSION, Owner, OwnershipPolicy, RunAs, Server, Site,
    SshSettings,
};
pub use resolve::{
    BUILTIN_CONNECT_TIMEOUT_SECS, BUILTIN_MULTIPLEX, BUILTIN_REMOTE_TMP_DIR, BUILTIN_SSH_BINARY,
    BUILTIN_WP_CLI, ResolvedServer, ResolvedSite, ResolvedSsh, ResolvedTargets,
    STANDARD_PLUGINS_SUBDIR, Setting, SettingSource, TargetWarning, Targets,
};

use std::collections::HashSet;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::problem::{Problem, quoted};
use model::{Document, Record};

/// Servers, sites and groups, always valid.
///
/// # Example
///
/// ```
/// use apvm_sites::{Group, Inventory, Server, Site};
///
/// # fn main() -> apvm_sites::Result<()> {
/// let mut inventory = Inventory::new();
/// inventory.add_server(Server::new("eu-1", "203.0.113.10"))?;
/// inventory.add_site(Site::new("shop", "eu-1", "/var/www/shop/public"))?;
/// inventory.add_group(Group::new("qa", vec!["shop".into()]))?;
///
/// let saved = inventory.to_json()?;
/// assert_eq!(Inventory::from_json(&saved)?, inventory);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    /// The document; valid by construction.
    doc: Document,
}

/// What [`Inventory::remove_server`] removed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServerRemoval {
    /// The removed server.
    pub server: Server,
    /// Sites removed with it (only when the removal cascades), in inventory
    /// order.
    pub sites: Vec<Site>,
    /// Ids of the groups those sites were removed from, in inventory order.
    pub groups: Vec<String>,
}

/// What [`Inventory::remove_site`] removed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SiteRemoval {
    /// The removed site.
    pub site: Site,
    /// Ids of the groups it was removed from, in inventory order.
    pub groups: Vec<String>,
}

impl Default for Inventory {
    /// An empty inventory ([`Inventory::new`]).
    fn default() -> Self {
        Self::new()
    }
}

impl Inventory {
    /// An empty inventory of the current format version.
    pub fn new() -> Self {
        Self {
            doc: Document::empty(),
        }
    }

    /// Parse and validate an inventory document. A leading UTF-8 byte-order
    /// mark is ignored (some Windows editors write one).
    ///
    /// # Arguments
    ///
    /// * `text` - The document (JSON)
    ///
    /// # Errors
    ///
    /// - [`Error::InventoryJson`]: not JSON of the inventory's shape (syntax,
    ///   wrong type, missing `version` or other required field, unknown
    ///   field), with line and column.
    /// - [`Error::InventoryInvalid`]: it breaks rules, or holds a record
    ///   written as a JSON array; every problem is listed with its location.
    /// - [`Error::InventoryUnsupported`]: a newer format version this build
    ///   cannot read (its problems stay available).
    pub fn from_json(text: &str) -> Result<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let value: Value = serde_json::from_str(text).map_err(parse_error)?;
        let parsed = Self::parse(text, &value);
        match newer_version(&value) {
            Some(found) => parsed.map_err(|source| Error::InventoryUnsupported {
                found,
                supported: INVENTORY_VERSION,
                source: Box::new(source),
            }),
            None => parsed,
        }
    }

    /// Strictly parse (for positioned errors) and fully validate a document
    /// whose JSON is `value`.
    fn parse(text: &str, value: &Value) -> Result<Self> {
        let doc: Document = serde_json::from_str(text).map_err(parse_error)?;
        let mut problems = validate::json_shape(value);
        if problems.is_empty() {
            problems = validate::document(&doc);
        }
        if !problems.is_empty() {
            return Err(Error::InventoryInvalid { problems });
        }
        Ok(Self { doc })
    }

    /// The document to save: pretty-printed JSON with a trailing newline.
    /// Unset optional fields are omitted.
    ///
    /// # Errors
    ///
    /// - [`Error::InventoryReadOnly`]: the inventory has a newer format
    ///   version.
    /// - [`Error::InventoryJson`]: serialization failed (not expected: every
    ///   field is a string, number, boolean, list or record).
    pub fn to_json(&self) -> Result<String> {
        self.ensure_writable()?;
        let mut text =
            serde_json::to_string_pretty(&self.doc).map_err(|source| Error::InventoryJson {
                context: "cannot serialize the inventory",
                source,
            })?;
        text.push('\n');
        Ok(text)
    }

    /// The document's format version.
    pub fn version(&self) -> u32 {
        self.doc.version
    }

    /// Whether the inventory has a newer format version, so it can be read
    /// but not changed or saved.
    pub fn is_read_only(&self) -> bool {
        self.doc.version > INVENTORY_VERSION
    }

    /// Settings every server inherits.
    pub fn defaults(&self) -> &Defaults {
        &self.doc.defaults
    }

    /// Every server, in inventory order.
    pub fn servers(&self) -> &[Server] {
        &self.doc.servers
    }

    /// Every site, in inventory order.
    pub fn sites(&self) -> &[Site] {
        &self.doc.sites
    }

    /// Every group, in inventory order.
    pub fn groups(&self) -> &[Group] {
        &self.doc.groups
    }

    /// The server with this id.
    pub fn server(&self, id: &str) -> Option<&Server> {
        find(&self.doc.servers, id)
    }

    /// The site with this id.
    pub fn site(&self, id: &str) -> Option<&Site> {
        find(&self.doc.sites, id)
    }

    /// The group with this id.
    pub fn group(&self, id: &str) -> Option<&Group> {
        find(&self.doc.groups, id)
    }

    /// The sites on a server, in inventory order.
    ///
    /// # Arguments
    ///
    /// * `server` - The server id
    pub fn sites_on<'a>(&'a self, server: &'a str) -> impl Iterator<Item = &'a Site> + 'a {
        self.doc
            .sites
            .iter()
            .filter(move |site| site.server == server)
    }

    /// The groups that list a site, in inventory order.
    ///
    /// # Arguments
    ///
    /// * `site` - The site id
    pub fn groups_of<'a>(&'a self, site: &'a str) -> impl Iterator<Item = &'a Group> + 'a {
        self.doc
            .groups
            .iter()
            .filter(move |group| group.sites.iter().any(|member| member == site))
    }

    /// Replace the default settings.
    ///
    /// # Arguments
    ///
    /// * `defaults` - The new defaults
    ///
    /// # Returns
    ///
    /// The previous defaults.
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::InvalidInput`] when a value
    /// breaks a rule (nothing changes).
    pub fn set_defaults(&mut self, defaults: Defaults) -> Result<Defaults> {
        self.ensure_writable()?;
        refuse("defaults".into(), validate::defaults_record(&defaults))?;
        Ok(std::mem::replace(&mut self.doc.defaults, defaults))
    }

    /// Add a server.
    ///
    /// # Arguments
    ///
    /// * `server` - The server; its id must be new
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::AlreadyExists`];
    /// [`Error::InvalidInput`] (nothing changes).
    pub fn add_server(&mut self, server: Server) -> Result<()> {
        self.ensure_writable()?;
        ensure_absent(&self.doc.servers, &server.id)?;
        refuse(subject(&server), validate::server_record(&server))?;
        self.doc.servers.push(server);
        Ok(())
    }

    /// Replace the server with the same id.
    ///
    /// # Arguments
    ///
    /// * `server` - The new server record
    ///
    /// # Returns
    ///
    /// The previous server.
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::NotFound`];
    /// [`Error::InvalidInput`] (nothing changes).
    pub fn update_server(&mut self, server: Server) -> Result<Server> {
        self.ensure_writable()?;
        let slot = slot(&mut self.doc.servers, &server.id)?;
        refuse(subject(&server), validate::server_record(&server))?;
        Ok(std::mem::replace(slot, server))
    }

    /// Remove a server. While sites are on it, the removal is refused unless
    /// it cascades: then those sites are removed too, and leave their
    /// groups.
    ///
    /// # Arguments
    ///
    /// * `id` - The server id
    /// * `cascade` - Whether to remove the server's sites with it
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::NotFound`];
    /// [`Error::ServerInUse`] without `cascade` (nothing changes).
    pub fn remove_server(&mut self, id: &str, cascade: bool) -> Result<ServerRemoval> {
        self.ensure_writable()?;
        ensure_present(&self.doc.servers, id)?;
        let dependents: Vec<String> = self.sites_on(id).map(|site| site.id.clone()).collect();
        if !dependents.is_empty() && !cascade {
            return Err(Error::ServerInUse {
                id: id.to_string(),
                sites: dependents,
            });
        }
        let server = take(&mut self.doc.servers, id)?;
        let (sites, kept): (Vec<Site>, Vec<Site>) = std::mem::take(&mut self.doc.sites)
            .into_iter()
            .partition(|site| site.server == server.id);
        self.doc.sites = kept;
        let groups = self.leave_groups(&sites.iter().map(Record::id).collect());
        Ok(ServerRemoval {
            server,
            sites,
            groups,
        })
    }

    /// Add a site.
    ///
    /// # Arguments
    ///
    /// * `site` - The site; its id must be new and its server must exist
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::AlreadyExists`];
    /// [`Error::InvalidInput`], including an unknown server (nothing
    /// changes).
    pub fn add_site(&mut self, site: Site) -> Result<()> {
        self.ensure_writable()?;
        ensure_absent(&self.doc.sites, &site.id)?;
        check_site(&self.doc.servers, &site)?;
        self.doc.sites.push(site);
        Ok(())
    }

    /// Replace the site with the same id (it may move to another server).
    ///
    /// # Arguments
    ///
    /// * `site` - The new site record
    ///
    /// # Returns
    ///
    /// The previous site.
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::NotFound`];
    /// [`Error::InvalidInput`], including an unknown server (nothing
    /// changes).
    pub fn update_site(&mut self, site: Site) -> Result<Site> {
        self.ensure_writable()?;
        let Document { servers, sites, .. } = &mut self.doc;
        let slot = slot(sites, &site.id)?;
        check_site(servers, &site)?;
        Ok(std::mem::replace(slot, site))
    }

    /// Remove a site; it also leaves every group that lists it.
    ///
    /// # Arguments
    ///
    /// * `id` - The site id
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::NotFound`].
    pub fn remove_site(&mut self, id: &str) -> Result<SiteRemoval> {
        self.ensure_writable()?;
        let site = take(&mut self.doc.sites, id)?;
        let groups = self.leave_groups(&HashSet::from([site.id.as_str()]));
        Ok(SiteRemoval { site, groups })
    }

    /// Add a group.
    ///
    /// # Arguments
    ///
    /// * `group` - The group; its id must be new and its sites must exist
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::AlreadyExists`];
    /// [`Error::InvalidInput`], including unknown or repeated sites
    /// (nothing changes).
    pub fn add_group(&mut self, group: Group) -> Result<()> {
        self.ensure_writable()?;
        ensure_absent(&self.doc.groups, &group.id)?;
        check_group(&self.doc.sites, &group)?;
        self.doc.groups.push(group);
        Ok(())
    }

    /// Replace the group with the same id.
    ///
    /// # Arguments
    ///
    /// * `group` - The new group record
    ///
    /// # Returns
    ///
    /// The previous group.
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::NotFound`];
    /// [`Error::InvalidInput`], including unknown or repeated sites
    /// (nothing changes).
    pub fn update_group(&mut self, group: Group) -> Result<Group> {
        self.ensure_writable()?;
        let Document { sites, groups, .. } = &mut self.doc;
        let slot = slot(groups, &group.id)?;
        check_group(sites, &group)?;
        Ok(std::mem::replace(slot, group))
    }

    /// Remove a group; its sites stay.
    ///
    /// # Arguments
    ///
    /// * `id` - The group id
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`]; [`Error::NotFound`].
    pub fn remove_group(&mut self, id: &str) -> Result<Group> {
        self.ensure_writable()?;
        take(&mut self.doc.groups, id)
    }

    /// Refuse changes to an inventory with a newer format version.
    ///
    /// # Errors
    ///
    /// [`Error::InventoryReadOnly`].
    fn ensure_writable(&self) -> Result<()> {
        if self.is_read_only() {
            return Err(Error::InventoryReadOnly {
                found: self.doc.version,
                supported: INVENTORY_VERSION,
            });
        }
        Ok(())
    }

    /// Remove `sites` from every group.
    ///
    /// # Returns
    ///
    /// Ids of the groups that listed any of them, in inventory order.
    fn leave_groups(&mut self, sites: &HashSet<&str>) -> Vec<String> {
        let mut touched = Vec::new();
        for group in &mut self.doc.groups {
            let before = group.sites.len();
            group
                .sites
                .retain(|member| !sites.contains(member.as_str()));
            if group.sites.len() != before {
                touched.push(group.id.clone());
            }
        }
        touched
    }
}

/// [`Error::InventoryJson`] for a document that does not parse.
fn parse_error(source: serde_json::Error) -> Error {
    Error::InventoryJson {
        context: "cannot parse the inventory",
        source,
    }
}

/// The document's `version` when it is newer than this build's. Read from
/// a JSON object only: a document written as an array has no field names,
/// so its first element is no version.
fn newer_version(value: &Value) -> Option<u32> {
    let version = value.get("version")?.as_u64()?;
    u32::try_from(version)
        .ok()
        .filter(|version| *version > INVENTORY_VERSION)
}

/// Check a site against the rules and the servers it may reference.
///
/// # Errors
///
/// [`Error::InvalidInput`] listing every problem.
fn check_site(servers: &[Server], site: &Site) -> Result<()> {
    let servers = servers.iter().map(Record::id).collect();
    refuse(subject(site), validate::site_record(site, &servers))
}

/// Check a group against the rules and the sites it may reference.
///
/// # Errors
///
/// [`Error::InvalidInput`] listing every problem.
fn check_group(sites: &[Site], group: &Group) -> Result<()> {
    let sites = sites.iter().map(Record::id).collect();
    refuse(subject(group), validate::group_record(group, &sites))
}

/// The record with this id.
fn find<'a, T: Record>(items: &'a [T], id: &str) -> Option<&'a T> {
    items.iter().find(|item| item.id() == id)
}

/// The record with this id, to replace.
///
/// # Errors
///
/// [`Error::NotFound`] when there is none.
fn slot<'a, T: Record>(items: &'a mut [T], id: &str) -> Result<&'a mut T> {
    items
        .iter_mut()
        .find(|item| item.id() == id)
        .ok_or_else(|| not_found::<T>(id))
}

/// Remove the record with this id, keeping the others in order.
///
/// # Errors
///
/// [`Error::NotFound`] when there is none.
fn take<T: Record>(items: &mut Vec<T>, id: &str) -> Result<T> {
    let index = items
        .iter()
        .position(|item| item.id() == id)
        .ok_or_else(|| not_found::<T>(id))?;
    Ok(items.remove(index))
}

/// Refuse an id that no record has.
///
/// # Errors
///
/// [`Error::NotFound`] when there is none.
fn ensure_present<T: Record>(items: &[T], id: &str) -> Result<()> {
    find(items, id).map(drop).ok_or_else(|| not_found::<T>(id))
}

/// Refuse an id that is already taken.
///
/// # Errors
///
/// [`Error::AlreadyExists`] when a record has this id.
fn ensure_absent<T: Record>(items: &[T], id: &str) -> Result<()> {
    if find(items, id).is_some() {
        return Err(Error::AlreadyExists {
            kind: T::KIND,
            id: id.to_string(),
        });
    }
    Ok(())
}

/// [`Error::NotFound`] for a record of kind `T`.
fn not_found<T: Record>(id: &str) -> Error {
    Error::NotFound {
        kind: T::KIND,
        id: id.to_string(),
    }
}

/// How a record is named in [`Error::InvalidInput`]: `site 'shop'`.
fn subject<T: Record>(record: &T) -> String {
    format!("{} {}", T::KIND, quoted(record.id()))
}

/// Turn problems into [`Error::InvalidInput`].
///
/// # Arguments
///
/// * `subject` - What was checked (`site 'shop'`, `defaults`)
/// * `problems` - What is wrong with it
///
/// # Errors
///
/// [`Error::InvalidInput`] when `problems` is not empty.
fn refuse(subject: String, problems: Vec<Problem>) -> Result<()> {
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::InvalidInput { subject, problems })
}

#[cfg(test)]
mod tests;
