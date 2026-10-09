//! End-to-end tests of the `apvm` binary: argument parsing, exit codes, and
//! the offline commands (`list`, `info`, `cache`, early `build` failures).
//!
//! Each test runs the real binary inside a hermetic [`common::Sandbox`] — no
//! network, no access to the developer's `~/.apvm` or `~/.claude`.
#![cfg(unix)]

mod common;

use common::{Sandbox, assert_exit, stderr, stdout};

// ─────────────────────────────────────────────────────────────────────────────
// Global flags and parsing
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn version_flag_prints_crate_version_and_author() {
    let out = Sandbox::new().run(&["--version"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(concat!("apvm ", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(lines.next(), Some(env!("CARGO_PKG_AUTHORS")));
}

#[test]
fn version_flag_is_accepted_on_subcommands() {
    // `propagate_version` — documented as "supported on every subcommand".
    for sub in ["cache", "config", "skill", "build"] {
        let out = Sandbox::new().run(&[sub, "-V"]);
        assert_exit(&out, 0);
        assert!(
            stdout(&out).contains(env!("CARGO_PKG_VERSION")),
            "`apvm {sub} -V` did not print the version: {}",
            stdout(&out)
        );
    }
}

#[test]
fn help_lists_every_top_level_command() {
    let out = Sandbox::new().run(&["--help"]);
    assert_exit(&out, 0);
    let help = stdout(&out);
    for command in [
        "build",
        "list",
        "info",
        "cache",
        "config",
        "skill",
        "update",
        "uninstall",
    ] {
        assert!(
            help.lines()
                .any(|l| l.trim_start().starts_with(&format!("{command} "))),
            "`--help` does not list `{command}`:\n{help}"
        );
    }
}

#[test]
fn usage_errors_exit_with_code_2() {
    // clap's usage-error code; `1` is reserved for command failures, so
    // scripts can tell "wrong invocation" from "the command failed".
    for args in [
        &["no-such-command"][..],
        &[][..],
        &["build"][..],
        &["info"][..],
        &["list", "--no-such-flag"][..],
    ] {
        let out = Sandbox::new().run(args);
        assert_exit(&out, 2);
        assert!(stdout(&out).is_empty(), "usage errors must go to stderr");
    }
}

#[test]
fn verbose_flag_is_global() {
    let out = Sandbox::new().run(&["list", "--verbose"]);
    assert_exit(&out, 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// list / info
// ─────────────────────────────────────────────────────────────────────────────

/// Plugin names printed by `apvm list`, in output order.
fn listed_plugins(sandbox: &Sandbox) -> Vec<String> {
    let out = sandbox.run(&["list"]);
    assert_exit(&out, 0);
    stdout(&out)
        .lines()
        .filter(|l| l.ends_with("(public)") || l.ends_with("(private)"))
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

#[test]
fn list_prints_every_registered_plugin_sorted_with_visibility() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["list"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.starts_with("Available plugins:"), "{text}");
    // BackWPup Pro is the one private repository (needs a token).
    assert!(
        text.contains("backwpup       wp-media/backwpup-pro (private)"),
        "{text}"
    );
    assert!(text.contains("wp-media/imagify-plugin (public)"), "{text}");
    assert!(text.contains("wp-media/wp-rocket (public)"), "{text}");

    let names = listed_plugins(&sandbox);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "plugins must be listed alphabetically");
    assert_eq!(names, ["backwpup", "imagify", "wp-rocket"]);
}

#[test]
fn info_succeeds_for_every_listed_plugin() {
    let sandbox = Sandbox::new();
    for name in listed_plugins(&sandbox) {
        let out = sandbox.run(&["info", &name]);
        assert_exit(&out, 0);
        let text = stdout(&out);
        assert!(text.starts_with(&format!("Plugin: {name}\n")), "{text}");
        for section in [
            "  Repository:  https://github.com/",
            "Version:",
            "Requirement:",
        ] {
            assert!(text.contains(section), "`info {name}` lacks {section:?}");
        }
    }
}

#[test]
fn info_backwpup_shows_privacy_variants_and_defaults() {
    let out = Sandbox::new().run(&["info", "backwpup"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("Private:     yes (requires GITHUB_TOKEN)"));
    assert!(text.contains("Requirement: Required (must be provided via --ver)"));
    assert!(text.contains("Default:     9.99.99"), "{text}");
    for variant in ["free", "pro-de", "pro-en"] {
        assert!(
            text.lines()
                .any(|l| l.trim_start().starts_with(&format!("{variant} "))),
            "variant {variant} missing:\n{text}"
        );
    }
}

#[test]
fn info_single_variant_plugin_reports_no_variants() {
    let out = Sandbox::new().run(&["info", "wp-rocket"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("Private:     no"));
    assert!(text.contains("Variants:      none (single output)"));
    assert!(text.contains("Tool dependencies:"));
}

#[test]
fn info_unknown_plugin_fails_with_exit_1() {
    let out = Sandbox::new().run(&["info", "no-such-plugin"]);
    assert_exit(&out, 1);
    assert_eq!(stderr(&out), "Error: Project not found: no-such-plugin\n");
    assert!(stdout(&out).is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// build — failures that must happen before any network access
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn build_unknown_plugin_fails_before_resolving_the_reference() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["build", "no-such-plugin", "develop", "out"]);
    assert_exit(&out, 1);
    assert!(
        stderr(&out).contains("Project not found: no-such-plugin"),
        "{}",
        stderr(&out)
    );
    assert!(
        !sandbox.cwd().join("out").exists(),
        "a rejected build must not create its output directory"
    );
}

#[test]
fn build_rejects_warm_cache_combined_with_no_cache() {
    let out = Sandbox::new().run(&["build", "backwpup", "develop", "--warm-cache", "--no-cache"]);
    assert_exit(&out, 2);
    assert!(
        stderr(&out).contains("cannot be used with"),
        "{}",
        stderr(&out)
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// cache — location precedence and read-only behavior on a missing cache
// ─────────────────────────────────────────────────────────────────────────────

/// The `Location:` reported by `apvm cache info` in `sandbox`.
fn reported_cache_location(sandbox: &Sandbox) -> String {
    let out = sandbox.run(&["cache", "info"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    text.lines()
        .find_map(|l| l.trim().strip_prefix("Location: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no Location line in:\n{text}"))
}

#[test]
fn cache_info_on_a_missing_cache_reports_empty_and_creates_nothing() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["cache", "info"]);
    assert_exit(&out, 0);
    assert!(
        stdout(&out).contains("Cache is empty — nothing has been cached yet."),
        "{}",
        stdout(&out)
    );
    assert!(
        !sandbox.cache_env_dir().exists(),
        "inspecting a missing cache must not create it"
    );
}

#[test]
fn cache_dir_precedence_env_over_config_over_default() {
    // Documented order: APVM_CACHE_DIR > config `cache-dir` > ~/.apvm/cache.
    let plain = Sandbox::without_cache_env();
    let default = plain.home().join(".apvm").join("cache");
    assert_eq!(
        reported_cache_location(&plain),
        default.display().to_string()
    );

    let configured = plain.scratch().join("from-config");
    let set = plain.run(&["config", "set", "cache-dir", configured.to_str().unwrap()]);
    assert_exit(&set, 0);
    assert_eq!(
        reported_cache_location(&plain),
        configured.display().to_string()
    );

    let env_dir = plain.scratch().join("from-env");
    let out = plain
        .command_for(std::path::Path::new(common::APVM))
        .args(["cache", "info"])
        .env("APVM_CACHE_DIR", &env_dir)
        .output()
        .expect("run apvm");
    assert_exit(&out, 0);
    assert!(
        stdout(&out).contains(&format!("Location: {}", env_dir.display())),
        "APVM_CACHE_DIR must win over the config file:\n{}",
        stdout(&out)
    );
}

#[test]
fn cache_rejects_invalid_flags_even_without_a_cache() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["cache", "clean", "--older-than", "bogus"]);
    assert_exit(&out, 1);
    assert!(stderr(&out).contains("'bogus'"), "{}", stderr(&out));
    assert!(!sandbox.cache_env_dir().exists());
}
