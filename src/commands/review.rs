use crate::commands::status::Observation;
use crate::engine::Tuple;
use crate::git::{Entry, Git};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

/// Replacements/removals observed or computed without writing the repository.
pub type Delta = BTreeMap<PathBuf, Stored>;

/// Produced re-encryption bytes and the settings snapshot that selected their recipients.
pub struct Reencrypted {
    pub files: Delta,
    pub settings: Stored,
    pub index: BTreeMap<PathBuf, Entry>,
}

/// Only the staging policy a capture changes; no template values or approvals.
pub struct StagePolicy {
    pub patterns: Vec<String>,
    pub kept: BTreeSet<String>,
}

/// Logical home captures and their recorded state; ciphertext stays outside this snapshot.
#[derive(Default)]
pub struct PostCapture {
    pub sources: BTreeMap<String, crate::engine::status::SourceEntry>,
    pub files: BTreeMap<String, crate::state::Entry>,
    pub modes: BTreeMap<String, String>,
}

impl PostCapture {
    pub(crate) fn restore(
        &self,
        config: &mut crate::repo_config::RepoConfig,
        state: &mut crate::state::State,
    ) {
        config.modes.clone_from(&self.modes);
        state.files.extend(self.files.clone());
    }
}

/// Ciphertext is explicitly unknown until a real capture encrypts it.
#[derive(Clone, PartialEq, Eq)]
pub enum Stored {
    File { bytes: Vec<u8>, mode: u32 },
    Link(PathBuf),
    Missing,
    Ciphertext,
}

impl Stored {
    pub(crate) fn read(root: &Path, path: &Path) -> Result<Self> {
        Self::read_inner(root, path, false)
    }

    pub(crate) fn read_staged(root: &Path, path: &Path, tracked: bool) -> Result<Self> {
        Self::read_inner(root, path, tracked)
    }

    fn read_inner(root: &Path, path: &Path, displaced: bool) -> Result<Self> {
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "stored path is not repository-relative"
        );
        let mut parents: Vec<_> = path
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        parents.reverse();
        for parent in parents {
            match fs::symlink_metadata(root.join(parent)) {
                Ok(meta) if displaced && meta.is_file() => return Ok(Self::Missing),
                Ok(meta) => ensure!(meta.is_dir(), "stored path has a non-directory parent"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Missing),
                Err(error) => return Err(error.into()),
            }
        }
        let full = root.join(path);
        let meta = match fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => return Err(error.into()),
        };
        if meta.is_symlink() {
            return Ok(Self::Link(fs::read_link(full)?));
        }
        if displaced && meta.is_dir() {
            match fs::symlink_metadata(full.join(".git")) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => anyhow::bail!(
                    "outgoing review does not support a potential embedded Git repository"
                ),
            }
            return Ok(Self::Missing);
        }
        ensure!(meta.is_file(), "stored path is not a file or symlink");
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(full)?;
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "stored path changed while being read");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Self::File {
            bytes,
            mode: meta.mode() & 0o7777,
        })
    }

    pub(crate) fn observation(&self) -> Observation {
        match self {
            Self::File { bytes, mode } => Observation::tuple(&Tuple::of_file(bytes, *mode)),
            Self::Link(target) => Observation::tuple(&Tuple::of_link(target)),
            Self::Missing => Observation::Missing,
            Self::Ciphertext => Observation::Unavailable {
                reason: "hypothetical_ciphertext",
            },
        }
    }

    pub(crate) fn git_tuple(&self) -> Option<Tuple> {
        let mut tuple = match self {
            Self::File { bytes, mode } => Tuple::of_file(bytes, *mode),
            Self::Link(target) => Tuple::of_link(target),
            Self::Missing | Self::Ciphertext => return None,
        };
        if tuple.kind == crate::state::Kind::File {
            tuple.mode = if tuple.mode & 0o100 == 0 {
                0o644
            } else {
                0o755
            };
        }
        Some(tuple)
    }

    pub(crate) fn staged_tuple(&self, filemode: bool, index: Option<&Entry>) -> Option<Tuple> {
        let mut tuple = self.git_tuple()?;
        if tuple.kind == crate::state::Kind::File && !filemode {
            // Git retains a regular index entry's mode; a new regular entry defaults to 0644.
            tuple.mode = if index.is_some_and(|entry| entry.mode == "100755") {
                0o755
            } else {
                0o644
            };
        }
        Some(tuple)
    }

    pub(crate) fn from_entry(git: &Git, entry: &Entry) -> Result<Self> {
        let mode = match entry.mode.as_str() {
            "100644" => Some(0o644),
            "100755" => Some(0o755),
            "120000" => None,
            _ => anyhow::bail!(
                "outgoing review supports files and symlinks, not Git mode {}",
                entry.mode
            ),
        };
        let bytes = git.blob(&entry.oid)?;
        Ok(match mode {
            Some(mode) => Self::File { bytes, mode },
            None => Self::Link(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes))),
        })
    }
}
