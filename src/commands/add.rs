//! `dotman add`: copy files and symlinks into the repo worktree and record their modes.
//!
//! Planning reads `$HOME` and changes nothing, so a refused path leaves the repo untouched.

use crate::cli::AddArgs;
use crate::commands::Outcome;
use crate::commands::apply::{Classified, classify_key};
use crate::context::{Context, TEMP_PREFIX, is_real_dir};
use crate::engine::status::{Content, SourceEntry, Variants, Worktree};
use crate::engine::{
    Node, Tuple, parents, portable_link_target, read_node, recorded_mode, safe_mode,
};
use crate::lock::Lock;
use crate::name;
use crate::output;
use crate::paths::{
    absolute_input, derive_key, find_collisions, refuse_non_nfc, validate_key, write_atomic,
};
use crate::prompt;
use crate::remote;
use crate::repo_config::RepoConfig;
use crate::secrets::{self, HoldsKey};
use crate::state::{IdentitySnapshot, Kind, State};
use crate::system::{self, Ownership, Standing};
use crate::variant::{self, Flag, Selection, Sources};
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
pub(crate) const MATCH: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};
// A guard that `.ENV` or `Credentials.json` slipped past would defeat its purpose.
const MATCH_SECRET: MatchOptions = MatchOptions {
    case_sensitive: false,
    ..MATCH
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Leaf {
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
    /// Under a managed directory this is held back; the text says why.
    Held(Hold, String),
    Skip(String),
}

/// Why a walk left an entry out.
enum Left {
    /// Ignored, `.git` or a dotman temp file: never mentioned.
    Quiet,
    /// One of dotman's own files: listed in the preview only.
    Own(String),
    /// A name dotman cannot use as a key.
    Name(String),
}

#[derive(Default)]
pub(crate) struct DirPlan {
    pub(crate) key: String,
    pub(crate) items: BTreeMap<String, Leaf>,
    pub(crate) skipped: Vec<String>,
    pub(crate) dangling: Vec<String>,
    /// Sockets, FIFOs, devices and what cannot be read.
    pub(crate) held: BTreeMap<String, Hold>,
    /// Entries whose names cannot be keys.
    pub(crate) notes: Vec<String>,
}

#[derive(Default)]
struct Plan {
    files: BTreeMap<String, Leaf>,
    dirs: Vec<DirPlan>,
}

impl Plan {
    /// Every leaf to add, and the keys named explicitly, which are announced one by one.
    fn items(&self) -> (BTreeMap<String, Leaf>, BTreeSet<String>) {
        let mut items = self.files.clone();
        for dir in &self.dirs {
            for (key, leaf) in &dir.items {
                items.entry(key.clone()).or_insert(*leaf);
            }
        }
        (items, self.files.keys().cloned().collect())
    }
}

pub(crate) const DANGLING: &str = "dangling symlink";
pub(crate) const BLOCK_NAME: &str =
    "the name holds a variant block, so dotman would read it as a variant of another file";

pub(crate) struct Ignore {
    any: Vec<Pattern>,
    dirs_only: Vec<Pattern>,
}

impl Ignore {
    pub(crate) fn compile(patterns: &[String]) -> Result<Self> {
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

    pub(crate) fn matches(&self, key: &str, is_dir: bool) -> bool {
        self.any.iter().any(|p| p.matches_with(key, MATCH))
            || (is_dir && self.dirs_only.iter().any(|p| p.matches_with(key, MATCH)))
    }
}

/// Runs `dotman add`.
///
/// # Errors
///
/// Fails when the repo is not initialised or has a rebase or merge in progress, another process
/// holds the lock, an input is outside `$HOME`, ignored, protected or collides with a managed key,
/// or a file cannot be read or stored.
pub fn run(args: &AddArgs) -> Result<Outcome> {
    let ctx = Context::resolve("add")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
    if !dry_run {
        ctx.sweep_temp_files()?;
    }
    if args.system {
        return add_system(&ctx, args);
    }
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
    let managed: Vec<String> = ctx.managed()?.into_keys().collect();
    let mut targets = Targets {
        sources: Sources::group(managed.iter().map(String::as_str)),
        managed,
        identity: ctx.identity(),
        conditions: args.for_conditions.as_deref(),
        encrypt: args.encrypt,
        resumed: BTreeMap::new(),
    };
    if args.encrypt {
        targets.find_interrupted(&ctx, &plan);
    }
    refuse_collisions(&targets.sources, &plan)?;
    targets.check(&ctx, &repo_config, &plan)?;
    let public = remote_is_public(&ctx, &mut state, dry_run)?;
    let guarded = guard_secrets(
        &mut plan,
        &repo_config,
        &targets.sources,
        targets.identity,
        public,
        args,
    )?;
    guarded.warn(public, &ctx.paths.home, args.for_conditions.as_deref());
    let specials = warn_held(&plan);
    let outcome = if guarded.held.is_empty() && specials == 0 {
        Outcome::Done
    } else {
        Outcome::Attention
    };

    let settled = drop_unchanged(&ctx, &repo_config, &state, &targets.sources, &mut plan);
    for dir in plan.dirs.iter().filter(|dir| !settled.contains(&dir.key)) {
        preview(dir);
    }
    let in_dirs: usize = plan.dirs.iter().map(|d| d.items.len()).sum();
    if dry_run {
        let (items, announced) = plan.items();
        let failed = targets.try_all(&ctx, &items, &announced);
        if in_dirs > 0 {
            output::info(&format!("would add {in_dirs} files from directories"));
        }
        return finish(failed, items.len(), outcome);
    }

    if in_dirs > 0 && !args.yes && prompt::interactive() {
        let question = format!("add {in_dirs} files? [y/N]");
        if !prompt::confirm(&question)? {
            output::info("directories not added");
            plan.dirs.clear();
        }
    }

    let (items, announced) = plan.items();
    for dir in &plan.dirs {
        if !repo_config.dirs.paths.contains(&dir.key) {
            repo_config.dirs.paths.push(dir.key.clone());
        }
    }
    let (added, failed) =
        targets.commit_all(&ctx, &mut repo_config, &mut state, &items, &announced);
    // The record names the set every `{age}` file is encrypted to; one that exists may mark a
    // re-encryption that is still due, so only a missing one is created.
    if args.encrypt && added > 0 && secrets::read_record(&ctx.paths.repo)?.is_none() {
        secrets::write_record(&ctx.paths.repo, &repo_config.age.recipients)?;
    }

    if state.identity.is_none() {
        state.identity = Some(targets.identity.clone());
    }
    repo_config.save(&ctx.repo_config_file(), false)?;
    state.save(&ctx.paths.state_file(), false)?;
    if failed == 0 {
        output::success(&format!("added {}", crate::import::count(added, "path")));
    }
    finish(failed, items.len(), outcome)
}

/// Takes the files a directory already manages, unchanged, out of its plan, so that adding it
/// again previews and adds only what changed. Returns the directories left with nothing to add.
fn drop_unchanged(
    ctx: &Context,
    repo_config: &RepoConfig,
    state: &State,
    sources: &Sources,
    plan: &mut Plan,
) -> BTreeSet<String> {
    let mut settled = BTreeSet::new();
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);
    let variants = Variants::new(
        worktree,
        sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    for dir in &mut plan.dirs {
        let before = dir.items.len();
        dir.items.retain(|key, _| {
            !sources.contains(key)
                || !matches!(classify_key(ctx, &variants, state, key),
                    Ok(Classified::Item(item)) if item.token() == "clean")
        });
        let same = before.saturating_sub(dir.items.len());
        if same > 0 {
            output::info(&format!(
                "{}/: {} already managed and unchanged",
                dir.key,
                crate::import::count(same, "file")
            ));
            if dir.items.is_empty() {
                settled.insert(dir.key.clone());
            }
        }
    }
    settled
}

/// Lists the sockets, FIFOs, devices and unreadable entries the directories hold back, which
/// `[dirs]` would hold at every sync too. Returns how many.
fn warn_held(plan: &Plan) -> usize {
    let mut held = 0usize;
    for (key, hold) in plan.dirs.iter().flat_map(|dir| &dir.held) {
        output::warning(&format!(
            "held {key}: {}; an [ignore] pattern in .dotman.toml silences it",
            hold.reason()
        ));
        held = held.saturating_add(1);
    }
    held
}

/// A dry run ends here too, so it exits as the real run would.
fn finish(failed: usize, total: usize, outcome: Outcome) -> Result<Outcome> {
    if failed > 0 {
        bail!(
            "{failed} of {} could not be added",
            crate::import::count(total, "path")
        );
    }
    Ok(outcome)
}

/// Whether `origin` is known to be public. A remote with no stored result is checked first, and
/// the result saved at once, because a refusal below ends the run before the state is written.
fn remote_is_public(ctx: &Context, state: &mut State, dry_run: bool) -> Result<bool> {
    let Some(origin) = ctx.git.remote_url("origin")? else {
        return Ok(false);
    };
    if !dry_run && remote::needs_check(state, &origin) {
        remote::refresh(ctx, state, &origin);
        state.save(&ctx.paths.state_file(), false)?;
    }
    Ok(remote::is_public(state, &origin))
}

/// What the `[guard] secret_names` check left of a plan.
#[derive(Debug, Default)]
struct Guarded {
    /// New entries that a directory brought in, taken out of its plan.
    held: Vec<(String, Leaf)>,
    /// Files named explicitly that go in as plain text.
    plain: Vec<String>,
}

/// `key` as a path to paste into a shell.
fn pasteable(home: &Path, key: &str) -> String {
    shell_words::quote(&home.join(key).to_string_lossy()).into_owned()
}

impl Guarded {
    /// `conditions` is the `--for` of this run, which an `--encrypt` hint has to repeat.
    fn warn(&self, public: bool, home: &Path, conditions: Option<&str>) {
        let bin = name::bin();
        let keep = if public { "--allow-plain " } else { "" };
        for (key, leaf) in &self.held {
            let path = pasteable(home, key);
            let hint = match leaf {
                // --encrypt takes regular files only.
                Leaf::Link => format!("`{bin} add {keep}{path}` stores the link as it is"),
                Leaf::File { .. } => format!(
                    "`{bin} add --encrypt {path}` stores it encrypted, `{bin} add {keep}{path}` stores it as it is"
                ),
            };
            output::warning(&format!(
                "held {key}: name matches [guard] secret_names; {hint}"
            ));
        }
        let (verb, which) = match self.plain.as_slice() {
            [] => return,
            [one] => ("matches [guard] secret_names and is", pasteable(home, one)),
            _ => ("match [guard] secret_names and are", "<file>".to_owned()),
        };
        let variant = conditions.map_or_else(String::new, |conditions| {
            format!("--for {} ", shell_words::quote(conditions))
        });
        output::warning(&format!(
            "{} {verb} stored as plain text; `{bin} add --encrypt {variant}{which}` stores it encrypted instead",
            self.plain.join(", ")
        ));
    }
}

/// A secret-named file goes in only when named explicitly or with `--allow-plain`, so a directory
/// holds back each new one, whatever the remote. With a public remote, an explicit one also needs
/// `--encrypt` or `--allow-plain`. A file counts as managed only when this machine applies a
/// source for it: another host's `{age}` variant must not let a plain copy in.
fn guard_secrets(
    plan: &mut Plan,
    repo_config: &RepoConfig,
    managed: &Sources,
    identity: &IdentitySnapshot,
    public: bool,
    args: &AddArgs,
) -> Result<Guarded> {
    let secrets = secret_patterns(repo_config)?;
    let named = |key: &str| secrets.iter().any(|p| p.matches_with(key, MATCH_SECRET));
    let explicit: Vec<String> = plan
        .files
        .keys()
        .filter(|key| named(key))
        .cloned()
        .collect();
    let is_link = |key: &String| matches!(plan.files.get(key), Some(Leaf::Link));
    if public && !args.encrypt && !args.allow_plain && !explicit.is_empty() {
        let fix = if explicit.iter().all(is_link) {
            "pass --allow-plain to store the links as they are"
        } else {
            "pass --encrypt to store them encrypted, or --allow-plain to store them as they are"
        };
        bail!(
            "{} match [guard] secret_names and the remote is public; {fix}",
            explicit.join(", ")
        );
    }
    let mut guarded = Guarded::default();
    if !args.encrypt && !args.allow_plain {
        // A link stores where it points, not the secret, so it gets no warning.
        guarded.plain = explicit
            .iter()
            .filter(|key| !is_link(key))
            .cloned()
            .collect();
    }
    if !args.allow_plain {
        for dir in &mut plan.dirs {
            dir.items.retain(|key, leaf| {
                let keep = managed.select(key, identity).winner().is_some() || !named(key);
                if !keep {
                    guarded.held.push((key.clone(), *leaf));
                }
                keep
            });
        }
    }
    Ok(guarded)
}

struct Targets<'a> {
    managed: Vec<String>,
    sources: Sources,
    identity: &'a IdentitySnapshot,
    conditions: Option<&'a str>,
    encrypt: bool,
    /// Keys whose interrupted `add --encrypt` this run completes, and their `{age}` source.
    resumed: BTreeMap<String, String>,
}

impl Targets<'_> {
    /// An interrupted `add --encrypt` leaves the plain source beside its `{age}` one. When the
    /// ciphertext already holds what `$HOME` has, the rerun only has to remove the plain source.
    fn find_interrupted(&mut self, ctx: &Context, plan: &Plan) {
        for key in plan.files.keys() {
            let Selection::Ambiguous(tied) = self.sources.select(key, self.identity) else {
                continue;
            };
            let [a, b] = tied.as_slice() else {
                continue;
            };
            let pair = [(a, b), (b, a)].into_iter().find(|(plain, age)| {
                self.sources.flag(key, plain) == Flag::Plain
                    && variant::with_age(plain).is_ok_and(|source| source == **age)
            });
            let Some((_, age)) = pair else {
                continue;
            };
            let done = matches!(read_node(&ctx.paths.home, key), Ok(Node::File { bytes, .. })
                if ctx.age().decrypt(&ctx.paths.repo.join(age)).is_ok_and(|plain| plain == bytes));
            if done {
                self.resumed.insert(key.clone(), age.clone());
            }
        }
    }

    fn check(&self, ctx: &Context, repo_config: &RepoConfig, plan: &Plan) -> Result<()> {
        if self.encrypt {
            if !plan.dirs.is_empty() {
                bail!("--encrypt takes files, not directories");
            }
            if let Some(key) = plan.files.iter().find_map(|(key, leaf)| match leaf {
                Leaf::Link => Some(key),
                Leaf::File { .. } => None,
            }) {
                bail!("{key} is a symlink; --encrypt takes regular files");
            }
            if secrets::recipient_set(&repo_config.age.recipients).is_empty() {
                bail!(secrets::no_recipients());
            }
            ctx.age().require_cli()?;
            for key in plan.files.keys() {
                self.source_for(key)?;
            }
        }
        if let Some(conditions) = self.conditions {
            if !plan.dirs.is_empty() {
                bail!("--for takes files and symlinks, not directories");
            }
            for key in plan.files.keys() {
                variant::variant_source(key, conditions)?;
            }
        }
        Ok(())
    }

    fn commit_all(
        &self,
        ctx: &Context,
        repo_config: &mut RepoConfig,
        state: &mut State,
        items: &BTreeMap<String, Leaf>,
        announced: &BTreeSet<String>,
    ) -> (usize, usize) {
        let (mut added, mut failed) = (0usize, 0usize);
        let mut dir_modes = BTreeMap::new();
        for (key, leaf) in items {
            if !announced.contains(key) && self.is_template(key) {
                output::warning(&format!("{key}: rendered from a template; skipped"));
                continue;
            }
            match self.commit(ctx, repo_config, state, &mut dir_modes, key, leaf.kind()) {
                Ok(source) => {
                    added = added.saturating_add(1);
                    if announced.contains(key) {
                        output::action("add", &shown(key, &source));
                    }
                }
                Err(e) => {
                    failed = failed.saturating_add(1);
                    output::warning(&format!("could not add {key}: {e:#}"));
                }
            }
        }
        (added, failed)
    }

    /// [`Targets::commit_all`] without writing: what a real run would add, announced, and the
    /// number it would fail to add.
    fn try_all(
        &self,
        ctx: &Context,
        items: &BTreeMap<String, Leaf>,
        announced: &BTreeSet<String>,
    ) -> usize {
        let mut failed = 0usize;
        for (key, leaf) in items {
            let explicit = announced.contains(key);
            if !explicit && self.is_template(key) {
                output::warning(&format!("{key}: rendered from a template; skipped"));
                continue;
            }
            let checked = self.source_for(key).and_then(|source| {
                let encrypted = self.encrypt || self.sources.flag(key, &source) == Flag::Age;
                let plain_file = !encrypted && matches!(leaf, Leaf::File { .. });
                if plain_file && holds_key(&ctx.paths.home, key) {
                    return Err(HoldsKey.into());
                }
                worktree_parents(&ctx.paths.repo, &source, false)?;
                Ok(source)
            });
            match checked {
                Ok(source) if explicit => {
                    output::action("would add", &shown(key, &source));
                    if let Some(plain) = self.replaced(key, &source) {
                        output::warning(&format!(
                            "{plain} holds {key} in plain text and would be removed, but git history would still have it"
                        ));
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    failed = failed.saturating_add(1);
                    output::warning(&format!("could not add {key}: {e:#}"));
                }
            }
        }
        failed
    }

    fn source_for(&self, key: &str) -> Result<String> {
        let source = self.plain_source_for(key)?;
        if self.encrypt {
            variant::with_age(&source)
        } else {
            Ok(source)
        }
    }

    // Capture goes into the source this machine applies, which may be a variant.
    fn plain_source_for(&self, key: &str) -> Result<String> {
        if let Some(conditions) = self.conditions {
            return variant::variant_source(key, conditions);
        }
        if let Some(source) = self.resumed.get(key) {
            return Ok(source.clone());
        }
        match self.sources.select(key, self.identity) {
            Selection::None => Ok(key.to_owned()),
            Selection::Winner(source) if self.sources.flag(key, source) == Flag::Tmpl => bail!(
                "{source} is a template, so the file cannot replace it; edit the template with `{} edit {key}`",
                name::bin()
            ),
            Selection::Winner(source) => Ok(source.to_owned()),
            Selection::Ambiguous(tied) => bail!(
                "{} are equally specific here; remove one of them from the repo first, or stop managing the file with `{} forget ~/{key}`",
                tied.join(", "),
                name::bin()
            ),
            Selection::Invalid(problems) => bail!(
                "{}",
                problems
                    .iter()
                    .map(|(source, reason)| format!("{source}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }

    fn is_template(&self, key: &str) -> bool {
        self.sources
            .select(key, self.identity)
            .winner()
            .is_some_and(|source| self.sources.flag(key, source) == Flag::Tmpl)
    }

    fn commit(
        &self,
        ctx: &Context,
        repo_config: &mut RepoConfig,
        state: &mut State,
        dir_modes: &mut BTreeMap<String, u32>,
        key: &str,
        kind: Kind,
    ) -> Result<String> {
        let source = self.source_for(key)?;
        let encrypted = self.encrypt || self.sources.flag(key, &source) == Flag::Age;
        let entry = commit_one(ctx, repo_config, dir_modes, key, &source, kind, encrypted)?;
        let replaced = self.replaced(key, &source);
        if let Some(plain) = &replaced {
            let path = ctx.paths.repo.join(plain);
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("cannot remove {}", path.display()));
                }
                _ => {}
            }
            repo_config.modes.remove(plain);
            output::warning(&format!(
                "{plain} held {key} in plain text and is removed, but git history still has it; to purge it, rewrite the history with a tool such as git filter-repo, run in the repo through `{} git`",
                name::bin()
            ));
        }
        if self.applies(key, &source, replaced.as_deref()) {
            state
                .files
                .insert(key.to_owned(), entry.tuple.entry(&source));
        } else {
            output::info(&format!(
                "{key}: this machine does not apply {source}, so it keeps its current source"
            ));
        }
        Ok(source)
    }

    /// With `--encrypt`, the plain source that `source` encrypts, so the two never coexist.
    fn replaced(&self, key: &str, source: &str) -> Option<String> {
        if !self.encrypt {
            return None;
        }
        self.sources
            .sources_of(key)
            .find(|path| {
                *path != source
                    && self.sources.flag(key, path) == Flag::Plain
                    && variant::with_age(path).is_ok_and(|encrypted| encrypted == source)
            })
            .map(str::to_owned)
    }

    // A `--for` variant may be meant for other machines; S follows only what this one applies.
    fn applies(&self, key: &str, source: &str, removed: Option<&str>) -> bool {
        if self.conditions.is_none() {
            return true;
        }
        let after = self
            .managed
            .iter()
            .map(String::as_str)
            .filter(|path| *path != source && Some(*path) != removed)
            .chain([source]);
        Sources::group(after).select(key, self.identity).winner() == Some(&source)
    }
}

fn shown(key: &str, source: &str) -> String {
    if key == source {
        key.to_owned()
    } else {
        format!("{key} as {source}")
    }
}

/// Why a new file waits for an explicit `dotman add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hold {
    Large,
    Binary,
    SecretName,
    SecretKey,
    Special,
    Unreadable,
    Collision,
}

impl Hold {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Large => "over 1 MiB",
            Self::Binary => "binary: NUL in the first 8 KiB",
            Self::SecretName => "name matches [guard] secret_names",
            Self::SecretKey => "holds an age secret key (AGE-SECRET-KEY-)",
            Self::Special => "a socket, FIFO or device, which dotman never stores",
            Self::Unreadable => "cannot be read",
            Self::Collision => "differs only in case or Unicode normalization from another path",
        }
    }
}

/// Candidates under the `[dirs]` entries that no key and no ignore pattern covers.
#[derive(Default)]
pub(crate) struct Scan {
    pub new: BTreeMap<String, Kind>,
    pub held: BTreeMap<String, Hold>,
    /// Dangling links and names that cannot be keys: reported, never captured.
    pub notes: Vec<String>,
}

/// The `[guard] secret_names` patterns, compiled.
pub(crate) fn secret_patterns(repo_config: &RepoConfig) -> Result<Vec<Pattern>> {
    repo_config
        .guard
        .secret_names
        .iter()
        .map(|text| {
            Pattern::new(text)
                .with_context(|| format!("bad [guard] pattern `{text}` in .dotman.toml"))
        })
        .collect()
}

/// The holds that need only the name and size of a new file. A name that matches `secrets` wins
/// over everything else.
pub(crate) fn hold_by_name_and_size(secrets: &[Pattern], key: &str, size: u64) -> Option<Hold> {
    if secrets.iter().any(|p| p.matches_with(key, MATCH_SECRET)) {
        Some(Hold::SecretName)
    } else {
        (size > HOLD_BYTES).then_some(Hold::Large)
    }
}

/// The holds that need the bytes of a file that passed [`hold_by_name_and_size`].
pub(crate) fn hold_by_content(bytes: &[u8]) -> Option<Hold> {
    let sniffed = usize::try_from(SNIFF_BYTES).unwrap_or(8192);
    if bytes.iter().take(sniffed).any(|b| *b == 0) {
        Some(Hold::Binary)
    } else {
        secrets::holds_secret_key(bytes).then_some(Hold::SecretKey)
    }
}

/// Walks every `[dirs]` entry with `add <dir>`'s walker. `known` is every key that has an R or an S.
pub(crate) fn scan_dirs(
    ctx: &Context,
    repo_config: &RepoConfig,
    known: &BTreeSet<String>,
) -> Result<Scan> {
    let secrets = secret_patterns(repo_config)?;
    let planner = Planner {
        ctx,
        ignore: Ignore::compile(&repo_config.ignore.patterns)?,
        cwd: PathBuf::new(),
        out: Plan::default(),
    };
    let mut found: BTreeMap<String, Leaf> = BTreeMap::new();
    let mut dangling = BTreeSet::new();
    let mut scan = Scan::default();
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
        scan.held.extend(walked.held);
        scan.notes.extend(walked.notes);
    }
    found.retain(|key, _| !known.contains(key));
    dangling.retain(|key| !known.contains(key));
    scan.held.retain(|key, _| !known.contains(key));
    scan.notes
        .extend(dangling.into_iter().map(|key| format!("{key}: {DANGLING}")));

    let colliding = find_collisions(
        known
            .iter()
            .map(String::as_str)
            .chain(found.keys().map(String::as_str)),
    );
    for key in colliding.iter().flat_map(|(a, b)| [a, b]) {
        if found.remove(key).is_some() {
            scan.held.insert(key.clone(), Hold::Collision);
        }
    }
    for (key, leaf) in found {
        let hold = match leaf {
            Leaf::File { size, binary, .. } => hold_by_name_and_size(&secrets, &key, size)
                .or_else(|| binary.then_some(Hold::Binary))
                .or_else(|| holds_key(&ctx.paths.home, &key).then_some(Hold::SecretKey)),
            Leaf::Link => secrets
                .iter()
                .any(|p| p.matches_with(&key, MATCH_SECRET))
                .then_some(Hold::SecretName),
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
            bail!(
                "{key}: names starting with {TEMP_PREFIX} are dotman's temporary files, which are never managed; rename it to add it"
            );
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
                if !key.rsplit('/').next().is_some_and(variant::is_literal) {
                    bail!("{key}: {BLOCK_NAME}");
                }
                if matches!(leaf, Leaf::Link) {
                    self.link_to_dir(&key);
                }
                self.out.files.insert(key, leaf);
            }
            Found::Dir => {
                let dir = self.walk(&key);
                self.out.dirs.push(dir);
            }
            Found::Dangling => bail!("{key}: {DANGLING}, which dotman does not store"),
            Found::Held(Hold::Special, reason) => {
                bail!("{key}: {reason}; dotman stores regular files, symlinks and directories")
            }
            Found::Held(_, reason) | Found::Skip(reason) => bail!("{key}: {reason}"),
        }
        Ok(())
    }

    // Stored as a link, so the files behind it stay unmanaged.
    fn link_to_dir(&self, key: &str) {
        let paths = &self.ctx.paths;
        let Ok(target) = fs::canonicalize(paths.home.join(key)) else {
            return;
        };
        if !is_real_dir(&target) {
            return;
        }
        let inside = std::iter::once(paths.home.as_path())
            .chain(paths.home_real.as_deref())
            .find_map(|root| target.strip_prefix(root).ok())
            .filter(|rel| !rel.as_os_str().is_empty());
        let hint = inside.map_or_else(String::new, |rel| {
            format!(
                "; to manage its files, add the directory: `{} add {}`",
                name::bin(),
                pasteable(&paths.home, &rel.to_string_lossy())
            )
        });
        output::warning(&format!(
            "{key} is a symlink to the directory {}: only the link is stored, not the files in it{hint}",
            target.display()
        ));
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
                Err(e) => Found::Held(Hold::Unreadable, format!("unreadable: {e}")),
            }
        } else {
            Found::Held(Hold::Special, "socket, FIFO or device".to_owned())
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
                    let at = e.path().unwrap_or_else(|| Path::new(""));
                    let line = printable(&format!("{}: unreadable: {e}", at.display()));
                    match self.entry_key(at, true) {
                        Ok(key) => {
                            dir.held.insert(key, Hold::Unreadable);
                        }
                        Err(Left::Quiet | Left::Own(_)) => {}
                        Err(Left::Name(_)) => dir.notes.push(line.clone()),
                    }
                    dir.skipped.push(line);
                    continue;
                }
            };
            let is_dir = entry.file_type().is_dir();
            let name = entry.file_name().to_string_lossy();
            let skip_silently = name.starts_with(TEMP_PREFIX) || name == ".git";
            let verdict = if skip_silently {
                Err(Left::Quiet)
            } else {
                self.entry_key(entry.path(), is_dir)
            };
            match verdict {
                Err(left) => {
                    match left {
                        Left::Quiet => {}
                        Left::Own(line) => dir.skipped.push(line),
                        Left::Name(line) => {
                            dir.notes.push(line.clone());
                            dir.skipped.push(line);
                        }
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
                    Found::Held(hold, reason) => {
                        dir.skipped.push(format!("{key}: {reason}"));
                        dir.held.insert(key, hold);
                    }
                    Found::Skip(reason) => {
                        let line = format!("{key}: {reason}");
                        dir.notes.push(line.clone());
                        dir.skipped.push(line);
                    }
                    Found::Dir => {}
                },
            }
        }
        dir
    }

    fn entry_key(&self, path: &Path, is_dir: bool) -> std::result::Result<String, Left> {
        let paths = &self.ctx.paths;
        let named = |text: String| Left::Name(printable(&text));
        let rel = path
            .strip_prefix(&paths.home)
            .map_err(|e| named(format!("{}: {e}", path.display())))?;
        // Before the name checks, so an ignored entry is never reported, whatever its name; a
        // name that is not UTF-8 matches in its lossy form.
        if self.ignore.matches(&rel.to_string_lossy(), is_dir) {
            return Err(Left::Quiet);
        }
        if let Err(e) = refuse_non_nfc(rel) {
            return Err(named(format!("{e}")));
        }
        let key = rel
            .to_str()
            .ok_or_else(|| named(format!("{} is not valid UTF-8", path.display())))?;
        validate_key(key).map_err(|e| named(format!("{key}: {e}")))?;
        if !is_dir && !key.rsplit('/').next().is_some_and(variant::is_literal) {
            return Err(named(format!("{key}: {BLOCK_NAME}")));
        }
        if paths.refuse_protected(path).is_err() {
            return Err(Left::Own(format!(
                "{key}: dotman's own files are never managed"
            )));
        }
        Ok(key.to_owned())
    }
}

/// Scan notes reach the terminal, so a name's control characters are escaped: a newline would
/// forge a line of output, an ESC would drive the terminal.
fn printable(text: &str) -> String {
    text.chars().fold(String::new(), |mut out, c| {
        if c.is_control() {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
        out
    })
}

// An unreadable file is not held here; capturing it fails and is reported then.
pub(crate) fn holds_key(home: &Path, key: &str) -> bool {
    matches!(read_node(home, key), Ok(Node::File { bytes, .. }) if secrets::holds_secret_key(&bytes))
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

fn refuse_collisions(existing: &Sources, plan: &Plan) -> Result<()> {
    let new: BTreeSet<&str> = plan
        .files
        .keys()
        .chain(plan.dirs.iter().flat_map(|d| d.items.keys()))
        .map(String::as_str)
        .collect();
    let pairs = find_collisions(existing.keys().chain(new.iter().copied()));
    match pairs
        .iter()
        .find(|(a, b)| new.contains(a.as_str()) || new.contains(b.as_str()))
    {
        Some((a, b)) => bail!("{a} and {b} differ only in case or normalization"),
        None => Ok(()),
    }
}

pub(crate) fn preview(dir: &DirPlan) {
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

/// Copies H of `key` into the repo at `source`, which is `key` or one of its variants, and records
/// the mode under `source`, so each variant keeps its own. An `encrypted` source gets ciphertext,
/// and a plain one holding an age identity is refused with [`HoldsKey`]. `expected` is the kind
/// the caller saw, and a different kind now is an error. The caller records S and saves
/// `repo_config`.
pub(crate) fn commit_one(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    dir_modes: &mut BTreeMap<String, u32>,
    key: &str,
    source: &str,
    expected: Kind,
    encrypted: bool,
) -> Result<SourceEntry> {
    validate_key(source)?;
    let home = &ctx.paths.home;
    let repo = &ctx.paths.repo;
    let target = repo.join(source);
    let node = read_node(home, key)?;
    if let Node::File { bytes, .. } = &node
        && !encrypted
        && secrets::holds_secret_key(bytes)
    {
        return Err(HoldsKey.into());
    }
    ensure_worktree_parents(repo, source)?;
    let plaintext = match (expected, node) {
        (Kind::File, Node::File { bytes, mode, .. }) if encrypted => {
            store_encrypted(ctx, &repo_config.age.recipients, source, &bytes)?;
            keep_private(repo_config, key, source, safe_mode(mode))?;
            Some(SourceEntry {
                source: source.to_owned(),
                tuple: Tuple::of_file(&bytes, safe_mode(mode)),
                content: Content::Bytes(bytes),
                template: false,
                encrypted: true,
            })
        }
        (Kind::File, Node::File { bytes, mode, .. }) => {
            let stored = if mode & 0o111 == 0 { 0o644 } else { 0o755 };
            write_atomic(&target, &bytes, stored)?;
            record_mode(repo_config, source, mode & 0o777, stored);
            None
        }
        (Kind::Symlink, Node::Symlink(_)) if encrypted => {
            bail!("an encrypted file must be a regular file, not a symlink")
        }
        (Kind::Symlink, Node::Symlink(link)) => {
            let relative = portable_link_target(&link, key, home, ctx.paths.home_real.as_deref());
            replace_with_symlink(&target, &relative)?;
            repo_config.modes.remove(source);
            None
        }
        _ => bail!("it changed while being added"),
    };
    // Machines differ in umask, so an entry, which may be another machine's, is never replaced or
    // removed here; otherwise each capture would flip it.
    for parent in parents(key) {
        let entry = format!("{parent}/");
        if repo_config.modes.contains_key(&entry) {
            continue;
        }
        let mode = dir_mode(dir_modes, home, parent)?;
        record_mode(repo_config, &entry, mode, 0o755);
    }
    if let Some(entry) = plaintext {
        return Ok(entry);
    }
    Worktree::new(repo, &repo_config.modes)
        .get(source)?
        .ok_or_else(|| anyhow!("{source} is not in the repo after being written"))
}

// A secret never loosens to group or other access through a capture: such a mode is not recorded,
// so R keeps its own and the next apply tightens H back (S records H, which makes it row 10). A
// looser mode already in `[modes]` was written by hand and is kept.
fn keep_private(repo_config: &mut RepoConfig, key: &str, source: &str, mode: u32) -> Result<()> {
    let applied = recorded_mode(&repo_config.modes, source)?.map_or(0o600, safe_mode);
    if mode & 0o077 != 0 && mode != applied {
        output::warning(&format!(
            "{key} is {mode:04o}; this encrypted file applies at {applied:04o}, so this copy is tightened on the next apply; to keep {mode:04o}, set it for {source} in [modes] in .dotman.toml"
        ));
    } else {
        record_mode(repo_config, source, mode, 0o600);
    }
    Ok(())
}

// age output differs on every run, so a source whose plaintext is unchanged keeps its ciphertext.
fn store_encrypted(
    ctx: &Context,
    recipients: &[String],
    source: &str,
    plaintext: &[u8],
) -> Result<()> {
    let repo = &ctx.paths.repo;
    let unchanged = matches!(read_node(repo, source)?, Node::File { .. })
        && ctx
            .age()
            .decrypt(&repo.join(source))
            .is_ok_and(|old| old == plaintext);
    if unchanged {
        return Ok(());
    }
    let ciphertext = ctx.age().encrypt(plaintext, recipients)?;
    secrets::write_ciphertext(repo, source, &ciphertext)
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

pub(crate) fn record_mode(repo_config: &mut RepoConfig, key: &str, mode: u32, default: u32) {
    if mode == default {
        repo_config.modes.remove(key);
    } else {
        repo_config
            .modes
            .insert(key.to_owned(), format!("{mode:04o}"));
    }
}

// A symlink in the worktree (a cloned repo can ship one) must not carry the write out of the repo.
pub(crate) fn ensure_worktree_parents(repo: &Path, key: &str) -> Result<()> {
    worktree_parents(repo, key, true)
}

/// A dry run checks the parents the way [`ensure_worktree_parents`] does, without creating any.
fn worktree_parents(repo: &Path, key: &str, create: bool) -> Result<()> {
    for parent in parents(key) {
        let path = repo.join(parent);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "{} in the repo is a symlink or a file, not a directory",
                path.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !create => return Ok(()),
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

struct Planned {
    key: String,
    rel: String,
    source: String,
    bytes: Vec<u8>,
    mode: u32,
    uid: u32,
    gid: u32,
    package_owned: bool,
}

/// `dotman add --system`: copies readable regular files outside `$HOME` into `.dotman/system`.
/// Exits 3 without writing anything when a root is missing and cannot be asked about.
fn add_system(ctx: &Context, args: &AddArgs) -> Result<Outcome> {
    if args.encrypt {
        bail!(
            "--encrypt cannot be used with --system: system scope does not encrypt; keep secrets as {{age}} files under $HOME"
        );
    }
    let dry_run = args.dry_run;
    let repo_id = ctx.repo_id()?;
    let mut repo_config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;
    let scope = ctx.system_scope();
    let sources = Sources::group(ctx.managed_system()?.keys().map(String::as_str));
    let cwd = std::env::current_dir().context("cannot read the current directory")?;

    let mut files = Vec::new();
    let mut unchecked = false;
    for input in &args.paths {
        let key = system_key(ctx, &scope, input, &cwd).with_context(|| {
            if input.is_absolute() {
                input.display().to_string()
            } else {
                format!(
                    "{} (relative, so resolved against the current directory)",
                    input.display()
                )
            }
        })?;
        let file = read_system_file(ctx, &scope, &sources, args, key)?;
        unchecked |= !file.checked;
        files.push(file.file);
    }
    if unchecked {
        output::info(
            "no pacman, dpkg or rpm found; cannot tell whether a package owns these files",
        );
    }
    let collisions = find_collisions(
        sources
            .keys()
            .chain(files.iter().map(|file| file.rel.as_str())),
    );
    if let Some((a, b)) = collisions
        .iter()
        .find(|(a, b)| files.iter().any(|f| f.rel == *a || f.rel == *b))
    {
        bail!("{a} and {b} differ only in case or normalization");
    }

    let missing: BTreeSet<String> = files
        .iter()
        .filter(|file| scope.standing(&file.key) == Standing::Off)
        .filter_map(|file| system::offered_root(&file.key, scope.macos()))
        .collect();
    if !missing.is_empty() && !settle_roots(ctx, &scope, &missing, args)? {
        return Ok(Outcome::Attention);
    }

    for file in &files {
        let shown_source = shown(&file.key, &format!("/{}", file.source));
        if dry_run {
            output::action("would add", &shown_source);
            continue;
        }
        let path = system::source_path(&file.source);
        ensure_worktree_parents(&ctx.paths.repo, &path)?;
        write_atomic(
            &ctx.paths.repo.join(&path),
            &file.bytes,
            system::stored_mode(file.mode),
        )?;
        system::record(&mut repo_config, &file.key, file.uid, file.gid, file.mode)?;
        if let Some(record) = repo_config.system.get_mut(&file.key) {
            record.package_owned = file.package_owned;
        }
        if applies(&sources, args, ctx.identity(), file) {
            state.system.insert(
                file.key.clone(),
                Tuple::of_file(&file.bytes, file.mode).entry(&path),
            );
        } else {
            output::info(&format!(
                "{}: this machine does not apply {path}, so it keeps its current source",
                file.key
            ));
        }
        output::action("add", &shown_source);
    }
    if dry_run {
        return Ok(Outcome::Done);
    }
    if state.identity.is_none() {
        state.identity = Some(ctx.identity().clone());
    }
    repo_config.save(&ctx.repo_config_file(), false)?;
    state.save(&ctx.paths.state_file(), false)?;
    output::success(&format!("added {} system files", files.len()));
    Ok(Outcome::Done)
}

fn system_key(ctx: &Context, scope: &system::Scope, input: &Path, cwd: &Path) -> Result<String> {
    let abs = absolute_input(input, cwd, &ctx.paths.home);
    let key = abs
        .to_str()
        .ok_or_else(|| anyhow!("it is not valid UTF-8"))?
        .to_owned();
    let paths = &ctx.paths;
    let in_home = std::iter::once(paths.home.as_path())
        .chain(paths.home_real.as_deref())
        .any(|root| abs.starts_with(root));
    if in_home {
        bail!("{key} is under $HOME; add it without --system");
    }
    refuse_non_nfc(&abs)?;
    system::rel(&key)?;
    if !key.rsplit('/').next().is_some_and(variant::is_literal) {
        bail!("{key}: {BLOCK_NAME}; system scope takes no {{tmpl}} or {{age}} either");
    }
    if let Standing::Refused(why) = scope.standing(&key) {
        bail!("{why}");
    }
    if system::offered_root(&key, scope.macos()).is_none() {
        bail!("{key} is not inside a directory that can be a root");
    }
    Ok(key)
}

struct Prepared {
    file: Planned,
    /// A package manager answered.
    checked: bool,
}

fn read_system_file(
    ctx: &Context,
    scope: &system::Scope,
    sources: &Sources,
    args: &AddArgs,
    key: String,
) -> Result<Prepared> {
    let unreadable = |why: &str| anyhow!("{key} {why}: {}", system::SECRETS_NOTE);
    let node = scope.node(&key).map_err(|e| {
        if system::is_unreadable(&e) {
            unreadable("cannot be read by you")
        } else {
            e
        }
    })?;
    let (bytes, mode, uid, gid) = match node {
        Node::File {
            bytes,
            mode,
            uid,
            gid,
            ..
        } => {
            if mode & 0o444 == 0 {
                return Err(unreadable("has no read permission"));
            }
            (bytes, mode & 0o777, uid, gid)
        }
        Node::Missing => bail!("{key} does not exist"),
        Node::BadParent(parent) => {
            bail!("/{parent} is a symlink or not a directory; dotman does not add through it")
        }
        Node::Dir => bail!("{key} is a directory; system scope takes regular files only"),
        Node::Symlink(_) => bail!("{key} is a symlink; system scope takes regular files only"),
        Node::Special => bail!("{key} is a socket, FIFO or device"),
    };
    if secrets::holds_secret_key(&bytes) {
        bail!("{key} holds an age secret key (AGE-SECRET-KEY-), and system scope does not encrypt");
    }

    let checked = match system::package_owner(&scope.disk(&key))? {
        Ownership::Package(owner) if !args.package_owned => bail!(
            "{key} belongs to a package ({owner}); add it with --package-owned to manage it anyway"
        ),
        Ownership::Package(_) | Ownership::Unowned => true,
        Ownership::NoChecker => false,
    };

    let rel = system::rel(&key)?.to_owned();
    let source = match args.for_conditions.as_deref() {
        Some(conditions) => variant::variant_source(&rel, conditions)?,
        None => match sources.select(&rel, ctx.identity()) {
            Selection::None => rel.clone(),
            Selection::Winner(source) if sources.flag(&rel, source) != Flag::Plain => {
                bail!("{source} is a template or encrypted file, which system scope does not take")
            }
            Selection::Winner(source) => source.to_owned(),
            Selection::Ambiguous(tied) => bail!(
                "{} are equally specific here; remove one first",
                tied.join(", ")
            ),
            Selection::Invalid(problems) => bail!(
                "{}",
                problems
                    .iter()
                    .map(|(source, reason)| format!("{source}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        },
    };
    Ok(Prepared {
        checked,
        file: Planned {
            key,
            rel,
            source,
            bytes,
            mode,
            uid,
            gid,
            package_owned: args.package_owned,
        },
    })
}

// A `--for` variant may be meant for other machines; S follows only what this one applies.
fn applies(sources: &Sources, args: &AddArgs, identity: &IdentitySnapshot, file: &Planned) -> bool {
    if args.for_conditions.is_none() {
        return true;
    }
    let after: Vec<&str> = sources
        .sources_of(&file.rel)
        .filter(|path| *path != file.source)
        .chain([file.source.as_str()])
        .collect();
    Sources::group(after).select(&file.rel, identity).winner() == Some(&file.source.as_str())
}

/// Adds the missing roots to the machine config when the user agrees. When it cannot ask, prints
/// the config line to add and returns false, having written nothing.
fn settle_roots(
    ctx: &Context,
    scope: &system::Scope,
    missing: &BTreeSet<String>,
    args: &AddArgs,
) -> Result<bool> {
    let wanted = missing.iter().cloned().collect::<Vec<_>>().join(", ");
    let config_file = &ctx.paths.config_file;
    let agreed = if args.add_root {
        true
    } else if prompt::interactive() && !args.dry_run {
        prompt::confirm(&format!(
            "{wanted} is not in [system] roots; add it to {}? [y/N]",
            config_file.display()
        ))?
    } else if prompt::interactive() {
        true
    } else {
        let mut roots: Vec<String> = scope
            .roots()
            .iter()
            .filter(|root| root.problem.is_none())
            .map(|root| root.shown.clone())
            .collect();
        roots.extend(missing.iter().cloned());
        let list = roots
            .iter()
            .map(|root| format!("\"{root}\""))
            .collect::<Vec<_>>()
            .join(", ");
        output::warning(&format!(
            "{wanted} is not in [system] roots, so system scope is off for it. Add it to {} and run again:",
            config_file.display()
        ));
        output::warning("[system]");
        output::warning(&format!("roots = [{list}]"));
        output::warning("or run again with --add-root");
        return Ok(false);
    };
    if !agreed {
        bail!("{wanted} is not in [system] roots, so nothing was added");
    }
    let mut machine = ctx.machine.clone();
    for root in missing {
        machine.system.roots.push(PathBuf::from(root));
    }
    machine.save(config_file, args.dry_run)?;
    output::info(&format!(
        "{} {wanted} to [system] roots in {}",
        if args.dry_run { "would add" } else { "added" },
        config_file.display()
    ));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> Leaf {
        Leaf::File {
            mode: 0o644,
            size: 1,
            binary: false,
        }
    }

    fn plan_of(files: &[&str], dir_items: &[&str]) -> Plan {
        Plan {
            files: files.iter().map(|k| ((*k).to_owned(), file())).collect(),
            dirs: vec![DirPlan {
                key: ".config/x".to_owned(),
                items: dir_items
                    .iter()
                    .map(|k| ((*k).to_owned(), file()))
                    .collect(),
                ..DirPlan::default()
            }],
        }
    }

    #[test]
    fn a_secret_named_link_is_never_pointed_at_encrypt() {
        let config = RepoConfig::default();
        let none = Sources::default();
        let mut plan = plan_of(&[], &[]);
        plan.files.insert(".env".to_owned(), Leaf::Link);
        let err = guard_secrets(
            &mut plan,
            &config,
            &none,
            &here(),
            true,
            &args(false, false),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("--allow-plain") && !err.contains("--encrypt"),
            "{err}"
        );
        let guarded = guard_secrets(
            &mut plan,
            &config,
            &none,
            &here(),
            false,
            &args(false, false),
        )
        .unwrap();
        assert!(
            guarded.plain.is_empty(),
            "a link gets no plain-text warning"
        );
    }

    fn args(encrypt: bool, allow_plain: bool) -> AddArgs {
        AddArgs {
            paths: Vec::new(),
            for_conditions: None,
            encrypt,
            allow_plain,
            yes: false,
            dry_run: false,
            system: false,
            package_owned: false,
            add_root: false,
        }
    }

    fn kept(plan: &Plan) -> Vec<&String> {
        plan.dirs.iter().flat_map(|d| d.items.keys()).collect()
    }

    fn here() -> IdentitySnapshot {
        IdentitySnapshot {
            host: "box".to_owned(),
            user: "u".to_owned(),
            os: "linux".to_owned(),
            distro: "arch".to_owned(),
            like: String::new(),
            arch: "x86_64".to_owned(),
            profiles: Vec::new(),
        }
    }

    fn held_keys(guarded: &Guarded) -> Vec<&str> {
        guarded.held.iter().map(|(key, _)| key.as_str()).collect()
    }

    #[test]
    fn a_directory_holds_a_new_secret_name_whatever_the_remote() {
        let config = RepoConfig::default();
        let me = here();
        let managed = Sources::group([".config/x/managed.token"]);
        let items = [".config/x/.env", ".config/x/rc", ".config/x/managed.token"];
        for public in [false, true] {
            let mut plan = plan_of(&[], &items);
            let guarded = guard_secrets(
                &mut plan,
                &config,
                &managed,
                &me,
                public,
                &args(false, false),
            )
            .unwrap();
            assert_eq!(held_keys(&guarded), [".config/x/.env"], "public: {public}");
            assert_eq!(kept(&plan), [".config/x/managed.token", ".config/x/rc"]);
        }
        let mut plan = plan_of(&[], &items);
        let guarded =
            guard_secrets(&mut plan, &config, &managed, &me, false, &args(false, true)).unwrap();
        assert_eq!(guarded.held.len(), 0);
        assert_eq!(kept(&plan).len(), 3);
    }

    #[test]
    fn another_machine_s_variant_does_not_make_a_secret_name_managed_here() {
        let config = RepoConfig::default();
        let me = here();
        let managed = Sources::group([".config/x/.env{host=laptop+age}"]);
        let mut plan = plan_of(&[], &[".config/x/.env", ".config/x/rc"]);
        let guarded = guard_secrets(
            &mut plan,
            &config,
            &managed,
            &me,
            false,
            &args(false, false),
        )
        .unwrap();
        assert_eq!(held_keys(&guarded), [".config/x/.env"]);
        assert_eq!(kept(&plan), [".config/x/rc"]);
    }

    #[test]
    fn an_explicit_secret_name_goes_in_as_plain_text_without_a_public_remote() {
        let config = RepoConfig::default();
        let me = here();
        let none = Sources::default();
        let mut plan = plan_of(&[".bashrc", ".env"], &[]);
        let guarded =
            guard_secrets(&mut plan, &config, &none, &me, false, &args(false, false)).unwrap();
        assert_eq!(guarded.plain, [".env"]);
        assert_eq!(plan.files.len(), 2);
        for flags in [args(true, false), args(false, true)] {
            let mut plan = plan_of(&[".env"], &[]);
            let guarded = guard_secrets(&mut plan, &config, &none, &me, true, &flags).unwrap();
            assert!(guarded.plain.is_empty() && guarded.held.is_empty());
        }
    }

    #[test]
    fn a_public_remote_refuses_an_explicit_secret_name() {
        let config = RepoConfig::default();
        let none = Sources::default();
        let mut plan = plan_of(&[".bashrc", ".ENV", ".aws/credentials"], &[]);
        let err = guard_secrets(
            &mut plan,
            &config,
            &none,
            &here(),
            true,
            &args(false, false),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(".ENV") && err.contains(".aws/credentials"),
            "{err}"
        );
        assert!(!err.contains(".bashrc"), "{err}");
        assert!(
            err.contains("--encrypt") && err.contains("--allow-plain"),
            "{err}"
        );
    }

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
