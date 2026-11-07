//! Styled status output on stderr, gated by a global verbosity level.

mod progress;

use colored::Colorize;
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

/// Prints a success message in green (respects quiet mode).
pub fn success(message: &str) {
    if get_verbosity() == Verbosity::Quiet {
        return;
    }
    eprintln!("{}", message.green());
}

/// Prints an error message in bold red (always shown).
pub fn error(message: &str) {
    eprintln!("{}", message.red().bold());
}

/// Prints a warning message in bold yellow (always shown).
pub fn warning(message: &str) {
    eprintln!("{}", message.yellow().bold());
}

/// Prints an informational message in dimmed color (respects quiet mode).
pub fn info(message: &str) {
    if get_verbosity() == Verbosity::Quiet {
        return;
    }
    eprintln!("{}", message.dimmed());
}

/// Prints a verbose debug message (only in verbose mode).
pub fn verbose(message: &str) {
    if get_verbosity() != Verbosity::Verbose {
        return;
    }
    eprintln!("{}", message.dimmed());
}

/// Prints a git-style action message with dimmed verb and normal message.
pub fn action(verb: &str, message: &str) {
    if get_verbosity() == Verbosity::Quiet {
        return;
    }
    eprintln!("{} {}", verb.dimmed().bold(), message);
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
}
