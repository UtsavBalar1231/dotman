//! Platform-specific calls. The only module allowed to use `unsafe`.

use std::ffi::{CStr, CString, OsStr, c_char};
use std::mem::MaybeUninit;
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

/// Puts back the SIGPIPE action that [`ignore_sigpipe`] replaced, when dropped.
#[cfg(unix)]
pub struct SigpipeIgnored(libc::sighandler_t);

/// Ignores SIGPIPE until the value is dropped. Meanwhile a write to a child that has already
/// exited fails with EPIPE instead of killing the process.
#[cfg(unix)]
#[must_use]
pub fn ignore_sigpipe() -> SigpipeIgnored {
    // SAFETY: `signal` with SIG_IGN installs no handler; the previous action is kept to be put back.
    SigpipeIgnored(unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) })
}

#[cfg(unix)]
impl Drop for SigpipeIgnored {
    fn drop(&mut self) {
        if self.0 == libc::SIG_ERR {
            return;
        }
        // SAFETY: the value came from `signal` for SIGPIPE, so it is a valid action for it.
        unsafe {
            libc::signal(libc::SIGPIPE, self.0);
        }
    }
}

/// Effective user id of the process.
#[must_use]
pub fn geteuid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
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
    let mut buf_len = initial_buf_len(libc::_SC_GETGR_R_SIZE_MAX);
    loop {
        let mut buf: Vec<c_char> = vec![0; buf_len];
        let mut grp = MaybeUninit::<libc::group>::zeroed();
        let mut out: *mut libc::group = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; `grp` is zeroed, which is valid for `group`'s
        // integer and pointer fields; `buf` has `buf.len()` writable bytes; all outlive the call.
        let rc = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                grp.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut out,
            )
        };
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
}
