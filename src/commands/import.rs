//! `dotman import`: convert another setup into the repo, or overlay one onto it.

use crate::cli::ImportArgs;
use crate::commands::Outcome;
use crate::context::Context;
use crate::import;
use crate::lock::Lock;
use crate::remote;
use anyhow::Result;

/// Runs `dotman import`.
pub fn run(args: &ImportArgs) -> Result<Outcome> {
    let ctx = Context::resolve("import")?;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), args.dry_run)?;
    ctx.guard()?;
    let outcome = import::run(&ctx, args)?;
    if !args.dry_run {
        remote::note_origin(&ctx, false);
    }
    Ok(outcome)
}
