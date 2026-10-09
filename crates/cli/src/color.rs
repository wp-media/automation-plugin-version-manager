//! Minimal ANSI coloring for CLI output.
//!
//! Coloring is suppressed when `NO_COLOR` is set (any value, per
//! <https://no-color.org>) or when stdout is not a terminal (piped/redirected),
//! so machine-readable output never contains escape codes.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether colored output should be emitted. Computed once from the
/// environment and the stdout TTY state.
fn colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        should_color(
            std::env::var_os("NO_COLOR").is_some(),
            std::io::stdout().is_terminal(),
        )
    })
}

/// The coloring policy, separated from the environment for unit testing:
/// color only on a terminal, and never when `NO_COLOR` is set.
fn should_color(no_color_set: bool, stdout_is_tty: bool) -> bool {
    !no_color_set && stdout_is_tty
}

/// A small palette of SGR colors used by the CLI.
#[derive(Clone, Copy)]
pub enum Color {
    /// Freshly built artifacts.
    Green,
    /// Cache-reused artifacts.
    Cyan,
    /// Downloaded release assets.
    Blue,
}

impl Color {
    /// The SGR foreground code for this color.
    fn code(self) -> &'static str {
        match self {
            Color::Green => "32",
            Color::Cyan => "36",
            Color::Blue => "34",
        }
    }
}

/// Wrap `text` in the given color when coloring is enabled; otherwise return
/// it unchanged (so the label stays readable in pipes and `NO_COLOR` mode).
pub fn paint(text: &str, color: Color) -> String {
    paint_if(text, color, colors_enabled())
}

/// [`paint`] with an explicit on/off switch (separated for unit testing).
fn paint_if(text: &str, color: Color, enabled: bool) -> String {
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
    fn color_codes_are_stable() {
        // Each provenance keeps its own color; swapping two would silently
        // mislabel built vs. cached vs. downloaded artifacts.
        assert_eq!(Color::Green.code(), "32");
        assert_eq!(Color::Cyan.code(), "36");
        assert_eq!(Color::Blue.code(), "34");
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
