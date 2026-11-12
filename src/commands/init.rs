//! `dotman init`: create the repo or set its remote, or clone an existing repo and apply it.

use crate::bootstrap;
use crate::cli::{Decisions, ImportArgs, InitArgs};
use crate::commands::wizard::{self, Ask, Wizard};
use crate::commands::{Outcome, apply};
use crate::context::{Context, is_real_dir};
use crate::git::{Git, local_path_remote};
use crate::hooks::Policy;
use crate::import;
use crate::name;
use crate::output;
use crate::paths::private_temp_dir;
use crate::prompt;
use crate::remote;
use crate::repo_config::RepoConfig;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
    let _lock = ctx.lock(dry_run)?;
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
    if exists {
        output::info(&format!("the repo at {} is already set up", repo.display()));
    } else {
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
    let shown = crate::git::redact_credentials(source);
    let dry_run = args.dry_run;
    let _lock = ctx.lock(dry_run)?;
    let repo = &ctx.paths.repo;
    // A repo already there that this command cloned from `source` is one a run stopped before it
    // set up this machine, which this run finishes; any other repo is left alone.
    let place = if repo.join(".git").exists() {
        refuse_another_repo(ctx, source)?;
        None
    } else {
        // Before the clone, so a dry run refuses as the real run does and nothing is downloaded
        // first.
        Some(Place::of(repo)?)
    };
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
        if let Some(place) = &place {
            clone_into_place(ctx, place, source)?;
            output::success(&format!("cloned {shown} into {}", repo.display()));
        } else {
            output::info(&format!(
                "{} already holds the clone of {shown}; setting up this machine",
                repo.display()
            ));
        }
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
    let (_tmp, fresh);
    let clone = if place.is_some() {
        let tmp = private_temp_dir("dotman-init-", &std::env::temp_dir())?;
        let paths = ctx.paths.with_clone(tmp.path().join("repo"));
        _tmp = tmp;
        fresh = Context::new(paths, ctx.machine.clone(), "init");
        fresh.git.clone_from(source)?;
        output::info(&format!("would clone {shown} into {}", repo.display()));
        &fresh
    } else {
        ctx
    };
    let wizard = Wizard {
        ctx: clone,
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

/// Fails unless the repo at the repo path is a dotman repo whose `origin` is `source`.
fn refuse_another_repo(ctx: &Context, source: &str) -> Result<()> {
    let ours = ctx.paths.repo.join(".git/dotman-id").is_file()
        && ctx
            .git
            .remote_url("origin")?
            .is_some_and(|origin| same_source(&origin, &ctx.paths.repo, source));
    ensure!(
        ours,
        "a repo already exists at {}; `{} apply` applies it",
        ctx.paths.repo.display(),
        name::bin()
    );
    Ok(())
}

/// Whether `origin`, the URL stored in `repo`, is what a clone of `source` stores. A URL matches
/// only itself. A local path names a directory: git resolves a relative origin against `repo`,
/// and `source` was typed in the working directory. They match when the origin is the absolute
/// form git stores for `source`, or both resolve to the same directory. A path starting with `~`,
/// which git expands, matches only itself.
fn same_source(origin: &str, repo: &Path, source: &str) -> bool {
    let local = |url: &str| local_path_remote(url) && !url.starts_with('~');
    if !local(origin) || !local(source) {
        return origin == source;
    }
    let origin = repo.join(origin);
    let Ok(source) = std::path::absolute(source) else {
        return false;
    };
    origin == source
        || matches!(
            (fs::canonicalize(&origin), fs::canonicalize(&source)),
            (Ok(origin), Ok(source)) if origin == source
        )
}

const STAGING_PREFIX: &str = ".dotman-init-";

/// Where a clone goes: the repo path, or the directory a symlink there points at, which a rename
/// cannot replace through the link.
struct Place {
    target: PathBuf,
    /// Mode, owner and group of an empty directory already at `target`, which the repo keeps.
    existing: Option<(u32, u32, u32)>,
}

impl Place {
    /// Fails when something other than an empty directory is at the target.
    fn of(repo: &Path) -> Result<Self> {
        let target = match fs::symlink_metadata(repo) {
            Ok(meta) if meta.file_type().is_symlink() => {
                fs::canonicalize(repo).with_context(|| {
                    format!(
                        "{} is a symlink to a directory that does not exist; create it first",
                        repo.display()
                    )
                })?
            }
            _ => repo.to_path_buf(),
        };
        let existing = match fs::symlink_metadata(&target) {
            Ok(meta) if !meta.is_dir() => {
                bail!("{} exists and is not a directory", target.display())
            }
            Ok(meta) => {
                let mut entries = fs::read_dir(&target)
                    .with_context(|| format!("cannot read {}", target.display()))?;
                if entries.next().is_some() {
                    bail!(
                        "{} exists and is not empty, so nothing was cloned; move it aside, or set another repo path in the machine config",
                        target.display()
                    );
                }
                Some((meta.mode() & 0o7777, meta.uid(), meta.gid()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot stat {}", target.display())),
        };
        Ok(Self { target, existing })
    }
}

/// A repo built in a directory beside where it goes, on the same filesystem, so that moving it into
/// place is one rename. Dropping it removes whatever is left; a killed run leaves only this
/// directory, never a half repo at the repo path, and a later init removes it.
struct Staging {
    _dir: TempDir,
    git: Git,
}

impl Staging {
    fn new(target: &Path) -> Result<Self> {
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        sweep_staging(parent);
        // The PID tells a killed run's directory from a live one's.
        let prefix = format!("{STAGING_PREFIX}{}-", std::process::id());
        let dir = private_temp_dir(&prefix, parent)?;
        let git = Git::new(dir.path().join("repo"), "init");
        Ok(Self { _dir: dir, git })
    }

    fn path(&self) -> &Path {
        self.git.repo()
    }
}

/// Removes the staging directories that killed runs left in `parent`.
fn sweep_staging(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if crate::context::is_leftover(&path, STAGING_PREFIX)
            && is_real_dir(&path)
            && let Err(e) = fs::remove_dir_all(&path)
        {
            output::warning(&format!("cannot remove {}: {e}", path.display()));
        }
    }
}

/// Clones `source` and moves the clone into place only once its checkout is complete.
fn clone_into_place(ctx: &Context, place: &Place, source: &str) -> Result<()> {
    let staging = Staging::new(&place.target)?;
    staging.git.clone_from(source)?;
    // An empty remote leaves HEAD unborn, with nothing to check out.
    let complete = staging.git.head()?.is_none() || staging.git.status()?.is_empty();
    ensure!(
        complete,
        "the checkout of {source} left files missing or changed; nothing was created at {}",
        ctx.paths.repo.display()
    );
    let target = &place.target;
    if let Some((mode, uid, gid)) = place.existing {
        let staged = staging.path();
        let meta = fs::symlink_metadata(staged)
            .with_context(|| format!("cannot stat {}", staged.display()))?;
        if (meta.uid(), meta.gid()) != (uid, gid) {
            std::os::unix::fs::chown(staged, Some(uid), Some(gid)).with_context(|| {
                format!(
                    "cannot give the clone the owner and group of {}",
                    target.display()
                )
            })?;
        }
        fs::set_permissions(staged, fs::Permissions::from_mode(mode))
            .with_context(|| format!("cannot set the mode of {}", staged.display()))?;
    }
    // rename(2) replaces an empty directory in one step.
    fs::rename(staging.path(), target)
        .with_context(|| format!("cannot move the clone to {}", target.display()))
}

/// Creates `.git` with its metadata, then moves it into the repo path, which may already hold files.
fn init_into_place(ctx: &Context) -> Result<()> {
    let target = fs::canonicalize(&ctx.paths.repo).unwrap_or_else(|_| ctx.paths.repo.clone());
    let staging = Staging::new(&target)?;
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
        let url = args.remote.as_deref().map(wizard::origin_url).transpose()?;
        if let Some(url) = &url {
            output::info(&format!(
                "then run `{} init --remote {}`",
                name::bin(),
                shell_words::quote(url)
            ));
        }
        output::info("or start a fresh repo with:");
        let remote = url.as_ref().map_or_else(String::new, |url| {
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
        keep_history: false,
        yes: false,
        no_bootstrap: args.no_bootstrap,
        dry_run: args.dry_run,
    };
    let _lock = ctx.lock(args.dry_run)?;
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
