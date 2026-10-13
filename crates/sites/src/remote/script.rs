//! Framed scripts for both delivery modes, and the hermetic WP-CLI command
//! (plan §6.1).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::identity::{Identity, RootAccess, RootCommand};
use super::invalid;
use super::quote::Word;
use super::statement::{Body, Statement};
use crate::allowlist;
use crate::error::{Error, Result};
use crate::problem::Scope;

/// The first command of every script, inside its brace group: sudo and
/// coreutils then print the English messages apvm's classifiers match
/// (sudo keeps `LC_ALL`).
const PRELUDE: &str = "LC_ALL=C; export LC_ALL";
/// What follows the commands: 255 becomes 254, so a 255 always means that
/// ssh itself failed, and the script exits with the commands' status.
const EPILOGUE: &str = r#"rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc""#;
/// Environment variables removed for every WP-CLI call: each one, when set
/// for the identity, makes WP-CLI 2.12 or PHP load code, run another
/// program, or read arguments, the site or root differently (checked in
/// the 2.12.0 phar, its `bin/wp` launcher and PHP's documentation):
///
/// - `PHPRC`, `PHP_INI_SCAN_DIR`: another `php.ini` (`auto_prepend_file`);
/// - `WP_CLI_EARLY_REQUIRE`, `WP_CLI_REQUIRE`, `WP_CLI_PACKAGES_DIR`: PHP
///   files and packages to load;
/// - `WP_CLI_PHP`, `WP_CLI_PHP_ARGS`, `WP_CLI_PHP_USED`: the PHP binary
///   and its options (`bin/wp`, and WP-CLI relaunching itself);
/// - `WP_CLI_CUSTOM_SHELL`, `WP_CLI_SSH_BINARY`, `WP_CLI_SSH_PRE_CMD`:
///   programs `shell` and `--ssh` run;
/// - `WP_CONFIG_PATH`: another `wp-config.php`;
/// - `WP_CLI_RUNTIME_ALIAS`: alias definitions;
/// - `WP_CLI_STRICT_ARGS_MODE`: how global parameters are parsed;
/// - `WP_CLI_ALLOW_ROOT`: running as root without `--allow-root`;
/// - `WP_CLI_FORCE_USER_LOGIN`: how `--user` is resolved;
/// - `EDITOR`: the program `config edit`/`post edit` run through a shell;
/// - `WP_CLI_CACHE_DIR`: the download cache `core download`/`update` and
///   language packs install from.
const WP_CLI_UNSET_ENV: [&str; 18] = [
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
/// Environment variables every WP-CLI call sets: no user config
/// (`require:` files, command defaults), and `cat` as the pager `wp help`
/// always runs (through a shell, even when not on a terminal).
const WP_CLI_SET_ENV: [&str; 2] = ["WP_CLI_CONFIG_PATH=/dev/null", "PAGER=cat"];
/// What every nonce starts with.
const NONCE_PREFIX: &str = "apvm-";
/// Hexadecimal digits after [`NONCE_PREFIX`]: 128 random bits.
const NONCE_HEX_DIGITS: usize = 32;

/// The per-call random marker a script prints before its payload. Only
/// stdout after the line holding exactly the nonce is the command's
/// output; what comes before is login noise (`.bashrc`, MOTD). Its form is
/// `apvm-` and 32 lowercase hexadecimal digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Nonce(String);

impl Nonce {
    /// A fresh random nonce, from the standard library's randomly keyed
    /// hasher (no extra dependency): unpredictable to the server, and
    /// distinct for every call.
    pub fn random() -> Self {
        Self(format!(
            "{NONCE_PREFIX}{:016x}{:016x}",
            random_u64(),
            random_u64()
        ))
    }

    /// Check and wrap a nonce, such as one a test fixes.
    ///
    /// # Arguments
    ///
    /// * `text` - `apvm-` and 32 lowercase hexadecimal digits
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when `text` has another form.
    pub fn parse(text: &str) -> Result<Self> {
        let valid = text.strip_prefix(NONCE_PREFIX).is_some_and(|hex| {
            hex.len() == NONCE_HEX_DIGITS
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if !valid {
            return Err(invalid(
                "nonce",
                "nonce",
                "must be 'apvm-' followed by 32 lowercase hexadecimal digits",
            ));
        }
        Ok(Self(text.to_string()))
    }

    /// The nonce text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Nonce {
    /// The nonce text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 64 random bits: a fresh [`RandomState`] (keyed from the operating
/// system's random source) hashing a process-wide counter, the clock and
/// the process id, so two calls never repeat.
fn random_u64() -> u64 {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(CALLS.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// A file mode as `stat %a` prints it: permission bits plus setuid, setgid
/// and sticky, at most `0o7777`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileMode(u32);

impl FileMode {
    /// The largest mode: every permission and special bit.
    pub const MAX: u32 = 0o7777;

    /// Check and wrap mode bits.
    ///
    /// # Arguments
    ///
    /// * `bits` - The mode, such as `0o2775`
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] above [`FileMode::MAX`] (a file type, not a
    /// mode).
    pub fn new(bits: u32) -> Result<Self> {
        if bits > Self::MAX {
            return Err(invalid("file mode", "mode", "must be at most 0o7777"));
        }
        Ok(Self(bits))
    }

    /// Read a mode as `stat -c %a` prints it: 1 to 4 octal digits.
    ///
    /// # Arguments
    ///
    /// * `text` - The digits, such as `2775` or `755`
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] for anything else (signs, spaces, other
    /// digits, more than 4 digits).
    pub fn from_octal(text: &str) -> Result<Self> {
        let digits =
            (1..=4).contains(&text.len()) && text.bytes().all(|b| (b'0'..=b'7').contains(&b));
        match u32::from_str_radix(text, 8) {
            Ok(bits) if digits => Self::new(bits),
            _ => Err(invalid("file mode", "mode", "must be 1 to 4 octal digits")),
        }
    }

    /// The mode bits.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Only the group bits (`mode & 0o070`): what WP-CLI's umask is relaxed
    /// by, so new files get the group access the directory grants.
    pub fn group_bits(self) -> u32 {
        self.0 & 0o070
    }
}

/// A hermetic WP-CLI command (plan §6.1 item 5):
///
/// ```text
/// env -u PHPRC … -u WP_CONFIG_PATH WP_CLI_CONFIG_PATH=/dev/null PAGER=cat <wp_cli…>
///     --skip-packages --exec='umask(umask() & ~0<g>);' [--allow-root] --path=<path>
///     [--url=<url>] <args…>
/// ```
///
/// - `WP_CLI_CONFIG_PATH=/dev/null` keeps the identity's
///   `~/.wp-cli/config.yml` (`require:` files, command defaults) out, and
///   `--skip-packages` its installed packages: P0 showed both run code on
///   every call. `env -u` removes the variables that would load code or
///   change WP-CLI's behavior another way (`WP_CLI_REQUIRE`, `PHPRC`, …;
///   GNU, busybox and BSD `env` all take `-u`), and `PAGER=cat` keeps
///   `wp help` from running the identity's pager.
/// - `--exec` relaxes, inside WP-CLI only, the umask's group bits that the
///   plugins directory grants (`<g>` = its mode `& 070`), so WordPress
///   creates files with the right group access and keeps setgid; the
///   identity's "other" bits stay as they are.
/// - `--path` is always explicit (and the script runs from `/`), so no
///   `wp-cli.yml` picks the site.
///
/// Every value is quoted, so a path may hold `'`, `$`, `*` or spaces, and
/// arguments any character but NUL. Arguments cannot set WP-CLI global
/// parameters other than plan §6.12's overrides ([`WpCli::arg`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WpCli {
    /// The site's `wp_cli` tokens (checked).
    program: Vec<String>,
    /// The plugins directory's mode (for `--exec`).
    mode: FileMode,
    /// The WordPress root (checked).
    path: String,
    /// The site URL (checked), if any.
    url: Option<String>,
    /// Whether `--allow-root` is passed.
    allow_root: bool,
    /// WP-CLI's command and arguments (no NUL).
    args: Vec<String>,
}

/// What [`WpCli`] errors call the command.
const WP_SUBJECT: &str = "WP-CLI command";

impl WpCli {
    /// A WP-CLI command for one site, without a subcommand yet.
    ///
    /// # Arguments
    ///
    /// * `wp_cli` - The site's `wp_cli` tokens: one program, or a PHP
    ///   interpreter, `-n`/`-q` and an absolute WP-CLI script (plan §7)
    /// * `plugins_dir_mode` - The mode of the site's plugins directory
    /// * `path` - The WordPress root: absolute and canonical (plan §7)
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] listing every token or value that breaks its
    /// rule (located as `wp_cli[1]`, `path`).
    pub fn new(wp_cli: &[String], plugins_dir_mode: FileMode, path: &str) -> Result<Self> {
        let mut problems = Vec::new();
        let mut scope = Scope::root(&mut problems);
        allowlist::wp_cli(wp_cli, &mut scope);
        scope.check("path", allowlist::remote_path(path));
        if !problems.is_empty() {
            return Err(Error::InvalidInput {
                subject: WP_SUBJECT.to_string(),
                problems,
            });
        }
        Ok(Self {
            program: wp_cli.to_vec(),
            mode: plugins_dir_mode,
            path: path.to_string(),
            url: None,
            allow_root: false,
            args: Vec::new(),
        })
    }

    /// Pass `--url=<url>` (the site's URL, or a subsite's).
    ///
    /// # Arguments
    ///
    /// * `url` - An `http(s)` URL (plan §7)
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when `url` breaks the rule.
    #[must_use = "the builder returns the changed command"]
    pub fn url(mut self, url: &str) -> Result<Self> {
        allowlist::url(url).map_err(|reason| invalid(WP_SUBJECT, "url", reason))?;
        self.url = Some(url.to_string());
        Ok(self)
    }

    /// Pass `--allow-root`: only for a site whose identity is root, which
    /// the site must allow (`allow_root`, plan §6.2).
    #[must_use = "the builder returns the changed command"]
    pub fn allow_root(mut self) -> Self {
        self.allow_root = true;
        self
    }

    /// Append one argument (a subcommand word, an option, a value). Any
    /// character but NUL arrives verbatim.
    ///
    /// WP-CLI reads its global parameters anywhere in its arguments (after
    /// the subcommand and after `--` too), so an argument that sets one is
    /// refused, except plan §6.12's overrides: `--url=…`, `--user=…`,
    /// `--skip-plugins[=…]`, `--skip-themes[=…]`, `--debug[=…]` and
    /// `--quiet`. That refuses `--require=…` and `--exec=…` (they run
    /// PHP), `--path`, `--ssh`, `--http` and `--blog` (another site),
    /// `--allow-root`, `--skip-packages`, `--context`, `--prompt` and
    /// `--color`, each also as a bare flag and as `--no-…`. A first
    /// argument starting with `@` (an alias) is refused too. A later
    /// `--url=…` replaces [`WpCli::url`]'s (WP-CLI keeps the last).
    ///
    /// # Arguments
    ///
    /// * `arg` - The argument
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] (located as `args[<i>]`) when `arg` holds
    /// NUL, sets a refused global parameter, or is a leading alias.
    #[must_use = "the builder returns the changed command"]
    pub fn arg(mut self, arg: &str) -> Result<Self> {
        let first = self.args.is_empty();
        if let Err(reason) = allowlist::wp_cli_arg(arg, first) {
            let location = format!("args[{}]", self.args.len());
            return Err(invalid(WP_SUBJECT, &location, reason));
        }
        self.args.push(arg.to_string());
        Ok(self)
    }

    /// Append several arguments ([`WpCli::arg`] each).
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments, in order
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] at the first argument [`WpCli::arg`]
    /// refuses.
    #[must_use = "the builder returns the changed command"]
    pub fn args<I, S>(self, args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        args.into_iter()
            .try_fold(self, |wp, arg| wp.arg(arg.as_ref()))
    }

    /// The command as a statement run as `identity`: `env` and its
    /// arguments.
    ///
    /// # Arguments
    ///
    /// * `identity` - Who runs it
    pub(crate) fn statement(&self, identity: &Identity) -> Statement {
        Statement::as_identity(identity, "env", self.env_args())
    }

    /// `env`'s arguments: the variables it removes and sets, then the
    /// WP-CLI command line.
    fn env_args(&self) -> Vec<Word> {
        let unset = WP_CLI_UNSET_ENV.map(|name| [Word::lit("-u"), Word::lit(name)]);
        let mut words: Vec<Word> = unset.into_iter().flatten().collect();
        words.extend(WP_CLI_SET_ENV.map(Word::lit));
        words.extend(self.program.iter().map(|token| Word::checked(token)));
        words.push(Word::lit("--skip-packages"));
        words.push(Word::option("--exec=", &umask_exec(self.mode)));
        if self.allow_root {
            words.push(Word::lit("--allow-root"));
        }
        words.push(Word::option("--path=", &self.path));
        if let Some(url) = &self.url {
            words.push(Word::option("--url=", url));
        }
        words.extend(self.args.iter().map(|arg| Word::checked(arg)));
        words
    }
}

/// The PHP code `--exec` runs: clear from the umask the group bits `mode`
/// grants. The digits are formatted from a number, never taken from text.
///
/// # Arguments
///
/// * `mode` - The plugins directory's mode
fn umask_exec(mode: FileMode) -> String {
    format!("umask(umask() & ~0{:03o});", mode.group_bits())
}

/// The nonce line: `printf '%s\n' '<nonce>'`. Its only backslash is in
/// `\n`, which no shell reads as an escape inside single quotes.
///
/// # Arguments
///
/// * `nonce` - The call's nonce
fn nonce_line(nonce: &Nonce) -> String {
    format!(r"printf '%s\n' {}", Word::checked(nonce.as_str()).as_str())
}

/// How [`frame`] lays a script out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One part per line, for `sh -s`.
    Lines,
    /// One line, parts separated by `; `, for `sh -c` through any login
    /// shell (csh and tcsh refuse newlines inside quotes).
    OneLine,
}

/// Frame `commands` (plan §6.1 item 2): the only place that writes the
/// brace group, the prelude, the nonce line and the epilogue.
///
/// ```text
/// { LC_ALL=C; export LC_ALL
/// printf '%s\n' '<nonce>'
/// cd / && <commands>; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"
/// }
/// ```
///
/// The brace group makes a shell parse the whole script before it runs
/// any of it: a script cut short at its end (a dropped connection) is a
/// syntax error that runs nothing, instead of a partial last command. (A
/// login shell whose startup files eat the *start* of stdin is not
/// covered: the remaining lines may still parse; the missing nonce line
/// then fails the call.)
///
/// # Arguments
///
/// * `nonce` - The call's nonce
/// * `commands` - The commands' shell text, redirections included
/// * `layout` - Lines for `sh -s`, one line for `sh -c`
fn frame(nonce: &Nonce, commands: &str, layout: Layout) -> String {
    let (separator, end) = match layout {
        Layout::Lines => ("\n", "\n"),
        Layout::OneLine => ("; ", ""),
    };
    let nonce = nonce_line(nonce);
    format!(
        "{{ {PRELUDE}{separator}{nonce}{separator}cd / && {commands}; {EPILOGUE}{separator}}}{end}"
    )
}

/// A script delivered on stdin to `sh -s` ([`Script::REMOTE_COMMAND`]):
/// the login shell parses only `sh -s`, and POSIX `sh` takes the quoted
/// values literally, whatever they hold (plan §6.1 item 3).
///
/// ```text
/// { LC_ALL=C; export LC_ALL
/// printf '%s\n' '<nonce>'
/// cd / && <prefix> <argv…> </dev/null; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"
/// }
/// ```
///
/// Nothing runs unless the whole script arrived (the brace group). The
/// command's stdin is `/dev/null`, so it can never read the rest of the
/// script (bash reads a piped script unbuffered). Built only from typed
/// commands: [`Script::wp`] and [`Script::root`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// The call's nonce.
    nonce: Nonce,
    /// The script's text.
    text: String,
}

impl Script {
    /// The remote command that reads a script from stdin.
    pub const REMOTE_COMMAND: &'static str = "sh -s";

    /// Run a WP-CLI command as `identity`.
    ///
    /// # Arguments
    ///
    /// * `nonce` - The call's nonce (usually [`Nonce::random`])
    /// * `identity` - Who runs it
    /// * `command` - The hermetic WP-CLI command
    pub fn wp(nonce: Nonce, identity: &Identity, command: &WpCli) -> Self {
        Self::framed(nonce, &Body::new(command.statement(identity), []))
    }

    /// Run a root command, through sudo or as the root login.
    ///
    /// # Arguments
    ///
    /// * `nonce` - The call's nonce (usually [`Nonce::random`])
    /// * `access` - How root is reached
    /// * `command` - The root command
    pub fn root(nonce: Nonce, access: RootAccess, command: &RootCommand) -> Self {
        Self::framed(nonce, &Body::new(command.statement(access), []))
    }

    /// Frame a body of one or more statements: the constructor every
    /// typed script of this crate goes through.
    ///
    /// # Arguments
    ///
    /// * `nonce` - The call's nonce
    /// * `body` - The statements
    pub(crate) fn framed(nonce: Nonce, body: &Body) -> Self {
        let text = frame(&nonce, &body.render(), Layout::Lines);
        Self { nonce, text }
    }

    /// The call's nonce.
    pub fn nonce(&self) -> &Nonce {
        &self.nonce
    }

    /// The script, as written to the remote `sh -s`'s stdin.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// What [`UploadScript`] errors call the script.
const UPLOAD_SUBJECT: &str = "upload script";

/// A script for upload mode: stdin carries a file, so the script is the
/// remote command itself, `sh -c '<script>'` ([`UploadScript::remote_command`]),
/// and the login shell parses it (plan §6.1 item 3).
///
/// To survive sh, bash, dash, zsh, fish and tcsh login shells (P0):
///
/// - every value matches `^[A-Za-z0-9._/+-]+$`, so it needs no escaping
///   (fish reads `\\` and `\'` inside single quotes as escapes, and a `'`
///   would need one);
/// - the template is one line (csh and tcsh refuse newlines inside quotes)
///   with neither `\\` nor `\'`.
///
/// Remote paths with other characters (`path` and `plugins_dir` may hold
/// `'`, `$`, `*` or spaces) never go here; they travel in a [`Script`].
/// Identity names always fit: [`AccountName`](super::AccountName)'s
/// characters are a subset.
///
/// ```text
/// { LC_ALL=C; export LC_ALL; printf '%s\n' '<nonce>'; cd / && <prefix> tee -- '<destination>' >/dev/null; rc=$?; …; }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadScript {
    /// The call's nonce.
    nonce: Nonce,
    /// The script, before it is quoted into the remote command.
    script: String,
}

impl UploadScript {
    /// Stream stdin into `destination` as `identity`: `tee -- <destination>`
    /// (the staging upload, plan §6.6).
    ///
    /// # Arguments
    ///
    /// * `nonce` - The call's nonce (usually [`Nonce::random`])
    /// * `identity` - Who writes the file
    /// * `destination` - An absolute, canonical path of
    ///   `[A-Za-z0-9._/+-]` only
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when `destination` is not an absolute,
    /// canonical path of upload values only (the temporary directory rule).
    pub fn tee(nonce: Nonce, identity: &Identity, destination: &str) -> Result<Self> {
        allowlist::remote_tmp_dir(destination)
            .map_err(|reason| invalid(UPLOAD_SUBJECT, "destination", reason))?;
        let args = vec![Word::lit("--"), Word::checked(destination)];
        let tee = Statement::as_identity(identity, "tee", args);
        // stdin is the upload; only tee's own stdout is discarded.
        let commands = format!("{} >/dev/null", tee.render());
        let script = frame(&nonce, &commands, Layout::OneLine);
        Ok(Self { nonce, script })
    }

    /// The call's nonce.
    pub fn nonce(&self) -> &Nonce {
        &self.nonce
    }

    /// The script, as `sh -c` receives it.
    pub fn script(&self) -> &str {
        &self.script
    }

    /// The remote command for ssh: `sh -c '<script>'`.
    pub fn remote_command(&self) -> String {
        format!("sh -c {}", Word::checked(&self.script).as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;
    use crate::remote::{AccountName, OwnerIds, PluginEntry};

    /// A fixed nonce for goldens.
    const NONCE: &str = "apvm-0123456789abcdef0123456789abcdef";
    /// The plan's framing tail.
    const TAIL: &str = r#"</dev/null; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc""#;
    /// The hermetic `env` words every WP-CLI call starts with.
    const ENV: &str = "env -u EDITOR -u PHPRC -u PHP_INI_SCAN_DIR -u WP_CLI_ALLOW_ROOT \
                       -u WP_CLI_CACHE_DIR -u WP_CLI_CUSTOM_SHELL -u WP_CLI_EARLY_REQUIRE -u WP_CLI_FORCE_USER_LOGIN \
                       -u WP_CLI_PACKAGES_DIR -u WP_CLI_PHP -u WP_CLI_PHP_ARGS -u WP_CLI_PHP_USED \
                       -u WP_CLI_REQUIRE -u WP_CLI_RUNTIME_ALIAS -u WP_CLI_SSH_BINARY \
                       -u WP_CLI_SSH_PRE_CMD -u WP_CLI_STRICT_ARGS_MODE -u WP_CONFIG_PATH \
                       WP_CLI_CONFIG_PATH=/dev/null PAGER=cat";

    /// The fixed nonce.
    fn nonce() -> Nonce {
        Nonce::parse(NONCE).unwrap()
    }

    /// `tokens` as owned strings.
    ///
    /// # Arguments
    ///
    /// * `tokens` - The `wp_cli` tokens
    fn wp_cli(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| token.to_string()).collect()
    }

    /// `wp` for `/var/www/shop`, plugins dir mode `mode`.
    ///
    /// # Arguments
    ///
    /// * `mode` - The plugins directory's mode bits
    fn shop(mode: u32) -> WpCli {
        WpCli::new(
            &wp_cli(&["wp"]),
            FileMode::new(mode).unwrap(),
            "/var/www/shop",
        )
        .unwrap()
    }

    /// The shell text of `wp` run as the login user.
    ///
    /// # Arguments
    ///
    /// * `wp` - The command
    fn render(wp: &WpCli) -> String {
        wp.statement(&Identity::Login).render()
    }

    /// One identity of each kind.
    fn identities() -> Vec<Identity> {
        vec![
            Identity::Login,
            Identity::sudo("www-data", Some("www-data")).unwrap(),
            Identity::sudo("ftpuser", None).unwrap(),
            Identity::runuser("ftpuser", "www-data").unwrap(),
        ]
    }

    /// Every script these tests build: every identity and root access.
    fn every_script() -> Vec<Script> {
        let wp = shop(0o2775).args(["plugin", "list"]).unwrap();
        let entry = PluginEntry::new("/srv/p", "akismet").unwrap();
        let roots = [
            RootCommand::stat_ownership("/srv/p").unwrap(),
            RootCommand::stat_file_type(entry.clone()),
            RootCommand::chown(
                entry,
                OwnerIds {
                    uid: 1001,
                    gid: 1001,
                },
                OwnerIds { uid: 33, gid: 33 },
                1000,
            )
            .unwrap(),
        ];
        let mut scripts: Vec<Script> = identities()
            .iter()
            .map(|identity| Script::wp(nonce(), identity, &wp))
            .collect();
        for access in [RootAccess::Sudo, RootAccess::RootLogin] {
            scripts.extend(roots.iter().map(|root| Script::root(nonce(), access, root)));
        }
        scripts
    }

    #[test]
    fn random_nonces_are_well_formed_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let nonce = Nonce::random();
            assert_eq!(Nonce::parse(nonce.as_str()).unwrap(), nonce);
            assert_eq!(nonce.to_string(), nonce.as_str());
            assert!(seen.insert(nonce), "a nonce repeated");
        }
    }

    #[test]
    fn random_nonces_vary_in_both_halves_and_every_digit() {
        const SAMPLES: usize = 2_000;
        let hex: Vec<String> = (0..SAMPLES)
            .map(|_| Nonce::random().as_str()[NONCE_PREFIX.len()..].to_string())
            .collect();
        // Each 64-bit half is distinct in every sample: neither is fixed
        // or derived from the other.
        for half in [0..16, 16..32] {
            let distinct: std::collections::HashSet<&str> =
                hex.iter().map(|digits| &digits[half.clone()]).collect();
            assert_eq!(distinct.len(), SAMPLES, "half {half:?} repeats");
        }
        // Every position takes all 16 digits (missing one by chance in
        // 2000 uniform samples has a probability below 1e-50).
        for position in 0..NONCE_HEX_DIGITS {
            let digits: std::collections::HashSet<u8> = hex
                .iter()
                .map(|digits| digits.as_bytes()[position])
                .collect();
            assert_eq!(digits.len(), 16, "position {position}: {digits:?}");
        }
    }

    #[test]
    fn nonces_have_one_form() {
        assert_eq!(nonce().as_str(), NONCE);
        let bad = [
            "",
            "apvm-",
            "apvm-0123456789abcdef0123456789abcde",
            "apvm-0123456789abcdef0123456789abcdef0",
            "apvm-0123456789ABCDEF0123456789abcdef",
            "apvm-0123456789abcdef0123456789abcdeg",
            "APVM-0123456789abcdef0123456789abcdef",
            "xapvm-0123456789abcdef0123456789abcde",
            "apvm-0123456789abcdef0123456789abcd'f",
        ];
        for text in bad {
            let error = Nonce::parse(text).unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput, "{text:?}");
        }
    }

    #[test]
    fn file_modes_and_their_group_bits() {
        for (text, bits, group) in [
            ("2775", 0o2775, 0o070),
            ("755", 0o755, 0o050),
            ("750", 0o750, 0o050),
            ("700", 0o700, 0),
            ("0", 0, 0),
            ("7777", 0o7777, 0o070),
            ("0730", 0o730, 0o030),
        ] {
            let mode = FileMode::from_octal(text).unwrap();
            assert_eq!((mode.bits(), mode.group_bits()), (bits, group), "{text}");
            assert_eq!(FileMode::new(bits).unwrap(), mode);
        }
        assert!(FileMode::new(FileMode::MAX).is_ok());
        assert!(FileMode::new(0o10000).is_err());
        assert!(FileMode::new(0o40755).is_err());
        for text in [
            "", "8", "75a", "+755", "-755", " 755", "755\n", "02775", "0o755", "１",
        ] {
            assert!(FileMode::from_octal(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn hermetic_wp_cli_golden_per_plugins_dir_mode() {
        let cases = [
            (0o2775, "070"),
            (0o0775, "070"),
            (0o0770, "070"),
            (0o2750, "050"),
            (0o0755, "050"),
            (0o0750, "050"),
            (0o0740, "040"),
            (0o0730, "030"),
            (0o0710, "010"),
            (0o0700, "000"),
            (0o0707, "000"),
            (0o7777, "070"),
        ];
        for (mode, g) in cases {
            assert_eq!(
                render(&shop(mode)),
                format!(
                    "{ENV} 'wp' --skip-packages --exec='umask(umask() & ~0{g});' \
                     --path='/var/www/shop'"
                ),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn wp_cli_options_render_in_a_fixed_order() {
        let tokens = wp_cli(&["/usr/bin/php8.3", "-n", "-q", "/opt/wp-cli.phar"]);
        let path = "/srv/it's $HOME/*/my site";
        let wp = WpCli::new(&tokens, FileMode::new(0o755).unwrap(), path)
            .unwrap()
            .arg("option")
            .unwrap()
            .url("https://shop.example.com/sub")
            .unwrap()
            .allow_root()
            .args(["get", "it's \\ $(id)\n", "--url=https://shop.example.com/b"])
            .unwrap();
        assert_eq!(
            render(&wp),
            format!(
                "{ENV} '/usr/bin/php8.3' '-n' '-q' '/opt/wp-cli.phar' \
                 --skip-packages --exec='umask(umask() & ~0050);' --allow-root \
                 --path='/srv/it'\\''s $HOME/*/my site' --url='https://shop.example.com/sub' \
                 'option' 'get' 'it'\\''s \\ $(id)\n' '--url=https://shop.example.com/b'"
            )
        );
    }

    #[test]
    fn wp_cli_refuses_bad_tokens_paths_urls_and_nul() {
        let mode = FileMode::new(0o755).unwrap();
        let error = WpCli::new(&wp_cli(&["sudo", "eval-file"]), mode, "rel").unwrap_err();
        assert_eq!(error.code(), Code::InvalidInput);
        let locations: Vec<&str> = error
            .problems()
            .iter()
            .map(|p| p.location.as_str())
            .collect();
        assert_eq!(locations, ["wp_cli[0]", "wp_cli[1]", "path"]);
        assert!(
            error.to_string().starts_with("invalid WP-CLI command: "),
            "{error}"
        );
        for tokens in [
            vec![],
            vec!["-wp"],
            vec!["w p"],
            vec!["wp'"],
            vec!["A=b"],
            vec!["php", "-r", "/x.php"],
            vec!["php", "x.php"],
            vec!["php", "-n", "-q", "-n", "/x.php"],
        ] {
            assert!(
                WpCli::new(&wp_cli(&tokens), mode, "/srv").is_err(),
                "{tokens:?}"
            );
        }
        for path in ["", "/", "/srv/", "/srv/../etc", "/srv\\x", "/a\nb", "/a\0b"] {
            assert!(
                WpCli::new(&wp_cli(&["wp"]), mode, path).is_err(),
                "{path:?}"
            );
        }
        for url in [
            "",
            "ftp://x",
            "https://user@x",
            "https://x/?q",
            "https://x/a b",
        ] {
            let error = shop(0o755).url(url).unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with("invalid WP-CLI command: url: "),
                "{error}"
            );
        }
        let error = shop(0o755).args(["ok", "bad\0"]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid WP-CLI command: args[1]: contains U+0000 (NUL)"
        );
    }

    #[test]
    fn wp_cli_args_cannot_set_refused_global_parameters() {
        let refused = [
            (vec!["plugin", "list", "--require=/tmp/x.php"], 2),
            (vec!["cli", "version", "--", "--exec=system('id');"], 3),
            (vec!["plugin", "list", "--no-skip-packages"], 2),
            (vec!["option", "get", "x", "--path=/srv/other"], 3),
            (vec!["--allow-root", "cli", "info"], 0),
            (vec!["@prod", "plugin", "list"], 0),
        ];
        for (args, at) in refused {
            let error = shop(0o755).args(&args).unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput);
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("invalid WP-CLI command: args[{at}]: ")),
                "{args:?}: {error}"
            );
        }
        let overrides = [
            "--url=https://shop.example.com/b",
            "--user=admin",
            "--skip-plugins",
            "--skip-plugins=akismet",
            "--skip-themes",
            "--debug",
            "--quiet",
            "--format=json",
            "@prod",
        ];
        let wp = shop(0o755).arg("plugin").unwrap().args(overrides).unwrap();
        assert!(render(&wp).ends_with("'--quiet' '--format=json' '@prod'"));
    }

    #[test]
    fn stdin_script_goldens_per_identity() {
        let wp = shop(0o2775).args(["plugin", "list"]).unwrap();
        let command = format!(
            "{ENV} 'wp' --skip-packages --exec='umask(umask() & ~0070);' \
             --path='/var/www/shop' 'plugin' 'list'"
        );
        let prefixes = [
            "",
            "sudo -n -u 'www-data' -g 'www-data' -- ",
            "sudo -n -u 'ftpuser' -- ",
            "runuser -u 'ftpuser' -g 'www-data' -- ",
        ];
        for (identity, prefix) in identities().iter().zip(prefixes) {
            let script = Script::wp(nonce(), identity, &wp);
            assert_eq!(
                script.text(),
                format!(
                    "{{ LC_ALL=C; export LC_ALL\nprintf '%s\\n' '{NONCE}'\n\
                     cd / && {prefix}{command} {TAIL}\n}}\n"
                )
            );
            assert_eq!(script.nonce().as_str(), NONCE);
        }
    }

    #[test]
    fn stdin_script_goldens_per_root_command() {
        let entry = PluginEntry::new("/srv/a b/plugins", "backwpup").unwrap();
        let ids = |uid, gid| OwnerIds { uid, gid };
        let cases = [
            (
                RootCommand::stat_ownership("/srv/a b/plugins").unwrap(),
                "stat -c '%u\n%g\n%a\n%F\n%U\n%G' -- '/srv/a b/plugins'",
            ),
            (
                RootCommand::stat_file_type(entry.clone()),
                "stat -c %F -- '/srv/a b/plugins/backwpup'",
            ),
            (
                RootCommand::chown(entry, ids(1001, 1001), ids(1002, 33), 1000).unwrap(),
                "chown -R -P -h --from=+1001:+1001 -- +1002:+33 '/srv/a b/plugins/backwpup'",
            ),
        ];
        for (command, argv) in cases {
            for (access, prefix) in [
                (RootAccess::Sudo, "sudo -n -u root -- "),
                (RootAccess::RootLogin, ""),
            ] {
                assert_eq!(
                    Script::root(nonce(), access, &command).text(),
                    format!(
                        "{{ LC_ALL=C; export LC_ALL\nprintf '%s\\n' '{NONCE}'\n\
                         cd / && {prefix}{argv} {TAIL}\n}}\n"
                    )
                );
            }
        }
    }

    #[test]
    fn bodies_of_several_statements_share_one_frame() {
        let first = Statement::as_identity(&Identity::Login, "id", vec![Word::lit("-u")]);
        let entry = PluginEntry::new("/srv/p", "akismet").unwrap();
        let second = RootCommand::stat_file_type(entry).statement(RootAccess::Sudo);
        let script = Script::framed(nonce(), &Body::new(first, [second]));
        assert_eq!(
            script.text(),
            format!(
                "{{ LC_ALL=C; export LC_ALL\nprintf '%s\\n' '{NONCE}'\n\
                 cd / && id -u </dev/null && sudo -n -u root -- stat -c %F -- \
                 '/srv/p/akismet' {TAIL}\n}}\n"
            )
        );
    }

    #[test]
    fn every_script_is_one_brace_group_that_starts_with_lc_all() {
        for script in every_script() {
            let text = script.text();
            assert!(text.starts_with("{ LC_ALL=C; export LC_ALL\n"), "{text}");
            assert_eq!(
                text.lines().nth(1),
                Some(format!("printf '%s\\n' '{NONCE}'").as_str())
            );
            assert!(text.ends_with(&format!("{TAIL}\n}}\n")), "{text}");
            // The stat ownership format holds five literal newlines.
            let format_lines = if text.contains("%U") { 5 } else { 0 };
            assert_eq!(text.lines().count(), 4 + format_lines, "{text}");
        }
        assert_eq!(Script::REMOTE_COMMAND, "sh -s");
    }

    /// The program that follows the identity or root prefix in `command`
    /// (after `cd / && `): the first word after `--` when a prefix is
    /// there, else the first word.
    ///
    /// # Arguments
    ///
    /// * `command` - A framed script's command part
    fn program_after_prefix(command: &str) -> &str {
        let command = command.strip_prefix("cd / && ").unwrap_or(command);
        let words: Vec<&str> = command.split(' ').collect();
        let start = match words.first() {
            Some(&"sudo") | Some(&"runuser") => words
                .iter()
                .position(|word| *word == "--")
                .map_or(0, |at| at + 1),
            _ => 0,
        };
        words.get(start).copied().unwrap_or_default()
    }

    #[test]
    fn no_shell_ever_runs_under_sudo_or_runuser() {
        let mut programs = Vec::new();
        for script in every_script() {
            let command = script.text().split('\n').nth(2).unwrap_or_default();
            programs.push(program_after_prefix(command).to_string());
            assert!(!script.text().contains("sh -c"), "{}", script.text());
        }
        for identity in identities() {
            let upload =
                UploadScript::tee(nonce(), &identity, "/tmp/apvm.a/package-1.zip").unwrap();
            let command = upload.script().split("; ").nth(3).unwrap_or_default();
            programs.push(program_after_prefix(command).to_string());
            assert!(!upload.script().contains("sh -c"), "{}", upload.script());
        }
        programs.sort();
        programs.dedup();
        assert_eq!(programs, ["chown", "env", "stat", "tee"]);
    }

    #[test]
    fn upload_script_golden() {
        let identity = Identity::sudo("ftpuser", Some("www-data")).unwrap();
        let upload =
            UploadScript::tee(nonce(), &identity, "/tmp/apvm.Ab3dE6gH9k/package-1.zip").unwrap();
        let script = format!(
            "{{ LC_ALL=C; export LC_ALL; printf '%s\\n' '{NONCE}'; cd / && sudo -n -u 'ftpuser' \
             -g 'www-data' -- tee -- '/tmp/apvm.Ab3dE6gH9k/package-1.zip' >/dev/null; \
             rc=$?; [ \"$rc\" -eq 255 ] && rc=254; exit \"$rc\"; }}"
        );
        assert_eq!(upload.script(), script);
        assert_eq!(upload.nonce().as_str(), NONCE);
        assert_eq!(
            upload.remote_command(),
            format!("sh -c '{}'", script.replace('\'', "'\\''"))
        );
        let login = UploadScript::tee(nonce(), &Identity::Login, "/var/tmp/x+y/p.zip").unwrap();
        assert!(
            login
                .script()
                .contains("; cd / && tee -- '/var/tmp/x+y/p.zip' >/dev/null; "),
            "{}",
            login.script()
        );
    }

    #[test]
    fn upload_templates_are_one_line_without_fish_escapes() {
        for identity in identities() {
            let upload =
                UploadScript::tee(nonce(), &identity, "/tmp/apvm.x/package-9.zip").unwrap();
            let script = upload.script();
            assert!(
                script.starts_with("{ LC_ALL=C; export LC_ALL; "),
                "{script}"
            );
            assert!(script.ends_with(r#"exit "$rc"; }"#), "{script}");
            assert!(!script.contains(['\n', '\r']), "{script}");
            assert!(
                !script.contains("\\\\") && !script.contains("\\'"),
                "{script}"
            );
            assert!(!script.ends_with('\\'), "{script}");
            // Quoted as one word, every backslash in the remote command sits
            // either in `\n` (inside quotes) or in `'\''` (outside them).
            let command = upload.remote_command();
            let backslashes = command.matches('\\').count();
            let escaped_quotes = command.matches("'\\''").count();
            assert_eq!(backslashes, escaped_quotes + 1, "{command}");
        }
    }

    #[test]
    fn upload_destinations_outside_the_allowlist_are_refused() {
        let bad = [
            "",
            "tmp/x.zip",
            "/",
            "/tmp/",
            "/tmp//x",
            "/tmp/./x",
            "/tmp/../x",
            "/tmp/it's",
            "/tmp/a\\b",
            "/tmp/a b",
            "/tmp/$HOME",
            "/tmp/`id`",
            "/tmp/*",
            "/tmp/a;b",
            "/tmp/a\nb",
            "/tmp/~x",
            "/tmp/a=b",
            "/tmp/a:b",
            "/tmp/a@b",
            "/tmp/a%b",
            "/tmp/é",
            "/tmp/a\0b",
            "/tmp/a\"b",
            "/tmp/a|b",
            "/tmp/a&b",
            "/tmp/(x)",
            "/tmp/{a,b}",
            "/tmp/a!b",
            "/tmp/a#b",
        ];
        for destination in bad {
            let error = UploadScript::tee(nonce(), &Identity::Login, destination).unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput, "{destination:?}");
            assert!(
                error
                    .to_string()
                    .starts_with("invalid upload script: destination: "),
                "{destination:?}: {error}"
            );
        }
        let longest = format!("/{}", "a".repeat(4095));
        assert!(UploadScript::tee(nonce(), &Identity::Login, &longest).is_ok());
        let too_long = format!("/{}", "a".repeat(4096));
        assert!(UploadScript::tee(nonce(), &Identity::Login, &too_long).is_err());
    }

    #[test]
    fn account_names_are_always_upload_values() {
        // Character by character: every character a name may hold is an
        // upload character, so `UploadScript` needs no check of its own.
        let others = ['é', '\u{a0}', '\u{2028}', '😀', '\u{ff0f}'];
        for c in (0u8..=127).map(char::from).chain(others) {
            for name in [format!("a{c}"), format!("{c}a")] {
                if AccountName::new(&name).is_ok() {
                    assert!(allowlist::upload_value(&name).is_ok(), "{name:?}");
                }
            }
        }
        // And on generated names, through a real upload script.
        let mut state: u64 = 0x5EED_CAFE;
        let alphabet: Vec<char> = (0u8..=127).map(char::from).chain(others).collect();
        let mut accepted = 0;
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let len = (state >> 60) as usize % 6 + 1;
            let name: String = (0..len)
                .map(|i| alphabet[((state >> (8 * i)) as usize) % alphabet.len()])
                .collect();
            let Ok(identity) = Identity::sudo(&name, Some(&name)) else {
                continue;
            };
            accepted += 1;
            assert!(allowlist::upload_value(&name).is_ok(), "{name:?}");
            let upload = UploadScript::tee(nonce(), &identity, "/tmp/x.zip").unwrap();
            assert!(
                upload.script().contains(&format!("-u '{name}'")),
                "{name:?}"
            );
        }
        assert!(accepted > 100, "too few valid names generated: {accepted}");
    }
}
