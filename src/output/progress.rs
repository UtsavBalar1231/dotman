//! Progress bar implementation for long-running operations.

use colored::Colorize;
use std::io::{self, IsTerminal, Write};

/// In-place progress bar on a TTY, silent otherwise; renders as "Title: 100% (6/6), done."
pub struct Progress {
    title: String,
    total: usize,
    current: usize,
    is_tty: bool,
    last_percent: u8,
    started: bool,
}

impl Progress {
    /// Creates a progress bar for `total` items.
    #[must_use]
    pub fn new(title: &str, total: usize) -> Self {
        let is_tty = io::stderr().is_terminal();

        let mut progress = Self {
            title: title.to_string(),
            total,
            current: 0,
            is_tty,
            last_percent: 0,
            started: false,
        };

        if is_tty && total > 0 {
            progress.display();
            progress.started = true;
        }

        progress
    }

    /// Moves to `current` (clamped to the total); redraws only when the percentage changes.
    pub fn update(&mut self, current: usize) {
        self.current = current.min(self.total);

        let percent = self.percent();
        if percent != self.last_percent {
            self.last_percent = percent;
            if self.is_tty {
                self.display();
            }
        }
    }

    /// Prints the final "done" line.
    // reason: skips Drop, which would print a newline after the completed bar
    #[allow(clippy::mem_forget)]
    pub fn finish(mut self) {
        self.current = self.total;
        self.last_percent = 100;

        if self.is_tty && self.started {
            self.display_final();
        }

        std::mem::forget(self);
    }

    /// Whole-number percentage; 0 when the total is 0.
    fn percent(&self) -> u8 {
        self.current
            .checked_mul(100)
            .and_then(|v| v.checked_div(self.total))
            .map_or(0, |p| u8::try_from(p.min(100)).unwrap_or(100))
    }

    fn display(&self) {
        if !self.is_tty || self.total == 0 {
            return;
        }

        eprint!(
            "\r{}: {}% ({}/{})",
            self.title.dimmed(),
            self.percent().to_string().dimmed(),
            self.current,
            self.total
        );
        let _ = io::stderr().flush();
    }

    fn display_final(&self) {
        eprintln!(
            "\r{}: 100% ({}/{}), done.",
            self.title.dimmed(),
            self.total,
            self.total
        );
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.is_tty && self.started && self.current < self.total {
            eprintln!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_initial_state() {
        let progress = Progress::new("Test", 100);
        assert_eq!(progress.current, 0);
        assert_eq!(progress.total, 100);
        assert_eq!(progress.last_percent, 0);
    }

    #[test]
    fn test_progress_update() {
        let mut progress = Progress::new("Test", 100);

        progress.update(50);
        assert_eq!(progress.current, 50);
        assert_eq!(progress.last_percent, 50);

        progress.update(75);
        assert_eq!(progress.current, 75);
        assert_eq!(progress.last_percent, 75);
    }

    #[test]
    fn test_progress_bounds_clamping() {
        let mut progress = Progress::new("Test", 10);
        progress.update(20);
        assert_eq!(progress.current, 10);
    }

    #[test]
    fn test_progress_zero_total() {
        let mut progress = Progress::new("Test", 0);
        progress.update(5);
        assert_eq!(progress.current, 0);
        assert_eq!(progress.last_percent, 0);
    }

    #[test]
    fn test_progress_percentage_calculation() {
        let mut progress = Progress::new("Test", 100);

        progress.update(25);
        assert_eq!(progress.last_percent, 25);

        progress.update(50);
        assert_eq!(progress.last_percent, 50);

        progress.update(100);
        assert_eq!(progress.last_percent, 100);
    }
}
