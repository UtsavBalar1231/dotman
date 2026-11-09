//! System scope: files outside `$HOME`, read and captured as the user.
//!
//! A key is the user-facing absolute path, such as `/etc/keyd/default.conf`, and its source lives
//! at `.dotman/system/etc/keyd/default.conf`. Everything that reuses the home machinery
//! (`Sources`, `read_node`, `Worktree`) works on the relative form, because those reject a key
//! that starts with `/`. Never join the absolute form onto a directory: `Path::join` would discard
//! the directory.
//!
//! Nothing here writes outside the repo and the state dir. `apply --system` writes system paths
//! through `escalate`, which runs [`helper`] as root.

pub(crate) mod escalate;
pub mod helper;

use crate::commands::add::ensure_worktree_parents;
use crate::config::Config;
use crate::context::{Context, is_under};
use crate::engine::status::{Content, Row, SourceEntry, Worktree, classify};
use crate::engine::{Home, Node, Tuple, read_node};
use crate::paths::{Paths, absolute_input, validate_key, write_atomic};
use crate::proc;
use crate::repo_config::{RepoConfig, SystemFile};
use crate::secrets::{self, HoldsKey};
use crate::state::State;
use crate::sys;
use crate::variant::{Flag, Selection, Sources};
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where system sources live in the repo.
pub const DIR: &str = ".dotman/system";

/// Files a package manager leaves next to a configuration file it did not overwrite.
const SIBLINGS: [&str; 6] = [
    ".pacnew",
    ".pacsave",
    ".dpkg-dist",
    ".dpkg-old",
    ".rpmnew",
    ".rpmsave",
];

const NOT_PLAIN: &str = "system scope takes plain files only, no {tmpl} or {age}";
const NOT_A_FILE: &str = "a system source is a regular file, not a symlink";

/// The plan's reason for refusing a file the user cannot read.
pub const SECRETS_NOTE: &str = "a file you cannot read is usually a secret; system scope does not encrypt and the remote may be public. Keep secrets as {age} files under $HOME";

// Linux paths that are never managed. `/` is listed so that a root of `/` is refused, because it
// would admit every path; it does not ban every key.
const NEVER: [&str; 6] = ["/proc", "/sys", "/dev", "/run", "/tmp", "/var/tmp"];
// Lowercase, because the default macOS filesystems fold case. Apple SIP protects these.
const NEVER_MACOS: [&str; 6] = [
    "/system",
    "/bin",
    "/sbin",
    "/var",
    "/private/var",
    "/private/tmp",
];

/// Whether `canonical`, a path in canonical form, may be neither a root nor a key.
#[must_use]
pub fn never_allowed(canonical: &str, macos: bool) -> bool {
    if canonical == "/" {
        return true;
    }
    if NEVER.iter().any(|dir| is_under(canonical, dir)) {
        return true;
    }
    if !macos {
        return false;
    }
    let lower = canonical.to_lowercase();
    NEVER_MACOS.iter().any(|dir| is_under(&lower, dir))
        || (is_under(&lower, "/usr") && !is_under(&lower, "/usr/local"))
}

// On macOS `/etc`, `/var` and `/tmp` are symlinks to `/private/...`. These three are the only
// OS-level aliases the symlink rule exempts; any other symlink in a key is rejected.
fn canonical_form(key: &str, macos: bool) -> String {
    if macos {
        for alias in ["etc", "var", "tmp"] {
            let rest = key
                .strip_prefix('/')
                .and_then(|k| k.strip_prefix(alias))
                .filter(|rest| rest.is_empty() || rest.starts_with('/'));
            if let Some(rest) = rest {
                return format!("/private/{alias}{rest}");
            }
        }
    }
    key.to_owned()
}

/// The part of a key below `/`, validated as a relative key.
pub(crate) fn rel(key: &str) -> Result<&str> {
    let rel = key
        .strip_prefix('/')
        .ok_or_else(|| anyhow!("{key} is not an absolute path"))?;
    validate_key(rel).with_context(|| format!("{key} cannot be a system file"))?;
    Ok(rel)
}

/// The key a source path relative to [`DIR`] targets.
#[must_use]
pub fn key_of(rel: &str) -> String {
    format!("/{rel}")
}

/// The repo path of a source path relative to [`DIR`].
#[must_use]
pub fn source_path(rel: &str) -> String {
    format!("{DIR}/{rel}")
}

/// One entry of `[system] roots`, as configured and in canonical form.
#[derive(Debug, Clone)]
pub struct Root {
    /// The text in the machine config.
    pub shown: String,
    /// The canonical form that keys are compared with.
    pub canon: String,
    /// Why this root is not used, if it is not.
    pub problem: Option<&'static str>,
    /// The directory exists below the prefix.
    pub exists: bool,
}

/// Where a key stands against the roots and the never-allowed list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Inside a root.
    Active,
    /// Outside every root, or there are no roots.
    Off,
    /// Never usable, with the reason.
    Refused(String),
}

/// `DOTMAN_SYSTEM_ROOT` and the configured roots, resolved once.
#[derive(Debug, Clone)]
pub struct Scope {
    prefix: Option<PathBuf>,
    roots: Vec<Root>,
    macos: bool,
}

impl Scope {
    /// The scope of one run.
    #[must_use]
    pub fn new(paths: &Paths, machine: &Config) -> Self {
        Self::build(
            paths.system_root.clone(),
            &machine.system.roots,
            cfg!(target_os = "macos"),
        )
    }

    fn build(prefix: Option<PathBuf>, roots: &[PathBuf], macos: bool) -> Self {
        let base = prefix.as_deref().unwrap_or_else(|| Path::new("/"));
        let roots = roots
            .iter()
            .map(|raw| resolve_root(raw, base, prefix.is_some(), macos))
            .collect();
        Self {
            prefix,
            roots,
            macos,
        }
    }

    /// `DOTMAN_SYSTEM_ROOT`, canonicalized, when set.
    #[must_use]
    pub fn prefix(&self) -> Option<&Path> {
        self.prefix.as_deref()
    }

    /// Every configured root, usable or not.
    #[must_use]
    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Whether the platform is macOS, which has the longer never-allowed list.
    #[must_use]
    pub const fn macos(&self) -> bool {
        self.macos
    }

    fn base(&self) -> &Path {
        self.prefix.as_deref().unwrap_or_else(|| Path::new("/"))
    }

    /// The path a plan names for the key, the one [`Scope::disk`] reads below the prefix: the
    /// canonical form, so `/private/etc/x` for `/etc/x` on macOS, except under a prefix.
    #[must_use]
    pub fn target(&self, key: &str) -> String {
        format!("/{}", self.walk_rel(key))
    }

    // The aliases are real symlinks on macOS, so without a prefix the walk goes through the
    // canonical form. Under a prefix the key is the path below it.
    fn walk_rel(&self, key: &str) -> String {
        let walked = if self.prefix.is_none() {
            canonical_form(key, self.macos)
        } else {
            key.to_owned()
        };
        walked.trim_start_matches('/').to_owned()
    }

    /// Where the key is on disk: below the prefix when one is set.
    #[must_use]
    pub fn disk(&self, key: &str) -> PathBuf {
        self.base().join(self.walk_rel(key))
    }

    /// Checks the key against the never-allowed list and the roots. Refused keys are never read.
    #[must_use]
    pub fn standing(&self, key: &str) -> Standing {
        if let Err(e) = rel(key) {
            return Standing::Refused(format!("{e:#}"));
        }
        let canon = canonical_form(key, self.macos);
        if never_allowed(&canon, self.macos) {
            return Standing::Refused(format!("{key} is on the never-allowed list"));
        }
        let inside = self
            .roots
            .iter()
            .filter(|root| root.problem.is_none())
            .any(|root| is_under(&canon, &root.canon));
        if inside {
            Standing::Active
        } else {
            Standing::Off
        }
    }

    /// What is at the key, read as the user without following a symlink. A symlinked parent below
    /// the prefix shows up as [`Node::BadParent`].
    pub(crate) fn node(&self, key: &str) -> Result<Node> {
        read_node(self.base(), &self.walk_rel(key))
    }

    /// H for a key. Setuid, setgid and sticky bits are masked off, as they are in R.
    pub(crate) fn observe(&self, key: &str) -> Result<Home> {
        Ok(match self.node(key)?.home() {
            Home::Present(mut tuple) if tuple.kind == crate::state::Kind::File => {
                tuple.mode &= 0o777;
                Home::Present(tuple)
            }
            home => home,
        })
    }

    /// The package-manager leftovers that exist beside the key.
    #[must_use]
    pub fn siblings(&self, key: &str) -> Vec<&'static str> {
        let path = self.disk(key).into_os_string();
        SIBLINGS
            .into_iter()
            .filter(|suffix| {
                let mut sibling: OsString = path.clone();
                sibling.push(suffix);
                fs::symlink_metadata(sibling).is_ok()
            })
            .collect()
    }
}

// Lexical cleanup, then the real path when the directory exists, so a root that is a symlink
// compares as the directory it names.
fn resolve_root(raw: &Path, base: &Path, prefixed: bool, macos: bool) -> Root {
    let shown = raw.display().to_string();
    let clean = raw.to_str().and_then(clean_root);
    let Some(clean) = clean else {
        return Root {
            shown: shown.clone(),
            canon: shown,
            problem: Some("is not an absolute path without `.`, `..` or empty parts"),
            exists: false,
        };
    };
    let lexical = canonical_form(&clean, macos);
    let disk = base.join(clean.trim_start_matches('/'));
    let exists = fs::metadata(&disk).is_ok_and(|meta| meta.is_dir());
    let mut problem = None;
    let canon = match fs::canonicalize(&disk) {
        Ok(real) if prefixed => real.strip_prefix(base).map_or_else(
            |_| {
                problem = Some("resolves outside DOTMAN_SYSTEM_ROOT");
                lexical.clone()
            },
            |inside| format!("/{}", inside.display()),
        ),
        Ok(real) => real.to_str().map_or(lexical, str::to_owned),
        Err(_) => lexical,
    };
    if problem.is_none() && never_allowed(&canon, macos) {
        problem = Some("is on the never-allowed list");
    }
    Root {
        shown,
        canon,
        problem,
        exists,
    }
}

fn clean_root(text: &str) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Some("/".to_owned());
    }
    rest.split('/')
        .all(|part| !matches!(part, "" | "." | ".."))
        .then(|| format!("/{rest}"))
}

/// The top-level directory of `key` to offer as a root, such as `/etc`.
#[must_use]
pub fn offered_root(key: &str, macos: bool) -> Option<String> {
    let mut parts = key.strip_prefix('/')?.split('/');
    let (top, deeper) = (parts.next()?, parts.next().is_some());
    if !deeper {
        return None;
    }
    let top_level = format!("/{top}");
    if !never_allowed(&canonical_form(&top_level, macos), macos) {
        return Some(top_level);
    }
    // macOS keeps `/usr/local` usable under a banned `/usr`.
    let mut parts = key.strip_prefix('/')?.split('/');
    let second = format!("/{}/{}", parts.next()?, parts.next()?);
    let usable = parts.next().is_some() && !never_allowed(&canonical_form(&second, macos), macos);
    usable.then_some(second)
}

/// One system key, evaluated.
#[derive(Debug)]
pub struct Evaluated {
    /// The absolute key.
    pub key: String,
    /// Outside every root, so it is shown as `system-off`. It is still read and captured.
    pub off: bool,
    /// What was found.
    pub verdict: Verdict,
}

/// What reading one system key gave.
#[derive(Debug)]
pub enum Verdict {
    /// The key can never be used.
    Refused(String),
    /// A source is a template, encrypted or a symlink, or its block is broken.
    Invalid(Vec<(String, &'static str)>),
    /// Equally specific variants.
    Ambiguous(Vec<String>),
    /// R, H or S could not be read.
    Unreadable(String),
    /// The three tuples gave a row.
    Row {
        /// The row of the three-way model.
        row: Row,
        /// R, when a source applies here.
        r: Option<SourceEntry>,
        /// H as read from the system.
        h: Home,
    },
}

// A hand-written empty mode would fail every read of the key, so it counts as unrecorded.
fn source_modes(sources: &Sources, repo_config: &RepoConfig) -> BTreeMap<String, String> {
    let mut modes = BTreeMap::new();
    for rel in sources.keys() {
        let Some(file) = repo_config.system.get(&key_of(rel)) else {
            continue;
        };
        if file.mode.is_empty() {
            continue;
        }
        for source in sources.sources_of(rel) {
            modes.insert(source.to_owned(), file.mode.clone());
        }
    }
    modes
}

fn select(
    ctx: &Context,
    sources: &Sources,
    worktree: &Worktree<'_>,
    rel: &str,
) -> Result<Selection<SourceEntry>> {
    let not_plain: Vec<(String, &'static str)> = sources
        .sources_of(rel)
        .filter(|source| sources.flag(rel, source) != Flag::Plain)
        .map(|source| (source_path(source), NOT_PLAIN))
        .collect();
    if !not_plain.is_empty() {
        return Ok(Selection::Invalid(not_plain));
    }
    Ok(match sources.select(rel, ctx.identity()) {
        Selection::None => Selection::None,
        Selection::Ambiguous(tied) => {
            Selection::Ambiguous(tied.iter().map(|source| source_path(source)).collect())
        }
        Selection::Invalid(problems) => Selection::Invalid(
            problems
                .into_iter()
                .map(|(source, why)| (source_path(&source), why))
                .collect(),
        ),
        Selection::Winner(source) => match worktree.get(source)? {
            None => Selection::None,
            Some(entry) if matches!(entry.content, Content::Link(_)) => {
                Selection::Invalid(vec![(source_path(source), NOT_A_FILE)])
            }
            Some(mut entry) => {
                entry.source = source_path(source);
                Selection::Winner(entry)
            }
        },
    })
}

/// Evaluates every system key that has a source or a state entry. Reads the system as the user
/// and writes nothing.
pub(crate) fn evaluate(
    ctx: &Context,
    repo_config: &RepoConfig,
    state: &State,
) -> Result<Vec<Evaluated>> {
    let scope = ctx.system_scope();
    let sources = ctx.system_sources()?;
    let modes = source_modes(&sources, repo_config);
    let root = ctx.paths.repo.join(DIR);
    let worktree = Worktree::new(&root, &modes);
    let keys: BTreeSet<String> = sources
        .keys()
        .map(key_of)
        .chain(state.system.keys().cloned())
        .collect();
    let mut found = Vec::new();
    for key in keys {
        let standing = scope.standing(&key);
        let off = standing == Standing::Off;
        let verdict = match standing {
            Standing::Refused(why) => Some(Verdict::Refused(why)),
            Standing::Active | Standing::Off => {
                row_of(ctx, &scope, &sources, &worktree, state, &key)
                    .unwrap_or_else(|e| Some(Verdict::Unreadable(format!("{e:#}"))))
            }
        };
        if let Some(verdict) = verdict {
            found.push(Evaluated { key, off, verdict });
        }
    }
    Ok(found)
}

// `None` when neither R nor S exists, such as a variant meant for another machine.
fn row_of(
    ctx: &Context,
    scope: &Scope,
    sources: &Sources,
    worktree: &Worktree<'_>,
    state: &State,
    key: &str,
) -> Result<Option<Verdict>> {
    let rel = rel(key)?;
    let r = match select(ctx, sources, worktree, rel)? {
        Selection::None => None,
        Selection::Winner(entry) => Some(entry),
        Selection::Ambiguous(tied) => return Ok(Some(Verdict::Ambiguous(tied))),
        Selection::Invalid(problems) => return Ok(Some(Verdict::Invalid(problems))),
    };
    let recorded = state.system.get(key);
    let s = recorded.map(Tuple::from_entry).transpose()?;
    if r.is_none() && s.is_none() {
        return Ok(None);
    }
    let h = scope.observe(key)?;
    let Some(row) = classify(r.as_ref().map(|r| &r.tuple), &h, s.as_ref()) else {
        return Ok(None);
    };
    let winner = r.as_ref().map(|r| r.source.as_str());
    let switched = recorded.is_some_and(|s| Some(s.source.as_str()) != winner);
    let row = if switched { row.switched() } else { row };
    Ok(Some(Verdict::Row { row, r, h }))
}

/// Rows where `apply --system` would write.
#[must_use]
pub const fn needs_write(row: Row) -> bool {
    matches!(
        row,
        Row::NotApplied
            | Row::Existing
            | Row::ChangedInRepo
            | Row::BothChanged
            | Row::DeletedLocally
            | Row::DeletedChangedUpstream
    )
}

/// The file mode a source is stored with: git keeps only the owner execute bit.
#[must_use]
pub const fn stored_mode(mode: u32) -> u32 {
    if mode & 0o111 == 0 { 0o644 } else { 0o755 }
}

/// Records owner and group by name, and the mode without setuid, setgid and sticky, for `key`.
/// `package_owned` stays as it was.
pub(crate) fn record(
    repo_config: &mut RepoConfig,
    key: &str,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<()> {
    let owner = sys::passwd_by_uid(uid)
        .ok_or_else(|| anyhow!("uid {uid} has no passwd entry, so its name cannot be recorded"))?
        .name;
    let group = sys::group_by_gid(gid)
        .ok_or_else(|| anyhow!("gid {gid} has no group entry, so its name cannot be recorded"))?
        .name;
    let package_owned = repo_config
        .system
        .get(key)
        .is_some_and(|file| file.package_owned);
    repo_config.system.insert(
        key.to_owned(),
        SystemFile {
            owner,
            group,
            mode: format!("{:04o}", mode & 0o777),
            package_owned,
        },
    );
    Ok(())
}

/// Copies the system file at `key` into the repo at `source`, a path under [`DIR`].
///
/// Updates the `[system]` record and fails with [`HoldsKey`] for a file holding an age identity.
/// A dry run reads and checks but writes nothing. The caller records S and saves `repo_config`.
pub(crate) fn capture(
    ctx: &Context,
    key: &str,
    source: &str,
    repo_config: &mut RepoConfig,
    dry_run: bool,
) -> Result<Tuple> {
    let Node::File {
        bytes,
        mode,
        uid,
        gid,
        ..
    } = ctx.system_scope().node(key)?
    else {
        bail!("it changed while being captured");
    };
    if secrets::holds_secret_key(&bytes) {
        return Err(HoldsKey.into());
    }
    let mode = mode & 0o777;
    if !dry_run {
        ensure_worktree_parents(&ctx.paths.repo, source)?;
        write_atomic(&ctx.paths.repo.join(source), &bytes, stored_mode(mode))?;
        record(repo_config, key, uid, gid, mode)?;
    }
    Ok(Tuple::of_file(&bytes, mode))
}

/// Whether an error is the user lacking permission to read.
#[must_use]
pub fn is_unreadable(error: &anyhow::Error) -> bool {
    error
        .root_cause()
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
}

/// Whether a package manager owns a file.
#[derive(Debug, PartialEq, Eq)]
pub enum Ownership {
    /// The first line the checker printed.
    Package(String),
    /// A checker ran and found no owner.
    Unowned,
    /// None of `pacman`, `dpkg` and `rpm` is on `PATH`.
    NoChecker,
}

/// Asks the first available of `pacman -Qo`, `dpkg -S` and `rpm -qf` about `path`.
pub(crate) fn package_owner(path: &Path) -> Result<Ownership> {
    let checkers: [(&str, &[&str]); 3] =
        [("pacman", &["-Qo"]), ("dpkg", &["-S"]), ("rpm", &["-qf"])];
    for (name, args) in checkers {
        let Some(program) = proc::find_program(name) else {
            continue;
        };
        let out = proc::command(&program)
            .args(args)
            .arg(path)
            .output()
            .with_context(|| format!("cannot run {}", program.display()))?;
        if !out.status.success() {
            return Ok(Ownership::Unowned);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().next().unwrap_or_default().trim();
        return Ok(Ownership::Package(if line.is_empty() {
            name.to_owned()
        } else {
            line.to_owned()
        }));
    }
    Ok(Ownership::NoChecker)
}

/// The inputs that are not under `$HOME`, as system filters, and the rest, which stay paths.
pub(crate) fn split_inputs(
    paths: &Paths,
    inputs: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let (mut home, mut system) = (Vec::new(), Vec::new());
    for input in inputs {
        let abs = absolute_input(input, &cwd, &paths.home);
        let under_home = std::iter::once(paths.home.as_path())
            .chain(paths.home_real.as_deref())
            .any(|root| abs.starts_with(root));
        if under_home {
            home.push(input.clone());
        } else {
            let text = abs
                .to_str()
                .ok_or_else(|| anyhow!("{} is not valid UTF-8", input.display()))?;
            rel(text)?;
            system.push(text.to_owned());
        }
    }
    Ok((home, system))
}

/// The keys under the system `filters`, or all of them when there are no filters. A filter that
/// matches nothing is an error.
pub(crate) fn select_keys(filters: &[String], keys: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    if filters.is_empty() {
        return Ok(keys.clone());
    }
    let mut selected = BTreeSet::new();
    for filter in filters {
        let under: Vec<&String> = keys.iter().filter(|key| is_under(key, filter)).collect();
        if under.is_empty() {
            bail!("{filter} is not managed");
        }
        selected.extend(under.into_iter().cloned());
    }
    Ok(selected)
}

/// The note about leftovers beside `key`.
#[must_use]
pub fn sibling_note(key: &str, found: &[&str]) -> String {
    let names: Vec<String> = found
        .iter()
        .map(|suffix| format!("{key}{suffix}"))
        .collect();
    format!(
        "{key}: the package manager left {}; compare it with the managed file and merge or remove it",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn the_never_allowed_list_bans_paths_and_a_root_of_slash() {
        for banned in [
            "/",
            "/proc",
            "/proc/1/x",
            "/sys/x",
            "/dev/null",
            "/run/x",
            "/tmp/x",
            "/var/tmp/y",
        ] {
            assert!(never_allowed(banned, false), "{banned}");
        }
        for fine in [
            "/etc/x",
            "/usr/lib/x",
            "/var/lib/x",
            "/opt/x",
            "/tmpx/y",
            "/procs",
        ] {
            assert!(!never_allowed(fine, false), "{fine}");
        }
        for banned in [
            "/System/x",
            "/bin/sh",
            "/sbin/x",
            "/var/x",
            "/private/var/x",
            "/private/tmp/x",
            "/usr/bin/x",
            "/usr",
        ] {
            assert!(never_allowed(banned, true), "{banned}");
        }
        for fine in [
            "/private/etc/x",
            "/usr/local/etc/x",
            "/usr/local",
            "/Library/x",
            "/opt/x",
        ] {
            assert!(!never_allowed(fine, true), "{fine}");
        }
    }

    #[test]
    fn only_the_three_aliases_change_on_macos() {
        assert_eq!(canonical_form("/etc/hosts", true), "/private/etc/hosts");
        assert_eq!(canonical_form("/var/db/x", true), "/private/var/db/x");
        assert_eq!(canonical_form("/tmp", true), "/private/tmp");
        assert_eq!(canonical_form("/etcetera/x", true), "/etcetera/x");
        assert_eq!(canonical_form("/usr/x", true), "/usr/x");
        assert_eq!(canonical_form("/etc/hosts", false), "/etc/hosts");
    }

    #[test]
    fn roots_are_cleaned_and_judged() {
        assert_eq!(clean_root("/etc/"), Some("/etc".to_owned()));
        assert_eq!(clean_root("/"), Some("/".to_owned()));
        for bad in ["etc", "/etc/../x", "/etc//x", "/./etc", ""] {
            assert_eq!(clean_root(bad), None, "{bad}");
        }
        let tmp = TempDir::new().unwrap();
        let prefix = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(prefix.join("etc")).unwrap();
        let roots = [
            PathBuf::from("/etc"),
            PathBuf::from("/"),
            PathBuf::from("/tmp"),
            PathBuf::from("rel"),
            PathBuf::from("/missing"),
        ];
        let scope = Scope::build(Some(prefix), &roots, false);
        let problems: Vec<Option<&str>> = scope.roots().iter().map(|r| r.problem).collect();
        let [etc, slash, tmp, relative, missing] = problems.as_slice() else {
            unreachable!("five roots");
        };
        assert_eq!(*etc, None);
        assert!(scope.roots().first().is_some_and(|r| r.exists));
        assert!([slash, tmp, relative].iter().all(|p| p.is_some()));
        assert_eq!(*missing, None);
        assert!(scope.roots().get(4).is_some_and(|r| !r.exists));
        assert_eq!(scope.standing("/etc/keyd/x"), Standing::Active);
        assert_eq!(scope.standing("/opt/x"), Standing::Off);
        assert!(matches!(scope.standing("/tmp/x"), Standing::Refused(_)));
        assert!(matches!(scope.standing("etc/x"), Standing::Refused(_)));
        assert!(matches!(scope.standing("/etc/../x"), Standing::Refused(_)));
    }

    #[test]
    fn no_roots_means_every_key_is_off() {
        let scope = Scope::build(None, &[], false);
        assert_eq!(scope.standing("/etc/x"), Standing::Off);
    }

    #[test]
    fn a_symlink_above_the_prefix_is_ignored_and_one_below_is_not_followed() {
        let tmp = TempDir::new().unwrap();
        let real = fs::canonicalize(tmp.path()).unwrap().join("real");
        fs::create_dir_all(real.join("etc")).unwrap();
        fs::write(real.join("etc/x"), b"x").unwrap();
        symlink(&real, real.parent().unwrap().join("link")).unwrap();
        // The test supplies the already canonical prefix, as `Paths::resolve` does.
        let scope = Scope::build(Some(real.clone()), &[PathBuf::from("/etc")], false);
        assert!(matches!(scope.observe("/etc/x").unwrap(), Home::Present(_)));

        fs::create_dir_all(real.join("elsewhere")).unwrap();
        fs::write(real.join("elsewhere/y"), b"y").unwrap();
        symlink(real.join("elsewhere"), real.join("etc/sub")).unwrap();
        assert!(matches!(
            scope.observe("/etc/sub/y").unwrap(),
            Home::Blocked(crate::engine::Obstacle::Parent(_))
        ));
    }

    #[test]
    fn the_offered_root_is_the_top_level_directory() {
        assert_eq!(
            offered_root("/etc/keyd/default.conf", false).as_deref(),
            Some("/etc")
        );
        assert_eq!(offered_root("/etc/x", false).as_deref(), Some("/etc"));
        assert_eq!(offered_root("/x", false), None);
        assert_eq!(
            offered_root("/usr/local/etc/x", true).as_deref(),
            Some("/usr/local")
        );
        assert_eq!(offered_root("/usr/local/x", false).as_deref(), Some("/usr"));
    }

    #[test]
    fn siblings_are_found_beside_a_key() {
        let tmp = TempDir::new().unwrap();
        let prefix = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(prefix.join("etc")).unwrap();
        for name in ["pacman.conf", "pacman.conf.pacnew", "pacman.conf.rpmsave"] {
            fs::write(prefix.join("etc").join(name), b"").unwrap();
        }
        let scope = Scope::build(Some(prefix), &[], false);
        assert_eq!(scope.siblings("/etc/pacman.conf"), [".pacnew", ".rpmsave"]);
        assert_eq!(scope.siblings("/etc/other"), Vec::<&str>::new());
    }

    #[test]
    fn filters_select_keys_under_them() {
        let keys: BTreeSet<String> = ["/etc/a/x", "/etc/b", "/opt/c"].map(str::to_owned).into();
        let picked = select_keys(&["/etc".to_owned()], &keys).unwrap();
        assert_eq!(picked.len(), 2);
        assert!(select_keys(&["/etc/zzz".to_owned()], &keys).is_err());
        assert_eq!(select_keys(&[], &keys).unwrap(), keys);
    }
}
