//! `dotman edit`: open the source of a managed file in the user's editor, then apply that one path.

use crate::cli::EditArgs;
use crate::commands::Outcome;
use crate::commands::add::pasteable;
use crate::commands::apply::{self, Options};
use crate::context::Context;
use crate::engine::write::{back_up_bytes, backup_run_dir};
use crate::hooks::Policy;
use crate::name;
use crate::output;
use crate::paths::derive_key;
use crate::proc;
use crate::variant::Flag;
use anyhow::{Context as _, Result, anyhow, bail};
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// Runs `dotman edit`. The lock is taken only after the editor exits, so a long edit blocks nothing;
/// a run that changed the state meanwhile makes it apply nothing.
///
/// # Errors
///
/// Fails when the path is not managed, is age-encrypted, or is not a regular file, when no editor
/// can be started or it exits non-zero, when the lock is held after the editor exits, or when
/// another run changed the state while the editor was open.
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
        bail!(
            "edit `~/{key}` directly, then `{} sync`",
            crate::name::bin()
        );
    }
    let path = ctx.paths.repo.join(source);
    if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
        bail!("{source} is not a regular file, so there is nothing to open");
    }

    let before = fs::read(&path).with_context(|| format!("cannot read {source}"))?;
    let state_file = ctx.paths.state_file();
    let state_before = read_state(&state_file)?;
    let head_before = ctx.git.head()?;
    let (program, rest) = editor_command()?;
    if proc::find_program(&program).is_none() {
        bail!(
            "no editor found: `{program}` is not on PATH; set $VISUAL or $EDITOR to the editor to use"
        );
    }
    let status = proc::command(&program)
        .args(rest)
        .arg(&path)
        .status()
        .with_context(|| format!("cannot run the editor `{program}`"))?;
    if !status.success() {
        bail!("the editor exited with {status}; nothing was applied");
    }

    let _lock = ctx.lock(false)?;
    let after = fs::read(&path).with_context(|| format!("cannot read {source}"))?;
    // A sync or apply that ran meanwhile may have written this key with H matching what it saw;
    // applying on top would compare against a state the edit never saw.
    if read_state(&state_file)? != state_before {
        return Err(changed_meanwhile(
            &ctx,
            &key,
            source,
            &after,
            head_before.as_deref(),
        )?);
    }
    // Opening the editor to look must never replace a locally edited file, so drift is answered
    // only when the source changed.
    if before == after {
        output::info(&format!("{source} is unchanged"));
    }
    let opts = Options {
        take_drift: before != after,
        ..Options::default()
    };
    let applied = apply::apply(&ctx, std::slice::from_ref(&args.path), &opts)?;
    apply::finish(&ctx, &applied, Policy::default())
}

/// Why nothing was applied after another run changed the state while the editor was open. The
/// edit is saved under the backups first. When HEAD moved and brought another version of
/// `source`, an editor that wrote back what it had loaded has reverted that version, and the
/// next sync would commit the revert, so the repo copy is put back to HEAD's.
fn changed_meanwhile(
    ctx: &Context,
    key: &str,
    source: &str,
    edited: &[u8],
    head_before: Option<&str>,
) -> Result<anyhow::Error> {
    let bin = name::bin();
    let key = pasteable(&ctx.paths.home, key);
    let path = ctx.paths.repo.join(source);
    let run = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
    let saved = back_up_bytes(&run, &format!(".dotman/edit/{source}"), edited)?;
    let shown = |p: &Path| shell_words::quote(&p.to_string_lossy()).into_owned();
    let head = ctx.git.head()?;
    let at = |rev: Option<&str>| -> Result<Option<Vec<u8>>> {
        match rev {
            Some(rev) if ctx.git.tree_mode(rev, source)?.is_some() => {
                Ok(Some(ctx.git.show_blob(rev, source)?))
            }
            _ => Ok(None),
        }
    };
    let short = |rev: Option<&str>| {
        rev.map_or("no commit", |rev| rev.get(..12).unwrap_or(rev))
            .to_owned()
    };
    if head.as_deref() != head_before && at(head_before)? != at(head.as_deref())? {
        let restored = match head.as_deref() {
            Some(rev) if at(Some(rev))?.is_some() => {
                ctx.git.checkout_path(rev, Path::new(source))?;
                format!("{} holds the new version again", shown(&path))
            }
            _ => format!("the new HEAD has no {source}"),
        };
        return Ok(anyhow!(
            "{source} changed while the editor was open: another {bin} run moved HEAD from {} to {} and brought a new version of it, so nothing was applied. Your edit is saved in {}, and {restored}. Compare them with `diff -u {} {}`, then make your change again with `{bin} edit {key}`",
            short(head_before),
            short(head.as_deref()),
            shown(&saved),
            shown(&path),
            shown(&saved)
        ));
    }
    Ok(anyhow!(
        "another {bin} run changed the state while the editor was open, so nothing was applied. HEAD did not bring another version of {source}, so your edit is still in {}, and a copy is in {}; run `{bin} status` to see where {key} stands, then `{bin} apply {key}` applies it",
        shown(&path),
        shown(&saved)
    ))
}

/// The state file's bytes; `None` when there is none.
fn read_state(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
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
