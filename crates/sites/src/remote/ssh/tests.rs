//! The SSH transport end to end through the hermetic `fake-ssh`
//! (`tests/support/fake-remote/fake-ssh`, Unix only): the argv and the
//! generated file ssh gets, the file's lifetime on every path, the checks
//! that run before ssh, a missing or unusable ssh, and the failure classes.
//!
//! `fake-ssh` is a committed script that tests never write, so exec'ing it
//! cannot hit Linux's "Text file busy" race. Its behavior comes from
//! `#fake-ssh:` lines in each test's own ssh_config file.

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
    Exit, Identity, Nonce, Payload, Script, SshFailureKind, UploadScript, classify,
    host_key_notices,
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
