//! `dot sync`: capture local changes, commit them, pull with rebase, apply what arrived and push.
//!
//! Capture and apply are two passes of `apply`, split by the pull. A rebase conflict aborts the
//! pull and leaves everything as it was, with the recovery steps printed. Encrypted files are
//! re-encrypted after the apply, when `[age] recipients` changed, so a machine that pulls the
//! re-encryption finds nothing left to do.

use crate::cli::SyncArgs;
use crate::commands::Outcome;
use crate::commands::apply::{self, Applied, Options, Pass};
use crate::context::Context;
use crate::git::PullOutcome;
use crate::hooks::{self, Policy};
use crate::lock::Lock;
use crate::output;
use crate::remote;
use crate::secrets;
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeSet;
use std::path::PathBuf;

const REMOTE: &str = "origin";

// Paths named in the generated commit message; the rest are counted.
const LISTED: usize = 3;

pub(crate) const IDENTITY_FIX: &str = "git has no user.name and user.email to commit with; set them with:\n  dot git config --global user.name \"Your Name\"\n  dot git config --global user.email \"you@example.com\"";

/// Runs `dot sync`.
pub fn run(args: &SyncArgs) -> Result<Outcome> {
    if args.message.as_deref().is_some_and(|m| m.trim().is_empty()) {
        bail!("the commit message is empty");
    }
    let ctx = Context::resolve("sync")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;

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
    commit(&ctx, args.message.as_deref(), dry_run, &first.captured)?;

    let branch = match ctx.git.remote_url(REMOTE)? {
        Some(_) => Some(
            ctx.git
                .branch()?
                .ok_or_else(|| anyhow!("the repo's HEAD is detached; check out a branch first"))?,
        ),
        None => None,
    };
    let Some(branch) = branch else {
        output::info("no remote is set; add one with `dot init --remote <url>`");
        return without_remote(&ctx, args, first, policy);
    };
    if !dry_run {
        remote::note_origin(&ctx, true);
    }
    // A failed path never stops the sync; the failure is reported after the push.
    let (first_failed, uncaptured) = (first.failed, first.uncaptured);
    let mut attention = first.outcome == Outcome::Attention;
    let written = first.written;
    if dry_run {
        output::info(&format!(
            "would pull {REMOTE} {branch}, apply what arrived and push"
        ));
        secrets::reencrypt(&ctx, true)?;
        let hooked = hooks::run(&ctx, &written, policy);
        apply::check_failed(first_failed).and(hooked)?;
        return Ok(finish(attention));
    }

    // A remote without the branch is a first push: there is nothing to pull.
    if ctx.git.remote_has_branch(REMOTE, &branch)?
        && let PullOutcome::Conflict { paths } = ctx.git.pull_rebase(REMOTE, &branch)?
    {
        report_conflict(&ctx, &paths);
        apply::check_failed(first_failed)?;
        return Ok(Outcome::Conflict);
    }

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
    let reencrypted = reencrypt(&ctx, false);
    let hooked = hooks::run(&ctx, &second.written, policy);
    let failed = apply::check_failed(second.failed.saturating_add(uncaptured));

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
    let reencrypted = reencrypt(ctx, dry_run);
    let outcome = apply::finish(ctx, &second, policy);
    reencrypted?;
    let attention = first.outcome == Outcome::Attention || outcome? == Outcome::Attention;
    Ok(finish(attention))
}

/// Commits a re-encryption with the generated message, whatever `-m` said.
fn reencrypt(ctx: &Context, dry_run: bool) -> Result<()> {
    if secrets::reencrypt(ctx, dry_run)? > 0 {
        commit(ctx, None, false, &BTreeSet::new())?;
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

/// Stages everything and commits it. A dry run stages nothing, so it counts the keys that would
/// be captured plus what the worktree already holds.
fn commit(
    ctx: &Context,
    message: Option<&str>,
    dry_run: bool,
    captured: &BTreeSet<String>,
) -> Result<()> {
    let mut changed = BTreeSet::new();
    if dry_run {
        changed.extend(captured.iter().cloned());
    } else {
        ctx.git.stage_all()?;
    }
    changed.extend(
        ctx.git
            .status()?
            .iter()
            .map(|entry| entry.path.to_string_lossy().into_owned()),
    );
    if changed.is_empty() {
        return Ok(());
    }
    let message = message.map_or_else(
        || default_message(&ctx.identity().host, &changed),
        str::to_owned,
    );
    if dry_run {
        output::info(&format!("would commit: {message}"));
        return Ok(());
    }
    if !ctx.git.identity_configured()? {
        bail!(IDENTITY_FIX);
    }
    if ctx.git.commit(&message)?.is_some() {
        output::success(&format!("committed: {message}"));
    }
    Ok(())
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
    output::warning("  1. dot git pull --rebase");
    output::warning(&format!(
        "  2. Edit the conflicting files under {}.",
        ctx.paths.repo.display()
    ));
    output::warning(&format!("  3. dot git add {add}"));
    output::warning("  4. dot git rebase --continue");
    output::warning("  5. dot sync");
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
