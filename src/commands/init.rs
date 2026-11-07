//! `dot init`: create the repo or set its remote, or clone an existing repo and apply it.

use crate::cli::{Decisions, InitArgs};
use crate::commands::{Outcome, apply};
use crate::context::Context;
use crate::git::Git;
use crate::lock::Lock;
use crate::output;
use crate::paths::private_temp_dir;
use crate::prompt;
use crate::repo_config::RepoConfig;
use anyhow::{Result, bail};

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
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
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
    let repo = &ctx.paths.repo;
    if repo.join(".git").exists() {
        bail!(
            "a repo already exists at {}; `dot apply` applies it",
            repo.display()
        );
    }
    let opts = apply::Options {
        dry_run,
        in_place: false,
        decisions: Decisions {
            keep_local: args.keep_local,
            take_repo: args.take_repo,
            ..Decisions::default()
        },
        summary: true,
    };
    if !dry_run {
        ctx.git.clone_from(source)?;
        output::success(&format!("cloned {source} into {}", repo.display()));
        return apply::apply(ctx, &[], &opts);
    }

    // The plan needs the repo's content, so a dry run clones for real, into a temp dir that is
    // removed afterwards; nothing is created at the repo path.
    let tmp = private_temp_dir("dotman-init-", &std::env::temp_dir())?;
    let mut paths = ctx.paths.clone();
    paths.repo = tmp.path().join("repo");
    let clone = Context {
        git: Git::new(paths.repo.clone(), "init"),
        paths,
    };
    clone.git.clone_from(source)?;
    output::info(&format!("would clone {source} into {}", repo.display()));
    apply::apply(&clone, &[], &opts)
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
