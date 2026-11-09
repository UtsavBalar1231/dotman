//! Hermetic harness: environment is set per command; the process environment is never mutated.
#![allow(dead_code)]
// reason: test-only helpers; clippy.toml exempts expect only inside #[test] fns, not shared helpers.
#![allow(clippy::expect_used)]

use assert_cmd::Command;
use dotman::VERSION;
use dotman::repo_config::RepoConfig;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::ops::Deref;
use std::os::unix::fs::{PermissionsExt, symlink as unix_symlink};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};
use std::sync::{Arc, OnceLock};
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

/// `GIT_CONFIG_PARAMETERS` that rewrites every http(s) URL to a missing local path.
pub const NO_NETWORK: &str = "'url.file:///dotman-tests-no-network/.insteadOf=https://' 'url.file:///dotman-tests-no-network/.insteadOf=http://'";

pub struct TestEnv {
    root: Arc<TempDir>,
    /// Owner of `remote.git`; differs from `root` on a second device.
    remote_owner: Arc<TempDir>,
    path_prepend: Vec<PathBuf>,
    system_root_on: bool,
}

impl TestEnv {
    pub fn new() -> Self {
        let root = Arc::new(TempDir::new().expect("create temp root"));
        let env = Self {
            remote_owner: Arc::clone(&root),
            root,
            path_prepend: Vec::new(),
            system_root_on: false,
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
            system_root_on: false,
        };
        env.init_dirs();
        env
    }

    fn init_dirs(&self) {
        fs::create_dir_all(self.gh_block_dir()).expect("create the gh stub dir");
        write_stub(
            &self.gh_block_dir().join("gh"),
            "#!/bin/sh\necho 'real gh blocked in tests' >&2\nexit 97\n",
        );
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

    /// Holds a `gh` that refuses to run, so the real one is never reached. It comes after
    /// [`TestEnv::prepend_path`] directories, so a test's own `gh` stub wins.
    fn gh_block_dir(&self) -> PathBuf {
        self.root().join("gh-blocked")
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

    /// The directory `DOTMAN_SYSTEM_ROOT` names once [`TestEnv::use_system_root`] ran.
    pub fn system_root(&self) -> PathBuf {
        self.root().join("sysroot")
    }

    /// Points `DOTMAN_SYSTEM_ROOT` at an empty directory, and puts stub `pacman`, `dpkg`, `rpm` and
    /// `apk` in front of PATH that own nothing, so the host's package database is never asked.
    pub fn use_system_root(&mut self) {
        fs::create_dir_all(self.system_root()).expect("create the system root");
        self.system_root_on = true;
        let stubs = self.root().join("stubs");
        fs::create_dir_all(&stubs).expect("create the stub dir");
        self.prepend_path(&stubs);
        self.stub_package_manager(&[]);
        for name in ["dpkg", "rpm", "apk"] {
            write_stub(&stubs.join(name), "#!/bin/sh\nexit 1\n");
        }
    }

    /// Replaces the stub `pacman`: it owns every path that ends with one of `owned`.
    pub fn stub_package_manager(&self, owned: &[&str]) {
        let mut arms = String::new();
        for suffix in owned {
            arms.push_str("  *");
            arms.push_str(suffix);
            arms.push_str(") echo \"$2 is owned by keyd 2.4.3-1\"; exit 0;;\n");
        }
        let body = format!(
            "#!/bin/sh\ncase \"$2\" in\n{arms}esac\necho \"error: No package owns $2\" >&2\nexit 1\n"
        );
        write_stub(&self.root().join("stubs/pacman"), &body);
    }

    /// Where `key`, an absolute path such as `/etc/x`, is on disk under the system root.
    pub fn system_path(&self, key: &str) -> PathBuf {
        self.system_root().join(key.trim_start_matches('/'))
    }

    /// Writes a file at `key` under the system root with `mode`, creating parents.
    pub fn write_system(&self, key: &str, bytes: &[u8], mode: u32) {
        let path = self.system_path(key);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create parents");
        fs::write(&path, bytes).expect("write system file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// Put `dir` in front of PATH (for stub binaries).
    pub fn prepend_path(&mut self, dir: impl Into<PathBuf>) {
        self.path_prepend.insert(0, dir.into());
    }

    fn vars(&self) -> Vec<(&'static str, OsString)> {
        let mut dirs = self.path_prepend.clone();
        dirs.push(self.gh_block_dir());
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
            // Every http(s) URL points at a path that does not exist, so no test reaches the network.
            // GIT_CONFIG_PARAMETERS is what `git -c` sets; unlike GIT_CONFIG_COUNT (git 2.31) it
            // works on the 2.25 floor.
            ("GIT_CONFIG_PARAMETERS", NO_NETWORK.into()),
            // Fails closed if a rewrite is ever missed: only local transports may run.
            ("GIT_ALLOW_PROTOCOL", "file".into()),
            ("DOTMAN_NON_INTERACTIVE", "1".into()),
            ("NO_PAGER", "1".into()),
            ("LC_ALL", "C".into()),
            ("PATH", path),
        ];
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            vars.push(("TMPDIR", tmp));
        }
        if self.system_root_on {
            vars.push(("DOTMAN_SYSTEM_ROOT", self.system_root().into()));
        }
        vars
    }

    pub fn dot(&self) -> Command {
        let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("dotman");
        cmd.env_clear();
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        cmd.current_dir(self.home());
        cmd
    }

    /// `dotman` as a std command, for what `assert_cmd` does not expose, such as a process group.
    pub fn dot_process(&self) -> std::process::Command {
        self.process_for(Path::new(env!("CARGO_BIN_EXE_dotman")))
    }

    /// The program at `exe` with the harness environment. A link or copy of the binary under
    /// another name runs as that name.
    pub fn process_for(&self, exe: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(exe);
        cmd.env_clear();
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        cmd.current_dir(self.home());
        cmd
    }

    /// `program` with the harness environment, but with `PATH` set to exactly `path`, so a `dot`
    /// the developer has installed cannot be found.
    pub fn command_with_path(&self, program: &str, path: &[PathBuf]) -> std::process::Command {
        let mut cmd = std::process::Command::new(program);
        cmd.env_clear();
        for (k, v) in self.vars() {
            cmd.env(k, v);
        }
        cmd.env("PATH", std::env::join_paths(path).expect("join PATH"));
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

    /// Replaces `$HOME/.gitconfig` with one that names no identity.
    pub fn clear_git_identity(&self) {
        fs::write(self.home().join(".gitconfig"), "[core]\n\teditor = true\n")
            .expect("write .gitconfig");
    }

    /// Replaces the machine config, `$XDG_CONFIG_HOME/dotman/config.toml`.
    pub fn write_machine_config(&self, text: &str) {
        let dir = self.xdg_config().join("dotman");
        fs::create_dir_all(&dir).expect("create config dir");
        fs::write(dir.join("config.toml"), text).expect("write machine config");
    }

    /// Writes an executable file at `name` under the temp root, outside HOME and the repo.
    pub fn write_script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root().join(name);
        fs::write(&path, body).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
        path
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
        self.run_with(&[], args)
    }

    /// Like [`TestEnv::run`], with extra environment variables such as `DOTMAN_HOST`.
    pub fn run_with(&self, vars: &[(&str, &str)], args: &[&str]) -> Output {
        let mut cmd = self.dot();
        for (name, value) in vars {
            cmd.env(name, value);
        }
        cmd.args(args).output().expect("run dot")
    }

    /// Like [`TestEnv::run_with`], with `stdin` as standard input.
    pub fn run_stdin(&self, vars: &[(&str, &str)], stdin: &[u8], args: &[&str]) -> Output {
        let mut cmd = self.dot();
        for (name, value) in vars {
            cmd.env(name, value);
        }
        cmd.args(args)
            .write_stdin(stdin.to_vec())
            .output()
            .expect("run dot")
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

    /// Like [`TestEnv::ok`], with extra environment variables.
    pub fn ok_with(&self, vars: &[(&str, &str)], args: &[&str]) -> String {
        let out = self.run_with(vars, args);
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
        self.porcelain_with(&[], command)
    }

    pub fn porcelain_with(&self, vars: &[(&str, &str)], command: &str) -> Vec<(String, String)> {
        let out = self.run_with(vars, &[command, "--porcelain"]);
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

fn write_stub(path: &Path, body: &str) {
    fs::write(path, body).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
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

/// Regular files under `dir` whose bytes contain `needle`; symlinks are not followed.
pub fn files_containing(dir: &Path, needle: &[u8]) -> Vec<String> {
    tree_checksum(dir)
        .into_iter()
        .filter(|(_, entry)| entry.kind == Kind::File)
        .map(|(rel, _)| rel)
        .filter(|rel| {
            let bytes = fs::read(dir.join(rel)).expect("read file");
            bytes.windows(needle.len()).any(|window| window == needle)
        })
        .collect()
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

/// `(entries, converted, need action)` from the summary line.
pub fn accounting(stderr: &str) -> (usize, usize, usize) {
    assert!(
        stderr.contains(" entries: "),
        "no summary line in:\n{stderr}"
    );
    let line = stderr
        .lines()
        .find(|line| line.contains(" entries: "))
        .expect("a summary line");
    let numbers: Vec<usize> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().expect("a number"))
        .collect();
    let [entries, converted, needs]: [usize; 3] = numbers.try_into().expect("three numbers");
    assert_eq!(
        entries,
        converted.saturating_add(needs),
        "N = C + R in: {line}"
    );
    (entries, converted, needs)
}

/// Whether an age CLI is on PATH. Prints why a test skips, and fails it instead when
/// `DOTMAN_REQUIRE_AGE=1`.
pub fn age_installed() -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path)
        .any(|dir| ["age", "rage"].iter().any(|name| dir.join(name).is_file()));
    if !found {
        assert!(
            std::env::var_os("DOTMAN_REQUIRE_AGE").is_none_or(|v| v != "1"),
            "DOTMAN_REQUIRE_AGE=1, but neither age nor rage is on PATH"
        );
        eprintln!("skipped: neither age nor rage is on PATH (DOTMAN_REQUIRE_AGE=1 fails instead)");
    }
    found
}

/// Whether chezmoi can run here; says why not, and fails instead when
/// `DOTMAN_REQUIRE_CHEZMOI=1`.
pub fn have_chezmoi() -> bool {
    let found = StdCommand::new("chezmoi")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        assert!(
            std::env::var("DOTMAN_REQUIRE_CHEZMOI").as_deref() != Ok("1"),
            "chezmoi is not on PATH, and DOTMAN_REQUIRE_CHEZMOI=1 asks for it"
        );
        eprintln!("skipping: chezmoi is not on PATH, so this test cannot run");
    }
    found
}

/// Whether curl can run here, which `install.sh` needs to fetch `file://` URLs.
pub fn curl_available() -> bool {
    let found = StdCommand::new("curl")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("skipped: curl is not installed, and install.sh needs it to fetch file:// URLs");
    }
    found
}

/// `git -C dir args`, asserting success; returns stdout.
pub fn git_in(env: &TestEnv, dir: &Path, args: &[&str]) -> String {
    let dir = dir.to_string_lossy();
    let mut full = vec!["-C", &*dir];
    full.extend_from_slice(args);
    env.git(&full)
}

/// A source tree outside the repo. Dropping it fails the test if an import changed it since the
/// last [`Source::seal`].
pub struct Source {
    path: PathBuf,
    sum: RefCell<BTreeMap<String, Entry>>,
}

impl Source {
    pub fn new(path: PathBuf) -> Self {
        let sum = RefCell::new(tree_checksum(&path));
        Self { path, sum }
    }

    /// Accepts the edits the test made to the source itself.
    pub fn seal(&self) {
        *self.sum.borrow_mut() = tree_checksum(&self.path);
    }

    /// Fails if the source differs from its last checksum.
    pub fn check(&self) {
        assert_eq!(
            tree_checksum(&self.path),
            *self.sum.borrow(),
            "the source {} was modified",
            self.path.display()
        );
    }
}

impl Deref for Source {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.check();
        }
    }
}

/// File name, bytes, executable.
pub type File<'a> = (&'a str, &'a str, bool);

/// A work tree with two commits holding `files`, and `gitlink` as a submodule if given.
pub fn work_tree(env: &TestEnv, name: &str, files: &[File], gitlink: Option<&str>) -> PathBuf {
    let dir = env.root().join("work").join(name);
    fs::create_dir_all(&dir).expect("create work tree");
    git_in(env, &dir, &["init", "-q"]);
    git_in(env, &dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    for (path, text, executable) in files {
        let full = dir.join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("create parents");
        fs::write(&full, text).expect("write file");
        let mode = if *executable { 0o755 } else { 0o644 };
        fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
    }
    git_in(env, &dir, &["add", "-A"]);
    git_in(env, &dir, &["commit", "-q", "-m", "first"]);
    if let Some(path) = gitlink {
        let sha = git_in(env, &dir, &["rev-parse", "HEAD"]);
        let cacheinfo = format!("160000,{},{path}", sha.trim());
        git_in(
            env,
            &dir,
            &["update-index", "--add", "--cacheinfo", &cacheinfo],
        );
    }
    git_in(
        env,
        &dir,
        &["commit", "-q", "--allow-empty", "-m", "second"],
    );
    dir
}

/// Copies `work` to `dest` as a bare repo. `worktree` is what yadm and vcsh record.
pub fn bare(env: &TestEnv, work: &Path, dest: &Path, worktree: Option<&Path>) -> Source {
    fs::create_dir_all(dest.parent().expect("parent")).expect("create parent");
    let (work_arg, dest_arg) = (work.to_string_lossy(), dest.to_string_lossy());
    env.git(&["clone", "-q", "--bare", &*work_arg, &*dest_arg]);
    git_in(env, dest, &["config", "--unset", "remote.origin.url"]);
    if let Some(worktree) = worktree {
        git_in(env, dest, &["config", "core.bare", "false"]);
        git_in(
            env,
            dest,
            &["config", "core.worktree", &worktree.to_string_lossy()],
        );
    }
    Source::new(dest.to_path_buf())
}

pub fn yadm_repo(env: &TestEnv) -> PathBuf {
    env.xdg_data().join("yadm/repo.git")
}

pub const TOOLRC: &str = "[core]\n{% if yadm.os == \"Linux\" %}\neditor = vim\n{% else %}\neditor = nano\n{% endif %}\n# {{ env.EDITOR }}\n";

/// 13 entries: 8 convert, 5 need action.
pub const YADM_FILES: [File; 12] = [
    (".bashrc", "bash\n", false),
    ("bin/run", "#!/bin/sh\n", true),
    ("README.md", "readme\n", false),
    (".zshrc##o.Darwin", "mac\n", false),
    (".zshrc##o.Linux", "linux\n", false),
    ("x##h.box,c.work", "x\n", false),
    (".toolrc##template", TOOLRC, false),
    ("neg##~o.Darwin", "not mac\n", false),
    ("esh.conf##template.esh", "<%= $USER %>\n", false),
    ("inc.conf##template", "{% include \"other\" %}\n", false),
    ("dir##o.Linux/file", "dir\n", false),
    ("host.conf##h.my.host", "dotted\n", false),
];

/// The yadm repo in its default place, with a submodule, and the checksum of the source.
pub fn yadm_fixture(env: &TestEnv) -> Source {
    let work = work_tree(env, "yadm", &YADM_FILES, Some("sub"));
    bare(env, &work, &yadm_repo(env), Some(&env.home()))
}

/// The read-only legacy dotman repo, and the directory that holds it and its README.
pub const LEGACY_REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-v2/repo");
pub const LEGACY_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-v2");

/// The legacy fixture, which no import may change.
pub fn legacy_fixture() -> Source {
    Source::new(PathBuf::from(LEGACY_ROOT))
}

/// A git repo that holds five files of the legacy fixture, `run_mode` on `.local/bin/run`.
pub fn overlay_source(env: &TestEnv, run_mode: u32, bashrc: &[u8]) -> PathBuf {
    let src = env.root().join("overlay-src");
    let files: [(&str, &[u8], u32); 5] = [
        (".bashrc", bashrc, 0o644),
        (".local/bin/run", b"#!/bin/sh\necho run\n", run_mode),
        (".ssh/config", b"Host example\n  User legacyuser\n", 0o644),
        (".empty", b"", 0o644),
        (".config/abs.conf", b"abs=true\n", 0o644),
    ];
    for (key, bytes, mode) in files {
        let path = src.join(key);
        fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        fs::write(&path, bytes).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
    }
    git_in(env, &src, &["init", "-q"]);
    git_in(env, &src, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git_in(env, &src, &["add", "-A"]);
    git_in(env, &src, &["commit", "-q", "-m", "base"]);
    src
}

/// Writes `files` (path, text) under `$ROOT/<name>`, as a chezmoi source.
pub fn source_tree(env: &TestEnv, name: &str, files: &[(&str, &str)]) -> Source {
    let path = env.root().join(name);
    for (file, text) in files {
        let full = path.join(file);
        fs::create_dir_all(full.parent().expect("parent")).expect("create parents");
        fs::write(&full, text).expect("write file");
    }
    fs::create_dir_all(&path).expect("create source");
    Source::new(path)
}

/// A `chezmoi` that predates entry types: no `--exclude` and no `--no-pager` in its help, no
/// `dump`. `managed` prints `managed`; `cat` prints the text of the first entry of `files`
/// whose key ends the path. It logs what it was asked and returns the log's path.
pub fn stub_chezmoi(env: &mut TestEnv, managed: &[&str], files: &[(&str, &str)]) -> PathBuf {
    let dir = env.root().join("stub-bin");
    fs::create_dir_all(&dir).expect("create stub dir");
    let log = env.root().join("stub.log");
    let listing = managed.iter().fold(String::new(), |mut out, key| {
        let _ = write!(out, "{key}\\n");
        out
    });
    let mut arms = String::new();
    for (key, text) in files {
        let _ = writeln!(arms, "    */{key}) printf '{text}' ;;");
    }
    let script = format!(
        r#"#!/bin/sh
log='{log}'
while [ $# -gt 0 ]; do
  case "$1" in
    --config|--persistent-state|--cache|--source|--destination) shift 2 ;;
    --no-tty) shift ;;
    *) break ;;
  esac
done
echo "$*" >> "$log"
cmd="$1"; shift
case "$cmd" in
  help) echo "Usage: chezmoi dump [target]... [flags]"; echo "  -f, --format <none>|json|yaml   Output format" ;;
  managed) printf '{listing}' ;;
  cat) case "$1" in
{arms}    *) echo "stub: cannot cat $1" >&2; exit 1 ;;
  esac ;;
  *) echo "stub: unexpected $cmd" >&2; exit 1 ;;
esac
"#,
        log = log.display()
    );
    let path = dir.join("chezmoi");
    fs::write(&path, script).expect("write stub");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
    env.prepend_path(&dir);
    log
}

pub const GITCONFIG: &str = "[user]\n\tname = Test User\n\temail = test@example.invalid\n";

/// The Stow layout: packages, a folded directory link and several file links in `$HOME`.
pub fn stow(env: &TestEnv) {
    env.write("dotfiles/zsh/.zshrc", b"export A=1\n", 0o644);
    env.write("dotfiles/git/.gitconfig", GITCONFIG.as_bytes(), 0o644);
    env.write("dotfiles/tmux/.tmux.conf", b"set -g mouse on\n", 0o644);
    env.write(
        "dotfiles/scripts/.local/bin/hello",
        b"#!/bin/sh\necho hi\n",
        0o755,
    );
    env.write(
        "dotfiles/nvim/.config/nvim/init.lua",
        b"vim.o.number = true\n",
        0o644,
    );
    env.write(
        "dotfiles/nvim/.config/nvim/lua/opts.lua",
        b"return {}\n",
        0o600,
    );
    fs::remove_file(env.home().join(".gitconfig")).expect("remove the harness .gitconfig");
    env.symlink(".gitconfig", "dotfiles/git/.gitconfig");
    env.symlink(".zshrc", "dotfiles/zsh/.zshrc");
    env.symlink(".tmux.conf", "dotfiles/tmux/.tmux.conf");
    env.symlink(
        ".local/bin/hello",
        "../../dotfiles/scripts/.local/bin/hello",
    );
    env.symlink(".config/nvim", "../dotfiles/nvim/.config/nvim");
}

pub type R<T = ()> = Result<T, Box<dyn Error>>;

/// The `.bashrc` that the bootstrap repo holds.
pub const BOOTSTRAP_BASHRC: &[u8] = b"export EDITOR=vi\n";

const SYSTEM_DIRS: [&str; 2] = ["/usr/bin", "/bin"];

/// The tools `install.sh` and the `dotman` it starts need, besides `dot`.
pub const INSTALLER_TOOLS: &[&str] = &[
    "sh",
    "bash",
    "dirname",
    "uname",
    "mktemp",
    "curl",
    "wget",
    "awk",
    "tar",
    "gzip",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "chmod",
    "ln",
    "readlink",
    "id",
    "cat",
    "tr",
    "grep",
    "head",
    "cut",
    "sed",
    "sha256sum",
    "shasum",
    "git",
];

/// A directory under `dir` holding links to `tools` and nothing else. Hosts ship a `dot` of their
/// own (Graphviz), and a system directory cannot hide it, so a test of "no `dot` on `PATH`" runs
/// with only this directory.
pub fn toolbox(dir: &Path, tools: &[&str]) -> R<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    for tool in tools {
        let found = std::env::split_paths(&path)
            .chain(SYSTEM_DIRS.iter().map(PathBuf::from))
            .map(|d| d.join(tool))
            .find(|candidate| candidate.is_file());
        if let Some(found) = found
            && fs::symlink_metadata(dir.join(tool)).is_err()
        {
            unix_symlink(found, dir.join(tool))?;
        }
    }
    Ok(dir.to_path_buf())
}

/// stdout and stderr of `out`, in one text.
pub fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn check(cmd: &mut StdCommand) -> R {
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} failed: {}", text(&out)).into())
    }
}

/// The release asset `install.sh` fetches under [`Setup::shell`], whose `uname` answers as
/// `x86_64` Linux on every host.
pub fn archive_name() -> String {
    format!("dotman-v{VERSION}-x86_64-unknown-linux-musl.tar.gz")
}

/// A directory holding only a `uname` that answers `-s` with `Linux` and `-m` with `x86_64`, and
/// passes anything else to the real one, so `install.sh` takes its Linux download path on macOS
/// and on other architectures too.
fn linux_uname(dir: &Path) -> R<PathBuf> {
    // Alpine has only /bin/uname, macOS only /usr/bin/uname.
    let real = SYSTEM_DIRS
        .iter()
        .map(|dir| Path::new(dir).join("uname"))
        .find(|path| path.is_file())
        .ok_or("no uname in /usr/bin or /bin")?;
    write_exe(
        &dir.join("uname"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  -s) echo Linux ;;\n  -m) echo x86_64 ;;\n  *) exec '{}' \"$@\" ;;\nesac\n",
            real.display()
        ),
    )?;
    Ok(dir.to_path_buf())
}

fn sha256_line(dir: &Path, name: &str) -> R<String> {
    let out = StdCommand::new("sha256sum")
        .arg(name)
        .current_dir(dir)
        .output()
        .or_else(|_| {
            StdCommand::new("shasum")
                .args(["-a", "256", name])
                .current_dir(dir)
                .output()
        })?;
    Ok(String::from_utf8(out.stdout)?)
}

/// Writes an executable file, creating parents.
pub fn write_exe(path: &Path, body: &str) -> R {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// The release directory a mirror would serve, built once per test run: a tarball whose top level
/// holds the built `dotman`, and `checksums.txt` with a line for another architecture beside it.
pub fn release() -> R<PathBuf> {
    static RELEASE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    RELEASE
        .get_or_init(|| build_release().map_err(|e| e.to_string()))
        .clone()
        .map_err(Into::into)
}

fn build_release() -> R<PathBuf> {
    let exe = std::env::current_exe()?;
    let name = exe.file_stem().map_or_else(
        || "tests".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("release-{name}"));
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    let pkg = dir.join("pkg");
    fs::create_dir_all(&pkg)?;
    fs::copy(env!("CARGO_BIN_EXE_dotman"), pkg.join("dotman"))?;
    let name = archive_name();
    check(
        StdCommand::new("tar")
            .env("GZIP", "-1")
            .arg("-czf")
            .arg(dir.join(&name))
            .arg("-C")
            .arg(&pkg)
            .arg("dotman"),
    )?;
    let other = "0".repeat(64);
    fs::write(
        dir.join("checksums.txt"),
        format!(
            "{other}  dotman-v{VERSION}-riscv64-unknown-linux-musl.tar.gz\n{}",
            sha256_line(&dir, &name)?
        ),
    )?;
    Ok(dir)
}

/// A machine that runs `install.sh`, and the depth-1 clone of a dotman repo it runs it from.
pub struct Setup {
    _author: TestEnv,
    pub machine: TestEnv,
    pub dotfiles: PathBuf,
    /// The directory of the `uname` stub that [`Setup::shell`] puts on `PATH`.
    pub uname: PathBuf,
}

impl Setup {
    pub fn new() -> R<Self> {
        let author = TestEnv::new();
        author.write(".bashrc", BOOTSTRAP_BASHRC, 0o644);
        author.ok(&["init", "--no-import"]);
        author.ok(&["add", ".bashrc"]);
        author.ok(&["sync"]);
        let machine = TestEnv::new();
        let dotfiles = machine.root().join("dotfiles");
        author.git(&[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", author.repo().display()),
            &dotfiles.to_string_lossy(),
        ]);
        assert!(dotfiles.join("install.sh").is_file());
        assert!(
            dotfiles.join(".git/shallow").is_file(),
            "not a depth-1 clone"
        );
        let uname = linux_uname(&machine.root().join("linux-uname"))?;
        Ok(Self {
            _author: author,
            machine,
            dotfiles: fs::canonicalize(dotfiles)?,
            uname,
        })
    }

    pub fn home(&self) -> PathBuf {
        self.machine.home()
    }

    /// Whether the script put a `dotman` in `~/.local/bin`.
    pub fn installed(&self) -> bool {
        self.home().join(".local/bin/dotman").exists()
    }

    /// Whether `~/.bashrc` holds what the repo holds.
    pub fn applied(&self) -> bool {
        fs::read(self.home().join(".bashrc")).is_ok_and(|bytes| bytes == BOOTSTRAP_BASHRC)
    }

    /// `sh` with `stubs`, then the `uname` stub, in front of the system directories, and no
    /// script yet.
    pub fn shell(&self, base: &Path, stubs: &[&Path]) -> StdCommand {
        let dirs: Vec<PathBuf> = stubs
            .iter()
            .map(|dir| dir.to_path_buf())
            .chain(std::iter::once(self.uname.clone()))
            .chain(SYSTEM_DIRS.iter().map(PathBuf::from))
            .collect();
        self.shell_on(base, &dirs)
    }

    /// `sh` with `PATH` set to exactly `dirs`, and no script yet.
    pub fn shell_on(&self, base: &Path, dirs: &[PathBuf]) -> StdCommand {
        let mut cmd = self.machine.command_with_path("/bin/sh", dirs);
        cmd.env(
            "DOTMAN_BOOTSTRAP_BASE_URL",
            format!("file://{}", base.display()),
        );
        cmd
    }

    /// `sh <dotfiles>/install.sh`.
    pub fn command(&self, base: &Path, stubs: &[&Path]) -> StdCommand {
        let mut cmd = self.shell(base, stubs);
        cmd.arg(self.dotfiles.join("install.sh"));
        cmd
    }

    pub fn run(&self, stubs: &[&Path]) -> R<Output> {
        Ok(self.command(&release()?, stubs).output()?)
    }

    pub fn stubs(&self, name: &str) -> PathBuf {
        self.machine.root().join(name)
    }

    pub fn empty_base(&self) -> R<PathBuf> {
        let dir = self.machine.root().join("empty");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

/// The machine config file.
pub fn machine_config(env: &TestEnv) -> PathBuf {
    env.xdg_config().join("dotman/config.toml")
}

/// An initialised repo with the system root in use and `roots` configured.
pub fn system_setup(roots: &[&str]) -> TestEnv {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.ok(&["init"]);
    if !roots.is_empty() {
        let list: Vec<String> = roots.iter().map(|r| format!("\"{r}\"")).collect();
        env.write_machine_config(&format!("[system]\nroots = [{}]\n", list.join(", ")));
    }
    env
}

/// `system_setup` with `escalate = "none"`, so the helper runs unprivileged below the prefix.
pub fn system_setup_none(roots: &[&str]) -> TestEnv {
    let env = system_setup(&[]);
    configure_machine(&env, &system_config(roots, "none"));
    env
}

/// Replaces the machine config at 0644, so a umask of 002 cannot leave it group-writable, which
/// the helper refuses.
pub fn configure_machine(env: &TestEnv, text: &str) {
    env.write_machine_config(text);
    fs::set_permissions(machine_config(env), fs::Permissions::from_mode(0o644))
        .expect("chmod the machine config");
}

/// Machine config text with `roots` and the `escalate` tool.
pub fn system_config(roots: &[&str], escalate: &str) -> String {
    let list: Vec<String> = roots.iter().map(|r| format!("\"{r}\"")).collect();
    format!(
        "[system]\nroots = [{}]\nescalate = \"{escalate}\"\n",
        list.join(", ")
    )
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

pub fn xxh3(bytes: &[u8]) -> String {
    format!("{:032x}", xxh3_128(bytes))
}

pub fn root_group() -> String {
    dotman::sys::group_by_gid(0).expect("gid 0").name
}

pub fn plan_entry_hashed(target: &str, bytes: &[u8], mode: &str, hash: &str) -> Value {
    json!({
        "target": target,
        "bytes": hex(bytes),
        "xxh3": hash,
        "owner": "root",
        "group": root_group(),
        "mode": mode,
    })
}

pub fn plan_entry(target: &str, bytes: &[u8], mode: &str) -> Value {
    plan_entry_hashed(target, bytes, mode, &xxh3(bytes))
}

pub fn plan_json(env: &TestEnv, roots: &[&str], entries: &[Value]) -> Value {
    let prefix = fs::canonicalize(env.system_root()).expect("canonical system root");
    json!({
        "config": machine_config(env),
        "roots": roots,
        "prefix": prefix,
        "entries": entries,
    })
}

/// Writes `plan` as `apply --system` would: 0600, in 0700 directories under the state dir.
pub fn write_plan(env: &TestEnv, plan: &Value) -> PathBuf {
    let parent = env.state_dir().join("system-plan");
    let dir = parent.join("by-hand");
    fs::create_dir_all(&dir).expect("create the plan dir");
    for private in [&parent, &dir] {
        fs::set_permissions(private, fs::Permissions::from_mode(0o700)).expect("chmod");
    }
    let file = dir.join("plan.json");
    fs::write(&file, serde_json::to_vec(plan).expect("plan JSON")).expect("write the plan");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod the plan");
    file
}

pub fn system_write(env: &TestEnv, plan: &Path) -> Output {
    env.run(&["system-write", "--plan", &plan.to_string_lossy()])
}

/// Every hook name git runs around commit, checkout, merge, rebase and push.
pub const GIT_HOOKS: [&str; 10] = [
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "post-checkout",
    "post-merge",
    "pre-rebase",
    "post-rewrite",
    "pre-push",
    "reference-transaction",
];

/// Runs the binary with `args` under `script(1)`, so its stdin and stdout are a terminal, as an
/// interactive run with the harness environment plus `vars`, feeding it `input`. Returns what the
/// terminal showed, or `None` where there is no `script` to run.
pub fn in_a_terminal(
    env: &TestEnv,
    vars: &[(&str, &str)],
    args: &[&str],
    input: &[u8],
) -> Option<String> {
    use std::io::Write as _;
    use std::process::Stdio;
    if !cfg!(target_os = "linux") {
        return None;
    }
    let script = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("script"))
        .find(|path| path.is_file())?;
    let dot = env.dot_process();
    let mut line = shell_words::quote(&dot.get_program().to_string_lossy()).into_owned();
    for arg in args {
        line.push(' ');
        line.push_str(&shell_words::quote(arg));
    }
    let mut cmd = StdCommand::new(script);
    cmd.env_clear();
    for (name, value) in dot.get_envs() {
        if let Some(value) = value {
            cmd.env(name, value);
        }
    }
    cmd.env("DOTMAN_NON_INTERACTIVE", "");
    for (name, value) in vars {
        cmd.env(name, value);
    }
    let mut child = cmd
        .current_dir(env.home())
        .args(["-qec", &line, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input).ok()?;
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The arguments after `dotman` of the first backticked `dotman <prefix>...` command in `text`,
/// split as a shell would, so a test can run a printed hint exactly as printed.
pub fn hint(text: &str, prefix: &str) -> Vec<String> {
    let found = text
        .split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|quoted| shell_words::split(quoted).ok())
        .filter(|words| words.first().map(String::as_str) == Some("dotman"))
        .map(|words| words.into_iter().skip(1).collect::<Vec<_>>())
        .find(|args| args.join(" ").starts_with(prefix));
    assert!(found.is_some(), "no `dotman {prefix}...` hint in: {text}");
    found.unwrap_or_default()
}

/// Runs the hint that [`hint`] finds, as printed.
pub fn run_hint(env: &TestEnv, text: &str, prefix: &str) -> Output {
    let args = hint(text, prefix);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    env.run(&args)
}

/// Writes an executable hook of every name in [`GIT_HOOKS`] into `hooks_dir`; each appends its
/// name to `marker`.
pub fn plant_git_hooks(hooks_dir: &Path, marker: &Path) {
    fs::create_dir_all(hooks_dir).expect("create the hooks dir");
    for name in GIT_HOOKS {
        write_exe(
            &hooks_dir.join(name),
            &format!("#!/bin/sh\necho {name} >> '{}'\nexit 0\n", marker.display()),
        )
        .expect("write a hook");
    }
}
