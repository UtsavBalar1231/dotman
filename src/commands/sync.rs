//! `dotman sync`: capture local changes, commit them, pull with rebase, apply what arrived and push.
//!
//! Capture and apply are two passes of `apply`, split by the pull. A rebase conflict aborts the
//! pull and leaves everything as it was, with the recovery steps printed. Encrypted files are
//! re-encrypted after the apply, when `[age] recipients` changed, so a machine that pulls the
//! re-encryption finds nothing left to do.

use crate::cli::SyncArgs;
use crate::commands::Outcome;
use crate::commands::apply::{self, Applied, Options, Pass};
use crate::context::{Context, TEMP_PREFIX};
use crate::git::{PullOutcome, StatusEntry, StatusKind};
use crate::hooks::{self, Policy};
use crate::lock::Lock;
use crate::name;
use crate::output;
use crate::prompt;
use crate::remote;
use crate::repo_config::Allow;
use crate::secrets::{self, Finding};
use crate::variant::Sources;
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const REMOTE: &str = "origin";

// Paths named in the generated commit message; the rest are counted.
const LISTED: usize = 3;

// A commit that deletes this many tracked files, and at least half of them, looks like a broken
// checkout or a lost worktree rather than an edit.
const MASS_DELETE: usize = 10;

pub(crate) fn identity_fix() -> String {
    let bin = name::bin();
    format!(
        "git has no user.name and user.email to commit with; set them with:\n  {bin} git config --global user.name \"Your Name\"\n  {bin} git config --global user.email \"you@example.com\""
    )
}

/// Runs `dotman sync`.
///
/// # Errors
///
/// Fails when the message is empty, the lock is held, keys collide, HEAD is detached, a hook fails,
/// or git cannot commit, pull or push.
pub fn run(args: &SyncArgs) -> Result<Outcome> {
    if args.message.as_deref().is_some_and(|m| m.trim().is_empty()) {
        bail!("the commit message is empty");
    }
    let ctx = Context::resolve("sync")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
    // Before git is asked about the index, which needs a repo to exist.
    ctx.repo_id()?;
    ctx.refuse_git_locks()?;
    ctx.refuse_collisions()?;
    if !dry_run {
        ctx.sweep_temp_files()?;
    }

    let first = apply::apply(
        &ctx,
        &[],
        &Options {
            dry_run,
            decisions: args.decisions,
            pass: Pass::Capture,
            ..Options::default()
        },
    )?;
    let policy = Policy {
        approve: args.decisions.approve_hooks,
        no_hooks: args.no_hooks,
        dry_run,
        never_ask: false,
    };
    let held = commit(
        &ctx,
        args.message.as_deref(),
        dry_run,
        args.allow_mass_delete,
        &first.captured,
    )?;

    let branch = match ctx.git.remote_url(REMOTE)? {
        Some(_) => Some(
            ctx.git
                .branch()?
                .ok_or_else(|| anyhow!("the repo's HEAD is detached; check out a branch first"))?,
        ),
        None => None,
    };
    let Some(branch) = branch else {
        output::info(&format!(
            "no remote is set; add one with `{} init --remote <url>`",
            name::bin()
        ));
        return without_remote(&ctx, args, first, policy, held);
    };
    if !dry_run {
        remote::note_origin(&ctx, true);
    }
    // A failed path never stops the sync; the failure is reported after the push.
    let (first_failed, uncaptured) = (first.failed, first.uncaptured);
    let mut attention = first.outcome == Outcome::Attention || held;
    let written = first.written;
    if dry_run {
        output::info(&format!(
            "would pull {REMOTE} {branch}, apply what arrived and push"
        ));
        secrets::reencrypt(&ctx, true)?;
        let hooked = hooks::run(&ctx, &written, policy);
        apply::check_failed(first_failed, uncaptured).and(hooked)?;
        return Ok(finish(attention));
    }

    // A remote without the branch is a first push: there is nothing to pull.
    if ctx.git.remote_has_branch(REMOTE, &branch)? {
        refuse_unrelated(&ctx, &branch)?;
        if let PullOutcome::Conflict { paths } = ctx.git.pull_rebase(REMOTE, &branch)? {
            report_conflict(&ctx, &paths);
            apply::check_failed(first_failed, uncaptured)?;
            return Ok(Outcome::Conflict);
        }
    }
    // The pull can bring a pair that only a case-sensitive filesystem holds; the pulled commits
    // stay, and nothing is written for any key.
    ctx.refuse_collisions()?;

    let mut second = apply::apply(
        &ctx,
        &[],
        &Options {
            decisions: args.decisions,
            skip: first.undecided,
            system: args.system,
            yes: args.yes,
            ..Options::default()
        },
    )?;
    attention |= second.outcome == Outcome::Attention;
    second.written.extend(written);
    let reencrypted = reencrypt(&ctx, args, false);
    let hooked = hooks::run(&ctx, &second.written, policy);
    let failed = apply::check_failed(second.failed.saturating_add(uncaptured), uncaptured);

    // A repo with no commit has nothing to push.
    if ctx.git.head()?.is_some() {
        ctx.git.push_set_upstream(REMOTE, &branch)?;
        output::success(&format!("{REMOTE}/{branch} is up to date"));
    }
    failed.and(reencrypted).and(hooked)?;
    Ok(finish(attention))
}

fn without_remote(
    ctx: &Context,
    args: &SyncArgs,
    first: Applied,
    policy: Policy,
    held: bool,
) -> Result<Outcome> {
    let dry_run = args.dry_run;
    // A dry run captured nothing, so its captured keys are not reported a second time.
    let mut skip = first.undecided;
    if dry_run {
        skip.extend(first.captured);
    }
    let mut second = apply::apply(
        ctx,
        &[],
        &Options {
            dry_run,
            decisions: args.decisions,
            skip,
            system: args.system,
            yes: args.yes,
            ..Options::default()
        },
    )?;
    second.written.extend(first.written);
    second.failed = second.failed.saturating_add(first.uncaptured);
    second.uncaptured = second.uncaptured.saturating_add(first.uncaptured);
    let reencrypted = reencrypt(ctx, args, dry_run);
    let outcome = apply::finish(ctx, &second, policy);
    reencrypted?;
    let attention = held || first.outcome == Outcome::Attention || outcome? == Outcome::Attention;
    Ok(finish(attention))
}

/// Commits a re-encryption with the generated message, whatever `-m` said.
fn reencrypt(ctx: &Context, args: &SyncArgs, dry_run: bool) -> Result<()> {
    if secrets::reencrypt(ctx, dry_run)? > 0 {
        commit(ctx, None, false, args.allow_mass_delete, &BTreeSet::new())?;
    }
    Ok(())
}

const fn finish(attention: bool) -> Outcome {
    if attention {
        Outcome::Attention
    } else {
        Outcome::Done
    }
}

/// Stages everything and commits it, except files that hold an age secret key, and returns
/// whether any were held. A dry run stages nothing, so it counts the keys that would be captured
/// plus what the worktree already holds.
fn commit(
    ctx: &Context,
    message: Option<&str>,
    dry_run: bool,
    allow_mass_delete: bool,
    captured: &BTreeSet<String>,
) -> Result<bool> {
    let before = ctx.git.status()?;
    if !allow_mass_delete {
        refuse_mass_delete(ctx, &before, dry_run)?;
    }
    // `dotman edit` and hand edits in the repo reach no capture, so this is their only check. A
    // config that cannot be read allows nothing.
    let allow = ctx.repo_config().map(|c| c.guard.allow).unwrap_or_default();
    let held: Vec<(&StatusEntry, Finding)> = before
        .iter()
        .filter(|entry| entry.kind != StatusKind::Unmerged)
        .filter_map(|entry| Some((entry, holds_secret_key(ctx, &entry.path, &allow)?)))
        .collect();
    let held_paths: Vec<PathBuf> = held.iter().map(|(entry, _)| entry.path.clone()).collect();
    let mut changed = BTreeSet::new();
    if dry_run {
        changed.extend(captured.iter().cloned());
    } else {
        ctx.git.stage_all_except(&held_paths)?;
        let staged: Vec<PathBuf> = held
            .iter()
            .filter(|(entry, _)| entry.kind != StatusKind::Untracked && !entry.xy.starts_with('.'))
            .map(|(entry, _)| entry.path.clone())
            .collect();
        if !staged.is_empty() {
            ctx.git.unstage(&staged)?;
        }
    }
    report_held(ctx, &held, dry_run);
    changed.extend(
        ctx.git
            .status()?
            .iter()
            .filter(|entry| !held_paths.contains(&entry.path))
            .filter(|entry| !(entry.kind == StatusKind::Untracked && is_temp(&entry.path)))
            .map(|entry| entry.path.to_string_lossy().into_owned()),
    );
    let held = !held_paths.is_empty();
    if changed.is_empty() {
        return Ok(held);
    }
    let message = message.map_or_else(
        || default_message(&ctx.identity().host, &changed),
        str::to_owned,
    );
    if dry_run {
        output::info(&format!("would commit: {message}"));
        return Ok(held);
    }
    if !ctx.git.identity_configured()? {
        bail!(identity_fix());
    }
    if ctx.git.commit(&message)?.is_some() {
        output::success(&format!("committed: {message}"));
    }
    Ok(held)
}

/// What the content guard holds in the regular file at `path` in the repo, minus what `[guard]
/// allow` covers for its key. A FIFO swapped in cannot hang the open, and anything unreadable is
/// left to `git add` to report.
fn holds_secret_key(ctx: &Context, path: &Path, allow: &[Allow]) -> Option<Finding> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(ctx.paths.repo.join(path))
        .ok()?;
    let mut bytes = Vec::new();
    if !file.metadata().is_ok_and(|meta| meta.is_file()) || file.read_to_end(&mut bytes).is_err() {
        return None;
    }
    let source = path.to_string_lossy();
    let key = Sources::group([source.as_ref()])
        .keys()
        .next()
        .map(str::to_owned);
    secrets::held(&bytes, key.as_deref().unwrap_or(&source), allow)
}

fn is_temp(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_bytes().starts_with(TEMP_PREFIX.as_bytes()))
}

fn report_held(ctx: &Context, held: &[(&StatusEntry, Finding)], dry_run: bool) {
    if held.is_empty() {
        return;
    }
    output::warning(&format!(
        "{} {} in the repo {} an age secret key or a credential, so {} {}committed; remove it from {}, keep such a file encrypted with `{bin} add --encrypt`, or store a credential as it is with `{bin} add --allow-plain`:",
        held.len(),
        if held.len() == 1 { "file" } else { "files" },
        if held.len() == 1 { "holds" } else { "hold" },
        if held.len() == 1 { "it is" } else { "they are" },
        if dry_run { "would not be " } else { "not " },
        ctx.paths.repo.display(),
        bin = name::bin()
    ));
    for (entry, found) in held {
        output::warning(&format!("  {}: {found}", entry.path.display()));
    }
}

/// Refuses a commit that deletes at least [`MASS_DELETE`] tracked files and at least half of
/// them, unless an interactive answer allows it.
fn refuse_mass_delete(ctx: &Context, status: &[StatusEntry], dry_run: bool) -> Result<()> {
    // Only files gone from the worktree: a lost index also lists every file as deleted.
    let deleted = status
        .iter()
        .filter(|entry| entry.kind == StatusKind::Ordinary && entry.xy.contains('D'))
        .filter(|entry| fs::symlink_metadata(ctx.paths.repo.join(&entry.path)).is_err())
        .count();
    if deleted < MASS_DELETE || ctx.git.head()?.is_none() {
        return Ok(());
    }
    let tracked = ctx.git.ls_tree("HEAD")?.len();
    if deleted.saturating_mul(2) < tracked {
        return Ok(());
    }
    let what = format!("the changes in the repo delete {deleted} of its {tracked} tracked files");
    if !dry_run && prompt::interactive() {
        output::warning(&format!(
            "{what}, as a broken checkout or a forget of many files does"
        ));
        if prompt::confirm("commit the deletions? [y/N]")? {
            return Ok(());
        }
        bail!("nothing was committed or pushed");
    }
    let bin = name::bin();
    bail!(
        "{what}, as a broken checkout or a forget of many files does; nothing was committed or pushed. To bring the files back: `{bin} git checkout -- .`; if the deletions are meant: `{bin} sync --allow-mass-delete`"
    )
}

/// Refuses to pull a `branch` that shares no commit with `HEAD`: rebasing onto a replaced or
/// unrelated history would silently drop whatever only one side has.
fn refuse_unrelated(ctx: &Context, branch: &str) -> Result<()> {
    if ctx.git.head()?.is_none() {
        return Ok(());
    }
    ctx.git.fetch(REMOTE, branch)?;
    if ctx.git.merge_base("HEAD", "FETCH_HEAD")?.is_some() {
        return Ok(());
    }
    let bin = name::bin();
    let repo = ctx.paths.repo.display();
    let url = ctx.git.remote_url(REMOTE)?.map_or_else(
        || "<url>".to_owned(),
        |url| shell_words::quote(&url).into_owned(),
    );
    bail!(
        "{REMOTE}/{branch} shares no history with the repo at {repo}: the remote's history was replaced, or it is another repo. Nothing was pulled or pushed, and your files are as they were. To start over from the remote and keep what only this repo has:\n  mv {repo} {repo}.old\n  {bin} import {url}\n  {bin} import --overlay {repo}.old\nTo merge the two histories anyway: `{bin} git pull --no-rebase --allow-unrelated-histories {REMOTE} {branch}`, then `{bin} sync`"
    )
}

/// `sync(<host>): a/b, c/d, e/f (+N more)`, naming the first three paths in sorted order.
fn default_message(host: &str, changed: &BTreeSet<String>) -> String {
    let host = if host.is_empty() { "unknown" } else { host };
    let named: Vec<&str> = changed.iter().take(LISTED).map(|p| tail(p)).collect();
    let more = changed.len().saturating_sub(LISTED);
    let rest = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    format!("sync({host}): {}{rest}", named.join(", "))
}

/// The last two components of `path`.
fn tail(path: &str) -> &str {
    path.rmatch_indices('/')
        .nth(1)
        .and_then(|(at, _)| path.get(at.saturating_add(1)..))
        .unwrap_or(path)
}

fn report_conflict(ctx: &Context, paths: &[PathBuf]) {
    let names: Vec<String> = paths
        .iter()
        .map(|p| shell_words::quote(&p.to_string_lossy()).into_owned())
        .collect();
    let add = if names.is_empty() {
        "<paths>".to_owned()
    } else {
        names.join(" ")
    };
    output::warning(
        "the pull conflicts with your changes. It was aborted, and your files and commits are as they were. To resolve it:",
    );
    let bin = name::bin();
    output::warning(&format!("  1. {bin} git pull --rebase"));
    output::warning(&format!(
        "  2. Edit the conflicting files under {}.",
        ctx.paths.repo.display()
    ));
    output::warning(&format!("  3. {bin} git add {add}"));
    output::warning(&format!("  4. {bin} git rebase --continue"));
    output::warning(&format!("  5. {bin} sync"));
    output::warning("conflicting paths:");
    for path in paths {
        output::warning(&format!("  {}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_shorten_to_their_last_two_components() {
        assert_eq!(tail(".bashrc"), ".bashrc");
        assert_eq!(tail(".config/nvim"), ".config/nvim");
        assert_eq!(tail(".config/nvim/init.lua"), "nvim/init.lua");
        assert_eq!(tail("a/b/c/d"), "c/d");
    }

    #[test]
    fn the_message_names_three_paths_and_counts_the_rest() {
        let set = |keys: &[&str]| keys.iter().map(|k| (*k).to_owned()).collect();
        let three = default_message("box", &set(&["x/a", "x/b", "x/c"]));
        assert_eq!(three, "sync(box): x/a, x/b, x/c");
        let five = default_message("box", &set(&["x/e", "x/a", "x/d", "x/b", "x/c"]));
        assert_eq!(five, "sync(box): x/a, x/b, x/c (+2 more)");
        assert!(default_message("", &set(&["a"])).starts_with("sync(unknown): "));
    }
}
