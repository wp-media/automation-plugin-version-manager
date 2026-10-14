//! Why sudo refused a command (plan §6.2's sudo classification, from the
//! facts of §3.5). Pure: the probe runs the commands.
//!
//! With `-n`, sudo prints `sudo: a password is required` for **every**
//! refusal but requiretty (it authenticates before it applies the
//! policy), so a refused command's stderr alone cannot tell the cases
//! apart. The probe then lists the one rule it needed, `sudo -n -l -u U
//! [-g G] <command>` ([`listing`]):
//!
//! | Signal | Kind |
//! |---|---|
//! | `sudo: sorry, you must have a tty to run sudo` | [`SudoRefusalKind::Requiretty`] |
//! | the listing exits 1 with no output (password-less rules, but not this one) | [`SudoRefusalKind::NotAllowed`] |
//! | the listing succeeds, the command still needs a password (`listpw=any`) | [`SudoRefusalKind::PasswordRequired`] |
//! | the command was refused otherwise (`… is not allowed to execute …`) | [`SudoRefusalKind::NotAllowed`] |
//! | the listing says `a password is required` (no password-less rule) | [`SudoRefusalKind::PasswordRequired`] |
//!
//! **sudo-rs** (verified with 0.2.13 on Ubuntu 26.04; its messages start
//! with `sudo:`, or `sudo-rs:` on 25.10, so only the phrases are matched)
//! says more: `interactive authentication is required` where sudo says "a
//! password is required" (a rule that needs one, or the listing of a user
//! whose rules all do), `I'm sorry <user>. I'm afraid I can't do that` for a
//! command no rule allows (the user may have no rule at all), and the
//! listing `Sorry, user <user> may not run sudo on <host>.` for a user
//! without any rule: [`SudoRefusalKind::NotAllowed`]. It has no requiretty.
//!
//! Every refused command is logged by sudo on the server; the probe never
//! retries one.

use super::AccountName;
use crate::error::Code;
use crate::problem::quoted;
use crate::remote::quote::Word;
use crate::remote::statement::Statement;
use crate::remote::steps::StepOutput;

/// sudo's refusal without a terminal (requiretty).
const TTY: &str = "sorry, you must have a tty to run sudo";
/// sudo's refusal of every other case under `-n`, and sudo-rs's refusal
/// of a rule that needs a password.
const PASSWORD: [&str; 2] = [
    "a password is required",
    "interactive authentication is required",
];
/// What sudo prints when a rule matched but forbids the command (without
/// `-n`'s authentication first), and sudo-rs's refusals of a command no
/// rule allows and of a listing for a user without rules.
const NOT_ALLOWED: [&str; 4] = [
    "is not allowed to execute",
    "is not in the sudoers file",
    "I'm afraid I can't do that",
    "may not run sudo on",
];

/// The starts of the lines sudo itself prints: its own messages
/// (`sudo:`, or `sudo-rs:` on Ubuntu 25.10) and classic sudo's policy
/// refusal (`Sorry, user … is not allowed to execute …`). A phrase counts
/// only on such a line, so a command's own output (WP-CLI, a login
/// script) quoting sudo's words decides nothing.
const SUDO_LINES: [&str; 3] = ["sudo: ", "sudo-rs: ", "Sorry, user "];

/// Whether a line of `step`'s stderr that sudo printed holds any of
/// `phrases`.
///
/// # Arguments
///
/// * `step` - The output
/// * `phrases` - The phrases
fn says(step: &StepOutput, phrases: &[&str]) -> bool {
    String::from_utf8_lossy(&step.stderr).lines().any(|line| {
        let line = line.trim_end_matches('\r');
        SUDO_LINES.iter().any(|start| line.starts_with(start))
            && phrases.iter().any(|phrase| line.contains(phrase))
    })
}

/// What sudo's stderr says about a refused command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SudoSignal {
    /// requiretty.
    Tty,
    /// "a password is required": any refusal under `-n`.
    Password,
    /// A rule forbids it, said outright.
    NotAllowed,
}

/// Whether `stderr` is sudo refusing the command, and how.
///
/// # Arguments
///
/// * `step` - The refused command's output
pub(crate) fn signal(step: &StepOutput) -> Option<SudoSignal> {
    if says(step, &[TTY]) {
        return Some(SudoSignal::Tty);
    }
    if says(step, &PASSWORD) {
        return Some(SudoSignal::Password);
    }
    says(step, &NOT_ALLOWED).then_some(SudoSignal::NotAllowed)
}

/// `sudo -n -l -u <user> [-g <group>] <command>`, as the login user: lists
/// the rule for that one command without running anything.
///
/// # Arguments
///
/// * `user` - The target user
/// * `group` - The target group, if any
/// * `command` - The command (`true` for an identity, `stat` for root)
pub(crate) fn listing(
    user: &AccountName,
    group: Option<&AccountName>,
    command: &'static str,
) -> Statement {
    let mut args = ["-n", "-l", "-u"].map(Word::lit).to_vec();
    args.push(Word::checked(user.as_str()));
    if let Some(group) = group {
        args.extend([Word::lit("-g"), Word::checked(group.as_str())]);
    }
    args.push(Word::lit(command));
    Statement::as_identity(&super::Identity::Login, "sudo", args)
}

/// How sudo refused, from the command's signal and, for
/// [`SudoSignal::Password`], the [`listing`]'s result (the module's table).
///
/// # Arguments
///
/// * `signal` - What the refused command's stderr said
/// * `listing` - The listing's output, when it ran
pub(crate) fn classify(signal: SudoSignal, listing: Option<&StepOutput>) -> SudoRefusalKind {
    match (signal, listing) {
        (SudoSignal::Tty, _) => SudoRefusalKind::Requiretty,
        (SudoSignal::NotAllowed, _) => SudoRefusalKind::NotAllowed,
        (SudoSignal::Password, None) => SudoRefusalKind::PasswordRequired,
        (SudoSignal::Password, Some(listing)) if says(listing, &[TTY]) => {
            SudoRefusalKind::Requiretty
        }
        (SudoSignal::Password, Some(listing)) if says(listing, &NOT_ALLOWED) => {
            SudoRefusalKind::NotAllowed
        }
        (SudoSignal::Password, Some(listing)) if says(listing, &PASSWORD) || listing.success() => {
            SudoRefusalKind::PasswordRequired
        }
        (SudoSignal::Password, Some(_)) => SudoRefusalKind::NotAllowed,
    }
}

/// How sudo refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SudoRefusalKind {
    /// sudo needs a terminal (`sudo_requiretty`).
    Requiretty,
    /// The rule needs a password, or the login user has no password-less
    /// rule (`sudo_password_required`).
    PasswordRequired,
    /// No rule lets the login user run it (`sudo_not_allowed`).
    NotAllowed,
}

/// sudo refused a command the probe needs: who asked, as whom, for what,
/// and the sudoers change that fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SudoRefusal {
    /// How.
    pub kind: SudoRefusalKind,
    /// The login user's name, when it has one.
    pub login: Option<String>,
    /// The target user.
    pub user: String,
    /// The target group, if any.
    pub group: Option<String>,
    /// What the rule must allow: `ALL` for an identity (apvm runs `env` and
    /// the WP-CLI argv there), `/usr/bin/stat` for the probe's root `stat`.
    pub command: &'static str,
}

impl SudoRefusal {
    /// The stable code (plan §4.5).
    pub fn code(&self) -> Code {
        match self.kind {
            SudoRefusalKind::Requiretty => Code::SudoRequiretty,
            SudoRefusalKind::PasswordRequired => Code::SudoPasswordRequired,
            SudoRefusalKind::NotAllowed => Code::SudoNotAllowed,
        }
    }

    /// The sudoers line the fix needs: `<login> ALL=(<user>[ : <group>])
    /// NOPASSWD: <command>`.
    pub fn rule(&self) -> String {
        let login = self.login.as_deref().unwrap_or("<login user>");
        let runas = match &self.group {
            Some(group) => format!("{} : {group}", self.user),
            None => self.user.clone(),
        };
        format!("{login} ALL=({runas}) NOPASSWD: {}", self.command)
    }

    /// Who the command was asked for, in messages: `'user'` or
    /// `'user:group'`.
    fn target(&self) -> String {
        match &self.group {
            Some(group) => quoted(&format!("{}:{group}", self.user)),
            None => quoted(&self.user),
        }
    }

    /// The login user in messages.
    fn who(&self) -> String {
        self.login
            .as_deref()
            .map_or_else(|| "the login user".to_string(), quoted)
    }
}

impl std::fmt::Display for SudoRefusal {
    /// What sudo refused, and the sudoers change that fixes it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (who, target, rule) = (self.who(), self.target(), self.rule());
        match self.kind {
            SudoRefusalKind::Requiretty => write!(
                f,
                "sudo requires a terminal for {who} (requiretty), and apvm runs without one: \
                 turn it off for this user (Defaults:{} !requiretty)",
                self.login.as_deref().unwrap_or("<login user>")
            ),
            SudoRefusalKind::PasswordRequired => write!(
                f,
                "sudo asks {who} for a password to run commands as {target}: make the rule \
                 password-less ({rule})"
            ),
            SudoRefusalKind::NotAllowed => write!(
                f,
                "sudo does not let {who} run commands as {target}: add the rule ({rule})"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step that exited `status` with `stderr`.
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

    /// A refusal of `kind` for `deploy` as `www-data:www-data`.
    ///
    /// # Arguments
    ///
    /// * `kind` - How sudo refused
    fn refusal(kind: SudoRefusalKind) -> SudoRefusal {
        SudoRefusal {
            kind,
            login: Some("deploy".into()),
            user: "www-data".into(),
            group: Some("www-data".into()),
            command: "ALL",
        }
    }

    #[test]
    fn signals_come_from_sudos_own_lines() {
        let cases = [
            (format!("sudo: {TTY}\n"), Some(SudoSignal::Tty)),
            (
                format!("sudo: {}\n", PASSWORD[0]),
                Some(SudoSignal::Password),
            ),
            // Sudo's words on a line of another program's: nothing.
            ("Error: sudo: a password is required\n".into(), None),
            ("hint: is not allowed to execute\n".into(), None),
            (
                "Sorry, user deploy is not allowed to execute '/usr/bin/env' as www-data on x.\n"
                    .to_string(),
                Some(SudoSignal::NotAllowed),
            ),
            (
                "Error: This does not seem to be a WordPress installation.\n".into(),
                None,
            ),
            (String::new(), None),
        ];
        for (stderr, expected) in cases {
            assert_eq!(signal(&step(1, "", &stderr)), expected, "{stderr:?}");
        }
    }

    #[test]
    fn the_classification_table() {
        // Captured from the harness (sudo 1.9.16p2, plan §3.5).
        let password = format!("sudo: {}\n", PASSWORD[0]);
        let tty = format!("sudo: {TTY}\n");
        let listed = step(0, "/usr/bin/true\n", "");
        let other_rules = step(1, "", "");
        let no_rules = step(1, "", &password);
        let cases = [
            (SudoSignal::Tty, None, SudoRefusalKind::Requiretty),
            (SudoSignal::NotAllowed, None, SudoRefusalKind::NotAllowed),
            (
                SudoSignal::Password,
                None,
                SudoRefusalKind::PasswordRequired,
            ),
            (
                SudoSignal::Password,
                Some(&listed),
                SudoRefusalKind::PasswordRequired,
            ),
            (
                SudoSignal::Password,
                Some(&other_rules),
                SudoRefusalKind::NotAllowed,
            ),
            (
                SudoSignal::Password,
                Some(&no_rules),
                SudoRefusalKind::PasswordRequired,
            ),
        ];
        for (signal, listing, expected) in cases {
            assert_eq!(
                classify(signal, listing),
                expected,
                "{signal:?} {listing:?}"
            );
        }
        let tty_listing = step(1, "", &tty);
        assert_eq!(
            classify(SudoSignal::Password, Some(&tty_listing)),
            SudoRefusalKind::Requiretty
        );
    }

    #[test]
    fn sudo_rs_refusals_are_classified() {
        // Captured with sudo-rs 0.2.13 (Ubuntu 26.04), both prefixes.
        let needs_password = step(1, "", "sudo: interactive authentication is required\n");
        let no_rule = step(
            1,
            "",
            "sudo-rs: I'm sorry plain. I'm afraid I can't do that\n",
        );
        assert_eq!(signal(&needs_password), Some(SudoSignal::Password));
        assert_eq!(signal(&no_rule), Some(SudoSignal::NotAllowed));
        assert_eq!(
            classify(SudoSignal::NotAllowed, None),
            SudoRefusalKind::NotAllowed
        );
        let cases = [
            // `mixed`: the rule is listed but needs a password.
            (
                step(0, "/usr/bin/true\n", ""),
                SudoRefusalKind::PasswordRequired,
            ),
            // Every rule of the user needs a password.
            (needs_password.clone(), SudoRefusalKind::PasswordRequired),
            // No rule at all.
            (
                step(
                    1,
                    "",
                    "sudo: Sorry, user plain may not run sudo on c0ffee.\n",
                ),
                SudoRefusalKind::NotAllowed,
            ),
            // Rules, but not this one.
            (step(1, "", ""), SudoRefusalKind::NotAllowed),
        ];
        for (listing, expected) in cases {
            assert_eq!(
                classify(SudoSignal::Password, Some(&listing)),
                expected,
                "{listing:?}"
            );
        }
    }

    #[test]
    fn listings_name_the_one_rule_needed() {
        let user = AccountName::new("www-data").unwrap();
        let group = AccountName::new("web").unwrap();
        assert_eq!(
            listing(&user, Some(&group), "true").render(),
            "sudo -n -l -u 'www-data' -g 'web' true"
        );
        assert_eq!(
            listing(&AccountName::new("root").unwrap(), None, "stat").render(),
            "sudo -n -l -u 'root' stat"
        );
    }

    #[test]
    fn refusals_carry_their_code_and_fix() {
        let cases = [
            (
                SudoRefusalKind::Requiretty,
                Code::SudoRequiretty,
                "Defaults:deploy !requiretty",
            ),
            (
                SudoRefusalKind::PasswordRequired,
                Code::SudoPasswordRequired,
                "(deploy ALL=(www-data : www-data) NOPASSWD: ALL)",
            ),
            (
                SudoRefusalKind::NotAllowed,
                Code::SudoNotAllowed,
                "add the rule (deploy ALL=(www-data : www-data) NOPASSWD: ALL)",
            ),
        ];
        for (kind, code, fix) in cases {
            let refusal = refusal(kind);
            assert_eq!(refusal.code(), code);
            assert!(refusal.to_string().contains(fix), "{refusal}");
        }
        let root = SudoRefusal {
            kind: SudoRefusalKind::NotAllowed,
            login: None,
            user: "root".into(),
            group: None,
            command: "/usr/bin/stat",
        };
        assert_eq!(
            root.rule(),
            "<login user> ALL=(root) NOPASSWD: /usr/bin/stat"
        );
        assert_eq!(
            root.to_string(),
            "sudo does not let the login user run commands as 'root': add the rule \
             (<login user> ALL=(root) NOPASSWD: /usr/bin/stat)"
        );
    }
}
