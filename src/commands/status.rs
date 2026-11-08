//! `dot status`: each managed key's row in the three-way model.

use crate::cli::StatusArgs;
use crate::commands::add::{self, Hold};
use crate::commands::apply::{Classified, classify_key};
use crate::context::{Context, is_under};
use crate::engine::status::{Row, Variants, Worktree};
use crate::output;
use crate::paths::derive_key;
use crate::repo_config::RepoConfig;
use crate::state::State;
use crate::system::{self, Verdict};
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
        Row::TemplateDrift => "template-drift",
        Row::Blocked => "blocked",
        Row::SymlinkAtTarget => "symlink-at-target",
    }
}

const UNREADABLE: &str = "unreadable";

/// Runs `dot status`. Takes no lock, writes nothing and always exits 0 when it can read the repo.
pub fn run(args: &StatusArgs) -> Result<()> {
    let ctx = Context::resolve("status")?;
    ctx.guard()?;
    let repo_id = ctx.repo_id()?;
    let repo_config = ctx.repo_config()?;
    let state = ctx.state(&repo_id)?;

    let (home_paths, system_filters) = system::split_inputs(&ctx.paths, &args.paths)?;
    let filters = if home_paths.is_empty() {
        Vec::new()
    } else {
        let cwd = std::env::current_dir().context("cannot read the current directory")?;
        home_paths
            .iter()
            .map(|p| derive_key(p, &cwd, &ctx.paths.home, ctx.paths.home_real.as_deref()))
            .collect::<Result<Vec<_>>>()?
    };
    let unfiltered = args.paths.is_empty();

    let sources = ctx.sources()?;
    let mut keys: BTreeSet<String> = sources.keys().map(str::to_owned).collect();
    keys.extend(state.files.keys().cloned());
    let scan = add::scan_dirs(&ctx, &repo_config, &keys)?;
    let in_scope = |key: &str| unfiltered || filters.iter().any(|f| is_under(key, f));
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);
    let variants = Variants::new(
        worktree,
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );

    let mut rows = Vec::new();
    for key in keys {
        if !in_scope(&key) {
            continue;
        }
        let label = match classify_key(&ctx, &variants, &state, &key) {
            Ok(Classified::Unmanaged) => continue,
            Ok(Classified::Item(item)) => item.token(),
            Ok(Classified::Ambiguous(_)) => "ambiguous",
            Ok(Classified::Invalid(_)) => "invalid",
            Ok(Classified::Locked(_)) => "locked",
            Err(e) => {
                output::warning(&format!("{key}: {e:#}"));
                UNREADABLE
            }
        };
        rows.push((label, key));
    }

    let wanted = |key: &str| unfiltered || system_filters.iter().any(|f| is_under(key, f));
    rows.extend(system_rows(&ctx, &repo_config, &state, wanted)?);

    let new: Vec<&String> = scan.new.keys().filter(|k| in_scope(k)).collect();
    let held: Vec<(&String, Hold)> = scan
        .held
        .iter()
        .filter(|(k, _)| in_scope(k))
        .map(|(k, h)| (k, *h))
        .collect();

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
    if args.porcelain {
        for key in &new {
            write!(out, "new\t{key}\0").context("cannot write to stdout")?;
        }
        for (key, _) in &held {
            write!(out, "held\t{key}\0").context("cannot write to stdout")?;
        }
    } else {
        list_new(&mut out, &new, &held).context("cannot write to stdout")?;
        for note in &scan.notes {
            output::warning(note);
        }
        let dirty = rows.iter().filter(|(label, _)| *label != "clean").count();
        output::info(&format!(
            "{} managed, {} clean, {dirty} to look at",
            rows.len(),
            rows.len().saturating_sub(dirty)
        ));
    }
    Ok(())
}

fn list_new(
    out: &mut impl Write,
    new: &[&String],
    held: &[(&String, Hold)],
) -> std::io::Result<()> {
    if !new.is_empty() {
        writeln!(out, "\nnew files, which `dot sync` captures:")?;
        for key in new {
            writeln!(out, "  {key}")?;
        }
    }
    if !held.is_empty() {
        writeln!(
            out,
            "\nheld back; `dot add <file>` keeps one, or add an [ignore] pattern to .dotman.toml:"
        )?;
        for (key, hold) in held {
            writeln!(out, "  {key} ({})", hold.reason())?;
        }
    }
    Ok(())
}

fn system_rows(
    ctx: &Context,
    repo_config: &RepoConfig,
    state: &State,
    wanted: impl Fn(&str) -> bool,
) -> Result<Vec<(&'static str, String)>> {
    let scope = ctx.system_scope();
    let mut rows = Vec::new();
    for found in system::evaluate(ctx, repo_config, state)? {
        if !wanted(&found.key) {
            continue;
        }
        let key = found.key;
        let label = match found.verdict {
            Verdict::Refused(why) => {
                output::warning(&format!("{key}: {why}"));
                "invalid"
            }
            Verdict::Invalid(problems) => {
                for (source, reason) in problems {
                    output::warning(&format!("{key}: skipped, {source}: {reason}"));
                }
                "invalid"
            }
            Verdict::Ambiguous(_) => "ambiguous",
            _ if found.off => "system-off",
            Verdict::Unreadable(why) => {
                output::warning(&format!("{key}: {why}"));
                UNREADABLE
            }
            Verdict::Row { row, .. } => {
                let siblings = scope.siblings(&key);
                if !siblings.is_empty() {
                    output::warning(&system::sibling_note(&key, &siblings));
                }
                token(row)
            }
        };
        rows.push((label, key));
    }
    Ok(rows)
}
