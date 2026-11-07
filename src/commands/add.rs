//! `dot add`: copy files and symlinks into the repo worktree and record their modes.
//!
//! Planning reads `$HOME` and changes nothing, so a refused path leaves the repo untouched.

use crate::cli::AddArgs;
use crate::context::{Context, TEMP_PREFIX, is_real_dir};
use crate::engine::status::{Source, Worktree};
use crate::engine::{Node, parents, portable_link_target, read_node};
use crate::lock::Lock;
use crate::output;
use crate::paths::{
    absolute_input, derive_key, find_collisions, refuse_non_nfc, validate_key, write_atomic,
};
use crate::prompt;
use crate::repo_config::RepoConfig;
use crate::state::{Kind, State};
use anyhow::{Context as _, Result, anyhow, bail};
use glob::{MatchOptions, Pattern};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

// Binary detection looks at this many leading bytes, as git does.
const SNIFF_BYTES: u64 = 8192;
// A new file over this size is probably a download, a cache or a database, not a config.
const HOLD_BYTES: u64 = 1_048_576;
const LARGEST_SHOWN: usize = 5;
const MATCH: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};
// A guard that `.ENV` or `Credentials.json` slipped past would defeat its purpose.
const MATCH_SECRET: MatchOptions = MatchOptions {
    case_sensitive: false,
    ..MATCH
};

#[derive(Clone, Copy)]
enum Leaf {
    File { mode: u32, size: u64, binary: bool },
    Link,
}

impl Leaf {
    const fn kind(self) -> Kind {
        match self {
            Self::File { .. } => Kind::File,
            Self::Link => Kind::Symlink,
        }
    }
}

enum Found {
    Leaf(Leaf),
    Dir,
    Dangling,
    Skip(String),
}

#[derive(Default)]
struct DirPlan {
    key: String,
    items: BTreeMap<String, Leaf>,
    skipped: Vec<String>,
    dangling: Vec<String>,
}

#[derive(Default)]
struct Plan {
    files: BTreeMap<String, Leaf>,
    dirs: Vec<DirPlan>,
    skipped: Vec<String>,
}

const DANGLING: &str = "dangling symlink";

struct Ignore {
    any: Vec<Pattern>,
    dirs_only: Vec<Pattern>,
}

impl Ignore {
    fn compile(patterns: &[String]) -> Result<Self> {
        let mut ignore = Self {
            any: Vec::new(),
            dirs_only: Vec::new(),
        };
        for text in patterns {
            let (body, dirs_only) = text
                .strip_suffix('/')
                .map_or((text.as_str(), false), |body| (body, true));
            let pattern = Pattern::new(body)
                .with_context(|| format!("bad [ignore] pattern `{text}` in .dotman.toml"))?;
            if dirs_only {
                ignore.dirs_only.push(pattern);
            } else {
                ignore.any.push(pattern);
            }
        }
        Ok(ignore)
    }

    fn matches(&self, key: &str, is_dir: bool) -> bool {
        self.any.iter().any(|p| p.matches_with(key, MATCH))
            || (is_dir && self.dirs_only.iter().any(|p| p.matches_with(key, MATCH)))
    }
}

/// Runs `dot add`.
pub fn run(args: &AddArgs) -> Result<()> {
    let ctx = Context::resolve("add")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
    let repo_id = ctx.repo_id()?;
    let mut repo_config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;

    let mut plan = Planner {
        ctx: &ctx,
        ignore: Ignore::compile(&repo_config.ignore.patterns)?,
        cwd: std::env::current_dir().context("cannot read the current directory")?,
        out: Plan::default(),
    }
    .plan(&args.paths)?;
    refuse_collisions(&ctx, &plan)?;

    for line in &plan.skipped {
        output::warning(&format!("skipped {line}"));
    }
    for dir in &plan.dirs {
        preview(dir);
    }
    let in_dirs: usize = plan.dirs.iter().map(|d| d.items.len()).sum();
    if dry_run {
        for key in plan.files.keys() {
            output::action("would add", key);
        }
        if in_dirs > 0 {
            output::info(&format!("would add {in_dirs} files from directories"));
        }
        return Ok(());
    }

    if in_dirs > 0 && !args.yes && prompt::interactive() {
        let question = format!("add {in_dirs} files? [y/N]");
        if !prompt::confirm(&question)? {
            output::info("directories not added");
            plan.dirs.clear();
        }
    }

    let announced: BTreeSet<String> = plan.files.keys().cloned().collect();
    let mut items = plan.files;
    for dir in &plan.dirs {
        for (key, leaf) in &dir.items {
            items.entry(key.clone()).or_insert(*leaf);
        }
        if !repo_config.dirs.paths.contains(&dir.key) {
            repo_config.dirs.paths.push(dir.key.clone());
        }
    }
    let mut added = 0usize;
    let mut failed = 0usize;
    let mut dir_modes = BTreeMap::new();
    for (key, leaf) in &items {
        let one = commit_one(
            &ctx,
            &mut repo_config,
            &mut state,
            &mut dir_modes,
            key,
            leaf.kind(),
        );
        match one {
            Ok(()) => {
                added = added.saturating_add(1);
                if announced.contains(key) {
                    output::action("add", key);
                }
            }
            Err(e) => {
                failed = failed.saturating_add(1);
                output::warning(&format!("could not add {key}: {e:#}"));
            }
        }
    }

    repo_config.save(&ctx.repo_config_file(), false)?;
    state.save(&ctx.paths.state_file(), false)?;
    if failed > 0 {
        bail!("{failed} of {} paths could not be added", items.len());
    }
    output::success(&format!("added {added} paths"));
    Ok(())
}

/// Why a new file waits for an explicit `dot add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hold {
    Large,
    Binary,
    SecretName,
}

impl Hold {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Large => "over 1 MiB",
            Self::Binary => "binary: NUL in the first 8 KiB",
            Self::SecretName => "name matches [guard] secret_names",
        }
    }
}

/// Candidates under the `[dirs]` entries that no key and no ignore pattern covers.
#[derive(Default)]
pub(crate) struct Scan {
    pub new: BTreeMap<String, Kind>,
    pub held: BTreeMap<String, Hold>,
    /// Dangling links and case or normalization collisions: reported, never captured.
    pub notes: Vec<String>,
}

/// Walks every `[dirs]` entry with `add <dir>`'s walker. `known` is every key that has an R or an S.
pub(crate) fn scan_dirs(
    ctx: &Context,
    repo_config: &RepoConfig,
    known: &BTreeSet<String>,
) -> Result<Scan> {
    let secrets = repo_config
        .guard
        .secret_names
        .iter()
        .map(|text| {
            Pattern::new(text)
                .with_context(|| format!("bad [guard] pattern `{text}` in .dotman.toml"))
        })
        .collect::<Result<Vec<_>>>()?;
    let planner = Planner {
        ctx,
        ignore: Ignore::compile(&repo_config.ignore.patterns)?,
        cwd: PathBuf::new(),
        out: Plan::default(),
    };
    let mut found: BTreeMap<String, Leaf> = BTreeMap::new();
    let mut dangling = BTreeSet::new();
    for dir in &repo_config.dirs.paths {
        // The entry comes from a tracked file, so it is checked like a user-supplied path.
        let plain = validate_key(dir).is_ok()
            && parents(dir)
                .chain(std::iter::once(dir.as_str()))
                .all(|part| is_real_dir(&ctx.paths.home.join(part)));
        if !plain {
            continue;
        }
        let walked = planner.walk(dir);
        found.extend(walked.items);
        dangling.extend(walked.dangling);
    }
    found.retain(|key, _| !known.contains(key));
    dangling.retain(|key| !known.contains(key));

    let mut scan = Scan {
        notes: dangling
            .into_iter()
            .map(|key| format!("{key}: {DANGLING}"))
            .collect(),
        ..Scan::default()
    };
    let colliding = find_collisions(
        known
            .iter()
            .map(String::as_str)
            .chain(found.keys().map(String::as_str)),
    );
    for key in colliding.iter().flat_map(|(a, b)| [a, b]) {
        if found.remove(key).is_some() {
            scan.notes.push(format!(
                "{key}: differs only in case or normalization from another path"
            ));
        }
    }
    for (key, leaf) in found {
        let hold = if secrets.iter().any(|p| p.matches_with(&key, MATCH_SECRET)) {
            Some(Hold::SecretName)
        } else {
            match leaf {
                Leaf::File { size, .. } if size > HOLD_BYTES => Some(Hold::Large),
                Leaf::File { binary: true, .. } => Some(Hold::Binary),
                _ => None,
            }
        };
        match hold {
            Some(hold) => {
                scan.held.insert(key, hold);
            }
            None => {
                scan.new.insert(key, leaf.kind());
            }
        }
    }
    Ok(scan)
}

struct Planner<'a> {
    ctx: &'a Context,
    ignore: Ignore,
    cwd: PathBuf,
    out: Plan,
}

impl Planner<'_> {
    fn plan(mut self, inputs: &[PathBuf]) -> Result<Plan> {
        for input in inputs {
            self.input(input)?;
        }
        Ok(self.out)
    }

    fn input(&mut self, input: &Path) -> Result<()> {
        let paths = &self.ctx.paths;
        let home_real = paths.home_real.as_deref();
        let key = derive_key(input, &self.cwd, &paths.home, home_real)?;
        if key
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
        {
            return Ok(());
        }
        let abs = absolute_input(input, &self.cwd, &paths.home);
        paths.refuse_protected(&abs)?;
        let rel = std::iter::once(paths.home.as_path())
            .chain(home_real)
            .find_map(|root| abs.strip_prefix(root).ok())
            .unwrap_or(&abs);
        refuse_non_nfc(rel)?;
        for parent in parents(&key) {
            let meta = fs::symlink_metadata(paths.home.join(parent))
                .with_context(|| format!("{} does not exist", input.display()))?;
            if !meta.is_dir() {
                bail!("`{parent}` is a symlink or not a directory; dotman does not add through it");
            }
        }
        if is_missing(&paths.home, &key) {
            bail!("{} does not exist", input.display());
        }
        match self.inspect(&key) {
            Found::Leaf(leaf) => {
                self.out.files.insert(key, leaf);
            }
            Found::Dir => {
                let dir = self.walk(&key);
                self.out.dirs.push(dir);
            }
            Found::Dangling => self.out.skipped.push(format!("{key}: {DANGLING}")),
            Found::Skip(reason) => self.out.skipped.push(format!("{key}: {reason}")),
        }
        Ok(())
    }

    fn inspect(&self, key: &str) -> Found {
        let path = self.ctx.paths.home.join(key);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => return Found::Skip(format!("cannot stat: {e}")),
        };
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            match fs::metadata(&path) {
                Ok(_) => Found::Leaf(Leaf::Link),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Found::Dangling,
                Err(e) => Found::Skip(format!("symlink cannot be resolved: {e}")),
            }
        } else if file_type.is_dir() {
            Found::Dir
        } else if file_type.is_file() {
            match has_nul_in_head(&path) {
                Ok(binary) => Found::Leaf(Leaf::File {
                    mode: meta.mode() & 0o7777,
                    size: meta.len(),
                    binary,
                }),
                Err(e) => Found::Skip(format!("unreadable: {e}")),
            }
        } else {
            Found::Skip("socket, FIFO or device".to_owned())
        }
    }

    fn walk(&self, dir_key: &str) -> DirPlan {
        let paths = &self.ctx.paths;
        let mut dir = DirPlan {
            key: dir_key.to_owned(),
            ..DirPlan::default()
        };
        let mut walk = WalkDir::new(paths.home.join(dir_key))
            .follow_links(false)
            .min_depth(1)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = walk.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    let at = e
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    dir.skipped.push(format!("{at}: unreadable: {e}"));
                    continue;
                }
            };
            let is_dir = entry.file_type().is_dir();
            let name = entry.file_name().to_string_lossy();
            let skip_silently = name.starts_with(TEMP_PREFIX) || name == ".git";
            let verdict = if skip_silently {
                Err(None)
            } else {
                self.entry_key(entry.path(), is_dir)
            };
            match verdict {
                Err(reason) => {
                    if let Some(reason) = reason {
                        dir.skipped.push(reason);
                    }
                    if is_dir {
                        walk.skip_current_dir();
                    }
                }
                Ok(_) if is_dir => {}
                Ok(key) => match self.inspect(&key) {
                    Found::Leaf(leaf) => {
                        dir.items.insert(key, leaf);
                    }
                    Found::Dangling => {
                        dir.skipped.push(format!("{key}: {DANGLING}"));
                        dir.dangling.push(key);
                    }
                    Found::Skip(reason) => dir.skipped.push(format!("{key}: {reason}")),
                    Found::Dir => {}
                },
            }
        }
        dir
    }

    // `Err(None)` is an entry left out silently, `Err(Some(_))` one that is reported.
    fn entry_key(&self, path: &Path, is_dir: bool) -> std::result::Result<String, Option<String>> {
        let paths = &self.ctx.paths;
        let rel = path
            .strip_prefix(&paths.home)
            .map_err(|e| Some(format!("{}: {e}", path.display())))?;
        if let Err(e) = refuse_non_nfc(rel) {
            return Err(Some(format!("{e}")));
        }
        let key = rel
            .to_str()
            .ok_or_else(|| Some(format!("{} is not valid UTF-8", path.display())))?;
        validate_key(key).map_err(|e| Some(format!("{key}: {e}")))?;
        if paths.refuse_protected(path).is_err() {
            return Err(Some(format!("{key}: dotman's own files are never managed")));
        }
        if self.ignore.matches(key, is_dir) {
            return Err(None);
        }
        Ok(key.to_owned())
    }
}

fn is_missing(home: &Path, key: &str) -> bool {
    fs::symlink_metadata(home.join(key)).is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
}

fn has_nul_in_head(path: &Path) -> io::Result<bool> {
    // O_NONBLOCK so that a file swapped for a FIFO after the lstat cannot hang the open.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let mut head = Vec::new();
    file.take(SNIFF_BYTES).read_to_end(&mut head)?;
    Ok(head.contains(&0))
}

fn refuse_collisions(ctx: &Context, plan: &Plan) -> Result<()> {
    let new: BTreeSet<&str> = plan
        .files
        .keys()
        .chain(plan.dirs.iter().flat_map(|d| d.items.keys()))
        .map(String::as_str)
        .collect();
    let existing = ctx.managed()?;
    let pairs = find_collisions(
        existing
            .keys()
            .map(String::as_str)
            .chain(new.iter().copied()),
    );
    match pairs
        .iter()
        .find(|(a, b)| new.contains(a.as_str()) || new.contains(b.as_str()))
    {
        Some((a, b)) => bail!("{a} and {b} differ only in case or normalization"),
        None => Ok(()),
    }
}

fn preview(dir: &DirPlan) {
    let mut sized: Vec<(u64, &str)> = Vec::new();
    let (mut binary, mut private) = (0usize, 0usize);
    for (key, leaf) in &dir.items {
        if let Leaf::File {
            mode,
            size,
            binary: is_binary,
        } = leaf
        {
            sized.push((*size, key));
            binary = binary.saturating_add(usize::from(*is_binary));
            private = private.saturating_add(usize::from(mode & 0o777 == 0o600));
        }
    }
    let total = sized
        .iter()
        .fold(0u64, |sum, (size, _)| sum.saturating_add(*size));
    sized.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    output::info(&format!("{}/", dir.key));
    output::info(&format!("  files: {}", dir.items.len()));
    output::info(&format!("  total size: {total} bytes"));
    output::info("  largest:");
    for (size, key) in sized.iter().take(LARGEST_SHOWN) {
        output::info(&format!("    {size} bytes  {key}"));
    }
    output::info(&format!("  binary files: {binary}"));
    output::info(&format!("  files at mode 0600: {private}"));
    output::info(&format!("  skipped: {}", dir.skipped.len()));
    for line in &dir.skipped {
        output::info(&format!("    {line}"));
    }
}

/// Copies H of `key` into the repo worktree, records its mode in `repo_config` and sets S to the
/// result. `expected` is the kind the caller saw, and a different kind now is an error. The caller
/// saves `repo_config` and `state`.
pub(crate) fn commit_one(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    state: &mut State,
    dir_modes: &mut BTreeMap<String, u32>,
    key: &str,
    expected: Kind,
) -> Result<()> {
    let home = &ctx.paths.home;
    let repo = &ctx.paths.repo;
    let target = repo.join(key);
    ensure_worktree_parents(repo, key)?;
    match (expected, read_node(home, key)?) {
        (Kind::File, Node::File { bytes, mode, .. }) => {
            let stored = if mode & 0o111 == 0 { 0o644 } else { 0o755 };
            write_atomic(&target, &bytes, stored)?;
            record_mode(repo_config, key, mode & 0o777, stored);
        }
        (Kind::Symlink, Node::Symlink(link)) => {
            let relative = portable_link_target(&link, key, home, ctx.paths.home_real.as_deref());
            replace_with_symlink(&target, &relative)?;
            repo_config.modes.remove(key);
        }
        _ => bail!("it changed while being added"),
    }
    for parent in parents(key) {
        let mode = dir_mode(dir_modes, home, parent)?;
        record_mode(repo_config, &format!("{parent}/"), mode, 0o755);
    }
    let entry = Worktree::new(repo, &repo_config.modes)
        .get(key)?
        .ok_or_else(|| anyhow!("{key} is not in the repo after being written"))?;
    state.files.insert(key.to_owned(), entry.tuple.entry(key));
    Ok(())
}

fn dir_mode(cache: &mut BTreeMap<String, u32>, home: &Path, parent: &str) -> Result<u32> {
    if let Some(mode) = cache.get(parent) {
        return Ok(*mode);
    }
    let mode = fs::symlink_metadata(home.join(parent))
        .with_context(|| format!("cannot stat {parent}"))?
        .mode()
        & 0o777;
    cache.insert(parent.to_owned(), mode);
    Ok(mode)
}

fn record_mode(repo_config: &mut RepoConfig, key: &str, mode: u32, default: u32) {
    if mode == default {
        repo_config.modes.remove(key);
    } else {
        repo_config
            .modes
            .insert(key.to_owned(), format!("{mode:04o}"));
    }
}

// A symlink in the worktree (a cloned repo can ship one) must not carry the write out of the repo.
fn ensure_worktree_parents(repo: &Path, key: &str) -> Result<()> {
    for parent in parents(key) {
        let path = repo.join(parent);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "{} in the repo is a symlink or a file, not a directory",
                path.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&path)
                .with_context(|| format!("cannot create {}", path.display()))?,
            Err(e) => return Err(e).with_context(|| format!("cannot stat {}", path.display())),
        }
    }
    Ok(())
}

fn replace_with_symlink(path: &Path, target: &Path) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    for attempt in 0..16u32 {
        let tmp = dir.join(format!(
            "{TEMP_PREFIX}link-{}-{attempt}",
            std::process::id()
        ));
        match symlink(target, &tmp) {
            Ok(()) => {
                return fs::rename(&tmp, path).map_err(|e| {
                    let _ = fs::remove_file(&tmp);
                    anyhow!(e).context(format!("cannot replace {}", path.display()))
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).context("cannot create a temp symlink"),
        }
    }
    bail!("cannot find a free temp name in {}", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_patterns_match_like_gitignore_globs() {
        let ignore = Ignore::compile(&[
            "**/.git/".to_owned(),
            "**/*.swp".to_owned(),
            "**/node_modules/".to_owned(),
        ])
        .unwrap();
        assert!(ignore.matches("a.swp", false));
        assert!(ignore.matches(".config/x/a.swp", false));
        assert!(ignore.matches(".config/x/node_modules", true));
        assert!(!ignore.matches(".config/x/node_modules", false));
        assert!(!ignore.matches(".config/x/a.conf", false));
    }
}
