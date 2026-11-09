//! `dotman git`: git in the repo with dotman's isolation flags. Unguarded and unlocked, because it is
//! how a rebase gets resolved.

use crate::cli::GitArgs;
use crate::context::Context;
use crate::name;
use anyhow::{Context as _, Result, bail};
use std::fs;
use std::os::unix::process::ExitStatusExt;

/// Runs git with `args` and returns its exit code, 128 plus the signal number if it was killed.
///
/// # Errors
///
/// Fails when `$HOME` or the machine config cannot be resolved, the repo does not exist yet, or
/// git cannot be started. A git failure comes back as its exit code.
pub fn run(args: &GitArgs) -> Result<i32> {
    let ctx = Context::resolve("git")?;
    // Only the repo has to exist: an adopted repo with no dotman-id, or one mid-rebase, is what
    // this command is for.
    let repo = &ctx.paths.repo;
    if fs::symlink_metadata(repo.join(".git")).is_err() {
        bail!(
            "no repo at {} yet; run `{} init` first",
            repo.display(),
            name::bin()
        );
    }
    let status = ctx
        .git
        .command()
        .args(&args.args)
        .status()
        .context("failed to run git")?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|s| s.saturating_add(128)))
        .unwrap_or(1))
}
