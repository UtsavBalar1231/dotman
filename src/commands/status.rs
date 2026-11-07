//! `dot status`: each managed key's row in the three-way model.

use crate::cli::StatusArgs;
use crate::context::{Context, is_under};
use crate::engine::status::{Row, Source, Worktree, classify};
use crate::engine::{Tuple, observe};
use crate::output;
use crate::paths::derive_key;
use anyhow::{Context as _, Result};
use std::collections::BTreeSet;
use std::io::Write;

/// The stable porcelain token for a row.
#[must_use]
pub const fn token(row: Row) -> &'static str {
    match row {
        Row::GoneEverywhere => "gone",
        Row::Forgotten => "forgotten",
        Row::ForgottenEdited => "forgotten-edited",
        Row::Clean { .. } => "clean",
        Row::NotApplied => "not-applied",
        Row::DeletedLocally => "deleted-locally",
        Row::DeletedChangedUpstream => "deleted-changed-upstream",
        Row::Existing => "existing",
        Row::ChangedLocally => "changed-locally",
        Row::ChangedInRepo => "changed-in-repo",
        Row::BothChanged => "both-changed",
        Row::Blocked => "blocked",
        Row::SymlinkAtTarget => "symlink-at-target",
    }
}

const UNREADABLE: &str = "unreadable";

/// Runs `dot status`. Takes no lock, writes nothing and always exits 0 when it can read the repo.
pub fn run(args: &StatusArgs) -> Result<()> {
    let ctx = Context::resolve("status")?;
    let repo_id = ctx.repo_id()?;
    let repo_config = ctx.repo_config()?;
    let state = ctx.state(&repo_id)?;

    let filters = if args.paths.is_empty() {
        Vec::new()
    } else {
        let cwd = std::env::current_dir().context("cannot read the current directory")?;
        args.paths
            .iter()
            .map(|p| derive_key(p, &cwd, &ctx.paths.home, ctx.paths.home_real.as_deref()))
            .collect::<Result<Vec<_>>>()?
    };

    let mut keys: BTreeSet<String> = ctx.managed()?.into_keys().collect();
    keys.extend(state.files.keys().cloned());
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);

    let mut rows = Vec::new();
    for key in keys {
        if !filters.is_empty() && !filters.iter().any(|f| is_under(&key, f)) {
            continue;
        }
        let r = worktree.get(&key);
        let h = observe(&ctx.paths.home, &key);
        let s = state.files.get(&key).map(Tuple::from_entry).transpose();
        let label = match (&r, &h, &s) {
            (Ok(r), Ok(h), Ok(s)) => match classify(r.as_ref().map(|e| &e.tuple), h, s.as_ref()) {
                Some(row) => token(row),
                None => continue,
            },
            _ => UNREADABLE,
        };
        rows.push((label, key));
    }

    let mut out = std::io::stdout().lock();
    for (label, key) in &rows {
        if args.porcelain {
            write!(out, "{label}\t{key}\0")
        } else if *label == "clean" {
            Ok(())
        } else {
            writeln!(out, "{label:<26}{key}")
        }
        .context("cannot write to stdout")?;
    }
    if !args.porcelain {
        let dirty = rows.iter().filter(|(label, _)| *label != "clean").count();
        output::info(&format!(
            "{} managed, {} clean, {dirty} to look at",
            rows.len(),
            rows.len().saturating_sub(dirty)
        ));
    }
    Ok(())
}
