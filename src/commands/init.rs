//! `dotman init`: create the repo or set its remote, or clone an existing repo and apply it.

use crate::bootstrap;
use crate::cli::{Decisions, ImportArgs, InitArgs};
use crate::commands::wizard::{self, Ask, Wizard};
use crate::commands::{Outcome, apply};
use crate::context::Context;
use crate::git::Git;
use crate::hooks::Policy;
use crate::import;
use crate::lock::Lock;
use crate::name;
use crate::output;
use crate::paths::private_temp_dir;
use crate::prompt;
use crate::remote;
use crate::repo_config::RepoConfig;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Runs `dotman init`.
///
/// # Errors
///
/// Fails when `--remote` or the source is not a usable URL, the lock is held, git or age fails, a
/// repo file cannot be written, or `--source` cannot be cloned or applied.
pub fn run(args: &InitArgs) -> Result<Outcome> {
    let ctx = Context::resolve("init")?;
    if let Some(source) = &args.source {
        return clone_and_apply(&ctx, source, args);
    }
    let dry_run = args.dry_run;
    if let Some(url) = &args.remote {
        check_url(url)?;
    }
    // Before the import probe, which would fail on such a repo with a raw git error.
    ctx.refuse_incomplete()?;
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
    let origin = if exists {
        ctx.git.remote_url("origin")?
    } else {
        None
    };

    let ask = match (prompt::interactive() && !dry_run, exists) {
        (false, _) => Ask::Nothing,
        (true, true) => Ask::Remote,
        (true, false) => Ask::Everything,
    };
    let bootstrap = !args.no_bootstrap && (!exists || args.refresh_bootstrap);
    let wizard = Wizard {
        ctx: &ctx,
        args,
        ask,
    };
    wizard.check_flags()?;
    let answers = wizard.new_repo(origin.as_deref())?;
    wizard.check(&answers, origin.as_deref(), dry_run)?;

    if dry_run {
        if !exists {
            output::info(&format!("would create the repo at {}", repo.display()));
        }
        if !config_file.exists() {
            output::info(&format!("would write {}", config_file.display()));
        }
        if bootstrap {
            bootstrap::write(repo, true)?;
        }
        wizard.describe(&answers);
        return Ok(Outcome::Done);
    }

    if exists {
        ctx.git.init()?;
    } else {
        init_into_place(&ctx)?;
    }
    if !config_file.exists() {
        RepoConfig::default().save(&config_file, false)?;
    }
    if !exists {
        output::success(&format!("created the repo at {}", repo.display()));
    }
    if bootstrap {
        bootstrap::write(repo, false)?;
    }
    wizard.apply(&answers)?;
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
            "a repo already exists at {}; `{} apply` applies it",
            repo.display(),
            name::bin()
        );
    }
    // Everything a flag can get wrong is refused before the clone, which would be left behind.
    Wizard {
        ctx,
        args,
        ask: Ask::Nothing,
    }
    .check_flags()?;
    let key = args
        .age_key_file
        .as_deref()
        .map(|path| wizard::read_key_file(ctx, path, args.force))
        .transpose()?;
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
        clone_into_place(ctx, source)?;
        output::success(&format!("cloned {source} into {}", repo.display()));
        remote::note_origin(ctx, false);
        let ask = if prompt::interactive() {
            Ask::Everything
        } else {
            Ask::Nothing
        };
        let wizard = Wizard { ctx, args, ask };
        let answers = wizard.existing_repo(key)?;
        let updated = wizard.apply(&answers)?;
        let ctx = updated.as_ref().unwrap_or(ctx);
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
    let wizard = Wizard {
        ctx: &clone,
        args,
        ask: Ask::Nothing,
    };
    let answers = wizard.existing_repo(key)?;
    wizard.check(&answers, None, true)?;
    wizard.describe(&answers);
    let planning = Context::new(clone.paths.clone(), wizard.machine_for(&answers), "init");
    let applied = apply::apply(&planning, &[], &opts)?;
    apply::finish(&planning, &applied, policy)
}

/// A repo built in a directory beside the repo path, so that moving it into place is one rename.
/// Dropping it removes whatever is left; a killed run leaves only this directory, never a half
/// repo at the repo path.
struct Staging {
    _dir: TempDir,
    git: Git,
}

impl Staging {
    fn new(ctx: &Context) -> Result<Self> {
        let repo = &ctx.paths.repo;
        let parent = repo
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", repo.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        let dir = private_temp_dir(".dotman-init-", parent)?;
        let git = Git::new(dir.path().join("repo"), "init");
        Ok(Self { _dir: dir, git })
    }

    fn path(&self) -> &Path {
        self.git.repo()
    }
}

/// Clones `source` and moves the clone into place only once its checkout is complete.
fn clone_into_place(ctx: &Context, source: &str) -> Result<()> {
    let staging = Staging::new(ctx)?;
    staging.git.clone_from(source)?;
    // An empty remote leaves HEAD unborn, with nothing to check out.
    let complete = staging.git.head()?.is_none() || staging.git.status()?.is_empty();
    ensure!(
        complete,
        "the checkout of {source} left files missing or changed; nothing was created at {}",
        ctx.paths.repo.display()
    );
    let repo = &ctx.paths.repo;
    fs::rename(staging.path(), repo)
        .with_context(|| format!("cannot move the clone to {}", repo.display()))
}

/// Creates `.git` with its metadata, then moves it into the repo path, which may already hold files.
fn init_into_place(ctx: &Context) -> Result<()> {
    let staging = Staging::new(ctx)?;
    staging.git.init()?;
    let repo = &ctx.paths.repo;
    fs::create_dir_all(repo).with_context(|| format!("cannot create {}", repo.display()))?;
    fs::rename(staging.path().join(".git"), repo.join(".git"))
        .with_context(|| format!("cannot move the new repo to {}", repo.display()))
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
                "  {} import {}    # {label}",
                name::bin(),
                shell_words::quote(&path.to_string_lossy())
            ));
        }
        if let Some(url) = &args.remote {
            output::info(&format!(
                "then run `{} init --remote {}`",
                name::bin(),
                shell_words::quote(url)
            ));
        }
        output::info("or start a fresh repo with:");
        let remote = args.remote.as_ref().map_or_else(String::new, |url| {
            format!(" --remote {}", shell_words::quote(url))
        });
        output::info(&format!("  {} init --no-import{remote}", name::bin()));
        return Ok(Some(Outcome::Attention));
    }
    let fresh = found.len().saturating_add(1);
    let question = format!(
        "Import which? 1-{}, {fresh} to start fresh, empty to do nothing:",
        found.len()
    );
    let Some(choice) = prompt::ask_parsed(&question, |answer| prompt::choice(answer, fresh))?
    else {
        output::info("nothing created");
        return Ok(Some(Outcome::Done));
    };
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
        no_bootstrap: args.no_bootstrap,
        dry_run: args.dry_run,
    };
    let _lock = Lock::acquire(&ctx.paths.lock_file(), args.dry_run)?;
    ctx.guard()?;
    let outcome = import::run(ctx, &import_args)?;
    if !args.dry_run {
        if let Some(url) = &args.remote {
            wizard::set_origin(ctx, url)?;
        }
        remote::note_origin(ctx, false);
    }
    Ok(Some(outcome))
}

// A leading `-` would be read by git as an option.
fn check_url(url: &str) -> Result<()> {
    wizard::usable_url(url).map(drop).map_err(|e| anyhow!(e))
}
