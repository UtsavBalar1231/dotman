//! Hermetic harness: every `dot` and `git` invocation runs inside a temp root
//! with its own HOME and XDG dirs, and the process environment is never mutated.
#![allow(dead_code)]
// reason: test-only helpers; clippy.toml exempts expect only inside #[test] fns, not shared helpers.
#![allow(clippy::expect_used)]

use assert_cmd::Command;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink as unix_symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use xxhash_rust::xxh3::xxh3_128;

/// Kind of a filesystem entry recorded by [`tree_checksum`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// One entry of a [`tree_checksum`] map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// Permission bits, `mode & 0o7777`.
    pub mode: u32,
    /// xxh3-128 of the file bytes as 32 hex chars (files only).
    pub hash: Option<String>,
    /// Link target (symlinks only).
    pub target: Option<PathBuf>,
}

/// An isolated device: HOME, XDG dirs and a bare remote under one temp root.
pub struct TestEnv {
    root: Arc<TempDir>,
    /// Owner of `remote.git`; differs from `root` on a second device.
    remote_owner: Arc<TempDir>,
    path_prepend: Vec<PathBuf>,
}

impl TestEnv {
    /// Create a fresh root with HOME, XDG dirs, `.gitconfig` and a bare `remote.git`.
    pub fn new() -> Self {
        let root = Arc::new(TempDir::new().expect("create temp root"));
        let env = Self {
            remote_owner: Arc::clone(&root),
            root,
            path_prepend: Vec::new(),
        };
        env.init_dirs();
        let remote = env.remote_path();
        fs::create_dir_all(&remote).expect("create remote dir");
        let out = env
            .git_command()
            .args(["init", "--bare", "-q"])
            .arg(&remote)
            .output()
            .expect("run git init");
        assert!(out.status.success(), "git init --bare failed");
        env
    }

    /// A new device with its own HOME and XDG dirs that shares this `remote.git`.
    pub fn second_device(&self) -> Self {
        let env = Self {
            root: Arc::new(TempDir::new().expect("create temp root")),
            remote_owner: Arc::clone(&self.remote_owner),
            path_prepend: self.path_prepend.clone(),
        };
        env.init_dirs();
        env
    }

    fn init_dirs(&self) {
        for dir in [
            self.home(),
            self.xdg_config(),
            self.xdg_state(),
            self.xdg_data(),
        ] {
            fs::create_dir_all(dir).expect("create dir");
        }
        fs::write(
            self.home().join(".gitconfig"),
            "[user]\n\tname = Test User\n\temail = test@example.invalid\n",
        )
        .expect("write .gitconfig");
    }

    /// Temp root directory.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Device HOME.
    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    /// `XDG_CONFIG_HOME`.
    pub fn xdg_config(&self) -> PathBuf {
        self.root().join("xdg/config")
    }

    /// `XDG_STATE_HOME`.
    pub fn xdg_state(&self) -> PathBuf {
        self.root().join("xdg/state")
    }

    /// `XDG_DATA_HOME`.
    pub fn xdg_data(&self) -> PathBuf {
        self.root().join("xdg/data")
    }

    /// Path of the shared bare remote.
    pub fn remote_path(&self) -> PathBuf {
        self.remote_owner.path().join("remote.git")
    }

    /// Put `dir` in front of PATH for later `dot()` and `git()` calls (stub binaries).
    pub fn prepend_path(&mut self, dir: impl Into<PathBuf>) {
        self.path_prepend.insert(0, dir.into());
    }

    fn vars(&self) -> Vec<(&'static str, OsString)> {
        let mut dirs = self.path_prepend.clone();
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(dirs).expect("join PATH");
        let mut vars: Vec<(&'static str, OsString)> = vec![
            ("HOME", self.home().into()),
            ("XDG_CONFIG_HOME", self.xdg_config().into()),
            ("XDG_STATE_HOME", self.xdg_state().into()),
            ("XDG_DATA_HOME", self.xdg_data().into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("DOTMAN_NON_INTERACTIVE", "1".into()),
            ("NO_PAGER", "1".into()),
            ("LC_ALL", "C".into()),
            ("PATH", path),
        ];
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            vars.push(("TMPDIR", tmp));
        }
        vars
    }

    /// The `dot` binary with this device's environment, running in HOME.
    pub fn dot(&self) -> Command {
        let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("dot");
        cmd.env_clear();
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        cmd.current_dir(self.home());
        cmd
    }

    fn git_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.env_clear();
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        cmd.current_dir(self.home());
        cmd
    }

    /// Run `git` with this device's environment in HOME; returns stdout, panics on failure.
    pub fn git(&self, args: &[&str]) -> String {
        let out = self.git_command().args(args).output().expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Write `bytes` to `path` (relative to HOME, or absolute) with `mode`, creating parents.
    pub fn write(&self, path: impl AsRef<Path>, bytes: &[u8], mode: u32) {
        let full = self.home().join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parents");
        }
        fs::write(&full, bytes).expect("write file");
        fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// Create a symlink at `path` (relative to HOME, or absolute) pointing at `target`.
    pub fn symlink(&self, path: impl AsRef<Path>, target: impl AsRef<Path>) {
        let full = self.home().join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parents");
        }
        unix_symlink(target, full).expect("create symlink");
    }

    /// Output of `git ls-remote` against the shared remote.
    pub fn remote_refs(&self) -> String {
        self.git(&["ls-remote", &self.remote_path().to_string_lossy()])
    }
}

impl Default for TestEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Map of relative path to kind, mode, xxh3-128 and link target. Symlinks are not followed.
pub fn tree_checksum(dir: &Path) -> BTreeMap<String, Entry> {
    let mut map = BTreeMap::new();
    walk(dir, dir, &mut map).expect("walk tree");
    map
}

fn walk(base: &Path, dir: &Path, map: &mut BTreeMap<String, Entry>) -> io::Result<()> {
    for item in fs::read_dir(dir)? {
        let path = item?.path();
        let meta = fs::symlink_metadata(&path)?;
        let rel = path
            .strip_prefix(base)
            .map_err(io::Error::other)?
            .to_string_lossy()
            .into_owned();
        let mode = meta.permissions().mode() & 0o7777;
        let ft = meta.file_type();
        let entry = if ft.is_symlink() {
            Entry {
                kind: Kind::Symlink,
                mode,
                hash: None,
                target: Some(fs::read_link(&path)?),
            }
        } else if ft.is_dir() {
            walk(base, &path, map)?;
            Entry {
                kind: Kind::Dir,
                mode,
                hash: None,
                target: None,
            }
        } else {
            let hash = format!("{:032x}", xxh3_128(&fs::read(&path)?));
            Entry {
                kind: Kind::File,
                mode,
                hash: Some(hash),
                target: None,
            }
        };
        map.insert(rel, entry);
    }
    Ok(())
}
