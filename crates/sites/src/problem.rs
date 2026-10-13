//! Located validation problems, and safe quoting of values in messages.
//!
//! Validation never stops at the first problem: a [`Scope`] collects every
//! one under a location such as `sites[1].server`, so a hand-edited
//! inventory can be fixed in one pass.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::allowlist::{self, Rule};

/// One validation finding: where it is, and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Where the problem is, such as `sites[1].server` (document) or `path`
    /// (a record handed to the inventory). Built only from field names and
    /// indexes, never from input values.
    pub location: String,
    /// What is wrong. Input values in it are quoted with `'` and `\` escaped,
    /// and control and invisible formatting characters as `\u{…}`, so they
    /// can neither drive the terminal nor pass for message text.
    pub message: String,
}

impl Problem {
    /// Create a problem.
    ///
    /// # Arguments
    ///
    /// * `location` - Where the problem is
    /// * `message` - What is wrong
    pub(crate) fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            message: message.into(),
        }
    }

    /// Join problems into one line, separated by `; `.
    pub(crate) fn join(problems: &[Problem]) -> String {
        problems
            .iter()
            .map(Problem::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl std::fmt::Display for Problem {
    /// `location: message`, or the message alone when there is no location.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.location.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.location, self.message)
        }
    }
}

/// Collects problems under a location prefix.
pub(crate) struct Scope<'a> {
    /// Location of the value being checked (`""` at the root).
    at: String,
    /// Where problems are collected.
    out: &'a mut Vec<Problem>,
}

impl<'a> Scope<'a> {
    /// A scope at the root (no location prefix).
    ///
    /// # Arguments
    ///
    /// * `out` - Where problems are collected
    pub(crate) fn root(out: &'a mut Vec<Problem>) -> Self {
        Self {
            at: String::new(),
            out,
        }
    }

    /// The location of `field` inside this scope (`sites[1]` + `server` →
    /// `sites[1].server`).
    fn location(&self, field: &str) -> String {
        if self.at.is_empty() {
            field.to_string()
        } else {
            format!("{}.{field}", self.at)
        }
    }

    /// A nested scope for `field`, collecting into the same list.
    ///
    /// # Arguments
    ///
    /// * `field` - Field name, possibly indexed (`servers[0]`)
    pub(crate) fn child(&mut self, field: &str) -> Scope<'_> {
        Scope {
            at: self.location(field),
            out: self.out,
        }
    }

    /// Record a problem at `field`.
    ///
    /// # Arguments
    ///
    /// * `field` - Field name inside this scope
    /// * `message` - What is wrong
    pub(crate) fn problem(&mut self, field: &str, message: impl Into<String>) {
        let location = self.location(field);
        self.out.push(Problem::new(location, message));
    }

    /// Record `verdict`'s reason at `field` when it is a refusal.
    ///
    /// # Arguments
    ///
    /// * `field` - Field name inside this scope
    /// * `verdict` - The outcome of an allowlist rule
    pub(crate) fn check(&mut self, field: &str, verdict: Rule) {
        if let Err(reason) = verdict {
            self.problem(field, reason);
        }
    }

    /// Apply `rule` to an optional value: an absent value passes.
    ///
    /// # Arguments
    ///
    /// * `field` - Field name inside this scope
    /// * `value` - The value, if set
    /// * `rule` - The allowlist rule for it
    pub(crate) fn check_opt<T: ?Sized>(
        &mut self,
        field: &str,
        value: Option<&T>,
        rule: impl FnOnce(&T) -> Rule,
    ) {
        if let Some(value) = value {
            self.check(field, rule(value));
        }
    }

    /// Record that this scope's record uses `key`, and report a duplicate
    /// against the record that used it first.
    ///
    /// # Arguments
    ///
    /// * `seen` - Where each key was first used
    /// * `field` - Field holding the key (`id`, `name`, `slug`)
    /// * `what` - What the key is, for the message (`id`, `project`, …)
    /// * `key` - The key
    pub(crate) fn unique<'k>(
        &mut self,
        seen: &mut FirstSeen<'k>,
        field: &str,
        what: &str,
        key: &'k str,
    ) {
        if let Some(first) = seen.note(key, &self.at) {
            self.problem(
                field,
                format!("duplicate {what} {} (first at {first})", quoted(key)),
            );
        }
    }
}

/// The location where each key was first used, to report duplicates.
#[derive(Default)]
pub(crate) struct FirstSeen<'k> {
    /// Key → location of the record that used it first.
    locations: HashMap<&'k str, String>,
}

impl<'k> FirstSeen<'k> {
    /// Note that the record at `location` uses `key`.
    ///
    /// # Returns
    ///
    /// `None` the first time; afterwards, the location of the first use.
    fn note(&mut self, key: &'k str, location: &str) -> Option<String> {
        match self.locations.entry(key) {
            Entry::Occupied(first) => Some(first.get().clone()),
            Entry::Vacant(slot) => {
                slot.insert(location.to_string());
                None
            }
        }
    }

    /// Every key noted, for checking references to them.
    pub(crate) fn keys(&self) -> std::collections::HashSet<&'k str> {
        self.locations.keys().copied().collect()
    }
}

/// `value` in single quotes, unambiguous and safe to print: `'` and `\` are
/// backslash-escaped (so a value cannot close its quotes and pass for
/// message text), and control and invisible formatting characters are
/// escaped as `\u{…}` (so it cannot drive or reorder the terminal).
pub(crate) fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        match c {
            '\'' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if allowlist::is_forbidden(c) => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `text` with control and invisible formatting characters escaped
/// (`\u{1b}`); everything else is kept as is. For messages written by
/// others (serde_json's) that may echo input.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if allowlist::is_forbidden(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// A local path or program in single quotes, for messages: control and
/// invisible formatting characters escaped as by [`printable`], backslashes
/// kept single so a Windows path reads as the platform shows it. Use it,
/// not [`quoted`], for local paths (`quoted` doubles every `\\`).
pub(crate) fn quoted_path(path: &str) -> String {
    format!("'{}'", printable(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_display_with_and_without_location() {
        assert_eq!(
            Problem::new("sites[1].server", "unknown server 'eu-2'").to_string(),
            "sites[1].server: unknown server 'eu-2'"
        );
        assert_eq!(Problem::new("", "bad").to_string(), "bad");
        assert_eq!(
            Problem::join(&[Problem::new("a", "x"), Problem::new("b", "y")]),
            "a: x; b: y"
        );
        assert_eq!(Problem::join(&[]), "");
    }

    #[test]
    fn scopes_build_nested_locations() {
        let mut out = Vec::new();
        let mut root = Scope::root(&mut out);
        root.problem("version", "must be at least 1");
        {
            let mut server = root.child("servers[0]");
            server.check("host", Err("must not be empty".into()));
            server.check("id", Ok(()));
            let mut ssh = server.child("ssh");
            ssh.check_opt("port", Some(&0u16), |port| allowlist::port(*port));
            ssh.check_opt::<str>("user", None, allowlist::user);
        }
        assert_eq!(
            out.iter().map(|p| p.location.as_str()).collect::<Vec<_>>(),
            ["version", "servers[0].host", "servers[0].ssh.port"]
        );
    }

    #[test]
    fn duplicates_point_at_the_first_use() {
        let mut out = Vec::new();
        let mut root = Scope::root(&mut out);
        let mut seen = FirstSeen::default();
        for (index, key) in ["a", "b", "a", "a"].into_iter().enumerate() {
            root.child(&format!("list[{index}]"))
                .unique(&mut seen, "id", "id", key);
        }
        assert_eq!(
            out.iter().map(Problem::to_string).collect::<Vec<_>>(),
            [
                "list[2].id: duplicate id 'a' (first at list[0])",
                "list[3].id: duplicate id 'a' (first at list[0])",
            ]
        );
        assert_eq!(seen.keys(), ["a", "b"].into_iter().collect());
    }

    #[test]
    fn quoting_escapes_quotes_backslashes_and_invisible_characters() {
        assert_eq!(quoted("eu-1"), "'eu-1'");
        assert_eq!(quoted("café 😀"), "'café 😀'");
        assert_eq!(quoted("C:\\keys"), "'C:\\\\keys'");
        // A value cannot close its quotes and forge more message text.
        assert_eq!(quoted("x', 'y"), "'x\\', \\'y'");
        assert_eq!(quoted("a\u{1b}[2J"), "'a\\u{1b}[2J'");
        assert_eq!(quoted("a\nb"), "'a\\u{a}b'");
        assert_eq!(quoted("\u{202e}txt"), "'\\u{202e}txt'");
        assert_eq!(quoted("tag\u{e0041}"), "'tag\\u{e0041}'");
        assert_eq!(quoted("zw\u{200b}sp"), "'zw\\u{200b}sp'");
    }

    #[test]
    fn printable_escapes_only_invisible_characters() {
        assert_eq!(printable("x\u{7f}"), "x\\u{7f}");
        assert_eq!(
            printable("unknown field `a\u{2028}b`"),
            "unknown field `a\\u{2028}b`"
        );
        assert_eq!(printable("it's C:\\x"), "it's C:\\x");
    }

    #[test]
    fn quoted_path_keeps_backslashes_and_escapes_invisible_characters() {
        assert_eq!(
            quoted_path("C:\\Users\\RUNNER~1\\kh"),
            "'C:\\Users\\RUNNER~1\\kh'"
        );
        assert_eq!(quoted_path("/tmp/a\u{1b}b"), "'/tmp/a\\u{1b}b'");
    }
}
