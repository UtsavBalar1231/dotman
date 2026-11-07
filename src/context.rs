//! What every command needs: resolved paths, the machine config and a git handle.

use crate::config::Config;
use crate::git::{Git, Operation};
use crate::identity;
use crate::paths::{Env, Paths, derive_key, validate_key};
use crate::repo_config::RepoConfig;
use crate::state::{IdentitySnapshot, Kind, State};
use crate::sys;
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
            "a {name} is in progress in {}; finish it with `dot git {name} --continue` or undo it with `dot git {name} --abort`",
            self.repo.display()
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
        }
    }

    /// Detected once per run, because on macOS it runs `scutil`.
    pub fn identity(&self) -> &IdentitySnapshot {
        self.identity
            .get_or_init(|| identity::current(&self.machine))
    }

    /// [`Context::managed`] grouped by target key, so variants of one file share a key.
    pub fn sources(&self) -> Result<Sources> {
        Ok(Sources::group(self.managed()?.keys().map(String::as_str)))
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

    /// The repo's `.git/dotman-id`, trimmed. Fails when `dot init` has not run.
    pub fn repo_id(&self) -> Result<String> {
        match fs::read_to_string(self.paths.repo.join(".git/dotman-id")) {
            Ok(id) => Ok(id.trim().to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
                "no dotman repo at {}; run `dot init` first",
                self.paths.repo.display()
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
