//! POSIX shell quoting, and the words remote commands are made of.

use std::borrow::Cow;

use super::invalid;
use crate::error::Result;

/// Quote `value` as one POSIX shell word: wrapped in single quotes, with
/// each `'` written as `'\''` (close the quotes, an escaped quote, reopen).
///
/// Inside single quotes a POSIX shell takes every character literally, so
/// the command receives `value` byte for byte: newlines, `$`, backticks,
/// globs, backslashes, a leading `-` and any Unicode included. The empty
/// string becomes `''` (one empty argument). The result is meant for a
/// script that POSIX `sh` parses ([`Script`](super::Script)); a login shell
/// such as fish reads `\'` inside quotes differently, which is why
/// [`UploadScript`](super::UploadScript) only takes values that need no
/// escaping.
///
/// # Arguments
///
/// * `value` - The text the command must receive
///
/// # Returns
///
/// The quoted word.
///
/// # Errors
///
/// [`Error::InvalidInput`](crate::Error::InvalidInput) when `value` holds
/// NUL: no command argument can carry it.
///
/// # Example
///
/// ```
/// use apvm_sites::remote::quote;
///
/// assert_eq!(quote("it's $HOME").unwrap(), r"'it'\''s $HOME'");
/// assert_eq!(quote("").unwrap(), "''");
/// assert!(quote("a\0b").is_err());
/// ```
pub fn quote(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(invalid(
            "shell value",
            "value",
            "contains U+0000 (NUL), which no command argument can hold",
        ));
    }
    Ok(quote_unchecked(value))
}

/// [`quote()`] for a value already known to hold no NUL.
///
/// # Arguments
///
/// * `value` - Text without NUL
fn quote_unchecked(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    out.push_str(&value.replace('\'', r"'\''"));
    out.push('\'');
    out
}

/// One word of a command line, already in shell syntax: a literal of
/// apvm's templates, written as is, or a quoted value. Only this module
/// writes unquoted text, and only from `'static` literals and numbers, so
/// data from callers can never become shell syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word(Cow<'static, str>);

impl Word {
    /// A literal of apvm's own templates, written as is (such as `sudo`,
    /// `-n` or `--skip-packages`).
    ///
    /// # Arguments
    ///
    /// * `text` - Shell text that needs no quoting
    pub(crate) const fn lit(text: &'static str) -> Self {
        Self(Cow::Borrowed(text))
    }

    /// A value its constructor has already checked, quoted. Every check
    /// refuses NUL (most rules refuse every control character; WP-CLI
    /// arguments refuse only NUL and may hold newlines), so quoting cannot
    /// fail.
    ///
    /// # Arguments
    ///
    /// * `value` - A checked value without NUL (an account name, a remote
    ///   path, a WP-CLI argument, …)
    pub(crate) fn checked(value: &str) -> Self {
        Self(Cow::Owned(quote_unchecked(value)))
    }

    /// An option and its checked value in one word, such as
    /// `--path='/var/www/a b'` (see [`Word::checked`]).
    ///
    /// # Arguments
    ///
    /// * `name` - The literal option text, with its `=`
    /// * `value` - The option's checked value
    pub(crate) fn option(name: &'static str, value: &str) -> Self {
        Self(Cow::Owned(format!("{name}{}", quote_unchecked(value))))
    }

    /// A literal prefix and a numeric `+uid:+gid` pair, such as
    /// `--from=+1001:+33`. GNU `chown` reads a bare `1001` as a user *name*
    /// first (a user named `1001` would win); the `+` makes it an id.
    /// Digits, `+` and `:` need no quoting.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Literal text before the pair (may be empty)
    /// * `uid` - The user id
    /// * `gid` - The group id
    pub(crate) fn ids(prefix: &'static str, uid: u32, gid: u32) -> Self {
        Self(Cow::Owned(format!("{prefix}+{uid}:+{gid}")))
    }

    /// The word's shell text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// `words` as a command line: their shell text, separated by spaces.
///
/// # Arguments
///
/// * `words` - The words, in order
pub(crate) fn join(words: &[Word]) -> String {
    let texts: Vec<&str> = words.iter().map(Word::as_str).collect();
    texts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;

    #[test]
    fn quoting_wraps_in_single_quotes_and_escapes_only_quotes() {
        let cases = [
            ("", "''"),
            ("wp", "'wp'"),
            ("it's", r"'it'\''s'"),
            ("'", r"''\'''"),
            ("''", r"''\'''\'''"),
            (r"a\b", r"'a\b'"),
            ("$(id) `id` ${X}", "'$(id) `id` ${X}'"),
            ("a\nb", "'a\nb'"),
            ("-n", "'-n'"),
            ("*?[a]~", "'*?[a]~'"),
            ("é 😀", "'é 😀'"),
        ];
        for (value, quoted) in cases {
            assert_eq!(quote(value).unwrap(), quoted, "{value:?}");
        }
    }

    #[test]
    fn nul_is_refused_without_echoing_the_value() {
        for value in ["\0", "a\0", "\0b", "secret\0value"] {
            let error = quote(value).unwrap_err();
            assert_eq!(error.code(), Code::InvalidInput);
            assert_eq!(
                error.to_string(),
                "invalid shell value: value: contains U+0000 (NUL), which no command \
                 argument can hold"
            );
        }
    }

    #[test]
    fn words_render_literals_values_options_and_ids() {
        let words = [
            Word::lit("sudo"),
            Word::checked("www-data"),
            Word::option("--path=", "/srv/a'b"),
            Word::ids("--from=", 1001, 33),
            Word::ids("", 0, 0),
        ];
        assert_eq!(
            join(&words),
            r"sudo 'www-data' --path='/srv/a'\''b' --from=+1001:+33 +0:+0"
        );
        assert_eq!(join(&[]), "");
        assert_eq!(Word::ids("", u32::MAX, 1).as_str(), "+4294967295:+1");
    }
}
