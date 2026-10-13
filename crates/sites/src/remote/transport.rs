//! The transport contract: what runs a framed script and how its result
//! comes back (plan §6.1 items 3, 6 and 7; §8.1 item 5).
//!
//! A [`Transport`] runs one [`Request`] and returns an [`Execution`]:
//!
//! - **Delivery** ([`Delivery`]): a [`Script`] travels on stdin to the
//!   remote command `sh -s`; an upload's [`UploadScript`] *is* the remote
//!   command (`sh -c '<script>'`) and stdin carries the [`Payload`].
//! - **Result:** how it ended ([`Exit`]: the script's status, a transport
//!   failure, a signal or the timeout), stdout and stderr up to their
//!   limits with truncation flags ([`Captured`]), whether all of stdin was
//!   written, and the time it took. [`Execution::payload`] is the framed
//!   output; a missing nonce line or a cut payload is an error, never "no
//!   output" or a shorter one.
//! - **Lifetime:** a call never outlives its future. When it is dropped
//!   (cancelled) or its timeout passes, the local process and everything
//!   it started are killed.
//!
//! Transports never retry: retrying is the caller's choice (reads only, on
//! transport failures only, plan §6.1 item 8).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::capture::{Captured, PayloadError, unframe};
use super::{Nonce, Script, UploadScript};
use crate::error::Result;

/// A boxed, sendable future: what [`Transport::execute`] returns, so the
/// trait stays dyn-compatible without `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs framed scripts somewhere: on a server over SSH, through a local
/// shell (this crate's tests), or a fake (tests of code that runs scripts:
/// build results with [`Execution::new`] and [`Captured::new`]).
///
/// Implementations uphold the module's contract: the remote command is
/// [`Request::remote_command`] with [`Request`]'s stdin, outputs are
/// capped ([`STDOUT_LIMIT`](super::STDOUT_LIMIT),
/// [`STDERR_LIMIT`](super::STDERR_LIMIT)) and drained to their end, the
/// timeout and a dropped future kill whatever the call started, and
/// nothing is retried.
pub trait Transport: Send + Sync {
    /// Run `request`.
    ///
    /// # Arguments
    ///
    /// * `request` - The script, its delivery and its timeout
    ///
    /// # Returns
    ///
    /// How the call ended, once it started: a timeout, a transport failure
    /// (exit 255) and any status are results, not errors.
    ///
    /// # Errors
    ///
    /// When the call could not start or the local side failed: the
    /// program is missing, an upload's file cannot be read, or its output
    /// cannot be read ([`Code::ProcessFailed`](crate::Code::ProcessFailed)
    /// for the local transport). What had started was killed; the remote
    /// command may have run in part.
    fn execute<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Execution>>;
}

/// What an upload streams to the remote command's stdin.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Payload {
    /// Bytes in memory.
    Bytes(Arc<[u8]>),
    /// A local file, streamed as it is read; it must exist when the call
    /// starts.
    File(PathBuf),
}

impl std::fmt::Debug for Payload {
    /// The size, not the bytes (they may be megabytes), or the file.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(bytes) => write!(f, "Bytes({} bytes)", bytes.len()),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
        }
    }
}

impl From<Vec<u8>> for Payload {
    /// Bytes in memory.
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes.into())
    }
}

/// How a script reaches the server (plan §6.1 item 3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivery {
    /// The remote command is `sh -s`; the script travels on stdin, so the
    /// login shell parses only `sh -s`.
    Stdin(Script),
    /// The remote command is `sh -c '<script>'`; stdin carries `payload`.
    Upload {
        /// The script, made of upload-safe values only.
        script: UploadScript,
        /// What its stdin carries.
        payload: Payload,
    },
}

/// One call: what to run, how it is delivered, and how long it may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The script and its delivery.
    delivery: Delivery,
    /// How long the call may run before it is killed.
    timeout: Duration,
}

impl Request {
    /// Deliver `script` on stdin to `sh -s`.
    ///
    /// # Arguments
    ///
    /// * `script` - The framed script
    /// * `timeout` - How long the call may run before it is killed (plan
    ///   §6.1 item 7: 300 s per command); `Duration::ZERO` times out at once
    pub fn script(script: Script, timeout: Duration) -> Self {
        Self {
            delivery: Delivery::Stdin(script),
            timeout,
        }
    }

    /// Run `script` as `sh -c '<script>'`, with `payload` on its stdin.
    ///
    /// # Arguments
    ///
    /// * `script` - The framed upload script
    /// * `payload` - What its stdin carries
    /// * `timeout` - How long the call may run before it is killed (plan
    ///   §6.1 item 7: 900 s for installs)
    pub fn upload(script: UploadScript, payload: Payload, timeout: Duration) -> Self {
        Self {
            delivery: Delivery::Upload { script, payload },
            timeout,
        }
    }

    /// The script and its delivery.
    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    /// How long the call may run before it is killed.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The call's nonce, which frames its output.
    pub fn nonce(&self) -> &Nonce {
        match &self.delivery {
            Delivery::Stdin(script) => script.nonce(),
            Delivery::Upload { script, .. } => script.nonce(),
        }
    }

    /// The command for the login shell (`ssh`'s remote command): `sh -s`,
    /// or `sh -c '<script>'` for an upload.
    pub fn remote_command(&self) -> String {
        match &self.delivery {
            Delivery::Stdin(_) => Script::REMOTE_COMMAND.to_string(),
            Delivery::Upload { script, .. } => script.remote_command(),
        }
    }
}

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Exit {
    /// The remote command exited with this status (never 255). Once the
    /// nonce line is out, it is the script's: its commands' status, with
    /// 255 mapped to [`Exit::REMAPPED_255`]. Before (the remote `sh`
    /// itself failed), it is that failure's.
    Status(i32),
    /// Exit 255: the transport itself failed (ssh could not connect,
    /// authenticate or verify the host key, or lost the connection). Scripts
    /// map their own 255 to 254, so this is never the script's status.
    TransportFailed,
    /// The local process ended without an exit status: killed by `signal`
    /// (Unix), if known, by something other than this call's timeout (a
    /// process scope's `kill_all`, a user's `kill`). On Windows a killed
    /// process has an exit status (`TerminateProcess` gives 1), so it reads
    /// as `Status(1)` there.
    Killed {
        /// The signal number.
        signal: Option<i32>,
    },
    /// The call's timeout passed: the local process and everything it
    /// started were killed. A mutation may or may not have happened.
    TimedOut,
}

impl std::fmt::Display for Exit {
    /// How the call ended, for messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(code) => write!(f, "exit status {code}"),
            Self::TransportFailed => f.write_str("the transport failed (exit 255)"),
            Self::Killed {
                signal: Some(signal),
            } => write!(f, "killed by signal {signal}"),
            Self::Killed { signal: None } => f.write_str("killed"),
            Self::TimedOut => f.write_str("timed out"),
        }
    }
}

impl Exit {
    /// The status a script reports when a command exited 255 (a PHP fatal
    /// before WordPress loads, or during activation, plan §3.5).
    pub const REMAPPED_255: i32 = 254;
    /// The exit status that means the transport failed.
    const TRANSPORT_FAILURE: i32 = 255;

    /// The exit for a transport's exit status `code`: 255 is a transport
    /// failure, anything else the remote command's status.
    ///
    /// # Arguments
    ///
    /// * `code` - The local process's exit status
    pub fn from_code(code: i32) -> Self {
        if code == Self::TRANSPORT_FAILURE {
            return Self::TransportFailed;
        }
        Self::Status(code)
    }

    /// The exit for a finished local process.
    ///
    /// # Arguments
    ///
    /// * `status` - Its exit status
    pub(crate) fn from_status(status: std::process::ExitStatus) -> Self {
        match status.code() {
            Some(code) => Self::from_code(code),
            None => Self::Killed {
                signal: signal_of(status),
            },
        }
    }

    /// Whether the script ran and succeeded (status 0).
    pub fn success(self) -> bool {
        self == Self::Status(0)
    }

    /// This exit with its invariant restored: `Status(255)`, which a
    /// caller can build by hand, is the transport failure it means.
    fn normalized(self) -> Self {
        match self {
            Self::Status(code) => Self::from_code(code),
            other => other,
        }
    }
}

/// The signal that ended a process.
///
/// # Arguments
///
/// * `status` - Its exit status
#[cfg(unix)]
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(&status)
}

/// The signal that ended a process: none outside Unix.
///
/// # Arguments
///
/// * `_status` - Its exit status
#[cfg(not(unix))]
fn signal_of(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

/// The result of a call that started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    /// The call's nonce, which frames stdout.
    nonce: Nonce,
    /// How it ended.
    exit: Exit,
    /// Its stdout (login noise, the nonce line, the payload), capped.
    stdout: Captured,
    /// Its stderr (login noise and the command's messages), capped.
    stderr: Captured,
    /// Whether all of stdin was written and then closed.
    stdin_written: bool,
    /// From the start to the exit (or the kill).
    elapsed: Duration,
}

impl Execution {
    /// A call's result, as a [`Transport`] reports it (this crate's
    /// transports, one outside it, or a fake in tests).
    ///
    /// `exit` is normalized: `Exit::Status(255)` becomes
    /// [`Exit::TransportFailed`], since a 255 always means the transport
    /// failed ([`Exit::from_code`]).
    ///
    /// # Arguments
    ///
    /// * `nonce` - The call's nonce ([`Request::nonce`]), which frames
    ///   `stdout`
    /// * `exit` - How it ended
    /// * `stdout` - Its stdout, as kept
    /// * `stderr` - Its stderr, as kept
    /// * `stdin_written` - Whether all of stdin was written and closed
    /// * `elapsed` - How long it ran
    pub fn new(
        nonce: Nonce,
        exit: Exit,
        stdout: Captured,
        stderr: Captured,
        stdin_written: bool,
        elapsed: Duration,
    ) -> Self {
        Self {
            nonce,
            exit: exit.normalized(),
            stdout,
            stderr,
            stdin_written,
            elapsed,
        }
    }

    /// How the call ended.
    pub fn exit(&self) -> Exit {
        self.exit
    }

    /// The whole stdout as kept: login noise, the nonce line and the
    /// payload. Parse [`Self::payload`] instead.
    pub fn stdout(&self) -> &Captured {
        &self.stdout
    }

    /// The stderr as kept: login noise and the command's messages, never
    /// framed (show it escaped, as remote text).
    pub fn stderr(&self) -> &Captured {
        &self.stderr
    }

    /// Whether all of stdin (the script, or the upload) was handed to the
    /// transport and stdin then closed. `false` when the other side stopped
    /// reading first (it exited, or closed stdin): an upload is then
    /// incomplete (the brace group keeps a cut script from running).
    ///
    /// `true` is not proof of receipt: the bytes may still sit in a pipe
    /// or socket buffer that nobody reads (a remote command that exits
    /// without reading a small input). Verify what matters on the other
    /// side, as staging does with the upload's SHA-256 (plan §6.6).
    pub fn stdin_written(&self) -> bool {
        self.stdin_written
    }

    /// How long the call ran: from its start to its exit or kill.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// The script's whole output: stdout after the nonce line (plan §6.1
    /// item 2; see [`unframe`]), from a call that ran to its end
    /// ([`Exit::Status`]).
    ///
    /// # Errors
    ///
    /// [`PayloadError::Interrupted`] when the call did not run to its end
    /// (a transport failure, a kill, a timeout): what it printed is at most
    /// a beginning ([`Self::stdout`] still holds it).
    /// [`PayloadError::NonceMissing`] when no kept stdout line equals the
    /// nonce: the script did not run as sent (never read that as "no
    /// output"; after a mutation, re-read the state).
    /// [`PayloadError::Truncated`] when stdout went past its limit, so the
    /// payload is only a beginning.
    pub fn payload(&self) -> std::result::Result<&[u8], PayloadError> {
        if !matches!(self.exit, Exit::Status(_)) {
            return Err(PayloadError::Interrupted { exit: self.exit });
        }
        unframe(&self.stdout, &self.nonce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{FileMode, Identity, WpCli};

    /// A fixed nonce.
    fn nonce() -> Nonce {
        Nonce::parse("apvm-0123456789abcdef0123456789abcdef").unwrap()
    }

    /// A `wp plugin list` script.
    fn script() -> Script {
        let wp = WpCli::new(&["wp".into()], FileMode::new(0o755).unwrap(), "/srv/www")
            .unwrap()
            .args(["plugin", "list"])
            .unwrap();
        Script::wp(nonce(), &Identity::Login, &wp)
    }

    /// An upload of stdin into `/tmp/apvm.x/package-1.zip`.
    fn upload() -> UploadScript {
        UploadScript::tee(nonce(), &Identity::Login, "/tmp/apvm.x/package-1.zip").unwrap()
    }

    /// An execution with `exit` and `stdout`.
    ///
    /// # Arguments
    ///
    /// * `exit` - How it ended
    /// * `stdout` - Its stdout
    fn execution(exit: Exit, stdout: &str) -> Execution {
        Execution::new(
            nonce(),
            exit,
            Captured::whole(stdout.as_bytes().to_vec()),
            Captured::whole(b"warning\n".to_vec()),
            true,
            Duration::from_millis(5),
        )
    }

    #[test]
    fn a_stdin_request_runs_sh_s_with_the_script_on_stdin() {
        let request = Request::script(script(), Duration::from_secs(300));
        assert_eq!(request.remote_command(), "sh -s");
        assert_eq!(request.nonce(), &nonce());
        assert_eq!(request.timeout(), Duration::from_secs(300));
        assert_eq!(request.delivery(), &Delivery::Stdin(script()));
    }

    #[test]
    fn an_upload_request_runs_its_script_as_the_remote_command() {
        let payload = Payload::from(vec![1, 2, 3]);
        let request = Request::upload(upload(), payload.clone(), Duration::from_secs(900));
        assert_eq!(request.remote_command(), upload().remote_command());
        assert!(request.remote_command().starts_with("sh -c '{ LC_ALL=C;"));
        assert_eq!(request.nonce(), &nonce());
        assert_eq!(request.timeout(), Duration::from_secs(900));
        let Delivery::Upload {
            script,
            payload: carried,
        } = request.delivery()
        else {
            panic!("an upload delivery");
        };
        assert_eq!((script, carried), (&upload(), &payload));
    }

    #[test]
    fn payloads_debug_print_their_size_not_their_bytes() {
        assert_eq!(
            format!("{:?}", Payload::from(vec![0; 20_000_000])),
            "Bytes(20000000 bytes)"
        );
        assert_eq!(
            format!("{:?}", Payload::File("/tmp/a.zip".into())),
            r#"File("/tmp/a.zip")"#
        );
    }

    #[test]
    fn exit_255_is_the_transport_and_every_other_code_the_script() {
        assert_eq!(Exit::from_code(255), Exit::TransportFailed);
        for code in [0, 1, 2, 127, 254, 256, -1, i32::MIN, i32::MAX] {
            assert_eq!(Exit::from_code(code), Exit::Status(code));
        }
        assert_eq!(Exit::REMAPPED_255, 254);
    }

    #[test]
    fn only_status_zero_is_success() {
        assert!(Exit::Status(0).success());
        for exit in [
            Exit::Status(1),
            Exit::Status(Exit::REMAPPED_255),
            Exit::TransportFailed,
            Exit::Killed { signal: Some(9) },
            Exit::Killed { signal: None },
            Exit::TimedOut,
        ] {
            assert!(!exit.success(), "{exit:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn exit_statuses_map_codes_and_signals() {
        use std::os::unix::process::ExitStatusExt;

        // Raw wait statuses: the code in the second byte, a signal in the first.
        assert_eq!(
            Exit::from_status(ExitStatusExt::from_raw(0)),
            Exit::Status(0)
        );
        assert_eq!(
            Exit::from_status(ExitStatusExt::from_raw(254 << 8)),
            Exit::Status(254)
        );
        assert_eq!(
            Exit::from_status(ExitStatusExt::from_raw(255 << 8)),
            Exit::TransportFailed
        );
        assert_eq!(
            Exit::from_status(ExitStatusExt::from_raw(9)),
            Exit::Killed { signal: Some(9) }
        );
    }

    #[test]
    fn the_payload_is_stdout_after_the_nonce_line() {
        let framed = format!("motd\n{}\n[]\n", nonce());
        let done = execution(Exit::Status(0), &framed);
        assert_eq!(done.payload(), Ok(&b"[]\n"[..]));
        assert_eq!(done.stdout().bytes(), framed.as_bytes());
        assert_eq!(done.stderr().bytes(), b"warning\n");
        assert!(done.stdin_written());
        assert_eq!(done.elapsed(), Duration::from_millis(5));
        assert_eq!(done.exit(), Exit::Status(0));
    }

    #[test]
    fn a_missing_nonce_line_is_an_error_not_empty_output() {
        let missing = Err(PayloadError::NonceMissing {
            stdout_truncated: false,
        });
        let eaten = execution(Exit::Status(2), "motd\n");
        assert_eq!(eaten.payload(), missing);
        let silent = execution(Exit::Status(127), "");
        assert_eq!(silent.payload(), missing);
    }

    #[test]
    fn a_call_that_did_not_run_to_its_end_has_no_payload() {
        // Even framed output is only a beginning when the call was cut.
        let framed = format!("{}\nstarted\n", nonce());
        for exit in [
            Exit::TransportFailed,
            Exit::Killed { signal: Some(9) },
            Exit::Killed { signal: None },
            Exit::TimedOut,
        ] {
            let cut = execution(exit, &framed);
            assert_eq!(cut.payload(), Err(PayloadError::Interrupted { exit }));
            assert_eq!(cut.stdout().bytes(), framed.as_bytes(), "still kept");
        }
    }

    #[test]
    fn exits_and_interruptions_read_as_sentences() {
        assert_eq!(Exit::Status(3).to_string(), "exit status 3");
        assert_eq!(
            Exit::TransportFailed.to_string(),
            "the transport failed (exit 255)"
        );
        assert_eq!(
            Exit::Killed { signal: Some(9) }.to_string(),
            "killed by signal 9"
        );
        assert_eq!(Exit::Killed { signal: None }.to_string(), "killed");
        assert_eq!(Exit::TimedOut.to_string(), "timed out");
        let interrupted = PayloadError::Interrupted {
            exit: Exit::TimedOut,
        };
        assert_eq!(
            interrupted.to_string(),
            "the call did not run to its end (timed out): its output is incomplete"
        );
    }

    #[test]
    fn a_cut_payload_is_an_error_not_a_shorter_output() {
        let stdout = Captured::new(format!("{}\n[1,", nonce()).into_bytes(), true);
        let cut = Execution::new(
            nonce(),
            Exit::Status(0),
            stdout,
            Captured::default(),
            true,
            Duration::ZERO,
        );
        assert_eq!(cut.payload(), Err(PayloadError::Truncated));
        assert!(cut.stdout().bytes().ends_with(b"[1,"), "still kept");
    }

    #[test]
    fn a_hand_built_status_255_is_a_transport_failure() {
        let built = execution(Exit::Status(255), "");
        assert_eq!(built.exit(), Exit::TransportFailed);
        for exit in [
            Exit::Status(254),
            Exit::TimedOut,
            Exit::Killed { signal: None },
        ] {
            assert_eq!(execution(exit, "").exit(), exit);
        }
    }

    #[test]
    fn the_trait_is_dyn_compatible_and_its_futures_are_send() {
        /// Accepts only `Send` values.
        fn send<T: Send>(_: T) {}

        /// A transport that answers every call with exit 0 and no output.
        struct Silent;

        impl Transport for Silent {
            fn execute<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Execution>> {
                let stdout = format!("{}\n", request.nonce()).into_bytes();
                Box::pin(async move {
                    Ok(Execution::new(
                        request.nonce().clone(),
                        Exit::Status(0),
                        Captured::whole(stdout),
                        Captured::default(),
                        true,
                        Duration::ZERO,
                    ))
                })
            }
        }

        let transport: Box<dyn Transport> = Box::new(Silent);
        let request = Request::script(script(), Duration::from_secs(1));
        send(transport.execute(&request));
    }
}
