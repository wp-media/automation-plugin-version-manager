//! The line grammar of a WP-CLI command's stdout: which lines are PHP's
//! (skipped as [`Notices`]) and which are content (the payload, or a
//! refusal).
//!
//! A **notice** is, verified with PHP 8.4/8.5 and Xdebug 3 (fixtures under
//! `tests/fixtures/wp/`):
//!
//! - a diagnostic line: `[<time>] ` (log timestamp, optional), `PHP `
//!   (optional), a PHP label, `:` (`Deprecated: …`, `PHP Warning:  …`);
//! - right after a diagnostic that does not end in PHP's location
//!   (` in <file> on line <n>`), the rest of its message
//!   (`trigger_error("a\nb")` prints `Warning: a` then `b in /x.php on
//!   line 3`): at most 16 lines, the last ending in that location, none of
//!   them blank, HTML, a payload candidate or another diagnostic.
//!   Otherwise none of them is a notice, so a payload is never swallowed;
//! - after a diagnostic and its message, until any other line (blank lines
//!   aside): stack trace lines, PHP's (`Stack trace:`, `#<n> …`,
//!   `  thrown in …`) and Xdebug's (`Call Stack:` and its `<time> <memory>
//!   <n>. …` frames, `PHP Stack trace:` and its `PHP   <n>. …` frames,
//!   with or without a log timestamp), and, once, after an `Uncaught`
//!   fatal error and a blank line, Xdebug's repetition of the exception
//!   (`<Class>: … in <file> on line <n>`);
//! - a UTF-8 byte-order mark at the start of a line (a site file saved
//!   with one prints it), stripped.
//!
//! Everything else is content: the parsers accept exactly one payload line
//! and refuse any other.

use super::{EXCERPT_CHARS, Notices};

/// The labels PHP prints before a diagnostic (`php_error_cb`), for both
/// `display_errors` (`Deprecated: …`) and `log_errors` (`PHP Deprecated:
/// …`).
const PHP_LABELS: [&str; 8] = [
    "Deprecated",
    "Notice",
    "Warning",
    "Strict Standards",
    "Fatal error",
    "Recoverable fatal error",
    "Parse error",
    "Unknown error",
];
/// The UTF-8 byte-order mark.
const BOM: &[u8] = b"\xEF\xBB\xBF";
/// The most lines a multi-line diagnostic message may continue over.
const MESSAGE_LINES: usize = 16;
/// The most digits of a line number in ` on line <n>`.
const LINE_NUMBER_DIGITS: usize = 10;

/// One line of stdout.
#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    /// Its number, from 1.
    number: usize,
    /// Its bytes, without `\n` and a trailing `\r`.
    bytes: &'a [u8],
    /// Whether a `\n` ended it (the last line of a cut output has none).
    terminated: bool,
}

/// `stdout`'s lines, lazily: a cheap, cloneable iterator, so a lookahead
/// costs no copy.
///
/// # Arguments
///
/// * `stdout` - stdout after the nonce line
fn numbered_lines(stdout: &[u8]) -> impl Iterator<Item = Line<'_>> + Clone {
    stdout
        .split_inclusive(|b| *b == b'\n')
        .enumerate()
        .map(|(index, raw)| {
            let (bytes, terminated) = match raw.strip_suffix(b"\n") {
                Some(bytes) => (bytes, true),
                None => (raw, false),
            };
            Line {
                number: index + 1,
                bytes: bytes.strip_suffix(b"\r").unwrap_or(bytes),
                terminated,
            }
        })
}

/// stdout sorted into notices (skipped) and content, kept as a summary
/// (never a copy of every line), so memory stays bounded whatever stdout
/// holds.
pub(super) struct Scan<'a> {
    /// What was skipped.
    pub(super) notices: Notices,
    /// Whether any content line (neither blank nor a notice) was seen.
    pub(super) content: bool,
    /// The first HTML line, if any (the scan stops there).
    pub(super) html: Option<&'a [u8]>,
    /// The first content line that is not a payload candidate, with its
    /// number.
    pub(super) other: Option<(usize, &'a [u8])>,
    /// How many payload candidates were seen.
    pub(super) candidates: usize,
    /// The first payload candidate, and whether a `\n` ended it.
    pub(super) payload: Option<(&'a [u8], bool)>,
}

/// What may follow, as the scan goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No diagnostic is open: every line not blank is a diagnostic or
    /// content.
    Content,
    /// After a diagnostic and its message: trace lines may follow.
    Trace {
        /// Whether the diagnostic was an `Uncaught` fatal error whose
        /// repetition Xdebug may still print.
        uncaught: bool,
        /// Whether a blank line came since.
        blank_seen: bool,
    },
}

impl<'a> Scan<'a> {
    /// Sort `stdout`'s lines (see the module documentation), stopping at
    /// the first HTML line (the verdict then is known).
    ///
    /// # Arguments
    ///
    /// * `stdout` - stdout after the nonce line
    /// * `is_candidate` - Whether a line may be the payload: such a line
    ///   never continues a diagnostic message
    pub(super) fn of(stdout: &'a [u8], is_candidate: &dyn Fn(&[u8]) -> bool) -> Self {
        let mut scan = Self {
            notices: Notices::default(),
            content: false,
            html: None,
            other: None,
            candidates: 0,
            payload: None,
        };
        let mut lines = numbered_lines(stdout);
        let mut state = State::Content;
        while let Some(line) = lines.next() {
            let bytes = strip_boms(line.bytes, &mut scan.notices);
            let (verdict, next) = step(bytes, lines.clone(), state, is_candidate);
            state = next;
            match verdict {
                Verdict::Blank => {}
                Verdict::Notice { extra } => {
                    scan.notices.push_line(bytes);
                    for line in lines.by_ref().take(extra) {
                        let bytes = strip_boms(line.bytes, &mut scan.notices);
                        scan.notices.push_line(bytes);
                    }
                }
                Verdict::Content => {
                    if scan.add_content(Line { bytes, ..line }, is_candidate) {
                        break;
                    }
                }
            }
        }
        scan
    }

    /// Record one content line.
    ///
    /// # Arguments
    ///
    /// * `line` - The line (byte-order marks removed)
    /// * `is_candidate` - Whether a line may be the payload
    ///
    /// # Returns
    ///
    /// Whether the scan can stop (an HTML line decides the verdict).
    fn add_content(&mut self, line: Line<'a>, is_candidate: &dyn Fn(&[u8]) -> bool) -> bool {
        self.content = true;
        if is_html(line.bytes) {
            self.html = Some(line.bytes);
            return true;
        }
        if !is_candidate(line.bytes) {
            self.other.get_or_insert((line.number, line.bytes));
            return false;
        }
        self.candidates += 1;
        self.payload.get_or_insert((line.bytes, line.terminated));
        false
    }
}

/// What one line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Blank: neither kept nor counted.
    Blank,
    /// PHP's, with `extra` more lines of its message right after it.
    Notice {
        /// How many following lines continue its message.
        extra: usize,
    },
    /// Content: the payload, or a line to refuse.
    Content,
}

/// One step of the scan: what `line` is, and the state after it.
///
/// # Arguments
///
/// * `line` - The current line (byte-order marks removed)
/// * `rest` - The lines after it (a lookahead, only read)
/// * `state` - The state before it
/// * `is_candidate` - Whether a line may be the payload
fn step<'a>(
    line: &[u8],
    rest: impl Iterator<Item = Line<'a>>,
    state: State,
    is_candidate: &dyn Fn(&[u8]) -> bool,
) -> (Verdict, State) {
    if is_blank(line) {
        let state = match state {
            State::Trace { uncaught, .. } => State::Trace {
                uncaught,
                blank_seen: true,
            },
            State::Content => State::Content,
        };
        return (Verdict::Blank, state);
    }
    let text = String::from_utf8_lossy(line);
    if is_php_diagnostic(&text) {
        let trace = State::Trace {
            uncaught: text.contains("Uncaught "),
            blank_seen: false,
        };
        let extra = if ends_with_php_location(&text) {
            0
        } else {
            message_end(rest, is_candidate).map_or(0, |end| end + 1)
        };
        return (Verdict::Notice { extra }, trace);
    }
    let State::Trace {
        uncaught,
        blank_seen,
    } = state
    else {
        return (Verdict::Content, State::Content);
    };
    if is_trace_line(&text) {
        return (Verdict::Notice { extra: 0 }, state);
    }
    if uncaught && blank_seen && is_uncaught_repetition(&text) {
        let after = State::Trace {
            uncaught: false,
            blank_seen: false,
        };
        return (Verdict::Notice { extra: 0 }, after);
    }
    (Verdict::Content, State::Content)
}

/// Where a multi-line diagnostic message ends: the offset of the first
/// line that ends in PHP's location, within [`MESSAGE_LINES`], with no
/// blank, HTML, payload-candidate or diagnostic line before it.
///
/// # Arguments
///
/// * `lines` - The lines after the diagnostic line
/// * `is_candidate` - Whether a line may be the payload
fn message_end<'a>(
    lines: impl Iterator<Item = Line<'a>>,
    is_candidate: &dyn Fn(&[u8]) -> bool,
) -> Option<usize> {
    for (offset, line) in lines.take(MESSAGE_LINES).enumerate() {
        let bytes = strip_bom_prefixes(line.bytes);
        if is_blank(bytes) || is_html(bytes) || is_candidate(bytes) {
            return None;
        }
        let text = String::from_utf8_lossy(bytes);
        if is_php_diagnostic(&text) {
            return None;
        }
        if ends_with_php_location(&text) {
            return Some(offset);
        }
    }
    None
}

/// Whether `text` is Xdebug's repetition of an uncaught exception:
/// `<Class>: <message> in <file> on line <n>`, the class made of letters,
/// digits, `_` and `\`.
///
/// # Arguments
///
/// * `text` - One line
fn is_uncaught_repetition(text: &str) -> bool {
    let class = text.split_once(": ").map(|(class, _)| class);
    let is_class = class.is_some_and(|class| {
        !class.is_empty()
            && class
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\\')
    });
    is_class && ends_with_php_location(text)
}

/// Whether `text` ends like a PHP diagnostic: ` in <file> on line <n>`.
///
/// # Arguments
///
/// * `text` - One line
fn ends_with_php_location(text: &str) -> bool {
    text.rsplit_once(" on line ")
        .is_some_and(|(before, number)| {
            before.contains(" in ")
                && (1..=LINE_NUMBER_DIGITS).contains(&number.len())
                && number.bytes().all(|b| b.is_ascii_digit())
        })
}

/// `line` without leading byte-order marks, each counted in `notices`.
///
/// # Arguments
///
/// * `line` - One line
/// * `notices` - Where the marks are counted
fn strip_boms<'a>(line: &'a [u8], notices: &mut Notices) -> &'a [u8] {
    let rest = strip_bom_prefixes(line);
    let marks = (line.len() - rest.len()) / BOM.len();
    for _ in 0..marks {
        notices.push("UTF-8 byte-order mark".to_string());
    }
    rest
}

/// `line` without leading byte-order marks (not counted: for lookaheads).
///
/// # Arguments
///
/// * `line` - One line
fn strip_bom_prefixes(mut line: &[u8]) -> &[u8] {
    while let Some(rest) = line.strip_prefix(BOM) {
        line = rest;
    }
    line
}

/// `bytes` split at `\n`, each line without a trailing `\r`.
///
/// # Arguments
///
/// * `bytes` - The output
pub(super) fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|b| *b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

/// Whether `line` holds only ASCII whitespace.
///
/// # Arguments
///
/// * `line` - One line
pub(super) fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// Whether `line` is the start of an HTML document or fragment.
///
/// # Arguments
///
/// * `line` - One content line
pub(super) fn is_html(line: &[u8]) -> bool {
    line.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<')
}

/// Whether `text` is a PHP diagnostic: `[<time>] ` (log timestamp,
/// optional), `PHP ` (optional), then a PHP label and `:`.
///
/// # Arguments
///
/// * `text` - One line
fn is_php_diagnostic(text: &str) -> bool {
    let text = strip_log_time(text);
    let text = text.strip_prefix("PHP ").unwrap_or(text);
    PHP_LABELS.iter().any(|label| {
        text.strip_prefix(label)
            .is_some_and(|rest| rest.starts_with(':'))
    })
}

/// `text` without a leading PHP log timestamp, `[13-Oct-2026 01:25:54 UTC] `.
/// Only letters, digits, spaces and `:/_+.-` may stand between the brackets,
/// so a JSON line (`[{"…`) never loses its start.
///
/// # Arguments
///
/// * `text` - One line
pub(super) fn strip_log_time(text: &str) -> &str {
    let Some(rest) = text.strip_prefix('[') else {
        return text;
    };
    let Some((time, after)) = rest.split_once("] ") else {
        return text;
    };
    let is_time_char = |c: char| c.is_ascii_alphanumeric() || " :/_+.-".contains(c);
    if (1..=48).contains(&time.len()) && time.chars().all(is_time_char) {
        after
    } else {
        text
    }
}

/// Whether `text` is a stack trace line after a diagnostic: PHP's
/// (`Stack trace:`, `#0 …`, `  thrown in …`) or Xdebug's (`Call Stack:`
/// and its frames, `PHP Stack trace:` and its log frames), with or without
/// a log timestamp.
///
/// # Arguments
///
/// * `text` - One line
pub(super) fn is_trace_line(text: &str) -> bool {
    let text = strip_log_time(text);
    matches!(text, "Stack trace:" | "Call Stack:" | "PHP Stack trace:")
        || text.starts_with("  thrown in ")
        || text
            .strip_prefix('#')
            .is_some_and(starts_with_number_and_space)
        || is_xdebug_frame(text)
        || is_xdebug_log_frame(text)
}

/// Whether `text` starts with digits and a space (`12 …`).
///
/// # Arguments
///
/// * `text` - The text after a `#`
fn starts_with_number_and_space(text: &str) -> bool {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && text.as_bytes().get(digits) == Some(&b' ')
}

/// Whether `text` is an Xdebug frame: indented, then a time (`0.0003`), a
/// memory size (`390104`) and a frame number (`1.`).
///
/// # Arguments
///
/// * `text` - One line
fn is_xdebug_frame(text: &str) -> bool {
    if !text.starts_with(char::is_whitespace) {
        return false;
    }
    let mut words = text.split_whitespace();
    let time = words.next().and_then(|word| word.split_once('.'));
    let is_number = |word: &str| !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
    let time = time.is_some_and(|(whole, fraction)| is_number(whole) && is_number(fraction));
    let memory = words.next().is_some_and(is_number);
    let frame = words
        .next()
        .and_then(|word| word.strip_suffix('.'))
        .is_some_and(is_number);
    time && memory && frame
}

/// Whether `text` is an Xdebug log frame: `PHP`, spaces, a frame number
/// and `. ` (`PHP   1. {main}() /x.php:0`).
///
/// # Arguments
///
/// * `text` - One line
fn is_xdebug_log_frame(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("PHP ") else {
        return false;
    };
    let rest = rest.trim_start_matches(' ');
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(". ")
}

/// `bytes` for a message: decoded (invalid UTF-8 replaced), escaped with
/// [`str::escape_debug`], at most [`EXCERPT_CHARS`] characters, `…` when
/// cut.
///
/// # Arguments
///
/// * `bytes` - The text
pub(super) fn excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(EXCERPT_CHARS).collect();
    let cut = if chars.next().is_some() { "…" } else { "" };
    format!("{}{cut}", head.escape_debug())
}

/// The last [`EXCERPT_CHARS`] characters of `bytes`, escaped, `…` first
/// when cut: where a truncated payload ends.
///
/// # Arguments
///
/// * `bytes` - The text
pub(super) fn tail_excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let count = text.chars().count();
    let skip = count.saturating_sub(EXCERPT_CHARS);
    let tail: String = text.chars().skip(skip).collect();
    let cut = if skip > 0 { "…" } else { "" };
    format!("{cut}{}", tail.escape_debug())
}
