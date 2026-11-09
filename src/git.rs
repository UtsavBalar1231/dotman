//! Git CLI wrapper that ignores the user's global git configuration where it would change stored content.
//!
//! Every fallible method returns an error naming the failing git arguments and its stderr.

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_128;

const ATTRIBUTES: &str = "* -text -filter -ident -working-tree-encoding\n";

const MIN_VERSION: (u32, u32) = (2, 25);

/// The write engine's temp files, which are never staged.
const TEMP_FILES: &str = ":(exclude,glob)**/.dotman-tmp-*";
const TRACKED_TEMP_FILES: &str = ":(glob)**/.dotman-tmp-*";

/// `core.excludesFile=` must stay empty: otherwise the user's global ignore file hides files from `git status`.
/// `core.hooksPath=/dev/null` makes every hook lookup fail, so no hook runs.
/// `core.fsmonitor=false` keeps a configured monitor daemon from answering for the worktree.
/// `status.renames=true` keeps the rename records that a held rename relies on, whatever
/// `status.renames` or `diff.renames` the user set.
const ISOLATION: [&str; 18] = [
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
    "core.fsmonitor=false",
    "-c",
    "status.renames=true",
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

/// A git repository driven through the `git` binary with config isolation.
#[derive(Debug)]
pub struct Git {
    repo: PathBuf,
    dot_command: &'static str,
    env: Vec<(OsString, OsString)>,
    version_checked: OnceLock<()>,
}

/// Result of [`Git::pull_rebase`].
#[derive(Debug, PartialEq, Eq)]
pub enum PullOutcome {
    /// The pull and rebase finished.
    Ok,
    /// The rebase hit conflicts and was aborted; the working tree is as before the pull.
    Conflict {
        /// Repo-relative, as git reports them.
        paths: Vec<PathBuf>,
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

/// Which `git status --porcelain=v2` record an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// A changed tracked path (`1` record).
    Ordinary,
    /// A renamed or copied path (`2` record).
    Renamed,
    /// A path with unresolved conflicts (`u` record).
    Unmerged,
    /// A path git does not track (`?` record).
    Untracked,
}

/// One parsed `git status --porcelain=v2 -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// The record type this entry was parsed from.
    pub kind: StatusKind,
    /// Two-letter staged/unstaged code such as `M.`; `??` for untracked.
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
        }
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
    /// or `HEAD` is corrupt.
    fn failure<S: AsRef<OsStr>>(&self, args: &[S], out: &Output) -> anyhow::Error {
        let err = failure(args, out);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let git_dir = self.repo.join(".git");
        let index = git_dir.join("index");
        let broken = if stderr.contains("index file") || stderr.contains("bad index") {
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
        self.run(&["init", "-q"])?;
        if self.head()?.is_none() {
            self.run(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
        }
        self.write_metadata()
    }

    /// Clones `src` into this handle's repo path and populates the working tree.
    ///
    /// Attributes are written before checkout so a tracked `.gitattributes` cannot transform
    /// content. A local `src` with an `origin` remote passes that URL on as the clone's `origin`.
    pub fn clone_from(&self, src: &str) -> Result<()> {
        let src_origin = if Path::new(src).is_dir() {
            let out = self.output(
                self.command_in(Path::new(src)),
                &["config", "--get", "remote.origin.url"],
            )?;
            out.status.success().then(|| text(&out))
        } else {
            None
        };
        self.run_with(
            self.base(),
            &[
                OsStr::new("clone"),
                OsStr::new("-q"),
                OsStr::new("--no-checkout"),
                OsStr::new(src),
                self.repo.as_os_str(),
            ],
        )?;
        self.write_metadata()?;
        if let Some(url) = src_origin {
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
        let info = git_dir.join("info");
        fs::create_dir_all(&info)
            .with_context(|| format!("failed to create {}", info.display()))?;
        fs::write(info.join("attributes"), ATTRIBUTES)
            .context("failed to write .git/info/attributes")?;
        let id_path = git_dir.join("dotman-id");
        if !id_path.exists() {
            fs::write(&id_path, self.new_repo_id()?).context("failed to write .git/dotman-id")?;
        }
        Ok(())
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
        self.stage_all_except(&[])
    }

    /// Like [`Git::stage_all`], leaving out `held`, repo-relative paths taken literally.
    pub fn stage_all_except(&self, held: &[PathBuf]) -> Result<()> {
        let mut args: Vec<OsString> = ["add", "-A", "-f", "--", ".", TEMP_FILES]
            .map(OsString::from)
            .to_vec();
        args.extend(held.iter().map(|path| pathspec(":(exclude,literal)", path)));
        self.run(&args)?;
        // Untracks the leftovers that earlier versions committed.
        let args = [
            "rm",
            "-r",
            "-q",
            "--cached",
            "--ignore-unmatch",
            "--",
            TRACKED_TEMP_FILES,
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

    /// Parsed `git status --porcelain=v2`, including every untracked file. Never rewrites the
    /// index, so it is safe in a dry run.
    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        let mut cmd = self.command();
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        let args = [
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ];
        parse_status(&self.run_with(cmd, &args)?.stdout)
    }

    /// The commit `HEAD` points at; `None` on an unborn branch.
    pub fn head(&self) -> Result<Option<String>> {
        self.optional(&["rev-parse", "--verify", "-q", "HEAD"])
    }

    /// The branch `HEAD` points at, also when it is unborn; `None` when `HEAD` is detached.
    pub fn branch(&self) -> Result<Option<String>> {
        self.optional(&["symbolic-ref", "--short", "-q", "HEAD"])
    }

    /// Raw bytes of `path` as stored in `rev`, with no filters or text conversion.
    pub fn show_blob(&self, rev: &str, path: impl AsRef<Path>) -> Result<Vec<u8>> {
        let mut spec = OsString::from(rev);
        spec.push(":");
        spec.push(path.as_ref());
        let out = self.run(&[OsStr::new("cat-file"), OsStr::new("blob"), &spec])?;
        Ok(out.stdout)
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

    /// `git pull --rebase --autostash`. On conflict the rebase is aborted and the paths returned.
    pub fn pull_rebase(&self, remote: &str, branch: &str) -> Result<PullOutcome> {
        let stash = ["rev-parse", "-q", "--verify", "refs/stash"];
        let stashed_before = self.optional(&stash)?;
        let args = ["pull", "--rebase", "--autostash", remote, branch];
        let out = self.probe(&args)?;
        if out.status.success() {
            // git exits 0 when putting the autostash back conflicts, and keeps it in the stash.
            let paths = self.unmerged_paths()?;
            if !paths.is_empty() || self.optional(&stash)? != stashed_before {
                return Ok(PullOutcome::Unstashed { paths });
            }
            return Ok(PullOutcome::Ok);
        }
        if !self.operation_in_progress() {
            return Err(self.failure(&args, &out));
        }
        let paths = self
            .status()?
            .into_iter()
            .filter(|e| e.kind == StatusKind::Unmerged)
            .map(|e| e.path)
            .collect();
        self.run(&["rebase", "--abort"])?;
        Ok(PullOutcome::Conflict { paths })
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

    /// Lock files git left in `.git`: the index, `HEAD`, the config and any ref. A killed git
    /// leaves them behind, and every later git that writes then fails.
    #[must_use]
    pub fn lock_files(&self) -> Vec<PathBuf> {
        let git_dir = self.repo.join(".git");
        let mut found: Vec<PathBuf> =
            ["index.lock", "HEAD.lock", "packed-refs.lock", "config.lock"]
                .iter()
                .map(|name| git_dir.join(name))
                .filter(|path| fs::symlink_metadata(path).is_ok())
                .collect();
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

    /// The branch an unfinished rebase started from, such as `main`.
    #[must_use]
    pub fn rebase_branch(&self) -> Option<String> {
        ["rebase-merge", "rebase-apply"].iter().find_map(|dir| {
            let name =
                fs::read_to_string(self.repo.join(".git").join(dir).join("head-name")).ok()?;
            Some(name.trim().strip_prefix("refs/heads/")?.to_owned())
        })
    }

    /// Whether [`Git::operation`] finds an unfinished operation.
    #[must_use]
    pub fn operation_in_progress(&self) -> bool {
        self.operation().is_some()
    }

    /// Fetches `branch` from `remote` into `FETCH_HEAD`.
    pub fn fetch(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["fetch", "-q", remote, branch]).map(drop)
    }

    /// Like [`Git::fetch`], and fetches the history a shallow clone left out.
    pub fn unshallow(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["fetch", "-q", "--unshallow", remote, branch])
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
        self.run(&["push", remote, branch]).map(drop)
    }

    /// Like [`Git::push`], and records `remote` as the upstream of `branch`, so that a bare
    /// `git pull --rebase` works.
    pub fn push_set_upstream(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["push", "-u", remote, branch]).map(drop)
    }

    /// Whether `remote` has `branch`. Asks the remote and writes nothing locally.
    pub fn remote_has_branch(&self, remote: &str, branch: &str) -> Result<bool> {
        let pattern = format!("refs/heads/{branch}");
        let args = ["ls-remote", "--exit-code", remote, pattern.as_str()];
        let out = self.probe(&args)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(2) => Ok(false),
            _ => Err(self.failure(&args, &out)),
        }
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
        let out = self.probe_bounded(self.base(), &args)?;
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

    /// Points remote `name` at `url`, adding the remote if needed.
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<()> {
        let verb = if self.remote_url(name)?.is_some() {
            "set-url"
        } else {
            "add"
        };
        self.run(&["remote", verb, name, url]).map(drop)
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
    /// that does not exist yet has only the global config, so git then runs from `/`.
    pub fn author_email(&self) -> Result<Option<String>> {
        let args = ["-c", "user.useConfigOnly=true", "var", "GIT_AUTHOR_IDENT"];
        let dir = if self.repo.is_dir() {
            self.repo.as_path()
        } else {
            Path::new("/")
        };
        let out = self.probe_bounded(self.command_in(dir), &args)?;
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
        let mut cmd = self.base();
        cmd.args(["-c", "credential.helper=", "ls-remote", url, "HEAD"]);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GIT_ASKPASS", "").env("SSH_ASKPASS", "");
        let status = crate::proc::status_within(&mut cmd, timeout).context("failed to run git")?;
        Ok(status.is_some_and(|status| status.success()))
    }

    /// Sets `key` in the repo's config, or in the user's global config.
    pub fn set_config(&self, key: &str, value: &str, global: bool) -> Result<()> {
        let scope = if global { "--global" } else { "--local" };
        let cmd = if global { self.base() } else { self.command() };
        self.run_with(cmd, &["config", scope, "--", key, value])
            .map(drop)
    }
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
            Some(b'?') => (StatusKind::Untracked, 2),
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
        let xy = if kind == StatusKind::Untracked {
            "??".to_owned()
        } else {
            String::from_utf8_lossy(xy).into_owned()
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

fn parse_log(raw: &[u8]) -> Result<Vec<LogEntry>> {
    let fields: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|f| !f.is_empty()).collect();
    fields
        .chunks(3)
        .map(|chunk| match chunk {
            [oid, time, subject] => Ok(LogEntry {
                oid: String::from_utf8_lossy(oid).into_owned(),
                time: String::from_utf8_lossy(time)
                    .parse()
                    .context("git log printed a bad author time")?,
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
            Git::new(self.dir.path().join(name), "test").with_env(self.env())
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
        let msg = err.to_string();
        assert!(msg.contains("git push nope main failed"), "{msg}");
        assert!(msg.contains("'nope' does not appear"), "{msg}");
        assert_eq!(
            redact_credentials("https://user:s3cret@host/a@b"),
            "https://***@host/a@b"
        );
        assert_eq!(redact_credentials("git@host:x/y.git"), "git@host:x/y.git");
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
            ATTRIBUTES
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
        assert_eq!(
            outcome,
            PullOutcome::Conflict {
                paths: vec![PathBuf::from("shared.txt")]
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
}
