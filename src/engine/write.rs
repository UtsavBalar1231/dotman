//! The only path that writes into `$HOME`: no write through a symlink, a backup before anything is
//! replaced, and a temp file renamed into place after re-reading the target.

use super::{Home, Node, Obstacle, Tuple, parents, read_node, recorded_mode, safe_mode};
use crate::paths::create_private_dir;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempPath;
use walkdir::WalkDir;

const TEMP_PREFIX: &str = ".dotman-tmp-";

// The PID lets a later run tell a killed run's leftovers from a live run's files.
fn temp_prefix() -> String {
    format!("{TEMP_PREFIX}{}-", std::process::id())
}
// Under `.dotman/`, a reserved root name, so no key's backup can land on the record.
const LINKS_FILE: &str = ".dotman/links.txt";
const MODES_FILE: &str = ".dotman/modes.txt";

/// How one write behaves.
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions<'a> {
    /// Check and report only: no directory, temp file or backup is created.
    pub dry_run: bool,
    /// Rewrite the file in place when rename fails with EBUSY or EXDEV (`--in-place`).
    pub in_place: bool,
    /// The run's directory from [`backup_run_dir`]; `None` replaces the target without a backup.
    pub backup: Option<&'a Path>,
    /// H as classified. The target is read again before the rename, and a mismatch skips the
    /// write as [`Skip::Changed`].
    pub expected: &'a Home,
    /// `.dotman.toml` `[modes]`. Its `dir/` entries give the modes of parents that have to be
    /// created; others get 0755.
    pub modes: &'a BTreeMap<String, String>,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The target holds the new content; in a dry run, it would.
    Written(Written),
    /// `$HOME` is unchanged.
    Skipped(Skip),
}

/// Details of a write that happened, or would happen in a dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The backup file, or `.dotman/links.txt` in the run dir when the old target was a symlink.
    pub backup: Option<PathBuf>,
    /// Rename was impossible, so the file was rewritten in place and kept its inode.
    pub in_place: bool,
    /// `st_nlink` of the replaced file, when above 1 and the file was renamed over.
    pub hard_links: Option<u64>,
}

impl Written {
    /// A warning for the user, if the write has one.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        self.hard_links
            .map(|n| format!("had {n} hard links; the other links keep the old content"))
    }
}

/// Why `$HOME` was left unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// The target no longer matches what classification saw: a conflict.
    Changed,
    /// A symlink is where a regular file was to be written: a conflict.
    Symlink,
    /// The target or one of its parents is in the way.
    Blocked(Obstacle),
    /// Rename failed with EBUSY or EXDEV, as on a single-file bind mount.
    Busy,
    /// Rename failed with EPERM, as for an immutable file.
    Protected,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Changed => f.write_str("changed since it was checked; skipped as a conflict"),
            Self::Symlink => {
                f.write_str("is a symlink, which is never written through; skipped as a conflict")
            }
            Self::Blocked(Obstacle::Directory) => f.write_str("a directory is in the way; skipped"),
            Self::Blocked(Obstacle::Special) => {
                f.write_str("a socket, FIFO or device is in the way; skipped")
            }
            Self::Blocked(Obstacle::Parent(parent)) => {
                write!(f, "`{parent}` is a symlink or not a directory; skipped")
            }
            Self::Busy => f.write_str(
                "cannot be replaced by rename (a bind mount?); skipped, `--in-place` rewrites it in place",
            ),
            Self::Protected => {
                f.write_str("protected, skipped; if it is immutable, run `chattr -i` on it first")
            }
        }
    }
}

/// Writes `bytes` to `key` below `home`, with `mode` minus setuid, setgid and sticky.
pub fn safe_write(
    home: &Path,
    key: &str,
    bytes: &[u8],
    mode: u32,
    opts: &WriteOptions<'_>,
) -> Result<WriteOutcome> {
    let mode = safe_mode(mode);
    write_node(home, key, New::File { bytes, mode }, opts, false)
}

/// Like [`safe_write`], and also replaces a symlink at `key` that `opts.expected` names. The
/// rename lands on the link itself, so its target is never written.
pub fn safe_write_over_link(
    home: &Path,
    key: &str,
    bytes: &[u8],
    mode: u32,
    opts: &WriteOptions<'_>,
) -> Result<WriteOutcome> {
    let mode = safe_mode(mode);
    write_node(home, key, New::File { bytes, mode }, opts, true)
}

/// Points `key` below `home` at `target`, stored verbatim, replacing a file or symlink there.
pub fn safe_symlink(
    home: &Path,
    key: &str,
    target: &Path,
    opts: &WriteOptions<'_>,
) -> Result<WriteOutcome> {
    write_node(home, key, New::Link(target), opts, false)
}

/// `<backups>/<UTC timestamp>-<pid>`, the directory for one run's backups, created on first use.
/// The timestamp is `YYYYMMDDTHHMMSSZ`. When that directory exists, as for `sync`'s second pass
/// in the same second, a `-2`, `-3` and so on is added, so a key backed up by both passes keeps
/// both copies.
pub fn backup_run_dir(backups: &Path, now: SystemTime) -> Result<PathBuf> {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .context("the system clock is before 1970")?
        .as_secs();
    let stamp = utc_stamp(secs).ok_or_else(|| anyhow!("the system clock is out of range"))?;
    let base = format!("{stamp}-{}", std::process::id());
    std::iter::once(backups.join(&base))
        .chain((2_u32..=u32::MAX).map(|n| backups.join(format!("{base}-{n}"))))
        .find(|dir| fs::symlink_metadata(dir).is_err())
        .ok_or_else(|| anyhow!("no free backup directory name under {}", backups.display()))
}

/// Appends `key<TAB>target` to `<dir>/.dotman/links.txt`, the record a restore reads, and returns
/// the file.
pub fn record_link(dir: &Path, key: &str, target: &Path) -> Result<PathBuf> {
    back_up(dir, key, &Node::Symlink(target.to_path_buf()), false)?
        .ok_or_else(|| anyhow!("nothing to record for {key}"))
}

/// One entry of a tree that takes the place of a link to a directory, keyed by its path below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Member {
    /// A regular file.
    File {
        /// Its content.
        bytes: Vec<u8>,
        /// Its mode before special bits are masked off.
        mode: u32,
    },
    /// A symlink, stored verbatim.
    Link(PathBuf),
}

/// What a restore found at the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// The link is back.
    Back,
    /// The link was never replaced.
    Untouched,
    /// Something other than what the swap wrote is there, so it was left alone.
    Kept,
}

fn expect_link(home: &Path, key: &str, expected: &Path) -> Result<()> {
    match read_node(home, key)? {
        Node::Symlink(target) if target == expected => Ok(()),
        _ => bail!("{key} changed since it was checked; left alone"),
    }
}

fn parent_of(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))
}

/// Replaces the symlink at `key` that has the target text `expected` with a regular file. The
/// rename deliberately lands on the link itself: it replaces the link and never writes through it.
pub fn replace_link_with_file(
    home: &Path,
    key: &str,
    expected: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    expect_link(home, key, expected)?;
    let path = home.join(key);
    let dir = parent_of(&path)?;
    let temp = make_temp(
        dir,
        New::File {
            bytes,
            mode: safe_mode(mode),
        },
    )?;
    expect_link(home, key, expected)?;
    temp.persist(&path)
        .map_err(|e| e.error)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    sync_dir(dir)
}

/// Replaces the symlink at `key` that has the target text `expected` with a directory holding
/// `members`. `modes` gives the modes of the directories, which are 0755 where it is silent.
pub fn replace_link_with_dir(
    home: &Path,
    key: &str,
    expected: &Path,
    members: &BTreeMap<String, Member>,
    modes: &BTreeMap<String, String>,
) -> Result<()> {
    expect_link(home, key, expected)?;
    let path = home.join(key);
    let dir = parent_of(&path)?;
    let temp = tempfile::Builder::new()
        .prefix(&temp_prefix())
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(dir)
        .with_context(|| format!("cannot create a temp dir in {}", dir.display()))?;
    let staged = temp.path().to_path_buf();
    populate(&staged, key, members, modes)?;
    expect_link(home, key, expected)?;

    // rename(2) cannot replace a non-directory with a directory, so the link goes aside first.
    let aside = aside_path(&path)?;
    fs::rename(&path, &aside).with_context(|| format!("cannot move {} aside", path.display()))?;
    let staged = temp.keep();
    if let Err(e) = fs::rename(&staged, &path) {
        let _ = fs::rename(&aside, &path);
        let _ = fs::remove_dir_all(&staged);
        return Err(e)
            .with_context(|| format!("cannot move the new directory to {}", path.display()));
    }
    // The tree is in place; a leftover aside link is only clutter.
    let _ = fs::remove_file(&aside);
    sync_dir(dir)
}

fn aside_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no name", path.display()))?;
    let mut aside = name.to_os_string();
    aside.push(format!(".dotman-swap-{}", std::process::id()));
    Ok(parent_of(path)?.join(aside))
}

fn populate(
    root: &Path,
    key: &str,
    members: &BTreeMap<String, Member>,
    modes: &BTreeMap<String, String>,
) -> Result<()> {
    // Directories stay 0700 until everything is in, so a recorded 0555 cannot block its own files.
    let mut dirs: BTreeMap<String, u32> = BTreeMap::new();
    dirs.insert(
        String::new(),
        safe_mode(recorded_mode(modes, &format!("{key}/"))?.unwrap_or(0o755)),
    );
    for (rel, member) in members {
        for parent in parents(rel) {
            if !dirs.contains_key(parent) {
                fs::create_dir(root.join(parent))
                    .with_context(|| format!("cannot create {key}/{parent}"))?;
                let mode = recorded_mode(modes, &format!("{key}/{parent}/"))?.unwrap_or(0o755);
                dirs.insert(parent.to_owned(), safe_mode(mode));
            }
        }
        let path = root.join(rel);
        match member {
            Member::File { bytes, mode } => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .with_context(|| format!("cannot create {key}/{rel}"))?;
                file.write_all(bytes)?;
                file.set_permissions(Permissions::from_mode(safe_mode(*mode)))?;
                file.sync_all()?;
            }
            Member::Link(target) => symlink(target, &path)
                .with_context(|| format!("cannot create the symlink {key}/{rel}"))?,
        }
    }
    for (rel, mode) in dirs.iter().rev() {
        let dir = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        sync_dir(&dir)?;
        fs::set_permissions(&dir, Permissions::from_mode(*mode))?;
    }
    Ok(())
}

/// Puts the symlink `target` back at `key`, over the file the swap wrote there. A file that is
/// not exactly what the swap wrote is left alone.
pub fn restore_file_link(
    home: &Path,
    key: &str,
    target: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<Restored> {
    match read_node(home, key)? {
        Node::Symlink(now) if now == target => return Ok(Restored::Untouched),
        Node::File { .. } => {}
        _ => return Ok(Restored::Kept),
    }
    let expected = Home::Present(Tuple::of_file(bytes, safe_mode(mode)));
    let modes = BTreeMap::new();
    let opts = WriteOptions {
        dry_run: false,
        in_place: false,
        backup: None,
        expected: &expected,
        modes: &modes,
    };
    Ok(match safe_symlink(home, key, target, &opts)? {
        WriteOutcome::Written(_) => Restored::Back,
        WriteOutcome::Skipped(_) => Restored::Kept,
    })
}

/// Puts the symlink `target` back at `key`, over the directory the swap wrote there. A directory
/// that holds anything other than `members` is left alone.
pub fn restore_dir_link(
    home: &Path,
    key: &str,
    target: &Path,
    members: &BTreeMap<String, Member>,
) -> Result<Restored> {
    match read_node(home, key)? {
        Node::Symlink(now) if now == target => return Ok(Restored::Untouched),
        Node::Dir => {}
        _ => return Ok(Restored::Kept),
    }
    let path = home.join(key);
    if !holds_exactly(&path, members) {
        return Ok(Restored::Kept);
    }
    let dir = parent_of(&path)?;
    let link = tempfile::Builder::new()
        .prefix(&temp_prefix())
        .make_in(dir, |temp| symlink(target, temp))
        .with_context(|| format!("cannot create a temp link in {}", dir.display()))?
        .into_temp_path();
    let aside = aside_path(&path)?;
    fs::rename(&path, &aside).with_context(|| format!("cannot move {} aside", path.display()))?;
    if let Err(e) = link.persist(&path) {
        let _ = fs::rename(&aside, &path);
        return Err(e.error).with_context(|| format!("cannot restore {}", path.display()));
    }
    fs::remove_dir_all(&aside)
        .with_context(|| format!("the link is back, but {} is left", aside.display()))?;
    sync_dir(dir)?;
    Ok(Restored::Back)
}

/// Whether the tree at `dir` holds the files and links of `members` and nothing else.
pub fn holds_exactly(dir: &Path, members: &BTreeMap<String, Member>) -> bool {
    let mut seen = 0usize;
    for entry in WalkDir::new(dir).follow_links(false).min_depth(1) {
        let Ok(entry) = entry else { return false };
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        let Some(rel) = entry.path().strip_prefix(dir).ok().and_then(Path::to_str) else {
            return false;
        };
        let same = match (members.get(rel), file_type.is_symlink()) {
            (Some(Member::Link(target)), true) => {
                fs::read_link(entry.path()).is_ok_and(|now| now == *target)
            }
            (Some(Member::File { bytes, .. }), false) if file_type.is_file() => {
                fs::read(entry.path()).is_ok_and(|now| now == *bytes)
            }
            _ => false,
        };
        if !same {
            return false;
        }
        seen = seen.saturating_add(1);
    }
    seen == members.len()
}

#[derive(Clone, Copy)]
enum New<'a> {
    File { bytes: &'a [u8], mode: u32 },
    Link(&'a Path),
}

fn write_node(
    home: &Path,
    key: &str,
    new: New<'_>,
    opts: &WriteOptions<'_>,
    over_link: bool,
) -> Result<WriteOutcome> {
    let current = read_node(home, key)?;
    let refusal = match &current {
        Node::BadParent(parent) => Some(Skip::Blocked(Obstacle::Parent(parent.clone()))),
        Node::Dir => Some(Skip::Blocked(Obstacle::Directory)),
        Node::Special => Some(Skip::Blocked(Obstacle::Special)),
        Node::Symlink(_) if matches!(new, New::File { .. }) && !over_link => Some(Skip::Symlink),
        Node::Missing | Node::Symlink(_) | Node::File { .. } => None,
    };
    if let Some(skip) = refusal {
        return Ok(WriteOutcome::Skipped(skip));
    }
    let hard_links = match current {
        Node::File { nlink, .. } if nlink > 1 => Some(nlink),
        _ => None,
    };
    if opts.dry_run {
        if current.home() != *opts.expected {
            return Ok(WriteOutcome::Skipped(Skip::Changed));
        }
        let backup = match opts.backup {
            Some(dir) => back_up(dir, key, &current, true)?,
            None => None,
        };
        return Ok(WriteOutcome::Written(Written {
            backup,
            in_place: false,
            hard_links,
        }));
    }

    create_parents(home, key, opts.modes)?;
    let backup = match opts.backup {
        Some(dir) => back_up(dir, key, &current, false)?,
        None => None,
    };
    let path = home.join(key);
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    // In the target's own directory, so the rename never crosses filesystems.
    let temp = make_temp(dir, new)?;

    // Classification may be long past; whatever reached the target since then is the user's.
    if read_node(home, key)?.home() != *opts.expected {
        return Ok(WriteOutcome::Skipped(Skip::Changed));
    }
    let in_place = match temp.persist(&path) {
        Ok(()) => false,
        Err(e) => match (rename_failure(&e.error), new) {
            (Some(Skip::Busy), New::File { bytes, mode }) if opts.in_place => {
                rewrite_in_place(&path, bytes, mode)
                    .with_context(|| format!("cannot rewrite {} in place", path.display()))?;
                true
            }
            (Some(skip), _) => return Ok(WriteOutcome::Skipped(skip)),
            (None, _) => {
                return Err(e.error).with_context(|| format!("cannot replace {}", path.display()));
            }
        },
    };
    sync_dir(dir)?;
    Ok(WriteOutcome::Written(Written {
        backup,
        in_place,
        hard_links: hard_links.filter(|_| !in_place),
    }))
}

fn rename_failure(err: &io::Error) -> Option<Skip> {
    match err.raw_os_error()? {
        libc::EBUSY | libc::EXDEV => Some(Skip::Busy),
        libc::EPERM => Some(Skip::Protected),
        _ => None,
    }
}

fn create_parents(home: &Path, key: &str, modes: &BTreeMap<String, String>) -> Result<()> {
    for parent in parents(key) {
        let dir = home.join(parent);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!(
                "{} stopped being a directory during the write",
                dir.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mode = safe_mode(recorded_mode(modes, &format!("{parent}/"))?.unwrap_or(0o755));
                create_dir(&dir, mode)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("cannot stat {}", dir.display())),
        }
    }
    Ok(())
}

fn create_dir(dir: &Path, mode: u32) -> io::Result<()> {
    DirBuilder::new().mode(mode).create(dir)?;
    // The umask may have narrowed `mode`. The no-follow handle keeps a directory swapped for a
    // symlink in the meantime from passing the chmod on to its target.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(dir)?
        .set_permissions(Permissions::from_mode(mode))
}

/// Copies `bytes` to `<dir>/<rel>` at 0600, creating 0700 directories, and never replaces a file
/// already there.
pub fn back_up_bytes(dir: &Path, rel: &str, bytes: &[u8]) -> Result<PathBuf> {
    let dest = dir.join(rel);
    write_backup(&dest, bytes, false)?;
    Ok(dest)
}

/// Copies a file at the target to `<dir>/<key>` and records its mode in `<dir>/.dotman/modes.txt`,
/// or records a symlink in `<dir>/.dotman/links.txt`. Returns where the copy or record went; a dry
/// run only reports it.
fn back_up(dir: &Path, key: &str, current: &Node, dry_run: bool) -> Result<Option<PathBuf>> {
    // One `key<TAB>value` line per entry. `escape_ascii` keeps tabs, newlines and non-UTF-8 text
    // from breaking a line, and is lossless.
    let line = |value: &[u8]| {
        format!(
            "{}\t{}\n",
            key.as_bytes().escape_ascii(),
            value.escape_ascii()
        )
        .into_bytes()
    };
    match current {
        Node::File { bytes, mode, .. } => {
            let dest = dir.join(key);
            if !dry_run {
                write_backup(&dest, bytes, false)?;
                write_backup(
                    &dir.join(MODES_FILE),
                    &line(format!("{mode:04o}").as_bytes()),
                    true,
                )?;
            }
            Ok(Some(dest))
        }
        Node::Symlink(target) => {
            let dest = dir.join(LINKS_FILE);
            if !dry_run {
                write_backup(&dest, &line(target.as_os_str().as_bytes()), true)?;
            }
            Ok(Some(dest))
        }
        Node::Missing | Node::BadParent(_) | Node::Dir | Node::Special => Ok(None),
    }
}

/// Writes `bytes` to `dest` at 0600 under 0700 directories. `append` adds to a record file; a copy
/// is created new, so a second backup of the same key in one run never overwrites the first, which
/// holds the user's original.
fn write_backup(dest: &Path, bytes: &[u8], append: bool) -> Result<()> {
    let parent = parent_of(dest)?;
    create_private_dir(parent, false)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let mut open = OpenOptions::new();
    open.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    if append {
        open.append(true).create(true);
    } else {
        open.write(true).create_new(true);
    }
    let mut file = open
        .open(dest)
        .with_context(|| format!("cannot create backup {}", dest.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    sync_dir(parent)
}

fn make_temp(dir: &Path, new: New<'_>) -> Result<TempPath> {
    let prefix = temp_prefix();
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix);
    let temp = match new {
        New::File { bytes, mode } => {
            // 0600 at open, so private or decrypted content is never readable by others, even
            // briefly; the final mode is set after the bytes are in.
            let mut temp = builder
                .permissions(Permissions::from_mode(0o600))
                .tempfile_in(dir)
                .with_context(|| format!("cannot create a temp file in {}", dir.display()))?;
            temp.write_all(bytes)?;
            temp.as_file()
                .set_permissions(Permissions::from_mode(mode))?;
            temp.as_file().sync_all()?;
            temp.into_temp_path()
        }
        New::Link(target) => builder
            .make_in(dir, |path| symlink(target, path))
            .with_context(|| format!("cannot create a temp link in {}", dir.display()))?
            .into_temp_path(),
    };
    Ok(temp)
}

fn rewrite_in_place(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("it is no longer a regular file");
    }
    // The mode goes first so that new private content never sits under an older, wider mode.
    file.set_permissions(Permissions::from_mode(mode))?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("cannot fsync {}", dir.display()))
}

fn utc_stamp(secs: u64) -> Option<String> {
    let days = secs / 86_400;
    let time = secs % 86_400;
    // Howard Hinnant's civil_from_days, for days counted from 1970-01-01.
    let z = days.checked_add(719_468)?;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = doe
        .checked_sub(doe / 1_460)?
        .checked_add(doe / 36_524)?
        .checked_sub(doe / 146_096)?
        / 365;
    let doy = doe.checked_sub(
        yoe.checked_mul(365)?
            .checked_add(yoe / 4)?
            .checked_sub(yoe / 100)?,
    )?;
    let mp = doy.checked_mul(5)?.checked_add(2)? / 153;
    let day = doy
        .checked_sub(mp.checked_mul(153)?.checked_add(2)? / 5)?
        .checked_add(1)?;
    let (month, next_year) = if mp < 10 {
        (mp.checked_add(3)?, 0)
    } else {
        (mp.checked_sub(9)?, 1)
    };
    let year = era
        .checked_mul(400)?
        .checked_add(yoe)?
        .checked_add(next_year)?;
    Some(format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Tuple, hash_hex, observe};
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;
    use tempfile::TempDir;
    use walkdir::WalkDir;

    struct Fixture {
        tmp: TempDir,
        home: PathBuf,
        run_dir: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let run_dir =
            backup_run_dir(&tmp.path().join("state/dotman/backups"), SystemTime::now()).unwrap();
        Fixture { tmp, home, run_dir }
    }

    struct Call<'a> {
        backup: Option<&'a Path>,
        dry_run: bool,
        modes: &'a BTreeMap<String, String>,
    }

    const PLAIN: Call<'static> = Call {
        backup: None,
        dry_run: false,
        modes: &BTreeMap::new(),
    };

    fn options<'a>(call: &Call<'a>, expected: &'a Home) -> WriteOptions<'a> {
        WriteOptions {
            dry_run: call.dry_run,
            in_place: false,
            backup: call.backup,
            expected,
            modes: call.modes,
        }
    }

    fn write(home: &Path, key: &str, bytes: &[u8], mode: u32, call: &Call<'_>) -> WriteOutcome {
        let expected = observe(home, key).unwrap();
        safe_write(home, key, bytes, mode, &options(call, &expected)).unwrap()
    }

    fn link(home: &Path, key: &str, target: &str, call: &Call<'_>) -> WriteOutcome {
        let expected = observe(home, key).unwrap();
        safe_symlink(home, key, Path::new(target), &options(call, &expected)).unwrap()
    }

    fn written(backup: Option<PathBuf>) -> WriteOutcome {
        WriteOutcome::Written(Written {
            backup,
            in_place: false,
            hard_links: None,
        })
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn tree(root: &Path) -> BTreeMap<PathBuf, (u32, Option<String>)> {
        WalkDir::new(root)
            .into_iter()
            .map(|entry| {
                let path = entry.unwrap().into_path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let content = if meta.is_symlink() {
                    Some(fs::read_link(&path).unwrap().display().to_string())
                } else if meta.is_file() {
                    Some(hash_hex(&fs::read(&path).unwrap()))
                } else {
                    None
                };
                (path, (meta.mode(), content))
            })
            .collect()
    }

    #[test]
    fn writes_with_recorded_parent_modes_and_leaves_no_temp_file() {
        let f = fixture();
        let modes = BTreeMap::from([(".ssh/".to_owned(), "0700".to_owned())]);
        let call = Call {
            modes: &modes,
            ..PLAIN
        };
        assert_eq!(
            write(&f.home, ".ssh/config", b"Host *\n", 0o600, &call),
            written(None)
        );
        assert_eq!(
            write(&f.home, ".config/a/b.conf", b"b", 0o644, &call),
            written(None)
        );
        assert_eq!(fs::read(f.home.join(".ssh/config")).unwrap(), b"Host *\n");
        assert_eq!(mode_of(&f.home.join(".ssh")), 0o700);
        assert_eq!(mode_of(&f.home.join(".config")), 0o755);
        assert_eq!(mode_of(&f.home.join(".config/a")), 0o755);
        assert_eq!(names(&f.home.join(".ssh")), ["config"]);

        assert_eq!(
            write(&f.home, ".ssh/config", b"two", 0o600, &call),
            written(None)
        );
        assert_eq!(fs::read(f.home.join(".ssh/config")).unwrap(), b"two");
        assert_eq!(names(&f.home.join(".ssh")), ["config"]);
    }

    #[test]
    fn special_bits_are_masked_and_modes_are_exact() {
        let f = fixture();
        write(&f.home, "suid", b"x", 0o4755, &PLAIN);
        write(&f.home, "sticky", b"x", 0o1777, &PLAIN);
        write(&f.home, "private", b"x", 0o600, &PLAIN);
        assert_eq!(mode_of(&f.home.join("suid")), 0o755);
        assert_eq!(mode_of(&f.home.join("sticky")), 0o777);
        assert_eq!(mode_of(&f.home.join("private")), 0o600);
        write(&f.home, "private", b"y", 0o640, &PLAIN);
        assert_eq!(mode_of(&f.home.join("private")), 0o640);

        let modes = BTreeMap::from([("d/".to_owned(), "2775".to_owned())]);
        write(
            &f.home,
            "d/x",
            b"x",
            0o644,
            &Call {
                modes: &modes,
                ..PLAIN
            },
        );
        assert_eq!(mode_of(&f.home.join("d")), 0o775);
    }

    #[test]
    fn invalid_keys_are_rejected() {
        let f = fixture();
        let before = tree(f.tmp.path());
        let opts = options(&PLAIN, &Home::Missing);
        for key in [
            "../x",
            "/abs",
            ".git/x",
            ".dotman/x",
            "install.sh",
            "a/../b",
            "",
            "a//b",
        ] {
            assert!(
                safe_write(&f.home, key, b"x", 0o644, &opts).is_err(),
                "{key}"
            );
            assert!(
                safe_symlink(&f.home, key, Path::new("t"), &opts).is_err(),
                "{key}"
            );
        }
        assert_eq!(tree(f.tmp.path()), before);
    }

    #[test]
    fn a_parent_symlink_pointing_outside_is_refused() {
        let f = fixture();
        let outside = f.tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("kept"), b"k").unwrap();
        symlink(&outside, f.home.join(".config")).unwrap();
        let before = tree(f.tmp.path());

        let refused = WriteOutcome::Skipped(Skip::Blocked(Obstacle::Parent(".config".to_owned())));
        assert_eq!(
            write(&f.home, ".config/app/x", b"x", 0o644, &PLAIN),
            refused
        );
        assert_eq!(link(&f.home, ".config/x", "t", &PLAIN), refused);
        for expected in [Home::Missing, Home::Present(Tuple::of_file(b"k", 0o644))] {
            let opts = options(&PLAIN, &expected);
            assert_eq!(
                safe_write(&f.home, ".config/kept", b"x", 0o644, &opts).unwrap(),
                refused
            );
        }
        assert_eq!(tree(f.tmp.path()), before);
    }

    #[test]
    fn a_directory_in_the_way_is_left_alone() {
        let f = fixture();
        fs::create_dir_all(f.home.join(".bashrc/inner")).unwrap();
        let before = tree(f.tmp.path());
        let blocked = WriteOutcome::Skipped(Skip::Blocked(Obstacle::Directory));
        assert_eq!(write(&f.home, ".bashrc", b"x", 0o644, &PLAIN), blocked);
        assert_eq!(link(&f.home, ".bashrc", "t", &PLAIN), blocked);
        assert_eq!(tree(f.tmp.path()), before);
    }

    #[test]
    fn a_target_symlink_is_never_written_through_but_can_be_replaced_by_a_link() {
        let f = fixture();
        let outside = f.tmp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, f.home.join(".bashrc")).unwrap();
        let before = tree(f.tmp.path());
        assert_eq!(
            write(&f.home, ".bashrc", b"x", 0o644, &PLAIN),
            WriteOutcome::Skipped(Skip::Symlink)
        );
        assert_eq!(tree(f.tmp.path()), before);

        let call = Call {
            backup: Some(&f.run_dir),
            ..PLAIN
        };
        let links = f.run_dir.join(".dotman/links.txt");
        assert_eq!(
            link(&f.home, ".bashrc", "../new", &call),
            written(Some(links.clone()))
        );
        assert_eq!(
            fs::read_link(f.home.join(".bashrc")).unwrap(),
            Path::new("../new")
        );
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_eq!(mode_of(&links), 0o600);
        assert_eq!(mode_of(&f.run_dir.join(".dotman")), 0o700);
        assert_eq!(names(&f.home), [".bashrc"]);

        symlink("a\tb\nc\u{e9}", f.home.join(".odd")).unwrap();
        link(&f.home, ".odd", "x", &call);
        let records = format!(
            ".bashrc\t{}\n.odd\ta\\tb\\nc\\xc3\\xa9\n",
            outside.display()
        );
        assert_eq!(fs::read_to_string(&links).unwrap(), records);
    }

    #[test]
    fn an_expected_link_is_replaced_by_a_file_and_its_target_is_never_written() {
        let f = fixture();
        let outside = f.tmp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, f.home.join(".rc")).unwrap();
        let call = Call {
            backup: Some(&f.run_dir),
            ..PLAIN
        };
        let stale = Home::Present(Tuple::of_link(Path::new("elsewhere")));
        assert_eq!(
            safe_write_over_link(&f.home, ".rc", b"x", 0o644, &options(&PLAIN, &stale)).unwrap(),
            WriteOutcome::Skipped(Skip::Changed)
        );
        assert!(
            fs::symlink_metadata(f.home.join(".rc"))
                .unwrap()
                .is_symlink()
        );

        let expected = observe(&f.home, ".rc").unwrap();
        let links = f.run_dir.join(".dotman/links.txt");
        assert_eq!(
            safe_write_over_link(&f.home, ".rc", b"new", 0o644, &options(&call, &expected))
                .unwrap(),
            written(Some(links.clone()))
        );
        assert_eq!(fs::read(f.home.join(".rc")).unwrap(), b"new");
        assert_eq!(mode_of(&f.home.join(".rc")), 0o644);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_eq!(
            fs::read_to_string(&links).unwrap(),
            format!(".rc\t{}\n", outside.display())
        );
        assert_eq!(names(&f.home), [".rc"]);
    }

    #[test]
    fn a_links_txt_key_and_a_link_record_back_up_to_distinct_files() {
        let f = fixture();
        let original = b"the user's own links.txt\n";
        fs::write(f.home.join("links.txt"), original).unwrap();
        symlink("old", f.home.join(".link")).unwrap();
        let call = Call {
            backup: Some(&f.run_dir),
            ..PLAIN
        };
        let copy = f.run_dir.join("links.txt");
        let record = f.run_dir.join(".dotman/links.txt");
        assert_eq!(
            write(&f.home, "links.txt", b"repo", 0o644, &call),
            written(Some(copy.clone()))
        );
        assert_eq!(
            link(&f.home, ".link", "new", &call),
            written(Some(record.clone()))
        );
        assert_eq!(fs::read(&copy).unwrap(), original);
        assert_eq!(fs::read_to_string(&record).unwrap(), ".link\told\n");

        let second = f.tmp.path().join("state/dotman/backups/second");
        let call = Call {
            backup: Some(&second),
            ..PLAIN
        };
        link(&f.home, ".link", "newer", &call);
        write(&f.home, "links.txt", b"repo 2", 0o644, &call);
        assert_eq!(fs::read(second.join("links.txt")).unwrap(), b"repo");
        assert_eq!(
            fs::read_to_string(second.join(".dotman/links.txt")).unwrap(),
            ".link\tnew\n"
        );
    }

    #[test]
    fn a_link_replaces_a_file_after_backing_it_up() {
        let f = fixture();
        fs::write(f.home.join(".vimrc"), b"mine").unwrap();
        let call = Call {
            backup: Some(&f.run_dir),
            ..PLAIN
        };
        let backup = f.run_dir.join(".vimrc");
        assert_eq!(
            link(&f.home, ".vimrc", "/abs/target", &call),
            written(Some(backup.clone()))
        );
        assert_eq!(
            fs::read_link(f.home.join(".vimrc")).unwrap(),
            Path::new("/abs/target")
        );
        assert_eq!(fs::read(backup).unwrap(), b"mine");
    }

    #[test]
    fn hard_links_are_warned_about() {
        let f = fixture();
        fs::write(f.home.join("a"), b"old").unwrap();
        fs::hard_link(f.home.join("a"), f.home.join("b")).unwrap();
        let outcome = write(&f.home, "a", b"new", 0o644, &PLAIN);
        let WriteOutcome::Written(done) = outcome else {
            unreachable!("{outcome:?}")
        };
        assert_eq!(done.hard_links, Some(2));
        let warning = done.warning().unwrap();
        assert!(warning.contains("hard links"), "{warning}");
        assert_eq!(fs::read(f.home.join("a")).unwrap(), b"new");
        assert_eq!(fs::read(f.home.join("b")).unwrap(), b"old");
        assert_eq!(write(&f.home, "a", b"newer", 0o644, &PLAIN), written(None));
    }

    #[test]
    fn backups_follow_the_plan_layout() {
        let root = Path::new("/state/dotman/backups");
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let expected = format!("20231114T221320Z-{}", std::process::id());
        assert_eq!(backup_run_dir(root, at).unwrap(), root.join(expected));

        let f = fixture();
        let name = f.run_dir.file_name().unwrap().to_str().unwrap();
        let (stamp, pid) = name.split_once('-').unwrap();
        assert_eq!(stamp.len(), 16);
        assert!(
            stamp.ends_with('Z') && stamp.chars().nth(8) == Some('T'),
            "{stamp}"
        );
        assert_eq!(pid.parse::<u32>().unwrap(), std::process::id());

        let key = ".config/nvim/init.lua";
        let call = Call {
            backup: Some(&f.run_dir),
            ..PLAIN
        };
        write(&f.home, key, b"original", 0o640, &PLAIN);
        let dest = f.run_dir.join(key);
        assert_eq!(
            write(&f.home, key, b"repo", 0o644, &call),
            written(Some(dest.clone()))
        );
        assert_eq!(fs::read(&dest).unwrap(), b"original");
        assert_eq!(mode_of(&dest), 0o600);
        let modes = f.run_dir.join(".dotman/modes.txt");
        assert_eq!(
            fs::read_to_string(&modes).unwrap(),
            format!("{key}\t0640\n")
        );
        assert_eq!(mode_of(&modes), 0o600);
        for dir in ["", ".config", ".config/nvim", ".dotman"] {
            assert_eq!(mode_of(&f.run_dir.join(dir)), 0o700, "{dir}");
        }
        assert_eq!(fs::read(f.home.join(key)).unwrap(), b"repo");

        let expected = observe(&f.home, key).unwrap();
        assert!(safe_write(&f.home, key, b"again", 0o644, &options(&call, &expected)).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"original");
        assert_eq!(write(&f.home, "new", b"n", 0o644, &call), written(None));
    }

    #[test]
    fn utc_stamps_are_civil_dates() {
        assert_eq!(utc_stamp(0).unwrap(), "19700101T000000Z");
        assert_eq!(utc_stamp(951_782_400).unwrap(), "20000229T000000Z");
        assert_eq!(utc_stamp(1_700_000_000).unwrap(), "20231114T221320Z");
        assert_eq!(utc_stamp(4_107_542_399).unwrap(), "21000228T235959Z");
        assert_eq!(utc_stamp(253_402_300_799).unwrap(), "99991231T235959Z");
    }

    #[test]
    fn a_target_changed_after_classification_is_a_conflict() {
        let f = fixture();
        let key = ".bashrc";
        fs::write(f.home.join(key), b"classified").unwrap();
        let stale = observe(&f.home, key).unwrap();
        fs::write(f.home.join(key), b"edited since").unwrap();
        for dry_run in [true, false] {
            let call = Call {
                dry_run,
                backup: Some(&f.run_dir),
                ..PLAIN
            };
            let opts = options(&call, &stale);
            assert_eq!(
                safe_write(&f.home, key, b"repo", 0o644, &opts).unwrap(),
                WriteOutcome::Skipped(Skip::Changed)
            );
        }
        let opts = options(&PLAIN, &stale);
        assert_eq!(
            safe_symlink(&f.home, key, Path::new("t"), &opts).unwrap(),
            WriteOutcome::Skipped(Skip::Changed)
        );
        assert_eq!(fs::read(f.home.join(key)).unwrap(), b"edited since");
        assert_eq!(names(&f.home), [key]);

        let opts = options(&PLAIN, &Home::Missing);
        fs::write(f.home.join("appeared"), b"new").unwrap();
        assert_eq!(
            safe_write(&f.home, "appeared", b"repo", 0o644, &opts).unwrap(),
            WriteOutcome::Skipped(Skip::Changed)
        );
        assert_eq!(fs::read(f.home.join("appeared")).unwrap(), b"new");
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let f = fixture();
        fs::write(f.home.join(".bashrc"), b"mine").unwrap();
        symlink("old", f.home.join(".link")).unwrap();
        fs::hard_link(f.home.join(".bashrc"), f.home.join(".bashrc.hard")).unwrap();
        let before = tree(f.tmp.path());
        let call = Call {
            dry_run: true,
            backup: Some(&f.run_dir),
            ..PLAIN
        };

        assert_eq!(
            write(&f.home, ".bashrc", b"repo", 0o600, &call),
            WriteOutcome::Written(Written {
                backup: Some(f.run_dir.join(".bashrc")),
                in_place: false,
                hard_links: Some(2),
            })
        );
        assert_eq!(
            write(&f.home, ".config/new/file", b"n", 0o644, &call),
            written(None)
        );
        assert_eq!(
            link(&f.home, ".link", "new", &call),
            written(Some(f.run_dir.join(".dotman/links.txt")))
        );
        assert_eq!(link(&f.home, ".local/bin/x", "t", &call), written(None));

        assert_eq!(tree(f.tmp.path()), before);
        assert!(!f.home.join(".config").exists());
        assert!(!f.tmp.path().join("state").exists());
    }

    #[test]
    fn rename_errors_map_to_skips() {
        let skip = |errno| rename_failure(&io::Error::from_raw_os_error(errno));
        assert_eq!(skip(libc::EBUSY), Some(Skip::Busy));
        assert_eq!(skip(libc::EXDEV), Some(Skip::Busy));
        assert_eq!(skip(libc::EPERM), Some(Skip::Protected));
        assert_eq!(skip(libc::EACCES), None);
        assert!(Skip::Protected.to_string().contains("protected, skipped"));
        assert!(Skip::Protected.to_string().contains("chattr -i"));
        assert!(Skip::Busy.to_string().contains("--in-place"));
    }

    #[test]
    fn in_place_rewrites_keep_the_inode() {
        let f = fixture();
        let path = f.home.join("mounted");
        fs::write(&path, b"old content").unwrap();
        fs::hard_link(&path, f.home.join("other")).unwrap();
        rewrite_in_place(&path, b"new", 0o600).unwrap();
        assert_eq!(fs::read(f.home.join("other")).unwrap(), b"new");
        assert_eq!(mode_of(&path), 0o600);

        symlink(&path, f.home.join("link")).unwrap();
        assert!(rewrite_in_place(&f.home.join("link"), b"x", 0o644).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    fn members(files: &[(&str, &[u8])]) -> BTreeMap<String, Member> {
        files
            .iter()
            .map(|(rel, bytes)| {
                (
                    (*rel).to_owned(),
                    Member::File {
                        bytes: bytes.to_vec(),
                        mode: 0o644,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_link_is_replaced_by_a_file_and_the_target_is_never_written() {
        let f = fixture();
        fs::write(f.tmp.path().join("farm"), b"farm").unwrap();
        symlink("../farm", f.home.join(".rc")).unwrap();
        // The link text no longer matches, as if the link had been swapped for another.
        assert!(replace_link_with_file(&f.home, ".rc", Path::new("other"), b"x", 0o644).is_err());
        assert!(
            fs::symlink_metadata(f.home.join(".rc"))
                .unwrap()
                .is_symlink()
        );

        replace_link_with_file(&f.home, ".rc", Path::new("../farm"), b"new", 0o4755).unwrap();
        assert_eq!(fs::read(f.home.join(".rc")).unwrap(), b"new");
        assert_eq!(mode_of(&f.home.join(".rc")), 0o755);
        assert_eq!(fs::read(f.tmp.path().join("farm")).unwrap(), b"farm");
        assert_eq!(names(&f.home), [".rc"]);
        assert!(replace_link_with_file(&f.home, ".rc", Path::new("../farm"), b"x", 0o644).is_err());
    }

    #[test]
    fn a_file_is_restored_to_its_link_only_while_it_is_what_the_swap_wrote() {
        let f = fixture();
        let target = Path::new("../farm");
        symlink(target, f.home.join(".rc")).unwrap();
        assert_eq!(
            restore_file_link(&f.home, ".rc", target, b"new", 0o644).unwrap(),
            Restored::Untouched
        );
        replace_link_with_file(&f.home, ".rc", target, b"new", 0o644).unwrap();
        fs::write(f.home.join(".rc"), b"edited").unwrap();
        assert_eq!(
            restore_file_link(&f.home, ".rc", target, b"new", 0o644).unwrap(),
            Restored::Kept
        );
        assert_eq!(fs::read(f.home.join(".rc")).unwrap(), b"edited");

        fs::write(f.home.join(".rc"), b"new").unwrap();
        fs::set_permissions(f.home.join(".rc"), Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            restore_file_link(&f.home, ".rc", target, b"new", 0o644).unwrap(),
            Restored::Back
        );
        assert_eq!(fs::read_link(f.home.join(".rc")).unwrap(), target);
        assert_eq!(names(&f.home), [".rc"]);
    }

    #[test]
    fn a_link_to_a_directory_becomes_a_tree_and_comes_back_only_if_it_is_unchanged() {
        let f = fixture();
        let target = Path::new("../farm/nvim");
        symlink(target, f.home.join("nvim")).unwrap();
        let tree_of = members(&[("init.lua", b"one"), ("lua/opts.lua", b"two")]);
        let modes = BTreeMap::from([("nvim/lua/".to_owned(), "0700".to_owned())]);

        assert!(replace_link_with_dir(&f.home, "nvim", Path::new("x"), &tree_of, &modes).is_err());
        replace_link_with_dir(&f.home, "nvim", target, &tree_of, &modes).unwrap();
        assert_eq!(fs::read(f.home.join("nvim/lua/opts.lua")).unwrap(), b"two");
        assert_eq!(mode_of(&f.home.join("nvim")), 0o755);
        assert_eq!(mode_of(&f.home.join("nvim/lua")), 0o700);
        assert_eq!(names(&f.home), ["nvim"]);
        assert!(holds_exactly(&f.home.join("nvim"), &tree_of));

        fs::write(f.home.join("nvim/extra"), b"mine").unwrap();
        assert_eq!(
            restore_dir_link(&f.home, "nvim", target, &tree_of).unwrap(),
            Restored::Kept
        );
        fs::remove_file(f.home.join("nvim/extra")).unwrap();
        assert_eq!(
            restore_dir_link(&f.home, "nvim", target, &tree_of).unwrap(),
            Restored::Back
        );
        assert_eq!(fs::read_link(f.home.join("nvim")).unwrap(), target);
        assert_eq!(names(&f.home), ["nvim"]);
    }

    #[test]
    fn a_link_record_keeps_the_exact_target_text() {
        let f = fixture();
        let file = record_link(&f.run_dir, "a\tb", Path::new("t\narget")).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a\\tb\tt\\narget\n");
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(mode_of(file.parent().unwrap()), 0o700);
    }
}
