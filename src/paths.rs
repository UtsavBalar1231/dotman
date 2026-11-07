//! Locations of dotman's own files, and the rules that turn user paths into repo keys.
//!
//! Nothing here reads the process environment except [`Env::from_process`], so every rule can be
//! tested with injected values.

use crate::config::Config;
use crate::output;
use crate::sys;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use unicode_normalization::{UnicodeNormalization, is_nfc};

/// The environment variables dotman reads (`HOME`, `XDG_{CONFIG,STATE,DATA}_HOME`,
/// `DOTMAN_REPO_PATH`, `DOTMAN_CONFIG_PATH`, `DOTMAN_AGE_KEY_FILE`, `SUDO_USER`), captured once.
#[derive(Debug, Default, Clone)]
pub struct Env {
    home: Option<OsString>,
    xdg_config_home: Option<OsString>,
    xdg_state_home: Option<OsString>,
    xdg_data_home: Option<OsString>,
    repo_path: Option<OsString>,
    config_path: Option<OsString>,
    age_key_file: Option<OsString>,
    sudo_user: Option<OsString>,
}

impl Env {
    /// Reads the variables through `get`, which tests replace with a fixed table.
    pub fn from_fn(get: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            home: get("HOME"),
            xdg_config_home: get("XDG_CONFIG_HOME"),
            xdg_state_home: get("XDG_STATE_HOME"),
            xdg_data_home: get("XDG_DATA_HOME"),
            repo_path: get("DOTMAN_REPO_PATH"),
            config_path: get("DOTMAN_CONFIG_PATH"),
            age_key_file: get("DOTMAN_AGE_KEY_FILE"),
            sudo_user: get("SUDO_USER"),
        }
    }

    /// Reads the variables from the running process.
    #[must_use]
    pub fn from_process() -> Self {
        Self::from_fn(|name| std::env::var_os(name))
    }
}

/// Fails when running as root without `--allow-root` or `DOTMAN_ALLOW_ROOT=1`.
pub fn ensure_not_root(euid: u32, allow_flag: bool, allow_env: Option<&OsStr>) -> Result<()> {
    if euid == 0 && !allow_flag && allow_env != Some(OsStr::new("1")) {
        bail!("dotman does not run as root; use --allow-root inside containers");
    }
    Ok(())
}

/// Resolved locations for one run.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `$HOME` exactly as the environment gave it. Keys are derived from this lexically.
    pub home: PathBuf,
    /// `realpath($HOME)` when it differs from `home` (Fedora Silverblue links `/home` to `/var/home`).
    pub home_real: Option<PathBuf>,
    /// Root of the git repo, which maps to `$HOME`. Precedence: `DOTMAN_REPO_PATH`, then the
    /// machine config's `repo`, then `$XDG_DATA_HOME/dotman`.
    pub repo: PathBuf,
    /// Machine config file; never synced. `DOTMAN_CONFIG_PATH` overrides the default.
    pub config_file: PathBuf,
    /// Holds `state.toml`, the lock and backups; never synced.
    pub state_dir: PathBuf,
    /// Age identity file. The machine config's `[age] identity` replaces the default.
    pub age_identity: PathBuf,
    /// Identity file named by `DOTMAN_AGE_KEY_FILE`, read as well as `age_identity`.
    pub age_key_file: Option<PathBuf>,
    repo_from_env: bool,
}

impl Paths {
    /// Validates `$HOME` and resolves every location. Creates nothing.
    pub fn resolve(env: &Env, euid: u32) -> Result<Self> {
        let home = match env.home.as_deref() {
            Some(h) if !h.is_empty() => PathBuf::from(h),
            _ => bail!("$HOME is not set"),
        };
        ensure!(
            home.is_absolute(),
            "$HOME must be an absolute path, got {}",
            home.display()
        );
        let owner = fs::metadata(&home)
            .with_context(|| format!("cannot stat $HOME ({})", home.display()))?
            .uid();
        check_home(
            &home,
            euid,
            owner,
            env.sudo_user.as_deref().is_some_and(|s| !s.is_empty()),
            || sys::passwd_by_uid(euid).map(|pw| pw.home),
        )?;

        let config_home = xdg_dir(env.xdg_config_home.as_deref(), &home, ".config");
        let state_home = xdg_dir(env.xdg_state_home.as_deref(), &home, ".local/state");
        let data_home = xdg_dir(env.xdg_data_home.as_deref(), &home, ".local/share");

        let repo_override = absolute_override("DOTMAN_REPO_PATH", env.repo_path.as_deref())?;
        let config_override = absolute_override("DOTMAN_CONFIG_PATH", env.config_path.as_deref())?;
        let age_key_file = absolute_override("DOTMAN_AGE_KEY_FILE", env.age_key_file.as_deref())?;

        let home_real = fs::canonicalize(&home).ok().filter(|real| *real != home);
        Ok(Self {
            home,
            home_real,
            repo_from_env: repo_override.is_some(),
            repo: repo_override.unwrap_or_else(|| data_home.join("dotman")),
            config_file: config_override.unwrap_or_else(|| config_home.join("dotman/config.toml")),
            state_dir: state_home.join("dotman"),
            age_identity: config_home.join("dotman/age-identity.txt"),
            age_key_file,
        })
    }

    /// Applies the machine config. `DOTMAN_REPO_PATH` still beats its `repo`.
    pub fn apply_config(mut self, config: &Config) -> Result<Self> {
        if let Some(repo) = config.repo.as_ref().filter(|_| !self.repo_from_env) {
            ensure!(
                repo.is_absolute(),
                "`repo` in the machine config must be an absolute path, got {}",
                repo.display()
            );
            self.repo.clone_from(repo);
        }
        if let Some(identity) = &config.age.identity {
            ensure!(
                identity.is_absolute(),
                "`[age] identity` in the machine config must be an absolute path, got {}",
                identity.display()
            );
            self.age_identity.clone_from(identity);
        }
        Ok(self)
    }

    /// Path of `state.toml`.
    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.toml")
    }

    /// Path of the process lock file.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("lock")
    }

    /// Fails for `path` if it is, or lies inside, the repo, the state dir, the machine config
    /// file or an age identity file. `path` must be absolute.
    pub fn refuse_protected(&self, path: &Path) -> Result<()> {
        ensure!(
            !path.components().any(|c| c == Component::ParentDir),
            "{} contains `..`",
            path.display()
        );
        let protected = [
            ("the dotman repo", &self.repo),
            ("the dotman state directory", &self.state_dir),
            ("the dotman machine config", &self.config_file),
            ("the age identity file", &self.age_identity),
        ];
        let key_file = self
            .age_key_file
            .as_ref()
            .map(|file| ("the age identity file named by DOTMAN_AGE_KEY_FILE", file));
        for (what, root) in protected.into_iter().chain(key_file) {
            // The canonical form is checked too, so `/var/home/...` input matches a `/home/...` default.
            let inside = path.starts_with(root)
                || fs::canonicalize(root).is_ok_and(|real| path.starts_with(real));
            ensure!(
                !inside,
                "{} is {what} or inside it; dotman never manages its own files",
                path.display()
            );
        }
        Ok(())
    }
}

fn check_home(
    home: &Path,
    euid: u32,
    owner: u32,
    sudo: bool,
    passwd_home: impl FnOnce() -> Option<PathBuf>,
) -> Result<()> {
    ensure!(
        owner == euid,
        "$HOME ({}) is owned by uid {owner}, not by the current user (uid {euid})",
        home.display()
    );
    if euid != 0 && !sudo {
        return Ok(());
    }
    // sudo keeps or rewrites HOME depending on the distro, so under root or sudo HOME is checked against passwd.
    match passwd_home() {
        Some(pw) if !same_dir(&pw, home) => bail!(
            "$HOME ({}) differs from the passwd home of uid {euid} ({}); run dot with the user's own environment",
            home.display(),
            pw.display()
        ),
        Some(_) => {}
        None => output::warning(&format!(
            "no passwd entry for uid {euid}; using $HOME ({}) unverified",
            home.display()
        )),
    }
    Ok(())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

fn xdg_dir(value: Option<&OsStr>, home: &Path, fallback: &str) -> PathBuf {
    match value {
        Some(v) if Path::new(v).is_absolute() => PathBuf::from(v),
        _ => home.join(fallback),
    }
}

fn absolute_override(name: &str, value: Option<&OsStr>) -> Result<Option<PathBuf>> {
    match value {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) => {
            let path = PathBuf::from(v);
            ensure!(
                path.is_absolute(),
                "{name} must be an absolute path, got {}",
                path.display()
            );
            Ok(Some(path))
        }
    }
}

/// Creates `path` and any missing ancestors at mode 0700. A dry run creates nothing.
pub fn create_private_dir(path: &Path, dry_run: bool) -> io::Result<()> {
    if dry_run {
        return Ok(());
    }
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// A 0700 directory under `base`, removed when the value is dropped. tempfile's default mode would
/// follow the umask and could leave rendered or decrypted content readable by others.
pub fn private_temp_dir(prefix: &str, base: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(base)
        .with_context(|| format!("cannot create a temp dir in {}", base.display()))
}

/// Writes `bytes` to a temp file beside `path`, fsyncs it, renames it over `path` and fsyncs the
/// directory. The final mode is exactly `mode`, whatever the umask. `path`'s directory must exist.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".dotman-tmp-")
        .tempfile_in(dir)
        .with_context(|| format!("cannot create a temp file in {}", dir.display()))?;
    tmp.write_all(bytes)?;
    tmp.as_file()
        .set_permissions(Permissions::from_mode(mode))?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("cannot fsync {}", dir.display()))
}

/// Expands a leading `~` and joins a relative path onto `cwd`, without resolving anything.
#[must_use]
pub fn absolute_input(input: &Path, cwd: &Path, home: &Path) -> PathBuf {
    let expanded = input
        .strip_prefix("~")
        .map_or_else(|_| input.to_path_buf(), |rest| home.join(rest));
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Derives the `$HOME`-relative key for a user-supplied path (absolute, `~`-prefixed or relative to `cwd`).
///
/// The derivation is lexical: the target is never resolved, so a symlink at the target stays a
/// symlink. Input under `home_real` is accepted as well as input under `home`.
pub fn derive_key(
    input: &Path,
    cwd: &Path,
    home: &Path,
    home_real: Option<&Path>,
) -> Result<String> {
    let absolute = absolute_input(input, cwd, home);
    ensure!(
        !absolute.components().any(|c| c == Component::ParentDir),
        "{} contains `..`",
        input.display()
    );
    let rest = std::iter::once(home)
        .chain(home_real)
        .find_map(|root| absolute.strip_prefix(root).ok())
        .ok_or_else(|| {
            anyhow!(
                "{} is not under $HOME ({})",
                input.display(),
                home.display()
            )
        })?;

    let mut parts = Vec::new();
    for component in rest.components() {
        let Component::Normal(name) = component else {
            bail!("{} is not a plain relative path", input.display());
        };
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("{} is not valid UTF-8 and cannot be a key", input.display()))?;
        parts.push(name);
    }
    let key: String = parts.join("/").nfc().collect();
    validate_key(&key)?;
    Ok(key)
}

/// Checks a key as stored in the repo: relative, NFC, no `.`/`..`/empty components, not a reserved name.
pub fn validate_key(key: &str) -> Result<()> {
    ensure!(!key.is_empty(), "empty key");
    ensure!(!key.starts_with('/'), "key `{key}` is absolute");
    ensure!(!key.contains('\0'), "key `{key}` contains a NUL byte");
    ensure!(is_nfc(key), "key `{key}` is not NFC-normalized");
    for component in key.split('/') {
        ensure!(
            !matches!(component, "" | "." | ".."),
            "key `{key}` has an empty, `.` or `..` component"
        );
    }
    let first = key.split('/').next().unwrap_or_default();
    ensure!(
        !is_reserved_root_name(first),
        "`{first}` is a reserved name at the repo root"
    );
    Ok(())
}

// Case-insensitive because APFS and other case-insensitive filesystems resolve `.GIT` to `.git`.
fn is_reserved_root_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    matches!(
        lower.as_str(),
        ".git" | ".dotman" | ".dotman.toml" | "install.sh"
    ) || lower.starts_with("readme")
        || lower.starts_with("license")
}

/// Pairs of distinct keys that are equal under case folding or NFC. Each key after the first in
/// a colliding group is paired with that first key.
#[must_use]
pub fn find_collisions<'a>(keys: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut groups: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for key in keys {
        groups
            .entry(key.to_lowercase().nfc().collect())
            .or_default()
            .insert(key);
    }
    let mut pairs = Vec::new();
    for group in groups.into_values() {
        let mut members = group.into_iter();
        if let Some(first) = members.next() {
            pairs.extend(members.map(|other| (first.to_owned(), other.to_owned())));
        }
    }
    pairs
}

/// Refuses a path with a component that is not NFC, naming the NFC form. On ext4 the NFD and NFC
/// spellings are different files, so an NFD file could not be found again at its NFC key.
pub fn refuse_non_nfc(path: &Path) -> Result<()> {
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let Some(name) = name.to_str() else {
            bail!("{} is not valid UTF-8", path.display());
        };
        if !is_nfc(name) {
            let nfc: String = name.nfc().collect();
            bail!(
                "{} has a name that is not NFC-normalized; its NFC form is `{nfc}`",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgeConfig;
    use proptest::prelude::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn env_with(home: &Path, extra: &[(&str, &str)]) -> Env {
        let home = home.as_os_str().to_owned();
        Env::from_fn(|name| {
            if name == "HOME" {
                return Some(home.clone());
            }
            extra
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        })
    }

    fn resolve(env: &Env) -> Paths {
        Paths::resolve(env, sys::geteuid()).expect("resolve")
    }

    #[test]
    fn root_needs_flag_or_env() {
        assert!(ensure_not_root(0, false, None).is_err());
        assert!(ensure_not_root(0, false, Some(OsStr::new("0"))).is_err());
        assert!(ensure_not_root(0, true, None).is_ok());
        assert!(ensure_not_root(0, false, Some(OsStr::new("1"))).is_ok());
        assert!(ensure_not_root(1000, false, None).is_ok());
        let msg = ensure_not_root(0, false, None).unwrap_err().to_string();
        assert_eq!(
            msg,
            "dotman does not run as root; use --allow-root inside containers"
        );
    }

    #[test]
    fn home_must_be_set_absolute_and_owned() {
        let tmp = TempDir::new().unwrap();
        let euid = sys::geteuid();
        assert!(Paths::resolve(&Env::default(), euid).is_err());
        assert!(Paths::resolve(&env_with(Path::new(""), &[]), euid).is_err());
        assert!(Paths::resolve(&env_with(Path::new("relative"), &[]), euid).is_err());
        assert!(Paths::resolve(&env_with(tmp.path(), &[]), euid.wrapping_add(1)).is_err());
        assert!(Paths::resolve(&env_with(tmp.path(), &[]), euid).is_ok());
    }

    #[test]
    fn home_is_checked_against_passwd_under_root_or_sudo() {
        let home = Path::new("/home/alice");
        let same = || Some(PathBuf::from("/home/alice"));
        let other = || Some(PathBuf::from("/root"));
        assert!(check_home(home, 0, 0, false, same).is_ok());
        assert!(check_home(home, 0, 0, false, other).is_err());
        assert!(check_home(home, 1000, 1000, true, other).is_err());
        assert!(check_home(home, 1000, 1000, false, other).is_ok());
        assert!(check_home(home, 0, 0, false, || None).is_ok());
        assert!(check_home(home, 1000, 0, false, same).is_err());
    }

    #[test]
    fn sudo_user_triggers_passwd_comparison_through_resolve() {
        let tmp = TempDir::new().unwrap();
        let euid = sys::geteuid();
        let env = env_with(tmp.path(), &[("SUDO_USER", "someone")]);
        if let Some(pw) = sys::passwd_by_uid(euid)
            && !same_dir(&pw.home, tmp.path())
        {
            assert!(Paths::resolve(&env, euid).is_err());
        }
    }

    #[test]
    fn xdg_unset_empty_or_relative_use_defaults() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        for value in ["", "relative/dir", "."] {
            let set = env_with(
                home,
                &[
                    ("XDG_CONFIG_HOME", value),
                    ("XDG_STATE_HOME", value),
                    ("XDG_DATA_HOME", value),
                ],
            );
            for env in [set, env_with(home, &[])] {
                let paths = resolve(&env);
                assert_eq!(paths.config_file, home.join(".config/dotman/config.toml"));
                assert_eq!(paths.state_dir, home.join(".local/state/dotman"));
                assert_eq!(paths.repo, home.join(".local/share/dotman"));
                assert_eq!(
                    paths.age_identity,
                    home.join(".config/dotman/age-identity.txt")
                );
            }
        }
    }

    #[test]
    fn absolute_xdg_values_are_used() {
        let tmp = TempDir::new().unwrap();
        let env = env_with(
            tmp.path(),
            &[
                ("XDG_CONFIG_HOME", "/xdg/config"),
                ("XDG_STATE_HOME", "/xdg/state"),
                ("XDG_DATA_HOME", "/xdg/data"),
            ],
        );
        let paths = resolve(&env);
        assert_eq!(
            paths.config_file,
            Path::new("/xdg/config/dotman/config.toml")
        );
        assert_eq!(paths.state_dir, Path::new("/xdg/state/dotman"));
        assert_eq!(paths.repo, Path::new("/xdg/data/dotman"));
        assert_eq!(
            paths.state_file(),
            Path::new("/xdg/state/dotman/state.toml")
        );
        assert_eq!(paths.lock_file(), Path::new("/xdg/state/dotman/lock"));
    }

    #[test]
    fn env_overrides_beat_config_and_defaults() {
        let tmp = TempDir::new().unwrap();
        let config = Config {
            repo: Some("/cfg/repo".into()),
            age: AgeConfig {
                identity: Some("/cfg/identity.txt".into()),
                ..AgeConfig::default()
            },
            ..Config::default()
        };

        let env = env_with(
            tmp.path(),
            &[
                ("DOTMAN_REPO_PATH", "/env/repo"),
                ("DOTMAN_CONFIG_PATH", "/env/config.toml"),
            ],
        );
        let paths = resolve(&env).apply_config(&config).unwrap();
        assert_eq!(paths.repo, Path::new("/env/repo"));
        assert_eq!(paths.config_file, Path::new("/env/config.toml"));
        assert_eq!(paths.age_identity, Path::new("/cfg/identity.txt"));

        let paths = resolve(&env_with(tmp.path(), &[]))
            .apply_config(&config)
            .unwrap();
        assert_eq!(paths.repo, Path::new("/cfg/repo"));

        let relative = env_with(tmp.path(), &[("DOTMAN_REPO_PATH", "relative")]);
        assert!(Paths::resolve(&relative, sys::geteuid()).is_err());
    }

    #[test]
    fn symlinked_home_works_and_both_spellings_give_one_key() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("var/home/u");
        fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("home-u");
        symlink(&real, &link).unwrap();

        let paths = resolve(&env_with(&link, &[]));
        assert_eq!(paths.home, link);
        let real_canon = fs::canonicalize(&real).unwrap();
        assert_eq!(paths.home_real.as_deref(), Some(real_canon.as_path()));

        let home_real = paths.home_real.as_deref();
        let via_link = derive_key(&link.join(".bashrc"), &link, &paths.home, home_real);
        let via_real = derive_key(&real_canon.join(".bashrc"), &link, &paths.home, home_real);
        assert_eq!(via_link.unwrap(), ".bashrc");
        assert_eq!(via_real.unwrap(), ".bashrc");
    }

    #[test]
    fn keys_from_relative_tilde_and_absolute_input() {
        let home = Path::new("/home/u");
        let cwd = Path::new("/home/u/.config");
        let key = |p: &str| derive_key(Path::new(p), cwd, home, None);
        assert_eq!(key("nvim/init.lua").unwrap(), ".config/nvim/init.lua");
        assert_eq!(key("~/.zshrc").unwrap(), ".zshrc");
        assert_eq!(key("/home/u/.a/./b/").unwrap(), ".a/b");
        assert_eq!(key("./x").unwrap(), ".config/x");
        assert!(key("/etc/passwd").is_err());
        assert!(key("~").is_err());
        assert!(key("/home/u").is_err());
        assert!(key("../.bashrc").is_err());
        assert!(key("/home/u/a/../b").is_err());
        assert!(key("/home/user2/x").is_err());
    }

    #[test]
    fn keys_are_nfc_normalized() {
        let home = Path::new("/home/u");
        let key = derive_key(Path::new("e\u{301}.txt"), home, home, None).unwrap();
        assert_eq!(key, "\u{e9}.txt");
    }

    #[test]
    fn reserved_names_only_at_the_root() {
        let home = Path::new("/home/u");
        let key = |p: &str| derive_key(Path::new(p), home, home, None);
        for name in [
            ".git",
            ".git/config",
            ".dotman",
            ".dotman/system/x",
            ".dotman.toml",
            "README.md",
            "readme",
            "LICENSE-MIT",
            "install.sh",
        ] {
            assert!(key(name).is_err(), "{name} should be reserved");
        }
        assert_eq!(
            key(".config/nvim/README.md").unwrap(),
            ".config/nvim/README.md"
        );
        assert_eq!(key("sub/install.sh").unwrap(), "sub/install.sh");
        assert_eq!(key(".gitconfig").unwrap(), ".gitconfig");
    }

    #[test]
    fn collisions_by_case_and_normalization() {
        let keys = [
            "a/File",
            "a/file",
            "b",
            "caf\u{e9}",
            "cafe\u{301}",
            "c/x",
            "c/x",
        ];
        assert_eq!(
            find_collisions(keys),
            vec![
                ("a/File".to_owned(), "a/file".to_owned()),
                ("cafe\u{301}".to_owned(), "caf\u{e9}".to_owned()),
            ]
        );
        assert_eq!(find_collisions(["a", "b", "a"]), Vec::new());
    }

    #[test]
    fn nfd_names_are_refused_with_the_nfc_form() {
        let err = refuse_non_nfc(Path::new("/home/u/cafe\u{301}.txt"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("caf\u{e9}.txt"), "{err}");
        assert!(refuse_non_nfc(Path::new("/home/u/caf\u{e9}.txt")).is_ok());
        assert!(refuse_non_nfc(Path::new("/home/u/cafe\u{301}/x")).is_err());
    }

    #[test]
    fn own_files_are_protected() {
        let tmp = TempDir::new().unwrap();
        let paths = resolve(&env_with(tmp.path(), &[]));
        for p in [
            paths.repo.clone(),
            paths.repo.join(".bashrc"),
            paths.state_dir.join("state.toml"),
            paths.config_file.clone(),
            paths.age_identity.clone(),
        ] {
            assert!(paths.refuse_protected(&p).is_err(), "{}", p.display());
        }
        assert!(paths.refuse_protected(&tmp.path().join(".bashrc")).is_ok());
        assert!(
            paths
                .refuse_protected(&tmp.path().join(".local/share/dotman-other"))
                .is_ok()
        );
        assert!(paths.refuse_protected(&paths.repo.join("../x")).is_err());

        let key_file = tmp.path().join("keys/age.txt");
        let key_env = env_with(
            tmp.path(),
            &[("DOTMAN_AGE_KEY_FILE", key_file.to_str().unwrap())],
        );
        let with_key = resolve(&key_env);
        assert!(with_key.refuse_protected(&key_file).is_err());
        assert!(paths.refuse_protected(&key_file).is_ok());
        let relative = env_with(tmp.path(), &[("DOTMAN_AGE_KEY_FILE", "age.txt")]);
        assert!(Paths::resolve(&relative, sys::geteuid()).is_err());
    }

    #[test]
    fn protected_check_matches_the_real_spelling_of_a_symlinked_home() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join(".local/share/dotman")).unwrap();
        let link = tmp.path().join("link");
        symlink(&real, &link).unwrap();
        let paths = resolve(&env_with(&link, &[]));
        let via_real = fs::canonicalize(&real)
            .unwrap()
            .join(".local/share/dotman/.bashrc");
        assert!(paths.refuse_protected(&via_real).is_err());
    }

    #[test]
    fn private_dirs_are_0700_and_dry_run_creates_nothing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("a/b");
        create_private_dir(&dir, true).unwrap();
        assert!(!tmp.path().join("a").exists());
        create_private_dir(&dir, false).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn private_temp_dirs_are_0700_and_removed_on_drop() {
        let tmp = TempDir::new().unwrap();
        let dir = private_temp_dir("dotman-test-", tmp.path()).unwrap();
        let path = dir.path().to_path_buf();
        assert!(path.starts_with(tmp.path()));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn write_atomic_sets_mode_and_replaces() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f");
        write_atomic(&file, b"one", 0o600).unwrap();
        write_atomic(&file, b"two", 0o644).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"two");
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    proptest! {
        #[test]
        fn derived_keys_are_never_absolute_or_contain_dotdot(
            input in "[a-zA-Z0-9./~\\\\ \u{e9}\u{301}-]{0,40}",
            use_real in any::<bool>(),
        ) {
            let home = Path::new("/home/u");
            let real = Path::new("/var/home/u");
            let cwd = Path::new("/home/u/work");
            if let Ok(key) = derive_key(Path::new(&input), cwd, home, use_real.then_some(real)) {
                prop_assert!(!key.starts_with('/'));
                prop_assert!(key.split('/').all(|c| c != ".." && !c.is_empty()));
                prop_assert!(validate_key(&key).is_ok());
            }
        }

        #[test]
        fn validate_key_never_accepts_absolute_or_dotdot(key in "[a-z./]{0,20}") {
            if validate_key(&key).is_ok() {
                prop_assert!(!key.starts_with('/'));
                prop_assert!(key.split('/').all(|c| c != ".."));
            }
        }
    }
}
