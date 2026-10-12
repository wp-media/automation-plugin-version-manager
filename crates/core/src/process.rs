//! Running child processes that never outlive the work that started them.
//!
//! Every external program apvm runs (git, `gh`, a builder's shell steps) is
//! awaited as part of a future. When that future is dropped instead of
//! finishing — a cancelled build, a Node.js worker that exited mid-call, a
//! runtime shutting down — what it started must stop too, or it keeps
//! running unobserved: a `git clone` writing into a deleted workspace, a
//! `composer install` nobody waits for.
//!
//! [`output`] is the one way core runs a [`command`]: if its future is
//! dropped before the child exits, the child **and every process it
//! started** are killed — git's transport helpers, the tools a shell step
//! runs. The workspace's `clippy.toml` forbids `tokio::process::Command`'s
//! own constructor and runners, so no async spawn bypasses this (std's
//! blocking `Command` is left to tests and to this module's own helpers).
//!
//! The tree is found by process ancestry rather than by putting children in
//! their own process group: a separate group would stop a terminal's Ctrl+C
//! from reaching them, orphaning build tools whenever apvm (or the Node.js
//! process hosting it) is interrupted. On Unix the whole tree is first
//! stopped (`SIGSTOP`) so it cannot grow, then killed (`SIGKILL`), through
//! the `kill` system call and the process table (`/proc` on Linux, `/bin/ps`
//! elsewhere): no `kill` or `ps` program is needed on Linux, where slim
//! container images have neither. On Windows only the child itself is
//! killed (`taskkill /F`): `taskkill /T` finds children by a parent PID that
//! Windows never clears when the parent exits, so a recycled PID would make
//! it kill unrelated processes. This runs only on that cancellation path,
//! never when a command finishes normally.
//!
//! It is best-effort. Not killed, because not found or not allowed: a
//! process that outlives its own parent (a daemon a tool starts), which has
//! left the tree; the descendants, when the process table cannot be read; a
//! tree still growing after a bounded number of snapshots, beyond what they
//! found; a descendant apvm may not signal (e.g. a setuid tool). If the host
//! process itself ends between the stop and the kill, the stopped processes
//! stay stopped.
//!
//! Dropping the future is not always possible in time: a Node.js main thread
//! calling `process.exit()` ends the process without dropping anything. An
//! embedder can therefore run its work inside a [`ProcessScope`], which
//! records every process tree started within it, and [`ProcessScope::kill_all`]
//! kills them synchronously at that moment.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::{BorrowedHandle, OwnedHandle};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(unix)]
use rustix::process::{Pid, Signal, kill_process};
#[cfg(target_os = "linux")]
use rustix::process::{PidfdFlags, pidfd_open, pidfd_send_signal};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

tokio::task_local! {
    /// The scope the current task's work runs in, if any ([`ProcessScope::run`]).
    static CURRENT_SCOPE: ProcessScope;
}

/// Records the process trees started by one owner's work — e.g. all calls
/// of one Node.js environment — so they can be killed at once, synchronously,
/// when that owner goes away ([`Self::kill_all`]).
///
/// Cheap to clone: clones share the same set.
#[derive(Clone, Debug, Default)]
pub struct ProcessScope {
    /// Shared with every clone.
    state: Arc<Mutex<ScopeState>>,
}

/// The mutable part of a [`ProcessScope`].
#[derive(Debug, Default)]
struct ScopeState {
    /// The root of every tree running in the scope, by registration. Keyed
    /// by registration, not PID: a PID freed and reused by another of the
    /// scope's processes must not merge or drop the two entries.
    running: HashMap<u64, Root>,
    /// The key of the next registration.
    next_key: u64,
    /// Set by [`ProcessScope::kill_all`]: no new process may start.
    closed: bool,
}

impl ProcessScope {
    /// An empty, open scope.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `work` so that every process it starts through apvm (git, `gh`,
    /// build steps) is recorded in this scope while it runs.
    ///
    /// # Arguments
    ///
    /// * `work` - The future to run (its spawned tasks are not covered;
    ///   apvm runs its processes in the calling task)
    ///
    /// # Returns
    ///
    /// `work`'s output.
    pub async fn run<F: Future>(&self, work: F) -> F::Output {
        CURRENT_SCOPE.scope(self.clone(), work).await
    }

    /// Kill every process tree running in this scope, and close the scope:
    /// processes started in it afterwards are refused (or killed at once).
    ///
    /// A root that ends on its own meanwhile is not mistaken for another
    /// process that gets its PID: on Linux its pidfd names it, on Windows
    /// an open handle keeps the PID reserved, and elsewhere it must still be
    /// listed as this process's child.
    ///
    /// Blocks briefly (it reads the process table, or runs `taskkill`, per
    /// tree), so call it where that is acceptable — e.g. an environment's
    /// teardown or a process `exit` handler. Idempotent.
    pub fn kill_all(&self) {
        let roots: Vec<Root> = {
            let mut state = self.lock();
            state.closed = true;
            state.running.drain().map(|(_, root)| root).collect()
        };
        for root in &roots {
            kill_tree(root.pid, Pin::Recorded(root));
        }
    }

    /// Whether [`Self::kill_all`] was called.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Record a started tree.
    ///
    /// # Arguments
    ///
    /// * `root` - The tree's root (the started child, not yet reaped)
    ///
    /// # Returns
    ///
    /// The key to [`Self::unregister`] it with, or `None` (not recorded) when
    /// the scope is closed, so the caller kills it.
    fn register(&self, root: Root) -> Option<u64> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        let key = state.next_key;
        state.next_key = key.wrapping_add(1);
        state.running.insert(key, root);
        Some(key)
    }

    /// Forget a tree whose root was reaped (or killed): from then on its PID
    /// may name another process.
    ///
    /// # Arguments
    ///
    /// * `key` - What [`Self::register`] returned
    fn unregister(&self, key: u64) {
        self.lock().running.remove(&key);
    }

    /// The state, even if a panicking holder poisoned the lock (the set
    /// stays consistent: every update is a single operation).
    fn lock(&self) -> std::sync::MutexGuard<'_, ScopeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of trees currently recorded.
    #[cfg(test)]
    fn running_count(&self) -> usize {
        self.lock().running.len()
    }
}

/// A started child as a [`ProcessScope`] records it: its PID, pinned where
/// the OS allows, so that signalling it never reaches another process that
/// got the PID after the child was reaped.
#[derive(Debug)]
struct Root {
    /// The child's process ID.
    pid: u32,
    /// A pidfd: it keeps naming this very process, even once it is reaped.
    #[cfg(target_os = "linux")]
    pidfd: Option<OwnedFd>,
    /// An open handle: it keeps the PID from being reused.
    #[cfg(windows)]
    handle: Option<OwnedHandle>,
}

impl Root {
    /// Pin a started child (`None` once it was reaped). Pinning itself is
    /// best-effort: without a pin (e.g. a Linux kernel older than 5.3, which
    /// has no pidfd), the root is identified by its PID and parent alone.
    ///
    /// # Arguments
    ///
    /// * `child` - The child, still owned by the caller
    ///
    /// # Returns
    ///
    /// The child's record.
    fn pin(child: &Child) -> Option<Self> {
        let pid = child.id()?;
        Some(Self {
            pid,
            #[cfg(target_os = "linux")]
            pidfd: i32::try_from(pid)
                .ok()
                .and_then(Pid::from_raw)
                .and_then(|pid| pidfd_open(pid, PidfdFlags::empty()).ok()),
            #[cfg(windows)]
            handle: child.raw_handle().and_then(|raw| {
                // SAFETY: `raw_handle` is `Some` only while `child` is not
                // reaped, and `child` owns that handle for the whole borrow:
                // it is valid until it is duplicated here.
                unsafe { BorrowedHandle::borrow_raw(raw) }
                    .try_clone_to_owned()
                    .ok()
            }),
        })
    }

    /// A record without a pin.
    ///
    /// # Arguments
    ///
    /// * `pid` - The process ID
    ///
    /// # Returns
    ///
    /// The record.
    #[cfg(test)]
    fn unpinned(pid: u32) -> Self {
        Self {
            pid,
            #[cfg(target_os = "linux")]
            pidfd: None,
            #[cfg(windows)]
            handle: None,
        }
    }
}

/// How a PID about to be signalled is known to still name the child.
#[derive(Clone, Copy)]
enum Pin<'a> {
    /// The caller holds the child, not reaped: its PID cannot be reused.
    Held,
    /// A scope's record, read when the child may have been reaped already.
    Recorded(&'a Root),
}

/// A [`Command`] for `program`, to be run with [`output`]. It also has
/// `kill_on_drop` set: should the tree kill not run, tokio still kills the
/// direct child when its handle is dropped.
///
/// # Arguments
///
/// * `program` - The program to run (looked up on `PATH` like
///   [`Command::new`])
///
/// # Returns
///
/// The command, ready for arguments and options.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(clippy::disallowed_methods)] // the one sanctioned constructor
    let mut command = Command::new(program);
    command.kill_on_drop(true);
    command
}

/// Run `command` to completion and collect its output: stdout and stderr
/// are captured, and — unlike tokio's [`Command::output`], which lets the
/// child inherit stdin — stdin is closed, as with std's `output`. A program
/// that asks a question (whose prompt nobody would see, its output being
/// captured) reads end-of-file and goes on or fails, instead of waiting
/// forever.
///
/// If the returned future is dropped before the child exits, the child and
/// every process it started are killed (see the module docs). Run inside a
/// [`ProcessScope`], the tree is also recorded there while it runs — so
/// await it in the work's own task: a `tokio::spawn`ed task is outside the
/// scope.
///
/// # Arguments
///
/// * `command` - A command from [`command`]
///
/// # Returns
///
/// The exit status and the captured stdout/stderr.
///
/// # Errors
///
/// When the program cannot be started or its output cannot be read, or
/// ([`ScopeClosed`]) when the surrounding [`ProcessScope`] was closed.
pub(crate) async fn output(command: &mut Command) -> io::Result<Output> {
    let scope = CURRENT_SCOPE.try_with(ProcessScope::clone).ok();
    if scope.as_ref().is_some_and(ProcessScope::is_closed) {
        return Err(scope_closed());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[allow(clippy::disallowed_methods)] // spawned under a `TreeGuard`
    let mut child = command.spawn()?;
    // Declared before the guard, so dropped after it: a cancelled tree is
    // killed while its output pipes are still open. Closed first, they could
    // make members die of SIGPIPE (and free their PIDs) before the kill.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut guard = TreeGuard {
        child: Some(child),
        scoped: None,
    };
    if let Some(scope) = scope
        && let Some(root) = guard.child.as_ref().and_then(Root::pin)
    {
        let Some(key) = scope.register(root) else {
            // Closed between the check and the spawn: the armed guard kills it.
            return Err(scope_closed());
        };
        guard.scoped = Some((scope, key));
    }

    let (status, out, err) = tokio::try_join!(
        guard.wait(),
        read_all(stdout.as_mut()),
        read_all(stderr.as_mut())
    )?;
    guard.disarm();
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// A process was not started because its [`ProcessScope`] was closed: the
/// work's owner has gone away. Permanent — retrying cannot succeed.
#[derive(Debug)]
struct ScopeClosed;

impl std::fmt::Display for ScopeClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not starting a process: the work's owner has gone away")
    }
}

impl std::error::Error for ScopeClosed {}

/// The error for a process refused because its [`ProcessScope`] was closed.
///
/// # Returns
///
/// An [`io::ErrorKind::Other`] error carrying [`ScopeClosed`].
fn scope_closed() -> io::Error {
    io::Error::other(ScopeClosed)
}

/// Read a captured stream to its end (empty when it was not captured).
///
/// # Arguments
///
/// * `stream` - The child's stdout or stderr handle
///
/// # Returns
///
/// Every byte read.
///
/// # Errors
///
/// When reading the pipe fails.
async fn read_all(stream: Option<impl AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(mut stream) = stream {
        stream.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

/// Owns a running child; dropped while armed (the future awaiting the child
/// was dropped), it kills the child's whole process tree.
struct TreeGuard {
    /// The child, until the command completed ([`Self::disarm`]).
    child: Option<Child>,
    /// The scope the tree is recorded in, with its registration key, until
    /// the child is reaped.
    scoped: Option<(ProcessScope, u64)>,
}

impl TreeGuard {
    /// Wait for the child to exit, and reap it.
    ///
    /// A reaped child leaves its scope at once: the call may go on reading
    /// output that a process it left behind holds open, while its PID is
    /// already free for reuse. If waiting fails, the child's state is
    /// unknown (e.g. something else reaped it): it is let go, so no tree
    /// kill or [`ProcessScope::kill_all`] ever targets its PID.
    ///
    /// # Returns
    ///
    /// The child's exit status.
    ///
    /// # Errors
    ///
    /// When waiting fails, or the guard was already disarmed.
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        let Some(child) = self.child.as_mut() else {
            return Err(io::Error::other("the child was already reaped"));
        };
        let waited = child.wait().await;
        if waited.is_err() {
            self.child = None;
        }
        self.leave_scope();
        waited
    }

    /// The command completed: nothing is left to kill.
    fn disarm(&mut self) {
        self.child = None;
        self.leave_scope();
    }

    /// Forget the tree in its scope, if still recorded there.
    fn leave_scope(&mut self) {
        if let Some((scope, key)) = self.scoped.take() {
            scope.unregister(key);
        }
    }
}

impl Drop for TreeGuard {
    /// Still armed: the awaiting future was dropped mid-command, so the
    /// child's tree is killed.
    fn drop(&mut self) {
        // `Child::id` is `None` once reaped: a freed PID is never signalled.
        if let Some(pid) = self.child.as_ref().and_then(Child::id) {
            kill_tree(pid, Pin::Held);
        }
        self.leave_scope();
        // Dropping the child afterwards is harmless: `kill_on_drop` only
        // re-sends a kill to a process that is already gone.
    }
}

/// Most fresh process-table snapshots [`stop_descendants`] takes. Each finds
/// the processes started since the previous one; a tree still growing after
/// that many is killed as far as it was found.
#[cfg(unix)]
const STOP_ROUNDS: usize = 8;

/// Kill process `pid` and all its descendants, best-effort.
///
/// The root is stopped first, but only while it is known to be the child
/// (see [`Pin`]); its descendants are stopped next ([`stop_descendants`]),
/// but only when the process table lists the root as this process's child —
/// otherwise (an unreadable table, or a `/proc` from another PID namespace)
/// the root is killed alone. Everything is then killed with `SIGKILL` (which
/// also ends stopped processes) children before parents: a stopped, living
/// parent does not reap a child that dies meanwhile, so between the stop and
/// the kill no member's PID is freed and reused. (Except under a parent that
/// ignores `SIGCHLD`, whose children the kernel reaps itself; and descendant
/// PIDs come from snapshots, so one that dies and is reaped in the instant
/// before its stop is the inherent limit of signalling by PID — its reuse
/// would take the system's PID range to wrap around meanwhile.)
///
/// # Arguments
///
/// * `pid` - The child's process ID
/// * `pin` - How `pid` is known to still name the child
#[cfg(unix)]
fn kill_tree(pid: u32, pin: Pin<'_>) {
    let table = process_table();
    let listed = table.contains(&(pid, std::process::id()));
    if !signal_root(pid, pin, listed, Signal::STOP) {
        return;
    }
    let members = if listed {
        stop_descendants(pid, table)
    } else {
        Vec::new()
    };
    for member in members.into_iter().rev() {
        send_signal(member, Signal::KILL);
    }
    signal_root(pid, pin, listed, Signal::KILL);
}

/// Send `signal` to a tree's root, if it is still known to be the child.
///
/// # Arguments
///
/// * `pid` - The root's process ID
/// * `pin` - How `pid` is known to still name the child
/// * `listed` - Whether a fresh process table lists `pid` as this process's
///   child
/// * `signal` - The signal to send
///
/// # Returns
///
/// Whether the root was signalled.
#[cfg(unix)]
fn signal_root(pid: u32, pin: Pin<'_>, listed: bool, signal: Signal) -> bool {
    match pin {
        Pin::Held => {
            send_signal(pid, signal);
            true
        }
        Pin::Recorded(root) => signal_recorded(root, listed, signal),
    }
}

/// Send `signal` to a recorded root through its pidfd — which fails once
/// the process is gone, whoever has its PID by then — or, without one, by
/// PID while the table still lists it as this process's child.
///
/// # Arguments
///
/// * `root` - The scope's record
/// * `listed` - Whether a fresh process table lists the root as this
///   process's child
/// * `signal` - The signal to send
///
/// # Returns
///
/// Whether the root was signalled.
#[cfg(target_os = "linux")]
fn signal_recorded(root: &Root, listed: bool, signal: Signal) -> bool {
    match &root.pidfd {
        Some(pidfd) => pidfd_send_signal(pidfd, signal).is_ok(),
        None => signal_if_listed(root.pid, listed, signal),
    }
}

/// Send `signal` to a recorded root by PID, while the table still lists it
/// as this process's child (no pidfds here; PIDs are not reused that fast).
///
/// # Arguments
///
/// * `root` - The scope's record
/// * `listed` - Whether a fresh process table lists the root as this
///   process's child
/// * `signal` - The signal to send
///
/// # Returns
///
/// Whether the root was signalled.
#[cfg(all(unix, not(target_os = "linux")))]
fn signal_recorded(root: &Root, listed: bool, signal: Signal) -> bool {
    signal_if_listed(root.pid, listed, signal)
}

/// Send `signal` to `pid` only if it is `listed` as this process's child.
///
/// # Arguments
///
/// * `pid` - The process ID
/// * `listed` - Whether a fresh process table lists `pid` as this process's
///   child
/// * `signal` - The signal to send
///
/// # Returns
///
/// `listed`: whether it was signalled.
#[cfg(unix)]
fn signal_if_listed(pid: u32, listed: bool, signal: Signal) -> bool {
    if listed {
        send_signal(pid, signal);
    }
    listed
}

/// Stop (`SIGSTOP`) every descendant of the stopped `root`, round by round,
/// until a snapshot taken after all those found were stopped shows no new
/// one: stopped processes cannot start others, so the tree is then complete.
///
/// # Arguments
///
/// * `root` - The tree's root, already stopped
/// * `table` - A snapshot from before the root stopped: it starts the
///   search, but cannot end it
///
/// # Returns
///
/// The descendants' PIDs, each parent before its children.
#[cfg(unix)]
fn stop_descendants(root: u32, mut table: Vec<(u32, u32)>) -> Vec<u32> {
    let mut members: Vec<u32> = Vec::new();
    // Round 0 reads the stale snapshot; rounds 1.. each read a fresh one.
    for round in 0..=STOP_ROUNDS {
        let new: Vec<u32> = descendants(root, &table)
            .into_iter()
            .filter(|pid| !members.contains(pid))
            .collect();
        if new.is_empty() && round > 0 {
            break;
        }
        for &pid in &new {
            send_signal(pid, Signal::STOP);
        }
        members.extend(new);
        if round < STOP_ROUNDS {
            table = process_table();
        }
    }
    members
}

/// Send `signal` to process `pid` with the `kill` system call, best-effort
/// (the process may be gone already). See [`signal_target`] for the PIDs it
/// never signals.
///
/// # Arguments
///
/// * `pid` - The process to signal
/// * `signal` - The signal to send
#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) {
    if let Some(pid) = signal_target(pid) {
        let _ = kill_process(pid, signal);
    }
}

/// `pid` as a single process to signal, or `None` when it must not be: 0
/// (`kill` would take it as this process's whole group), anything beyond
/// `i32` (negative: a group), or this process itself — a stopped apvm (or
/// Node.js host) could never resume.
///
/// # Arguments
///
/// * `pid` - A PID from the process table
///
/// # Returns
///
/// The PID to pass to `kill`, if any.
#[cfg(unix)]
fn signal_target(pid: u32) -> Option<Pid> {
    if pid == std::process::id() {
        return None;
    }
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

/// Kill process `pid` (`taskkill /F`), best-effort — the child only, not
/// its descendants (see the module docs for why not `/T`).
///
/// # Arguments
///
/// * `pid` - The child's process ID
/// * `pin` - How `pid` is known to still name the child: a record without
///   an open handle is skipped, as its PID may have been reused
#[cfg(windows)]
fn kill_tree(pid: u32, pin: Pin<'_>) {
    use std::os::windows::process::CommandExt;

    /// `CREATE_NO_WINDOW`: no console window flashes up in GUI hosts.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    if let Pin::Recorded(Root { handle: None, .. }) = pin {
        return;
    }
    let _ = std::process::Command::new(taskkill_path())
        .args(["/PID", &pid.to_string(), "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `taskkill.exe` in the Windows system directory; by name only when
/// `SystemRoot` is unset. Absolute, like `/bin/ps`: a `taskkill` found
/// first elsewhere (e.g. next to the host executable) could do anything.
///
/// # Returns
///
/// The program to run.
#[cfg(windows)]
fn taskkill_path() -> std::path::PathBuf {
    std::env::var_os("SystemRoot").map_or_else(
        || std::path::PathBuf::from("taskkill"),
        |root| {
            std::path::Path::new(&root)
                .join("System32")
                .join("taskkill.exe")
        },
    )
}

/// Every process as `(pid, parent pid)`, read from `/proc`; empty when it
/// cannot be read (the child is then still killed alone).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_table() -> Vec<(u32, u32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid = entry.file_name().to_str()?.parse().ok()?;
            // Unreadable once the process is gone: skipped.
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            Some((pid, parse_proc_stat(&stat)?.1))
        })
        .collect()
}

/// The state and parent PID in a Linux `/proc/<pid>/stat` line,
/// `pid (comm) state ppid ...`. `comm` may itself hold spaces and
/// parentheses, so the fields are read after its last `)`.
///
/// # Arguments
///
/// * `stat` - The file's content
///
/// # Returns
///
/// `(state, parent PID)`, or `None` when the line is malformed.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_proc_stat(stat: &str) -> Option<(&str, u32)> {
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_whitespace();
    let state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    Some((state, parent))
}

/// Every process as `(pid, parent pid)`, from one `/bin/ps` snapshot; empty
/// when it cannot run (the child is then still killed alone). Run by
/// absolute path: a `ps` earlier on `PATH` could name any process to kill.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn process_table() -> Vec<(u32, u32)> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    parse_process_table(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `ps -o pid= -o ppid=` output into `(pid, parent pid)` pairs,
/// skipping lines that are not exactly two numbers.
///
/// # Arguments
///
/// * `text` - The `ps` output
///
/// # Returns
///
/// The pairs, in input order.
#[cfg(any(all(unix, not(any(target_os = "linux", target_os = "android"))), test))]
fn parse_process_table(text: &str) -> Vec<(u32, u32)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            fields.next().is_none().then_some((pid, ppid))
        })
        .collect()
}

/// Every descendant of `root` in `table` (children, grandchildren, ...).
///
/// # Arguments
///
/// * `root` - The process whose tree to collect (not included)
/// * `table` - `(pid, parent pid)` of every process
///
/// # Returns
///
/// The descendants' PIDs, each once, each parent before its children.
#[cfg(any(unix, test))]
fn descendants(root: u32, table: &[(u32, u32)]) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &(pid, ppid) in table {
            // `pid != root` and the `found` check guard against cycles in a
            // racy snapshot (a reused pid listed under a later process).
            if ppid == parent && pid != root && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found
}

/// Helpers for tests that check child processes stopped (Unix: `ps`).
#[cfg(all(test, unix))]
pub(crate) mod testutil {
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Whether `pid` is still running: listed in `/proc` and neither a
    /// zombie nor dead. A killed child stays a zombie until tokio reaps it in
    /// the background, so "exists" (`kill -0`) would report a killed child
    /// as alive. Needs no `ps`, like the code under test.
    #[cfg(target_os = "linux")]
    pub(crate) fn running(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .as_deref()
            .and_then(super::parse_proc_stat)
            .is_some_and(|(state, _)| state != "Z" && state != "X")
    }

    /// Whether `pid` is still running: listed by `ps` and not a zombie (see
    /// the Linux variant).
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn running(pid: &str) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .is_ok_and(|out| {
                let state = String::from_utf8_lossy(&out.stdout);
                let state = state.trim();
                !state.is_empty() && !state.starts_with('Z')
            })
    }

    /// Read the PID written to `file`, waiting until it is there.
    pub(crate) fn wait_for_pid(file: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(pid) = std::fs::read_to_string(file)
                && !pid.trim().is_empty()
            {
                return pid.trim().to_string();
            }
            assert!(Instant::now() < deadline, "the process never started");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether `pid` stops running within a few seconds.
    pub(crate) fn stops_soon(pid: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A shell script that records its PID in `file`, then becomes (via
    /// `exec`) a 30-second `sleep` — a long-running direct child.
    pub(crate) fn sleeper_script(file: &Path) -> String {
        format!("echo $$ > '{}'; exec sleep 30", file.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_process_table_reads_pid_and_parent_pairs() {
        let text = "    1     0\n  412     1\n bad line\n 413 412\n\n9 8 7\n";
        assert_eq!(parse_process_table(text), [(1, 0), (412, 1), (413, 412)]);
    }

    #[test]
    fn descendants_collects_the_whole_subtree_only() {
        // 10 ─┬─ 11 ── 13 ── 14
        //     └─ 12
        // 20 ── 21          (unrelated)
        let table = [
            (10, 1),
            (11, 10),
            (12, 10),
            (13, 11),
            (14, 13),
            (20, 1),
            (21, 20),
        ];
        let mut found = descendants(10, &table);
        found.sort_unstable();
        assert_eq!(found, [11, 12, 13, 14]);
        assert!(descendants(14, &table).is_empty());
        assert!(descendants(99, &table).is_empty());
    }

    #[test]
    fn descendants_terminates_on_a_cyclic_snapshot() {
        // A racy snapshot could list a reused pid under its own descendant.
        let table = [(2, 1), (3, 2), (1, 3)];
        let mut found = descendants(1, &table);
        found.sort_unstable();
        assert_eq!(found, [2, 3]);
    }

    #[test]
    fn descendants_lists_parents_before_their_children() {
        // `kill_tree` kills in reverse order: children before parents.
        let table = [(13, 11), (12, 10), (11, 10), (14, 13)];
        let found = descendants(10, &table);
        let position = |pid| found.iter().position(|&p| p == pid).unwrap();
        assert!(position(11) < position(13));
        assert!(position(13) < position(14));
        assert_eq!(found.len(), 4);
    }

    #[test]
    fn parse_proc_stat_reads_the_fields_after_the_last_parenthesis() {
        let plain = "4242 (git) S 4241 4242 100 0 -1 4194560 120 0 0 0";
        assert_eq!(parse_proc_stat(plain), Some(("S", 4241)));
        // A program name may hold spaces and parentheses.
        let tricky = "77 (a) b (c)) Z 1 77 77 0 -1";
        assert_eq!(parse_proc_stat(tricky), Some(("Z", 1)));
        assert_eq!(parse_proc_stat(""), None);
        assert_eq!(parse_proc_stat("12 (sh"), None);
        assert_eq!(parse_proc_stat("12 (sh) S"), None);
        assert_eq!(parse_proc_stat("12 (sh) S x"), None);
    }

    #[test]
    fn registrations_of_a_reused_pid_stay_distinct() {
        // A PID freed by one of the scope's processes may be handed to the
        // next: forgetting one registration must keep the other.
        let scope = ProcessScope::new();
        // Never killed: `kill_all` is not called on this scope.
        let first = scope.register(Root::unpinned(4242)).unwrap();
        let second = scope.register(Root::unpinned(4242)).unwrap();
        assert_ne!(first, second);
        assert_eq!(scope.running_count(), 2);
        scope.unregister(first);
        assert_eq!(scope.running_count(), 1);
        scope.unregister(first);
        assert_eq!(scope.running_count(), 1, "unregistering is idempotent");
        scope.unregister(second);
        assert_eq!(scope.running_count(), 0);
    }

    #[test]
    fn a_closed_scope_records_nothing() {
        let scope = ProcessScope::new();
        scope.kill_all(); // empty: signals nothing
        assert_eq!(scope.register(Root::unpinned(4242)), None);
        assert_eq!(scope.running_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn signal_target_never_names_a_group_or_this_process() {
        assert_eq!(signal_target(0), None, "0 is this process's group");
        assert_eq!(signal_target(u32::MAX), None, "negative: a group");
        assert_eq!(signal_target(std::process::id()), None);
        assert_eq!(
            signal_target(4242).map(Pid::as_raw_nonzero),
            std::num::NonZeroI32::new(4242)
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::super::testutil::{running, sleeper_script, stops_soon, wait_for_pid};
        use super::super::*;

        #[tokio::test]
        async fn dropping_the_future_kills_the_child() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let mut child = command("sh");
            child.args(["-c", &sleeper_script(&pid_file)]);

            let pending = tokio::spawn(async move { output(&mut child).await });
            let pid = tokio::task::spawn_blocking(move || wait_for_pid(&pid_file))
                .await
                .unwrap();
            assert!(running(&pid), "precondition: the child is running");

            pending.abort();
            let _ = pending.await;

            assert!(
                stops_soon(&pid),
                "the child {pid} outlived its dropped future"
            );
        }

        #[tokio::test]
        async fn dropping_the_future_kills_what_the_child_started() {
            // git's transport helpers and a shell step's tools are
            // grandchildren; a silent one (here `sleep`) never notices its
            // output pipe closing, so only a tree kill stops it.
            let dir = tempfile::tempdir().unwrap();
            let child_file = dir.path().join("child");
            let grandchild_file = dir.path().join("grandchild");
            let script = format!(
                "sleep 30 & echo $! > '{}'; echo $$ > '{}'; wait",
                grandchild_file.display(),
                child_file.display()
            );
            let mut child = command("sh");
            child.args(["-c", &script]);

            let pending = tokio::spawn(async move { output(&mut child).await });
            let (child_pid, grandchild_pid) = tokio::task::spawn_blocking(move || {
                (wait_for_pid(&child_file), wait_for_pid(&grandchild_file))
            })
            .await
            .unwrap();
            assert!(running(&grandchild_pid), "precondition: grandchild runs");

            pending.abort();
            let _ = pending.await;

            assert!(stops_soon(&child_pid), "the child {child_pid} survived");
            assert!(
                stops_soon(&grandchild_pid),
                "the grandchild {grandchild_pid} survived"
            );
        }

        #[tokio::test]
        async fn dropping_the_future_kills_a_tree_that_keeps_growing() {
            // A descendant (not the child itself) keeps starting processes,
            // as a build tool may: the tree must be frozen whole before it is
            // killed, or members started meanwhile survive. Bounded, so a
            // failure cannot leave an endless spawner behind.
            let dir = tempfile::tempdir().unwrap();
            let spawner_file = dir.path().join("spawner");
            let members_file = dir.path().join("members");
            let spawner = dir.path().join("spawn.sh");
            std::fs::write(
                &spawner,
                "i=0\n\
                 while [ $i -lt 200 ]; do\n\
                 \tsh -c 'echo $$ >> \"$1\"; exec sleep 30' sh \"$1\" &\n\
                 \tsleep 0.02; i=$((i + 1))\n\
                 done\n",
            )
            .unwrap();
            let mut child = command("sh");
            child.args([
                "-c",
                "sh \"$0\" \"$2\" & echo $! > \"$1\"; wait",
                &spawner.display().to_string(),
                &spawner_file.display().to_string(),
                &members_file.display().to_string(),
            ]);

            let pending = tokio::spawn(async move { output(&mut child).await });
            let file = members_file.clone();
            let spawner_pid = tokio::task::spawn_blocking(move || {
                // Wait until the tree is visibly growing.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while read_pids(&file).len() < 3 {
                    assert!(std::time::Instant::now() < deadline, "the tree never grew");
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                wait_for_pid(&spawner_file)
            })
            .await
            .unwrap();

            pending.abort();
            let _ = pending.await;

            assert!(stops_soon(&spawner_pid), "the spawner survived");
            for member in read_pids(&members_file) {
                assert!(stops_soon(&member), "member {member} survived");
            }
        }

        /// The PIDs listed in `file`, one per complete line (none if absent).
        fn read_pids(file: &std::path::Path) -> Vec<String> {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            let complete = text.rsplit_once('\n').map_or("", |(lines, _)| lines);
            complete.lines().map(str::to_string).collect()
        }

        /// Start `script` (sh) inside `scope` on a new task, returning the
        /// task and the PID it records in `pid_file`.
        async fn start_in_scope(
            scope: &ProcessScope,
            pid_file: &std::path::Path,
        ) -> (tokio::task::JoinHandle<io::Result<Output>>, String) {
            let scope = scope.clone();
            let script = sleeper_script(pid_file);
            let task = tokio::spawn(async move {
                scope
                    .run(async { output(command("sh").args(["-c", &script])).await })
                    .await
            });
            let file = pid_file.to_path_buf();
            let pid = tokio::task::spawn_blocking(move || wait_for_pid(&file))
                .await
                .unwrap();
            (task, pid)
        }

        #[tokio::test]
        async fn kill_all_stops_the_scopes_processes_without_dropping_futures() {
            // Node.js `process.exit()` drops nothing: the scope must kill on
            // its own, while the awaiting future is still alive.
            let dir = tempfile::tempdir().unwrap();
            let scope = ProcessScope::new();
            let (task, pid) = start_in_scope(&scope, &dir.path().join("pid")).await;
            assert_eq!(scope.running_count(), 1);

            let killer = scope.clone();
            tokio::task::spawn_blocking(move || killer.kill_all())
                .await
                .unwrap();

            assert!(stops_soon(&pid), "the scoped process {pid} survived");
            let finished = task.await.unwrap().unwrap();
            assert!(!finished.status.success(), "it ended by the kill");
            assert_eq!(scope.running_count(), 0);
        }

        #[tokio::test]
        async fn kill_all_leaves_other_scopes_alone() {
            // A worker exiting must not touch the main thread's builds.
            let dir = tempfile::tempdir().unwrap();
            let (doomed, kept) = (ProcessScope::new(), ProcessScope::new());
            let (_t1, doomed_pid) = start_in_scope(&doomed, &dir.path().join("a")).await;
            let (t2, kept_pid) = start_in_scope(&kept, &dir.path().join("b")).await;

            tokio::task::spawn_blocking(move || doomed.kill_all())
                .await
                .unwrap();

            assert!(stops_soon(&doomed_pid));
            assert!(running(&kept_pid), "another scope's process was killed");
            t2.abort();
            let _ = t2.await;
            assert!(stops_soon(&kept_pid));
        }

        #[tokio::test]
        async fn a_reaped_child_leaves_the_scope_while_its_output_is_read() {
            // The child exits at once, but leaves a process holding its
            // stdout open, so the call keeps reading. The child's PID is
            // free for reuse from its reaping on: `kill_all` must no longer
            // be able to signal it.
            let dir = tempfile::tempdir().unwrap();
            let left_file = dir.path().join("left");
            let script = format!("sleep 30 & echo $! > '{}'", left_file.display());
            let scope = ProcessScope::new();
            let task = {
                let scope = scope.clone();
                tokio::spawn(async move {
                    scope
                        .run(async { output(command("sh").args(["-c", &script])).await })
                        .await
                })
            };
            let left = tokio::task::spawn_blocking(move || wait_for_pid(&left_file))
                .await
                .unwrap();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while scope.running_count() > 0 && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let reaped_left_scope = scope.running_count() == 0;
            let still_reading = !task.is_finished();
            // End the call: the left-behind process is this test's own.
            send_signal(left.parse().unwrap(), Signal::KILL);
            let finished = task.await.unwrap().unwrap();

            assert!(still_reading, "precondition: the call was still reading");
            assert!(reaped_left_scope, "the reaped child stayed in the scope");
            assert!(finished.status.success());
        }

        #[tokio::test]
        async fn kill_all_spares_a_recorded_pid_that_is_not_its_child() {
            // A recorded root that ended may have its PID reused by an
            // unrelated process; without a pin, `kill_all` signals only a PID
            // still listed as this process's child. The stand-in for the
            // unrelated process is a grandchild — never a direct child.
            let dir = tempfile::tempdir().unwrap();
            let grandchild_file = dir.path().join("grandchild");
            let script = format!("sleep 30 & echo $! > '{}'; wait", grandchild_file.display());
            let parent =
                tokio::spawn(async move { output(command("sh").args(["-c", &script])).await });
            let grandchild = tokio::task::spawn_blocking(move || wait_for_pid(&grandchild_file))
                .await
                .unwrap();
            let scope = ProcessScope::new();
            scope.register(Root::unpinned(grandchild.parse().unwrap()));

            let killer = scope.clone();
            tokio::task::spawn_blocking(move || killer.kill_all())
                .await
                .unwrap();
            let spared = running(&grandchild);
            parent.abort(); // kills the real tree, grandchild included
            let _ = parent.await;

            assert!(spared, "a process that is not this one's child was killed");
            assert!(stops_soon(&grandchild));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_recorded_root_is_never_signalled_by_pid_once_reaped() {
            // Its pidfd keeps naming the reaped process, so the signal fails
            // instead of falling back to a PID that may be someone else's.
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            let pidfd = pidfd_open(
                Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
                PidfdFlags::empty(),
            )
            .unwrap();
            child.wait().unwrap();
            let root = Root {
                pid,
                pidfd: Some(pidfd),
            };
            // `listed` is true: only the pidfd can tell the PID is stale.
            assert!(!signal_recorded(&root, true, Signal::KILL));
        }

        #[tokio::test]
        async fn a_cancelled_tree_is_killed_before_its_pipes_close() {
            // A descendant writing to the captured stdout: had the pipes
            // closed first, its next write would fail (EPIPE) and it would
            // record that before being stopped.
            let dir = tempfile::tempdir().unwrap();
            let writer_file = dir.path().join("writer");
            let closed_file = dir.path().join("closed");
            let script = format!(
                "( trap '' PIPE; while :; do echo x || {{ echo closed > '{}'; exit; }}; done ) & \
                 echo $! > '{}'; wait",
                closed_file.display(),
                writer_file.display()
            );
            let mut child = command("sh");
            child.args(["-c", &script]);

            let pending = tokio::spawn(async move { output(&mut child).await });
            let writer = tokio::task::spawn_blocking(move || wait_for_pid(&writer_file))
                .await
                .unwrap();
            pending.abort();
            let _ = pending.await;

            assert!(stops_soon(&writer), "the writer {writer} survived");
            assert!(!closed_file.exists(), "the pipes closed before the kill");
        }

        #[tokio::test]
        async fn a_closed_scope_refuses_new_processes() {
            let scope = ProcessScope::new();
            scope.kill_all();
            assert!(scope.is_closed());
            let err = scope
                .run(async { output(command("sh").args(["-c", "true"])).await })
                .await
                .unwrap_err();
            // Not `Interrupted`, which callers may take as "retry".
            assert_eq!(err.kind(), io::ErrorKind::Other);
            assert!(err.get_ref().is_some_and(|inner| inner.is::<ScopeClosed>()));
        }

        #[tokio::test]
        async fn finished_processes_leave_the_scope() {
            let scope = ProcessScope::new();
            let out = scope
                .run(async { output(command("sh").args(["-c", "echo hi"])).await })
                .await
                .unwrap();
            assert_eq!(out.stdout, b"hi\n");
            assert_eq!(scope.running_count(), 0);
        }

        #[tokio::test]
        async fn a_finished_command_reports_status_and_output() {
            let out = output(command("sh").args(["-c", "echo out; echo err >&2; exit 3"]))
                .await
                .unwrap();
            assert_eq!(out.status.code(), Some(3));
            assert_eq!(out.stdout, b"out\n");
            assert_eq!(out.stderr, b"err\n");
        }

        #[tokio::test]
        async fn large_output_on_both_streams_does_not_deadlock() {
            // Both pipes are drained while waiting; reading one at a time
            // would block once the other's pipe buffer fills.
            let script = "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2";
            let out = output(command("sh").args(["-c", script])).await.unwrap();
            assert!(out.status.success());
            assert_eq!((out.stdout.len(), out.stderr.len()), (1 << 20, 1 << 20));
        }

        #[tokio::test]
        async fn stdin_is_closed() {
            // Like `Command::output`: a child reading stdin sees EOF at once
            // instead of waiting on (or stealing) the caller's terminal.
            let out = output(command("sh").args(["-c", "cat; echo done"]))
                .await
                .unwrap();
            assert_eq!(out.stdout, b"done\n");
        }

        #[tokio::test]
        async fn a_missing_program_is_an_error() {
            let err = output(&mut command("apvm-no-such-program-xyz"))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
        }
    }
}
