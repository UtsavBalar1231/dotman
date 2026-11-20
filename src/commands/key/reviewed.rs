use crate::commands::Outcome;
use crate::context::{Context, TEMP_PREFIX};
use crate::decisions::InputPath;
use crate::lock::Lock;
use crate::output::json;
use crate::repo_config::RepoConfig;
use crate::sys;
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

mod settings;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    repo_id: String,
    home: InputPath,
    repo: InputPath,
    config: InputPath,
    identity: InputPath,
    config_observation: Option<Observation>,
    parent_dev: u64,
    parent_ino: u64,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Create,
    GrantPreflight,
    Import {
        source: InputPath,
        source_observation: Observation,
    },
    Grant {
        identity_observation: Observation,
        repo_config_observation: Observation,
        repo_config_digest: String,
        repo_dev: u64,
        repo_ino: u64,
        recovery_parent: InputPath,
        recovery_parent_dev: u64,
        recovery_parent_ino: u64,
        recipients: Vec<String>,
    },
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Observation {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

#[derive(Serialize)]
struct Receipt {
    record: &'static str,
    recipients: Vec<String>,
}

#[derive(Serialize)]
struct GrantPreflight<'a> {
    record: &'static str,
    schema_version: u32,
    repo_id: &'a str,
    home: &'a InputPath,
    repo: &'a InputPath,
    config: &'a InputPath,
    identity: &'a InputPath,
    config_observation: &'a Option<Observation>,
    parent_dev: u64,
    parent_ino: u64,
    repo_dev: u64,
    repo_ino: u64,
    identity_observation: Observation,
    repo_config_observation: Observation,
    repo_config_digest: String,
    recipients: Vec<String>,
    recipient_present: bool,
    backup_consequence: &'static str,
    recovery_parent: json::PathValue,
    recovery_parent_dev: u64,
    recovery_parent_ino: u64,
}

fn observe(path: &Path) -> Result<Option<Observation>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(meta.is_file(), "machine config is not a regular file");
    Ok(Some(observation(&meta)))
}

fn observation(meta: &fs::Metadata) -> Observation {
    Observation {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        ctime: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    }
}

fn source(path: &Path, expected: &Observation) -> Result<Vec<u8>> {
    ensure!(path.is_absolute(), "identity source must be absolute");
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid identity source"))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("invalid identity source"))?;
    let dir = open_dir(parent).map_err(|_| anyhow!("cannot open identity source parent"))?;
    let bytes = source_at(&dir, name, expected)?;
    let again = open_dir(parent).map_err(|_| anyhow!("identity source parent changed"))?;
    let first = dir.metadata()?;
    let last = again.metadata()?;
    ensure!(
        (first.dev(), first.ino()) == (last.dev(), last.ino()),
        "identity source parent changed"
    );
    Ok(bytes)
}

fn source_at(dir: &File, name: &std::ffi::OsStr, expected: &Observation) -> Result<Vec<u8>> {
    let path = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        dir.as_raw_fd()
    ))
    .join(name);
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|_| anyhow!("cannot open identity source"))?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == sys::geteuid()
            && meta.mode().trailing_zeros() >= 6
            && meta.nlink() == 1
            && observation(&meta) == *expected
            && observe(&path)?.as_ref() == Some(expected),
        "identity source changed or is not private"
    );
    let mut bytes = Vec::new();
    (&mut file).take(65_537).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 65_536
            && observation(&file.metadata()?) == *expected
            && observe(&path)?.as_ref() == Some(expected),
        "identity source changed or is too large"
    );
    native(&bytes)?;
    Ok(bytes)
}

fn native(bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("identity is not text"))?;
    let mut keys = 0_u32;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        ensure!(line.starts_with("AGE-SECRET-KEY-1"), "unsupported identity");
        keys = keys.saturating_add(1);
    }
    ensure!(keys == 1, "expected one native identity");
    Ok(())
}

fn open_dir(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "identity parent must be absolute");
    let mut dir = File::open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(part) => {
                let name = CString::new(part.as_bytes())?;
                dir = File::from(sys::openat_dir(dir.as_fd(), &name)?);
            }
            _ => bail!("identity parent must be literal"),
        }
    }
    Ok(dir)
}

fn parent(ctx: &Context, request: &Request) -> Result<File> {
    let path = ctx
        .paths
        .age_identity
        .parent()
        .ok_or_else(|| anyhow!("identity has no parent"))?;
    let dir = open_dir(path)?;
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid()
            && meta.mode().trailing_zeros() >= 6
            && (meta.dev(), meta.ino()) == (request.parent_dev, request.parent_ino),
        "identity parent changed or is not private"
    );
    Ok(dir)
}

fn destination(ctx: &Context, dir: &File) -> Result<()> {
    let leaf = ctx
        .paths
        .age_identity
        .file_name()
        .ok_or_else(|| anyhow!("identity has no name"))?;
    let parent_meta = dir.metadata()?;
    let config = ctx.repo_config()?;
    let state = ctx.state(&ctx.repo_id()?)?;
    let managed = ctx.managed()?;
    let index = ctx.git.index_paths()?;
    let groups =
        crate::variant::Sources::group(managed.keys().chain(index.iter()).map(String::as_str));
    let mut paths = vec![
        ctx.paths.repo.clone(),
        ctx.paths.state_dir.clone(),
        ctx.paths.config_file.clone(),
    ];
    if let Some(path) = &ctx.paths.age_key_file {
        paths.push(path.clone());
    }
    paths.extend(groups.keys().map(|key| ctx.paths.home.join(key)));
    paths.extend(state.files.keys().map(|key| ctx.paths.home.join(key)));
    let mut trees = vec![ctx.paths.repo.clone(), ctx.paths.state_dir.clone()];
    trees.extend(config.dirs.paths.iter().map(|key| ctx.paths.home.join(key)));
    if let Some(dirs) = &state.capture_dirs {
        trees.extend(dirs.iter().map(|key| ctx.paths.home.join(key)));
    }
    for entry in fs::read_dir(&ctx.paths.home)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(crate::paths::is_reserved_root_name)
        {
            match fs::metadata(entry.path()) {
                Ok(meta) if meta.is_dir() => trees.push(entry.path()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    paths.extend(trees.iter().cloned());
    for protected in &paths {
        if let (Some(parent), Some(name)) = (protected.parent(), protected.file_name()) {
            match fs::canonicalize(parent) {
                Ok(path) => {
                    let meta = open_dir(&path)?.metadata()?;
                    let protected_id = (meta.dev(), meta.ino());
                    let parent_id = (parent_meta.dev(), parent_meta.ino());
                    ensure!(
                        name != leaf || protected_id != parent_id,
                        "identity destination overlaps a control path"
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let home = fs::canonicalize(&ctx.paths.home)?;
    let home_meta = open_dir(&home)?.metadata()?;
    if (parent_meta.dev(), parent_meta.ino()) == (home_meta.dev(), home_meta.ino()) {
        if let Some(name) = leaf.to_str() {
            ensure!(
                !crate::paths::is_reserved_root_name(name),
                "identity destination is reserved"
            );
        } else {
            bail!("identity destination name is not UTF-8");
        }
    }
    let mut protected = Vec::new();
    for path in &trees {
        match fs::canonicalize(path) {
            Ok(path) => {
                for entry in walkdir::WalkDir::new(&path).follow_links(false) {
                    let entry = entry?;
                    if entry.file_type().is_dir() {
                        let meta = open_dir(entry.path())?.metadata()?;
                        protected.push((meta.dev(), meta.ino()));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    reject_protected_ancestors(dir, &protected)
}

fn reject_protected_ancestors(dir: &File, protected: &[(u64, u64)]) -> Result<()> {
    let mut current = dir.try_clone()?;
    loop {
        let meta = current.metadata()?;
        let id = (meta.dev(), meta.ino());
        ensure!(
            !protected.contains(&id),
            "identity destination overlaps a control directory"
        );
        let next = File::from(sys::openat_dir(current.as_fd(), c"..")?);
        let meta = next.metadata()?;
        if (meta.dev(), meta.ino()) == id {
            break;
        }
        current = next;
    }
    Ok(())
}

fn lock(ctx: &Context) -> Result<(Lock, (u64, u64))> {
    let base = ctx
        .paths
        .state_dir
        .parent()
        .ok_or_else(|| anyhow!("state directory has no parent"))?;
    let base = open_dir(base)?;
    let meta = base.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode() & 0o022 == 0,
        "unsafe state parent"
    );
    let name = CString::new(
        ctx.paths
            .state_dir
            .file_name()
            .ok_or_else(|| anyhow!("state directory has no name"))?
            .as_bytes(),
    )?;
    match sys::mkdirat(base.as_fd(), &name, 0o700) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let dir = File::from(sys::openat_dir(base.as_fd(), &name)?);
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode().trailing_zeros() >= 6,
        "unsafe state directory"
    );
    let file = sys::openat_lock(dir.as_fd(), c"lock")?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == sys::geteuid()
            && meta.nlink() == 1
            && meta.mode().trailing_zeros() >= 6,
        "unsafe lock file"
    );
    let id = (meta.dev(), meta.ino());
    Ok((Lock::acquire_strict(file)?, id))
}

fn check_lock(ctx: &Context, id: (u64, u64)) -> Result<()> {
    let dir = open_dir(&ctx.paths.state_dir)?;
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode().trailing_zeros() >= 6,
        "unsafe state directory"
    );
    let path = format!("/proc/{}/fd/{}/lock", std::process::id(), dir.as_raw_fd());
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file()
            && meta.uid() == sys::geteuid()
            && meta.nlink() == 1
            && meta.mode().trailing_zeros() >= 6
            && (meta.dev(), meta.ino()) == id,
        "lock changed"
    );
    Ok(())
}

fn check(ctx: &Context, request: &Request) -> Result<()> {
    ensure!(
        ctx.paths.home.as_os_str() == request.home.path()?.as_os_str()
            && ctx.paths.repo.as_os_str() == request.repo.path()?.as_os_str()
            && ctx.paths.config_file.as_os_str() == request.config.path()?.as_os_str()
            && ctx.paths.age_identity.as_os_str() == request.identity.path()?.as_os_str()
            && ctx.repo_id()? == request.repo_id
            && observe(&ctx.paths.config_file)? == request.config_observation,
        "reviewed identity context changed"
    );
    Ok(())
}

fn check_recovery(ctx: &Context, path: &Path, dev: u64, ino: u64) -> Result<()> {
    ensure!(
        ctx.paths.state_dir.as_os_str() == path.as_os_str(),
        "recovery parent changed"
    );
    let meta = open_dir(path)?.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid()
            && meta.mode() & 0o7077 == 0
            && (meta.dev(), meta.ino()) == (dev, ino),
        "recovery parent changed or is not private"
    );
    Ok(())
}

fn check_request_recovery(ctx: &Context, request: &Request) -> Result<()> {
    if let Operation::Grant {
        recovery_parent,
        recovery_parent_dev,
        recovery_parent_ino,
        ..
    } = &request.operation
    {
        check_recovery(
            ctx,
            &recovery_parent.path()?,
            *recovery_parent_dev,
            *recovery_parent_ino,
        )?;
    }
    Ok(())
}

fn check_grant_context(ctx: &Context, request: &Request, lock_id: (u64, u64)) -> Result<()> {
    let current = Context::resolve("key")?;
    check(&current, request)?;
    check_request_recovery(&current, request)?;
    check_lock(ctx, lock_id)
}

fn check_preflight_context(
    ctx: &Context,
    request: &Request,
    recovery: &fs::Metadata,
) -> Result<()> {
    let current = Context::resolve("key")?;
    check(&current, request)?;
    check_recovery(
        &current,
        &ctx.paths.state_dir,
        recovery.dev(),
        recovery.ino(),
    )
}

fn recipients(ctx: &Context, bytes: &[u8]) -> Result<Vec<String>> {
    let recipients = ctx
        .age()
        .recipients_of(bytes)
        .map_err(|_| anyhow!("cannot derive recipient"))?;
    ensure!(
        recipients.len() == 1
            && recipients.iter().all(|r| r.len() == 62
                && r.starts_with("age1")
                && r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
        "unsupported native age identity"
    );
    Ok(recipients)
}

fn preflight(ctx: &Context, request: &Request, identity_dir: &File) -> Result<Outcome> {
    let recovery = open_dir(&ctx.paths.state_dir)?.metadata()?;
    check_recovery(ctx, &ctx.paths.state_dir, recovery.dev(), recovery.ino())?;
    let repo = open_dir(&ctx.paths.repo)?;
    let repo_meta = repo.metadata()?;
    ensure!(
        repo_meta.uid() == sys::geteuid() && repo_meta.mode() & 0o022 == 0,
        "repository directory is not private"
    );
    let identity_name = ctx
        .paths
        .age_identity
        .file_name()
        .ok_or_else(|| anyhow!("identity has no name"))?;
    let identity_path = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        identity_dir.as_raw_fd()
    ))
    .join(identity_name);
    let identity_observation =
        observe(&identity_path)?.ok_or_else(|| anyhow!("identity is absent"))?;
    let bytes = source_at(identity_dir, identity_name, &identity_observation)?;
    let recipients = recipients(ctx, &bytes)?;
    let path = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}/.dotman.toml",
        std::process::id(),
        repo.as_raw_fd()
    ));
    let mut settings = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|_| anyhow!("cannot open repository settings"))?;
    let meta = settings.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == sys::geteuid() && meta.nlink() == 1,
        "repository settings are not an owned single-link regular file"
    );
    let settings_observation = observation(&meta);
    let mut content = Vec::new();
    (&mut settings).take(1_048_577).read_to_end(&mut content)?;
    ensure!(
        content.len() <= 1_048_576
            && observation(&settings.metadata()?) == settings_observation
            && observe(&path)?.as_ref() == Some(&settings_observation),
        "repository settings changed or are too large"
    );
    let text =
        std::str::from_utf8(&content).map_err(|_| anyhow!("repository settings are not text"))?;
    let config = RepoConfig::parse(text)?;
    let recipient_present = recipients
        .iter()
        .all(|recipient| config.age.recipients.contains(recipient));
    check_preflight_context(ctx, request, &recovery)?;
    let again = parent(ctx, request)?;
    ensure!(
        (again.metadata()?.dev(), again.metadata()?.ino())
            == (request.parent_dev, request.parent_ino)
            && source_at(identity_dir, identity_name, &identity_observation)? == bytes,
        "identity changed"
    );
    let again = open_dir(&ctx.paths.repo)?.metadata()?;
    ensure!(
        (again.dev(), again.ino()) == (repo_meta.dev(), repo_meta.ino())
            && observe(&path)?.as_ref() == Some(&settings_observation),
        "repository changed"
    );
    json::write(
        &mut io::stdout().lock(),
        &GrantPreflight {
            record: "grant_preflight",
            schema_version: 1,
            repo_id: &request.repo_id,
            home: &request.home,
            repo: &request.repo,
            config: &request.config,
            identity: &request.identity,
            config_observation: &request.config_observation,
            parent_dev: request.parent_dev,
            parent_ino: request.parent_ino,
            repo_dev: repo_meta.dev(),
            repo_ino: repo_meta.ino(),
            identity_observation,
            repo_config_observation: settings_observation,
            repo_config_digest: crate::engine::hash_hex(&content),
            recipients,
            recipient_present,
            backup_consequence: "A settings change retains a reviewed-byte copy and the displaced settings inode in a private recovery directory under this state directory.",
            recovery_parent: json::PathValue::new(&ctx.paths.state_dir),
            recovery_parent_dev: recovery.dev(),
            recovery_parent_ino: recovery.ino(),
        },
    )?;
    Ok(Outcome::Done)
}

fn grant(
    ctx: &Context,
    request: &Request,
    identity_dir: &File,
    lock_id: (u64, u64),
) -> Result<Outcome> {
    let Operation::Grant {
        identity_observation,
        repo_config_observation,
        repo_config_digest,
        repo_dev,
        repo_ino,
        recipients: approved,
        ..
    } = &request.operation
    else {
        bail!("not a grant request");
    };
    let repo = open_dir(&ctx.paths.repo)?;
    let meta = repo.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid()
            && meta.mode() & 0o022 == 0
            && (meta.dev(), meta.ino()) == (*repo_dev, *repo_ino),
        "repository directory changed or is not private"
    );
    let path = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}/.dotman.toml",
        std::process::id(),
        repo.as_raw_fd()
    ));
    ensure!(
        observe(&path)?.as_ref() == Some(repo_config_observation),
        "repository settings changed"
    );
    let identity = ctx
        .paths
        .age_identity
        .file_name()
        .ok_or_else(|| anyhow!("identity has no name"))?;
    let bytes = source_at(identity_dir, identity, identity_observation)?;
    ensure!(
        recipients(ctx, &bytes)? == *approved,
        "recipient review changed"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)?;
    let file_meta = file.metadata()?;
    ensure!(
        file_meta.is_file()
            && file_meta.uid() == sys::geteuid()
            && file_meta.nlink() == 1
            && observation(&file_meta) == *repo_config_observation,
        "repository settings changed"
    );
    let mut content = Vec::new();
    (&mut file).take(1_048_577).read_to_end(&mut content)?;
    ensure!(
        content.len() <= 1_048_576
            && observation(&file.metadata()?) == *repo_config_observation
            && crate::engine::hash_hex(&content) == *repo_config_digest,
        "repository settings changed"
    );
    let config = std::str::from_utf8(&content)?;
    let config = RepoConfig::parse(config)?;
    ensure!(
        observe(&path)?.as_ref() == Some(repo_config_observation),
        "repository settings changed"
    );
    let recheck = || -> Result<()> {
        check_grant_context(ctx, request, lock_id)?;
        parent(ctx, request)?;
        ensure!(
            source_at(identity_dir, identity, identity_observation)? == bytes
                && observe(&path)?.as_ref() == Some(repo_config_observation),
            "recipient review changed"
        );
        let again = open_dir(&ctx.paths.repo)?.metadata()?;
        ensure!(
            (again.dev(), again.ino()) == (meta.dev(), meta.ino()),
            "repository changed"
        );
        Ok(())
    };
    if approved
        .iter()
        .all(|recipient| config.age.recipients.contains(recipient))
    {
        recheck()?;
    } else {
        let recipient = approved
            .first()
            .ok_or_else(|| anyhow!("missing approved recipient"))?;
        settings::publish(ctx, &repo, &file, &content, recipient, recheck)?;
    }
    json::write(
        &mut io::stdout().lock(),
        &Receipt {
            record: "recipients_added",
            recipients: approved.clone(),
        },
    )?;
    Ok(Outcome::Done)
}

pub(super) fn run(ctx: &Context) -> Result<Outcome> {
    let mut input = Vec::new();
    io::stdin().lock().take(1_048_577).read_to_end(&mut input)?;
    ensure!(input.len() <= 1_048_576, "identity request too large");
    let request: Request = serde_json::from_slice(&input)?;
    ensure!(
        request.schema_version == 1,
        "unsupported identity request version"
    );
    let imported = match &request.operation {
        Operation::Import {
            source: path,
            source_observation,
        } => Some(source(&path.path()?, source_observation)?),
        Operation::Create | Operation::GrantPreflight | Operation::Grant { .. } => None,
    };
    check(ctx, &request)?;
    check_request_recovery(ctx, &request)?;
    let dir = parent(ctx, &request)?;
    destination(ctx, &dir)?;
    if matches!(request.operation, Operation::GrantPreflight) {
        return preflight(ctx, &request, &dir);
    }
    let leaf = CString::new(
        ctx.paths
            .age_identity
            .file_name()
            .ok_or_else(|| anyhow!("identity has no name"))?
            .as_bytes(),
    )?;
    if !matches!(request.operation, Operation::Grant { .. }) {
        ensure!(
            sys::kind_at(dir.as_fd(), &leaf)?.is_none(),
            "identity destination exists"
        );
    }
    let (_lock, lock_id) = lock(ctx)?;
    ctx.guard()?;
    let current = Context::resolve("key")?;
    check(&current, &request)?;
    ensure!(
        current.paths.lock_file() == ctx.paths.lock_file(),
        "lock path changed"
    );
    check_lock(&current, lock_id)?;
    let current_parent = parent(&current, &request)?;
    destination(&current, &current_parent)?;
    ensure!(
        current_parent.metadata()?.ino() == dir.metadata()?.ino()
            && current_parent.metadata()?.dev() == dir.metadata()?.dev(),
        "identity parent changed"
    );
    if matches!(request.operation, Operation::Grant { .. }) {
        return grant(&current, &request, &dir, lock_id);
    }
    ensure!(
        sys::kind_at(dir.as_fd(), &leaf)?.is_none(),
        "identity destination exists"
    );

    install(
        &current,
        &request,
        &dir,
        &leaf,
        imported.as_deref(),
        lock_id,
    )
}

fn install(
    current: &Context,
    request: &Request,
    dir: &File,
    leaf: &std::ffi::CStr,
    imported: Option<&[u8]>,
    lock_id: (u64, u64),
) -> Result<Outcome> {
    let base = format!("/proc/{}/fd/{}", std::process::id(), dir.as_raw_fd());
    let stage = tempfile::Builder::new()
        .prefix(&format!("{TEMP_PREFIX}{}-", std::process::id()))
        .tempdir_in(base)?;
    let stage_dir = File::open(stage.path())?;
    let staged = stage.path().join("identity");
    if let Some(bytes) = imported {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        file.write_all(bytes)?;
    } else {
        current
            .age()
            .generate(&staged)
            .map_err(|_| anyhow!("age key generation failed"))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&staged)?;
    ensure!(
        file.metadata()?.is_file(),
        "generated identity is not a regular file"
    );
    file.set_permissions(Permissions::from_mode(0o600))?;
    let mut bytes = Vec::new();
    (&mut file).take(65_537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65_536, "generated identity is too large");
    native(&bytes)?;
    let recipients = recipients(current, &bytes)?;
    file.sync_all()?;
    check(&Context::resolve("key")?, request)?;
    check_lock(current, lock_id)?;
    if let Operation::Import {
        source: path,
        source_observation,
    } = &request.operation
    {
        ensure!(
            source(&path.path()?, source_observation)? == bytes,
            "identity source changed"
        );
    }
    let again = parent(current, request)?;
    destination(current, &again)?;
    ensure!(
        again.metadata()?.ino() == dir.metadata()?.ino()
            && again.metadata()?.dev() == dir.metadata()?.dev(),
        "identity parent changed"
    );
    sys::rename_noreplace(stage_dir.as_fd(), c"identity", dir.as_fd(), leaf)?;
    dir.sync_all()?;
    json::write(
        &mut io::stdout().lock(),
        &Receipt {
            record: "identity",
            recipients,
        },
    )?;
    Ok(Outcome::Done)
}
