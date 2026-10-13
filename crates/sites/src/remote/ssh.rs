//! The SSH transport: framed scripts run on a server through the system
//! OpenSSH client (plan D2, §6.1, P3.3).
//!
//! One call is one `ssh` process, started like every child apvm starts (the
//! caller's process scope, a tree kill when dropped or timed out; see
//! [`Transport`]):
//!
//! ```text
//! <ssh program> -F apvm-ssh.conf [-l <user>] [-p <port>] [-J <hops>] -- <host> <remote command>
//! ```
//!
//! **The generated file** (`config.rs`) carries every option apvm sets,
//! because ssh hands jump hosts only `-l`, `-p`, `-F` and `-v` of the main
//! connection (verified: with `-o` options alone, a jump host prompted for
//! its host key on a terminal despite `BatchMode`, and used
//! `~/.ssh/known_hosts` and the default key). Its options apply to the
//! server and to every jump host, and win over the user's ssh_config: no
//! prompts (`BatchMode`), host keys checked (`yes`, or `accept-new` when
//! the caller trusts new keys for this call; never `no`, plan §10 T6) and
//! recorded in apvm's known_hosts file (the inventory's, else
//! `~/.ssh/known_hosts`, never a `UserKnownHostsFile` an ssh_config names;
//! an ssh_config can still add trusted keys through `GlobalKnownHostsFile`,
//! `KnownHostsCommand` or `VerifyHostKeyDNS`, never turn the check off),
//! the inventory's key first with `IdentitiesOnly` (no agent key nobody
//! named; `IdentityFile` adds up, so an ssh_config's own keys are offered
//! after it) and never added to the user's agent (`AddKeysToAgent no`),
//! the connect timeout, keep-alives, no pseudo-terminal, no forwarding, no
//! reused connection (P3.4 adds apvm's own), and, when the client knows them, a
//! session that is apvm's command fed by apvm's stdin in the foreground,
//! never closed for being quiet (`SessionType default`, `StdinNull no`,
//! `ForkAfterAuthentication no` from OpenSSH 8.7, `ChannelTimeout none`
//! from 9.6: from an ssh_config they would hang the call, drop the script,
//! detach ssh from apvm's process tree, or kill a silent command;
//! verified). It then includes what ssh would have read: the user's
//! `~/.ssh/config` and the system-wide `ssh_config` (or, when that one does
//! not exist, its vendor copy under `/usr`, which openSUSE's ssh reads
//! then), or the inventory's `ssh_config_file` alone.
//!
//! **ssh's defaults** are read once per transport with `ssh -T -F none -G`
//! (no file read, no connection; in a private directory of its own, like
//! a call, so a bare program name finds the same ssh): its compiled-in
//! `globalknownhostsfile` names ssh's configuration directory, where the
//! system `ssh_config` is (the case for every build checked: Debian,
//! Ubuntu, Fedora, AlmaLinux, Alpine, Arch, openSUSE, nixpkgs, macOS), the
//! keywords it prints tell which of those session options the client knows
//! (an older one would refuse the file over them), and `updatehostkeys
//! true` that it turns `UpdateHostKeys` on for its own known_hosts files
//! (8.5+): when the inventory names no `known_hosts_file`, the generated
//! file ends with that default, which naming ssh's files would turn off.
//!
//! The file is private (`0600`, created exclusively in a new `0700`
//! directory with a random name) and deleted with its directory when the
//! call ends, however it ends (finished, timed out, failed, or its future
//! dropped). Only a killed apvm leaves them behind (paths and options, no
//! secret). ssh runs in that directory and gets the file's absolute path
//! when that path is shell-literal, else its bare name: for a jump host ssh
//! pastes the `-F` value unquoted into a command for the user's shell
//! (after expanding `%` tokens), where `apvm-ssh.conf` stays literal
//! whatever the directory's path holds (spaces in a Windows user name, for
//! one), and the jump host's ssh runs in the same directory (verified on
//! OpenSSH 7.6p1, 8.2p1, 9.6p1, 10.0p2 and 10.3p1). The absolute path is
//! preferred because that shell reads its startup file first (zsh's
//! `.zshenv`, fish's `config.fish`), and one that changes directory makes
//! the bare name miss (`Can't open user config file`, verified). So the
//! directory goes in the system's temporary directory when its path is
//! shell-literal, else on Unix in `/tmp` (when it is root's and sticky, so
//! the private directory stays private there); only a configured directory
//! ([`SshOptions::with_config_dir`]), or a temporary directory whose path
//! is not shell-literal on Windows (or on a Unix system whose `/tmp` is not
//! such a directory), gets the bare name. The temporary directory itself is
//! trusted, as by every program that writes there: one that others may
//! write without the sticky bit would let them swap the private directory
//! for theirs. The
//! directory being private matters: what ssh and the commands it runs find
//! through a relative `PATH` entry (an empty one, as in `PATH=:/usr/bin`,
//! is the current directory) is resolved there, and a shared `/tmp` would
//! let another local user plant an `ssh` there (verified). The ssh
//! program's own path is pasted the same way, so it must be shell-literal
//! (the default, `ssh`, is).
//!
//! **Before anything runs**, the local files are checked (`files.rs`): the
//! key and the inventory's ssh_config file must exist; the known_hosts file
//! new keys go to must keep them (plan P3.3: a symbolic link to `/dev/null`
//! is refused); an included ssh_config must be one ssh agrees to include
//! (the system-wide one too, which ssh alone reads unchecked), and a
//! default one this user cannot read is left out, as ssh skips it.
//!
//! **ssh's environment** is apvm's, minus apvm's GitHub tokens
//! (`GITHUB_TOKEN`, `GH_TOKEN`: ssh, its ProxyCommands and `Match exec`
//! have no use for them), plus `LC_ALL=C`, so the system messages in ssh's
//! stderr are the English ones [`classify`](super::classify()) reads, and
//! `SSH_ASKPASS_REQUIRE=never`: `BatchMode` keeps ssh itself from asking,
//! and this keeps an ssh that a ProxyCommand of the user's starts from
//! opening an ssh-askpass dialog (OpenSSH 8.4+; it inherits the variable).
//! That ssh can still ask on apvm's terminal: ssh runs in apvm's session,
//! so that Ctrl+C reaches it (`apvm-process`), and `/dev/tty` stays open
//! to it.
//!
//! **Errors:** a missing ssh program is [`Error::SshMissing`]
//! (`ssh_missing`). An exit 255 is a result ([`Exit::TransportFailed`]):
//! [`classify`](super::classify()) tells why.
//!
//! [`Exit::TransportFailed`]: super::Exit::TransportFailed

mod config;
mod files;
#[cfg(all(test, unix))]
mod tests;

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::sync::OnceCell;

use self::config::{ConfigInputs, Platform};
use self::files::{ConfigSource, LocalFiles};
use super::classify::is_noise;
use super::local::{self, CANNOT_START, EnvChange, Launch};
use super::transport::{BoxFuture, Execution, Request, Transport};
use crate::error::{Error, Result};
use crate::inventory::ResolvedServer;
use crate::problem::{Problem, printable, quoted};

/// The environment changes for every ssh process: apvm's GitHub tokens
/// removed, English system messages, and no ssh-askpass dialog from ssh or
/// anything it starts (OpenSSH 8.4+; see the module docs).
const SSH_ENV: [EnvChange; 4] = [
    ("GITHUB_TOKEN", None),
    ("GH_TOKEN", None),
    ("LC_ALL", Some("C")),
    ("SSH_ASKPASS_REQUIRE", Some("never")),
];

/// How long `ssh -F none -G` may take: it reads no file and opens no
/// connection.
const DEFAULTS_TIMEOUT: Duration = Duration::from_secs(10);

/// The host name `ssh -G` evaluates while apvm reads ssh's defaults: any
/// name does (nothing is resolved); `.invalid` never exists.
const DEFAULTS_HOST: &str = "apvm-ssh-defaults.invalid";

/// The name ssh's compiled-in known_hosts file has in its configuration
/// directory, next to the system-wide `ssh_config`.
const SYSTEM_KNOWN_HOSTS: &str = "ssh_known_hosts";

/// The system-wide ssh_config's name in that directory.
const SYSTEM_CONFIG: &str = "ssh_config";

/// The generated file's name in its private directory: shell-literal, as
/// ssh pastes it into a jump host's command.
const CONFIG_NAME: &str = "apvm-ssh.conf";

/// What ssh does with a host key it does not know yet (`StrictHostKeyChecking`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostKeyPolicy {
    /// Refuse it: the call fails with `ssh_host_key_unknown` (`yes`).
    #[default]
    Strict,
    /// Record it in the known_hosts file and go on, for this call only
    /// (`accept-new`; a front end's `--trust-new-host-key`). A changed key
    /// is still refused.
    AcceptNew,
}

impl HostKeyPolicy {
    /// The `StrictHostKeyChecking` value.
    const fn option_value(self) -> &'static str {
        match self {
            Self::Strict => "yes",
            Self::AcceptNew => "accept-new",
        }
    }
}

/// How an [`SshTransport`] runs ssh, beyond the inventory's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshOptions {
    /// What ssh does with an unknown host key.
    host_keys: HostKeyPolicy,
    /// The home directory for `~/` and the default files; `None`: the
    /// `HOME` apvm runs with.
    home: Option<PathBuf>,
    /// Where each call's private directory (and its generated ssh_config)
    /// goes; `None`: the system's temporary directory (or `/tmp`, see the
    /// module docs).
    config_dir: Option<PathBuf>,
}

impl SshOptions {
    /// Strict host keys, the `HOME` apvm runs with, the system's temporary
    /// directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set what ssh does with an unknown host key.
    ///
    /// # Arguments
    ///
    /// * `host_keys` - The policy
    pub fn with_host_keys(mut self, host_keys: HostKeyPolicy) -> Self {
        self.host_keys = host_keys;
        self
    }

    /// Set the home directory that `~/` and the default files
    /// (`~/.ssh/known_hosts`, `~/.ssh/config`) are under (an absolute
    /// path; a relative one counts as no home).
    ///
    /// # Arguments
    ///
    /// * `home` - The directory
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Set where the generated ssh_config files go (the system's temporary
    /// directory otherwise, or `/tmp` when that one's path is not
    /// shell-literal). Each call creates a private directory in it, where
    /// ssh runs, and so do its jump hosts' commands and the ProxyCommands
    /// and `Match exec` commands of the user's ssh_config. A path that is
    /// not shell-literal (see the module docs) gives ssh the file's bare
    /// name, which a shell startup file that changes directory breaks for
    /// jump hosts.
    ///
    /// # Arguments
    ///
    /// * `dir` - An existing directory
    pub fn with_config_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config_dir = Some(dir.into());
        self
    }

    /// What ssh does with an unknown host key.
    pub fn host_keys(&self) -> HostKeyPolicy {
        self.host_keys
    }
}

/// Runs framed scripts on one server over SSH (see the module docs).
///
/// # Example
///
/// ```no_run
/// use std::time::Duration;
///
/// use apvm_sites::remote::{
///     FileMode, Identity, Nonce, Request, Script, SshOptions, SshTransport, Transport, WpCli,
///     classify,
/// };
/// use apvm_sites::{Inventory, Server};
///
/// # async fn demo() -> apvm_sites::Result<()> {
/// let mut inventory = Inventory::new();
/// inventory.add_server(Server::new("eu-1", "203.0.113.10"))?;
/// let ssh = SshTransport::new(inventory.resolve_server("eu-1")?, SshOptions::new());
///
/// let wp = WpCli::new(&["wp".into()], FileMode::new(0o755)?, "/var/www/shop")?
///     .args(["cli", "version"])?;
/// let request = Request::script(Script::wp(Nonce::random(), &Identity::Login, &wp),
///                               Duration::from_secs(300));
/// let execution = ssh.execute(&request).await?;
/// if let Some(failure) = classify(&execution) {
///     eprintln!("{} ({})", failure, failure.code());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SshTransport {
    /// The server and its effective SSH settings.
    server: ResolvedServer,
    /// How ssh is run.
    options: SshOptions,
    /// ssh's compiled-in defaults, once read, and the program they are
    /// of.
    defaults: OnceCell<(PathBuf, SshDefaults)>,
}

/// What `ssh -T -F none -G` tells about the client (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SshDefaults {
    /// The system-wide ssh_config, next to the compiled-in
    /// `globalknownhostsfile` (`None` when ssh named none apvm can use).
    system_config: Option<PathBuf>,
    /// The options apvm pins that this client knows (`SessionType` …,
    /// `config.rs`), by the keywords `-G` printed.
    pinned: Vec<&'static str>,
    /// Whether the client turns `UpdateHostKeys` on when no known_hosts
    /// file is named (`updatehostkeys true`: 8.5+).
    updates_host_keys: bool,
}

/// What a call runs: the program and the text of its generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Prepared {
    /// The ssh program.
    program: PathBuf,
    /// The generated ssh_config.
    config: String,
}

impl SshTransport {
    /// A transport to `server`.
    ///
    /// # Arguments
    ///
    /// * `server` - The server and its effective SSH settings (from
    ///   [`Inventory::resolve_server`](crate::Inventory::resolve_server) or
    ///   a [`ResolvedSite`](crate::ResolvedSite)'s `server`)
    /// * `options` - How ssh is run
    pub fn new(server: ResolvedServer, options: SshOptions) -> Self {
        Self {
            server,
            options,
            defaults: OnceCell::new(),
        }
    }

    /// The server this transport reaches.
    pub fn server(&self) -> &ResolvedServer {
        &self.server
    }

    /// How ssh is run.
    pub fn options(&self) -> &SshOptions {
        &self.options
    }

    /// The ssh_config text the next call writes, for logs and diagnostics:
    /// the same checks run first.
    ///
    /// # Errors
    ///
    /// As [`Transport::execute`] before it starts ssh.
    pub async fn config_text(&self) -> Result<String> {
        Ok(self.prepare().await?.config)
    }

    /// Resolve and check the local files, find the system ssh_config when
    /// it is included, and render the generated file.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] for an unusable local file or setting;
    /// [`Error::SshMissing`] or [`Error::SshUnusable`] when ssh's defaults
    /// cannot be read (or name no system ssh_config when one is included);
    /// [`Error::SshConfigFile`] when the directory to read them in cannot
    /// be made.
    async fn prepare(&self) -> Result<Prepared> {
        let cwd = std::env::current_dir().map_err(|error| {
            self.refused(vec![Problem::new(
                "",
                format!("the current directory cannot be read: {error}"),
            )])
        })?;
        // A relative (or empty) home would make `~/.ssh/known_hosts` relative
        // to the directory ssh runs in: no home is known then.
        let home = self
            .options
            .home
            .clone()
            .or_else(std::env::home_dir)
            .filter(|home| home.is_absolute());
        let files = LocalFiles::resolve(&self.server.ssh, home.as_deref(), &cwd)
            .map_err(|problems| self.refused(problems))?;
        files::check(&files, self.options.host_keys).map_err(|problems| self.refused(problems))?;
        config::shell_literal(&files.program.path, Platform::CURRENT)
            .map_err(|reason| self.refused(vec![Problem::new("binary", reason)]))?;
        let config = self.render(&files).await?;
        Ok(Prepared {
            program: files.program.path,
            config,
        })
    }

    /// The generated file's text for `files`.
    ///
    /// # Arguments
    ///
    /// * `files` - The resolved, checked files
    ///
    /// # Errors
    ///
    /// As [`Self::prepare`].
    async fn render(&self, files: &LocalFiles) -> Result<String> {
        let defaults = self.defaults(&files.program.path).await?;
        let includes = self.includes(files, &defaults)?;
        let known_hosts: Vec<&Path> = files.known_hosts.iter().map(|f| f.path.as_path()).collect();
        let includes: Vec<&Path> = includes.iter().map(PathBuf::as_path).collect();
        let inputs = ConfigInputs {
            host_keys: self.options.host_keys,
            known_hosts: &known_hosts,
            identity: files.identity.as_ref().map(|key| key.path.as_path()),
            connect_timeout_secs: self.server.ssh.connect_timeout_secs.value,
            pinned: &defaults.pinned,
            includes: &includes,
            // ssh's own files, so ssh's own default (see `config.rs`).
            update_host_keys: defaults.updates_host_keys
                && self.server.ssh.known_hosts_file.is_none(),
        };
        config::render(&inputs, Platform::CURRENT)
            .map_err(|(field, reason)| self.refused(vec![Problem::new(field, reason)]))
    }

    /// The ssh_config files the generated file includes: the inventory's
    /// `ssh_config_file`, else those ssh reads without `-F`
    /// ([`files::default_includes`]).
    ///
    /// # Arguments
    ///
    /// * `files` - The resolved, checked files
    /// * `defaults` - The ssh program's defaults
    ///
    /// # Errors
    ///
    /// [`Error::SshUnusable`] when ssh named no system ssh_config;
    /// [`Error::InvalidInput`] when ssh would refuse to include it.
    fn includes(&self, files: &LocalFiles, defaults: &SshDefaults) -> Result<Vec<PathBuf>> {
        let user = match &files.config {
            ConfigSource::Inventory(file) => return Ok(vec![file.path.clone()]),
            ConfigSource::Default { user } => user,
        };
        let system = defaults.system_config.as_deref().ok_or_else(|| {
            unusable(
                &files.program.path,
                "'-F none -G' named no 'globalknownhostsfile …/ssh_known_hosts' (apvm needs \
                 OpenSSH 7.6 or later)",
            )
        })?;
        files::default_includes(user, system).map_err(|problem| self.refused(vec![problem]))
    }

    /// The compiled-in defaults of `program`, read once per transport (a
    /// failure is not kept: the next call tries again). A program other
    /// than the one first read (a relative `binary` after apvm changed
    /// directory) is read again, uncached.
    ///
    /// # Arguments
    ///
    /// * `program` - The ssh program
    ///
    /// # Errors
    ///
    /// [`Error::SshMissing`] or [`Error::SshUnusable`];
    /// [`Error::SshConfigFile`] when its private directory cannot be made.
    async fn defaults(&self, program: &Path) -> Result<Cow<'_, SshDefaults>> {
        let dir = self.config_dir();
        let (read_for, defaults) = self
            .defaults
            .get_or_try_init(|| async {
                Ok::<_, Error>((program.to_path_buf(), ssh_defaults(program, &dir).await?))
            })
            .await?;
        if read_for == program {
            return Ok(Cow::Borrowed(defaults));
        }
        Ok(Cow::Owned(ssh_defaults(program, &dir).await?))
    }

    /// Where each call's private directory goes ([`private_base`]).
    fn config_dir(&self) -> PathBuf {
        private_base(
            self.options.config_dir.as_deref(),
            std::env::temp_dir(),
            shared_tmp,
        )
    }

    /// An [`Error::InvalidInput`] about this server's SSH settings.
    ///
    /// # Arguments
    ///
    /// * `problems` - What is wrong, located by field
    fn refused(&self, problems: Vec<Problem>) -> Error {
        Error::InvalidInput {
            subject: format!("SSH settings of server {}", quoted(&self.server.id)),
            problems,
        }
    }

    /// Run `request` (see the module docs).
    ///
    /// # Arguments
    ///
    /// * `request` - The call
    ///
    /// # Errors
    ///
    /// As [`Transport::execute`].
    async fn call(&self, request: &Request) -> Result<Execution> {
        let prepared = self.prepare().await?;
        let dir = self.config_dir();
        let private =
            write_config(&dir, &prepared.config).map_err(|source| config_file(&dir, source))?;
        let file = config_argument(&private.path().join(CONFIG_NAME));
        let args = config::arguments(&file, &self.server, &request.remote_command());
        let launch = Launch {
            program: prepared.program.as_os_str(),
            args: &args,
            env: &SSH_ENV,
            dir: Some(private.path()),
        };
        let outcome = local::run(&launch, request).await;
        // Deleted once ssh and every jump host it started are gone.
        drop(private);
        outcome.map_err(|error| ssh_missing(error, &prepared.program))
    }
}

impl Transport for SshTransport {
    /// Run `request` on the server over SSH.
    fn execute<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Execution>> {
        Box::pin(self.call(request))
    }
}

/// Write `text` to [`CONFIG_NAME`] in a new private directory in `dir`:
/// `apvm-ssh-<random>` (`0700` on Unix, created exclusively), the file
/// `0600` on Unix, created exclusively. Both are deleted when the returned
/// directory is dropped.
///
/// # Arguments
///
/// * `dir` - An existing directory
/// * `text` - The ssh_config text
///
/// # Errors
///
/// When the directory or the file cannot be created or written (what was
/// created is deleted).
fn write_config(dir: &Path, text: &str) -> std::io::Result<tempfile::TempDir> {
    let private = private_dir(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(private.path().join(CONFIG_NAME))?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    // Closed before ssh opens it (Windows shares no open file by default).
    drop(file);
    Ok(private)
}

/// The `-F` value for the generated `file`: its path when that is absolute
/// and shell-literal, else its bare name (ssh runs in its directory; see the
/// module docs).
///
/// # Arguments
///
/// * `file` - The generated file, in its private directory
fn config_argument(file: &Path) -> PathBuf {
    if file.is_absolute() && config::shell_literal(file, Platform::CURRENT).is_ok() {
        return file.to_path_buf();
    }
    PathBuf::from(CONFIG_NAME)
}

/// The directory each call's private directory goes in: `configured`, when
/// set; else `temp` (the system's temporary directory) when its path is
/// absolute and shell-literal, so ssh gets the generated file's absolute
/// path (see the module docs); else the directory `shared` names, if any
/// (Unix `/tmp`, [`shared_tmp`]); else `temp` (ssh then gets the bare name).
///
/// # Arguments
///
/// * `configured` - [`SshOptions::with_config_dir`]'s directory
/// * `temp` - The system's temporary directory
/// * `shared` - Finds the shell-literal shared directory, when `temp` is not
///   usable
fn private_base(
    configured: Option<&Path>,
    temp: PathBuf,
    shared: impl FnOnce() -> Option<PathBuf>,
) -> PathBuf {
    if let Some(dir) = configured {
        return dir.to_path_buf();
    }
    if temp.is_absolute() && config::shell_literal(&temp, Platform::CURRENT).is_ok() {
        return temp;
    }
    shared().unwrap_or(temp)
}

/// `/tmp` when a private directory in it stays private: a directory that
/// belongs to root, with the sticky bit, so no other user can rename or
/// remove what this one creates there (Unix; `None` elsewhere).
fn shared_tmp() -> Option<PathBuf> {
    let tmp = Path::new("/tmp");
    sticky_root_dir(tmp).then(|| tmp.to_path_buf())
}

/// Whether `dir` is a directory of root's with the sticky bit (links
/// followed); never on Windows.
///
/// # Arguments
///
/// * `dir` - The directory
#[cfg(unix)]
fn sticky_root_dir(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir).is_ok_and(|metadata| {
        metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o1000 != 0
    })
}

/// Whether `dir` is a sticky directory of root's: never on Windows.
///
/// # Arguments
///
/// * `_dir` - The directory
#[cfg(not(unix))]
fn sticky_root_dir(_dir: &Path) -> bool {
    false
}

/// A new, empty private directory in `dir`: `apvm-ssh-<random>`, created
/// exclusively (`0700` on Unix), deleted with what it holds when dropped.
/// ssh runs in one, so nothing is found there through a relative `PATH`
/// entry (see the module docs).
///
/// # Arguments
///
/// * `dir` - An existing directory
///
/// # Errors
///
/// When it cannot be created.
fn private_dir(dir: &Path) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("apvm-ssh-");
    #[cfg(unix)]
    builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
    builder.tempdir_in(dir)
}

/// The [`Error::SshConfigFile`] for a private directory or file that could
/// not be created in `dir`.
///
/// # Arguments
///
/// * `dir` - The directory
/// * `source` - Why
fn config_file(dir: &Path, source: std::io::Error) -> Error {
    Error::SshConfigFile {
        dir: dir.to_path_buf(),
        source,
    }
}

/// `error` as [`Error::SshMissing`] when `program` could not be started
/// because it is missing or not executable; any other error as it is.
///
/// # Arguments
///
/// * `error` - A transport error
/// * `program` - The ssh program
fn ssh_missing(error: Error, program: &Path) -> Error {
    match error {
        Error::Process {
            context, source, ..
        } if context == CANNOT_START && is_missing(&source) => Error::SshMissing {
            program: program.display().to_string(),
            source,
        },
        other => other,
    }
}

/// Whether a start error means the program is missing or not executable.
///
/// # Arguments
///
/// * `error` - The start error
fn is_missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    )
}

/// Read `program`'s compiled-in defaults: `<program> -T -F none -G <host>`
/// prints them, reading no file and opening no connection. Its
/// `globalknownhostsfile` (`/etc/ssh/ssh_known_hosts …`) is in the
/// directory that holds the system `ssh_config` (verified on OpenSSH
/// 7.6p1, 8.2p1, 9.6p1, 10.0p2 and 10.3p1, and in the builds of Debian,
/// Ubuntu, Fedora, AlmaLinux 8 and 9, Alpine, Arch, openSUSE, nixpkgs and
/// macOS, audit 5).
///
/// It runs like a call, in a new empty private directory in `dir`, so a
/// bare program name resolves to the same ssh the calls run (a relative
/// `PATH` entry would otherwise find a program in apvm's directory).
///
/// # Arguments
///
/// * `program` - The ssh program
/// * `dir` - Where the private directory goes
///
/// # Errors
///
/// [`Error::SshConfigFile`] when the directory cannot be created;
/// [`Error::SshMissing`] when ssh cannot be started; [`Error::SshUnusable`]
/// when it fails or hangs.
async fn ssh_defaults(program: &Path, dir: &Path) -> Result<SshDefaults> {
    let private = private_dir(dir).map_err(|source| config_file(dir, source))?;
    let mut command = defaults_command(program, private.path());
    let output = tokio::time::timeout(DEFAULTS_TIMEOUT, apvm_process::output(&mut command))
        .await
        .map_err(|_| {
            let secs = DEFAULTS_TIMEOUT.as_secs();
            unusable(
                program,
                &format!("'-F none -G' did not answer within {secs} s"),
            )
        })?
        .map_err(|source| started(program, source))?;
    if !output.status.success() {
        return Err(defaults_failed(program, &output));
    }
    Ok(parse_defaults(&String::from_utf8_lossy(&output.stdout)))
}

/// The `-T -F none -G` command for `program`, run in `cwd` with ssh's
/// environment and no stdin. `-T` (no pseudo-terminal, 7.6p1–10.6p1) keeps
/// ssh from printing `Pseudo-terminal will not be allocated because stdin
/// is not a terminal.` on stderr, which it does before `-G` prints when no
/// command is given (seen on 8.0p1, 9.6p1, 10.0p2, 10.3p1 and 10.6p1, and
/// in the sources from 7.6p1; `-T` changes only `requesttty`, which apvm
/// does not read).
///
/// # Arguments
///
/// * `program` - The ssh program
/// * `cwd` - The (private) directory it runs in
fn defaults_command(program: &Path, cwd: &Path) -> tokio::process::Command {
    let mut command = apvm_process::command(program);
    command
        .args(["-T", "-F", "none", "-G", DEFAULTS_HOST])
        .stdin(Stdio::null())
        .current_dir(cwd);
    local::apply_env(&mut command, &SSH_ENV);
    command
}

/// The defaults `ssh -F none -G` printed (see [`SshDefaults`]).
///
/// # Arguments
///
/// * `output` - Its stdout
fn parse_defaults(output: &str) -> SshDefaults {
    let keywords: Vec<&str> = output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    SshDefaults {
        system_config: system_config_path(output),
        pinned: config::pinned_options(|keyword| keywords.contains(&keyword)),
        updates_host_keys: output.lines().any(|line| line == "updatehostkeys true"),
    }
}

/// The [`Error::SshUnusable`] for a `-F none -G` run that failed: its
/// status and the first line it printed on stderr that is not noise (as
/// [`classify`](super::classify()) reads it: never `Pseudo-terminal will not
/// be allocated because stdin is not a terminal.`, which ssh prints there
/// when no command is given and stdin is not a terminal).
///
/// # Arguments
///
/// * `program` - The ssh program
/// * `output` - What the run left
fn defaults_failed(program: &Path, output: &std::process::Output) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_noise(line))
        .map(|line| format!(": {}", printable(line)))
        .unwrap_or_default();
    unusable(
        program,
        &format!("'-F none -G' failed ({}){said}", output.status),
    )
}

/// The error for an ssh program that could not be started.
///
/// # Arguments
///
/// * `program` - The program
/// * `source` - Why
fn started(program: &Path, source: std::io::Error) -> Error {
    if is_missing(&source) {
        return Error::SshMissing {
            program: program.display().to_string(),
            source,
        };
    }
    Error::Process {
        context: CANNOT_START,
        program: program.display().to_string(),
        source,
    }
}

/// An [`Error::SshUnusable`] for `program`.
///
/// # Arguments
///
/// * `program` - The program
/// * `reason` - What went wrong
fn unusable(program: &Path, reason: &str) -> Error {
    Error::SshUnusable {
        program: program.display().to_string(),
        reason: reason.to_string(),
    }
}

/// The system ssh_config named by `ssh -G` output: next to the first
/// `globalknownhostsfile` path, when that path ends in `ssh_known_hosts`.
///
/// # Arguments
///
/// * `output` - `ssh -F none -G` stdout
fn system_config_path(output: &str) -> Option<PathBuf> {
    let files = output
        .lines()
        .find_map(|line| line.strip_prefix("globalknownhostsfile "))?;
    let first = Path::new(files.split_whitespace().next()?);
    if first.file_name() != Some(OsStr::new(SYSTEM_KNOWN_HOSTS)) {
        return None;
    }
    let dir = first.parent().filter(|dir| dir.is_absolute())?;
    Some(dir.join(SYSTEM_CONFIG))
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::inventory::{Defaults, Inventory, Server, SshSettings};

    /// The effective settings of server `s` (host `example.com`) with
    /// `ssh`, under `defaults`.
    fn resolved(defaults: SshSettings, ssh: SshSettings) -> ResolvedServer {
        let mut inventory = Inventory::new();
        inventory.set_defaults(Defaults { ssh: defaults }).unwrap();
        let mut server = Server::new("s", "example.com");
        server.ssh = ssh;
        inventory.add_server(server).unwrap();
        inventory.resolve_server("s").unwrap()
    }

    /// The argv after the program, as text.
    fn argv(server: &ResolvedServer) -> Vec<String> {
        config::arguments(Path::new("/tmp/apvm-ssh-x.conf"), server, "sh -s")
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn the_argv_carries_only_what_belongs_to_the_server_across_every_level() {
        let none = SshSettings::default();
        let user = |u: &str| SshSettings {
            user: Some(u.into()),
            ..SshSettings::default()
        };
        let port = |p: u16| SshSettings {
            port: Some(p),
            ..SshSettings::default()
        };
        let jump = |j: &str| SshSettings {
            proxy_jump: Some(j.into()),
            ..SshSettings::default()
        };
        let all = SshSettings {
            user: Some("deploy".into()),
            port: Some(2222),
            proxy_jump: Some("bastion@jump.example:2200,10.0.0.1".into()),
            ..SshSettings::default()
        };
        let base = ["-F", "/tmp/apvm-ssh-x.conf"];
        let tail = ["--", "example.com", "sh -s"];
        let cases: Vec<(SshSettings, SshSettings, Vec<&str>)> = vec![
            // Nothing set: ssh_config (or ssh) decides user and port.
            (none.clone(), none.clone(), vec![]),
            // Defaults apply when the server sets nothing.
            (user("ubuntu"), none.clone(), vec!["-l", "ubuntu"]),
            (port(2200), none.clone(), vec!["-p", "2200"]),
            (jump("bastion"), none.clone(), vec!["-J", "bastion"]),
            // The server wins field by field.
            (user("ubuntu"), user("deploy"), vec!["-l", "deploy"]),
            (port(2200), port(22), vec!["-p", "22"]),
            (jump("a"), jump("b,c:2222"), vec!["-J", "b,c:2222"]),
            (
                SshSettings {
                    user: Some("ubuntu".into()),
                    ..port(2200)
                },
                jump("j"),
                vec!["-l", "ubuntu", "-p", "2200", "-J", "j"],
            ),
            (
                none.clone(),
                all,
                vec![
                    "-l",
                    "deploy",
                    "-p",
                    "2222",
                    "-J",
                    "bastion@jump.example:2200,10.0.0.1",
                ],
            ),
        ];
        for (defaults, server, middle) in cases {
            let expected: Vec<&str> = base.iter().chain(&middle).chain(&tail).copied().collect();
            assert_eq!(argv(&resolved(defaults, server)), expected);
        }
    }

    #[test]
    fn options_ssh_must_hand_to_jump_hosts_never_go_on_the_command_line() {
        let server = resolved(
            SshSettings {
                identity_file: Some("/k".into()),
                known_hosts_file: Some("/kh".into()),
                ssh_config_file: Some("/c".into()),
                connect_timeout_secs: Some(9),
                binary: Some("/usr/bin/ssh".into()),
                multiplex: Some(true),
                ..SshSettings::default()
            },
            SshSettings::default(),
        );
        let args = argv(&server);
        assert_eq!(
            args,
            ["-F", "/tmp/apvm-ssh-x.conf", "--", "example.com", "sh -s"]
        );
    }

    #[test]
    fn an_upload_command_is_one_argument_after_the_host() {
        let server = resolved(SshSettings::default(), SshSettings::default());
        let command = "sh -c '{ LC_ALL=C; export LC_ALL; tee -- /tmp/apvm.x/p >/dev/null; }'";
        let args = config::arguments(Path::new("/c"), &server, command);
        assert_eq!(args.last().map(|a| a.to_str()), Some(Some(command)));
        assert_eq!(args.len(), 5);
    }

    #[test]
    fn windows_argv_and_paths() {
        // The argv is the same on Windows; the program and the file follow
        // Windows paths (and the file holds forward slashes).
        let server = resolved(
            SshSettings {
                user: Some("deploy".into()),
                binary: Some(r"C:\Windows\System32\OpenSSH\ssh.exe".into()),
                ..SshSettings::default()
            },
            SshSettings::default(),
        );
        let file = Path::new(r"C:\Users\Jane\AppData\Local\Temp\apvm-ssh-AbC123.conf");
        let args: Vec<String> = config::arguments(file, &server, "sh -s")
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-F",
                r"C:\Users\Jane\AppData\Local\Temp\apvm-ssh-AbC123.conf",
                "-l",
                "deploy",
                "--",
                "example.com",
                "sh -s"
            ]
        );
        assert!(config::shell_literal(file, Platform::Windows).is_ok());
        assert!(
            config::shell_literal(Path::new(&server.ssh.binary.value), Platform::Windows).is_ok()
        );
        let spaced = Path::new(r"C:\Users\Jane Doe\AppData\Local\Temp");
        assert!(config::shell_literal(spaced, Platform::Windows).is_err());
    }

    #[test]
    fn the_host_key_policy_is_strict_unless_new_keys_are_trusted() {
        assert_eq!(HostKeyPolicy::default(), HostKeyPolicy::Strict);
        assert_eq!(HostKeyPolicy::Strict.option_value(), "yes");
        assert_eq!(HostKeyPolicy::AcceptNew.option_value(), "accept-new");
        let options = SshOptions::new().with_host_keys(HostKeyPolicy::AcceptNew);
        assert_eq!(options.host_keys(), HostKeyPolicy::AcceptNew);
        assert_eq!(SshOptions::new().host_keys(), HostKeyPolicy::Strict);
    }

    #[cfg(unix)]
    #[test]
    fn the_system_config_is_next_to_the_compiled_in_known_hosts() {
        // Real `ssh -F none -G` lines (OpenSSH 10.3p1; 9.6p1 and 10.0p2
        // print the same).
        let output = "user sandy\nport 22\n\
                      globalknownhostsfile /etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2\n\
                      userknownhostsfile /Users/sandy/.ssh/known_hosts /Users/sandy/.ssh/known_hosts2\n";
        assert_eq!(
            system_config_path(output),
            Some(PathBuf::from("/etc/ssh/ssh_config"))
        );
        // Another build's directory (Homebrew's, not sampled).
        let brew = "globalknownhostsfile /opt/homebrew/etc/ssh/ssh_known_hosts\n";
        assert_eq!(
            system_config_path(brew),
            Some(PathBuf::from("/opt/homebrew/etc/ssh/ssh_config"))
        );
        for odd in [
            "",
            "userknownhostsfile /etc/ssh/ssh_known_hosts\n",
            "globalknownhostsfile\n",
            "globalknownhostsfile none\n",
            "globalknownhostsfile /etc/ssh/other_hosts\n",
            "globalknownhostsfile ssh_known_hosts\n",
            "globalknownhostsfile relative/ssh_known_hosts\n",
        ] {
            assert_eq!(system_config_path(odd), None, "{odd:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_system_config_is_next_to_the_compiled_in_known_hosts_on_windows() {
        // The shape Win32-OpenSSH's defaults would have (not sampled).
        let output = "globalknownhostsfile C:\\ProgramData\\ssh/ssh_known_hosts\n";
        assert_eq!(
            system_config_path(output),
            Some(PathBuf::from(r"C:\ProgramData\ssh\ssh_config"))
        );
        assert_eq!(
            system_config_path("globalknownhostsfile /etc/ssh/ssh_known_hosts\n"),
            None
        );
    }

    #[test]
    fn ssh_gets_the_environment_minus_apvms_tokens_with_english_messages_and_no_askpass() {
        let mut command = apvm_process::command("ssh");
        local::apply_env(&mut command, &SSH_ENV);
        let mut changes: Vec<(String, Option<String>)> = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        changes.sort();
        assert_eq!(
            changes,
            [
                ("GH_TOKEN".to_string(), None),
                ("GITHUB_TOKEN".to_string(), None),
                ("LC_ALL".to_string(), Some("C".to_string())),
                ("SSH_ASKPASS_REQUIRE".to_string(), Some("never".to_string())),
            ]
        );
    }

    #[test]
    fn ssh_gets_the_files_path_when_a_shell_keeps_it_literal_else_its_name() {
        #[cfg(unix)]
        let literal = Path::new("/var/folders/2x/ab_c-1/T/apvm-ssh-AbC123/apvm-ssh.conf");
        #[cfg(windows)]
        let literal = Path::new(r"C:\Users\Jane\AppData\Local\Temp\apvm-ssh-AbC\apvm-ssh.conf");
        assert_eq!(config_argument(literal), literal);
        for other in [
            "/home/Jane Doe/tmp/apvm-ssh-x/apvm-ssh.conf",
            "/tmp/a$(x)/apvm-ssh-x/apvm-ssh.conf",
            "/tmp/a%h/apvm-ssh-x/apvm-ssh.conf",
            "tmp/apvm-ssh-x/apvm-ssh.conf",
            // A Windows short name (`~` is special in a shell).
            r"C:\Users\JANEDO~1\AppData\Local\Temp\apvm-ssh-x\apvm-ssh.conf",
        ] {
            assert_eq!(
                config_argument(Path::new(other)),
                Path::new(CONFIG_NAME),
                "{other}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_private_directory_goes_where_its_path_stays_shell_literal() {
        let not_asked = || -> Option<PathBuf> { panic!("the shared directory was not needed") };
        let shared = || Some(PathBuf::from("/tmp"));
        // A configured directory is used as it is (ssh may get the bare name).
        assert_eq!(
            private_base(Some(Path::new("/a b")), "/var/t".into(), not_asked),
            Path::new("/a b")
        );
        // A shell-literal temporary directory (macOS's, Linux's).
        for temp in ["/var/folders/2x/ab_c-1/T/", "/tmp", "/run/user/1000"] {
            assert_eq!(private_base(None, temp.into(), not_asked), Path::new(temp));
        }
        // Otherwise `/tmp`, so ssh still gets an absolute path.
        for temp in [
            "/home/Jane Doe/tmp",
            "/tmp/a$(x)",
            "/tmp/caf\u{e9}",
            "tmp",
            "",
        ] {
            assert_eq!(
                private_base(None, temp.into(), shared),
                Path::new("/tmp"),
                "{temp}"
            );
            // … when it is safe to share; else the temporary directory.
            assert_eq!(private_base(None, temp.into(), || None), Path::new(temp));
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_a_sticky_directory_of_roots_is_shared() {
        use std::os::unix::fs::PermissionsExt;
        // macOS (through its link to /private/tmp), Linux, containers.
        assert!(sticky_root_dir(Path::new("/tmp")));
        assert_eq!(shared_tmp(), Some(PathBuf::from("/tmp")));
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o1777)).unwrap();
        // Not root's (unless root runs the test).
        assert_eq!(sticky_root_dir(&open), rustix::process::geteuid().is_root());
        std::fs::write(dir.path().join("file"), "").unwrap();
        assert!(!sticky_root_dir(&dir.path().join("file")));
        assert!(!sticky_root_dir(&dir.path().join("missing")));
        // Root's, not sticky.
        assert!(!sticky_root_dir(Path::new("/")));
    }

    #[test]
    fn the_defaults_are_read_in_the_given_directory_with_sshs_environment() {
        // A bare `ssh` then resolves as it does for the calls, which run in
        // a private directory too.
        let command = defaults_command(Path::new("ssh"), Path::new("/private/x"));
        let command = command.as_std();
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["-T", "-F", "none", "-G", DEFAULTS_HOST]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/private/x")));
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "LC_ALL" && value == Some(OsStr::new("C")))
        );
    }

    #[test]
    fn only_a_missing_or_unexecutable_program_is_ssh_missing() {
        use std::io::{Error as IoError, ErrorKind};
        let program = Path::new("/opt/ssh");
        for kind in [ErrorKind::NotFound, ErrorKind::PermissionDenied] {
            let error = Error::Process {
                context: CANNOT_START,
                program: "/opt/ssh".into(),
                source: IoError::from(kind),
            };
            let mapped = ssh_missing(error, program);
            assert_eq!(mapped.code(), crate::Code::SshMissing, "{kind:?}");
            assert!(
                mapped
                    .to_string()
                    .starts_with("cannot run the ssh program '/opt/ssh': ")
            );
        }
        for error in [
            Error::Process {
                context: CANNOT_START,
                program: "/opt/ssh".into(),
                source: IoError::from(ErrorKind::OutOfMemory),
            },
            Error::Process {
                context: "cannot read the output of",
                program: "/opt/ssh".into(),
                source: IoError::from(ErrorKind::NotFound),
            },
        ] {
            assert_eq!(
                ssh_missing(error, program).code(),
                crate::Code::ProcessFailed
            );
        }
        assert_eq!(
            started(program, IoError::from(ErrorKind::NotFound)).code(),
            crate::Code::SshMissing
        );
        assert_eq!(
            started(program, IoError::other("x")).code(),
            crate::Code::ProcessFailed
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_generated_file_is_private_shell_literal_and_deleted_with_its_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Any directory: ssh runs in a private one inside it and gets the
        // bare name.
        let spaced = dir.path().join("a b$(x)");
        std::fs::create_dir(&spaced).unwrap();
        let private = write_config(&spaced, "BatchMode yes\n").unwrap();
        let name = private.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("apvm-ssh-"), "{name}");
        assert_eq!(private.path().parent(), Some(spaced.as_path()));
        assert_eq!(
            config::shell_literal(Path::new(CONFIG_NAME), Platform::Unix),
            Ok(CONFIG_NAME.to_string())
        );
        // Nothing but the file, so nothing can be found there through a
        // relative PATH entry.
        let entries: Vec<_> = std::fs::read_dir(private.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, [CONFIG_NAME]);
        let file = private.path().join(CONFIG_NAME);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "BatchMode yes\n");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(private.path()), 0o700);
        // Two calls never share a directory.
        let other = write_config(&spaced, "x").unwrap();
        assert_ne!(other.path(), private.path());
        let kept = private.path().to_path_buf();
        drop(private);
        assert!(!kept.exists());
        assert!(write_config(&dir.path().join("missing"), "x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_defaults_run_shows_its_first_line_that_is_not_noise() {
        use std::os::unix::process::ExitStatusExt;
        let output = |stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(255 << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        // What ssh prints before `-G` without `-T` (seen on 8.0p1–10.6p1),
        // then why it failed.
        let stderr = "Pseudo-terminal will not be allocated because stdin is not a terminal.\r\n\
                      /x: line 1: Bad configuration option: foo\r\n";
        let error = defaults_failed(Path::new("ssh"), &output(stderr));
        assert_eq!(
            error.to_string(),
            "the ssh program 'ssh' is not a usable OpenSSH client: '-F none -G' failed (exit \
             status: 255): /x: line 1: Bad configuration option: foo"
        );
        // Noise alone, or nothing: the status only.
        for stderr in [
            "Pseudo-terminal will not be allocated because stdin is not a terminal.\r\n",
            "",
        ] {
            let error = defaults_failed(Path::new("ssh"), &output(stderr));
            assert!(
                error.to_string().ends_with("failed (exit status: 255)"),
                "{error}"
            );
        }
    }

    #[test]
    fn the_defaults_tell_the_system_config_and_the_options_the_client_knows() {
        // 10.3p1 prints every pinned option's keyword, and turns
        // UpdateHostKeys on without a named known_hosts file.
        let modern = "user u\nsessiontype default\nstdinnull no\nforkafterauthentication no\n\
                      channeltimeout none\nupdatehostkeys true\n\
                      globalknownhostsfile /etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2\n";
        let defaults = parse_defaults(modern);
        assert!(defaults.updates_host_keys);
        // 8.2p1 prints `updatehostkeys false`.
        assert!(!parse_defaults("updatehostkeys false\n").updates_host_keys);
        assert!(!parse_defaults("x updatehostkeys true\n").updates_host_keys);
        assert_eq!(
            defaults.pinned,
            [
                "SessionType default",
                "StdinNull no",
                "ForkAfterAuthentication no",
                "ChannelTimeout none",
            ]
        );
        // A Unix path names no directory on Windows (that case has its own
        // test).
        #[cfg(unix)]
        assert_eq!(
            defaults.system_config,
            Some(PathBuf::from("/etc/ssh/ssh_config"))
        );
        // 7.6p1 and 8.2p1 print none of them (and would refuse them in a
        // file: `Bad configuration option: sessiontype`).
        let old =
            "user u\nglobalknownhostsfile /etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2\n";
        assert!(parse_defaults(old).pinned.is_empty());
        // Whole keywords at the start of a line only.
        for odd in [
            "sessiontypes default\n",
            "x sessiontype\n",
            "user sessiontype\n",
        ] {
            assert!(parse_defaults(odd).pinned.is_empty(), "{odd:?}");
        }
        // Each option on its own (a client between 8.7 and 9.6).
        let partial = "sessiontype default\nstdinnull no\n";
        assert_eq!(
            parse_defaults(partial).pinned,
            ["SessionType default", "StdinNull no"]
        );
        assert_eq!(parse_defaults("").system_config, None);
    }
}
