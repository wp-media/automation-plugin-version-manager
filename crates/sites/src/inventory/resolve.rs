//! Effective settings and target selection (plan §4.1, §4.2).
//!
//! Pure: it reads an [`Inventory`], never the disk or the network.
//!
//! **Settings.** A [`ResolvedSite`] holds what apvm uses for a site, each
//! value with the level it came from ([`SettingSource`]):
//!
//! - SSH, field by field: the server's `ssh` → `defaults.ssh` → the
//!   built-in value. A field none of them sets is `None`: apvm passes
//!   nothing, so ssh decides (the user's ssh_config, or ssh's own default).
//!   Built-in values exist only for what apvm always passes:
//!   [`BUILTIN_CONNECT_TIMEOUT_SECS`], [`BUILTIN_MULTIPLEX`] and
//!   [`BUILTIN_SSH_BINARY`]. `binary` is read from `defaults.ssh` only.
//! - The site's execution settings: the site's value → the built-in value
//!   (`wp_cli` `["wp"]`, `run_as` `auto`, `owner` `auto`, `ownership`
//!   `mirror`, `plugins_dir` `<path>/wp-content/plugins`, `allow_root`
//!   `false`, `remote_tmp_dir` `/tmp`).
//!
//! **Targets.** [`Targets`] names sites, groups, servers, or every site.
//! [`Inventory::resolve_targets`] takes their union, in inventory order,
//! each site once. An unknown id is an error, and so is a selection that
//! names nothing or matches no site: it never means "everything". It warns
//! about selected groups or servers without sites, and about sites that name
//! the same WordPress install: the same path on the same ssh destination,
//! as apvm passes it to ssh ([`TargetWarning`]).
//!
//! Resolved values come from a valid inventory only, so they satisfy every
//! rule of plan §7; their constructors are private to the crate.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::Serialize;

use super::Inventory;
use super::model::{
    Defaults, EntityKind, Owner, OwnershipPolicy, RunAs, Server, Site, SshSettings,
};
use crate::error::{Error, Result};
use crate::problem::{Problem, quoted};

/// Built-in SSH connect timeout, in seconds (plan §6.1: `ConnectTimeout`
/// 15 s).
pub const BUILTIN_CONNECT_TIMEOUT_SECS: u32 = 15;

/// Built-in multiplexing setting (plan §6.1). The transport may still not
/// multiplex where OpenSSH cannot (Windows, a control path too long).
pub const BUILTIN_MULTIPLEX: bool = true;

/// Built-in ssh program: the system OpenSSH client, found on `PATH`.
pub const BUILTIN_SSH_BINARY: &str = "ssh";

/// Built-in WP-CLI command (plan §7: `wp_cli` unset is `["wp"]`).
pub const BUILTIN_WP_CLI: &str = "wp";

/// Built-in remote temporary directory, for staging and the site lock.
pub const BUILTIN_REMOTE_TMP_DIR: &str = "/tmp";

/// The standard plugins directory, relative to the WordPress root.
pub const STANDARD_PLUGINS_SUBDIR: &str = "wp-content/plugins";

/// The message for a selection that names nothing.
const NOTHING_NAMED: &str =
    "no site selected: name at least one site, group or server, or all sites";

/// Where an effective setting came from, most specific last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SettingSource {
    /// apvm's built-in value (no level of the inventory sets it).
    BuiltIn,
    /// The inventory's `defaults`.
    Defaults,
    /// The server record.
    Server,
    /// The site record.
    Site,
}

impl SettingSource {
    /// The written form: `built-in`, `defaults`, `server` or `site`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Defaults => "defaults",
            Self::Server => "server",
            Self::Site => "site",
        }
    }
}

impl fmt::Display for SettingSource {
    /// The written form ([`SettingSource::as_str`]).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An effective value and the level it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Setting<T> {
    /// The value apvm uses.
    pub value: T,
    /// Where it came from.
    pub source: SettingSource,
}

impl<T> Setting<T> {
    /// A value from `source`.
    ///
    /// # Arguments
    ///
    /// * `value` - The effective value
    /// * `source` - Where it came from
    pub const fn new(value: T, source: SettingSource) -> Self {
        Self { value, source }
    }
}

/// A server's effective SSH settings: the server's `ssh` over
/// `defaults.ssh` over the built-in values, field by field. `None` means
/// apvm passes nothing, so ssh decides (the user's ssh_config, or ssh's own
/// default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ResolvedSsh {
    /// Login user.
    pub user: Option<Setting<String>>,
    /// TCP port.
    pub port: Option<Setting<u16>>,
    /// Private key file on this machine (`~` not yet expanded).
    pub identity_file: Option<Setting<String>>,
    /// Seconds allowed to establish a connection.
    pub connect_timeout_secs: Setting<u32>,
    /// known_hosts file to use instead of the user's.
    pub known_hosts_file: Option<Setting<String>>,
    /// ssh_config file to use instead of the user's.
    pub ssh_config_file: Option<Setting<String>>,
    /// Jump hosts, as `ssh -J` takes them.
    pub proxy_jump: Option<Setting<String>>,
    /// Whether to reuse one connection per server.
    pub multiplex: Setting<bool>,
    /// The ssh program (from `defaults.ssh` only).
    pub binary: Setting<String>,
}

impl ResolvedSsh {
    /// Merge SSH settings: `server` over `defaults` over the built-in
    /// values, field by field. `binary` is read from `defaults` only (a
    /// valid inventory never sets it on a server).
    ///
    /// # Arguments
    ///
    /// * `defaults` - The inventory's `defaults.ssh`
    /// * `server` - The server's `ssh`
    pub(crate) fn merge(defaults: &SshSettings, server: &SshSettings) -> Self {
        let pick = |field: fn(&SshSettings) -> Option<&String>| layered(defaults, server, field);
        Self {
            user: pick(|ssh| ssh.user.as_ref()),
            port: layered(defaults, server, |ssh| ssh.port.as_ref()),
            identity_file: pick(|ssh| ssh.identity_file.as_ref()),
            connect_timeout_secs: layered(defaults, server, |ssh| {
                ssh.connect_timeout_secs.as_ref()
            })
            .unwrap_or_else(|| builtin(BUILTIN_CONNECT_TIMEOUT_SECS)),
            known_hosts_file: pick(|ssh| ssh.known_hosts_file.as_ref()),
            ssh_config_file: pick(|ssh| ssh.ssh_config_file.as_ref()),
            proxy_jump: pick(|ssh| ssh.proxy_jump.as_ref()),
            multiplex: layered(defaults, server, |ssh| ssh.multiplex.as_ref())
                .unwrap_or_else(|| builtin(BUILTIN_MULTIPLEX)),
            binary: from_level(defaults.binary.as_ref(), SettingSource::Defaults)
                .unwrap_or_else(|| builtin(BUILTIN_SSH_BINARY.to_string())),
        }
    }
}

/// A server with its effective SSH settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ResolvedServer {
    /// The server id.
    pub id: String,
    /// Host name, IP address or ssh_config alias.
    pub host: String,
    /// The effective SSH settings.
    pub ssh: ResolvedSsh,
}

impl ResolvedServer {
    /// Resolve a server against the inventory's defaults.
    ///
    /// # Arguments
    ///
    /// * `server` - The server record
    /// * `defaults` - The inventory's defaults
    pub(crate) fn new(server: &Server, defaults: &Defaults) -> Self {
        Self {
            id: server.id.clone(),
            host: server.host.clone(),
            ssh: ResolvedSsh::merge(&defaults.ssh, &server.ssh),
        }
    }
}

/// A site with every effective setting apvm needs to work on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ResolvedSite {
    /// The site id.
    pub id: String,
    /// The server the site is on, with its effective SSH settings.
    pub server: ResolvedServer,
    /// The WordPress root on the server.
    pub path: String,
    /// The site URL, when set (passed to WP-CLI as `--url`).
    pub url: Option<String>,
    /// The WP-CLI command.
    pub wp_cli: Setting<Vec<String>>,
    /// Who WP-CLI runs as.
    pub run_as: Setting<RunAs>,
    /// Who must own the plugin files.
    pub owner: Setting<Owner>,
    /// How ownership is checked and repaired.
    pub ownership: Setting<OwnershipPolicy>,
    /// The plugins directory: pinned by the site
    /// ([`ResolvedSite::plugins_dir_is_pinned`]), or `<path>/wp-content/plugins`.
    pub plugins_dir: Setting<String>,
    /// Whether WP-CLI may run as root, or the files be owned by root.
    pub allow_root: Setting<bool>,
    /// Remote temporary directory for staging and the site lock.
    pub remote_tmp_dir: Setting<String>,
    /// Seconds a remote command may run, when the site sets it (otherwise
    /// each operation uses its own default).
    pub command_timeout_secs: Option<u32>,
}

impl ResolvedSite {
    /// Resolve a site on its server.
    ///
    /// # Arguments
    ///
    /// * `site` - The site record
    /// * `server` - The site's server, already resolved
    pub(crate) fn new(site: &Site, server: ResolvedServer) -> Self {
        let standard_plugins_dir = || format!("{}/{STANDARD_PLUGINS_SUBDIR}", site.path);
        Self {
            id: site.id.clone(),
            server,
            path: site.path.clone(),
            url: site.url.clone(),
            wp_cli: site_or(site.wp_cli.as_ref(), || vec![BUILTIN_WP_CLI.to_string()]),
            run_as: site_or(site.run_as.as_ref(), RunAs::default),
            owner: site_or(site.owner.as_ref(), Owner::default),
            ownership: site_or(site.ownership.as_ref(), OwnershipPolicy::default),
            plugins_dir: site_or(site.plugins_dir.as_ref(), standard_plugins_dir),
            allow_root: site_or(site.allow_root.as_ref(), || false),
            remote_tmp_dir: site_or(site.remote_tmp_dir.as_ref(), || {
                BUILTIN_REMOTE_TMP_DIR.to_string()
            }),
            command_timeout_secs: site.command_timeout_secs,
        }
    }

    /// Whether the site pins its plugins directory (`plugins_dir`), so the
    /// probe accepts a `WP_PLUGIN_DIR` other than `<path>/wp-content/plugins`
    /// (plan §6.2).
    pub fn plugins_dir_is_pinned(&self) -> bool {
        self.plugins_dir.source == SettingSource::Site
    }
}

/// Which sites a command works on (plan §4.2, §5.1 TARGETS): sites,
/// every site of groups, every site on servers, or every site. The parts
/// combine as a union; duplicates are allowed and count once.
///
/// # Example
///
/// ```
/// use apvm_sites::Targets;
///
/// let targets = Targets::new().with_sites(["shop", "blog"]).with_groups(["qa"]);
/// assert_eq!(targets.sites, ["shop", "blog"]);
/// assert!(!targets.is_empty());
/// assert!(Targets::new().is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Targets {
    /// Site ids.
    pub sites: Vec<String>,
    /// Group ids: every site of each group.
    pub groups: Vec<String>,
    /// Server ids: every site on each server.
    pub servers: Vec<String>,
    /// Every site in the inventory.
    pub all_sites: bool,
}

impl Targets {
    /// A selection that names nothing yet (resolving it is an error).
    pub fn new() -> Self {
        Self::default()
    }

    /// Every site in the inventory.
    pub fn all() -> Self {
        Self::new().with_all_sites(true)
    }

    /// Add site ids.
    ///
    /// # Arguments
    ///
    /// * `ids` - Site ids
    pub fn with_sites<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.sites.extend(ids.into_iter().map(Into::into));
        self
    }

    /// Add group ids: every site of each group.
    ///
    /// # Arguments
    ///
    /// * `ids` - Group ids
    pub fn with_groups<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups.extend(ids.into_iter().map(Into::into));
        self
    }

    /// Add server ids: every site on each server.
    ///
    /// # Arguments
    ///
    /// * `ids` - Server ids
    pub fn with_servers<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.servers.extend(ids.into_iter().map(Into::into));
        self
    }

    /// Set whether every site is selected.
    ///
    /// # Arguments
    ///
    /// * `all` - Whether to select every site
    pub fn with_all_sites(mut self, all: bool) -> Self {
        self.all_sites = all;
        self
    }

    /// Whether the selection names nothing.
    pub fn is_empty(&self) -> bool {
        !self.all_sites
            && self.sites.is_empty()
            && self.groups.is_empty()
            && self.servers.is_empty()
    }
}

/// Something to tell the user about a selection that is still usable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TargetWarning {
    /// A selected group or server has no sites, so it added nothing.
    NoSites {
        /// [`EntityKind::Group`] or [`EntityKind::Server`].
        kind: EntityKind,
        /// The group or server id.
        id: String,
    },
    /// Selected sites name the same WordPress install: the same path on
    /// the same ssh destination (plan §4.2). The destination is what apvm
    /// passes to ssh, compared exactly: host, port, login user, jump hosts
    /// and ssh_config file. What only ssh_config sets counts as unset, so an
    /// unset port and `22` differ, and so do `Web` and `web` (ssh matches
    /// `Host` patterns case-sensitively). apvm works on the install once per
    /// site.
    SameWordPress {
        /// The site ids, at least two, in inventory order.
        sites: Vec<String>,
        /// The host.
        host: String,
        /// The effective port, when apvm passes one.
        port: Option<u16>,
        /// The effective login user, when apvm passes one.
        user: Option<String>,
        /// The effective jump hosts, when apvm passes them.
        proxy_jump: Option<String>,
        /// The effective ssh_config file, when apvm passes one.
        ssh_config_file: Option<String>,
        /// The WordPress root.
        path: String,
    },
}

impl fmt::Display for TargetWarning {
    /// A one-line explanation, values quoted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSites { kind, id } => write!(f, "{kind} {} has no sites", quoted(id)),
            Self::SameWordPress {
                sites,
                host,
                port,
                user,
                proxy_jump,
                ssh_config_file,
                path,
            } => {
                let ids: Vec<String> = sites.iter().map(|id| quoted(id)).collect();
                write!(
                    f,
                    "sites {} are the same WordPress install: host {}",
                    ids.join(", "),
                    quoted(host)
                )?;
                if let Some(port) = port {
                    write!(f, ", port {port}")?;
                }
                let texts = [
                    ("user", user),
                    ("jump hosts", proxy_jump),
                    ("ssh_config", ssh_config_file),
                ];
                for (label, text) in texts {
                    if let Some(text) = text {
                        write!(f, ", {label} {}", quoted(text))?;
                    }
                }
                write!(f, ", path {}; apvm works on it once per site", quoted(path))
            }
        }
    }
}

/// The sites a [`Targets`] selection resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedTargets {
    /// The selected sites, in inventory order, each once (never empty).
    pub sites: Vec<ResolvedSite>,
    /// What the user should know about the selection: groups or servers
    /// without sites (in the order named), then sites that are the same
    /// WordPress install.
    pub warnings: Vec<TargetWarning>,
}

impl ResolvedTargets {
    /// The selected site ids, in inventory order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.sites.iter().map(|site| site.id.as_str())
    }
}

impl Inventory {
    /// A server with its effective SSH settings (what `server show`
    /// displays).
    ///
    /// # Arguments
    ///
    /// * `id` - The server id
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when no server has this id.
    pub fn resolve_server(&self, id: &str) -> Result<ResolvedServer> {
        let server = self
            .server(id)
            .ok_or_else(|| not_found(EntityKind::Server, id))?;
        Ok(ResolvedServer::new(server, self.defaults()))
    }

    /// A site with every effective setting.
    ///
    /// # Arguments
    ///
    /// * `id` - The site id
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when no site has this id.
    pub fn resolve_site(&self, id: &str) -> Result<ResolvedSite> {
        let site = self
            .site(id)
            .ok_or_else(|| not_found(EntityKind::Site, id))?;
        self.resolve(site)
    }

    /// The sites a selection names: the union of its parts, in inventory
    /// order, each once, with their effective settings.
    ///
    /// # Arguments
    ///
    /// * `targets` - The selection
    ///
    /// # Returns
    ///
    /// The sites (never empty) and the warnings about the selection.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] (subject `targets`) listing every problem,
    /// located by the part that holds it (`groups[1]`): unknown ids (with a
    /// hint when only letter case or surrounding spaces differ), or a
    /// selection that names nothing or matches no site.
    pub fn resolve_targets(&self, targets: &Targets) -> Result<ResolvedTargets> {
        if targets.is_empty() {
            return Err(targets_error(vec![Problem::new("", NOTHING_NAMED)]));
        }
        let selection = Selection::collect(self, targets);
        if !selection.unknown.is_empty() {
            return Err(targets_error(selection.unknown));
        }
        let sites = self
            .sites()
            .iter()
            .filter(|site| selection.chosen.contains(site.id.as_str()))
            .map(|site| self.resolve(site))
            .collect::<Result<Vec<_>>>()?;
        if sites.is_empty() {
            return Err(targets_error(nothing_matched(selection.empty)));
        }
        let mut warnings: Vec<TargetWarning> = selection
            .empty
            .into_iter()
            .filter_map(Empty::warning)
            .collect();
        warnings.extend(same_wordpress(&sites));
        Ok(ResolvedTargets { sites, warnings })
    }

    /// Resolve a site of this inventory.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for its server (not expected: a valid inventory
    /// has every site's server).
    fn resolve(&self, site: &Site) -> Result<ResolvedSite> {
        let server = self.resolve_server(&site.server)?;
        Ok(ResolvedSite::new(site, server))
    }
}

/// A selected group or server without sites, or `all_sites` on an
/// inventory without sites.
struct Empty {
    /// Where it was named (`groups[1]`, `all_sites`).
    location: String,
    /// The group or server, `None` for `all_sites`.
    named: Option<(EntityKind, String)>,
}

impl Empty {
    /// Why it selected nothing, located, for the error when nothing else
    /// matched.
    fn problem(&self) -> Problem {
        let message = match &self.named {
            Some((kind, id)) => format!("{kind} {} has no sites", quoted(id)),
            None => "the inventory has no sites".to_string(),
        };
        Problem::new(self.location.clone(), message)
    }

    /// The warning when other parts did select sites (`None` for
    /// `all_sites`, which then selected every site).
    fn warning(self) -> Option<TargetWarning> {
        self.named
            .map(|(kind, id)| TargetWarning::NoSites { kind, id })
    }
}

/// What the parts of a [`Targets`] selection name.
struct Selection<'a> {
    /// Ids of the selected sites.
    chosen: HashSet<&'a str>,
    /// Unknown ids, located.
    unknown: Vec<Problem>,
    /// Parts that selected no site, the first naming of each only.
    empty: Vec<Empty>,
}

impl<'a> Selection<'a> {
    /// Collect what each part of `targets` names in `inventory`.
    ///
    /// # Arguments
    ///
    /// * `inventory` - Where the ids are looked up
    /// * `targets` - The selection
    fn collect(inventory: &'a Inventory, targets: &Targets) -> Self {
        let mut selection = Self {
            chosen: HashSet::new(),
            unknown: Vec::new(),
            empty: Vec::new(),
        };
        selection.add_sites(inventory, &targets.sites);
        selection.add_groups(inventory, &targets.groups);
        selection.add_servers(inventory, &targets.servers);
        if targets.all_sites {
            selection.add_all(inventory);
        }
        selection
    }

    /// Select sites by id.
    ///
    /// # Arguments
    ///
    /// * `inventory` - Where the ids are looked up
    /// * `ids` - The `sites` part of the selection
    fn add_sites(&mut self, inventory: &'a Inventory, ids: &[String]) {
        for (index, id) in ids.iter().enumerate() {
            match inventory.site(id) {
                Some(site) => {
                    self.chosen.insert(site.id.as_str());
                }
                None => self.note_unknown(
                    EntityKind::Site,
                    index,
                    id,
                    inventory.sites().iter().map(|site| site.id.as_str()),
                ),
            }
        }
    }

    /// Select every site of each group.
    ///
    /// # Arguments
    ///
    /// * `inventory` - Where the ids are looked up
    /// * `ids` - The `groups` part of the selection
    fn add_groups(&mut self, inventory: &'a Inventory, ids: &[String]) {
        for (index, id) in ids.iter().enumerate() {
            let Some(group) = inventory.group(id) else {
                self.note_unknown(
                    EntityKind::Group,
                    index,
                    id,
                    inventory.groups().iter().map(|group| group.id.as_str()),
                );
                continue;
            };
            self.chosen.extend(group.sites.iter().map(String::as_str));
            if group.sites.is_empty() {
                self.note_empty(EntityKind::Group, index, &group.id);
            }
        }
    }

    /// Select every site on each server.
    ///
    /// # Arguments
    ///
    /// * `inventory` - Where the ids are looked up
    /// * `ids` - The `servers` part of the selection
    fn add_servers(&mut self, inventory: &'a Inventory, ids: &[String]) {
        for (index, id) in ids.iter().enumerate() {
            let Some(server) = inventory.server(id) else {
                self.note_unknown(
                    EntityKind::Server,
                    index,
                    id,
                    inventory.servers().iter().map(|server| server.id.as_str()),
                );
                continue;
            };
            let mut on_server = inventory.sites_on(&server.id).peekable();
            if on_server.peek().is_none() {
                self.note_empty(EntityKind::Server, index, &server.id);
            }
            self.chosen.extend(on_server.map(|site| site.id.as_str()));
        }
    }

    /// Select every site.
    ///
    /// # Arguments
    ///
    /// * `inventory` - The inventory whose sites are selected
    fn add_all(&mut self, inventory: &'a Inventory) {
        self.chosen
            .extend(inventory.sites().iter().map(|site| site.id.as_str()));
        if inventory.sites().is_empty() {
            self.empty.push(Empty {
                location: "all_sites".into(),
                named: None,
            });
        }
    }

    /// Record an unknown id.
    ///
    /// # Arguments
    ///
    /// * `kind` - What the id should name
    /// * `index` - Its position in its part
    /// * `id` - The id
    /// * `known` - The ids of that kind, for a hint
    fn note_unknown<'k>(
        &mut self,
        kind: EntityKind,
        index: usize,
        id: &str,
        known: impl IntoIterator<Item = &'k str>,
    ) {
        let location = format!("{}[{index}]", part(kind));
        self.unknown
            .push(Problem::new(location, unknown_message(kind, id, known)));
    }

    /// Record a group or server without sites, once per id.
    ///
    /// # Arguments
    ///
    /// * `kind` - [`EntityKind::Group`] or [`EntityKind::Server`]
    /// * `index` - Its position in its part
    /// * `id` - Its id
    fn note_empty(&mut self, kind: EntityKind, index: usize, id: &str) {
        let seen = self
            .empty
            .iter()
            .any(|empty| matches!(&empty.named, Some((k, i)) if *k == kind && i == id));
        if seen {
            return;
        }
        self.empty.push(Empty {
            location: format!("{}[{index}]", part(kind)),
            named: Some((kind, id.to_string())),
        });
    }
}

/// The field of [`Targets`] that names records of `kind`, for locations.
fn part(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Site => "sites",
        EntityKind::Group => "groups",
        EntityKind::Server => "servers",
    }
}

/// `unknown site 'Shop'; did you mean 'shop'?`: the hint names a known id
/// that differs only in letter case or surrounding spaces (ids are
/// lowercase and never contain spaces).
///
/// # Arguments
///
/// * `kind` - What the id should name
/// * `id` - The unknown id
/// * `known` - The ids of that kind
fn unknown_message<'k>(
    kind: EntityKind,
    id: &str,
    known: impl IntoIterator<Item = &'k str>,
) -> String {
    let wanted = id.trim().to_ascii_lowercase();
    let message = format!("unknown {kind} {}", quoted(id));
    match known.into_iter().find(|candidate| *candidate == wanted) {
        Some(candidate) => format!("{message}; did you mean {}?", quoted(candidate)),
        None => message,
    }
}

/// The problems of a selection that matched no site: a summary, then why
/// each part selected nothing.
///
/// # Arguments
///
/// * `empty` - The parts that selected no site
fn nothing_matched(empty: Vec<Empty>) -> Vec<Problem> {
    let summary = Problem::new("", "no site selected: the selection matches no site");
    std::iter::once(summary)
        .chain(empty.iter().map(Empty::problem))
        .collect()
}

/// [`Error::InvalidInput`] about a selection.
///
/// # Arguments
///
/// * `problems` - What is wrong with it
fn targets_error(problems: Vec<Problem>) -> Error {
    Error::InvalidInput {
        subject: "targets".into(),
        problems,
    }
}

/// [`Error::NotFound`] for a record of `kind` with this `id`.
fn not_found(kind: EntityKind, id: &str) -> Error {
    Error::NotFound {
        kind,
        id: id.to_string(),
    }
}

/// The SSH setting at the most specific level that sets it: the server's,
/// else the default; `None` when neither does.
///
/// # Arguments
///
/// * `defaults` - The inventory's `defaults.ssh`
/// * `server` - The server's `ssh`
/// * `field` - Reads the field from one level
fn layered<T: Clone>(
    defaults: &SshSettings,
    server: &SshSettings,
    field: impl Fn(&SshSettings) -> Option<&T>,
) -> Option<Setting<T>> {
    from_level(field(server), SettingSource::Server)
        .or_else(|| from_level(field(defaults), SettingSource::Defaults))
}

/// `value`, when set, as a setting from `source`.
///
/// # Arguments
///
/// * `value` - The level's value, if set
/// * `source` - The level
fn from_level<T: Clone>(value: Option<&T>, source: SettingSource) -> Option<Setting<T>> {
    value.map(|value| Setting::new(value.clone(), source))
}

/// A built-in value.
fn builtin<T>(value: T) -> Setting<T> {
    Setting::new(value, SettingSource::BuiltIn)
}

/// The site's value when set, else the built-in one.
///
/// # Arguments
///
/// * `value` - The site's value, if set
/// * `default` - Builds the built-in value
fn site_or<T: Clone>(value: Option<&T>, default: impl FnOnce() -> T) -> Setting<T> {
    from_level(value, SettingSource::Site).unwrap_or_else(|| builtin(default()))
}

/// What makes two selected sites the same WordPress install (plan §4.2):
/// the same path on the same ssh destination. The destination is what apvm
/// passes to ssh, compared exactly: the host as written, the port and login
/// user, the jump hosts and the ssh_config file. ssh matches ssh_config
/// `Host` patterns case-sensitively on the host as given, and resolves what
/// apvm leaves unset (port, user, an alias) through that file; behind
/// another jump host the same name may be another machine. So any
/// difference may be another install: no guessing. Paths are canonical
/// (plan §7), so one spelling per directory; symlinks are only seen by the
/// probe.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct InstallKey<'a> {
    /// The host, as written.
    host: &'a str,
    /// The effective port, when apvm passes one.
    port: Option<u16>,
    /// The effective login user, when apvm passes one.
    user: Option<&'a str>,
    /// The effective jump hosts, when apvm passes them.
    proxy_jump: Option<&'a str>,
    /// The effective ssh_config file, when apvm passes one.
    ssh_config_file: Option<&'a str>,
    /// The WordPress root.
    path: &'a str,
}

impl<'a> InstallKey<'a> {
    /// The key of a resolved site.
    ///
    /// # Arguments
    ///
    /// * `site` - The site
    fn of(site: &'a ResolvedSite) -> Self {
        let ssh = &site.server.ssh;
        Self {
            host: &site.server.host,
            port: ssh.port.as_ref().map(|port| port.value),
            user: value_str(ssh.user.as_ref()),
            proxy_jump: value_str(ssh.proxy_jump.as_ref()),
            ssh_config_file: value_str(ssh.ssh_config_file.as_ref()),
            path: &site.path,
        }
    }

    /// The warning about `sites`, which share this key.
    ///
    /// # Arguments
    ///
    /// * `sites` - The ids of the sites, at least two, in inventory order
    fn warning(&self, sites: Vec<String>) -> TargetWarning {
        TargetWarning::SameWordPress {
            sites,
            host: self.host.to_string(),
            port: self.port,
            user: self.user.map(String::from),
            proxy_jump: self.proxy_jump.map(String::from),
            ssh_config_file: self.ssh_config_file.map(String::from),
            path: self.path.to_string(),
        }
    }
}

/// The value of an optional text setting.
///
/// # Arguments
///
/// * `setting` - The setting, if set
fn value_str(setting: Option<&Setting<String>>) -> Option<&str> {
    setting.map(|setting| setting.value.as_str())
}

/// One warning per WordPress install that several of `sites` name, in the
/// order of each install's first site.
///
/// # Arguments
///
/// * `sites` - The selected sites, in inventory order
fn same_wordpress(sites: &[ResolvedSite]) -> Vec<TargetWarning> {
    let mut installs: Vec<(InstallKey<'_>, Vec<String>)> = Vec::new();
    let mut slots: HashMap<InstallKey<'_>, usize> = HashMap::new();
    for site in sites {
        let key = InstallKey::of(site);
        // A new install gets the next slot, filled just below.
        let slot = *slots.entry(key).or_insert(installs.len());
        match installs.get_mut(slot) {
            Some((_, ids)) => ids.push(site.id.clone()),
            None => installs.push((key, vec![site.id.clone()])),
        }
    }
    installs
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(key, ids)| key.warning(ids))
        .collect()
}

#[cfg(test)]
mod tests;
