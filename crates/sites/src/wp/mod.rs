//! WordPress through WP-CLI: the typed commands apvm runs on a site, and
//! strict parsers for what they print (plan §6.1 item 5, §6.2, §6.4, §9).
//!
//! - [`WpCommand`]: every WP-CLI command the lifecycle needs, built from
//!   typed values only ([`Resolved`] plugins, [`StagedZip`]), so no caller
//!   can add a flag, nor name a slug WP-CLI could take for another plugin. The lifecycle verbs never use WP-CLI's `--all`, `--exclude`,
//!   `--deactivate`, `--uninstall` or `--activate` (plan §9 rule 6, D9):
//!   each step is one process, verified by a read.
//! - [`WpSite`]: one site's hermetic WP-CLI base (plan §6.1 item 5,
//!   P3.1's [`WpCli`](crate::remote::WpCli)), which turns a
//!   [`WpCommand`] into the command a [`Script`](crate::remote::Script)
//!   runs.
//! - [`parse`]: reads a command's [`CommandOutput`] (stdout after the
//!   nonce, stderr, exit status). PHP notices before or after the payload
//!   (the PHP 8.5 deprecation line, a shutdown warning) are skipped and
//!   reported; anything else that is not exactly the expected payload (an
//!   HTML page, truncated or empty output, a second payload, an unknown
//!   line) is refused as [`Error::WpOutput`](crate::Error::WpOutput)
//!   (`wp_command_failed`). A read never turns noise into "not installed".
//!   Commands that load plugins are not reads ([`CommandKind::is_read`]):
//!   the boot check and every mutation run under the site lock and are
//!   never retried; [`CommandKind::changes_files`] is true for `reset` too
//!   (WP-CLI deletes the plugin's translations).
//! - [`PluginList`] and [`PluginRow`]: the plugins a site reports, with
//!   their [`PluginStatus`] (unknown values kept explicitly), looked up by
//!   slug the way WP-CLI's plugin fetcher resolves it ([`Lookup`]). WP-CLI
//!   hides a regular single-file plugin behind a must-use plugin or drop-in
//!   of the same file name, so such a lookup is [`Lookup::Shadowed`], never
//!   "absent"; [`WpCommand::regular_plugins`] and
//!   [`PluginList::find_regular`] resolve it.
//! - [`BootCheck`]: booted when `option get siteurl` exits 0 and prints
//!   one URL line; a changed siteurl is a warning.
//! - [`RequiresPlugins`]: a plugin's dependencies, read as WordPress reads
//!   its `Requires Plugins` header.
//! - [`snippets`]: the constant PHP snippets apvm runs with `wp eval`
//!   (plan §6.2 step W, §10: `wp eval` only with compile-time constants).
//! - [`probe`]: plan §6.2 end to end through a
//!   [`Transport`](crate::remote::Transport): the site's identity, whether
//!   `wp_cli` is WP-CLI, WordPress's facts, and what the identity can do.
//!
//! Everything here but [`probe`] is pure: no I/O, no transport.
//!
//! # Example
//!
//! ```
//! use apvm_sites::remote::{FileMode, Identity, Nonce, Script};
//! use apvm_sites::wp::{self, ActivationScope, CommandOutput, Slug, WpCommand, WpSite};
//!
//! # fn main() -> apvm_sites::Result<()> {
//! // The site's plugins, as `plugin list` printed them (here with the PHP
//! // 8.5 deprecation line WP-CLI 2.12.0 prints when `display_errors` is on).
//! let stdout = b"\nDeprecated: Case statements followed by a semicolon (;) are deprecated, \
//!     use a colon (:) instead in phar:///usr/local/bin/wp/vendor/react/promise/src/functions.php \
//!     on line 369\n[{\"name\":\"akismet\",\"status\":\"inactive\",\"version\":\"5.7.2\",\
//!     \"file\":\"akismet\\/akismet.php\",\"title\":\"Akismet\"}]";
//! let list = wp::parse::plugin_list(&CommandOutput::new(0, stdout, b""))?;
//! assert_eq!(list.notices.total(), 1);
//!
//! // A command acts only on a plugin its slug resolves to uniquely.
//! let Some(akismet) = list.value.resolve(&Slug::new("akismet")?) else {
//!     panic!("akismet is listed once");
//! };
//! assert_eq!(akismet.row().version(), Some("5.7.2"));
//! let site = WpSite::new(&["wp".to_string()], FileMode::new(0o2775)?, "/var/www/shop")?
//!     .url("https://shop.example.com")?;
//! let activate = site.command(&WpCommand::activate(&akismet, ActivationScope::Site))?;
//! let script = Script::wp(Nonce::random(), &Identity::Login, &activate);
//! assert!(script.text().contains("--url='https://shop.example.com' 'plugin' 'activate' 'akismet'"));
//! # Ok(())
//! # }
//! ```

mod command;
pub mod parse;
pub mod probe;
pub mod snippets;
mod state;

pub use command::{
    ActivationScope, CommandKind, DeactivateMode, InstallMode, StagedZip, WpCommand, WpSite,
};
pub use parse::{
    BootCheck, CommandOutput, Notices, OutputDefect, Parsed, RequiresPlugins, SiteFacts,
    WpCliVersion,
};
pub use probe::{ProbeFailure, ProbeWarning, SiteProbe};
pub use state::{Lookup, PluginList, PluginRow, PluginStatus, Resolved, Slug};
