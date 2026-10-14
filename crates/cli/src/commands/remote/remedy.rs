//! What the user can do about a failed test, by its code and, for ssh's
//! own failures, by the line ssh printed (plan §3.5 "Classification
//! table", P3.3's obligations). `apvm-sites` messages name no front-end
//! remedy; these name the CLI's flags and commands.
//!
//! Host keys are never trusted on the user's behalf: an unknown key needs
//! `--trust-new-host-key` after checking it; a changed key needs
//! `ssh-keygen -R` once the new key is confirmed, never for a revoked one
//! (it would drop the revocation); and a key refused for another reason
//! (an invalid host certificate) never gets `--trust-new-host-key`, which
//! would bypass the certificate authority.

use apvm_sites::remote::{SshFailure, SshFailureKind};
use apvm_sites::wp::ProbeFailure;
use apvm_sites::{Code, Error, Problem, ResolvedServer};

/// The record a test was for, which decides how a fix is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject<'a> {
    /// A server: `server test`, or `server add` (`adding`).
    Server {
        /// Whether `server add` is adding it (fixes are flags to pass).
        adding: bool,
    },
    /// A site: `site test`, or `site add` (`adding`).
    Site {
        /// The site id.
        id: &'a str,
        /// Whether `site add` is adding it (fixes are flags to pass).
        adding: bool,
    },
}

/// What a remedy needs to know besides the failure.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The server the test connected to.
    pub server: &'a ResolvedServer,
    /// The record tested.
    pub subject: Subject<'a>,
}

impl Context<'_> {
    /// How to change the server's `flags`: on the `server add` command
    /// line, or with `apvm server edit`.
    ///
    /// # Arguments
    ///
    /// * `flags` - The flags (with a value placeholder when there is one)
    fn server_fix(&self, flags: &[&str]) -> String {
        let one = flags.len() == 1;
        match self.subject {
            Subject::Server { adding: true } => format!("pass {}", or_list(flags)),
            _ if one => format!("apvm server edit {} {}", self.server.id, or_list(flags)),
            _ => format!(
                "apvm server edit {} with {}",
                self.server.id,
                or_list(flags)
            ),
        }
    }

    /// How to change the site's `flags`: on the `site add` command line,
    /// or with `apvm site edit`.
    ///
    /// # Arguments
    ///
    /// * `flags` - The flags and their values
    fn site_fix(&self, flags: &str) -> String {
        match self.subject {
            Subject::Site { adding: true, .. } => format!("pass {flags}"),
            Subject::Site { id, .. } => format!("apvm site edit {id} {flags}"),
            Subject::Server { .. } => format!("set {flags} on the site"),
        }
    }

    /// The known_hosts file new keys go to, for `ssh-keygen -f`: the
    /// server's, when it names one (`None`: ssh's own default).
    fn known_hosts(&self) -> Option<&str> {
        self.server
            .ssh
            .known_hosts_file
            .as_ref()
            .map(|setting| setting.value.as_str())
    }
}

/// `a`, `a or b`, `a, b or c`.
///
/// # Arguments
///
/// * `items` - The items
fn or_list(items: &[&str]) -> String {
    match items.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

/// What the user can do about `error`, one remedy per line (none when the
/// message says it all).
///
/// # Arguments
///
/// * `error` - Why the test failed
/// * `context` - The server and the record tested
pub fn remedies(error: &Error, context: &Context<'_>) -> Vec<String> {
    match error {
        Error::Ssh(failure) => ssh_remedies(failure, context),
        Error::Probe(failure) => probe_remedies(failure, context),
        Error::ScriptNotRun { .. } => vec![
            "the server's login shell did not run apvm's script: check for a forced command \
             (command= in authorized_keys, ForceCommand) or a login shell such as nologin"
                .into(),
        ],
        Error::InvalidInput { problems, .. } => settings_remedies(problems, context),
        _ => code_remedies(error.code(), context),
    }
}

/// Remedies by code alone.
///
/// # Arguments
///
/// * `code` - The error's code
/// * `context` - The server and the record tested
fn code_remedies(code: Code, context: &Context<'_>) -> Vec<String> {
    match code {
        Code::SshMissing => vec![
            "install an OpenSSH client, or name it: apvm server defaults --ssh-binary <PATH>"
                .into(),
        ],
        Code::Timeout => match context.subject {
            Subject::Site { .. } => vec![format!(
                "the server answered too slowly; allow more time: {}",
                context.site_fix("--timeout <SECS>")
            )],
            Subject::Server { .. } => vec!["the server answered too slowly: try again".into()],
        },
        _ => Vec::new(),
    }
}

/// Remedies for ssh's own failure (exit 255).
///
/// # Arguments
///
/// * `failure` - The class and the line ssh printed
/// * `context` - The server and the record tested
fn ssh_remedies(failure: &SshFailure, context: &Context<'_>) -> Vec<String> {
    let line = failure.line().unwrap_or_default();
    match failure.kind() {
        SshFailureKind::HostKeyUnknown => vec![
            "check the host key's fingerprint with the server's administrator, then run again \
             with --trust-new-host-key to record it"
                .into(),
            "a ProxyCommand in your ssh_config runs its own ssh, which --trust-new-host-key \
             does not reach: record that host's key with it"
                .into(),
        ],
        SshFailureKind::HostKeyChanged => changed_key(line, context),
        SshFailureKind::AuthFailed => auth_failed(line, context),
        SshFailureKind::Unreachable => vec![format!(
            "check the host name, the port and the network (and the jump hosts): {}",
            context.server_fix(&["--host", "--port", "--jump"])
        )],
        SshFailureKind::Timeout => vec![format!(
            "check the network and any firewall, or allow longer: {}",
            context.server_fix(&["--connect-timeout <SECS>"])
        )],
        SshFailureKind::ConnectionLost => {
            vec!["the connection dropped during the test: run it again".into()]
        }
        _ => unrecognized(line),
    }
}

/// Remedies for a changed (or revoked) host key.
///
/// # Arguments
///
/// * `line` - The line ssh printed
/// * `context` - The server and the record tested
fn changed_key(line: &str, context: &Context<'_>) -> Vec<String> {
    if is_revocation(line) {
        return vec![
            "the server presented a host key marked revoked: find out why before connecting \
             again, and never remove it with ssh-keygen -R (that drops the revocation)"
                .into(),
        ];
    }
    let (name, what) = match address_of(line) {
        Some(address) => (address, "the old key of its address"),
        None => (
            changed_name(line).map_or_else(|| ssh_name(context.server), str::to_string),
            "the old key",
        ),
    };
    if name.contains('\'') {
        return vec![
            "if the server's new key is genuine, remove its old key from known_hosts".into(),
        ];
    }
    let file = context
        .known_hosts()
        .map(|file| format!(" -f {file}"))
        .unwrap_or_default();
    vec![
        format!(
            "this may be an attack: confirm the server's new key with its administrator; if it \
             is genuine, remove {what}: ssh-keygen -R '{name}'{file}"
        ),
        "then run again with --trust-new-host-key to record the new key".into(),
    ]
}

/// Whether ssh's line says the key is revoked: an `@revoked` known_hosts
/// line (`… host key for X was revoked and you have requested strict
/// checking.`) or a `RevokedHostKeys` file (`Host key … revoked by file
/// …`), the classifier's phrases. Not any line holding "revoked": a host
/// named `revoked-db` whose key changed must get the `ssh-keygen -R` fix.
///
/// # Arguments
///
/// * `line` - The line ssh printed
fn is_revocation(line: &str) -> bool {
    line.contains(" was revoked and you have requested strict checking")
        || line.contains(" revoked by file ")
}

/// The address of a `Warning: the … host key for 'X' differs from the key
/// for the IP address 'A'` line (`CheckHostIP`).
///
/// # Arguments
///
/// * `line` - The line ssh printed
fn address_of(line: &str) -> Option<String> {
    let rest = line
        .split_once("differs from the key for the IP address '")?
        .1;
    Some(rest.split_once('\'')?.0.to_string())
}

/// The host of a `Host key for X has changed and you have requested
/// strict checking.` line.
///
/// # Arguments
///
/// * `line` - The line ssh printed
fn changed_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("Host key for ")?;
    Some(rest.split_once(" has changed")?.0)
}

/// The name ssh files the server's key under: the host, bracketed with
/// its port when apvm passes one other than 22.
///
/// # Arguments
///
/// * `server` - The server
fn ssh_name(server: &ResolvedServer) -> String {
    match &server.ssh.port {
        Some(port) if port.value != 22 => format!("[{}]:{}", server.host, port.value),
        _ => server.host.clone(),
    }
}

/// Remedies for a refused login.
///
/// # Arguments
///
/// * `line` - The line ssh printed
/// * `context` - The server and the record tested
fn auth_failed(line: &str, context: &Context<'_>) -> Vec<String> {
    let mut remedies = Vec::new();
    if line.ends_with("are too open.") {
        remedies.push(
            "ssh ignores a key file others can read: chmod 600 the file named above".to_string(),
        );
    } else if line.starts_with("Load key ") {
        remedies.push("ssh cannot load the key file named above: convert or replace it".into());
    }
    remedies.push(format!(
        "check the login user, the key and the server's authorized_keys: {}",
        context.server_fix(&["--user", "--key"])
    ));
    remedies
}

/// Remedies for `ssh_failed`, by the line ssh printed.
///
/// # Arguments
///
/// * `line` - The line ssh printed
fn unrecognized(line: &str) -> Vec<String> {
    const CERTIFICATE: [&str; 4] = [
        "Certificate invalid",
        "Certificate lacks principal list",
        "unsupported critical option",
        "Certificate signed with disallowed algorithm",
    ];
    if CERTIFICATE.iter().any(|phrase| line.contains(phrase)) {
        return vec![
            "ssh refused the server's host certificate: fix the certificate on the server; \
             never use --trust-new-host-key here (it would trust the plain key and bypass your \
             certificate authority)"
                .into(),
        ];
    }
    if line.starts_with("Error checking host key")
        || line.contains("not permitted by HostkeyAlgorithms")
    {
        return vec!["fix your ssh_config's RevokedHostKeys or HostKeyAlgorithms setting".into()];
    }
    if line == "Host key verification failed." {
        return vec![
            "a ProxyCommand in your ssh_config refused the host key with its own ssh: record \
             the key in that ssh's known_hosts (apvm's options do not reach it)"
                .into(),
        ];
    }
    Vec::new()
}

/// Remedies for a probe failure.
///
/// # Arguments
///
/// * `failure` - Why the probe failed
/// * `context` - The server and the record tested
fn probe_remedies(failure: &ProbeFailure, context: &Context<'_>) -> Vec<String> {
    match failure {
        ProbeFailure::RootStatNeedsSudo { .. } => vec![
            "let the login user read the plugins directory, or install sudo with a rule for \
             /usr/bin/stat as root"
                .into(),
        ],
        ProbeFailure::Sudo(_) => {
            vec!["edit the sudoers rules on the server with visudo".into()]
        }
        ProbeFailure::PluginsDirMissing { pinned: false, .. } => vec![format!(
            "check the site's WordPress root: {}",
            context.site_fix("--path <WP_ROOT>")
        )],
        ProbeFailure::PluginsDirMissing { pinned: true, .. }
        | ProbeFailure::PluginsDirNotDirectory { .. }
        | ProbeFailure::PluginsDirLinked { .. }
        | ProbeFailure::PluginsDirMismatch { .. } => vec![format!(
            "pin the plugins directory once you checked who owns it: {}",
            context.site_fix("--plugins-dir <PATH>")
        )],
        ProbeFailure::PluginsDirChanged { .. } => {
            vec!["the plugins directory changed during the test: run it again".into()]
        }
        ProbeFailure::WpCliMissing { .. } => vec![format!(
            "name the WP-CLI command: {}",
            context.site_fix("--wp-cli \"<CMD>\" (e.g. \"/usr/bin/php /opt/wp-cli.phar\")")
        )],
        ProbeFailure::NotWordpress { .. } => vec![format!(
            "check the site's WordPress root: {}",
            context.site_fix("--path <WP_ROOT>")
        )],
        ProbeFailure::WpNotInstalled { .. } => vec![format!(
            "install WordPress there (wp core install), or check the site's WordPress root: {}",
            context.site_fix("--path <WP_ROOT>")
        )],
        _ => identity_remedies(failure.code(), context),
    }
}

/// Remedies for the identity table's refusals, by code.
///
/// # Arguments
///
/// * `code` - The refusal's code
/// * `context` - The server and the record tested
fn identity_remedies(code: Code, context: &Context<'_>) -> Vec<String> {
    match code {
        Code::OwnerIsRoot => vec![format!(
            "root owns the plugins directory (or would run WP-CLI): give it to the site's \
             user, or allow root: {}",
            context.site_fix("--allow-root")
        )],
        Code::SudoMissing => vec![format!(
            "install sudo on the server, or log in as the plugins directory's owner: {}",
            context.server_fix(&["--user <USER>"])
        )],
        Code::PluginsDirMismatch => vec![format!(
            "check who owns the plugins directory, or name the owner: {}",
            context.site_fix("--owner <USER[:GROUP]>")
        )],
        _ => Vec::new(),
    }
}

/// Remedies for SSH settings the transport refused before running ssh,
/// by the field and the problem.
///
/// # Arguments
///
/// * `problems` - The located problems
/// * `context` - The server and the record tested
fn settings_remedies(problems: &[Problem], context: &Context<'_>) -> Vec<String> {
    let mut remedies = Vec::new();
    for problem in problems {
        let message = problem.message.as_str();
        let remedy = match problem.location.as_str() {
            "ssh_config_file" if message.contains("the system ssh_config") => Some(
                "set an ssh_config of your own: apvm server defaults --ssh-config <PATH>".into(),
            ),
            "ssh_config_file" if message.contains("writable by its group or others") => {
                Some("make it private: chmod go-w on the ssh_config file named above".into())
            }
            "ssh_config_file" => Some(context.server_fix(&["--ssh-config <PATH>"])),
            "known_hosts_file" if message.contains("holds '%'") => Some(format!(
                "choose a known_hosts path without '%': {}",
                context.server_fix(&["--known-hosts <PATH>"])
            )),
            "known_hosts_file" => Some(context.server_fix(&["--known-hosts <PATH>"])),
            "identity_file" => Some(context.server_fix(&["--key <PATH>"])),
            "binary" => Some("apvm server defaults --ssh-binary <PATH>".into()),
            _ => None,
        };
        if let Some(remedy) = remedy
            && !remedies.contains(&remedy)
        {
            remedies.push(remedy);
        }
    }
    remedies
}

#[cfg(test)]
mod tests;
