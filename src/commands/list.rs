//! `dotman list`: the managed set.

use crate::cli::ListArgs;
use crate::context::Context;
use crate::output::escape_line;
use crate::state::Kind;
use crate::system;
use crate::variant::{Flag, Sources};
use anyhow::{Context as _, Result};
use std::io::Write;

/// Runs `dotman list`. Takes no lock and writes nothing. A key is listed with the source this machine
/// applies; one with none, or with ambiguous or invalid variants, is left to `dotman status`.
///
/// # Errors
///
/// Fails when the repo is not initialised, has a rebase or merge in progress, or stdout cannot be
/// written.
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
        let flag = if sources.flag(key, source) == Flag::Age {
            "  [age]"
        } else {
            ""
        };
        if args.porcelain {
            write!(out, "{kind}\t{key}\0")
        } else if source == key {
            writeln!(out, "{kind:<8}{}{flag}", escape_line(key))
        } else {
            writeln!(
                out,
                "{kind:<8}{}  ({}){flag}",
                escape_line(key),
                escape_line(source)
            )
        }
        .context("cannot write to stdout")?;
    }
    // The kind column says `system`, so a system key is told apart from a home key by more than
    // its leading `/`.
    let system_managed = ctx.managed_system()?;
    let system_sources = Sources::group(system_managed.keys().map(String::as_str));
    for rel in system_sources.keys() {
        let Some(source) = system_sources.select(rel, ctx.identity()).winner().copied() else {
            continue;
        };
        if system_managed.get(source) != Some(&Kind::File)
            || system_sources.flag(rel, source) != Flag::Plain
        {
            continue;
        }
        let key = system::key_of(rel);
        if args.porcelain {
            write!(out, "system\t{key}\0")
        } else if source == rel {
            writeln!(out, "{:<8}{}", "system", escape_line(&key))
        } else {
            writeln!(
                out,
                "{:<8}{}  ({})",
                "system",
                escape_line(&key),
                escape_line(&system::source_path(source))
            )
        }
        .context("cannot write to stdout")?;
    }
    Ok(())
}
