//! Quiet exit when an output stream is closed early.
//!
//! `apvm list | head -1` closes the pipe while apvm may still be writing.
//! Rust ignores `SIGPIPE`, so the next `println!` fails with `EPIPE` and std
//! panics with "failed printing to stdout: Broken pipe (os error 32)" — a
//! scary message for a normal shell idiom.
//!
//! [`install`] silences exactly that panic, and [`run`] lets it unwind —
//! so every destructor runs (build workspaces, temp files) — before turning
//! it into [`EXIT_CODE`], the status a shell reports for a process killed by
//! `SIGPIPE`. Every other panic keeps the default report and outcome.
//!
//! Restoring the default `SIGPIPE` action instead would kill the process on
//! *any* closed pipe or socket — including a dropped HTTPS connection mid
//! download — which Rust's runtime deliberately avoids; it would also skip
//! all cleanup.

use std::any::Any;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;

/// Exit status after a closed stdout/stderr: 128 + `SIGPIPE` (13), what a
/// shell reports for a process killed by `SIGPIPE` (Windows has no such
/// signal; the same code is used there). A pipeline without `pipefail`
/// reports its last command's status, so `apvm list | head` succeeds; with
/// `pipefail` the cut-off is visible, as with coreutils.
pub const EXIT_CODE: u8 = 141;

/// Install the panic hook (call once, first thing in `main`).
///
/// A broken stdout/stderr pipe panic prints nothing: the reader left on
/// purpose, and the stream is closed anyway. Every other panic is forwarded
/// to the previously installed (default) hook unchanged.
pub fn install() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if !info.payload_as_str().is_some_and(is_broken_pipe_message) {
            default_hook(info);
        }
    }));
}

/// Run `body`, mapping a broken-pipe panic to [`EXIT_CODE`] once it has
/// unwound (so destructors ran). Any other panic resumes unwinding.
///
/// # Arguments
///
/// * `body` - The whole command (runtime included)
///
/// # Returns
///
/// `body`'s exit code, or [`EXIT_CODE`] when it panicked on a closed
/// output stream.
pub fn run(body: impl FnOnce() -> ExitCode) -> ExitCode {
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(payload) if is_broken_pipe_payload(payload.as_ref()) => ExitCode::from(EXIT_CODE),
        Err(payload) => panic::resume_unwind(payload),
    }
}

/// [`is_broken_pipe_message`] for a caught panic payload (`String` or
/// `&'static str`, as std's panics carry).
///
/// # Arguments
///
/// * `payload` - The payload `catch_unwind` returned
///
/// # Returns
///
/// Whether it is std's panic for printing to a closed stdout/stderr.
fn is_broken_pipe_payload(payload: &(dyn Any + Send)) -> bool {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied());
    message.is_some_and(is_broken_pipe_message)
}

/// Whether `message` is std's panic for printing to a closed stdout/stderr.
///
/// std reports `failed printing to <stream>: <io error>`; the OS error code
/// at its end is classified by std itself, so this works for `EPIPE` on Unix
/// and `ERROR_NO_DATA` / `ERROR_BROKEN_PIPE` on Windows.
///
/// # Arguments
///
/// * `message` - The panic message
///
/// # Returns
///
/// `true` only for a print to stdout/stderr that failed with a broken pipe.
fn is_broken_pipe_message(message: &str) -> bool {
    let printing = ["stdout", "stderr"]
        .iter()
        .any(|stream| message.starts_with(&format!("failed printing to {stream}: ")));
    printing
        && os_error_code(message).is_some_and(|code| {
            io::Error::from_raw_os_error(code).kind() == io::ErrorKind::BrokenPipe
        })
}

/// The `N` of a trailing `(os error N)`, as `io::Error`'s Display writes it.
///
/// # Arguments
///
/// * `message` - Text ending in an OS error description
///
/// # Returns
///
/// The OS error code, or `None` when the text does not end with one.
fn os_error_code(message: &str) -> Option<i32> {
    const MARKER: &str = "(os error ";
    let start = message.rfind(MARKER)? + MARKER.len();
    message[start..].strip_suffix(')')?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// std's panic message for a failed print of `error` to `stream`.
    fn print_failure(stream: &str, error: io::Error) -> String {
        format!("failed printing to {stream}: {error}")
    }

    /// An OS error code that std classifies as `BrokenPipe` on this platform.
    #[cfg(unix)]
    const BROKEN_PIPE: i32 = 32; // EPIPE
    #[cfg(windows)]
    const BROKEN_PIPE: i32 = 232; // ERROR_NO_DATA ("The pipe is being closed")

    /// Sets its flag when dropped — proves destructors ran during unwinding.
    struct SetOnDrop<'a>(&'a AtomicBool);

    impl Drop for SetOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn os_error_code_reads_the_trailing_code() {
        assert_eq!(os_error_code("Broken pipe (os error 32)"), Some(32));
        assert_eq!(os_error_code("x (os error 232)"), Some(232));
        assert_eq!(os_error_code("no code here"), None);
        assert_eq!(os_error_code("(os error abc)"), None);
        assert_eq!(os_error_code("(os error 32) trailing"), None);
    }

    #[test]
    fn broken_pipe_print_failures_are_recognized_on_both_streams() {
        let broken = || io::Error::from_raw_os_error(BROKEN_PIPE);
        assert!(is_broken_pipe_message(&print_failure("stdout", broken())));
        assert!(is_broken_pipe_message(&print_failure("stderr", broken())));
    }

    #[test]
    fn other_panics_are_left_to_the_default_hook() {
        // A different I/O failure while printing (disk full, ...).
        let other = io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 });
        assert!(!is_broken_pipe_message(&print_failure("stdout", other)));
        // A broken pipe somewhere else (e.g. a socket) is a real error.
        let socket = format!(
            "connection lost: {}",
            io::Error::from_raw_os_error(BROKEN_PIPE)
        );
        assert!(!is_broken_pipe_message(&socket));
        assert!(!is_broken_pipe_message("index out of bounds"));
    }

    #[test]
    fn run_maps_a_broken_pipe_panic_to_the_exit_code_after_cleanup() {
        // `process::exit` from the hook would skip this destructor — and,
        // in a real build, leave the temp workspace on disk.
        let dropped = AtomicBool::new(false);
        let code = run(|| {
            let _workspace = SetOnDrop(&dropped);
            panic::panic_any(print_failure(
                "stdout",
                io::Error::from_raw_os_error(BROKEN_PIPE),
            ))
        });
        assert_eq!(code, ExitCode::from(EXIT_CODE));
        assert!(dropped.load(Ordering::SeqCst), "destructors must run");
    }

    #[test]
    fn run_passes_exit_codes_through_and_resumes_other_panics() {
        assert_eq!(run(|| ExitCode::FAILURE), ExitCode::FAILURE);

        let other = panic::catch_unwind(|| run(|| panic!("a real bug")));
        let payload = other.expect_err("a non-pipe panic must keep unwinding");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"a real bug"));
    }
}
