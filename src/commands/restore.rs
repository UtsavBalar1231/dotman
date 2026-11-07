//! `dot restore`: write an earlier version of a managed file into `$HOME`.
//!
//! Only `$HOME` changes. The repo and the state stay as they were, so the next `status` shows the
//! file as changed locally and `sync` records it as a new commit.

use crate::cli::RestoreArgs;
use crate::context::Context;
use crate::engine::write::{WriteOptions, WriteOutcome, backup_run_dir, safe_symlink, safe_write};
use crate::engine::{Home, Tuple, observe_portable, recorded_mode};
use crate::lock::Lock;
use crate::output;
use crate::paths::derive_key;
use anyhow::{Context as _, Result, anyhow, bail};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::SystemTime;

/// Runs `dot restore`.
pub fn run(args: &RestoreArgs) -> Result<()> {
    let ctx = Context::resolve("restore")?;
    let dry_run = args.dry_run;
    let _lock = Lock::acquire(&ctx.paths.lock_file(), dry_run)?;
    ctx.guard()?;
    ctx.repo_id()?;
    ctx.git.check_version()?;
    let repo_config = ctx.repo_config()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let home = &ctx.paths.home;
    let key = derive_key(&args.path, &cwd, home, ctx.paths.home_real.as_deref())?;
    if !ctx.managed()?.contains_key(&key) {
        bail!("{key} is not managed");
    }
    refuse_template(&key)?;

    let from = args.from.as_deref().unwrap_or("HEAD");
    let rev = match ctx.git.resolve_commit(from)? {
        Some(rev) => rev,
        None => ctx.git.rev_before(from)?.ok_or_else(|| {
            anyhow!("`{from}` is neither a revision nor a date after the first commit")
        })?,
    };
    let short = rev.get(..8).unwrap_or(&rev);
    let git_mode = ctx
        .git
        .tree_mode(&rev, &key)?
        .ok_or_else(|| anyhow!("{key} does not exist in {short}"))?;
    let blob = ctx.git.show_blob(&rev, &key)?;

    let exec_default = if git_mode == "100755" { 0o755 } else { 0o644 };
    let mode = recorded_mode(&repo_config.modes, &key)?.unwrap_or(exec_default);
    let link = (git_mode == "120000").then(|| Path::new(OsStr::from_bytes(&blob)));
    let wanted = link.map_or_else(|| Tuple::of_file(&blob, mode), Tuple::of_link);

    let expected = observe_portable(home, ctx.paths.home_real.as_deref(), &key)?;
    if matches!(&expected, Home::Present(h) if *h == wanted) {
        output::info(&format!("{key}: already matches {short}"));
        return Ok(());
    }
    let backups = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
    let opts = WriteOptions {
        dry_run,
        in_place: false,
        backup: Some(&backups),
        expected: &expected,
        modes: &repo_config.modes,
    };
    let outcome = match link {
        Some(target) => safe_symlink(home, &key, target, &opts)?,
        None => safe_write(home, &key, &blob, mode, &opts)?,
    };
    match outcome {
        WriteOutcome::Written(written) => {
            let verb = if dry_run { "would restore" } else { "restore" };
            output::action(verb, &format!("{key} from {short}"));
            if let Some(backup) = &written.backup {
                let verb = if dry_run {
                    "would back up to"
                } else {
                    "backed up to"
                };
                output::info(&format!("  {verb} {}", backup.display()));
            }
            if let Some(warning) = written.warning() {
                output::warning(&format!("{key}: {warning}"));
            }
            Ok(())
        }
        WriteOutcome::Skipped(skip) => bail!("{key}: {skip}"),
    }
}

/// Templates are rendered, so their repo bytes are not what `$HOME` holds. None exist yet.
// reason: task 14 adds the template check that returns the error
#[allow(clippy::unnecessary_wraps)]
const fn refuse_template(_key: &str) -> Result<()> {
    Ok(())
}
