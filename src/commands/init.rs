//! `dot init`: create the repo, or set its remote.

use crate::cli::InitArgs;
use crate::context::Context;
use crate::lock::Lock;
use crate::output;
use crate::prompt;
use crate::repo_config::RepoConfig;
use anyhow::{Result, bail};

/// Runs `dot init`.
pub fn run(args: &InitArgs) -> Result<()> {
    let ctx = Context::resolve("init")?;
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
        return Ok(());
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
    Ok(())
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
