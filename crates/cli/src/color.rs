//! Minimal ANSI coloring for CLI output.
//!
//! Coloring is suppressed when `NO_COLOR` is set to a non-empty value (per
//! <https://no-color.org>) or when the stream the text is written to is not a
//! terminal (piped/redirected). Stdout and stderr are checked separately, so
//! `apvm … 2>log` keeps colors on the terminal while the log stays plain, and
//! apvm's output never puts escape codes into a pipe. (`RUST_LOG`
//! diagnostics are formatted by `tracing-subscriber`; `main` hands it the
//! stderr decision from here.)

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::sync::OnceLock;

/// An output stream. Each has its own terminal state, so styling is decided
/// per stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output (data, build summaries).
    Stdout,
    /// Standard error (status lines, prompts, warnings).
    Stderr,
}

/// Whether the user opted out of color: `NO_COLOR` is present and not empty.
pub fn no_color_requested() -> bool {
    no_color_value_opts_out(std::env::var_os("NO_COLOR").as_deref())
}

/// The `NO_COLOR` rule on an already-read value (pure, for unit testing): any
/// non-empty value opts out; an unset or empty variable does not.
fn no_color_value_opts_out(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// Whether text written to `stream` should be colored. Computed once per
/// stream from `NO_COLOR` and that stream's TTY state.
pub fn colors_enabled(stream: Stream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();
    match stream {
        Stream::Stdout => *STDOUT
            .get_or_init(|| should_color(no_color_requested(), std::io::stdout().is_terminal())),
        Stream::Stderr => *STDERR
            .get_or_init(|| should_color(no_color_requested(), std::io::stderr().is_terminal())),
    }
}

/// The coloring policy, separated from the environment for unit testing:
/// color only on a terminal, and never when `NO_COLOR` opts out.
fn should_color(no_color_set: bool, stream_is_tty: bool) -> bool {
    !no_color_set && stream_is_tty
}

/// A small palette of SGR colors and text attributes used by the CLI.
#[derive(Clone, Copy, Debug)]
pub enum Color {
    /// Freshly built artifacts; success marks.
    Green,
    /// Cache-reused artifacts.
    Cyan,
    /// Downloaded release assets; `info` prefixes.
    Blue,
    /// `warn` prefixes and confirmation prompts.
    Yellow,
    /// `error` prefixes.
    Red,
    /// Emphasis.
    Bold,
    /// De-emphasized detail.
    Dim,
}

impl Color {
    /// The SGR code for this color or attribute.
    fn code(self) -> &'static str {
        match self {
            Color::Green => "32",
            Color::Cyan => "36",
            Color::Blue => "34",
            Color::Yellow => "33",
            Color::Red => "31",
            Color::Bold => "1",
            Color::Dim => "2",
        }
    }
}

/// Wrap `text` for stdout in the given color when coloring is enabled;
/// otherwise return it unchanged (so the label stays readable in pipes and
/// `NO_COLOR` mode).
pub fn paint(text: &str, color: Color) -> String {
    paint_if(text, color, colors_enabled(Stream::Stdout))
}

/// [`paint`] for text written to stderr, which has its own TTY state.
pub fn paint_stderr(text: &str, color: Color) -> String {
    paint_if(text, color, colors_enabled(Stream::Stderr))
}

/// [`paint`] with an explicit on/off switch (separated for unit testing).
pub fn paint_if(text: &str, color: Color, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{}m{}\x1b[0m", color.code(), text)
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_color_only_on_a_tty_without_no_color() {
        assert!(should_color(false, true));
        assert!(!should_color(true, true), "NO_COLOR must win over a TTY");
        assert!(!should_color(false, false), "pipes must stay escape-free");
        assert!(!should_color(true, false));
    }

    #[test]
    fn no_color_opts_out_only_when_set_and_non_empty() {
        // https://no-color.org: "when present and not an empty string".
        assert!(no_color_value_opts_out(Some(OsStr::new("1"))));
        assert!(no_color_value_opts_out(Some(OsStr::new("false"))));
        assert!(!no_color_value_opts_out(Some(OsStr::new(""))));
        assert!(!no_color_value_opts_out(None));
    }

    #[test]
    fn color_codes_are_stable() {
        // Each provenance keeps its own color; swapping two would silently
        // mislabel built vs. cached vs. downloaded artifacts.
        assert_eq!(Color::Green.code(), "32");
        assert_eq!(Color::Cyan.code(), "36");
        assert_eq!(Color::Blue.code(), "34");
    }

    #[test]
    fn status_and_attribute_codes_are_stable() {
        assert_eq!(Color::Yellow.code(), "33");
        assert_eq!(Color::Red.code(), "31");
        assert_eq!(Color::Bold.code(), "1");
        assert_eq!(Color::Dim.code(), "2");
    }

    #[test]
    fn paint_if_enabled_wraps_and_resets() {
        assert_eq!(
            paint_if("built", Color::Green, true),
            "\x1b[32mbuilt\x1b[0m"
        );
    }

    #[test]
    fn paint_if_disabled_returns_text_unchanged() {
        assert_eq!(paint_if("cache  ", Color::Cyan, false), "cache  ");
    }
}
