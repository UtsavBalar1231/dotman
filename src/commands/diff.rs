//! `dotman diff`: how `$HOME` differs from the repo, shown by `git diff --no-index`.
//!
//! The comparison runs on copies in a private temp tree, so the user's pager, colours and external
//! diff tool never see a path inside `$HOME` or the repo.

use crate::cli::DiffArgs;
use crate::context::Context;
use crate::engine::status::{Content, Source, SourceEntry, Variants, Worktree};
use crate::engine::{Home, Node, observe_portable, read_node};
use crate::git::Git;
use crate::output;
use crate::paths::{create_private_dir, private_temp_dir};
use crate::secrets::Locked;
use crate::sys;
use crate::system::{self, Verdict};
use crate::variant::Selection;
use anyhow::{Context as _, Result, bail};
use libc::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use std::collections::BTreeSet;
use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};

/// Runs `dotman diff`. Takes no lock and writes nothing outside its temp tree; differences are not an error.
///
/// # Errors
///
/// Fails when the repo is not initialised or has a rebase or merge in progress, an input cannot be
/// resolved to a key, or a file cannot be rendered for comparison.
pub fn run(args: &DiffArgs, no_pager: bool) -> Result<()> {
    let ctx = Context::resolve("diff")?;
    ctx.guard()?;
    ctx.repo_id()?;
    let repo_config = ctx.repo_config()?;
    let (home_paths, system_filters) = system::split_inputs(&ctx.paths, &args.paths)?;
    let sources = ctx.sources()?;
    let keys = if args.paths.is_empty() || !home_paths.is_empty() {
        ctx.select(&home_paths, sources.keys().map(str::to_owned).collect())?
    } else {
        BTreeSet::new()
    };
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);
    let variants = Variants::new(
        worktree,
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    let mut entries = Vec::new();
    for key in keys {
        match variants.get(&key) {
            Ok(Selection::Winner(r)) => entries.push((key, r)),
            Ok(Selection::None) => {}
            Err(e) if e.is::<Locked>() => output::warning(&format!("{key}: locked, not compared")),
            Err(e) => output::warning(&format!("{key}: not compared: {e:#}")),
            Ok(Selection::Ambiguous(_)) => {
                output::warning(&format!("{key}: ambiguous, not compared"));
            }
            Ok(Selection::Invalid(_)) => {
                output::warning(&format!("{key}: has an invalid variant, not compared"));
            }
        }
    }
    let state = ctx.state(&ctx.repo_id()?)?;
    let mut system_entries = Vec::new();
    if args.paths.is_empty() || !system_filters.is_empty() {
        let found = system::evaluate(&ctx, &repo_config, &state)?;
        let all = found.iter().map(|f| f.key.clone()).collect();
        let wanted = system::select_keys(&system_filters, &all)?;
        for item in found.into_iter().filter(|f| wanted.contains(&f.key)) {
            match item.verdict {
                Verdict::Row { r: Some(r), .. } => system_entries.push((item.key, r)),
                Verdict::Row { r: None, .. } => {}
                Verdict::Unreadable(why) => {
                    output::warning(&format!("{}: not compared: {why}", item.key));
                }
                Verdict::Refused(_) | Verdict::Invalid(_) | Verdict::Ambiguous(_) => {
                    output::warning(&format!("{}: not valid here, not compared", item.key));
                }
            }
        }
    }
    let refs: Vec<(&str, &SourceEntry)> = entries.iter().map(|(k, r)| (k.as_str(), r)).collect();
    let system_refs: Vec<(&str, &SourceEntry)> = system_entries
        .iter()
        .map(|(k, r)| (k.as_str(), r))
        .collect();
    let no_pager = no_pager || std::env::var_os("NO_PAGER").is_some_and(|v| !v.is_empty());
    show_all(&ctx, &refs, &system_refs, no_pager)
}

/// One `git diff`, without a pager, from `a/<key>` (H) to `b/<key>` (R) over the keys whose H
/// differs from R; system keys are read through the system scope.
pub(crate) fn show(
    ctx: &Context,
    entries: &[(&str, &SourceEntry)],
    system_entries: &[(&str, &SourceEntry)],
) -> Result<()> {
    show_all(ctx, entries, system_entries, true)
}

// System keys go under `system/` in both trees, so one can never land on a home key.
fn show_all(
    ctx: &Context,
    entries: &[(&str, &SourceEntry)],
    system_entries: &[(&str, &SourceEntry)],
    no_pager: bool,
) -> Result<()> {
    ctx.git.check_version()?;
    // Decrypted content goes here, so it is removed on every return, a failed git diff included.
    let tmp = private_temp_dir("dotman-diff-", &private_base(ctx)?)?;
    let (home_side, repo_side) = (tmp.path().join("a"), tmp.path().join("b"));
    let (home, home_real) = (&ctx.paths.home, ctx.paths.home_real.as_deref());
    let mut differs = false;
    for (key, r) in entries {
        let h = match observe_portable(home, home_real, key) {
            Ok(h) => h,
            Err(e) => {
                output::warning(&format!("{key}: not compared: {e:#}"));
                continue;
            }
        };
        differs |= stage(
            &[&home_side, &repo_side],
            Path::new(key),
            (key, r),
            h,
            || read_node(home, key),
        )?;
    }
    let scope = ctx.system_scope();
    for (key, r) in system_entries {
        let h = match scope.observe(key) {
            Ok(h) => h,
            Err(e) => {
                output::warning(&format!("{key}: not compared: {e:#}"));
                continue;
            }
        };
        let rel = Path::new("system").join(key.trim_start_matches('/'));
        differs |= stage(&[&home_side, &repo_side], &rel, (key, r), h, || {
            scope.node(key)
        })?;
    }
    if !differs {
        return Ok(());
    }
    for side in [&home_side, &repo_side] {
        fs::create_dir_all(side).with_context(|| format!("cannot create {}", side.display()))?;
    }

    let mut cmd = Git::new(tmp.path(), "diff").command();
    if no_pager {
        cmd.arg("--no-pager");
    }
    // `--no-prefix` with sides named `a` and `b` gives the usual `a/<key>` and `b/<key>` headers.
    cmd.args([
        "diff",
        "--no-index",
        "--color=auto",
        "--no-prefix",
        "--",
        "a",
        "b",
    ]);
    let mut child = cmd.spawn().context("failed to run git diff")?;
    // Ctrl-C reaches the whole foreground group. Ignored here only after the spawn, so git and the
    // pager keep their default actions, and dotman outlives them to remove the temp dir; only SIGKILL
    // can still leave it behind.
    let status = {
        let _ignored = sys::ignore_signals(&[SIGINT, SIGQUIT, SIGHUP, SIGTERM]);
        child.wait()
    }
    .context("failed to run git diff")?;
    match status.code() {
        Some(0 | 1) => Ok(()),
        None if status.signal() == Some(libc::SIGPIPE) => Ok(()),
        _ => bail!("git diff --no-index failed ({status})"),
    }
}

// Places both sides of one key under `sides` at `rel`; whether they differ.
fn stage(
    sides: &[&PathBuf; 2],
    rel: &Path,
    (key, r): (&str, &SourceEntry),
    h: Home,
    read: impl FnOnce() -> Result<Node>,
) -> Result<bool> {
    let [home_side, repo_side] = sides;
    match h {
        Home::Present(h) if h == r.tuple => return Ok(false),
        Home::Blocked(_) => {
            output::warning(&format!("{key}: blocked, not compared"));
            return Ok(false);
        }
        Home::Present(_) => match read()? {
            Node::File { bytes, mode, .. } => {
                place(&home_side.join(rel), &Content::Bytes(bytes), mode)?;
            }
            Node::Symlink(target) => place(&home_side.join(rel), &Content::Link(target), 0)?,
            _ => return Ok(false),
        },
        Home::Missing => {}
    }
    place(&repo_side.join(rel), &r.content, r.tuple.mode)?;
    Ok(true)
}

// `$XDG_RUNTIME_DIR` is private to the user and usually memory-backed; the state dir is private too.
fn private_base(ctx: &Context) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        return Ok(dir);
    }
    let state = &ctx.paths.state_dir;
    create_private_dir(state, false)
        .with_context(|| format!("cannot create {}", state.display()))?;
    Ok(state.clone())
}

fn place(path: &Path, content: &Content, mode: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    match content {
        Content::Bytes(bytes) => {
            fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
            // git shows only the execute bit; the 0700 temp dir keeps the content private.
            let shown = if mode & 0o111 == 0 { 0o600 } else { 0o700 };
            fs::set_permissions(path, Permissions::from_mode(shown))?;
        }
        Content::Link(target) => {
            symlink(target, path).with_context(|| format!("cannot create {}", path.display()))?;
        }
    }
    Ok(())
}
