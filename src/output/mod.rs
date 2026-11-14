//! Styled status output on stderr, gated by a global verbosity level.
//!
//! Every message is escaped before it is styled, so a file name or a command from the repo cannot
//! move the cursor, retitle the terminal or overwrite a line already printed. On a terminal at
//! least 40 columns wide, a long message is also wrapped at spaces outside backticks.

mod progress;

use colored::Colorize;
use std::borrow::Cow;
use std::fmt::Display;
use std::io::{self, IsTerminal};
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

/// `n` with `noun` pluralised by an `s`: `1 file`, `2 files`.
#[must_use]
pub fn count<N>(n: N, noun: &str) -> String
where
    N: Copy + PartialEq + From<u8> + Display,
{
    format!("{n} {noun}{}", plural(n, "", "s"))
}

/// `one` when `n` is 1, `many` otherwise, for a word that agrees with a count.
#[must_use]
pub fn plural<'a, N>(n: N, one: &'a str, many: &'a str) -> &'a str
where
    N: Copy + PartialEq + From<u8>,
{
    if n == N::from(1) { one } else { many }
}

/// Narrower terminals are left to wrap by themselves.
const MIN_WRAP_COLUMNS: usize = 40;

fn terminal_columns() -> Option<usize> {
    if !io::stderr().is_terminal() {
        return None;
    }
    let columns = usize::from(crate::sys::stderr_columns()?);
    (columns >= MIN_WRAP_COLUMNS).then_some(columns)
}

/// `question` broken between its double-spaced answers into rows that fit the terminal with room
/// for a typed answer on the last; one row when stderr is not a terminal wide enough to wrap for.
#[must_use]
pub fn menu_rows(question: &str) -> Vec<String> {
    terminal_columns().map_or_else(
        || vec![question.to_owned()],
        |columns| rows_within(question, columns.saturating_sub(2)),
    )
}

fn rows_within(text: &str, limit: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for piece in text.split("  ") {
        match rows.last_mut() {
            Some(row)
                if row
                    .chars()
                    .count()
                    .saturating_add(2)
                    .saturating_add(piece.chars().count())
                    <= limit =>
            {
                row.push_str("  ");
                row.push_str(piece);
            }
            _ => rows.push(piece.to_owned()),
        }
    }
    rows
}

/// `message` escaped, and wrapped to the terminal when stderr is one.
fn fit(message: &str) -> Cow<'_, str> {
    let escaped = escape(message);
    match terminal_columns() {
        Some(columns) => Cow::Owned(wrap(&escaped, columns)),
        None => escaped,
    }
}

/// Breaks each line of `text` that starts with a non-space and is longer than `columns` at spaces
/// outside backtick spans, so a pasted command never holds a newline. A continuation line is
/// indented by two spaces. An indented line is a list item or a bare command and is kept as it
/// is, and so is a token longer than a line, and a line with an odd number of backticks, whose
/// spans cannot be told. Wide characters count as one column.
fn wrap(text: &str, columns: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let long = line.chars().nth(columns).is_some();
        let paired = line.matches('`').count() % 2 == 0;
        if long && paired && !line.starts_with(char::is_whitespace) {
            wrap_line(line, columns, &mut out);
        } else {
            out.push_str(line);
        }
    }
    out
}

fn wrap_line(line: &str, columns: usize, out: &mut String) {
    let mut width = 0usize;
    let mut on_line = false;
    for (i, token) in tokens(line).enumerate() {
        let token_width = token.chars().count();
        if i > 0 {
            let overflows = width.saturating_add(1).saturating_add(token_width) > columns;
            if on_line && !token.is_empty() && overflows {
                out.push_str("\n  ");
                width = 2;
                on_line = false;
            } else {
                out.push(' ');
                width = width.saturating_add(1);
            }
        }
        out.push_str(token);
        width = width.saturating_add(token_width);
        on_line = on_line || !token.is_empty();
    }
}

/// The pieces of `line` between spaces that lie outside a backtick span.
fn tokens(line: &str) -> impl Iterator<Item = &str> {
    let mut in_span = false;
    line.split(move |c| {
        if c == '`' {
            in_span = !in_span;
        }
        c == ' ' && !in_span
    })
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
    eprintln!("{}", fit(message).green());
}

/// Prints an error message in bold red (always shown).
pub fn error(message: &str) {
    eprintln!("{}", fit(message).red().bold());
}

/// Prints a warning message in bold yellow (always shown).
pub fn warning(message: &str) {
    eprintln!("{}", fit(message).yellow().bold());
}

/// Prints an informational message in dimmed color (respects quiet mode).
pub fn info(message: &str) {
    if get_verbosity() == Verbosity::Quiet {
        return;
    }
    eprintln!("{}", fit(message).dimmed());
}

/// Prints what a question is about, in dimmed color. Always shown: `-q` must not hide what an
/// answer approves.
pub fn context(message: &str) {
    eprintln!("{}", fit(message).dimmed());
}

/// Prints a verbose debug message (only in verbose mode).
pub fn verbose(message: &str) {
    if get_verbosity() != Verbosity::Verbose {
        return;
    }
    eprintln!("{}", fit(message).dimmed());
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

    #[test]
    fn a_count_agrees_with_its_noun() {
        assert_eq!(count(0_usize, "file"), "0 files");
        assert_eq!(count(1_usize, "file"), "1 file");
        assert_eq!(count(2_usize, "file"), "2 files");
        assert_eq!(count(0_u64, "file"), "0 files");
        assert_eq!(count(1_u64, "file"), "1 file");
        assert_eq!(count(2_u64, "file"), "2 files");
        assert_eq!(plural(1_usize, "entry", "entries"), "entry");
        assert_eq!(plural(0_u64, "needs", "need"), "need");
        assert_eq!(plural(2_u64, "needs", "need"), "need");
    }

    const LONG: &str = "next: run `dotman apply --dry-run` to see what would change, then `dotman apply` to write it, or `dotman status` to look again";

    #[test]
    fn a_long_line_breaks_at_spaces_outside_backticks_and_indents_the_rest() {
        assert_eq!(
            wrap(LONG, 60),
            "next: run `dotman apply --dry-run` to see what would change,\n  then `dotman apply` to write it, or `dotman status` to\n  look again"
        );
    }

    #[test]
    fn a_menu_breaks_between_answers_and_keeps_room_for_the_typed_one() {
        let menu = "[k] keep all local  [t] take all from repo  [d] show diffs  [p] decide per file  [s] skip:";
        assert_eq!(rows_within(menu, 200), [menu]);
        assert_eq!(
            rows_within(menu, 69),
            [
                "[k] keep all local  [t] take all from repo  [d] show diffs",
                "[p] decide per file  [s] skip:"
            ]
        );
        assert_eq!(rows_within("a  b", 1), ["a", "b"]);
        assert_eq!(rows_within("", 10), [""]);
    }

    #[test]
    fn a_short_line_an_indented_line_and_an_overlong_token_are_kept() {
        assert_eq!(wrap("short line", 40), "short line");
        let indented = format!("  {LONG}");
        assert_eq!(wrap(&indented, 40), indented);
        let token = "x".repeat(60);
        assert_eq!(
            wrap(&format!("a {token} b"), 40),
            format!("a\n  {token}\n  b")
        );
        assert_eq!(wrap("a\nb", 40), "a\nb");
    }

    #[test]
    fn a_line_with_an_odd_number_of_backticks_is_kept() {
        let line = format!("{LONG} and a name with a ` in it");
        assert_eq!(wrap(&line, 40), line);
        assert_eq!(
            wrap(&format!("{line}\n{LONG}"), 60),
            format!("{line}\n{}", wrap(LONG, 60))
        );
    }

    #[test]
    fn each_line_of_a_message_wraps_on_its_own() {
        let wrapped = wrap(&format!("{LONG}\n  {LONG}\n{LONG}"), 80);
        let lines: Vec<&str> = wrapped.split('\n').collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines.get(2), Some(&format!("  {LONG}").as_str()));
        assert!(
            lines
                .iter()
                .filter(|line| !line.starts_with("  "))
                .all(|line| line.chars().count() <= 80)
        );
    }

    proptest::proptest! {
        #[test]
        fn wrapping_keeps_spans_and_order_and_the_width(
            text in "[a-z`]{1,14}( [a-z`]{1,14}){0,30}",
            columns in 40usize..120,
        ) {
            let wrapped = wrap(&text, columns);
            proptest::prop_assert!(wrapped.replace("\n  ", " ") == text);
            let spans = |s: &str| s.split('`').skip(1).step_by(2).map(str::to_owned).collect::<Vec<_>>();
            proptest::prop_assert_eq!(spans(&wrapped), spans(&text));
            for (i, line) in wrapped.split('\n').enumerate() {
                if i > 0 {
                    proptest::prop_assert!(line.starts_with("  "));
                    proptest::prop_assert!(!line.starts_with("   "));
                }
                if line.chars().count() > columns && text.matches('`').count() % 2 == 0 {
                    let body = line.trim_start();
                    proptest::prop_assert_eq!(tokens(body).count(), 1, "{:?}", line);
                }
            }
        }

        #[test]
        fn an_indented_line_is_untouched(
            text in " {1,4}[a-z`]{1,14}( [a-z`]{1,14}){0,30}",
            columns in 40usize..120,
        ) {
            proptest::prop_assert_eq!(wrap(&text, columns), text);
        }
    }
}
