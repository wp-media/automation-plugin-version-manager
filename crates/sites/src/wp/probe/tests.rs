//! The probe over [`FakeTransport`]: every step's requests, every §6.2
//! failure code, the fallbacks (BSD `stat`, root `stat`, a refused sudo
//! identity) and the warnings. Answers are the harness's (plan §3.5).

use super::*;
use crate::error::Code;
use crate::inventory::{Inventory, Owner, RunAs, Server, Site};
use crate::remote::fake::{FakeStep, FakeTransport, Reply};
use crate::remote::{Delivery, Request};

/// The standard plugins directory of the test site.
const PLUGINS: &str = "/var/www/single/wp-content/plugins";
/// sudo's refusal under `-n`.
const PASSWORD: &str = "sudo: a password is required\n";

/// A site on `/var/www/single`, changed by `edit`.
///
/// # Arguments
///
/// * `edit` - Changes to the site record
pub(crate) fn site(edit: impl FnOnce(&mut Site)) -> ResolvedSite {
    let mut inventory = Inventory::new();
    inventory
        .add_server(Server::new("s", "203.0.113.1"))
        .unwrap();
    let mut site = Site::new("single", "s", "/var/www/single");
    site.url = Some("http://single.test".into());
    edit(&mut site);
    inventory.add_site(site).unwrap();
    inventory.resolve_site("single").unwrap()
}

/// What the fake server's step H answers.
#[derive(Clone)]
pub(crate) struct Host {
    /// The login uid (and primary gid).
    uid: u32,
    /// The login name.
    name: &'static str,
    /// Whether sudo is installed.
    sudo: bool,
    /// Whether runuser is on the login user's PATH.
    runuser: bool,
    /// Whether any checksum tool is installed.
    checksum: bool,
    /// The walk's answer; `None`: the plugins directory and [`Host::stat`]'s
    /// lines when that succeeded, else a failed `cd`.
    walk: Option<FakeStep>,
    /// The diagnostic `stat -L`'s answer (six lines when it succeeds).
    stat: FakeStep,
    /// `id -u <name>` answers, in step H's order.
    users: Vec<FakeStep>,
    /// What `id -G` prints (empty: the uid alone).
    groups: &'static str,
}

impl Host {
    /// `deploy` (2001) with sudo, on a `www-data:www-data` directory.
    pub(crate) fn deploy() -> Self {
        Self {
            uid: 2001,
            name: "deploy",
            sudo: true,
            runuser: false,
            checksum: true,
            walk: None,
            stat: stat_of(33, 33, "755", "www-data", "www-data"),
            users: Vec::new(),
            groups: "",
        }
    }

    /// The walk's answer ([`Host::walk`]).
    fn walk_step(&self) -> FakeStep {
        if let Some(walk) = &self.walk {
            return walk.clone();
        }
        if self.stat.status == 0 {
            return walk_at(PLUGINS, &self.stat);
        }
        FakeStep::status(70, "", "sh: 1: cd: can't cd to x\n")
    }

    /// The step H reply.
    fn reply(&self) -> Reply {
        let found = |yes: bool, path: &str| {
            if yes {
                FakeStep::ok(format!("{path}\n"))
            } else {
                FakeStep::status(1, "", "")
            }
        };
        let mut steps = vec![
            FakeStep::ok(format!("{}\n", self.uid)),
            FakeStep::ok(format!("{}\n", self.uid)),
            FakeStep::ok(format!("{}\n", self.name)),
            FakeStep::ok(format!("{}\n", self.name)),
            FakeStep::ok(if self.groups.is_empty() {
                format!("{}\n", self.uid)
            } else {
                format!("{}\n", self.groups)
            }),
            FakeStep::ok("/bin/bash\n"),
            found(self.sudo, "/usr/bin/sudo"),
            found(self.runuser, "/usr/sbin/runuser"),
            found(self.checksum, "/usr/bin/sha256sum"),
            found(false, ""),
            found(false, ""),
            FakeStep::ok("/usr/local/bin/wp\n"),
            self.walk_step(),
            self.stat.clone(),
        ];
        steps.extend(self.users.iter().cloned());
        Reply::Steps(steps)
    }
}

/// A successful walk ending in `path`, with `stat`'s six lines.
///
/// # Arguments
///
/// * `path` - The physical path
/// * `stat` - A successful `stat` answer
fn walk_at(path: &str, stat: &FakeStep) -> FakeStep {
    let mut stdout = format!("{path}\n").into_bytes();
    stdout.extend(&stat.stdout);
    FakeStep::ok(stdout)
}

/// stat's six lines.
///
/// # Arguments
///
/// * `uid` - The owner
/// * `gid` - The group
/// * `mode` - The octal mode
/// * `user` - The owner's name
/// * `group` - The group's name
fn stat_of(uid: u32, gid: u32, mode: &str, user: &str, group: &str) -> FakeStep {
    FakeStep::ok(format!(
        "{uid}\n{gid}\n{mode}\ndirectory\n{user}\n{group}\n"
    ))
}

/// `cli version`'s answer.
fn version() -> Reply {
    Reply::Steps(vec![FakeStep::ok("WP-CLI 2.12.0\n")])
}

/// `id -g`'s answer as the identity.
///
/// # Arguments
///
/// * `gid` - The primary group id
fn primary_gid(gid: &str) -> Reply {
    Reply::Steps(vec![FakeStep::ok(format!("{gid}\n"))])
}

/// A refused sudo identity.
///
/// # Arguments
///
/// * `stderr` - sudo's message
fn refused(stderr: &str) -> Reply {
    Reply::Steps(vec![FakeStep::status(1, "", stderr)])
}

/// Step W's facts, with `plugin_dir` as `WP_PLUGIN_DIR` and its realpath.
///
/// # Arguments
///
/// * `plugin_dir` - The plugins directory WordPress uses
fn facts(plugin_dir: &str) -> Reply {
    let json = format!(
        r#"{{"wp_cli_version":"2.12.0","php_version":"8.4.26","wp_version":"7.1.3","multisite":false,"plugin_dir":"{plugin_dir}","plugin_dir_realpath":"{plugin_dir}","content_dir":"/var/www/single/wp-content","content_dir_realpath":"/var/www/single/wp-content","siteurl":"http://single.test","home":"http://single.test","disallow_file_mods":false}}"#
    );
    Reply::Steps(vec![FakeStep::ok(json)])
}

/// Step C's answers: everything writable, space known.
fn capable() -> Reply {
    let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
              overlay 90243936 10630268 74983604 13% /\n";
    Reply::Steps(vec![
        FakeStep::ok(""),
        FakeStep::ok(df),
        FakeStep::ok(""),
        FakeStep::ok(""),
        FakeStep::ok(""),
        FakeStep::ok(df),
    ])
}

/// The script text of a request.
///
/// # Arguments
///
/// * `request` - The request
fn text(request: &Request) -> String {
    match request.delivery() {
        Delivery::Stdin(script) => script.text().to_string(),
        other => panic!("not a stdin script: {other:?}"),
    }
}

/// Probe `site` against `replies`; every reply must be used.
///
/// # Arguments
///
/// * `site` - The site
/// * `replies` - The fake server's answers, in order
async fn run(site: &ResolvedSite, replies: Vec<Reply>) -> (Result<SiteProbe>, Vec<String>) {
    let fake = FakeTransport::new(replies);
    let result = probe(&fake, site).await;
    assert_eq!(fake.remaining(), 0, "every reply is used");
    (result, fake.calls().iter().map(text).collect())
}

/// The happy path's replies for `host`.
///
/// # Arguments
///
/// * `host` - Step H's answers
pub(crate) fn happy(host: &Host) -> Vec<Reply> {
    vec![host.reply(), version(), facts(PLUGINS), capable()]
}

/// The code of a failed probe.
///
/// # Arguments
///
/// * `result` - The probe's result
fn code(result: Result<SiteProbe>) -> Code {
    result.unwrap_err().code()
}

#[tokio::test]
async fn deploy_runs_as_the_owner_through_sudo_with_its_group() {
    let (result, scripts) = run(&site(|_| {}), happy(&Host::deploy())).await;
    let probe = result.unwrap();
    let www = Identity::sudo("www-data", Some("www-data")).unwrap();
    assert_eq!(probe.identity.identity, www);
    assert_eq!(
        (probe.identity.uid, probe.identity.creation),
        (33, Creation::AtCreation)
    );
    assert_eq!(probe.plugins_dir.path, PLUGINS);
    assert!(!probe.plugins_dir.read_by_root);
    assert_eq!(probe.wp_cli_version.to_string(), "2.12.0");
    assert_eq!(probe.facts.siteurl, "http://single.test");
    assert!(probe.capabilities.plugins_dir_writable);
    assert_eq!(probe.capabilities.tmp_free_kib, Some(74_983_604));
    assert_eq!(probe.warnings, []);
    assert_eq!(probe.site_id, "single");
    // Step H as the login user, with independent statements.
    assert!(
        scripts[0].contains("id -u </dev/null; printf"),
        "{}",
        scripts[0]
    );
    assert!(scripts[0].contains(&format!(
        "stat -L -c '%u\n%g\n%a\n%F\n%U\n%G' -- '{PLUGINS}'"
    )));
    assert!(
        scripts[0].contains("env -u BASH_ENV -u ENV -u CDPATH sh -c 'u=$1 f=$2 six=$3 dm=$4 lo=$5")
    );
    assert!(scripts[0].contains(
        "' sh '' -c '%u\n%g\n%a\n%F\n%U\n%G' '%u %a' %u 'var' 'www' 'single' 'wp-content' \
         'plugins' </dev/null"
    ));
    // Step I: the exact command shape, no --allow-root.
    assert!(scripts[1].contains("sudo -n -u 'www-data' -g 'www-data' -- env -u EDITOR"));
    assert!(scripts[1].contains("'wp' --skip-packages --exec='umask(umask() & ~0050);'"));
    assert!(scripts[1].contains("'cli' 'version'"));
    // Step W with the site's URL.
    assert!(scripts[2].contains("--url='http://single.test' 'eval'"));
    // Step C as the identity.
    assert!(scripts[3].contains(&format!(
        "sudo -n -u 'www-data' -g 'www-data' -- test -w '{PLUGINS}'"
    )));
    assert!(scripts.iter().all(|s| !s.contains("--allow-root")));
    // The session's base carries the URL.
    let base = probe
        .wp()
        .wp_cli()
        .statement(&probe.identity.identity)
        .render();
    assert!(base.contains("--url='http://single.test'"), "{base}");
}

#[tokio::test]
async fn allow_root_is_passed_exactly_for_uid_0() {
    let host = Host {
        uid: 0,
        name: "root",
        stat: stat_of(0, 0, "755", "root", "root"),
        ..Host::deploy()
    };
    let site = site(|site| site.allow_root = Some(true));
    let (result, scripts) = run(&site, happy(&host)).await;
    let probe = result.unwrap();
    assert_eq!(
        (probe.identity.identity.clone(), probe.identity.uid),
        (Identity::Login, 0)
    );
    assert!(scripts[1].contains("--skip-packages --exec='umask(umask() & ~0050);' --allow-root"));
    assert!(scripts[2].contains("--allow-root"));
    let base = probe.wp().wp_cli().statement(&Identity::Login).render();
    assert!(base.contains("--allow-root"));
    // A root login running as www-data through runuser: uid 33, no flag.
    let host = Host {
        uid: 0,
        name: "root",
        runuser: true,
        ..Host::deploy()
    };
    let (result, scripts) = run(&site, happy(&host)).await;
    let probe = result.unwrap();
    assert_eq!(
        probe.identity.identity,
        Identity::runuser("www-data", "www-data").unwrap()
    );
    assert!(scripts[1].contains("runuser -u 'www-data' -g 'www-data' -- env"));
    assert!(scripts.iter().all(|s| !s.contains("--allow-root")));
}

#[tokio::test]
async fn owner_is_root_stops_after_step_h() {
    let host = Host {
        stat: stat_of(0, 0, "755", "root", "root"),
        ..Host::deploy()
    };
    let (result, scripts) = run(&site(|_| {}), vec![host.reply()]).await;
    assert_eq!(code(result), Code::OwnerIsRoot);
    assert_eq!(scripts.len(), 1);
}

#[tokio::test]
async fn a_refused_group_falls_back_to_sudo_without_it() {
    let replies = vec![
        Host::deploy().reply(),
        refused(PASSWORD),
        primary_gid("33"),
        version(),
        facts(PLUGINS),
        capable(),
    ];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    let probe = result.unwrap();
    // Without -g, the identity's primary group is read first: not root.
    assert!(scripts[2].contains("sudo -n -u 'www-data' -- id -g </dev/null"));
    assert_eq!(
        probe.identity.identity,
        Identity::sudo("www-data", None).unwrap()
    );
    assert_eq!(probe.identity.creation, Creation::CheckAfter);
    assert_eq!(
        probe.warnings,
        [
            ProbeWarning::IdentityFallback {
                refused: Identity::sudo("www-data", Some("www-data")).unwrap()
            },
            ProbeWarning::CheckedAfterCreation
        ]
    );
    assert!(scripts[3].contains("sudo -n -u 'www-data' -- env"));
}

#[tokio::test]
async fn sudo_refusals_are_classified() {
    // (refusal, listing answer, code); the listing runs only for "a
    // password is required".
    let tty = "sudo: sorry, you must have a tty to run sudo\n";
    let cases: [(&str, Option<FakeStep>, Code); 4] = [
        (tty, None, Code::SudoRequiretty),
        (
            PASSWORD,
            Some(FakeStep::ok("/usr/bin/true\n")),
            Code::SudoPasswordRequired,
        ),
        (
            PASSWORD,
            Some(FakeStep::status(1, "", "")),
            Code::SudoNotAllowed,
        ),
        (
            PASSWORD,
            Some(FakeStep::status(1, "", PASSWORD)),
            Code::SudoPasswordRequired,
        ),
    ];
    for (refusal, listing, expected) in cases {
        let mut replies = vec![Host::deploy().reply(), refused(refusal), refused(refusal)];
        let listed = listing.is_some();
        replies.extend(listing.map(|step| Reply::Steps(vec![step])));
        let (result, scripts) = run(&site(|_| {}), replies).await;
        let error = result.unwrap_err();
        assert_eq!(error.code(), expected, "{error}");
        if listed {
            // The listing names the first, preferred identity.
            assert!(scripts[3].contains("sudo -n -l -u 'www-data' -g 'www-data' true"));
        }
    }
}

#[tokio::test]
async fn sudo_messages_name_the_rule_to_add() {
    let replies = vec![
        Host::deploy().reply(),
        refused(PASSWORD),
        refused(PASSWORD),
        Reply::Steps(vec![FakeStep::status(1, "", "")]),
    ];
    let (result, _) = run(&site(|_| {}), replies).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "sudo does not let 'deploy' run commands as 'www-data:www-data': add the rule \
         (deploy ALL=(www-data : www-data) NOPASSWD: ALL)"
    );
}

#[tokio::test]
async fn no_sudo_is_sudo_missing() {
    let host = Host {
        sudo: false,
        ..Host::deploy()
    };
    let (result, scripts) = run(&site(|_| {}), vec![host.reply()]).await;
    assert_eq!(code(result), Code::SudoMissing);
    assert_eq!(scripts.len(), 1);
}

#[tokio::test]
async fn the_login_user_owning_the_tree_needs_no_prefix() {
    // ftpuser on an FTP tree (ftpuser:www-data, setgid), sudo refuses its
    // `-g www-data`: no prefix, checked after.
    let host = Host {
        uid: 2010,
        name: "ftpuser",
        stat: stat_of(2010, 33, "2775", "ftpuser", "www-data"),
        ..Host::deploy()
    };
    let replies = vec![
        host.reply(),
        refused(PASSWORD),
        version(),
        facts(PLUGINS),
        capable(),
    ];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    let probe = result.unwrap();
    assert_eq!(probe.identity.identity, Identity::Login);
    assert_eq!(probe.identity.creation, Creation::CheckAfter);
    assert!(scripts[1].contains("sudo -n -u 'ftpuser' -g 'www-data' -- env"));
    assert!(scripts[2].contains("cd / && { printf '\\n%s\\n'"));
    assert!(scripts[2].contains("env -u EDITOR"));
    assert!(!scripts[2].contains("sudo"));
    assert!(scripts[1].contains("~0070"), "group bits of 2775");
}

#[tokio::test]
async fn wordpress_elsewhere_is_a_plugins_dir_mismatch_with_the_identity_unchanged() {
    let alt = "/var/www/single/wp-content/plugins-alt";
    let replies = vec![Host::deploy().reply(), version(), facts(alt)];
    let (result, _) = run(&site(|_| {}), replies).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch);
    let Error::Probe(failure) = error else {
        panic!("a probe failure");
    };
    assert_eq!(
        *failure,
        ProbeFailure::PluginsDirMismatch {
            expected: PLUGINS.into(),
            found: Some(alt.into()),
            identity: Identity::sudo("www-data", Some("www-data")).unwrap(),
        }
    );
    // Pinned: step H measures the pinned directory, which WordPress uses.
    let host = Host {
        walk: Some(walk_at(
            alt,
            &stat_of(2010, 33, "2775", "ftpuser", "www-data"),
        )),
        ..Host::deploy()
    };
    let pinned = site(|site| site.plugins_dir = Some(alt.into()));
    let replies = vec![host.reply(), version(), facts(alt), capable()];
    let (result, scripts) = run(&pinned, replies).await;
    let probe = result.unwrap();
    assert_eq!(
        probe.identity.identity,
        Identity::sudo("ftpuser", Some("www-data")).unwrap()
    );
    assert!(scripts[0].contains("'wp-content' 'plugins-alt' </dev/null"));
}

#[tokio::test]
async fn an_explicit_owner_that_does_not_own_the_dir_is_a_mismatch() {
    let host = Host {
        users: vec![FakeStep::ok("2010\n")],
        ..Host::deploy()
    };
    let site = site(|site| site.owner = Some(Owner::from("ftpuser")));
    let (result, scripts) = run(&site, vec![host.reply()]).await;
    assert_eq!(code(result), Code::PluginsDirMismatch);
    assert!(scripts[0].contains("id -u 'ftpuser' </dev/null"));
}

#[tokio::test]
async fn run_as_a_user_is_resolved_by_id() {
    // A run_as user that does not exist.
    let host = Host {
        users: vec![FakeStep::status(1, "", "id: 'ghost': no such user\n")],
        ..Host::deploy()
    };
    let site_ghost = site(|site| site.run_as = Some(RunAs::from("ghost")));
    let (result, _) = run(&site_ghost, vec![host.reply()]).await;
    assert_eq!(code(result), Code::InvalidInput);
    // One of uid 0 by another name, without allow_root.
    let host = Host {
        users: vec![FakeStep::ok("0\n")],
        ..Host::deploy()
    };
    let site_toor = site(|site| site.run_as = Some(RunAs::from("toor")));
    let (result, _) = run(&site_toor, vec![host.reply()]).await;
    assert_eq!(code(result), Code::OwnerIsRoot);
}

#[tokio::test]
async fn wp_cli_that_cannot_run_or_is_not_wp_cli_is_wp_cli_missing() {
    let cases = [
        // Captured: `env` without the program (GNU coreutils).
        FakeStep::status(127, "", "env: 'wp': No such file or directory\n"),
        FakeStep::status(1, "", "Could not open input file: /opt/wp-cli.phar\n"),
        FakeStep::ok("hello\n"),
        FakeStep::ok(""),
    ];
    for answer in cases {
        let replies = vec![Host::deploy().reply(), Reply::Steps(vec![answer.clone()])];
        let (result, _) = run(&site(|_| {}), replies).await;
        let error = result.unwrap_err();
        assert_eq!(error.code(), Code::WpCliMissing, "{answer:?}: {error}");
        assert!(
            error.to_string().starts_with("the site's wp_cli 'wp' "),
            "{error}"
        );
    }
    // WordPress reports another WP-CLI than `cli version` printed.
    let replies = vec![
        Host::deploy().reply(),
        Reply::Steps(vec![FakeStep::ok("WP-CLI 2.11.0\n")]),
        facts(PLUGINS),
    ];
    let (result, _) = run(&site(|_| {}), replies).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::WpCliMissing);
    assert!(
        error
            .to_string()
            .contains("printed 2.11.0, WordPress's WP_CLI_VERSION is 2.12.0")
    );
}

#[tokio::test]
async fn wp_clis_own_failures_are_classified() {
    // Captured from WP-CLI 2.12.0 on the harness.
    let cases = [
        (
            "Error: This does not seem to be a WordPress installation.\nThe used path is: \
             /var/www/single/\nPass --path=`path/to/wordpress` or run `wp core download`.\n",
            Code::NotWordpress,
        ),
        (
            "Error: 'wp-config.php' not found.\nEither create one manually or use `wp config \
             create`.\n",
            Code::WpNotInstalled,
        ),
        (
            "Error: The site you have requested is not installed.\nYour table prefix is \
             'none_'. Found installation with table prefix: wp_.\nOr, run `wp core install` to \
             create database tables.\n",
            Code::WpNotInstalled,
        ),
        (
            "Error: Error establishing a database connection.\n",
            Code::WpCommandFailed,
        ),
    ];
    for (stderr, expected) in cases {
        let replies = vec![
            Host::deploy().reply(),
            version(),
            Reply::Steps(vec![FakeStep::status(1, "", stderr)]),
        ];
        let (result, _) = run(&site(|_| {}), replies).await;
        let error = result.unwrap_err();
        assert_eq!(error.code(), expected, "{error}");
    }
    let replies = vec![
        Host::deploy().reply(),
        version(),
        Reply::Steps(vec![FakeStep::status(1, "", cases[0].0)]),
    ];
    let (result, _) = run(&site(|_| {}), replies).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "'/var/www/single' holds no WordPress: 'Error: This does not seem to be a WordPress \
         installation.'"
    );
}

#[tokio::test]
async fn a_missing_plugins_dir_is_not_wordpress_unless_pinned() {
    let host = Host {
        stat: FakeStep::status(1, "", "stat: cannot statx 'x': No such file or directory\n"),
        ..Host::deploy()
    };
    let (result, _) = run(&site(|_| {}), vec![host.reply()]).await;
    assert_eq!(code(result), Code::NotWordpress);
    let pinned = site(|site| site.plugins_dir = Some("/srv/plugins".into()));
    let (result, _) = run(&pinned, vec![host.reply()]).await;
    assert_eq!(code(result), Code::PluginsDirMismatch);
}

/// Step H of `deploy` on the FTP site, which it cannot reach.
fn denied() -> Host {
    let denied = "stat: cannot statx 'x': Permission denied\n";
    Host {
        stat: FakeStep::status(1, "", denied),
        ..Host::deploy()
    }
}

#[tokio::test]
async fn an_unreachable_plugins_dir_is_read_by_roots_stat() {
    let root_stat = || {
        Reply::Steps(vec![FakeStep::ok(
            "2010\n33\n2775\ndirectory\nftpuser\nwww-data\n",
        )])
    };
    let ftp = || stat_of(2010, 33, "2775", "ftpuser", "www-data");
    let confirmed = Reply::Steps(vec![walk_at(PLUGINS, &ftp())]);
    let replies = vec![
        denied().reply(),
        root_stat(),
        version(),
        confirmed,
        facts(PLUGINS),
        capable(),
    ];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    let probe = result.unwrap();
    assert!(probe.plugins_dir.read_by_root);
    assert_eq!(probe.plugins_dir.path, PLUGINS);
    assert_eq!(
        probe.identity.identity,
        Identity::sudo("ftpuser", Some("www-data")).unwrap()
    );
    assert!(scripts[1].contains(&format!(
        "sudo -n -u root -- stat -c '%u\n%g\n%a\n%F\n%U\n%G' -- '{PLUGINS}'"
    )));
    // Immediately before WordPress loads, the identity walks the path
    // itself, trusting links of root or the login user (2001).
    assert!(
        scripts[3].contains(
            "sudo -n -u 'ftpuser' -g 'www-data' -- env -u BASH_ENV -u ENV -u CDPATH sh -c"
        )
    );
    assert!(scripts[3].contains("' sh '2001' -c "));
    assert!(scripts[4].contains("'eval'"));
    // A link the site could plant on the way (root's stat follows a
    // linked wp-content): refused before W.
    let linked = Reply::Steps(vec![FakeStep::status(
        73,
        "",
        "apvm: unprotected link wp-content\n",
    )]);
    let replies = vec![denied().reply(), root_stat(), version(), linked];
    let (result, _) = run(&site(|_| {}), replies).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch);
    assert!(
        error
            .to_string()
            .contains("('apvm: unprotected link wp-content')"),
        "{error}"
    );
    // Linked for root's stat, restored before the identity walks: the
    // owner differs from root's reading, refused before W.
    let restored = Reply::Steps(vec![walk_at(
        PLUGINS,
        &stat_of(33, 33, "755", "www-data", "www-data"),
    )]);
    let replies = vec![denied().reply(), root_stat(), version(), restored];
    let (result, _) = run(&site(|_| {}), replies).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch);
    assert!(
        error.to_string().contains(
            "changed while it was probed: root's stat read uid 2010 gid 33, the identity found \
             uid 33 gid 33"
        ),
        "{error}"
    );
    // sudo refusing the identity's walk is a sudo failure.
    let replies = vec![
        denied().reply(),
        root_stat(),
        version(),
        refused(PASSWORD),
        Reply::Steps(vec![FakeStep::ok("/usr/bin/true\n")]),
    ];
    let (result, _) = run(&site(|_| {}), replies).await;
    assert_eq!(code(result), Code::SudoPasswordRequired);
}

#[tokio::test]
async fn roots_stat_refuses_links_and_needs_sudo() {
    // A symbolic link: root's stat describes it, never follows.
    let link = Reply::Steps(vec![FakeStep::ok("0\n0\n777\nsymbolic link\nroot\nroot\n")]);
    let (result, _) = run(&site(|_| {}), vec![denied().reply(), link]).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::PluginsDirMismatch);
    assert!(
        error
            .to_string()
            .contains("is a 'symbolic link', not a directory")
    );
    // No sudo.
    let host = Host {
        sudo: false,
        ..denied()
    };
    let (result, _) = run(&site(|_| {}), vec![host.reply()]).await;
    assert_eq!(code(result), Code::SudoMissing);
    // sudo refuses root's stat: the listing asks for `stat`.
    let replies = vec![
        denied().reply(),
        refused(PASSWORD),
        Reply::Steps(vec![FakeStep::status(1, "", "")]),
    ];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::SudoNotAllowed);
    assert!(scripts[2].contains("sudo -n -l -u 'root' stat"));
    assert!(
        error
            .to_string()
            .contains("deploy ALL=(root) NOPASSWD: /usr/bin/stat")
    );
}

#[tokio::test]
async fn a_bsd_stat_is_retried_with_its_own_format() {
    let host = Host {
        stat: FakeStep::status(1, "", "stat: illegal option -- c\nusage: stat …\n"),
        ..Host::deploy()
    };
    let bsd_stat = FakeStep::ok("33\n33\n0755\nDirectory\nwww-data\nwww-data\n");
    let bsd = Reply::Steps(vec![walk_at(PLUGINS, &bsd_stat), bsd_stat.clone()]);
    let replies = vec![host.reply(), bsd, version(), facts(PLUGINS), capable()];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    let probe = result.unwrap();
    assert_eq!(probe.plugins_dir.ownership.mode.bits(), 0o755);
    assert!(scripts[1].contains("' sh '' -f '%u%n%g%n%Mp%Lp%n%HT%n%Su%n%Sg' '%u %Lp' %u"));
    assert!(scripts[1].contains("stat -L -f '%u%n%g%n%Mp%Lp%n%HT%n%Su%n%Sg' --"));
    // Only a refused `-c` is retried: any other stat failure is unreadable.
    let broken = Host {
        stat: FakeStep::status(1, "", "stat: broken\n"),
        ..Host::deploy()
    };
    let (result, scripts) = run(&site(|_| {}), vec![broken.reply()]).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::ProbeFailed);
    assert_eq!(scripts.len(), 1);
    assert!(
        error
            .to_string()
            .contains("the plugins directory could not be read: 'stat: broken'"),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_runuser_is_a_probe_failure() {
    let host = Host {
        uid: 0,
        name: "root",
        runuser: true,
        ..Host::deploy()
    };
    let failed = Reply::Steps(vec![FakeStep::status(
        1,
        "",
        "runuser: user www-data does not exist or the user entry does not contain all the \
         required fields\n",
    )]);
    let (result, _) = run(&site(|_| {}), vec![host.reply(), failed]).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::ProbeFailed);
    assert!(
        error
            .to_string()
            .starts_with("runuser -u www-data -g www-data failed: 'runuser:")
    );
}

#[tokio::test]
async fn transport_failures_keep_their_codes() {
    let lost = Reply::Unframed {
        exit: crate::remote::Exit::TransportFailed,
        stdout: Vec::new(),
        stderr: b"ssh: connect to host x port 22: Connection refused\r\n".to_vec(),
    };
    let (result, _) = run(&site(|_| {}), vec![lost]).await;
    assert_eq!(code(result), Code::SshUnreachable);
    let (result, _) = run(&site(|_| {}), vec![Reply::timed_out("")]).await;
    assert_eq!(code(result), Code::Timeout);
    // Output without the step markers.
    let (result, _) = run(&site(|_| {}), vec![Reply::ok("garbage")]).await;
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::ProbeFailed);
    assert_eq!(
        error.to_string(),
        "probe step H printed output apvm cannot read: the output of step 0 is incomplete"
    );
}

#[tokio::test]
async fn the_sites_timeout_bounds_every_step() {
    let fake = FakeTransport::new(happy(&Host::deploy()));
    let site = site(|site| site.command_timeout_secs = Some(42));
    probe(&fake, &site).await.unwrap();
    assert!(
        fake.calls()
            .iter()
            .all(|call| call.timeout() == Duration::from_secs(42))
    );
    let fake = FakeTransport::new(happy(&Host::deploy()));
    probe(&fake, &self::site(|_| {})).await.unwrap();
    assert_eq!(fake.calls()[0].timeout(), DEFAULT_TIMEOUT);
}

#[tokio::test]
async fn capability_findings_are_warnings() {
    let host = Host {
        checksum: false,
        ..Host::deploy()
    };
    let read_only = Reply::Steps(vec![
        FakeStep::status(1, "", ""),
        FakeStep::status(1, "", "df: /tmp: No such file or directory\n"),
        FakeStep::ok(""),
        FakeStep::ok(""),
        FakeStep::ok(""),
        FakeStep::status(1, "", ""),
    ]);
    let site = site(|site| site.url = Some("https://shop.example.com".into()));
    let replies = vec![host.reply(), version(), facts(PLUGINS), read_only];
    let (result, _) = run(&site, replies).await;
    let probe = result.unwrap();
    assert!(!probe.capabilities.plugins_dir_writable);
    let codes: Vec<Option<Code>> = probe.warnings.iter().map(ProbeWarning::code).collect();
    assert!(codes.contains(&Some(Code::PluginsDirNotWritable)));
    assert!(probe.warnings.contains(&ProbeWarning::NoChecksumTool));
    assert!(probe.warnings.contains(&ProbeWarning::SiteurlDiffers {
        siteurl: "http://single.test".into(),
        url: "https://shop.example.com".into()
    }));
    assert!(
        probe
            .warnings
            .contains(&ProbeWarning::SpaceUnknown { dir: "/tmp".into() })
    );
}

#[tokio::test]
async fn a_plugins_dir_reached_through_a_link_is_refused_before_wordpress_loads() {
    // The site replaced `plugins` with a link to the login user's home: the
    // walk finds a link in wp-content, a directory the site may write.
    for (status, detail) in [
        (73, "apvm: unprotected link plugins"),
        // Swapped in between the walk's `-h` test and its `cd`.
        (72, "apvm: moved plugins"),
    ] {
        let host = Host {
            walk: Some(FakeStep::status(status, "", format!("{detail}\n"))),
            ..Host::deploy()
        };
        let (result, scripts) = run(&site(|_| {}), vec![host.reply()]).await;
        let error = result.unwrap_err();
        assert_eq!(error.code(), Code::PluginsDirMismatch);
        assert_eq!(
            error.to_string(),
            format!(
                "the plugins directory '{PLUGINS}' is reached through a symbolic link the site \
                 could have planted or replaced ('{detail}'): apvm follows a link only when it \
                 and the directory holding it belong to root or the login user and neither \
                 group nor others may write that directory (as for a release layout's current \
                 -> releases/<n>); fix the link's ownership, or check who owns the real \
                 directory and pin the site's plugins_dir (and path) to it"
            )
        );
        assert_eq!(scripts.len(), 1, "no identity, no WordPress");
    }
    // A protected link (a release layout): the physical path is used.
    let release = "/srv/releases/42/wp-content/plugins";
    let host = Host {
        walk: Some(walk_at(
            release,
            &stat_of(33, 33, "755", "www-data", "www-data"),
        )),
        ..Host::deploy()
    };
    let replies = vec![host.reply(), version(), facts(release), capable()];
    let (result, _) = run(&site(|_| {}), replies).await;
    assert_eq!(result.unwrap().plugins_dir.path, release);
}

#[tokio::test]
async fn sudo_rs_refusals_fall_back_and_classify() {
    // sudo-rs 0.2.13: a rule needing a password, then `-g`-less sudo works.
    let replies = vec![
        Host::deploy().reply(),
        refused("sudo: interactive authentication is required\n"),
        primary_gid("33"),
        version(),
        facts(PLUGINS),
        capable(),
    ];
    let (result, _) = run(&site(|_| {}), replies).await;
    assert_eq!(
        result.unwrap().identity.identity,
        Identity::sudo("www-data", None).unwrap()
    );
    // No rule at all: refused outright, no listing needed.
    let no_rule = "sudo-rs: I'm sorry deploy. I'm afraid I can't do that\n";
    let replies = vec![Host::deploy().reply(), refused(no_rule), refused(no_rule)];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    assert_eq!(code(result), Code::SudoNotAllowed);
    assert_eq!(scripts.len(), 3);
}

#[tokio::test]
async fn a_setgid_tree_member_is_probed_without_sudo() {
    let host = Host {
        uid: 2010,
        name: "ftpuser",
        groups: "2010 33",
        stat: stat_of(2010, 33, "2775", "ftpuser", "www-data"),
        ..Host::deploy()
    };
    let (result, scripts) = run(&site(|_| {}), happy(&host)).await;
    let probe = result.unwrap();
    assert_eq!(probe.identity.identity, Identity::Login);
    assert!(scripts.iter().all(|script| !script.contains("sudo -n")));
    assert_eq!(probe.warnings, [ProbeWarning::CheckedAfterCreation]);
}

#[tokio::test]
async fn unreadable_checks_only_warn() {
    let replies = vec![
        Host::deploy().reply(),
        version(),
        facts(PLUGINS),
        Reply::ok("garbage"),
    ];
    let (result, _) = run(&site(|_| {}), replies).await;
    let probe = result.unwrap();
    assert!(!probe.capabilities.plugins_dir_writable);
    assert_eq!(probe.capabilities.upgrade_dir, UpgradeDir::Unknown);
    assert!(matches!(
        probe.warnings.as_slice(),
        [ProbeWarning::ChecksUnreadable { .. }]
    ));
}

#[tokio::test]
async fn a_sudo_target_whose_primary_group_is_root_is_refused() {
    let replies = vec![Host::deploy().reply(), refused(PASSWORD), primary_gid("0")];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    // Refused before WP-CLI ran as it.
    assert!(!scripts[2].contains("'cli' 'version'"));
    assert_eq!(scripts.len(), 3);
    let error = result.unwrap_err();
    assert_eq!(error.code(), Code::OwnerIsRoot);
    assert!(
        error
            .to_string()
            .starts_with("the group WP-CLI would run with is root"),
        "{error}"
    );
    // sudo refusing `id -g`: classified like any refusal of the identity.
    let replies = vec![
        Host::deploy().reply(),
        refused(PASSWORD),
        refused(PASSWORD),
        Reply::Steps(vec![FakeStep::status(1, "", "")]),
    ];
    let (result, scripts) = run(&site(|_| {}), replies).await;
    assert_eq!(code(result), Code::SudoNotAllowed);
    assert!(scripts[2].contains("-- id -g"));
    // A non-numeric answer is unreadable.
    let replies = vec![
        Host::deploy().reply(),
        refused(PASSWORD),
        primary_gid("root"),
    ];
    let (result, _) = run(&site(|_| {}), replies).await;
    assert_eq!(code(result), Code::ProbeFailed);
    // With allow_root, no check runs.
    let replies = vec![
        Host::deploy().reply(),
        refused(PASSWORD),
        version(),
        facts(PLUGINS),
        capable(),
    ];
    let (result, _) = run(&site(|site| site.allow_root = Some(true)), replies).await;
    assert!(result.is_ok());
}
