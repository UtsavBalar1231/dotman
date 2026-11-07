//! `dot diff`: how `$HOME` differs from the repo, shown by `git diff --no-index`.
//!
//! The comparison runs on copies in a private temp tree, so the user's pager, colours and external
//! diff tool never see a path inside `$HOME` or the repo.

use crate::cli::DiffArgs;
use crate::context::Context;
use crate::engine::status::{Content, Source, SourceEntry, Worktree};
use crate::engine::{Home, Node, observe_portable, read_node};
use crate::git::Git;
use crate::output;
use crate::paths::private_temp_dir;
use anyhow::{Context as _, Result, bail};
use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};

/// Runs `dot diff`. Takes no lock and writes nothing outside its temp tree; differences are not an error.
pub fn run(args: &DiffArgs, no_pager: bool) -> Result<()> {
    let ctx = Context::resolve("diff")?;
    ctx.repo_id()?;
    let repo_config = ctx.repo_config()?;
    let keys = ctx.select(&args.paths, ctx.managed()?.into_keys().collect())?;
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);
    let mut entries = Vec::new();
    for key in keys {
        if let Some(r) = worktree.get(&key)? {
            entries.push((key, r));
        }
    }
    let refs: Vec<(&str, &SourceEntry)> = entries.iter().map(|(k, r)| (k.as_str(), r)).collect();
    let no_pager = no_pager || std::env::var_os("NO_PAGER").is_some_and(|v| !v.is_empty());
    show(&ctx, &refs, no_pager)
}

/// One `git diff` from `a/<key>` (H) to `b/<key>` (R) over the keys whose H differs from R.
pub(crate) fn show(ctx: &Context, entries: &[(&str, &SourceEntry)], no_pager: bool) -> Result<()> {
    ctx.git.check_version()?;
    let tmp = private_temp_dir("dotman-diff-", &runtime_dir())?;
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
        match h {
            Home::Present(h) if h == r.tuple => continue,
            Home::Blocked(_) => {
                output::warning(&format!("{key}: blocked, not compared"));
                continue;
            }
            Home::Present(_) => match read_node(home, key)? {
                Node::File { bytes, mode, .. } => {
                    place(&home_side.join(key), &Content::Bytes(bytes), mode)?;
                }
                Node::Symlink(target) => place(&home_side.join(key), &Content::Link(target), 0)?,
                _ => continue,
            },
            Home::Missing => {}
        }
        place(&repo_side.join(key), &r.content, r.tuple.mode)?;
        differs = true;
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
    let status = cmd.status().context("failed to run git diff")?;
    match status.code() {
        Some(0 | 1) => Ok(()),
        None if status.signal() == Some(libc::SIGPIPE) => Ok(()),
        _ => bail!("git diff --no-index failed ({status})"),
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
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
