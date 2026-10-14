//! The ssh_config file and the argv of one call (pure).
//!
//! Every option apvm sets goes into a generated file passed with `-F`,
//! because that is the only part of the main connection ssh hands to jump
//! hosts: for a ProxyJump it runs `ssh -l <user> -p <port> -F <file> -v
//! -W '[%h]:%p' <hop>` through the user's shell, with none of the main
//! connection's `-o`, `-i` or other options (verified on OpenSSH 9.6p1,
//! 10.0p2 and 10.3p1; plan P3.3). The file starts with apvm's options at
//! top level, which apply to the server and to every jump host and win
//! over everything after them (ssh keeps the first value it reads), then
//! `Include`s what ssh would have read without `-F`: the user's
//! `~/.ssh/config` and the system-wide `ssh_config`, or the inventory's
//! `ssh_config_file` alone (as `ssh -F <file>` reads only that file). When
//! apvm uses ssh's own known_hosts files, it ends with ssh's default
//! `UpdateHostKeys yes` (8.5+), which naming them would turn off
//! ([`UPDATE_HOST_KEYS_DEFAULT`]).
//!
//! The command line holds only what applies to the server itself: `-l`,
//! `-p` and `-J`, which ssh does not hand to jump hosts (an inventory jump
//! list in the file would make every hop jump again).
//!
//! **Paths in the file** are absolute and double-quoted (spaces, `#`, `'`
//! and `=` then stay literal, verified). Refused, never escaped differently:
//! `"`, control and invisible formatting characters; on Unix a `\` (ssh
//! reads `\\` and `\"` as escapes); `${` in key and known_hosts paths (ssh
//! 8.4 and later expand environment variables there); `*`, `?` and `[` in
//! included paths (`Include` expands patterns); `%` in known_hosts paths
//! (ssh 8.4 and later expand `%` tokens there, older ones read `%%`
//! literally: verified on 7.6p1 and 8.2p1, so no spelling works for both).
//! `%` in the key path is doubled (every version expands `%` tokens there;
//! `%%` is a literal `%`, verified on 7.6p1–10.3p1), and so is it in the
//! control directory of a `ControlPath` (`remote/mux.rs`), where `${` is
//! refused too and the socket name's `%C` stays a token. On Windows `\`
//! becomes `/`, which Win32-OpenSSH also reads as a separator.
//!
//! **What ssh pastes into a shell command** for a jump host, the program
//! and the `-F` value, must be shell-literal ([`shell_literal`]): ssh
//! neither quotes them nor stops at `%`. The `-F` value is the generated
//! file's path when it is, else its bare name (ssh runs in its directory),
//! which always is.

use std::ffi::OsString;
use std::path::Path;

use super::HostKeyPolicy;
use crate::allowlist::is_forbidden;
use crate::inventory::ResolvedServer;
use crate::problem::quoted_path;

/// How ssh notices a server that stopped answering: `ServerAliveInterval`
/// (seconds without data before ssh asks the server whether it is still
/// there) and `ServerAliveCountMax` (unanswered checks before ssh gives the
/// connection up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerAlive {
    /// `ServerAliveInterval`, in seconds.
    pub(crate) interval_secs: u32,
    /// `ServerAliveCountMax`.
    pub(crate) count_max: u32,
}

impl ServerAlive {
    /// A call's own connection (plan §6.1 item 7): given up after about a
    /// minute of silence from the server.
    pub(crate) const OWN: Self = Self {
        interval_secs: 15,
        count_max: 4,
    };

    /// A shared connection (P3.4, `remote/mux.rs`): a dead master is given
    /// up after 30 to 40 s rather than a minute and more (5 s × 3 gave 19.6 s
    /// on 10.3p1 through a relay that drops a live connection), while a
    /// blip shorter than that (a Wi-Fi roam, a VPN reconnect) does not cut
    /// the long calls riding it (audit round 2: 5 s × 3 cut a 900 s install
    /// on a 15 s blip). `ssh -O stop` after a stalled call bounds the rest.
    pub(crate) const SHARED: Self = Self {
        interval_secs: 10,
        count_max: 3,
    };
}

/// The options every call sets after the host key, key and timeout lines,
/// before the connection sharing lines ([`ConfigInputs::control`]), each
/// with why (they win over the user's ssh_config, for jump hosts too).
const SESSION_OPTIONS: &[&str] = &[
    // The P3.3 classifier reads ssh's messages: `QUIET` would hide them,
    // `DEBUG` flood stderr.
    "LogLevel INFO",
    // Scripts and uploads travel on stdin byte for byte, and stderr stays
    // apart from stdout: no pseudo-terminal, whatever the config asks.
    "RequestTTY no",
    // The remote command is apvm's (`sh -s` or an upload's `sh -c`).
    "RemoteCommand none",
];

/// The options every call sets after the connection sharing lines, each
/// with why.
const FORWARDING_OPTIONS: &[&str] = &[
    // Nothing else is forwarded to the server or run locally per call.
    "ForwardAgent no",
    "ForwardX11 no",
    "ClearAllForwardings yes",
    "PermitLocalCommand no",
    "Tunnel no",
    // The inventory's key stays out of the user's agent: an ssh_config's
    // `AddKeysToAgent yes` added it there for good (verified on 10.3p1), and
    // `ask`/`confirm` would ask through ssh-askpass, BatchMode or not
    // (OpenSSH 7.2+; first value wins, verified on 7.6p1–10.6p1).
    "AddKeysToAgent no",
];

/// The options that keep the session apvm's, each with the `ssh -G`
/// keyword that shows the client knows it: written only then, as older
/// clients refuse an unknown option (`Bad configuration option`, verified
/// on 7.6p1 and 8.2p1), and their ssh_config cannot set it either. From a
/// user's ssh_config each would break a call (verified on 10.3p1):
/// `SessionType none` runs no command and hangs until the timeout,
/// `StdinNull yes` never sends the script or the upload (the call ends
/// with exit 0 and nothing ran), `ForkAfterAuthentication yes` detaches
/// ssh from apvm, so the command runs on outside its timeout and process
/// tree, and a `ChannelTimeout` kills a command that prints nothing for
/// that long (an install, a long WP-CLI command; exit 255). A jump host's
/// `-W` sets its own session type on its command line, which wins
/// (verified).
const PINNED_IF_KNOWN: &[(&str, &str)] = &[
    // OpenSSH 8.7.
    ("sessiontype", "SessionType default"),
    ("stdinnull", "StdinNull no"),
    ("forkafterauthentication", "ForkAfterAuthentication no"),
    // OpenSSH 9.6 (the client's; 9.2 added only sshd's: 9.2p1 refuses it
    // and its `-G` does not print it, verified).
    ("channeltimeout", "ChannelTimeout none"),
];

/// The [`PINNED_IF_KNOWN`] options a client knows, in order.
///
/// # Arguments
///
/// * `knows` - Whether the client's `ssh -G` printed a keyword
pub(crate) fn pinned_options(knows: impl Fn(&str) -> bool) -> Vec<&'static str> {
    PINNED_IF_KNOWN
        .iter()
        .filter(|(keyword, _)| knows(keyword))
        .map(|(_, line)| *line)
        .collect()
}

/// Why a path cannot be written into the generated file or a jump host's
/// shell command: refused, never escaped.
pub(crate) type PathRule = Result<String, String>;

/// The platform whose conventions paths follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    /// `/` separators; `\` is an ordinary (refused) character.
    Unix,
    /// `\` or `/` separators; drive letters.
    Windows,
}

impl Platform {
    /// The platform apvm runs on.
    pub(crate) const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };
}

/// What the generated file says, every path resolved and checked
/// ([`super::files::LocalFiles`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigInputs<'a> {
    /// Whether unknown host keys are refused or recorded.
    pub(crate) host_keys: HostKeyPolicy,
    /// The known_hosts files, the one new keys go to first.
    pub(crate) known_hosts: &'a [&'a Path],
    /// The private key, when the inventory sets one.
    pub(crate) identity: Option<&'a Path>,
    /// `ConnectTimeout`, in seconds.
    pub(crate) connect_timeout_secs: u32,
    /// The keep-alive checks.
    pub(crate) server_alive: ServerAlive,
    /// The connection sharing lines (`remote/mux.rs`): apvm's own
    /// `ControlMaster`/`ControlPath`/`ControlPersist`, or none shared at
    /// all, so no connection the user opened with other options is reused
    /// (that would skip apvm's host key checks).
    pub(crate) control: &'a [&'a str],
    /// The [`PINNED_IF_KNOWN`] lines this ssh knows ([`pinned_options`]).
    pub(crate) pinned: &'a [&'static str],
    /// The ssh_config files to include, in order.
    pub(crate) includes: &'a [&'a Path],
    /// Whether to end the file with [`UPDATE_HOST_KEYS_DEFAULT`]: the client
    /// turns `UpdateHostKeys` on by default (8.5+) and apvm uses the
    /// known_hosts files ssh would.
    pub(crate) update_host_keys: bool,
}

/// What ssh 8.5 and later default `UpdateHostKeys` to when no
/// `UserKnownHostsFile` is set (`readconf.c`: on unless `VerifyHostKeyDNS`
/// is, or another known_hosts file is named). apvm always names the
/// known_hosts files, which turns that default off, so when they are ssh's
/// own (`~/.ssh/known_hosts` and `~/.ssh/known_hosts2`) the file ends with
/// it, after the included files: one of them that sets `UpdateHostKeys`
/// wins (ssh keeps the first value), as without apvm. ssh then records the
/// other host keys a server proves with the known one, as plain ssh does
/// in the same file (verified: `ssh -G` shows `updatehostkeys true` for
/// both, 8.5+).
pub(crate) const UPDATE_HOST_KEYS_DEFAULT: &str = "UpdateHostKeys yes";

/// The ssh_config text for one call.
///
/// # Arguments
///
/// * `inputs` - What it says
/// * `platform` - Whose path conventions apply
///
/// # Errors
///
/// The field (`known_hosts_file`, `identity_file` or `ssh_config_file`)
/// and why, when a path cannot be written into the file (see the module
/// docs).
pub(crate) fn render(
    inputs: &ConfigInputs<'_>,
    platform: Platform,
) -> Result<String, (&'static str, String)> {
    let mut lines = vec![
        "# Generated by apvm for one ssh call, and deleted after it. ssh keeps the first"
            .to_string(),
        "# value it reads: these options win over the included files, for the server and"
            .to_string(),
        "# every jump host.".to_string(),
        "BatchMode yes".to_string(),
        format!("StrictHostKeyChecking {}", inputs.host_keys.option_value()),
    ];
    lines.push(known_hosts_line(inputs.known_hosts, platform)?);
    if let Some(identity) = inputs.identity {
        let path =
            config_path(identity, PathUse::Identity, platform).map_err(|e| ("identity_file", e))?;
        lines.push(format!("IdentityFile {path}"));
        lines.push("IdentitiesOnly yes".to_string());
    }
    lines.push(format!("ConnectTimeout {}", inputs.connect_timeout_secs));
    lines.push(format!(
        "ServerAliveInterval {}",
        inputs.server_alive.interval_secs
    ));
    lines.push(format!(
        "ServerAliveCountMax {}",
        inputs.server_alive.count_max
    ));
    lines.extend(SESSION_OPTIONS.iter().map(|option| option.to_string()));
    lines.extend(inputs.control.iter().map(|option| option.to_string()));
    lines.extend(FORWARDING_OPTIONS.iter().map(|option| option.to_string()));
    lines.extend(inputs.pinned.iter().map(|option| option.to_string()));
    for include in inputs.includes {
        let path =
            config_path(include, PathUse::Include, platform).map_err(|e| ("ssh_config_file", e))?;
        lines.push(format!("Include {path}"));
    }
    if inputs.update_host_keys {
        lines.push(
            "# ssh's own default for its own known_hosts files (8.5+), unless set above."
                .to_string(),
        );
        lines.push(UPDATE_HOST_KEYS_DEFAULT.to_string());
    }
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(text)
}

/// The `UserKnownHostsFile` line: every file, quoted.
///
/// # Arguments
///
/// * `files` - The known_hosts files, the one new keys go to first
/// * `platform` - Whose path conventions apply
///
/// # Errors
///
/// `known_hosts_file` and why, when a path cannot be written.
fn known_hosts_line(files: &[&Path], platform: Platform) -> Result<String, (&'static str, String)> {
    let paths = files
        .iter()
        .map(|file| config_path(file, PathUse::KnownHosts, platform))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|reason| ("known_hosts_file", reason))?;
    Ok(format!("UserKnownHostsFile {}", paths.join(" ")))
}

/// How ssh reads a path in the generated file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathUse {
    /// `IdentityFile`: `%` tokens are expanded by every version, `${VAR}`
    /// by 8.4 and later.
    Identity,
    /// `UserKnownHostsFile`: `%` tokens and `${VAR}` are expanded by 8.4
    /// and later only.
    KnownHosts,
    /// `Include`: patterns are expanded.
    Include,
    /// The directory of a `ControlPath`: `%` tokens and `${VAR}` are
    /// expanded by every version (`${VAR}` from 8.4; verified on 10.3p1).
    Control,
}

/// `path` as one double-quoted ssh_config argument (see the module docs).
///
/// # Arguments
///
/// * `path` - An absolute local path
/// * `usage` - How ssh reads it
/// * `platform` - Whose path conventions apply
///
/// # Errors
///
/// Why it cannot be written, phrased to follow the path.
fn config_path(path: &Path, usage: PathUse, platform: Platform) -> PathRule {
    Ok(format!("\"{}\"", escaped(path, usage, platform)?))
}

/// The `ControlPath` value for the sockets named `name` in `dir`, one
/// double-quoted argument: `dir` escaped like any path ssh expands `%`
/// tokens in (`%%`, verified with a directory holding a space and a `%` on
/// 10.3p1), `name` as it is, so the `%C` it holds stays a token.
///
/// # Arguments
///
/// * `dir` - The control directory, an absolute local path
/// * `name` - The socket name pattern (`[a-z2-7]` and `%C`)
/// * `platform` - Whose path conventions apply
///
/// # Errors
///
/// Why `dir` cannot be written, phrased to follow it.
pub(crate) fn control_path(dir: &Path, name: &str, platform: Platform) -> PathRule {
    let dir = escaped(dir, PathUse::Control, platform)?;
    Ok(format!("\"{}/{name}\"", dir.trim_end_matches('/')))
}

/// `path` as ssh_config text, before quoting (see [`config_path`]).
///
/// # Arguments
///
/// * `path` - An absolute local path
/// * `usage` - How ssh reads it
/// * `platform` - Whose path conventions apply
///
/// # Errors
///
/// Why it cannot be written, phrased to follow the path.
fn escaped(path: &Path, usage: PathUse, platform: Platform) -> PathRule {
    let text = utf8(path)?;
    if let Some(bad) = text
        .chars()
        .find(|c| *c == '"' || c.is_control() || is_forbidden(*c))
    {
        return Err(refusal(text, &format!("holds {}", character(bad))));
    }
    let text = match platform {
        Platform::Unix if text.contains('\\') => {
            return Err(refusal(
                text,
                "holds '\\', which ssh would read as an escape",
            ));
        }
        Platform::Unix => text.to_string(),
        Platform::Windows => text.replace('\\', "/"),
    };
    let text = match usage {
        PathUse::Identity | PathUse::KnownHosts | PathUse::Control if text.contains("${") => {
            return Err(refusal(&text, "holds '${', which ssh would expand"));
        }
        PathUse::KnownHosts if text.contains('%') => {
            return Err(refusal(
                &text,
                "holds '%', which ssh 8.4 and later expand and older versions do not",
            ));
        }
        PathUse::Identity | PathUse::Control => text.replace('%', "%%"),
        PathUse::KnownHosts => text,
        PathUse::Include if text.contains(['*', '?', '[']) => {
            return Err(refusal(
                &text,
                "holds '*', '?' or '[', which Include would expand",
            ));
        }
        PathUse::Include => text,
    };
    Ok(text)
}

/// `path` as text, refused when it is not valid UTF-8 (ssh_config holds
/// text).
///
/// # Arguments
///
/// * `path` - A local path
///
/// # Errors
///
/// Why it is refused.
fn utf8(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| {
        format!(
            "{} is not valid UTF-8, so ssh_config cannot name it",
            quoted_path(&path.to_string_lossy())
        )
    })
}

/// A refusal of `text`: `'<text>' <reason>`.
///
/// # Arguments
///
/// * `text` - The path
/// * `reason` - Why
fn refusal(text: &str, reason: &str) -> String {
    format!("{} {reason}", quoted_path(text))
}

/// A character for messages: `'x'`, or its code point when it is not
/// printable.
///
/// # Arguments
///
/// * `c` - The character
fn character(c: char) -> String {
    if c.is_control() || is_forbidden(c) {
        return format!("U+{:04X}", u32::from(c));
    }
    format!("'{c}'")
}

/// Check that ssh can paste `path` into a jump host's shell command as it
/// is: only `[A-Za-z0-9._/:+-]` (and `\` on Windows). ssh runs `exec <ssh
/// program> … -F <file> …` through the user's shell after expanding `%`
/// tokens, quoting neither (plan §7, revision 8).
///
/// # Arguments
///
/// * `path` - The ssh program (or the generated file's name)
/// * `platform` - Whose path conventions apply
///
/// # Errors
///
/// Why it is refused.
pub(crate) fn shell_literal(path: &Path, platform: Platform) -> PathRule {
    let text = utf8(path)?;
    let allowed = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '.' | '_' | '/' | ':' | '+' | '-')
            || (platform == Platform::Windows && c == '\\')
    };
    match text.chars().find(|c| !allowed(*c)) {
        Some(bad) => Err(refusal(
            text,
            &format!(
                "holds {}: ssh pastes it unquoted into the shell command that reaches a jump \
                 host, so only letters, digits, '.', '_', '/', ':', '+' and '-' are allowed",
                character(bad)
            ),
        )),
        None if text.is_empty() => Err("is empty".into()),
        None => Ok(text.to_string()),
    }
}

/// The argv after the program: `-F <config> [-l <user>] [-p <port>] [-J
/// <hops>] -- <host> <remote command>`.
///
/// # Arguments
///
/// * `config` - The generated file, as ssh is to open it (its path, or its
///   bare name: ssh runs in its directory)
/// * `server` - The server and its effective SSH settings
/// * `remote_command` - What the login shell runs (`sh -s`, or an upload's
///   `sh -c '<script>'`)
pub(crate) fn arguments(
    config: &Path,
    server: &ResolvedServer,
    remote_command: &str,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-F".into(), config.into()];
    args.extend(destination(server));
    args.push(remote_command.into());
    args
}

/// The part of the argv that names where ssh connects: `[-l <user>] [-p
/// <port>] [-J <hops>] -- <host>` (the same for every call to `server`).
///
/// # Arguments
///
/// * `server` - The server and its effective SSH settings
pub(crate) fn destination(server: &ResolvedServer) -> Vec<OsString> {
    let ssh = &server.ssh;
    let mut args: Vec<OsString> = Vec::new();
    if let Some(user) = &ssh.user {
        args.extend(["-l".into(), user.value.clone().into()]);
    }
    if let Some(port) = &ssh.port {
        args.extend(["-p".into(), port.value.to_string().into()]);
    }
    if let Some(hops) = &ssh.proxy_jump {
        args.extend(["-J".into(), hops.value.clone().into()]);
    }
    args.extend(["--".into(), server.host.clone().into()]);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::mux::NOT_SHARED;

    /// Inputs with one known_hosts file, no key and no include.
    fn minimal<'a>(known_hosts: &'a [&'a Path]) -> ConfigInputs<'a> {
        ConfigInputs {
            host_keys: HostKeyPolicy::Strict,
            known_hosts,
            identity: None,
            connect_timeout_secs: 15,
            server_alive: ServerAlive::OWN,
            control: NOT_SHARED,
            pinned: &[],
            includes: &[],
            update_host_keys: false,
        }
    }

    /// The lines every file ends its options with.
    const FIXED: &str = "ServerAliveInterval 15\nServerAliveCountMax 4\nLogLevel INFO\n\
                         RequestTTY no\nRemoteCommand none\nControlMaster no\nControlPath none\n\
                         ForwardAgent no\nForwardX11 no\nClearAllForwardings yes\n\
                         PermitLocalCommand no\nTunnel no\nAddKeysToAgent no\n";

    /// The lines a client that knows them (OpenSSH 8.7+) gets after them.
    const SESSION: &str =
        "SessionType default\nStdinNull no\nForkAfterAuthentication no\nChannelTimeout none\n";

    /// Every pinned option, as a client that knows them all (10.3p1) gets them.
    const ALL_PINNED: [&str; 4] = [
        "SessionType default",
        "StdinNull no",
        "ForkAfterAuthentication no",
        "ChannelTimeout none",
    ];

    /// The comment every file starts with.
    const HEADER: &str = "# Generated by apvm for one ssh call, and deleted after it. ssh keeps \
                          the first\n# value it reads: these options win over the included \
                          files, for the server and\n# every jump host.\n";

    #[test]
    fn the_full_file_for_unix() {
        let known = [
            Path::new("/home/u/.ssh/known_hosts"),
            Path::new("/home/u/.ssh/known_hosts2"),
        ];
        let includes = [
            Path::new("/home/u/.ssh/config"),
            Path::new("/etc/ssh/ssh_config"),
        ];
        let inputs = ConfigInputs {
            host_keys: HostKeyPolicy::Strict,
            known_hosts: &known,
            identity: Some(Path::new("/home/u/.ssh/id_ed25519")),
            connect_timeout_secs: 15,
            server_alive: ServerAlive::OWN,
            control: NOT_SHARED,
            pinned: &ALL_PINNED,
            includes: &includes,
            update_host_keys: true,
        };
        let expected = format!(
            "{HEADER}BatchMode yes\nStrictHostKeyChecking yes\n\
             UserKnownHostsFile \"/home/u/.ssh/known_hosts\" \"/home/u/.ssh/known_hosts2\"\n\
             IdentityFile \"/home/u/.ssh/id_ed25519\"\nIdentitiesOnly yes\nConnectTimeout 15\n\
             {FIXED}{SESSION}Include \"/home/u/.ssh/config\"\nInclude \"/etc/ssh/ssh_config\"\n\
             # ssh's own default for its own known_hosts files (8.5+), unless set above.\n\
             UpdateHostKeys yes\n"
        );
        assert_eq!(render(&inputs, Platform::Unix), Ok(expected));
    }

    #[test]
    fn accept_new_and_a_lone_config_file() {
        let known = [Path::new("/srv/apvm/known_hosts")];
        let includes = [Path::new("/srv/apvm/ssh config")];
        let inputs = ConfigInputs {
            host_keys: HostKeyPolicy::AcceptNew,
            known_hosts: &known,
            identity: None,
            connect_timeout_secs: 3600,
            server_alive: ServerAlive::OWN,
            control: NOT_SHARED,
            pinned: &[],
            includes: &includes,
            update_host_keys: false,
        };
        let expected = format!(
            "{HEADER}BatchMode yes\nStrictHostKeyChecking accept-new\n\
             UserKnownHostsFile \"/srv/apvm/known_hosts\"\nConnectTimeout 3600\n\
             {FIXED}Include \"/srv/apvm/ssh config\"\n"
        );
        assert_eq!(render(&inputs, Platform::Unix), Ok(expected));
    }

    #[test]
    fn the_full_file_for_windows_uses_forward_slashes() {
        let known = [Path::new(r"C:\Users\Jane Doe\.ssh\known_hosts")];
        let includes = [
            Path::new(r"C:\Users\Jane Doe\.ssh\config"),
            Path::new(r"C:\ProgramData\ssh\ssh_config"),
        ];
        let inputs = ConfigInputs {
            host_keys: HostKeyPolicy::Strict,
            known_hosts: &known,
            identity: Some(Path::new(r"C:\Users\Jane Doe\.ssh\id_ed25519")),
            connect_timeout_secs: 15,
            server_alive: ServerAlive::OWN,
            control: NOT_SHARED,
            pinned: &[],
            includes: &includes,
            update_host_keys: false,
        };
        let expected = format!(
            "{HEADER}BatchMode yes\nStrictHostKeyChecking yes\n\
             UserKnownHostsFile \"C:/Users/Jane Doe/.ssh/known_hosts\"\n\
             IdentityFile \"C:/Users/Jane Doe/.ssh/id_ed25519\"\nIdentitiesOnly yes\n\
             ConnectTimeout 15\n{FIXED}Include \"C:/Users/Jane Doe/.ssh/config\"\n\
             Include \"C:/ProgramData/ssh/ssh_config\"\n"
        );
        assert_eq!(render(&inputs, Platform::Windows), Ok(expected));
    }

    #[test]
    fn options_never_reach_hops_through_the_command_line_and_every_value_is_one_line() {
        // A jump list in the file would make every hop jump again; user
        // and port belong to the server alone.
        let known = [Path::new("/k")];
        let text = render(&minimal(&known), Platform::Unix).unwrap();
        for option in [
            "ProxyJump",
            "ProxyCommand",
            "User ",
            "Port ",
            "Host ",
            "Match ",
        ] {
            assert!(!text.contains(option), "{option}");
        }
        assert!(text.lines().all(|line| !line.starts_with(' ')));
    }

    #[test]
    fn options_are_pinned_only_when_the_client_knows_them() {
        assert_eq!(pinned_options(|_| true), ALL_PINNED);
        assert!(pinned_options(|_| false).is_empty());
        // 9.2: the 8.7 options, not 9.6's ChannelTimeout.
        let nine = pinned_options(|keyword| keyword != "channeltimeout");
        assert_eq!(nine, ALL_PINNED[..3]);

        let known = [Path::new("/k")];
        let old = render(&minimal(&known), Platform::Unix).unwrap();
        for option in [
            "SessionType",
            "StdinNull",
            "ForkAfterAuthentication",
            "ChannelTimeout",
        ] {
            assert!(!old.contains(option), "{option}");
        }
        let inputs = ConfigInputs {
            control: NOT_SHARED,
            pinned: &ALL_PINNED,
            ..minimal(&known)
        };
        let new = render(&inputs, Platform::Unix).unwrap();
        // After apvm's other options, before any included file.
        assert!(
            new.ends_with(&format!("AddKeysToAgent no\n{SESSION}")),
            "{new}"
        );
    }

    #[test]
    fn percent_signs_are_doubled_in_the_key_path_refused_in_known_hosts_kept_in_includes() {
        let known = [Path::new("/home/ab/kh")];
        let includes = [Path::new("/home/a%b/config")];
        let inputs = ConfigInputs {
            identity: Some(Path::new("/home/a%b/key%")),
            includes: &includes,
            ..minimal(&known)
        };
        let text = render(&inputs, Platform::Unix).unwrap();
        assert!(
            text.contains("IdentityFile \"/home/a%%b/key%%\"\n"),
            "{text}"
        );
        assert!(text.contains("Include \"/home/a%b/config\"\n"), "{text}");
        // `%%` would be a literal `%%` for ssh before 8.4 (verified).
        let known = [Path::new("/home/a%b/kh")];
        let (field, reason) = render(&minimal(&known), Platform::Unix).unwrap_err();
        assert_eq!(field, "known_hosts_file");
        assert_eq!(
            reason,
            "'/home/a%b/kh' holds '%', which ssh 8.4 and later expand and older versions do not"
        );
    }

    #[test]
    fn paths_ssh_would_misread_are_refused_with_their_field() {
        let cases: [(&str, &str, &str); 9] = [
            ("known_hosts_file", "/a\"b/kh", "holds '\"'"),
            ("known_hosts_file", "/a\nb/kh", "holds U+000A"),
            ("known_hosts_file", "/a\u{202e}b/kh", "holds U+202E"),
            ("known_hosts_file", "/a\\b/kh", "holds '\\'"),
            ("known_hosts_file", "/a${HOME}/kh", "holds '${'"),
            ("identity_file", "/a${X}/key", "holds '${'"),
            ("identity_file", "/a\tb/key", "holds U+0009"),
            ("ssh_config_file", "/a*/config", "Include would expand"),
            ("ssh_config_file", "/a[1]/config", "Include would expand"),
        ];
        for (field, path, reason) in cases {
            let path = Path::new(path);
            let good = [Path::new("/k")];
            let (known, identity, includes) = match field {
                "known_hosts_file" => (vec![path], None, vec![]),
                "identity_file" => (good.to_vec(), Some(path), vec![]),
                _ => (good.to_vec(), None, vec![path]),
            };
            let inputs = ConfigInputs {
                known_hosts: &known,
                identity,
                includes: &includes,
                ..minimal(&good)
            };
            let (got_field, got) = render(&inputs, Platform::Unix).unwrap_err();
            assert_eq!(got_field, field, "{path:?}");
            assert!(got.contains(reason), "{path:?}: {got}");
        }
        // `${` stays literal in an Include, `?` in a key path.
        let known = [Path::new("/k?")];
        let includes = [Path::new("/a${X}/config")];
        let inputs = ConfigInputs {
            includes: &includes,
            ..minimal(&known)
        };
        assert!(render(&inputs, Platform::Unix).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"/a\xffb"));
        let known = [path.as_path()];
        let (field, reason) = render(&minimal(&known), Platform::Unix).unwrap_err();
        assert_eq!(field, "known_hosts_file");
        assert!(reason.contains("is not valid UTF-8"), "{reason}");
    }

    #[test]
    fn shell_literal_paths() {
        for ok in [
            "ssh",
            "/usr/bin/ssh",
            "/var/folders/2x/abc_d-1/T/apvm-ssh-AbC123.conf",
            "/a:b+c",
        ] {
            assert_eq!(
                shell_literal(Path::new(ok), Platform::Unix),
                Ok(ok.to_string())
            );
        }
        assert!(
            shell_literal(
                Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe"),
                Platform::Windows
            )
            .is_ok()
        );
        for bad in [
            "/Users/Jane Doe/bin/ssh",
            "/tmp/$(touch x)",
            "/tmp/a%h",
            "/tmp/a'b",
            "/tmp/a\\b",
            "/tmp/a*b",
            "/tmp/~x",
            "",
        ] {
            let refused = shell_literal(Path::new(bad), Platform::Unix);
            assert!(refused.is_err(), "{bad:?}");
        }
        let message = shell_literal(Path::new("/a b"), Platform::Unix).unwrap_err();
        assert_eq!(
            message,
            "'/a b' holds ' ': ssh pastes it unquoted into the shell command that reaches a \
             jump host, so only letters, digits, '.', '_', '/', ':', '+' and '-' are allowed"
        );
    }

    #[test]
    fn refusals_show_backslashes_as_written() {
        let refused = config_path(Path::new(r"/tmp/a\b"), PathUse::Identity, Platform::Unix);
        assert_eq!(
            refused,
            Err(r"'/tmp/a\b' holds '\', which ssh would read as an escape".to_string())
        );
    }
}
