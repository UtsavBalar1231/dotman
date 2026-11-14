//! Every subcommand listed in `dotman --help` needs a row in `MINIMAL_ARGS`; add one with the command.

mod common;

use common::TestEnv;
use std::fs;

const NOT_COVERED: &[&str] = &["git", "doctor", "completion", "system-write", "help"];

const MINIMAL_ARGS: &[(&str, &[&str])] = &[
    ("init", &[]),
    ("add", &[".bashrc"]),
    ("forget", &[".bashrc"]),
    ("list", &[]),
    ("status", &[]),
    ("apply", &[]),
    ("diff", &[]),
    ("edit", &[".bashrc"]),
    ("sync", &[]),
    ("log", &[]),
    ("restore", &[".bashrc"]),
    ("key", &["init"]),
    ("import", &[]),
];

fn listed_subcommands(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|line| line.trim_end() != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| !NOT_COVERED.contains(name))
        .map(str::to_owned)
        .collect()
}

fn subcommands() -> Vec<String> {
    let out = TestEnv::new().run(&["--help"]);
    assert!(out.status.success(), "dot --help failed");
    let listed = listed_subcommands(&String::from_utf8_lossy(&out.stdout));
    assert!(!listed.is_empty(), "found no subcommands in `dot --help`");
    listed
}

#[test]
fn every_subcommand_has_minimal_args() {
    let missing: Vec<String> = subcommands()
        .into_iter()
        .filter(|name| !MINIMAL_ARGS.iter().any(|(known, _)| known == name))
        .collect();
    assert!(
        missing.is_empty(),
        "add a row to MINIMAL_ARGS in tests/guard.rs for: {missing:?}"
    );
}

#[test]
fn minimal_args_are_accepted_by_the_parser() {
    for (name, args) in MINIMAL_ARGS {
        let out = TestEnv::new()
            .dot()
            .arg(name)
            .args(*args)
            .output()
            .expect("run dot");
        assert!(
            out.status
                .code()
                .is_some_and(|code| code != 2 && code != 101),
            "`dot {name} {args:?}` was rejected by clap or panicked: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn stderr_of(env: &TestEnv, args: &[&str]) -> (Option<i32>, String) {
    let out = env.run(args);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `log` only reads the history, so it runs through an unfinished operation.
#[test]
fn every_subcommand_but_log_exits_4_while_a_rebase_is_in_progress() {
    for (name, args) in MINIMAL_ARGS {
        let env = TestEnv::new();
        env.ok(&["init"]);
        fs::create_dir(env.repo().join(".git/rebase-merge")).expect("mark the rebase");
        let mut full = vec![*name];
        full.extend_from_slice(args);
        let (code, err) = stderr_of(&env, &full);
        if *name == "log" {
            assert_eq!(code, Some(0), "`dot {full:?}`: {err}");
            continue;
        }
        assert_eq!(code, Some(4), "`dot {full:?}`: {err}");
        assert!(err.contains("a rebase is in progress"), "{name}: {err}");
        assert!(
            err.contains("dotman git -c core.editor=true rebase --continue"),
            "{name}: {err}"
        );
        assert!(err.contains("dotman git rebase --abort"), "{name}: {err}");
    }
}

#[test]
fn the_other_markers_are_refused_and_named() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let marker = env.repo().join(".git/rebase-apply");
    fs::create_dir(&marker).expect("mark the rebase");
    assert_eq!(stderr_of(&env, &["status"]).0, Some(4));
    fs::remove_dir(&marker).expect("clear the rebase");

    fs::write(env.repo().join(".git/MERGE_HEAD"), "x").expect("mark the merge");
    let (code, err) = stderr_of(&env, &["status"]);
    assert_eq!(code, Some(4), "{err}");
    assert!(err.contains("a merge is in progress"), "{err}");
    assert!(
        err.contains("dotman git -c core.editor=true merge --continue"),
        "{err}"
    );
    assert!(err.contains("dotman git merge --abort"), "{err}");
}

/// A repo whose last commit has `.dotman.toml` and `.bashrc`.
fn committed() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);
    env
}

/// A newline or a comment alone holds no settings, so it loses them just as an empty file does,
/// and sync never commits it.
#[test]
fn every_subcommand_but_log_refuses_a_lost_dotman_toml() {
    for (how, written) in [
        ("is missing", None),
        ("is empty", Some(&b""[..])),
        ("holds no settings", Some(b"\n")),
        ("holds no settings", Some(b"# todo\n")),
    ] {
        for (name, args) in MINIMAL_ARGS {
            let env = committed();
            let config = env.repo().join(".dotman.toml");
            match written {
                Some(bytes) => fs::write(&config, bytes).expect("overwrite the config"),
                None => fs::remove_file(&config).expect("delete the config"),
            }
            let mut full = vec![*name];
            full.extend_from_slice(args);
            let (code, err) = stderr_of(&env, &full);
            if *name == "log" {
                assert_eq!(code, Some(0), "`dot {full:?}`: {err}");
                continue;
            }
            assert_eq!(code, Some(1), "`dot {full:?}`: {err}");
            assert!(err.contains(how), "{name}: {err}");
            assert!(
                err.contains("dotman git checkout -- .dotman.toml"),
                "{name}: {err}"
            );
            assert_eq!(fs::read(&config).ok().as_deref(), written);
            let repo = env.repo().to_string_lossy().into_owned();
            let committed = env.git(&["-C", &repo, "show", "HEAD:.dotman.toml"]);
            assert!(committed.contains("[ignore]"), "{name}: {committed}");
        }
    }
}

/// A last commit whose `.dotman.toml` is empty too has no settings to lose: every command runs on
/// the defaults with a note, and the restore hint, which would only bring back an empty file, is
/// never printed.
#[test]
fn a_committed_empty_dotman_toml_reads_as_the_defaults() {
    let env = committed();
    let repo = env.repo().to_string_lossy().into_owned();
    fs::write(env.repo().join(".dotman.toml"), b"").expect("empty the config");
    env.git(&["-C", &repo, "commit", "-q", "-am", "empty config"]);
    for deleted in [false, true] {
        if deleted {
            fs::remove_file(env.repo().join(".dotman.toml")).expect("delete the config");
        }
        for args in [&["status"][..], &["apply"], &["list"], &["diff"]] {
            let (code, err) = stderr_of(&env, args);
            assert_eq!(code, Some(0), "{args:?}: {err}");
            assert!(
                err.contains("as in the last commit, so the defaults apply"),
                "{err}"
            );
            assert!(!err.contains("git checkout -- .dotman.toml"), "{err}");
        }
    }
    env.write(".bashrc", b"edited\n", 0o644);
    let (code, err) = stderr_of(&env, &["sync"]);
    assert_eq!(code, Some(0), "{err}");
    let shown = env.git(&["-C", &repo, "show", "HEAD:.bashrc"]);
    assert_eq!(shown, "edited\n");
}

#[test]
fn cherry_pick_revert_sequencer_and_bisect_markers_are_refused_with_their_hints() {
    let env = committed();
    let git_dir = env.repo().join(".git");
    for (marker, dir, text, hint) in [
        (
            "CHERRY_PICK_HEAD",
            false,
            "x",
            "dotman git -c core.editor=true cherry-pick --continue",
        ),
        ("REVERT_HEAD", false, "x", "dotman git revert --abort"),
        (
            "sequencer",
            true,
            "revert 0000000 x\n",
            "dotman git -c core.editor=true revert --continue",
        ),
        (
            "sequencer",
            true,
            "pick 0000000 x\n",
            "dotman git cherry-pick --abort",
        ),
        ("BISECT_LOG", false, "x", "dotman git bisect reset"),
    ] {
        let path = git_dir.join(marker);
        if dir {
            fs::create_dir(&path).expect("mark");
            fs::write(path.join("todo"), text).expect("todo");
        } else {
            fs::write(&path, text).expect("mark");
        }
        for args in [&["status"][..], &["apply"], &["sync"]] {
            let (code, err) = stderr_of(&env, args);
            assert_eq!(code, Some(4), "{marker} {args:?}: {err}");
            assert!(err.contains(hint), "{marker}: {err}");
        }
        if dir {
            fs::remove_dir_all(&path).expect("unmark");
        } else {
            fs::remove_file(&path).expect("unmark");
        }
    }
    assert_eq!(stderr_of(&env, &["status"]).0, Some(0));
}

#[test]
fn a_stash_pop_conflict_is_refused_and_nothing_reaches_home() {
    let env = committed();
    let repo = env.repo().to_string_lossy().into_owned();
    fs::write(env.repo().join(".bashrc"), b"stashed\n").expect("edit");
    env.git(&["-C", &repo, "stash", "-q"]);
    fs::write(env.repo().join(".bashrc"), b"committed\n").expect("edit");
    env.git(&["-C", &repo, "commit", "-q", "-am", "other"]);
    let pop = env.git_raw(&["-C", &repo, "stash", "pop"]);
    assert!(!pop.status.success(), "the pop should conflict");
    assert!(!env.repo().join(".git/MERGE_HEAD").exists());

    for args in [&["status"][..], &["apply"], &["sync"]] {
        let (code, err) = stderr_of(&env, args);
        assert_eq!(code, Some(4), "{args:?}: {err}");
        assert!(err.contains("unresolved conflicts in .bashrc"), "{err}");
        assert!(err.contains("dotman git add -- .bashrc"), "{err}");
        assert!(err.contains("dotman git checkout HEAD -- .bashrc"), "{err}");
    }
    assert_eq!(stderr_of(&env, &["log"]).0, Some(0));
    assert_eq!(env.read(".bashrc"), b"x\n");
}

/// git reads a path that starts with a dash as an option unless `--` comes first.
#[test]
fn the_printed_add_resolves_a_stash_pop_conflict_on_a_key_that_starts_with_a_dash() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("-x", b"x\n", 0o644);
    env.ok(&["add", "./-x"]);
    env.ok(&["sync"]);
    let repo = env.repo().to_string_lossy().into_owned();
    fs::write(env.repo().join("-x"), b"stashed\n").expect("edit");
    env.git(&["-C", &repo, "stash", "-q"]);
    fs::write(env.repo().join("-x"), b"committed\n").expect("edit");
    env.git(&["-C", &repo, "commit", "-q", "-am", "other"]);
    assert!(!env.git_raw(&["-C", &repo, "stash", "pop"]).status.success());

    let (code, err) = stderr_of(&env, &["status"]);
    assert_eq!(code, Some(4), "{err}");
    assert!(err.contains("`dotman git add -- -x`"), "{err}");
    fs::write(env.repo().join("-x"), b"resolved\n").expect("resolve");
    let out = common::run_hint(&env, &err, "git add");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stderr_of(&env, &["status"]).0, Some(0));
}

#[test]
fn stale_git_locks_are_reported_with_the_recovery_steps() {
    let env = committed();
    let lock = env.repo().join(".git/index.lock");
    fs::write(&lock, b"").expect("leave a lock");
    let err = env.fails(&["sync"]);
    assert!(err.contains("git's lock files are in the repo"), "{err}");
    assert!(err.contains(&format!("rm {}", lock.display())), "{err}");
    assert!(lock.exists(), "never removed silently");
    assert_eq!(stderr_of(&env, &["status"]).0, Some(0));

    fs::create_dir(env.repo().join(".git/rebase-merge")).expect("mark the rebase");
    fs::write(
        env.repo().join(".git/rebase-merge/head-name"),
        b"refs/heads/main\n",
    )
    .expect("head-name");
    let (code, err) = stderr_of(&env, &["status"]);
    assert_eq!(code, Some(4), "{err}");
    for step in [
        format!("1. rm {}", lock.display()),
        "2. dotman git rebase --abort".to_owned(),
        "dotman git rebase --quit && dotman git checkout -f main && dotman git reset --hard ORIG_HEAD"
            .to_owned(),
        "4. dotman sync".to_owned(),
    ] {
        assert!(err.contains(&step), "{step}: {err}");
    }
}

/// The marker a sync writes before it replays a pull, naming no settled path: the branch, its head
/// before the pull and the commit the pull fetched.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn mark_pull(env: &TestEnv, old: &str, onto: &str) -> std::path::PathBuf {
    let marker = env.repo().join(".git/dotman-pull");
    fs::write(&marker, format!("dotman-pull 1\0main\0{old}\0{onto}\0")).expect("write the marker");
    marker
}

fn rev(env: &TestEnv, rev: &str) -> String {
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "rev-parse", rev]).trim().to_owned()
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

/// A writing command puts back what a killed sync's replay left; every other command, and every
/// dry run, exits 4 with the one line that points to sync, and keeps the marker.
#[test]
fn every_subcommand_meets_the_marker_of_a_stopped_pull() {
    const WRITES: &[&str] = &[
        "init", "add", "forget", "apply", "sync", "restore", "key", "import",
    ];
    const DRY: &[&str] = &[
        "init", "add", "forget", "apply", "sync", "restore", "import",
    ];
    let refused = "an earlier `dotman sync` was stopped while it finished a pull in";
    let restored = "an earlier sync was stopped while it finished a pull; the repo is back as it was before that pull";
    for (name, args) in MINIMAL_ARGS {
        for dry in [false, true] {
            if dry && !DRY.contains(name) {
                continue;
            }
            let env = committed();
            let head = rev(&env, "HEAD");
            let marker = mark_pull(&env, &head, &head);
            let mut full = vec![*name];
            full.extend_from_slice(args);
            if dry {
                full.push("--dry-run");
            }
            let (code, err) = stderr_of(&env, &full);
            if WRITES.contains(name) && !dry {
                assert!(err.contains(restored), "`dot {full:?}`: {err}");
                assert!(!marker.exists(), "`dot {full:?}`");
                continue;
            }
            assert_eq!(code, Some(4), "`dot {full:?}`: {err}");
            assert!(err.contains(refused), "`dot {full:?}`: {err}");
            assert!(
                err.contains(
                    "run `dotman sync`, which puts the repo back as it was before that pull"
                ),
                "{err}"
            );
            assert!(marker.exists(), "`dot {full:?}`");
        }
    }
}

/// A lock file a killed git left stops the recovery; the printed steps, run as printed, finish it.
#[test]
fn a_git_lock_under_the_marker_prints_rm_and_then_sync() {
    let env = committed();
    let head = rev(&env, "HEAD");
    let marker = mark_pull(&env, &head, &head);
    let lock = env.repo().join(".git/index.lock");
    fs::write(&lock, b"").expect("leave a lock");
    for args in [&["status"][..], &["sync"]] {
        let (code, err) = stderr_of(&env, args);
        assert_eq!(code, Some(4), "{args:?}: {err}");
        assert!(!err.contains("reset --hard"), "{err}");
        let steps = numbered_steps(&err);
        assert_eq!(
            steps,
            [format!("rm {}", lock.display()), "dotman sync".to_owned()],
            "{err}"
        );
    }
    assert!(marker.exists() && lock.exists());
    let doctor = String::from_utf8_lossy(&env.run(&["doctor"]).stdout).into_owned();
    assert!(
        doctor.contains("fail  pull: an earlier dotman sync was stopped while it finished a pull; run dotman sync"),
        "{doctor}"
    );
    let steps = numbered_steps(&stderr_of(&env, &["sync"]).1);
    for step in &steps {
        let out = common::paste(&env, step);
        assert!(
            out.status.success(),
            "{step}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!marker.exists() && !lock.exists());
    assert_eq!(stderr_of(&env, &["status"]).0, Some(0));
}

/// Under the marker, a rebase dotman did not start is refused: nothing is reset, both commits are
/// named, and the printed steps, run as printed, end in a clean sync.
#[test]
fn a_rebase_the_marker_does_not_explain_is_refused_with_steps_that_lead_to_a_sync() {
    let env = committed();
    let repo = env.repo().to_string_lossy().into_owned();
    let git = |args: &[&str]| {
        let mut full = vec!["-C", repo.as_str()];
        full.extend_from_slice(args);
        env.git(&full)
    };
    git(&["checkout", "-q", "-b", "side"]);
    fs::write(env.repo().join(".bashrc"), b"side\n").expect("edit");
    git(&["commit", "-q", "-am", "side"]);
    git(&["checkout", "-q", "main"]);
    fs::write(env.repo().join(".bashrc"), b"main\n").expect("edit");
    git(&["commit", "-q", "-am", "main"]);
    let old = rev(&env, "main");
    assert!(
        !env.git_raw(&["-C", &repo, "rebase", "side"])
            .status
            .success()
    );
    let marker = mark_pull(&env, &old, &old);
    let before = git(&["status", "--porcelain"]);

    let (code, err) = stderr_of(&env, &["sync"]);
    assert_eq!(code, Some(4), "{err}");
    assert!(
        err.contains("a rebase dotman did not start is in progress"),
        "{err}"
    );
    assert!(err.contains(&format!("main was at {old}")), "{err}");
    assert!(
        err.contains(&format!("HEAD is now at {}", rev(&env, "HEAD"))),
        "{err}"
    );
    assert_eq!(git(&["status", "--porcelain"]), before, "nothing was reset");
    assert!(marker.exists());
    let steps = numbered_steps(&err);
    assert_eq!(
        steps,
        [
            "dotman git rebase --quit".to_owned(),
            "dotman git checkout -f main".to_owned(),
            format!("rm {}", marker.display()),
            "dotman sync".to_owned(),
        ]
    );
    for step in &steps {
        let out = common::paste(&env, step);
        assert!(
            out.status.success(),
            "{step}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!marker.exists());
    assert_eq!(git(&["status", "--porcelain"]), "");
    assert_eq!(stderr_of(&env, &["status"]).0, Some(0));
}

/// A repo path that is a linked worktree, whose `.git` is a file naming the real git dir, is
/// refused by every subcommand with what dotman needs instead of a raw "Not a directory".
#[test]
fn a_linked_worktree_as_the_repo_is_refused_clearly() {
    let env = committed();
    let main = env.xdg_data().join("main");
    fs::rename(env.repo(), &main).expect("move the repo aside");
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&[
        "-C",
        &main.to_string_lossy(),
        "worktree",
        "add",
        "-q",
        &repo,
        "-b",
        "linked",
    ]);
    assert!(env.repo().join(".git").is_file());
    let home = common::tree_checksum(&env.home());
    for (name, args) in MINIMAL_ARGS {
        let mut full = vec![*name];
        full.extend_from_slice(args);
        let (code, err) = stderr_of(&env, &full);
        assert_eq!(code, Some(1), "`dot {full:?}`: {err}");
        assert!(
            err.contains("is a linked git worktree or a submodule (its .git is a file)"),
            "{name}: {err}"
        );
        assert!(!err.contains("Not a directory"), "{name}: {err}");
    }
    assert_eq!(common::tree_checksum(&env.home()), home);
}
