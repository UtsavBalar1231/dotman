//! Child processes. Every child starts from [`command`], so the age key in `DOTMAN_AGE_KEY` never
//! reaches git, hooks, the editor or any other program; the age CLI gets it on stdin instead.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Holds age identity text. Never written to a file and never passed to a child's environment.
pub const AGE_KEY_VAR: &str = "DOTMAN_AGE_KEY";

/// A command for `program` whose environment lacks [`AGE_KEY_VAR`].
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove(AGE_KEY_VAR);
    cmd
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
