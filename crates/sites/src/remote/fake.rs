//! A scripted [`Transport`] for unit tests of what runs scripts (the probe,
//! the executor, staging): it records every request and answers each with
//! the next canned [`Reply`], framed with that request's nonce as a real
//! script's output would be.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::capture::Captured;
use super::transport::{BoxFuture, Execution, Exit, Request, Transport};
use crate::error::{Error, Result};

/// How long every faked call claims to have run.
const ELAPSED: Duration = Duration::from_millis(1);

/// What a [`FakeTransport`] answers one call with.
pub(crate) enum Reply {
    /// The script ran: its nonce line, then `payload` on stdout.
    Ran {
        /// How it ended.
        exit: Exit,
        /// Stdout after the nonce line.
        payload: Vec<u8>,
        /// Its stderr.
        stderr: Vec<u8>,
    },
    /// The script did not run as sent: stdout has no nonce line (startup
    /// files ate it, the remote `sh` failed, ssh failed).
    Unframed {
        /// How it ended.
        exit: Exit,
        /// The whole stdout.
        stdout: Vec<u8>,
        /// Its stderr.
        stderr: Vec<u8>,
    },
    /// The call failed before or while starting.
    Fails(Box<dyn FnOnce() -> Error + Send>),
    /// The call never ends (its caller's timeout or cancellation must).
    Hangs,
}

impl Reply {
    /// Exit 0 with `payload` after the nonce line, and no stderr.
    ///
    /// # Arguments
    ///
    /// * `payload` - The script's output
    pub(crate) fn ok(payload: impl Into<Vec<u8>>) -> Self {
        Self::status(0, payload, Vec::new())
    }

    /// Exit `code` (255: a transport failure) with `payload` after the
    /// nonce line, and `stderr`.
    ///
    /// # Arguments
    ///
    /// * `code` - The exit status
    /// * `payload` - The script's output
    /// * `stderr` - Its messages
    pub(crate) fn status(
        code: i32,
        payload: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
    ) -> Self {
        Self::Ran {
            exit: Exit::from_code(code),
            payload: payload.into(),
            stderr: stderr.into(),
        }
    }

    /// The call timed out after the script printed `payload` (kept in
    /// stdout after the nonce line; `payload()` refuses it as incomplete).
    ///
    /// # Arguments
    ///
    /// * `payload` - What it printed before it was killed
    pub(crate) fn timed_out(payload: impl Into<Vec<u8>>) -> Self {
        Self::Ran {
            exit: Exit::TimedOut,
            payload: payload.into(),
            stderr: Vec::new(),
        }
    }

    /// The answer to `request`.
    ///
    /// # Arguments
    ///
    /// * `request` - The call
    async fn answer(self, request: &Request) -> Result<Execution> {
        let (exit, stdout, stderr) = match self {
            Self::Ran {
                exit,
                payload,
                stderr,
            } => {
                // As the scripts print it (`printf '\n%s\n'`).
                let mut stdout = format!("\n{}\n", request.nonce()).into_bytes();
                stdout.extend(payload);
                (exit, stdout, stderr)
            }
            Self::Unframed {
                exit,
                stdout,
                stderr,
            } => (exit, stdout, stderr),
            Self::Fails(error) => return Err(error()),
            Self::Hangs => std::future::pending().await,
        };
        Ok(Execution::new(
            request.nonce().clone(),
            exit,
            Captured::whole(stdout),
            Captured::whole(stderr),
            true,
            ELAPSED,
        ))
    }
}

/// Answers calls with canned [`Reply`]s, in order, and records them.
/// A call beyond the replies fails the test.
pub(crate) struct FakeTransport {
    /// The replies not used yet.
    replies: Mutex<VecDeque<Reply>>,
    /// Every request, in order.
    calls: Mutex<Vec<Request>>,
}

impl FakeTransport {
    /// A transport answering with `replies`, in order.
    ///
    /// # Arguments
    ///
    /// * `replies` - One per expected call
    pub(crate) fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every request so far, in order.
    pub(crate) fn calls(&self) -> Vec<Request> {
        locked(&self.calls).clone()
    }

    /// How many replies are left.
    pub(crate) fn remaining(&self) -> usize {
        locked(&self.replies).len()
    }
}

impl Transport for FakeTransport {
    /// Record `request`, and answer with the next reply.
    fn execute<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Execution>> {
        locked(&self.calls).push(request.clone());
        let reply = locked(&self.replies).pop_front();
        Box::pin(async move {
            let Some(reply) = reply else {
                panic!("unexpected call: {request:?}");
            };
            reply.answer(request).await
        })
    }
}

/// The value behind `mutex`, even if a failed test poisoned it.
///
/// # Arguments
///
/// * `mutex` - The mutex
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;
    use crate::remote::capture::PayloadError;
    use crate::remote::{FileMode, Identity, Nonce, Script, WpCli};

    /// A `wp plugin list` request with a fresh nonce.
    fn request() -> Request {
        let wp = WpCli::new(&["wp".into()], FileMode::new(0o755).unwrap(), "/srv/www")
            .unwrap()
            .args(["plugin", "list"])
            .unwrap();
        let script = Script::wp(Nonce::random(), &Identity::Login, &wp);
        Request::script(script, Duration::from_secs(300))
    }

    #[tokio::test]
    async fn replies_come_in_order_framed_with_each_calls_nonce() {
        let fake = FakeTransport::new([Reply::ok("[]"), Reply::status(1, "", "Error: no")]);
        let (first, second) = (request(), request());

        let done = fake.execute(&first).await.unwrap();
        assert!(
            done.stdout()
                .bytes()
                .starts_with(format!("\n{}\n", first.nonce()).as_bytes())
        );
        assert_eq!(done.exit(), Exit::Status(0));
        assert_eq!(done.payload(), Ok(&b"[]"[..]));
        assert!(done.stdin_written());
        assert_eq!(done.elapsed(), ELAPSED);

        let failed = fake.execute(&second).await.unwrap();
        assert_eq!(failed.exit(), Exit::Status(1));
        assert_eq!(failed.payload(), Ok(&b""[..]));
        assert_eq!(failed.stderr().bytes(), b"Error: no");

        assert_eq!(fake.calls(), [first, second]);
        assert_eq!(fake.remaining(), 0);
    }

    #[tokio::test]
    async fn transport_failures_timeouts_and_unframed_output_are_scripted() {
        let fake = FakeTransport::new([
            Reply::status(
                255,
                "",
                "ssh: connect to host x port 22: Connection refused",
            ),
            Reply::timed_out("partial"),
            Reply::Unframed {
                exit: Exit::Status(2),
                stdout: b"motd\n".to_vec(),
                stderr: Vec::new(),
            },
        ]);
        let ssh = fake.execute(&request()).await.unwrap();
        assert_eq!(ssh.exit(), Exit::TransportFailed);
        let slow = fake.execute(&request()).await.unwrap();
        assert_eq!(
            slow.payload(),
            Err(PayloadError::Interrupted {
                exit: Exit::TimedOut
            })
        );
        assert!(
            slow.stdout().bytes().ends_with(b"\npartial"),
            "kept in stdout"
        );
        let eaten = fake.execute(&request()).await.unwrap();
        assert_eq!(
            eaten.payload(),
            Err(PayloadError::NonceMissing {
                stdout_truncated: false
            })
        );
        assert_eq!(eaten.stdout().bytes(), b"motd\n");
    }

    #[tokio::test]
    async fn errors_are_returned_as_scripted() {
        let fake = FakeTransport::new([Reply::Fails(Box::new(|| Error::Process {
            context: "cannot start",
            program: "ssh".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        }))]);
        let error = fake.execute(&request()).await.unwrap_err();
        assert_eq!(error.code(), Code::ProcessFailed);
        assert_eq!(fake.calls().len(), 1, "a failed call is still recorded");
    }

    #[tokio::test]
    async fn a_hanging_call_never_ends() {
        let fake = FakeTransport::new([Reply::Hangs]);
        let request = request();
        let call = fake.execute(&request);
        let waited = tokio::time::timeout(Duration::from_millis(50), call).await;
        assert!(waited.is_err(), "it never answered");
    }

    #[tokio::test]
    #[should_panic(expected = "unexpected call")]
    async fn a_call_beyond_the_replies_fails_the_test() {
        let fake = FakeTransport::new([]);
        let _ = fake.execute(&request()).await;
    }
}
