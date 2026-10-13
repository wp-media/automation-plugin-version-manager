//! Allowlists for the values apvm-sites accepts (plan §7).
//!
//! Each rule is a pure function returning `Ok(())` or the reason the value
//! is refused, phrased to follow its location (`path: must be an absolute
//! path`). Values that later reach the `ssh` argv, a shell command ssh
//! builds, or a remote command are limited to what stays literal and
//! unambiguous there. Free text only refuses characters that could drive or
//! disguise a terminal. Living in the library, the rules protect every front
//! end, Node callers included (plan §10, T1).
//!
//! - this module: names, ids, numbers and free text;
//! - [`net`]: site URLs and SSH jump hosts;
//! - [`paths`]: remote and local paths, the WP-CLI command and its
//!   arguments, upload-script values, variant markers.

mod net;
mod paths;

pub(crate) use net::{proxy_jump, url};
/// Only the tests outside `paths` call it; the library reaches it through
/// [`remote_tmp_dir`].
#[cfg(test)]
pub(crate) use paths::upload_value;
pub(crate) use paths::{
    known_hosts_file, marker_path, needle, remote_path, remote_tmp_dir, ssh_path, wp_cli,
    wp_cli_arg,
};

/// A rule's verdict: `Err` holds the reason the value is refused.
pub(crate) type Rule = Result<(), String>;

/// Why a forbidden character is refused.
const FORBIDDEN_CHARS: &str = "control and invisible formatting characters are not allowed";

/// A token of one character class, with a stricter first character.
struct Word {
    /// Longest accepted value, in bytes (= characters: the classes are ASCII).
    max_len: usize,
    /// Whether a character may come first.
    first: fn(char) -> bool,
    /// The first-character class, for messages.
    first_desc: &'static str,
    /// Whether a character may appear anywhere.
    any: fn(char) -> bool,
    /// The character class, for messages.
    any_desc: &'static str,
}

impl Word {
    /// Check `value` against this word's classes and length.
    ///
    /// # Arguments
    ///
    /// * `value` - The value to check
    fn check(&self, value: &str) -> Rule {
        let Some(first) = value.chars().next() else {
            return Err("must not be empty".into());
        };
        if let Some(bad) = value.chars().find(|c| !(self.any)(*c)) {
            return Err(format!(
                "contains {}; only {} are allowed",
                describe(bad),
                self.any_desc
            ));
        }
        if !(self.first)(first) {
            return Err(format!("must start with {}", self.first_desc));
        }
        if value.len() > self.max_len {
            return Err(format!("must be at most {} characters", self.max_len));
        }
        Ok(())
    }
}

/// Inventory ids, project and variant names: `^[a-z0-9][a-z0-9._-]{0,63}$`.
const ID: Word = Word {
    max_len: 64,
    first: |c| c.is_ascii_lowercase() || c.is_ascii_digit(),
    first_desc: "a lowercase letter or digit",
    any: |c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'),
    any_desc: "lowercase letters, digits, '.', '_' and '-'",
};

/// Plugin slugs: `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$`.
const SLUG: Word = Word {
    max_len: 100,
    first: |c| c.is_ascii_alphanumeric(),
    first_desc: "a letter or digit",
    any: is_name_char,
    any_desc: "letters, digits, '.', '_' and '-'",
};

/// SSH hosts: `[A-Za-z0-9._:-]`, no leading `-` (it would read as an ssh
/// option), at most 253 characters (a DNS name).
const HOST: Word = Word {
    max_len: 253,
    first: |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':'),
    first_desc: "a letter, digit, '.', '_' or ':'",
    any: |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'),
    any_desc: "letters, digits, '.', '_', ':' and '-'",
};

/// Unix user and group names: `[A-Za-z0-9._-]`, no leading `-` (it would
/// read as an option of ssh or sudo), at most 64 characters. Messages say
/// "ASCII letters": a name like `İlker` is refused, and "letters" alone
/// would read as allowing it.
const USER: Word = Word {
    max_len: 64,
    first: |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'),
    first_desc: "an ASCII letter, digit, '.' or '_'",
    any: is_name_char,
    any_desc: "ASCII letters, digits, '.', '_' and '-'",
};

/// `[A-Za-z0-9._-]`: file and user names.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Whether `c` is refused everywhere, free text included: a control
/// character (C0, DEL, C1) or an invisible formatting character, which can
/// drive, reorder or hide text in a terminal ("Trojan Source", tag
/// characters, zero-width spaces).
pub(crate) fn is_forbidden(c: char) -> bool {
    c.is_control() || is_format_or_separator(c)
}

/// Whether `c` is in Unicode general category Cf (format), Zl (line
/// separator) or Zp (paragraph separator), per Unicode 16.0 (the ranges
/// were generated from its UnicodeData; std has no category lookup).
fn is_format_or_separator(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x2028..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// Name a refused character in a message: printable ones quoted, the others
/// (whitespace, controls, formatting) as `U+XXXX`.
fn describe(c: char) -> String {
    if is_forbidden(c) || c.is_whitespace() {
        format!("U+{:04X}", u32::from(c))
    } else {
        format!("{c:?}")
    }
}

/// An inventory id, project name or variant name.
pub(crate) fn id(value: &str) -> Rule {
    ID.check(value)
}

/// A plugin slug (WP-CLI's `name`).
pub(crate) fn slug(value: &str) -> Rule {
    SLUG.check(value)
}

/// An SSH host: a name, an address or an ssh_config alias.
pub(crate) fn host(value: &str) -> Rule {
    HOST.check(value)
}

/// A Unix user or group name. Names of digits only (`chown`, `find` and
/// `id` read them as numeric ids, so `0` means root) and `.` / `..` are
/// refused, as shadow-utils refuses them.
pub(crate) fn user(value: &str) -> Rule {
    USER.check(value)?;
    if value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("must not be all digits (tools read it as a numeric id)".into());
    }
    if matches!(value, "." | "..") {
        return Err("must not be '.' or '..'".into());
    }
    Ok(())
}

/// Whether a user or group name means root: `root` in any letter case
/// (macOS resolves `ROOT` to uid 0). Other aliases of uid 0 are caught where
/// ids are resolved (plan §6.2).
pub(crate) fn is_root(name: &str) -> bool {
    name.eq_ignore_ascii_case("root")
}

/// A TCP port: 1–65535.
pub(crate) fn port(value: u16) -> Rule {
    if value == 0 {
        return Err("must be between 1 and 65535".into());
    }
    Ok(())
}

/// A duration in whole seconds: 1–`max`.
///
/// # Arguments
///
/// * `value` - The duration
/// * `max` - The longest duration accepted
pub(crate) fn seconds(value: u32, max: u32) -> Rule {
    if !(1..=max).contains(&value) {
        return Err(format!("must be between 1 and {max} seconds"));
    }
    Ok(())
}

/// Free text (names, notes): non-empty, at most `max_chars` characters, no
/// control or invisible formatting characters (a shared inventory must not
/// be able to drive or disguise the terminal that lists it).
///
/// # Arguments
///
/// * `value` - The text
/// * `max_chars` - The most characters accepted
pub(crate) fn text(value: &str, max_chars: usize) -> Rule {
    if value.is_empty() {
        return Err("must not be empty (omit it instead)".into());
    }
    if let Some(bad) = value.chars().find(|c| is_forbidden(*c)) {
        return Err(format!("contains {}; {FORBIDDEN_CHARS}", describe(bad)));
    }
    if value.chars().count() > max_chars {
        return Err(format!("must be at most {max_chars} characters"));
    }
    Ok(())
}

/// Assert that `rule` accepts every value in `ok`, and refuses every value in
/// `bad` with a reason containing the paired text.
#[cfg(test)]
fn table(rule: impl Fn(&str) -> Rule, ok: &[&str], bad: &[(&str, &str)]) {
    for value in ok {
        assert_eq!(rule(value), Ok(()), "expected {value:?} to be accepted");
    }
    for (value, reason) in bad {
        match rule(value) {
            Ok(()) => panic!("expected {value:?} to be refused"),
            Err(found) => assert!(
                found.contains(reason),
                "{value:?}: expected a reason containing {reason:?}, got {found:?}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = "must start with";
    const EMPTY: &str = "must not be empty";

    #[test]
    fn ids() {
        let longest = "a".repeat(64);
        let too_long = "a".repeat(65);
        table(
            id,
            &["a", "0", "eu-1", "shop.prod", "a_b", "0-a", &longest],
            &[
                ("", EMPTY),
                ("-a", START),
                (".a", START),
                ("_a", START),
                ("A", "contains 'A'"),
                ("eu 1", "contains U+0020"),
                ("eu/1", "contains '/'"),
                ("é", "contains 'é'"),
                ("a\u{1b}", "contains U+001B"),
                (&too_long, "at most 64"),
            ],
        );
    }

    #[test]
    fn slugs() {
        let longest = "A".repeat(100);
        let too_long = "a".repeat(101);
        table(
            slug,
            &[
                "backwpup",
                "backwpup-pro",
                "Akismet",
                "hello",
                "a..b",
                &longest,
            ],
            &[
                ("", EMPTY),
                ("-x", START),
                (".x", START),
                ("..", START),
                ("a/b", "contains '/'"),
                ("a b", "contains U+0020"),
                ("a\\b", "contains '\\\\'"),
                ("a'b", "contains '\\''"),
                (&too_long, "at most 100"),
            ],
        );
    }

    #[test]
    fn hosts() {
        let longest = "a".repeat(253);
        let too_long = "a".repeat(254);
        table(
            host,
            &[
                "203.0.113.10",
                "example.com",
                "staging",
                "::1",
                "_svc.local",
                &longest,
            ],
            &[
                ("", EMPTY),
                ("-oProxyCommand=x", "contains '='"),
                ("-example.com", START),
                ("a b", "contains U+0020"),
                ("a@b", "contains '@'"),
                ("[::1]", "contains '['"),
                ("a%h", "contains '%'"),
                (&too_long, "at most 253"),
            ],
        );
    }

    #[test]
    fn users() {
        let longest = "u".repeat(64);
        let too_long = "u".repeat(65);
        table(
            user,
            &[
                "deploy", "www-data", "ftp.user", "_svc", "u1", "1u", "Admin", "...", &longest,
            ],
            &[
                ("", EMPTY),
                ("-x", START),
                ("a b", "contains U+0020"),
                ("a@b", "contains '@'"),
                ("a:b", "contains ':'"),
                ("#0", "contains '#'"),
                ("0", "all digits"),
                ("1000", "all digits"),
                (".", "'.' or '..'"),
                ("..", "'.' or '..'"),
                (&too_long, "at most 64"),
            ],
        );
    }

    #[test]
    fn root_in_any_case() {
        for name in ["root", "ROOT", "Root", "rOoT"] {
            assert!(is_root(name), "{name}");
        }
        for name in ["toor", "roots", "_root", "admin"] {
            assert!(!is_root(name), "{name}");
        }
    }

    #[test]
    fn ports_and_seconds() {
        assert!(port(0).is_err());
        assert_eq!(port(1), Ok(()));
        assert_eq!(port(65535), Ok(()));
        assert_eq!(
            seconds(0, 60),
            Err("must be between 1 and 60 seconds".into())
        );
        assert_eq!(seconds(1, 60), Ok(()));
        assert_eq!(seconds(60, 60), Ok(()));
        assert_eq!(
            seconds(61, 60),
            Err("must be between 1 and 60 seconds".into())
        );
        assert!(seconds(u32::MAX, 60).is_err());
    }

    #[test]
    fn free_text() {
        let rule = |value: &str| text(value, 8);
        table(
            rule,
            &[
                "EU VPS",
                "ftp:www",
                "it's",
                "café 😀",
                "12345678",
                "éééééééé",
            ],
            &[
                ("", "omit it instead"),
                ("a\nb", "U+000A"),
                ("a\tb", "U+0009"),
                ("\u{1b}[2J", "U+001B"),
                ("\u{7f}", "U+007F"),
                ("\u{85}", "U+0085"),
                ("\u{202e}abc", "U+202E"),
                ("a\u{2066}b", "U+2066"),
                ("a\u{200b}b", "U+200B"),
                ("a\u{2028}b", "U+2028"),
                ("a\u{feff}", "U+FEFF"),
                ("Shop\u{e0049}", "U+E0049"),
                ("soft\u{ad}", "U+00AD"),
                ("123456789", "at most 8 characters"),
            ],
        );
    }

    #[test]
    fn forbidden_characters_cover_every_listed_range() {
        let controls = [
            '\0', '\n', '\r', '\t', '\u{1b}', '\u{7f}', '\u{85}', '\u{9f}',
        ];
        // Both ends of every Cf/Zl/Zp range of Unicode 16.0.
        let formats = [
            0x00AD, 0x0600, 0x0605, 0x061C, 0x06DD, 0x070F, 0x0890, 0x0891, 0x08E2, 0x180E, 0x200B,
            0x200F, 0x2028, 0x202E, 0x2060, 0x2064, 0x2066, 0x206F, 0xFEFF, 0xFFF9, 0xFFFB,
            0x110BD, 0x110CD, 0x13430, 0x1343F, 0x1BCA0, 0x1BCA3, 0x1D173, 0x1D17A, 0xE0001,
            0xE0020, 0xE007F,
        ];
        for c in controls {
            assert!(is_forbidden(c), "{c:?}");
        }
        for code in formats {
            let c = char::from_u32(code).expect("a valid scalar value");
            assert!(is_forbidden(c), "U+{code:04X}");
        }
        // Neighbours of the ranges, and ordinary characters, stay allowed.
        let allowed = [
            0x20, 0x41, 0xE9, 0x00AC, 0x0606, 0x2027, 0x202F, 0x2065, 0x2070, 0xFEFE, 0xFFFC,
            0xE0002, 0xE0080, 0x1F600,
        ];
        for code in allowed {
            let c = char::from_u32(code).expect("a valid scalar value");
            assert!(!is_forbidden(c), "U+{code:04X}");
        }
    }

    #[test]
    fn messages_name_the_offending_character() {
        assert_eq!(
            id("Eu"),
            Err(
                "contains 'E'; only lowercase letters, digits, '.', '_' and '-' are allowed".into()
            )
        );
        assert_eq!(
            id("-eu"),
            Err("must start with a lowercase letter or digit".into())
        );
        assert_eq!(
            text("a\u{1b}", 10),
            Err(format!("contains U+001B; {FORBIDDEN_CHARS}"))
        );
        // The user message says ASCII letters (it was "letters", which
        // read as allowing `İ`); the rule itself is unchanged.
        assert_eq!(
            user("a'b"),
            Err("contains '\\''; only ASCII letters, digits, '.', '_' and '-' are allowed".into())
        );
        assert_eq!(
            user("a\u{a0}b"),
            Err("contains U+00A0; only ASCII letters, digits, '.', '_' and '-' are allowed".into())
        );
        assert_eq!(
            user("\u{130}lker"),
            Err("contains 'İ'; only ASCII letters, digits, '.', '_' and '-' are allowed".into())
        );
    }
}
