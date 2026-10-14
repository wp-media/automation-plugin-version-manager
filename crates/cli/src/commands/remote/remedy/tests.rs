//! Remedies for every failure class, from real OpenSSH stderr samples
//! (the classifier's, plan §3.5) and the probe's failures.

use std::time::Duration;

use apvm_sites::remote::{Captured, Exit, Identity, Nonce, classify};
use apvm_sites::wp::ProbeFailure;
use apvm_sites::{Inventory, Server};

use super::*;

/// Server `eu-1` at 203.0.113.10, changed by `edit`.
///
/// # Arguments
///
/// * `edit` - Changes to the server record
fn server(edit: impl FnOnce(&mut Server)) -> ResolvedServer {
    let mut record = Server::new("eu-1", "203.0.113.10");
    edit(&mut record);
    let mut inventory = Inventory::new();
    inventory.add_server(record).expect("valid server");
    inventory.resolve_server("eu-1").expect("resolves")
}

/// The context of a test of `server` for `subject`.
fn context<'a>(server: &'a ResolvedServer, subject: Subject<'a>) -> Context<'a> {
    Context { server, subject }
}

/// ssh's failure for `stderr` (exit 255), classified as the transport
/// does.
///
/// # Arguments
///
/// * `stderr` - What ssh printed
fn ssh(stderr: &str) -> Error {
    let execution = apvm_sites::remote::Execution::new(
        Nonce::random(),
        Exit::TransportFailed,
        Captured::whole(Vec::new()),
        Captured::whole(stderr.as_bytes().to_vec()),
        true,
        Duration::ZERO,
    );
    Error::Ssh(classify(&execution).expect("exit 255 is classified"))
}

/// The remedies for `error` when testing site `shop` (existing) on
/// `server`.
fn for_site(error: &Error, server: &ResolvedServer) -> Vec<String> {
    remedies(
        error,
        &context(
            server,
            Subject::Site {
                id: "shop",
                adding: false,
            },
        ),
    )
}

/// The remedies for `error` when testing the existing `server`.
fn for_server(error: &Error, server: &ResolvedServer) -> Vec<String> {
    remedies(error, &context(server, Subject::Server { adding: false }))
}

/// A located problem (the library's constructor is its own).
fn problem(location: &str, message: &str) -> Problem {
    Problem {
        location: location.into(),
        message: message.into(),
    }
}

/// Whether any remedy holds `needle`.
fn mentions(remedies: &[String], needle: &str) -> bool {
    remedies.iter().any(|remedy| remedy.contains(needle))
}

#[test]
fn an_unknown_host_key_needs_a_checked_trust_new_host_key() {
    let error = ssh(
        "No ED25519 host key is known for [127.0.0.1]:2722 and you have requested strict \
         checking.\r\nHost key verification failed.\r\n",
    );
    assert_eq!(error.code(), Code::SshHostKeyUnknown);
    let remedies = for_server(&error, &server(|_| {}));
    assert!(mentions(&remedies, "fingerprint"), "{remedies:?}");
    assert!(mentions(&remedies, "--trust-new-host-key"), "{remedies:?}");
    assert!(mentions(&remedies, "ProxyCommand"), "{remedies:?}");
}

#[test]
fn a_changed_key_is_removed_by_the_name_ssh_printed() {
    let error = ssh(
        "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
         @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n\
         @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
         Offending ED25519 key in /tmp/exp/kh_changed:1\r\n\
         Host key for [127.0.0.1]:2722 has changed and you have requested strict checking.\r\n\
         Host key verification failed.\r\n",
    );
    assert_eq!(error.code(), Code::SshHostKeyChanged);
    let plain = for_server(&error, &server(|_| {}));
    assert!(
        mentions(&plain, "ssh-keygen -R '[127.0.0.1]:2722'"),
        "{plain:?}"
    );
    assert!(!mentions(&plain, " -f "), "ssh's default file: {plain:?}");
    assert!(mentions(&plain, "attack"), "{plain:?}");
    assert!(mentions(&plain, "--trust-new-host-key"), "{plain:?}");
    let with_file = for_server(
        &error,
        &server(|s| s.ssh.known_hosts_file = Some("~/.apvm/kh".into())),
    );
    assert!(
        mentions(&with_file, "ssh-keygen -R '[127.0.0.1]:2722' -f ~/.apvm/kh"),
        "{with_file:?}"
    );
}

#[test]
fn a_key_that_differs_from_its_address_key_is_removed_by_address() {
    let error = ssh(
        "Warning: the ECDSA host key for 'target' differs from the key for the IP address \
         '172.19.0.3'\nOffending key for IP in /root/kh:5\nMatching host key in \
         /root/kh:1\r\nExiting, you have requested strict checking.\r\n\
         Host key verification failed.\r\n",
    );
    assert_eq!(error.code(), Code::SshHostKeyChanged);
    let remedies = for_server(&error, &server(|_| {}));
    assert!(
        mentions(
            &remedies,
            "the old key of its address: ssh-keygen -R '172.19.0.3'"
        ),
        "{remedies:?}"
    );
}

#[test]
fn a_revoked_key_is_never_removed() {
    for stderr in [
        "ED25519 host key for [127.0.0.1]:2722 was revoked and you have requested strict \
         checking.\r\nHost key verification failed.\r\n",
        "Host key ED25519 SHA256:Ur+osiBn7eNSbfdLzumsOkJxUf7ZVNN18FB5X/Gv8R4 revoked by file \
         /k/revoked_keys\r\nHost key verification failed.\r\n",
    ] {
        let error = ssh(stderr);
        assert_eq!(error.code(), Code::SshHostKeyChanged);
        let remedies = for_server(&error, &server(|_| {}));
        assert_eq!(remedies.len(), 1, "{remedies:?}");
        assert!(mentions(&remedies, "revoked"), "{remedies:?}");
        assert!(mentions(&remedies, "never remove it"), "{remedies:?}");
        assert!(!mentions(&remedies, "ssh-keygen -R '"), "{remedies:?}");
        assert!(!mentions(&remedies, "run again"), "{remedies:?}");
    }
}

#[test]
fn a_host_named_revoked_whose_key_changed_gets_the_removal() {
    let stderr = |host: &str| {
        format!(
            "Offending ED25519 key in /tmp/exp/kh:1\r\nHost key for {host} has changed and you \
             have requested strict checking.\r\nHost key verification failed.\r\n"
        )
    };
    for host in ["revoked-db.example.com", "[revoked]:2722"] {
        let error = ssh(&stderr(host));
        assert_eq!(error.code(), Code::SshHostKeyChanged);
        let remedies = for_server(&error, &server(|_| {}));
        assert!(
            mentions(&remedies, &format!("ssh-keygen -R '{host}'")),
            "{remedies:?}"
        );
        assert!(!mentions(&remedies, "never remove it"), "{remedies:?}");
    }
    let by_address = ssh(
        "Warning: the ECDSA host key for 'revoked-db' differs from the key for the IP address \
         '172.19.0.3'\nOffending key for IP in /root/kh:5\r\nExiting, you have requested strict \
         checking.\r\nHost key verification failed.\r\n",
    );
    let remedies = for_server(&by_address, &server(|_| {}));
    assert!(
        mentions(&remedies, "ssh-keygen -R '172.19.0.3'"),
        "{remedies:?}"
    );
}

#[test]
fn a_changed_key_without_a_name_line_uses_the_servers_ssh_name() {
    assert_eq!(ssh_name(&server(|_| {})), "203.0.113.10");
    assert_eq!(ssh_name(&server(|s| s.ssh.port = Some(22))), "203.0.113.10");
    assert_eq!(
        ssh_name(&server(|s| s.ssh.port = Some(2722))),
        "[203.0.113.10]:2722"
    );
    // The banner alone (no name line): the server's own name.
    let remedies = changed_key(
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        &context(
            &server(|s| s.ssh.port = Some(2722)),
            Subject::Server { adding: false },
        ),
    );
    assert!(
        mentions(&remedies, "ssh-keygen -R '[203.0.113.10]:2722'"),
        "{remedies:?}"
    );
    // A name holding a quote is never pasted into a command.
    let remedies = changed_key(
        "Host key for it's has changed and you have requested strict checking.",
        &context(&server(|_| {}), Subject::Server { adding: false }),
    );
    assert!(!mentions(&remedies, "ssh-keygen"), "{remedies:?}");
}

#[test]
fn a_refused_certificate_never_suggests_trusting_new_keys() {
    let error = ssh(
        "Certificate invalid: name is not a listed principal\r\nNo ED25519 host key is known \
         for [127.0.0.1]:27203 and you have requested strict checking.\r\nHost key \
         verification failed.\r\n",
    );
    assert_eq!(error.code(), Code::SshFailed);
    let remedies = for_server(&error, &server(|_| {}));
    assert_eq!(remedies.len(), 1, "{remedies:?}");
    assert!(mentions(&remedies, "host certificate"), "{remedies:?}");
    assert!(
        mentions(&remedies, "never use --trust-new-host-key"),
        "{remedies:?}"
    );
}

#[test]
fn a_proxycommands_own_refusal_points_at_that_ssh() {
    let error = ssh("Host key verification failed.\r\nConnection closed by UNKNOWN port 65535\r\n");
    assert_eq!(error.code(), Code::SshFailed);
    let remedies = for_server(&error, &server(|_| {}));
    assert!(mentions(&remedies, "ProxyCommand"), "{remedies:?}");
    assert!(!mentions(&remedies, "--trust-new-host-key"), "{remedies:?}");
}

#[test]
fn other_ssh_failed_lines_get_their_own_remedy_or_none() {
    let revoked_file = unrecognized(
        "Error checking host key ED25519 SHA256:x in revoked keys file /k: Permission denied",
    );
    assert!(
        mentions(&revoked_file, "RevokedHostKeys"),
        "{revoked_file:?}"
    );
    assert!(unrecognized("/home/u/.ssh/config line 3: Bad configuration option: x").is_empty());
}

#[test]
fn a_refused_login_names_the_user_and_key_flags() {
    let error = ssh("deploy@127.0.0.1: Permission denied (publickey).\r\n");
    assert_eq!(error.code(), Code::SshAuthFailed);
    let existing = for_server(&error, &server(|_| {}));
    assert_eq!(
        existing,
        [
            "check the login user, the key and the server's authorized_keys: apvm server edit \
          eu-1 with --user or --key"
        ]
    );
    let adding = remedies(
        &error,
        &context(&server(|_| {}), Subject::Server { adding: true }),
    );
    assert!(mentions(&adding, ": pass --user or --key"), "{adding:?}");
    // A site's test fixes the server's settings too.
    assert!(mentions(
        &for_site(&error, &server(|_| {})),
        "apvm server edit eu-1"
    ));
}

#[test]
fn a_refused_login_explains_an_open_or_unloadable_key() {
    let open = ssh(
        "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
         @         WARNING: UNPROTECTED PRIVATE KEY FILE!          @\r\n\
         @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
         Permissions 0644 for '/home/bob/openkey' are too open.\r\n\
         It is required that your private key files are NOT accessible by others.\r\n\
         This private key will be ignored.\r\n\
         Load key \"/home/bob/openkey\": bad permissions\r\n\
         deploy@target: Permission denied (publickey).\r\n",
    );
    let remedies = for_server(&open, &server(|_| {}));
    assert!(mentions(&remedies, "chmod 600"), "{remedies:?}");
    let garbage = auth_failed(
        "Load key \"/tmp/garbage\": invalid format",
        &context(&server(|_| {}), Subject::Server { adding: false }),
    );
    assert!(mentions(&garbage, "convert or replace"), "{garbage:?}");
}

#[test]
fn network_failures_name_the_connection_flags() {
    let refused = ssh("ssh: connect to host 127.0.0.1 port 1: Connection refused\r\n");
    assert_eq!(refused.code(), Code::SshUnreachable);
    assert_eq!(
        for_server(&refused, &server(|_| {})),
        [
            "check the host name, the port and the network (and the jump hosts): apvm server edit \
          eu-1 with --host, --port or --jump"
        ]
    );
    let slow = ssh("ssh: connect to host 192.0.2.1 port 22: Operation timed out\r\n");
    assert_eq!(slow.code(), Code::SshTimeout);
    assert_eq!(
        for_server(&slow, &server(|_| {})),
        [
            "check the network and any firewall, or allow longer: apvm server edit eu-1 \
          --connect-timeout <SECS>"
        ]
    );
    let adding = remedies(
        &slow,
        &context(&server(|_| {}), Subject::Server { adding: true }),
    );
    assert!(
        mentions(&adding, "pass --connect-timeout <SECS>"),
        "{adding:?}"
    );
}

#[test]
fn script_and_tool_failures_have_their_remedy() {
    let not_run = Error::ScriptNotRun {
        exit: Exit::Status(1),
        line: Some("This account is currently not available.".into()),
    };
    assert!(mentions(
        &for_server(&not_run, &server(|_| {})),
        "ForceCommand"
    ));
    let missing = Error::SshMissing {
        program: "ssh".into(),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    };
    assert!(mentions(
        &for_server(&missing, &server(|_| {})),
        "--ssh-binary"
    ));
    let slow = Error::Timeout {
        after: Duration::from_secs(300),
        started: true,
    };
    assert!(mentions(
        &for_site(&slow, &server(|_| {})),
        "apvm site edit shop --timeout"
    ));
    assert!(mentions(&for_server(&slow, &server(|_| {})), "try again"));
    let unknown = Error::Process {
        context: "cannot start",
        program: "ssh".into(),
        source: std::io::Error::other("x"),
    };
    assert!(for_server(&unknown, &server(|_| {})).is_empty());
}

#[test]
fn probe_failures_name_the_site_flag_to_change() {
    let srv = server(|_| {});
    let cases: Vec<(ProbeFailure, &str)> = vec![
        (
            ProbeFailure::PluginsDirMismatch {
                expected: "/p".into(),
                found: Some("/q".into()),
                identity: Identity::Login,
            },
            "--plugins-dir <PATH>",
        ),
        (
            ProbeFailure::PluginsDirLinked {
                dir: "/p".into(),
                detail: None,
            },
            "--plugins-dir <PATH>",
        ),
        (
            ProbeFailure::PluginsDirNotDirectory {
                dir: "/p".into(),
                found: "symbolic link".into(),
            },
            "--plugins-dir <PATH>",
        ),
        (
            ProbeFailure::PluginsDirMissing {
                dir: "/p".into(),
                pinned: true,
            },
            "--plugins-dir <PATH>",
        ),
        (
            ProbeFailure::PluginsDirMissing {
                dir: "/p".into(),
                pinned: false,
            },
            "--path <WP_ROOT>",
        ),
        (
            ProbeFailure::WpCliMissing {
                program: vec!["wp".into()],
                reason: "was not found".into(),
            },
            "--wp-cli",
        ),
        (
            ProbeFailure::NotWordpress {
                path: "/p".into(),
                message: None,
            },
            "--path <WP_ROOT>",
        ),
        (
            ProbeFailure::WpNotInstalled {
                path: "/p".into(),
                message: None,
            },
            "wp core install",
        ),
    ];
    for (failure, flag) in cases {
        let error = Error::Probe(Box::new(failure));
        let existing = for_site(&error, &srv);
        assert!(
            mentions(&existing, &format!("apvm site edit shop {flag}"))
                || mentions(&existing, flag),
            "{error}: {existing:?}"
        );
        let adding = remedies(
            &error,
            &context(
                &srv,
                Subject::Site {
                    id: "shop",
                    adding: true,
                },
            ),
        );
        assert!(!mentions(&adding, "apvm site edit"), "{error}: {adding:?}");
    }
    let changed = Error::Probe(Box::new(ProbeFailure::PluginsDirChanged {
        dir: "/p".into(),
        expected: (1, 1),
        found: (2, 2),
    }));
    assert!(mentions(&for_site(&changed, &srv), "run it again"));
    let root_stat = Error::Probe(Box::new(ProbeFailure::RootStatNeedsSudo {
        dir: "/p".into(),
    }));
    assert!(mentions(&for_site(&root_stat, &srv), "/usr/bin/stat"));
    let unreadable = Error::Probe(Box::new(ProbeFailure::Unreadable {
        step: "H",
        reason: "x".into(),
    }));
    assert!(for_site(&unreadable, &srv).is_empty());
}

#[test]
fn identity_refusals_name_what_to_change() {
    let srv = server(|_| {});
    let owner_is_root = Error::Probe(Box::new(ProbeFailure::Identity(
        apvm_sites::remote::IdentityRefusal::OwnerIsRoot {
            role: apvm_sites::remote::RootRole::Owner,
            account: apvm_sites::remote::Account::new(0, None),
        },
    )));
    assert_eq!(owner_is_root.code(), Code::OwnerIsRoot);
    assert!(mentions(
        &for_site(&owner_is_root, &srv),
        "apvm site edit shop --allow-root"
    ));
    let no_sudo = Error::Probe(Box::new(ProbeFailure::Identity(
        apvm_sites::remote::IdentityRefusal::SudoMissing {
            user: apvm_sites::remote::Account::new(33, None),
        },
    )));
    assert_eq!(no_sudo.code(), Code::SudoMissing);
    assert!(mentions(
        &for_site(&no_sudo, &srv),
        "apvm server edit eu-1 --user <USER>"
    ));
}

#[test]
fn refused_settings_get_one_remedy_each() {
    let srv = server(|_| {});
    let problems = vec![
        problem(
            "ssh_config_file",
            "'/c' is writable by its group or others, and OpenSSH refuses to include such a file",
        ),
        problem(
            "ssh_config_file",
            "'/c' is writable by its group or others, and OpenSSH refuses to include such a file",
        ),
        problem(
            "known_hosts_file",
            "'/k%h' holds '%', which ssh 8.4 and later expand and older versions do not",
        ),
        problem("identity_file", "'/k' does not exist"),
        problem("binary", "'/x/ssh' does not exist"),
        problem("elsewhere", "x"),
    ];
    let error = Error::InvalidInput {
        subject: "SSH settings of server 'eu-1'".into(),
        problems,
    };
    let remedies = for_server(&error, &srv);
    assert_eq!(
        remedies,
        [
            "make it private: chmod go-w on the ssh_config file named above",
            "choose a known_hosts path without '%': apvm server edit eu-1 --known-hosts <PATH>",
            "apvm server edit eu-1 --key <PATH>",
            "apvm server defaults --ssh-binary <PATH>",
        ]
    );
    let system = Error::InvalidInput {
        subject: "SSH settings of server 'eu-1'".into(),
        problems: vec![problem(
            "ssh_config_file",
            "is not set, and the system ssh_config '/etc/ssh/ssh_config' belongs to another user",
        )],
    };
    assert_eq!(
        for_server(&system, &srv),
        ["set an ssh_config of your own: apvm server defaults --ssh-config <PATH>"]
    );
}

#[test]
fn lists_read_naturally() {
    assert_eq!(or_list(&[]), "");
    assert_eq!(or_list(&["--a"]), "--a");
    assert_eq!(or_list(&["--a", "--b"]), "--a or --b");
    assert_eq!(or_list(&["--a", "--b", "--c"]), "--a, --b or --c");
}
