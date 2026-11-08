//! The symlink-farm importer and `dotman import --swap`.
//!
//! A farm is a directory of dotfiles that Stow, dotbot, rcm or a script links into `$HOME`. The
//! importer copies what `$HOME` shows through those links and changes nothing: not `$HOME`, not
//! the farm, and no farm script runs. `--swap` later replaces the links with real files.
//!
//! The accounting unit is a link in `$HOME` (within depth 4) whose first hop lies under the farm
//! root, or a file below a folded directory link. Each one is converted or reported. Links that
//! point somewhere else are not entries, except links into the Nix store, which are always
//! reported.

use super::gittree::{Staging, check_target, install_keeping_origin, is_git_dir};
use super::{
    EditedConfig, Item, Report, author_email, chezmoi, count, proceed, refuse_overlap,
    require_fresh, resolved,
};
use crate::cli::ImportArgs;
use crate::commands::Outcome;
use crate::commands::add::{
    BLOCK_NAME, DANGLING, DirPlan, Hold, Ignore, Leaf, ensure_worktree_parents, hold_by_content,
    hold_by_name_and_size, preview, record_mode, secret_patterns,
};
use crate::context::{Context, TEMP_PREFIX, is_real_dir, is_under};
use crate::engine::status::{Content as RContent, Source as _, SourceEntry, Variants, Worktree};
use crate::engine::write::{
    Member, Restored, backup_run_dir, record_link, replace_link_with_dir, replace_link_with_file,
    restore_dir_link, restore_file_link,
};
use crate::engine::{parents, portable_link_target};
use crate::git::Git;
use crate::name;
use crate::output;
use crate::paths::{find_collisions, refuse_non_nfc, validate_key, write_atomic};
use crate::repo_config::RepoConfig;
use crate::state::Entry;
use crate::sys;
use crate::variant::{self, Selection};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use glob::Pattern;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

/// Roots under these are system files, not somebody's dotfiles. A root below `$HOME` is exempt:
/// `$HOME` itself may live under one of them.
const SYSTEM_PREFIXES: [&str; 13] = [
    "/usr",
    "/etc",
    "/opt",
    "/var",
    "/nix",
    "/snap",
    "/run",
    "/proc",
    "/sys",
    "/tmp",
    "/Applications",
    "/System",
    "/Library",
];
const DEPTH: usize = 4;
/// A home with more entries than this within `DEPTH` is not searched further.
const MAX_VISITED: usize = 200_000;
const MAX_ENTRIES: usize = 100_000;
/// File content over this much, in total, is refused, so a hostile farm cannot exhaust memory.
const MAX_BYTES: u64 = 268_435_456;
const READ_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK;

const NIX: &str = "links into the Nix store, which is read-only: they are never imported";
const DANGLING_LINK: &str = "dangling links";
const OUTSIDE: &str = "links that resolve outside the farm";
const ODD: &str = "links to something that is neither a regular file nor a directory";
const HELD: &str =
    "files held back, as `{bin} add` holds them: check each, then `{bin} add` the ones you want";
const UNREADABLE: &str = "files that could not be read";
const UNMANAGEABLE: &str = "paths dotman cannot manage";
const CLASH: &str = "paths that clash with another path of the import";
const DIFFERS: &str = "links whose content differs from the repo, left in place: bring the repo's version and the linked file in line, then swap again";
const NOT_FILE: &str = "links that do not resolve to a regular file";

/// A symlink in `$HOME`.
struct Link {
    /// Relative to `$HOME`.
    rel: PathBuf,
    /// Under the canonical `$HOME`, so it compares with canonical paths.
    real: PathBuf,
    /// The link's own target text.
    raw: PathBuf,
}

impl Link {
    /// Where the first hop lands, resolved against the link's own directory.
    fn first_hop(&self) -> PathBuf {
        let target = if self.raw.is_absolute() {
            self.raw.clone()
        } else {
            self.real
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(&self.raw)
        };
        normalize(&target)
    }

    fn shown(&self) -> String {
        format!("{} -> {}", self.rel.display(), self.raw.display())
    }
}

/// Removes `.` and `..` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn real_home(ctx: &Context) -> PathBuf {
    fs::canonicalize(&ctx.paths.home).unwrap_or_else(|_| ctx.paths.home.clone())
}

/// `/nix/store`, and `$DOTMAN_NIX_STORE` when tests point it somewhere else; both as written and
/// as resolved.
fn nix_stores() -> Vec<PathBuf> {
    let mut stores = vec![PathBuf::from("/nix/store")];
    stores.extend(
        std::env::var_os("DOTMAN_NIX_STORE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    );
    let real: Vec<PathBuf> = stores
        .iter()
        .filter_map(|store| fs::canonicalize(store).ok())
        .collect();
    stores.extend(real);
    stores
}

fn in_stores(path: &Path, stores: &[PathBuf]) -> bool {
    stores.iter().any(|store| path.starts_with(store))
}

/// Directories of the other importers, dotman's own, and whatever else must not be searched.
fn skipped_dirs(ctx: &Context) -> Vec<PathBuf> {
    let paths = &ctx.paths;
    let mut dirs = vec![
        paths.repo.clone(),
        paths.state_dir.clone(),
        paths.home.join(".dotman"),
        paths.data_home.join("yadm"),
        paths.home.join(".local/share/yadm"),
        paths.config_home.join("vcsh"),
        paths.config_home.join("dotman"),
    ];
    dirs.extend(chezmoi::candidates(ctx));
    let real: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| fs::canonicalize(dir).ok())
        .collect();
    dirs.extend(real);
    dirs
}

/// Every symlink in `$HOME` within `DEPTH`. Symlinked directories are not entered.
fn home_links(ctx: &Context) -> Vec<Link> {
    let home_real = real_home(ctx);
    let skip = skipped_dirs(ctx);
    let walk = WalkDir::new(&ctx.paths.home)
        .follow_links(false)
        .min_depth(1)
        .max_depth(DEPTH)
        .into_iter()
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && (entry.file_name() == ".git"
                    || skip.iter().any(|dir| entry.path() == dir)
                    || is_git_dir(entry.path())))
        });
    let mut links = Vec::new();
    for (seen, entry) in walk.filter_map(std::result::Result::ok).enumerate() {
        if seen >= MAX_VISITED {
            break;
        }
        if !entry.file_type().is_symlink() {
            continue;
        }
        let (Ok(rel), Ok(raw)) = (
            entry.path().strip_prefix(&ctx.paths.home),
            fs::read_link(entry.path()),
        ) else {
            continue;
        };
        links.push(Link {
            real: home_real.join(rel),
            rel: rel.to_path_buf(),
            raw,
        });
    }
    links
}

/// What a link in `$HOME` is, relative to one farm root.
enum Fate {
    /// Resolves to a regular file under the root.
    File(PathBuf),
    /// Resolves to a directory under the root: a folded link.
    Dir(PathBuf),
    Nix,
    Dangling,
    /// Starts under the root and ends elsewhere.
    Outside(PathBuf),
    /// Resolves to a socket, FIFO or device under the root.
    Odd,
}

/// `None` for a link that has nothing to do with `root`.
fn fate(link: &Link, root: &Path, stores: &[PathBuf]) -> Option<Fate> {
    let first_hop = link.first_hop();
    let resolved = fs::canonicalize(&link.real);
    if in_stores(&first_hop, stores) || resolved.as_ref().is_ok_and(|real| in_stores(real, stores))
    {
        return Some(Fate::Nix);
    }
    let starts_under = first_hop.starts_with(root);
    match resolved {
        Ok(real) if real.starts_with(root) && real != root => {
            let meta = fs::symlink_metadata(&real).ok()?;
            Some(if meta.is_dir() {
                Fate::Dir(real)
            } else if meta.is_file() {
                Fate::File(real)
            } else {
                Fate::Odd
            })
        }
        Ok(real) => starts_under.then_some(Fate::Outside(real)),
        Err(_) => starts_under.then_some(Fate::Dangling),
    }
}

/// The first path components below `$HOME`, or the first two of a path elsewhere.
fn anchor(target: &Path, home_real: &Path) -> PathBuf {
    let keep = if target.starts_with(home_real) {
        home_real.components().count().saturating_add(1)
    } else {
        3
    };
    target.components().take(keep).collect()
}

/// The longest directory that is a prefix of every path, one level up when it is a target itself.
fn common_dir(targets: &[&Path]) -> PathBuf {
    let mut iter = targets.iter();
    let Some(first) = iter.next() else {
        return PathBuf::new();
    };
    let mut prefix: Vec<Component<'_>> = first.components().collect();
    for target in iter {
        let same = prefix
            .iter()
            .zip(target.components())
            .take_while(|(a, b)| *a == b)
            .count();
        prefix.truncate(same);
    }
    let root: PathBuf = prefix.iter().collect();
    if targets.iter().any(|target| **target == root) {
        root.parent().map(Path::to_path_buf).unwrap_or_default()
    } else {
        root
    }
}

/// Why a root cannot be a farm, if it cannot. `explicit` roots are not checked for ownership.
fn refusal(ctx: &Context, root: &Path, stores: &[PathBuf], explicit: bool) -> Option<String> {
    let home_real = real_home(ctx);
    if root == home_real || home_real.starts_with(root) {
        return Some("it is, or contains, $HOME".to_owned());
    }
    if !in_stores(root, stores)
        && !root.starts_with(&home_real)
        && let Some(prefix) = SYSTEM_PREFIXES
            .iter()
            .find(|prefix| root.starts_with(prefix))
    {
        return Some(format!("it lies under {prefix}, a system directory"));
    }
    let xdg = [
        ctx.paths.config_home.clone(),
        ctx.paths.data_home.clone(),
        ctx.paths
            .state_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        ctx.paths.home.join(".cache"),
        ctx.paths.home.join(".config"),
        ctx.paths.home.join(".local/share"),
        ctx.paths.home.join(".local/state"),
    ];
    if xdg
        .iter()
        .any(|dir| resolved(dir).starts_with(root) || dir.starts_with(root))
    {
        return Some("it contains a base directory such as ~/.config".to_owned());
    }
    if refuse_overlap(&ctx.paths.repo, root).is_err()
        || resolved(&ctx.paths.state_dir).starts_with(root)
    {
        return Some("it is, contains or lies inside the dotman repo or state".to_owned());
    }
    if skipped_dirs(ctx).iter().any(|dir| root.starts_with(dir)) {
        return Some("it belongs to another importer".to_owned());
    }
    if !explicit {
        let owned = fs::symlink_metadata(root).is_ok_and(|meta| meta.uid() == sys::geteuid());
        if !owned {
            return Some("it is not owned by you".to_owned());
        }
    }
    None
}

/// Farm roots found by looking at `$HOME`: directories under which at least two of its links
/// resolve.
pub(super) fn candidates(ctx: &Context) -> Vec<PathBuf> {
    let stores = nix_stores();
    let home_real = real_home(ctx);
    let mut groups: BTreeMap<PathBuf, Vec<(PathBuf, PathBuf)>> = BTreeMap::new();
    for link in home_links(ctx) {
        let Ok(target) = fs::canonicalize(&link.real) else {
            continue;
        };
        if in_stores(&target, &stores) || in_stores(&link.first_hop(), &stores) {
            continue;
        }
        groups
            .entry(anchor(&target, &home_real))
            .or_default()
            .push((link.real, target));
    }
    let mut roots = Vec::new();
    for members in groups.into_values() {
        let targets: Vec<&Path> = members.iter().map(|(_, target)| target.as_path()).collect();
        let root = common_dir(&targets);
        if !is_real_dir(&root) || refusal(ctx, &root, &stores, false).is_some() {
            continue;
        }
        // A link inside the root is part of the farm, not a deployment of it.
        let deployed: BTreeSet<&Path> = members
            .iter()
            .filter(|(link, target)| !link.starts_with(&root) && *target != root)
            .map(|(_, target)| target.as_path())
            .collect();
        if deployed.len() >= 2 && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots.sort();
    roots
}

/// Whether `path` is a directory that at least one link in `$HOME` points into. Links into the
/// Nix store count only when `path` is itself in the store.
pub(super) fn is_source(ctx: &Context, path: &Path) -> bool {
    let Ok(root) = fs::canonicalize(path) else {
        return false;
    };
    if !is_real_dir(&root) {
        return false;
    }
    let stores = nix_stores();
    let nix_root = in_stores(&root, &stores);
    home_links(ctx).iter().any(|link| {
        !link.real.starts_with(&root)
            && match fate(link, &root, &stores) {
                Some(Fate::Nix) => nix_root && link.first_hop().starts_with(&root),
                other => other.is_some(),
            }
    })
}

/// A folded directory's link text for `key`: kept when it stays inside the directory, which has
/// the same layout when deployed, otherwise made portable the way `add` does it.
fn link_text(raw: &Path, dir: &Path, root: &Path, key: &str, ctx: &Context) -> Option<String> {
    let absolute = normalize(&if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        dir.join(raw)
    });
    let text = if raw.is_relative() && absolute.starts_with(root) {
        raw.to_path_buf()
    } else {
        portable_link_target(
            &absolute,
            key,
            &ctx.paths.home,
            ctx.paths.home_real.as_deref(),
        )
    };
    text.to_str().map(str::to_owned)
}

enum Body {
    Bytes(Vec<u8>),
    Link(String),
}

struct Write {
    key: String,
    body: Body,
    mode: u32,
}

/// What became of one file, for the directory preview.
enum Taken {
    Kept(Leaf),
    Held(Hold),
    Failed,
}

struct Planner<'a> {
    ctx: &'a Context,
    secrets: Vec<Pattern>,
    ignore: Ignore,
    writes: Vec<Write>,
    items: Vec<Item>,
    previews: Vec<DirPlan>,
    /// Folded links and the directories they point to.
    folds: Vec<(String, PathBuf)>,
    ignored: usize,
    bytes: u64,
    too_big: bool,
}

impl Planner<'_> {
    fn item(&mut self, reason: &'static str, detail: String) {
        self.items.push(Item { reason, detail });
    }

    fn link(&mut self, link: &Link, fate: Fate) {
        let shown = link.shown();
        match fate {
            Fate::Nix => self.item(NIX, shown),
            Fate::Dangling => self.item(DANGLING_LINK, shown),
            Fate::Outside(to) => {
                self.item(OUTSIDE, format!("{shown} (resolves to {})", to.display()));
            }
            Fate::Odd => self.item(ODD, shown),
            Fate::File(real) => match link_key(link, false) {
                Ok(key) => {
                    self.file(&key, &real);
                }
                Err(e) => self.item(UNMANAGEABLE, format!("{}: {e:#}", link.rel.display())),
            },
            Fate::Dir(real) => match link_key(link, true) {
                Ok(key) => self.dir(&key, &real),
                Err(e) => self.item(UNMANAGEABLE, format!("{}: {e:#}", link.rel.display())),
            },
        }
    }

    /// Reads the file at `path` for `key`, applying `add`'s guards.
    fn file(&mut self, key: &str, path: &Path) -> Taken {
        match self.read(key, path) {
            Ok(Ok((bytes, mode))) => {
                let leaf = Leaf::File {
                    mode,
                    size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    binary: false,
                };
                self.writes.push(Write {
                    key: key.to_owned(),
                    body: Body::Bytes(bytes),
                    mode,
                });
                Taken::Kept(leaf)
            }
            Ok(Err(hold)) => {
                self.item(HELD, format!("{key}: {}", hold.reason()));
                Taken::Held(hold)
            }
            Err(e) => {
                self.item(UNREADABLE, format!("{key}: {e:#}"));
                Taken::Failed
            }
        }
    }

    fn read(
        &mut self,
        key: &str,
        path: &Path,
    ) -> Result<std::result::Result<(Vec<u8>, u32), Hold>> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(READ_FLAGS)
            .open(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "it is not a regular file");
        if let Some(hold) = hold_by_name_and_size(&self.secrets, key, meta.len()) {
            return Ok(Err(hold));
        }
        self.bytes = self.bytes.saturating_add(meta.len());
        if self.bytes > MAX_BYTES {
            self.too_big = true;
            return Ok(Err(Hold::Large));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(hold_by_content(&bytes).map_or_else(|| Ok((bytes, meta.mode() & 0o777)), Err))
    }

    /// Everything below a folded link becomes a key below the link's path.
    fn dir(&mut self, key: &str, real: &Path) {
        let mut plan = DirPlan {
            key: key.to_owned(),
            ..DirPlan::default()
        };
        let before = self.writes.len();
        let mut walk = WalkDir::new(real)
            .follow_links(false)
            .min_depth(1)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walk.next() {
            if self.writes.len().saturating_add(self.items.len()) > MAX_ENTRIES {
                self.too_big = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    self.item(UNREADABLE, format!("{key}: {e}"));
                    continue;
                }
            };
            let is_dir = entry.file_type().is_dir();
            let name = entry.file_name().to_string_lossy();
            let hidden = name.starts_with(TEMP_PREFIX) || name == ".git";
            let sub = entry
                .path()
                .strip_prefix(real)
                .map_err(|e| anyhow!(e))
                .and_then(|rel| sub_key(key, rel));
            let sub = match sub {
                Ok(sub) if !hidden => sub,
                Ok(_) => {
                    if is_dir {
                        walk.skip_current_dir();
                    }
                    continue;
                }
                Err(e) => {
                    self.item(UNMANAGEABLE, format!("{key}/{name}: {e:#}"));
                    if is_dir {
                        walk.skip_current_dir();
                    }
                    continue;
                }
            };
            if self.ignore.matches(&sub, is_dir) {
                self.ignored = self.ignored.saturating_add(1);
                if is_dir {
                    walk.skip_current_dir();
                }
                continue;
            }
            if !is_dir {
                self.member(&mut plan, &sub, &entry, real);
            }
        }
        if self.writes.len() > before {
            self.folds.push((key.to_owned(), real.to_path_buf()));
        }
        self.previews.push(plan);
    }

    fn member(&mut self, plan: &mut DirPlan, sub: &str, entry: &walkdir::DirEntry, real: &Path) {
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            self.member_link(plan, sub, entry.path(), real);
        } else if file_type.is_file() && entry.file_name().to_str().is_some_and(variant::is_literal)
        {
            match self.file(sub, entry.path()) {
                Taken::Kept(leaf) => {
                    plan.items.insert(sub.to_owned(), leaf);
                }
                Taken::Held(hold) => plan.skipped.push(format!("{sub}: held, {}", hold.reason())),
                Taken::Failed => plan.skipped.push(format!("{sub}: unreadable")),
            }
        } else if file_type.is_file() {
            plan.skipped.push(format!("{sub}: {BLOCK_NAME}"));
            self.item(UNMANAGEABLE, format!("{sub}: {BLOCK_NAME}"));
        } else {
            plan.skipped.push(format!("{sub}: socket, FIFO or device"));
            self.item(ODD, sub.to_owned());
        }
    }

    fn member_link(&mut self, plan: &mut DirPlan, sub: &str, path: &Path, real: &Path) {
        let Ok(raw) = fs::read_link(path) else {
            self.item(UNREADABLE, sub.to_owned());
            return;
        };
        if fs::metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            plan.skipped.push(format!("{sub}: {DANGLING}"));
            self.item(DANGLING_LINK, format!("{sub} -> {}", raw.display()));
            return;
        }
        if hold_by_name_and_size(&self.secrets, sub, 0).is_some() {
            plan.skipped
                .push(format!("{sub}: held, {}", Hold::SecretName.reason()));
            self.item(HELD, format!("{sub}: {}", Hold::SecretName.reason()));
            return;
        }
        let dir = path.parent().unwrap_or(real);
        let Some(text) = link_text(&raw, dir, real, sub, self.ctx) else {
            self.item(ODD, format!("{sub}: the link text is not valid UTF-8"));
            return;
        };
        plan.items.insert(sub.to_owned(), Leaf::Link);
        self.writes.push(Write {
            key: sub.to_owned(),
            body: Body::Link(text),
            mode: 0o777,
        });
    }

    /// Case and Unicode clashes, and keys below a key that is a file, become items.
    fn settle(&mut self) {
        let keys: BTreeSet<&str> = self.writes.iter().map(|write| write.key.as_str()).collect();
        let mut clashing: BTreeMap<String, String> = find_collisions(keys.iter().copied())
            .into_iter()
            .map(|(first, other)| {
                (
                    other,
                    format!("differs from {first} only in case or Unicode form"),
                )
            })
            .collect();
        for key in &keys {
            if parents(key).any(|parent| keys.contains(parent)) {
                clashing.insert(
                    (*key).to_owned(),
                    "a parent of it is also a file".to_owned(),
                );
            }
        }
        let Self { writes, items, .. } = self;
        writes.retain(|write| {
            let Some(why) = clashing.get(&write.key) else {
                return true;
            };
            items.push(Item {
                reason: CLASH,
                detail: format!("{}: {why}", write.key),
            });
            false
        });
    }
}

/// The key of a link: valid, NFC, and not the name of a variant block.
fn link_key(link: &Link, dir: bool) -> Result<String> {
    refuse_non_nfc(&link.rel)?;
    let key = link
        .rel
        .to_str()
        .ok_or_else(|| anyhow!("the name is not valid UTF-8"))?;
    validate_key(key)?;
    ensure!(
        dir || link
            .rel
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(variant::is_literal),
        "{BLOCK_NAME}"
    );
    Ok(key.to_owned())
}

fn sub_key(key: &str, rel: &Path) -> Result<String> {
    refuse_non_nfc(rel)?;
    let rel = rel
        .to_str()
        .ok_or_else(|| anyhow!("the name is not valid UTF-8"))?;
    let sub = format!("{key}/{rel}");
    validate_key(&sub)?;
    Ok(sub)
}

/// Files that other tools keep in a farm and that dotman reads for nothing.
fn tool_notes(root: &Path) -> Vec<String> {
    const FILES: [(&str, &str); 9] = [
        ("install.conf.yaml", "dotbot (`if:` and `shell:` entries)"),
        ("install.conf.yml", "dotbot (`if:` and `shell:` entries)"),
        ("install", "the dotbot or install script"),
        ("rcrc", "rcm"),
        (".rcrc", "rcm"),
        ("hooks", "rcm hooks"),
        ("Hooks", "tuckr hooks"),
        (".stow-local-ignore", "Stow"),
        (".dotter", "dotter"),
    ];
    let mut found = Vec::new();
    for (name, tool) in FILES {
        if fs::symlink_metadata(root.join(name)).is_ok() {
            found.push(format!("{name} ({tool})"));
        }
    }
    if let Ok(entries) = fs::read_dir(root) {
        let packages = entries.flatten().filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir()) && entry.file_name() != ".git"
        });
        for package in packages {
            if fs::symlink_metadata(package.path().join(".stow-local-ignore")).is_ok() {
                found.push(format!(
                    "{}/.stow-local-ignore (Stow)",
                    package.file_name().to_string_lossy()
                ));
            }
        }
    }
    found.sort();
    if found.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "the farm holds {}; dotman never runs or reads them, so per-host logic, hooks and conditions are not carried over: use variants and `[hooks]` in .dotman.toml",
        found.join(", ")
    )]
}

fn farm_remote(root: &Path) -> Option<String> {
    let repo = root
        .ancestors()
        .find(|dir| dir.join(".git").exists())?
        .to_path_buf();
    Git::new(repo, "import")
        .remote_url("origin")
        .ok()
        .flatten()
        .map(|url| crate::git::redact_credentials(&url))
}

/// The parent directories' modes are those of the directories the files are deployed through.
fn dir_mode(ctx: &Context, folds: &[(String, PathBuf)], parent: &str) -> Option<u32> {
    let path = folds
        .iter()
        .find(|(link, _)| is_under(parent, link))
        .map_or_else(
            || ctx.paths.home.join(parent),
            |(link, real)| {
                real.join(
                    parent
                        .strip_prefix(link.as_str())
                        .unwrap_or_default()
                        .trim_start_matches('/'),
                )
            },
        );
    fs::symlink_metadata(path)
        .ok()
        .map(|meta| meta.mode() & 0o777)
}

const fn stored_mode(mode: u32) -> u32 {
    if mode & 0o111 == 0 { 0o644 } else { 0o755 }
}

fn repo_config(ctx: &Context, planner: &Planner<'_>) -> RepoConfig {
    let mut config = RepoConfig::default();
    for (link, _) in &planner.folds {
        config.dirs.paths.push(link.clone());
    }
    config.dirs.paths.sort();
    for write in &planner.writes {
        if matches!(write.body, Body::Bytes(_)) {
            record_mode(&mut config, &write.key, write.mode, stored_mode(write.mode));
        }
        for parent in parents(&write.key) {
            let mode = dir_mode(ctx, &planner.folds, parent).unwrap_or(0o755);
            record_mode(&mut config, &format!("{parent}/"), mode, 0o755);
        }
    }
    config
}

fn write_files(repo: &Path, writes: &[Write]) -> Result<()> {
    for write in writes {
        ensure_worktree_parents(repo, &write.key)?;
        let path = repo.join(&write.key);
        match &write.body {
            Body::Link(text) => symlink(text, &path)
                .with_context(|| format!("cannot create the symlink {}", write.key))?,
            Body::Bytes(bytes) => write_atomic(&path, bytes, stored_mode(write.mode))?,
        }
    }
    Ok(())
}

/// Imports the farm at `source`.
pub(super) fn import(ctx: &Context, source: &Path, args: &ImportArgs) -> Result<Outcome> {
    require_fresh(ctx)?;
    author_email(ctx)?;
    check_target(&ctx.paths.repo)?;
    let edited = EditedConfig::find(ctx)?;
    let root =
        fs::canonicalize(source).with_context(|| format!("cannot resolve {}", source.display()))?;
    ensure!(is_real_dir(&root), "{} is not a directory", root.display());
    let stores = nix_stores();
    if let Some(why) = refusal(ctx, &root, &stores, true) {
        bail!("{} cannot be a symlink farm: {why}", source.display());
    }

    let defaults = RepoConfig::default();
    let mut planner = Planner {
        ctx,
        secrets: secret_patterns(&defaults)?,
        ignore: Ignore::compile(&defaults.ignore.patterns)?,
        writes: Vec::new(),
        items: Vec::new(),
        previews: Vec::new(),
        folds: Vec::new(),
        ignored: 0,
        bytes: 0,
        too_big: false,
    };
    let mut links = home_links(ctx);
    links.sort_by(|a, b| a.rel.cmp(&b.rel));
    for link in links.iter().filter(|link| !link.real.starts_with(&root)) {
        if let Some(fate) = fate(link, &root, &stores) {
            planner.link(link, fate);
        }
    }
    ensure!(
        !planner.too_big,
        "the farm holds more than {} entries or {} MiB; `{} import` will not read a tree that large",
        MAX_ENTRIES,
        MAX_BYTES >> 20,
        name::bin()
    );
    ensure!(
        !planner.writes.is_empty() || !planner.items.is_empty(),
        "no symlink in {} resolves under {}",
        ctx.paths.home.display(),
        root.display()
    );
    planner.settle();

    for plan in &planner.previews {
        preview(plan);
    }
    let config = repo_config(ctx, &planner);
    let report = Report {
        converted: planner.writes.len(),
        needs_action: std::mem::take(&mut planner.items),
        notes: notes(&root, &planner, edited.as_ref(), args.dry_run),
    };
    report.print();

    let repo = &ctx.paths.repo;
    let files = count(planner.writes.len(), "file");
    if args.dry_run {
        output::info(&format!(
            "would import {files} into {} as one commit; $HOME stays as it is",
            repo.display()
        ));
        return Ok(report.outcome());
    }
    if planner.writes.is_empty() {
        output::info("nothing to import");
        return Ok(report.outcome());
    }
    if !proceed(
        args,
        &format!("import {files} into {}? [y/N]", repo.display()),
    )? {
        output::info("nothing imported");
        return Ok(Outcome::Done);
    }

    let staging = Staging::new(ctx, true)?;
    staging.git.init()?;
    write_files(&staging.repo, &planner.writes)?;
    config.save(&staging.repo.join(".dotman.toml"), false)?;
    staging.git.stage_all()?;
    staging
        .git
        .commit(&format!("import: symlink farm {}", root.display()))?;
    install_keeping_origin(ctx, &staging, edited.as_ref())?;
    output::success(&format!("imported {files} into {}", repo.display()));
    let bin = name::bin();
    output::info(&format!(
        "next: run `{bin} status`, then `{bin} import --swap` to replace the links with real files; run `{bin} apply` after that"
    ));
    Ok(report.outcome())
}

fn notes(
    root: &Path,
    planner: &Planner<'_>,
    edited: Option<&EditedConfig>,
    dry_run: bool,
) -> Vec<String> {
    let mut notes = vec![format!(
        "imported from the symlink farm {}; the farm and $HOME are not changed",
        root.display()
    )];
    let bin = name::bin();
    notes.push(format!(
        "the links in $HOME still stand until `{bin} import --swap` replaces them with real files; after that, do not run `stow -D`, dotbot or a link script on the swapped paths"
    ));
    notes.push(farm_remote(root).map_or_else(
        || format!("the new repo has no remote; add one with `{bin} init --remote <url>`"),
        |url| {
            format!(
                "the farm's git history is not imported, and its origin is {url} (`{bin} init --remote {url}` sets it here)"
            )
        },
    ));
    notes.extend(tool_notes(root));
    if planner.ignored > 0 {
        notes.push(format!(
            "{} below folded directories skipped by the [ignore] patterns",
            count(planner.ignored, "path")
        ));
    }
    if let Some(edited) = edited {
        notes.push(edited.note(dry_run));
    }
    notes
}

/// Where a link is in `$HOME`, by key.
enum At {
    Link(PathBuf),
    /// A parent of the key is a link: the outermost one, as a key.
    Under(String),
    Other,
}

fn locate(home: &Path, key: &str) -> At {
    let mut path = home.to_path_buf();
    let mut prefix = String::new();
    let total = key.split('/').count();
    for (at, part) in key.split('/').enumerate() {
        path.push(part);
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return At::Other;
        };
        if meta.file_type().is_symlink() {
            let Ok(raw) = fs::read_link(&path) else {
                return At::Other;
            };
            return if at.saturating_add(1) == total {
                At::Link(raw)
            } else {
                At::Under(prefix)
            };
        }
        if !meta.is_dir() {
            return At::Other;
        }
    }
    At::Other
}

enum Change {
    File { bytes: Vec<u8>, mode: u32 },
    Dir(BTreeMap<String, Member>),
}

struct Swap {
    key: String,
    raw: PathBuf,
    change: Change,
    /// What S records for each key the swap writes, as `apply` would.
    records: Vec<(String, Entry)>,
}

#[derive(Default)]
struct Found {
    swaps: Vec<Swap>,
    items: Vec<Item>,
}

fn winner(variants: &Variants<'_>, key: &str) -> Option<SourceEntry> {
    match variants.get(key) {
        Ok(Selection::Winner(entry)) => Some(entry),
        _ => None,
    }
}

/// Why a link cannot be compared with the repo, or what it resolves to.
fn deployed(
    home_real: &Path,
    key: &str,
    raw: &Path,
    stores: &[PathBuf],
) -> Result<PathBuf, (&'static str, String)> {
    let link = Link {
        rel: PathBuf::from(key),
        real: home_real.join(key),
        raw: raw.to_path_buf(),
    };
    let shown = link.shown();
    let target = fs::canonicalize(&link.real);
    if in_stores(&link.first_hop(), stores)
        || target.as_ref().is_ok_and(|real| in_stores(real, stores))
    {
        return Err((NIX, shown));
    }
    target.map_err(|_| (DANGLING_LINK, shown))
}

impl Found {
    fn file(
        &mut self,
        ctx: &Context,
        variants: &Variants<'_>,
        key: &str,
        raw: &Path,
        stores: &[PathBuf],
    ) {
        let Some(r) = winner(variants, key) else {
            return;
        };
        let RContent::Bytes(bytes) = &r.content else {
            return;
        };
        let real = match deployed(&real_home(ctx), key, raw, stores) {
            Ok(real) => real,
            Err((reason, detail)) => {
                self.items.push(Item { reason, detail });
                return;
            }
        };
        let shown = format!("{key} -> {}", raw.display());
        let same = fs::metadata(&real)
            .ok()
            .filter(fs::Metadata::is_file)
            .is_some_and(|meta| {
                meta.len() == u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                    && OpenOptions::new()
                        .read(true)
                        .custom_flags(READ_FLAGS)
                        .open(&real)
                        .and_then(|mut file| {
                            let mut now = Vec::new();
                            file.read_to_end(&mut now).map(|_| now)
                        })
                        .is_ok_and(|now| now == *bytes)
            });
        if !same {
            let reason = if fs::metadata(&real).is_ok_and(|meta| meta.is_file()) {
                DIFFERS
            } else {
                NOT_FILE
            };
            self.items.push(Item {
                reason,
                detail: shown,
            });
            return;
        }
        self.swaps.push(Swap {
            key: key.to_owned(),
            raw: raw.to_path_buf(),
            records: vec![(key.to_owned(), r.tuple.entry(&r.source))],
            change: Change::File {
                bytes: bytes.clone(),
                mode: r.tuple.mode,
            },
        });
    }

    fn dir(
        &mut self,
        ctx: &Context,
        variants: &Variants<'_>,
        dir: &str,
        keys: &[String],
        stores: &[PathBuf],
    ) {
        let Ok(raw) = fs::read_link(ctx.paths.home.join(dir)) else {
            return;
        };
        let real = match deployed(&real_home(ctx), dir, &raw, stores) {
            Ok(real) => real,
            Err((reason, detail)) => {
                self.items.push(Item { reason, detail });
                return;
            }
        };
        let shown = format!("{dir} -> {}", raw.display());
        if !is_real_dir(&real) {
            self.items.push(Item {
                reason: NOT_FILE,
                detail: shown,
            });
            return;
        }
        let mut members = BTreeMap::new();
        let mut records = Vec::new();
        for key in keys {
            let Some(below) = key
                .strip_prefix(dir)
                .map(|rest| rest.trim_start_matches('/'))
            else {
                continue;
            };
            let Some(r) = winner(variants, key) else {
                self.items.push(Item {
                    reason: DIFFERS,
                    detail: format!("{shown}: no single version of {key} applies here"),
                });
                return;
            };
            members.insert(
                below.to_owned(),
                match &r.content {
                    RContent::Bytes(bytes) => Member::File {
                        bytes: bytes.clone(),
                        mode: r.tuple.mode,
                    },
                    RContent::Link(target) => Member::Link(target.clone()),
                },
            );
            records.push((key.clone(), r.tuple.entry(&r.source)));
        }
        if let Some(why) = tree_difference(ctx, &real, dir, &members) {
            self.items.push(Item {
                reason: DIFFERS,
                detail: format!("{shown}: {why}"),
            });
            return;
        }
        self.swaps.push(Swap {
            key: dir.to_owned(),
            raw,
            change: Change::Dir(members),
            records,
        });
    }
}

/// The first way in which the files below `real` differ from `members`, if any.
fn tree_difference(
    ctx: &Context,
    real: &Path,
    dir: &str,
    members: &BTreeMap<String, Member>,
) -> Option<String> {
    let mut matched = 0usize;
    for entry in WalkDir::new(real)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
    {
        let Ok(entry) = entry else {
            return Some("part of it cannot be read".to_owned());
        };
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        let name = entry
            .path()
            .strip_prefix(real)
            .ok()
            .and_then(Path::to_str)?;
        let same = match members.get(name) {
            None => return Some(format!("it holds {name}, which the repo lacks")),
            Some(Member::Link(target)) => {
                file_type.is_symlink()
                    && fs::read_link(entry.path()).is_ok_and(|raw| {
                        let key = format!("{dir}/{name}");
                        let parent = entry.path().parent().unwrap_or(real);
                        link_text(&raw, parent, real, &key, ctx).as_deref() == target.to_str()
                    })
            }
            Some(Member::File { bytes, .. }) => {
                file_type.is_file() && fs::read(entry.path()).is_ok_and(|now| now == *bytes)
            }
        };
        if !same {
            return Some(format!("{name} differs from the repo"));
        }
        matched = matched.saturating_add(1);
    }
    members
        .keys()
        .find(|rel| real.join(rel).symlink_metadata().is_err())
        .map(|rel| format!("the repo has {rel}, which the directory lacks"))
        .or_else(|| (matched != members.len()).then(|| "its files differ from the repo".to_owned()))
}

fn find_swaps(ctx: &Context, repo_config: &RepoConfig) -> Result<Found> {
    let sources = ctx.sources()?;
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);
    let variants = Variants::new(
        worktree,
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    let stores = nix_stores();
    let mut found = Found::default();
    let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in sources.keys() {
        match locate(&ctx.paths.home, key) {
            At::Link(raw) => found.file(ctx, &variants, key, &raw, &stores),
            At::Under(dir) => dirs.entry(dir).or_default().push(key.to_owned()),
            At::Other => {}
        }
    }
    for (dir, keys) in dirs {
        found.dir(ctx, &variants, &dir, &keys, &stores);
    }
    found.swaps.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(found)
}

/// Replaces each swap, or puts every link back and fails.
fn replace_all(ctx: &Context, swaps: &[Swap], modes: &BTreeMap<String, String>) -> Result<()> {
    let home = &ctx.paths.home;
    let run = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
    let mut record = None;
    for swap in swaps {
        record = Some(record_link(&run, &swap.key, &swap.raw)?);
    }
    if let Some(file) = &record {
        output::info(&format!("the links are recorded in {}", file.display()));
    }
    for (done, swap) in swaps.iter().enumerate() {
        let result = match &swap.change {
            Change::File { bytes, mode } => {
                replace_link_with_file(home, &swap.key, &swap.raw, bytes, *mode)
            }
            Change::Dir(members) => {
                replace_link_with_dir(home, &swap.key, &swap.raw, members, modes)
            }
        };
        if let Err(e) = result {
            let attempted = swaps.get(..=done).unwrap_or(swaps);
            let (restored, kept) = restore_all(home, attempted);
            let mut message = format!(
                "could not replace {}: {e:#}; {} restored",
                swap.key,
                count(restored, "link")
            );
            if let (true, Some(file)) = (kept > 0, &record) {
                let _ = write!(
                    message,
                    ", {kept} could not be restored and are listed in {}",
                    file.display()
                );
            }
            bail!("{message}");
        }
    }
    Ok(())
}

/// Puts back the links of `swaps`, last first. Returns how many are links again and how many were
/// left because they are no longer what the swap wrote.
fn restore_all(home: &Path, swaps: &[Swap]) -> (usize, usize) {
    let (mut restored, mut kept) = (0usize, 0usize);
    for swap in swaps.iter().rev() {
        let result = match &swap.change {
            Change::File { bytes, mode } => {
                restore_file_link(home, &swap.key, &swap.raw, bytes, *mode)
            }
            Change::Dir(members) => restore_dir_link(home, &swap.key, &swap.raw, members),
        };
        match result {
            Ok(Restored::Back | Restored::Untouched) => restored = restored.saturating_add(1),
            Ok(Restored::Kept) => {
                kept = kept.saturating_add(1);
                output::warning(&format!(
                    "{}: left as it is; it is no longer what the swap wrote",
                    swap.key
                ));
            }
            Err(e) => {
                kept = kept.saturating_add(1);
                output::warning(&format!("{}: cannot restore the link: {e:#}", swap.key));
            }
        }
    }
    (restored, kept)
}

/// `dotman import --swap`: replaces the links at managed keys with real files holding the same
/// bytes, or puts every link back when one replacement fails. A link whose content differs from
/// the repo is reported and kept.
pub(super) fn swap(ctx: &Context, args: &ImportArgs) -> Result<Outcome> {
    let repo_id = ctx.repo_id()?;
    let repo_config = ctx.repo_config()?;
    let found = find_swaps(ctx, &repo_config)?;
    let report = Report {
        converted: found.swaps.len(),
        needs_action: found.items,
        notes: Vec::new(),
    };
    let total = count(found.swaps.len(), "link");
    for swap in &found.swaps {
        output::action(
            if args.dry_run { "would swap" } else { "swap" },
            &format!("{} -> {}", swap.key, swap.raw.display()),
        );
    }
    report.print_items();
    if found.swaps.is_empty() {
        output::info("no links to replace");
        return Ok(report.outcome());
    }
    if args.dry_run {
        output::info(&format!("would replace {total} with real files"));
        return Ok(report.outcome());
    }
    if !proceed(args, &format!("replace {total} with real files? [y/N]"))? {
        output::info("nothing swapped");
        return Ok(Outcome::Done);
    }

    replace_all(ctx, &found.swaps, &repo_config.modes)?;
    let mut state = ctx.state(&repo_id)?;
    for swap in &found.swaps {
        for (key, entry) in &swap.records {
            state.files.insert(key.clone(), entry.clone());
        }
    }
    if state.identity.is_none() {
        state.identity = Some(ctx.identity().clone());
    }
    state.save(&ctx.paths.state_file(), false)?;
    output::success(&format!("swapped {total} into real files"));
    Ok(report.outcome())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalizing_drops_dots_without_touching_the_disk() {
        assert_eq!(
            normalize(Path::new("/h/dotfiles/../x/./y")),
            Path::new("/h/x/y")
        );
        assert_eq!(normalize(Path::new("/a/b/../../..")), Path::new("/"));
    }

    #[test]
    fn the_common_directory_is_one_level_up_when_it_is_a_target() {
        let paths = |items: &[&str]| -> PathBuf {
            let owned: Vec<PathBuf> = items.iter().map(PathBuf::from).collect();
            let refs: Vec<&Path> = owned.iter().map(PathBuf::as_path).collect();
            common_dir(&refs)
        };
        assert_eq!(
            paths(&[
                "/h/d/zsh/.zshrc",
                "/h/d/git/.gitconfig",
                "/h/d/nvim/.config/nvim"
            ]),
            Path::new("/h/d")
        );
        assert_eq!(
            paths(&["/h/.local/share/Steam", "/h/.local/share/Steam"]),
            Path::new("/h/.local/share")
        );
        assert_eq!(paths(&["/h/d/a/x", "/h/d/a/y"]), Path::new("/h/d/a"));
    }

    #[test]
    fn targets_are_grouped_below_home_or_by_two_components_elsewhere() {
        let home = Path::new("/home/u");
        assert_eq!(
            anchor(Path::new("/home/u/dotfiles/zsh/.zshrc"), home),
            Path::new("/home/u/dotfiles")
        );
        assert_eq!(
            anchor(Path::new("/mnt/data/dotfiles/zsh/.zshrc"), home),
            Path::new("/mnt/data")
        );
    }
}
