//! Local inspection and reviewed edits of the origin used by desktop sync.

use super::{Git, selected_network_url};
use crate::context::{Context, ReviewChanged};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    schema_version: u32,
    operation: Operation,
    repo_id: String,
    expected: Option<String>,
    change: Option<Change>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    SyncDestination,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    fetch: String,
    push: Option<String>,
}

#[derive(Serialize)]
struct Destination {
    branch: Option<String>,
    fetch: Vec<Option<String>>,
    push: Vec<Option<String>>,
    separate_push: bool,
    editable: bool,
    token: String,
}

fn safe_url(value: &str) -> bool {
    if value.len() > 2048 || crate::secrets::findings(value.as_bytes()).next().is_some() {
        return false;
    }
    if selected_network_url(value) {
        return true;
    }
    if let Some((host, path)) = value.strip_prefix("git@").and_then(|v| v.split_once(':')) {
        return !host.contains('/')
            && !host.contains('@')
            && !host.contains(':')
            && selected_network_url(&format!("ssh://git@{host}/{path}"));
    }
    let path = value.strip_prefix("file://").unwrap_or(value);
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() > 1
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._- ".contains(&b))
}

fn urls(git: &Git, key: &str, local: bool) -> Result<Vec<String>> {
    let mut args = vec!["config", "-z"];
    if local {
        args.extend(["--local", "--no-includes"]);
    }
    args.extend(["--get-all", key]);
    let out = git.probe(&args)?;
    if out.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    ensure!(out.status.success(), "could not inspect origin");
    Ok(out
        .stdout
        .strip_suffix(&[0])
        .unwrap_or_default()
        .split(|b| *b == 0)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect())
}

fn effective(git: &Git, push: bool) -> Result<Vec<String>> {
    let mut args = vec!["remote", "get-url", "--all"];
    if push {
        args.push("--push");
    }
    args.push("origin");
    let out = git.probe(&args)?;
    ensure!(out.status.success(), "origin could not be resolved");
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn check_routes(git: &Git, change: &Change) -> Result<()> {
    let out = git.probe(&[
        "config",
        "-z",
        "--get-regexp",
        r"^url\..*\.(insteadof|pushinsteadof)$",
    ])?;
    if out.status.code() == Some(1) {
        return Ok(());
    }
    ensure!(out.status.success(), "could not check Git URL rewrites");
    for record in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let split = record
            .iter()
            .position(|b| *b == b'\n')
            .context("invalid Git rewrite rule")?;
        let (key, value) = record.split_at(split);
        let prefix = value
            .strip_prefix(b"\n")
            .context("invalid Git rewrite prefix")?;
        let matching = if key.ends_with(b".pushinsteadof") {
            change.push.is_none() && change.fetch.as_bytes().starts_with(prefix)
        } else {
            change.fetch.as_bytes().starts_with(prefix)
                || change
                    .push
                    .as_ref()
                    .is_some_and(|url| url.as_bytes().starts_with(prefix))
        };
        ensure!(
            !matching,
            "Git rewrites this address. Enter its resolved destination."
        );
    }
    Ok(())
}

fn inspect(ctx: &Context, config: &[u8]) -> Result<Destination> {
    let git = &ctx.git;
    let fetch = urls(git, "remote.origin.url", false)?;
    let push = urls(git, "remote.origin.pushurl", false)?;
    let names = git.run(&["remote"])?;
    let exists = names.stdout.split(|b| *b == b'\n').any(|n| n == b"origin");
    let resolved = if exists {
        (effective(git, false)?, effective(git, true)?)
    } else {
        (vec![], vec![])
    };
    let direct = !exists
        || (!fetch.is_empty()
            && fetch == resolved.0
            && (if push.is_empty() { &fetch } else { &push }) == &resolved.1);
    let editable = direct
        && fetch.len() <= 1
        && push.len() <= 1
        && fetch == urls(git, "remote.origin.url", true)?
        && push == urls(git, "remote.origin.pushurl", true)?
        && fetch.iter().chain(&push).all(|s| safe_url(s));
    let token = git.hash_object(
        &serde_json::to_vec(&(
            ctx.repo_id()?,
            &ctx.paths.repo,
            git.hash_object(config, false)?,
            &fetch,
            &push,
            &resolved,
            editable,
        ))?,
        false,
    )?;
    let project = |raw: &[String], values: Vec<String>| {
        if raw.iter().any(|s| s.contains(['\r', '\n'])) {
            return vec![None];
        }
        values
            .into_iter()
            .map(|v| safe_url(&v).then_some(v))
            .collect()
    };
    Ok(Destination {
        branch: git.branch()?,
        fetch: project(&fetch, resolved.0.into_iter().take(1).collect()),
        push: project(if push.is_empty() { &fetch } else { &push }, resolved.1),
        separate_push: !push.is_empty(),
        editable,
        token,
    })
}

fn read_config(path: &Path) -> Result<(Vec<u8>, u32)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= 1_048_576,
        "unsupported Git configuration"
    );
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "Git configuration is too large");
    Ok((bytes, meta.mode() & 0o777))
}

fn replace(ctx: &Context, dir: &Path, expected: &str, change: &Change) -> Result<()> {
    let mut locked = tempfile::Builder::new()
        .prefix("config.lock")
        .rand_bytes(0)
        .tempfile_in(dir)?;
    let config = dir.join("config");
    let (bytes, mode) = read_config(&config)?;
    let before = inspect(ctx, &bytes)?;
    ensure!(before.token == expected, ReviewChanged);
    ensure!(
        before.editable,
        "advanced origin configuration is read-only"
    );
    check_routes(&ctx.git, change)?;
    // Git edits this private copy so fetch and push become visible together on the final rename.
    let mut scratch = tempfile::NamedTempFile::new()?;
    scratch.write_all(&bytes)?;
    let file = scratch
        .path()
        .to_str()
        .context("invalid configuration path")?;
    let set = |key: &str, value: &str| -> Result<()> {
        ensure!(
            ctx.git
                .probe(&["config", "--file", file, "--replace-all", key, value])?
                .status
                .success(),
            "could not update origin"
        );
        Ok(())
    };
    set("remote.origin.url", &change.fetch)?;
    if before.fetch.is_empty() {
        set("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")?;
    }
    if let Some(push) = &change.push {
        set("remote.origin.pushurl", push)?;
    } else if before.separate_push {
        ensure!(
            ctx.git
                .probe(&[
                    "config",
                    "--file",
                    file,
                    "--unset-all",
                    "remote.origin.pushurl"
                ])?
                .status
                .success(),
            "could not clear push override"
        );
    }
    std::io::copy(&mut File::open(scratch.path())?, &mut locked)?;
    locked
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    locked.as_file().sync_all()?;
    ensure!(read_config(&config)? == (bytes, mode), ReviewChanged);
    locked.persist(config)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn run(req: &Request) -> Result<()> {
    let Operation::SyncDestination = req.operation;
    ensure!(
        req.schema_version == 1 && (req.expected.is_none() || req.change.is_some()),
        "invalid destination request"
    );
    let ctx = Context::resolve("remedy")?;
    let guard = ctx.lock_for_review(req.expected.is_some())?;
    ensure!(ctx.repo_id()? == req.repo_id, ReviewChanged);
    let repo = File::open(&ctx.paths.repo)?;
    let fd = crate::sys::openat_dir(repo.as_fd(), c".git")?;
    // Keep the lock and the final rename in the directory we opened, even if its path is replaced.
    let dir = std::path::PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    ensure!(
        fs::symlink_metadata(dir.join("commondir"))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "shared Git configuration is unsupported"
    );
    if let Some(change) = &req.change {
        ensure!(
            safe_url(&change.fetch) && change.push.as_deref().is_none_or(safe_url),
            "enter a credential-free repository address"
        );
        if let Some(expected) = &req.expected {
            ensure!(guard.is_held(), "repository lock is unavailable");
            replace(&ctx, &dir, expected, change)?;
        } else {
            check_routes(&ctx.git, change)?;
        }
    }
    let view = inspect(&ctx, &read_config(&dir.join("config"))?.0)?;
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string(&view)?
    )?;
    Ok(())
}
