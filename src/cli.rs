//! Command-line definitions, shared with xtask for man page generation.

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

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
    /// Create the dotman repo, or set its remote
    Init(InitArgs),
    /// Start managing files or directories
    Add(AddArgs),
    /// Stop managing files or directories, keeping them in $HOME
    Forget(ForgetArgs),
    /// List the managed files
    List(ListArgs),
    /// Compare $HOME, the repo and the last applied state
    Status(StatusArgs),
    /// Generate shell completion scripts
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// Arguments of `dot init`.
#[derive(Args)]
pub struct InitArgs {
    /// Set or replace the `origin` remote
    #[arg(long, value_name = "URL")]
    pub remote: Option<String>,
    /// Show what would be created and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dot add`.
#[derive(Args)]
pub struct AddArgs {
    /// Files, symlinks or directories under $HOME
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Do not ask before adding directories
    #[arg(long)]
    pub yes: bool,
    /// Show what would be added and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dot forget`.
#[derive(Args)]
pub struct ForgetArgs {
    /// Files, symlinks or directories to stop managing
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Show what would be forgotten and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dot list`.
#[derive(Args)]
pub struct ListArgs {
    /// Print `<kind>\t<key>\0` records
    #[arg(long)]
    pub porcelain: bool,
}

/// Arguments of `dot status`.
#[derive(Args)]
pub struct StatusArgs {
    /// Limit the report to these paths
    pub paths: Vec<PathBuf>,
    /// Print `<status>\t<key>\0` records for every managed path
    #[arg(long)]
    pub porcelain: bool,
}
