//! What every command needs: resolved paths, the machine config and a git handle.

use crate::commands::add::Ignore;
use crate::config::Config;
use crate::git::{Git, Operation};
use crate::identity;
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
use std::path::{Path, PathBuf};
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
                    "a {name} is in progress in {repo}; finish it with `{bin} git {name} --continue` or undo it with `{bin} git {name} --abort`"
                )?;
            }
            Unfinished::Conflicts(paths) => {
                let quoted = quote_all(paths);
                write!(
                    f,
                    "the repo at {repo} has unresolved conflicts in {quoted}, as `git stash pop` leaves them; resolve them and run `{bin} git add {quoted}`, or put back the committed version with `{bin} git checkout HEAD -- {quoted}`"
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
        Self {
            git: Git::new(paths.repo.clone(), dot_command),
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
        for entry in WalkDir::new(&root).follow_links(false).min_depth(1) {
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

    /// Fails when the repo is incomplete, with [`InProgress`] while a git operation or a conflict
    /// is unfinished in it, and when HEAD tracks a `.dotman.toml` that is gone or empty. Every
    /// command except `doctor` and `git` calls it, after taking its lock; `log` skips the last
    /// check, so the history stays readable.
    pub(crate) fn guard(&self) -> Result<()> {
        self.refuse_incomplete()?;
        if !is_real_dir(&self.paths.repo.join(".git")) {
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
        if self.command == "log" {
            return Ok(());
        }
        self.refuse_lost_config()
    }

    /// Fails on a lock file git left in `.git`. While dotman holds its own lock, none of them can
    /// be dotman's, so they are either another git running in the repo or a killed one's.
    pub(crate) fn refuse_git_locks(&self) -> Result<()> {
        let locks = self.git.lock_files();
        if locks.is_empty() {
            return Ok(());
        }
        let quoted = quote_all(&locks);
        bail!(
            "git's lock files are in the repo: {quoted}. Another git is running in {}, or one was killed and left them behind; if no git is running, remove them with `rm {quoted}` and run this again",
            self.paths.repo.display()
        )
    }

    /// A `.git` directory with no `HEAD`, or with a `config.lock` and no `dotman-id`, is what a
    /// killed `git init` or clone leaves. `dotman-id` is written last, so a repo that has it
    /// finished its init, whatever else is broken.
    pub(crate) fn refuse_incomplete(&self) -> Result<()> {
        let git_dir = self.paths.repo.join(".git");
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

    /// An empty or missing `.dotman.toml` would read as the defaults, losing `[dirs]`, `[modes]`
    /// and `[age]`, and the next sync would commit that.
    fn refuse_lost_config(&self) -> Result<()> {
        let file = self.repo_config_file();
        let how = match fs::symlink_metadata(&file) {
            Ok(meta) if meta.len() > 0 => return Ok(()),
            Ok(_) => "empty",
            Err(e) if e.kind() == io::ErrorKind::NotFound => "missing",
            Err(e) => return Err(e).with_context(|| format!("cannot stat {}", file.display())),
        };
        let tracked =
            self.git.head()?.is_some() && self.git.tree_mode("HEAD", ".dotman.toml")?.is_some();
        if tracked {
            bail!(
                "{} is {how}, but the last commit has it, so its [dirs], [modes] and [age] would be lost; restore it with `{} git checkout -- .dotman.toml`",
                file.display(),
                crate::name::bin()
            );
        }
        Ok(())
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
            Err(e) => Err(e).context("cannot read .git/dotman-id"),
        }
    }

    /// Loads `.dotman.toml`.
    pub(crate) fn repo_config(&self) -> Result<RepoConfig> {
        RepoConfig::load(&self.repo_config_file())
    }

    /// Loads `state.toml` for the repo identified by `repo_id`.
    pub(crate) fn state(&self, repo_id: &str) -> Result<State> {
        State::load(&self.paths.state_file(), repo_id)
    }

    /// Every regular file and symlink in the worktree outside `.git`, the reserved root names and
    /// the `[ignore]` patterns, with its kind. A symlink to a directory is one entry, never
    /// followed. Keyed by repo path, which for a variant is not the key it targets.
    pub(crate) fn managed(&self) -> Result<BTreeMap<String, Kind>> {
        let root = &self.paths.repo;
        // An editor's swap file or a merge's `.orig` in the worktree is not a key.
        let ignore = Ignore::compile(&self.repo_config()?.ignore.patterns)?;
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
                    .is_some_and(|dir| ignore.matches(dir, true));
                if ignored || (entry.depth() == 1 && entry.file_name() == OsStr::new(".git")) {
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
                || ignore.matches(key, false)
            {
                continue;
            }
            found.insert(key.to_owned(), kind);
        }
        Ok(found)
    }

    /// Removes the temp files and directories that killed runs left in the worktree and in the
    /// `$HOME` directories of managed keys. Only names whose embedded PID is no live process go,
    /// so a live run that writes without the lock keeps its own. The caller holds the lock.
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
            } else if is_stale_temp(entry.file_name()) {
                if entry.file_type().is_dir() {
                    walk.skip_current_dir();
                }
                stale.push(entry.into_path());
            }
        }
        let home = &self.paths.home;
        let mut dirs = BTreeSet::new();
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
                    .filter(|entry| is_stale_temp(&entry.file_name()))
                    .map(|entry| entry.path()),
            );
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

/// A `.dotman-tmp-<pid>-…` or `.dotman-tmp-link-<pid>-…` name whose process is gone. Names
/// without a PID, from older versions, are left alone.
fn is_stale_temp(name: &OsStr) -> bool {
    let Some(rest) = name
        .to_str()
        .and_then(|name| name.strip_prefix(TEMP_PREFIX))
    else {
        return false;
    };
    let rest = rest.strip_prefix("link-").unwrap_or(rest);
    let Some((pid, _)) = rest.split_once('-') else {
        return false;
    };
    pid.parse::<u32>()
        .is_ok_and(|pid| pid != std::process::id() && !sys::pid_alive(pid))
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

    fn dead_pid() -> u32 {
        let mut child = crate::proc::command("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn only_temp_names_of_dead_processes_are_stale() {
        let dead = dead_pid();
        let me = std::process::id();
        let stale = |name: String| is_stale_temp(OsStr::new(&name));
        assert!(stale(format!("{TEMP_PREFIX}{dead}-a1B2c3")));
        assert!(stale(format!("{TEMP_PREFIX}link-{dead}-0")));
        assert!(!stale(format!("{TEMP_PREFIX}{me}-a1B2c3")));
        assert!(!stale(format!("{TEMP_PREFIX}1-a1B2c3")), "pid 1 is alive");
        assert!(
            !stale(format!("{TEMP_PREFIX}a1B2c3")),
            "no PID: an older version's"
        );
        assert!(!stale(format!("{TEMP_PREFIX}{dead}")));
        assert!(!stale(format!("x{TEMP_PREFIX}{dead}-a")));
        assert!(sys::pid_alive(me));
        assert!(!sys::pid_alive(dead));
    }
}
