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
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
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
    /// Attention/ambiguity (e.g. a cache version mismatch).
    Yellow,
}

impl Color {
    fn code(self) -> &'static str {
        match self {
            Color::Green => "32",
            Color::Cyan => "36",
            Color::Blue => "34",
            Color::Yellow => "33",
        }
    }
}

/// Wrap `text` in the given color when coloring is enabled; otherwise return
/// it unchanged (so the label stays readable in pipes and `NO_COLOR` mode).
pub fn paint(text: &str, color: Color) -> String {
    if colors_enabled() {
        format!("\x1b[{}m{}\x1b[0m", color.code(), text)
    } else {
        text.to_string()
    }
}
