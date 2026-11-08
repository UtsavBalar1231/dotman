//! The name the user typed to start the program, for hints and usage lines.
//!
//! Files that are generated or committed always say `dotman`, never this name.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::OnceLock;

const CANONICAL: &str = "dotman";
// The alias exists only as a symlink or copy; no other name is echoed back, so a path or an
// unrelated name never ends up in a hint the user is told to type.
const ALIAS: &str = "dot";

static INVOKED: OnceLock<&'static str> = OnceLock::new();

/// The name for an `argv[0]`: `dot` when its last component is exactly that, else `dotman`.
#[must_use]
pub fn from_argv0(argv0: Option<&OsStr>) -> &'static str {
    match argv0.and_then(|arg| Path::new(arg).file_name()) {
        Some(base) if base == ALIAS => ALIAS,
        _ => CANONICAL,
    }
}

/// Records the invoked name from `argv[0]`. Only the first call has an effect.
pub fn init(argv0: Option<&OsStr>) {
    let _ = INVOKED.set(from_argv0(argv0));
}

/// The command to put in a hint: `dot` or `dotman`, as invoked. `dotman` before [`init`] runs.
#[must_use]
pub fn bin() -> &'static str {
    INVOKED.get().copied().unwrap_or(CANONICAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_base_name_of_exactly_dot_is_the_alias() {
        for (argv0, want) in [
            ("dot", "dot"),
            ("/home/me/.local/bin/dot", "dot"),
            ("./dot", "dot"),
            ("dotman", "dotman"),
            ("/usr/bin/dotman", "dotman"),
            ("dot2", "dotman"),
            ("mydot", "dotman"),
            ("dot.exe", "dotman"),
            ("", "dotman"),
            ("/", "dotman"),
        ] {
            assert_eq!(from_argv0(Some(OsStr::new(argv0))), want, "{argv0:?}");
        }
        assert_eq!(from_argv0(None), "dotman");
    }

    #[test]
    fn the_name_is_dotman_until_it_is_set() {
        assert_eq!(bin(), "dotman");
    }
}
