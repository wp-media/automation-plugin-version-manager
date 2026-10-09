//! End-to-end tests of `apvm config` through the real binary: the on-disk
//! file lifecycle, value sanitization, and token masking.
//!
//! Each test runs inside a hermetic [`common::Sandbox`] whose fake `$HOME`
//! holds the config file, so the developer's real config is never touched.
#![cfg(unix)]

mod common;

use common::{Sandbox, assert_exit, stderr, stdout};

/// A syntactically valid classic PAT used to check masking. Its last four
/// characters (`WXYZ`) are the only part that may ever be displayed.
const TOKEN: &str = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

#[test]
fn path_reports_the_file_under_home_and_whether_it_exists() {
    let sandbox = Sandbox::new();
    let file = sandbox.config_file();

    let out = sandbox.run(&["config", "path"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        format!(
            "{}\n  (not created yet — will be created on first 'config set')\n",
            file.display()
        )
    );

    assert_exit(&sandbox.run(&["config", "set", "cache", "true"]), 0);
    let out = sandbox.run(&["config", "path"]);
    assert_eq!(stdout(&out), format!("{}\n  (exists)\n", file.display()));
}

#[test]
fn show_on_a_fresh_install_lists_every_key_as_default() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["config"]);
    assert_exit(&out, 0);
    let text = stdout(&out);
    let default_cache = sandbox.home().join(".apvm").join("cache");
    for (key, value) in [
        ("token", "(not set)".to_string()),
        ("cache-dir", default_cache.display().to_string()),
        ("cache", "true".to_string()),
    ] {
        // Compared token-wise: the column padding is presentation only.
        assert!(
            text.lines().any(|l| {
                let words: Vec<&str> = l.split_whitespace().collect();
                words.first() == Some(&key) && l.contains(&format!("{value} (default)"))
            }),
            "`{key}` should show `{value} (default)`:\n{text}"
        );
    }
    assert!(text.contains(&format!("Config file: {}", sandbox.config_file().display())));
    assert!(
        !sandbox.config_file().exists(),
        "showing the config must not create the file"
    );
}

#[test]
fn set_get_round_trip_normalizes_booleans() {
    let sandbox = Sandbox::new();
    for (input, stored) in [("off", "false"), ("YES", "true"), (" 0 ", "false")] {
        let out = sandbox.run(&["config", "set", "cache", input]);
        assert_exit(&out, 0);
        assert_eq!(stdout(&out), format!("Set 'cache' = {stored}\n"));

        let out = sandbox.run(&["config", "get", "cache"]);
        assert_exit(&out, 0);
        assert_eq!(stdout(&out), format!("{stored}\n"));
    }
}

#[test]
fn token_is_masked_in_every_output_and_stored_verbatim() {
    let sandbox = Sandbox::new();
    let masked = "ghp_***...WXYZ";

    let outputs = [
        sandbox.run(&["config", "set", "token", &format!("  {TOKEN}\n")]),
        sandbox.run(&["config", "get", "token"]),
        sandbox.run(&["config"]),
    ];
    for out in &outputs {
        assert_exit(out, 0);
        let all = format!("{}{}", stdout(out), stderr(out));
        assert!(all.contains(masked), "token not masked as {masked}:\n{all}");
        // The secret part must never reach the terminal (or logs/CI output).
        assert!(!all.contains("ABCDEFGH"), "token leaked:\n{all}");
    }
    assert!(
        stderr(&outputs[0]).is_empty(),
        "a known prefix must not warn"
    );

    // Whitespace is trimmed; the file holds the real token for API use.
    let file = std::fs::read_to_string(sandbox.config_file()).unwrap();
    assert!(file.contains(&format!("\"{TOKEN}\"")), "{file}");
}

#[test]
fn token_with_unknown_prefix_warns_but_is_stored() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(&["config", "set", "token", "custom-token-1234567890"]);
    assert_exit(&out, 0);
    assert!(
        stderr(&out).starts_with("Warning: Token doesn't start with a known GitHub prefix"),
        "{}",
        stderr(&out)
    );
    assert_eq!(stdout(&out), "Set 'token' = ***...7890\n");
}

#[test]
fn invalid_input_fails_with_exit_1_and_writes_nothing() {
    let sandbox = Sandbox::new();
    for (args, message) in [
        (
            &["config", "set", "cache", "maybe"][..],
            "Error: Configuration error: Invalid boolean 'maybe'. Use 'true' or 'false'.",
        ),
        (
            &["config", "set", "token", "   "][..],
            "Error: Configuration error: Token cannot be empty.",
        ),
        (
            &["config", "set", "cache-dir", ""][..],
            "Error: Configuration error: Path cannot be empty.",
        ),
        (
            &["config", "set", "nope", "x"][..],
            "Error: Configuration error: Unknown config key 'nope'. Valid keys: token, cache-dir, cache",
        ),
        (
            &["config", "get", "nope"][..],
            "Error: Configuration error: Unknown config key 'nope'",
        ),
        (
            &["config", "unset", "nope"][..],
            "Error: Configuration error: Unknown config key 'nope'",
        ),
    ] {
        let out = sandbox.run(args);
        assert_exit(&out, 1);
        assert!(
            stderr(&out).starts_with(message),
            "`apvm {args:?}` stderr:\n{}",
            stderr(&out)
        );
    }
    assert!(
        !sandbox.config_file().exists(),
        "a rejected value must never create the config file"
    );
}

#[test]
fn cache_dir_is_stored_as_an_absolute_path() {
    let sandbox = Sandbox::new();

    // `~` expands to the (fake) home directory.
    let out = sandbox.run(&["config", "set", "cache-dir", "~/apvm-cache"]);
    assert_exit(&out, 0);
    let expected = sandbox.home().join("apvm-cache");
    assert_eq!(
        stdout(&out),
        format!("Set 'cache-dir' = {}\n", expected.display())
    );

    // A relative path resolves against the working directory.
    let out = sandbox.run(&["config", "set", "cache-dir", "rel/../cache"]);
    assert_exit(&out, 0);
    let out = sandbox.run(&["config", "get", "cache-dir"]);
    let stored = std::path::PathBuf::from(stdout(&out).trim_end());
    assert!(stored.is_absolute(), "{}", stored.display());
    assert!(
        stored.ends_with("cache"),
        "`..` not resolved: {}",
        stored.display()
    );
    assert!(!stored.to_string_lossy().contains(".."));
}

#[test]
fn unset_reverts_and_removes_the_file_with_the_last_key() {
    let sandbox = Sandbox::new();
    assert_exit(&sandbox.run(&["config", "set", "cache", "false"]), 0);
    assert_exit(&sandbox.run(&["config", "set", "token", TOKEN]), 0);

    let out = sandbox.run(&["config", "unset", "cache"]);
    assert_exit(&out, 0);
    assert_eq!(stdout(&out), "Unset 'cache' (reverted to default: true)\n");
    assert!(sandbox.config_file().exists(), "token is still set");

    let out = sandbox.run(&["config", "unset", "token"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "Unset 'token' (config file removed, using defaults)\n"
    );
    assert!(
        !sandbox.config_file().exists(),
        "an empty config must be deleted, not written as `{{}}`"
    );

    // Idempotent: unsetting again is a successful no-op.
    let out = sandbox.run(&["config", "unset", "token"]);
    assert_exit(&out, 0);
    assert_eq!(
        stdout(&out),
        "'token' is already unset (using default: (not set))\n"
    );
}

#[test]
fn corrupt_config_file_is_reported_not_overwritten() {
    let sandbox = Sandbox::new();
    let file = sandbox.config_file();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "{ not json").unwrap();

    for args in [
        &["config"][..],
        &["config", "get", "cache"][..],
        &["config", "set", "cache", "true"][..],
        &["list"][..],
    ] {
        let out = sandbox.run(args);
        assert_exit(&out, 1);
        assert!(
            stderr(&out).starts_with("Error: "),
            "`apvm {args:?}`: {}",
            stderr(&out)
        );
    }
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "{ not json",
        "a config that cannot be parsed must be left for the user to fix"
    );
}
