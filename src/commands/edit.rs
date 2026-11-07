//! `dot edit`: open the source of a managed file in the user's editor, then apply that one path.

use crate::cli::EditArgs;
use crate::commands::Outcome;
use crate::commands::apply::{self, Options};
use crate::context::Context;
use crate::hooks::Policy;
use crate::lock::Lock;
use crate::paths::derive_key;
use crate::proc;
use crate::variant::Flag;
use anyhow::{Context as _, Result, bail};
use std::fs;

/// Runs `dot edit`. The lock is taken only after the editor exits, so a long edit blocks nothing.
pub fn run(args: &EditArgs) -> Result<Outcome> {
    let ctx = Context::resolve("edit")?;
    ctx.guard()?;
    ctx.repo_id()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let key = derive_key(
        &args.path,
        &cwd,
        &ctx.paths.home,
        ctx.paths.home_real.as_deref(),
    )?;
    let sources = ctx.sources()?;
    if !sources.contains(&key) {
        bail!("{key} is not managed");
    }
    let source = sources.select(&key, ctx.identity()).require(&key)?;
    if sources.flag(&key, source) == Flag::Age {
        bail!("edit `~/{key}` directly, then `dot sync`");
    }
    let path = ctx.paths.repo.join(source);
    if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
        bail!("{source} is not a regular file, so there is nothing to open");
    }

    let before = fs::read(&path).with_context(|| format!("cannot read {source}"))?;
    let (program, rest) = editor_command()?;
    let status = proc::command(&program)
        .args(rest)
        .arg(&path)
        .status()
        .with_context(|| format!("cannot run the editor `{program}`"))?;
    if !status.success() {
        bail!("the editor exited with {status}; nothing was applied");
    }

    let _lock = Lock::acquire(&ctx.paths.lock_file(), false)?;
    ctx.guard()?;
    // Opening the editor to look must never replace a locally edited file, so drift is answered
    // only when the source changed.
    let after = fs::read(&path).with_context(|| format!("cannot read {source}"))?;
    let opts = Options {
        take_drift: before != after,
        ..Options::default()
    };
    let applied = apply::apply(&ctx, std::slice::from_ref(&args.path), &opts)?;
    apply::finish(&ctx, &applied, Policy::default())
}

/// `$VISUAL`, else `$EDITOR`, else `vi`, split like a shell would.
fn editor_command() -> Result<(String, Vec<String>)> {
    let command = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned());
    let mut words = shell_words::split(&command)
        .with_context(|| format!("cannot parse the editor command `{command}`"))?
        .into_iter();
    let Some(program) = words.next() else {
        bail!("the editor command `{command}` is empty");
    };
    Ok((program, words.collect()))
}
