//! Yes/no confirmation prompts, shared by every command that asks before it
//! changes something (`uninstall`, `cache clear`, `server remove`, …).
//!
//! The prompt goes to stderr, so stdout keeps only the command's output.
//! Only `y` or `yes` (any case, surrounding whitespace ignored) confirms:
//! an empty line, end of input or anything else is a refusal, so a stray
//! Enter never confirms.

use std::io::{self, BufRead, IsTerminal, Write};

/// How a prompt words its I/O failures: what failed, in front of the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wording {
    /// Writing the question failed (`Failed to flush stderr`).
    pub write: &'static str,
    /// Reading the answer failed (`Failed to read user input`).
    pub read: &'static str,
}

/// The wording of [`confirm`] (`uninstall` and the inventory commands).
pub const WORDING: Wording = Wording {
    write: "Failed to flush stderr",
    read: "Failed to read user input",
};

/// Ask `prompt` on stderr and read the answer from stdin, whatever stdin
/// is (a terminal or a pipe).
///
/// # Arguments
///
/// * `prompt` - The question, including its `[y/N]` hint
///
/// # Returns
///
/// Whether the answer is `y` or `yes`.
///
/// # Errors
///
/// An I/O error when the prompt cannot be written or the answer read; its
/// message says which ([`WORDING`]: `Failed to flush stderr: …`,
/// `Failed to read user input: …`).
pub fn confirm(prompt: &str) -> io::Result<bool> {
    confirm_worded(prompt, WORDING)
}

/// [`confirm`] with the I/O failures worded by `wording` (for a command
/// whose messages predate [`WORDING`]).
///
/// # Arguments
///
/// * `prompt` - The question, including its `[y/N]` hint
/// * `wording` - What the error messages start with
///
/// # Errors
///
/// As [`confirm`], worded by `wording`.
pub fn confirm_worded(prompt: &str, wording: Wording) -> io::Result<bool> {
    let line = ask(prompt, &mut io::stdin().lock(), &mut io::stderr(), wording)?;
    Ok(is_affirmative(&line))
}

/// [`confirm`], but only when stdin is a terminal: a script or a pipe
/// cannot be asked, so it must confirm up front (`-y`).
///
/// When the answer ends without a newline (Ctrl-D), a newline is written
/// after it, so what follows starts on its own line rather than after the
/// prompt.
///
/// # Arguments
///
/// * `prompt` - The question, including its `[y/N]` hint
///
/// # Returns
///
/// `None` when stdin is not a terminal (nothing was asked), else whether
/// the answer is `y` or `yes`.
///
/// # Errors
///
/// Those of [`confirm`].
pub fn confirm_on_terminal(prompt: &str) -> io::Result<Option<bool>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let answer = ask_ending_line(prompt, &mut io::stdin().lock(), &mut io::stderr())?;
    Ok(Some(answer))
}

/// [`ask`] with [`WORDING`], then end the line when the answer did not
/// (end of input): the core of [`confirm_on_terminal`], for tests.
///
/// # Arguments
///
/// * `prompt` - The question
/// * `input` - Where the answer is read from
/// * `output` - Where the question (and the closing newline) is written
///
/// # Returns
///
/// Whether the answer is `y` or `yes`.
///
/// # Errors
///
/// Those of [`ask`]. Writing the closing newline is best effort: the
/// answer is already known, and a refusal must stay one.
fn ask_ending_line(
    prompt: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    let line = ask(prompt, input, output, WORDING)?;
    if !line.ends_with('\n') {
        let _ = output.write_all(b"\n").and_then(|()| output.flush());
    }
    Ok(is_affirmative(&line))
}

/// Write `prompt` to `output`, then read one line from `input`. The pure
/// core of every prompt, for tests.
///
/// # Arguments
///
/// * `prompt` - The question
/// * `input` - Where the answer is read from
/// * `output` - Where the question is written
/// * `wording` - What the error messages start with
///
/// # Returns
///
/// The line read, with its newline; without one at end of input (empty
/// when nothing was typed).
///
/// # Errors
///
/// An I/O error, with `wording` in front, when writing, flushing or
/// reading fails.
fn ask(
    prompt: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
    wording: Wording,
) -> io::Result<String> {
    output
        .write_all(prompt.as_bytes())
        .and_then(|()| output.flush())
        .map_err(|e| with_context(wording.write, e))?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .map_err(|e| with_context(wording.read, e))?;
    Ok(answer)
}

/// `error` with `context` in front of its message, keeping its kind.
fn with_context(context: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

/// Whether a raw confirmation answer means "yes": exactly `y` or `yes`,
/// ignoring surrounding whitespace and case. Anything else — including an
/// empty line — is a refusal.
///
/// # Arguments
///
/// * `input` - The line the user typed, newline included or not
pub fn is_affirmative(input: &str) -> bool {
    let answer = input.trim().to_lowercase();
    answer == "y" || answer == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose every write fails.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A reader whose every read fails.
    struct Unreadable;

    impl io::Read for Unreadable {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::InvalidData, "bad bytes"))
        }
    }

    /// Run [`ask`] on `answer`, returning whether it confirms and what was
    /// written.
    fn ask_with(answer: &str) -> (io::Result<bool>, String) {
        let mut output = Vec::new();
        let result = ask("Go? [y/N] ", &mut answer.as_bytes(), &mut output, WORDING);
        let confirmed = result.map(|line| is_affirmative(&line));
        (confirmed, String::from_utf8_lossy(&output).into_owned())
    }

    /// Run [`ask_ending_line`] on `answer`, returning the result and what
    /// was written.
    fn ask_ending_line_with(answer: &str) -> (io::Result<bool>, String) {
        let mut output = Vec::new();
        let result = ask_ending_line("Go? ", &mut answer.as_bytes(), &mut output);
        (result, String::from_utf8_lossy(&output).into_owned())
    }

    #[test]
    fn ask_writes_the_prompt_and_reads_one_line() {
        let (result, written) = ask_with("yes\nno\n");
        assert!(result.expect("answers"));
        assert_eq!(written, "Go? [y/N] ");
    }

    #[test]
    fn ask_refuses_anything_but_yes_and_end_of_input() {
        for answer in ["", "\n", "n\n", "nope\ny\n", " maybe "] {
            let (result, _) = ask_with(answer);
            assert!(!result.expect("answers"), "{answer:?} must not confirm");
        }
    }

    #[test]
    fn ask_reports_a_failed_prompt_with_context() {
        let err = ask("Go?", &mut "y\n".as_bytes(), &mut Broken, WORDING).expect_err("write fails");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(err.to_string(), "Failed to flush stderr: closed");
    }

    #[test]
    fn ask_reports_a_failed_read_with_context() {
        let mut input = io::BufReader::new(Unreadable);
        let err = ask("Go?", &mut input, &mut Vec::new(), WORDING).expect_err("read fails");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "Failed to read user input: bad bytes");
    }

    #[test]
    fn ask_words_failures_as_the_caller_asks() {
        let wording = Wording {
            write: "failed to flush stderr",
            read: "failed to read input",
        };
        let err = ask("Go?", &mut "y\n".as_bytes(), &mut Broken, wording).expect_err("write");
        assert_eq!(err.to_string(), "failed to flush stderr: closed");
        let mut input = io::BufReader::new(Unreadable);
        let err = ask("Go?", &mut input, &mut Vec::new(), wording).expect_err("read");
        assert_eq!(err.to_string(), "failed to read input: bad bytes");
    }

    #[test]
    fn ask_ending_line_ends_the_line_only_at_end_of_input() {
        // Ctrl-D on an empty line, or after typing: the terminal echoes no
        // newline, so the prompt's line is ended for what follows.
        for (answer, confirmed) in [("", false), ("y", true), ("n", false)] {
            let (result, written) = ask_ending_line_with(answer);
            assert_eq!(result.expect("answers"), confirmed, "{answer:?}");
            assert_eq!(written, "Go? \n", "{answer:?}");
        }
        // A typed line already ended it (the terminal echoed the Enter).
        for (answer, confirmed) in [("y\n", true), ("\n", false)] {
            let (result, written) = ask_ending_line_with(answer);
            assert_eq!(result.expect("answers"), confirmed, "{answer:?}");
            assert_eq!(written, "Go? ", "{answer:?}");
        }
    }

    #[test]
    fn ask_ending_line_keeps_the_refusal_when_the_newline_cannot_be_written() {
        /// Accepts the prompt, then fails every later write.
        struct FailsAfterPrompt(bool);
        impl Write for FailsAfterPrompt {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if std::mem::replace(&mut self.0, true) {
                    return Err(io::Error::other("closed"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = FailsAfterPrompt(false);
        let result = ask_ending_line("Go? ", &mut "".as_bytes(), &mut output);
        assert!(!result.expect("a refusal, not an error"));
    }

    #[test]
    fn is_affirmative_accepts_only_y_and_yes() {
        for yes in ["y", "Y", "yes", "YES", "  Yes \n", "y\r\n"] {
            assert!(is_affirmative(yes), "{yes:?} must confirm");
        }
        for no in ["", "\n", "n", "no", "ye", "yess", "y es", "maybe", "1"] {
            assert!(!is_affirmative(no), "{no:?} must not confirm");
        }
    }

    #[test]
    fn confirm_on_terminal_never_reads_a_pipe() {
        // `cargo test` runs with stdin redirected (not a terminal) in CI and
        // under the harness, so nothing is asked and nothing blocks.
        if io::stdin().is_terminal() {
            return;
        }
        assert!(matches!(confirm_on_terminal("Go? "), Ok(None)));
    }
}
