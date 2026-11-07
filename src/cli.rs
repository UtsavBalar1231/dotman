//! Command-line definitions, shared with xtask for man page generation.

use clap::{Parser, Subcommand};
use clap_complete::Shell;

/// Top-level arguments for `dot`.
#[derive(Parser)]
#[command(name = "dot", version = crate::VERSION, about = "Git-like dotfiles manager")]
// reason: independent global flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Commands,

    /// Show verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Suppress informational messages
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Disable pager output
    #[arg(long, global = true)]
    pub no_pager: bool,

    /// Allow running as root
    #[arg(long, global = true)]
    pub allow_root: bool,
}

/// Available subcommands.
#[derive(Subcommand)]
pub enum Commands {
    /// Generate shell completion scripts
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}
