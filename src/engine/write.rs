//! The only path that writes into `$HOME`: no write through a symlink, a backup before local
//! changes are replaced, and a temp file renamed into place after re-reading the target.

use super::{Home, Node, Obstacle, parents, read_node, recorded_mode, safe_mode};
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

const TEMP_PREFIX: &str = ".dotman-tmp-";
const LINKS_FILE: &str = "links.txt";

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
    /// The backup file, or `links.txt` when the old target was a symlink.
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
    write_node(home, key, New::File { bytes, mode }, opts)
}

/// Points `key` below `home` at `target`, stored verbatim, replacing a file or symlink there.
pub fn safe_symlink(
    home: &Path,
    key: &str,
    target: &Path,
    opts: &WriteOptions<'_>,
) -> Result<WriteOutcome> {
    write_node(home, key, New::Link(target), opts)
}

/// `<backups>/<UTC timestamp>-<pid>`, the directory for one run's backups, created on first use.
/// The timestamp is `YYYYMMDDTHHMMSSZ`.
pub fn backup_run_dir(backups: &Path, now: SystemTime) -> Result<PathBuf> {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .context("the system clock is before 1970")?
        .as_secs();
    let stamp = utc_stamp(secs).ok_or_else(|| anyhow!("the system clock is out of range"))?;
    Ok(backups.join(format!("{stamp}-{}", std::process::id())))
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
) -> Result<WriteOutcome> {
    let current = read_node(home, key)?;
    let refusal = match &current {
        Node::BadParent(parent) => Some(Skip::Blocked(Obstacle::Parent(parent.clone()))),
        Node::Dir => Some(Skip::Blocked(Obstacle::Directory)),
        Node::Special => Some(Skip::Blocked(Obstacle::Special)),
        Node::Symlink(_) if matches!(new, New::File { .. }) => Some(Skip::Symlink),
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

/// Copies a file at the target to `<dir>/<key>`, or records a symlink in `<dir>/links.txt`.
/// Returns where the copy went; a dry run only reports it.
fn back_up(dir: &Path, key: &str, current: &Node, dry_run: bool) -> Result<Option<PathBuf>> {
    let (dest, record) = match current {
        Node::File { bytes, .. } => (dir.join(key), bytes.clone()),
        // One `key<TAB>target` line per link. `escape_ascii` keeps tabs, newlines and
        // non-UTF-8 targets from breaking a line, and is lossless.
        Node::Symlink(target) => (
            dir.join(LINKS_FILE),
            format!(
                "{}\t{}\n",
                key.as_bytes().escape_ascii(),
                target.as_os_str().as_bytes().escape_ascii()
            )
            .into_bytes(),
        ),
        Node::Missing | Node::BadParent(_) | Node::Dir | Node::Special => return Ok(None),
    };
    if dry_run {
        return Ok(Some(dest));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", dest.display()))?;
    create_private_dir(parent, false)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let mut open = OpenOptions::new();
    open.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    // A second backup of the same key in one run must never overwrite the first, which holds
    // the user's original.
    if matches!(current, Node::File { .. }) {
        open.write(true).create_new(true);
    } else {
        open.append(true).create(true);
    }
    let mut file = open
        .open(&dest)
        .with_context(|| format!("cannot create backup {}", dest.display()))?;
    file.write_all(&record)?;
    file.sync_all()?;
    sync_dir(parent)?;
    Ok(Some(dest))
}

fn make_temp(dir: &Path, new: New<'_>) -> Result<TempPath> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(TEMP_PREFIX);
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
        let links = f.run_dir.join("links.txt");
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
        for dir in ["", ".config", ".config/nvim"] {
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
            written(Some(f.run_dir.join("links.txt")))
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
}
