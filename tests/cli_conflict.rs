//! The conflict flow's edges: its width on a narrow terminal, a path under a symlinked directory,
//! a sync killed while you decide or at each step of the replay, and a rebase started by hand.

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

/// A device on host `alpha` with `files` added, `origin` set and the first sync done.
fn first_device(files: &[(&str, &[u8], u32)]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init", "--host", "alpha"]);
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

/// [`first_device`] and a second device on host `beta`.
fn pair(files: &[(&str, &[u8], u32)]) -> (TestEnv, TestEnv) {
    let a = first_device(files);
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo", "--host", "beta"]);
    (a, b)
}

const PLAIN: [(&str, &str); 2] = [("NO_COLOR", "1"), ("TERM", "dumb")];

/// `sync` on `env` in a terminal with the typed `input`; fails when `script` is missing, since CI
/// has it.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn sync_in_a_terminal(env: &TestEnv, input: &[u8]) -> String {
    common::in_a_terminal(env, &PLAIN, &["sync"], input)
        .expect("script(1) runs")
        .replace('\r', "")
}

fn remote_file(env: &TestEnv, source: &str) -> String {
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&["--git-dir", &remote, "show", &format!("main:{source}")])
}

/// Commits `bytes` as the repo path `path` on `env`, as `dotman git` would.
fn commit_in_repo(env: &TestEnv, path: &str, bytes: &[u8]) {
    env.write_repo(path, bytes);
    env.ok(&["git", "add", "--", path]);
    env.ok(&["git", "commit", "-qm", path]);
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

/// After a kill, the user edits a decided path in the repo by hand: recovery saves the edit to a
/// backup run, says where, and then puts the repo back.
#[test]
fn recovery_saves_a_hand_edit_of_a_decided_path_first() {
    for pattern in [
        "*' update-ref '*",
        "*' rebase --continue'*|*' rebase --skip'*",
    ] {
        let (_a, mut b) = replay_pair();
        let (pre, post) = kill_once(&b, pattern, true);
        wrap_git(&mut b, &pre, &post);
        let out = b.run(&["sync", "--keep-local"]);
        assert_eq!(out.status.code(), None, "killed: {}", stderr(&out));
        let edit = b"1\nfrom b\n3\nby hand\n";
        fs::write(b.repo().join(".t"), edit).expect("edit by hand");

        let err = exits(&b, 0, &["sync", "--keep-local"]);
        assert!(
            err.contains("the repo is back as it was before that pull"),
            "{pattern}: {err}"
        );
        let dir = err
            .lines()
            .find_map(|line| line.strip_prefix("saved the uncommitted edits of the repo's .t to "))
            .map(std::path::PathBuf::from)
            .expect("a saved line");
        assert!(dir.starts_with(b.state_dir().join("backups")), "{err}");
        assert_eq!(fs::read(dir.join(".t")).expect("the saved edit"), edit);
        no_replay_left(&b);
    }
}

/// The lines numbered `  1. ` and on in `err`, in order.
fn numbered_steps(err: &str) -> Vec<String> {
    err.lines()
        .filter_map(|line| {
            let (n, step) = line.trim_start().split_once(". ")?;
            n.parse::<u32>().is_ok().then(|| step.to_owned())
        })
        .collect()
}

/// After a kill, the user edits a file the pull does not touch: recovery refuses, names it, and
/// its printed steps copy it to a backup run before `checkout -f` drops it. Run as printed, they
/// lead to a clean sync, and the edit survives in the copy.
#[test]
fn the_refusal_s_steps_save_an_uncommitted_edit_before_they_check_out() {
    for pattern in [
        "*' update-ref '*",
        "*' rebase --continue'*|*' rebase --skip'*",
    ] {
        let (a, mut b) = pair(&[(".t", b"1\n2\n3\n", 0o644), (".other", b"other\n", 0o644)]);
        a.write(".t", b"1\nfrom a\n3\n", 0o644);
        exits(&a, 0, &["sync"]);
        b.write(".t", b"1\nfrom b\n3\n", 0o644);
        let (pre, post) = kill_once(&b, pattern, true);
        wrap_git(&mut b, &pre, &post);
        let out = b.run(&["sync", "--keep-local"]);
        assert_eq!(out.status.code(), None, "killed: {}", stderr(&out));
        let edit = b"other\nby hand\n";
        fs::write(b.repo().join(".other"), edit).expect("edit by hand");

        let err = exits(&b, 4, &["sync", "--keep-local"]);
        assert!(err.contains(".other has changes"), "{pattern}: {err}");
        assert!(err.contains("after copying .other to "), "{pattern}: {err}");
        let steps = numbered_steps(&err);
        let checkout = steps
            .iter()
            .position(|step| step == "dotman git checkout -f main")
            .expect("a checkout step");
        let save = steps
            .iter()
            .position(|step| step.contains("cp -P -- "))
            .expect("a save step");
        assert!(save < checkout, "{err}");
        for step in &steps {
            let out = common::paste(&b, step);
            let code = if step == "dotman sync" { 4 } else { 0 };
            assert_eq!(out.status.code(), Some(code), "{step}: {}", stderr(&out));
        }
        exits(&b, 0, &["sync", "--keep-local"]);
        no_replay_left(&b);
        let saved = common::files_containing(&b.state_dir().join("backups"), b"by hand");
        assert_eq!(saved.len(), 1, "{pattern}: {saved:?}");
        exits(&a, 0, &["sync"]);
    }
}

/// After a kill, the user saves copies in the repo named as git 2.25 names the copies a stop leaves,
/// `.t~mine` and an editor's `.t~`: recovery keeps them. A new file that holds the blob the replay
/// had at `.t` is git's, and goes.
#[test]
fn recovery_keeps_a_user_s_file_named_like_git_s_leftover() {
    for pattern in [
        "*' update-ref '*",
        "*' rebase --continue'*|*' rebase --skip'*",
    ] {
        let (_a, mut b) = replay_pair();
        let (pre, post) = kill_once(&b, pattern, true);
        wrap_git(&mut b, &pre, &post);
        let out = b.run(&["sync", "--keep-local"]);
        assert_eq!(out.status.code(), None, "killed: {}", stderr(&out));
        let repo = b.repo();
        for name in [".t~mine", ".t~"] {
            fs::write(repo.join(name), format!("user copy {name}\n")).expect("write a copy");
        }
        fs::write(repo.join(".t~HEAD"), b"1\nfrom b\n3\n").expect("write git's copy");

        let err = exits(&b, 0, &["sync", "--keep-local"]);
        assert!(
            err.contains("the repo is back as it was before that pull"),
            "{pattern}: {err}"
        );
        for name in [".t~mine", ".t~"] {
            assert_eq!(
                fs::read(repo.join(name)).expect("the copy survives"),
                format!("user copy {name}\n").as_bytes(),
                "{pattern}: {err}"
            );
        }
        assert!(!repo.join(".t~HEAD").exists(), "{pattern}: {err}");
        no_replay_left(&b);
    }
}

/// [`TestEnv::four_trees`] with the repo's `.git` left out but for its objects and refs, since an
/// aborted pull rewrites the index, the reflogs and `ORIG_HEAD`; with the status in their place.
fn outside_git(
    env: &TestEnv,
) -> (
    Vec<std::collections::BTreeMap<String, common::Entry>>,
    String,
) {
    let [home, mut repo, state, remote] = env.four_trees();
    repo.retain(|path, _| {
        !(path == ".git" || path.starts_with(".git/"))
            || path.starts_with(".git/objects/")
            || path.starts_with(".git/refs/")
    });
    let status = env.git(&["-C", &env.repo().to_string_lossy(), "status", "--porcelain"]);
    (vec![home, repo, state, remote], status)
}

/// A sync killed while you decide, here by the editor `[e]` opens, leaves nothing half merged: no
/// marker and no rebase, and `$HOME`, the repo, the state and the remote as the last unanswered
/// run left them. The next sync reports the same conflict, and a flag then settles it.
#[test]
fn a_sync_killed_in_the_editor_leaves_nothing_half_merged() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    exits(&b, 4, &["sync"]);
    let before = outside_git(&b);

    // The editor's copy goes to the runtime dir, outside the trees compared.
    let run = b.root().join("run");
    fs::create_dir(&run).expect("runtime dir");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("chmod");
    let editor = b.root().join("editor");
    fs::write(&editor, "#!/bin/sh\nkill -KILL \"$PPID\"\n").expect("write the editor");
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).expect("chmod");
    let (editor, run) = (editor.to_string_lossy(), run.to_string_lossy());
    let vars = [
        PLAIN.as_slice(),
        &[("VISUAL", &*editor), ("XDG_RUNTIME_DIR", &*run)],
    ]
    .concat();
    let seen = common::in_a_terminal(&b, &vars, &["sync"], b"e\n").expect("script(1) runs");
    assert!(seen.contains(".a [k] keep mine"), "{seen}");
    // Killed in the editor: no refusal, no stop report and nothing saved was printed.
    for after in [
        "nothing was taken",
        "conflicting paths:",
        "saved this machine's",
    ] {
        assert!(!seen.contains(after), "{after}: {seen}");
    }

    no_replay_left(&b);
    assert_eq!(outside_git(&b), before, "{seen}");
    let err = exits(&b, 4, &["sync"]);
    assert!(err.contains("conflicting paths:\n  .a\n"), "{err}");
    exits(&b, 0, &["sync", "--keep-local"]);
    exits(&a, 0, &["sync"]);
    common::assert_converged(&a, &b);
}

/// The private copies a sync killed in the editor leaves in the runtime dir are removed by the next
/// sync at once, not ten minutes later.
#[test]
fn the_private_copies_a_killed_editor_leaves_are_removed_by_the_next_sync() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    let run = b.root().join("run");
    fs::create_dir(&run).expect("runtime dir");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("chmod");
    let editor = b.root().join("editor");
    fs::write(&editor, "#!/bin/sh\nkill -KILL \"$PPID\"\n").expect("write the editor");
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).expect("chmod");
    let (editor, run_var) = (editor.to_string_lossy(), run.to_string_lossy());
    let vars = [
        PLAIN.as_slice(),
        &[("VISUAL", &*editor), ("XDG_RUNTIME_DIR", &*run_var)],
    ]
    .concat();
    let seen = common::in_a_terminal(&b, &vars, &["sync"], b"e\n").expect("script(1) runs");
    let private = || -> usize {
        fs::read_dir(&run)
            .expect("read the runtime dir")
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dotman-tmp-")
            })
            .count()
    };
    assert_ne!(private(), 0, "the kill left the copies: {seen}");

    let out = b.run_with(&[("XDG_RUNTIME_DIR", &run_var)], &["sync", "--keep-local"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(private(), 0, "{}", stderr(&out));
}

/// This machine's version of a file that loses is saved before the replay starts: a sync killed as
/// the replay's rebase begins has already saved it, and the copy outlasts the recovery.
#[test]
fn the_version_that_loses_is_saved_before_the_replay_starts() {
    let (a, mut b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    let (pre, post) = kill_once(&b, "*' rebase --onto '*", true);
    wrap_git(&mut b, &pre, &post);
    let out = b.run(&["sync", "--take-repo"]);
    assert_eq!(out.status.code(), None, "killed: {}", stderr(&out));
    let err = stderr(&out);
    assert!(b.repo().join(".git/dotman-pull").exists(), "{err}");
    let saved = err
        .lines()
        .find_map(|line| line.strip_prefix("saved this machine's .a to "))
        .map(std::path::PathBuf::from)
        .expect("a saved line");
    assert!(saved.ends_with(".dotman/repo/.a"), "{}", saved.display());
    assert_eq!(fs::read(&saved).expect("the saved copy"), b"from b\n");

    exits(&b, 0, &["sync", "--take-repo"]);
    no_replay_left(&b);
    assert_eq!(b.read(".a"), b"from a\n");
    assert_eq!(fs::read(&saved).expect("the saved copy"), b"from b\n");
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

/// `[h]` keeps this machine's version as the variant `add --for host=beta` names, with this
/// machine's `[modes]` entry; the shared source takes origin's version, so the other machine keeps
/// origin's after it syncs. The exit-4 report, where `[h]` would apply, names no
/// `add --for host=` step.
#[test]
fn on_a_terminal_keep_mine_here_only_keeps_this_machine_s_version_as_its_variant() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o600);
    let report = exits(&b, 4, &["sync"]);
    assert!(report.contains("conflicting paths:\n  .a\n"), "{report}");
    assert!(!report.contains("--for host="), "{report}");
    let seen = sync_in_a_terminal(&b, b"h\n");
    assert!(
        seen.contains("this machine keeps its version as .a{host=beta}; every other machine takes origin/main's\n.a [k] keep mine  [t] take theirs  [h] keep mine on this machine only  [e] edit  [d] diff  [s] stop: "),
        "{seen}"
    );
    assert!(seen.contains("saved this machine's .a to "), "{seen}");
    no_replay_left(&b);
    assert_eq!(b.read(".a"), b"from b\n", "{seen}");
    assert_eq!(common::mode_of(&b.home().join(".a")), 0o600);
    assert_eq!(remote_file(&b, ".a"), "from a\n");
    assert_eq!(remote_file(&b, ".a{host=beta}"), "from b\n");
    let modes = b.repo_config().modes;
    assert_eq!(
        modes.get(".a{host=beta}").map(String::as_str),
        Some("0600"),
        "{modes:?}"
    );
    assert_eq!(modes.get(".a"), None, "{modes:?}");

    exits(&a, 0, &["sync"]);
    assert_eq!(a.read(".a"), b"from a\n");
    assert_eq!(common::mode_of(&a.home().join(".a")), 0o644);
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".a"), b"from b\n");
}

/// A sync killed at the correction commit, the step that adds `[h]`'s variant, leaves the marker;
/// the next sync puts the repo back with no trace of the variant in the worktree or the index, so
/// nothing captures it unasked, and answering `[h]` again finishes the pull.
#[test]
fn a_sync_killed_while_it_adds_the_variant_keep_mine_here_only_makes_recovers() {
    let (a, mut b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    let (pre, post) = kill_once(&b, "*' commit '*' resolve '*", true);
    wrap_git(&mut b, &pre, &post);
    let seen = sync_in_a_terminal(&b, b"h\n");
    assert!(
        b.repo().join(".git/dotman-pull").exists(),
        "the marker stays: {seen}"
    );
    let err = exits(&b, 4, &["status"]);
    assert!(
        err.contains("was stopped while it finished a pull"),
        "{err}"
    );

    let err = exits(&b, 4, &["sync"]);
    assert!(
        err.contains("the repo is back as it was before that pull"),
        "{err}"
    );
    assert!(err.contains("conflicting paths:\n  .a\n"), "{err}");
    no_replay_left(&b);
    let repo = b.repo().to_string_lossy().into_owned();
    assert!(!b.repo().join(".a{host=beta}").exists());
    let tracked = b.git(&["-C", &repo, "ls-files"]);
    assert!(!tracked.contains("{host=beta}"), "{tracked}");
    assert_eq!(b.git(&["-C", &repo, "status", "--porcelain"]), "");

    sync_in_a_terminal(&b, b"h\n");
    no_replay_left(&b);
    assert_eq!(remote_file(&b, ".a{host=beta}"), "from b\n");
    assert_eq!(b.read(".a"), b"from b\n");
}

/// `[h]` is not offered when origin already has the variant it would add, nor for a system
/// source, and typing it is not taken as an answer.
#[test]
fn on_a_terminal_keep_mine_here_only_is_not_offered_for_an_existing_variant_or_a_system_file() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    a.ok(&["add", "--for", "host=beta", ".a"]);
    exits(&a, 0, &["sync"]);
    b.write(".a", b"from b\n", 0o644);
    let seen = sync_in_a_terminal(&b, b"h\ns\n");
    assert!(
        seen.contains(".a [k] keep mine  [t] take theirs  [e] edit  [d] diff  [s] stop: "),
        "{seen}"
    );
    assert!(!seen.contains("keep mine on this machine only"), "{seen}");
    assert!(seen.contains("not one of the listed answers"), "{seen}");
    assert_eq!(b.read(".a"), b"from b\n");
    no_replay_left(&b);

    let system = ".dotman/system/etc/app.conf";
    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    commit_in_repo(&a, system, b"base\n");
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    commit_in_repo(&a, system, b"from a\n");
    exits(&a, 0, &["sync"]);
    commit_in_repo(&b, system, b"from b\n");
    let seen = sync_in_a_terminal(&b, b"h\ns\n");
    assert!(
        seen.contains(&format!(
            "{system} [k] keep mine  [t] take theirs  [e] edit  [d] diff  [s] stop: "
        )),
        "{seen}"
    );
    assert!(!seen.contains("keep mine on this machine only"), "{seen}");
    assert!(seen.contains("not one of the listed answers"), "{seen}");
    no_replay_left(&b);
}

/// The sweep removes a killed run's `.dotman-tmp-` dir from `$XDG_RUNTIME_DIR`, where the conflict
/// flow puts its private copies, as soon as its process is gone, old or fresh; a live run's dir and
/// a user's directory of a similar name stay.
#[test]
fn the_sweep_removes_a_dead_run_s_temp_dir_from_the_runtime_dir_and_keeps_a_live_one() {
    use std::os::unix::fs::DirBuilderExt;
    let env = first_device(&[(".a", b"a\n", 0o644)]);
    let run = env.root().join("run");
    fs::create_dir(&run).expect("runtime dir");
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).expect("chmod");
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let dead = child.id();
    child.wait().expect("wait");
    let old = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(3600))
        .expect("an hour ago");
    let dir = |name: String, mode: u32, aged: bool| {
        let path = run.join(name);
        fs::DirBuilder::new()
            .mode(mode)
            .create(&path)
            .expect("mkdir");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
        fs::write(path.join("merged"), b"plaintext\n").expect("write");
        if aged {
            fs::File::open(&path)
                .and_then(|dir| dir.set_modified(old))
                .expect("age it");
        }
        path
    };
    let stale = dir(format!(".dotman-tmp-{dead}-a1B2c3"), 0o700, true);
    let fresh = dir(format!(".dotman-tmp-{dead}-f1B2c3"), 0o700, false);
    let live = dir(
        format!(".dotman-tmp-{}-l1B2c3", std::process::id()),
        0o700,
        true,
    );
    let users = dir(format!(".dotman-tmp-{dead}-u1B2c3"), 0o755, true);
    let run_var = run.to_string_lossy().into_owned();
    let err = env.ok_with(&[("XDG_RUNTIME_DIR", run_var.as_str())], &["sync"]);
    assert!(!stale.exists(), "{err}");
    assert!(!fresh.exists(), "{err}");
    assert!(live.exists(), "{err}");
    assert!(users.exists(), "{err}");
}
