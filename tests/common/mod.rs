//! Hermetic harness: environment is set per command; the process environment is never mutated.
#![allow(dead_code)]
// reason: test-only helpers; clippy.toml exempts expect only inside #[test] fns, not shared helpers.
#![allow(clippy::expect_used)]

use assert_cmd::Command;
use dotman::repo_config::RepoConfig;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink as unix_symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use tempfile::TempDir;
use xxhash_rust::xxh3::xxh3_128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// Permission bits, `mode & 0o7777`.
    pub mode: u32,
    /// xxh3-128 of the file bytes as 32 hex chars (files only).
    pub hash: Option<String>,
    pub target: Option<PathBuf>,
}

pub struct TestEnv {
    root: Arc<TempDir>,
    /// Owner of `remote.git`; differs from `root` on a second device.
    remote_owner: Arc<TempDir>,
    path_prepend: Vec<PathBuf>,
}

impl TestEnv {
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

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    pub fn xdg_config(&self) -> PathBuf {
        self.root().join("xdg/config")
    }

    pub fn xdg_state(&self) -> PathBuf {
        self.root().join("xdg/state")
    }

    pub fn xdg_data(&self) -> PathBuf {
        self.root().join("xdg/data")
    }

    pub fn remote_path(&self) -> PathBuf {
        self.remote_owner.path().join("remote.git")
    }

    /// Put `dir` in front of PATH (for stub binaries).
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

    /// Runs git and returns its output, whatever the exit status.
    pub fn git_raw(&self, args: &[&str]) -> Output {
        self.git_command().args(args).output().expect("run git")
    }

    pub fn git(&self, args: &[&str]) -> String {
        let out = self.git_raw(args);
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

    /// Contents of `path` (relative to HOME, or absolute).
    pub fn read(&self, path: impl AsRef<Path>) -> Vec<u8> {
        fs::read(self.home().join(path)).expect("read file")
    }

    /// Writes into the repo worktree directly, as a pull from upstream would.
    pub fn write_repo(&self, key: &str, bytes: &[u8]) {
        let path = self.repo().join(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create repo dirs");
        }
        fs::write(path, bytes).expect("write repo file");
    }

    /// Contents and mode of every backup of `key`, across all backup runs.
    pub fn backups(&self, key: &str) -> Vec<(Vec<u8>, u32)> {
        let Ok(runs) = fs::read_dir(self.state_dir().join("backups")) else {
            return Vec::new();
        };
        runs.map(|run| run.expect("read backups").path().join(key))
            .filter(|path| path.exists())
            .map(|path| (fs::read(&path).expect("read backup"), mode_of(&path)))
            .collect()
    }

    /// Commits the repo worktree with plain git and pushes it to the remote's `main`, which
    /// becomes the remote's HEAD so a clone checks it out.
    pub fn push_repo(&self) {
        let repo = self.repo().to_string_lossy().into_owned();
        let remote = self.remote_path().to_string_lossy().into_owned();
        self.git(&["-C", &repo, "add", "-A", "-f"]);
        self.git(&["-C", &repo, "commit", "-q", "-m", "test"]);
        self.git(&["-C", &repo, "push", "-q", &remote, "HEAD:refs/heads/main"]);
        self.git(&[
            "--git-dir",
            &remote,
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ]);
    }

    pub fn remote_url(&self) -> String {
        format!("file://{}", self.remote_path().display())
    }

    /// The dotman repo under the default `XDG_DATA_HOME`.
    pub fn repo(&self) -> PathBuf {
        self.xdg_data().join("dotman")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.xdg_state().join("dotman")
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.dot().args(args).output().expect("run dot")
    }

    /// Runs `dot`, asserts exit 0 and returns stderr.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "dot {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// Runs `dot`, asserts exit 1 and returns stderr.
    pub fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(out.status.code(), Some(1), "dot {args:?} should exit 1");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// Parses the `<token>\t<key>\0` records of `dot <command> --porcelain`.
    pub fn porcelain(&self, command: &str) -> Vec<(String, String)> {
        let out = self.run(&[command, "--porcelain"]);
        assert!(out.status.success());
        out.stdout
            .split(|b| *b == 0)
            .filter(|record| !record.is_empty())
            .map(|record| {
                let text = String::from_utf8_lossy(record).into_owned();
                let (token, key) = text.split_once('\t').expect("tab in record");
                (token.to_owned(), key.to_owned())
            })
            .collect()
    }

    pub fn token_of(&self, command: &str, key: &str) -> Option<String> {
        self.porcelain(command)
            .into_iter()
            .find(|(_, k)| k == key)
            .map(|(token, _)| token)
    }

    pub fn repo_config(&self) -> RepoConfig {
        RepoConfig::load(&self.repo().join(".dotman.toml")).expect("load .dotman.toml")
    }

    /// Checksums of home, repo, state dir and remote; a missing directory counts as empty.
    pub fn four_trees(&self) -> [BTreeMap<String, Entry>; 4] {
        [
            self.home(),
            self.repo(),
            self.state_dir(),
            self.remote_path(),
        ]
        .map(|dir| {
            if dir.exists() {
                tree_checksum(&dir)
            } else {
                BTreeMap::new()
            }
        })
    }

    pub fn remote_refs(&self) -> String {
        self.git(&["ls-remote", &self.remote_path().to_string_lossy()])
    }
}

impl Default for TestEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// `uname -n`, cut at the first `.`, as `sync` puts it in its commit message.
pub fn short_host() -> String {
    let out = std::process::Command::new("uname")
        .arg("-n")
        .output()
        .expect("run uname");
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    name.split('.').next().unwrap_or_default().to_owned()
}

/// Every managed key holds the same bytes, link target and mode on both devices.
pub fn assert_converged(a: &TestEnv, b: &TestEnv) {
    let keys = a.porcelain("list");
    assert_eq!(keys, b.porcelain("list"));
    assert_ne!(keys.len(), 0);
    for (kind, key) in keys {
        let (on_a, on_b) = (a.home().join(&key), b.home().join(&key));
        if kind == "symlink" {
            assert_eq!(
                fs::read_link(&on_a).expect("link on a"),
                fs::read_link(&on_b).expect("link on b"),
                "{key}"
            );
        } else {
            assert_eq!(
                fs::read(&on_a).expect("file on a"),
                fs::read(&on_b).expect("file on b"),
                "{key}"
            );
            assert_eq!(mode_of(&on_a), mode_of(&on_b), "{key}");
        }
    }
}

/// Permission bits of `path` itself; a symlink is not followed.
pub fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

/// Symlinks are not followed.
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
