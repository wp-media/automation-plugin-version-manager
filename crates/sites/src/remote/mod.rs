//! Remote scripts: what apvm sends to a server, and how (plan §6.1).
//!
//! sshd runs every command through the login user's shell, which may be
//! bash, dash, zsh, fish or tcsh. apvm never lets that shell parse data:
//!
//! - **Stdin delivery** ([`Script`]): the remote command is `sh -s` and the
//!   script travels on stdin, so the login shell parses only `sh -s`. Values
//!   in the script may hold any character but NUL: they are POSIX-quoted
//!   ([`quote()`]), and POSIX `sh` takes them literally. Remote paths (which
//!   may hold `'`, `$`, `*` and spaces), WP-CLI arguments and variant
//!   needles (which may hold `'` and `\`) only ever travel this way.
//! - **Upload delivery** ([`UploadScript`]): stdin carries a file, so the
//!   script is the remote command `sh -c '<script>'` and the login shell
//!   parses it. Its values are limited to `[A-Za-z0-9._/+-]`, and its
//!   template is a single line with neither `\\` nor `\'`: that survives
//!   sh, bash, dash, zsh, fish and tcsh (P0).
//!
//! Every script is **framed** the same way (plan §6.1):
//!
//! ```text
//! { LC_ALL=C; export LC_ALL
//! printf '%s\n' '<nonce>'
//! cd / && <identity prefix> <argv…> </dev/null; rc=$?; [ "$rc" -eq 255 ] && rc=254; exit "$rc"
//! }
//! ```
//!
//! - The brace group makes the shell read the whole script before it runs
//!   any of it: a script cut short at its end by a dropped connection is a
//!   syntax error, never a partial command. (Startup files that eat the
//!   *start* of stdin are not covered; the missing nonce line then fails
//!   the call.)
//! - `LC_ALL=C` comes first, so sudo and coreutils speak the English the
//!   classifiers match (sudo keeps `LC_ALL` for the command).
//! - Only stdout after the per-call random [`Nonce`] line is parsed, so
//!   `.bashrc` or MOTD noise is ignored.
//! - `cd /` keeps a project `wp-cli.yml` out of play (only a root-owned
//!   `/wp-cli.yml` could still apply).
//! - `</dev/null` keeps the command from eating the rest of a stdin script
//!   (bash reads piped scripts unbuffered).
//! - Exit 255 becomes 254, so a 255 always means that ssh itself failed.
//!
//! Upload scripts use the same frame on one line, `{ …; …; }`.
//!
//! **Who runs a command** is an [`Identity`]: the login user, `sudo -n -u U
//! [-g G] --`, or `runuser -u U -g G --` for a root login without sudo.
//! What follows the prefix is never a shell. Commands that run as root are a
//! closed set, [`RootCommand`] (the probe's `stat` fallback and the opt-in
//! guarded `chown`, with numeric `+uid:+gid` owners), run through
//! [`RootAccess`] (`sudo -n -u root --`, or a root login).
//!
//! **WP-CLI is hermetic** ([`WpCli`]): `env -u PHPRC … -u WP_CONFIG_PATH
//! WP_CLI_CONFIG_PATH=/dev/null PAGER=cat <wp_cli> --skip-packages
//! --exec='umask(umask() & ~0<g>);'`, so the identity's WP-CLI config,
//! packages and code-loading environment variables never take effect,
//! arguments cannot set WP-CLI global parameters other than plan §6.12's
//! overrides, and files are created with the group bits the plugins
//! directory grants ([`FileMode`]).
//!
//! Every type here validates on construction, so an unsafe script cannot be
//! built: a refused value is an [`Error::InvalidInput`]
//! ([`Code::InvalidInput`](crate::Code::InvalidInput)) naming the rule.
//!
//! # Example
//!
//! ```
//! use apvm_sites::remote::{FileMode, Identity, Nonce, Script, WpCli};
//!
//! # fn main() -> apvm_sites::Result<()> {
//! let identity = Identity::sudo("www-data", Some("www-data"))?;
//! let wp = WpCli::new(&["wp".to_string()], FileMode::new(0o2775)?, "/var/www/shop")?
//!     .args(["plugin", "list", "--format=json"])?;
//! let script = Script::wp(Nonce::random(), &identity, &wp);
//!
//! assert_eq!(Script::REMOTE_COMMAND, "sh -s");
//! assert!(script.text().starts_with("{ LC_ALL=C; export LC_ALL\n"));
//! assert!(script.text().contains("cd / && sudo -n -u 'www-data' -g 'www-data' -- env -u "));
//! assert!(script.text().contains(
//!     "WP_CLI_CONFIG_PATH=/dev/null PAGER=cat 'wp' --skip-packages \
//!      --exec='umask(umask() & ~0070);' --path='/var/www/shop' \
//!      'plugin' 'list' '--format=json' </dev/null"
//! ));
//! // WP-CLI global parameters cannot be smuggled in as arguments.
//! assert!(wp.clone().arg("--require=/tmp/x.php").is_err());
//! assert!(script.text().ends_with("exit \"$rc\"\n}\n"));
//! # Ok(())
//! # }
//! ```

mod identity;
mod quote;
mod script;
mod statement;

pub use identity::{AccountName, Identity, OwnerIds, PluginEntry, RootAccess, RootCommand};
pub use quote::quote;
pub use script::{FileMode, Nonce, Script, UploadScript, WpCli};

use crate::error::Error;
use crate::problem::Problem;

/// An [`Error::InvalidInput`] for one refused value.
///
/// # Arguments
///
/// * `subject` - What was being built, such as `upload script`
/// * `location` - The refused value's name, such as `destination`
/// * `reason` - Why it was refused
fn invalid(subject: &str, location: &str, reason: impl Into<String>) -> Error {
    Error::InvalidInput {
        subject: subject.to_string(),
        problems: vec![Problem::new(location, reason)],
    }
}

#[cfg(all(test, unix))]
mod tests;
