//! The SSH transport against the real OpenSSH server of the E2E harness
//! (`tests/e2e/sites/`, plan P3.3), through the system ssh client: a
//! connection that runs a script, an upload, an unknown host key (refused,
//! then recorded with `accept-new`), a changed host key, one marked
//! revoked, a wrong key, a
//! key file others can read, a refused port, a jump host that gets apvm's
//! options, an ssh_config that would take the session away, a timed-out
//! call through a jump host that leaves no ssh behind, and a transport
//! without `ssh_config_file`, which includes `~/.ssh/config` and the
//! system's ssh_config.
//!
//! Gated: every test returns at once unless `APVM_E2E_SITES=1` and the
//! harness address are set, as `eval "$(tests/e2e/sites/harness.sh env)"`
//! does (CI's `e2e-sites` job runs them after `harness.sh up`).
//!
//! Nothing here reads or writes the user's own SSH files: every transport
//! gets a temporary home, an empty ssh_config file of its own (or, in the
//! one test without, the temporary home's) and a known_hosts file of its
//! own, and the tests check the generated configuration names that file
//! before they run ssh with `accept-new`. One test reads (never writes)
//! this machine's system-wide ssh_config, as every transport without an
//! `ssh_config_file` does.
//!
//! Unix only: the harness runs on macOS and Linux clients.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use apvm_sites::remote::{
    FileMode, HostKeyPolicy, Identity, Nonce, Payload, Request, Script, SshFailureKind, SshOptions,
    SshTransport, Transport, UploadScript, WpCli, classify, host_key_notices,
};
use apvm_sites::{Defaults, Inventory, Server, SshSettings};

/// How long one call may take.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The harness address and credentials, from `harness.sh env`.
struct Harness {
    /// The target's address as seen from here.
    host: String,
    /// Its SSH port.
    port: u16,
    /// The key every harness user accepts.
    key: PathBuf,
    /// The target's host keys, as `[host]:port` entries.
    known_hosts: PathBuf,
}

impl Harness {
    /// The harness, when the E2E tests are enabled.
    fn get() -> Option<Self> {
        if std::env::var("APVM_E2E_SITES").as_deref() != Ok("1") {
            eprintln!("skipped: set APVM_E2E_SITES=1 (eval \"$(tests/e2e/sites/harness.sh env)\")");
            return None;
        }
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        Some(Self {
            host: var("APVM_E2E_SSH_HOST"),
            port: var("APVM_E2E_SSH_PORT").parse().unwrap(),
            key: var("APVM_E2E_SSH_KEY").into(),
            known_hosts: var("APVM_E2E_KNOWN_HOSTS").into(),
        })
    }

    /// The target as OpenSSH names it in known_hosts and its messages:
    /// `[host]:port`, or the bare host on port 22.
    fn known_hosts_name(&self) -> String {
        if self.port == 22 {
            return self.host.clone();
        }
        format!("[{}]:{}", self.host, self.port)
    }

    /// The harness's known_hosts entries, renamed to `name` (`localhost`
    /// for the target seen from itself).
    ///
    /// # Arguments
    ///
    /// * `name` - The host name the entries are for
    fn host_keys_as(&self, name: &str) -> String {
        std::fs::read_to_string(&self.known_hosts)
            .unwrap()
            .lines()
            .map(|line| {
                let (_, key) = line.split_once(' ').unwrap();
                format!("{name} {key}\n")
            })
            .collect()
    }
}

/// A temporary home with an empty ssh_config file, and where known_hosts
/// files and generated files go.
struct Sandbox {
    /// The directory (deleted on drop).
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A new sandbox under `/tmp`, whose path ssh may paste into a jump
    /// host's shell command.
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("apvm-ssh-e2e-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("ssh_config"), "").unwrap();
        // ssh includes no ssh_config file its group may write (any umask).
        std::fs::set_permissions(
            dir.path().join("ssh_config"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        Self { dir }
    }

    /// Replace the sandbox's ssh_config file (included by every transport)
    /// with `text`.
    ///
    /// # Arguments
    ///
    /// * `text` - The ssh_config text
    fn user_config(&self, text: &str) {
        std::fs::write(self.path("ssh_config"), text).unwrap();
    }

    /// `name` in the sandbox.
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// `name`'s path as text.
    ///
    /// # Arguments
    ///
    /// * `name` - A relative path
    fn text(&self, name: &str) -> String {
        self.path(name).to_str().unwrap().to_string()
    }

    /// A transport to `host` with `ssh` (the sandbox's ssh_config file
    /// added), checked to keep host keys in `known_hosts` only.
    ///
    /// # Arguments
    ///
    /// * `host` - The server's host
    /// * `ssh` - Its settings
    /// * `host_keys` - The host key policy
    async fn transport(
        &self,
        host: &str,
        ssh: SshSettings,
        host_keys: HostKeyPolicy,
    ) -> SshTransport {
        let known_hosts = ssh.known_hosts_file.clone().unwrap();
        let mut inventory = Inventory::new();
        let defaults = SshSettings {
            ssh_config_file: Some(self.text("ssh_config")),
            connect_timeout_secs: Some(10),
            ..SshSettings::default()
        };
        inventory.set_defaults(Defaults { ssh: defaults }).unwrap();
        let mut server = Server::new("harness", host);
        server.ssh = ssh;
        inventory.add_server(server).unwrap();
        let options = SshOptions::new()
            .with_host_keys(host_keys)
            .with_home(self.path("home"))
            .with_config_dir(self.dir.path());
        let transport = SshTransport::new(inventory.resolve_server("harness").unwrap(), options);
        // Never let ssh fall back to the user's own known_hosts.
        let config = transport.config_text().await.unwrap();
        assert!(
            config.contains(&format!("\nUserKnownHostsFile \"{known_hosts}\"\n")),
            "{config}"
        );
        assert!(!config.contains("/.ssh/"), "{config}");
        transport
    }
}

/// The settings of user `deploy` on the harness with `key` and
/// `known_hosts`.
///
/// # Arguments
///
/// * `harness` - The harness
/// * `key` - The private key
/// * `known_hosts` - The known_hosts file
fn deploy(harness: &Harness, key: &Path, known_hosts: &str) -> SshSettings {
    SshSettings {
        user: Some("deploy".into()),
        port: Some(harness.port),
        identity_file: Some(key.to_str().unwrap().into()),
        known_hosts_file: Some(known_hosts.into()),
        ..SshSettings::default()
    }
}

/// A `wp cli version` script on the harness's `single` site, as the login
/// user.
fn cli_version() -> Request {
    let wp = WpCli::new(
        &["wp".into()],
        FileMode::new(0o755).unwrap(),
        "/var/www/single",
    )
    .unwrap()
    .args(["cli", "version"])
    .unwrap();
    Request::script(Script::wp(Nonce::random(), &Identity::Login, &wp), TIMEOUT)
}

/// Assert `transport` runs [`cli_version`] and gets WP-CLI's answer.
///
/// # Arguments
///
/// * `transport` - The transport
async fn assert_runs(transport: &SshTransport) {
    let done = transport.execute(&cli_version()).await.unwrap();
    let stderr = String::from_utf8_lossy(done.stderr().bytes()).into_owned();
    assert!(done.exit().success(), "{:?}: {stderr}", done.exit());
    assert_eq!(done.payload(), Ok(&b"WP-CLI 2.12.0\n"[..]), "{stderr}");
}

/// Assert `transport` fails in ssh with `kind`, and that nothing ran.
///
/// # Arguments
///
/// * `transport` - The transport
/// * `kind` - The expected class
/// * `phrase` - Part of the line that shows it
async fn assert_fails(transport: &SshTransport, kind: SshFailureKind, phrase: &str) {
    let done = transport.execute(&cli_version()).await.unwrap();
    assert!(!done.script_started());
    let failure = classify(&done).unwrap();
    assert_eq!(failure.kind(), kind, "{failure}");
    assert!(
        failure.line().unwrap_or_default().contains(phrase),
        "{failure}"
    );
}

#[tokio::test]
async fn a_script_runs_over_ssh_and_its_generated_file_is_gone_after() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .transport(
            &harness.host,
            deploy(&harness, &harness.key, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    assert_runs(&ssh).await;
    let leftovers: Vec<_> = std::fs::read_dir(sandbox.dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("apvm-ssh-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn without_an_ssh_config_file_the_users_and_the_systems_are_read() {
    let Some(harness) = Harness::get() else {
        return;
    };
    // The one test that reads this machine's system-wide ssh_config, as a
    // transport does by default: real ssh must accept apvm's `Include` of it.
    let sandbox = Sandbox::new();
    let ssh_dir = sandbox.path("home/.ssh");
    std::fs::create_dir(&ssh_dir).unwrap();
    // An alias only the (sandbox's) user ssh_config knows.
    let user_config = ssh_dir.join("config");
    std::fs::write(
        &user_config,
        format!(
            "Host apvm-e2e-alias\n  HostName {}\n  Port {}\n",
            harness.host, harness.port
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &user_config,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let mut inventory = Inventory::new();
    let mut server = Server::new("harness", "apvm-e2e-alias");
    server.ssh = SshSettings {
        user: Some("deploy".into()),
        identity_file: Some(harness.key.to_str().unwrap().into()),
        known_hosts_file: Some(harness.known_hosts.to_str().unwrap().into()),
        connect_timeout_secs: Some(10),
        ..SshSettings::default()
    };
    inventory.add_server(server).unwrap();
    let options = SshOptions::new()
        .with_home(sandbox.path("home"))
        .with_config_dir(sandbox.dir.path());
    let ssh = SshTransport::new(inventory.resolve_server("harness").unwrap(), options);
    let config = ssh.config_text().await.unwrap();
    let includes: Vec<&str> = config
        .lines()
        .filter_map(|line| line.strip_prefix("Include "))
        .collect();
    assert_eq!(includes.len(), 2, "{config}");
    assert_eq!(includes[0], format!("\"{}\"", user_config.display()));
    assert!(includes[1].ends_with("/ssh_config\""), "{config}");
    assert_runs(&ssh).await;
}

#[tokio::test]
async fn with_sshs_own_known_hosts_files_ssh_learns_the_servers_other_keys_as_alone() {
    let Some(harness) = Harness::get() else {
        return;
    };
    // Plain ssh 8.5+ turns `UpdateHostKeys` on when it uses its own
    // known_hosts files, so it records the other host keys a server proves
    // with the known one; apvm names those files, which would turn it off,
    // so it ends the file with ssh's default (audit 5).
    let sandbox = Sandbox::new();
    let ssh_dir = sandbox.path("home/.ssh");
    std::fs::create_dir(&ssh_dir).unwrap();
    let known = ssh_dir.join("known_hosts");
    let known_keys = std::fs::read_to_string(&harness.known_hosts).unwrap();
    let ed25519 = known_keys
        .lines()
        .find(|line| line.contains(" ssh-ed25519 "))
        .map(|line| format!("{line}\n"))
        .unwrap();
    std::fs::write(&known, &ed25519).unwrap();
    let mut inventory = Inventory::new();
    let mut server = Server::new("harness", &harness.host);
    server.ssh = SshSettings {
        known_hosts_file: None,
        ssh_config_file: Some(sandbox.text("ssh_config")),
        connect_timeout_secs: Some(10),
        ..deploy(&harness, &harness.key, "unused")
    };
    inventory.add_server(server).unwrap();
    let options = SshOptions::new()
        .with_home(sandbox.path("home"))
        .with_config_dir(sandbox.dir.path());
    let ssh = SshTransport::new(inventory.resolve_server("harness").unwrap(), options);
    let config = ssh.config_text().await.unwrap();
    assert!(
        config.contains(&format!(
            "\nUserKnownHostsFile \"{}\" \"{}2\"\n",
            known.display(),
            known.display()
        )),
        "{config}"
    );
    // This client's own default (off before 8.5, with or without apvm).
    let defaults = Command::new("ssh")
        .args(["-T", "-F", "none", "-G", "apvm-e2e.invalid"])
        .output()
        .unwrap();
    let updates = String::from_utf8_lossy(&defaults.stdout)
        .lines()
        .any(|line| line == "updatehostkeys true");
    assert_eq!(
        config.ends_with("\nUpdateHostKeys yes\n"),
        updates,
        "{config}"
    );
    assert_runs(&ssh).await;
    let recorded = std::fs::read_to_string(&known).unwrap();
    if updates {
        let kinds = |text: &str| text.lines().count();
        assert_eq!(kinds(&recorded), kinds(&known_keys), "{recorded}");
        assert!(recorded.starts_with(&ed25519), "{recorded}");
    } else {
        assert_eq!(recorded, ed25519);
    }
}

#[tokio::test]
async fn an_upload_reaches_the_server_byte_for_byte() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .transport(
            &harness.host,
            deploy(&harness, &harness.key, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    // `harness.sh reset` removes /tmp/apvm-e2e-*.
    let destination = format!("/tmp/apvm-e2e-ssh-upload-{}", std::process::id());
    let bytes: Vec<u8> = (0..=255u8).cycle().take(1_000_000).collect();
    let script = UploadScript::tee(Nonce::random(), &Identity::Login, &destination).unwrap();
    let upload = Request::upload(script, Payload::Bytes(Arc::from(bytes.clone())), TIMEOUT);
    let done = ssh.execute(&upload).await.unwrap();
    assert!(
        done.exit().success(),
        "{:?}",
        String::from_utf8_lossy(done.stderr().bytes())
    );
    assert!(done.stdin_written());

    // Read it back: its size through WP-CLI's PHP, as the same user.
    let wp = WpCli::new(
        &["wp".into()],
        FileMode::new(0o755).unwrap(),
        "/var/www/single",
    )
    .unwrap()
    .args([
        "eval",
        &format!("echo filesize('{destination}');"),
        "--skip-wordpress",
    ])
    .unwrap();
    let size = ssh
        .execute(&Request::script(
            Script::wp(Nonce::random(), &Identity::Login, &wp),
            TIMEOUT,
        ))
        .await
        .unwrap();
    assert_eq!(size.payload(), Ok(&b"1000000"[..]));
}

#[tokio::test]
async fn an_unknown_host_key_is_refused_then_recorded_with_accept_new() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let known = sandbox.text("known_hosts");
    std::fs::write(&known, "").unwrap();
    let settings = deploy(&harness, &harness.key, &known);
    let strict = sandbox
        .transport(&harness.host, settings.clone(), HostKeyPolicy::Strict)
        .await;
    assert_fails(
        &strict,
        SshFailureKind::HostKeyUnknown,
        "host key is known for",
    )
    .await;
    assert_eq!(std::fs::read_to_string(&known).unwrap(), "");

    let trusting = sandbox
        .transport(&harness.host, settings, HostKeyPolicy::AcceptNew)
        .await;
    let done = trusting.execute(&cli_version()).await.unwrap();
    assert!(done.exit().success());
    let name = harness.known_hosts_name();
    assert_eq!(recorded_hosts(done.stderr()), std::slice::from_ref(&name));
    assert!(!std::fs::read_to_string(&known).unwrap().is_empty());
    // Recorded for good: the strict transport now connects.
    assert_runs(&strict).await;
}

#[tokio::test]
async fn a_changed_host_key_is_refused_even_with_accept_new() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let other = generate_key(&sandbox, "other");
    let public = std::fs::read_to_string(other.with_extension("pub")).unwrap();
    let mut parts = public.split_whitespace();
    let (kind, key) = (parts.next().unwrap(), parts.next().unwrap());
    let known = sandbox.text("changed_known_hosts");
    std::fs::write(
        &known,
        format!("{} {kind} {key}\n", harness.known_hosts_name()),
    )
    .unwrap();
    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
        let ssh = sandbox
            .transport(
                &harness.host,
                deploy(&harness, &harness.key, &known),
                policy,
            )
            .await;
        assert_fails(&ssh, SshFailureKind::HostKeyChanged, "has changed").await;
    }
}

#[tokio::test]
async fn a_host_key_marked_revoked_is_refused_as_a_changed_key() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    // The target's keys, each also marked `@revoked`.
    let keys = std::fs::read_to_string(&harness.known_hosts).unwrap();
    let revoked: String = keys
        .lines()
        .map(|line| format!("@revoked {line}\n"))
        .collect();
    let known = sandbox.text("revoked_known_hosts");
    std::fs::write(&known, format!("{keys}{revoked}")).unwrap();
    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew] {
        let ssh = sandbox
            .transport(
                &harness.host,
                deploy(&harness, &harness.key, &known),
                policy,
            )
            .await;
        assert_fails(
            &ssh,
            SshFailureKind::HostKeyChanged,
            "was revoked and you have requested strict checking",
        )
        .await;
    }
}

#[tokio::test]
async fn a_wrong_key_is_an_auth_failure() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let wrong = generate_key(&sandbox, "wrong");
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .transport(
            &harness.host,
            deploy(&harness, &wrong, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    assert_fails(
        &ssh,
        SshFailureKind::AuthFailed,
        "Permission denied (publickey)",
    )
    .await;
}

#[tokio::test]
async fn a_key_file_others_can_read_is_an_auth_failure_that_says_why() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    // The right key, but ssh ignores a key file its group or others may read.
    let open = sandbox.path("open_key");
    std::fs::copy(&harness.key, &open).unwrap();
    std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .transport(
            &harness.host,
            deploy(&harness, &open, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    assert_fails(
        &ssh,
        SshFailureKind::AuthFailed,
        &format!("Permissions 0644 for '{}' are too open.", open.display()),
    )
    .await;
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let settings = SshSettings {
        port: Some(1),
        ..deploy(&harness, &harness.key, &known)
    };
    let ssh = sandbox
        .transport(&harness.host, settings, HostKeyPolicy::Strict)
        .await;
    assert_fails(&ssh, SshFailureKind::Unreachable, "Connection refused").await;
}

#[tokio::test]
async fn a_jump_host_gets_apvms_host_key_checks_and_key() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    // The target is its own jump host: hop to it, then to `localhost:22`
    // as seen from inside it.
    let hop = format!("deploy@{}:{}", harness.host, harness.port);
    let destination_only = sandbox.text("destination_known_hosts");
    std::fs::write(&destination_only, harness.host_keys_as("localhost")).unwrap();
    let both = sandbox.text("both_known_hosts");
    let hop_keys = std::fs::read_to_string(&harness.known_hosts).unwrap();
    std::fs::write(
        &both,
        format!("{hop_keys}{}", harness.host_keys_as("localhost")),
    )
    .unwrap();
    let jump = |known: &str| SshSettings {
        user: Some("deploy".into()),
        identity_file: Some(harness.key.to_str().unwrap().into()),
        known_hosts_file: Some(known.into()),
        proxy_jump: Some(hop.clone()),
        ..SshSettings::default()
    };
    let ssh = sandbox
        .transport("localhost", jump(&both), HostKeyPolicy::Strict)
        .await;
    assert_runs(&ssh).await;

    // Without the hop's key in apvm's file the hop refuses, with the
    // message strict checking prints: the hop read apvm's options (with
    // ssh's own defaults it would only say `Host key verification failed.`).
    let ssh = sandbox
        .transport("localhost", jump(&destination_only), HostKeyPolicy::Strict)
        .await;
    let name = harness.known_hosts_name();
    // (The key type it names is the client's preferred one: ED25519 from
    // 8.5, ECDSA on 7.6p1 and 8.2p1.)
    assert_fails(
        &ssh,
        SshFailureKind::HostKeyUnknown,
        &format!(" host key is known for {name} and you have requested strict checking"),
    )
    .await;

    // With accept-new, the hop's key and the destination's land in apvm's
    // file.
    let fresh = sandbox.text("fresh_known_hosts");
    std::fs::write(&fresh, "").unwrap();
    let ssh = sandbox
        .transport("localhost", jump(&fresh), HostKeyPolicy::AcceptNew)
        .await;
    let done = ssh.execute(&cli_version()).await.unwrap();
    assert!(
        done.exit().success(),
        "{:?}",
        String::from_utf8_lossy(done.stderr().bytes())
    );
    assert_eq!(
        recorded_hosts(done.stderr()),
        [name, "localhost".to_string()]
    );
    let recorded = std::fs::read_to_string(&fresh).unwrap();
    assert_eq!(recorded.lines().count(), 2, "{recorded}");
}

/// The settings that reach the harness's own sshd as `localhost` through
/// the target as jump host, with `known` as the known_hosts file.
///
/// # Arguments
///
/// * `harness` - The harness
/// * `known` - The known_hosts file
fn through_jump(harness: &Harness, known: &str) -> SshSettings {
    SshSettings {
        user: Some("deploy".into()),
        identity_file: Some(harness.key.to_str().unwrap().into()),
        known_hosts_file: Some(known.into()),
        proxy_jump: Some(format!("deploy@{}:{}", harness.host, harness.port)),
        ..SshSettings::default()
    }
}

/// A known_hosts file in `sandbox` that knows the target directly and as
/// every name in `names` (seen from itself).
///
/// # Arguments
///
/// * `harness` - The harness
/// * `sandbox` - Where the file goes
/// * `names` - Further names of the target
fn known_as(harness: &Harness, sandbox: &Sandbox, names: &[&str]) -> String {
    let mut text = std::fs::read_to_string(&harness.known_hosts).unwrap();
    for name in names {
        text.push_str(&harness.host_keys_as(name));
    }
    let path = sandbox.text(&format!("known_{}", names.join("_")));
    std::fs::write(&path, text).unwrap();
    path
}

#[tokio::test]
async fn an_ssh_config_cannot_take_the_session_away() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    // Each of these, unchecked, broke a call (OpenSSH 10.3p1): no command
    // and a hang, a script never sent (exit 0, nothing ran), ssh detached
    // from apvm, or a command killed after a second without output. The
    // first three need OpenSSH 8.7, ChannelTimeout 9.6, like apvm's
    // overrides: the ssh_config sets what this client accepts.
    let session = "Host *\n  SessionType none\n  StdinNull yes\n  ForkAfterAuthentication yes\n";
    let with_channel = format!("{session}  ChannelTimeout session=1s\n");
    let accepted = |text: &str| {
        sandbox.user_config(text);
        let config = sandbox.path("ssh_config");
        let probe = Command::new("ssh")
            .args(["-F", config.to_str().unwrap(), "-G", "x"])
            .output()
            .unwrap();
        probe.status.success()
    };
    let pins = if accepted(&with_channel) {
        "\nSessionType default\nStdinNull no\nForkAfterAuthentication no\nChannelTimeout none\n"
    } else if accepted(session) {
        "\nSessionType default\nStdinNull no\nForkAfterAuthentication no\nInclude "
    } else {
        eprintln!("skipped: this ssh predates SessionType (OpenSSH 8.7)");
        return;
    };
    let known = known_as(&harness, &sandbox, &["localhost"]);
    let direct = sandbox
        .transport(
            &harness.host,
            deploy(&harness, &harness.key, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    let config = direct.config_text().await.unwrap();
    assert!(config.contains(pins), "{config}");
    assert_runs(&direct).await;
    // Three silent seconds outlive the ssh_config's channel timeout.
    let quiet = WpCli::new(
        &["wp".into()],
        FileMode::new(0o755).unwrap(),
        "/var/www/single",
    )
    .unwrap()
    .args(["eval", "sleep(3); echo 'slept';", "--skip-wordpress"])
    .unwrap();
    let done = direct
        .execute(&Request::script(
            Script::wp(Nonce::random(), &Identity::Login, &quiet),
            TIMEOUT,
        ))
        .await
        .unwrap();
    assert_eq!(done.payload(), Ok(&b"slept"[..]), "{:?}", done.stderr());
    // An upload's stdin arrives whole.
    let destination = format!("/tmp/apvm-e2e-ssh-session-{}", std::process::id());
    let bytes: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    let script = UploadScript::tee(Nonce::random(), &Identity::Login, &destination).unwrap();
    let upload = Request::upload(script, Payload::Bytes(Arc::from(bytes)), TIMEOUT);
    let done = direct.execute(&upload).await.unwrap();
    assert!(done.exit().success(), "{:?}", done.stderr());
    let size = WpCli::new(
        &["wp".into()],
        FileMode::new(0o755).unwrap(),
        "/var/www/single",
    )
    .unwrap()
    .args([
        "eval",
        &format!("echo filesize('{destination}');"),
        "--skip-wordpress",
    ])
    .unwrap();
    let size = direct
        .execute(&Request::script(
            Script::wp(Nonce::random(), &Identity::Login, &size),
            TIMEOUT,
        ))
        .await
        .unwrap();
    assert_eq!(size.payload(), Ok(&b"100000"[..]));
    // A jump host still forwards: its `-W` keeps its own session type.
    let jumped = sandbox
        .transport(
            "localhost",
            through_jump(&harness, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    assert_runs(&jumped).await;
}

#[tokio::test]
async fn a_timed_out_call_through_a_jump_host_leaves_no_ssh_behind() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    // `127.0.0.2` (the target's loopback, seen from itself) appears only in
    // this test's jump host process: `ssh … -W '[127.0.0.2]:22' <hop>`.
    let known = known_as(&harness, &sandbox, &["127.0.0.2"]);
    let ssh = sandbox
        .transport(
            "127.0.0.2",
            through_jump(&harness, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    let pattern = r"-W \[127\.0\.0\.2\]:22";
    let sleep = WpCli::new(
        &["wp".into()],
        FileMode::new(0o755).unwrap(),
        "/var/www/single",
    )
    .unwrap()
    .args(["eval", "sleep(30);", "--skip-wordpress"])
    .unwrap();
    let request = Request::script(
        Script::wp(Nonce::random(), &Identity::Login, &sleep),
        Duration::from_secs(4),
    );
    let watcher = tokio::task::spawn_blocking(move || {
        // The jump host's ssh shows up while the call runs.
        (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            pgrep(pattern)
        })
    });
    let done = ssh.execute(&request).await.unwrap();
    assert_eq!(done.exit(), apvm_sites::remote::Exit::TimedOut);
    assert!(watcher.await.unwrap(), "the jump host's ssh was never seen");
    assert!(!pgrep(pattern), "a jump host's ssh outlived the call");
}

/// Whether a process whose command line matches `pattern` runs.
///
/// # Arguments
///
/// * `pattern` - An extended regular expression for `pgrep -f`
fn pgrep(pattern: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", "--", pattern])
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// The hosts whose new key ssh recorded, by name: before OpenSSH 8.5
/// (`CheckHostIP yes` by default) ssh names a host by name and address
/// (`target,172.19.0.3`), which this drops.
///
/// # Arguments
///
/// * `stderr` - ssh's stderr
fn recorded_hosts(stderr: &apvm_sites::remote::Captured) -> Vec<String> {
    host_key_notices(stderr)
        .recorded
        .iter()
        .map(|host| host.split(',').next().unwrap_or_default().to_string())
        .collect()
}

/// A new passphrase-less ed25519 key `name` in the sandbox.
///
/// # Arguments
///
/// * `sandbox` - The sandbox
/// * `name` - The key's file name
fn generate_key(sandbox: &Sandbox, name: &str) -> PathBuf {
    let path = sandbox.path(name);
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "apvm-e2e", "-f"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    path
}
