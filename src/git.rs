//! Git CLI wrapper that ignores the user's global git configuration where it would change stored content.
//!
//! Every fallible method returns an error naming the failing git arguments and its stderr.

use crate::engine::write::{back_up_bytes, backup_run_dir};
use crate::paths::{validate_key, write_atomic};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Seek, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_128;

/// `merge`, set with no value, is git's built-in text merge whatever a tracked `merge=union`, a
/// `merge.<name>.driver` or `merge.default` says; either of those could join both sides of a
/// conflict without a word. Not `merge=text`: git looks a driver named `text` up in the user's
/// config first. Not `-merge`: unset means binary, which keeps one side. It must be the last line
/// of `.git/info/attributes`, where a later line wins. `merge` comes first: git reads a line from
/// its last attribute back and keeps the first value it sets, and `merge` is the only one set to
/// true, so a tracked `[attr]merge` macro expands, and its members finding the other four already
/// set change nothing. Written last, the macro's `text` or `filter=...` would win.
const ATTRIBUTES: &str = "* merge -text -filter -ident -working-tree-encoding";

/// The lines older dotman versions wrote where [`ATTRIBUTES`] goes; a refresh drops them and keeps
/// every line the user wrote.
const OLD_ATTRIBUTES: [&str; 3] = [
    "* -text -filter -ident -working-tree-encoding",
    "* -text -filter -ident -working-tree-encoding merge=text",
    "* -text -filter -ident -working-tree-encoding merge",
];

/// What git says about an index it cannot parse. Not a bare "index file": a failed object write
/// ends in "unable to index file", which says nothing about the index.
const CORRUPT_INDEX: [&str; 5] = [
    "index file corrupt",
    "bad index file",
    "index file smaller than expected",
    "bad index version",
    "bad signature",
];

const MIN_VERSION: (u32, u32) = (2, 25);

/// For `init` and `clone`: an empty template directory copies nothing, so neither
/// `init.templateDir` nor `GIT_TEMPLATE_DIR` can put a config, an exclude file or a sparse
/// checkout into dotman's repo.
const NO_TEMPLATE: &str = "--template=";

/// The write engine's temp files, which are never staged. A wildcard pathspec matches whole paths
/// only, so a temp directory needs its own `/**`.
const TEMP_FILES: &str = ":(exclude,glob)**/.dotman-tmp-*";
const TEMP_DIRS: &str = ":(exclude,glob)**/.dotman-tmp-*/**";
const TRACKED_TEMP_FILES: &str = ":(glob)**/.dotman-tmp-*";
const TRACKED_TEMP_DIRS: &str = ":(glob)**/.dotman-tmp-*/**";

/// `core.excludesFile=` must stay empty: otherwise the user's global ignore file hides files from `git status`.
/// `core.hooksPath=/dev/null` makes every hook lookup fail, so no hook runs.
/// `core.fsmonitor=false` keeps a configured monitor daemon from answering for the worktree.
/// `status.renames=true` keeps the rename records that a held rename relies on, whatever
/// `status.renames` or `diff.renames` the user set.
/// `core.symlinks=true`: with `false`, git checks a managed symlink out as a file holding its
/// target, and status calls it clean.
/// Each `color.<command>` switch beats `color.ui`, so a user's `color.diff=always` would color
/// the `git log` output that dotman parses unless it is turned off too.
const ISOLATION: [&str; 40] = [
    "-c",
    "core.excludesFile=",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.safecrlf=false",
    "-c",
    "core.quotePath=false",
    "-c",
    "log.showSignature=false",
    "-c",
    "color.ui=false",
    "-c",
    "color.diff=false",
    "-c",
    "color.status=false",
    "-c",
    "color.branch=false",
    "-c",
    "color.showBranch=false",
    "-c",
    "color.grep=false",
    "-c",
    "color.interactive=false",
    "-c",
    "color.advice=false",
    "-c",
    "color.push=false",
    "-c",
    "color.remote=false",
    "-c",
    "color.transport=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "status.renames=true",
    "-c",
    "core.symlinks=true",
];

/// How long a short local probe, such as `git --version`, may take before it counts as hung.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Variables that make git ignore `-C`, as when `dotman` runs inside a git hook, or that point it
/// at other refs, objects or history: `GIT_NAMESPACE` pushes to `refs/namespaces/<ns>/` while
/// `main` stays behind. The pathspec ones would change how the held and temp exclusions read.
/// `GIT_CEILING_DIRECTORIES` is set again by [`Git::base`].
const LOCAL_REPO_ENV: [&str; 18] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_QUARANTINE_PATH",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// Picks, from the untracked paths `git status` lists, those that [`Git::status`] hides and
/// [`Git::stage_all_except`] leaves out, as git does with what its own ignore rules match. A
/// directory it returns stands for every path under it.
pub type LeaveOut = Box<dyn Fn(&Git, &[PathBuf]) -> Result<Vec<PathBuf>> + Send + Sync>;

/// A git repository driven through the `git` binary with config isolation.
pub struct Git {
    repo: PathBuf,
    dot_command: &'static str,
    env: Vec<(OsString, OsString)>,
    version_checked: OnceLock<()>,
    leave_out: Option<LeaveOut>,
    /// Where [`Git::recover_pull`] saves an uncommitted edit before it puts the repo back.
    backups: Option<PathBuf>,
}

impl fmt::Debug for Git {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Git")
            .field("repo", &self.repo)
            .field("dot_command", &self.dot_command)
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

/// On the first pull and on every rebase of the replay: the user's config must not move another
/// branch, or the branch itself off a detached `HEAD`, or settle a conflict from a recorded
/// resolution. On git before 2.38 `rebase.updateRefs` is unknown and ignored. The user's
/// `rebase.backend` is overridden by [`Git::pull_backend`] and the replay's `--merge`.
const PULL_PINS: [&str; 4] = [
    "-c",
    "rebase.updateRefs=false",
    "-c",
    "rerere.enabled=false",
];

/// In `.git`. While it exists, a replay may have moved `HEAD`, the index or the worktree, and
/// [`Git::recover_pull`] puts them back.
const PULL_MARKER: &str = "dotman-pull";

/// The first field of the marker, so a file from another version is never misread.
const MARKER_VERSION: &[u8] = b"dotman-pull 1";

/// One entry of a tree or an index stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `100644`, `100755`, `120000`, or `160000` for a gitlink.
    pub mode: String,
    /// Full object id.
    pub oid: String,
}

/// The final content of one path for a whole pull, however many local commits stop on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The path holds this entry.
    Set(Entry),
    /// The path is removed.
    Absent,
}

/// Which side of each clash [`Git::merge_file`] takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The first version given.
    Ours,
    /// The third version given.
    Theirs,
}

/// Each path's blob ids in `trees` and in the `unmerged` stages; gitlinks are left out.
fn known_blobs<'a>(
    trees: impl IntoIterator<Item = &'a BTreeMap<PathBuf, Entry>>,
    unmerged: &'a BTreeMap<PathBuf, [Option<Entry>; 3]>,
) -> BTreeMap<PathBuf, BTreeSet<String>> {
    let staged = unmerged
        .iter()
        .flat_map(|(path, stages)| stages.iter().flatten().map(move |entry| (path, entry)));
    let mut blobs: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for (path, entry) in trees.into_iter().flatten().chain(staged) {
        if entry.mode != "160000" {
            blobs
                .entry(path.clone())
                .or_default()
                .insert(entry.oid.clone());
        }
    }
    blobs
}

fn holds(tree: &BTreeMap<PathBuf, Entry>, path: &Path, decision: &Decision) -> bool {
    match decision {
        Decision::Set(entry) => tree.get(path) == Some(entry),
        Decision::Absent => !tree.contains_key(path),
    }
}

/// One path a rebase stopped on, from its stages in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped {
    /// Repo-relative, as git reports it.
    pub path: PathBuf,
    /// Stage 1, the merge base's version; `None` when the base lacks it.
    pub base: Option<Entry>,
    /// Stage 2, the side the rebase builds on: the fetched commit with the local commits replayed
    /// so far.
    pub ours: Option<Entry>,
    /// Stage 3, the local commit being replayed.
    pub replayed: Option<Entry>,
    /// A stage is not what `HEAD` or the replayed commit holds at this path: git followed a
    /// rename into it, or put a copy there that a file against a directory displaced.
    pub follows_rename: bool,
    /// Where git 2.25 put this path's file when a directory holds the path in the worktree: an
    /// untracked copy, repo-relative, such as `x~<commit>`. Later gits stop on the copy's path
    /// instead, with the stage there.
    pub aside: Option<PathBuf>,
}

/// Result of [`Git::replay`].
#[derive(Debug, PartialEq, Eq)]
pub enum Replayed {
    /// The branch holds the fetched commit with the local commits on top, every settled path at
    /// its decision.
    Done,
    /// A local commit stopped on paths that have no decision. The replay was undone, and the repo
    /// is as before it.
    Stopped(Vec<Stopped>),
}

/// What [`Git::recover_pull`] did.
#[derive(Debug)]
pub enum Recovery {
    /// There is no marker.
    NoMarker,
    /// The pull had not moved the branch; the repo is back as it was before it.
    Restored {
        /// Uncommitted edits of the marker's paths, saved first.
        saved: Option<Saved>,
    },
    /// The branch already held the finished pull; `HEAD` is on it again.
    Finished,
    /// Lock files a killed git left; recovery waits until they are removed.
    Locked(Vec<PathBuf>),
    /// The repo is in a state the marker does not explain; nothing was changed.
    Refused(Refusal),
}

/// Worktree content of the marker's paths that holds none of the replay's blobs, an edit made by
/// hand after the kill, which [`Git::recover_pull`] copied before it put the repo back.
#[derive(Debug)]
pub struct Saved {
    /// Repo-relative.
    pub paths: Vec<PathBuf>,
    /// The `.dotman/repo` directory of a backup run, which holds each copy at its repo path.
    pub dir: PathBuf,
}

/// Why [`Git::recover_pull`] left the repo as it was, with the steps that put it back by hand.
#[derive(Debug)]
pub struct Refusal {
    why: String,
    repo: PathBuf,
    /// `None` when the marker cannot be read.
    marker: Option<(String, String)>,
    /// With no marker read and `HEAD` detached, the branch the repo syncs, which the steps check
    /// out again; a sync on a detached `HEAD` would only point at a new branch.
    detached_from: Option<String>,
    head: Option<String>,
    rebase_dir: bool,
    /// Worktree files, repo-relative, that hold changes `checkout -f` would drop.
    dirty: Vec<PathBuf>,
    /// Where the steps copy `dirty` first: the `.dotman/repo` directory of a new backup run, made
    /// private with the parent of each copy. `None` when no backups directory is set or it could
    /// not be made.
    saves_to: Option<PathBuf>,
}

impl Refusal {
    /// The steps that save the dirty files, check out `branch` and go on. A symlink is copied as
    /// one.
    fn checkout_steps(&self, branch: &str) -> Vec<String> {
        let bin = crate::name::bin();
        let quote = |path: &Path| shell_words::quote(&path.to_string_lossy()).into_owned();
        let mut steps = Vec::new();
        if let Some(dir) = &self.saves_to {
            for path in &self.dirty {
                steps.push(format!(
                    "cp -P -- {} {}",
                    quote(&self.repo.join(path)),
                    quote(&dir.join(path))
                ));
            }
        }
        if self.rebase_dir {
            steps.push(format!("{bin} git rebase --quit"));
        }
        steps.push(format!(
            "{bin} git checkout -f {}",
            shell_words::quote(branch)
        ));
        steps
    }

    /// What `checkout -f` drops, and where the steps save it first.
    fn drops(&self) -> String {
        let shown: Vec<String> = self
            .dirty
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        match &self.saves_to {
            Some(dir) if !shown.is_empty() => format!(
                "which drops the changes that are not committed in the repo, after copying {} to {}",
                shown.join(", "),
                dir.display()
            ),
            _ => "which drops the changes that are not committed in the repo".to_owned(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bin = crate::name::bin();
        let marker = self.repo.join(".git").join(PULL_MARKER);
        let marker = shell_words::quote(&marker.to_string_lossy()).into_owned();
        write!(
            f,
            "an earlier `{bin} sync` was stopped while it finished a pull in {}, and dotman cannot tell how to put the repo back: {}. Nothing was reset.",
            self.repo.display(),
            self.why
        )?;
        let head = self.head.as_deref().unwrap_or("no commit");
        let mut steps = match (&self.marker, &self.detached_from) {
            (Some((branch, old)), _) => {
                write!(
                    f,
                    " Before that pull, {branch} was at {old}; HEAD is now at {head}. To put the repo on {branch} as it is now, {}:",
                    self.drops()
                )?;
                self.checkout_steps(branch)
            }
            (None, Some(branch)) => {
                write!(
                    f,
                    " HEAD is detached at {head}. To put the repo on {branch} as it is now, {}, then remove the marker and sync:",
                    self.drops()
                )?;
                self.checkout_steps(branch)
            }
            (None, None) => {
                write!(f, " To go on, remove the marker and sync:")?;
                Vec::new()
            }
        };
        steps.push(format!("rm {marker}"));
        steps.push(format!("{bin} sync"));
        for (n, step) in (1_u32..).zip(steps) {
            write!(f, "\n  {n}. {step}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Refusal {}

/// What the marker holds: the pull being replayed, and what [`Git::recover_pull`] needs to put
/// the repo back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Marker {
    branch: String,
    old: String,
    onto: String,
    decisions: BTreeMap<PathBuf, Decision>,
    /// The untracked files before the replay; a 2.25 rebase leaves its renamed copies as new ones.
    untracked: BTreeSet<PathBuf>,
}

impl Marker {
    /// NUL-terminated fields, so a path may hold any byte but NUL.
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut field = |bytes: &[u8]| {
            out.extend_from_slice(bytes);
            out.push(0);
        };
        field(MARKER_VERSION);
        for value in [&self.branch, &self.old, &self.onto] {
            field(value.as_bytes());
        }
        for (path, decision) in &self.decisions {
            field(b"D");
            match decision {
                Decision::Set(entry) => {
                    field(entry.mode.as_bytes());
                    field(entry.oid.as_bytes());
                }
                Decision::Absent => {
                    field(b"-");
                    field(b"-");
                }
            }
            field(path.as_os_str().as_bytes());
        }
        for path in &self.untracked {
            field(b"U");
            field(path.as_os_str().as_bytes());
        }
        out
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut fields = bytes.strip_suffix(b"\0")?.split(|b| *b == 0);
        if fields.next()? != MARKER_VERSION {
            return None;
        }
        let mut text = || String::from_utf8(fields.next()?.to_vec()).ok();
        let (branch, old, onto) = (text()?, text()?, text()?);
        checked_branch(&branch).ok()?;
        if !is_oid(&old) || !is_oid(&onto) {
            return None;
        }
        let mut marker = Self {
            branch,
            old,
            onto,
            decisions: BTreeMap::new(),
            untracked: BTreeSet::new(),
        };
        while let Some(kind) = fields.next() {
            match kind {
                b"D" => {
                    let (mode, oid, path) = (fields.next()?, fields.next()?, fields.next()?);
                    let decision = if mode == b"-" && oid == b"-" {
                        Decision::Absent
                    } else {
                        Decision::Set(checked_entry(mode, oid)?)
                    };
                    marker.decisions.insert(path_from(path), decision);
                }
                b"U" => {
                    marker.untracked.insert(path_from(fields.next()?));
                }
                _ => return None,
            }
        }
        Some(marker)
    }
}

/// A git object id: 40 or 64 lowercase hex digits.
fn is_oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `mode` and `oid` as an [`Entry`], when each is what git writes; nothing else reaches the
/// `--index-info` stream.
fn checked_entry(mode: &[u8], oid: &[u8]) -> Option<Entry> {
    let mode = std::str::from_utf8(mode).ok()?;
    let oid = std::str::from_utf8(oid).ok()?;
    (mode.len() == 6 && mode.bytes().all(|b| b.is_ascii_digit()) && is_oid(oid)).then(|| Entry {
        mode: mode.to_owned(),
        oid: oid.to_owned(),
    })
}

/// A repo path whose parent in the worktree is a symlink: git 2.25's `restore` and every
/// version's `rm` delete through one, so dotman never settles, removes or restores such a path.
#[derive(Debug)]
struct UnderSymlink(PathBuf);

impl fmt::Display for UnderSymlink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} lies under a symlink in the repo, so dotman does not write it",
            self.0.display()
        )
    }
}

impl std::error::Error for UnderSymlink {}

/// Result of [`Git::pull_rebase`].
#[derive(Debug, PartialEq, Eq)]
pub enum PullOutcome {
    /// The pull and rebase finished.
    Ok,
    /// The rebase hit conflicts and was aborted; the working tree is as before the pull.
    Conflict {
        /// The paths of the first stop, in git's order.
        stopped: Vec<Stopped>,
    },
    /// The pull finished, but the uncommitted edits it set aside did not go back cleanly: they
    /// are kept in the stash, and the worktree may hold conflict markers.
    Unstashed {
        /// Unmerged paths in the index, repo-relative; empty when the stash was kept without them.
        paths: Vec<PathBuf>,
    },
}

/// An unfinished git operation that blocks dotman commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// `.git/rebase-merge` or `.git/rebase-apply` exists.
    Rebase,
    /// `.git/MERGE_HEAD` exists.
    Merge,
    /// `.git/CHERRY_PICK_HEAD` exists, or `.git/sequencer` is picking.
    CherryPick,
    /// `.git/REVERT_HEAD` exists, or `.git/sequencer` is reverting.
    Revert,
    /// `.git/BISECT_LOG` exists: the worktree holds an older commit.
    Bisect,
}

impl Operation {
    /// The git subcommand that continues or aborts it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
        }
    }
}

/// Why a command that talks to a remote failed, as far as git's message tells. It is the context of
/// the error, so a caller can say what the failure means for its own run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFailure {
    /// The remote could not be reached or read.
    Unreachable,
    /// The push was refused because the remote has commits the branch lacks.
    Moved,
}

impl fmt::Display for RemoteFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unreachable => "could not reach the remote",
            Self::Moved => "the remote has commits that are not here",
        })
    }
}

impl RemoteFailure {
    /// What git's `stderr` says about the remote. Git's messages are matched in English, so in
    /// another locale the failure keeps git's text alone.
    fn read(stderr: &str) -> Option<Self> {
        const UNREACHABLE: [&str; 3] = [
            "Could not read from remote repository",
            "unable to access '",
            "remote end hung up unexpectedly",
        ];
        if stderr.contains("[rejected]")
            && (stderr.contains("(fetch first)") || stderr.contains("(non-fast-forward)"))
        {
            Some(Self::Moved)
        } else if UNREACHABLE.iter().any(|text| stderr.contains(text)) {
            Some(Self::Unreachable)
        } else {
            None
        }
    }
}

/// Which `git status --porcelain=v2` record an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// A changed tracked path (`1` record).
    Ordinary,
    /// A renamed or copied path (`2` record).
    Renamed,
    /// A path with unresolved conflicts (`u` record).
    Unmerged,
    /// A path git does not track: a `?` record, or a `!` one that a `.gitignore` matches, which
    /// [`Git::stage_all_except`] stages all the same.
    Untracked,
}

/// One parsed `git status --porcelain=v2 -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// The record type this entry was parsed from.
    pub kind: StatusKind,
    /// Two-letter staged/unstaged code such as `M.`; `??` for untracked, `!!` for ignored.
    pub xy: String,
    /// Relative to the repo root; raw bytes, not necessarily UTF-8.
    pub path: PathBuf,
    /// Set only for [`StatusKind::Renamed`].
    pub orig_path: Option<PathBuf>,
}

/// One blob, symlink or gitlink from [`Git::ls_tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// `100644`, `100755`, `120000`, or `160000` for a submodule.
    pub mode: String,
    /// Relative to the tree root; raw bytes, not necessarily UTF-8.
    pub path: PathBuf,
}

/// One commit from [`Git::log`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Full commit hash.
    pub oid: String,
    /// Author time, seconds since the Unix epoch.
    pub time: i64,
    /// First line of the message.
    pub subject: String,
}

impl Git {
    /// A handle on `repo`. `dot_command` names the dotman command in the missing-git error.
    #[must_use]
    pub fn new(repo: impl Into<PathBuf>, dot_command: &'static str) -> Self {
        Self {
            repo: repo.into(),
            dot_command,
            env: Vec::new(),
            version_checked: OnceLock::new(),
            leave_out: None,
            backups: None,
        }
    }

    /// Sets what [`Git::status`] hides and [`Git::stage_all_except`] leaves out.
    #[must_use]
    pub fn leaving_out(mut self, pick: LeaveOut) -> Self {
        self.leave_out = Some(pick);
        self
    }

    /// Sets the backups directory a run dir is made in for what [`Git::recover_pull`] saves.
    /// Without one, recovery refuses rather than drop an uncommitted edit.
    #[must_use]
    pub fn backing_up_into(mut self, backups: PathBuf) -> Self {
        self.backups = Some(backups);
        self
    }

    /// Adds environment variables to every git process; a later pair overrides an earlier one.
    #[must_use]
    pub fn with_env<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env
            .extend(vars.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// The path given to [`Git::new`], not canonicalised.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// A `git -C <repo>` command with the isolation flags, for callers that spawn it themselves.
    /// It does not check the git version.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut cmd = self.command_in(&self.repo);
        // Named outright, so git never searches: a `.git` without a usable HEAD is an error here,
        // not a reason to look in the parent directories, where the user's own repo may be.
        let git_dir = self.repo.join(".git");
        if fs::symlink_metadata(&git_dir).is_ok() {
            let mut dir = OsString::from("--git-dir=");
            dir.push(&git_dir);
            let mut tree = OsString::from("--work-tree=");
            tree.push(&self.repo);
            cmd.arg(dir).arg(tree);
        }
        cmd
    }

    fn command_in(&self, dir: &Path) -> Command {
        let mut cmd = self.base();
        cmd.arg("-C").arg(dir);
        cmd
    }

    /// For a call that needs no repo, such as `config --global` or `ls-remote`. Git would still
    /// look for one from dotman's working directory and fail on a broken `.git` there, as a
    /// gitfile whose target is gone; a `--git-dir` that is no repo makes it run with none, on git
    /// 2.25 as on later ones.
    fn repoless(&self) -> Command {
        let mut cmd = self.base();
        cmd.arg("--git-dir=/dev/null");
        cmd
    }

    fn base(&self) -> Command {
        let mut cmd = crate::proc::command("git");
        for var in LOCAL_REPO_ENV {
            cmd.env_remove(var);
        }
        // Where git does search, as for a repo with no `.git` yet, it stops at the repo's
        // parent. Git compares the real path, so a symlinked repo path is resolved first.
        let real = fs::canonicalize(&self.repo).unwrap_or_else(|_| self.repo.clone());
        if let Some(parent) = real.parent().filter(|p| p.is_absolute()) {
            cmd.env("GIT_CEILING_DIRECTORIES", parent);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.args(ISOLATION);
        cmd
    }

    /// Fails unless the installed git is at least 2.25. Checked once per handle.
    pub fn check_version(&self) -> Result<()> {
        if self.version_checked.get().is_some() {
            return Ok(());
        }
        let mut cmd = self.base();
        cmd.arg("--version");
        let out = self.spawn_bounded(cmd, &["--version"])?;
        if !out.status.success() {
            return Err(self.failure(&["--version"], &out));
        }
        check_version_text(&String::from_utf8_lossy(&out.stdout))?;
        let _ = self.version_checked.set(());
        Ok(())
    }

    /// The installed git's `(major, minor, patch)`.
    pub fn version(&self) -> Result<(u32, u32, u32)> {
        let mut cmd = self.base();
        cmd.arg("--version");
        let out = self.spawn_bounded(cmd, &["--version"])?;
        let line = String::from_utf8_lossy(&out.stdout);
        parse_version(&line)
            .ok_or_else(|| anyhow!("could not read the git version from {:?}", line.trim()))
    }

    fn spawn(&self, cmd: Command) -> Result<Output> {
        self.spawn_with(cmd, Stdio::null())
    }

    fn spawn_with(&self, mut cmd: Command, stdin: Stdio) -> Result<Output> {
        cmd.stdin(stdin);
        cmd.output().map_err(|err| self.spawn_error(err))
    }

    /// Like [`Git::spawn`], killing git after [`PROBE_TIMEOUT`]. Only for short local commands;
    /// one that talks to a remote or walks the history may take as long as it needs.
    fn spawn_bounded<S: AsRef<OsStr>>(&self, mut cmd: Command, args: &[S]) -> Result<Output> {
        crate::proc::output_within(&mut cmd, PROBE_TIMEOUT)
            .map_err(|err| self.spawn_error(err))?
            .ok_or_else(|| {
                let label: Vec<String> = args
                    .iter()
                    .map(|a| a.as_ref().to_string_lossy().into_owned())
                    .collect();
                anyhow!(
                    "`git {}` did not finish within {} s; check that git runs at all, for example with `git --version`",
                    label.join(" "),
                    PROBE_TIMEOUT.as_secs()
                )
            })
    }

    fn spawn_error(&self, err: io::Error) -> anyhow::Error {
        if err.kind() == io::ErrorKind::NotFound {
            anyhow!(
                "`{} {}` needs git, but no `git` executable was found in PATH. Install it with: {}",
                crate::name::bin(),
                self.dot_command,
                install_hint(cfg!(target_os = "macos"), read_os_release().as_deref())
            )
        } else {
            anyhow!(err).context("failed to run git")
        }
    }

    /// [`failure`], with one line in front naming the repo file when git's error says the index
    /// or `HEAD` is corrupt, or that a lock file is in the way.
    fn failure<S: AsRef<OsStr>>(&self, args: &[S], out: &Output) -> anyhow::Error {
        let err = failure(args, out);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if let Some(lock) = stderr
            .split_once("Unable to create '")
            .and_then(|(_, rest)| rest.split_once("': File exists"))
            .map(|(lock, _)| shell_words::quote(lock).into_owned())
        {
            return anyhow!(
                "git's lock file {lock} is in the repo. Another git is running in {}, or one was killed and left it behind; if no git is running, remove it with `rm {lock}` and run this again\n{err}",
                self.repo.display()
            );
        }
        let git_dir = self.repo.join(".git");
        let index = git_dir.join("index");
        let broken = if CORRUPT_INDEX.iter().any(|said| stderr.contains(said)) {
            format!("{} is corrupt", index.display())
        } else if stderr.contains("not a git repository") && git_dir.is_dir() {
            format!("{} is missing or corrupt", git_dir.join("HEAD").display())
        } else if stderr.contains("unable to read") {
            format!(
                "an object that {} or HEAD names cannot be read",
                index.display()
            )
        } else {
            return err;
        };
        err.context(format!(
            "{broken}; `{} doctor` checks the repo",
            crate::name::bin()
        ))
    }

    /// [`Git::failure`] of a command that talks to a remote, with [`RemoteFailure`] as its
    /// context when git's message tells why.
    fn remote_failure<S: AsRef<OsStr>>(&self, args: &[S], out: &Output) -> anyhow::Error {
        let err = self.failure(args, out);
        match RemoteFailure::read(&String::from_utf8_lossy(&out.stderr)) {
            Some(why) => err.context(why),
            None => err,
        }
    }

    fn run_remote<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Output> {
        let out = self.probe(args)?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(self.remote_failure(args, &out))
        }
    }

    fn output<S: AsRef<OsStr>>(&self, mut cmd: Command, args: &[S]) -> Result<Output> {
        self.check_version()?;
        cmd.args(args);
        self.spawn(cmd)
    }

    fn run_with<S: AsRef<OsStr>>(&self, cmd: Command, args: &[S]) -> Result<Output> {
        let out = self.output(cmd, args)?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(self.failure(args, &out))
        }
    }

    /// Like [`Git::probe`], bounded by [`PROBE_TIMEOUT`].
    fn probe_bounded<S: AsRef<OsStr>>(&self, mut cmd: Command, args: &[S]) -> Result<Output> {
        self.check_version()?;
        cmd.args(args);
        self.spawn_bounded(cmd, args)
    }

    fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Output> {
        self.run_with(self.command(), args)
    }

    fn probe<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Output> {
        self.output(self.command(), args)
    }

    /// Stdout of a short command whose exit status 1 means "no answer".
    fn optional<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Option<String>> {
        let out = self.probe_bounded(self.command(), args)?;
        match out.status.code() {
            Some(0) => Ok(Some(text(&out))),
            Some(1) => Ok(None),
            _ => Err(self.failure(args, &out)),
        }
    }

    /// Creates the repo with `main` as the unborn branch. Safe to rerun: an existing branch and
    /// repo id are kept.
    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.repo)
            .with_context(|| format!("failed to create {}", self.repo.display()))?;
        self.run(&["init", "-q", NO_TEMPLATE])?;
        if self.head()?.is_none() {
            self.run(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
        }
        self.write_metadata()
    }

    /// Clones `src` into this handle's repo path and populates the working tree.
    ///
    /// Attributes are written before checkout so a tracked `.gitattributes` cannot transform
    /// content. A local `src` with an `origin` remote passes that URL on as the clone's `origin`,
    /// as [`Git::remote_target`] reads it in `src`, unless it is not a [`usable_remote`]. When
    /// `src`'s `HEAD` names a branch with no commits, see [`Git::adopt_unborn_head`].
    pub fn clone_from(&self, src: &str) -> Result<Option<Adopted>> {
        self.clone_with(src, &[])?;
        self.adopt_unborn_head(src)
    }

    /// The remote's `HEAD` names a branch with no commits, which a clone leaves unborn while the
    /// remote's other branches arrive as `refs/remotes/origin/*`. Puts `HEAD` on the one such
    /// branch, or on `main` among several, and checks it out; `None` when `HEAD` is born or the
    /// remote has no branch. Refuses several branches without `main`, and a branch name
    /// [`checked_branch`] refuses, and a repo with changes that are not committed, before it
    /// changes anything: the checkout overwrites. Reads only the local refs.
    pub fn adopt_unborn_head(&self, src: &str) -> Result<Option<Adopted>> {
        if self.head()?.is_some() {
            return Ok(None);
        }
        let listed = self.run(&[
            "for-each-ref",
            "--format=%(refname)%00",
            "refs/remotes/origin/",
        ])?;
        let branches: Vec<String> = listed
            .stdout
            .split(|b| *b == 0)
            .filter_map(|name| {
                let name = String::from_utf8_lossy(name);
                let branch = name.trim().strip_prefix("refs/remotes/origin/")?;
                (branch != "HEAD").then(|| branch.to_owned())
            })
            .collect();
        let shown = redact_credentials(src);
        let branch = match branches.as_slice() {
            [] => return Ok(None),
            [only] => checked_branch(only).map_err(|unusable| {
                anyhow!(
                    "{shown}'s HEAD names a branch with no commits, and its only branch is one whose {}, so dotman does not check it out. Nothing was cloned. Give its commits a branch named main in the remote and run this again",
                    unusable.why()
                )
            })?,
            several if several.iter().any(|name| name == "main") => "main".to_owned(),
            several => bail!(
                "{shown}'s HEAD names a branch with no commits, and none of its {} branches is main ({}), so nothing says which one holds your dotfiles. Nothing was cloned. Make that branch the remote's default branch and run this again",
                several.len(),
                branch_names(several)
            ),
        };
        let dirty = self.run(&["status", "--porcelain", "-z", "--untracked-files=all"])?;
        ensure!(
            dirty.stdout.is_empty(),
            "{} has changes that are not committed, so dotman does not check out {branch} over them; commit or move them, then run this again",
            self.repo.display()
        );
        let full = full_ref(&branch);
        self.run(&[
            "update-ref",
            full.as_str(),
            &format!("refs/remotes/origin/{branch}"),
            "",
        ])?;
        self.run(&["symbolic-ref", "HEAD", full.as_str()])?;
        self.checkout_head()?;
        Ok(Some(Adopted {
            branch,
            of: branches.len(),
        }))
    }

    /// [`Git::clone_from`], bringing only the branch `src`'s `HEAD` names, with no tags: an
    /// import takes that history and nothing else.
    pub fn clone_head_from(&self, src: &str) -> Result<()> {
        self.clone_with(src, &["--single-branch", "--no-tags"])
    }

    fn clone_with(&self, src: &str, refs: &[&str]) -> Result<()> {
        let src_origin = if Path::new(src).is_dir() {
            Self::new(src, self.dot_command)
                .with_env(self.env.clone())
                .remote_target("origin")?
        } else {
            None
        };
        // A local path otherwise takes git's shortcut that copies or hardlinks the source's
        // object store whole, the subject of the 2024 local-clone CVEs, which the 2.25 floor
        // does not rule out; through the transport, only the wanted history arrives.
        let mut args: Vec<&OsStr> = ["clone", "-q", "--no-checkout", "--no-local", NO_TEMPLATE]
            .map(OsStr::new)
            .to_vec();
        args.extend(refs.iter().map(OsStr::new));
        args.extend(["--", src].map(OsStr::new));
        args.push(self.repo.as_os_str());
        self.run_with(self.base(), &args)?;
        self.write_metadata()?;
        if let Some(url) = src_origin.filter(|url| usable_remote(url)) {
            self.set_remote_url("origin", &url)?;
        }
        if self.head()?.is_some() {
            self.checkout_head()?;
        }
        Ok(())
    }

    /// Makes the index and working tree match `HEAD`, overwriting files in the way.
    pub fn checkout_head(&self) -> Result<()> {
        self.run(&["checkout", "-q", "-f", "HEAD"]).map(drop)
    }

    /// Puts `path`, repo-relative and taken literally, back in the index and the worktree as
    /// `rev` has it.
    pub fn checkout_path(&self, rev: &str, path: &Path) -> Result<()> {
        let spec = pathspec(":(literal)", path);
        let args = [
            OsStr::new("checkout"),
            OsStr::new("-q"),
            OsStr::new(rev),
            OsStr::new("--"),
            &spec,
        ];
        self.run(&args).map(drop)
    }

    /// Feeds a `git fast-import` stream that ends in `done` to git. Git moves the refs the stream
    /// names only after it has read the whole stream. A file, not a pipe: if git exits early, a
    /// write to a closed pipe would kill `dotman` with SIGPIPE.
    pub fn fast_import(&self, stream: File) -> Result<()> {
        self.check_version()?;
        let args = ["fast-import", "--quiet", "--done"];
        let mut cmd = self.command();
        cmd.args(args);
        let out = self.spawn_with(cmd, Stdio::from(stream))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(self.failure(&args, &out))
        }
    }

    fn write_metadata(&self) -> Result<()> {
        let git_dir = self.repo.join(".git");
        self.refresh_attributes()?;
        let id_path = git_dir.join("dotman-id");
        if !id_path.exists() {
            fs::write(&id_path, self.new_repo_id()?).context("failed to write .git/dotman-id")?;
        }
        Ok(())
    }

    fn attributes_path(&self) -> PathBuf {
        self.repo.join(".git/info/attributes")
    }

    /// Whether the last line of `.git/info/attributes` that is not blank is the one `init` and
    /// every clone write, so that it wins over the user's lines above it. An older dotman wrote a
    /// line without `merge`, or with `merge=text`.
    #[must_use]
    pub fn attributes_current(&self) -> bool {
        fs::read(self.attributes_path()).is_ok_and(|bytes| pinned_last(&bytes))
    }

    /// Unless [`Git::attributes_current`], writes `.git/info/attributes` again with the user's
    /// lines in their order, the lines dotman wrote dropped and the pinned line last; returns
    /// whether it wrote. A file that cannot be read is an error, so its lines are never lost.
    pub fn refresh_attributes(&self) -> Result<bool> {
        let path = self.attributes_path();
        let old = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        if pinned_last(&old) {
            return Ok(false);
        }
        let info = self.repo.join(".git/info");
        fs::create_dir_all(&info)
            .with_context(|| format!("failed to create {}", info.display()))?;
        write_atomic(&path, &with_pinned_last(&old), 0o644)
            .context("failed to write .git/info/attributes")?;
        Ok(true)
    }

    /// Why [`Git::refresh_attributes`] cannot write, judged without trying, since `doctor` writes
    /// nothing: the file cannot be read, or `.git/info`, or `.git` while it is missing, is not a
    /// directory this user may write to. `None` when nothing stands in the way.
    #[must_use]
    pub fn attributes_unwritable(&self) -> Option<String> {
        if let Err(e) = fs::read(self.attributes_path())
            && e.kind() != io::ErrorKind::NotFound
        {
            return Some(format!("it cannot be read: {e}"));
        }
        let info = self.repo.join(".git/info");
        let dir = if fs::symlink_metadata(&info).is_ok() {
            info
        } else {
            self.repo.join(".git")
        };
        let writable = fs::metadata(&dir).is_ok_and(|meta| {
            meta.is_dir() && meta.uid() == crate::sys::geteuid() && meta.mode() & 0o300 == 0o300
        });
        (!writable).then(|| format!("{} is not a directory you can write to", dir.display()))
    }

    fn new_repo_id(&self) -> Result<String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();
        let mut bytes = self.repo.as_os_str().as_bytes().to_vec();
        bytes.extend_from_slice(&nanos.to_le_bytes());
        bytes.extend_from_slice(&std::process::id().to_le_bytes());
        Ok(format!("{:032x}", xxh3_128(&bytes)))
    }

    /// Stages every change, including paths ignored by `.gitignore`.
    pub fn stage_all(&self) -> Result<()> {
        self.stage(&[])
    }

    /// Like [`Git::stage_all`], leaving out `held`, repo-relative paths taken literally, and the
    /// untracked paths the [`LeaveOut`] picks. A tracked path is staged whatever it picks.
    pub fn stage_all_except(&self, held: &[PathBuf]) -> Result<()> {
        let mut left_out = self.left_out(&self.full_status()?)?;
        left_out.extend_from_slice(held);
        self.stage(&left_out)
    }

    fn stage(&self, left_out: &[PathBuf]) -> Result<()> {
        // On stdin: tens of thousands of left-out paths as arguments overflow the kernel's
        // argument limit, and `git add` fails with E2BIG.
        let mut specs: Vec<u8> = Vec::new();
        let left_out = left_out
            .iter()
            .map(|path| pathspec(":(exclude,literal)", path));
        for spec in [".", TEMP_FILES, TEMP_DIRS]
            .map(OsString::from)
            .into_iter()
            .chain(left_out)
        {
            specs.extend_from_slice(spec.as_bytes());
            specs.push(0);
        }
        // In `.git`, not TMPDIR: a missing or read-only TMPDIR must not fail a sync after its
        // captures were made.
        let dir = self.repo.join(".git");
        let mut input = tempfile::tempfile_in(&dir)
            .with_context(|| format!("cannot create a temp file in {}", dir.display()))?;
        input
            .write_all(&specs)
            .and_then(|()| input.rewind())
            .context("cannot write a temp file")?;
        let args = [
            "add",
            "-A",
            "-f",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ];
        self.check_version()?;
        let mut cmd = self.command();
        cmd.args(args);
        let out = self.spawn_with(cmd, Stdio::from(input))?;
        if !out.status.success() {
            return Err(self.failure(&args, &out));
        }
        // Untracks the leftovers that earlier versions committed.
        let args = [
            "rm",
            "-r",
            "-q",
            "--cached",
            "--ignore-unmatch",
            "--",
            TRACKED_TEMP_FILES,
            TRACKED_TEMP_DIRS,
        ];
        self.run(&args).map(drop)
    }

    /// Takes `paths` out of the index again, back to what `HEAD` has.
    pub fn unstage(&self, paths: &[PathBuf]) -> Result<()> {
        let mut args: Vec<OsString> = if self.head()?.is_some() {
            ["reset", "-q", "--"].map(OsString::from).to_vec()
        } else {
            ["rm", "-q", "--cached", "--ignore-unmatch", "--"]
                .map(OsString::from)
                .to_vec()
        };
        args.extend(paths.iter().map(|path| pathspec(":(literal)", path)));
        self.run(&args).map(drop)
    }

    /// The index entries of `paths`, taken literally, as `(mode, object id, path)`, for
    /// [`Git::restore_index`]. Only stage 0: a path with conflicts has no single entry.
    pub fn index_entries(&self, paths: &[PathBuf]) -> Result<Vec<(String, String, PathBuf)>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<OsString> = ["ls-files", "-s", "-z", "--"].map(OsString::from).to_vec();
        args.extend(paths.iter().map(|path| pathspec(":(literal)", path)));
        let out = self.run(&args)?;
        // One `mode oid stage<TAB>path` record per entry.
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter_map(|record| {
                let (meta, path) =
                    record.split_at_checked(record.iter().position(|b| *b == b'\t')?)?;
                let mut fields = meta.split(|b| *b == b' ');
                let mode = String::from_utf8_lossy(fields.next()?).into_owned();
                let oid = String::from_utf8_lossy(fields.next()?).into_owned();
                let path = path_from(path.get(1..)?);
                (fields.next()? == b"0").then_some((mode, oid, path))
            })
            .collect())
    }

    /// Makes the index hold `entries` from [`Git::index_entries`] again, and none for the rest
    /// of `paths`. The worktree is left alone.
    pub fn restore_index(
        &self,
        paths: &[PathBuf],
        entries: &[(String, String, PathBuf)],
    ) -> Result<()> {
        let gone: Vec<OsString> = paths
            .iter()
            .filter(|path| !entries.iter().any(|(_, _, kept)| kept == *path))
            .map(|path| pathspec(":(literal)", path))
            .collect();
        if !gone.is_empty() {
            let mut args: Vec<OsString> = ["rm", "-q", "--cached", "--ignore-unmatch", "--"]
                .map(OsString::from)
                .to_vec();
            args.extend(gone);
            self.run(&args)?;
        }
        if entries.is_empty() {
            return Ok(());
        }
        let mut args = vec![OsString::from("update-index"), OsString::from("--add")];
        for (mode, oid, path) in entries {
            let mut info = OsString::from(format!("{mode},{oid},"));
            info.push(path);
            args.extend([OsString::from("--cacheinfo"), info]);
        }
        self.run(&args).map(drop)
    }

    /// Commits the staged changes; `None` when nothing is staged.
    pub fn commit(&self, message: &str) -> Result<Option<String>> {
        let args = ["diff", "--cached", "--quiet", "--no-ext-diff"];
        let staged = self.probe(&args)?;
        match staged.status.code() {
            Some(0) => return Ok(None),
            Some(1) => {}
            _ => return Err(self.failure(&args, &staged)),
        }
        self.run(&["commit", "-q", "-m", message])?;
        self.head()
    }

    /// Parsed `git status --porcelain=v2`, including every untracked file, ignored ones too,
    /// except those the [`LeaveOut`] picks. Never rewrites the index, so it is safe in a dry run.
    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        let entries = self.full_status()?;
        let left_out: BTreeSet<PathBuf> = self.left_out(&entries)?.into_iter().collect();
        Ok(entries
            .into_iter()
            .filter(|entry| {
                entry.kind != StatusKind::Untracked
                    || !entry.path.ancestors().any(|root| left_out.contains(root))
            })
            .collect())
    }

    /// The untracked paths of `entries` that the [`LeaveOut`] picks; none without one.
    fn left_out(&self, entries: &[StatusEntry]) -> Result<Vec<PathBuf>> {
        let untracked: Vec<PathBuf> = entries
            .iter()
            .filter(|entry| entry.kind == StatusKind::Untracked)
            .map(|entry| entry.path.clone())
            .collect();
        match &self.leave_out {
            Some(pick) if !untracked.is_empty() => pick(self, &untracked),
            _ => Ok(Vec::new()),
        }
    }

    /// Lists what a tracked `.gitignore` ignores, since staging force-adds it: every check on
    /// untracked files must see it. `traditional` with `--untracked-files=all` lists each file
    /// under an ignored directory, where `matching` lists only the directory, which the content
    /// guard cannot read.
    fn full_status(&self) -> Result<Vec<StatusEntry>> {
        let mut cmd = self.command();
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        let args = [
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignored=traditional",
            "--ignore-submodules=all",
        ];
        parse_status(&self.run_with(cmd, &args)?.stdout)
    }

    /// The commit `HEAD` points at; `None` on an unborn branch.
    pub fn head(&self) -> Result<Option<String>> {
        self.optional(&["rev-parse", "--verify", "-q", "HEAD"])
    }

    /// The branch `HEAD` points at, also when it is unborn; `None` when `HEAD` is detached. Fails
    /// with [`UnusableBranch`] on a name [`checked_branch`] refuses.
    pub fn branch(&self) -> Result<Option<String>> {
        let Some(name) = self.optional(&["symbolic-ref", "-q", "HEAD"])? else {
            return Ok(None);
        };
        let short = name.strip_prefix("refs/heads/").unwrap_or(&name);
        match checked_branch(short) {
            Ok(branch) => Ok(Some(branch)),
            Err(mut unusable) => {
                unusable.unborn = self.head()?.is_none();
                Err(unusable.into())
            }
        }
    }

    /// Raw bytes of `path` as stored in `rev`, with no filters or text conversion.
    pub fn show_blob(&self, rev: &str, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        let mut spec = OsString::from(rev);
        spec.push(":");
        spec.push(path.as_ref());
        let out = self.run(&[OsStr::new("cat-file"), OsStr::new("blob"), &spec])?;
        Ok(out.stdout)
    }

    /// Raw bytes of the blob `oid`.
    pub fn blob(&self, oid: &str) -> Result<Vec<u8>> {
        ensure!(is_oid(oid), "not a git object id: {oid}");
        Ok(self.run(&["cat-file", "blob", oid])?.stdout)
    }

    /// The commit `rev` names, or `None` when it is not a revision.
    pub fn resolve_commit(&self, rev: &str) -> Result<Option<String>> {
        if rev.starts_with('-') {
            return Ok(None);
        }
        self.optional(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
    }

    /// The git file mode of `path` in `rev` (`100644`, `100755`, `120000`); `None` when absent.
    pub fn tree_mode(&self, rev: &str, path: &str) -> Result<Option<String>> {
        let out = self.run(&["ls-tree", "-z", rev, "--", path])?;
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter_map(|record| record.split_at_checked(record.iter().position(|b| *b == b'\t')?))
            .find(|(_, rest)| rest.get(1..) == Some(path.as_bytes()))
            .and_then(|(meta, _)| meta.split(|b| *b == b' ').next())
            .map(|mode| String::from_utf8_lossy(mode).into_owned()))
    }

    /// Commits reachable from `HEAD`, newest first, touching `paths` when given.
    pub fn log(&self, paths: &[&str], limit: Option<usize>) -> Result<Vec<LogEntry>> {
        if self.head()?.is_none() {
            return Ok(Vec::new());
        }
        let mut args = vec![
            "log".to_owned(),
            "--no-color".to_owned(),
            "--no-ext-diff".to_owned(),
            "-z".to_owned(),
            "--format=%H%x00%at%x00%s".to_owned(),
        ];
        if let Some(limit) = limit {
            args.push(format!("--max-count={limit}"));
        }
        args.push("HEAD".to_owned());
        args.push("--".to_owned());
        args.extend(paths.iter().map(|p| (*p).to_owned()));
        let out = self.run(&args)?;
        parse_log(&out.stdout)
    }

    /// The newest commit on `HEAD` committed before `date`; `None` if there is none.
    pub fn rev_before(&self, date: &str) -> Result<Option<String>> {
        if self.head()?.is_none() {
            return Ok(None);
        }
        let out = self.run(&["rev-list", "-1", &format!("--before={date}"), "HEAD"])?;
        let oid = text(&out);
        Ok((!oid.is_empty()).then_some(oid))
    }

    /// True after `clone --depth`: history older than the cut is absent.
    pub fn is_shallow(&self) -> Result<bool> {
        let out = self.run(&["rev-parse", "--is-shallow-repository"])?;
        Ok(text(&out) == "true")
    }

    /// `git pull --rebase --autostash`. On conflict the rebase is aborted, the copies git 2.25
    /// left removed, and the paths returned.
    pub fn pull_rebase(&self, remote: &str, branch: &str) -> Result<PullOutcome> {
        let stash = ["rev-parse", "-q", "--verify", "refs/stash"];
        let stashed_before = self.optional(&stash)?;
        let untracked = self.untracked()?;
        let refname = full_ref(branch);
        let mut args = vec!["pull", "--rebase", "--autostash"];
        args.extend_from_slice(self.pull_backend()?);
        args.extend([remote, refname.as_str()]);
        let mut pinned = self.command();
        pinned.args(PULL_PINS);
        let mut out = self.output(pinned, &args)?;
        // git before 2.26 stops on a commit the pull leaves empty, which later gits drop.
        let mut last = None;
        while !out.status.success() && self.stopped_empty()? {
            let at = (self.head()?, self.resolve_commit("REBASE_HEAD")?);
            if last.as_ref() == Some(&at) {
                break;
            }
            last = Some(at);
            let mut cmd = self.command();
            cmd.args(PULL_PINS);
            out = self.output(cmd, &["rebase", "--skip"])?;
        }
        if out.status.success() {
            // git exits 0 when putting the autostash back conflicts, and keeps it in the stash.
            let paths = self.unmerged_paths()?;
            if !paths.is_empty() || self.optional(&stash)? != stashed_before {
                return Ok(PullOutcome::Unstashed { paths });
            }
            return Ok(PullOutcome::Ok);
        }
        if !self.operation_in_progress() {
            return Err(self.remote_failure(&args, &out));
        }
        let stopped = self.stopped(None, &untracked);
        let blobs = self.stop_blobs();
        self.run(&["rebase", "--abort"])?;
        let stopped = stopped?;
        self.remove_leftovers(&untracked, &blobs?)?;
        // A rebase that stopped with nothing in conflict hit something else, as a lock file in
        // `.git` stops it, and calling it a conflict would leave no step that helps.
        if stopped.is_empty() {
            let stopped = self.failure(&args, &out);
            return Err(match self.refuse_locks() {
                Err(locks) => anyhow!("{locks:#}\n{stopped:#}"),
                Ok(()) => stopped,
            });
        }
        Ok(PullOutcome::Conflict { stopped })
    }

    /// The unmerged paths of a stopped rebase with their stages, each marked when a stage is not
    /// what `HEAD` or `REBASE_HEAD` holds at the path, and with the copy git 2.25 set aside, a
    /// file not in `untracked` before the rebase. With `decided`, only the paths it lacks.
    fn stopped(
        &self,
        decided: Option<&BTreeMap<PathBuf, Decision>>,
        untracked: &BTreeSet<PathBuf>,
    ) -> Result<Vec<Stopped>> {
        let unmerged = self.unmerged_entries()?;
        if unmerged.is_empty() {
            return Ok(Vec::new());
        }
        let head = self.tree_entries("HEAD")?;
        let replayed = match self.resolve_commit("REBASE_HEAD")? {
            Some(commit) => Some(self.tree_entries(&commit)?),
            None => None,
        };
        let mut aside: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        let blobs = known_blobs(std::iter::once(&head).chain(&replayed), &unmerged);
        for (copy, stem) in self.leftovers(untracked, &blobs)? {
            let under_directory = unmerged.contains_key(&stem)
                && self.symlinked_parent(&stem)?.is_none()
                && fs::symlink_metadata(self.repo.join(&stem)).is_ok_and(|meta| meta.is_dir());
            if under_directory {
                aside.entry(stem).or_insert(copy);
            }
        }
        Ok(unmerged
            .into_iter()
            .filter(|(path, _)| decided.is_none_or(|decided| !decided.contains_key(path)))
            .map(|(path, [base, ours, theirs])| {
                let moved = ours.as_ref() != head.get(&path)
                    || replayed
                        .as_ref()
                        .is_some_and(|tree| theirs.as_ref() != tree.get(&path));
                Stopped {
                    aside: aside.remove(&path),
                    path,
                    base,
                    ours,
                    replayed: theirs,
                    follows_rename: moved,
                }
            })
            .collect())
    }

    /// The blobs a stopped rebase could have written at each path: its entries in `HEAD` and
    /// `REBASE_HEAD`, and its index stages.
    fn stop_blobs(&self) -> Result<BTreeMap<PathBuf, BTreeSet<String>>> {
        let mut trees = vec![self.tree_entries("HEAD")?];
        if let Some(commit) = self.resolve_commit("REBASE_HEAD")? {
            trees.push(self.tree_entries(&commit)?);
        }
        Ok(known_blobs(&trees, &self.unmerged_entries()?))
    }

    /// Whether a rebase stopped on a commit whose every change `HEAD` already holds, as git
    /// before 2.26 does where later gits drop the commit: nothing in conflict, nothing staged and
    /// no lock file. Skipping it loses nothing.
    fn stopped_empty(&self) -> Result<bool> {
        if self.rebase_dir().is_none()
            || !self.lock_files().is_empty()
            || !self.unmerged_entries()?.is_empty()
        {
            return Ok(false);
        }
        let Some(commit) = self.resolve_commit("REBASE_HEAD")? else {
            return Ok(false);
        };
        let Some(parent) = self.resolve_commit(&format!("{commit}^"))? else {
            return Ok(false);
        };
        let staged = ["diff", "--cached", "--quiet", "--no-ext-diff"];
        if self.probe(&staged)?.status.code() != Some(0) {
            return Ok(false);
        }
        let (before, after) = (self.tree_entries(&parent)?, self.tree_entries(&commit)?);
        let head = self.tree_entries("HEAD")?;
        Ok(before
            .keys()
            .chain(after.keys())
            .filter(|path| before.get(*path) != after.get(*path))
            .all(|path| head.get(path) == after.get(path)))
    }

    /// What `git pull --rebase` takes after `--rebase` to rebase with the merge backend whatever
    /// the user's `rebase.backend`, which git before 2.26 does not know: the git's default
    /// strategy, named. The apply backend merges against a base made of the patch's paths alone,
    /// so a file removed in origin that another file's content matches reads as renamed there,
    /// and the edit of it lands in that other file with no conflict.
    ///
    /// # Errors
    ///
    /// Fails when the git version cannot be read.
    pub fn pull_backend(&self) -> Result<&'static [&'static str]> {
        Ok(if self.version()? < (2, 34, 0) {
            &["-s", "recursive"]
        } else {
            &["-s", "ort"]
        })
    }

    /// `ls-files -u`: each unmerged path with its stages 1, 2 and 3.
    fn unmerged_entries(&self) -> Result<BTreeMap<PathBuf, [Option<Entry>; 3]>> {
        let out = self.run(&["ls-files", "-u", "-z"])?;
        let mut found: BTreeMap<PathBuf, [Option<Entry>; 3]> = BTreeMap::new();
        // One `mode oid stage<TAB>path` record per stage.
        for record in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let parsed = record
                .iter()
                .position(|b| *b == b'\t')
                .and_then(|tab| record.split_at_checked(tab))
                .and_then(|(meta, path)| {
                    let mut fields = meta.split(|b| *b == b' ');
                    let entry = checked_entry(fields.next()?, fields.next()?)?;
                    let stage = match fields.next()? {
                        b"1" => 0,
                        b"2" => 1,
                        b"3" => 2,
                        _ => return None,
                    };
                    Some((path_from(path.get(1..)?), stage, entry))
                });
            let Some((path, stage, entry)) = parsed else {
                bail!(
                    "unexpected git ls-files record: {}",
                    String::from_utf8_lossy(record)
                );
            };
            if let Some(slot) = found.entry(path).or_default().get_mut(stage) {
                *slot = Some(entry);
            }
        }
        Ok(found)
    }

    /// Every blob, symlink and gitlink of `rev` with its entry. A directory is no entry, so a
    /// path that is one in `rev` is absent from the map.
    pub fn tree_entries(&self, rev: &str) -> Result<BTreeMap<PathBuf, Entry>> {
        let out = self.run(&["ls-tree", "-r", "-z", "--full-tree", rev])?;
        // One `mode type oid<TAB>path` record per entry.
        out.stdout
            .split(|b| *b == 0)
            .filter(|record| !record.is_empty())
            .map(|record| {
                record
                    .iter()
                    .position(|b| *b == b'\t')
                    .and_then(|tab| record.split_at_checked(tab))
                    .and_then(|(meta, path)| {
                        let mut fields = meta.split(|b| *b == b' ');
                        let mode = fields.next()?;
                        let entry = checked_entry(mode, fields.nth(1)?)?;
                        Some((path_from(path.get(1..)?), entry))
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "unexpected git ls-tree record: {}",
                            String::from_utf8_lossy(record)
                        )
                    })
            })
            .collect()
    }

    /// Stores `bytes` as a blob, with no filter, and returns its id.
    pub fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        self.hash_object(bytes, true)
    }

    /// The blob id of `bytes`, with no filter; stored only when `store`.
    fn hash_object(&self, bytes: &[u8], store: bool) -> Result<String> {
        let args: &[&str] = if store {
            &["hash-object", "-w", "--no-filters", "--stdin"]
        } else {
            &["hash-object", "--no-filters", "--stdin"]
        };
        let out = self.run_stdin(args, bytes)?;
        let oid = text(&out);
        ensure!(is_oid(&oid), "git hash-object printed {oid:?}");
        Ok(oid)
    }

    /// `git merge-file -p` of `ours`, `base` and `theirs`, written under those names into `dir`,
    /// a private directory the caller owns. `side` takes that side of each clash, and the result
    /// then has no markers. The markers are plain whatever `merge.conflictStyle` the user set, and
    /// the labels are escaped. Returns the merged bytes and whether the merge was clean.
    pub fn merge_file(
        &self,
        dir: &Path,
        versions: [&[u8]; 3],
        labels: [&str; 3],
        side: Option<Side>,
    ) -> Result<(Vec<u8>, bool)> {
        let [ours, base, theirs] = versions;
        let names = ["ours", "base", "theirs"].map(|name| dir.join(name));
        for (name, bytes) in names.iter().zip([ours, base, theirs]) {
            fs::write(name, bytes).with_context(|| format!("cannot write {}", name.display()))?;
        }
        let mut args: Vec<OsString> = ["-c", "merge.conflictStyle=merge", "merge-file", "-p"]
            .map(OsString::from)
            .to_vec();
        if let Some(side) = side {
            args.push(OsString::from(match side {
                Side::Ours => "--ours",
                Side::Theirs => "--theirs",
            }));
        }
        for label in labels {
            args.push(OsString::from("-L"));
            args.push(OsString::from(
                crate::output::escape_line(label).into_owned(),
            ));
        }
        args.extend(names.into_iter().map(PathBuf::into_os_string));
        let out = self.probe(&args)?;
        // Exit 1 to 127 counts the clashes; above that, or a signal, is a failure.
        match out.status.code() {
            Some(0) => Ok((out.stdout, true)),
            Some(1..=127) => Ok((out.stdout, false)),
            _ => Err(self.failure(&args, &out)),
        }
    }

    /// Whether `.git/dotman-pull` exists: a replay was stopped before it finished.
    #[must_use]
    pub fn own_pull(&self) -> bool {
        fs::symlink_metadata(self.marker_path()).is_ok()
    }

    fn marker_path(&self) -> PathBuf {
        self.repo.join(".git").join(PULL_MARKER)
    }

    fn remove_marker(&self) -> Result<()> {
        let path = self.marker_path();
        fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))
    }

    /// Replays the local commits of `branch` onto `onto`, the commit a pull from `remote` fetched,
    /// settling every stop with `decisions`, then moves the branch there in one compare-and-swap.
    /// A settled path that still differs afterwards is corrected in one commit, `sync(<host>):
    /// resolve <paths>`. SIGINT, SIGTERM and SIGHUP are ignored while it runs, so only a kill
    /// leaves the marker for [`Git::recover_pull`]. A stop on a path with no decision undoes the
    /// replay and returns [`Replayed::Stopped`]; any other failure undoes it and fails, or fails
    /// with a [`Refusal`] when undoing it cannot.
    pub fn replay(
        &self,
        remote: &str,
        branch: &str,
        onto: &str,
        decisions: &BTreeMap<PathBuf, Decision>,
        host: &str,
    ) -> Result<Replayed> {
        let _ignored = crate::sys::ignore_signals(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]);
        let (marker, fork) = match self.start_replay(remote, branch, onto, decisions) {
            Ok(started) => started,
            Err(e) if self.own_pull() => return Err(self.undo(e)),
            Err(e) => return Err(e),
        };
        let replayed = self.finish_rebase(&marker, &fork).and_then(|stopped| {
            if stopped.is_none() {
                self.correct(&marker, host)?;
                self.move_branch(&marker)?;
            }
            Ok(stopped)
        });
        match replayed {
            Ok(Some(stopped)) => {
                self.undo_quietly()?;
                Ok(Replayed::Stopped(stopped))
            }
            Ok(None) => {
                self.run(&["symbolic-ref", "HEAD", &full_ref(&marker.branch)])?;
                self.remove_marker()?;
                Ok(Replayed::Done)
            }
            Err(e) => Err(self.undo(e)),
        }
    }

    /// Writes the marker, then detaches `HEAD` at the branch's tip. Returns the marker and the
    /// commit the replayed commits start after.
    fn start_replay(
        &self,
        remote: &str,
        branch: &str,
        onto: &str,
        decisions: &BTreeMap<PathBuf, Decision>,
    ) -> Result<(Marker, String)> {
        let branch = checked_branch(branch)?;
        let refname = full_ref(&branch);
        ensure!(
            self.optional(&["symbolic-ref", "-q", "HEAD"])?.as_deref() == Some(refname.as_str()),
            "HEAD is not on {branch}, so dotman does not replay the pull"
        );
        let old = self
            .resolve_commit(&refname)?
            .ok_or_else(|| anyhow!("{branch} has no commit to replay"))?;
        let onto = self
            .resolve_commit(onto)?
            .ok_or_else(|| anyhow!("{onto} is not a commit"))?;
        for decision in decisions.values() {
            if let Decision::Set(entry) = decision {
                ensure!(
                    checked_entry(entry.mode.as_bytes(), entry.oid.as_bytes()).is_some(),
                    "not a git entry: {} {}",
                    entry.mode,
                    entry.oid
                );
            }
        }
        let fork = self.fork_point(remote, &branch, &onto, &old)?;
        let marker = Marker {
            branch,
            old,
            onto,
            decisions: decisions.clone(),
            untracked: self.untracked()?,
        };
        write_atomic(&self.marker_path(), &marker.to_bytes(), 0o600)
            .context("cannot write the pull marker")?;
        self.run(&["checkout", "-q", "--detach"])?;
        Ok((marker, fork))
    }

    /// The commit `git pull --rebase` replays after: the fork point of the remote-tracking ref,
    /// as it computes it, so a force-pushed upstream picks the same commits; else the merge base.
    fn fork_point(&self, remote: &str, branch: &str, onto: &str, old: &str) -> Result<String> {
        let tracking = format!("refs/remotes/{remote}/{branch}");
        let out = self.probe(&["merge-base", "--fork-point", &tracking, old])?;
        let fork = text(&out);
        if out.status.success() && is_oid(&fork) {
            return Ok(fork);
        }
        self.merge_base(onto, old)?
            .ok_or_else(|| anyhow!("{branch} and {onto} have no commit in common"))
    }

    /// Runs the rebase to its end, settling each stop. `Some` when a stop has paths with no
    /// decision, which leaves the rebase stopped there.
    fn finish_rebase(&self, marker: &Marker, fork: &str) -> Result<Option<Vec<Stopped>>> {
        // `--merge`: see `Git::pull_backend` for why never the apply backend.
        let mut said = self.rebase(&["--merge", "--onto", &marker.onto, fork])?;
        let mut last = None;
        while self.rebase_dir().is_some() {
            let at = (self.head()?, self.resolve_commit("REBASE_HEAD")?);
            ensure!(
                last.as_ref() != Some(&at),
                "the rebase stopped again where it was: {said}"
            );
            last = Some(at);
            let unsettled = self.stopped(Some(&marker.decisions), &marker.untracked)?;
            if !unsettled.is_empty() {
                return Ok(Some(unsettled));
            }
            let leftovers: Vec<PathBuf> = self
                .untracked()?
                .difference(&marker.untracked)
                .cloned()
                .collect();
            if !leftovers.is_empty() {
                let shown: Vec<String> = leftovers
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                bail!(
                    "the rebase left files dotman does not settle: {}",
                    shown.join(", ")
                );
            }
            let settled: Vec<(PathBuf, Decision)> = self
                .unmerged_entries()?
                .into_keys()
                .filter_map(|path| {
                    let decision = marker.decisions.get(&path)?.clone();
                    Some((path, decision))
                })
                .collect();
            self.settle(&settled)?;
            let staged = ["diff", "--cached", "--quiet", "--no-ext-diff"];
            let out = self.probe(&staged)?;
            let step = match out.status.code() {
                Some(0) => "--skip",
                Some(1) => "--continue",
                _ => return Err(self.failure(&staged, &out)),
            };
            said = self.rebase(&[step])?;
        }
        let ended = self
            .probe(&["merge-base", "--is-ancestor", &marker.onto, "HEAD"])?
            .status
            .success();
        ensure!(ended, "the rebase did not finish: {said}");
        Ok(None)
    }

    /// `git rebase` with the pins and `GIT_EDITOR=true`; returns what git printed, whether or not
    /// it stopped.
    fn rebase(&self, args: &[&str]) -> Result<String> {
        let mut cmd = self.command();
        cmd.args(PULL_PINS).env("GIT_EDITOR", "true");
        let mut full = vec!["rebase"];
        full.extend_from_slice(args);
        let out = self.output(cmd, &full)?;
        Ok(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }

    fn rebase_dir(&self) -> Option<PathBuf> {
        ["rebase-merge", "rebase-apply"]
            .iter()
            .map(|dir| self.repo.join(".git").join(dir))
            .find(|dir| fs::symlink_metadata(dir).is_ok())
    }

    /// Corrects, in one commit, every settled path that `HEAD` does not hold at its decision,
    /// then checks them all.
    fn correct(&self, marker: &Marker, host: &str) -> Result<()> {
        let wrong = |tree: &BTreeMap<PathBuf, Entry>| -> Vec<(PathBuf, Decision)> {
            marker
                .decisions
                .iter()
                .filter(|(path, decision)| !holds(tree, path, decision))
                .map(|(path, decision)| (path.clone(), decision.clone()))
                .collect()
        };
        let wrong_now = wrong(&self.tree_entries("HEAD")?);
        if !wrong_now.is_empty() {
            let shown: Vec<String> = wrong_now
                .iter()
                .map(|(path, _)| path.to_string_lossy().into_owned())
                .collect();
            self.settle(&wrong_now)?;
            self.commit(&format!("sync({host}): resolve {}", shown.join(", ")))?;
        }
        if let Some((path, _)) = wrong(&self.tree_entries("HEAD")?).first() {
            bail!(
                "after the replay, {} does not hold the version decided for it",
                path.display()
            );
        }
        Ok(())
    }

    /// Moves the branch from the old head to `HEAD`, only if it is still at the old head.
    fn move_branch(&self, marker: &Marker) -> Result<()> {
        let head = self
            .head()?
            .ok_or_else(|| anyhow!("HEAD has no commit after the replay"))?;
        let refname = full_ref(&marker.branch);
        self.run(&[
            "update-ref",
            "-m",
            "dotman: pull",
            &refname,
            &head,
            &marker.old,
        ])
        .map(drop)
    }

    /// [`Git::recover_pull`] after a failed step; the error says what failed, and what recovery
    /// said when it could not undo it.
    fn undo(&self, failed: anyhow::Error) -> anyhow::Error {
        match self.undo_quietly() {
            Ok(()) => failed.context("the pull was undone, and the repo is as it was before it"),
            Err(e) => e.context(format!("{failed:#}")),
        }
    }

    /// In the run that replayed, nobody edited the worktree since, so nothing is saved: a decided
    /// path at a stop still holds git's conflict markers.
    fn undo_quietly(&self) -> Result<()> {
        match self.recover(false)? {
            Recovery::Restored { .. } | Recovery::NoMarker => Ok(()),
            Recovery::Finished => bail!("the branch moved during the replay"),
            Recovery::Locked(locks) => {
                let shown: Vec<String> = locks
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                bail!(
                    "git's lock files are in the repo: {}; run `{} sync` once they are gone",
                    shown.join(" "),
                    crate::name::bin()
                )
            }
            Recovery::Refused(refusal) => Err(refusal.into()),
        }
    }

    /// Makes the index and the worktree hold each `(path, decision)`: removals first, then the
    /// entries through `update-index --index-info` and `checkout-index`. Paths travel on stdin
    /// only. Refuses, before it changes anything, a path whose worktree parent is a symlink.
    fn settle(&self, settled: &[(PathBuf, Decision)]) -> Result<()> {
        for (path, _) in settled {
            if let Some(link) = self.symlinked_parent(path)? {
                return Err(UnderSymlink(link).into());
            }
        }
        let mut removed = Vec::new();
        let mut entries = Vec::new();
        let mut paths = Vec::new();
        for (path, decision) in settled {
            let bytes = path.as_os_str().as_bytes();
            match decision {
                Decision::Absent => {
                    removed.extend_from_slice(bytes);
                    removed.push(0);
                }
                Decision::Set(entry) => {
                    entries.extend_from_slice(format!("{} {}\t", entry.mode, entry.oid).as_bytes());
                    entries.extend_from_slice(bytes);
                    entries.push(0);
                    paths.extend_from_slice(bytes);
                    paths.push(0);
                }
            }
        }
        if !removed.is_empty() {
            self.run_stdin(
                &["update-index", "--force-remove", "-z", "--stdin"],
                &removed,
            )?;
            for (path, decision) in settled {
                if *decision == Decision::Absent {
                    self.remove_worktree_file(path)?;
                }
            }
        }
        if !entries.is_empty() {
            self.run_stdin(
                &["update-index", "--add", "--replace", "-z", "--index-info"],
                &entries,
            )?;
            // `-f`: at a stop the worktree file holds git's conflict markers.
            self.run_stdin(&["checkout-index", "-u", "-f", "-z", "--stdin"], &paths)?;
        }
        Ok(())
    }

    fn symlinked_parent(&self, path: &Path) -> Result<Option<PathBuf>> {
        let mut parents: Vec<&Path> = path
            .ancestors()
            .skip(1)
            .filter(|dir| !dir.as_os_str().is_empty())
            .collect();
        parents.reverse();
        for dir in parents {
            match fs::symlink_metadata(self.repo.join(dir)) {
                Ok(meta) if meta.file_type().is_symlink() => return Ok(Some(dir.to_path_buf())),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("cannot stat {}", dir.display())),
            }
        }
        Ok(None)
    }

    /// Removes the file or symlink at repo path `path`; a directory or nothing there is left.
    fn remove_worktree_file(&self, path: &Path) -> Result<()> {
        if let Some(link) = self.symlinked_parent(path)? {
            return Err(UnderSymlink(link).into());
        }
        let full = self.repo.join(path);
        match fs::symlink_metadata(&full) {
            Ok(meta) if !meta.is_dir() => {
                fs::remove_file(&full).with_context(|| format!("cannot remove {}", full.display()))
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot stat {}", full.display())),
        }
    }

    /// Every untracked file in the worktree, ignored ones too.
    fn untracked(&self) -> Result<BTreeSet<PathBuf>> {
        let out = self.run(&["ls-files", "-o", "-z"])?;
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|path| !path.is_empty())
            .map(path_from)
            .collect())
    }

    /// `git <args>` with `input` on stdin, from a file in `.git`, so git exiting early cannot kill
    /// dotman with SIGPIPE.
    fn run_stdin(&self, args: &[&str], input: &[u8]) -> Result<Output> {
        let dir = self.repo.join(".git");
        let mut file = tempfile::tempfile_in(&dir)
            .with_context(|| format!("cannot create a temp file in {}", dir.display()))?;
        file.write_all(input)
            .and_then(|()| file.rewind())
            .context("cannot write a temp file")?;
        self.check_version()?;
        let mut cmd = self.command();
        cmd.args(args);
        let out = self.spawn_with(cmd, Stdio::from(file))?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(self.failure(args, &out))
        }
    }

    /// Puts back a repo that a killed replay left under its marker, by the one rule for every
    /// state after the marker was written: while the branch is still at the old head, back to
    /// it; when the branch holds every decision, onto the branch; anything else is refused and
    /// changes nothing. Dirtiness is judged before anything changes. SIGINT, SIGTERM and SIGHUP
    /// are ignored while it runs.
    pub fn recover_pull(&self) -> Result<Recovery> {
        self.recover(true)
    }

    /// [`Git::recover_pull`]; with `keep_edits`, a hand edit of a marker path is saved first.
    fn recover(&self, keep_edits: bool) -> Result<Recovery> {
        let path = self.marker_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Recovery::NoMarker),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let locks = self.lock_files();
        if !locks.is_empty() {
            return Ok(Recovery::Locked(locks));
        }
        let _ignored = crate::sys::ignore_signals(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]);
        let marker = Marker::parse(&bytes);
        let refused = |why: String| {
            let dirty = self.dirty_files().unwrap_or_default();
            let saves_to = self.save_dir(&dirty);
            Recovery::Refused(Refusal {
                why,
                repo: self.repo.clone(),
                marker: marker
                    .as_ref()
                    .map(|marker| (marker.branch.clone(), marker.old.clone())),
                detached_from: if marker.is_none() && matches!(self.branch(), Ok(None)) {
                    self.synced_branch().ok().flatten()
                } else {
                    None
                },
                head: self.head().ok().flatten(),
                rebase_dir: self.rebase_dir().is_some(),
                dirty,
                saves_to,
            })
        };
        let Some(found) = &marker else {
            return Ok(refused(format!("{} cannot be read", path.display())));
        };
        Ok(match self.recover_from(found, keep_edits) {
            Ok(Ok(recovery)) => recovery,
            Ok(Err(why)) => refused(why),
            Err(e) => refused(format!("{e:#}")),
        })
    }

    /// [`Git::recover_pull`] with a marker that parsed; `Err` inside says why it refuses.
    fn recover_from(&self, marker: &Marker, keep_edits: bool) -> Result<Result<Recovery, String>> {
        let onto = self
            .rebase_dir()
            .map(|dir| fs::read_to_string(dir.join("onto")).unwrap_or_default());
        let matching = match onto.as_deref().map(str::trim) {
            None => false,
            Some(onto) if onto == marker.onto => true,
            Some(_) => {
                return Ok(Err(
                    "a rebase dotman did not start is in progress".to_owned()
                ));
            }
        };
        let refname = full_ref(&marker.branch);
        let on = self.optional(&["symbolic-ref", "-q", "HEAD"])?;
        if on.as_ref().is_some_and(|on| *on != refname) {
            return Ok(Err(format!("HEAD is on {}", on.unwrap_or_default())));
        }
        let Some(tip) = self.resolve_commit(&refname)? else {
            return Ok(Err(format!("{} has no commit", marker.branch)));
        };
        let settled = |path: &Path| marker.decisions.contains_key(path);
        for change in self.changes()? {
            // At a matching stop, what git staged and the paths in conflict are the rebase's own.
            let dirty = if matching {
                change.kind != StatusKind::Unmerged && change.xy.get(1..) != Some(".")
            } else {
                true
            };
            let paths = std::iter::once(&change.path).chain(&change.orig_path);
            if dirty && paths.into_iter().any(|path| !settled(path)) {
                return Ok(Err(format!(
                    "{} has changes that are not committed",
                    change.path.display()
                )));
            }
        }
        let head = self.head()?;
        if tip != marker.old {
            let tree = self.tree_entries(&tip)?;
            let finished = head.as_deref() == Some(tip.as_str())
                && !matching
                && marker
                    .decisions
                    .iter()
                    .all(|(path, decision)| holds(&tree, path, decision));
            if !finished {
                return Ok(Err(format!(
                    "{} is at {tip}, which is neither where it was before the pull nor the pull's result",
                    marker.branch
                )));
            }
            self.run(&["symbolic-ref", "HEAD", &refname])?;
            self.remove_marker()?;
            return Ok(Ok(Recovery::Finished));
        }
        let Some(head) = head else {
            return Ok(Err("HEAD has no commit".to_owned()));
        };
        let blobs = self.replay_blobs(marker)?;
        let saved = if keep_edits {
            match self.save_edits(marker, &blobs)? {
                Ok(saved) => saved,
                Err(why) => return Ok(Err(why)),
            }
        } else {
            None
        };
        self.remove_leftovers(&marker.untracked, &blobs)?;
        if matching {
            self.run(&["rebase", "--abort"])?;
        } else {
            let tree = self.tree_entries(&head)?;
            let restore: Vec<(PathBuf, Decision)> = marker
                .decisions
                .keys()
                .map(|path| {
                    let decision = tree
                        .get(path)
                        .map_or(Decision::Absent, |entry| Decision::Set(entry.clone()));
                    (path.clone(), decision)
                })
                .collect();
            self.settle(&restore)?;
            // `read-tree` refuses a path whose stat data is stale, as `--index-info` leaves it.
            self.probe(&["update-index", "-q", "--unmerged", "--refresh"])?;
            self.run(&["read-tree", "-m", "-u", "HEAD", &marker.old])?;
        }
        self.remove_leftovers(&marker.untracked, &blobs)?;
        self.run(&["symbolic-ref", "HEAD", &refname])?;
        if let Some(change) = self.changes()?.first() {
            return Ok(Err(format!(
                "{} still differs from {} after the restore",
                change.path.display(),
                marker.old
            )));
        }
        self.remove_marker()?;
        Ok(Ok(Recovery::Restored { saved }))
    }

    /// Copies each of the marker's paths whose worktree content is none of `blobs` for it, so an
    /// edit made by hand after the kill, to `.dotman/repo/<path>` in a new backup run. `Err`
    /// inside, naming the path, when there is one and no backups directory is set.
    fn save_edits(
        &self,
        marker: &Marker,
        blobs: &BTreeMap<PathBuf, BTreeSet<String>>,
    ) -> Result<Result<Option<Saved>, String>> {
        let mut edits = Vec::new();
        for path in marker.decisions.keys() {
            let Some(bytes) = self.worktree_bytes(path)? else {
                continue;
            };
            let oid = self.hash_object(&bytes, false)?;
            if !blobs.get(path).is_some_and(|known| known.contains(&oid)) {
                edits.push((path, bytes));
            }
        }
        let Some((first, _)) = edits.first() else {
            return Ok(Ok(None));
        };
        let Some(backups) = &self.backups else {
            return Ok(Err(format!(
                "{} has changes that are not committed",
                first.display()
            )));
        };
        let dir = backup_run_dir(backups, SystemTime::now())?.join(".dotman/repo");
        let mut paths = Vec::new();
        for (path, bytes) in edits {
            let text = path
                .to_str()
                .ok_or_else(|| anyhow!("{} is not UTF-8", path.display()))?;
            // Under `.dotman/repo/`, a reserved root name is a plain path.
            validate_key(&format!("repo/{text}"))
                .with_context(|| format!("cannot save {}", path.display()))?;
            back_up_bytes(&dir, text, &bytes)?;
            paths.push(path.clone());
        }
        Ok(Ok(Some(Saved { paths, dir })))
    }

    /// The branch `origin`'s `HEAD` names, else the only local branch, when it is a local branch
    /// whose name [`checked_branch`] takes.
    fn synced_branch(&self) -> Result<Option<String>> {
        let remote_head = self
            .optional(&["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])?
            .and_then(|name| name.strip_prefix("refs/remotes/origin/").map(str::to_owned));
        let branch = if remote_head.is_some() {
            remote_head
        } else {
            let out = self.run(&["for-each-ref", "--format=%(refname)", "refs/heads/"])?;
            let listed = String::from_utf8_lossy(&out.stdout).into_owned();
            let mut names = listed
                .lines()
                .filter_map(|name| name.strip_prefix("refs/heads/"));
            match (names.next(), names.next()) {
                (Some(only), None) => Some(only.to_owned()),
                _ => None,
            }
        };
        let Some(branch) = branch.and_then(|branch| checked_branch(&branch).ok()) else {
            return Ok(None);
        };
        Ok(self.resolve_commit(&full_ref(&branch))?.map(|_| branch))
    }

    /// The `.dotman/repo` directory of a new backup run, with the parent of each of `dirty` in
    /// it made 0700, so a printed `cp` can copy each there. `None` when there is nothing to copy,
    /// no backups directory is set, or one cannot be made.
    fn save_dir(&self, dirty: &[PathBuf]) -> Option<PathBuf> {
        if dirty.is_empty() {
            return None;
        }
        let dir = backup_run_dir(self.backups.as_ref()?, SystemTime::now())
            .ok()?
            .join(".dotman/repo");
        for path in dirty {
            let parent = dir.join(path);
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent.parent()?)
                .ok()?;
        }
        Some(dir)
    }

    /// The files in the worktree whose content `checkout -f` would drop: with a rebase dir, those
    /// that differ from the index outside the paths in conflict, as what git staged is its own;
    /// else every changed path. A path deleted from disk or under a symlink has nothing to copy.
    fn dirty_files(&self) -> Result<Vec<PathBuf>> {
        let rebase = self.rebase_dir().is_some();
        let mut dirty = Vec::new();
        for change in self.changes()? {
            let own =
                rebase && (change.kind == StatusKind::Unmerged || change.xy.get(1..) == Some("."));
            if !own && self.worktree_bytes(&change.path)?.is_some() {
                dirty.push(change.path);
            }
        }
        Ok(dirty)
    }

    /// Tracked changes, staged or not, and unmerged paths; untracked files are left out.
    fn changes(&self) -> Result<Vec<StatusEntry>> {
        let mut cmd = self.command();
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        let args = [
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=no",
            "--ignore-submodules=all",
        ];
        parse_status(&self.run_with(cmd, &args)?.stdout)
    }

    /// The blobs the replay could have written at each path: its index stages, its entries in
    /// `HEAD`, the old head and the fetched commit, and its decision. Gitlinks are left out.
    fn replay_blobs(&self, marker: &Marker) -> Result<BTreeMap<PathBuf, BTreeSet<String>>> {
        let mut trees = Vec::new();
        for rev in ["HEAD", &marker.old, &marker.onto] {
            trees.push(self.tree_entries(rev)?);
        }
        let decided: BTreeMap<PathBuf, Entry> = marker
            .decisions
            .iter()
            .filter_map(|(path, decision)| match decision {
                Decision::Set(entry) => Some((path.clone(), entry.clone())),
                Decision::Absent => None,
            })
            .collect();
        trees.push(decided);
        Ok(known_blobs(&trees, &self.unmerged_entries()?))
    }

    /// The bytes at repo path `path` in the worktree: a file's content or a symlink's target.
    /// `None` when nothing, a directory or a special file is there, or the path lies under a
    /// symlink.
    fn worktree_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        if self.symlinked_parent(path)?.is_some() {
            return Ok(None);
        }
        let full = self.repo.join(path);
        let meta = match fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot stat {}", full.display())),
        };
        if meta.file_type().is_symlink() {
            let target =
                fs::read_link(&full).with_context(|| format!("cannot read {}", full.display()))?;
            return Ok(Some(target.into_os_string().into_vec()));
        }
        if !meta.is_file() {
            return Ok(None);
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&full)
            .with_context(|| format!("cannot open {}", full.display()))?;
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut file, &mut bytes)
            .with_context(|| format!("cannot read {}", full.display()))?;
        Ok(Some(bytes))
    }

    /// The untracked files git 2.25 leaves from a stop, which `rebase --abort` keeps, each with the
    /// path it is a copy of: those not in `untracked` from before the rebase, named
    /// `<path>~<suffix>`, that hold one of `blobs` for `<path>`. Another new file is none, as a
    /// user's `<path>~mine` or an editor's `<path>~` is not.
    fn leftovers(
        &self,
        untracked: &BTreeSet<PathBuf>,
        blobs: &BTreeMap<PathBuf, BTreeSet<String>>,
    ) -> Result<Vec<(PathBuf, PathBuf)>> {
        let mut found = Vec::new();
        for path in self.untracked()?.difference(untracked) {
            let stem = path
                .file_name()
                .and_then(|name| name.as_bytes().split(|b| *b == b'~').next())
                .filter(|stem| {
                    !stem.is_empty() && path.file_name().map(OsStrExt::as_bytes) != Some(*stem)
                })
                .map(|stem| path.with_file_name(OsStr::from_bytes(stem)));
            let Some((stem, known)) = stem.and_then(|stem| blobs.get_key_value(&stem)) else {
                continue;
            };
            let Some(bytes) = self.worktree_bytes(path)? else {
                continue;
            };
            if known.contains(&self.hash_object(&bytes, false)?) {
                found.push((path.clone(), stem.clone()));
            }
        }
        Ok(found)
    }

    fn remove_leftovers(
        &self,
        untracked: &BTreeSet<PathBuf>,
        blobs: &BTreeMap<PathBuf, BTreeSet<String>>,
    ) -> Result<()> {
        for (path, _) in self.leftovers(untracked, blobs)? {
            self.remove_worktree_file(&path)?;
        }
        Ok(())
    }

    /// Fails naming every lock file of [`Git::lock_files`], with one `rm` step for them all. A
    /// killed git leaves them, and so does a git still running in the repo, which the message
    /// allows for.
    pub fn refuse_locks(&self) -> Result<()> {
        let locks = self.lock_files();
        if locks.is_empty() {
            return Ok(());
        }
        let quoted = locks
            .iter()
            .map(|path| shell_words::quote(&path.to_string_lossy()).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        bail!(
            "git's lock files are in the repo: {quoted}. Another git is running in {}, or one was killed and left them behind; if no git is running, remove them with `rm {quoted}` and run this again",
            self.repo.display()
        )
    }

    /// Looks for the marker files of an unfinished rebase, merge, cherry-pick, revert or bisect
    /// under `.git`, in that order.
    #[must_use]
    pub fn operation(&self) -> Option<Operation> {
        let git_dir = self.repo.join(".git");
        let has = |name: &str| git_dir.join(name).exists();
        if has("rebase-merge") || has("rebase-apply") {
            Some(Operation::Rebase)
        } else if has("MERGE_HEAD") {
            Some(Operation::Merge)
        } else if has("CHERRY_PICK_HEAD") {
            Some(Operation::CherryPick)
        } else if has("REVERT_HEAD") {
            Some(Operation::Revert)
        } else if has("sequencer") {
            // Between the picks of a series no HEAD file names it; the todo list does.
            let todo = fs::read_to_string(git_dir.join("sequencer/todo")).unwrap_or_default();
            Some(if todo.trim_start().starts_with("revert") {
                Operation::Revert
            } else {
                Operation::CherryPick
            })
        } else {
            has("BISECT_LOG").then_some(Operation::Bisect)
        }
    }

    /// Paths with unresolved conflicts in the index, which `git stash pop` leaves with no
    /// operation to continue.
    pub fn unmerged_paths(&self) -> Result<Vec<PathBuf>> {
        // One `mode oid stage<TAB>path` record per stage.
        let out = self.run(&["ls-files", "-u", "-z"])?;
        let mut paths: Vec<PathBuf> = out
            .stdout
            .split(|b| *b == 0)
            .filter(|record| !record.is_empty())
            .map(|record| {
                let path = record
                    .iter()
                    .position(|b| *b == b'\t')
                    .and_then(|tab| record.get(tab.saturating_add(1)..))
                    .unwrap_or(record);
                path_from(path)
            })
            .collect();
        paths.dedup();
        Ok(paths)
    }

    /// Lock files git left in `.git`: every `*.lock` at its top, such as `index.lock`,
    /// `MERGE_MSG.lock` or `AUTO_MERGE.lock`, and any under `refs`. A killed git leaves them
    /// behind, and every later git that writes the same file then fails.
    #[must_use]
    pub fn lock_files(&self) -> Vec<PathBuf> {
        let git_dir = self.repo.join(".git");
        let mut found: Vec<PathBuf> = fs::read_dir(&git_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().as_bytes().ends_with(b".lock"))
            .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
        found.sort();
        found.extend(
            walkdir::WalkDir::new(git_dir.join("refs"))
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().as_bytes().ends_with(b".lock"))
                .map(walkdir::DirEntry::into_path),
        );
        found
    }

    /// The branch an unfinished rebase started from, such as `main`; `None` when no `head-name`
    /// names one. Fails with [`UnusableBranch`] on a name [`checked_branch`] refuses.
    pub fn rebase_branch(&self) -> Result<Option<String>, UnusableBranch> {
        let name = ["rebase-merge", "rebase-apply"].iter().find_map(|dir| {
            let name =
                fs::read_to_string(self.repo.join(".git").join(dir).join("head-name")).ok()?;
            Some(name.trim().strip_prefix("refs/heads/")?.to_owned())
        });
        name.map(|name| checked_branch(&name)).transpose()
    }

    /// Whether [`Git::operation`] finds an unfinished operation.
    #[must_use]
    pub fn operation_in_progress(&self) -> bool {
        self.operation().is_some()
    }

    /// Fetches `branch` from `remote` into `FETCH_HEAD`.
    pub fn fetch(&self, remote: &str, branch: &str) -> Result<()> {
        self.run_remote(&["fetch", "-q", remote, &full_ref(branch)])
            .map(drop)
    }

    /// Like [`Git::fetch`], killing git after `timeout`, which fails like any other fetch that
    /// could not reach the remote.
    pub fn fetch_within(&self, remote: &str, branch: &str, timeout: Duration) -> Result<()> {
        self.check_version()?;
        let args = ["fetch", "-q", remote, &full_ref(branch)];
        let mut cmd = self.command();
        cmd.args(args);
        let out = crate::proc::output_within(&mut cmd, timeout)
            .map_err(|err| self.spawn_error(err))?
            .ok_or_else(|| {
                anyhow!("`git fetch` did not finish within {} s", timeout.as_secs())
                    .context(RemoteFailure::Unreachable)
            })?;
        if out.status.success() {
            Ok(())
        } else {
            Err(self.remote_failure(&args, &out))
        }
    }

    /// Like [`Git::fetch`], and fetches the history a shallow clone left out.
    pub fn unshallow(&self, remote: &str, branch: &str) -> Result<()> {
        self.run_remote(&["fetch", "-q", "--unshallow", remote, &full_ref(branch)])
            .map(drop)
    }

    /// The best common ancestor of `a` and `b`; `None` when their histories are unrelated.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let args = ["merge-base", a, b];
        let out = self.probe(&args)?;
        match out.status.code() {
            Some(0) => Ok(Some(text(&out))),
            Some(1) => Ok(None),
            _ => Err(self.failure(&args, &out)),
        }
    }

    /// Fails if the remote has commits `branch` lacks; pull first.
    pub fn push(&self, remote: &str, branch: &str) -> Result<()> {
        self.run_remote(&["push", remote, &push_refspec(branch)])
            .map(drop)
    }

    /// Like [`Git::push`], and records `remote` as the upstream of `branch`, so that a bare
    /// `git pull --rebase` works.
    pub fn push_set_upstream(&self, remote: &str, branch: &str) -> Result<()> {
        self.run_remote(&["push", "-u", remote, &push_refspec(branch)])
            .map(drop)
    }

    /// How many commits `tip` has that `base` lacks; every commit of `tip` when `base` is `None`.
    pub fn count_commits(&self, base: Option<&str>, tip: &str) -> Result<u64> {
        let range = base.map_or_else(|| tip.to_owned(), |base| format!("{base}..{tip}"));
        let out = self.run(&["rev-list", "--count", range.as_str()])?;
        text(&out)
            .parse()
            .context("git rev-list printed a bad count")
    }

    /// The paths `tip` holds that differ from `base` or that `base` lacks; every path of `tip`
    /// when `base` is `None`. A path that moved counts as added.
    pub fn changed_paths(&self, base: Option<&str>, tip: &str) -> Result<Vec<PathBuf>> {
        let Some(base) = base else {
            return Ok(self
                .ls_tree(tip)?
                .into_iter()
                .map(|entry| entry.path)
                .collect());
        };
        let out = self.run(&[
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            "--diff-filter=d",
            base,
            tip,
        ])?;
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|path| !path.is_empty())
            .map(|path| PathBuf::from(OsStr::from_bytes(path)))
            .collect())
    }

    /// Whether `remote` has `branch`. Asks the remote and writes nothing locally. Its failure is
    /// [`RemoteFailure::Unreachable`] whatever git's message says, since ls-remote only reads.
    pub fn remote_has_branch(&self, remote: &str, branch: &str) -> Result<bool> {
        let pattern = full_ref(branch);
        let args = ["ls-remote", "--exit-code", remote, pattern.as_str()];
        let out = self.probe(&args)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(2) => Ok(false),
            _ => Err(self
                .failure(&args, &out)
                .context(RemoteFailure::Unreachable)),
        }
    }

    /// Whether `url` has any branch. Asks the remote and writes nothing locally.
    pub fn remote_has_any_branch(&self, url: &str) -> Result<bool> {
        let out = self.run_remote(&["ls-remote", "--heads", "--", url])?;
        Ok(!out.stdout.is_empty())
    }

    /// Every file, symlink and gitlink of `rev`, in git's order.
    pub fn ls_tree(&self, rev: &str) -> Result<Vec<TreeEntry>> {
        let out = self.run(&["ls-tree", "-r", "-z", rev])?;
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter_map(|record| {
                let (meta, path) =
                    record.split_at_checked(record.iter().position(|b| *b == b'\t')?)?;
                let mode = meta.split(|b| *b == b' ').next()?;
                Some(TreeEntry {
                    mode: String::from_utf8_lossy(mode).into_owned(),
                    path: PathBuf::from(OsStr::from_bytes(path.get(1..)?)),
                })
            })
            .collect())
    }

    /// Every path in the index, in git's order, including files that are gone from the worktree.
    pub fn index_paths(&self) -> Result<Vec<String>> {
        let out = self.run(&["ls-files", "-z"])?;
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|path| !path.is_empty())
            .filter_map(|path| String::from_utf8(path.to_vec()).ok())
            .collect())
    }

    /// Every value of `key` in the config file `file`, which need not belong to a repo. A
    /// missing key or file gives no values.
    pub fn config_values(&self, file: &Path, key: &str) -> Result<Vec<String>> {
        let args = [
            OsStr::new("config"),
            OsStr::new("--file"),
            file.as_os_str(),
            OsStr::new("--get-all"),
            OsStr::new(key),
        ];
        let out = self.probe_bounded(self.repoless(), &args)?;
        match out.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_owned)
                .collect()),
            Some(1) => Ok(Vec::new()),
            _ if !file.exists() => Ok(Vec::new()),
            _ => Err(self.failure(&args, &out)),
        }
    }

    /// Removes remote `name` and its remote-tracking refs.
    pub fn remove_remote(&self, name: &str) -> Result<()> {
        self.run(&["remote", "remove", name]).map(drop)
    }

    /// The URL of remote `name` exactly as written in the config, without `insteadOf` rewriting.
    pub fn remote_url(&self, name: &str) -> Result<Option<String>> {
        self.optional(&["config", "--get", &format!("remote.{name}.url")])
    }

    /// [`Git::remote_url`] as git reads it here: a relative local path made absolute against the
    /// directory git runs in, the top of the work tree, also for a non-bare repo given as its git
    /// dir, or else the repo path itself.
    pub fn remote_target(&self, name: &str) -> Result<Option<String>> {
        let Some(url) = self.remote_url(name)? else {
            return Ok(None);
        };
        if !relative_path_remote(&url) {
            return Ok(Some(url));
        }
        let out = self.probe_bounded(self.command(), &["rev-parse", "--show-toplevel"])?;
        let top = out.stdout.trim_ascii_end();
        let dir = if out.status.success() && !top.is_empty() {
            path_from(top)
        } else {
            self.work_tree_of_git_dir()?
                .unwrap_or_else(|| self.repo.clone())
        };
        absolute_remote(&url, &dir).map(Some)
    }

    /// The work tree of a non-bare repo whose git dir this handle names, where `--show-toplevel`
    /// fails: the git dir's parent.
    fn work_tree_of_git_dir(&self) -> Result<Option<PathBuf>> {
        let args = ["rev-parse", "--is-bare-repository", "--absolute-git-dir"];
        let out = self.probe_bounded(self.command(), &args)?;
        if !out.status.success() {
            return Ok(None);
        }
        let mut lines = out.stdout.split(|b| *b == b'\n');
        if lines.next() != Some(b"false") {
            return Ok(None);
        }
        Ok(lines
            .next()
            .filter(|dir| !dir.is_empty())
            .and_then(|dir| path_from(dir).parent().map(Path::to_path_buf)))
    }

    /// Points remote `name` at `url`, adding the remote if needed.
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<()> {
        let verb = if self.remote_url(name)?.is_some() {
            "set-url"
        } else {
            "add"
        };
        self.run(&["remote", verb, "--", name, url]).map(drop)
    }

    /// Whether git can name both an author and a committer from config or the environment. With
    /// `useConfigOnly`, an identity guessed from the host name does not count.
    pub fn identity_configured(&self) -> Result<bool> {
        for role in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
            let args = ["-c", "user.useConfigOnly=true", "var", role];
            let out = self.probe_bounded(self.command(), &args)?;
            match out.status.code() {
                Some(0) => {}
                Some(128) => return Ok(false),
                _ => return Err(self.failure(&args, &out)),
            }
        }
        Ok(true)
    }

    /// The email in `git var GIT_AUTHOR_IDENT`; `None` when git has no configured identity. A repo
    /// that does not exist yet has only the global config, so git then runs with no repo.
    pub fn author_email(&self) -> Result<Option<String>> {
        let args = ["-c", "user.useConfigOnly=true", "var", "GIT_AUTHOR_IDENT"];
        let cmd = if self.repo.is_dir() {
            self.command_in(&self.repo)
        } else {
            self.repoless()
        };
        let out = self.probe_bounded(cmd, &args)?;
        match out.status.code() {
            Some(0) => Ok(parse_ident_email(&text(&out))),
            Some(128) => Ok(None),
            _ => Err(self.failure(&args, &out)),
        }
    }

    /// Whether `url` answers `git ls-remote` within `timeout` with no credentials and no prompt.
    /// A timeout counts as no answer.
    pub fn ls_remote_anonymous(&self, url: &str, timeout: Duration) -> Result<bool> {
        self.check_version()?;
        let mut cmd = self.repoless();
        cmd.args(["-c", "credential.helper=", "ls-remote", url, "HEAD"]);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GIT_ASKPASS", "").env("SSH_ASKPASS", "");
        let status = crate::proc::status_within(&mut cmd, timeout).context("failed to run git")?;
        Ok(status.is_some_and(|status| status.success()))
    }

    /// Sets `key` in the repo's config, or in the user's global config.
    pub fn set_config(&self, key: &str, value: &str, global: bool) -> Result<()> {
        let scope = if global { "--global" } else { "--local" };
        let cmd = if global {
            self.repoless()
        } else {
            self.command()
        };
        self.run_with(cmd, &["config", scope, "--", key, value])
            .map(drop)
    }
}

fn pinned_last(attributes: &[u8]) -> bool {
    attributes
        .split(|b| *b == b'\n')
        .rfind(|line| !line.trim_ascii().is_empty())
        .is_some_and(|line| line.trim_ascii() == ATTRIBUTES.as_bytes())
}

/// `attributes` without the lines dotman wrote and the blank ones at its end, then [`ATTRIBUTES`].
fn with_pinned_last(attributes: &[u8]) -> Vec<u8> {
    let dotmans = |line: &[u8]| {
        let line = line.trim_ascii();
        line == ATTRIBUTES.as_bytes() || OLD_ATTRIBUTES.iter().any(|old| line == old.as_bytes())
    };
    let mut lines: Vec<&[u8]> = attributes
        .split(|b| *b == b'\n')
        .filter(|line| !dotmans(line))
        .collect();
    while lines
        .last()
        .is_some_and(|line| line.trim_ascii().is_empty())
    {
        lines.pop();
    }
    let mut out = Vec::new();
    for line in lines.into_iter().chain([ATTRIBUTES.as_bytes()]) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

fn pathspec(magic: &str, path: &Path) -> OsString {
    let mut spec = OsString::from(magic);
    spec.push(path);
    spec
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn failure<S: AsRef<OsStr>>(args: &[S], out: &Output) -> anyhow::Error {
    let label = args
        .iter()
        .map(|a| redact_credentials(&a.as_ref().to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    anyhow!(
        "git {label} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// `Name <email> 1700000000 +0000` gives `email`.
fn parse_ident_email(ident: &str) -> Option<String> {
    let after = ident.rsplit_once('<')?.1;
    let email = after.split_once('>')?.0;
    (!email.is_empty()).then(|| email.to_owned())
}

/// The branch [`Git::adopt_unborn_head`] put `HEAD` on.
#[derive(Debug, PartialEq, Eq)]
pub struct Adopted {
    /// A name [`checked_branch`] takes.
    pub branch: String,
    /// How many branches the remote has, this one among them.
    pub of: usize,
}

/// Up to eight of `names` for a message, each one [`checked_branch`] takes or a stand-in.
fn branch_names(names: &[String]) -> String {
    const SHOWN: usize = 8;
    let mut shown: Vec<String> = names
        .iter()
        .take(SHOWN)
        .map(|name| {
            checked_branch(name).unwrap_or_else(|_| "a name dotman does not show".to_owned())
        })
        .collect();
    let more = names.len().saturating_sub(SHOWN);
    if more > 0 {
        shown.push(format!("{more} more"));
    }
    shown.join(", ")
}

/// A branch name that [`checked_branch`] refuses. It is shown escaped, or not at all when it holds a
/// credential or a backtick, and the way out it prints puts the repo on `main` without naming it.
#[derive(Debug)]
pub struct UnusableBranch {
    shown: Option<String>,
    why: String,
    unborn: bool,
}

impl UnusableBranch {
    /// Why the name is refused, without the name, to follow "whose".
    #[must_use]
    pub fn why(&self) -> &str {
        &self.why
    }

    /// The git arguments that put `HEAD` on `main` without naming the branch. An unborn branch
    /// has no ref for `branch -m` to rename, so `HEAD` is pointed at `main` instead.
    const fn way_out(&self) -> [&'static str; 3] {
        if self.unborn {
            ["symbolic-ref", "HEAD", "refs/heads/main"]
        } else {
            ["branch", "-m", "main"]
        }
    }
}

impl fmt::Display for UnusableBranch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.shown {
            Some(name) => write!(f, "the repo's branch {name} is one whose {}", self.why)?,
            None => write!(f, "the repo's branch is one whose {}", self.why)?,
        }
        write!(
            f,
            ", so dotman does not pass it to git; put the repo on main with `{} git {}` and run this again",
            crate::name::bin(),
            self.way_out().join(" ")
        )
    }
}

impl std::error::Error for UnusableBranch {}

/// `name` when dotman passes it to git as a branch. Besides `git check-ref-format --branch`, which
/// refuses a leading `-`, it takes only ASCII letters, digits, `.`, `_`, `-` and `/`: git allows
/// `+`, which a refspec reads as a forced push, and characters a shell expands or splits. A name
/// that holds a credential would reach the remote as a ref name.
pub fn checked_branch(name: &str) -> Result<String, UnusableBranch> {
    if let Some(found) = crate::secrets::held_name(name) {
        return Err(UnusableBranch {
            shown: None,
            why: format!(
                "name holds a credential the content guard finds ({})",
                found.rule
            ),
            unborn: false,
        });
    }
    let why = if name.starts_with('-') {
        "name starts with a dash, which git reads as an option"
    } else if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    {
        "name holds a character other than an ASCII letter, a digit, a dot, an underscore, a dash and a slash"
    } else if name == "HEAD"
        || name.contains("..")
        || name.ends_with('.')
        || name.split('/').any(|part| {
            part.is_empty() || part.starts_with('.') || part.as_bytes().ends_with(b".lock")
        })
    {
        "name breaks git's rules for a branch name"
    } else {
        return Ok(name.to_owned());
    };
    // A backtick would let the name open a command span of its own in the message.
    Err(UnusableBranch {
        shown: (!name.contains('`')).then(|| crate::output::escape_line(name).into_owned()),
        why: why.to_owned(),
        unborn: false,
    })
}

/// A branch as a full refname, which git never reads as an option.
fn full_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

fn push_refspec(branch: &str) -> String {
    let refname = full_ref(branch);
    format!("{refname}:{refname}")
}

/// Whether `url` can be a remote's URL: git would read an empty one or one starting with `-` as
/// nothing or as an option wherever it passes the URL on, and a control character (C0, DEL or
/// C1) would forge lines in a note that shows it or change a command that sets it. git hands
/// `<transport>::<address>` to the program `git-remote-<transport>`, and `ext::` runs the address
/// as a command, so that form is refused too: letters, digits, `+`, `.` and `-`, then `::`.
pub fn usable_remote(url: &str) -> bool {
    let transport =
        url.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    !url.is_empty()
        && !url.starts_with('-')
        && !url.chars().any(char::is_control)
        && !transport.starts_with("::")
}

/// Whether git reads `url` as a local path: it does unless a `:` comes before the first `/`, as in
/// `scheme://...` and the ssh forms `host:path` and `user@host:path`.
pub fn local_path_remote(url: &str) -> bool {
    url.find(':')
        .is_none_or(|colon| url.find('/').is_some_and(|slash| slash < colon))
}

/// Whether git reads `url` as a local path relative to where it runs. git expands a leading `~` or
/// `~user` itself, against the home of whoever runs it, so such a path is not.
pub fn relative_path_remote(url: &str) -> bool {
    usable_remote(url)
        && local_path_remote(url)
        && Path::new(url).is_relative()
        && !url.starts_with('~')
}

/// `url` as git reads it when it runs in `dir`: a relative local path made absolute, through the
/// real directory when the path exists. Anything else, a path starting with `~` among it, comes
/// back as it is.
pub fn absolute_remote(url: &str, dir: &Path) -> Result<String> {
    if !relative_path_remote(url) {
        return Ok(url.to_owned());
    }
    let joined = dir.join(url);
    let resolved = fs::canonicalize(&joined)
        .or_else(|_| std::path::absolute(&joined))
        .with_context(|| format!("cannot make {} absolute", joined.display()))?;
    resolved
        .into_os_string()
        .into_string()
        .map_err(|path| anyhow!("{} is not valid UTF-8", Path::new(&path).display()))
}

/// Hides `user:password@` in URLs so tokens do not reach error messages.
pub fn redact_credentials(arg: &str) -> String {
    let Some((scheme, rest)) = arg.split_once("://") else {
        return arg.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    rest.get(..authority_end)
        .and_then(|authority| authority.rfind('@'))
        .map_or_else(
            || arg.to_owned(),
            |at| {
                format!(
                    "{scheme}://***@{}",
                    rest.get(at.saturating_add(1)..).unwrap_or("")
                )
            },
        )
}

/// Parses the `git --version` line, e.g. `git version 2.39.5 (Apple Git-154)`.
fn parse_version(line: &str) -> Option<(u32, u32, u32)> {
    let token = line
        .trim()
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = token.split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

fn check_version_text(line: &str) -> Result<()> {
    let (major, minor, patch) = parse_version(line)
        .ok_or_else(|| anyhow!("could not read the git version from {:?}", line.trim()))?;
    if (major, minor) < MIN_VERSION {
        bail!(
            "git {major}.{minor}.{patch} is too old; dotman needs git {}.{} or newer",
            MIN_VERSION.0,
            MIN_VERSION.1
        );
    }
    Ok(())
}

fn read_os_release() -> Option<String> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|p| fs::read_to_string(p).ok())
}

/// Reads `ID` and `ID_LIKE` by parsing, never by sourcing the file.
fn install_hint(macos: bool, os_release: Option<&str>) -> String {
    if macos {
        return "xcode-select --install".to_owned();
    }
    let mut ids = Vec::new();
    for line in os_release.unwrap_or_default().lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        match key.trim() {
            "ID" => ids.insert(0, value.to_owned()),
            "ID_LIKE" => ids.extend(value.split_whitespace().map(str::to_owned)),
            _ => {}
        }
    }
    for id in &ids {
        match id.as_str() {
            "arch" => return "pacman -S git".to_owned(),
            "debian" | "ubuntu" => return "apt install git".to_owned(),
            "fedora" | "rhel" => return "dnf install git".to_owned(),
            "alpine" => return "apk add git".to_owned(),
            _ => {}
        }
    }
    "pacman -S git (Arch), apt install git (Debian, Ubuntu), dnf install git (Fedora, RHEL), \
     apk add git (Alpine), xcode-select --install (macOS)"
        .to_owned()
}

fn path_from(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

fn parse_status(raw: &[u8]) -> Result<Vec<StatusEntry>> {
    let mut records = raw.split(|b| *b == 0);
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let (kind, field_count) = match record.first() {
            Some(b'1') => (StatusKind::Ordinary, 9),
            Some(b'2') => (StatusKind::Renamed, 10),
            Some(b'u') => (StatusKind::Unmerged, 11),
            Some(b'?' | b'!') => (StatusKind::Untracked, 2),
            _ => bail!(
                "unexpected git status record: {}",
                String::from_utf8_lossy(record)
            ),
        };
        let fields: Vec<&[u8]> = record.splitn(field_count, |b| *b == b' ').collect();
        let (true, Some(xy), Some(path)) =
            (fields.len() == field_count, fields.get(1), fields.last())
        else {
            bail!(
                "malformed git status record: {}",
                String::from_utf8_lossy(record)
            );
        };
        let xy = match record.first() {
            Some(b'?') => "??".to_owned(),
            Some(b'!') => "!!".to_owned(),
            _ => String::from_utf8_lossy(xy).into_owned(),
        };
        let orig_path = if kind == StatusKind::Renamed {
            let orig = records
                .next()
                .ok_or_else(|| anyhow!("git status rename record has no source path"))?;
            Some(path_from(orig))
        } else {
            None
        };
        entries.push(StatusEntry {
            kind,
            xy,
            path: path_from(path),
            orig_path,
        });
    }
    Ok(entries)
}

/// `-z` separates the records with a NUL and ends the last with one. An empty subject is an empty
/// field, and a commit git cannot read an author time from, such as one in UTF-16, shows 0.
fn parse_log(raw: &[u8]) -> Result<Vec<LogEntry>> {
    let raw = raw.strip_suffix(b"\0").unwrap_or(raw);
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let fields: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    fields
        .chunks(3)
        .map(|chunk| match chunk {
            [oid, time, subject] => Ok(LogEntry {
                oid: String::from_utf8_lossy(oid).into_owned(),
                time: String::from_utf8_lossy(time).parse().unwrap_or(0),
                subject: String::from_utf8_lossy(subject).into_owned(),
            }),
            _ => Err(anyhow!("git log output ended mid-record")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn a_log_record_with_an_empty_subject_or_no_author_time_keeps_its_place() {
        let raw = b"aaa\x00100\x00\x00bbb\x00\x00second\x00ccc\x00300\x00third\x00";
        let entries = parse_log(raw).unwrap();
        let got: Vec<(&str, i64, &str)> = entries
            .iter()
            .map(|e| (e.oid.as_str(), e.time, e.subject.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("aaa", 100, ""),
                ("bbb", 0, "second"),
                ("ccc", 300, "third")
            ]
        );
        assert_eq!(parse_log(b"").unwrap().len(), 0);
        assert!(parse_log(b"aaa\x00100\x00").is_err());
    }

    struct Sandbox {
        dir: TempDir,
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = TempDir::new().expect("tempdir");
            let home = dir.path().join("home");
            fs::create_dir_all(home.join(".config")).expect("mkdir");
            fs::write(
                home.join(".gitconfig"),
                "[user]\n\tname = Test User\n\temail = test@example.invalid\n",
            )
            .expect("gitconfig");
            Self { dir }
        }

        fn env(&self) -> Vec<(OsString, OsString)> {
            let home = self.dir.path().join("home");
            vec![
                ("HOME".into(), home.clone().into()),
                ("XDG_CONFIG_HOME".into(), home.join(".config").into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ]
        }

        fn git(&self, name: &str) -> Git {
            Git::new(self.dir.path().join(name), "test")
                .with_env(self.env())
                .backing_up_into(self.dir.path().join(format!("{name}-backups")))
        }

        /// Un-isolated git, run the way a user would run it.
        fn raw(&self, dir: &Path, args: &[&str]) -> Output {
            let mut cmd = crate::proc::command("git");
            for var in LOCAL_REPO_ENV {
                cmd.env_remove(var);
            }
            cmd.envs(self.env()).arg("-C").arg(dir).args(args);
            cmd.output().expect("spawn git")
        }

        fn raw_ok(&self, dir: &Path, args: &[&str]) -> String {
            let out = self.raw(dir, args);
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            text(&out)
        }

        fn bare_remote(&self) -> PathBuf {
            let path = self.dir.path().join("remote.git");
            fs::create_dir_all(&path).expect("mkdir");
            self.raw_ok(&path, &["init", "-q", "--bare"]);
            self.raw_ok(&path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            path
        }
    }

    fn utf8(path: &Path) -> &str {
        path.to_str().expect("utf8 path")
    }

    fn write(repo: &Git, rel: &str, bytes: &[u8]) {
        fs::write(repo.repo().join(rel), bytes).expect("write");
    }

    fn commit_file(repo: &Git, rel: &str, bytes: &[u8], msg: &str) -> String {
        write(repo, rel, bytes);
        repo.stage_all().expect("stage");
        repo.commit(msg).expect("commit").expect("a commit")
    }

    #[test]
    fn parses_distro_and_apple_version_strings() {
        assert_eq!(parse_version("git version 2.25.1\n"), Some((2, 25, 1)));
        assert_eq!(
            parse_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39, 5))
        );
        assert_eq!(parse_version("git version 2.56.0"), Some((2, 56, 0)));
        assert_eq!(parse_version("not git"), None);
    }

    #[test]
    fn version_floor_is_2_25() {
        assert!(check_version_text("git version 2.25.0").is_ok());
        assert!(check_version_text("git version 3.0.0").is_ok());
        let err = check_version_text("git version 2.24.0").expect_err("too old");
        assert!(err.to_string().contains("2.24.0"), "{err}");
        assert!(check_version_text("garbage").is_err());
    }

    #[test]
    fn install_hint_follows_os_release() {
        let hint = |text: &str| install_hint(false, Some(text));
        assert_eq!(hint("ID=arch\n"), "pacman -S git");
        assert_eq!(
            hint("NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n"),
            "apt install git"
        );
        assert_eq!(hint("ID=\"fedora\""), "dnf install git");
        assert_eq!(
            hint("ID=rocky\nID_LIKE=\"rhel centos fedora\""),
            "dnf install git"
        );
        assert_eq!(hint("ID=alpine"), "apk add git");
        assert_eq!(hint("ID=manjaro\nID_LIKE=arch"), "pacman -S git");
        assert_eq!(install_hint(true, None), "xcode-select --install");
        let unknown = install_hint(false, Some("ID=plan9"));
        for needle in ["pacman", "apt", "dnf", "apk", "xcode-select"] {
            assert!(unknown.contains(needle), "{unknown}");
        }
    }

    #[test]
    fn missing_git_error_names_the_command() {
        let sb = Sandbox::new();
        let empty = sb.dir.path().join("empty-path");
        fs::create_dir_all(&empty).expect("mkdir");
        let git = sb.git("repo").with_env([("PATH", empty.as_os_str())]);
        let err = git.status().expect_err("git is not on PATH");
        let msg = err.to_string();
        assert!(msg.contains("`dotman test` needs git"), "{msg}");
        assert!(msg.contains("Install it with:"), "{msg}");
    }

    #[test]
    fn failure_names_command_and_stderr_and_hides_credentials() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        commit_file(&git, "a", b"a", "one");
        let err = git.push("nope", "main").expect_err("no such remote");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git push nope refs/heads/main:refs/heads/main failed"),
            "{msg}"
        );
        assert!(msg.contains("'nope' does not appear"), "{msg}");
        assert_eq!(
            err.downcast_ref::<RemoteFailure>(),
            Some(&RemoteFailure::Unreachable)
        );
        assert_eq!(
            redact_credentials(concat!("https://user:", "s3cret@host/a@b")),
            "https://***@host/a@b"
        );
        assert_eq!(redact_credentials("git@host:x/y.git"), "git@host:x/y.git");
    }

    /// git's corrupt-index messages name `.git/index`; a failed object write, which ends in
    /// "unable to index file", keeps git's own text.
    #[test]
    fn only_a_corrupt_index_is_reported_as_one() {
        use std::os::unix::process::ExitStatusExt;
        let tmp = TempDir::new().expect("tmp");
        let git = Git::new(tmp.path().join("repo"), "test");
        let said = |stderr: &str| {
            let out = Output {
                status: std::process::ExitStatus::from_raw(32768),
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            };
            format!("{:#}", git.failure(&["add"], &out))
        };
        for corrupt in [
            "error: bad signature 0x00000000\nfatal: index file corrupt",
            "error: bad index file sha1 signature\nfatal: index file corrupt",
            "fatal: index file smaller than expected",
            "error: bad index version 9\nfatal: index file corrupt",
        ] {
            assert!(said(corrupt).contains(".git/index is corrupt"), "{corrupt}");
        }
        let denied = "error: insufficient permission for adding an object to repository database .git/objects\nerror: .a: failed to insert into database\nerror: unable to index file '.a'\nfatal: updating files failed";
        let err = said(denied);
        assert!(!err.contains("corrupt"), "{err}");
        assert!(
            err.contains("insufficient permission for adding an object"),
            "{err}"
        );
    }

    #[test]
    fn init_sets_main_attributes_and_stable_id() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        assert_eq!(
            sb.raw_ok(git.repo(), &["symbolic-ref", "HEAD"]),
            "refs/heads/main"
        );
        let git_dir = git.repo().join(".git");
        assert_eq!(
            fs::read_to_string(git_dir.join("info/attributes")).expect("attributes"),
            format!("{ATTRIBUTES}\n")
        );
        let id = fs::read_to_string(git_dir.join("dotman-id")).expect("id");
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");

        commit_file(&git, "a", b"a", "one");
        sb.raw_ok(git.repo(), &["branch", "-m", "other"]);
        git.init().expect("rerun");
        assert_eq!(
            fs::read_to_string(git_dir.join("dotman-id")).expect("id"),
            id
        );
        assert_eq!(
            sb.raw_ok(git.repo(), &["symbolic-ref", "HEAD"]),
            "refs/heads/other"
        );
    }

    #[test]
    fn ignored_crlf_and_text_auto_content_survives_commit_and_clone() {
        let sb = Sandbox::new();
        let src = sb.git("src");
        src.init().expect("init");
        let files: [(&str, &[u8]); 3] = [
            (".gitignore", b"*.conf\n"),
            (".gitattributes", b"* text=auto\n"),
            ("kitty.conf", b"font_size 11\r\nbold_font auto\r\n"),
        ];
        for (name, bytes) in files {
            write(&src, name, bytes);
        }
        src.stage_all().expect("stage");
        src.commit("init").expect("commit").expect("a commit");
        assert_eq!(
            src.show_blob("HEAD", "kitty.conf").expect("blob"),
            files[2].1
        );

        let dst = sb.git("dst");
        dst.clone_from(utf8(src.repo())).expect("clone");
        for (name, bytes) in files {
            assert_eq!(
                fs::read(dst.repo().join(name)).expect("read"),
                bytes,
                "{name}"
            );
        }
        assert_eq!(dst.status().expect("status"), Vec::new());
        let src_id = fs::read_to_string(src.repo().join(".git/dotman-id")).expect("id");
        let dst_id = fs::read_to_string(dst.repo().join(".git/dotman-id")).expect("id");
        assert_eq!(dst_id.len(), 32);
        assert_ne!(src_id, dst_id);
    }

    #[test]
    fn user_global_excludes_do_not_hide_files() {
        let sb = Sandbox::new();
        let ignore_dir = sb.dir.path().join("home/.config/git");
        fs::create_dir_all(&ignore_dir).expect("mkdir");
        fs::write(ignore_dir.join("ignore"), "x.conf\n").expect("ignore");

        let git = sb.git("repo");
        git.init().expect("init");
        write(&git, "x.conf", b"secret-ish\n");
        let plain = sb.raw(git.repo(), &["check-ignore", "x.conf"]);
        assert!(plain.status.success(), "plain git must ignore x.conf");
        assert_eq!(text(&plain), "x.conf");
        let listed: Vec<_> = git
            .status()
            .expect("status")
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(listed, [PathBuf::from("x.conf")]);

        git.stage_all().expect("stage");
        git.commit("add").expect("commit").expect("a commit");
        let tree = sb.raw_ok(git.repo(), &["ls-tree", "-r", "--name-only", "HEAD"]);
        assert_eq!(tree, "x.conf");
    }

    #[test]
    fn names_with_space_and_nfc_accent_round_trip() {
        let sb = Sandbox::new();
        let src = sb.git("src");
        src.init().expect("init");
        let names = ["my file.conf", "caf\u{e9}.conf"];
        for name in names {
            write(&src, name, name.as_bytes());
        }
        let mut seen: Vec<_> = src
            .status()
            .expect("status")
            .into_iter()
            .inspect(|e| assert_eq!(e.kind, StatusKind::Untracked))
            .map(|e| e.path)
            .collect();
        seen.sort();
        let mut want: Vec<_> = names.iter().map(PathBuf::from).collect();
        want.sort();
        assert_eq!(seen, want);

        src.stage_all().expect("stage");
        src.commit("names").expect("commit").expect("a commit");
        let dst = sb.git("dst");
        dst.clone_from(utf8(src.repo())).expect("clone");
        for name in names {
            assert_eq!(
                fs::read(dst.repo().join(name)).expect("read"),
                name.as_bytes()
            );
            assert_eq!(dst.show_blob("HEAD", name).expect("blob"), name.as_bytes());
        }
    }

    #[test]
    fn status_reports_renamed_paths() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        commit_file(&git, "old name.txt", b"some long enough content\n", "one");
        fs::rename(
            git.repo().join("old name.txt"),
            git.repo().join("new name.txt"),
        )
        .expect("rename");
        git.stage_all().expect("stage");
        let entries = git.status().expect("status");
        assert_eq!(entries.len(), 1, "{entries:?}");
        let entry = entries.first().expect("one entry");
        assert_eq!(entry.kind, StatusKind::Renamed);
        assert_eq!(entry.path, Path::new("new name.txt"));
        assert_eq!(entry.orig_path.as_deref(), Some(Path::new("old name.txt")));
    }

    #[test]
    fn commit_returns_none_when_nothing_changed() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        assert_eq!(git.head().expect("head"), None);
        assert_eq!(git.commit("empty").expect("commit"), None);
        let oid = commit_file(&git, "a", b"a", "one");
        assert_eq!(git.head().expect("head"), Some(oid));
        git.stage_all().expect("stage");
        assert_eq!(git.commit("again").expect("commit"), None);
    }

    #[test]
    fn identity_requires_name_and_email() {
        let sb = Sandbox::new();
        let a = sb.git("a");
        a.init().expect("init");
        assert!(a.identity_configured().expect("check"));
        let bare_home = sb.dir.path().join("bare-home");
        fs::create_dir_all(&bare_home).expect("mkdir");
        let git = sb
            .git("a")
            .with_env([("HOME", &bare_home), ("XDG_CONFIG_HOME", &bare_home)]);
        assert!(!git.identity_configured().expect("check"));
    }

    #[test]
    fn author_email_is_read_with_or_without_a_repo() {
        assert_eq!(
            parse_ident_email("A <b> <a@b.invalid> 1700000000 +0000").as_deref(),
            Some("a@b.invalid")
        );
        assert_eq!(parse_ident_email("no email 1 +0000"), None);
        assert_eq!(parse_ident_email("A <> 1 +0000"), None);

        // Pinned in the environment: `git commit` exports GIT_AUTHOR_* to the hooks that run
        // the tests, and those would override the sandbox config. The config path and the
        // missing-identity path run through the binary in tests/cli_import.rs.
        let sb = Sandbox::new();
        let git = sb.git("repo").with_env([
            ("GIT_AUTHOR_NAME", "Pinned"),
            ("GIT_AUTHOR_EMAIL", "pinned@example.invalid"),
        ]);
        let email = |git: &Git| git.author_email().expect("author_email");
        assert_eq!(email(&git).as_deref(), Some("pinned@example.invalid"));
        git.init().expect("init");
        assert_eq!(email(&git).as_deref(), Some("pinned@example.invalid"));
    }

    #[test]
    fn fast_import_builds_a_branch_from_a_stream_file() {
        use std::io::{Seek, Write};
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        let mut stream = tempfile::tempfile().expect("tempfile");
        stream
            .write_all(
                b"blob\nmark :1\ndata 3\nhi\n\ncommit refs/heads/main\nauthor A <a@b> 1700000000 +0000\ncommitter A <a@b> 1700000000 +0000\ndata 2\nm\nM 100644 :1 \"f\"\n\ndone\n",
            )
            .expect("write");
        stream.rewind().expect("rewind");
        git.fast_import(stream).expect("import");
        git.checkout_head().expect("checkout");
        assert_eq!(fs::read(git.repo().join("f")).expect("read"), b"hi\n");

        let mut truncated = tempfile::tempfile().expect("tempfile");
        truncated
            .write_all(
                b"commit refs/heads/other\ncommitter A <a@b> 1700000000 +0000\ndata 2\nm\n\n",
            )
            .expect("write");
        truncated.rewind().expect("rewind");
        let err = git.fast_import(truncated).expect_err("no done");
        assert!(err.to_string().contains("fast-import"), "{err}");
        let other = sb.raw(git.repo(), &["rev-parse", "--verify", "-q", "other"]);
        assert_eq!(other.status.code(), Some(1));
    }

    #[test]
    fn log_rev_before_and_shallow() {
        let sb = Sandbox::new();
        let at = |date: &'static str| {
            sb.git("repo")
                .with_env([("GIT_AUTHOR_DATE", date), ("GIT_COMMITTER_DATE", date)])
        };
        let old = at("2020-01-01T00:00:00+0000");
        old.init().expect("init");
        let first = commit_file(&old, "a.txt", b"1", "first");
        let new = at("2024-01-01T00:00:00+0000");
        let second = commit_file(&new, "b.txt", b"2", "second");

        let all = new.log(&[], None).expect("log");
        let subjects: Vec<_> = all.iter().map(|e| e.subject.as_str()).collect();
        assert_eq!(subjects, ["second", "first"]);
        assert_eq!(all.first().map(|e| e.oid.as_str()), Some(second.as_str()));
        assert_eq!(all.last().map(|e| e.time), Some(1_577_836_800));
        assert_eq!(new.log(&["a.txt"], None).expect("log").len(), 1);
        assert_eq!(new.log(&[], Some(1)).expect("log").len(), 1);

        assert_eq!(new.rev_before("2022-01-01").expect("rev"), Some(first));
        assert_eq!(new.rev_before("2019-01-01").expect("rev"), None);
        assert!(!new.is_shallow().expect("shallow"));

        let url = format!("file://{}", new.repo().display());
        sb.raw_ok(
            sb.dir.path(),
            &["clone", "-q", "--depth", "1", &url, "shallow"],
        );
        assert!(sb.git("shallow").is_shallow().expect("shallow"));
        let unborn = sb.git("unborn");
        unborn.init().expect("init");
        assert_eq!(unborn.log(&[], None).expect("log"), Vec::new());
    }

    #[test]
    fn clone_passes_on_origin_of_local_source() {
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        commit_file(&a, "f", b"f", "one");
        assert_eq!(a.remote_url("origin").expect("url"), None);

        let no_origin = sb.git("no-origin");
        no_origin.clone_from(utf8(a.repo())).expect("clone");
        assert_eq!(
            no_origin.remote_url("origin").expect("url").as_deref(),
            Some(utf8(a.repo()))
        );

        a.set_remote_url("origin", utf8(&remote)).expect("add");
        let b = sb.git("b");
        b.clone_from(utf8(a.repo())).expect("clone");
        assert_eq!(
            b.remote_url("origin").expect("url").as_deref(),
            Some(utf8(&remote))
        );
        assert_eq!(fs::read(b.repo().join("f")).expect("read"), b"f");

        b.set_remote_url("origin", "/elsewhere").expect("set-url");
        assert_eq!(
            b.remote_url("origin").expect("url").as_deref(),
            Some("/elsewhere")
        );

        // An origin git would read as an option is not passed on; the clone keeps its source.
        a.set_remote_url("origin", "--push")
            .expect("set-url after --");
        assert_eq!(
            a.remote_url("origin").expect("url").as_deref(),
            Some("--push")
        );
        let c = sb.git("c");
        c.clone_from(utf8(a.repo())).expect("clone");
        assert_eq!(
            c.remote_url("origin").expect("url").as_deref(),
            Some(utf8(a.repo()))
        );
        c.remove_remote("origin").expect("remove");
    }

    #[test]
    fn a_remote_helper_url_is_not_usable() {
        for (url, usable) in [
            ("ext::sh -c touch% /tmp/x", false),
            ("fd::0", false),
            ("gcrypt::rsync://host/x", false),
            ("a+b.c-d::x", false),
            ("::x", false),
            ("https://host/x::y", true),
            ("/srv/x::y.git", true),
            ("git@host:me/x.git", true),
        ] {
            assert_eq!(usable_remote(url), usable, "{url}");
        }
    }

    #[test]
    fn a_relative_local_origin_is_made_absolute_where_git_reads_it() {
        for (url, local) in [
            ("../x.git", true),
            ("/srv/x.git", true),
            ("./host:x.git", true),
            ("host:x.git", false),
            ("git@host:me/x.git", false),
            ("file:///srv/x.git", false),
        ] {
            assert_eq!(local_path_remote(url), local, "{url}");
        }
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let real = fs::canonicalize(&remote).expect("canonical remote");
        let a = sb.git("a");
        a.init().expect("init");
        commit_file(&a, "f", b"f", "one");
        a.set_remote_url("origin", "../remote.git")
            .expect("relative");
        assert_eq!(
            a.remote_target("origin").expect("target").as_deref(),
            Some(utf8(&real))
        );
        let bare = Git::new(&remote, "test").with_env(sb.env());
        bare.set_remote_url("origin", "../gone.git")
            .expect("relative");
        assert_eq!(
            bare.remote_target("origin").expect("target").as_deref(),
            Some(utf8(&sb.dir.path().join("remote.git/../gone.git")))
        );
        for url in ["host:x.git", "--push", "/srv/x.git", "~/x.git", "~me/x.git"] {
            assert_eq!(absolute_remote(url, sb.dir.path()).expect("as is"), url);
        }

        let b = sb.git("b");
        b.clone_from(utf8(a.repo())).expect("clone");
        assert_eq!(
            b.remote_url("origin").expect("url").as_deref(),
            Some(utf8(&real))
        );

        // Given as its git dir, a non-bare repo still resolves against its work tree.
        let git_dir = a.repo().join(".git");
        let inside = Git::new(&git_dir, "test").with_env(sb.env());
        assert_eq!(
            inside.remote_target("origin").expect("target").as_deref(),
            Some(utf8(&real))
        );
        let c = sb.git("c");
        c.clone_from(utf8(&git_dir)).expect("clone of the git dir");
        assert_eq!(
            c.remote_url("origin").expect("url").as_deref(),
            Some(utf8(&real))
        );
    }

    #[test]
    fn clone_of_empty_repo_leaves_unborn_head() {
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let dst = sb.git("dst");
        dst.clone_from(utf8(&remote)).expect("clone");
        assert_eq!(dst.head().expect("head"), None);
        assert!(dst.repo().join(".git/dotman-id").exists());
    }

    /// A bare remote whose `HEAD` names `master`, with one commit on each of `branches`.
    fn remote_with_unborn_head(sb: &Sandbox, branches: &[&str]) -> PathBuf {
        let remote = sb.bare_remote();
        sb.raw_ok(&remote, &["symbolic-ref", "HEAD", "refs/heads/master"]);
        let src = sb.git("src");
        src.init().expect("init");
        commit_file(&src, "a.txt", b"a\n", "one");
        for branch in branches {
            let target = format!("HEAD:refs/heads/{branch}");
            sb.raw_ok(src.repo(), &["push", "-q", utf8(&remote), &target]);
        }
        remote
    }

    #[test]
    fn a_clone_checks_out_the_only_branch_when_the_remote_head_is_unborn() {
        let sb = Sandbox::new();
        let remote = remote_with_unborn_head(&sb, &["trunk"]);
        let dst = sb.git("dst");
        let adopted = dst.clone_from(utf8(&remote)).expect("clone");
        assert_eq!(
            adopted,
            Some(Adopted {
                branch: "trunk".to_owned(),
                of: 1
            })
        );
        assert!(dst.head().expect("head").is_some());
        assert_eq!(dst.branch().expect("branch").as_deref(), Some("trunk"));
        assert_eq!(fs::read(dst.repo().join("a.txt")).expect("read"), b"a\n");
        assert_eq!(dst.status().expect("status"), Vec::new());
        assert_eq!(dst.adopt_unborn_head(utf8(&remote)).expect("again"), None);
    }

    #[test]
    fn a_clone_prefers_main_among_several_when_the_remote_head_is_unborn() {
        let sb = Sandbox::new();
        let remote = remote_with_unborn_head(&sb, &["dev", "main", "zed"]);
        let dst = sb.git("dst");
        let adopted = dst.clone_from(utf8(&remote)).expect("clone");
        assert_eq!(
            adopted,
            Some(Adopted {
                branch: "main".to_owned(),
                of: 3
            })
        );
        assert_eq!(dst.branch().expect("branch").as_deref(), Some("main"));
        assert_eq!(dst.status().expect("status"), Vec::new());
    }

    #[test]
    fn a_clone_refuses_several_branches_without_main_when_the_remote_head_is_unborn() {
        let sb = Sandbox::new();
        let remote = remote_with_unborn_head(&sb, &["dev", "zed"]);
        let dst = sb.git("dst");
        let err = dst.clone_from(utf8(&remote)).expect_err("refused");
        assert!(
            err.to_string()
                .contains("none of its 2 branches is main (dev, zed)"),
            "{err}"
        );
        assert_eq!(dst.head().expect("head"), None);
        assert_eq!(sb.raw_ok(dst.repo(), &["for-each-ref", "refs/heads"]), "");
    }

    #[test]
    fn clone_head_from_takes_nothing_from_an_unborn_head() {
        let sb = Sandbox::new();
        let remote = remote_with_unborn_head(&sb, &["main"]);
        let dst = sb.git("dst");
        dst.clone_head_from(utf8(&remote)).expect("clone");
        assert_eq!(dst.head().expect("head"), None);
        assert_eq!(sb.raw_ok(dst.repo(), &["for-each-ref"]), "");
        assert!(
            dst.remote_has_any_branch(utf8(&remote)).expect("ls-remote"),
            "the remote has the branch the clone did not take"
        );
    }

    #[test]
    fn pull_rebase_fast_forwards_and_reports_conflicts() {
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        commit_file(&a, "shared.txt", b"base\n", "base");
        a.set_remote_url("origin", utf8(&remote)).expect("remote");
        a.push("origin", "main").expect("push");

        let b = sb.git("b");
        b.clone_from(utf8(a.repo())).expect("clone");

        commit_file(&a, "other.txt", b"o\n", "other");
        a.push("origin", "main").expect("push");
        assert_eq!(
            b.pull_rebase("origin", "main").expect("pull"),
            PullOutcome::Ok
        );
        assert!(b.repo().join("other.txt").exists());

        commit_file(&a, "shared.txt", b"from a\n", "a edits");
        a.push("origin", "main").expect("push");
        let mine = commit_file(&b, "shared.txt", b"from b\n", "b edits");
        let outcome = b.pull_rebase("origin", "main").expect("pull");
        let blob = |bytes: &[u8]| Entry {
            mode: "100644".to_owned(),
            oid: b.write_blob(bytes).expect("blob"),
        };
        assert_eq!(
            outcome,
            PullOutcome::Conflict {
                stopped: vec![Stopped {
                    path: PathBuf::from("shared.txt"),
                    base: Some(blob(b"base\n")),
                    ours: Some(blob(b"from a\n")),
                    replayed: Some(blob(b"from b\n")),
                    follows_rename: false,
                    aside: None,
                }]
            }
        );
        assert!(!b.operation_in_progress());
        assert_eq!(b.head().expect("head"), Some(mine));
        assert_eq!(
            fs::read(b.repo().join("shared.txt")).expect("read"),
            b"from b\n"
        );
    }

    #[test]
    fn pull_rebase_reports_an_uncommitted_edit_that_did_not_go_back() {
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        commit_file(&a, "shared.txt", b"l1\nl2\n", "base");
        a.set_remote_url("origin", utf8(&remote)).expect("remote");
        a.push("origin", "main").expect("push");
        let b = sb.git("b");
        b.clone_from(utf8(a.repo())).expect("clone");
        b.set_remote_url("origin", utf8(&remote)).expect("remote");

        commit_file(&a, "shared.txt", b"l1 from a\nl2\n", "a edits");
        a.push("origin", "main").expect("push");
        write(&b, "shared.txt", b"l1 uncommitted\nl2\n");
        let outcome = b.pull_rebase("origin", "main").expect("pull");
        assert_eq!(
            outcome,
            PullOutcome::Unstashed {
                paths: vec![PathBuf::from("shared.txt")]
            }
        );
        let stash = b.optional(&["rev-parse", "-q", "--verify", "refs/stash"]);
        assert!(stash.expect("stash").is_some(), "the edit stays stashed");
    }

    #[test]
    fn operation_in_progress_sees_rebase_and_merge_markers() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        assert!(!git.operation_in_progress());
        for marker in ["rebase-merge", "rebase-apply"] {
            let dir = git.repo().join(".git").join(marker);
            fs::create_dir(&dir).expect("mkdir");
            assert_eq!(git.operation(), Some(Operation::Rebase), "{marker}");
            fs::remove_dir(&dir).expect("rmdir");
        }
        fs::write(git.repo().join(".git/MERGE_HEAD"), "x").expect("write");
        assert_eq!(git.operation(), Some(Operation::Merge));
    }

    #[test]
    fn branch_and_remote_branch_lookups() {
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        assert_eq!(a.branch().expect("branch").as_deref(), Some("main"));
        a.set_remote_url("origin", utf8(&remote)).expect("remote");
        assert!(!a.remote_has_branch("origin", "main").expect("empty remote"));
        commit_file(&a, "f", b"f", "one");
        a.push_set_upstream("origin", "main").expect("push");
        assert_eq!(
            sb.raw_ok(a.repo(), &["rev-parse", "--abbrev-ref", "@{u}"]),
            "origin/main"
        );
        assert!(a.remote_has_branch("origin", "main").expect("pushed"));
        assert!(!a.remote_has_branch("origin", "ain").expect("tail only"));
        sb.raw_ok(a.repo(), &["checkout", "-q", "--detach"]);
        assert_eq!(a.branch().expect("detached"), None);
        a.set_remote_url("origin", utf8(&sb.dir.path().join("none")))
            .expect("set-url");
        assert!(a.remote_has_branch("origin", "main").is_err());
    }

    /// Every name dotman takes is one `git check-ref-format --branch` takes too; `+main` shows why
    /// git's own rules are not enough.
    #[test]
    fn checked_branch_takes_a_subset_of_what_git_takes() {
        let sb = Sandbox::new();
        let token = format!("ghp_{}", "a".repeat(36));
        let accepted = ["main", "feature/x", "v1.2_rc-3", "a/b/c"];
        let refused = [
            "-x",
            "--mirror",
            "--upload-pack=touch${IFS}x;true",
            "HEAD",
            "",
            "a..b",
            "a/.b",
            ".a",
            "a.lock",
            "a/",
            "/a",
            "a//b",
            "a.",
            "a b",
            "a:b",
            "+main",
            "a=b",
            "a{b",
            "a`b",
            "ä",
            "a\u{1b}b",
            &format!("wip-{token}"),
        ];
        for name in accepted {
            assert_eq!(checked_branch(name).ok().as_deref(), Some(name));
            sb.raw_ok(sb.dir.path(), &["check-ref-format", "--branch", name]);
        }
        for name in refused {
            let err = checked_branch(name).expect_err(name);
            let shown = err.to_string();
            assert!(!shown.contains('\u{1b}'), "{shown}");
            assert!(!shown.contains(&token), "{shown}");
            assert!(shown.contains("`dotman git branch -m main`"), "{shown}");
            assert_eq!(shown.matches('`').count(), 2, "{shown}");
        }
        assert!(
            sb.raw(sb.dir.path(), &["check-ref-format", "--branch", "+main"])
                .status
                .success()
        );
    }

    #[test]
    fn a_fetch_that_outlasts_its_timeout_is_killed_and_fails_as_unreachable() {
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        commit_file(&a, "f", b"f", "one");
        a.set_remote_url("origin", utf8(&remote)).expect("add");
        a.push_set_upstream("origin", "main").expect("push");
        a.fetch_within("origin", "main", Duration::from_secs(30))
            .expect("a quick fetch");

        let slow = sb.dir.path().join("slow-upload-pack");
        fs::write(&slow, "#!/bin/sh\nexec sleep 20\n").expect("write");
        fs::set_permissions(&slow, fs::Permissions::from_mode(0o755)).expect("chmod");
        a.set_config("remote.origin.uploadpack", utf8(&slow), false)
            .expect("config");
        let started = std::time::Instant::now();
        let err = a
            .fetch_within("origin", "main", Duration::from_millis(500))
            .expect_err("a slow remote");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(err.downcast_ref(), Some(&RemoteFailure::Unreachable));
    }

    #[test]
    fn ls_remote_anonymous_reports_reachability() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        let wait = Duration::from_secs(30);
        assert!(git.ls_remote_anonymous(utf8(git.repo()), wait).expect("ls"));
        let missing = sb.dir.path().join("missing");
        assert!(!git.ls_remote_anonymous(utf8(&missing), wait).expect("ls"));
    }

    #[test]
    fn status_ignores_an_fsmonitor_hook_and_submodules() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        let oid = commit_file(&git, "a", b"a", "one");

        // A gitlink whose checkout sits at another commit would read as a modified submodule.
        let gitlink = format!("160000,{oid},sub");
        sb.raw_ok(
            git.repo(),
            &["update-index", "--add", "--cacheinfo", &gitlink],
        );
        sb.raw_ok(git.repo(), &["commit", "-q", "-m", "sub"]);
        let sub = sb.git("repo/sub");
        sub.init().expect("init sub");
        commit_file(&sub, "b", b"b", "elsewhere");
        let plain = sb.raw_ok(
            git.repo(),
            &["-c", "core.fsmonitor=false", "status", "--porcelain"],
        );
        assert!(plain.contains("sub"), "{plain}");

        let marker = sb.dir.path().join("fsmonitor-ran");
        let hook = sb.dir.path().join("fsmonitor.sh");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).expect("hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
        sb.raw_ok(git.repo(), &["config", "core.fsmonitor", utf8(&hook)]);
        assert_eq!(git.status().expect("status"), Vec::new());
        assert!(!marker.exists(), "the fsmonitor hook ran");
    }

    #[test]
    fn repo_hooks_do_not_run() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        let hook = git.repo().join(".git/hooks/pre-commit");
        fs::create_dir_all(hook.parent().expect("parent")).expect("mkdir");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
        sb.raw_ok(git.repo(), &["config", "core.hooksPath", ".git/hooks"]);
        commit_file(&git, "a", b"a", "one");
    }

    /// The user's `rebase.backend`, which dotman's pull and replay override; git before 2.26
    /// ignores it.
    const BACKENDS: [&str; 2] = ["'rebase.backend=merge'", "'rebase.backend=apply'"];

    /// Writes, or with `None` removes, each file, then commits them all.
    fn change(repo: &Git, files: &[(&str, Option<&[u8]>)], msg: &str) -> String {
        for (rel, bytes) in files {
            let path = repo.repo().join(rel);
            match bytes {
                Some(bytes) => {
                    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                    fs::write(&path, bytes).expect("write");
                }
                None => fs::remove_file(&path).expect("remove"),
            }
        }
        repo.stage_all().expect("stage");
        repo.commit(msg).expect("commit").expect("a commit")
    }

    /// Machine `b` whose pull conflicts: `base` is pushed and cloned, origin then commits
    /// `theirs` and `b` commits `mine`. Returns `b`, its head before the pull, the fetched commit
    /// and what the pull stopped on.
    struct Diverged {
        sb: Sandbox,
        b: Git,
        old: String,
        onto: String,
        stopped: Vec<Stopped>,
    }

    type Files<'a> = &'a [(&'a str, Option<&'a [u8]>)];

    fn diverged(backend: &str, base: Files, theirs: Files, mine: &[Files]) -> Diverged {
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        change(&a, base, "base");
        a.set_remote_url("origin", utf8(&remote)).expect("remote");
        a.push("origin", "main").expect("push");
        // An editor that fails: only the replay's own `GIT_EDITOR=true` lets `--continue` commit.
        let b = sb
            .git("b")
            .with_env([("GIT_CONFIG_PARAMETERS", backend), ("GIT_EDITOR", "false")]);
        b.clone_from(utf8(a.repo())).expect("clone");
        change(&a, theirs, "theirs");
        a.push("origin", "main").expect("push");
        let mut old = String::new();
        for (n, files) in mine.iter().enumerate() {
            old = change(&b, files, &format!("mine {n}"));
        }
        let stopped = conflict(b.pull_rebase("origin", "main").expect("pull"));
        let onto = b
            .resolve_commit("FETCH_HEAD")
            .expect("rev-parse")
            .expect("fetched");
        Diverged {
            sb,
            b,
            old,
            onto,
            stopped,
        }
    }

    fn conflict(outcome: PullOutcome) -> Vec<Stopped> {
        match outcome {
            PullOutcome::Conflict { stopped } => Some(stopped),
            _ => None,
        }
        .expect("the pull should conflict")
    }

    fn refused(recovery: Recovery) -> Refusal {
        match recovery {
            Recovery::Refused(refusal) => Some(refusal),
            _ => None,
        }
        .expect("recovery refuses")
    }

    fn set(git: &Git, bytes: &[u8]) -> Decision {
        Decision::Set(Entry {
            mode: "100644".to_owned(),
            oid: git.write_blob(bytes).expect("blob"),
        })
    }

    fn decide(pairs: Vec<(&str, Decision)>) -> BTreeMap<PathBuf, Decision> {
        pairs
            .into_iter()
            .map(|(path, decision)| (PathBuf::from(path), decision))
            .collect()
    }

    fn lines(changed: &[(usize, &str)]) -> Vec<u8> {
        (1..=10_usize)
            .map(|n| {
                changed
                    .iter()
                    .find(|(at, _)| *at == n)
                    .map_or_else(|| format!("l{n}\n"), |(_, line)| format!("{line}\n"))
            })
            .collect::<String>()
            .into_bytes()
    }

    /// The repo is as before the pull: on its branch at `old`, clean, nothing left of the replay.
    fn assert_restored(d: &Diverged) {
        let b = &d.b;
        assert!(!b.own_pull(), "the marker is gone");
        assert_eq!(b.rebase_dir(), None);
        assert_eq!(b.branch().expect("branch").as_deref(), Some("main"));
        assert_eq!(b.head().expect("head"), Some(d.old.clone()));
        let status = d.sb.raw_ok(b.repo(), &["status", "--porcelain", "-uall"]);
        assert_eq!(status, "", "the worktree is clean");
        assert_eq!(
            d.sb.raw_ok(b.repo(), &["write-tree"]),
            d.sb.raw_ok(b.repo(), &["rev-parse", &format!("{}^{{tree}}", d.old)]),
            "the index is the old head's tree"
        );
    }

    fn blob_at(git: &Git, rev: &str, path: &str) -> Vec<u8> {
        git.show_blob(rev, path).expect("blob")
    }

    #[test]
    fn a_replay_sets_each_settled_path_and_corrects_once() {
        for backend in BACKENDS {
            for correct in [false, true] {
                let conf_base: &[u8] = b"c1\nc2\nc3\nc4\nc5\n";
                let conf_theirs: &[u8] = b"c1 origin\nc2\nc3\nc4\nc5\n";
                let base = lines(&[]);
                let theirs = lines(&[(2, "origin2"), (9, "origin9")]);
                let mine = lines(&[(2, "local2"), (5, "local5")]);
                let d = diverged(
                    backend,
                    &[("a.txt", Some(&base)), ("conf", Some(conf_base))],
                    &[("a.txt", Some(&theirs)), ("conf", Some(conf_theirs))],
                    &[&[
                        ("a.txt", Some(&mine)),
                        ("conf", Some(b"c1\nc2\nc3\nc4\nc5 local\n")),
                    ]],
                );
                let paths: Vec<&Path> = d.stopped.iter().map(|s| s.path.as_path()).collect();
                assert_eq!(paths, [Path::new("a.txt")], "{backend}");
                let tmp = TempDir::new().expect("tmp");
                let (merged, clean) =
                    d.b.merge_file(
                        tmp.path(),
                        [&mine, &base, &theirs],
                        ["this machine (beta)", "base", "origin/main"],
                        Some(Side::Ours),
                    )
                    .expect("merge-file");
                assert!(clean);
                let want = lines(&[(2, "local2"), (5, "local5"), (9, "origin9")]);
                assert_eq!(merged, want);
                let mut decisions = vec![("a.txt", set(&d.b, &merged))];
                if correct {
                    decisions.push(("conf", set(&d.b, conf_theirs)));
                }
                let replayed =
                    d.b.replay("origin", "main", &d.onto, &decide(decisions), "beta")
                        .expect("replay");
                assert_eq!(replayed, Replayed::Done, "{backend}");
                assert!(!d.b.own_pull());
                assert_eq!(d.b.branch().expect("branch").as_deref(), Some("main"));
                assert_eq!(blob_at(&d.b, "HEAD", "a.txt"), want);
                assert_eq!(fs::read(d.b.repo().join("a.txt")).expect("read"), want);
                let count = d.b.count_commits(Some(&d.onto), "HEAD").expect("count");
                let subject = d.sb.raw_ok(d.b.repo(), &["log", "-1", "--format=%s"]);
                if correct {
                    assert_eq!(count, 2, "{backend}");
                    assert_eq!(subject, "sync(beta): resolve conf");
                    assert_eq!(blob_at(&d.b, "HEAD", "conf"), conf_theirs);
                } else {
                    assert_eq!(count, 1, "{backend}: no correction commit");
                    assert_eq!(subject, "mine 0");
                }
                let status = d.sb.raw_ok(d.b.repo(), &["status", "--porcelain", "-uall"]);
                assert_eq!(status, "");
            }
        }
    }

    /// Taking origin's version whole empties the local commit, which the replay drops.
    #[test]
    fn a_commit_the_replay_empties_is_skipped() {
        for backend in BACKENDS {
            let theirs = lines(&[(2, "origin2")]);
            let d = diverged(
                backend,
                &[("a.txt", Some(&lines(&[])))],
                &[("a.txt", Some(&theirs))],
                &[&[("a.txt", Some(&lines(&[(2, "local2")])))]],
            );
            let decisions = decide(vec![("a.txt", set(&d.b, &theirs))]);
            let replayed =
                d.b.replay("origin", "main", &d.onto, &decisions, "beta")
                    .expect("replay");
            assert_eq!(replayed, Replayed::Done, "{backend}");
            assert_eq!(d.b.head().expect("head"), Some(d.onto.clone()), "{backend}");
            assert_eq!(d.b.branch().expect("branch").as_deref(), Some("main"));
            assert_eq!(fs::read(d.b.repo().join("a.txt")).expect("read"), theirs);
        }
    }

    /// Two local commits stop on two paths; only the first has a decision.
    #[test]
    fn an_unsettled_path_leaves_the_repo_as_it_was() {
        for backend in BACKENDS {
            let d = diverged(
                backend,
                &[("a.txt", Some(b"a\n")), ("b.txt", Some(b"b\n"))],
                &[
                    ("a.txt", Some(b"a origin\n")),
                    ("b.txt", Some(b"b origin\n")),
                ],
                &[
                    &[("a.txt", Some(b"a local\n"))],
                    &[("b.txt", Some(b"b local\n"))],
                ],
            );
            let decisions = decide(vec![("a.txt", set(&d.b, b"a local\n"))]);
            let replayed =
                d.b.replay("origin", "main", &d.onto, &decisions, "beta")
                    .expect("replay");
            let stopped = match replayed {
                Replayed::Stopped(stopped) => Some(stopped),
                Replayed::Done => None,
            }
            .expect("the second commit stops on b.txt");
            let paths: Vec<&Path> = stopped.iter().map(|s| s.path.as_path()).collect();
            assert_eq!(paths, [Path::new("b.txt")]);
            assert_restored(&d);
            assert_eq!(
                fs::read(d.b.repo().join("b.txt")).expect("read"),
                b"b local\n"
            );
        }
    }

    /// git 2.25 leaves its copy of the file as an untracked `x~<commit>`, which the stop names
    /// and both the first pull's abort and the replay's undo remove.
    #[test]
    fn a_file_against_a_directory_stops_and_leaves_nothing_behind() {
        for backend in BACKENDS {
            let d = diverged(
                backend,
                &[("base", Some(b"b\n"))],
                &[("x/y", Some(b"dir\n"))],
                &[&[("x", Some(b"file\n"))]],
            );
            let copies = d
                .stopped
                .iter()
                .map(|stop| stop.aside.as_ref().unwrap_or(&stop.path))
                .filter(|copy| copy.to_string_lossy().starts_with("x~"))
                .count();
            assert_eq!(copies, 1, "{backend}: {:?}", d.stopped);
            let status = d.sb.raw_ok(d.b.repo(), &["status", "--porcelain", "-uall"]);
            assert_eq!(status, "", "{backend}: the pull left nothing behind");
            let replayed =
                d.b.replay("origin", "main", &d.onto, &BTreeMap::new(), "beta")
                    .expect("replay");
            assert!(
                matches!(replayed, Replayed::Stopped(ref stopped) if !stopped.is_empty()),
                "{backend}: {replayed:?}"
            );
            assert_restored(&d);
        }
    }

    /// git 2.25's layout of a file against a directory, made by hand where git is newer: the
    /// stage at `x` itself and the copy untracked. The stop names the copy, and the abort's
    /// cleanup removes it.
    #[test]
    fn the_untracked_copy_git_2_25_sets_aside_is_named_and_removed() {
        let d = diverged(
            BACKENDS[0],
            &[("base", Some(b"b\n"))],
            &[("x/y", Some(b"dir\n"))],
            &[&[("x", Some(b"file\n"))]],
        );
        let b = &d.b;
        let before = b.untracked().expect("untracked");
        let by_hand = d.sb.raw(b.repo(), &["rebase", "-m", &d.onto]);
        assert!(!by_hand.status.success());
        let zero = "0".repeat(40);
        for (path, [_, _, theirs]) in b.unmerged_entries().expect("unmerged") {
            if let Some(entry) = theirs.filter(|_| path != Path::new("x")) {
                let info = format!(
                    "0 {zero}\t{}\n{} {} 3\tx\n",
                    path.display(),
                    entry.mode,
                    entry.oid
                );
                b.run_stdin(&["update-index", "--index-info"], info.as_bytes())
                    .expect("index-info");
            }
        }
        let stopped = b.stopped(None, &before).expect("stopped");
        let file = stopped
            .iter()
            .find(|stop| stop.path == Path::new("x"))
            .expect("the stop at x");
        let copy = file.aside.clone().expect("the copy is named");
        assert!(copy.to_string_lossy().starts_with("x~"), "{copy:?}");
        assert_eq!(fs::read(b.repo().join(&copy)).expect("copy"), b"file\n");
        let blobs = b.stop_blobs().expect("blobs");
        d.sb.raw_ok(b.repo(), &["rebase", "--abort"]);
        assert!(b.repo().join(&copy).exists(), "the abort keeps the copy");
        b.remove_leftovers(&before, &blobs).expect("remove");
        assert_restored(&d);
    }

    /// Origin removes `gone` while this machine edits it, and `twin` holds what `gone` held. The
    /// apply backend, which the user's config may pick, would read `gone` as renamed to `twin`
    /// and put the edit there with no conflict.
    #[test]
    fn an_edit_against_a_removal_stops_whatever_backend_the_user_set() {
        for backend in BACKENDS {
            let d = diverged(
                backend,
                &[("gone", Some(b"same\n")), ("twin", Some(b"same\n"))],
                &[("gone", None)],
                &[&[("gone", Some(b"edited\n"))]],
            );
            let stops: Vec<(&Path, bool, bool)> = d
                .stopped
                .iter()
                .map(|s| (s.path.as_path(), s.ours.is_some(), s.replayed.is_some()))
                .collect();
            assert_eq!(stops, [(Path::new("gone"), false, true)], "{backend}");
            assert_restored(&d);
            let decisions = decide(vec![("gone", set(&d.b, b"edited\n"))]);
            let replayed =
                d.b.replay("origin", "main", &d.onto, &decisions, "beta")
                    .expect("replay");
            assert_eq!(replayed, Replayed::Done, "{backend}");
            assert_eq!(blob_at(&d.b, "HEAD", "gone"), b"edited\n", "{backend}");
            assert_eq!(blob_at(&d.b, "HEAD", "twin"), b"same\n", "{backend}");
            assert_eq!(fs::read(d.b.repo().join("twin")).expect("twin"), b"same\n");
        }
    }

    /// Origin and this machine make the same edit, which leaves this machine's first commit
    /// empty; git before 2.26 stops on it where later gits drop it.
    #[test]
    fn a_commit_the_pull_empties_is_dropped() {
        for backend in BACKENDS {
            let sb = Sandbox::new();
            let remote = sb.bare_remote();
            let a = sb.git("a");
            a.init().expect("init");
            change(&a, &[("x", Some(b"x\n")), ("y", Some(b"y\n"))], "base");
            a.set_remote_url("origin", utf8(&remote)).expect("remote");
            a.push("origin", "main").expect("push");
            let b = sb
                .git("b")
                .with_env([("GIT_CONFIG_PARAMETERS", backend), ("GIT_EDITOR", "false")]);
            b.clone_from(utf8(a.repo())).expect("clone");
            change(&a, &[("x", Some(b"X\n")), ("y", Some(b"Y\n"))], "theirs");
            a.push("origin", "main").expect("push");
            change(&b, &[("x", Some(b"X\n"))], "mine 0");
            change(&b, &[("z", Some(b"z\n"))], "mine 1");
            assert_eq!(
                b.pull_rebase("origin", "main").expect("pull"),
                PullOutcome::Ok,
                "{backend}"
            );
            let onto = b.resolve_commit("FETCH_HEAD").expect("rev-parse");
            assert_eq!(
                b.resolve_commit("HEAD^").expect("rev-parse"),
                onto,
                "{backend}"
            );
            assert_eq!(blob_at(&b, "HEAD", "z"), b"z\n");
            assert_eq!(b.rebase_dir(), None);
        }
    }

    /// A stop on a commit `HEAD` already holds, made by hand where git drops such a commit, is
    /// one to skip; a stop in conflict, or on a commit `HEAD` lacks, is not.
    #[test]
    fn only_a_stop_on_a_commit_head_already_holds_is_empty() {
        let d = diverged(
            BACKENDS[0],
            &[
                ("x", Some(b"x\n")),
                ("y", Some(b"y\n")),
                ("c", Some(b"c\n")),
            ],
            &[
                ("x", Some(b"X\n")),
                ("y", Some(b"Y\n")),
                ("c", Some(b"c origin\n")),
            ],
            &[&[("x", Some(b"X\n"))], &[("c", Some(b"c local\n"))]],
        );
        let b = &d.b;
        let mut args = vec!["rebase", "-m"];
        match b.version().expect("version") {
            v if v >= (2, 45, 0) => args.push("--empty=stop"),
            v if v >= (2, 26, 0) => args.push("--empty=ask"),
            _ => {}
        }
        let fork = b.merge_base(&d.onto, &d.old).expect("base").expect("fork");
        args.extend(["--onto", d.onto.as_str(), fork.as_str()]);
        assert!(!d.sb.raw(b.repo(), &args).status.success());
        assert!(
            b.stopped_empty().expect("empty"),
            "the first commit holds X"
        );
        let later = b.resolve_commit(&d.old).expect("rev-parse").expect("old");
        d.sb.raw_ok(b.repo(), &["update-ref", "REBASE_HEAD", &later]);
        assert!(!b.stopped_empty().expect("empty"), "HEAD lacks c local");
        assert!(!d.sb.raw(b.repo(), &["rebase", "--skip"]).status.success());
        assert_eq!(b.unmerged_paths().expect("unmerged"), [Path::new("c")]);
        assert!(!b.stopped_empty().expect("empty"), "c is in conflict");
        d.sb.raw_ok(b.repo(), &["rebase", "--abort"]);
    }

    #[test]
    fn a_key_named_dash_x_settles_and_is_removed() {
        for backend in BACKENDS {
            let d = diverged(
                backend,
                &[("-x", Some(b"x\n")), ("-y", Some(b"y\n"))],
                &[("-x", Some(b"x origin\n")), ("-y", Some(b"y origin\n"))],
                &[&[("-x", Some(b"x local\n")), ("-y", Some(b"y local\n"))]],
            );
            let decisions = decide(vec![
                ("-x", set(&d.b, b"x settled\n")),
                ("-y", Decision::Absent),
            ]);
            let replayed =
                d.b.replay("origin", "main", &d.onto, &decisions, "beta")
                    .expect("replay");
            assert_eq!(replayed, Replayed::Done, "{backend}");
            assert_eq!(blob_at(&d.b, "HEAD", "-x"), b"x settled\n");
            let tree = d.b.tree_entries("HEAD").expect("tree");
            assert!(!tree.contains_key(Path::new("-y")), "{backend}");
            assert!(!d.b.repo().join("-y").exists(), "{backend}");
            let status = d.sb.raw_ok(d.b.repo(), &["status", "--porcelain", "-uall"]);
            assert_eq!(status, "");
        }
    }

    /// Origin turns `.p` into a template with an edit to the line this machine edits too, so git
    /// stops on `.p{tmpl}` with this machine's `.p` as the replayed stage.
    #[test]
    fn stopped_marks_a_path_that_followed_a_rename() {
        let body = lines(&[]);
        let sb = Sandbox::new();
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        change(&a, &[(".p", Some(&body)), ("x", Some(b"x\n"))], "base");
        a.set_remote_url("origin", utf8(&remote)).expect("remote");
        a.push("origin", "main").expect("push");
        let b = sb.git("b");
        b.clone_from(utf8(a.repo())).expect("clone");
        sb.raw_ok(a.repo(), &["mv", ".p", ".p{tmpl}"]);
        change(
            &a,
            &[
                (".p{tmpl}", Some(&lines(&[(1, "{{ origin }}")]))),
                ("x", Some(b"x origin\n")),
            ],
            "theirs",
        );
        a.push("origin", "main").expect("push");
        let local = lines(&[(1, "local")]);
        change(
            &b,
            &[(".p", Some(&local)), ("x", Some(b"x local\n"))],
            "mine",
        );
        let stopped = conflict(b.pull_rebase("origin", "main").expect("pull"));
        let followed: Vec<(&Path, bool)> = stopped
            .iter()
            .map(|s| (s.path.as_path(), s.follows_rename))
            .collect();
        assert_eq!(
            followed,
            [(Path::new(".p{tmpl}"), true), (Path::new("x"), false)]
        );
        let renamed = stopped.first().expect("first");
        let local_oid = b.write_blob(&local).expect("blob");
        assert_eq!(
            renamed.replayed.as_ref().map(|e| e.oid.as_str()),
            Some(local_oid.as_str())
        );
    }

    #[test]
    fn the_marker_tells_dotmans_rebase_from_one_started_by_hand() {
        let d = diverged(
            BACKENDS[0],
            &[("a.txt", Some(b"a\n"))],
            &[("a.txt", Some(b"a origin\n"))],
            &[&[("a.txt", Some(b"a local\n"))]],
        );
        let b = &d.b;
        // By hand: a rebase dir, no marker. Recovery has nothing to do with it.
        let by_hand = d.sb.raw(b.repo(), &["rebase", &d.onto]);
        assert!(!by_hand.status.success());
        assert!(b.rebase_dir().is_some());
        assert!(!b.own_pull());
        assert!(matches!(
            b.recover_pull().expect("recover"),
            Recovery::NoMarker
        ));
        d.sb.raw_ok(b.repo(), &["rebase", "--abort"]);

        // Under the marker, a rebase dir onto another commit, or with no `onto` yet, is refused.
        let decisions = decide(vec![("a.txt", set(b, b"a local\n"))]);
        for onto in [Some(d.old.as_str()), None] {
            let (marker, fork) = b
                .start_replay("origin", "main", &d.onto, &decisions)
                .expect("start");
            b.rebase(&["--merge", "--onto", &marker.onto, &fork])
                .expect("rebase");
            let dir = b.rebase_dir().expect("stopped");
            match onto {
                Some(other) => fs::write(dir.join("onto"), format!("{other}\n")).expect("onto"),
                None => fs::remove_file(dir.join("onto")).expect("no onto"),
            }
            let before = d.sb.raw_ok(b.repo(), &["status", "--porcelain", "-uall"]);
            let refusal = refused(b.recover_pull().expect("recover"));
            let shown = refusal.to_string();
            assert!(shown.contains("a rebase dotman did not start"), "{shown}");
            assert!(shown.contains(&d.old), "{shown}");
            assert!(shown.contains("dotman git rebase --quit"), "{shown}");
            assert!(shown.contains("dotman git checkout -f main"), "{shown}");
            assert!(b.own_pull(), "the marker stays");
            assert_eq!(
                d.sb.raw_ok(b.repo(), &["status", "--porcelain", "-uall"]),
                before,
                "nothing was reset"
            );
            // The printed steps, through plain git.
            d.sb.raw_ok(b.repo(), &["rebase", "--quit"]);
            d.sb.raw_ok(b.repo(), &["checkout", "-q", "-f", "main"]);
            fs::remove_file(b.marker_path()).expect("rm");
            assert_restored(&d);
        }
    }

    /// Each kill point a replay can die at, reached by running its steps and stopping; recovery
    /// then restores the old head while the branch has not moved, and attaches to the result
    /// once it has. The replay runs again afterwards.
    #[test]
    fn recovery_puts_back_a_replay_killed_at_each_step() {
        for backend in BACKENDS {
            for kill in [
                "stop",
                "stop-settled",
                "rebased",
                "correction-staged",
                "update-ref",
                "symbolic-ref",
            ] {
                let theirs = lines(&[(2, "origin2")]);
                let d = diverged(
                    backend,
                    &[("a.txt", Some(&lines(&[]))), ("conf", Some(b"c\n"))],
                    &[("a.txt", Some(&theirs))],
                    &[&[
                        ("a.txt", Some(&lines(&[(2, "local2")]))),
                        ("conf", Some(b"c local\n")),
                    ]],
                );
                let b = &d.b;
                let decisions = decide(vec![
                    ("a.txt", set(b, &lines(&[(2, "settled")]))),
                    ("conf", set(b, b"c settled\n")),
                ]);
                let (marker, fork) = b
                    .start_replay("origin", "main", &d.onto, &decisions)
                    .expect("start");
                if kill.starts_with("stop") {
                    b.rebase(&["--merge", "--onto", &marker.onto, &fork])
                        .expect("rebase");
                    assert!(b.rebase_dir().is_some(), "{backend} {kill}");
                    if kill == "stop-settled" {
                        let at = decide(vec![("a.txt", set(b, &lines(&[(2, "settled")])))]);
                        let at: Vec<(PathBuf, Decision)> = at.into_iter().collect();
                        b.settle(&at).expect("settle");
                    }
                } else {
                    assert_eq!(b.finish_rebase(&marker, &fork).expect("rebase"), None);
                    if kill == "correction-staged" {
                        let conf = vec![(PathBuf::from("conf"), set(b, b"c settled\n"))];
                        b.settle(&conf).expect("settle");
                    } else {
                        b.correct(&marker, "beta").expect("correct");
                    }
                    if kill == "update-ref" || kill == "symbolic-ref" {
                        b.move_branch(&marker).expect("update-ref");
                    }
                    if kill == "symbolic-ref" {
                        b.run(&["symbolic-ref", "HEAD", "refs/heads/main"])
                            .expect("symbolic-ref");
                    }
                }
                let moved = kill == "update-ref" || kill == "symbolic-ref";
                let result = b.head().expect("head");
                let recovery = b.recover_pull().expect("recover");
                if moved {
                    assert!(
                        matches!(recovery, Recovery::Finished),
                        "{backend} {kill}: {recovery:?}"
                    );
                    assert!(!b.own_pull());
                    assert_eq!(b.branch().expect("branch").as_deref(), Some("main"));
                    assert_eq!(b.head().expect("head"), result);
                    assert_eq!(blob_at(b, "HEAD", "conf"), b"c settled\n");
                    let status = d.sb.raw_ok(b.repo(), &["status", "--porcelain", "-uall"]);
                    assert_eq!(status, "");
                    continue;
                }
                // Recovery cannot tell git's conflict markers in a decided path at a stop from an
                // edit, so it keeps a copy of them.
                let saved = match &recovery {
                    Recovery::Restored { saved } => saved.as_ref().map(|saved| saved.paths.clone()),
                    _ => Some(Vec::new()),
                };
                let markers = (kill == "stop").then(|| vec![PathBuf::from("a.txt")]);
                assert_eq!(saved, markers, "{backend} {kill}: {recovery:?}");
                assert_restored(&d);
                let again = b
                    .replay("origin", "main", &d.onto, &decisions, "beta")
                    .expect("replay");
                assert_eq!(again, Replayed::Done, "{backend} {kill}");
                assert_eq!(blob_at(b, "HEAD", "a.txt"), lines(&[(2, "settled")]));
            }
        }
    }

    /// The stopped commit also changes `b.txt` cleanly, which git stages: that is the rebase's
    /// own work, not a change of the user's.
    #[test]
    fn a_stop_with_a_second_file_staged_recovers() {
        for backend in BACKENDS {
            let d = diverged(
                backend,
                &[("a.txt", Some(b"a\n")), ("b.txt", Some(b"b\n"))],
                &[("a.txt", Some(b"a origin\n"))],
                &[&[("a.txt", Some(b"a local\n")), ("b.txt", Some(b"b local\n"))]],
            );
            let b = &d.b;
            let decisions = decide(vec![("a.txt", set(b, b"a local\n"))]);
            let (marker, fork) = b
                .start_replay("origin", "main", &d.onto, &decisions)
                .expect("start");
            b.rebase(&["--merge", "--onto", &marker.onto, &fork])
                .expect("rebase");
            let staged = d.sb.raw_ok(b.repo(), &["diff", "--cached", "--name-only"]);
            assert!(staged.contains("b.txt"), "{backend}: {staged}");
            let recovery = b.recover_pull().expect("recover");
            assert!(
                matches!(recovery, Recovery::Restored { .. }),
                "{recovery:?}"
            );
            assert_restored(&d);
        }
    }

    /// A change the user made in the worktree after the kill is never dropped: recovery refuses.
    #[test]
    fn recovery_refuses_a_change_outside_the_settled_paths() {
        let d = diverged(
            BACKENDS[0],
            &[("a.txt", Some(b"a\n")), ("other", Some(b"o\n"))],
            &[("a.txt", Some(b"a origin\n"))],
            &[&[("a.txt", Some(b"a local\n"))]],
        );
        let b = &d.b;
        let decisions = decide(vec![("a.txt", set(b, b"a local\n"))]);
        b.start_replay("origin", "main", &d.onto, &decisions)
            .expect("start");
        write(b, "other", b"edited by hand\n");
        let refusal = refused(b.recover_pull().expect("recover"));
        assert!(
            refusal.to_string().contains("other has changes"),
            "{refusal}"
        );
        assert_eq!(
            fs::read(b.repo().join("other")).expect("read"),
            b"edited by hand\n"
        );
        assert!(b.own_pull());

        fs::write(b.repo().join(".git/index.lock"), b"").expect("lock");
        assert!(matches!(
            b.recover_pull().expect("recover"),
            Recovery::Locked(locks) if locks.len() == 1
        ));
    }

    /// The user's config would move a second branch, answer from a recorded resolution and add a
    /// base section to the markers; none of it reaches the replay or `merge_file`.
    #[test]
    fn the_user_s_rebase_rerere_and_conflict_style_settings_are_pinned() {
        for backend in BACKENDS {
            let d = diverged(
                backend,
                &[("a.txt", Some(&lines(&[])))],
                &[("a.txt", Some(&lines(&[(2, "origin2")])))],
                &[&[("a.txt", Some(&lines(&[(2, "local2")])))]],
            );
            let config = d.sb.dir.path().join("home/.gitconfig");
            let mut text = fs::read_to_string(&config).expect("gitconfig");
            text.push_str(
                "[rebase]\n\tupdateRefs = true\n[rerere]\n\tenabled = true\n\tautoUpdate = true\n[merge]\n\tconflictStyle = diff3\n",
            );
            fs::write(&config, text).expect("gitconfig");
            let b = &d.b;
            d.sb.raw_ok(b.repo(), &["branch", "keep", &d.old]);
            let tmp = TempDir::new().expect("tmp");
            let (marked, clean) = b
                .merge_file(
                    tmp.path(),
                    [
                        &lines(&[(2, "local2")]),
                        &lines(&[]),
                        &lines(&[(2, "origin2")]),
                    ],
                    ["this machine (b\u{1b}[2Jeta)", "base", "origin/main"],
                    None,
                )
                .expect("merge-file");
            let marked = String::from_utf8(marked).expect("utf8");
            assert!(!clean);
            assert!(!marked.contains("|||||||"), "{marked}");
            assert!(!marked.contains('\u{1b}'), "{marked}");
            assert!(marked.contains("<<<<<<< this machine (b"), "{marked}");

            let decisions = decide(vec![("a.txt", set(b, &lines(&[(2, "settled")])))]);
            let replayed = b
                .replay("origin", "main", &d.onto, &decisions, "beta")
                .expect("replay");
            assert_eq!(replayed, Replayed::Done, "{backend}");
            assert_eq!(
                b.resolve_commit("refs/heads/keep").expect("keep"),
                Some(d.old.clone()),
                "{backend}: the second branch did not move"
            );
            assert_eq!(blob_at(b, "HEAD", "a.txt"), lines(&[(2, "settled")]));
        }
    }

    /// `rebase.updateRefs=true` would move a second branch at the local tip along with the first
    /// pull's rebase.
    #[test]
    fn the_first_pull_moves_no_other_branch() {
        let sb = Sandbox::new();
        let config = sb.dir.path().join("home/.gitconfig");
        let mut text = fs::read_to_string(&config).expect("gitconfig");
        text.push_str("[rebase]\n\tupdateRefs = true\n\tbackend = merge\n");
        fs::write(&config, text).expect("gitconfig");
        let remote = sb.bare_remote();
        let a = sb.git("a");
        a.init().expect("init");
        commit_file(&a, "base", b"b\n", "base");
        a.set_remote_url("origin", utf8(&remote)).expect("remote");
        a.push("origin", "main").expect("push");
        let b = sb.git("b");
        b.clone_from(utf8(a.repo())).expect("clone");
        commit_file(&a, "theirs", b"t\n", "theirs");
        a.push("origin", "main").expect("push");
        let mine = commit_file(&b, "mine", b"m\n", "mine");
        sb.raw_ok(b.repo(), &["branch", "keep"]);
        assert_eq!(
            b.pull_rebase("origin", "main").expect("pull"),
            PullOutcome::Ok
        );
        assert_ne!(b.head().expect("head"), Some(mine.clone()));
        assert_eq!(
            b.resolve_commit("refs/heads/keep").expect("keep"),
            Some(mine)
        );
    }

    /// git 2.25's `restore` and every git's `rm` delete through a symlinked parent; the settle
    /// refuses before anything runs.
    #[test]
    fn the_symlink_parent_guard_refuses_and_leaves_an_outside_file_intact() {
        let sb = Sandbox::new();
        let git = sb.git("repo");
        git.init().expect("init");
        change(&git, &[("d/f", Some(b"inside\n"))], "base");
        let outside = sb.dir.path().join("outside");
        fs::create_dir(&outside).expect("mkdir");
        fs::write(outside.join("f"), b"outside\n").expect("write");
        fs::remove_dir_all(git.repo().join("d")).expect("rm d");
        std::os::unix::fs::symlink(&outside, git.repo().join("d")).expect("symlink");
        let blob = set(&git, b"settled\n");
        for decision in [Decision::Absent, blob] {
            let err = git
                .settle(&[(PathBuf::from("d/f"), decision)])
                .expect_err("refused");
            assert!(err.to_string().contains("lies under a symlink"), "{err}");
            assert_eq!(fs::read(outside.join("f")).expect("read"), b"outside\n");
        }
        assert!(git.remove_worktree_file(Path::new("d/f")).is_err());
        assert_eq!(fs::read(outside.join("f")).expect("read"), b"outside\n");
        assert!(
            fs::symlink_metadata(git.repo().join("d"))
                .expect("d")
                .is_symlink()
        );
    }

    #[test]
    fn the_marker_reads_back_any_path_and_refuses_anything_else() {
        let oid = "a".repeat(40);
        let marker = Marker {
            branch: "main".to_owned(),
            old: oid.clone(),
            onto: "b".repeat(40),
            decisions: decide(vec![
                (
                    "tab\tand\nnewline",
                    Decision::Set(Entry {
                        mode: "100755".to_owned(),
                        oid,
                    }),
                ),
                ("-x", Decision::Absent),
            ]),
            untracked: std::iter::once(PathBuf::from("x~local")).collect(),
        };
        let bytes = marker.to_bytes();
        assert_eq!(Marker::parse(&bytes), Some(marker));
        assert_eq!(Marker::parse(b""), None);
        assert_eq!(Marker::parse(b"dotman-pull 2\0main\0"), None);
        let bad_mode = bytes
            .windows(6)
            .position(|w| w == b"100755")
            .map(|at| {
                let mut copy = bytes.clone();
                copy.splice(at..at.saturating_add(6), *b"10 755");
                copy
            })
            .expect("mode");
        assert_eq!(Marker::parse(&bad_mode), None);
    }
}
