//! Strict parsers for what WP-CLI prints (plan §9 rule 2, §6.4, §6.2).
//!
//! Each parser takes a command's [`CommandOutput`]: its exit status, its
//! stdout after the nonce line (the transport's unframing, P3.2), and its
//! stderr. It returns the typed value plus the [`Notices`] it skipped, or
//! [`Error::WpOutput`] (`wp_command_failed`) with an [`OutputDefect`].
//!
//! The rules, from what WP-CLI 2.12.0 was seen to print (plan §3.5, and
//! the fixtures under `tests/fixtures/wp/`):
//!
//! - A non-zero exit status is a failure, whatever stdout holds.
//! - The payload is **one line**: `--format=json` prints compact JSON
//!   without a trailing newline, `cli version` prints `WP-CLI 2.12.0`.
//! - **Skipped, and reported** as [`Notices`]: blank lines, PHP diagnostics
//!   (`Deprecated: …`, `PHP Warning:  …`, `[<time>] PHP Notice:  …`) with
//!   their multi-line messages and stack traces (PHP's and Xdebug's
//!   `Call Stack:`) before or after the payload, and UTF-8 byte-order marks
//!   (the grammar: `scan.rs`). With `display_errors` on, PHP 8.5 prints WP-CLI
//!   2.12.0's `Deprecated: Case statements followed by a semicolon …` line
//!   before the payload; a plugin that turns `display_errors` back on
//!   prints its shutdown warnings after it.
//! - **Refused**: empty output, only notices, an HTML page (a maintenance
//!   plugin can print one and exit 0), any other line (the error names it
//!   and the server-side remedy), two payloads,
//!   truncated or invalid JSON, a field of the wrong type, a missing or
//!   unknown field, a row whose name does not match its file. A read is
//!   never guessed from partial output, so noise never reads as "not
//!   installed".

mod scan;

use std::collections::BTreeSet;
use std::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::command::CommandKind;
use super::state::{
    PluginList, PluginRow, PluginStatus, Resolved, Slug, UnreadableRow, check_status,
};
use crate::allowlist;
use crate::error::{Error, Result};
use crate::problem::{printable, quoted};
use scan::{Scan, excerpt, is_blank, lines, tail_excerpt};

/// The most characters an excerpt keeps.
const EXCERPT_CHARS: usize = 240;
/// The most skipped lines [`Notices`] keeps (it counts them all).
const NOTICE_LINES: usize = 8;
/// The most `Error:` lines [`CommandOutput::error_lines`] returns.
const ERROR_LINES: usize = 8;
/// What the first line of `cli version` starts with.
const CLI_VERSION_PREFIX: &str = "WP-CLI ";
/// The longest PHP or WordPress version text accepted.
const VERSION_TEXT_MAX_CHARS: usize = 64;
/// The longest path a site fact may hold, in characters.
const FACT_PATH_MAX_CHARS: usize = 4096;
/// The longest URL a site fact or the boot check may hold, in characters.
const URL_MAX_CHARS: usize = 2048;
/// The longest version suffix kept (`2.13.0-alpha-1a2b3c4`), in bytes.
const VERSION_SUFFIX_MAX_BYTES: usize = 64;
/// What a WP-CLI command printed and how it ended: the plain inputs every
/// parser takes, so no parser depends on the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandOutput<'a> {
    /// The remote exit status (255 arrives as 254, plan §6.1).
    exit_code: i32,
    /// stdout after the nonce line.
    stdout: &'a [u8],
    /// stderr.
    stderr: &'a [u8],
    /// Whether the transport cut stdout at its capture limit.
    stdout_truncated: bool,
}

impl<'a> CommandOutput<'a> {
    /// What one command printed.
    ///
    /// # Arguments
    ///
    /// * `exit_code` - The remote exit status
    /// * `stdout` - stdout after the nonce line
    /// * `stderr` - stderr
    pub fn new(exit_code: i32, stdout: &'a [u8], stderr: &'a [u8]) -> Self {
        Self {
            exit_code,
            stdout,
            stderr,
            stdout_truncated: false,
        }
    }

    /// Mark stdout as cut by the transport's capture limit (plan §6.1 item
    /// 6: 8 MiB). Every parser then refuses it as
    /// [`OutputDefect::Truncated`], even when what is left looks complete:
    /// callers (P4.2's session) set it from the transport's flag.
    ///
    /// # Arguments
    ///
    /// * `truncated` - Whether stdout was cut
    #[must_use = "the builder returns the changed output"]
    pub fn with_stdout_truncated(mut self, truncated: bool) -> Self {
        self.stdout_truncated = truncated;
        self
    }

    /// Whether the transport cut stdout.
    pub fn stdout_truncated(&self) -> bool {
        self.stdout_truncated
    }

    /// The remote exit status.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// stdout after the nonce line.
    pub fn stdout(&self) -> &'a [u8] {
        self.stdout
    }

    /// stderr.
    pub fn stderr(&self) -> &'a [u8] {
        self.stderr
    }

    /// The first `Error: …` lines of stderr, escaped for display: what
    /// `WP_CLI::error` printed. A mutation whose expected state was
    /// reached is still `done`, with these as a warning (plan §6.4).
    pub fn error_lines(&self) -> Vec<String> {
        lines(self.stderr)
            .filter(|line| line.starts_with(b"Error:"))
            .take(ERROR_LINES)
            .map(excerpt)
            .collect()
    }

    /// The message that best explains a failure, escaped for display: the
    /// last `Error: …` line of stderr, else its last non-blank line (a
    /// fatal error's text). Shown to the user, never relied on: a site can
    /// change WordPress's messages (plan §6.4).
    pub fn message(&self) -> Option<String> {
        let non_blank: Vec<&[u8]> = lines(self.stderr).filter(|line| !is_blank(line)).collect();
        let last_error = non_blank
            .iter()
            .rev()
            .find(|line| line.starts_with(b"Error:"));
        last_error.or(non_blank.last()).map(|line| excerpt(line))
    }
}

/// Lines PHP printed around a payload, skipped: escaped excerpts of the
/// first few, and how many there were. Front ends show them as a warning
/// (plan §9 rule 2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notices {
    /// Excerpts of the first [`NOTICE_LINES`] skipped lines.
    lines: Vec<String>,
    /// How many lines were skipped.
    total: usize,
}

impl Notices {
    /// Excerpts of the first skipped lines (escaped for display).
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// How many lines were skipped in all.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Whether nothing was skipped.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Count one skipped line, keeping its excerpt if there is room.
    ///
    /// # Arguments
    ///
    /// * `text` - The line, already escaped
    fn push(&mut self, text: String) {
        self.total += 1;
        if self.lines.len() < NOTICE_LINES {
            self.lines.push(text);
        }
    }

    /// Count one skipped line, building its excerpt only if there is room
    /// (so a flood of notices costs no memory).
    ///
    /// # Arguments
    ///
    /// * `line` - The line's bytes
    fn push_line(&mut self, line: &[u8]) {
        self.total += 1;
        if self.lines.len() < NOTICE_LINES {
            self.lines.push(excerpt(line));
        }
    }
}

/// A parsed value and the lines skipped around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed<T> {
    /// The value.
    pub value: T,
    /// The PHP notices and other skipped lines (plan §9: skipped with a
    /// warning).
    pub notices: Notices,
}

/// Why a command's output was refused. Every variant maps to
/// `wp_command_failed` (plan §4.5, §9); excerpts are escaped
/// ([`str::escape_debug`]) and at most 240 characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OutputDefect {
    /// The command exited with a non-zero status.
    #[error("exited with status {exit_code}{}", message.as_ref().map(|m| format!(": '{m}'")).unwrap_or_default())]
    Failed {
        /// The exit status.
        exit_code: i32,
        /// The last `Error:` line of stderr, else its last line.
        message: Option<String>,
    },
    /// It printed nothing (or only blank lines).
    #[error("printed nothing")]
    Empty,
    /// It printed only an empty line: WP-CLI 2.12.0's `--format=json` does
    /// that when `json_encode` fails on a value that is not valid UTF-8
    /// (verified with `plugin get` and a `Requires Plugins` header).
    #[error("printed an empty line instead of JSON: a value is not valid UTF-8")]
    Unencodable,
    /// It printed only PHP notices, no payload.
    #[error("printed only PHP notices, starting with '{excerpt}'")]
    OnlyNotices {
        /// The first notice.
        excerpt: String,
    },
    /// It printed an HTML page (a maintenance or security plugin, a
    /// web-server error) instead of WP-CLI's output.
    #[error("printed an HTML page instead of WP-CLI output: '{excerpt}'")]
    Html {
        /// The first HTML line.
        excerpt: String,
    },
    /// It printed a line that is neither the payload nor a PHP notice.
    #[error(
        "printed an unexpected line {line}: '{excerpt}' (only WP-CLI's output and PHP \
         notices are read; such a line usually comes from wp-config.php, a must-use \
         plugin, a drop-in, or PHP at startup (php.ini's display_errors, Xdebug's \
         develop mode): remove that output, or turn those off for the PHP CLI)"
    )]
    UnexpectedLine {
        /// The line number in stdout, from 1.
        line: usize,
        /// The line.
        excerpt: String,
    },
    /// It printed several lines that look like the payload.
    #[error("printed {count} payloads instead of one")]
    SeveralPayloads {
        /// How many.
        count: usize,
    },
    /// The JSON payload ends before it is complete (a cut connection or an
    /// output cap).
    #[error("printed truncated JSON ending in '{excerpt}'")]
    Truncated {
        /// The end of the payload.
        excerpt: String,
    },
    /// The payload is not JSON of the expected shape.
    #[error("printed invalid JSON ({reason}): '{excerpt}'")]
    InvalidJson {
        /// serde_json's message (escaped), with its line and column.
        reason: String,
        /// The start of the payload.
        excerpt: String,
    },
    /// A `plugin list` row breaks a rule.
    #[error("printed a plugin row {index} that {reason}")]
    InvalidRow {
        /// The row's position in the payload, from 0.
        index: usize,
        /// Why it is refused (values escaped).
        reason: String,
    },
    /// The `plugin list` rows contradict each other.
    #[error("printed a plugin list that {reason}")]
    InvalidList {
        /// Why it is refused (values escaped).
        reason: String,
    },
    /// `cli version` printed a version it cannot read.
    #[error("printed an unreadable WP-CLI version: '{excerpt}'")]
    InvalidVersion {
        /// The line.
        excerpt: String,
    },
    /// A field (a site fact, a plugin's name) breaks its rule.
    #[error("printed a field '{field}' that {reason}")]
    InvalidField {
        /// The field's key.
        field: &'static str,
        /// Why it is refused (values escaped).
        reason: String,
    },
}

/// A WP-CLI version, `<major>.<minor>.<patch>[-<suffix>]` (`2.12.0`,
/// `2.13.0-alpha-1a2b3c4`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WpCliVersion {
    /// The major number.
    pub major: u32,
    /// The minor number.
    pub minor: u32,
    /// The patch number.
    pub patch: u32,
    /// What follows a `-`, if anything (`[A-Za-z0-9.-]`, at most 64 bytes).
    pub suffix: Option<String>,
}

impl WpCliVersion {
    /// Read a version.
    ///
    /// # Arguments
    ///
    /// * `text` - Such as `2.12.0`
    ///
    /// # Returns
    ///
    /// The version, or `None` for any other text.
    pub fn parse(text: &str) -> Option<Self> {
        let (core, suffix) = match text.split_once('-') {
            Some((core, suffix)) => (core, Some(suffix)),
            None => (text, None),
        };
        let mut numbers = core.split('.').map(version_number);
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) = (
            numbers.next(),
            numbers.next(),
            numbers.next(),
            numbers.next(),
        ) else {
            return None;
        };
        if suffix.is_some_and(|s| !is_version_suffix(s)) {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            suffix: suffix.map(str::to_string),
        })
    }
}

impl std::fmt::Display for WpCliVersion {
    /// The version as WP-CLI prints it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match &self.suffix {
            Some(suffix) => write!(f, "-{suffix}"),
            None => Ok(()),
        }
    }
}

/// One number of a version: 1 to 9 ASCII digits.
///
/// # Arguments
///
/// * `text` - The number's text
fn version_number(text: &str) -> Option<u32> {
    let digits = (1..=9).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten()
}

/// Whether `text` is a version suffix: 1 to 64 of `[A-Za-z0-9.-]`.
///
/// # Arguments
///
/// * `text` - What follows the first `-`
fn is_version_suffix(text: &str) -> bool {
    (1..=VERSION_SUFFIX_MAX_BYTES).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

/// What probe step W reads inside WordPress (plan §6.2; the
/// [`SITE_FACTS`](super::snippets::SITE_FACTS) snippet). These are
/// **observations**, not inputs: paths need only be absolute and URLs not
/// empty, so a working site with a non-canonical `WP_CONTENT_DIR`
/// (`/var/www/html/../content`), an IDN or `_` host, or an upper-case
/// scheme is read as it is. No value may hold a control or invisible
/// formatting character (the crate's free-text rule), so none can drive a
/// terminal. They come from the site, which controls `wp-config.php` and
/// its must-use plugins: compare them, never trust them for the identity
/// (plan §6.2 step O). Before a value becomes an argument, check it with
/// that argument's own rule: a `siteurl` used as `--url` must pass the
/// site URL rule ([`WpSite::url`](super::WpSite::url) refuses it
/// otherwise), and paths compared or used in scripts should be the
/// `*_realpath` ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SiteFacts {
    /// `WP_CLI_VERSION`.
    pub wp_cli_version: WpCliVersion,
    /// `PHP_VERSION`.
    pub php_version: String,
    /// WordPress's version.
    pub wp_version: String,
    /// `is_multisite()`.
    pub multisite: bool,
    /// `WP_PLUGIN_DIR`.
    pub plugin_dir: String,
    /// `realpath( WP_PLUGIN_DIR )`; `None` when PHP could not resolve or
    /// encode it (then step O cannot confirm the plugins directory).
    pub plugin_dir_realpath: Option<String>,
    /// `WP_CONTENT_DIR`.
    pub content_dir: String,
    /// `realpath( WP_CONTENT_DIR )`; `None` when PHP could not resolve or
    /// encode it. Step C's space and write checks use this one.
    pub content_dir_realpath: Option<String>,
    /// The `siteurl` option.
    pub siteurl: String,
    /// The `home` option.
    pub home: String,
    /// Whether `DISALLOW_FILE_MODS` is defined and true (plan §6.7).
    pub disallow_file_mods: bool,
}

/// The boot check's verdict (plan D13, §6.4 `Boots`). WordPress booted
/// when `option get siteurl`, with every plugin loaded, exits 0 **and**
/// prints exactly one `http://` or `https://` line (PHP notices aside): a
/// plugin that prints a page and `exit`s on `init`, or `die()`s on
/// `plugins_loaded`, exits 0 too (verified with WP-CLI 2.12.0), and is
/// caught that way. Every verdict but [`BootCheck::Boots`] is
/// `health_check_failed`.
///
/// What it is not: proof against deception. A plugin can print a URL and
/// exit, and that passes; the check catches fatal errors, `die()`, HTML
/// pages and empty output, not a plugin that fakes a boot on purpose.
/// WordPress's own fatal handler exits 1 with `Error: There has been a
/// critical error on this website.`; messages are shown, never relied on
/// (a drop-in can change them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootCheck {
    /// WordPress booted with every plugin loaded.
    Boots {
        /// PHP notices skipped around it (show them as a warning).
        notices: Notices,
        /// The URL printed (escaped), when it differs from the expected
        /// `siteurl`: a plugin filters or rewrote it (force-HTTPS, domain
        /// mapping). Show it as a warning; it is not a failure.
        siteurl_changed: Option<String>,
    },
    /// The command exited with a non-zero status.
    Fails {
        /// The exit status.
        exit_code: i32,
        /// [`CommandOutput::message`].
        message: Option<String>,
    },
    /// It exited 0 but did not print one well-formed URL line: nothing (a
    /// `die()`), an HTML page, other lines, or a URL with a control
    /// character or over 2048 characters. WordPress did not finish booting.
    NoBoot {
        /// What was wrong with the output.
        defect: OutputDefect,
    },
}

impl BootCheck {
    /// Whether WordPress booted ([`BootCheck::Boots`]).
    pub fn boots(&self) -> bool {
        matches!(self, Self::Boots { .. })
    }
}

/// A plugin's `Requires Plugins` header (plan §6.3: activations in
/// dependency order), read with
/// [`WpCommand::requires_plugins`](super::WpCommand::requires_plugins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiresPlugins {
    /// The header as written (raw text from the site: escape it before
    /// showing it).
    pub raw: String,
    /// The set of slugs WordPress reads from it: the same entries as
    /// WordPress 7.1.3's `WP_Plugin_Dependencies::sanitize_dependency_slugs`
    /// keeps (split at commas, PHP-trimmed, only
    /// `^[a-z0-9]+(-[a-z0-9]+)*$`, de-duplicated). The order is byte order,
    /// not WordPress's `sort()` (which compares numeric slugs as numbers);
    /// it is a set, so order carries no meaning. Match the entries against
    /// [`PluginRow::dependency_slug`] (`hello-dolly` is `hello.php`). Not
    /// applied: the `wp_plugin_dependencies_slug` filter (a plugin may map
    /// slugs; the read skips plugins).
    pub slugs: Vec<Slug>,
    /// The non-empty entries WordPress ignores (not of that format, or too
    /// long for a [`Slug`]), escaped: worth a plan warning.
    pub ignored: Vec<String>,
}

/// Read `plugin list`'s output ([`WpCommand::plugin_list`](super::WpCommand::plugin_list)).
///
/// Rows printed with a `null` name or file (a name that is not valid
/// UTF-8) are counted in [`PluginList::unnamed`], never matched. A `null`
/// version or title (not valid UTF-8) reads as none.
///
/// # Arguments
///
/// * `output` - What the command printed
///
/// # Errors
///
/// [`Error::WpOutput`] (`wp_command_failed`) with the [`OutputDefect`].
pub fn plugin_list(output: &CommandOutput<'_>) -> Result<Parsed<PluginList>> {
    parse(CommandKind::PluginList, output, |payload| {
        let rows: Vec<Object<RawRow>> = decode_json(payload, b'[')?;
        list_from_rows(rows.into_iter().map(|row| (row.0, None)))
    })
}

/// Read the [`REGULAR_PLUGINS`](super::snippets::REGULAR_PLUGINS)
/// snippet's output ([`WpCommand::regular_plugins`](super::WpCommand::regular_plugins)):
/// `plugin list`'s row shape and rules, every row a regular plugin. Look
/// a slug up with [`PluginList::find_regular`].
///
/// # Arguments
///
/// * `output` - What the command printed
///
/// # Errors
///
/// [`Error::WpOutput`] (`wp_command_failed`) with the [`OutputDefect`]; a
/// must-use, drop-in or unknown status is refused (`get_plugins()` never
/// returns one).
pub fn regular_plugins(output: &CommandOutput<'_>) -> Result<Parsed<PluginList>> {
    parse(CommandKind::RegularPlugins, output, |payload| {
        let rows: Vec<Object<RawRegularRow>> = decode_json(payload, b'[')?;
        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(index, Object(row))| regular_row(index, row))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        list_from_rows(rows.into_iter())
    })
}

/// One [`RawRegularRow`] as a `plugin list` row and its file's bytes: the
/// status must be regular, `file_hex` must decode, and match `file` when
/// `file` is printed (it is `null` only for bytes that are not UTF-8).
///
/// # Arguments
///
/// * `index` - The row's position, for errors
/// * `row` - The decoded row
fn regular_row(
    index: usize,
    row: RawRegularRow,
) -> std::result::Result<(RawRow, Option<Vec<u8>>), OutputDefect> {
    let invalid = |reason: String| OutputDefect::InvalidRow { index, reason };
    if !PluginStatus::parse(&row.status).is_regular() {
        return Err(OutputDefect::InvalidList {
            reason: format!(
                "has a plugin of status {}, which get_plugins() never returns",
                PluginStatus::parse(&row.status)
            ),
        });
    }
    let bytes = decode_hex(&row.file_hex)
        .ok_or_else(|| invalid("has a file_hex that is not lowercase hexadecimal".into()))?;
    let consistent = match &row.file {
        Some(file) => file.as_bytes() == bytes.as_slice(),
        None => std::str::from_utf8(&bytes).is_err(),
    };
    if !consistent {
        return Err(invalid(
            "has a file_hex that does not match its file".into(),
        ));
    }
    let raw = RawRow {
        name: row.name,
        status: row.status,
        version: row.version,
        file: row.file,
        title: row.title,
    };
    Ok((raw, Some(bytes)))
}

/// `text` decoded from lowercase hexadecimal (PHP's `bin2hex`).
///
/// # Arguments
///
/// * `text` - The hexadecimal text
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| match pair {
            [high, low] => Some(digit(*high)? << 4 | digit(*low)?),
            _ => None,
        })
        .collect()
}

/// Read `cli version`'s output ([`WpCommand::cli_version`](super::WpCommand::cli_version)):
/// one line, `WP-CLI <version>`.
///
/// # Arguments
///
/// * `output` - What the command printed
///
/// # Errors
///
/// [`Error::WpOutput`] (`wp_command_failed`) with the [`OutputDefect`];
/// any other program than WP-CLI fails here.
pub fn cli_version(output: &CommandOutput<'_>) -> Result<Parsed<WpCliVersion>> {
    let is_candidate = |line: &[u8]| line.starts_with(CLI_VERSION_PREFIX.as_bytes());
    parse_with(
        CommandKind::CliVersion,
        output,
        &is_candidate,
        Payload::Line,
        |line| {
            std::str::from_utf8(line)
                .ok()
                .and_then(|text| text.strip_prefix(CLI_VERSION_PREFIX))
                .and_then(WpCliVersion::parse)
                .ok_or_else(|| OutputDefect::InvalidVersion {
                    excerpt: excerpt(line),
                })
        },
    )
}

/// Read the site facts snippet's output ([`WpCommand::site_facts`](super::WpCommand::site_facts)).
///
/// # Arguments
///
/// * `output` - What the command printed
///
/// # Errors
///
/// [`Error::WpOutput`] (`wp_command_failed`) with the [`OutputDefect`].
pub fn site_facts(output: &CommandOutput<'_>) -> Result<Parsed<SiteFacts>> {
    parse(CommandKind::SiteFacts, output, |payload| {
        let Object(raw) = decode_json::<Object<RawFacts>>(payload, b'{')?;
        facts_from_raw(raw)
    })
}

/// Read `plugin get --fields=name,requires_plugins`'s output
/// ([`WpCommand::requires_plugins`](super::WpCommand::requires_plugins)):
/// one JSON object, `{"name":"child","requires_plugins":"parent"}`
/// (verified with WP-CLI 2.12.0; an empty header prints `""`). `name` is
/// WP-CLI's `get_plugin_name()` of the file it fetched, which must be the
/// target's ([`Resolved::wp_cli_name`]); with a [`Resolved`] target the
/// fetch cannot have hit another plugin of that name.
///
/// # Arguments
///
/// * `output` - What the command printed
/// * `target` - The plugin asked for
///
/// # Errors
///
/// [`Error::WpOutput`] (`wp_command_failed`) with the [`OutputDefect`]:
/// [`OutputDefect::Unencodable`] when a value is not valid UTF-8 (WP-CLI
/// then prints only `\n`, exit 0; verified), and exit 1 for a plugin that
/// is not installed (`Error: The '<slug>' plugin could not be found.`).
pub fn requires_plugins(
    output: &CommandOutput<'_>,
    target: &Resolved<'_>,
) -> Result<Parsed<RequiresPlugins>> {
    if output.exit_code == 0 && !output.stdout_truncated && output.stdout == b"\n" {
        return Err(Error::WpOutput {
            command: CommandKind::RequiresPlugins.label(),
            defect: OutputDefect::Unencodable,
        });
    }
    let expected = target.wp_cli_name().as_str();
    parse(CommandKind::RequiresPlugins, output, |payload| {
        let Object(raw) = decode_json::<Object<RawRequires>>(payload, b'{')?;
        if raw.name != expected {
            return Err(invalid_field(
                "name",
                format!("is {} instead of '{expected}'", quoted(&raw.name)),
            ));
        }
        Ok(dependencies(raw.requires_plugins))
    })
}

/// Read the boot check's outcome ([`WpCommand::boot_check`](super::WpCommand::boot_check)),
/// see [`BootCheck`].
///
/// # Arguments
///
/// * `output` - What the command printed
/// * `siteurl` - The site's `siteurl`, as probe step W read it
///   ([`SiteFacts::siteurl`], same `--url`): a different URL is reported
///   in [`BootCheck::Boots::siteurl_changed`], not refused
pub fn boot_check(output: &CommandOutput<'_>, siteurl: &str) -> BootCheck {
    let line = payload(output, &is_url_line, Payload::Line).and_then(|(line, notices)| {
        let text = std::str::from_utf8(line)
            .map_err(|_| invalid_field("siteurl", "is not valid UTF-8"))?;
        boot_url(text).map_err(|reason| invalid_field("siteurl", reason))?;
        Ok((text, notices))
    });
    match line {
        Ok((text, notices)) => BootCheck::Boots {
            notices,
            siteurl_changed: (text != siteurl).then(|| excerpt(text.as_bytes())),
        },
        Err(OutputDefect::Failed { exit_code, message }) => BootCheck::Fails { exit_code, message },
        Err(defect) => BootCheck::NoBoot { defect },
    }
}

/// The rule for the URL the boot check printed: an observed URL
/// ([`observed_url`]) with no surrounding whitespace and a non-empty host
/// after the scheme.
///
/// # Arguments
///
/// * `text` - The URL line
fn boot_url(text: &str) -> allowlist::Rule {
    observed_url(text)?;
    if text.trim() != text {
        return Err("has surrounding whitespace".into());
    }
    let rest = text.split_once("://").map_or("", |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        return Err("has no host".into());
    }
    Ok(())
}

/// Whether `line` starts with `http://` or `https://`, in any letter case.
///
/// # Arguments
///
/// * `line` - One stdout line
fn is_url_line(line: &[u8]) -> bool {
    let starts = |scheme: &[u8]| {
        line.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    };
    starts(b"http://") || starts(b"https://")
}

/// Whether `line` may be a JSON payload: compact JSON as `json_encode`
/// prints it starts with `[{`, `[]`, `{"` or `{}`, and a payload cut
/// right after its first byte is `[` or `{` alone. Anything else that
/// starts with a bracket (`[info] …`, an unrecognized log line) is
/// content, refused as an unexpected line, never counted as a payload.
///
/// # Arguments
///
/// * `line` - One stdout line
fn is_json_line(line: &[u8]) -> bool {
    matches!(
        line,
        [b'['] | [b'{'] | [b'[', b'{' | b']', ..] | [b'{', b'"' | b'}', ..]
    )
}

/// Parse a JSON payload: the one stdout line [`is_json_line`] accepts.
///
/// # Arguments
///
/// * `kind` - The command, for the error
/// * `output` - What it printed
/// * `read` - Reads the payload line
fn parse<T>(
    kind: CommandKind,
    output: &CommandOutput<'_>,
    read: impl FnOnce(&[u8]) -> std::result::Result<T, OutputDefect>,
) -> Result<Parsed<T>> {
    parse_with(kind, output, &is_json_line, Payload::Json, read)
}

/// The shared parser: refuse a failed command, find the one payload line
/// among the notices, read it.
///
/// # Arguments
///
/// * `kind` - The command, for the error
/// * `output` - What it printed
/// * `is_candidate` - Whether a line is the payload
/// * `payload_kind` - What the payload is
/// * `read` - Reads the payload line
fn parse_with<T>(
    kind: CommandKind,
    output: &CommandOutput<'_>,
    is_candidate: &dyn Fn(&[u8]) -> bool,
    payload_kind: Payload,
    read: impl FnOnce(&[u8]) -> std::result::Result<T, OutputDefect>,
) -> Result<Parsed<T>> {
    let parsed = payload(output, is_candidate, payload_kind)
        .and_then(|(line, notices)| read(line).map(|value| Parsed { value, notices }));
    parsed.map_err(|defect| Error::WpOutput {
        command: kind.label(),
        defect,
    })
}

/// What a payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Payload {
    /// Compact JSON, printed without a trailing newline (a cut one fails
    /// to decode).
    Json,
    /// A text line (`WP-CLI 2.12.0`, a URL), always printed with its `\n`:
    /// one without it was cut, and may read as another valid value
    /// (`2.12.1` for `2.12.10`).
    Line,
}

/// The payload line of a successful command, and the notices skipped.
///
/// # Arguments
///
/// * `output` - What the command printed
/// * `is_candidate` - Whether a line is the payload
/// * `kind` - What the payload is
fn payload<'a>(
    output: &CommandOutput<'a>,
    is_candidate: &dyn Fn(&[u8]) -> bool,
    kind: Payload,
) -> std::result::Result<(&'a [u8], Notices), OutputDefect> {
    if output.exit_code != 0 {
        return Err(OutputDefect::Failed {
            exit_code: output.exit_code,
            message: output.message(),
        });
    }
    if output.stdout_truncated {
        return Err(OutputDefect::Truncated {
            excerpt: tail_excerpt(output.stdout),
        });
    }
    let scan = Scan::of(output.stdout, is_candidate);
    if !scan.content {
        return Err(match scan.notices.lines.first() {
            Some(first) => OutputDefect::OnlyNotices {
                excerpt: first.clone(),
            },
            None => OutputDefect::Empty,
        });
    }
    if let Some(html) = scan.html {
        return Err(OutputDefect::Html {
            excerpt: excerpt(html),
        });
    }
    if let Some((number, line)) = scan.other {
        return Err(OutputDefect::UnexpectedLine {
            line: number,
            excerpt: excerpt(line),
        });
    }
    match scan.payload {
        Some((line, terminated)) if scan.candidates == 1 => {
            if kind == Payload::Line && !terminated {
                return Err(OutputDefect::Truncated {
                    excerpt: tail_excerpt(line),
                });
            }
            Ok((line, scan.notices))
        }
        _ => Err(OutputDefect::SeveralPayloads {
            count: scan.candidates,
        }),
    }
}

/// Decode a JSON payload that must start with `opener`.
///
/// # Arguments
///
/// * `payload` - The payload line
/// * `opener` - `[` for an array, `{` for an object
fn decode_json<T: serde::de::DeserializeOwned>(
    payload: &[u8],
    opener: u8,
) -> std::result::Result<T, OutputDefect> {
    if payload.first() != Some(&opener) {
        return Err(OutputDefect::InvalidJson {
            reason: format!(
                "expected a JSON {}",
                if opener == b'[' { "array" } else { "object" }
            ),
            excerpt: excerpt(payload),
        });
    }
    serde_json::from_slice(payload).map_err(|error| {
        if error.is_eof() {
            return OutputDefect::Truncated {
                excerpt: tail_excerpt(payload),
            };
        }
        OutputDefect::InvalidJson {
            reason: printable(&error.to_string()),
            excerpt: excerpt(payload),
        }
    })
}

/// A record that must be a JSON object. serde's derived structs also read
/// a JSON array by position, which bypasses the field names (and
/// `deny_unknown_fields`); this wrapper accepts objects only.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    /// Read `T` from a JSON object; anything else is refused.
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The value's deserializer
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

/// [`Object`]'s visitor: a map only, read by `T`'s own field rules.
struct ObjectVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
    type Value = Object<T>;

    /// What is expected, for serde's messages.
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON object")
    }

    /// Hand the object's entries to `T`'s derived deserializer (which
    /// refuses missing, unknown and duplicate fields).
    ///
    /// # Arguments
    ///
    /// * `map` - The object's entries
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> std::result::Result<Self::Value, A::Error> {
        T::deserialize(MapAccessDeserializer::new(map)).map(Object)
    }
}

/// A field that must be present but may be `null` (serde reads a missing
/// `Option` field as `None` unless it has its own deserializer).
///
/// # Arguments
///
/// * `deserializer` - The field's deserializer
fn nullable<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

/// One `plugin list` row as printed: exactly the five fields, each present.
/// WP-CLI prints `null` for a value `json_encode` cannot encode (not valid
/// UTF-8; verified with WP-CLI 2.12.0).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    /// `name`.
    #[serde(deserialize_with = "nullable")]
    name: Option<String>,
    /// `status`: never `null`.
    status: String,
    /// `version`.
    #[serde(deserialize_with = "nullable")]
    version: Option<String>,
    /// `file`.
    #[serde(deserialize_with = "nullable")]
    file: Option<String>,
    /// `title`.
    #[serde(deserialize_with = "nullable")]
    title: Option<String>,
}

/// One [`REGULAR_PLUGINS`](super::snippets::REGULAR_PLUGINS) row as
/// printed: `plugin list`'s five fields and `file_hex`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegularRow {
    /// `name`.
    #[serde(deserialize_with = "nullable")]
    name: Option<String>,
    /// `status`.
    status: String,
    /// `version`.
    #[serde(deserialize_with = "nullable")]
    version: Option<String>,
    /// `file`.
    #[serde(deserialize_with = "nullable")]
    file: Option<String>,
    /// `title`.
    #[serde(deserialize_with = "nullable")]
    title: Option<String>,
    /// `bin2hex( $file )`.
    file_hex: String,
}

/// Check the rows and build the list.
///
/// # Arguments
///
/// * `raw` - The decoded rows, each with its file's bytes when the read
///   reports them
fn list_from_rows(
    raw: impl ExactSizeIterator<Item = (RawRow, Option<Vec<u8>>)>,
) -> std::result::Result<PluginList, OutputDefect> {
    let mut rows = Vec::with_capacity(raw.len());
    let mut unreadable = Vec::new();
    for (index, (row, file_bytes)) in raw.enumerate() {
        let status = PluginStatus::parse(&row.status);
        check_status(&status).map_err(|reason| OutputDefect::InvalidRow { index, reason })?;
        let (Some(name), Some(file)) = (row.name, row.file) else {
            unreadable.push(UnreadableRow::new(status, file_bytes));
            continue;
        };
        let row = PluginRow::new(name, status, row.version, file, row.title)
            .map_err(|reason| OutputDefect::InvalidRow { index, reason })?;
        rows.push(row);
    }
    PluginList::new(rows, unreadable).map_err(|reason| OutputDefect::InvalidList { reason })
}

/// The site facts as printed: exactly the keys [`SITE_FACTS`](super::snippets::SITE_FACTS) prints.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFacts {
    /// `WP_CLI_VERSION`.
    wp_cli_version: String,
    /// `PHP_VERSION`.
    php_version: String,
    /// WordPress's version.
    wp_version: String,
    /// `is_multisite()`.
    multisite: bool,
    /// `WP_PLUGIN_DIR`.
    plugin_dir: String,
    /// `realpath( WP_PLUGIN_DIR )` or `null`.
    #[serde(deserialize_with = "nullable")]
    plugin_dir_realpath: Option<String>,
    /// `WP_CONTENT_DIR`.
    content_dir: String,
    /// `realpath( WP_CONTENT_DIR )` or `null`.
    #[serde(deserialize_with = "nullable")]
    content_dir_realpath: Option<String>,
    /// `siteurl`.
    siteurl: String,
    /// `home`.
    home: String,
    /// `DISALLOW_FILE_MODS`.
    disallow_file_mods: bool,
}

/// Check the raw facts as observations: the WP-CLI version parses, the
/// PHP and WordPress versions are short free text, paths are absolute and
/// URLs not empty, and no value holds a control or invisible formatting
/// character (see [`SiteFacts`]).
///
/// # Arguments
///
/// * `raw` - The decoded facts
fn facts_from_raw(raw: RawFacts) -> std::result::Result<SiteFacts, OutputDefect> {
    let wp_cli_version = WpCliVersion::parse(&raw.wp_cli_version)
        .ok_or_else(|| invalid_field("wp_cli_version", "is not a WP-CLI version"))?;
    let checks: [FieldCheck<'_>; 8] = [
        ("php_version", Some(&raw.php_version), version_text),
        ("wp_version", Some(&raw.wp_version), version_text),
        ("plugin_dir", Some(&raw.plugin_dir), observed_path),
        (
            "plugin_dir_realpath",
            raw.plugin_dir_realpath.as_deref(),
            observed_path,
        ),
        ("content_dir", Some(&raw.content_dir), observed_path),
        (
            "content_dir_realpath",
            raw.content_dir_realpath.as_deref(),
            observed_path,
        ),
        ("siteurl", Some(&raw.siteurl), observed_url),
        ("home", Some(&raw.home), observed_url),
    ];
    for (field, value, rule) in checks {
        if let Some(Err(reason)) = value.map(rule) {
            return Err(invalid_field(field, reason));
        }
    }
    Ok(SiteFacts {
        wp_cli_version,
        php_version: raw.php_version,
        wp_version: raw.wp_version,
        multisite: raw.multisite,
        plugin_dir: raw.plugin_dir,
        plugin_dir_realpath: raw.plugin_dir_realpath,
        content_dir: raw.content_dir,
        content_dir_realpath: raw.content_dir_realpath,
        siteurl: raw.siteurl,
        home: raw.home,
        disallow_file_mods: raw.disallow_file_mods,
    })
}

/// One fact to check: its key, its value (`None`: nothing to check) and
/// the rule it must pass.
type FieldCheck<'a> = (&'static str, Option<&'a str>, fn(&str) -> allowlist::Rule);

/// The rule for a PHP or WordPress version: free text (no control or
/// invisible formatting character), 1 to 64 characters, not blank.
///
/// # Arguments
///
/// * `value` - The version
fn version_text(value: &str) -> allowlist::Rule {
    observed_text(value, VERSION_TEXT_MAX_CHARS)
}

/// The rule for an observed path: absolute, at most 4096 characters, no
/// control or invisible formatting character. Not canonical: WordPress
/// works with `/var/www/html/../content` (verified).
///
/// # Arguments
///
/// * `value` - The path
fn observed_path(value: &str) -> allowlist::Rule {
    observed_text(value, FACT_PATH_MAX_CHARS)?;
    if !value.starts_with('/') {
        return Err("is not an absolute path".into());
    }
    Ok(())
}

/// The rule for an observed URL: not empty, at most 2048 characters, no
/// control or invisible formatting character. No URL grammar: IDN hosts,
/// `_` in hosts and upper-case schemes all work in WordPress.
///
/// # Arguments
///
/// * `value` - The URL
fn observed_url(value: &str) -> allowlist::Rule {
    observed_text(value, URL_MAX_CHARS)
}

/// The free-text rule for an observation: not blank, at most `max_chars`,
/// no control or invisible formatting character (`allowlist::text`).
///
/// # Arguments
///
/// * `value` - The text
/// * `max_chars` - The most characters accepted
fn observed_text(value: &str, max_chars: usize) -> allowlist::Rule {
    if value.trim().is_empty() {
        return Err("is empty".into());
    }
    allowlist::text(value, max_chars)
}

/// An [`OutputDefect::InvalidField`].
///
/// # Arguments
///
/// * `field` - The field's key
/// * `reason` - Why it is refused (values already escaped)
fn invalid_field(field: &'static str, reason: impl Into<String>) -> OutputDefect {
    OutputDefect::InvalidField {
        field,
        reason: reason.into(),
    }
}

/// `plugin get --fields=name,requires_plugins` as printed: exactly these
/// two fields.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequires {
    /// The plugin's name.
    name: String,
    /// The raw header (WP-CLI never prints `null` here: a value it cannot
    /// encode makes it print nothing).
    requires_plugins: String,
}

/// The dependencies WordPress reads from a `Requires Plugins` header
/// (see [`RequiresPlugins::slugs`]).
///
/// # Arguments
///
/// * `raw` - The header
fn dependencies(raw: String) -> RequiresPlugins {
    let mut slugs = BTreeSet::new();
    let mut ignored = Vec::new();
    // PHP's trim() strips these.
    let trim = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\0' | '\u{b}');
    for entry in raw.split(',').map(|entry| entry.trim_matches(trim)) {
        if entry.is_empty() {
            continue;
        }
        match Slug::new(entry) {
            Ok(slug) if is_dependency_slug(entry) => {
                slugs.insert(slug);
            }
            _ => ignored.push(excerpt(entry.as_bytes())),
        }
    }
    RequiresPlugins {
        raw,
        slugs: slugs.into_iter().collect(),
        ignored,
    }
}

/// Whether `entry` has WordPress.org's slug format,
/// `^[a-z0-9]+(-[a-z0-9]+)*$`.
///
/// # Arguments
///
/// * `entry` - One trimmed entry of the header
fn is_dependency_slug(entry: &str) -> bool {
    entry.split('-').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests;
