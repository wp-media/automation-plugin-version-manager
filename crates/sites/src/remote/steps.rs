//! Independent statements in one script, each with its own exit status,
//! stdout and stderr (plan §6.2: the probe's independent steps, one round
//! trip each).
//!
//! A [`Body`](super::statement::Body) chains its statements with `&&`, so
//! the first failure stops the script. A [`Steps`] runs every statement
//! whatever the others did, and frames each one's output with marker lines
//! built from the call's [`Nonce`]:
//!
//! ```text
//! { printf '\n%s\n' '<nonce>' >&2
//! s0 </dev/null; printf '\n<nonce> 0 %s\n' "$?"; printf '\n<nonce> 0\n' >&2
//! s1 </dev/null; printf '\n<nonce> 1 %s\n' "$?"; printf '\n<nonce> 1\n' >&2
//! … }
//! ```
//!
//! - stdout: after the frame's nonce line, statement *i*'s output ends at
//!   the line `<nonce> <i> <status>`; the newline printed before it ends a
//!   last line the statement left open, and is not part of its output.
//! - stderr: the line `<nonce>` starts the statements' messages (login
//!   noise comes before it), and `<nonce> <i>` ends statement *i*'s.
//!
//! The nonce is random per call, so no statement can print a marker. A
//! missing marker means the script did not run to its end, and its steps
//! are refused rather than guessed ([`StepsError`]). The script's own exit
//! status is the last `printf`'s: always 0 when it ran.

use super::capture::{Captured, PayloadError, after_nonce_line};
use super::script::Nonce;
use super::statement::Statement;
use super::transport::Execution;

/// Statements that run one after the other, each whatever the others
/// did, with every statement's status and output kept apart. Never
/// empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Steps(Vec<Statement>);

impl Steps {
    /// Steps of `first`, then `rest` in order.
    ///
    /// # Arguments
    ///
    /// * `first` - The first statement (steps are never empty)
    /// * `rest` - The statements after it
    pub(crate) fn new(first: Statement, rest: impl IntoIterator<Item = Statement>) -> Self {
        Self(std::iter::once(first).chain(rest).collect())
    }

    /// How many statements there are.
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// The commands' shell text, for [`Script::steps`](super::Script):
    /// one brace group, so the frame's `cd / &&` guards every statement.
    ///
    /// # Arguments
    ///
    /// * `nonce` - The call's nonce, which the markers carry
    pub(crate) fn render(&self, nonce: &Nonce) -> String {
        let nonce = nonce.as_str();
        let mut lines = vec![format!(r"{{ printf '\n%s\n' '{nonce}' >&2")];
        for (index, statement) in self.0.iter().enumerate() {
            lines.push(format!(
                r#"{} </dev/null; printf '\n{nonce} {index} %s\n' "$?"; printf '\n{nonce} {index}\n' >&2"#,
                statement.render()
            ));
        }
        let mut text = lines.join("\n");
        text.push_str("\n}");
        text
    }
}

/// What one statement of a [`Steps`] script did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepOutput {
    /// Its exit status (0–255: `$?` in `sh`).
    pub(crate) status: i32,
    /// Its stdout, whole.
    pub(crate) stdout: Vec<u8>,
    /// Its stderr, as kept: only a beginning when `stderr_truncated`.
    pub(crate) stderr: Vec<u8>,
    /// Whether the call's stderr limit cut this statement's messages.
    pub(crate) stderr_truncated: bool,
}

impl StepOutput {
    /// Whether the statement exited 0.
    pub(crate) fn success(&self) -> bool {
        self.status == 0
    }

    /// stdout without the one newline that ends it, as text, when it is
    /// valid UTF-8.
    pub(crate) fn line(&self) -> Option<&str> {
        let bytes = self.stdout.strip_suffix(b"\n").unwrap_or(&self.stdout);
        std::str::from_utf8(bytes).ok()
    }

    /// Whether stderr holds `phrase`.
    ///
    /// # Arguments
    ///
    /// * `phrase` - The text looked for
    pub(crate) fn stderr_has(&self, phrase: &str) -> bool {
        let phrase = phrase.as_bytes();
        self.stderr
            .windows(phrase.len().max(1))
            .any(|window| window == phrase)
    }
}

/// Why a [`Steps`] script's output cannot be split into its statements.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StepsError {
    /// The call has no complete payload ([`Execution::payload`]).
    #[error("{0}")]
    Payload(PayloadError),
    /// Statement `step`'s stdout marker is missing: the script stopped
    /// before it ended.
    #[error("the output of step {step} is incomplete")]
    Incomplete {
        /// The statement, from 0.
        step: usize,
    },
    /// Something follows the last marker, or a marker is malformed.
    #[error("the output of step {step} is malformed")]
    Malformed {
        /// The statement, from 0.
        step: usize,
    },
}

/// Split a [`Steps`] call's output into its statements' outputs.
///
/// # Arguments
///
/// * `execution` - The call, which ran a script of `count` steps
/// * `nonce` - The script's nonce
/// * `count` - How many statements the script had ([`Steps::len`])
///
/// # Errors
///
/// [`StepsError`] when the call did not run to its end, its stdout was cut,
/// or a marker is missing or malformed.
pub(crate) fn split(
    execution: &Execution,
    nonce: &Nonce,
    count: usize,
) -> Result<Vec<StepOutput>, StepsError> {
    let payload = execution.payload().map_err(StepsError::Payload)?;
    let stdouts = split_stdout(payload, nonce, count)?;
    let stderrs = split_stderr(execution.stderr(), nonce, count)?;
    Ok(stdouts
        .into_iter()
        .zip(stderrs)
        .map(
            |((status, stdout), (stderr, stderr_truncated))| StepOutput {
                status,
                stdout,
                stderr,
                stderr_truncated,
            },
        )
        .collect())
}

/// Statement statuses and stdouts, from the payload after the nonce line.
///
/// # Arguments
///
/// * `payload` - stdout after the frame's nonce line
/// * `nonce` - The call's nonce
/// * `count` - How many statements
fn split_stdout(
    payload: &[u8],
    nonce: &Nonce,
    count: usize,
) -> Result<Vec<(i32, Vec<u8>)>, StepsError> {
    let mut rest = payload;
    let mut steps = Vec::with_capacity(count);
    for step in 0..count {
        let (status, output, after) = stdout_step(rest, nonce, step)?;
        steps.push((status, output.to_vec()));
        rest = after;
    }
    if !rest.is_empty() {
        return Err(StepsError::Malformed {
            step: count.saturating_sub(1),
        });
    }
    Ok(steps)
}

/// Statement `step`'s status and stdout at the start of `rest`, and what
/// follows its marker line.
///
/// # Arguments
///
/// * `rest` - stdout from the statement's first byte
/// * `nonce` - The call's nonce
/// * `step` - The statement, from 0
fn stdout_step<'a>(
    rest: &'a [u8],
    nonce: &Nonce,
    step: usize,
) -> Result<(i32, &'a [u8], &'a [u8]), StepsError> {
    let marker = format!("\n{nonce} {step} ");
    let at = find(rest, marker.as_bytes()).ok_or(StepsError::Incomplete { step })?;
    let (output, after) = rest.split_at(at);
    let after = after.get(marker.len()..).unwrap_or_default();
    let end = after
        .iter()
        .position(|&byte| byte == b'\n')
        .ok_or(StepsError::Incomplete { step })?;
    let status = std::str::from_utf8(after.get(..end).unwrap_or_default())
        .ok()
        .and_then(|digits| digits.parse::<i32>().ok())
        .filter(|status| (0..=255).contains(status))
        .ok_or(StepsError::Malformed { step })?;
    Ok((status, output, after.get(end + 1..).unwrap_or_default()))
}

/// Statement stderrs: after the start line, each ended by its marker. A
/// stderr cut by its limit leaves the statements after the cut with what
/// was kept (possibly nothing), flagged truncated.
///
/// # Arguments
///
/// * `stderr` - The call's stderr, as kept
/// * `nonce` - The call's nonce
/// * `count` - How many statements
fn split_stderr(
    stderr: &Captured,
    nonce: &Nonce,
    count: usize,
) -> Result<Vec<(Vec<u8>, bool)>, StepsError> {
    let truncated = stderr.is_truncated();
    let Some(mut rest) = after_nonce_line(stderr.bytes(), nonce) else {
        if truncated {
            return Ok(vec![(Vec::new(), true); count]);
        }
        return Err(StepsError::Incomplete { step: 0 });
    };
    let mut steps = Vec::with_capacity(count);
    for step in 0..count {
        let marker = format!("\n{nonce} {step}\n");
        match find(rest, marker.as_bytes()) {
            Some(at) => {
                steps.push((rest.get(..at).unwrap_or_default().to_vec(), false));
                rest = rest.get(at + marker.len()..).unwrap_or_default();
            }
            None if truncated => {
                steps.push((rest.to_vec(), true));
                rest = &[];
            }
            None => return Err(StepsError::Incomplete { step }),
        }
    }
    Ok(steps)
}

/// The first position of `needle` in `haystack`.
///
/// # Arguments
///
/// * `haystack` - The bytes searched
/// * `needle` - The bytes looked for (not empty)
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::remote::quote::Word;
    use crate::remote::transport::Exit;
    use crate::remote::{Identity, RootAccess, Script};

    /// A fixed nonce.
    const NONCE: &str = "apvm-0123456789abcdef0123456789abcdef";

    /// The fixed nonce.
    fn nonce() -> Nonce {
        Nonce::parse(NONCE).unwrap()
    }

    /// `id -u` as the login user.
    fn id() -> Statement {
        Statement::as_identity(&Identity::Login, "id", vec![Word::lit("-u")])
    }

    /// An execution of the fixed nonce with `stdout` (after the nonce
    /// line) and `stderr`.
    ///
    /// # Arguments
    ///
    /// * `payload` - stdout after the nonce line
    /// * `stderr` - stderr
    /// * `stderr_truncated` - Whether stderr was cut
    fn execution(payload: &str, stderr: &str, stderr_truncated: bool) -> Execution {
        Execution::new(
            nonce(),
            Exit::Status(0),
            Captured::whole(format!("motd\n\n{NONCE}\n{payload}").into_bytes()),
            Captured::new(stderr.as_bytes().to_vec(), stderr_truncated),
            true,
            Duration::from_millis(1),
        )
    }

    #[test]
    fn steps_render_every_statement_with_its_markers() {
        let steps = Steps::new(
            id(),
            [Statement::as_root(
                RootAccess::Sudo,
                "stat",
                vec![Word::checked("/a b")],
            )],
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps.render(&nonce()),
            format!(
                "{{ printf '\\n%s\\n' '{NONCE}' >&2\n\
                 id -u </dev/null; printf '\\n{NONCE} 0 %s\\n' \"$?\"; printf '\\n{NONCE} 0\\n' >&2\n\
                 sudo -n -u root -- stat '/a b' </dev/null; printf '\\n{NONCE} 1 %s\\n' \"$?\"; \
                 printf '\\n{NONCE} 1\\n' >&2\n}}"
            )
        );
        let script = Script::steps(nonce(), &steps);
        assert!(
            script.text().contains("cd / && { printf"),
            "{}",
            script.text()
        );
    }

    #[test]
    fn outputs_split_by_their_markers() {
        let payload = format!("1000\n\n{NONCE} 0 0\nno newline\n{NONCE} 1 1\n\n{NONCE} 2 127\n");
        let stderr = format!(
            "noise\n\n{NONCE}\n\n{NONCE} 0\nwarn\n\n{NONCE} 1\nsh: x: not found\n\n{NONCE} 2\n"
        );
        let steps = split(&execution(&payload, &stderr, false), &nonce(), 3).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!((steps[0].status, steps[0].line()), (0, Some("1000")));
        assert_eq!(steps[0].stdout, b"1000\n");
        assert!(steps[0].stderr.is_empty() && steps[0].success());
        assert_eq!(
            (steps[1].status, &steps[1].stdout[..]),
            (1, &b"no newline"[..])
        );
        assert_eq!(steps[1].stderr, b"warn\n");
        assert_eq!((steps[2].status, &steps[2].stdout[..]), (127, &b""[..]));
        assert!(steps[2].stderr_has("not found") && !steps[2].stderr_truncated);
    }

    #[test]
    fn missing_or_malformed_markers_are_refused() {
        let start = format!("\n{NONCE}\n");
        let cases = [
            (
                format!("1\n\n{NONCE} 0 0\n"),
                StepsError::Incomplete { step: 1 },
            ),
            (
                format!("1\n\n{NONCE} 0 0"),
                StepsError::Incomplete { step: 0 },
            ),
            (
                format!("\n{NONCE} 0 x\n\n{NONCE} 1 0\n"),
                StepsError::Malformed { step: 0 },
            ),
            (
                format!("\n{NONCE} 0 256\n\n{NONCE} 1 0\n"),
                StepsError::Malformed { step: 0 },
            ),
            (
                format!("\n{NONCE} 0 0\n\n{NONCE} 1 0\nextra"),
                StepsError::Malformed { step: 1 },
            ),
        ];
        let stderr = format!("{start}\n{NONCE} 0\n\n{NONCE} 1\n");
        for (payload, expected) in cases {
            assert_eq!(
                split(&execution(&payload, &stderr, false), &nonce(), 2),
                Err(expected),
                "{payload:?}"
            );
        }
        let payload = format!("\n{NONCE} 0 0\n\n{NONCE} 1 0\n");
        // stderr without its start line, or without a step's marker.
        for stderr in [format!("\n{NONCE} 0\n"), format!("{start}\n{NONCE} 0\n")] {
            assert!(matches!(
                split(&execution(&payload, &stderr, false), &nonce(), 2),
                Err(StepsError::Incomplete { .. })
            ));
        }
    }

    #[test]
    fn a_cut_stderr_keeps_what_it_has() {
        let payload = format!("\n{NONCE} 0 0\n\n{NONCE} 1 1\n");
        let steps = split(
            &execution(&payload, &format!("\n{NONCE}\n\n{NONCE} 0\nxxx"), true),
            &nonce(),
            2,
        )
        .unwrap();
        assert!(!steps[0].stderr_truncated);
        assert_eq!(
            (&steps[1].stderr[..], steps[1].stderr_truncated),
            (&b"xxx"[..], true)
        );
        let steps = split(&execution(&payload, "noise", true), &nonce(), 2).unwrap();
        assert!(
            steps
                .iter()
                .all(|step| step.stderr_truncated && step.stderr.is_empty())
        );
    }

    #[test]
    fn an_interrupted_call_has_no_steps() {
        let interrupted = Execution::new(
            nonce(),
            Exit::TimedOut,
            Captured::whole(format!("\n{NONCE}\n").into_bytes()),
            Captured::default(),
            true,
            Duration::from_millis(1),
        );
        assert!(matches!(
            split(&interrupted, &nonce(), 1),
            Err(StepsError::Payload(PayloadError::Interrupted { .. }))
        ));
    }
}
