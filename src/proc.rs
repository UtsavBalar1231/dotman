//! Child processes. Every child starts from [`command`], so the age key in `DOTMAN_AGE_KEY` never
//! reaches git, hooks, the editor or any other program; the age CLI gets it on stdin instead.

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(20);

/// Holds age identity text. Never written to a file and never passed to a child's environment.
pub const AGE_KEY_VAR: &str = "DOTMAN_AGE_KEY";

/// A command for `program` whose environment lacks [`AGE_KEY_VAR`].
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove(AGE_KEY_VAR);
    cmd
}

/// Runs `cmd` with no stdin and its output discarded, and kills it after `timeout`, giving `None`.
pub fn status_within(cmd: &mut Command, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            // The child may have exited since the poll, which makes the kill fail; the wait reaps it.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(POLL.min(left));
    }
}

/// `name` itself when it contains a `/`, else the first executable `name` in an absolute `PATH`
/// entry. Relative entries such as `.` are skipped, so the working directory cannot plant one.
#[must_use]
pub fn find_program(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return is_executable(Path::new(name)).then(|| PathBuf::from(name));
    }
    find_program_in(name, &std::env::var_os("PATH")?)
}

/// Like [`find_program`], searching `path` instead of `PATH`.
#[must_use]
pub fn find_program_in(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use walkdir::WalkDir;

    #[test]
    fn the_age_key_is_removed_from_the_environment() {
        let cmd = command("true");
        let removed = cmd
            .get_envs()
            .any(|(name, value)| name == OsStr::new(AGE_KEY_VAR) && value.is_none());
        assert!(removed);
    }

    #[test]
    fn every_child_process_in_src_starts_here() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let own = src.join("proc.rs");
        let offenders: Vec<String> = WalkDir::new(&src)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension() == Some(OsStr::new("rs")) && e.path() != own)
            .filter(|e| {
                fs::read_to_string(e.path()).is_ok_and(|text| text.contains("Command::new("))
            })
            .map(|e| e.path().display().to_string())
            .collect();
        assert!(offenders.is_empty(), "use proc::command in {offenders:?}");
    }

    #[test]
    fn a_child_past_the_timeout_is_killed() {
        let started = Instant::now();
        let mut slow = command("sh");
        slow.args(["-c", "sleep 30"]);
        assert_eq!(
            status_within(&mut slow, Duration::from_millis(200)).unwrap(),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(10));

        let mut quick = command("sh");
        quick.args(["-c", "exit 3"]);
        let status = status_within(&mut quick, Duration::from_secs(30)).unwrap();
        assert_eq!(status.and_then(|s| s.code()), Some(3));
    }

    #[test]
    fn programs_are_found_by_path_or_name() {
        assert_eq!(
            find_program("/bin/sh").as_deref(),
            Some(Path::new("/bin/sh"))
        );
        assert!(find_program("sh").is_some());
        assert_eq!(find_program("/nonexistent/dotman-x"), None);
        assert_eq!(find_program("dotman-no-such-program"), None);
        assert_eq!(find_program_in("sh", OsStr::new("relative:")), None);
        assert!(find_program_in("sh", OsStr::new("/nonexistent:/bin:/usr/bin")).is_some());
    }
}
