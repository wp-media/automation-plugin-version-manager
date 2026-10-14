//! The identity table, row by row (plan §6.2), with every group case.

use super::*;

/// The login user's uid and gid in these tests (`deploy:deploy`).
const L: u32 = 2001;
/// `www-data`'s uid and gid.
const WWW: u32 = 33;
/// `ftpuser`'s uid.
const FTP: u32 = 2010;

/// A usable name.
///
/// # Arguments
///
/// * `name` - The name
fn name(name: &str) -> Option<AccountName> {
    Some(AccountName::new(name).unwrap())
}

/// An account of `id` named `text` (`""`: no name).
///
/// # Arguments
///
/// * `id` - The id
/// * `text` - The name, or `""`
fn account(id: u32, text: &str) -> Account {
    Account::new(id, if text.is_empty() { None } else { name(text) })
}

/// The login user `deploy` (2001:2001), or `root` when `uid` is 0.
///
/// # Arguments
///
/// * `uid` - Its uid (and primary gid)
fn login(uid: u32) -> LoginAccount {
    let text = if uid == 0 { "root" } else { "deploy" };
    LoginAccount {
        user: account(uid, text),
        group: account(uid, text),
        groups: vec![uid],
    }
}

/// A plugins directory owned by `owner:group`, mode 2775.
///
/// # Arguments
///
/// * `owner` - Its owner
/// * `group` - Its group
fn dir(owner: Account, group: Account) -> DirOwnership {
    DirOwnership {
        owner,
        group,
        mode: FileMode::new(0o2775).unwrap(),
    }
}

/// `www-data:www-data`.
fn www_dir() -> DirOwnership {
    dir(account(WWW, "www-data"), account(WWW, "www-data"))
}

/// Sudo and runuser both present.
const BOTH: SwitchTools = SwitchTools {
    sudo: true,
    runuser: true,
};
/// Only sudo.
const SUDO: SwitchTools = SwitchTools {
    sudo: true,
    runuser: false,
};
/// Neither.
const NONE: SwitchTools = SwitchTools {
    sudo: false,
    runuser: false,
};

/// The names `id -u` resolves on the test server.
const USERS: [(&str, u32); 5] = [
    ("deploy", L),
    ("www-data", WWW),
    ("ftpuser", FTP),
    ("root", 0),
    ("toor", 0),
];

/// One table row's inputs, with the defaults `auto`/`auto`, no root.
struct Row {
    /// `run_as`.
    run_as: RunAs,
    /// `owner`.
    owner: Owner,
    /// `allow_root`.
    allow_root: bool,
    /// The login user.
    login: LoginAccount,
    /// The plugins directory.
    dir: DirOwnership,
    /// The tools.
    tools: SwitchTools,
}

impl Row {
    /// `deploy` on a `www-data:www-data` directory, sudo only.
    fn new() -> Self {
        Self {
            run_as: RunAs::Auto,
            owner: Owner::Auto,
            allow_root: false,
            login: login(L),
            dir: www_dir(),
            tools: SUDO,
        }
    }

    /// Plan this row.
    fn plan(&self) -> Result<IdentityPlan, IdentityRefusal> {
        let users: Vec<(String, u32)> = USERS
            .iter()
            .map(|(name, uid)| (name.to_string(), *uid))
            .collect();
        plan_identity(&IdentityFacts {
            run_as: &self.run_as,
            owner: &self.owner,
            allow_root: self.allow_root,
            login: &self.login,
            dir: &self.dir,
            tools: self.tools,
            users: &users,
        })
    }

    /// The candidates, as `(prefix kind, uid, creation)`.
    fn candidates(&self) -> Vec<(Identity, u32, Creation)> {
        let plan = self.plan().unwrap();
        plan.candidates
            .into_iter()
            .map(|c| (c.identity, c.uid, c.creation))
            .collect()
    }
}

/// `sudo -n -u <user> [-g <group>]`.
///
/// # Arguments
///
/// * `user` - The user
/// * `group` - The group, or `""`
fn sudo(user: &str, group: &str) -> Identity {
    Identity::sudo(user, (!group.is_empty()).then_some(group)).unwrap()
}

/// `runuser -u <user> -g <group>`.
///
/// # Arguments
///
/// * `user` - The user
/// * `group` - The group
fn runuser(user: &str, group: &str) -> Identity {
    Identity::runuser(user, group).unwrap()
}

use Creation::{AtCreation, CheckAfter};

#[test]
fn auto_owner_is_the_login_user_with_its_own_group() {
    let row = Row {
        dir: dir(account(L, "deploy"), account(L, "deploy")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, AtCreation)]);
    let plan = row.plan().unwrap();
    assert_eq!(plan.reference.owner, account(L, "deploy"));
    assert_eq!(plan.reference.mode.bits(), 0o2775);
}

#[test]
fn auto_owner_is_the_login_user_with_another_group() {
    let row = Row {
        dir: dir(account(L, "deploy"), account(WWW, "www-data")),
        ..Row::new()
    };
    // sudo -u L -g G first; no prefix (repaired after) when sudo refuses.
    assert_eq!(
        row.candidates(),
        [
            (sudo("deploy", "www-data"), L, AtCreation),
            (Identity::Login, L, CheckAfter)
        ]
    );
    // No sudo: no prefix, checked after.
    let row = Row { tools: NONE, ..row };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
    // Membership in G does not make the login's own files group G.
    let row = Row {
        login: LoginAccount {
            groups: vec![L, WWW],
            ..login(L)
        },
        ..row
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
}

#[test]
fn a_setgid_tree_whose_group_the_owner_is_in_needs_no_sudo() {
    // ftpuser on its FTP tree (2775, ftpuser:www-data), member of www-data:
    // no `sudo -u ftpuser -g www-data` that would be refused and logged.
    let member = LoginAccount {
        user: account(FTP, "ftpuser"),
        group: account(FTP, "ftpuser"),
        groups: vec![FTP, WWW],
    };
    let row = Row {
        login: member.clone(),
        dir: dir(account(FTP, "ftpuser"), account(WWW, "www-data")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, FTP, CheckAfter)]);
    // Without setgid, sudo -g still comes first.
    let row = Row {
        dir: DirOwnership {
            mode: FileMode::new(0o775).unwrap(),
            ..dir(account(FTP, "ftpuser"), account(WWW, "www-data"))
        },
        ..row
    };
    assert_eq!(
        row.candidates()[0],
        (sudo("ftpuser", "www-data"), FTP, AtCreation)
    );
    // setgid, but not a member: sudo -g first.
    let row = Row {
        login: LoginAccount {
            groups: vec![FTP],
            ..member
        },
        dir: dir(account(FTP, "ftpuser"), account(WWW, "www-data")),
        ..Row::new()
    };
    assert_eq!(row.candidates().len(), 2);
}

#[test]
fn auto_same_user_without_usable_names_skips_sudo() {
    // The group has no name.
    let row = Row {
        dir: dir(account(L, "deploy"), account(4242, "")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
    // The login user has no name.
    let row = Row {
        login: LoginAccount {
            user: account(L, ""),
            ..login(L)
        },
        dir: dir(account(L, ""), account(WWW, "www-data")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
}

#[test]
fn auto_another_owner_through_sudo_with_then_without_the_group() {
    assert_eq!(
        Row::new().candidates(),
        [
            (sudo("www-data", "www-data"), WWW, AtCreation),
            (sudo("www-data", ""), WWW, CheckAfter)
        ]
    );
    // An FTP-style tree: ftpuser:www-data.
    let row = Row {
        dir: dir(account(FTP, "ftpuser"), account(WWW, "www-data")),
        ..Row::new()
    };
    assert_eq!(
        row.candidates(),
        [
            (sudo("ftpuser", "www-data"), FTP, AtCreation),
            (sudo("ftpuser", ""), FTP, CheckAfter)
        ]
    );
    // A group without a name: only `sudo -u U`.
    let row = Row {
        dir: dir(account(FTP, "ftpuser"), account(4242, "")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(sudo("ftpuser", ""), FTP, CheckAfter)]);
}

#[test]
fn auto_another_owner_from_a_root_login() {
    // runuser when present, whatever sudo is.
    for tools in [
        BOTH,
        SwitchTools {
            sudo: false,
            runuser: true,
        },
    ] {
        let row = Row {
            login: login(0),
            tools,
            ..Row::new()
        };
        assert_eq!(
            row.candidates(),
            [(runuser("www-data", "www-data"), WWW, AtCreation)]
        );
    }
    // No runuser: sudo.
    let row = Row {
        login: login(0),
        ..Row::new()
    };
    assert_eq!(
        row.candidates()[0],
        (sudo("www-data", "www-data"), WWW, AtCreation)
    );
    // Neither.
    let row = Row {
        login: login(0),
        tools: NONE,
        ..Row::new()
    };
    let refusal = row.plan().unwrap_err();
    assert_eq!(refusal.code(), Code::SudoMissing);
    // runuser needs the group's name: sudo without -g when there is sudo,
    // else refused.
    let row = Row {
        login: login(0),
        tools: BOTH,
        dir: dir(account(WWW, "www-data"), account(4242, "")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(sudo("www-data", ""), WWW, CheckAfter)]);
    let row = Row {
        tools: SwitchTools {
            sudo: false,
            runuser: true,
        },
        ..row
    };
    assert_eq!(
        row.plan().unwrap_err(),
        IdentityRefusal::Unnamed {
            role: NamedRole::Group,
            account: account(4242, "")
        }
    );
}

#[test]
fn auto_another_owner_without_sudo_is_sudo_missing() {
    let row = Row {
        tools: NONE,
        ..Row::new()
    };
    let refusal = row.plan().unwrap_err();
    assert_eq!(
        refusal,
        IdentityRefusal::SudoMissing {
            user: account(WWW, "www-data")
        }
    );
    assert_eq!(refusal.code(), Code::SudoMissing);
    assert_eq!(
        refusal.to_string(),
        "running WP-CLI as 'www-data' (33) needs sudo (or runuser, for a root login), and the \
         server has neither: install sudo with a password-less rule for the login user"
    );
    // runuser alone does not help a login that is not root.
    let row = Row {
        tools: SwitchTools {
            sudo: false,
            runuser: true,
        },
        ..Row::new()
    };
    assert_eq!(row.plan().unwrap_err().code(), Code::SudoMissing);
}

#[test]
fn an_owner_without_a_usable_name_cannot_be_switched_to() {
    let row = Row {
        dir: dir(account(1005, ""), account(WWW, "www-data")),
        ..Row::new()
    };
    let refusal = row.plan().unwrap_err();
    assert_eq!(refusal.code(), Code::InvalidInput);
    assert_eq!(
        refusal.to_string(),
        "the user WP-CLI must run as has no name apvm can pass to sudo or runuser (id 1005); set \
         the site's run_as and owner to a valid user name"
    );
}

#[test]
fn root_is_decided_by_id_and_needs_allow_root() {
    let cases = [
        // Owner uid 0, whatever its name.
        (
            dir(account(0, "root"), account(WWW, "www-data")),
            RootRole::Owner,
            0,
        ),
        (
            dir(account(0, "toor"), account(WWW, "www-data")),
            RootRole::Owner,
            0,
        ),
        (
            dir(account(0, ""), account(WWW, "www-data")),
            RootRole::Owner,
            0,
        ),
    ];
    for (dir, role, id) in cases {
        let row = Row {
            dir: dir.clone(),
            ..Row::new()
        };
        let refusal = row.plan().unwrap_err();
        assert_eq!(refusal.code(), Code::OwnerIsRoot, "{dir:?}");
        assert!(matches!(
            refusal,
            IdentityRefusal::OwnerIsRoot { role: r, account: Account { id: i, .. } } if r == role && i == id
        ));
    }
    // A root group alone is not refused: the identity runs without `-g`
    // (never with gid 0), checked after.
    let row = Row {
        dir: dir(account(WWW, "www-data"), account(0, "root")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(sudo("www-data", ""), WWW, CheckAfter)]);
    // The login owns the tree, the group is root: no prefix.
    let row = Row {
        dir: dir(account(L, "deploy"), account(0, "root")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
    // A root login: runuser needs -g, so sudo without it, else refused.
    let row = Row {
        login: login(0),
        tools: BOTH,
        dir: dir(account(WWW, "www-data"), account(0, "root")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(sudo("www-data", ""), WWW, CheckAfter)]);
    let row = Row {
        tools: SwitchTools {
            sudo: false,
            runuser: true,
        },
        ..row
    };
    assert!(matches!(
        row.plan().unwrap_err(),
        IdentityRefusal::OwnerIsRoot {
            role: RootRole::Group,
            ..
        }
    ));
    // A name that only looks like root is not root.
    let row = Row {
        dir: dir(account(1000, "Root"), account(1000, "Root")),
        ..Row::new()
    };
    assert_eq!(row.candidates()[0].1, 1000);
}

#[test]
fn allowed_root_runs_as_uid_0() {
    // Login is root and owns the tree: no prefix, uid 0.
    let row = Row {
        login: login(0),
        dir: dir(account(0, "root"), account(0, "root")),
        allow_root: true,
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, 0, AtCreation)]);
    // Another login: sudo to root (by the name stat printed).
    let row = Row {
        dir: dir(account(0, "toor"), account(0, "wheel")),
        allow_root: true,
        ..Row::new()
    };
    assert_eq!(
        row.candidates(),
        [
            (sudo("toor", "wheel"), 0, AtCreation),
            (sudo("toor", ""), 0, CheckAfter)
        ]
    );
    // Only the group is root: a normal uid.
    let row = Row {
        dir: dir(account(WWW, "www-data"), account(0, "root")),
        allow_root: true,
        ..Row::new()
    };
    assert_eq!(
        row.candidates()[0],
        (sudo("www-data", "root"), WWW, AtCreation)
    );
}

#[test]
fn run_as_login_never_switches() {
    let row = Row {
        run_as: RunAs::Login,
        tools: BOTH,
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
    let row = Row {
        run_as: RunAs::Login,
        dir: dir(account(L, "deploy"), account(L, "deploy")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, AtCreation)]);
    // Same user, other group: still no prefix.
    let row = Row {
        run_as: RunAs::Login,
        dir: dir(account(L, "deploy"), account(WWW, "www-data")),
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
    // A root login runs as root only with allow_root.
    let row = Row {
        run_as: RunAs::Login,
        login: login(0),
        ..Row::new()
    };
    assert!(matches!(
        row.plan().unwrap_err(),
        IdentityRefusal::OwnerIsRoot {
            role: RootRole::Identity,
            ..
        }
    ));
    let row = Row {
        allow_root: true,
        ..row
    };
    assert_eq!(row.candidates(), [(Identity::Login, 0, CheckAfter)]);
}

#[test]
fn run_as_a_user_is_auto_with_that_user() {
    let row = Row {
        run_as: RunAs::User("ftpuser".into()),
        ..Row::new()
    };
    // ftpuser is not the reference owner (www-data): its files never get
    // the reference ownership at creation, `-g` or not.
    assert_eq!(
        row.candidates(),
        [
            (sudo("ftpuser", "www-data"), FTP, CheckAfter),
            (sudo("ftpuser", ""), FTP, CheckAfter)
        ]
    );
    // The reference stays the directory's.
    assert_eq!(
        row.plan().unwrap().reference.owner,
        account(WWW, "www-data")
    );
    // The login user by name: the same-user rows.
    let row = Row {
        run_as: RunAs::User("deploy".into()),
        ..Row::new()
    };
    assert_eq!(
        row.candidates(),
        [
            (sudo("deploy", "www-data"), L, CheckAfter),
            (Identity::Login, L, CheckAfter)
        ]
    );
    // Root by id through another name.
    let row = Row {
        run_as: RunAs::User("toor".into()),
        ..Row::new()
    };
    assert!(matches!(
        row.plan().unwrap_err(),
        IdentityRefusal::OwnerIsRoot {
            role: RootRole::Identity,
            ..
        }
    ));
    // Unknown on the server.
    let row = Row {
        run_as: RunAs::User("ghost".into()),
        ..Row::new()
    };
    let refusal = row.plan().unwrap_err();
    assert_eq!(refusal.code(), Code::InvalidInput);
    assert_eq!(
        refusal.to_string(),
        "the site's run_as names the user 'ghost', which does not exist on the server"
    );
}

#[test]
fn an_explicit_owner_must_own_the_directory() {
    // Owns it: the reference names it.
    let row = Row {
        owner: Owner::from("www-data"),
        ..Row::new()
    };
    assert_eq!(
        row.plan().unwrap().reference.owner,
        account(WWW, "www-data")
    );
    let row = Row {
        owner: Owner::from("www-data:www-data"),
        ..Row::new()
    };
    assert_eq!(row.candidates()[0].0, sudo("www-data", "www-data"));
    // Another user owns it.
    let row = Row {
        owner: Owner::from("ftpuser"),
        ..Row::new()
    };
    let refusal = row.plan().unwrap_err();
    assert_eq!(refusal.code(), Code::PluginsDirMismatch);
    assert_eq!(
        refusal.to_string(),
        "the site's owner 'ftpuser' does not own the plugins directory, which belongs to \
         'www-data' (33)"
    );
    // Another group.
    let row = Row {
        owner: Owner::from("www-data:ftpuser"),
        ..Row::new()
    };
    assert_eq!(
        row.plan().unwrap_err(),
        IdentityRefusal::OwnerMismatch {
            expected: "www-data:ftpuser".into(),
            found: account(WWW, "www-data")
        }
    );
    // A group without a name never matches.
    let row = Row {
        owner: Owner::from("www-data:www-data"),
        dir: dir(account(WWW, "www-data"), account(WWW, "")),
        ..Row::new()
    };
    assert_eq!(row.plan().unwrap_err().code(), Code::PluginsDirMismatch);
    // Unknown on the server.
    let row = Row {
        owner: Owner::from("ghost"),
        ..Row::new()
    };
    let refusal = row.plan().unwrap_err();
    assert_eq!(refusal.code(), Code::PluginsDirMismatch);
    assert!(refusal.to_string().contains("owner names the user 'ghost'"));
}

#[test]
fn an_explicit_owner_of_uid_0_needs_allow_root() {
    let row = Row {
        owner: Owner::from("toor"),
        dir: dir(account(0, "root"), account(WWW, "www-data")),
        ..Row::new()
    };
    assert!(matches!(
        row.plan().unwrap_err(),
        IdentityRefusal::OwnerIsRoot {
            role: RootRole::Owner,
            ..
        }
    ));
    // Allowed: it runs as the name the setting gave.
    let row = Row {
        allow_root: true,
        ..row
    };
    assert_eq!(
        row.candidates()[0],
        (sudo("toor", "www-data"), 0, AtCreation)
    );
}

#[test]
fn root_refusals_say_what_is_root() {
    let row = Row {
        dir: dir(account(0, "root"), account(0, "root")),
        ..Row::new()
    };
    assert_eq!(
        row.plan().unwrap_err().to_string(),
        "the plugins directory's owner is root ('root' (0)); apvm runs nothing as root unless \
         the site sets allow_root"
    );
}

#[test]
fn a_login_whose_primary_group_is_root_needs_allow_root() {
    let root_group = LoginAccount {
        user: account(L, "deploy"),
        group: account(0, "root"),
        groups: vec![0],
    };
    // The login would be the identity: refused.
    let row = Row {
        login: root_group.clone(),
        run_as: RunAs::Login,
        ..Row::new()
    };
    assert!(matches!(
        row.plan().unwrap_err(),
        IdentityRefusal::OwnerIsRoot {
            role: RootRole::PrimaryGroup,
            ..
        }
    ));
    // It owns the tree, the group differs: only `sudo -u L -g G` is kept.
    let row = Row {
        login: root_group.clone(),
        dir: dir(account(L, "deploy"), account(WWW, "www-data")),
        ..Row::new()
    };
    assert_eq!(
        row.candidates(),
        [(sudo("deploy", "www-data"), L, AtCreation)]
    );
    // Allowed: no change.
    let row = Row {
        login: root_group,
        run_as: RunAs::Login,
        allow_root: true,
        ..Row::new()
    };
    assert_eq!(row.candidates(), [(Identity::Login, L, CheckAfter)]);
}
