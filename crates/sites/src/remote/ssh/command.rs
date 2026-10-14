//! The short ssh commands around a shared call (`-O stop`,
//! `-G`, a master's starter): how they are built and run (P3.4).
//!
//! Every one runs like a call (apvm-process, ssh's environment, no stdin,
//! in a private directory) with a timeout of its own, and keeps what it
//! printed, also when it was killed for its time.

use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

use super::super::local;
use super::SSH_ENV;

/// The most output kept from one of these commands, per stream.
const KEPT: usize = 64 * 1024;

/// The ssh commands around one call.
#[derive(Debug, Clone, Copy)]
pub(super) struct Commands<'a> {
    /// The ssh program.
    pub(super) program: &'a Path,
    /// The generated file, as ssh gets it (a call's: `ControlMaster no`).
    pub(super) file: &'a Path,
    /// The directory the commands run in (a private one).
    pub(super) dir: &'a Path,
    /// `[-l <user>] [-p <port>] [-J <hops>] -- <host>`.
    pub(super) destination: &'a [OsString],
    /// The connect timeout ssh is given.
    pub(super) connect_timeout: Duration,
}

impl Commands<'_> {
    /// `<program> -F <file> <before…> <destination> <after…>`, in the
    /// private directory with ssh's environment, no stdin, output piped.
    ///
    /// # Arguments
    ///
    /// * `file` - The generated file to use
    /// * `before` - Options before the destination
    /// * `after` - Arguments after it (a remote command)
    pub(super) fn command(
        &self,
        file: &Path,
        before: &[&str],
        after: &[&str],
    ) -> tokio::process::Command {
        let mut command = apvm_process::command(self.program);
        command
            .arg("-F")
            .arg(file)
            .args(before)
            .args(self.destination)
            .args(after)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(self.dir);
        local::apply_env(&mut command, &SSH_ENV);
        command
    }
}

/// How one of these commands ended.
#[derive(Debug, Default)]
pub(super) struct Finished {
    /// Its exit status; `None` when it could not start, took too long, or
    /// was stopped early (it was killed then, tree and all).
    pub(super) status: Option<ExitStatus>,
    /// Whether it was stopped early because its stderr showed what the
    /// caller watched for.
    pub(super) stopped: bool,
    /// Its stdout.
    pub(super) stdout: Vec<u8>,
    /// Its stderr, as far as it came.
    pub(super) stderr: Vec<u8>,
}

impl Finished {
    /// Whether it ended with exit status 0.
    pub(super) fn succeeded(&self) -> bool {
        self.status.is_some_and(|status| status.success())
    }
}

/// Run `command` for at most `limit`, keeping its output.
///
/// # Arguments
///
/// * `command` - The command, its stdout and stderr piped
/// * `limit` - Its timeout
pub(super) async fn run(command: tokio::process::Command, limit: Duration) -> Finished {
    run_until(command, limit, |_| false).await
}

/// Run `command` for at most `limit`, keeping its output, and kill it as
/// soon as its stderr satisfies `stop` ([`Finished::stopped`]).
///
/// # Arguments
///
/// * `command` - The command, its stdout and stderr piped
/// * `limit` - Its timeout
/// * `stop` - Whether what it printed on stderr so far ends its run
pub(super) async fn run_until(
    mut command: tokio::process::Command,
    limit: Duration,
    stop: impl Fn(&[u8]) -> bool,
) -> Finished {
    let mut finished = Finished::default();
    let Ok((mut pipes, mut running)) = apvm_process::spawn(&mut command) else {
        return finished;
    };
    let work = async {
        if drain(&mut pipes, &mut finished, &stop).await {
            return (None, true);
        }
        (running.wait().await.ok(), false)
    };
    // What was read before a timeout stays in the buffers.
    let (status, stopped) = tokio::time::timeout(limit, work)
        .await
        .unwrap_or((None, false));
    if status.is_none() {
        running.kill();
    }
    finished.status = status;
    finished.stopped = stopped;
    finished
}

/// Read `pipes`' stdout and stderr into `finished` until both end, or until
/// stderr satisfies `stop`: whether it did.
///
/// # Arguments
///
/// * `pipes` - The command's pipes
/// * `finished` - Where the output goes
/// * `stop` - Whether what stderr printed so far ends the run
async fn drain(
    pipes: &mut apvm_process::Pipes,
    finished: &mut Finished,
    stop: &impl Fn(&[u8]) -> bool,
) -> bool {
    let out = read_kept(pipes.stdout.as_mut(), &mut finished.stdout, |_| false);
    let err = read_kept(pipes.stderr.as_mut(), &mut finished.stderr, stop);
    tokio::pin!(out, err);
    let (mut out_done, mut err_done) = (false, false);
    while !(out_done && err_done) {
        tokio::select! {
            watched = &mut err, if !err_done => {
                if watched.unwrap_or(false) {
                    return true;
                }
                err_done = true;
            }
            _ = &mut out, if !out_done => out_done = true,
        }
    }
    false
}

/// Read `stream` to its end into `into`, keeping at most [`KEPT`] bytes
/// (the rest is read and dropped, so the process never blocks), or until
/// what was kept satisfies `stop`.
///
/// # Arguments
///
/// * `stream` - The pipe, if any
/// * `into` - Where the bytes go
/// * `stop` - Whether to stop reading
///
/// # Errors
///
/// When the pipe cannot be read.
async fn read_kept(
    stream: Option<&mut (impl AsyncRead + Unpin)>,
    into: &mut Vec<u8>,
    stop: impl Fn(&[u8]) -> bool,
) -> std::io::Result<bool> {
    let Some(stream) = stream else {
        return Ok(false);
    };
    let mut buffer = [0u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(false);
        }
        let room = KEPT.saturating_sub(into.len());
        into.extend_from_slice(&buffer[..read.min(room)]);
        if stop(into) {
            return Ok(true);
        }
    }
}

/// Whether ssh itself printed `line` in `stderr`: ssh ends each of its own
/// messages with `\r\n`, while what it relays from the server ends as the
/// server wrote it (`remote/classify.rs`), so a remote program cannot pass
/// for ssh.
///
/// # Arguments
///
/// * `stderr` - ssh's stderr
/// * `line` - The message, without its ending
pub(super) fn ssh_said(stderr: &[u8], line: &str) -> bool {
    String::from_utf8_lossy(stderr)
        .split('\n')
        .filter_map(|text| text.strip_suffix('\r'))
        .any(|text| text.trim() == line)
}

/// Whether ssh itself printed a line ending with `tail` in `stderr` (see
/// [`ssh_said`]).
///
/// # Arguments
///
/// * `stderr` - ssh's stderr
/// * `tail` - The end of the message
pub(super) fn ssh_said_ending(stderr: &[u8], tail: &str) -> bool {
    String::from_utf8_lossy(stderr)
        .split('\n')
        .filter_map(|text| text.strip_suffix('\r'))
        .any(|text| text.trim_end().ends_with(tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sshs_own_lines_count() {
        let stderr = b"muxclient: master hello exchange failed\r\nnext\r\n";
        assert!(ssh_said(stderr, "muxclient: master hello exchange failed"));
        // A remote program's line ends with `\n` alone.
        let remote = b"muxclient: master hello exchange failed\n";
        assert!(!ssh_said(remote, "muxclient: master hello exchange failed"));
        assert!(!ssh_said(
            b"echo muxclient: master hello exchange failed\r\n",
            "muxclient: master hello exchange failed"
        ));
        assert!(!ssh_said(b"", "x"));
        let race = b"ControlSocket /m/abc already exists, disabling multiplexing\r\n";
        assert!(ssh_said_ending(
            race,
            "already exists, disabling multiplexing"
        ));
        assert!(!ssh_said_ending(
            b"ControlSocket /m/abc already exists, disabling multiplexing\n",
            "already exists, disabling multiplexing"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_is_stopped_when_its_stderr_shows_what_is_watched() {
        let mut command = apvm_process::command("sh");
        command
            .args(["-c", "printf 'start\\r\\nSTOP\\r\\n' >&2; sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = std::time::Instant::now();
        let finished = run_until(command, Duration::from_secs(20), |stderr| {
            ssh_said(stderr, "STOP")
        })
        .await;
        assert!(finished.stopped);
        assert_eq!(finished.status, None);
        assert_eq!(finished.stderr, b"start\r\nSTOP\r\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_killed_for_its_time_keeps_what_it_printed() {
        let mut command = apvm_process::command("sh");
        command
            .args(["-c", "printf 'notice\\r\\n' >&2; sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let finished = run(command, Duration::from_millis(500)).await;
        assert!(!finished.stopped);
        assert_eq!(finished.status, None);
        assert_eq!(finished.stderr, b"notice\r\n");
    }
}
