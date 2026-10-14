//! The WP-CLI commands apvm runs (plan §6.2–§6.7, §6.14), and the
//! per-site hermetic base they run on (§6.1 item 5).
//!
//! A [`WpCommand`] is built from typed values only ([`Resolved`] plugins,
//! [`StagedZip`], small enums), so its arguments are fixed by this module:
//! a caller can choose *which* command, never add a flag. In particular
//! the lifecycle verbs never use WP-CLI's `--all`, `--exclude`,
//! `--deactivate`, `--uninstall`, `--activate` or `--activate-network`
//! (plan §9 rule 6, D9, D11): one step is one process, verified by a read.
//! WP-CLI's global parameters are P3.1's: [`WpSite`] goes through
//! [`WpCli`], which sets `--skip-packages`, the umask `--exec` and
//! `--path` itself and refuses every other global but plan §6.12's
//! overrides.

use super::snippets::{REGULAR_PLUGINS, SITE_FACTS};
use super::state::{PluginStatus, Resolved};
use crate::allowlist;
use crate::error::{Error, Result};
use crate::problem::Problem;
use crate::remote::{FileMode, WpCli};

/// `wp plugin list`'s fields for every read (plan §6.4): exactly what the
/// parser accepts.
const LIST_FIELDS_ARG: &str = "--fields=name,status,version,file,title";
/// What a staging directory's name starts with (plan §6.6: `mktemp -d
/// <tmp>/apvm.XXXXXXXXXX`).
const STAGING_PREFIX: &str = "apvm.";
/// The random characters `mktemp` puts after [`STAGING_PREFIX`].
const STAGING_RANDOM_CHARS: usize = 10;

/// Where an activation applies (plan §6.3: `--network` is derived from the
/// observed status, never asked for blindly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivationScope {
    /// This site (no flag).
    Site,
    /// The whole network of a multisite (`--network`).
    Network,
}

impl ActivationScope {
    /// The scope a plugin is active in, for deactivating it: `active` →
    /// [`ActivationScope::Site`], `active-network` →
    /// [`ActivationScope::Network`] (WP-CLI refuses to deactivate a
    /// network-active plugin without `--network`, plan §3.5). `None` for
    /// any other status: there is nothing to deactivate.
    ///
    /// # Arguments
    ///
    /// * `status` - The observed status
    pub fn of(status: &PluginStatus) -> Option<Self> {
        match status {
            PluginStatus::Active => Some(Self::Site),
            PluginStatus::ActiveNetwork => Some(Self::Network),
            _ => None,
        }
    }
}

/// How a plugin is deactivated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeactivateMode {
    /// WordPress loads the plugin and runs its deactivation hook.
    Load,
    /// `--skip-plugins=<WP-CLI name>`: WordPress does not load it, so a
    /// plugin that breaks every request can still be deactivated, and its
    /// deactivation hook does not run (plan §6.14, `apvm wp deactivate
    /// --no-load`). WP-CLI skips every file with that name, so other regular
    /// plugins sharing it (`a.php` and `a/b.php` are both named `a`) are not
    /// loaded either: a WP-CLI limit callers should warn about.
    NoLoad,
}

/// How a staged zip is installed (plan §6.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallMode {
    /// A plain install: WordPress refuses an existing folder
    /// (`folder_exists`).
    Fresh,
    /// `--force`: the files are replaced, the data and the active state kept
    /// (`--if-installed replace`).
    Replace,
}

/// What a [`WpCommand`] does, for events, logs and retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CommandKind {
    /// `plugin list` (read).
    PluginList,
    /// `cli version` (read).
    CliVersion,
    /// `eval` of [`SITE_FACTS`] (read).
    SiteFacts,
    /// `plugin get --fields=name,requires_plugins` (read).
    RequiresPlugins,
    /// `eval` of [`REGULAR_PLUGINS`] (read).
    RegularPlugins,
    /// `option get siteurl` with every plugin loaded: the boot check. Not a
    /// read in the retry and lock sense ([`CommandKind::is_read`]).
    BootCheck,
    /// `plugin activate`.
    Activate,
    /// `plugin deactivate`.
    Deactivate,
    /// `plugin uninstall` (data and files).
    Uninstall,
    /// `plugin uninstall --skip-delete`: the uninstall routine runs, the
    /// plugin's files stay, but WP-CLI deletes its translation files
    /// ([`CommandKind::changes_files`]).
    Reset,
    /// `plugin delete` (files only).
    Delete,
    /// `plugin install <staged zip>`.
    Install,
}

impl CommandKind {
    /// The WP-CLI command, such as `plugin list`, for messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::PluginList => "plugin list",
            Self::CliVersion => "cli version",
            Self::SiteFacts | Self::RegularPlugins => "eval",
            Self::RequiresPlugins => "plugin get",
            Self::BootCheck => "option get",
            Self::Activate => "plugin activate",
            Self::Deactivate => "plugin deactivate",
            Self::Uninstall | Self::Reset => "plugin uninstall",
            Self::Delete => "plugin delete",
            Self::Install => "plugin install",
        }
    }

    /// Whether the command is a read in plan §6.1 item 8's and §6.13's
    /// sense: it loads no regular plugin or theme (`--skip-plugins
    /// --skip-themes`, or no WordPress at all), so it may be retried after
    /// a transport failure and runs without the site lock. The boot check
    /// is **not** one: it loads every plugin, and a plugin may write on any
    /// request (BackWPup's install routine runs on `init`, §3.1, §6.9).
    pub fn is_read(self) -> bool {
        matches!(
            self,
            Self::PluginList
                | Self::CliVersion
                | Self::SiteFacts
                | Self::RequiresPlugins
                | Self::RegularPlugins
        )
    }

    /// Whether WordPress loads the site's regular plugins for it: the boot
    /// check and every mutation (a `--no-load` deactivation skips its own
    /// plugin, and any other sharing its WP-CLI name). Such a command runs under the site lock, inside a
    /// mutating operation, and is never retried (plan §6.1 item 8, §6.13).
    pub fn loads_plugins(self) -> bool {
        !self.is_read()
    }

    /// Whether the command changes the site (plugin state, data or files):
    /// never retried; verified by a read (plan §6.4).
    pub fn is_mutation(self) -> bool {
        !self.is_read() && self != Self::BootCheck
    }

    /// Whether the command names a plugin resolved in a read
    /// ([`Resolved`]): a [`Session`](crate::Session) runs these only for a
    /// target bound to it ([`Session::run_on`](crate::Session::run_on)).
    pub fn targets_plugin(self) -> bool {
        matches!(
            self,
            Self::RequiresPlugins
                | Self::Activate
                | Self::Deactivate
                | Self::Uninstall
                | Self::Reset
                | Self::Delete
        )
    }

    /// Whether the command creates or deletes files: `install`,
    /// `uninstall`, `delete`, and `reset`, which keeps the plugin's files
    /// but deletes its translations (`languages/plugins/<slug>-*.po`, `.mo`,
    /// `.l10n.php`, `*.json`; WP-CLI 2.12.0's `uninstall` removes them
    /// before its `--skip-delete` branch, verified). A site with
    /// `DISALLOW_FILE_MODS` blocks these (plan §6.7).
    pub fn changes_files(self) -> bool {
        matches!(
            self,
            Self::Install | Self::Uninstall | Self::Reset | Self::Delete
        )
    }
}

/// One WP-CLI command, its arguments fixed by its constructor. Turn it into
/// a runnable command with [`WpSite::command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WpCommand {
    /// What it does.
    kind: CommandKind,
    /// WP-CLI's arguments after apvm's own (`plugin`, `list`, …).
    args: Vec<String>,
}

impl WpCommand {
    /// A command of `kind` with `args`.
    ///
    /// # Arguments
    ///
    /// * `kind` - What it does
    /// * `args` - Its arguments
    fn new(kind: CommandKind, args: Vec<String>) -> Self {
        Self { kind, args }
    }

    /// The read every verification uses (plan §6.4):
    /// `plugin list --format=json --fields=name,status,version,file,title
    /// --skip-update-check --skip-plugins --skip-themes`. Regular plugins
    /// and themes are not loaded, so one that breaks the site does not
    /// break the read (must-use plugins and drop-ins still load, §6.14).
    /// Parse it with [`parse::plugin_list`](super::parse::plugin_list).
    pub fn plugin_list() -> Self {
        let args = [
            "plugin",
            "list",
            "--format=json",
            LIST_FIELDS_ARG,
            "--skip-update-check",
            "--skip-plugins",
            "--skip-themes",
        ];
        Self::new(CommandKind::PluginList, owned(&args))
    }

    /// `cli version`: probe step I's exact-shape check (plan §6.2), which
    /// does not load WordPress. Parse it with
    /// [`parse::cli_version`](super::parse::cli_version).
    pub fn cli_version() -> Self {
        Self::new(CommandKind::CliVersion, owned(&["cli", "version"]))
    }

    /// `eval '<SITE_FACTS>' --skip-plugins --skip-themes`: probe step W
    /// (plan §6.2), the constant [`SITE_FACTS`] snippet. Parse it with
    /// [`parse::site_facts`](super::parse::site_facts).
    pub fn site_facts() -> Self {
        let args = ["eval", SITE_FACTS, "--skip-plugins", "--skip-themes"];
        Self::new(CommandKind::SiteFacts, owned(&args))
    }

    /// `eval '<REGULAR_PLUGINS>' --skip-plugins --skip-themes`: every
    /// regular plugin `get_plugins()` knows, in `plugin list`'s row shape,
    /// with none hidden by a must-use plugin or drop-in. Run it when a
    /// lookup is [`Lookup::Shadowed`](super::Lookup::Shadowed) and resolve
    /// with [`PluginList::find_regular`](super::PluginList::find_regular).
    /// Parse it with [`parse::regular_plugins`](super::parse::regular_plugins).
    pub fn regular_plugins() -> Self {
        let args = ["eval", REGULAR_PLUGINS, "--skip-plugins", "--skip-themes"];
        Self::new(CommandKind::RegularPlugins, owned(&args))
    }

    /// `plugin get <slug> --fields=name,requires_plugins --format=json
    /// --skip-plugins --skip-themes`: the plugin's raw `Requires Plugins`
    /// header, for dependency order (plan §6.3; `plugin list` has no such
    /// field in WP-CLI 2.12.0). Parse it with
    /// [`parse::requires_plugins`](super::parse::requires_plugins).
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin, resolved uniquely in the site's last read
    pub fn requires_plugins(target: &Resolved<'_>) -> Self {
        let mut args = plugin_args("get", target);
        args.extend(owned(&[
            "--fields=name,requires_plugins",
            "--format=json",
            "--skip-plugins",
            "--skip-themes",
        ]));
        Self::new(CommandKind::RequiresPlugins, args)
    }

    /// `option get siteurl` with every plugin and the theme loaded: the
    /// boot check after an activation (plan D13, §6.4 `Boots`). It is not
    /// a read ([`CommandKind::is_read`]): it runs under the site lock and is
    /// never retried. Read it with
    /// [`parse::boot_check`](super::parse::boot_check).
    pub fn boot_check() -> Self {
        Self::new(CommandKind::BootCheck, owned(&["option", "get", "siteurl"]))
    }

    /// `plugin activate <slug> [--network]` (plan §6.3, §6.7, §6.9). Never
    /// several slugs: one process per step.
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin, resolved uniquely in the site's last read
    /// * `scope` - This site, or the network
    pub fn activate(target: &Resolved<'_>, scope: ActivationScope) -> Self {
        let mut args = plugin_args("activate", target);
        push_scope(&mut args, scope);
        Self::new(CommandKind::Activate, args)
    }

    /// `plugin deactivate <slug> [--network] [--skip-plugins=<name>]`: pass
    /// 1 of a removal, a variant switch, or `apvm wp deactivate` (plan
    /// §6.5, §6.9, §6.14). Never `--uninstall`.
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin, resolved uniquely in the site's last read
    /// * `scope` - Where it is active ([`ActivationScope::of`] its status)
    /// * `mode` - Whether WordPress loads it first
    pub fn deactivate(target: &Resolved<'_>, scope: ActivationScope, mode: DeactivateMode) -> Self {
        let mut args = plugin_args("deactivate", target);
        push_scope(&mut args, scope);
        if mode == DeactivateMode::NoLoad {
            // WP-CLI skips by `get_plugin_name()`, not by the fetcher's slug.
            args.push(format!("--skip-plugins={}", target.wp_cli_name()));
        }
        Self::new(CommandKind::Deactivate, args)
    }

    /// `plugin uninstall <slug>`: pass 2 of `apvm wp uninstall` (data and
    /// files), in its own process after the plugin was verified inactive
    /// (plan §6.5). Never `--deactivate`.
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin, resolved uniquely in the site's last read
    pub fn uninstall(target: &Resolved<'_>) -> Self {
        Self::new(CommandKind::Uninstall, plugin_args("uninstall", target))
    }

    /// `plugin uninstall <slug> --skip-delete`: pass 2 of `apvm wp reset`.
    /// The uninstall routine removes the plugin's data; its plugin files
    /// stay and it stays inactive (plan §3.5), **but WP-CLI deletes its
    /// translation files** ([`CommandKind::changes_files`] is true).
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin, resolved uniquely in the site's last read
    pub fn reset(target: &Resolved<'_>) -> Self {
        let mut args = plugin_args("uninstall", target);
        args.push("--skip-delete".to_string());
        Self::new(CommandKind::Reset, args)
    }

    /// `plugin delete <slug>`: pass 2 of `apvm wp delete` (files only; WP-CLI
    /// does not check that it is inactive, so pass 1 must have, plan §3.2).
    ///
    /// # Arguments
    ///
    /// * `target` - The plugin, resolved uniquely in the site's last read
    pub fn delete(target: &Resolved<'_>) -> Self {
        Self::new(CommandKind::Delete, plugin_args("delete", target))
    }

    /// `plugin install <staged zip> [--force]` (plan §6.7). Never
    /// `--activate`: activation is its own verified step.
    ///
    /// # Arguments
    ///
    /// * `zip` - The uploaded package
    /// * `mode` - Fresh, or replace the existing files
    pub fn install(zip: &StagedZip, mode: InstallMode) -> Self {
        let mut args = owned(&["plugin", "install", zip.path()]);
        if mode == InstallMode::Replace {
            args.push("--force".to_string());
        }
        Self::new(CommandKind::Install, args)
    }

    /// What the command does.
    pub fn kind(&self) -> CommandKind {
        self.kind
    }

    /// WP-CLI's arguments after apvm's own, for logs (they hold no
    /// secret).
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// `values` as owned strings.
///
/// # Arguments
///
/// * `values` - The arguments
fn owned(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// `plugin <verb> <slug>`.
///
/// # Arguments
///
/// * `verb` - The `plugin` subcommand
/// * `target` - The plugin, resolved uniquely in the site's last read
fn plugin_args(verb: &str, target: &Resolved<'_>) -> Vec<String> {
    owned(&["plugin", verb, target.slug().as_str()])
}

/// Append `--network` for [`ActivationScope::Network`].
///
/// # Arguments
///
/// * `args` - The arguments so far
/// * `scope` - The scope
fn push_scope(args: &mut Vec<String>, scope: ActivationScope) {
    if scope == ActivationScope::Network {
        args.push("--network".to_string());
    }
}

/// A plugin zip uploaded to a site's staging directory (plan §6.6):
/// `<remote_tmp_dir>/apvm.XXXXXXXXXX/package-<n>.zip`, built from those
/// parts, so the directory is always a direct child of the site's
/// `remote_tmp_dir` matching `^<remote_tmp_dir>/apvm\.[A-Za-z0-9]{10}$`:
/// the only shape §6.6 lets cleanup remove. The fixed shape keeps user file
/// names out of remote commands, and the absolute path ending in `.zip`
/// makes WP-CLI install that file, never a wordpress.org plugin of that
/// name. P8.1 creates the directory, checks that `mktemp` printed
/// `<remote_tmp_dir>/<name>`, and uploads to [`StagedZip::path`] (an
/// [`UploadScript`](crate::remote::UploadScript) value: it holds only
/// `[A-Za-z0-9._/+-]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StagedZip {
    /// `<remote_tmp_dir>/<name>`.
    dir: String,
    /// `<dir>/package-<index>.zip`.
    path: String,
    /// The package number, from 1.
    index: u32,
}

impl StagedZip {
    /// The `index`-th package of the staging directory `name` in
    /// `remote_tmp_dir`.
    ///
    /// # Arguments
    ///
    /// * `remote_tmp_dir` - The site's `remote_tmp_dir` (default `/tmp`):
    ///   absolute, canonical, `[A-Za-z0-9._/+-]` only
    /// * `name` - The directory's name `mktemp -d
    ///   <remote_tmp_dir>/apvm.XXXXXXXXXX` created: `apvm.` and 10 letters
    ///   or digits
    /// * `index` - The package number, from 1
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] naming the part that breaks its rule
    /// (`remote_tmp_dir`, `name`, `index`, or the joined `path`).
    pub fn new(remote_tmp_dir: &str, name: &str, index: u32) -> Result<Self> {
        let invalid = |location: &str, reason: String| Error::InvalidInput {
            subject: "staged package".to_string(),
            problems: vec![Problem::new(location, reason)],
        };
        allowlist::remote_tmp_dir(remote_tmp_dir)
            .map_err(|reason| invalid("remote_tmp_dir", reason))?;
        if !is_staging_name(name) {
            return Err(invalid(
                "name",
                format!("must be '{STAGING_PREFIX}' and {STAGING_RANDOM_CHARS} letters or digits"),
            ));
        }
        if index == 0 {
            return Err(invalid("index", "must be at least 1".to_string()));
        }
        let dir = format!("{remote_tmp_dir}/{name}");
        let path = format!("{dir}/package-{index}.zip");
        allowlist::remote_tmp_dir(&path).map_err(|reason| invalid("path", reason))?;
        Ok(Self { dir, path, index })
    }

    /// The zip's absolute path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The staging directory, `<remote_tmp_dir>/apvm.XXXXXXXXXX`.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// The package number, from 1.
    pub fn index(&self) -> u32 {
        self.index
    }
}

/// Whether `name` is a staging directory name: `apvm.` and 10 ASCII letters
/// or digits (`mktemp`'s `X`s).
///
/// # Arguments
///
/// * `name` - The directory's last component
fn is_staging_name(name: &str) -> bool {
    name.strip_prefix(STAGING_PREFIX).is_some_and(|random| {
        random.len() == STAGING_RANDOM_CHARS && random.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// One site's hermetic WP-CLI base (plan §6.1 item 5): its `wp_cli`, the
/// plugins directory's mode (for the umask `--exec`), its path, and
/// optionally its URL and `--allow-root`, with no command yet. It wraps a
/// [`WpCli`] that never holds arguments, so every [`WpCommand`] starts
/// right after apvm's own options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WpSite(WpCli);

impl WpSite {
    /// The base for one site ([`WpCli::new`]).
    ///
    /// # Arguments
    ///
    /// * `wp_cli` - The site's `wp_cli` tokens
    /// * `plugins_dir_mode` - The plugins directory's mode (probe step H)
    /// * `path` - The WordPress root: absolute and canonical
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] listing every value that breaks its rule.
    pub fn new(wp_cli: &[String], plugins_dir_mode: FileMode, path: &str) -> Result<Self> {
        WpCli::new(wp_cli, plugins_dir_mode, path).map(Self)
    }

    /// Pass `--url=<url>`: the site's URL, or a subsite's (plan §6.11).
    ///
    /// # Arguments
    ///
    /// * `url` - An `http(s)` URL (plan §7)
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when `url` breaks the rule.
    #[must_use = "the builder returns the changed base"]
    pub fn url(self, url: &str) -> Result<Self> {
        self.0.url(url).map(Self)
    }

    /// Pass `--allow-root`: exactly when the identity's uid is 0, which the
    /// site must allow (plan §6.2; P4.2 decides).
    #[must_use = "the builder returns the changed base"]
    pub fn allow_root(self) -> Self {
        Self(self.0.allow_root())
    }

    /// The base itself, with no arguments: for `apvm wp run`, whose
    /// arguments [`WpCli::arg`] checks one by one (plan §6.12, P9.1).
    pub fn wp_cli(&self) -> WpCli {
        self.0.clone()
    }

    /// `command` on this site, ready for
    /// [`Script::wp`](crate::remote::Script::wp).
    ///
    /// # Arguments
    ///
    /// * `command` - The command
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] if [`WpCli::arg`] refused an argument. That
    /// cannot happen for the commands of this module (a test runs every
    /// one through it); the error is returned rather than assumed away.
    pub fn command(&self, command: &WpCommand) -> Result<WpCli> {
        self.wp_cli().args(command.args())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;
    use crate::remote::{Identity, Nonce, Script};
    use crate::wp::{PluginList, PluginRow, PluginStatus, Slug};

    /// The hermetic `env` words every WP-CLI call starts with (P3.1).
    const ENV: &str = "env -u EDITOR -u PHPRC -u PHP_INI_SCAN_DIR -u WP_CLI_ALLOW_ROOT \
                       -u WP_CLI_CACHE_DIR -u WP_CLI_CUSTOM_SHELL -u WP_CLI_EARLY_REQUIRE \
                       -u WP_CLI_FORCE_USER_LOGIN -u WP_CLI_PACKAGES_DIR -u WP_CLI_PHP \
                       -u WP_CLI_PHP_ARGS -u WP_CLI_PHP_USED -u WP_CLI_REQUIRE \
                       -u WP_CLI_RUNTIME_ALIAS -u WP_CLI_SSH_BINARY -u WP_CLI_SSH_PRE_CMD \
                       -u WP_CLI_STRICT_ARGS_MODE -u WP_CONFIG_PATH \
                       WP_CLI_CONFIG_PATH=/dev/null PAGER=cat";
    /// apvm's own options for [`shop`], before a command's arguments.
    const BASE: &str = "'wp' --skip-packages --exec='umask(umask() & ~0070);' \
                        --path='/var/www/shop' --url='https://shop.example.com'";
    /// A staging directory as `mktemp -d` prints it.
    const STAGING: &str = "apvm.Ab3dE6gH9k";
    /// The WP-CLI flags the lifecycle must never use (plan §9 rule 6, D9,
    /// D11), and the global parameters only P3.1's base may set or that
    /// apvm refuses (§6.1 item 5, §6.12).
    const BANNED: [&str; 16] = [
        "--all",
        "--exclude",
        "--deactivate",
        "--uninstall",
        "--activate",
        "--activate-network",
        "--require",
        "--exec",
        "--path",
        "--url",
        "--ssh",
        "--http",
        "--allow-root",
        "--skip-packages",
        "--prompt",
        "--context",
    ];

    /// The base for `/var/www/shop` (plugins dir `2775`) with its URL.
    fn shop() -> WpSite {
        WpSite::new(
            &["wp".to_string()],
            FileMode::new(0o2775).unwrap(),
            "/var/www/shop",
        )
        .unwrap()
        .url("https://shop.example.com")
        .unwrap()
    }

    /// A list holding one folder plugin, `<name>/<name>.php`.
    ///
    /// # Arguments
    ///
    /// * `name` - The folder
    fn listed(name: &str) -> PluginList {
        let file = format!("{name}/{name}.php");
        let row = PluginRow::new(name.into(), PluginStatus::Inactive, None, file, None).unwrap();
        PluginList::new(vec![row], vec![]).unwrap()
    }

    /// `name` resolved in `list`.
    ///
    /// # Arguments
    ///
    /// * `list` - The list
    /// * `name` - The slug
    fn resolved<'a>(list: &'a PluginList, name: &str) -> Resolved<'a> {
        list.resolve(&Slug::new(name).unwrap()).unwrap()
    }

    /// The `index`-th package of [`STAGING`].
    ///
    /// # Arguments
    ///
    /// * `index` - The package number
    fn zip(index: u32) -> StagedZip {
        StagedZip::new("/tmp", STAGING, index).unwrap()
    }

    /// `command` on [`shop`], as the login user's shell text.
    ///
    /// # Arguments
    ///
    /// * `command` - The command
    fn render(command: &WpCommand) -> String {
        shop()
            .command(command)
            .unwrap()
            .statement(&Identity::Login)
            .render()
    }

    /// Every command of this module, in every variant, for `slug`.
    ///
    /// # Arguments
    ///
    /// * `slug` - The plugin
    fn every_command(slug: &Resolved<'_>) -> Vec<WpCommand> {
        let scopes = [ActivationScope::Site, ActivationScope::Network];
        let modes = [DeactivateMode::Load, DeactivateMode::NoLoad];
        let mut commands = vec![
            WpCommand::plugin_list(),
            WpCommand::cli_version(),
            WpCommand::site_facts(),
            WpCommand::boot_check(),
            WpCommand::requires_plugins(slug),
            WpCommand::regular_plugins(),
            WpCommand::uninstall(slug),
            WpCommand::reset(slug),
            WpCommand::delete(slug),
            WpCommand::install(&zip(1), InstallMode::Fresh),
            WpCommand::install(&zip(12), InstallMode::Replace),
        ];
        for scope in scopes {
            commands.push(WpCommand::activate(slug, scope));
            for mode in modes {
                commands.push(WpCommand::deactivate(slug, scope, mode));
            }
        }
        commands
    }

    #[test]
    fn argv_goldens_per_command() {
        let list = listed("akismet");
        let akismet = resolved(&list, "akismet");
        let cases = [
            (
                WpCommand::plugin_list(),
                "'plugin' 'list' '--format=json' '--fields=name,status,version,file,title' \
                 '--skip-update-check' '--skip-plugins' '--skip-themes'",
            ),
            (WpCommand::cli_version(), "'cli' 'version'"),
            (
                WpCommand::requires_plugins(&akismet),
                "'plugin' 'get' 'akismet' '--fields=name,requires_plugins' '--format=json' \
                 '--skip-plugins' '--skip-themes'",
            ),
            (WpCommand::boot_check(), "'option' 'get' 'siteurl'"),
            (
                WpCommand::activate(&akismet, ActivationScope::Site),
                "'plugin' 'activate' 'akismet'",
            ),
            (
                WpCommand::activate(&akismet, ActivationScope::Network),
                "'plugin' 'activate' 'akismet' '--network'",
            ),
            (
                WpCommand::deactivate(&akismet, ActivationScope::Site, DeactivateMode::Load),
                "'plugin' 'deactivate' 'akismet'",
            ),
            (
                WpCommand::deactivate(&akismet, ActivationScope::Network, DeactivateMode::Load),
                "'plugin' 'deactivate' 'akismet' '--network'",
            ),
            (
                WpCommand::deactivate(&akismet, ActivationScope::Site, DeactivateMode::NoLoad),
                "'plugin' 'deactivate' 'akismet' '--skip-plugins=akismet'",
            ),
            (
                WpCommand::deactivate(&akismet, ActivationScope::Network, DeactivateMode::NoLoad),
                "'plugin' 'deactivate' 'akismet' '--network' '--skip-plugins=akismet'",
            ),
            (
                WpCommand::uninstall(&akismet),
                "'plugin' 'uninstall' 'akismet'",
            ),
            (
                WpCommand::reset(&akismet),
                "'plugin' 'uninstall' 'akismet' '--skip-delete'",
            ),
            (WpCommand::delete(&akismet), "'plugin' 'delete' 'akismet'"),
            (
                WpCommand::install(&zip(1), InstallMode::Fresh),
                "'plugin' 'install' '/tmp/apvm.Ab3dE6gH9k/package-1.zip'",
            ),
            (
                WpCommand::install(&zip(2), InstallMode::Replace),
                "'plugin' 'install' '/tmp/apvm.Ab3dE6gH9k/package-2.zip' '--force'",
            ),
        ];
        for (command, args) in cases {
            assert_eq!(
                render(&command),
                format!("{ENV} {BASE} {args}"),
                "{command:?}"
            );
        }
    }

    #[test]
    fn snippet_commands_quote_the_snippet_as_one_word() {
        for (command, snippet) in [
            (WpCommand::site_facts(), SITE_FACTS),
            (WpCommand::regular_plugins(), REGULAR_PLUGINS),
        ] {
            let quoted = snippet.replace('\'', r"'\''");
            assert_eq!(
                render(&command),
                format!("{ENV} {BASE} 'eval' '{quoted}' '--skip-plugins' '--skip-themes'")
            );
        }
    }

    #[test]
    fn commands_never_carry_banned_flags() {
        for name in [
            "akismet",
            "backwpup-pro",
            "hello",
            "a",
            "all",
            "exclude",
            "A.b_c-9",
        ] {
            for command in every_command(&resolved(&listed(name), name)) {
                for arg in command.args() {
                    let flag = arg.split('=').next().unwrap_or_default();
                    assert!(!BANNED.contains(&flag), "{command:?} carries {arg}");
                    assert!(!arg.starts_with("--no-"), "{command:?} carries {arg}");
                    assert!(!arg.starts_with('@'), "{command:?} carries {arg}");
                }
                // Every argument passes P3.1's rule, so `command` never fails.
                for (at, arg) in command.args().iter().enumerate() {
                    assert_eq!(allowlist::wp_cli_arg(arg, at == 0), Ok(()), "{arg}");
                }
            }
        }
    }

    #[test]
    fn apvm_sets_each_global_option_exactly_once() {
        let base = shop().allow_root();
        for command in every_command(&resolved(&listed("akismet"), "akismet")) {
            let rendered = base
                .command(&command)
                .unwrap()
                .statement(&Identity::Login)
                .render();
            let count = |needle: &str| rendered.matches(needle).count();
            assert_eq!(count("--skip-packages"), 1, "{rendered}");
            assert_eq!(count("--exec="), 1, "{rendered}");
            assert_eq!(count("--exec='umask(umask() & ~0070);'"), 1, "{rendered}");
            assert_eq!(count("--path="), 1, "{rendered}");
            assert_eq!(count("--url="), 1, "{rendered}");
            assert_eq!(count("--allow-root"), 1, "{rendered}");
            for refused in [
                "--require",
                "--ssh",
                "--http",
                "--prompt",
                "--context",
                "--blog",
            ] {
                assert_eq!(count(refused), 0, "{rendered}");
            }
            // The command's own arguments come last, after apvm's.
            let tail: Vec<String> = command.args().iter().map(|a| format!("'{a}'")).collect();
            if !matches!(
                command.kind(),
                CommandKind::SiteFacts | CommandKind::RegularPlugins
            ) {
                assert!(rendered.ends_with(&tail.join(" ")), "{rendered}");
            }
        }
        // Without `allow_root`, no command adds it.
        for command in every_command(&resolved(&listed("akismet"), "akismet")) {
            assert!(!render(&command).contains("--allow-root"));
        }
    }

    #[test]
    fn lifecycle_commands_name_exactly_one_plugin() {
        let list = listed("akismet");
        let akismet = resolved(&list, "akismet");
        for command in every_command(&akismet) {
            let positional: Vec<&String> = command
                .args()
                .iter()
                .filter(|arg| !arg.starts_with("--"))
                .collect();
            match command.kind() {
                CommandKind::Activate
                | CommandKind::Deactivate
                | CommandKind::Uninstall
                | CommandKind::Reset
                | CommandKind::Delete
                | CommandKind::RequiresPlugins => {
                    assert_eq!(positional.len(), 3, "{command:?}");
                    assert_eq!(positional[2], "akismet");
                }
                CommandKind::Install => {
                    assert_eq!(positional.len(), 3, "{command:?}");
                    assert!(positional[2].starts_with("/tmp/apvm."));
                }
                _ => assert!(!command.args().contains(&"akismet".to_string())),
            }
        }
    }

    #[test]
    fn kinds_label_and_classify_commands() {
        let list = listed("akismet");
        let akismet = resolved(&list, "akismet");
        let expected = [
            (WpCommand::plugin_list(), "plugin list", true),
            (WpCommand::cli_version(), "cli version", true),
            (WpCommand::site_facts(), "eval", true),
            (WpCommand::requires_plugins(&akismet), "plugin get", true),
            (WpCommand::regular_plugins(), "eval", true),
            (WpCommand::boot_check(), "option get", false),
            (
                WpCommand::activate(&akismet, ActivationScope::Site),
                "plugin activate",
                false,
            ),
            (
                WpCommand::deactivate(&akismet, ActivationScope::Site, DeactivateMode::Load),
                "plugin deactivate",
                false,
            ),
            (WpCommand::uninstall(&akismet), "plugin uninstall", false),
            (WpCommand::reset(&akismet), "plugin uninstall", false),
            (WpCommand::delete(&akismet), "plugin delete", false),
            (
                WpCommand::install(&zip(1), InstallMode::Fresh),
                "plugin install",
                false,
            ),
        ];
        for (command, label, read) in expected {
            let kind = command.kind();
            assert_eq!(kind.label(), label);
            assert_eq!(kind.is_read(), read, "{label}");
            assert_eq!(kind.loads_plugins(), !read, "{label}");
            assert_eq!(
                kind.is_mutation(),
                !read && kind != CommandKind::BootCheck,
                "{label}"
            );
            assert!(command.args().join(" ").starts_with(label), "{label}");
        }
        // Reads skip every regular plugin (or load no WordPress at all).
        for command in [
            WpCommand::plugin_list(),
            WpCommand::site_facts(),
            WpCommand::regular_plugins(),
            WpCommand::requires_plugins(&akismet),
        ] {
            assert!(
                command
                    .args()
                    .ends_with(&["--skip-plugins".to_string(), "--skip-themes".to_string()])
            );
        }
        assert!(
            !WpCommand::boot_check()
                .args()
                .iter()
                .any(|a| a.starts_with("--skip"))
        );
    }

    #[test]
    fn file_changing_commands_include_reset() {
        let list = listed("akismet");
        let akismet = resolved(&list, "akismet");
        let changes: Vec<bool> = every_command(&akismet)
            .iter()
            .map(|command| command.kind().changes_files())
            .collect();
        let expected: Vec<bool> = every_command(&akismet)
            .iter()
            .map(|command| {
                matches!(
                    command.kind(),
                    CommandKind::Install
                        | CommandKind::Uninstall
                        | CommandKind::Reset
                        | CommandKind::Delete
                )
            })
            .collect();
        assert_eq!(changes, expected);
        // `reset` keeps the plugin's files but deletes its translations.
        assert!(WpCommand::reset(&akismet).kind().changes_files());
        assert!(
            !WpCommand::activate(&akismet, ActivationScope::Site)
                .kind()
                .changes_files()
        );
    }

    #[test]
    fn activation_scope_follows_the_observed_status() {
        assert_eq!(
            ActivationScope::of(&PluginStatus::Active),
            Some(ActivationScope::Site)
        );
        assert_eq!(
            ActivationScope::of(&PluginStatus::ActiveNetwork),
            Some(ActivationScope::Network)
        );
        for status in [
            PluginStatus::Inactive,
            PluginStatus::MustUse,
            PluginStatus::Dropin,
            PluginStatus::Unknown("paused".into()),
        ] {
            assert_eq!(ActivationScope::of(&status), None, "{status}");
        }
    }

    #[test]
    fn staged_zips_are_built_from_the_sites_tmp_dir() {
        let staged = StagedZip::new("/var/tmp/x+y", "apvm.0123456789", 3).unwrap();
        assert_eq!(staged.path(), "/var/tmp/x+y/apvm.0123456789/package-3.zip");
        assert_eq!(staged.dir(), "/var/tmp/x+y/apvm.0123456789");
        assert_eq!(staged.index(), 3);
        assert_eq!(
            zip(u32::MAX).path(),
            "/tmp/apvm.Ab3dE6gH9k/package-4294967295.zip"
        );
        let refused = |tmp: &str, name: &str, index: u32, location: &str| {
            let error = StagedZip::new(tmp, name, index).unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput, "{tmp:?} {name:?}");
            let prefix = format!("invalid staged package: {location}: ");
            assert!(
                error.to_string().starts_with(&prefix),
                "{tmp:?} {name:?}: {error}"
            );
        };
        for tmp in [
            "",
            "tmp",
            "/",
            "/tmp/",
            "/tmp/../x",
            "/tmp/a b",
            "/tmp/it's",
            "/tmp/$x",
            "/tmp/\0",
        ] {
            refused(tmp, STAGING, 1, "remote_tmp_dir");
        }
        for name in [
            "",
            "apvm.",
            "apvm.Ab3dE6gH9",
            "apvm.Ab3dE6gH9kk",
            "apvm.Ab3dE6gH9-",
            "apvm.Ab3dE6gH9.",
            "Apvm.Ab3dE6gH9k",
            "apvm.Ab3dE6gH9k/",
            "apvm.Ab3dE6gH9k/x",
            "x/apvm.Ab3dE6gH9k",
            "../apvm.Ab3dE6gH9k",
            "/tmp/apvm.Ab3dE6gH9k",
            "apvm.Ab3dE6gH9é",
            "apvm.Ab3dE6gH\0k",
        ] {
            refused("/tmp", name, 1, "name");
        }
        let error = StagedZip::new("/tmp", STAGING, 0).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid staged package: index: must be at least 1"
        );
        // A valid tmp dir whose package path exceeds 4096 bytes.
        let deep = format!("/{}", "a".repeat(4069));
        refused(&deep, STAGING, 1, "path");
    }

    #[test]
    fn site_bases_check_their_values_and_hold_no_arguments() {
        let mode = FileMode::new(0o755).unwrap();
        let wrapper = ["sudo".to_string(), "eval-file".to_string()];
        let error = WpSite::new(&wrapper, mode, "relative").unwrap_err();
        assert_eq!(error.code(), Code::InvalidInput);
        assert_eq!(error.problems().len(), 3);
        let base = WpSite::new(&["wp".to_string()], mode, "/srv/wp").unwrap();
        assert!(base.clone().url("ftp://x").is_err());
        assert_eq!(
            base.wp_cli().statement(&Identity::Login).render(),
            format!("{ENV} 'wp' --skip-packages --exec='umask(umask() & ~0050);' --path='/srv/wp'")
        );
        let root = base.allow_root();
        assert!(
            root.wp_cli()
                .statement(&Identity::Login)
                .render()
                .ends_with("--allow-root --path='/srv/wp'")
        );
    }

    #[test]
    fn no_load_skips_the_plugin_by_wp_clis_name() {
        // WP-CLI's `is_plugin_skipped()` compares `get_plugin_name()`:
        // verified live, `--skip-plugins=crash.php` loaded a crashing
        // `crash.php` (exit 1), `--skip-plugins=crash` skipped it.
        let single = PluginRow::new(
            "hello".into(),
            PluginStatus::Active,
            None,
            "hello.php".into(),
            None,
        )
        .unwrap();
        let list = PluginList::new(vec![single], vec![]).unwrap();
        for name in ["hello.php", "hello"] {
            let target = resolved(&list, name);
            assert_eq!(target.wp_cli_name().as_str(), "hello");
            let command =
                WpCommand::deactivate(&target, ActivationScope::Site, DeactivateMode::NoLoad);
            assert_eq!(
                command.args(),
                ["plugin", "deactivate", name, "--skip-plugins=hello"],
                "{name}"
            );
        }
        // A folder plugin is skipped by its folder.
        let list = listed("akismet");
        let target = resolved(&list, "akismet");
        let command =
            WpCommand::deactivate(&target, ActivationScope::Network, DeactivateMode::NoLoad);
        assert_eq!(
            command.args(),
            [
                "plugin",
                "deactivate",
                "akismet",
                "--network",
                "--skip-plugins=akismet"
            ]
        );
    }

    #[test]
    fn commands_frame_into_scripts() {
        let identity = Identity::sudo("www-data", Some("www-data")).unwrap();
        let wp = shop().command(&WpCommand::plugin_list()).unwrap();
        let nonce = Nonce::parse("apvm-0123456789abcdef0123456789abcdef").unwrap();
        let script = Script::wp(nonce, &identity, &wp);
        assert!(script.text().contains(&format!(
            "cd / && sudo -n -u 'www-data' -g 'www-data' -- {ENV} {BASE} 'plugin' 'list' "
        )));
        assert!(script.text().ends_with("exit \"$rc\"\n}\n"));
    }

    /// Run `script` through the local `sh -s`, as the server would.
    ///
    /// # Arguments
    ///
    /// * `script` - The framed script
    ///
    /// # Returns
    ///
    /// Its exit status and stdout.
    #[cfg(unix)]
    fn run_sh(script: &Script) -> (Option<i32>, String) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(script.text().as_bytes()).unwrap();
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
        )
    }

    #[test]
    #[cfg(unix)]
    fn every_command_reaches_the_program_verbatim_through_sh() {
        // `/bin/echo` stands in for WP-CLI: it prints the argv it got, so
        // the snippet's `$`, quotes and parentheses must arrive unchanged.
        let echo = WpSite::new(
            &["/bin/echo".to_string()],
            FileMode::new(0o755).unwrap(),
            "/srv/it's $HOME",
        )
        .unwrap();
        let nonce = "apvm-0123456789abcdef0123456789abcdef";
        for command in every_command(&resolved(&listed("akismet"), "akismet")) {
            let wp = echo.command(&command).unwrap();
            let script = Script::wp(Nonce::parse(nonce).unwrap(), &Identity::Login, &wp);
            // The frame prints a newline before the nonce line (§6.1), so
            // login noise without a final newline cannot glue onto it.
            let expected = format!(
                "\n{nonce}\n--skip-packages --exec=umask(umask() & ~0050); \
                 --path=/srv/it's $HOME {}\n",
                command.args().join(" ")
            );
            assert_eq!(run_sh(&script), (Some(0), expected), "{command:?}");
        }
    }
}
