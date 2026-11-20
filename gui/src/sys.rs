//! Process primitives confined to the GUI's Linux child runner.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

pub fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and has no preconditions.
    unsafe { libc::geteuid() }
}

pub fn new_session(command: &mut Command) {
    // SAFETY: The closure calls only async-signal-safe setsid and reads errno on failure.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

pub fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: fcntl operates on the live borrowed descriptor and does not retain it.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL takes an integer flag value and the descriptor remains borrowed.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn wait_readable(
    stdout: Option<BorrowedFd<'_>>,
    stderr: Option<BorrowedFd<'_>>,
    stdin: Option<BorrowedFd<'_>>,
) -> io::Result<bool> {
    let mut fds = [
        (stdout, libc::POLLIN),
        (stderr, libc::POLLIN),
        (stdin, libc::POLLOUT),
    ]
    .map(|(fd, events)| libc::pollfd {
        fd: fd.map_or(-1, |fd| fd.as_raw_fd()),
        events,
        revents: 0,
    });
    // SAFETY: The array is initialized and writable for its full length. All
    // descriptors remain borrowed and live for the call; poll retains neither.
    let result = unsafe { libc::poll(fds.as_mut_ptr(), 3, 20) };
    if result == -1 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        };
    }
    if fds.iter().any(|fd| {
        fd.revents & libc::POLLNVAL != 0
            || (fd.events == libc::POLLIN && fd.revents & libc::POLLERR != 0)
    }) {
        return Err(io::Error::other("child output readiness failed"));
    }
    Ok(result > 0)
}

pub fn exited(child: &Child) -> io::Result<bool> {
    let pid = libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::other("invalid child process ID"))?;
    if pid <= 1 {
        return Err(io::Error::other("invalid child process ID"));
    }
    let id = libc::id_t::try_from(pid).map_err(|_| io::Error::other("invalid child process ID"))?;
    // SAFETY: Zero is a valid initial representation for Linux siginfo_t, as required
    // for detecting an empty WNOHANG result.
    let mut info = unsafe { MaybeUninit::<libc::siginfo_t>::zeroed().assume_init() };
    // SAFETY: info is writable and waitid only observes this exclusively owned child.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: info was initialized before waitid and si_pid is valid for its result.
    let observed = unsafe { info.si_pid() };
    if observed == 0 {
        Ok(false)
    } else if observed == pid {
        Ok(true)
    } else {
        Err(io::Error::other("unexpected child wait result"))
    }
}

pub fn terminate_group(child: &Child) -> io::Result<()> {
    let pid = libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::other("invalid child process ID"))?;
    if pid <= 1 {
        return Err(io::Error::other("invalid child process group"));
    }
    // SAFETY: The caller exclusively owns the unreaped child, preventing PID reuse.
    let group = unsafe { libc::getpgid(pid) };
    if group == -1 {
        return Err(io::Error::last_os_error());
    }
    if group != pid {
        return Err(io::Error::other("child process group changed"));
    }
    let target = pid
        .checked_neg()
        .ok_or_else(|| io::Error::other("invalid process group"))?;
    // SAFETY: A new session made the owned child its group leader; its unreaped PID
    // cannot be reused while this exclusively owned Child is held.
    if unsafe { libc::kill(target, libc::SIGTERM) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::wait_readable;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn readiness_timeout_data_and_hangup() -> std::io::Result<()> {
        let (mut reader, mut writer) = UnixStream::pair()?;
        assert!(!wait_readable(Some(reader.as_fd()), None, None)?);
        writer.write_all(b"ready")?;
        assert!(wait_readable(None, Some(reader.as_fd()), None)?);
        let mut bytes = [0; 5];
        reader.read_exact(&mut bytes)?;
        assert_eq!(&bytes, b"ready");
        drop(writer);
        assert!(wait_readable(Some(reader.as_fd()), None, None)?);
        assert!(!wait_readable(None, None, None)?);
        Ok(())
    }
}
