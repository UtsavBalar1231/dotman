//! `dot apply`: write R into `$HOME`. Paths that need a decision are asked about once per
//! category, never per file unless the user picks that; a run that cannot ask leaves them alone.

use crate::cli::{ApplyArgs, Decisions};
use crate::commands::status::token;
use crate::commands::{Outcome, add, diff, forget};
use crate::context::Context;
use crate::engine::status::{Content, Row, Source, SourceEntry, Worktree, classify};
use crate::engine::write::{
    Skip, WriteOptions, WriteOutcome, backup_run_dir, safe_symlink, safe_write,
};
use crate::engine::{Home, Tuple, observe, observe_portable};
use crate::lock::Lock;
use crate::output;
use crate::prompt;
use crate::repo_config::RepoConfig;
use crate::state::{Kind, State};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Runs `dot apply`.
pub fn run(args: &ApplyArgs) -> Result<Outcome> {
    let ctx = Context::resolve("apply")?;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), args.dry_run)?;
    ctx.guard()?;
    let opts = Options {
        dry_run: args.dry_run,
        in_place: args.in_place,
        decisions: args.decisions,
        ..Options::default()
    };
    apply(&ctx, &args.paths, &opts).map(|applied| applied.outcome)
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

#[derive(Default)]
pub(crate) struct Options {
    pub dry_run: bool,
    pub in_place: bool,
    pub decisions: Decisions,
    /// Print how many paths are in each row before anything else.
    pub summary: bool,
    pub pass: Pass,
    /// Keys left alone, because an earlier pass already reported them undecided.
    pub skip: BTreeSet<String>,
}

pub(crate) struct Applied {
    pub outcome: Outcome,
    /// Keys that were asked about and left unanswered.
    pub undecided: BTreeSet<String>,
    /// Keys captured into the repo; in a dry run, the keys that would be.
    pub captured: BTreeSet<String>,
}

/// Applies the keys under `inputs` (every key when empty). The caller holds the lock.
pub(crate) fn apply(ctx: &Context, inputs: &[PathBuf], opts: &Options) -> Result<Applied> {
    let repo_id = ctx.repo_id()?;
    let mut repo_config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;
    let mut keys: BTreeSet<String> = ctx.managed()?.into_keys().collect();
    keys.extend(state.files.keys().cloned());
    let mut keys = ctx.select(inputs, keys)?;
    keys.retain(|key| !opts.skip.contains(key));

    let mut run = Run {
        ctx,
        opts,
        modes: repo_config.modes.clone(),
        backups: backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?,
        attention: false,
        failed: 0,
        forget: BTreeSet::new(),
        undecided: BTreeSet::new(),
        captured: BTreeSet::new(),
        dir_modes: BTreeMap::new(),
    };
    let items = run.classify(&state, &keys);
    if opts.summary {
        summarize(&items);
    }
    let actions = run.plan(&items)?;
    for (item, action) in items.iter().zip(actions) {
        if let Some(action) = action {
            run.execute(item, action, &mut state, &mut repo_config);
        }
    }

    let repo_saved = if opts.dry_run || (run.forget.is_empty() && run.captured.is_empty()) {
        Ok(())
    } else {
        let removed = if run.forget.is_empty() {
            Ok(())
        } else {
            forget::remove_keys(ctx, &mut repo_config, &mut state, &run.forget)
        };
        removed.and_then(|()| repo_config.save(&ctx.repo_config_file(), false))
    };
    state.save(&ctx.paths.state_file(), opts.dry_run)?;
    repo_saved?;
    if run.failed > 0 {
        bail!("{} paths could not be applied", run.failed);
    }
    Ok(Applied {
        outcome: if run.attention {
            Outcome::Attention
        } else {
            Outcome::Done
        },
        undecided: run.undecided,
        captured: run.captured,
    })
}

struct Item {
    key: String,
    row: Row,
    r: Option<SourceEntry>,
    /// H as the write path reads it again before renaming. It differs from the classified H only
    /// for an absolute link into `$HOME`, which classification compares in its relative form.
    raw: Home,
    /// H differs from S, so replacing it needs a backup first.
    local_changes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Write,
    /// Leave H as it is and set S to R, so the next `sync` captures H.
    KeepLocal,
    /// H already equals R; S catches up silently.
    Record,
    DropState,
    /// Remove the key from the repo, as `dot forget` does.
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
    forget: BTreeSet<String>,
    undecided: BTreeSet<String>,
    captured: BTreeSet<String>,
    dir_modes: BTreeMap<String, u32>,
}

impl Run<'_> {
    fn classify(&mut self, state: &State, keys: &BTreeSet<String>) -> Vec<Item> {
        let worktree = Worktree::new(&self.ctx.paths.repo, &self.modes);
        let mut items = Vec::new();
        for key in keys {
            match classify_key(self.ctx, &worktree, state, key) {
                Ok(Some(item)) => items.push(item),
                Ok(None) => {}
                Err(e) => {
                    output::warning(&format!("{key}: cannot be read: {e:#}"));
                    self.failed = self.failed.saturating_add(1);
                }
            }
        }
        items
    }

    fn plan(&mut self, items: &[Item]) -> Result<Vec<Option<Action>>> {
        let mut actions: Vec<Option<Action>> = items.iter().map(|item| self.direct(item)).collect();
        for question in &QUESTIONS {
            let (indices, members): (Vec<usize>, Vec<&Item>) = items
                .iter()
                .enumerate()
                .filter(|(_, item)| (question.rows)(item.row))
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
        if local == Action::KeepLocal
            && self.opts.pass == Pass::Capture
            && item.row != Row::DeletedChangedUpstream
        {
            Action::Capture
        } else {
            local
        }
    }

    /// The action for rows that need no question, with their reports.
    fn direct(&mut self, item: &Item) -> Option<Action> {
        if self.opts.pass == Pass::Capture {
            return (item.row == Row::ChangedLocally).then_some(Action::Capture);
        }
        let key = &item.key;
        match item.row {
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
                    "{key}: forgotten upstream but edited here; `dot add` keeps it, `dot forget` accepts the removal"
                ));
                None
            }
            Row::Clean { update_state } => update_state.then_some(Action::Record),
            Row::NotApplied | Row::ChangedInRepo => Some(Action::Write),
            Row::ChangedLocally => {
                output::info(&format!("{key}: changed locally; `dot sync` captures it"));
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
        let entries: Vec<(&str, &SourceEntry)> = items
            .iter()
            .filter_map(|item| item.r.as_ref().map(|r| (item.key.as_str(), r)))
            .collect();
        if let Err(e) = diff::show(self.ctx, &entries, true) {
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
        let key = &item.key;
        match action {
            Action::DropState => {
                state.files.remove(key);
            }
            Action::Forget => {
                self.announce("forget", key);
                self.forget.insert(key.clone());
            }
            Action::Capture => self.capture(item, state, repo_config),
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

    fn capture(&mut self, item: &Item, state: &mut State, repo_config: &mut RepoConfig) {
        let key = &item.key;
        self.announce("capture", key);
        if self.opts.dry_run {
            self.captured.insert(key.clone());
            return;
        }
        let Home::Present(h) = &item.raw else { return };
        let done = add::commit_one(
            self.ctx,
            repo_config,
            state,
            &mut self.dir_modes,
            key,
            h.kind,
        );
        match done {
            Ok(()) => {
                self.captured.insert(key.clone());
            }
            Err(e) => {
                self.failed = self.failed.saturating_add(1);
                output::warning(&format!("{key}: cannot be captured: {e:#}"));
            }
        }
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

fn classify_key(
    ctx: &Context,
    worktree: &Worktree<'_>,
    state: &State,
    key: &str,
) -> Result<Option<Item>> {
    let home = &ctx.paths.home;
    let r = worktree.get(key)?;
    let h = observe_portable(home, ctx.paths.home_real.as_deref(), key)?;
    let s = state.files.get(key).map(Tuple::from_entry).transpose()?;
    let Some(row) = classify(r.as_ref().map(|e| &e.tuple), &h, s.as_ref()) else {
        return Ok(None);
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
    Ok(Some(Item {
        key: key.to_owned(),
        row,
        r,
        raw,
        local_changes,
    }))
}

fn summarize(items: &[Item]) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for item in items {
        let count = counts.entry(token(item.row)).or_insert(0);
        *count = count.saturating_add(1);
    }
    let parts: Vec<String> = counts.iter().map(|(t, n)| format!("{n} {t}")).collect();
    output::info(&format!(
        "{} managed paths: {}",
        items.len(),
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
