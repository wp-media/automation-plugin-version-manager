//! Unit tests of the connecting commands' plumbing: the fake remote the
//! command tests use, bounded concurrency, the save-anyway question, and
//! the reports (text and `--json`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::Duration;

use apvm_sites::remote::{Captured, Exit, HostKeyNotices, Nonce, classify};
use apvm_sites::{Error, Inventory, Server, Site};

use super::check::{
    CapabilitySummary, DirSummary, IdentitySummary, Login, Named, SiteSummary, WarningSummary,
    WordPressSummary, WpCliSummary,
};
use super::render::{ServerResult, SiteResult, servers_json, servers_text, sites_json, sites_text};
use super::*;
use crate::commands::inventory::Recorder;

/// A [`Remote`] that answers from canned results, and records what was
/// asked (`server:<id>` / `site:<id>`, with `+trust` for
/// `--trust-new-host-key`). An id without an answer fails as unreachable.
#[derive(Default)]
pub(crate) struct FakeRemote {
    /// Failures by server id; any other server connects.
    server_failures: HashMap<String, Error>,
    /// Failures by site id; any other site is ready.
    site_failures: HashMap<String, Error>,
    /// Host keys every test reports as recorded.
    recorded: Vec<String>,
    /// What was asked, in order.
    pub asked: RefCell<Vec<String>>,
}

impl FakeRemote {
    /// A remote where every server connects and every site is ready.
    pub(crate) fn healthy() -> Self {
        Self::default()
    }

    /// This remote, with server `id`'s test failing with `error`.
    pub(crate) fn failing_server(mut self, id: &str, error: Error) -> Self {
        self.server_failures.insert(id.into(), error);
        self
    }

    /// This remote, with site `id`'s probe failing with `error`.
    pub(crate) fn failing_site(mut self, id: &str, error: Error) -> Self {
        self.site_failures.insert(id.into(), error);
        self
    }

    /// This remote, reporting `host` as a newly recorded host key.
    pub(crate) fn recording(mut self, host: &str) -> Self {
        self.recorded.push(host.into());
        self
    }

    /// The host key notices every test reports.
    fn notices(&self) -> HostKeyNotices {
        let mut notices = HostKeyNotices::default();
        notices.recorded.clone_from(&self.recorded);
        notices
    }

    /// Record a question.
    fn ask(&self, what: &str, id: &str, keys: HostKeyPolicy) {
        let trust = if keys == HostKeyPolicy::AcceptNew {
            "+trust"
        } else {
            ""
        };
        self.asked.borrow_mut().push(format!("{what}:{id}{trust}"));
    }
}

impl Remote for FakeRemote {
    fn test_server<'a>(
        &'a self,
        server: &'a ResolvedServer,
        keys: HostKeyPolicy,
    ) -> LocalFuture<'a, ServerCheck> {
        self.ask("server", &server.id, keys);
        let outcome = match self.server_failures.get(&server.id) {
            Some(error) => Err(copy(error)),
            None => Ok(login()),
        };
        let notices = self.notices();
        Box::pin(async move {
            ServerCheck {
                host_keys: notices,
                outcome,
            }
        })
    }

    fn test_site<'a>(
        &'a self,
        site: &'a ResolvedSite,
        keys: HostKeyPolicy,
    ) -> LocalFuture<'a, SiteCheck> {
        self.ask("site", &site.id, keys);
        let outcome = match self.site_failures.get(&site.id) {
            Some(error) => Err(copy(error)),
            None => Ok(summary()),
        };
        let notices = self.notices();
        Box::pin(async move {
            SiteCheck {
                host_keys: notices,
                outcome,
            }
        })
    }
}

/// A copy of a canned failure (`Error` is not `Clone`): its class and
/// line for ssh failures, its message otherwise.
fn copy(error: &Error) -> Error {
    match error {
        Error::Ssh(failure) => Error::Ssh(failure.clone()),
        Error::Probe(failure) => Error::Probe(failure.clone()),
        Error::Timeout { after, started } => Error::Timeout {
            after: *after,
            started: *started,
        },
        other => Error::ScriptNotRun {
            exit: Exit::Status(1),
            line: Some(other.to_string()),
        },
    }
}

/// ssh's failure for `stderr` (exit 255), as the transport classifies it.
///
/// # Arguments
///
/// * `stderr` - What ssh printed
pub(crate) fn ssh_error(stderr: &str) -> Error {
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

/// A refused connection (`ssh_unreachable`).
pub(crate) fn unreachable() -> Error {
    ssh_error("ssh: connect to host 127.0.0.1 port 1: Connection refused\r\n")
}

/// An unknown host key (`ssh_host_key_unknown`).
pub(crate) fn unknown_key() -> Error {
    ssh_error(
        "No ED25519 host key is known for [127.0.0.1]:2722 and you have requested strict \
         checking.\r\nHost key verification failed.\r\n",
    )
}

/// `deploy` (2001) on a server with sudo.
pub(crate) fn login() -> Login {
    Login {
        user: Named {
            id: 2001,
            name: Some("deploy".into()),
        },
        group: Named {
            id: 2001,
            name: Some("deploy".into()),
        },
        groups: vec![2001, 33],
        shell: Some("/bin/bash".into()),
        sudo: Some("/usr/bin/sudo".into()),
        runuser: None,
        checksum: Some("/usr/bin/sha256sum".into()),
        wp_cli: Some("/usr/local/bin/wp".into()),
    }
}

/// `www-data`.
fn www() -> Named {
    Named {
        id: 33,
        name: Some("www-data".into()),
    }
}

/// A ready single site owned by `www-data`, run through sudo.
pub(crate) fn summary() -> SiteSummary {
    SiteSummary {
        login: login(),
        plugins_dir: DirSummary {
            path: "/var/www/shop/wp-content/plugins".into(),
            owner: www(),
            group: www(),
            mode: "0755".into(),
            read_by_root: false,
        },
        identity: IdentitySummary {
            switch: "sudo",
            runs_as: "sudo -u www-data -g www-data".into(),
            uid: 33,
            creation: "at_creation",
            owner: www(),
            group: www(),
        },
        wp_cli: WpCliSummary {
            version: "2.12.0".into(),
            path: Some("/usr/local/bin/wp".into()),
        },
        wordpress: WordPressSummary {
            version: "7.1.3".into(),
            php_version: "8.4.26".into(),
            multisite: false,
            siteurl: "http://shop.test".into(),
            home: "http://shop.test".into(),
            disallow_file_mods: false,
        },
        capabilities: CapabilitySummary {
            plugins_dir_writable: true,
            upgrade_dir: "writable",
            tmp_free_kib: Some(74_983_604),
            content_free_kib: Some(512_000),
        },
        warnings: Vec::new(),
        php_notices: Vec::new(),
    }
}

/// Server `eu-1` (deploy@203.0.113.10) and its site `shop`, resolved.
fn shop() -> ResolvedSite {
    let mut inventory = Inventory::new();
    let mut server = Server::new("eu-1", "203.0.113.10");
    server.ssh.user = Some("deploy".into());
    inventory.add_server(server).expect("valid server");
    inventory
        .add_site(Site::new("shop", "eu-1", "/var/www/shop"))
        .expect("valid site");
    inventory.resolve_site("shop").expect("resolves")
}

/// Run `future` to its end on a current-thread runtime.
pub(crate) fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

// ── bounded ──────────────────────────────────────────────────────────────

#[test]
fn bounded_keeps_the_order_asked_whatever_finishes_first() {
    let tasks: Vec<LocalFuture<'_, usize>> = (0..5)
        .map(|index| -> LocalFuture<'_, usize> {
            Box::pin(async move {
                // Later tasks finish first.
                tokio::time::sleep(Duration::from_millis(50 - 10 * index as u64)).await;
                index
            })
        })
        .collect();
    let jobs = NonZeroUsize::new(5).expect("non-zero");
    assert_eq!(block_on(bounded(tasks, jobs)), [0, 1, 2, 3, 4]);
}

#[test]
fn bounded_runs_at_most_jobs_at_once_and_all_of_them() {
    let running = Cell::new(0_usize);
    let most = Cell::new(0_usize);
    let tasks: Vec<LocalFuture<'_, ()>> = (0..7)
        .map(|_| -> LocalFuture<'_, ()> {
            Box::pin(async {
                running.set(running.get() + 1);
                most.set(most.get().max(running.get()));
                tokio::time::sleep(Duration::from_millis(5)).await;
                running.set(running.get() - 1);
            })
        })
        .collect();
    let jobs = NonZeroUsize::new(3).expect("non-zero");
    assert_eq!(block_on(bounded(tasks, jobs)).len(), 7);
    assert_eq!(most.get(), 3, "the limit is reached and never passed");
    assert_eq!(running.get(), 0);
}

#[test]
fn bounded_with_nothing_to_do_returns_nothing() {
    let tasks: Vec<LocalFuture<'_, u8>> = Vec::new();
    assert!(block_on(bounded(tasks, NonZeroUsize::MIN)).is_empty());
}

// ── helpers ──────────────────────────────────────────────────────────────

#[test]
fn trust_new_host_key_means_accept_new() {
    assert_eq!(host_keys(true), HostKeyPolicy::AcceptNew);
    assert_eq!(host_keys(false), HostKeyPolicy::Strict);
}

#[test]
fn a_server_test_allows_each_connection_its_connect_timeout() {
    let resolved = |edit: fn(&mut Server)| {
        let mut server = Server::new("eu-1", "203.0.113.10");
        edit(&mut server);
        let mut inventory = Inventory::new();
        inventory.add_server(server).expect("valid server");
        inventory.resolve_server("eu-1").expect("resolves")
    };
    // The built-in connect timeout (15 s), one connection.
    assert_eq!(
        server_test_timeout(&resolved(|_| {})),
        SERVER_TEST_TIMEOUT + Duration::from_secs(15)
    );
    // A slow link the user allowed 120 s: the test must not end first.
    let slow = resolved(|server| server.ssh.connect_timeout_secs = Some(120));
    assert_eq!(server_test_timeout(&slow), Duration::from_secs(180));
    // Two jump hosts: three connections, each with its own timeout.
    let jumped = resolved(|server| {
        server.ssh.connect_timeout_secs = Some(10);
        server.ssh.proxy_jump = Some("bastion,inner:2222".into());
    });
    assert_eq!(server_test_timeout(&jumped), Duration::from_secs(90));
}

#[test]
fn a_newer_inventory_is_refused_before_any_connection() {
    let newer = Inventory::from_json(r#"{"version": 7}"#).expect("readable");
    let Err(failure) = writable(newer) else {
        panic!("a read-only inventory is refused");
    };
    assert_eq!(failure.code(), "inventory_read_only");
    assert!(writable(Inventory::new()).is_ok());
}

#[test]
fn a_passed_test_saves_without_asking() {
    let mut recorder = Recorder::new();
    let result = recorder.run(Some(false), |console| {
        save_anyway(console, None, EntityKind::Site, "shop")
    });
    assert!(result.is_ok());
    assert!(recorder.asked.is_empty());
}

#[test]
fn a_failed_test_asks_and_yes_saves_with_a_warning() {
    let error = unreachable();
    let mut recorder = Recorder::new();
    let result = recorder.run(Some(true), |console| {
        save_anyway(console, Some(&error), EntityKind::Server, "eu-1")
    });
    assert!(result.is_ok());
    assert_eq!(
        recorder.asked,
        ["The test failed (ssh_unreachable). Save server 'eu-1' anyway? [y/N]"]
    );
    assert!(
        recorder
            .err
            .contains("saving server 'eu-1' although its test failed (ssh_unreachable)"),
        "{}",
        recorder.err
    );
}

#[test]
fn a_failed_test_answered_no_saves_nothing() {
    let error = unknown_key();
    let mut recorder = Recorder::new();
    let result = recorder.run(Some(false), |console| {
        save_anyway(console, Some(&error), EntityKind::Site, "shop")
    });
    let Err(failure) = result else {
        panic!("no means not saved");
    };
    assert_eq!(failure.code(), "ssh_host_key_unknown");
    assert_eq!(
        failure.to_string(),
        "site 'shop' was not added: its test failed (ssh_host_key_unknown)\n  fix the problem \
         and add it again, or pass --no-test to save it without testing"
    );
}

#[test]
fn a_failed_test_without_a_terminal_saves_nothing() {
    let error = unreachable();
    let mut recorder = Recorder::new();
    let result = recorder.run(None, |console| {
        save_anyway(console, Some(&error), EntityKind::Site, "shop")
    });
    let Err(failure) = result else {
        panic!("nobody to ask means not saved");
    };
    assert!(
        failure
            .to_string()
            .contains("and stdin is not a terminal to ask whether to save it anyway"),
        "{failure}"
    );
    let failed_prompt = recorder.run_asking(
        |_| Err(std::io::Error::other("closed")),
        |console| save_anyway(console, Some(&error), EntityKind::Site, "shop"),
    );
    assert!(matches!(failed_prompt, Err(Failure::Prompt(_))));
}

// ── reports ──────────────────────────────────────────────────────────────

#[test]
fn a_connected_server_lists_its_login_user_and_tools() {
    let site = shop();
    let check = ServerCheck {
        host_keys: HostKeyNotices::default(),
        outcome: Ok(login()),
    };
    let result = ServerResult {
        server: &site.server,
        check: &check,
        adding: false,
    };
    assert_eq!(
        servers_text(&[result]),
        "Server 'eu-1' (deploy@203.0.113.10)\n\
         \x20 ✓ connected\n\
         \x20 login     deploy (2001), group deploy (2001)\n\
         \x20 groups    2001, 33\n\
         \x20 shell     /bin/bash\n\
         \x20 sudo      /usr/bin/sudo\n\
         \x20 runuser   not found\n\
         \x20 checksum  /usr/bin/sha256sum\n\
         \x20 wp        /usr/local/bin/wp\n"
    );
}

#[test]
fn a_failed_server_shows_its_code_message_and_remedies() {
    let site = shop();
    let check = ServerCheck {
        host_keys: HostKeyNotices::default(),
        outcome: Err(unknown_key()),
    };
    let result = ServerResult {
        server: &site.server,
        check: &check,
        adding: false,
    };
    let text = servers_text(&[result]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Server 'eu-1' (deploy@203.0.113.10)");
    assert_eq!(
        lines[1],
        "  ✗ ssh_host_key_unknown: the server's host key is not known: No ED25519 host key is \
         known for [127.0.0.1]:2722 and you have requested strict checking."
    );
    assert!(
        lines[2].starts_with("    check the host key's fingerprint"),
        "{text}"
    );
    assert!(lines[3].starts_with("    a ProxyCommand"), "{text}");
}

#[test]
fn host_key_notices_are_shown_before_the_outcome() {
    let site = shop();
    let mut host_keys = HostKeyNotices::default();
    host_keys.recorded.push("[127.0.0.1]:2722".into());
    host_keys.not_saved = true;
    host_keys.refused_certificate = Some("Certificate invalid: expired".into());
    let check = SiteCheck {
        host_keys,
        outcome: Ok(summary()),
    };
    let result = SiteResult {
        id: "shop",
        server: &site.server,
        check: &check,
        adding: false,
    };
    let text = sites_text(&[result]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[1],
        "  host key  trusted the new key of [127.0.0.1]:2722 and recorded it in \
         ~/.ssh/known_hosts"
    );
    assert!(lines[2].contains("trusted for this run only"), "{text}");
    assert!(
        lines[3].contains("(Certificate invalid: expired)") && lines[3].contains("bypassed"),
        "{text}"
    );
    assert_eq!(lines[4], "  ✓ ready");
}

#[test]
fn a_ready_site_shows_identity_wordpress_and_capabilities() {
    let site = shop();
    let mut ready = summary();
    ready.wordpress.disallow_file_mods = true;
    ready.warnings.push(WarningSummary {
        code: None,
        message: "the server has no sha256sum".into(),
    });
    ready.php_notices.push("Deprecated: x".into());
    let check = SiteCheck {
        host_keys: HostKeyNotices::default(),
        outcome: Ok(ready),
    };
    let result = SiteResult {
        id: "shop",
        server: &site.server,
        check: &check,
        adding: false,
    };
    assert_eq!(
        sites_text(&[result]),
        "Site 'shop' on server 'eu-1' (deploy@203.0.113.10)\n\
         \x20 ✓ ready\n\
         \x20 login        deploy (2001), group deploy (2001)\n\
         \x20 groups       2001, 33\n\
         \x20 plugins dir  /var/www/shop/wp-content/plugins (www-data:www-data 0755)\n\
         \x20 runs as      sudo -u www-data -g www-data (uid 33)\n\
         \x20 new files    owned by www-data:www-data at creation\n\
         \x20 WP-CLI       2.12.0 (/usr/local/bin/wp)\n\
         \x20 WordPress    7.1.3, PHP 8.4.26, single site\n\
         \x20 site URL     http://shop.test\n\
         \x20 writable     plugins dir yes, upgrade dir yes\n\
         \x20 free space   temp dir 71.5 GiB, content 500.0 MiB\n\
         \x20 file mods    disallowed (DISALLOW_FILE_MODS): installs and removals need \
         --allow-file-mods\n\
         \x20 warn  the server has no sha256sum\n\
         \x20 warn  PHP printed notices around WordPress's answer: Deprecated: x\n"
    );
}

#[test]
fn several_results_end_with_a_count() {
    let site = shop();
    let ready = SiteCheck {
        host_keys: HostKeyNotices::default(),
        outcome: Ok(summary()),
    };
    let failed = SiteCheck {
        host_keys: HostKeyNotices::default(),
        outcome: Err(unreachable()),
    };
    let results = [
        SiteResult {
            id: "shop",
            server: &site.server,
            check: &ready,
            adding: false,
        },
        SiteResult {
            id: "blog",
            server: &site.server,
            check: &failed,
            adding: false,
        },
    ];
    let text = sites_text(&results);
    assert!(
        text.ends_with("\n1 of 2 sites ready; 1 failed.\n"),
        "{text}"
    );
    assert!(text.contains("ready\n"), "{text}");
    assert!(text.contains("\n\nSite 'blog' on server 'eu-1'"), "{text}");
}

#[test]
fn site_json_holds_the_probe_or_the_error_with_remedies() {
    let site = shop();
    let ready = SiteCheck {
        host_keys: HostKeyNotices::default(),
        outcome: Ok(summary()),
    };
    let failed = SiteCheck {
        host_keys: HostKeyNotices::default(),
        outcome: Err(unknown_key()),
    };
    let results = [
        SiteResult {
            id: "shop",
            server: &site.server,
            check: &ready,
            adding: false,
        },
        SiteResult {
            id: "blog",
            server: &site.server,
            check: &failed,
            adding: false,
        },
    ];
    let warning = apvm_sites::TargetWarning::NoSites {
        kind: apvm_sites::EntityKind::Group,
        id: "empty".into(),
    };
    let text = sites_json(&results, &[warning]).expect("serializes");
    let json: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(json["schema"], 1);
    assert_eq!(json["ok"], false);
    assert_eq!(json["warnings"][0], "group 'empty' has no sites");
    let shop = &json["sites"][0];
    assert_eq!(shop["site"], "shop");
    assert_eq!(shop["server"], "eu-1");
    assert_eq!(shop["destination"], "deploy@203.0.113.10");
    assert_eq!(shop["ok"], true);
    assert_eq!(shop["error"], serde_json::Value::Null);
    assert_eq!(
        shop["probe"]["identity"]["runs_as"],
        "sudo -u www-data -g www-data"
    );
    assert_eq!(shop["probe"]["identity"]["creation"], "at_creation");
    assert_eq!(shop["probe"]["plugins_dir"]["owner"]["name"], "www-data");
    assert_eq!(shop["probe"]["capabilities"]["upgrade_dir"], "writable");
    assert_eq!(shop["probe"]["wordpress"]["version"], "7.1.3");
    assert_eq!(shop["probe"]["wp_cli"]["version"], "2.12.0");
    assert_eq!(
        shop["host_keys"],
        serde_json::json!({"recorded": [], "not_saved": false, "refused_certificate": null})
    );
    let blog = &json["sites"][1];
    assert_eq!(blog["ok"], false);
    assert_eq!(blog["probe"], serde_json::Value::Null);
    assert_eq!(blog["error"]["code"], "ssh_host_key_unknown");
    assert_eq!(blog["error"]["problems"], serde_json::json!([]));
    let remedies = blog["error"]["remedies"].as_array().expect("remedies");
    assert_eq!(remedies.len(), 2);
}

#[test]
fn server_json_holds_the_login_or_the_error() {
    let site = shop();
    let mut host_keys = HostKeyNotices::default();
    host_keys.recorded.push("[127.0.0.1]:2722".into());
    let check = ServerCheck {
        host_keys,
        outcome: Ok(login()),
    };
    let result = ServerResult {
        server: &site.server,
        check: &check,
        adding: false,
    };
    let json: serde_json::Value =
        serde_json::from_str(&servers_json(&[result]).expect("serializes")).expect("JSON");
    assert_eq!(json["ok"], true);
    let server = &json["servers"][0];
    assert_eq!(server["server"], "eu-1");
    assert_eq!(
        server["login"]["user"],
        serde_json::json!({"id": 2001, "name": "deploy"})
    );
    assert_eq!(server["login"]["runuser"], serde_json::Value::Null);
    assert_eq!(server["host_keys"]["recorded"][0], "[127.0.0.1]:2722");
    assert_eq!(server["error"], serde_json::Value::Null);
}

#[test]
fn sizes_read_in_mib_or_gib() {
    assert_eq!(render::size_text(None), "unknown");
    assert_eq!(render::size_text(Some(0)), "0.0 MiB");
    assert_eq!(render::size_text(Some(1536)), "1.5 MiB");
    assert_eq!(render::size_text(Some(1024 * 1024)), "1.0 GiB");
    assert_eq!(render::size_text(Some(74_983_604)), "71.5 GiB");
}
