//! Styled status output on stderr, gated by a global verbosity level.
//!
//! Every message is escaped before it is styled, so a file name or a command from the repo cannot
//! move the cursor, retitle the terminal or overwrite a line already printed.

mod progress;

use colored::Colorize;
use std::borrow::Cow;
use std::sync::atomic::{AtomicU8, Ordering};

pub use progress::Progress;

/// Verbosity level for output messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    /// Suppress informational messages, show only warnings and errors.
    Quiet = 0,
    /// Default verbosity level, show all standard messages.
    Normal = 1,
    /// Show verbose debug messages in addition to standard output.
    Verbose = 2,
}

static VERBOSITY: AtomicU8 = AtomicU8::new(1);

/// Sets the global verbosity level for all output functions.
pub fn set_verbosity(level: Verbosity) {
    VERBOSITY.store(level as u8, Ordering::Relaxed);
}

/// Gets the current global verbosity level.
pub fn get_verbosity() -> Verbosity {
    match VERBOSITY.load(Ordering::Relaxed) {
        0 => Verbosity::Quiet,
        2 => Verbosity::Verbose,
        _ => Verbosity::Normal,
    }
}

/// `text` with control characters other than newline and tab, and hidden format characters, escaped.
///
/// A control character (C0, DEL and C1) is written as `char::escape_debug` writes it, such as
/// `\u{1b}` or `\r`, and an [`is_hidden_format`] character as `\u{202e}`. Everything else is left
/// alone.
#[must_use]
pub fn escape(text: &str) -> Cow<'_, str> {
    escape_where(text, |c| {
        (c.is_control() && c != '\n' && c != '\t') || is_hidden_format(c)
    })
}

/// [`escape`] that also escapes newline and tab, for a field that must stay on one line, such
/// as a key or a hook command.
#[must_use]
pub fn escape_line(text: &str) -> Cow<'_, str> {
    escape_where(text, |c| c.is_control() || is_hidden_format(c))
}

/// Whether `c` changes how the text around it reads without showing itself.
///
/// Such a character makes a line read as other text than it holds: a bidi control (U+061C, U+200E,
/// U+200F, U+202A to U+202E, U+2066 to U+2069), a line or paragraph separator, or an invisible
/// format character (soft hyphen, Mongolian vowel separator, zero-width space, word joiner,
/// invisible operators, deprecated format controls, byte order mark, interlinear annotation marks,
/// tags). The zero-width joiner and non-joiner are not: Persian and Indic names and emoji need
/// them, and they reorder nothing.
#[must_use]
pub const fn is_hidden_format(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{61c}'
            | '\u{180e}'
            | '\u{200b}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

fn escape_where(text: &str, unsafe_char: impl Fn(char) -> bool) -> Cow<'_, str> {
    if !text.chars().any(&unsafe_char) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if !unsafe_char(c) {
            out.push(c);
        } else if c.is_control() {
            out.extend(c.escape_debug());
        } else {
            out.extend(c.escape_unicode());
        }
    }
    Cow::Owned(out)
}

/// Prints a success message in green (respects quiet mode).
pub fn success(message: &str) {
    if get_verbosity() == Verbosity::Quiet {
        return;
    }
    eprintln!("{}", escape(message).green());
}

/// Prints an error message in bold red (always shown).
pub fn error(message: &str) {
    eprintln!("{}", escape(message).red().bold());
}

/// Prints a warning message in bold yellow (always shown).
pub fn warning(message: &str) {
    eprintln!("{}", escape(message).yellow().bold());
}

/// Prints an informational message in dimmed color (respects quiet mode).
pub fn info(message: &str) {
    if get_verbosity() == Verbosity::Quiet {
        return;
    }
    eprintln!("{}", escape(message).dimmed());
}

/// Prints what a question is about, in dimmed color. Always shown: `-q` must not hide what an
/// answer approves.
pub fn context(message: &str) {
    eprintln!("{}", escape(message).dimmed());
}

/// Prints a verbose debug message (only in verbose mode).
pub fn verbose(message: &str) {
    if get_verbosity() != Verbosity::Verbose {
        return;
    }
    eprintln!("{}", escape(message).dimmed());
}

/// Prints a git-style action message with dimmed verb and normal message, each kept on one line.
pub fn action(verb: &str, message: &str) {
    if get_verbosity() == Verbosity::Quiet {
        return;
    }
    eprintln!(
        "{} {}",
        escape_line(verb).dimmed().bold(),
        escape_line(message)
    );
}

/// Starts a new progress bar for tracking long operations.
#[must_use]
pub fn start_progress(title: &str, total: usize) -> Progress {
    Progress::new(title, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The verbosity is a process global, so only one test may set it; parallel tests would race.
    #[test]
    fn test_verbosity_round_trip() {
        let levels = [Verbosity::Quiet, Verbosity::Normal, Verbosity::Verbose];
        for level in &levels {
            set_verbosity(*level);
            assert_eq!(get_verbosity(), *level);
        }
    }

    #[test]
    fn control_characters_are_escaped_and_the_rest_is_kept() {
        assert!(matches!(escape("a/b c\t\"q\" \\n\né"), Cow::Borrowed(_)));
        assert_eq!(
            escape("d/\u{1b}]0;TITLE\u{7}\u{1b}[31mred\u{1b}[0m"),
            "d/\\u{1b}]0;TITLE\\u{7}\\u{1b}[31mred\\u{1b}[0m"
        );
        assert_eq!(escape("x #\r\u{1b}[2K y"), "x #\\r\\u{1b}[2K y");
        assert_eq!(escape("\0\u{7f}\u{9b}\u{85}"), "\\0\\u{7f}\\u{9b}\\u{85}");
        assert_eq!(escape("line\nnext\ttab"), "line\nnext\ttab");
        let once = escape("\u{1b}[2J").into_owned();
        assert_eq!(escape(&once), once);
    }

    #[test]
    fn bidi_controls_and_invisible_format_characters_are_escaped_and_joiners_are_not() {
        let hidden = "\u{ad}\u{61c}\u{180e}\u{200b}\u{200e}\u{200f}\u{2028}\u{2029}\u{202a}\u{202e}\u{2060}\u{2064}\u{2066}\u{2069}\u{206f}\u{feff}\u{fff9}\u{fffb}\u{e0001}\u{e0020}\u{e007f}";
        let shown = "\\u{ad}\\u{61c}\\u{180e}\\u{200b}\\u{200e}\\u{200f}\\u{2028}\\u{2029}\\u{202a}\\u{202e}\\u{2060}\\u{2064}\\u{2066}\\u{2069}\\u{206f}\\u{feff}\\u{fff9}\\u{fffb}\\u{e0001}\\u{e0020}\\u{e007f}";
        assert_eq!(escape(hidden), shown);
        assert_eq!(escape_line(hidden), shown);
        assert_eq!(
            escape("rm #\u{202e}\u{2066} ;)x(\u{2069}\n"),
            "rm #\\u{202e}\\u{2066} ;)x(\\u{2069}\n"
        );
        for kept in ["\u{200c}", "\u{200d}", "\u{2065}", "\u{e0000}", "\u{e0080}"] {
            assert!(matches!(escape_line(kept), Cow::Borrowed(_)), "{kept:?}");
        }
        assert_eq!(escape(shown), shown);
    }

    #[test]
    fn a_one_line_field_also_escapes_newline_and_tab() {
        assert_eq!(escape_line("a\nb\tc\rd\u{1b}e"), "a\\nb\\tc\\rd\\u{1b}e");
        assert!(matches!(escape_line("a/b \"c\" é"), Cow::Borrowed(_)));
        let once = escape_line("x\ny").into_owned();
        assert_eq!(escape(&once), once);
    }
}
