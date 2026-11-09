//! `dotman diff` runs from the repo to `$HOME`, so the user's own edits show as added lines, and
//! it colours only a terminal that has not asked for no colour.

mod common;

use common::TestEnv;
use std::process::Output;

/// A managed `.bashrc` edited in `$HOME`, so the repo holds `two` and `$HOME` holds `TWO`.
fn edited() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"one\ntwo\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.write(".bashrc", b"one\nTWO\n", 0o644);
    env
}

fn stdout(out: &Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_local_edit_shows_as_added_lines_from_repo_to_home() {
    let env = edited();
    env.write_repo(".config/new.conf", b"new\n");
    let shown = stdout(&env.run(&["diff"]));
    for expected in [
        "diff --git repo/.bashrc home/.bashrc",
        "--- repo/.bashrc",
        "+++ home/.bashrc",
        "-two",
        "+TWO",
        "--- repo/.config/new.conf",
        "+++ /dev/null",
    ] {
        assert!(
            shown.contains(expected),
            "missing `{expected}` in:\n{shown}"
        );
    }
}

#[test]
fn a_pipe_gets_no_colour_even_when_git_thinks_a_pager_runs() {
    let env = edited();
    let shown = stdout(&env.run_with(&[("GIT_PAGER_IN_USE", "true")], &["diff"]));
    assert!(shown.contains("+TWO"), "{shown}");
    assert!(!shown.contains('\u{1b}'), "{shown:?}");
}

#[test]
fn a_terminal_gets_colour_unless_no_color_is_set() {
    let env = edited();
    let colour = [("TERM", "xterm")];
    let Some(seen) = common::in_a_terminal(&env, &colour, &["diff", "--no-pager"], b"") else {
        eprintln!("skipped: no script(1) to give dotman a terminal");
        return;
    };
    assert!(seen.contains("TWO"), "{seen:?}");
    assert!(seen.contains('\u{1b}'), "a terminal is coloured: {seen:?}");

    let plain = [("TERM", "xterm"), ("NO_COLOR", "1")];
    let seen =
        common::in_a_terminal(&env, &plain, &["diff", "--no-pager"], b"").unwrap_or_default();
    assert!(seen.contains("+TWO"), "{seen:?}");
    assert!(!seen.contains('\u{1b}'), "NO_COLOR is honoured: {seen:?}");
}
