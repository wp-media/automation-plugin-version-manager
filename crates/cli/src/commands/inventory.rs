//! Shared plumbing of the site-inventory commands (`apvm server`, `apvm site`
//! and `apvm group`).
//!
//! - [`Failure`]: why an inventory command failed, with the stable code of
//!   plan §4.5, a human rendering (one problem per line, plus a remedy) and
//!   a `--json` rendering.
//! - [`name_flags`]: rewrites the locations of a refused record (`ssh.port`)
//!   into the flags the user typed (`--port`).
//! - [`Console`]: where a command's output, warnings and confirmation
//!   prompts go; injected so whole commands run in unit tests.
//! - [`load`], [`confirm_change`], [`json_document`] and [`table`]: reading the
//!   inventory, asking before a removal, `--json` documents
//!   (`"schema": 1`) and aligned columns.
//! - [`field_table`]: the one-table field pattern (each command declares its
//!   settings once; applying, clearing, conflicts, reports and the locked
//!   change derive from it).
//! - [`remove_confirmed`]: removals that ask first and re-check what was
//!   confirmed under the lock.
//!
//! Every change goes through `apvm_sites::inventory::io::update` (lock →
//! load → change → save, all under the inventory's lock), never a `load`
//! followed by a `save`, which could lose another process's edit.

use std::io::{self, Write};
use std::path::Path;

use apvm_sites::inventory::io as inventory_io;
use apvm_sites::{Code, EntityKind, Error, INVENTORY_VERSION, Inventory, Problem, ResolvedSsh};
use serde::Serialize;

use crate::prompt;
use crate::status::{warn_line, yellow};

pub mod field_table;
mod removal;

pub use removal::{Removal, remove_confirmed};

/// Version of every `--json` document (`"schema": 1`, plan §5.1).
pub const JSON_SCHEMA: u32 = 1;

/// Code of a failure apvm cannot explain by the user's input or files: a
/// `--json` document that could not be serialized (not expected).
const INTERNAL_ERROR: &str = "internal_error";

/// Code of a change that needs a confirmation the user could not give
/// (plan §4.5).
const CONFIRMATION_REQUIRED: &str = "confirmation_required";

// ─────────────────────────────────────────────────────────────────────────────
// Failure
// ─────────────────────────────────────────────────────────────────────────────

/// Why an inventory command failed. Every failure exits with status 1.
#[derive(Debug)]
pub enum Failure {
    /// The sites library refused: invalid input (already naming the flags
    /// the user typed, see [`name_flags`]), an unknown or taken id, or an
    /// inventory file that cannot be read, locked or written.
    Sites(Error),
    /// A library refusal with the CLI's own remedy for it (`--notes ""`:
    /// `to clear it: --unset notes`); otherwise as [`Failure::Sites`].
    Remedied {
        /// What the library returned.
        error: Error,
        /// What the user can do, shown after the library's message (one
        /// line per remedy).
        remedy: String,
    },
    /// Arguments that contradict each other (`--user` with `--unset user`).
    Usage(String),
    /// A change needs confirmation, but stdin is not a terminal.
    ConfirmationRequired(String),
    /// The confirmation prompt could not be shown or answered.
    Prompt(io::Error),
    /// The inventory changed between the confirmation and the change, so
    /// what was confirmed is no longer what would happen.
    Changed(String),
    /// A `--json` document could not be serialized (not expected).
    Json(serde_json::Error),
    /// A connection test failed (`server test`, `site test`, or the test
    /// `add` runs before saving): its report, `--json` included, is already
    /// printed, so only this summary goes to stderr. `code` is the first
    /// failed test's (plan §4.5).
    TestFailed {
        /// What failed, and what was not done because of it.
        message: String,
        /// The first failed test's stable code.
        code: &'static str,
    },
}

impl Failure {
    /// The stable code (plan §4.5): the library's for [`Failure::Sites`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::Sites(error) | Self::Remedied { error, .. } => error.code().as_str(),
            Self::Usage(_) => Code::InvalidInput.as_str(),
            Self::ConfirmationRequired(_) | Self::Prompt(_) => CONFIRMATION_REQUIRED,
            Self::Changed(_) => Code::InventoryChanged.as_str(),
            Self::Json(_) => INTERNAL_ERROR,
            Self::TestFailed { code, .. } => code,
        }
    }

    /// Whether the command already printed its own report of this failure
    /// (so no `--json` error document follows it).
    fn reported(&self) -> bool {
        matches!(self, Self::TestFailed { .. })
    }

    /// The located problems: the library's, else none.
    fn problems(&self) -> &[Problem] {
        match self {
            Self::Sites(error) | Self::Remedied { error, .. } => error.problems(),
            _ => &[],
        }
    }

    /// The `--json` document of this failure:
    /// `{"schema": 1, "error": {"code", "message", "problems": [{"location", "message"}]}}`.
    /// `message` is one line, without the remedy.
    ///
    /// # Errors
    ///
    /// [`Failure::Json`] when serialization fails (not expected).
    pub fn to_json(&self) -> Result<String, Failure> {
        let message = match self {
            Self::Sites(error) | Self::Remedied { error, .. } => error.to_string(),
            other => other.to_string(),
        };
        let problems = self
            .problems()
            .iter()
            .map(|problem| JsonProblem {
                location: &problem.location,
                message: &problem.message,
            })
            .collect();
        json_document(&JsonFailure {
            error: JsonError {
                code: self.code(),
                message,
                problems,
            },
        })
    }
}

impl std::fmt::Display for Failure {
    /// The human rendering: the library's message with one problem per line
    /// and a remedy when there is one; otherwise the message itself.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sites(error) => f.write_str(&render(error)),
            Self::Remedied { error, remedy } => write!(f, "{}\n  {remedy}", render(error)),
            Self::Usage(message)
            | Self::ConfirmationRequired(message)
            | Self::Changed(message)
            | Self::TestFailed { message, .. } => f.write_str(message),
            Self::Prompt(error) => write!(f, "cannot ask for confirmation: {error}"),
            Self::Json(error) => write!(f, "cannot write the JSON output: {error}"),
        }
    }
}

impl std::error::Error for Failure {}

impl Failure {
    /// This failure with `remedy` shown after it, when it is a library
    /// refusal (after any remedy it already has, on its own line); any
    /// other failure is returned as is.
    ///
    /// # Arguments
    ///
    /// * `remedy` - What the user can do (one line)
    pub fn with_remedy(self, remedy: String) -> Self {
        match self {
            Self::Sites(error) => Self::Remedied { error, remedy },
            Self::Remedied {
                error,
                remedy: first,
            } => Self::Remedied {
                error,
                remedy: format!("{first}\n  {remedy}"),
            },
            other => other,
        }
    }
}

impl From<Error> for Failure {
    /// A library error, as is (see [`name_flags`] for refused input).
    fn from(error: Error) -> Self {
        Self::Sites(error)
    }
}

/// `--json` body of a failure.
#[derive(Serialize)]
struct JsonFailure<'a> {
    /// The failure.
    error: JsonError<'a>,
}

/// A failure in `--json` output.
#[derive(Serialize)]
struct JsonError<'a> {
    /// Stable code (plan §4.5).
    code: &'static str,
    /// One-line message.
    message: String,
    /// Located problems (locations name flags for refused input).
    problems: Vec<JsonProblem<'a>>,
}

/// A located problem in `--json` output.
#[derive(Serialize)]
struct JsonProblem<'a> {
    /// Where (`--port`, `servers[1].host`).
    location: &'a str,
    /// What is wrong.
    message: &'a str,
}

/// A library error as shown to a person: problems one per line, then the
/// remedy (`apvm-sites` messages name none: each front end adds its own).
fn render(error: &Error) -> String {
    let mut text = listed(error).unwrap_or_else(|| error.to_string());
    if let Some(hint) = remedy(error) {
        text.push_str("\n  ");
        text.push_str(&hint);
    }
    text
}

/// The message with its problems one per line, when the library's
/// one-line message ends with them (`<head>: <p1>; <p2>`); `None` when it
/// has no problems or another shape (it is then shown as is).
fn listed(error: &Error) -> Option<String> {
    let problems = error.problems();
    if problems.is_empty() {
        return None;
    }
    let joined = problems
        .iter()
        .map(Problem::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    let message = error.to_string();
    let head = message.strip_suffix(&joined)?.strip_suffix(": ")?;
    Some(problems.iter().fold(format!("{head}:"), |text, problem| {
        format!("{text}\n  {problem}")
    }))
}

/// What the user can do about `error`, when apvm knows.
fn remedy(error: &Error) -> Option<String> {
    match error {
        Error::NotFound { kind, .. } => Some(list_remedy(*kind)),
        Error::AlreadyExists { kind, .. } => Some(format!(
            "change it with 'apvm {kind} edit', or choose another id"
        )),
        Error::ServerInUse { .. } => Some("pass --cascade to remove those sites with it".into()),
        Error::InventoryLocked { .. } => Some(
            "edits hold the lock for milliseconds, so that process is likely stuck: \
             stop it, then retry"
                .into(),
        ),
        _ if error.code() == Code::InventoryInvalid => Some(
            "fix the file by hand (apvm never rewrites an inventory it cannot read), \
             or point APVM_INVENTORY at another inventory"
                .into(),
        ),
        _ => None,
    }
}

/// Where to see the records of `kind`: `'apvm site list' shows the sites`.
fn list_remedy(kind: EntityKind) -> String {
    format!("'apvm {kind} list' shows the {kind}s")
}

/// A [`Error::NotFound`] for `id` of `kind`, as the library reports one.
///
/// # Arguments
///
/// * `kind` - The kind of record
/// * `id` - The id that is not in the inventory
pub fn not_found(kind: EntityKind, id: &str) -> Failure {
    Failure::Sites(Error::NotFound {
        kind,
        id: id.to_string(),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Flag names
// ─────────────────────────────────────────────────────────────────────────────

/// Which flag sets each field of a record handed to the library:
/// `(location, flag)`, such as `("ssh.port", "--port")`.
pub type FlagMap = [(&'static str, &'static str)];

/// `error` with the locations of refused input renamed after the flags the
/// user typed (`ssh.port: …` → `--port: …`). A location under a mapped
/// field keeps its tail (`wp_cli[1]` → `--wp-cli[1]`); unmapped locations
/// and other errors are unchanged.
///
/// # Arguments
///
/// * `error` - What the library returned
/// * `flags` - The command's flag of each field
pub fn name_flags(error: Error, flags: &FlagMap) -> Failure {
    let error = match error {
        Error::InvalidInput { subject, problems } => Error::InvalidInput {
            subject,
            problems: problems
                .into_iter()
                .map(|problem| Problem {
                    location: flag_location(&problem.location, flags),
                    message: problem.message,
                })
                .collect(),
        },
        other => other,
    };
    Failure::Sites(error)
}

/// `location` with its field renamed after its flag (see [`name_flags`]).
fn flag_location(location: &str, flags: &FlagMap) -> String {
    flags
        .iter()
        .find_map(|(field, flag)| {
            let tail = location.strip_prefix(field)?;
            (tail.is_empty() || tail.starts_with(['.', '['])).then(|| format!("{flag}{tail}"))
        })
        .unwrap_or_else(|| location.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Console
// ─────────────────────────────────────────────────────────────────────────────

/// Where a command's I/O goes: its output (stdout), diagnostics (stderr)
/// and confirmation prompts. Injected so tests can capture and script it.
pub struct Console<'a> {
    /// Receives the command's output.
    pub out: &'a mut dyn FnMut(&str),
    /// Receives warnings and other diagnostics.
    pub err: &'a mut dyn FnMut(&str),
    /// Asks a yes/no question: `None` when nobody can be asked (stdin is
    /// not a terminal), else whether the answer is yes.
    pub ask: &'a mut dyn FnMut(&str) -> io::Result<Option<bool>>,
}

impl Console<'_> {
    /// Run `command` with a console on the real terminal: output to
    /// stdout, diagnostics to stderr (both best effort, so a closed pipe
    /// cannot crash a change that already happened), and prompts only when
    /// stdin is a terminal.
    ///
    /// # Arguments
    ///
    /// * `command` - What to run
    pub fn with_terminal<T>(command: impl FnOnce(&mut Console<'_>) -> T) -> T {
        command(&mut Terminal::new().console())
    }

    /// Write `text` to the output.
    pub fn print(&mut self, text: &str) {
        (self.out)(text);
    }

    /// Write a `warn` line to the diagnostics.
    pub fn warn(&mut self, message: &str) {
        (self.err)(&format!("{}\n", warn_line(message)));
    }
}

/// The real terminal's I/O, owned, so an `async` command can hold a
/// [`Console`] on it across `.await`s ([`Console::with_terminal`] runs a
/// closure).
pub struct Terminal {
    /// Writes the command's output.
    out: fn(&str),
    /// Writes diagnostics.
    err: fn(&str),
    /// Asks a yes/no question on the terminal.
    ask: fn(&str) -> io::Result<Option<bool>>,
}

impl Terminal {
    /// The terminal: output to stdout, diagnostics to stderr (both best
    /// effort, so a closed pipe cannot crash a change that already
    /// happened), and prompts only when stdin is a terminal.
    pub fn new() -> Self {
        Self {
            out: |text| {
                let _ = std::io::stdout().write_all(text.as_bytes());
            },
            err: |text| {
                let _ = std::io::stderr().write_all(text.as_bytes());
            },
            ask: |question| {
                prompt::confirm_on_terminal(&format!("{} ", yellow(&format!("? {question}"))))
            },
        }
    }

    /// A console on this terminal.
    pub fn console(&mut self) -> Console<'_> {
        Console {
            out: &mut self.out,
            err: &mut self.err,
            ask: &mut self.ask,
        }
    }
}

/// Ask `question` unless `yes` already confirmed it.
///
/// # Arguments
///
/// * `console` - Where to ask
/// * `yes` - Whether `-y`/`--yes` was passed
/// * `question` - The question, without the `[y/N]` hint
/// * `refusal` - The error message when nobody can be asked
///
/// # Returns
///
/// Whether to go ahead.
///
/// # Errors
///
/// [`Failure::ConfirmationRequired`] when stdin is not a terminal (with
/// `refusal`); [`Failure::Prompt`] when the prompt fails.
pub fn confirm_change(
    console: &mut Console<'_>,
    yes: bool,
    question: &str,
    refusal: impl FnOnce() -> String,
) -> Result<bool, Failure> {
    if yes {
        return Ok(true);
    }
    match (console.ask)(&format!("{question} [y/N]")).map_err(Failure::Prompt)? {
        Some(answer) => Ok(answer),
        None => Err(Failure::ConfirmationRequired(refusal())),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Reading and output helpers
// ─────────────────────────────────────────────────────────────────────────────

/// With `--json`, print a failed command's `--json` document on the
/// output (stderr still gets the `Error:` line from `main`). A document
/// that cannot be built prints nothing: the failure itself is reported.
///
/// # Arguments
///
/// * `result` - What the command returned
/// * `json` - Whether the user asked for JSON output
/// * `console` - Where the document goes
pub fn print_json_failure(result: &Result<(), Failure>, json: bool, console: &mut Console<'_>) {
    if let Err(failure) = result
        && json
        && !failure.reported()
        && let Ok(document) = failure.to_json()
    {
        console.print(&document);
    }
}

/// Load the inventory for reading, warning when it is read-only (written by
/// a newer apvm: it can be shown but not changed).
///
/// # Arguments
///
/// * `path` - The inventory file
/// * `console` - Where the warning goes
///
/// # Errors
///
/// Those of `inventory_io::load` (an unreadable, invalid or unsupported file).
pub fn load(path: &Path, console: &mut Console<'_>) -> Result<Inventory, Failure> {
    let inventory = inventory_io::load(path)?;
    if inventory.is_read_only() {
        console.warn(&format!(
            "the inventory has format version {}, newer than this apvm writes ({}); \
             it is read-only: upgrade apvm to change it",
            inventory.version(),
            INVENTORY_VERSION
        ));
    }
    Ok(inventory)
}

/// A `--json` document: `body`'s fields after `"schema": 1`, pretty-printed,
/// with a trailing newline.
///
/// # Arguments
///
/// * `body` - A value that serializes to a JSON object
///
/// # Errors
///
/// [`Failure::Json`] when serialization fails (not expected for apvm's
/// records).
pub fn json_document<T: Serialize>(body: &T) -> Result<String, Failure> {
    /// The document: the schema version, then the body's fields.
    #[derive(Serialize)]
    struct Document<'a, T> {
        /// Always [`JSON_SCHEMA`].
        schema: u32,
        /// The command's fields.
        #[serde(flatten)]
        body: &'a T,
    }
    let document = Document {
        schema: JSON_SCHEMA,
        body,
    };
    let mut text = serde_json::to_string_pretty(&document).map_err(Failure::Json)?;
    text.push('\n');
    Ok(text)
}

/// Rows aligned in columns two spaces apart, one line each (with its
/// newline); the last column is not padded, so no line ends with spaces.
/// Widths count characters.
///
/// # Arguments
///
/// * `indent` - Text in front of every line
/// * `rows` - The cells, row by row
pub fn table(indent: &str, rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect();
            format!("{indent}{}\n", cells.join("  ").trim_end())
        })
        .collect()
}

/// Where ssh connects: `[user@]host[:port]`, with the effective user and
/// port; an IPv6 address is bracketed when a port follows.
///
/// # Arguments
///
/// * `host` - The server's host, as written
/// * `ssh` - The server's effective SSH settings
pub fn destination(host: &str, ssh: &ResolvedSsh) -> String {
    let user = ssh
        .user
        .as_ref()
        .map(|user| format!("{}@", user.value))
        .unwrap_or_default();
    match &ssh.port {
        Some(port) if host.contains(':') => format!("{user}[{host}]:{}", port.value),
        Some(port) => format!("{user}{host}:{}", port.value),
        None => format!("{user}{host}"),
    }
}

/// `n` and `noun`, plural when `n` is not 1: `1 site`, `2 sites`.
///
/// # Arguments
///
/// * `n` - How many
/// * `noun` - The singular noun (plural adds `s`)
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Ids quoted and comma-separated: `'shop', 'blog'`. For ids from the
/// inventory, which never hold quotes or control characters.
///
/// # Arguments
///
/// * `ids` - The ids
pub fn quoted_ids<S: AsRef<str>>(ids: &[S]) -> String {
    ids.iter()
        .map(|id| format!("'{}'", id.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A [`Console`] for tests: it records the output, the diagnostics and the
/// questions asked, and answers every question the same way.
#[cfg(test)]
pub(crate) struct Recorder {
    /// Everything printed to the output.
    pub out: String,
    /// Everything printed to the diagnostics.
    pub err: String,
    /// Every question asked, in order.
    pub asked: Vec<String>,
}

#[cfg(test)]
impl Recorder {
    /// An empty recorder.
    pub fn new() -> Self {
        Self {
            out: String::new(),
            err: String::new(),
            asked: Vec::new(),
        }
    }

    /// Run `f` with a console that answers `answer` (`None`: nobody can
    /// be asked).
    pub fn run<T>(&mut self, answer: Option<bool>, f: impl FnOnce(&mut Console<'_>) -> T) -> T {
        self.run_asking(|_| Ok(answer), f)
    }

    /// Run `f` with a console whose questions `ask` answers.
    pub fn run_asking<T>(
        &mut self,
        mut ask: impl FnMut(&str) -> io::Result<Option<bool>>,
        f: impl FnOnce(&mut Console<'_>) -> T,
    ) -> T {
        let (out, err, asked) = (&mut self.out, &mut self.err, &mut self.asked);
        let mut out = |text: &str| out.push_str(text);
        let mut err = |text: &str| err.push_str(text);
        let mut ask = |question: &str| {
            asked.push(question.to_string());
            ask(question)
        };
        f(&mut Console {
            out: &mut out,
            err: &mut err,
            ask: &mut ask,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apvm_sites::{Server, Site};

    /// A refused server: two problems, with record locations.
    fn refused_server() -> Error {
        let mut server = Server::new("eu-1", "203.0.113.10");
        server.ssh.port = Some(0);
        server.ssh.identity_file = Some("~/my key".into());
        match Inventory::new().add_server(server) {
            Err(error) => error,
            Ok(()) => panic!("port 0 and a key path with a space are refused"),
        }
    }

    #[test]
    fn name_flags_renames_mapped_locations_and_keeps_the_rest() {
        let flags: &FlagMap = &[("ssh.port", "--port"), ("wp_cli", "--wp-cli")];
        assert_eq!(flag_location("ssh.port", flags), "--port");
        assert_eq!(flag_location("wp_cli[1]", flags), "--wp-cli[1]");
        assert_eq!(flag_location("wp_cli.x", flags), "--wp-cli.x");
        // A longer field that merely starts like a mapped one is not it.
        assert_eq!(flag_location("ssh.portal", flags), "ssh.portal");
        assert_eq!(flag_location("host", flags), "host");
        assert_eq!(flag_location("", flags), "");
    }

    #[test]
    fn name_flags_rewrites_only_invalid_input() {
        let flags: &FlagMap = &[("ssh.port", "--port"), ("ssh.identity_file", "--key")];
        let Failure::Sites(error) = name_flags(refused_server(), flags) else {
            panic!("stays a library error");
        };
        let locations: Vec<&str> = error
            .problems()
            .iter()
            .map(|p| p.location.as_str())
            .collect();
        assert_eq!(locations, ["--port", "--key"]);
        assert_eq!(error.code(), Code::InvalidInput);

        let other = Error::NotFound {
            kind: EntityKind::Server,
            id: "x".into(),
        };
        let Failure::Sites(kept) = name_flags(other, flags) else {
            panic!("stays a library error");
        };
        assert_eq!(kept.to_string(), "server 'x' does not exist");
    }

    #[test]
    fn invalid_input_lists_one_problem_per_line() {
        let failure = name_flags(refused_server(), &[("ssh.port", "--port")]);
        let text = failure.to_string();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("invalid server 'eu-1':"));
        assert!(
            lines.next().is_some_and(|l| l.starts_with("  --port: ")),
            "{text}"
        );
        assert!(
            lines
                .next()
                .is_some_and(|l| l.starts_with("  ssh.identity_file: ")),
            "{text}"
        );
        assert_eq!(lines.next(), None, "{text}");
    }

    #[test]
    fn errors_without_problems_render_as_is_with_their_remedy() {
        let mut inventory = Inventory::new();
        inventory
            .add_server(Server::new("eu-1", "h"))
            .expect("valid server");
        inventory
            .add_site(Site::new("shop", "eu-1", "/var/www/shop"))
            .expect("valid site");
        let in_use = inventory.remove_server("eu-1", false).expect_err("in use");
        assert_eq!(
            Failure::from(in_use).to_string(),
            "server 'eu-1' is used by 1 site(s): 'shop'\n  \
             pass --cascade to remove those sites with it"
        );
        assert_eq!(
            not_found(EntityKind::Server, "nope").to_string(),
            "server 'nope' does not exist\n  'apvm server list' shows the servers"
        );
        let taken = inventory
            .add_server(Server::new("eu-1", "h"))
            .expect_err("taken");
        assert_eq!(
            Failure::from(taken).to_string(),
            "server 'eu-1' already exists\n  \
             change it with 'apvm server edit', or choose another id"
        );
    }

    #[test]
    fn unknown_ids_name_where_to_find_the_records() {
        assert_eq!(
            not_found(EntityKind::Site, "x").to_string(),
            "site 'x' does not exist\n  'apvm site list' shows the sites"
        );
        assert_eq!(
            not_found(EntityKind::Group, "qa").to_string(),
            "group 'qa' does not exist\n  'apvm group list' shows the groups"
        );
    }

    #[test]
    fn with_remedy_adds_a_line_to_a_remedied_failure() {
        let failure = not_found(EntityKind::Site, "x")
            .with_remedy("first".into())
            .with_remedy("second".into());
        assert!(
            failure.to_string().ends_with("\n  first\n  second"),
            "{failure}"
        );
        let usage = Failure::Usage("u".into()).with_remedy("r".into());
        assert_eq!(usage.to_string(), "u", "only library refusals get one");
    }

    #[test]
    fn an_invalid_file_lists_its_problems_and_says_how_to_fix_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("inventory.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "servers": [{"id": "a", "host": "-x"}, {"id": "a", "host": "h"}]}"#,
        )
        .expect("write");
        let failure = Failure::from(inventory_io::load(&path).expect_err("invalid"));
        assert_eq!(failure.code(), "inventory_invalid");
        let text = failure.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].ends_with("inventory.json: invalid inventory:"),
            "{text}"
        );
        assert!(lines[1].starts_with("  servers[0].host: "), "{text}");
        assert!(
            lines[2].starts_with("  servers[1].id: duplicate id 'a'"),
            "{text}"
        );
        assert!(lines[3].starts_with("  fix the file by hand"), "{text}");
        assert_eq!(lines.len(), 4, "{text}");
    }

    #[test]
    fn listed_falls_back_when_the_message_has_another_shape() {
        // A newer, unreadable inventory: its message holds the problems in
        // the middle, so it is shown on one line, unchanged.
        let text = r#"{"version": 2, "servers": [{"id": "-bad", "host": "h"}]}"#;
        let error = Inventory::from_json(text).expect_err("unsupported");
        assert!(!error.problems().is_empty());
        assert_eq!(listed(&error), None);
        assert_eq!(render(&error), error.to_string());
    }

    #[test]
    fn every_failure_has_a_stable_code() {
        let json_error = match serde_json::from_str::<u8>("x") {
            Err(error) => error,
            Ok(_) => panic!("not JSON"),
        };
        let cases = [
            (Failure::Usage("x".into()), "invalid_input"),
            (
                Failure::ConfirmationRequired("x".into()),
                "confirmation_required",
            ),
            (
                Failure::Prompt(io::Error::other("x")),
                "confirmation_required",
            ),
            (Failure::Changed("x".into()), "inventory_changed"),
            (Failure::Json(json_error), "internal_error"),
            (not_found(EntityKind::Server, "x"), "invalid_input"),
        ];
        for (failure, code) in cases {
            assert_eq!(failure.code(), code, "{failure}");
        }
        assert_eq!(
            Failure::Prompt(io::Error::other("closed")).to_string(),
            "cannot ask for confirmation: closed"
        );
    }

    #[test]
    fn failures_render_as_json_with_code_message_and_problems() {
        let failure = name_flags(refused_server(), &[("ssh.port", "--port")]);
        let json: serde_json::Value =
            serde_json::from_str(&failure.to_json().expect("serializes")).expect("JSON");
        assert_eq!(json["schema"], 1);
        assert_eq!(json["error"]["code"], "invalid_input");
        let message = json["error"]["message"].as_str().expect("message");
        assert!(
            message.starts_with("invalid server 'eu-1': --port: "),
            "{message}"
        );
        assert!(!message.contains('\n'), "one line: {message}");
        assert_eq!(json["error"]["problems"][0]["location"], "--port");
        assert_eq!(
            json["error"]["problems"][1]["location"],
            "ssh.identity_file"
        );

        let usage = Failure::Usage("pass one".into())
            .to_json()
            .expect("serializes");
        let json: serde_json::Value = serde_json::from_str(&usage).expect("JSON");
        assert_eq!(json["error"]["message"], "pass one");
        assert_eq!(json["error"]["problems"], serde_json::json!([]));
    }

    #[test]
    fn confirm_change_skips_the_question_with_yes() {
        let mut recorder = Recorder::new();
        let go = recorder.run(Some(false), |console| {
            confirm_change(console, true, "Remove?", || unreachable!("never asked"))
        });
        assert!(go.expect("confirmed"));
        assert!(recorder.asked.is_empty());
    }

    #[test]
    fn confirm_change_asks_and_returns_the_answer() {
        for answer in [true, false] {
            let mut recorder = Recorder::new();
            let go = recorder.run(Some(answer), |console| {
                confirm_change(console, false, "Remove x?", String::new)
            });
            assert_eq!(go.expect("answered"), answer);
            assert_eq!(recorder.asked, ["Remove x? [y/N]"]);
        }
    }

    #[test]
    fn confirm_change_refuses_when_nobody_can_be_asked() {
        let mut recorder = Recorder::new();
        let refused = recorder.run(None, |console| {
            confirm_change(console, false, "Remove?", || "pass -y".to_string())
        });
        match refused {
            Err(Failure::ConfirmationRequired(message)) => assert_eq!(message, "pass -y"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn confirm_change_reports_a_failed_prompt() {
        let mut recorder = Recorder::new();
        let result = recorder.run_asking(
            |_| Err(io::Error::other("no tty")),
            |console| confirm_change(console, false, "Remove?", String::new),
        );
        assert!(matches!(result, Err(Failure::Prompt(_))));
    }

    #[test]
    fn load_warns_about_a_read_only_inventory_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("inventory.json");
        let mut recorder = Recorder::new();
        let empty = recorder.run(None, |console| load(&path, console));
        assert_eq!(empty.expect("missing = empty"), Inventory::new());
        assert_eq!(recorder.err, "");

        std::fs::write(&path, r#"{"version": 7}"#).expect("write");
        let newer = recorder.run(None, |console| load(&path, console));
        assert!(newer.expect("readable").is_read_only());
        assert!(
            recorder
                .err
                .contains("format version 7, newer than this apvm writes (1)"),
            "{}",
            recorder.err
        );
    }

    #[test]
    fn json_document_puts_the_schema_first_and_ends_with_a_newline() {
        #[derive(Serialize)]
        struct Body {
            servers: Vec<u8>,
        }
        let text = json_document(&Body { servers: vec![1] }).expect("serializes");
        assert!(text.ends_with("}\n"), "{text}");
        let json: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(json, serde_json::json!({"schema": 1, "servers": [1]}));
        assert!(text.find("schema") < text.find("servers"), "{text}");
    }

    #[test]
    fn table_aligns_columns_without_trailing_spaces() {
        let rows = vec![
            vec!["ID".to_string(), "NAME".to_string()],
            vec!["eu-1".to_string(), String::new()],
            vec!["ñ".to_string(), "Ünïcode".to_string()],
        ];
        assert_eq!(
            table("  ", &rows),
            "  ID    NAME\n  eu-1\n  ñ     Ünïcode\n"
        );
        assert_eq!(table("", &[]), "");
    }

    #[test]
    fn count_and_quoted_ids_read_naturally() {
        assert_eq!(count(0, "site"), "0 sites");
        assert_eq!(count(1, "site"), "1 site");
        assert_eq!(count(2, "group"), "2 groups");
        assert_eq!(quoted_ids(&["shop", "blog"]), "'shop', 'blog'");
        assert_eq!(quoted_ids::<&str>(&[]), "");
    }

    #[test]
    fn console_warn_writes_one_warn_line_to_the_diagnostics() {
        let mut recorder = Recorder::new();
        recorder.run(None, |console| {
            console.warn("careful");
            console.print("data\n");
        });
        assert!(recorder.err.ends_with("  careful\n"), "{:?}", recorder.err);
        assert!(recorder.err.contains("warn"), "{:?}", recorder.err);
        assert_eq!(recorder.out, "data\n");
    }
}
