//! Questions to the user. A run is interactive only when stdin is a TTY and
//! `DOTMAN_NON_INTERACTIVE` is unset or empty; a non-interactive run never asks.

use anyhow::{Context, Result};
use std::io::{self, BufRead, IsTerminal, Write};

/// Whether this run may ask questions.
#[must_use]
pub fn interactive() -> bool {
    io::stdin().is_terminal()
        && std::env::var_os("DOTMAN_NON_INTERACTIVE").is_none_or(|v| v.is_empty())
}

/// Prints `question` on stderr and returns the trimmed answer line.
pub fn ask(question: &str) -> Result<String> {
    let mut err = io::stderr().lock();
    write!(err, "{question} ").context("cannot write to stderr")?;
    err.flush().context("cannot write to stderr")?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("cannot read the answer")?;
    Ok(line.trim().to_owned())
}

/// Asks a yes/no question; anything but `y` or `yes` is no.
pub fn confirm(question: &str) -> Result<bool> {
    let answer = ask(question)?;
    Ok(matches!(answer.to_lowercase().as_str(), "y" | "yes"))
}
