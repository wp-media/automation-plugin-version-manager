//! One remote call with its transport-level failures turned into errors:
//! what every caller that runs scripts (the probe, the session) needs
//! before it reads a script's output (plan §6.1 items 2 and 7, P3.3's
//! obligations).
//!
//! | How the call ended | Result |
//! |---|---|
//! | exit 255 | [`Error::Ssh`] from [`classify`] (`ssh_*`; after the nonce line, `ssh_connection_lost`) |
//! | its timeout passed | [`Error::Timeout`] (`timeout`), saying whether the script had started |
//! | killed by something else | [`Error::Killed`] (`process_failed`) |
//! | a status, but no nonce line | [`Error::ScriptNotRun`] (`ssh_failed`): the login shell did not run the script as sent |
//! | a status after the nonce line | the [`Execution`], whatever the status |

use std::time::Duration;

use super::capture::{PayloadError, after_nonce_line};
use super::classify::classify;
use super::script::{Nonce, Script};
use super::transport::{Execution, Exit, Request, Transport};
use crate::error::{Error, Result};
use crate::problem::printable;

/// Run `script` through `transport` and keep only a call that ran it to
/// its end (the module's table).
///
/// # Arguments
///
/// * `transport` - Where it runs
/// * `script` - The script
/// * `timeout` - How long it may run
///
/// # Errors
///
/// The transport's own error, or the error of the module's table.
pub(crate) async fn call(
    transport: &dyn Transport,
    script: Script,
    timeout: Duration,
) -> Result<Execution> {
    let request = Request::script(script, timeout);
    let execution = transport.execute(&request).await?;
    finished(execution, timeout)
}

/// `execution` when its script ran to its end, else the error that says
/// why not.
///
/// # Arguments
///
/// * `execution` - The call's result
/// * `timeout` - The call's timeout (for the message)
///
/// # Errors
///
/// The module's table.
pub(crate) fn finished(execution: Execution, timeout: Duration) -> Result<Execution> {
    match execution.exit() {
        Exit::TransportFailed => Err(transport_failure(&execution)),
        Exit::TimedOut => Err(Error::Timeout {
            after: timeout,
            started: execution.script_started(),
        }),
        Exit::Status(_) => match execution.payload() {
            Err(PayloadError::NonceMissing { .. }) => Err(Error::ScriptNotRun {
                exit: execution.exit(),
                line: last_line(execution.stderr().bytes()),
            }),
            _ => Ok(execution),
        },
        exit => Err(Error::Killed { exit }),
    }
}

/// The error of a call that exited 255.
///
/// # Arguments
///
/// * `execution` - The call
fn transport_failure(execution: &Execution) -> Error {
    match classify(execution) {
        Some(failure) => Error::Ssh(failure),
        // classify() always answers for exit 255; never assume it.
        None => Error::ScriptNotRun {
            exit: execution.exit(),
            line: last_line(execution.stderr().bytes()),
        },
    }
}

/// stdout after the nonce line of a call [`finished`] returned: the whole
/// payload, or only its beginning when stdout went past its limit
/// (`execution.stdout().is_truncated()`; parsers refuse it then).
///
/// # Arguments
///
/// * `execution` - The call
/// * `nonce` - Its script's nonce
pub(crate) fn framed_stdout<'a>(execution: &'a Execution, nonce: &Nonce) -> &'a [u8] {
    after_nonce_line(execution.stdout().bytes(), nonce).unwrap_or_default()
}

/// The last non-blank line of `stderr`, escaped for display and at most
/// 240 characters.
///
/// # Arguments
///
/// * `stderr` - The bytes
pub(crate) fn last_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    Some(printable(&line.chars().take(240).collect::<String>()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;
    use crate::remote::capture::Captured;
    use crate::remote::fake::{FakeTransport, Reply};
    use crate::remote::{RootAccess, RootCommand};

    /// A root `stat` script.
    fn script() -> Script {
        let command = RootCommand::stat_ownership("/srv/p").unwrap();
        Script::root(Nonce::random(), RootAccess::RootLogin, &command)
    }

    /// The error of a call answered with `reply`.
    ///
    /// # Arguments
    ///
    /// * `reply` - The canned answer
    async fn error_of(reply: Reply) -> Error {
        let fake = FakeTransport::new([reply]);
        call(&fake, script(), Duration::from_secs(7))
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn finished_calls_come_back_whatever_their_status() {
        for code in [0, 1, 127, Exit::REMAPPED_255] {
            let fake = FakeTransport::new([Reply::status(code, "out", "err")]);
            let execution = call(&fake, script(), Duration::from_secs(7)).await.unwrap();
            assert_eq!(execution.exit(), Exit::Status(code));
            assert_eq!(execution.payload(), Ok(&b"out"[..]));
        }
        // A cut stdout is the parser's to refuse.
        let fake = FakeTransport::new([Reply::ok("x")]);
        let execution = call(&fake, script(), Duration::from_secs(7)).await.unwrap();
        assert!(execution.script_started());
    }

    #[tokio::test]
    async fn transport_failures_are_classified() {
        let refused = error_of(Reply::Unframed {
            exit: Exit::TransportFailed,
            stdout: Vec::new(),
            stderr: b"ssh: connect to host x port 22: Connection refused\r\n".to_vec(),
        })
        .await;
        assert_eq!(refused.code(), Code::SshUnreachable);
        let lost = error_of(Reply::status(255, "partial", "")).await;
        assert_eq!(lost.code(), Code::SshConnectionLost);
    }

    #[tokio::test]
    async fn timeouts_say_whether_the_script_started() {
        let error = error_of(Reply::timed_out("partial")).await;
        assert_eq!(error.code(), Code::Timeout);
        assert!(matches!(error, Error::Timeout { started: true, .. }));
        assert_eq!(
            error.to_string(),
            "the remote command ran past its 7 s timeout and was killed; it may have run in part"
        );
        let unstarted = error_of(Reply::Unframed {
            exit: Exit::TimedOut,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
        .await;
        assert!(matches!(unstarted, Error::Timeout { started: false, .. }));
        assert_eq!(
            unstarted.to_string(),
            "the remote command ran past its 7 s timeout and was killed"
        );
    }

    #[tokio::test]
    async fn a_script_the_login_shell_did_not_run_is_ssh_failed() {
        let error = error_of(Reply::Unframed {
            exit: Exit::Status(1),
            stdout: b"motd\n".to_vec(),
            stderr: b"This account is currently not available.\n\n".to_vec(),
        })
        .await;
        assert_eq!(error.code(), Code::SshFailed);
        assert_eq!(
            error.to_string(),
            "the server did not run apvm's script (exit status 1): \
             'This account is currently not available.'"
        );
    }

    #[test]
    fn killed_calls_are_process_failures() {
        let execution = Execution::new(
            Nonce::random(),
            Exit::Killed { signal: Some(9) },
            Captured::default(),
            Captured::default(),
            true,
            Duration::from_millis(1),
        );
        let error = finished(execution, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.code(), Code::ProcessFailed);
        assert_eq!(
            error.to_string(),
            "the local transport process was killed by signal 9 before the remote command ended"
        );
    }

    #[test]
    fn last_lines_skip_blanks_and_escape() {
        assert_eq!(last_line(b"a\n b \n\n"), Some("b".into()));
        assert_eq!(last_line(b"x\x1b[31m\n"), Some("x\\u{1b}[31m".into()));
        assert_eq!(last_line(b"\n \n"), None);
        assert_eq!(last_line(&[b'y'; 500]).unwrap().len(), 240);
    }
}
