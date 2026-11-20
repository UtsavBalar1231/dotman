use super::{identity, name, open_dir, resolved};
use crate::commands::add::{hold_by_name_and_size, record_mode, secret_patterns, shareable_mode};
use crate::context::Context;
use crate::git::Git;
use crate::output::json::PathValue;
use crate::repo_config::RepoConfig;
use crate::{paths, secrets, sys};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

const MAX_FILES: usize = 4096;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    path: String,
    dev: u64,
    ino: u64,
    mode: u32,
    size: u64,
    oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Directory {
    path: String,
    dev: u64,
    ino: u64,
    mode: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Review {
    source: Vec<u8>,
    home: Vec<u8>,
    repo: Vec<u8>,
    parent_dev: u64,
    parent_ino: u64,
    source_dev: u64,
    source_ino: u64,
    manifest_sha256: String,
    directories: Vec<Directory>,
    files: Vec<Entry>,
}

impl Review {
    pub(super) const fn root_id(&self) -> (u64, u64) {
        (self.source_dev, self.source_ino)
    }
}

#[derive(Serialize)]
pub(super) struct Preview {
    record: &'static str,
    kind: &'static str,
    semantics: &'static str,
    source: PathValue,
    count: usize,
    review: Review,
}

pub(super) struct Snapshot {
    pub(super) review: Review,
    contents: Vec<Vec<u8>>,
}

fn safe_source_name(source: &Path) -> Result<()> {
    let patterns = secret_patterns(&RepoConfig::default())?;
    let text = source
        .to_str()
        .ok_or_else(|| anyhow!("sensitive or unsupported source path"))?;
    ensure!(
        hold_by_name_and_size(&patterns, text, 0).is_none(),
        "sensitive or unsupported source path"
    );
    for component in source.components() {
        if let std::path::Component::Normal(part) = component {
            let part = part
                .to_str()
                .ok_or_else(|| anyhow!("sensitive or unsupported source path"))?;
            ensure!(
                hold_by_name_and_size(&patterns, &format!("source/{part}"), 0).is_none(),
                "sensitive or unsupported source path"
            );
        }
    }
    Ok(())
}

fn contains_path(root: &File, path: &Path) -> Result<bool> {
    let id = identity(root)?;
    for ancestor in path.ancestors() {
        match fs::metadata(ancestor) {
            Ok(meta) if (meta.dev(), meta.ino()) == id => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn inside_directory(root: &File, path: &Path) -> Result<bool> {
    let target = match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => (meta.dev(), meta.ino()),
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut dir = root.try_clone()?;
    for _ in 0..256 {
        let id = identity(&dir)?;
        if id == target {
            return Ok(true);
        }
        let parent = File::from(sys::openat_dir(dir.as_fd(), c"..")?);
        if identity(&parent)? == id {
            return Ok(false);
        }
        dir = parent;
    }
    Err(anyhow!("source ancestry is too deep"))
}

fn check_source(ctx: &Context, source: &Path, repo: &Path, root: &File) -> Result<()> {
    ensure!(
        fs::canonicalize(source)? == source,
        "source must be canonical"
    );
    ensure!(
        identity(&open_dir(source)?)? == identity(root)?,
        "source changed"
    );
    let home = resolved(&ctx.paths.home)?;
    ensure!(!contains_path(root, &home)?, "source contains home");
    let repo = resolved(repo)?;
    ensure!(
        !contains_path(root, &repo)? && !inside_directory(root, &repo)?,
        "source overlaps repository"
    );
    let default_identity = ctx.paths.config_home.join("dotman/age-identity.txt");
    for path in [
        Some(&ctx.paths.config_file),
        Some(&ctx.paths.state_dir),
        Some(&ctx.paths.age_identity),
        Some(&default_identity),
        ctx.paths.age_key_file.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let path = resolved(path)?;
        ensure!(
            !contains_path(root, &path)? && !inside_directory(root, &path)?,
            "source overlaps a protected path"
        );
    }
    Ok(())
}

fn protected_files(ctx: &Context) -> Result<BTreeSet<(u64, u64)>> {
    let default_identity = ctx.paths.config_home.join("dotman/age-identity.txt");
    let state = ctx.paths.state_file();
    let mut identities = BTreeSet::new();
    for path in [
        Some(&ctx.paths.config_file),
        Some(&state),
        Some(&ctx.paths.age_identity),
        Some(&default_identity),
        ctx.paths.age_key_file.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        match fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                identities.insert((meta.dev(), meta.ino()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(identities)
}

fn check_protected(meta: &fs::Metadata, protected: &BTreeSet<(u64, u64)>) -> Result<()> {
    ensure!(
        !protected.contains(&(meta.dev(), meta.ino())),
        "source contains a protected file"
    );
    Ok(())
}

pub(super) fn capture(
    source: &Path,
    home: &Path,
    repo: &Path,
    parent: (u64, u64),
    git: &Git,
    ctx: &Context,
    expected_root: Option<(u64, u64)>,
) -> Result<Snapshot> {
    safe_source_name(source)?;
    let root = open_dir(source)?;
    let (source_dev, source_ino) = identity(&root)?;
    ensure!(
        expected_root.is_none_or(|expected| expected == (source_dev, source_ino)),
        "source changed since preview"
    );
    check_source(ctx, source, repo, &root)?;
    let protected = protected_files(ctx)?;
    let patterns = secret_patterns(&RepoConfig::default())?;
    let stage = git
        .repo()
        .parent()
        .ok_or_else(|| anyhow!("missing setup stage"))?;
    let digest_dir = tempfile::Builder::new()
        .prefix("digest-")
        .tempdir_in(stage)?;
    let digest_git = Git::new(digest_dir.path(), "init").for_guarded_setup(stage.to_path_buf());
    digest_git
        .init_guarded_sha256()
        .map_err(|_| anyhow!("guarded import requires Git with SHA-256 repository support"))?;
    let mut snapshot = Snapshot {
        review: Review {
            source: source.as_os_str().as_bytes().to_vec(),
            home: home.as_os_str().as_bytes().to_vec(),
            repo: repo.as_os_str().as_bytes().to_vec(),
            parent_dev: parent.0,
            parent_ino: parent.1,
            source_dev,
            source_ino,
            manifest_sha256: String::new(),
            directories: Vec::new(),
            files: Vec::new(),
        },
        contents: Vec::new(),
    };
    let mut total = 0;
    let mut count = 0;
    walk(
        &root,
        Path::new(""),
        0,
        &mut total,
        &mut count,
        (ctx, git, &patterns, &protected),
        &mut snapshot,
    )?;
    ensure!(
        !snapshot.review.files.is_empty(),
        "source has no importable files"
    );
    ensure!(
        identity(&open_dir(source)?)? == (source_dev, source_ino),
        "source changed"
    );
    let keys = snapshot
        .review
        .files
        .iter()
        .map(|entry| entry.path.as_str())
        .chain(
            snapshot
                .review
                .directories
                .iter()
                .map(|entry| entry.path.as_str()),
        );
    ensure!(
        paths::find_collisions(keys).is_empty(),
        "source has colliding paths"
    );
    snapshot.review.manifest_sha256 = snapshot.manifest_digest(&digest_git)?;
    digest_dir.close()?;
    ensure!(
        serde_json::to_vec(&snapshot.review)?.len() <= 900_000,
        "import review is too large"
    );
    Ok(snapshot)
}

fn walk(
    dir: &File,
    prefix: &Path,
    depth: usize,
    total: &mut u64,
    count: &mut usize,
    tools: (&Context, &Git, &[glob::Pattern], &BTreeSet<(u64, u64)>),
    snapshot: &mut Snapshot,
) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "source is too deep");
    let before_dir = dir.metadata()?;
    let mut entries = fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?
        .take(MAX_FILES.saturating_add(1))
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(entries.len() <= MAX_FILES, "source has too many entries");
    entries.sort_by_key(std::fs::DirEntry::file_name);
    if prefix.as_os_str().is_empty()
        && ["HEAD", "objects", "refs"]
            .iter()
            .all(|name| entries.iter().any(|entry| entry.file_name() == *name))
    {
        return Err(anyhow!(
            "bare Git sources are unsupported; use terminal import"
        ));
    }
    for entry in entries {
        let part = entry.file_name();
        if prefix.as_os_str().is_empty() && part == ".git" {
            continue;
        }
        *count = count
            .checked_add(1)
            .ok_or_else(|| anyhow!("source has too many entries"))?;
        ensure!(*count <= MAX_FILES, "source has too many entries");
        let path = prefix.join(&part);
        let key = path
            .to_str()
            .ok_or_else(|| anyhow!("source path is not UTF-8"))?;
        ensure!(
            hold_by_name_and_size(tools.2, key, 0).is_none(),
            "source path is held by the content guard"
        );
        paths::validate_key(key)?;
        ensure!(
            !key.split('/').any(|p| p.starts_with(".chezmoi")
                || p.starts_with("dot_")
                || p.starts_with("private_")
                || p.starts_with("run_")
                || p.starts_with("executable_")
                || p.starts_with("create_")
                || p.starts_with("symlink_")
                || p.starts_with("modify_")
                || p.starts_with("exact_")
                || p.starts_with(".git")
                || p.starts_with(".dotman-tmp-")
                || p.contains("##")
                || Path::new(p)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("tmpl"))
                || p.contains(['{', '}'])),
            "unsupported source transformation at {key}; use terminal import"
        );
        let component = name(&part)?;
        let pinned = File::from(sys::openat_path(dir.as_fd(), &component)?);
        let before = pinned.metadata()?;
        tools
            .0
            .check_key(key, !before.is_dir())
            .map_err(|_| anyhow!("source path is not manageable"))?;
        if before.is_dir() {
            let start = snapshot.review.files.len();
            snapshot.review.directories.push(Directory {
                path: key.to_owned(),
                dev: before.dev(),
                ino: before.ino(),
                mode: before.mode() & 0o777,
            });
            walk(
                &pinned,
                &path,
                depth.saturating_add(1),
                total,
                count,
                tools,
                snapshot,
            )?;
            let current = File::from(sys::openat_path(dir.as_fd(), &component)?);
            ensure!(
                identity(&current)? == identity(&pinned)?,
                "source directory changed"
            );
            ensure!(
                snapshot.review.files.len() > start,
                "empty source directory is unsupported"
            );
            continue;
        }
        read_file(dir, &pinned, key, total, (tools.1, tools.3), snapshot)?;
    }
    let after_dir = dir.metadata()?;
    ensure!(
        before_dir.mtime() == after_dir.mtime()
            && before_dir.mtime_nsec() == after_dir.mtime_nsec()
            && before_dir.ctime() == after_dir.ctime()
            && before_dir.ctime_nsec() == after_dir.ctime_nsec(),
        "source directory changed"
    );
    Ok(())
}

fn read_file(
    dir: &File,
    pinned: &File,
    key: &str,
    total: &mut u64,
    tools: (&Git, &BTreeSet<(u64, u64)>),
    snapshot: &mut Snapshot,
) -> Result<()> {
    let before = pinned.metadata()?;
    ensure!(before.is_file(), "unsupported source entry {key}");
    check_protected(&before, tools.1)?;
    ensure!(before.nlink() == 1, "hard-linked source file {key}");
    let mut file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    ensure!(
        before.mode() & 0o111 == 0,
        "executable source file {key}; use terminal import"
    );
    ensure!(
        snapshot.review.files.len() < MAX_FILES,
        "source has too many files"
    );
    *total = total
        .checked_add(before.len())
        .ok_or_else(|| anyhow!("source too large"))?;
    ensure!(*total <= MAX_BYTES, "source too large");
    let mut bytes = Vec::new();
    file.by_ref()
        .take(before.len().saturating_add(1))
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let part = Path::new(key)
        .file_name()
        .ok_or_else(|| anyhow!("invalid source path"))?;
    let current = File::from(sys::openat_path(dir.as_fd(), &name(part)?)?);
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && identity(&current)? == (before.dev(), before.ino())
            && before.mode() == after.mode()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && u64::try_from(bytes.len())? == before.len(),
        "source file changed"
    );
    ensure!(
        !bytes.starts_with(b"#!"),
        "script in source {key}; use terminal import"
    );
    let text =
        std::str::from_utf8(&bytes).map_err(|_| anyhow!("binary source file is unsupported"))?;
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
        "binary or control-bearing source file is unsupported"
    );
    ensure!(
        secrets::held(&bytes, key, &[]).is_none(),
        "source file is held by the content guard"
    );
    let oid = tools.0.hash_object(&bytes, false)?;
    snapshot.review.files.push(Entry {
        path: key.to_owned(),
        dev: before.dev(),
        ino: before.ino(),
        mode: before.mode() & 0o777,
        size: before.len(),
        oid,
    });
    snapshot.contents.push(bytes);
    Ok(())
}

impl Snapshot {
    fn manifest_digest(&self, git: &Git) -> Result<String> {
        fn field(output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
            output.extend_from_slice(&u64::try_from(bytes.len())?.to_le_bytes());
            output.extend_from_slice(bytes);
            Ok(())
        }

        let mut manifest = b"dotman-static-import-v1\0".to_vec();
        for directory in &self.review.directories {
            manifest.push(b'd');
            field(&mut manifest, directory.path.as_bytes())?;
            manifest.extend_from_slice(&directory.mode.to_le_bytes());
        }
        for (entry, bytes) in self.review.files.iter().zip(&self.contents) {
            manifest.push(b'f');
            field(&mut manifest, entry.path.as_bytes())?;
            manifest.extend_from_slice(&entry.mode.to_le_bytes());
            field(&mut manifest, bytes)?;
        }
        let digest = git.hash_object(&manifest, false)?;
        ensure!(digest.len() == 64, "Git did not produce a SHA-256 digest");
        Ok(digest)
    }

    pub(super) fn config(&self) -> Result<RepoConfig> {
        let mut config = RepoConfig::default();
        for entry in &self.review.directories {
            record_mode(
                &mut config.modes,
                &format!("{}/", entry.path),
                entry.mode,
                0o755,
            )?;
        }
        for entry in &self.review.files {
            record_mode(&mut config.modes, &entry.path, entry.mode, 0o644)?;
        }
        Ok(config)
    }

    pub(super) fn verify_commit(&self, git: &Git) -> Result<()> {
        let tree = git.tree_entries("HEAD")?;
        ensure!(
            tree.len() == self.review.files.len().saturating_add(1),
            "staged import differs from preview"
        );
        for entry in &self.review.files {
            ensure!(
                tree.get(Path::new(&entry.path))
                    .is_some_and(|item| item.mode == "100644" && item.oid == entry.oid),
                "staged import differs from preview"
            );
        }
        let config_oid =
            git.hash_object(toml::to_string_pretty(&self.config()?)?.as_bytes(), false)?;
        ensure!(
            tree.get(Path::new(".dotman.toml"))
                .is_some_and(|item| item.mode == "100644" && item.oid == config_oid),
            "staged import configuration is missing"
        );
        ensure!(git.status()?.is_empty(), "staged import worktree changed");
        Ok(())
    }

    pub(super) fn preview(&self, source: &Path) -> Preview {
        Preview {
            record: "import_preview",
            kind: "static_tree",
            semantics: "working tree only; no history, remote, hooks or apply",
            source: PathValue::new(source),
            count: self.review.files.len(),
            review: self.review.clone(),
        }
    }

    pub(super) fn write(&self, stage: &File) -> Result<()> {
        let repo = File::from(sys::openat_dir(stage.as_fd(), c"repo")?);
        for directory in &self.review.directories {
            let path = Path::new(&directory.path);
            let parent = open_relative(
                &repo,
                path.parent()
                    .ok_or_else(|| anyhow!("invalid import path"))?,
            )?;
            let leaf = name(
                path.file_name()
                    .ok_or_else(|| anyhow!("invalid import path"))?,
            )?;
            sys::mkdirat(parent.as_fd(), &leaf, 0o700)?;
        }
        for (entry, bytes) in self.review.files.iter().zip(&self.contents) {
            let path = Path::new(&entry.path);
            let parent = open_relative(
                &repo,
                path.parent()
                    .ok_or_else(|| anyhow!("invalid import path"))?,
            )?;
            let leaf = name(
                path.file_name()
                    .ok_or_else(|| anyhow!("invalid import path"))?,
            )?;
            let mut file = sys::create_excl(parent.as_fd(), &leaf, 0o600)?;
            std::io::Write::write_all(&mut file, bytes)?;
            let mode = shareable_mode(&BTreeMap::new(), &entry.path, entry.mode)?;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        for directory in self.review.directories.iter().rev() {
            open_relative(&repo, Path::new(&directory.path))?
                .set_permissions(fs::Permissions::from_mode(directory.mode & 0o755))?;
        }
        Ok(())
    }
}

fn open_relative(root: &File, path: &Path) -> Result<File> {
    let mut dir = root.try_clone()?;
    for part in path.components() {
        let std::path::Component::Normal(part) = part else {
            return Err(anyhow!("invalid import path"));
        };
        dir = File::from(sys::openat_dir(dir.as_fd(), &name(part)?)?);
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_file_identity_is_refused_before_reading() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("fixture");
        fs::write(&path, b"synthetic")?;
        let file = File::open(&path)?;
        let meta = file.metadata()?;
        let protected = BTreeSet::from([(meta.dev(), meta.ino())]);
        assert!(check_protected(&meta, &protected).is_err());
        assert!(check_protected(&meta, &BTreeSet::new()).is_ok());
        Ok(())
    }
}
