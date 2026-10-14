//! Connection sharing: the generated lines, when it is off and why, the
//! socket path limit, the tag, and the control directory's checks.

use super::*;

/// A tag as [`connection_tag`] makes them.
const TAG: &str = "abcdefgh";

/// Inputs that turn multiplexing on for `dir` on Unix, with `limit`.
///
/// # Arguments
///
/// * `dir` - The control directory
/// * `limit` - The longest socket path
fn on(dir: &Path, limit: usize) -> MuxInputs<'_> {
    MuxInputs {
        enabled: true,
        control_dir: Some(dir),
        platform: Platform::Unix,
        tag: TAG,
        limit,
    }
}

#[test]
fn the_shared_connection_lines() {
    let dir = Path::new("/home/u/.apvm/ssh-mux");
    let socket = control_socket(&on(dir, SOCKET_PATH_LIMIT)).unwrap();
    // A call rides a master, never becomes one; the starter becomes it.
    assert_eq!(
        socket.client_lines(),
        [
            "ControlMaster no",
            "ControlPath \"/home/u/.apvm/ssh-mux/abcdefgh%C\"",
        ]
    );
    assert_eq!(
        socket.master_lines(),
        [
            "ControlMaster auto",
            "ControlPath \"/home/u/.apvm/ssh-mux/abcdefgh%C\"",
            "ControlPersist 15s",
        ]
    );
    assert_eq!(socket.tag(), TAG);
    assert_eq!(
        socket.multiplexing(),
        Multiplexing::On {
            control_path: PathBuf::from("/home/u/.apvm/ssh-mux/abcdefgh%C")
        }
    );
    assert_eq!(socket.dir(), dir);
    assert_eq!(NOT_SHARED, ["ControlMaster no", "ControlPath none"]);
}

#[test]
fn the_directory_is_quoted_with_its_percent_signs_doubled() {
    // ssh splits ControlPath on whitespace and expands `%` (plan rev 8);
    // `%C` itself stays a token. A trailing slash is not doubled.
    let socket = control_socket(&on(Path::new("/Users/Jane Doe/a%b/"), 200)).unwrap();
    assert_eq!(
        socket.client_lines()[1],
        "ControlPath \"/Users/Jane Doe/a%%b/abcdefgh%C\""
    );
}

#[test]
fn it_is_off_when_disabled_without_a_directory_and_on_windows() {
    let dir = Path::new("/home/u/.apvm/ssh-mux");
    let disabled = MuxInputs {
        enabled: false,
        control_dir: None,
        ..on(dir, SOCKET_PATH_LIMIT)
    };
    assert_eq!(control_socket(&disabled), Err(NoMultiplexing::Disabled));
    let no_dir = MuxInputs {
        control_dir: None,
        ..on(dir, SOCKET_PATH_LIMIT)
    };
    assert_eq!(control_socket(&no_dir), Err(NoMultiplexing::NoControlDir));
    // Win32-OpenSSH has no ControlMaster, whatever the directory.
    let windows = MuxInputs {
        control_dir: Some(Path::new(r"C:\Users\Jane\.apvm\ssh-mux")),
        platform: Platform::Windows,
        ..on(dir, SOCKET_PATH_LIMIT)
    };
    assert_eq!(control_socket(&windows), Err(NoMultiplexing::Unsupported));
}

#[test]
fn a_directory_ssh_would_misread_turns_it_off_and_says_why() {
    let cases = [
        ("relative/mux", "'relative/mux' is not absolute"),
        (
            "/a${HOME}/mux",
            "'/a${HOME}/mux' holds '${', which ssh would expand",
        ),
        ("/a\"b/mux", "'/a\"b/mux' holds '\"'"),
        (
            "/a\\b/mux",
            "'/a\\b/mux' holds '\\', which ssh would read as an escape",
        ),
        ("/a\nb", "'/a\\u{a}b' holds U+000A"),
    ];
    for (dir, reason) in cases {
        let got = control_socket(&on(Path::new(dir), 200));
        assert_eq!(
            got,
            Err(NoMultiplexing::UnusableControlDir {
                reason: reason.to_string()
            }),
            "{dir:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_directory_that_is_not_utf8_turns_it_off() {
    use std::os::unix::ffi::OsStrExt;
    let dir = PathBuf::from(std::ffi::OsStr::from_bytes(b"/a\xffb"));
    let Err(NoMultiplexing::UnusableControlDir { reason }) = control_socket(&on(&dir, 200)) else {
        panic!("a path that is not UTF-8 was accepted");
    };
    assert!(reason.contains("is not valid UTF-8"), "{reason}");
}

#[test]
fn a_socket_path_over_the_limit_turns_it_off() {
    // `<dir>/<tag><40 hex>`: 49 bytes past the directory.
    let dir = Path::new("/Users/sandyfigueroa/.apvm/ssh-mux");
    assert_eq!(dir.as_os_str().len(), 34);
    assert!(control_socket(&on(dir, 83)).is_ok());
    assert_eq!(
        control_socket(&on(dir, 82)),
        Err(NoMultiplexing::PathTooLong {
            length: 83,
            limit: 82
        })
    );
    // A trailing slash is not counted twice.
    assert!(control_socket(&on(Path::new("/Users/sandyfigueroa/.apvm/ssh-mux/"), 83)).is_ok());
    // `%` counts once: ssh expands `%%` back.
    assert_eq!(expanded_length(Path::new("/a%b"), TAG), 4 + 1 + 8 + 40);
}

#[test]
fn the_limit_is_what_ssh_can_bind_here() {
    // Verified: 86 bytes bind on macOS (10.3p1) and 90 on Linux (10.0p2),
    // one more fails the call.
    #[cfg(target_os = "macos")]
    assert_eq!(SOCKET_PATH_LIMIT, 86);
    #[cfg(target_os = "linux")]
    assert_eq!(SOCKET_PATH_LIMIT, 90);
    // The CLI's directory fits for a 16-character macOS user name.
    let dir = Path::new("/Users/abcdefghijklmnop/.apvm/ssh-mux");
    assert!(expanded_length(dir, TAG) <= 86);
}

#[test]
fn a_tag_is_eight_base32_characters_that_tell_configurations_apart() {
    let destination = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
    let base = connection_tag(
        Path::new("ssh"),
        "BatchMode yes\n",
        &destination(&["-l", "u", "--", "h"]),
    );
    assert_eq!(base.len(), TAG_LEN);
    assert!(base.bytes().all(|b| BASE32.contains(&b)), "{base}");
    // The same inputs, the same tag (a later call finds the master).
    let again = connection_tag(
        Path::new("ssh"),
        "BatchMode yes\n",
        &destination(&["-l", "u", "--", "h"]),
    );
    assert_eq!(again, base);
    // Any difference, another tag; fields never run into each other.
    for (program, config, args) in [
        (
            "/usr/bin/ssh",
            "BatchMode yes\n",
            vec!["-l", "u", "--", "h"],
        ),
        ("ssh", "BatchMode yes\nX\n", vec!["-l", "u", "--", "h"]),
        ("ssh", "BatchMode yes\n", vec!["-l", "v", "--", "h"]),
        (
            "ssh",
            "BatchMode yes\n",
            vec!["-l", "u", "-J", "j", "--", "h"],
        ),
        ("ssh", "BatchMode yes\n", vec!["-l", "u-", "-", "h"]),
        ("ss", "hBatchMode yes\n", vec!["-l", "u", "--", "h"]),
    ] {
        let other = connection_tag(Path::new(program), config, &destination(&args));
        assert_ne!(other, base, "{program} {config:?} {args:?}");
    }
}

#[test]
fn base32_follows_rfc_4648_in_lowercase() {
    // RFC 4648 §10: BASE32("fooba") = "MZXW6YTB".
    assert_eq!(base32(b"fooba"), "mzxw6ytb");
    assert_eq!(base32(&[0; 5]), "aaaaaaaa");
    assert_eq!(base32(&[0xff; 10]), "7777777777777777");
    assert_eq!(base32(&[]), "");
}

#[test]
fn reasons_read_as_clauses() {
    let cases = [
        (
            NoMultiplexing::Disabled,
            "multiplexing is off for this server",
        ),
        (
            NoMultiplexing::NoControlDir,
            "no control directory was given",
        ),
        (
            NoMultiplexing::Unsupported,
            "OpenSSH for Windows cannot multiplex",
        ),
        (
            NoMultiplexing::PathTooLong {
                length: 91,
                limit: 86,
            },
            "the control socket's path would be 91 bytes, over this system's 86: choose a \
             shorter control directory",
        ),
        (
            NoMultiplexing::UnusableControlDir {
                reason: "'/m' is a symbolic link".into(),
            },
            "the control directory is unusable: '/m' is a symbolic link",
        ),
    ];
    for (reason, text) in cases {
        assert_eq!(reason.to_string(), text);
    }
}

// The fallback is Unix-only, like multiplexing itself: on Windows, `join`
// would build `/tmp\apvm-mux-501`, which no Unix rule accepts.
#[cfg(unix)]
#[test]
fn the_fallback_directory_is_short_and_this_users() {
    let dir = fallback_dir(Path::new("/tmp"), 501);
    assert_eq!(dir, Path::new("/tmp/apvm-mux-501"));
    // Fits with room to spare: 17 + 49 bytes.
    assert!(expanded_length(&dir, TAG) <= 86);
    assert!(control_socket(&on(&dir, 86)).is_ok());
}

#[test]
fn without_a_shared_tmp_there_is_no_fallback() {
    assert_eq!(fallback_here(None), None);
}

#[cfg(unix)]
mod directory {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// This process's effective user.
    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// `socket`'s check, with a planned socket in `dir`.
    ///
    /// # Arguments
    ///
    /// * `dir` - The control directory
    fn check(dir: &Path) -> Result<(), NoMultiplexing> {
        usable_dir(&control_socket(&on(dir, 4096)).unwrap())
    }

    /// The mode bits of `path`.
    ///
    /// # Arguments
    ///
    /// * `path` - An existing path
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn a_private_directory_of_this_users_is_the_only_one_used() {
        let me = me();
        let dir = EntryKind::Directory;
        assert_eq!(private_dir_rule(dir, me, 0o40700, me), Ok(()));
        // Others may read and enter: they cannot plant a socket.
        assert_eq!(private_dir_rule(dir, me, 0o40755, me), Ok(()));
        assert_eq!(
            private_dir_rule(dir, me, 0o40770, me),
            Err("may be written by its group or others (mode 0770): run 'chmod go-w' on it".into())
        );
        assert!(private_dir_rule(dir, me, 0o41777, me).is_err());
        assert!(private_dir_rule(dir, me, 0o40702, me).is_err());
        assert_eq!(
            private_dir_rule(dir, me + 1, 0o40700, me),
            Err(format!(
                "belongs to user {}, not to this user ({me})",
                me + 1
            ))
        );
        // ssh must create its socket there.
        assert_eq!(
            private_dir_rule(dir, me, 0o40500, me),
            Err("cannot be written by this user (mode 0500)".into())
        );
        assert!(private_dir_rule(dir, me, 0o40600, me).is_err());
        assert_eq!(
            private_dir_rule(EntryKind::Link, me, 0o120777, me),
            Err("is a symbolic link".into())
        );
        assert_eq!(
            private_dir_rule(EntryKind::Other, me, 0o100600, me),
            Err("is not a directory".into())
        );
    }

    #[test]
    fn every_directory_above_it_is_this_users_or_roots_and_safe_to_share() {
        let me = me();
        let other = me + 1;
        assert_eq!(ancestor_rule(me, 0o40700, me), Ok(()));
        assert_eq!(ancestor_rule(0, 0o40755, me), Ok(()));
        // `/tmp`: anyone may write, but the sticky bit keeps others from
        // renaming what this user made there.
        assert_eq!(ancestor_rule(0, 0o41777, me), Ok(()));
        assert_eq!(
            ancestor_rule(me, 0o40777, me),
            Err("its group or others may write without the sticky bit (mode 0777)".into())
        );
        assert!(ancestor_rule(0, 0o40775, me).is_err());
        assert_eq!(
            ancestor_rule(other, 0o40755, me),
            Err(format!("belongs to user {other}"))
        );
    }

    #[test]
    fn a_directory_under_one_others_may_write_turns_it_off() {
        let base = tempfile::tempdir().unwrap();
        let open = base.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let got = check(&open.join("mux"));
        let Err(NoMultiplexing::UnusableControlDir { reason }) = got else {
            panic!("{got:?}");
        };
        assert!(
            reason.ends_with(
                "/open', which its group or others may write without the sticky bit (mode 0777)"
            ),
            "{reason}"
        );
        // Sticky, it is fine (as `/tmp`).
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(check(&open.join("mux2")), Ok(()));
    }

    #[test]
    fn a_missing_directory_is_created_private_with_its_parents() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("a/b/ssh-mux");
        assert_eq!(check(&dir), Ok(()));
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&base.path().join("a")), 0o700);
        // The test socket is gone; an existing private directory is fine.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(check(&dir), Ok(()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn an_unsafe_directory_turns_it_off() {
        let base = tempfile::tempdir().unwrap();
        let reason = |dir: &Path| match check(dir) {
            Err(NoMultiplexing::UnusableControlDir { reason }) => reason,
            other => panic!("{other:?}"),
        };
        let open = base.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let shown = open.to_str().unwrap();
        assert_eq!(
            reason(&open),
            format!(
                "'{shown}' may be written by its group or others (mode 0777): run 'chmod go-w' \
                 on it"
            )
        );
        let link = base.path().join("link");
        std::os::unix::fs::symlink(base.path(), &link).unwrap();
        assert!(reason(&link).ends_with("' is a symbolic link"));
        let file = base.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(
            reason(&file).contains("cannot be created: "),
            "{}",
            reason(&file)
        );
        let under_file = file.join("mux");
        assert!(reason(&under_file).contains("cannot be created: "));
    }

    #[test]
    fn a_directory_this_user_cannot_write_turns_it_off() {
        if rustix::process::geteuid().is_root() {
            // Root writes anywhere; the mode check still refuses it.
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("ro");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let got = check(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let Err(NoMultiplexing::UnusableControlDir { reason }) = got else {
            panic!("{got:?}");
        };
        assert!(
            reason.ends_with("cannot be written by this user (mode 0500)"),
            "{reason}"
        );
    }
}
