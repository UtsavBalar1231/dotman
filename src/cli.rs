//! Command-line definitions, shared with xtask for man page generation.

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;
use std::ffi::OsString;
use std::path::PathBuf;

// xtask reads this layout to write the man page: each `Exit status:` or `Environment:` block
// becomes that section, a line indented two spaces starts an entry, and a line indented further
// continues it. The after_long_help of a subcommand follows the same rules.
const EXIT_AND_ENVIRONMENT: &str = "\
Exit status:
  0  Success. `status` exits 0 even when it lists paths to look at.
  1  An error. `doctor` also exits 1 when one of its checks fails.
  2  The command line could not be parsed.
  3  The run finished, but left paths for you: undecided, held back,
     blocked, or reported for action.
  4  `sync` hit a rebase conflict, aborted the pull and printed the
     steps to resolve it; or a git operation or conflict is unfinished
     in the repo.
  The `git` subcommand exits with git's own status.

Environment:
  DOTMAN_REPO_PATH
      The repo, as an absolute path. It wins over `repo` in the machine
      config. Default: $XDG_DATA_HOME/dotman.
  DOTMAN_CONFIG_PATH
      The machine config, as an absolute path.
      Default: $XDG_CONFIG_HOME/dotman/config.toml.
  DOTMAN_NON_INTERACTIVE
      Any non-empty value asks nothing, as when stdin is not a terminal.
      Paths a prompt would decide are skipped and listed, and the run
      exits 3.
  DOTMAN_ALLOW_ROOT
      1 lets dotman run as root, as --allow-root does.
  DOTMAN_AGE_KEY
      The text of an age identity. It reaches age only on its stdin, and
      every other child process starts without it.
  DOTMAN_AGE_KEY_FILE
      An age identity file, as an absolute path, tried before the
      identity file in $XDG_CONFIG_HOME/dotman.
  DOTMAN_HOST
      Replaces the host name, detected or set under [identity] in the
      machine config, for variants and templates.
  DOTMAN_PROFILES
      Comma-separated profiles that replace the machine config's.
  DOTMAN_APPROVE_HOOKS, DOTMAN_BOOTSTRAP_BASE_URL
      Read by the install.sh that `init` writes, not by dotman: 1
      approves the repo's hooks, and the URL replaces the release
      download base.
  DOTMAN_SYSTEM_ROOT, DOTMAN_NIX_STORE
      For tests only; `doctor` warns when either is set. The system-write
      helper refuses DOTMAN_SYSTEM_ROOT when it runs as root.
  NO_COLOR
      Turns colour off.
  NO_PAGER
      Any non-empty value makes `diff` skip the pager, as --no-pager does.
  VISUAL, EDITOR
      The editor `edit` starts; VISUAL wins.
  XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME
      Where the machine config, the repo and the state with its backups
      live. Each defaults under $HOME.
  XDG_RUNTIME_DIR
      Where `diff` puts its private temporary copies; the state
      directory when it is unset.
  RUST_LOG
      A tracing filter for debug output on stderr. Default: warn.";

const STATUS_HELP: &str = "\
Exit status: 0 whenever the repo can be read, whatever the paths show; 4
while a git operation is unfinished in the repo.

With --porcelain, each record is `<token>\\t<key>\\0`. The tokens:
  clean, not-applied, existing, changed-locally, changed-in-repo,
  both-changed, deleted-locally, deleted-changed-upstream, forgotten,
  forgotten-edited, gone, template-drift, blocked, symlink-at-target
      the paths compared in $HOME, the repo and the last applied state
  ambiguous, invalid, locked, template-error, unreadable
      paths that could not be chosen, read, decrypted or rendered
  system-off
      a system file on a machine whose [system] roots do not cover it
  collision
      two keys that differ only in case or Unicode normalization
  new, held
      files under a managed directory that sync would capture, or holds
  held-in-repo
      an uncommitted repo edit that sync holds; its key is the repo path";

const SYNC_HELP: &str = "\
Exit status:
  3  Paths were left undecided or held back. Everything else is still
     committed, pulled, applied and pushed, except when an uncommitted
     repo edit holds a credential or an age key: then sync stops before
     the pull.
  4  The pull hit a conflict. Sync aborted it, so nothing it brought
     reached $HOME, and printed the steps to resolve it.
  1  An error. Among them: the remote shares no commit with the repo, or
     the commit would delete at least 10 tracked files and at least half
     of them, which needs a yes at the prompt or --allow-mass-delete.
     The next sync captures again the files under [dirs] that $HOME
     still has, unless `forget <dir>` stops managing them first.";

const DIFF_HELP: &str = "\
The diff runs from repo/<key> to home/<key>, so an edit made in $HOME shows
as added lines. It goes through git's pager unless --no-pager or NO_PAGER
is set, or the pager cannot be found. NO_COLOR turns colour off.";

const GIT_HELP: &str = "\
Runs git in the repo with the isolation flags every git call of dotman
carries, and exits with git's own status. It takes no lock and runs while
a rebase is unfinished, which is how one gets resolved.";

/// Top-level arguments for `dotman`.
#[derive(Parser)]
#[command(
    name = "dotman",
    version = crate::VERSION,
    about = "A dotfiles manager with no learning curve, safe by default",
    after_long_help = EXIT_AND_ENVIRONMENT
)]
// reason: independent global flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Commands,

    /// Show verbose output
    #[arg(short, long, global = true, help_heading = "Global options")]
    pub verbose: bool,

    /// Suppress informational messages
    #[arg(short, long, global = true, help_heading = "Global options")]
    pub quiet: bool,

    /// Disable pager output
    #[arg(long, global = true, help_heading = "Global options")]
    pub no_pager: bool,

    /// Allow running as root
    #[arg(long, global = true, help_heading = "Global options")]
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
    #[command(long_about = concat!(
        "List the managed files.\n\n",
        "With --porcelain, each record is `<kind>\\t<key>\\0`: a kind word, a tab, the key and a NUL. ",
        "It carries no source or flags, and the layout is stable for scripts.\n\n",
        "The kind is one of:\n",
        "  file     a regular file under $HOME; the key is its path relative to $HOME\n",
        "  symlink  a symbolic link under $HOME, stored as its target\n",
        "  system   a file outside $HOME, written by `apply --system`; the key is its absolute path"
    ))]
    List(ListArgs),
    /// Compare $HOME, the repo and the last applied state
    #[command(after_long_help = STATUS_HELP)]
    Status(StatusArgs),
    /// Write the repo's files into $HOME
    Apply(ApplyArgs),
    /// Show how $HOME differs from the repo, as a diff from repo/ to home/
    #[command(after_long_help = DIFF_HELP)]
    Diff(DiffArgs),
    /// Open a managed file's source in $VISUAL or $EDITOR, then apply it
    Edit(EditArgs),
    /// Capture local changes, commit, pull, apply what arrived and push
    #[command(after_long_help = SYNC_HELP)]
    Sync(SyncArgs),
    /// Show the repo's history
    Log(LogArgs),
    /// Write an earlier version of a file into $HOME, keeping the current one as a backup
    Restore(RestoreArgs),
    /// Convert another setup into the repo, or add what the repo lacks from one
    Import(ImportArgs),
    /// Run git in the repo
    #[command(after_long_help = GIT_HELP)]
    Git(GitArgs),
    /// Check the environment and the repo for problems
    Doctor(DoctorArgs),
    /// Create or import the age identity that decrypts encrypted files
    Key(KeyArgs),
    /// Generate shell completion scripts
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Write the system files a plan names; `apply --system` starts it through sudo, doas or run0
    #[command(hide = true)]
    SystemWrite(SystemWriteArgs),
}

/// Arguments of the hidden `dotman system-write`.
#[derive(Args)]
pub struct SystemWriteArgs {
    /// The plan file that `apply --system` wrote
    #[arg(long, value_name = "FILE")]
    pub plan: PathBuf,
}

/// Arguments of `dotman init`.
#[derive(Args)]
// reason: independent flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct InitArgs {
    /// Git URL or local path of an existing dotman repo to clone and apply
    #[arg(value_name = "URL_OR_PATH", help_heading = "Source")]
    pub source: Option<String>,
    /// Set or replace the `origin` remote
    #[arg(
        long,
        value_name = "URL",
        conflicts_with = "source",
        help_heading = "Remote"
    )]
    pub remote: Option<String>,
    /// Create a fresh repo without looking for an existing setup to import
    #[arg(long, conflicts_with = "source", help_heading = "Source")]
    pub no_import: bool,
    /// Keep the local version of files that differ from the repo
    #[arg(
        long,
        requires = "source",
        conflicts_with = "take_repo",
        help_heading = "Decisions"
    )]
    pub keep_local: bool,
    /// Take the repo version of files that differ, after backing them up
    #[arg(long, requires = "source", help_heading = "Decisions")]
    pub take_repo: bool,
    /// Approve the `[hooks]` table of the cloned repo on this machine and run its hooks
    #[arg(long, requires = "source", help_heading = "Decisions")]
    pub approve_hooks: bool,
    /// Show what would be created and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Do not write install.sh, the script that sets up `dotman` in a container
    #[arg(long, conflicts_with_all = ["source", "refresh_bootstrap"], help_heading = "This machine")]
    pub no_bootstrap: bool,
    /// Rewrite install.sh with this version's script, unless it was edited
    #[arg(long, conflicts_with = "source", help_heading = "This machine")]
    pub refresh_bootstrap: bool,
    /// Git `user.name` to set when git has no identity
    #[arg(long, value_name = "NAME", help_heading = "This machine")]
    pub git_name: Option<String>,
    /// Git `user.email` to set when git has no identity
    #[arg(long, value_name = "EMAIL", help_heading = "This machine")]
    pub git_email: Option<String>,
    /// Store --git-name and --git-email in the global git config instead of the repo's
    #[arg(long, help_heading = "This machine")]
    pub git_global: bool,
    /// Create the private GitHub repo `OWNER/NAME` with `gh` and make it `origin`
    #[arg(
        long,
        value_name = "OWNER/NAME",
        conflicts_with_all = ["source", "remote", "no_remote"],
        help_heading = "Remote"
    )]
    pub create_github: Option<String>,
    /// Do not ask for a remote
    #[arg(long, conflicts_with_all = ["source", "remote"], help_heading = "Remote")]
    pub no_remote: bool,
    /// Host name of this machine, written to `[identity]` in the machine config
    #[arg(long, value_name = "NAME", help_heading = "This machine")]
    pub host: Option<String>,
    /// A profile this machine opts into, written to the machine config; repeatable
    #[arg(long, value_name = "NAME", help_heading = "This machine")]
    pub profile: Vec<String>,
    /// A directory outside $HOME whose files this machine manages, added to `[system] roots`; repeatable
    #[arg(long, value_name = "DIR", help_heading = "This machine")]
    pub system_root: Vec<PathBuf>,
    /// Do not ask about system files
    #[arg(long, conflicts_with = "system_root", help_heading = "This machine")]
    pub no_system: bool,
    /// Create an age identity for encrypted files
    #[arg(long, conflicts_with_all = ["source", "no_key"], help_heading = "This machine")]
    pub create_key: bool,
    /// Do not ask about an age identity
    #[arg(long, conflicts_with = "source", help_heading = "This machine")]
    pub no_key: bool,
    /// Import the age identity in FILE before applying, so encrypted files decrypt
    #[arg(
        long,
        value_name = "FILE",
        requires = "source",
        help_heading = "This machine"
    )]
    pub age_key_file: Option<PathBuf>,
    /// With --age-key-file: replace a different existing identity, after backing it up
    #[arg(long, requires = "age_key_file", help_heading = "This machine")]
    pub force: bool,
}

/// Answers to the prompts of `apply` and `sync`, given in advance.
#[derive(Args, Clone, Copy, Debug, Default)]
// reason: independent answers to separate prompts, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct Decisions {
    /// Keep the local version of conflicting and existing files; sync captures it into the repo for every machine, or into the variant this machine applies when that names its host. To keep a file to this machine only, run `add --for host=<host> <file>` instead, or `add --encrypt --for host=<host> <file>` when any of its sources is encrypted
    #[arg(long, help_heading = "Decisions", conflicts_with = "take_repo")]
    pub keep_local: bool,
    /// Take the repo version of conflicting and existing files, after backing them up
    #[arg(long, help_heading = "Decisions")]
    pub take_repo: bool,
    /// Stop managing files deleted locally; they stay deleted
    #[arg(long, help_heading = "Decisions", conflicts_with = "restore_deleted")]
    pub forget_deleted: bool,
    /// Write files deleted locally back from the repo
    #[arg(long, help_heading = "Decisions")]
    pub restore_deleted: bool,
    /// Accept a changed machine identity and use the variants it selects
    #[arg(long, help_heading = "Decisions")]
    pub accept_identity: bool,
    /// Approve the current `[hooks]` table on this machine and run its hooks
    #[arg(long, help_heading = "Decisions")]
    pub approve_hooks: bool,
}

/// Arguments of `dotman apply`.
#[derive(Args)]
// reason: independent flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct ApplyArgs {
    /// Apply only these files or directories, with --system system files too; every managed path when none are given
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
    /// Also write the system files outside $HOME, through one sudo, doas or run0 call
    #[arg(long)]
    pub system: bool,
    /// With --system: write the system files without asking
    #[arg(long, requires = "system")]
    pub yes: bool,
}

/// Arguments of `dotman edit`.
#[derive(Args)]
pub struct EditArgs {
    /// The managed file to edit
    pub path: PathBuf,
}

/// Arguments of `dotman sync`.
#[derive(Args)]
// reason: independent flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
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
    /// After the pull, also write the system files outside $HOME, through one sudo, doas or run0 call
    #[arg(long)]
    pub system: bool,
    /// With --system: write the system files without asking
    #[arg(long, requires = "system")]
    pub yes: bool,
    /// Commit even when the repo changes delete at least half of the tracked files (10 or more); a deleted file under a `[dirs]` entry that $HOME still has is captured again by the next sync, so stop managing those with `forget <dir>` first
    #[arg(long)]
    pub allow_mass_delete: bool,
}

/// Arguments of `dotman diff`.
#[derive(Args)]
pub struct DiffArgs {
    /// Compare only these files or directories; every managed path when none are given
    pub paths: Vec<PathBuf>,
}

/// Arguments of `dotman add`.
#[derive(Args)]
// reason: independent flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct AddArgs {
    /// Files, symlinks or directories under $HOME; with --system, files outside it
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Add the files as variants that apply only where these `+`-joined conditions hold, such as
    /// `host=laptop+os!=macos`
    #[arg(long = "for", value_name = "CONDITIONS")]
    pub for_conditions: Option<String>,
    /// Store the files encrypted to the `[age] recipients` in .dotman.toml
    #[arg(long)]
    pub encrypt: bool,
    /// Store in plain text what the guard holds back: files whose names match `[guard] secret_names`, and credentials in files named here, not ones found in a directory; never an age secret key
    #[arg(long, conflicts_with = "encrypt")]
    pub allow_plain: bool,
    /// Do not ask before adding directories
    #[arg(long)]
    pub yes: bool,
    /// Show what would be added and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Add regular files outside $HOME that you can read, such as /etc/keyd/default.conf
    #[arg(long)]
    pub system: bool,
    /// With --system: the file belongs to a package; manage it anyway
    #[arg(long, requires = "system")]
    pub package_owned: bool,
    /// With --system: add the top-level directory to `[system] roots` without asking
    #[arg(long, requires = "system")]
    pub add_root: bool,
}

/// Arguments of `dotman forget`.
#[derive(Args)]
pub struct ForgetArgs {
    /// Files, symlinks or directories to stop managing
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Show what would be forgotten and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dotman list`.
#[derive(Args)]
pub struct ListArgs {
    /// Print `<kind>\t<key>\0` records
    #[arg(long)]
    pub porcelain: bool,
}

/// Arguments of `dotman status`.
#[derive(Args)]
pub struct StatusArgs {
    /// Limit the report to these paths
    pub paths: Vec<PathBuf>,
    /// Print `<status>\t<key>\0` records for every managed path
    #[arg(long)]
    pub porcelain: bool,
}

/// Arguments of `dotman log`.
#[derive(Args)]
pub struct LogArgs {
    /// Files, directories or globs to filter by; a trailing `/` means the whole subtree
    pub paths: Vec<String>,
    /// Show at most this many commits
    #[arg(short = 'n', long, value_name = "N")]
    pub max_count: Option<usize>,
}

/// Arguments of `dotman restore`.
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

/// Arguments of `dotman import`.
#[derive(Args)]
// reason: independent flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
pub struct ImportArgs {
    /// Legacy dotman repo, chezmoi source, symlink-farm directory, yadm, bare or vcsh repo, or git URL to convert; found in $HOME when omitted
    #[arg(value_name = "SOURCE", conflicts_with = "overlay")]
    pub source: Option<OsString>,
    /// Add what SOURCE has and the repo lacks as one commit: modes, and with `--include-missing` files
    #[arg(long, value_name = "SOURCE")]
    pub overlay: Option<OsString>,
    /// With `--overlay`, add the files the repo lacks without asking
    #[arg(long)]
    pub include_missing: bool,
    /// For a chezmoi source, run its templates and import this host's rendering
    #[arg(long, conflicts_with = "overlay")]
    pub render_templates: bool,
    /// Replace the symlinks of an imported farm with real files holding the same bytes
    #[arg(
        long,
        conflicts_with_all = ["source", "overlay", "include_missing", "render_templates"]
    )]
    pub swap: bool,
    /// For a git source whose history holds a credential or an age secret key, import every commit instead of one commit of the current tree
    #[arg(long, conflicts_with_all = ["overlay", "swap"])]
    pub keep_history: bool,
    /// Do not ask before writing
    #[arg(long)]
    pub yes: bool,
    /// Do not write install.sh, the script that sets up `dotman` in a container
    #[arg(long)]
    pub no_bootstrap: bool,
    /// Show the report and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments of `dotman doctor`.
#[derive(Args)]
pub struct DoctorArgs {
    /// Also ask the remote with `git ls-remote`
    #[arg(long)]
    pub online: bool,
}

/// Arguments of `dotman key`.
#[derive(Args)]
pub struct KeyArgs {
    /// What to do with the identity.
    #[command(subcommand)]
    pub command: KeyCommand,
}

/// Subcommands of `dotman key`.
#[derive(Subcommand)]
pub enum KeyCommand {
    /// Generate an age identity and add its recipient to .dotman.toml
    Init,
    /// Copy an existing age identity into place and check that its recipient is listed
    Import(KeyImportArgs),
}

/// Arguments of `dotman key import`.
#[derive(Args)]
pub struct KeyImportArgs {
    /// Identity file to import; `-` reads standard input
    #[arg(value_name = "FILE", default_value = "-")]
    pub file: PathBuf,
    /// Add the identity's recipient to `[age] recipients` in .dotman.toml if it is missing
    #[arg(long)]
    pub add_recipient: bool,
    /// Replace an existing, different identity, after backing it up
    #[arg(long)]
    pub force: bool,
}

/// Arguments of `dotman git`.
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
