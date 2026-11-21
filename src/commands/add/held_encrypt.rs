use super::{Hold, scan_dirs};
use crate::commands::apply::{self, Classified};
use crate::commands::forget::reviewed::{self, Source};
use crate::commands::review::Stored;
use crate::context::Context;
use crate::engine::Tuple;
use crate::engine::status::{Row, Variants, Worktree};
use crate::secrets::{self, Place};
use crate::state::Entry;
use crate::variant::{self, Sources};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// reason: these are independent effects that can occur before an error.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
pub struct EncryptPartial {
    pub record_created: bool,
    pub ciphertext_created: bool,
    pub state_needs_review: bool,
    pub backup_dir_created: bool,
    pub recovery: Option<std::path::PathBuf>,
}

impl std::fmt::Display for EncryptPartial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "encryption partially completed (record created: {}, ciphertext created: {}, backup directory created: {}); review state and recipient record",
            self.record_created, self.ciphertext_created, self.backup_dir_created
        )?;
        if let Some(path) = &self.recovery {
            write!(f, "; original retained at {}", path.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for EncryptPartial {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewReview {
    key: String,
    source: String,
    repo_id: String,
    home_hash: String,
    home_mode: u32,
    record_hash: String,
    record_mode: u32,
    creates_recipient_record: bool,
    recipients: Vec<String>,
    policy_hash: String,
    hold: String,
    mode_consequence: String,
    plaintext_history_may_remain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedReview {
    key: String,
    destination: String,
    repo_id: String,
    home_hash: String,
    home_mode: u32,
    pub selected: String,
    variants: Vec<Source>,
    record_hash: String,
    record_mode: u32,
    pub creates_recipient_record: bool,
    recipients: Vec<String>,
    policy_hash: String,
    git_hash: String,
    machine_file_hash: String,
    pub recovery_parent: String,
    recovery_binding: serde_json::Value,
    pub creates_backup_directory: bool,
    pub recovery_consequence: String,
    pub mode_consequence: String,
    pub plaintext_history_may_remain: bool,
}

fn inspect_managed(ctx: &Context, key: &str) -> Result<(ManagedReview, Vec<u8>)> {
    ctx.check_key(key, true)?;
    ensure!(secrets::held_name(key).is_none(), "credential in file name");
    let repo_id = ctx.repo_id()?;
    let config = ctx.repo_config()?;
    let state = ctx.state(&repo_id)?;
    let sources = ctx.sources()?;
    let variants = Variants::new(
        Worktree::new(&ctx.paths.repo, &config.modes),
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    let Classified::Item(item) = apply::classify_key(ctx, &variants, &state, key)? else {
        anyhow::bail!("managed file cannot be selected");
    };
    ensure!(
        matches!(item.row(), Row::ChangedLocally | Row::Clean { .. }) && !item.identity_changed(),
        "managed file has an unsupported state"
    );
    let (selected, logical, encrypted, template) = item
        .source_metadata()
        .ok_or_else(|| anyhow::anyhow!("managed file has no source"))?;
    ensure!(!encrypted && !template, "selected source is not plain");
    let entry = state
        .files
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("missing recorded state"))?;
    ensure!(
        entry.source == selected && entry.hash == logical.hash,
        "recorded source differs"
    );
    let Stored::File { bytes, mode } = Stored::read(&ctx.paths.home, Path::new(key))? else {
        anyhow::bail!("home file is not regular");
    };
    ensure!(
        item.observations().1 == &crate::engine::Home::Present(Tuple::of_file(&bytes, mode)),
        "home file changed during classification"
    );
    let named = super::secret_patterns(&config)?
        .iter()
        .any(|pattern| pattern.matches_with(key, super::MATCH_SECRET));
    ensure!(
        named
            || secrets::held(&bytes, key, &ctx.machine.guard.allow)
                .is_some_and(|finding| finding.place == Place::Content),
        "file is not held for encryption"
    );
    let inventory = reviewed::inventory(ctx, key)?;
    let selected_item = inventory
        .iter()
        .find(|item| item.path == selected)
        .ok_or_else(|| anyhow::anyhow!("selected source is missing"))?;
    ensure!(
        selected_item.kind == crate::state::Kind::File,
        "source is not regular"
    );
    let Stored::File { .. } = Stored::read(&ctx.paths.repo, Path::new(selected))? else {
        anyhow::bail!("source is not regular");
    };
    let destination = variant::with_age(selected)?;
    let mode_consequence = reviewed_mode(&config, &destination, mode)?;
    ensure!(
        matches!(
            Stored::read(&ctx.paths.repo, Path::new(&destination))?,
            Stored::Missing
        ),
        "encrypted destination is not absent"
    );
    let (record, record_mode, recipients) = recipient_record(ctx, &config.age.recipients)?;
    let git_hash = git_snapshot(ctx, &destination)?;
    let (recovery_parent, recovery_binding, creates_backup_directory) =
        recovery_location(ctx, selected_item)?;
    let policy = serde_json::to_vec(&(&config, &state, &ctx.machine))?;
    Ok((ManagedReview {
        key: key.to_owned(),
        destination,
        repo_id,
        home_hash: Tuple::of_file(&bytes, mode).hash,
        home_mode: mode,
        selected: selected.to_owned(),
        variants: inventory,
        record_hash: record_hash(&record, record_mode),
        record_mode,
        creates_recipient_record: record_mode == u32::MAX,
        recipients,
        policy_hash: Tuple::of_file(&policy, 0).hash,
        git_hash,
        machine_file_hash: machine_snapshot(ctx)?,
        recovery_parent,
        recovery_binding,
        creates_backup_directory,
        recovery_consequence: if creates_backup_directory {
            "Create a 0700 backups directory in the private state directory, then move the existing repository copy into a private, unencrypted recovery backup outside the repository."
        } else {
            "Move the existing repository copy to a private, unencrypted recovery backup outside the repository."
        }.to_owned(),
        mode_consequence,
        plaintext_history_may_remain: true,
    }, bytes))
}

pub fn review_managed(ctx: &Context, key: &str) -> Result<ManagedReview> {
    inspect_managed(ctx, key).map(|(review, _)| review)
}

fn reviewed_mode(
    config: &crate::repo_config::RepoConfig,
    source: &str,
    mode: u32,
) -> Result<String> {
    ensure!(mode == 0o600 || mode == 0o644, "unsupported home file mode");
    ensure!(
        !config.modes.contains_key(source),
        "explicit encrypted target mode is unsupported"
    );
    Ok(if mode == 0o644 {
        "The home file is 0644; the encrypted file applies at 0600 on the next apply."
    } else {
        "The encrypted file applies at 0600."
    }
    .to_owned())
}

fn machine_snapshot(ctx: &Context) -> Result<String> {
    let relative = ctx.paths.config_file.strip_prefix("/")?;
    match Stored::read(Path::new("/"), relative)? {
        Stored::Missing => {
            ensure!(
                ctx.machine == crate::config::Config::default(),
                "machine config changed"
            );
            Ok("missing".to_owned())
        }
        Stored::File { bytes, mode } => {
            let (config, _) = crate::config::Config::parse(std::str::from_utf8(&bytes)?)?;
            ensure!(config == ctx.machine, "machine config changed");
            let tuple = Tuple::of_file(&bytes, mode);
            Ok(format!("{}:{:04o}", tuple.hash, tuple.mode))
        }
        _ => anyhow::bail!("machine config is not a regular file"),
    }
}

fn recipient_record(ctx: &Context, configured: &[String]) -> Result<(Vec<u8>, u32, Vec<String>)> {
    let recipients = secrets::recipient_set(configured);
    ensure!(!recipients.is_empty(), "recipient set is empty");
    let (bytes, mode) = match Stored::read(&ctx.paths.repo, Path::new(secrets::RECORD))? {
        Stored::File { bytes, mode } => (bytes, mode),
        Stored::Missing => {
            ensure_no_encrypted(ctx)?;
            return Ok((Vec::new(), u32::MAX, recipients));
        }
        _ => anyhow::bail!("recipient record must be a regular file"),
    };
    let text = std::str::from_utf8(&bytes)?;
    ensure!(
        !recipients.is_empty()
            && secrets::recipient_set(&text.lines().map(str::to_owned).collect::<Vec<_>>())
                == recipients,
        "recipient record differs"
    );
    Ok((bytes, mode, recipients))
}

fn record_hash(bytes: &[u8], mode: u32) -> String {
    if mode == u32::MAX {
        "missing".to_owned()
    } else {
        Tuple::of_file(bytes, mode).hash
    }
}

fn ensure_no_encrypted(ctx: &Context) -> Result<()> {
    check_encrypted_inventory(ctx, None)
}

fn check_encrypted_inventory(ctx: &Context, allowed: Option<(&str, &[u8])>) -> Result<()> {
    let git = ctx.git.staging_review(&[], &[])?;
    let mut paths = Vec::new();
    let mut walk = walkdir::WalkDir::new(&ctx.paths.repo)
        .min_depth(1)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = walk.next() {
        let entry = entry?;
        let path = entry.path().strip_prefix(&ctx.paths.repo)?;
        if entry.depth() == 1 && matches!(entry.file_name().to_str(), Some(".git" | ".dotman")) {
            if entry.file_type().is_dir() {
                walk.skip_current_dir();
            }
            continue;
        }
        if !entry.file_type().is_dir() {
            paths.push(
                path.to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path prevents recipient review"))?
                    .to_owned(),
            );
        }
    }
    let worktree = Sources::group(paths.iter().map(String::as_str));
    ensure!(
        worktree.keys().all(|key| worktree
            .sources_of(key)
            .all(|source| worktree.flag(key, source) != variant::Flag::Age
                || allowed.is_some_and(|(target, _)| source == target))),
        "encrypted inventory changed"
    );
    if let Some((target, expected)) = allowed {
        ensure!(
            matches!(Stored::read(&ctx.paths.repo, Path::new(target))?,
                Stored::File { bytes, .. } if bytes == expected),
            "published ciphertext changed"
        );
    }
    paths.clear();
    for path in git.head.keys().chain(git.index.keys()) {
        paths.push(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 git path prevents recipient review"))?
                .to_owned(),
        );
    }
    let sources = Sources::group(paths.iter().map(String::as_str));
    ensure!(
        sources.keys().all(|key| sources
            .sources_of(key)
            .all(|source| sources.flag(key, source) != variant::Flag::Age)),
        "recipient record is missing while encrypted sources exist"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_first_record(
    ctx: &Context,
    recipients: &[String],
    target: &str,
    ciphertext: &[u8],
) -> Result<()> {
    create_first_record_inner(ctx, recipients, target, ciphertext, || Ok(()))
}

#[cfg(target_os = "linux")]
fn create_first_record_inner(
    ctx: &Context,
    recipients: &[String],
    target: &str,
    ciphertext: &[u8],
    after_publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    check_encrypted_inventory(ctx, Some((target, ciphertext)))?;
    ensure!(
        secrets::recipient_set(&ctx.repo_config()?.age.recipients) == recipients,
        "recipients changed"
    );
    ensure!(
        matches!(
            Stored::read(&ctx.paths.repo, Path::new(secrets::RECORD))?,
            Stored::Missing
        ),
        "recipient record changed"
    );
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&ctx.paths.repo)?;
    match crate::sys::mkdirat(root.as_fd(), c".dotman", 0o755) {
        Ok(()) => root.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let dir = std::fs::File::from(crate::sys::openat_dir(root.as_fd(), c".dotman")?);
    let anchor = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let mut staged = tempfile::NamedTempFile::new_in(anchor)?;
    let text = format!("{}\n", recipients.join("\n"));
    staged.write_all(text.as_bytes())?;
    staged.as_file().sync_all()?;
    check_encrypted_inventory(ctx, Some((target, ciphertext)))?;
    ensure!(
        secrets::recipient_set(&ctx.repo_config()?.age.recipients) == recipients,
        "recipients changed"
    );
    ensure!(
        matches!(
            Stored::read(&ctx.paths.repo, Path::new(secrets::RECORD))?,
            Stored::Missing
        ),
        "recipient record changed"
    );
    let current = std::fs::File::from(crate::sys::openat_dir(root.as_fd(), c".dotman")?);
    let before = dir.metadata()?;
    let after = current.metadata()?;
    ensure!(
        before.dev() == after.dev() && before.ino() == after.ino(),
        "recipient record directory changed"
    );
    let leaf = std::ffi::CString::new(
        staged
            .path()
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("temporary record has no name"))?
            .as_bytes(),
    )?;
    let mode = staged.as_file().metadata()?.mode() & 0o7777;
    crate::sys::rename_noreplace(dir.as_fd(), &leaf, dir.as_fd(), c"age-recipients")?;
    let hook_ok = after_publish().is_ok();
    let valid_record = matches!(
        Stored::read(&ctx.paths.repo, Path::new(secrets::RECORD)),
        Ok(Stored::File { bytes, mode: actual }) if bytes == text.as_bytes() && actual == mode
    );
    if !hook_ok
        || dir.sync_all().is_err()
        || !valid_record
        || check_encrypted_inventory(ctx, Some((target, ciphertext))).is_err()
        || !ctx
            .repo_config()
            .is_ok_and(|config| secrets::recipient_set(&config.age.recipients) == recipients)
    {
        return Err(EncryptPartial {
            record_created: true,
            ciphertext_created: true,
            state_needs_review: true,
            backup_dir_created: false,
            recovery: None,
        }
        .into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn create_first_record(
    _ctx: &Context,
    _recipients: &[String],
    _target: &str,
    _ciphertext: &[u8],
) -> Result<()> {
    anyhow::bail!("creating the first recipient record is not supported on this platform")
}

#[cfg(target_os = "linux")]
fn open_directory(path: &Path) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

    ensure!(path.is_absolute(), "directory path is not absolute");
    let mut dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                dir = crate::sys::openat_dir(dir.as_fd(), &CString::new(name.as_bytes())?)?.into();
            }
            _ => anyhow::bail!("directory path is not normalized"),
        }
    }
    Ok(dir)
}

#[cfg(all(test, target_os = "linux"))]
fn prepare_recovery(ctx: &Context) -> Result<()> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let state = open_directory(&ctx.paths.state_dir)?;
    let metadata = state.metadata()?;
    ensure!(
        metadata.uid() == crate::sys::geteuid() && metadata.mode() & 0o7777 == 0o700,
        "recovery state directory must be private and owned by this user"
    );
    ensure!(
        !std::fs::canonicalize(&ctx.paths.state_dir)?
            .starts_with(std::fs::canonicalize(&ctx.paths.repo)?),
        "recovery directory must be outside the repository"
    );
    match crate::sys::mkdirat(state.as_fd(), c"backups", 0o700) {
        Ok(()) => state.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let backup = std::fs::File::from(crate::sys::openat_dir(state.as_fd(), c"backups")?);
    let meta = backup.metadata()?;
    ensure!(
        meta.uid() == crate::sys::geteuid() && meta.mode() & 0o7777 == 0o700,
        "recovery backups directory must be private and owned by this user"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RecoveryBinding {
    state_device: u64,
    state_inode: u64,
    state_mode: u32,
    source_device: u64,
    source_inode: u64,
    source_mode: u32,
    existing: Option<serde_json::Value>,
}

#[cfg(target_os = "linux")]
fn open_recovery_source(ctx: &Context, selected: &Source) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let source = Path::new(&selected.path);
    let parent = open_directory(
        &ctx.paths.repo.join(
            source
                .parent()
                .ok_or_else(|| anyhow::anyhow!("source has no parent"))?,
        ),
    )?;
    let anchor = format!("/proc/self/fd/{}", parent.as_raw_fd());
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(
            Path::new(&anchor).join(
                source
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("source has no name"))?,
            ),
        )?)
}

#[cfg(target_os = "linux")]
fn recovery_location(
    ctx: &Context,
    selected: &Source,
) -> Result<(String, serde_json::Value, bool)> {
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let state = open_directory(&ctx.paths.state_dir)?;
    let meta = state.metadata()?;
    ensure!(
        meta.uid() == crate::sys::geteuid() && meta.mode() & 0o7777 == 0o700,
        "state directory is not private"
    );
    ensure!(
        !std::fs::canonicalize(&ctx.paths.state_dir)?
            .starts_with(std::fs::canonicalize(&ctx.paths.repo)?),
        "recovery directory must be outside the repository"
    );
    let mut file = open_recovery_source(ctx, selected)?;
    let source_meta = file.metadata()?;
    ensure!(
        source_meta.is_file()
            && source_meta.uid() == crate::sys::geteuid()
            && source_meta.nlink() == 1
            && source_meta.dev() == meta.dev(),
        "recovery requires an owned regular file with one link on the backup filesystem"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        source_meta.dev() == after.dev()
            && source_meta.ino() == after.ino()
            && source_meta.mode() == after.mode()
            && source_meta.size() == after.size()
            && source_meta.mtime() == after.mtime()
            && source_meta.mtime_nsec() == after.mtime_nsec()
            && source_meta.ctime() == after.ctime()
            && source_meta.ctime_nsec() == after.ctime_nsec(),
        "source changed while read"
    );
    let tuple = Tuple::of_file(&bytes, source_meta.mode());
    ensure!(
        tuple.kind == selected.kind && tuple.hash == selected.hash && tuple.mode == selected.mode,
        "source changed"
    );
    let existing = match crate::sys::openat_dir(state.as_fd(), c"backups") {
        Ok(_) => Some(serde_json::to_value(reviewed::recovery_preflight(
            ctx, selected,
        )?)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let creates = existing.is_none();
    let binding = RecoveryBinding {
        state_device: meta.dev(),
        state_inode: meta.ino(),
        state_mode: meta.mode() & 0o7777,
        source_device: source_meta.dev(),
        source_inode: source_meta.ino(),
        source_mode: source_meta.mode() & 0o7777,
        existing,
    };
    let path = ctx.paths.state_dir.join("backups");
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("recovery path is not UTF-8"))?;
    Ok((text.to_owned(), serde_json::to_value(binding)?, creates))
}

#[cfg(not(target_os = "linux"))]
fn recovery_location(
    _ctx: &Context,
    _selected: &Source,
) -> Result<(String, serde_json::Value, bool)> {
    anyhow::bail!("managed encryption requires same-filesystem recovery support")
}

#[cfg(target_os = "linux")]
fn create_reviewed_backup(ctx: &Context, review: &ManagedReview) -> Result<()> {
    create_reviewed_backup_inner(ctx, review, || Ok(()))
}

#[cfg(target_os = "linux")]
fn create_reviewed_backup_inner(
    ctx: &Context,
    review: &ManagedReview,
    after_create: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let expected: RecoveryBinding = serde_json::from_value(review.recovery_binding.clone())?;
    ensure!(
        expected.existing.is_none(),
        "backup directory was not reviewed as absent"
    );
    let source = review
        .variants
        .iter()
        .find(|item| item.path == review.selected)
        .ok_or_else(|| anyhow::anyhow!("review has no selected source"))?;
    ensure!(
        recovery_location(ctx, source)?.1 == review.recovery_binding,
        "recovery preflight changed"
    );
    let source_fd = open_recovery_source(ctx, source)?;
    let source_metadata = source_fd.metadata()?;
    ensure!(
        source_metadata.is_file()
            && source_metadata.dev() == expected.source_device
            && source_metadata.ino() == expected.source_inode
            && source_metadata.mode() & 0o7777 == expected.source_mode,
        "source changed"
    );
    let state = open_directory(&ctx.paths.state_dir)?;
    let meta = state.metadata()?;
    ensure!(
        meta.dev() == expected.state_device
            && meta.ino() == expected.state_inode
            && meta.mode() & 0o7777 == expected.state_mode
            && meta.uid() == crate::sys::geteuid(),
        "state directory changed"
    );
    let current = open_directory(&ctx.paths.state_dir)?.metadata()?;
    ensure!(
        current.dev() == meta.dev() && current.ino() == meta.ino() && current.mode() == meta.mode(),
        "state directory changed"
    );
    crate::sys::mkdirat(state.as_fd(), c"backups", 0o700)?;
    let verified = (|| -> Result<()> {
        after_create()?;
        state.sync_all()?;
        let backup = std::fs::File::from(crate::sys::openat_dir(state.as_fd(), c"backups")?);
        let actual = backup.metadata()?;
        ensure!(
            actual.dev() == expected.source_device
                && actual.uid() == crate::sys::geteuid()
                && actual.mode() & 0o7777 == 0o700,
            "created backup directory changed"
        );
        let current = open_directory(&ctx.paths.state_dir)?.metadata()?;
        ensure!(
            current.dev() == expected.state_device
                && current.ino() == expected.state_inode
                && current.mode() & 0o7777 == expected.state_mode,
            "state directory changed"
        );
        let (_, binding, _) = recovery_location(ctx, source)?;
        let mut actual: RecoveryBinding = serde_json::from_value(binding)?;
        ensure!(actual.existing.is_some(), "backup directory is missing");
        actual.existing = None;
        ensure!(actual == expected, "source or state changed");
        let held = source_fd.metadata()?;
        ensure!(
            held.dev() == expected.source_device
                && held.ino() == expected.source_inode
                && held.mode() & 0o7777 == expected.source_mode,
            "source changed"
        );
        Ok(())
    })();
    verified.map_err(|_| {
        EncryptPartial {
            record_created: false,
            ciphertext_created: false,
            state_needs_review: true,
            backup_dir_created: true,
            recovery: None,
        }
        .into()
    })
}

fn git_snapshot(ctx: &Context, destination: &str) -> Result<String> {
    let git = ctx.git.staging_review(&[], &[])?;
    ensure!(
        !git.head.contains_key(Path::new(destination))
            && !git.index.contains_key(Path::new(destination)),
        "encrypted destination is present in git"
    );
    let describe = |entries: &std::collections::BTreeMap<std::path::PathBuf, crate::git::Entry>| {
        entries
            .iter()
            .map(|(path, entry)| {
                (
                    path.as_os_str().as_bytes().to_vec(),
                    entry.mode.clone(),
                    entry.oid.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let metadata = (
        ctx.git.head()?,
        describe(&git.head),
        describe(&git.index),
        git.filemode,
    );
    Ok(Tuple::of_file(&serde_json::to_vec(&metadata)?, 0).hash)
}

#[derive(Serialize)]
pub struct ManagedExecution {
    pub entry: Entry,
    pub backup: std::path::PathBuf,
    pub backup_dir_created: bool,
    pub recovery_consequence: String,
    pub plaintext_history_may_remain: bool,
}

#[cfg(target_os = "linux")]
struct PendingMove {
    dir: std::fs::File,
    backup_root: std::fs::File,
    source_leaf: std::ffi::CString,
    dest_leaf: std::ffi::CString,
    staged: tempfile::NamedTempFile,
    first_record: bool,
    backup_dir_created: bool,
}

#[cfg(target_os = "linux")]
const fn backup_created_partial() -> EncryptPartial {
    EncryptPartial {
        record_created: false,
        ciphertext_created: false,
        state_needs_review: true,
        backup_dir_created: true,
        recovery: None,
    }
}

#[cfg(target_os = "linux")]
fn prepare_managed_review(ctx: &Context, review: &ManagedReview) -> Result<ManagedReview> {
    if !review.creates_backup_directory {
        return Ok(review.clone());
    }
    create_reviewed_backup(ctx, review)?;
    let refreshed = inspect_managed(ctx, &review.key)
        .map_err(|_| backup_created_partial())?
        .0;
    let old_binding: RecoveryBinding = serde_json::from_value(review.recovery_binding.clone())
        .map_err(|_| backup_created_partial())?;
    let mut new_binding: RecoveryBinding =
        serde_json::from_value(refreshed.recovery_binding.clone())
            .map_err(|_| backup_created_partial())?;
    if new_binding.existing.take().is_none() || new_binding != old_binding {
        return Err(backup_created_partial().into());
    }
    let mut expected = review.clone();
    expected.recovery_binding = refreshed.recovery_binding.clone();
    expected.creates_backup_directory = refreshed.creates_backup_directory;
    expected
        .recovery_consequence
        .clone_from(&refreshed.recovery_consequence);
    if expected != refreshed || refreshed.creates_backup_directory {
        return Err(backup_created_partial().into());
    }
    Ok(refreshed)
}

// The caller holds the process lock and persists the returned entry after publication.
#[cfg(target_os = "linux")]
pub fn execute_managed(ctx: &Context, review: &ManagedReview) -> Result<ManagedExecution> {
    let (current, bytes) = inspect_managed(ctx, &review.key)?;
    ensure!(current == *review, "review is stale");
    let backup_dir_created = review.creates_backup_directory;
    let consequence = review.recovery_consequence.clone();
    let effective = prepare_managed_review(ctx, review)?;
    let review = &effective;
    let encrypted = (|| -> Result<Vec<u8>> {
        let ciphertext = ctx.age().encrypt(&bytes, &review.recipients)?;
        ensure!(
            review_managed(ctx, &review.key)? == *review,
            "review changed during encryption"
        );
        Ok(ciphertext)
    })();
    let ciphertext = match encrypted {
        Ok(bytes) => bytes,
        Err(error) if backup_dir_created => {
            return Err(error.context(backup_created_partial()));
        }
        Err(error) => return Err(error),
    };
    let first_record = review.record_mode == u32::MAX;
    let result = stage_managed(
        ctx,
        review,
        &bytes,
        &ciphertext,
        backup_dir_created,
        consequence,
    );
    let result = match result {
        Ok(result) => result,
        Err(error) if backup_dir_created => {
            if let Some(partial) = error.downcast_ref::<EncryptPartial>() {
                return Err(EncryptPartial {
                    backup_dir_created: true,
                    record_created: partial.record_created,
                    ciphertext_created: partial.ciphertext_created,
                    state_needs_review: partial.state_needs_review,
                    recovery: partial.recovery.clone(),
                }
                .into());
            }
            return Err(error.context(backup_created_partial()));
        }
        Err(error) => return Err(error),
    };
    if first_record
        && let Err(error) =
            create_first_record(ctx, &review.recipients, &review.destination, &ciphertext)
    {
        let record_created = error
            .downcast_ref::<EncryptPartial>()
            .is_some_and(|partial| partial.record_created);
        return Err(EncryptPartial {
            record_created,
            ciphertext_created: true,
            state_needs_review: true,
            backup_dir_created,
            recovery: Some(result.backup),
        }
        .into());
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
fn stage_managed(
    ctx: &Context,
    review: &ManagedReview,
    bytes: &[u8],
    ciphertext: &[u8],
    backup_dir_created: bool,
    consequence: String,
) -> Result<ManagedExecution> {
    use anyhow::Context as _;
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let source = Path::new(&review.selected);
    let destination = Path::new(&review.destination);
    let parent = source
        .parent()
        .ok_or_else(|| anyhow::anyhow!("source has no parent"))?;
    ensure!(
        destination.parent() == Some(parent),
        "destination parent differs"
    );
    let dir = open_directory(&ctx.paths.repo.join(parent))?;
    let backup_root = open_directory(Path::new(&review.recovery_parent))?;
    let source_leaf = CString::new(source.file_name().context("source has no name")?.as_bytes())?;
    let dest_leaf = CString::new(
        destination
            .file_name()
            .context("destination has no name")?
            .as_bytes(),
    )?;
    let parent_anchor = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let mut staged = tempfile::NamedTempFile::new_in(&parent_anchor)?;
    staged.write_all(ciphertext)?;
    staged.as_file().sync_all()?;
    ensure!(
        review_managed(ctx, &review.key)? == *review,
        "review changed before recovery move"
    );
    let recovery = publish_managed(
        ctx,
        review,
        PendingMove {
            dir,
            backup_root,
            source_leaf,
            dest_leaf,
            staged,
            first_record: review.record_mode == u32::MAX,
            backup_dir_created,
        },
    )?;
    Ok(ManagedExecution {
        entry: Tuple::of_file(bytes, review.home_mode).entry(&review.destination),
        backup: recovery,
        backup_dir_created,
        recovery_consequence: consequence,
        plaintext_history_may_remain: true,
    })
}

#[cfg(target_os = "linux")]
fn publish_managed(
    ctx: &Context,
    review: &ManagedReview,
    pending: PendingMove,
) -> Result<std::path::PathBuf> {
    use anyhow::Context as _;
    use std::ffi::CString;
    use std::os::fd::AsFd;
    let PendingMove {
        dir,
        backup_root,
        source_leaf,
        dest_leaf,
        staged,
        first_record,
        backup_dir_created,
    } = pending;
    let staged_leaf = CString::new(
        staged
            .path()
            .file_name()
            .context("staged file has no name")?
            .as_bytes(),
    )?;
    if first_record {
        ensure_no_encrypted(ctx).context("encrypted inventory changed before recovery move")?;
    }
    let selected = review
        .variants
        .iter()
        .find(|item| item.path == review.selected)
        .ok_or_else(|| anyhow::anyhow!("review has no selected source"))?;
    let binding: RecoveryBinding = serde_json::from_value(review.recovery_binding.clone())?;
    let binding = serde_json::from_value(
        binding
            .existing
            .ok_or_else(|| anyhow::anyhow!("backup directory is missing"))?,
    )?;
    let recovery = reviewed::move_to_recovery(ctx, &review.key, selected, &binding)
        .map_err(|error| recovery_move_error(error, backup_dir_created))?;
    let backup_path = recovery.parent().context("recovery path has no parent")?;
    let backup_dir = open_directory(backup_path).map_err(|_| EncryptPartial {
        record_created: false,
        ciphertext_created: false,
        state_needs_review: true,
        backup_dir_created,
        recovery: Some(recovery.clone()),
    })?;
    let checked = check_moved(ctx, review, backup_path, selected, &dir, &backup_root);
    if let Err(error) = checked {
        use std::os::unix::fs::MetadataExt;
        let parent = Path::new(&review.selected)
            .parent()
            .context("source has no parent")?;
        let parent_current = open_directory(&ctx.paths.repo.join(parent))
            .and_then(|current| Ok((current.metadata()?, dir.metadata()?)))
            .is_ok_and(|(current, pinned)| {
                current.dev() == pinned.dev() && current.ino() == pinned.ino()
            });
        if !parent_current {
            return Err(EncryptPartial {
                record_created: false,
                ciphertext_created: false,
                state_needs_review: true,
                backup_dir_created,
                recovery: Some(recovery),
            }
            .into());
        }
        if crate::sys::rename_noreplace(backup_dir.as_fd(), c"source", dir.as_fd(), &source_leaf)
            .is_err()
        {
            return Err(EncryptPartial {
                record_created: false,
                ciphertext_created: false,
                state_needs_review: true,
                backup_dir_created,
                recovery: Some(recovery),
            }
            .into());
        }
        std::fs::remove_dir(backup_path)?;
        anyhow::bail!("review changed ({error:#}); original was restored without overwriting");
    }
    if first_record && ensure_no_encrypted(ctx).is_err() {
        return Err(EncryptPartial {
            record_created: false,
            ciphertext_created: false,
            state_needs_review: true,
            backup_dir_created,
            recovery: Some(recovery),
        }
        .into());
    }
    finish_managed_write(
        &dir,
        &backup_root,
        &backup_dir,
        &staged_leaf,
        &dest_leaf,
        &recovery,
        backup_dir_created,
    )?;
    Ok(recovery)
}

#[cfg(target_os = "linux")]
fn recovery_move_error(error: anyhow::Error, backup_dir_created: bool) -> anyhow::Error {
    let recovery = error
        .downcast_ref::<reviewed::RecoveryMovePartial>()
        .map(|partial| partial.recovery.clone());
    recovery.map_or_else(
        || error,
        |path| {
            EncryptPartial {
                record_created: false,
                ciphertext_created: false,
                state_needs_review: true,
                backup_dir_created,
                recovery: Some(path),
            }
            .into()
        },
    )
}

#[cfg(target_os = "linux")]
fn finish_managed_write(
    dir: &std::fs::File,
    backup_root: &std::fs::File,
    backup_dir: &std::fs::File,
    staged: &std::ffi::CStr,
    destination: &std::ffi::CStr,
    recovery: &Path,
    backup_dir_created: bool,
) -> Result<()> {
    use std::os::fd::AsFd;
    let partial = |ciphertext_created| EncryptPartial {
        record_created: false,
        ciphertext_created,
        state_needs_review: true,
        backup_dir_created,
        recovery: Some(recovery.to_path_buf()),
    };
    backup_dir
        .sync_all()
        .and_then(|()| backup_root.sync_all())
        .and_then(|()| dir.sync_all())
        .map_err(|_| partial(false))?;
    crate::sys::rename_noreplace(dir.as_fd(), staged, dir.as_fd(), destination)
        .map_err(|_| partial(false))?;
    dir.sync_all().map_err(|_| partial(true))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn execute_managed(_ctx: &Context, _review: &ManagedReview) -> Result<ManagedExecution> {
    anyhow::bail!("managed encryption requires same-filesystem recovery support")
}

#[cfg(target_os = "linux")]
fn check_moved(
    ctx: &Context,
    review: &ManagedReview,
    backup: &Path,
    selected: &Source,
    source_dir: &std::fs::File,
    backup_root: &std::fs::File,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let source_parent = Path::new(&review.selected)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("source has no parent"))?;
    let current = open_directory(&ctx.paths.repo.join(source_parent))?.metadata()?;
    let pinned = source_dir.metadata()?;
    ensure!(
        current.dev() == pinned.dev() && current.ino() == pinned.ino(),
        "repository parent changed"
    );
    let recovery = open_directory(Path::new(&review.recovery_parent))?.metadata()?;
    let pinned_backup = backup_root.metadata()?;
    let state_dir = open_directory(&ctx.paths.state_dir)?.metadata()?;
    ensure!(
        recovery.dev() == pinned_backup.dev()
            && recovery.ino() == pinned_backup.ino()
            && recovery.uid() == crate::sys::geteuid()
            && recovery.mode() & 0o7777 == 0o700,
        "recovery location changed"
    );
    ensure!(
        state_dir.uid() == crate::sys::geteuid() && state_dir.mode() & 0o7777 == 0o700,
        "recovery state directory changed"
    );
    let Stored::File { bytes, mode } = Stored::read(backup, Path::new("source"))? else {
        anyhow::bail!("recovery source is not regular");
    };
    let private = std::fs::symlink_metadata(backup.join("source"))?;
    ensure!(
        private.is_file() && private.uid() == crate::sys::geteuid() && private.nlink() == 1,
        "recovery source is not an owned single-link regular file"
    );
    let actual = Tuple::of_file(&bytes, mode);
    ensure!(
        actual.kind == selected.kind
            && actual.hash == selected.hash
            && actual.mode == selected.mode,
        "recovery source changed"
    );
    let Stored::File { bytes: home, mode } = Stored::read(&ctx.paths.home, Path::new(&review.key))?
    else {
        anyhow::bail!("home file changed");
    };
    ensure!(
        Tuple::of_file(&home, mode).hash == review.home_hash && mode == review.home_mode,
        "home file changed"
    );
    let config = ctx.repo_config()?;
    let state = ctx.state(&ctx.repo_id()?)?;
    let policy = serde_json::to_vec(&(&config, &state, &ctx.machine))?;
    ensure!(
        ctx.repo_id()? == review.repo_id && Tuple::of_file(&policy, 0).hash == review.policy_hash,
        "policy changed"
    );
    ensure!(
        machine_snapshot(ctx)? == review.machine_file_hash,
        "machine config changed"
    );
    let (record, mode, recipients) = recipient_record(ctx, &config.age.recipients)?;
    ensure!(
        record_hash(&record, mode) == review.record_hash
            && mode == review.record_mode
            && recipients == review.recipients,
        "recipient record changed"
    );
    ensure!(
        git_snapshot(ctx, &review.destination)? == review.git_hash,
        "git metadata changed"
    );
    let sources = ctx.sources()?;
    let expected: Vec<_> = review
        .variants
        .iter()
        .filter(|source| source.path != review.selected)
        .cloned()
        .collect();
    let actual = if sources.sources_of(&review.key).next().is_none() {
        Vec::new()
    } else {
        reviewed::inventory(ctx, &review.key)?
    };
    ensure!(actual == expected, "variant inventory changed");
    ensure!(
        matches!(
            Stored::read(&ctx.paths.repo, Path::new(&review.selected))?,
            Stored::Missing
        ) && matches!(
            Stored::read(&ctx.paths.repo, Path::new(&review.destination))?,
            Stored::Missing
        ),
        "repository source or destination changed"
    );
    Ok(())
}

fn inspect(ctx: &Context, key: &str) -> Result<(NewReview, Vec<u8>)> {
    ctx.check_key(key, true)?;
    ensure!(secrets::held_name(key).is_none(), "credential in file name");
    let repo_id = ctx.repo_id()?;
    let config = ctx.repo_config()?;
    let state = ctx.state(&repo_id)?;
    let sources = ctx.sources()?;
    let git = ctx.git.staging_review(&[], &[])?;
    let indexed = Sources::group(
        git.head
            .keys()
            .chain(git.index.keys())
            .filter_map(|path| path.to_str()),
    );
    ensure!(
        sources.sources_of(key).next().is_none()
            && indexed.sources_of(key).next().is_none()
            && !state.files.contains_key(key),
        "file is already managed"
    );
    let mut known: BTreeSet<String> = sources.keys().map(str::to_owned).collect();
    known.extend(state.files.keys().cloned());
    let scan = scan_dirs(ctx, &config, &known, &state)?;
    let hold = match scan.held.get(key) {
        Some(Hold::SecretName | Hold::SecretBinary) => "secret_name",
        Some(Hold::Content(finding)) if finding.place == Place::Content => "content",
        _ => anyhow::bail!("file is not held for encryption"),
    };
    let Stored::File { bytes, mode } = Stored::read(&ctx.paths.home, Path::new(key))? else {
        anyhow::bail!("held file is not a regular file");
    };
    let source = variant::with_age(key)?;
    let mode_consequence = reviewed_mode(&config, &source, mode)?;
    let named = super::secret_patterns(&config)?
        .iter()
        .any(|pattern| pattern.matches_with(key, super::MATCH_SECRET));
    let content = secrets::held(&bytes, key, &ctx.machine.guard.allow)
        .is_some_and(|finding| finding.place == Place::Content);
    ensure!(
        (hold == "secret_name" && named) || (hold == "content" && content),
        "held file changed during classification"
    );
    ensure!(
        matches!(
            Stored::read(&ctx.paths.repo, Path::new(key))?,
            Stored::Missing
        ) && matches!(
            Stored::read(&ctx.paths.repo, Path::new(&source))?,
            Stored::Missing
        ),
        "repository destination is not absent"
    );
    let (record, record_mode, recipients) = recipient_record(ctx, &config.age.recipients)?;
    let policy = serde_json::to_vec(&(&config, &state, &ctx.machine))?;
    let snapshot = NewReview {
        key: key.to_owned(),
        source,
        repo_id,
        home_hash: Tuple::of_file(&bytes, mode).hash,
        home_mode: mode,
        record_hash: record_hash(&record, record_mode),
        record_mode,
        creates_recipient_record: record_mode == u32::MAX,
        recipients,
        policy_hash: Tuple::of_file(&policy, 0).hash,
        hold: hold.to_owned(),
        mode_consequence,
        plaintext_history_may_remain: true,
    };
    Ok((snapshot, bytes))
}

pub fn review_new(ctx: &Context, key: &str) -> Result<NewReview> {
    inspect(ctx, key).map(|(review, _)| review)
}

// The caller holds the process lock and persists the returned entry after publication.
pub fn execute_new(ctx: &Context, review: &NewReview) -> Result<Entry> {
    let (current, bytes) = inspect(ctx, &review.key)?;
    ensure!(current == *review, "held file changed since review");
    let ciphertext = ctx.age().encrypt(&bytes, &review.recipients)?;
    let (current, _) = inspect(ctx, &review.key)?;
    ensure!(current == *review, "held file changed during encryption");
    let first = review.record_mode == u32::MAX;
    publish(ctx, review, &ciphertext, first)?;
    if first
        && let Err(error) =
            create_first_record(ctx, &review.recipients, &review.source, &ciphertext)
    {
        let record_created = error
            .downcast_ref::<EncryptPartial>()
            .is_some_and(|partial| partial.record_created);
        return Err(EncryptPartial {
            record_created,
            ciphertext_created: true,
            state_needs_review: true,
            backup_dir_created: false,
            recovery: None,
        }
        .into());
    }
    Ok(Tuple::of_file(&bytes, review.home_mode).entry(&review.source))
}

#[cfg(target_os = "linux")]
fn publish(ctx: &Context, review: &NewReview, ciphertext: &[u8], first: bool) -> Result<()> {
    use crate::sys;
    use std::ffi::CString;
    use std::fs::OpenOptions;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let path = Path::new(&review.source);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
    let leaf = CString::new(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("missing leaf"))?
            .as_bytes(),
    )?;
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&ctx.paths.repo)?;
    let mut dir: std::os::fd::OwnedFd = root.into();
    for part in parent.components() {
        let name = CString::new(part.as_os_str().as_bytes())?;
        dir = sys::openat_dir(dir.as_fd(), &name)?;
    }
    let pinned = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let mut temporary = tempfile::NamedTempFile::new_in(pinned)?;
    temporary.write_all(ciphertext)?;
    temporary.as_file().sync_all()?;
    let (current, _) = inspect(ctx, &review.key)?;
    ensure!(current == *review, "held file changed during encryption");
    if first {
        ensure_no_encrypted(ctx)?;
    }
    let temp_name = CString::new(
        temporary
            .path()
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("missing temporary name"))?
            .as_bytes(),
    )?;
    sys::rename_noreplace(dir.as_fd(), &temp_name, dir.as_fd(), &leaf)?;
    let dir = std::fs::File::from(dir);
    if dir.sync_all().is_err() {
        return Err(EncryptPartial {
            record_created: false,
            ciphertext_created: true,
            state_needs_review: true,
            backup_dir_created: false,
            recovery: None,
        }
        .into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn publish(_ctx: &Context, _review: &NewReview, _ciphertext: &[u8], _first: bool) -> Result<()> {
    anyhow::bail!("held-file encryption is not supported on this platform")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::paths::{Env, Paths};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    struct Fixture {
        root: TempDir,
        ctx: Context,
        recipient: String,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let root = tempfile::tempdir()?;
            let home = root.path().join("home");
            let repo = root.path().join("repo");
            fs::create_dir_all(home.join(".config"))?;
            fs::create_dir_all(repo.join(".config"))?;
            let init = crate::proc::command("git")
                .args(["init", "--quiet", "--template="])
                .arg(&repo)
                .env("HOME", &home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()?;
            assert!(init.success());
            fs::write(repo.join(".git/dotman-id"), "fixture\n")?;
            let key = root.path().join("identity");
            assert!(
                crate::proc::command("age-keygen")
                    .arg("-o")
                    .arg(&key)
                    .output()?
                    .status
                    .success()
            );
            let output = crate::proc::command("age-keygen")
                .arg("-y")
                .arg(&key)
                .output()?;
            assert!(output.status.success());
            let recipient = String::from_utf8(output.stdout)?.trim().to_owned();
            fs::create_dir_all(repo.join(".dotman"))?;
            fs::write(repo.join(secrets::RECORD), format!("{recipient}\n"))?;
            fs::write(
                repo.join(".dotman.toml"),
                format!("[dirs]\npaths = [\".config\"]\n[age]\nrecipients = [\"{recipient}\"]\n"),
            )?;
            let leaf = home.join(".config/.env");
            fs::write(
                &leaf,
                concat!("gh", "p_", "012345678901234567890123456789012345"),
            )?;
            fs::set_permissions(&leaf, fs::Permissions::from_mode(0o600))?;
            fs::write(home.join("unrelated"), b"untouched")?;
            let env = Env::from_fn(|name| match name {
                "HOME" => Some(home.clone().into_os_string()),
                "DOTMAN_REPO_PATH" => Some(repo.clone().into_os_string()),
                _ => None,
            });
            let paths = Paths::resolve(&env, crate::sys::geteuid())?;
            let ctx = Context::new(paths, Config::default(), "test");
            let mut state = ctx.state("fixture")?;
            state.approve_dirs([&".config".to_owned()]);
            state.save(&ctx.paths.state_file(), false)?;
            prepare_recovery(&ctx)?;
            Ok(Self {
                root,
                ctx,
                recipient,
            })
        }
    }

    #[test]
    fn reviewed_new_file_encrypts_without_disclosing_plaintext() -> Result<()> {
        let fixture = Fixture::new()?;
        let key = ".config/.env";
        fs::remove_file(fixture.ctx.paths.repo.join(secrets::RECORD))?;
        fs::set_permissions(
            fixture.ctx.paths.home.join(key),
            fs::Permissions::from_mode(0o644),
        )?;
        let review = review_new(&fixture.ctx, key)?;
        assert_eq!(review.record_hash, "missing");
        assert!(review.mode_consequence.contains("0600"));
        let json = serde_json::to_string(&review)?;
        assert!(!json.contains(concat!("gh", "p_", "012345678901234567890123456789012345")));
        assert_eq!(serde_json::from_str::<NewReview>(&json)?, review);
        let target = fixture.ctx.paths.repo.join(&review.source);
        assert!(!target.exists());
        assert!(!fixture.ctx.paths.repo.join(key).exists());
        let entry = execute_new(&fixture.ctx, &review)?;
        assert_eq!(entry.source, review.source);
        assert_eq!(entry.mode, "0644");
        let ciphertext = fs::read(&target)?;
        let plain = fs::read(fixture.ctx.paths.home.join(key))?;
        assert!(
            !ciphertext
                .windows(plain.len())
                .any(|window| window == plain)
        );
        let output = crate::proc::command("age")
            .arg("-d")
            .arg("-i")
            .arg(fixture.root.path().join("identity"))
            .arg(&target)
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, plain);
        assert_eq!(review.recipients, vec![fixture.recipient]);
        let record = fs::read(fixture.ctx.paths.repo.join(secrets::RECORD))?;
        assert!(!record.windows(plain.len()).any(|window| window == plain));
        assert_eq!(
            fs::read(fixture.ctx.paths.home.join("unrelated"))?,
            b"untouched"
        );
        Ok(())
    }

    #[test]
    fn stale_file_recipient_and_destination_are_refused() -> Result<()> {
        let fixture = Fixture::new()?;
        let key = ".config/.env";
        let review = review_new(&fixture.ctx, key)?;
        let home = fixture.ctx.paths.home.join(key);
        let original = fs::read(&home)?;
        fs::write(&home, b"changed")?;
        assert!(execute_new(&fixture.ctx, &review).is_err());
        fs::write(&home, &original)?;
        let record = fixture.ctx.paths.repo.join(secrets::RECORD);
        fs::write(&record, b"age1different\n")?;
        assert!(execute_new(&fixture.ctx, &review).is_err());
        fs::write(&record, format!("{}\n", fixture.recipient))?;
        let config = fixture.ctx.paths.repo.join(".dotman.toml");
        let old_config = fs::read(&config)?;
        fs::write(
            &config,
            b"[dirs]\npaths = [\".config\"]\n[age]\nrecipients = [\"age1different\"]\n",
        )?;
        assert!(execute_new(&fixture.ctx, &review).is_err());
        fs::write(config, old_config)?;
        let target = fixture.ctx.paths.repo.join(&review.source);
        fs::write(&target, b"keep")?;
        assert!(execute_new(&fixture.ctx, &review).is_err());
        assert_eq!(fs::read(target)?, b"keep");
        assert_eq!(fs::read(home)?, original);
        Ok(())
    }

    #[test]
    fn nonregular_record_and_plain_counterpart_are_refused() -> Result<()> {
        let fixture = Fixture::new()?;
        let record = fixture.ctx.paths.repo.join(secrets::RECORD);
        fs::remove_file(&record)?;
        std::os::unix::fs::symlink("missing", &record)?;
        assert!(review_new(&fixture.ctx, ".config/.env").is_err());
        fs::remove_file(&record)?;
        fs::write(&record, format!("{}\n", fixture.recipient))?;
        fs::write(fixture.ctx.paths.repo.join(".config/.env"), b"existing")?;
        assert!(review_new(&fixture.ctx, ".config/.env").is_err());
        Ok(())
    }

    #[test]
    fn missing_record_refuses_existing_encrypted_sources_and_stale_record() -> Result<()> {
        let fixture = Fixture::new()?;
        let record = fixture.ctx.paths.repo.join(secrets::RECORD);
        fs::remove_file(&record)?;
        let other = fixture.ctx.paths.repo.join(".config/other{age}");
        fs::write(&other, b"existing ciphertext")?;
        assert!(review_new(&fixture.ctx, ".config/.env").is_err());
        fs::remove_file(other)?;
        let review = review_new(&fixture.ctx, ".config/.env")?;
        fs::write(&record, b"age1different\n")?;
        assert!(execute_new(&fixture.ctx, &review).is_err());
        assert_eq!(fs::read(record)?, b"age1different\n");
        assert!(!fixture.ctx.paths.repo.join(&review.source).exists());
        Ok(())
    }

    #[test]
    fn first_record_inventory_drift_reports_typed_partial() -> Result<()> {
        let fixture = Fixture::new()?;
        fs::remove_file(fixture.ctx.paths.repo.join(secrets::RECORD))?;
        let review = review_new(&fixture.ctx, ".config/.env")?;
        let plain = fs::read(fixture.ctx.paths.home.join(".config/.env"))?;
        let ciphertext = fixture.ctx.age().encrypt(&plain, &review.recipients)?;
        let target = fixture.ctx.paths.repo.join(&review.source);
        fs::write(&target, &ciphertext)?;
        let extra = fixture.ctx.paths.repo.join(".config/other{age}");
        let Err(error) = create_first_record_inner(
            &fixture.ctx,
            &review.recipients,
            &review.source,
            &ciphertext,
            || {
                fs::write(&extra, b"another ciphertext")?;
                Ok(())
            },
        ) else {
            anyhow::bail!("inventory drift was accepted")
        };
        let partial = error
            .downcast_ref::<EncryptPartial>()
            .ok_or_else(|| anyhow::anyhow!("expected structured partial"))?;
        assert!(partial.record_created && partial.ciphertext_created && partial.state_needs_review);
        assert_eq!(fs::read(&target)?, ciphertext);
        assert_eq!(fs::read(&extra)?, b"another ciphertext");
        assert_eq!(
            fs::read(fixture.ctx.paths.home.join(".config/.env"))?,
            plain
        );
        Ok(())
    }

    #[test]
    fn recovery_move_partial_keeps_private_locator() -> Result<()> {
        let path = std::path::PathBuf::from("/state/backups/recovered-test/source");
        let error = anyhow::Error::new(reviewed::RecoveryMovePartial {
            recovery: path.clone(),
        });
        let mapped = recovery_move_error(error, true);
        let partial = mapped
            .downcast_ref::<EncryptPartial>()
            .ok_or_else(|| anyhow::anyhow!("missing encryption partial"))?;
        assert_eq!(partial.recovery.as_deref(), Some(path.as_path()));
        assert!(partial.backup_dir_created && partial.state_needs_review);
        assert!(!partial.ciphertext_created && !partial.record_created);
        Ok(())
    }

    #[test]
    fn publication_rechecks_home_after_encryption() -> Result<()> {
        let fixture = Fixture::new()?;
        let key = ".config/.env";
        let review = review_new(&fixture.ctx, key)?;
        let home = fixture.ctx.paths.home.join(key);
        let bytes = fs::read(&home)?;
        let ciphertext = fixture.ctx.age().encrypt(&bytes, &review.recipients)?;
        fs::write(&home, b"changed")?;
        assert!(publish(&fixture.ctx, &review, &ciphertext, false).is_err());
        assert!(!fixture.ctx.paths.repo.join(&review.source).exists());
        assert_eq!(fs::read(home)?, b"changed");
        assert_eq!(
            fs::read_dir(fixture.ctx.paths.repo.join(".config"))?.count(),
            0
        );
        Ok(())
    }

    #[test]
    fn managed_review_is_read_only_and_binds_source() -> Result<()> {
        let fixture = Fixture::new()?;
        let key = ".config/.env";
        let home = fs::read(fixture.ctx.paths.home.join(key))?;
        let source = fixture.ctx.paths.repo.join(key);
        fs::write(&source, &home)?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        let config = fixture.ctx.repo_config_file();
        let mut text = fs::read_to_string(&config)?;
        text.push_str("\n[modes]\n\".config/.env\" = \"0600\"\n");
        fs::write(config, text)?;
        let mut state = fixture.ctx.state("fixture")?;
        state
            .files
            .insert(key.to_owned(), Tuple::of_file(&home, 0o600).entry(key));
        state.save(&fixture.ctx.paths.state_file(), false)?;
        let review = review_managed(&fixture.ctx, key)?;
        assert_eq!(review.selected, key);
        assert_eq!(review.variants.len(), 1);
        assert!(!serde_json::to_string(&review)?.contains(concat!(
            "gh",
            "p_",
            "012345678901234567890123456789012345"
        )));
        assert_eq!(fs::read(&source)?, home);
        assert!(!fixture.ctx.paths.repo.join(&review.destination).exists());
        fs::write(&source, b"other uncommitted content")?;
        assert!(execute_managed(&fixture.ctx, &review).is_err());
        assert_eq!(fs::read(&source)?, b"other uncommitted content");
        assert!(!fixture.ctx.paths.repo.join(&review.destination).exists());
        assert!(review_managed(&fixture.ctx, key).is_err());
        fs::write(&source, &home)?;
        let target = fixture.ctx.paths.repo.join(&review.destination);
        fs::write(&target, b"existing target")?;
        assert!(execute_managed(&fixture.ctx, &review).is_err());
        assert_eq!(fs::read(&target)?, b"existing target");
        assert_eq!(fs::read(&source)?, home);
        assert_eq!(
            fs::read_dir(fixture.ctx.paths.state_dir.join("backups"))?.count(),
            0
        );
        fs::remove_file(target)?;
        fs::remove_dir(fixture.ctx.paths.state_dir.join("backups"))?;
        let link = fixture.ctx.paths.home.join("source-link");
        fs::hard_link(&source, &link)?;
        assert!(review_managed(&fixture.ctx, key).is_err());
        assert!(!fixture.ctx.paths.state_dir.join("backups").exists());
        assert_eq!(fs::read(&link)?, home);
        fs::remove_file(link)?;
        let review = review_managed(&fixture.ctx, key)?;
        let Err(error) = create_reviewed_backup_inner(&fixture.ctx, &review, || {
            fs::write(&source, b"changed after directory creation")?;
            Ok(())
        }) else {
            anyhow::bail!("changed source was accepted");
        };
        let partial = error
            .downcast_ref::<EncryptPartial>()
            .ok_or_else(|| anyhow::anyhow!("missing partial result"))?;
        assert!(partial.backup_dir_created);
        assert!(!partial.ciphertext_created);
        assert!(fixture.ctx.paths.state_dir.join("backups").is_dir());
        assert!(!fixture.ctx.paths.repo.join(&review.destination).exists());
        Ok(())
    }

    fn assert_stale_recovery(
        fixture: &Fixture,
        review: &ManagedReview,
        source: &Path,
        original: &[u8],
    ) -> Result<()> {
        let backups = fixture.ctx.paths.state_dir.join("backups");
        let linked = fixture.ctx.paths.home.join("extra-link");
        fs::hard_link(source, &linked)?;
        assert!(execute_managed(&fixture.ctx, review).is_err());
        assert!(!backups.exists());
        assert_eq!(fs::read(&linked)?, original);
        fs::remove_file(linked)?;
        let old_state = fixture.ctx.paths.state_dir.with_extension("moved");
        fs::rename(&fixture.ctx.paths.state_dir, &old_state)?;
        fs::create_dir(&fixture.ctx.paths.state_dir)?;
        fs::set_permissions(
            &fixture.ctx.paths.state_dir,
            fs::Permissions::from_mode(0o700),
        )?;
        assert!(execute_managed(&fixture.ctx, review).is_err());
        assert!(!backups.exists());
        assert!(!old_state.join("backups").exists());
        fs::remove_dir(&fixture.ctx.paths.state_dir)?;
        fs::rename(&old_state, &fixture.ctx.paths.state_dir)?;
        fs::create_dir(&backups)?;
        fs::set_permissions(&backups, fs::Permissions::from_mode(0o700))?;
        assert!(execute_managed(&fixture.ctx, review).is_err());
        assert_eq!(fs::read(source)?, original);
        assert!(!fixture.ctx.paths.repo.join(&review.destination).exists());
        fs::remove_dir(&backups)?;
        fs::set_permissions(
            &fixture.ctx.paths.state_dir,
            fs::Permissions::from_mode(0o755),
        )?;
        assert!(execute_managed(&fixture.ctx, review).is_err());
        assert!(!backups.exists());
        assert_eq!(fs::read(source)?, original);
        fs::set_permissions(
            &fixture.ctx.paths.state_dir,
            fs::Permissions::from_mode(0o700),
        )?;
        Ok(())
    }

    #[test]
    fn managed_encryption_preserves_original_open_inode() -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let fixture = Fixture::new()?;
        fs::remove_file(fixture.ctx.paths.repo.join(secrets::RECORD))?;
        fs::remove_dir(fixture.ctx.paths.state_dir.join("backups"))?;
        let key = ".config/.env";
        let home_path = fixture.ctx.paths.home.join(key);
        let original = fs::read(&home_path)?;
        let source = fixture.ctx.paths.repo.join(key);
        fs::write(&source, &original)?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        let other = fixture.ctx.paths.repo.join(".config/.env{os=plan9}");
        fs::write(&other, b"other variant")?;
        let config = fixture.ctx.repo_config_file();
        let mut text = fs::read_to_string(&config)?;
        text.push_str("\n[modes]\n\".config/.env\" = \"0600\"\n");
        fs::write(config, text)?;
        let mut state = fixture.ctx.state("fixture")?;
        state
            .files
            .insert(key.to_owned(), Tuple::of_file(&original, 0o600).entry(key));
        state.save(&fixture.ctx.paths.state_file(), false)?;
        let edited = format!("{}\nlocal edit\n", std::str::from_utf8(&original)?);
        fs::write(&home_path, edited.as_bytes())?;
        fs::set_permissions(&home_path, fs::Permissions::from_mode(0o644))?;
        let mut open = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source)?;
        let inode = open.metadata()?.ino();
        let backups = fixture.ctx.paths.state_dir.join("backups");
        fs::set_permissions(
            &fixture.ctx.paths.state_dir,
            fs::Permissions::from_mode(0o755),
        )?;
        assert!(review_managed(&fixture.ctx, key).is_err());
        assert_eq!(fs::read(&source)?, original);
        fs::set_permissions(
            &fixture.ctx.paths.state_dir,
            fs::Permissions::from_mode(0o700),
        )?;
        let review = review_managed(&fixture.ctx, key)?;
        assert!(review.recovery_consequence.contains("unencrypted"));
        assert!(review.creates_backup_directory);
        assert!(!backups.exists());
        assert_stale_recovery(&fixture, &review, &source, &original)?;
        let result = execute_managed(&fixture.ctx, &review)?;
        assert!(result.backup_dir_created);
        let result_json = serde_json::to_string(&result)?;
        assert!(result_json.contains("unencrypted"));
        assert!(result_json.contains("plaintext_history_may_remain"));
        assert!(!source.exists());
        assert_eq!(fs::metadata(&result.backup)?.ino(), inode);
        open.seek(SeekFrom::End(0))?;
        open.write_all(b"\nupdated through open file")?;
        open.seek(SeekFrom::Start(0))?;
        let mut retained = Vec::new();
        open.read_to_end(&mut retained)?;
        assert_eq!(fs::read(&result.backup)?, retained);
        assert_eq!(fs::read(&home_path)?, edited.as_bytes());
        let output = crate::proc::command("age")
            .arg("-d")
            .arg("-i")
            .arg(fixture.root.path().join("identity"))
            .arg(fixture.ctx.paths.repo.join(&review.destination))
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, edited.as_bytes());
        assert_eq!(result.entry.source, review.destination);
        assert_eq!(result.entry.mode, "0644");
        assert_eq!(fs::read(other)?, b"other variant");
        Ok(())
    }
}
