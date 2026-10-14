//! Step H's commands and readers, with outputs captured from the harness
//! (Debian 13 coreutils) and from macOS's BSD `stat`.

use super::*;
use crate::inventory::{Inventory, Server, Site};

/// A step that exited `status`.
///
/// # Arguments
///
/// * `status` - Its exit status
/// * `stdout` - Its output
/// * `stderr` - Its messages
fn step(status: i32, stdout: &str, stderr: &str) -> StepOutput {
    StepOutput {
        status,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        stderr_truncated: false,
    }
}

/// The plugins directory these tests read.
const PLUGINS: &str = "/var/www/single/wp-content/plugins";

/// A successful step printing `stdout`.
///
/// # Arguments
///
/// * `stdout` - Its output
fn ok(stdout: &str) -> StepOutput {
    step(0, stdout, "")
}

/// A site with `run_as` and `owner` set when given.
///
/// # Arguments
///
/// * `run_as` - The `run_as` text, if any
/// * `owner` - The `owner` text, if any
fn site(run_as: Option<&str>, owner: Option<&str>) -> ResolvedSite {
    let mut inventory = Inventory::new();
    inventory
        .add_server(Server::new("s", "203.0.113.1"))
        .unwrap();
    let mut site = Site::new("shop", "s", "/var/www/shop");
    site.run_as = run_as.map(RunAs::from);
    site.owner = owner.map(Owner::from);
    inventory.add_site(site).unwrap();
    inventory.resolve_site("shop").unwrap()
}

/// A healthy step H as `deploy` (2001) on Debian: sudo, no runuser on its
/// PATH, every checksum tool.
fn host_steps() -> Vec<StepOutput> {
    vec![
        ok("2001\n"),
        ok("2001\n"),
        ok("deploy\n"),
        ok("deploy\n"),
        ok("2001 33\n"),
        ok("/bin/bash\n"),
        ok("/usr/bin/sudo\n"),
        step(1, "", ""),
        ok("/usr/bin/sha256sum\n"),
        ok("/usr/bin/shasum\n"),
        ok("/usr/bin/openssl\n"),
        ok("/usr/local/bin/wp\n"),
        ok("/var/www/single/wp-content/plugins\n"),
        ok("33\n33\n755\ndirectory\nwww-data\nwww-data\n"),
    ]
}

#[test]
fn checks_come_in_the_readers_order() {
    let users = [AccountName::new("ftpuser").unwrap()];
    let checks = checks("wp", "/srv/p", &users);
    assert_eq!(checks.len(), at::USERS + 1);
    assert_eq!(checks[at::UID], Inspect::Uid);
    assert_eq!(checks[at::SHELL], Inspect::Shell);
    assert_eq!(checks[at::TOOLS], Inspect::Which("sudo"));
    assert_eq!(checks[at::WP_CLI], Inspect::Which("wp"));
    assert_eq!(
        checks[at::WALK],
        Inspect::Walk {
            path: "/srv/p",
            login_uid: None,
            flavor: StatFlavor::Gnu
        }
    );
    assert_eq!(checks[at::STAT], Inspect::StatGnu("/srv/p"));
    assert_eq!(checks[at::USERS], Inspect::UserId(&users[0]));
}

#[test]
fn login_checks_are_the_start_of_step_h() {
    let login = login_checks("/opt/wp");
    assert_eq!(login.len(), at::WP_CLI + 1);
    assert_eq!(login[at::WP_CLI], Inspect::Which("/opt/wp"));
    let users = [AccountName::new("ftpuser").unwrap()];
    assert_eq!(
        checks("/opt/wp", "/srv/p", &users)[..login.len()],
        login[..]
    );
    // A server's test reads its answers with step H's reader.
    let facts = host_facts(&host_steps()[..=at::WP_CLI]).unwrap();
    assert_eq!(facts, host_facts(&host_steps()).unwrap());
}

#[test]
fn named_users_come_from_run_as_and_owner_once_each() {
    let names = |run_as, owner| -> Vec<String> {
        named_users(&site(run_as, owner))
            .iter()
            .map(|name| name.as_str().to_string())
            .collect()
    };
    assert_eq!(names(None, None), Vec::<String>::new());
    assert_eq!(names(Some("auto"), Some("auto")), Vec::<String>::new());
    assert_eq!(names(Some("login"), None), Vec::<String>::new());
    assert_eq!(names(Some("ftpuser"), None), ["ftpuser"]);
    assert_eq!(names(None, Some("www-data:www-data")), ["www-data"]);
    assert_eq!(
        names(Some("ftpuser"), Some("www-data")),
        ["ftpuser", "www-data"]
    );
    assert_eq!(names(Some("ftpuser"), Some("ftpuser")), ["ftpuser"]);
}

#[test]
fn host_facts_read_ids_names_and_tools() {
    let facts = host_facts(&host_steps()).unwrap();
    assert_eq!(
        facts.login,
        LoginAccount {
            user: Account::new(2001, AccountName::new("deploy").ok()),
            group: Account::new(2001, AccountName::new("deploy").ok()),
            groups: vec![2001, 33],
        }
    );
    assert_eq!(facts.shell.as_deref(), Some("/bin/bash"));
    assert_eq!(facts.sudo.as_deref(), Some("/usr/bin/sudo"));
    assert_eq!(facts.runuser, None);
    assert_eq!(facts.checksum.as_deref(), Some("/usr/bin/sha256sum"));
    assert_eq!(facts.wp_cli_path.as_deref(), Some("/usr/local/bin/wp"));
    assert_eq!(
        facts.switch_tools(),
        SwitchTools {
            sudo: true,
            runuser: false
        }
    );
}

#[test]
fn host_facts_tolerate_missing_names_and_tools() {
    let mut steps = host_steps();
    // `id -un` fails for a uid without a name; a name breaking the rule.
    steps[at::USER_NAME] = step(1, "", "id: cannot find name for user ID 2001\n");
    steps[at::GROUP_NAME] = ok("a b\n");
    steps[at::GROUPS] = step(1, "", "");
    steps[at::SHELL] = step(1, "", "");
    for tool in 0..TOOLS.len() {
        steps[at::TOOLS + tool] = step(1, "", "");
    }
    steps[at::TOOLS + 4] = ok("/usr/bin/openssl\n");
    let facts = host_facts(&steps).unwrap();
    assert_eq!(facts.login.user.name, None);
    assert_eq!(facts.login.group.name, None);
    assert_eq!(facts.login.groups, [2001]);
    assert_eq!((facts.shell, facts.sudo), (None, None));
    assert_eq!(facts.checksum.as_deref(), Some("/usr/bin/openssl"));
}

#[test]
fn unreadable_ids_fail_step_h() {
    let cases: [(usize, StepOutput, &str); 5] = [
        (at::UID, step(1, "", ""), "id -u exited 1"),
        (at::UID, ok("abc\n"), "id -u printed no decimal id"),
        (at::GID, ok("99999999999\n"), "id -g printed no decimal id"),
        (at::GID, ok("\n"), "id -g printed no decimal id"),
        (at::GROUPS, ok("1 x\n"), "id -G printed a non-numeric id"),
    ];
    for (index, answer, reason) in cases {
        let mut steps = host_steps();
        steps[index] = answer;
        assert_eq!(
            host_facts(&steps).unwrap_err(),
            ProbeFailure::Unreadable {
                step: "H",
                reason: reason.into()
            }
        );
    }
    assert!(host_facts(&host_steps()[..3]).is_err());
}

#[test]
fn user_ids_keep_resolved_names_only() {
    let users = [
        AccountName::new("ftpuser").unwrap(),
        AccountName::new("ghost").unwrap(),
    ];
    let mut steps = host_steps();
    steps.push(ok("2010\n"));
    steps.push(step(1, "", "id: 'ghost': no such user\n"));
    assert_eq!(user_ids(&steps, &users), [("ftpuser".to_string(), 2010)]);
}

#[test]
fn gnu_and_bsd_stat_read_the_same() {
    let gnu = ok("33\n33\n2775\ndirectory\nwww-data\nwww-data\n");
    // macOS: `stat -L -f '%u%n%g%n%Mp%Lp%n%HT%n%Su%n%Sg'`.
    let bsd = ok("33\n33\n2775\nDirectory\nwww-data\nwww-data\n");
    for answer in [gnu, bsd] {
        let lines = stat_lines(&answer).unwrap();
        assert!(lines.is_directory());
        let ownership = lines.ownership();
        assert_eq!(
            ownership.owner,
            Account::new(33, AccountName::new("www-data").ok())
        );
        assert_eq!(ownership.mode.bits(), 0o2775);
    }
    // BSD's four-digit mode, and names it prints as ids.
    let lines = stat_lines(&ok("501\n20\n0755\nDirectory\n501\n20\n")).unwrap();
    assert_eq!(lines.ownership().mode.bits(), 0o755);
    assert_eq!(lines.ownership().owner.name, None);
    assert_eq!(lines.ownership().group.name, None);
    // GNU's UNKNOWN, and a name with a space: no name.
    let lines = stat_lines(&ok("1005\n1005\n755\ndirectory\nUNKNOWN\nweb users\n")).unwrap();
    assert_eq!(
        (lines.ownership().owner.name, lines.ownership().group.name),
        (None, None)
    );
    // A link root's stat describes.
    let link = stat_lines(&ok("0\n0\n777\nsymbolic link\nroot\nroot\n")).unwrap();
    assert!(!link.is_directory());
    assert_eq!(link.kind(), "symbolic link");
}

#[test]
fn malformed_stat_output_is_unreadable() {
    for (stdout, reason) in [
        (
            "33\n33\n755\ndirectory\nwww-data\n",
            "stat printed 5 lines instead of 6",
        ),
        (
            "33\n33\n755\ndirectory\nwww\ndata\nx\n",
            "stat printed 7 lines instead of 6",
        ),
        (
            "x\n33\n755\ndirectory\nw\nw\n",
            "stat printed a non-numeric id",
        ),
        (
            "33\n-1\n755\ndirectory\nw\nw\n",
            "stat printed a non-numeric id",
        ),
        (
            "33\n33\n0o755\ndirectory\nw\nw\n",
            "stat printed an invalid mode",
        ),
        (
            "33\n33\n17777\ndirectory\nw\nw\n",
            "stat printed an invalid mode",
        ),
    ] {
        assert_eq!(
            stat_lines(&ok(stdout)).unwrap_err(),
            ProbeFailure::Unreadable {
                step: "H",
                reason: reason.into()
            },
            "{stdout:?}"
        );
    }
    let invalid = StepOutput {
        stdout: vec![0xff],
        ..ok("")
    };
    assert!(stat_lines(&invalid).is_err());
}

/// A successful walk ending in `path` with `six` stat lines.
///
/// # Arguments
///
/// * `path` - The physical path
/// * `six` - The six lines
fn walk(path: &str, six: &str) -> StepOutput {
    ok(&format!("{path}\n{six}"))
}

/// The walk's failure to enter a component (dash says no reason).
fn cd_failed() -> StepOutput {
    step(70, "", "sh: 1: cd: can't cd to plugins\n")
}

#[test]
fn dir_reads_tell_every_outcome_apart() {
    let six = "33\n33\n755\ndirectory\nwww-data\nwww-data\n";
    let stat = ok(six);
    assert!(matches!(
        dir_read(&walk(PLUGINS, six), &stat).unwrap(),
        DirRead::Found { path, ownership } if path == PLUGINS && ownership.owner.id == 33
    ));
    // A protected link on the way: the physical path.
    assert!(matches!(
        dir_read(&walk("/srv/releases/7/wp-content/plugins", six), &stat).unwrap(),
        DirRead::Found { path, .. } if path == "/srv/releases/7/wp-content/plugins"
    ));
    // Captured on the harness: deploy on ftpsite.
    let denied = step(
        1,
        "",
        "stat: cannot statx '/home/ftpuser/public_html/wp-content/plugins': Permission denied\n",
    );
    assert_eq!(dir_read(&cd_failed(), &denied).unwrap(), DirRead::Denied);
    let missing = step(
        1,
        "",
        "stat: cannot statx '/x': No such file or directory\n",
    );
    assert_eq!(dir_read(&cd_failed(), &missing).unwrap(), DirRead::Missing);
    // macOS's stat refuses -c; GNU's says "invalid option".
    for refusal in [
        "stat: illegal option -- c\nusage: stat [-FLnq] …\n",
        "stat: invalid option -- 'c'\n",
    ] {
        let bsd = step(1, "", refusal);
        assert_eq!(dir_read(&step(1, "", ""), &bsd).unwrap(), DirRead::BsdStat);
    }
    // Any other stat failure is not retried.
    assert_eq!(
        dir_read(&cd_failed(), &step(1, "", "stat: broken\n")).unwrap(),
        DirRead::Other {
            message: Some("stat: broken".into())
        }
    );
    let file = ok("33\n33\n644\nregular file\nwww-data\nwww-data\n");
    assert_eq!(
        dir_read(&cd_failed(), &file).unwrap(),
        DirRead::NotDirectory {
            found: "regular file".into()
        }
    );
    // The walk could not enter a directory stat can see (dash's `cd` says
    // no reason, e.g. a 0750 plugins dir of www-data): denied.
    assert_eq!(dir_read(&cd_failed(), &stat).unwrap(), DirRead::Denied);
    // The walk failed otherwise but stat reads a directory: its message.
    assert_eq!(
        dir_read(&step(74, "", "stat: x\n"), &stat).unwrap(),
        DirRead::Other {
            message: Some("stat: x".into())
        }
    );
}

#[test]
fn links_the_site_could_plant_are_refused() {
    let stat = ok("2001\n2001\n755\ndirectory\ndeploy\ndeploy\n");
    for (status, message) in [
        (73, "apvm: unprotected link plugins"),
        (72, "apvm: moved plugins"),
        (75, "apvm: too many links at loop"),
    ] {
        let walk = step(status, "", &format!("{message}\n"));
        assert_eq!(
            dir_read(&walk, &stat).unwrap(),
            DirRead::Linked {
                detail: Some(message.into())
            }
        );
    }
}

#[test]
fn a_walk_apvm_cannot_read_is_unreadable() {
    let six = "33\n33\n755\ndirectory\nwww-data\nwww-data\n";
    let stat = ok(six);
    for walk in [
        ok("relative\n33\n"),
        walk("/a\\b", six),
        walk("/a/", six),
        ok("/srv/p"),
        walk("/srv/p", "33\n33\n755\ndirectory\n"),
        walk("/srv/p", "33\n33\n755\nsymbolic link\nw\nw\n"),
    ] {
        assert!(
            matches!(
                dir_read(&walk, &stat),
                Err(ProbeFailure::Unreadable { step: "H", .. })
            ),
            "{walk:?}"
        );
    }
}
