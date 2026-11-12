//! `dotman doctor`: reads the environment and the repo, writes nothing and never stops at the first problem.

use crate::VERSION;
use crate::bootstrap::{self, Found};
use crate::cli::DoctorArgs;
use crate::commands::sync;
use crate::context::Context;
use crate::git::{Git, Operation, UnusableBranch};
use crate::hooks::{self, Standing};
use crate::import::{OriginArg, clone_arg, count};
use crate::lock::Lock;
use crate::name;
use crate::output;
use crate::proc;
use crate::secrets::{self, Locked};
use crate::state::State;
use crate::sys;
use crate::system::{self, Verdict};
use crate::variant::{Flag, Selection};
use anyhow::{Context as _, Result};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How long the version probe of an old binary on PATH may take before it is killed.
const VERSION_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Pass,
    Warn,
    Fail,
    Info,
}

struct Check {
    level: Level,
    line: String,
}

fn pass(what: &str) -> Check {
    Check {
        level: Level::Pass,
        line: what.to_owned(),
    }
}

fn warn(what: &str, why: &str) -> Check {
    Check {
        level: Level::Warn,
        line: format!("{what}: {why}"),
    }
}

fn fail(what: &str, why: &str, fix: &str) -> Check {
    Check {
        level: Level::Fail,
        line: format!("{what}: {why}; {fix}"),
    }
}

fn info(what: &str, detail: &str) -> Check {
    Check {
        level: Level::Info,
        line: format!("{what}: {detail}"),
    }
}

/// Runs `dotman doctor`. Returns false when any check failed. Takes no lock and skips the in-progress guard.
///
/// # Errors
///
/// Fails only when a result line cannot be written to stdout; failed checks are reported through
/// the returned `false`.
pub fn run(args: &DoctorArgs) -> Result<bool> {
    let checks = collect(args.online);
    let mut out = std::io::stdout().lock();
    for check in &checks {
        let tag = match check.level {
            Level::Pass => "pass",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Info => "info",
        };
        writeln!(out, "{tag:<6}{}", output::escape_line(&check.line))
            .context("cannot write to stdout")?;
    }
    Ok(checks.iter().all(|c| c.level != Level::Fail))
}

fn collect(online: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    let ctx = match Context::resolve("doctor") {
        Ok(ctx) => Some(ctx),
        Err(e) => {
            checks.push(fail(
                "paths",
                &format!("{e:#}"),
                "fix $HOME and the machine config",
            ));
            None
        }
    };
    let repo_exists = ctx
        .as_ref()
        .is_some_and(|c| c.paths.repo.join(".git").exists());
    // Outside a repo `git config` still reads the global files; a missing repo directory would make `-C` fail.
    let git = match &ctx {
        Some(c) if repo_exists => Git::new(c.paths.repo.clone(), "doctor"),
        Some(c) => Git::new(c.paths.home.clone(), "doctor"),
        None => Git::new(".", "doctor"),
    };

    let git_ok = match git.check_version() {
        Ok(()) => {
            checks.push(pass("git"));
            true
        }
        Err(e) => {
            checks.push(fail("git", &format!("{e:#}"), "install git 2.25 or newer"));
            false
        }
    };
    if git_ok {
        checks.push(identity(&git));
    }
    if let Some(ctx) = &ctx {
        let incomplete = repo_exists.then(|| incomplete(ctx)).flatten();
        checks.extend(directories(ctx, incomplete.is_none()));
        checks.extend(system_roots(ctx));
        if let Some(check) = incomplete {
            checks.push(check);
        } else if repo_exists {
            if git_ok {
                checks.push(remote(&git, online));
                checks.extend(branch(&git));
                checks.push(objects(ctx));
            }
            checks.extend(repo_health(ctx));
            checks.extend(hook_table(ctx));
            checks.extend(dir_approval(ctx));
            checks.extend(unlisted_ignores(ctx));
            checks.extend(variants(ctx));
            checks.extend(encryption(ctx));
            checks.extend(system_files(ctx));
            checks.extend(install_script(ctx));
        } else {
            checks.push(fail(
                "repo",
                &format!("not found at {}", ctx.paths.repo.display()),
                &format!("run {} init", name::bin()),
            ));
        }
    }
    checks.extend(warnings(ctx.as_ref(), &git, git_ok));
    if let Some(ctx) = &ctx {
        checks.extend(old_dotman(ctx));
        checks.extend(backups(ctx));
    }
    checks
}

/// A repo dir that a stopped `init` or clone left half made: no `HEAD`, no repo id, or a
/// `config.lock` that no running git holds.
fn incomplete(ctx: &Context) -> Option<Check> {
    let repo = &ctx.paths.repo;
    let git_dir = repo.join(".git");
    let lock = git_dir.join("config.lock");
    let lock_free = Lock::acquire(&ctx.paths.lock_file(), true).is_ok();
    let (missing, stale_lock) = if !git_dir.join("HEAD").is_file() {
        (".git/HEAD is missing", false)
    } else if !git_dir.join("dotman-id").is_file() {
        (".git/dotman-id is missing", false)
    } else if lock_free && lock.exists() {
        (
            ".git/config.lock is left from a git run that was stopped",
            true,
        )
    } else {
        return None;
    };
    let fix = if !holds_commits(ctx, &git_dir) {
        format!("remove {} and rerun {} init", repo.display(), name::bin())
    } else if stale_lock {
        format!("remove {}", lock.display())
    } else {
        format!(
            "it holds commits, so move {} aside rather than removing it, then rerun {} init",
            repo.display(),
            name::bin()
        )
    };
    Some(fail(
        "repo",
        &format!("{} is incomplete: {missing}", repo.display()),
        &fix,
    ))
}

/// Whether removing the repo could lose commits. git is asked, since only it reads every ref
/// format, reftable included; `--git-dir` keeps it from finding a repo above this one. Where git
/// cannot read the repo, as without `HEAD`, any object in it counts.
fn holds_commits(ctx: &Context, git_dir: &Path) -> bool {
    let mut cmd = ctx.git.command();
    cmd.arg("--git-dir")
        .arg(git_dir)
        .args(["for-each-ref", "--count=1", "--format=%(refname)"])
        .stdin(Stdio::null());
    match cmd.output() {
        Ok(out) if out.status.success() => {
            !out.stdout.is_empty() || !matches!(ctx.git.head(), Ok(None))
        }
        _ => has_objects(&git_dir.join("objects")),
    }
}

fn has_objects(objects: &Path) -> bool {
    let info = objects.join("info");
    let mut stack = vec![objects.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() && path != info => stack.push(path),
                Ok(kind) if !kind.is_dir() => return true,
                _ => {}
            }
        }
    }
    false
}

/// `git fsck --connectivity-only`: every object a ref reaches is present and readable. The repo
/// may hold commits that are nowhere else, so no step moves or removes it before a backup.
fn objects(ctx: &Context) -> Check {
    let mut cmd = ctx.git.command();
    cmd.args([
        "fsck",
        "--connectivity-only",
        "--no-dangling",
        "--no-progress",
    ])
    // English, so the broken refs can be read out of the message.
    .env("LC_ALL", "C")
    .stdin(Stdio::null());
    match cmd.output() {
        Ok(out) if out.status.success() => pass("repo objects"),
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout)
            );
            let lines: Vec<&str> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            let shown: Vec<&str> = lines.iter().take(3).copied().collect();
            let refs: Option<Vec<&str>> = lines.iter().map(|line| broken_ref(line)).collect();
            let fix = match refs {
                Some(refs) if !refs.is_empty() => format!(
                    "only refs that point at nothing are broken, and removing them loses no commit; a fetch brings a remote's back: `{}`",
                    remove_refs(&refs)
                ),
                _ => corrupt_repo_steps(ctx),
            };
            fail(
                "repo objects",
                &format!("git fsck failed: {}", shown.join("; ")),
                &fix,
            )
        }
        Err(e) => fail(
            "repo objects",
            &format!("cannot run git fsck: {e}"),
            "install git",
        ),
    }
}

/// The ref in fsck's `error: <ref>: invalid sha1 pointer <oid>`, as a dangling symref or a ref to
/// a missing object gives, unless it is a branch or `HEAD`: those name the user's history, which a
/// repair must not drop. A ref name may hold `;` or `$(`, which the step quotes, but a backtick
/// would close the step's span in the message and a control character would be shown escaped.
fn broken_ref(line: &str) -> Option<&str> {
    let (name, rest) = line.strip_prefix("error: ")?.split_once(": ")?;
    let usable = rest.starts_with("invalid sha1 pointer ")
        && name.starts_with("refs/")
        && !name.starts_with("refs/heads/")
        && !name.contains(|c: char| c.is_whitespace() || c.is_control() || c == '`');
    usable.then_some(name)
}

fn remove_refs(refs: &[&str]) -> String {
    let steps: Vec<String> = refs
        .iter()
        .map(|name| {
            format!(
                "{} git update-ref --no-deref -d {}",
                name::bin(),
                shell_words::quote(name)
            )
        })
        .collect();
    steps.join(" && ")
}

/// Objects are missing or damaged: back the repo up first, since it may hold commits that are
/// nowhere else, then clone again from origin when there is one.
fn corrupt_repo_steps(ctx: &Context) -> String {
    let repo = ctx.paths.repo.to_string_lossy();
    let (here, backup) = (
        shell_words::quote(&repo),
        shell_words::quote(&format!("{repo}.bak")).into_owned(),
    );
    let backup_step = format!(
        "back the repo up first with `cp -a {here} {backup}`, which keeps any commit that is only here"
    );
    let clone = ctx
        .git
        .remote_target("origin")
        .ok()
        .flatten()
        .map(|url| clone_arg(&url, &ctx.paths.home));
    let bin = name::bin();
    match clone {
        Some((_, OriginArg::Quoted(url))) => format!(
            "{backup_step}; then clone it again from origin: `rm -rf {here}` and `{bin} init {url}`"
        ),
        Some((_, OriginArg::Placeholder(slot, what))) => format!(
            "{backup_step}; then clone it again from origin: `rm -rf {here}` and `{bin} init {slot}`, given {what}"
        ),
        Some((shown, OriginArg::Unusable)) => format!(
            "{backup_step}; `{bin} git fsck` lists what is missing, and the origin URL {shown} is not a usable remote URL, so no command clones it"
        ),
        None => format!(
            "{backup_step}; `{bin} git fsck` lists what is missing, and with no origin there is no other copy to clone"
        ),
    }
}

/// Whether this machine approved the repo's `[hooks]` table.
fn hook_table(ctx: &Context) -> Option<Check> {
    let table = ctx.repo_config().ok()?.hooks;
    let state = ctx.state(&ctx.repo_id().ok()?).ok()?;
    let approve = format!(
        "read the [hooks] table in .dotman.toml, then run `{} apply --approve-hooks`",
        name::bin()
    );
    match hooks::standing(&table, &state) {
        Ok(Standing::Empty) => None,
        Ok(Standing::Approved) => Some(pass("hooks approved")),
        Ok(Standing::Unapproved) => Some(warn(
            "hooks",
            &format!("not approved on this machine, so none runs; {approve}"),
        )),
        Ok(Standing::Changed { old_format: false }) => Some(warn(
            "hooks",
            &format!("the table changed since this machine approved it, so none runs; {approve}"),
        )),
        Ok(Standing::Changed { old_format: true }) => Some(warn(
            "hooks",
            &format!(
                "the approval format changed, so the approval an older dotman saved no longer counts and none runs; {approve}"
            ),
        )),
        Err(e) => Some(Check {
            level: Level::Fail,
            line: format!("hooks: {e:#}"),
        }),
    }
}

/// Whether this machine approved every `[dirs]` entry. A warning, as for hooks: an unapproved
/// entry is a decision that waits, and each sync holds its new files and exits 3, but nothing is
/// broken.
fn dir_approval(ctx: &Context) -> Option<Check> {
    let dirs = ctx.repo_config().ok()?.dirs.paths;
    if dirs.is_empty() {
        return None;
    }
    let approved = ctx.state(&ctx.repo_id().ok()?).ok()?.dirs_approved();
    let unapproved: Vec<String> = dirs
        .iter()
        .filter(|dir| !approved.contains(*dir))
        .map(|dir| output::escape_line(dir).into_owned())
        .collect();
    let them = match unapproved.as_slice() {
        [] => return Some(pass("[dirs] entries approved")),
        [_] => "it",
        _ => "them",
    };
    Some(warn(
        "dirs",
        &format!(
            "{} not approved on this machine, so sync captures no new file under {them}; read [dirs] in .dotman.toml, then run `{} sync --approve-dirs`",
            unapproved.join(", "),
            name::bin()
        ),
    ))
}

/// The `[ignore]` patterns this machine still applies that `.dotman.toml` no longer lists.
/// Information only: keeping them is the safe default.
fn unlisted_ignores(ctx: &Context) -> Option<Check> {
    let listed = ctx.repo_config().ok()?.ignore.patterns;
    let unlisted = ctx
        .state(&ctx.repo_id().ok()?)
        .ok()?
        .unlisted_ignores(&listed);
    if unlisted.is_empty() {
        return None;
    }
    Some(info(
        "ignore",
        &format!(
            "this machine still applies {}, which .dotman.toml no longer lists, so sync captures nothing they match; `{} sync --drop-unlisted-ignores` stops applying them here",
            unlisted
                .iter()
                .map(|pattern| output::escape_line(pattern))
                .collect::<Vec<_>>()
                .join(", "),
            name::bin()
        ),
    ))
}

fn identity(git: &Git) -> Check {
    match git.identity_configured() {
        Ok(true) => pass("git identity"),
        Ok(false) => fail(
            "git identity",
            "user.name or user.email is not set",
            &sync::identity_steps(),
        ),
        Err(e) => fail("git identity", &format!("{e:#}"), "check your git config"),
    }
}

fn remote(git: &Git, online: bool) -> Check {
    match git.remote_url("origin") {
        Err(e) => return fail("remote", &format!("{e:#}"), "check .git/config in the repo"),
        Ok(None) => {
            return fail(
                "remote",
                "origin is not configured",
                &format!("run {} init --remote <url>", name::bin()),
            );
        }
        Ok(Some(_)) if !online => return pass("remote configured"),
        Ok(Some(_)) => {}
    }
    let mut cmd = git.command();
    // Credential helpers stay on; only the terminal prompt would hang a diagnostic run.
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .args(["ls-remote", "origin", "HEAD"])
        .stdin(Stdio::null());
    match cmd.output() {
        Ok(out) if out.status.success() => pass("remote reachable"),
        Ok(out) => fail(
            "remote",
            String::from_utf8_lossy(&out.stderr).trim(),
            "check the URL and your credentials",
        ),
        Err(e) => fail("remote", &format!("cannot run git: {e}"), "install git"),
    }
}

/// A branch name that sync and status refuse, or a detached `HEAD` that sync refuses. A rebase or
/// a bisect detaches `HEAD` too, and gets its own check.
fn branch(git: &Git) -> Option<Check> {
    let line = match git.branch() {
        Ok(None) => {
            let remote = matches!(git.remote_url("origin"), Ok(Some(_)));
            if !remote || git.operation().is_some() {
                return None;
            }
            sync::detached(git).ok()?
        }
        Err(err) if err.is::<UnusableBranch>() => err.to_string(),
        Ok(Some(_)) | Err(_) => return None,
    };
    Some(Check {
        level: Level::Fail,
        line: format!("branch: {line}"),
    })
}

/// Ownership and modes of the dotman directories; the repo passes only when `repo_whole`, so an
/// incomplete repo is never also reported as passing.
fn directories(ctx: &Context, repo_whole: bool) -> Vec<Check> {
    let euid = sys::geteuid();
    let config_dir = ctx.paths.config_file.parent().map(Path::to_path_buf);
    let dirs = [
        ("repo", Some(ctx.paths.repo.clone())),
        ("state dir", Some(ctx.paths.state_dir.clone())),
        ("config dir", config_dir),
    ];
    let mut checks = Vec::new();
    for (name, dir) in dirs {
        let Some(dir) = dir else { continue };
        let meta = match fs::metadata(&dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                checks.push(fail(
                    name,
                    &format!("cannot stat {}: {e}", dir.display()),
                    "check its permissions",
                ));
                continue;
            }
        };
        if meta.uid() != euid {
            checks.push(fail(
                name,
                &format!("{} is owned by uid {}, not you", dir.display(), meta.uid()),
                &format!("run chown -R $USER {}", dir.display()),
            ));
        } else if name == "state dir" && meta.mode() & 0o077 != 0 {
            checks.push(warn(
                name,
                &format!(
                    "{} has mode {:04o}, wider than 0700",
                    dir.display(),
                    meta.mode() & 0o7777
                ),
            ));
        } else if name != "repo" || repo_whole {
            checks.push(pass(&format!("{name} {}", dir.display())));
        }
    }
    checks
}

fn repo_health(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let lock_free = match Lock::acquire(&ctx.paths.lock_file(), true) {
        Ok(_lock) => {
            checks.push(pass("lock free"));
            true
        }
        Err(e) => {
            checks.push(fail("lock", &format!("{e:#}"), "wait for it to finish"));
            false
        }
    };
    // While a dotman runs, the git it started may hold one; with none running, sync refuses until
    // they are removed.
    if lock_free && let Err(e) = ctx.refuse_git_locks() {
        checks.push(Check {
            level: Level::Fail,
            line: format!("git locks: {e:#}"),
        });
    }
    let state_file = ctx.paths.state_file();
    match ctx.repo_id() {
        Err(e) => checks.push(fail(
            "repo id",
            &format!("{e:#}"),
            &format!("run {} init", name::bin()),
        )),
        Ok(id) => match State::read(&state_file) {
            Err(e) => checks.push(fail(
                "state",
                &format!("{e:#}"),
                &format!("delete {} to start fresh", state_file.display()),
            )),
            Ok(Some(state)) if state.repo_id != id => checks.push(warn(
                "state",
                &format!(
                    "{} belongs to another repo and will be reset",
                    state_file.display()
                ),
            )),
            Ok(_) => checks.push(pass("state")),
        },
    }
    checks.extend(lost_config(ctx));
    match ctx.git.operation() {
        Some(op) => checks.push(fail(
            "git operation",
            &format!("a {} is in progress", op.name()),
            &if op == Operation::Bisect {
                format!("run {} git bisect reset", name::bin())
            } else {
                format!(
                    "run {bin} git -c core.editor=true {op} --continue or {bin} git {op} --abort",
                    bin = name::bin(),
                    op = op.name()
                )
            },
        )),
        None => checks.push(pass("no git operation in progress")),
    }
    checks.push(attributes(ctx));
    checks
}

/// Lines the user added to `.git/info/attributes` are theirs; only a pinned line that is missing
/// or not last is flagged, and it fails when no command could write it.
fn attributes(ctx: &Context) -> Check {
    if ctx.git.attributes_current() {
        return pass("git attributes");
    }
    let path = ctx.paths.repo.join(".git/info/attributes");
    let what = format!(
        "{} does not end with the line dotman pins, so a tracked .gitattributes or your git config can still choose the merge driver",
        path.display()
    );
    let init = format!(
        "run `{} init`, which writes the line and changes nothing in $HOME",
        name::bin()
    );
    ctx.git.attributes_unwritable().map_or_else(
        || {
            warn(
                "git attributes",
                &format!("{what}; the next command that writes puts it last, or now {init}"),
            )
        },
        |why| {
            fail(
                "git attributes",
                &format!("{what}, and no command can write it: {why}"),
                &format!("fix that, then {init}"),
            )
        },
    )
}

/// A `.dotman.toml` that is missing or holds no settings reads as the defaults, so `[dirs]`,
/// `[modes]` and `[age]` would be lost, and the next sync would commit that; the last commit's
/// copy is the one to restore. The same rule as the guard's.
fn lost_config(ctx: &Context) -> Option<Check> {
    let file = ctx.repo_config_file();
    let how = match ctx.config_without_settings() {
        Ok(how) => how?,
        Err(e) => {
            return Some(fail(
                ".dotman.toml",
                &format!("{e:#}"),
                "check its permissions",
            ));
        }
    };
    let committed = ctx
        .committed_config_has_settings()
        .is_ok_and(|settings| settings == Some(true));
    committed.then(|| {
        fail(
            ".dotman.toml",
            &format!(
                "{} {how}, but the last commit has it, so its [dirs], [modes] and [age] would be lost",
                file.display()
            ),
            &format!(
                "restore it with `{} git checkout -- .dotman.toml`",
                name::bin()
            ),
        )
    })
}

fn install_script(ctx: &Context) -> Option<Check> {
    match bootstrap::inspect(&ctx.paths.repo) {
        Err(e) => Some(warn("install.sh", &format!("{e:#}"))),
        Ok(Found::Missing) => None,
        Ok(Found::Edited) => Some(info(
            "install.sh",
            &format!(
                "edited by hand, so `{} init --refresh-bootstrap` leaves it as it is",
                name::bin()
            ),
        )),
        Ok(Found::Unedited { version, .. }) if bootstrap::is_older(&version) => Some(warn(
            "install.sh",
            &format!(
                "pins dotman {version}, older than {VERSION}; run `{} init --refresh-bootstrap`",
                name::bin()
            ),
        )),
        Ok(Found::Unedited { current: false, .. }) => Some(warn(
            "install.sh",
            &format!(
                "written by another build of dotman {VERSION}; run `{} init --refresh-bootstrap`",
                name::bin()
            ),
        )),
        Ok(Found::Unedited { .. }) => Some(pass("install.sh")),
    }
}

// Ambiguity depends on this machine's identity; an invalid block is invalid everywhere.
fn variants(ctx: &Context) -> Vec<Check> {
    let sources = match ctx.sources() {
        Ok(sources) => sources,
        Err(e) => return vec![warn("variants", &format!("{e:#}"))],
    };
    let mut checks = Vec::new();
    for key in sources.keys() {
        match sources.select(key, ctx.identity()) {
            Selection::Ambiguous(tied) => checks.push(warn(
                "variants",
                &format!(
                    "{key}: {} are equally specific here and none is applied; remove one",
                    tied.join(", ")
                ),
            )),
            Selection::Invalid(problems) => {
                for (source, reason) in problems {
                    checks.push(warn(
                        "variants",
                        &format!("{source}: {reason}; {key} is not applied"),
                    ));
                }
            }
            Selection::None | Selection::Winner(_) => {}
        }
    }
    if checks.is_empty() {
        checks.push(pass("variants"));
    }
    checks
}

fn encryption(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let identities = [
        Some(&ctx.paths.age_identity),
        ctx.paths.age_key_file.as_ref(),
    ];
    for identity in identities.into_iter().flatten() {
        if let Ok(meta) = fs::metadata(identity)
            && meta.is_file()
            && meta.mode() & 0o044 != 0
        {
            checks.push(warn(
                "age identity",
                &format!(
                    "{} has mode {:04o}, so other users can read the key; run chmod 600 {}",
                    identity.display(),
                    meta.mode() & 0o7777,
                    identity.display()
                ),
            ));
        }
    }
    match ctx.age().key_reader() {
        Some(Ok(version)) => checks.push(pass(&format!("DOTMAN_AGE_KEY is read by age {version}"))),
        Some(Err(why)) => checks.push(fail(
            "DOTMAN_AGE_KEY",
            &why,
            &format!(
                "put the key in a file and point DOTMAN_AGE_KEY_FILE at it, or run {} key import",
                name::bin()
            ),
        )),
        None => {}
    }
    if let Ok(config) = ctx.repo_config()
        && secrets::recipient_set(&config.age.recipients).len() == 1
    {
        checks.push(warn(
            "age recipients",
            &format!(
                "only one; if its identity is lost, the encrypted files cannot be recovered. Add a second key: {bin} key init on another machine, or {bin} key import --add-recipient",
                bin = name::bin()
            ),
        ));
    }
    let Ok(sources) = ctx.sources() else {
        return checks;
    };
    let (mut total, mut locked) = (0_usize, Vec::new());
    for key in sources.keys() {
        let Selection::Winner(source) = sources.select(key, ctx.identity()) else {
            continue;
        };
        if sources.flag(key, source) != Flag::Age {
            continue;
        }
        total = total.saturating_add(1);
        if let Err(e) = ctx.age().decrypt(&ctx.paths.repo.join(source))
            && let Some(Locked(why)) = e.downcast_ref::<Locked>()
        {
            locked.push(format!("{key} ({why})"));
        }
    }
    if total == 0 {
        return checks;
    }
    if locked.is_empty() {
        checks.push(pass(&format!("encrypted files: {total}, none locked")));
    } else {
        checks.push(warn(
            "encrypted files",
            &format!("{} of {total} locked: {}", locked.len(), locked.join(", ")),
        ));
    }
    checks
}

fn system_roots(ctx: &Context) -> Vec<Check> {
    let scope = ctx.system_scope();
    let mut checks = Vec::new();
    for root in scope.roots() {
        match root.problem {
            Some(problem) => checks.push(fail(
                "system roots",
                &format!("{} {problem}", root.shown),
                "remove it from [system] roots in the machine config",
            )),
            None if !root.exists => checks.push(warn(
                "system roots",
                &format!("{} does not exist", root.shown),
            )),
            None => {}
        }
    }
    if checks.is_empty() && !scope.roots().is_empty() {
        let shown: Vec<&str> = scope.roots().iter().map(|r| r.shown.as_str()).collect();
        checks.push(pass(&format!("system roots {}", shown.join(", "))));
    }
    checks
}

// System files are read as the user; a leftover beside one means the package manager and the repo disagree.
fn system_files(ctx: &Context) -> Vec<Check> {
    let (Ok(config), Ok(id)) = (ctx.repo_config(), ctx.repo_id()) else {
        return Vec::new();
    };
    let Ok(state) = ctx.state(&id) else {
        return Vec::new();
    };
    let found = match system::evaluate(ctx, &config, &state) {
        Ok(found) => found,
        Err(e) => return vec![warn("system files", &format!("{e:#}"))],
    };
    let scope = ctx.system_scope();
    let mut checks = Vec::new();
    let (mut total, mut off) = (0_usize, 0_usize);
    for item in found {
        total = total.saturating_add(1);
        if item.off {
            off = off.saturating_add(1);
        }
        match item.verdict {
            Verdict::Refused(why) => checks.push(warn("system files", &why)),
            Verdict::Invalid(problems) => {
                for (source, reason) in problems {
                    checks.push(warn(
                        "system files",
                        &format!("{source}: {reason}; {} is not applied", item.key),
                    ));
                }
            }
            Verdict::Ambiguous(tied) => checks.push(warn(
                "system files",
                &format!(
                    "{}: {} are equally specific here",
                    item.key,
                    tied.join(", ")
                ),
            )),
            Verdict::Unreadable(why) if !item.off => {
                checks.push(warn("system files", &format!("{}: {why}", item.key)));
            }
            Verdict::Row { .. } if !item.off => {
                let siblings = scope.siblings(&item.key);
                if !siblings.is_empty() {
                    checks.push(warn(
                        "system files",
                        &system::sibling_note(&item.key, &siblings),
                    ));
                }
            }
            Verdict::Unreadable(_) | Verdict::Row { .. } => {}
        }
    }
    if off > 0 {
        checks.push(info(
            "system files",
            &format!("{off} of {total} are outside [system] roots and stay off"),
        ));
    }
    checks
}

fn warnings(ctx: Option<&Context>, git: &Git, git_ok: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    let home = ctx
        .map(|c| c.paths.home.clone())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if let (Some(xdg), Some(home)) = (
        std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()),
        home,
    ) && Path::new(&xdg) != home.join(".config")
    {
        checks.push(warn(
            "XDG_CONFIG_HOME",
            &format!(
                "is {}, not $HOME/.config; make sure every shell and service sets it",
                Path::new(&xdg).display()
            ),
        ));
    }
    if git_ok {
        let mut cmd = git.command();
        cmd.args(["config", "--type=bool", "--get", "commit.gpgSign"]);
        let signs = cmd.output().is_ok_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true"
        });
        if signs {
            checks.push(warn(
                "commit.gpgSign",
                "is true; a non-interactive sync may prompt for a passphrase",
            ));
        }
    }
    if std::env::var_os("DOTMAN_SYSTEM_ROOT").is_some() {
        checks.push(warn(
            "DOTMAN_SYSTEM_ROOT",
            "is set; it exists for tests only",
        ));
    }
    if std::env::var_os("DOTMAN_NIX_STORE").is_some() {
        checks.push(warn(
            "DOTMAN_NIX_STORE",
            "is set; it exists for tests only, and on a real machine links into /nix/store are always report-only",
        ));
    }
    checks
}

/// The store and the binary of dotman 0.0.1, which this dotman replaces.
fn old_dotman(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    let store = ctx.paths.home.join(".dotman");
    let legacy = store.join("index.bin").is_file() || store.join("commits").is_dir();
    if legacy && store != ctx.paths.repo {
        let shown = store.to_string_lossy();
        // Only named, never run: whether the import brought everything is the user's to check.
        let step = if ctx.paths.repo.join(".git").exists() {
            format!(
                "once you have checked that the repo at {} holds what you imported from it, retire it, for example with `mv {} {}`",
                ctx.paths.repo.display(),
                shell_words::quote(&shown),
                shell_words::quote(&format!("{shown}.old"))
            )
        } else {
            format!(
                "run `{} import {}` to bring its files into the repo",
                name::bin(),
                shell_words::quote(&shown)
            )
        };
        checks.push(warn(
            "legacy store",
            &format!("{shown} is the store of dotman 0.0.1; {step}"),
        ));
    }
    if let Some(dot) = proc::find_program("dot")
        && version_of(&dot).is_some_and(|text| is_old_version(&text))
    {
        checks.push(warn(
            "dot",
            &format!(
                "{} is dotman 0.0.1, which reads another store; run `cargo uninstall dotman` to remove it",
                dot.display()
            ),
        ));
    }
    checks
}

/// What `program --version` prints on stdout, or `None` when it fails or takes too long. Stdout
/// goes to an unnamed temp file rather than a pipe: a pipe stays open while any process it forked
/// lives, and reading it would wait for them all.
fn version_of(program: &Path) -> Option<String> {
    let mut out = tempfile::tempfile().ok()?;
    let mut child = proc::command(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out.try_clone().ok()?))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if started.elapsed() < VERSION_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(Some(_)) => return None,
            _ => {
                // It may have exited since the poll, which makes the kill fail; the wait reaps it.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    out.seek(SeekFrom::Start(0)).ok()?;
    let mut text = String::new();
    out.take(4096).read_to_string(&mut text).ok()?;
    Some(text)
}

// dotman 0.0.1 named its binary dot; the alias of this dotman prints this version.
fn is_old_version(text: &str) -> bool {
    let words: Vec<&str> = text
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    matches!(words.as_slice(), ["dot" | "dotman", version] if version.starts_with("0.0."))
}

fn backups(ctx: &Context) -> Vec<Check> {
    let dir = ctx.paths.state_dir.join("backups");
    let plaintext = plaintext_keys(ctx);
    let (mut total, mut files, mut runs, mut decrypted) = (0_u64, 0_usize, 0_usize, 0_usize);
    for run in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let run = run.path();
        if !fs::symlink_metadata(&run).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        runs = runs.saturating_add(1);
        let mut stack = vec![run.clone()];
        while let Some(next) = stack.pop() {
            let Ok(meta) = fs::symlink_metadata(&next) else {
                continue;
            };
            if meta.is_dir() {
                if let Ok(entries) = fs::read_dir(&next) {
                    stack.extend(entries.flatten().map(|e| e.path()));
                }
                continue;
            }
            total = total.saturating_add(meta.len());
            files = files.saturating_add(1);
            let key = next.strip_prefix(&run).ok().and_then(Path::to_str);
            if key.is_some_and(|key| plaintext.contains(key)) {
                decrypted = decrypted.saturating_add(1);
            }
        }
    }
    let mut checks = vec![info(
        "backups",
        &format!(
            "{} in {} from {}, in {}",
            size(total),
            count(files, "file"),
            count(runs, "run"),
            dir.display()
        ),
    )];
    if decrypted > 0 {
        checks.push(info(
            "backups",
            &format!(
                "{decrypted} of them copy an encrypted file or an age identity, and hold it in plaintext; remove the runs you no longer need"
            ),
        ));
    }
    checks
}

/// The keys whose backups hold plaintext: encrypted files as `$HOME` has them, and the identity
/// backups that `key import --force` keeps.
fn plaintext_keys(ctx: &Context) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    if let Some(name) = ctx.paths.age_identity.file_name() {
        keys.insert(format!(".dotman/{}", name.to_string_lossy()));
    }
    let Ok(sources) = ctx.sources() else {
        return keys;
    };
    for key in sources.keys() {
        if let Selection::Winner(source) = sources.select(key, ctx.identity())
            && sources.flag(key, source) == Flag::Age
        {
            keys.insert(key.to_owned());
        }
    }
    keys
}

fn size(bytes: u64) -> String {
    let mut value = bytes;
    let mut unit = 0_usize;
    while value >= 1024 && unit < 3 {
        value /= 1024;
        unit = unit.saturating_add(1);
    }
    let name = ["B", "KiB", "MiB", "GiB"].get(unit).copied().unwrap_or("B");
    format!("{value} {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ref name can hold what a shell runs; the step quotes it, so a shell reads the name back.
    /// A backtick or a control character cannot be shown in the step, so no step names it.
    #[test]
    fn a_broken_ref_is_quoted_in_the_step_or_not_named() {
        let pointer = format!("invalid sha1 pointer {}", "1".repeat(40));
        let name = "refs/remotes/x;touch${IFS}PWNED$(id)";
        assert_eq!(broken_ref(&format!("error: {name}: {pointer}")), Some(name));
        let words = shell_words::split(&remove_refs(&[name])).unwrap();
        assert_eq!(
            words,
            [name::bin(), "git", "update-ref", "--no-deref", "-d", name]
        );
        for name in ["refs/remotes/x`id`", "refs/remotes/x\u{1b}[2J"] {
            assert_eq!(broken_ref(&format!("error: {name}: {pointer}")), None);
        }
    }
}
