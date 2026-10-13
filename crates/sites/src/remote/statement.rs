//! The commands inside a script: who runs each one, a fixed program and its
//! words (plan §6.1 items 1 and 4). Framing them (prelude, nonce, brace
//! group, exit status) is [`Script`](super::Script)'s job alone.
//!
//! Later phases add typed constructors (probe steps, ownership checks, the
//! site lock, staging) by building [`Statement`]s and a [`Body`], never by
//! writing script text.

use super::identity::{Identity, RootAccess};
use super::quote::{Word, join};

/// One command of a script: the prefix that picks who runs it (none for
/// the login user), a literal program, and its words. The program is a
/// `'static` literal of apvm's own templates, so data can never choose it,
/// and no prefix is ever followed by a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Statement {
    /// `sudo …--`, `runuser …--`, or nothing.
    prefix: Vec<Word>,
    /// The program, such as `env`, `stat` or `tee`.
    program: &'static str,
    /// Its arguments.
    args: Vec<Word>,
}

impl Statement {
    /// `program args…`, run as `identity` (no prefix for
    /// [`Identity::Login`]).
    ///
    /// # Arguments
    ///
    /// * `identity` - Who runs it
    /// * `program` - The literal program
    /// * `args` - Its arguments
    pub(crate) fn as_identity(identity: &Identity, program: &'static str, args: Vec<Word>) -> Self {
        Self {
            prefix: identity.prefix(),
            program,
            args,
        }
    }

    /// `program args…`, run as root through `access`.
    ///
    /// # Arguments
    ///
    /// * `access` - How root is reached
    /// * `program` - The literal program
    /// * `args` - Its arguments
    pub(crate) fn as_root(access: RootAccess, program: &'static str, args: Vec<Word>) -> Self {
        Self {
            prefix: access.prefix(),
            program,
            args,
        }
    }

    /// The command's shell text: prefix, program and arguments, without a
    /// redirection.
    pub(crate) fn render(&self) -> String {
        let mut words = self.prefix.clone();
        words.push(Word::lit(self.program));
        words.extend(self.args.iter().cloned());
        join(&words)
    }
}

/// The statements of a stdin script: one or more, run in order. Each gets
/// `</dev/null`, so none can read the rest of the script, and they are
/// chained with `&&`: the first failure stops the script and its status
/// becomes the script's (255 mapped to 254). Reading every statement's own
/// status (the probe's independent steps) is a later phase's addition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Body(Vec<Statement>);

impl Body {
    /// A body of `first`, then `rest` in order.
    ///
    /// # Arguments
    ///
    /// * `first` - The first statement (a body is never empty)
    /// * `rest` - The statements after it
    pub(crate) fn new(first: Statement, rest: impl IntoIterator<Item = Statement>) -> Self {
        Self(std::iter::once(first).chain(rest).collect())
    }

    /// The body's shell text: `s1 </dev/null && s2 </dev/null …`.
    pub(crate) fn render(&self) -> String {
        let commands: Vec<String> = self
            .0
            .iter()
            .map(|statement| format!("{} </dev/null", statement.render()))
            .collect();
        commands.join(" && ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `stat -- <path>` as a statement for `identity`.
    ///
    /// # Arguments
    ///
    /// * `identity` - Who runs it
    /// * `path` - A path for the argument
    fn stat(identity: &Identity, path: &str) -> Statement {
        Statement::as_identity(identity, "stat", vec![Word::lit("--"), Word::checked(path)])
    }

    #[test]
    fn statements_render_prefix_program_and_quoted_words() {
        let login = stat(&Identity::Login, "/a b");
        assert_eq!(login.render(), "stat -- '/a b'");
        let sudo = Identity::sudo("www-data", Some("web")).unwrap();
        assert_eq!(
            stat(&sudo, "/x'y").render(),
            r"sudo -n -u 'www-data' -g 'web' -- stat -- '/x'\''y'"
        );
        let root = Statement::as_root(RootAccess::Sudo, "stat", vec![Word::lit("-c")]);
        assert_eq!(root.render(), "sudo -n -u root -- stat -c");
        let root_login = Statement::as_root(RootAccess::RootLogin, "id", Vec::new());
        assert_eq!(root_login.render(), "id");
    }

    #[test]
    fn bodies_chain_statements_with_stdin_from_dev_null() {
        let one = Body::new(stat(&Identity::Login, "/a"), []);
        assert_eq!(one.render(), "stat -- '/a' </dev/null");
        let three = Body::new(
            stat(&Identity::Login, "/a"),
            [
                stat(&Identity::Login, "/b"),
                Statement::as_root(RootAccess::Sudo, "id", Vec::new()),
            ],
        );
        assert_eq!(
            three.render(),
            "stat -- '/a' </dev/null && stat -- '/b' </dev/null && \
             sudo -n -u root -- id </dev/null"
        );
    }
}
