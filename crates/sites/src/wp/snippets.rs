//! The PHP apvm runs with `wp eval`: compile-time constants only (plan
//! §10, T1: "`wp eval` only with compile-time constant snippets", `wp run`
//! aside). No snippet takes a value from a caller, the inventory or the
//! site: whatever varies is read by the snippet inside WordPress, and its
//! output is parsed strictly ([`parse`](super::parse)).
//!
//! - [`SITE_FACTS`]: plan §6.2 step W.
//! - [`REGULAR_PLUGINS`]: the regular plugins as WordPress's
//!   `get_plugins()` sees them, to resolve a
//!   [`Lookup::Shadowed`](super::Lookup::Shadowed).
//!
//! Multisite's
//! per-subsite activations (§6.11) are P10.1's addition. WP-CLI's
//! `--exec` stays reserved for the hermetic umask (§6.1 item 5): a snippet
//! is always `eval`'s positional argument, never `--exec`.

/// The facts of probe step W (plan §6.2), printed as one JSON object:
///
/// | Key | Value |
/// |---|---|
/// | `wp_cli_version` | `WP_CLI_VERSION` (proves `wp_cli` really is WP-CLI) |
/// | `php_version` | `PHP_VERSION` |
/// | `wp_version` | `get_bloginfo( 'version' )` |
/// | `multisite` | `is_multisite()` |
/// | `plugin_dir` | `WP_PLUGIN_DIR` |
/// | `plugin_dir_realpath` | `realpath( WP_PLUGIN_DIR )`, `null` when it fails (step O compares it with step H's path) |
/// | `content_dir` | `WP_CONTENT_DIR` |
/// | `content_dir_realpath` | `realpath( WP_CONTENT_DIR )`, `null` when it fails (step C's space and write checks use the canonical directory) |
/// | `siteurl`, `home` | the options (`siteurl` ≠ the site's `url` is a warning, step C) |
/// | `disallow_file_mods` | `DISALLOW_FILE_MODS` defined and true (§6.7) |
///
/// `JSON_PARTIAL_OUTPUT_ON_ERROR` turns a value `json_encode` cannot encode
/// (a path that is not valid UTF-8) into `null`, which the parser refuses
/// for every key but the two realpaths, instead of the whole object
/// becoming an empty output. One line, no `<?php` (`eval` takes code), and
/// no NUL, so it passes [`WpCli::arg`](crate::remote::WpCli::arg) and is
/// quoted like any argument. Run it with `--skip-plugins --skip-themes`
/// ([`WpCommand::site_facts`](super::WpCommand::site_facts)).
pub const SITE_FACTS: &str = concat!(
    "$apvm_dir = realpath( WP_PLUGIN_DIR ); ",
    "$apvm_content = realpath( WP_CONTENT_DIR ); ",
    "echo json_encode( array( ",
    "'wp_cli_version' => WP_CLI_VERSION, ",
    "'php_version' => PHP_VERSION, ",
    "'wp_version' => get_bloginfo( 'version' ), ",
    "'multisite' => is_multisite(), ",
    "'plugin_dir' => WP_PLUGIN_DIR, ",
    "'plugin_dir_realpath' => false === $apvm_dir ? null : $apvm_dir, ",
    "'content_dir' => WP_CONTENT_DIR, ",
    "'content_dir_realpath' => false === $apvm_content ? null : $apvm_content, ",
    "'siteurl' => get_option( 'siteurl' ), ",
    "'home' => get_option( 'home' ), ",
    "'disallow_file_mods' => defined( 'DISALLOW_FILE_MODS' ) && DISALLOW_FILE_MODS ",
    "), JSON_PARTIAL_OUTPUT_ON_ERROR );",
);

/// The keys [`SITE_FACTS`] prints, in order: the parser's tests check that
/// it accepts exactly these.
#[cfg(test)]
pub(crate) const SITE_FACTS_KEYS: [&str; 11] = [
    "wp_cli_version",
    "php_version",
    "wp_version",
    "multisite",
    "plugin_dir",
    "plugin_dir_realpath",
    "content_dir",
    "content_dir_realpath",
    "siteurl",
    "home",
    "disallow_file_mods",
];

/// Every regular plugin file WordPress's `get_plugins()` finds, printed in
/// `plugin list`'s row shape plus one field: `name` (WP-CLI's
/// `get_plugin_name()`: the folder, or the file without `.php`), `status`
/// (WP-CLI's `get_status()`: `active-network` checked before `active`,
/// since `is_plugin_active()` is also true for a network-active plugin),
/// `version` and `title` (the `Version` and `Plugin Name` headers), `file`,
/// and `file_hex`, the file name's bytes in hexadecimal: lossless, so a
/// file name that is not valid UTF-8 (printed as `null` in `file`) is
/// still known (see [`Lookup::Unreadable`](super::Lookup::Unreadable)).
///
/// Unlike `plugin list`, no must-use plugin or drop-in can replace a row
/// here (see [`Lookup::Shadowed`](super::Lookup::Shadowed)).
/// `--skip-plugins` does not change the statuses (WP-CLI removes its
/// filter once plugins are loaded; verified with WP-CLI 2.12.0).
///
/// It reads the raw `get_plugins()`, while WP-CLI's fetcher and `plugin
/// list` read `apply_filters( 'all_plugins', get_plugins() )`. Under
/// `--skip-plugins` only must-use plugins and drop-ins can filter it, and
/// they are site code that can falsify any read (plan §10, known limits).
/// A filter that hides a plugin leaves it here, so a slug that resolves to
/// it alone makes WP-CLI fail with "could not be found" (nothing acts on
/// another plugin, and the verification read reports it). An entry a filter
/// adds is not here, but the fetcher can still take it: `activate` refuses
/// it (`validate_plugin` reads the unfiltered list), while `uninstall` and
/// `delete` act on its `dirname`. Only a deliberate filter in site code
/// does that, which can falsify any read anyway. Run it with
/// `--skip-plugins --skip-themes`
/// ([`WpCommand::regular_plugins`](super::WpCommand::regular_plugins)).
pub const REGULAR_PLUGINS: &str = concat!(
    "if ( ! function_exists( 'get_plugins' ) ) { require_once ABSPATH . 'wp-admin/includes/plugin.php'; } ",
    "$apvm_rows = array(); ",
    "foreach ( get_plugins() as $apvm_file => $apvm_data ) { ",
    "$apvm_rows[] = array( ",
    "'name' => false === strpos( $apvm_file, '/' ) ? basename( $apvm_file, '.php' ) : dirname( $apvm_file ), ",
    "'status' => is_plugin_active_for_network( $apvm_file ) ? 'active-network' ",
    ": ( is_plugin_active( $apvm_file ) ? 'active' : 'inactive' ), ",
    "'version' => $apvm_data['Version'], ",
    "'file' => $apvm_file, ",
    "'title' => $apvm_data['Name'], ",
    "'file_hex' => bin2hex( $apvm_file ) ",
    "); } ",
    "echo json_encode( $apvm_rows, JSON_PARTIAL_OUTPUT_ON_ERROR );",
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist;

    /// Every snippet.
    const SNIPPETS: [&str; 2] = [SITE_FACTS, REGULAR_PLUGINS];

    /// Check that `snippet` prints each of `keys` once, in order.
    ///
    /// # Arguments
    ///
    /// * `snippet` - The PHP
    /// * `keys` - The keys it must print
    fn prints_keys_in_order(snippet: &str, keys: &[&str]) {
        let mut from = 0;
        for key in keys {
            let quoted = format!("'{key}' =>");
            assert_eq!(snippet.matches(&quoted).count(), 1, "{key}");
            let at = snippet.find(&quoted).unwrap();
            assert!(at > from, "{key} is out of order");
            from = at;
        }
        assert_eq!(snippet.matches("' =>").count(), keys.len());
    }

    #[test]
    fn snippets_are_one_safe_eval_argument_each() {
        for snippet in SNIPPETS {
            assert!(!snippet.contains(['\n', '\r', '\0']));
            assert!(!snippet.contains("<?"));
            assert!(!snippet.starts_with('-') && !snippet.starts_with('@'));
            assert_eq!(allowlist::wp_cli_arg(snippet, false), Ok(()));
            assert!(snippet.ends_with(';'));
        }
    }

    #[test]
    fn snippets_print_every_key_once_in_order() {
        prints_keys_in_order(SITE_FACTS, &SITE_FACTS_KEYS);
        // The `plugin list` row shape, plus the lossless file name.
        prints_keys_in_order(
            REGULAR_PLUGINS,
            &["name", "status", "version", "file", "title", "file_hex"],
        );
    }

    #[test]
    fn regular_plugins_checks_network_activation_first() {
        // `is_plugin_active()` is true for a network-active plugin too.
        let network = REGULAR_PLUGINS
            .find("is_plugin_active_for_network( $apvm_file ) ? 'active-network'")
            .unwrap();
        let site = REGULAR_PLUGINS
            .find("is_plugin_active( $apvm_file ) ? 'active'")
            .unwrap();
        assert!(network < site);
        assert!(REGULAR_PLUGINS.contains("'file_hex' => bin2hex( $apvm_file )"));
    }

    #[test]
    fn snippets_balance_their_brackets_and_quotes() {
        for snippet in SNIPPETS {
            let count = |c: char| snippet.matches(c).count();
            assert_eq!(count('('), count(')'));
            assert_eq!(count('{'), count('}'));
            assert_eq!(count('['), count(']'));
            assert_eq!(count('\'') % 2, 0);
            assert!(!snippet.contains('"'));
        }
    }
}
