//! Plugin headers, read the way WordPress reads them (plan §6.10).
//!
//! Mirrors `get_file_data()` and `get_plugin_data()` of WordPress 7.1.3
//! (`wp-includes/functions.php`, `wp-admin/includes/plugin.php`), on bytes:
//!
//! - only the first [`HEADER_BYTES`] (8 KiB) of the file count;
//! - every `\r` becomes `\n`, so CR-only and CRLF line endings work;
//! - a header is the **first** line matching
//!   `^(?:[ \t]*<\?(?:php)?)?[ \t/*#@]*<Label>:(.*)$`, with the label in any
//!   ASCII letter case. When that line's value is `''` or `'0'` (falsy in
//!   PHP), the header is empty: later lines are not tried;
//! - the value is cut at the first `*/` or `?>` (and the whitespace before
//!   it), then trimmed like PHP's `trim()`.
//!
//! Values are decoded as UTF-8 with invalid sequences replaced, so a
//! multibyte character cut at the 8 KiB boundary yields `U+FFFD`, never a
//! panic.

/// How much of a PHP file WordPress reads for its headers: `8 * KB_IN_BYTES`.
pub const HEADER_BYTES: usize = 8 * 1024;

/// The header WordPress requires: a file without it is not a plugin.
const NAME: &str = "Plugin Name";

/// Every label read, in the order [`values`] returns them.
const LABELS: [&str; 8] = [
    NAME,
    "Version",
    "Requires at least",
    "Requires PHP",
    "Network",
    "Site Wide Only",
    "Requires Plugins",
    "Text Domain",
];

/// The characters `[ \t/*#@]` the regex allows before a label.
const PREFIX_CLASS: &[u8] = b" \t/*#@";

/// The headers of a plugin's main file that apvm uses (plan §6.10).
///
/// Values are what WordPress reads; they may hold any character, so quote
/// or escape them before printing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginHeaders {
    /// `Plugin Name`: never empty, never `0` (WordPress skips such files).
    pub name: String,
    /// `Version`.
    pub version: Option<String>,
    /// `Requires at least`: the oldest WordPress version supported.
    pub requires_wp: Option<String>,
    /// `Requires PHP`: the oldest PHP version supported.
    pub requires_php: Option<String>,
    /// `Network: true` (any letter case), or the deprecated
    /// `Site Wide Only: true` when `Network` is empty or `0`: the plugin can
    /// only be network-activated on a multisite.
    pub network: bool,
    /// `Requires Plugins`: the slugs this plugin depends on. Like WordPress,
    /// entries are split on `,`, trimmed, and kept only in the WordPress.org
    /// slug format (`^[a-z0-9]+(-[a-z0-9]+)*$`); unique, in byte order.
    pub requires_plugins: Vec<String>,
    /// `Text Domain`, as written (WordPress falls back to the slug when it
    /// is empty).
    pub text_domain: Option<String>,
}

impl PluginHeaders {
    /// Read the headers from the start of a PHP file.
    ///
    /// # Arguments
    ///
    /// * `head` - The file's first bytes; anything past [`HEADER_BYTES`] is
    ///   ignored, as WordPress ignores it
    ///
    /// # Returns
    ///
    /// The headers, or `None` when the file has no usable `Plugin Name`
    /// (WordPress does not treat it as a plugin).
    pub(crate) fn parse(head: &[u8]) -> Option<Self> {
        let text = normalize(head);
        let [
            name,
            version,
            requires_wp,
            requires_php,
            network,
            site_wide,
            requires,
            domain,
        ] = values(&text).map(|raw| raw.and_then(clean_value));
        let name = name.filter(|name| name != "0")?;
        // `! $plugin_data['Network']`: a `0` is falsy too, so it falls back.
        let network = network.filter(|value| value != "0").or(site_wide);
        Some(Self {
            name,
            version,
            requires_wp,
            requires_php,
            network: network.is_some_and(|value| value.eq_ignore_ascii_case("true")),
            requires_plugins: requires
                .map(|value| dependency_slugs(&value))
                .unwrap_or_default(),
            text_domain: domain,
        })
    }
}

/// The raw value (`(.*)$`) of each of [`LABELS`]: the rest of the first
/// line that matches it, as `preg_match()` finds it.
///
/// One pass over the lines, so 8 KiB of short lines costs one scan, not one
/// per label. Lines without a `:` are skipped: the regex needs one.
fn values(text: &[u8]) -> [Option<&[u8]>; 8] {
    let mut raw = [None; 8];
    for line in text.split(|&byte| byte == b'\n') {
        if !line.contains(&b':') {
            continue;
        }
        match_labels(line, &mut raw);
        if raw.iter().all(Option::is_some) {
            break;
        }
    }
    raw
}

/// Fill the slots of `raw` still empty with the labels `line` holds.
///
/// The regex `^(?:[ \t]*<\?(?:php)?)?[ \t/*#@]*<label>:` tries, in order:
/// after an opening tag `<?php`, after a short tag `<?`, and with no tag. A
/// label starts with a letter, which the class never holds, so the greedy
/// class run is the only split it can use.
fn match_labels<'l>(line: &'l [u8], raw: &mut [Option<&'l [u8]>; 8]) {
    let tagged = trim_start(line, b" \t").strip_prefix(b"<?");
    let with_php = tagged.and_then(|rest| strip_prefix_ignore_case(rest, b"php"));
    for start in [with_php, tagged, Some(line)].into_iter().flatten() {
        let start = trim_start(start, PREFIX_CLASS);
        for (slot, label) in raw.iter_mut().zip(LABELS) {
            if slot.is_none() {
                *slot = strip_prefix_ignore_case(start, label.as_bytes())
                    .and_then(|rest| rest.strip_prefix(b":"));
            }
        }
    }
}

/// The first [`HEADER_BYTES`] of `head`, with every `\r` turned into `\n`.
fn normalize(head: &[u8]) -> Vec<u8> {
    head.iter()
        .take(HEADER_BYTES)
        .map(|&byte| if byte == b'\r' { b'\n' } else { byte })
        .collect()
}

/// A header's value, as `get_file_data()` returns it, from its raw match;
/// `None` for `''`.
fn clean_value(raw: &[u8]) -> Option<String> {
    // PHP's `&& $match[1]`: '' and '0' are falsy, and the first matching
    // line decides even then.
    if raw.is_empty() || raw == b"0" {
        return None;
    }
    let value = cleanup(raw);
    (!value.is_empty()).then(|| String::from_utf8_lossy(value).into_owned())
}

/// `text` without its leading bytes from `set`.
fn trim_start<'t>(text: &'t [u8], set: &[u8]) -> &'t [u8] {
    let start = text
        .iter()
        .position(|byte| !set.contains(byte))
        .unwrap_or(text.len());
    &text[start..]
}

/// `text` without `prefix`, compared in any ASCII letter case (PCRE `/i`
/// without `/u`).
fn strip_prefix_ignore_case<'t>(text: &'t [u8], prefix: &[u8]) -> Option<&'t [u8]> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// `_cleanup_header_comment()`: drop everything from the whitespace before
/// the first `*/` or `?>` (`/\s*(?:\*\/|\?>).*/`), then PHP's `trim()`.
fn cleanup(raw: &[u8]) -> &[u8] {
    let end = raw
        .windows(2)
        .position(|pair| pair == b"*/" || pair == b"?>")
        .map_or(raw.len(), |terminator| {
            let kept = &raw[..terminator];
            kept.len() - kept.iter().rev().take_while(|b| is_pcre_space(**b)).count()
        });
    php_trim(&raw[..end])
}

/// PCRE's `\s` without UTF mode: space, `\t`, `\n`, `\v`, `\f`, `\r`.
fn is_pcre_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// PHP's `trim()` with its default set: space, `\t`, `\n`, `\r`, NUL, `\v`.
fn php_trim(text: &[u8]) -> &[u8] {
    const SET: &[u8] = b" \t\n\r\0\x0B";
    let start = trim_start(text, SET);
    let kept = start.iter().rev().take_while(|b| SET.contains(b)).count();
    &start[..start.len() - kept]
}

/// `WP_Plugin_Dependencies::sanitize_dependency_slugs()`: split on `,`,
/// trim, keep WordPress.org-style slugs, dedupe, sort.
fn dependency_slugs(value: &str) -> Vec<String> {
    let mut slugs: Vec<String> = value
        .split(',')
        .map(|slug| String::from_utf8_lossy(php_trim(slug.as_bytes())).into_owned())
        .filter(|slug| is_dependency_slug(slug))
        .collect();
    slugs.sort();
    slugs.dedup();
    slugs
}

/// Whether `slug` matches `^[a-z0-9]+(-[a-z0-9]+)*$`.
fn is_dependency_slug(slug: &str) -> bool {
    slug.split('-').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headers of `text`, which must have a plugin name.
    fn parse(text: &str) -> PluginHeaders {
        match PluginHeaders::parse(text.as_bytes()) {
            Some(headers) => headers,
            None => panic!("expected headers in {text:?}"),
        }
    }

    /// A header's value as `get_file_data()` would return it.
    fn value(text: &str, label: &str) -> Option<String> {
        let index = LABELS.iter().position(|known| *known == label)?;
        values(&normalize(text.as_bytes()))[index].and_then(clean_value)
    }

    #[test]
    fn reads_a_standard_header_block() {
        let headers = parse(
            "<?php\n/**\n * Plugin Name: BackWPup Pro\n * Version: 5.7.6\n \
             * Requires at least: 6.4\n * Requires PHP: 7.4\n * Network: true\n \
             * Text Domain: backwpup\n * Requires Plugins: woocommerce, akismet\n */\n",
        );
        assert_eq!(headers.name, "BackWPup Pro");
        assert_eq!(headers.version.as_deref(), Some("5.7.6"));
        assert_eq!(headers.requires_wp.as_deref(), Some("6.4"));
        assert_eq!(headers.requires_php.as_deref(), Some("7.4"));
        assert!(headers.network);
        assert_eq!(headers.text_domain.as_deref(), Some("backwpup"));
        assert_eq!(headers.requires_plugins, ["akismet", "woocommerce"]);
    }

    #[test]
    fn a_file_without_a_plugin_name_is_not_a_plugin() {
        for text in [
            "",
            "<?php\n// Version: 1.0\n",
            "<?php\n/* Plugin Name: */\n",
            // The first matching line decides, even when it is empty.
            "<?php\n/* Plugin Name:\n * Plugin Name: Later */\n",
            // WordPress's empty('0') skips a plugin named 0.
            "<?php\n/* Plugin Name: 0 */\n",
            "<?php\n/* Plugin Name:0 */\n",
            // The label must be followed by ':' right away.
            "<?php\n/* Plugin Name : X */\n",
            // Letters before the label are not in the prefix class.
            "<?php\n/* My Plugin Name: X */\n",
        ] {
            assert_eq!(PluginHeaders::parse(text.as_bytes()), None, "{text:?}");
        }
    }

    #[test]
    fn line_prefixes_follow_the_regex() {
        for (text, expected) in [
            ("Plugin Name: Bare", "Bare"),
            ("<?php Plugin Name: Tag", "Tag"),
            ("  \t<?PHP /* Plugin Name: Upper tag */", "Upper tag"),
            ("<? Plugin Name: Short tag", "Short tag"),
            ("<?phpPlugin Name: Glued", "Glued"),
            ("# Plugin Name: Hash", "Hash"),
            ("@@ Plugin Name: At", "At"),
            ("// Plugin Name: Slashes", "Slashes"),
            ("\t * plugin name: Lower case label", "Lower case label"),
        ] {
            assert_eq!(value(text, NAME).as_deref(), Some(expected), "{text:?}");
        }
        for text in [
            "x Plugin Name: Y",
            "<?php x Plugin Name: Y",
            "- Plugin Name: Y",
        ] {
            assert_eq!(value(text, NAME), None, "{text:?}");
        }
    }

    #[test]
    fn values_are_cleaned_like_wordpress() {
        for (line, expected) in [
            ("Version: 1.2.3 */", Some("1.2.3")),
            ("Version: 1.2 ?> trailing", Some("1.2")),
            ("Version: \t 1.0 \x0B\0", Some("1.0")),
            ("Version: a*/b*/", Some("a")),
            ("Version: */", None),
            ("Version:  0", Some("0")),
            ("Version: 1.0 \x0C*/", Some("1.0")),
            // NUL is trimmed by trim() but is not \s: it stops the \s* run.
            ("Version: 1.0\0 */", Some("1.0")),
        ] {
            assert_eq!(value(line, "Version").as_deref(), expected, "{line:?}");
        }
    }

    #[test]
    fn crlf_and_cr_line_endings_work() {
        let crlf = parse("<?php\r\n/*\r\n * Plugin Name: CRLF\r\n * Version: 2.0\r\n */\r\n");
        assert_eq!(crlf.name, "CRLF");
        assert_eq!(crlf.version.as_deref(), Some("2.0"));
        let cr = parse("<?php\r/*\r * Plugin Name: CR only\r * Version: 3.0\r */\r");
        assert_eq!(cr.name, "CR only");
        assert_eq!(cr.version.as_deref(), Some("3.0"));
    }

    #[test]
    fn only_the_first_8_kib_count() {
        let padding = format!("<?php\n/*{}*/\n", " ".repeat(HEADER_BYTES));
        let late = format!("{padding}/* Plugin Name: Too late */\n");
        assert_eq!(PluginHeaders::parse(late.as_bytes()), None);

        // A header ending exactly at the boundary is read; its tail is cut.
        let line = "Plugin Name: Edge";
        let filler = "\n".repeat(HEADER_BYTES - line.len());
        let edge = format!("{filler}{line}Overflow");
        assert_eq!(parse(&edge).name, "Edge");
    }

    #[test]
    fn a_multibyte_character_cut_at_the_boundary_does_not_panic() {
        let line = "Plugin Name: Caf\u{e9}";
        // 'é' is two bytes: start it at the last byte of the window.
        let filler = "\n".repeat(HEADER_BYTES - (line.len() - 1));
        let text = format!("{filler}{line}");
        assert_eq!(parse(&text).name, "Caf\u{fffd}");
        // Valid multibyte text inside the window is kept.
        assert_eq!(
            parse("Plugin Name: Caf\u{e9} \u{1f680}").name,
            "Caf\u{e9} \u{1f680}"
        );
        // Invalid UTF-8 anywhere is replaced, never a panic.
        assert_eq!(
            PluginHeaders::parse(b"Plugin Name: \xff\xfe ok").map(|h| h.name),
            Some("\u{fffd}\u{fffd} ok".into())
        );
    }

    #[test]
    fn network_reads_true_in_any_case_and_falls_back_to_site_wide_only() {
        assert!(parse("Plugin Name: A\nNetwork: TRUE").network);
        assert!(!parse("Plugin Name: A\nNetwork: yes").network);
        assert!(!parse("Plugin Name: A").network);
        assert!(parse("Plugin Name: A\nSite Wide Only: true").network);
        // A non-empty Network wins over the deprecated header.
        assert!(!parse("Plugin Name: A\nNetwork: false\nSite Wide Only: true").network);
        // ... unless it is "0", which PHP reads as false (WordPress 7.1.3
        // `get_plugin_data()`: `! $plugin_data['Network'] && ...`).
        assert!(parse("Plugin Name: A\nNetwork: 0\nSite Wide Only: true").network);
        assert!(parse("Plugin Name: A\nNetwork:  0 */\nSite Wide Only: TRUE").network);
        assert!(!parse("Plugin Name: A\nNetwork: 0\nSite Wide Only: 0").network);
    }

    #[test]
    fn requires_plugins_keeps_wordpress_org_slugs_only() {
        let headers =
            parse("Plugin Name: A\nRequires Plugins: b-c, a, , A, x_y, -d, e-, f--g, a, 9, h\t");
        assert_eq!(headers.requires_plugins, ["9", "a", "b-c", "h"]);
        assert!(parse("Plugin Name: A").requires_plugins.is_empty());
    }

    #[test]
    fn absent_optional_headers_are_none() {
        let headers = parse("Plugin Name: Minimal");
        assert_eq!(
            headers,
            PluginHeaders {
                name: "Minimal".into(),
                version: None,
                requires_wp: None,
                requires_php: None,
                network: false,
                requires_plugins: Vec::new(),
                text_domain: None,
            }
        );
    }

    #[test]
    fn the_first_matching_line_wins() {
        let headers = parse("Plugin Name: First\nPlugin Name: Second\nVersion: 1\nVersion: 2");
        assert_eq!(headers.name, "First");
        assert_eq!(headers.version.as_deref(), Some("1"));
        // "Version" is not "Versions".
        assert_eq!(
            value("Versions: 9\nVersion: 1", "Version").as_deref(),
            Some("1")
        );
    }
}
