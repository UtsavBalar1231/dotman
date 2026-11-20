use super::{Context, RepoConfig, json, observation, open_dir, reject_protected_ancestors, sys};
use anyhow::{Result, anyhow, ensure};
use serde::Serialize;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[derive(Serialize)]
pub(super) struct Recovery {
    record: &'static str,
    phase: &'static str,
    directory: json::PathValue,
}

fn report(phase: &'static str, path: &std::path::Path) -> Result<()> {
    json::write(
        &mut io::stdout().lock(),
        &Recovery {
            record: "settings_recovery",
            phase,
            directory: json::PathValue::new(path),
        },
    )
}

fn fd_path(dir: &File, name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        dir.as_raw_fd()
    ))
    .join(name)
}

fn read_at(dir: &File, name: &str) -> Result<(File, Vec<u8>)> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(fd_path(dir, name))?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == sys::geteuid()
            && meta.nlink() == 1
            && meta.size() <= 1_048_576,
        "unsafe settings recovery file"
    );
    let mut bytes = Vec::new();
    (&mut file).take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1_048_576 && observation(&file.metadata()?) == observation(&meta),
        "settings recovery file changed"
    );
    Ok((file, bytes))
}

fn check_dirs(
    ctx: &Context,
    repo: &File,
    state: &File,
    dir: &File,
    name: &std::ffi::CStr,
) -> Result<()> {
    let repo_meta = repo.metadata()?;
    let state_meta = state.metadata()?;
    let dir_meta = dir.metadata()?;
    let named_repo = open_dir(&ctx.paths.repo)?.metadata()?;
    let named_state = open_dir(&ctx.paths.state_dir)?.metadata()?;
    let named_dir = File::from(sys::openat_dir(state.as_fd(), name)?).metadata()?;
    ensure!(
        repo_meta.uid() == sys::geteuid()
            && repo_meta.mode() & 0o022 == 0
            && state_meta.uid() == sys::geteuid()
            && state_meta.mode() & 0o7077 == 0
            && dir_meta.uid() == sys::geteuid()
            && dir_meta.mode() & 0o7077 == 0
            && (named_repo.dev(), named_repo.ino()) == (repo_meta.dev(), repo_meta.ino())
            && (named_state.dev(), named_state.ino()) == (state_meta.dev(), state_meta.ino())
            && (named_dir.dev(), named_dir.ino()) == (dir_meta.dev(), dir_meta.ino()),
        "recovery or repository directory changed"
    );
    Ok(())
}

fn same_metadata(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.is_file()
        && b.is_file()
        && (a.dev(), a.ino(), a.uid(), a.gid(), a.mode(), a.nlink())
            == (b.dev(), b.ino(), b.uid(), b.gid(), b.mode(), b.nlink())
}

fn edited(content: &[u8], recipient: &str) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(content)?;
    let mut expected = RepoConfig::parse(text)?;
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    if !doc.contains_key("age") {
        doc.as_table_mut()
            .insert("age", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let age = doc
        .get_mut("age")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| anyhow!("unsupported age table"))?;
    if !age.contains_key("recipients") {
        age.insert("recipients", toml_edit::value(toml_edit::Array::new()));
    }
    age.get_mut("recipients")
        .and_then(toml_edit::Item::as_array_mut)
        .ok_or_else(|| anyhow!("unsupported recipients array"))?
        .push(recipient);
    expected.age.recipients.push(recipient.to_owned());
    let candidate = doc.to_string().into_bytes();
    ensure!(
        RepoConfig::parse(std::str::from_utf8(&candidate)?)? == expected,
        "settings semantics changed"
    );
    Ok(candidate)
}

fn stage(
    dir: &File,
    content: &[u8],
    candidate: &[u8],
    meta: &fs::Metadata,
) -> Result<(fs::Metadata, fs::Metadata)> {
    let mut copy = sys::create_excl(dir.as_fd(), c"reviewed", 0o600)?;
    copy.write_all(content)?;
    copy.sync_all()?;
    let mut staged = sys::create_excl(dir.as_fd(), c"candidate", meta.mode() & 0o777)?;
    staged.write_all(candidate)?;
    staged.set_permissions(Permissions::from_mode(meta.mode() & 0o777))?;
    staged.sync_all()?;
    let candidate_meta = staged.metadata()?;
    ensure!(
        candidate_meta.is_file()
            && candidate_meta.nlink() == 1
            && (
                candidate_meta.uid(),
                candidate_meta.gid(),
                candidate_meta.mode()
            ) == (meta.uid(), meta.gid(), meta.mode())
            && !sys::has_xattrs(staged.as_fd())?,
        "cannot preserve settings metadata"
    );
    Ok((copy.metadata()?, candidate_meta))
}

pub(super) fn publish(
    ctx: &Context,
    repo: &File,
    original: &File,
    content: &[u8],
    recipient: &str,
    recheck: impl Fn() -> Result<()>,
) -> Result<()> {
    let candidate = edited(content, recipient)?;
    let meta = original.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == sys::geteuid()
            && meta.nlink() == 1
            && meta.mode() & 0o7000 == 0
            && !sys::has_xattrs(original.as_fd())?,
        "unsupported repository settings metadata"
    );
    let state = open_dir(&ctx.paths.state_dir)?;
    let state_meta = state.metadata()?;
    ensure!(
        state_meta.uid() == sys::geteuid()
            && state_meta.mode() & 0o7077 == 0
            && state_meta.dev() == meta.dev(),
        "recovery directory is not private or on the settings device"
    );
    let repo_meta = repo.metadata()?;
    reject_protected_ancestors(&state, &[(repo_meta.dev(), repo_meta.ino())])?;
    let tmp = tempfile::Builder::new()
        .prefix("key-grant-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(fd_path(&state, ""))?;
    let name = tmp
        .path()
        .file_name()
        .ok_or_else(|| anyhow!("invalid recovery directory"))?
        .to_owned();
    let recovery_path = ctx.paths.state_dir.join(&name);
    let _kept = tmp.keep();
    let name = CString::new(name.as_bytes())?;
    let dir = File::from(sys::openat_dir(state.as_fd(), &name)?);
    let dir_meta = dir.metadata()?;
    ensure!(
        dir_meta.uid() == sys::geteuid()
            && dir_meta.mode() & 0o7077 == 0
            && dir_meta.dev() == meta.dev(),
        "unsafe recovery directory"
    );
    let (copy_meta, candidate_meta) = stage(&dir, content, &candidate, &meta)?;
    dir.sync_all()?;
    state.sync_all()?;
    check_dirs(ctx, repo, &state, &dir, &name)?;
    report("prepared", &recovery_path)?;
    recheck()?;
    let (checked_stage, checked_bytes) = read_at(&dir, "candidate")?;
    let (checked_copy, checked_copy_bytes) = read_at(&dir, "reviewed")?;
    ensure!(
        same_metadata(&candidate_meta, &checked_stage.metadata()?)
            && checked_bytes == candidate
            && !sys::has_xattrs(checked_stage.as_fd())?
            && same_metadata(&copy_meta, &checked_copy.metadata()?)
            && checked_copy_bytes == content,
        "recovery files changed"
    );
    ensure!(
        same_metadata(&meta, &original.metadata()?) && !sys::has_xattrs(original.as_fd())?,
        "settings metadata changed"
    );
    let (current, current_bytes) = read_at(repo, ".dotman.toml")?;
    ensure!(
        same_metadata(&meta, &current.metadata()?) && current_bytes == content,
        "settings changed"
    );
    check_dirs(ctx, repo, &state, &dir, &name)?;
    sys::rename_exchange(dir.as_fd(), c"candidate", repo.as_fd(), c".dotman.toml")?;
    (|| -> Result<()> {
        dir.sync_all()?;
        repo.sync_all()?;
        let (installed, installed_bytes) = read_at(repo, ".dotman.toml")?;
        let (displaced, displaced_bytes) = read_at(&dir, "candidate")?;
        let (reviewed, reviewed_bytes) = read_at(&dir, "reviewed")?;
        ensure!(
            same_metadata(&candidate_meta, &installed.metadata()?)
                && installed_bytes == candidate
                && same_metadata(&meta, &displaced.metadata()?)
                && displaced_bytes == content
                && same_metadata(&copy_meta, &reviewed.metadata()?)
                && reviewed_bytes == content
                && !sys::has_xattrs(installed.as_fd())?
                && !sys::has_xattrs(displaced.as_fd())?,
            "settings mismatch"
        );
        check_dirs(ctx, repo, &state, &dir, &name)?;
        Ok(())
    })().map_err(|_| anyhow!("settings publication is ambiguous; do not restore automatically; refresh and inspect recovery"))?;
    report("published", &recovery_path).map_err(|_| {
        anyhow!("settings publication is ambiguous; do not restore automatically; refresh and inspect recovery")
    })?;
    Ok(())
}
