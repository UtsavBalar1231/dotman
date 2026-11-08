//! The per-user process lock, so two `dotman` runs never write at once.

use crate::output;
use crate::paths::create_private_dir;
use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Held for as long as the value lives. The lock is an `flock` on the open file, so dropping
/// the value releases it.
#[derive(Debug)]
pub struct Lock {
    file: Option<File>,
}

impl Lock {
    /// Takes the lock without waiting.
    ///
    /// A dry run only contends on a lock file that already exists: it creates neither the file
    /// nor its directory. A filesystem that cannot lock (NFS without lockd, for example) warns and
    /// proceeds unlocked.
    pub fn acquire(path: &Path, dry_run: bool) -> Result<Self> {
        let open = if dry_run {
            OpenOptions::new().read(true).open(path)
        } else {
            if let Some(dir) = path.parent() {
                create_private_dir(dir, false)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)
        };
        let file = match open {
            Ok(file) => file,
            Err(e) if dry_run && e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self { file: None });
            }
            Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
        };
        match file.try_lock_exclusive() {
            Ok(true) => Ok(Self { file: Some(file) }),
            Ok(false) => bail!("another {} process is running", crate::name::bin()),
            Err(e) if cannot_lock(&e) => {
                output::warning(&format!(
                    "{} does not support locking; continuing without the lock",
                    path.display()
                ));
                Ok(Self { file: None })
            }
            Err(e) => Err(e).with_context(|| format!("cannot lock {}", path.display())),
        }
    }

    /// Whether this value holds the lock; false after a dry run with no lock file or an unsupported filesystem.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        self.file.is_some()
    }
}

fn cannot_lock(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(code) if code == libc::ENOLCK || code == libc::EOPNOTSUPP || code == libc::ENOTSUP
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    // Parallel tests fork `git`; until the child execs it still shares the open file description,
    // so a released lock can stay held for a few milliseconds.
    fn acquire_after_release(path: &Path, dry_run: bool) -> Lock {
        for _ in 0..1000 {
            if let Ok(lock) = Lock::acquire(path, dry_run) {
                return lock;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Lock::acquire(path, dry_run).expect("lock never released")
    }

    #[test]
    fn two_separately_opened_handles_contend() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state/lock");
        let first = Lock::acquire(&path, false).unwrap();
        assert!(first.is_held());
        let err = Lock::acquire(&path, false).unwrap_err().to_string();
        assert_eq!(err, "another dotman process is running");
        drop(first);
        assert!(acquire_after_release(&path, false).is_held());
    }

    #[test]
    fn dry_run_contends_only_on_an_existing_lock_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state/lock");
        let dry = Lock::acquire(&path, true).unwrap();
        assert!(!dry.is_held());
        assert!(!tmp.path().join("state").exists());

        let real = Lock::acquire(&path, false).unwrap();
        assert!(Lock::acquire(&path, true).is_err());
        drop(real);
        assert!(acquire_after_release(&path, true).is_held());
    }

    #[test]
    fn unsupported_locking_errors_are_recognised() {
        assert!(cannot_lock(&io::Error::from_raw_os_error(libc::ENOLCK)));
        assert!(cannot_lock(&io::Error::from_raw_os_error(libc::EOPNOTSUPP)));
        assert!(!cannot_lock(&io::Error::from_raw_os_error(libc::EACCES)));
    }
}
