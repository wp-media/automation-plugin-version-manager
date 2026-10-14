//! The shared connection's master, kept out of every call's process tree
//! (P3.4, audit rounds 2–5; design in `remote/mux.rs`).
//!
//! Before a call that shares connections, under its configuration's gate
//! (one per control directory and tag in the process, [`gate_for`]), apvm
//! looks behind the control socket (`socket.rs`: a live master says hello
//! at once). With no master there, the starter runs: `ssh -F <master file>
//! -N <destination>` (`ControlMaster auto`, `ControlPersist`), whose ssh
//! authenticates, forks the master and exits at once, running no remote
//! command (verified on 10.3p1: exit 0 in 0.1 s, the master live, the
//! server's host key notice on its stderr).
//!
//! **A stuck master** (stopped, wedged) leaves a connection to its socket
//! queued and unanswered: no hello within 3 s. A call that met it would wait for the connect
//! timeout and print `muxclient: master hello exchange failed` (9.6 and
//! later), or hang (8.9p1, 9.2p1, audit round 4). So its socket is removed
//! (it is this configuration's, in the checked control directory), and the
//! call runs on a connection of its own, its file saying `ControlPath none`
//! ([`Ready::Stuck`]); the next call starts a new master. ssh's own hello
//! failure line in a call's stderr (ending `\r\n`, as ssh's messages do;
//! a remote program would have to print it so on purpose, and could then
//! only cost a master its socket) removes the socket too. A stuck master that later resumes and exits removes its
//! path, which is then its successor's socket: the successor runs on,
//! unreachable, until idle, and the next call starts another master (a
//! documented churn; apvm cannot tell the stuck master's process apart to
//! end it first). After a call that timed out before its script started,
//! apvm stops its master (`ssh -O stop`).
//!
//! **Races** between starters of other processes: ssh lets one bind the
//! socket; the other prints `ControlSocket … already exists, disabling
//! multiplexing` and would stay a plain `ssh -N` until killed (verified on
//! 8.9p1 by audit round 4): apvm kills it on that line and looks again, and
//! a call that finds the other's master rides it. A starter that finds a
//! master live by the time it connects runs as its client, and its `-N`
//! opens a session (a login shell with no stdin, which ends at once;
//! verified on 10.3p1). A master that exits removes its own socket; a stale
//! one (a master killed) refuses connections and the starter replaces it.
//!
//! Every command here has a timeout of its own (`command.rs`); none fails
//! the call, except a starter that could not connect (exit 255 with a
//! reason [`classify`](super::super::classify()) knows): the call would fail
//! the same way. Any other 255 (a master that could not set up its socket)
//! lets the call run on its own connection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::command::{Commands, run, run_until, ssh_said, ssh_said_ending};
use super::socket::{self, Probe};

/// How long `ssh -O stop` may take: a live master answers at once.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// What ssh prints when a master accepted the connection to its socket but
/// did not answer the hello exchange (9.6 and later, after the connect
/// timeout).
const HELLO_FAILED: &str = "muxclient: master hello exchange failed";

/// The end of what a starter prints when another master took the socket
/// first; it would then wait as a plain `ssh -N`.
const LOST_RACE: &str = "already exists, disabling multiplexing";

/// Where a transport's master is, and how to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shared {
    /// The starter's generated file text (`ControlMaster auto`).
    pub(super) master_config: String,
    /// The file text for a call that must not touch the socket (a stuck
    /// master's): `ControlMaster no`, `ControlPath none`.
    pub(super) own_config: String,
    /// The control directory, checked.
    pub(super) control_dir: PathBuf,
    /// The configuration's tag, which starts its sockets' names.
    pub(super) tag: String,
    /// The control socket a call uses, as `ssh -G` expands it (one of this
    /// configuration's, [`socket::ours`]).
    pub(super) socket: PathBuf,
}

impl Shared {
    /// The key of this configuration's gate: its control directory and tag.
    pub(super) fn gate_key(&self) -> String {
        format!("{}\u{0}{}", self.control_dir.display(), self.tag)
    }
}

/// What getting a master ready found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ready {
    /// A master said hello behind the socket.
    Live,
    /// The starter ran (or another's master is live): the starter's
    /// stderr (host key notices a caller must see).
    Started(Vec<u8>),
    /// The starter could not connect (ssh exited 255): the call would not
    /// either. Its stderr, and how long it took.
    Unreachable {
        /// The starter's stderr.
        stderr: Vec<u8>,
        /// How long it ran.
        elapsed: Duration,
    },
    /// The starter did not finish in time, could not run, or failed for
    /// another reason: the call goes on, on a master if one came up, else
    /// on its own connection. What the starter printed.
    Unknown(Vec<u8>),
    /// A stuck master answered nothing: its socket was removed, and the
    /// call runs on a connection of its own, away from the socket.
    Stuck,
}

impl Ready {
    /// Whether calls that waited while this was found take it as theirs
    /// instead of trying again: the server could not be reached, did not
    /// answer in time, or its master is stuck, a moment ago (a call that
    /// comes later tries again).
    pub(super) fn shared_with_waiters(&self) -> bool {
        matches!(
            self,
            Self::Unreachable { .. } | Self::Unknown(_) | Self::Stuck
        )
    }

    /// This result for a call that waited for it: without the host key
    /// notices, which only the call whose starter met them reports.
    pub(super) fn for_waiter(&self) -> Self {
        match self {
            Self::Unreachable { stderr, elapsed } => Self::Unreachable {
                stderr: without_notices(stderr),
                elapsed: *elapsed,
            },
            Self::Unknown(stderr) => Self::Unknown(without_notices(stderr)),
            other => other.clone(),
        }
    }
}

/// `stderr` without ssh's host key notices (`Warning: Permanently added …`,
/// `Failed to add the host …`).
///
/// # Arguments
///
/// * `stderr` - ssh's stderr
fn without_notices(stderr: &[u8]) -> Vec<u8> {
    stderr
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| {
            let text = String::from_utf8_lossy(line);
            !text.contains("Warning: Permanently added ")
                && !text.contains("Failed to add the host to the list of known hosts")
        })
        .flatten()
        .copied()
        .collect()
}

/// What a configuration's last master start found, so the calls that
/// waited for it share a failure instead of each meeting it again (audit
/// round 3).
#[derive(Debug, Default)]
pub(super) struct Gate {
    /// How many starts have been recorded.
    generation: u64,
    /// What the last one found.
    last: Option<Ready>,
}

impl Gate {
    /// How many starts have been recorded.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// The last start's result, for a call that asked when `ticket` starts
    /// had been recorded, if one was since and is shared with waiters.
    ///
    /// # Arguments
    ///
    /// * `ticket` - The generation the call saw before it waited
    pub(super) fn shared_since(&self, ticket: u64) -> Option<Ready> {
        if self.generation == ticket {
            return None;
        }
        self.last
            .as_ref()
            .filter(|last| last.shared_with_waiters())
            .map(Ready::for_waiter)
    }

    /// Record what getting a master ready found (a live master starts
    /// nothing, so it changes nothing).
    ///
    /// # Arguments
    ///
    /// * `ready` - The result
    pub(super) fn record(&mut self, ready: &Ready) {
        if *ready == Ready::Live {
            return;
        }
        self.generation += 1;
        self.last = Some(ready.clone());
    }
}

/// A configuration's gate and its generation, readable without the lock.
#[derive(Debug, Default)]
pub(super) struct GateCell {
    /// The gate, held while a call checks and starts a master.
    pub(super) gate: tokio::sync::Mutex<Gate>,
    /// The gate's generation, read before waiting for it.
    pub(super) generation: AtomicU64,
}

impl GateCell {
    /// The generation a call sees before it waits.
    pub(super) fn ticket(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Record `ready` in `gate` (held) and publish its generation.
    ///
    /// # Arguments
    ///
    /// * `gate` - The held gate
    /// * `ready` - The result
    pub(super) fn record(&self, gate: &mut Gate, ready: &Ready) {
        gate.record(ready);
        self.generation.store(gate.generation(), Ordering::SeqCst);
    }
}

/// The process-wide gate of the configuration `key` ([`Shared::gate_key`]):
/// every transport with the same options shares it, so they start one
/// master. One per configuration used; never removed (a few bytes each).
///
/// # Arguments
///
/// * `key` - The configuration's control directory and tag
pub(super) fn gate_for(key: &str) -> Arc<GateCell> {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<GateCell>>>> = OnceLock::new();
    let gates = GATES.get_or_init(Mutex::default);
    // Entries are only ever added whole, so a poisoned map is still sound.
    let mut gates = gates.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(gates.entry(key.to_string()).or_default())
}

/// Make sure a master is live for the call's connection: look behind its
/// socket, clear a stuck master, start one when there is none (see the
/// module docs).
///
/// # Arguments
///
/// * `starter` - The starter's commands (its own private directory)
/// * `shared` - The master's configuration
/// * `master_file` - The starter's generated file, as ssh gets it
/// * `start_timeout` - How long the starter may take
pub(super) async fn ensure(
    starter: &Commands<'_>,
    shared: &Shared,
    master_file: &Path,
    start_timeout: Duration,
) -> Ready {
    match socket::probe(&shared.socket).await {
        Probe::Live => Ready::Live,
        Probe::Stuck => stuck(shared),
        Probe::Absent => start(starter, shared, master_file, start_timeout).await,
    }
}

/// A stuck master: remove its socket.
///
/// # Arguments
///
/// * `shared` - The master's configuration
fn stuck(shared: &Shared) -> Ready {
    socket::remove(&shared.socket);
    Ready::Stuck
}

/// Run the starter; when another master took the socket first, stop it
/// and look again.
///
/// # Arguments
///
/// * `starter` - The starter's commands
/// * `shared` - The master's configuration
/// * `master_file` - The starter's generated file, as ssh gets it
/// * `limit` - How long it may take
async fn start(
    starter: &Commands<'_>,
    shared: &Shared,
    master_file: &Path,
    limit: Duration,
) -> Ready {
    let started = Instant::now();
    let command = starter.command(master_file, &["-N"], &[]);
    let finished = run_until(command, limit, |stderr| ssh_said_ending(stderr, LOST_RACE)).await;
    if finished.stopped {
        return match socket::probe(&shared.socket).await {
            Probe::Live => Ready::Started(finished.stderr),
            Probe::Stuck => stuck(shared),
            Probe::Absent => Ready::Unknown(finished.stderr),
        };
    }
    match finished.status.and_then(|status| status.code()) {
        Some(255) => Ready::Unreachable {
            stderr: finished.stderr,
            elapsed: started.elapsed(),
        },
        Some(_) => Ready::Started(finished.stderr),
        None => Ready::Unknown(finished.stderr),
    }
}

/// After a call that shares connections: remove a stuck master's socket the
/// call met (ssh's own hello failure line: the call then went on over a
/// connection of its own, so its script may well have started), else stop
/// a master the call may have stalled on (it timed out before its script
/// started). Best effort.
///
/// # Arguments
///
/// * `commands` - The call's commands
/// * `shared` - The master's configuration
/// * `stalled` - Whether the call timed out before its script started
/// * `stderr` - The call's stderr
pub(super) async fn after(commands: &Commands<'_>, shared: &Shared, stalled: bool, stderr: &[u8]) {
    if ssh_said(stderr, HELLO_FAILED) {
        socket::remove(&shared.socket);
        return;
    }
    if stalled {
        run(
            commands.command(commands.file, &["-O", "stop"], &[]),
            STOP_TIMEOUT,
        )
        .await;
    }
}

#[cfg(test)]
mod tests;
