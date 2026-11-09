//! `dotman forget`: stop managing a path. Only the repo and the state change, never `$HOME`.

use crate::cli::ForgetArgs;
use crate::commands::add::Ignore;
use crate::context::{Context, is_real_dir, is_under};
use crate::lock::Lock;
use crate::output;
use crate::paths::derive_key;
use crate::repo_config::RepoConfig;
use crate::state::State;
use crate::system;
use anyhow::{Context as _, Result, bail};
use glob::Pattern;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Runs `dotman forget`.
///
/// # Errors
///
/// Fails when an input is not managed, the lock is held, the repo is not initialised or has a
/// rebase or merge in progress, or a repo file cannot be removed.
pub fn run(args: &ForgetArgs) -> Result<()> {
    let ctx = Context::resolve("forget")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
    let repo_id = ctx.repo_id()?;
    let mut repo_config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;
    let sources = ctx.sources()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;

    // Every input is resolved before anything changes, so a typo leaves the repo as it was.
    let mut gone: BTreeSet<String> = BTreeSet::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut inputs: Vec<String> = Vec::new();
    for input in &args.paths {
        let key = derive_key(input, &cwd, &ctx.paths.home, ctx.paths.home_real.as_deref())?;
        inputs.push(key.clone());
        let before = (gone.len(), dirs.len());
        gone.extend(
            sources
                .keys()
                .filter(|k| is_under(k, &key))
                .map(str::to_owned),
        );
        gone.extend(state.files.keys().filter(|k| is_under(k, &key)).cloned());
        dirs.extend(
            repo_config
                .dirs
                .paths
                .iter()
                .filter(|d| is_under(d, &key))
                .cloned(),
        );
        if (gone.len(), dirs.len()) == before && !gone.contains(&key) {
            bail!("{key} is not managed");
        }
    }

    let verb = if dry_run { "would forget" } else { "forget" };
    for key in &gone {
        output::action(verb, key);
    }
    for dir in &dirs {
        output::action(verb, &format!("{dir}/"));
    }
    if dry_run {
        return Ok(());
    }

    repo_config.dirs.paths.retain(|d| !dirs.contains(d));
    for key in &inputs {
        let is_key = gone.contains(key) || sources.contains(key);
        if !is_key && !dirs.contains(key) && under_dirs(&repo_config, key) {
            ignore_forgotten(
                &mut repo_config,
                &format!("{}/**", Pattern::escape(key)),
                key,
                true,
            )?;
        }
    }
    remove_keys(&ctx, &mut repo_config, &mut state, &gone)?;

    repo_config.save(&ctx.repo_config_file(), false)?;
    state.save(&ctx.paths.state_file(), false)?;
    Ok(())
}

/// Removes `keys`, every variant included, from the worktree, `[modes]` and the state, then drops
/// `dir/` modes that no remaining key lies under. `$HOME` is left alone. A key under a remaining
/// `[dirs]` entry gets an `[ignore]` pattern, because the scan would capture the file that stays
/// in `$HOME` again.
pub(crate) fn remove_keys(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    state: &mut State,
    keys: &BTreeSet<String>,
) -> Result<()> {
    let sources = ctx.sources()?;
    for key in keys {
        for source in sources.sources_of(key) {
            remove_source(&ctx.paths.repo, source)?;
            repo_config.modes.remove(source);
        }
        repo_config.modes.remove(key);
        state.files.remove(key);
        if under_dirs(repo_config, key) {
            ignore_forgotten(repo_config, &Pattern::escape(key), key, false)?;
        }
    }
    let remaining = ctx.managed()?;
    repo_config.modes.retain(|key, _| {
        !key.ends_with('/') || remaining.keys().any(|k| k.starts_with(key.as_str()))
    });
    Ok(())
}

/// Removes system `keys`, every variant included, from `.dotman/system`, `[system]` and the state.
pub(crate) fn remove_system_keys(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    state: &mut State,
    keys: &BTreeSet<String>,
) -> Result<()> {
    let sources = ctx.system_sources()?;
    for key in keys {
        for source in sources.sources_of(system::rel(key)?) {
            remove_source(&ctx.paths.repo, &system::source_path(source))?;
        }
        repo_config.system.remove(key);
        state.system.remove(key);
    }
    Ok(())
}

fn remove_source(repo: &Path, source: &str) -> Result<()> {
    let path = repo.join(source);
    match fs::remove_file(&path) {
        Ok(()) => remove_empty_parents(repo, &path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
    Ok(())
}

fn under_dirs(repo_config: &RepoConfig, key: &str) -> bool {
    repo_config
        .dirs
        .paths
        .iter()
        .any(|dir| dir != key && is_under(key, dir))
}

/// Appends `pattern` to `[ignore]` unless a pattern already hides `key`.
fn ignore_forgotten(
    repo_config: &mut RepoConfig,
    pattern: &str,
    key: &str,
    is_dir: bool,
) -> Result<()> {
    let patterns = &mut repo_config.ignore.patterns;
    let hidden = Ignore::compile(patterns)?.matches(key, is_dir);
    if !hidden && !patterns.iter().any(|p| p == pattern) {
        patterns.push(pattern.to_owned());
    }
    Ok(())
}

// Git does not track directories, so an emptied one would only linger in the worktree.
fn remove_empty_parents(root: &Path, removed: &Path) {
    let mut dir = removed.parent();
    while let Some(d) = dir.filter(|d| *d != root && d.starts_with(root) && is_real_dir(d)) {
        if fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}
