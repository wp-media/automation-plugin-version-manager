//! Error types and their stable codes.
//!
//! Every fallible operation returns [`Result<T>`]. [`Error`] and [`Code`] are
//! `#[non_exhaustive]`: later phases add variants, so consumers keep a
//! wildcard arm. Messages name no front-end remedy (no CLI flag, no Node
//! option); each front end adds its own.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::inventory::EntityKind;
use crate::inventory::io::FileKind;
use crate::package::PackageDefect;
use crate::problem::{Problem, printable, quoted, quoted_path};
use crate::remote::{Exit, SshFailure};
use crate::wp::{OutputDefect, ProbeFailure};

/// Convenient result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Stable, machine-readable error codes (plan §4.5), carried by the CLI's
/// `--json` output and by the Node bindings' `code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Code {
    /// A value handed to the library breaks a rule (`invalid_input`).
    InvalidInput,
    /// The inventory document cannot be parsed or breaks a rule
    /// (`inventory_invalid`).
    InventoryInvalid,
    /// The inventory was written by a newer apvm, in a form this one cannot
    /// read (`inventory_unsupported`).
    InventoryUnsupported,
    /// The inventory was written by a newer apvm: this one may read it but
    /// never change it (`inventory_read_only`).
    InventoryReadOnly,
    /// The inventory file, its lock file or its directory cannot be read,
    /// written, locked or created, or is not a regular file
    /// (`inventory_io`).
    InventoryIo,
    /// Another process held the inventory's lock for the whole wait limit
    /// (`inventory_locked`); nothing was written.
    InventoryLocked,
    /// The inventory file changed after it was loaded, so a guarded save
    /// wrote nothing (`inventory_changed`).
    InventoryChanged,
    /// A plugin zip breaks an inspection rule (`package_invalid`).
    PackageInvalid,
    /// A local program a transport runs (`ssh`, or a shell) could not be
    /// started or talked to, or the file an upload streams could not be
    /// opened or read for a reason other than the caller's path
    /// (`process_failed`).
    ProcessFailed,
    /// A WP-CLI command failed, or printed output apvm cannot read with
    /// certainty (`wp_command_failed`).
    WpCommandFailed,
    /// The ssh program cannot be run, or is not an OpenSSH client apvm can
    /// use (`ssh_missing`).
    SshMissing,
    /// The server refused the SSH login (`ssh_auth_failed`).
    SshAuthFailed,
    /// No host key is known for the server (or a jump host), and new ones
    /// are not trusted on this call (`ssh_host_key_unknown`).
    SshHostKeyUnknown,
    /// A server's (or jump host's) host key differs from the known one
    /// (`ssh_host_key_changed`).
    SshHostKeyChanged,
    /// The server cannot be reached over SSH (`ssh_unreachable`).
    SshUnreachable,
    /// The SSH connection or handshake timed out (`ssh_timeout`).
    SshTimeout,
    /// The SSH connection was lost after it was made; the remote command
    /// may have run in part (`ssh_connection_lost`).
    SshConnectionLost,
    /// ssh failed for a reason apvm does not recognize, or refused a host
    /// key for a reason no other code names (an invalid host certificate;
    /// `ssh_failed`).
    SshFailed,
    /// Running as the site's identity needs sudo, and the server has none
    /// (`sudo_missing`).
    SudoMissing,
    /// sudo wants a password for a command apvm needs (`sudo_password_required`).
    SudoPasswordRequired,
    /// No sudo rule lets the login user run a command apvm needs
    /// (`sudo_not_allowed`).
    SudoNotAllowed,
    /// sudo requires a terminal, and apvm runs without one (`sudo_requiretty`).
    SudoRequiretty,
    /// Root would own the plugin files or run WP-CLI, and the site does not
    /// allow it (`owner_is_root`).
    OwnerIsRoot,
    /// The plugins directory WordPress uses is not the one apvm measured,
    /// or not the site's explicit owner's (`plugins_dir_mismatch`).
    PluginsDirMismatch,
    /// The site's `wp_cli` cannot be run, or is not WP-CLI (`wp_cli_missing`).
    WpCliMissing,
    /// The site's path holds no WordPress (`not_wordpress`).
    NotWordpress,
    /// WordPress is there but not installed: no `wp-config.php`, or no
    /// tables (`wp_not_installed`).
    WpNotInstalled,
    /// The identity cannot write the plugins directory
    /// (`plugins_dir_not_writable`).
    PluginsDirNotWritable,
    /// A remote command ran past its timeout and was killed (`timeout`).
    Timeout,
    /// A probe step printed something apvm cannot read with certainty
    /// (`probe_failed`).
    ProbeFailed,
}

impl Code {
    /// The stable `snake_case` code string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::InventoryInvalid => "inventory_invalid",
            Self::InventoryUnsupported => "inventory_unsupported",
            Self::InventoryReadOnly => "inventory_read_only",
            Self::InventoryIo => "inventory_io",
            Self::InventoryLocked => "inventory_locked",
            Self::InventoryChanged => "inventory_changed",
            Self::PackageInvalid => "package_invalid",
            Self::ProcessFailed => "process_failed",
            Self::WpCommandFailed => "wp_command_failed",
            Self::SshMissing => "ssh_missing",
            Self::SshAuthFailed => "ssh_auth_failed",
            Self::SshHostKeyUnknown => "ssh_host_key_unknown",
            Self::SshHostKeyChanged => "ssh_host_key_changed",
            Self::SshUnreachable => "ssh_unreachable",
            Self::SshTimeout => "ssh_timeout",
            Self::SshConnectionLost => "ssh_connection_lost",
            Self::SshFailed => "ssh_failed",
            Self::SudoMissing => "sudo_missing",
            Self::SudoPasswordRequired => "sudo_password_required",
            Self::SudoNotAllowed => "sudo_not_allowed",
            Self::SudoRequiretty => "sudo_requiretty",
            Self::OwnerIsRoot => "owner_is_root",
            Self::PluginsDirMismatch => "plugins_dir_mismatch",
            Self::WpCliMissing => "wp_cli_missing",
            Self::NotWordpress => "not_wordpress",
            Self::WpNotInstalled => "wp_not_installed",
            Self::PluginsDirNotWritable => "plugins_dir_not_writable",
            Self::Timeout => "timeout",
            Self::ProbeFailed => "probe_failed",
        }
    }
}

impl std::fmt::Display for Code {
    /// The stable code string ([`Code::as_str`]).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// All errors the sites crate can produce.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The inventory text is not JSON of the inventory's shape (syntax, a
    /// wrong type, a missing or unknown field). serde_json's message says
    /// where (line and column).
    #[error("{context}: {}", printable(&source.to_string()))]
    InventoryJson {
        /// What was being done (`"cannot parse the inventory"`, …).
        context: &'static str,
        /// The underlying serde_json error.
        #[source]
        source: serde_json::Error,
    },

    /// The inventory parsed but breaks one or more rules. Every problem is
    /// listed with its location, such as `sites[1].server`.
    #[error("invalid inventory: {}", Problem::join(problems))]
    InventoryInvalid {
        /// Every problem found, in document order.
        problems: Vec<Problem>,
    },

    /// The inventory has a newer format version, and this build cannot read
    /// it: it has fields or values this build does not know, or breaks
    /// rules this build applies. Ignoring what it does not understand could
    /// drop a setting that restricts what apvm does, so it is refused.
    #[error(
        "the inventory has format version {found}, newer than this apvm supports \
         ({supported}), and cannot be read: {source}; upgrade apvm"
    )]
    InventoryUnsupported {
        /// Version recorded in the document.
        found: u32,
        /// Newest version this build reads and writes.
        supported: u32,
        /// Why it could not be read ([`Error::InventoryJson`] or
        /// [`Error::InventoryInvalid`]); [`Error::problems`] returns its
        /// problems.
        #[source]
        source: Box<Error>,
    },

    /// A change was attempted on an inventory with a newer format version.
    /// It can be read, but writing it would drop what the newer version
    /// means, so it is read-only.
    #[error(
        "the inventory has format version {found}, newer than this apvm writes \
         ({supported}); it is read-only: upgrade apvm to change it"
    )]
    InventoryReadOnly {
        /// Version recorded in the document.
        found: u32,
        /// Newest version this build reads and writes.
        supported: u32,
    },

    /// A value handed to the library breaks one or more rules: a record
    /// (or the defaults) handed to the inventory, in which case nothing was
    /// changed, or a value refused while building a remote script (an
    /// account name, a path, a WP-CLI token or argument, a nonce, a file
    /// mode, a root command's owners).
    #[error("invalid {subject}: {}", Problem::join(problems))]
    InvalidInput {
        /// What was rejected, such as `site 'shop'`, `defaults` or
        /// `WP-CLI command`.
        subject: String,
        /// Every problem found; locations are relative to the record.
        problems: Vec<Problem>,
    },

    /// A record with this id already exists.
    #[error("{kind} {} already exists", quoted(id))]
    AlreadyExists {
        /// The kind of record.
        kind: EntityKind,
        /// The id that is taken.
        id: String,
    },

    /// No record has this id.
    #[error("{kind} {} does not exist", quoted(id))]
    NotFound {
        /// The kind of record.
        kind: EntityKind,
        /// The id that was not found.
        id: String,
    },

    /// A server cannot be removed while sites use it (unless the removal
    /// cascades to them).
    #[error(
        "server {} is used by {} site(s): {}",
        quoted(id),
        sites.len(),
        sites.iter().map(|site| quoted(site)).collect::<Vec<_>>().join(", ")
    )]
    ServerInUse {
        /// The server id.
        id: String,
        /// Ids of the sites on it, in inventory order.
        sites: Vec<String>,
    },

    /// Reading or writing the inventory file failed: the file, its lock
    /// file or its directory could not be read, written, locked or created.
    #[error("{context} {}: {source}", shown(path))]
    InventoryIo {
        /// What was being done (`"cannot read the inventory"`, …).
        context: &'static str,
        /// The file or directory involved (the inventory, its lock file or
        /// its directory).
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Another process held the inventory's lock for the whole wait limit
    /// ([`crate::inventory::io::LOCK_WAIT`]), so nothing was written. Edits
    /// hold the lock for milliseconds: a holder this slow is stuck.
    #[error(
        "cannot lock the inventory with {}: another process held it for {}",
        shown(path),
        span(*waited)
    )]
    InventoryLocked {
        /// The lock file.
        path: PathBuf,
        /// How long apvm waited.
        waited: Duration,
    },

    /// The inventory file changed after it was loaded (it was saved again,
    /// deleted, or created where it was missing), so a guarded save
    /// ([`crate::inventory::io::save_if_unchanged`]) wrote nothing: writing
    /// would have discarded that change.
    #[error(
        "the inventory {} changed after it was loaded; nothing was written",
        shown(path)
    )]
    InventoryChanged {
        /// The inventory file.
        path: PathBuf,
    },

    /// The inventory path, or its lock file, is not a regular file. A
    /// symbolic link is never followed: it could make apvm read or replace
    /// a file elsewhere.
    #[error("refusing {}: it is {found}, not a regular file", shown(path))]
    InventoryNotAFile {
        /// The refused path.
        path: PathBuf,
        /// What is there instead.
        found: FileKind,
    },

    /// The inventory file's content could not be read: `source` says why
    /// ([`Error::InventoryJson`], [`Error::InventoryInvalid`] or
    /// [`Error::InventoryUnsupported`]), and [`Error::code`] and
    /// [`Error::problems`] are those of `source`.
    #[error("{}: {source}", shown(path))]
    InventoryFile {
        /// The inventory file.
        path: PathBuf,
        /// Why its content was refused.
        #[source]
        source: Box<Error>,
    },

    /// Plugin hints break one or more rules.
    #[error("invalid plugin hints: {}", Problem::join(problems))]
    InvalidHints {
        /// Every problem found, with locations such as
        /// `projects[0].plugins[1].slug`.
        problems: Vec<Problem>,
    },

    /// A plugin zip breaks a rule of package inspection (plan §6.10). The
    /// path is shown as the platform displays it (control characters
    /// escaped), not escaped like an id, which would double Windows
    /// backslashes.
    #[error("invalid plugin package '{}': {defect}", printable(path))]
    PackageInvalid {
        /// The zip file, as given.
        path: String,
        /// The rule it breaks.
        #[source]
        defect: PackageDefect,
    },

    /// A local program a transport runs (`ssh`, or a shell) could not be
    /// started, or reading its output or waiting for it failed. When it
    /// had started, it was killed along with every process it started.
    #[error("{context} {}: {source}", quoted_path(program))]
    Process {
        /// What failed (`"cannot start"`, `"cannot read the output of"`, …).
        context: &'static str,
        /// The program, as given.
        program: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The file an upload streams to the remote command could not be opened
    /// (other than missing, not permitted, or not a regular file: those are
    /// [`Error::InvalidInput`]), or reading it failed while it was streamed;
    /// the command was then killed along with every process it started.
    #[error("cannot read the upload {}: {source}", shown(path))]
    UploadSource {
        /// The file.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A WP-CLI command failed, or its output breaks a parser rule (plan
    /// §9): never read as a guess, so noise never reads as "not
    /// installed".
    #[error("'wp {command}' {defect}")]
    WpOutput {
        /// The WP-CLI command, such as `plugin list`.
        command: &'static str,
        /// What was wrong with its output.
        #[source]
        defect: OutputDefect,
    },

    /// The ssh program could not be started: it is missing, or not
    /// executable. Nothing ran.
    #[error("cannot run the ssh program {}: {source}", quoted_path(program))]
    SshMissing {
        /// The program, as configured (`ssh`, or a path).
        program: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The ssh program ran but did not behave like an OpenSSH client apvm
    /// can use (it could not report its configuration with `-G`). Nothing
    /// was sent to a server.
    #[error(
        "the ssh program {} is not a usable OpenSSH client: {reason}",
        quoted_path(program)
    )]
    SshUnusable {
        /// The program, as configured.
        program: String,
        /// What went wrong, escaped.
        reason: String,
    },

    /// The private directory and file apvm writes for one ssh call (its
    /// generated ssh configuration, plan P3.3), or the private directory
    /// ssh's defaults are read in, could not be created or written. Nothing
    /// ran.
    #[error("cannot write the ssh configuration file in {}: {source}", shown(dir))]
    SshConfigFile {
        /// The directory it was written to.
        dir: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// ssh itself failed (it exited 255): the class
    /// ([`crate::remote::SshFailureKind`]) and the stderr line that shows
    /// it. Built by callers from [`crate::remote::classify`].
    #[error("{0}")]
    Ssh(SshFailure),

    /// A remote command ran past its timeout: the transport and everything
    /// it started were killed. When the script had started, it may have run
    /// in part (re-read the state after a mutation).
    #[error(
        "the remote command ran past its {} timeout and was killed{}",
        span(*after),
        if *started { "; it may have run in part" } else { "" }
    )]
    Timeout {
        /// The timeout.
        after: Duration,
        /// Whether the script had started (its nonce line came back).
        started: bool,
    },

    /// The local transport process ended without an exit status, killed by
    /// something other than the call's timeout (a process scope's teardown,
    /// a user's `kill`).
    #[error("the local transport process was {exit} before the remote command ended")]
    Killed {
        /// How it ended.
        exit: Exit,
    },

    /// The call ended with a status, but its stdout has no nonce line: the
    /// server's login shell did not run apvm's script as sent (a forced
    /// command, a `nologin` shell, startup files that ate the script).
    #[error(
        "the server did not run apvm's script ({exit}){}",
        line.as_ref().map(|line| format!(": '{line}'")).unwrap_or_default()
    )]
    ScriptNotRun {
        /// How the call ended.
        exit: Exit,
        /// stderr's last line, escaped.
        line: Option<String>,
    },

    /// Probing a site failed (plan §6.2): the probe's failure says which
    /// step, and the stable code.
    #[error("{0}")]
    Probe(Box<ProbeFailure>),
}

impl Error {
    /// The stable code of this error.
    pub fn code(&self) -> Code {
        match self {
            Self::InventoryJson { .. } | Self::InventoryInvalid { .. } => Code::InventoryInvalid,
            Self::InventoryUnsupported { .. } => Code::InventoryUnsupported,
            Self::InventoryReadOnly { .. } => Code::InventoryReadOnly,
            Self::InventoryIo { .. } | Self::InventoryNotAFile { .. } => Code::InventoryIo,
            Self::InventoryLocked { .. } => Code::InventoryLocked,
            Self::InventoryChanged { .. } => Code::InventoryChanged,
            Self::InventoryFile { source, .. } => source.code(),
            Self::InvalidInput { .. }
            | Self::AlreadyExists { .. }
            | Self::NotFound { .. }
            | Self::ServerInUse { .. }
            | Self::InvalidHints { .. } => Code::InvalidInput,
            Self::PackageInvalid { .. } => Code::PackageInvalid,
            Self::Process { .. } | Self::UploadSource { .. } => Code::ProcessFailed,
            Self::WpOutput { .. } => Code::WpCommandFailed,
            Self::SshMissing { .. } | Self::SshUnusable { .. } => Code::SshMissing,
            Self::SshConfigFile { .. } => Code::ProcessFailed,
            Self::Ssh(failure) => failure.code(),
            Self::Timeout { .. } => Code::Timeout,
            Self::Killed { .. } => Code::ProcessFailed,
            Self::ScriptNotRun { .. } => Code::SshFailed,
            Self::Probe(failure) => failure.code(),
        }
    }

    /// The located problems this error carries, those of an unreadable newer
    /// inventory included (empty for errors without any), so front ends can
    /// list them one per line.
    pub fn problems(&self) -> &[Problem] {
        match self {
            Self::InventoryInvalid { problems }
            | Self::InvalidInput { problems, .. }
            | Self::InvalidHints { problems } => problems,
            Self::InventoryUnsupported { source, .. } | Self::InventoryFile { source, .. } => {
                source.problems()
            }
            _ => &[],
        }
    }
}

/// A path as shown in messages: as the platform displays it, with control
/// and invisible formatting characters escaped. Not quoted, so Windows
/// paths keep single backslashes.
fn shown(path: &Path) -> String {
    printable(&path.display().to_string())
}

/// A duration as shown in messages: whole seconds as `10 s`, anything
/// else in milliseconds (`150 ms`), so short test waits stay exact.
fn span(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shape that refuses unknown fields, like the inventory's records.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {}

    /// A serde_json error whose message holds a raw ESC: serde quotes an
    /// unknown field name verbatim.
    fn json_error() -> serde_json::Error {
        match serde_json::from_str::<Strict>(r#"{"a\u001bb": 1}"#) {
            Err(err) => err,
            Ok(value) => panic!("expected a parse error, got {value:?}"),
        }
    }

    fn problems() -> Vec<Problem> {
        vec![
            Problem::new("sites[1].server", "unknown server 'eu-2'"),
            Problem::new("groups[0].id", "must not be empty"),
        ]
    }

    #[test]
    fn codes_are_stable_strings() {
        assert_eq!(Code::InvalidInput.as_str(), "invalid_input");
        assert_eq!(Code::InventoryInvalid.as_str(), "inventory_invalid");
        assert_eq!(Code::InventoryUnsupported.as_str(), "inventory_unsupported");
        assert_eq!(Code::InventoryReadOnly.as_str(), "inventory_read_only");
        assert_eq!(Code::InventoryReadOnly.to_string(), "inventory_read_only");
        assert_eq!(Code::InventoryIo.as_str(), "inventory_io");
        assert_eq!(Code::InventoryLocked.as_str(), "inventory_locked");
        assert_eq!(Code::InventoryChanged.as_str(), "inventory_changed");
        assert_eq!(Code::WpCommandFailed.as_str(), "wp_command_failed");
        assert_eq!(Code::SshMissing.as_str(), "ssh_missing");
        assert_eq!(Code::SshAuthFailed.as_str(), "ssh_auth_failed");
        assert_eq!(Code::SshHostKeyUnknown.as_str(), "ssh_host_key_unknown");
        assert_eq!(Code::SshHostKeyChanged.as_str(), "ssh_host_key_changed");
        assert_eq!(Code::SshUnreachable.as_str(), "ssh_unreachable");
        assert_eq!(Code::SshTimeout.as_str(), "ssh_timeout");
        assert_eq!(Code::SshConnectionLost.as_str(), "ssh_connection_lost");
        assert_eq!(Code::SshFailed.as_str(), "ssh_failed");
        assert_eq!(Code::SudoMissing.as_str(), "sudo_missing");
        assert_eq!(
            Code::SudoPasswordRequired.as_str(),
            "sudo_password_required"
        );
        assert_eq!(Code::SudoNotAllowed.as_str(), "sudo_not_allowed");
        assert_eq!(Code::SudoRequiretty.as_str(), "sudo_requiretty");
        assert_eq!(Code::OwnerIsRoot.as_str(), "owner_is_root");
        assert_eq!(Code::PluginsDirMismatch.as_str(), "plugins_dir_mismatch");
        assert_eq!(Code::WpCliMissing.as_str(), "wp_cli_missing");
        assert_eq!(Code::NotWordpress.as_str(), "not_wordpress");
        assert_eq!(Code::WpNotInstalled.as_str(), "wp_not_installed");
        assert_eq!(
            Code::PluginsDirNotWritable.as_str(),
            "plugins_dir_not_writable"
        );
        assert_eq!(Code::Timeout.as_str(), "timeout");
        assert_eq!(Code::ProbeFailed.as_str(), "probe_failed");
    }

    #[test]
    fn ssh_errors_name_the_program_or_directory_and_map_to_their_codes() {
        let error = Error::SshMissing {
            program: "/opt/s\u{1b}sh".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert_eq!(error.code(), Code::SshMissing);
        assert!(
            error
                .to_string()
                .starts_with("cannot run the ssh program '/opt/s\\u{1b}sh': "),
            "{error}"
        );
        let error = Error::SshUnusable {
            program: "ssh".into(),
            reason: "it printed no globalknownhostsfile".into(),
        };
        assert_eq!(error.code(), Code::SshMissing);
        assert_eq!(
            error.to_string(),
            "the ssh program 'ssh' is not a usable OpenSSH client: it printed no \
             globalknownhostsfile"
        );
        let error = Error::SshConfigFile {
            dir: PathBuf::from("/tmp/a\u{1b}b"),
            source: std::io::Error::other("full"),
        };
        assert_eq!(error.code(), Code::ProcessFailed);
        assert_eq!(
            error.to_string(),
            "cannot write the ssh configuration file in /tmp/a\\u{1b}b: full"
        );
        assert!(error.problems().is_empty());
    }

    #[test]
    fn package_paths_keep_windows_backslashes_and_escape_control_characters() {
        let error = Error::PackageInvalid {
            path: "C:\\Users\\me\\a\u{1b}b.zip".into(),
            defect: PackageDefect::NotAFile,
        };
        let message = error.to_string();
        assert!(
            message.starts_with("invalid plugin package 'C:\\Users\\me\\a\\u{1b}b.zip': "),
            "{message}"
        );
        assert_eq!(error.code(), Code::PackageInvalid);
    }

    #[test]
    fn a_changed_inventory_names_the_file() {
        let error = Error::InventoryChanged {
            path: PathBuf::from("/home/me/.apvm/inventory.json"),
        };
        assert_eq!(error.code(), Code::InventoryChanged);
        assert_eq!(
            error.to_string(),
            "the inventory /home/me/.apvm/inventory.json changed after it was loaded; \
             nothing was written"
        );
        assert!(error.problems().is_empty());
    }

    #[test]
    fn a_busy_lock_names_the_lock_file_and_the_wait() {
        let error = Error::InventoryLocked {
            path: PathBuf::from("/home/me/.apvm/inventory.json.lock"),
            waited: Duration::from_secs(10),
        };
        assert_eq!(error.code(), Code::InventoryLocked);
        assert_eq!(
            error.to_string(),
            "cannot lock the inventory with /home/me/.apvm/inventory.json.lock: \
             another process held it for 10 s"
        );
        assert!(error.problems().is_empty());
    }

    #[test]
    fn spans_read_in_seconds_or_milliseconds() {
        assert_eq!(span(Duration::from_secs(10)), "10 s");
        assert_eq!(span(Duration::from_millis(1500)), "1500 ms");
        assert_eq!(span(Duration::from_millis(150)), "150 ms");
        assert_eq!(span(Duration::ZERO), "0 s");
        assert_eq!(
            span(Duration::MAX),
            format!("{} ms", Duration::MAX.as_millis())
        );
    }

    #[test]
    fn file_errors_name_their_path_and_map_to_their_codes() {
        let path = PathBuf::from("/home/me/.apvm/inventory.json");
        let error = Error::InventoryIo {
            context: "cannot read the inventory",
            path: path.clone(),
            source: std::io::Error::other("boom"),
        };
        assert_eq!(error.code(), Code::InventoryIo);
        assert_eq!(
            error.to_string(),
            "cannot read the inventory /home/me/.apvm/inventory.json: boom"
        );
        assert!(error.problems().is_empty());

        let error = Error::InventoryNotAFile {
            path: path.clone(),
            found: FileKind::Symlink,
        };
        assert_eq!(error.code(), Code::InventoryIo);
        assert_eq!(
            error.to_string(),
            "refusing /home/me/.apvm/inventory.json: it is a symbolic link, not a regular file"
        );

        // A content error keeps the code and problems of its source.
        let error = Error::InventoryFile {
            path,
            source: Box::new(Error::InventoryUnsupported {
                found: 2,
                supported: 1,
                source: Box::new(Error::InventoryInvalid {
                    problems: problems(),
                }),
            }),
        };
        assert_eq!(error.code(), Code::InventoryUnsupported);
        assert_eq!(error.problems(), problems().as_slice());
        assert!(
            error
                .to_string()
                .starts_with("/home/me/.apvm/inventory.json: the inventory has format version 2"),
            "{error}"
        );
    }

    #[test]
    fn paths_in_messages_escape_control_characters() {
        let error = Error::InventoryNotAFile {
            path: PathBuf::from("/tmp/a\u{1b}[2Jb"),
            found: FileKind::Directory,
        };
        assert_eq!(
            error.to_string(),
            "refusing /tmp/a\\u{1b}[2Jb: it is a directory, not a regular file"
        );
    }

    #[test]
    fn remote_and_probe_errors_map_to_their_codes() {
        let cases = [
            (
                Error::Timeout {
                    after: Duration::from_secs(300),
                    started: false,
                },
                Code::Timeout,
            ),
            (
                Error::Killed {
                    exit: Exit::Killed { signal: None },
                },
                Code::ProcessFailed,
            ),
            (
                Error::ScriptNotRun {
                    exit: Exit::Status(1),
                    line: None,
                },
                Code::SshFailed,
            ),
            (
                Error::Probe(Box::new(ProbeFailure::WpNotInstalled {
                    path: "/var/www".into(),
                    message: None,
                })),
                Code::WpNotInstalled,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
        }
        assert_eq!(
            Error::Killed {
                exit: Exit::Killed { signal: None }
            }
            .to_string(),
            "the local transport process was killed before the remote command ended"
        );
        assert_eq!(
            Error::ScriptNotRun {
                exit: Exit::Status(1),
                line: None
            }
            .to_string(),
            "the server did not run apvm's script (exit status 1)"
        );
    }

    #[test]
    fn every_error_maps_to_its_code() {
        let cases = [
            (
                Error::InventoryJson {
                    context: "cannot parse the inventory",
                    source: json_error(),
                },
                Code::InventoryInvalid,
            ),
            (
                Error::InventoryInvalid {
                    problems: problems(),
                },
                Code::InventoryInvalid,
            ),
            (
                Error::InventoryUnsupported {
                    found: 2,
                    supported: 1,
                    source: Box::new(Error::InventoryInvalid {
                        problems: problems(),
                    }),
                },
                Code::InventoryUnsupported,
            ),
            (
                Error::InventoryReadOnly {
                    found: 2,
                    supported: 1,
                },
                Code::InventoryReadOnly,
            ),
            (
                Error::InvalidInput {
                    subject: "defaults".into(),
                    problems: problems(),
                },
                Code::InvalidInput,
            ),
            (
                Error::AlreadyExists {
                    kind: EntityKind::Site,
                    id: "shop".into(),
                },
                Code::InvalidInput,
            ),
            (
                Error::NotFound {
                    kind: EntityKind::Group,
                    id: "qa".into(),
                },
                Code::InvalidInput,
            ),
            (
                Error::ServerInUse {
                    id: "eu-1".into(),
                    sites: vec!["shop".into()],
                },
                Code::InvalidInput,
            ),
            (
                Error::InvalidHints {
                    problems: problems(),
                },
                Code::InvalidInput,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
        }
    }

    #[test]
    fn messages_list_every_problem_with_its_location() {
        let error = Error::InventoryInvalid {
            problems: problems(),
        };
        assert_eq!(
            error.to_string(),
            "invalid inventory: sites[1].server: unknown server 'eu-2'; \
             groups[0].id: must not be empty"
        );
        assert_eq!(error.problems(), problems().as_slice());

        let error = Error::InvalidInput {
            subject: "site 'shop'".into(),
            problems: vec![Problem::new("path", "must not be empty")],
        };
        assert_eq!(
            error.to_string(),
            "invalid site 'shop': path: must not be empty"
        );
    }

    #[test]
    fn errors_without_problems_return_an_empty_list() {
        let error = Error::NotFound {
            kind: EntityKind::Server,
            id: "eu-1".into(),
        };
        assert!(error.problems().is_empty());
        assert_eq!(error.to_string(), "server 'eu-1' does not exist");
    }

    #[test]
    fn messages_escape_terminal_control_characters() {
        // Ids come from callers and documents: an escape sequence must never
        // reach the terminal raw.
        let error = Error::AlreadyExists {
            kind: EntityKind::Site,
            id: "a\u{1b}[2Jb".into(),
        };
        assert_eq!(error.to_string(), "site 'a\\u{1b}[2Jb' already exists");

        let error = Error::ServerInUse {
            id: "eu-1".into(),
            sites: vec!["shop".into(), "blog".into()],
        };
        assert_eq!(
            error.to_string(),
            "server 'eu-1' is used by 2 site(s): 'shop', 'blog'"
        );

        let source = json_error();
        assert!(
            source.to_string().contains('\u{1b}'),
            "the test needs a raw ESC"
        );
        let error = Error::InventoryJson {
            context: "cannot parse the inventory",
            source,
        };
        let message = error.to_string();
        assert!(
            message.starts_with("cannot parse the inventory: "),
            "{message}"
        );
        assert!(message.contains("a\\u{1b}b"), "{message}");
        assert!(!message.contains('\u{1b}'), "{message}");
    }

    #[test]
    fn process_errors_name_the_program_or_file_and_map_to_process_failed() {
        assert_eq!(Code::ProcessFailed.as_str(), "process_failed");
        let error = Error::Process {
            context: "cannot start",
            program: "s\u{1b}h".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert_eq!(error.code(), Code::ProcessFailed);
        assert!(
            error.to_string().starts_with("cannot start 's\\u{1b}h': "),
            "{error}"
        );
        assert!(error.problems().is_empty());

        let error = Error::UploadSource {
            path: PathBuf::from("/tmp/a\u{1b}b.zip"),
            source: std::io::Error::other("boom"),
        };
        assert_eq!(error.code(), Code::ProcessFailed);
        assert_eq!(
            error.to_string(),
            "cannot read the upload /tmp/a\\u{1b}b.zip: boom"
        );
    }

    #[test]
    fn version_errors_explain_the_remedy() {
        let error = Error::InventoryReadOnly {
            found: 2,
            supported: 1,
        };
        assert!(error.to_string().contains("read-only"));
        assert!(error.to_string().contains("upgrade apvm"));
        let error = Error::InventoryUnsupported {
            found: 3,
            supported: 1,
            source: Box::new(Error::InventoryInvalid {
                problems: problems(),
            }),
        };
        assert_eq!(
            error.to_string(),
            "the inventory has format version 3, newer than this apvm supports (1), and \
             cannot be read: invalid inventory: sites[1].server: unknown server 'eu-2'; \
             groups[0].id: must not be empty; upgrade apvm"
        );
        // The problems stay listable.
        assert_eq!(error.problems(), problems().as_slice());
    }
}
