//! `dotman sync`: capture local changes, commit them, pull with rebase, apply what arrived and push.
//!
//! Capture and apply are two passes of `apply`, split by the pull. A rebase conflict aborts the
//! pull and leaves everything as it was, with the recovery steps printed. Encrypted files are
//! re-encrypted after the apply, when `[age] recipients` changed, so a machine that pulls the
//! re-encryption finds nothing left to do.

use crate::cli::SyncArgs;
use crate::commands::Outcome;
use crate::commands::add;
use crate::commands::apply::{self, Applied, Options, Pass};
use crate::config::Allow;
use crate::context::{Context, TEMP_PREFIX};
use crate::engine::{Node, read_node};
use crate::git::{PullOutcome, StatusEntry, StatusKind};
use crate::hooks::{self, Policy};
use crate::lock::Lock;
use crate::name;
use crate::output;
use crate::prompt;
use crate::remote;
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
    if holds_back_the_pull(&ctx)? {
        return Ok(Outcome::Attention);
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
    let Committed { held, refusal } = commit(
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
        let outcome = without_remote(&ctx, args, first, policy, held);
        return refused(refusal).and(outcome);
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
        apply::check_failed(first_failed, uncaptured)
            .and(hooked)
            .and_then(|()| refused(refusal))?;
        return Ok(finish(attention));
    }

    // A remote without the branch is a first push: there is nothing to pull.
    let on_remote = ctx.git.remote_has_branch(REMOTE, &branch)?;
    if on_remote && !pull(&ctx, &branch)? {
        apply::check_failed(first_failed, uncaptured)?;
        return Ok(Outcome::Conflict);
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

    push(&ctx, &branch, on_remote)?;
    failed.and(reencrypted).and(hooked)?;
    Ok(finish(attention))
}

/// Pushes `branch` and says how many commits went out. `pulled` means the pull left the remote's
/// tip in `FETCH_HEAD`, which a remote-tracking ref matches only when the remote has a fetch
/// refspec.
fn push(ctx: &Context, branch: &str, pulled: bool) -> Result<()> {
    // A repo with no commit has nothing to push.
    if ctx.git.head()?.is_none() {
        return Ok(());
    }
    let pushed = ctx
        .git
        .count_commits(pulled.then_some("FETCH_HEAD"), "HEAD")?;
    ctx.git.push_set_upstream(REMOTE, branch)?;
    output::success(&match pushed {
        0 => format!("{REMOTE}/{branch} is up to date"),
        1 => format!("pushed 1 commit to {REMOTE}/{branch}"),
        n => format!("pushed {n} commits to {REMOTE}/{branch}"),
    });
    Ok(())
}

/// Pulls `branch`; false when the pull stopped, with the recovery steps printed, before anything
/// reached `$HOME`.
fn pull(ctx: &Context, branch: &str) -> Result<bool> {
    refuse_unrelated(ctx, branch)?;
    match ctx.git.pull_rebase(REMOTE, branch)? {
        PullOutcome::Ok => Ok(true),
        PullOutcome::Conflict { paths } => {
            report_conflict(ctx, &paths);
            Ok(false)
        }
        // The worktree may hold conflict markers, which the next pass would write into `$HOME`.
        PullOutcome::Unstashed { paths } => {
            report_unstashed(ctx, &paths);
            Ok(false)
        }
    }
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
        commit(ctx, None, false, args.allow_mass_delete, &BTreeSet::new()).map(drop)?;
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

/// What [`commit`] did.
struct Committed {
    /// Files that hold an age secret key were left out.
    held: bool,
    /// In a dry run, why the real run would refuse to commit; reported once the preview is done.
    refusal: Option<String>,
}

/// Fails with `refusal`, if there is one.
fn refused(refusal: Option<String>) -> Result<()> {
    refusal.map_or(Ok(()), |why| Err(anyhow!(why)))
}

/// Stages everything and commits it, except files that hold an age secret key. A dry run stages
/// nothing, so it counts the keys that would be captured plus what the worktree already holds.
fn commit(
    ctx: &Context,
    message: Option<&str>,
    dry_run: bool,
    allow_mass_delete: bool,
    captured: &BTreeSet<String>,
) -> Result<Committed> {
    let before = ctx.git.status()?;
    let refusal = if allow_mass_delete {
        None
    } else {
        refuse_mass_delete(ctx, &before, dry_run)?
    };
    let held = held_entries(ctx, &before);
    let held_paths: Vec<PathBuf> = held.iter().map(|(entry, _)| entry.path.clone()).collect();
    // A held rename keeps its old path out of the commit too, which would otherwise delete it as
    // a forget does. The rename is staged again afterwards, so the next sync holds both again.
    let renamed: Vec<PathBuf> = held
        .iter()
        .filter_map(|(entry, _)| Some([entry.path.clone(), entry.orig_path.clone()?]))
        .flatten()
        .collect();
    let left_out: Vec<PathBuf> = held_paths.iter().chain(&renamed).cloned().collect();
    let mut changed = BTreeSet::new();
    let mut staged_rename = Vec::new();
    if dry_run {
        changed.extend(captured.iter().cloned());
    } else {
        staged_rename = ctx.git.index_entries(&renamed)?;
        ctx.git.stage_all_except(&left_out)?;
        let staged: Vec<PathBuf> = held
            .iter()
            .filter(|(entry, _)| entry.kind != StatusKind::Untracked && !entry.xy.starts_with('.'))
            .map(|(entry, _)| entry.path.clone())
            .chain(renamed.iter().cloned())
            .collect();
        if !staged.is_empty() {
            ctx.git.unstage(&staged)?;
        }
    }
    let done = Committed {
        held: !held_paths.is_empty(),
        refusal,
    };
    let committed = commit_staged(ctx, message, dry_run, &held, &left_out, changed, done);
    if !dry_run && !renamed.is_empty() {
        ctx.git.restore_index(&renamed, &staged_rename)?;
    }
    committed
}

/// Reports the held files and commits what [`commit`] staged; `changed` names what a dry run
/// would capture.
fn commit_staged(
    ctx: &Context,
    message: Option<&str>,
    dry_run: bool,
    held: &[(&StatusEntry, Finding)],
    left_out: &[PathBuf],
    mut changed: BTreeSet<String>,
    done: Committed,
) -> Result<Committed> {
    report_held(ctx, held, dry_run);
    changed.extend(
        ctx.git
            .status()?
            .iter()
            .filter(|entry| !left_out.contains(&entry.path))
            .filter(|entry| !(entry.kind == StatusKind::Untracked && is_temp(&entry.path)))
            .map(|entry| entry.path.to_string_lossy().into_owned()),
    );
    if changed.is_empty() {
        return Ok(done);
    }
    let message = message.map_or_else(
        || default_message(&ctx.identity().host, &changed),
        str::to_owned,
    );
    if dry_run {
        output::info(&format!("would commit: {message}"));
        return Ok(done);
    }
    if !ctx.git.identity_configured()? {
        bail!(identity_fix());
    }
    if ctx.git.commit(&message)?.is_some() {
        output::success(&format!("committed: {message}"));
    }
    Ok(done)
}

/// The changed worktree files the content guard holds, minus what this machine's `[guard] allow`
/// covers. `dotman edit` and hand edits in the repo reach no capture, so this is their only check.
pub(crate) fn held_entries<'a>(
    ctx: &Context,
    status: &'a [StatusEntry],
) -> Vec<(&'a StatusEntry, Finding)> {
    let allow = &ctx.machine.guard.allow;
    status
        .iter()
        .filter(|entry| entry.kind != StatusKind::Unmerged && !is_temp(&entry.path))
        .filter_map(|entry| Some((entry, holds_secret_key(ctx, &entry.path, allow)?)))
        .collect()
}

/// How to resolve the hold on `entry` without losing the edit. A tracked file whose edit is not
/// in `$HOME` must never be pointed at `add`, which would replace the only copy with `$HOME`'s.
pub(crate) fn held_remedy(ctx: &Context, entry: &StatusEntry, found: Finding) -> String {
    let bin = name::bin();
    let source = entry.path.to_string_lossy();
    let quoted = shell_words::quote(&source);
    let copy = shell_words::quote(&ctx.paths.repo.join(&entry.path).to_string_lossy()).into_owned();
    if entry.kind == StatusKind::Untracked {
        return format!("not tracked; move {copy} out of the repo to drop it");
    }
    let key = Sources::group([source.as_ref()])
        .keys()
        .next()
        .map_or_else(|| source.clone().into_owned(), str::to_owned);
    let in_home = matches!(
        (read_node(&ctx.paths.home, &key), read_node(&ctx.paths.repo, &source)),
        (Ok(Node::File { bytes: home, .. }), Ok(Node::File { bytes: repo, .. })) if home == repo
    );
    if !in_home {
        return format!(
            "the edit is only in the repo copy, not in $HOME; `{bin} git diff -- {quoted}` shows it. To keep it, copy {copy} elsewhere and remove the secret from that copy, then drop the edit with `{bin} git checkout -- {quoted}`"
        );
    }
    let home = add::pasteable(&ctx.paths.home, &key);
    let plain = if found.overridable() {
        format!(", `{bin} add --allow-plain {home}` stores it as is")
    } else {
        String::new()
    };
    format!(
        "{home} has the same edit: `{bin} add --encrypt {home}` stores it encrypted{plain}; or remove the secret there and drop the repo edit with `{bin} git checkout -- {quoted}`"
    )
}

/// Each held file with its finding, then how to resolve it.
fn list_held(ctx: &Context, held: &[(&StatusEntry, Finding)]) {
    for (entry, found) in held {
        let path = entry.path.to_string_lossy();
        output::warning(&format!("  {}: {found}", output::escape_line(&path)));
        output::warning(&format!(
            "    {}",
            output::escape_line(&held_remedy(ctx, entry, *found))
        ));
    }
}

/// A held edit stays uncommitted in the worktree, so a pull would stash it and put it back over
/// what arrives, conflict markers and all. Reports the held files and returns true when there are
/// any and a remote is set.
fn holds_back_the_pull(ctx: &Context) -> Result<bool> {
    if ctx.git.remote_url(REMOTE)?.is_none() {
        return Ok(false);
    }
    let status = ctx.git.status()?;
    let held = held_entries(ctx, &status);
    if held.is_empty() {
        return Ok(false);
    }
    let one = held.len() == 1;
    let it = if one { "it" } else { "them" };
    output::warning(&format!(
        "{} {} in the repo {} an age secret key or a credential and {} not committed, so sync stopped before the pull, which would set {it} aside and put {it} back over what arrives. Nothing was written:",
        held.len(),
        if one { "file" } else { "files" },
        if one { "holds" } else { "hold" },
        if one { "is" } else { "are" },
    ));
    list_held(ctx, &held);
    Ok(true)
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
        "{} {} in the repo {} an age secret key or a credential, so {} {}committed:",
        held.len(),
        if held.len() == 1 { "file" } else { "files" },
        if held.len() == 1 { "holds" } else { "hold" },
        if held.len() == 1 { "it is" } else { "they are" },
        if dry_run { "would not be " } else { "not " },
    ));
    list_held(ctx, held);
}

/// Refuses a commit that deletes at least [`MASS_DELETE`] tracked files and at least half of
/// them, unless an interactive answer allows it. A dry run gets the refusal back instead, to
/// report after its preview.
fn refuse_mass_delete(
    ctx: &Context,
    status: &[StatusEntry],
    dry_run: bool,
) -> Result<Option<String>> {
    // Only files gone from the worktree: a lost index also lists every file as deleted.
    let deleted = status
        .iter()
        .filter(|entry| entry.kind == StatusKind::Ordinary && entry.xy.contains('D'))
        .filter(|entry| fs::symlink_metadata(ctx.paths.repo.join(&entry.path)).is_err())
        .count();
    if deleted < MASS_DELETE || ctx.git.head()?.is_none() {
        return Ok(None);
    }
    let tracked = ctx.git.ls_tree("HEAD")?.len();
    if deleted.saturating_mul(2) < tracked {
        return Ok(None);
    }
    let what = format!("the changes in the repo delete {deleted} of its {tracked} tracked files");
    if !dry_run && prompt::interactive() {
        output::warning(&format!(
            "{what}, as a broken checkout or a forget of many files does"
        ));
        if prompt::confirm("commit the deletions? [y/N]")? {
            return Ok(None);
        }
        bail!("nothing was committed or pushed");
    }
    let bin = name::bin();
    let fix = format!(
        "To bring the files back: `{bin} git checkout -- .`; if the deletions are meant: `{bin} sync --allow-mass-delete`"
    );
    if dry_run {
        return Ok(Some(format!(
            "sync would refuse: {what}, as a broken checkout or a forget of many files does, and would commit or push nothing. {fix}"
        )));
    }
    bail!(
        "{what}, as a broken checkout or a forget of many files does; nothing was committed or pushed. {fix}"
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
    // A shallow clone lacks the history below its cut, where the common commit may be.
    if ctx.git.is_shallow()? {
        ctx.git.unshallow(REMOTE, branch)?;
        if ctx.git.merge_base("HEAD", "FETCH_HEAD")?.is_some() {
            return Ok(());
        }
    }
    let bin = name::bin();
    let repo = ctx.paths.repo.display();
    let url = ctx.git.remote_url(REMOTE)?.map_or_else(
        || "<url>".to_owned(),
        |url| shell_words::quote(&url).into_owned(),
    );
    bail!(
        "{REMOTE}/{branch} shares no history with the repo at {repo}: the remote's history was replaced, or it is another repo. Nothing was pulled or pushed, and your files are as they were. To start over from the remote and keep what only this repo has:\n  mv {repo} {repo}.old\n  {bin} import {url}\n  {bin} import --overlay {repo}.old --include-missing    # --include-missing adds the files only this repo has\nTo merge the two histories anyway: `{bin} git pull --no-rebase --allow-unrelated-histories {REMOTE} {branch}`, then `{bin} sync`"
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

fn report_unstashed(ctx: &Context, paths: &[PathBuf]) {
    let bin = name::bin();
    output::warning(
        "the pull finished, but uncommitted edits in the repo did not go back on top of it cleanly. Nothing was applied to $HOME and nothing was pushed; git keeps the edits in its stash. To recover:",
    );
    let mut steps = Vec::new();
    if !paths.is_empty() {
        let names: Vec<String> = paths
            .iter()
            .map(|p| shell_words::quote(&p.to_string_lossy()).into_owned())
            .collect();
        steps.push(format!(
            "{bin} git checkout HEAD -- {}   (puts back the pulled version)",
            names.join(" ")
        ));
    }
    steps.push(format!(
        "{bin} git stash show -p   (shows the edits; make the ones you want again under {})",
        ctx.paths.repo.display()
    ));
    steps.push(format!("{bin} git stash drop"));
    steps.push(format!("{bin} sync"));
    for (n, text) in (1_u8..).zip(steps) {
        output::warning(&format!("  {n}. {text}"));
    }
    if !paths.is_empty() {
        output::warning("conflicting paths:");
        for path in paths {
            output::warning(&format!(
                "  {}",
                output::escape_line(&path.to_string_lossy())
            ));
        }
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
