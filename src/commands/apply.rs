//! `dotman apply`: write R into `$HOME`. Paths that need a decision are asked about once per
//! category, never per file unless the user picks that; a run that cannot ask leaves them alone.

use crate::cli::{ApplyArgs, Decisions};
use crate::commands::add::{self, Scan};
use crate::commands::status::token;
use crate::commands::{Outcome, diff, forget};
use crate::context::Context;
use crate::engine::status::{Content, Row, Source, SourceEntry, Variants, Worktree, classify};
use crate::engine::write::{
    Skip, WriteOptions, WriteOutcome, back_up_bytes, backup_run_dir, safe_symlink, safe_write,
};
use crate::engine::{Home, Node, Tuple, hash_hex, observe, observe_portable};
use crate::hooks::{self, Policy};
use crate::identity;
use crate::lock::Lock;
use crate::name;
use crate::output;
use crate::prompt;
use crate::repo_config::RepoConfig;
use crate::secrets::{HoldsKey, Locked};
use crate::state::{IdentitySnapshot, Kind, State};
use crate::sys;
use crate::system::escalate::{Escalator, Tool};
use crate::system::helper::{self, Plan, PlanEntry, Reported};
use crate::system::{self, Scope, Verdict};
use crate::variant::{Selection, Sources};
use anyhow::{Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
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
    let _lock = Lock::acquire(&ctx.paths.lock_file(), args.dry_run)?;
    ctx.guard()?;
    ctx.refuse_collisions()?;
    let opts = Options {
        dry_run: args.dry_run,
        in_place: args.in_place,
        decisions: args.decisions,
        system: args.system,
        yes: args.yes,
        ..Options::default()
    };
    let applied = apply(&ctx, &args.paths, &opts)?;
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
    applied.check().and(hooked)?;
    Ok(applied.outcome)
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
    /// Keys that were asked about and left unanswered, or reported as template drift or held.
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
}

impl Applied {
    pub(crate) fn check(&self) -> Result<()> {
        check_failed(self.failed)
    }
}

pub(crate) fn check_failed(failed: usize) -> Result<()> {
    if failed > 0 {
        bail!(
            "{failed} {} could not be applied",
            if failed == 1 { "path" } else { "paths" }
        );
    }
    Ok(())
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

    let unchanged =
        run.forget.is_empty() && run.forget_system.is_empty() && run.captured.is_empty();
    let repo_saved = if opts.dry_run || unchanged {
        Ok(())
    } else {
        let removed = if run.forget.is_empty() {
            Ok(())
        } else {
            forget::remove_keys(ctx, &mut repo_config, &mut state, &run.forget)
        };
        removed
            .and_then(|()| {
                forget::remove_system_keys(ctx, &mut repo_config, &mut state, &run.forget_system)
            })
            .and_then(|()| repo_config.save(&ctx.repo_config_file(), false))
    };
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
    })
}

pub(crate) struct Item {
    key: String,
    row: Row,
    r: Option<SourceEntry>,
    /// H as the write path reads it again before renaming. It differs from the classified H only
    /// for an absolute link into `$HOME`, which classification compares in its relative form.
    raw: Home,
    /// H differs from S, so replacing it needs a backup first.
    local_changes: bool,
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
    pub(crate) const fn token(&self) -> &'static str {
        if self.identity {
            "identity-changed"
        } else {
            token(self.row)
        }
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
    flags: &'static str,
}

const KEEP: Answer = Answer {
    letter: "k",
    all: "keep all local",
    one: "keep local",
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
        rows: |row| matches!(row, Row::DeletedChangedUpstream | Row::BothChanged),
        flag: |d| choice(d.keep_local, d.take_repo),
        local: KEEP,
        local_action: Action::KeepLocal,
        repo: TAKE,
        diffs: true,
        flags: "--keep-local or --take-repo",
    },
    Question {
        what: "existing files",
        rows: |row| row == Row::Existing,
        flag: |d| choice(d.keep_local, d.take_repo),
        local: KEEP,
        local_action: Action::KeepLocal,
        repo: TAKE,
        diffs: true,
        flags: "--keep-local or --take-repo",
    },
    Question {
        what: "local deletions",
        rows: |row| row == Row::DeletedLocally,
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
        flags: "--forget-deleted or --restore-deleted",
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

struct Run<'a> {
    ctx: &'a Context,
    opts: &'a Options,
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
}

impl Run<'_> {
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
                Err(e) => {
                    output::warning(&format!("{key}: skipped: {e:#}"));
                    self.failed = self.failed.saturating_add(1);
                }
            }
        }
        items
    }

    fn report_locked(&self) {
        if self.locked.is_empty() {
            return;
        }
        let n = self.locked.len();
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
            output::info(&format!(
                "the machine identity changed ({change}), which gives {} paths another source:",
                pending.len()
            ));
            for key in &pending {
                output::info(&format!("  {key}"));
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
            "identity changed ({change}): {} paths left undecided; answer with --accept-identity:",
            pending.len()
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
            let mut undecided = Vec::new();
            for ((index, item), choice) in indices.iter().zip(&members).zip(choices) {
                let action = match choice {
                    Choice::Local => Some(self.keep(question.local_action, item)),
                    Choice::Repo => Some(Action::Write),
                    Choice::Skip => {
                        undecided.push(item.key.as_str());
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
                output::warning(&format!(
                    "{} left undecided ({}); answer with {}:",
                    question.what,
                    undecided.len(),
                    question.flags
                ));
                for key in undecided {
                    output::warning(&format!("  {key}"));
                }
            }
        }
        Ok(actions)
    }

    /// Keeping H means different things per pass. `sync` captures it, except for a path deleted
    /// here, which has no H to capture.
    fn keep(&self, local: Action, item: &Item) -> Action {
        let into_template = item.r.as_ref().is_some_and(|r| r.template);
        if local == Action::KeepLocal
            && self.opts.pass == Pass::Capture
            && item.row != Row::DeletedChangedUpstream
            && !into_template
        {
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
                    "{key}: template-drift; edit the template: {bin} edit {key}, or discard the local change: {bin} apply --take-repo {key}",
                    bin = name::bin()
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
                    "{key}: forgotten upstream but edited here; `{bin} add` keeps it, `{bin} forget` accepts the removal",
                    bin = name::bin()
                ));
                None
            }
            Row::Clean { update_state } => update_state.then_some(Action::Record),
            Row::NotApplied | Row::ChangedInRepo => Some(Action::Write),
            Row::ChangedLocally => {
                output::info(&format!(
                    "{key}: changed locally; `{} sync` captures it",
                    name::bin()
                ));
                None
            }
            Row::Blocked => {
                self.attention = true;
                if let Home::Blocked(obstacle) = &item.raw {
                    output::warning(&format!("{key}: {}", Skip::Blocked(obstacle.clone())));
                }
                None
            }
            Row::SymlinkAtTarget => {
                self.attention = true;
                output::warning(&format!("{key}: {}", Skip::Symlink));
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
        output::info(&format!("{} {}:", items.len(), question.what));
        for item in items {
            output::info(&format!("  {:<26}{}", token(item.row), item.key));
        }
        loop {
            match parse_reply(question, &prompt::ask(&menu(question, true))?, true) {
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
        loop {
            let asked = format!("{} {}", item.key, menu(question, false));
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
                let source = item.r.as_ref().map_or(key.as_str(), |r| r.source.as_str());
                let encrypted = item.r.as_ref().is_some_and(|r| r.encrypted);
                self.capture(key, source, h.kind, encrypted, state, repo_config);
            }
            Action::Record | Action::KeepLocal | Action::Write => {
                let Some(r) = &item.r else { return };
                let done = match action {
                    Action::Write => self.write(item, r),
                    Action::KeepLocal => {
                        self.announce("keep local", key);
                        true
                    }
                    _ => true,
                };
                if done {
                    state.files.insert(key.clone(), r.tuple.entry(&r.source));
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
                    let s = state.system.get(&key).map(Tuple::from_entry).transpose()?;
                    let local_changes = matches!(&h, Home::Present(h) if s.as_ref() != Some(h));
                    items.push(Item {
                        key,
                        row,
                        r,
                        raw: h,
                        local_changes,
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
            "{} left unwritten; answer with --yes:",
            system_files(ready.len())
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
                    "{key}: held, it holds an age secret key (AGE-SECRET-KEY-) and system scope does not encrypt"
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
            self.capture(key, key, *kind, false, state, repo_config);
        }
        for note in &scan.notes {
            output::warning(note);
        }
        if !scan.held.is_empty() {
            self.attention = true;
            output::warning(&format!(
                "{} new files held back; `{} add <file>` keeps one, an [ignore] pattern in .dotman.toml silences it:",
                scan.held.len(),
                name::bin()
            ));
            for (key, hold) in &scan.held {
                output::warning(&format!("  {key} ({})", hold.reason()));
            }
        }
    }

    fn capture(
        &mut self,
        key: &str,
        source: &str,
        kind: Kind,
        encrypted: bool,
        state: &mut State,
        repo_config: &mut RepoConfig,
    ) {
        if self.opts.dry_run {
            // Only a dry run reads H here; a real capture checks what it is about to store.
            if !encrypted && add::holds_key(&self.ctx.paths.home, key) {
                self.held_key(key);
                return;
            }
            self.announce("capture", key);
            self.captured.insert(key.to_owned());
            return;
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
                self.announce("capture", key);
                state
                    .files
                    .insert(key.to_owned(), entry.tuple.entry(&entry.source));
                self.captured.insert(key.to_owned());
            }
            Err(e) if e.is::<HoldsKey>() => self.held_key(key),
            Err(e) => {
                self.failed = self.failed.saturating_add(1);
                self.uncaptured = self.uncaptured.saturating_add(1);
                output::warning(&format!("{key}: cannot be captured: {e:#}"));
            }
        }
    }

    /// Never staged or committed; the pass after the pull leaves it alone too.
    fn held_key(&mut self, key: &str) {
        self.attention = true;
        self.undecided.insert(key.to_owned());
        output::warning(&format!(
            "{key}: held, it {HoldsKey}; or keep it out with an [ignore] pattern"
        ));
    }

    /// Whether R reached `$HOME`. A skipped or failed write is reported here.
    fn write(&mut self, item: &Item, r: &SourceEntry) -> bool {
        let key = &item.key;
        let opts = WriteOptions {
            dry_run: self.opts.dry_run,
            in_place: self.opts.in_place,
            backup: item.local_changes.then_some(self.backups.as_path()),
            expected: &item.raw,
            modes: &self.modes,
        };
        let home = &self.ctx.paths.home;
        let outcome = match &r.content {
            Content::Bytes(bytes) => safe_write(home, key, bytes, r.tuple.mode, &opts),
            Content::Link(target) => safe_symlink(home, key, target, &opts),
        };
        match outcome {
            Ok(WriteOutcome::Written(written)) => {
                self.written.insert(key.clone());
                self.announce("apply", key);
                if let Some(backup) = &written.backup {
                    self.backed_up(backup);
                }
                if let Some(warning) = written.warning() {
                    output::warning(&format!("{key}: {warning}"));
                }
                if written.in_place {
                    output::info(&format!("{key}: rewritten in place"));
                }
                true
            }
            Ok(WriteOutcome::Skipped(skip)) => {
                self.attention = true;
                output::warning(&format!("{key}: {skip}"));
                false
            }
            Err(e) => {
                self.failed = self.failed.saturating_add(1);
                output::warning(&format!("{key}: cannot be applied: {e:#}"));
                false
            }
        }
    }

    fn announce(&self, verb: &str, key: &str) {
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
    let r = match selection {
        Selection::None => None,
        Selection::Winner(entry) => Some(entry),
        Selection::Ambiguous(tied) => return Ok(Classified::Ambiguous(tied)),
        Selection::Invalid(problems) => return Ok(Classified::Invalid(problems)),
    };
    let home = &ctx.paths.home;
    let h = observe_portable(home, ctx.paths.home_real.as_deref(), key)?;
    let recorded = state.files.get(key);
    let s = recorded.map(Tuple::from_entry).transpose()?;
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
    let local_changes = match (&h, &s) {
        (Home::Present(h), Some(s)) => h != s,
        (Home::Present(_), None) => true,
        _ => false,
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
        local_changes,
        identity,
        system: false,
    }))
}

fn system_files(n: usize) -> String {
    format!("{n} system {}", if n == 1 { "file" } else { "files" })
}

/// Each target with the owner, group and mode it gets and has now, then the diff.
fn show_system(ctx: &Context, ready: &[Ready]) {
    let user = |uid: u32| sys::passwd_by_uid(uid).map_or_else(|| uid.to_string(), |pw| pw.name);
    let group = |gid: u32| sys::group_by_gid(gid).map_or_else(|| gid.to_string(), |gr| gr.name);
    output::info(&format!("{} to write as root:", system_files(ready.len())));
    for item in ready {
        let now = item.current.as_ref().map_or_else(
            || "new".to_owned(),
            |(_, mode, uid, gid)| format!("now {}:{} {mode:04o}", user(*uid), group(*gid)),
        );
        output::info(&format!(
            "  {}  {}:{} {:04o}  ({now})",
            item.key, item.owner, item.group, item.r.tuple.mode
        ));
    }
    let entries: Vec<(&str, &SourceEntry)> = ready
        .iter()
        .map(|item| (item.key.as_str(), &item.r))
        .collect();
    if let Err(e) = diff::show(ctx, &[], &entries) {
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

fn menu(question: &Question, bulk: bool) -> String {
    let (local, repo) = if bulk {
        (question.local.all, question.repo.all)
    } else {
        (question.local.one, question.repo.one)
    };
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
            menu(conflicts, true),
            "[k] keep all local  [t] take all from repo  [d] show diffs  [p] decide per file  [s] skip:"
        );
        assert_eq!(menu(deletions, false), "[f] forget  [r] restore  [s] skip:");
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
    }
}
