//! `dot forget`: stop managing a path. Only the repo and the state change, never `$HOME`.

use crate::cli::ForgetArgs;
use crate::context::{Context, is_real_dir, is_under};
use crate::lock::Lock;
use crate::output;
use crate::paths::derive_key;
use crate::repo_config::RepoConfig;
use crate::state::State;
use anyhow::{Context as _, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Runs `dot forget`.
pub fn run(args: &ForgetArgs) -> Result<()> {
    let ctx = Context::resolve("forget")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    let repo_id = ctx.repo_id()?;
    let mut repo_config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;
    let managed = ctx.managed()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;

    // Every input is resolved before anything changes, so a typo leaves the repo as it was.
    let mut gone: BTreeSet<String> = BTreeSet::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for input in &args.paths {
        let key = derive_key(input, &cwd, &ctx.paths.home, ctx.paths.home_real.as_deref())?;
        let before = (gone.len(), dirs.len());
        gone.extend(managed.keys().filter(|k| is_under(k, &key)).cloned());
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

    for key in &gone {
        output::action("forget", key);
    }
    for dir in &dirs {
        output::action("forget", &format!("{dir}/"));
    }
    if dry_run {
        return Ok(());
    }

    remove_keys(&ctx, &mut repo_config, &mut state, &gone)?;
    repo_config.dirs.paths.retain(|d| !dirs.contains(d));

    repo_config.save(&ctx.repo_config_file(), false)?;
    state.save(&ctx.paths.state_file(), false)?;
    Ok(())
}

/// Removes `keys` from the worktree, `[modes]` and the state, then drops `dir/` modes that no
/// remaining key lies under. `[dirs]` entries and `$HOME` are left alone.
pub(crate) fn remove_keys(
    ctx: &Context,
    repo_config: &mut RepoConfig,
    state: &mut State,
    keys: &BTreeSet<String>,
) -> Result<()> {
    for key in keys {
        let path = ctx.paths.repo.join(key);
        match fs::remove_file(&path) {
            Ok(()) => remove_empty_parents(&ctx.paths.repo, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot remove {}", path.display())),
        }
        repo_config.modes.remove(key);
        state.files.remove(key);
    }
    let remaining = ctx.managed()?;
    repo_config.modes.retain(|key, _| {
        !key.ends_with('/') || remaining.keys().any(|k| k.starts_with(key.as_str()))
    });
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
