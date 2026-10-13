//! What a site reports about its plugins (`wp plugin list`, plan §6.4):
//! typed rows, their status, and lookups by slug or file (plan §9 rule 3:
//! rows match by `name == slug`, cross-checked with `file`).

use std::collections::BTreeMap;

use crate::allowlist;
use crate::error::{Error, Result};
use crate::problem::{Problem, quoted};

/// The longest status text kept, in bytes. WP-CLI's statuses are at most
/// 14 bytes (`active-network`); an unknown one longer than this is not a
/// status.
const STATUS_MAX_BYTES: usize = 64;

/// A plugin slug apvm may name in a command: WP-CLI's `name` of a regular
/// plugin, `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$` (plan §7). It never starts
/// with `-`, so WP-CLI can never read it as an option, and it holds no `/`,
/// so it names one entry of the plugins directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Slug(String);

impl Slug {
    /// Check and wrap a slug.
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin's WP-CLI name, such as `backwpup-pro` or
    ///   `hello` (the single-file `hello.php`)
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] naming the rule `slug` breaks.
    pub fn new(slug: &str) -> Result<Self> {
        allowlist::slug(slug).map_err(|reason| Error::InvalidInput {
            subject: "plugin slug".to_string(),
            problems: vec![Problem::new("slug", reason)],
        })?;
        Ok(Self(slug.to_string()))
    }

    /// The slug text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Slug {
    /// The slug text (it needs no escaping: the allowlist is plain ASCII).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A plugin's status as `wp plugin list` reports it (WP-CLI 2.12.0:
/// `active`, `active-network`, `inactive`, `must-use`, `dropin`, plan
/// §3.2). A value this build does not know is kept as
/// [`PluginStatus::Unknown`], never mapped to a known one: callers must
/// decide what it means (the planner blocks it, a status query reports it
/// as undetermined).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PluginStatus {
    /// Active on this site (`active`).
    Active,
    /// Network-activated on a multisite (`active-network`).
    ActiveNetwork,
    /// Installed, not active (`inactive`).
    Inactive,
    /// A must-use plugin (`must-use`): always loaded, never managed by apvm.
    MustUse,
    /// A drop-in such as `advanced-cache.php` (`dropin`): never managed by
    /// apvm.
    Dropin,
    /// A status this build does not know, as printed (raw: escape it before
    /// showing it).
    Unknown(String),
}

impl PluginStatus {
    /// Read a status as WP-CLI prints it. Never fails: an unknown value is
    /// kept as [`PluginStatus::Unknown`].
    ///
    /// # Arguments
    ///
    /// * `text` - The `status` field
    pub fn parse(text: &str) -> Self {
        match text {
            "active" => Self::Active,
            "active-network" => Self::ActiveNetwork,
            "inactive" => Self::Inactive,
            "must-use" => Self::MustUse,
            "dropin" => Self::Dropin,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// The status as WP-CLI prints it (raw for an unknown one).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::ActiveNetwork => "active-network",
            Self::Inactive => "inactive",
            Self::MustUse => "must-use",
            Self::Dropin => "dropin",
            Self::Unknown(text) => text,
        }
    }

    /// Whether the plugin runs: `active` or `active-network` (plan §6.15's
    /// `--expect active`).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active | Self::ActiveNetwork)
    }

    /// Whether it is a regular plugin, the only kind apvm changes:
    /// `active`, `active-network` or `inactive` (plan §4.3).
    pub fn is_regular(&self) -> bool {
        matches!(self, Self::Active | Self::ActiveNetwork | Self::Inactive)
    }

    /// Whether this build knows the status.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

impl std::fmt::Display for PluginStatus {
    /// The status text; an unknown one quoted and escaped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(text) => write!(f, "unknown status {}", quoted(text)),
            known => f.write_str(known.as_str()),
        }
    }
}

/// One row of `wp plugin list --fields=name,status,version,file,title`.
///
/// `name`, `file` and `title` come from the site: they may hold any text
/// (`My Plugin`, control characters), so quote and escape them before
/// showing them, and never put them into a command (use a [`Slug`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// WP-CLI's name: the plugin's folder, a single file's name without
    /// `.php`, a must-use file's name without `.php`, a drop-in's file name;
    /// for regular plugins sharing a name, the file without `.php`
    /// (`twofiles/a`).
    name: String,
    /// Its status.
    status: PluginStatus,
    /// The `Version` header, if not empty.
    version: Option<String>,
    /// The main file, relative to its directory (`akismet/akismet.php`,
    /// `hello.php`).
    file: String,
    /// The `Plugin Name` header, if not empty.
    title: Option<String>,
}

impl PluginRow {
    /// Check a row: `file` must be a `.php` file and `name` must be what
    /// WP-CLI derives from it for that status (plan §3.5 [wpcli.list-read]):
    ///
    /// - regular statuses: `<name>/<main>.php` or `<name>.php`, or, when
    ///   several regular plugins would share a name, the file with every
    ///   `.php` removed (`twofiles/a.php` → `twofiles/a`; WP-CLI 2.12.0's
    ///   `get_item_list()`, verified);
    /// - `must-use`: `<name>.php`;
    /// - `dropin`: `name == file` (`advanced-cache.php`);
    /// - an unknown status: any of these shapes.
    ///
    /// A row that breaks this was not produced by WP-CLI's own listing.
    /// Names are for display: lookups follow WP-CLI's fetcher, by file
    /// ([`PluginList::find`]).
    ///
    /// # Arguments
    ///
    /// * `name` - The `name` field
    /// * `status` - The parsed `status` field
    /// * `version` - The `version` field (`None` when empty or `null`)
    /// * `file` - The `file` field
    /// * `title` - The `title` field (`None` when empty or `null`)
    ///
    /// # Errors
    ///
    /// Why the row is refused (located by the caller).
    pub(crate) fn new(
        name: String,
        status: PluginStatus,
        version: Option<String>,
        file: String,
        title: Option<String>,
    ) -> std::result::Result<Self, String> {
        check_status(&status)?;
        check_name_and_file(&name, &status, &file)?;
        Ok(Self {
            name,
            status,
            version: version.filter(|v| !v.is_empty()),
            file,
            title: title.filter(|t| !t.is_empty()),
        })
    }

    /// WP-CLI's name (raw text from the site).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The status.
    pub fn status(&self) -> &PluginStatus {
        &self.status
    }

    /// The `Version` header, or `None` when the plugin declares none (or
    /// WP-CLI could not encode it). Compared as text, never as a number:
    /// `1.10` is not `1.1`.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// The main file (raw text from the site).
    pub fn file(&self) -> &str {
        &self.file
    }

    /// The `Plugin Name` header (raw text from the site), or `None` when
    /// empty (or WP-CLI could not encode it).
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Whether it is a single-file plugin (`hello.php`): its files are that
    /// one file, not a folder (plan §6.8).
    pub fn is_single_file(&self) -> bool {
        self.status.is_regular() && !self.file.contains('/')
    }

    /// The slug WordPress gives this plugin as a dependency (WordPress
    /// 7.1.3's `WP_Plugin_Dependencies::convert_to_slug`): `hello.php` →
    /// `hello-dolly`, a folder plugin → its folder, a single file → its
    /// name with every `.php` removed. Match `Requires Plugins` entries
    /// against this (P6.1), not against [`PluginRow::name`]. `None` when it
    /// is not a [`Slug`], or for a must-use plugin or drop-in (WordPress
    /// only resolves dependencies among regular plugins).
    pub fn dependency_slug(&self) -> Option<Slug> {
        if !self.status.is_regular() {
            return None;
        }
        let slug = if self.file == "hello.php" {
            "hello-dolly".to_string()
        } else if let Some((folder, _)) = self.file.split_once('/') {
            folder.to_string()
        } else {
            self.file.replace(".php", "")
        };
        Slug::new(&slug).ok()
    }

    /// The name WP-CLI's `get_plugin_name()` gives the file before any
    /// disambiguation: the folder, or the file name without its `.php`
    /// suffix (PHP's `basename( $file, '.php' )`).
    fn fetch_name(&self) -> &str {
        match self.file.split_once('/') {
            Some((folder, _)) => folder,
            None => self.file.strip_suffix(".php").unwrap_or(&self.file),
        }
    }

    /// Whether this row may hide a regular single-file plugin of the same
    /// file name: a must-use plugin, a drop-in or a row of an unknown
    /// status, at a top-level `<x>.php` (see [`Lookup::Shadowed`]).
    fn may_shadow(&self) -> bool {
        !self.status.is_regular() && !self.file.contains('/')
    }
}

/// Refuse an empty or oversized unknown status (also checked for rows
/// printed without a name).
///
/// # Arguments
///
/// * `status` - The parsed status
pub(crate) fn check_status(status: &PluginStatus) -> std::result::Result<(), String> {
    let PluginStatus::Unknown(text) = status else {
        return Ok(());
    };
    if text.is_empty() {
        return Err("has an empty status".into());
    }
    if text.len() > STATUS_MAX_BYTES {
        return Err(format!("has a status longer than {STATUS_MAX_BYTES} bytes"));
    }
    Ok(())
}

/// The shapes a row's `name` and `file` may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `<name>/<main>.php`: a plugin folder.
    Folder,
    /// `<name>.php`: a single file (regular or must-use).
    File,
    /// `name == file`: a drop-in.
    Dropin,
    /// `name` is the file with every `.php` removed: regular plugins whose
    /// names would repeat (`twofiles/a.php` → `twofiles/a`).
    Disambiguated,
}

/// Check that `name` is what WP-CLI derives from `file` for `status`.
///
/// # Arguments
///
/// * `name` - The `name` field
/// * `status` - The parsed status
/// * `file` - The `file` field
fn check_name_and_file(
    name: &str,
    status: &PluginStatus,
    file: &str,
) -> std::result::Result<(), String> {
    if matches!(name, "" | "." | "..") {
        return Err(format!("has an invalid name {}", quoted(name)));
    }
    if !file.ends_with(".php") {
        return Err(format!(
            "has a file {} that is not a '.php' file",
            quoted(file)
        ));
    }
    let allowed: &[Shape] = match status {
        PluginStatus::Active | PluginStatus::ActiveNetwork | PluginStatus::Inactive => {
            &[Shape::Folder, Shape::File, Shape::Disambiguated]
        }
        PluginStatus::MustUse => &[Shape::File],
        PluginStatus::Dropin => &[Shape::Dropin],
        PluginStatus::Unknown(_) => &[
            Shape::Folder,
            Shape::File,
            Shape::Dropin,
            Shape::Disambiguated,
        ],
    };
    if allowed.iter().any(|shape| fits(*shape, name, file)) {
        return Ok(());
    }
    Err(format!(
        "has a name {} that does not match its file {} for status {}",
        quoted(name),
        quoted(file),
        status
    ))
}

/// Whether `name` and `file` (already known to end in `.php`) have `shape`.
///
/// # Arguments
///
/// * `shape` - The shape to test
/// * `name` - The `name` field
/// * `file` - The `file` field
fn fits(shape: Shape, name: &str, file: &str) -> bool {
    match shape {
        Shape::Folder => file.split_once('/').is_some_and(|(folder, main)| {
            folder == name && main.len() > ".php".len() && !main.contains('/')
        }),
        Shape::File => !file.contains('/') && file.strip_suffix(".php") == Some(name),
        Shape::Dropin => !file.contains('/') && file == name,
        Shape::Disambiguated => {
            file.matches('/').count() <= 1
                && !file.starts_with('/')
                && file.replace(".php", "") == name
        }
    }
}

/// Whether WP-CLI's plugin fetcher (`Fetchers\Plugin::get`, 2.12.0)
/// takes the plugin file `file` for `slug`: the file is `<slug>.php` or
/// `<slug>`, or its folder is `<slug>`. On bytes, so a file name that is
/// not valid UTF-8 is matched too.
///
/// # Arguments
///
/// * `file` - The plugin file, relative to the plugins directory
/// * `slug` - The name asked for
fn fetcher_takes(file: &[u8], slug: &str) -> bool {
    let slug = slug.as_bytes();
    let folder = file
        .iter()
        .position(|b| *b == b'/')
        .and_then(|at| file.get(..at));
    file.strip_suffix(b".php") == Some(slug) || file == slug || folder == Some(slug)
}

/// Whether WP-CLI's fetcher considers rows of `status`: regular plugins,
/// and (conservatively) rows of a status this build does not know.
///
/// # Arguments
///
/// * `status` - A row's status
fn fetchable(status: &PluginStatus) -> bool {
    !matches!(status, PluginStatus::MustUse | PluginStatus::Dropin)
}

/// A row whose name or file WP-CLI could not encode (not valid UTF-8, so
/// `json_encode` printed `null`). apvm never acts on it: no command can
/// name its file safely, and no read can confirm a change to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreadableRow {
    /// Its status.
    status: PluginStatus,
    /// The file name's bytes, when the read reports them
    /// ([`REGULAR_PLUGINS`](super::snippets::REGULAR_PLUGINS)'s
    /// `file_hex`); `None` from `plugin list`, which prints only `null`.
    file: Option<Vec<u8>>,
}

impl UnreadableRow {
    /// A row with `status` and, if known, its file's bytes.
    ///
    /// # Arguments
    ///
    /// * `status` - Its status
    /// * `file` - Its file's bytes, if known
    pub(crate) fn new(status: PluginStatus, file: Option<Vec<u8>>) -> Self {
        Self { status, file }
    }

    /// Whether WP-CLI's fetcher may take this row for `slug`: a fetchable
    /// status, and a file that matches, or an unknown one (which may).
    ///
    /// # Arguments
    ///
    /// * `slug` - The name asked for
    fn may_be_fetched(&self, slug: &str) -> bool {
        fetchable(&self.status)
            && self
                .file
                .as_deref()
                .is_none_or(|file| fetcher_takes(file, slug))
    }
}

/// The result of looking a plugin up in a [`PluginList`]. Only
/// [`Lookup::Found`] lets a caller act ([`PluginList::resolve`]); every
/// other answer but [`Lookup::Absent`] means "undetermined": a planner
/// blocks the plugin, a status query reports it as undetermined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// No row matches: WP-CLI's fetcher finds no plugin of that slug (the
    /// list itself was read and parsed in full, so this is a fact, not a
    /// guess).
    Absent,
    /// Exactly one row matches.
    Found(&'a PluginRow),
    /// Several rows match, such as a folder `twofiles/` with two headered
    /// files (`twofiles/a.php`, `twofiles/b.php`: WP-CLI's fetcher takes
    /// the first for `twofiles`), `foo.php/main.php` next to `foo.php.php`
    /// for `foo.php`, or a row next to a must-use plugin or drop-in that may
    /// hide another match (see [`Lookup::Shadowed`]). WP-CLI would act on
    /// one of them, so a caller must not act on any.
    Ambiguous(Vec<&'a PluginRow>),
    /// No other row matches, but a must-use plugin, a drop-in or a row of
    /// an unknown status has the file `<slug>.php` or `<slug>`. WP-CLI 2.12
    /// lists every kind in one array keyed by file (`get_all_items()`), so
    /// such a row replaces a regular plugin of that file name, which then
    /// does not appear at all, while WP-CLI's fetcher (regular plugins
    /// only) still acts on it (verified: an active regular
    /// `advanced-cache.php` behind the drop-in, and Hello Dolly's
    /// `hello.php` behind a must-use `hello.php`, where `wp plugin path
    /// hello.php` and `hello` both returned the hidden file). Resolve it
    /// with a second read, [`WpCommand::regular_plugins`](super::WpCommand::regular_plugins)
    /// (what `get_plugins()` itself returns, so nothing can hide there), and
    /// [`PluginList::find_regular`] on that list. Only when that read fails
    /// is the answer undetermined; never "absent" (plan §9).
    Shadowed {
        /// The row that may hide it.
        by: &'a PluginRow,
    },
    /// Rows whose file name is not valid UTF-8 may be what WP-CLI's fetcher
    /// takes for this slug: `plugin list` prints them as `null` (their
    /// folder unknown, so they may match any slug), and
    /// [`WpCommand::regular_plugins`](super::WpCommand::regular_plugins)
    /// reports their bytes (so [`PluginList::find_regular`] says which
    /// slugs they match). Verified: with `akismet/a\xe9.php` next to
    /// Akismet, `wp plugin activate akismet` may act on either. apvm cannot
    /// act on such a file or confirm a change to it: undetermined, with the
    /// files reported ([`PluginList::unreadable_files`]).
    Unreadable {
        /// How many such rows may match.
        count: usize,
    },
}

impl<'a> Lookup<'a> {
    /// The row, when exactly one matches.
    pub fn found(&self) -> Option<&'a PluginRow> {
        match self {
            Self::Found(row) => Some(row),
            _ => None,
        }
    }

    /// Whether no row matches.
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// The lookup for `rows`.
    ///
    /// # Arguments
    ///
    /// * `rows` - Every matching row
    fn of(mut rows: Vec<&'a PluginRow>) -> Self {
        match rows.len() {
            0 => Self::Absent,
            1 => rows.pop().map_or(Self::Absent, Self::Found),
            _ => Self::Ambiguous(rows),
        }
    }
}

/// A regular plugin that a slug resolves to uniquely ([`Lookup::Found`]):
/// the only way to build a lifecycle command
/// ([`WpCommand`](super::WpCommand)) or to trust a
/// [`requires_plugins`](super::parse::requires_plugins) read, so no command
/// can name a slug that WP-CLI might take for another, possibly hidden,
/// plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<'a> {
    /// The slug the commands name.
    slug: Slug,
    /// The row it resolves to.
    row: &'a PluginRow,
    /// WP-CLI's `get_plugin_name()` of the row's file.
    wp_cli_name: Slug,
}

impl<'a> Resolved<'a> {
    /// The slug commands name (WP-CLI's fetcher takes exactly
    /// [`Resolved::row`] for it).
    pub fn slug(&self) -> &Slug {
        &self.slug
    }

    /// The row.
    pub fn row(&self) -> &'a PluginRow {
        self.row
    }

    /// WP-CLI's `get_plugin_name()` of the file: the folder, or the file
    /// without `.php` (`hello` for `hello.php`, even when the slug is
    /// `hello.php`). It is what `--skip-plugins=<name>` and `plugin get`'s
    /// `name` use (WP-CLI 2.12.0's `is_plugin_skipped()`; verified:
    /// `--skip-plugins=hello.php` loads Hello Dolly, `--skip-plugins=hello`
    /// skips it).
    pub fn wp_cli_name(&self) -> &Slug {
        &self.wp_cli_name
    }
}

/// Every plugin a site reports, in WP-CLI's order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginList {
    /// The rows with a name and a file.
    rows: Vec<PluginRow>,
    /// Row indexes by name.
    by_name: BTreeMap<String, Vec<usize>>,
    /// Row indexes by file (files are unique).
    by_file: BTreeMap<String, usize>,
    /// Row indexes by folder, for files in one.
    by_dir: BTreeMap<String, Vec<usize>>,
    /// Rows whose name or file is not valid UTF-8.
    unreadable: Vec<UnreadableRow>,
}

impl PluginList {
    /// A list of `rows`, plus the `unreadable` rows WP-CLI printed without
    /// a name or file (a folder or file name that is not valid UTF-8, which
    /// `json_encode` turns into `null`; verified with WP-CLI 2.12.0).
    ///
    /// # Arguments
    ///
    /// * `rows` - The checked rows, in order
    /// * `unreadable` - The rows without a name or file
    ///
    /// # Errors
    ///
    /// Why the list is refused: two rows with one file, whatever their
    /// statuses. WP-CLI 2.12 keys every row by its file (`get_all_items()`
    /// merges regular, must-use and drop-in plugins into one array), so it
    /// never prints that.
    pub(crate) fn new(
        rows: Vec<PluginRow>,
        unreadable: Vec<UnreadableRow>,
    ) -> std::result::Result<Self, String> {
        let mut by_file = BTreeMap::new();
        let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_dir: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, row) in rows.iter().enumerate() {
            if by_file.insert(row.file.clone(), index).is_some() {
                return Err(format!("lists the file {} twice", quoted(&row.file)));
            }
            by_name.entry(row.name.clone()).or_default().push(index);
            if let Some((folder, _)) = row.file.split_once('/') {
                by_dir.entry(folder.to_string()).or_default().push(index);
            }
        }
        Ok(Self {
            rows,
            by_name,
            by_file,
            by_dir,
            unreadable,
        })
    }

    /// Every named row, in WP-CLI's order.
    pub fn rows(&self) -> &[PluginRow] {
        &self.rows
    }

    /// How many rows WP-CLI printed with a `null` name or file. Front ends
    /// should mention them: such a plugin exists but cannot be managed.
    pub fn unnamed(&self) -> usize {
        self.unreadable.len()
    }

    /// The file names of the unreadable rows, where the read reported them
    /// ([`WpCommand::regular_plugins`](super::WpCommand::regular_plugins)),
    /// decoded with replacement characters and escaped for display.
    pub fn unreadable_files(&self) -> Vec<String> {
        self.unreadable
            .iter()
            .filter_map(|row| row.file.as_deref())
            .map(|file| quoted(&String::from_utf8_lossy(file)))
            .collect()
    }

    /// The plugin `slug` names, as WP-CLI resolves it:
    ///
    /// - [`Lookup::Unreadable`] when a row whose file is not valid UTF-8
    ///   may match (from `plugin list`, any such regular row);
    /// - regular rows (and rows of an unknown status) that WP-CLI's plugin
    ///   fetcher takes for `slug`: file `<slug>.php` or `<slug>`, or folder
    ///   `<slug>` (`Fetchers\Plugin::get`, 2.12.0; it acts on the first, so
    ///   several are [`Lookup::Ambiguous`]). Names are not used here: WP-CLI
    ///   renames plugins that share a name (`twofiles/a`), and the fetcher
    ///   ignores names;
    /// - must-use plugins and drop-ins named `slug` (the caller blocks
    ///   them);
    /// - a must-use, drop-in or unknown-status row at `<slug>.php` or
    ///   `<slug>` may hide a regular file the fetcher would take: alone it
    ///   is [`Lookup::Shadowed`], with any other match
    ///   [`Lookup::Ambiguous`] (all of them).
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin's slug
    pub fn find(&self, slug: &Slug) -> Lookup<'_> {
        let count = self.unreadable_matches(slug, fetchable);
        if count > 0 {
            return Lookup::Unreadable { count };
        }
        let mut rows = self.fetched(slug, fetchable);
        let named = self.by_name.get(slug.as_str()).into_iter().flatten();
        let other_kinds = named
            .filter_map(|index| self.rows.get(*index))
            .filter(|row| !fetchable(&row.status));
        rows.extend(other_kinds);
        let shadows: Vec<&PluginRow> = [format!("{slug}.php"), slug.to_string()]
            .iter()
            .filter_map(|file| self.row_at(file))
            .filter(|row| row.may_shadow())
            .collect();
        rows.retain(|row| !shadows.iter().any(|shadow| std::ptr::eq(*row, *shadow)));
        match (rows.is_empty(), shadows.as_slice()) {
            (_, []) => Lookup::of(rows),
            (true, [by]) => Lookup::Shadowed { by },
            _ => {
                rows.extend(shadows);
                Lookup::Ambiguous(rows)
            }
        }
    }

    /// The row whose main file is `file` (`akismet/akismet.php`). A
    /// must-use plugin, drop-in or unknown-status row at a top-level file
    /// may hide a regular plugin of that file, so it is
    /// [`Lookup::Shadowed`], never [`Lookup::Found`]. (An unreadable row's
    /// file is never valid UTF-8, so it cannot be `file`.)
    ///
    /// # Arguments
    ///
    /// * `file` - The main file, as WP-CLI prints it
    pub fn find_file(&self, file: &str) -> Lookup<'_> {
        match self.row_at(file) {
            None => Lookup::Absent,
            Some(row) if row.may_shadow() => Lookup::Shadowed { by: row },
            Some(row) => Lookup::Found(row),
        }
    }

    /// The regular plugin `slug` names, by WP-CLI's fetcher rules only:
    /// [`Lookup::Absent`], [`Lookup::Found`], [`Lookup::Ambiguous`] or
    /// [`Lookup::Unreadable`], never [`Lookup::Shadowed`]. Use it on the
    /// list [`parse::regular_plugins`](super::parse::regular_plugins) read
    /// (which knows the bytes of unreadable file names), to resolve a
    /// lookup that was [`Lookup::Shadowed`] or [`Lookup::Unreadable`] in
    /// `plugin list`.
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin's slug
    pub fn find_regular(&self, slug: &Slug) -> Lookup<'_> {
        let count = self.unreadable_matches(slug, PluginStatus::is_regular);
        if count > 0 {
            return Lookup::Unreadable { count };
        }
        Lookup::of(self.fetched(slug, PluginStatus::is_regular))
    }

    /// `slug` resolved for a command: `Some` only when
    /// [`PluginList::find`] is [`Lookup::Found`] for a regular row whose
    /// WP-CLI name is a [`Slug`]. On a list from
    /// [`parse::regular_plugins`](super::parse::regular_plugins) (only
    /// regular rows, so nothing can shadow) it answers as
    /// [`PluginList::find_regular`].
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin's slug
    pub fn resolve(&self, slug: &Slug) -> Option<Resolved<'_>> {
        let row = self.find(slug).found()?;
        if !row.status.is_regular() {
            return None;
        }
        let wp_cli_name = Slug::new(row.fetch_name()).ok()?;
        Some(Resolved {
            slug: slug.clone(),
            row,
            wp_cli_name,
        })
    }

    /// The slug apvm can name `row` by: WP-CLI's `get_plugin_name()` of its
    /// file, when [`PluginList::resolve`] resolves it to exactly this row.
    /// `None` for a must-use plugin or drop-in, a name like `My Plugin`, a
    /// plugin WP-CLI's fetcher cannot single out (`twofiles/b.php`, which
    /// `twofiles` does not reach), and any plugin whose slug another row
    /// may shadow or an unreadable row may match. `--all` (plan D11) must
    /// report the rows without a slug, not skip them silently.
    ///
    /// # Arguments
    ///
    /// * `row` - A row of this list
    pub fn slug_of(&self, row: &PluginRow) -> Option<Slug> {
        let slug = Slug::new(row.fetch_name()).ok()?;
        let target = self.resolve(&slug)?;
        std::ptr::eq(target.row, row).then_some(slug)
    }

    /// The regular plugins (`active`, `active-network`, `inactive`), those
    /// apvm cannot name included. This is what `--all` expands to (plan
    /// D11, §4.3), but `--all` must not act on every one silently: rows
    /// whose [`PluginList::slug_of`] is `None`, rows of an unknown status
    /// (not listed here) and [`PluginList::unnamed`] rows cannot be
    /// managed, and the planner (P6.1) reports them instead of skipping
    /// them without a word.
    pub fn regular(&self) -> impl Iterator<Item = &PluginRow> {
        self.rows.iter().filter(|row| row.status.is_regular())
    }

    /// The row whose file is `file`.
    ///
    /// # Arguments
    ///
    /// * `file` - The main file
    fn row_at(&self, file: &str) -> Option<&PluginRow> {
        self.by_file
            .get(file)
            .and_then(|index| self.rows.get(*index))
    }

    /// The rows WP-CLI's fetcher takes for `slug` whose status passes
    /// `keep`, in list order.
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin's slug
    /// * `keep` - Which statuses count
    fn fetched(&self, slug: &Slug, keep: impl Fn(&PluginStatus) -> bool) -> Vec<&PluginRow> {
        let slug = slug.as_str();
        let as_file = [format!("{slug}.php"), slug.to_string()];
        let mut indexes: Vec<usize> = as_file
            .iter()
            .filter_map(|file| self.by_file.get(file).copied())
            .chain(self.by_dir.get(slug).into_iter().flatten().copied())
            .collect();
        indexes.sort_unstable();
        indexes.dedup();
        indexes
            .into_iter()
            .filter_map(|index| self.rows.get(index))
            .filter(|row| keep(&row.status) && fetcher_takes(row.file.as_bytes(), slug))
            .collect()
    }

    /// How many unreadable rows whose status passes `keep` WP-CLI's fetcher
    /// may take for `slug`.
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin's slug
    /// * `keep` - Which statuses count
    fn unreadable_matches(&self, slug: &Slug, keep: impl Fn(&PluginStatus) -> bool) -> usize {
        self.unreadable
            .iter()
            .filter(|row| keep(&row.status) && row.may_be_fetched(slug.as_str()))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;

    /// A checked row with a version and a title.
    ///
    /// # Arguments
    ///
    /// * `name` - The name
    /// * `status` - The status text
    /// * `file` - The main file
    fn row(name: &str, status: &str, file: &str) -> PluginRow {
        PluginRow::new(
            name.into(),
            PluginStatus::parse(status),
            Some("1.0".into()),
            file.into(),
            Some("Title".into()),
        )
        .unwrap()
    }

    /// An unreadable must-use row: WP-CLI's fetcher never takes it.
    fn mu_unreadable() -> UnreadableRow {
        UnreadableRow::new(PluginStatus::MustUse, None)
    }

    /// `row(…)`'s verdict as a plain result.
    ///
    /// # Arguments
    ///
    /// * `name` - The name
    /// * `status` - The status text
    /// * `file` - The main file
    fn check(name: &str, status: &str, file: &str) -> std::result::Result<(), String> {
        PluginRow::new(
            name.into(),
            PluginStatus::parse(status),
            None,
            file.into(),
            None,
        )
        .map(|_| ())
    }

    /// `slug` as a [`Slug`].
    ///
    /// # Arguments
    ///
    /// * `slug` - A valid slug
    fn slug(slug: &str) -> Slug {
        Slug::new(slug).unwrap()
    }

    #[test]
    fn slugs_follow_the_allowlist() {
        for ok in [
            "akismet",
            "backwpup-pro",
            "hello",
            "a",
            "WP_Rocket.2",
            &"a".repeat(100),
        ] {
            assert_eq!(Slug::new(ok).unwrap().as_str(), ok);
            assert_eq!(Slug::new(ok).unwrap().to_string(), ok);
        }
        for bad in [
            "",
            "-all",
            "--all",
            "a/b",
            "../x",
            "a b",
            "é",
            "a\0",
            "a\n",
            ".hidden",
            "@prod",
            &"a".repeat(101),
        ] {
            let error = Slug::new(bad).unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput, "{bad:?}");
            assert!(
                error.to_string().starts_with("invalid plugin slug: slug: "),
                "{error}"
            );
        }
    }

    #[test]
    fn statuses_round_trip_and_unknown_ones_stay_unknown() {
        let known = [
            ("active", PluginStatus::Active, true, true),
            ("active-network", PluginStatus::ActiveNetwork, true, true),
            ("inactive", PluginStatus::Inactive, false, true),
            ("must-use", PluginStatus::MustUse, false, false),
            ("dropin", PluginStatus::Dropin, false, false),
        ];
        for (text, status, active, regular) in known {
            let parsed = PluginStatus::parse(text);
            assert_eq!(parsed, status);
            assert_eq!(parsed.as_str(), text);
            assert_eq!(parsed.to_string(), text);
            assert_eq!((parsed.is_active(), parsed.is_regular()), (active, regular));
            assert!(parsed.is_known());
        }
        for text in ["paused", "Active", "active ", "", "recovery\u{1b}[2J"] {
            let parsed = PluginStatus::parse(text);
            assert_eq!(parsed, PluginStatus::Unknown(text.to_string()));
            assert_eq!(parsed.as_str(), text);
            assert!(!parsed.is_known() && !parsed.is_active() && !parsed.is_regular());
        }
        assert_eq!(
            PluginStatus::parse("x'\u{1b}").to_string(),
            "unknown status 'x\\'\\u{1b}'"
        );
    }

    #[test]
    fn row_names_must_match_their_files() {
        // The shapes WP-CLI 2.12.0 prints (plan §3.5 [wpcli.list-read]).
        for (name, status, file) in [
            ("akismet", "inactive", "akismet/akismet.php"),
            ("backwpup-pro", "active", "backwpup-pro/backwpup.php"),
            ("hello", "active-network", "hello.php"),
            ("My Plugin", "inactive", "My Plugin/my.php"),
            ("apvm-e2e-mu", "must-use", "apvm-e2e-mu.php"),
            ("advanced-cache.php", "dropin", "advanced-cache.php"),
            // Regular plugins that share a name (WP-CLI 2.12.0).
            ("twofiles/a", "inactive", "twofiles/a.php"),
            ("foo/main", "inactive", "foo.php/main.php"),
            ("foo", "inactive", "foo.php.php"),
            ("x", "paused", "x/x.php"),
            ("x", "paused", "x.php"),
            ("x.php", "paused", "x.php"),
        ] {
            assert_eq!(check(name, status, file), Ok(()), "{name} {status} {file}");
        }
        for (name, status, file) in [
            ("akismet", "inactive", "other/akismet.php"),
            ("akismet", "inactive", "akismet/sub/akismet.php"),
            ("akismet", "inactive", "akismet/.php"),
            ("akismet", "inactive", "akismet/"),
            ("akismet", "inactive", "akismet/readme.txt"),
            ("hello", "inactive", "hello.PHP"),
            ("hello.php", "inactive", "hello.php"),
            ("hello", "inactive", "/hello.php"),
            ("mu", "must-use", "mu/mu.php"),
            ("mu.php", "must-use", "mu.php"),
            ("advanced-cache", "dropin", "advanced-cache.php"),
            ("", "inactive", ".php"),
            ("..", "inactive", "../x.php"),
            (".", "inactive", "./x.php"),
            ("x", "paused", "y.php"),
            ("twofiles/a", "must-use", "twofiles/a.php"),
            ("twofiles/c", "inactive", "twofiles/a.php"),
            ("twofiles/a.php", "inactive", "twofiles/a.php"),
            // A shared name is never deeper than one folder, nor absolute.
            ("a/b/c", "inactive", "a/b/c.php"),
            ("/x", "inactive", "/x.php"),
        ] {
            assert!(check(name, status, file).is_err(), "{name} {status} {file}");
        }
        let error = check("a\u{1b}", "inactive", "b.php").unwrap_err();
        assert_eq!(
            error,
            "has a name 'a\\u{1b}' that does not match its file 'b.php' for status inactive"
        );
    }

    #[test]
    fn unknown_statuses_must_be_short_and_not_empty() {
        assert_eq!(check("x", "", "x.php").unwrap_err(), "has an empty status");
        assert!(check("x", &"s".repeat(STATUS_MAX_BYTES), "x.php").is_ok());
        assert!(check("x", &"s".repeat(STATUS_MAX_BYTES + 1), "x.php").is_err());
    }

    #[test]
    fn rows_expose_their_fields_and_drop_empty_texts() {
        let hello = PluginRow::new(
            "hello".into(),
            PluginStatus::Active,
            Some(String::new()),
            "hello.php".into(),
            Some(String::new()),
        )
        .unwrap();
        assert_eq!(hello.name(), "hello");
        assert_eq!(hello.status(), &PluginStatus::Active);
        assert_eq!((hello.version(), hello.title()), (None, None));
        assert_eq!(hello.file(), "hello.php");
        assert!(hello.is_single_file());
        let folder = row("akismet", "inactive", "akismet/akismet.php");
        assert_eq!(
            (folder.version(), folder.title()),
            (Some("1.0"), Some("Title"))
        );
        assert!(!folder.is_single_file());
        // A must-use file is not a single-file regular plugin.
        assert!(!row("mu", "must-use", "mu.php").is_single_file());
    }

    #[test]
    fn lookups_by_slug_and_file() {
        let list = PluginList::new(
            vec![
                row("akismet", "inactive", "akismet/akismet.php"),
                row("hello", "active", "hello.php"),
                row("mu", "must-use", "mu.php"),
                row("advanced-cache.php", "dropin", "advanced-cache.php"),
            ],
            vec![mu_unreadable(), mu_unreadable()],
        )
        .unwrap();
        assert_eq!(list.rows().len(), 4);
        assert_eq!(list.unnamed(), 2);
        let found = list.find(&slug("hello")).found().unwrap();
        assert_eq!(found.file(), "hello.php");
        assert!(list.find(&slug("Hello")).is_absent());
        // WP-CLI's fetcher also takes a file name (`hello.php`).
        assert_eq!(
            list.find(&slug("hello.php")).found().unwrap().name(),
            "hello"
        );
        assert_eq!(
            list.find_file("akismet/akismet.php")
                .found()
                .unwrap()
                .name(),
            "akismet"
        );
        assert_eq!(list.find_file("hello.php").found().unwrap().name(), "hello");
        assert!(list.find_file("akismet").is_absent());
        // A drop-in at the slug's file may hide a regular one there.
        assert!(matches!(
            list.find(&slug("advanced-cache.php")),
            Lookup::Shadowed { by } if by.status() == &PluginStatus::Dropin
        ));
        let regular: Vec<&str> = list.regular().map(PluginRow::name).collect();
        assert_eq!(regular, ["akismet", "hello"]);
        assert_eq!(Lookup::Absent.found(), None);
    }

    #[test]
    fn must_use_and_drop_in_files_shadow_regular_single_files() {
        let list = PluginList::new(
            vec![
                row("mu", "must-use", "mu.php"),
                row("advanced-cache.php", "dropin", "advanced-cache.php"),
                row("odd", "paused", "odd.php"),
                row("folder", "paused", "folder/folder.php"),
            ],
            vec![],
        )
        .unwrap();
        // WP-CLI may have hidden a regular `<slug>.php` behind each.
        for (name, by) in [
            ("mu", "mu.php"),
            ("advanced-cache", "advanced-cache.php"),
            ("odd", "odd.php"),
        ] {
            let lookup = list.find(&slug(name));
            assert!(
                matches!(lookup, Lookup::Shadowed { by: row } if row.file() == by),
                "{name}: {lookup:?}"
            );
            assert!(!lookup.is_absent() && lookup.found().is_none());
            assert!(
                matches!(list.find_file(by), Lookup::Shadowed { .. }),
                "{by}"
            );
        }
        // A folder cannot collide with a top-level file.
        assert_eq!(
            list.find(&slug("folder")).found().unwrap().file(),
            "folder/folder.php"
        );
        assert!(matches!(
            list.find_file("folder/folder.php"),
            Lookup::Found(_)
        ));
    }

    #[test]
    fn duplicate_names_are_ambiguous_and_duplicate_files_refused() {
        // `foo/` with two headered files: WordPress lists each file, so the
        // name repeats. A must-use `hello.php` next to `hello/hello.php` is
        // ambiguous too (and may hide a regular `hello.php`).
        let list = PluginList::new(
            vec![
                row("foo", "inactive", "foo/a.php"),
                row("foo", "active", "foo/b.php"),
                row("hello", "inactive", "hello/hello.php"),
                row("hello", "must-use", "hello.php"),
            ],
            vec![],
        )
        .unwrap();
        let Lookup::Ambiguous(rows) = list.find(&slug("foo")) else {
            panic!("two rows are named foo");
        };
        assert_eq!(rows.len(), 2);
        let Lookup::Ambiguous(rows) = list.find(&slug("hello")) else {
            panic!("a folder and a must-use file are named hello");
        };
        let files: Vec<&str> = rows.iter().map(|row| row.file()).collect();
        assert_eq!(files, ["hello/hello.php", "hello.php"]);
        assert!(list.find(&slug("foo")).found().is_none());
        assert!(!list.find(&slug("foo")).is_absent());

        // WP-CLI keys every row by file, whatever the status.
        for (first, second) in [
            (("hello", "inactive"), ("hello", "active")),
            (("hello", "inactive"), ("hello", "must-use")),
            (("hello", "paused"), ("hello", "inactive")),
            (("hello", "paused"), ("hello", "paused")),
        ] {
            let error = PluginList::new(
                vec![
                    row(first.0, first.1, "hello.php"),
                    row(second.0, second.1, "hello.php"),
                ],
                vec![],
            )
            .unwrap_err();
            assert_eq!(
                error, "lists the file 'hello.php' twice",
                "{first:?} {second:?}"
            );
        }
    }

    #[test]
    fn lookups_follow_wp_clis_fetcher_not_names() {
        // WP-CLI renames regular plugins that share a name (verified:
        // `twofiles/a`, `foo/main` for `foo.php/main.php`, `foo` for
        // `foo.php.php`); its fetcher takes the first file that is
        // `<slug>.php` or `<slug>`, or lies in folder `<slug>`.
        let list = PluginList::new(
            vec![
                row("foo/main", "inactive", "foo.php/main.php"),
                row("foo", "inactive", "foo.php.php"),
                row("twofiles/a", "inactive", "twofiles/a.php"),
                row("twofiles/b", "active", "twofiles/b.php"),
                row("solo.php", "inactive", "solo.php/main.php"),
            ],
            vec![],
        )
        .unwrap();
        let files = |lookup: Lookup<'_>| -> Vec<String> {
            match lookup {
                Lookup::Absent => vec![],
                Lookup::Found(row) => vec![row.file().to_string()],
                Lookup::Ambiguous(rows) => rows.iter().map(|r| format!("?{}", r.file())).collect(),
                Lookup::Shadowed { by } => vec![format!("shadowed by {}", by.file())],
                Lookup::Unreadable { count } => vec![format!("{count} unreadable")],
            }
        };
        let cases: [(&str, &[&str]); 7] = [
            // `foo.php`: the folder (dirname) and the file `foo.php.php`
            // (`<slug>.php`) both match: WP-CLI picks the folder.
            ("foo.php", &["?foo.php/main.php", "?foo.php.php"]),
            // The row named `foo` is out of the fetcher's reach.
            ("foo", &[]),
            ("foo.php.php", &["foo.php.php"]),
            ("twofiles", &["?twofiles/a.php", "?twofiles/b.php"]),
            ("solo.php", &["solo.php/main.php"]),
            ("solo", &[]),
            ("main", &[]),
        ];
        for (name, expected) in cases {
            assert_eq!(files(list.find(&slug(name))), expected, "{name}");
            assert_eq!(files(list.find_regular(&slug(name))), expected, "{name}");
        }
    }

    #[test]
    fn regular_lookups_ignore_other_statuses() {
        // An unknown status is a (conservative) match for `find`, never
        // for `find_regular`, which reads `get_plugins()`'s list.
        let list = PluginList::new(
            vec![
                row("x", "paused", "x/x.php"),
                row("y", "inactive", "y/y.php"),
            ],
            vec![UnreadableRow::new(
                PluginStatus::Unknown("paused".into()),
                Some(b"z/\xff.php".to_vec()),
            )],
        )
        .unwrap();
        assert_eq!(list.find(&slug("x")).found().unwrap().file(), "x/x.php");
        assert!(list.find_regular(&slug("x")).is_absent());
        assert_eq!(list.find(&slug("z")), Lookup::Unreadable { count: 1 });
        assert!(list.find_regular(&slug("z")).is_absent());
        assert_eq!(
            list.find_regular(&slug("y")).found().unwrap().file(),
            "y/y.php"
        );
        // `resolve` never hands out a row of an unknown status.
        assert!(list.resolve(&slug("x")).is_none());
    }

    #[test]
    fn slugs_of_rows_resolve_back_to_them() {
        let list = PluginList::new(
            vec![
                row("akismet", "inactive", "akismet/akismet.php"),
                row("hello", "active", "hello.php"),
                row("My Plugin", "inactive", "My Plugin/x.php"),
                row("twofiles/a", "inactive", "twofiles/a.php"),
                row("twofiles/b", "inactive", "twofiles/b.php"),
                row("mu", "must-use", "mu.php"),
                row("x", "paused", "x/x.php"),
            ],
            vec![],
        )
        .unwrap();
        let slugs: Vec<Option<String>> = list
            .rows()
            .iter()
            .map(|row| list.slug_of(row).map(|slug| slug.to_string()))
            .collect();
        assert_eq!(
            slugs,
            [
                Some("akismet".into()),
                Some("hello".into()),
                None,
                None,
                None,
                None,
                None
            ]
        );
        // A row of another list never resolves.
        let other = row("akismet", "inactive", "akismet/akismet.php");
        assert_eq!(list.slug_of(&other), None);
    }

    #[test]
    fn dependency_slugs_follow_wordpress() {
        for (status, file, expected) in [
            ("inactive", "hello.php", Some("hello-dolly")),
            ("active", "akismet/akismet.php", Some("akismet")),
            ("inactive", "solo.php", Some("solo")),
            ("inactive", "foo.php.php", Some("foo")),
            ("inactive", "foo.php/main.php", Some("foo.php")),
            ("inactive", "My Plugin/x.php", None),
            ("must-use", "mu.php", None),
            ("dropin", "advanced-cache.php", None),
        ] {
            let name = match file.split_once('/') {
                Some((folder, _)) => folder.to_string(),
                None if status == "dropin" => file.to_string(),
                None => file.strip_suffix(".php").unwrap_or(file).to_string(),
            };
            let got = row(&name, status, file).dependency_slug();
            assert_eq!(got.as_ref().map(Slug::as_str), expected, "{file}");
        }
    }

    #[test]
    fn very_large_lists_are_indexed() {
        let rows: Vec<PluginRow> = (0..20_000)
            .map(|i| row(&format!("p{i}"), "inactive", &format!("p{i}/p{i}.php")))
            .collect();
        let list = PluginList::new(rows, vec![]).unwrap();
        assert_eq!(list.rows().len(), 20_000);
        for i in [0, 9_999, 19_999] {
            let found = list.find(&slug(&format!("p{i}"))).found().unwrap();
            assert_eq!(found.file(), format!("p{i}/p{i}.php"));
        }
        assert!(list.find(&slug("p20000")).is_absent());
        assert_eq!(PluginList::default().rows().len(), 0);
    }
}
