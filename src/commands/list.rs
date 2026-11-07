//! `dot list`: the managed set.

use crate::cli::ListArgs;
use crate::context::Context;
use crate::state::Kind;
use anyhow::{Context as _, Result};
use std::io::Write;

/// Runs `dot list`. Takes no lock and writes nothing.
pub fn run(args: &ListArgs) -> Result<()> {
    let ctx = Context::resolve("list")?;
    ctx.guard()?;
    ctx.repo_id()?;
    let managed = ctx.managed()?;
    let mut out = std::io::stdout().lock();
    for (key, kind) in &managed {
        let kind = match kind {
            Kind::File => "file",
            Kind::Symlink => "symlink",
        };
        if args.porcelain {
            write!(out, "{kind}\t{key}\0")
        } else {
            writeln!(out, "{kind:<8}{key}")
        }
        .context("cannot write to stdout")?;
    }
    Ok(())
}
