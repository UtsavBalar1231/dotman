//! `dotman add`: copy files and symlinks into the repo worktree and record their modes.
//!
//! Planning reads `$HOME` and changes nothing, so a refused path leaves the repo untouched.

use crate::cli::AddArgs;
use crate::commands::Outcome;
use crate::commands::apply::{Classified, classify_key};
use crate::config::{Allow, Config};
use crate::context::{Context, TEMP_PREFIX, is_real_dir, is_under};
use crate::engine::status::{Variants, Worktree};
use crate::engine::{
    Node, Tuple, parents, portable_link_target, read_node, recorded_mode, safe_mode,
};
use crate::name;
use crate::output;
use crate::paths::{
    OwnPath, absolute_input, find_collisions, refuse_non_nfc, shown_key, validate_key, write_atomic,
};
use crate::prompt;
use crate::remote;
use crate::repo_config::RepoConfig;
use crate::secrets::{self, Finding, Held};
use crate::state::{Entry, IdentitySnapshot, Kind, State};
use crate::system::{self, Ownership, Standing};
use crate::template;
use crate::variant::{self, Flag, Selection, Sources};
use anyhow::{Context as _, Result, anyhow, bail};
use glob::{MatchOptions, Pattern};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

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
    /// Matched only by a pattern `.dotman.toml` no longer lists, which this machine still applies.
    StillIgnored(String, StillIgnored),
}

/// An entry that a pattern `.dotman.toml` no longer lists keeps out on this machine.
pub(crate) struct StillIgnored {
    pub(crate) pattern: String,
    pub(crate) is_dir: bool,
}

impl StillIgnored {
    /// The pattern, and for a file what adds it; `path` is the entry as [`pasteable`] gives it.
    pub(crate) fn listed(&self, path: &str) -> String {
        if self.is_dir {
            format!("[ignore] {}", self.pattern)
        } else {
            format!(
                "[ignore] {}; `{} add {path}` adds it",
                self.pattern,
                name::bin()
            )
        }
    }
}

pub(crate) fn still_ignored_header() -> String {
    format!(
        "kept out by [ignore] patterns that .dotman.toml no longer lists and this machine still applies; `{} sync --drop-unlisted-ignores` stops applying them here and captures what they kept out:",
        name::bin()
    )
}

pub(crate) fn warn_still_ignored(home: &Path, found: &BTreeMap<String, StillIgnored>) {
    if found.is_empty() {
        return;
    }
    output::warning(&still_ignored_header());
    for (key, entry) in found {
        output::warning(&format!(
            "  {key}{} ({})",
            if entry.is_dir { "/" } else { "" },
            entry.listed(&pasteable(home, key))
        ));
    }
}

#[derive(Default)]
pub(crate) struct DirPlan {
    pub(crate) key: String,
    pub(crate) items: BTreeMap<String, Leaf>,
    pub(crate) skipped: Vec<String>,
    pub(crate) still_ignored: BTreeMap<String, StillIgnored>,
    pub(crate) dangling: Vec<String>,
    /// Sockets, FIFOs, devices and what cannot be read.
    pub(crate) held: BTreeMap<String, Hold>,
    /// Entries whose names cannot be keys.
    pub(crate) notes: Vec<String>,
    /// Files it already manages unchanged, taken out of `items`.
    pub(crate) unchanged: Vec<String>,
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

    /// The first pattern that matches, as `[ignore]` spells it, escaped onto one line: it comes
    /// from a tracked file and is only ever printed.
    fn matching(&self, key: &str, is_dir: bool) -> Option<String> {
        let any = self.any.iter().find(|p| p.matches_with(key, MATCH));
        let dir = || {
            self.dirs_only
                .iter()
                .find(|p| is_dir && p.matches_with(key, MATCH))
                .map(|p| format!("{}/", p.as_str()))
        };
        any.map(|p| p.as_str().to_owned())
            .or_else(dir)
            .map(|text| output::escape_line(&text).into_owned())
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
    let mut ctx = Context::resolve("add")?;
    let dry_run = args.dry_run;
    let _lock = ctx.lock(dry_run)?;
    if !dry_run {
        ctx.sweep_temp_files()?;
    }
    if args.system {
        return add_system(&ctx, args);
    }
    let repo_id = ctx.repo_id()?;
    let mut repo_config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;
    let identity = ctx.identity().clone();

    let mut plan = Planner::new(
        &ctx,
        &repo_config,
        &state,
        std::env::current_dir().context("cannot read the current directory")?,
    )?
    .plan(&args.paths)?;
    let managed: Vec<String> = ctx.managed()?.into_keys().collect();
    let mut targets = Targets {
        sources: Sources::group(managed.iter().map(String::as_str)),
        managed,
        identity: &identity,
        home: &ctx.paths.home,
        conditions: args.for_conditions.as_deref(),
        encrypt: args.encrypt,
        template: args.template,
        resumed: BTreeMap::new(),
    };
    if args.encrypt || args.template {
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
        |key| holds_key(&ctx.paths.home, key, &ctx.machine.guard.allow),
    )?;
    let settled = drop_unchanged(&ctx, &repo_config, &state, &targets.sources, &mut plan);
    let content = guard_content(&ctx, &mut plan, &targets, args)?;
    guarded.warn(&ctx, public, args.for_conditions.as_deref());
    warn_content_held(&ctx, &content.held);
    let specials = warn_held(&ctx, &plan, &targets.sources);
    for dir in &plan.dirs {
        warn_still_ignored(&ctx.paths.home, &dir.still_ignored);
    }
    let outcome = if guarded.held.is_empty() && content.held.is_empty() && specials == 0 {
        Outcome::Done
    } else {
        Outcome::Attention
    };

    for dir in plan.dirs.iter().filter(|dir| !settled.contains(&dir.key)) {
        preview(dir);
    }
    let in_dirs: usize = plan.dirs.iter().map(|d| d.items.len()).sum();
    let files = output::count(in_dirs, "file");
    if dry_run {
        let (items, announced) = plan.items();
        content.record(&mut ctx.machine, &ctx.paths.config_file, &items, true)?;
        let failed = targets.try_all(&ctx, &state, &items, &announced);
        if in_dirs > 0 {
            output::info(&format!("would add {files} from directories"));
        }
        return finish(failed, items.len(), outcome);
    }

    if in_dirs > 0 && !args.yes && prompt::interactive() {
        let question = format!("add {files}? [y/N]");
        if !prompt::confirm(&question)? {
            output::info("directories not added");
            plan.dirs.clear();
        }
    }

    let (items, announced) = plan.items();
    adopt_dirs(&ctx, &mut repo_config, &mut state, &plan.dirs)?;
    content.record(&mut ctx.machine, &ctx.paths.config_file, &items, false)?;
    let (added, failed) =
        targets.commit_all(&ctx, &mut repo_config, &mut state, &items, &announced)?;
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
            let changed = !sources.contains(key)
                || !matches!(classify_key(ctx, &variants, state, key),
                    Ok(Classified::Item(item)) if item.token() == "clean");
            if !changed {
                dir.unchanged.push(key.clone());
            }
            changed
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

/// Lists each directory in `[dirs]` and approves it on this machine, whose user named it, and
/// records the modes of the directories above the files it already manages unchanged, which no
/// capture records: adding it again is how a tightened directory mode reaches the repo.
fn adopt_dirs(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    state: &mut State,
    dirs: &[DirPlan],
) -> Result<()> {
    let mut modes = BTreeMap::new();
    state.approve_dirs(dirs.iter().map(|dir| &dir.key));
    for dir in dirs {
        if !repo_config.dirs.paths.contains(&dir.key) {
            repo_config.dirs.paths.push(dir.key.clone());
        }
        for key in &dir.unchanged {
            record_dirs(repo_config, &mut modes, &ctx.paths.home, key)?;
        }
    }
    Ok(())
}

fn warn_content_held(ctx: &Context, held: &[(String, Finding)]) {
    for (key, found) in held {
        let (name, path) = (shown_key(key), pasteable(&ctx.paths.home, key));
        output::warning(&format!(
            "held {name}: {found}{}",
            found.remedy(Some(&path))
        ));
    }
}

/// Lists the sockets, FIFOs, devices and unreadable entries the directories hold back, which
/// `[dirs]` would hold at every sync too. Returns how many. An `[ignore]` pattern keeps a managed
/// file managed, so for one of those the way out is `forget`.
fn warn_held(ctx: &Context, plan: &Plan, sources: &Sources) -> usize {
    let mut held = 0usize;
    for (key, hold) in plan.dirs.iter().flat_map(|dir| &dir.held) {
        let way_out = if sources.contains(key) {
            forget_hint(&ctx.paths.home, sources, key)
        } else {
            "an [ignore] pattern in .dotman.toml silences it".to_owned()
        };
        output::warning(&format!(
            "held {}: {}; {way_out}",
            shown_key(key),
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
    if !dry_run && remote::is_stale(state, &origin, remote::now()) {
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

const UNSCANNED: &str = "it is binary, which the content guard does not scan";

/// `key` as a path to paste into a shell.
pub(crate) fn pasteable(home: &Path, key: &str) -> String {
    shell_words::quote(&home.join(key).to_string_lossy()).into_owned()
}

/// The hint that `forget` stops managing `key`. It takes a key, never one source, so for a key
/// with more than one source the hint says it removes them all, other machines' variants too.
pub(crate) fn forget_hint(home: &Path, sources: &Sources, key: &str) -> String {
    let command = format!("`{} forget {}`", name::bin(), pasteable(home, key));
    let all: Vec<&str> = sources.sources_of(key).collect();
    if all.len() < 2 {
        return format!("{command} stops managing it");
    }
    format!(
        "{command} stops managing it on every machine and removes all {} of its repo files ({})",
        all.len(),
        all.join(", ")
    )
}

impl Guarded {
    /// `conditions` is the `--for` of this run, which an `--encrypt` hint has to repeat.
    fn warn(&self, ctx: &Context, public: bool, conditions: Option<&str>) {
        let (home, allow) = (&ctx.paths.home, &ctx.machine.guard.allow);
        let bin = name::bin();
        let keep = if public { "--allow-plain " } else { "" };
        for (key, leaf) in &self.held {
            let path = pasteable(home, key);
            let hint = match (leaf, holds_key(home, key, allow)) {
                (_, Some(found)) if found.place != secrets::Place::Content => {
                    format!(", and {found}{}", found.remedy(None))
                }
                // --encrypt takes regular files only.
                (Leaf::Link, _) => format!("; `{bin} add {keep}{path}` stores the link as it is"),
                (Leaf::File { binary: true, .. }, _) => {
                    format!(", and {UNSCANNED}; `{bin} add --encrypt {path}` stores it encrypted")
                }
                // Storing it as it is would put the credential in the repo.
                (Leaf::File { .. }, Some(found)) => format!(
                    ", and {found}; `{bin} add --encrypt {path}` stores it encrypted, or remove the secret from it, then add it"
                ),
                (Leaf::File { .. }, None) => format!(
                    "; `{bin} add --encrypt {path}` stores it encrypted, `{bin} add {keep}{path}` stores it as it is"
                ),
            };
            output::warning(&format!(
                "held {}: name matches [guard] secret_names{hint}",
                shown_key(key)
            ));
        }
        let (verb, which) = match self.plain.as_slice() {
            [] => return,
            [one] => ("matches [guard] secret_names and is", pasteable(home, one)),
            many => (
                "match [guard] secret_names and are",
                many.iter()
                    .map(|key| pasteable(home, key))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        };
        let variant = conditions.map_or_else(String::new, |conditions| {
            format!("--for {} ", shell_words::quote(conditions))
        });
        output::warning(&format!(
            "{} {verb} stored as plain text; `{bin} add --encrypt {variant}{which}` stores it encrypted instead",
            self.plain
                .iter()
                .map(|key| shown_key(key))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
}

/// A secret-named file goes in only when named explicitly or with `--allow-plain`, so a directory
/// holds back each new one, whatever the remote. With a public remote, an explicit one also needs
/// `--encrypt` or `--allow-plain`. A file counts as managed only when this machine applies a
/// source for it: another host's `{age}` variant must not let a plain copy in. `holds` is what the
/// content guard finds in a file.
fn guard_secrets(
    plan: &mut Plan,
    repo_config: &RepoConfig,
    managed: &Sources,
    identity: &IdentitySnapshot,
    public: bool,
    args: &AddArgs,
    holds: impl Fn(&str) -> Option<Finding>,
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
        let found = explicit
            .iter()
            .find_map(|key| holds(key).map(|found| (key, found)));
        let unscanned = explicit
            .iter()
            .find(|key| matches!(plan.files.get(*key), Some(Leaf::File { binary: true, .. })));
        // --allow-plain would publish the credential, or whatever the guard did not see.
        let fix = if explicit.iter().all(is_link) {
            "pass --allow-plain to store the links as they are".to_owned()
        } else if let Some((key, found)) = found {
            format!("{key}: {found}{}", found.remedy(None))
        } else if let Some(key) = unscanned {
            format!("{key}: {UNSCANNED}; --encrypt stores it encrypted")
        } else {
            "pass --encrypt to store them encrypted, or --allow-plain to store them as they are"
                .to_owned()
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

/// What the content guard left of a plan.
#[derive(Default)]
struct ContentGuard {
    /// Files a directory brought in, taken out of its plan.
    held: Vec<(String, Finding)>,
    /// What `--allow-plain` lets in, to record in `[guard] allow` once the files are added.
    allowed: Vec<(Allow, Finding)>,
}

impl ContentGuard {
    /// Adds the entries of the files that are still in `items` to `machine`'s `[guard] allow` and
    /// saves it to `file`; a dry run saves nothing.
    fn record(
        &self,
        machine: &mut Config,
        file: &Path,
        items: &BTreeMap<String, Leaf>,
        dry_run: bool,
    ) -> Result<()> {
        let kept: Vec<&(Allow, Finding)> = self
            .allowed
            .iter()
            .filter(|(entry, _)| items.contains_key(&entry.path))
            .collect();
        for (entry, found) in &kept {
            output::warning(&format!(
                "{}: {found}; stored as it is, as --allow-plain asks, and [guard] allow in {} {} it",
                entry.path,
                file.display(),
                if dry_run { "would record" } else { "records" }
            ));
            machine.guard.allow.push(entry.clone());
        }
        if kept.is_empty() {
            return Ok(());
        }
        machine.save(file, dry_run)
    }
}

/// The content guard, whatever the remote, over every file this run would store in plain text.
/// A named file that holds a credential is refused, and a directory holds each such file back.
/// `--allow-plain` lets in a named file only, to be recorded by rule in this machine's `[guard]
/// allow`: taking every credential a directory holds at once is too easy to do by mistake. An age
/// identity is never stored in plain text, whatever the flags.
fn guard_content(
    ctx: &Context,
    plan: &mut Plan,
    targets: &Targets,
    args: &AddArgs,
) -> Result<ContentGuard> {
    let home = &ctx.paths.home;
    let allow = &ctx.machine.guard.allow;
    let mut content = ContentGuard::default();
    // An unreadable file, or one whose source cannot be chosen, fails later with its own error.
    let mut check = |key: &str, leaf: &Leaf, named: bool| -> Option<Finding> {
        // Neither a flag nor an allow entry lets a name or a link target through.
        if let Some(found) = secrets::held_name(key) {
            return Some(found);
        }
        if matches!(leaf, Leaf::Link) {
            return holds_key(home, key, allow);
        }
        let source = targets.source_for(key).ok()?;
        if !matches!(leaf, Leaf::File { .. })
            || args.encrypt
            || targets.sources.flag(key, &source) == Flag::Age
        {
            return None;
        }
        let Ok(Node::File { bytes, .. }) = read_node(home, key) else {
            return None;
        };
        let found = secrets::held(&bytes, key, allow)?;
        if !named
            || !args.allow_plain
            || secrets::findings(&bytes).any(|found| !found.overridable())
        {
            return Some(found);
        }
        let mut rules = BTreeSet::new();
        for found in secrets::findings(&bytes) {
            if !found.allowed(key, allow) && rules.insert(found.rule) {
                let entry = Allow {
                    path: key.to_owned(),
                    rule: found.rule.to_owned(),
                };
                content.allowed.push((entry, found));
            }
        }
        None
    };
    let mut refused = Vec::new();
    for (key, leaf) in &plan.files {
        if let Some(found) = check(key, leaf, true) {
            refused.push(format!("{key}: {found}{}", found.remedy(None)));
        }
    }
    if let [one] = refused.as_slice() {
        bail!("{one}");
    }
    if !refused.is_empty() {
        for line in &refused {
            output::warning(line);
        }
        bail!(
            "{} files hold a credential or an age secret key, so nothing was added",
            refused.len()
        );
    }
    let mut held = Vec::new();
    for dir in &mut plan.dirs {
        dir.items.retain(|key, leaf| {
            let found = check(key, leaf, false);
            held.extend(found.map(|found| (key.clone(), found)));
            found.is_none()
        });
    }
    content.held = held;
    Ok(content)
}

struct Targets<'a> {
    managed: Vec<String>,
    sources: Sources,
    identity: &'a IdentitySnapshot,
    home: &'a Path,
    conditions: Option<&'a str>,
    encrypt: bool,
    template: bool,
    /// Keys whose interrupted `add --encrypt` or `add --template` this run completes, and their
    /// `{age}` or `{tmpl}` source.
    resumed: BTreeMap<String, String>,
}

impl Targets<'_> {
    /// An interrupted `add --encrypt` or `add --template` leaves the plain source beside its
    /// `{age}` or `{tmpl}` one. When that one already holds what `$HOME` has, the rerun only has to
    /// remove the plain source. A template that already renders to `$HOME` is finished too.
    fn find_interrupted(&mut self, ctx: &Context, plan: &Plan) {
        for key in plan.files.keys() {
            let tied = match self.sources.select(key, self.identity) {
                Selection::Ambiguous(tied) => tied,
                Selection::Winner(source)
                    if self.template && self.sources.flag(key, source) == Flag::Tmpl =>
                {
                    if matches!(read_node(&ctx.paths.home, key), Ok(Node::File { bytes, .. })
                        if self.holds(ctx, source, &bytes))
                    {
                        self.resumed.insert(key.clone(), source.to_owned());
                    }
                    continue;
                }
                _ => continue,
            };
            let [a, b] = tied.as_slice() else {
                continue;
            };
            let pair = [(a, b), (b, a)].into_iter().find(|(plain, stored)| {
                self.sources.flag(key, plain) == Flag::Plain
                    && self
                        .stored_form(plain)
                        .is_ok_and(|source| source == **stored)
            });
            let Some((_, stored)) = pair else {
                continue;
            };
            let done = matches!(read_node(&ctx.paths.home, key), Ok(Node::File { bytes, .. })
                if self.holds(ctx, stored, &bytes));
            if done {
                self.resumed.insert(key.clone(), stored.clone());
            }
        }
    }

    /// `plain` as `--encrypt` or `--template` stores it.
    fn stored_form(&self, plain: &str) -> Result<String> {
        if self.encrypt {
            variant::with_age(plain)
        } else {
            variant::with_tmpl(plain)
        }
    }

    /// Whether the stored source `source` gives `bytes` back: decrypted, or rendered here.
    fn holds(&self, ctx: &Context, source: &str, bytes: &[u8]) -> bool {
        if self.encrypt {
            return ctx
                .age()
                .decrypt(&ctx.paths.repo.join(source))
                .is_ok_and(|plain| plain == bytes);
        }
        renders_to(ctx, source, bytes)
    }

    fn check(&self, ctx: &Context, repo_config: &RepoConfig, plan: &Plan) -> Result<()> {
        if self.template {
            if !plan.dirs.is_empty() {
                bail!("--template takes files, not directories");
            }
            for (key, leaf) in &plan.files {
                match leaf {
                    Leaf::Link => bail!("{key} is a symlink; --template takes regular files"),
                    Leaf::File { binary: true, .. } => {
                        bail!("{key} is not text, so it cannot be a template")
                    }
                    Leaf::File { .. } => {}
                }
                if matches!(read_node(&ctx.paths.home, key), Ok(Node::File { bytes, .. })
                    if std::str::from_utf8(&bytes).is_err())
                {
                    bail!("{key} is not text, so it cannot be a template");
                }
                if let Some(encrypted) = self
                    .sources
                    .sources_of(key)
                    .find(|source| self.sources.flag(key, source) == Flag::Age)
                {
                    bail!(
                        "{encrypted} stores {key} encrypted, and a template is stored in plain text, so it cannot be a template"
                    );
                }
                self.source_for(key)?;
            }
        }
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
                // A plain variant would put in the repo, in plain text, what an `{age}` source
                // keeps encrypted, whichever machine either one is for.
                let encrypted = self
                    .sources
                    .sources_of(key)
                    .find(|source| self.sources.flag(key, source) == Flag::Age);
                if let (Some(encrypted), false) = (encrypted, self.encrypt) {
                    bail!(
                        "{encrypted} stores {key} encrypted, so a plain variant would put it in the repo in plain text; `{} add --encrypt --for {} {}` stores the variant encrypted",
                        name::bin(),
                        shell_words::quote(conditions),
                        pasteable(&ctx.paths.home, key)
                    );
                }
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
    ) -> Result<(usize, usize)> {
        let (mut added, mut failed) = (0usize, 0usize);
        let mut dir_modes = BTreeMap::new();
        // Every plain file's mode in one save before any content, rather than one save per file
        // in `commit_one`, which a directory of 0600 files would make quadratic. An encrypted
        // file without an entry applies at 0600, so it can wait for `commit_one`.
        let (home, mut changed) = (&ctx.paths.home, false);
        for (key, leaf) in items {
            let Ok(source) = self.source_for(key) else {
                continue;
            };
            let encrypted = self.encrypt || self.sources.flag(key, &source) == Flag::Age;
            let file_mode = match leaf {
                Leaf::File { mode, .. } if !encrypted => Some(*mode),
                _ => None,
            };
            // A failure here fails the key in `commit_one`; what it recorded first is saved.
            changed = match record_ahead(
                repo_config,
                &mut dir_modes,
                home,
                key,
                &source,
                file_mode,
                false,
            ) {
                Ok((more, _)) => changed || more,
                Err(_) => true,
            };
        }
        if changed {
            repo_config.save(&ctx.repo_config_file(), false)?;
        }
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
                        if self.template {
                            output::info(&format!(
                                "{} is now a template; `{} edit {}` opens it",
                                shown_key(key),
                                name::bin(),
                                pasteable(home, key)
                            ));
                        }
                    }
                }
                Err(e) => {
                    failed = failed.saturating_add(1);
                    output::warning(&format!("could not add {key}: {e:#}"));
                }
            }
        }
        Ok((added, failed))
    }

    /// [`Targets::commit_all`] without writing: what a real run would add, announced, and the
    /// number it would fail to add.
    fn try_all(
        &self,
        ctx: &Context,
        state: &State,
        items: &BTreeMap<String, Leaf>,
        announced: &BTreeSet<String>,
    ) -> usize {
        let allow = &ctx.machine.guard.allow;
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
                if plain_file && let Some(found) = holds_key(&ctx.paths.home, key, allow) {
                    return Err(Held(found).into());
                }
                self.refuse_unsaved(ctx, state, key, &source)?;
                if self.template
                    && let Ok(Node::File { bytes, .. }) = read_node(&ctx.paths.home, key)
                    && !renders_to(ctx, &source, &bytes)
                {
                    template_text(ctx, key, &source, &bytes, true)?;
                }
                worktree_parents(&ctx.paths.repo, &source, false)?;
                Ok(source)
            });
            match checked {
                Ok(source) if explicit => {
                    output::action("would add", &shown(key, &source));
                    if let Some(plain) = self.replaced(key, &source)
                        && !self.template
                    {
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
        if self.encrypt || self.template {
            self.stored_form(&source)
        } else {
            Ok(source)
        }
    }

    // Capture goes into the source this machine applies, which may be a variant.
    fn plain_source_for(&self, key: &str) -> Result<String> {
        if let Some(conditions) = self.conditions {
            let source = variant::variant_source(key, conditions)?;
            if secrets::held_name(key).is_none()
                && let Some(found) = secrets::held_name(&source)
            {
                bail!(
                    "the --for value holds a credential ({}), which would be in the repo's file name; give a different --for value",
                    found.rule
                );
            }
            return Ok(source);
        }
        if let Some(source) = self.resumed.get(key) {
            return Ok(source.clone());
        }
        match self.sources.select(key, self.identity) {
            Selection::None => Ok(key.to_owned()),
            Selection::Winner(source) if self.sources.flag(key, source) == Flag::Tmpl => bail!(
                "{source} is a template, so the file cannot replace it; edit the template with `{} edit {}`",
                name::bin(),
                pasteable(self.home, key)
            ),
            Selection::Winner(source) => Ok(source.to_owned()),
            Selection::Ambiguous(tied) => bail!(
                "{} are equally specific here; remove one of them from the repo first, or stop managing the file with `{} forget {}`",
                tied.join(", "),
                name::bin(),
                pasteable(self.home, key)
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
        let store = if self.template {
            Store::Template
        } else if self.encrypt || self.sources.flag(key, &source) == Flag::Age {
            Store::Encrypted
        } else {
            Store::Plain
        };
        self.refuse_unsaved(ctx, state, key, &source)?;
        let entry = commit_stored(ctx, repo_config, dir_modes, key, &source, kind, store)?;
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
            if !self.template {
                output::warning(&format!(
                    "{plain} held {key} in plain text and is removed, but git history still has it; to purge it, rewrite the history with a tool such as git filter-repo, run in the repo through `{} git`",
                    name::bin()
                ));
            }
        }
        if self.applies(key, &source, replaced.as_deref()) {
            state.files.insert(key.to_owned(), entry);
        } else {
            output::info(&format!(
                "{key}: this machine does not apply {source}, so it keeps its current source"
            ));
        }
        Ok(source)
    }

    /// Refuses to overwrite, or with `--encrypt` to remove, a plain repo copy whose bytes are
    /// nowhere else: not in HEAD, not in `$HOME`, and not what S recorded for the key. A hand
    /// edit in the repo leaves such a copy, and so does `dotman edit` once `$HOME` changes again.
    fn refuse_unsaved(&self, ctx: &Context, state: &State, key: &str, source: &str) -> Result<()> {
        let plain = if self.encrypt || self.template {
            self.replaced(key, source)
        } else {
            (self.sources.flag(key, source) != Flag::Age).then(|| source.to_owned())
        };
        let Some(plain) = plain else {
            return Ok(());
        };
        let Ok(Node::File { bytes, .. }) = read_node(&ctx.paths.repo, &plain) else {
            return Ok(());
        };
        if matches!(read_node(&ctx.paths.home, key), Ok(Node::File { bytes: home, .. }) if home == bytes)
        {
            return Ok(());
        }
        let hash = Tuple::of_file(&bytes, 0).hash;
        if state.files.get(key).is_some_and(|entry| {
            entry.source == plain && entry.kind == Kind::File && entry.hash == hash
        }) {
            return Ok(());
        }
        let committed = ctx.git.head()?.is_some() && ctx.git.tree_mode("HEAD", &plain)?.is_some();
        if committed && ctx.git.show_blob("HEAD", &plain)? == bytes {
            return Ok(());
        }
        let bin = name::bin();
        let copy = shell_words::quote(&ctx.paths.repo.join(&plain).to_string_lossy()).into_owned();
        let quoted = shell_words::quote(&plain).into_owned();
        let verb = if self.encrypt || self.template {
            "remove"
        } else {
            "overwrite"
        };
        if committed {
            bail!(
                "{plain} in the repo has an edit that is neither committed nor in $HOME, and add would {verb} it; `{bin} git diff -- {quoted}` shows the edit. To keep it, copy {copy} elsewhere first; `{bin} git checkout -- {quoted}` then drops it from the repo, and add runs again"
            )
        }
        bail!(
            "{plain} in the repo is not committed and differs from $HOME, and add would {verb} it. To keep it, move {copy} elsewhere first, then add again"
        )
    }

    /// With `--encrypt` or `--template`, the plain source that `source` stores encrypted or as a
    /// template, so the two never coexist.
    fn replaced(&self, key: &str, source: &str) -> Option<String> {
        if !self.encrypt && !self.template {
            return None;
        }
        self.sources
            .sources_of(key)
            .find(|path| {
                *path != source
                    && self.sources.flag(key, path) == Flag::Plain
                    && self.stored_form(path).is_ok_and(|stored| stored == source)
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
    /// A [`Self::SecretName`] file the content guard does not scan, so storing it as it is could
    /// publish what it holds.
    SecretBinary,
    /// An age identity or a credential that `[guard] allow` does not cover.
    Content(Finding),
    Special,
    Unreadable,
    Collision,
    /// Under no `[dirs]` entry this machine approved.
    Unapproved,
}

impl Hold {
    pub(crate) fn reason(self) -> String {
        match self {
            Self::Large => "over 1 MiB".to_owned(),
            Self::Binary => "binary: NUL in the first 8000 bytes".to_owned(),
            Self::SecretName => "name matches [guard] secret_names".to_owned(),
            Self::SecretBinary => format!("{}, and {UNSCANNED}", Self::SecretName.reason()),
            Self::Content(found) => format!("{found}{}", found.remedy(None)),
            Self::Special => "a socket, FIFO or device, which dotman never stores".to_owned(),
            Self::Unreadable => "cannot be read".to_owned(),
            Self::Collision => {
                "differs only in case or Unicode normalization from another path".to_owned()
            }
            Self::Unapproved => "under a [dirs] entry not approved on this machine".to_owned(),
        }
    }

    /// The reason, and what lets the file in, for the lists of held files; an `[ignore]` pattern
    /// silences any of them. `path` is the file as [`pasteable`] gives it.
    pub(crate) fn listed(self, path: &str) -> String {
        let bin = name::bin();
        match self {
            Self::Large | Self::Binary => format!("{}; `{bin} add {path}` adds it", self.reason()),
            Self::SecretName => format!(
                "{}; `{bin} add --encrypt {path}` stores it encrypted, `{bin} add --allow-plain {path}` as it is",
                self.reason()
            ),
            Self::SecretBinary => format!(
                "{}; `{bin} add --encrypt {path}` stores it encrypted",
                self.reason()
            ),
            Self::Content(found) => format!("{found}{}", found.remedy(Some(path))),
            Self::Unreadable => format!("{}; fix its permissions to add it", self.reason()),
            Self::Collision => format!("{}; rename one of them", self.reason()),
            Self::Unapproved => format!(
                "{}; `{bin} sync --approve-dirs` approves every [dirs] entry not approved here and captures it",
                self.reason()
            ),
            Self::Special => self.reason(),
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
    /// The files in `held` as [`Hold::Unapproved`], which approving their entries would capture.
    pub waiting: BTreeMap<String, Kind>,
    /// The `[dirs]` entries this machine has not approved, in `.dotman.toml` order.
    pub unapproved: Vec<String>,
    /// What only a pattern `.dotman.toml` no longer lists keeps out: reported, never captured.
    pub still_ignored: BTreeMap<String, StillIgnored>,
}

impl Scan {
    /// Each unapproved entry with how many of the waiting files lie under it.
    pub(crate) fn unapproved_counts(&self) -> Vec<(&str, usize)> {
        self.unapproved
            .iter()
            .map(|dir| {
                let n = self.waiting.keys().filter(|key| is_under(key, dir)).count();
                (dir.as_str(), n)
            })
            .collect()
    }

    pub(crate) fn release_waiting(&mut self) {
        for (key, kind) in std::mem::take(&mut self.waiting) {
            self.held.remove(&key);
            self.new.insert(key, kind);
        }
        self.unapproved.clear();
    }
}

/// The default secret-name patterns and the ones `[guard] secret_names` adds, compiled.
pub(crate) fn secret_patterns(repo_config: &RepoConfig) -> Result<Vec<Pattern>> {
    repo_config
        .guard
        .effective_secret_names()
        .into_iter()
        .map(|text| {
            Pattern::new(text)
                .with_context(|| format!("bad [guard] pattern `{text}` in .dotman.toml"))
        })
        .collect()
}

/// The holds that need only the name and size of a new file. A credential in the name wins over
/// everything else, since no flag lets one through, then a name that matches `secrets`.
pub(crate) fn hold_by_name_and_size(secrets: &[Pattern], key: &str, size: u64) -> Option<Hold> {
    secrets::held_name(key).map(Hold::Content).or_else(|| {
        if secrets.iter().any(|p| p.matches_with(key, MATCH_SECRET)) {
            Some(Hold::SecretName)
        } else {
            (size > HOLD_BYTES).then_some(Hold::Large)
        }
    })
}

/// The holds that need the bytes of `key`, a file that passed [`hold_by_name_and_size`].
pub(crate) fn hold_by_content(bytes: &[u8], key: &str, allow: &[Allow]) -> Option<Hold> {
    if secrets::looks_binary(bytes) {
        Some(Hold::Binary)
    } else {
        secrets::held(bytes, key, allow).map(Hold::Content)
    }
}

/// Walks every `[dirs]` entry with `add <dir>`'s walker. `known` is every key that has an R or an S.
/// A new file that no entry `state` approves covers is held as [`Hold::Unapproved`] when it would
/// be captured otherwise, and left out when it would be held anyway: this machine has not agreed
/// to manage what lies there.
pub(crate) fn scan_dirs(
    ctx: &Context,
    repo_config: &RepoConfig,
    known: &BTreeSet<String>,
    state: &State,
) -> Result<Scan> {
    let secrets = secret_patterns(repo_config)?;
    let approved = state.dirs_approved();
    let planner = Planner::new(ctx, repo_config, state, PathBuf::new())?;
    let mut found: BTreeMap<String, Leaf> = BTreeMap::new();
    let mut dangling = BTreeSet::new();
    let mut scan = Scan::default();
    let mut covered: Vec<&str> = Vec::new();
    for dir in &repo_config.dirs.paths {
        let ok = approved.contains(dir);
        if !ok && !scan.unapproved.contains(dir) {
            scan.unapproved.push(dir.clone());
        }
        // The entry comes from a tracked file, so it is checked like a user-supplied path.
        let plain = ctx.check_key(dir, false).is_ok()
            && parents(dir)
                .chain(std::iter::once(dir.as_str()))
                .all(|part| is_real_dir(&ctx.paths.home.join(part)));
        if !plain {
            continue;
        }
        let walked = planner.walk(dir);
        found.extend(walked.items);
        if ok {
            covered.push(dir);
            dangling.extend(walked.dangling);
            scan.held.extend(walked.held);
            scan.notes.extend(walked.notes);
            scan.still_ignored.extend(walked.still_ignored);
        }
    }
    let is_covered = |key: &str| covered.iter().any(|dir| is_under(key, dir));
    found.retain(|key, _| !known.contains(key));
    dangling.retain(|key| !known.contains(key));
    scan.held.retain(|key, _| !known.contains(key));
    // A directory added by name is an entry of its own, walked as one.
    scan.still_ignored
        .retain(|key, _| !known.contains(key) && !repo_config.dirs.paths.contains(key));
    scan.notes
        .extend(dangling.into_iter().map(|key| format!("{key}: {DANGLING}")));

    let colliding = find_collisions(
        known
            .iter()
            .map(String::as_str)
            .chain(found.keys().map(String::as_str)),
    );
    for key in colliding.iter().flat_map(|(a, b)| [a, b]) {
        if found.remove(key).is_some() && is_covered(key) {
            scan.held.insert(key.clone(), Hold::Collision);
        }
    }
    for (key, leaf) in found {
        let hold = match leaf {
            Leaf::File { size, binary, .. } => {
                let content = || {
                    holds_key(&ctx.paths.home, &key, &ctx.machine.guard.allow).map(Hold::Content)
                };
                match hold_by_name_and_size(&secrets, &key, size) {
                    // A name hold's hint offers --allow-plain, which only a scanned file with no
                    // finding gets.
                    Some(Hold::SecretName) if binary => Some(Hold::SecretBinary),
                    Some(Hold::SecretName) => content().or(Some(Hold::SecretName)),
                    Some(hold) => Some(hold),
                    None => binary.then_some(Hold::Binary).or_else(content),
                }
            }
            Leaf::Link => holds_key(&ctx.paths.home, &key, &ctx.machine.guard.allow)
                .map(Hold::Content)
                .or_else(|| {
                    secrets
                        .iter()
                        .any(|p| p.matches_with(&key, MATCH_SECRET))
                        .then_some(Hold::SecretName)
                }),
        };
        match (hold, is_covered(&key)) {
            (Some(hold), true) => {
                scan.held.insert(key, hold);
            }
            (None, true) => {
                scan.new.insert(key, leaf.kind());
            }
            (None, false) => {
                scan.waiting.insert(key.clone(), leaf.kind());
                scan.held.insert(key, Hold::Unapproved);
            }
            (Some(_), false) => {}
        }
    }
    Ok(scan)
}

struct Planner<'a> {
    ctx: &'a Context,
    ignore: Ignore,
    /// The patterns this machine has seen in `[ignore]` that `.dotman.toml` no longer lists: an
    /// upstream edit that drops one must not widen what this machine captures.
    unlisted: Ignore,
    cwd: PathBuf,
    out: Plan,
}

impl<'a> Planner<'a> {
    fn new(
        ctx: &'a Context,
        repo_config: &RepoConfig,
        state: &State,
        cwd: PathBuf,
    ) -> Result<Self> {
        let listed = &repo_config.ignore.patterns;
        Ok(Self {
            ctx,
            ignore: Ignore::compile(listed)?,
            unlisted: Ignore::compile(&state.unlisted_ignores(listed))?,
            cwd,
            out: Plan::default(),
        })
    }

    fn plan(mut self, inputs: &[PathBuf]) -> Result<Plan> {
        for input in inputs {
            self.input(input)?;
        }
        Ok(self.out)
    }

    fn input(&mut self, input: &Path) -> Result<()> {
        let paths = &self.ctx.paths;
        let home_real = paths.home_real.as_deref();
        let key = self.ctx.input_key(input, &self.cwd)?;
        if key
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
        {
            bail!(
                "{key}: names starting with {TEMP_PREFIX} are dotman's temporary files, which are never managed; rename it to add it"
            );
        }
        // A directory's key goes into `[dirs]` in `.dotman.toml`.
        if let Some(found) = secrets::held_name(&key) {
            bail!("{key}: {found}{}", found.remedy(None));
        }
        let abs = absolute_input(input, &self.cwd, &paths.home);
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
                self.ctx.check_key(&key, true)?;
                if matches!(leaf, Leaf::Link) {
                    self.link_to_dir(&key);
                }
                self.warn_ignored(&key, false);
                self.out.files.insert(key, leaf);
            }
            Found::Dir => {
                self.warn_ignored(&key, true);
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

    // `[ignore]` keeps paths out of directory walks; a path named on the command line goes in.
    fn warn_ignored(&self, key: &str, is_dir: bool) {
        let unlisted = || {
            self.unlisted.matching(key, is_dir).map(|pattern| {
                format!(
                    "{pattern}, which .dotman.toml no longer lists and this machine still applies"
                )
            })
        };
        if let Some(pattern) = self.ignore.matching(key, is_dir).or_else(unlisted) {
            output::warning(&format!(
                "{key}: matches [ignore] {pattern}; added because you named it"
            ));
        }
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
                        Err(Left::Quiet | Left::Own(_) | Left::StillIgnored(..)) => {}
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
                        Left::StillIgnored(key, entry) => {
                            dir.still_ignored.insert(key, entry);
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
        let rel = path
            .strip_prefix(&paths.home)
            .map_err(|e| Left::Name(printable(&format!("{}: {e}", path.display()))))?;
        // Before the name checks, so an ignored entry is never reported, whatever its name; a
        // name that is not UTF-8 matches in its lossy form.
        let lossy = rel.to_string_lossy();
        if self.ignore.matches(&lossy, is_dir) {
            return Err(Left::Quiet);
        }
        let unlisted = self.unlisted.matching(&lossy, is_dir);
        match (self.checked_key(path, rel, is_dir), unlisted) {
            (Ok(key), Some(pattern)) => {
                Err(Left::StillIgnored(key, StillIgnored { pattern, is_dir }))
            }
            (Err(_), Some(_)) => Err(Left::Quiet),
            (checked, None) => checked,
        }
    }

    fn checked_key(
        &self,
        path: &Path,
        rel: &Path,
        is_dir: bool,
    ) -> std::result::Result<String, Left> {
        let named = |text: String| Left::Name(printable(&text));
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
        if self.ctx.own_key(key, !is_dir).is_some() {
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

/// What the content guard holds when H of `key` is captured into `source`: [`commit_one`]'s check,
/// which a dry run and `status` ask without capturing. An unreadable H is not held here.
pub(crate) fn capture_hold(
    ctx: &Context,
    key: &str,
    source: &str,
    encrypted: bool,
) -> Option<Finding> {
    let node = read_node(&ctx.paths.home, key).ok()?;
    node_hold(ctx, &node, key, source, encrypted)
}

fn node_hold(
    ctx: &Context,
    node: &Node,
    key: &str,
    source: &str,
    encrypted: bool,
) -> Option<Finding> {
    let found = match node {
        Node::File { bytes, .. } if !encrypted => {
            secrets::held(bytes, key, &ctx.machine.guard.allow)
        }
        Node::Symlink(target) => secrets::held_link(target.as_os_str().as_bytes(), key),
        _ => None,
    };
    found.or_else(|| secrets::held_name(source))
}

/// What the content guard holds in H of `key`, a file or a symlink. An unreadable file is not held
/// here; capturing it fails and is reported then.
pub(crate) fn holds_key(home: &Path, key: &str, allow: &[Allow]) -> Option<Finding> {
    match read_node(home, key) {
        Ok(Node::File { bytes, .. }) => secrets::held(&bytes, key, allow),
        Ok(Node::Symlink(target)) => secrets::held_link(target.as_os_str().as_bytes(), key),
        _ => None,
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
    file.take(u64::try_from(secrets::SNIFF_BYTES).unwrap_or(u64::MAX))
        .read_to_end(&mut head)?;
    Ok(secrets::looks_binary(&head))
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

    // What the "add N files?" question approves, so `-q` does not hide it.
    output::context(&format!("{}/", dir.key));
    output::context(&format!("  files: {}", dir.items.len()));
    output::context(&format!("  total size: {}", output::count(total, "byte")));
    output::context("  largest:");
    for (size, key) in sized.iter().take(LARGEST_SHOWN) {
        output::context(&format!("    {}  {key}", output::count(*size, "byte")));
    }
    output::context(&format!("  binary files: {binary}"));
    output::context(&format!("  files at mode 0600: {private}"));
    output::context(&format!("  skipped: {}", dir.skipped.len()));
    for line in &dir.skipped {
        output::context(&format!("    {line}"));
    }
}

/// Copies H of `key` into the repo at `source`, which is `key` or one of its variants, and records
/// the mode under `source`, so each variant keeps its own. An `encrypted` source gets ciphertext,
/// and a plain one that the content guard holds is refused with [`Held`]. `expected` is the kind
/// the caller saw, and a different kind now is an error. Returns S for `key`; the caller records it
/// and saves `repo_config`.
pub(crate) fn commit_one(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    dir_modes: &mut BTreeMap<String, u32>,
    key: &str,
    source: &str,
    expected: Kind,
    encrypted: bool,
) -> Result<Entry> {
    let store = if encrypted {
        Store::Encrypted
    } else {
        Store::Plain
    };
    commit_stored(ctx, repo_config, dir_modes, key, source, expected, store)
}

/// How [`commit_stored`] puts the bytes of H in the repo.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Store {
    Plain,
    Encrypted,
    /// As text that renders back to the bytes, between `{% raw %}` and `{% endraw %}` when it
    /// would not otherwise. The content guard still scans the bytes of H.
    Template,
}

fn commit_stored(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    dir_modes: &mut BTreeMap<String, u32>,
    key: &str,
    source: &str,
    expected: Kind,
    store: Store,
) -> Result<Entry> {
    let encrypted = store == Store::Encrypted;
    validate_key(source)?;
    let home = &ctx.paths.home;
    let repo = &ctx.paths.repo;
    let target = repo.join(source);
    if let Some(what) = ctx.keys().alias(home, key, true) {
        return Err(OwnPath::new(key, what).into());
    }
    let node = read_node(home, key)?;
    if let Some(found) = node_hold(ctx, &node, key, source, encrypted) {
        return Err(Held(found).into());
    }
    ensure_worktree_parents(repo, source)?;
    let file_mode = match (expected, &node) {
        (Kind::File, Node::File { mode, .. }) => Some(*mode),
        _ => None,
    };
    let (changed, remove_later) = record_ahead(
        repo_config,
        dir_modes,
        home,
        key,
        source,
        file_mode,
        encrypted,
    )?;
    // Saved first, so a kill before the content lands leaves a mode without its content, never
    // content that other machines would apply at the default.
    if changed {
        repo_config.save(&ctx.repo_config_file(), false)?;
    }
    let mut rendered = None;
    let plaintext = match (expected, node) {
        (Kind::File, Node::File { bytes, mode, .. }) if store == Store::Template => {
            // A template that already renders to H stays, which keeps a rerun from replacing its
            // expressions with their output.
            if !renders_to(ctx, source, &bytes) {
                let text = template_text(ctx, key, source, &bytes, false)?;
                write_atomic(&target, &text, stored_mode(mode))?;
            }
            rendered = Some(bytes);
            None
        }
        (Kind::File, Node::File { bytes, mode, .. }) if encrypted => {
            store_encrypted(ctx, &repo_config.age.recipients, source, &bytes)?;
            Some(Tuple::of_file(&bytes, safe_mode(mode)).entry(source))
        }
        (Kind::File, Node::File { bytes, mode, .. }) => {
            write_atomic(&target, &bytes, stored_mode(mode))?;
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
    if remove_later {
        repo_config.modes.remove(source);
    }
    if let Some(entry) = plaintext {
        return Ok(entry);
    }
    let written = Worktree::new(repo, &repo_config.modes)
        .get(source)?
        .ok_or_else(|| anyhow!("{source} is not in the repo after being written"))?
        .tuple;
    // S records what renders, which is H.
    let written = rendered.map_or_else(
        || written.clone(),
        |bytes| Tuple::of_file(&bytes, written.mode),
    );
    let mut entry = written.entry(source);
    // A mode `[modes]` leaves out stays this machine's: S holds it against the repo's, as for a
    // mode apply keeps, so the file stays clean here.
    if let Some(mode) = file_mode
        .map(safe_mode)
        .filter(|mode| *mode != written.mode)
    {
        entry.mode = format!("{mode:04o}");
        entry.repo_mode = Some(format!("{:04o}", written.mode));
    }
    Ok(entry)
}

/// Whether the repo's template `source` renders to `bytes` on this machine.
fn renders_to(ctx: &Context, source: &str, bytes: &[u8]) -> bool {
    let Ok(Node::File { bytes: stored, .. }) = read_node(&ctx.paths.repo, source) else {
        return false;
    };
    std::str::from_utf8(&stored).is_ok_and(|text| {
        template::render(source, text, ctx.template_vars()).is_ok_and(|out| out.as_bytes() == bytes)
    })
}

/// The text to store as the template `source` of `key` so that it renders to `bytes` on this
/// machine: `bytes` themselves, or wrapped in a raw block when they hold syntax a template would
/// read. `dry_run` only changes the wording of the note about the wrap.
fn template_text(
    ctx: &Context,
    key: &str,
    source: &str,
    bytes: &[u8],
    dry_run: bool,
) -> Result<Vec<u8>> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        bail!("{key} is not text, so it cannot be a template");
    };
    let renders = |stored: &str| {
        template::render(source, stored, ctx.template_vars()).is_ok_and(|out| out == text)
    };
    if renders(text) {
        return Ok(bytes.to_vec());
    }
    let wrapped = format!("{{% raw %}}{text}{{% endraw %}}");
    if !renders(&wrapped) {
        bail!(
            "{key} holds text that a template reads as its own syntax, and it cannot be stored between {{% raw %}} and {{% endraw %}}"
        );
    }
    output::warning(&format!(
        "{key} holds text a template reads as its own syntax, so it {} between {{% raw %}} and {{% endraw %}}; move those markers as you edit",
        if dry_run {
            "would be stored"
        } else {
            "is stored"
        }
    ));
    Ok(wrapped.into_bytes())
}

const fn stored_mode(mode: u32) -> u32 {
    if mode & 0o111 == 0 { 0o644 } else { 0o755 }
}

/// Puts in `[modes]` what capturing `key` as `source` adds or changes, for a file at `file_mode`
/// and the directories above it. Returns whether anything changed, and whether the file's entry
/// is to go, which waits for the content: until then the old content would apply at the default.
fn record_ahead(
    repo_config: &mut RepoConfig,
    dir_modes: &mut BTreeMap<String, u32>,
    home: &Path,
    key: &str,
    source: &str,
    file_mode: Option<u32>,
    encrypted: bool,
) -> Result<(bool, bool)> {
    let mut changed = record_dirs(repo_config, dir_modes, home, key)?;
    let mut remove_later = false;
    if let Some(mode) = file_mode {
        let old = repo_config.modes.get(source).cloned();
        if encrypted {
            keep_private(repo_config, key, source, safe_mode(mode))?;
        } else {
            let mode = safe_mode(mode);
            let shared = record_mode(&mut repo_config.modes, source, mode, stored_mode(mode))?;
            let narrowed = shared != mode && shared != stored_mode(mode);
            if narrowed && old.as_deref() != Some(format!("{shared:04o}").as_str()) {
                let dropped = match mode & !shared & SHARED_WRITE {
                    0o020 => "group write",
                    0o002 => "other write",
                    _ => "group and other write",
                };
                output::warning(&format!(
                    "{key} is {mode:04o}; [modes] records it as {shared:04o}, without {dropped}, for every machine, so this copy is narrowed on the next apply; to keep {mode:04o}, set it for {source} in [modes] in .dotman.toml"
                ));
            }
        }
        let new = repo_config.modes.get(source).cloned();
        match (old, new) {
            (Some(old), None) => {
                repo_config.modes.insert(source.to_owned(), old);
                remove_later = true;
            }
            (old, Some(new)) => changed = changed || old.as_ref() != Some(&new),
            (None, None) => {}
        }
    }
    Ok((changed, remove_later))
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
        record_mode(&mut repo_config.modes, source, mode, 0o600)?;
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
    secrets::write_ciphertext(ctx, source, &ciphertext)
}

/// Group and other write bits, which a umask of 002 gives every new file and directory. A capture
/// or an import records one only when `[modes]` already holds it for that key, so no machine
/// passes them on to the others unless someone wrote them in by hand.
const SHARED_WRITE: u32 = 0o022;

/// `mode` as `[modes]` may hold it for `key`: without each [`SHARED_WRITE`] bit that the entry for
/// `key` lacks. A bit the entry has stays only while `mode` has it too, so clearing it narrows the
/// entry.
pub(crate) fn shareable_mode(
    modes: &BTreeMap<String, String>,
    key: &str,
    mode: u32,
) -> Result<u32> {
    let existing = recorded_mode(modes, key)?.unwrap_or(0);
    Ok((mode & !SHARED_WRITE) | (mode & existing & SHARED_WRITE))
}

/// Records the mode of each directory above `key` that `[modes]` has no entry for, and returns
/// whether it added one. Machines differ in umask, so an entry, which may be another machine's, is
/// never replaced or removed here; otherwise each capture would flip it.
fn record_dirs(
    repo_config: &mut RepoConfig,
    cache: &mut BTreeMap<String, u32>,
    home: &Path,
    key: &str,
) -> Result<bool> {
    let mut added = false;
    for parent in parents(key) {
        let entry = format!("{parent}/");
        if repo_config.modes.contains_key(&entry) {
            continue;
        }
        let mode = record_mode(
            &mut repo_config.modes,
            &entry,
            dir_mode(cache, home, parent)?,
            0o755,
        )?;
        added = added || mode != 0o755;
    }
    Ok(added)
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

/// Every capture and import writes `[modes]` through here. Records [`shareable_mode`] of `mode`
/// for `key`, or removes the entry when that is `default`, and returns it.
pub(crate) fn record_mode(
    modes: &mut BTreeMap<String, String>,
    key: &str,
    mode: u32,
    default: u32,
) -> Result<u32> {
    let mode = shareable_mode(modes, key, mode)?;
    if mode == default {
        modes.remove(key);
    } else {
        modes.insert(key.to_owned(), format!("{mode:04o}"));
    }
    Ok(mode)
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
    output::success(&format!(
        "added {}",
        output::count(files.len(), "system file")
    ));
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
    if let Some(found) = secrets::findings(&bytes).next() {
        bail!(
            "{key}: {found}, and system scope does not encrypt, so it is never stored; keep secrets as {{age}} files under $HOME"
        );
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

    #[test]
    fn a_shared_write_bit_is_recorded_only_where_the_entry_and_the_mode_both_have_it() {
        for (entry, mode, recorded) in [
            (Some("0664"), 0o666, Some("0664")),
            (Some("0664"), 0o664, Some("0664")),
            (Some("0664"), 0o644, None),
            (Some("0664"), 0o662, Some("0660")),
            (None, 0o664, None),
            (None, 0o660, Some("0640")),
            (Some("0600"), 0o666, None),
        ] {
            let mut modes: BTreeMap<String, String> = entry
                .map(|mode| (".f".to_owned(), mode.to_owned()))
                .into_iter()
                .collect();
            record_mode(&mut modes, ".f", mode, 0o644).unwrap();
            assert_eq!(
                modes.get(".f").map(String::as_str),
                recorded,
                "{entry:?} with {mode:o}"
            );
        }
        let mut modes = BTreeMap::from([(".d/".to_owned(), "0775".to_owned())]);
        record_mode(&mut modes, ".d/", 0o777, 0o755).unwrap();
        assert_eq!(modes.get(".d/").map(String::as_str), Some("0775"));
    }

    #[test]
    fn another_name_for_an_own_file_is_never_read_into_the_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().as_os_str().to_owned();
        let env = crate::paths::Env::from_fn(|name| (name == "HOME").then(|| home.clone()));
        let paths = crate::paths::Paths::resolve(&env, crate::sys::geteuid()).unwrap();
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        fs::create_dir_all(&paths.repo).unwrap();
        fs::write(&paths.config_file, b"[vars]\nmine = \"1\"\n").unwrap();
        fs::hard_link(&paths.config_file, tmp.path().join("alias")).unwrap();
        let ctx = Context::new(paths, crate::config::Config::default(), "add");
        let err = commit_one(
            &ctx,
            &mut RepoConfig::default(),
            &mut BTreeMap::new(),
            "alias",
            "alias",
            Kind::File,
            false,
        )
        .unwrap_err();
        assert!(err.is::<OwnPath>(), "{err:#}");
        assert!(fs::symlink_metadata(ctx.paths.repo.join("alias")).is_err());
    }

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
            |_| None,
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
            |_| None,
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
            template: false,
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
                |_| None,
            )
            .unwrap();
            assert_eq!(held_keys(&guarded), [".config/x/.env"], "public: {public}");
            assert_eq!(kept(&plan), [".config/x/managed.token", ".config/x/rc"]);
        }
        let mut plan = plan_of(&[], &items);
        let guarded = guard_secrets(
            &mut plan,
            &config,
            &managed,
            &me,
            false,
            &args(false, true),
            |_| None,
        )
        .unwrap();
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
            |_| None,
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
        let guarded = guard_secrets(
            &mut plan,
            &config,
            &none,
            &me,
            false,
            &args(false, false),
            |_| None,
        )
        .unwrap();
        assert_eq!(guarded.plain, [".env"]);
        assert_eq!(plan.files.len(), 2);
        for flags in [args(true, false), args(false, true)] {
            let mut plan = plan_of(&[".env"], &[]);
            let guarded =
                guard_secrets(&mut plan, &config, &none, &me, true, &flags, |_| None).unwrap();
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
            |_| None,
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
    fn a_public_remote_never_offers_allow_plain_for_a_file_that_holds_a_credential() {
        let config = RepoConfig::default();
        let none = Sources::default();
        let mut plan = plan_of(&[".ENV", ".aws/credentials"], &[]);
        let token = |key: &str| {
            (key == ".aws/credentials").then_some(Finding {
                rule: "github-token",
                line: 2,
                place: secrets::Place::Content,
            })
        };
        let err = guard_secrets(
            &mut plan,
            &config,
            &none,
            &here(),
            true,
            &args(false, false),
            token,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(".aws/credentials: line 2 holds a github-token; --encrypt stores it encrypted; or remove the secret from the file"),
            "{err}"
        );
        assert!(!err.contains("--allow-plain"), "{err}");
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
