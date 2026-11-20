use super::{Delta, Node, Stored, apply, output, read_node};
use crate::cli::SyncArgs;
use crate::commands::Outcome;
use crate::commands::partial::TextChanges;
use crate::context::Context;
use crate::engine::status::{Row, Variants, Worktree};
use crate::engine::write::{WriteOptions, WriteOutcome, backup_run_dir, safe_write};
use crate::engine::{Home, Tuple, observe};
use crate::state::Kind;
use anyhow::{Result, anyhow, bail, ensure};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(super) fn run(ctx: &mut Context, args: &SyncArgs) -> Result<Outcome> {
    ensure!(args.no_hooks, "selected capture requires --no-hooks");
    let _lock = ctx.start(args.json, args.dry_run)?;
    ctx.refuse_git_locks()?;
    ctx.refuse_collisions()?;
    let target = ctx
        .git
        .selected_target()?
        .ok_or_else(|| anyhow!("selected push target is unavailable or unsupported"))?;
    let sync = ctx
        .reviewed
        .as_ref()
        .and_then(|review| review.input.sync.as_ref())
        .ok_or_else(|| anyhow!("missing sync review"))?;
    ensure!(
        sync.selected_target.as_deref() == Some(target.binding.as_str())
            || (target.binding == "local" && sync.selected_target.is_none()),
        "selected push target changed or was not reviewed"
    );
    let prepared = prepare(ctx, args)?;
    execute(ctx, args, prepared, &target)
}

struct Prepared {
    key: String,
    head: String,
    path: PathBuf,
    projected: Vec<u8>,
    replacement: Stored,
    expected: Home,
    repo_mode: u32,
    home_mode: u32,
}

fn prepare(ctx: &Context, args: &SyncArgs) -> Result<Prepared> {
    let review = ctx
        .reviewed
        .as_ref()
        .ok_or_else(|| anyhow!("missing review"))?;
    check_repository(ctx, review)?;
    let sync = review
        .input
        .sync
        .as_ref()
        .ok_or_else(|| anyhow!("missing sync review"))?;
    let selected = sync
        .selected_lines
        .as_ref()
        .ok_or_else(|| anyhow!("missing selected lines"))?;
    ensure!(
        selected.len() == 1,
        "selected capture currently supports one key only"
    );
    let head = sync
        .head
        .as_deref()
        .ok_or_else(|| anyhow!("selected capture requires HEAD"))?;
    let state = ctx.state(&ctx.repo_id()?)?;
    check_identity(ctx, &state)?;
    let config = ctx.repo_config()?;
    let sources = ctx.sources()?;
    let variants = Variants::new(
        Worktree::new(&ctx.paths.repo, &config.modes),
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    let (key, ids) = selected
        .iter()
        .next()
        .ok_or_else(|| anyhow!("missing selection"))?;
    let capture = review
        .capture(key)
        .ok_or_else(|| anyhow!("missing capture"))?;
    ensure!(
        capture.source == *key,
        "selected capture requires a direct source"
    );
    ensure!(
        sources.select(key, ctx.identity()).require(key)? == key
            && sources.flag(key, key) == crate::variant::Flag::Plain,
        "selected capture requires a plain direct source"
    );
    ctx.stage("capture", args.dry_run)?;
    let apply::Classified::Item(item) = apply::classify_key(ctx, &variants, &state, key)? else {
        bail!("selected capture is not eligible");
    };
    ensure!(
        item.row() == Row::ChangedLocally && !item.identity_changed(),
        "selected capture is not a local-only change"
    );
    let (source, logical, encrypted, template) = item
        .source_metadata()
        .ok_or_else(|| anyhow!("missing source"))?;
    ensure!(
        source == key && !encrypted && !template,
        "selected capture requires a plain direct source"
    );
    let entry = state
        .files
        .get(key)
        .ok_or_else(|| anyhow!("missing recorded state"))?;
    ensure!(
        entry.source == *key && entry.kind == Kind::File,
        "selected capture has ambiguous state"
    );
    let stored = Stored::read(&ctx.paths.repo, Path::new(source))?;
    let Node::File {
        bytes: home,
        mode: home_mode,
        ..
    } = read_node(&ctx.paths.home, key)?
    else {
        bail!("selected capture requires a regular home file");
    };
    ensure!(
        matches!(&stored, Stored::File { .. }),
        "selected capture requires a regular repository file"
    );
    ensure!(
        capture.logical_r.matches(Some(logical))
            && capture
                .raw_h
                .matches(Some(&Tuple::of_file(&home, home_mode)))
            && capture.stored_before.stored(&stored),
        "selected capture changed since review"
    );
    ensure!(
        Tuple::from_entry(entry)? == *logical && home_mode == Tuple::from_entry(entry)?.mode,
        "selected capture mode or state changed"
    );
    project(ctx, key, ids, head, &stored, &home, home_mode)
}

fn check_identity(ctx: &Context, state: &crate::state::State) -> Result<()> {
    ensure!(
        state.identity.as_ref() == Some(ctx.identity()),
        "identity changed"
    );
    Ok(())
}

fn check_repository(ctx: &Context, review: &crate::decisions::Reviewed) -> Result<()> {
    review.recheck(ctx)?;
    ensure!(
        ctx.git.status()?.is_empty(),
        "selected capture requires a clean repository and index"
    );
    Ok(())
}

fn project(
    ctx: &Context,
    key: &str,
    ids: &[u32],
    head: &str,
    stored: &Stored,
    home: &[u8],
    home_mode: u32,
) -> Result<Prepared> {
    let Stored::File {
        bytes: repo,
        mode: repo_mode,
    } = stored
    else {
        bail!("selected capture requires a regular repository file");
    };
    let index = ctx.git.staging_review(&[], &[])?;
    let path = PathBuf::from(key);
    ensure!(
        index.head.contains_key(&path) && index.head.get(&path) == index.index.get(&path),
        "selected source is not unchanged in the index"
    );
    let head_stored = Stored::from_entry(
        &ctx.git,
        index
            .head
            .get(&path)
            .ok_or_else(|| anyhow!("missing HEAD source"))?,
    )?;
    ensure!(
        stored.git_tuple() == head_stored.git_tuple(),
        "selected source changed in the repository"
    );
    ensure!(
        super::holds_stored(&path, stored, &[]).is_none(),
        "repository source is held"
    );
    ensure!(
        super::holds_stored(
            &path,
            &Stored::File {
                bytes: home.to_owned(),
                mode: home_mode
            },
            &[]
        )
        .is_none(),
        "home source is held"
    );
    let projected = TextChanges::between(repo, home)
        .and_then(|changes| changes.project(ids))
        .ok_or_else(|| anyhow!("invalid or unsupported selected edits"))?;
    let replacement = Stored::File {
        bytes: projected.clone(),
        mode: *repo_mode,
    };
    ensure!(
        super::holds_stored(&path, &replacement, &[]).is_none(),
        "selected result is held"
    );
    ensure!(projected != *repo, "selection makes no change");
    let expected = observe(&ctx.paths.repo, key)?;
    ensure!(
        matches!(&expected, Home::Present(tuple) if tuple == &Tuple::of_file(repo, *repo_mode)),
        "repository source changed"
    );
    Ok(Prepared {
        key: key.to_owned(),
        head: head.to_owned(),
        path,
        projected,
        replacement,
        expected,
        repo_mode: *repo_mode,
        home_mode,
    })
}

fn execute(
    ctx: &Context,
    args: &SyncArgs,
    prepared: Prepared,
    target: &crate::git::SelectedTarget,
) -> Result<Outcome> {
    let Prepared {
        key,
        head,
        path,
        projected,
        replacement,
        expected,
        repo_mode,
        home_mode,
    } = prepared;
    let review = ctx
        .reviewed
        .as_ref()
        .ok_or_else(|| anyhow!("missing review"))?;
    let capture = review
        .capture(&key)
        .ok_or_else(|| anyhow!("missing capture"))?;
    let config = ctx.repo_config()?;
    review.check_head(ctx)?;
    ensure!(ctx.git.identity_configured()?, "{}", super::identity_fix());
    ctx.stage("commit", args.dry_run)?;
    if args.dry_run {
        output::info("would commit selected edits");
        return Ok(Outcome::Done);
    }
    review.recheck(ctx)?;
    ensure!(
        capture.raw_h.home(&observe(&ctx.paths.home, &key)?),
        "home changed since review"
    );
    ensure!(
        ctx.git.status()?.is_empty(),
        "repository changed since review"
    );
    ctx.git.check_prepared_transaction()?;
    let backups = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
    let options = WriteOptions {
        dry_run: false,
        in_place: false,
        backup: Some(&backups),
        expected: &expected,
        modes: &config.modes,
        own: ctx.keys(),
    };
    match safe_write(&ctx.paths.repo, &key, &projected, repo_mode, &options)? {
        WriteOutcome::Written(_) => {}
        WriteOutcome::Skipped(skip) => bail!("{key}: {skip}"),
    }
    let mut delta = Delta::new();
    delta.insert(path, replacement);
    let message = args.message.as_deref().unwrap_or("sync: selected edits");
    let commit = ctx.git.commit_reviewed(Some(&head), review.branch(), &delta, message)
        .map_err(|error| anyhow!("selected repo file was written but commit failed; its previous contents are backed up at {}: {error:#}", backups.display()))?;
    let mut state = ctx.state(&ctx.repo_id()?)?;
    let state_entry = state
        .files
        .get_mut(&key)
        .ok_or_else(|| anyhow!("missing recorded state"))?;
    state_entry.hash = Tuple::of_file(&projected, home_mode).hash;
    state
        .save(&ctx.paths.state_file(), false)
        .map_err(|error| {
            anyhow!("selected commit {commit} was created, but state could not be saved: {error:#}")
        })?;
    output::success(&format!("committed selected edits: {commit}"));
    if target.binding != "local" {
        ctx.stage("push", false)?;
        let current = ctx
            .git
            .selected_target()?
            .ok_or_else(|| anyhow!("selected push target is unavailable; local commit retained"))?;
        ensure!(
            current.binding == target.binding,
            "selected push target changed; local commit retained"
        );
        ctx.git.push_selected(&current, &commit)?;
        output::success("pushed selected commit");
    }
    Ok(Outcome::Done)
}
