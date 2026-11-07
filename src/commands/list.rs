//! `dot list`: the managed set.

use crate::cli::ListArgs;
use crate::context::Context;
use crate::state::Kind;
use crate::variant::Sources;
use anyhow::{Context as _, Result};
use std::io::Write;

/// Runs `dot list`. Takes no lock and writes nothing. A key is listed with the source this machine
/// applies; one with none, or with ambiguous or invalid variants, is left to `dot status`.
pub fn run(args: &ListArgs) -> Result<()> {
    let ctx = Context::resolve("list")?;
    ctx.guard()?;
    ctx.repo_id()?;
    let managed = ctx.managed()?;
    let sources = Sources::group(managed.keys().map(String::as_str));
    let mut out = std::io::stdout().lock();
    for key in sources.keys() {
        let Some(source) = sources.select(key, ctx.identity()).winner().copied() else {
            continue;
        };
        let Some(kind) = managed.get(source) else {
            continue;
        };
        let kind = match kind {
            Kind::File => "file",
            Kind::Symlink => "symlink",
        };
        if args.porcelain {
            write!(out, "{kind}\t{key}\0")
        } else if source == key {
            writeln!(out, "{kind:<8}{key}")
        } else {
            writeln!(out, "{kind:<8}{key}  ({source})")
        }
        .context("cannot write to stdout")?;
    }
    Ok(())
}
