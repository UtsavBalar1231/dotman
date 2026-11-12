//! Locations of dotman's own files, and the rules that turn user paths into repo keys.
//!
//! Nothing here reads the process environment except [`Env::from_process`], so every rule can be
//! tested with injected values.

use crate::config::Config;
use crate::context::is_under;
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
/// `DOTMAN_REPO_PATH`, `DOTMAN_CONFIG_PATH`, `DOTMAN_AGE_KEY_FILE`, `DOTMAN_SYSTEM_ROOT`, `SUDO_USER`),
/// captured once.
#[derive(Debug, Default, Clone)]
pub struct Env {
    home: Option<OsString>,
    xdg_config_home: Option<OsString>,
    xdg_state_home: Option<OsString>,
    xdg_data_home: Option<OsString>,
    repo_path: Option<OsString>,
    config_path: Option<OsString>,
    age_key_file: Option<OsString>,
    system_root: Option<OsString>,
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
            system_root: get("DOTMAN_SYSTEM_ROOT"),
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
///
/// # Errors
///
/// Fails when `euid` is 0 and neither `allow_flag` nor an `allow_env` of `1` permits it.
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
    /// `$XDG_CONFIG_HOME`, or `$HOME/.config`.
    pub config_home: PathBuf,
    /// `$XDG_DATA_HOME`, or `$HOME/.local/share`.
    pub data_home: PathBuf,
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
    /// `DOTMAN_SYSTEM_ROOT`, canonicalized once. When set, system paths are read below it.
    pub system_root: Option<PathBuf>,
    repo_from_env: bool,
}

impl Paths {
    /// Validates `$HOME` and resolves every location. Creates nothing.
    ///
    /// # Errors
    ///
    /// Fails when `$HOME` is unset, relative or not owned by the user, an override variable is
    /// relative, or `DOTMAN_SYSTEM_ROOT` cannot be resolved.
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

        let system_root = absolute_override("DOTMAN_SYSTEM_ROOT", env.system_root.as_deref())?
            .map(|root| {
                fs::canonicalize(&root).with_context(|| {
                    format!("DOTMAN_SYSTEM_ROOT ({}) cannot be resolved", root.display())
                })
            })
            .transpose()?;
        let home_real = fs::canonicalize(&home).ok().filter(|real| *real != home);
        Ok(Self {
            home,
            home_real,
            repo_from_env: repo_override.is_some(),
            repo: repo_override.unwrap_or_else(|| data_home.join("dotman")),
            config_file: config_override.unwrap_or_else(|| config_home.join("dotman/config.toml")),
            state_dir: state_home.join("dotman"),
            age_identity: config_home.join("dotman/age-identity.txt"),
            config_home,
            data_home,
            age_key_file,
            system_root,
        })
    }

    /// Applies the machine config. `DOTMAN_REPO_PATH` still beats its `repo`.
    pub(crate) fn apply_config(mut self, config: &Config) -> Result<Self> {
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
}

/// Decides whether a key may be managed on this machine. Every key that a repo file, a state entry
/// or a `$HOME` file becomes goes through [`KeyRules::check`].
#[derive(Debug)]
pub(crate) struct KeyRules {
    /// The keys of dotman's own files and directories under `$HOME`, in [`fold`]ed form, with
    /// what each one is.
    own: Vec<(String, &'static str)>,
}

impl KeyRules {
    /// dotman's own paths come from this machine's environment and config, so a key that is one
    /// here is refused even when the machine that pushed it keeps them elsewhere.
    #[must_use]
    pub(crate) fn new(paths: &Paths) -> Self {
        let config_dir = paths.config_home.join("dotman");
        let listed = [
            ("the dotman repo", Some(&paths.repo)),
            ("the dotman state directory", Some(&paths.state_dir)),
            ("the dotman machine config", Some(&paths.config_file)),
            ("the age identity file", Some(&paths.age_identity)),
            (
                "the age identity file named by DOTMAN_AGE_KEY_FILE",
                paths.age_key_file.as_ref(),
            ),
            ("the dotman config directory", Some(&config_dir)),
        ];
        let homes: Vec<&Path> = std::iter::once(paths.home.as_path())
            .chain(paths.home_real.as_deref())
            .collect();
        let mut own: Vec<(String, &'static str)> = Vec::new();
        for (what, path) in listed {
            let Some(path) = path else { continue };
            // The real form too, so `~/.config` linked to `~/cfg` also refuses `cfg/dotman/...`.
            for form in [path.clone(), resolved(path)] {
                let keys = homes
                    .iter()
                    .filter_map(|home| form.strip_prefix(home).ok()?.to_str())
                    .filter(|key| !key.is_empty())
                    .map(fold);
                for key in keys {
                    if !own.iter().any(|(known, _)| *known == key) {
                        own.push((key, what));
                    }
                }
            }
        }
        Self { own }
    }

    /// Fails for a key that [`validate_key`] refuses or that is, or lies inside, one of dotman's
    /// own paths. A `leaf`, a file or symlink rather than a directory to walk, also fails when one
    /// of them lies inside it: a symlink there would move dotman's files to what the repo points
    /// it at.
    pub(crate) fn check(&self, key: &str, leaf: bool) -> Result<()> {
        validate_key(key)?;
        if let Some(what) = self.own(key, leaf) {
            bail!(
                "`{key}` is {what} on this machine, or a path inside it or above it; dotman never manages its own files"
            );
        }
        Ok(())
    }

    /// What of dotman's own `key` is, holds or lies inside; `None` when it is none of them.
    #[must_use]
    pub(crate) fn own(&self, key: &str, leaf: bool) -> Option<&'static str> {
        let key = fold(key);
        self.own
            .iter()
            .find(|(own, _)| is_under(&key, own) || (leaf && is_under(own, &key)))
            .map(|(_, what)| *what)
    }
}

/// Lowercase NFC, because APFS and other case-insensitive filesystems resolve `.Config/Dotman` to
/// `.config/dotman`.
fn fold(key: &str) -> String {
    key.nfc().collect::<String>().to_lowercase()
}

/// `path` with its longest existing ancestor resolved, so a file not created yet still resolves
/// through a symlinked directory above it.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = fs::canonicalize(at) {
            return rest.iter().rev().fold(real, |dir, name| dir.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
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
            "$HOME ({}) differs from the passwd home of uid {euid} ({}); run {} with the user's own environment",
            home.display(),
            pw.display(),
            crate::name::bin()
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

/// Creates `path` and any missing ancestors at mode 0700, whatever the umask. A dry run creates
/// nothing.
pub(crate) fn create_private_dir(path: &Path, dry_run: bool) -> io::Result<()> {
    if dry_run {
        return Ok(());
    }
    let missing: Vec<&Path> = path
        .ancestors()
        .filter(|dir| !dir.as_os_str().is_empty())
        .take_while(|dir| {
            fs::symlink_metadata(dir).is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        })
        .collect();
    for dir in missing.into_iter().rev() {
        match DirBuilder::new().mode(0o700).create(dir) {
            // The umask narrows mkdir's mode, and under 0277 a 0500 directory could not hold the
            // next one or any file, so each is set to 0700 before anything goes inside it.
            Ok(()) => fs::set_permissions(dir, Permissions::from_mode(0o700))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    if fs::metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a directory", path.display()),
        ))
    }
}

/// A 0700 directory under `base`, removed when the value is dropped. tempfile's default mode would
/// follow the umask and could leave rendered or decrypted content readable by others.
pub(crate) fn private_temp_dir(prefix: &str, base: &Path) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(base)
        .with_context(|| format!("cannot create a temp dir in {}", base.display()))?;
    // The umask narrows the mode tempfile asks for, as it does any mkdir.
    fs::set_permissions(dir.path(), Permissions::from_mode(0o700))
        .with_context(|| format!("cannot set the mode of {}", dir.path().display()))?;
    Ok(dir)
}

/// Writes `bytes` to a temp file beside `path`, fsyncs it, renames it over `path` and fsyncs the
/// directory. The final mode is exactly `mode`, whatever the umask. `path`'s directory must exist.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".dotman-tmp-{}-", std::process::id()))
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
pub(crate) fn derive_key(
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

/// Checks a key as stored in the repo: relative, NFC, no `.`/`..`/empty components, no `.git`
/// component at any depth, not a reserved name.
pub(crate) fn validate_key(key: &str) -> Result<()> {
    ensure!(!key.is_empty(), "empty key");
    // A newline in a name would forge lines of output, and an ESC or a C1 CSI (U+009B) would
    // reach the terminal. First, so the messages below can show the key as it is.
    ensure!(
        !key.chars().any(char::is_control),
        "key `{}` contains a control character",
        key.escape_debug()
    );
    // Output escapes them, so a printed command could not name such a key, and git's own output
    // under `dotman git` would show it raw.
    ensure!(
        !key.chars().any(crate::output::is_hidden_format),
        "key `{}` contains a bidi control or an invisible format character",
        crate::output::escape_line(key)
    );
    ensure!(!key.starts_with('/'), "key `{key}` is absolute");
    ensure!(is_nfc(key), "key `{key}` is not NFC-normalized");
    for component in key.split('/') {
        ensure!(
            !matches!(component, "" | "." | ".."),
            "key `{key}` has an empty, `.` or `..` component"
        );
        // A `.git/config` applied into HOME runs its `core.*` commands the next time git is used
        // in that directory. Git refuses the names that NTFS resolves to `.git` as well: `.GIT`,
        // the short name `git~1`, trailing spaces and dots, an `:stream` suffix and `\` separators.
        let git_dir = component.split('\\').any(|part| {
            let bare = part
                .split(':')
                .next()
                .unwrap_or_default()
                .trim_end_matches([' ', '.']);
            bare.eq_ignore_ascii_case(".git") || bare.eq_ignore_ascii_case("git~1")
        });
        ensure!(!git_dir, "key `{key}` has a `.git` component");
    }
    let first = key.split('/').next().unwrap_or_default();
    ensure!(
        !is_reserved_root_name(first),
        "`{first}` is a reserved name at the repo root"
    );
    Ok(())
}

/// Whether a root-level name is one dotman never applies. Case-insensitive because APFS and other
/// case-insensitive filesystems resolve `.GIT` to `.git`.
#[must_use]
pub fn is_reserved_root_name(name: &str) -> bool {
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
pub(crate) fn refuse_non_nfc(path: &Path) -> Result<()> {
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
    fn a_git_component_is_refused_at_any_depth() {
        for name in [
            ".config/x/.git/config",
            "a/.GIT",
            "a/git~1/hooks/pre-commit",
            ".git ",
            ".GIT.",
            "a/.git. ./config",
            "a/.Git  ",
            "git~1.",
            ".config/x/.git:stream",
            "a/.git\\y",
            "a/x\\.GIT. ",
            "GIT~1:z",
        ] {
            assert!(validate_key(name).is_err(), "{name}");
        }
        for fine in [
            ".config/x/.github/ci.yml",
            ".config/x/.gitx",
            ".config/x/ .git",
            ".git.d",
            ".gitconfig",
            ".git~",
            "a/x:.git",
        ] {
            assert!(validate_key(fine).is_ok(), "{fine}");
        }
    }

    #[test]
    fn a_control_character_is_refused_in_any_component() {
        for name in [
            "a/b\nc",
            "a\tb",
            "x/\u{1b}[2Jy",
            "del\u{7f}",
            "nul\0",
            "/abs\nnext: run x",
        ] {
            let err = validate_key(name).unwrap_err().to_string();
            assert!(
                !err.chars().any(char::is_control),
                "{name:?}: the error echoes it raw: {err:?}"
            );
        }
        assert!(validate_key("caf\u{e9}/\u{a0}x").is_ok());
    }

    #[test]
    fn a_c1_control_character_is_refused_and_escaped() {
        for (name, shown) in [
            ("x/csi\u{9b}2Jy", "csi\\u{9b}2Jy"),
            ("\u{80}first", "\\u{80}first"),
            ("a/nel\u{85}", "nel\\u{85}"),
            ("a/\u{9f}last", "\\u{9f}last"),
        ] {
            let err = validate_key(name).unwrap_err().to_string();
            assert!(err.contains("control character"), "{name:?}: {err}");
            assert!(err.contains(shown), "{name:?}: {err}");
            assert!(!err.chars().any(char::is_control), "{name:?}: {err:?}");
        }
        assert!(validate_key("\u{a0}nbsp/\u{7e}tilde").is_ok());
    }

    #[test]
    fn a_bidi_control_or_invisible_format_character_is_refused_and_escaped() {
        for (name, shown) in [
            ("a/evil\u{202e}fdp.sh", "evil\\u{202e}fdp.sh"),
            ("x\u{2066}y\u{2069}", "x\\u{2066}y\\u{2069}"),
            ("zw\u{200b}sp", "zw\\u{200b}sp"),
            ("\u{feff}bom", "\\u{feff}bom"),
        ] {
            let err = validate_key(name).unwrap_err().to_string();
            assert!(err.contains(shown), "{name:?}: {err}");
            assert!(
                !err.chars().any(crate::output::is_hidden_format),
                "{name:?}: {err:?}"
            );
        }
        assert!(validate_key("\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}").is_ok());
        assert!(validate_key("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}").is_ok());
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
    fn own_files_are_refused_as_keys() {
        let tmp = TempDir::new().unwrap();
        let rules = KeyRules::new(&resolve(&env_with(tmp.path(), &[])));
        for key in [
            ".local/share/dotman",
            ".local/share/dotman/.bashrc",
            ".local/share/dotman/.dotman.toml",
            ".local/state/dotman/state.toml",
            ".config/dotman/config.toml",
            ".config/dotman/age-identity.txt",
            ".config/dotman/anything",
            ".Config/DotMan/config.toml",
        ] {
            assert!(rules.check(key, true).is_err(), "{key}");
            assert!(rules.check(key, false).is_err(), "{key}");
        }
        for fine in [".bashrc", ".local/share/dotman-other", ".config/dotmanx"] {
            assert!(rules.check(fine, true).is_ok(), "{fine}");
        }
        // A link at an ancestor would move dotman's files; a directory to walk does not.
        for ancestor in [".config", ".local", ".local/state"] {
            assert!(rules.check(ancestor, true).is_err(), "{ancestor}");
            assert!(rules.check(ancestor, false).is_ok(), "{ancestor}");
        }
        assert!(rules.check(".local/share/dotman/a\u{202e}b", true).is_err());

        let key_file = tmp.path().join("keys/age.txt");
        let key_env = env_with(
            tmp.path(),
            &[("DOTMAN_AGE_KEY_FILE", key_file.to_str().unwrap())],
        );
        assert!(
            KeyRules::new(&resolve(&key_env))
                .check("keys/age.txt", true)
                .is_err()
        );
        assert!(rules.check("keys/age.txt", true).is_ok());
        let relative = env_with(tmp.path(), &[("DOTMAN_AGE_KEY_FILE", "age.txt")]);
        assert!(Paths::resolve(&relative, sys::geteuid()).is_err());
    }

    #[test]
    fn own_keys_follow_this_machine_s_xdg_dirs() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let at = |dir: &str| home.join(dir).to_str().unwrap().to_owned();
        let (config, state, data) = (at("cfg"), at("st"), at("dat"));
        let env = env_with(
            home,
            &[
                ("XDG_CONFIG_HOME", &config),
                ("XDG_STATE_HOME", &state),
                ("XDG_DATA_HOME", &data),
            ],
        );
        let rules = KeyRules::new(&resolve(&env));
        for key in [
            "cfg/dotman/config.toml",
            "st/dotman/state.toml",
            "dat/dotman/x",
        ] {
            assert!(rules.check(key, true).is_err(), "{key}");
        }
        for key in [
            ".config/dotman/config.toml",
            ".local/state/dotman/state.toml",
        ] {
            assert!(rules.check(key, true).is_ok(), "{key}");
        }
        let outside = env_with(home, &[("XDG_CONFIG_HOME", "/elsewhere/config")]);
        assert!(
            KeyRules::new(&resolve(&outside))
                .check(".config/dotman/config.toml", true)
                .is_ok()
        );
    }

    #[test]
    fn own_keys_match_the_real_spelling_of_a_symlinked_home_or_dir() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join(".local/share/dotman")).unwrap();
        fs::create_dir_all(real.join("cfg")).unwrap();
        symlink(real.join("cfg"), real.join(".config")).unwrap();
        let link = tmp.path().join("link");
        symlink(&real, &link).unwrap();
        let paths = resolve(&env_with(&link, &[]));
        let rules = KeyRules::new(&paths);
        let home_real = paths.home_real.as_deref();
        let via_real = fs::canonicalize(&real)
            .unwrap()
            .join(".local/share/dotman/.bashrc");
        let key = derive_key(&via_real, &link, &paths.home, home_real).unwrap();
        assert!(rules.check(&key, true).is_err(), "{key}");
        // No config file exists yet, and `.config` is a link to `cfg`.
        assert!(rules.check("cfg/dotman/config.toml", true).is_err());
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
