//! `dot init`: create the repo or set its remote, or clone an existing repo and apply it.

use crate::cli::{Decisions, ImportArgs, InitArgs};
use crate::commands::{Outcome, apply};
use crate::context::Context;
use crate::hooks::Policy;
use crate::import;
use crate::lock::Lock;
use crate::output;
use crate::paths::private_temp_dir;
use crate::prompt;
use crate::repo_config::RepoConfig;
use anyhow::{Result, anyhow, bail};
use std::path::PathBuf;

/// Runs `dot init`.
pub fn run(args: &InitArgs) -> Result<Outcome> {
    let ctx = Context::resolve("init")?;
    if let Some(source) = &args.source {
        return clone_and_apply(&ctx, source, args);
    }
    let dry_run = args.dry_run;
    if let Some(url) = &args.remote {
        check_url(url)?;
    }
    if !args.no_import && import::is_fresh(&ctx)? {
        let found = import::detect(&ctx)?;
        if !found.is_empty()
            && let Some(outcome) = offer_import(&ctx, &found, args)?
        {
            return Ok(outcome);
        }
    }
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
    let repo = &ctx.paths.repo;
    let config_file = ctx.repo_config_file();
    let exists = repo.join(".git").exists();

    if dry_run {
        if !exists {
            output::info(&format!("would create the repo at {}", repo.display()));
        }
        if !config_file.exists() {
            output::info(&format!("would write {}", config_file.display()));
        }
        if let Some(url) = &args.remote {
            output::info(&format!("would set origin to {url}"));
        }
        return Ok(Outcome::Done);
    }

    ctx.git.init()?;
    if !config_file.exists() {
        RepoConfig::default().save(&config_file, false)?;
    }
    if !exists {
        output::success(&format!("created the repo at {}", repo.display()));
    }

    let current = ctx.git.remote_url("origin")?;
    let wanted = match &args.remote {
        Some(url) => Some(url.clone()),
        None if current.is_none() && prompt::interactive() => ask_remote()?,
        None => None,
    };
    if let Some(url) = wanted
        && current.as_deref() != Some(url.as_str())
    {
        ctx.git.set_remote_url("origin", &url)?;
        output::success(&format!("origin is {url}"));
    }
    Ok(Outcome::Done)
}

fn clone_and_apply(ctx: &Context, source: &str, args: &InitArgs) -> Result<Outcome> {
    check_url(source)?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
    let repo = &ctx.paths.repo;
    if repo.join(".git").exists() {
        bail!(
            "a repo already exists at {}; `dot apply` applies it",
            repo.display()
        );
    }
    let opts = apply::Options {
        dry_run,
        decisions: Decisions {
            keep_local: args.keep_local,
            take_repo: args.take_repo,
            ..Decisions::default()
        },
        summary: true,
        ..apply::Options::default()
    };
    let policy = Policy {
        approve: args.approve_hooks,
        dry_run,
        never_ask: true,
        ..Policy::default()
    };
    if !dry_run {
        ctx.git.clone_from(source)?;
        output::success(&format!("cloned {source} into {}", repo.display()));
        let applied = apply::apply(ctx, &[], &opts)?;
        return apply::finish(ctx, &applied, policy);
    }

    // The plan needs the repo's content, so a dry run clones for real, into a temp dir that is
    // removed afterwards; nothing is created at the repo path.
    let tmp = private_temp_dir("dotman-init-", &std::env::temp_dir())?;
    let mut paths = ctx.paths.clone();
    paths.repo = tmp.path().join("repo");
    let clone = Context::new(paths, ctx.machine.clone(), "init");
    clone.git.clone_from(source)?;
    output::info(&format!("would clone {source} into {}", repo.display()));
    let applied = apply::apply(&clone, &[], &opts)?;
    apply::finish(&clone, &applied, policy)
}

/// `None` when the user chose to start fresh and `init` goes on.
fn offer_import(
    ctx: &Context,
    found: &[(&'static str, PathBuf)],
    args: &InitArgs,
) -> Result<Option<Outcome>> {
    output::info("found an existing setup:");
    for (n, (label, path)) in found.iter().enumerate() {
        output::info(&format!(
            "  {}. {label}: {}",
            n.saturating_add(1),
            path.display()
        ));
    }
    if !prompt::interactive() {
        output::info("import one with:");
        for (label, path) in found {
            output::info(&format!(
                "  dot import {}    # {label}",
                shell_words::quote(&path.to_string_lossy())
            ));
        }
        if let Some(url) = &args.remote {
            output::info(&format!(
                "then run `dot init --remote {}`",
                shell_words::quote(url)
            ));
        }
        output::info("or start a fresh repo with:");
        let remote = args.remote.as_ref().map_or_else(String::new, |url| {
            format!(" --remote {}", shell_words::quote(url))
        });
        output::info(&format!("  dot init --no-import{remote}"));
        return Ok(Some(Outcome::Attention));
    }
    let fresh = found.len().saturating_add(1);
    let answer = prompt::ask(&format!(
        "Import which? 1-{}, {fresh} to start fresh, empty to do nothing:",
        found.len()
    ))?;
    if answer.is_empty() {
        output::info("nothing created");
        return Ok(Some(Outcome::Done));
    }
    let choice: usize = answer
        .parse()
        .map_err(|_| anyhow!("`{answer}` is not a number from 1 to {fresh}"))?;
    if choice == fresh {
        return Ok(None);
    }
    let (_, path) = choice
        .checked_sub(1)
        .and_then(|i| found.get(i))
        .ok_or_else(|| anyhow!("{choice} is not a number from 1 to {fresh}"))?;
    let import_args = ImportArgs {
        source: Some(path.clone().into_os_string()),
        overlay: None,
        include_missing: false,
        render_templates: false,
        swap: false,
        yes: false,
        dry_run: args.dry_run,
    };
    let _lock = Lock::acquire(&ctx.paths.lock_file(), args.dry_run)?;
    ctx.guard()?;
    let outcome = import::run(ctx, &import_args)?;
    if let Some(url) = &args.remote
        && !args.dry_run
    {
        ctx.git.set_remote_url("origin", url)?;
        output::success(&format!("origin is {url}"));
    }
    Ok(Some(outcome))
}

fn ask_remote() -> Result<Option<String>> {
    let answer = prompt::ask("Remote URL for origin (empty to skip):")?;
    if answer.is_empty() {
        return Ok(None);
    }
    check_url(&answer)?;
    Ok(Some(answer))
}

// A leading `-` would be read by git as an option.
fn check_url(url: &str) -> Result<()> {
    if url.is_empty() || url.starts_with('-') {
        bail!("`{url}` is not a usable remote URL");
    }
    Ok(())
}
