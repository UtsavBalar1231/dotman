//! `dotman status`: each managed key's row in the three-way model.

use crate::cli::StatusArgs;
use crate::commands::add::{self, Hold};
use crate::commands::apply::{Classified, classify_key};
use crate::commands::sync;
use crate::context::{self, Context, is_under};
use crate::engine::status::{Row, TemplateError, Variants, Worktree};
use crate::git::StatusEntry;
use crate::name;
use crate::output::{self, escape_line};
use crate::paths::derive_key;
use crate::repo_config::RepoConfig;
use crate::secrets::Finding;
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

/// Runs `dotman status`. Takes no lock, writes nothing and always exits 0 when it can read the repo.
///
/// # Errors
///
/// Fails when the repo is not initialised or has a rebase or merge in progress, or the repo, the
/// state or an input cannot be read.
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
    // Clean rows whose S lags, as after a run killed before it saved the state.
    let mut unrecorded = 0_usize;
    for key in keys {
        if !in_scope(&key) {
            continue;
        }
        if let Some(label) = home_label(&ctx, &variants, &state, &key, &mut unrecorded) {
            rows.push((label, key));
        }
    }

    let wanted = |key: &str| unfiltered || system_filters.iter().any(|f| is_under(key, f));
    rows.extend(system_rows(&ctx, &repo_config, &state, wanted)?);
    let mut twins = BTreeSet::new();
    for (a, b) in ctx.index_collisions()? {
        output::warning(&format!(
            "{} and {} differ only in case or Unicode normalization; `{} apply` and `{} sync` refuse until one is renamed",
            escape_line(&a),
            escape_line(&b),
            name::bin(),
            name::bin()
        ));
        twins.extend([a, b]);
    }
    rows.extend(twins.into_iter().map(|key| ("collision", key)));

    let new: Vec<&String> = scan.new.keys().filter(|k| in_scope(k)).collect();
    let held: Vec<(&String, Hold)> = scan
        .held
        .iter()
        .filter(|(k, _)| in_scope(k))
        .map(|(k, h)| (k, *h))
        .collect();
    let worktree_status = ctx.git.status()?;
    let mut in_repo = sync::held_entries(&ctx, &worktree_status);
    in_repo.retain(|(entry, _)| in_scope(&entry.path.to_string_lossy()));
    report_unnamed(&ctx, in_scope)?;

    let mut out = std::io::stdout().lock();
    for (label, key) in &rows {
        if args.porcelain {
            write!(out, "{label}\t{key}\0")
        } else if *label == "clean" {
            Ok(())
        } else {
            writeln!(out, "{label:<26}{}", escape_line(key))
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
        for (entry, _) in &in_repo {
            write!(out, "held-in-repo\t{}\0", entry.path.to_string_lossy())
                .context("cannot write to stdout")?;
        }
    } else {
        list_new(&mut out, &new, &held).context("cannot write to stdout")?;
        list_held_in_repo(&mut out, &ctx, &in_repo).context("cannot write to stdout")?;
        for note in &scan.notes {
            output::warning(note);
        }
        summarize(&rows, unrecorded, [new.len(), held.len(), in_repo.len()]);
        report_remote(&ctx)?;
    }
    Ok(())
}

/// The untracked repo files that sync never commits because their names are not UTF-8, and that
/// no `[ignore]` pattern silences.
fn report_unnamed(ctx: &Context, in_scope: impl Fn(&str) -> bool) -> Result<()> {
    for path in context::unnamed_untracked(ctx)? {
        let shown = path.to_string_lossy();
        if in_scope(&shown) {
            output::warning(&format!(
                "{}: in the repo, never committed: its name is not valid UTF-8, which no key can have; rename or remove it in {}, or keep it out with an [ignore] pattern",
                escape_line(&shown),
                ctx.paths.repo.display()
            ));
        }
    }
    Ok(())
}

/// How HEAD stands against `origin/<branch>` as it was last fetched; nothing is fetched.
fn report_remote(ctx: &Context) -> Result<()> {
    let git = &ctx.git;
    if git.remote_url("origin")?.is_none() || git.head()?.is_none() {
        return Ok(());
    }
    let Some(branch) = git.branch()? else {
        return Ok(());
    };
    let upstream = format!("refs/remotes/origin/{branch}");
    let commits = |n: usize| if n == 1 { "commit" } else { "commits" };
    // No branch was ever pushed, as when every push after `init --remote` failed.
    if git.resolve_commit(&upstream)?.is_none() {
        let out = git
            .command()
            .args([
                "rev-list",
                "--count",
                "HEAD",
                "--not",
                "--remotes=origin",
                "--",
            ])
            .output()
            .context("failed to run git")?;
        let count = String::from_utf8_lossy(&out.stdout).trim().parse::<usize>();
        if let (Ok(n @ 1..), true) = (count, out.status.success()) {
            output::info(&format!(
                "{n} {} not pushed yet: run `{} sync`",
                commits(n),
                name::bin()
            ));
        }
        return Ok(());
    }
    let out = git
        .command()
        .args(["rev-list", "--left-right", "--count"])
        .arg(format!("HEAD...{upstream}"))
        .arg("--")
        .output()
        .context("failed to run git")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let counts: Vec<usize> = text
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let (&[ahead, behind], true) = (counts.as_slice(), out.status.success()) else {
        return Ok(());
    };
    let position = match (ahead, behind) {
        (0, 0) => return Ok(()),
        (ahead, 0) => format!("{ahead} {} to push", commits(ahead)),
        (0, behind) => format!("{behind} {} to pull", commits(behind)),
        (ahead, behind) => format!("{ahead} {} to push, {behind} to pull", commits(ahead)),
    };
    output::info(&format!("{position}: run `{} sync`", name::bin()));
    Ok(())
}

/// The token for a home key; `None` when it is not managed. A clean key whose S lags counts in
/// `unrecorded`.
fn home_label(
    ctx: &Context,
    variants: &Variants<'_>,
    state: &State,
    key: &str,
    unrecorded: &mut usize,
) -> Option<&'static str> {
    Some(match classify_key(ctx, variants, state, key) {
        Ok(Classified::Unmanaged) => return None,
        Ok(Classified::Item(item)) => {
            if item.token() == "clean" && item.row() == (Row::Clean { update_state: true }) {
                *unrecorded = unrecorded.saturating_add(1);
            }
            if let Some((kept, repo)) = item.kept() {
                output::info(&format!(
                    "{key}: kept at {kept:04o} here; the repo says {repo:04o}"
                ));
            }
            item.token()
        }
        Ok(Classified::Ambiguous(_)) => "ambiguous",
        Ok(Classified::Invalid(_)) => "invalid",
        Ok(Classified::Locked(_)) => "locked",
        Err(e) => {
            output::warning(&format!("{key}: {e:#}"));
            if e.is::<TemplateError>() {
                "template-error"
            } else {
                UNREADABLE
            }
        }
    })
}

/// New and held files are under `[dirs]`, so they count as managed and as to look at.
fn summarize(rows: &[(&str, String)], unrecorded: usize, [new, held, held_in_repo]: [usize; 3]) {
    let clean = rows.iter().filter(|(label, _)| *label == "clean").count();
    let managed = rows.len().saturating_add(new).saturating_add(held);
    let breakdown = if new > 0 || held > 0 {
        format!(" ({new} new, {held} held back)")
    } else {
        String::new()
    };
    let in_repo = if held_in_repo > 0 {
        format!(", {held_in_repo} held in the repo")
    } else {
        String::new()
    };
    output::info(&format!(
        "{managed} managed, {clean} clean, {} to look at{breakdown}{in_repo}",
        managed.saturating_sub(clean)
    ));
    if unrecorded > 0 {
        output::info(&format!(
            "run `{} apply` to record {unrecorded} {} that already {} the repo",
            name::bin(),
            if unrecorded == 1 { "path" } else { "paths" },
            if unrecorded == 1 { "matches" } else { "match" }
        ));
    }
}

fn list_new(
    out: &mut impl Write,
    new: &[&String],
    held: &[(&String, Hold)],
) -> std::io::Result<()> {
    if !new.is_empty() {
        writeln!(out, "\nnew files, which `{} sync` captures:", name::bin())?;
        for key in new {
            writeln!(out, "  {}", escape_line(key))?;
        }
    }
    if !held.is_empty() {
        writeln!(
            out,
            "\nheld back; an [ignore] pattern in .dotman.toml silences one:"
        )?;
        for (key, hold) in held {
            writeln!(out, "  {} ({})", escape_line(key), hold.listed())?;
        }
    }
    Ok(())
}

/// The repo edits that sync holds, which a clean row cannot show: `dotman edit` leaves the key
/// clean while its uncommitted copy holds a credential.
fn list_held_in_repo(
    out: &mut impl Write,
    ctx: &Context,
    held: &[(&StatusEntry, Finding)],
) -> std::io::Result<()> {
    if held.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "\nheld in the repo; `{} sync` commits none of it, and with a remote stops before the pull:",
        name::bin()
    )?;
    for (entry, found) in held {
        writeln!(
            out,
            "  {}: {found}\n    {}",
            escape_line(&entry.path.to_string_lossy()),
            escape_line(&sync::held_remedy(ctx, entry, *found))
        )?;
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
