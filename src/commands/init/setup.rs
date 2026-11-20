use crate::commands::Outcome;
use crate::context::Context;
use crate::decisions::InputPath;
use crate::git::Git;
use crate::lock::Lock;
use crate::output::json::{self, Text};
use crate::repo_config::RepoConfig;
use crate::sys;
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[path = "static_tree.rs"]
mod static_tree;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    home: InputPath,
    repo: InputPath,
    parent_dev: u64,
    parent_ino: u64,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Fresh,
    Clone {
        source: String,
    },
    ImportPreview {
        source: InputPath,
    },
    Import {
        source: InputPath,
        review: static_tree::Review,
    },
}

fn source_protocol(source: &str) -> Result<&'static str> {
    ensure!(source.len() <= 8192, "clone source is too long");
    let (protocol, path) = if let Some(path) = source.strip_prefix("file://") {
        ("file", path)
    } else if let Some(rest) = source.strip_prefix("https://") {
        let (authority, path) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("invalid clone source"))?;
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        if let Some(port) = port {
            ensure!(
                !port.is_empty()
                    && port.bytes().all(|b| b.is_ascii_digit())
                    && port.parse::<u16>().is_ok_and(|n| n != 0),
                "invalid clone port"
            );
        }
        let ipv4 = host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.to_string() == host);
        let dns = host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            && host.rsplit('.').next().is_some_and(|last| {
                !last.bytes().all(|b| b.is_ascii_digit())
                    && !last.to_ascii_lowercase().starts_with("0x")
            });
        ensure!(ipv4 || dns, "invalid clone host");
        ("https", path)
    } else {
        bail!("invalid clone source");
    };
    let path = if protocol == "file" {
        path.strip_prefix('/')
            .ok_or_else(|| anyhow!("invalid file source"))?
    } else {
        path
    };
    ensure!(
        path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        }),
        "invalid clone path"
    );
    Ok(protocol)
}

#[derive(Serialize)]
struct Receipt {
    record: &'static str,
    operation: &'static str,
    repo_id: Text,
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<static_tree::Review>,
}

fn literal(input: &InputPath) -> Result<PathBuf> {
    let path = input.path()?;
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        bytes.starts_with(b"/")
            && bytes.len() > 1
            && bytes.get(1..).is_some_and(|rest| rest
                .split(|byte| *byte == b'/')
                .all(|part| !part.is_empty() && part != b"." && part != b"..")),
        "setup path must be absolute and literal"
    );
    Ok(path)
}

fn name(value: &OsStr) -> Result<CString> {
    Ok(CString::new(value.as_bytes())?)
}

fn open_dir(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "directory must be absolute");
    let mut dir = File::open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(part) => {
                dir = File::from(sys::openat_dir(dir.as_fd(), &name(part)?)?);
            }
            _ => bail!("directory path is not literal"),
        }
    }
    Ok(dir)
}

fn identity(file: &File) -> Result<(u64, u64)> {
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

fn absent(dir: &File, leaf: &CStr) -> Result<()> {
    ensure!(
        sys::kind_at(dir.as_fd(), leaf)?.is_none(),
        "destination exists"
    );
    Ok(())
}

fn resolved(path: &Path) -> Result<PathBuf> {
    let mut rest = Vec::new();
    let mut current = path;
    loop {
        match fs::canonicalize(current) {
            Ok(real) => return Ok(rest.iter().rev().fold(real, |dir, part| dir.join(part))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::symlink_metadata(current) {
                    Ok(_) => bail!("protected path cannot be resolved"),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                rest.push(
                    current
                        .file_name()
                        .ok_or_else(|| anyhow!("invalid protected path"))?,
                );
                current = current
                    .parent()
                    .ok_or_else(|| anyhow!("invalid protected path"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn contains(ancestor: &Path, descendant: &Path) -> Result<bool> {
    for left in ancestor.ancestors() {
        let left_meta = match fs::metadata(left) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for right in descendant.ancestors() {
            let right_meta = match fs::metadata(right) {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if (left_meta.dev(), left_meta.ino()) == (right_meta.dev(), right_meta.ino())
                && descendant
                    .strip_prefix(right)?
                    .starts_with(ancestor.strip_prefix(left)?)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn check_context(ctx: &Context, home: &Path, repo: &Path) -> Result<()> {
    ensure!(
        ctx.paths.home.as_os_str() == home.as_os_str()
            && ctx.paths.repo.as_os_str() == repo.as_os_str(),
        "setup context changed"
    );
    let actual_repo = resolved(repo)?;
    let actual_home = resolved(home)?;
    ensure!(
        !contains(&actual_repo, &actual_home)?,
        "repository overlaps home"
    );
    let default_identity = ctx.paths.config_home.join("dotman/age-identity.txt");
    let protected = [
        Some(&ctx.paths.config_file),
        Some(&ctx.paths.state_dir),
        Some(&ctx.paths.age_identity),
        Some(&default_identity),
        ctx.paths.age_key_file.as_ref(),
    ];
    for path in protected.into_iter().flatten() {
        let path = resolved(path)?;
        ensure!(
            !contains(&path, &actual_repo)? && !contains(&actual_repo, &path)?,
            "repository overlaps a protected path"
        );
    }
    Ok(())
}

fn parent(repo: &Path, expected: (u64, u64)) -> Result<File> {
    let path = repo
        .parent()
        .ok_or_else(|| anyhow!("repository has no parent"))?;
    ensure!(
        fs::canonicalize(path)?.as_os_str() == path.as_os_str(),
        "repository parent is not canonical"
    );
    let dir = open_dir(path)?;
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode() & 0o022 == 0,
        "unsafe repository parent"
    );
    ensure!(identity(&dir)? == expected, "repository parent changed");
    Ok(dir)
}

fn strict_lock(ctx: &Context) -> Result<(Lock, (u64, u64))> {
    let base = ctx
        .paths
        .state_dir
        .parent()
        .ok_or_else(|| anyhow!("missing state base"))?;
    let base = open_dir(base)?;
    let meta = base.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode() & 0o022 == 0,
        "unsafe state base"
    );
    match sys::mkdirat(base.as_fd(), c"dotman", 0o700) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let dir = File::from(sys::openat_dir(base.as_fd(), c"dotman")?);
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode().trailing_zeros() >= 6,
        "unsafe lock directory"
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

fn check_lock(ctx: &Context, expected: (u64, u64)) -> Result<()> {
    let base = ctx
        .paths
        .state_dir
        .parent()
        .ok_or_else(|| anyhow!("missing state base"))?;
    let meta = open_dir(base)?.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode() & 0o022 == 0,
        "unsafe state base"
    );
    let dir = open_dir(&ctx.paths.state_dir)?;
    let meta = dir.metadata()?;
    ensure!(
        meta.uid() == sys::geteuid() && meta.mode().trailing_zeros() >= 6,
        "unsafe lock directory"
    );
    let path = PathBuf::from(format!("/proc/self/fd/{}/lock", dir.as_raw_fd()));
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file()
            && meta.uid() == sys::geteuid()
            && meta.nlink() == 1
            && meta.mode().trailing_zeros() >= 6
            && (meta.dev(), meta.ino()) == expected,
        "lock path changed"
    );
    Ok(())
}

struct Stage {
    parent: File,
    dir: File,
    leaf: CString,
}

impl Stage {
    fn create(parent: File) -> Result<Self> {
        let parent_path = format!("/proc/self/fd/{}", parent.as_raw_fd());
        let dir = tempfile::Builder::new()
            .prefix(".dotman-setup-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&parent_path)?;
        let path = dir.keep();
        let leaf = name(
            path.file_name()
                .ok_or_else(|| anyhow!("missing stage name"))?,
        )?;
        let dir = File::from(sys::openat_dir(parent.as_fd(), &leaf)?);
        Ok(Self { parent, dir, leaf })
    }

    fn path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.dir.as_raw_fd()
        ))
    }

    fn named(&self) -> Result<bool> {
        match sys::openat_dir(self.parent.as_fd(), &self.leaf) {
            Ok(fd) => Ok(identity(&File::from(fd))? == identity(&self.dir)?),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let repo = self.path().join("repo");
        if let Err(error) = fs::remove_dir_all(repo)
            && error.kind() != io::ErrorKind::NotFound
        {
            crate::output::warning(&format!("cannot clean setup stage: {error}"));
        }
        if self.named().unwrap_or(false)
            && let Err(error) = sys::remove_dir_at(self.parent.as_fd(), &self.leaf)
        {
            crate::output::warning(&format!("cannot remove setup stage: {error}"));
        }
    }
}

fn prepare_import(
    ctx: &Context,
    source: &InputPath,
    operation: &Operation,
    paths: (&Path, &Path, (u64, u64)),
    git: &Git,
    stage: &File,
) -> Result<Option<static_tree::Snapshot>> {
    let (home, repo, expected) = paths;
    let source = literal(source)?;
    git.init()?;
    fs::set_permissions(git.repo(), fs::Permissions::from_mode(0o700))?;
    let root = match operation {
        Operation::Import { review, .. } => Some(review.root_id()),
        _ => None,
    };
    let snapshot = static_tree::capture(&source, home, repo, expected, git, ctx, root)?;
    if let Operation::ImportPreview { .. } = operation {
        json::write(&mut io::stdout().lock(), &snapshot.preview(&source))?;
        return Ok(None);
    }
    let Operation::Import { review, .. } = operation else {
        bail!("invalid setup operation");
    };
    ensure!(
        snapshot.review == *review,
        "import source changed since preview"
    );
    snapshot.write(stage)?;
    snapshot
        .config()?
        .save(&git.repo().join(".dotman.toml"), false)?;
    git.stage_all()?;
    ensure!(
        git.commit("import: selected working tree")?.is_some(),
        "empty import"
    );
    snapshot.verify_commit(git)?;
    Ok(Some(snapshot))
}

fn verify_import(
    ctx: &Context,
    operation: &Operation,
    paths: (&Path, &Path, (u64, u64)),
    git: &Git,
    snapshot: Option<&static_tree::Snapshot>,
) -> Result<()> {
    if let Operation::Import { source, review } = operation {
        let (home, repo, expected) = paths;
        let source = literal(source)?;
        snapshot
            .ok_or_else(|| anyhow!("missing import snapshot"))?
            .verify_commit(git)?;
        let latest = static_tree::capture(
            &source,
            home,
            repo,
            expected,
            git,
            ctx,
            Some(review.root_id()),
        )?;
        ensure!(
            latest.review == *review,
            "import source changed since preview"
        );
    }
    Ok(())
}

fn request() -> Result<Request> {
    let mut bytes = Vec::new();
    io::stdin().lock().take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "setup request is too large");
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid setup request"))?;
    ensure!(request.schema_version == 1, "unsupported setup version");
    Ok(request)
}

fn recheck_publication(
    ctx: &Context,
    lock_id: (u64, u64),
    stage: &Stage,
    repo: &Path,
    expected: (u64, u64),
    leaf: &CStr,
) -> Result<()> {
    check_lock(ctx, lock_id)?;
    ensure!(
        identity(&stage.parent)? == identity(&parent(repo, expected)?)?,
        "repository parent changed"
    );
    ensure!(stage.named()?, "setup stage moved");
    absent(&stage.parent, leaf)
}

pub(super) fn run() -> Result<Outcome> {
    let request = request()?;
    let protocol = match &request.operation {
        Operation::Clone { source } => Some(source_protocol(source)?),
        Operation::Fresh | Operation::ImportPreview { .. } | Operation::Import { .. } => None,
    };
    let home = literal(&request.home)?;
    let repo = literal(&request.repo)?;
    let ctx = Context::resolve("init")?;
    crate::commands::status::write_context(&ctx, "init", None)?;
    check_context(&ctx, &home, &repo)?;
    let expected = (request.parent_dev, request.parent_ino);
    let parent = parent(&repo, expected)?;
    let leaf = name(
        repo.file_name()
            .ok_or_else(|| anyhow!("repository has no name"))?,
    )?;
    absent(&parent, &leaf)?;
    let (_lock, lock_id) = strict_lock(&ctx)?;
    let current = Context::resolve("init")?;
    check_context(&current, &home, &repo)?;
    ensure!(
        current.paths.lock_file() == ctx.paths.lock_file(),
        "lock path changed"
    );
    check_lock(&current, lock_id)?;
    ensure!(
        identity(&parent)? == identity(&self::parent(&repo, expected)?)?,
        "repository parent changed"
    );
    absent(&parent, &leaf)?;
    let stage = Stage::create(parent)?;
    let repo_stage = stage.path().join("repo");
    let git = Git::new(&repo_stage, "init")
        .for_guarded_setup(stage.path())
        .with_env([
            ("GIT_AUTHOR_NAME", "dotman"),
            ("GIT_AUTHOR_EMAIL", "dotman@localhost"),
            ("GIT_COMMITTER_NAME", "dotman"),
            ("GIT_COMMITTER_EMAIL", "dotman@localhost"),
        ]);
    let mut import_snapshot = None;
    let operation = match (&request.operation, protocol) {
        (Operation::Fresh, None) => {
            git.init()?;
            RepoConfig::default().save(&repo_stage.join(".dotman.toml"), false)?;
            "fresh"
        }
        (Operation::Clone { source }, Some(protocol)) => {
            git.guarded_clone(source, protocol)?;
            "clone"
        }
        (Operation::ImportPreview { source } | Operation::Import { source, .. }, None) => {
            import_snapshot = prepare_import(
                &current,
                source,
                &request.operation,
                (&home, &repo, expected),
                &git,
                &stage.dir,
            )?;
            if import_snapshot.is_none() {
                return Ok(Outcome::Done);
            }
            "import"
        }
        _ => bail!("invalid setup operation"),
    };
    let id = fs::read_to_string(repo_stage.join(".git/dotman-id"))?;
    let current = Context::resolve("init")?;
    check_context(&current, &home, &repo)?;
    ensure!(
        current.paths.lock_file() == ctx.paths.lock_file(),
        "lock path changed"
    );
    recheck_publication(&current, lock_id, &stage, &repo, expected, &leaf)?;
    verify_import(
        &current,
        &request.operation,
        (&home, &repo, expected),
        &git,
        import_snapshot.as_ref(),
    )?;
    recheck_publication(&current, lock_id, &stage, &repo, expected, &leaf)?;
    sys::rename_noreplace(stage.dir.as_fd(), c"repo", stage.parent.as_fd(), &leaf)?;
    json::write(
        &mut io::stdout().lock(),
        &Receipt {
            record: "setup",
            operation,
            repo_id: Text::new(id.trim()),
            review: import_snapshot.map(|snapshot| snapshot.review),
        },
    )?;
    Ok(Outcome::Done)
}
