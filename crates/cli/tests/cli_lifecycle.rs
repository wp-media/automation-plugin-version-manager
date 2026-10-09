//! End-to-end tests of the commands that change the user's machine:
//! `apvm skill install|uninstall`, `apvm uninstall`, and the background
//! update check's opt-outs.
//!
//! Each test runs inside a hermetic [`common::Sandbox`]. `apvm uninstall`
//! only ever runs against a **throwaway copy** of the binary placed in the
//! fake home's `.apvm/bin/` (the installer layout), so even a regression in
//! its confirmation logic can delete nothing but that copy.
#![cfg(unix)]

mod common;

use std::path::Path;

use common::{Sandbox, assert_exit, repo_skill_file, stderr};

/// Files the binary embeds and installs, relative to the skill directory.
const SKILL_FILES: [&str; 2] = ["SKILL.md", "references/git-refs.md"];

/// Assert `dir` holds a byte-exact copy of every embedded skill file.
#[track_caller]
fn assert_skill_installed(dir: &Path) {
    for rel in SKILL_FILES {
        let installed = std::fs::read(dir.join(rel))
            .unwrap_or_else(|e| panic!("{} missing: {e}", dir.join(rel).display()));
        assert_eq!(
            installed,
            repo_skill_file(rel),
            "{rel} differs from the repository copy"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// skill install / uninstall
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn skill_install_writes_the_project_local_skill_and_warns_outside_git() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["skill", "install"]);
    assert_exit(&out, 0);

    assert_skill_installed(&sandbox.local_skill_dir());
    assert!(
        !sandbox.global_skill_dir().exists(),
        "a project-local install must not touch ~/.claude"
    );
    let err = stderr(&out);
    assert!(
        err.contains("does not appear to be inside a git repository"),
        "{err}"
    );
    assert!(
        err.contains(&format!("bundled with apvm {}", env!("CARGO_PKG_VERSION"))),
        "{err}"
    );
}

#[test]
fn skill_install_inside_a_git_repository_does_not_warn() {
    let sandbox = Sandbox::new();
    std::fs::create_dir(sandbox.cwd().join(".git")).unwrap();
    let out = sandbox.run(&["skill", "install"]);
    assert_exit(&out, 0);
    assert!(!stderr(&out).contains("git repository"), "{}", stderr(&out));
}

#[test]
fn skill_install_replaces_a_stale_installation() {
    // `apvm update` relies on this to refresh an outdated skill.
    let sandbox = Sandbox::new();
    let dir = sandbox.local_skill_dir();
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "stale").unwrap();
    std::fs::write(dir.join("obsolete.md"), "from an older release").unwrap();

    assert_exit(&sandbox.run(&["skill", "install"]), 0);
    assert_skill_installed(&dir);
    assert!(
        !dir.join("obsolete.md").exists(),
        "files dropped from the skill must not linger"
    );
}

#[test]
fn skill_uninstall_removes_only_the_apvm_skill_and_is_idempotent() {
    let sandbox = Sandbox::new();
    assert_exit(&sandbox.run(&["skill", "install"]), 0);
    let sibling = sandbox.cwd().join(".claude/skills/other-skill/SKILL.md");
    std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
    std::fs::write(&sibling, "someone else's skill").unwrap();

    let out = sandbox.run(&["skill", "uninstall"]);
    assert_exit(&out, 0);
    assert!(!sandbox.local_skill_dir().exists());
    assert!(sibling.exists(), "sibling skills must never be removed");

    let out = sandbox.run(&["skill", "uninstall"]);
    assert_exit(&out, 0);
    assert!(stderr(&out).contains("nothing to do"), "{}", stderr(&out));
}

#[test]
fn skill_global_install_and_uninstall_use_the_home_directory() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["skill", "install", "--global"]);
    assert_exit(&out, 0);
    assert_skill_installed(&sandbox.global_skill_dir());
    assert!(
        !sandbox.cwd().join(".claude").exists(),
        "a global install must not touch the working directory"
    );

    let out = sandbox.run(&["skill", "uninstall", "-g"]);
    assert_exit(&out, 0);
    assert!(!sandbox.global_skill_dir().exists());
    assert!(
        sandbox.home().join(".claude/skills").exists(),
        "parent directories must be kept"
    );
}

#[test]
fn skill_uninstall_refuses_a_directory_without_a_manifest() {
    // Guard against deleting a user-made directory that merely shares the
    // skill's name.
    let sandbox = Sandbox::new();
    let dir = sandbox.local_skill_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "mine").unwrap();

    let out = sandbox.run(&["skill", "uninstall"]);
    assert_exit(&out, 1);
    assert!(
        stderr(&out).contains("does not look like an installed skill (no SKILL.md)"),
        "{}",
        stderr(&out)
    );
    assert!(dir.join("notes.txt").exists());
}

// ─────────────────────────────────────────────────────────────────────────────
// uninstall (against a throwaway copy of the binary)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn uninstall_without_a_yes_answer_changes_nothing() {
    // Empty input, plain Enter, and "n" must all cancel.
    for answer in ["", "\n", "n\n", "nope\n"] {
        let sandbox = Sandbox::new();
        let binary = sandbox.installed_copy();
        assert_exit(&sandbox.run(&["skill", "install", "-g"]), 0);

        let out = sandbox.run_binary(&binary, &["uninstall"], answer);
        assert_exit(&out, 0);
        let err = stderr(&out);
        assert!(err.contains("Uninstall cancelled."), "{answer:?}: {err}");
        assert!(binary.exists(), "{answer:?} removed the binary");
        assert_skill_installed(&sandbox.global_skill_dir());
    }
}

#[test]
fn uninstall_plan_lists_the_binary_and_global_skill() {
    let sandbox = Sandbox::new();
    let binary = sandbox.installed_copy();
    assert_exit(&sandbox.run(&["skill", "install", "-g"]), 0);

    let out = sandbox.run_binary(&binary, &["uninstall"], "n\n");
    assert_exit(&out, 0);
    let err = stderr(&out);
    assert!(err.contains(&binary.display().to_string()), "{err}");
    assert!(
        err.contains(&sandbox.global_skill_dir().display().to_string()),
        "{err}"
    );
    assert!(err.contains(env!("CARGO_PKG_VERSION")), "{err}");
}

#[test]
fn uninstall_confirmed_removes_binary_bin_dir_and_global_skill_only() {
    for (args, stdin) in [
        (&["uninstall", "--yes"][..], ""),
        (&["uninstall"][..], "y\n"),
    ] {
        let sandbox = Sandbox::new();
        let binary = sandbox.installed_copy();
        assert_exit(&sandbox.run(&["skill", "install", "-g"]), 0);
        assert_exit(&sandbox.run(&["skill", "install"]), 0);
        assert_exit(&sandbox.run(&["config", "set", "cache", "false"]), 0);

        let out = sandbox.run_binary(&binary, args, stdin);
        assert_exit(&out, 0);
        assert!(
            stderr(&out).contains("has been uninstalled"),
            "{}",
            stderr(&out)
        );

        assert!(!binary.exists(), "binary must be deleted");
        assert!(
            !binary.parent().unwrap().exists(),
            "empty bin/ must be removed"
        );
        assert!(
            !sandbox.global_skill_dir().exists(),
            "global skill must be removed"
        );
        // Documented as kept: configuration and project-local skills.
        assert!(sandbox.config_file().exists(), "config must be kept");
        assert_skill_installed(&sandbox.local_skill_dir());
    }
    assert!(
        Path::new(common::APVM).exists(),
        "the build output must never be touched"
    );
}

#[test]
fn uninstall_keeps_a_non_empty_bin_directory() {
    let sandbox = Sandbox::new();
    let binary = sandbox.installed_copy();
    let neighbour = binary.with_file_name("other-tool");
    std::fs::write(&neighbour, "not ours").unwrap();

    let out = sandbox.run_binary(&binary, &["uninstall", "-y"], "");
    assert_exit(&out, 0);
    assert!(!binary.exists());
    assert!(neighbour.exists(), "unrelated files in bin/ must survive");
}

// ─────────────────────────────────────────────────────────────────────────────
// Background update check
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn update_check_never_runs_when_stderr_is_not_a_terminal() {
    // CI and pipes must stay silent and offline even without the explicit
    // opt-out: no check means no state file is ever written.
    let sandbox = Sandbox::new();
    let out = sandbox
        .command_for(Path::new(common::APVM))
        .arg("list")
        .env_remove("APVM_NO_UPDATE_CHECK")
        .output()
        .expect("run apvm");
    assert_exit(&out, 0);
    assert!(stderr(&out).is_empty(), "{}", stderr(&out));
    assert!(!sandbox.home().join(".apvm/update-check.json").exists());
}
