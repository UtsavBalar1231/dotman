//! What every command needs: resolved paths, the machine config and a git handle.

use crate::commands::add::Ignore;
use crate::config::Config;
use crate::git::{Git, Operation, StatusKind};
use crate::identity;
use crate::lock::Lock;
use crate::paths::{Env, Paths, derive_key, find_collisions, is_reserved_root_name, validate_key};
use crate::repo_config::RepoConfig;
use crate::secrets::Age;
use crate::state::{IdentitySnapshot, Kind, State};
use crate::sys;
use crate::system::{self, Scope};
use crate::template::Vars;
use crate::variant::Sources;
use anyhow::{Context as _, Result, bail};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

/// Leftover temp files of the write engine; never managed.
pub const TEMP_PREFIX: &str = ".dotman-tmp-";

/// A git operation or a conflict is unfinished in the repo. `main` maps it to exit code 4.
#[derive(Debug)]
pub struct InProgress {
    unfinished: Unfinished,
    repo: PathBuf,
    /// Lock files a killed git left, which make the recovery commands fail until removed.
    locks: Vec<PathBuf>,
    /// The branch a rebase started from, which its recovery checks out again.
    branch: Option<String>,
}

#[derive(Debug)]
enum Unfinished {
    Operation(Operation),
    /// Left by `git stash pop` and the like, with no operation to continue or abort.
    Conflicts(Vec<PathBuf>),
}

impl fmt::Display for InProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bin = crate::name::bin();
        let repo = self.repo.display();
        match &self.unfinished {
            Unfinished::Operation(Operation::Bisect) => write!(
                f,
                "a bisect is in progress in {repo}, so the worktree holds an older commit; end it with `{bin} git bisect reset`"
            )?,
            Unfinished::Operation(operation) => {
                let name = operation.name();
                write!(
                    f,
                    "a {name} is in progress in {repo}; finish it with `{bin} git -c core.editor=true {name} --continue` or undo it with `{bin} git {name} --abort`"
                )?;
            }
            Unfinished::Conflicts(paths) => {
                let quoted = quote_all(paths);
                write!(
                    f,
                    "the repo at {repo} has unresolved conflicts in {quoted}, as `git stash pop` leaves them; resolve them and run `{bin} git add -- {quoted}`, or put back the committed version with `{bin} git checkout HEAD -- {quoted}`"
                )?;
            }
        }
        if self.locks.is_empty() {
            return Ok(());
        }
        let locks = quote_all(&self.locks);
        write!(
            f,
            "\ngit left lock files behind, so those commands fail until they are removed. If no git is running in the repo, recover with:\n  1. rm {locks}"
        )?;
        if matches!(self.unfinished, Unfinished::Operation(Operation::Rebase)) {
            let branch = self.branch.as_deref().unwrap_or("main");
            write!(
                f,
                "\n  2. {bin} git rebase --abort\n  3. if that fails: {bin} git rebase --quit && {bin} git checkout -f {branch} && {bin} git reset --hard ORIG_HEAD\n  4. {bin} sync"
            )?;
        }
        Ok(())
    }
}

fn quote_all(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| shell_words::quote(&path.to_string_lossy()).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

impl std::error::Error for InProgress {}

/// Resolved locations, with the machine config applied, and the git handle for one run.
pub struct Context {
    /// Where everything lives.
    pub paths: Paths,
    /// Git in the repo.
    pub git: Git,
    /// The machine config, already applied to `paths`.
    pub machine: Config,
    command: &'static str,
    /// Keys already warned about in this run.
    warned: RefCell<BTreeSet<String>>,
    identity: OnceCell<IdentitySnapshot>,
    template_vars: OnceCell<Vars>,
    age: OnceCell<Age>,
}

impl Context {
    /// Resolves paths from the process environment. Creates nothing.
    pub(crate) fn resolve(dot_command: &'static str) -> Result<Self> {
        let paths = Paths::resolve(&Env::from_process(), sys::geteuid())?;
        let machine = Config::load(&paths.config_file)?;
        let paths = paths.apply_config(&machine)?;
        Ok(Self::new(paths, machine, dot_command))
    }

    /// A context whose git runs in `paths.repo`; `machine` must be the config `paths` came from.
    #[must_use]
    pub fn new(paths: Paths, machine: Config, dot_command: &'static str) -> Self {
        let state_file = paths.state_file();
        let leave_out =
            move |git: &Git, untracked: &[PathBuf]| left_out(git, &state_file, untracked);
        Self {
            git: Git::new(paths.repo.clone(), dot_command).leaving_out(Box::new(leave_out)),
            paths,
            machine,
            command: dot_command,
            warned: RefCell::new(BTreeSet::new()),
            identity: OnceCell::new(),
            template_vars: OnceCell::new(),
            age: OnceCell::new(),
        }
    }

    /// Prints `message` as a warning, unless one was already printed for `key` in this run, such
    /// as by `sync`'s pass before the pull.
    pub(crate) fn warn_once(&self, key: &str, message: &str) {
        if self.warned.borrow_mut().insert(key.to_owned()) {
            crate::output::warning(message);
        }
    }

    /// Detected once per run, because on macOS it runs `scutil`.
    pub fn identity(&self) -> &IdentitySnapshot {
        self.identity
            .get_or_init(|| identity::current(&self.machine))
    }

    /// What templates render with, from the identity as overridden.
    pub fn template_vars(&self) -> &Vars {
        self.template_vars.get_or_init(|| {
            Vars::new(self.identity(), &self.machine, &self.paths.home, |name| {
                std::env::var(name).ok()
            })
        })
    }

    /// The age CLI and this machine's keys, resolved on first use.
    pub fn age(&self) -> &Age {
        self.age
            .get_or_init(|| Age::from_env(&self.machine, &self.paths))
    }

    /// [`Context::managed`] grouped by target key, so variants of one file share a key.
    pub(crate) fn sources(&self) -> Result<Sources> {
        Ok(Sources::group(self.managed()?.keys().map(String::as_str)))
    }

    /// System sources: regular files and symlinks under `.dotman/system`, keyed by their path
    /// relative to it. [`Context::managed`] leaves them out, because `.dotman` is a reserved name.
    pub(crate) fn managed_system(&self) -> Result<BTreeMap<String, Kind>> {
        let root = self.paths.repo.join(system::DIR);
        let real = is_real_dir(&self.paths.repo.join(".dotman")) && is_real_dir(&root);
        let mut found = BTreeMap::new();
        if !real {
            return Ok(found);
        }
        let temp_dir = |entry: &walkdir::DirEntry| {
            entry.file_type().is_dir()
                && entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
        };
        let walk = WalkDir::new(&root).follow_links(false).min_depth(1);
        for entry in walk.into_iter().filter_entry(|entry| !temp_dir(entry)) {
            let entry = entry.with_context(|| format!("cannot read {}", root.display()))?;
            let file_type = entry.file_type();
            let kind = if file_type.is_symlink() {
                Kind::Symlink
            } else if file_type.is_file() {
                Kind::File
            } else {
                continue;
            };
            let Some(inside) = entry.path().strip_prefix(&root).ok().and_then(Path::to_str) else {
                continue;
            };
            if !entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
                && validate_key(inside).is_ok()
            {
                found.insert(inside.to_owned(), kind);
            }
        }
        Ok(found)
    }

    /// [`Context::managed_system`] grouped by the key each source targets, without the leading `/`.
    pub(crate) fn system_sources(&self) -> Result<Sources> {
        Ok(Sources::group(
            self.managed_system()?.keys().map(String::as_str),
        ))
    }

    /// `DOTMAN_SYSTEM_ROOT` and `[system] roots` of this run.
    #[must_use]
    pub fn system_scope(&self) -> Scope {
        Scope::new(&self.paths, &self.machine)
    }

    /// Takes the lock for a command that writes, then runs [`Context::guard`]. Outside a dry run it
    /// also brings `.git/info/attributes` up to date in a repo `init` finished, so a repo an older
    /// dotman made gets git's built-in text merge before anything is merged or checked out. When it
    /// cannot write the file, it warns and the command goes on.
    pub(crate) fn lock(&self, dry_run: bool) -> Result<Lock> {
        let lock = Lock::acquire(&self.paths.lock_file(), dry_run)?;
        self.guard()?;
        let git_dir = self.paths.repo.join(".git");
        if dry_run || !is_real_dir(&git_dir) || !git_dir.join("dotman-id").is_file() {
            return Ok(lock);
        }
        let attributes = git_dir.join("info/attributes");
        match self.git.refresh_attributes() {
            Ok(true) => crate::output::info(&format!(
                "rewrote {} to end with the line dotman pins",
                attributes.display()
            )),
            Ok(false) => {}
            Err(e) => self.warn_once(
                "attributes",
                &format!(
                    "{e:#}; until {} ends with the line dotman pins, a tracked .gitattributes can choose the merge driver. Fix that, then run `{} init`, which writes the line and changes nothing in $HOME",
                    attributes.display(),
                    crate::name::bin()
                ),
            ),
        }
        Ok(lock)
    }

    /// Fails when the repo is incomplete, with [`InProgress`] while a git operation or a conflict
    /// is unfinished in it, and when HEAD's `.dotman.toml` has settings and the worktree's none. Every
    /// command except `doctor` and `git` calls it, the writing ones through [`Context::lock`]; `log`,
    /// which only reads the history, skips all but the first check.
    pub(crate) fn guard(&self) -> Result<()> {
        self.refuse_incomplete()?;
        if self.command == "log" || !is_real_dir(&self.paths.repo.join(".git")) {
            return Ok(());
        }
        let unfinished = match self.git.operation() {
            Some(operation) => Some(Unfinished::Operation(operation)),
            // Only a repo with commits can hold an index entry with stages.
            None if self.git.head()?.is_some() => {
                let paths = self.git.unmerged_paths()?;
                (!paths.is_empty()).then_some(Unfinished::Conflicts(paths))
            }
            None => None,
        };
        if let Some(unfinished) = unfinished {
            return Err(InProgress {
                unfinished,
                repo: self.paths.repo.clone(),
                locks: self.git.lock_files(),
                branch: self.git.rebase_branch(),
            }
            .into());
        }
        self.refuse_lost_config()
    }

    /// Fails on a lock file git left in `.git`. While dotman holds its own lock, none of them can
    /// be dotman's, so they are either another git running in the repo or a killed one's.
    pub(crate) fn refuse_git_locks(&self) -> Result<()> {
        self.git.refuse_locks()
    }

    /// A `.git` directory with no `HEAD`, or with a `config.lock` and no `dotman-id`, is what a
    /// killed `git init` or clone leaves. `dotman-id` is written last, so a repo that has it
    /// finished its init, whatever else is broken. A `.git` file, as a linked worktree or a
    /// submodule has, is refused too: the metadata dotman keeps in `.git` would not be there.
    pub(crate) fn refuse_incomplete(&self) -> Result<()> {
        let git_dir = self.paths.repo.join(".git");
        if fs::symlink_metadata(&git_dir).is_ok_and(|meta| meta.is_file()) {
            bail!(
                "{} is a linked git worktree or a submodule (its .git is a file), and dotman needs a plain git repo there; point `repo` in {} at the main checkout, or move it aside and run `{} init <url>`",
                self.paths.repo.display(),
                self.paths.config_file.display(),
                crate::name::bin()
            );
        }
        let incomplete = is_real_dir(&git_dir)
            && !git_dir.join("dotman-id").exists()
            && (!git_dir.join("HEAD").is_file() || git_dir.join("config.lock").exists());
        if incomplete {
            bail!(
                "the repo at {} is incomplete (an earlier init was interrupted); remove it and rerun `{} init`",
                self.paths.repo.display(),
                crate::name::bin()
            );
        }
        Ok(())
    }

    /// A `.dotman.toml` that is missing or holds no settings would read as the defaults, losing
    /// `[dirs]`, `[modes]` and `[age]`, and the next sync would commit that. When the last commit's
    /// copy holds no settings either, there is nothing to lose, and restoring it would bring back
    /// no settings.
    fn refuse_lost_config(&self) -> Result<()> {
        let file = self.repo_config_file();
        let Some(how) = self.config_without_settings()? else {
            return Ok(());
        };
        match self.committed_config_has_settings()? {
            Some(true) => bail!(
                "{} {how}, but the last commit has it, so its [dirs], [modes] and [age] would be lost; restore it with `{} git checkout -- .dotman.toml`",
                file.display(),
                crate::name::bin()
            ),
            // A committed file of comments alone is deliberate, so it is not noted on every run.
            Some(false) if how != HOLDS_NO_SETTINGS => crate::output::info(&format!(
                "{} {how}, as in the last commit, so the defaults apply",
                file.display()
            )),
            _ => {}
        }
        Ok(())
    }

    /// How `.dotman.toml` in the worktree reads as the defaults: "is missing", "is empty", or
    /// [`HOLDS_NO_SETTINGS`] when it has only blank lines and comments. `None` when it holds
    /// settings, or is not a readable regular file or not TOML, which loading it reports.
    pub(crate) fn config_without_settings(&self) -> Result<Option<&'static str>> {
        let file = self.repo_config_file();
        let how = match fs::symlink_metadata(&file) {
            Ok(meta) if !meta.is_file() => None,
            Ok(meta) if meta.len() == 0 => Some("is empty"),
            Ok(_) => fs::read(&file)
                .ok()
                .filter(|bytes| holds_no_settings(bytes))
                .map(|_| HOLDS_NO_SETTINGS),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some("is missing"),
            Err(e) => return Err(e).with_context(|| format!("cannot stat {}", file.display())),
        };
        Ok(how)
    }

    /// Whether the last commit's `.dotman.toml` holds settings; `None` when the last commit has
    /// none. A copy that is not TOML counts as holding them.
    pub(crate) fn committed_config_has_settings(&self) -> Result<Option<bool>> {
        if self.git.head()?.is_none() || self.git.tree_mode("HEAD", ".dotman.toml")?.is_none() {
            return Ok(None);
        }
        let committed = self.git.show_blob("HEAD", ".dotman.toml")?;
        Ok(Some(!holds_no_settings(&committed)))
    }

    /// Path of `.dotman.toml` in the repo.
    #[must_use]
    pub fn repo_config_file(&self) -> std::path::PathBuf {
        self.paths.repo.join(".dotman.toml")
    }

    /// The repo's `.git/dotman-id`, trimmed. Fails when `dotman init` has not run.
    pub(crate) fn repo_id(&self) -> Result<String> {
        let repo = &self.paths.repo;
        match fs::read_to_string(repo.join(".git/dotman-id")) {
            Ok(id) => Ok(id.trim().to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.refuse_incomplete()?;
                if repo.join(".git/HEAD").is_file() {
                    bail!(
                        "the git repo at {} has no .git/dotman-id, which `{bin} init` writes; run `{bin} init` to adopt it",
                        repo.display(),
                        bin = crate::name::bin()
                    );
                }
                bail!(
                    "no dotman repo at {}; run `{} init` first",
                    repo.display(),
                    crate::name::bin()
                )
            }
            Err(e) => {
                self.refuse_incomplete()?;
                Err(e).context("cannot read .git/dotman-id")
            }
        }
    }

    pub(crate) fn repo_config(&self) -> Result<RepoConfig> {
        RepoConfig::load(&self.repo_config_file())
    }

    pub(crate) fn state(&self, repo_id: &str) -> Result<State> {
        State::load(&self.paths.state_file(), repo_id)
    }

    /// Every regular file and symlink in the worktree outside `.git`, the reserved root names and
    /// the `[ignore]` patterns, with its kind. A symlink to a directory is one entry, never
    /// followed. Keyed by repo path, which for a variant is not the key it targets.
    pub(crate) fn managed(&self) -> Result<BTreeMap<String, Kind>> {
        let root = &self.paths.repo;
        // An editor's swap file or a merge's `.orig` in the worktree is not a key.
        let dropped = Dropped::load(&self.git, &self.paths.state_file())?;
        let mut found = BTreeMap::new();
        let mut walk = WalkDir::new(root)
            .follow_links(false)
            .min_depth(1)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walk.next() {
            let entry = entry.with_context(|| format!("cannot read {}", root.display()))?;
            let file_type = entry.file_type();
            if file_type.is_dir() {
                let ignored = entry
                    .path()
                    .strip_prefix(root)
                    .ok()
                    .and_then(Path::to_str)
                    .is_some_and(|dir| dropped.drops(dir, true));
                if ignored
                    || entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
                    || (entry.depth() == 1 && entry.file_name() == OsStr::new(".git"))
                {
                    walk.skip_current_dir();
                }
                continue;
            }
            let kind = if file_type.is_symlink() {
                Kind::Symlink
            } else if file_type.is_file() {
                Kind::File
            } else {
                continue;
            };
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let Some(key) = rel.to_str() else { continue };
            if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
                || validate_key(key).is_err()
                || dropped.drops(key, false)
            {
                continue;
            }
            found.insert(key.to_owned(), kind);
        }
        Ok(found)
    }

    /// Removes the temp files and directories that killed runs left in the worktree, in dotman's
    /// state and config directories and in the `$HOME` directories of managed keys; see
    /// [`is_leftover`] for what counts. The caller holds the lock.
    pub(crate) fn sweep_temp_files(&self) -> Result<()> {
        let repo = &self.paths.repo;
        if !is_real_dir(&repo.join(".git")) {
            return Ok(());
        }
        let mut stale = Vec::new();
        let mut walk = WalkDir::new(repo)
            .follow_links(false)
            .min_depth(1)
            .into_iter();
        while let Some(entry) = walk.next() {
            let Ok(entry) = entry else { continue };
            if entry.depth() == 1 && entry.file_name() == OsStr::new(".git") {
                walk.skip_current_dir();
            } else if is_leftover(entry.path(), TEMP_PREFIX) {
                if entry.file_type().is_dir() {
                    walk.skip_current_dir();
                }
                stale.push(entry.into_path());
            }
        }
        let home = &self.paths.home;
        // dotman's own directories hold the state and the age identity, both written atomically.
        let mut dirs: BTreeSet<PathBuf> = std::iter::once(self.paths.state_dir.clone())
            .chain(self.paths.config_file.parent().map(Path::to_path_buf))
            .collect();
        for key in self.sources()?.keys() {
            // Only real directories, so the sweep never reaches through a symlinked parent.
            let parents: Vec<&str> = crate::engine::parents(key).collect();
            if parents.iter().all(|parent| is_real_dir(&home.join(parent))) {
                dirs.insert(
                    parents
                        .last()
                        .map_or_else(|| home.clone(), |p| home.join(p)),
                );
            }
        }
        for dir in dirs {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            stale.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| is_leftover(path, TEMP_PREFIX)),
            );
        }
        // An import stages its clone beside the repo, or in the temp directory for a dry run.
        let tmp = std::env::temp_dir();
        for dir in repo.parent().into_iter().chain([tmp.as_path()]) {
            stale.extend(import_leftovers(dir));
        }
        for path in &stale {
            let removed = if is_real_dir(path) {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            if let Err(e) = removed {
                crate::output::warning(&format!("cannot remove {}: {e}", path.display()));
            }
        }
        if !stale.is_empty() {
            crate::output::info(&format!(
                "removed {} temp {} that an interrupted run left behind",
                stale.len(),
                if stale.len() == 1 { "file" } else { "files" }
            ));
        }
        Ok(())
    }

    /// Pairs of keys in the git index, home and system alike, that differ only in case or Unicode
    /// normalization. A variant counts as the key it targets.
    pub(crate) fn index_collisions(&self) -> Result<Vec<(String, String)>> {
        // Unlike `managed`, this keeps names that are not NFC: they are what twins look like.
        let reserved = |p: &str| is_reserved_root_name(p.split('/').next().unwrap_or(p));
        let paths = self.git.index_paths()?;
        let system_prefix = format!("{}/", system::DIR);
        let (system_paths, home_paths): (Vec<&str>, Vec<&str>) = paths
            .iter()
            .map(String::as_str)
            .filter(|p| !p.rsplit('/').next().unwrap_or(p).starts_with(TEMP_PREFIX))
            .partition(|p| p.starts_with(&system_prefix));
        let home = Sources::group(home_paths.into_iter().filter(|p| !reserved(p)));
        let system = Sources::group(
            system_paths
                .into_iter()
                .filter_map(|p| p.strip_prefix(&system_prefix))
                .filter(|rel| !reserved(rel)),
        );
        let keys: Vec<String> = home
            .keys()
            .map(str::to_owned)
            .chain(system.keys().map(system::key_of))
            .collect();
        Ok(find_collisions(keys.iter().map(String::as_str)))
    }

    /// Fails when the index holds two keys that differ only in case or Unicode normalization,
    /// because a case-insensitive filesystem cannot hold both.
    pub(crate) fn refuse_collisions(&self) -> Result<()> {
        let pairs = self.index_collisions()?;
        if pairs.is_empty() {
            return Ok(());
        }
        let bin = crate::name::bin();
        let lines: Vec<String> = pairs
            .iter()
            .map(|(a, b)| {
                format!(
                    "{a} and {b} differ only in case or Unicode normalization; a case-insensitive filesystem cannot hold both. Rename one with `{bin} git mv` on a case-sensitive machine."
                )
            })
            .collect();
        bail!("{}", lines.join("\n"))
    }

    /// The `keys` under the user-supplied `inputs`, or all of them when there are no inputs. An
    /// input that matches no key is an error, so a mistyped path is never a silent no-op.
    pub(crate) fn select(
        &self,
        inputs: &[PathBuf],
        keys: BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        if inputs.is_empty() {
            return Ok(keys);
        }
        let cwd = std::env::current_dir().context("cannot read the current directory")?;
        let mut selected = BTreeSet::new();
        for input in inputs {
            let home_real = self.paths.home_real.as_deref();
            let filter = derive_key(input, &cwd, &self.paths.home, home_real)?;
            let under: Vec<&String> = keys.iter().filter(|k| is_under(k, &filter)).collect();
            if under.is_empty() {
                bail!("{filter} is not managed");
            }
            selected.extend(under.into_iter().cloned());
        }
        Ok(selected)
    }
}

/// What `[ignore]` drops from the repo worktree: what its patterns match, minus what git's own
/// ignore rules would keep, a tracked file. Here that is a path in the index, or one this machine
/// applied or added, which `add` puts in the worktree before any commit.
struct Dropped {
    ignore: Ignore,
    kept: BTreeSet<String>,
}

impl Dropped {
    /// A state that cannot be read keeps nothing; the command that loads it reports it.
    fn load(git: &Git, state_file: &Path) -> Result<Self> {
        let repo = git.repo();
        let ignore = Ignore::compile(
            &RepoConfig::load(&repo.join(".dotman.toml"))?
                .ignore
                .patterns,
        )?;
        let mut kept = BTreeSet::new();
        if is_real_dir(&repo.join(".git")) {
            kept.extend(git.index_paths()?);
            let state = fs::read_to_string(repo.join(".git/dotman-id"))
                .ok()
                .and_then(|id| State::load(state_file, id.trim()).ok());
            if let Some(state) = state {
                kept.extend(state.files.into_values().map(|entry| entry.source));
            }
        }
        Ok(Self { ignore, kept })
    }

    /// A directory that holds a kept path stays, but as in git, what lies under a directory a
    /// pattern matches is still dropped unless it is kept or holds a kept path.
    fn drops(&self, path: &str, is_dir: bool) -> bool {
        let under_matched_dir = || {
            path.match_indices('/')
                .filter_map(|(at, _)| path.get(..at))
                .any(|dir| self.ignore.matches(dir, true))
        };
        // The set lookups first: they are cheap, and most directories hold a kept path.
        let kept = if is_dir {
            holds_any(&self.kept, path)
        } else {
            self.kept.contains(path)
        };
        !kept && (self.ignore.matches(path, is_dir) || under_matched_dir())
    }
}

/// The untracked repo paths, as `git status` lists them, that are never staged: what `[ignore]`
/// drops and dotman's temp names, at any depth, and the names that are not UTF-8. A directory
/// stands for every path under it. The reserved root names are not keys, so `[ignore]` never
/// applies to them.
fn left_out(git: &Git, state_file: &Path, untracked: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let (mut roots, unnamed) = sort_untracked(git, state_file, untracked)?;
    roots.extend(unnamed);
    Ok(roots)
}

/// The untracked repo paths that [`left_out`] leaves out only because their names are not UTF-8:
/// no key has such a name, so committing one would spread bytes that no machine applies and no
/// status shows. Status lists them instead.
pub(crate) fn unnamed_untracked(ctx: &Context) -> Result<Vec<PathBuf>> {
    // A handle with no leave-out filter, which would hide them.
    let untracked: Vec<PathBuf> = Git::new(ctx.paths.repo.clone(), ctx.command)
        .status()?
        .into_iter()
        .filter(|entry| entry.kind == StatusKind::Untracked)
        .map(|entry| entry.path)
        .collect();
    Ok(sort_untracked(&ctx.git, &ctx.paths.state_file(), &untracked)?.1)
}

/// What [`left_out`] leaves out of `untracked`: the roots that `[ignore]` and the temp names drop,
/// and apart, the paths whose names are not UTF-8 and that nothing drops. `[ignore]` matches such
/// a name in its lossy form. Under a reserved root name, which is never a key, such a name stays.
fn sort_untracked(
    git: &Git,
    state_file: &Path,
    untracked: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let dropped = Dropped::load(git, state_file)?;
    let temp = |path: &str| {
        path.rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
    };
    let mut roots = BTreeSet::new();
    let mut unnamed = Vec::new();
    for path in untracked {
        let lossy = path.to_string_lossy();
        let key = !is_reserved_root_name(lossy.split('/').next().unwrap_or(&lossy));
        let drops = |sub: &str, is_dir| temp(sub) || (key && dropped.drops(sub, is_dir));
        // The real parents, shortest first: a lossy name's byte offsets are not the path's.
        let mut parents: Vec<&Path> = path
            .ancestors()
            .skip(1)
            .filter(|dir| !dir.as_os_str().is_empty())
            .collect();
        parents.reverse();
        let dir = parents
            .into_iter()
            .find(|dir| drops(&dir.to_string_lossy(), true));
        match dir {
            Some(dir) => {
                roots.insert(dir.to_path_buf());
            }
            None if drops(&lossy, false) => {
                roots.insert(path.clone());
            }
            None if key && path.to_str().is_none() => unnamed.push(path.clone()),
            None => {}
        }
    }
    Ok((roots.into_iter().collect(), unnamed))
}

/// How long a leftover must be untouched before a sweep removes it. A live run in another PID
/// namespace, as in a container that shares `$HOME`, can carry a PID that is dead here.
const STALE_AFTER: Duration = Duration::from_secs(600);

/// Whether `path` is what a killed dotman run left: named `<prefix><pid>-` and the 6 letters or
/// digits tempfile adds, or `<prefix>link-<pid>-<attempt>`; a regular file, a symlink, or a 0700
/// directory of this user, as dotman makes them; untouched for [`STALE_AFTER`]; and its PID no
/// live process. A user's file of a similar name, and names from older versions, stay.
pub(crate) fn is_leftover(path: &Path, prefix: &str) -> bool {
    path.file_name()
        .and_then(|name| leftover_pid(name, prefix))
        .is_some_and(|pid| is_stale(path, Some(pid)))
}

/// What an import names its staging directory before the PID and tempfile's 6 letters or digits.
pub(crate) const IMPORT_STAGING: &str = ".dotman-import-";

/// [`is_leftover`] for an import's staging directory, which versions before the PID was added
/// named with tempfile's 6 letters or digits alone. Such a one is stale once untouched for
/// [`STALE_AFTER`]: every import holds the lock that the sweep holds too.
pub(crate) fn is_leftover_import(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    if let Some(pid) = leftover_pid(name, IMPORT_STAGING) {
        return is_real_dir(path) && is_stale(path, Some(pid));
    }
    let unnamed = name
        .to_str()
        .and_then(|name| name.strip_prefix(IMPORT_STAGING))
        .is_some_and(|rest| rest.len() == 6 && rest.bytes().all(|b| b.is_ascii_alphanumeric()));
    unnamed && is_real_dir(path) && is_stale(path, None)
}

/// Removes what [`is_leftover_import`] finds in `dir`. The caller holds the lock.
pub(crate) fn sweep_import_staging(dir: &Path) {
    for path in import_leftovers(dir) {
        if let Err(e) = fs::remove_dir_all(&path) {
            crate::output::warning(&format!("cannot remove {}: {e}", path.display()));
        }
    }
}

fn import_leftovers(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_leftover_import(path))
        .collect()
}

/// A regular file, a symlink, or a 0700 directory of this user, untouched for [`STALE_AFTER`],
/// whose `pid`, when there is one, is no live process.
fn is_stale(path: &Path, pid: Option<u32>) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    let ours = meta.is_file()
        || meta.file_type().is_symlink()
        || (meta.is_dir() && meta.mode() & 0o777 == 0o700 && meta.uid() == sys::geteuid());
    let untouched = meta
        .modified()
        .ok()
        .and_then(|at| at.elapsed().ok())
        .is_some_and(|age| age >= STALE_AFTER);
    let dead = pid.is_none_or(|pid| pid != std::process::id() && !sys::pid_alive(pid));
    ours && untouched && dead
}

fn leftover_pid(name: &OsStr, prefix: &str) -> Option<u32> {
    let rest = name.to_str()?.strip_prefix(prefix)?;
    let (rest, link) = rest
        .strip_prefix("link-")
        .map_or((rest, false), |rest| (rest, true));
    let (pid, suffix) = rest.split_once('-')?;
    let suffix_fits = if link {
        (1..=2).contains(&suffix.len()) && suffix.bytes().all(|b| b.is_ascii_digit())
    } else {
        suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
    };
    let pid_fits = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    if suffix_fits && pid_fits {
        pid.parse().ok()
    } else {
        None
    }
}

const HOLDS_NO_SETTINGS: &str = "holds no settings";

/// Whether `.dotman.toml` text is blank lines and comments only, which reads as the defaults.
fn holds_no_settings(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(text).ok())
        .is_some_and(|table| table.is_empty())
}

/// Whether any of `paths` lies under `dir`.
fn holds_any(paths: &BTreeSet<String>, dir: &str) -> bool {
    let prefix = format!("{dir}/");
    paths
        .range(prefix.clone()..)
        .next()
        .is_some_and(|path| path.starts_with(&prefix))
}

/// Whether `key` is `dir` or lies under it.
#[must_use]
pub fn is_under(key: &str, dir: &str) -> bool {
    key == dir
        || key
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `path` is an existing directory, without following a symlink.
#[must_use]
pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    fn dead_pid() -> u32 {
        let mut child = crate::proc::command("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn only_dotman_temp_names_carry_a_pid() {
        let pid = |name: &str| leftover_pid(OsStr::new(name), TEMP_PREFIX);
        assert_eq!(pid(&format!("{TEMP_PREFIX}42-a1B2c3")), Some(42));
        assert_eq!(pid(&format!("{TEMP_PREFIX}link-42-0")), Some(42));
        assert_eq!(pid(&format!("{TEMP_PREFIX}link-42-15")), Some(42));
        for other in [
            "a1B2c3",
            "42",
            "42-notes.txt",
            "42-mydir",
            "42-a1B2c3d",
            "42-a1B2-3",
            "link-42-abc",
            "x-a1B2c3",
        ] {
            assert_eq!(pid(&format!("{TEMP_PREFIX}{other}")), None, "{other}");
        }
        assert_eq!(pid("x.dotman-tmp-42-a1B2c3"), None);
    }

    #[test]
    fn only_old_leftovers_of_dead_processes_are_stale() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dead = dead_pid();
        let me = std::process::id();
        let old = std::time::SystemTime::now()
            .checked_sub(STALE_AFTER)
            .and_then(|at| at.checked_sub(Duration::from_secs(60)))
            .unwrap();
        let make = |name: String, dir: Option<u32>| {
            let path = tmp.path().join(name);
            match dir {
                Some(mode) => fs::DirBuilder::new()
                    .mode(mode)
                    .create(&path)
                    .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(mode)))
                    .unwrap(),
                None => fs::write(&path, b"x").unwrap(),
            }
            fs::File::open(&path).unwrap().set_modified(old).unwrap();
            path
        };
        assert!(is_leftover(
            &make(format!("{TEMP_PREFIX}{dead}-a1B2c3"), None),
            TEMP_PREFIX
        ));
        assert!(is_leftover(
            &make(format!("{TEMP_PREFIX}{dead}-d1B2c3"), Some(0o700)),
            TEMP_PREFIX
        ));
        assert!(
            !is_leftover(
                &make(format!("{TEMP_PREFIX}{dead}-u1B2c3"), Some(0o755)),
                TEMP_PREFIX
            ),
            "a user's directory"
        );
        assert!(!is_leftover(
            &make(format!("{TEMP_PREFIX}{me}-a1B2c3"), None),
            TEMP_PREFIX
        ));
        assert!(
            !is_leftover(&make(format!("{TEMP_PREFIX}1-a1B2c3"), None), TEMP_PREFIX),
            "pid 1 is alive"
        );
        let young = tmp.path().join(format!("{TEMP_PREFIX}{dead}-y1B2c3"));
        fs::write(&young, b"x").unwrap();
        assert!(
            !is_leftover(&young, TEMP_PREFIX),
            "touched within {STALE_AFTER:?}"
        );
        assert!(sys::pid_alive(me));
        assert!(!sys::pid_alive(dead));
    }
}
