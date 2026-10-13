//! What went wrong when `ssh` itself failed (plan §4.5's SSH codes), read
//! from its stderr, and the host key notices it prints on success.
//!
//! Pure: it reads an [`Execution`], never a process.
//!
//! **Only exit 255 is classified** ([`Exit::TransportFailed`]): ssh exits
//! with the remote command's status otherwise, and the scripts map their
//! own 255 to 254, so a remote program's stderr that happens to say
//! `Permission denied (publickey).` is never read as an ssh failure.
//!
//! **Phrases, not lines** (plan §3.5): OpenSSH's wording differs between
//! versions around the same phrase (`nodename nor servname provided` on
//! macOS, `Name or service not known` on Linux; Ubuntu adds `remove with:`
//! lines to the changed-key banner), so each class is a list of phrases
//! found in a line (a few only at its start, one as the whole line). The
//! phrases come from real stderr captured through the E2E harness with
//! OpenSSH 7.6p1 (Ubuntu 18.04, the plan's oldest client), 8.2p1, 9.6p1,
//! 10.0p2 and 10.3p1 (macOS), the transport's way (a generated `-F` file,
//! plan P3.3), jump hosts included, recaptured with 8.9p1, 9.2p1 and 10.6p1
//! too (audit 6), and from the client's sources at each of those releases
//! (plan §3.5 "Classification table"); the tests hold the captured ones
//! byte for byte and mark the few no run produced.
//!
//! **The cause is ssh's last message** (audit 6). ssh stops at its first
//! fatal error, and every message it logs ends with `\r\n` (`log.c`,
//! 7.6p1–10.6p1, Win32-OpenSSH too), while what it relays from the server,
//! a login banner or a remote startup file's stderr, ends as the server
//! wrote it (`\n`, verified with a banner on 7.6p1–10.6p1). So the class is
//! read from the last line that ends one of ssh's messages, skipping noise
//! and lines that only sum up the one before (`…: terminating, 1 bad
//! configuration options`, `Disconnected from …`); a banner that says
//! sessions are "timed out", or quotes `Host key verification failed.`,
//! decides nothing. In order:
//! - a jump host's `stdio forwarding failed` takes its class from the
//!   channel refusal before it (`timed out`, else unreachable), which is
//!   kept;
//! - ssh's closing `Host key verification failed.` takes it from the lines
//!   before it, most specific first: a changed host key (also one that
//!   differs from the key known for the server's address, and a revoked
//!   one), a host key refused for a reason no other class names (an
//!   invalid host certificate: trusting new keys would bypass it; an
//!   unreadable `RevokedHostKeys` file), an unknown host key, else (alone:
//!   a ProxyCommand's own ssh asking nobody) `ssh_failed`;
//! - else the message's own phrases: refused authentication, a timeout, an
//!   unreachable host, a lost connection (a line holding two classes'
//!   phrases is the first one's); a refused login keeps the line that says
//!   why when ssh printed one (a key file others can read, `… are too
//!   open.`, or one ssh cannot load, `Load key "…": invalid format`), and a
//!   run of messages of one class the most telling (`Protocol major
//!   versions differ: 2 vs. 1` before `banner exchange: …`);
//! - else, when the message holds the server's text over several lines
//!   (7.6p1's `Bad remote protocol version identification: '…'`), its
//!   first line.
//!
//! A script that printed its nonce line had connected, so its 255 is
//! always a lost connection.
//!
//! **Unrecognized** (`ssh_failed`): ssh's last message; when ssh printed
//! none (a remote shell that exited 255 after its startup output), the last
//! line, as text, whatever it says.
//!
//! **Noise is skipped** before matching: the `accept-new` notice, a remote
//! shell's locale warning, the pseudo-terminal notice, OpenSSH 10.1's
//! warning about a key exchange that is not post-quantum, an ssh_config
//! option this ssh lacks (`Unsupported option "gssapiauthentication"`) or
//! key file it misses (`no such identity`), and what ssh prints after its
//! jump host or ProxyCommand ended: `Connection closed by UNKNOWN port
//! 65535`, and before 9.5 `kex_exchange_identification: Connection closed by
//! remote host` (`ssh_exchange_identification:` before 8.0) as the last of
//! several lines (alone, it is a server that closed before its banner), so
//! the jump host's own line decides on every version.

use std::fmt;

use self::Phrase::{In, Is, Starts};
use super::capture::Captured;
use super::transport::{Execution, Exit};
use crate::error::Code;
use crate::problem::printable;

/// Most characters of a stderr line kept in a [`SshFailure`].
const LINE_MAX_CHARS: usize = 512;

/// Where a phrase must be in a (trimmed) stderr line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phrase {
    /// Anywhere in it.
    In(&'static str),
    /// At its start.
    Starts(&'static str),
    /// The whole line.
    Is(&'static str),
}

impl Phrase {
    /// Whether `line` holds this phrase where it must be.
    ///
    /// # Arguments
    ///
    /// * `line` - A trimmed stderr line
    fn found_in(self, line: &str) -> bool {
        match self {
            Self::In(phrase) => line.contains(phrase),
            Self::Starts(phrase) => line.starts_with(phrase),
            Self::Is(phrase) => line == phrase,
        }
    }
}

/// Lines that never explain a failure (plan §3.5 "Noise").
const NOISE: &[Phrase] = &[
    // `StrictHostKeyChecking accept-new` recording a key (exit 0), and
    // `CheckHostIP` recording an address before 8.5.
    In("Warning: Permanently added "),
    // A remote bash warning about a locale ssh forwarded (`SendEnv`).
    In(": warning: setlocale: "),
    In("Pseudo-terminal will not be allocated"),
    // Printed by ssh after its jump host's ProxyCommand ended: the cause is
    // the line before it.
    In("Connection closed by UNKNOWN port "),
    // OpenSSH 10.1+ on every connection whose key exchange is not
    // post-quantum (a server before 9.0, verified with 10.3p1 against
    // 8.2p1): three lines, before any failure, and alone when a session is
    // lost without a message. The first phrase also matches the one-line
    // warning of a rekeyed connection (`WARNING: rekeyed connection is not
    // using a post-quantum key exchange algorithm: "…"`, 10.6p1's kex.c;
    // not sampled).
    In("not using a post-quantum key exchange algorithm"),
    In("** This session may be vulnerable to \"store now, decrypt later\""),
    In("** The server may need to be upgraded. See https://openssh.com/pq.html"),
    // An ssh_config keyword this build does not support (`/etc/ssh/ssh_config
    // line 4: Unsupported option "gssapiauthentication"`, a build without
    // GSSAPI): reported, and ssh goes on (7.6p1–10.6p1).
    In(": Unsupported option \""),
    // An ssh_config `IdentityFile` that does not exist (`no such identity:
    // /home/u/.ssh/id_work: No such file or directory`): ssh tries the
    // next key. apvm's own key is checked before ssh runs.
    Starts("no such identity: "),
];

/// What ssh prints, as the last of several lines, when its jump host or
/// ProxyCommand ended without an SSH banner (`ssh_exchange_identification:`
/// before 8.0, `kex_exchange_identification:` from 8.0 to 9.4; 9.5 and later
/// print only [`NOISE`]'s `Connection closed by UNKNOWN port 65535`, which
/// 8.9p1–9.4p1 add after it; sources): the cause is a line before it. Alone, it
/// is a server that closed the connection before its banner (`MaxStartups`,
/// a firewall, OpenSSH 9.8's `PerSourcePenalties`), and it decides
/// [`SshFailureKind::Unreachable`].
const PROXY_ENDED: &str = "_exchange_identification: Connection closed by remote host";

/// ssh's last message on every host key refusal: `fatal("Host key
/// verification failed.")` (`sshconnect2.c`, 7.6p1–10.6p1), the server's or
/// a jump host's.
const HOST_KEY_REFUSED: Phrase = Is("Host key verification failed.");

/// Why ssh refused a server's host certificate (`sshkey.c`, 7.6p1–10.6p1).
const CERTIFICATE_REFUSALS: &[Phrase] = &[
    // `Certificate invalid: name is not a listed principal`, `…: expired`,
    // `…: not yet valid`, `…: not a host certificate`.
    In("Certificate invalid: "),
    // The other reasons (not sampled): `Certificate lacks principal list`,
    // `Certificate contains unsupported critical options` (7.6p1:
    // `Certificate for <host> contains unsupported critical options(s)`),
    // `Certificate signed with disallowed algorithm`.
    In("Certificate lacks principal list"),
    In("contains unsupported critical option"),
    In("Certificate signed with disallowed algorithm"),
];

/// One class's phrases.
struct Rule {
    /// The class.
    kind: SshFailureKind,
    /// The phrases, most telling first: the first one that a line holds
    /// names the line kept, unless an explanation is found.
    phrases: &'static [Phrase],
    /// Lines of ssh's (before its last message) that explain the class
    /// better than the deciding line, when ssh printed one: the first such
    /// line is kept instead.
    explanations: &'static [Phrase],
}

/// What the lines before ssh's closing `Host key verification failed.`
/// tell, in classification order (most specific first).
const HOST_KEY_RULES: &[Rule] = &[
    Rule {
        kind: SshFailureKind::HostKeyChanged,
        phrases: &[
            // `Host key for [127.0.0.1]:2722 has changed and you have
            // requested strict checking.` (every version, strict or
            // accept-new; `ED25519 host key for target has changed …` after
            // 7.6p1's and 8.2p1's `POSSIBLE DNS SPOOFING` banner)
            In(" has changed and you have requested strict checking"),
            // The banner, also before an unknown key whose SSHFP record in
            // DNS differs (`VerifyHostKeyDNS`, source).
            In("REMOTE HOST IDENTIFICATION HAS CHANGED"),
            // `Warning: the ECDSA host key for 'target' differs from the key
            // for the IP address '172.19.0.3'`: with `CheckHostIP yes` (the
            // default before 8.5), the address's known key is another one;
            // refused strict and accept-new alike (7.6p1, 8.2p1, 9.6p1,
            // 10.0p2, 10.6p1).
            In(" differs from the key for the IP address "),
            // A key the user marked revoked: `ED25519 host key for
            // [127.0.0.1]:2722 was revoked and you have requested strict
            // checking.` (an `@revoked` line in known_hosts, after its
            // banner), `Host key ED25519 SHA256:… revoked by file /k`
            // (`RevokedHostKeys`) (10.3p1; the first also on 7.6p1–10.6p1).
            In(" was revoked and you have requested strict checking"),
            In(" revoked by file "),
        ],
        explanations: &[],
    },
    Rule {
        // A host certificate an `@cert-authority` line rejects: neither a
        // new nor a changed key. ssh then retries the certificate's plain
        // key, which is new (`No ED25519 host key is known …` follows), so
        // `--trust-new-host-key` would bypass the CA: with accept-new the
        // call runs, and the plain key is recorded (sampled on 7.6p1–10.6p1;
        // [`HostKeyNotices::refused_certificate`]).
        kind: SshFailureKind::Unrecognized,
        phrases: CERTIFICATE_REFUSALS,
        explanations: &[],
    },
    Rule {
        // Other refusals that trusting new keys or forgetting the known one
        // would not fix.
        kind: SshFailureKind::Unrecognized,
        phrases: &[
            // A `RevokedHostKeys` file that cannot be read (`Error checking
            // host key ED25519 SHA256:… in revoked keys file /k: No such
            // file or directory`).
            In("Error checking host key "),
            // A certificate's plain key that `HostKeyAlgorithms` excludes
            // (`host key ssh-ed25519 not permitted by HostkeyAlgorithms`, not
            // sampled).
            In(" not permitted by HostkeyAlgorithms"),
        ],
        explanations: &[],
    },
    Rule {
        kind: SshFailureKind::HostKeyUnknown,
        phrases: &[
            // `No ED25519 host key is known for [127.0.0.1]:2722 and you
            // have requested strict checking.`: the one message of an
            // unknown key under `StrictHostKeyChecking yes`, the server's or
            // a jump host's (which reads apvm's options too). A bare `Host
            // key verification failed.` is another refusal (a ProxyCommand
            // of the user's asking nobody), which `--trust-new-host-key`
            // cannot reach: `ssh_failed`.
            In(" host key is known for "),
        ],
        explanations: &[],
    },
];

/// The classes ssh's last message shows by itself, in classification
/// order (a line holding phrases of two classes is the first one's).
const CAUSE_RULES: &[Rule] = &[
    Rule {
        kind: SshFailureKind::AuthFailed,
        phrases: &[
            // `deploy@127.0.0.1: Permission denied (publickey).` (a wrong
            // key and an unknown user read the same)
            In("Permission denied ("),
            // sshd's `MaxAuthTries` (default 6) reached with too many keys:
            // `Received disconnect from 172.19.0.3 port 22:2: Too many
            // authentication failures`.
            In("Too many authentication failures"),
        ],
        explanations: &[
            // ssh ignored the key file because its group or others may read
            // it (`Permissions 0644 for '/k' are too open.`, after `WARNING:
            // UNPROTECTED PRIVATE KEY FILE!`; the server then refuses the
            // login, unless an agent holds the same key).
            In(" are too open."),
            // ssh could not load the key file (`Load key "/k": invalid
            // format`: not a key, or one in another program's format).
            Starts("Load key \""),
        ],
    },
    Rule {
        kind: SshFailureKind::Timeout,
        phrases: &[
            // `…: Operation timed out` (macOS, musl), `…: Connection timed
            // out` (glibc), `Connection timed out during banner exchange`,
            // `Connection to 127.0.0.1 port 2722 timed out`.
            In("timed out"),
        ],
        explanations: &[],
    },
    Rule {
        kind: SshFailureKind::Unreachable,
        phrases: &[
            // Any failure to connect: `ssh: connect to host 127.0.0.1 port
            // 1: Connection refused`, `…: Network is unreachable` (no IPv6
            // route), `…: No route to host`, `…: Permission denied` (a
            // broadcast address, a local firewall), `…: Host is down`
            // (macOS) …; a timeout is the class before.
            In("ssh: connect to host "),
            In("Connection refused"),
            // `ssh: Could not resolve hostname x: nodename nor servname
            // provided, or not known` (macOS), `…: Name or service not
            // known` (glibc), `…: Name does not resolve` (musl).
            In("Could not resolve hostname"),
            In("Name or service not known"),
            In("nodename nor servname provided"),
            In("No route to host"),
            In("Network is unreachable"),
            // A jump host's last line when it opened no channel to the next
            // hop, without the channel line [`HOP_FAILED`] reads (not
            // sampled alone).
            Starts("stdio forwarding failed"),
            // The server closed or reset the connection before the SSH
            // banner, or sent something else: `kex_exchange_identification:
            // Connection closed by remote host` / `…: banner line contains
            // invalid characters` (8.2p1), `ssh_exchange_identification:
            // read: Connection reset by peer` (7.6p1: the name before 8.0).
            In("kex_exchange_identification:"),
            In("ssh_exchange_identification:"),
            // An SSH-1 server (`Protocol major versions differ: 2 vs. 1`), or
            // a banner ssh cannot read (`Bad remote protocol version
            // identification: '…'`, then 8.0+'s `banner exchange: Connection
            // to … port …: invalid format`).
            In("Protocol major versions differ"),
            In("Bad remote protocol version identification"),
            Starts("banner exchange: "),
            // `Connection closed by 127.0.0.1 port 8722`: something that is
            // no SSH server, or one that refused the session (also one that
            // closed it during authentication: ssh prints the same).
            In("Connection closed by "),
            // `Connection reset by 127.0.0.1 port 2222`: reset during the
            // handshake (a `…: Connection reset by peer` line elsewhere is a
            // lost connection).
            Starts("Connection reset by "),
        ],
        explanations: &[],
    },
    Rule {
        kind: SshFailureKind::ConnectionLost,
        phrases: &[
            // `Connection to target closed by remote host.`
            In("closed by remote host"),
            // `Timeout, server target not responding.` (ServerAlive*)
            In("Timeout, server "),
            // A write to, or read from, a dead connection: `client_loop:
            // send disconnect: Broken pipe` (8.2p1, a session's sshd killed).
            In("client_loop: "),
            In("Broken pipe"),
            In("Connection reset by peer"),
        ],
        explanations: &[],
    },
];

/// A jump host's last message when it could not open the channel to the
/// next hop (7.6p1–10.6p1): the line before it, [`CHANNEL_REFUSED`], says
/// why (`channel 0: open failed: connect failed: Connection refused`,
/// `…: Name or service not known`, `…: administratively prohibited: open
/// failed`), and decides: `timed out`, else unreachable. A channel refusal
/// without it is the server's own session refusal (`MaxSessions 0`:
/// `channel 0: open failed: connect failed: open failed`, alone):
/// `ssh_failed`.
const HOP_FAILED: Phrase = Starts("stdio forwarding failed");

/// A jump host's channel refusal, before [`HOP_FAILED`].
const CHANNEL_REFUSED: Phrase = In(": open failed: ");

/// The one ssh message known to hold the server's own text, newlines
/// included: 7.6p1's `Bad remote protocol version identification:
/// 'SSH-garbage\n\n'` (later versions cut the text at its first line end).
/// Its first line names it, so it decides when the message's last line
/// says nothing.
const SERVER_TEXT: Phrase = Starts("Bad remote protocol version identification: ");

/// Lines that only sum up the one before them, so never the cause:
/// `/home/u/.ssh/config: terminating, 1 bad configuration options` after
/// `/home/u/.ssh/config line 3: Bad configuration option: usekeychain`,
/// `Disconnected from 10.0.0.5 port 22` after `Received disconnect from
/// 10.0.0.5 port 22:2: <reason>`.
const SUMMARIES: &[Phrase] = &[In(": terminating, "), Starts("Disconnected from ")];

/// Why ssh failed, from its stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SshFailureKind {
    /// The server's host key differs from the one known for it, or from
    /// the one known for its address (`CheckHostIP`), or is one marked
    /// revoked: a possible man-in-the-middle, or a reinstalled server
    /// (`ssh_host_key_changed`).
    HostKeyChanged,
    /// No host key is known for the server (or a jump host), and new ones
    /// are not trusted on this call (`ssh_host_key_unknown`).
    HostKeyUnknown,
    /// The server refused the login: wrong key, unknown user, or too many
    /// keys offered (`ssh_auth_failed`).
    AuthFailed,
    /// The connection or the SSH handshake timed out (`ssh_timeout`).
    Timeout,
    /// The host could not be reached over SSH: refused, unknown name, no
    /// route, not an SSH server, or a jump host that could not reach it
    /// (`ssh_unreachable`).
    Unreachable,
    /// The connection was lost after it was made: the remote command may
    /// have run in part (`ssh_connection_lost`).
    ConnectionLost,
    /// ssh failed for a reason no other class names (`ssh_failed`): one
    /// apvm does not recognize, or a host key refused for a reason that
    /// trusting new keys or forgetting the known one would not fix (an
    /// invalid host certificate); the kept line says what ssh printed.
    Unrecognized,
}

impl SshFailureKind {
    /// The stable code (plan §4.5).
    pub const fn code(self) -> Code {
        match self {
            Self::HostKeyChanged => Code::SshHostKeyChanged,
            Self::HostKeyUnknown => Code::SshHostKeyUnknown,
            Self::AuthFailed => Code::SshAuthFailed,
            Self::Timeout => Code::SshTimeout,
            Self::Unreachable => Code::SshUnreachable,
            Self::ConnectionLost => Code::SshConnectionLost,
            Self::Unrecognized => Code::SshFailed,
        }
    }

    /// What happened, as the start of a message.
    const fn sentence(self) -> &'static str {
        match self {
            Self::HostKeyChanged => "the server's host key has changed",
            Self::HostKeyUnknown => "the server's host key is not known",
            Self::AuthFailed => "the server refused the SSH login",
            Self::Timeout => "the SSH connection timed out",
            Self::Unreachable => "the server cannot be reached over SSH",
            Self::ConnectionLost => "the SSH connection was lost",
            Self::Unrecognized => "ssh failed",
        }
    }
}

/// An ssh failure: its class, and the stderr line that shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshFailure {
    /// Its class.
    kind: SshFailureKind,
    /// The stderr line that decided it (or, unrecognized, the last one),
    /// control characters escaped and cut at 512 characters.
    line: Option<String>,
}

impl SshFailure {
    /// Its class.
    pub fn kind(&self) -> SshFailureKind {
        self.kind
    }

    /// Its stable code.
    pub fn code(&self) -> Code {
        self.kind.code()
    }

    /// The stderr line that shows it, if ssh printed one: escaped remote
    /// or local text, never parsed further.
    pub fn line(&self) -> Option<&str> {
        self.line.as_deref()
    }
}

impl fmt::Display for SshFailure {
    /// `<what happened>: <the line>`, or just what happened.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.line {
            Some(line) => write!(f, "{}: {line}", self.kind.sentence()),
            None => f.write_str(self.kind.sentence()),
        }
    }
}

/// Why `execution` failed in ssh itself, if it did.
///
/// # Arguments
///
/// * `execution` - A call through the SSH transport
///
/// # Returns
///
/// `None` unless ssh exited 255 ([`Exit::TransportFailed`]): any other exit
/// is the remote command's, whatever its stderr says.
pub fn classify(execution: &Execution) -> Option<SshFailure> {
    if execution.exit() != Exit::TransportFailed {
        return None;
    }
    Some(failure_of(
        execution.script_started(),
        execution.stderr().bytes(),
    ))
}

/// A meaningful stderr line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    /// The line, trimmed (`\r` included).
    text: String,
    /// Whether it ended with `\r\n`, as every message ssh logs does (the
    /// last line of a message, whose other lines end with `\n`); text ssh
    /// relays from the server (a login banner, a remote command's stderr)
    /// ends as the server wrote it.
    ends_message: bool,
}

/// The failure an exit 255 with `stderr` shows (see the module docs).
///
/// # Arguments
///
/// * `started` - Whether the script printed its nonce line (so the
///   connection had been made)
/// * `stderr` - ssh's stderr, as kept
fn failure_of(started: bool, stderr: &[u8]) -> SshFailure {
    let lines = meaningful_lines(stderr);
    let Some(cause) = cause(&lines) else {
        // Nothing of ssh's own: no class; the last line, as text.
        let kind = if started {
            SshFailureKind::ConnectionLost
        } else {
            SshFailureKind::Unrecognized
        };
        return failure(kind, last_telling(&lines).map(|i| lines[i].text.as_str()));
    };
    if started {
        return failure(SshFailureKind::ConnectionLost, Some(&lines[cause].text));
    }
    let (kind, line) = decide(&lines, cause);
    failure(kind, Some(&lines[line].text))
}

/// The class `lines[cause]`, ssh's last message, shows, and the line that
/// shows it best: a jump host's channel refusal, a host key refusal, the
/// message's own class, or the server's text in it; else unrecognized.
///
/// # Arguments
///
/// * `lines` - The meaningful lines
/// * `cause` - The index of ssh's last message ([`cause`])
fn decide(lines: &[Line], cause: usize) -> (SshFailureKind, usize) {
    hop_refusal(lines, cause)
        .or_else(|| host_key_refusal(lines, cause))
        .or_else(|| own_class(lines, cause))
        .or_else(|| server_text(lines, cause))
        .unwrap_or((SshFailureKind::Unrecognized, cause))
}

/// A jump host that opened no channel to the next hop ([`HOP_FAILED`]
/// after [`CHANNEL_REFUSED`]): a timeout when the channel line says so,
/// else unreachable; the channel line is kept.
///
/// # Arguments
///
/// * `lines` - The meaningful lines
/// * `cause` - The index of ssh's last message
fn hop_refusal(lines: &[Line], cause: usize) -> Option<(SshFailureKind, usize)> {
    if !HOP_FAILED.found_in(&lines[cause].text) {
        return None;
    }
    let channel = cause
        .checked_sub(1)
        .filter(|&before| CHANNEL_REFUSED.found_in(&lines[before].text))?;
    let kind = if lines[channel].text.contains("timed out") {
        SshFailureKind::Timeout
    } else {
        SshFailureKind::Unreachable
    };
    Some((kind, channel))
}

/// A host key refusal (ssh's last message is [`HOST_KEY_REFUSED`]): the
/// first [`HOST_KEY_RULES`] class a line before it shows, with that line;
/// else (the refusal alone) unrecognized.
///
/// # Arguments
///
/// * `lines` - The meaningful lines
/// * `cause` - The index of ssh's last message
fn host_key_refusal(lines: &[Line], cause: usize) -> Option<(SshFailureKind, usize)> {
    if !HOST_KEY_REFUSED.found_in(&lines[cause].text) {
        return None;
    }
    let before = &lines[..cause];
    let class = HOST_KEY_RULES
        .iter()
        .find_map(|rule| first_holding(before, rule.phrases).map(|line| (rule.kind, line)));
    Some(class.unwrap_or((SshFailureKind::Unrecognized, cause)))
}

/// The first [`CAUSE_RULES`] class ssh's last message holds a phrase of,
/// and the line kept: an explanation ssh printed before it, else the most
/// telling of the message and the messages of the same class right before
/// it ([`most_telling`]).
///
/// # Arguments
///
/// * `lines` - The meaningful lines
/// * `cause` - The index of ssh's last message
fn own_class(lines: &[Line], cause: usize) -> Option<(SshFailureKind, usize)> {
    let rule = CAUSE_RULES
        .iter()
        .find(|rule| holds_any(&lines[cause].text, rule.phrases))?;
    let line = explanation(&lines[..cause], rule.explanations)
        .unwrap_or_else(|| most_telling(lines, cause, rule.phrases));
    Some((rule.kind, line))
}

/// The server's text inside ssh's last message ([`SERVER_TEXT`]): its
/// first line, which names the failure, when the message's last line says
/// nothing (unreachable).
///
/// # Arguments
///
/// * `lines` - The meaningful lines
/// * `cause` - The index of ssh's last message
fn server_text(lines: &[Line], cause: usize) -> Option<(SshFailureKind, usize)> {
    let start = lines[..cause]
        .iter()
        .rposition(|line| line.ends_message)
        .map_or(0, |end| end + 1);
    lines[start..cause]
        .iter()
        .position(|line| SERVER_TEXT.found_in(&line.text))
        .map(|line| (SshFailureKind::Unreachable, start + line))
}

/// The first of ssh's own lines in `before` that holds one of
/// `explanations`.
///
/// # Arguments
///
/// * `before` - The meaningful lines before ssh's last message
/// * `explanations` - The rule's explanations
fn explanation(before: &[Line], explanations: &[Phrase]) -> Option<usize> {
    before
        .iter()
        .position(|line| line.ends_message && holds_any(&line.text, explanations))
}

/// The most telling line of ssh's last message `lines[cause]` and the
/// messages right before it that hold phrases of the same class (`Protocol
/// major versions differ: 2 vs. 1` before `banner exchange: …`): the first
/// one holding the first phrase any of them holds.
///
/// # Arguments
///
/// * `lines` - The meaningful lines
/// * `cause` - The index of ssh's last message
/// * `phrases` - The class's phrases, most telling first
fn most_telling(lines: &[Line], cause: usize, phrases: &[Phrase]) -> usize {
    let mut start = cause;
    while start > 0 && {
        let before = &lines[start - 1];
        before.ends_message && holds_any(&before.text, phrases)
    } {
        start -= 1;
    }
    first_holding(&lines[start..=cause], phrases).map_or(cause, |line| start + line)
}

/// The index of the first line holding the first of `phrases` that any of
/// `lines` holds.
///
/// # Arguments
///
/// * `lines` - Meaningful lines
/// * `phrases` - The phrases, most telling first
fn first_holding(lines: &[Line], phrases: &[Phrase]) -> Option<usize> {
    phrases
        .iter()
        .find_map(|phrase| lines.iter().position(|line| phrase.found_in(&line.text)))
}

/// The index of ssh's last message: the last line that ends one of ssh's
/// messages ([`Line::ends_message`]) and does not only sum up the one
/// before it ([`SUMMARIES`]), else the last that ends one; `None` when ssh
/// printed none.
///
/// # Arguments
///
/// * `lines` - The meaningful lines
fn cause(lines: &[Line]) -> Option<usize> {
    let ends = || {
        lines
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, line)| line.ends_message)
    };
    ends()
        .find(|(_, line)| !holds_any(&line.text, SUMMARIES))
        .or_else(|| ends().next())
        .map(|(index, _)| index)
}

/// The index of the last line that does not only sum up the one before
/// it, else of the last line, whoever printed it.
///
/// # Arguments
///
/// * `lines` - The meaningful lines
fn last_telling(lines: &[Line]) -> Option<usize> {
    lines
        .iter()
        .rposition(|line| !holds_any(&line.text, SUMMARIES))
        .or_else(|| lines.len().checked_sub(1))
}

/// A failure of `kind`, its line made printable and cut.
///
/// # Arguments
///
/// * `kind` - The class
/// * `line` - The line that shows it
fn failure(kind: SshFailureKind, line: Option<&str>) -> SshFailure {
    SshFailure {
        kind,
        line: line.map(shown),
    }
}

/// `line` for messages: control characters escaped, at most
/// [`LINE_MAX_CHARS`] characters (`…` marks a cut).
///
/// # Arguments
///
/// * `line` - A stderr line
fn shown(line: &str) -> String {
    let mut kept: String = line.chars().take(LINE_MAX_CHARS).collect();
    if kept.len() < line.len() {
        kept.push('…');
    }
    printable(&kept)
}

/// The non-empty, non-noise lines of `stderr`, trimmed, each with whether
/// it ended one of ssh's messages, without a last [`PROXY_ENDED`] line
/// that follows others.
///
/// # Arguments
///
/// * `stderr` - ssh's stderr
fn meaningful_lines(stderr: &[u8]) -> Vec<Line> {
    let mut lines: Vec<Line> = String::from_utf8_lossy(stderr)
        .split_inclusive('\n')
        .filter_map(|raw| {
            let text = raw.trim();
            (!text.is_empty() && !is_noise(text)).then(|| Line {
                text: text.to_string(),
                ends_message: raw.ends_with("\r\n"),
            })
        })
        .collect();
    if lines.len() > 1
        && lines
            .last()
            .is_some_and(|line| line.text.contains(PROXY_ENDED))
    {
        lines.pop();
    }
    lines
}

/// Whether `line` is noise ([`NOISE`]): never the reason ssh failed.
///
/// # Arguments
///
/// * `line` - A trimmed stderr line
pub(crate) fn is_noise(line: &str) -> bool {
    holds_any(line, NOISE)
}

/// Whether `line` holds any of `phrases`.
///
/// # Arguments
///
/// * `line` - A trimmed stderr line
/// * `phrases` - The phrases
fn holds_any(line: &str, phrases: &[Phrase]) -> bool {
    phrases.iter().any(|phrase| phrase.found_in(line))
}

/// What ssh said about host keys on a call that connected: keys it
/// recorded (`StrictHostKeyChecking accept-new`), whether one could not be
/// saved, and a host certificate it refused first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostKeyNotices {
    /// The hosts whose new key ssh recorded, as it names them
    /// (`[127.0.0.1]:2722`, `localhost`; with `CheckHostIP yes`, the
    /// default before OpenSSH 8.5, a name and its address:
    /// `target,172.19.0.3`, verified on 7.6p1 and 8.2p1), escaped, in
    /// order: the server and every jump host trusted for the first time.
    pub recorded: Vec<String>,
    /// Whether ssh accepted a new key that it could not write to the
    /// known_hosts file (`Failed to add the host to the list of known
    /// hosts`): it was trusted for this call only, and will be new again.
    pub not_saved: bool,
    /// Why ssh refused a host certificate on this call (`Certificate
    /// invalid: expired`), escaped: with `accept-new`, ssh then trusts the
    /// certificate's plain key as a new one (OpenSSH's own fallback), so a
    /// key in [`Self::recorded`] next to it bypassed the certificate
    /// authority the user trusts (sampled on 7.6p1–10.3p1).
    pub refused_certificate: Option<String>,
}

/// The host key notices in `stderr`: in ssh's own lines, which end with
/// `\r\n` (see the module docs).
///
/// # Arguments
///
/// * `stderr` - ssh's stderr
///
/// # Example
///
/// ```
/// use apvm_sites::remote::{Captured, host_key_notices};
///
/// let stderr = Captured::whole(
///     b"Warning: Permanently added '[127.0.0.1]:2722' (ED25519) to the list of known hosts.\r\n"
///         .to_vec(),
/// );
/// let notices = host_key_notices(&stderr);
/// assert_eq!(notices.recorded, ["[127.0.0.1]:2722"]);
/// assert!(!notices.not_saved);
/// ```
pub fn host_key_notices(stderr: &Captured) -> HostKeyNotices {
    let text = String::from_utf8_lossy(stderr.bytes());
    let mut notices = HostKeyNotices::default();
    // ssh's own lines only (`\r\n`): a remote command's stderr copying one
    // reports nothing.
    for line in text
        .split_inclusive('\n')
        .filter(|raw| raw.ends_with("\r\n"))
    {
        if let Some(host) = recorded_host(line) {
            notices.recorded.push(printable(host));
        }
        notices.not_saved |= line.contains("Failed to add the host to the list of known hosts");
        let line = line.trim();
        if notices.refused_certificate.is_none() && holds_any(line, CERTIFICATE_REFUSALS) {
            notices.refused_certificate = Some(shown(line));
        }
    }
    notices
}

/// The host of a `Warning: Permanently added '<host>' (<type>) to the list
/// of known hosts.` line.
///
/// # Arguments
///
/// * `line` - A stderr line
fn recorded_host(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("Warning: Permanently added '")?;
    let (host, tail) = rest.split_once("' (")?;
    tail.contains("to the list of known hosts").then_some(host)
}

#[cfg(test)]
mod tests;
