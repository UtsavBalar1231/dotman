//! The conflict flow's edges: its width on a narrow terminal, a path under a symlinked directory,
//! a sync killed at each step of the replay, and a rebase started by hand.

mod common;

use common::TestEnv;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs `dotman`, asserts the exit code and returns stderr.
fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    let out = env.run(args);
    assert_eq!(
        out.status.code(),
        Some(code),
        "dot {args:?}: {}",
        stderr(&out)
    );
    stderr(&out)
}

/// A device with `files` added, `origin` set and the first sync done.
fn first_device(files: &[(&str, &[u8], u32)]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    let mut args = vec!["add"];
    for (key, bytes, mode) in files {
        env.write(key, bytes, *mode);
        args.push(key);
    }
    env.ok(&args);
    env.ok(&["sync"]);
    // A bare remote's HEAD names `master` until something says otherwise, and a clone follows it.
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    env
}

fn pair(files: &[(&str, &[u8], u32)]) -> (TestEnv, TestEnv) {
    let a = first_device(files);
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    (a, b)
}

fn no_replay_left(env: &TestEnv) {
    for dir in ["rebase-merge", "rebase-apply", "dotman-pull"] {
        assert!(!env.repo().join(".git").join(dir).exists(), "{dir}");
    }
}

// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn real_git() -> String {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH")
        .to_string_lossy()
        .into_owned()
}

/// Puts a `git` in front of PATH that runs the shell snippet `before` with the arguments in `$*`,
/// then `after` once the real git has run and exited with `$rc`.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn wrap_git(env: &mut TestEnv, before: &str, after: &str) {
    let stubs = env.root().join("git-wrapper");
    fs::create_dir_all(&stubs).expect("stub dir");
    let wrapper = format!(
        "#!/bin/sh\n{before}\n'{}' \"$@\"\nrc=$?\n{after}\nexit $rc\n",
        real_git()
    );
    fs::write(stubs.join("git"), wrapper).expect("write the wrapper");
    fs::set_permissions(stubs.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    env.prepend_path(&stubs);
}

/// A snippet that kills dotman, once, the first time git runs a command whose arguments match the
/// shell glob `pattern`, before git runs it when `before` and right after it when not. The
/// returned snippets are for [`wrap_git`]'s `before` and `after`.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn kill_once(env: &TestEnv, pattern: &str, before: bool) -> (String, String) {
    let armed = env.root().join("kill-armed");
    fs::write(&armed, b"").expect("arm the kill");
    if before {
        // The exit keeps the real git from running after dotman is gone.
        let kill = format!(
            "case \" $* \" in {pattern}) if [ -e '{armed}' ]; then rm '{armed}'; kill -KILL \"$PPID\"; exit 137; fi;; esac",
            armed = armed.display(),
        );
        (kill, String::new())
    } else {
        // The kill must come after the real git ran; the `case` sets the arm before git runs.
        let marked = env.root().join("kill-after");
        let set = format!(
            "case \" $* \" in {pattern}) if [ -e '{armed}' ]; then rm '{armed}'; : > '{marked}'; fi;; esac",
            armed = armed.display(),
            marked = marked.display(),
        );
        let after = format!(
            "if [ -e '{marked}' ]; then rm '{marked}'; kill -KILL \"$PPID\"; fi",
            marked = marked.display()
        );
        (set, after)
    }
}

/// The lines of a terminal capture that are wider than `cols`, judged with the harness's long
/// temp root shortened. A line that is one word, such as a backup path on a line of its own, cannot
/// be broken and is left out.
fn wide_lines(env: &TestEnv, seen: &str, cols: usize) -> Vec<String> {
    let root = env.root().to_string_lossy().into_owned();
    seen.replace("\r\n", "\n")
        .replace(&root, "/t")
        .lines()
        .filter(|line| line.chars().count() > cols && line.split_whitespace().count() > 1)
        .map(str::to_owned)
        .collect()
}

/// The unanswered report, the restricted git steps for a shape dotman does not settle, and a
/// flag's answer with the saved line and the settings line, all on the width the demo films
/// record at: no line is wider than the terminal.
#[test]
#[cfg(target_os = "linux")]
fn on_a_71_column_terminal_the_conflict_flow_prints_no_line_wider_than_it() {
    let plain = [("NO_COLOR", "1"), ("TERM", "dumb")];

    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    let seen = common::in_a_terminal_cols(&b, &plain, &["sync"], b"", 71).expect("script(1) runs");
    assert!(
        seen.contains("the pull conflicts with your changes"),
        "{seen}"
    );
    assert!(seen.contains("conflicting paths:"), "{seen}");
    assert!(seen.contains("`dotman sync --keep-local`"), "{seen}");
    assert_eq!(wide_lines(&b, &seen, 71), Vec::<String>::new(), "{seen}");

    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".x/y", b"y\n", 0o644);
    a.write(".a", b"from a\n", 0o644);
    a.ok(&["add", ".x/y"]);
    exits(&a, 0, &["sync"]);
    b.write(".x", b"file\n", 0o644);
    b.write(".a", b"from b\n", 0o644);
    b.ok(&["add", ".x"]);
    let seen = common::in_a_terminal_cols(&b, &plain, &["sync", "--keep-local"], b"", 71)
        .expect("script(1) runs");
    let words = seen.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(words.contains("which dotman cannot settle"), "{seen}");
    assert!(words.contains("To resolve the pull with git:"), "{seen}");
    assert!(
        words.contains("env GIT_EDITOR=true dotman git rebase --continue"),
        "{seen}"
    );
    assert_eq!(wide_lines(&b, &seen, 71), Vec::<String>::new(), "{seen}");

    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    for (env, name) in [(&a, "from-a"), (&b, "from-b")] {
        env.write(format!(".{name}.key"), b"k\n", 0o600);
        env.ok(&["add", "--yes", &format!(".{name}.key")]);
    }
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    let seen = common::in_a_terminal_cols(&b, &plain, &["sync", "--take-repo"], b"", 71)
        .expect("script(1) runs");
    let words = seen.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(words.contains("saved this machine's .a to /"), "{seen}");
    assert!(
        words.contains("merged .dotman.toml: 1 setting from origin/main"),
        "{seen}"
    );
    assert_eq!(wide_lines(&b, &seen, 71), Vec::<String>::new(), "{seen}");
    assert_eq!(b.read(".a"), b"from a\n");
}

/// The repo's directory `d` turns into a symlink to a directory outside the repo while the replay
/// is stopped on `d/f`: dotman settles nothing through it. The file outside stays as it was, no
/// file is added beside it, and once the symlink is gone the sync goes through.
#[test]
fn a_conflicting_path_under_a_symlinked_directory_is_never_written_through() {
    let (a, mut b) = pair(&[(".a", b"a\n", 0o644), ("d/f", b"base\n", 0o644)]);
    a.write("d/f", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write("d/f", b"from b\n", 0o644);

    let outside = b.root().join("outside");
    fs::create_dir(&outside).expect("create the outside directory");
    fs::write(outside.join("f"), b"outside\n").expect("write the outside file");
    let armed = b.root().join("swap-armed");
    fs::write(&armed, b"").expect("arm the swap");
    let repo = b.repo();
    wrap_git(
        &mut b,
        "",
        &format!(
            "case \" $* \" in *' rebase --onto '*) if [ -e '{armed}' ]; then rm '{armed}'; rm -rf '{d}'; ln -s '{outside}' '{d}'; fi;; esac",
            armed = armed.display(),
            d = repo.join("d").display(),
            outside = outside.display(),
        ),
    );
    let out = b.run(&["sync", "--keep-local"]);
    assert!(!out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("the pull was undone"), "{err}");
    assert_eq!(fs::read(outside.join("f")).expect("read"), b"outside\n");
    let names: Vec<_> = fs::read_dir(&outside)
        .expect("read the outside directory")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(names, ["f"], "{err}");
    no_replay_left(&b);

    // The undone pull put `d` back as the directory the repo tracks.
    assert!(fs::symlink_metadata(repo.join("d")).expect("d").is_dir());
    exits(&b, 0, &["sync", "--keep-local"]);
    assert_eq!(b.read("d/f"), b"from b\n");
    assert_eq!(fs::read(outside.join("f")).expect("read"), b"outside\n");
}

/// A merge where a text file and a private binary file clash, so the winner's `[modes]` entry also
/// corrects `.dotman.toml` after the replay: every step of the replay runs.
fn replay_pair() -> (TestEnv, TestEnv) {
    let (a, b) = pair(&[(".t", b"1\n2\n3\n", 0o644), (".bin", b"bin\0base", 0o644)]);
    a.write(".t", b"1\nfrom a\n3\n", 0o644);
    a.write(".bin", b"bin\0from a", 0o600);
    exits(&a, 0, &["sync"]);
    b.write(".t", b"1\nfrom b\n3\n", 0o644);
    b.write(".bin", b"bin\0from b", 0o644);
    (a, b)
}

/// A sync killed at a step of the replay: `status` then exits 4 with the one-line message,
/// `sync --dry-run` changes nothing, and `sync` puts the repo back and finishes the pull with the
/// answer it is given, after which both machines hold the same files.
fn killed_at(pattern: &str, before: bool) {
    let (a, mut b) = replay_pair();
    let (pre, post) = kill_once(&b, pattern, before);
    wrap_git(&mut b, &pre, &post);
    let out = b.run(&["sync", "--keep-local"]);
    assert_eq!(out.status.code(), None, "killed: {}", stderr(&out));
    assert!(
        b.repo().join(".git/dotman-pull").exists(),
        "the marker stays"
    );

    let err = exits(&b, 4, &["status"]);
    assert!(
        err.contains("was stopped while it finished a pull"),
        "{err}"
    );
    let before_trees = b.four_trees();
    let err = exits(&b, 4, &["sync", "--dry-run"]);
    assert!(
        err.contains("was stopped while it finished a pull"),
        "{err}"
    );
    assert_eq!(
        b.four_trees(),
        before_trees,
        "the dry run changed something"
    );

    let err = exits(&b, 0, &["sync", "--keep-local"]);
    assert!(
        err.contains("the repo is back as it was before that pull")
            || err.contains("finished its pull but was stopped before tidying up"),
        "{err}"
    );
    no_replay_left(&b);
    assert_eq!(b.read(".t"), b"1\nfrom b\n3\n");
    assert_eq!(b.read(".bin"), b"bin\0from b");
    exits(&a, 0, &["sync"]);
    common::assert_converged(&a, &b);
    exits(&b, 0, &["sync"]);
    assert_eq!(a.read(".bin"), b"bin\0from b");
    assert_eq!(common::mode_of(&b.home().join(".bin")), 0o644);
}

#[test]
fn a_sync_killed_before_the_rebase_continues_recovers() {
    killed_at("*' rebase --continue'*|*' rebase --skip'*", true);
}

#[test]
fn a_sync_killed_after_the_rebase_continues_recovers() {
    killed_at("*' rebase --continue'*|*' rebase --skip'*", false);
}

#[test]
fn a_sync_killed_at_the_correction_commit_recovers() {
    killed_at("*' commit '*' resolve '*", true);
}

#[test]
fn a_sync_killed_just_before_the_branch_moves_recovers() {
    killed_at("*' update-ref '*", true);
}

#[test]
fn a_sync_killed_just_after_the_branch_moves_recovers() {
    killed_at("*' update-ref '*", false);
}

/// A rebase the user started by hand has no marker: every command refuses it as before, and
/// undoing it by hand lets the next sync through.
#[test]
fn a_rebase_started_by_hand_is_still_refused() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    exits(&b, 4, &["sync"]);

    let out = b.run(&["git", "pull", "--rebase", "origin", "main"]);
    assert!(!out.status.success(), "the pull stops: {}", stderr(&out));
    assert!(!b.repo().join(".git/dotman-pull").exists());
    for args in [
        &["sync"][..],
        &["sync", "--keep-local"],
        &["sync", "--dry-run"],
        &["status"],
    ] {
        let err = exits(&b, 4, args);
        assert!(err.contains("a rebase is in progress"), "{args:?}: {err}");
        assert!(
            !err.contains("was stopped while it finished a pull"),
            "{args:?}: {err}"
        );
    }
    assert!(!b.repo().join(".git/dotman-pull").exists());

    exits(&b, 0, &["git", "rebase", "--abort"]);
    exits(&b, 0, &["sync", "--keep-local"]);
    exits(&a, 0, &["sync"]);
    common::assert_converged(&a, &b);
}
