//! Platform-specific calls. The only module allowed to use `unsafe`.

/// Restores the default SIGPIPE action so a closed pipe ends the process quietly.
#[cfg(unix)]
pub fn restore_sigpipe() {
    // SAFETY: `signal` with SIG_DFL installs no handler; called once at startup before any threads.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
