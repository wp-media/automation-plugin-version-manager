//! Status lines and inline styles for human-facing messages on stderr.
//!
//! `skill`, `update` and `uninstall` report progress on stderr (stdout stays
//! for data). Styling follows [`crate::color`] for stderr: plain when
//! `NO_COLOR` is set or stderr is not a terminal (pipes, CI logs).

use crate::color::{self, Color, Stream};

/// Kind of status line; decides its prefix and color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `  ✓  message` — a step that completed.
    Success,
    /// `info  message`
    Info,
    /// `warn  message`
    Warn,
    /// `error message`
    Error,
}

/// Render one status line, without the trailing newline. The `info`, `warn`
/// and `error` prefixes are padded so their messages line up; a success line
/// is an indented check mark.
///
/// # Arguments
///
/// * `kind` - Which prefix to use
/// * `msg` - The message text (never styled)
/// * `styled` - Whether to color the prefix
fn line(kind: Kind, msg: &str, styled: bool) -> String {
    let paint = |text: &str, color: Color| color::paint_if(text, color, styled);
    match kind {
        Kind::Success => format!("  {}  {msg}", paint("✓", Color::Green)),
        Kind::Info => format!("{}  {msg}", paint("info", Color::Blue)),
        Kind::Warn => format!("{}  {msg}", paint("warn", Color::Yellow)),
        Kind::Error => format!("{} {msg}", paint("error", Color::Red)),
    }
}

/// Print a status line of `kind` to stderr.
fn emit(kind: Kind, msg: &str) {
    eprintln!("{}", line(kind, msg, color::colors_enabled(Stream::Stderr)));
}

/// Print a success line (`  ✓  msg`) to stderr.
pub fn success(msg: &str) {
    emit(Kind::Success, msg);
}

/// Print an informational line (`info  msg`) to stderr.
pub fn info(msg: &str) {
    emit(Kind::Info, msg);
}

/// Print a warning line (`warn  msg`) to stderr.
pub fn warn(msg: &str) {
    emit(Kind::Warn, msg);
}

/// Print an error line (`error msg`) to stderr.
pub fn error(msg: &str) {
    emit(Kind::Error, msg);
}

/// `text` dimmed, for inline use in a stderr message.
pub fn dim(text: &str) -> String {
    color::paint_stderr(text, Color::Dim)
}

/// `text` in bold, for inline use in a stderr message.
pub fn bold(text: &str) -> String {
    color::paint_stderr(text, Color::Bold)
}

/// `text` in yellow, for inline use in a stderr message (e.g. a prompt).
pub fn yellow(text: &str) -> String {
    color::paint_stderr(text, Color::Yellow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_keep_their_prefixes_aligned() {
        // Without styling, the text is exactly what scripts and logs see.
        assert_eq!(line(Kind::Success, "done", false), "  ✓  done");
        assert_eq!(line(Kind::Info, "note", false), "info  note");
        assert_eq!(line(Kind::Warn, "careful", false), "warn  careful");
        assert_eq!(line(Kind::Error, "failed", false), "error failed");
        // The word prefixes line up: their messages start at column 6.
        for kind in [Kind::Info, Kind::Warn, Kind::Error] {
            let rendered = line(kind, "X", false);
            assert_eq!(rendered.chars().position(|c| c == 'X'), Some(6), "{kind:?}");
        }
    }

    #[test]
    fn styled_lines_color_only_the_prefix() {
        assert_eq!(
            line(Kind::Success, "done", true),
            "  \x1b[32m✓\x1b[0m  done"
        );
        assert_eq!(line(Kind::Info, "note", true), "\x1b[34minfo\x1b[0m  note");
        assert_eq!(
            line(Kind::Warn, "careful", true),
            "\x1b[33mwarn\x1b[0m  careful"
        );
        assert_eq!(
            line(Kind::Error, "failed", true),
            "\x1b[31merror\x1b[0m failed"
        );
    }

    #[test]
    fn plain_lines_never_contain_escape_codes() {
        for kind in [Kind::Success, Kind::Info, Kind::Warn, Kind::Error] {
            assert!(!line(kind, "msg", false).contains('\x1b'), "{kind:?}");
        }
    }
}
