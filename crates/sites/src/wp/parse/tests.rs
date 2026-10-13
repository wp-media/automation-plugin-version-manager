//! Parser tests.
//!
//! Fixtures under `tests/fixtures/wp/` are **real outputs**, captured on
//! 2026-10-12 from the P0 harness (`tests/e2e/sites/`: WordPress 7.1.3,
//! PHP 8.4.26, WP-CLI 2.12.0) with the commands of this module, most of
//! them through P3.1's framed scripts (nonce line removed):
//!
//! | Fixture | What produced it |
//! |---|---|
//! | `plugin-list-single.json` | `single`: a folder plugin, a single-file plugin, a must-use plugin, a drop-in |
//! | `plugin-list-multisite.json` | `multi` after `plugin activate hello --network` |
//! | `plugin-list-empty.json` | a list filtered to nothing (`[]`, no newline) |
//! | `plugin-list-unicode.json` | `single` plus a title with `é ☕ 😀 " <b> \`, a `My Plugin` folder with `Version: 2`, no `Version` header, and a title that is not valid UTF-8 (`null`) |
//! | `plugin-list-unnamed.json` | a folder and a file whose names are not valid UTF-8 (`"name":null,"file":null`) |
//! | `plugin-list-shutdown-warning.txt` | a must-use plugin turning `display_errors` on in a shutdown function: the warning follows the JSON |
//! | `maintenance-page.html.txt` | a must-use plugin printing a maintenance page and exiting (exit 0) |
//! | `fatal-must-use.stderr.txt` | a must-use plugin throwing (exit 1, stdout empty) |
//! | `wp-die.stderr.txt` | a must-use plugin calling `wp_die()` (exit 1, stdout empty) |
//! | `install-exists.stderr.txt` | `plugin install <staged zip>` over an existing folder (exit 1) |
//! | `deactivate-network-active.stderr.txt` | `plugin deactivate` of a network-active plugin without `--network` (exit 1) |
//! | `cli-version.txt` | `cli version` |
//! | `cli-version-php85.txt` | `cli version` under PHP 8.5.11 with `display_errors=stdout` (the WP-CLI 2.12.0 phar in `php:8.5-cli`) |
//! | `site-facts-*.json` | the `SITE_FACTS` snippet on `single`, `multi`, `single` with `DISALLOW_FILE_MODS`, and `single` with a non-canonical `WP_CONTENT_DIR` |
//! | `plugin-list-shadowed.json` | an active regular `advanced-cache.php` behind the drop-in, Hello Dolly behind a must-use `hello.php` |
//! | `plugin-list-shadowed-hello-php.json`, `regular-plugins-shadowed-hello-php.json` | the same must-use `hello.php` plus a regular `hello.php.php`, as `plugin list` and as `REGULAR_PLUGINS` |
//! | `plugin-list-multi-file-folder.json`, `regular-plugins-shadowed.json` | `twofiles/{a,b}.php`, `foo.php/main.php`, `foo.php.php` and the shadows above, both reads |
//! | `plugin-list-unreadable.json`, `regular-plugins-unreadable.json` | `akismet/a\xe9.php` (Latin-1) next to Akismet, both reads |
//! | `regular-plugins-{single,multisite}.json` | `REGULAR_PLUGINS` on a clean `single`, and on `multi` with Hello Dolly network-active |
//! | `requires-plugins-*.json` | `plugin get --fields=name,requires_plugins` (`not-utf8`: a header that is not valid UTF-8 prints only `\n`) |
//! | `boot-check-*.txt` | `option get siteurl`: clean, a plugin printing a page and exiting on `init`, a `die()` on `plugins_loaded` |
//! | `xdebug-*.txt`, `php-multiline-notice.txt` | PHP 8.4.26 with Xdebug 3 (develop mode) and without, in `php:8.4-cli` |
//!
//! Inline cases are **synthetic**, built from those: a truncated list, an
//! unknown status, the PHP 8.5 deprecation line in front of a list (the
//! line is real; the combination could not run: the PHP 8.5 image has no
//! MySQL driver), PHP's log format, byte-order marks, wrong types, extra,
//! missing and duplicate fields.

use super::scan::{is_trace_line, strip_log_time};
use super::*;
use crate::error::Code;
use crate::wp::snippets::SITE_FACTS_KEYS;
use crate::wp::{Lookup, Resolved, Slug};

/// A real fixture's bytes.
macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/wp/",
            $name
        ))
        .as_slice()
    };
}

/// The PHP 8.5 deprecation line WP-CLI 2.12.0 prints first (real, from
/// `cli-version-php85.txt`).
const PHP85_DEPRECATION: &str = "Deprecated: Case statements followed by a semicolon (;) are \
    deprecated, use a colon (:) instead in \
    phar:///usr/local/bin/wp/vendor/react/promise/src/functions.php on line 369";

/// A successful command's output.
///
/// # Arguments
///
/// * `stdout` - What it printed
fn ok(stdout: &[u8]) -> CommandOutput<'_> {
    CommandOutput::new(0, stdout, b"")
}

/// `plugin list`'s parse of `stdout` (exit 0).
///
/// # Arguments
///
/// * `stdout` - What it printed
fn list(stdout: &[u8]) -> Result<Parsed<PluginList>> {
    plugin_list(&ok(stdout))
}

/// The defect inside `error`, which must be `wp_command_failed`.
///
/// # Arguments
///
/// * `error` - The parser's error
fn defect(error: Error) -> OutputDefect {
    assert_eq!(error.code(), Code::WpCommandFailed, "{error}");
    match error {
        Error::WpOutput { defect, .. } => defect,
        other => panic!("not a WP output error: {other}"),
    }
}

/// The defect of a refused parse.
///
/// # Arguments
///
/// * `result` - The parser's result
fn defect_of<T: std::fmt::Debug>(result: Result<T>) -> OutputDefect {
    defect(result.unwrap_err())
}

/// The defect of a refused `plugin list` output.
///
/// # Arguments
///
/// * `stdout` - What it printed
fn list_defect(stdout: &[u8]) -> OutputDefect {
    defect_of(list(stdout))
}

/// `slug` as a [`Slug`].
///
/// # Arguments
///
/// * `slug` - A valid slug
fn slug(slug: &str) -> Slug {
    Slug::new(slug).unwrap()
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
    list.resolve(&slug(name)).unwrap()
}

/// One `plugin list` row as WP-CLI prints it.
///
/// # Arguments
///
/// * `name` - The name
/// * `status` - The status
/// * `file` - The file (with `\/` as WP-CLI escapes it)
fn json_row(name: &str, status: &str, file: &str) -> String {
    format!(
        r#"{{"name":"{name}","status":"{status}","version":"1.0","file":"{file}","title":"T"}}"#
    )
}

/// The real single-site facts with `key` set to `value` (raw JSON), or
/// removed when `value` is `None`.
///
/// # Arguments
///
/// * `key` - A fact's key
/// * `value` - Its new JSON value
fn facts_with(key: &str, value: Option<&str>) -> String {
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(fixture!("site-facts-single.json")).unwrap();
    match value {
        Some(value) => {
            map.insert(key.to_string(), serde_json::from_str(value).unwrap());
        }
        None => assert!(map.remove(key).is_some(), "{key}"),
    }
    serde_json::to_string(&map).unwrap()
}

#[test]
fn a_clean_list_reads_every_row_shape() {
    let parsed = list(fixture!("plugin-list-single.json")).unwrap();
    assert!(parsed.notices.is_empty());
    let rows: Vec<_> = parsed
        .value
        .rows()
        .iter()
        .map(|r| {
            (
                r.name(),
                r.status().as_str(),
                r.version(),
                r.file(),
                r.title(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                "akismet",
                "inactive",
                Some("5.7.2"),
                "akismet/akismet.php",
                Some("Akismet Anti-spam: Spam Protection")
            ),
            (
                "hello",
                "inactive",
                Some("1.7.2"),
                "hello.php",
                Some("Hello Dolly")
            ),
            (
                "apvm-e2e-mu",
                "must-use",
                None,
                "apvm-e2e-mu.php",
                Some("APVM E2E Must-Use")
            ),
            (
                "advanced-cache.php",
                "dropin",
                None,
                "advanced-cache.php",
                None
            ),
        ]
    );
    let list = parsed.value;
    assert!(list.find(&slug("hello")).found().unwrap().is_single_file());
    // The drop-in's file is the slug: it may hide a regular file of that
    // name, which WP-CLI's fetcher would take (replaces `Found(drop-in)`).
    let Lookup::Shadowed { by } = list.find(&slug("advanced-cache.php")) else {
        panic!("a drop-in at the slug's file");
    };
    assert_eq!(by.status(), &PluginStatus::Dropin);
    assert!(list.find(&slug("backwpup")).is_absent());
    let regular: Vec<&str> = list.regular().map(PluginRow::name).collect();
    assert_eq!(regular, ["akismet", "hello"]);
    assert_eq!(list.unnamed(), 0);
}

#[test]
fn multisite_lists_read_network_active_rows() {
    let list = list(fixture!("plugin-list-multisite.json")).unwrap().value;
    let hello = list.find(&slug("hello")).found().unwrap();
    assert_eq!(hello.status(), &PluginStatus::ActiveNetwork);
    assert!(hello.status().is_active());
}

#[test]
fn an_empty_list_is_a_fact_but_empty_output_is_not() {
    let parsed = list(fixture!("plugin-list-empty.json")).unwrap();
    assert!(parsed.value.rows().is_empty());
    assert!(parsed.value.find(&slug("akismet")).is_absent());
    for empty in [&b""[..], b"\n", b"\n\n  \r\n", b"\t"] {
        assert_eq!(list_defect(empty), OutputDefect::Empty, "{empty:?}");
    }
}

#[test]
fn unicode_titles_versions_as_text_and_null_titles() {
    let list = list(fixture!("plugin-list-unicode.json")).unwrap().value;
    let cafe = list.find(&slug("cafe")).found().unwrap();
    assert_eq!(
        cafe.title(),
        Some("Café ☕ 😀 \"quoted\" <b>bold</b> \\ back")
    );
    assert_eq!(cafe.version(), None);
    let bad = list.find(&slug("badutf")).found().unwrap();
    assert_eq!((bad.version(), bad.title()), (Some("1.0"), None));
    // A folder with a space: listed, but no slug can name it.
    let spaced = list.find_file("My Plugin/my.php").found().unwrap();
    assert_eq!((spaced.name(), spaced.version()), ("My Plugin", Some("2")));
    assert!(Slug::new("My Plugin").is_err());
}

#[test]
fn rows_without_a_name_are_counted_never_matched() {
    let list = list(fixture!("plugin-list-unnamed.json")).unwrap().value;
    assert_eq!(list.unnamed(), 2);
    assert_eq!(list.rows().len(), 6);
    assert!(
        list.rows()
            .iter()
            .all(|row| !row.title().unwrap_or("").starts_with("Bad"))
    );
    // Their folders are unknown, so WP-CLI's fetcher may take one for any
    // slug (replaces `Found(hello)`: undetermined, see item 2 of the
    // audit).
    for name in ["hello", "akismet", "anything"] {
        assert_eq!(
            list.find(&slug(name)),
            Lookup::Unreadable { count: 2 },
            "{name}"
        );
        assert!(list.resolve(&slug(name)).is_none());
    }
}

#[test]
fn the_php85_deprecation_line_before_the_payload_is_skipped_and_reported() {
    // Real: `cli version` under PHP 8.5 with display_errors on.
    let parsed = cli_version(&ok(fixture!("cli-version-php85.txt"))).unwrap();
    assert_eq!(parsed.value.to_string(), "2.12.0");
    assert_eq!(parsed.notices.total(), 1);
    assert_eq!(parsed.notices.lines(), [PHP85_DEPRECATION]);
    // Synthetic: the same line in front of a real list.
    let mut stdout = format!("\n{PHP85_DEPRECATION}\n").into_bytes();
    stdout.extend_from_slice(fixture!("plugin-list-single.json"));
    let parsed = list(&stdout).unwrap();
    assert_eq!(parsed.value.rows().len(), 4);
    assert_eq!(parsed.notices.lines(), [PHP85_DEPRECATION]);
}

#[test]
fn notices_after_the_payload_are_skipped_and_reported() {
    let parsed = list(fixture!("plugin-list-shutdown-warning.txt")).unwrap();
    // The site's four rows, plus the must-use plugin that printed it.
    assert_eq!(parsed.value.rows().len(), 5);
    // A must-use file may hide a regular one of the same name.
    let Lookup::Shadowed { by } = parsed.value.find(&slug("zz-noise")) else {
        panic!("a must-use zz-noise.php");
    };
    assert_eq!(by.status(), &PluginStatus::MustUse);
    assert_eq!(
        parsed.notices.lines(),
        ["Warning: apvm shutdown warning in \
          /var/www/single/wp-content/mu-plugins/zz-noise.php on line 2"]
    );
}

#[test]
fn every_php_diagnostic_form_is_a_notice() {
    let payload = format!("[{}]", json_row("hello", "active", "hello.php"));
    for notice in [
        "Deprecated: x in /a.php on line 1",
        "PHP Deprecated:  x in /a.php on line 1",
        "Notice: x",
        "PHP Notice:  x",
        "Warning: x",
        "PHP Warning:  x",
        "Strict Standards: x",
        "Recoverable fatal error: x",
        "Parse error: x",
        "Unknown error: x",
        "[13-Oct-2026 01:25:54 UTC] PHP Warning:  apvm load-time warning in /x.php on line 2",
        "[13-Oct-2026 01:25:54 Europe/Berlin] PHP Deprecated:  x",
    ] {
        for stdout in [
            format!("{notice}\n{payload}"),
            format!("{payload}\n{notice}\n"),
            format!("\n{notice}\n\n{payload}\n\n{notice}\n"),
        ] {
            let parsed = list(stdout.as_bytes()).unwrap();
            assert!(parsed.notices.total() >= 1, "{stdout:?}");
            assert_eq!(parsed.value.rows().len(), 1, "{stdout:?}");
        }
    }
    // Labels match exactly: these are content, not notices.
    for line in [
        "Deprecated x",
        "Warnings: x",
        "warning: x",
        "PHP  Warning: x",
    ] {
        let stdout = format!("{line}\n{payload}");
        assert!(
            matches!(
                list_defect(stdout.as_bytes()),
                OutputDefect::UnexpectedLine { line: 1, .. }
            ),
            "{line}"
        );
    }
}

#[test]
fn a_fatal_error_trace_after_the_payload_is_skipped() {
    let stdout = format!(
        "[{}]\nPHP Fatal error:  Uncaught RuntimeException: apvm-e2e in /x.php:2\nStack trace:\n\
         #0 /var/www/single/wp-settings.php(508): include_once()\n#1 {{main}}\n  thrown in /x.php on line 2\n",
        json_row("hello", "active", "hello.php")
    );
    let parsed = list(stdout.as_bytes()).unwrap();
    assert_eq!(parsed.notices.total(), 5);
    // A trace line on its own is not a notice.
    let stray = format!("#0 nothing\n[{}]", json_row("hello", "active", "hello.php"));
    assert_eq!(
        list_defect(stray.as_bytes()),
        OutputDefect::UnexpectedLine {
            line: 1,
            excerpt: "#0 nothing".into()
        }
    );
}

#[test]
fn notices_keep_a_few_excerpts_and_count_them_all() {
    let mut stdout = String::new();
    for i in 0..20 {
        stdout.push_str(&format!("Notice: n{i}\n"));
    }
    stdout.push_str("[]");
    let notices = list(stdout.as_bytes()).unwrap().notices;
    assert_eq!(notices.total(), 20);
    assert_eq!(notices.lines().len(), NOTICE_LINES);
    assert_eq!(notices.lines()[0], "Notice: n0");
    // Excerpts are escaped and cut.
    let long = format!("Warning: \u{1b}[2J{}\n[]", "x".repeat(400));
    let notices = list(long.as_bytes()).unwrap().notices;
    assert!(notices.lines()[0].starts_with("Warning: \\u{1b}[2J"));
    assert!(notices.lines()[0].ends_with('…'));
    // The escape adds characters to the 240 kept.
    assert_eq!(notices.lines()[0].chars().count(), EXCERPT_CHARS + 1 + 5);
}

#[test]
fn only_notices_is_not_an_empty_list() {
    let stdout = format!("\n{PHP85_DEPRECATION}\n");
    assert_eq!(
        list_defect(stdout.as_bytes()),
        OutputDefect::OnlyNotices {
            excerpt: PHP85_DEPRECATION.into()
        }
    );
}

#[test]
fn html_pages_are_refused() {
    let OutputDefect::Html { excerpt } = list_defect(fixture!("maintenance-page.html.txt")) else {
        panic!("a maintenance page is HTML");
    };
    assert_eq!(excerpt, "<!DOCTYPE html>");
    let mut mixed = b"  <br />\n".to_vec();
    mixed.extend_from_slice(fixture!("plugin-list-single.json"));
    assert!(matches!(list_defect(&mixed), OutputDefect::Html { .. }));
}

#[test]
fn truncated_json_is_refused() {
    let full = fixture!("plugin-list-single.json");
    for cut in [1, 2, 40, full.len() / 2, full.len() - 1] {
        let defect = list_defect(&full[..cut]);
        assert!(
            matches!(defect, OutputDefect::Truncated { .. }),
            "cut at {cut}: {defect:?}"
        );
    }
    let OutputDefect::Truncated { excerpt } = list_defect(&full[..full.len() - 1]) else {
        panic!("truncated");
    };
    assert!(excerpt.starts_with('…'), "{excerpt}");
    assert!(excerpt.ends_with("\\\"title\\\":\\\"\\\"}"), "{excerpt}");
}

#[test]
fn trailing_output_on_the_payload_line_is_refused() {
    let mut stdout = fixture!("plugin-list-single.json").to_vec();
    stdout.extend_from_slice(b"<!-- page cache -->");
    let OutputDefect::InvalidJson { reason, .. } = list_defect(&stdout) else {
        panic!("trailing characters");
    };
    assert!(reason.starts_with("trailing characters"), "{reason}");
}

#[test]
fn unexpected_lines_and_several_payloads_are_refused() {
    let row = format!("[{}]", json_row("hello", "active", "hello.php"));
    let unexpected = |line: usize, excerpt: &str| OutputDefect::UnexpectedLine {
        line,
        excerpt: excerpt.into(),
    };
    let cases = [
        (format!("hello\n{row}"), unexpected(1, "hello")),
        (
            format!("{row}\n\nError: boom"),
            unexpected(3, "Error: boom"),
        ),
        (
            format!("{row}\n{row}"),
            OutputDefect::SeveralPayloads { count: 2 },
        ),
        (
            format!("{row}\n{{}}"),
            OutputDefect::SeveralPayloads { count: 2 },
        ),
        ("Success: done".to_string(), unexpected(1, "Success: done")),
    ];
    for (stdout, expected) in cases {
        assert_eq!(list_defect(stdout.as_bytes()), expected, "{stdout:?}");
    }
    // Not a log timestamp: a JSON line never loses its start.
    let OutputDefect::InvalidJson { reason, .. } = list_defect(br#"[{"a] PHP Warning: x":1}]"#)
    else {
        panic!("a JSON line");
    };
    assert!(
        reason.starts_with("unknown field `a] PHP Warning: x`"),
        "{reason}"
    );
}

#[test]
fn a_failed_command_is_refused_whatever_stdout_holds() {
    let output = CommandOutput::new(1, b"", fixture!("fatal-must-use.stderr.txt"));
    assert_eq!(
        defect_of(plugin_list(&output)),
        OutputDefect::Failed {
            exit_code: 1,
            message: Some(
                "Error: There has been a critical error on this website.Learn more about \
                 troubleshooting WordPress. There has been a critical error on this website."
                    .into()
            ),
        }
    );
    let complete = CommandOutput::new(254, fixture!("plugin-list-single.json"), b"");
    assert_eq!(
        defect_of(plugin_list(&complete)),
        OutputDefect::Failed {
            exit_code: 254,
            message: None
        }
    );
    let wp_die = CommandOutput::new(1, b"", fixture!("wp-die.stderr.txt"));
    assert_eq!(
        plugin_list(&wp_die).unwrap_err().to_string(),
        "'wp plugin list' exited with status 1: 'Error: Site suspended. Contact your host.'"
    );
}

#[test]
fn unknown_statuses_are_kept_explicitly() {
    let stdout = format!(
        "[{},{},{}]",
        json_row("hello", "paused", "hello\\/hello.php"),
        json_row("akismet", "active", "akismet\\/akismet.php"),
        json_row("odd", "paused", "odd.php")
    );
    let list = list(stdout.as_bytes()).unwrap().value;
    let hello = list.find(&slug("hello")).found().unwrap();
    assert_eq!(hello.status(), &PluginStatus::Unknown("paused".into()));
    assert!(!hello.status().is_known() && !hello.status().is_regular());
    assert_eq!(list.regular().count(), 1);
    // At a top-level file, an unknown status may hide a regular plugin.
    assert!(matches!(list.find(&slug("odd")), Lookup::Shadowed { .. }));
    // The status of a row without a name is checked too.
    let unnamed = r#"[{"name":null,"status":"","version":"","file":null,"title":""}]"#;
    assert_eq!(
        list_defect(unnamed.as_bytes()),
        OutputDefect::InvalidRow {
            index: 0,
            reason: "has an empty status".into()
        }
    );
    let empty = format!("[{}]", json_row("hello", "", "hello.php"));
    assert_eq!(
        list_defect(empty.as_bytes()),
        OutputDefect::InvalidRow {
            index: 0,
            reason: "has an empty status".into()
        }
    );
}

/// A valid row's fields, as raw JSON values.
const ROW_FIELDS: [(&str, &str); 5] = [
    ("name", "\"hello\""),
    ("status", "\"active\""),
    ("version", "\"1.0\""),
    ("file", "\"hello.php\""),
    ("title", "\"T\""),
];

/// A one-row list of `fields` (raw JSON values), in order.
///
/// # Arguments
///
/// * `fields` - The keys and raw values
fn raw_list(fields: &[(&str, &str)]) -> String {
    let fields: Vec<String> = fields.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!("[{{{}}}]", fields.join(","))
}

/// [`ROW_FIELDS`] with `field` set to `value`, or removed when `None`.
///
/// # Arguments
///
/// * `field` - The key
/// * `value` - Its raw JSON value
fn row_with<'a>(field: &str, value: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    ROW_FIELDS
        .iter()
        .filter_map(|(k, v)| match (*k == field, value) {
            (false, _) => Some((*k, *v)),
            (true, Some(value)) => Some((*k, value)),
            (true, None) => None,
        })
        .collect()
}

#[test]
fn fields_must_be_present_typed_and_known() {
    let mut extra = ROW_FIELDS.to_vec();
    extra.push(("author", "\"x\""));
    let mut twice = ROW_FIELDS.to_vec();
    twice.push(("name", "\"other\""));
    let cases = [
        (row_with("name", None), "missing field `name`"),
        (row_with("version", None), "missing field `version`"),
        (row_with("title", None), "missing field `title`"),
        (
            row_with("status", Some("null")),
            "invalid type: null, expected a string",
        ),
        (
            row_with("version", Some("2")),
            "invalid type: integer `2`, expected a string",
        ),
        (
            row_with("version", Some("1.10")),
            "invalid type: floating point `1.1`, expected a string",
        ),
        (
            row_with("title", Some("true")),
            "invalid type: boolean `true`, expected a string",
        ),
        (
            row_with("name", Some("[\"hello\"]")),
            "invalid type: sequence, expected a string",
        ),
        (extra, "unknown field `author`"),
        (twice, "duplicate field `name`"),
    ];
    for (fields, reason) in cases {
        let stdout = raw_list(&fields);
        let OutputDefect::InvalidJson { reason: got, .. } = list_defect(stdout.as_bytes()) else {
            panic!("{stdout}");
        };
        assert!(got.starts_with(reason), "{stdout}: {got}");
    }
    // Versions stay text: "1.10" is not "1.1".
    let stdout = raw_list(&row_with("version", Some("\"1.10\"")));
    let parsed = list(stdout.as_bytes()).unwrap().value;
    assert_eq!(
        parsed.find(&slug("hello")).found().unwrap().version(),
        Some("1.10")
    );
    // `null` reads as none where WP-CLI prints it.
    let stdout = raw_list(&row_with("version", Some("null")));
    let parsed = list(stdout.as_bytes()).unwrap().value;
    assert_eq!(parsed.find(&slug("hello")).found().unwrap().version(), None);
}

#[test]
fn a_payload_of_another_shape_is_refused() {
    for (stdout, reason) in [
        ("{}", "expected a JSON array"),
        ("{\"a\":1}", "expected a JSON array"),
        ("[{}]", "missing field `name`"),
        ("[{\"name\":\"\\ud800\"}]", "unexpected end of hex escape"),
        ("[{\"name\":\"\\ud800\\u0041\"}]", "lone leading surrogate"),
    ] {
        let OutputDefect::InvalidJson { reason: got, .. } = list_defect(stdout.as_bytes()) else {
            panic!("{stdout}");
        };
        assert!(got.starts_with(reason), "{stdout}: {got}");
    }
    // Lines that open with a bracket but not as compact JSON prints a
    // payload are content (this replaces their earlier `InvalidJson`
    // verdicts: `[info] …` lines counted as payloads). Positional records
    // (`[[…]]`, which serde would read by position) stay refused.
    for stdout in [
        "[1]",
        "[null]",
        "[[]]",
        "[\"a\"]",
        "[[\"hello\",\"active\",\"1.0\",\"hello.php\",\"T\"]]",
        "[info] cache warmed",
        "{ \"name\": 1 }",
    ] {
        assert_eq!(
            list_defect(stdout.as_bytes()),
            OutputDefect::UnexpectedLine {
                line: 1,
                excerpt: stdout.escape_debug().to_string()
            },
            "{stdout}"
        );
    }
    let not_utf8 = b"[{\"name\":\"a\xff\"}]\n";
    assert!(matches!(
        list_defect(not_utf8),
        OutputDefect::InvalidJson { .. }
    ));
}

#[test]
fn bracketed_log_lines_are_never_payloads() {
    let payload = format!("[{}]", json_row("hello", "active", "hello.php"));
    // Xdebug's log form with timestamps is a notice block.
    let stdout = format!(
        "[13-Oct-2026 01:25:54 UTC] PHP Warning:  apvm in /x.php on line 2\n\
         [13-Oct-2026 01:25:54 UTC] PHP Stack trace:\n\
         [13-Oct-2026 01:25:54 UTC] PHP   1. {{main}}() /x.php:0\n{payload}"
    );
    let parsed = list(stdout.as_bytes()).unwrap();
    assert_eq!(parsed.notices.total(), 3);
    // An `[info]` line next to the payload is one unexpected line, not a
    // second payload.
    let stdout = format!("[info] x\n{payload}\n[debug] y");
    assert_eq!(
        list_defect(stdout.as_bytes()),
        OutputDefect::UnexpectedLine {
            line: 1,
            excerpt: "[info] x".into()
        }
    );
    // A payload cut after its first byte, or after `[{`, is still a
    // truncated payload, never a notice or other content.
    for cut in ["[", "[{", "[{\"name\""] {
        let defect = list_defect(cut.as_bytes());
        assert!(
            matches!(defect, OutputDefect::Truncated { .. }),
            "{cut}: {defect:?}"
        );
        let after_notice = format!("Warning: x in /a.php on line 1\n{cut}");
        assert!(matches!(
            list_defect(after_notice.as_bytes()),
            OutputDefect::Truncated { .. }
        ));
        let in_message = format!("Warning: a\n{cut}");
        assert!(matches!(
            list_defect(in_message.as_bytes()),
            OutputDefect::Truncated { .. }
        ));
    }
    for cut in ["{", "{\"wp_cli_version\""] {
        assert!(
            matches!(
                defect_of(site_facts(&ok(cut.as_bytes()))),
                OutputDefect::Truncated { .. }
            ),
            "{cut}"
        );
    }
}

#[test]
fn rows_whose_name_does_not_match_their_file_are_refused() {
    let stdout = format!(
        "[{},{}]",
        json_row("hello", "active", "hello.php"),
        json_row("akismet", "inactive", "other\\/akismet.php")
    );
    assert_eq!(
        list_defect(stdout.as_bytes()),
        OutputDefect::InvalidRow {
            index: 1,
            reason: "has a name 'akismet' that does not match its file 'other/akismet.php' \
                     for status inactive"
                .into()
        }
    );
    let twice = format!(
        "[{},{}]",
        json_row("hello", "active", "hello.php"),
        json_row("hello", "inactive", "hello.php")
    );
    assert!(matches!(
        list_defect(twice.as_bytes()),
        OutputDefect::InvalidList { .. }
    ));
}

#[test]
fn duplicate_names_parse_and_look_up_as_ambiguous() {
    let stdout = format!(
        "[{},{}]",
        json_row("foo", "inactive", "foo\\/a.php"),
        json_row("foo", "active", "foo\\/b.php")
    );
    let list = list(stdout.as_bytes()).unwrap().value;
    assert!(matches!(list.find(&slug("foo")), Lookup::Ambiguous(rows) if rows.len() == 2));
}

#[test]
fn byte_order_marks_and_crlf_are_tolerated_and_reported() {
    let mut stdout = b"\xEF\xBB\xBF".to_vec();
    stdout.extend_from_slice(fixture!("plugin-list-single.json"));
    stdout.extend_from_slice(b"\r\n");
    let parsed = list(&stdout).unwrap();
    assert_eq!(parsed.value.rows().len(), 4);
    assert_eq!(parsed.notices.lines(), ["UTF-8 byte-order mark"]);
    let crlf = b"Notice: x\r\n[]\r\n";
    assert_eq!(list(crlf).unwrap().notices.lines(), ["Notice: x"]);
}

#[test]
fn very_large_lists_parse() {
    let rows: Vec<String> = (0..50_000)
        .map(|i| json_row(&format!("p{i}"), "inactive", &format!("p{i}\\/p{i}.php")))
        .collect();
    let stdout = format!("[{}]", rows.join(","));
    let list = list(stdout.as_bytes()).unwrap().value;
    assert_eq!(list.rows().len(), 50_000);
    assert_eq!(
        list.find(&slug("p49999")).found().unwrap().file(),
        "p49999/p49999.php"
    );
}

#[test]
fn cli_version_reads_one_line() {
    let parsed = cli_version(&ok(fixture!("cli-version.txt"))).unwrap();
    assert_eq!(
        parsed.value,
        WpCliVersion {
            major: 2,
            minor: 12,
            patch: 0,
            suffix: None
        }
    );
    assert!(parsed.notices.is_empty());
    let invalid = |excerpt: &str| OutputDefect::InvalidVersion {
        excerpt: excerpt.into(),
    };
    for (stdout, expected) in [
        (&b"WP-CLI 2.12\n"[..], invalid("WP-CLI 2.12")),
        (b"WP-CLI 2.12.0 extra\n", invalid("WP-CLI 2.12.0 extra")),
        (
            b"WP-CLI 2.12.0\nWP-CLI 2.12.0\n",
            OutputDefect::SeveralPayloads { count: 2 },
        ),
        (
            b"PHP 8.4.26 (cli)\n",
            OutputDefect::UnexpectedLine {
                line: 1,
                excerpt: "PHP 8.4.26 (cli)".into(),
            },
        ),
        (b"", OutputDefect::Empty),
        (
            b"<html>\n",
            OutputDefect::Html {
                excerpt: "<html>".into(),
            },
        ),
    ] {
        assert_eq!(defect_of(cli_version(&ok(stdout))), expected, "{stdout:?}");
    }
}

#[test]
fn wp_cli_versions() {
    for (text, shown) in [
        ("2.12.0", "2.12.0"),
        ("0.0.0", "0.0.0"),
        ("2.13.0-alpha", "2.13.0-alpha"),
        ("2.13.0-alpha-1a2b3c4", "2.13.0-alpha-1a2b3c4"),
        ("123456789.1.1", "123456789.1.1"),
    ] {
        assert_eq!(WpCliVersion::parse(text).unwrap().to_string(), shown);
    }
    let alpha = WpCliVersion::parse("2.13.0-alpha-1a2b3c4").unwrap();
    assert_eq!(alpha.suffix.as_deref(), Some("alpha-1a2b3c4"));
    let too_long = format!("2.12.0-{}", "a".repeat(65));
    for bad in [
        "",
        "2",
        "2.12",
        "2.12.0.1",
        "v2.12.0",
        "2.12.0-",
        "2.12.0-a b",
        "2.12.0-é",
        "+2.12.0",
        "2..0",
        "1234567890.0.0",
        "２.12.0",
        &too_long,
    ] {
        assert_eq!(WpCliVersion::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn site_facts_read_the_snippet_output() {
    let single = site_facts(&ok(fixture!("site-facts-single.json"))).unwrap();
    assert!(single.notices.is_empty());
    let facts = single.value;
    assert_eq!(facts.wp_cli_version.to_string(), "2.12.0");
    assert_eq!(
        (facts.php_version.as_str(), facts.wp_version.as_str()),
        ("8.4.26", "7.1.3")
    );
    assert!(!facts.multisite && !facts.disallow_file_mods);
    assert_eq!(facts.plugin_dir, "/var/www/single/wp-content/plugins");
    assert_eq!(
        facts.plugin_dir_realpath.as_deref(),
        Some("/var/www/single/wp-content/plugins")
    );
    assert_eq!(facts.content_dir, "/var/www/single/wp-content");
    assert_eq!(
        (facts.siteurl.as_str(), facts.home.as_str()),
        ("http://single.test", "http://single.test")
    );
    let multi = site_facts(&ok(fixture!("site-facts-multisite.json"))).unwrap();
    assert!(multi.value.multisite);
    let locked = site_facts(&ok(fixture!("site-facts-file-mods-disallowed.json"))).unwrap();
    assert!(locked.value.disallow_file_mods);
}

#[test]
fn site_facts_are_observations() {
    // Real: a working site with a non-canonical WP_CONTENT_DIR
    // (`/var/www/single/../single//wp-content`); the realpaths are
    // canonical.
    let facts = site_facts(&ok(fixture!("site-facts-non-canonical.json")))
        .unwrap()
        .value;
    assert_eq!(facts.content_dir, "/var/www/single/../single//wp-content");
    assert_eq!(
        facts.content_dir_realpath.as_deref(),
        Some("/var/www/single/wp-content")
    );
    assert_eq!(
        facts.plugin_dir_realpath.as_deref(),
        Some("/var/www/single/wp-content/plugins")
    );
    // Synthetic: values the earlier §7 rules refused, all accepted now.
    for (key, value) in [
        ("plugin_dir", r#""/var/www/html/../content/plugins""#),
        ("plugin_dir", r#""/var/www//plugins/""#),
        ("content_dir", r#""/var/www/html/./wp-content/""#),
        ("content_dir_realpath", r#""/srv/a b/$x/wp-content""#),
        ("siteurl", r#""https://bücher.de""#),
        ("siteurl", r#""http://wp_app:8080""#),
        ("siteurl", r#""HTTPS://Example.COM/a+b/ü""#),
        ("home", r#""https://example.com/?lang=de""#),
        ("home", r#""//example.com""#),
    ] {
        let stdout = facts_with(key, Some(value));
        assert!(
            site_facts(&ok(stdout.as_bytes())).is_ok(),
            "{key} = {value}"
        );
    }
    for key in ["plugin_dir_realpath", "content_dir_realpath"] {
        let stdout = facts_with(key, Some("null"));
        assert!(site_facts(&ok(stdout.as_bytes())).is_ok(), "{key}");
    }
}

#[test]
fn site_facts_are_strict() {
    let realpath_unknown = facts_with("plugin_dir_realpath", Some("null"));
    let facts = site_facts(&ok(realpath_unknown.as_bytes())).unwrap().value;
    assert_eq!(facts.plugin_dir_realpath, None);
    // Each refused value, and how the reason starts.
    let long_path = format!("\"/{}\"", "a".repeat(4096));
    let long_url = format!("\"http://{}\"", "a".repeat(2048));
    let refused = [
        ("wp_cli_version", r#""2.12""#, "is not a WP-CLI version"),
        ("php_version", r#"" ""#, "is empty"),
        ("wp_version", r#""""#, "is empty"),
        ("wp_version", r#""7.1\u001b[2J""#, "contains U+001B"),
        (
            "wp_version",
            &format!("\"{}\"", "9".repeat(65)),
            "must be at most 64 characters",
        ),
        (
            "plugin_dir",
            r#""wp-content/plugins""#,
            "is not an absolute path",
        ),
        ("plugin_dir", r#""/var/www/a\u0000b""#, "contains U+0000"),
        ("plugin_dir", r#""/var/www/a\nb""#, "contains U+000A"),
        ("plugin_dir", r#""/var/www/a\u0085b""#, "contains U+0085"),
        ("plugin_dir", &long_path, "must be at most 4096 characters"),
        ("plugin_dir_realpath", r#""""#, "is empty"),
        (
            "plugin_dir_realpath",
            r#""/x/\u001b]0;evil""#,
            "contains U+001B",
        ),
        ("content_dir", r#""C:\\wp""#, "is not an absolute path"),
        (
            "content_dir_realpath",
            r#""/x/\u202egnp.x""#,
            "contains U+202E",
        ),
        ("siteurl", r#""""#, "is empty"),
        ("siteurl", r#""http://x\u001b[2J""#, "contains U+001B"),
        ("siteurl", r#""http://x/\u200b""#, "contains U+200B"),
        ("siteurl", r#""http://x/\u007f""#, "contains U+007F"),
        ("siteurl", &long_url, "must be at most 2048 characters"),
        ("home", r#""http://a\u2028b""#, "contains U+2028"),
    ];
    for (key, value, reason) in refused {
        let stdout = facts_with(key, Some(value));
        let defect = defect_of(site_facts(&ok(stdout.as_bytes())));
        let OutputDefect::InvalidField { field, reason: got } = &defect else {
            panic!("{key} = {value}: {defect:?}");
        };
        assert_eq!(*field, key, "{value}");
        assert!(got.starts_with(reason), "{key} = {value}: {got}");
    }
    for (key, value) in [
        ("plugin_dir", "null"),
        ("siteurl", "false"),
        ("multisite", "1"),
        ("disallow_file_mods", "\"1\""),
        ("extra", "1"),
    ] {
        let stdout = facts_with(key, Some(value));
        let defect = defect_of(site_facts(&ok(stdout.as_bytes())));
        assert!(
            matches!(defect, OutputDefect::InvalidJson { .. }),
            "{key}: {defect}"
        );
    }
    // Every key the snippet prints is required.
    for key in SITE_FACTS_KEYS {
        let stdout = facts_with(key, None);
        let OutputDefect::InvalidJson { reason, .. } =
            defect_of(site_facts(&ok(stdout.as_bytes())))
        else {
            panic!("{key}");
        };
        assert!(
            reason.starts_with(&format!("missing field `{key}`")),
            "{reason}"
        );
    }
    assert_eq!(
        defect_of(site_facts(&ok(b"[]"))),
        OutputDefect::InvalidJson {
            reason: "expected a JSON object".into(),
            excerpt: "[]".into()
        }
    );
    let error = site_facts(&ok(facts_with("home", Some(r#""x""#)).as_bytes()));
    assert!(error.is_ok(), "a URL is not checked as one: {error:?}");
    let error = site_facts(&ok(facts_with("home", Some(r#""""#)).as_bytes())).unwrap_err();
    assert_eq!(
        error.to_string(),
        "'wp eval' printed a field 'home' that is empty"
    );
}

/// The site's `siteurl` in the boot fixtures.
const SITEURL: &str = "http://single.test";

/// A boot that printed the expected siteurl with no notices.
fn clean_boot() -> BootCheck {
    BootCheck::Boots {
        notices: Notices::default(),
        siteurl_changed: None,
    }
}

#[test]
fn boot_checks_need_exit_0_and_one_url_line() {
    // Real: a clean boot.
    let ok_boot = boot_check(&ok(fixture!("boot-check-ok.txt")), SITEURL);
    assert_eq!(ok_boot, clean_boot());
    assert!(ok_boot.boots());
    // Real: a plugin printing a page and exiting on `init` (exit 0).
    let html = boot_check(&ok(fixture!("boot-check-html-exit.txt")), SITEURL);
    assert_eq!(
        html,
        BootCheck::NoBoot {
            defect: OutputDefect::Html {
                excerpt: "<!DOCTYPE html>".into()
            }
        }
    );
    assert!(!html.boots());
    // Real: a plugin calling die() on `plugins_loaded` (exit 0, nothing).
    let died = boot_check(&ok(fixture!("boot-check-die.txt")), SITEURL);
    assert_eq!(
        died,
        BootCheck::NoBoot {
            defect: OutputDefect::Empty
        }
    );
    // Real: the maintenance page fixture.
    assert!(matches!(
        boot_check(&ok(fixture!("maintenance-page.html.txt")), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::Html { .. }
        }
    ));
    // Synthetic: another line next to the URL, two URLs, a bad URL line.
    assert!(matches!(
        boot_check(&ok(b"http://single.test\nbye\n"), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::UnexpectedLine { line: 2, .. }
        }
    ));
    assert_eq!(
        boot_check(&ok(b"http://single.test\nhttp://single.test\n"), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::SeveralPayloads { count: 2 }
        }
    );
    for bad in [
        &b"http://x\x1b[2J\n"[..],
        b"https://x/\xe2\x80\xae\n",
        b"http://\xff\n",
    ] {
        assert!(
            matches!(
                boot_check(&ok(bad), SITEURL),
                BootCheck::NoBoot {
                    defect: OutputDefect::InvalidField {
                        field: "siteurl",
                        ..
                    }
                }
            ),
            "{bad:?}"
        );
    }
    let long = format!("http://{}\n", "a".repeat(2050));
    assert!(matches!(
        boot_check(&ok(long.as_bytes()), SITEURL),
        BootCheck::NoBoot { .. }
    ));
    // Notices around it are skipped and reported.
    let noisy = format!("\n{PHP85_DEPRECATION}\nhttp://single.test\n");
    let BootCheck::Boots { notices, .. } = boot_check(&ok(noisy.as_bytes()), SITEURL) else {
        panic!("a notice does not stop a boot");
    };
    assert_eq!(notices.lines(), [PHP85_DEPRECATION]);
    // A non-zero exit fails, whatever stdout holds.
    let fatal = CommandOutput::new(1, b"", fixture!("fatal-must-use.stderr.txt"));
    let BootCheck::Fails { exit_code, message } = boot_check(&fatal, SITEURL) else {
        panic!("a fatal error fails the boot check");
    };
    assert_eq!(exit_code, 1);
    assert!(
        message
            .unwrap()
            .starts_with("Error: There has been a critical error")
    );
    assert_eq!(
        boot_check(
            &CommandOutput::new(254, b"http://single.test\n", b""),
            SITEURL
        ),
        BootCheck::Fails {
            exit_code: 254,
            message: None
        }
    );
}

#[test]
fn a_changed_siteurl_still_boots_with_a_warning() {
    // Replaces the earlier rule that a different URL failed the check:
    // force-HTTPS, domain mapping or per-language plugins filter the
    // option, and WordPress did boot.
    for (printed, changed) in [
        ("https://single.test", Some("https://single.test")),
        ("http://single.test/", Some("http://single.test/")),
        ("HTTPS://SINGLE.TEST", Some("HTTPS://SINGLE.TEST")),
        ("https://bücher.de/wp", Some("https://bücher.de/wp")),
        ("http://wp_app:8080", Some("http://wp_app:8080")),
        ("http://single.test", None),
    ] {
        let stdout = format!("{printed}\n");
        assert_eq!(
            boot_check(&ok(stdout.as_bytes()), SITEURL),
            BootCheck::Boots {
                notices: Notices::default(),
                siteurl_changed: changed.map(str::to_string),
            },
            "{printed}"
        );
    }
    // Not a URL line at all: no boot.
    assert!(matches!(
        boot_check(&ok(b"ftp://single.test\n"), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::UnexpectedLine { .. }
        }
    ));
}

#[test]
fn requires_plugins_reads_the_header_as_wordpress_does() {
    let child = requires_plugins(
        &ok(fixture!("requires-plugins-child.json")),
        &resolved(&listed("child"), "child"),
    )
    .unwrap()
    .value;
    assert_eq!(
        child,
        RequiresPlugins {
            raw: "parent".into(),
            slugs: vec![slug("parent")],
            ignored: vec![],
        }
    );
    let none = requires_plugins(
        &ok(fixture!("requires-plugins-none.json")),
        &resolved(&listed("parent"), "parent"),
    )
    .unwrap()
    .value;
    assert!(none.slugs.is_empty() && none.ignored.is_empty() && none.raw.is_empty());
    // Real: "Child , parent,  ,BAD SLUG!, parent". WordPress keeps only
    // lowercase slugs, trimmed, once each.
    let messy = requires_plugins(
        &ok(fixture!("requires-plugins-messy.json")),
        &resolved(&listed("multi-req"), "multi-req"),
    )
    .unwrap()
    .value;
    assert_eq!(messy.slugs, [slug("parent")]);
    assert_eq!(messy.ignored, ["Child", "BAD SLUG!"]);
    // Synthetic: WordPress's format, in byte order.
    let stdout = br#"{"name":"x","requires_plugins":"wp-rocket,\tb2,a-b-c ,a--b,-a,a-,10,9,x_y,\u00e9,a.b"}"#;
    let parsed = requires_plugins(&ok(stdout), &resolved(&listed("x"), "x"))
        .unwrap()
        .value;
    let slugs: Vec<&str> = parsed.slugs.iter().map(Slug::as_str).collect();
    assert_eq!(slugs, ["10", "9", "a-b-c", "b2", "wp-rocket"]);
    assert_eq!(parsed.ignored, ["a--b", "-a", "a-", "x_y", "é", "a.b"]);
}

#[test]
fn requires_plugins_expects_wp_clis_name_not_the_slug() {
    // Hello Dolly's file is `hello.php`, so the fetcher takes slug
    // `hello.php` while `plugin get` prints `get_plugin_name()`: `hello`.
    let row = PluginRow::new(
        "hello".into(),
        PluginStatus::Inactive,
        None,
        "hello.php".into(),
        None,
    )
    .unwrap();
    let list = PluginList::new(vec![row], vec![]).unwrap();
    let target = resolved(&list, "hello.php");
    let named = br#"{"name":"hello","requires_plugins":""}"#;
    assert!(requires_plugins(&ok(named), &target).is_ok());
    let by_slug = br#"{"name":"hello.php","requires_plugins":""}"#;
    assert!(matches!(
        defect_of(requires_plugins(&ok(by_slug), &target)),
        OutputDefect::InvalidField { .. }
    ));
}

#[test]
fn requires_plugins_is_strict() {
    let list = listed("x");
    let x = resolved(&list, "x");
    let other = br#"{"name":"y","requires_plugins":""}"#;
    assert_eq!(
        defect_of(requires_plugins(&ok(other), &x)),
        OutputDefect::InvalidField {
            field: "name",
            reason: "is 'y' instead of 'x'".into()
        }
    );
    // WP-CLI never prints `null` there (replaces the earlier model of a
    // `null` header): it prints an empty line instead (real fixture).
    assert_eq!(
        defect_of(requires_plugins(
            &ok(fixture!("requires-plugins-not-utf8.json")),
            &x
        )),
        OutputDefect::Unencodable
    );
    assert_eq!(
        requires_plugins(&ok(b"\n"), &x).unwrap_err().to_string(),
        "'wp plugin get' printed an empty line instead of JSON: a value is not valid UTF-8"
    );
    // Only an empty line is that; nothing at all is `Empty`.
    assert_eq!(
        defect_of(requires_plugins(&ok(b""), &x)),
        OutputDefect::Empty
    );
    for stdout in [
        &br#"{"name":"x","requires_plugins":null}"#[..],
        br#"{"name":"x"}"#,
        br#"{"name":"x","requires_plugins":"","title":"T"}"#,
        br#"{"name":"x","requires_plugins":1}"#,
        br#"[{"name":"x","requires_plugins":""}]"#,
    ] {
        assert!(
            matches!(
                defect_of(requires_plugins(&ok(stdout), &x)),
                OutputDefect::InvalidJson { .. }
            ),
            "{}",
            String::from_utf8_lossy(stdout)
        );
    }
    // Not compact JSON: content.
    assert!(matches!(
        defect_of(requires_plugins(&ok(br#"["x",""]"#), &x)),
        OutputDefect::UnexpectedLine { line: 1, .. }
    ));
    // Real: a plugin that is not installed.
    let missing = CommandOutput::new(1, b"", fixture!("plugin-get-missing.stderr.txt"));
    let error = requires_plugins(&missing, &resolved(&listed("nosuch"), "nosuch")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "'wp plugin get' exited with status 1: 'Error: The \\'nosuch\\' plugin could not be found.'"
    );
}

#[test]
fn stderr_messages_for_reports() {
    let exists = CommandOutput::new(1, b"", fixture!("install-exists.stderr.txt"));
    assert_eq!(exists.error_lines(), ["Error: No plugins installed."]);
    assert_eq!(
        exists.message().as_deref(),
        Some("Error: No plugins installed.")
    );
    let network = CommandOutput::new(1, b"", fixture!("deactivate-network-active.stderr.txt"));
    assert_eq!(network.error_lines(), ["Error: No plugins deactivated."]);
    // Without an `Error:` line, the last non-blank line explains it.
    let plain = CommandOutput::new(1, b"", b"first\nlast words\n\n");
    assert_eq!(plain.message().as_deref(), Some("last words"));
    assert!(plain.error_lines().is_empty());
    let quiet = CommandOutput::new(0, b"x", b"\n");
    assert_eq!(quiet.message(), None);
    assert_eq!(
        (quiet.exit_code(), quiet.stdout(), quiet.stderr()),
        (0, &b"x"[..], &b"\n"[..])
    );
    // Escaped for display, and capped.
    let many: String = (0..20).map(|i| format!("Error: e{i}\u{1b}\n")).collect();
    let many = CommandOutput::new(1, b"", many.as_bytes());
    assert_eq!(many.error_lines().len(), ERROR_LINES);
    assert_eq!(many.error_lines()[0], "Error: e0\\u{1b}");
    assert_eq!(many.message().as_deref(), Some("Error: e19\\u{1b}"));
}

#[test]
fn errors_name_the_command_and_map_to_wp_command_failed() {
    let error = plugin_list(&ok(fixture!("maintenance-page.html.txt"))).unwrap_err();
    assert_eq!(error.code(), Code::WpCommandFailed);
    assert_eq!(
        error.to_string(),
        "'wp plugin list' printed an HTML page instead of WP-CLI output: '<!DOCTYPE html>'"
    );
    assert!(std::error::Error::source(&error).is_some());
    let error = cli_version(&ok(b"")).unwrap_err();
    assert_eq!(error.to_string(), "'wp cli version' printed nothing");
    let error = site_facts(&ok(b"x")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "'wp eval' printed an unexpected line 1: 'x' (only WP-CLI's output and PHP notices \
         are read; such a line usually comes from wp-config.php, a must-use plugin, a \
         drop-in, or PHP at startup (php.ini's display_errors, Xdebug's develop mode): \
         remove that output, or turn those off for the PHP CLI)"
    );
    assert!(error.problems().is_empty());
}

#[test]
fn excerpts_escape_and_cut() {
    assert_eq!(excerpt(b"plain"), "plain");
    assert_eq!(excerpt(b"a'b\"c\\d\te"), "a\\'b\\\"c\\\\d\\te");
    assert_eq!(excerpt(b"\xff\xfe"), "\u{fffd}\u{fffd}");
    assert_eq!(excerpt("\u{202e}evil".as_bytes()), "\\u{202e}evil");
    let long = "é".repeat(EXCERPT_CHARS + 1);
    let cut = excerpt(long.as_bytes());
    assert_eq!(cut.chars().count(), EXCERPT_CHARS + 1);
    assert!(cut.ends_with('…'));
    let exact = excerpt("é".repeat(EXCERPT_CHARS).as_bytes());
    assert_eq!(exact.chars().count(), EXCERPT_CHARS);
    assert_eq!(tail_excerpt(b"short"), "short");
    let tail = tail_excerpt("x".repeat(EXCERPT_CHARS + 5).as_bytes());
    assert!(tail.starts_with('…'));
    assert_eq!(tail.chars().count(), EXCERPT_CHARS + 1);
}

#[test]
fn log_timestamps_and_trace_lines_are_recognized_narrowly() {
    assert_eq!(strip_log_time("[13-Oct-2026 01:25:54 UTC] PHP x"), "PHP x");
    assert_eq!(strip_log_time("[a] b"), "b");
    let long = format!("[{}] b", "1".repeat(49));
    for text in ["[{\"a\"] b", "[] b", "[x]b", "no bracket", &long] {
        assert_eq!(strip_log_time(text), text);
    }
    assert!(is_trace_line("#12 x"));
    assert!(is_trace_line("Stack trace:"));
    assert!(is_trace_line("  thrown in /x.php on line 2"));
    for text in ["#x", "#1", "# 1 x", "Stack trace", " thrown in x"] {
        assert!(!is_trace_line(text), "{text}");
    }
}

#[test]
fn shadowed_regular_plugins_never_read_as_absent() {
    // Real: an active regular `advanced-cache.php` behind the drop-in, and
    // Hello Dolly's `hello.php` behind a must-use `hello.php`; WP-CLI lists
    // neither regular plugin, while `plugin is-installed` succeeds for both.
    let list = list(fixture!("plugin-list-shadowed.json")).unwrap().value;
    for (name, file, status) in [
        ("advanced-cache", "advanced-cache.php", PluginStatus::Dropin),
        ("hello", "hello.php", PluginStatus::MustUse),
    ] {
        let lookup = list.find(&slug(name));
        let Lookup::Shadowed { by } = &lookup else {
            panic!("{name}: {lookup:?}");
        };
        assert_eq!((by.file(), by.status()), (file, &status), "{name}");
        assert!(!lookup.is_absent());
    }
    assert!(list.regular().all(|row| row.name() != "hello"));
}

#[test]
fn xdebug_call_stacks_and_multi_line_messages_are_notices() {
    // Real (PHP 8.4.26 + Xdebug 3, develop mode, display_errors=stdout):
    // two warnings, one of them two lines long, each with a call stack,
    // then the payload `[]`.
    let parsed = list(fixture!("xdebug-develop-notices.txt")).unwrap();
    assert!(parsed.value.rows().is_empty());
    assert_eq!(parsed.notices.total(), 11);
    assert_eq!(
        parsed.notices.lines()[0],
        "Warning: apvm warning in /xd/t.php on line 2"
    );
    // Real (no Xdebug): a two-line `trigger_error` message.
    let parsed = list(fixture!("php-multiline-notice.txt")).unwrap();
    assert_eq!(
        parsed.notices.lines(),
        [
            "Warning: apvm warning in /xd/t.php on line 2",
            "Warning: first line",
            "second line in /xd/t.php on line 3"
        ]
    );
    // Real lines, synthetic placement: Xdebug's log form (it went to
    // stderr) in front of a payload.
    let mut stdout = fixture!("xdebug-log-notices.stderr.txt").to_vec();
    stdout.extend_from_slice(b"[]");
    assert_eq!(list(&stdout).unwrap().notices.total(), 11);
    // Real: an uncaught exception under Xdebug (its message repeated after
    // a blank line, then the call stack); alone it is only notices.
    let uncaught = fixture!("xdebug-uncaught.txt");
    let OutputDefect::OnlyNotices { excerpt } = list_defect(uncaught) else {
        panic!("only notices");
    };
    assert!(excerpt.starts_with("Fatal error: Uncaught RuntimeException: boom"));
    let mut stdout = uncaught.to_vec();
    stdout.extend_from_slice(b"[]");
    assert_eq!(list(&stdout).unwrap().notices.total(), 4);
}

#[test]
fn multi_line_messages_never_swallow_content() {
    let payload = format!("[{}]", json_row("hello", "active", "hello.php"));
    // Up to 16 lines of message, ending in ` on line <n>`.
    let middle: String = (0..15).map(|i| format!("part {i}\n")).collect();
    let stdout = format!("Warning: start\n{middle}end in /x.php on line 9\n{payload}");
    assert_eq!(list(stdout.as_bytes()).unwrap().notices.total(), 17);
    // A 17-line message is too long: its lines are content.
    let middle: String = (0..16).map(|i| format!("part {i}\n")).collect();
    let stdout = format!("Warning: start\n{middle}end in /x.php on line 9\n{payload}");
    assert_eq!(
        list_defect(stdout.as_bytes()),
        OutputDefect::UnexpectedLine {
            line: 2,
            excerpt: "part 0".into()
        }
    );
    // A message that never ends, or is cut by a blank line, HTML, a payload
    // or another diagnostic, is content.
    for stdout in [
        format!("Warning: start\nnever ends\n{payload}"),
        format!("Warning: start\nmore\n\nend in /x.php on line 9\n{payload}"),
        format!("Warning: start\nmore\n<p>end in /x.php on line 9\n{payload}"),
        format!("Warning: start\nmore\n{payload}\nend in /x.php on line 9"),
        format!("Warning: start\nmore\nNotice: x\nend in /x.php on line 9\n{payload}"),
    ] {
        let defect = list_defect(stdout.as_bytes());
        assert!(
            matches!(
                defect,
                OutputDefect::UnexpectedLine { line: 2, .. } | OutputDefect::Html { .. }
            ),
            "{stdout:?}: {defect:?}"
        );
    }
    // Without a diagnostic first, a location-like line is content.
    let stdout = format!("x in /x.php on line 9\n{payload}");
    assert!(matches!(
        list_defect(stdout.as_bytes()),
        OutputDefect::UnexpectedLine { line: 1, .. }
    ));
    // ` on line <n>` needs 1 to 10 digits and nothing after them.
    for end in [
        "on line 9x",
        "on line ",
        "on line 12345678901",
        "on line -1",
    ] {
        let stdout = format!("Warning: a\nb {end}\n{payload}");
        assert!(list(stdout.as_bytes()).is_err(), "{end}");
    }
}

#[test]
fn xdebug_frames_are_recognized_narrowly() {
    for text in [
        "Call Stack:",
        "PHP Stack trace:",
        "    0.0003     390104   1. {main}() /xd/t.php:0",
        "\t0.1 2 3. f()",
        "PHP   1. {main}() /xd/t.php:0",
        "PHP 12. f()",
    ] {
        assert!(is_trace_line(text), "{text:?}");
    }
    for text in [
        "0.0003     390104   1. {main}()",
        "    0.0003     390104   1 {main}()",
        "    x.0003     390104   1. {main}()",
        "    0.     390104   1. {main}()",
        "    0.0003     39x104   1. {main}()",
        "    0.0003",
        "PHP 1.f()",
        "PHP x. f()",
        "PHP    . f()",
        "Call Stack",
    ] {
        assert!(!is_trace_line(text), "{text:?}");
    }
}

#[test]
fn notice_blocks_end_where_php_output_ends() {
    let payload = format!("[{}]", json_row("hello", "active", "hello.php"));
    // After a message ends, only trace lines follow: location-like lines
    // are content, however many.
    let many: String = (0..100)
        .map(|i| format!("row {i} in x on line {i}\n"))
        .collect();
    let stdout = format!("Warning: x\n{many}{payload}");
    assert_eq!(
        list_defect(stdout.as_bytes()),
        OutputDefect::UnexpectedLine {
            line: 3,
            excerpt: "row 1 in x on line 1".into()
        }
    );
    // A message line must end in PHP's location, ` in <file> on line <n>`.
    let boot = "Warning: plugin x\nSite is under maintenance, come back later on line 1\n\
                http://single.test\n";
    assert!(matches!(
        boot_check(&ok(boot.as_bytes()), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::UnexpectedLine { line: 2, .. }
        }
    ));
    // Trace lines need a diagnostic before them, and stop at content.
    let stdout = format!("Warning: a in /x.php on line 1\nhello\nCall Stack:\n{payload}");
    assert!(matches!(
        list_defect(stdout.as_bytes()),
        OutputDefect::UnexpectedLine { line: 2, .. }
    ));
}

#[test]
fn xdebug_repeats_an_uncaught_exception_once_after_a_blank_line() {
    let payload = "[]";
    let fatal = "Fatal error: Uncaught RuntimeException: boom in /x.php on line 1";
    let repeat = "RuntimeException: boom in /x.php on line 1";
    // The real shape (fixture `xdebug-uncaught.txt`): accepted.
    let stdout = format!("\n{fatal}\n\n{repeat}\n\nCall Stack:\n{payload}");
    assert_eq!(list(stdout.as_bytes()).unwrap().notices.total(), 3);
    for stdout in [
        // Twice.
        format!("{fatal}\n\n{repeat}\n\n{repeat}\n{payload}"),
        // Without the blank line.
        format!("{fatal}\n{repeat}\n{payload}"),
        // After a diagnostic that is not an uncaught exception.
        format!("Warning: w in /x.php on line 1\n\n{repeat}\n{payload}"),
        // Not a class name before `: `.
        format!("{fatal}\n\nsome text: boom in /x.php on line 1\n{payload}"),
    ] {
        assert!(
            matches!(
                list_defect(stdout.as_bytes()),
                OutputDefect::UnexpectedLine { .. }
            ),
            "{stdout:?}"
        );
    }
    // Namespaced classes are classes.
    let stdout = format!("{fatal}\n\nApp\\Errors\\Boom_1: boom in /x.php on line 1\n{payload}");
    assert!(list(stdout.as_bytes()).is_ok());
}

#[test]
fn regular_plugins_resolve_shadowed_lookups() {
    // Real: `plugin list` hides Hello Dolly (behind a must-use `hello.php`)
    // and an active regular `advanced-cache.php` (behind the drop-in); the
    // REGULAR_PLUGINS snippet, run on the same site, lists both.
    let shown = list(fixture!("plugin-list-multi-file-folder.json"))
        .unwrap()
        .value;
    let regular = regular_plugins(&ok(fixture!("regular-plugins-shadowed.json")))
        .unwrap()
        .value;
    for (name, file, status) in [
        ("hello", "hello.php", PluginStatus::Active),
        ("advanced-cache", "advanced-cache.php", PluginStatus::Active),
    ] {
        assert!(
            matches!(shown.find(&slug(name)), Lookup::Shadowed { .. }),
            "{name}"
        );
        let row = regular.find_regular(&slug(name)).found().unwrap();
        assert_eq!((row.file(), row.status()), (file, &status), "{name}");
    }
    // WP-CLI renamed the two-file folder's rows in `plugin list`; both
    // lists agree that `twofiles` is ambiguous, and `foo.php` too.
    for name in ["twofiles", "foo.php"] {
        assert!(matches!(shown.find(&slug(name)), Lookup::Ambiguous(rows) if rows.len() == 2));
        assert!(
            matches!(regular.find_regular(&slug(name)), Lookup::Ambiguous(rows) if rows.len() == 2)
        );
    }
    assert!(regular.find_regular(&slug("akismet")).found().is_some());
    assert!(regular.find_regular(&slug("nothing")).is_absent());
    // Real: the same read on a clean site and on the network.
    let clean = regular_plugins(&ok(fixture!("regular-plugins-single.json")))
        .unwrap()
        .value;
    assert_eq!(clean.rows().len(), 2);
    let network = regular_plugins(&ok(fixture!("regular-plugins-multisite.json")))
        .unwrap()
        .value;
    assert_eq!(
        network
            .find_regular(&slug("hello"))
            .found()
            .unwrap()
            .status(),
        &PluginStatus::ActiveNetwork
    );
}

#[test]
fn regular_plugin_reads_refuse_other_kinds() {
    for status in ["must-use", "dropin", "paused"] {
        let file = if status == "dropin" {
            "x.php"
        } else {
            "x\\/x.php"
        };
        let name = if status == "dropin" { "x.php" } else { "x" };
        let file = if status == "must-use" { "x.php" } else { file };
        let hex: String = file
            .replace("\\/", "/")
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        let row = json_row(name, status, file);
        let stdout = format!("[{},\"file_hex\":\"{hex}\"}}]", &row[..row.len() - 1]);
        let defect = defect_of(regular_plugins(&ok(stdout.as_bytes())));
        assert!(
            matches!(&defect, OutputDefect::InvalidList { reason } if reason.contains("get_plugins()")),
            "{status}: {defect:?}"
        );
    }
    let error = regular_plugins(&ok(b"")).unwrap_err();
    assert_eq!(error.to_string(), "'wp eval' printed nothing");
}

#[test]
fn dependency_entries_are_trimmed_like_php_trim() {
    // PHP's trim() strips " \t\n\r\0\x0B" (and nothing else).
    for c in [' ', '\t', '\n', '\r', '\0', '\u{b}'] {
        let raw = format!("{c}parent{c}");
        let header = serde_json::to_string(&raw).unwrap();
        let stdout = format!(r#"{{"name":"x","requires_plugins":{header}}}"#);
        let parsed = requires_plugins(&ok(stdout.as_bytes()), &resolved(&listed("x"), "x"))
            .unwrap()
            .value;
        assert_eq!(parsed.slugs, [slug("parent")], "{c:?}");
        assert!(parsed.ignored.is_empty(), "{c:?}");
    }
    for c in ['\u{c}', '\u{a0}', '\u{3000}'] {
        let raw = format!("{c}parent");
        let header = serde_json::to_string(&raw).unwrap();
        let stdout = format!(r#"{{"name":"x","requires_plugins":{header}}}"#);
        let parsed = requires_plugins(&ok(stdout.as_bytes()), &resolved(&listed("x"), "x"))
            .unwrap()
            .value;
        assert!(parsed.slugs.is_empty(), "{c:?}");
        assert_eq!(parsed.ignored.len(), 1, "{c:?}");
    }
}

#[test]
fn shadows_at_the_slug_itself_are_found() {
    // Real: Hello Dolly active behind a must-use `hello.php` (it is
    // missing from the list); `wp plugin path hello.php` and `hello` both
    // returned the hidden file.
    let list = list(fixture!("plugin-list-shadowed.json")).unwrap().value;
    for (name, file) in [
        ("hello.php", "hello.php"),
        ("hello", "hello.php"),
        ("advanced-cache.php", "advanced-cache.php"),
        ("advanced-cache", "advanced-cache.php"),
    ] {
        let lookup = list.find(&slug(name));
        assert!(
            matches!(lookup, Lookup::Shadowed { by } if by.file() == file),
            "{name}: {lookup:?}"
        );
        assert!(list.resolve(&slug(name)).is_none(), "{name}");
    }
    // Real: a regular `hello.php.php` next to the must-use `hello.php`
    // (Hello Dolly hidden again): WP-CLI took the hidden `hello.php` for
    // `hello.php`.
    let list = list_of(fixture!("plugin-list-shadowed-hello-php.json"));
    let Lookup::Ambiguous(rows) = list.find(&slug("hello.php")) else {
        panic!("hello.php.php and the shadowing must-use hello.php");
    };
    let files: Vec<&str> = rows.iter().map(|row| row.file()).collect();
    assert_eq!(files, ["hello.php.php", "hello.php"]);
    let hello_php_php = list.find_file("hello.php.php").found().unwrap();
    assert_eq!(list.slug_of(hello_php_php), None);
    assert!(list.resolve(&slug("hello.php")).is_none());
    // `hello.php.php` itself is still reachable by its full name.
    assert_eq!(
        list.resolve(&slug("hello.php.php")).unwrap().row().file(),
        "hello.php.php"
    );
    // The regular read sees both regular files: `hello.php` is ambiguous.
    let regular = regular_list(fixture!("regular-plugins-shadowed-hello-php.json"));
    let Lookup::Ambiguous(rows) = regular.find_regular(&slug("hello.php")) else {
        panic!("two regular files match hello.php");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(
        regular
            .find_regular(&slug("hello"))
            .found()
            .unwrap()
            .status(),
        &PluginStatus::Active
    );
}

/// `plugin list`'s value for `stdout`.
///
/// # Arguments
///
/// * `stdout` - A real `plugin list` output
fn list_of(stdout: &[u8]) -> PluginList {
    list(stdout).unwrap().value
}

/// The regular plugins read's value for `stdout`.
///
/// # Arguments
///
/// * `stdout` - A real REGULAR_PLUGINS output
fn regular_list(stdout: &[u8]) -> PluginList {
    regular_plugins(&ok(stdout)).unwrap().value
}

#[test]
fn unreadable_file_names_make_lookups_undetermined_until_resolved() {
    // Real: `akismet/a\xe9.php` (Latin-1) next to Akismet. `plugin list`
    // prints it as a `null` row and renames Akismet's row
    // `akismet/akismet` (the names repeat).
    let list = list_of(fixture!("plugin-list-unreadable.json"));
    assert_eq!(list.unnamed(), 1);
    assert!(list.unreadable_files().is_empty());
    for name in ["akismet", "hello", "nothing"] {
        assert_eq!(
            list.find(&slug(name)),
            Lookup::Unreadable { count: 1 },
            "{name}"
        );
        assert!(list.resolve(&slug(name)).is_none());
    }
    let akismet = list.find_file("akismet/akismet.php").found().unwrap();
    assert_eq!(list.slug_of(akismet), None);
    // Real: the regular read reports the bytes (`file_hex`), so only the
    // slugs the file can match stay undetermined.
    let regular = regular_list(fixture!("regular-plugins-unreadable.json"));
    assert_eq!(regular.unnamed(), 1);
    assert_eq!(regular.unreadable_files(), ["'akismet/a\u{fffd}.php'"]);
    assert_eq!(
        regular.find_regular(&slug("akismet")),
        Lookup::Unreadable { count: 1 }
    );
    assert!(regular.resolve(&slug("akismet")).is_none());
    let hello = regular.resolve(&slug("hello")).unwrap();
    assert_eq!(hello.row().file(), "hello.php");
    assert!(regular.find_regular(&slug("nothing")).is_absent());
}

#[test]
fn regular_rows_must_carry_their_file_bytes() {
    let row = |file: &str, hex: &str| {
        format!(
            r#"[{{"name":"x","status":"inactive","version":"","file":{file},"title":"","file_hex":"{hex}"}}]"#
        )
    };
    // `x.php` is 782e706870.
    assert!(regular_plugins(&ok(row("\"x.php\"", "782e706870").as_bytes())).is_ok());
    for (file, hex, reason) in [
        (
            "\"x.php\"",
            "782e70687",
            "has a file_hex that is not lowercase hexadecimal",
        ),
        (
            "\"x.php\"",
            "782E706870",
            "has a file_hex that is not lowercase hexadecimal",
        ),
        (
            "\"x.php\"",
            "792e706870",
            "has a file_hex that does not match its file",
        ),
        // `null` only for bytes that are not UTF-8.
        (
            "null",
            "782e706870",
            "has a file_hex that does not match its file",
        ),
    ] {
        assert_eq!(
            defect_of(regular_plugins(&ok(row(file, hex).as_bytes()))),
            OutputDefect::InvalidRow {
                index: 0,
                reason: reason.into()
            },
            "{file} {hex}"
        );
    }
    let missing = r#"[{"name":"x","status":"inactive","version":"","file":"x.php","title":""}]"#;
    assert!(matches!(
        defect_of(regular_plugins(&ok(missing.as_bytes()))),
        OutputDefect::InvalidJson { .. }
    ));
}

#[test]
fn line_payloads_need_their_newline_and_truncated_stdout_is_refused() {
    // A cut `WP-CLI 2.12.10` reads as a valid `2.12.1`: refused.
    assert!(matches!(
        defect_of(cli_version(&ok(b"WP-CLI 2.12.1"))),
        OutputDefect::Truncated { .. }
    ));
    assert!(matches!(
        boot_check(&ok(b"http://single.test"), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::Truncated { .. }
        }
    ));
    // JSON has no newline (real output); a cut one fails to decode.
    assert!(list(fixture!("plugin-list-single.json")).is_ok());
    // The transport's capture flag refuses any output.
    let cut =
        |stdout: &'static [u8]| CommandOutput::new(0, stdout, b"").with_stdout_truncated(true);
    assert!(cut(b"").stdout_truncated());
    assert!(!ok(b"").stdout_truncated());
    let refused = [
        defect_of(plugin_list(&cut(fixture!("plugin-list-single.json")))),
        defect_of(regular_plugins(&cut(fixture!(
            "regular-plugins-unreadable.json"
        )))),
        defect_of(cli_version(&cut(fixture!("cli-version.txt")))),
        defect_of(site_facts(&cut(fixture!("site-facts-single.json")))),
        defect_of(requires_plugins(
            &cut(fixture!("requires-plugins-child.json")),
            &resolved(&listed("child"), "child"),
        )),
    ];
    for defect in refused {
        assert!(
            matches!(defect, OutputDefect::Truncated { .. }),
            "{defect:?}"
        );
    }
    assert!(matches!(
        boot_check(&cut(fixture!("boot-check-ok.txt")), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::Truncated { .. }
        }
    ));
    // A failed command stays a failure.
    let failed = CommandOutput::new(1, b"", b"Error: x\n").with_stdout_truncated(true);
    assert!(matches!(
        defect_of(plugin_list(&failed)),
        OutputDefect::Failed { .. }
    ));
}

#[test]
fn boot_urls_need_a_host_and_no_surrounding_whitespace() {
    for (stdout, reason) in [
        (&b"https://\n"[..], "has no host"),
        (b"http:///path\n", "has no host"),
        (b"http://?q\n", "has no host"),
        (b"http://x   \n", "has surrounding whitespace"),
        (b"http://x\t\n", "contains U+0009"),
    ] {
        let verdict = boot_check(&ok(stdout), SITEURL);
        assert!(
            matches!(&verdict, BootCheck::NoBoot { defect: OutputDefect::InvalidField { field: "siteurl", reason: got } } if got.starts_with(reason)),
            "{stdout:?}: {verdict:?}"
        );
    }
    // Leading whitespace: not a URL line at all.
    assert!(matches!(
        boot_check(&ok(b" http://x\n"), SITEURL),
        BootCheck::NoBoot {
            defect: OutputDefect::UnexpectedLine { .. }
        }
    ));
    assert!(boot_check(&ok(b"http://x\n"), SITEURL).boots());
    assert!(boot_check(&ok(b"http://x\r\n"), SITEURL).boots());
}

#[test]
fn line_numbers_have_at_most_ten_digits() {
    let accepted = "Warning: a\nb in /x.php on line 1234567890\n[]";
    assert_eq!(list(accepted.as_bytes()).unwrap().notices.total(), 2);
    let refused = "Warning: a\nb in /x.php on line 12345678901\n[]";
    assert!(matches!(
        list_defect(refused.as_bytes()),
        OutputDefect::UnexpectedLine { line: 2, .. }
    ));
}

#[test]
fn floods_keep_memory_bounded() {
    // 4 Mi lines of noise: one excerpt kept per notice slot, the rest
    // counted; content summarized, never copied.
    let notices = "Warning: x in /x.php on line 1\n".repeat(1 << 18);
    let stdout = format!("{notices}[]");
    let parsed = list(stdout.as_bytes()).unwrap();
    assert_eq!(parsed.notices.total(), 1 << 18);
    assert_eq!(parsed.notices.lines().len(), NOTICE_LINES);
    let content = "a\n".repeat(1 << 20);
    assert_eq!(
        list_defect(content.as_bytes()),
        OutputDefect::UnexpectedLine {
            line: 1,
            excerpt: "a".into()
        }
    );
    let payloads = "[]\n".repeat(1 << 20);
    assert_eq!(
        list_defect(payloads.as_bytes()),
        OutputDefect::SeveralPayloads { count: 1 << 20 }
    );
}
