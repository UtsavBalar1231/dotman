//! Text that a hostile upstream or a hostile file name controls reaches the terminal escaped: a
//! commit subject in `log`, a hook command where approval is asked, a name refused as a key, and a
//! source name in the import report.

mod common;

use common::TestEnv;
use std::process::Output;

const OSC: &str = "\u{1b}]0;PWNED\u{7}\u{1b}[31mred\u{1b}[0m";
const OSC_SHOWN: &str = "\\u{1b}]0;PWNED\\u{7}\\u{1b}[31mred\\u{1b}[0m";

fn raw(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| *b == 0x1b || *b == b'\r')
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn ok(out: &Output) -> (String, String) {
    assert!(out.status.success(), "{}", text(&out.stderr));
    (text(&out.stdout), text(&out.stderr))
}

#[test]
fn a_commit_subject_from_upstream_is_shown_escaped_by_log() {
    let subject = format!("fix {OSC} #\r\u{1b}[2Kforged");
    let shown = format!("fix {OSC_SHOWN} #\\r\\u{{1b}}[2Kforged");
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    let upstream = env.root().join("upstream").to_string_lossy().into_owned();
    env.git(&["clone", "-q", "-b", "main", &env.remote_url(), &upstream]);
    env.git(&[
        "-C",
        &upstream,
        "commit",
        "-q",
        "--allow-empty",
        "--cleanup=verbatim",
        "-m",
        &subject,
    ]);
    env.git(&["-C", &upstream, "push", "-q", "origin", "main"]);
    ok(&env.run(&["sync"]));

    let out = env.run(&["log"]);
    let (log, _) = ok(&out);
    assert!(!raw(&out.stdout), "{log:?}");
    let newest = log.lines().next().unwrap_or_default();
    assert!(newest.ends_with(&shown), "{log:?}");
    assert_eq!(log.lines().count(), 2, "{log:?}");
}

/// An initialised repo whose worktree holds a file that `.config/app/**` matches, with `command`,
/// a TOML basic string body, as the hook for it.
fn hooked(env: &TestEnv, command: &str) {
    env.ok(&["init"]);
    env.write_repo(".config/app/a.conf", b"a\n");
    env.write_repo(
        ".dotman.toml",
        format!("[hooks]\n\".config/app/**\" = \"{command}\"\n").as_bytes(),
    );
}

/// What the terminal showed for `apply`, then for `-q apply`, each after a new upstream change
/// to the hooked file, with colour off, so any escape byte in it came from the repo. Fails where
/// Linux has no `script(1)`, so no assertion is skipped.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
#[cfg(target_os = "linux")]
fn applied_in_a_terminal(env: &TestEnv, input: &[u8]) -> Vec<String> {
    let plain = [("NO_COLOR", "1"), ("TERM", "dumb")];
    [&["apply"][..], &["-q", "apply"]]
        .iter()
        .map(|args| {
            env.write_repo(".config/app/a.conf", args.join(" ").as_bytes());
            common::in_a_terminal(env, &plain, args, input)
                .expect("script(1) gives dotman a terminal")
        })
        .collect()
}

#[test]
#[cfg(target_os = "linux")]
fn a_hook_command_is_shown_escaped_where_approval_is_asked_even_with_quiet() {
    // An escape sequence in a command is refused before anything is asked, and named escaped.
    let env = TestEnv::new();
    hooked(
        &env,
        &format!(
            "true #\\r\\u001b[2K{}",
            OSC.replace('\u{1b}', "\\u001b").replace('\u{7}', "\\u0007")
        ),
    );
    for seen in applied_in_a_terminal(&env, b"y\n") {
        assert!(!seen.contains('\u{1b}'), "{seen:?}");
        assert!(
            seen.contains(&format!("true #\\r\\u{{1b}}[2K{OSC_SHOWN}")),
            "{seen:?}"
        );
        assert!(seen.contains("U+000D carriage return"), "{seen:?}");
        assert!(!seen.contains("run the matching hooks"), "{seen:?}");
    }

    // A newline and a tab reach the table that approval covers, each kept on its line.
    let env = TestEnv::new();
    let marker = env.root().join("never");
    hooked(
        &env,
        &format!("true\\n#\\tforged -> touch '{}'", marker.display()),
    );
    for seen in applied_in_a_terminal(&env, b"n\n") {
        assert!(!seen.contains('\u{1b}'), "{seen:?}");
        assert!(seen.contains("run the matching hooks"), "{seen:?}");
        assert!(
            seen.contains(&format!(
                ".config/app/** -> true\\n#\\tforged -> touch '{}'",
                marker.display()
            )),
            "{seen:?}"
        );
        assert!(
            !seen.lines().any(|line| line.trim_start().starts_with('#')),
            "a newline in a command never starts a line: {seen:?}"
        );
    }
    assert!(!marker.exists());
}

#[test]
fn a_name_refused_as_a_key_is_reported_escaped() {
    let names = [
        (format!("d/{OSC}"), format!("d/{OSC_SHOWN}")),
        ("d/cr\rspoof".to_owned(), "d/cr\\rspoof".to_owned()),
        ("d/nl\nforged".to_owned(), "d/nl\\nforged".to_owned()),
    ];
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("d/ok", b"ok\n", 0o644);
    for (name, _) in &names {
        env.write(name, b"x\n", 0o644);
    }
    let out = env.run(&["add", "--yes", "d"]);
    let (_, added) = ok(&out);
    assert!(!raw(&out.stderr), "{added:?}");
    for (_, shown) in &names {
        assert!(added.contains(shown.as_str()), "add: {added}");
    }

    let out = env.run(&["status"]);
    let (status, notes) = ok(&out);
    assert!(
        !raw(&out.stdout) && !raw(&out.stderr),
        "{status:?} {notes:?}"
    );
    for (_, shown) in &names {
        assert!(notes.contains(shown.as_str()), "status: {notes}");
    }
    assert!(
        !notes.lines().any(|line| line.starts_with("forged")),
        "a newline in a name never starts a line: {notes}"
    );

    let (listed, _) = ok(&env.run(&["list"]));
    assert_eq!(listed.lines().count(), 1, "only d/ok is managed: {listed}");
    assert!(listed.contains("d/ok"), "{listed}");
}

/// A source name with a newline stays on its line in the import report, both as an entry that
/// needs action and in a note.
#[test]
fn a_source_name_in_the_import_report_never_starts_a_line() {
    let env = TestEnv::new();
    let source = env.root().join("src");
    let src = source.to_string_lossy().into_owned();
    env.git(&["init", "-q", &src]);
    for name in [".bashrc", ".plain\nnext: forged", "README\nnext: forged"] {
        std::fs::write(source.join(name), b"x\n").unwrap();
    }
    env.git(&["-C", &src, "add", "-A"]);
    env.git(&["-C", &src, "commit", "-q", "-m", "init"]);
    let out = env.run(&["import", "--yes", &src]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains("  .plain\\nnext: forged: "), "{err}");
    assert!(err.contains("README\\nnext: forged"), "{err}");
    assert!(
        !err.lines()
            .any(|line| line.contains("forged") && !line.contains("\\n")),
        "a newline in a name never starts a line: {err}"
    );
}
