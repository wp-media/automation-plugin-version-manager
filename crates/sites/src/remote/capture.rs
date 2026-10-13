//! Bounded capture of a child's output, and unframing (plan §6.1 items 2
//! and 6).
//!
//! - **Capture:** stdout keeps at most [`STDOUT_LIMIT`] bytes and stderr
//!   [`STDERR_LIMIT`]; what goes beyond is read and dropped, and the
//!   [`Captured`] output says it was truncated. Reading never stops at the
//!   limit, so a chatty child never blocks on a full pipe.
//! - **Unframing:** only stdout after the first line exactly equal to the
//!   call's [`Nonce`] is the command's output ([`unframe`]). What comes
//!   before is login noise (`.bashrc`, MOTD). No such line is a failure
//!   ([`PayloadError::NonceMissing`]), never "no output": a login shell
//!   whose startup files ate the start of the script, or a script that
//!   never ran, is caught that way. A payload cut at the limit is a failure
//!   too ([`PayloadError::Truncated`]), never a shorter output.

use tokio::io::{AsyncRead, AsyncReadExt};

use super::Nonce;
use super::transport::Exit;

/// Most stdout bytes a call keeps (8 MiB).
pub const STDOUT_LIMIT: usize = 8 * 1024 * 1024;
/// Most stderr bytes a call keeps (1 MiB).
pub const STDERR_LIMIT: usize = 1024 * 1024;
/// Bytes read from a pipe at a time.
const CHUNK: usize = 64 * 1024;

/// What a call kept of one output stream: its first bytes, up to the
/// stream's limit, and whether more was dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    /// The bytes kept, in order.
    bytes: Vec<u8>,
    /// Whether the stream went on past the limit.
    truncated: bool,
}

impl Captured {
    /// What a transport kept of one stream: its first `bytes`, and whether
    /// the stream went on past them. For transports outside this crate
    /// (and fakes in their tests); this crate's limits are
    /// [`STDOUT_LIMIT`] and [`STDERR_LIMIT`].
    ///
    /// # Arguments
    ///
    /// * `bytes` - The bytes kept, in order
    /// * `truncated` - Whether more was read and dropped
    pub fn new(bytes: Vec<u8>, truncated: bool) -> Self {
        Self { bytes, truncated }
    }

    /// Output that was kept whole: `bytes`, not truncated.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Everything the stream carried
    pub fn whole(bytes: Vec<u8>) -> Self {
        Self::new(bytes, false)
    }

    /// The bytes kept (all of them, unless [`Self::is_truncated`]).
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether the stream carried more than the limit, so the bytes kept
    /// are only its beginning.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
}

/// Collects one stream into a [`Captured`] while it is read. The bytes
/// read so far stay here if reading is abandoned (a timeout, the grace
/// after the child exited), so a call still reports them.
#[derive(Debug)]
pub(crate) struct Capture {
    /// What is kept so far.
    captured: Captured,
    /// Most bytes kept.
    limit: usize,
}

impl Capture {
    /// An empty capture keeping at most `limit` bytes.
    ///
    /// # Arguments
    ///
    /// * `limit` - Most bytes kept
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            captured: Captured::default(),
            limit,
        }
    }

    /// Read `stream` to its end (nothing when there is none), keeping what
    /// fits. Cancel-safe: dropped mid-way, every byte read is kept.
    ///
    /// # Arguments
    ///
    /// * `stream` - The child's stdout or stderr
    ///
    /// # Errors
    ///
    /// When reading the pipe fails.
    pub(crate) async fn drain<R: AsyncRead + Unpin>(
        &mut self,
        stream: Option<&mut R>,
    ) -> std::io::Result<()> {
        let Some(stream) = stream else {
            return Ok(());
        };
        let mut chunk = vec![0; CHUNK];
        loop {
            // `read` is cancel-safe: no byte is lost if it is dropped.
            let read = stream.read(&mut chunk).await?;
            let Some(bytes) = chunk.get(..read).filter(|bytes| !bytes.is_empty()) else {
                return Ok(());
            };
            self.keep(bytes);
        }
    }

    /// Keep what fits of `bytes`, and note any excess.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Bytes just read
    fn keep(&mut self, bytes: &[u8]) {
        let room = self.limit.saturating_sub(self.captured.bytes.len());
        let (kept, dropped) = bytes.split_at(room.min(bytes.len()));
        self.captured.bytes.extend_from_slice(kept);
        self.captured.truncated |= !dropped.is_empty();
    }

    /// What was kept.
    pub(crate) fn finish(self) -> Captured {
        self.captured
    }
}

/// The output of a script: kept stdout after its nonce line, only when it
/// is whole (plan §6.1 item 2).
///
/// The nonce line is the first line (a run of bytes ended by `\n`, at the
/// start of stdout or after another `\n`) that is exactly the nonce. A
/// line with anything more (a `\r`, a space, a prefix) does not count, nor
/// does the nonce without its `\n` at the very end: the scripts print it
/// with `printf '\n%s\n'`, so anything else is not their output.
///
/// # Arguments
///
/// * `stdout` - The call's stdout, as kept
/// * `nonce` - The call's nonce
///
/// # Returns
///
/// The payload (possibly empty), complete.
///
/// # Errors
///
/// [`PayloadError::NonceMissing`] when no kept line equals the nonce;
/// [`PayloadError::Truncated`] when it does but stdout went past
/// [`STDOUT_LIMIT`], so the payload is cut: a parser must never take it for
/// the whole output.
pub fn unframe<'a>(stdout: &'a Captured, nonce: &Nonce) -> Result<&'a [u8], PayloadError> {
    let Some(payload) = after_nonce_line(stdout.bytes(), nonce) else {
        return Err(PayloadError::NonceMissing {
            stdout_truncated: stdout.is_truncated(),
        });
    };
    if stdout.is_truncated() {
        return Err(PayloadError::Truncated);
    }
    Ok(payload)
}

/// The bytes after the first line that is exactly `nonce` (see
/// [`unframe`]), if any.
///
/// # Arguments
///
/// * `stdout` - Output bytes
/// * `nonce` - The call's nonce
fn after_nonce_line<'a>(stdout: &'a [u8], nonce: &Nonce) -> Option<&'a [u8]> {
    let wanted = nonce.as_str().as_bytes();
    let mut end = 0;
    for line in stdout.split_inclusive(|&byte| byte == b'\n') {
        end += line.len();
        if line.strip_suffix(b"\n") == Some(wanted) {
            return stdout.get(end..);
        }
    }
    None
}

/// Why a call has no usable payload ([`unframe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PayloadError {
    /// No kept stdout line equals the nonce, so the script did not run as
    /// sent: startup files of the login shell read part of it, the remote
    /// `sh` or the transport failed, or (when `stdout_truncated`) login
    /// noise filled the whole stdout limit before the nonce. Never "no
    /// output"; after a mutation the command may still have run, so the
    /// state must be re-read.
    NonceMissing {
        /// Whether stdout went past its limit.
        stdout_truncated: bool,
    },
    /// The payload went past [`STDOUT_LIMIT`]: only its beginning was
    /// kept (see [`Captured::bytes`]), so it cannot be parsed as a whole.
    Truncated,
    /// The call did not run to its end ([`Exit::TransportFailed`],
    /// [`Exit::Killed`] or [`Exit::TimedOut`]): what its script printed is
    /// at most a beginning. Read it from
    /// [`Execution::stdout`](super::Execution::stdout) for messages, never
    /// as the output. After a mutation, re-read the state.
    Interrupted {
        /// How the call ended.
        exit: Exit,
    },
}

impl std::fmt::Display for PayloadError {
    /// A sentence for messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonceMissing {
                stdout_truncated: false,
            } => f.write_str(
                "the output has no line equal to the call's nonce: the script did not run as sent",
            ),
            Self::NonceMissing {
                stdout_truncated: true,
            } => f.write_str(
                "the output has no line equal to the call's nonce within its first 8 MiB: \
                 login noise filled the limit, or the script did not run as sent",
            ),
            Self::Truncated => f.write_str("the output went past 8 MiB and was cut"),
            Self::Interrupted { exit } => {
                write!(
                    f,
                    "the call did not run to its end ({exit}): its output is incomplete"
                )
            }
        }
    }
}

impl std::error::Error for PayloadError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed nonce.
    fn nonce() -> Nonce {
        Nonce::parse("apvm-0123456789abcdef0123456789abcdef").unwrap()
    }

    /// Whole stdout: `text` with every `N` replaced by the nonce.
    ///
    /// # Arguments
    ///
    /// * `text` - Output with `N` where the nonce goes
    fn with_nonce(text: &str) -> Captured {
        Captured::whole(with_text(text).into_bytes())
    }

    /// The error for a missing nonce line in untruncated stdout.
    const MISSING: PayloadError = PayloadError::NonceMissing {
        stdout_truncated: false,
    };

    /// What `drain` keeps of `input` under `limit`.
    ///
    /// # Arguments
    ///
    /// * `input` - The stream's bytes
    /// * `limit` - Most bytes kept
    async fn drained(input: &[u8], limit: usize) -> Captured {
        let mut capture = Capture::new(limit);
        let mut stream = input;
        capture.drain(Some(&mut stream)).await.unwrap();
        assert!(stream.is_empty(), "the stream is read to its end");
        capture.finish()
    }

    #[tokio::test]
    async fn output_up_to_the_limit_is_kept_whole() {
        assert_eq!(drained(b"", 4).await, Captured::whole(Vec::new()));
        assert_eq!(drained(b"abc", 4).await, Captured::whole(b"abc".to_vec()));
        // Exactly at the limit: nothing dropped.
        assert_eq!(drained(b"abcd", 4).await, Captured::whole(b"abcd".to_vec()));
    }

    #[tokio::test]
    async fn output_past_the_limit_is_cut_flagged_and_still_read() {
        let one_over = drained(b"abcde", 4).await;
        assert_eq!(one_over.bytes(), b"abcd");
        assert!(one_over.is_truncated());
        // Far past the limit, across many chunks: still read to the end.
        let big = vec![7u8; 5 * CHUNK + 3];
        let captured = drained(&big, CHUNK + 1).await;
        assert_eq!(captured.bytes(), &big[..CHUNK + 1]);
        assert!(captured.is_truncated());
        // A zero limit keeps nothing but still notes the output.
        let none = drained(b"x", 0).await;
        assert!(none.bytes().is_empty() && none.is_truncated());
        assert!(!drained(b"", 0).await.is_truncated());
    }

    #[test]
    fn keeping_chunk_by_chunk_stops_exactly_at_the_limit() {
        let mut capture = Capture::new(5);
        capture.keep(b"ab");
        capture.keep(b"cde");
        assert!(!capture.captured.is_truncated(), "exactly the limit");
        capture.keep(b"");
        assert!(
            !capture.captured.is_truncated(),
            "an empty chunk is no excess"
        );
        capture.keep(b"f");
        capture.keep(b"gh");
        let captured = capture.finish();
        assert_eq!(captured.bytes(), b"abcde");
        assert!(captured.is_truncated());
    }

    #[tokio::test]
    async fn a_missing_stream_is_empty() {
        let mut capture = Capture::new(4);
        capture
            .drain(None::<&mut &[u8]>)
            .await
            .expect("nothing to read");
        assert_eq!(capture.finish(), Captured::default());
    }

    #[tokio::test]
    async fn bytes_read_before_reading_is_abandoned_are_kept() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut writer, b"partial")
            .await
            .unwrap();
        let mut capture = Capture::new(STDERR_LIMIT);
        // The writer stays open: the drain would wait forever.
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            capture.drain(Some(&mut reader)),
        )
        .await;
        assert!(waited.is_err(), "still draining when abandoned");
        assert_eq!(capture.finish().bytes(), b"partial");
    }

    #[tokio::test]
    async fn read_errors_are_reported() {
        /// A stream whose every read fails.
        struct Failing;

        impl AsyncRead for Failing {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("broken")))
            }
        }

        let mut capture = Capture::new(4);
        let error = capture.drain(Some(&mut Failing)).await.unwrap_err();
        assert_eq!(error.to_string(), "broken");
    }

    #[test]
    fn the_payload_follows_the_nonce_line() {
        let stdout = with_nonce("N\n[1,2]\n");
        assert_eq!(unframe(&stdout, &nonce()), Ok(&b"[1,2]\n"[..]));
        // The payload is kept byte for byte: CRLF, no trailing newline.
        let stdout = with_nonce("N\na\r\nb");
        assert_eq!(unframe(&stdout, &nonce()), Ok(&b"a\r\nb"[..]));
        // As the scripts print it: an empty line first.
        let stdout = with_nonce("\nN\nout");
        assert_eq!(unframe(&stdout, &nonce()), Ok(&b"out"[..]));
    }

    #[test]
    fn noise_before_the_nonce_line_is_skipped() {
        let stdout = with_nonce("Welcome to Debian\n\nlast login: today\nN\npayload\n");
        assert_eq!(unframe(&stdout, &nonce()), Ok(&b"payload\n"[..]));
        // Noise without a final newline: the scripts' leading newline
        // ends its line.
        let stdout = with_nonce("Welcome\nN\npayload\n");
        assert_eq!(unframe(&stdout, &nonce()), Ok(&b"payload\n"[..]));
    }

    #[test]
    fn an_empty_payload_is_not_a_missing_nonce() {
        // Nonce line at the very end: no output, which is fine.
        assert_eq!(unframe(&with_nonce("N\n"), &nonce()), Ok(&b""[..]));
        assert_eq!(unframe(&with_nonce("noise\nN\n"), &nonce()), Ok(&b""[..]));
    }

    #[test]
    fn only_the_first_nonce_line_frames() {
        // A payload may repeat the nonce (an echo of the script): it stays.
        let stdout = with_nonce("N\nN\nrest");
        let rest = with_text("N\nrest");
        assert_eq!(unframe(&stdout, &nonce()), Ok(rest.as_bytes()));
    }

    #[test]
    fn lines_that_only_look_like_the_nonce_do_not_frame() {
        let other = "apvm-ffffffffffffffffffffffffffffffff";
        for stdout in [
            String::new(),
            "\n\n".into(),
            "payload without a nonce\n".into(),
            format!("{other}\n"),
            with_text("N\r\n"),
            with_text(" N\n"),
            with_text("N \n"),
            with_text("xN\n"),
            with_text("Nx\n"),
            // Cut: the nonce without its newline at the very end.
            with_text("N"),
            with_text("noise\nN"),
            // Text glued onto the nonce's line (the scripts prevent this
            // with their leading newline).
            with_text("motd without newlineN\npayload\n"),
            // The nonce's prefix or a part of it.
            "apvm-\n".into(),
            with_text("N")[..20].to_string() + "\n",
        ] {
            let stdout_bytes = Captured::whole(stdout.clone().into_bytes());
            assert_eq!(unframe(&stdout_bytes, &nonce()), Err(MISSING), "{stdout:?}");
        }
    }

    #[test]
    fn a_cut_stdout_never_yields_a_payload() {
        // The nonce was kept, but the payload after it is only a beginning.
        let cut = Captured::new(with_text("N\n[1,2,").into_bytes(), true);
        assert_eq!(unframe(&cut, &nonce()), Err(PayloadError::Truncated));
        // Noise filled the limit: the nonce never made it.
        let noise = Captured::new(b"motd\n".repeat(4), true);
        assert_eq!(
            unframe(&noise, &nonce()),
            Err(PayloadError::NonceMissing {
                stdout_truncated: true
            })
        );
    }

    /// `text` with every `N` replaced by the nonce, as a string.
    ///
    /// # Arguments
    ///
    /// * `text` - Output with `N` where the nonce goes
    fn with_text(text: &str) -> String {
        text.replace('N', nonce().as_str())
    }

    #[test]
    fn payload_errors_explain_themselves() {
        let missing = MISSING.to_string();
        assert!(missing.contains("nonce"), "{missing}");
        assert!(missing.contains("did not run as sent"), "{missing}");
        let flooded = PayloadError::NonceMissing {
            stdout_truncated: true,
        }
        .to_string();
        assert!(flooded.contains("within its first 8 MiB"), "{flooded}");
        let cut = PayloadError::Truncated.to_string();
        assert!(cut.contains("was cut"), "{cut}");
    }

    #[test]
    fn captured_output_is_built_as_given() {
        let captured = Captured::new(b"ab".to_vec(), true);
        assert_eq!(
            (captured.bytes(), captured.is_truncated()),
            (&b"ab"[..], true)
        );
        assert_eq!(
            Captured::whole(b"x".to_vec()),
            Captured::new(b"x".to_vec(), false)
        );
    }

    #[test]
    fn the_limits_are_the_plans() {
        assert_eq!(STDOUT_LIMIT, 8_388_608);
        assert_eq!(STDERR_LIMIT, 1_048_576);
    }
}
