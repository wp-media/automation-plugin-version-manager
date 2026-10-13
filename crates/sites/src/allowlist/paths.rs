//! Path-like values: remote directories, local files ssh reads, the WP-CLI
//! command and its arguments, and variant markers.

use super::{FORBIDDEN_CHARS, Rule, Word, describe, is_forbidden, is_name_char, url};
use crate::problem::Scope;

/// Longest path accepted, in bytes (Linux `PATH_MAX`).
const PATH_MAX_BYTES: usize = 4096;
/// Longest variant marker path accepted, in bytes.
const MARKER_PATH_MAX_BYTES: usize = 1024;
/// Longest variant marker needle accepted, in bytes.
const NEEDLE_MAX_BYTES: usize = 256;
/// Most tokens a site's `wp_cli` command may have: a PHP interpreter, both
/// [`PHP_FLAGS`] and the WP-CLI script.
const WP_CLI_MAX_TOKENS: usize = 2 + PHP_FLAGS.len();
/// PHP options allowed between the interpreter and the WP-CLI script. Both
/// only change how PHP starts (`-n`: no php.ini; `-q`: no CGI headers);
/// options that run code (`-r`, `-d auto_prepend_file=…`, …) are refused.
const PHP_FLAGS: [&str; 2] = ["-n", "-q"];
/// Why a path with empty, `.` or `..` components is refused.
const CANONICAL: &str = "must be canonical: no empty, '.' or '..' components and no trailing '/'";
/// Why a local file path with empty, `.` or `..` components is refused.
const CANONICAL_FILE: &str =
    "must be canonical: no empty, '.' or '..' components and no trailing separator";
/// The characters of [`SAFE`], for messages.
const SAFE_CHARS: &str = "letters, digits, '.', '_', '/', '+' and '-'";

/// `[A-Za-z0-9._/+-]` without a leading `-`: the upload-script allowlist
/// (plan §6.1), literal inside single quotes under every login shell P0
/// tested, with no `=` (an `env` assignment) and no leading `-` (an option).
const SAFE: Word = Word {
    max_len: PATH_MAX_BYTES,
    first: |c| is_safe_char(c) && c != '-',
    first_desc: "a character other than '-'",
    any: is_safe_char,
    any_desc: SAFE_CHARS,
};

/// `[A-Za-z0-9._/+-]`.
fn is_safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '+' | '-')
}

/// A remote directory (`path`, `plugins_dir`): absolute and canonical (no
/// empty, `.` or `..` component, no trailing `/`, not `/` itself), at most
/// 4096 bytes, with no backslash, control or invisible formatting
/// character, no whitespace but spaces, and no component that starts or
/// ends with a space. Canonical, unpadded paths keep one site from hiding
/// behind a second spelling that looks the same.
pub(crate) fn remote_path(value: &str) -> Rule {
    let Some(relative) = value.strip_prefix('/') else {
        return Err(if value.is_empty() {
            "must not be empty".into()
        } else {
            "must be an absolute path (start with '/')".into()
        });
    };
    let refused = |c: &char| is_forbidden(*c) || *c == '\\' || (c.is_whitespace() && *c != ' ');
    if let Some(bad) = value.chars().find(refused) {
        return Err(format!(
            "contains {}; backslashes, whitespace other than spaces, and {FORBIDDEN_CHARS}",
            describe(bad)
        ));
    }
    if value.len() > PATH_MAX_BYTES {
        return Err(format!("must be at most {PATH_MAX_BYTES} bytes"));
    }
    if relative.is_empty() {
        return Err("must not be the filesystem root '/'".into());
    }
    for part in relative.split('/') {
        if matches!(part, "" | "." | "..") {
            return Err(CANONICAL.into());
        }
        if part.starts_with(' ') || part.ends_with(' ') {
            return Err("must not have a component that starts or ends with a space".into());
        }
    }
    Ok(())
}

/// A value of an upload script (plan §6.1): `^[A-Za-z0-9._/+-]+$` without a
/// leading `-`, at most 4096 characters. Such a value is literal inside
/// single quotes under every login shell P0 tested (fish reads `\\` and
/// `\'` there as escapes), so it needs no escaping.
pub(crate) fn upload_value(value: &str) -> Rule {
    SAFE.check(value)
}

/// The remote temporary directory: a [`remote_path`] that is also an
/// [`upload_value`], so the inventory never accepts a directory an upload
/// would refuse (plan §6.1, §7).
pub(crate) fn remote_tmp_dir(value: &str) -> Rule {
    upload_value(value)?;
    remote_path(value)
}

/// A local file ssh reads, or the ssh program (`identity_file`,
/// `known_hosts_file`, `ssh_config_file`, `binary`):
/// `[A-Za-z0-9._/\:+-]`, `~` only as a leading `~/` (or `~\`), no leading
/// `-`, canonical (no empty, `.` or `..` component, so a path cannot step
/// into a directory a later rule refuses, such as `/dev`), at most 4096
/// bytes.
///
/// ssh does not take these literally: it expands `%` tokens and `${VAR}` in
/// key and known_hosts paths, splits known_hosts paths on whitespace, and,
/// for a ProxyJump, pastes the ssh program and the `-F` file unquoted into a
/// command run by the user's shell (verified: a `-F` path holding `$(…)`
/// runs it). Paths with other characters are refused, not escaped.
pub(crate) fn ssh_path(value: &str) -> Rule {
    let rest = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        .unwrap_or(value);
    if rest.is_empty() {
        return Err("must name a file".into());
    }
    let allowed = |c: char| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '\\' | ':' | '+' | '-')
    };
    if let Some(bad) = rest.chars().find(|c| !allowed(*c)) {
        return Err(format!(
            "contains {}; only letters, digits, '.', '_', '/', '\\', ':', '+' and '-' are \
             allowed, and '~' only as a leading '~/' (ssh would expand, split or run the rest)",
            describe(bad)
        ));
    }
    if value.starts_with('-') {
        return Err("must not start with '-'".into());
    }
    if value.len() > PATH_MAX_BYTES {
        return Err(format!("must be at most {PATH_MAX_BYTES} bytes"));
    }
    let body = rest.strip_prefix(['/', '\\']).unwrap_or(rest);
    if body
        .split(['/', '\\'])
        .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(CANONICAL_FILE.into());
    }
    Ok(())
}

/// A known_hosts file: an [`ssh_path`] that keeps keys. `none`, `NUL` and
/// `/dev/…` are refused: with them ssh records nothing, so every connection
/// would trust whatever key a host presents (plan §10, T6).
pub(crate) fn known_hosts_file(value: &str) -> Rule {
    ssh_path(value)?;
    if value.eq_ignore_ascii_case("none")
        || value.eq_ignore_ascii_case("nul")
        || value.starts_with("/dev/")
    {
        return Err(
            "must be a file that keeps host keys ('none', 'NUL' and '/dev/…' would let any \
             host key pass)"
                .into(),
        );
    }
    Ok(())
}

/// Rules for a site's `wp_cli`: one program, or a PHP interpreter, its
/// allowed options and an absolute WP-CLI script. Only an interpreter may
/// take arguments, so WP-CLI cannot be wrapped in another program.
///
/// # Arguments
///
/// * `argv` - The `wp_cli` tokens
/// * `scope` - Where problems go, located as `wp_cli` or `wp_cli[<i>]`
pub(crate) fn wp_cli(argv: &[String], scope: &mut Scope<'_>) {
    let Some((program, rest)) = argv.split_first() else {
        scope.problem("wp_cli", "must name the WP-CLI program");
        return;
    };
    if argv.len() > WP_CLI_MAX_TOKENS {
        let message = format!(
            "must have at most {WP_CLI_MAX_TOKENS} tokens: a PHP interpreter, its -n and -q \
             options, and the WP-CLI script"
        );
        scope.problem("wp_cli", message);
        return;
    }
    let Some((script, options)) = rest.split_last() else {
        scope.check("wp_cli[0]", wp_cli_program(program));
        return;
    };
    scope.check("wp_cli[0]", php_interpreter(program));
    for (index, option) in options.iter().enumerate() {
        scope.check(&format!("wp_cli[{}]", index + 1), php_flag(option));
    }
    let at = format!("wp_cli[{}]", options.len() + 1);
    scope.check(&at, wp_cli_script(script));
}

/// The program WP-CLI runs as (`wp_cli[0]`): a command name found in `PATH`,
/// or an absolute canonical path, of `[A-Za-z0-9._/+-]` with no leading `-`.
/// It follows `env WP_CLI_CONFIG_PATH=/dev/null` (plan §6.1), where a
/// leading `-` would be an option and a `=` an assignment.
pub(crate) fn wp_cli_program(value: &str) -> Rule {
    SAFE.check(value)?;
    if value.contains('/') {
        if !value.starts_with('/') {
            return Err("must be a command name or an absolute path".into());
        }
        return remote_path(value);
    }
    if matches!(value, "." | "..") {
        return Err("must be a command name or an absolute path".into());
    }
    Ok(())
}

/// A PHP interpreter running a WP-CLI script: a [`wp_cli_program`] named
/// `php`, optionally with a version (`php8.3`, `php83`, `php-8.3`) or
/// cPanel's `ea-` prefix (`ea-php81`). Only an interpreter may take
/// arguments, so `wp_cli` can never wrap WP-CLI in another program (`sudo`,
/// `env -i`, a shell) or turn apvm's command into arguments of another
/// WP-CLI command (`wp eval-file`).
pub(crate) fn php_interpreter(value: &str) -> Rule {
    wp_cli_program(value)?;
    let name = value.rsplit('/').next().unwrap_or(value);
    let version = name.strip_prefix("ea-").unwrap_or(name).strip_prefix("php");
    match version {
        Some(version)
            if version
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-')) =>
        {
            Ok(())
        }
        _ => Err(
            "must be a PHP interpreter (php, php8.3, php-8.3, ea-php81) when more tokens follow"
                .into(),
        ),
    }
}

/// A PHP option between the interpreter and the WP-CLI script: one of
/// [`PHP_FLAGS`].
pub(crate) fn php_flag(value: &str) -> Rule {
    if !PHP_FLAGS.contains(&value) {
        return Err(format!(
            "must be the WP-CLI script (last) or one of the PHP options {}",
            PHP_FLAGS.join(", ")
        ));
    }
    Ok(())
}

/// The WP-CLI script a PHP interpreter runs: an absolute canonical path of
/// `[A-Za-z0-9._/+-]` (WP-CLI runs from `/`, so a relative path would name
/// a file there).
pub(crate) fn wp_cli_script(value: &str) -> Rule {
    SAFE.check(value)?;
    if !value.starts_with('/') {
        return Err("must be an absolute path (WP-CLI runs from '/')".into());
    }
    remote_path(value)
}

/// WP-CLI 2.12's global parameters: the `config-spec.php` entries with a
/// command-line form. WP-CLI reads one anywhere in its arguments, after the
/// subcommand and after `--` included, by its exact (case-sensitive) name.
const WP_CLI_GLOBALS: [&str; 17] = [
    "allow-root",
    "blog",
    "color",
    "context",
    "debug",
    "exec",
    "http",
    "path",
    "prompt",
    "quiet",
    "require",
    "skip-packages",
    "skip-plugins",
    "skip-themes",
    "ssh",
    "url",
    "user",
];
/// The global parameters an argument may set as `--<name>=<value>` (plan
/// §6.12's allowed overrides).
const WP_CLI_VALUE_OVERRIDES: [&str; 5] = ["url", "user", "skip-plugins", "skip-themes", "debug"];
/// The global parameters an argument may set as a bare `--<name>`.
const WP_CLI_FLAG_OVERRIDES: [&str; 4] = ["skip-plugins", "skip-themes", "debug", "quiet"];
/// The allowed overrides, for messages.
const WP_CLI_OVERRIDES: &str =
    "--url=…, --user=…, --skip-plugins[=…], --skip-themes[=…], --debug[=…] and --quiet";

/// How WP-CLI reads an argument that starts with `--` (its
/// `Configurator::extract_assoc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WpCliOption<'a> {
    /// `--<name>`: the parameter set to true.
    Flag(&'a str),
    /// `--no-<name>`: the parameter set to false.
    Negated(&'a str),
    /// `--<name>=<value>`.
    Value(&'a str),
}

/// Read `arg` the way WP-CLI 2.12 does: `--no-<name>`, `--<name>` or
/// `--<name>=<value>`, where `<name>` is not empty and holds no `=`.
///
/// # Arguments
///
/// * `arg` - One argument
///
/// # Returns
///
/// The option form and its name, or `None` for a positional argument.
fn wp_cli_option(arg: &str) -> Option<WpCliOption<'_>> {
    let rest = arg.strip_prefix("--")?;
    if let Some((name, _)) = rest.split_once('=') {
        return (!name.is_empty()).then_some(WpCliOption::Value(name));
    }
    match rest.strip_prefix("no-") {
        Some(name) if !name.is_empty() => Some(WpCliOption::Negated(name)),
        _ => (!rest.is_empty()).then_some(WpCliOption::Flag(rest)),
    }
}

/// An argument apvm passes to WP-CLI after its own (plan §6.12): any
/// character but NUL, and no WP-CLI global parameter except the allowed
/// overrides (`--url=…`, `--user=…`, `--skip-plugins[=…]`,
/// `--skip-themes[=…]`, `--debug[=…]`, `--quiet`). WP-CLI reads globals
/// anywhere, so `--require=…` or `--exec=…` would run code, `--path`,
/// `--ssh` or `--http` would target another site, and `--no-skip-packages`
/// would undo apvm's own setting. A `--url=…` value must be a valid site
/// URL. A first argument starting with `@` (an
/// alias) is refused too.
///
/// # Arguments
///
/// * `arg` - The argument
/// * `first` - Whether it is the first argument after apvm's own
pub(crate) fn wp_cli_arg(arg: &str, first: bool) -> Rule {
    if arg.contains('\0') {
        return Err("contains U+0000 (NUL)".into());
    }
    if first && arg.starts_with('@') {
        return Err("must not be a WP-CLI alias ('@…'): apvm picks the site".into());
    }
    let allowed = match wp_cli_option(arg) {
        Some(WpCliOption::Value(name)) if WP_CLI_GLOBALS.contains(&name) => {
            WP_CLI_VALUE_OVERRIDES.contains(&name)
        }
        Some(WpCliOption::Flag(name)) if WP_CLI_GLOBALS.contains(&name) => {
            WP_CLI_FLAG_OVERRIDES.contains(&name)
        }
        Some(WpCliOption::Negated(name)) => !WP_CLI_GLOBALS.contains(&name),
        _ => true,
    };
    if !allowed {
        return Err(format!(
            "is a WP-CLI global parameter that apvm sets or refuses; only {WP_CLI_OVERRIDES} \
             are allowed"
        ));
    }
    url_override(arg)
}

/// The value of a `--url=…` override must pass the same rule as a site URL:
/// WP-CLI keeps the last `--url`, so it replaces the one apvm checked.
///
/// # Arguments
///
/// * `arg` - An argument already known to be allowed
fn url_override(arg: &str) -> Rule {
    match arg.strip_prefix("--url=") {
        Some(value) => url(value).map_err(|reason| format!("is a '--url=' whose URL {reason}")),
        None => Ok(()),
    }
}

/// A variant marker path: relative to the plugin directory, canonical, at
/// most 1024 bytes, of `[A-Za-z0-9._-]` components of at most 255 bytes.
pub(crate) fn marker_path(value: &str) -> Rule {
    const COMPONENT: Word = Word {
        max_len: 255,
        first: is_name_char,
        first_desc: "a letter, digit, '.', '_' or '-'",
        any: is_name_char,
        any_desc: "letters, digits, '.', '_' and '-'",
    };
    if value.is_empty() {
        return Err("must not be empty".into());
    }
    if value.len() > MARKER_PATH_MAX_BYTES {
        return Err(format!("must be at most {MARKER_PATH_MAX_BYTES} bytes"));
    }
    if value.starts_with('/') {
        return Err("must be relative to the plugin directory (no leading '/')".into());
    }
    for component in value.split('/') {
        if matches!(component, "" | "." | "..") {
            return Err(CANONICAL.into());
        }
        COMPONENT
            .check(component)
            .map_err(|reason| format!("has a component that {reason}"))?;
    }
    Ok(())
}

/// A variant marker needle: the fixed text `grep -F` looks for. Printable
/// ASCII only (a newline would split it into several patterns), at most
/// 256 bytes. It may hold `'` and `\`, so scripts must quote it.
pub(crate) fn needle(value: &str) -> Rule {
    if value.is_empty() {
        return Err("must not be empty".into());
    }
    if let Some(bad) = value.chars().find(|c| !(' '..='~').contains(c)) {
        return Err(format!(
            "contains {}; only printable ASCII is allowed",
            describe(bad)
        ));
    }
    if value.len() > NEEDLE_MAX_BYTES {
        return Err(format!("must be at most {NEEDLE_MAX_BYTES} bytes"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::table;
    use super::*;

    const CANON: &str = "must be canonical";

    #[test]
    fn remote_paths() {
        let longest = format!("/{}", "a".repeat(4095));
        let too_long = format!("/{}", "a".repeat(4096));
        table(
            remote_path,
            &[
                "/var/www/shop/public",
                "/home/ftpuser/public_html",
                "/srv/My Site",
                "/srv/sité",
                "/srv/a'b",
                "/srv/...",
                &longest,
            ],
            &[
                ("", "must not be empty"),
                ("relative/path", "absolute"),
                ("~/site", "absolute"),
                ("/", "filesystem root"),
                ("/var/www/", CANON),
                ("/var//www", CANON),
                ("/var/./www", CANON),
                ("/var/../etc", CANON),
                ("/..", CANON),
                ("/var/www\\x", "'\\\\'"),
                ("/var/www\nx", "U+000A"),
                ("/var/\u{202e}x", "U+202E"),
                ("/var/\u{200b}x", "U+200B"),
                ("/var/\u{0}x", "U+0000"),
                ("/var/www/shop\u{a0}", "U+00A0"),
                ("/var/www/shop ", "starts or ends with a space"),
                ("/var/ www/shop", "starts or ends with a space"),
                (&too_long, "at most 4096 bytes"),
            ],
        );
    }

    #[test]
    fn remote_tmp_dirs() {
        table(
            remote_tmp_dir,
            &["/tmp", "/var/tmp/apvm", "/tmp/a+b", "/tmp/A_b-1.2"],
            &[
                ("", "must not be empty"),
                ("tmp", "absolute"),
                ("/", "filesystem root"),
                ("/tmp/", CANON),
                ("/tmp//x", CANON),
                ("/tmp/../etc", CANON),
                ("/tmp/my dir", "U+0020"),
                ("/tmp/a'b", "'\\''"),
                ("/tmp/a\\b", "'\\\\'"),
                ("/tmp/$x", "'$'"),
                ("/tmp/é", "'é'"),
                ("/tmp/a:b", "':'"),
            ],
        );
    }

    #[test]
    fn ssh_paths() {
        let longest = "a".repeat(4096);
        let too_long = "a".repeat(4097);
        table(
            ssh_path,
            &[
                "~/.ssh/id_ed25519",
                "~\\.ssh\\id_ed25519",
                "C:\\Users\\me\\.ssh\\id",
                "C:/Windows/System32/OpenSSH/ssh.exe",
                "/usr/bin/ssh",
                "ssh",
                "keys/id+backup",
                ".ssh/id",
                &longest,
            ],
            &[
                ("", "must name a file"),
                ("~/", "must name a file"),
                ("c$(touch PWNED)", "'$'"),
                ("k`id`", "'`'"),
                ("/tmp/(id)", "'('"),
                ("cfg plain", "U+0020"),
                ("%h_key", "'%'"),
                ("/x/${GITHUB_TOKEN}", "'$'"),
                ("/tmp/a\"b", "'\"'"),
                ("/tmp/a'b", "'\\''"),
                ("~user/key", "'~'"),
                ("a~b", "'~'"),
                ("/k;id", "';'"),
                ("/k|id", "'|'"),
                ("/k*", "'*'"),
                ("/k\u{1b}", "U+001B"),
                ("-oProxyCommand", "start with '-'"),
                ("/tmp/../dev/null", "canonical"),
                ("~/../../dev/null", "canonical"),
                ("./key", "canonical"),
                ("keys//id", "canonical"),
                ("keys/", "canonical"),
                ("C:\\Users\\..\\key", "canonical"),
                ("\\\\server\\share\\key", "canonical"),
                (&too_long, "at most 4096 bytes"),
            ],
        );
    }

    #[test]
    fn known_hosts_files_must_keep_keys() {
        table(
            known_hosts_file,
            &["~/.ssh/known_hosts", "/etc/apvm/known_hosts", "nonexistent"],
            &[
                ("none", "keeps host keys"),
                ("NONE", "keeps host keys"),
                ("None", "keeps host keys"),
                ("NUL", "keeps host keys"),
                ("/dev/null", "keeps host keys"),
                ("/dev/stdout", "keeps host keys"),
                ("/tmp/../dev/null", "canonical"),
                ("/tmp/My Keys/kh", "U+0020"),
                ("~/kh_%h", "'%'"),
            ],
        );
    }

    #[test]
    fn wp_cli_programs() {
        table(
            wp_cli_program,
            &[
                "wp",
                "wp-cli.phar",
                "/usr/local/bin/wp",
                "/usr/bin/php8.3",
                "sudo",
            ],
            &[
                ("", "must not be empty"),
                ("-S", "other than '-'"),
                ("--", "other than '-'"),
                ("a b", "U+0020"),
                ("WP_CLI_CONFIG_PATH=/tmp/x", "'='"),
                ("a;b", "';'"),
                ("$(id)", "'$'"),
                ("bin/wp", "absolute path"),
                ("./wp", "absolute path"),
                ("..", "command name or an absolute path"),
                ("/usr/../bin/wp", CANON),
                ("/usr/bin/", CANON),
            ],
        );
    }

    #[test]
    fn php_interpreters() {
        table(
            php_interpreter,
            &[
                "php",
                "php8.3",
                "php83",
                "php-8.3",
                "ea-php81",
                "/usr/bin/php8.3",
                "/opt/plesk/php/8.2/bin/php",
                "/usr/local/bin/ea-php81",
            ],
            &[
                ("sudo", "PHP interpreter"),
                ("/usr/bin/env", "PHP interpreter"),
                ("sh", "PHP interpreter"),
                ("wp", "PHP interpreter"),
                ("phpdbg", "PHP interpreter"),
                ("php-cgi", "PHP interpreter"),
                ("/usr/bin/php/x", "PHP interpreter"),
                ("xphp", "PHP interpreter"),
                ("-php", "other than '-'"),
            ],
        );
    }

    #[test]
    fn php_flags_and_scripts() {
        table(
            php_flag,
            &["-n", "-q"],
            &[
                ("-r", "PHP options -n, -q"),
                ("-d", "PHP options"),
                ("-f", "PHP options"),
                ("eval-file", "PHP options"),
                ("", "PHP options"),
            ],
        );
        let longest = format!("/{}", "a".repeat(4095));
        let too_long = format!("/{}", "a".repeat(4096));
        table(
            wp_cli_script,
            &["/opt/wp-cli.phar", "/usr/local/bin/wp", &longest],
            &[
                ("wp-cli.phar", "absolute path"),
                ("-n", "other than '-'"),
                ("/opt/../wp-cli.phar", CANON),
                ("/opt/a b.phar", "U+0020"),
                (&too_long, "at most 4096 characters"),
            ],
        );
    }

    #[test]
    fn marker_paths() {
        let component = "a".repeat(255);
        let longest = format!("{}/{}", vec!["a".repeat(200); 5].join("/"), "b".repeat(19));
        assert_eq!(longest.len(), 1024);
        let too_long = longest.clone() + "c";
        table(
            marker_path,
            &[
                "inc/Pro/License/License.php",
                "License.php",
                ".version",
                &component,
                &longest,
            ],
            &[
                ("", "must not be empty"),
                ("/abs", "relative"),
                ("a//b", CANON),
                ("a/./b", CANON),
                ("../x", CANON),
                ("a/", CANON),
                (".", CANON),
                ("a b", "component that contains U+0020"),
                ("a\\b", "component that contains '\\\\'"),
                ("é.php", "component that contains 'é'"),
                (
                    &"a".repeat(256),
                    "component that must be at most 255 characters",
                ),
                (&too_long, "at most 1024 bytes"),
            ],
        );
    }

    #[test]
    fn needles() {
        let longest = "x".repeat(256);
        let too_long = "x".repeat(257);
        table(
            needle,
            &[
                "https://backwpup.de/",
                "x",
                "a 'quoted' \\ needle",
                &longest,
            ],
            &[
                ("", "must not be empty"),
                ("a\nb", "U+000A"),
                ("a\tb", "U+0009"),
                ("é", "'é'"),
                ("\u{7f}", "U+007F"),
                (&too_long, "at most 256 bytes"),
            ],
        );
    }

    #[test]
    fn upload_values() {
        // `^[A-Za-z0-9._/+-]+$` without a leading '-', against every ASCII
        // character and a few others.
        let others = ['é', '\u{a0}', '\u{2028}', '😀', '\u{ff0f}'];
        for c in (0u8..=127).map(char::from).chain(others) {
            let allowed = c.is_ascii_alphanumeric() || "._/+-".contains(c);
            let value = format!("a{c}b");
            assert_eq!(upload_value(&value).is_ok(), allowed, "{c:?}");
            // The temporary directory rule admits the same characters.
            let dir = format!("/a{c}b");
            assert_eq!(remote_tmp_dir(&dir).is_ok(), allowed, "{c:?}");
        }
        let longest = "a".repeat(4096);
        let too_long = "a".repeat(4097);
        table(
            upload_value,
            &["x-", "/tmp/apvm.Ab3/package-1.zip", "a+b", &longest],
            &[
                ("", "must not be empty"),
                ("-x", "other than '-'"),
                ("a'b", "'\\''"),
                ("a\\b", "'\\\\'"),
                ("a b", "U+0020"),
                (&too_long, "at most 4096 characters"),
            ],
        );
    }

    /// [`wp_cli_arg`] as a rule for a first argument.
    fn first_wp_cli_arg(arg: &str) -> Rule {
        wp_cli_arg(arg, true)
    }

    /// [`wp_cli_arg`] as a rule for a later argument.
    fn later_wp_cli_arg(arg: &str) -> Rule {
        wp_cli_arg(arg, false)
    }

    #[test]
    fn wp_cli_args_refuse_global_parameters_but_the_overrides() {
        const GLOBAL: &str = "WP-CLI global parameter";
        // WP-CLI 2.12 matches names exactly (no abbreviations, case
        // sensitive), reads `--` as a positional argument, and a value
        // with a trailing newline is part of the name (verified).
        let ok = [
            "plugin",
            "it's $(id) \\ \n é",
            "--format=json",
            "--skip-plugins",
            "--skip-plugins=akismet,hello",
            "--skip-themes",
            "--skip-themes=twentytwenty",
            "--debug",
            "--debug=bootstrap",
            "--quiet",
            "--url=https://shop.example.com/sub",
            "--user=admin",
            "--req=/tmp/x.php",
            "--REQUIRE=/tmp/x.php",
            "--require\n",
            "--no-foo",
            "--no-",
            "--",
            "--=x",
            "-",
            "-r",
            "@alias",
        ];
        let refused = [
            ("--require=/tmp/x.php", GLOBAL),
            ("--require", GLOBAL),
            ("--no-require", GLOBAL),
            ("--exec=system('id');", GLOBAL),
            ("--exec", GLOBAL),
            ("--path=/srv/other", GLOBAL),
            ("--path", GLOBAL),
            ("--ssh=evil.example.com", GLOBAL),
            ("--http=https://evil.example.com", GLOBAL),
            ("--blog=https://x", GLOBAL),
            ("--context=admin", GLOBAL),
            ("--color", GLOBAL),
            ("--no-color", GLOBAL),
            ("--prompt", GLOBAL),
            ("--prompt=user", GLOBAL),
            ("--allow-root", GLOBAL),
            ("--skip-packages", GLOBAL),
            ("--skip-packages=0", GLOBAL),
            ("--no-skip-packages", GLOBAL),
            ("--no-skip-plugins", GLOBAL),
            ("--no-skip-themes", GLOBAL),
            ("--no-debug", GLOBAL),
            ("--no-quiet", GLOBAL),
            ("--no-url", GLOBAL),
            ("--no-user", GLOBAL),
            ("--url", GLOBAL),
            ("--user", GLOBAL),
            ("--quiet=1", GLOBAL),
            ("a\0b", "U+0000"),
            ("--url=https://user@x.example.com", "user info"),
            ("--url=https://x.example.com/a\nb", "--url="),
            ("--url=ftp://x.example.com", "--url="),
            ("--url=", "--url="),
        ];
        table(later_wp_cli_arg, &ok, &refused);
        let mut first_ok = ok.to_vec();
        first_ok.retain(|arg| !arg.starts_with('@'));
        let mut first_refused = refused.to_vec();
        first_refused.extend([("@alias", "alias"), ("@all", "alias"), ("@", "alias")]);
        table(first_wp_cli_arg, &first_ok, &first_refused);
    }

    #[test]
    fn every_wp_cli_global_is_refused_in_every_other_form() {
        for name in WP_CLI_GLOBALS {
            // `--url=` values must also be valid URLs (`url_override`).
            let sample = if name == "url" {
                "https://x.example.com"
            } else {
                "x"
            };
            let value = format!("--{name}={sample}");
            let flag = format!("--{name}");
            let negated = format!("--no-{name}");
            let value_ok = WP_CLI_VALUE_OVERRIDES.contains(&name);
            let flag_ok = WP_CLI_FLAG_OVERRIDES.contains(&name);
            assert_eq!(later_wp_cli_arg(&value).is_ok(), value_ok, "{value}");
            assert_eq!(later_wp_cli_arg(&flag).is_ok(), flag_ok, "{flag}");
            assert!(later_wp_cli_arg(&negated).is_err(), "{negated}");
        }
        // The overrides are exactly plan §6.12's list.
        let mut overrides: Vec<&str> = WP_CLI_VALUE_OVERRIDES
            .iter()
            .chain(&WP_CLI_FLAG_OVERRIDES)
            .copied()
            .collect();
        overrides.sort_unstable();
        overrides.dedup();
        assert_eq!(
            overrides,
            [
                "debug",
                "quiet",
                "skip-plugins",
                "skip-themes",
                "url",
                "user"
            ]
        );
    }

    #[test]
    fn wp_cli_options_are_read_like_wp_cli_does() {
        let cases = [
            ("--a", Some(WpCliOption::Flag("a"))),
            ("--no-a", Some(WpCliOption::Negated("a"))),
            ("--no-a=1", Some(WpCliOption::Value("no-a"))),
            ("--a=", Some(WpCliOption::Value("a"))),
            ("--a=b=c", Some(WpCliOption::Value("a"))),
            ("--no-", Some(WpCliOption::Flag("no-"))),
            ("--", None),
            ("--=a", None),
            ("-a", None),
            ("a", None),
        ];
        for (arg, expected) in cases {
            assert_eq!(wp_cli_option(arg), expected, "{arg:?}");
        }
    }
}
