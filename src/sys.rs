//! Platform-specific calls. The only module allowed to use `unsafe`.

use std::ffi::{CStr, CString, OsStr, c_char};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

/// Upper bound for the `*_r` lookup buffers, so a misbehaving NSS module cannot make us grow forever.
const MAX_LOOKUP_BUF: usize = 1 << 20;

/// Restores the default SIGPIPE action so a closed pipe ends the process quietly.
#[cfg(unix)]
pub fn restore_sigpipe() {
    // SAFETY: `signal` with SIG_DFL installs no handler; called once at startup before any threads.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Puts back the actions that [`ignore_signals`] replaced, when dropped.
#[cfg(unix)]
pub struct SignalsIgnored(Vec<(libc::c_int, libc::sighandler_t)>);

/// Ignores `signals` until the value is dropped. `SIG_IGN` survives exec, so a child spawned
/// meanwhile would ignore them too; spawn it first.
#[cfg(unix)]
#[must_use]
pub fn ignore_signals(signals: &[libc::c_int]) -> SignalsIgnored {
    SignalsIgnored(
        signals
            .iter()
            .map(|&signal| {
                // SAFETY: `signal` with SIG_IGN installs no handler; the previous action is kept
                // to be put back.
                (signal, unsafe { libc::signal(signal, libc::SIG_IGN) })
            })
            .collect(),
    )
}

/// Ignores SIGPIPE until the value is dropped. Meanwhile a write to a child that has already
/// exited fails with EPIPE instead of killing the process.
#[cfg(unix)]
#[must_use]
pub fn ignore_sigpipe() -> SignalsIgnored {
    ignore_signals(&[libc::SIGPIPE])
}

#[cfg(unix)]
impl Drop for SignalsIgnored {
    fn drop(&mut self) {
        for &(signal, previous) in &self.0 {
            if previous == libc::SIG_ERR {
                continue;
            }
            // SAFETY: `previous` came from `signal` for this signal, so it is a valid action for it.
            unsafe {
                libc::signal(signal, previous);
            }
        }
    }
}

/// Effective user id of the process.
#[must_use]
pub fn geteuid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// Whether a process with id `pid` exists, as `kill(pid, 0)` tells: one owned by another user
/// counts. A `pid` that cannot name a single process counts as alive, so nothing is ever taken
/// from a live one.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: signal 0 sends nothing; the call only checks that `pid` exists and may be signalled.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Opens the directory `name` inside `dir`. A symlink is never followed: it fails with ELOOP, or
/// ENOTDIR on some systems.
pub(crate) fn openat_dir(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open descriptor and `name` is NUL-terminated; both outlive the call.
    owned(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) })
}

/// Creates `name` inside `dir` for writing at `mode`. Fails if anything is there, a symlink
/// included.
pub(crate) fn create_excl(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> io::Result<File> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: as in `openat_dir`; with O_CREAT, openat reads one more variadic argument, the mode,
    // which is passed as an unsigned int as C's default promotion of mode_t expects.
    owned(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) }).map(File::from)
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a non-negative return from open is a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Symlink,
    Other,
}

/// What `name` inside `dir` is, without following a symlink; `None` when nothing is there.
pub(crate) fn kind_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<Option<EntryKind>> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `stat` is writable for a whole `stat`; `dir` is open and `name` NUL-terminated, and
    // all three outlive the call.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    match check(rc) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    // SAFETY: zeroed memory is a valid `stat`, and fstatat succeeded, so it is filled in.
    let stat = unsafe { stat.assume_init() };
    Ok(Some(match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => EntryKind::File,
        libc::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }))
}

/// Changes the owner and group of an open file.
pub(crate) fn fchown(file: BorrowedFd<'_>, uid: u32, gid: u32) -> io::Result<()> {
    std::os::unix::fs::fchown(file, Some(uid), Some(gid))
}

/// Renames `from` to `to`, both inside `dir`, replacing `to` atomically.
pub(crate) fn renameat(dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> io::Result<()> {
    let fd = dir.as_raw_fd();
    // SAFETY: `dir` is open and both names are NUL-terminated; all outlive the call.
    check(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) })
}

/// Removes the file `name` inside `dir`.
pub(crate) fn unlinkat(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
    // SAFETY: `dir` is open and `name` is NUL-terminated; both outlive the call.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) })
}

/// Flushes a directory, so that a rename inside it survives a crash.
pub(crate) fn fsync_dir(dir: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: `fsync` only takes the open descriptor `dir`.
    check(unsafe { libc::fsync(dir.as_raw_fd()) })
}

/// The node name `uname -n` prints, unshortened. `None` when the call fails or the name does not
/// fit the buffer.
#[must_use]
pub fn hostname() -> Option<String> {
    // Linux caps host names at 64 bytes and macOS at 255, both below this.
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for `buf.len()` bytes and outlives the call; on success the
    // result is NUL-terminated unless it was truncated, which is checked below.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0)?;
    Some(String::from_utf8_lossy(buf.get(..end)?).into_owned())
}

/// The hardware name `uname -m` prints, such as `x86_64`, or `arm64` on Apple silicon. `None`
/// when the call fails.
#[must_use]
pub fn machine() -> Option<String> {
    let mut name = MaybeUninit::<libc::utsname>::zeroed();
    // SAFETY: `name` is writable for a whole `utsname` and outlives the call.
    if unsafe { libc::uname(name.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: zeroed memory is a valid `utsname`, which holds only `c_char` arrays, and `uname`
    // succeeded, so the fields hold its strings.
    let name = unsafe { name.assume_init() };
    // `c_char` is `i8` on x86_64 and `u8` on aarch64 Linux.
    let bytes: Vec<u8> = name
        .machine
        .iter()
        .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    let end = bytes.iter().position(|b| *b == 0)?;
    Some(String::from_utf8_lossy(bytes.get(..end)?).into_owned())
}

/// Unix seconds of a wall-clock time in the local time zone, daylight saving time included, as
/// `mktime` reads it. `None` when the time is before 1970 or the C library cannot represent it.
#[must_use]
pub fn local_epoch(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<u64> {
    let field = |value: u32, base: u32| {
        value
            .checked_sub(base)
            .and_then(|v| libc::c_int::try_from(v).ok())
    };
    // SAFETY: zeroed memory is a valid `tm`: integers, plus on some platforms a zone-name pointer,
    // for which null is allowed.
    let mut tm: libc::tm = unsafe { MaybeUninit::<libc::tm>::zeroed().assume_init() };
    tm.tm_year = field(year, 1900)?;
    tm.tm_mon = field(month, 1)?;
    tm.tm_mday = field(day, 0)?;
    tm.tm_hour = field(hour, 0)?;
    tm.tm_min = field(minute, 0)?;
    tm.tm_sec = field(second, 0)?;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is initialized and outlives the call; mktime normalizes its fields in place and
    // keeps no pointer to it.
    let secs = unsafe { libc::mktime(&raw mut tm) };
    u64::try_from(secs).ok()
}

/// A passwd database entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passwd {
    /// `pw_name`, converted lossily to UTF-8.
    pub name: String,
    /// `pw_uid`.
    pub uid: u32,
    /// `pw_dir` as raw bytes; not required to be UTF-8.
    pub home: PathBuf,
}

/// A group database entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// `gr_name`, converted lossily to UTF-8.
    pub name: String,
    /// `gr_gid`.
    pub gid: u32,
}

/// Looks up a user by id. `None` covers both "no such user" and lookup failure, since a static
/// musl build cannot see LDAP or systemd-homed users.
#[must_use]
pub fn passwd_by_uid(uid: u32) -> Option<Passwd> {
    lookup_passwd(|pwd, buf, len, out| {
        // SAFETY: `pwd`, `buf` and `out` come from `lookup_passwd`: `buf` is `len` writable
        // bytes and all three outlive the call.
        unsafe { libc::getpwuid_r(uid, pwd, buf, len, out) }
    })
}

/// Looks up a user by name. `None` for an unknown name or one containing a NUL byte.
#[must_use]
pub fn passwd_by_name(name: &str) -> Option<Passwd> {
    let name = CString::new(name).ok()?;
    lookup_passwd(|pwd, buf, len, out| {
        // SAFETY: `name` is NUL-terminated and outlives the call; the other pointers are
        // as in `passwd_by_uid`.
        unsafe { libc::getpwnam_r(name.as_ptr(), pwd, buf, len, out) }
    })
}

/// Looks up a group by name. `None` for an unknown name or one containing a NUL byte.
#[must_use]
pub fn group_by_name(name: &str) -> Option<Group> {
    let name = CString::new(name).ok()?;
    lookup_group(|grp, buf, len, out| {
        // SAFETY: `name` is NUL-terminated and outlives the call; the other pointers come from
        // `lookup_group`: `buf` is `len` writable bytes and all three outlive the call.
        unsafe { libc::getgrnam_r(name.as_ptr(), grp, buf, len, out) }
    })
}

/// Looks up a group by id. `None` covers both "no such group" and lookup failure.
#[must_use]
pub fn group_by_gid(gid: u32) -> Option<Group> {
    lookup_group(|grp, buf, len, out| {
        // SAFETY: the pointers come from `lookup_group`: `buf` is `len` writable bytes and all
        // three outlive the call.
        unsafe { libc::getgrgid_r(gid, grp, buf, len, out) }
    })
}

fn lookup_group(
    call: impl Fn(*mut libc::group, *mut c_char, usize, *mut *mut libc::group) -> i32,
) -> Option<Group> {
    let mut buf_len = initial_buf_len(libc::_SC_GETGR_R_SIZE_MAX);
    loop {
        let mut buf: Vec<c_char> = vec![0; buf_len];
        let mut grp = MaybeUninit::<libc::group>::zeroed();
        let mut out: *mut libc::group = ptr::null_mut();
        let rc = call(grp.as_mut_ptr(), buf.as_mut_ptr(), buf.len(), &raw mut out);
        if rc == libc::ERANGE {
            buf_len = grow(buf_len)?;
            continue;
        }
        if rc != 0 || out.is_null() {
            return None;
        }
        // SAFETY: on success `grp` is initialised and `gr_name` points to a NUL-terminated
        // string inside `buf`, which is alive here.
        let (gr_name, gid) = unsafe {
            let grp = grp.assume_init_ref();
            (CStr::from_ptr(grp.gr_name), grp.gr_gid)
        };
        return Some(Group {
            name: gr_name.to_string_lossy().into_owned(),
            gid,
        });
    }
}

fn lookup_passwd(
    call: impl Fn(*mut libc::passwd, *mut c_char, usize, *mut *mut libc::passwd) -> i32,
) -> Option<Passwd> {
    let mut buf_len = initial_buf_len(libc::_SC_GETPW_R_SIZE_MAX);
    loop {
        let mut buf: Vec<c_char> = vec![0; buf_len];
        let mut pwd = MaybeUninit::<libc::passwd>::zeroed();
        let mut out: *mut libc::passwd = ptr::null_mut();
        let rc = call(pwd.as_mut_ptr(), buf.as_mut_ptr(), buf.len(), &raw mut out);
        if rc == libc::ERANGE {
            buf_len = grow(buf_len)?;
            continue;
        }
        if rc != 0 || out.is_null() {
            return None;
        }
        // SAFETY: on success `pwd` is initialised and `pw_name` and `pw_dir` point to
        // NUL-terminated strings inside `buf`, which is alive here.
        let (pw_name, pw_dir, uid) = unsafe {
            let pwd = pwd.assume_init_ref();
            (
                CStr::from_ptr(pwd.pw_name),
                CStr::from_ptr(pwd.pw_dir),
                pwd.pw_uid,
            )
        };
        return Some(Passwd {
            name: pw_name.to_string_lossy().into_owned(),
            uid,
            home: PathBuf::from(OsStr::from_bytes(pw_dir.to_bytes())),
        });
    }
}

fn initial_buf_len(name: libc::c_int) -> usize {
    // SAFETY: `sysconf` reads no caller memory; an unknown name yields -1.
    let hint = unsafe { libc::sysconf(name) };
    usize::try_from(hint)
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1024)
}

fn grow(len: usize) -> Option<usize> {
    len.checked_mul(2).filter(|n| *n <= MAX_LOOKUP_BUF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_resolves_by_uid_and_name() {
        let euid = geteuid();
        let Some(by_uid) = passwd_by_uid(euid) else {
            return;
        };
        assert_eq!(by_uid.uid, euid);
        assert!(by_uid.home.is_absolute());
        let by_name = passwd_by_name(&by_uid.name).expect("lookup by name");
        assert_eq!(by_name.uid, euid);
    }

    #[test]
    fn hostname_matches_uname() {
        let uname = crate::proc::command("uname")
            .arg("-n")
            .output()
            .expect("run uname");
        assert_eq!(
            hostname().as_deref(),
            Some(String::from_utf8_lossy(&uname.stdout).trim())
        );
    }

    #[test]
    fn machine_matches_uname() {
        let uname = crate::proc::command("uname")
            .arg("-m")
            .output()
            .expect("run uname");
        assert_eq!(
            machine().as_deref(),
            Some(String::from_utf8_lossy(&uname.stdout).trim())
        );
    }

    #[test]
    fn root_group_has_gid_zero() {
        if let Some(group) = group_by_name("root") {
            assert_eq!(group.gid, 0);
            assert_eq!(group_by_gid(0).map(|g| g.name).as_deref(), Some("root"));
        }
    }

    #[test]
    fn unknown_and_nul_names_are_none() {
        assert_eq!(passwd_by_name("no-such-user-dotman"), None);
        assert_eq!(passwd_by_name("a\0b"), None);
        assert_eq!(group_by_name("no-such-group-dotman"), None);
    }

    #[test]
    fn buffer_growth_is_bounded() {
        assert_eq!(grow(1024), Some(2048));
        assert_eq!(grow(MAX_LOOKUP_BUF), None);
    }

    extern "C" fn noop(_: libc::c_int) {}

    fn action(signal: libc::c_int) -> libc::sighandler_t {
        let mut current = MaybeUninit::<libc::sigaction>::zeroed();
        // SAFETY: a null new action only reads the current one into `current`, which is writable.
        let rc = unsafe { libc::sigaction(signal, ptr::null(), current.as_mut_ptr()) };
        assert_eq!(rc, 0);
        // SAFETY: zeroed memory is a valid `sigaction`, and the call filled it in.
        unsafe { current.assume_init() }.sa_sigaction
    }

    // SIGUSR1 and SIGUSR2 are touched by nothing else, so parallel tests cannot race on them.
    #[test]
    fn ignored_signals_get_their_previous_action_back() {
        let handler = noop as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: `noop` is a valid handler that does nothing.
        unsafe { libc::signal(libc::SIGUSR1, handler) };
        let default = action(libc::SIGUSR2);
        {
            let _ignored = ignore_signals(&[libc::SIGUSR1, libc::SIGUSR2]);
            assert_eq!(action(libc::SIGUSR1), libc::SIG_IGN);
            assert_eq!(action(libc::SIGUSR2), libc::SIG_IGN);
        }
        assert_eq!(action(libc::SIGUSR1), handler);
        assert_eq!(action(libc::SIGUSR2), default);
        // SAFETY: SIG_DFL installs no handler.
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };
    }
}
