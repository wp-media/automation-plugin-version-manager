//! The read-only commands of the probe (plan §6.2 steps H and C), as
//! typed statements: what `id`, `command -v`, `stat`, `test` and `df` are
//! asked, and the plugins directory walk ([`Inspect::Walk`], a constant
//! script whose data are positional parameters). They run as independent
//! [`Steps`], each with its own status and output, one round trip per
//! probe step.

use super::identity::{AccountName, Identity, RootAccess, RootCommand};
use super::invalid;
use super::quote::Word;
use super::script::WpCli;
use super::statement::Statement;
use super::steps::Steps;
use crate::allowlist;
use crate::error::Result;

/// GNU and busybox `stat -L -c` format: owner uid, group gid, mode in
/// octal, file type, owner name, group name, one per line (names last:
/// they may hold spaces; `UNKNOWN` when an id has no name).
const GNU_FORMAT: &str = "'%u\n%g\n%a\n%F\n%U\n%G'";
/// The BSD `stat -L -f` equivalent (verified with macOS's stat): `%Mp%Lp`
/// prints the mode as four octal digits (`0755`, `2775`), `%HT` the type
/// (`Directory`), and `%Su`/`%Sg` print the id itself when it has no name.
const BSD_FORMAT: &str = "'%u%n%g%n%Mp%Lp%n%HT%n%Su%n%Sg'";

/// `stat`'s dialect on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatFlavor {
    /// GNU coreutils and busybox: `stat -c`.
    Gnu,
    /// BSD and macOS: `stat -f`.
    Bsd,
}

impl StatFlavor {
    /// The format option, the six-line ownership format, and the
    /// "owner mode" format of a containing directory.
    fn formats(self) -> [&'static str; 3] {
        match self {
            Self::Gnu => ["-c", GNU_FORMAT, "'%u %a'"],
            Self::Bsd => ["-f", BSD_FORMAT, "'%u %Lp'"],
        }
    }
}

/// The plugins directory walk (plan §6.2 step H, P4.2 audits): one `sh`
/// process enters the configured directory one component at a time
/// (`cd -P`), then prints its physical path (`pwd -P`) and `stat`s `.`,
/// so the path and the ownership describe the **same** directory, never
/// two lookups a site could swap a link between. Components are taken from
/// a queue (the positional parameters):
///
/// - A component that is not a symbolic link must land at the previous
///   physical directory plus its name, checked after entering it (a link
///   swapped in after the `-h` test is caught): else exit 72.
/// - A symbolic link is followed only when the site cannot have planted
///   or replaced it: the link and the directory holding it are owned by
///   root or the login user (`$1`), and that directory is writable by
///   neither its group nor others (a release layout's `current ->
///   releases/<n>`, `/var/www -> /srv/www`); else exit 73. Its target is
///   never handed to the kernel whole: `readlink` reads it, and its
///   components join the front of the queue (from `/` for an absolute
///   target), each checked by the same rules, so a trusted link into a
///   directory the site owns cannot reach through a link the site planted
///   there (audit 3). `..` moves to the physical parent. More than 40
///   links: exit 75.
/// - Every component is used as `./<name>`, so a name such as `-` is
///   never an option.
/// - A component that cannot be entered: exit 70. Step H's walk comes
///   with a separate `stat` diagnostic that says why (denied, missing, a
///   BSD `stat`); the identity's confirming walk has none, so its 70 is
///   `probe_failed`. A `stat` or `readlink` that fails: exit 74.
/// - A link target keeps its trailing newlines (`readlink` output ends
///   with a `/` marker, removed after the command substitution).
///
/// Parameters: login uid (empty: `id -u`), `stat`'s format option, the
/// six-line format, the directory "owner mode" format, the link owner
/// format, then the components. The text is constant (no `'`), and data
/// reach it only as positional parameters, like a constant `wp eval`
/// snippet (§10). It needs `sh`, `id`, `stat`, `readlink` and `pwd`
/// (POSIX; GNU, busybox and BSD/macOS all have them).
const WALK: &str = r#"'u=$1 f=$2 six=$3 dm=$4 lo=$5
shift 5
[ -n "$u" ] || u=$(id -u) || exit 74
set -f
n=0
cd -P / || exit 70
p=
while [ "$#" -gt 0 ]; do
  c=$1
  shift
  case $c in
    "" | .) continue ;;
    ..)
      cd -P .. || exit 70
      p=$(pwd -P) || exit 70
      [ "$p" = / ] && p=
      continue ;;
  esac
  if [ -h "./$c" ]; then
    m=$(stat "$f" "$dm" .) || exit 74
    l=$(stat "$f" "$lo" -- "./$c") || exit 74
    o=${m%% *} a=${m##* }
    a=${a#"${a%??}"}
    case "$o:$l:$a" in
      0:0:[0145][0145] | 0:"$u":[0145][0145] | "$u":0:[0145][0145] | "$u":"$u":[0145][0145]) ;;
      *) printf "apvm: unprotected link %s\n" "$c" >&2; exit 73 ;;
    esac
    n=$((n + 1))
    [ "$n" -le 40 ] || { printf "apvm: too many links at %s\n" "$c" >&2; exit 75; }
    t=$(readlink -- "./$c" && echo /) || exit 74
    t=${t%/}
    t=${t%?}
    case $t in
      /*) cd -P / || exit 70; p= ;;
    esac
    s=$IFS; IFS=/; set -- $t "$@"; IFS=$s
  else
    cd -P -- "./$c" || exit 70
    q=$(pwd -P) || exit 70
    [ "$q" = "$p/$c" ] || { printf "apvm: moved %s\n" "$c" >&2; exit 72; }
    p=$q
  fi
done
printf "%s\n" "${p:-/}"
exec stat "$f" "$six" .'"#;

/// The walk's exit status when a component could not be entered.
pub(crate) const WALK_UNREACHABLE: i32 = 70;

/// Whether the walk's exit `status` means it refused the path: it landed
/// elsewhere than the path says (72), met a link the site could have
/// planted (73), or followed too many links (75).
///
/// # Arguments
///
/// * `status` - The walk's exit status
pub(crate) fn walk_refused(status: i32) -> bool {
    matches!(status, 72 | 73 | 75)
}

/// One read-only command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Inspect<'a> {
    /// `id -u`.
    Uid,
    /// `id -g`.
    Gid,
    /// `id -un`.
    UserName,
    /// `id -gn`.
    GroupName,
    /// `id -G`.
    Groups,
    /// `printenv SHELL`.
    Shell,
    /// `command -v <program>`: a tool name or the site's `wp_cli`
    /// program (`[A-Za-z0-9._/+-]`, plan §7).
    Which(&'a str),
    /// The plugins directory walk ([`WALK`]), the login user's uid
    /// trusted for links besides root.
    Walk {
        /// The configured directory.
        path: &'a str,
        /// The SSH login user's uid; `None` when the login user runs the
        /// walk itself (`id -u` then).
        login_uid: Option<u32>,
        /// `stat`'s dialect.
        flavor: StatFlavor,
    },
    /// `stat -L -c '<GNU format>' -- <path>`.
    StatGnu(&'a str),
    /// `stat -L -f '<BSD format>' -- <path>`.
    StatBsd(&'a str),
    /// `id -u <name>`.
    UserId(&'a AccountName),
    /// `test -w <path>`.
    Writable(&'a str),
    /// `test -d <path>`.
    IsDir(&'a str),
    /// `df -Pk <path>`.
    FreeSpace(&'a str),
}

impl Inspect<'_> {
    /// The command as a statement run as `identity`.
    ///
    /// # Arguments
    ///
    /// * `identity` - Who runs it
    ///
    /// # Errors
    ///
    /// `invalid_input` when a path breaks the remote path rule, or a
    /// program the `wp_cli` token rule.
    fn statement(&self, identity: &Identity) -> Result<Statement> {
        let (program, args) = self.argv()?;
        Ok(Statement::as_identity(identity, program, args))
    }

    /// The program and its words.
    fn argv(&self) -> Result<(&'static str, Vec<Word>)> {
        let lits = |words: &[&'static str]| words.iter().map(|w| Word::lit(w)).collect::<Vec<_>>();
        Ok(match *self {
            Self::Uid => ("id", lits(&["-u"])),
            Self::Gid => ("id", lits(&["-g"])),
            Self::UserName => ("id", lits(&["-un"])),
            Self::GroupName => ("id", lits(&["-gn"])),
            Self::Groups => ("id", lits(&["-G"])),
            Self::Shell => ("printenv", lits(&["SHELL"])),
            Self::Which(program) => ("command", with(lits(&["-v"]), program_word(program)?)),
            Self::Walk {
                path,
                login_uid,
                flavor,
            } => ("env", walk_args(path, login_uid, flavor)?),
            Self::StatGnu(path) => ("stat", stat_args("-c", GNU_FORMAT, path)?),
            Self::StatBsd(path) => ("stat", stat_args("-f", BSD_FORMAT, path)?),
            Self::UserId(name) => ("id", with(lits(&["-u"]), Word::checked(name.as_str()))),
            Self::Writable(path) => ("test", with(lits(&["-w"]), path_word(path)?)),
            Self::IsDir(path) => ("test", with(lits(&["-d"]), path_word(path)?)),
            Self::FreeSpace(path) => ("df", with(lits(&["-Pk"]), path_word(path)?)),
        })
    }
}

/// `words` with `last` appended.
///
/// # Arguments
///
/// * `words` - The first words
/// * `last` - The word appended
fn with(mut words: Vec<Word>, last: Word) -> Vec<Word> {
    words.push(last);
    words
}

/// `env`'s arguments for the walk: the variables that would make `sh`
/// read code or change `cd` removed, then `sh -c '<WALK>' sh <params…>`.
///
/// # Arguments
///
/// * `path` - The configured directory
/// * `login_uid` - The login user's uid, if the walk runs as another user
/// * `flavor` - `stat`'s dialect
fn walk_args(path: &str, login_uid: Option<u32>, flavor: StatFlavor) -> Result<Vec<Word>> {
    path_word(path)?;
    let mut words = lits_of(&["-u", "BASH_ENV", "-u", "ENV", "-u", "CDPATH", "sh", "-c"]);
    words.extend([Word::lit(WALK), Word::lit("sh")]);
    let uid = login_uid.map(|uid| uid.to_string()).unwrap_or_default();
    words.push(Word::checked(&uid));
    let [flag, six, dir_mode] = flavor.formats();
    words.extend(lits_of(&[flag, six, dir_mode, "%u"]));
    words.extend(path.split('/').skip(1).map(Word::checked));
    Ok(words)
}

/// Literal words.
///
/// # Arguments
///
/// * `words` - The literals
fn lits_of(words: &[&'static str]) -> Vec<Word> {
    words.iter().map(|word| Word::lit(word)).collect()
}

/// `-L <flag> <format> -- <path>`.
///
/// # Arguments
///
/// * `flag` - `-c` (GNU) or `-f` (BSD)
/// * `format` - The literal format word
/// * `path` - The path
fn stat_args(flag: &'static str, format: &'static str, path: &str) -> Result<Vec<Word>> {
    Ok(vec![
        Word::lit("-L"),
        Word::lit(flag),
        Word::lit(format),
        Word::lit("--"),
        path_word(path)?,
    ])
}

/// A checked remote path.
///
/// # Arguments
///
/// * `path` - Absolute and canonical (plan §7)
fn path_word(path: &str) -> Result<Word> {
    allowlist::remote_path(path).map_err(|reason| invalid("probe command", "path", reason))?;
    Ok(Word::checked(path))
}

/// A checked program name or path.
///
/// # Arguments
///
/// * `program` - A command name or an absolute canonical path of
///   `[A-Za-z0-9._/+-]`, no leading `-` (the `wp_cli` program rule)
fn program_word(program: &str) -> Result<Word> {
    allowlist::wp_cli_program(program)
        .map_err(|reason| invalid("probe command", "program", reason))?;
    Ok(Word::checked(program))
}

/// `checks` as independent steps run as `identity`.
///
/// # Arguments
///
/// * `identity` - Who runs them
/// * `checks` - The commands, at least one
///
/// # Errors
///
/// `invalid_input` for a refused value, or when `checks` is empty.
pub(crate) fn steps(identity: &Identity, checks: &[Inspect<'_>]) -> Result<Steps> {
    let mut statements = checks.iter().map(|check| check.statement(identity));
    let Some(first) = statements.next() else {
        return Err(invalid("probe command", "checks", "must not be empty"));
    };
    Ok(Steps::new(first?, statements.collect::<Result<Vec<_>>>()?))
}

/// One root command as a single step, so its own stderr (sudo's refusal)
/// is kept apart from login noise.
///
/// # Arguments
///
/// * `access` - How root is reached
/// * `command` - The root command
pub(crate) fn root_step(access: RootAccess, command: &RootCommand) -> Steps {
    Steps::new(command.statement(access), [])
}

/// One WP-CLI command as a single step run as `identity`, so its own
/// stderr (sudo's refusal, WP-CLI's errors) is kept apart from login noise.
///
/// # Arguments
///
/// * `identity` - Who runs it
/// * `command` - The hermetic WP-CLI command
pub(crate) fn wp_step(identity: &Identity, command: &WpCli) -> Steps {
    Steps::new(command.statement(identity), [])
}

/// The sudo listing ([`sudo::listing`](super::identity::sudo::listing)) as
/// a single step.
///
/// # Arguments
///
/// * `user` - The target user
/// * `group` - The target group, if any
/// * `command` - The command the rule must allow (`true`, `stat`)
pub(crate) fn sudo_listing_step(
    user: &AccountName,
    group: Option<&AccountName>,
    command: &'static str,
) -> Steps {
    Steps::new(super::identity::sudo::listing(user, group, command), [])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Nonce;

    /// The rendered statement of `check` as the login user.
    ///
    /// # Arguments
    ///
    /// * `check` - The command
    fn render(check: Inspect<'_>) -> String {
        check.statement(&Identity::Login).unwrap().render()
    }

    #[test]
    fn every_command_renders_quoted() {
        let www = AccountName::new("www-data").unwrap();
        let dir = "/srv/it's a $dir";
        let cases = [
            (Inspect::Uid, "id -u"),
            (Inspect::Gid, "id -g"),
            (Inspect::UserName, "id -un"),
            (Inspect::GroupName, "id -gn"),
            (Inspect::Groups, "id -G"),
            (Inspect::Shell, "printenv SHELL"),
            (Inspect::Which("sudo"), "command -v 'sudo'"),
            (
                Inspect::Which("/usr/bin/php8.3"),
                "command -v '/usr/bin/php8.3'",
            ),
            (
                Inspect::StatGnu(dir),
                "stat -L -c '%u\n%g\n%a\n%F\n%U\n%G' -- '/srv/it'\\''s a $dir'",
            ),
            (
                Inspect::StatBsd("/srv/p"),
                "stat -L -f '%u%n%g%n%Mp%Lp%n%HT%n%Su%n%Sg' -- '/srv/p'",
            ),
            (Inspect::UserId(&www), "id -u 'www-data'"),
            (Inspect::Writable("/srv/p"), "test -w '/srv/p'"),
            (Inspect::IsDir("/srv/p"), "test -d '/srv/p'"),
            (Inspect::FreeSpace("/tmp"), "df -Pk '/tmp'"),
        ];
        for (check, expected) in cases {
            assert_eq!(render(check), expected);
        }
        let sudo = Identity::sudo("www-data", Some("www-data")).unwrap();
        assert_eq!(
            Inspect::Writable("/srv/p")
                .statement(&sudo)
                .unwrap()
                .render(),
            "sudo -n -u 'www-data' -g 'www-data' -- test -w '/srv/p'"
        );
    }

    #[test]
    fn the_walk_passes_data_as_parameters_only() {
        let walk = |flavor| Inspect::Walk {
            path: "/srv/it's/wp-content/plugins",
            login_uid: Some(2001),
            flavor,
        };
        let gnu = render(walk(StatFlavor::Gnu));
        assert!(
            gnu.starts_with("env -u BASH_ENV -u ENV -u CDPATH sh -c 'u=$1 f=$2"),
            "{gnu}"
        );
        assert!(
            gnu.ends_with(
                "exec stat \"$f\" \"$six\" .' sh '2001' -c '%u\n%g\n%a\n%F\n%U\n%G' '%u %a' %u \
                 'srv' 'it'\\''s' 'wp-content' 'plugins'"
            ),
            "{gnu}"
        );
        let bsd = render(walk(StatFlavor::Bsd));
        assert!(bsd.contains("' sh '2001' -f '%u%n%g%n%Mp%Lp%n%HT%n%Su%n%Sg' '%u %Lp' %u 'srv'"));
        // The constant holds no single quote of its own.
        assert_eq!(WALK.matches('\'').count(), 2);
        let sudo = Identity::sudo("www-data", Some("www-data")).unwrap();
        let text = walk(StatFlavor::Gnu).statement(&sudo).unwrap().render();
        assert!(text.starts_with("sudo -n -u 'www-data' -g 'www-data' -- env -u BASH_ENV"));
    }

    #[test]
    fn unsafe_values_are_refused() {
        for check in [
            Inspect::Walk {
                path: "relative",
                login_uid: None,
                flavor: StatFlavor::Gnu,
            },
            Inspect::StatGnu("/a/../b"),
            Inspect::Writable("/a\nb"),
            Inspect::Which("-x"),
            Inspect::Which("a b"),
            Inspect::Which("x'y"),
        ] {
            let error = check.statement(&Identity::Login).unwrap_err();
            assert!(
                error.to_string().starts_with("invalid probe command: "),
                "{error}"
            );
        }
        assert!(steps(&Identity::Login, &[]).is_err());
    }

    #[test]
    fn checks_become_independent_steps() {
        let steps = steps(&Identity::Login, &[Inspect::Uid, Inspect::Shell]).unwrap();
        assert_eq!(steps.len(), 2);
        let text = steps.render(&Nonce::random());
        assert!(text.contains("id -u </dev/null; printf"), "{text}");
        assert!(text.contains("printenv SHELL </dev/null; printf"), "{text}");
    }
}
