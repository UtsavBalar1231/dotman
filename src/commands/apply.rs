//! `dotman apply`: write R into `$HOME`. Paths that need a decision are asked about once per
//! category, never per file unless the user picks that; a run that cannot ask leaves them alone.

use crate::cli::{ApplyArgs, Decisions};
use crate::commands::add::{self, Scan};
use crate::commands::status::token;
use crate::commands::{Outcome, diff, forget};
use crate::context::{Context, is_real_dir};
use crate::engine::status::{Content, Row, Source, SourceEntry, Variants, Worktree, classify};
use crate::engine::write::{
    Skip, WriteOptions, WriteOutcome, back_up_bytes, backup_run_dir, record_mode, safe_symlink,
    safe_write, safe_write_over_link,
};
use crate::engine::{
    Home, Node, Tuple, hash_hex, never_widen, observe, observe_portable, parse_mode, read_node,
};
use crate::hooks::{self, Policy};
use crate::identity;
use crate::name;
use crate::output;
use crate::prompt;
use crate::repo_config::RepoConfig;
use crate::secrets::{self, Finding, Held, HoldsKey, Locked};
use crate::state::{Entry, IdentitySnapshot, Kind, State};
use crate::sys;
use crate::system::escalate::{Escalator, Tool};
use crate::system::helper::{self, Plan, PlanEntry, Reported};
use crate::system::{self, Scope, Verdict};
use crate::variant::{self, Flag, Selection, Sources};
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Runs `dotman apply`.
///
/// # Errors
///
/// Fails when the repo is not initialised, a rebase or merge is in progress, another process holds
/// the lock, two keys collide by case or normalization, a path cannot be applied, or a hook fails.
pub fn run(args: &ApplyArgs) -> Result<Outcome> {
    let ctx = Context::resolve("apply")?;
    let _lock = ctx.lock(args.dry_run)?;
    // Before git is asked about the index, which needs a repo to exist.
    ctx.repo_id()?;
    ctx.refuse_collisions()?;
    if !args.dry_run {
        ctx.sweep_temp_files()?;
    }
    let opts = Options {
        dry_run: args.dry_run,
        in_place: args.in_place,
        decisions: args.decisions,
        system: args.system,
        yes: args.yes,
        ..Options::default()
    };
    let applied = apply(&ctx, &args.paths, &opts)?;
    if applied.announced == 0 && applied.failed == 0 && applied.outcome == Outcome::Done {
        output::info(&match applied.left {
            0 => "nothing to apply".to_owned(),
            1 => "nothing written; the path above stays as it is".to_owned(),
            n => format!("nothing written; the {n} paths above stay as they are"),
        });
    }
    let policy = Policy {
        approve: args.decisions.approve_hooks,
        no_hooks: args.no_hooks,
        dry_run: args.dry_run,
        never_ask: false,
    };
    finish(&ctx, &applied, policy)
}

/// Runs the hooks for what `applied` wrote, then fails if a path or a hook failed.
pub(crate) fn finish(ctx: &Context, applied: &Applied, policy: Policy) -> Result<Outcome> {
    let hooked = hooks::run(ctx, &applied.written, policy);
    let hooked = applied.check().and(hooked)?;
    Ok(if applied.outcome == Outcome::Done {
        hooked
    } else {
        applied.outcome
    })
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    /// Write R into `$HOME`; H changed here is left for the next `sync` to capture.
    #[default]
    Apply,
    /// `sync`'s pass before the pull: capture H into the repo where `Apply` would leave it, and
    /// leave the rows that need no decision for the pass after the pull.
    Capture,
}

// reason: independent switches, not a state machine
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
pub(crate) struct Options {
    pub dry_run: bool,
    pub in_place: bool,
    pub decisions: Decisions,
    /// Print how many paths are in each row before anything else.
    pub summary: bool,
    pub pass: Pass,
    /// Answers template drift like `--take-repo`, for a caller that has just changed the template.
    pub take_drift: bool,
    /// Keys left alone, because an earlier pass already reported them undecided.
    pub skip: BTreeSet<String>,
    /// Write system files too, after the home files, through one escalation.
    pub system: bool,
    /// Write them without asking.
    pub yes: bool,
}

pub(crate) struct Applied {
    pub outcome: Outcome,
    /// Keys that were asked about and left unanswered, reported as template drift or held, or
    /// that a capture could not read.
    pub undecided: BTreeSet<String>,
    /// Keys captured into the repo; in a dry run, the keys that would be.
    pub captured: BTreeSet<String>,
    /// Keys whose `$HOME` entry was written; in a dry run, the keys that would be.
    pub written: BTreeSet<String>,
    /// Paths that could not be applied or captured. The caller reports them with `check`, after
    /// it has run the hooks for what was written.
    pub failed: usize,
    /// The part of `failed` that a later pass cannot clear: captures that did not happen.
    pub uncaptured: usize,
    /// Paths reported as applied, kept, captured or forgotten.
    pub announced: usize,
    /// Paths reported and left as they are without needing attention: locked, changed here, or
    /// system files that only `apply --system` writes.
    pub left: usize,
}

impl Applied {
    pub(crate) fn check(&self) -> Result<()> {
        check_failed(self.failed, self.uncaptured)
    }
}

/// Fails when any of `failed` paths failed; `uncaptured` of them are captures that did not happen.
pub(crate) fn check_failed(failed: usize, uncaptured: usize) -> Result<()> {
    let applied = failed.saturating_sub(uncaptured);
    let paths = |n: usize| if n == 1 { "path" } else { "paths" };
    let mut parts = Vec::new();
    if applied > 0 {
        parts.push(format!("{applied} {} could not be applied", paths(applied)));
    }
    if uncaptured > 0 {
        parts.push(format!(
            "{uncaptured} {} could not be captured",
            paths(uncaptured)
        ));
    }
    if parts.is_empty() {
        return Ok(());
    }
    bail!("{}", parts.join(", and "))
}

/// H could not be read, so the key can be neither applied nor captured.
#[derive(Debug)]
pub(crate) struct HomeUnreadable;

impl std::fmt::Display for HomeUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("it cannot be read in $HOME")
    }
}

/// Applies the keys under `inputs` (every key when empty). The caller holds the lock.
pub(crate) fn apply(ctx: &Context, inputs: &[PathBuf], opts: &Options) -> Result<Applied> {
    let repo_id = ctx.repo_id()?;
    let mut repo_config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;
    let sources = ctx.sources()?;
    let mut keys: BTreeSet<String> = sources.keys().map(str::to_owned).collect();
    keys.extend(state.files.keys().cloned());
    // Only a whole-repo capture looks for new files; `apply <path>` has no R for them.
    let scan = if opts.pass == Pass::Capture && inputs.is_empty() {
        add::scan_dirs(ctx, &repo_config, &keys)?
    } else {
        Scan::default()
    };
    let (home_inputs, system_filters) = if opts.system {
        system::split_inputs(&ctx.paths, inputs)?
    } else {
        (inputs.to_vec(), Vec::new())
    };
    let only_system = !inputs.is_empty() && home_inputs.is_empty();
    let mut keys = if only_system {
        BTreeSet::new()
    } else {
        ctx.select(&home_inputs, keys)?
    };
    keys.retain(|key| !opts.skip.contains(key));

    let mut run = Run {
        ctx,
        opts,
        sources: &sources,
        modes: repo_config.modes.clone(),
        backups: backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?,
        attention: false,
        failed: 0,
        uncaptured: 0,
        forget: BTreeSet::new(),
        undecided: BTreeSet::new(),
        captured: BTreeSet::new(),
        written: BTreeSet::new(),
        dir_modes: BTreeMap::new(),
        accepted: false,
        locked: Vec::new(),
        system_writes: Vec::new(),
        forget_system: BTreeSet::new(),
        announced: 0,
        left: 0,
        kept: Vec::new(),
    };
    let mut items = run.classify(&sources, &state, &keys);
    if opts.system && (inputs.is_empty() || !system_filters.is_empty()) {
        items.extend(run.system_items(&state, &repo_config, &system_filters)?);
    }
    run.report_locked();
    if opts.summary {
        summarize(&items, run.locked.len());
    }
    run.decide_identity(&mut items, state.identity.as_ref())?;
    let actions = run.plan(&items)?;
    for (item, action) in items.iter().zip(actions) {
        if let Some(action) = action {
            run.execute(item, action, &mut state, &mut repo_config);
        }
    }
    if inputs.is_empty() && !opts.system {
        run.system(&mut state, &mut repo_config);
    }
    run.write_system(&mut state, &repo_config);
    run.capture_new(&scan, &mut state, &mut repo_config);
    run.report_kept();

    let repo_saved = run.save_repo(&mut repo_config, &mut state);
    if !opts.dry_run && (state.identity.is_none() || run.accepted) {
        state.identity = Some(ctx.identity().clone());
    }
    state.save(&ctx.paths.state_file(), opts.dry_run)?;
    repo_saved?;
    Ok(Applied {
        outcome: if run.attention {
            Outcome::Attention
        } else {
            Outcome::Done
        },
        undecided: run.undecided,
        captured: run.captured,
        written: run.written,
        failed: run.failed,
        uncaptured: run.uncaptured,
        announced: run.announced,
        left: run.left,
    })
}

pub(crate) struct Item {
    key: String,
    row: Row,
    r: Option<SourceEntry>,
    /// H as the write path reads it again before renaming. It differs from the classified H only
    /// for an absolute link into `$HOME`, which classification compares in its relative form.
    raw: Home,
    /// The repo's own mode, when `r` carries the mode this machine keeps instead.
    repo_mode: Option<u32>,
    /// The repo's mode when S was recorded, for a home key.
    basis: Option<u32>,
    /// The identity change since the snapshot moved this key to another source. Until the change
    /// is accepted, the key is left alone.
    identity: bool,
    /// An absolute key outside `$HOME`, which only the system-write helper writes.
    system: bool,
}

/// A system key that is to be written once every home write is done.
struct SystemWrite {
    key: String,
    r: SourceEntry,
    h: Home,
}

/// A [`SystemWrite`] that was read again and found unchanged, with what the plan needs.
struct Ready {
    key: String,
    target: String,
    r: SourceEntry,
    bytes: Vec<u8>,
    owner: String,
    group: String,
    /// The file there now, which is backed up first: bytes, mode, uid and gid.
    current: Option<(Vec<u8>, u32, u32, u32)>,
}

impl Item {
    /// The row, before any identity change is accepted.
    pub(crate) const fn row(&self) -> Row {
        self.row
    }

    pub(crate) const fn token(&self) -> &'static str {
        if self.identity {
            "identity-changed"
        } else {
            token(self.row)
        }
    }

    /// The mode kept on this machine and the repo's, when they differ.
    pub(crate) fn kept(&self) -> Option<(u32, u32)> {
        Some((self.r.as_ref()?.tuple.mode, self.repo_mode?))
    }
}

pub(crate) enum Classified {
    Unmanaged,
    Item(Item),
    Ambiguous(Vec<String>),
    Invalid(Vec<(String, &'static str)>),
    /// Encrypted, and no key here decrypts it: left alone, with a warning only.
    Locked(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Write,
    /// Leave H as it is and set S to R, so the next `sync` captures H.
    KeepLocal,
    /// H already equals R; S catches up silently.
    Record,
    DropState,
    /// Remove the key from the repo, as `dotman forget` does.
    Forget,
    /// Copy H into the repo, so that S, R and H agree.
    Capture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Keep H; for a local deletion, forget the path.
    Local,
    Repo,
    Skip,
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Choose(Choice),
    Diffs,
    PerFile,
}

struct Answer {
    letter: &'static str,
    all: &'static str,
    one: &'static str,
}

struct Question {
    what: &'static str,
    rows: fn(Row) -> bool,
    flag: fn(Decisions) -> Option<Choice>,
    local: Answer,
    local_action: Action,
    repo: Answer,
    diffs: bool,
    /// The flags that answer `local` and `repo`.
    flags: [&'static str; 2],
}

const KEEP: Answer = Answer {
    letter: "k",
    all: "keep all local (capture them into the repo for every machine)",
    one: "keep local (capture it into the repo for every machine)",
};
const TAKE: Answer = Answer {
    letter: "t",
    all: "take all from repo",
    one: "take from repo",
};

// Asked in this order, one prompt per category per run. Symlink-at-target is a conflict too, but
// it is only reported: taking the repo version would mean writing through the link.
const QUESTIONS: [Question; 3] = [
    Question {
        what: "conflicts",
        rows: |row| row == Row::BothChanged,
        flag: |d| choice(d.keep_local, d.take_repo),
        local: KEEP,
        local_action: Action::KeepLocal,
        repo: TAKE,
        diffs: true,
        flags: ["--keep-local", "--take-repo"],
    },
    Question {
        what: "existing files",
        rows: |row| row == Row::Existing,
        flag: |d| choice(d.keep_local, d.take_repo),
        local: KEEP,
        local_action: Action::KeepLocal,
        repo: TAKE,
        diffs: true,
        flags: ["--keep-local", "--take-repo"],
    },
    Question {
        what: "local deletions",
        // Deleted here and changed upstream too: keeping "local" is forgetting it, and taking the
        // repo's is restoring it.
        rows: |row| matches!(row, Row::DeletedLocally | Row::DeletedChangedUpstream),
        flag: |d| choice(d.forget_deleted, d.restore_deleted),
        local: Answer {
            letter: "f",
            all: "forget all",
            one: "forget",
        },
        local_action: Action::Forget,
        repo: Answer {
            letter: "r",
            all: "restore all",
            one: "restore",
        },
        diffs: false,
        flags: ["--forget-deleted", "--restore-deleted"],
    },
];

const fn choice(local: bool, repo: bool) -> Option<Choice> {
    if local {
        Some(Choice::Local)
    } else if repo {
        Some(Choice::Repo)
    } else {
        None
    }
}

/// Where keeping `$HOME`'s version captures each key: into its winning source when that names
/// this host, so that no other machine applies it, or into the repo for every machine.
#[derive(Default)]
struct Keeping<'k> {
    shared: Vec<&'k str>,
    /// Each key with its winning source.
    own: Vec<(&'k str, String)>,
}

impl Keeping<'_> {
    /// Such as `.p into .p{host=b}, which only this machine applies`.
    fn own(&self) -> Option<String> {
        if self.own.is_empty() {
            return None;
        }
        let into: Vec<String> = self
            .own
            .iter()
            .map(|(key, source)| format!("{key} into {source}"))
            .collect();
        Some(format!(
            "{}, which only this machine applies",
            into.join(", ")
        ))
    }

    /// What follows "this machine's version": nothing when the shared keys are all of them, which
    /// were listed already, and their names otherwise.
    fn of(&self) -> String {
        if self.own.is_empty() {
            String::new()
        } else {
            format!(" of {}", self.shared.join(", "))
        }
    }

    const fn them(&self) -> &'static str {
        if self.shared.len() == 1 { "it" } else { "them" }
    }

    /// The keep answer's text, when it captures some keys into sources of their own.
    const fn label(&self, bulk: bool) -> Option<&'static str> {
        match (self.own.is_empty(), self.shared.is_empty(), bulk) {
            (true, _, _) => None,
            (false, true, false) => Some("keep local (capture it into this machine's own variant)"),
            (false, true, true) => {
                Some("keep all local (capture them into this machine's own variants)")
            }
            (false, false, _) => Some("keep all local (capture them into the repo)"),
        }
    }
}

struct Run<'a> {
    ctx: &'a Context,
    opts: &'a Options,
    sources: &'a Sources,
    modes: BTreeMap<String, String>,
    backups: PathBuf,
    attention: bool,
    failed: usize,
    uncaptured: usize,
    forget: BTreeSet<String>,
    undecided: BTreeSet<String>,
    captured: BTreeSet<String>,
    written: BTreeSet<String>,
    dir_modes: BTreeMap<String, u32>,
    accepted: bool,
    locked: Vec<(String, String)>,
    system_writes: Vec<SystemWrite>,
    forget_system: BTreeSet<String>,
    announced: usize,
    left: usize,
    /// Keys kept local this run, other than templates.
    kept: Vec<String>,
}

impl Run<'_> {
    /// Removes what this run forgets and saves `.dotman.toml`, when the run changed the repo. A
    /// dry run only says where an uncommitted edit that forgetting removes would be saved.
    fn save_repo(&self, repo_config: &mut RepoConfig, state: &mut State) -> Result<()> {
        let (ctx, backups) = (self.ctx, &self.backups);
        let (forget, system) = (&self.forget, &self.forget_system);
        if forget.is_empty() && system.is_empty() && self.captured.is_empty() {
            return Ok(());
        }
        if self.opts.dry_run {
            forget::save_repo_edits(ctx, state, forget, backups, true)?;
            return forget::save_system_edits(ctx, system, backups, true);
        }
        if !forget.is_empty() {
            forget::remove_keys(ctx, repo_config, state, forget, backups)?;
        }
        forget::remove_system_keys(ctx, repo_config, state, system, backups)?;
        repo_config.save(&ctx.repo_config_file(), false)
    }

    fn classify(&mut self, sources: &Sources, state: &State, keys: &BTreeSet<String>) -> Vec<Item> {
        let worktree = Worktree::new(&self.ctx.paths.repo, &self.modes);
        let variants = Variants::new(
            worktree,
            sources,
            self.ctx.identity(),
            self.ctx.template_vars(),
            self.ctx.age(),
        );
        let mut items = Vec::new();
        for key in keys {
            match classify_key(self.ctx, &variants, state, key) {
                Ok(Classified::Item(item)) => items.push(item),
                Ok(Classified::Unmanaged) => {}
                // Reported by the pass that applies, like the rows that are only reported.
                Ok(Classified::Ambiguous(_) | Classified::Invalid(_) | Classified::Locked(_))
                    if self.opts.pass == Pass::Capture => {}
                Ok(Classified::Locked(reason)) => self.locked.push((key.clone(), reason)),
                Ok(Classified::Ambiguous(tied)) => {
                    self.attention = true;
                    output::warning(&format!(
                        "{key}: {} are equally specific here; skipped until one is removed",
                        tied.join(", ")
                    ));
                }
                Ok(Classified::Invalid(problems)) => {
                    self.attention = true;
                    for (source, reason) in problems {
                        output::warning(&format!("{key}: skipped, {source}: {reason}"));
                    }
                }
                // An unreadable H is what `sync` came to capture; the pass after the pull leaves
                // it alone instead of reporting it again.
                Err(e) if self.opts.pass == Pass::Capture && e.is::<HomeUnreadable>() => {
                    output::warning(&format!("{key}: cannot be captured: {e:#}"));
                    self.failed = self.failed.saturating_add(1);
                    self.uncaptured = self.uncaptured.saturating_add(1);
                    self.undecided.insert(key.clone());
                }
                Err(e) => {
                    output::warning(&format!("{key}: skipped: {e:#}"));
                    self.failed = self.failed.saturating_add(1);
                }
            }
        }
        items
    }

    fn report_locked(&mut self) {
        if self.locked.is_empty() {
            return;
        }
        let n = self.locked.len();
        self.left = self.left.saturating_add(n);
        output::warning(&format!(
            "{n} encrypted {} locked, left alone until a key here decrypts {}:",
            if n == 1 { "path is" } else { "paths are" },
            if n == 1 { "it" } else { "them" },
        ));
        for (key, reason) in &self.locked {
            output::warning(&format!("  {key} ({reason})"));
        }
    }

    /// Asked before every other category, because accepting turns some of these paths into
    /// conflicts. A path still waiting on it takes no part in any later category.
    fn decide_identity(
        &mut self,
        items: &mut [Item],
        snapshot: Option<&IdentitySnapshot>,
    ) -> Result<()> {
        let pending: Vec<String> = items
            .iter()
            .filter(|item| item.identity)
            .map(|item| item.key.clone())
            .collect();
        let change = snapshot
            .map(|old| identity::describe_change(old, self.ctx.identity()))
            .unwrap_or_default();
        let accepted = if self.opts.decisions.accept_identity {
            true
        } else if pending.is_empty() || self.opts.dry_run || !prompt::interactive() {
            false
        } else {
            output::context(&format!(
                "the machine identity changed ({change}), which gives {} paths another source:",
                pending.len()
            ));
            for key in &pending {
                output::context(&format!("  {key}"));
            }
            prompt::confirm("accept the new identity? [y/N]")?
        };
        if accepted {
            self.accepted = true;
            for item in items.iter_mut() {
                item.identity = false;
            }
            return Ok(());
        }
        if pending.is_empty() {
            return Ok(());
        }
        self.attention = true;
        output::warning(&format!(
            "identity changed ({change}): {} paths left undecided; answer with `{} {} --accept-identity`:",
            pending.len(),
            name::bin(),
            self.command()
        ));
        for key in pending {
            output::warning(&format!("  {key}"));
            self.undecided.insert(key);
        }
        Ok(())
    }

    fn plan(&mut self, items: &[Item]) -> Result<Vec<Option<Action>>> {
        let mut actions: Vec<Option<Action>> = items
            .iter()
            .map(|item| {
                if item.identity {
                    None
                } else {
                    self.direct(item)
                }
            })
            .collect();
        for question in &QUESTIONS {
            let (indices, members): (Vec<usize>, Vec<&Item>) = items
                .iter()
                .enumerate()
                .filter(|(_, item)| !item.identity && (question.rows)(item.row))
                .unzip();
            let choices = self.decide(question, &members, (question.flag)(self.opts.decisions))?;
            let mut undecided: Vec<&Item> = Vec::new();
            for ((index, item), choice) in indices.iter().zip(&members).zip(choices) {
                let action = match choice {
                    Choice::Local => Some(self.keep(question.local_action, item)),
                    Choice::Repo => Some(Action::Write),
                    Choice::Skip => {
                        undecided.push(item);
                        self.undecided.insert(item.key.clone());
                        None
                    }
                };
                if let Some(slot) = actions.get_mut(*index) {
                    *slot = action;
                }
            }
            if !undecided.is_empty() {
                self.attention = true;
                let [local, repo] = question.flags;
                output::warning(&format!(
                    "{} left undecided ({}); answer with `{bin} {command} {local}` or `{bin} {command} {repo}`:",
                    question.what,
                    undecided.len(),
                    bin = name::bin(),
                    command = self.command(),
                ));
                for item in &undecided {
                    output::warning(&format!("  {}", item.key));
                }
                if let Some(keeping) = self.keep_here(question, &undecided) {
                    if let Some(own) = keeping.own() {
                        output::warning(&format!("{local} captures {own}"));
                    }
                    if let Some(add) = self.for_host(&keeping.shared) {
                        output::warning(&format!(
                            "{local} captures this machine's version{} into the repo for every machine; {add} instead keeps {} to this machine",
                            keeping.of(),
                            keeping.them()
                        ));
                    }
                }
            }
        }
        Ok(actions)
    }

    /// The command whose flags answer what this pass leaves undecided.
    fn command(&self) -> &'static str {
        if self.opts.pass == Pass::Capture {
            "sync"
        } else {
            "apply"
        }
    }

    /// Keeping H means different things per pass: `sync` captures it.
    fn keep(&self, local: Action, item: &Item) -> Action {
        let into_template = item.r.as_ref().is_some_and(|r| r.template);
        if local == Action::KeepLocal && self.opts.pass == Pass::Capture && !into_template {
            Action::Capture
        } else {
            local
        }
    }

    /// The action for rows that need no question, with their reports.
    fn direct(&mut self, item: &Item) -> Option<Action> {
        let take_drift = self.opts.decisions.take_repo || self.opts.take_drift;
        let reports_drift = item.row == Row::TemplateDrift && !take_drift;
        if self.opts.pass == Pass::Capture && !reports_drift {
            return (item.row == Row::ChangedLocally).then_some(Action::Capture);
        }
        let key = &item.key;
        match item.row {
            Row::TemplateDrift if take_drift => Some(Action::Write),
            Row::TemplateDrift => {
                self.attention = true;
                // `sync` reports it in its capture pass; the pass after the pull leaves it alone.
                self.undecided.insert(key.clone());
                output::warning(&format!(
                    "{key}: template-drift; edit the template with `{bin} edit {path}`, or discard the local change with `{bin} apply --take-repo {path}`",
                    bin = name::bin(),
                    path = add::pasteable(&self.ctx.paths.home, key)
                ));
                None
            }
            Row::GoneEverywhere => Some(Action::DropState),
            Row::Forgotten => {
                output::info(&format!(
                    "{key}: forgotten upstream; the file stays in place"
                ));
                Some(Action::DropState)
            }
            Row::ForgottenEdited => {
                self.attention = true;
                output::warning(&format!(
                    "{key}: forgotten upstream but edited here; `{bin} add {path}` keeps it, `{bin} forget {path}` accepts the removal",
                    bin = name::bin(),
                    path = add::pasteable(&self.ctx.paths.home, key)
                ));
                None
            }
            Row::Clean { update_state } => update_state.then_some(Action::Record),
            Row::NotApplied | Row::ChangedInRepo => Some(Action::Write),
            Row::ChangedLocally => {
                self.left = self.left.saturating_add(1);
                output::info(&format!(
                    "{key}: changed locally; `{} sync` captures it",
                    name::bin()
                ));
                None
            }
            Row::Blocked => {
                self.attention = true;
                if let Home::Blocked(obstacle) = &item.raw {
                    self.skipped(key, &Skip::Blocked(obstacle.clone()));
                }
                None
            }
            Row::SymlinkAtTarget => {
                self.skipped(key, &Skip::Symlink);
                None
            }
            Row::DeletedLocally
            | Row::DeletedChangedUpstream
            | Row::Existing
            | Row::BothChanged => None,
        }
    }

    fn decide(
        &self,
        question: &Question,
        items: &[&Item],
        flag: Option<Choice>,
    ) -> Result<Vec<Choice>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(choice) = flag {
            return Ok(vec![choice; items.len()]);
        }
        // A dry run never asks, so it cannot block on an answer it would not use.
        if self.opts.dry_run || !prompt::interactive() {
            return Ok(vec![Choice::Skip; items.len()]);
        }
        output::context(&format!("{} {}:", items.len(), question.what));
        for item in items {
            output::context(&format!("  {:<26}{}", token(item.row), item.key));
        }
        let keeping = self.keeping_items(question, items);
        let letter = question.local.letter;
        if let Some(keeping) = keeping.as_ref().filter(|_| self.opts.pass == Pass::Capture) {
            if let Some(own) = keeping.own() {
                output::context(&format!("[{letter}] captures {own}"));
            }
            if let Some(add) = self.for_host(&keeping.shared) {
                let version = if keeping.shared.len() == 1 {
                    "version"
                } else {
                    "versions"
                };
                output::context(&format!(
                    "[{letter}] captures this machine's {version}{} into the repo for every machine; to keep {} to this machine only, answer [s] and run {add}",
                    keeping.of(),
                    keeping.them()
                ));
            }
        }
        let label = keeping.and_then(|keeping| keeping.label(true));
        loop {
            match parse_reply(question, &prompt::ask(&menu(question, true, label))?, true) {
                Some(Reply::Choose(choice)) => return Ok(vec![choice; items.len()]),
                Some(Reply::Diffs) => self.show_diffs(items),
                Some(Reply::PerFile) => {
                    return items
                        .iter()
                        .map(|item| self.decide_one(question, item))
                        .collect();
                }
                None => output::warning("not one of the listed answers"),
            }
        }
    }

    fn decide_one(&self, question: &Question, item: &Item) -> Result<Choice> {
        let label = self
            .keeping_items(question, &[item])
            .and_then(|keeping| keeping.label(false));
        loop {
            let asked = format!("{} {}", item.key, menu(question, false, label));
            match parse_reply(question, &prompt::ask(&asked)?, false) {
                Some(Reply::Choose(choice)) => return Ok(choice),
                Some(Reply::Diffs) => self.show_diffs(&[item]),
                Some(Reply::PerFile) | None => output::warning("not one of the listed answers"),
            }
        }
    }

    fn show_diffs(&self, items: &[&Item]) {
        let entries = |system: bool| -> Vec<(&str, &SourceEntry)> {
            items
                .iter()
                .filter(|item| item.system == system)
                .filter_map(|item| item.r.as_ref().map(|r| (item.key.as_str(), r)))
                .collect()
        };
        if let Err(e) = diff::show(self.ctx, &entries(false), &entries(true)) {
            output::warning(&format!("cannot show the diffs: {e:#}"));
        }
    }

    fn execute(
        &mut self,
        item: &Item,
        action: Action,
        state: &mut State,
        repo_config: &mut RepoConfig,
    ) {
        if item.system {
            self.execute_system(item, action, state);
            return;
        }
        let key = &item.key;
        match action {
            Action::DropState => {
                state.files.remove(key);
            }
            Action::Forget => {
                self.announce("forget", key);
                self.forget.insert(key.clone());
            }
            Action::Capture => {
                let Home::Present(h) = &item.raw else { return };
                if !self.save_repo_edit(item, h, state.files.get(key)) {
                    self.uncaptured = self.uncaptured.saturating_add(1);
                    return;
                }
                let source = item.r.as_ref().map_or(key.as_str(), |r| r.source.as_str());
                let encrypted = item.r.as_ref().is_some_and(|r| r.encrypted);
                let before = repo_config.modes.get(source).cloned();
                if self.capture(key, source, h.kind, encrypted, state, repo_config) {
                    // Row 9 is an edit made here; any other row got here by keep-local.
                    if item.row == Row::ChangedLocally {
                        self.announce("capture", key);
                    } else {
                        let replaced = if variant::names_host(source, &self.ctx.identity().host) {
                            format!("{source}, which only this machine applies")
                        } else {
                            "the repo version for every machine".to_owned()
                        };
                        self.announce("capture", &format!("{key} (replaces {replaced})"));
                        self.kept.push(key.clone());
                    }
                }
                // An edit to a file whose own mode is kept here travels as content only, so
                // other machines keep the repo's mode.
                let kept = item.kept().filter(|(mode, _)| *mode == h.mode);
                if let (Some((mode, repo)), Some(entry), false) =
                    (kept, state.files.get_mut(key), self.opts.dry_run)
                    && self.captured.contains(key)
                {
                    match before {
                        Some(value) => repo_config.modes.insert(source.to_owned(), value),
                        None => repo_config.modes.remove(source),
                    };
                    entry.mode = format!("{mode:04o}");
                    entry.repo_mode = Some(format!("{repo:04o}"));
                }
            }
            Action::Record | Action::KeepLocal | Action::Write => {
                let Some(r) = &item.r else { return };
                let written = match (action, &item.raw) {
                    (Action::Write, _) => self.write(item, r),
                    // S takes R's bytes, so the next sync captures H over the repo copy without
                    // seeing them as unsaved.
                    (Action::KeepLocal, Home::Present(h))
                        if !self.save_repo_edit(item, h, state.files.get(key)) =>
                    {
                        None
                    }
                    (Action::KeepLocal, _) => {
                        self.announce("keep local", key);
                        if !r.template {
                            self.kept.push(key.clone());
                        }
                        Some(r.tuple.mode)
                    }
                    _ => Some(r.tuple.mode),
                };
                if let Some(mode) = written {
                    state.files.insert(key.clone(), recorded(item, r, mode));
                }
            }
        }
    }

    /// System files are never written here. `sync`'s capture pass copies edits made to them into
    /// the repo, and any other pass says once that the rest needs `apply --system`.
    fn system(&mut self, state: &mut State, repo_config: &mut RepoConfig) {
        let found = match system::evaluate(self.ctx, repo_config, state) {
            Ok(found) => found,
            Err(e) => {
                output::warning(&format!("system files: skipped: {e:#}"));
                return;
            }
        };
        let capturing = self.opts.pass == Pass::Capture;
        let mut pending = 0_usize;
        for item in found {
            let key = item.key;
            match item.verdict {
                Verdict::Row {
                    row: Row::ChangedLocally,
                    r: Some(r),
                    ..
                } if capturing => self.capture_system(&key, &r.source, state, repo_config),
                Verdict::Row { row, .. } if !capturing && !item.off && system::needs_write(row) => {
                    pending = pending.saturating_add(1);
                }
                Verdict::Unreadable(why) if !capturing && !item.off => {
                    output::warning(&format!("{key}: skipped, it cannot be read: {why}"));
                }
                _ => {}
            }
        }
        if pending > 0 {
            self.left = self.left.saturating_add(pending);
            output::info(&format!(
                "{} {} from the repo; `{} apply --system` writes {}",
                system_files(pending),
                if pending == 1 { "differs" } else { "differ" },
                name::bin(),
                if pending == 1 { "it" } else { "them" }
            ));
        }
    }

    /// The system keys `filters` select, as items for the same questions home keys get. A key
    /// outside every root is listed and left alone.
    fn system_items(
        &mut self,
        state: &State,
        repo_config: &RepoConfig,
        filters: &[String],
    ) -> Result<Vec<Item>> {
        let found = system::evaluate(self.ctx, repo_config, state)?;
        let all = found.iter().map(|item| item.key.clone()).collect();
        let wanted = system::select_keys(filters, &all)?;
        let mut items = Vec::new();
        for item in found {
            let key = item.key;
            if !wanted.contains(&key) || self.opts.skip.contains(&key) {
                continue;
            }
            match item.verdict {
                Verdict::Refused(why) => {
                    self.attention = true;
                    output::warning(&format!("{key}: skipped, {why}"));
                }
                Verdict::Invalid(problems) => {
                    self.attention = true;
                    for (source, reason) in problems {
                        output::warning(&format!("{key}: skipped, {source}: {reason}"));
                    }
                }
                Verdict::Ambiguous(tied) => {
                    self.attention = true;
                    output::warning(&format!(
                        "{key}: {} are equally specific here; skipped until one is removed",
                        tied.join(", ")
                    ));
                }
                _ if item.off => {
                    output::info(&format!(
                        "{key}: system-off, outside [system] roots; skipped"
                    ));
                }
                Verdict::Unreadable(why) => {
                    self.attention = true;
                    output::warning(&format!(
                        "{key}: refused, it cannot be read: {why}; {}",
                        system::SECRETS_NOTE
                    ));
                }
                Verdict::Row { row, r, h } => {
                    items.push(Item {
                        key,
                        row,
                        r,
                        raw: h,
                        repo_mode: None,
                        basis: None,
                        identity: false,
                        system: true,
                    });
                }
            }
        }
        Ok(items)
    }

    /// S changes at once; writes wait for [`Run::write_system`].
    fn execute_system(&mut self, item: &Item, action: Action, state: &mut State) {
        let key = &item.key;
        match (action, &item.r) {
            (Action::DropState, _) => {
                state.system.remove(key);
            }
            (Action::Forget, _) => {
                self.announce("forget", key);
                self.forget_system.insert(key.clone());
            }
            (Action::Write, Some(r)) => self.system_writes.push(SystemWrite {
                key: key.clone(),
                r: r.clone(),
                h: item.raw.clone(),
            }),
            (Action::KeepLocal | Action::Record, Some(r)) => {
                if action == Action::KeepLocal {
                    self.announce("keep local", key);
                }
                state.system.insert(key.clone(), r.tuple.entry(&r.source));
            }
            // Only `sync`'s pass before the pull captures, and it never takes system items.
            (Action::Capture, _) | (_, None) => {}
        }
    }

    /// Reads each target again as the user, shows what changes, asks once, backs up, then
    /// writes them all through one escalation. S follows only what the helper reports written.
    fn write_system(&mut self, state: &mut State, repo_config: &RepoConfig) {
        let pending = std::mem::take(&mut self.system_writes);
        if pending.is_empty() {
            return;
        }
        let scope = self.ctx.system_scope();
        let ready: Vec<Ready> = pending
            .into_iter()
            .filter_map(|write| self.reread(&scope, repo_config, write))
            .collect();
        if ready.is_empty() {
            return;
        }
        show_system(self.ctx, &ready);
        if self.opts.dry_run {
            for item in &ready {
                self.announce("write", &item.key);
            }
            if !self.opts.yes {
                self.unconfirmed(&ready);
            }
            return;
        }
        let Some(escalator) = self.escalator(ready.len()) else {
            return;
        };
        if !self.confirmed(&ready) {
            return;
        }
        let ready = self.back_up_system(ready);
        if ready.is_empty() {
            return;
        }
        let plan = match plan_of(self.ctx, &scope, &ready) {
            Ok(plan) => plan,
            Err(e) => {
                output::warning(&format!("system files: {e:#}"));
                self.failed = self.failed.saturating_add(ready.len());
                return;
            }
        };
        let name = escalator.tool.name();
        let ran = match escalator.run(&self.ctx.paths.state_dir, &plan, prompt::interactive()) {
            Ok(ran) => ran,
            Err(e) => {
                output::warning(&format!("system files not written: {e:#}"));
                self.failed = self.failed.saturating_add(ready.len());
                return;
            }
        };
        let mut silent = true;
        for item in &ready {
            let key = &item.key;
            match helper::reported(&ran.stdout, &item.target) {
                Some((Reported::Written | Reported::OwnerSkipped, _)) => {
                    silent = false;
                    self.announce("apply", key);
                    state
                        .system
                        .insert(key.clone(), item.r.tuple.entry(&item.r.source));
                    self.written.insert(key.clone());
                    if escalator.tool == Tool::None {
                        output::info(&format!(
                            "{key}: owner and group left as they are, because escalate = \"none\""
                        ));
                    }
                }
                Some((Reported::Failed, why)) => {
                    silent = false;
                    self.failed = self.failed.saturating_add(1);
                    output::warning(&format!("{key}: cannot be applied: {why}"));
                }
                Some((Reported::NotWritten, _)) => {
                    silent = false;
                    self.failed = self.failed.saturating_add(1);
                    output::warning(&format!(
                        "{key}: not written, an earlier system file failed"
                    ));
                }
                None => self.failed = self.failed.saturating_add(1),
            }
        }
        if silent && !ran.status.success() {
            output::warning(&format!(
                "{name} did not run system-write, or it refused the plan ({}); {}",
                ran.status, ran.hint
            ));
        }
    }

    /// The target as the user reads it now. Anything other than what was classified skips the
    /// key as a conflict; a file the user cannot read is refused, because it is likely a secret.
    fn reread(
        &mut self,
        scope: &Scope,
        repo_config: &RepoConfig,
        write: SystemWrite,
    ) -> Option<Ready> {
        let key = write.key;
        let current = match scope.node(&key) {
            Ok(Node::File {
                bytes,
                mode,
                uid,
                gid,
                ..
            }) => Some((bytes, mode & 0o777, uid, gid)),
            Ok(Node::Missing) => None,
            Ok(_) => {
                self.attention = true;
                output::warning(&format!("{key}: {}", Skip::Changed));
                return None;
            }
            Err(e) if system::is_unreadable(&e) => {
                self.attention = true;
                output::warning(&format!(
                    "{key}: refused, you cannot read it: {}",
                    system::SECRETS_NOTE
                ));
                return None;
            }
            Err(e) => {
                self.failed = self.failed.saturating_add(1);
                output::warning(&format!("{key}: cannot be applied: {e:#}"));
                return None;
            }
        };
        let now = current.as_ref().map_or(Home::Missing, |(bytes, mode, ..)| {
            Home::Present(Tuple::of_file(bytes, *mode))
        });
        let Content::Bytes(bytes) = &write.r.content else {
            return None;
        };
        if now != write.h {
            self.attention = true;
            output::warning(&format!("{key}: {}", Skip::Changed));
            return None;
        }
        // A source that arrived without a [system] record belongs to root, as system files do.
        let record = repo_config.system.get(&key);
        let named = |name: Option<&String>| name.filter(|name| !name.is_empty()).cloned();
        let owner = named(record.map(|file| &file.owner)).unwrap_or_else(|| "root".to_owned());
        let group = named(record.map(|file| &file.group)).unwrap_or_else(|| {
            sys::group_by_gid(0).map_or_else(|| "root".to_owned(), |group| group.name)
        });
        Some(Ready {
            target: scope.target(&key),
            key,
            bytes: bytes.clone(),
            r: write.r,
            owner,
            group,
            current,
        })
    }

    /// `None` counts the `pending` writes as failed.
    fn escalator(&mut self, pending: usize) -> Option<Escalator> {
        let setting = self.ctx.machine.system.escalate.as_deref();
        let escalator =
            match Escalator::choose(setting, &self.ctx.paths.config_file, sys::geteuid()) {
                Ok(escalator) => escalator,
                Err(e) => {
                    output::warning(&format!("{e:#}"));
                    self.failed = self.failed.saturating_add(pending);
                    return None;
                }
            };
        if escalator.tool == Tool::Root
            && let Some(named) = setting.filter(|name| *name != "none")
        {
            output::info(&format!(
                "{} runs as root, so `escalate = \"{named}\"` is not used",
                name::bin()
            ));
        }
        Some(escalator)
    }

    fn confirmed(&mut self, ready: &[Ready]) -> bool {
        if self.opts.yes {
            return true;
        }
        if prompt::interactive() {
            let question = format!("write {} as root? [y/N]", system_files(ready.len()));
            match prompt::confirm(&question) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => output::warning(&format!("{e:#}")),
            }
        }
        self.unconfirmed(ready);
        false
    }

    fn unconfirmed(&mut self, ready: &[Ready]) {
        self.attention = true;
        output::warning(&format!(
            "{} left unwritten; answer with `{} apply --system --yes`:",
            system_files(ready.len()),
            name::bin()
        ));
        for item in ready {
            output::warning(&format!("  {}", item.key));
            self.undecided.insert(item.key.clone());
        }
    }

    /// Copies each current target into the run's backups as `.dotman/system/<path>`, so it can
    /// never land on a home key. A target that cannot be backed up is not written.
    fn back_up_system(&mut self, ready: Vec<Ready>) -> Vec<Ready> {
        let mut kept = Vec::new();
        for item in ready {
            let Some((bytes, ..)) = &item.current else {
                kept.push(item);
                continue;
            };
            let rel = system::source_path(item.key.trim_start_matches('/'));
            match back_up_bytes(&self.backups, &rel, bytes) {
                Ok(path) => {
                    self.backed_up(&path);
                    kept.push(item);
                }
                Err(e) => {
                    self.failed = self.failed.saturating_add(1);
                    output::warning(&format!(
                        "{}: not written, the backup failed: {e:#}",
                        item.key
                    ));
                }
            }
        }
        kept
    }

    fn capture_system(
        &mut self,
        key: &str,
        source: &str,
        state: &mut State,
        repo_config: &mut RepoConfig,
    ) {
        let dry_run = self.opts.dry_run;
        match system::capture(self.ctx, key, source, repo_config, dry_run) {
            Ok(tuple) => {
                self.announce("capture", key);
                if !dry_run {
                    state.system.insert(key.to_owned(), tuple.entry(source));
                }
                self.captured.insert(key.to_owned());
            }
            Err(e) if e.is::<HoldsKey>() => {
                self.attention = true;
                self.undecided.insert(key.to_owned());
                output::warning(&format!(
                    "{key}: held, {}, and system scope does not encrypt",
                    secrets::system_finding(self.ctx, key)
                ));
            }
            Err(e) => {
                self.failed = self.failed.saturating_add(1);
                self.uncaptured = self.uncaptured.saturating_add(1);
                output::warning(&format!("{key}: cannot be captured: {e:#}"));
            }
        }
    }

    /// Captures the new files and lists the held ones, which keep the run at exit 3.
    fn capture_new(&mut self, scan: &Scan, state: &mut State, repo_config: &mut RepoConfig) {
        for (key, kind) in &scan.new {
            if self.capture(key, key, *kind, false, state, repo_config) {
                self.announce("capture", key);
            }
        }
        for note in &scan.notes {
            output::warning(note);
        }
        if !scan.held.is_empty() {
            self.attention = true;
            output::warning(&format!(
                "{} new files held back; an [ignore] pattern in .dotman.toml silences one:",
                scan.held.len(),
            ));
            for (key, hold) in &scan.held {
                let path = add::pasteable(&self.ctx.paths.home, key);
                output::warning(&format!("  {key} ({})", hold.listed(&path)));
            }
        }
    }

    /// True when `key` was captured, or in a dry run would be; the caller announces it.
    fn capture(
        &mut self,
        key: &str,
        source: &str,
        kind: Kind,
        encrypted: bool,
        state: &mut State,
        repo_config: &mut RepoConfig,
    ) -> bool {
        if is_real_dir(&self.ctx.paths.repo.join(source)) {
            self.attention = true;
            self.undecided.insert(key.to_owned());
            output::warning(&format!(
                "{key}: a file here, but a directory of managed files in the repo; skipped. To manage the file instead: `{bin} forget {path}`, then `{bin} add {path}`",
                bin = name::bin(),
                path = add::pasteable(&self.ctx.paths.home, key)
            ));
            return false;
        }
        let managed = self.sources.contains(key) || state.files.contains_key(key);
        if self.opts.dry_run {
            // Only a dry run reads H here; a real capture checks what it is about to store.
            let found = if encrypted {
                secrets::held_name(source)
            } else {
                add::holds_key(&self.ctx.paths.home, key, &self.ctx.machine.guard.allow)
            };
            if let Some(found) = found {
                self.held_key(key, found, managed);
                return false;
            }
            self.captured.insert(key.to_owned());
            return true;
        }
        let done = add::commit_one(
            self.ctx,
            repo_config,
            &mut self.dir_modes,
            key,
            source,
            kind,
            encrypted,
        );
        match done {
            Ok(entry) => {
                state.files.insert(key.to_owned(), entry);
                self.captured.insert(key.to_owned());
                return true;
            }
            Err(e) => match e.downcast::<Held>() {
                Ok(Held(found)) => self.held_key(key, found, managed),
                Err(e) => {
                    self.failed = self.failed.saturating_add(1);
                    self.uncaptured = self.uncaptured.saturating_add(1);
                    output::warning(&format!("{key}: cannot be captured: {e:#}"));
                }
            },
        }
        false
    }

    /// Never staged or committed; the pass after the pull leaves it alone too. An `[ignore]`
    /// pattern keeps out only a file that is not managed yet.
    fn held_key(&mut self, key: &str, found: Finding, managed: bool) {
        self.attention = true;
        self.undecided.insert(key.to_owned());
        let path = add::pasteable(&self.ctx.paths.home, key);
        let keep_out = if managed {
            add::forget_hint(&self.ctx.paths.home, self.sources, key)
        } else {
            "keep it out with an [ignore] pattern".to_owned()
        };
        output::warning(&format!(
            "{key}: held, {found}{}; or {keep_out}",
            found.remedy(Some(&path))
        ));
    }

    /// Copies the repo copy of `item` into the run's backups, as `.dotman/repo/<source>` so it can
    /// never land on a home key's backup, before keeping H means replacing it. Needed when its
    /// bytes are nowhere else: not in HEAD, not H's and not what S records, the copy `add`
    /// refuses to overwrite. A hand edit in the repo or a `dotman edit` leaves one. False, with
    /// the failure counted, when such a copy could not be saved.
    fn save_repo_edit(&mut self, item: &Item, h: &Tuple, recorded: Option<&Entry>) -> bool {
        let Some(r) = item.r.as_ref().filter(|r| !r.template) else {
            return true;
        };
        let applied = recorded.is_some_and(|s| s.source == r.source && s.hash == r.tuple.hash);
        if r.tuple.hash == h.hash || applied {
            return true;
        }
        let Ok(Node::File { bytes, .. }) = read_node(&self.ctx.paths.repo, &r.source) else {
            return true;
        };
        let key = &item.key;
        let git = &self.ctx.git;
        let committed = git.head().and_then(|head| {
            if head.is_none() || git.tree_mode("HEAD", &r.source)?.is_none() {
                return Ok(false);
            }
            Ok(git.show_blob("HEAD", &r.source)? == bytes)
        });
        let saved = match committed {
            Ok(true) => return true,
            Ok(false) if self.opts.dry_run => {
                let path = self.backups.join(".dotman/repo").join(&r.source);
                output::warning(&format!(
                    "{key}: would save the repo copy's uncommitted edit at {}",
                    path.display()
                ));
                return true;
            }
            Ok(false) => {
                back_up_bytes(&self.backups, &format!(".dotman/repo/{}", r.source), &bytes)
            }
            Err(e) => Err(e),
        };
        match saved {
            Ok(path) => {
                output::warning(&format!(
                    "{key}: the repo copy's uncommitted edit is saved at {}",
                    path.display()
                ));
                true
            }
            Err(e) => {
                self.failed = self.failed.saturating_add(1);
                output::warning(&format!(
                    "{key}: left as it is, because the repo copy's uncommitted edit could not be saved: {e:#}"
                ));
                false
            }
        }
    }

    /// Says once per run where the files kept local go, and how to keep them to this machine.
    /// `sync`'s capture pass has already captured them, so there it says nothing: its question
    /// offers the way to keep them here before the answer.
    fn report_kept(&self) {
        if self.kept.is_empty() || self.opts.pass == Pass::Capture {
            return;
        }
        let bin = name::bin();
        let keeping = self.keeping(self.kept.iter().map(String::as_str));
        if let Some(own) = keeping.own() {
            output::info(&format!("kept; the next `{bin} sync` captures {own}"));
        }
        if keeping.shared.is_empty() {
            return;
        }
        let them = keeping.them();
        let what = if keeping.own.is_empty() {
            them.to_owned()
        } else {
            keeping.shared.join(", ")
        };
        let mut said = vec![format!(
            "kept; the next `{bin} sync` captures {what} into the repo for every machine"
        )];
        if let Some(add) = self.for_host(&keeping.shared) {
            said.push(format!("{add} keeps {them} to this machine"));
        }
        output::info(&said.join("; "));
    }

    /// In `sync`'s capture pass, where keeping H captures it at once, where `items` would go; the
    /// `add --for` that keeps the shared ones to this machine instead only works while they are
    /// left undecided.
    fn keep_here<'i>(&self, question: &Question, items: &[&'i Item]) -> Option<Keeping<'i>> {
        if self.opts.pass != Pass::Capture {
            return None;
        }
        self.keeping_items(question, items)
    }

    /// Where keeping `$HOME`'s version of `items` puts them, for a question whose local answer
    /// keeps it. Templates and system files are left out: keeping H never captures over them.
    fn keeping_items<'i>(&self, question: &Question, items: &[&'i Item]) -> Option<Keeping<'i>> {
        (question.local_action == Action::KeepLocal).then(|| {
            self.keeping(
                items
                    .iter()
                    .filter(|item| !item.system && !item.r.as_ref().is_some_and(|r| r.template))
                    .map(|item| item.key.as_str()),
            )
        })
    }

    fn keeping<'k>(&self, keys: impl IntoIterator<Item = &'k str>) -> Keeping<'k> {
        let identity = self.ctx.identity();
        let mut keeping = Keeping::default();
        for key in keys {
            let own = self
                .sources
                .select(key, identity)
                .winner()
                .filter(|source| variant::names_host(source, &identity.host))
                .map(|source| (*source).to_owned());
            match own {
                Some(source) => keeping.own.push((key, source)),
                None => keeping.shared.push(key),
            }
        }
        keeping
    }

    /// `` `<bin> add --for host=<host> <paths>` ``, when `keys` is not empty and the host name is
    /// one a variant condition can hold. A key with an `{age}` source, for whichever machine, gets
    /// a command of its own with `--encrypt`, by the rule `add --for` refuses a plain variant by.
    fn for_host(&self, keys: &[&str]) -> Option<String> {
        let host = &self.ctx.identity().host;
        // A variant condition takes only these characters.
        let named = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !named || keys.is_empty() {
            return None;
        }
        let command = |encrypt: bool| {
            let paths: Vec<String> = keys
                .iter()
                .filter(|key| self.stored_encrypted(key) == encrypt)
                .map(|key| add::pasteable(&self.ctx.paths.home, key))
                .collect();
            let flag = if encrypt { "--encrypt " } else { "" };
            (!paths.is_empty()).then(|| {
                format!(
                    "`{} add {flag}--for host={host} {}`",
                    name::bin(),
                    paths.join(" ")
                )
            })
        };
        let commands: Vec<String> = [false, true].into_iter().filter_map(command).collect();
        Some(commands.join(" and "))
    }

    fn stored_encrypted(&self, key: &str) -> bool {
        self.sources
            .sources_of(key)
            .any(|source| self.sources.flag(key, source) == Flag::Age)
    }

    /// The mode R reached `$HOME` with; `None` when it did not. A skipped or failed write is
    /// reported here.
    fn write(&mut self, item: &Item, r: &SourceEntry) -> Option<u32> {
        let key = &item.key;
        let mode = self.kept_mode(item, r);
        // Whatever S says: after a rebase that dropped the local commit, H can equal S and still
        // be the only copy of the user's bytes.
        let replaces = matches!(&item.raw, Home::Present(h)
            if self.opts.in_place || h.kind != r.tuple.kind || h.hash != r.tuple.hash);
        // A change of mode alone keeps the bytes, so only the mode they had is recorded.
        let mut mode_record = None;
        if let Home::Present(h) = &item.raw
            && !replaces
            && h.kind == Kind::File
            && h.mode != mode
        {
            match record_mode(&self.backups, key, h.mode, self.opts.dry_run) {
                Ok(path) => mode_record = Some(path),
                Err(e) => {
                    self.failed = self.failed.saturating_add(1);
                    output::warning(&format!("{key}: not written, the backup failed: {e:#}"));
                    return None;
                }
            }
        }
        let opts = WriteOptions {
            dry_run: self.opts.dry_run,
            in_place: self.opts.in_place,
            backup: replaces.then_some(self.backups.as_path()),
            expected: &item.raw,
            modes: &self.modes,
        };
        let home = &self.ctx.paths.home;
        let outcome = match &r.content {
            // Row 10 found H equal to S, so a link there is the one dotman applied.
            Content::Bytes(bytes) if item.row == Row::ChangedInRepo => {
                safe_write_over_link(home, key, bytes, mode, &opts)
            }
            Content::Bytes(bytes) => safe_write(home, key, bytes, mode, &opts),
            Content::Link(target) => safe_symlink(home, key, target, &opts),
        };
        match outcome {
            Ok(WriteOutcome::Written(written)) => {
                self.written.insert(key.clone());
                if r.encrypted {
                    self.announce("apply", &format!("{key} (decrypted)"));
                } else {
                    self.announce("apply", key);
                }
                if let Some(backup) = written.backup.as_ref().or(mode_record.as_ref()) {
                    self.backed_up(backup);
                }
                let repo = item.repo_mode.unwrap_or(r.tuple.mode);
                if mode != repo && mode != r.tuple.mode {
                    output::info(&format!(
                        "{key}: kept at {mode:04o} here; the repo says {repo:04o}, but dotman never gives group or others access the file did not have. A [modes] entry in .dotman.toml sets the mode for every machine"
                    ));
                }
                if let Some(warning) = written.warning() {
                    output::warning(&format!("{key}: {warning}"));
                }
                if written.in_place {
                    output::info(&format!("{key}: rewritten in place"));
                }
                Some(mode)
            }
            Ok(WriteOutcome::Skipped(skip)) => {
                self.skipped(key, &skip);
                None
            }
            Err(e) => {
                self.failed = self.failed.saturating_add(1);
                output::warning(&format!("{key}: cannot be applied: {e:#}"));
                None
            }
        }
    }

    /// The repo's mode, narrowed by [`never_widen`] over a file already in `$HOME`. Without a
    /// `[modes]` entry the repo's mode is mostly git's default, which must not open up a file
    /// here. A `[modes]` entry is meant for every machine, so it applies as it is, and so does a
    /// mode whose read and write bits changed in the repo since S, as when another machine
    /// loosened one back to git's default and the capture removed its entry.
    fn kept_mode(&self, item: &Item, r: &SourceEntry) -> u32 {
        let repo = item.repo_mode.unwrap_or(r.tuple.mode);
        let Home::Present(h) = &item.raw else {
            return repo;
        };
        let changed = item
            .basis
            .is_some_and(|basis| basis & 0o666 != repo & 0o666);
        if h.kind != Kind::File
            || r.tuple.kind != Kind::File
            || changed
            || self.modes.contains_key(&r.source)
        {
            repo
        } else {
            never_widen(repo, h.mode)
        }
    }

    /// Reports a write left undone. What stays in the way of one is the user's, so forgetting the
    /// key is named as a way out.
    fn skipped(&mut self, key: &str, skip: &Skip) {
        self.attention = true;
        let way_out = if matches!(skip, Skip::Blocked(_) | Skip::Symlink) {
            format!(
                ". {}",
                add::forget_hint(&self.ctx.paths.home, self.sources, key)
            )
        } else {
            String::new()
        };
        output::warning(&format!("{key}: {skip}{way_out}"));
    }

    fn announce(&mut self, verb: &str, key: &str) {
        self.announced = self.announced.saturating_add(1);
        if self.opts.dry_run {
            output::action(&format!("would {verb}"), key);
        } else {
            output::action(verb, key);
        }
    }

    fn backed_up(&self, backup: &Path) {
        let verb = if self.opts.dry_run {
            "would back up to"
        } else {
            "backed up to"
        };
        output::info(&format!("  {verb} {}", backup.display()));
    }
}

/// R, H and S of `key` and its row, once variants and the identity snapshot are taken into account.
pub(crate) fn classify_key(
    ctx: &Context,
    variants: &Variants<'_>,
    state: &State,
    key: &str,
) -> Result<Classified> {
    let selection = match variants.get(key) {
        Ok(selection) => selection,
        Err(e) => {
            return e
                .downcast::<Locked>()
                .map(|locked| Classified::Locked(locked.0));
        }
    };
    let mut r = match selection {
        Selection::None => None,
        Selection::Winner(entry) => Some(entry),
        Selection::Ambiguous(tied) => return Ok(Classified::Ambiguous(tied)),
        Selection::Invalid(problems) => return Ok(Classified::Invalid(problems)),
    };
    let home = &ctx.paths.home;
    let h = observe_portable(home, ctx.paths.home_real.as_deref(), key).context(HomeUnreadable)?;
    if let Home::Present(tuple) = &h
        && tuple.kind == Kind::File
    {
        warn_special_bits(ctx, key);
    }
    let recorded = state.files.get(key);
    let s = recorded.map(Tuple::from_entry).transpose()?;
    let kept_against = recorded
        .and_then(|entry| entry.repo_mode.as_deref())
        .map(parse_mode)
        .transpose()?;
    let basis = kept_against.or_else(|| s.as_ref().map(|s| s.mode));
    // A mode this machine keeps is its own, not a local change, while the repo's mode is
    // the one it was kept against.
    let mut repo_mode = None;
    if let (Some(entry), Some(s), Some(against)) = (r.as_mut(), s.as_ref(), kept_against)
        && entry.tuple.kind == Kind::File
        && entry.tuple.mode == against
        && !variants.mode_is_explicit(&entry.source)
    {
        repo_mode = Some(against);
        entry.tuple.mode = s.mode;
    }
    let Some(row) = classify(r.as_ref().map(|e| &e.tuple), &h, s.as_ref()) else {
        return Ok(Classified::Unmanaged);
    };
    let winner = r.as_ref().map(|r| r.source.as_str());
    let switched = recorded.is_some_and(|s| Some(s.source.as_str()) != winner);
    // A switch that a commit caused, such as a new variant upstream, follows the rows directly.
    let identity = switched
        && state
            .identity
            .as_ref()
            .is_some_and(|old| old != ctx.identity() && variants.winner_under(key, old) != winner);
    let row = if switched { row.switched() } else { row };
    let row = if r.as_ref().is_some_and(|r| r.template) {
        row.drifted()
    } else {
        row
    };
    let raw = match &h {
        Home::Present(tuple) if tuple.kind == Kind::Symlink => observe(home, key)?,
        _ => h,
    };
    Ok(Classified::Item(Item {
        key: key.to_owned(),
        row,
        r,
        raw,
        repo_mode,
        basis,
        identity,
        system: false,
    }))
}

/// S for `r` written with `mode`, recording the repo's mode when it is not the one written.
fn recorded(item: &Item, r: &SourceEntry, mode: u32) -> Entry {
    let repo = item.repo_mode.unwrap_or(r.tuple.mode);
    let mut entry = Tuple {
        mode,
        ..r.tuple.clone()
    }
    .entry(&r.source);
    if r.tuple.kind == Kind::File && mode != repo {
        entry.repo_mode = Some(format!("{repo:04o}"));
    }
    entry
}

/// H's tuple leaves these bits out, so a file that has them still settles; this says so once per
/// run.
fn warn_special_bits(ctx: &Context, key: &str) {
    let mode = fs::symlink_metadata(ctx.paths.home.join(key)).map_or(0, |meta| meta.mode());
    let bits: Vec<&str> = [(0o4000, "setuid"), (0o2000, "setgid"), (0o1000, "sticky")]
        .into_iter()
        .filter(|(bit, _)| mode & bit != 0)
        .map(|(_, name)| name)
        .collect();
    if !bits.is_empty() {
        ctx.warn_once(
            key,
            &format!(
                "{key}: has the {} {}, which dotman does not track; a write from the repo drops {}",
                bits.join(" and "),
                if bits.len() == 1 { "bit" } else { "bits" },
                if bits.len() == 1 { "it" } else { "them" }
            ),
        );
    }
}

fn system_files(n: usize) -> String {
    format!("{n} system {}", if n == 1 { "file" } else { "files" })
}

/// Each target with the owner, group and mode it gets and has now, then the diff.
fn show_system(ctx: &Context, ready: &[Ready]) {
    let user = |uid: u32| sys::passwd_by_uid(uid).map_or_else(|| uid.to_string(), |pw| pw.name);
    let group = |gid: u32| sys::group_by_gid(gid).map_or_else(|| gid.to_string(), |gr| gr.name);
    output::context(&format!("{} to write as root:", system_files(ready.len())));
    for item in ready {
        let now = item.current.as_ref().map_or_else(
            || "new".to_owned(),
            |(_, mode, uid, gid)| format!("now {}:{} {mode:04o}", user(*uid), group(*gid)),
        );
        output::context(&format!(
            "  {}  {}:{} {:04o}  ({now})",
            item.key, item.owner, item.group, item.r.tuple.mode
        ));
    }
    let entries: Vec<(&str, &SourceEntry)> = ready
        .iter()
        .map(|item| (item.key.as_str(), &item.r))
        .collect();
    if let Err(e) = diff::show_writes(ctx, &entries) {
        output::warning(&format!("cannot show the diffs: {e:#}"));
    }
}

fn plan_of(ctx: &Context, scope: &Scope, ready: &[Ready]) -> Result<Plan> {
    let utf8 = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("{} is not valid UTF-8", path.display()))
    };
    Ok(Plan {
        config: utf8(&ctx.paths.config_file)?,
        roots: helper::usable_roots(scope),
        prefix: scope.prefix().map(utf8).transpose()?,
        entries: ready
            .iter()
            .map(|item| PlanEntry {
                target: item.target.clone(),
                bytes: helper::hex(&item.bytes),
                xxh3: hash_hex(&item.bytes),
                owner: item.owner.clone(),
                group: item.group.clone(),
                mode: format!("{:04o}", item.r.tuple.mode),
            })
            .collect(),
    })
}

fn summarize(items: &[Item], locked: usize) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for item in items {
        let count = counts.entry(item.token()).or_insert(0);
        *count = count.saturating_add(1);
    }
    if locked > 0 {
        counts.insert("locked", locked);
    }
    let parts: Vec<String> = counts.iter().map(|(t, n)| format!("{n} {t}")).collect();
    output::info(&format!(
        "{} managed paths: {}",
        items.len().saturating_add(locked),
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(", ")
        }
    ));
}

/// The answers to `question`; `label` replaces the local answer's text.
fn menu(question: &Question, bulk: bool, label: Option<&str>) -> String {
    let (local, repo) = if bulk {
        (question.local.all, question.repo.all)
    } else {
        (question.local.one, question.repo.one)
    };
    let local = label.unwrap_or(local);
    let mut answers = vec![
        format!("[{}] {local}", question.local.letter),
        format!("[{}] {repo}", question.repo.letter),
    ];
    if question.diffs {
        answers.push(
            if bulk {
                "[d] show diffs"
            } else {
                "[d] show diff"
            }
            .to_owned(),
        );
    }
    if bulk {
        answers.push("[p] decide per file".to_owned());
    }
    answers.push("[s] skip".to_owned());
    format!("{}:", answers.join("  "))
}

// An empty answer, which is also what end of input reads as, skips: the run never guesses.
fn parse_reply(question: &Question, answer: &str, bulk: bool) -> Option<Reply> {
    let answer = answer.to_lowercase();
    match answer.as_str() {
        "" | "s" => Some(Reply::Choose(Choice::Skip)),
        a if a == question.local.letter => Some(Reply::Choose(Choice::Local)),
        a if a == question.repo.letter => Some(Reply::Choose(Choice::Repo)),
        "d" if question.diffs => Some(Reply::Diffs),
        "p" if bulk => Some(Reply::PerFile),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_parse_per_question_and_empty_input_skips() {
        let [conflicts, _, deletions] = &QUESTIONS;
        let reply = |q: &Question, text: &str, bulk: bool| parse_reply(q, text, bulk);
        assert_eq!(
            reply(conflicts, "K", true),
            Some(Reply::Choose(Choice::Local))
        );
        assert_eq!(
            reply(conflicts, "t", false),
            Some(Reply::Choose(Choice::Repo))
        );
        assert_eq!(reply(conflicts, "d", true), Some(Reply::Diffs));
        assert_eq!(reply(conflicts, "p", true), Some(Reply::PerFile));
        assert_eq!(reply(conflicts, "p", false), None);
        assert_eq!(reply(conflicts, "f", true), None);
        assert_eq!(
            reply(deletions, "f", true),
            Some(Reply::Choose(Choice::Local))
        );
        assert_eq!(
            reply(deletions, "r", false),
            Some(Reply::Choose(Choice::Repo))
        );
        assert_eq!(reply(deletions, "d", true), None);
        for skip in ["", "s"] {
            assert_eq!(
                reply(deletions, skip, true),
                Some(Reply::Choose(Choice::Skip))
            );
        }
        assert_eq!(
            menu(conflicts, true, None),
            "[k] keep all local (capture them into the repo for every machine)  [t] take all from repo  [d] show diffs  [p] decide per file  [s] skip:"
        );
        assert_eq!(
            menu(conflicts, false, None),
            "[k] keep local (capture it into the repo for every machine)  [t] take from repo  [d] show diff  [s] skip:"
        );
        assert_eq!(
            menu(deletions, false, None),
            "[f] forget  [r] restore  [s] skip:"
        );
    }

    #[test]
    fn flags_answer_their_own_category_only() {
        let [conflicts, existing, deletions] = &QUESTIONS;
        let take = Decisions {
            take_repo: true,
            ..Decisions::default()
        };
        assert_eq!((conflicts.flag)(take), Some(Choice::Repo));
        assert_eq!((existing.flag)(take), Some(Choice::Repo));
        assert_eq!((deletions.flag)(take), None);
        let forget = Decisions {
            forget_deleted: true,
            ..Decisions::default()
        };
        assert_eq!((deletions.flag)(forget), Some(Choice::Local));
        assert_eq!((conflicts.flag)(forget), None);
        assert!((conflicts.rows)(Row::BothChanged));
        assert!(!(conflicts.rows)(Row::SymlinkAtTarget));
        assert!(!(conflicts.rows)(Row::DeletedChangedUpstream));
        assert!((deletions.rows)(Row::DeletedChangedUpstream));
    }
}
