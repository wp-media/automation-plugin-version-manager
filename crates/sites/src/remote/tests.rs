//! Remote scripts through real local shells (Unix only).
//!
//! - **Inner layer:** what the remote `sh` parses. Each script runs through
//!   every POSIX shell found here (`sh`, `bash`, `dash`, `zsh`, `ksh`, …),
//!   as `-s` (stdin) and as `-c` (argument), since a server's `/bin/sh` may
//!   be any of them.
//! - **Outer layer:** what sshd hands the login shell: `<login> -c 'sh -s'`
//!   or `<login> -c "sh -c '<script>'"`, through every login shell found
//!   (fish, tcsh and csh too, where installed).
//!
//! Shells that are not installed are skipped; each test prints the ones it
//! ran (`cargo test -- --nocapture`) and fails if none ran. Commands only
//! exec installed shells, never a file the tests write, so Linux's "Text
//! file busy" race cannot occur. Every child is killed after
//! [`CHILD_TIMEOUT`], failing its test instead of hanging it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::allowlist;

/// How long one child process may run before it is killed.
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// A POSIX shell a server's `/bin/sh` may be.
struct PosixShell {
    /// A label for messages.
    label: &'static str,
    /// The argv that starts it.
    argv: &'static [&'static str],
    /// Whether it reads non-ASCII scripts under the C locale. yash 2.52
    /// does not ("cannot read input: Invalid or incomplete multibyte or
    /// wide character"), so it gets ASCII values only.
    unicode: bool,
}

/// POSIX shells a server's `/bin/sh` may be.
const POSIX_SHELLS: [PosixShell; 8] = [
    shell("sh", &["sh"]),
    shell("bash", &["bash"]),
    shell("dash", &["dash"]),
    shell("zsh", &["zsh"]),
    shell("ksh", &["ksh"]),
    shell("mksh", &["mksh"]),
    PosixShell {
        label: "yash",
        argv: &["yash"],
        unicode: false,
    },
    shell("busybox ash", &["busybox", "ash"]),
];

/// A [`PosixShell`] that reads any UTF-8 script.
///
/// # Arguments
///
/// * `label` - A label for messages
/// * `argv` - The argv that starts it
const fn shell(label: &'static str, argv: &'static [&'static str]) -> PosixShell {
    PosixShell {
        label,
        argv,
        unicode: true,
    }
}

/// An installed [`PosixShell`].
struct Installed {
    /// A label for messages.
    label: &'static str,
    /// The argv that starts it, its program resolved in `PATH`.
    argv: Vec<String>,
    /// Whether it reads non-ASCII scripts (see [`PosixShell::unicode`]).
    unicode: bool,
}

impl Installed {
    /// The argv followed by `extra`.
    ///
    /// # Arguments
    ///
    /// * `extra` - Arguments to append
    fn with(&self, extra: &[&str]) -> Vec<String> {
        let extra = extra.iter().map(|word| word.to_string());
        self.argv.iter().cloned().chain(extra).collect()
    }

    /// `values`, or only the ASCII ones for a shell that reads no other.
    ///
    /// # Arguments
    ///
    /// * `values` - Values for a script
    fn readable(&self, values: &[String]) -> Vec<String> {
        let readable = |value: &&String| self.unicode || value.is_ascii();
        values.iter().filter(readable).cloned().collect()
    }
}

/// Login shells sshd may run a remote command through (`<shell> -c`).
const LOGIN_SHELLS: [&str; 10] = [
    "sh", "bash", "dash", "zsh", "ksh", "mksh", "yash", "fish", "tcsh", "csh",
];

/// `name` in `PATH`, if installed.
///
/// # Arguments
///
/// * `name` - A program name
fn find(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The installed [`POSIX_SHELLS`].
fn posix_shells() -> Vec<Installed> {
    let shells: Vec<_> = POSIX_SHELLS
        .iter()
        .filter_map(|shell| {
            let program = find(shell.argv[0])?;
            let mut argv: Vec<String> = shell.argv.iter().map(|word| word.to_string()).collect();
            argv[0] = program.to_string_lossy().into_owned();
            Some(Installed {
                label: shell.label,
                argv,
                unicode: shell.unicode,
            })
        })
        .collect();
    let labels: Vec<_> = shells.iter().map(|shell| shell.label).collect();
    eprintln!("POSIX shells: {labels:?}");
    assert!(!shells.is_empty(), "no POSIX shell found");
    shells
}

/// The installed [`LOGIN_SHELLS`]: label and path.
fn login_shells() -> Vec<(&'static str, PathBuf)> {
    let shells: Vec<_> = LOGIN_SHELLS
        .iter()
        .filter_map(|name| Some((*name, find(name)?)))
        .collect();
    let labels: Vec<_> = shells.iter().map(|(label, _)| *label).collect();
    eprintln!("login shells: {labels:?}");
    assert!(!shells.is_empty(), "no login shell found");
    shells
}

/// Run `argv` with `stdin` fed from a thread (so large inputs cannot
/// deadlock against the output pipes) and `env` added. A child still
/// running after [`CHILD_TIMEOUT`] is killed and the test fails.
///
/// # Arguments
///
/// * `argv` - Program and arguments
/// * `stdin` - Bytes for its stdin
/// * `env` - Extra environment variables
fn run<S: AsRef<std::ffi::OsStr> + std::fmt::Debug>(
    argv: &[S],
    stdin: &[u8],
    env: &[(&str, &str)],
) -> Output {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let bytes = stdin.to_vec();
    // A shell that exits early closes the pipe: that is the test's to judge.
    std::thread::spawn(move || input.write_all(&bytes).ok());
    let pid = child.id();
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || done.send(child.wait_with_output()).ok());
    match finished.recv_timeout(CHILD_TIMEOUT) {
        Ok(output) => output.unwrap(),
        Err(_) => {
            // Through `sh`'s builtin: slim images ship no `kill` binary.
            let kill = format!("kill -9 {pid}");
            Command::new("sh").args(["-c", &kill]).status().ok();
            panic!("{argv:?} still ran after {CHILD_TIMEOUT:?}: killed");
        }
    }
}

/// A small deterministic generator (a 64-bit LCG), so failures replay.
struct Lcg(u64);

impl Lcg {
    /// The next pseudo-random number.
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap()
    }

    /// A string of up to 24 characters, heavy on shell syntax, control
    /// characters and Unicode (never NUL).
    fn string(&mut self) -> String {
        const SPECIAL: &str = "'\"\\$`*?[]{}()~!#&|;<>^%=,:@+-./ \t\n\r";
        const UNICODE: [char; 9] = [
            'é', 'ß', '中', '😀', '\u{2028}', '\u{85}', '\u{feff}', '\u{202e}', '\u{301}',
        ];
        let len = self.next() % 25;
        (0..len)
            .map(|_| match self.next() % 4 {
                0 | 1 => SPECIAL.chars().nth(self.next() % SPECIAL.len()).unwrap(),
                2 => char::from_u32(u32::try_from(self.next() % 127 + 1).unwrap()).unwrap(),
                _ => UNICODE[self.next() % UNICODE.len()],
            })
            .collect()
    }
}

/// Hand-picked hostile values, then `generated` random ones.
///
/// # Arguments
///
/// * `generated` - How many random values to add
fn hostile_values(generated: usize) -> Vec<String> {
    let fixed = [
        "",
        " ",
        "'",
        "''",
        "'\\''",
        "\\",
        "\\\\",
        "\\'",
        "a'b",
        "it's",
        "\"",
        "$",
        "$HOME",
        "${HOME}",
        "$(touch /tmp/apvm-pwned)",
        "`id`",
        "*",
        "?",
        "[a]",
        "~",
        "~root",
        "!",
        "!!",
        "#",
        "&",
        "|",
        ";",
        "<",
        ">",
        "(",
        ")",
        "{a,b}",
        "-",
        "--",
        "-n",
        "-e",
        "--exec=x",
        "a\nb",
        "\n",
        "\r\n",
        "\t",
        "a b  c",
        "%s",
        "%",
        "\\n",
        "\\0",
        "\u{1}\u{7f}\u{1b}[2J",
        "é😀中",
        "\u{2028}\u{feff}\u{202e}",
        "https://backwpup.de/",
        "C:\\x\\y'z",
    ];
    let mut values: Vec<String> = fixed.iter().map(|value| value.to_string()).collect();
    let mut rng = Lcg(0x00A9_5EED);
    values.extend((0..generated).map(|_| rng.string()));
    values
}

/// A temporary directory whose path passes the upload allowlist (and so
/// every path rule), for files the scripts name.
fn safe_tempdir() -> tempfile::TempDir {
    let system = std::env::temp_dir();
    let base = match system.to_str() {
        Some(text) if allowlist::remote_tmp_dir(text.trim_end_matches('/')).is_ok() => system,
        _ => PathBuf::from("/tmp"),
    };
    let dir = tempfile::Builder::new()
        .prefix("apvm-remote-")
        .tempdir_in(base)
        .unwrap();
    assert!(allowlist::remote_tmp_dir(text(dir.path())).is_ok());
    dir
}

/// The text of `path` (which must be valid UTF-8).
fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// The bytes after the first line that is exactly `nonce`.
///
/// # Arguments
///
/// * `stdout` - A script's whole stdout
/// * `nonce` - The script's nonce
fn after_nonce<'a>(stdout: &'a [u8], nonce: &Nonce) -> &'a [u8] {
    let line = format!("{nonce}\n");
    let at = stdout
        .windows(line.len())
        .enumerate()
        .find(|(at, window)| *window == line.as_bytes() && (*at == 0 || stdout[at - 1] == b'\n'))
        .map(|(at, _)| at + line.len());
    match at {
        Some(at) => &stdout[at..],
        None => panic!("no nonce line in {:?}", String::from_utf8_lossy(stdout)),
    }
}

/// NUL-separated fields, as `printf '%s\0'` writes them.
///
/// # Arguments
///
/// * `bytes` - The output
fn fields(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let mut fields: Vec<String> = text.split('\0').map(str::to_string).collect();
    assert_eq!(
        fields.pop().as_deref(),
        Some(""),
        "output must end with NUL"
    );
    fields
}

#[test]
fn quoted_values_reach_commands_byte_exact_through_sh_s_and_sh_c() {
    let all = hostile_values(3000);
    for shell in posix_shells() {
        let values = shell.readable(&all);
        let quoted: Vec<String> = values.iter().map(|value| quote(value).unwrap()).collect();
        let script = format!("printf '%s\\0' {}\n", quoted.join(" "));
        let label = shell.label;
        let stdin = run(&shell.with(&["-s"]), script.as_bytes(), &[]);
        assert!(stdin.status.success(), "{label} -s: {stdin:?}");
        assert_eq!(fields(&stdin.stdout), values, "{label} -s");
        let arg = run(&shell.with(&["-c", &script]), b"", &[]);
        assert!(arg.status.success(), "{label} -c: {arg:?}");
        assert_eq!(fields(&arg.stdout), values, "{label} -c");
    }
}

/// The variables every WP-CLI call removes (the renderer's own list is
/// checked by the goldens in `script.rs`).
const UNSET_ENV: [&str; 18] = [
    "EDITOR",
    "PHPRC",
    "PHP_INI_SCAN_DIR",
    "WP_CLI_ALLOW_ROOT",
    "WP_CLI_CACHE_DIR",
    "WP_CLI_CUSTOM_SHELL",
    "WP_CLI_EARLY_REQUIRE",
    "WP_CLI_FORCE_USER_LOGIN",
    "WP_CLI_PACKAGES_DIR",
    "WP_CLI_PHP",
    "WP_CLI_PHP_ARGS",
    "WP_CLI_PHP_USED",
    "WP_CLI_REQUIRE",
    "WP_CLI_RUNTIME_ALIAS",
    "WP_CLI_SSH_BINARY",
    "WP_CLI_SSH_PRE_CMD",
    "WP_CLI_STRICT_ARGS_MODE",
    "WP_CONFIG_PATH",
];

/// The environment a WP-CLI script runs in: every [`UNSET_ENV`] variable
/// set (the script must remove them), an inherited `LC_ALL` and `PAGER`
/// (the script must replace them), and the stand-in's exit code.
///
/// # Arguments
///
/// * `exit` - What the stand-in exits with
fn hostile_env(exit: &str) -> Vec<(&'static str, String)> {
    let mut env: Vec<(&str, String)> = UNSET_ENV
        .iter()
        .map(|name| (*name, "/tmp/apvm-evil".to_string()))
        .collect();
    env.extend([
        ("LC_ALL", "POSIX".to_string()),
        ("PAGER", "evil-pager".to_string()),
        ("APVM_TEST_EXIT", exit.to_string()),
    ]);
    env
}

/// `env` as the `(&str, &str)` pairs [`run`] takes.
///
/// # Arguments
///
/// * `env` - Owned pairs
fn pairs<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    env.iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect()
}

/// A WP-CLI stand-in: `php` (a symbolic link to an installed shell, named
/// so the `wp_cli` rule accepts it as an interpreter) running `dump.sh`,
/// which prints its working directory, `LC_ALL`, `WP_CLI_CONFIG_PATH`,
/// `PAGER`, every [`UNSET_ENV`] variable (or `unset`) and its arguments,
/// NUL-separated, then whatever it reads on stdin, and exits with
/// `$APVM_TEST_EXIT`. Nothing written here is executed directly.
struct FakeWp {
    /// Holds `php` and `dump.sh`.
    dir: tempfile::TempDir,
}

impl FakeWp {
    /// Set up the stand-in, or `None` (with a message) when only busybox
    /// provides a shell: busybox picks its applet by the name it is run
    /// as, so a link named `php` cannot start it.
    fn new() -> Option<Self> {
        let Some(shell) = Self::shell() else {
            eprintln!("skipped: no POSIX shell other than busybox for the WP-CLI stand-in");
            return None;
        };
        let dir = safe_tempdir();
        std::os::unix::fs::symlink(shell, dir.path().join("php")).unwrap();
        let unset: Vec<String> = UNSET_ENV
            .iter()
            .map(|name| format!("\"${{{name}-unset}}\""))
            .collect();
        let dump = format!(
            "printf '%s\\0' \"$(pwd -P)\" \"${{LC_ALL-unset}}\" \"${{WP_CLI_CONFIG_PATH-unset}}\" \
             \"${{PAGER-unset}}\" {} \"$@\"\ncat\nexit \"${{APVM_TEST_EXIT:-0}}\"\n",
            unset.join(" ")
        );
        std::fs::write(dir.path().join("dump.sh"), dump).unwrap();
        Some(Self { dir })
    }

    /// The first installed shell that is not busybox.
    fn shell() -> Option<PathBuf> {
        ["bash", "dash", "zsh", "ksh", "mksh", "sh"]
            .iter()
            .filter_map(|name| std::fs::canonicalize(find(name)?).ok())
            .find(|path| path.file_name().is_some_and(|name| name != "busybox"))
    }

    /// The site's `wp_cli` tokens for the stand-in.
    fn tokens(&self) -> Vec<String> {
        ["php", "dump.sh"]
            .iter()
            .map(|name| text(&self.dir.path().join(name)).to_string())
            .collect()
    }
}

/// The hostile site path of the rev 8 audit: `'`, `$`, `*` and spaces.
const HOSTILE_PATH: &str = "/srv/it's $HOME/*/my site";

/// Hostile WP-CLI arguments: [`hostile_values`] without the ones
/// [`WpCli::arg`] refuses (WP-CLI global parameters such as `--exec=x`).
///
/// # Arguments
///
/// * `generated` - How many random values to add
fn hostile_args(generated: usize) -> Vec<String> {
    let args: Vec<String> = hostile_values(generated)
        .into_iter()
        .filter(|value| allowlist::wp_cli_arg(value, false).is_ok())
        .collect();
    assert!(allowlist::wp_cli_arg(&args[0], true).is_ok());
    args
}

/// A WP-CLI command for the stand-in, with hostile arguments.
///
/// # Arguments
///
/// * `fake` - The stand-in
/// * `args` - Arguments for WP-CLI
fn hostile_wp(fake: &FakeWp, args: &[String]) -> WpCli {
    WpCli::new(&fake.tokens(), FileMode::new(0o2775).unwrap(), HOSTILE_PATH)
        .unwrap()
        .url("https://shop.example.com/sub")
        .unwrap()
        .args(args)
        .unwrap()
}

/// What the stand-in must print for `args`.
///
/// # Arguments
///
/// * `args` - Arguments for WP-CLI
fn expected_dump(args: &[String]) -> Vec<String> {
    let env = ["/", "C", "/dev/null", "cat"];
    let unset = [UNSET_ENV.map(|_| "unset")].concat();
    let options = [
        "--skip-packages",
        "--exec=umask(umask() & ~0070);",
        "--path=/srv/it's $HOME/*/my site",
        "--url=https://shop.example.com/sub",
    ];
    env.iter()
        .chain(&unset)
        .chain(&options)
        .map(|field| field.to_string())
        .chain(args.iter().cloned())
        .collect()
}

/// Check one run of a framed WP-CLI script: payload after the nonce, no
/// leaked script text, the mapped exit code.
///
/// # Arguments
///
/// * `label` - Which shells ran it, for messages
/// * `output` - The run's output
/// * `script` - The script
/// * `expected` - The stand-in's expected fields
/// * `exit` - The expected exit code
fn check_framed(label: &str, output: &Output, script: &Script, expected: &[String], exit: i32) {
    assert_eq!(output.status.code(), Some(exit), "{label}: {output:?}");
    let payload = after_nonce(&output.stdout, script.nonce());
    assert_eq!(fields(payload), expected, "{label}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("LEAKED"),
        "{label}"
    );
}

#[test]
fn framed_wp_scripts_deliver_argv_env_and_cwd_verbatim() {
    let Some(fake) = FakeWp::new() else { return };
    let all = hostile_args(500);
    for shell in posix_shells() {
        let args = shell.readable(&all);
        let script = Script::wp(Nonce::random(), &Identity::Login, &hostile_wp(&fake, &args));
        // Text after the script must never run: `exit` ends it, and the
        // command's stdin is /dev/null, so it cannot read it either.
        let stdin = format!("{}echo LEAKED\n", script.text());
        let expected = expected_dump(&args);
        for (code, mapped) in [(0, 0), (1, 1), (3, 3), (254, 254), (255, 254)] {
            let env = hostile_env(&code.to_string());
            let output = run(&shell.with(&["-s"]), stdin.as_bytes(), &pairs(&env));
            let label = format!("{} -s, exit {code}", shell.label);
            check_framed(&label, &output, &script, &expected, mapped);
        }
    }
}

#[test]
fn stdin_scripts_pass_through_every_login_shell() {
    let Some(fake) = FakeWp::new() else { return };
    let args = hostile_args(200);
    let script = Script::wp(Nonce::random(), &Identity::Login, &hostile_wp(&fake, &args));
    let stdin = format!("{}echo LEAKED\n", script.text());
    let expected = expected_dump(&args);
    let env = hostile_env("255");
    for (label, login) in login_shells() {
        let argv = [text(&login), "-c", Script::REMOTE_COMMAND];
        let output = run(&argv, stdin.as_bytes(), &pairs(&env));
        let label = format!("{label} -c 'sh -s'");
        check_framed(&label, &output, &script, &expected, 254);
    }
}

/// Threads per shell in [`a_stdin_script_cut_short_runs_nothing`]: each
/// cut is its own child process, so they run side by side.
const CUT_THREADS: usize = 4;

/// Feed the proper prefixes of `script` whose length is `lane` modulo
/// [`CUT_THREADS`] to `shell -s`, and check that none prints anything.
///
/// # Arguments
///
/// * `shell` - The shell
/// * `script` - The whole script
/// * `lane` - Which prefixes this call checks
fn check_cuts(shell: &Installed, script: &str, lane: usize) {
    let bytes = script.as_bytes();
    let argv = shell.with(&["-s"]);
    // Without its final newline the brace group is already complete.
    for cut in (lane..bytes.len() - 1).step_by(CUT_THREADS) {
        let output = run(&argv, &bytes[..cut], &[]);
        // Only the start of the output: a cut after `export` would list
        // the whole environment.
        let start = &output.stdout[..output.stdout.len().min(80)];
        assert!(
            output.stdout.is_empty(),
            "{} ran a script cut at byte {cut} of {}: {:?}…",
            shell.label,
            bytes.len(),
            String::from_utf8_lossy(start)
        );
    }
}

#[test]
fn a_stdin_script_cut_short_runs_nothing() {
    let Some(fake) = FakeWp::new() else { return };
    let args: Vec<String> = ["plugin", "list", "it's $(id)"]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let script = Script::wp(Nonce::random(), &Identity::Login, &hostile_wp(&fake, &args));
    let expected = expected_dump(&args);
    let shells = posix_shells();
    let text = script.text();
    std::thread::scope(|scope| {
        for shell in &shells {
            for lane in 0..CUT_THREADS {
                scope.spawn(move || check_cuts(shell, text, lane));
            }
        }
    });
    // The whole script runs, with or without its final newline.
    let bytes = script.text().as_bytes();
    for shell in &shells {
        for end in [bytes.len() - 1, bytes.len()] {
            let output = run(&shell.with(&["-s"]), &bytes[..end], &[]);
            let label = format!("{} -s, {end} bytes", shell.label);
            check_framed(&label, &output, &script, &expected, 0);
        }
    }
}

/// 1 MiB of every byte value, in a scrambled order.
fn upload_bytes() -> Vec<u8> {
    let mut rng = Lcg(0xB17E5);
    (0..1 << 20).map(|_| (rng.next() & 0xFF) as u8).collect()
}

/// Check one upload run: only the nonce on stdout, exit 0, the file equal
/// to what was sent.
///
/// # Arguments
///
/// * `label` - Which shells ran it, for messages
/// * `output` - The run's output
/// * `upload` - The script
/// * `file` - The destination
/// * `bytes` - What was sent
fn check_upload(label: &str, output: &Output, upload: &UploadScript, file: &Path, bytes: &[u8]) {
    assert_eq!(output.status.code(), Some(0), "{label}: {output:?}");
    let nonce_line = format!("{}\n", upload.nonce());
    assert_eq!(output.stdout, nonce_line.as_bytes(), "{label}");
    assert!(
        std::fs::read(file).unwrap() == bytes,
        "{label}: the file differs"
    );
    std::fs::remove_file(file).unwrap();
}

#[test]
fn upload_scripts_stream_stdin_byte_exact_under_every_login_shell() {
    let dir = safe_tempdir();
    let file = dir.path().join("package-1.zip");
    let upload = UploadScript::tee(Nonce::random(), &Identity::Login, text(&file)).unwrap();
    let bytes = upload_bytes();
    for shell in posix_shells() {
        let output = run(&shell.with(&["-c", upload.script()]), &bytes, &[]);
        check_upload(
            &format!("{} -c", shell.label),
            &output,
            &upload,
            &file,
            &bytes,
        );
    }
    let command = upload.remote_command();
    for (label, login) in login_shells() {
        let output = run(&[text(&login), "-c", &command], &bytes, &[]);
        let label = format!("{label} -c \"sh -c '…'\"");
        check_upload(&label, &output, &upload, &file, &bytes);
    }
}

#[test]
fn a_failed_upload_reports_the_tee_status_after_the_nonce() {
    let dir = safe_tempdir();
    let missing = dir.path().join("missing").join("package-1.zip");
    let upload = UploadScript::tee(Nonce::random(), &Identity::Login, text(&missing)).unwrap();
    let output = run(&["sh", "-c", &upload.remote_command()], b"data", &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, format!("{}\n", upload.nonce()).as_bytes());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("tee"), "{stderr}");
    assert!(!missing.exists());
}

/// Root commands on Linux, run without a prefix (GNU or busybox `stat`,
/// GNU `chown`; macOS ships BSD tools). The chown matches no file, so the
/// tests need no root.
#[cfg(target_os = "linux")]
mod gnu {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    /// Run a root command's script with `sh -s`: exit code and payload.
    ///
    /// # Arguments
    ///
    /// * `command` - The root command
    fn run_root(command: &RootCommand) -> (Option<i32>, String) {
        let script = Script::root(Nonce::random(), RootAccess::RootLogin, command);
        let output = run(&["sh", "-s"], script.text().as_bytes(), &[]);
        let payload = after_nonce(&output.stdout, script.nonce());
        let payload = String::from_utf8_lossy(payload).into_owned();
        (output.status.code(), payload)
    }

    #[test]
    fn root_stat_prints_six_lines_with_the_names_last() {
        let base = safe_tempdir();
        let dir = base.path().join("it's $HOME * x");
        std::fs::create_dir(&dir).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        let (code, out) = run_root(&RootCommand::stat_ownership(text(&dir)).unwrap());
        assert_eq!(code, Some(0), "{out}");
        let lines: Vec<&str> = out.strip_suffix('\n').unwrap_or(&out).split('\n').collect();
        assert_eq!(lines.len(), 6, "{out:?}");
        assert_eq!(lines[0], meta.uid().to_string());
        assert_eq!(lines[1], meta.gid().to_string());
        assert_eq!(lines[2], format!("{:o}", meta.mode() & 0o7777));
        assert_eq!(
            FileMode::from_octal(lines[2]).unwrap().bits(),
            meta.mode() & 0o7777
        );
        assert_eq!(lines[3], "directory");
        assert!(!lines[4].is_empty() && !lines[5].is_empty(), "{out:?}");
    }

    #[test]
    fn root_stat_describes_a_link_instead_of_following_it() {
        let base = safe_tempdir();
        std::fs::create_dir(base.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", base.path().join("linked")).unwrap();
        let linked = base.path().join("linked");
        let (code, out) = run_root(&RootCommand::stat_ownership(text(&linked)).unwrap());
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(out.split('\n').nth(3), Some("symbolic link"), "{out:?}");
        for (name, kind) in [("real", "directory\n"), ("linked", "symbolic link\n")] {
            let entry = PluginEntry::new(text(base.path()), name).unwrap();
            let (code, out) = run_root(&RootCommand::stat_file_type(entry));
            assert_eq!((code, out.as_str()), (Some(0), kind));
        }
    }

    /// Whether `chown` is GNU's: busybox has no `--from`, so the root
    /// repair fails closed there (and is not tested).
    fn gnu_chown() -> bool {
        let output = run(&["chown", "--version"], b"", &[]);
        String::from_utf8_lossy(&output.stdout).contains("GNU coreutils")
    }

    #[test]
    fn root_chown_argv_is_accepted_by_gnu_chown() {
        if !gnu_chown() {
            eprintln!("skipped: chown is not GNU coreutils");
            return;
        }
        let base = safe_tempdir();
        let tree = base.path().join("plugin");
        std::fs::create_dir(&tree).unwrap();
        let meta = std::fs::metadata(&tree).unwrap();
        let me = OwnerIds {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        // A foreign owner no file has: chown changes nothing and succeeds,
        // so `--from=+F:+FG` and `+U:+G` parse as ids.
        let foreign = OwnerIds {
            uid: 4_000_001,
            gid: 4_000_001,
        };
        let entry = PluginEntry::new(text(base.path()), "plugin").unwrap();
        let command = RootCommand::chown(entry, foreign, me, me.uid).unwrap();
        let (code, out) = run_root(&command);
        assert_eq!((code, out.as_str()), (Some(0), ""));
        assert_eq!(std::fs::metadata(&tree).unwrap().uid(), me.uid);
    }
}
