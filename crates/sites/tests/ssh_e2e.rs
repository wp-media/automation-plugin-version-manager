//! The SSH transport against the real OpenSSH server of the E2E harness
//! (`tests/e2e/sites/`, plan P3.3), through the system ssh client: a
//! connection that runs a script, an upload, an unknown host key (refused,
//! then recorded with `accept-new`), a changed host key, one marked
//! revoked, a wrong key, a
//! key file others can read, a refused port, a jump host that gets apvm's
//! options, an ssh_config that would take the session away, a timed-out
//! call through a jump host that leaves no ssh behind, a transport
//! without `ssh_config_file`, which includes `~/.ssh/config` and the
//! system's ssh_config, and multiplexing (P3.4): calls that share one
//! connection, a master `ssh -O check` finds live after the first call,
//! masters for a jump host and its server, a master that dies during a
//! call, and one whose connection dies silently (a local relay).
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
    Exit, FileMode, HostKeyPolicy, Identity, Multiplexing, NoMultiplexing, Nonce, Payload, Request,
    Script, SshFailureKind, SshOptions, SshTransport, Transport, UploadScript, WpCli, classify,
    host_key_notices,
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

impl Sandbox {
    /// [`Sandbox::transport`] with multiplexing sockets in the sandbox's
    /// `mux` directory (short enough for a socket path under `/tmp`).
    ///
    /// # Arguments
    ///
    /// * `host` - The server's host
    /// * `ssh` - Its settings
    async fn shared(&self, host: &str, ssh: SshSettings) -> SshTransport {
        let base = self.transport(host, ssh, HostKeyPolicy::Strict).await;
        let options = base.options().clone().with_control_dir(self.path("mux"));
        let shared = SshTransport::new(base.server().clone(), options);
        let Multiplexing::On { control_path } = shared.multiplexing().await.unwrap() else {
            panic!("multiplexing is off");
        };
        assert!(
            control_path.starts_with(self.path("mux")),
            "{control_path:?}"
        );
        shared
    }

    /// The control sockets in the sandbox's `mux` directory.
    fn sockets(&self) -> Vec<String> {
        std::fs::read_dir(self.path("mux"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}

/// Kills, when dropped, every ssh process whose command line names the
/// sandbox: the masters a multiplexed test leaves (`ControlPersist`), and
/// the jump host processes they keep.
struct Masters {
    /// The sandbox's path.
    pattern: String,
}

impl Masters {
    /// The guard for `sandbox`'s processes.
    ///
    /// # Arguments
    ///
    /// * `sandbox` - The sandbox
    fn of(sandbox: &Sandbox) -> Self {
        Self {
            pattern: sandbox.dir.path().to_str().unwrap().to_string(),
        }
    }
}

impl Drop for Masters {
    /// Kill them (best effort: none may be left).
    fn drop(&mut self) {
        // `-KILL`: a stopped master ignores anything else until resumed.
        let _ = Command::new("pkill")
            .args(["-KILL", "-f", "--", &self.pattern])
            .status();
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

/// A script that prints `SSH_CONNECTION` (the client's address and port
/// as the server sees them: the same for every call over one connection).
fn ssh_connection() -> Request {
    php("echo getenv('SSH_CONNECTION');", TIMEOUT)
}

/// A script that runs `code` in WP-CLI's PHP, without WordPress, as the
/// login user on the harness's `single` site.
///
/// # Arguments
///
/// * `code` - PHP code
/// * `timeout` - The call's timeout
fn php(code: &str, timeout: Duration) -> Request {
    let wp = WpCli::new(
        &["wp".into()],
        FileMode::new(0o755).unwrap(),
        "/var/www/single",
    )
    .unwrap()
    .args(["eval", code, "--skip-wordpress"])
    .unwrap();
    Request::script(Script::wp(Nonce::random(), &Identity::Login, &wp), timeout)
}

/// The payload of `request` on `transport`, which must succeed.
///
/// # Arguments
///
/// * `transport` - The transport
/// * `request` - The call
async fn payload_of(transport: &SshTransport, request: &Request) -> String {
    let done = transport.execute(request).await.unwrap();
    let stderr = String::from_utf8_lossy(done.stderr().bytes()).into_owned();
    assert!(done.exit().success(), "{:?}: {stderr}", done.exit());
    String::from_utf8_lossy(done.payload().unwrap()).into_owned()
}

/// Run `ssh -O <command>` with `transport`'s generated file and the
/// destination arguments of `deploy` on the harness: what ssh says, and
/// whether it succeeded.
///
/// # Arguments
///
/// * `sandbox` - Where the file is written
/// * `harness` - The harness
/// * `transport` - The transport
/// * `command` - `check` or `exit`
async fn control(
    sandbox: &Sandbox,
    harness: &Harness,
    transport: &SshTransport,
    command: &str,
) -> (bool, String) {
    let file = sandbox.path("control.conf");
    std::fs::write(&file, transport.config_text().await.unwrap()).unwrap();
    let output = Command::new("ssh")
        .arg("-F")
        .arg(&file)
        .args([
            "-l",
            "deploy",
            "-p",
            &harness.port.to_string(),
            "-O",
            command,
        ])
        .args(["--", &harness.host])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), said)
}

#[tokio::test]
async fn calls_share_one_connection_whose_master_is_live_after_the_first() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let _masters = Masters::of(&sandbox);
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .shared(&harness.host, deploy(&harness, &harness.key, &known))
        .await;
    let (live, said) = control(&sandbox, &harness, &ssh, "check").await;
    assert!(!live, "a master before the first call: {said}");

    let first = payload_of(&ssh, &ssh_connection()).await;
    let (live, said) = control(&sandbox, &harness, &ssh, "check").await;
    assert!(live, "{said}");
    assert!(said.starts_with("Master running (pid="), "{said}");
    assert_eq!(sandbox.sockets().len(), 1, "{:?}", sandbox.sockets());

    // Later calls ride the same connection: a script, then an upload read
    // back.
    assert_eq!(payload_of(&ssh, &ssh_connection()).await, first);
    let destination = format!("/tmp/apvm-e2e-ssh-mux-upload-{}", std::process::id());
    let bytes: Vec<u8> = (0..=255u8).cycle().take(1_000_000).collect();
    let script = UploadScript::tee(Nonce::random(), &Identity::Login, &destination).unwrap();
    let upload = Request::upload(script, Payload::Bytes(Arc::from(bytes)), TIMEOUT);
    let done = ssh.execute(&upload).await.unwrap();
    assert!(done.exit().success(), "{:?}", done.stderr());
    let size = php(&format!("echo filesize('{destination}');"), TIMEOUT);
    assert_eq!(payload_of(&ssh, &size).await, "1000000");

    // A transport whose server does not multiplex opens its own.
    let mut settings = deploy(&harness, &harness.key, &known);
    settings.multiplex = Some(false);
    let own = sandbox
        .transport(&harness.host, settings, HostKeyPolicy::Strict)
        .await;
    assert_ne!(payload_of(&own, &ssh_connection()).await, first);

    let (stopped, said) = control(&sandbox, &harness, &ssh, "exit").await;
    assert!(stopped, "{said}");
}

impl Sandbox {
    /// [`Sandbox::transport`] with a control directory in the sandbox, for
    /// a server whose connection must not be shared (a proxy in the way).
    ///
    /// # Arguments
    ///
    /// * `host` - The server's host
    /// * `ssh` - Its settings
    /// * `host_keys` - The host key policy
    async fn unshared(
        &self,
        host: &str,
        ssh: SshSettings,
        host_keys: HostKeyPolicy,
    ) -> SshTransport {
        let base = self.transport(host, ssh, host_keys).await;
        let options = base.options().clone().with_control_dir(self.path("mux"));
        let unshared = SshTransport::new(base.server().clone(), options);
        assert_eq!(
            unshared.multiplexing().await.unwrap(),
            Multiplexing::Off(NoMultiplexing::Proxied)
        );
        unshared
    }
}

#[tokio::test]
async fn a_jump_host_turns_multiplexing_off_so_its_failures_are_told() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let _masters = Masters::of(&sandbox);
    // With `ControlPersist`, ssh sends a jump host's messages to /dev/null
    // (audit round 3): an unknown hop key would read as nothing at all.
    // Unshared, it is told: the destination's key alone is known.
    let destination_only = sandbox.text("destination_known_hosts");
    std::fs::write(&destination_only, harness.host_keys_as("127.0.0.3")).unwrap();
    let ssh = sandbox
        .unshared(
            "127.0.0.3",
            through_jump(&harness, &destination_only),
            HostKeyPolicy::Strict,
        )
        .await;
    let name = harness.known_hosts_name();
    assert_fails(
        &ssh,
        SshFailureKind::HostKeyUnknown,
        &format!(" host key is known for {name} and you have requested strict checking"),
    )
    .await;
    // With both keys known it runs, on a connection of its own.
    let known = known_as(&harness, &sandbox, &["127.0.0.3"]);
    let ssh = sandbox
        .unshared(
            "127.0.0.3",
            through_jump(&harness, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    assert_runs(&ssh).await;
    assert!(sandbox.sockets().is_empty(), "{:?}", sandbox.sockets());
}

#[tokio::test]
async fn a_master_that_dies_during_a_call_is_a_lost_connection() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let _masters = Masters::of(&sandbox);
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .shared(&harness.host, deploy(&harness, &harness.key, &known))
        .await;
    payload_of(&ssh, &ssh_connection()).await;
    // A master is its starter's ssh (`ssh -F <file> -N …`, its file in a
    // private directory of the sandbox), left in the background.
    let master = master_pattern(&sandbox);
    assert!(pgrep(&master), "no master is running");
    let killer = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(1500));
        Command::new("pkill")
            .args(["-KILL", "-f", "--", &master])
            .status()
            .unwrap()
            .success()
    });
    let done = ssh
        .execute(&php("echo 'started'; sleep(5);", TIMEOUT))
        .await
        .unwrap();
    assert!(killer.await.unwrap(), "the master was not killed");
    assert_eq!(done.exit(), Exit::TransportFailed);
    assert!(done.script_started());
    let failure = classify(&done).unwrap();
    assert_eq!(failure.kind(), SshFailureKind::ConnectionLost, "{failure}");
    // The stale socket is replaced silently by the next call.
    payload_of(&ssh, &ssh_connection()).await;
}

/// A TCP relay to the harness that can silently drop every connection it
/// relays (as a Wi-Fi switch or a NAT reset does): data on them is then
/// swallowed, never forwarded nor refused, while new connections work.
struct Relay {
    /// The port it listens on, on 127.0.0.1.
    port: u16,
    /// Connections accepted under an older generation are dropped.
    generation: Arc<std::sync::atomic::AtomicU64>,
}

impl Relay {
    /// Start relaying to `harness`'s SSH port (threads that end with the
    /// test process).
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness
    fn start(harness: &Harness) -> Self {
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::AtomicU64;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let generation = Arc::new(AtomicU64::new(0));
        let target = format!("{}:{}", harness.host, harness.port);
        let shared = Arc::clone(&generation);
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                let born = shared.load(std::sync::atomic::Ordering::SeqCst);
                for (from, to) in [
                    (client.try_clone().unwrap(), server.try_clone().unwrap()),
                    (server, client),
                ] {
                    let shared = Arc::clone(&shared);
                    std::thread::spawn(move || pump(from, to, born, &shared));
                }
            }
        });
        Self { port, generation }
    }

    /// Silently drop every connection relayed so far.
    fn blackhole(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Copy `from` to `to` until either ends, swallowing everything once the
/// relay's generation is no longer `born`.
///
/// # Arguments
///
/// * `from` - The source
/// * `to` - The destination
/// * `born` - The generation the connection was accepted under
/// * `generation` - The relay's generation
fn pump(
    mut from: std::net::TcpStream,
    mut to: std::net::TcpStream,
    born: u64,
    generation: &std::sync::atomic::AtomicU64,
) {
    use std::io::{Read, Write};
    let mut buffer = [0u8; 65536];
    while let Ok(read) = from.read(&mut buffer) {
        if read == 0 {
            break;
        }
        if generation.load(std::sync::atomic::Ordering::SeqCst) != born {
            continue;
        }
        if to.write_all(&buffer[..read]).is_err() {
            break;
        }
    }
}

#[tokio::test]
async fn a_master_whose_connection_died_costs_one_timed_out_call() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let _masters = Masters::of(&sandbox);
    let relay = Relay::start(&harness);
    let name = format!("[{}]:{}", harness.host, relay.port);
    let known = sandbox.text("relay_known_hosts");
    std::fs::write(&known, harness.host_keys_as(&name)).unwrap();
    let mut settings = deploy(&harness, &harness.key, &known);
    settings.port = Some(relay.port);
    let ssh = sandbox.shared(&harness.host, settings).await;
    let first = payload_of(&ssh, &ssh_connection()).await;

    relay.blackhole();
    // The master's connection is dead: the call stalls until it times out
    // (its keep-alive checks would take 30 s or more), before its script.
    let stalled = ssh
        .execute(&php("echo 1;", Duration::from_secs(4)))
        .await
        .unwrap();
    assert_eq!(stalled.exit(), Exit::TimedOut);
    assert!(!stalled.script_started());
    // The master was told to stop taking calls: the next one starts a new
    // master at once, on a new connection.
    let started = std::time::Instant::now();
    let next = payload_of(&ssh, &php("echo getenv('SSH_CONNECTION');", TIMEOUT)).await;
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );
    assert_ne!(next, first);
}

/// The process ids whose command line matches `pattern` (`pgrep -f`).
///
/// # Arguments
///
/// * `pattern` - An extended regular expression
fn pids(pattern: &str) -> Vec<String> {
    let output = Command::new("pgrep")
        .args(["-f", "--", pattern])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Send `name` to process `pid`.
///
/// # Arguments
///
/// * `name` - `STOP`, `CONT`, `KILL`, …
/// * `pid` - The process
fn signal(name: &str, pid: &str) {
    let status = Command::new("kill")
        .args([&format!("-{name}"), pid])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{name} {pid}");
}

#[tokio::test]
async fn a_timed_out_call_through_a_jump_host_leaves_the_shared_connection_up() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let _masters = Masters::of(&sandbox);
    // `127.0.0.5`: no other test names it.
    let known = known_as(&harness, &sandbox, &["127.0.0.5"]);
    let ssh = Arc::new(
        sandbox
            .unshared(
                "127.0.0.5",
                through_jump(&harness, &known),
                HostKeyPolicy::Strict,
            )
            .await,
    );
    // Regression (audit round 2): call A runs long and times out, so apvm
    // kills its process tree, while call B runs. When calls became masters,
    // A's tree held the jump host's `ssh -W` carrying the master B rode,
    // and B was cut mid-script (exit 255). Through a jump host nothing is
    // shared now, so B is untouched.
    let first = {
        let ssh = Arc::clone(&ssh);
        tokio::spawn(async move {
            ssh.execute(&php("echo 'A'; sleep(30);", Duration::from_secs(4)))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = {
        let ssh = Arc::clone(&ssh);
        tokio::spawn(async move { ssh.execute(&php("sleep(8); echo 'B';", TIMEOUT)).await })
    };
    let a = first.await.unwrap().unwrap();
    assert_eq!(a.exit(), Exit::TimedOut);
    assert!(a.script_started());
    let b = second.await.unwrap().unwrap();
    let stderr = String::from_utf8_lossy(b.stderr().bytes()).into_owned();
    assert!(b.exit().success(), "{:?}: {stderr}", b.exit());
    assert_eq!(b.payload(), Ok(&b"B"[..]));
    assert!(!payload_of(&ssh, &ssh_connection()).await.is_empty());
}

#[tokio::test]
async fn a_stopped_master_is_replaced() {
    let Some(harness) = Harness::get() else {
        return;
    };
    let sandbox = Sandbox::new();
    let _masters = Masters::of(&sandbox);
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .shared(&harness.host, deploy(&harness, &harness.key, &known))
        .await;
    let first = payload_of(&ssh, &ssh_connection()).await;
    let master = master_pattern(&sandbox);
    let stopped = pids(&master);
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    // A stopped master accepts connections to its socket but answers
    // nothing: `-O check` gets no answer (3 s), so its socket is removed and
    // the call runs on its own connection, away from it (before 9.6 a call
    // that met it would hang).
    signal("STOP", &stopped[0]);
    let started = std::time::Instant::now();
    let next = ssh.execute(&ssh_connection()).await.unwrap();
    let took = started.elapsed();
    let stderr = String::from_utf8_lossy(next.stderr().bytes()).into_owned();
    assert!(next.exit().success(), "{stderr}");
    assert!(took < Duration::from_secs(30), "{took:?}");
    assert_ne!(String::from_utf8_lossy(next.payload().unwrap()), first);
    // The next call starts a new master and rides it.
    let third = payload_of(&ssh, &ssh_connection()).await;
    signal("CONT", &stopped[0]);
    // Best effort: resumed, it may already have exited.
    let _ = Command::new("kill").args(["-KILL", &stopped[0]]).status();
    let now: Vec<String> = pids(&master)
        .into_iter()
        .filter(|pid| *pid != stopped[0])
        .collect();
    assert_eq!(now.len(), 1, "{now:?}");
    assert_eq!(payload_of(&ssh, &ssh_connection()).await, third);
}

/// The command line of `sandbox`'s masters, for `pgrep -f`: their starter's
/// `ssh -F <sandbox>/apvm-ssh-…/apvm-ssh.conf -N …` (OpenSSH retitles a
/// master only when it forks from a call with a command).
///
/// # Arguments
///
/// * `sandbox` - The sandbox
fn master_pattern(sandbox: &Sandbox) -> String {
    format!(
        "^ssh -F {}/apvm-ssh-[^ ]*/apvm-ssh.conf -N ",
        sandbox.dir.path().display()
    )
}

#[tokio::test]
async fn a_jump_host_only_calls_get_turns_multiplexing_off() {
    let Some(harness) = Harness::get() else {
        return;
    };
    // OpenSSH 10's `Match command`: only a call's remote command (`sh -s`)
    // gets the jump host, which a plain `ssh -G` and a master's `-N` miss.
    let sandbox = Sandbox::new();
    sandbox.user_config("Match command \"sh -s\"\n  ProxyJump nobody@hop.invalid\n");
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let base = sandbox
        .transport(
            &harness.host,
            deploy(&harness, &harness.key, &known),
            HostKeyPolicy::Strict,
        )
        .await;
    let options = base.options().clone().with_control_dir(sandbox.path("mux"));
    let ssh = SshTransport::new(base.server().clone(), options);
    match ssh.multiplexing().await.unwrap() {
        Multiplexing::Off(NoMultiplexing::Proxied) => {}
        // A client before 10.0 refuses the attribute: nothing is shared.
        Multiplexing::Off(NoMultiplexing::Unresolved { reason }) => {
            eprintln!("skipped: this ssh knows no 'Match command': {reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_slow_ssh_config_does_not_make_a_live_master_look_stuck() {
    let Some(harness) = Harness::get() else {
        return;
    };
    // `Match exec` runs on every read of the configuration: `ssh -O check`
    // took over 4 s here, past apvm's 3 s wait for a master (audit round
    // 5). apvm looks behind the socket itself, so a live master is seen.
    let sandbox = Sandbox::new();
    let _masters = Masters::of(&sandbox);
    sandbox.user_config("Match exec \"sleep 4\"\n  ServerAliveCountMax 3\n");
    let known = harness.known_hosts.to_str().unwrap().to_string();
    let ssh = sandbox
        .shared(&harness.host, deploy(&harness, &harness.key, &known))
        .await;
    let first = payload_of(&ssh, &ssh_connection()).await;
    let next = payload_of(&ssh, &ssh_connection()).await;
    assert_eq!(next, first, "the second call did not ride the master");
    assert_eq!(sandbox.sockets().len(), 1, "{:?}", sandbox.sockets());
}
