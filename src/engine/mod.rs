//! The apply engine: what is in the repo (R), in `$HOME` (H) and in the state (S), and the only
//! code that writes into `$HOME`.

pub mod status;
pub mod write;

use crate::paths::validate_key;
use crate::state::{Entry, Kind};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use xxhash_rust::xxh3::xxh3_128;

// Symlink permission bits are ignored on Linux and depend on the umask on macOS, so every
// symlink compares with the same mode.
const SYMLINK_MODE: u32 = 0o777;

/// Kind, hash and mode of one entry in R, H or S. Modes count as content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tuple {
    /// Regular file or symlink.
    pub kind: Kind,
    /// xxh3-128 as 32 lowercase hex chars, of the file bytes or of the link target.
    pub hash: String,
    /// `st_mode & 0o7777`; always 0o777 for a symlink.
    pub mode: u32,
}

impl Tuple {
    /// A regular file holding `bytes`.
    #[must_use]
    pub fn of_file(bytes: &[u8], mode: u32) -> Self {
        Self {
            kind: Kind::File,
            hash: hash_hex(bytes),
            mode: mode & 0o7777,
        }
    }

    /// A symlink pointing at `target`, which is hashed verbatim.
    #[must_use]
    pub fn of_link(target: &Path) -> Self {
        Self {
            kind: Kind::Symlink,
            hash: hash_hex(target.as_os_str().as_bytes()),
            mode: SYMLINK_MODE,
        }
    }

    /// S as recorded in `state.toml`.
    pub fn from_entry(entry: &Entry) -> Result<Self> {
        let mode = match entry.kind {
            Kind::File => parse_mode(&entry.mode)?,
            Kind::Symlink => SYMLINK_MODE,
        };
        Ok(Self {
            kind: entry.kind,
            hash: entry.hash.clone(),
            mode,
        })
    }

    /// The state entry recording this tuple as applied from `source`.
    #[must_use]
    pub fn entry(&self, source: &str) -> Entry {
        Entry {
            source: source.to_owned(),
            kind: self.kind,
            hash: self.hash.clone(),
            mode: format!("{:04o}", self.mode),
        }
    }
}

/// H: what `$HOME` holds at a key, read without following any symlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Home {
    /// Nothing at the key, or a parent directory is missing.
    Missing,
    /// A regular file or symlink.
    Present(Tuple),
    /// Something dotman never removes or writes through is in the way.
    Blocked(Obstacle),
}

/// Why a key in `$HOME` is blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Obstacle {
    /// Never removed, even when empty.
    Directory,
    /// A socket, FIFO or device.
    Special,
    /// This key prefix is a symlink or not a directory.
    Parent(String),
}

/// Reads H for `key` below `home`.
pub fn observe(home: &Path, key: &str) -> Result<Home> {
    Ok(read_node(home, key)?.home())
}

/// Like [`observe`], but a symlink's tuple is computed from [`portable_link_target`], so an
/// absolute in-`$HOME` link compares equal to the relative form `add` stores.
pub fn observe_portable(home: &Path, home_real: Option<&Path>, key: &str) -> Result<Home> {
    Ok(match read_node(home, key)? {
        Node::Symlink(target) => Home::Present(Tuple::of_link(&portable_link_target(
            &target, key, home, home_real,
        ))),
        node => node.home(),
    })
}

/// An absolute target inside `$HOME` becomes relative to the link's parent, so the repo is portable
/// across home directories. Anything else, `..` components included, is kept verbatim.
#[must_use]
pub fn portable_link_target(
    target: &Path,
    key: &str,
    home: &Path,
    home_real: Option<&Path>,
) -> PathBuf {
    if !target.is_absolute() {
        return target.to_path_buf();
    }
    let Some(inside) = std::iter::once(home)
        .chain(home_real)
        .find_map(|root| target.strip_prefix(root).ok())
    else {
        return target.to_path_buf();
    };
    if inside
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return target.to_path_buf();
    }
    let mut relative = PathBuf::new();
    for _ in parents(key) {
        relative.push("..");
    }
    relative.push(inside);
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}

pub enum Node {
    Missing,
    BadParent(String),
    Dir,
    Special,
    File {
        bytes: Vec<u8>,
        mode: u32,
        nlink: u64,
        uid: u32,
        gid: u32,
    },
    Symlink(PathBuf),
}

impl Node {
    pub(crate) fn home(&self) -> Home {
        match self {
            Self::Missing => Home::Missing,
            Self::BadParent(parent) => Home::Blocked(Obstacle::Parent(parent.clone())),
            Self::Dir => Home::Blocked(Obstacle::Directory),
            Self::Special => Home::Blocked(Obstacle::Special),
            Self::File { bytes, mode, .. } => Home::Present(Tuple::of_file(bytes, *mode)),
            Self::Symlink(target) => Home::Present(Tuple::of_link(target)),
        }
    }
}

// Every component is lstat'ed, so neither a parent symlink nor one at the key is followed.
// The key is validated here because joining an absolute key would discard `root`.
pub fn read_node(root: &Path, key: &str) -> Result<Node> {
    validate_key(key)?;
    for parent in parents(key) {
        match fs::symlink_metadata(root.join(parent)) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(Node::BadParent(parent.to_owned())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Node::Missing),
            Err(e) => return Err(e).with_context(|| format!("cannot stat {parent}")),
        }
    }
    let path = root.join(key);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Node::Missing),
        Err(e) => return Err(e).with_context(|| format!("cannot stat {}", path.display())),
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        let target =
            fs::read_link(&path).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Node::Symlink(target))
    } else if file_type.is_dir() {
        Ok(Node::Dir)
    } else if file_type.is_file() {
        read_file(&path).with_context(|| format!("cannot read {}", path.display()))
    } else {
        Ok(Node::Special)
    }
}

fn read_file(path: &Path) -> Result<Node> {
    // O_NONBLOCK so that a file swapped for a FIFO after the lstat cannot hang the open.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "it changed while being read");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Node::File {
        bytes,
        mode: meta.mode() & 0o7777,
        nlink: meta.nlink(),
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

/// Outermost first: `a/b/c` gives `a`, then `a/b`.
pub fn parents(key: &str) -> impl Iterator<Item = &str> {
    key.match_indices('/').filter_map(|(at, _)| key.get(..at))
}

pub fn hash_hex(bytes: &[u8]) -> String {
    format!("{:032x}", xxh3_128(bytes))
}

// A hostile repo must never get setuid, setgid or sticky bits into `$HOME`.
pub const fn safe_mode(mode: u32) -> u32 {
    mode & 0o777
}

/// The `.dotman.toml` `[modes]` value for `key`; directory keys end with `/`.
pub fn recorded_mode(modes: &BTreeMap<String, String>, key: &str) -> Result<Option<u32>> {
    modes
        .get(key)
        .map(|value| parse_mode(value).with_context(|| format!("[modes] entry `{key}`")))
        .transpose()
}

// `.dotman.toml` is shared, so a malformed mode is an error rather than a default.
pub fn parse_mode(text: &str) -> Result<u32> {
    if text.is_empty() || !text.bytes().all(|b| matches!(b, b'0'..=b'7')) {
        bail!("`{text}` is not an octal mode");
    }
    u32::from_str_radix(text, 8)
        .ok()
        .filter(|mode| *mode <= 0o7777)
        .ok_or_else(|| anyhow!("`{text}` is above 7777"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn parents_lists_every_prefix() {
        assert_eq!(parents("a/b/c").collect::<Vec<_>>(), ["a", "a/b"]);
        assert_eq!(parents("a").count(), 0);
    }

    #[test]
    fn modes_parse_strictly() {
        assert_eq!(parse_mode("0644").unwrap(), 0o644);
        assert_eq!(parse_mode("4755").unwrap(), 0o4755);
        for bad in ["", "+644", "0o644", "0899", "17777", " 644", "-1"] {
            assert!(parse_mode(bad).is_err(), "{bad}");
        }
        let modes = BTreeMap::from([(".ssh/".to_owned(), "0700".to_owned())]);
        assert_eq!(recorded_mode(&modes, ".ssh/").unwrap(), Some(0o700));
        assert_eq!(recorded_mode(&modes, ".ssh").unwrap(), None);
    }

    #[test]
    fn state_entries_round_trip_and_symlink_modes_are_ignored() {
        let file = Tuple::of_file(b"x", 0o600);
        let entry = file.entry(".bashrc");
        assert_eq!(entry.mode, "0600");
        assert_eq!(Tuple::from_entry(&entry).unwrap(), file);

        let link = Tuple::of_link(Path::new("../target"));
        let mut entry = link.entry(".link");
        entry.mode = "0755".to_owned();
        assert_eq!(Tuple::from_entry(&entry).unwrap(), link);
    }

    #[test]
    fn absolute_targets_inside_home_become_relative() {
        let home = Path::new("/home/u");
        let rel = |t: &str, key: &str| portable_link_target(Path::new(t), key, home, None);
        assert_eq!(
            rel("/home/u/.config/a", ".local/l"),
            Path::new("../.config/a")
        );
        assert_eq!(rel("/home/u/x", "l"), Path::new("x"));
        assert_eq!(rel("/home/u", ".l/k"), Path::new(".."));
        assert_eq!(rel("/home/u", "l"), Path::new("."));
        assert_eq!(rel("/etc/hosts", "l"), Path::new("/etc/hosts"));
        assert_eq!(rel("/home/u/../v/x", "l"), Path::new("/home/u/../v/x"));
        assert_eq!(rel("../x", "a/l"), Path::new("../x"));
    }

    #[test]
    fn observe_portable_equates_absolute_and_relative_links() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".config")).unwrap();
        fs::write(home.join(".bashrc"), b"x").unwrap();
        symlink(home.join(".bashrc"), home.join(".config/abs")).unwrap();
        symlink("../.bashrc", home.join(".config/rel")).unwrap();
        symlink("/etc/hostname", home.join(".config/out")).unwrap();

        let abs = observe_portable(&home, None, ".config/abs").unwrap();
        let rel = observe_portable(&home, None, ".config/rel").unwrap();
        assert_eq!(abs, rel);
        assert_eq!(abs, Home::Present(Tuple::of_link(Path::new("../.bashrc"))));
        assert_eq!(
            observe_portable(&home, None, ".config/out").unwrap(),
            Home::Present(Tuple::of_link(Path::new("/etc/hostname")))
        );
        assert_eq!(
            observe_portable(&home, None, ".bashrc").unwrap(),
            observe(&home, ".bashrc").unwrap()
        );
    }

    #[test]
    fn observe_never_follows_symlinks() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(home.join("dir")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("x"), b"secret").unwrap();
        symlink(&outside, home.join(".config")).unwrap();
        symlink("missing-target", home.join("dangling")).unwrap();
        fs::write(home.join("file"), b"").unwrap();

        assert_eq!(
            observe(&home, ".config/x").unwrap(),
            Home::Blocked(Obstacle::Parent(".config".to_owned()))
        );
        assert_eq!(
            observe(&home, "file/x").unwrap(),
            Home::Blocked(Obstacle::Parent("file".to_owned()))
        );
        assert_eq!(
            observe(&home, ".config").unwrap(),
            Home::Present(Tuple::of_link(&outside))
        );
        assert_eq!(
            observe(&home, "dangling").unwrap(),
            Home::Present(Tuple::of_link(Path::new("missing-target")))
        );
        assert_eq!(
            observe(&home, "dir").unwrap(),
            Home::Blocked(Obstacle::Directory)
        );
        assert_eq!(observe(&home, "none/x").unwrap(), Home::Missing);
        assert!(observe(&home, "/etc/passwd").is_err());
        assert!(observe(&home, "../outside/x").is_err());
    }
}
