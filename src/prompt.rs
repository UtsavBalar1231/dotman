//! Questions to the user. A run is interactive only when stdin is a TTY and
//! `DOTMAN_NON_INTERACTIVE` is unset or empty; a non-interactive run never asks.

use crate::output;
use anyhow::{Context, Result};
use std::io::{self, BufRead, IsTerminal, Write};

/// Whether this run may ask questions.
#[must_use]
pub fn interactive() -> bool {
    io::stdin().is_terminal()
        && std::env::var_os("DOTMAN_NON_INTERACTIVE").is_none_or(|v| v.is_empty())
}

/// Prints `question` on stderr, kept on one line with control characters escaped, and returns
/// the trimmed answer line.
pub fn ask(question: &str) -> Result<String> {
    ask_on(&mut io::stderr().lock(), &mut io::stdin().lock(), question)
}

/// [`ask`] for a menu whose answers are two spaces apart. A terminal too narrow for one line gets
/// the rows before the last as plain output first, so no answer is cut by the terminal's own wrap.
pub fn ask_menu(question: &str) -> Result<String> {
    let mut rows = output::menu_rows(question);
    let last = rows.pop().unwrap_or_default();
    for row in &rows {
        output::context(row);
    }
    ask(&last)
}

// A question can name a key or a path from the repo.
fn ask_on(err: &mut impl Write, input: &mut impl BufRead, question: &str) -> Result<String> {
    write!(err, "{} ", output::escape_line(question)).context("cannot write to stderr")?;
    err.flush().context("cannot write to stderr")?;
    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("cannot read the answer")?;
    Ok(line.trim().to_owned())
}

/// Asks a yes/no question; anything but `y` or `yes` is no.
pub fn confirm(question: &str) -> Result<bool> {
    let answer = ask(question)?;
    Ok(matches!(answer.to_lowercase().as_str(), "y" | "yes"))
}

/// Asks until `parse` accepts the answer, naming the reason after each refusal. An empty answer,
/// or end of input, gives `None`.
pub fn ask_parsed<T>(
    question: &str,
    parse: impl FnMut(&str) -> Result<T, String>,
) -> Result<Option<T>> {
    reask(|| ask(question), parse)
}

fn reask<T>(
    mut next: impl FnMut() -> Result<String>,
    mut parse: impl FnMut(&str) -> Result<T, String>,
) -> Result<Option<T>> {
    loop {
        let answer = next()?;
        if answer.is_empty() {
            return Ok(None);
        }
        match parse(&answer) {
            Ok(value) => return Ok(Some(value)),
            Err(why) => output::warning(&why),
        }
    }
}

/// Asks a yes/no question that re-asks on any other answer; an empty answer is no.
pub fn ask_yes_no(question: &str) -> Result<bool> {
    Ok(ask_parsed(question, yes_no)?.unwrap_or(false))
}

/// `y`, `yes`, `n` or `no`, in any case.
pub fn yes_no(answer: &str) -> Result<bool, String> {
    match answer.to_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err(format!("`{answer}` is not y or n")),
    }
}

/// A number from 1 to `max`.
pub fn choice(answer: &str, max: usize) -> Result<usize, String> {
    answer
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=max).contains(n))
        .ok_or_else(|| format!("`{answer}` is not a number from 1 to {max}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scripted(answers: &[&str]) -> impl FnMut() -> Result<String> {
        let mut rest: Vec<String> = answers.iter().rev().map(|a| (*a).to_owned()).collect();
        move || Ok(rest.pop().unwrap_or_default())
    }

    #[test]
    fn an_invalid_answer_is_asked_again() {
        let mut refused = Vec::new();
        let picked = reask(scripted(&["x", "9", "0", "2"]), |a| {
            choice(a, 3).inspect_err(|why| refused.push(why.clone()))
        })
        .unwrap();
        assert_eq!(picked, Some(2));
        assert_eq!(refused.len(), 3);
        assert!(
            refused.first().is_some_and(|r| r.contains("`x`")),
            "{refused:?}"
        );
    }

    #[test]
    fn an_empty_answer_or_the_end_of_input_gives_nothing() {
        assert_eq!(reask(scripted(&[""]), |a| choice(a, 3)).unwrap(), None);
        assert_eq!(reask(scripted(&["x"]), |a| choice(a, 3)).unwrap(), None);
    }

    #[test]
    fn the_question_is_shown_escaped_on_one_line() {
        let mut shown = Vec::new();
        let answer = ask_on(
            &mut shown,
            &mut b" y \n".as_slice(),
            "keep d/\u{1b}]0;PWNED\u{7}x\r\nforged? [y/N]",
        )
        .unwrap();
        assert_eq!(answer, "y");
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            "keep d/\\u{1b}]0;PWNED\\u{7}x\\r\\nforged? [y/N] "
        );
    }

    #[test]
    fn choices_and_yes_no_are_strict() {
        assert_eq!(choice("3", 3), Ok(3));
        assert!(choice("4", 3).is_err() && choice("0", 3).is_err() && choice("-1", 3).is_err());
        assert_eq!(yes_no("Yes"), Ok(true));
        assert_eq!(yes_no("N"), Ok(false));
        assert!(yes_no("maybe").is_err());
    }
}
