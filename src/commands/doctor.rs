//! `dot doctor`: reads the environment and the repo, writes nothing and never stops at the first problem.

use crate::VERSION;
use crate::bootstrap::{self, Found};
use crate::cli::DoctorArgs;
use crate::context::Context;
use crate::git::Git;
use crate::lock::Lock;
use crate::secrets::{self, Locked};
use crate::state::State;
use crate::sys;
use crate::system::{self, Verdict};
use crate::variant::{Flag, Selection};
use anyhow::{Context as _, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

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

/// Runs `dot doctor`. Returns false when any check failed. Takes no lock and skips the in-progress guard.
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
        writeln!(out, "{tag:<6}{}", check.line).context("cannot write to stdout")?;
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
        checks.extend(directories(ctx));
        checks.extend(system_roots(ctx));
        if repo_exists {
            if git_ok {
                checks.push(remote(&git, online));
            }
            checks.extend(repo_health(ctx));
            checks.extend(variants(ctx));
            checks.extend(encryption(ctx));
            checks.extend(system_files(ctx));
            checks.extend(install_script(ctx));
        } else {
            checks.push(fail(
                "repo",
                &format!("not found at {}", ctx.paths.repo.display()),
                "run dot init",
            ));
        }
    }
    checks.extend(warnings(ctx.as_ref(), &git, git_ok));
    if let Some(ctx) = &ctx {
        checks.push(backups(&ctx.paths.state_dir.join("backups")));
    }
    checks
}

fn identity(git: &Git) -> Check {
    match git.identity_configured() {
        Ok(true) => pass("git identity"),
        Ok(false) => fail(
            "git identity",
            "user.name or user.email is not set",
            "run git config --global user.name and user.email",
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
                "run dot init --remote <url>",
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

fn directories(ctx: &Context) -> Vec<Check> {
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
        } else {
            checks.push(pass(&format!("{name} {}", dir.display())));
        }
    }
    checks
}

fn repo_health(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();
    match Lock::acquire(&ctx.paths.lock_file(), true) {
        Ok(_lock) => checks.push(pass("lock free")),
        Err(e) => checks.push(fail("lock", &format!("{e:#}"), "wait for it to finish")),
    }
    let state_file = ctx.paths.state_file();
    match ctx.repo_id() {
        Err(e) => checks.push(fail("repo id", &format!("{e:#}"), "run dot init")),
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
    match ctx.git.operation() {
        Some(op) => checks.push(fail(
            "git operation",
            &format!("a {} is in progress", op.name()),
            "run dot git rebase --continue or dot git rebase --abort",
        )),
        None => checks.push(pass("no git operation in progress")),
    }
    checks
}

fn install_script(ctx: &Context) -> Option<Check> {
    match bootstrap::inspect(&ctx.paths.repo) {
        Err(e) => Some(warn("install.sh", &format!("{e:#}"))),
        Ok(Found::Missing) => None,
        Ok(Found::Edited) => Some(info(
            "install.sh",
            "edited by hand, so `dot init --refresh-bootstrap` leaves it as it is",
        )),
        Ok(Found::Unedited { version }) if bootstrap::is_older(&version) => Some(warn(
            "install.sh",
            &format!(
                "pins dot {version}, older than {VERSION}; run `dot init --refresh-bootstrap`"
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
    match ctx.age().key_reader() {
        Some(Ok(version)) => checks.push(pass(&format!("DOTMAN_AGE_KEY is read by age {version}"))),
        Some(Err(why)) => checks.push(fail(
            "DOTMAN_AGE_KEY",
            &why,
            "install age 1.1.0 or newer, or use DOTMAN_AGE_KEY_FILE or dot key import",
        )),
        None => {}
    }
    if let Ok(config) = ctx.repo_config()
        && secrets::recipient_set(&config.age.recipients).len() == 1
    {
        checks.push(warn(
            "age recipients",
            "only one; if its identity is lost, the encrypted files cannot be recovered. Add a second key: dot key init on another machine, or dot key import --add-recipient",
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

fn backups(dir: &Path) -> Check {
    let mut total: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&next) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&next) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    let mut value = total;
    let mut unit = 0_usize;
    while value >= 1024 && unit < 3 {
        value /= 1024;
        unit = unit.saturating_add(1);
    }
    let name = ["B", "KiB", "MiB", "GiB"].get(unit).copied().unwrap_or("B");
    info("backups", &format!("{value} {name} in {}", dir.display()))
}
