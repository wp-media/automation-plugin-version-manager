//! The SSH transport end to end through the hermetic `fake-ssh`
//! (`tests/support/fake-remote/fake-ssh`, Unix only): the argv and the
//! generated file ssh gets, the file's lifetime on every path, the checks
//! that run before ssh, a missing or unusable ssh, and the failure classes.
//!
//! `fake-ssh` is a committed script that tests never write, so exec'ing it
//! cannot hit Linux's "Text file busy" race. Its behavior comes from
//! `#fake-ssh:` lines in each test's own ssh_config file.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use apvm_process::test_support::{
    assert_all_gone, heartbeat, recorded_pids, tree_visible, wait_for_lines,
};

use super::super::quote::Word;
use super::super::statement::{Body, Statement};
use super::super::tests::safe_tempdir;
use super::*;
use crate::Code;
use crate::inventory::{Defaults, Inventory, Server, SshSettings};
use crate::remote::{
    Exit, Identity, Multiplexing, NoMultiplexing, Nonce, Payload, Script, ServerLimits,
    SshFailureKind, UploadScript, classify, host_key_notices,
};

/// A timeout no test reaches unless something hangs.
const LONG: Duration = Duration::from_secs(60);

/// The committed fake ssh client, as an absolute canonical path.
fn fake_ssh() -> PathBuf {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/fake-remote/fake-ssh");
    std::fs::canonicalize(path).unwrap()
}

/// A framed stdin script whose one statement is `sh -c <code>`.
///
/// # Arguments
///
/// * `code` - Shell code for the statement
fn framed(code: &str) -> Script {
    let words = vec![Word::lit("-c"), Word::checked(code)];
    let statement = Statement::as_identity(&Identity::Login, "sh", words);
    Script::framed(Nonce::random(), &Body::new(statement, []))
}

/// Write `text` to `path` with mode `0600`, whatever the umask: ssh refuses
/// to include an ssh_config file its group may write.
///
/// # Arguments
///
/// * `path` - The file
/// * `text` - Its content
fn write_private(path: &Path, text: impl AsRef<[u8]>) {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    std::io::Write::write_all(&mut file, text.as_ref()).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
}

/// A private directory for one test: the home, the generated files'
/// directory, the inventory's local files.
struct Sandbox {
    /// The directory (deleted on drop).
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A sandbox whose ssh_config file holds `directives` (`#fake-ssh:`
    /// lines), with a key and an existing directory for generated files.
    ///
    /// # Arguments
    ///
    /// * `directives` - `KEY VALUE` pairs
    fn new(directives: &[&str]) -> Self {
        let dir = safe_tempdir();
        let sandbox = Self { dir };
        std::fs::create_dir(sandbox.generated()).unwrap();
        std::fs::write(sandbox.path("id_ed25519"), "not a real key\n").unwrap();
        let text: String = directives
            .iter()
            .map(|directive| format!("#fake-ssh: {directive}\n"))
            .collect();
        write_private(&sandbox.path("ssh_config"), text);
        sandbox
    }

    /// `name` in the sandbox.
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The text of `name` in the sandbox (`""` when missing).
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    /// Where the generated ssh_config files go.
    fn generated(&self) -> PathBuf {
        self.path("generated")
    }

    /// The generated files that exist now.
    fn generated_files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.generated())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    /// Settings naming the fake ssh, the sandbox's key, known_hosts and
    /// ssh_config files, user `deploy` and port 2222.
    fn settings(&self) -> SshSettings {
        SshSettings {
            user: Some("deploy".into()),
            port: Some(2222),
            identity_file: Some(self.text("id_ed25519")),
            known_hosts_file: Some(self.text("known_hosts")),
            ssh_config_file: Some(self.text("ssh_config")),
            binary: Some(fake_ssh().to_str().unwrap().into()),
            ..SshSettings::default()
        }
    }

    /// `name`'s path as text.
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn text(&self, name: &str) -> String {
        self.path(name).to_str().unwrap().to_string()
    }

    /// A transport to `example.com` with `settings` as `defaults.ssh`.
    ///
    /// # Arguments
    ///
    /// * `settings` - The defaults
    /// * `host_keys` - The host key policy
    fn transport(&self, settings: SshSettings, host_keys: HostKeyPolicy) -> SshTransport {
        let mut inventory = Inventory::new();
        inventory.set_defaults(Defaults { ssh: settings }).unwrap();
        inventory
            .add_server(Server::new("s", "example.com"))
            .unwrap();
        let server = inventory.resolve_server("s").unwrap();
        let options = SshOptions::new()
            .with_host_keys(host_keys)
            .with_home(self.path("home"))
            .with_config_dir(self.generated());
        SshTransport::new(server, options)
    }

    /// The strict transport with [`Sandbox::settings`].
    fn strict(&self) -> SshTransport {
        self.transport(self.settings(), HostKeyPolicy::Strict)
    }
}

/// The `arg:` lines of a fake-ssh log.
///
/// # Arguments
///
/// * `log` - The log's text
fn logged_args(log: &str) -> Vec<&str> {
    log.lines()
        .filter_map(|line| line.strip_prefix("arg: "))
        .collect()
}

/// The `cwd:` line of a fake-ssh log: the directory ssh ran in.
///
/// # Arguments
///
/// * `log` - The log's text
fn logged_cwd(log: &str) -> PathBuf {
    let line = log.lines().find_map(|line| line.strip_prefix("cwd: "));
    PathBuf::from(line.unwrap())
}

/// The `config:` … `end` block of a fake-ssh log.
///
/// # Arguments
///
/// * `log` - The log's text
fn logged_config(log: &str) -> String {
    let start = log.find("config:\n").unwrap() + "config:\n".len();
    let end = log[start..].find("end\n").unwrap();
    log[start..start + end].to_string()
}

#[tokio::test]
async fn a_script_runs_with_the_generated_file_and_its_argv() {
    let sandbox = Sandbox::new(&[]);
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n"),
    );
    let ssh = sandbox.strict();
    let request = Request::script(
        framed("printf 'hello\\n'; printf 'err\\n' >&2; exit 3"),
        LONG,
    );

    let done = ssh.execute(&request).await.unwrap();
    assert_eq!(done.exit(), Exit::Status(3));
    assert_eq!(done.payload(), Ok(&b"hello\n"[..]));
    assert_eq!(done.stderr().bytes(), b"err\n");
    assert_eq!(classify(&done), None);

    let log = sandbox.read("log");
    let args = logged_args(&log);
    assert_eq!(args.len(), 9, "{log}");
    // ssh runs in the call's private directory, inside the generated
    // files' directory, and gets the file's path there (shell-literal).
    let cwd = logged_cwd(&log);
    let generated = std::fs::canonicalize(sandbox.generated()).unwrap();
    assert_eq!(cwd.parent(), Some(generated.as_path()), "{log}");
    let private = cwd.file_name().unwrap().to_str().unwrap();
    assert!(private.starts_with("apvm-ssh-"), "{private}");
    let file = sandbox.generated().join(private).join("apvm-ssh.conf");
    assert_eq!(&args[..2], ["-F", file.to_str().unwrap()]);
    assert!(log.contains("\ndir: 700 apvm-ssh.conf\n"), "{log}");
    assert_eq!(
        &args[2..],
        ["-l", "deploy", "-p", "2222", "--", "example.com", "sh -s"]
    );
    assert!(log.contains("\nmode: 600\n"), "{log}");
    // The file ssh read is the one config_text shows, and it is gone now.
    let config = logged_config(&log);
    assert_eq!(config, ssh.config_text().await.unwrap());
    assert!(config.contains(&format!(
        "UserKnownHostsFile \"{}\"\n",
        sandbox.text("known_hosts")
    )));
    assert!(config.contains(&format!(
        "IdentityFile \"{}\"\nIdentitiesOnly yes\n",
        sandbox.text("id_ed25519")
    )));
    // fake-ssh reports 10.3p1's defaults: the pinned options are there.
    assert!(
        config.ends_with(&format!(
            "Tunnel no\nAddKeysToAgent no\nSessionType default\nStdinNull no\n\
             ForkAfterAuthentication no\n\
             ChannelTimeout none\nInclude \"{}\"\n",
            sandbox.text("ssh_config")
        )),
        "{config}"
    );
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn a_client_before_openssh_8_7_gets_no_pinned_session_options() {
    let sandbox = Sandbox::new(&[]);
    // fake-ssh run through a `-old` link reports an older client's
    // defaults (the link's target is committed, never written).
    let old = sandbox.path("fake-ssh-old");
    std::os::unix::fs::symlink(fake_ssh(), &old).unwrap();
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n"),
    );
    let settings = SshSettings {
        binary: Some(old.to_str().unwrap().into()),
        ..sandbox.settings()
    };
    let ssh = sandbox.transport(settings, HostKeyPolicy::Strict);
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    let config = logged_config(&sandbox.read("log"));
    assert!(config.contains("AddKeysToAgent no\nInclude \""), "{config}");
    for option in [
        "SessionType",
        "StdinNull",
        "ForkAfterAuthentication",
        "ChannelTimeout",
    ] {
        assert!(!config.contains(option), "{option}: {config}");
    }
}

#[tokio::test]
async fn an_upload_streams_its_payload_through_ssh() {
    let sandbox = Sandbox::new(&[]);
    let destination = sandbox.text("uploaded.zip");
    let script = UploadScript::tee(Nonce::random(), &Identity::Login, &destination).unwrap();
    let bytes: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let request = Request::upload(script, Payload::Bytes(Arc::from(bytes.clone())), LONG);

    let done = sandbox.strict().execute(&request).await.unwrap();
    assert_eq!(done.exit(), Exit::Status(0), "{:?}", done.stderr());
    assert!(done.stdin_written());
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn the_generated_file_is_deleted_when_the_call_times_out() {
    let sandbox = Sandbox::new(&[]);
    let request = Request::script(framed("sleep 30"), Duration::from_millis(500));
    let done = sandbox.strict().execute(&request).await.unwrap();
    assert_eq!(done.exit(), Exit::TimedOut);
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn a_dropped_call_kills_ssh_and_deletes_the_generated_file() {
    if !tree_visible() {
        return;
    }
    let sandbox = Sandbox::new(&[]);
    let (beats, pids) = (sandbox.path("beats"), sandbox.path("pids"));
    let request = Request::script(framed(&heartbeat(&beats, &pids)), LONG);
    let ssh = sandbox.strict();
    let call = tokio::spawn(async move { ssh.execute(&request).await });
    let (file, list) = (beats.clone(), pids.clone());
    tokio::task::spawn_blocking(move || {
        wait_for_lines(&list, 2);
        wait_for_lines(&file, 2);
    })
    .await
    .unwrap();
    // While ssh runs, its private directory and file exist.
    let during = sandbox.generated_files();
    assert_eq!(during.len(), 1);
    let mode = |path: &Path| {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions())
            & 0o777
    };
    assert_eq!(mode(&during[0]), 0o700);
    assert_eq!(mode(&during[0].join("apvm-ssh.conf")), 0o600);

    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    let pids = recorded_pids(&pids);
    assert_eq!(pids.len(), 2, "the remote shell and its heartbeat");
    tokio::task::spawn_blocking(move || assert_all_gone(&pids, "a remote process"))
        .await
        .unwrap();
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn a_missing_ssh_program_is_ssh_missing_and_leaves_no_file() {
    let sandbox = Sandbox::new(&[]);
    let missing = sandbox.text("no-such-ssh");
    let request = Request::script(framed("true"), LONG);
    // With the inventory's ssh_config file: the spawn fails.
    let settings = SshSettings {
        binary: Some(missing.clone()),
        ..sandbox.settings()
    };
    let error = sandbox
        .transport(settings.clone(), HostKeyPolicy::Strict)
        .execute(&request)
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::SshMissing, "{error}");
    assert!(
        error
            .to_string()
            .starts_with(&format!("cannot run the ssh program '{missing}': "))
    );
    assert!(sandbox.generated_files().is_empty());
    // Without one: finding ssh's defaults fails first.
    let settings = SshSettings {
        ssh_config_file: None,
        ..settings
    };
    let error = sandbox
        .transport(settings, HostKeyPolicy::Strict)
        .execute(&request)
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::SshMissing, "{error}");
}

#[tokio::test]
async fn a_program_that_reports_no_defaults_is_not_a_usable_ssh() {
    let sandbox = Sandbox::new(&[]);
    for (program, reason) in [
        ("/usr/bin/true", "named no 'globalknownhostsfile"),
        ("/usr/bin/false", "'-F none -G' failed (exit status: 1)"),
    ] {
        if !Path::new(program).exists() {
            continue;
        }
        let settings = SshSettings {
            binary: Some(program.into()),
            ssh_config_file: None,
            ..sandbox.settings()
        };
        let ssh = sandbox.transport(settings, HostKeyPolicy::Strict);
        let error = ssh.config_text().await.unwrap_err();
        assert_eq!(error.code(), Code::SshMissing, "{error}");
        let message = error.to_string();
        assert!(
            message.starts_with(&format!(
                "the ssh program '{program}' is not a usable OpenSSH client: "
            )),
            "{message}"
        );
        assert!(message.contains(reason), "{message}");
        // The directory `-G` ran in is gone too.
        assert!(sandbox.generated_files().is_empty());
    }
}

/// A link to fake-ssh whose `-G` names the sandbox's `etc` directory as
/// ssh's configuration directory, so the system ssh_config is the
/// sandbox's (`etc/ssh_config`, `0600`), never this machine's.
///
/// # Arguments
///
/// * `sandbox` - The sandbox
/// * `text` - The system ssh_config's text
///
/// # Returns
///
/// The program (the link) and the system ssh_config.
fn with_system_config(sandbox: &Sandbox, text: &str) -> (PathBuf, PathBuf) {
    let etc = sandbox.path("etc");
    std::fs::create_dir(&etc).unwrap();
    let program = etc.join("fake-ssh-sysconf");
    std::os::unix::fs::symlink(fake_ssh(), &program).unwrap();
    let system = etc.join("ssh_config");
    write_private(&system, text);
    (program, system)
}

#[tokio::test]
async fn without_an_ssh_config_file_the_users_and_the_systems_are_included() {
    let sandbox = Sandbox::new(&[]);
    // Hermetic: this machine's system file could be refused, or (openSUSE)
    // be another file.
    let (program, system) = with_system_config(&sandbox, "");
    let home = sandbox.path("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let log = sandbox.text("log");
    write_private(&home.join(".ssh/config"), format!("#fake-ssh: log {log}\n"));
    let settings = SshSettings {
        ssh_config_file: None,
        known_hosts_file: None,
        binary: Some(program.to_str().unwrap().into()),
        ..sandbox.settings()
    };
    let ssh = sandbox.transport(settings, HostKeyPolicy::Strict);
    let config = ssh.config_text().await.unwrap();
    let home = home.to_str().unwrap();
    assert!(config.contains(&format!(
        "UserKnownHostsFile \"{home}/.ssh/known_hosts\" \"{home}/.ssh/known_hosts2\"\n"
    )));
    // ssh's own known_hosts files, so ssh's own `UpdateHostKeys` default
    // last (this fake client is like 10.3p1, which turns it on).
    assert!(config.ends_with(&format!(
        "Include \"{home}/.ssh/config\"\nInclude \"{}\"\n\
         # ssh's own default for its own known_hosts files (8.5+), unless set above.\n\
         UpdateHostKeys yes\n",
        system.display()
    )));
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // The directive came from the included user config.
    assert_eq!(logged_config(&sandbox.read("log")), config);
}

#[tokio::test]
async fn ssh_failures_are_classified_from_its_stderr() {
    let cases = [
        (
            "auth",
            SshFailureKind::AuthFailed,
            "deploy@example.com: Permission denied (publickey).",
        ),
        (
            "host-key-unknown",
            SshFailureKind::HostKeyUnknown,
            "No ED25519 host key is known for [example.com]:2222 and you have requested strict checking.",
        ),
        (
            "host-key-changed",
            SshFailureKind::HostKeyChanged,
            "Host key for [example.com]:2222 has changed and you have requested strict checking.",
        ),
        (
            "refused",
            SshFailureKind::Unreachable,
            "ssh: connect to host example.com port 2222: Connection refused",
        ),
        (
            "dns",
            SshFailureKind::Unreachable,
            "ssh: Could not resolve hostname example.com: Name or service not known",
        ),
        (
            "timeout",
            SshFailureKind::Timeout,
            "ssh: connect to host example.com port 2222: Connection timed out",
        ),
    ];
    for (class, kind, line) in cases {
        let directive = format!("fail {class}");
        let sandbox = Sandbox::new(&[&directive]);
        let marker = sandbox.path("ran");
        let code = format!("touch '{}'", marker.display());
        let done = sandbox
            .strict()
            .execute(&Request::script(framed(&code), LONG))
            .await
            .unwrap();
        assert_eq!(done.exit(), Exit::TransportFailed, "{class}");
        assert!(!marker.exists(), "{class}: nothing ran");
        let failure = classify(&done).unwrap();
        assert_eq!(
            (failure.kind(), failure.line()),
            (kind, Some(line)),
            "{class}"
        );
        let error = crate::Error::Ssh(failure);
        assert_eq!(error.code(), kind.code());
        assert!(error.to_string().ends_with(line), "{error}");
    }
}

#[tokio::test]
async fn a_connection_lost_after_the_script_started_is_never_an_auth_failure() {
    let sandbox = Sandbox::new(&["fail lost"]);
    let code = "echo 'deploy@x: Permission denied (publickey).' >&2";
    let done = sandbox
        .strict()
        .execute(&Request::script(framed(code), LONG))
        .await
        .unwrap();
    assert_eq!(done.exit(), Exit::TransportFailed);
    assert!(done.script_started());
    let failure = classify(&done).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::ConnectionLost);
    assert_eq!(
        failure.line(),
        Some("Connection to example.com closed by remote host.")
    );
    assert_eq!(failure.code(), Code::SshConnectionLost);
}

#[tokio::test]
async fn a_new_host_key_is_refused_strictly_and_recorded_with_accept_new() {
    let sandbox = Sandbox::new(&["host-key new"]);
    let request = Request::script(framed("echo ran"), LONG);
    let refused = sandbox.strict().execute(&request).await.unwrap();
    assert_eq!(
        classify(&refused).map(|f| f.kind()),
        Some(SshFailureKind::HostKeyUnknown)
    );
    assert!(!sandbox.path("known_hosts").exists());

    let ssh = sandbox.transport(sandbox.settings(), HostKeyPolicy::AcceptNew);
    assert!(
        ssh.config_text()
            .await
            .unwrap()
            .contains("StrictHostKeyChecking accept-new\n")
    );
    let done = ssh.execute(&request).await.unwrap();
    assert_eq!(done.payload(), Ok(&b"ran\n"[..]));
    assert!(
        sandbox
            .read("known_hosts")
            .starts_with("[example.com]:2222 ssh-ed25519 ")
    );
    let notices = host_key_notices(done.stderr());
    assert_eq!(notices.recorded, ["[example.com]:2222"]);
    assert!(!notices.not_saved);
}

#[tokio::test]
async fn unusable_local_files_are_refused_before_ssh_runs() {
    let sandbox = Sandbox::new(&[]);
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n"),
    );
    std::os::unix::fs::symlink("/dev/null", sandbox.path("null_kh")).unwrap();
    let cases = [
        (
            SshSettings {
                known_hosts_file: Some(sandbox.text("null_kh")),
                ..sandbox.settings()
            },
            "known_hosts_file: '",
            "is not a regular file, so host keys would not be kept",
        ),
        (
            SshSettings {
                identity_file: Some(sandbox.text("missing_key")),
                ..sandbox.settings()
            },
            "identity_file: '",
            "' does not exist",
        ),
        (
            SshSettings {
                ssh_config_file: Some(sandbox.text("missing_config")),
                ..sandbox.settings()
            },
            "ssh_config_file: '",
            "' does not exist",
        ),
    ];
    for host_keys in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
        for (settings, location, reason) in &cases {
            let ssh = sandbox.transport(settings.clone(), host_keys);
            let error = ssh
                .execute(&Request::script(framed("true"), LONG))
                .await
                .unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput, "{error}");
            let message = error.to_string();
            assert!(
                message.starts_with("invalid SSH settings of server 's': "),
                "{message}"
            );
            assert!(
                message.contains(location) && message.contains(reason),
                "{message}"
            );
        }
    }
    assert_eq!(sandbox.read("log"), "", "ssh never ran");
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn ssh_runs_with_english_messages_no_askpass_and_without_apvms_tokens() {
    let sandbox = Sandbox::new(&[]);
    let env = sandbox.text("env");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: env {env}\n"),
    );
    let done = sandbox
        .strict()
        .execute(&Request::script(framed("true"), LONG))
        .await
        .unwrap();
    assert_eq!(done.exit(), Exit::Status(0));
    let seen = sandbox.read("env");
    assert!(seen.lines().any(|line| line == "LC_ALL=C"), "{seen}");
    assert!(
        seen.lines().any(|line| line == "SSH_ASKPASS_REQUIRE=never"),
        "{seen}"
    );
    for token in ["GITHUB_TOKEN=", "GH_TOKEN="] {
        assert!(!seen.lines().any(|line| line.starts_with(token)), "{token}");
    }
}

#[tokio::test]
async fn only_the_program_must_be_shell_literal_not_the_generated_files_directory() {
    let sandbox = Sandbox::new(&[]);
    // A home with a space: `~/` makes the program's path unsafe for a
    // jump host's shell command.
    let spaced_home = sandbox.path("a b");
    let mut inventory = Inventory::new();
    let settings = SshSettings {
        binary: Some("~/bin/ssh".into()),
        ..sandbox.settings()
    };
    inventory.set_defaults(Defaults { ssh: settings }).unwrap();
    inventory
        .add_server(Server::new("s", "example.com"))
        .unwrap();
    let server = inventory.resolve_server("s").unwrap();
    let ssh = SshTransport::new(server.clone(), SshOptions::new().with_home(&spaced_home));
    let error = ssh.config_text().await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidInput);
    assert!(error.to_string().contains("binary: '"), "{error}");
    assert!(
        error
            .to_string()
            .contains("holds ' ': ssh pastes it unquoted"),
        "{error}"
    );

    // The generated files' directory may hold anything: ssh runs there and
    // gets the bare name.
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n"),
    );
    let odd = sandbox.path("gen dir $(touch pwned)");
    std::fs::create_dir(&odd).unwrap();
    let mut inventory = Inventory::new();
    inventory
        .set_defaults(Defaults {
            ssh: sandbox.settings(),
        })
        .unwrap();
    inventory
        .add_server(Server::new("s", "example.com"))
        .unwrap();
    let options = SshOptions::new()
        .with_home(sandbox.path("home"))
        .with_config_dir(&odd);
    let ssh = SshTransport::new(inventory.resolve_server("s").unwrap(), options);
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    assert_eq!(std::fs::read_dir(&odd).unwrap().count(), 0);
    assert!(!odd.join("pwned").exists() && !sandbox.path("pwned").exists());
    let log = sandbox.read("log");
    assert_eq!(&logged_args(&log)[..2], ["-F", "apvm-ssh.conf"], "{log}");
    assert_eq!(
        logged_cwd(&log).parent(),
        Some(std::fs::canonicalize(&odd).unwrap().as_path())
    );
}

#[tokio::test]
async fn the_system_config_is_found_once_per_transport() {
    let sandbox = Sandbox::new(&[]);
    let (program, system) = with_system_config(&sandbox, "");
    let settings = SshSettings {
        ssh_config_file: None,
        binary: Some(program.to_str().unwrap().into()),
        ..sandbox.settings()
    };
    let ssh = sandbox.transport(settings, HostKeyPolicy::Strict);
    let first = ssh.config_text().await.unwrap();
    assert_eq!(
        ssh.defaults
            .get()
            .and_then(|(_, d)| d.system_config.as_deref()),
        Some(system.as_path())
    );
    assert_eq!(ssh.config_text().await.unwrap(), first);
}

#[tokio::test]
async fn the_defaults_are_read_with_an_inventory_ssh_config_file_too() {
    // They decide the session options, whichever files are included.
    let sandbox = Sandbox::new(&[]);
    let ssh = sandbox.strict();
    assert!(
        ssh.config_text()
            .await
            .unwrap()
            .contains("\nSessionType default\n")
    );
    assert!(
        ssh.defaults
            .get()
            .is_some_and(|(_, d)| !d.pinned.is_empty())
    );
}

#[tokio::test]
async fn the_system_config_is_included_checked_and_left_out_when_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new(&[]);
    let log = sandbox.text("log");
    let (program, system) = with_system_config(&sandbox, &format!("#fake-ssh: log {log}\n"));
    let settings = SshSettings {
        ssh_config_file: None,
        binary: Some(program.to_str().unwrap().into()),
        ..sandbox.settings()
    };
    let ssh = sandbox.transport(settings, HostKeyPolicy::Strict);
    let user = sandbox.path("home").join(".ssh/config");
    let both = format!(
        "Include \"{}\"\nInclude \"{}\"\n",
        user.display(),
        system.display()
    );
    // apvm's own known_hosts file: ssh's default would be off.
    let config = ssh.config_text().await.unwrap();
    assert!(config.ends_with(&both), "{config}");
    assert!(!config.contains("UpdateHostKeys"), "{config}");
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // The directive came from the included system file.
    assert!(sandbox.read("log").contains("\nconfig:\n"));

    // One ssh would refuse to include (it reads it unchecked on its own).
    std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o664)).unwrap();
    std::fs::remove_file(sandbox.path("log")).unwrap();
    let error = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidInput, "{error}");
    assert!(
        error.to_string().contains(&format!(
            "ssh_config_file: is not set, and the system ssh_config '{}' is writable by its \
             group or others: ssh reads that file unchecked on its own, but refuses to include \
             it",
            system.display()
        )),
        "{error}"
    );
    assert_eq!(sandbox.read("log"), "", "ssh never ran");
    assert!(sandbox.generated_files().is_empty());

    // One this user cannot read: ssh skips it, so apvm leaves it out.
    if !rustix::process::geteuid().is_root() {
        std::fs::set_permissions(&system, std::fs::Permissions::from_mode(0o000)).unwrap();
        let config = ssh.config_text().await.unwrap();
        assert!(
            config.ends_with(&format!("Include \"{}\"\n", user.display())),
            "{config}"
        );
        assert!(!config.contains(&system.display().to_string()), "{config}");
    }
}

#[tokio::test]
async fn the_defaults_of_another_program_than_the_first_are_read_again() {
    // A relative `binary` names another file once apvm changes directory.
    let sandbox = Sandbox::new(&[]);
    let old = sandbox.path("fake-ssh-old");
    std::os::unix::fs::symlink(fake_ssh(), &old).unwrap();
    let ssh = sandbox.strict();
    let first = ssh.defaults(&fake_ssh()).await.unwrap();
    assert!(matches!(first, Cow::Borrowed(_)));
    assert_eq!(first.pinned.len(), 4);
    let other = ssh.defaults(&old).await.unwrap();
    assert!(matches!(other, Cow::Owned(_)));
    assert!(other.pinned.is_empty());
    // Like 8.2p1, it leaves `UpdateHostKeys` off; 10.3p1 turns it on.
    assert!(first.updates_host_keys && !other.updates_host_keys);
    // The first program's stay cached.
    let again = ssh.defaults(&fake_ssh()).await.unwrap();
    assert!(matches!(again, Cow::Borrowed(_)));
    assert_eq!(
        ssh.defaults.get().map(|(program, _)| program.clone()),
        Some(fake_ssh())
    );
}

#[tokio::test]
async fn concurrent_calls_get_directories_of_their_own() {
    let sandbox = Sandbox::new(&[]);
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n"),
    );
    let ssh = Arc::new(sandbox.strict());
    let calls: Vec<_> = (0..8)
        .map(|i| {
            let ssh = Arc::clone(&ssh);
            tokio::spawn(async move {
                let request = Request::script(framed(&format!("echo {i}")), LONG);
                ssh.execute(&request).await
            })
        })
        .collect();
    for (i, call) in calls.into_iter().enumerate() {
        let done = call.await.unwrap().unwrap();
        assert_eq!(done.payload(), Ok(format!("{i}\n").as_bytes()));
    }
    let log = sandbox.read("log");
    let mut dirs: Vec<&str> = log
        .lines()
        .filter_map(|line| line.strip_prefix("cwd: "))
        .collect();
    assert_eq!(dirs.len(), 8, "{log}");
    dirs.sort_unstable();
    dirs.dedup();
    assert_eq!(dirs.len(), 8, "every call had its own directory: {log}");
    assert!(sandbox.generated_files().is_empty());
}

/// A short private directory under `/tmp` for control sockets: the
/// sandbox's own path can be too long for one (`mux.rs`).
fn short_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("apvm-mux-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// [`Sandbox::strict`]'s transport with `options` changed by `change`.
///
/// # Arguments
///
/// * `sandbox` - The sandbox
/// * `settings` - The SSH defaults
/// * `change` - What to add to the options
fn transport_with(
    sandbox: &Sandbox,
    settings: SshSettings,
    change: impl FnOnce(SshOptions) -> SshOptions,
) -> SshTransport {
    let base = sandbox.transport(settings, HostKeyPolicy::Strict);
    let options = change(base.options().clone());
    SshTransport::new(base.server().clone(), options)
}

/// The `ControlPath` value of a generated file, unquoted.
///
/// # Arguments
///
/// * `config` - The file's text
fn control_path_of(config: &str) -> String {
    let line = config
        .lines()
        .find_map(|line| line.strip_prefix("ControlPath "));
    line.unwrap().trim_matches('"').to_string()
}

/// Every `config:` … `end` block of a fake-ssh log, in order.
///
/// # Arguments
///
/// * `log` - The log's text
fn logged_configs(log: &str) -> Vec<String> {
    log.split("config:\n")
        .skip(1)
        .filter_map(|block| block.split_once("end\n").map(|(text, _)| text.to_string()))
        .collect()
}

/// The `run:` lines of a fake-ssh log: each run's remote command, `-N`
/// for a master's starter (control commands left out).
///
/// # Arguments
///
/// * `log` - The log's text
fn logged_commands(log: &str) -> Vec<&str> {
    log.lines()
        .filter_map(|line| line.strip_prefix("run: "))
        .collect()
}

#[tokio::test]
async fn with_a_control_directory_a_starter_makes_the_master_and_calls_ride_it() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let control = short.path().join("mux");
    let ssh = logged_shared(&sandbox, &control, 4);
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // The directory was made private.
    let mode = std::fs::metadata(&control).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    // No master behind the socket, so the starter ran (`-N`: no remote
    // command), then the call.
    let log = sandbox.read("log");
    assert!(logged_controls(&log).is_empty(), "{log}");
    assert_eq!(logged_commands(&log), ["-N", "sh -s"], "{log}");
    let configs = logged_configs(&log);
    let [master, call] = configs.as_slice() else {
        panic!("{log}");
    };
    let path = control_path_of(call);
    let tag = path
        .strip_prefix(&format!("{}/", control.display()))
        .and_then(|name| name.strip_suffix("%C"))
        .unwrap();
    assert_eq!(tag.len(), 8, "{call}");
    // The starter becomes the master; the call never does.
    assert!(
        master.contains(&format!(
            "RemoteCommand none\nControlMaster auto\nControlPath \"{path}\"\n\
             ControlPersist 15s\nForwardAgent no\n"
        )),
        "{master}"
    );
    assert!(
        call.contains(&format!(
            "RemoteCommand none\nControlMaster no\nControlPath \"{path}\"\nForwardAgent no\n"
        )),
        "{call}"
    );
    assert_eq!(*call, ssh.config_text().await.unwrap());
    // Shared connections give a dead master up sooner.
    for text in [master, call] {
        assert!(
            text.contains("\nServerAliveInterval 10\nServerAliveCountMax 3\n"),
            "{text}"
        );
    }
    assert_eq!(
        ssh.multiplexing().await.unwrap(),
        Multiplexing::On {
            control_path: PathBuf::from(&path)
        }
    );
    // Other options (a new host key trusted), another socket.
    let trusting = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(&control)
            .with_host_keys(HostKeyPolicy::AcceptNew)
    });
    let other = control_path_of(&trusting.config_text().await.unwrap());
    assert_ne!(other, path);
    assert!(other.starts_with(&format!("{}/", control.display())));
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn a_live_master_is_used_without_a_starter() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    let _master = live_master(&ssh).await;
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    let log = sandbox.read("log");
    assert!(logged_controls(&log).is_empty(), "{log}");
    assert_eq!(logged_commands(&log), ["sh -s"], "{log}");
}

#[tokio::test]
async fn a_starter_that_cannot_connect_is_the_calls_failure() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: fail auth\n"),
    );
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    // The call did not run a second, doomed connection.
    assert_eq!(done.exit(), Exit::TransportFailed);
    assert!(!done.script_started());
    assert!(!done.stdin_written());
    assert_eq!(classify(&done).unwrap().kind(), SshFailureKind::AuthFailed);
    let log = sandbox.read("log");
    assert_eq!(logged_commands(&log), ["-N"], "{log}");
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn host_keys_the_starter_recorded_are_in_the_calls_stderr() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    write_private(&sandbox.path("ssh_config"), "#fake-ssh: host-key new\n");
    let ssh = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(short.path().join("mux"))
            .with_host_keys(HostKeyPolicy::AcceptNew)
    });
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // A direct connection's starter prints the server's key notice before
    // its master forks (verified on 10.3p1), so the caller sees it; fake-ssh
    // records the key on each run: the starter's notice first, then the
    // call's. (Through a jump host connections are not shared: ssh hides a
    // proxy's messages once `ControlPersist` is set.)
    let stderr = String::from_utf8_lossy(done.stderr().bytes()).into_owned();
    assert_eq!(stderr.matches("Permanently added").count(), 2, "{stderr}");
    assert_eq!(
        host_key_notices(done.stderr()).recorded,
        ["[example.com]:2222", "[example.com]:2222"]
    );
}

#[tokio::test]
async fn without_multiplexing_no_connection_is_shared_and_the_reason_is_told() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let control = short.path().join("mux");
    let not_shared = "RemoteCommand none\nControlMaster no\nControlPath none\nForwardAgent no\n";
    // No control directory (the library's default).
    let ssh = sandbox.strict();
    let text = ssh.config_text().await.unwrap();
    assert!(text.contains(not_shared));
    assert!(text.contains("\nServerAliveInterval 15\nServerAliveCountMax 4\n"));
    assert_eq!(
        ssh.multiplexing().await.unwrap(),
        Multiplexing::Off(NoMultiplexing::NoControlDir)
    );
    // The server's setting is off.
    let settings = SshSettings {
        multiplex: Some(false),
        ..sandbox.settings()
    };
    let off = transport_with(&sandbox, settings, |o| o.with_control_dir(&control));
    assert!(off.config_text().await.unwrap().contains(not_shared));
    assert_eq!(
        off.multiplexing().await.unwrap(),
        Multiplexing::Off(NoMultiplexing::Disabled)
    );
    assert!(!control.exists(), "nothing is made when it is off");
    // A directory too long for a socket path: the call still runs.
    let long = short
        .path()
        .join("x".repeat(super::super::mux::SOCKET_PATH_LIMIT));
    let too_long = transport_with(&sandbox, sandbox.settings(), |o| o.with_control_dir(&long));
    assert!(too_long.config_text().await.unwrap().contains(not_shared));
    assert!(matches!(
        too_long.multiplexing().await.unwrap(),
        Multiplexing::Off(NoMultiplexing::PathTooLong { .. })
    ));
    let done = too_long
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // A directory others may write.
    std::fs::create_dir(&control).unwrap();
    std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o777)).unwrap();
    let open = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(&control)
    });
    assert!(open.config_text().await.unwrap().contains(not_shared));
    let Multiplexing::Off(NoMultiplexing::UnusableControlDir { reason }) =
        open.multiplexing().await.unwrap()
    else {
        panic!("a directory others may write was used");
    };
    assert!(
        reason.contains("may be written by its group or others"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_server_runs_at_most_its_limit_of_calls_at_once() {
    let sandbox = Sandbox::new(&[]);
    let running = sandbox.path("running");
    std::fs::create_dir(&running).unwrap();
    let limits = ServerLimits::new(std::num::NonZeroUsize::new(2).unwrap());
    let ssh = Arc::new(transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_server_limits(limits)
    }));
    // Each call marks itself running, counts the marks, then leaves.
    let dir = running.to_str().unwrap();
    let calls: Vec<_> = (0..5)
        .map(|i| {
            let ssh = Arc::clone(&ssh);
            let code =
                format!("touch {dir}/{i}; sleep 0.4; ls {dir} | wc -l | tr -d ' '; rm {dir}/{i}");
            tokio::spawn(async move { ssh.execute(&Request::script(framed(&code), LONG)).await })
        })
        .collect();
    let started = std::time::Instant::now();
    let mut most = 0;
    for call in calls {
        let done = call.await.unwrap().unwrap();
        let payload = done.payload().unwrap();
        let seen: usize = String::from_utf8_lossy(payload).trim().parse().unwrap();
        most = most.max(seen);
    }
    // Never more than 2 at once (the count is a lower bound of what ran,
    // so it cannot pass wrongly) and, however the scheduler spreads them,
    // 5 calls of 0.4 s two at a time take at least 3 rounds.
    assert!((1..=2).contains(&most), "{most}");
    assert!(
        started.elapsed() >= Duration::from_millis(1200),
        "{:?}",
        started.elapsed()
    );
}

/// A transport like [`Sandbox::strict`] that logs to the sandbox's `log`,
/// shares connections through a short directory, and allows `per_server`
/// calls at once.
///
/// # Arguments
///
/// * `sandbox` - The sandbox
/// * `control` - The control directory
/// * `per_server` - The calls per server
fn logged_shared(sandbox: &Sandbox, control: &Path, per_server: usize) -> SshTransport {
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n"),
    );
    let limits = ServerLimits::new(std::num::NonZeroUsize::new(per_server).unwrap());
    transport_with(sandbox, sandbox.settings(), |o| {
        o.with_control_dir(control).with_server_limits(limits)
    })
}

/// The `control:` lines of a fake-ssh log: the `-O` commands ssh got.
///
/// # Arguments
///
/// * `log` - The log's text
fn logged_controls(log: &str) -> Vec<&str> {
    log.lines()
        .filter_map(|line| line.strip_prefix("control: "))
        .collect()
}

/// Directives for a fake-ssh that logs to `log` and, with `stall`, waits
/// 30 s before running anything, as a call stalled on a dead master does.
///
/// # Arguments
///
/// * `sandbox` - The sandbox
/// * `stall` - Whether calls stall
fn set_directives(sandbox: &Sandbox, stall: bool) {
    let log = sandbox.text("log");
    let stall = if stall { "#fake-ssh: stall 30\n" } else { "" };
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n{stall}"),
    );
}

#[tokio::test]
async fn a_shared_call_that_timed_out_before_its_script_stops_the_master() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(short.path().join("mux"))
    });
    set_directives(&sandbox, true);
    let _master = live_master(&ssh).await;
    let request = Request::script(framed("echo x"), Duration::from_millis(400));
    let done = ssh.execute(&request).await.unwrap();
    assert_eq!(done.exit(), Exit::TimedOut);
    assert!(!done.script_started());
    let log = sandbox.read("log");
    assert_eq!(logged_controls(&log), ["stop"], "{log}");
    // With the call's file and its whole destination, as ssh got them.
    let control = log.split("control: stop\n").nth(1).unwrap();
    let args = logged_args(control);
    assert_eq!(&args[2..4], ["-O", "stop"], "{args:?}");
    assert_eq!(
        &args[4..],
        ["-l", "deploy", "-p", "2222", "--", "example.com"]
    );
    assert_eq!(args[0], "-F");
    assert!(args[1].ends_with("/apvm-ssh.conf"), "{args:?}");
    assert!(sandbox.generated_files().is_empty());
}

#[tokio::test]
async fn the_master_is_left_alone_after_a_started_call_or_without_sharing() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    // Timed out after its script started: the master answered.
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    set_directives(&sandbox, false);
    let _master = live_master(&ssh).await;
    // Long enough for the nonce to come back on a slow machine.
    let request = Request::script(framed("sleep 30"), Duration::from_secs(3));
    let done = ssh.execute(&request).await.unwrap();
    assert_eq!(done.exit(), Exit::TimedOut);
    assert!(done.script_started());
    // Stalled, but not shared: there is no master.
    set_directives(&sandbox, true);
    let own = sandbox.strict();
    let stalled = Request::script(framed("echo x"), Duration::from_millis(400));
    let done = own.execute(&stalled).await.unwrap();
    assert_eq!(done.exit(), Exit::TimedOut);
    assert!(!done.script_started());
    let log = sandbox.read("log");
    assert!(logged_controls(&log).is_empty(), "{log}");
}

#[tokio::test]
async fn a_timed_out_or_dropped_call_frees_its_slot() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = Arc::new(logged_shared(&sandbox, &short.path().join("mux"), 1));
    // Timed out.
    let slow = Request::script(framed("sleep 30"), Duration::from_millis(300));
    assert_eq!(ssh.execute(&slow).await.unwrap().exit(), Exit::TimedOut);
    let quick = Request::script(framed("echo ok"), LONG);
    let next = tokio::time::timeout(Duration::from_secs(20), ssh.execute(&quick));
    assert_eq!(next.await.unwrap().unwrap().payload(), Ok(&b"ok\n"[..]));
    // Dropped while it runs.
    let running = {
        let ssh = Arc::clone(&ssh);
        tokio::spawn(async move {
            let slow = Request::script(framed("sleep 30"), LONG);
            ssh.execute(&slow).await
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    let next = tokio::time::timeout(Duration::from_secs(20), ssh.execute(&quick));
    assert_eq!(next.await.unwrap().unwrap().payload(), Ok(&b"ok\n"[..]));
}

#[tokio::test]
async fn an_opt_in_short_directory_replaces_one_too_long_for_a_socket() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let long = short
        .path()
        .join("x".repeat(super::super::mux::SOCKET_PATH_LIMIT));
    // A private stand-in for `/tmp` (production uses `/tmp` itself).
    let tmp = short_dir();
    let ssh = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(&long)
            .with_tmp_control_fallback()
            .with_fallback_tmp(tmp.path())
    });
    assert!(ssh.options().tmp_control_fallback());
    let uid = rustix::process::geteuid().as_raw();
    let expected = tmp.path().join(format!("apvm-mux-{uid}"));
    let Multiplexing::On { control_path } = ssh.multiplexing().await.unwrap() else {
        panic!("the short directory was not used");
    };
    assert_eq!(control_path.parent(), Some(expected.as_path()));
    let mode = std::fs::metadata(&expected).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    // A directory that fits is used as it is.
    let fits = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(short.path().join("mux"))
            .with_tmp_control_fallback()
            .with_fallback_tmp(tmp.path())
    });
    let Multiplexing::On { control_path } = fits.multiplexing().await.unwrap() else {
        panic!("off");
    };
    assert!(control_path.starts_with(short.path()));
    // Without the opt-in, a long directory turns multiplexing off.
    let off = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(&long).with_fallback_tmp(tmp.path())
    });
    assert!(matches!(
        off.multiplexing().await.unwrap(),
        Multiplexing::Off(NoMultiplexing::PathTooLong { .. })
    ));
}

/// `n` concurrent calls of `echo ok` on `ssh`, each with `timeout`.
///
/// # Arguments
///
/// * `ssh` - The transport
/// * `n` - How many
/// * `timeout` - Each call's timeout
async fn concurrently(ssh: &Arc<SshTransport>, n: usize, timeout: Duration) -> Vec<Execution> {
    let calls: Vec<_> = (0..n)
        .map(|_| {
            let ssh = Arc::clone(ssh);
            tokio::spawn(async move {
                let request = Request::script(framed("echo ok"), timeout);
                ssh.execute(&request).await
            })
        })
        .collect();
    let mut done = Vec::new();
    for call in calls {
        done.push(call.await.unwrap().unwrap());
    }
    done
}

#[tokio::test]
async fn concurrent_calls_start_exactly_one_master() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = Arc::new(logged_shared(&sandbox, &short.path().join("mux"), 4));
    for done in concurrently(&ssh, 4, LONG).await {
        assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    }
    let log = sandbox.read("log");
    let runs = logged_commands(&log);
    assert_eq!(runs.iter().filter(|run| **run == "-N").count(), 1, "{log}");
    assert_eq!(
        runs.iter().filter(|run| **run == "sh -s").count(),
        4,
        "{log}"
    );
}

#[tokio::test]
async fn calls_waiting_on_an_unreachable_server_share_its_failure() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = Arc::new(logged_shared(&sandbox, &short.path().join("mux"), 4));
    let log = sandbox.text("log");
    // Each connection takes 2 s to fail, as one to a server that drops
    // packets does until its connect timeout.
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: stall 2\n#fake-ssh: fail refused\n"),
    );
    let started = std::time::Instant::now();
    let results = concurrently(&ssh, 4, LONG).await;
    let took = started.elapsed();
    for done in &results {
        assert_eq!(done.exit(), Exit::TransportFailed);
        assert!(!done.script_started());
        assert_eq!(
            classify(done).map(|failure| failure.kind()),
            Some(SshFailureKind::Unreachable)
        );
    }
    // One starter met it (two, on a machine so slow that a call arrived
    // after the first had failed); one after another, four take 8 s.
    let log = sandbox.read("log");
    let starters = logged_commands(&log).len();
    assert!((1..=2).contains(&starters), "{log}");
    assert!(took < Duration::from_millis(7500), "{took:?}");
    // A later call tries again (the server may be back).
    let again = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(again.exit(), Exit::TransportFailed);
    assert_eq!(logged_commands(&sandbox.read("log")).len(), starters + 1);
}

/// The socket fake-ssh's `-G` names for `control_path` (`%C` expanded as
/// fake-ssh does).
///
/// # Arguments
///
/// * `control_path` - The pattern [`Multiplexing::On`] reports
fn fake_socket(control_path: &Path) -> PathBuf {
    let pattern = control_path.to_str().unwrap();
    PathBuf::from(pattern.replace("%C", "0123456789abcdef0123456789abcdef01234567"))
}

/// Whether `path` is a Unix socket.
///
/// # Arguments
///
/// * `path` - An existing path
fn socket_at(path: &Path) -> bool {
    let kind = std::fs::symlink_metadata(path).unwrap().file_type();
    std::os::unix::fs::FileTypeExt::is_socket(&kind)
}

#[tokio::test]
async fn a_stuck_masters_socket_is_removed_and_the_call_runs_on_its_own() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: master stuck\n"),
    );
    let Multiplexing::On { control_path } = ssh.multiplexing().await.unwrap() else {
        panic!("off");
    };
    let socket = fake_socket(&control_path);
    let _stuck = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // Nothing said hello behind the socket (3 s): its socket is gone, no
    // starter ran, and the call ran on its own (`ControlPath none`, so
    // fake-ssh prints no hello failure).
    assert!(!socket.exists());
    assert_eq!(logged_commands(&sandbox.read("log")), ["sh -s"]);
}

#[tokio::test]
async fn a_socket_that_is_not_this_configurations_is_never_removed() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: master stuck\n"),
    );
    // Another configuration's (another host key policy, another tag).
    let other = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(short.path().join("mux"))
            .with_host_keys(HostKeyPolicy::AcceptNew)
    });
    let Multiplexing::On { control_path } = other.multiplexing().await.unwrap() else {
        panic!("off");
    };
    let theirs = fake_socket(&control_path);
    let _theirs = std::os::unix::net::UnixListener::bind(&theirs).unwrap();
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: master stuck\n"),
    );
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    assert!(socket_at(&theirs));
}

#[tokio::test]
async fn a_jump_host_or_a_proxy_command_turns_multiplexing_off() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let not_shared = "RemoteCommand none\nControlMaster no\nControlPath none\nForwardAgent no\n";
    // A jump host from the inventory.
    let settings = SshSettings {
        proxy_jump: Some("hop.example".into()),
        ..sandbox.settings()
    };
    let jumped = transport_with(&sandbox, settings, |o| {
        o.with_control_dir(short.path().join("mux"))
    });
    assert_eq!(
        jumped.multiplexing().await.unwrap(),
        Multiplexing::Off(NoMultiplexing::Proxied)
    );
    assert!(jumped.config_text().await.unwrap().contains(not_shared));
    // A ProxyCommand from the user's ssh_config (as `ssh -G` resolves it).
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: proxy-command nc %h %p\n"),
    );
    let proxied = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(short.path().join("mux"))
    });
    assert_eq!(
        proxied.multiplexing().await.unwrap(),
        Multiplexing::Off(NoMultiplexing::Proxied)
    );
    let done = proxied
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // No master, no check: the call alone.
    let log = sandbox.read("log");
    assert!(logged_controls(&log).is_empty(), "{log}");
    assert_eq!(logged_commands(&log), ["sh -s"], "{log}");
}

#[tokio::test]
async fn a_master_an_old_client_hangs_on_is_bypassed_and_its_socket_removed() {
    // OpenSSH before 9.6 prints no hello failure: a call that meets a
    // stopped master hangs (verified on 8.9p1 and 9.2p1 by the audit).
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: master hang\n"),
    );
    let Multiplexing::On { control_path } = ssh.multiplexing().await.unwrap() else {
        panic!("off");
    };
    let socket = fake_socket(&control_path);
    let _stuck = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let started = std::time::Instant::now();
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // Nothing said hello (3 s): the call ran away from the socket.
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(!socket.exists());
    let log = sandbox.read("log");
    assert_eq!(logged_commands(&log), ["sh -s"], "{log}");
    let configs = logged_configs(&log);
    assert!(
        configs[0].contains("\nControlPath none\n"),
        "{}",
        configs[0]
    );
}

#[tokio::test]
async fn a_starter_that_lost_the_race_is_stopped_and_the_winners_master_used() {
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: race yes\n"),
    );
    let started = std::time::Instant::now();
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    // Not the 30 s the losing starter would wait as a plain `ssh -N`.
    assert!(started.elapsed() < Duration::from_secs(15));
    let log = sandbox.read("log");
    assert!(logged_controls(&log).is_empty(), "{log}");
    assert_eq!(logged_commands(&log), ["-N", "sh -s"], "{log}");
}

#[tokio::test]
async fn a_call_resolved_elsewhere_than_its_master_turns_multiplexing_off() {
    // OpenSSH 10's `Match command`: a call goes to another host than `-N`.
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    write_private(
        &sandbox.path("ssh_config"),
        "#fake-ssh: command-host elsewhere.example\n",
    );
    let ssh = transport_with(&sandbox, sandbox.settings(), |o| {
        o.with_control_dir(short.path().join("mux"))
    });
    assert_eq!(
        ssh.multiplexing().await.unwrap(),
        Multiplexing::Off(NoMultiplexing::SessionDependent)
    );
}

/// A stand-in for a live master on a Unix socket: says hello to every
/// connection, as OpenSSH's does, until dropped.
struct LiveMaster {
    /// Set to end the thread.
    done: Arc<std::sync::atomic::AtomicBool>,
    /// The thread that accepts.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LiveMaster {
    /// Listen on `path` (nonblocking, polled every 20 ms).
    ///
    /// # Arguments
    ///
    /// * `path` - The socket
    fn start(path: &Path) -> Self {
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = Arc::clone(&done);
        let thread = std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut client, _)) => {
                        let _ = std::io::Write::write_all(&mut client, b"\0\0\0\x15");
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Self {
            done,
            thread: Some(thread),
        }
    }
}

impl Drop for LiveMaster {
    /// End the thread.
    fn drop(&mut self) {
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A live stand-in master where `ssh`'s calls look for theirs.
///
/// # Arguments
///
/// * `ssh` - A transport that shares connections
async fn live_master(ssh: &SshTransport) -> LiveMaster {
    let found = ssh.multiplexing().await.unwrap();
    let Multiplexing::On { control_path } = found else {
        panic!("{found:?}");
    };
    LiveMaster::start(&fake_socket(&control_path))
}

#[tokio::test]
async fn a_call_that_meets_a_stuck_master_itself_has_its_socket_removed() {
    // The master said hello, then stopped before the call reached it: the
    // call prints ssh's own hello failure (9.6+) and runs on its own.
    let sandbox = Sandbox::new(&[]);
    let short = short_dir();
    let ssh = logged_shared(&sandbox, &short.path().join("mux"), 4);
    let master = live_master(&ssh).await;
    let log = sandbox.text("log");
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n#fake-ssh: master stuck\n"),
    );
    let Multiplexing::On { control_path } = ssh.multiplexing().await.unwrap() else {
        panic!("off");
    };
    let socket = fake_socket(&control_path);
    let done = ssh
        .execute(&Request::script(framed("echo ok"), LONG))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"ok\n"[..]));
    let stderr = String::from_utf8_lossy(done.stderr().bytes()).into_owned();
    assert!(stderr.contains("master hello exchange failed"), "{stderr}");
    assert!(!socket.exists());
    drop(master);
    // A remote program's copy of the line (no `\r`) removes nothing.
    let _again = LiveMaster::start(&socket);
    write_private(
        &sandbox.path("ssh_config"),
        format!("#fake-ssh: log {log}\n"),
    );
    let echoed = ssh
        .execute(&Request::script(
            framed("echo 'muxclient: master hello exchange failed' >&2"),
            LONG,
        ))
        .await
        .unwrap();
    assert!(echoed.exit().success());
    assert!(socket.exists());
}
