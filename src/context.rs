//! What every command needs: resolved paths, the machine config and a git handle.

use crate::config::Config;
use crate::git::Git;
use crate::paths::{Env, Paths, validate_key};
use crate::repo_config::RepoConfig;
use crate::state::{Kind, State};
use crate::sys;
use anyhow::{Context as _, Result, bail};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use walkdir::WalkDir;

/// Leftover temp files of the write engine; never managed.
pub const TEMP_PREFIX: &str = ".dotman-tmp-";

/// Resolved locations, with the machine config applied, and the git handle for one run.
pub struct Context {
    /// Where everything lives.
    pub paths: Paths,
    /// Git in the repo.
    pub git: Git,
}

impl Context {
    /// Resolves paths from the process environment. Creates nothing.
    pub fn resolve(dot_command: &'static str) -> Result<Self> {
        let paths = Paths::resolve(&Env::from_process(), sys::geteuid())?;
        let machine = Config::load(&paths.config_file)?;
        let paths = paths.apply_config(&machine)?;
        let git = Git::new(paths.repo.clone(), dot_command);
        Ok(Self { paths, git })
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
    /// names, with its kind. A symlink to a directory is one entry, never followed.
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
