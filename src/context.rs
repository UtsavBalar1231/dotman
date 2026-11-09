//! What every command needs: resolved paths, the machine config and a git handle.

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
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Leftover temp files of the write engine; never managed.
pub const TEMP_PREFIX: &str = ".dotman-tmp-";

/// A rebase or merge is unfinished in the repo. `main` maps it to exit code 4.
#[derive(Debug)]
pub struct InProgress {
    operation: Operation,
    repo: PathBuf,
}

impl fmt::Display for InProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.operation.name();
        write!(
            f,
            "a {name} is in progress in {}; finish it with `{bin} git {name} --continue` or undo it with `{bin} git {name} --abort`",
            self.repo.display(),
            bin = crate::name::bin()
        )
    }
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
    identity: OnceCell<IdentitySnapshot>,
    template_vars: OnceCell<Vars>,
    age: OnceCell<Age>,
}

impl Context {
    /// Resolves paths from the process environment. Creates nothing.
    pub fn resolve(dot_command: &'static str) -> Result<Self> {
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
            identity: OnceCell::new(),
            template_vars: OnceCell::new(),
            age: OnceCell::new(),
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
    pub fn sources(&self) -> Result<Sources> {
        Ok(Sources::group(self.managed()?.keys().map(String::as_str)))
    }

    /// System sources: regular files and symlinks under `.dotman/system`, keyed by their path
    /// relative to it. [`Context::managed`] leaves them out, because `.dotman` is a reserved name.
    pub fn managed_system(&self) -> Result<BTreeMap<String, Kind>> {
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
    pub fn system_sources(&self) -> Result<Sources> {
        Ok(Sources::group(
            self.managed_system()?.keys().map(String::as_str),
        ))
    }

    /// `DOTMAN_SYSTEM_ROOT` and `[system] roots` of this run.
    #[must_use]
    pub fn system_scope(&self) -> Scope {
        Scope::new(&self.paths, &self.machine)
    }

    /// Fails with [`InProgress`] while a rebase or merge is unfinished in the repo. Every command
    /// except `doctor` and `git` calls it, after taking its lock.
    pub fn guard(&self) -> Result<()> {
        self.git.operation().map_or(Ok(()), |operation| {
            Err(InProgress {
                operation,
                repo: self.paths.repo.clone(),
            }
            .into())
        })
    }

    /// Path of `.dotman.toml` in the repo.
    #[must_use]
    pub fn repo_config_file(&self) -> std::path::PathBuf {
        self.paths.repo.join(".dotman.toml")
    }

    /// The repo's `.git/dotman-id`, trimmed. Fails when `dotman init` has not run.
    pub fn repo_id(&self) -> Result<String> {
        match fs::read_to_string(self.paths.repo.join(".git/dotman-id")) {
            Ok(id) => Ok(id.trim().to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
                "no dotman repo at {}; run `{} init` first",
                self.paths.repo.display(),
                crate::name::bin()
            ),
            Err(e) => Err(e).context("cannot read .git/dotman-id"),
        }
    }

    /// Loads `.dotman.toml`.
    pub fn repo_config(&self) -> Result<RepoConfig> {
        RepoConfig::load(&self.repo_config_file())
    }

    /// Loads `state.toml` for the repo identified by `repo_id`.
    pub fn state(&self, repo_id: &str) -> Result<State> {
        State::load(&self.paths.state_file(), repo_id)
    }

    /// Every regular file and symlink in the worktree outside `.git` and the reserved root
    /// names, with its kind. A symlink to a directory is one entry, never followed. Keyed by repo
    /// path, which for a variant is not the key it targets.
    pub fn managed(&self) -> Result<BTreeMap<String, Kind>> {
        let root = &self.paths.repo;
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
                if entry.depth() == 1 && entry.file_name() == OsStr::new(".git") {
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
            {
                continue;
            }
            found.insert(key.to_owned(), kind);
        }
        Ok(found)
    }

    /// Pairs of keys in the git index, home and system alike, that differ only in case or Unicode
    /// normalization. A variant counts as the key it targets.
    pub fn index_collisions(&self) -> Result<Vec<(String, String)>> {
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
    pub fn refuse_collisions(&self) -> Result<()> {
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
    pub fn select(&self, inputs: &[PathBuf], keys: BTreeSet<String>) -> Result<BTreeSet<String>> {
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
