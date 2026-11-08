//! `dotman git`: git in the repo with dotman's isolation flags. Unguarded and unlocked, because it is
//! how a rebase gets resolved.

use crate::cli::GitArgs;
use crate::context::Context;
use anyhow::{Context as _, Result};
use std::os::unix::process::ExitStatusExt;

/// Runs git with `args` and returns its exit code, 128 plus the signal number if it was killed.
pub fn run(args: &GitArgs) -> Result<i32> {
    let ctx = Context::resolve("git")?;
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
