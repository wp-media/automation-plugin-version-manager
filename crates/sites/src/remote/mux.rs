//! Connection sharing for the SSH transport (plan §6.1 item 9, P3.4):
//! OpenSSH multiplexing, so repeated calls to a server reuse one
//! authenticated connection instead of opening one each.
//!
//! **Calls never become masters.** A call's generated file says
//! `ControlMaster no` and `ControlPath "<control dir>/<tag>%C"`: it rides a
//! live master's connection, else opens one of its own (ssh's fallback when
//! the socket is missing or refuses). The master is started apart
//! (`remote/ssh/master.rs`), when nothing says hello behind the socket
//! before a call (`remote/ssh/socket.rs`: apvm connects to it and waits up
//! to 3 s for the master's first bytes): `ssh -F <file> -N <destination>` with `ControlMaster auto` and
//! `ControlPersist 15s` authenticates, forks the master into the
//! background (a session of its own, standard streams on `/dev/null`) and
//! exits, running nothing on the server. So no call's process tree holds
//! the shared connection: a call that times out or is dropped, whose tree
//! apvm kills, cannot take the others' connection with it (audit round 2).
//! The master stays until it has been idle for [`CONTROL_PERSIST_SECS`];
//! its process is the starter's `ssh -F … -N …`, which OpenSSH does not
//! retitle.
//!
//! **Only direct connections are shared.** `ssh -G` resolves the
//! configuration twice, as for a call (with its remote command, `sh -s`)
//! and as for a master (`-N`), since OpenSSH 10's `Match command` and
//! `Match sessiontype` can tell them apart (`ssh/resolve.rs`). When either
//! reaches the server through a jump host or a ProxyCommand (`proxyjump`
//! or `proxycommand`, from the inventory or the user's ssh_config),
//! multiplexing is off ([`NoMultiplexing::Proxied`]); when they differ in
//! host, user, port or socket, too ([`NoMultiplexing::SessionDependent`]):
//! a master would serve no call (audit round 4). The verdict is reused for
//! 30 s per configuration (`-G` reads included files and runs `Match exec`
//! commands each time: twice per check, so such a command runs more often
//! than with plain ssh). With `ControlPersist`
//! set, ssh sends the proxy's stderr to `/dev/null`, so a jump host's
//! refusal (an unknown host key, a failed login, an unknown name) would
//! fail the start with no reason, and the host key notices of an
//! `accept-new` hop would be lost (audit round 3, verified on 10.3p1). The
//! proxy would also stay a child of the starter until it exits.
//!
//! Shared connections check the server every 10 s and give up after three
//! unanswered checks (`ServerAliveInterval 10`, `ServerAliveCountMax 3`,
//! instead of a call's own 15 s and 4): a dead master is found sooner,
//! while a network blip of under about 30 s does not cut a long call.
//!
//! A stale socket (a master killed with `SIGKILL`) refuses connections, and
//! the starter replaces it. A master that dies during a call ends it
//! with exit 255 and nothing on stderr, which reads as a lost connection
//! once the script had started (verified on 10.3p1).
//!
//! **A master that stopped working** stalls the calls that reach it, which
//! then go on over a connection of their own (verified on 10.3p1):
//! - its network connection died silently (a Wi-Fi or VPN switch, a NAT
//!   reset, a laptop's sleep): a call waits until the master's keep-alive
//!   checks give up (about 30 to 40 s), and ssh prints
//!   `mux_client_request_session: read from master failed: Broken pipe`. A
//!   call that timed out before its script started asks that master to
//!   stop taking calls (`ssh -O stop`: its running calls go on), so the
//!   next call starts a new one;
//! - it is stopped (`SIGSTOP`) or wedged: a connection to its socket is
//!   queued and nothing answers (`ssh -O check` would wait forever; a call
//!   hangs before OpenSSH 9.6, verified on 8.9p1 and 9.2p1 by audit round
//!   4, and 9.6 and later give up after the connect timeout with
//!   `muxclient: master hello exchange failed`). So no hello within 3 s
//!   means a stuck master: its socket is removed (only one of this
//!   configuration's in the checked control directory), the call (and the
//!   calls that waited for that look) runs on a connection of its own with
//!   `ControlPath none`, and the next call starts a new master. ssh's own
//!   hello failure line in a call's stderr removes the socket too. A stuck
//!   master that later resumes and exits removes its path, by then its
//!   successor's socket: the successor runs on unreachable until idle, and
//!   the next call starts another (apvm cannot tell the stuck master's
//!   process apart to end it first). apvm does not run `ssh -O check`,
//!   which first reads the whole ssh_config: a slow `Match exec` or DNS
//!   lookup made a live master look stuck (audit round 5).
//!
//! These lines are noise to [`classify`](super::classify()). The short
//! `ControlPersist` keeps a master from outliving a burst of calls by long,
//! which bounds the window for any of this.
//!
//! **Only calls with identical options share a connection.** `<tag>` is
//! [`TAG_LEN`] base32 characters of a SHA-256 of the ssh program, the
//! generated file (without these lines) and the destination arguments
//! (`-l`, `-p`, the host): a call with another key, known_hosts file, host
//! key policy or ssh_config never rides a master that was authenticated,
//! and checked, under different ones. `%C` (ssh's hash of the local host
//! and the server's host, port and user: 40 hex digits on every client
//! checked) completes the name.
//!
//! **Off** ([`NoMultiplexing`]) when the server's `multiplex` setting is
//! off, when the caller gave no control directory (a library has no default
//! location, plan §8.1), on Windows (Win32-OpenSSH has no `ControlMaster`),
//! when the socket path would be too long, when the control directory
//! cannot be used, and through a proxy (above). It is never an error: the
//! call then opens its own connection, as before P3.4.
//!
//! **The socket path limit:** a Unix socket's path must fit `sun_path` (104
//! bytes with its NUL on macOS and the BSDs, 108 on Linux), and the master
//! first binds `<path>.<16 random characters>`, then gives it its name. So the
//! expanded path may hold at most 86 bytes on macOS and 90 on Linux; one byte
//! more and ssh fails the whole call (`unix_listener: path "…" too long for
//! Unix domain socket`, exit 255; verified on 10.3p1 macOS and 10.0p2
//! Debian). With the 8-character tag and `%C`'s 40, that leaves 37 bytes for
//! the directory on macOS (`/Users/<16 characters>/.apvm/ssh-mux`) and 41 on
//! Linux. A caller can opt in to a shorter one for that case
//! ([`SshOptions::with_tmp_control_fallback`](super::SshOptions::with_tmp_control_fallback)):
//! `/tmp/apvm-mux-<uid>`, when `/tmp` is root's and sticky, under the same
//! checks.
//!
//! **The control directory** is created when missing (`0700`, parents
//! included), and used only when it is a directory (not a symbolic link) of
//! this user's that no one else may write and this user may enter and write,
//! whose every ancestor (links resolved) belongs to this user or to root
//! and is not writable by its group or others unless sticky (as `/tmp`;
//! stricter than OpenSSH, which checks none: a home directory `0775` with a
//! user-private group turns multiplexing off, `chmod g-w` fixes it),
//! and where a socket can be created (a test bind: on a file system that
//! holds none, ssh would fail every call; a test hard link too, as the
//! master links its socket into place). Anyone who could write there, or
//! swap a directory above it, could plant a socket that apvm's ssh would
//! hand its script to. The checks run before every call (and for
//! [`SshTransport::config_text`](super::SshTransport::config_text) and
//! [`SshTransport::multiplexing`](super::SshTransport::multiplexing)), so
//! they create the directory and bind and remove a test socket in it.
//!
//! **Concurrent calls** with the same options start one master: the check
//! and the start run under one gate per configuration in the process (any
//! transport), and calls that waited while a start failed (unreachable, no
//! answer in time, a stuck master) take that result, without the host key
//! notices only the first call reports, instead of trying again: N calls
//! to a server that does not answer cost one connect timeout, not N (audit
//! round 3). A start's time, and the wait for the gate, come before a
//! call's own timeout starts. Starters of two processes may race: ssh lets
//! one bind the socket, and the other prints `ControlSocket … already
//! exists, disabling multiplexing` and would wait as a plain `ssh -N`
//! (verified on 8.9p1 by audit round 4): apvm stops it on that line and
//! checks again, so its call rides the winner's master. A starter that
//! finds a master live by the time it connects runs as its client, and its
//! `-N` then opens a session (a login shell with no stdin, which ends at
//! once; verified on 10.3p1). A master at sshd's `MaxSessions` (10 by
//! default) refuses a new session: ssh prints `mux_client_request_session:
//! session request failed: Session open refused by peer`, then opens a
//! connection of its own (exit 0, verified on 10.3p1 with eleven calls at
//! once). These lines are noise to [`classify`](super::classify()); apvm's
//! per-server limit ([`ServerLimits`](super::ServerLimits), 4) keeps one
//! process's calls below `MaxSessions`.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::ssh::config::{self, Platform};
use crate::problem::quoted_path;

/// How long an idle master stays up after its last call (`ControlPersist`,
/// plan §6.1 item 9): long enough for the calls of one run, short enough
/// that a master rarely outlives a network change (see the module docs).
pub const CONTROL_PERSIST_SECS: u32 = 15;

/// The length of a socket name's tag ([`connection_tag`]): 8 base32
/// characters, 40 bits, which only tell one user's own configurations
/// apart.
pub(crate) const TAG_LEN: usize = 8;

/// The length of ssh's `%C` expansion: a SHA-1 in hex.
const HASH_LEN: usize = 40;

/// The lines that share no connection: no master is started or used, so
/// no connection the user opened with other options is reused.
pub(crate) const NOT_SHARED: &[&str] = &["ControlMaster no", "ControlPath none"];

/// The bytes of a Unix socket path, its NUL included (`sun_path`): 108 on
/// Linux, 104 on macOS and the BSDs.
const SUN_PATH_BYTES: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
    108
} else {
    104
};

/// What the master appends while it binds its socket: `.` and 16 random
/// characters (OpenSSH's `muxserver_listen`, seen in its `unix_listener`
/// error and its open files on 10.3p1 and 10.0p2).
const BIND_SUFFIX_LEN: usize = 17;

/// The longest expanded `ControlPath` ssh can bind on this system: 86 bytes
/// on macOS, 90 on Linux (both verified: one byte more fails the call).
pub(crate) const SOCKET_PATH_LIMIT: usize = SUN_PATH_BYTES - 1 - BIND_SUFFIX_LEN;

/// The base32 alphabet of tags (RFC 4648, lowercase): one case only, as
/// macOS file names ignore case by default.
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Whether calls share a connection, and where its socket is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Multiplexing {
    /// Calls share a master connection through sockets matching this
    /// pattern: the control directory, the configuration's tag and ssh's
    /// `%C` token (expanded by ssh, once per hop).
    On {
        /// `<control dir>/<tag>%C`.
        control_path: PathBuf,
    },
    /// Every call opens its own connection, for this reason.
    Off(NoMultiplexing),
}

/// Why calls do not share a connection (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoMultiplexing {
    /// The server's `multiplex` setting is off.
    Disabled,
    /// The caller gave no control directory
    /// ([`SshOptions::with_control_dir`](super::SshOptions::with_control_dir)).
    NoControlDir,
    /// Not on Windows: Win32-OpenSSH has no `ControlMaster`.
    Unsupported,
    /// The socket path would be longer than this system allows.
    PathTooLong {
        /// The expanded path's length, in bytes.
        length: usize,
        /// The longest path ssh can bind here (86 bytes on macOS, 90 on Linux).
        limit: usize,
    },
    /// ssh reaches the server through a jump host or a ProxyCommand (as
    /// `ssh -G` resolves the configuration): with `ControlPersist`, ssh
    /// sends the proxy's messages to `/dev/null`, so a refused jump host
    /// would fail without a reason and its host key notices would be lost
    /// (verified on 10.3p1).
    Proxied,
    /// The user's ssh_config sends a call elsewhere than a master (another
    /// host, user, port or socket, through `Match command` or `Match
    /// sessiontype`, OpenSSH 10.0+): a master would serve no call.
    SessionDependent,
    /// `ssh -G` could not tell whether connections can be shared.
    Unresolved {
        /// Why, escaped.
        reason: String,
    },
    /// The control directory cannot be used.
    UnusableControlDir {
        /// Why: a sentence that names the directory, escaped.
        reason: String,
    },
}

impl fmt::Display for NoMultiplexing {
    /// Why connections are not shared, as a clause for messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("multiplexing is off for this server"),
            Self::NoControlDir => f.write_str("no control directory was given"),
            Self::Unsupported => f.write_str("OpenSSH for Windows cannot multiplex"),
            Self::PathTooLong { length, limit } => write!(
                f,
                "the control socket's path would be {length} bytes, over this system's \
                 {limit}: choose a shorter control directory"
            ),
            Self::Proxied => f.write_str(
                "the server is reached through a jump host or a ProxyCommand, whose messages \
                 ssh hides from a shared connection",
            ),
            Self::SessionDependent => f.write_str(
                "the ssh configuration resolves a call and a shared connection differently \
                 ('Match command' or 'Match sessiontype')",
            ),
            Self::Unresolved { reason } => {
                write!(f, "whether connections can be shared is unknown: {reason}")
            }
            Self::UnusableControlDir { reason } => {
                write!(f, "the control directory is unusable: {reason}")
            }
        }
    }
}

/// What decides whether a call shares a connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MuxInputs<'a> {
    /// The server's `multiplex` setting.
    pub(crate) enabled: bool,
    /// The caller's control directory, if any.
    pub(crate) control_dir: Option<&'a Path>,
    /// Whose conventions apply.
    pub(crate) platform: Platform,
    /// The configuration's tag ([`connection_tag`]).
    pub(crate) tag: &'a str,
    /// The longest expanded socket path ([`SOCKET_PATH_LIMIT`]).
    pub(crate) limit: usize,
}

/// Where a call's connection is shared: the generated file's lines for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlSocket {
    /// The control directory.
    dir: PathBuf,
    /// The configuration's tag.
    tag: String,
    /// The socket name pattern: the tag and `%C`.
    name: String,
    /// The `ControlPath` line.
    path_line: String,
}

impl ControlSocket {
    /// A call's lines: it uses a live master's connection, else opens its
    /// own, and never becomes a master (see the module docs).
    pub(crate) fn client_lines(&self) -> [String; 2] {
        ["ControlMaster no".to_string(), self.path_line.clone()]
    }

    /// The master starter's lines: it becomes the master, in the
    /// background, unless one is live.
    pub(crate) fn master_lines(&self) -> [String; 3] {
        [
            "ControlMaster auto".to_string(),
            self.path_line.clone(),
            format!("ControlPersist {CONTROL_PERSIST_SECS}s"),
        ]
    }

    /// The control directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The configuration's tag, which starts every socket name.
    pub(crate) fn tag(&self) -> &str {
        &self.tag
    }

    /// What callers are told: on, with the socket pattern.
    pub(crate) fn multiplexing(&self) -> Multiplexing {
        Multiplexing::On {
            control_path: self.dir.join(&self.name),
        }
    }
}

/// The socket a call would share its connection through, from its inputs
/// alone (no file is looked at; [`usable_dir`] checks the directory).
///
/// # Arguments
///
/// * `inputs` - The setting, the directory, the platform, the tag and the
///   limit
///
/// # Errors
///
/// Why connections are not shared (see the module docs).
pub(crate) fn control_socket(inputs: &MuxInputs<'_>) -> Result<ControlSocket, NoMultiplexing> {
    let dir = usable_setting(inputs)?;
    let name = format!("{}%C", inputs.tag);
    let path_line = config::control_path(dir, &name, inputs.platform)
        .map(|value| format!("ControlPath {value}"))
        .map_err(|reason| NoMultiplexing::UnusableControlDir { reason })?;
    let length = expanded_length(dir, inputs.tag);
    if length > inputs.limit {
        return Err(NoMultiplexing::PathTooLong {
            length,
            limit: inputs.limit,
        });
    }
    Ok(ControlSocket {
        dir: dir.to_path_buf(),
        tag: inputs.tag.to_string(),
        name,
        path_line,
    })
}

/// The control directory when the setting, the platform and the path's
/// form allow multiplexing at all.
///
/// # Arguments
///
/// * `inputs` - What decides it
///
/// # Errors
///
/// Why connections are not shared.
fn usable_setting<'a>(inputs: &MuxInputs<'a>) -> Result<&'a Path, NoMultiplexing> {
    if !inputs.enabled {
        return Err(NoMultiplexing::Disabled);
    }
    let dir = inputs.control_dir.ok_or(NoMultiplexing::NoControlDir)?;
    if inputs.platform == Platform::Windows {
        return Err(NoMultiplexing::Unsupported);
    }
    // A Unix path from here on (Windows returned above), checked as one
    // whatever the host, so the rules can be tested anywhere.
    if !dir.to_string_lossy().starts_with('/') {
        return Err(unusable(dir, "is not absolute"));
    }
    Ok(dir)
}

/// [`NoMultiplexing::UnusableControlDir`]: `'<dir>' <clause>`.
///
/// # Arguments
///
/// * `dir` - The control directory
/// * `clause` - Why it cannot be used
fn unusable(dir: &Path, clause: &str) -> NoMultiplexing {
    NoMultiplexing::UnusableControlDir {
        reason: format!("{} {clause}", quoted_path(&dir.to_string_lossy())),
    }
}

/// The length in bytes of the socket path ssh binds, once `%%` is `%`
/// again and `%C` is expanded: `<dir>/<tag><40 hex digits>`.
///
/// # Arguments
///
/// * `dir` - The control directory
/// * `tag` - The configuration's tag
fn expanded_length(dir: &Path, tag: &str) -> usize {
    let dir = dir.as_os_str().to_string_lossy();
    dir.trim_end_matches('/').len() + 1 + tag.len() + HASH_LEN
}

/// The tag of a configuration: [`TAG_LEN`] base32 characters of a SHA-256
/// over the ssh program, the generated file without its connection sharing
/// lines, and the destination arguments, each length-prefixed so no two
/// inputs read alike.
///
/// # Arguments
///
/// * `program` - The ssh program
/// * `config` - The generated file, with [`NOT_SHARED`] in place
/// * `destination` - `[-l <user>] [-p <port>] [-J <hops>] -- <host>`
pub(crate) fn connection_tag(program: &Path, config: &str, destination: &[OsString]) -> String {
    let mut hash = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(program.as_os_str().to_string_lossy().as_bytes());
    field(config.as_bytes());
    for arg in destination {
        field(arg.to_string_lossy().as_bytes());
    }
    base32(&hash.finalize()[..TAG_LEN * 5 / 8])
}

/// `bytes` in lowercase base32, without padding (5 bytes → 8 characters).
///
/// # Arguments
///
/// * `bytes` - A whole number of 5-byte groups
fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5);
    for group in bytes.chunks(5) {
        let mut word = [0u8; 8];
        word[3..3 + group.len()].copy_from_slice(group);
        let bits = u64::from_be_bytes(word);
        for shift in (0..8).rev() {
            let index = (bits >> (shift * 5)) & 0x1f;
            out.push(char::from(BASE32[index as usize]));
        }
    }
    out
}

/// Make sure `socket`'s directory can hold apvm's sockets: create it when
/// missing (`0700`, parents included), then check it (see the module docs).
///
/// # Arguments
///
/// * `socket` - The planned socket
///
/// # Errors
///
/// [`NoMultiplexing::UnusableControlDir`] with why.
pub(crate) fn usable_dir(socket: &ControlSocket) -> Result<(), NoMultiplexing> {
    let dir = socket.dir();
    prepare_dir(dir).map_err(|clause| unusable(dir, &clause))
}

/// Create `dir` when missing, check it is private, and that a socket can be
/// created in it.
///
/// # Arguments
///
/// * `dir` - The control directory (absolute)
///
/// # Errors
///
/// Why it cannot be used, as a clause that follows the directory.
#[cfg(unix)]
fn prepare_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| format!("cannot be created: {error}"))?;
    let metadata =
        std::fs::symlink_metadata(dir).map_err(|error| format!("cannot be read: {error}"))?;
    let me = rustix::process::geteuid().as_raw();
    private_dir_rule(
        EntryKind::of(&metadata),
        metadata.uid(),
        metadata.mode(),
        me,
    )?;
    safe_ancestors(dir, me)?;
    probe_socket(dir)
        .map_err(|error| format!("cannot hold a socket (and a hard link to it): {error}"))
}

/// Check every ancestor of `dir`, links resolved ([`ancestor_rule`]).
///
/// # Arguments
///
/// * `dir` - The control directory (existing)
/// * `me` - This process's effective user
///
/// # Errors
///
/// Why not, as a clause that follows the directory.
#[cfg(unix)]
fn safe_ancestors(dir: &Path, me: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let real =
        std::fs::canonicalize(dir).map_err(|error| format!("cannot be resolved: {error}"))?;
    for ancestor in real.ancestors().skip(1) {
        let metadata = std::fs::metadata(ancestor).map_err(|error| {
            format!(
                "is under {}, which cannot be read: {error}",
                quoted_path(&ancestor.to_string_lossy())
            )
        })?;
        ancestor_rule(metadata.uid(), metadata.mode(), me).map_err(|why| {
            format!(
                "is under {}, which {why}",
                quoted_path(&ancestor.to_string_lossy())
            )
        })?;
    }
    Ok(())
}

/// Whether a directory above the control directory keeps it private: it
/// belongs to `me` or to root, and neither its group nor others may write
/// it, unless it is sticky (as `/tmp`: then no one else may rename or
/// remove what `me` made there).
///
/// # Arguments
///
/// * `owner` - Its owner
/// * `mode` - Its mode bits
/// * `me` - This process's effective user
///
/// # Errors
///
/// Why not, as a clause after `which`.
#[cfg(unix)]
fn ancestor_rule(owner: u32, mode: u32, me: u32) -> Result<(), String> {
    if owner != me && owner != 0 {
        return Err(format!("belongs to user {owner}"));
    }
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Err(format!(
            "its group or others may write without the sticky bit (mode {:04o})",
            mode & 0o7777
        ));
    }
    Ok(())
}

/// The opt-in short control directory in `tmp` for user `uid`:
/// `<tmp>/apvm-mux-<uid>`.
///
/// # Arguments
///
/// * `tmp` - A sticky directory of root's (`/tmp`)
/// * `uid` - This process's effective user
#[cfg(any(unix, test))]
pub(crate) fn fallback_dir(tmp: &Path, uid: u32) -> PathBuf {
    tmp.join(format!("apvm-mux-{uid}"))
}

/// This user's opt-in short control directory, when `tmp` (a sticky
/// directory of root's, if any) is given: [`fallback_dir`] (Unix; `None`
/// elsewhere).
///
/// # Arguments
///
/// * `tmp` - `/tmp` when it is root's and sticky
#[cfg(unix)]
pub(crate) fn fallback_here(tmp: Option<PathBuf>) -> Option<PathBuf> {
    let uid = rustix::process::geteuid().as_raw();
    tmp.map(|tmp| fallback_dir(&tmp, uid))
}

/// No short control directory: Windows does not multiplex.
///
/// # Arguments
///
/// * `_tmp` - Unused
#[cfg(not(unix))]
pub(crate) fn fallback_here(_tmp: Option<PathBuf>) -> Option<PathBuf> {
    None
}

/// Never reached: Windows does not multiplex ([`control_socket`]).
///
/// # Arguments
///
/// * `_dir` - The control directory
///
/// # Errors
///
/// Always.
#[cfg(not(unix))]
fn prepare_dir(_dir: &Path) -> Result<(), String> {
    Err("cannot hold OpenSSH sockets on this system".into())
}

/// What a control directory's path names, links not followed.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// A directory.
    Directory,
    /// A symbolic link.
    Link,
    /// Anything else.
    Other,
}

#[cfg(unix)]
impl EntryKind {
    /// The kind `metadata` (from `symlink_metadata`) describes.
    ///
    /// # Arguments
    ///
    /// * `metadata` - The entry's metadata, links not followed
    fn of(metadata: &std::fs::Metadata) -> Self {
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return Self::Link;
        }
        if kind.is_dir() {
            return Self::Directory;
        }
        Self::Other
    }
}

/// Whether a control directory with this type, owner and mode is private
/// to user `me` and usable: a directory (not a symbolic link), `me`'s, that
/// `me` may enter and write, and that neither its group nor others may
/// write.
///
/// # Arguments
///
/// * `kind` - Its file type, links not followed
/// * `owner` - Its owner
/// * `mode` - Its mode bits
/// * `me` - This process's effective user
///
/// # Errors
///
/// Why not, as a clause that follows the directory.
#[cfg(unix)]
fn private_dir_rule(kind: EntryKind, owner: u32, mode: u32, me: u32) -> Result<(), String> {
    match kind {
        EntryKind::Directory => {}
        EntryKind::Link => return Err("is a symbolic link".into()),
        EntryKind::Other => return Err("is not a directory".into()),
    }
    if owner != me {
        return Err(format!("belongs to user {owner}, not to this user ({me})"));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "may be written by its group or others (mode {:04o}): run 'chmod go-w' on it",
            mode & 0o7777
        ));
    }
    if mode & 0o300 != 0o300 {
        return Err(format!(
            "cannot be written by this user (mode {:04o})",
            mode & 0o7777
        ));
    }
    Ok(())
}

/// Bind a Unix socket with a random name in `dir` and give it a second name
/// with a hard link, as a master does (it binds `<path>.<random>`, then
/// links it to `<path>`, OpenSSH's `muxserver_listen`), then remove both: a
/// file system that holds no socket or no hard link (some network or FUSE
/// mounts) would otherwise fail every master's start.
///
/// # Arguments
///
/// * `dir` - The control directory
///
/// # Errors
///
/// Why the socket could not be created or linked.
#[cfg(unix)]
fn probe_socket(dir: &Path) -> std::io::Result<()> {
    let probe = tempfile::Builder::new()
        .prefix(".probe-")
        .rand_bytes(6)
        .make_in(dir, |path| std::os::unix::net::UnixListener::bind(path))?;
    let linked = probe.path().with_extension("link");
    let link = std::fs::hard_link(probe.path(), &linked);
    if link.is_ok() {
        std::fs::remove_file(&linked)?;
    }
    // Removes the socket file.
    probe.close()?;
    link
}

#[cfg(test)]
mod tests;
