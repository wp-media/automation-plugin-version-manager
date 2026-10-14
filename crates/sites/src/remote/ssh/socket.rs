//! A shared connection's control socket, seen directly (P3.4, audit round
//! 5): is a master behind it, live or stuck, and which sockets apvm may
//! remove.
//!
//! A live master writes its hello as soon as it accepts a connection to
//! its socket (`MUX_MSG_HELLO`; verified on 10.3p1: 25 bytes at once); a
//! stopped or wedged one accepts nothing, so a connection waits in the
//! listen queue and no byte comes; a socket nobody listens on refuses the
//! connection. apvm connects and reads, rather than running `ssh -O check`,
//! which first reads the whole ssh_config (a slow `Match exec` or a DNS
//! lookup for `CanonicalizeHostname` made a live master look stuck,
//! verified by audit round 5 on 10.3p1 and 8.9p1). Connecting and closing
//! does a master no harm (it drops the client).

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a live master may take to send its hello.
const HELLO_WAIT: Duration = Duration::from_secs(3);

/// What is behind a control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(unix),
    expect(
        dead_code,
        reason = "Windows does not multiplex: nothing is ever behind a socket"
    )
)]
pub(super) enum Probe {
    /// A live master: it sent its hello.
    Live,
    /// No master: no socket, or one nobody listens on (a stale socket),
    /// or one that closed without a word.
    Absent,
    /// A stuck master: the connection was accepted (queued), and nothing
    /// came within [`HELLO_WAIT`].
    Stuck,
}

/// Look behind `socket` (blocking at most [`HELLO_WAIT`], off the async
/// runtime's threads).
///
/// # Arguments
///
/// * `socket` - The expanded control path
pub(super) async fn probe(socket: &Path) -> Probe {
    let socket = socket.to_path_buf();
    tokio::task::spawn_blocking(move || probe_now(&socket, HELLO_WAIT))
        .await
        .unwrap_or(Probe::Absent)
}

/// Connect to `socket` and wait up to `wait` for a byte.
///
/// # Arguments
///
/// * `socket` - The expanded control path
/// * `wait` - How long to wait for the hello
#[cfg(unix)]
fn probe_now(socket: &Path, wait: Duration) -> Probe {
    use std::io::{ErrorKind, Read};
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return Probe::Absent;
    };
    if stream.set_read_timeout(Some(wait)).is_err() {
        return Probe::Absent;
    }
    let mut byte = [0u8; 1];
    match stream.read(&mut byte) {
        Ok(1..) => Probe::Live,
        Ok(0) => Probe::Absent,
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            Probe::Stuck
        }
        Err(_) => Probe::Absent,
    }
}

/// No master on Windows (it does not multiplex).
///
/// # Arguments
///
/// * `_socket` - The expanded control path
/// * `_wait` - How long to wait
#[cfg(not(unix))]
fn probe_now(_socket: &Path, _wait: Duration) -> Probe {
    Probe::Absent
}

/// Whether `path` names one of this configuration's sockets: directly in
/// `control_dir`, named `<tag>` and 40 lowercase hex digits (`%C`: a SHA-1
/// in hex on every client checked, 7.6p1 to 10.3p1; with another form,
/// multiplexing is off).
///
/// # Arguments
///
/// * `path` - The expanded control path
/// * `control_dir` - The checked control directory
/// * `tag` - The configuration's tag
pub(super) fn ours(path: &Path, control_dir: &Path, tag: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(hash) = name.strip_prefix(tag) else {
        return false;
    };
    let hex = hash.len() == 40
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let trimmed = |p: &Path| p.to_string_lossy().trim_end_matches('/').to_string();
    hex && path.parent().map(trimmed) == Some(trimmed(control_dir))
}

/// Remove `path` when it is a Unix socket (links not followed). Best
/// effort.
///
/// # Arguments
///
/// * `path` - One of this configuration's sockets ([`ours`])
pub(super) fn remove(path: &PathBuf) {
    if is_socket(path) {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether `path` is a Unix socket (links not followed).
///
/// # Arguments
///
/// * `path` - The path
#[cfg(unix)]
pub(super) fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

/// Never: Windows does not multiplex.
///
/// # Arguments
///
/// * `_path` - The path
#[cfg(not(unix))]
pub(super) fn is_socket(_path: &Path) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    use super::*;

    /// A tag and a `%C` as ssh expands it.
    const NAME: &str = "abcdefgh24b95b452d45b637cea6bcd02cba7dce6b61a64e";

    #[test]
    fn a_master_that_says_hello_is_live_one_that_does_not_is_stuck() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let answering = std::thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            client.write_all(b"\0\0\0\x15").unwrap();
            listener
        });
        assert_eq!(probe_now(&path, Duration::from_secs(5)), Probe::Live);
        // The same socket, nobody accepting: queued, silent.
        let _listener = answering.join().unwrap();
        assert_eq!(probe_now(&path, Duration::from_millis(300)), Probe::Stuck);
    }

    #[test]
    fn no_socket_or_nobody_listening_is_no_master() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        assert_eq!(probe_now(&path, Duration::from_millis(300)), Probe::Absent);
        drop(UnixListener::bind(&path).unwrap());
        // A stale socket file: the connection is refused.
        assert!(is_socket(&path));
        assert_eq!(probe_now(&path, Duration::from_millis(300)), Probe::Absent);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "").unwrap();
        assert_eq!(probe_now(&path, Duration::from_millis(300)), Probe::Absent);
    }

    #[tokio::test]
    async fn probing_runs_off_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(probe(&dir.path().join("none")).await, Probe::Absent);
    }

    #[test]
    fn only_this_configurations_sockets_in_the_control_directory_are_ours() {
        let dir = Path::new("/home/u/.apvm/ssh-mux");
        let socket = dir.join(NAME);
        assert!(ours(&socket, dir, "abcdefgh"));
        assert!(ours(
            &socket,
            Path::new("/home/u/.apvm/ssh-mux/"),
            "abcdefgh"
        ));
        // Another tag, another directory, a name that is not `%C`.
        assert!(!ours(&socket, dir, "zzzzzzzz"));
        assert!(!ours(&Path::new("/tmp").join(NAME), dir, "abcdefgh"));
        assert!(!ours(&dir.join("sub").join(NAME), dir, "abcdefgh"));
        assert!(!ours(&dir.join("abcdefghx"), dir, "abcdefgh"));
        let upper = format!("abcdefgh{}", "A".repeat(40));
        assert!(!ours(&dir.join(upper), dir, "abcdefgh"));
        assert!(!ours(&dir.join(format!("{NAME}0")), dir, "abcdefgh"));
        // Not expanded.
        assert!(!ours(&dir.join("abcdefgh%C"), dir, "abcdefgh"));
    }

    #[test]
    fn only_a_socket_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(is_socket(&socket));
        let file = dir.path().join("f");
        std::fs::write(&file, "").unwrap();
        remove(&file);
        assert!(file.exists());
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        assert!(!is_socket(&link));
        remove(&link);
        assert!(link.exists());
        remove(&socket);
        assert!(!socket.exists());
        assert!(!is_socket(&dir.path().join("missing")));
    }
}
