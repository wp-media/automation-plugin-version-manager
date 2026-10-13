//! Running a call as a local child process ([`run`]), and the test
//! transport that runs scripts on this machine (`LocalShell`).
//!
//! [`run`] is the pattern every process-based transport follows (the SSH
//! transport runs `ssh … <host> <remote command>` through it):
//!
//! 1. An upload's file is opened first, so a missing file runs nothing.
//! 2. The child starts through [`apvm_process::spawn`] (recorded in the
//!    caller's [`ProcessScope`](apvm_process::ProcessScope), if any, which
//!    the Node bindings kill at exit); stdin is fed, and
//!    stdout and stderr are drained into capped [`Capture`]s, all at once,
//!    so neither side ever blocks on a full pipe.
//! 3. The call ends when the child exits, or when the timeout passes: the
//!    child and everything it started are then killed ([`Exit::TimedOut`]).
//! 4. After that, output and stdin get [`SETTLE`] more to finish: a process
//!    that escaped the tree (a background job of the script) may hold the
//!    pipes open, and must not hold the call.
//!
//! Dropping the call's future kills the child's whole tree (the guard).
//!
//! The child inherits apvm's environment (as `ssh` must: `SSH_AUTH_SOCK`,
//! `HOME`); the SSH transport decides what to remove.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::pin::{Pin, pin};
use std::process::Stdio;
use std::time::{Duration, Instant};

use apvm_process::{Pipes, Running};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

use super::capture::{Capture, STDERR_LIMIT, STDOUT_LIMIT};
use super::invalid;
#[cfg(test)]
use super::transport::{BoxFuture, Transport};
use super::transport::{Delivery, Execution, Exit, Payload, Request};
use crate::error::{Error, Result};
use crate::problem::quoted;

/// How long a call keeps feeding stdin and reading output after the child
/// exited or was killed. Output already written is read at once; only a
/// process outside the tree that holds a pipe open makes it wait this long.
const SETTLE: Duration = Duration::from_secs(2);
/// Bytes of an upload's file read at a time.
const CHUNK: usize = 64 * 1024;

/// Runs scripts on this machine the way sshd runs them on a server: the
/// login shell gets `-c <remote command>` ([`Request::remote_command`]:
/// `sh -s`, or an upload's `sh -c '<script>'`), and stdin carries the
/// script or the upload. For the crate's tests only: apvm runs nothing
/// through a local shell (plan D2; local transports are enhancement E2).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalShell {
    /// The login shell's program.
    login_shell: OsString,
}

#[cfg(test)]
impl LocalShell {
    /// A transport whose login shell is `login_shell` (such as `sh`).
    ///
    /// # Arguments
    ///
    /// * `login_shell` - The program, looked up on `PATH` unless a path
    pub(crate) fn new(login_shell: impl Into<OsString>) -> Self {
        Self {
            login_shell: login_shell.into(),
        }
    }
}

#[cfg(test)]
impl Transport for LocalShell {
    /// Run `request` as `<login shell> -c <remote command>`.
    fn execute<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Execution>> {
        Box::pin(async move {
            let args = [OsString::from("-c"), request.remote_command().into()];
            run(&self.login_shell, &args, request).await
        })
    }
}

/// Run `program` with `args` as one call of `request`: its stdin, capped
/// output, timeout and cancellation (see the module docs).
///
/// # Arguments
///
/// * `program` - The program (`ssh`, or a login shell)
/// * `args` - Its arguments, the remote command included
/// * `request` - What its stdin carries, its nonce and its timeout
///
/// # Returns
///
/// How the call ended, once the program started.
///
/// # Errors
///
/// [`Error::InvalidInput`] when an upload's file cannot be opened or is
/// not a regular file (nothing ran); [`Error::UploadSource`] when reading
/// it fails mid-call; [`Error::Process`] when the program cannot be
/// started, or its output read, or waited for. A started child was killed
/// with its tree.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the SSH transport (P3.3) runs through it")
)]
pub(crate) async fn run(
    program: &OsStr,
    args: &[OsString],
    request: &Request,
) -> Result<Execution> {
    let mut input = Input::open(request).await?;
    // Pipes before the guard: dropped after it (see `apvm_process::spawn`).
    let (mut pipes, mut guard) =
        start(program, args).map_err(|source| failed("cannot start", program, source))?;
    let started = Instant::now();
    let mut kept = Kept::new();
    let (exit, ended) = {
        let io = exchange(&mut pipes, &mut input, &mut kept, program);
        supervise(&mut guard, io, request.timeout(), program).await?
    };
    Ok(kept.into_execution(request, exit, ended.saturating_duration_since(started)))
}

/// Start `program` with `args`, its stdin, stdout and stderr piped and its
/// environment inherited, through apvm's one spawn.
///
/// # Arguments
///
/// * `program` - The program, looked up on `PATH` unless a path
/// * `args` - Its arguments
///
/// # Errors
///
/// When it cannot be started, or the caller's process scope was closed.
fn start(program: &OsStr, args: &[OsString]) -> std::io::Result<(Pipes, Running)> {
    let mut command = apvm_process::command(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apvm_process::spawn(&mut command)
}

/// What a call collects while it runs.
struct Kept {
    /// Its stdout, capped.
    out: Capture,
    /// Its stderr, capped.
    err: Capture,
    /// Whether all of stdin was written and closed.
    written: bool,
}

impl Kept {
    /// Nothing yet.
    fn new() -> Self {
        Self {
            out: Capture::new(STDOUT_LIMIT),
            err: Capture::new(STDERR_LIMIT),
            written: false,
        }
    }

    /// The call's result.
    ///
    /// # Arguments
    ///
    /// * `request` - The call
    /// * `exit` - How it ended
    /// * `elapsed` - How long it ran
    fn into_execution(self, request: &Request, exit: Exit, elapsed: Duration) -> Execution {
        Execution::new(
            request.nonce().clone(),
            exit,
            self.out.finish(),
            self.err.finish(),
            self.written,
            elapsed,
        )
    }
}

/// Feed the child's stdin and drain its stdout and stderr, all at once.
///
/// # Arguments
///
/// * `pipes` - The child's pipes (stdin is taken, to be closed at the end)
/// * `input` - What stdin carries
/// * `kept` - Where output and the stdin outcome go
/// * `program` - The program, for messages
///
/// # Errors
///
/// [`Error::UploadSource`] or [`Error::Process`], from [`feed`] or
/// [`drain`].
async fn exchange(
    pipes: &mut Pipes,
    input: &mut Input<'_>,
    kept: &mut Kept,
    program: &OsStr,
) -> Result<()> {
    let Kept { out, err, written } = kept;
    let feed = feed(pipes.stdin.take(), input, written);
    let drain = drain(
        pipes.stdout.as_mut(),
        pipes.stderr.as_mut(),
        out,
        err,
        program,
    );
    tokio::try_join!(feed, drain).map(|_| ())
}

/// Wait for the child to exit while `io` runs, up to `timeout`; kill its
/// tree if the timeout passes; then give `io` [`SETTLE`] to finish.
///
/// # Arguments
///
/// * `guard` - The child
/// * `io` - [`exchange`]
/// * `timeout` - How long the child may run
/// * `program` - The program, for messages
///
/// # Returns
///
/// How the call ended, and when (the exit, or the kill).
///
/// # Errors
///
/// When `io` fails, or waiting for the child fails. The caller then drops
/// the guard, which kills what still runs.
async fn supervise(
    guard: &mut Running,
    io: impl Future<Output = Result<()>>,
    timeout: Duration,
    program: &OsStr,
) -> Result<(Exit, Instant)> {
    let mut io = pin!(io);
    let mut io_done = false;
    let exit = wait_or_time_out(guard, io.as_mut(), &mut io_done, timeout, program).await?;
    let ended = Instant::now();
    if exit == Exit::TimedOut {
        stop(guard).await;
    }
    if !io_done {
        settle(io).await?;
    }
    Ok((exit, ended))
}

/// Run `io` until the child exits or `timeout` passes, whichever first.
///
/// # Arguments
///
/// * `guard` - The child
/// * `io` - [`exchange`]
/// * `io_done` - Set when `io` finished
/// * `timeout` - How long the child may run
/// * `program` - The program, for messages
///
/// # Returns
///
/// The child's exit, or [`Exit::TimedOut`].
///
/// # Errors
///
/// When `io` fails, or waiting for the child fails.
async fn wait_or_time_out<F: Future<Output = Result<()>>>(
    guard: &mut Running,
    mut io: Pin<&mut F>,
    io_done: &mut bool,
    timeout: Duration,
    program: &OsStr,
) -> Result<Exit> {
    let mut deadline = pin!(tokio::time::sleep(timeout));
    loop {
        tokio::select! {
            // An exit that is ready wins over a deadline passing with it.
            biased;
            status = guard.wait() => {
                let status = status.map_err(|source| failed("cannot wait for", program, source))?;
                return Ok(Exit::from_status(status));
            }
            done = io.as_mut(), if !*io_done => {
                done?;
                *io_done = true;
            }
            () = &mut deadline => return Ok(Exit::TimedOut),
        }
    }
}

/// Kill a timed-out child's tree, and reap the child.
///
/// # Arguments
///
/// * `guard` - The child
async fn stop(guard: &mut Running) {
    guard.kill();
    // Reaped at once after SIGKILL; if not, the guard's drop retries.
    let _ = tokio::time::timeout(SETTLE, guard.wait()).await;
}

/// Give `io` [`SETTLE`] to finish after the child ended: output already
/// written is read; a pipe that a process outside the tree holds open is
/// abandoned, with what was read so far kept.
///
/// # Arguments
///
/// * `io` - [`exchange`], not finished yet
///
/// # Errors
///
/// When `io` fails meanwhile.
async fn settle(io: Pin<&mut impl Future<Output = Result<()>>>) -> Result<()> {
    match tokio::time::timeout(SETTLE, io).await {
        Ok(done) => done,
        Err(_) => Ok(()),
    }
}

/// What a call writes to the child's stdin.
enum Input<'a> {
    /// Bytes in memory: a script, or an upload's bytes.
    Bytes(&'a [u8]),
    /// An upload's file, open for reading.
    File {
        /// The open file.
        file: tokio::fs::File,
        /// Its path, for messages.
        path: &'a Path,
    },
}

impl<'a> Input<'a> {
    /// The stdin of `request`, its file opened.
    ///
    /// # Arguments
    ///
    /// * `request` - The call
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when an upload's file cannot be opened or
    /// is not a regular file.
    async fn open(request: &'a Request) -> Result<Self> {
        match request.delivery() {
            Delivery::Stdin(script) => Ok(Self::Bytes(script.text().as_bytes())),
            Delivery::Upload { payload, .. } => match payload {
                Payload::Bytes(bytes) => Ok(Self::Bytes(bytes)),
                Payload::File(path) => Ok(Self::File {
                    file: open_regular(path).await?,
                    path,
                }),
            },
        }
    }
}

/// Open `path` for reading, refusing anything but a regular file: a
/// directory would fail only mid-upload, and a FIFO would block.
///
/// The type is checked before opening, so a directory gets the same message
/// on every platform (Windows cannot even open one) and a FIFO is not
/// opened, and again on the opened handle, since the path may have changed
/// in between. On Unix the open itself uses `O_NONBLOCK`, so a FIFO swapped
/// in meanwhile cannot block it; the flag is cleared once the handle is
/// known to be a regular file, so network and FUSE file systems read it as
/// usual.
///
/// # Arguments
///
/// * `path` - The upload's file
///
/// # Errors
///
/// [`Error::InvalidInput`] when the caller's path is unusable: missing,
/// not permitted, or not a regular file. [`Error::UploadSource`] for any
/// other failure (too many open files, an I/O error).
async fn open_regular(path: &Path) -> Result<tokio::fs::File> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| open_failed(path, error))?;
    if !metadata.is_file() {
        return Err(not_a_regular_file(path));
    }
    let file = open_nonblocking(path)
        .await
        .map_err(|error| open_failed(path, error))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| open_failed(path, error))?;
    if !metadata.is_file() {
        return Err(not_a_regular_file(path));
    }
    restore_blocking(&file).map_err(|source| Error::UploadSource {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(file)
}

/// Open `path` for reading, without blocking on a FIFO (Unix:
/// `O_NONBLOCK`).
///
/// # Arguments
///
/// * `path` - The file
///
/// # Errors
///
/// When it cannot be opened.
async fn open_nonblocking(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nonblocking());
    options.open(path).await
}

/// The `O_NONBLOCK` open flag.
#[cfg(unix)]
fn nonblocking() -> i32 {
    // A small positive constant on every Unix: the cast keeps its bits.
    rustix::fs::OFlags::NONBLOCK.bits() as i32
}

/// Clear `O_NONBLOCK` on an opened regular file.
///
/// # Arguments
///
/// * `file` - The file
///
/// # Errors
///
/// When its flags cannot be read or set.
#[cfg(unix)]
fn restore_blocking(file: &tokio::fs::File) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(file)?;
    rustix::fs::fcntl_setfl(file, flags - rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

/// Nothing to clear outside Unix.
///
/// # Arguments
///
/// * `_file` - The file
#[cfg(not(unix))]
fn restore_blocking(_file: &tokio::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// The error for an upload file that could not be opened or inspected.
///
/// # Arguments
///
/// * `path` - The file
/// * `error` - Why
fn open_failed(path: &Path, error: std::io::Error) -> Error {
    if !is_the_callers(&error) {
        return Error::UploadSource {
            path: path.to_path_buf(),
            source: error,
        };
    }
    refused(path, format!("cannot be opened: {error}"))
}

/// Whether `error` says the caller's path is unusable (rather than the
/// system failing).
///
/// # Arguments
///
/// * `error` - An open or inspection error
fn is_the_callers(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    matches!(
        error.kind(),
        ErrorKind::NotFound
            | ErrorKind::PermissionDenied
            | ErrorKind::IsADirectory
            | ErrorKind::NotADirectory
    )
}

/// The error for an upload path that is not a regular file.
///
/// # Arguments
///
/// * `path` - The path
fn not_a_regular_file(path: &Path) -> Error {
    refused(path, "is not a regular file".to_string())
}

/// An [`Error::InvalidInput`] about the upload's `path`.
///
/// # Arguments
///
/// * `path` - The path
/// * `reason` - What is wrong with it
fn refused(path: &Path, reason: String) -> Error {
    let path = quoted(&path.display().to_string());
    invalid("upload payload", "path", format!("{path} {reason}"))
}

/// Write `input` to the child's stdin, then close it. A write that fails
/// means the child stopped reading (it exited, or closed stdin): that
/// ends the feed, leaving `written` false, and is no error.
///
/// # Arguments
///
/// * `stdin` - The child's stdin
/// * `input` - What to write
/// * `written` - Set once everything was written and stdin closed
///
/// # Errors
///
/// [`Error::UploadSource`] when an upload's file cannot be read.
async fn feed(stdin: Option<ChildStdin>, input: &mut Input<'_>, written: &mut bool) -> Result<()> {
    let Some(mut stdin) = stdin else {
        return Ok(());
    };
    let complete = match input {
        Input::Bytes(bytes) => stdin.write_all(bytes).await.is_ok(),
        Input::File { file, path } => stream(file, path, &mut stdin).await?,
    };
    // Closing stdin is the end of the script, or of the upload.
    drop(stdin);
    *written = complete;
    Ok(())
}

/// Copy `file` into `stdin`.
///
/// # Arguments
///
/// * `file` - The upload's open file
/// * `path` - Its path, for messages
/// * `stdin` - The child's stdin
///
/// # Returns
///
/// Whether everything was written (`false`: the child stopped reading).
///
/// # Errors
///
/// [`Error::UploadSource`] when reading the file fails.
async fn stream(file: &mut tokio::fs::File, path: &Path, stdin: &mut ChildStdin) -> Result<bool> {
    let mut chunk = vec![0; CHUNK];
    loop {
        let read = file
            .read(&mut chunk)
            .await
            .map_err(|source| Error::UploadSource {
                path: path.to_path_buf(),
                source,
            })?;
        let Some(bytes) = chunk.get(..read).filter(|bytes| !bytes.is_empty()) else {
            return Ok(true);
        };
        if stdin.write_all(bytes).await.is_err() {
            return Ok(false);
        }
    }
}

/// Read stdout and stderr to their ends at once, keeping what fits.
///
/// # Arguments
///
/// * `stdout` - The child's stdout
/// * `stderr` - The child's stderr
/// * `out` - Where stdout goes
/// * `err` - Where stderr goes
/// * `program` - The program, for messages
///
/// # Errors
///
/// [`Error::Process`] when reading a pipe fails.
async fn drain(
    stdout: Option<&mut ChildStdout>,
    stderr: Option<&mut ChildStderr>,
    out: &mut Capture,
    err: &mut Capture,
    program: &OsStr,
) -> Result<()> {
    tokio::try_join!(out.drain(stdout), err.drain(stderr))
        .map(|_| ())
        .map_err(|source| failed("cannot read the output of", program, source))
}

/// An [`Error::Process`] about `program`.
///
/// # Arguments
///
/// * `context` - What failed
/// * `program` - The program
/// * `source` - Why
fn failed(context: &'static str, program: &OsStr, source: std::io::Error) -> Error {
    Error::Process {
        context,
        program: program.to_string_lossy().into_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;
    use crate::remote::{Identity, UploadScript};

    /// A request no transport may start: its upload file is missing.
    ///
    /// # Arguments
    ///
    /// * `path` - The upload's file
    fn upload_from(path: &Path) -> Request {
        let script = UploadScript::tee(
            crate::remote::Nonce::random(),
            &Identity::Login,
            "/tmp/apvm.x/package-1.zip",
        )
        .unwrap();
        Request::upload(script, Payload::File(path.into()), Duration::from_secs(5))
    }

    #[tokio::test]
    async fn a_missing_login_shell_fails_to_start() {
        let shell = LocalShell::new("apvm-no-such-shell-xyz");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package.zip");
        std::fs::write(&file, b"zip").unwrap();
        let error = shell.execute(&upload_from(&file)).await.unwrap_err();
        assert_eq!(error.code(), Code::ProcessFailed);
        let message = error.to_string();
        assert!(
            message.starts_with("cannot start 'apvm-no-such-shell-xyz': "),
            "{message}"
        );
        let Error::Process { source, .. } = error else {
            panic!("a process error");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn an_upload_whose_file_is_missing_or_not_a_file_runs_nothing() {
        // The login shell does not exist either: a spawn would fail
        // differently, so the file is checked first.
        let shell = LocalShell::new("apvm-no-such-shell-xyz");
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.zip");
        for (path, reason) in [
            (missing, "cannot be opened: "),
            (dir.path().to_path_buf(), "is not a regular file"),
        ] {
            let error = shell.execute(&upload_from(&path)).await.unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput, "{error}");
            let message = error.to_string();
            assert!(
                message.starts_with("invalid upload payload: path: '"),
                "{message}"
            );
            assert!(message.contains(reason), "{message}");
        }
    }

    #[test]
    fn only_a_missing_forbidden_or_wrong_kind_of_path_is_the_callers_error() {
        use std::io::{Error as IoError, ErrorKind};

        for kind in [
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::IsADirectory,
            ErrorKind::NotADirectory,
        ] {
            let error = open_failed(Path::new("/x.zip"), IoError::from(kind));
            assert_eq!(error.code(), Code::InvalidInput, "{kind:?}");
        }
        for kind in [
            ErrorKind::Other,
            ErrorKind::Interrupted,
            ErrorKind::OutOfMemory,
        ] {
            let error = open_failed(Path::new("/x.zip"), IoError::from(kind));
            assert_eq!(error.code(), Code::ProcessFailed, "{kind:?}");
            assert!(matches!(error, Error::UploadSource { .. }), "{kind:?}");
        }
    }

    /// Make a FIFO at `path` (`mkfifo`).
    ///
    /// # Arguments
    ///
    /// * `path` - Where
    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_a_fifo_never_blocks() {
        // The path may become a FIFO after the type check: the open itself
        // must not wait for a writer.
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("package.zip");
        mkfifo(&fifo);
        let opened = tokio::time::timeout(Duration::from_secs(5), open_nonblocking(&fifo)).await;
        let Ok(opened) = opened else {
            // Blocked in the blocking pool: a writer releases it, so the
            // runtime can stop and the test fails instead of hanging.
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nonblocking())
                .open(&fifo);
            panic!("opening a FIFO blocked");
        };
        let metadata = opened.unwrap().metadata().await.unwrap();
        assert!(!metadata.is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_opened_upload_reads_without_o_nonblock() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package.zip");
        std::fs::write(&file, b"zip").unwrap();
        let opened = open_regular(&file).await.unwrap();
        let flags = rustix::fs::fcntl_getfl(&opened).unwrap();
        assert!(!flags.contains(rustix::fs::OFlags::NONBLOCK), "{flags:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_upload_from_a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("package.zip");
        mkfifo(&fifo);
        // Nobody ever writes to it: a blocking open would hang here.
        let (shell, request) = (LocalShell::new("sh"), upload_from(&fifo));
        let call = shell.execute(&request);
        let refused = tokio::time::timeout(Duration::from_secs(10), call)
            .await
            .expect("the open blocked")
            .unwrap_err();
        assert_eq!(refused.code(), Code::InvalidInput);
        assert!(
            refused.to_string().contains("is not a regular file"),
            "{refused}"
        );
    }

    #[cfg(unix)]
    mod unix {
        use std::sync::Arc;

        use sha2::{Digest, Sha256};

        use super::super::super::quote::Word;
        use super::super::super::statement::{Body, Statement};
        use super::super::super::tests::{safe_tempdir, text};
        use super::super::*;
        use crate::remote::capture::PayloadError;
        use crate::remote::{Identity, Nonce, Script, UploadScript};
        use apvm_process::test_support::{
            assert_all_gone, heartbeat, lines, recorded_pids, tree_visible, wait_for_lines,
        };

        /// A missing nonce line in untruncated stdout.
        const MISSING: PayloadError = PayloadError::NonceMissing {
            stdout_truncated: false,
        };

        /// A timeout no test reaches unless something hangs.
        const LONG: Duration = Duration::from_secs(60);

        /// A framed stdin script whose one statement is `sh -c <code>`.
        ///
        /// # Arguments
        ///
        /// * `code` - Shell code for the statement
        fn framed(code: &str) -> Script {
            let words = vec![Word::lit("-c"), Word::checked(code)];
            let statement = Statement::as_identity(&Identity::Login, "sh", words);
            Script::framed(Nonce::random(), &Body::new(statement, []))
        }

        /// Run `code` framed, through `sh` as the login shell.
        ///
        /// # Arguments
        ///
        /// * `code` - Shell code for the script's statement
        /// * `timeout` - The call's timeout
        async fn sh(code: &str, timeout: Duration) -> Execution {
            let request = Request::script(framed(code), timeout);
            LocalShell::new("sh").execute(&request).await.unwrap()
        }

        /// Run `request` through a login "shell" that runs `login` first,
        /// then the remote command (`$1`): a stand-in for startup files.
        ///
        /// # Arguments
        ///
        /// * `login` - Shell code run before the remote command
        /// * `request` - The call
        async fn behind(login: &str, request: &Request) -> Execution {
            let code = format!("{login}\nexec sh -c \"$1\"");
            let args = [
                "-c".into(),
                code.into(),
                "login".into(),
                request.remote_command().into(),
            ];
            run(OsStr::new("sh"), &args, request).await.unwrap()
        }

        /// `len` pseudo-random bytes (a 64-bit LCG), the same every run.
        ///
        /// # Arguments
        ///
        /// * `len` - How many
        fn noise_bytes(len: usize) -> Vec<u8> {
            let mut state: u64 = 0x005E_ED0F_A975;
            (0..len)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    (state >> 56) as u8
                })
                .collect()
        }

        /// The SHA-256 of `bytes`, in hexadecimal.
        ///
        /// # Arguments
        ///
        /// * `bytes` - The data
        fn sha256(bytes: &[u8]) -> String {
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }

        #[tokio::test]
        async fn a_script_reports_its_payload_status_stderr_and_timing() {
            let done = sh("printf 'out\\n'; printf 'err\\n' >&2; exit 3", LONG).await;
            assert_eq!(done.exit(), Exit::Status(3));
            assert_eq!(done.payload(), Ok(&b"out\n"[..]));
            assert_eq!(done.stderr().bytes(), b"err\n");
            assert!(!done.stdout().is_truncated() && !done.stderr().is_truncated());
            assert!(done.stdin_written());
            assert!(done.elapsed() < LONG);
        }

        #[tokio::test]
        async fn empty_unterminated_and_crlf_payloads_arrive_byte_exact() {
            let empty = sh("true", LONG).await;
            assert_eq!(
                (empty.exit(), empty.payload()),
                (Exit::Status(0), Ok(&b""[..]))
            );
            let unterminated = sh("printf abc", LONG).await;
            assert_eq!(unterminated.payload(), Ok(&b"abc"[..]));
            let crlf = sh("printf 'a\\r\\nb\\r\\n'", LONG).await;
            assert_eq!(crlf.payload(), Ok(&b"a\r\nb\r\n"[..]));
        }

        #[tokio::test]
        async fn a_command_exiting_255_reports_254_and_a_255_is_the_transport() {
            let fatal = sh("exit 255", LONG).await;
            assert_eq!(fatal.exit(), Exit::Status(Exit::REMAPPED_255));
            assert_eq!(fatal.payload(), Ok(&b""[..]), "the script ran");

            // A login shell (ssh) failing with 255 before the script ran.
            let request = Request::script(framed("true"), LONG);
            let failed = behind("exit 255", &request).await;
            assert_eq!(failed.exit(), Exit::TransportFailed);
            assert_eq!(
                failed.payload(),
                Err(PayloadError::Interrupted {
                    exit: Exit::TransportFailed
                })
            );
        }

        #[tokio::test]
        async fn a_killed_login_shell_reports_its_signal() {
            let request = Request::script(framed("true"), LONG);
            let killed = behind("kill -KILL $$", &request).await;
            assert_eq!(killed.exit(), Exit::Killed { signal: Some(9) });
        }

        #[tokio::test]
        async fn login_noise_without_a_final_newline_does_not_hide_the_nonce() {
            let request = Request::script(framed("printf 'payload\\n'"), LONG);
            let done = behind("printf 'Welcome'", &request).await;
            assert_eq!(done.payload(), Ok(&b"payload\n"[..]));
        }

        #[tokio::test]
        async fn login_noise_past_the_limit_is_a_flagged_missing_nonce() {
            let request = Request::script(framed("printf 'payload\\n'"), LONG);
            let noise = format!("head -c {} /dev/zero", STDOUT_LIMIT + 1);
            let done = behind(&noise, &request).await;
            assert_eq!(
                done.payload(),
                Err(PayloadError::NonceMissing {
                    stdout_truncated: true
                })
            );
        }

        #[tokio::test]
        async fn login_noise_before_the_nonce_is_skipped() {
            let request = Request::script(framed("printf 'payload\\n'"), LONG);
            let noise = "printf 'Welcome to Debian\\n\\napvm-0123456789abcdef0123456789abcdef\\n'\n\
                         printf 'apvm-\\n%s \\n' \"$0\"\n\
                         echo 'bashrc warning' >&2";
            let done = behind(noise, &request).await;
            assert_eq!(done.exit(), Exit::Status(0));
            assert_eq!(done.payload(), Ok(&b"payload\n"[..]));
            assert!(done.stdout().bytes().starts_with(b"Welcome to Debian\n"));
            assert_eq!(done.stderr().bytes(), b"bashrc warning\n");
        }

        #[tokio::test]
        async fn startup_files_that_eat_the_start_of_the_script_fail_the_framing() {
            let request = Request::script(framed("echo ran"), LONG);
            // Reads the script's first 40 bytes, one at a time: the brace
            // and the nonce line's start. What is left still parses, and
            // its command may run (plan §6.1 item 2's known limit), but
            // the nonce line never appears: the call fails, so a caller
            // re-reads after a mutation instead of trusting the output.
            let done = behind("dd bs=1 count=40 of=/dev/null 2>/dev/null", &request).await;
            assert_eq!(done.payload(), Err(MISSING), "never \"no output\"");
        }

        #[tokio::test]
        async fn stdout_keeps_8_mib_exactly_and_flags_more() {
            for (extra, truncated) in [(0, false), (1, true), (12 << 20, true)] {
                // The nonce line (`\n<nonce>\n`) takes its share of the limit.
                let payload = STDOUT_LIMIT + extra - (Nonce::random().as_str().len() + 2);
                let code = format!("head -c {payload} /dev/zero");
                let done = sh(&code, LONG).await;
                assert_eq!(done.exit(), Exit::Status(0), "drained, never blocked");
                assert_eq!(done.stdout().bytes().len(), STDOUT_LIMIT, "+{extra}");
                assert_eq!(done.stdout().is_truncated(), truncated, "+{extra}");
                let expected = if truncated {
                    Err(PayloadError::Truncated)
                } else {
                    Ok(STDOUT_LIMIT - (Nonce::random().as_str().len() + 2))
                };
                assert_eq!(done.payload().map(<[u8]>::len), expected, "+{extra}");
            }
        }

        #[tokio::test]
        async fn stderr_keeps_1_mib_exactly_and_flags_more() {
            for (len, truncated) in [
                (STDERR_LIMIT, false),
                (STDERR_LIMIT + 1, true),
                (5 * STDERR_LIMIT, true),
            ] {
                let done = sh(&format!("head -c {len} /dev/zero >&2; echo ok"), LONG).await;
                assert_eq!(done.exit(), Exit::Status(0));
                assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
                assert_eq!(done.stderr().bytes().len(), STDERR_LIMIT, "{len}");
                assert_eq!(done.stderr().is_truncated(), truncated, "{len}");
            }
        }

        #[tokio::test]
        async fn a_20_mb_upload_arrives_byte_exact_from_memory_and_from_a_file() {
            let dir = safe_tempdir();
            let bytes = noise_bytes(20_000_000);
            let source = dir.path().join("source.zip");
            std::fs::write(&source, &bytes).unwrap();
            let payloads = [
                Payload::Bytes(Arc::from(bytes.as_slice())),
                Payload::File(source),
            ];
            for (index, payload) in payloads.into_iter().enumerate() {
                let file = dir.path().join(format!("package-{index}.zip"));
                let upload =
                    UploadScript::tee(Nonce::random(), &Identity::Login, text(&file)).unwrap();
                let request = Request::upload(upload, payload, LONG);
                let done = LocalShell::new("sh").execute(&request).await.unwrap();
                assert_eq!(done.exit(), Exit::Status(0), "{:?}", done.stderr());
                assert_eq!(done.payload(), Ok(&b""[..]));
                assert!(done.stdin_written());
                let received = std::fs::read(&file).unwrap();
                assert_eq!(sha256(&received), sha256(&bytes), "payload {index}");
            }
        }

        #[tokio::test]
        async fn a_child_that_stops_reading_leaves_stdin_unwritten() {
            // Far more than a pipe holds: the write fails (EPIPE) once the
            // child is gone, and that is no error.
            let upload = UploadScript::tee(
                Nonce::random(),
                &Identity::Login,
                "/tmp/apvm.x/package-1.zip",
            )
            .unwrap();
            let request = Request::upload(upload, Payload::from(vec![0; 4 << 20]), LONG);
            let done = behind("exit 7", &request).await;
            assert_eq!(done.exit(), Exit::Status(7));
            assert!(!done.stdin_written());
            assert_eq!(done.payload(), Err(MISSING));
        }

        #[tokio::test]
        async fn a_child_closing_its_output_early_still_reports_its_exit() {
            let request = Request::script(framed("true"), LONG);
            let done = behind("echo early; exec >&- 2>&-; sleep 0.3; exit 4", &request).await;
            assert_eq!(done.exit(), Exit::Status(4));
            assert_eq!(done.stdout().bytes(), b"early\n");
        }

        #[tokio::test]
        async fn a_process_left_holding_the_output_does_not_hold_the_call() {
            // The script ends; a background job it left keeps stdout open.
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let code = format!("sleep 30 & echo $! > '{}'; echo done", pid_file.display());
            let call = sh(&code, LONG);
            let done = tokio::time::timeout(Duration::from_secs(20), call).await;
            // Clean up first: the job is this test's own.
            let pid = std::fs::read_to_string(&pid_file).unwrap();
            let pid = rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            let done = done.expect("the call waited for the job");
            assert_eq!(done.exit(), Exit::Status(0));
            assert_eq!(done.payload(), Ok(&b"done\n"[..]));
        }

        #[tokio::test]
        async fn output_past_the_limit_then_a_hang_times_out_cut() {
            // 9 MiB, then silence: the limit and the timeout both apply,
            // without a CPU-bound writer. Under load the child may not get
            // far before the timeout: only what the marker proves is
            // checked.
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("past-the-limit");
            let code = format!(
                "head -c {} /dev/zero && : > '{}' && sleep 30",
                STDOUT_LIMIT + (1 << 20),
                marker.display()
            );
            let done = sh(&code, Duration::from_secs(3)).await;
            assert_eq!(done.exit(), Exit::TimedOut);
            assert!(done.stdout().bytes().len() <= STDOUT_LIMIT);
            let interrupted = Err(PayloadError::Interrupted {
                exit: Exit::TimedOut,
            });
            assert_eq!(done.payload(), interrupted);
            if marker.exists() {
                // 9 MiB went through the pipe, so more than 8 MiB was read.
                assert!(done.stdout().is_truncated());
                assert_eq!(done.stdout().bytes().len(), STDOUT_LIMIT);
            } else {
                eprintln!("note: the child did not pass 8 MiB before the timeout");
            }
        }

        #[tokio::test]
        async fn a_zero_timeout_times_out_at_once() {
            let done = sh("sleep 30", Duration::ZERO).await;
            assert_eq!(done.exit(), Exit::TimedOut);
            assert!(done.elapsed() < Duration::from_secs(10));
        }

        #[tokio::test]
        async fn a_timeout_kills_the_child_and_its_descendants() {
            let dir = tempfile::tempdir().unwrap();
            let (beats, pids) = (dir.path().join("beats"), dir.path().join("pids"));
            let code = format!("echo started; {}", heartbeat(&beats, &pids));
            let timeout = Duration::from_secs(3);
            let done = sh(&code, timeout).await;
            assert_eq!(done.exit(), Exit::TimedOut);
            assert!(done.elapsed() >= timeout, "{:?}", done.elapsed());
            // Under load the script may not have started before the
            // timeout: only what the PID file proves is checked.
            let pids = recorded_pids(&pids);
            if pids.is_empty() {
                eprintln!("note: the script did not start before the timeout");
                return;
            }
            // `echo started` ran before the first PID was recorded: what
            // it printed before the kill is kept in stdout, but it is no
            // payload: the call did not run to its end.
            assert_eq!(
                done.payload(),
                Err(PayloadError::Interrupted {
                    exit: Exit::TimedOut
                })
            );
            let stdout = String::from_utf8_lossy(done.stdout().bytes());
            assert!(stdout.starts_with("\napvm-"), "{stdout:?}");
            assert!(stdout.ends_with("\nstarted\n"), "{stdout:?}");
            if tree_visible() {
                let check = move || assert_all_gone(&pids, "a descendant");
                tokio::task::spawn_blocking(check).await.unwrap();
            }
        }

        #[tokio::test]
        async fn a_call_in_a_process_scope_dies_with_the_scopes_kill_all() {
            // The Node bindings run every call in their env's scope and
            // call `kill_all` at `process.exit()`: an `ssh` (here a local
            // shell) started by this crate must be in that scope too.
            if !tree_visible() {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let (beats, pids) = (dir.path().join("beats"), dir.path().join("pids"));
            let request = Request::script(framed(&heartbeat(&beats, &pids)), LONG);
            let scope = apvm_process::ProcessScope::new();
            let call = {
                let scope = scope.clone();
                tokio::spawn(async move {
                    let shell = LocalShell::new("sh");
                    scope.run(shell.execute(&request)).await
                })
            };
            let (file, list) = (beats.clone(), pids.clone());
            tokio::task::spawn_blocking(move || {
                wait_for_lines(&list, 2);
                wait_for_lines(&file, 2);
            })
            .await
            .unwrap();

            let killer = scope.clone();
            tokio::task::spawn_blocking(move || killer.kill_all())
                .await
                .unwrap();

            let pids = recorded_pids(&pids);
            let check = move || assert_all_gone(&pids, "a descendant");
            tokio::task::spawn_blocking(check).await.unwrap();
            let done = call.await.unwrap().unwrap();
            assert_eq!(done.exit(), Exit::Killed { signal: Some(9) });
            // And the closed scope refuses the next call.
            let next = Request::script(framed("true"), LONG);
            let refused = scope.run(LocalShell::new("sh").execute(&next)).await;
            assert_eq!(refused.unwrap_err().code(), crate::Code::ProcessFailed);
        }

        #[tokio::test]
        async fn dropping_the_call_kills_the_child_and_its_descendants() {
            if !tree_visible() {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let (beats, pids) = (dir.path().join("beats"), dir.path().join("pids"));
            let request = Request::script(framed(&heartbeat(&beats, &pids)), LONG);
            let call = tokio::spawn(async move { LocalShell::new("sh").execute(&request).await });
            let (file, list) = (beats.clone(), pids.clone());
            tokio::task::spawn_blocking(move || {
                wait_for_lines(&list, 2);
                wait_for_lines(&file, 2);
            })
            .await
            .unwrap();

            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());

            let pids = recorded_pids(&pids);
            assert_eq!(pids.len(), 2, "the shell and its heartbeat");
            let check = move || assert_all_gone(&pids, "a descendant");
            tokio::task::spawn_blocking(check).await.unwrap();
            assert!(!lines(&beats).is_empty());
        }
    }
}
