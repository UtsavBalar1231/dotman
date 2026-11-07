//! Command-line definitions, shared with xtask for man page generation.

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;
use std::ffi::OsString;
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
    /// Create the dotman repo or set its remote, or clone a repo and apply it
    Init(InitArgs),
    /// Start managing files or directories
    Add(AddArgs),
    /// Stop managing files or directories, keeping them in $HOME
    Forget(ForgetArgs),
    /// List the managed files
    List(ListArgs),
    /// Compare $HOME, the repo and the last applied state
    Status(StatusArgs),
    /// Write the repo's files into $HOME
    Apply(ApplyArgs),
    /// Show how $HOME differs from the repo
    Diff(DiffArgs),
    /// Open a managed file's source in $VISUAL or $EDITOR, then apply it
    Edit(EditArgs),
    /// Capture local changes, commit, pull, apply what arrived and push
    Sync(SyncArgs),
    /// Show the repo's history
    Log(LogArgs),
    /// Write an earlier version of a file into $HOME, keeping the current one as a backup
    Restore(RestoreArgs),
    /// Run git in the repo
    Git(GitArgs),
    /// Check the environment and the repo for problems
    Doctor(DoctorArgs),
    /// Generate shell completion scripts
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// Arguments of `dot init`.
#[derive(Args)]
// reason: independent flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct InitArgs {
    /// Git URL or local path of an existing dotman repo to clone and apply
    #[arg(value_name = "URL_OR_PATH")]
    pub source: Option<String>,
    /// Set or replace the `origin` remote
    #[arg(long, value_name = "URL", conflicts_with = "source")]
    pub remote: Option<String>,
    /// Keep the local version of files that differ from the repo
    #[arg(long, requires = "source", conflicts_with = "take_repo")]
    pub keep_local: bool,
    /// Take the repo version of files that differ, after backing them up
    #[arg(long, requires = "source")]
    pub take_repo: bool,
    /// Approve the `[hooks]` table of the cloned repo on this machine and run its hooks
    #[arg(long, requires = "source")]
    pub approve_hooks: bool,
    /// Show what would be created and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Answers to the prompts of `apply` and `sync`, given in advance.
#[derive(Args, Clone, Copy, Debug, Default)]
// reason: independent answers to separate prompts, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct Decisions {
    /// Keep the local version of conflicting and existing files
    #[arg(long, conflicts_with = "take_repo")]
    pub keep_local: bool,
    /// Take the repo version of conflicting and existing files, after backing them up
    #[arg(long)]
    pub take_repo: bool,
    /// Stop managing files deleted locally; they stay deleted
    #[arg(long, conflicts_with = "restore_deleted")]
    pub forget_deleted: bool,
    /// Write files deleted locally back from the repo
    #[arg(long)]
    pub restore_deleted: bool,
    /// Accept a changed machine identity and use the variants it selects
    #[arg(long)]
    pub accept_identity: bool,
    /// Approve the current `[hooks]` table on this machine and run its hooks
    #[arg(long)]
    pub approve_hooks: bool,
}

/// Arguments of `dot apply`.
#[derive(Args)]
pub struct ApplyArgs {
    /// Apply only these files or directories; every managed path when none are given
    pub paths: Vec<PathBuf>,
    /// Answers that replace the prompts, which a non-interactive run never shows.
    #[command(flatten)]
    pub decisions: Decisions,
    /// Rewrite files in place when a rename cannot replace them, as on a bind mount
    #[arg(long)]
    pub in_place: bool,
    /// Run no hooks
    #[arg(long, conflicts_with = "approve_hooks")]
    pub no_hooks: bool,
    /// Show what would be written and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dot edit`.
#[derive(Args)]
pub struct EditArgs {
    /// The managed file to edit
    pub path: PathBuf,
}

/// Arguments of `dot sync`.
#[derive(Args)]
pub struct SyncArgs {
    /// Commit message, replacing the generated `sync(<host>): ...` line
    #[arg(short = 'm', long, value_name = "MSG")]
    pub message: Option<String>,
    /// Answers that replace the prompts, which a non-interactive run never shows.
    #[command(flatten)]
    pub decisions: Decisions,
    /// Run no hooks
    #[arg(long, conflicts_with = "approve_hooks")]
    pub no_hooks: bool,
    /// Show what would be captured, committed, pulled, applied and pushed; change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dot diff`.
#[derive(Args)]
pub struct DiffArgs {
    /// Compare only these files or directories; every managed path when none are given
    pub paths: Vec<PathBuf>,
}

/// Arguments of `dot add`.
#[derive(Args)]
pub struct AddArgs {
    /// Files, symlinks or directories under $HOME
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Add the files as variants that apply only where these `+`-joined conditions hold, such as
    /// `host=laptop+os!=macos`
    #[arg(long = "for", value_name = "CONDITIONS")]
    pub for_conditions: Option<String>,
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

/// Arguments of `dot log`.
#[derive(Args)]
pub struct LogArgs {
    /// Files, directories or globs to filter by; a trailing `/` means the whole subtree
    pub paths: Vec<String>,
    /// Show at most this many commits
    #[arg(short = 'n', long, value_name = "N")]
    pub max_count: Option<usize>,
}

/// Arguments of `dot restore`.
#[derive(Args)]
pub struct RestoreArgs {
    /// The managed file or symlink to restore
    pub path: PathBuf,
    /// Revision or date to restore from; `HEAD` when omitted
    #[arg(long, value_name = "REV_OR_DATE")]
    pub from: Option<String>,
    /// Show what would be written and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dot doctor`.
#[derive(Args)]
pub struct DoctorArgs {
    /// Also ask the remote with `git ls-remote`
    #[arg(long)]
    pub online: bool,
}

/// Arguments of `dot git`.
#[derive(Args)]
#[command(disable_help_flag = true)]
pub struct GitArgs {
    /// Passed to git as given
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub args: Vec<OsString>,
}
